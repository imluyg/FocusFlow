//! Edge 浏览器历史记录模块。
//!
//! 镜像 Python 版 `edge_history.py`：
//! - 直读 Edge History SQLite（%LOCALAPPDATA%\Microsoft\Edge\User Data\Default\History）
//! - Chrome 时间戳（1601-01-01 起微秒）转换
//! - 只读连接优先，被锁时复制 WAL 兜底
//! - 本地存储每日计数（focusflow_edge_history.db）供趋势图

use std::path::PathBuf;

use chrono::{Local, NaiveDate, TimeZone, Utc};
use rusqlite::Connection;

use crate::paths;

/// Edge History 数据库路径。
pub fn edge_history_path() -> PathBuf {
    if let Ok(local_app) = std::env::var("LOCALAPPDATA") {
        PathBuf::from(local_app).join(r"Microsoft\Edge\User Data\Default\History")
    } else {
        PathBuf::from(r"C:\Users\Default\AppData\Local\Microsoft\Edge\User Data\Default\History")
    }
}

/// Chrome 时间戳（1601-01-01 起微秒）转 datetime。
#[allow(dead_code)]
fn chrome_to_datetime(chrome_time: i64) -> chrono::DateTime<Utc> {
    let epoch = Utc.with_ymd_and_hms(1601, 1, 1, 0, 0, 0).unwrap();
    epoch + chrono::Duration::microseconds(chrome_time)
}

/// datetime 转 Chrome 时间戳。
fn datetime_to_chrome(dt: &chrono::DateTime<Local>) -> i64 {
    let epoch = Utc.with_ymd_and_hms(1601, 1, 1, 0, 0, 0).unwrap();
    (dt.with_timezone(&Utc) - epoch)
        .num_microseconds()
        .unwrap_or(0)
}

/// 打开 Edge History（只读优先，锁定则复制）。
/// 复制前大小保护阈值：Edge 历史库超过该大小（字节）时不再复制
/// （复制几百 MB 会卡顿，直接返回错误提示）。
const EDGE_COPY_MAX_BYTES: u64 = 100 * 1024 * 1024; // 100MB

/// 直连 busy 等待上限：Edge 运行时会频繁短事务持排他锁，
/// 等待过短会让查询在锁窗口内失败并静默返回 0；过长则批量查询时逐个干等。
/// 被锁时 300ms 后自动转复制兜底（毫秒级），整体感知最快。
const EDGE_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);

/// 复制兜底用的临时副本路径：放系统临时目录 + 随机文件名。
/// 副本包含 Edge 完整浏览记录（URL/标题/时间），不落在程序目录，
/// 且随机名 + 短生命周期把崩溃残留的风险窗口压到最低。
fn temp_copy_path() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("focusflow_edge_{}_{nanos}.db", std::process::id()))
}

/// 删除副本及其 WAL/SHM 附属文件。
fn remove_temp_copy(temp: &std::path::Path) {
    let _ = std::fs::remove_file(temp);
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{}", temp.display(), suffix));
    }
}

/// 旧版本曾把副本放在程序目录 `data/_edge_history_temp.db`，
/// 崩溃时会残留明文浏览记录，首次查询时清理历史残留。
fn cleanup_legacy_temp_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let legacy = paths::data_dir().join("_edge_history_temp.db");
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", legacy.display(), suffix));
        }
    });
}

/// 在 Edge History 上执行查询，返回 Option（None 表示读取失败/被锁）。
/// 策略：
/// 1) 只读直连（busy_timeout 1s）：Edge 未运行或锁间隙时最快。
/// 2) 查询失败（被锁）→ 复制主文件兜底重试；复制是整文件快照，
///    可能撞上 Edge 写事务产生撕裂快照，用少量重试覆盖。
///    主文件在回滚日志模式下含全部已提交数据；WAL 模式下附带复制 -wal/-shm。
fn query_edge_count(query: impl Fn(&Connection) -> Option<i64>) -> Option<i64> {
    let path = edge_history_path();
    if !path.exists() {
        return None;
    }

    // 1) 只读直连：rusqlite 打开不一定失败（锁在首个查询才报 SQLITE_BUSY），
    //    必须实际验证查询可用才采用，否则 Edge 运行时直连会静默返回 0。
    if let Ok(conn) = Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        let _ = conn.busy_timeout(EDGE_BUSY_TIMEOUT);
        if let Some(v) = query(&conn) {
            return Some(v);
        }
        drop(conn);
    }

    // 2) 复制兜底：先检查大小，超大库跳过复制避免卡顿
    cleanup_legacy_temp_once();
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if size > EDGE_COPY_MAX_BYTES {
        tracing::warn!(
            "Edge 历史库过大（{}MB），跳过复制（只读直连被锁定）",
            size / (1024 * 1024)
        );
        return None;
    }
    let temp = temp_copy_path();
    for attempt in 0..3 {
        if std::fs::copy(&path, &temp).is_ok() {
            for suffix in ["-wal", "-shm"] {
                let src = format!("{}{}", path.display(), suffix);
                if std::path::Path::new(&src).exists() {
                    let _ = std::fs::copy(&src, format!("{}{}", temp.display(), suffix));
                }
            }
            if let Ok(conn) = Connection::open(&temp) {
                if let Some(v) = query(&conn) {
                    drop(conn);
                    remove_temp_copy(&temp);
                    return Some(v);
                }
            }
        }
        remove_temp_copy(&temp);
        tracing::debug!("Edge 历史库复制查询失败（第{}次），重试", attempt + 1);
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    tracing::warn!("Edge 历史库无法读取（直连被锁且复制失败）");
    None
}

/// 查询 Edge 总历史记录数（失败/被锁返回 None）。
pub fn query_edge_total_count() -> Option<i64> {
    query_edge_count(|conn| {
        conn.query_row("SELECT COUNT(*) FROM urls", [], |r| r.get::<_, i64>(0))
            .ok()
    })
}

/// 查询指定日期的 Edge 历史记录数（失败/被锁返回 None）。
pub fn query_edge_history_count(target_date: NaiveDate) -> Option<i64> {
    let naive = target_date.and_hms_opt(0, 0, 0)?;
    // DST 空档（时钟跳变）该时刻无对应本地时间，回退取最早可用映射
    let day_start = match Local.from_local_datetime(&naive).single() {
        Some(dt) => dt,
        None => Local.from_local_datetime(&naive).earliest()?,
    };
    let day_end = day_start + chrono::Duration::days(1);
    let chrome_start = datetime_to_chrome(&day_start);
    let chrome_end = datetime_to_chrome(&day_end);
    query_edge_count(|conn| {
        conn.query_row(
            "SELECT COUNT(*) FROM urls WHERE last_visit_time >= ?1 AND last_visit_time < ?2",
            rusqlite::params![chrome_start, chrome_end],
            |r| r.get::<_, i64>(0),
        )
        .ok()
    })
}

fn edge_db_path() -> PathBuf {
    paths::data_dir().join("focusflow_edge_history.db")
}

/// 保存指定日期的计数到本地。
pub fn save_edge_history_count(target_date: NaiveDate, count: i64) {
    let conn = match Connection::open(edge_db_path()) {
        Ok(c) => c,
        Err(_) => return,
    };
    let _ = conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS edge_history (
            date TEXT PRIMARY KEY,
            count INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );",
    );
    let _ = conn.execute(
        "INSERT OR REPLACE INTO edge_history (date, count, updated_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![
            target_date.format("%Y-%m-%d").to_string(),
            count,
            Utc::now().timestamp()
        ],
    );
}

/// 获取近 N 天 Edge 历史计数。
pub fn get_edge_history_counts(days: i64) -> Vec<(String, i64)> {
    let path = edge_db_path();
    if !path.exists() {
        return Vec::new();
    }
    let conn = match Connection::open(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let start = (Local::now().date_naive() - chrono::Days::new((days - 1).max(0) as u64))
        .format("%Y-%m-%d")
        .to_string();
    let result = conn
        .prepare("SELECT date, count FROM edge_history WHERE date >= ?1 ORDER BY date")
        .and_then(|mut stmt| {
            stmt.query_map([&start], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .map(|it| it.flatten().collect())
        });
    result.unwrap_or_default()
}

/// 更新今天并返回 (是否成功, 今日数, 总数)。
/// 任一步失败（Edge 库被锁/不可读）返回 (false, 0, 0)，调用方据此提示用户，
/// 避免把失败静默当成"0 条记录"。成功后后台补齐近 30 天缺失的历史计数。
pub fn update_today_edge_history() -> (bool, i64, i64) {
    let today = Local::now().date_naive();
    match (query_edge_history_count(today), query_edge_total_count()) {
        (Some(today_count), Some(total)) => {
            save_edge_history_count(today, today_count);
            save_edge_history_meta("today", today_count);
            save_edge_history_meta("total", total);
            // 后台补齐近 30 天缺失日期（不阻塞刷新返回）
            std::thread::Builder::new()
                .name("edge-backfill".into())
                .spawn(backfill_edge_history(30))
                .ok();
            (true, today_count, total)
        }
        _ => (false, 0, 0),
    }
}

/// 补齐近 N 天缺失的 Edge 历史计数（趋势表）。
/// 只查询本地库中还没有记录的天，避免每次全量重查。
fn backfill_edge_history(days: i64) -> impl FnOnce() + Send + 'static {
    move || {
        let today = Local::now().date_naive();
        let start = today - chrono::Days::new((days - 1).max(0) as u64);
        let existing: std::collections::HashSet<String> = Connection::open(edge_db_path())
            .ok()
            .and_then(|conn| {
                conn.prepare("SELECT date FROM edge_history WHERE date >= ?1")
                    .ok()
                    .and_then(|mut stmt| {
                        stmt.query_map([start.format("%Y-%m-%d").to_string()], |r| {
                            r.get::<_, String>(0)
                        })
                        .ok()
                        .map(|it| it.flatten().collect())
                    })
            })
            .unwrap_or_default();
        let mut filled = 0;
        let mut day = start;
        while day <= today {
            let key = day.format("%Y-%m-%d").to_string();
            if !existing.contains(&key) {
                if let Some(c) = query_edge_history_count(day) {
                    save_edge_history_count(day, c);
                    filled += 1;
                }
            }
            day = day + chrono::Days::new(1);
        }
        if filled > 0 {
            tracing::info!("Edge 历史已补齐 {} 天缺失记录", filled);
        }
    }
}

/// 保存上次刷新的数值（meta 表），插件重启后恢复显示，避免出现误导性的 "—" / 0。
fn save_edge_history_meta(key: &str, value: i64) {
    let conn = match Connection::open(edge_db_path()) {
        Ok(c) => c,
        Err(_) => return,
    };
    let _ = conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    );
    let _ = conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
        rusqlite::params![key, value.to_string()],
    );
}

/// 读取上次保存的今日计数（本地缓存，未刷新过返回 None）。
pub fn get_edge_history_saved_today() -> Option<i64> {
    let conn = Connection::open(edge_db_path()).ok()?;
    conn.query_row("SELECT value FROM meta WHERE key='today'", [], |r| {
        r.get::<_, String>(0)
    })
    .ok()
    .and_then(|v| v.parse().ok())
}

/// 读取上次保存的总记录数（本地缓存，未刷新过返回 None）。
pub fn get_edge_history_saved_total() -> Option<i64> {
    let conn = Connection::open(edge_db_path()).ok()?;
    conn.query_row("SELECT value FROM meta WHERE key='total'", [], |r| {
        r.get::<_, String>(0)
    })
    .ok()
    .and_then(|v| v.parse().ok())
}

/// 本地趋势（近 N 天），供插件展示。
pub fn trend_counts(days: i64) -> Vec<(String, i64)> {
    get_edge_history_counts(days)
}
