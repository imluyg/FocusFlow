//! 统计查询 API（聚合版）。
//!
//! 数据以按天聚合表存储（`daily_counts` / `hourly_counts` / `key_counts`），
//! 不再保留逐条按键明细。所有查询只读聚合表，不阻塞写入线程。
//! - 今日计数 / 指定周期 / 指定年度 / 指定日期
//! - 每日计数（趋势图）/ 小时分布 / 星期分布
//! - 年度列表（带缓存）

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::{Datelike, Days, Local, TimeZone};
use rusqlite::Connection;

use crate::db::connection;
use crate::paths;

/// 年度列表缓存 TTL（秒）
const YEARS_CACHE_TTL: Duration = Duration::from_secs(30);

/// 年度缓存：app_dir -> (上次构建时间, 年份列表)
///
/// `None` 表示「已被显式失效」：失效不需要伪造一个过去的 `Instant`
/// （`Instant::now() - Duration` 在系统开机时间短于该 Duration 时会 panic，
/// 而应用有开机自启，release 下 `panic = "abort"` 会直接崩进程）。
type YearsCache = std::sync::Mutex<HashMap<String, (Option<Instant>, Vec<i32>)>>;
static YEARS_CACHE: std::sync::OnceLock<YearsCache> = std::sync::OnceLock::new();

/// 查询天数上限（约 100 年）。
///
/// 所有进入日期运算的天数都必须先经过 [`clamp_query_days`]：周期值来自
/// 配置（`gui.default_period`，用户可手改）、IPC（`set_period`）与 Lua 插件
/// （`focusflow.stats`），不设上界时 `NaiveDate - Days` 越界会 panic。
pub const MAX_QUERY_DAYS: i64 = 36_500;

/// 把外部传入的天数收敛到可安全参与日期运算的区间 `1..=MAX_QUERY_DAYS`。
pub fn clamp_query_days(days: i64) -> i64 {
    days.clamp(1, MAX_QUERY_DAYS)
}

/// 统计周期取值是否合法：-1=今日 / 0=总计 / 1..=MAX_QUERY_DAYS 天。
///
/// 周期值来自前端 IPC 与 `gui.default_period`（用户可手改），必须在入口拦截：
/// 此前任意负值（如 -2）会在 `NaiveDate - Days` 处 panic，release 下
/// `panic = "abort"`，且默认周期每次启动都会重聚合 —— 应用会永久无法启动。
pub fn is_valid_period(period: i64) -> bool {
    period == -1 || (0..=MAX_QUERY_DAYS).contains(&period)
}

/// `date - days`，越界时返回 `None`（不使用会 panic 的 `Sub<Days>` 实现）。
fn date_minus_days(date: chrono::NaiveDate, days: i64) -> Option<chrono::NaiveDate> {
    date.checked_sub_days(Days::new(clamp_query_days(days) as u64))
}

fn years_cache() -> &'static YearsCache {
    YEARS_CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn cache_key() -> String {
    paths::data_dir().to_string_lossy().to_string()
}

/// 使年度列表缓存失效（归档/初始化后调用）。
pub fn invalidate_years_cache() {
    let mut c = years_cache().lock().unwrap_or_else(|e| e.into_inner());
    c.insert(cache_key(), (None, vec![]));
    // 数据文件可能被替换/移动，同时失效只读连接缓存
    crate::db::connection::clear_ro_cache();
}

/// 获取所有有数据的年份列表（降序，带 30 秒缓存，按 app_dir 隔离）。
pub fn available_years() -> Vec<i32> {
    let key = cache_key();
    {
        let c = years_cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = c.get(&key) {
            if let Some(built) = entry.0 {
                if built.elapsed() < YEARS_CACHE_TTL {
                    return entry.1.clone();
                }
            }
        }
    }
    let mut years: Vec<i32> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(paths::data_dir()) {
        for entry in entries.flatten() {
            if let Some(year) = paths::is_year_db_file(&entry.path()) {
                years.push(year);
            }
        }
    }
    years.sort_unstable_by(|a, b| b.cmp(a));
    let mut c = years_cache().lock().unwrap_or_else(|e| e.into_inner());
    c.insert(key, (Some(Instant::now()), years.clone()));
    years
}

/// 本地时区相对 UTC 的偏移秒数（如 UTC+8 = 28800 秒）。
pub(crate) fn local_utc_offset_seconds() -> i64 {
    Local::now().offset().local_minus_utc() as i64
}

/// Unix 秒 → 本地时区天数序号（1970-01-01 起）。
pub(crate) fn day_key_of_ts(ts: i64) -> i64 {
    (ts + local_utc_offset_seconds()).div_euclid(86_400)
}

/// Unix 毫秒 → 本地时区天数序号（前台应用采集的采样时刻为毫秒）。
pub(crate) fn day_key_of_ts_ms(ts_ms: i64) -> i64 {
    (ts_ms.div_euclid(1000) + local_utc_offset_seconds()).div_euclid(86_400)
}

/// 本地日期 → 天数序号。
pub(crate) fn day_key_of_date(date: chrono::NaiveDate) -> i64 {
    day_key_of_ts(local_day_start_ts(date))
}

/// 天数序号 → 本地日期。
pub(crate) fn day_key_to_date(day_key: i64) -> Option<chrono::NaiveDate> {
    chrono::DateTime::from_timestamp(day_key * 86_400, 0)
        .map(|dt| dt.with_timezone(&Local).date_naive())
}

/// Unix 秒 → 当日小时（0-23，本地时区）。
pub(crate) fn hour_of_ts(ts: i64) -> i64 {
    ((ts + local_utc_offset_seconds()).div_euclid(3600)) % 24
}

/// 查询今日按键数（聚合表）。
fn query_today_count() -> i64 {
    let dk = day_key_of_date(Local::now().date_naive());
    query_day_total(paths::current_year(), dk).unwrap_or(0)
}

/// 查询某年某天的总计数。
fn query_day_total(year: i32, day_key: i64) -> Option<i64> {
    let path = paths::year_db_path(year);
    connection::with_ro_conn(&path, |conn| {
        if !table_exists(conn, "daily_counts") {
            return None;
        }
        conn.query_row(
            "SELECT COALESCE(SUM(count), 0) FROM daily_counts WHERE date_key = ?1",
            [day_key],
            |r| r.get::<_, i64>(0),
        )
        .ok()
    })
    .flatten()
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |_| Ok(()),
    )
    .is_ok()
}

/// 获取今日按键数（写入器缓存优先，未启动写入器时查库）。
pub fn get_today_count(writer: Option<&crate::db::DbWriter>) -> i64 {
    if let Some(w) = writer {
        w.today_count() as i64
    } else {
        query_today_count()
    }
}

/// 根据查询范围确定年份列表。
fn query_years(days: Option<i64>, target_date: Option<chrono::NaiveDate>) -> Vec<i32> {
    if let Some(d) = target_date {
        return vec![d.year()];
    }
    if days.is_none() {
        return available_years();
    }
    let now = Local::now();
    // 天数先收敛再进日期运算：越界会 panic（release 下 panic=abort 直接崩进程）。
    let sanitized = clamp_query_days(days.unwrap_or(1));
    let start = date_minus_days(now.date_naive(), sanitized)
        .unwrap_or_else(|| now.date_naive() - Days::new(MAX_QUERY_DAYS as u64));
    let years: Vec<i32> = (start.year()..=now.year()).collect();
    let available: std::collections::HashSet<i32> = available_years().into_iter().collect();
    let filtered: Vec<i32> = years
        .into_iter()
        .filter(|y| available.contains(y))
        .collect();
    if filtered.is_empty() {
        vec![now.year()]
    } else {
        filtered
    }
}

/// 周期天数 → 起始 date_key（含当天，共 N 天）。None 表示不限。
fn cutoff_day_key(days: Option<i64>) -> Option<i64> {
    days.map(|d| day_key_of_date(Local::now().date_naive()) - clamp_query_days(d) + 1)
}

/// 查询统计：返回 (总数, {键名: 次数})。
pub fn get_stats(days: Option<i64>, year: Option<i32>) -> (i64, HashMap<String, i64>) {
    if let Some(y) = year {
        return stats_single_year(y, days);
    }
    let years = query_years(days, None);
    if years.len() == 1 {
        return stats_single_year(years[0], days);
    }
    stats_multi_year(&years, days)
}

/// 单库统计公共实现：`daily_counts` 求总数 + `key_counts` 按键聚合。
///
/// `cond` 为日期条件片段（如 "date_key >= ?1"），空串表示全表；`param` 与 `cond`
/// 配套（None = 无参数）。`group=true` 时键聚合用 SUM+GROUP BY（多日窗口需合并），
/// false 时单日内键唯一、直接取 count。表不存在时返回 None（调用方回退空结果）。
fn query_stats_in_conn(
    conn: &rusqlite::Connection,
    cond: &str,
    param: Option<i64>,
    group: bool,
) -> Option<(i64, HashMap<String, i64>)> {
    if !table_exists(conn, "daily_counts") {
        return None;
    }
    let where_clause = if cond.is_empty() {
        String::new()
    } else {
        format!(" WHERE {cond}")
    };
    let total: i64 = match param {
        Some(p) => conn
            .query_row(
                &format!("SELECT COALESCE(SUM(count), 0) FROM daily_counts{where_clause}"),
                [p],
                |r| r.get(0),
            )
            .unwrap_or(0),
        None => conn
            .query_row(
                "SELECT COALESCE(SUM(count), 0) FROM daily_counts",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0),
    };
    let (agg, group_clause, order) = if group {
        (
            "SUM(count) as cnt",
            " GROUP BY key_name",
            "ORDER BY cnt DESC",
        )
    } else {
        ("count", "", "ORDER BY count DESC")
    };
    let sql = format!("SELECT key_name, {agg} FROM key_counts{where_clause}{group_clause} {order}");
    let map: HashMap<String, i64> = {
        let mapper = |r: &rusqlite::Row<'_>| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?));
        let rows = match conn.prepare(&sql) {
            Ok(mut stmt) => match param {
                // SQL 出错时回退空结果并记日志，进程不因查询异常 panic 崩溃
                Some(p) => stmt
                    .query_map([p], mapper)
                    .map(|rows| rows.flatten().collect::<Vec<_>>()),
                None => stmt
                    .query_map([], mapper)
                    .map(|rows| rows.flatten().collect::<Vec<_>>()),
            },
            Err(e) => Err(e),
        };
        match rows {
            Ok(list) => list.into_iter().collect(),
            Err(e) => {
                tracing::error!("key_counts 查询失败 ({sql}): {e}");
                return None;
            }
        }
    };
    Some((total, map))
}

fn stats_single_year(year: i32, days: Option<i64>) -> (i64, HashMap<String, i64>) {
    let path = paths::year_db_path(year);
    let start_dk = cutoff_day_key(days);
    let cond = if start_dk.is_some() {
        "date_key >= ?1"
    } else {
        ""
    };
    let result = connection::with_ro_conn(&path, |conn| {
        query_stats_in_conn(conn, cond, start_dk, true)
    });
    result.flatten().unwrap_or((0, HashMap::new()))
}

/// 跨年查询：逐库聚合后在 Rust 侧合并。
fn stats_multi_year(years: &[i32], days: Option<i64>) -> (i64, HashMap<String, i64>) {
    if years.is_empty() {
        return (0, HashMap::new());
    }
    let start_dk = cutoff_day_key(days);
    let mut total_all: i64 = 0;
    let mut map_all: HashMap<String, i64> = HashMap::new();
    for year in years.iter().copied() {
        let path = paths::year_db_path(year);
        let cond = if start_dk.is_some() {
            "date_key >= ?1"
        } else {
            ""
        };
        let result = connection::with_ro_conn(&path, |conn| {
            query_stats_in_conn(conn, cond, start_dk, true)
        });
        if let Some((t, m)) = result.flatten() {
            total_all += t;
            for (k, v) in m {
                *map_all.entry(k).or_insert(0) += v;
            }
        }
    }
    (total_all, map_all)
}

/// 查询指定日期统计。
pub fn get_stats_by_date(target_date: chrono::NaiveDate) -> (i64, HashMap<String, i64>) {
    let dk = day_key_of_date(target_date);
    let path = paths::year_db_path(target_date.year());
    let result = connection::with_ro_conn(&path, |conn| {
        query_stats_in_conn(conn, "date_key = ?1", Some(dk), false)
    });
    result.flatten().unwrap_or((0, HashMap::new()))
}

/// 全历史最高单日（跨年度库）：返回 (YYYY-MM-DD, 次数)。无数据时返回 None。
pub fn get_alltime_max_day() -> Option<(String, i64)> {
    let mut best: Option<(i64, i64)> = None; // (date_key, count)
    for year in available_years() {
        let path = paths::year_db_path(year);
        if let Some((dk, c)) = connection::with_ro_conn(&path, |conn| {
            if !table_exists(conn, "daily_counts") {
                return None;
            }
            conn.query_row(
                "SELECT date_key, count FROM daily_counts ORDER BY count DESC, date_key ASC LIMIT 1",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .ok()
        })
        .flatten()
        {
            if best.is_none_or(|(_, bc)| c > bc) {
                best = Some((dk, c));
            }
        }
    }
    best.and_then(|(dk, c)| day_key_to_date(dk).map(|d| (d.format("%Y-%m-%d").to_string(), c)))
}

/// 查询最近 N 天每日按键数：返回 [(YYYY-MM-DD, 次数)]。
pub fn get_daily_counts(days: i64, year: Option<i32>) -> Vec<(String, i64)> {
    let now = Local::now();
    // 含当天共 N 天：起点为 now-(N-1)；天数先收敛，越界会让日期运算 panic。
    let days = clamp_query_days(days);
    let start = date_minus_days(now.date_naive(), days - 1)
        .unwrap_or_else(|| now.date_naive() - Days::new((MAX_QUERY_DAYS - 1) as u64));
    let start_dk = day_key_of_date(start);
    let end_dk = day_key_of_date(now.date_naive());

    let mut years_to_query: Vec<i32> = match year {
        Some(y) => vec![y],
        None => {
            let years: Vec<i32> = (start.year()..=now.year()).collect();
            let available: std::collections::HashSet<i32> = available_years().into_iter().collect();
            let f: Vec<i32> = years
                .into_iter()
                .filter(|y| available.contains(y))
                .collect();
            if f.is_empty() {
                vec![now.year()]
            } else {
                f
            }
        }
    };
    years_to_query.sort_unstable();

    // 初始化所有日期为 0
    let mut daily_map: HashMap<i64, i64> = HashMap::new();
    for i in 0..days.max(1) {
        daily_map.insert(start_dk + i, 0);
    }

    for y in &years_to_query {
        let path = paths::year_db_path(*y);
        if !path.exists() {
            continue;
        }
        let year_result = connection::with_ro_conn(&path, |conn| -> rusqlite::Result<()> {
            if !table_exists(conn, "daily_counts") {
                return Ok(());
            }
            let mut stmt = conn.prepare(
                "SELECT date_key, count FROM daily_counts WHERE date_key >= ?1 AND date_key <= ?2",
            )?;
            let rows = stmt.query_map(rusqlite::params![start_dk, end_dk], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?;
            for row in rows.flatten() {
                if let Some(e) = daily_map.get_mut(&row.0) {
                    *e += row.1;
                }
            }
            Ok(())
        });
        // 单个年度库查询失败只记日志跳过，不影响其余年份与进程存活
        if let Some(Err(e)) = year_result {
            tracing::error!("每日计数查询失败（{y} 年库）: {e}");
        }
    }

    let mut result: Vec<(String, i64)> = Vec::with_capacity(daily_map.len());
    for (dk, c) in daily_map {
        if let Some(d) = day_key_to_date(dk) {
            result.push((d.format("%Y-%m-%d").to_string(), c));
        }
    }
    result.sort();
    result
}

/// 查询指定日期每小时按键数（返回长度 24 的列表）。
pub fn get_hourly_stats(target_date: Option<chrono::NaiveDate>) -> Vec<i64> {
    let d = target_date.unwrap_or_else(|| Local::now().date_naive());
    let dk = day_key_of_date(d);
    let path = paths::year_db_path(d.year());
    let result: Option<Option<Vec<i64>>> = connection::with_ro_conn(&path, |conn| {
        if !table_exists(conn, "hourly_counts") {
            return None;
        }
        let query = (|| -> rusqlite::Result<Vec<i64>> {
            let mut stmt =
                conn.prepare("SELECT hour, count FROM hourly_counts WHERE date_key = ?1")?;
            let rows = stmt.query_map([dk], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
            let mut hourly = vec![0i64; 24];
            for row in rows.flatten() {
                if (0..24).contains(&row.0) {
                    hourly[row.0 as usize] = row.1;
                }
            }
            Ok(hourly)
        })();
        match query {
            Ok(hourly) => Some(hourly),
            Err(e) => {
                tracing::error!("小时分布查询失败: {e}");
                None
            }
        }
    });
    result.flatten().unwrap_or_else(|| vec![0i64; 24])
}

/// 查询前台应用使用时长公共实现：app_usage 求总秒数 + 按应用聚合。
/// `cond` 为日期条件片段，空串表示全表；表不存在（旧库）时返回 None。
fn query_apps_in_conn(
    conn: &rusqlite::Connection,
    cond: &str,
    param: Option<i64>,
) -> Option<(i64, HashMap<String, i64>)> {
    if !table_exists(conn, "app_usage") {
        return None;
    }
    let where_clause = if cond.is_empty() {
        String::new()
    } else {
        format!(" WHERE {cond}")
    };
    let total: i64 = match param {
        Some(p) => conn
            .query_row(
                &format!("SELECT COALESCE(SUM(seconds), 0) FROM app_usage{where_clause}"),
                [p],
                |r| r.get(0),
            )
            .unwrap_or(0),
        None => conn
            .query_row("SELECT COALESCE(SUM(seconds), 0) FROM app_usage", [], |r| {
                r.get(0)
            })
            .unwrap_or(0),
    };
    // 必须按应用聚合后再进 HashMap：app_usage 主键是 (date_key, app_name)，
    // 同一应用跨多天有多行，裸选行 collect 会同名覆盖（只剩最后一天），总时长被吃掉一大截。
    let sql =
        format!("SELECT app_name, SUM(seconds) FROM app_usage{where_clause} GROUP BY app_name");
    let map: HashMap<String, i64> = {
        let mapper = |r: &rusqlite::Row<'_>| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?));
        let rows = match conn.prepare(&sql) {
            Ok(mut stmt) => match param {
                Some(p) => stmt
                    .query_map([p], mapper)
                    .map(|rows| rows.flatten().collect::<Vec<_>>()),
                None => stmt
                    .query_map([], mapper)
                    .map(|rows| rows.flatten().collect::<Vec<_>>()),
            },
            Err(e) => Err(e),
        };
        match rows {
            Ok(list) => list.into_iter().collect(),
            Err(e) => {
                tracing::error!("app_usage 查询失败 ({sql}): {e}");
                return None;
            }
        }
    };
    Some((total, map))
}

/// 查询前台应用使用时长：返回 (总秒数, {应用名: 秒数})，周期选择与按键统计一致。
pub fn get_app_stats(days: Option<i64>, year: Option<i32>) -> (i64, HashMap<String, i64>) {
    if let Some(y) = year {
        return apps_single_year(y, days);
    }
    let years = query_years(days, None);
    if years.len() == 1 {
        return apps_single_year(years[0], days);
    }
    // 跨年逐库聚合后在 Rust 侧合并
    let start_dk = cutoff_day_key(days);
    let mut total_all: i64 = 0;
    let mut map_all: HashMap<String, i64> = HashMap::new();
    for year in years {
        let path = paths::year_db_path(year);
        let cond = if start_dk.is_some() {
            "date_key >= ?1"
        } else {
            ""
        };
        let result =
            connection::with_ro_conn(&path, |conn| query_apps_in_conn(conn, cond, start_dk));
        if let Some((t, m)) = result.flatten() {
            total_all += t;
            for (k, v) in m {
                *map_all.entry(k).or_insert(0) += v;
            }
        }
    }
    (total_all, map_all)
}

fn apps_single_year(year: i32, days: Option<i64>) -> (i64, HashMap<String, i64>) {
    let path = paths::year_db_path(year);
    let start_dk = cutoff_day_key(days);
    let cond = if start_dk.is_some() {
        "date_key >= ?1"
    } else {
        ""
    };
    let result = connection::with_ro_conn(&path, |conn| query_apps_in_conn(conn, cond, start_dk));
    result.flatten().unwrap_or((0, HashMap::new()))
}

/// 查询指定日期前台应用使用时长。
pub fn get_app_stats_by_date(target_date: chrono::NaiveDate) -> (i64, HashMap<String, i64>) {
    let dk = day_key_of_date(target_date);
    let path = paths::year_db_path(target_date.year());
    let result = connection::with_ro_conn(&path, |conn| {
        query_apps_in_conn(conn, "date_key = ?1", Some(dk))
    });
    result.flatten().unwrap_or((0, HashMap::new()))
}

/// 查询最近 N 天按星期统计（0=周一 ... 6=周日）。
pub fn get_weekday_stats(days: i64) -> HashMap<i64, i64> {
    let daily = get_daily_counts(days, None);
    aggregate_weekday(&daily)
}

/// 从每日计数列表聚合星期分布（0=周一 ... 6=周日）。
///
/// 供 UI 复用已查得的每日计数，避免重复扫描数据库。
pub fn aggregate_weekday(daily: &[(String, i64)]) -> HashMap<i64, i64> {
    let mut result: HashMap<i64, i64> = HashMap::new();
    for (date_str, count) in daily {
        if let Ok(d) = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
            let weekday = d.weekday().num_days_from_monday() as i64;
            *result.entry(weekday).or_insert(0) += count;
        }
    }
    result
}

/// 今日 Unix 秒起始。
pub fn today_start_ts() -> i64 {
    local_day_start_ts(Local::now().date_naive())
}

/// 当前 Unix 秒。
pub fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

/// 当前 Unix 毫秒。
pub fn now_ts_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 本地日期转当日起始 Unix 秒（本地时区）。
///
/// DST 空档（如春令时 0:00-1:00 不存在）时 `.single()` 返回 None，
/// 回退取最早可用映射，绝不让统计路径 panic。
fn local_day_start_ts(date: chrono::NaiveDate) -> i64 {
    let naive = match date.and_hms_opt(0, 0, 0) {
        Some(t) => t,
        None => return 0,
    };
    match Local.from_local_datetime(&naive).single() {
        Some(dt) => dt.timestamp(),
        // DST 空档取最早映射；仍失败则按 UTC 零点近似并记日志
        None => match Local.from_local_datetime(&naive).earliest() {
            Some(dt) => dt.timestamp(),
            None => {
                tracing::warn!("本地时区转换失败，跳过该日起始时间: {date}");
                naive.and_utc().timestamp()
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 周期合法性守卫：-1/0/N 合法，其他负值与超大天数非法。
    #[test]
    fn period_validation_accepts_only_known_ranges() {
        assert!(is_valid_period(-1), "-1 = 今日");
        assert!(is_valid_period(0), "0 = 总计");
        assert!(is_valid_period(1));
        assert!(is_valid_period(365));
        assert!(is_valid_period(MAX_QUERY_DAYS));
        assert!(!is_valid_period(-2), "回归：-2 曾让应用每次启动即崩溃");
        assert!(!is_valid_period(-100));
        assert!(!is_valid_period(MAX_QUERY_DAYS + 1));
        assert!(!is_valid_period(i64::MIN));
        assert!(!is_valid_period(i64::MAX));
    }

    /// 天数收敛：非法天数被夹到安全区间，不参与越界日期运算。
    #[test]
    fn clamp_query_days_bounds_everything() {
        assert_eq!(clamp_query_days(-1), 1);
        assert_eq!(clamp_query_days(0), 1);
        assert_eq!(clamp_query_days(30), 30);
        assert_eq!(clamp_query_days(i64::MAX), MAX_QUERY_DAYS);
        assert_eq!(clamp_query_days(i64::MIN), 1);
    }

    /// 回归：非法周期值不得 panic（`panic = "abort"` 下会直接终止进程，
    /// 且 `gui.default_period` 每次启动都会重聚合 —— 曾导致应用永久无法启动）。
    #[test]
    fn invalid_days_never_panic_on_date_math() {
        let _lock = crate::paths::test_app_dir_lock();
        let dir = std::env::temp_dir().join(format!("ff_period_guard_{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        crate::paths::set_app_dir(&dir);

        // 这些值此前都会让 `NaiveDate - Days::new(days as u64)` panic
        for days in [-2i64, -100, i64::MIN, i64::MAX, MAX_QUERY_DAYS + 1] {
            let _ = get_stats(Some(days), None);
            let _ = get_app_stats(Some(days), None);
            let _ = get_daily_counts(days, None);
            let _ = get_weekday_stats(days);
        }
        // 合法值同样走通
        let _ = get_stats(Some(7), None);
        let _ = get_daily_counts(7, None);
    }

    /// 含当天共 N 天：N=1 只含今天，N=200 跨 200 个日期。
    #[test]
    fn daily_counts_span_includes_today() {
        let _lock = crate::paths::test_app_dir_lock();
        let dir = std::env::temp_dir().join(format!("ff_daily_span_{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        crate::paths::set_app_dir(&dir);

        assert_eq!(get_daily_counts(1, None).len(), 1);
        assert_eq!(get_daily_counts(200, None).len(), 200);
        assert_eq!(get_daily_counts(0, None).len(), 1, "非法天数收敛为 1 天");
    }
}
