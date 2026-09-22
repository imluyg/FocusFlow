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

use crate::config::FocusFlowConfig;
use crate::db::connection;
use crate::db::queries;
use crate::paths;

/// 年度库中的全部按天数据表（列名统一，date_key 均为本地天数序号）。
///
/// 归档、清理、清空必须使用同一份清单：此前只覆盖 daily/hourly/key_counts，
/// 导致 active_seconds 与 app_usage 永不归档、永不清理 —— 跨年后前台应用
/// 时长仍留在旧文件里，而 CLI 的 `--reset` 会报告"已清空"却留着这两张表。
/// （2026-09-21：`active_seconds` 已并入 `daily_counts.seconds`，不再单独成表。）
///
/// 注意 `devices`（设备字典）不在其中：它没有 date_key，不能按日期切分，
/// 归档/清空时单独处理（见 [`sync_device_dict`] 与 [`reset_all_data`]）；
/// 两张 device 明细表存的是 `devices.id`，跨库搬迁必须过 id 映射（见 [`move_device_rows`]）。
const DATA_TABLES: [&str; 6] = [
    "daily_counts",
    "hourly_counts",
    "key_counts",
    "app_usage",
    "device_counts",
    "device_key_counts",
];

/// 非主键的数值列：归档时**累加**到目标库的同名列。
///
/// `daily_counts` 有两列（2026-09-21 起合并了活跃时长），所以这里返回切片
/// 而非单个列名 —— 归档 SQL 的 `DO UPDATE SET` 要逐列拼出 `a = a + excluded.a`。
fn value_cols(table: &str) -> &'static [&'static str] {
    match table {
        "daily_counts" => &["count", "seconds"],
        "hourly_counts" | "key_counts" => &["count"],
        "app_usage" => &["seconds"],
        "device_counts" | "device_key_counts" => &["count"],
        _ => unreachable!("DATA_TABLES 新增表时必须补计数列"),
    }
}

/// 存 `devices.id` 而不是文本路径的设备表（归档时要走 id 映射）。
fn is_device_id_table(table: &str) -> bool {
    matches!(table, "device_counts" | "device_key_counts")
}

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
    if !stale.is_empty() {
        // 归档前的兜底快照（不受备份开关影响）。
        //
        // 归档是**跨库搬数据**：ATTACH 源库，在同一个事务里 INSERT 进目标年库、
        // 再 DELETE 源库数据。而 SQLite 官方文档明确：WAL 模式下「多库事务只保证
        // 每个库各自原子，整体不原子」。所以崩溃可能留下「目标库已加、源库未删」，
        // 下次启动会再次归档同一段数据，而 UPSERT 是 `count = count + excluded.count`
        // → 历史计数翻倍且无法自动识别。
        // 只有在确实检测到往年数据时才快照（正常年份不进这里），代价可接受。
        snapshot_before_destructive("yearly_archive");
    }
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

    // 1. 确保 target_year 与 source_year 库都是最新表结构
    //    （源库也必须先迁移：附着上来的旧库仍是 device_key 文本形态时，
    //     下面的 id 映射 SQL 会整段失败）
    let target_path = paths::year_db_path(target_year);
    let source_path = paths::year_db_path(source_year);
    if let Err(e) = connection::open_rw(&target_path)
        .and_then(|conn| connection::ensure_schema(&conn, target_year))
    {
        tracing::error!("年度归档失败：目标库初始化失败: {e}");
        return false;
    }
    if let Err(e) = connection::open_rw(&source_path)
        .and_then(|conn| connection::ensure_schema(&conn, source_year))
    {
        tracing::error!("年度归档失败：源库初始化失败: {e}");
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
            // 设备字典先同步 + 建 id 映射（明细表跨库搬 id 必须换算）
            sync_device_dict(&conn, dk_from, dk_to)?;
            for table in DATA_TABLES {
                if is_device_id_table(table) {
                    continue; // 单独走 id 映射搬迁
                }
                let pk_cols = match table {
                    "daily_counts" => "date_key",
                    "hourly_counts" => "date_key, hour",
                    "key_counts" => "date_key, key_name",
                    "app_usage" => "date_key, app_name",
                    _ => unreachable!("DATA_TABLES 新增表时必须补主键列"),
                };
                let cols = value_cols(table);
                let col_list = cols.join(", ");
                let updates = cols
                    .iter()
                    .map(|c| format!("{c} = {table}.{c} + excluded.{c}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                // 只迁「本表确实有行」的年份：若某年只有 daily_counts 有数据，
                // 其余表插入 0 行也删除 0 行，行数校验天然成立。
                conn.execute(
                    &format!(
                        "INSERT INTO {table} ({pk_cols}, {col_list}) \
                         SELECT {pk_cols}, {col_list} FROM source.{table} \
                          WHERE date_key >= ?1 AND date_key < ?2 \
                         ON CONFLICT({pk_cols}) DO UPDATE SET {updates}"
                    ),
                    rusqlite::params![dk_from, dk_to],
                )?;
                let deleted = conn.execute(
                    &format!("DELETE FROM source.{table} WHERE date_key >= ?1 AND date_key < ?2"),
                    rusqlite::params![dk_from, dk_to],
                )?;
                moved += deleted;
            }
            moved += move_device_rows(&conn, dk_from, dk_to)?;
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

/// 归档前把源库的设备字典同步进目标库，并建立「源 id → 目标 id」映射。
///
/// 两个库的自增 id 各不相干，明细行不能照搬 `device_id`。
/// 源库里只有统计行、没有登记行的历史数据（归档发生在设备登记表落地之前）
/// 先补一条占位登记，否则映射 JOIN 会静默丢掉这些行。
/// 调用方需保证已 ATTACH 源库为 `source` 且处于同一事务中。
fn sync_device_dict(conn: &Connection, dk_from: i64, dk_to: i64) -> anyhow::Result<()> {
    // 1. 补齐源库的孤儿登记（占位名 device-id:<n>：路径已经丢了，救不回设备名，
    //    但计数必须保住）
    for table in ["device_counts", "device_key_counts"] {
        conn.execute(
            &format!(
                "INSERT OR IGNORE INTO source.devices (device_key, name, kind)
                 SELECT '{prefix}' || c.device_id, '{prefix}' || c.device_id, 'unknown'
                   FROM source.{table} c
                  WHERE c.date_key >= ?1 AND c.date_key < ?2
                    AND NOT EXISTS (SELECT 1 FROM source.devices d WHERE d.id = c.device_id)",
                prefix = queries::ARCHIVED_DEVICE_KEY_PREFIX,
            ),
            rusqlite::params![dk_from, dk_to],
        )?;
    }
    // 2. 登记整表搬（元数据没有 date_key，不能按日期切；已存在的行保留目标库的名字）
    conn.execute(
        "INSERT OR IGNORE INTO devices (device_key, name, kind)
         SELECT device_key, name, kind FROM source.devices",
        [],
    )?;
    // 3. id 映射（临时表，事务结束即失效）
    conn.execute("DROP TABLE IF EXISTS temp._dev_id_map", [])?;
    conn.execute(
        "CREATE TEMP TABLE _dev_id_map (src_id INTEGER PRIMARY KEY, dst_id INTEGER NOT NULL)",
        [],
    )?;
    conn.execute(
        "INSERT INTO _dev_id_map (src_id, dst_id)
         SELECT s.id, t.id FROM source.devices s
           JOIN devices t ON t.device_key = s.device_key",
        [],
    )?;
    Ok(())
}

/// 搬迁两张设备明细表（`device_id` 经 `_dev_id_map` 换算），返回搬迁行数。
fn move_device_rows(conn: &Connection, dk_from: i64, dk_to: i64) -> anyhow::Result<usize> {
    let mut moved = 0usize;
    for (table, pk_cols) in [
        ("device_counts", "date_key, device_id"),
        ("device_key_counts", "date_key, device_id, key_name"),
    ] {
        let key_expr = if table == "device_counts" {
            ""
        } else {
            ", c.key_name"
        };
        conn.execute(
            &format!(
                "INSERT INTO {table} ({pk_cols}, count) \
                 SELECT c.date_key, m.dst_id{key_expr}, c.count FROM source.{table} c \
                   JOIN _dev_id_map m ON m.src_id = c.device_id \
                  WHERE c.date_key >= ?1 AND c.date_key < ?2 \
                 ON CONFLICT({pk_cols}) DO UPDATE SET count = \
                    {table}.count + excluded.count"
            ),
            rusqlite::params![dk_from, dk_to],
        )?;
        moved += conn.execute(
            &format!("DELETE FROM source.{table} WHERE date_key >= ?1 AND date_key < ?2"),
            rusqlite::params![dk_from, dk_to],
        )?;
    }
    conn.execute("DROP TABLE IF EXISTS temp._dev_id_map", [])?;
    Ok(moved)
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
        // 廉价守卫放最前：绝大多数库没有旧版暂存表，连写连接都不该开。
        if !connection::table_exists_readonly(path, "key_log") {
            return Ok(0);
        }
        let conn = connection::open_rw(path)?;
        connection::ensure_schema(&conn, year)?;
        let row_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM key_log", [], |r| r.get(0))
            .unwrap_or(0);
        if row_count == 0 {
            // 旧版 `ensure_schema` 无条件建的暂存表：空表直接丢弃，
            // 回收表 + 两个单列索引共 3 页（实测占库体积 9%）
            drop_staging_table(&conn);
            return Ok(0);
        }

        let off = queries::local_utc_offset_seconds();
        conn.execute("BEGIN IMMEDIATE;", [])?;
        // 三条聚合都必须累加而不是直接插入：目标年库很可能早有当天的聚合行
        // （新版一直在跑，之后又导入同年的旧版明细），plain INSERT 会撞主键
        // 让整段迁移回滚 —— 旧明细既聚不上也清不掉，且每次启动重复失败，
        // 导入结果看起来「成功」但界面上什么都看不见。
        // 敢累加是因为同一事务末尾会 DELETE FROM key_log：明细与聚合同生同灭，
        // 不存在重放一遍就翻倍的路径。
        // daily_counts.seconds 不进冲突分支：旧版明细没有活跃时长，
        // 用 0 覆盖会抹掉该天已有的时长统计。
        conn.execute(
            "INSERT INTO daily_counts (date_key, count)
             SELECT CAST((timestamp + ?1) / 86400 AS INTEGER), COUNT(*)
             FROM key_log GROUP BY 1
             ON CONFLICT(date_key) DO UPDATE SET count = count + excluded.count",
            [off],
        )?;
        conn.execute(
            "INSERT INTO hourly_counts (date_key, hour, count)
             SELECT CAST((timestamp + ?1) / 86400 AS INTEGER),
                    CAST(((timestamp + ?1) / 3600) % 24 AS INTEGER),
                    COUNT(*)
             FROM key_log GROUP BY 1, 2
             ON CONFLICT(date_key, hour) DO UPDATE SET count = count + excluded.count",
            [off],
        )?;
        conn.execute(
            "INSERT INTO key_counts (date_key, key_name, count)
             SELECT CAST((timestamp + ?1) / 86400 AS INTEGER), key_name, COUNT(*)
             FROM key_log GROUP BY 1, 2
             ON CONFLICT(date_key, key_name) DO UPDATE SET count = count + excluded.count",
            [off],
        )?;

        // 旧版 Ctrl+X 组合键名修正。改名是「并入」而非「替换」：同一天往往
        // 已经存在修正后的键名，直接 UPDATE 会撞 key_counts 的
        // (date_key, key_name) 主键；而 UPDATE 是整条语句作废（不是逐行跳过），
        // 结果是一个键都没改、新旧两名长期分裂，且原先的 let _ = 把它吞了。
        {
            let mut stmt = conn
                .prepare("SELECT DISTINCT key_name FROM key_counts WHERE key_name GLOB 'Ctrl+*'")?;
            let names: Vec<String> = stmt
                .query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            for old in names {
                let Some(new) = combo_key_mapping(&old) else {
                    continue;
                };
                if new == old {
                    continue;
                }
                let rows: Vec<(i64, i64)> = {
                    let mut q =
                        conn.prepare("SELECT date_key, count FROM key_counts WHERE key_name = ?1")?;
                    let list = q
                        .query_map(rusqlite::params![old], |r| Ok((r.get(0)?, r.get(1)?)))?
                        .collect::<Result<_, _>>()?;
                    list
                };
                for (date_key, count) in rows {
                    conn.execute(
                        "INSERT INTO key_counts (date_key, key_name, count)
                         VALUES (?1, ?2, ?3)
                         ON CONFLICT(date_key, key_name)
                         DO UPDATE SET count = count + excluded.count",
                        rusqlite::params![date_key, new, count],
                    )?;
                }
                conn.execute(
                    "DELETE FROM key_counts WHERE key_name = ?1",
                    rusqlite::params![old],
                )?;
            }
        }

        // 清空暂存表并标记
        conn.execute("DELETE FROM key_log", [])?;
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_v2', '1')",
            [],
        )?;
        conn.execute("COMMIT;", [])?;
        // 聚合完成后暂存表就没用了：连表一起丢掉（回收表 + 两个索引共 3 页）
        drop_staging_table(&conn);
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

/// 丢弃运行期不再使用的 `key_log` 暂存表。
///
/// 它只在「导入旧版库」时按需创建（`connection::ensure_staging_table`）；
/// 旧版本无条件建表会让每个年度库白占 3 页（表 + 两个单列索引）。
/// 注意：旧表带的 AUTOINCREMENT 会留下 1 页 `sqlite_sequence` 空壳，**删不掉** ——
/// SQLite 直接拒绝 `DROP TABLE sqlite_sequence`（实测 "table sqlite_sequence
/// may not be dropped"），VACUUM 也不回收，所以这部分体积认了。
fn drop_staging_table(conn: &Connection) {
    let _ = conn.execute("DROP TABLE IF EXISTS key_log", []);
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

/// 清理「保留 keep_days 天（含今天）」之外的数据，返回删除的聚合行数。
///
/// 口径与 `get_daily_counts` 对齐：保留 N 天 = 含今天在内的 N 个自然日。
/// 不可逆操作：执行前先做一次全量备份。
///
/// `keep_days < 1` 一律拒绝而不是钳制成 1：0 或负数会让 cutoff 落到今天甚至
/// 未来，一条 DELETE 就把全部历史（含今天）清空；而钳制同样会把误输入的 -1
/// 变成「只留今天」，两者都是不可逆的灾难，只能拒绝对方才有机会发现打错了字。
pub fn cleanup_old_data(keep_days: i64) -> i64 {
    if keep_days < 1 {
        tracing::error!("清理天数必须 >= 1（收到 {keep_days}），已拒绝");
        return 0;
    }
    snapshot_before_destructive("cleanup_old_data");
    let cutoff_dk = queries::day_key_of_date(Local::now().date_naive()) - (keep_days - 1);
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
        // 设备字典按「还有没有引用」清理（它没有 date_key，不能按日期删）
        prune_orphan_devices(&conn);
        tracing::info!("已清理 {year} 年 {cutoff_dk} 前的数据");
    }
    if total > 0 {
        tracing::info!("共清理 {total} 行聚合数据");
    }
    total
}

/// 清掉已经没有任何统计行引用的设备登记（`devices` 没有 date_key，不能按日期删）。
///
/// 别名存在 `device_aliases.json`，不受影响；设备再出现时会按同一路径重新登记。
fn prune_orphan_devices(conn: &Connection) -> usize {
    conn.execute(
        "DELETE FROM devices WHERE id NOT IN (SELECT device_id FROM device_counts) \
                                   AND id NOT IN (SELECT device_id FROM device_key_counts)",
        [],
    )
    .unwrap_or(0)
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

/// 备份文件的 sidecar 路径（WAL 模式会生成 `-wal` / `-shm`）。
fn sidecar_paths(db: &Path) -> [std::path::PathBuf; 2] {
    let mut wal = db.as_os_str().to_owned();
    wal.push("-wal");
    let mut shm = db.as_os_str().to_owned();
    shm.push("-shm");
    [std::path::PathBuf::from(wal), std::path::PathBuf::from(shm)]
}

/// 收尾备份文件，让它成为**单个自包含文件**。
///
/// 在线备份 API 会把源库的「WAL 模式」头一起复制过去，于是备份本身也是 WAL 库：
/// - 每次备份/校验都会在旁边留下 `-wal`（0 字节）与 `-shm`（32KB），轮转又只认
///   `.db`，这些 sidecar 永久堆积（实测 2 小时就攒了 66 个、1MB）
/// - 只读打开（校验逻辑、只读介质、网盘同步目录）需要目录写权限才建得出 `-shm`，
///   拿不到时会被误判为「坏备份」删掉
///
/// 所以备份完成后切回 rollback journal 并清掉 sidecar。
fn finalize_backup(dst: &Path) {
    let switched = Connection::open(dst)
        .map(|conn| conn.pragma_update(None, "journal_mode", "DELETE").is_ok())
        .unwrap_or(false);
    let [wal, shm] = sidecar_paths(dst);
    // 非空 WAL 里可能有还没并回主库的数据：只有确认切换成功、或 WAL 缺失/0 字节时才删
    let wal_empty = std::fs::metadata(&wal)
        .map(|m| m.len() == 0)
        .unwrap_or(true);
    if switched || wal_empty {
        let _ = std::fs::remove_file(&wal);
        let _ = std::fs::remove_file(&shm);
    }
}

/// 删除备份文件及其 sidecar（轮转、失败清理共用）。
fn remove_backup(db: &Path) {
    let _ = std::fs::remove_file(db);
    for p in sidecar_paths(db) {
        let _ = std::fs::remove_file(p);
    }
}

/// 清理备份目录里遗留的 sidecar（`-wal`/`-shm`）垃圾。
///
/// 旧版备份会把源库的 WAL 头复制进备份文件、并在旁边留下 sidecar，而轮转只认
/// `.db` —— 被轮转掉的备份留下孤儿 sidecar，保留下来的备份也一直挂着两个垃圾文件。
/// 这里的判据是安全的：**主库已不存在**（孤儿），或**对应 `-wal` 为空/缺失**
/// （内容已全部并回主库）。非空 WAL 绝不删（里面可能有未合并的数据）。
pub fn sweep_stale_sidecars() -> usize {
    let dir = paths::backup_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("focusflow_") {
            continue;
        }
        let Some(base) = name
            .strip_suffix("-wal")
            .or_else(|| name.strip_suffix("-shm"))
        else {
            continue;
        };
        let db_exists = dir.join(base).exists();
        let wal_empty = std::fs::metadata(dir.join(format!("{base}-wal")))
            .map(|m| m.len() == 0)
            .unwrap_or(true);
        if (!db_exists || wal_empty) && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// 启动运行中定时在线备份线程。
///
/// 每 60 秒醒来重新读一次配置（`[database] online_backup_interval_hours`，0 = 关闭），
/// 所以设置页里改开关/间隔**不必重启**：关闭期间不计时，重新打开后从零开始重新计时。
/// 备份走 SQLite 在线备份 API，不阻塞读写（与写线程的短事务天然错开）。
pub fn start_periodic_backup() {
    std::thread::Builder::new()
        .name("periodic-backup".into())
        .spawn(move || {
            tracing::info!(
                "定时在线备份线程已启动（间隔读 [database] online_backup_interval_hours，0 = 关闭）"
            );
            let tick = std::time::Duration::from_secs(60);
            let mut last = std::time::Instant::now();
            loop {
                std::thread::sleep(tick);
                let config = crate::config::instance();
                // 上限一年，避免异常配置（如 i64::MAX）在秒换算时溢出
                let hours = config
                    .get_int("database", "online_backup_interval_hours", 24)
                    .clamp(0, 24 * 365) as u64;
                if hours == 0 {
                    // 关闭期间不计时：重新打开后从零开始，不会立刻补一次备份
                    last = std::time::Instant::now();
                    continue;
                }
                if last.elapsed() >= std::time::Duration::from_secs(hours * 3600) {
                    let max_backups = config.get_int("database", "max_backups", 5);
                    if backup_database(max_backups).is_none() {
                        tracing::debug!("定时备份：无可备份的年度库");
                    }
                    last = std::time::Instant::now();
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

/// 附属数据库：`(库名, 对应插件的文件名 stem, 路径)`。
///
/// 第二个字段用于判断该插件是否已被停用 —— 停用的插件其库不再被写入，
/// 没必要占用备份轮转名额（可用 `[database] backup_disabled_plugins = true` 强制全量）。
fn auxiliary_db_paths() -> Vec<(&'static str, &'static str, std::path::PathBuf)> {
    vec![
        (
            "accounting",
            "accounting_plugin",
            crate::accounting::db_path(),
        ),
        ("pomodoro", "pomodoro_plugin", crate::pomodoro::db_path()),
        ("scheduler", "scheduler_plugin", crate::scheduler::db_path()),
        (
            "edge_history",
            "edge_history_plugin",
            crate::edge_history::edge_db_path(),
        ),
    ]
}

/// 解析 `[plugins] disabled`（逗号分隔的插件文件名 stem，与 plugins::manager 同语义）。
fn disabled_plugin_stems(config: &FocusFlowConfig) -> Vec<String> {
    config
        .get_or("plugins", "disabled", "")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// 该附属库是否参与本次备份：插件停用时跳过（`force` = 强制全量）。
fn should_backup_aux(plugin_stem: &str, disabled: &[String], force: bool) -> bool {
    force || !disabled.iter().any(|s| s == plugin_stem)
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
    // 顺手清掉遗留的 sidecar 垃圾（旧版只删 .db，`-wal`/`-shm` 会永久堆积）
    let swept = sweep_stale_sidecars();
    if swept > 0 {
        tracing::info!("已清理 {swept} 个遗留备份残留文件（-wal/-shm）");
    }
    // 毫秒精度：即使锁被绕过，同秒内的两次备份也不会撞同一个文件名
    let timestamp = Local::now().format("%Y%m%d_%H%M%S%3f").to_string();
    // 异常体检基线：本轮之前最新的一份年度库备份（用于对比体量）
    let baseline_path = newest_backup_of_group(&chrono::Local::now().year().to_string());
    let baseline_fp = baseline_path.as_deref().and_then(backup_fingerprint);
    let mut suspicious_detail: Option<String> = None;
    let mut backed_up: Vec<std::path::PathBuf> = Vec::new();
    for year in queries::available_years() {
        let src = paths::year_db_path(year);
        if !src.exists() {
            continue;
        }
        // 源库指纹必须在**任何备份动作之前**取：备份过程本身可能触碰源库
        // （兜底路径的 checkpoint 会改写主库文件），取晚了就会把"我改了源库"
        // 记成"源库变了"，下次又来一遍。
        let src_fp = source_fingerprint(&src);
        // 历史年度库归档后不再变化，没必要每轮备份都重快照一遍（全量复制 +
        // 切 journal_mode + quick_check 全读，而 backup_on_exit 默认开）。
        // 当年库始终备份（每天都在写）；历史库与本组最新备份记录的源库指纹比对，
        // 一致才跳过 —— 源库被外部改过 / 刚从归档补写 / 首次备份都会照常备份。
        if year != chrono::Local::now().year() && !year_db_needs_backup(year, src_fp) {
            tracing::debug!("跳过未变化的历史年度库备份: {year}");
            continue;
        }
        let dst = paths::backup_dir().join(format!("focusflow_{year}_{timestamp}.db"));
        let mut ok = backup_db_file(&src, &dst);
        if ok {
            // 指纹要在 finalize 之前写：finalize 负责把备份收尾成"单个自包含
            // 文件"（切回 rollback journal 并清掉 -wal/-shm）。用 open_rw 写
            // meta 会重新产生 sidecar，等于把 finalize 的成果作废。
            write_source_fingerprint(&dst, src_fp);
            finalize_backup(&dst);
            ok = verify_backup_file(&dst);
        }
        if !ok {
            // 失败的半成品会占轮转名额、顶掉好备份：先清干净再走兜底
            remove_backup(&dst);
            // 兜底：checkpoint 后直接复制主文件（复制的是变化中的文件，可能撕裂，
            // 必须校验，避免坏备份顶掉轮转中的好备份）
            let mut checkpointed = false;
            if let Ok(conn) = connection::open_rw(&src) {
                checkpointed = conn
                    .pragma_update(None, "wal_checkpoint", "TRUNCATE")
                    .is_ok();
                drop(conn);
            }
            if std::fs::copy(&src, &dst).is_ok() {
                // 只有 checkpoint 成功时 -wal 才算全部并回主文件，整文件复制才是
                // 完整快照。BUSY 时复制到的是旧的主文件，此时不能盖源库指纹：
                // 那等于宣称"这份备份已等于源库"，而主文件的 size/mtime 要等下次
                // checkpoint 才变化，期间的增量会被 year_db_needs_backup 一路跳过。
                // 不盖指纹正好落回它自己的约定 —— 指纹缺失就保守重备份。
                if checkpointed {
                    write_source_fingerprint(&dst, src_fp);
                }
                finalize_backup(&dst);
                ok = verify_backup_file(&dst);
            }
        }
        if ok {
            // 异常体检：与上一份备份比总量/天数（只对当前年份库有意义）
            if year == chrono::Local::now().year() {
                if let (Some(prev), Some(cur)) = (baseline_fp, backup_fingerprint(&dst)) {
                    if is_suspicious_change(prev, cur) {
                        suspicious_detail = Some(format!(
                            "对比对象: {}（总次数 {}，天数 {}）\n本次备份: {}（总次数 {}，天数 {}）",
                            baseline_path
                                .as_deref()
                                .map(|p| p.display().to_string())
                                .unwrap_or_default(),
                            prev.total,
                            prev.days,
                            dst.display(),
                            cur.total,
                            cur.days
                        ));
                    }
                }
            }
            backed_up.push(dst);
        } else {
            remove_backup(&dst);
            tracing::error!("年度库备份失败（已清理半成品）: {year}");
        }
    }
    // 附属库同样纳入备份与轮转（命名沿用 focusflow_{组名}_{时间戳}.db，
    // rotate_backups 按第一段分组，"accounting" 等名称各自成组）。
    // 已停用插件的数据默认跳过：其库不再被写入，备份它只会白占轮转名额。
    let config = crate::config::instance();
    let disabled = disabled_plugin_stems(config);
    let force_aux = config.get_bool("database", "backup_disabled_plugins", false);
    for (name, stem, src) in auxiliary_db_paths() {
        if !src.exists() {
            continue;
        }
        if !should_backup_aux(stem, &disabled, force_aux) {
            tracing::debug!("跳过已停用插件的数据备份: {name}（{stem}）");
            continue;
        }
        let dst = paths::backup_dir().join(format!("focusflow_{name}_{timestamp}.db"));
        let ok = backup_db_file(&src, &dst) && {
            finalize_backup(&dst);
            verify_backup_file(&dst)
        };
        if ok {
            backed_up.push(dst);
        } else {
            remove_backup(&dst);
            tracing::error!("附属库备份失败（已清理半成品）: {name}");
        }
    }
    if !backed_up.is_empty() {
        let mut policy = RetentionPolicy::from_config(config);
        // 调用方传入的 max_backups 覆盖「最近 N 份」（保持旧签名语义不变）
        policy.recent = max_backups.max(1) as usize;
        // 异常体检发现可疑 → 冻结本次轮转（不删任何旧备份）+ 留一份 SUSPECT 说明
        if let Some(detail) = suspicious_detail {
            tracing::error!(
                "备份数据异常：已冻结轮转，本次不删除任何旧备份。\n{detail}\n\
                 建议先排查统计异常，必要时用更早的备份恢复。"
            );
            write_suspect_note(&timestamp, &detail);
            rotate_backups(policy, true);
        } else {
            rotate_backups(policy, false);
        }
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

/// 备份体量指纹（只对含统计表的年度库有意义）。
///
/// 用于「异常体检」：与上一份备份对比总量与天数，骤降说明可能丢数据、
/// 暴增说明可能重复计数（例如跨年归档重复执行导致的计数翻倍）。
#[derive(Debug, Clone, Copy, PartialEq)]
struct BackupFingerprint {
    /// daily_counts 的总次数
    total: i64,
    /// daily_counts 的天数（行数）
    days: i64,
}

/// 读取备份文件的体量指纹（打不开或表缺失返回 None）。
fn backup_fingerprint(path: &Path) -> Option<BackupFingerprint> {
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let total: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(count), 0) FROM daily_counts",
            [],
            |r| r.get(0),
        )
        .ok()?;
    let days: i64 = conn
        .query_row("SELECT COUNT(*) FROM daily_counts", [], |r| r.get(0))
        .ok()?;
    Some(BackupFingerprint { total, days })
}

/// 新快照相对上一份是否「异常」。判定保守：样本太小（第一份备份、刚装好）时不判定。
fn is_suspicious_change(prev: BackupFingerprint, cur: BackupFingerprint) -> bool {
    /// 样本下限：总量低于此值时阈值没有统计意义
    const MIN_SAMPLE: i64 = 1_000;
    if prev.total < MIN_SAMPLE || cur.total < MIN_SAMPLE {
        return false;
    }
    let dropped = cur.total * 2 < prev.total; // 掉了一半以上：疑似丢数据
    let rose = cur.total > prev.total * 4; // 涨到 4 倍以上：疑似重复计数
    let days_lost = cur.days + 1 < prev.days; // 天数明显减少（一天最多新增 1 天）
    dropped || rose || days_lost
}

/// 把异常体检结果写成 `backup/SUSPECT-<时间戳>.txt`：日志之外留一份可追溯的记录，
/// 设置页也会读它做提示（轮转只认 `.db`，不会碰它）。
fn write_suspect_note(timestamp: &str, detail: &str) -> std::path::PathBuf {
    let path = paths::backup_dir().join(format!("SUSPECT-{timestamp}.txt"));
    let body = format!(
        "检测到备份数据异常，已冻结轮转（本次未删除任何旧备份）\n\
         时间: {timestamp}\n{detail}\n\
         建议：先别继续备份，确认统计是否异常（可与更早的备份对比）。\n\
         备份是单文件快照，可直接把更早的那份改名回 data/ 下的同名库来恢复。\n"
    );
    if let Err(e) = std::fs::write(&path, body) {
        tracing::error!("异常说明文件写入失败 {}: {e}", path.display());
    }
    path
}

/// 文件的修改时间（取不到返回 None）。
fn file_mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

const SOURCE_SIZE_KEY: &str = "src_size";
const SOURCE_MTIME_KEY: &str = "src_mtime";

/// 源库指纹：大小 + 纳秒级 mtime。
///
/// 不能用「源库 mtime > 备份文件 mtime」判断是否需要备份 —— 备份过程本身会触碰
/// 源库（在线备份读取、兜底路径的 checkpoint 都会更新主库时间戳），源库 mtime
/// 永远晚于备份文件，判定恒为"需要备份"，优化直接失效。所以把"这份备份对应源库
/// 的什么状态"记进备份自己的 `meta` 表，下一轮据此比对。
fn source_fingerprint(path: &Path) -> Option<(i64, i128)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    Some((meta.len() as i64, mtime.as_nanos() as i128))
}

/// 读取备份自己记录的源库指纹（旧备份没有这两个键 → None）。
fn backup_source_fingerprint(backup: &Path) -> Option<(i64, i128)> {
    let conn = Connection::open_with_flags(
        backup,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let get = |key: &str| -> Option<String> {
        conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| {
            r.get::<_, String>(0)
        })
        .ok()
    };
    let size: i64 = get(SOURCE_SIZE_KEY)?.parse().ok()?;
    let mtime: i128 = get(SOURCE_MTIME_KEY)?.parse().ok()?;
    Some((size, mtime))
}

/// 把源库指纹写进备份（必须在 [`finalize_backup`] **之前**调用）。
///
/// finalize 负责把备份收尾成"单个自包含文件"（切回 rollback journal 并清掉
/// -wal/-shm）；用 open_rw 写 meta 会重新产生 sidecar，放它后面等于白收尾。
/// 失败只记日志：备份本身已有效，只是下次会多备份一次，不能让整份备份作废。
fn write_source_fingerprint(backup: &Path, fp: Option<(i64, i128)>) {
    let Some((size, mtime)) = fp else {
        return;
    };
    let result = (|| -> anyhow::Result<()> {
        let conn = connection::open_rw(backup)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;",
        )?;
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            rusqlite::params![SOURCE_SIZE_KEY, size.to_string()],
        )?;
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            rusqlite::params![SOURCE_MTIME_KEY, mtime.to_string()],
        )?;
        Ok(())
    })();
    if let Err(e) = result {
        tracing::warn!("备份指纹写入失败 {}: {e}", backup.display());
    }
}

/// 该年度库是否还需要备份：与**本组最新备份记录的源库指纹**比对。
///
/// 指纹一致 → 这份备份就是当前源库状态的快照，跳过；任何读取失败/指纹缺失
/// （本组还没备份、或备份由更早版本产出）→ 保守备份。
fn year_db_needs_backup(year: i32, current: Option<(i64, i128)>) -> bool {
    let Some(current) = current else {
        // 源库状态读不到（刚被删/权限异常）：保守备份
        return true;
    };
    match newest_backup_of_group(&year.to_string())
        .as_deref()
        .and_then(backup_source_fingerprint)
    {
        Some(recorded) => recorded != current,
        None => true,
    }
}

/// 某分组（年份 / 插件名）中最新的一份备份。
fn newest_backup_of_group(group: &str) -> Option<std::path::PathBuf> {
    let prefix = format!("focusflow_{group}_");
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(paths::backup_dir())
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| {
                    let n = n.to_string_lossy();
                    n.starts_with(&prefix) && n.ends_with(".db")
                })
                .unwrap_or(false)
        })
        .collect();
    // sort_by_cached_key：key 里有一次 stat 系统调用，用 sort_by_key 会按比较次数重复 stat
    files.sort_by_cached_key(|p| file_mtime(p.as_path()));
    files.pop()
}

/// 保留策略：最近 N 份 + 每天/每周/每月各一份。
#[derive(Debug, Clone, Copy, PartialEq)]
struct RetentionPolicy {
    /// 最近 N 份（近端全留）
    recent: usize,
    daily: usize,
    weekly: usize,
    monthly: usize,
}

impl RetentionPolicy {
    fn from_config(config: &FocusFlowConfig) -> Self {
        let get =
            |key: &str, default: i64| config.get_int("database", key, default).max(0) as usize;
        Self {
            // 兼容旧键：max_backups 语义不变（最近 N 份）
            recent: config.get_int("database", "max_backups", 5).max(1) as usize,
            daily: get("backup_keep_daily", 7),
            weekly: get("backup_keep_weekly", 4),
            monthly: get("backup_keep_monthly", 6),
        }
    }
}

/// 从备份文件名解析日期（`focusflow_{组}_{YYYYMMDD}_{HHMMSS[mmm]}.db`），
/// 解析失败回退文件修改时间。
fn backup_file_date(path: &Path) -> Option<NaiveDate> {
    let name = path.file_name()?.to_string_lossy().to_string();
    if let Some(stem) = name.strip_suffix(".db") {
        let parts: Vec<&str> = stem.split('_').collect();
        if parts.len() >= 4 {
            if let Ok(d) = NaiveDate::parse_from_str(parts[2], "%Y%m%d") {
                return Some(d);
            }
        }
    }
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let dt: chrono::DateTime<Local> = modified.into();
    Some(dt.date_naive())
}

/// 按桶保留：每桶（天/周/月）只留最新的一份，最多 `max_buckets` 个桶（桶按新→旧取）。
/// `files` 必须已按新→旧排序；已被近端保留的条目跳过（但计入桶去重）。
fn retain_newest_per_bucket(
    files: &[std::path::PathBuf],
    keep: &mut [bool],
    max_buckets: usize,
    bucket_of: impl Fn(&Path) -> Option<String>,
) {
    if max_buckets == 0 {
        return;
    }
    let mut seen: Vec<String> = Vec::new();
    for (i, f) in files.iter().enumerate() {
        let Some(bucket) = bucket_of(f.as_path()) else {
            continue;
        };
        if seen.iter().any(|s| s == &bucket) {
            continue;
        }
        if seen.len() >= max_buckets {
            // 桶已按新→旧取满，更旧的桶整体丢弃
            return;
        }
        // 该桶最新的一份：已被近端保留也算（不重复保留，但占用桶名额）
        if !keep[i] {
            keep[i] = true;
        }
        seen.push(bucket);
    }
}

/// 选出要保留的备份（与 `files` 等长，true = 保留）。
///
/// 为什么不做「只留最新 N 份」：一旦异常数据进入统计，之后每次备份都会把它固化，
/// 最新 N 份被依次顶掉 —— 几轮之后就没有任何干净的历史了（按份数轮转时，
/// 开关几次程序就能把 5 个槽位全换一遍）。分层保留保证无论短期备份多密集，
/// 都留着「约 1 天前 / 1 周前 / 1 个月前」的快照。
fn select_retained(files: &[std::path::PathBuf], policy: RetentionPolicy) -> Vec<bool> {
    let mut keep = vec![false; files.len()];
    for k in keep.iter_mut().take(policy.recent.min(files.len())) {
        *k = true;
    }
    // 每天一份
    retain_newest_per_bucket(files, &mut keep, policy.daily, |p| {
        backup_file_date(p).map(|d| d.format("%Y%m%d").to_string())
    });
    // 每周一份（ISO 周）
    retain_newest_per_bucket(files, &mut keep, policy.weekly, |p| {
        backup_file_date(p).map(|d| format!("{}-W{:02}", d.iso_week().year(), d.iso_week().week()))
    });
    // 每月一份
    retain_newest_per_bucket(files, &mut keep, policy.monthly, |p| {
        backup_file_date(p).map(|d| d.format("%Y%m").to_string())
    });
    keep
}

/// 按保留策略清理每组的旧备份（同时删掉其 `-wal`/`-shm`）。
///
/// `freeze = true` 时**本次不删任何文件**：异常体检发现数据可疑时用它兜底，
/// 宁可多留旧备份，也不让坏数据把干净的历史顶掉。
/// 分组键取文件名第一段（`focusflow_2026_…` → "2026"，`focusflow_accounting_…`
/// → "accounting"）。此前按第二段分组取到的是日期，每天自成一组导致轮转
/// 永远删不到旧日期的备份，backup/ 目录无限增长。
fn rotate_backups(policy: RetentionPolicy, freeze: bool) {
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
    for (group, files) in groups.iter_mut() {
        files.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
        files.reverse();
        if freeze {
            tracing::warn!(
                "数据异常，冻结轮转：{group} 组保留全部 {} 份备份（未删除）",
                files.len()
            );
            continue;
        }
        let keep = select_retained(files, policy);
        let removed = keep.iter().filter(|k| !**k).count();
        for (i, f) in files.iter().enumerate() {
            if !keep[i] {
                remove_backup(f);
            }
        }
        if removed > 0 {
            tracing::debug!(
                "轮转：{group} 组删除 {removed} 份旧备份，保留 {} 份",
                files.len() - removed
            );
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
        // 设备字典也在「统计数据」范围内：统计行清空后留着旧登记名会让
        // 「已清空全部统计数据」名不副实（别名在 json 里，不受影响）
        total += conn.execute("DELETE FROM devices", []).unwrap_or(0) as i64;
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
        // 先用只读连接体检：健康的库（绝大多数）连写连接都不开，不产生 WAL 副作用、
        // 不拿写锁。此前无条件 open_rw + 两条相关子查询 UPDATE，每次启动都在每个
        // 年度库上跑一遍全表扫描，纯属白干。
        let dirty = match connection::open_ro(&path) {
            Ok(conn) => daily_inconsistency(&conn),
            Err(_) => continue,
        };
        if !dirty.has_daily_mismatch && dirty.hourly_mismatch_days.is_empty() {
            continue;
        }
        let conn = match connection::open_rw(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if dirty.has_daily_mismatch {
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
            let cleared = conn
                .execute(
                    "UPDATE daily_counts SET count = 0
                     WHERE count != 0 AND NOT EXISTS
                         (SELECT 1 FROM key_counts WHERE key_counts.date_key = daily_counts.date_key)",
                    [],
                )
                .unwrap_or(0);
            if fixed_daily > 0 || cleared > 0 {
                tracing::info!(
                    "一致性自愈：{year} 年库修正 {fixed_daily} 天 daily_counts（另清零 {cleared} 天）"
                );
            }
        }

        // Σhourly → daily 对齐（只处理确有偏差的天）
        if !dirty.hourly_mismatch_days.is_empty() {
            for dk in &dirty.hourly_mismatch_days {
                let daily: i64 = conn
                    .query_row(
                        "SELECT count FROM daily_counts WHERE date_key=?1",
                        [dk],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                scale_hourly_to_total(&conn, *dk, daily);
            }
            tracing::info!(
                "一致性自愈：{year} 年库重算 {} 天的 hourly_counts",
                dirty.hourly_mismatch_days.len()
            );
            queries::invalidate_years_cache();
        }
    }
}

/// 一致性体检结果（全部只在只读连接上得出）。
#[derive(Default)]
struct Inconsistency {
    /// daily_counts 与 Σkey_counts 存在偏差（或有残留天）
    has_daily_mismatch: bool,
    /// Σhourly 与 daily 不一致的天
    hourly_mismatch_days: Vec<i64>,
}

/// 只读体检：找出 daily/hourly 与明细表不一致的天。
fn daily_inconsistency(conn: &Connection) -> Inconsistency {
    // 1. 有没有哪天的 daily != Σkey_counts（含 daily 有值但明细全无的残留天）
    let has_daily_mismatch = conn
        .query_row(
            "SELECT 1 FROM daily_counts d
              WHERE d.count != COALESCE((SELECT SUM(count) FROM key_counts k
                                          WHERE k.date_key = d.date_key), 0)
              LIMIT 1",
            [],
            |_| Ok(()),
        )
        .is_ok();
    // 2. 有没有哪天的 Σhourly != daily
    let hourly_mismatch_days = conn
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
    Inconsistency {
        has_daily_mismatch,
        hourly_mismatch_days,
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

        rotate_backups(
            RetentionPolicy {
                recent: 2,
                daily: 0,
                weekly: 0,
                monthly: 0,
            },
            false,
        );

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

    /// 跨年归档前必须留下「归档前」快照。
    ///
    /// 回归：归档是跨库搬数据（ATTACH + INSERT 目标 + DELETE 源），WAL 下多库事务
    /// 整体不原子，崩溃可能造成「目标已加、源未删」→ 下次启动重复归档 → 计数翻倍。
    /// 此前快照只挂在 清理/清空/删除今日 上，跨年这一刻没有任何兜底。
    #[test]
    fn archiving_takes_snapshot_before_migrating() {
        let _lock = crate::paths::test_app_dir_lock();
        let dir = std::env::temp_dir().join(format!("ff_archive_snap_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).ok();
        crate::paths::set_app_dir(&dir);

        let source_year = 2025;
        let stale_year = 2024;
        let stale_dk =
            queries::day_key_of_date(NaiveDate::from_ymd_opt(stale_year, 6, 1).expect("date"));
        {
            let path = paths::year_db_path(source_year);
            let conn = connection::open_rw(&path).unwrap();
            connection::ensure_schema(&conn, source_year).unwrap();
            conn.execute(
                "INSERT INTO daily_counts (date_key, count) VALUES (?1, 77)",
                [stale_dk],
            )
            .unwrap();
        }
        queries::invalidate_years_cache();

        assert!(archive_stale_years(source_year), "应迁移往年数据");

        // 归档后：源库里往年数据已清空，目标（stale_year）库里有了
        let before: i64 = connection::open_ro(&paths::year_db_path(source_year))
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM daily_counts WHERE date_key = ?1",
                [stale_dk],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(before, 0, "源库中的往年数据应已迁走");

        // 快照必须存在，且内容是**归档前**的（源库里还能看到那条往年数据）
        let snapshots: Vec<std::path::PathBuf> = std::fs::read_dir(paths::backup_dir())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                let n = p.file_name().unwrap().to_string_lossy().to_string();
                n.starts_with(&format!("focusflow_{source_year}_")) && n.ends_with(".db")
            })
            .collect();
        assert_eq!(snapshots.len(), 1, "归档前应留下一份当年库快照");
        let snap_count: i64 = connection::open_ro(&snapshots[0])
            .unwrap()
            .query_row(
                "SELECT COALESCE(SUM(count), 0) FROM daily_counts WHERE date_key = ?1",
                [stale_dk],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            snap_count, 77,
            "快照必须是归档前的状态（能查到尚未迁走的 77 次）"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 设备维度跨库归档：整数 id 必须过映射表换算，设备登记也要跟着搬。
    ///
    /// 两个年度库的自增 id 各不相干，若照搬 device_id，明细会挂到目标库里
    /// 编号相同的另一台设备上（查询显示错名字）；而 devices 不搬则老年度
    /// 只能显示 VID/PID 回退名。
    #[test]
    fn archive_remaps_device_ids_and_carries_registry() {
        let _lock = crate::paths::test_app_dir_lock();
        let dir = std::env::temp_dir().join(format!("ff_archive_dev_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).ok();
        crate::paths::set_app_dir(&dir);

        let dev = "HID#VID_046D&PID_C52B&MI_00#7&1f126e19&0&0000";
        let other = "HID#VID_1B1C&PID_1B2D#other";
        let dk = |y: i32, m: u32, d: u32| {
            queries::day_key_of_date(NaiveDate::from_ymd_opt(y, m, d).expect("date"))
        };
        let (stale, fresh) = (dk(2024, 6, 1), dk(2025, 3, 1));

        // 目标年库先放一台「别的」设备，占掉 id=1，逼出源/目标 id 不一致
        {
            let conn = connection::open_rw(&paths::year_db_path(2024)).unwrap();
            connection::ensure_schema(&conn, 2024).unwrap();
            conn.execute(
                "INSERT INTO devices (device_key, name, kind) VALUES (?1, 'HID 鼠标 · 1B1C/1B2D', 'mouse')",
                [other],
            )
            .unwrap();
        }
        // 源库（2025）里混入 2024 年的设备数据
        {
            let conn = connection::open_rw(&paths::year_db_path(2025)).unwrap();
            connection::ensure_schema(&conn, 2025).unwrap();
            conn.execute(
                "INSERT INTO devices (device_key, name, kind) VALUES (?1, 'HID 键盘 · 046D/C52B', 'keyboard')",
                [dev],
            )
            .unwrap();
            let sid = connection::device_id_of(&conn, dev).expect("源 id");
            assert_eq!(sid, 1, "源库自己编号");
            conn.execute(
                "INSERT INTO device_counts (date_key, device_id, count) VALUES (?1, ?2, 9)",
                rusqlite::params![stale, sid],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO device_key_counts (date_key, device_id, key_name, count) VALUES (?1, ?2, '空格', 9)",
                rusqlite::params![stale, sid],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO device_counts (date_key, device_id, count) VALUES (?1, ?2, 4)",
                rusqlite::params![fresh, sid],
            )
            .unwrap();
        }

        assert!(
            archive_year_range(2024, 2025, dk(2024, 1, 1), dk(2025, 1, 1)),
            "应把 2024 年数据迁到 2024 年库"
        );

        // 目标库：登记搬到了、id 是本库的（=2，不是照搬源库的 1）、名字对得上
        let dst = connection::open_ro(&paths::year_db_path(2024)).unwrap();
        let did = connection::device_id_of(&dst, dev).expect("设备登记应随归档搬过去");
        assert_ne!(did, 1, "id 必须经映射换算，不能照搬源库编号");
        let n: i64 = dst
            .query_row(
                "SELECT count FROM device_counts WHERE date_key=?1 AND device_id=?2",
                rusqlite::params![stale, did],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 9);
        let (name, kname): (String, String) = dst
            .query_row(
                "SELECT d.name, k.key_name FROM device_key_counts k \
                   JOIN devices d ON d.id = k.device_id WHERE k.date_key=?1",
                [stale],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (name.as_str(), kname.as_str()),
            ("HID 键盘 · 046D/C52B", "空格"),
            "明细必须挂在对的设备上"
        );
        drop(dst);

        // 源库只少了往年那一行
        let src = connection::open_ro(&paths::year_db_path(2025)).unwrap();
        let sid = connection::device_id_of(&src, dev).expect("源登记仍在");
        let left: i64 = src
            .query_row(
                "SELECT SUM(count) FROM device_counts WHERE device_id=?1",
                [sid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 4, "2025 年自己的数据不得迁走");
        drop(src);

        // 端到端：按 2024 年查设备排行，名字与次数都要对
        let (total, stats) = queries::get_device_stats(None, Some(2024));
        assert_eq!(total, 9);
        let hit = stats
            .iter()
            .find(|s| s.key == dev)
            .expect("2024 年应能查到该设备");
        assert_eq!(hit.name, "HID 键盘 · 046D/C52B");
        assert_eq!(hit.count, 9);

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
            // 活跃时长已并入 daily_counts（2026-09-21），同一行同时写两列
            conn.execute(
                "INSERT INTO daily_counts (date_key, count, seconds) VALUES (?1, 100, 200)",
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
                    "SELECT seconds FROM daily_counts WHERE date_key=?1",
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

    /// 备份必须是**单个自包含文件**：不留 `-wal`/`-shm`，且能被只读打开。
    ///
    /// 回归：在线备份 API 会把源库的 WAL 头一起复制过来，于是每次备份都在旁边留下
    /// `-wal`(0B)/`-shm`(32KB)；轮转只认 `.db`，sidecar 永久堆积（实测 66 个/1MB）。
    #[test]
    fn backup_is_self_contained_and_leaves_no_junk() {
        let _lock = crate::paths::test_app_dir_lock();
        let dir = std::env::temp_dir().join(format!("ff_backup_self_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).ok();
        crate::paths::set_app_dir(&dir);

        // 源库走 WAL 模式（与运行时一致），确保覆盖「WAL 头被复制」这条路径
        let year = chrono::Local::now().year();
        let src = paths::year_db_path(year);
        {
            let conn = connection::open_rw(&src).unwrap();
            connection::ensure_schema(&conn, year).unwrap();
            conn.execute(
                "INSERT INTO daily_counts (date_key, count) VALUES (1, 42)",
                [],
            )
            .unwrap();
        }
        queries::invalidate_years_cache();

        let dst = backup_database(5).expect("应产出备份");
        let entries: Vec<String> = std::fs::read_dir(paths::backup_dir())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            entries.iter().all(|n| n.ends_with(".db")),
            "备份目录不应残留 -wal/-shm: {entries:?}"
        );
        assert!(dst.exists());

        // 只读打开（模拟校验/只读介质）：必须是 rollback 模式且数据完整
        let conn = Connection::open_with_flags(
            &dst,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .expect("备份应能被只读打开");
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "delete", "备份应收尾为 rollback journal");
        let check: String = conn
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(check, "ok");
        let count: i64 = conn
            .query_row("SELECT count FROM daily_counts WHERE date_key=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 42, "备份内容应完整");
        drop(conn);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 未变化的历史年度库不再重复备份：内容相同的快照只是白占 backup/ 与磁盘 IO。
    /// 但源库一旦真的变了（归档补写、恢复备份、手工替换）必须照常备份。
    ///
    /// 回归：判定不能用 mtime 比较（备份过程本身会更新源库时间戳，判定恒为真），
    /// 必须比对备份自己记录的源库指纹 —— 否则这个"跳过"永远不会生效。
    #[test]
    fn unchanged_historical_year_is_not_rebacked_up() {
        let _lock = crate::paths::test_app_dir_lock();
        let dir = std::env::temp_dir().join(format!("ff_backup_skip_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).ok();
        crate::paths::set_app_dir(&dir);

        let year = chrono::Local::now().year();
        let old = year - 1;
        for (y, marker) in [(year, 11i64), (old, 22i64)] {
            let conn = connection::open_rw(&paths::year_db_path(y)).unwrap();
            connection::ensure_schema(&conn, y).unwrap();
            conn.execute(
                "INSERT INTO daily_counts (date_key, count) VALUES (1, ?1)",
                [marker],
            )
            .unwrap();
        }
        queries::invalidate_years_cache();

        let backup_files = || -> Vec<String> {
            std::fs::read_dir(paths::backup_dir())
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| n.ends_with(".db"))
                .collect()
        };
        let old_prefix = format!("focusflow_{old}_");

        // 首轮：两个年度库都必须有备份（本组此前没有任何备份）
        backup_database(5).expect("首轮应有备份");
        assert!(
            backup_files().iter().any(|n| n.starts_with(&old_prefix)),
            "首轮应备份历史年度库: {:?}",
            backup_files()
        );

        // 第二轮：内容未变 → 不应新增历史年度库的备份
        let before: Vec<String> = backup_files()
            .into_iter()
            .filter(|n| n.starts_with(&old_prefix))
            .collect();
        backup_database(5).expect("第二轮应产出当年库备份");
        let after: Vec<String> = backup_files()
            .into_iter()
            .filter(|n| n.starts_with(&old_prefix))
            .collect();
        assert_eq!(
            after,
            before,
            "未变化的历史年度库不应被重复备份（新增 {:?}）",
            after
                .iter()
                .filter(|n| !before.contains(n))
                .collect::<Vec<_>>()
        );

        // 源库真的变了（模拟恢复备份/归档补写）→ 必须重新备份，否则新数据没被覆盖
        {
            let conn = connection::open_rw(&paths::year_db_path(old)).unwrap();
            conn.execute("UPDATE daily_counts SET count = 33 WHERE date_key = 1", [])
                .unwrap();
        }
        // 保证大小/时间戳确实变了（同毫秒内改写可能指纹相同）
        std::thread::sleep(std::time::Duration::from_millis(20));
        backup_database(5).expect("第三轮应产出备份");
        let changed: Vec<String> = backup_files()
            .into_iter()
            .filter(|n| n.starts_with(&old_prefix))
            .collect();
        assert!(
            changed.len() > before.len(),
            "源库变化后必须重新备份（前 {before:?}，后 {changed:?}）"
        );
        // 最新那份的内容必须是改后的值（证明备份到了新状态）
        let newest = changed.iter().max().expect("应有历史年度库备份");
        let conn = Connection::open(paths::backup_dir().join(newest)).unwrap();
        let count: i64 = conn
            .query_row("SELECT count FROM daily_counts WHERE date_key=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 33, "新备份应包含改动后的数据");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 启动自愈：不一致的天必须被修好，且体检阶段只读。
    #[test]
    fn heal_detects_before_writing_and_repairs_mismatch() {
        let _lock = crate::paths::test_app_dir_lock();
        let dir = std::env::temp_dir().join(format!("ff_heal_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).ok();
        crate::paths::set_app_dir(&dir);

        let year = chrono::Local::now().year();
        let path = paths::year_db_path(year);
        {
            let conn = connection::open_rw(&path).unwrap();
            connection::ensure_schema(&conn, year).unwrap();
            // 一致的一天
            conn.execute(
                "INSERT INTO daily_counts (date_key, count) VALUES (1, 10)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO key_counts (date_key, key_name, count) VALUES (1, 'A', 10)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO hourly_counts (date_key, hour, count) VALUES (1, 9, 10)",
                [],
            )
            .unwrap();
            // 不一致的一天：daily=99 但明细只有 4
            conn.execute(
                "INSERT INTO daily_counts (date_key, count) VALUES (2, 99)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO key_counts (date_key, key_name, count) VALUES (2, 'A', 4)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO hourly_counts (date_key, hour, count) VALUES (2, 9, 4)",
                [],
            )
            .unwrap();
        }
        queries::invalidate_years_cache();

        // 体检阶段：只读地发现第 2 天不一致
        {
            let ro = connection::open_ro(&path).unwrap();
            let d = daily_inconsistency(&ro);
            assert!(d.has_daily_mismatch, "应检出 daily 与明细不一致");
            assert_eq!(d.hourly_mismatch_days, vec![2], "只有第 2 天 hourly 偏差");
        }

        heal_daily_consistency();

        let conn = connection::open_ro(&path).unwrap();
        let day1: i64 = conn
            .query_row("SELECT count FROM daily_counts WHERE date_key=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        let day2: i64 = conn
            .query_row("SELECT count FROM daily_counts WHERE date_key=2", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(day1, 10, "一致的天不应被改动");
        assert_eq!(day2, 4, "不一致的天应被重算为 Σkey_counts");
        let sum2: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(count),0) FROM hourly_counts WHERE date_key=2",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sum2, 4, "Σhourly 应被对齐到 daily");
        drop(conn);

        // 再跑一次：已经没有可修的东西
        {
            let ro = connection::open_ro(&path).unwrap();
            let d = daily_inconsistency(&ro);
            assert!(!d.has_daily_mismatch);
            assert!(d.hourly_mismatch_days.is_empty());
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 轮转删除旧备份时要连 sidecar 一起删；遗留的 sidecar 垃圾也要被清理
    /// （包含保留下来的备份自己挂着的两个）。
    #[test]
    fn rotation_and_sweep_clean_sidecars() {
        let _lock = crate::paths::test_app_dir_lock();
        let dir = std::env::temp_dir().join(format!("ff_backup_junk_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("backup")).ok();
        crate::paths::set_app_dir(&dir);

        // 3 个年度备份（各带 sidecar）+ 2 个孤儿 sidecar（对应 .db 早已不在）
        for name in [
            "focusflow_2026_20260901_100000.db",
            "focusflow_2026_20260902_100000.db",
            "focusflow_2026_20260903_100000.db",
        ] {
            std::fs::write(paths::backup_dir().join(name), b"x").unwrap();
            std::fs::write(paths::backup_dir().join(format!("{name}-wal")), b"").unwrap();
            std::fs::write(paths::backup_dir().join(format!("{name}-shm")), b"junk").unwrap();
        }
        std::fs::write(
            paths::backup_dir().join("focusflow_2026_20260904_100000.db-wal"),
            b"",
        )
        .unwrap();
        std::fs::write(
            paths::backup_dir().join("focusflow_accounting_20260905_100000.db-shm"),
            b"junk",
        )
        .unwrap();
        std::fs::write(
            paths::backup_dir().join("focusflow_accounting_20260905_100000.db-wal"),
            b"junk",
        )
        .unwrap();
        // 非空 WAL：里面可能有没并回主库的数据，必须保留
        std::fs::write(
            paths::backup_dir().join("focusflow_2026_20260906_100000.db"),
            b"x",
        )
        .unwrap();
        std::fs::write(
            paths::backup_dir().join("focusflow_2026_20260906_100000.db-wal"),
            b"data",
        )
        .unwrap();

        let swept = sweep_stale_sidecars();
        assert_eq!(
            swept, 9,
            "9 个可安全删除（空 WAL / 主库已不存在的孤儿；非空 WAL 保留）"
        );
        assert!(
            paths::backup_dir()
                .join("focusflow_2026_20260906_100000.db-wal")
                .exists(),
            "非空 WAL 绝不能删（可能含未合并数据）"
        );

        // 轮转只保留 1 个（每组，仅近端）：被删备份的 sidecar 必须一起消失
        rotate_backups(
            RetentionPolicy {
                recent: 1,
                daily: 0,
                weekly: 0,
                monthly: 0,
            },
            false,
        );
        let left: Vec<String> = std::fs::read_dir(paths::backup_dir())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(left.len(), 2, "1 个 .db + 1 个非空 WAL，实际: {left:?}");
        assert!(
            !left.iter().any(|n| n.ends_with("-shm")),
            "空 WAL 的 sidecar 不该残留: {left:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 备份失败时不得留下半成品（半成品会占轮转名额、顶掉好备份）。    #[test]
    /// 已停用插件的数据不参与备份（可用 backup_disabled_plugins 强制全量）。
    ///
    /// 回归：此前只看文件在不在，插件关掉后其库仍被反复备份、白占轮转名额 ——
    /// 用户看到的现象是「插件都关了，备份里还有番茄钟/定时任务/Edge 历史」。
    #[test]
    fn disabled_plugin_data_skipped_in_backup() {
        let disabled = vec![
            "pomodoro_plugin".to_string(),
            "scheduler_plugin".to_string(),
            "edge_history_plugin".to_string(),
        ];
        assert!(
            should_backup_aux("accounting_plugin", &disabled, false),
            "启用中的插件照常备份"
        );
        assert!(!should_backup_aux("pomodoro_plugin", &disabled, false));
        assert!(!should_backup_aux("edge_history_plugin", &disabled, false));
        assert!(
            should_backup_aux("pomodoro_plugin", &disabled, true),
            "backup_disabled_plugins=true 时强制全量"
        );
        assert!(
            should_backup_aux("scheduler_plugin", &[], false),
            "无停用项时全部备份"
        );

        // 停用列表解析：逗号分隔 + 空白容错（与 plugins::manager 同语义）
        let dir = std::env::temp_dir().join(format!("ff_disabled_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).ok();
        let cfg_path = dir.join("config.ini");
        std::fs::write(&cfg_path, "[plugins]\ndisabled = a_plugin, b_plugin ,\n").unwrap();
        let cfg = FocusFlowConfig::load(&cfg_path).unwrap();
        assert_eq!(
            disabled_plugin_stems(&cfg),
            vec!["a_plugin".to_string(), "b_plugin".to_string()]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 附属库与插件文件的对应关系必须真实存在：插件改名后这条会立刻失败，
    /// 避免「停用开关失效」这种静默退化。
    #[test]
    fn aux_db_plugin_stems_match_plugin_files() {
        for (name, stem, _) in auxiliary_db_paths() {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("plugins")
                .join(format!("{stem}.lua"));
            assert!(
                path.exists(),
                "附属库 {name} 对应的插件文件不存在: {}（插件改名后需同步 auxiliary_db_paths）",
                path.display()
            );
        }
    }

    /// 分层保留：无论短期备份多密集，都留着「每天 / 每周 / 每月」的快照 ——
    /// 这样异常数据被连续备份时，仍有一份更早的干净副本可回滚。
    ///
    /// 回归场景：原来「只留最新 5 份」，一天内多开几次程序（退出即备份）就能
    /// 把 5 个槽位全换成坏数据，干净历史彻底消失。
    #[test]
    fn retention_keeps_daily_weekly_monthly_tiers() {
        // 造 2026-08-01 ~ 2026-09-20 每天 3 份的备份名（列表按新→旧，日内也是新→旧）
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        let start = NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
        let end = NaiveDate::from_ymd_opt(2026, 9, 20).unwrap();
        let mut d = end;
        loop {
            for t in ["235959000", "120000000", "010000000"] {
                files.push(std::path::PathBuf::from(format!(
                    "focusflow_2026_{}_{t}.db",
                    d.format("%Y%m%d")
                )));
            }
            if d == start {
                break;
            }
            d = d.pred_opt().unwrap();
        }

        let policy = RetentionPolicy {
            recent: 5,
            daily: 7,
            weekly: 4,
            monthly: 6,
        };
        let keep = select_retained(&files, policy);
        let kept: Vec<&std::path::PathBuf> = files
            .iter()
            .zip(keep.iter())
            .filter(|(_, k)| **k)
            .map(|(f, _)| f)
            .collect();

        assert_eq!(keep.len(), files.len(), "结果与输入等长");
        assert!(keep[..5].iter().all(|k| *k), "最近 5 份必须保留");
        assert!(
            kept.len() < files.len(),
            "必须真的删掉一部分（否则不算轮转）"
        );

        // 每天一份：最近 7 天里，超出「最近 5 份」窗口的那些日子必须**恰好**保留一份
        // （近端窗口内的日子会多留，属预期）
        for offset in 2..=6 {
            let day = end - chrono::Duration::days(offset);
            let day_kept: Vec<&&std::path::PathBuf> = kept
                .iter()
                .filter(|f| {
                    f.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .contains(&day.format("%Y%m%d").to_string())
                })
                .collect();
            assert_eq!(day_kept.len(), 1, "{day} 应恰好保留一份（每天一份）");
            assert!(
                day_kept[0]
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .contains("235959000"),
                "{day} 保留的应是该天最新那份"
            );
        }

        // 月级：8 月已被日/周档覆盖掉大部分，但月末那份（8-31 最新）必须留 ——
        // 它超出「最近 7 天」与「最近 4 周」，只有月档能保住它
        assert!(
            kept.iter().any(|f| f
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("20260831_235959000")),
            "月档必须保留 8 月最新一份（超出日/周窗口的干净副本）"
        );
    }

    /// 异常体检：骤降/暴涨/天数减少判定为可疑；样本太小不误报。
    #[test]
    fn suspicious_change_flags_drops_and_spikes() {
        let base = BackupFingerprint {
            total: 100_000,
            days: 120,
        };
        assert!(
            !is_suspicious_change(
                base,
                BackupFingerprint {
                    total: 102_000,
                    days: 121
                }
            ),
            "正常增长不应告警"
        );
        assert!(
            is_suspicious_change(
                base,
                BackupFingerprint {
                    total: 40_000,
                    days: 120
                }
            ),
            "总量掉一半以上：疑似丢数据"
        );
        assert!(
            is_suspicious_change(
                base,
                BackupFingerprint {
                    total: 500_000,
                    days: 120
                }
            ),
            "总量涨到 4 倍以上：疑似重复计数"
        );
        assert!(
            is_suspicious_change(
                base,
                BackupFingerprint {
                    total: 100_000,
                    days: 100
                }
            ),
            "天数明显减少（历史被清）"
        );
        assert!(
            !is_suspicious_change(
                BackupFingerprint {
                    total: 500,
                    days: 3
                },
                BackupFingerprint { total: 10, days: 1 },
            ),
            "样本太小不判定（新装用户）"
        );
    }

    /// 冻结轮转：体检可疑时一份都不删（宁可多留，也不让坏数据顶掉干净历史）。
    #[test]
    fn freeze_skips_rotation_entirely() {
        let _lock = crate::paths::test_app_dir_lock();
        let dir = std::env::temp_dir().join(format!("ff_freeze_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("backup")).ok();
        crate::paths::set_app_dir(&dir);

        for i in 0..10 {
            std::fs::write(
                paths::backup_dir().join(format!("focusflow_2026_202609{:02}_120000000.db", i + 1)),
                b"x",
            )
            .unwrap();
        }
        let policy = RetentionPolicy {
            recent: 2,
            daily: 0,
            weekly: 0,
            monthly: 0,
        };

        rotate_backups(policy, true);
        assert_eq!(
            std::fs::read_dir(paths::backup_dir()).unwrap().count(),
            10,
            "冻结时不得删除任何备份"
        );

        rotate_backups(policy, false);
        assert_eq!(
            std::fs::read_dir(paths::backup_dir()).unwrap().count(),
            2,
            "未冻结时按策略保留最近 2 份"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 备份失败时不得留下半成品（半成品会占轮转名额、顶掉好备份）。
    #[test]
    fn failed_backup_cleanup_removes_partial_file() {
        let _lock = crate::paths::test_app_dir_lock();
        let dir = std::env::temp_dir().join(format!("ff_backup_fail_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("backup")).ok();
        crate::paths::set_app_dir(&dir);

        let dst = paths::backup_dir().join("focusflow_2026_20260920_000000.db");
        std::fs::write(&dst, b"partial").unwrap();
        std::fs::write(
            dst.with_file_name("focusflow_2026_20260920_000000.db-shm"),
            b"j",
        )
        .unwrap();
        std::fs::write(
            dst.with_file_name("focusflow_2026_20260920_000000.db-wal"),
            b"",
        )
        .unwrap();

        remove_backup(&dst);

        let left: Vec<String> = std::fs::read_dir(paths::backup_dir())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(left.is_empty(), "半成品与 sidecar 都应被清掉: {left:?}");

        std::fs::remove_dir_all(&dir).ok();
    }
}
