//! 数据库维护：聚合迁移、年度归档、备份、VACUUM、清理。
//!
//! - `migrate_v2`：旧版逐条数据 → 按天聚合表（一次性迁移 + 组合键名修正 + 压缩）
//! - `_check_yearly_archive` / `_archive_year_data`：跨年数据归档
//! - 备份（SQLite 在线备份 API）+ 轮转
//! - VACUUM + PRAGMA optimize
//! - 清理旧数据

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use chrono::{Datelike, Local, NaiveDate};
use rusqlite::Connection;

use crate::db::connection;
use crate::db::queries;
use crate::paths;

/// 年度库中的全部按天数据表（列名统一，date_key 均为本地天数序号）。
///
/// 归档、清理、清空必须使用同一份清单：此前只覆盖 daily/hourly/key_counts，
/// 导致 active_seconds 与 app_usage 永不归档、永不清理 —— 跨年后前台应用
/// 时长仍留在旧文件里，而 CLI 的 `--reset` 会报告"已清空"却留着这两张表。
const DATA_TABLES: [&str; 5] = [
    "daily_counts",
    "hourly_counts",
    "key_counts",
    "active_seconds",
    "app_usage",
];

/// 检查是否需要年度归档（当前年份库中存在往年数据时）。
///
/// 统计口径是「所有早于本年的数据」，归档也必须一次迁完全部往年数据：
/// 只迁上一年时，若当前库里存在两个以上更早年份（长期未运行后由恢复文件
/// 回放、或旧版单库迁移而来），旧数据永远迁不走，而每次启动都会重复判定
/// "有数据要归档"，空跑一次 ATTACH + 事务 + VACUUM。
pub fn check_yearly_archive(yearly_archive_enabled: bool) {
    if !yearly_archive_enabled {
        return;
    }
    let current_year = Local::now().year();
    let year_start_dk =
        queries::day_key_of_date(NaiveDate::from_ymd_opt(current_year, 1, 1).expect("date"));

    let path = paths::current_year_db_path();
    let conn = match connection::open_ro(&path) {
        Ok(c) => c,
        Err(_) => return,
    };
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM daily_counts WHERE date_key < ?1",
            [year_start_dk],
            |r| r.get(0),
        )
        .unwrap_or(0);
    drop(conn);

    if count == 0 {
        return;
    }
    tracing::info!("检测到 {count} 天早于 {current_year} 年的数据在当前库中，开始归档...");
    if archive_stale_years(current_year) {
        queries::invalidate_years_cache();
    }
}

/// 把 `source_year` 库中所有早于 `source_year` 的按天数据迁到各自年份库。
/// 返回是否真的迁移了数据。
pub fn archive_stale_years(source_year: i32) -> bool {
    let source_path = paths::year_db_path(source_year);
    if !source_path.exists() {
        return false;
    }
    // 先查出需要迁移的年份及各年起始 date_key（用源库自己的连接查，不依赖猜测）
    let y0 = queries::day_key_of_date(NaiveDate::from_ymd_opt(source_year, 1, 1).expect("date"));
    let min_dk: Option<i64> = match connection::open_ro(&source_path) {
        Ok(conn) => conn
            .query_row(
                "SELECT MIN(date_key) FROM daily_counts WHERE date_key < ?1",
                [y0],
                |r| r.get(0),
            )
            .ok()
            .flatten(),
        Err(_) => None,
    };
    let first_stale_year = min_dk.and_then(queries::day_key_to_date).map(|d| d.year());
    let stale: Vec<(i32, i64, i64)> = match first_stale_year {
        Some(first) => (first..source_year)
            .map(|year| {
                let a =
                    queries::day_key_of_date(NaiveDate::from_ymd_opt(year, 1, 1).expect("date"));
                let b = queries::day_key_of_date(
                    NaiveDate::from_ymd_opt(year + 1, 1, 1).expect("date"),
                );
                (year, a, b)
            })
            .collect(),
        None => Vec::new(),
    };

    let mut migrated_any = false;
    for (year, a, b) in stale {
        if archive_year_range(year, source_year, a, b) {
            migrated_any = true;
        }
    }
    // 只在真的迁走了数据后才 VACUUM 源库：否则每次启动都要付一次全库重写的代价
    if migrated_any {
        vacuum_path(&source_path);
    }
    migrated_any
}

/// 将 `source_year` 库中 `[dk_from, dk_to)` 的数据迁移到 `target_year` 库。
///
/// 目标库可能已存在同 date_key（历史遗留、跨年误写），因此用 UPSERT 合并计数，
/// 而不是裸 INSERT —— 后者会主键冲突导致整个归档事务回滚、归档永久失败。
/// 返回是否迁移了数据。
pub fn archive_year_range(target_year: i32, source_year: i32, dk_from: i64, dk_to: i64) -> bool {
    if target_year >= source_year {
        tracing::error!("年度归档参数非法：目标年 {target_year} 不早于源年 {source_year}");
        return false;
    }

    // 1. 确保 target_year 库有表结构
    let target_path = paths::year_db_path(target_year);
    let source_path = paths::year_db_path(source_year);
    if let Err(e) = connection::open_rw(&target_path)
        .and_then(|conn| connection::ensure_schema(&conn, target_year))
    {
        tracing::error!("年度归档失败：目标库初始化失败: {e}");
        return false;
    }

    // 2-4. ATTACH 迁移：任一步失败即回滚，杜绝"源库已删、目标库未写"
    let result = (|| -> anyhow::Result<usize> {
        let source_str = source_path.to_str().ok_or_else(|| {
            anyhow::anyhow!("源库路径包含非 UTF-8 字符: {}", source_path.display())
        })?;
        let conn = connection::open_rw(&target_path)?;
        conn.execute(
            "ATTACH DATABASE ?1 AS source",
            rusqlite::params![source_str],
        )?;
        conn.execute("BEGIN;", [])?;
        let migrate: anyhow::Result<usize> = (|| {
            let mut moved = 0usize;
            for table in DATA_TABLES {
                let pk_cols = match table {
                    "daily_counts" | "active_seconds" => "date_key",
                    "hourly_counts" => "date_key, hour",
                    "key_counts" => "date_key, key_name",
                    "app_usage" => "date_key, app_name",
                    _ => unreachable!("DATA_TABLES 新增表时必须补主键列"),
                };
                let value_cols = match table {
                    "daily_counts" | "hourly_counts" | "key_counts" => "count",
                    "active_seconds" | "app_usage" => "seconds",
                    _ => unreachable!("DATA_TABLES 新增表时必须补计数列"),
                };
                // 只迁「本表确实有行」的年份：若某年只有 daily_counts 有数据，
                // 其余表插入 0 行也删除 0 行，行数校验天然成立。
                conn.execute(
                    &format!(
                        "INSERT INTO {table} ({pk_cols}, {value_cols}) \
                         SELECT {pk_cols}, {value_cols} FROM source.{table} \
                          WHERE date_key >= ?1 AND date_key < ?2 \
                         ON CONFLICT({pk_cols}) DO UPDATE SET {value_cols} = \
                            {table}.{value_cols} + excluded.{value_cols}"
                    ),
                    rusqlite::params![dk_from, dk_to],
                )?;
                let deleted = conn.execute(
                    &format!("DELETE FROM source.{table} WHERE date_key >= ?1 AND date_key < ?2"),
                    rusqlite::params![dk_from, dk_to],
                )?;
                moved += deleted;
            }
            Ok(moved)
        })();
        match migrate {
            Ok(moved) => {
                conn.execute("COMMIT;", [])?;
                conn.execute("DETACH DATABASE source", [])?;
                Ok(moved)
            }
            Err(e) => {
                let _ = conn.execute("ROLLBACK;", []);
                let _ = conn.execute("DETACH DATABASE source", []);
                Err(e)
            }
        }
    })();

    match result {
        Ok(0) => false,
        Ok(moved) => {
            tracing::info!(
                "归档完成：{target_year} 年 {moved} 行已迁移到 {}（源 {source_year} 年库）",
                target_path.display()
            );
            true
        }
        Err(e) => {
            tracing::error!("年度归档失败（{target_year} 年 <- {source_year} 年库）: {e}");
            false
        }
    }
}

/// 将 `source_year` 库中属于 `target_year` 的数据迁移到 `target_year` 库。
pub fn archive_year_data(target_year: i32, source_year: i32) {
    let y0 = queries::day_key_of_date(NaiveDate::from_ymd_opt(target_year, 1, 1).expect("date"));
    let y1 =
        queries::day_key_of_date(NaiveDate::from_ymd_opt(target_year + 1, 1, 1).expect("date"));
    if archive_year_range(target_year, source_year, y0, y1) {
        queries::invalidate_years_cache();
    }
}

/// 一次性迁移：把旧版逐条 `key_log` 数据聚合到三张聚合表，并压缩文件。
///
/// 幂等：迁移后 `key_log` 被清空，再次调用不重复聚合。
/// 所有年度库都会被检查（含跨年归档的旧文件与导入的旧格式文件）。
pub fn migrate_v2() {
    let mut years: Vec<i32> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(paths::data_dir()) {
        for entry in entries.flatten() {
            if let Some(y) = paths::is_year_db_file(&entry.path()) {
                years.push(y);
            }
        }
    }
    years.sort_unstable();
    for year in years {
        migrate_v2_file(&paths::year_db_path(year), year);
    }
    queries::invalidate_years_cache();
}

/// 迁移单个年度库，返回迁移的明细条数（无旧数据时为 0）。
fn migrate_v2_file(path: &Path, year: i32) -> i64 {
    let result = (|| -> anyhow::Result<i64> {
        let conn = connection::open_rw(path)?;
        connection::ensure_schema(&conn, year)?;
        let has_key_log: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='key_log'",
                [],
                |_| Ok(()),
            )
            .is_ok();
        if !has_key_log {
            return Ok(0);
        }
        let row_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM key_log", [], |r| r.get(0))
            .unwrap_or(0);
        if row_count == 0 {
            return Ok(0);
        }

        let off = queries::local_utc_offset_seconds();
        conn.execute("BEGIN IMMEDIATE;", [])?;
        conn.execute(
            "INSERT INTO daily_counts (date_key, count)
             SELECT CAST((timestamp + ?1) / 86400 AS INTEGER), COUNT(*)
             FROM key_log GROUP BY 1",
            [off],
        )?;
        conn.execute(
            "INSERT INTO hourly_counts (date_key, hour, count)
             SELECT CAST((timestamp + ?1) / 86400 AS INTEGER),
                    CAST(((timestamp + ?1) / 3600) % 24 AS INTEGER),
                    COUNT(*)
             FROM key_log GROUP BY 1, 2",
            [off],
        )?;
        conn.execute(
            "INSERT INTO key_counts (date_key, key_name, count)
             SELECT CAST((timestamp + ?1) / 86400 AS INTEGER), key_name, COUNT(*)
             FROM key_log GROUP BY 1, 2",
            [off],
        )?;

        // 旧版 Ctrl+X 组合键名修正
        {
            let mut stmt = conn
                .prepare("SELECT DISTINCT key_name FROM key_counts WHERE key_name GLOB 'Ctrl+*'")?;
            let names: Vec<String> = stmt
                .query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            for old in names {
                if let Some(new) = combo_key_mapping(&old) {
                    if new != old {
                        let _ = conn.execute(
                            "UPDATE key_counts SET key_name = ?1 WHERE key_name = ?2",
                            rusqlite::params![new, old],
                        );
                    }
                }
            }
        }

        // 清空暂存表并标记
        conn.execute("DELETE FROM key_log", [])?;
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_v2', '1')",
            [],
        )?;
        conn.execute("COMMIT;", [])?;
        Ok(row_count)
    })();

    match &result {
        Ok(n) if *n > 0 => {
            tracing::info!("旧数据已聚合迁移: {year} 年 {n} 条");
            vacuum_path(path);
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("聚合迁移失败（{year} 年）: {e}"),
    }
    result.unwrap_or(0)
}

/// `Ctrl+X` -> 物理键名映射。
fn combo_key_mapping(old: &str) -> Option<String> {
    // Ctrl+A-Z / Ctrl+a-z -> 字母大写
    if let Some(rest) = old.strip_prefix("Ctrl+") {
        if rest.len() == 1 {
            let c = rest.chars().next()?;
            if c.is_ascii_alphabetic() {
                return Some(c.to_ascii_uppercase().to_string());
            }
            if c.is_ascii_digit() {
                return Some(c.to_string());
            }
        }
    }
    if old == "Ctrl+127" {
        return Some("Delete".to_string());
    }
    None
}

/// 清理 keep_days 天前的数据，返回删除的聚合行数。
/// 不可逆操作：执行前先做一次全量备份。
pub fn cleanup_old_data(keep_days: i64) -> i64 {
    snapshot_before_destructive("cleanup_old_data");
    let cutoff_dk = queries::day_key_of_date(Local::now().date_naive()) - keep_days;
    let mut total = 0i64;
    for year in queries::available_years() {
        let path = paths::year_db_path(year);
        let conn = match connection::open_rw(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for table in DATA_TABLES {
            let n = conn
                .execute(
                    &format!("DELETE FROM {table} WHERE date_key < ?1"),
                    [cutoff_dk],
                )
                .unwrap_or(0);
            total += n as i64;
        }
        tracing::info!("已清理 {year} 年 {cutoff_dk} 前的数据");
    }
    if total > 0 {
        tracing::info!("共清理 {total} 行聚合数据");
    }
    total
}

/// VACUUM 指定数据库。
pub fn vacuum_path(path: &Path) {
    let result = (|| -> anyhow::Result<()> {
        let conn = connection::open_rw(path)?;
        conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
        conn.execute("VACUUM;", [])?;
        conn.execute("PRAGMA optimize;", [])?;
        Ok(())
    })();
    match result {
        Ok(()) => tracing::info!("已压缩 {}", path.display()),
        Err(e) => tracing::error!("VACUUM {} 失败: {e}", path.display()),
    }
}

/// 压缩所有年度数据库。
pub fn vacuum_all() {
    for year in queries::available_years() {
        vacuum_path(&paths::year_db_path(year));
    }
}

/// 按配置自动 VACUUM（检查 meta 表中的 last_vacuum）。
pub fn maybe_auto_vacuum(auto_vacuum_days: i64) {
    if auto_vacuum_days <= 0 {
        return;
    }
    let path = paths::current_year_db_path();
    let conn = match connection::open_ro(&path) {
        Ok(c) => c,
        Err(_) => return,
    };
    let last: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'last_vacuum'",
            [],
            |r| r.get(0),
        )
        .ok();
    drop(conn);

    if let Some(last) = last {
        if let Ok(last_dt) = chrono::DateTime::parse_from_rfc3339(&last) {
            let last_local = last_dt.with_timezone(&Local);
            let diff = Local::now().signed_duration_since(last_local);
            if diff.num_days() < auto_vacuum_days {
                return;
            }
        }
    }

    vacuum_all();
    let now_str = chrono::DateTime::to_rfc3339(&chrono::Utc::now());
    if let Ok(conn) = connection::open_rw(&path) {
        let _ = conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('last_vacuum', ?1)",
            [now_str],
        );
    }
    tracing::info!("自动 VACUUM 完成");
}

/// 校验备份数据库可打开且通过 quick_check。
/// 坏备份（撕裂的文件拷贝/中断的备份）不删除会占用轮转名额、顶掉好备份。
fn verify_backup_file(dst: &Path) -> bool {
    let result = Connection::open_with_flags(
        dst,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .and_then(|conn| conn.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0)));
    match result {
        Ok(status) if status == "ok" => true,
        other => {
            tracing::error!("备份完整性校验失败 {}: {other:?}", dst.display());
            let _ = std::fs::remove_file(dst);
            false
        }
    }
}

/// 用 SQLite 在线备份 API 生成一致快照（源库只读打开，避免备份触发 WAL 副作用）。
fn backup_db_file(src: &Path, dst: &Path) -> bool {
    let result = (|| -> anyhow::Result<()> {
        let src_conn = Connection::open_with_flags(
            src,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        src_conn.backup(rusqlite::DatabaseName::Main, dst, None)?;
        Ok(())
    })();
    match result {
        Ok(()) => true,
        Err(e) => {
            tracing::error!(
                "SQLite 在线备份失败 {} -> {}: {e}",
                src.display(),
                dst.display()
            );
            false
        }
    }
}

/// 启动运行中定时在线备份线程：每 `interval_hours` 小时静默备份一次全部年度库
/// （0 = 关闭）。进程被强杀时不再丢失自上次退出备份后的全部数据。
/// 备份走 SQLite 在线备份 API，不阻塞读写（与写线程的短事务天然错开）。
pub fn start_periodic_backup(interval_hours: u64, max_backups: i64) {
    if interval_hours == 0 {
        return;
    }
    std::thread::Builder::new()
        .name("periodic-backup".into())
        .spawn(move || {
            let interval = std::time::Duration::from_secs(interval_hours * 3600);
            tracing::info!("定时在线备份已启动：每 {interval_hours} 小时一次");
            loop {
                std::thread::sleep(interval);
                if backup_database(max_backups).is_none() {
                    tracing::debug!("定时备份：无可备份的年度库");
                }
            }
        })
        .expect("启动定时备份线程失败");
}

/// 破坏性操作（清空/清理/删除按键）前的兜底快照。
/// 备份失败不阻断操作本身（用户在 UI 主动发起），但错误日志会高亮，
/// 并且该次备份不会污染轮转（坏文件已被校验逻辑删除）。
fn snapshot_before_destructive(op: &str) {
    let max_backups = crate::config::instance()
        .get_int("database", "max_backups", 5)
        .max(1);
    if backup_database(max_backups).is_none() {
        tracing::error!("{op}: 执行前快照失败，没有任何数据库被备份");
    } else {
        tracing::info!("{op}: 已完成执行前快照");
    }
}

/// 附属数据库（记账/番茄钟/定时任务/Edge 历史）路径列表。
/// 这些库以前从不备份，记账等数据损坏后无法恢复。
fn auxiliary_db_paths() -> Vec<(&'static str, std::path::PathBuf)> {
    vec![
        ("accounting", crate::accounting::db_path()),
        ("pomodoro", crate::pomodoro::db_path()),
        ("scheduler", crate::scheduler::db_path()),
        ("edge_history", crate::edge_history::edge_db_path()),
    ]
}

/// 备份所有年度数据库与附属数据库到 backup/ 目录，返回首个备份路径。
/// 每个备份都做 quick_check 校验，坏备份会被删除、不占用轮转名额。
/// 备份串行化锁。
///
/// 备份有多个并发触发点（运行中定时备份线程、退出前备份、UI 手动备份、
/// 破坏性操作前的快照），而备份目标名原先只精确到秒：两个并发备份会写同一个
/// 文件，且 `verify_backup_file` 在打开失败时会删除 dst —— Windows 下读取另一个
/// 线程正在写的文件必然失败，于是刚写好的备份被自己人删掉、轮转也可能误删。
/// 整个「备份 -> 校验 -> 轮转」过程持锁执行，保证同一时刻只有一次备份在跑。
static BACKUP_LOCK: Mutex<()> = Mutex::new(());

pub fn backup_database(max_backups: i64) -> Option<std::path::PathBuf> {
    let _guard = BACKUP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::fs::create_dir_all(paths::backup_dir()).ok();
    // 毫秒精度：即使锁被绕过，同秒内的两次备份也不会撞同一个文件名
    let timestamp = Local::now().format("%Y%m%d_%H%M%S%3f").to_string();
    let mut backed_up: Vec<std::path::PathBuf> = Vec::new();
    for year in queries::available_years() {
        let src = paths::year_db_path(year);
        if !src.exists() {
            continue;
        }
        let dst = paths::backup_dir().join(format!("focusflow_{year}_{timestamp}.db"));
        let mut ok = backup_db_file(&src, &dst) && verify_backup_file(&dst);
        if !ok {
            // 兜底：checkpoint 后直接复制主文件（复制的是变化中的文件，可能撕裂，
            // 必须校验，避免坏备份顶掉轮转中的好备份）
            if let Ok(conn) = connection::open_rw(&src) {
                let _ = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
                drop(conn);
            }
            ok = std::fs::copy(&src, &dst).is_ok() && verify_backup_file(&dst);
        }
        if ok {
            backed_up.push(dst);
        }
    }
    // 附属库同样纳入备份与轮转（命名沿用 focusflow_{组名}_{时间戳}.db，
    // rotate_backups 按第一段分组，"accounting" 等名称各自成组）
    for (name, src) in auxiliary_db_paths() {
        if !src.exists() {
            continue;
        }
        let dst = paths::backup_dir().join(format!("focusflow_{name}_{timestamp}.db"));
        if backup_db_file(&src, &dst) && verify_backup_file(&dst) {
            backed_up.push(dst);
        } else {
            tracing::error!("附属库备份失败: {name}");
        }
    }
    if !backed_up.is_empty() {
        rotate_backups(max_backups);
        tracing::info!(
            "已备份 {} 个数据库到 {}",
            backed_up.len(),
            paths::backup_dir().display()
        );
        backed_up.first().cloned()
    } else {
        None
    }
}

/// 保留每组最近 N 个备份。
/// 分组键取文件名第一段（`focusflow_2026_…` → "2026"，`focusflow_accounting_…`
/// → "accounting"）。此前按第二段分组取到的是日期，每天自成一组导致轮转
/// 永远删不到旧日期的备份，backup/ 目录无限增长。
fn rotate_backups(max_keep: i64) {
    let mut groups: HashMap<String, Vec<std::path::PathBuf>> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(paths::backup_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            // focusflow_2026_20260723_075125.db / focusflow_accounting_20260723_075125.db
            if let Some(stem) = name
                .strip_prefix("focusflow_")
                .and_then(|s| s.strip_suffix(".db"))
            {
                let parts: Vec<&str> = stem.split('_').collect();
                if parts.len() >= 3 {
                    let group = parts[0].to_string();
                    groups.entry(group).or_default().push(entry.path());
                }
            }
        }
    }
    for files in groups.values_mut() {
        files.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
        files.reverse();
        for old in files.iter().skip(max_keep.max(0) as usize) {
            let _ = std::fs::remove_file(old);
        }
    }
}

/// 清空所有年度库的统计表（键鼠计数/小时分布/按键明细/活跃时长/前台应用），返回删除行数。
/// 不可逆操作：执行前先做一次全量备份（失败只记日志不阻断，但会在日志中高亮）。
pub fn reset_all_data() -> i64 {
    snapshot_before_destructive("reset_all_data");
    let mut total = 0i64;
    for year in queries::available_years() {
        let path = paths::year_db_path(year);
        let conn = match connection::open_rw(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for table in DATA_TABLES {
            let n = conn
                .execute(&format!("DELETE FROM {table}"), [])
                .unwrap_or(0);
            total += n as i64;
        }
    }
    queries::invalidate_years_cache();
    tracing::info!("已清空全部统计数据（键鼠/活跃时长/前台应用）{total} 行");
    total
}

/// 删除今日指定按键的聚合记录（含内存中未落库增量由调用方先 flush），返回删除的计数值。
///
/// key_counts 为每键每天一行；删除后同步修正 daily_counts 与 hourly_counts：
/// hourly_counts 只有全天各小时总量、不含按 key 拆分，故按 removed/daily_before
/// 比例整数分摊扣减（余数从最大小时补扣），保证 Σhourly == daily == Σkey_counts。
pub fn delete_key_today(key_name: &str) -> i64 {
    let key_name = key_name.trim();
    if key_name.is_empty() {
        return 0;
    }
    snapshot_before_destructive("delete_key_today");
    let today_dk = queries::day_key_of_date(Local::now().date_naive());
    let path = paths::current_year_db_path();
    let conn = match connection::open_rw(&path) {
        Ok(c) => c,
        Err(_) => return 0,
    };

    let removed: i64 = conn
        .query_row(
            "SELECT count FROM key_counts WHERE key_name=?1 AND date_key=?2",
            rusqlite::params![key_name, today_dk],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if removed <= 0 {
        return 0;
    }
    let daily_before: i64 = conn
        .query_row(
            "SELECT count FROM daily_counts WHERE date_key=?1",
            [today_dk],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let daily_after = (daily_before - removed).max(0);

    if let Err(e) = conn.execute(
        "DELETE FROM key_counts WHERE key_name=?1 AND date_key=?2",
        rusqlite::params![key_name, today_dk],
    ) {
        tracing::error!("delete_key_today 删除 key_counts 失败: {e}");
        return 0;
    }
    if let Err(e) = conn.execute(
        "UPDATE daily_counts SET count=?1 WHERE date_key=?2",
        rusqlite::params![daily_after, today_dk],
    ) {
        tracing::error!("delete_key_today 更新 daily_counts 失败: {e}");
    }

    // 各小时按占比分摊扣减
    if daily_before > 0 {
        let hours: Vec<(i64, i64)> = conn
            .prepare("SELECT hour, count FROM hourly_counts WHERE date_key=?1")
            .and_then(|mut s| {
                s.query_map([today_dk], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
                })
                .map(|rows| rows.flatten().collect())
            })
            .unwrap_or_default();
        let mut taken: i64 = 0;
        let mut updates: Vec<(i64, i64)> = Vec::with_capacity(hours.len());
        for (h, cnt) in &hours {
            let take = cnt * removed / daily_before;
            taken += take;
            updates.push((*h, cnt - take));
        }
        // 整数分摊的余数从计数最大的小时补扣
        let residual = removed - taken;
        if residual > 0 {
            let max_hour = hours.iter().max_by_key(|(_, c)| *c).map(|(h, _)| *h);
            if let Some((_, c)) = updates.iter_mut().find(|(h, _)| Some(*h) == max_hour) {
                *c -= residual;
            }
        }
        for (h, new_count) in updates {
            if let Err(e) = conn.execute(
                "UPDATE hourly_counts SET count=?1 WHERE date_key=?2 AND hour=?3",
                rusqlite::params![new_count, today_dk, h],
            ) {
                tracing::error!("delete_key_today 更新 hourly_counts 失败: {e}");
            }
        }
        let _ = conn.execute(
            "DELETE FROM hourly_counts WHERE count <= 0 AND date_key = ?1",
            [today_dk],
        );
        let _ = conn.execute(
            "DELETE FROM daily_counts WHERE count <= 0 AND date_key = ?1",
            [today_dk],
        );
    }

    tracing::info!("已删除今日按键 [{key_name}] 的聚合记录（计数 {removed}）");
    removed
}

/// 启动时一次性聚合表一致性自愈：
/// 1. daily_counts.count 与 Σkey_counts 不符的天，以 key_counts 为准重算；
/// 2. Σhourly_counts 与 daily_counts 不符的天，按比例把小时分布缩放到当日总数
///    （hourly 无按 key 拆分，只能整体缩放；余数补到最大小时）。
///
/// 幂等：启动时每次执行都安全，写入线程尚未开始，库为落盘后的权威状态。
pub fn heal_daily_consistency() {
    for year in queries::available_years() {
        let path = paths::year_db_path(year);
        let conn = match connection::open_rw(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        // 1. daily = Σ key_counts
        let fixed_daily = conn
            .execute(
                "UPDATE daily_counts SET count = COALESCE(
                     (SELECT SUM(count) FROM key_counts WHERE key_counts.date_key = daily_counts.date_key), 0)
                 WHERE count != COALESCE(
                     (SELECT SUM(count) FROM key_counts WHERE key_counts.date_key = daily_counts.date_key), 0)",
                [],
            )
            .unwrap_or(0);
        // 无 key_counts 行但 daily_counts 有值的残留天，直接清零
        let _ = conn.execute(
            "UPDATE daily_counts SET count = 0
             WHERE count != 0 AND NOT EXISTS
                 (SELECT 1 FROM key_counts WHERE key_counts.date_key = daily_counts.date_key)",
            [],
        );
        if fixed_daily > 0 {
            tracing::info!("一致性自愈：{year} 年库修正 {fixed_daily} 天的 daily_counts");
        }

        // 2. Σhourly → daily 对齐（仅处理有偏差的天）
        let days: Vec<i64> = conn
            .prepare(
                "SELECT d.date_key FROM daily_counts d
                 JOIN (SELECT date_key, SUM(count) s FROM hourly_counts GROUP BY date_key) h
                   ON h.date_key = d.date_key
                 WHERE h.s != d.count",
            )
            .and_then(|mut s| {
                s.query_map([], |r| r.get::<_, i64>(0))
                    .map(|rows| rows.flatten().collect())
            })
            .unwrap_or_default();
        for dk in &days {
            let daily: i64 = conn
                .query_row(
                    "SELECT count FROM daily_counts WHERE date_key=?1",
                    [dk],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            scale_hourly_to_total(&conn, *dk, daily);
        }
        if !days.is_empty() {
            tracing::info!(
                "一致性自愈：{year} 年库重算 {} 天的 hourly_counts",
                days.len()
            );
            queries::invalidate_years_cache();
        }
    }
}

/// 把某天 hourly_counts 各小时按比例缩放，使 Σhourly == target_total。
/// 整数分摊：每小时扣减 floor(cnt * excess / cur_total)，余数从最大小时补扣。
fn scale_hourly_to_total(conn: &Connection, day_key: i64, target_total: i64) {
    let hours: Vec<(i64, i64)> = conn
        .prepare("SELECT hour, count FROM hourly_counts WHERE date_key=?1")
        .and_then(|mut s| {
            s.query_map([day_key], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })
            .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default();
    let cur_total: i64 = hours.iter().map(|(_, c)| c).sum();
    if cur_total <= 0 || cur_total == target_total {
        return;
    }
    let excess = cur_total - target_total; // 可能为正（多记）也可能为负（少记）
    let mut applied: i64 = 0;
    let mut updates: Vec<(i64, i64)> = Vec::with_capacity(hours.len());
    for (h, cnt) in &hours {
        let take = cnt * excess / cur_total;
        applied += take;
        updates.push((*h, cnt - take));
    }
    let residual = excess - applied;
    if residual != 0 {
        let max_hour = hours.iter().max_by_key(|(_, c)| *c).map(|(h, _)| *h);
        if let Some((_, c)) = updates.iter_mut().find(|(h, _)| Some(*h) == max_hour) {
            *c -= residual;
        }
    }
    for (h, new_count) in updates {
        let _ = conn.execute(
            "UPDATE hourly_counts SET count=?1 WHERE date_key=?2 AND hour=?3",
            rusqlite::params![new_count, day_key, h],
        );
    }
    let _ = conn.execute(
        "DELETE FROM hourly_counts WHERE count <= 0 AND date_key = ?1",
        [day_key],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 轮转分组按文件名第一段（年份/库名）：跨日期同组累加，
    /// 超出保留数删除最旧的。回归：旧实现按第二段（日期）分组，
    /// 每天自成一组导致轮转永远删不到旧备份，backup/ 无限增长。
    #[test]
    fn rotate_groups_by_first_segment() {
        let _lock = crate::paths::test_app_dir_lock();
        let dir = std::env::temp_dir().join(format!("ff_rotate_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        crate::paths::set_app_dir(&dir);
        std::fs::create_dir_all(paths::backup_dir()).unwrap();

        let names = [
            "focusflow_2026_20260911_100000.db",
            "focusflow_2026_20260912_100000.db",
            "focusflow_2026_20260912_110000.db",
            "focusflow_accounting_20260911_100000.db",
            "focusflow_accounting_20260912_100000.db",
            "focusflow_accounting_20260912_110000.db",
        ];
        for name in names {
            std::fs::write(paths::backup_dir().join(name), b"x").unwrap();
        }

        rotate_backups(2);

        let remaining: Vec<String> = std::fs::read_dir(paths::backup_dir())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(remaining.len(), 4, "两组各保留 2 个");
        // 关键回归：旧日期的必须被删（修复前按日期分组永不删除）
        assert!(
            !remaining.contains(&"focusflow_2026_20260911_100000.db".to_string()),
            "2026 组最旧备份应被删除"
        );
        assert!(
            !remaining.contains(&"focusflow_accounting_20260911_100000.db".to_string()),
            "accounting 组最旧备份应被删除"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 年度归档必须一次迁走**所有**更早年份的数据（不只上一年），且覆盖全部统计表。
    ///
    /// 回归：旧实现只调 archive_year_data(上一年)，若当前库里存在两个以上更早年份
    /// （长期未运行后由恢复文件回放、或旧版单库迁移而来），旧数据永远迁不走，
    /// 而每次启动都重复判定"有数据要归档"空跑一次 ATTACH + VACUUM；
    /// 且 active_seconds / app_usage 从不参与归档。
    #[test]
    fn archive_migrates_all_stale_years_and_all_tables() {
        let _lock = crate::paths::test_app_dir_lock();
        let dir = std::env::temp_dir().join(format!("ff_archive_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).ok();
        crate::paths::set_app_dir(&dir);

        let source_year = 2025;
        let dk = |y: i32, m: u32, d: u32| {
            queries::day_key_of_date(NaiveDate::from_ymd_opt(y, m, d).expect("date"))
        };

        // 当前年份库（2025）里混入 2023 与 2024 两年的数据
        let src_path = paths::year_db_path(source_year);
        let conn = connection::open_rw(&src_path).unwrap();
        connection::ensure_schema(&conn, source_year).unwrap();
        for (y, m, d) in [(2023, 5, 1), (2024, 6, 1), (2025, 3, 1)] {
            let k = dk(y, m, d);
            conn.execute(
                "INSERT INTO daily_counts (date_key, count) VALUES (?1, 100)",
                [k],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO active_seconds (date_key, seconds) VALUES (?1, 200)",
                [k],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO app_usage (date_key, app_name, seconds) VALUES (?1, 'a.exe', 300)",
                [k],
            )
            .unwrap();
        }
        drop(conn);

        assert!(archive_stale_years(source_year), "应迁移到往年数据");

        // 各年库都拿到了自己那一份，且三张表都跟着走
        for (y, m, d) in [(2023, 5, 1), (2024, 6, 1)] {
            let k = dk(y, m, d);
            let year_conn = connection::open_ro(&paths::year_db_path(y)).unwrap();
            let daily: i64 = year_conn
                .query_row(
                    "SELECT count FROM daily_counts WHERE date_key=?1",
                    [k],
                    |r| r.get(0),
                )
                .unwrap();
            let active: i64 = year_conn
                .query_row(
                    "SELECT seconds FROM active_seconds WHERE date_key=?1",
                    [k],
                    |r| r.get(0),
                )
                .unwrap();
            let apps: i64 = year_conn
                .query_row(
                    "SELECT seconds FROM app_usage WHERE date_key=?1",
                    [k],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!((daily, active, apps), (100, 200, 300), "{y} 年数据不完整");
        }

        // 源库只剩 2025 年自己的数据
        let src_conn = connection::open_ro(&src_path).unwrap();
        let leftover: i64 = src_conn
            .query_row(
                "SELECT COUNT(*) FROM daily_counts WHERE date_key < ?1",
                [dk(2025, 1, 1)],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(leftover, 0, "往年数据必须全部迁走");
        let kept: i64 = src_conn
            .query_row(
                "SELECT count FROM daily_counts WHERE date_key=?1",
                [dk(2025, 3, 1)],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, 100, "当年数据不能被误迁");

        // 幂等：再次归档应无事可做（旧实现会每次启动空跑一遍）
        assert!(
            !archive_stale_years(source_year),
            "无往年数据时不应重复归档"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 目标库已存在同 date_key 时必须合并计数而不是冲突回滚。
    ///
    /// 回归：旧实现裸 INSERT，主键冲突会让整个归档事务回滚，
    /// 归档从此永久失败、旧数据一直留在错误的文件里。
    #[test]
    fn archive_merges_into_existing_target_rows() {
        let _lock = crate::paths::test_app_dir_lock();
        let dir = std::env::temp_dir().join(format!("ff_archive_merge_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).ok();
        crate::paths::set_app_dir(&dir);

        let k = queries::day_key_of_date(NaiveDate::from_ymd_opt(2024, 7, 1).expect("date"));
        // 目标库（2024）已有同一天的行（历史遗留/跨年误写）
        let target = connection::open_rw(&paths::year_db_path(2024)).unwrap();
        connection::ensure_schema(&target, 2024).unwrap();
        target
            .execute(
                "INSERT INTO daily_counts (date_key, count) VALUES (?1, 5)",
                [k],
            )
            .unwrap();
        drop(target);
        // 源库（2025）里也有这一天
        let source = connection::open_rw(&paths::year_db_path(2025)).unwrap();
        connection::ensure_schema(&source, 2025).unwrap();
        source
            .execute(
                "INSERT INTO daily_counts (date_key, count) VALUES (?1, 7)",
                [k],
            )
            .unwrap();
        drop(source);

        let y0 = queries::day_key_of_date(NaiveDate::from_ymd_opt(2024, 1, 1).expect("date"));
        let y1 = queries::day_key_of_date(NaiveDate::from_ymd_opt(2025, 1, 1).expect("date"));
        assert!(
            archive_year_range(2024, 2025, y0, y1),
            "同 date_key 冲突时必须靠 UPSERT 合并完成归档，而不是回滚"
        );

        let merged: i64 = connection::open_ro(&paths::year_db_path(2024))
            .unwrap()
            .query_row(
                "SELECT count FROM daily_counts WHERE date_key=?1",
                [k],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(merged, 12, "已存在的行应合并计数（5 + 7）");

        std::fs::remove_dir_all(&dir).ok();
    }
}
