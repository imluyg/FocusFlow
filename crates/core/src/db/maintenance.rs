//! 数据库维护：聚合迁移、年度归档、备份、VACUUM、清理。
//!
//! - `migrate_v2`：旧版逐条数据 → 按天聚合表（一次性迁移 + 组合键名修正 + 压缩）
//! - `_check_yearly_archive` / `_archive_year_data`：跨年数据归档
//! - 备份（SQLite 在线备份 API）+ 轮转
//! - VACUUM + PRAGMA optimize
//! - 清理旧数据

use std::collections::HashMap;
use std::path::Path;

use chrono::{Datelike, Local, NaiveDate};
use rusqlite::Connection;

use crate::db::connection;
use crate::db::queries;
use crate::paths;

/// 检查是否需要年度归档（当前年份库中存在上一年数据时）。
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
    let prev_year = current_year - 1;
    tracing::info!("检测到 {count} 天 {prev_year} 年数据在当前库中，开始归档...");
    archive_year_data(prev_year, current_year);
}

/// 将 `source_year` 库中属于 `target_year` 的数据迁移到 `target_year` 库。
pub fn archive_year_data(target_year: i32, source_year: i32) {
    let y0 = queries::day_key_of_date(NaiveDate::from_ymd_opt(target_year, 1, 1).expect("date"));
    let y1 =
        queries::day_key_of_date(NaiveDate::from_ymd_opt(target_year + 1, 1, 1).expect("date"));

    // 1. 确保 target_year 库有表结构
    let target_path = paths::year_db_path(target_year);
    let source_path = paths::year_db_path(source_year);
    if let Err(e) = connection::open_rw(&target_path)
        .and_then(|conn| connection::ensure_schema(&conn, target_year))
    {
        tracing::error!("年度归档失败：目标库初始化失败: {e}");
        return;
    }

    // 2-4. ATTACH 迁移（三张聚合表）：任一步失败即回滚，杜绝"源库已删、目标库未写"
    let result = (|| -> anyhow::Result<()> {
        let source_str = source_path.to_str().ok_or_else(|| {
            anyhow::anyhow!("源库路径包含非 UTF-8 字符: {}", source_path.display())
        })?;
        let conn = connection::open_rw(&target_path)?;
        conn.execute("ATTACH DATABASE ?1 AS source", rusqlite::params![source_str])?;
        conn.execute("BEGIN;", [])?;
        let migrate: anyhow::Result<()> = (|| {
            for table in ["daily_counts", "hourly_counts", "key_counts"] {
                let inserted = conn.execute(
                    &format!(
                        "INSERT INTO {table} SELECT * FROM source.{table} \
                         WHERE date_key >= ?1 AND date_key < ?2"
                    ),
                    rusqlite::params![y0, y1],
                )?;
                let deleted = conn.execute(
                    &format!("DELETE FROM source.{table} WHERE date_key >= ?1 AND date_key < ?2"),
                    rusqlite::params![y0, y1],
                )?;
                if inserted != deleted {
                    anyhow::bail!(
                        "{table} 迁移行数不一致（写入 {inserted} / 删除 {deleted}），已回滚"
                    );
                }
            }
            Ok(())
        })();
        match migrate {
            Ok(()) => {
                conn.execute("COMMIT;", [])?;
            }
            Err(e) => {
                let _ = conn.execute("ROLLBACK;", []);
                return Err(e);
            }
        }
        conn.execute("DETACH DATABASE source", [])?;
        Ok(())
    })();

    match result {
        Ok(()) => {
            tracing::info!(
                "归档完成：{target_year} 年数据已迁移到 {}",
                target_path.display()
            );
            vacuum_path(&source_path);
            queries::invalidate_years_cache();
        }
        Err(e) => {
            tracing::error!("年度归档失败: {e}");
        }
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
pub fn cleanup_old_data(keep_days: i64) -> i64 {
    let cutoff_dk = queries::day_key_of_date(Local::now().date_naive()) - keep_days;
    let mut total = 0i64;
    for year in queries::available_years() {
        let path = paths::year_db_path(year);
        let conn = match connection::open_rw(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for table in ["daily_counts", "hourly_counts", "key_counts"] {
            let n = conn
                .execute(
                    &format!("DELETE FROM {table} WHERE date_key < ?1"),
                    [cutoff_dk],
                )
                .unwrap_or(0);
            total += n as i64;
        }
        tracing::info!("已清理 {year} 年 {cutoff_dk} 前的聚合数据");
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

/// 用 SQLite 在线备份 API 生成一致快照。
fn backup_db_file(src: &Path, dst: &Path) -> bool {
    let result = (|| -> anyhow::Result<()> {
        let src_conn = Connection::open(src)?;
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

/// 备份所有年度数据库到 backup/ 目录，返回首个备份路径。
pub fn backup_database(max_backups: i64) -> Option<std::path::PathBuf> {
    std::fs::create_dir_all(paths::backup_dir()).ok();
    let timestamp = Local::now().format("%Y%m%d_%H%M%S").to_string();
    let mut backed_up: Vec<std::path::PathBuf> = Vec::new();
    for year in queries::available_years() {
        let src = paths::year_db_path(year);
        if !src.exists() {
            continue;
        }
        let dst = paths::backup_dir().join(format!("focusflow_{year}_{timestamp}.db"));
        let mut ok = backup_db_file(&src, &dst);
        if !ok {
            // 兜底：checkpoint 后直接复制主文件
            if let Ok(conn) = connection::open_rw(&src) {
                let _ = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
                drop(conn);
            }
            ok = std::fs::copy(&src, &dst).is_ok();
        }
        if dst.exists() {
            backed_up.push(dst);
        }
    }
    if !backed_up.is_empty() {
        rotate_backups(max_backups);
        tracing::info!(
            "已备份 {} 个年度库到 {}",
            backed_up.len(),
            paths::backup_dir().display()
        );
        backed_up.first().cloned()
    } else {
        None
    }
}

/// 保留最近 N 个备份（按年份分组）。
fn rotate_backups(max_keep: i64) {
    let mut groups: HashMap<String, Vec<std::path::PathBuf>> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(paths::backup_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            // focusflow_2026_20260723_075125.db
            if let Some(stem) = name
                .strip_prefix("focusflow_")
                .and_then(|s| s.strip_suffix(".db"))
            {
                let parts: Vec<&str> = stem.split('_').collect();
                if parts.len() >= 3 {
                    let year = parts[1].to_string();
                    groups.entry(year).or_default().push(entry.path());
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

/// 清空所有年度库的聚合数据，返回删除行数。
pub fn reset_all_data() -> i64 {
    let mut total = 0i64;
    for year in queries::available_years() {
        let path = paths::year_db_path(year);
        let conn = match connection::open_rw(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for table in ["daily_counts", "hourly_counts", "key_counts"] {
            let n = conn
                .execute(&format!("DELETE FROM {table}"), [])
                .unwrap_or(0);
            total += n as i64;
        }
    }
    queries::invalidate_years_cache();
    tracing::info!("已清空全部键鼠记录 {total} 行");
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
        let _ = conn.execute("DELETE FROM hourly_counts WHERE count <= 0 AND date_key = ?1", [today_dk]);
        let _ = conn.execute("DELETE FROM daily_counts WHERE count <= 0 AND date_key = ?1", [today_dk]);
    }

    tracing::info!("已删除今日按键 [{key_name}] 的聚合记录（计数 {removed}）");
    removed
}

/// 启动时一次性聚合表一致性自愈：
/// 1. daily_counts.count 与 Σkey_counts 不符的天，以 key_counts 为准重算；
/// 2. Σhourly_counts 与 daily_counts 不符的天，按比例把小时分布缩放到当日总数
///    （hourly 无按 key 拆分，只能整体缩放；余数补到最大小时）。
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
            tracing::info!("一致性自愈：{year} 年库重算 {} 天的 hourly_counts", days.len());
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
            s.query_map([day_key], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
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
