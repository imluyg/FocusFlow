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
///
/// `label` / `help` 是给人看的：界面上不再直接摆 `listener.ignore_function_keys`
/// 这种只有作者才懂的键名（2026-10-27 用户反馈"这些都是什么，看不懂"）。
#[derive(Clone, Debug)]
pub struct SettingRow {
    pub section: &'static str,
    pub key: &'static str,
    pub kind: u8,
    pub live: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub value: String,
    pub default: String,
}

/// 「高级设置」这一页的唯一清单：段、键、类型、什么时候生效全在这里。旧桌面端另有一份
/// `EXPOSED_SETTINGS`，两边靠人手对齐；那一页随 WebView 删了，所以现在只剩这一份。
///
/// 元组：段、键、类型、生效时机、中文名、一句话说明（说明以「分组 ·」开头，替掉原来
/// 每行都一样的"改完立即生效"）。`gui.floating_renderer` 这类原生端根本不读的旧键
/// 不再暴露。
const SETTINGS: &[(&str, &str, u8, &str, &str, &str)] = &[
    (
        "goal",
        "daily_keys",
        1,
        "改完立即生效",
        "每日目标",
        "目标 · 今天的活跃次数目标，主界面目标条用它",
    ),
    (
        "hotkey",
        "toggle_window",
        3,
        "改完立即生效",
        "唤出窗口热键",
        "热键 · 按下它把主窗口带到前台，如 ctrl+shift+f；留空 = 恢复默认",
    ),
    (
        "hotkey",
        "snip",
        3,
        "改完立即生效",
        "区域截图热键",
        "热键 · 按下它框选后松手即存（PNG + 剪贴板），如 shift+f1；两条热键不能相同",
    ),
    (
        "hotkey",
        "snip_annotate",
        3,
        "改完立即生效",
        "标注截图热键",
        "热键 · 按下它框选后进标注窗（方框/箭头/文字），如 shift+f2",
    ),
    (
        "listener",
        "ignore_modifier_keys",
        0,
        "改完立即生效",
        "忽略修饰键",
        "键鼠 · Ctrl/Shift/Alt/Win 本身不计入统计",
    ),
    (
        "listener",
        "ignore_function_keys",
        0,
        "改完立即生效",
        "忽略功能键",
        "键鼠 · F1–F12、Insert 等功能键不计入",
    ),
    (
        "listener",
        "ignore_key_repeat",
        0,
        "改完立即生效",
        "过滤长按重复",
        "键鼠 · 按住不放只算第一次",
    ),
    (
        "listener",
        "key_repeat_stale_seconds",
        2,
        "改完立即生效",
        "重复判定（秒）",
        "键鼠 · 同一键多久没抬起就当新的一次",
    ),
    (
        "listener",
        "mouse_enabled",
        0,
        "改完立即生效",
        "统计鼠标",
        "键鼠 · 鼠标点击与滚轮也计入活跃",
    ),
    (
        "listener",
        "scroll_burst_window",
        2,
        "改完立即生效",
        "滚轮合并（秒）",
        "键鼠 · 连续滚动只记一批，防止一滚刷几百次",
    ),
    (
        "rest",
        "enabled",
        0,
        "改完立即生效",
        "休息提醒",
        "休息 · 连续用键鼠太久时提醒你歇一会儿",
    ),
    (
        "rest",
        "window_minutes",
        1,
        "改完立即生效",
        "提醒窗口（分钟）",
        "休息 · 在多长时间内累计输入量",
    ),
    (
        "rest",
        "key_threshold",
        1,
        "改完立即生效",
        "触发阈值（次）",
        "休息 · 窗口内输入超过这个次数就提醒",
    ),
    (
        "rest",
        "cooldown_minutes",
        1,
        "改完立即生效",
        "提醒间隔（分钟）",
        "休息 · 两次提醒之间至少隔多久",
    ),
    (
        "rest",
        "rest_seconds",
        1,
        "改完立即生效",
        "建议休息（秒）",
        "休息 · 提醒后建议离开屏幕的时间",
    ),
    (
        "rest",
        "check_interval",
        1,
        "改完立即生效",
        "检查间隔（分钟）",
        "休息 · 多久检查一次要不要提醒",
    ),
    (
        "gui",
        "active_refresh_interval",
        1,
        "改完立即生效",
        "活跃刷新（秒）",
        "界面 · 今日/速度卡片多久刷新一次",
    ),
    (
        "gui",
        "full_refresh_interval",
        1,
        "改完立即生效",
        "全量刷新（秒）",
        "界面 · 排行与图表多久重新读库",
    ),
    (
        "database",
        "max_backups",
        1,
        "改完立即生效",
        "备份保留份数",
        "数据 · 自动备份最多保留几个",
    ),
    (
        "database",
        "backup_on_exit",
        0,
        "改完立即生效",
        "退出时自动备份",
        "数据 · 每次正常退出前给各类库各存一份快照",
    ),
    (
        "database",
        "online_backup_interval_hours",
        1,
        "改完立即生效",
        "定时备份间隔（小时）",
        "数据 · 运行中每隔几小时备份一次；0 = 关闭",
    ),
    (
        "database",
        "backup_disabled_plugins",
        0,
        "改完立即生效",
        "备份插件数据",
        "数据 · 把插件数据库也一起备份",
    ),
    (
        "app_stats",
        "enabled",
        0,
        "改完立即生效",
        "应用时长统计",
        "应用 · 记录前台应用的使用时长",
    ),
    (
        "app_stats",
        "exclude",
        3,
        "改完立即生效",
        "不统计的应用",
        "应用 · 进程名逗号分隔，如 chrome.exe,code.exe",
    ),
    (
        "gui",
        "unload_hidden",
        0,
        "下一次隐藏主窗口起效",
        "隐藏后释放内存",
        "界面 · 收起一段时间后卸载界面缓存",
    ),
    (
        "gui",
        "unload_hidden_delay",
        1,
        "下一次隐藏主窗口起效",
        "释放延迟（秒）",
        "界面 · 收起后多久释放",
    ),
    (
        "gui",
        "start_to_tray",
        0,
        "重启程序后生效",
        "启动即进托盘",
        "界面 · 开机启动不弹窗，只留托盘图标",
    ),
    (
        "database",
        "auto_vacuum_days",
        1,
        "重启程序后生效",
        "自动压缩（天）",
        "数据 · 每隔几天重建数据库回收空间",
    ),
    (
        "device_stats",
        "enabled",
        0,
        "重启程序后生效",
        "设备分别统计",
        "设备 · 按键盘/鼠标设备分别计数（Raw Input）",
    ),
];

fn load_settings() -> Vec<SettingRow> {
    let c = focusflow_core::config::instance();
    let defaults = focusflow_core::config::default_config();
    SETTINGS
        .iter()
        .map(|&(section, key, kind, live, label, help)| {
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
                label,
                help,
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
    /// 当前**生效**的数据根目录（`[paths] data_home`，建不出来时 core 会回落到程序目录）。
    /// 设置页「数据目录」那一行显示的就是它；`app_dir` 是程序目录，两者不是一回事。
    pub data_home: String,
    /// `[paths] data_migrate_from` 的原文：非空 = 已经安排了一次搬迁，重启时兑现。
    /// 显示出来，免得用户以为改完就已经搬走了。
    pub pending_move: String,
    /// 这一轮没读上来的年度库条数（非 0 = 上面那些数是**下界**）。
    pub gaps: usize,
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
    rank.sort_by(|a, b| focusflow_core::format::cmp_rank_desc(&a.0, &a.1, &b.0, &b.1));
    rank.truncate(100);

    // 日均的分母：窗口天数；"全部"按有数据的天数（与卡片上的"日均"同口径）。
    let avg_days = match days {
        Some(d) => d.max(1),
        None => daily
            .iter()
            .map(|(_, v)| *v)
            .filter(|v| *v > 0)
            .count()
            .max(1) as i64,
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
    apps.sort_by(|a, b| focusflow_core::format::cmp_rank_desc(&a.0, &a.1, &b.0, &b.1));
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
        data_home: focusflow_core::data_location::current_data_home()
            .display()
            .to_string(),
        pending_move: focusflow_core::config::instance()
            .get("paths", "data_migrate_from")
            .trim()
            .to_string(),
    }
}

/// 排名尺子只许有一份：这一屏的两份榜（以及 `main.rs` 周报那三处）必须走
/// `focusflow_core::format::cmp_rank_desc`，不许再各自手写"次数降序、同分按名字升序"。
#[cfg(test)]
mod rank_ruler_tests {
    /// 生产文本 = 本文件 `#[cfg(test)]` 之前的部分。判据用的针要拼出来，否则这条用例
    /// 自己的字面量会被它正在查的样式数进去（`main.rs` 里同一手）。
    fn prod(self_file: bool) -> String {
        let src = if self_file {
            include_str!("data.rs")
        } else {
            include_str!("main.rs")
        };
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        src[..cut].to_string()
    }

    #[test]
    fn the_rank_boards_use_the_one_comparator() {
        let hand = [".cmp(&a.1)", ".then_with"].concat();
        let wired = ["cmp_rank_desc", "("].concat();
        for what in [("data.rs", prod(true)), ("main.rs", prod(false))] {
            assert!(
                !what.1.contains(hand.as_str()),
                "{} 里又手写了一遍排名尺子 ⇒ 与 cmp_rank_desc 分叉时没有任何用例会红",
                what.0
            );
        }
        assert_eq!(
            prod(true).matches(wired.as_str()).count(),
            2,
            "键鼠榜与应用榜这两处都要走那一份尺子"
        );
        assert_eq!(
            prod(false).matches(wired.as_str()).count(),
            3,
            "周报的键、应用、设备三个 Top 榜都要走那一份尺子"
        );
    }
}
