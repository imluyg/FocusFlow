//! SQLite 连接管理。
//!
//! - WAL + synchronous=NORMAL + busy_timeout + cache_size + WAL 上限
//! - 聚合存储：`daily_counts` / `hourly_counts` / `key_counts` 三张按天聚合表
//!   （每条按键不再落明细行，体积约为原来的 1/170）
//! - 设备维度：`devices` 是整数 id 字典表，`device_counts` / `device_key_counts`
//!   只存 `device_id` —— 设备实例路径有 84~151 字符，逐行存它会让设备明细表
//!   年涨 4~7MB（是其余全部统计表的 4~7 倍），字典化后降到 ~0.5MB/年。
//!   旧库的 `device_key` TEXT 形态由 [`migrate_device_tables`] 自动转换。
//! - `key_log` 是**导入旧版库时的暂存表**，运行期不写：不再由 `ensure_schema`
//!   无条件创建（每年库白占 4 页），只在导入路径用 [`ensure_staging_table`] 建。
//! - `meta` 表记录元信息
//! - 提供读写连接与只读连接两种打开方式

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

// 线程本地只读连接缓存：同一线程内按文件路径复用，避免每次查询重开连接。
// 只读连接不参与写锁，WAL 模式下可安全并发；文件被替换/移动后通过
// [`clear_ro_cache`] 失效（归档/导入/压缩时调用）。
thread_local! {
    static RO_POOL: RefCell<HashMap<PathBuf, (Connection, std::time::Instant)>> =
        RefCell::new(HashMap::new());
}

// 连接缓存上限：超过则按 LRU 淘汰最久未用的条目，防止多年份库长期运行后无界增长。
const RO_POOL_MAX: usize = 4;

/// 使用缓存中的只读连接执行 `f`。连接不存在或打开失败时返回 `None`。
pub fn with_ro_conn<T>(path: &Path, f: impl FnOnce(&Connection) -> T) -> Option<T> {
    RO_POOL.with(|pool| {
        let mut pool = pool.borrow_mut();
        if !pool.contains_key(path) {
            // LRU 淘汰最久未用条目（此前整体 clear 会造成缓存抖动）
            if pool.len() >= RO_POOL_MAX {
                let oldest = pool
                    .iter()
                    .min_by_key(|(_, (_, last_used))| *last_used)
                    .map(|(k, _)| k.clone());
                if let Some(oldest) = oldest {
                    pool.remove(&oldest);
                }
            }
            match open_ro(path) {
                Ok(conn) => {
                    pool.insert(path.to_path_buf(), (conn, std::time::Instant::now()));
                }
                Err(e) => {
                    // 不静默：调用方普遍用 unwrap_or(0) 兜底，若不记日志，
                    // "库损坏/被锁/不可读"会表现为"今日 0 次、排行榜为空"，
                    // 用户与排查者都无法区分"没有数据"和"读不出来"。
                    tracing::error!("只读连接打开失败 {}: {e}", path.display());
                    return None;
                }
            }
        }
        match pool.get_mut(path) {
            Some((conn, last_used)) => {
                *last_used = std::time::Instant::now();
                Some(f(conn))
            }
            None => None,
        }
    })
}

/// 清空只读连接缓存（归档/导入/压缩/删除数据后调用，避免持有失效句柄）。
pub fn clear_ro_cache() {
    RO_POOL.with(|pool| pool.borrow_mut().clear());
}

/// 打开一个可写连接并应用标准 PRAGMA。
pub fn open_rw(path: &Path) -> anyhow::Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let conn = Connection::open(path)?;
    apply_rw_pragmas(&conn)?;
    Ok(conn)
}

/// 打开一个只读连接（并发读安全，不产生 WAL 副作用）。
pub fn open_ro(path: &Path) -> anyhow::Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(15))?;
    // 只读连接用较小的页缓存（默认约 2MB，聚合表查询无需大缓存）
    conn.pragma_update(None, "cache_size", -1024)?; // 1MB
    Ok(conn)
}

/// 应用与 Python 版一致的写入端 PRAGMA。
fn apply_rw_pragmas(conn: &Connection) -> anyhow::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_secs(15))?;
    conn.pragma_update(None, "cache_size", -8000)?; // 8MB 缓存
                                                    // WAL 高水位：默认 wal_autocheckpoint=1000 页（4MB）且不设 journal_size_limit 时，
                                                    // WAL 一旦涨到 4MB 就永远停在那里（帧早已 checkpoint 回主库，纯粹是文件不缩，
                                                    // 实测主库 132KB / WAL 4.14MB）。收紧阈值 + 设上限后稳定在 1MB 内。
    conn.pragma_update(None, "wal_autocheckpoint", 256)?; // 256 页 = 1MB
    conn.pragma_update(None, "journal_size_limit", 1048576)?; // checkpoint 后收缩到 1MB
    Ok(())
}

/// 表是否存在。
fn table_exists(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |_| Ok(()),
    )
    .is_ok()
}

/// 表里是否有指定列（旧形态判定用）。
fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    // 表名来自本模块内的字面量，无注入面；用 format 而非绑定参数是为了兼容
    // pragma_table_info 的参数绑定差异。
    conn.prepare(&format!(
        "SELECT 1 FROM pragma_table_info('{table}') WHERE name=?1"
    ))
    .and_then(|mut stmt| stmt.exists([column]))
    .unwrap_or(false)
}

/// 按设备实例路径取整数 id（统计表字典化后只存 id）。
pub fn device_id_of(conn: &Connection, device_key: &str) -> Option<i64> {
    conn.query_row(
        "SELECT id FROM devices WHERE device_key=?1",
        [device_key],
        |r| r.get(0),
    )
    .ok()
}

/// 确保「导入旧版库」用的暂存表 `key_log` 存在（幂等）。
///
/// 只在导入路径调用：运行期不写该表，无条件创建会让每个年度库白占
/// 表 + 索引 + `sqlite_sequence` 共 4 页（实测占库体积 12%，0 行）。
/// 兼容旧形态（rowid + AUTOINCREMENT + 两个单列索引）：用唯一索引兜住去重语义，
/// 导入侧才能安全地改用 `INSERT OR IGNORE`。
pub fn ensure_staging_table(conn: &Connection) -> anyhow::Result<()> {
    if !table_exists(conn, "key_log") {
        conn.execute_batch(
            "CREATE TABLE key_log (
                key_name TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                PRIMARY KEY (key_name, timestamp)
            ) WITHOUT ROWID;",
        )?;
    }
    conn.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_key_log_dedup ON key_log(key_name, timestamp)",
        [],
    )?;
    Ok(())
}

/// 确保指定连接的年度库 schema 存在（幂等）。
pub fn ensure_schema(conn: &Connection, year: i32) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS daily_counts (
            date_key INTEGER PRIMARY KEY,
            count INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS hourly_counts (
            date_key INTEGER NOT NULL,
            hour INTEGER NOT NULL,
            count INTEGER NOT NULL,
            PRIMARY KEY (date_key, hour)
        ) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS key_counts (
            date_key INTEGER NOT NULL,
            key_name TEXT NOT NULL,
            count INTEGER NOT NULL,
            PRIMARY KEY (date_key, key_name)
        ) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS active_seconds (
            date_key INTEGER PRIMARY KEY,
            seconds INTEGER NOT NULL
        ) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS app_usage (
            date_key INTEGER NOT NULL,
            app_name TEXT NOT NULL,
            seconds INTEGER NOT NULL,
            PRIMARY KEY (date_key, app_name)
        ) WITHOUT ROWID;
        -- 设备维度统计：date_key + 设备整数 id 按天聚合
        -- （id 指向 devices 字典表，采集侧给的是 Raw Input 设备实例路径）
        CREATE TABLE IF NOT EXISTS device_counts (
            date_key INTEGER NOT NULL,
            device_id INTEGER NOT NULL,
            count INTEGER NOT NULL,
            PRIMARY KEY (date_key, device_id)
        ) WITHOUT ROWID;
        -- 设备 × 键名明细：供「设备详情」的键名排行（独立口径，不与 key_counts 混用）
        CREATE TABLE IF NOT EXISTS device_key_counts (
            date_key INTEGER NOT NULL,
            device_id INTEGER NOT NULL,
            key_name TEXT NOT NULL,
            count INTEGER NOT NULL,
            PRIMARY KEY (date_key, device_id, key_name)
        ) WITHOUT ROWID;
        -- 设备字典表：首个事件时登记一次，之后只读缓存。
        -- id 是库内自增（各年度库互不相干），device_key 是查询入口。
        CREATE TABLE IF NOT EXISTS devices (
            id INTEGER PRIMARY KEY,
            device_key TEXT NOT NULL UNIQUE,
            name TEXT NOT NULL,
            kind TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        ) WITHOUT ROWID;",
    )?;
    // 顺序有讲究：先把旧的 device_key 文本形态迁成整数 id（旧表没有 device_id 列，
    // 索引建在它上面会直接报 "no such column"），再统一补索引。
    migrate_device_tables(conn)?;
    // 复合主键前缀是 date_key，按设备单列过滤（设备详情）只能全表扫：
    // 这两条索引把「按设备取序列 / 取键名明细」变成索引区间扫描。
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_device_counts_dev
            ON device_counts(device_id, date_key);
         CREATE INDEX IF NOT EXISTS idx_device_key_counts_dev
            ON device_key_counts(device_id, date_key);",
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('year', ?1)",
        [year.to_string()],
    )?;
    Ok(())
}

/// 把旧的「统计表直接存 device_key 文本」形态迁移成整数 id 形态（幂等）。
///
/// 旧形态每行都要带 84~151 字符的设备实例路径（实测 ≈200~300B/行），
/// 设备明细表一年涨 4~7MB。迁移只在首次遇到旧形态时执行，之后检测零成本
/// （一次 `pragma_table_info`）。整段在单事务里完成，失败即整体回滚。
fn migrate_device_tables(conn: &Connection) -> anyhow::Result<()> {
    let legacy_counts = column_exists(conn, "device_counts", "device_key");
    let legacy_detail = column_exists(conn, "device_key_counts", "device_key");
    let legacy_devices = table_exists(conn, "devices") && !column_exists(conn, "devices", "id");
    if !legacy_counts && !legacy_detail && !legacy_devices {
        return Ok(());
    }

    tracing::info!("设备统计表迁移：device_key 文本 → 整数 id 字典");
    conn.execute("BEGIN IMMEDIATE;", [])?;
    let migrate = (|| -> anyhow::Result<()> {
        conn.execute_batch(
            "DROP TABLE IF EXISTS devices_new;
             CREATE TABLE devices_new (
                id INTEGER PRIMARY KEY,
                device_key TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL,
                kind TEXT NOT NULL
             );",
        )?;
        if table_exists(conn, "devices") {
            conn.execute(
                "INSERT OR IGNORE INTO devices_new (device_key, name, kind)
                 SELECT device_key, name, kind FROM devices",
                [],
            )?;
        }
        // 统计行出现、登记表缺失的设备（归档早于登记表落地的历史库）补一行，
        // 否则下面的 JOIN 会静默丢行。
        for table in ["device_counts", "device_key_counts"] {
            if column_exists(conn, table, "device_key") {
                conn.execute(
                    &format!(
                        "INSERT OR IGNORE INTO devices_new (device_key, name, kind)
                         SELECT DISTINCT device_key, device_key, 'unknown' FROM {table}"
                    ),
                    [],
                )?;
            }
        }
        // 重建两张统计表：行数、数值不变，只把 device_key 换成 id
        for (table, pk, cols) in [
            (
                "device_counts",
                "date_key, device_id",
                "date_key, device_id, count",
            ),
            (
                "device_key_counts",
                "date_key, device_id, key_name",
                "date_key, device_id, key_name, count",
            ),
        ] {
            if !column_exists(conn, table, "device_key") {
                continue;
            }
            conn.execute_batch(&format!(
                "DROP TABLE IF EXISTS {table}_new;
                 CREATE TABLE {table}_new (
                    {cols_typed}
                    , PRIMARY KEY ({pk})
                 ) WITHOUT ROWID;",
                cols_typed = match table {
                    "device_counts" => {
                        "date_key INTEGER NOT NULL, device_id INTEGER NOT NULL, count INTEGER NOT NULL"
                    }
                    _ => "date_key INTEGER NOT NULL, device_id INTEGER NOT NULL, key_name TEXT NOT NULL, count INTEGER NOT NULL",
                }
            ))?;
            conn.execute(
                &format!(
                    "INSERT INTO {table}_new ({cols})
                     SELECT c.date_key, d.id{c_extra}, c.count FROM {table} c
                       JOIN devices_new d ON d.device_key = c.device_key",
                    c_extra = if table == "device_counts" {
                        ""
                    } else {
                        ", c.key_name"
                    }
                ),
                [],
            )?;
            conn.execute(&format!("DROP TABLE {table}"), [])?;
            conn.execute(&format!("ALTER TABLE {table}_new RENAME TO {table}"), [])?;
        }
        // 旧 devices 表整体换掉（WITHOUT ROWID + 文本主键 → 整数 id + 唯一键）
        if table_exists(conn, "devices") {
            conn.execute("DROP TABLE devices", [])?;
        }
        conn.execute("ALTER TABLE devices_new RENAME TO devices", [])?;
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_device_counts_dev
                ON device_counts(device_id, date_key);
             CREATE INDEX IF NOT EXISTS idx_device_key_counts_dev
                ON device_key_counts(device_id, date_key);",
        )?;
        Ok(())
    })();

    match migrate {
        Ok(()) => {
            conn.execute("COMMIT;", [])?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute("ROLLBACK;", []);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 旧形态（统计表直接存 device_key 文本）→ 整数 id 字典的自动迁移。
    ///
    /// 旧形态每行都要重复存 84~151 字符的设备实例路径（实测 200~300B/行，
    /// 设备明细表一年涨 4~7MB），迁移必须保住行数、计数与设备归属。
    #[test]
    fn migrates_legacy_device_key_tables_to_ids() {
        let dir = std::env::temp_dir().join("ff_rs_db_migrate_devices");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("focusflow_2026.db");
        std::fs::remove_file(&path).ok();

        let conn = open_rw(&path).unwrap();
        // 旧形态 schema（设备字典化之前）
        conn.execute_batch(
            "CREATE TABLE devices (device_key TEXT PRIMARY KEY, name TEXT NOT NULL, kind TEXT NOT NULL) WITHOUT ROWID;
             CREATE TABLE device_counts (
                date_key INTEGER NOT NULL, device_key TEXT NOT NULL, count INTEGER NOT NULL,
                PRIMARY KEY (date_key, device_key)) WITHOUT ROWID;
             CREATE TABLE device_key_counts (
                date_key INTEGER NOT NULL, device_key TEXT NOT NULL, key_name TEXT NOT NULL,
                count INTEGER NOT NULL, PRIMARY KEY (date_key, device_key, key_name)) WITHOUT ROWID;",
        )
        .unwrap();
        let kb = "HID#VID_046D&PID_C52B&MI_00#7&1f126e19&0&0000";
        conn.execute(
            "INSERT INTO devices (device_key, name, kind) VALUES (?1, 'HID 键盘 · 046D/C52B', 'keyboard')",
            [kb],
        )
        .unwrap();
        conn.execute("INSERT INTO device_counts VALUES (20716, ?1, 12)", [kb])
            .unwrap();
        conn.execute(
            "INSERT INTO device_key_counts VALUES (20716, ?1, '空格', 7)",
            [kb],
        )
        .unwrap();
        // 孤儿：只有统计行、登记表里没有（归档早于设备登记表落地的历史库）
        conn.execute(
            "INSERT INTO device_counts VALUES (20715, 'HID#ORPHAN', 3)",
            [],
        )
        .unwrap();

        ensure_schema(&conn, 2026).unwrap();

        assert!(column_exists(&conn, "devices", "id"), "devices 应有整数 id");
        assert!(!column_exists(&conn, "device_counts", "device_key"));
        assert!(
            !table_exists(&conn, "key_log"),
            "运行期不再无条件创建暂存表"
        );
        let id = device_id_of(&conn, kb).expect("登记行应保留");
        let cnt: i64 = conn
            .query_row(
                "SELECT count FROM device_counts WHERE date_key=20716 AND device_id=?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cnt, 12, "计数必须原样保留");
        let (key, n): (String, i64) = conn
            .query_row(
                "SELECT key_name, count FROM device_key_counts WHERE date_key=20716 AND device_id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((key.as_str(), n), ("空格", 7));
        let name: String = conn
            .query_row("SELECT name FROM devices WHERE device_key=?1", [kb], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(name, "HID 键盘 · 046D/C52B", "登记名不能丢");
        let orphans: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM devices WHERE device_key = 'HID#ORPHAN'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orphans, 1, "孤儿统计行必须补占位登记，不能静默丢数");
        let total: i64 = conn
            .query_row("SELECT SUM(count) FROM device_counts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 15);

        // 幂等：重复调用不得重复登记、重复加数
        ensure_schema(&conn, 2026).unwrap();
        let dev_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM devices", [], |r| r.get(0))
            .unwrap();
        assert_eq!(dev_rows, 2);
        let total2: i64 = conn
            .query_row("SELECT SUM(count) FROM device_counts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total2, 15);

        drop(conn);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn schema_roundtrip() {
        let dir = std::env::temp_dir().join("ff_rs_db_test");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("focusflow_2026.db");
        std::fs::remove_file(&path).ok();

        let conn = open_rw(&path).unwrap();
        ensure_schema(&conn, 2026).unwrap();
        conn.execute(
            "INSERT INTO daily_counts (date_key, count) VALUES (20771, 2)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO hourly_counts (date_key, hour, count) VALUES (20771, 10, 2)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO key_counts (date_key, key_name, count) VALUES (20771, 'A', 2)",
            [],
        )
        .unwrap();

        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM daily_counts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 1);

        // 新库直接就是字典化形态，且不建运行期用不到的 key_log
        assert!(column_exists(&conn, "devices", "id"));
        assert!(column_exists(&conn, "device_counts", "device_id"));
        assert!(column_exists(&conn, "device_key_counts", "device_id"));
        assert!(
            !table_exists(&conn, "key_log"),
            "key_log 只在导入时按需创建"
        );
        for idx in ["idx_device_counts_dev", "idx_device_key_counts_dev"] {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
                    [idx],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "{idx} 必须存在（设备详情按 device_key 过滤要靠它）");
        }

        drop(conn);
        std::fs::remove_dir_all(&dir).ok();
    }
}
