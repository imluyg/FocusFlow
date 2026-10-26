//! 只读数据快照：按当前统计周期算一屏要显示的数。
//!
//! 为什么是"快照"而不是让界面直接查库：真实版本里统计在后台线程算、算完推给界面，
//! 界面永远只读一份已算好的数。原型照这个形状来，P2 换过去时不用重写视图层。
//!
//! 读库走 `Database::init_readonly()`（CLI 用的那条），所以**不会**启动写入线程、
//! 不会碰正在运行的采集进程的写队列；`FOCUSFLOW_APP_DIR` 指向哪就读哪。

use focusflow_core::db::queries;
use focusflow_core::stats::{goal_status, GoalStatus, GOAL_LOOKBACK_DAYS};

/// 设备行。`auto_name` / `has_alias` 是 P2「还原」与「原名副标题」要用的，
/// 这一轮只做改名，先留着字段。
#[allow(dead_code)]
#[derive(Default, Clone, Debug)]
pub struct DevRow {
    pub key: String,
    pub name: String,
    pub auto_name: String,
    pub has_alias: bool,
    pub kind: String,
    pub count: i64,
}

/// 高级设置里的一行。`kind` 是控件类型（0 开关 / 1 整数 / 2 小数 / 3 文本），
/// 由这一侧说出，界面不许自己猜"这个键是不是布尔"。
#[derive(Clone, Debug)]
pub struct SettingRow {
    pub section: &'static str,
    pub key: &'static str,
    pub kind: u8,
    pub live: &'static str,
    pub value: String,
    pub default: String,
}

/// 「高级设置」这一页的唯一清单（与 `desktop/src/commands.rs` 的 `EXPOSED_SETTINGS`
/// 同一份口径：段、键、类型、什么时候生效）。
///
/// P7 之后这张表会搬进 `focusflow-core`，让两套界面（旧 Tauri / 新原生）读同一份；
/// 现在原生端先照抄一遍，逐项与那条清单对齐。
const SETTINGS: &[(&str, &str, u8, &str)] = &[
    ("goal", "daily_keys", 1, "改完立即生效"),
    ("listener", "ignore_modifier_keys", 0, "改完立即生效"),
    ("listener", "ignore_function_keys", 0, "改完立即生效"),
    ("listener", "ignore_key_repeat", 0, "改完立即生效"),
    ("listener", "key_repeat_stale_seconds", 2, "改完立即生效"),
    ("listener", "mouse_enabled", 0, "改完立即生效"),
    ("listener", "scroll_burst_window", 2, "改完立即生效"),
    ("rest", "enabled", 0, "改完立即生效"),
    ("rest", "window_minutes", 1, "改完立即生效"),
    ("rest", "key_threshold", 1, "改完立即生效"),
    ("rest", "cooldown_minutes", 1, "改完立即生效"),
    ("rest", "rest_seconds", 1, "改完立即生效"),
    ("rest", "check_interval", 1, "改完立即生效"),
    ("gui", "active_refresh_interval", 1, "改完立即生效"),
    ("gui", "full_refresh_interval", 1, "改完立即生效"),
    ("database", "max_backups", 1, "改完立即生效"),
    ("database", "backup_disabled_plugins", 0, "改完立即生效"),
    ("app_stats", "enabled", 0, "改完立即生效"),
    ("app_stats", "exclude", 3, "改完立即生效"),
    ("gui", "unload_hidden", 0, "下一次隐藏主窗口起效"),
    ("gui", "unload_hidden_delay", 1, "下一次隐藏主窗口起效"),
    ("gui", "start_to_tray", 0, "重启程序后生效"),
    ("gui", "floating_renderer", 3, "重启程序后生效"),
    ("database", "auto_vacuum_days", 1, "重启程序后生效"),
    ("device_stats", "enabled", 0, "重启程序后生效"),
];

fn load_settings() -> Vec<SettingRow> {
    let c = focusflow_core::config::instance();
    let defaults = focusflow_core::config::default_config();
    SETTINGS
        .iter()
        .map(|&(section, key, kind, live)| {
            let d = defaults
                .get(section)
                .and_then(|s| s.get(key))
                .cloned()
                .unwrap_or_default();
            // 布尔一律走 `get_bool`：手改 config.ini 写成 `1` / `yes` / `on` 时功能确实是
            // 开的（读者就是 `get_bool`），拿字符串比 "==true" 会画出一个灭勾。
            let value = if kind == 0 {
                c.get_bool(section, key, d.parse::<bool>().unwrap_or(false))
                    .to_string()
            } else {
                c.get_or(section, key, &d)
            };
            SettingRow {
                section,
                key,
                kind,
                live,
                value,
                default: d,
            }
        })
        .collect()
}

#[derive(Default, Clone, Debug)]
pub struct Snapshot {
    /// 当前周期（-1 今日 / 0 全部 / n 近 n 天）。
    pub period: i64,
    /// 今日次数（不受周期影响）。
    pub today: i64,
    /// 今日前台应用总时长（秒）。悬浮窗的「时长」那一格用（`[gui] floating_metric`
    /// 取 `duration` / `both` 时），与读数卡片的「今日活跃」是同一份快照。
    pub today_app_seconds: i64,
    /// 当前周期的键鼠总数。
    pub period_total: i64,
    /// 当前周期的日均（按窗口天数算）。
    pub avg: i64,
    /// 全历史最高单日。
    pub max_day: Option<(String, i64)>,
    pub rank: Vec<(String, i64)>,
    /// 前台应用时长（当前周期），按秒数降序。
    pub apps: Vec<(String, i64)>,
    pub app_total: i64,
    pub devices: Vec<DevRow>,
    /// 近 30 天每日次数（固定窗口，画趋势用）。
    pub daily: Vec<i64>,
    /// 当前周期内每小时的合计（今日 = 今天 24 小时）。
    pub hourly: Vec<i64>,
    /// 星期分布（0=周一 … 6=周日），跟随周期窗口。
    pub weekday: [i64; 7],
    /// 每日目标与连续打卡。`None` = 这一轮没算出来（页面上要说明）。
    pub goal: Option<GoalStatus>,
    /// 高级设置的行（值 + 类型 + 生效时机）。
    pub settings: Vec<SettingRow>,
    /// 开机自启：**现读**启动文件夹，不读配置键（用户在资源管理器里删掉它时，
    /// 配置里还写着开）。正式版每次进设置页读一次，探针跟着 2 秒刷新读。
    pub autostart: bool,
    /// 这一轮没读上来的年度库条数（非 0 = 上面那些数是**下界**）。
    pub gaps: usize,
    pub app_dir: String,
}

/// 周期 → 查询窗口天数：`None` = 全部历史。
fn window_days(period: i64) -> Option<i64> {
    match period {
        0 => None,
        -1 => Some(1),
        n if n > 0 => Some(n),
        _ => Some(7),
    }
}

pub fn load(period: i64) -> Snapshot {
    let days = window_days(period);
    // 采集开着时，写线程内存里那批**还没落库**的按键也要算进来：`queries::*` 读的是
    // 已落库的部分，不加这一步「今日」会比输入慢一个 `flush_interval`（默认 10 秒）。
    // `today_pending_count()` 在落库之后自己会减，所以 库值 + 增量 不会重复计。
    // 采集没开时它恒为 0（没有写线程）。
    let pending = crate::collect::pending_today();
    // 按周期查：键鼠排行、应用时长、设备次数都跟同一把尺子。
    let stats = queries::stats_totals(days, None);
    let all = queries::get_alltime_summary();
    // 趋势图固定近 30 天（今日只有 1 个点，画不出形状）。
    let daily = queries::get_daily_counts(30, None);
    // 小时图跟周期：今日就是今天 24 根；一周就是这周每天的同一小时合计。
    let hourly = queries::get_hourly_totals(days.unwrap_or(30), None);
    // 星期分布同样跟周期；"全部"按近 30 天（与小时图同一口径）。
    let mut weekday = [0i64; 7];
    for (k, v) in queries::get_weekday_stats(days.unwrap_or(30)) {
        if (0..7).contains(&k) {
            weekday[k as usize] = v;
        }
    }

    let mut rank: Vec<(String, i64)> = stats.rows.into_iter().collect();
    // 次数降序；次数相同按名字升序，保证两次刷新之间不会自己换位。
    rank.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rank.truncate(100);

    // 日均的分母：窗口天数；"全部"按有数据的天数（与卡片上的"日均"同口径）。
    let avg_days = match days {
        Some(d) => d.max(1),
        None => daily.iter().map(|(_, v)| *v).filter(|v| *v > 0).count().max(1) as i64,
    };
    let avg = (stats.total + pending) / avg_days;

    let mut hours = hourly;
    hours.resize(24, 0);

    let (_, device_list) = queries::get_device_stats(days, None);
    let devices: Vec<DevRow> = device_list
        .into_iter()
        .map(|d| DevRow {
            key: d.key,
            name: d.name,
            auto_name: d.auto_name,
            has_alias: d.has_alias,
            kind: d.kind,
            count: d.count,
        })
        .collect();

    let (app_total, app_map) = queries::get_app_stats(days, None);
    let mut apps: Vec<(String, i64)> = app_map.into_iter().collect();
    apps.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    apps.truncate(100);

    // 每日目标与连续打卡：要扫近 370 天按日序列。正式版按 60 秒节流，
    // 原型跟着 2 秒刷新一起算（探针库小，先不拆）。
    let goal_cfg = focusflow_core::config::instance().get_int("goal", "daily_keys", 20000);
    let goal_rows = queries::get_daily_counts(GOAL_LOOKBACK_DAYS, None);
    let today_str = chrono::Local::now().format("%Y-%m-%d").to_string();
    let goal = Some(goal_status(goal_cfg, &goal_rows, &today_str, 0));

    // 采集开着时，今日直接用写线程缓存：它含未落库与在途增量，落库前后都不跳
    // （用「库值 + pending」会在 flush 取走批次、尚未提交的那一瞬读到 0）。
    let today = match crate::collect::today_total() {
        Some(n) => n,
        None => queries::get_today_count(None),
    };
    // 今日前台时长：与下面应用排行同一个口径，只是窗口固定成 1 天（悬浮窗要的是"今天"）。
    let today_app_seconds = queries::get_app_stats(Some(1), None).0;

    Snapshot {
        period,
        today,
        today_app_seconds,
        period_total: stats.total + pending,
        avg,
        max_day: all.max_day,
        rank,
        apps,
        app_total,
        devices,
        daily: daily.into_iter().map(|(_, v)| v).collect(),
        hourly: hours,
        weekday,
        goal,
        settings: load_settings(),
        autostart: focusflow_core::autostart::is_autostart_enabled(),
        gaps: stats.unreadable.len() + all.unreadable.len(),
        app_dir: focusflow_core::paths::app_dir().display().to_string(),
    }
}
