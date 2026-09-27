//! 旧数据导入：从旧版数据目录迁移到当前数据目录。
//!
//! 场景：
//! - 从 Python 版 FocusFlow 切换到 Rust 版（schema 兼容，直接迁移）
//! - 换电脑/换目录后迁移旧数据
//!
//! 设计：
//! - 年度键鼠库 `focusflow_YYYY.db`：先写入暂存表 key_log（按 timestamp 去重，幂等），
//!   再通过聚合迁移落进 daily/hourly/key 三张聚合表并压缩文件；
//!   按天的**活跃时长**单独并（明细里没有时长信息，旧库存在 `active_seconds` 表
//!   或 `daily_counts.seconds` 里，同一天取两侧较大值 —— 幂等且不覆盖新值）
//! - 重复导入靠源文件指纹（大小+mtime）跳过；指纹变了但 key_log **内容**没变
//!   （复制/云盘摸过 mtime 的同一份文件）靠内容指纹（行数+最大时间戳）跳过 ——
//!   聚合是累加式，把同一批明细再聚一遍就是整体翻倍
//! - 附属库（accounting/pomodoro/scheduler/edge_history）：整体复制覆盖，
//!   **覆盖前先把现有库改名留档**（见 `backup_before_overwrite`）——
//!   导入目录由用户自己选，选错目录不能让当前数据凭空消失
//! - 若目标库不存在则整体复制文件（最快路径）
//! - 输出导入汇总

use std::path::Path;

use rusqlite::Connection;

use crate::db::connection;
use crate::paths;

/// 导入结果汇总。
#[derive(Debug, Default)]
pub struct ImportSummary {
    /// 导入的年度库
    pub year_dbs: Vec<i32>,
    /// 各年度导入的键鼠记录数
    pub records_by_year: Vec<(i32, i64)>,
    /// 复制的附属库
    pub copied_aux: Vec<String>,
    /// 被导入覆盖、已改名留档的附属库（旧文件路径，供 UI 提示用户）
    pub backed_up_aux: Vec<String>,
    /// 跳过的文件（无数据/已存在）
    pub skipped: Vec<String>,
    /// 错误
    pub errors: Vec<String>,
}

/// 附属库文件名列表（直接复制）。
const AUX_DBS: &[&str] = &[
    "focusflow_accounting.db",
    "focusflow_pomodoro.db",
    "focusflow_scheduler.db",
    "focusflow_edge_history.db",
];

/// 从旧数据目录导入全部数据到当前数据目录。
pub fn import_legacy_data(src_dir: &Path) -> ImportSummary {
    let mut summary = ImportSummary::default();

    // 1) 年度键鼠库
    let src_year_dbs = list_year_dbs(src_dir);
    for year in src_year_dbs {
        match import_year_db(src_dir, year) {
            Ok(imported) => {
                summary.year_dbs.push(year);
                summary.records_by_year.push((year, imported));
            }
            Err(e) => summary.errors.push(format!("{year} 年度库导入失败: {e}")),
        }
    }

    // 2) 附属库：整体复制（表独立，无法按行合并）
    for aux in AUX_DBS {
        let src = src_dir.join(aux);
        if !src.exists() {
            continue;
        }
        let dst = paths::data_dir().join(aux);
        // 自己导自己必须先看住：用户在目录选择器里挑到自己当前的 data/ 上是很常见的
        // 误操作。下面那步留档用的是 rename —— 它会把"源文件"一起搬走，紧接着的
        // copy 就找不到源了，结果是附属库从正名上消失、应用下次打开建一个空库，
        // 表现就是"我的账记/日程数据没了"（文件其实还在，只是躺在 .import-backup-* 里）。
        if same_file(&src, &dst) {
            summary
                .skipped
                .push(format!("{aux}（源与目标是同一个文件，无需导入）"));
            continue;
        }
        // 现有库先留档再覆盖：导入目录是用户手选的，选错目录（或新版本里
        // 已记了几个月账）时当前数据不能就这么没了。
        match backup_before_overwrite(&dst) {
            Ok(Some(kept)) => summary.backed_up_aux.push(kept.display().to_string()),
            Ok(None) => {}
            Err(e) => {
                // 留档失败就不覆盖：宁可这次不导入，也不能拿用户数据冒险
                summary
                    .errors
                    .push(format!("{aux} 覆盖前留档失败，已跳过: {e}"));
                continue;
            }
        }
        match std::fs::copy(&src, &dst) {
            Ok(_) => summary.copied_aux.push(aux.to_string()),
            Err(e) => summary.errors.push(format!("{aux} 复制失败: {e}")),
        }
    }

    summary
}

/// 两个路径是否指向同一个已存在的文件（用于看住"自己导自己"）。
///
/// 认不出来时返回 false：宁可照常走导入流程，也不要在读不了元数据的时候
/// 悄悄跳过一份真正该导入的数据。
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// 覆盖前的留档：把现有文件改名成 `<原名>.import-backup-<时间戳>`。
///
/// 用 rename 而不是 copy —— 既省一次全量拷贝，也保证不会出现"复制到一半
/// 失败、原文件和新文件都不完整"的中间态。返回被留下的路径（原先不存在时为 None）。
fn backup_before_overwrite(dst: &Path) -> anyhow::Result<Option<std::path::PathBuf>> {
    if !dst.exists() {
        return Ok(None);
    }
    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S%3f").to_string();
    let mut name = dst
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("路径没有文件名: {}", dst.display()))?
        .to_os_string();
    name.push(format!(".import-backup-{timestamp}"));
    let kept = dst.with_file_name(name);
    std::fs::rename(dst, &kept)?;
    // 顺带把源库遗留的 sidecar 一起挪走：只留主库会让下次打开看到半套 WAL 状态
    for suffix in ["-wal", "-shm"] {
        let from = std::path::PathBuf::from(format!("{}{suffix}", dst.display()));
        if from.exists() {
            let to = std::path::PathBuf::from(format!("{}{suffix}", kept.display()));
            let _ = std::fs::rename(&from, to);
        }
    }
    tracing::info!(
        "导入前已留档现有库: {} -> {}",
        dst.display(),
        kept.display()
    );
    Ok(Some(kept))
}

/// 列出数据目录下的年度库（focusflow_YYYY.db）。
fn list_year_dbs(dir: &Path) -> Vec<i32> {
    let mut years = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Some(y) = paths::is_year_db_file(&entry.path()) {
                years.push(y);
            }
        }
    }
    years.sort_unstable();
    years
}

/// 导入单个年度库：先写暂存表（按 timestamp 去重），再聚合落库。
/// 返回导入的记录数。
fn import_year_db(src_dir: &Path, year: i32) -> anyhow::Result<i64> {
    let src_path = src_dir.join(format!("focusflow_{year}.db"));
    let dst_path = paths::year_db_path(year);

    // 目标库不存在 → 整体复制（最快）
    if !dst_path.exists() {
        std::fs::copy(&src_path, &dst_path)?;
        // 复制 WAL/SHM（若有未 checkpoint 数据）
        for suffix in ["-wal", "-shm"] {
            let s = format!("{}{}", src_path.display(), suffix);
            if Path::new(&s).exists() {
                let _ = std::fs::copy(&s, format!("{}{}", dst_path.display(), suffix));
            }
        }
        // 用 Rusqlite 打开确认可用 + 建聚合表 + 迁移旧格式数据
        let conn = connection::open_rw(&dst_path)?;
        connection::ensure_schema(&conn, year)?;
        drop(conn);
        crate::db::maintenance::migrate_v2();
        // 统计导入条数（聚合表总量）
        let conn = connection::open_ro(&dst_path)?;
        let count: i64 = conn.query_row(
            "SELECT COALESCE(SUM(count), 0) FROM daily_counts",
            [],
            |r| r.get(0),
        )?;
        record_import_marker(&dst_path, &src_path)?;
        return Ok(count);
    }

    // 目标库已存在：源文件未变化（大小+修改时间一致）→ 已导入过，跳过
    if let Ok(meta) = std::fs::metadata(&src_path) {
        let marker = read_import_marker(&dst_path).unwrap_or(None);
        let same_size = marker.as_ref().map(|m| m.size) == Some(meta.len());
        let same_mtime = match (marker.as_ref().and_then(|m| m.mtime), meta.modified().ok()) {
            (Some(a), Some(b)) => {
                let a_s = a
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let b_s = b
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                a_s == b_s
            }
            _ => false,
        };
        if same_size && same_mtime {
            return Ok(0);
        }
    }

    // 去重合并（写入暂存表）
    let src_conn = Connection::open(&src_path)?;
    let dst_conn = connection::open_rw(&dst_path)?;
    connection::ensure_schema(&dst_conn, year)?;

    // 活跃时长必须在检查 key_log 之前先并：这一步与明细无关，而源库有可能
    // 早就只剩聚合表（暂存明细聚合完就被丢弃），那种库走不到下面那段。
    merge_active_seconds_from_src(&src_conn, &dst_conn)?;

    // 源库 key_log 的内容指纹：没有这张表就是"只剩聚合表"，没有明细可导。
    let Some(kl_now) = key_log_fingerprint(&src_conn)? else {
        return Ok(0);
    };

    // 大小/mtime 变了、key_log 内容却逐行没变 —— 同一份旧库被资源管理器复制、
    // 云盘同步或备份还原摸过 mtime 都是这个形状 —— 一行新增明细都没有。
    // 暂存表在上轮聚合后已清空，对它没有任何记忆：照旧往下走，整套旧明细会
    // 被**再聚合一遍**，而 migrate_v2 是累加式（`count = count + excluded`），
    // 那一年就整体翻倍，界面上没有任何提示。
    if let Some((c0, t0)) = read_import_marker(&dst_path)
        .unwrap_or(None)
        .and_then(|m| m.key_log)
    {
        if (c0, t0) == kl_now {
            tracing::info!("{year} 年源库明细内容未变（仅文件时间戳变了），跳过重复导入");
            return Ok(0);
        }
    }
    // 已知局限（判定不修，留档）：源库**真的追加过**新明细时，这里仍会整份重聚合，
    // 与上次导入的重叠段翻倍。要修就得按 `timestamp > 上次最大值` 过滤或保留暂存表
    // 当去重记忆 —— 前者会把"另一份更早的旧目录里独有的历史"挡在外面（多源回填
    // 是导入对话框明说的用法），后者要放弃"聚合后连表丢掉"的压缩设计。两个代价
    // 都比它想救的场景（旧版在两次导入之间还在写数据）大，交给指纹守卫 + 本条留档。
    // 注意 `imported` 计数与 `records_by_year` 报的是**本次暂存命中数**，重聚合
    // 不区分新旧 —— 界面上"导入 N 条"偏大是这条局限的可见症状。

    // 暂存表按需创建：确认源库确实有明细才建，避免在目标库留下空表 + 唯一索引
    // （运行期不写它，无条件建表会让每个年度库白占 2~4 页）
    connection::ensure_staging_table(&dst_conn)?;

    // 源库全部明细一次读出（旧库是逐条按键，量级几十万行，读完即关连接）
    let rows: Vec<(String, i64)> = {
        let mut stmt = src_conn.prepare("SELECT key_name, timestamp FROM key_log")?;
        let list = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        list
    };

    // 幂等：唯一索引 (key_name, timestamp) 去重，重复行直接忽略。
    // 旧写法是逐行 `INSERT ... WHERE NOT EXISTS(...)`：一次索引查找变两次，
    // 且两个单列索引无法一次性定位。
    dst_conn.execute("BEGIN IMMEDIATE;", [])?;
    let mut imported: i64 = 0;
    {
        let mut insert = dst_conn
            .prepare("INSERT OR IGNORE INTO key_log (key_name, timestamp) VALUES (?1, ?2)")?;
        for (key, ts) in &rows {
            if insert.execute(rusqlite::params![key, ts])? > 0 {
                imported += 1;
            }
        }
    }
    dst_conn.execute("COMMIT;", [])?;
    drop(dst_conn);

    // 聚合落库并压缩（幂等：key_log 迁移后清空）
    crate::db::maintenance::migrate_v2();
    record_import_marker(&dst_path, &src_path)?;

    Ok(imported)
}

/// 读一份「date_key → 活跃秒数」（`sql` 只接受本模块内的字面量）。
fn read_day_seconds(conn: &Connection, sql: &str) -> anyhow::Result<Vec<(i64, i64)>> {
    let mut stmt = conn.prepare(sql)?;
    let list = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<Result<_, _>>()?;
    Ok(list)
}

/// 把源库的「每天活跃秒数」并进目标库，返回目标库净增的秒数。
///
/// 合并分支（目标年库已存在）原先只搬 `key_log` 明细，而**明细里没有时长信息**：
/// 活跃时长按天存在独立表 `active_seconds`（旧版）或 `daily_counts.seconds`
/// （2026-09-21 并入后），那条路上没有任何一步读它们。于是"换电脑/换目录"把
/// 旧库并进已有同年库时，按键数上去了、活跃时长却整段留在旧文件里。
///
/// 取 `MAX()` 而不是相加：重复导入不能把一天算成两天；而目标库某天是新版本
/// 自己采集的，它的时长就是真值，不能被旧库那天的 0 覆盖（旧版早期没记时长）。
fn merge_active_seconds_from_src(src: &Connection, dst: &Connection) -> anyhow::Result<i64> {
    let rows: Vec<(i64, i64)> = if connection::table_exists(src, "active_seconds") {
        read_day_seconds(src, "SELECT date_key, seconds FROM active_seconds")?
    } else if connection::column_exists(src, "daily_counts", "seconds") {
        read_day_seconds(src, "SELECT date_key, seconds FROM daily_counts")?
    } else {
        // 最早的旧版根本没记活跃时长 —— 没东西可搬，不是丢数据
        return Ok(0);
    };
    if rows.iter().all(|(_, s)| *s <= 0) {
        return Ok(0);
    }

    let before: i64 = dst.query_row(
        "SELECT COALESCE(SUM(seconds), 0) FROM daily_counts",
        [],
        |r| r.get(0),
    )?;
    for (date_key, seconds) in rows.iter().filter(|(_, s)| *s > 0) {
        // 补进来的那天可能一个按键记录都没有（时长口径与按键口径分别上线过）：
        // count 给 0，与 `merge_active_seconds_into_daily` 的既有口径一致。
        dst.execute(
            "INSERT INTO daily_counts (date_key, count, seconds) VALUES (?1, 0, ?2)
             ON CONFLICT(date_key)
             DO UPDATE SET seconds = MAX(daily_counts.seconds, excluded.seconds)",
            rusqlite::params![date_key, seconds],
        )?;
    }
    let after: i64 = dst.query_row(
        "SELECT COALESCE(SUM(seconds), 0) FROM daily_counts",
        [],
        |r| r.get(0),
    )?;
    let gained = after - before;
    if gained > 0 {
        tracing::info!("从旧库并入活跃时长 {gained} 秒");
    }
    Ok(gained)
}

/// 源库 key_log 的内容指纹：(行数, 最大时间戳)。没有这张表返回 None。
///
/// 行数 + 最大时间戳足以分辨"这份旧库的明细变没变"：key_log 是按键发生时
/// 逐条追加的，内容没变时两者恒定，摸一摸 mtime 不会动它们。反推不成立
/// （两份不同内容可能撞上同一对数）不碍事 —— 误判的代价只是少跳过一次
/// 幂等的导入，而**不是**漏导或翻倍。
fn key_log_fingerprint(conn: &Connection) -> anyhow::Result<Option<(i64, i64)>> {
    let has = conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='key_log'",
        [],
        |_| Ok(()),
    );
    if has.is_err() {
        return Ok(None);
    }
    let row = conn.query_row(
        "SELECT COUNT(*), COALESCE(MAX(timestamp), 0) FROM key_log",
        [],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
    )?;
    Ok(Some(row))
}

/// 记录源文件指纹（大小 + 修改时间 + key_log 内容）到目标库 meta，用于重复导入检测。
fn record_import_marker(dst_path: &Path, src_path: &Path) -> anyhow::Result<()> {
    let meta = std::fs::metadata(src_path)?;
    let conn = connection::open_rw(dst_path)?;
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('imported_src_size', ?1)",
        [meta.len().to_string()],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('imported_src_mtime', ?1)",
        [meta
            .modified()
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs().to_string())
            .unwrap_or_default()],
    )?;
    // key_log 内容指纹：内容没变、只有 mtime 变的再导入要靠它跳过（见
    // import_year_db 的内容比对）。源库没有 key_log 时不写 —— 读取侧两个键
    // 缺任一个都按"没有指纹"处理，与旧版本留下的标记兼容。
    if let Some(kl) = Connection::open(src_path)
        .ok()
        .and_then(|c| key_log_fingerprint(&c).ok().flatten())
    {
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('imported_src_kl_count', ?1)",
            [kl.0.to_string()],
        )?;
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('imported_src_kl_maxts', ?1)",
            [kl.1.to_string()],
        )?;
    }
    Ok(())
}

/// 导入标记：源文件指纹 + 上次导入时源库 key_log 的内容指纹。
struct ImportMarker {
    size: u64,
    mtime: Option<std::time::SystemTime>,
    /// (行数, 最大时间戳)；旧版本留下的标记没有这两个键 → None。
    key_log: Option<(i64, i64)>,
}

/// 读取导入标记。
fn read_import_marker(dst_path: &Path) -> anyhow::Result<Option<ImportMarker>> {
    let conn = connection::open_ro(dst_path)?;
    let size: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'imported_src_size'",
            [],
            |r| r.get(0),
        )
        .ok();
    let mtime: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'imported_src_mtime'",
            [],
            |r| r.get(0),
        )
        .ok();
    let kl_count: Option<i64> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'imported_src_kl_count'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|v| v.parse().ok());
    let kl_maxts: Option<i64> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'imported_src_kl_maxts'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|v| v.parse().ok());
    let Some(sz) = size else {
        return Ok(None);
    };
    Ok(Some(ImportMarker {
        size: sz.parse::<u64>().unwrap_or(0),
        mtime: mtime
            .and_then(|mt| mt.parse::<u64>().ok())
            .map(|secs| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs)),
        key_log: kl_count.zip(kl_maxts),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把**当前**数据目录当成导入源（目录选择器里挑到自己 data/ 上很常见）：
    /// 附属库必须原地不动。
    ///
    /// 修之前留档那步 rename 会把"源文件"一起搬走，紧接着的 copy 找不到源而失败 ——
    /// 附属库从正名上消失，应用下次打开时建一个空库，表现就是"我的账记数据没了"。
    #[test]
    fn importing_the_current_data_dir_keeps_aux_dbs_in_place() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("selfimport");
        let data = crate::paths::data_dir();
        let aux = data.join("focusflow_accounting.db");
        std::fs::write(&aux, b"not a real db, but it must survive").unwrap();

        let summary = import_legacy_data(&data);

        assert!(
            aux.is_file(),
            "自导入把附属库从正名上弄丢了：{:?}",
            summary.errors
        );
        assert!(
            !summary.errors.iter().any(|e| e.contains("复制失败")),
            "不该出现源文件已被搬走导致的复制失败：{:?}",
            summary.errors
        );
        assert!(
            summary.skipped.iter().any(|s| s.contains("同一个文件")),
            "应说明为什么跳过：{:?}",
            summary.skipped
        );
        let leftovers: Vec<_> = std::fs::read_dir(&data)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".import-backup-"))
            .collect();
        assert!(leftovers.is_empty(), "不该留下留档文件：{leftovers:?}");
    }

    /// 真正的跨目录导入仍须照旧覆盖并留档（上一条的守卫不能顺手把正常路径也挡掉）。
    #[test]
    fn importing_from_another_dir_still_overwrites_and_keeps_backup() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("crossimport");
        let data = crate::paths::data_dir();
        let aux = data.join("focusflow_accounting.db");
        std::fs::write(&aux, b"current").unwrap();

        let src_dir = _dir.path().join("old");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("focusflow_accounting.db"), b"legacy").unwrap();

        let summary = import_legacy_data(&src_dir);

        assert!(
            summary.copied_aux.iter().any(|a| a.contains("accounting")),
            "跨目录导入应照常复制：{:?}",
            summary
        );
        assert_eq!(std::fs::read_to_string(&aux).unwrap(), "legacy");
        let kept: Vec<_> = std::fs::read_dir(&data)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".import-backup-"))
            .collect();
        assert_eq!(kept.len(), 1, "被覆盖的现有库必须留档一份");
    }

    /// 同一份旧库、内容一行没变、只是 mtime 变了（资源管理器复制一份、云盘
    /// 同步、备份还原都是这个形状）→ 再导入一次**不得**把那一年翻倍。
    ///
    /// 大小+mtime 的指纹守卫挡不住这个形状（mtime 变了就照常往下走），而暂存表
    /// 在上轮聚合后已经连表丢掉、对导过什么毫无记忆，聚合又是累加式 —— 没有
    /// 内容指纹这一刀，整套旧明细会被再聚一遍，统计翻倍且无任何提示。
    /// 注掉内容比对（注回旧行为）这条必红。
    #[test]
    fn reimporting_unchanged_key_log_after_mtime_touch_does_not_double_count() {
        use std::time::Duration;
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("reimport");
        let year = 2024i32;

        // 源库：旧版形状（只有 key_log 明细），100 条落进同一天
        let src_dir = _dir.path().join("old");
        std::fs::create_dir_all(&src_dir).unwrap();
        let src = src_dir.join(format!("focusflow_{year}.db"));
        {
            let c = Connection::open(&src).unwrap();
            c.execute_batch(
                "CREATE TABLE key_log (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    key_name TEXT NOT NULL,
                    timestamp INTEGER NOT NULL
                );",
            )
            .unwrap();
            for i in 0..100 {
                c.execute(
                    "INSERT INTO key_log (key_name, timestamp) VALUES (?1, ?2)",
                    rusqlite::params![format!("k{}", i % 10), 1_700_000_000i64 + i],
                )
                .unwrap();
            }
        }
        // 目标库：已存在的空年度库 → 走合并路径（而不是整文件复制）
        let dst = paths::year_db_path(year);
        {
            let c = connection::open_rw(&dst).unwrap();
            connection::ensure_schema(&c, year).unwrap();
        }
        let daily_total = || -> i64 {
            connection::open_ro(&dst)
                .unwrap()
                .query_row(
                    "SELECT COALESCE(SUM(count), 0) FROM daily_counts",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        };

        let first = import_legacy_data(&src_dir);
        assert_eq!(
            first.records_by_year,
            vec![(year, 100)],
            "首次应导入全部 100 条明细"
        );
        assert_eq!(daily_total(), 100);

        // 摸 mtime（往回拨一小时，避开"刚创建"的边界）：大小不变、时间戳变
        let older = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&src)
            .unwrap()
            .set_modified(older)
            .unwrap();

        let again = import_legacy_data(&src_dir);
        assert_eq!(
            again.records_by_year,
            vec![(year, 0)],
            "内容没变就没有任何新明细：{:?}",
            again
        );
        assert_eq!(daily_total(), 100, "重导同内容不得把那一年翻倍");
    }
}
