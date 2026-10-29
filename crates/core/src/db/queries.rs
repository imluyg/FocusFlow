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
/// 所有进入日期运算的天数都必须先经过 [`clamp_query_days`]：周期值是外部输入，今天从界面
/// 自己那几档与 Lua 插件（`focusflow.stats`）两处交进来（旧前端的 `set_period` 与
/// `gui.default_period` 现在都没有读点了），不设上界时 `NaiveDate - Days` 越界会 panic。
pub const MAX_QUERY_DAYS: i64 = 36_500;

/// 把外部传入的天数收敛到可安全参与日期运算的区间 `1..=MAX_QUERY_DAYS`。
pub fn clamp_query_days(days: i64) -> i64 {
    days.clamp(1, MAX_QUERY_DAYS)
}

/// 「这一轮没读全」用来点名的**域名清单，唯一出处**。
///
/// 一句 gap 长这样：`{哪一年} 年库{域名}读不出来: {原因}`。四个读数出口各自填自己的域名
/// （三个榜走 `gather_rank_totals`、设备走 `gather_device_totals`、时间窗口走
/// `read_window_years`、单日小时走 `hourly_stats_totals`）。消费侧今天这么用它：主窗口
/// 按域名分栏（`desktop-native/src/main.rs` 的 `gap_note`，一屏几栏各说各的），页脚再报一次
/// 总数；周报把攒到的 gap 逐条列进正文（`generate_weekly_report`）。
///
/// ⚠ 面板那一侧只念到**哪一年**：那句形状是它按 `{年} 年库` 切出来的，改了前缀它就不认得
/// 年份、只能照念整截。冒号后面那截原因上不了屏（那里只有一行、不换行也不裁切），它走日志
/// 与周报那两条路。
///
/// 面板那一侧有两条腿守着（`the_panel_claims_only_registered_domains`）：界面点的域名必须
/// 在这份清单上，清单上"该由面板接"的每一栏也必须真有人点。当年那条判据要防的症状就在这儿：
/// 加一栏时只改一处，界面那一栏就对着空榜说"暂无数据"。
pub const DOMAIN_KEYS: &str = "按键统计";
/// 前台应用时长榜。
pub const DOMAIN_APPS: &str = "应用时长";
/// 设备维度榜（Raw Input 那条侧信道）。
pub const DOMAIN_DEVICES: &str = "设备统计";
/// 按日序列（趋势图与星期分布都从它聚合出来，所以星期那一栏继承它的那句）。
pub const DOMAIN_DAILY: &str = "每日趋势";
/// 窗口内的每小时分布（跟着顶部周期那一问）。
pub const DOMAIN_HOURLY: &str = "每小时分布";
/// 某一天的 24 格（周报的时段直方图那一问，与上面那把窗口尺子是两件事）。
pub const DOMAIN_DAY_HOURS: &str = "当日小时分布";

/// 上面六条的一份清单，跨文件判据本该按它逐条走。
///
/// 新增域名时要让它被认出来，就得同时登记到这里 —— 这条**目前没有判据守着**（那条比对随
/// 旧前端一起删了）：漏登记不会红，只会让这个域名永远没人念。
pub const READ_DOMAINS: &[&str] = &[
    DOMAIN_KEYS,
    DOMAIN_APPS,
    DOMAIN_DEVICES,
    DOMAIN_DAILY,
    DOMAIN_HOURLY,
    DOMAIN_DAY_HOURS,
];

/// 面板"真有一栏来接"的那几条（每条在屏上都有自己那句提示）。
///
/// 桌面端按域名分栏说"这一轮没读全"，`desktop-native` 那条 `the_panel_claims_only_registered
/// _domains` 两头都查：面板点的名字必须在这份清单上，这份清单上该由面板接的每一栏也必须
/// 真有人点。不在这一份里的域名（当日小时分布）只有周报会念：面板上本来就没有"某一天几点"
/// 那一栏，硬要求它也有个牌子就是造一个永远为真的空位。
pub const PANEL_READ_DOMAINS: &[&str] = &[
    DOMAIN_KEYS,
    DOMAIN_APPS,
    DOMAIN_DEVICES,
    DOMAIN_DAILY,
    DOMAIN_HOURLY,
];

/// 统计周期取值是否合法：-1=今日 / 0=总计 / 1..=MAX_QUERY_DAYS 天。
///
/// 周期值必须在入口拦截，今天真走这条校验的是 CLI 的周期参数（`crates/cli/src/main.rs`）：
/// 此前任意负值（如 -2）会在 `NaiveDate - Days` 处 panic，release 下
/// `panic = "abort"`，且默认周期每次启动都会重聚合 —— 应用会永久无法启动。
pub fn is_valid_period(period: i64) -> bool {
    period == -1 || (0..=MAX_QUERY_DAYS).contains(&period)
}

/// `date - days`，越界时返回 `None`（不使用会 panic 的 `Sub<Days>` 实现）。
///
/// 入参的口径是"**往回减几天**"，`0` 是合法值（就是那一天本身）。刻意不复用
/// [`clamp_query_days`]：那是"窗口含几天"的口径、下界是 1，拿它钳增量会把
/// "减 0 天"抬成"减 1 天" ⇒ `get_daily_counts(1)`（起点 = 今天 - 0）返回的是**昨天**，
/// 而它自己的文档写着"含当天共 N 天"。
fn date_minus_days(date: chrono::NaiveDate, days: i64) -> Option<chrono::NaiveDate> {
    date.checked_sub_days(Days::new(days.clamp(0, MAX_QUERY_DAYS) as u64))
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

/// 年度库文件**在不在**，只许回答三态：在、确认不在、说不出（把错误交回调用方）。
///
/// 立这一条是因为 `Path::exists()` 对任何一次 stat 失败都回 `false`，于是"父目录所在的
/// 移动盘刚休眠"、"杀软首扫握着句柄"这类情况全被读成「那一年没有库」。在 `get_daily_counts`
/// 里，那一年每天都是 0，而日志一个字没有 —— 同一页上「今日」卡片还是真数。
/// 判据与 `data_location::is_confirmed_absent`、`unopenable_years` 一致：只有 `NotFound`
/// 算确认没有。
///
/// `pub(crate)`：问"这个年度库在不在"的地方都共用这一句（`db::writer::preload_device_meta`
/// 原先用 `Path::exists()`，把"这一次 stat 失败了"读成"今年还没建库"，于是那次启动
/// 所有设备名退成占位而日志一个字没有）。
/// 原先只在本模块用；写侧那处 `Path::exists()` 的漏读法（见下）把它提成共用的尺子。
/// 本机实测（都写在用例里）：路径的**父那一格是普通文件**时报的是 `ERROR_PATH_NOT_FOUND`
/// （⇒ NotFound ⇒ 按"没有"处理），所以那种形状不落在「说不出」这一支；`data` 位置被换成
/// 文件另有 `paths::ensure_dir` 的日志负责。
/// ⚠ 同一批现量还钉下这一支的两条边界。**走得到**：这个名字被换成自指 junction 时 stat 报
/// os error 1921（`FilesystemLoop`），用例是 `an_unanswerable_presence_question_is_not_read_as_no_database`。
/// **够不着它**：UNC 上那个共享不存在（os error 67/53）与盘符根本不存在（os error 3）都被
/// 标准库折成 `NotFound` ⇒ 落在「确认没有」那一支。所以"网络位置整个连不上"现在念起来
/// 与"那一年没建库"同形 —— 那是这把尺子已知的边界，要动它得先改 `is_confirmed_absent`
/// 的口径（只认 `NotFound`），已记台账等裁决，别在这里顺手改。
pub(crate) fn year_db_presence(path: &std::path::Path) -> Result<bool, std::io::Error> {
    classify_stat(std::fs::metadata(path))
}

/// `year_db_presence` 的判据部分（不经系统调用就能钉）。
fn classify_stat(stat: Result<std::fs::Metadata, std::io::Error>) -> Result<bool, std::io::Error> {
    match stat {
        Ok(_) => Ok(true),
        Err(e) if crate::data_location::is_confirmed_absent(&e) => Ok(false),
        Err(e) => Err(e),
    }
}

/// 这批年度库里有哪些**读不动**。
///
/// 「文件本来不存在」不算读不动：首启那一年还没建库，`get_daily_counts` 对它交回全零是正确答案。
/// 除此之外（确认在、以及 stat 说不出在不在）都去真开一次，**以开不开得起来定论** ——
/// 而不是把"stat 失败"直接当成读不动：那会让一道每 60 秒问一次的闸在数据其实读得出来的
/// 那一轮也拦下来。要说清的是这一句，不是"stat 失败也算读不动"。
fn unopenable_years(years: &[i32]) -> Vec<i32> {
    years
        .iter()
        .copied()
        .filter(|y| {
            let path = paths::year_db_path(*y);
            match year_db_presence(&path) {
                Ok(false) => false,
                // 确认在、以及"说不出在不在"，都去问一次真能不能打开（后者不预设结论）：
                // 这一支与 `Ok(true)` 同路是**刻意保持原口径**，别在这里顺手改成"当打不开"。
                Ok(true) | Err(_) => connection::open_ro(&path).is_err(),
            }
        })
        .collect()
}

/// 列出口径与读法口径都问一遍：数据目录读得动吗、每个列出来的年度库真打得开吗。
///
/// 这把尺子原先只有 CLI 有（`crates/cli/src/main.rs`），而 `available_years()` 把
/// `read_dir` 失败折成空列表、`stats_multi_year` 把打不开的那一年**静默跳过**，
/// 所以"读不到"在两个读侧函数里都不会留下任何痕迹。旧桌面端的导出是第三个读点，
/// 它原先直接 `get_stats(None, None)` 就写文件 —— 移动盘没插、或某年的库被同步盘
/// 换成占位文件时，用户拿到一份"总活跃次数 0"或明显偏小的报告，界面还报绿色成功
/// （那份导出的模块头当时写着"逻辑与 CLI 版一致"，那正是没对上的地方）。
///
/// 返回值里的 `Vec<i32>` 就是 `try_available_years()` 那份列表，调用方可以省一次枚举。
pub fn check_years_readable() -> anyhow::Result<Vec<i32>> {
    let years = try_available_years().map_err(|e| anyhow::anyhow!("年度库列不出来: {e}"))?;
    let bad = unopenable_years(&years);
    if !bad.is_empty() {
        // 只报年份不够：`[2026, 2020]` 这种形状告诉不了用户去动哪个文件，而这一句会一路
        // 走到 CLI 的退出与导出的拒绝那儿 —— 那一轮他什么数字都没拿到，手里只该有一件
        // 能做的事。文件路径这里给全（导出/命令行都是整段文字，容得下）；
        // 打卡条那一头走 `check_daily_counts_readable`，那是一行横条，不塞长路径。
        let files: Vec<String> = bad
            .iter()
            .map(|y| paths::year_db_path(*y).display().to_string())
            .collect();
        anyhow::bail!(
            "这些年度库打不开，它们的计数不会出现在结果里: {bad:?}（{}）—— \
             先确认这些文件还在原位、没有被同步盘换成占位文件、也没有被别的程序开着；\
             处理完再跑一次就有数",
            files.join("、")
        );
    }
    Ok(years)
}

/// `get_daily_counts` 那一段窗口的**起点与它会去读的年份**，一份算法两处用。
///
/// 口径：含当天共 N 天 ⇒ 起点 `今天 - (days-1)`；`days` 先收敛，越界会让日期运算 panic。
/// 为什么不直接复用 `query_years(days, None)`：那条的起点是 `今天 - days`，跨年头那一天两套
/// 清单会差一个年份 —— 可读性闸就会要么多拦（把不相干的坏年份算进这次读数），要么漏拦
/// （放过窗口里真正坏掉的那一年）。
fn window_start_and_years(
    today: chrono::NaiveDate,
    days: i64,
    year: Option<i32>,
) -> (chrono::NaiveDate, Vec<i32>) {
    let days = clamp_query_days(days);
    let start = date_minus_days(today, days - 1)
        .unwrap_or_else(|| today - Days::new((MAX_QUERY_DAYS - 1) as u64));
    let mut years: Vec<i32> = match year {
        Some(y) => vec![y],
        None => {
            let span: Vec<i32> = (start.year()..=today.year()).collect();
            let available: std::collections::HashSet<i32> = available_years().into_iter().collect();
            let f: Vec<i32> = span.into_iter().filter(|y| available.contains(y)).collect();
            // 一年都没有（首启、还没建库）：交回今年，读出来就是全零 —— 这是正确答案，
            // 不是"读不出来"，所以上面那道闸也不把"文件不存在"算成坏年份。
            if f.is_empty() {
                vec![today.year()]
            } else {
                f
            }
        }
    };
    years.sort_unstable();
    (start, years)
}

/// 只问「这次读数会碰到的那几个库」读不读得动 —— 打卡条看的是近 370 天，
/// 更早那一年坏了不该让这条自动刷新的横条整块变红，反之也不能放过窗口里坏掉的那一年
/// （它原先是**唯一没上这道闸的读数入口**：导出、周报与 CLI 那几条都问，界面这一头不问）。
///
/// 闸与读数各取一次"今天"：跨过午夜那一瞬两边的年份清单可能差一年，后果是这一次刷新被
/// 多拦或漏拦一次，下一次自动重试就一致了 —— 比"为了对齐把两次取时刻做成共享状态"便宜。
pub fn check_daily_counts_readable(days: i64, year: Option<i32>) -> anyhow::Result<Vec<i32>> {
    let years = window_start_and_years(Local::now().date_naive(), days, year).1;
    let bad = unopenable_years(&years);
    if !bad.is_empty() {
        anyhow::bail!("这些年度库打不开，那几年的计数不会出现在结果里: {bad:?}");
    }
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
///
/// **负序号必须能解**：`day_key_of_ts` 用的是 `div_euclid`，1970 年之前的时间戳
/// 本来就算得出负序号，而旧实现 `u64::try_from(day_key).ok()?` 把它们一律折成 `None`，
/// 后果是三处对不上（`maintenance.rs` 的归档算不出目标年份、每日序列丢掉那一行、
/// "最高单日"整格变成没有），而总计那一侧照算。现在两头都走 `checked_*_days`，
/// 只有真的超出 `NaiveDate` 可表示范围（约公元前 26 万年）才回 `None`。
pub(crate) fn day_key_to_date(day_key: i64) -> Option<chrono::NaiveDate> {
    let epoch = unix_epoch_date();
    // `unsigned_abs` 而不是 `-day_key`：后者在 `i64::MIN` 上会溢出，而 release 配置是
    // `panic = "abort"` —— 一次坏数据就让整个应用消失。
    let days = chrono::Days::new(day_key.unsigned_abs());
    if day_key >= 0 {
        epoch.checked_add_days(days)
    } else {
        epoch.checked_sub_days(days)
    }
}

/// Unix 秒 → 当日小时（0-23，本地时区）。
///
/// 与 `day_key_of_ts` 同一个理由：`div_euclid(3600)` 对 1970 年前的瞬时给出**负**商，
/// 而 Rust 的 `%` 跟着被除数的符号走 ⇒ 旧实现会算出 -1..-23 这种"小时"，
/// 它进得了 `(date_key, hour)` 主键、却落在任何 0..=23 的读侧窗口之外（又是总数算它、
/// 小时图看不见那一族）。`rem_euclid` 两头都收进 0..=23。
pub(crate) fn hour_of_ts(ts: i64) -> i64 {
    ((ts + local_utc_offset_seconds()).div_euclid(3600)).rem_euclid(24)
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
///
/// 走 `today_count_for_display`：跨过午夜、今天还没输入时缓存是昨天的，
/// 这里必须按日期对一次（插件 `today_count` 随时可能先被问到）。
pub fn get_today_count(writer: Option<&crate::db::DbWriter>) -> i64 {
    if let Some(w) = writer {
        w.today_count_for_display() as i64
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
///
/// ⚠ 这条二元组口子**分不出**"读不出来"与"没有数据"（它把 `unreadable` 丢掉了）：
/// 面板与导出请走 [`stats_totals`] / [`app_stats_totals`] / [`device_totals`]。
/// 留着它是因为 Lua 宿主那一格统计按 `(总数, 榜)` 用：现量（2026-10-29）生产端只有
/// `plugins/host.rs` 的"0 与非 0"两臂，CLI 早改走 [`stats_totals`]。
pub fn get_stats(days: Option<i64>, year: Option<i32>) -> (i64, HashMap<String, i64>) {
    let totals = stats_totals(days, year);
    (totals.total, totals.rows)
}

/// 查询指定日期统计。⚠ 同 [`get_stats`]：交回的是下界，说不出"没读全"。
pub fn get_stats_by_date(target_date: chrono::NaiveDate) -> (i64, HashMap<String, i64>) {
    let totals = stats_totals_on(target_date);
    (totals.total, totals.rows)
}

/// 一个年度库在这一趟读数里的结局。口径同 `YearMaxDay`（本文件上面那个每库读手）：
/// "打不开/查询失败"、"库里没这张表"、"读到了"是三件事，折成两件就会说谎。
enum YearRead<T> {
    /// 读到了（`T` 由各域决定形状）
    Read(T),
    /// 库里没有这张表：真没有可汇总的数据，按 0 计是对的
    NoTable,
    /// 这一问没有答案（表在而读不出来、或 SUM/查询失败）：不能按 0 计
    Unreadable(String),
}

/// 一个窗口内的排行读数（总次数/总秒数 + 榜单）连同**这次有没有读全**。
///
/// 立它的形状照 [`AlltimeMaxDay`]：`unreadable` 非空就是"结果不完整"的信号，
/// 面板与导出必须把它说出来，而不是把一份下界念成"今日 0 次、排行榜为空"。
#[derive(Debug, Clone, Default)]
pub struct RankTotals {
    /// 读到的那些年度库之和。`unreadable` 非空时它只是**下界**。
    pub total: i64,
    /// 读到的那些榜行（跨年已按名字合并；降序与截断由调用方负责）。
    pub rows: HashMap<String, i64>,
    /// 这一轮没答上来的年度库，每项形如「N 年库<域>读不出来: 原因」。空 = 读全了。
    pub unreadable: Vec<String>,
}

/// 逐年度库取排行读数并合并；`label` 只用于把 gap 说到"是哪个域的哪一年"。
fn gather_rank_totals(
    years: &[i32],
    label: &str,
    cond: &str,
    param: Option<i64>,
    query: impl Fn(&Connection, &str, Option<i64>) -> YearRead<(i64, HashMap<String, i64>)>,
) -> RankTotals {
    let mut out = RankTotals::default();
    for year in years.iter().copied() {
        let path = paths::year_db_path(year);
        // 先过那道"在不在"的三态闸：没有库是答案，说不出才要出声
        match gate_year(year, &path, label) {
            YearGate::SkipAbsent => continue,
            YearGate::CannotTell(why) => {
                out.unreadable
                    .push(format!("{year} 年库{label}读不出来: {why}"));
                continue;
            }
            YearGate::Read => {}
        }
        let got = connection::with_ro_conn_why(&path, |conn| query(conn, cond, param));
        match got {
            Err(why) => out
                .unreadable
                .push(format!("{year} 年库{label}读不出来: {why}")),
            Ok(YearRead::Unreadable(why)) => out
                .unreadable
                .push(format!("{year} 年库{label}读不出来: {why}")),
            // 没有这张表 = 这一年确实没有可汇总的数据；安静按 0 计
            Ok(YearRead::NoTable) => {}
            Ok(YearRead::Read((total, rows))) => {
                out.total += total;
                for (k, v) in rows {
                    *out.rows.entry(k).or_insert(0) += v;
                }
            }
        }
    }
    out
}

/// 逐年开库之前的那一道"在不在"之问（三处读数共用：榜、设备、时间窗口）。
///
/// 为什么单独成一个判据而不是各写一份：**那一年根本没有库**（首启那年、整年没记录、
/// 历史那年已归档清掉）是一个正确答案，不是"读不出来"；把它说成读不出来，周报逐日问
/// 2020 年时会拒绝生成一份本来正常的文件（本轮现量过一次，另一头则由面板念 0 过一次）。
/// 而"说不出在不在"又不许折回"没有"。三个结局必须是三条不同的路，`bool` 只能给两条。
/// 这一臂现量走得到的一种形状：这个名字是个解析不出目标的重解析点（自指／成环的目录
/// junction），stat 报 os error 1921 ⇒ 有用例端到端守着；按错误码钉的还有 5（拒绝访问）
/// 与 267。反过来，"父那一格是普通文件"、"UNC 上的共享不存在"、"盘符根本不存在"本机
/// 都报 `NotFound` ⇒ 走的是「确认没有」，别以为这道闸把它们也拦住了（那两条边界写在
/// `year_db_presence` 的文档上）。
enum YearGate {
    /// 确认在：照常开库去读
    Read,
    /// 确认没有这个文件：安静跳过，这一年按 0 计
    SkipAbsent,
    /// 说不出在不在：把原因交出去（调用方按自己的域名拼成 gap）
    CannotTell(String),
}

fn gate_year(year: i32, path: &std::path::Path, label: &str) -> YearGate {
    match year_db_presence(path) {
        Ok(false) => YearGate::SkipAbsent,
        Ok(true) => YearGate::Read,
        Err(e) => {
            tracing::error!(
                "说不出 {year} 年的库在不在（{}）：{e} —— 这一年不进{label}的读数，\
                 但那不是「那一年没有数据」",
                path.display()
            );
            YearGate::CannotTell(format!("说不出这一年的库在不在: {e}"))
        }
    }
}

/// 按键统计的读数（含"这一轮有没有读全"）。
pub fn stats_totals(days: Option<i64>, year: Option<i32>) -> RankTotals {
    let years = match year {
        Some(y) => vec![y],
        None => query_years(days, None),
    };
    let start_dk = cutoff_day_key(days);
    let cond = if start_dk.is_some() {
        "date_key >= ?1"
    } else {
        ""
    };
    gather_rank_totals(&years, DOMAIN_KEYS, cond, start_dk, |conn, cond, param| {
        query_stats_in_conn(conn, cond, param, true)
    })
}

/// 某一天的按键统计读数。
pub fn stats_totals_on(target_date: chrono::NaiveDate) -> RankTotals {
    let dk = day_key_of_date(target_date);
    gather_rank_totals(
        &[target_date.year()],
        DOMAIN_KEYS,
        "date_key = ?1",
        Some(dk),
        |conn, cond, param| query_stats_in_conn(conn, cond, param, false),
    )
}

/// 单库统计公共实现：`daily_counts` 求总数 + `key_counts` 按键聚合。
///
/// `cond` 为日期条件片段（如 "date_key >= ?1"），空串表示全表；`param` 与 `cond`
/// 配套（None = 无参数）。`group=true` 时键聚合用 SUM+GROUP BY（多日窗口需合并），
/// false 时单日内键唯一、直接取 count。
/// 三种结局分开：旧写法先 `table_exists` 折一次、`SUM` 再 `unwrap_or(0)` 折一次，
/// 于是"库损坏那一刻读不出来"与"这一年没数据"交回同一个 `None` ⇒ 上层念成 0。
///
/// ⚠ 取行要用 `collect::<Result<Vec<_>, _>>()`，不许回到旧写法 `rows.flatten()`：
/// `Result` 自身是 `IntoIterator`，flatten 会丢掉那一次错误、只交出坏点之前的行 ⇒
/// "读到第 N 页库坏了"被念成"榜就 N−1 行，读全了"。这条用例是
/// `a_rank_reading_separates_cannot_read_from_no_data` 的第四条腿（库头合法、页坏）。
fn query_stats_in_conn(
    conn: &rusqlite::Connection,
    cond: &str,
    param: Option<i64>,
    group: bool,
) -> YearRead<(i64, HashMap<String, i64>)> {
    match connection::table_presence_cached(conn, "daily_counts") {
        connection::Presence::No => return YearRead::NoTable,
        connection::Presence::Unreadable(why) => return YearRead::Unreadable(why),
        connection::Presence::Yes => {}
    }
    let where_clause = if cond.is_empty() {
        String::new()
    } else {
        format!(" WHERE {cond}")
    };
    let summed = match param {
        Some(p) => conn.query_row(
            &format!("SELECT COALESCE(SUM(count), 0) FROM daily_counts{where_clause}"),
            [p],
            |r| r.get::<_, i64>(0),
        ),
        None => conn.query_row(
            "SELECT COALESCE(SUM(count), 0) FROM daily_counts",
            [],
            |r| r.get::<_, i64>(0),
        ),
    };
    let total: i64 = match summed {
        Ok(v) => v,
        Err(e) => {
            return YearRead::Unreadable(format!("汇总这一窗口的次数时读不出来: {e}"));
        }
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
                // 迭代器每一项都是 Result：库读到一半坏了、还是某行取值失败，都出在这里。
                // collect 让它们都变成 Err；旧写法 flatten() 会把第一项错误之后的一切当"读完了"。
                Some(p) => stmt
                    .query_map([p], mapper)
                    .and_then(|rows| rows.collect::<Result<Vec<_>, rusqlite::Error>>()),
                None => stmt
                    .query_map([], mapper)
                    .and_then(|rows| rows.collect::<Result<Vec<_>, rusqlite::Error>>()),
            },
            Err(e) => Err(e),
        };
        match rows {
            Ok(list) => list.into_iter().collect(),
            Err(e) => {
                tracing::error!("key_counts 查询失败 ({sql}): {e}");
                return YearRead::Unreadable(format!("键名榜读不出来: {e}"));
            }
        }
    };
    YearRead::Read((total, map))
}

/// 全历史「最高单日」（跨年度库）连同**这次有没有读全**。
///
/// 这里刻意**不**算全历史总次数：屏上那一格「总计」念的是当前周期的
/// `stats_totals(...).total + pending`（`desktop-native/src/data.rs:528`），
/// 与这一趟无关。旧形状每拍多带一条 `SELECT SUM(count) FROM daily_counts` 走全部年度库，
/// 而它交回的 `total` 生产侧一个读者都没有（只有三条用例读它）——
/// 现量见 `crates/core/tests/perf_probe.rs` 的「每一臂自己几条语句」：那一趟每拍占
/// 「每库 1 条」，今日档 2 条、总计档 2 条，都是白算的。
/// 全历史次数这条口径仍由 [`get_stats`]／[`stats_totals`]（`days = None`）守着，
/// `aggregation_test.rs` 与 `real_data.rs` 都还在比它。
#[derive(Debug, Clone)]
pub struct AlltimeMaxDay {
    /// 读到的那些年度库里的最高单日（`YYYY-MM-DD`, 次数）。
    ///
    /// `unreadable` 非空时它只是**已读那几年的最大值**：某一年没答上来，它可能正握着
    /// 真正的那一天 —— 调用方据此不能把"少了一年"念成"纪录就是这个"。
    pub max_day: Option<(String, i64)>,
    /// 读到的那些年度库里 `daily_counts` **记到过次数**（count > 0）的天数，
    /// 也就是「总计」那一档「日均」的分母。
    ///
    /// 与 `max_day` 同一趟、同一个每库读手交回来，所以分子（全历史次数）与这个分母点到的
    /// 库是同一批；`unreadable` 非空时它同样是下界（少一年就少那一年那些天）。
    pub active_days: i64,
    /// 这一次没能读出来的年度库，每项形如「年份: 原因」。
    ///
    /// 非空就是"结果不完整"的信号：调用方（快照线程）要么把它念到屏上，
    /// 要么就别拿这份残缺值去覆盖上一份。
    pub unreadable: Vec<String>,
}

/// 单个年度库在这一趟「最高单日」遍历里的结局。
///
/// 旧写法把三件事折成同一个 `None`／`0`：打不开、没有 `daily_counts` 这张表、表在但
/// 查询失败 —— 于是「有一年完全读不出来」与「那一年本来就没数据」在结果里长得一模一样，
/// 而「最高单日」`unwrap_or` 之后还能与别的卡片拼出自相矛盾的一屏。
#[derive(Debug)]
enum YearMaxDay {
    /// 这一年读到了：`top` 是本年最高单日（表里没行时是 `None`），`days` 是本年有行的天数。
    Read { top: Option<(i64, i64)>, days: i64 },
    /// 这个库里没有 `daily_counts` 表：真的没有可汇总的数据，算"没有纪录"是对的。
    NoTable,
    /// 这一年**没有答案**（表在，但查询失败）：不能当成"这一年没纪录"。
    Unreadable(String),
}

/// 在一个年度库连接上取回这一年的最高单日，读不出来就说读不出来。
fn year_max_day_in_conn(conn: &Connection) -> YearMaxDay {
    match connection::table_presence_cached(conn, "daily_counts") {
        connection::Presence::No => return YearMaxDay::NoTable,
        connection::Presence::Unreadable(why) => return YearMaxDay::Unreadable(why),
        connection::Presence::Yes => {}
    }
    // 只这一条：排 (count DESC, date_key ASC) 两个键，`daily_counts` 一年 365 行，
    // 全表扫比给 count 单列建索引划算（那条索引已实测无收益并移除，见
    // `connection::ensure_schema` 里的论证）。
    // 第三列那个标量子查询顺手把"这一年有几天记过事"带回来（「总计」那一档「日均」的
    // 分母）：它跟着同一趟、同一个库，所以每库仍然只有**一条**语句，不为多要一个数
    // 再排一次队。表里没行时 `query_row` 先拿到 `QueryReturnedNoRows`，`days` 留在 0 ——
    // 那一年确实一天都没有，按 0 计是对的。
    let mut days = 0i64;
    let top = match conn.query_row(
        "SELECT date_key, count, (SELECT COUNT(*) FROM daily_counts WHERE count > 0) \
         FROM daily_counts ORDER BY count DESC, date_key ASC LIMIT 1",
        [],
        |r| {
            days = r.get::<_, i64>(2)?;
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        },
    ) {
        Ok(v) => Some(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => {
            return YearMaxDay::Unreadable(format!("找这一年的最高单日时读不出来: {e}"));
        }
    };
    YearMaxDay::Read { top, days }
}

/// 全历史「最高单日」（跨年度库）。
///
/// ⚠ 这里**没有**缓存，也**没有**注释曾经承诺的那份失效条件：本函数每次调用都无条件
/// 重扫一遍全部年度库（`available_years()` 那份 30 秒缓存只缓存"有哪几年"，不缓存结果）。
/// 旧注释写的「与「总计」卡片共用同一份失效条件（强制刷新 / 跨天 / 今日新增达阈值）」
/// 「这一趟的产出会被统计线程当作全历史基线缓存」在本仓都不成立 ——
/// 快照线程每拍调它（`desktop-native/src/data.rs` 的 `load()`），一句也没有兑现过。
/// 要不要真上一份缓存是另一件事：那要先把"今日恰好破纪录的那一拍必须跟上"钉成判据，
/// 已登记待用户点，不在本轮。
///
/// 返回里带 `unreadable`：某一年打不开或读不出来时，那个"最大值"只是**已读那几年的**
/// 最大值，必须让调用方看得见 —— 静默按"这一年没纪录"计入，屏上念的就不可能是真纪录。
pub fn get_alltime_max_day() -> AlltimeMaxDay {
    let mut best: Option<(i64, i64)> = None; // (date_key, count)
    let mut active_days: i64 = 0;
    let mut unreadable: Vec<String> = Vec::new();
    // 名单**本身**问不出来的时候不能折成"库里没历史"：`available_years()` 在 `read_dir`
    // 失败时交回空列表 ⇒ 下面这个循环一次都不跑 ⇒ `unreadable` 是空的，而本函数的文档写的
    // 正是「`unreadable` 非空才是结果不完整的信号」。于是屏上「最高单日」念「—」、「日均」
    // 的分母念 0，与"这台机器从没记过东西"长得一模一样 —— 同一个形状在自愈
    // (`heal_daily_consistency`) 与三个破坏性命令里都已经改成走 `try_available_years()`。
    let years = match try_available_years() {
        Ok(years) => years,
        Err(e) => {
            let why = format!("年度库列不出来: {e}");
            tracing::error!("全历史最高单日这一趟没能开始：{why}");
            return AlltimeMaxDay {
                max_day: None,
                active_days: 0,
                unreadable: vec![why],
            };
        }
    };
    for year in years {
        let path = paths::year_db_path(year);
        // `with_ro_conn` 返回 `None` 只可能是「连接没拿到」（打开失败已在连接层记过 error），
        // 拿到连接之后的结局由下面那个每库读手自己分臂 —— 两者原先被同一个
        // `.flatten()` 折成"这一年没数据"。
        let arm = connection::with_ro_conn(&path, year_max_day_in_conn);
        match arm {
            Some(YearMaxDay::Read { top, days }) => {
                active_days += days.max(0);
                if let Some((dk, c)) = top {
                    if best.is_none_or(|(_, bc)| c > bc) {
                        best = Some((dk, c));
                    }
                }
            }
            Some(YearMaxDay::NoTable) => {}
            Some(YearMaxDay::Unreadable(why)) => unreadable.push(format!("{year}: {why}")),
            None => unreadable.push(format!("{year}: 只读连接打不开（原因见上一行 error 日志）")),
        }
    }
    if !unreadable.is_empty() {
        tracing::warn!(
            "全历史「最高单日」不完整：{} 个年度库这次没读出来（{}），已读部分里的最大值: {:?}",
            unreadable.len(),
            unreadable.join("；"),
            best
        );
    }
    let max_day =
        best.and_then(|(dk, c)| day_key_to_date(dk).map(|d| (d.format("%Y-%m-%d").to_string(), c)));
    AlltimeMaxDay {
        max_day,
        active_days,
        unreadable,
    }
}

/// 每日趋势读数（含"这一轮没读全"）。`series` 在读不全时只是**下界**：
/// 没读出来的那一年，它那些天挂在 0 上。
#[derive(Debug, Clone, Default)]
pub struct DailyTotals {
    pub series: Vec<(String, i64)>,
    /// 每项形如「N 年库每日趋势读不出来: 原因」。空 = 读全了。
    pub unreadable: Vec<String>,
}

/// 所选窗口内每小时合计的读数（长度 24）。同样：读不全时只是下界。
#[derive(Debug, Clone, Default)]
pub struct HourlyTotals {
    pub hours: Vec<i64>,
    pub unreadable: Vec<String>,
}

/// 窗口内 `(第一列, 第二列)` 两段查询只差 SQL 一句，取行的错误处理完全一样：
/// 收成一个函数，免得一处补了另一处漏（这一轮之前两处都在 `rows.flatten()` 上把坏页折掉）。
fn read_pairs(
    conn: &Connection,
    sql: &str,
    start_dk: i64,
    end_dk: i64,
    why: &str,
) -> YearRead<Vec<(i64, i64)>> {
    let mapper = |r: &rusqlite::Row<'_>| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?));
    // `query_map` 借的是 `stmt`，所以这一步不能写成 `prepare(..).and_then(..)`（那要
    // 把借用带出闭包）；prepare 的错误与取行的错误在下面汇成同一个出口。
    let rows = match conn.prepare(sql) {
        Ok(mut stmt) => stmt
            .query_map(rusqlite::params![start_dk, end_dk], mapper)
            .and_then(|rows| rows.collect::<Result<Vec<_>, rusqlite::Error>>()),
        Err(e) => Err(e),
    };
    match rows {
        Ok(list) => YearRead::Read(list),
        Err(e) => {
            tracing::error!("{why}（{sql}）: {e}");
            YearRead::Unreadable(format!("{why}: {e}"))
        }
    }
}

/// 单库读窗口内的每日计数 `(date_key, count)`；三种结局分开（理由同 [`query_stats_in_conn`]）。
fn query_daily_window_in_conn(
    conn: &Connection,
    start_dk: i64,
    end_dk: i64,
) -> YearRead<Vec<(i64, i64)>> {
    match connection::table_presence_cached(conn, "daily_counts") {
        connection::Presence::No => return YearRead::NoTable,
        connection::Presence::Unreadable(why) => return YearRead::Unreadable(why),
        connection::Presence::Yes => {}
    }
    read_pairs(
        conn,
        "SELECT date_key, count FROM daily_counts WHERE date_key >= ?1 AND date_key <= ?2",
        start_dk,
        end_dk,
        "每日计数读不出来",
    )
}

/// 单库读窗口内的每小时合计 `(hour, SUM(count))`。
fn query_hourly_window_in_conn(
    conn: &Connection,
    start_dk: i64,
    end_dk: i64,
) -> YearRead<Vec<(i64, i64)>> {
    match connection::table_presence_cached(conn, "hourly_counts") {
        connection::Presence::No => return YearRead::NoTable,
        connection::Presence::Unreadable(why) => return YearRead::Unreadable(why),
        connection::Presence::Yes => {}
    }
    read_pairs(
        conn,
        "SELECT hour, SUM(count) FROM hourly_counts \
         WHERE date_key >= ?1 AND date_key <= ?2 GROUP BY hour",
        start_dk,
        end_dk,
        "每小时计数读不出来",
    )
}

/// 一趟窗口读回来的行：`(date_key, 次数)` 序列。
type WindowRows = Vec<(i64, i64)>;
/// 一趟窗口读回来的缺读：`(哪一年, 那句点名的话)`。
///
/// 年份这一格不是装饰：[`read_daily_pair`] 要把一趟宽窗口的读数分给两段窗口，
/// 分头的依据就是"这一年属不属于那一段"。若交回来的只是一串句子，它就得回头去切
/// `{年} 年库…` 那个前缀 —— 同一件事第二把尺子（面板那边已经切过一次了）。
type WindowGaps = Vec<(i32, String)>;

/// 逐年度库把窗口内的行读回来。`label` 只用于把 gap 说到"是哪个域的哪一年"。
///
/// 这一支以前有四道各自为政的折：`year_db_presence` 说不出在不在只留一行日志、
/// `with_ro_conn` 把"打不开"折成 `None`（连那句日志都不给调用方）、`table_exists` 把
/// "问不出有没有这张表"折成"没有"、`rows.flatten()` 把"读到一半坏页"折成"到这儿为止"。
/// 四道现在都落到 `unreadable`，交回的行只是**已读部分**。
///
/// ⚠ 交回的缺读带**年份整数**，不只带那句拼好的话：调用方里有一位要把一段宽窗口的
/// 缺读分给两个窄窗口（[`read_daily_pair`]），那必须按"哪一年"筛。若让它回头去切
/// `{年} 年库…` 那句字符串，同一件事就又有第二把尺子（面板那边已经切过一次了）。
fn read_window_years(
    label: &str,
    years: &[i32],
    start_dk: i64,
    end_dk: i64,
    query: impl Fn(&Connection, i64, i64) -> YearRead<Vec<(i64, i64)>>,
) -> (WindowRows, WindowGaps) {
    let mut pairs: WindowRows = Vec::new();
    let mut gaps: WindowGaps = Vec::new();
    for y in years.iter().copied() {
        let path = paths::year_db_path(y);
        match gate_year(y, &path, label) {
            // 没有这一年的库文件是**正常答案**（那年没数据、或全新安装）：安静跳过，
            // 那些天留在初始化好的 0 上。
            YearGate::SkipAbsent => continue,
            YearGate::CannotTell(why) => {
                gaps.push((y, format!("{y} 年库{label}读不出来: {why}")));
                continue;
            }
            YearGate::Read => {}
        }
        match connection::with_ro_conn_why(&path, |conn| query(conn, start_dk, end_dk)) {
            Err(why) => gaps.push((y, format!("{y} 年库{label}读不出来: {why}"))),
            Ok(YearRead::Unreadable(why)) => {
                gaps.push((y, format!("{y} 年库{label}读不出来: {why}")))
            }
            // 没有这张表 = 这一年确实没有可汇总的数据；安静按 0 计
            Ok(YearRead::NoTable) => {}
            Ok(YearRead::Read(v)) => pairs.extend(v),
        }
    }
    (pairs, gaps)
}

/// 一次库上的读，交出**两段**每日序列：窄的那段与宽的那段（宽 ⊇ 窄）。
///
/// 为什么是一段读两段用：快照线程那一拍原来为每日趋势与"打卡条"各扫一遍
/// `daily_counts` —— 一道 `chart_window_days`（最多 30 天）、一道 `GOAL_LOOKBACK_DAYS`
/// （370 天）。宽的那段把窄的整个盖住，同一天在两段里必然同值，问两遍就是白问。
///
/// ⚠ 缺读按**各自窗口的年份清单**分头交回，不许把宽窗口点到的坏年份原样塞给窄的那段：
/// 图上只画了 2026 年那些天，面板却说"2025 年库每日趋势读不出来"，那是把不相干的一年
/// 算在这张图头上 —— 数字没错，指控错了人。两份年份清单都出自
/// [`window_start_and_years`] 同一把尺子，所以"窄窗口的年份 ⊆ 宽窗口的年份"是构造保证的，
/// 不是从切出来的日期反推的。
fn read_daily_pair(
    small_days: i64,
    wide_days: i64,
    year: Option<i32>,
) -> (DailyTotals, DailyTotals) {
    let today = Local::now().date_naive();
    let small_days = clamp_query_days(small_days);
    // 宽的一段至少要把窄的盖住，否则"从一趟里切两段"这个前提就不成立了
    let wide_days = clamp_query_days(wide_days).max(small_days);
    let (start, years_to_query) = window_start_and_years(today, wide_days, year);
    let start_dk = day_key_of_date(start);
    let end_dk = day_key_of_date(today);

    // 初始化宽窗口里所有日期为 0（`day_key` 是连续的"历元第几天"，所以 +i 就是 +i 天）
    let mut daily_map: HashMap<i64, i64> = HashMap::new();
    for i in 0..wide_days {
        daily_map.insert(start_dk + i, 0);
    }

    let (pairs, gaps) = read_window_years(
        DOMAIN_DAILY,
        &years_to_query,
        start_dk,
        end_dk,
        query_daily_window_in_conn,
    );
    for (dk, c) in pairs {
        if let Some(e) = daily_map.get_mut(&dk) {
            *e += c;
        }
    }

    let small_start_dk = end_dk - small_days + 1;
    let small_years: std::collections::HashSet<i32> =
        window_start_and_years(today, small_days, year)
            .1
            .into_iter()
            .collect();
    let mut small_series: Vec<(String, i64)> = Vec::with_capacity(small_days as usize);
    let mut wide_series: Vec<(String, i64)> = Vec::with_capacity(daily_map.len());
    for (dk, c) in daily_map {
        if let Some(d) = day_key_to_date(dk) {
            let row = (d.format("%Y-%m-%d").to_string(), c);
            if dk >= small_start_dk {
                small_series.push(row.clone());
            }
            wide_series.push(row);
        }
    }
    small_series.sort();
    wide_series.sort();
    (
        DailyTotals {
            series: small_series,
            unreadable: gaps
                .iter()
                .filter(|(y, _)| small_years.contains(y))
                .map(|(_, s)| s.clone())
                .collect(),
        },
        DailyTotals {
            series: wide_series,
            unreadable: gaps.into_iter().map(|(_, s)| s).collect(),
        },
    )
}

/// 查询最近 N 天每日按键数：[(YYYY-MM-DD, 次数)]，含"这一轮没读全"。
pub fn daily_counts_totals(days: i64, year: Option<i32>) -> DailyTotals {
    // 单窗口出口 = 两段同宽。窗口算术、补零、缺读点名都住在 `read_daily_pair`，
    // 这里不再自己走一遍（那正是"同一件事两份答案"当年长出来的地方）。
    read_daily_pair(days, days, year).0
}

/// 一趟查回快照线程那一拍要的两段每日序列：给图的 `chart_days`，与给打卡条的
/// `lookback_days`（打卡那条要用宽的那段；它的缺读**不进**图的那句点名，理由见
/// 本文件上面的 `read_daily_pair`）。
pub fn daily_counts_windows(chart_days: i64, lookback_days: i64) -> (DailyTotals, DailyTotals) {
    read_daily_pair(chart_days, lookback_days, None)
}

/// 查询最近 N 天每日按键数：返回 [(YYYY-MM-DD, 次数)]。
///
/// ⚠ 数组交不回"这一轮有没有读全"；面板与导出请走 [`daily_counts_totals`]。
/// 留给 Lua 宿主、打卡条那条（它上面另有 `check_daily_counts_readable` 那道闸）与 CLI。
pub fn get_daily_counts(days: i64, year: Option<i32>) -> Vec<(String, i64)> {
    daily_counts_totals(days, year).series
}

/// 顶部周期 → 该数几天。`-1` 今日 / `0` 总计 / 正整数 = 近 N 天 / 其余退回 7 天。
///
/// 尺子放在 core 是因为同一件事有三处在问：界面的趋势/小时/星期三张图、导出报告里那
/// 三段"时间切面"、以及 `focusflow-cli --hours`。各写一份就会出现"同一份库、
/// 屏幕上说是近 30 天、命令行里说是全部历史"。
///
/// `0`（「总计」）刻意是 30 而不是 `MAX_QUERY_DAYS`：图横跨全部历史就画不成（一天一根柱，
/// 几年下来几百根挤在一张卡上），而这三张切面各自数的是哪一段由调用方在屏上自己念出来
/// （`desktop-native/src/data.rs` 的 `window_text`，「总计」那一档念「近 30 天」）。
/// ⚠ 这一把尺子**不再**是卡片那句「日均」的分母：那一格在「总计」档吃的是全历史里记到过
/// 次数的天数（[`get_alltime_max_day`] 的 `active_days`）。旧注释写的"与卡片同一个分母"
/// 在 2026-10-29 那一笔之后就不成立了 —— 分子是整个周期，分母却按图上这一段数，
/// 「总计」那一档的日均因此被抬了十几倍，那才是先要修的地方。
pub fn chart_window_days(period: i64) -> i64 {
    match period {
        -1 => 1,
        0 => 30,
        n if n > 0 => n,
        _ => 7,
    }
}

/// 指定日期每小时按键数（长度 24），含"这一轮没读全"。
///
/// 与 [`hourly_window_totals`] 是两件事：那一问跟着顶部周期把窗口内每天加总，这一问只看
/// 某一天几点多几点少。
///
/// ⚠ 现量（2026-10-29）：这一问在**生产端一个读者都没有**——唯一的引用是下面那个交数组的
/// 薄包装 [`get_hourly_stats`]，而那个包装也只剩测试与再导出在点名；周报正文里根本没有
/// 时段那一节（只有每日活跃／键鼠榜／应用／设备榜四节），Lua 宿主也没有 hourly 入口。
///
/// 这一支以前四道各自折：`with_ro_conn` 把打不开折成 `None`、`table_exists` 把问不出
/// 折成"没这张表"、`prepare`/`query_map` 的错误只写日志、取行用 `rows.flatten()`
/// 把"读到坏页"折成"就这几个小时有记录"。四道的结局都是 24 个 0 —— 于是周报把一份
/// 读不动的历史印成"这一周夜里什么都没发生"。
pub fn hourly_stats_totals(target_date: Option<chrono::NaiveDate>) -> HourlyTotals {
    let d = target_date.unwrap_or_else(|| Local::now().date_naive());
    let dk = day_key_of_date(d);
    let year = d.year();
    let path = paths::year_db_path(year);
    let gap_of = |why: String| format!("{year} 年库{DOMAIN_DAY_HOURS}读不出来: {why}");
    // 先过那道"在不在"的三态闸：没有这一年的库是答案（安静 24 个 0），
    // 说不出才要出声 —— 少了这一步，周报会把"没有 2020 年的库"讲成一份拒绝。
    let mut unreadable: Vec<String> = Vec::new();
    match gate_year(year, &path, DOMAIN_DAY_HOURS) {
        YearGate::SkipAbsent => {
            return HourlyTotals {
                hours: vec![0i64; 24],
                unreadable,
            }
        }
        YearGate::CannotTell(why) => {
            unreadable.push(gap_of(why));
            return HourlyTotals {
                hours: vec![0i64; 24],
                unreadable,
            };
        }
        YearGate::Read => {}
    }
    let got = connection::with_ro_conn_why(&path, |conn| -> YearRead<Vec<(i64, i64)>> {
        match connection::table_presence_cached(conn, "hourly_counts") {
            connection::Presence::No => return YearRead::NoTable,
            connection::Presence::Unreadable(why) => return YearRead::Unreadable(why),
            connection::Presence::Yes => {}
        }
        let rows = match conn.prepare("SELECT hour, count FROM hourly_counts WHERE date_key = ?1") {
            Ok(mut stmt) => stmt
                .query_map([dk], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
                .and_then(|rows| rows.collect::<Result<Vec<_>, rusqlite::Error>>()),
            Err(e) => Err(e),
        };
        match rows {
            Ok(list) => YearRead::Read(list),
            Err(e) => {
                tracing::error!("小时分布查询失败（{year} 年库）: {e}");
                YearRead::Unreadable(format!("{DOMAIN_DAY_HOURS}读不出来: {e}"))
            }
        }
    });
    let mut hours = vec![0i64; 24];
    match got {
        Err(why) => unreadable.push(gap_of(why)),
        Ok(YearRead::Unreadable(why)) => unreadable.push(gap_of(why)),
        // 没有这张表 = 那一天确实没有按小时的记录；安静交回 24 个 0
        Ok(YearRead::NoTable) => {}
        Ok(YearRead::Read(list)) => {
            for (h, c) in list {
                // 越界的小时值历史上真存过，这一问是承重的（与窗口那一问同一道闸）
                if (0..24).contains(&h) {
                    hours[h as usize] += c;
                }
            }
        }
    }
    HourlyTotals { hours, unreadable }
}

/// 查询指定日期每小时按键数（返回长度 24 的列表）。
///
/// ⚠ 同 [`get_daily_counts`]：24 个 0 说不出"那一天没记录"还是"这一年读不动"。
///
/// ⚠ 现量（2026-10-29）：生产端**没有调用方**——"只用数组的旧调用方"随旧前端一起删了，
/// 剩下的命中只有测试与 `db` 的再导出。要问每小时请走 [`hourly_window_totals`]（它交得回
/// "这一轮有没有读全"）。
pub fn get_hourly_stats(target_date: Option<chrono::NaiveDate>) -> Vec<i64> {
    hourly_stats_totals(target_date).hours
}

/// 所选窗口内每小时的合计键数（长度 24），含"这一轮没读全"。
///
/// 与 [`get_hourly_stats`]（只看某一天）不是一件事，两个都留着：周报的时段直方图问的就是
/// "那一天几点"，而分析页的每小时分布要跟顶部周期走 —— 把一头硬套成另一头，就会有一头
/// 说错话（旧写法是整页写死今日，顶部选了「近 1 年」图上的标题却还写着"今日"）。
/// ⚠ 那一天那一问（`get_hourly_stats`）现在只是它的薄包装：二元组交不回"这一轮有没有读全"。
/// 现量（2026-10-29）问这一条的只有两处——面板（`desktop-native/src/data.rs`）与 CLI
/// （`--hours`）；周报没有时段那一节，Lua 宿主也没有 hourly 入口，所以那一天那一问连同
/// 它的数组包装在生产端一个读者都没有。
///
/// 跨年份那套口径照 [`daily_counts_totals`]：某一年的库**问不出在不在**时跳过它并点名是哪一年、
/// 为什么，绝不折成"那一年每小时都是 0"。
pub fn hourly_window_totals(days: i64, year: Option<i32>) -> HourlyTotals {
    let today = Local::now().date_naive();
    let days = clamp_query_days(days);
    let (start, years_to_query) = window_start_and_years(today, days, year);
    let start_dk = day_key_of_date(start);
    let end_dk = day_key_of_date(today);
    let mut hours = vec![0i64; 24];
    let (pairs, gaps) = read_window_years(
        DOMAIN_HOURLY,
        &years_to_query,
        start_dk,
        end_dk,
        query_hourly_window_in_conn,
    );
    for (h, c) in pairs {
        // 越界的小时值历史上真存过，这一问是承重的（与 `get_hourly_stats` 同一道闸）
        if (0..24).contains(&h) {
            hours[h as usize] += c;
        }
    }
    // 每小时图只有一段窗口，用不着按年份分头交回 ⇒ 把年份这一格就地抹掉
    HourlyTotals {
        hours,
        unreadable: gaps.into_iter().map(|(_, s)| s).collect(),
    }
}

/// 查询所选窗口内每小时的合计键数（返回长度 24 的列表）。
///
/// ⚠ 同 [`get_daily_counts`]：长度 24 的数组说不出"这一轮没读全"。面板与导出走
/// [`hourly_window_totals`]。
pub fn get_hourly_totals(days: i64, year: Option<i32>) -> Vec<i64> {
    hourly_window_totals(days, year).hours
}

/// 查询前台应用使用时长公共实现：app_usage 求总秒数 + 按应用聚合。
/// `cond` 为日期条件片段，空串表示全表；三种结局分开（理由同 [`query_stats_in_conn`]）。
fn query_apps_in_conn(
    conn: &rusqlite::Connection,
    cond: &str,
    param: Option<i64>,
) -> YearRead<(i64, HashMap<String, i64>)> {
    match connection::table_presence_cached(conn, "app_usage") {
        connection::Presence::No => return YearRead::NoTable,
        connection::Presence::Unreadable(why) => return YearRead::Unreadable(why),
        connection::Presence::Yes => {}
    }
    let where_clause = if cond.is_empty() {
        String::new()
    } else {
        format!(" WHERE {cond}")
    };
    let summed = match param {
        Some(p) => conn.query_row(
            &format!("SELECT COALESCE(SUM(seconds), 0) FROM app_usage{where_clause}"),
            [p],
            |r| r.get::<_, i64>(0),
        ),
        None => conn.query_row("SELECT COALESCE(SUM(seconds), 0) FROM app_usage", [], |r| {
            r.get::<_, i64>(0)
        }),
    };
    let total: i64 = match summed {
        Ok(v) => v,
        Err(e) => {
            return YearRead::Unreadable(format!("汇总这一窗口的{DOMAIN_APPS}时读不出来: {e}"));
        }
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
                    .and_then(|rows| rows.collect::<Result<Vec<_>, rusqlite::Error>>()),
                None => stmt
                    .query_map([], mapper)
                    .and_then(|rows| rows.collect::<Result<Vec<_>, rusqlite::Error>>()),
            },
            Err(e) => Err(e),
        };
        match rows {
            Ok(list) => list.into_iter().collect(),
            Err(e) => {
                tracing::error!("app_usage 查询失败 ({sql}): {e}");
                return YearRead::Unreadable(format!("应用榜读不出来: {e}"));
            }
        }
    };
    YearRead::Read((total, map))
}

/// 查询前台应用使用时长：返回 (总秒数, {应用名: 秒数})，周期选择与按键统计一致。
///
/// ⚠ 二元组丢掉了"这一轮有没有读全"；面板与导出请走 [`app_stats_totals`]。
pub fn get_app_stats(days: Option<i64>, year: Option<i32>) -> (i64, HashMap<String, i64>) {
    let totals = app_stats_totals(days, year);
    (totals.total, totals.rows)
}

/// 前台应用时长读数（含"这一轮有没有读全"），周期选择与按键统计一致。
pub fn app_stats_totals(days: Option<i64>, year: Option<i32>) -> RankTotals {
    let years = match year {
        Some(y) => vec![y],
        None => query_years(days, None),
    };
    let start_dk = cutoff_day_key(days);
    let cond = if start_dk.is_some() {
        "date_key >= ?1"
    } else {
        ""
    };
    gather_rank_totals(&years, DOMAIN_APPS, cond, start_dk, query_apps_in_conn)
}

/// 某一天的前台应用时长读数。
pub fn app_stats_totals_on(target_date: chrono::NaiveDate) -> RankTotals {
    let dk = day_key_of_date(target_date);
    gather_rank_totals(
        &[target_date.year()],
        DOMAIN_APPS,
        "date_key = ?1",
        Some(dk),
        query_apps_in_conn,
    )
}

/// 查询指定日期前台应用使用时长。
pub fn get_app_stats_by_date(target_date: chrono::NaiveDate) -> (i64, HashMap<String, i64>) {
    let totals = app_stats_totals_on(target_date);
    (totals.total, totals.rows)
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
///
/// 完整形态是 `device-id:<年份>:<库内 id>`。**年份是必须的**：`devices.id` 每个
/// 年度库各自自增、互不相干，裸 id 当身份键的话，2025 库的 3 号设备与 2026 库的
/// 3 号设备（两台无关的设备）会在跨年视图里被并成一台 —— 次数相加、别名互串；
/// 跨年归档的 `INSERT OR IGNORE` + 按 device_key 建 id 映射还会把源库的计数
/// 映到目标库那台不相干的设备上（张冠李戴）。
/// 年份取"这条占位键诞生在哪个库"，随行搬到目标库后不再改写，所以同一台设备
/// 不管停在哪个库里，键都只有一个。
pub(crate) const ARCHIVED_DEVICE_KEY_PREFIX: &str = "device-id:";

/// 占位键的生成处：写入侧（补登记、迁移）用它，SQL 兜底用
/// [`archived_device_key_sql_prefix`]，两者由用例逐字对一次。
pub(crate) fn archived_device_key(year: i32, device_id: i64) -> String {
    format!("{ARCHIVED_DEVICE_KEY_PREFIX}{year}:{device_id}")
}

/// SQL 内联用的占位键前缀片段：`'device-id:2025:' || dc.device_id`。
///
/// `year` 来自我们自己数出来的年度库清单（i32），拼进 SQL 没有注入面。
pub(crate) fn archived_device_key_sql_prefix(year: i32) -> String {
    format!("'{}{year}:'", ARCHIVED_DEVICE_KEY_PREFIX)
}

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
        match id.split_once(':') {
            // 现形 `<年份>:<库内 id>`：年份就是它的来历，裸 id 不是
            Some((year, n)) => format!("未知设备 · {year} 年归档 #{n}"),
            // 旧形：库还没跑过 `ensure_schema` 时文件里仍是裸 id
            None => format!("未知设备 · 归档 #{id}"),
        }
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

/// 展示侧把库里存着的英文通用名换成中文词条（`device_stats::localize_generic_name` 那张表）。
///
/// 那张表过去**只在写侧用**（`display_name` 登记时套一次），于是它有个可预见的失效形态：
/// 词条是后来新增或改写的，旧年度库里的行永远停在写入那一刻的英文串 —— 同一台设备在
/// `2025.focusflow.db` 里是 `HID-compliant mouse · 24AE/1464`、在 `2026` 里是
/// `HID 鼠标 · 24AE/1464`，界面上取哪一条取决于哪个库先被读到（见 [`merge_device_rows`]）。
/// 在读侧过一次词条表就能对所有历史库成立，而且不用迁移任何行。
///
/// 按 ` · ` 分段逐段过表（写侧正是"名字 · VID/PID"这个形状），所以它对已经中文化的名字
/// 是幂等的：表里没有的串一律原样返回。
pub(crate) fn localize_display_name(stored: &str) -> String {
    stored
        .split(" · ")
        .map(crate::device_stats::localize_generic_name)
        .collect::<Vec<_>>()
        .join(" · ")
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

/// 单库设备查询的原始行：(device_key, 登记名, 类型, 次数, 这个库的年份)。
#[derive(Clone)]
struct DeviceRow {
    device_key: String,
    name: Option<String>,
    kind: Option<String>,
    count: i64,
    /// 这一行来自哪一年的库。同一台设备在两个年度库里各有一行时，展示名按
    /// **最新登记年**那条走（见 [`merge_device_rows`]），不能让读序决定界面。
    year: i32,
}

/// 设备查询公共实现：device_counts 聚合 + LEFT JOIN devices 取登记名。
/// `cond` 为 date_key 条件片段，空串表示全表；表不存在（旧库）时返回 None。
///
/// `year` 是**这个库文件的年份**，只用来给「统计行有、登记行没有」那批兜底键起名
/// （见 [`archived_device_key_sql_prefix`]）：`devices.id` 各库各自自增，
/// 不带年份的话两个库里的同号设备会被当成同一台。
fn query_devices_in_conn(
    conn: &Connection,
    cond: &str,
    param: Option<i64>,
    year: i32,
) -> YearRead<Vec<DeviceRow>> {
    match connection::table_presence_cached(conn, "device_counts") {
        connection::Presence::No => return YearRead::NoTable,
        connection::Presence::Unreadable(why) => return YearRead::Unreadable(why),
        connection::Presence::Yes => {}
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
        "SELECT COALESCE(d.device_key, {} || dc.device_id) AS dkey,
                d.name, d.kind, SUM(dc.count) AS cnt
         FROM device_counts dc LEFT JOIN devices d ON d.id = dc.device_id
         {where_clause} GROUP BY dc.device_id",
        archived_device_key_sql_prefix(year)
    );
    let mapper = |r: &rusqlite::Row<'_>| {
        Ok(DeviceRow {
            device_key: r.get(0)?,
            name: r.get::<_, Option<String>>(1)?,
            kind: r.get::<_, Option<String>>(2)?,
            count: r.get(3)?,
            year,
        })
    };
    let rows = match conn.prepare(&sql) {
        Ok(mut stmt) => match param {
            Some(p) => stmt
                .query_map([p], mapper)
                .and_then(|rows| rows.collect::<Result<Vec<_>, rusqlite::Error>>()),
            None => stmt
                .query_map([], mapper)
                .and_then(|rows| rows.collect::<Result<Vec<_>, rusqlite::Error>>()),
        },
        Err(e) => Err(e),
    };
    match rows {
        Ok(list) => YearRead::Read(list),
        Err(e) => {
            tracing::error!("device_counts 查询失败 ({sql}): {e}");
            YearRead::Unreadable(format!("设备榜读不出来: {e}"))
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
    // 认同改按**硬件身份段**（B14-2）：迁移按库做，个别历史库可能因占用没迁成、
    // 行里还是完整实例路径 —— 按原始 key 认会把同一台设备拆成两行；身份段对
    // 两种形态给同一个值，已迁移的键原样返回。
    let mut first_at: HashMap<String, usize> = HashMap::new();
    let mut merged: Vec<DeviceRow> = Vec::with_capacity(rows.len());
    // 与 merged 同序：每台展示行交出去的**规范键**（身份段）。
    // 行键必须是这个，不能是"这个组里第一条库行的原始 device_key"—— 那个形态
    // 取决于哪一年先被读到（`query_years` 在 period=0 走降序、跨年窗口走升序），
    // 于是同一台设备在不同统计周期交出去两种键：别名只认其中一种（换个周期名字
    // 就没了），而改名又按当前周期那种落盘（名字写在哪条键上跟着界面变）。
    let mut identities: Vec<String> = Vec::with_capacity(rows.len());
    for row in rows {
        let identity = crate::device_alias::hardware_identity_key(&row.device_key);
        match first_at.get(&identity).copied() {
            Some(i) => {
                let keep = &mut merged[i];
                keep.count += row.count;
                let newer = row.year >= keep.year;
                if newer {
                    keep.year = row.year;
                }
                // 名字/类型："空的那条永远让位"（归档与迁移会留空名占位行），两边都有值时
                // 按**最新登记年**那条走。后半个判据是新加的：读序随统计周期变
                // （`query_years` 在 period=0 降序、跨年窗口升序），旧实现是"第一条非空即定"，
                // 于是同一台设备换个周期就换个名字 —— 而详情弹窗复用排行的那条行，
                // 两处就会给出不同的名字与副标题。
                let incoming = row.name.as_deref().unwrap_or("").trim();
                let have = keep.name.as_deref().unwrap_or("").trim();
                if !incoming.is_empty() && (have.is_empty() || newer) {
                    keep.name = row.name.clone();
                }
                let incoming_kind = row.kind.as_deref().unwrap_or("").trim();
                let have_kind = keep.kind.as_deref().unwrap_or("").trim();
                if !incoming_kind.is_empty() && (have_kind.is_empty() || newer) {
                    keep.kind = row.kind.clone();
                }
            }
            None => {
                first_at.insert(identity.clone(), merged.len());
                identities.push(identity);
                merged.push(row);
            }
        }
    }
    let total: i64 = merged.iter().map(|r| r.count).sum();
    let mut stats: Vec<DeviceStat> = merged
        .into_iter()
        .zip(identities)
        .map(|(r, identity)| {
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
                .filter(|n| {
                    !n.is_empty()
                        // 占位登记用的名字有两种写法：本行的原始键，和归一之后的
                        // 身份键（`migrate_device_identity_keys` 用后者补占位行）。
                        // 只比原始键的话，历史库里那条"名字=完整实例路径"的占位行
                        // 会因为键已归一而被当成真名端给用户。
                        && *n != r.device_key.as_str()
                        && *n != identity.as_str()
                        // 还有第三种：合并行的名字来自**另一条库行**（同一台设备跨年
                        // 换过口，取的是最新登记年那条的 name），于是它既不等于本行原始键、
                        // 也不等于身份段本身 —— 整串 `\\?\HID#VID_…#{GUID}` 就被当真名端出去。
                        // 所以把名字也按身份键归一一次再比：与认设备同一个尺子，
                        // 三种占位写法一起折成「没名字」，而人起的名字归一后仍是它自己。
                        && crate::device_alias::hardware_identity_key(n) != identity
                })
                // 库里存的英文名在读侧过一次词条表（见 `localize_display_name`）：
                // 词条是后来加的，旧年度库的行停在登记那一刻的写法上。
                .map(localize_display_name)
                .unwrap_or_else(|| fallback_device_name(&identity));
            // 别名优先（用户改过的名字），别名缺失或为空时用自动名。
            // 去重只对自动名生效：两个不同设备可以起同一个别名，用户说了算。
            let alias = aliases.resolve(&identity).map(|s| s.to_string());
            let has_alias = alias.is_some();
            let aliased = alias.unwrap_or_else(|| auto_name.clone());
            DeviceStat {
                key: identity,
                name: aliased,
                auto_name,
                has_alias,
                kind: r.kind.unwrap_or_else(|| "unknown".to_string()),
                count: r.count,
            }
        })
        .collect();
    stats.sort_by(|a, b| crate::format::cmp_rank_desc(&a.name, &a.count, &b.name, &b.count));
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

/// 设备维度的一次读数（总次数 + 展示行）连同**这次有没有读全**。
///
/// 形状照 [`AlltimeMaxDay`]：`unreadable` 非空时 `total`/`devices` 都只是**下界**，
/// 面板不能把一次"有一年库没打开"念成"这一年没插设备"。
#[derive(Debug, Clone, Default)]
pub struct DeviceTotals {
    /// 读到的那些年度库的输入总次数（占比分母，未截断）。
    pub total: i64,
    /// 展示行：跨年已合并、别名优先、按次数降序（未截断）。
    pub devices: Vec<DeviceStat>,
    /// 这一轮没答上来的年度库，每项形如「N 年库设备统计读不出来: 原因」。空 = 读全了。
    pub unreadable: Vec<String>,
}

/// 逐年度库取设备行并合并；读不出来的那一年进 `unreadable`，不按"没设备"计。
fn gather_device_totals(years: &[i32], cond: &str, param: Option<i64>) -> DeviceTotals {
    let mut rows: Vec<DeviceRow> = Vec::new();
    let mut out = DeviceTotals::default();
    for year in years.iter().copied() {
        let gap_of = |why: String| format!("{year} 年库{DOMAIN_DEVICES}读不出来: {why}");
        let path = paths::year_db_path(year);
        match gate_year(year, &path, DOMAIN_DEVICES) {
            YearGate::SkipAbsent => continue,
            YearGate::CannotTell(why) => {
                out.unreadable.push(gap_of(why));
                continue;
            }
            YearGate::Read => {}
        }
        let got = connection::with_ro_conn_why(&path, |conn| {
            query_devices_in_conn(conn, cond, param, year)
        });
        match got {
            Err(why) => out.unreadable.push(gap_of(why)),
            Ok(YearRead::Unreadable(why)) => out.unreadable.push(gap_of(why)),
            // 没有 `device_counts` = 这一年确实还没积累设备数据
            Ok(YearRead::NoTable) => {}
            Ok(YearRead::Read(list)) => rows.extend(list),
        }
    }
    let (total, devices) = merge_device_rows(rows);
    out.total = total;
    out.devices = devices;
    out
}

/// 查询设备维度统计：返回 (总输入次数, 设备行列表)，周期选择与按键统计一致。
///
/// ⚠ 二元组丢掉了"这一轮有没有读全"；面板与导出请走 [`device_totals`]。
pub fn get_device_stats(days: Option<i64>, year: Option<i32>) -> (i64, Vec<DeviceStat>) {
    let totals = device_totals(days, year);
    (totals.total, totals.devices)
}

/// 设备维度读数（含"这一轮有没有读全"）。
pub fn device_totals(days: Option<i64>, year: Option<i32>) -> DeviceTotals {
    let years = match year {
        Some(y) => vec![y],
        None => query_years(days, None),
    };
    let start_dk = cutoff_day_key(days);
    let cond = if start_dk.is_some() {
        "date_key >= ?1"
    } else {
        ""
    };
    gather_device_totals(&years, cond, start_dk)
}

/// 查询指定日期设备维度统计。⚠ 同 [`get_device_stats`]：说不出"没读全"。
pub fn get_device_stats_by_date(target_date: chrono::NaiveDate) -> (i64, Vec<DeviceStat>) {
    let totals = device_totals_on(target_date);
    (totals.total, totals.devices)
}

/// 某一天的设备维度读数。
pub fn device_totals_on(target_date: chrono::NaiveDate) -> DeviceTotals {
    let dk = day_key_of_date(target_date);
    gather_device_totals(&[target_date.year()], "date_key = ?1", Some(dk))
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
    /// 这一轮没答上来的那些问（详情按年份库要问四趟：按天序列、键名明细、
    /// 明细标记、排行）。**非空时上面这一页所有数字都只是下界**，界面必须说出来，
    /// 否则"这一年读不动"会穿成"这台设备最近没怎么用"。空 = 读全了。
    pub unreadable: Vec<String>,
}

/// 这台设备在**本库**里的所有 `devices.id`（按硬件身份段认，见
/// [`crate::device_alias::hardware_identity_key`]）。
///
/// `Err` = 这一问没答案，不是「没有登记行」—— 调用方得把这两件事分开说。
///
/// 为什么不写 `WHERE d.device_key = ?1`：`devices.device_key` 的形态不止一种 ——
/// 归组键迁移（`connection::migrate_device_identity_keys`）按库做，历史库被占用时
/// 只跳过（`maintenance::migrate_all_year_device_keys`），于是同一台设备在旧库里
/// 还是完整实例路径、在新库里是身份段。展示侧早就按身份段把它们并成一行
/// （`merge_device_rows`），而单设备查询按文本等值比 —— 两边口径一岔，就是
/// 「排行那一行 1000 次、点进详情只有 600 次」和「别名切个周期就不显示」。
///
/// 在 Rust 里归一而不是注册 SQL 函数：登记表一个库就几行，读全表的成本可以忽略，
/// 而口径与展示侧用的是同一份实现；rusqlite 的自定义函数还要另开 `functions` feature。
///
/// `db_year` 是本库文件的年份，用来接住**占位键**那台设备：它压根没有登记行
/// （排行靠 `query_devices_in_conn` 的 COALESCE 兜底才出现在界面上），光扫 `devices`
/// 永远扫不到 —— 不接住的话界面就是"排行有次数、点开详情全是 0"。键里就写着它的
/// 库内 id，直接解出来；只认与本库年份相符的那一个，别的库的同编号设备不算它
/// （那正是归档占位键要避免的形态）。
///
/// ⚠ 这一条原先配着一个吞错误的 `Vec` 版包装（把读失败折成空表），唯一的调用方
/// （`registered_kind`）改走三态之后它就没人用了，已删。别再造回那种形状：
/// 折成空表等于把「登记表读不动」念成「这台设备这一年没记录」，界面念的是"用得少了"。
fn device_ids_of_identity_checked(
    conn: &Connection,
    device_key: &str,
    db_year: i32,
) -> Result<Vec<i64>, String> {
    // 传进来的可以是任意形态（`get_device_detail` 已经归一，直接调这些 helper 的
    // 用例与日后别的调用方未必）：归一是幂等的，这里再归一次不改变答案，
    // 却省掉了"helper 只收身份键"这种看不见的约定。
    let identity = crate::device_alias::hardware_identity_key(device_key);
    let mut ids: Vec<i64> = match conn.prepare("SELECT id, device_key FROM devices") {
        Ok(mut stmt) => {
            // 取行不许用 `rows.flatten()`：`Result` 自身是 `IntoIterator`，坏页那一项的
            // `Err` 会被丢掉 ⇒ 这台设备在这个库里的**一部分 id** 悄悄消失，
            // 后面按 id 查明细就少查几行，界面交出的是"这台设备用得少了"。
            match stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
                .and_then(|rows| rows.collect::<Result<Vec<_>, rusqlite::Error>>())
            {
                Ok(list) => list
                    .into_iter()
                    .filter(|(_, key)| crate::device_alias::hardware_identity_key(key) == identity)
                    .map(|(id, _)| id)
                    .collect(),
                Err(e) => return Err(format!("读登记表失败: {e}")),
            }
        }
        Err(e) => return Err(format!("准备登记表的查询失败: {e}")),
    };
    if let Some(rest) = identity.strip_prefix(ARCHIVED_DEVICE_KEY_PREFIX) {
        // 现形 `<年份>:<库内 id>`；旧形是裸 id（那个库连占位登记都没写过），
        // 说的就是"本库的 n 号"
        let (y, n) = match rest.split_once(':') {
            Some((y, n)) => (y.parse::<i32>().ok(), n.parse::<i64>().ok()),
            None => (None, rest.parse::<i64>().ok()),
        };
        let from_this_db = match y {
            Some(y) => y == db_year,
            None => true,
        };
        if let (true, Some(n)) = (from_this_db, n) {
            if !ids.contains(&n) {
                ids.push(n);
            }
        }
    }
    Ok(ids)
}

/// `IN (?,?,…)` 的占位符串：只有 `?n`，值一律走绑定参数。
fn in_placeholders(n: usize) -> String {
    (1..=n)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// 把 id 列表铺成绑定参数（`&[&dyn ToSql]`）。
fn id_params(ids: &[i64]) -> Vec<&dyn rusqlite::ToSql> {
    ids.iter().map(|i| i as &dyn rusqlite::ToSql).collect()
}

/// 「这台设备有没有键名明细」的三种结局。
///
/// 立三态的理由：`false` 会被界面念成一句关于**用户历史**的断言 ——
/// 「键名明细从该功能上线后开始积累，此前的历史数据无法回溯」。
/// 而原先每一条读不动的年份库（移动盘休眠、同步盘占位、库被别的连接握着）都会折进
/// 那句"没有"：某一年打不开 ⇒ 那一年不算数 ⇒ 只要别处也没查到就报"没有"。
/// 保守方向与本文件 `year_has_aggregates` 一致：**读不了就按"有"处理**，
/// 宁可那一格显示成"有明细、这次没列出来"，也不替用户编一段历史。
#[derive(Debug, PartialEq, Eq)]
enum DetailVerdict {
    /// 某一年库里确实查到过一条
    Has,
    /// 每一个能读的库都读完了，都说没有 —— 这才许界面念那句"无法回溯"
    Absent,
    /// 有年份库没答上来，原因都收在字符串里
    Unknown(String),
}

/// 该设备在任何年份库里有没有一条键名明细。
///
/// 与「本周期内有没有」是两件事，混起来会对着设备说谎：点「今日」看一台
/// 上周还在用、今天没按过的鼠标，界面说的是"键名明细从该功能上线后开始积累，
/// 此前的历史数据无法回溯"，而那台鼠标库里其实有几十条明细。
fn device_has_key_detail(identity: &str) -> DetailVerdict {
    let mut unknown: Vec<String> = Vec::new();
    for year in query_years(None, None) {
        let path = paths::year_db_path(year);
        let verdict = connection::with_ro_conn(&path, |conn| {
            match connection::table_presence_cached(conn, "device_key_counts") {
                // 这一年还没有这张表（键名明细是后来才加的功能）：这是**一个答案**
                connection::Presence::No => DetailVerdict::Absent,
                connection::Presence::Unreadable(why) => {
                    DetailVerdict::Unknown(format!("{year} 年：{why}"))
                }
                connection::Presence::Yes => {
                    match device_ids_of_identity_checked(conn, identity, year) {
                        // 这台设备在这一年没登记过：也是一个答案
                        Ok(ids) if ids.is_empty() => DetailVerdict::Absent,
                        Err(why) => DetailVerdict::Unknown(format!("{year} 年：{why}")),
                        Ok(ids) => {
                            let sql = format!(
                                "SELECT 1 FROM device_key_counts k \
                                 WHERE k.device_id IN ({}) LIMIT 1",
                                in_placeholders(ids.len())
                            );
                            let pv = id_params(&ids);
                            // 两处失败原先都折成 `false`（`ok()?` 与 `unwrap_or(false)`）
                            match conn.prepare(&sql).and_then(|mut s| s.exists(pv.as_slice())) {
                                Ok(true) => DetailVerdict::Has,
                                Ok(false) => DetailVerdict::Absent,
                                Err(e) => DetailVerdict::Unknown(format!("{year} 年查不动: {e}")),
                            }
                        }
                    }
                }
            }
        })
        // 库根本打不开：`with_ro_conn` 自己已经记了 error，这里把它记成"没答案"
        .unwrap_or_else(|| DetailVerdict::Unknown(format!("{year} 年的库打不开")));
        match verdict {
            DetailVerdict::Has => return DetailVerdict::Has,
            DetailVerdict::Unknown(why) => unknown.push(why),
            DetailVerdict::Absent => {}
        }
    }
    if unknown.is_empty() {
        DetailVerdict::Absent
    } else {
        DetailVerdict::Unknown(unknown.join("；"))
    }
}

/// 单设备在周期内的键名排行（跨年度库合并，次数降序），连同"这一轮没答上来的那些问"。
///
/// 表不存在（旧库）时安静交回空 —— 键名明细从该功能上线后开始积累，历史无法回溯。
/// ⚠ 但"读不出来"不是"没有"：这一支以前有四道折（`with_ro_conn` 把打不开折成 `None`、
/// `table_exists` 把问不出折成"没这张表"、`prepare(..).ok()` 与 `rows.flatten()` 把坏页
/// 折成"明细就这些"），四道最后都穿成一句"这台设备的键名明细短了一截"。
/// 现在四道都进 `unreadable`，交回的榜只是**已读部分**。
fn device_key_rows(identity: &str, period: i64) -> (Vec<(String, i64)>, Vec<String>) {
    let today_key = day_key_of_date(Local::now().date_naive());
    let start_key = match period {
        -1 => Some(today_key),
        0 => None,
        // 周期起点统一走 `period_start_key`：同一个公式只留一份实现，
        // 而且它夹到 `MAX_QUERY_DAYS`（就地写 `n.max(1) - 1` 那份不夹）。
        // 说清差别在哪：`today_key - (i64::MAX - 1)` **不会**下溢（结果约 −9.22e18，
        // 仍在 i64 范围内，注回旧写法实测没 panic），但它等于"没有任何下界"
        // ⇒ 极端周期从"近 100 年"变成"全表"，与 `period_start_key` 那一路不同口径。
        n => Some(period_start_key(today_key, n)),
    };
    let mut merged: HashMap<String, i64> = HashMap::new();
    let mut gaps: Vec<String> = Vec::new();
    for year in query_years(None, None) {
        let path = paths::year_db_path(year);
        let gap_of = |why: String| format!("{year} 年库键名明细读不出来: {why}");
        let got = connection::with_ro_conn_why(&path, |conn| -> YearRead<Vec<(String, i64)>> {
            match connection::table_presence_cached(conn, "device_key_counts") {
                connection::Presence::No => return YearRead::NoTable,
                connection::Presence::Unreadable(why) => return YearRead::Unreadable(why),
                connection::Presence::Yes => {}
            }
            // 这台设备在本库的 id 问不出 ⇒ 这一年的明细一条都没核对过，不能算"没有"
            let ids = match device_ids_of_identity_checked(conn, identity, year) {
                Ok(ids) => ids,
                Err(why) => return YearRead::Unreadable(why),
            };
            if ids.is_empty() {
                return YearRead::Read(Vec::new());
            }
            let ph = in_placeholders(ids.len());
            // GROUP BY key_name 在这台设备的**所有 id** 上做：同一台设备在同库里
            // 可能占两行（两种键形态的历史登记），明细必须加到一起。
            let (sql, tail) = match start_key {
                Some(sk) => (
                    format!(
                        "SELECT k.key_name, SUM(k.count) FROM device_key_counts k \
                         WHERE k.device_id IN ({ph}) AND k.date_key >= ?{} \
                         GROUP BY k.key_name",
                        ids.len() + 1
                    ),
                    Some(sk),
                ),
                None => (
                    format!(
                        "SELECT k.key_name, SUM(k.count) FROM device_key_counts k \
                         WHERE k.device_id IN ({ph}) GROUP BY k.key_name"
                    ),
                    None,
                ),
            };
            let mut pv = id_params(&ids);
            if let Some(ref sk) = tail {
                pv.push(sk as &dyn rusqlite::ToSql);
            }
            let rows = match conn.prepare(&sql) {
                Ok(mut stmt) => stmt
                    .query_map(pv.as_slice(), |r| Ok((r.get(0)?, r.get(1)?)))
                    .and_then(|rows| rows.collect::<Result<Vec<_>, rusqlite::Error>>()),
                Err(e) => Err(e),
            };
            match rows {
                Ok(list) => YearRead::Read(list),
                Err(e) => {
                    tracing::error!("device_key_counts 查询失败（{year} 年库，{sql}）: {e}");
                    YearRead::Unreadable(format!("键名明细读不出来: {e}"))
                }
            }
        });
        match got {
            Err(why) => gaps.push(gap_of(why)),
            Ok(YearRead::Unreadable(why)) => gaps.push(gap_of(why)),
            // 没有这张表 = 这一年还没开始记键名明细；安静按空计
            Ok(YearRead::NoTable) => {}
            Ok(YearRead::Read(list)) => {
                for (name, c) in list {
                    *merged.entry(name).or_insert(0) += c;
                }
            }
        }
    }
    let mut list: Vec<(String, i64)> = merged.into_iter().collect();
    list.sort_by(|a, b| crate::format::cmp_rank_desc(&a.0, &a.1, &b.0, &b.1));
    (list, gaps)
}

/// 单设备按天次数序列（跨年度库合并，date_key 升序），连同"这一轮没答上来的那些问"。
///
/// 设备维度按年份分库存储，查询必须跨库合并 —— 与 `get_device_stats` 同理。
/// 四道折的收口理由同 [`device_key_rows`]：这一栏读不全时界面念的是
/// "活跃 N 天 / 首次与最近使用日期"，那都是关于**用户历史**的断言。
fn device_date_series(identity: &str) -> (Vec<(i64, i64)>, Vec<String>) {
    let mut merged: HashMap<i64, i64> = HashMap::new();
    let mut gaps: Vec<String> = Vec::new();
    for year in query_years(None, None) {
        let path = paths::year_db_path(year);
        let gap_of = |why: String| format!("{year} 年库按天序列读不出来: {why}");
        let got = connection::with_ro_conn_why(&path, |conn| -> YearRead<Vec<(i64, i64)>> {
            match connection::table_presence_cached(conn, "device_counts") {
                connection::Presence::No => return YearRead::NoTable,
                connection::Presence::Unreadable(why) => return YearRead::Unreadable(why),
                connection::Presence::Yes => {}
            }
            let ids = match device_ids_of_identity_checked(conn, identity, year) {
                Ok(ids) => ids,
                Err(why) => return YearRead::Unreadable(why),
            };
            if ids.is_empty() {
                return YearRead::Read(Vec::new());
            }
            let sql = format!(
                "SELECT c.date_key, c.count FROM device_counts c \
                 WHERE c.device_id IN ({})",
                in_placeholders(ids.len())
            );
            let pv = id_params(&ids);
            let rows = match conn.prepare(&sql) {
                Ok(mut stmt) => stmt
                    .query_map(pv.as_slice(), |r| Ok((r.get(0)?, r.get(1)?)))
                    .and_then(|rows| rows.collect::<Result<Vec<_>, rusqlite::Error>>()),
                Err(e) => Err(e),
            };
            match rows {
                Ok(list) => YearRead::Read(list),
                Err(e) => {
                    tracing::error!("device_counts 查询失败（{year} 年库，{sql}）: {e}");
                    YearRead::Unreadable(format!("按天序列读不出来: {e}"))
                }
            }
        });
        match got {
            Err(why) => gaps.push(gap_of(why)),
            Ok(YearRead::Unreadable(why)) => gaps.push(gap_of(why)),
            Ok(YearRead::NoTable) => {}
            Ok(YearRead::Read(list)) => {
                for (dk, c) in list {
                    *merged.entry(dk).or_insert(0) += c;
                }
            }
        }
    }
    let mut series: Vec<(i64, i64)> = merged.into_iter().collect();
    series.sort_by_key(|(dk, _)| *dk);
    (series, gaps)
}

/// 查询单台设备详情。`period` 与统计视图一致：-1 今日 / 0 全部 / n 近 n 天。
///
/// 周期起点单独成一个函数：`period` 是外部输入（前端、CLI 都传），而
/// `today_key - (n - 1)` 在 `n = i64::MAX` 时下溢 —— debug 直接 panic、
/// release（`panic = "abort"`）连进程一起没，夹不住就别让它参与运算。
fn period_start_key(today_key: i64, period: i64) -> i64 {
    match period {
        -1 => today_key,
        0 => i64::MIN,
        n => today_key - (n.clamp(1, MAX_QUERY_DAYS) - 1),
    }
}

pub fn get_device_detail(device_key: &str, period: i64) -> DeviceDetail {
    // 交进来的键先归一成身份段。排行行的 key 已经是身份形态，但这是个外部入口
    // （界面快照、CLI、手改的别名文件都可能递来历史形态的完整实例路径），
    // 不归一的话单设备查询与下面那句 `s.key == device_key` 都认不出同一台设备 ——
    // 表现就是「点进去数字比排行那一行小」和「弹窗里没名字」。
    let device_key = crate::device_alias::hardware_identity_key(device_key);
    let today_date = Local::now().date_naive();
    let today_key = day_key_of_date(today_date);
    let (series, mut gaps) = device_date_series(&device_key);

    let from_key = period_start_key(today_key, period);
    let in_period = |dk: i64| dk >= from_key;
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
            let auto = fallback_device_name(&device_key);
            let alias = crate::device_alias::table()
                .resolve(&device_key)
                .map(|s| s.to_string());
            let (registered, kind_gaps) = registered_kind(&device_key);
            // 登记表那一问问不出的一句也进这一轮那份账：类型那格回退成 unknown 是显示层的
            // 事，但"哪几问没答上来"这一栏要如实带上它，不然这一格看着与"这台设备没登记"同形
            gaps.extend(kind_gaps);
            (
                alias.clone().unwrap_or_else(|| auto.clone()),
                auto,
                alias.is_some(),
                registered.unwrap_or_else(|| "unknown".to_string()),
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
    let (key_rows, key_gaps) = device_key_rows(&device_key, period);
    gaps.extend(key_gaps);
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

    // 只有"每一个能读的库都说没有"才报 false：那一格界面文案讲的是用户的历史，
    // 拿一次读不动当证据就等于替用户编一段历史。读不动时按 true 走
    // （那一格会显示成"有明细、这次没列出来"），原因除了留日志还进 `unreadable`，
    // 让界面把它说给人听。
    let detail_verdict = device_has_key_detail(&device_key);
    if let DetailVerdict::Unknown(why) = &detail_verdict {
        tracing::warn!("设备 {device_key} 有没有键名明细查不出来（{why}）：本次不断言「没有」");
        gaps.push(format!("这台设备有没有键名明细问不出来: {why}"));
    }
    DeviceDetail {
        key: device_key.clone(),
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
        has_key_detail: !matches!(detail_verdict, DetailVerdict::Absent),
        unreadable: gaps,
    }
}

/// 读取设备登记表里的类型（用于周期内无数据、无法从排行取到类型的情况）。
///
/// 交回 `(类型, 这一轮没问出来的那几问)`。按身份段找（[`device_ids_of_identity_checked`]）：
/// 同一台设备在一个库里可能占着两种键形态的历史登记，随便取一条的类型即可。
///
/// 「那一格写 unknown」是显示层的决定，但**某一年问不出**不许再被折成「那一年没登记这台
/// 设备」而一个字都不留。这一支原先有四道各自为政的折：`with_ro_conn` 把打不开折成 `None`、
/// bool 的 `table_exists` 把问不出折成「没这张表」、`device_ids_of_identity` 把登记表读不动
/// 折成「这台设备在这个库没有 id」、`query_row(..).ok()` 把那一行读不动折成「没有类型」。
/// 后果只到"类型那格写着未知"（名称、次数、别名都不受影响，也不写任何东西），
/// 所以它排在同族最后一名 —— 但**判低不等于可以继续静默**：原因并进调用方
/// `get_device_detail` 已有的那份 `unreadable`，详情页那句"这一轮没读全"跟着念出来。
fn registered_kind(identity: &str) -> (Option<String>, Vec<String>) {
    let mut gaps: Vec<String> = Vec::new();
    for year in query_years(None, None) {
        let path = paths::year_db_path(year);
        let got = connection::with_ro_conn_why(&path, |conn| -> YearRead<String> {
            match connection::table_presence_cached(conn, "devices") {
                // 没有这张表＝这个库压根没登记过设备：安静（那是答案，不是问不出）
                connection::Presence::No => return YearRead::NoTable,
                connection::Presence::Unreadable(why) => return YearRead::Unreadable(why),
                connection::Presence::Yes => {}
            }
            let ids = match device_ids_of_identity_checked(conn, identity, year) {
                Err(why) => return YearRead::Unreadable(why),
                Ok(list) => list,
            };
            let Some(first) = ids.first().copied() else {
                // 这台设备在这一年没登记过：同样是答案，安静
                return YearRead::NoTable;
            };
            match conn.query_row("SELECT kind FROM devices WHERE id = ?1", [first], |r| {
                r.get::<_, String>(0)
            }) {
                Ok(kind) => YearRead::Read(kind),
                Err(rusqlite::Error::QueryReturnedNoRows) => YearRead::NoTable,
                Err(e) => YearRead::Unreadable(format!("读那一行登记的类型时读不出来: {e}")),
            }
        });
        match got {
            Err(why) => {
                gaps.push(format!("{year} 年库设备登记表连打开都办不到: {why}"));
            }
            Ok(YearRead::Unreadable(why)) => {
                gaps.push(format!("{year} 年库设备登记表读不出来: {why}"));
            }
            Ok(YearRead::NoTable) => {}
            Ok(YearRead::Read(kind)) => {
                // 拿到一条就够（同一台设备在多个库里各有一条登记，类型取先问到的那一条）；
                // 前面那些年没问出来的那几句照样要说出去
                return (Some(kind), gaps);
            }
        }
    }
    (None, gaps)
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
    /// 设备详情的周期起点：`period` 是外部输入，极端值不许把 `today_key - (n-1)` 算崩。
    /// 修前是就地 `today_key - (n.max(1) - 1)`，`n = i64::MAX` 时下溢
    /// （debug 直接 panic，release abort）→ 这里必须只落进 `1..=MAX_QUERY_DAYS`。
    #[test]
    fn device_detail_period_cannot_underflow() {
        assert_eq!(period_start_key(20_720, -1), 20_720, "-1 = 今日");
        assert_eq!(
            period_start_key(20_720, 0),
            i64::MIN,
            "0 = 总计，下界不能筛掉任何一天"
        );
        assert_eq!(period_start_key(20_720, 7), 20_714);
        assert_eq!(period_start_key(20_720, 1), 20_720);
        assert_eq!(
            period_start_key(20_720, i64::MAX),
            period_start_key(20_720, MAX_QUERY_DAYS),
            "i64::MAX 要夹到 MAX_QUERY_DAYS，而不是回绕"
        );
        assert_eq!(period_start_key(20_720, i64::MIN), 20_720);
        assert_eq!(period_start_key(20_720, -100), 20_720, "非法负值退回今日");
    }
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
        // 往 1970 之前同样要往返。旧实现里 `day_key_to_date` 只对非负序号工作
        // （`u64::try_from(...).ok()?`），负序号一律 `None` ⇒ 归档算不出目标年份、
        // 按日视图丢掉那一行，而总计照算（两本账）。
        let eve = chrono::NaiveDate::from_ymd_opt(1969, 12, 31).expect("date");
        for i in 0..4000u64 {
            let d = eve.checked_sub_days(chrono::Days::new(i)).expect("date");
            let k = day_key_of_date(d);
            assert!(k < 0, "{d} 的序号应当是负数，实得 {k}");
            assert_eq!(day_key_to_date(k), Some(d), "{d} -> {k} -> 反算不一致");
        }
    }

    /// 换算的两端都得是不溢出的纯函数 —— release 配置是 `panic = "abort"`，
    /// 一个在 `i64::MIN` 上写错的 `-x` 就是整个应用消失。
    #[test]
    fn day_key_to_date_handles_the_extremes_without_panicking() {
        assert_eq!(
            day_key_to_date(-1),
            chrono::NaiveDate::from_ymd_opt(1969, 12, 31),
            "历元前一天要解得开"
        );
        assert_eq!(
            day_key_to_date(-100_000_000),
            None,
            "约公元前 27 万年，超出范围"
        );
        assert_eq!(day_key_to_date(i64::MIN), None, "不能因为取反而溢出");
        assert_eq!(day_key_to_date(i64::MAX), None);
    }

    /// `hour_of_ts` 在 1970 之前也必须落在 0..=23。
    ///
    /// `div_euclid(3600)` 对负瞬时给出负商，而 Rust 的 `%` 跟着被除数的符号走 ⇒
    /// 旧写法算出 -1..-23 这种「小时」：它进得了 `(date_key, hour)` 主键、却永远
    /// 落在任何 0..=23 的读侧窗口之外（又是总数算它、小时图看不见那一族）。
    #[test]
    fn hour_of_ts_stays_in_range_before_the_epoch() {
        let day_start = |dk: i64| dk * 86_400 - local_utc_offset_seconds();
        for dk in [-4000i64, -2, -1, 0, 1, 20727] {
            for h in 0..24i64 {
                let got = hour_of_ts(day_start(dk) + h * 3600);
                assert_eq!(got, h, "date_key={dk} 的第 {h} 小时，实得 {got}");
            }
        }
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

    /// 「这一年的库在不在」必须能说出"我不知道"。
    ///
    /// `Path::exists()` 把任何一次 stat 失败都答成"没有"，而 `get_daily_counts` 对"没有"
    /// 的做法是安静地跳过那一年 —— 于是休眠的移动盘、被同步盘换成普通文件的 `data`、
    /// 杀软首扫，全都被读成「那几年我没打字」，日志零字，而同一页上「今日」卡片还是真数。
    /// 这个 helper 按路径问（不收 `&self`），所以三态都能直接钉，不用碰进程全局。
    #[test]
    fn the_year_db_presence_question_can_also_say_it_cannot_tell() {
        let _lock = crate::paths::test_app_dir_lock();
        let tmp = crate::paths::test_app_dir("year_presence");
        let present = tmp.path().join("focusflow_2026.db");
        std::fs::write(&present, b"whatever").expect("写一个存在的库文件");
        let absent = tmp.path().join("focusflow_1999.db");

        assert!(year_db_presence(&present).unwrap(), "在就是在");
        assert!(
            !year_db_presence(&absent).unwrap(),
            "确认没有（NotFound）才是「那一年没有库」"
        );
        // 目录也算"在"：它会在后面真开一次连接时失败，那条路上有自己的日志
        // （`每日计数查询失败`），不该在这里被折成"没有"。
        let dir = tmp.path().join("focusflow_2027.db");
        std::fs::create_dir_all(&dir).expect("把某年的库位置做成目录");
        assert!(year_db_presence(&dir).unwrap(), "是个目录也不能说成没有");

        // 实测那一支：**`data` 被换成普通文件时，这台 Windows 的 stat 报的是 NotFound**，
        // 所以它落在「确认没有」而不是「说不出」。上一版用例假设它报"不是目录"，跑出来是
        // `Ok(false)` —— 判据要按量出来的写。（上游 `paths::ensure_dir` 在那个位置建不成
        // 目录时另有它自己的 error 日志，见 bc86445 那轮。）
        let plain = tmp.path().join("focusflow_2028.db");
        std::fs::write(&plain, b"not a folder").unwrap();
        let nested = plain.join("focusflow_2029.db");
        assert!(
            !year_db_presence(&nested).unwrap(),
            "父是普通文件在本机的读法是 NotFound ⇒ 按「没有」处理"
        );

        // 判据表：哪些码算"确认没有"、哪些算"说不出"。
        // ⚠ 这一臂**不再是眼验账**：`an_unanswerable_presence_question_is_not_read_as_no_database`
        // 用自指目录 junction 让真实的 stat 失败（os error 1921）走满整条链。这里按码钉的是
        // **映射规则**（哪些码不许折成"没有"），那条用例钉的是**真实系统调用下这一臂走得到**
        // ——两件事，谁也不替代谁，所以两条都留着。
        // （上一版这里写着"无头造不出真实的 stat 失败，要 ACL 拒绝或杀软握着句柄"：前一句被
        // 现量推翻——`mklink /J` 不要管理员，而这一臂要的只是 stat 回来的码不是 2/3。）
        for code in [2, 3] {
            // ERROR_FILE_NOT_FOUND / ERROR_PATH_NOT_FOUND
            assert!(
                matches!(
                    classify_stat(Err(std::io::Error::from_raw_os_error(code))),
                    Ok(false)
                ),
                "错误码 {code} 该算「确认没有」：全新安装与那年没数据都是这个形状"
            );
        }
        for code in [5, 32, 267] {
            // ERROR_ACCESS_DENIED / ERROR_SHARING_VIOLATION / ERROR_DIRECTORY
            let got = classify_stat(Err(std::io::Error::from_raw_os_error(code)));
            let Err(e) = got else {
                panic!("错误码 {code} 不许被折成「没有」，实得 {got:?}");
            };
            assert_ne!(
                e.kind(),
                std::io::ErrorKind::NotFound,
                "码 {code} 要落在「说不出」这一支"
            );
        }

        // 形状腿：那道"在不在"之问里 `exists()` 尺子不许回来，三态也不许再折成 bool。
        // 锚点第二次搬家：这轮把四处读数各自问的那一遍并成了共用的 `gate_year`
        // （榜／设备／时间窗口／当日小时）。判据留在调用方就只剩"它调了谁"，
        // 抽掉的那半边反而没人守 —— 探针＝把 `Ok(false) => SkipAbsent` 那一臂删掉，
        // 这条与 `a_missing_year_db_is_an_answer_not_a_refusal` 一起红。
        let src = include_str!("queries.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let at = prod.find("fn gate_year(").expect("gate_year 找不到了");
        let raw = &prod[at..at + prod[at..].find("\n}").expect("函数体切不出来")];
        // 先摘注释行：这一段自己的说明文字里就写着 `path.exists()`，带着它判"不许再用"
        // 会红在该改的地方之外、绿在真正要防的地方之上。
        let body: String = raw
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !body.contains(".exists()"),
            "又用 exists() 猜在不在 —— 那次失败会被读成「那一年没有库」"
        );
        assert!(
            body.contains("year_db_presence(path)"),
            "这一处必须问三态那把尺子"
        );
        assert!(
            body.contains("Ok(false) => YearGate::SkipAbsent"),
            "「确认没有」必须是第三条路，不能与「说不出」并成同一句读不出来"
        );
        assert!(
            body.contains("不进{label}的读数"),
            "说不出在不在那一支必须出声，而不是安静跳过"
        );
        // 四个调用方都要经过这道闸：谁绕过它自己开库，"没有那一年库"就又会被说成"读不出来"
        for head in [
            "fn gather_rank_totals(",
            "fn gather_device_totals(",
            "fn read_window_years(",
            "pub fn hourly_stats_totals(",
        ] {
            let w_at = prod.find(head).unwrap_or_else(|| panic!("{head} 找不到了"));
            let w_raw = &prod[w_at..w_at + prod[w_at..].find("\n}").expect("函数体切不出来")];
            let w_body: String = w_raw
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                w_body.contains("gate_year("),
                "{head} 绕过了那道三态闸 —— 「没有库」会在这一支被念成「读不出来」"
            );
            assert!(
                !w_body.contains("year_db_presence("),
                "{head} 自己又问了一遍在不在：判据要从共用那一份走，别在这里分叉"
            );
        }
        // 三个二元组口子必须是薄包装：它们自己再去读一遍库的话，上面那套判据就管不到它们了
        for head in [
            "pub fn get_daily_counts(",
            "pub fn get_hourly_totals(",
            "pub fn get_hourly_stats(",
        ] {
            let w_at = prod.find(head).unwrap_or_else(|| panic!("{head} 找不到了"));
            let w_raw = &prod[w_at..w_at + prod[w_at..].find("\n}").expect("包装函数体切不出来")];
            let w_body: String = w_raw
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                !w_body.contains("with_ro_conn"),
                "{head} 自己又开了一次库 —— gap 会从这条包装里漏掉"
            );
        }
        // 共用一把尺子之后，闸那一头的口径不许被顺手改掉（本轮只是把两处并成一份判据，
        // 没有改变"确认在/说不出都去真开一次"这件事）
        assert!(
            prod.contains("Ok(true) | Err(_) => connection::open_ro(&path).is_err()"),
            "那道闸的口径变了要显式改这条判据，别顺手把「说不出」当成打不开"
        );
    }

    /// 打卡条那道可读性闸只该拦「这一窗口真会读到的那一年」。
    ///
    /// 两半都有真实后果。漏拦：窗口里坏一年会被 `get_daily_counts` 静默跳过（只记一行日志），
    /// 那一年每天都是 0，于是「连续打卡 N 天」悄悄缩到跨年之后，横条照旧是绿色。
    /// 多拦：一条每 60 秒自动刷新的横条，因为一个它压根没读的旧年份坏了而整块变红。
    /// 所以这道闸不能直接套用"全部年度库都问一遍"那一条。
    #[test]
    fn the_daily_counts_gate_covers_exactly_the_years_it_reads() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("gate_window");
        invalidate_years_cache();
        let this = Local::now().year();
        let prev = this - 1;
        let far = this - 5;
        // 窗口里的两年：都建库，都真有聚合行（不然会被当成空壳从年份列表里退掉）
        for y in [this, prev] {
            let conn = connection::open_rw(&paths::year_db_path(y)).unwrap();
            connection::ensure_schema(&conn, y).unwrap();
            conn.execute(
                "INSERT INTO daily_counts (date_key, count, seconds) VALUES (1, 1, 1)",
                [],
            )
            .unwrap();
        }
        invalidate_years_cache();
        connection::clear_ro_cache();
        assert!(
            check_daily_counts_readable(370, None).is_ok(),
            "两个库都读得动时不该拦"
        );

        // 窗口**外**的旧年份坏了：全库那一条要拒绝，这一条必须放行 —— 这两半放在一起
        // 才说明这道闸不是"抄一份全库口径"
        std::fs::write(paths::year_db_path(far), b"not a database at all").unwrap();
        invalidate_years_cache();
        connection::clear_ro_cache();
        assert!(
            check_years_readable().is_err(),
            "对照：全库口径确实把这一年算成坏了"
        );
        assert!(
            check_daily_counts_readable(370, None).is_ok(),
            "这一年不在这个窗口里，不该让打卡条整块变红"
        );

        // 窗口**里**的那一年坏了：必须拒绝并点名是哪一年
        std::fs::write(paths::year_db_path(prev), b"not a database at all").unwrap();
        invalidate_years_cache();
        connection::clear_ro_cache();
        let msg = check_daily_counts_readable(370, None)
            .expect_err("窗口里坏一年时必须拒绝，否则那一年会被静默读成每天 0")
            .to_string();
        assert!(
            msg.contains(&prev.to_string()) && msg.contains("打不开"),
            "要说清是哪一年读不出来，实得: {msg}"
        );

        // 反向腿：把它修回来，同一个调用就必须通过（这条判据不是恒真）
        std::fs::remove_file(paths::year_db_path(prev)).unwrap();
        let conn = connection::open_rw(&paths::year_db_path(prev)).unwrap();
        connection::ensure_schema(&conn, prev).unwrap();
        conn.execute(
            "INSERT INTO daily_counts (date_key, count, seconds) VALUES (1, 1, 1)",
            [],
        )
        .unwrap();
        drop(conn);
        invalidate_years_cache();
        connection::clear_ro_cache();
        assert!(
            check_daily_counts_readable(370, None).is_ok(),
            "坏文件移走之后必须放行"
        );

        // 首启那一头：一个库都没有（年份清单会退回"今年"，而今年还没建库）——
        // "文件不存在"是"没有数据"，不是"读不出来"，这道闸不许把全新安装拦掉。
        for y in [this, prev, far] {
            let _ = std::fs::remove_file(paths::year_db_path(y));
        }
        invalidate_years_cache();
        connection::clear_ro_cache();
        assert!(
            check_daily_counts_readable(370, None).is_ok(),
            "没有任何年度库的首启必须放行（读出来就是全零）"
        );
        assert!(
            get_daily_counts(370, None).iter().all(|(_, c)| *c == 0),
            "同一批读数交回全零，而不是错误"
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
        // 现形：带来源年份
        assert_eq!(
            fallback_device_name("device-id:2025:3"),
            "未知设备 · 2025 年归档 #3",
            "归档占位要说明来历，不能显示机器串"
        );
        // 旧形：库还没跑过 ensure_schema 时文件里仍是裸 id，也得能显示
        assert_eq!(fallback_device_name("device-id:3"), "未知设备 · 归档 #3");
        assert_eq!(
            fallback_device_name("device-id:127"),
            "未知设备 · 归档 #127"
        );
        // 前缀必须一致：改了常量而忘记改生成侧会让这条失效
        assert_eq!(ARCHIVED_DEVICE_KEY_PREFIX, "device-id:");
        // 生成侧有两份实现（Rust 与 SQL 内联），飘开了同一台设备就会分成两行
        assert_eq!(
            archived_device_key(2025, 3),
            "device-id:2025:3",
            "Rust 侧的拼法"
        );
        assert_eq!(
            archived_device_key_sql_prefix(2025),
            "'device-id:2025:'",
            "SQL 侧的内联前缀"
        );
        // SQL 拼出来的串必须能被 Rust 侧同一个解析认出来（展示与归组同一份真相）
        let from_sql = format!(
            "{}{}",
            archived_device_key_sql_prefix(2026).trim_matches('\''),
            7
        );
        assert_eq!(from_sql, archived_device_key(2026, 7));
        assert!(fallback_device_name(&from_sql).contains("2026 年归档 #7"));
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
            // devA/devD：展示名相同的两只（登记名相同 → 去重加序号）。
            // B14-2 后同库键都是身份形态；「两只同型号」在真机上会并成一台
            //（见 hardware_identity_key 的硬取舍），所以这里用不同 VID 制造
            // 同名异机，去重路径的覆盖不变。
            ins_dev(
                "VID_046D&PID_C52B",
                "HID-compliant mouse · 046D/C52B",
                "mouse",
            );
            ins_dev(
                "VID_046D&PID_C52C",
                "HID-compliant mouse · 046D/C52B",
                "mouse",
            );
            // devB：登记名为空 → 走 VID/PID 回退
            ins_dev("VID_046D&PID_C52D", "", "mouse");
            // devC（键盘）：只有统计行、登记名为空 → 回退 + kind=unknown
            ins_cnt("VID_046D&PID_C52D", 70);
            ins_cnt("VID_1B1C&PID_1B2D", 50);
            // devA 也要有统计行：先登记后补一行计数
            ins_cnt("VID_046D&PID_C52B", 30);
            ins_cnt("VID_046D&PID_C52C", 10);
        }

        let (total, stats) = get_device_stats_by_date(Local::now().date_naive());
        assert_eq!(total, 160);
        assert_eq!(stats.len(), 4, "设备按身份键独立成行");
        // 降序：70 / 50 / 30 / 10
        assert_eq!(
            stats[0].name, "HID 设备 · 046D/C52D",
            "空登记名回退 VID/PID"
        );
        assert_eq!(stats[0].name, stats[0].auto_name, "无别名时展示名 = 自动名");
        assert_eq!(
            stats[0].key, "VID_046D&PID_C52D",
            "行内必须带 device_key（UI 改名回写要用）"
        );
        assert_eq!(stats[0].count, 70);
        assert_eq!(stats[1].name, "HID 设备 · 1B1C/1B2D", "未登记设备同样回退");
        assert_eq!(stats[1].kind, "unknown", "未登记设备的类型为 unknown");
        assert_eq!(stats[1].count, 50);
        assert_eq!(
            stats[2].name, "HID 鼠标 · 046D/C52B",
            "库里存的英文名在读侧换成词条（写入侧没动，历史库不迁移）"
        );
        assert_eq!(stats[2].count, 30);
        assert_eq!(stats[2].kind, "mouse");
        assert_eq!(
            stats[3].name, "HID 鼠标 · 046D/C52B (2)",
            "同显示名去重：换算是幂等的，两只才仍然算同名"
        );
        assert_eq!(stats[3].count, 10);
    }

    /// 跨年查询里同一台设备必须是一行（1 月看「最近 30 天」就会走到这条路）。
    ///
    /// `devices.id` 各年度库自增、互不相干，两个库里的同一台设备只有 device_key
    /// 认得出来。逐年 append 而不按 key 并起来时，设备页每个设备显示两行、
    /// 各占一半次数，第二行还被同名去重加上 "(2)"，看着就像多了一只鼠标。
    /// B14-2 后同库按身份键存：旧库若因占用没迁成，行里还是完整路径 ——
    /// 归组按身份段认（`merge_device_rows`），两种形态照样并成一行。
    #[test]
    fn device_stats_collapse_the_same_device_across_year_dbs() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("devq_year");

        let this_year = Local::now().year();
        let prev_year = this_year - 1;
        // 旧库是升级前的完整路径键、新库是身份键：同一台设备的两种形态
        let prev_key = "HID#VID_046D&PID_C52B&MI_00#7&1f126e19&0&0000";
        let this_key = "VID_046D&PID_C52B&MI_00";
        // 两个库各登记一台设备，让同一台设备在两侧的自增 id 真的错开
        for (year, n, other, key) in [
            (prev_year, 40, "HID#VID_1B1C&PID_1B2D#old", prev_key),
            (this_year, 60, "", this_key),
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
            .find(|s| s.key == prev_key || s.key == this_key)
            .expect("同一台设备必须只有一行（旧路径键与新身份键都行）");
        assert_eq!(same.count, 100, "两个年度库的次数必须并起来");
        assert!(
            !same.name.ends_with("(2)"),
            "跨年重复行不该被同名去重顶成两只：{}",
            same.name
        );
        // 按单年查询不受影响
        assert_eq!(get_device_stats(None, Some(this_year)).0, 60);
        assert_eq!(get_device_stats(None, Some(prev_year)).0, 45);

        // ===== 行键必须是身份段，不能是"这个组里第一条库行的原始键" =====
        // 读库顺序在两条路上是**反的**：period=0 走 `available_years()`（降序，
        // 今年的身份键先进来），跨年窗口走 `start.year()..=now.year()`（升序，
        // 去年的完整路径先进来）。行键跟着第一条走的话，同一台设备在两个统计
        // 周期里交出两种键 —— 别名只认一种，改名又按当前那种落盘。
        assert_eq!(same.key, this_key, "总计视图的行键");
        let window = get_device_stats(Some(600), None)
            .1
            .into_iter()
            .find(|s| s.key == this_key || s.key == prev_key)
            .expect("跨年窗口里这台设备必须还是那一行");
        assert_eq!(
            window.key, this_key,
            "跨年窗口的行键也得是身份段（升序读库时它是后到的那一条）"
        );
        assert_eq!(window.count, 100, "窗口里并起来的次数与总计同口径");

        // 一次改名，两个视图都要认（这就是"改完名切个周期名字没了"的那条腿）
        crate::device_alias::invalidate_cache();
        crate::device_alias::set(&window.key, "跨年也认的名字").unwrap();
        let (t_total, t_stats) = get_device_stats(None, None);
        assert_eq!(t_total, 105);
        assert_eq!(
            t_stats
                .iter()
                .find(|s| s.key == this_key)
                .expect("总计视图找得到这台设备")
                .name,
            "跨年也认的名字"
        );
        assert_eq!(
            get_device_stats(Some(600), None)
                .1
                .iter()
                .find(|s| s.key == this_key)
                .expect("跨年视图找得到这台设备")
                .name,
            "跨年也认的名字",
            "别名不能只认写入时那一种键形态"
        );

        // 详情必须给整台设备的数：单设备查询原先按 `d.device_key = ?1` 文本等值比，
        // 于是「排行那一行 100 次、点进详情只有 60 次」（另一半库在历史形态的键上）。
        let detail = get_device_detail(this_key, 0);
        assert_eq!(detail.all, 100, "详情的总计要等于排行那一行");
        assert_eq!(detail.period_count, 100);
        assert_eq!(detail.active_days, 2, "两个库各一天 = 活跃两天");
        assert_eq!(detail.name, "跨年也认的名字");
        // 拿历史形态的键进来（旧界面快照、CLI、手改的别名文件）也得是同一台设备
        let from_legacy = get_device_detail(prev_key, 0);
        assert_eq!(from_legacy.all, 100, "用旧形态键点详情也得拿到全部次数");
        assert_eq!(from_legacy.key, this_key, "详情回写的键必须是身份段");
        crate::device_alias::clear(this_key).unwrap();
        crate::device_alias::invalidate_cache();
    }

    /// 两个年度库里**恰好同号**的孤儿设备必须是两台设备（C6）。
    ///
    /// 「有统计行、没登记行」的历史数据在查询侧用 `device-id:<...>` 占位键兜底，
    /// 而 `devices.id` 是每个年度库各自自增的 —— 占位键不带来源年份时，2025 库的
    /// 3 号设备和 2026 库的 3 号设备（两台毫无关系的设备）会被 `merge_device_rows`
    /// 按同一身份并成一行：次数相加、给其中一台起的别名显示在另一台头上。
    #[test]
    fn archived_placeholder_keys_from_different_year_dbs_stay_separate() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("devq_orphan");

        let this_year = Local::now().year();
        let prev_year = this_year - 1;
        // 两个库各插一条"只有统计行、没有登记行"的孤儿，并且故意用同一个 id=3
        for (year, n) in [(prev_year, 40i64), (this_year, 60)] {
            let conn = crate::db::connection::open_rw(&paths::year_db_path(year)).unwrap();
            crate::db::connection::ensure_schema(&conn, year).unwrap();
            let dk = day_key_of_date(chrono::NaiveDate::from_ymd_opt(year, 6, 1).expect("date"));
            // 直接写 device_counts：绕开 devices 字典，制造孤儿行
            conn.execute(
                "INSERT INTO device_counts (date_key, device_id, count) VALUES (?1, 3, ?2)",
                rusqlite::params![dk, n],
            )
            .unwrap();
            // 让这个库真的算"有数据"（否则 available_years 把它当空壳滤掉）
            conn.execute(
                "INSERT INTO daily_counts (date_key, count, seconds) VALUES (?1, ?2, 10)",
                rusqlite::params![dk, n],
            )
            .unwrap();
            drop(conn);
        }
        invalidate_years_cache();

        // 总计视图（period=0）：两台设备两行
        let (total, stats) = get_device_stats(None, None);
        assert_eq!(total, 100, "两库次数都该算进来");
        assert_eq!(
            stats.len(),
            2,
            "跨库同号的孤儿必须是两台设备，不是一行: {:?}",
            stats.iter().map(|s| (&s.key, s.count)).collect::<Vec<_>>()
        );
        assert!(
            stats.iter().all(|s| s.count != 100),
            "任何一行都不许是两库相加的数"
        );
        // 键必须带来源年份，且各自指向自己的库
        let mut keys: Vec<&str> = stats.iter().map(|s| s.key.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                archived_device_key(prev_year, 3).as_str(),
                archived_device_key(this_year, 3).as_str()
            ],
            "占位键必须是 device-id:<来源年份>:<库内 id>"
        );
        // 显示名要说清来历，不能把机器串端给用户
        for s in &stats {
            assert!(
                s.name.contains("年归档 #3"),
                "占位设备的名得能看懂: {:?}",
                s.name
            );
            assert!(
                !s.name.contains("device-id"),
                "机器串不许上界面: {}",
                s.name
            );
        }

        // 别名互串是这台设备最疼的形态：给今年那只起名，去年的另一只不能跟着变
        crate::device_alias::invalidate_cache();
        let mine = archived_device_key(this_year, 3);
        let theirs = archived_device_key(prev_year, 3);
        crate::device_alias::set(&mine, "今年这只").unwrap();
        let (_, after) = get_device_stats(None, None);
        let row = |k: &str| {
            after
                .iter()
                .find(|s| s.key == k)
                .unwrap_or_else(|| panic!("找不到 {k}"))
        };
        assert_eq!(row(&mine).name, "今年这只");
        assert_eq!(
            row(&theirs).name,
            row(&theirs).auto_name,
            "另一台同号设备的名字被别名串走了"
        );
        // 详情也必须只统计自己那个库
        assert_eq!(get_device_detail(&mine, 0).all, 60);
        assert_eq!(get_device_detail(&theirs, 0).all, 40);
        crate::device_alias::clear(&mine).unwrap();
        crate::device_alias::invalidate_cache();
    }

    /// 库里已有的裸占位键（旧版本写下的）要在 `ensure_schema` 里补上来源年份，
    /// 并且**重复执行是空操作**（不能每次都改写一遍）。
    #[test]
    fn legacy_bare_placeholder_keys_get_retagged_once() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("devq_retag");
        let year = Local::now().year();
        let path = paths::year_db_path(year);
        {
            let conn = crate::db::connection::open_rw(&path).unwrap();
            crate::db::connection::ensure_schema(&conn, year).unwrap();
            // 旧形态：设备表里躺着 `device-id:3`（没有来源年份）
            conn.execute(
                "INSERT INTO devices (id, device_key, name, kind) VALUES (3, 'device-id:3', 'device-id:3', 'unknown')",
                [],
            )
            .unwrap();
            let dk = day_key_of_date(chrono::NaiveDate::from_ymd_opt(year, 6, 1).expect("date"));
            conn.execute(
                "INSERT INTO device_counts (date_key, device_id, count) VALUES (?1, 3, 9)",
                rusqlite::params![dk,],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO daily_counts (date_key, count, seconds) VALUES (?1, 9, 10)",
                rusqlite::params![dk],
            )
            .unwrap();
        }
        // 再走一次 ensure_schema（= 下次启动、或归档/导入打开这个库）
        let conn = crate::db::connection::open_rw(&path).unwrap();
        crate::db::connection::ensure_schema(&conn, year).unwrap();
        let key: String = conn
            .query_row("SELECT device_key FROM devices WHERE id = 3", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(key, archived_device_key(year, 3), "裸 id 要补上来源年份");
        // 计数必须还挂在这行上（改的是键的写法，不是归属）
        let n: i64 = conn
            .query_row("SELECT SUM(count) FROM device_counts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 9);
        drop(conn);
        // 幂等：第三次跑不得再改一次（第三次会把年份写成两层）
        let conn = crate::db::connection::open_rw(&path).unwrap();
        crate::db::connection::ensure_schema(&conn, year).unwrap();
        let key2: String = conn
            .query_row("SELECT device_key FROM devices WHERE id = 3", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(key2, key, "重跑 ensure_schema 不该再改写占位键");
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
        // 必须换**型号**才算另一台设备：B14-2 之后换实例号（原来这里是 `#never`）
        // 是同一台设备的另一种键形态，明细当然归它 —— 拿那种写法造"从没记过明细"
        // 的设备，造出来的只是同一台设备的第二个别名键。
        let never = "HID#VID_1111&PID_2222#never";
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
        // B14-2 后库里的键是身份形态；「name == key」这条脏登记规则与键形态无关
        let dirty = "VID_046D&PID_C52B&MI_00";
        let clean = "VID_1B1C&PID_1B2D";
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
        // B14-2 后库里的键是身份形态；鼠标/键盘两面靠接口段（&MI_00/&MI_01）区分，
        // 同 VID/PID 不同接口不会并组（这正是接口段进键的原因）。
        let a = "VID_046D&PID_C52B&MI_00";
        let b = "VID_046D&PID_C52B&MI_01";
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
            year: 2026,
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
            year: 2026,
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

    /// 词条表对显示名必须**幂等**，且不认识的一律原样交出去。
    ///
    /// 幂等这条不是讲究：读侧过表之后，两只同型号设备的名字要仍然相等，
    /// 同名去重才会给第二只加 "(2)"；表若对已中文化的串再动一次手，
    /// 两只就变成两个名字，去重静默失效（症状是设备页并列两只一模一样的鼠标）。
    #[test]
    fn localizing_a_display_name_is_idempotent_and_conservative() {
        assert_eq!(
            localize_display_name("HID-compliant mouse · 046D/C52B"),
            "HID 鼠标 · 046D/C52B",
            "写侧存过英文的旧行要换得过来"
        );
        assert_eq!(
            localize_display_name("HID 鼠标 · 046D/C52B"),
            "HID 鼠标 · 046D/C52B",
            "已经中文化的名字二次过表不许再变"
        );
        assert_eq!(
            localize_display_name("HID Keyboard Device"),
            "HID 键盘",
            "没有 ` · ` 分隔符的整串也要过表"
        );
        assert_eq!(
            localize_display_name("罗技 G502 HERO · 046D/C07D"),
            "罗技 G502 HERO · 046D/C07D",
            "词条表里没有的真名一个字都不许动"
        );
        assert_eq!(localize_display_name(""), "");
    }

    /// 同一台设备跨年两行时，展示名与类型都不许跟着"哪个库先读到"变。
    ///
    /// 上面那条 `device_stats_collapse_the_same_device_across_year_dbs` 钉住了**次数与行键**
    /// 在读序翻转时不变，却没钉名字：旧实现是"第一条非空即定"，于是两只库行名字不同时就
    /// 会「总计视图叫 A、最近 30 天叫 B」（详情弹窗复用排行那一行，两处跟着一起变）。
    /// 这里把两种读序都跑一遍，只许有一个答案。
    #[test]
    fn a_device_keeps_its_name_whichever_year_is_read_first() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("devq_name_order");
        crate::device_alias::invalidate_cache();

        // 旧库那行是升级前的完整实例路径、新库是身份键：同一台设备的两种形态
        let prev_key = "HID#VID_046D&PID_C52B&MI_00#7&1f126e19&0&0000";
        let this_key = "VID_046D&PID_C52B&MI_00";
        let row = |key: &str, name: &str, kind: &str, count: i64, year: i32| DeviceRow {
            device_key: key.to_string(),
            name: Some(name.to_string()),
            kind: Some(kind.to_string()),
            count,
            year,
        };
        let old = row(
            prev_key,
            "HID-compliant mouse · 046D/C52B",
            "mouse",
            30,
            2025,
        );
        let new = row(this_key, "罗技 M590 · 046D/C52B", "mouse", 70, 2026);

        for (first, second, label) in [
            (old.clone(), new.clone(), "旧库先读到（跨年窗口那条路）"),
            (
                new.clone(),
                old.clone(),
                "新库先读到（period=0 降序那条路）",
            ),
        ] {
            let (total, stats) = merge_device_rows(vec![first, second]);
            assert_eq!(total, 100, "{label}：次数必须照并");
            assert_eq!(stats.len(), 1, "{label}：两种键形态要并成一台");
            assert_eq!(
                stats[0].auto_name, "罗技 M590 · 046D/C52B",
                "{label}：名字取最新登记年那条，读序不参与决定"
            );
            assert_eq!(
                stats[0].key, "VID_046D&PID_C52B&MI_00",
                "{label}：行键是身份段"
            );
        }

        // 空名占位行仍然永远让位：新库那条没登记名时，只能用旧库那条（不能回退成占位串）
        let empty_new = row(this_key, "  ", "mouse", 70, 2026);
        let (_, stats) = merge_device_rows(vec![empty_new, old]);
        assert_eq!(
            stats[0].auto_name, "HID 鼠标 · 046D/C52B",
            "空白登记名让位于有值的那条，同时读侧词条照样生效"
        );
    }

    /// 别名生效：改过名的设备展示别名，自动名仍保留在 auto_name 里做副标题。
    #[test]
    fn device_alias_overrides_display_name() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("devq_alias");
        crate::device_alias::invalidate_cache();

        // B14-2 后库里的键是身份形态；别名按精确键解析（parse_vid_pid 对它照样有效）
        let key = "VID_046D&PID_C52B&MI_00";
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

    /// 回归：`get_daily_counts(1)` 返回的是**昨天**。
    ///
    /// `date_minus_days` 拿 [`clamp_query_days`]（"窗口含几天"的口径、下界 1）去钳
    /// "往回减几天"这个增量，于是 `days - 1 == 0` 被抬成 1 ⇒ 起点=昨天、map 里只有
    /// 昨天那一格，今天的那一行在 `daily_map.get_mut` 那一步被直接丢掉。而函数自己的
    /// 文档写着"含当天共 N 天"，`maintenance.rs` 那边的口径也是"早于本年"。
    /// 可达入口：`plugins/host.rs` 的 `focusflow.stats.daily_counts(days.max(1))`
    /// —— 传 1（或 0/负数）就拿不到今天。
    ///
    /// 与已有的 `daily_counts_span_includes_today` 的区别：那条只断言**长度**
    /// （1 格 / 200 格），不判日期 ⇒ 非区分性用例，所以这个错位它照不出来。
    #[test]
    fn one_day_window_ends_today_not_yesterday() {
        use chrono::Datelike;
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("daily_one");
        invalidate_years_cache();

        let today = Local::now().date_naive();
        let y = today.year();
        let today_s = today.format("%Y-%m-%d").to_string();
        {
            let conn = connection::open_rw(&paths::year_db_path(y)).unwrap();
            connection::ensure_schema(&conn, y).unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO daily_counts (date_key, count, seconds) VALUES (?1, 777, 10)",
                [day_key_of_date(today)],
            )
            .unwrap();
            let _ = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
        }
        invalidate_years_cache();

        let one = get_daily_counts(1, None);
        assert_eq!(one.len(), 1, "窗口 1 天就该只有一格，实得 {one:?}");
        assert_eq!(
            one[0].0, today_s,
            "那一格必须是**今天**（旧写法给的是昨天）"
        );
        assert_eq!(one[0].1, 777, "今天那行的数必须带出来，实得 {:?}", one[0]);

        // 对照腿：2 天窗口本来就是 [昨天, 今天] —— 证明不是我把 end_dk 读反了
        let two = get_daily_counts(2, None);
        assert_eq!(two.len(), 2, "实得 {two:?}");
        assert_eq!(
            two.last().map(|p| p.0.as_str()),
            Some(today_s.as_str()),
            "两天窗口的最后一格也必须是今天"
        );
    }

    /// 回归（`period_start_key` 那一族的另一半）：`device_key_rows` 自己写了一遍
    /// 周期起点公式 `today_key - (n.max(1) - 1)`，而 `period_start_key` 那份夹到
    /// `MAX_QUERY_DAYS` —— 同一个公式两份实现。
    ///
    /// 差别的形状要说清（我第一版按 `period_start_key` 注释里那句"会下溢"写成了
    /// **崩溃级**，注回旧写法实测**不 panic** —— `n = i64::MAX` 时结果约 −9.22e18，
    /// 仍在 i64 范围内，那个说法在这条 i64 键运算上不成立，收回）。真正的问题是
    /// 那一档等于"没有下界"：不夹的写法会把 1900 年的行也算进"近 N 天"。
    ///
    /// `period` 是外部输入：现在把它交下来的是原生端的设备详情弹窗
    /// （`desktop-native/src/main.rs` 直接点 `get_device_detail`），这一条
    /// 不过 `is_valid_period`（对比同一个文件里的统计视图那条）。
    ///
    /// 已有的 `device_detail_period_cannot_underflow` 只测纯函数那一份，
    /// 照不到第二处实现 —— 这条直接点 `device_key_rows` 本体。
    ///
    /// （第一版只从入口喂极端值 + 断言"空库返回空"，注入取证 INJ-39 注回旧写法后
    /// 仍然绿 —— 那等于什么都没测。第二版改成"种一条 1900 年的设备行"：
    /// 它在窗口之外、在全表之内，两种写法的答案不一样。）
    #[test]
    fn device_key_rows_extreme_period_is_clamped_not_unbounded() {
        use chrono::Datelike;
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("dev_keys_clamp");
        invalidate_years_cache();
        let key = "HID#VID_1234&PID_5678#0";
        let old_year = 1900i32;
        let old_dk =
            day_key_of_date(chrono::NaiveDate::from_ymd_opt(old_year, 6, 1).expect("date"));
        let today_key = day_key_of_date(Local::now().date_naive());
        {
            let conn = connection::open_rw(&paths::year_db_path(old_year)).unwrap();
            connection::ensure_schema(&conn, old_year).unwrap();
            conn.execute(
                "INSERT OR IGNORE INTO devices (id, device_key, name, kind) VALUES (7, ?1, '老键鼠', 'mouse')",
                [key],
            )
            .unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO device_key_counts (date_key, device_id, key_name, count) VALUES (?1, 7, 'A', 5)",
                [old_dk],
            )
            .unwrap();
            let _ = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
        }
        invalidate_years_cache();
        // 前提：这条行必须真的落在窗口之外，否则整条用例在测"查不到东西"
        assert!(
            today_key - old_dk > MAX_QUERY_DAYS,
            "夹具不对：1900 年那条行距今天只有 {} 天，窗口 {MAX_QUERY_DAYS} 天测不出来",
            today_key - old_dk
        );
        // 全表那一档必须看得见它
        let all = device_key_rows(key, 0).0;
        assert_eq!(
            all,
            vec![("A".to_string(), 5)],
            "period=0 是全表，实得 {all:?}"
        );
        // 极端周期：夹住 ⇒ 1900 年那行不计进来（注回旧写法会算进来 ⇒ 这条红）
        let huge = device_key_rows(key, i64::MAX).0;
        assert!(
            huge.is_empty(),
            "i64::MAX 要夹到 {MAX_QUERY_DAYS} 天，不能等于「没有下界」，实得 {huge:?}"
        );
        // 反向腿：窗口内的行照常计入，且那一档与 MAX_QUERY_DAYS 完全一致
        let cur = Local::now().year();
        {
            let conn = connection::open_rw(&paths::year_db_path(cur)).unwrap();
            connection::ensure_schema(&conn, cur).unwrap();
            conn.execute(
                "INSERT OR IGNORE INTO devices (id, device_key, name, kind) VALUES (7, ?1, '键鼠', 'mouse')",
                [key],
            )
            .unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO device_key_counts (date_key, device_id, key_name, count) VALUES (?1, 7, 'B', 3)",
                [today_key],
            )
            .unwrap();
            let _ = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
        }
        invalidate_years_cache();
        let got = device_key_rows(key, i64::MAX).0;
        assert_eq!(
            got,
            vec![("B".to_string(), 3)],
            "窗口内的行要照常计入、窗口外的不计（实得 {got:?}）"
        );
        assert_eq!(
            device_key_rows(key, MAX_QUERY_DAYS).0,
            got,
            "i64::MAX 与 MAX_QUERY_DAYS 必须是同一档，不能一份回绕一份夹住"
        );
    }

    /// 某一年打不开时必须**报出是哪一年**，而不是让调用方拿着少一年的总数当结论。
    ///
    /// 夹具形态就是真机会遇到的那一种：文件名是年度库，内容却不是 SQLite 库（同步盘
    /// 换成占位文件、半截下载、被别的进程独占）。`available_years()` 保守地把它列出来
    /// （"读不了就按有数据处理"，见 `year_has_aggregates`），而 `stats_multi_year`
    /// 会静默跳过那一年 —— 两边都不出声，导出的报告就少一年而界面报成功。
    #[test]
    fn an_unopenable_year_is_named_instead_of_silently_dropped() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("readable_years");
        invalidate_years_cache();
        crate::db::connection::clear_ro_cache();

        let good = 2031;
        let broken = 2032;
        {
            let conn = crate::db::connection::open_rw(&paths::year_db_path(good)).unwrap();
            crate::db::connection::ensure_schema(&conn, good).unwrap();
            conn.execute(
                "INSERT INTO daily_counts (date_key, count, seconds) VALUES (?1, 5, 60)",
                rusqlite::params![day_key_of_date(
                    chrono::NaiveDate::from_ymd_opt(good, 6, 1).expect("夹具日期")
                )],
            )
            .unwrap();
        }
        std::fs::write(paths::year_db_path(broken), b"not a sqlite file at all").unwrap();

        // 前提：坏的那一年确实被列出来了（它没有被"当作没有数据"悄悄抹掉）
        assert_eq!(
            available_years(),
            vec![broken, good],
            "列表是降序的，且打不开那一年也在里面"
        );
        let e = check_years_readable().unwrap_err().to_string();
        assert!(
            e.contains(&broken.to_string()) && e.contains("打不开"),
            "要报出是哪一年，不能只说\"读不出来\"：{e}"
        );
        // 那一轮用户什么数字都没拿到，手里只剩一件事可做 ⇒ 这句话要点名文件、并说清下一步
        assert!(
            e.contains(&format!("focusflow_{broken}.db")),
            "要报出是哪个文件（只给年份的话，用户不知道该去动哪一份）：{e}"
        );
        assert!(
            e.contains("再跑一次就有数"),
            "要说出处理完之后怎么办，而不是把一个死路当结论：{e}"
        );

        // 修好之后同一句话就不再响 —— 这条判据不是恒真
        std::fs::remove_file(paths::year_db_path(broken)).unwrap();
        invalidate_years_cache();
        crate::db::connection::clear_ro_cache();
        assert_eq!(
            check_years_readable().expect("都能打开时不该报错"),
            vec![good]
        );
    }

    /// 接线腿：插件的 `today_count` 也走"按日对一次"的显示口径。
    ///
    /// 午夜后、今天还没输入时，写线程缓存仍是昨天的 —— 直接照抄会让插件面板把
    /// 昨天一整天的数当成"今日"。纯函数用例在 writer.rs，这条钉调用点。
    #[test]
    fn get_today_count_uses_the_day_checked_cache() {
        let src = include_str!("queries.rs");
        let at = src
            .find("pub fn get_today_count")
            .expect("get_today_count 找不到了");
        let body = &src[at..at + src[at..].find("\n}").expect("函数体切不出来")];
        assert!(
            body.contains("today_count_for_display()"),
            "get_today_count 直接照抄了今日缓存（午夜后会把昨天当成今日）"
        );
    }

    /// 占位登记名还有第三种形态没被兜住：合并行的名字来自**另一条库行**。
    ///
    /// 同一台设备跨年换过口（或两个年度库里各留一条占位登记），合并时"名字取最新登记年
    /// 那条"，于是展示行的 `device_key` 是 A 的原始键、而 `name` 是 B 的完整实例路径 ——
    /// 旧判据只比这两者，两条都不命中 ⇒ 榜上直接把 `\\?\HID#VID_…#{GUID}` 端给用户。
    #[test]
    fn a_placeholder_name_from_another_row_is_still_recognised() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("devq_merge_name");
        crate::device_alias::invalidate_cache();

        let a = r"\\?\HID#VID_046D&PID_C52B#7&aaa&0&0000#{GUID-A}";
        let b = r"\\?\HID#VID_046D&PID_C52B#7&bbb&0&0000#{GUID-B}";
        // 两条库行都是「name == 自己的键」这种占位登记，只有年份不同
        let row = |key: &str, year: i32, count: i64| DeviceRow {
            device_key: key.to_string(),
            name: Some(key.to_string()),
            kind: Some("mouse".to_string()),
            count,
            year,
        };
        let (_, stats) = merge_device_rows(vec![row(a, 2024, 100), row(b, 2025, 50)]);

        assert_eq!(
            stats.len(),
            1,
            "夹具前提：同一身份的两条库行必须并成一行，否则测的不是合并名这一路"
        );
        assert!(
            !stats[0].auto_name.contains('#') && !stats[0].auto_name.contains(r"\\?\"),
            "合并过来的占位名要一起折成「没名字」走回退，实际端出去的是 {:?}",
            stats[0].auto_name
        );
        assert!(
            !stats[0].has_alias,
            "这台设备没起过别名（别名侧不参与这条判据）"
        );

        // 正向腿：真人起的名字不会被这条判据吃掉
        let (_, kept) = merge_device_rows(vec![row(a, 2024, 100), {
            let mut r = row(b, 2025, 50);
            r.name = Some("书房那只鼠标".to_string());
            r
        }]);
        assert_eq!(
            kept[0].auto_name, "书房那只鼠标",
            "归一后不等于身份段的真名必须原样保留"
        );
    }

    /// 「这台设备没有键名明细」只许在**每一个能读的库都说没有**的时候说。
    ///
    /// 这个布尔在界面上念出来的是「键名明细从该功能上线后开始积累，此前的历史数据
    /// 无法回溯」—— 一句关于用户历史的话。原先某一年库打不开（移动盘还没醒、
    /// 同步盘放的是占位、库被别的连接握着）也走同一句话，而且零日志。
    #[test]
    fn an_unreadable_year_is_not_reported_as_no_detail() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("q_device_detail");
        let identity = "HID#VID_046D&PID_C52B&MI_00#夹具";
        let broken = 2031;
        {
            let conn = connection::open_rw(&paths::current_year_db_path()).unwrap();
            connection::ensure_schema(&conn, chrono::Local::now().year()).unwrap();
        }
        invalidate_years_cache();
        connection::clear_ro_cache();
        assert_eq!(
            device_has_key_detail(identity),
            DetailVerdict::Absent,
            "有这张表、而这台设备一行明细都没有：这才是「没有明细」"
        );

        // 另一年变成一个不是 SQLite 的文件 ⇒ 那一年**答不上来**，不能算"没有"
        std::fs::write(paths::year_db_path(broken), b"not a sqlite file at all").unwrap();
        invalidate_years_cache();
        connection::clear_ro_cache();
        let verdict = device_has_key_detail(identity);
        assert!(
            matches!(&verdict, DetailVerdict::Unknown(why) if why.contains(&broken.to_string())),
            "要说清是哪一年答不上来，实得 {verdict:?}"
        );

        // 反向腿：坏文件拿走之后同一个问题回到 Absent —— 上面那条不是恒真
        std::fs::remove_file(paths::year_db_path(broken)).unwrap();
        invalidate_years_cache();
        connection::clear_ro_cache();
        assert_eq!(device_has_key_detail(identity), DetailVerdict::Absent);
    }

    /// 「某一年读不出来」不许在「最高单日」里等于「那一年没纪录」。
    ///
    /// 原先打不开、没有 `daily_counts` 表、表在但查询失败三件事都折成同一个"没有"：
    /// 一次瞬时的文件锁（别的实例握着库、移动盘还没醒、同步盘放的是占位）就把屏上那句
    /// "最高单日"念小，而一个字不吭。
    ///
    /// ⚠ 这一趟原来还算一个全历史总次数并交回 `total`，而生产侧一个读者都没有
    /// （屏上「总计」念的是 `stats_totals(...).total + pending`），已随那一刀摘掉。
    /// "跨库遍历不许漏年份"这条口径并没有失去归宿：它由 [`stats_totals`]／[`get_stats`]
    /// 在 `aggregation_test.rs` 与 `real_data.rs` 里继续守着 —— 那两条是有真读者的尺子。
    /// 年度库**名单本身**问不出来时，这一趟要交出「不完整」，不许念成"这台机器没记过东西"。
    ///
    /// 旧形状：循环吃的是 `available_years()`（`read_dir` 失败 ⇒ 空名单），于是一行都不跑、
    /// `unreadable` 也是空的 ⇒ 屏上「最高单日 —」与「日均分母 0」和真的零历史长得一模一样。
    #[test]
    fn the_alltime_read_speaks_up_when_the_year_list_cannot_be_read() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("q_alltime_list_fail");
        let data = paths::data_dir();
        // 夹具：把数据目录的位置占成一个普通文件 ⇒ `read_dir` 必失败（自愈那笔用的是同款）
        let _ = std::fs::remove_dir_all(&data);
        std::fs::write(&data, "这不是目录").expect("占住数据目录的位置");
        invalidate_years_cache();
        let got = get_alltime_max_day();
        assert!(
            got.unreadable.iter().any(|u| u.contains("年度库列不出来")),
            "名单读不出来这件事要留在 unreadable 里，实际: {:?}",
            got.unreadable
        );
        assert!(got.max_day.is_none(), "名单都没读出来，不许报出一个纪录");
        assert_eq!(got.active_days, 0, "一天都不该被数进分母");
        // 对照带：目录正常、只是库里没东西 ⇒ 那是"问得出、答案是零"，不许有怨言
        std::fs::remove_file(&data).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        invalidate_years_cache();
        let empty = get_alltime_max_day();
        assert!(
            empty.unreadable.is_empty(),
            "空目录是答得出来的零，不该混进同一句抱怨: {:?}",
            empty.unreadable
        );
    }

    #[test]
    fn an_unreadable_year_is_not_read_as_no_record_in_the_alltime_max_day() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("q_alltime_totals");
        let day_a = day_key_of_date(chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap());
        let day_b = day_key_of_date(chrono::NaiveDate::from_ymd_opt(2026, 1, 2).unwrap());
        {
            let conn = connection::open_rw(&paths::current_year_db_path()).unwrap();
            connection::ensure_schema(&conn, chrono::Local::now().year()).unwrap();
            conn.execute(
                "INSERT INTO daily_counts(date_key, count) VALUES (?1, 3)",
                [day_a],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO daily_counts(date_key, count) VALUES (?1, 5)",
                [day_b],
            )
            .unwrap();
        }
        invalidate_years_cache();
        connection::clear_ro_cache();
        let clean = get_alltime_max_day();
        assert!(
            clean.unreadable.is_empty(),
            "夹具前提：这里只有一个能读的库，实得 {:?}",
            clean.unreadable
        );
        assert_eq!(clean.max_day, Some(("2026-01-02".to_string(), 5)));

        // 一整年变成一个不是 SQLite 的文件 ⇒ 那一年**没有答案**
        let broken = 2031;
        std::fs::write(paths::year_db_path(broken), b"not a sqlite file at all").unwrap();
        // 另一年表在、列却不全（旧库里手工拼出来的形状）：那一行 `count = 7` 比已读的
        // 纪录 5 还大，可这一年**查不动**（`date_key` 那一列不存在）。
        // 于是它既不许被念成纪录（7 上不了屏），也不许被念成"那一年没纪录"——
        // 只能点名说它没答上来。旧形状里这一年是"算进总数、不给纪录"的半条答案，
        // 两张卡片来自同一个遍历时那种形态比整条没有更难查。
        let half_baked = 2030;
        {
            let conn = connection::open_rw(&paths::year_db_path(half_baked)).unwrap();
            conn.execute("CREATE TABLE daily_counts (count INTEGER)", [])
                .unwrap();
            conn.execute("INSERT INTO daily_counts(count) VALUES (7)", [])
                .unwrap();
        }
        invalidate_years_cache();
        connection::clear_ro_cache();
        let partial = get_alltime_max_day();
        assert_eq!(
            partial.max_day,
            Some(("2026-01-02".to_string(), 5)),
            "读不出来的那些年一律不许参与最大值：7 那个假纪录不能上屏，\
             而把这一年折成「没纪录」就等于屏上念小了还一个字不吭"
        );
        let said = partial.unreadable.join("；");
        assert!(
            said.contains(&broken.to_string()),
            "要说出是哪一年连不上连接，实得 {said}"
        );
        assert!(
            said.contains(&half_baked.to_string()) && said.contains("最高单日"),
            "半拉子那一年要按年份＋卡在哪一步说清，实得 {said}"
        );

        // 反向腿：两份坏文件都拿走 ⇒ 同一次遍历回到「读全了」，上面几条不是恒真
        // 先放掉只读池里的句柄：半拉子那一年是**能打开**的，连接还握在这个线程的池里，
        // Windows 上握着句柄就删不掉文件。
        connection::clear_ro_cache();
        std::fs::remove_file(paths::year_db_path(broken)).unwrap();
        std::fs::remove_file(paths::year_db_path(half_baked)).unwrap();
        invalidate_years_cache();
        let back = get_alltime_max_day();
        assert!(back.unreadable.is_empty(), "实得 {:?}", back.unreadable);
        assert_eq!(back.max_day, Some(("2026-01-02".to_string(), 5)));
    }

    /// 同一趟交回的「有记录的天数」：跨年度库相加，且次数为 0 的那天不算有记录。
    ///
    /// 它是原生端「总计」那一档「日均」的分母（`desktop-native/src/data.rs` 的 `avg_of`）。
    /// 立这条的理由是那一档过去拿的是**图上那 30 天**里有行的天数去除全部历史的次数，
    /// 所以这一趟必须真能交出"全历史干了事的那几天"，分母才有的可换。
    /// 每库仍然只有原来那一条语句（天数挂在同一句的标量子查询里），没有为它多排一次队。
    #[test]
    fn the_alltime_day_count_adds_up_across_years_and_skips_zero_days() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("q_alltime_days");
        let y_now = chrono::Local::now().year();
        // 本年：两天记到过次数（3 与 5），另有一天是 0 —— 那天没干过事，不该进分母
        {
            let conn = connection::open_rw(&paths::current_year_db_path()).unwrap();
            connection::ensure_schema(&conn, y_now).unwrap();
            conn.execute_batch(
                "INSERT INTO daily_counts(date_key, count) VALUES
                 (1, 3), (2, 5), (3, 0)",
            )
            .unwrap();
        }
        // 上一年只有一天有记录：跨库要相加
        let y_prev = y_now - 1;
        {
            let conn = connection::open_rw(&paths::year_db_path(y_prev)).unwrap();
            connection::ensure_schema(&conn, y_prev).unwrap();
            conn.execute(
                "INSERT INTO daily_counts(date_key, count) VALUES (10, 4)",
                rusqlite::params![],
            )
            .unwrap();
        }
        invalidate_years_cache();
        connection::clear_ro_cache();

        let got = get_alltime_max_day();
        assert!(
            got.unreadable.is_empty(),
            "夹具前提：两个库都读得动，实得 {:?}",
            got.unreadable
        );
        assert_eq!(
            got.active_days, 3,
            "本年两天 + 上一年一天，次数为 0 的那天不算（实得 {}）",
            got.active_days
        );
        // 反向对照：最大值仍来自同一次遍历，两个数不是各查各的
        assert_eq!(got.max_day.map(|(_, c)| c), Some(5));
    }

    /// 「没有这张表」是真答案，「有表但查询失败」不是。
    #[test]
    fn a_missing_aggregate_table_is_an_answer_but_a_failed_query_is_not() {
        let conn = Connection::open_in_memory().unwrap();
        assert!(
            matches!(year_max_day_in_conn(&conn), YearMaxDay::NoTable),
            "库里压根没有 daily_counts：这就是「没有可汇总的数据」，按「没有纪录」算是对的"
        );

        conn.execute("CREATE TABLE daily_counts (count INTEGER)", [])
            .unwrap();
        conn.execute("INSERT INTO daily_counts(count) VALUES (7)", [])
            .unwrap();
        match year_max_day_in_conn(&conn) {
            YearMaxDay::Unreadable(why) => {
                assert!(why.contains("最高单日"), "要说清卡在哪一步，实得 {why}")
            }
            other => panic!("缺列的表要落成「读不出来」，实得 {other:?}"),
        }
    }

    /// 第一次问出「没有」之后别人把那张表建出来了 ⇒ 第二次问必须看得见。
    ///
    /// 这条钉的是 `connection::table_presence_cached` **只记「有」、不记「没有」**那一半。
    /// 加记忆本身是有实测依据的：读侧每拍为每个库重复问一次存在性，现量今日档 10 条、
    /// 总计档 13 条全是这一问（`crates/core/tests/perf_probe.rs`「每一臂自己几条语句」）。
    /// 但一旦顺手把「没有」也记下来，症状不是红、是**安静**：导入补建的 `key_log`、
    /// 按日期删空后重建的表，本线程再也看不见，屏上永远念"这一栏没有数据"——
    /// 而省掉的那 10 条查询不会让任何用例因为"少查了一次"而响。
    /// 所以这条腿必须自己把顺序摆出来：先问出没有 → 不清池地补建 → 再问。
    /// 中间那句「不清池」是全部区分性所在：清了池就等于把记忆绕过去，用例恒绿。
    #[test]
    fn a_table_created_after_the_first_asking_is_still_seen() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("q_presence_memo_sees_late_tables");
        let year = Local::now().year();
        let path = paths::year_db_path(year);
        let today_dk = day_key_of_date(Local::now().date_naive());
        // 只建 `daily_counts` 并放一行（`available_years` 要认这一年），**故意不建 app_usage**
        {
            let conn = connection::open_rw(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE daily_counts (
                    date_key INTEGER PRIMARY KEY,
                    count INTEGER NOT NULL,
                    seconds INTEGER NOT NULL DEFAULT 0);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO daily_counts (date_key, count) VALUES (?1, 4)",
                [today_dk],
            )
            .unwrap();
        }
        invalidate_years_cache();
        connection::clear_ro_cache();

        // 第一趟：这张表确确实实没有 ⇒ 空榜，而且**不许**被说成"读不出来"
        let first = app_stats_totals(None, None);
        assert!(
            first.unreadable.is_empty(),
            "夹具前提：这是「没有这张表」这个真答案，实得 {:?}",
            first.unreadable
        );
        assert_eq!(first.total, 0, "没有 app_usage 这张表时总时长就是 0");
        assert!(first.rows.is_empty(), "空榜，实得 {:?}", first.rows);

        // 补建：**不清池、不失效代次** —— 上面那次查询已经把只读连接留在这个线程的池里，
        // 这一条测的就是"池还是那张池"的时候，新表看不看得见。
        {
            let conn = connection::open_rw(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE app_usage (
                    date_key INTEGER NOT NULL,
                    app_name TEXT NOT NULL,
                    seconds INTEGER NOT NULL,
                    PRIMARY KEY (date_key, app_name)) WITHOUT ROWID;",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO app_usage (date_key, app_name, seconds) VALUES (?1, 'note.exe', 90)",
                [today_dk],
            )
            .unwrap();
        }
        let second = app_stats_totals(None, None);
        assert_eq!(
            second.total, 90,
            "把「没有」记下来 ⇒ 本线程永远看不见这张后建出来的表，这里会是 0"
        );
        assert_eq!(second.rows.get("note.exe").copied(), Some(90));

        // 反向控制带（两条，各挡一种写法）：同一张**始终没建过**的表，
        // ①第一次问要回「没有」—— 名单不许把它当成"这张我有"；
        // ②**连着第二次问仍要回「没有」**—— 上面那条主腿只走得到"先没有、后建出来"，
        //    挡不住"把『没有』也塞进名单、下次直接答复『有』"那种写法（主腿照样会绿，
        //    因为那时表真的在了）。这条是那条的对照带，不是重复。
        let never = connection::open_ro(&path).unwrap();
        assert_eq!(
            connection::table_presence_cached(&never, "device_key_counts"),
            connection::Presence::No,
            "没建过的表要答得出「没有」"
        );
        assert_eq!(
            connection::table_presence_cached(&never, "device_key_counts"),
            connection::Presence::No,
            "第二次问不许因为名单里躺着一个「没有」就改口答「有」"
        );
    }

    /// 打卡条那一趟扫到的坏年份，不许算到"只画今天那一格"的图头上。
    ///
    /// 每日序列现在**一趟查两段**（`read_daily_pair`）：窄的给趋势图、宽的给打卡条。
    /// 合并带来一个必须当场处理的后果 —— 宽窗口够得着的那一年如果读不出来，它的点名
    /// 会跟着这一趟一起回来；而面板是按域名分栏念的，「每日趋势」那一栏于是会指控一张
    /// 根本没画那一年的图。**数字没错，指控错了人**，所以"按各自窗口的年份清单分头交回"
    /// 这一条得钉住。
    ///
    /// 夹具的两个前提都是构造出来的、不靠今天的日期：窄段取 1 天（年份集必然只有今年），
    /// 宽段取 `GOAL_LOOKBACK_DAYS` = 370 天（任一起点往回数 370 天必然跨进上一年）。
    /// 反向腿在下一段：宽段自己必须**点名得到**那一年 —— 少了它，上面那条
    /// 「图里不许有」会被"整串都丢了"这种更坏的写法满足掉。
    #[test]
    fn the_daily_trend_only_blames_the_years_it_actually_draws() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("q_daily_pair_gaps");
        let today = Local::now().date_naive();
        let this_year = today.year();
        let prev = this_year - 1;
        {
            let conn = connection::open_rw(&paths::current_year_db_path()).unwrap();
            connection::ensure_schema(&conn, this_year).unwrap();
            conn.execute(
                "INSERT INTO daily_counts (date_key, count) VALUES (?1, 11)",
                [day_key_of_date(today)],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO daily_counts (date_key, count) VALUES (?1, 5)",
                [day_key_of_date(today - chrono::Duration::days(200))],
            )
            .unwrap();
        }
        // 上一年变成一个不是 SQLite 的文件 ⇒ 这一年**答不上来**。它同时留在
        // `available_years()` 里（打不开的文件保守地算"有数据"），所以宽窗口够得着它。
        std::fs::write(paths::year_db_path(prev), b"not a sqlite file at all").unwrap();
        invalidate_years_cache();
        connection::clear_ro_cache();

        let (chart, goal) = daily_counts_windows(1, crate::stats::GOAL_LOOKBACK_DAYS);
        assert_eq!(chart.series.len(), 1, "窄段就今天一格");
        assert!(
            chart
                .unreadable
                .iter()
                .all(|g| !g.contains(&prev.to_string())),
            "只画今天那一格的图，不许指控去年：{:?}",
            chart.unreadable
        );
        // 反向腿：宽段确实够着去年，那句点名要留在它自己那份里（丢了就是上面那条恒真）
        assert_eq!(
            goal.series.len(),
            crate::stats::GOAL_LOOKBACK_DAYS as usize,
            "宽段还是 370 格，合并没把窗口自己改小"
        );
        assert!(
            goal.unreadable
                .iter()
                .any(|g| g.contains(&prev.to_string())),
            "去年那一年要在宽段点名，实得 {:?}",
            goal.unreadable
        );
    }

    /// 「这一年读不出来」与「这一年没数据」必须是两件事：前者要在 `unreadable` 里点名，
    /// 后者才允许安静地按 0 计。
    ///
    /// 旧写法把三件事（库打不开、表读不出来、真没这张表）全折成 `(0, 空榜)`，而面板吃的
    /// 就是那个二元组 ⇒ "2026 年库此刻打不开"念出来跟"你今天一次键盘都没敲"一模一样；
    /// 跨年合并更糟：坏掉那一年被 `if let Some(..)` 直接跳过，报出来的总数看着像完整值。
    #[test]
    fn a_rank_reading_separates_cannot_read_from_no_data() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("q_rank_gaps");
        let year = Local::now().year();
        let path = paths::year_db_path(year);

        // 一、库里没有 `daily_counts`：真没有可汇总的数据 ⇒ 0，且不该被说成读不出来
        {
            let conn = connection::open_rw(&path).unwrap();
            conn.execute_batch("CREATE TABLE unrelated (a INTEGER);")
                .unwrap();
        }
        let empty = stats_totals(None, Some(year));
        assert_eq!(empty.total, 0);
        assert!(
            empty.unreadable.is_empty(),
            "确认没有这张表不该进 gap，实得 {:?}",
            empty.unreadable
        );
        connection::clear_ro_cache();
        std::fs::remove_file(&path).expect("删掉这一年的库（先 clear_ro_cache）");

        // 二、这个文件不是一份 SQLite（同步盘占位、半截写入那一族）⇒ 必须点名
        std::fs::write(&path, b"this is not a sqlite database at all").unwrap();
        let broken = stats_totals(None, Some(year));
        assert_eq!(broken.total, 0, "读不出来时不许凭空报出数");
        assert_eq!(
            broken.unreadable.len(),
            1,
            "那一年要被点名，实得 {:?}",
            broken.unreadable
        );
        let gap = &broken.unreadable[0];
        assert!(gap.contains("按键统计"), "gap 要说清是哪个域，实得 {gap}");
        assert!(
            gap.contains(&year.to_string()),
            "gap 要带年份，不然用户不知道该拿哪个库怎么办，实得 {gap}"
        );
        // 这三串都要能被消费侧认出域名（旧前端按域名挑栏位，现在周报把它们逐条列进正文），
        // 应用时长那一栏走同一条合并路径
        let apps_broken = app_stats_totals(None, Some(year));
        assert!(
            apps_broken
                .unreadable
                .iter()
                .any(|g| g.contains("应用时长")),
            "应用时长的 gap 要能认出域名，实得 {:?}",
            apps_broken.unreadable
        );
        // 旧的二元组口子照旧给 0：它分不出这两件事 ⇒ 面板不许再用它
        assert_eq!(get_stats(None, Some(year)).0, 0);
        // 换文件之前必须作废只读池：那条连接还握着这个路径，Windows 上删不掉
        // （旧写法 `.ok()` 吞掉失败，于是下面写进去的还是那份坏文件 —— 测的就不是第三件事了）
        connection::clear_ro_cache();
        std::fs::remove_file(&path).expect("删掉这一年的库（先 clear_ro_cache）");

        // 三、跨年合并：坏掉那一年不许被静默丢掉，读到的那一年照原样进总数
        connection::open_rw(&path)
            .and_then(|conn| connection::ensure_schema(&conn, year))
            .unwrap();
        std::fs::write(&path, b"this is not a sqlite database at all").unwrap();
        let good = year + 1;
        let good_path = paths::year_db_path(good);
        {
            let conn = connection::open_rw(&good_path).unwrap();
            connection::ensure_schema(&conn, good).unwrap();
            conn.execute(
                "INSERT INTO daily_counts (date_key, count, seconds) VALUES (?1, 11, 0)",
                [day_key_of_date(Local::now().date_naive())],
            )
            .unwrap();
        }
        let merged = stats_totals(None, None);
        assert_eq!(
            merged.total, 11,
            "好的那一年要照常计入，实得 {} / gap {:?}",
            merged.total, merged.unreadable
        );
        assert!(
            merged
                .unreadable
                .iter()
                .any(|g| g.contains(&year.to_string())),
            "坏掉那一年必须出现在 gap 里，实得 {:?}",
            merged.unreadable
        );

        // 四、**库头合法、页坏**：`open_ro` 打得开，是"问有没有这张表 / 读榜"那一步失败。
        // 上面几条用的是没有 SQLite 魔数的文件，走的是"打不开"那一臂 —— 探针把
        // `YearRead::Unreadable` 折成"没有这张表"时，只有这一条会红，另三条照绿。
        //
        // 三份榜都要铺到文件后半段：取行是 `collect::<Result<Vec<_>, _>>()`，坏页落在
        // 哪张表上就只有那张表报错。夹具只喂 `key_counts` 的话，把应用榜或设备榜的
        // 取行改回 `rows.flatten()`（错误折成"数据到此为止"）这一条照绿 —— 探针实测过。
        let tail = year + 2;
        let tail_path = paths::year_db_path(tail);
        connection::clear_ro_cache();
        if let Err(e) = std::fs::remove_file(&tail_path) {
            // 这一年本来就还没建库（NotFound 可以安静），别的失败都是"没删掉"——
            // 留着旧文件往下写，测的就不是这份夹具了
            assert!(
                crate::data_location::is_confirmed_absent(&e),
                "换夹具前删不掉这一年的库（不是「本来就没有」）: {e}"
            );
        }
        {
            let conn = connection::open_rw(&tail_path).unwrap();
            connection::ensure_schema(&conn, tail).unwrap();
            let dk = day_key_of_date(Local::now().date_naive());
            for i in 0..12_000i64 {
                // 交错写入：让三张表的叶子页都摊到文件的前后两半
                conn.execute(
                    "INSERT INTO key_counts (date_key, key_name, count) VALUES (?1, ?2, 1)",
                    rusqlite::params![dk, format!("KK{i}")],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO app_usage (date_key, app_name, seconds) VALUES (?1, ?2, 1)",
                    rusqlite::params![dk, format!("AA{i}")],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO device_counts (date_key, device_id, count) VALUES (?1, ?2, 1)",
                    rusqlite::params![dk, i],
                )
                .unwrap();
            }
            let _ = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
        }
        connection::clear_ro_cache();
        let mut bytes = std::fs::read(&tail_path).unwrap();
        assert!(
            bytes.len() > 4096 * 40,
            "夹具太小，三份榜没铺到文件后半段（实得 {} 字节）",
            bytes.len()
        );
        let cut = bytes.len() / 2;
        for b in bytes.iter_mut().skip(cut) {
            *b = 0xA5;
        }
        std::fs::write(&tail_path, &bytes).unwrap();
        connection::clear_ro_cache();
        let half_broken = stats_totals(None, Some(tail));
        assert_eq!(
            half_broken.total, 0,
            "这一年没读全，总数不许报出半个数（实得 {}）",
            half_broken.total
        );
        assert_eq!(
            half_broken.unreadable.len(),
            1,
            "「打得开、读不出来」也必须点名（这条测的就是 `Unreadable` 那一臂）",
        );
        assert!(
            !half_broken.unreadable[0].contains("打不开"),
            "这一臂不是打不开，原因别说错：{}",
            half_broken.unreadable[0]
        );
        // 三个榜 × 两种取参（全表 / 带日期窗口）各一条腿：`query_*_in_conn` 的
        // `param` 两个分支是两段取行代码，只喂一边时另一边的 fold 探针照绿（实测）。
        //
        // ⚠ 应用榜这两条盯的是"取行之前那句聚合先报错"那一臂：`app_usage` 的
        // `SUM(seconds)` 与按应用聚合读的是同一批页，页坏时聚合就先失败，取行处的
        // `collect` 永远轮不到出声 —— 探针（把应用榜的取行改回 flatten()）只有形状
        // 守卫红、这六条全绿，实测过。设备榜不同：它只有那一条聚合查询，探针把取行
        // 改回 flatten() 时这里会红（「设备 榜那一年读不出来…实得 []」），所以设备
        // 这两条是真腿。应用那两条留在这是为了口径齐，别把它当成 flatten 的守卫。
        let mut arms: Vec<(&str, Vec<String>)> = Vec::new();
        for (domain, got) in [
            ("按键", stats_totals(Some(30), Some(tail))),
            ("应用", app_stats_totals(None, Some(tail))),
            ("应用", app_stats_totals(Some(30), Some(tail))),
        ] {
            arms.push((domain, got.unreadable));
        }
        for (domain, got) in [
            ("设备", device_totals(None, Some(tail))),
            ("设备", device_totals(Some(30), Some(tail))),
        ] {
            arms.push((domain, got.unreadable));
        }
        for (domain, gaps) in arms {
            assert_eq!(
                gaps.len(),
                1,
                "{domain} 榜那一年读不出来，必须点名（实得 {gaps:?}）"
            );
        }
    }

    /// 上面那些"库头合法、页坏"夹具的守门人：`rows.flatten()` 一旦回到取行处，
    /// 榜会短一截、趋势全是 0，而交回的数组看起来是"读全了"。
    ///
    /// 圈这几处：三个榜的单库读数、每日／每小时共用的 `read_pairs`、
    /// 设备详情的两条 helper，以及它们都要问的登记表 `device_ids_of_identity_checked`。
    ///
    /// **实测过的"只有这条守得住"**（别把它当冗余删掉）：
    /// - 应用榜的取行 —— 它前面那句 `SUM(seconds)` 与聚合读同一批页，坏页先撞聚合，取行轮不到出声。
    ///   探针实测：把 `flatten()` 折回放回设备榜 ⇒ 行为用例红；放回应用榜 ⇒ 只有这条红。
    ///
    /// 登记表的扫描（`device_ids_of_identity_checked`）原先也只有这一条守着：详情那份夹具只把
    /// `device_counts` 铺到后半段，而 `devices` 就一两行住在合法页里，取行换回 `flatten()` 时
    /// 行为用例照绿（第 212 轮探针实测）。**这一轮它有第二只眼睛了**：
    /// `the_device_kind_question_names_the_year_it_cannot_ask` 把 `devices` 自己铺到后半段，
    /// 同一个探针现在两条一起红（已现量）。形状判据仍留着 —— 它管的是"以后新加一处取行也得
    /// 走这条路"，那不在任何行为夹具的射程里。
    ///
    /// `registered_kind`（类型那一格背后的那一问）本轮也从四道折收进三态，由上面那条行为用例与
    /// `the_device_detail_readers_ask_the_three_state_rulers` 一起守。留在**这条**判据之外的
    /// 只剩 `get_hourly_stats`：它是 `hourly_stats_totals` 的薄包装，自己不再有取行可折，
    /// 归另一条"三个二元组口子必须是薄包装"的判据管。
    #[test]
    fn the_rank_readings_do_not_fold_a_broken_page_into_a_short_list() {
        let src = include_str!("queries.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        for header in [
            "fn query_stats_in_conn(",
            "fn query_apps_in_conn(",
            "fn query_devices_in_conn(",
            "fn read_pairs(",
            "fn hourly_stats_totals(",
            "fn device_ids_of_identity_checked(",
            "fn device_key_rows(",
            "fn device_date_series(",
        ] {
            let at = prod
                .find(header)
                .unwrap_or_else(|| panic!("{header} 找不到了"));
            let raw = &prod[at..at + prod[at..].find("\n}").expect("函数体切不出来")];
            let body: String = raw
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                !body.contains("rows.flatten()"),
                "{header} 又用 flatten() 收行 —— 它会把「读到一半坏了」折成「数据就到这里为止」"
            );
            assert!(
                body.contains("collect::<Result<Vec<_>"),
                "{header} 必须让取行错误变成 Err，而不是凑出一个能用的 Vec"
            );
        }
    }

    /// 每日与每小时那两个单库读数的三态接线：`table_exists` 那把 bool 尺子不许回来。
    ///
    /// 这条是**形状**判据不是行为判据：`Presence::Unreadable` 那一臂要一份"表问得出、
    /// 内容问不出"的库才触发得了，本轮没造出那种夹具（行为腿覆盖的是 NoTable／
    /// 打不开／扫到坏页三臂）。留这条是为了把"问不出＝没有"那次回退挡在编译之外。
    #[test]
    fn the_window_readers_ask_the_three_state_table_ruler() {
        let src = include_str!("queries.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        for header in [
            "fn query_daily_window_in_conn(",
            "fn query_hourly_window_in_conn(",
            "pub fn hourly_stats_totals(",
        ] {
            let at = prod
                .find(header)
                .unwrap_or_else(|| panic!("{header} 找不到了"));
            let raw = &prod[at..at + prod[at..].find("\n}").expect("函数体切不出来")];
            let body: String = raw
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                !body.contains("table_exists("),
                "{header} 又用 bool 那把尺子问表 —— 「问不出」会被折成「没有这张表」"
            );
            assert!(
                body.contains("connection::table_presence_cached(conn,"),
                "{header} 必须问三态那把尺子（读侧走带记忆的那一条）"
            );
            assert!(
                !body.contains("connection::table_presence(conn,"),
                "{header} 回到了不记忆的那一条 ⇒ 每拍为同一个库重复问同一张表\
                 （现量：今日档 10 条、总计档 13 条全是这一问）"
            );
            assert!(
                body.contains("Presence::Unreadable(why) => return YearRead::Unreadable(why)"),
                "{header} 问不出那一臂必须原样交出去，不许落进 NoTable"
            );
        }
    }

    /// 单设备详情那两条 helper 与「类型那一格背后的那一问」都走同一批三态尺子
    /// （形状判据，理由同 `the_window_readers_ask_the_three_state_table_ruler`）。
    ///
    /// 额外钉一条：它们必须问 `device_ids_of_identity_checked`，不许再写一条吞错误的变体
    /// （把"登记表读不动"折成"这台设备在这个库没有 id"，于是整年的明细安静消失，界面念的是
    /// "这台设备最近用得少了"）。那个 `Vec` 版包装本轮已随唯一调用方改走三态而删掉，
    /// 这条判据防的是它被造回来。
    ///
    /// ⚠ 与上一条判据同理，`registered_kind` 的臂各有几只是**行为**覆盖、几只是形状：
    /// 现量过的行为腿是"连库都打不开"与"登记表扫不动"，`table_presence` 问不出那一臂
    /// 与 `QueryReturnedNoRows` 那一臂只有这条形状判据守着（要"表问得出、内容问不出"以外
    /// 的形状才触发，本轮没造那种夹具）。
    #[test]
    fn the_device_detail_readers_ask_the_three_state_rulers() {
        let src = include_str!("queries.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        for header in [
            "fn device_key_rows(",
            "fn device_date_series(",
            "fn registered_kind(",
        ] {
            let at = prod
                .find(header)
                .unwrap_or_else(|| panic!("{header} 找不到了"));
            let raw = &prod[at..at + prod[at..].find("\n}").expect("函数体切不出来")];
            let body: String = raw
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                !body.contains("table_exists("),
                "{header} 又用 bool 那把尺子问表"
            );
            assert!(
                !body.contains("device_ids_of_identity(conn,"),
                "{header} 回到了吞错误的 ids 那一条：登记表读不动会被折成「这台设备这年没记录」"
            );
            assert!(
                body.contains("device_ids_of_identity_checked("),
                "{header} 必须问那条会说出原因的 ids"
            );
            assert!(
                !body.contains("with_ro_conn(&path"),
                "{header} 用只回 None 的那条打开库 —— 「打不开」就没人说了"
            );
            assert!(
                body.contains("with_ro_conn_why("),
                "{header} 必须带走打开失败的原因"
            );
            assert!(
                body.contains("connection::table_presence_cached(conn,"),
                "{header} 问表要走三态那把尺子（读侧走带记忆的那一条），不是 bool"
            );
            assert!(
                !body.contains("connection::table_presence(conn,"),
                "{header} 回到了不记忆的那一条 ⇒ 同一趟里为同一个库重复问同一张表"
            );
            assert!(
                body.contains("Presence::Unreadable(why) => return YearRead::Unreadable(why)"),
                "{header} 问不出那一臂必须原样交出去，不许落进 NoTable（「没这张表」）"
            );
        }
    }

    /// 设备详情那一页必须说得出"这一轮哪几问没答上来"。
    ///
    /// 详情按年份库要跑四趟（按天序列、键名明细、明细标记、排行），这四问以前各折各的：
    /// 读不动时界面交出的是一句关于用户历史的假话，而不是"这次没读全"。
    #[test]
    fn the_device_detail_says_which_readings_are_incomplete() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("q_detail_gaps");
        let this = Local::now().year();
        let key = r"\\?\hid#vid_1234&pid_5678#fake&0000";
        let dk = day_key_of_date(Local::now().date_naive());

        // 好的一年：明细与按天都有；读全了就不许凭空造一句
        {
            let conn = connection::open_rw(&paths::year_db_path(this)).unwrap();
            connection::ensure_schema(&conn, this).unwrap();
            conn.execute(
                "INSERT OR IGNORE INTO devices (id, device_key, name, kind) VALUES (7, ?1, '鼠标', 'mouse')",
                [key],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO device_counts (date_key, device_id, count) VALUES (?1, 7, 40)",
                [dk],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO device_key_counts (date_key, device_id, key_name, count) VALUES (?1, 7, '左键', 25)",
                [dk],
            )
            .unwrap();
            let _ = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
        }
        invalidate_years_cache();
        connection::clear_ro_cache();
        let healthy = get_device_detail(key, 0);
        assert!(
            healthy.unreadable.is_empty(),
            "读全了不该造一句警告，实得 {:?}",
            healthy.unreadable
        );
        assert_eq!(
            healthy.keys,
            vec![("左键".to_string(), 25)],
            "夹具得真被读进来，否则下面那些断言测的是空集"
        );
        assert_eq!(
            healthy.all, 40,
            "按天那份同样要真读进来，实得 {}",
            healthy.all
        );

        // 坏的一年（文件不是 SQLite）：两栏各点一条名，已读到的数不许一起归零
        let broken = this + 1;
        std::fs::write(
            paths::year_db_path(broken),
            b"this is not a sqlite database at all",
        )
        .unwrap();
        invalidate_years_cache();
        connection::clear_ro_cache();
        let detail = get_device_detail(key, 0);
        for domain in ["键名明细", "按天序列"] {
            assert!(
                detail
                    .unreadable
                    .iter()
                    .any(|g| g.contains(domain) && g.contains(&broken.to_string())),
                "{domain} 那一年读不出来必须点名，实得 {:?}",
                detail.unreadable
            );
        }
        assert_eq!(
            detail.keys,
            vec![("左键".to_string(), 25)],
            "坏掉那一年不该把已读到的明细一起吃掉"
        );

        // **库头合法、明细那几页坏**：取行撞上就是 `Err`，不许折成"这台设备的明细就这些"
        let tail = this + 2;
        let tail_path = paths::year_db_path(tail);
        if let Err(e) = std::fs::remove_file(&tail_path) {
            assert!(
                crate::data_location::is_confirmed_absent(&e),
                "换夹具前删不掉这一年的库（不是「本来就没有」）: {e}"
            );
        }
        {
            let conn = connection::open_rw(&tail_path).unwrap();
            connection::ensure_schema(&conn, tail).unwrap();
            conn.execute(
                "INSERT OR IGNORE INTO devices (id, device_key, name, kind) VALUES (7, ?1, '鼠标', 'mouse')",
                [key],
            )
            .unwrap();
            // 别人家的设备把页铺到文件后半段：这一台的行要到 seek 之后才撞坏页
            for i in 0..12_000i64 {
                conn.execute(
                    "INSERT INTO device_counts (date_key, device_id, count) VALUES (1, ?1, 1)",
                    [100 + i],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO device_key_counts (date_key, device_id, key_name, count) VALUES (1, ?1, ?2, 1)",
                    rusqlite::params![100 + i, format!("K{i}")],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO device_counts (date_key, device_id, count) VALUES (?1, 7, 60)",
                [dk],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO device_key_counts (date_key, device_id, key_name, count) VALUES (?1, 7, '右键', 33)",
                [dk],
            )
            .unwrap();
            let _ = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
        }
        connection::clear_ro_cache();
        let mut bytes = std::fs::read(&tail_path).unwrap();
        assert!(
            bytes.len() > 4096 * 40,
            "夹具太小，明细没铺到文件后半段（实得 {} 字节）",
            bytes.len()
        );
        let cut = bytes.len() / 2;
        for b in bytes.iter_mut().skip(cut) {
            *b = 0xA5;
        }
        std::fs::write(&tail_path, &bytes).unwrap();
        invalidate_years_cache();
        connection::clear_ro_cache();
        let half = get_device_detail(key, 0);
        let named: Vec<&String> = half
            .unreadable
            .iter()
            .filter(|g| g.contains(&tail.to_string()))
            .collect();
        assert!(
            named.len() >= 2,
            "{tail} 这一年两栏都该点名（明细标记那一问也算一问），实得 {:?}",
            half.unreadable
        );
        for g in &named {
            assert!(
                !g.contains("打不开"),
                "这一臂是打得开、读到一半坏页，原因别说错：{g}"
            );
        }
    }

    /// 「这台设备的类型问得出吗」也走同一把三态尺子：答不出的那一年要进详情页那份账。
    ///
    /// 后果本来只到「类型」那一格写 `unknown`（名称、次数、别名都不受影响，也不写任何东西），
    /// 所以它排在同族最后一名；但静默才是这一族的病根 —— 旧写法四道各自折：
    /// `with_ro_conn` 折掉打不开、bool 的 `table_exists` 折掉"表问不出"、
    /// `device_ids_of_identity` 折掉"登记表读不动"、`query_row(..).ok()` 折掉"那一行读不动"，
    /// 四道的结局都是"这台设备没登记类型"。
    ///
    /// 四段按「答得出 → 那一年连库都打不开 → 库打得开而登记表扫不动 → 又拿走」排：
    /// 中间两段是区分度，少了它们，"点一名"可能只是整段压根没去问；最后一段证明那句
    /// 不是缓存留着的老话。第三段用的夹具（把这台设备的登记表本身铺到文件后半段）
    /// 正是台账里登记过"造不出"的那一类，这一轮造出来了。
    #[test]
    fn the_device_kind_question_names_the_year_it_cannot_ask() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("q_registered_kind");
        std::fs::create_dir_all(crate::paths::data_dir()).unwrap();
        let this = Local::now().year();
        let key = r"\\?\hid#vid_1a2b&pid_3c4d#fake&0100";
        // 按天那一行放在 400 天前：这一年仍是「有数据的年」，而今日那一档里没有这台设备
        // —— 详情只有在这时才会回退去问登记表（`rank == 0` 就是这条路的现量凭据）。
        let old_dk = day_key_of_date(Local::now().date_naive()) - 400;

        // 一、答得出：类型从登记表来，且不许多造一句
        {
            let conn = connection::open_rw(&paths::year_db_path(this)).unwrap();
            connection::ensure_schema(&conn, this).unwrap();
            conn.execute(
                "INSERT OR IGNORE INTO devices (id, device_key, name, kind) VALUES (9, ?1, '键盘', 'keyboard')",
                [key],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO device_counts (date_key, device_id, count) VALUES (?1, 9, 12)",
                [old_dk],
            )
            .unwrap();
            let _ = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
        }
        invalidate_years_cache();
        connection::clear_ro_cache();
        let (kind, gaps) = registered_kind(key);
        assert_eq!(
            kind.as_deref(),
            Some("keyboard"),
            "夹具要走的就是登记表那条回退，实得 {kind:?} / {gaps:?}"
        );
        assert!(gaps.is_empty(), "答得出就不许造一句，实得 {gaps:?}");
        let detail = get_device_detail(key, -1);
        assert_eq!(
            detail.rank, 0,
            "夹具前提：今日那一档里没有这台设备，否则问不到登记表"
        );
        assert_eq!(detail.kind, "keyboard", "详情页那一格要从登记表来");
        assert!(
            detail.unreadable.is_empty(),
            "读全了不许凭空造一句，实得 {:?}",
            detail.unreadable
        );

        // 二、那一年连库都打不开（同步盘换成占位文件那一类）：类型照旧答得出，但要说出这一年
        let gone = this + 1;
        std::fs::write(
            paths::year_db_path(gone),
            b"this is not a sqlite database at all",
        )
        .unwrap();
        invalidate_years_cache();
        connection::clear_ro_cache();
        let (kind, gaps) = registered_kind(key);
        assert_eq!(
            kind.as_deref(),
            Some("keyboard"),
            "坏一年不许把别年问到的类型一起吃掉"
        );
        assert!(
            gaps.iter().any(|g| g.contains("设备登记表")
                && g.contains(&gone.to_string())
                && g.contains("连打开都办不到")),
            "{gone} 这一年连库都打不开，类型那一问要说出是哪一年问的，实得 {gaps:?}"
        );
        let detail = get_device_detail(key, -1);
        assert!(
            detail
                .unreadable
                .iter()
                .any(|g| g.contains("设备登记表") && g.contains(&gone.to_string())),
            "接线腿：登记表那一问的原因要走进详情页那份账，实得 {:?}",
            detail.unreadable
        );

        // 三、库打得开、**登记表本身**扫不动：把这台设备的登记行铺到文件后半段再涂坏后半段
        let tail = this + 2;
        let tail_path = paths::year_db_path(tail);
        {
            let conn = connection::open_rw(&tail_path).unwrap();
            connection::ensure_schema(&conn, tail).unwrap();
            conn.execute(
                "INSERT OR IGNORE INTO devices (id, device_key, name, kind) VALUES (9, ?1, '键盘', 'keyboard')",
                [key],
            )
            .unwrap();
            // 别人家的登记把 `devices` 这张表推到后半段：扫全表才撞得上坏页
            // （前一轮的详情夹具只把 `device_counts` 铺到后半段，`devices` 就一两行住在合法页里，
            // 所以那一轮这一臂拿不到行为见证）
            for i in 0..12_000i64 {
                conn.execute(
                    "INSERT INTO devices (id, device_key, name, kind) VALUES (?1, ?2, '别家', 'mouse')",
                    rusqlite::params![200 + i, format!(r"\\?\hid#vid_9999&pid_{i}#other&0000")],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO device_counts (date_key, device_id, count) VALUES (?1, 9, 5)",
                [old_dk],
            )
            .unwrap();
            let _ = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
        }
        connection::clear_ro_cache();
        let mut bytes = std::fs::read(&tail_path).unwrap();
        assert!(
            bytes.len() > 4096 * 40,
            "夹具太小，登记表那几页没铺到文件后半段（实得 {} 字节）",
            bytes.len()
        );
        let cut = bytes.len() / 2;
        for b in bytes.iter_mut().skip(cut) {
            *b = 0xA5;
        }
        std::fs::write(&tail_path, &bytes).unwrap();
        invalidate_years_cache();
        connection::clear_ro_cache();
        let (kind, gaps) = registered_kind(key);
        assert_eq!(
            kind.as_deref(),
            Some("keyboard"),
            "别年问到的类型不许被这一年吃掉"
        );
        let tail_gap = gaps
            .iter()
            .find(|g| g.contains("设备登记表") && g.contains(&tail.to_string()))
            .unwrap_or_else(|| panic!("{tail} 这一年登记表扫不动也要点名，实得 {gaps:?}"));
        assert!(
            !tail_gap.contains("打不开"),
            "这一臂是打得开、读到一半坏页，原因别说错：{tail_gap}"
        );
        assert!(
            tail_gap.contains("读登记表失败") || tail_gap.contains("准备登记表的查询失败"),
            "这一臂钉的是「表问得出、内容问不出」那个形状：`sqlite_master` 读得到，\
             扫全表才撞坏页（若红在这里，说明夹具让更早那一问就先失败了），实得 {tail_gap}"
        );

        // 四、把坏的那两年拿走：又回到安静（证明上面那句"点名"不是缓存留着的老话）
        connection::clear_ro_cache();
        std::fs::remove_file(paths::year_db_path(gone))
            .expect("删掉占位那一年（先 clear_ro_cache）");
        std::fs::remove_file(&tail_path).expect("删掉坏页那一年（先 clear_ro_cache）");
        invalidate_years_cache();
        connection::clear_ro_cache();
        let (kind, gaps) = registered_kind(key);
        assert_eq!(kind.as_deref(), Some("keyboard"));
        assert!(
            gaps.is_empty(),
            "坏年份拿走之后回到答得出，gap 不许留着，实得 {gaps:?}"
        );

        // 五、这台设备压根没登记过：那是答案，安静交回 `None`（详情页那一格写 unknown）。
        // 少了这一腿，"凡是问不到就点一名"那种改法照样能让上面四段都绿。
        let (never_kind, never_gaps) = registered_kind(r"\\?\hid#vid_dead&pid_beef#nobody&0000");
        assert_eq!(never_kind, None, "没登记过的设备不该被编出一个类型");
        assert!(
            never_gaps.is_empty(),
            "「确实没登记」不是「问不出」，不许造 gap，实得 {never_gaps:?}"
        );
    }

    /// 单日那一问同一条口径：没这张表＝安静 24 个 0；问不出／打不开／读到坏页＝点名。
    ///
    /// 周报的时段直方图问的就是这一问（`hourly_window_totals` 是跟着顶部周期的另一问）。
    /// 旧写法把四道折全收成 24 个 0，于是"读不动"在文件里长成"这一周夜里什么都没发生"。
    #[test]
    fn the_single_day_hourly_reading_names_the_year_it_cannot_read() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("q_hourly_day_gaps");
        std::fs::create_dir_all(crate::paths::data_dir()).unwrap();
        let today = Local::now().date_naive();
        let year = today.year();
        let dk = day_key_of_date(today);
        let path = paths::year_db_path(year);

        // 一、`hourly_counts` 真没有：安静交回 24 个 0，不许造一句"读不出来"
        {
            let conn = connection::open_rw(&path).unwrap();
            conn.execute_batch("CREATE TABLE unrelated (a INTEGER);")
                .unwrap();
        }
        connection::clear_ro_cache();
        let none = hourly_stats_totals(Some(today));
        assert!(
            none.unreadable.is_empty()
                && none.hours.len() == 24
                && none.hours.iter().all(|v| *v == 0),
            "没这张表是真没有记录，实得 {:?}",
            none.unreadable
        );
        connection::clear_ro_cache();
        std::fs::remove_file(&path).expect("删掉这一年的库（先 clear_ro_cache）");
        // 一之二、**那一年根本没有库**（首启那年、整年没记录）：这也是一个正确答案，
        // 不是"读不出来"。少了这一腿，桌面端那条"没有数据的周要交回 Ok(None)"会被
        // 一份「打不开」的拒绝顶掉 —— 本轮真实撞上过一次。
        let absent = hourly_stats_totals(Some(today));
        assert!(
            absent.unreadable.is_empty() && absent.hours.iter().all(|v| *v == 0),
            "没有这一年的库要安静交回 24 个 0，实得 {:?}",
            absent.unreadable
        );

        // 二、文件根本不是库：那一栏与那一年都要被点名，旧的数组口子在这一支还是念 0
        std::fs::write(&path, b"this is not a sqlite database at all").unwrap();
        connection::clear_ro_cache();
        let broken = hourly_stats_totals(Some(today));
        assert_eq!(
            broken.unreadable.len(),
            1,
            "打不开也要点名，实得 {:?}",
            broken.unreadable
        );
        assert!(
            broken.unreadable[0].contains("当日小时分布")
                && broken.unreadable[0].contains(&year.to_string()),
            "gap 要认出域名与年份，实得 {:?}",
            broken.unreadable[0]
        );
        assert!(
            get_hourly_stats(Some(today)).iter().all(|v| *v == 0),
            "二元组那个旧口子在这一支还是把读不出来念成 0"
        );
        connection::clear_ro_cache();
        std::fs::remove_file(&path).expect("换夹具前要删掉这一年的库（先 clear_ro_cache）");

        // 三、**库头合法、那一天那几页坏**：seek 落在文件后半段才会露头。
        // 先用 12000 个更早的 date_key 把叶子页往后推（它们的键都比今天小，不进这一问的账），
        // 再写今天那两格 —— 这样 `date_key = ?1` 那一问读的正是被写坏的那半段。
        {
            let conn = connection::open_rw(&path).unwrap();
            connection::ensure_schema(&conn, year).unwrap();
            for k in 1..12_000i64 {
                conn.execute(
                    "INSERT INTO hourly_counts (date_key, hour, count) VALUES (?1, 0, 1)",
                    [k],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO hourly_counts (date_key, hour, count) VALUES (?1, 21, 5)",
                [dk],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO hourly_counts (date_key, hour, count) VALUES (?1, 22, 3)",
                [dk],
            )
            .unwrap();
            let _ = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
        }
        connection::clear_ro_cache();
        let healthy = hourly_stats_totals(Some(today));
        assert!(
            healthy.unreadable.is_empty() && healthy.hours[21] == 5 && healthy.hours[22] == 3,
            "夹具得真被读进来，坏页那一腿红才有意义，实得 {:?}",
            healthy.hours
        );
        let mut bytes = std::fs::read(&path).unwrap();
        assert!(
            bytes.len() > 4096 * 40,
            "夹具太小，那一天那几页没铺到文件后半段（实得 {} 字节）",
            bytes.len()
        );
        let cut = bytes.len() / 2;
        for b in bytes.iter_mut().skip(cut) {
            *b = 0xA5;
        }
        std::fs::write(&path, &bytes).unwrap();
        connection::clear_ro_cache();
        let half = hourly_stats_totals(Some(today));
        assert_eq!(
            half.unreadable.len(),
            1,
            "「打得开、扫到坏页」也必须点名（这条测的就是取行那一臂）",
        );
        assert!(
            !half.unreadable[0].contains("打不开"),
            "这一臂不是打不开，原因别说错：{}",
            half.unreadable[0]
        );
        assert!(
            half.hours.iter().all(|v| *v == 0),
            "读不全时 24 个格子只是下界，不许把读到的半格当结论"
        );
    }

    /// 「那一年根本没有库」是一个**答案**，不是"读不出来"：五处读数都要安静交回 0 而不造 gap。
    ///
    /// 这条腿守的是本轮真实制造过的那个缺陷：逐日问榜时把"没有 2020 年的库"说成读不出来，
    /// 于是周报拒绝生成一份本来正常的文件。三小段按"没有 → 有但读不动 → 又拿走"排，
    /// 中间那段是区分度：少了它，"安静"可能只是整段根本没去读。
    #[test]
    fn a_missing_year_db_is_an_answer_not_a_refusal() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("q_absent_answers");
        std::fs::create_dir_all(crate::paths::data_dir()).unwrap();
        let today = Local::now().date_naive();
        let gone = today.year() + 7;
        let gone_day = chrono::NaiveDate::from_ymd_opt(gone, 6, 15).unwrap();
        let path = paths::year_db_path(gone);
        connection::clear_ro_cache();

        // 一、文件不存在：四处都安静
        assert!(!path.exists(), "夹具前提：这一年不该有库");
        let keys = stats_totals(None, Some(gone));
        let apps = app_stats_totals(None, Some(gone));
        let devices = device_totals(None, Some(gone));
        let daily = daily_counts_totals(30, Some(gone));
        let hours = hourly_stats_totals(Some(gone_day));
        assert!(
            keys.unreadable.is_empty()
                && apps.unreadable.is_empty()
                && devices.unreadable.is_empty()
                && daily.unreadable.is_empty()
                && hours.unreadable.is_empty(),
            "没有那一年库要被当成答案，实得 {:?} / {:?} / {:?}",
            keys.unreadable,
            daily.unreadable,
            hours.unreadable
        );
        assert_eq!(keys.total, 0);
        assert_eq!(daily.series.len(), 30, "缺库那一年仍然交回稠密的 30 格");
        assert!(daily.series.iter().all(|(_, c)| *c == 0));
        assert!(hours.hours.iter().all(|v| *v == 0));

        // 二、同一路径换成一份不是 SQLite 的文件：同一批读数必须立刻改口
        std::fs::write(&path, b"this is not a sqlite database at all").unwrap();
        connection::clear_ro_cache();
        for (domain, got) in [
            ("按键统计", stats_totals(None, Some(gone)).unreadable),
            ("应用时长", app_stats_totals(None, Some(gone)).unreadable),
            ("设备统计", device_totals(None, Some(gone)).unreadable),
            ("每日趋势", daily_counts_totals(30, Some(gone)).unreadable),
            (
                "当日小时分布",
                hourly_stats_totals(Some(gone_day)).unreadable,
            ),
        ] {
            assert!(
                got.iter()
                    .any(|g| g.contains(domain) && g.contains(&gone.to_string())),
                "{domain} 在文件存在却读不动时要点名，实得 {got:?}"
            );
        }

        // 三、再把文件拿走：又要安静回去（证明上面那个"安静"不是缓存或一次性跳过）
        connection::clear_ro_cache();
        std::fs::remove_file(&path).expect("删掉这一年的库（先 clear_ro_cache）");
        assert!(
            hourly_stats_totals(Some(gone_day)).unreadable.is_empty()
                && stats_totals(None, Some(gone)).unreadable.is_empty(),
            "文件消失后回到「确认没有」，gap 不许留着"
        );
    }

    /// 「说不出这一年的库在不在」不许被念成「那一年没有库」：这一臂第一次拿到真实
    /// 系统调用下的行为见证（此前只有形状判据与按错误码钉的那张映射表守着）。
    ///
    /// 夹具形状是把这一年的库位置做成**自指目录 junction**。本机现量：
    /// `std::fs::metadata` 对它报 os error 1921（`FilesystemLoop`），既不是"在"也不是
    /// `NotFound`，三态那把尺子于是只剩「说不出」这一条路。上一版这里登记成"无头造不出、
    /// 只能眼验"，理由是"要 ACL 拒绝或杀软握着句柄"——两条都不必：`mklink /J` 不要管理员
    /// （符号链接要，本机实测 os error 1314 拒建），而这一臂要的只是"stat 回来的码不是 2/3"。
    /// ⚠ 同一次现量还量出一条与本用例无关的读数：UNC 死共享（os error 67/53）与不存在的
    /// 盘符（os error 3）在本机都报 **NotFound** ⇒ "网络位置整个够不着"落在的是
    /// 「确认没有」那一臂，不是这一臂。那条另记台账，别以为这条用例把它也守住了。
    #[cfg(windows)]
    #[test]
    fn an_unanswerable_presence_question_is_not_read_as_no_database() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("q_presence_unanswerable");
        std::fs::create_dir_all(crate::paths::data_dir()).unwrap();
        let today = Local::now().date_naive();
        let gone = today.year() + 7;
        let gone_day = chrono::NaiveDate::from_ymd_opt(gone, 6, 15).unwrap();
        let path = paths::year_db_path(gone);

        // 夹具：自指 junction（工装与预热／归档那两条眼验账共用 `paths::test_self_loop_junction`）。
        // 建不上就跳过并出声 —— 那是这台机器的环境限制，不是产品的失败；静默跳过会把这一臂
        // 重新变回没人守。
        if !paths::test_self_loop_junction(&path) {
            eprintln!("mklink /J 建不出自指 junction，跳过（夹具做不出来）");
            return;
        }
        // 断言失败也要摘掉这个重解析点：留着它，用例收尾删的是链接而不是我们建的那格
        struct JunctionGuard<'a>(&'a std::path::Path);
        impl Drop for JunctionGuard<'_> {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir(self.0);
            }
        }
        let _loop = JunctionGuard(&path);
        connection::clear_ro_cache();

        // 一、夹具前提：这一问在真实 stat 之下确实"说不出"。少了这条腿，下面每一腿
        // 都可能落在"stat 其实成功了"的库上空跑，而读起来照样一片绿。
        let presence = year_db_presence(&path);
        assert!(
            presence.is_err(),
            "自指 junction 的 stat 该报错，实得 {presence:?} —— 夹具没生效，这条用例是空跑"
        );
        let stat_err = presence.unwrap_err();
        assert!(
            !crate::data_location::is_confirmed_absent(&stat_err),
            "stat 报回来的码（{:?}）不许被读成「确认没有」",
            stat_err.kind()
        );

        // 二、闸本身走的是第三条路：既不是「确认没有」也不是「确认在」
        match gate_year(gone, &path, "按键统计") {
            YearGate::CannotTell(why) => assert!(
                why.contains("说不出这一年的库在不在"),
                "原因要说的是「在不在问不出」，实得 {why}"
            ),
            YearGate::SkipAbsent => panic!("「问不出在不在」被折成了「那一年没有库」"),
            YearGate::Read => panic!("这一臂没被真实 stat 触发，夹具的形状变了"),
        }

        // 三、四个调用方各自把它拼成点名（`gate_year` 的四个调用点一处处过）
        for (domain, gaps) in [
            ("按键统计", stats_totals(None, Some(gone)).unreadable),
            ("设备统计", device_totals(None, Some(gone)).unreadable),
            ("每日趋势", daily_counts_totals(30, Some(gone)).unreadable),
            (
                "当日小时分布",
                hourly_stats_totals(Some(gone_day)).unreadable,
            ),
        ] {
            assert_eq!(gaps.len(), 1, "{domain} 该只点这一年的名，实得 {gaps:?}");
            assert!(
                gaps[0].contains(domain)
                    && gaps[0].contains(&gone.to_string())
                    && gaps[0].contains("说不出这一年的库在不在"),
                "{domain} 的 gap 要说清域名、年份与「在不在问不出」，实得 {}",
                gaps[0]
            );
            assert!(
                !gaps[0].contains("打不开"),
                "这一臂连库都没去开，原因别说成打不开：{}",
                gaps[0]
            );
        }

        // 四、数字那头照旧交回 0：把这一轮与"那一年真没数据"分开的只有上面那条 gap，
        // 所以断言落在"0 与 gap 同时在场"，不是落在 0 本身。
        assert_eq!(stats_totals(None, Some(gone)).total, 0);
        assert!(
            daily_counts_totals(30, Some(gone))
                .series
                .iter()
                .all(|(_, c)| *c == 0),
            "说不出在不在那一年，趋势仍交回稠密全零——区分它的是 gap，不是形状"
        );
    }

    /// 设备维度同一条口径：`device_counts` 没这张表 ⇒ 安静空榜；读不出来 ⇒ 点名。
    #[test]
    fn device_totals_says_which_year_it_cannot_read() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("q_device_gaps");
        let year = Local::now().year();
        let path = paths::year_db_path(year);

        {
            let conn = connection::open_rw(&path).unwrap();
            conn.execute_batch("CREATE TABLE unrelated (a INTEGER);")
                .unwrap();
        }
        let none = device_totals(None, Some(year));
        assert!(
            none.devices.is_empty() && none.unreadable.is_empty(),
            "没有 `device_counts` 是真没有数据，实得 {:?}",
            none.unreadable
        );
        connection::clear_ro_cache();
        std::fs::remove_file(&path).expect("删掉这一年的库（先 clear_ro_cache）");

        std::fs::write(&path, b"this is not a sqlite database at all").unwrap();
        let broken = device_totals(None, Some(year));
        assert!(
            broken
                .unreadable
                .iter()
                .any(|g| g.contains("设备统计") && g.contains(&year.to_string())),
            "读不出来的那一年必须点名，实得 {:?}",
            broken.unreadable
        );
        assert_eq!(
            broken.total, 0,
            "旧写法在这里交回 0 台 / 0 次，与「今天没插设备」同形"
        );
    }

    /// 每日趋势与每小时分布同一条口径：没这张表＝安静按 0；问不出／打不开／读到一半坏页＝点名。
    ///
    /// 这一支以前有四道各自为政的折（`year_db_presence` 说不出只留一行日志、`with_ro_conn`
    /// 把打不开折成 `None`、`table_exists` 把问不出折成"没有"、`rows.flatten()` 把坏页折成
    /// "到这儿为止"），面板那两条曲线与导出那两段切面都把"读不出来"画成了 0。
    #[test]
    fn the_daily_and_hourly_readings_name_the_year_they_cannot_read() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("q_window_gaps");
        std::fs::create_dir_all(crate::paths::data_dir()).unwrap();
        let year = Local::now().year();
        let path = paths::year_db_path(year);
        let dk = day_key_of_date(Local::now().date_naive());

        // 一、两张表都真没有：安静按 0，不许凭空造一句"读不出来"
        {
            let conn = connection::open_rw(&path).unwrap();
            conn.execute_batch("CREATE TABLE unrelated (a INTEGER);")
                .unwrap();
        }
        connection::clear_ro_cache();
        let none = daily_counts_totals(7, Some(year));
        assert!(
            none.unreadable.is_empty() && none.series.len() == 7,
            "没这张表是真没有数据，实得 {:?}",
            none.unreadable
        );
        let none_hourly = hourly_window_totals(7, Some(year));
        assert!(
            none_hourly.unreadable.is_empty() && none_hourly.hours.iter().all(|v| *v == 0),
            "每小时那一栏同理，实得 {:?}",
            none_hourly.unreadable
        );
        connection::clear_ro_cache();
        std::fs::remove_file(&path).expect("删掉这一年的库（先 clear_ro_cache）");

        // 二、文件根本不是库（同步盘占位、移动盘没醒那一类）：两个域都要说出是哪一年
        std::fs::write(&path, b"this is not a sqlite database at all").unwrap();
        connection::clear_ro_cache();
        for (domain, gaps) in [
            ("每日趋势", daily_counts_totals(7, Some(year)).unreadable),
            ("每小时分布", hourly_window_totals(7, Some(year)).unreadable),
        ] {
            assert_eq!(
                gaps.len(),
                1,
                "{domain} 那一年读不出来该只点一条名，实得 {gaps:?}"
            );
            assert!(
                gaps[0].contains(domain) && gaps[0].contains(&year.to_string()),
                "gap 要能认出域名与年份，实得 {:?}",
                gaps[0]
            );
        }
        // 二元组那个旧口子照旧交回全零数组 —— 它分不出这两件事，所以面板不许再用它
        assert!(
            get_daily_counts(7, Some(year)).iter().all(|(_, c)| *c == 0),
            "旧口子在这一支还是把读不出来念成 0"
        );
        connection::clear_ro_cache();
        std::fs::remove_file(&path).expect("换夹具前要删掉这一年的库（先 clear_ro_cache）");

        // 三、**库头合法、窗口那几页坏**：`open_ro` 打得开，是扫描那一步失败。
        // 先把窗口要读的那几行放在文件的**后半段**（填 12000 行小 date_key 把页往前推，
        // 再写今天这一行），否则 range seek 只碰前半段，坏页永远轮不到出声（假绿）。
        {
            let conn = connection::open_rw(&path).unwrap();
            connection::ensure_schema(&conn, year).unwrap();
            for k in 1..12_000i64 {
                conn.execute(
                    "INSERT INTO daily_counts (date_key, count, seconds) VALUES (?1, 1, 1)",
                    [k],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO hourly_counts (date_key, hour, count) VALUES (?1, 8, 1)",
                    [k],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO daily_counts (date_key, count, seconds) VALUES (?1, 77, 60)",
                [dk],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO hourly_counts (date_key, hour, count) VALUES (?1, 21, 5)",
                [dk],
            )
            .unwrap();
            let _ = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
        }
        connection::clear_ro_cache();
        // 夹具自检（改前先跑）：库好的时候这两笔数确实读得出来，坏页那一腿红才有意义
        let healthy = daily_counts_totals(7, Some(year));
        assert!(
            healthy.unreadable.is_empty() && healthy.series.last().map(|(_, c)| *c) == Some(77),
            "夹具没被读进来说不出回归： {:?}",
            healthy.series.last()
        );
        assert_eq!(
            hourly_window_totals(7, Some(year)).hours[21],
            5,
            "每小时那份夹具同样要真被读进来"
        );
        let mut bytes = std::fs::read(&path).unwrap();
        assert!(
            bytes.len() > 4096 * 40,
            "夹具太小，窗口那几行没铺到文件后半段（实得 {} 字节）",
            bytes.len()
        );
        let cut = bytes.len() / 2;
        for b in bytes.iter_mut().skip(cut) {
            *b = 0xA5;
        }
        std::fs::write(&path, &bytes).unwrap();
        connection::clear_ro_cache();
        let broken = daily_counts_totals(7, Some(year));
        assert_eq!(
            broken.unreadable.len(),
            1,
            "「打得开、扫到坏页」也必须点名（这条测的就是取行那一臂）",
        );
        assert!(
            !broken.unreadable[0].contains("打不开"),
            "这一臂不是打不开，原因别说错：{}",
            broken.unreadable[0]
        );
        assert!(
            broken.series.iter().all(|(_, c)| *c == 0),
            "读不全时数组只是下界，不许把窗口里那半截行算成数"
        );
        let broken_hourly = hourly_window_totals(7, Some(year));
        assert_eq!(
            broken_hourly.unreadable.len(),
            1,
            "每小时那一栏同一条口径，实得 {:?}",
            broken_hourly.unreadable
        );
    }

    /// 窗口内的每小时合计：只数窗口里那几天，越界的小时值不进任何一格。
    #[test]
    fn hourly_totals_sum_the_window_and_ignore_out_of_range_hours() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("q_hourly_totals");
        let year = Local::now().year();
        let today = Local::now().date_naive();
        {
            let conn = connection::open_rw(&paths::year_db_path(year)).unwrap();
            connection::ensure_schema(&conn, year).unwrap();
            let dk = day_key_of_date(today);
            // 窗口内（今天、3 天前）与窗口外（40 天前）各一条同一小时的记录：
            // 上界写错就是把 40 天前那笔也加进来
            conn.execute(
                "INSERT INTO hourly_counts (date_key, hour, count) VALUES (?1, 10, 5)",
                [dk],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO hourly_counts (date_key, hour, count) VALUES (?1, 10, 7)",
                [dk - 3],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO hourly_counts (date_key, hour, count) VALUES (?1, 10, 9)",
                [dk - 40],
            )
            .unwrap();
            // 越界的小时值历史上真存过：不能进账，也不能把 24 格撑爆
            conn.execute(
                "INSERT INTO hourly_counts (date_key, hour, count) VALUES (?1, 99, 1000)",
                [dk],
            )
            .unwrap();
        }
        invalidate_years_cache();
        connection::clear_ro_cache();

        let win = get_hourly_totals(4, None);
        assert_eq!(win.len(), 24, "交回的永远是 24 格");
        assert_eq!(win[10], 12, "窗口内那两天要相加，实得 {}", win[10]);
        assert_eq!(
            win.iter().sum::<i64>(),
            12,
            "越界的小时值一分都不许进账，实得 {win:?}"
        );

        // 窗口拉到 60 天，40 天前那笔才算数（这条防的是"永远只数今天"那种修法）
        let wide = get_hourly_totals(60, None);
        assert_eq!(
            wide[10], 21,
            "近 60 天要含上 40 天前那笔，实得 {}",
            wide[10]
        );
    }

    /// 排名尺子只许有一份：本文件那两处榜必须走 `crate::format::cmp_rank_desc`，
    /// 不许再各自手写"次数降序、同分按名字升序"。
    ///
    /// 为什么值得钉：`cmp_rank_desc` 抽出来防的正是"同一把尺子抄两处就会分叉"，而分叉的
    /// 症状是**谁进榜**都变（榜的输入是 HashMap，排序只比次数时稳定排序把随机原样留下，
    /// 后面紧跟着 `truncate`）。少一份手抄，就少一处没人认领的分叉。
    #[test]
    fn the_core_boards_use_the_one_comparator() {
        let src = include_str!("queries.rs");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod = &src[..cut];
        // 针要拼出来，否则这条用例自己的字面量会被它正在查的样式数进去。
        let hand_tuple = [".cmp(&a.", "1).then_with"].concat();
        let hand_struct = [".cmp(&a.count)", ".then_with"].concat();
        let wired = ["cmp_rank_desc", "("].concat();
        assert!(
            !prod.contains(hand_tuple.as_str()) && !prod.contains(hand_struct.as_str()),
            "core 里又手写了一遍排名尺子 ⇒ 它与 cmp_rank_desc 分叉时没有任何用例会红"
        );
        assert_eq!(
            prod.matches(wired.as_str()).count(),
            2,
            "设备榜那一列与设备详情那一列都要走那一份尺子，数量变了说明接线动过"
        );
    }
}
