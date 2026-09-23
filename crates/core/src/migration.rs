//! 旧数据导入：从旧版数据目录迁移到当前数据目录。
//!
//! 场景：
//! - 从 Python 版 FocusFlow 切换到 Rust 版（schema 兼容，直接迁移）
//! - 换电脑/换目录后迁移旧数据
//!
//! 设计：
//! - 年度键鼠库 `focusflow_YYYY.db`：先写入暂存表 key_log（按 timestamp 去重，幂等），
//!   再通过聚合迁移落进 daily/hourly/key 三张聚合表并压缩文件
//! - 附属库（accounting/pomodoro/scheduler/edge_history）：整体复制覆盖，
//!   **覆盖前先把现有库改名留档**（见 [`backup_before_overwrite`]）——
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
        let same_size = marker.map(|(s, _)| s) == Some(meta.len());
        let same_mtime = match (marker.and_then(|(_, m)| m), meta.modified().ok()) {
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

    // 确认源库有 key_log 表
    let has_src_table: bool = src_conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='key_log'",
            [],
            |_| Ok(()),
        )
        .is_ok();
    if !has_src_table {
        return Ok(0);
    }

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

/// 记录源文件指纹（大小 + 修改时间）到目标库 meta，用于重复导入检测。
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
    Ok(())
}

/// 读取导入指纹：(大小, 修改时间)。
fn read_import_marker(
    dst_path: &Path,
) -> anyhow::Result<Option<(u64, Option<std::time::SystemTime>)>> {
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
    match (size, mtime) {
        (Some(sz), Some(mt)) => {
            let size = sz.parse::<u64>().unwrap_or(0);
            let mtime = mt
                .parse::<u64>()
                .ok()
                .map(|secs| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs));
            Ok(Some((size, mtime)))
        }
        _ => Ok(None),
    }
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
}
