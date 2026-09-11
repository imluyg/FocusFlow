//! SQLite 连接管理。
//!
//! - WAL + synchronous=NORMAL + busy_timeout + cache_size
//! - 聚合存储：`daily_counts` / `hourly_counts` / `key_counts` 三张按天聚合表
//!   （每条按键不再落明细行，体积约为原来的 1/170）
//! - `key_log` 仅作导入暂存表（旧版数据/导入合并时使用，聚合后清空）
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
    static RO_POOL: RefCell<HashMap<PathBuf, Connection>> = RefCell::new(HashMap::new());
}

// 连接缓存上限：超过则整体清空，防止多年份库长期运行后无界增长。
const RO_POOL_MAX: usize = 4;

/// 使用缓存中的只读连接执行 `f`。连接不存在或打开失败时返回 `None`。
pub fn with_ro_conn<T>(path: &Path, f: impl FnOnce(&Connection) -> T) -> Option<T> {
    RO_POOL.with(|pool| {
        let mut pool = pool.borrow_mut();
        if pool.len() > RO_POOL_MAX {
            pool.clear();
        }
        if !pool.contains_key(path) {
            match open_ro(path) {
                Ok(conn) => {
                    pool.insert(path.to_path_buf(), conn);
                }
                Err(_) => return None,
            }
        }
        let conn = pool.get(path).unwrap();
        Some(f(conn))
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
    Ok(())
}

/// 确保指定连接的年度库 schema 存在（幂等）。
pub fn ensure_schema(conn: &Connection, year: i32) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS key_log (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            key_name TEXT NOT NULL,
            timestamp INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_timestamp ON key_log(timestamp);
        CREATE INDEX IF NOT EXISTS idx_key_name ON key_log(key_name);
        -- 按天聚合表：date_key = 本地时区下的天数序号（1970-01-01 起）
        CREATE TABLE IF NOT EXISTS daily_counts (
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
        CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );",
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('year', ?1)",
        [year.to_string()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

        drop(conn);
        std::fs::remove_dir_all(&dir).ok();
    }
}
