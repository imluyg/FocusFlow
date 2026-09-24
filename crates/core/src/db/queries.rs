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
///
/// 「有数据」是按聚合表里是否还有行判定的，不是按文件是否存在：删空的年度库、
/// 归档/清理留下的空壳都会在这里被滤掉，否则调用方（统计聚合、备份、VACUUM）
/// 每年都要为它们白跑一轮。打不开的文件保守地当作有数据。
pub fn available_years() -> Vec<i32> {
    try_available_years().unwrap_or_default()
}

/// 同 [`available_years`]（同一份缓存与口径），但把"数据目录读不出来"与
/// "真的一个年份都没有"分开返回。
///
/// 破坏性命令必须走这一条：`available_years()` 在 `read_dir` 失败时给的是空列表，
/// 于是 `--reset` 一行没删却照样打印"所有统计记录已清空 (0 行)"并退 0 ——
/// 用户以为追踪数据已经抹掉了，其实还在盘上。
pub fn try_available_years() -> anyhow::Result<Vec<i32>> {
    let key = cache_key();
    {
        let c = years_cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = c.get(&key) {
            if let Some(built) = entry.0 {
                if built.elapsed() < YEARS_CACHE_TTL {
                    return Ok(entry.1.clone());
                }
            }
        }
    }
    let mut years: Vec<i32> = Vec::new();
    let dir = paths::data_dir();
    let entries = std::fs::read_dir(&dir)
        .map_err(|e| anyhow::anyhow!("读取数据目录 {} 失败: {e}", dir.display()))?;
    for entry in entries.flatten() {
        if let Some(year) = paths::is_year_db_file(&entry.path()) {
            years.push(year);
        }
    }
    // 只保留真的有聚合行的年份。空壳年度库会永久出现在这个列表里，于是每轮刷新
    // 都要为它各开一次连接、把 6-8 趟聚合查询各跑一遍，备份与 VACUUM 也各挨一次
    // —— 而它一行数据都没有。`cleanup_old_data` 正是这种空壳的来源（按日期删空
    // 整库，但文件留着）。这里不删任何文件：判「有没有数据」就够了，删库是
    // 归档/重置那条路径的职责（它们也遵循「宁可不做，也不留没有数据来源的空库」）。
    years.retain(|y| year_has_aggregates(&paths::year_db_path(*y)));
    years.sort_unstable_by(|a, b| b.cmp(a));
    let mut c = years_cache().lock().unwrap_or_else(|e| e.into_inner());
    c.insert(key, (Some(Instant::now()), years.clone()));
    Ok(years)
}

/// 这个年度库里还有没有聚合数据行（空壳判定用）。
///
/// 读不了就按「有数据」处理：宁可多扫一趟，也不能因为一次 I/O 失败或库损坏，
/// 就把整整一年的数据从统计里抹掉——那会表现成"某一年的记录凭空消失"。
///
/// 用 `open_ro` 而不是池化的 `with_ro_conn`：本函数由 `available_years()` 调用，
/// 而它可能处在别的查询链路中间；连接池是 `RefCell`，在池内闭包里再取池会
/// panic（already mutably borrowed），release 的 panic=abort 等于进程消失。
fn year_has_aggregates(path: &std::path::Path) -> bool {
    let Ok(conn) = connection::open_ro(path) else {
        return true;
    };
    for table in crate::db::maintenance::DATA_TABLES.iter() {
        // 先问 `sqlite_master` 这张表在不在：缺表是一个**正常答案**（辅助库、旧格式
        // 文件、刚建出来的空壳），不该算成"有数据"。
        let present = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            [*table],
            |r| r.get::<_, i64>(0),
        );
        let has_rows = match present {
            Ok(0) => Ok(false),
            Ok(_) => conn
                .prepare(&format!("SELECT 1 FROM {table} LIMIT 1"))
                .and_then(|mut s| s.exists([])),
            Err(e) => Err(e),
        };
        match has_rows {
            Ok(true) => return true,
            Ok(false) => {}
            // 查都查不出来（库头合法但页损坏 / "disk image is malformed" / 半截文件）
            // **不是**"没有行"。原来只有 `open_ro` 失败走上面那条保守分支，逐表探测的
            // Err 被 `unwrap_or(false)` 折成"没行" —— 于是这种文件里的一整年会从
            // `available_years()` 凭空消失：总计、排行、趋势、启动自愈、备份、VACUUM
            // 都不再经过它，`--list-years` 也不列它，而一条日志都没有。
            // 这与本函数自己的文档、以及 `maintenance.rs` 里"读不出来 ≠ 没有行"那条
            // 规矩正好相反，所以按"有数据"办并留一条错误。
            Err(e) => {
                tracing::error!(
                    "年度库 {} 的 {table} 读不出来，本年度按「有数据」处理: {e}",
                    path.display()
                );
                return true;
            }
        }
    }
    false
}

/// 本地时区相对 UTC 的偏移秒数（如 UTC+8 = 28800 秒）。
pub(crate) fn local_utc_offset_seconds() -> i64 {
    Local::now().offset().local_minus_utc() as i64
}

/// 历元日期（1970-01-01）。取不到只可能是 chrono 自己出错，所以宁可退回 MIN
/// 也不在这儿 panic：release 是 `panic = "abort"`。
fn unix_epoch_date() -> chrono::NaiveDate {
    chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or(chrono::NaiveDate::MIN)
}

/// Unix 秒 → 本地时区天数序号（1970-01-01 起）。
///
/// 只在**写入侧**用（`writer.rs` 给刚发生的按键算桶），那里 `now` 的偏移就是
/// 那个时刻的偏移，结果与"该瞬时的本地日期序号"一致。
pub(crate) fn day_key_of_ts(ts: i64) -> i64 {
    (ts + local_utc_offset_seconds()).div_euclid(86_400)
}

/// 本地日期 → 天数序号：序号的定义就是"该日期距 1970-01-01 多少天"。
///
/// 原先写成 `day_key_of_ts(local_day_start_ts(date))`，绕一圈把该日零点的 Unix 秒
/// 再**加上此刻的**偏移：查一个偏移与今天不同的日期（有夏令时的时区里，冬天查夏天
/// 的日子）就少一天 —— 而查询条件是日期，少一天等于取回隔壁那天的数据，还不报错。
/// UTC+8 这种固定偏移看不出任何问题，所以这条一直没被撞到。
/// 与写入侧的等价性：同一天的 `ts + 当天偏移` 整除 86400 恰得该日期的历元天数。
pub(crate) fn day_key_of_date(date: chrono::NaiveDate) -> i64 {
    date.signed_duration_since(unix_epoch_date()).num_days()
}

/// 天数序号 → 本地日期：纯日历换算（见 `day_key_of_date` 里序号的定义）。
///
/// 原先是 `from_timestamp(day_key * 86_400).with_timezone(&Local)`，等于把时区偏移
/// **第二次**加上去：正偏移（含 UTC+8）恰好还在同一天所以看不出来，负偏移（美洲）
/// 会把每一行日数据标到前一天。
pub(crate) fn day_key_to_date(day_key: i64) -> Option<chrono::NaiveDate> {
    unix_epoch_date().checked_add_days(chrono::Days::new(u64::try_from(day_key).ok()?))
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

/// 全历史汇总（跨年度库）：返回 (总次数, 最高单日)。
///
/// 「总计」卡片与「最高单日」卡片共用同一份失效条件（强制刷新 / 跨天 /
/// 今日新增达阈值），因此两者在同一次跨库遍历里一起取出：`daily_counts`
/// 每天一行，SUM 与 ORDER BY 都是小表扫描，多取一个总数不增加连接开销。
pub fn get_alltime_summary() -> (i64, Option<(String, i64)>) {
    let mut total: i64 = 0;
    let mut best: Option<(i64, i64)> = None; // (date_key, count)
    for year in available_years() {
        let path = paths::year_db_path(year);
        let row = connection::with_ro_conn(&path, |conn| {
            if !table_exists(conn, "daily_counts") {
                return None;
            }
            let sum: i64 = conn
                .query_row(
                    "SELECT COALESCE(SUM(count), 0) FROM daily_counts",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            let top = conn
                .query_row(
                    "SELECT date_key, count FROM daily_counts ORDER BY count DESC, date_key ASC LIMIT 1",
                    [],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
                )
                .ok();
            Some((sum, top))
        })
        .flatten();
        if let Some((sum, top)) = row {
            total += sum;
            if let Some((dk, c)) = top {
                if best.is_none_or(|(_, bc)| c > bc) {
                    best = Some((dk, c));
                }
            }
        }
    }
    let max_day =
        best.and_then(|(dk, c)| day_key_to_date(dk).map(|d| (d.format("%Y-%m-%d").to_string(), c)));
    (total, max_day)
}

/// 全历史最高单日（跨年度库）：返回 (YYYY-MM-DD, 次数)。无数据时返回 None。
pub fn get_alltime_max_day() -> Option<(String, i64)> {
    get_alltime_summary().1
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

// ===== 设备维度统计（Raw Input 侧信道，独立口径）=====

/// 设备统计行（查询结果：显示名已按设备去重）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DeviceStat {
    /// 设备实例路径（Raw Input device_key）：改名/别名回写用，界面不直接展示
    pub key: String,
    /// 展示名：优先用户别名，其次自动解析名（如 "HID-compliant mouse · 046D/C52B"）
    pub name: String,
    /// 自动解析出的名字（未套别名）。有别名时界面用它做副标题，方便认设备
    pub auto_name: String,
    /// 是否已设用户别名（界面据此显示「还原」入口）
    pub has_alias: bool,
    /// 设备类型：mouse / keyboard / hybrid
    pub kind: String,
    /// 输入次数（键盘按下 + 鼠标按键按下 + 滚轮，独立口径）
    pub count: i64,
}

/// 跨年归档给「有统计行、没登记行」的历史设备补的 device_key 前缀（见 `sync_device_dict`）。
///
/// 这类设备的真实实例路径当时没登记，已经不可考。常数放在这里是为了让
/// 写入侧（生成占位）与展示侧（`fallback_device_name` 识别占位）共用同一份定义。
pub(crate) const ARCHIVED_DEVICE_KEY_PREFIX: &str = "device-id:";

/// 设备无登记名时的回退显示名：优先截取 VID/PID 段，其次处理归档占位，最后截断原路径。
///
/// 写入侧也用（`writer::device_id_in_db`）：恢复文件回放后的首次落库拿不到设备名，
/// 兜底登记用同一套命名，界面才不会因为「回放先于重新登记」而闪出原始路径。
pub(crate) fn fallback_device_name(device_key: &str) -> String {
    if let Some((vid, pid)) = parse_vid_pid(device_key) {
        format!("HID 设备 · {vid}/{pid}")
    } else if let Some(id) = device_key.strip_prefix(ARCHIVED_DEVICE_KEY_PREFIX) {
        // 跨年归档给「有统计行、没登记行」的历史设备补的占位键（`sync_device_dict`）：
        // 那批设备的真实路径当时没登记，已经不可考，只能说明它的来历，
        // 不能把 `device-id:3` 这种纯机器串端给用户。
        format!("未知设备 · 归档 #{id}")
    } else {
        let n = device_key.chars().count();
        if n > 40 {
            let prefix: String = device_key.chars().take(40).collect();
            format!("{prefix}…")
        } else {
            device_key.to_string()
        }
    }
}

/// 从设备实例路径解析 VID/PID，返回 (VID, PID) 十六进制串（大写）。
///
/// 支持两种真机形态（2026-09-20 实测）：
/// - USB/2.4G 接收器：`HID#VID_24AE&PID_1464&MI_00#...` → ("24AE", "1464")
/// - 蓝牙 HID：`HID#{GUID}_Dev_VID&0107d7_PID&efff_REV&0120_...` → ("07D7", "EFFF")
///   （`VID&` 后 8 位里高 2 位是 vendor id source，真 VID 是后 4 位）
///
/// 格式不符返回 None（如 `HID#MSFT0001&Col01#...`、`ACPI#MSFT0001#...`，无 VID 段）。
pub(crate) fn parse_vid_pid(path: &str) -> Option<(String, String)> {
    let upper = path.to_ascii_uppercase();
    if let Some(pair) = parse_usb_style(&upper) {
        return Some(pair);
    }
    parse_bluetooth_style(&upper)
}

/// USB 形态：`VID_XXXX&PID_YYYY`。
///
/// 切片一律用 `.get(..)?` 而不是 `&s[..4]`：`len()` 数的是**字节**，而设备实例路径
/// 不保证是 ASCII —— 驱动会把产品名写进路径（`HID#VID_罗技&PID_1464` 这类真实形状），
/// 按字节切落在多字节字符中间就直接 panic。函数名里带 `parse` 却能让进程消失，是因为
/// 它在读侧：`fallback_device_name` 每次设备查询都调它，`device_alias::model_key`
/// 对 `device_aliases.json`（文档写明"可直接手改"）的每个键也调它，而 release 是
/// `panic = "abort"` 且没有控制台。蓝牙分支用的本来就是安全写法，这里补齐。
fn parse_usb_style(upper: &str) -> Option<(String, String)> {
    let rest = upper.split("VID_").nth(1)?;
    let vid = rest.get(..4)?;
    if !vid.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let pid_rest = rest.get(4..)?.strip_prefix("&PID_")?;
    let pid = pid_rest.get(..4)?;
    if !pid.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some((vid.to_string(), pid.to_string()))
}

/// 蓝牙形态：`VID&<vendorSource><VID>` + PID 段。
///
/// Windows 里分隔符不统一——HID 子设备是 `..._Dev_VID&0107d7_PID&efff_...`，
/// 枚举器路径是 `BTHENUM\VID&046d_PID_c52b`（`&` 与 `_` 混用），两种都要接受。
fn parse_bluetooth_style(upper: &str) -> Option<(String, String)> {
    let rest = upper.split("VID&").nth(1)?;
    let vid_end = rest.find(['&', '_']).unwrap_or(rest.len());
    let vid_field = &rest[..vid_end];
    let after = &rest[vid_end..];
    let pid_rest = ["&PID&", "_PID&", "&PID_", "_PID_"]
        .iter()
        .find_map(|p| after.strip_prefix(p))?;
    let pid_field = pid_rest.split(['&', '_']).next()?;
    // VID 字段长度可变：4 位（无 source 前缀）或 6 位（前 2 位为 vendor id source）
    // 同样用 `get(..)`：`len()` 是字节数，路径里混进非 ASCII 时 `n - 4` 不是字符边界。
    let vid = match vid_field.len() {
        4 => vid_field,
        n if n >= 6 => vid_field.get(n - 4..)?,
        _ => return None,
    };
    let pid = pid_field.get(..4)?;
    if !vid.chars().all(|c| c.is_ascii_hexdigit()) || !pid.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some((vid.to_string(), pid.to_string()))
}

/// 单库设备查询的原始行：(device_key, 登记名, 类型, 次数)。
struct DeviceRow {
    device_key: String,
    name: Option<String>,
    kind: Option<String>,
    count: i64,
}

/// 设备查询公共实现：device_counts 聚合 + LEFT JOIN devices 取登记名。
/// `cond` 为 date_key 条件片段，空串表示全表；表不存在（旧库）时返回 None。
fn query_devices_in_conn(
    conn: &Connection,
    cond: &str,
    param: Option<i64>,
) -> Option<Vec<DeviceRow>> {
    if !table_exists(conn, "device_counts") {
        return None;
    }
    let where_clause = if cond.is_empty() {
        String::new()
    } else {
        format!(" WHERE dc.{cond}")
    };
    // 字典化：统计表只存 device_id，设备实例路径从 devices 取回。
    // 对外（UI 改名、别名表）仍以 device_key 字符串为准；GROUP BY 用整数 id
    // 顺带避免了按长文本分组的临时 B 树。
    // COALESCE 兜底：登记行缺失（历史库）时也不能丢计数。
    let sql = format!(
        "SELECT COALESCE(d.device_key, 'device-id:' || dc.device_id) AS dkey,
                d.name, d.kind, SUM(dc.count) AS cnt
         FROM device_counts dc LEFT JOIN devices d ON d.id = dc.device_id
         {where_clause} GROUP BY dc.device_id"
    );
    let mapper = |r: &rusqlite::Row<'_>| {
        Ok(DeviceRow {
            device_key: r.get(0)?,
            name: r.get::<_, Option<String>>(1)?,
            kind: r.get::<_, Option<String>>(2)?,
            count: r.get(3)?,
        })
    };
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
        Ok(list) => Some(list),
        Err(e) => {
            tracing::error!("device_counts 查询失败 ({sql}): {e}");
            None
        }
    }
}

/// 原始行合并成展示行：别名优先、登记名缺失回退、显示名去重（同型号两只加序号）、按次数降序。
fn merge_device_rows(rows: Vec<DeviceRow>) -> (i64, Vec<DeviceStat>) {
    let aliases = crate::device_alias::table();
    // 同一台设备在两个年度库里各查出一行时必须先并起来：`devices.id` 各库自增、
    // 互不相干，只有 `device_key` 认得出是同一台。跨年查询（1 月看「最近 30 天」、
    // 或任何 period=0/跨年窗口）走的是逐年 append，不并的话设备页每个设备两行、
    // 各占一半次数，第二行还要被下面的同名去重冠上 "(2)"。
    let mut first_at: HashMap<String, usize> = HashMap::new();
    let mut merged: Vec<DeviceRow> = Vec::with_capacity(rows.len());
    for row in rows {
        match first_at.get(&row.device_key).copied() {
            Some(i) => {
                let keep = &mut merged[i];
                keep.count += row.count;
                // 名字/类型取"哪一年登记过就用哪一年的"：归档与迁移会留空名占位行
                if keep.name.as_deref().unwrap_or("").trim().is_empty() {
                    keep.name = row.name;
                }
                if keep.kind.as_deref().unwrap_or("").is_empty() {
                    keep.kind = row.kind;
                }
            }
            None => {
                first_at.insert(row.device_key.clone(), merged.len());
                merged.push(row);
            }
        }
    }
    let total: i64 = merged.iter().map(|r| r.count).sum();
    let mut stats: Vec<DeviceStat> = merged
        .into_iter()
        .map(|r| {
            // 两种「没有可用名字」都要回退：
            //   1. 登记名缺失/空白          —— 登记表压根没这一行；
            //   2. 登记名 == device_key     —— 写入侧拿不到设备信息时的占位登记
            //      （恢复文件回放会把 device_meta 置空、`migrate_device_tables` 与跨年归档
            //      也都用 device_key 占位），存的是 `\\?\HID#VID_...` 或 `device-id:3` 这种
            //      机器串。它非空却不能给人看，必须一并视为「没名字」。
            // 放在查询侧而不是逐个堵写入点：写入/迁移/归档三条路径会随版本变化，
            // 只有这里的兜底对所有历史库和新数据都成立。
            let auto_name = r
                .name
                .as_ref()
                .map(|n| n.trim())
                .filter(|n| !n.is_empty() && *n != r.device_key.as_str())
                .map(|n| n.to_string())
                .unwrap_or_else(|| fallback_device_name(&r.device_key));
            // 别名优先（用户改过的名字），别名缺失或为空时用自动名。
            // 去重只对自动名生效：两个不同设备可以起同一个别名，用户说了算。
            let alias = aliases.resolve(&r.device_key).map(|s| s.to_string());
            let has_alias = alias.is_some();
            let aliased = alias.unwrap_or_else(|| auto_name.clone());
            DeviceStat {
                key: r.device_key,
                name: aliased,
                auto_name,
                has_alias,
                kind: r.kind.unwrap_or_else(|| "unknown".to_string()),
                count: r.count,
            }
        })
        .collect();
    stats.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
    // 显示名去重：两只同型号设备（VID/PID 相同、实例不同）加序号区分。
    //
    // **只对自动名计数**，别名不改写、但要占位 —— 这才是上面那句注释
    // （"去重只对自动名生效：两个不同设备可以起同一个别名，用户说了算"）的本意。
    // 原先键的是 `s.name`，而有别名时 `s.name` 就是别名，于是两条都反着来：
    // ① 两只设备起同一个别名，第二只被强行改成「别名 (2)」—— 恰恰违背"用户说了算"；
    // ② 别人的别名反过来改掉第三台的展示名：用户给 A 起名「罗技M590」，而 B 的**自动名**
    //    本来也叫「罗技M590」，按次数排序谁在前谁占走干净名字 —— 排在后面的那个
    //    往往是 A，也就是**用户亲手起的名字被加了序号**，而它本来是唯一确定的。
    // 别名先占位、自动名从占位之后开始编号：用户的名字永不改写，自动名之间、
    // 自动名与别名之间都不再撞车。
    let mut seen: HashMap<String, usize> = HashMap::new();
    for st in &stats {
        if st.has_alias {
            *seen.entry(st.name.clone()).or_insert(0) += 1;
        }
    }
    for st in &mut stats {
        if st.has_alias {
            continue;
        }
        let n = seen.entry(st.auto_name.clone()).or_insert(0);
        *n += 1;
        if *n > 1 {
            st.name = format!("{} ({})", st.name, *n);
        }
    }
    (total, stats)
}

/// 查询设备维度统计：返回 (总输入次数, 设备行列表)，周期选择与按键统计一致。
pub fn get_device_stats(days: Option<i64>, year: Option<i32>) -> (i64, Vec<DeviceStat>) {
    let rows = if let Some(y) = year {
        devices_single_year(y, days)
    } else {
        let years = query_years(days, None);
        if years.len() == 1 {
            devices_single_year(years[0], days)
        } else {
            let start_dk = cutoff_day_key(days);
            let mut merged: Vec<DeviceRow> = Vec::new();
            for year in years {
                let path = paths::year_db_path(year);
                let cond = if start_dk.is_some() {
                    "date_key >= ?1"
                } else {
                    ""
                };
                let result = connection::with_ro_conn(&path, |conn| {
                    query_devices_in_conn(conn, cond, start_dk)
                });
                if let Some(mut list) = result.flatten() {
                    merged.append(&mut list);
                }
            }
            merged
        }
    };
    merge_device_rows(rows)
}

fn devices_single_year(year: i32, days: Option<i64>) -> Vec<DeviceRow> {
    let path = paths::year_db_path(year);
    let start_dk = cutoff_day_key(days);
    let cond = if start_dk.is_some() {
        "date_key >= ?1"
    } else {
        ""
    };
    let result =
        connection::with_ro_conn(&path, |conn| query_devices_in_conn(conn, cond, start_dk));
    result.flatten().unwrap_or_default()
}

/// 查询指定日期设备维度统计。
pub fn get_device_stats_by_date(target_date: chrono::NaiveDate) -> (i64, Vec<DeviceStat>) {
    let dk = day_key_of_date(target_date);
    let path = paths::year_db_path(target_date.year());
    let result = connection::with_ro_conn(&path, |conn| {
        query_devices_in_conn(conn, "date_key = ?1", Some(dk))
    });
    merge_device_rows(result.flatten().unwrap_or_default())
}

/// 单台设备的详情（点击「设备排行」某一行时按需查询）。
///
/// 口径说明：设备维度只记录「次数」，**不含键名与时段明细**（采集侧未记录），
/// 所以这里给的是各周期次数、排名、占比、活跃天数与近 30 天分布。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeviceDetail {
    /// 设备实例路径（Raw Input device_key）
    pub key: String,
    /// 展示名（别名优先）
    pub name: String,
    /// 自动解析名
    pub auto_name: String,
    pub has_alias: bool,
    /// mouse / keyboard / hybrid / unknown
    pub kind: String,
    /// 今日 / 近 7 天 / 近 30 天 / 全部 次数
    pub today: i64,
    pub week: i64,
    pub month: i64,
    pub all: i64,
    /// 所选周期：-1 今日 / 0 全部 / n 近 n 天
    pub period: i64,
    pub period_count: i64,
    /// 周期内全部设备次数（占比分母）
    pub period_total: i64,
    /// 周期内排名（1 起；周期内无数据为 0）
    pub rank: i64,
    /// 周期内设备总数
    pub device_count: i64,
    /// 同类型设备内的排名与数量
    pub kind_rank: i64,
    pub kind_count: i64,
    /// 有输入的天数
    pub active_days: i64,
    /// 活跃日均次数（活跃天数为 0 时为 0）
    pub avg_per_active_day: f64,
    /// 首次 / 最近使用日期（YYYY-MM-DD）
    pub first_date: String,
    pub last_date: String,
    /// 近 30 天每日次数（含无输入的 0，日期升序）
    pub trend: Vec<(String, i64)>,
    /// 周期内键名排行（次数降序）：`(键名, 次数)`。
    /// 空表示该库还没有键名明细数据（功能上线前的历史，或采集侧未记录）。
    pub keys: Vec<(String, i64)>,
    /// 周期内键名明细的总次数（占比分母；与 period_count 可能略有出入：
    /// 旧版本只记了次数没记键名的部分不会有明细）
    pub key_total: i64,
    /// 该设备**任何时候**有没有键名明细（区分「从没记过」与「本周期没有」）。
    /// 周期内有没有数据看 `keys` 是否为空，不要拿这个字段代替。
    pub has_key_detail: bool,
}

/// 该设备在任何年份库里有没有一条键名明细。
///
/// 与「本周期内有没有」是两件事，混起来会对着设备说谎：点「今日」看一台
/// 上周还在用、今天没按过的鼠标，界面说的是"键名明细从该功能上线后开始积累，
/// 此前的历史数据无法回溯"，而那台鼠标库里其实有几十条明细。
fn device_has_key_detail(device_key: &str) -> bool {
    for year in query_years(None, None) {
        let path = paths::year_db_path(year);
        let found = connection::with_ro_conn(&path, |conn| {
            if !table_exists(conn, "device_key_counts") {
                return None;
            }
            let mut stmt = conn
                .prepare(
                    "SELECT 1 FROM device_key_counts k \
                     JOIN devices d ON d.id = k.device_id \
                     WHERE d.device_key = ?1 LIMIT 1",
                )
                .ok()?;
            Some(stmt.exists([device_key]).unwrap_or(false))
        })
        .flatten()
        .unwrap_or(false);
        if found {
            return true;
        }
    }
    false
}

/// 单设备在周期内的键名排行（跨年度库合并，次数降序）。
///
/// 表不存在（旧库）时返回空 —— 键名明细从该功能上线后开始积累，历史无法回溯。
fn device_key_rows(device_key: &str, period: i64) -> Vec<(String, i64)> {
    let today_key = day_key_of_date(Local::now().date_naive());
    let start_key = match period {
        -1 => Some(today_key),
        0 => None,
        n => Some(today_key - (n.max(1) - 1)),
    };
    let mut merged: HashMap<String, i64> = HashMap::new();
    for year in query_years(None, None) {
        let path = paths::year_db_path(year);
        let rows = connection::with_ro_conn(&path, |conn| {
            if !table_exists(conn, "device_key_counts") {
                return None;
            }
            let (sql, param) = match start_key {
                Some(sk) => (
                    "SELECT k.key_name, SUM(k.count) FROM device_key_counts k \
                       JOIN devices d ON d.id = k.device_id \
                     WHERE d.device_key = ?1 AND k.date_key >= ?2 GROUP BY k.key_name",
                    Some(sk),
                ),
                None => (
                    "SELECT k.key_name, SUM(k.count) FROM device_key_counts k \
                       JOIN devices d ON d.id = k.device_id \
                     WHERE d.device_key = ?1 GROUP BY k.key_name",
                    None,
                ),
            };
            let mut stmt = conn.prepare(sql).ok()?;
            let list: Vec<(String, i64)> = match param {
                Some(p) => stmt
                    .query_map(rusqlite::params![device_key, p], |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })
                    .ok()?
                    .flatten()
                    .collect(),
                None => stmt
                    .query_map(rusqlite::params![device_key], |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })
                    .ok()?
                    .flatten()
                    .collect(),
            };
            Some(list)
        });
        for (name, c) in rows.flatten().unwrap_or_default() {
            *merged.entry(name).or_insert(0) += c;
        }
    }
    let mut list: Vec<(String, i64)> = merged.into_iter().collect();
    list.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    list
}

/// 单设备按天次数序列（跨年度库合并，date_key 升序）。
///
/// 设备维度按年份分库存储，查询必须跨库合并 —— 与 `get_device_stats` 同理。
fn device_date_series(device_key: &str) -> Vec<(i64, i64)> {
    let mut merged: HashMap<i64, i64> = HashMap::new();
    for year in query_years(None, None) {
        let path = paths::year_db_path(year);
        let rows = connection::with_ro_conn(&path, |conn| {
            if !table_exists(conn, "device_counts") {
                return None;
            }
            let mut stmt = conn
                .prepare(
                    "SELECT c.date_key, c.count FROM device_counts c \
                       JOIN devices d ON d.id = c.device_id \
                     WHERE d.device_key = ?1",
                )
                .ok()?;
            let list: Vec<(i64, i64)> = stmt
                .query_map([device_key], |r| Ok((r.get(0)?, r.get(1)?)))
                .ok()?
                .flatten()
                .collect();
            Some(list)
        });
        for (dk, c) in rows.flatten().unwrap_or_default() {
            *merged.entry(dk).or_insert(0) += c;
        }
    }
    let mut series: Vec<(i64, i64)> = merged.into_iter().collect();
    series.sort_by_key(|(dk, _)| *dk);
    series
}

/// 查询单台设备详情。`period` 与统计视图一致：-1 今日 / 0 全部 / n 近 n 天。
pub fn get_device_detail(device_key: &str, period: i64) -> DeviceDetail {
    let today_date = Local::now().date_naive();
    let today_key = day_key_of_date(today_date);
    let series = device_date_series(device_key);

    let in_period = |dk: i64| match period {
        -1 => dk == today_key,
        0 => true,
        n => dk >= today_key - (n.max(1) - 1),
    };
    let sum_where = |f: &dyn Fn(i64) -> bool| -> i64 {
        series
            .iter()
            .filter(|(dk, _)| f(*dk))
            .map(|(_, c)| *c)
            .sum()
    };
    let period_count = sum_where(&in_period);
    let today = sum_where(&|dk| dk == today_key);
    let week = sum_where(&|dk| dk >= today_key - 6);
    let month = sum_where(&|dk| dk >= today_key - 29);
    let all: i64 = series.iter().map(|(_, c)| *c).sum();
    let active_days = series.len() as i64;

    // 周期内的排名/占比/类型：复用「设备排行」同一套口径（含别名与同名去重）
    let (period_total, stats) = if period == -1 {
        get_device_stats_by_date(today_date)
    } else {
        get_device_stats((period > 0).then_some(period), None)
    };
    let idx = stats.iter().position(|s| s.key == device_key);

    // 名称与类型：优先取排行里的行（已套别名、已去重），否则按登记表/VID-PID 回退
    let (name, auto_name, has_alias, kind) = match idx {
        Some(i) => (
            stats[i].name.clone(),
            stats[i].auto_name.clone(),
            stats[i].has_alias,
            stats[i].kind.clone(),
        ),
        None => {
            let auto = fallback_device_name(device_key);
            let alias = crate::device_alias::table()
                .resolve(device_key)
                .map(|s| s.to_string());
            (
                alias.clone().unwrap_or_else(|| auto.clone()),
                auto,
                alias.is_some(),
                registered_kind(device_key).unwrap_or_else(|| "unknown".to_string()),
            )
        }
    };
    let kind_count = stats.iter().filter(|s| s.kind == kind).count() as i64;
    let kind_rank = stats
        .iter()
        .filter(|s| s.kind == kind)
        .position(|s| s.key == device_key)
        .map(|i| i as i64 + 1)
        .unwrap_or(0);

    // 周期内键名排行（设备 × 键名明细；旧库无该表时为空）
    let key_rows = device_key_rows(device_key, period);
    let key_total: i64 = key_rows.iter().map(|(_, c)| *c).sum();

    // 近 30 天分布（缺数据补 0，便于前端直接画柱）
    let date_str = |dk: i64| {
        day_key_to_date(dk)
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_default()
    };
    let trend: Vec<(String, i64)> = (0..30)
        .map(|back| today_key - back)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|dk| {
            let c = series
                .iter()
                .find(|(k, _)| *k == dk)
                .map(|(_, c)| *c)
                .unwrap_or(0);
            (date_str(dk), c)
        })
        .collect();

    DeviceDetail {
        key: device_key.to_string(),
        name,
        auto_name,
        has_alias,
        kind,
        today,
        week,
        month,
        all,
        period,
        period_count,
        period_total,
        rank: idx.map(|i| i as i64 + 1).unwrap_or(0),
        device_count: stats.len() as i64,
        kind_rank,
        kind_count,
        active_days,
        avg_per_active_day: if active_days > 0 {
            all as f64 / active_days as f64
        } else {
            0.0
        },
        first_date: series
            .first()
            .map(|(dk, _)| date_str(*dk))
            .unwrap_or_default(),
        last_date: series
            .last()
            .map(|(dk, _)| date_str(*dk))
            .unwrap_or_default(),
        trend,
        keys: key_rows,
        key_total,
        has_key_detail: device_has_key_detail(device_key),
    }
}

/// 读取设备登记表里的类型（用于周期内无数据、无法从排行取到类型的情况）。
fn registered_kind(device_key: &str) -> Option<String> {
    for year in query_years(None, None) {
        let path = paths::year_db_path(year);
        let kind = connection::with_ro_conn(&path, |conn| {
            if !table_exists(conn, "devices") {
                return None;
            }
            conn.query_row(
                "SELECT kind FROM devices WHERE device_key = ?1",
                [device_key],
                |r| r.get::<_, String>(0),
            )
            .ok()
        });
        if let Some(Some(k)) = kind {
            return Some(k);
        }
    }
    None
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

    /// 日期 ↔ 天数序号必须原样往返。
    ///
    /// 说明清楚它的适用边界：在 UTC+8 这台机器上，改前改后都过 —— 老实现的错要
    /// 在**负偏移时区**（`day_key_to_date` 把偏移加了第二次 → 每行日数据标到前一天）
    /// 或**有夏令时的时区**（`day_key_of_date` 加的是"此刻"的偏移 → 冬天查夏天少一天）
    /// 才露出来。留着它是为了让任何一次时区相关的改动都不能悄悄把这条不变量弄坏，
    /// 也是 CI（跑在别的时区）上的真守卫。
    #[test]
    fn day_key_roundtrips_over_eight_hundred_days() {
        let base = chrono::NaiveDate::from_ymd_opt(2024, 1, 1).expect("date");
        for i in 0..800u64 {
            let d = base + chrono::Days::new(i);
            let k = day_key_of_date(d);
            assert_eq!(day_key_to_date(k), Some(d), "{d} -> {k} -> 反算不一致");
        }
        // 序号就是"距 1970-01-01 的天数"，与本机时区无关
        assert_eq!(
            day_key_of_date(chrono::NaiveDate::from_ymd_opt(1970, 1, 1).expect("date")),
            0
        );
        assert_eq!(
            day_key_of_date(chrono::NaiveDate::from_ymd_opt(1970, 1, 2).expect("date")),
            1
        );
        assert_eq!(
            day_key_of_date(base + chrono::Days::new(365)) - day_key_of_date(base),
            365
        );
    }

    /// 查询用的日期序号，必须等于写入侧给"该日中午那个瞬时"算出的序号。
    ///
    /// 取中午是刻意避开夏令时切换的那一小时（那里任何按瞬时分桶的写法都有歧义）。
    #[test]
    fn day_key_of_date_matches_the_instant_based_key_at_local_noon() {
        use chrono::{Local, TimeZone};
        for (y, m, d) in [
            (2024, 1, 5),
            (2024, 3, 31),
            (2024, 7, 1),
            (2024, 10, 27),
            (2025, 12, 31),
        ] {
            let date = chrono::NaiveDate::from_ymd_opt(y, m, d).expect("date");
            let naive = date.and_hms_opt(12, 0, 0).expect("noon");
            let ts = Local
                .from_local_datetime(&naive)
                .latest()
                .expect("可解析")
                .timestamp();
            assert_eq!(
                day_key_of_date(date),
                day_key_of_ts(ts),
                "{date} 的查询序号与写入侧分桶不一致"
            );
        }
    }

    /// 空壳年度库不该再算进 `available_years()`。
    ///
    /// 年份列表按文件名取，于是「按日期把一整年删空」（`cleanup_old_data`）或历史
    /// 遗留的空文件，会让此后每一轮刷新都为它开一次连接、把全部聚合查询各跑一遍，
    /// 备份与 VACUUM 也各挨一次 —— 收益是 0，因为它一行数据都没有。
    #[test]
    fn available_years_skips_empty_shell_dbs() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("years_shell");
        invalidate_years_cache();

        // 2031：只有表结构的空壳；2032：有一行聚合数据
        for y in [2031, 2032] {
            let conn = connection::open_rw(&paths::year_db_path(y)).unwrap();
            connection::ensure_schema(&conn, y).unwrap();
            if y == 2032 {
                conn.execute(
                    "INSERT INTO daily_counts (date_key, count, seconds) VALUES (1, 1, 1)",
                    [],
                )
                .unwrap();
            }
        }
        invalidate_years_cache();
        let years = available_years();
        assert!(years.contains(&2032), "有数据的年份必须在: {years:?}");
        assert!(!years.contains(&2031), "空壳年份不该再被扫: {years:?}");

        // 读不了的库方向相反，必须**保留**：宁可多扫一趟，也不能因为一次 I/O
        // 失败或文件损坏，把一整年静默地从统计里抹掉。
        std::fs::write(paths::year_db_path(2033), b"not a database at all").unwrap();
        invalidate_years_cache();
        assert!(
            available_years().contains(&2033),
            "损坏的年度库应按「有数据」保守处理"
        );
    }

    /// 另一半：**库头合法、页却是坏的**。这种文件 `open_ro` 是成功的，失败发生在查询时。
    ///
    /// 上面 2033 那条走的是"整个文件都是垃圾 → 打不开"的分支，盖不住这一半：
    /// 逐表探测的 Err 以前被 `unwrap_or(false)` 折成"这一张表没行"，六张全折完之后
    /// 该年就被 `available_years()` 剔除 —— 于是总计/排行/趋势/启动自愈/备份/VACUUM
    /// 都不再经过那一年，`--list-years` 也不列它，而且一条日志都没有。
    /// 截断与位翻转是真实损坏形状（掉电、同步盘半截文件、坏道），不是假想的。
    #[test]
    fn available_years_keeps_dbs_whose_pages_are_corrupt() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("years_corrupt");
        invalidate_years_cache();

        let path = paths::year_db_path(2034);
        {
            let conn = connection::open_rw(&path).unwrap();
            connection::ensure_schema(&conn, 2034).unwrap();
            // 数据必须铺到第 2 页之后，否则"只坏第 1 页之外"什么也测不到
            for k in 1..4001i64 {
                conn.execute(
                    "INSERT INTO daily_counts (date_key, count, seconds) VALUES (?1, 1, 1)",
                    [k],
                )
                .unwrap();
            }
            let _ = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
        }
        for suffix in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
        // 第 1 页（文件头 + `sqlite_master` 的根页）留着，后面的页全部打成垃圾
        let mut bytes = std::fs::read(&path).unwrap();
        assert!(
            bytes.len() > 4096 * 4,
            "夹具太小，坏不到东西：{} 字节",
            bytes.len()
        );
        for b in bytes.iter_mut().skip(4096) {
            *b = 0xA5;
        }
        std::fs::write(&path, &bytes).unwrap();

        // 先证明夹具真的落在"打得开、读不出"这一族，否则这条断言什么也没钉住
        let conn = connection::open_ro(&path).expect("库头合法，连接本身该打得开");
        assert!(
            conn.query_row("SELECT COUNT(*) FROM daily_counts", [], |r| r
                .get::<_, i64>(0))
                .is_err(),
            "夹具必须真的读不出行，否则测不到损坏分支"
        );

        invalidate_years_cache();
        assert!(
            available_years().contains(&2034),
            "读不出来的年份必须保留（宁可多扫一趟），实际: {:?}",
            available_years()
        );
    }

    /// 设备实例路径里混进非 ASCII 时不能把进程弄没。
    ///
    /// `parse_vid_pid` 原来按**字节**切（`len() < 4` 之后 `&rest[..4]`），而 Windows 的
    /// HID 路径里驱动是可以带产品名的（`HID#VID_罗技&PID_1464` 这类形状），一个汉字
    /// 占 3 字节 → 索引 4 不是字符边界 → slice panic。它在读侧：`fallback_device_name`
    /// 每次设备查询都走，`device_alias::model_key` 对可手改的 `device_aliases.json`
    /// 每个键也走，而 release 是 `panic = "abort"` 且无控制台 —— 症状是设备页一刷新
    /// 整个程序消失。修法是 `.get(..)?`（蓝牙分支本来就是这么写的）。
    #[test]
    fn non_ascii_device_keys_are_rejected_not_fatal() {
        assert_eq!(parse_vid_pid("HID#VID_罗技&PID_1464"), None);
        assert_eq!(parse_vid_pid("HID#VID_日本語のデバイス&PID_0001"), None);
        // 蓝牙那条路的变长字段同样按字节切过：`n - 4` 落在汉字中间
        assert_eq!(parse_vid_pid(r"BTHENUM#VID&012罗技_PID&c52b"), None);
        // 正常形状必须照旧解析出来，别把修 panic 做成修功能
        assert_eq!(
            parse_vid_pid(r"HID#VID_24AE&PID_1464&MI_00#7&1a2b&0&0000"),
            Some(("24AE".to_string(), "1464".to_string()))
        );
        assert_eq!(
            parse_vid_pid("BTHENUM\\VID&046d_PID_c52b"),
            Some(("046D".to_string(), "C52B".to_string()))
        );
        // 太短的路径走 None，不是 panic
        assert_eq!(parse_vid_pid("HID#VID_1"), None);
    }

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
        let _tmp = crate::paths::test_app_dir("period_guard");

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
        let _tmp = crate::paths::test_app_dir("daily_span");

        assert_eq!(get_daily_counts(1, None).len(), 1);
        assert_eq!(get_daily_counts(200, None).len(), 200);
        assert_eq!(get_daily_counts(0, None).len(), 1, "非法天数收敛为 1 天");
    }

    /// VID/PID 解析：USB 形态、蓝牙形态、大小写、残缺格式。
    #[test]
    fn parse_vid_pid_variants() {
        assert_eq!(
            parse_vid_pid(r"\\?\HID#VID_046D&PID_C52B&MI_00#8&2c5f&0&0000"),
            Some(("046D".to_string(), "C52B".to_string()))
        );
        assert_eq!(
            parse_vid_pid("hid#vid_1234&pid_5678#x"),
            Some(("1234".to_string(), "5678".to_string()))
        );
        // 蓝牙 HID（真机实测形态）：VID& 后 6 位（含 2 位 vendor id source）
        assert_eq!(
            parse_vid_pid(
                r"\\?\HID#{00001812-0000-1000-8000-00805f9b34fb}_Dev_VID&0107d7_PID&efff_REV&0120_d46d51083b12&Col03#9&20337b89&0&0002#{378de44c-56ef-11d1-bc8c-00a0c91405dd}"
            ),
            Some(("07D7".to_string(), "EFFF".to_string()))
        );
        // 蓝牙形态无 source 前缀 / 枚举器分隔符混用（& 与 _）
        assert_eq!(
            parse_vid_pid(r"BTHENUM#VID&046d_PID_c52b#x"),
            Some(("046D".to_string(), "C52B".to_string()))
        );
        assert_eq!(
            parse_vid_pid(r"BTHENUM\Dev_VID&0000046D&PID_C52B"),
            Some(("046D".to_string(), "C52B".to_string()))
        );
        assert_eq!(
            parse_vid_pid("HID#VID_GGGG&PID_C52B"),
            None,
            "非十六进制 VID"
        );
        assert_eq!(parse_vid_pid("HID#VID_046D"), None, "缺 PID");
        assert_eq!(
            parse_vid_pid("HID#MSFT0001&Col01#5&36f79095&0&0000"),
            None,
            "无 VID 段"
        );
        assert_eq!(
            parse_vid_pid("ACPI#MSFT0001#4&f25ce6e&0"),
            None,
            "ACPI 无 VID 段"
        );
    }

    /// 回退显示名：无登记名时用 VID/PID 段，取不到时截断长路径。
    #[test]
    fn fallback_device_name_uses_vid_pid() {
        assert_eq!(
            fallback_device_name("HID#VID_046D&PID_C52B&MI_00#8&2c5f&0&0000"),
            "HID 设备 · 046D/C52B"
        );
        let long = "X".repeat(60);
        assert!(fallback_device_name(&long).starts_with(&"X".repeat(40)));
        assert!(fallback_device_name(&long).ends_with('…'));
        assert_eq!(fallback_device_name("short#path"), "short#path");
    }

    /// 跨年归档的占位 device_key 必须显示成人话，不能把 `device-id:3` 端给用户。
    ///
    /// 这类行在 `merge_device_rows` 里会因为「name == device_key」走回退，
    /// 但回退本身若认不出这个前缀就会原样返回 —— 这条断言锁住后半段。
    #[test]
    fn fallback_device_name_handles_archived_placeholder() {
        assert_eq!(
            fallback_device_name("device-id:3"),
            "未知设备 · 归档 #3",
            "归档占位要说明来历，不能显示机器串"
        );
        assert_eq!(
            fallback_device_name("device-id:127"),
            "未知设备 · 归档 #127"
        );
        // 前缀必须一致：改了常量而忘记改生成侧会让这条失效
        assert_eq!(ARCHIVED_DEVICE_KEY_PREFIX, "device-id:");
        // 不能误伤正常短 key
        assert_eq!(fallback_device_name("short#path"), "short#path");
    }

    /// 设备查询：登记名 JOIN、无名回退、同名去重、按次数降序。
    #[test]
    fn device_stats_merge_join_and_dedupe() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("devq");

        let dk = day_key_of_date(Local::now().date_naive());
        {
            let path = paths::year_db_path(Local::now().year());
            let conn = crate::db::connection::open_rw(&path).unwrap();
            crate::db::connection::ensure_schema(&conn, Local::now().year()).unwrap();
            let ins_dev = |key: &str, name: &str, kind: &str| {
                conn.execute(
                    "INSERT INTO devices (device_key, name, kind) VALUES (?1, ?2, ?3)",
                    rusqlite::params![key, name, kind],
                )
                .unwrap();
            };
            let ins_cnt = |key: &str, n: i64| {
                // 字典化后统计表只存 devices.id：设备路径必须先在 devices 里注册。
                // 无登记行时补一行空名占位（与写入侧的兜底一致）。
                conn.execute(
                    "INSERT OR IGNORE INTO devices (device_key, name, kind) \
                     VALUES (?1, '', 'unknown')",
                    [key],
                )
                .unwrap();
                let id = crate::db::connection::device_id_of(&conn, key).expect("设备 id");
                conn.execute(
                    "INSERT INTO device_counts (date_key, device_id, count) VALUES (?1, ?2, ?3)",
                    rusqlite::params![dk, id, n],
                )
                .unwrap();
            };
            // devA/devD：同型号两只（显示名相同 → 去重加序号）
            ins_dev(
                "HID#VID_046D&PID_C52B#devA",
                "HID-compliant mouse · 046D/C52B",
                "mouse",
            );
            ins_dev(
                "HID#VID_046D&PID_C52B#devD",
                "HID-compliant mouse · 046D/C52B",
                "mouse",
            );
            // devB：登记名为空 → 走 VID/PID 回退
            ins_dev("HID#VID_046D&PID_C52B#devB", "", "mouse");
            // devC（键盘）：只有统计行、登记名为空 → 回退 + kind=unknown
            ins_cnt("HID#VID_046D&PID_C52B#devA", 30);
            ins_cnt("HID#VID_046D&PID_C52B#devD", 10);
            ins_cnt("HID#VID_046D&PID_C52B#devB", 70);
            ins_cnt("HID#VID_1B1C&PID_1B2D#kb", 50);
        }

        let (total, stats) = get_device_stats_by_date(Local::now().date_naive());
        assert_eq!(total, 160);
        assert_eq!(stats.len(), 4, "设备按 device_key 独立成行");
        // 降序：70 / 50 / 30 / 10
        assert_eq!(
            stats[0].name, "HID 设备 · 046D/C52B",
            "空登记名回退 VID/PID"
        );
        assert_eq!(stats[0].name, stats[0].auto_name, "无别名时展示名 = 自动名");
        assert_eq!(
            stats[0].key, "HID#VID_046D&PID_C52B#devB",
            "行内必须带 device_key（UI 改名回写要用）"
        );
        assert_eq!(stats[0].count, 70);
        assert_eq!(stats[1].name, "HID 设备 · 1B1C/1B2D", "未登记设备同样回退");
        assert_eq!(stats[1].kind, "unknown", "未登记设备的类型为 unknown");
        assert_eq!(stats[1].count, 50);
        assert_eq!(
            stats[2].name, "HID-compliant mouse · 046D/C52B",
            "先出现者保留原名"
        );
        assert_eq!(stats[2].count, 30);
        assert_eq!(stats[2].kind, "mouse");
        assert_eq!(
            stats[3].name, "HID-compliant mouse · 046D/C52B (2)",
            "同型号显示名去重"
        );
        assert_eq!(stats[3].count, 10);
    }

    /// 跨年查询里同一台设备必须是一行（1 月看「最近 30 天」就会走到这条路）。
    ///
    /// `devices.id` 各年度库自增、互不相干，两个库里的同一台设备只有 device_key
    /// 认得出来。逐年 append 而不按 key 并起来时，设备页每个设备显示两行、
    /// 各占一半次数，第二行还被同名去重加上 "(2)"，看着就像多了一只鼠标。
    #[test]
    fn device_stats_collapse_the_same_device_across_year_dbs() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("devq_year");

        let this_year = Local::now().year();
        let prev_year = this_year - 1;
        let key = "HID#VID_046D&PID_C52B#same";
        // 两个库各登记一台设备，让同一台设备在两侧的自增 id 真的错开
        for (year, n, other) in [
            (prev_year, 40, "HID#VID_1B1C&PID_1B2D#old"),
            (this_year, 60, ""),
        ] {
            let conn = crate::db::connection::open_rw(&paths::year_db_path(year)).unwrap();
            crate::db::connection::ensure_schema(&conn, year).unwrap();
            let dk = day_key_of_date(chrono::NaiveDate::from_ymd_opt(year, 6, 1).expect("date"));
            let ins_cnt = |k: &str, c: i64| {
                conn.execute(
                    "INSERT OR IGNORE INTO devices (device_key, name, kind) VALUES (?1, ?2, 'mouse')",
                    rusqlite::params![k, format!("HID-compliant mouse · {k}")],
                )
                .unwrap();
                let id = crate::db::connection::device_id_of(&conn, k).expect("设备 id");
                conn.execute(
                    "INSERT INTO device_counts (date_key, device_id, count) VALUES (?1, ?2, ?3)",
                    rusqlite::params![dk, id, c],
                )
                .unwrap();
            };
            if !other.is_empty() {
                ins_cnt(other, 5);
            }
            ins_cnt(key, n);
            // available_years() 会滤掉没有任何聚合行的空壳库，补一行当"这年真在用"
            conn.execute(
                "INSERT INTO daily_counts (date_key, count, seconds) VALUES (?1, ?2, 10)",
                rusqlite::params![dk, n],
            )
            .unwrap();
        }
        invalidate_years_cache();

        let (total, stats) = get_device_stats(None, None);
        let shown: Vec<_> = stats.iter().map(|s| (&s.key, &s.name, s.count)).collect();
        assert_eq!(total, 105, "跨年总次数 = 两个库相加");
        assert_eq!(
            stats.len(),
            2,
            "一只跨年共用设备 + 一只旧库设备 = 两行，实际：{shown:?}"
        );
        let same = stats
            .iter()
            .find(|s| s.key == key)
            .expect("同一台设备必须只有一行");
        assert_eq!(same.count, 100, "两个年度库的次数必须并起来");
        assert!(
            !same.name.ends_with("(2)"),
            "跨年重复行不该被同名去重顶成两只：{}",
            same.name
        );
        // 按单年查询不受影响
        assert_eq!(get_device_stats(None, Some(this_year)).0, 60);
        assert_eq!(get_device_stats(None, Some(prev_year)).0, 45);
    }

    /// `has_key_detail` 问的是"这台设备记过键名明细吗"，不是"本周期里有吗"。
    ///
    /// 两者混起来时，点「今日」看一台只在上周按过的鼠标，界面会说
    /// "键名明细从该功能上线后开始积累，此前的历史数据无法回溯" —— 而库里
    /// 其实有它的明细（他机器上就有这种设备：明细全落在 09-22 那一天）。
    #[test]
    fn device_key_detail_flag_is_not_period_scoped() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("devflag");

        let today = Local::now().date_naive();
        let dk = day_key_of_date(today);
        let old = "HID#VID_046D&PID_C52B&MI_00#old";
        let never = "HID#VID_046D&PID_C52B&MI_00#never";
        {
            let path = paths::year_db_path(today.year());
            let conn = crate::db::connection::open_rw(&path).unwrap();
            crate::db::connection::ensure_schema(&conn, today.year()).unwrap();
            for (key, kind) in [(old, "mouse"), (never, "mouse")] {
                conn.execute(
                    "INSERT INTO devices (device_key, name, kind) VALUES (?1, ?2, ?3)",
                    rusqlite::params![key, format!("HID 鼠标 · {kind}"), kind],
                )
                .unwrap();
            }
            let dev_id = |key: &str| -> i64 {
                crate::db::connection::device_id_of(&conn, key).expect("设备 id")
            };
            // 两台设备今天都有按键次数（否则压根进不了详情入口）
            for key in [old, never] {
                conn.execute(
                    "INSERT INTO device_counts (date_key, device_id, count) VALUES (?1, ?2, ?3)",
                    rusqlite::params![dk, dev_id(key), 7],
                )
                .unwrap();
            }
            // 但只有 `old` 记过键名明细，而且记在 5 天前
            conn.execute(
                "INSERT INTO device_key_counts (date_key, device_id, key_name, count) \
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![dk - 5, dev_id(old), "空格", 30],
            )
            .unwrap();
        }
        crate::device_alias::invalidate_cache();

        let today_view = get_device_detail(old, -1);
        assert!(
            today_view.keys.is_empty(),
            "今日确实没有明细（这条前提要成立，下面的断言才有意义）"
        );
        assert!(
            today_view.has_key_detail,
            "「本周期没有明细」不等于「这台设备没记过明细」"
        );
        let all_view = get_device_detail(old, 0);
        assert!(all_view.has_key_detail);
        assert_eq!(
            all_view
                .keys
                .iter()
                .find(|(k, _)| k == "空格")
                .map(|(_, c)| *c),
            Some(30),
            "换成长周期就该看到那条明细"
        );
        assert!(
            !get_device_detail(never, -1).has_key_detail,
            "从没记过明细的设备仍然要报 false，否则新文案就没意义了"
        );
    }

    /// 登记名被写成裸设备路径时（历史脏登记）不计为真名，一律走回退。
    ///
    /// 有三个来源都把 device_key 当 name 写进 devices 表：
    /// 恢复文件回放后缺 meta 的补登（2026-09-21 前的写法）、
    /// `migrate_device_tables` 对旧库的迁移补登、跨年归档的占位登记。
    /// 逐个堵写入点会随版本漏掉，所以兜底收在查询侧一处。
    #[test]
    fn device_stats_ignores_device_key_as_name() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("devdirty");

        let dk = day_key_of_date(Local::now().date_naive());
        let dirty = r"\\?\HID#VID_046D&PID_C52B&MI_00#8&2c5f&0&0000#{378de44c}";
        let clean = "HID#VID_1B1C&PID_1B2D#clean";
        {
            let path = paths::year_db_path(Local::now().year());
            let conn = crate::db::connection::open_rw(&path).unwrap();
            crate::db::connection::ensure_schema(&conn, Local::now().year()).unwrap();
            let ins = |key: &str, name: &str, kind: &str, n: i64| {
                conn.execute(
                    "INSERT INTO devices (device_key, name, kind) VALUES (?1, ?2, ?3)",
                    rusqlite::params![key, name, kind],
                )
                .unwrap();
                let id = crate::db::connection::device_id_of(&conn, key).expect("设备 id");
                conn.execute(
                    "INSERT INTO device_counts (date_key, device_id, count) VALUES (?1, ?2, ?3)",
                    rusqlite::params![dk, id, n],
                )
                .unwrap();
            };
            ins(dirty, dirty, "unknown", 40);
            ins(clean, "我的键盘 · 1B1C/1B2D", "keyboard", 10);
        }

        let (_, stats) = get_device_stats_by_date(Local::now().date_naive());
        let row = |key: &str| stats.iter().find(|s| s.key == key).expect("设备行");
        assert_eq!(
            row(dirty).name,
            "HID 设备 · 046D/C52B",
            "登记名等于 device_key 时必须当作「没名字」回退"
        );
        assert_ne!(row(dirty).name, dirty, "不能把设备实例路径显示出来");
        assert_eq!(
            row(clean).name,
            "我的键盘 · 1B1C/1B2D",
            "正常登记名不得被这条规则改掉"
        );
    }

    /// 设备详情：各周期次数、活跃天数、首末日期、排名占比与近 30 天分布。
    #[test]
    fn device_detail_reports_periods_and_trend() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("devdetail");

        let today = Local::now().date_naive();
        let dk = |d: &chrono::NaiveDate| day_key_of_date(*d);
        let a = "HID#VID_046D&PID_C52B&MI_00#a";
        let b = "HID#VID_046D&PID_C52B&MI_00#b";
        {
            let path = paths::year_db_path(today.year());
            let conn = crate::db::connection::open_rw(&path).unwrap();
            crate::db::connection::ensure_schema(&conn, today.year()).unwrap();
            conn.execute(
                "INSERT INTO devices (device_key, name, kind) VALUES (?1, ?2, ?3)",
                rusqlite::params![a, "HID 鼠标 · 046D/C52B", "mouse"],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO devices (device_key, name, kind) VALUES (?1, ?2, ?3)",
                rusqlite::params![b, "HID 键盘 · 046D/C52B", "keyboard"],
            )
            .unwrap();
            let dev_id = |key: &str| -> i64 {
                crate::db::connection::device_id_of(&conn, key).expect("设备 id")
            };
            let ins = |key: &str, off: i64, n: i64| {
                conn.execute(
                    "INSERT INTO device_counts (date_key, device_id, count) VALUES (?1, ?2, ?3)",
                    rusqlite::params![dk(&today) - off, dev_id(key), n],
                )
                .unwrap();
            };
            ins(a, 0, 100);
            ins(a, 1, 50);
            ins(a, 2, 25);
            ins(b, 0, 10);
            // 键名明细：A 设备今天 左下键多于滚轮；B 设备也有明细（用于验证按设备隔离）
            for (key, n, off) in [
                ("鼠标左键", 60, 0),
                ("滚轮上滑", 40, 0),
                ("鼠标左键", 150, 1),
            ] {
                conn.execute(
                    "INSERT INTO device_key_counts (date_key, device_id, key_name, count) \
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![dk(&today) - off, dev_id(a), key, n],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO device_key_counts (date_key, device_id, key_name, count) \
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![dk(&today), dev_id(b), "空格", 10],
            )
            .unwrap();
        }
        crate::device_alias::invalidate_cache();

        // 今日视角
        let d = get_device_detail(a, -1);
        assert_eq!(d.today, 100);
        assert_eq!(d.period_count, 100, "今日周期只算今天");
        assert_eq!(d.period_total, 110, "占比分母 = 今日全部设备");
        assert_eq!(d.rank, 1, "今日该设备第一");
        assert_eq!(d.device_count, 2);
        assert_eq!(d.kind_rank, 1, "鼠标类型内第一");
        assert_eq!(d.kind_count, 1);
        assert_eq!(d.kind, "mouse");

        // 全部历史视角
        let d = get_device_detail(a, 0);
        assert_eq!(d.all, 175);
        assert_eq!(d.today, 100);
        assert_eq!(d.week, 175, "三天数据都在 7 天窗口内");
        assert_eq!(d.month, 175);
        assert_eq!(d.period_count, 175);
        assert_eq!(d.active_days, 3);
        assert!((d.avg_per_active_day - 175.0 / 3.0).abs() < 0.01);
        assert_eq!(
            d.first_date,
            (today - chrono::Duration::days(2)).to_string()
        );
        assert_eq!(d.last_date, today.to_string());
        assert_eq!(d.trend.len(), 30, "近 30 天分布固定 30 格");
        assert_eq!(d.trend.last().unwrap().1, 100, "最后一格是今天");
        assert_eq!(
            d.trend[0].0,
            (today - chrono::Duration::days(29)).to_string()
        );

        // 近 2 天窗口：只含今天与昨天
        let d = get_device_detail(a, 2);
        assert_eq!(d.period_count, 150);
        assert_eq!(d.period_total, 160);

        // 键名明细：按设备隔离、按次数降序，且跟随所选周期
        let today_detail = get_device_detail(a, -1);
        assert!(today_detail.has_key_detail);
        assert_eq!(today_detail.keys.len(), 2, "今日只有左键与滚轮");
        assert_eq!(today_detail.keys[0], ("鼠标左键".to_string(), 60));
        assert_eq!(today_detail.keys[1], ("滚轮上滑".to_string(), 40));
        assert_eq!(today_detail.key_total, 100);
        let all_detail = get_device_detail(a, 0);
        assert_eq!(all_detail.key_total, 250, "含昨天的 150 次左键");
        assert_eq!(all_detail.keys[0].1, 210, "左键累计 60 + 150");
        let other = get_device_detail(b, -1);
        assert_eq!(other.keys.len(), 1, "另一台设备的明细互不混入");
        assert_eq!(other.keys[0], ("空格".to_string(), 10));

        crate::device_alias::invalidate_cache();
    }

    /// 用户起的别名永不被加序号，即使是两只设备起了同一个名字。
    #[test]
    fn identical_aliases_are_both_kept_verbatim() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("devq_two_aliases");
        crate::device_alias::invalidate_cache();
        // 两只设备的 VID/PID 必须**不同**：同一型号会走 model_key 回退，
        // 那会让两条键都解析到同一个别名，测的就不是去重而是回退了。
        let k1 = "HID#VID_1111&PID_2222#1";
        let k2 = "HID#VID_3333&PID_4444#2";
        crate::device_alias::set(k1, "双鼠").unwrap();
        crate::device_alias::set(k2, "双鼠").unwrap();
        crate::device_alias::invalidate_cache();
        let row = |key: &str, count: i64| DeviceRow {
            device_key: key.to_string(),
            name: Some("HID 鼠标 · 046D/C52B".to_string()),
            kind: Some("mouse".to_string()),
            count,
        };
        let (_, stats) = merge_device_rows(vec![row(k1, 300), row(k2, 200)]);
        assert_eq!(stats[0].name, "双鼠");
        assert_eq!(
            stats[1].name, "双鼠",
            "两只都叫「双鼠」是用户自己的决定，去重只对自动名生效",
        );
        crate::device_alias::clear(k1).unwrap();
        crate::device_alias::clear(k2).unwrap();
        crate::device_alias::invalidate_cache();
    }

    /// 别名与另一台的**自动名**撞车时：该加序号的是那台没起过名的设备。
    ///
    /// 旧实现按键是展示名，谁次数高谁占走干净名字 —— 而次数高的往往是用户
    /// 起了别名那台排在前面时才相反：这里 B 次数最高、A（别名）次数最低，
    /// 旧实现会把**用户起的别名**改写成「罗技M590 (2)」。
    #[test]
    fn an_alias_reserves_its_name_against_other_auto_names() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("devq_alias_reserve");
        crate::device_alias::invalidate_cache();
        // 同上：三只键各用不同 VID/PID，只有 aliased 那只起了别名
        let auto_hi = "HID#VID_AAAA&PID_BBBB#a";
        let aliased = "HID#VID_046D&PID_C52B#b";
        let auto_lo = "HID#VID_CCCC&PID_DDDD#c";
        crate::device_alias::set(aliased, "罗技M590").unwrap();
        crate::device_alias::invalidate_cache();
        let row = |key: &str, name: &str, count: i64| DeviceRow {
            device_key: key.to_string(),
            name: Some(name.to_string()),
            kind: Some("mouse".to_string()),
            count,
        };
        let (_, stats) = merge_device_rows(vec![
            row(auto_hi, "罗技M590", 300),
            row(aliased, "别的登记名", 100),
            row(auto_lo, "罗技M590", 50),
        ]);
        assert_eq!(stats[0].name, "罗技M590 (2)", "没起过名的自动名让路");
        assert_eq!(stats[1].name, "罗技M590", "用户起的别名一字不改");
        assert_eq!(stats[2].name, "罗技M590 (3)");
        assert_eq!(
            stats[1].auto_name, "别的登记名",
            "auto_name 仍是登记名，供副标题用"
        );
        crate::device_alias::clear(aliased).unwrap();
        crate::device_alias::invalidate_cache();
    }

    /// 别名生效：改过名的设备展示别名，自动名仍保留在 auto_name 里做副标题。
    #[test]
    fn device_alias_overrides_display_name() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("devq_alias");
        crate::device_alias::invalidate_cache();

        let key = "HID#VID_046D&PID_C52B&MI_00#7&1f126e19&0&0000";
        let dk = day_key_of_date(Local::now().date_naive());
        {
            let path = paths::year_db_path(Local::now().year());
            let conn = crate::db::connection::open_rw(&path).unwrap();
            crate::db::connection::ensure_schema(&conn, Local::now().year()).unwrap();
            conn.execute(
                "INSERT INTO devices (device_key, name, kind) VALUES (?1, ?2, ?3)",
                rusqlite::params![key, "HID 鼠标 · 046D/C52B", "mouse"],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO device_counts VALUES (?1, ?2, 42)",
                rusqlite::params![
                    dk,
                    crate::db::connection::device_id_of(&conn, key).expect("设备 id")
                ],
            )
            .unwrap();
        }

        // 无别名：展示自动名
        let (_, stats) = get_device_stats_by_date(Local::now().date_naive());
        assert_eq!(stats[0].name, "HID 鼠标 · 046D/C52B");
        assert_eq!(stats[0].name, stats[0].auto_name);

        // 改名后：展示别名，自动名保留
        crate::device_alias::set(key, "新鼠标").unwrap();
        let (_, stats) = get_device_stats_by_date(Local::now().date_naive());
        assert_eq!(stats[0].name, "新鼠标", "展示名应被别名覆盖");
        assert_eq!(stats[0].auto_name, "HID 鼠标 · 046D/C52B");
        assert_eq!(stats[0].count, 42, "改名不影响计数");

        // 清空别名：回到自动名
        crate::device_alias::clear(key).unwrap();
        let (_, stats) = get_device_stats_by_date(Local::now().date_naive());
        assert_eq!(stats[0].name, "HID 鼠标 · 046D/C52B");

        crate::device_alias::invalidate_cache();
    }
}
