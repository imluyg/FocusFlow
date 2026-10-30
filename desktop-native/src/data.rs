//! 只读数据快照：按当前统计周期算一屏要显示的数。
//!
//! 为什么是"快照"而不是让界面直接查库：真实版本里统计在后台线程算、算完推给界面，
//! 界面永远只读一份已算好的数。原型照这个形状来，P2 换过去时不用重写视图层。
//!
//! 读库走 `Database::init_readonly()`（CLI 用的那条），所以**不会**启动写入线程、
//! 不会碰正在运行的采集进程的写队列；`FOCUSFLOW_APP_DIR` 指向哪就读哪。

use std::collections::HashMap;

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
        "热键 · 按下它把主窗口带到前台，如 ctrl+shift+f；点值框右边的叉＝这条不注册",
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
        "snip",
        "hide_floating",
        0,
        "改完立即生效",
        "截图时隐藏悬浮窗",
        "截图 · 开＝整场都不显示；关＝冻结后立刻放回（会闪一下桌面）",
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
        "刷新间隔（秒）",
        "界面 · 快照线程多久重新读一次库",
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
            } else if section == "hotkey"
                && crate::hotkey_off_of(c.has_key(section, key), &c.get(section, key))
            {
                // 他清空过的那一条：这一格要显示"未启用"而不是默认那一串。
                // `get_or` 会把"有这行但空"与"没这行"捏成同一个默认值，照它读就会出现
                // "屏上写着 ctrl+shift+f、而这条根本没挂上"——正是这一族最难查的形状。
                String::new()
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
    /// 当前周期的日均（有限窗口按窗口含几天算；「总计」按全历史记到过次数的天数算，
    /// 见 [`avg_of`]）。
    pub avg: i64,
    /// 全历史最高单日。
    pub max_day: Option<(String, i64)>,
    pub rank: Vec<(String, i64)>,
    /// 键名分组榜（`format::KEY_GROUPS` 那八组的固定顺序），排行页「分组」那一屏的行。
    pub group: Vec<(String, i64)>,
    /// 这一轮里鼠标那一侧的总次数（`MOUSE_GROUPS` 两组之和）。
    pub mouse_total: i64,
    /// 键盘那一侧 = 榜单总数减鼠标：这两条加起来是**这一轮读回来的键鼠总次数**
    /// （[`Snapshot::rank_share_base`]），也就是榜里那些行的分母。
    /// 它比卡片那个「总计」（`period_total`）少一截 —— 少的是还没落库的那批增量，
    /// 它们还没归到键盘或鼠标任何一侧（`main.rs` 标题行写「其中」就是这个理由）。
    pub keyboard_total: i64,
    /// 前台应用时长（当前周期），按秒数降序。
    pub apps: Vec<(String, i64)>,
    pub app_total: i64,
    pub devices: Vec<DevRow>,
    /// 设备侧这一轮读回来的总次数 —— `devices` 那些行的分母。
    ///
    /// ⚠ 它**不是** `period_total` 的另一种写法：设备计数走 Raw Input 那条侧信道，与主链路
    /// 的口径本来就差一截（合成的键盘事件不计数、鼠标事件计），所以拿主链路的总数去除
    /// 设备那一栏的次数，除出来的是两把尺子的商，而不是"这台设备占了多少"。
    /// 与 `app_total` 同一个地位：每一屏的占比只认自己那一趟读回来的总数。
    pub device_total: i64,
    /// 每日次数，窗口 = core 的 `chart_window_days(period)`（今日 1 天、总计按 30 天）。
    /// 每项是 `(YYYY-MM-DD, 次数)`：窗口内每天都有格子（当天没数据的是 0），图上刻度与
    /// 导出那一栏念的就是这里交回来的日期，不由调用方按下标反推。
    pub daily: Vec<(String, i64)>,
    /// 当前周期内每小时的合计（今日 = 今天 24 小时）。
    pub hourly: Vec<i64>,
    /// 星期分布（0=周一 … 6=周日），跟随周期窗口。
    pub weekday: [i64; 7],
    /// 每日目标与连续打卡。`None` = 这一轮没算出来（页面上要说明）。
    pub goal: Option<GoalStatus>,
    /// 高级设置的行（值 + 类型 + 生效时机）。
    pub settings: Vec<SettingRow>,
    /// 开机自启：**现读**启动文件夹，不读配置键（用户在资源管理器里删掉它时，
    /// 配置里还写着开）。现在它跟着每一拍刷新一起读一次 —— 这条不是成本中心
    /// （`perf_probe` 量的是 31µs/次，同一拍里那些查询是 8~50ms），所以不为此另设一条
    /// "只在进设置页时读"的路：那条一改，屏上这一格就会在别人改快捷方式后停在旧值。
    pub autostart: bool,
    /// 当前**生效**的数据根目录（`[paths] data_home`，建不出来时 core 会回落到程序目录）。
    /// 设置页「数据目录」那一行显示的就是它；`app_dir` 是程序目录，两者不是一回事。
    pub data_home: String,
    /// `[paths] data_migrate_from` 的原文：非空 = 已经安排了一次搬迁，重启时兑现。
    /// 显示出来，免得用户以为改完就已经搬走了。
    pub pending_move: String,
    /// 这一轮没答上来的每一栏读数，每项形如「N 年库<域名>读不出来: 原因」；非空 =
    /// 上面那些数是**下界**。界面按 `queries::DOMAIN_*` 那几个域名过滤到各自那一栏去说。
    pub gaps: Vec<String>,
}

impl Snapshot {
    /// 键名/分组那些「占比」的分母：与榜里那些行**同一次读回来**的键鼠总次数。
    ///
    /// 为什么不用 `period_total`：那一个是卡片上要念的周期总数，含写线程内存里还没落库的
    /// 增量（`pending`），而榜里那些行读的是已落库的聚合表。拿含增量的数当分母、分子却是
    /// 不含增量的行，每一行的占比都按 `pending/(总数)` 这一档偏小 ——「分组」那一屏最明显：
    /// 八组本来就盖住全部键名，加起来本该正好 100%，这样除完只到 97% 甚至更少，而屏上没有
    /// 任何一句话解释为什么。两条相加就是这一份（`rank_break` 里 `keyboard_total` 是
    /// `total - mouse_total`），所以这里不另存一个数，免得同一件事有两份答案。
    pub fn rank_share_base(&self) -> i64 {
        self.mouse_total.max(0) + self.keyboard_total.max(0)
    }
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

/// 三条时间切面（趋势 / 小时 / 星期）当前数的是哪一段：图上那行小字与导出里那一栏的
/// 标题都念它。窗口本身来自 core 的 `chart_window_days`，所以这句话永远不会与取数错位。
///
/// 为什么不复用界面那份 `period_text`：那一份是排行榜与卡片用的，它对「总计」的说法是
/// 「全部」—— 按到这里就把一个 30 天的窗口夸成了全部历史（旧前端的 `windowText()`
/// 特意分开的就是这两句）。
pub fn window_text(period: i64) -> String {
    let days = queries::chart_window_days(period);
    if days == 1 {
        "今日".to_string()
    } else {
        format!("近 {days} 天")
    }
}

/// 键鼠榜那一屏的"鼠标侧"判据。
///
/// 用 core 的分组尺子（`classify_key` → `MOUSE_GROUPS`），不搬旧前端那句
/// "看键名是不是以『鼠标』『滚轮』开头"：同一件事有两份答案时，`classify_key` 一改产出名，
/// 鼠标那一路会静默拿到 0、键盘拿到总数 —— 数字看着完全正常，没有任何用例会红。
pub fn rank_is_mouse(key: &str) -> bool {
    focusflow_core::format::MOUSE_GROUPS.contains(&focusflow_core::format::classify_key(key))
}

/// 「日均」这一格：分子是这一轮的总次数（含还没落库的增量），分母按周期分两档 ——
/// 有限窗口就是窗口本身含几天，「总计」则是全历史里记到过次数的那几天。
///
/// 收成一条纯函数是因为分母的两档过去混在一个内联 `match` 里，而其中一档取错了源：
/// 拿"图上那 30 天里干了事的天数"去除"全部历史的次数"。这样写，两档各吃什么数在签名上就摊开了。
/// 分母至少 1：一个数都没有时不许除出个 `inf` 或 panic。
pub(crate) fn avg_of(total: i64, window_days: Option<i64>, active_days: i64) -> i64 {
    let days = match window_days {
        Some(d) => d.max(1),
        None => active_days.max(1),
    };
    total / days
}

/// 键名分组榜 + 键/鼠两侧总次数：一次算出，「分组」那一屏与标题行那两个数读同一份。
#[derive(Clone, Debug, Default)]
pub struct RankBreak {
    /// 按 `KEY_GROUPS` 的固定顺序排好的分组行（空组不出行，与旧版一致）。
    pub groups: Vec<(String, i64)>,
    pub mouse_total: i64,
    /// `total - mouse_total`：两条加起来正好等于传进来的那一份 `total`（这一轮读回来的
    /// 键鼠总次数），不留"两边各算一遍"的口子。⚠ 它**不是**卡片那个含未落库增量的
    /// `period_total` —— 差的就是那批还没归到任何一侧的增量。
    pub keyboard_total: i64,
}

/// 分组与总次数必须在**截断之前**算：`RankTotals.rows` 交回的是未截断的整榜
/// （降序与截断由调用方负责），截完再算会把 100 名以后的那些次数静默丢掉。
pub fn rank_break(rows: &HashMap<String, i64>, total: i64) -> RankBreak {
    let mut by_group: HashMap<&'static str, i64> = HashMap::new();
    for (k, v) in rows {
        *by_group
            .entry(focusflow_core::format::classify_key(k))
            .or_insert(0) += v;
    }
    let groups: Vec<(String, i64)> = focusflow_core::format::KEY_GROUPS
        .iter()
        .filter_map(|g| by_group.get(*g).map(|c| (g.to_string(), *c)))
        .collect();
    let mouse_total: i64 = focusflow_core::format::MOUSE_GROUPS
        .iter()
        .map(|g| by_group.get(*g).copied().unwrap_or(0))
        .sum();
    RankBreak {
        groups,
        mouse_total,
        keyboard_total: total - mouse_total,
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
    // 「最高单日」这一趟只问最大值：它交回的全历史总次数此前一个读者都没有
    // （屏上那个「总计」念的是上面 `stats.total + pending`），每拍多算一条每库的 SUM。
    let all = queries::get_alltime_max_day();
    // 三条时间切面（趋势 / 小时 / 星期）与三份导出里那栏"每日次数"共用 core 那把
    // `chart_window_days`：今日 1 天、总计按 30 天，其余就是周期本身。
    // ⚠ 「总计」那一档图上数的是近 30 天，而卡片那句「日均」数的是全历史里记到过次数的
    // 天数（下面 `avg_of` 的第三个入参）—— 两个不是一把尺子，各自在屏上念各自的窗口。
    // `crates/cli/src/main.rs` 的周期参数也读这一把 —— 界面自写一个
    // 固定 30 天，"今日"那一档就会画出 30 个与所选周期无关的点。
    let chart_days = queries::chart_window_days(period);
    // 每日序列连日期一起交回：图上的刻度与导出里那一栏念的是同一串日期，
    // 不由调用方按下标往回推。
    // 一趟查回**两段**：给图的这段（`chart_days`）与给打卡条的那段（`GOAL_LOOKBACK_DAYS`）。
    // 旧形状各扫一遍 `daily_counts`，而宽的那段把窄的整个盖住 ⇒ 同一天问了两遍。
    // 两段的"哪一年没读上"在 core 里按各自窗口的年份清单分头交回（`read_daily_pair`），
    // 所以下面 `gaps` 接的是**窄的那一段**，不会把打卡条扫到的坏年份算在这张图头上。
    let (daily_read, goal_read) = queries::daily_counts_windows(chart_days, GOAL_LOOKBACK_DAYS);
    let daily = daily_read.series;
    // 小时图同样问"带原因的那份读数"：读不全时要能点名是哪一年没答上来。
    let hourly_read = queries::hourly_window_totals(chart_days, None);
    // 星期分布是每日序列的聚合（core 的 `aggregate_weekday` 就是为复用这一份留的口子），
    // 不再为同一个窗口扫第二遍库；"哪一年没读上"也就继承上面每日趋势那一句。
    let mut weekday = [0i64; 7];
    for (k, v) in queries::aggregate_weekday(&daily) {
        if (0..7).contains(&k) {
            weekday[k as usize] = v;
        }
    }

    // 分组榜与键/鼠总次数：必须吃**未截断**的整榜，所以走在 `truncate` 之前。
    let br = rank_break(&stats.rows, stats.total);
    let mut rank: Vec<(String, i64)> = stats.rows.into_iter().collect();
    // 次数降序；次数相同按名字升序，保证两次刷新之间不会自己换位。
    rank.sort_by(|a, b| focusflow_core::format::cmp_rank_desc(&a.0, &a.1, &b.0, &b.1));
    rank.truncate(100);

    // 日均的分母：窗口天数；"全部"按**全历史记到过次数的天数**。
    // ⚠ 这一档的分母过去取的是 `daily`（窄窗口那一段，「总计」只有 30 天）里有行的天数，
    // 而分子是整个周期的总数 ——「总计」那一档的周期就是全部历史：于是每拍都在拿
    // "全部历史的次数"除以"最近 30 天里干了事的那几天"，两年历史能把这一格抬到真实值的
    // 二十几倍，而卡片下面那句写的却是"按周期窗口"。现在分子与分母同一段：`active_days`
    // 与全历史次数点的是同一批年度库（core 的 `get_alltime_max_day` 那一趟，每库仍一条
    // 语句，没为多要一个数再排一次队）。
    let avg = avg_of(stats.total + pending, days, all.active_days);

    let mut hours = hourly_read.hours;
    hours.resize(24, 0);

    let device_read = queries::device_totals(days, None);
    let devices: Vec<DevRow> = device_read
        .devices
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

    let app_read = queries::app_stats_totals(days, None);
    let mut apps: Vec<(String, i64)> = app_read.rows.into_iter().collect();
    apps.sort_by(|a, b| focusflow_core::format::cmp_rank_desc(&a.0, &a.1, &b.0, &b.1));
    apps.truncate(100);

    // 每日目标与连续打卡：要扫近 370 天按日序列 —— 这一趟**已经为图查过了**，
    // 上面 `daily_counts_windows` 交回的就是它的宽段，这里只取那一段的行。
    // ⚠ 宽段自己的"哪一年没读上"**不进**下面那个 `gaps`：面板是按域名把缺读分栏念的，
    // 而「每日趋势」那一栏只画窄窗口那些天，把打卡条扫到的坏年份念进去就是指控错了人。
    // 打卡条今天没有自己的缺读栏（`get_daily_counts` 那个二元组口子本来也交不回原因），
    // 要不要给它接上是一句口径题，已登记待点。
    let goal_cfg = focusflow_core::config::instance().get_int("goal", "daily_keys", 20000);
    let goal_rows = goal_read.series;
    let today_str = chrono::Local::now().format("%Y-%m-%d").to_string();
    // ⚠ 第四个参数必须是上面那份 `pending`，不许图省事填 `0`：core 的那份文档
    // (`stats::goal_status`) 明说判定要读"库值 + 未落库增量"这同一张 map，否则同一页会出现
    // 「进度条已过线、连续打卡还少一天」。症状（现量在屏上）：达标后的那最多 10 秒里，
    // 「今日 20,050 / 20,000」与那条绿进度条读的是写线程缓存（含增量），而「连续打卡 N 天」
    // 和最近七天那七个点读的是已落库的聚合表 —— 今天那一格还灰着。
    let goal = Some(goal_status(goal_cfg, &goal_rows, &today_str, pending));

    // 采集开着时，今日直接用写线程缓存：它含未落库与在途增量，落库前后都不跳
    // （用「库值 + pending」会在 flush 取走批次、尚未提交的那一瞬读到 0）。
    let today = match crate::collect::today_total() {
        Some(n) => n,
        None => queries::get_today_count(None),
    };
    // 今日前台时长：与上面应用排行同一个口径，只是窗口固定成 1 天（悬浮窗要的是"今天"）。
    // 周期本来就是「今日」时两者是**同一个窗口**（`get_app_stats` 就是 `app_stats_totals`
    // 的二元组包装，两个数本来同一把尺子），直接取上面那一趟的结果，不再为同一张
    // `app_usage` 扫第二遍 —— 现量那一臂每拍自己占 3 条语句（`perf_probe`「每一臂自己几条语句」）。
    // ⚠ 这条 dedup 只在"窗口恰好相等"时成立：`days` 是 7/15/30/总计 时，今日那一格
    // 仍是独立的一问，不许顺手把它也省了（屏上「时长」会变成整个周期的秒数）。
    let today_app_seconds = if days == Some(1) {
        app_read.total
    } else {
        queries::get_app_stats(Some(1), None).0
    };

    // 每一栏"这一轮有没有读全"合成一串，界面按域名分栏去念（该接哪几栏，
    // `queries::PANEL_READ_DOMAINS` 里已经写好答案）。全历史「最高单日」那几条的形状是
    // 「年份: 原因」、不带域名 ⇒ 屏上按域名过滤的那几格接不到它（`main.rs::gap_years`
    // 是按域名字样挑的），它只进导出里那句全句（`main.rs::gap_line` 念整串）。
    let mut gaps: Vec<String> = Vec::new();
    gaps.extend(stats.unreadable);
    gaps.extend(app_read.unreadable);
    gaps.extend(device_read.unreadable);
    gaps.extend(daily_read.unreadable);
    gaps.extend(hourly_read.unreadable);
    gaps.extend(all.unreadable);

    Snapshot {
        period,
        today,
        today_app_seconds,
        period_total: stats.total + pending,
        avg,
        max_day: all.max_day,
        rank,
        group: br.groups,
        mouse_total: br.mouse_total,
        keyboard_total: br.keyboard_total,
        apps,
        app_total: app_read.total,
        devices,
        device_total: device_read.total,
        daily,
        hourly: hours,
        weekday,
        goal,
        settings: load_settings(),
        autostart: focusflow_core::autostart::is_autostart_enabled(),
        gaps,
        data_home: focusflow_core::data_location::current_data_home()
            .display()
            .to_string(),
        pending_move: focusflow_core::config::instance()
            .get("paths", "data_migrate_from")
            .trim()
            .to_string(),
    }
}

#[cfg(test)]
mod readout_rulers_tests {
    use super::*;

    /// 生产文本（整行注释与 `///` 一并滤掉，判据数的是代码）。
    fn prod() -> String {
        let src = include_str!("data.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        src[..cut]
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 设置清单上的每一行都必须在 `default_config()` 里真有默认值。
    ///
    /// 段名/键名写错一个字母时，屏上照样画出这一行（值走 `unwrap_or_default()` 显示成空，
    /// 开关类就画成「关」），点一下又会往 config.ini 里写一个**新键** —— 而真正那个开关
    /// 从头到尾没人碰过。这条用例把清单与默认值表对齐。
    #[test]
    fn every_settings_row_has_a_default_in_the_shipped_config() {
        let defaults = focusflow_core::config::default_config();
        for (section, key, ..) in SETTINGS {
            assert!(
                defaults.get(*section).is_some_and(|s| s.contains_key(*key)),
                "设置行 {section}.{key} 在 default_config() 里没有默认值"
            );
        }
    }

    /// 开关行的说明文字必须装得进它那一列：值框从 x=720 起画，说明从 ~334 起、列宽
    /// `(720 - r.left - 30) ≈ 368 px`。这块 UI **不按矩形裁切**（`NO_WRAP`、不设
    /// `DWRITE_TRIMMING`），超出去的字照画 —— 2026-10-30 新加的那一行就是这么把
    /// "（旧观感，中途会闪一下桌面）"画到「开」按钮右边的（截图量过，改前 48 字、
    /// 改后约 350 px 正好收在框左）。开关行没有别的遮挡物，所以只有这一族的长度要钉。
    #[test]
    fn a_toggle_rows_help_fits_before_its_value_box() {
        // 近似字宽：中日韩全角 12 px，ASCII 6 px（与 12 号字量出来的行宽同量级）。
        let width = |s: &str| -> usize {
            s.chars()
                .map(|c| if (c as u32) < 0x2E80 { 6 } else { 12 })
                .sum()
        };
        for (section, key, kind, _, _, help) in SETTINGS {
            if *kind != 0 {
                continue;
            }
            assert!(
                width(help) <= 368,
                "开关行 {section}.{key} 的说明宽约 {} px，会画到值框上：{help}",
                width(help)
            );
        }
    }

    /// 「日均」那两档的分母各吃什么数。
    ///
    /// 换掉之前症状是一格数字：「总计」那一档的周期是全部历史，分子也是全部历史，
    /// 而分母数的是**图上那 30 天**里干了事的那几天 —— 两年历史能把这一格抬到真实值的
    /// 十几二十倍，卡片下面那句"按周期窗口"却没有第二个地方会响一声。
    #[test]
    fn the_average_uses_the_window_or_the_whole_history() {
        // 有限窗口：1000 次摊到窗口含的那 30 天
        assert_eq!(avg_of(1000, Some(30), 400), 33);
        // 「总计」：分母换成全部历史里记到过次数的那 400 天
        assert_eq!(avg_of(1000, None, 400), 2);
        assert_ne!(
            avg_of(1000, None, 400),
            avg_of(1000, Some(30), 400),
            "两档若吃同一个分母，这条纯函数就白分了 —— 说明有一档接错了入参"
        );
        // 一个数都没有也只是除不动，不许除出荒唐值（分母至少 1）
        assert_eq!(avg_of(1000, None, 0), 1000);
        assert_eq!(avg_of(1000, Some(0), 5), 1000);
    }

    /// 占比的分母 = 这一轮读回来的键鼠总数，不含还没落库的那批增量。
    #[test]
    fn the_share_base_is_the_landed_total_not_the_card_number() {
        // 卡片那个「总计」念的是 100（已落库）+ 50（还没落库）= 150
        let rank = vec![
            ("字母键".to_string(), 70i64),
            ("鼠标点击".to_string(), 30i64),
        ];
        let s = Snapshot {
            period_total: 150,
            mouse_total: 30,
            keyboard_total: 70,
            group: rank.clone(),
            rank,
            ..Default::default()
        };
        assert_eq!(
            s.rank_share_base(),
            100,
            "分母要和榜里那些行同一个出处，不能含没落库的增量"
        );
        assert_ne!(
            s.rank_share_base(),
            s.period_total,
            "卡片那个数含增量，不是分母"
        );

        // 「分组」那一屏的判据：八组盖住全部键名，按这个分母除，占比加起来正好 100%。
        // 旧的分母（150）除出来是 66.7% —— 而屏上没有任何一句话解释为什么不满一百。
        let sum: f64 = s
            .group
            .iter()
            .map(|(_, n)| *n as f64 * 100.0 / s.rank_share_base() as f64)
            .sum();
        assert!(
            (sum - 100.0).abs() < 0.001,
            "八组加起来该正好 100%，实得 {sum:.3}%"
        );
    }

    /// 接线腿：「日均」现在吃的是全历史那份天数，不是图上那一段。
    #[test]
    fn the_average_is_fed_the_alltime_day_count() {
        let prod = prod();
        let call = "avg_of(stats.total + pending, days, all.active_days)";
        assert_eq!(
            prod.matches(call).count(),
            1,
            "「日均」的分子、窗口与全历史天数要按这三个入参交进去，实得:\n{prod}"
        );
        assert!(
            !prod.contains(".filter(|v| *v > 0)"),
            "分母又回到拿图上那一段数天数了 —— 那正是「总计」那一档抬起来的原因"
        );
    }

    /// 接线腿：打卡条那一问要把未落库的增量并进去。
    ///
    /// 填 `0` 的症状在同一页上：「今日 20,050 / 20,000」与那条绿进度条读的是写线程缓存
    /// （含增量），而「连续打卡 N 天」和最近七天那七个点读的是已落库的聚合表 ——
    /// core 那份文档（`stats::goal_status`）写的就是"不并上它这半边就停在上次落库为止"。
    #[test]
    fn the_goal_strip_is_fed_the_unflushed_delta() {
        let prod = prod();
        let call = "goal_status(goal_cfg, &goal_rows, &today_str, pending)";
        assert_eq!(
            prod.matches(call).count(),
            1,
            "打卡条要吃到那份未落库增量（第四个参数是 pending），实得形状不对"
        );
        assert!(
            !prod.contains("goal_status(goal_cfg, &goal_rows, &today_str, 0)"),
            "又填回 0 了：同一页就会出现「进度条已过线、连续打卡还少一天」"
        );
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

/// 键名分组榜与键/鼠总次数：尺子在 core，这边只管"在截断之前用它"。
#[cfg(test)]
mod rank_break_tests {
    use super::*;

    /// 生产文本（连整行注释一起滤掉，判据数的是代码）：本文件 `#[cfg(test)]` 之前的部分。
    fn prod() -> String {
        let src = include_str!("data.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        src[..cut]
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn rows() -> HashMap<String, i64> {
        [
            ("a", 10i64),
            ("1", 5),
            ("F1", 7),
            ("Shift", 3),
            ("空格", 2),
            ("鼠标左键", 20),
            ("滚轮上滑", 8),
            ("某输入法标点", 1),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }

    #[test]
    fn the_groups_follow_the_core_order_not_the_counts() {
        let br = rank_break(&rows(), 56);
        let names: Vec<&str> = br.groups.iter().map(|(g, _)| g.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "字母键",
                "数字键",
                "功能键",
                "修饰键",
                "编辑键",
                "鼠标点击",
                "滚轮",
                "其他"
            ],
            "行序必须是 KEY_GROUPS 那一份固定顺序，不是按次数重排"
        );
        assert_eq!(br.groups[5].1, 20);
        assert_eq!(br.groups[6].1, 8);
    }

    #[test]
    fn an_empty_group_takes_no_row_and_the_two_sides_add_up() {
        let only_keys: HashMap<String, i64> = [("a", 4i64)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        let br = rank_break(&only_keys, 4);
        assert_eq!(br.groups.len(), 1, "没按过的组不许占一行（与旧版一致）");
        assert_eq!(br.groups[0].0, "字母键");
        assert_eq!(br.mouse_total, 0);
        assert_eq!(br.keyboard_total, 4);

        let br = rank_break(&rows(), 56);
        assert_eq!(br.mouse_total, 28, "鼠标点击 20 + 滚轮 8");
        // 这条钉的是"两边加起来就是总数"这个不变量，不是"两个数都非零"：
        // 鼠标那一路静默拿到 0 时，一个 0 一个 56 看着完全不像坏了。
        assert_eq!(br.mouse_total + br.keyboard_total, 56);
    }

    #[test]
    fn the_mouse_side_is_the_group_ruler_not_a_name_prefix() {
        assert!(rank_is_mouse("鼠标左键"));
        assert!(rank_is_mouse("滚轮上滑"));
        assert!(!rank_is_mouse("a"));
        assert!(!rank_is_mouse("Shift"));
        // 尺子只许有一份：这边再出现"看键名前缀"的写法，就是与 `classify_key` 分叉的第二把。
        let hand = ["starts_with(", "\"鼠标\"", ")"].concat();
        assert!(
            !prod().contains(hand.as_str()),
            "界面这边自己按前缀认鼠标键了 ⇒ `classify_key` 改产出名时它会静默失灵"
        );
    }

    /// 分组必须在**截断之前**算。挪到 `truncate` 之后，100 名开外的次数会静默丢掉：
    /// 榜看着一切正常，分组那一屏与标题上那两个数却比总数小，没有用例红。
    #[test]
    fn the_groups_are_made_before_the_board_is_truncated() {
        let prod = prod();
        let made_at = ["rank_break", "(&stats.rows"].concat();
        let cut_at = ["truncate", "(100"].concat();
        let made = prod.find(made_at.as_str()).expect("分组这一问不见了");
        let cut = prod.find(cut_at.as_str()).expect("截断那一句不见了");
        assert!(
            made < cut,
            "分组算在截断之后了：100 名开外的次数会静默从分组榜与键鼠总次数里消失"
        );
        assert_eq!(
            prod.matches(["rank_break", "("].concat().as_str()).count(),
            2,
            "一处定义 + 一处调用，数量变了说明接线动过"
        );
    }
}

/// 三条时间切面（趋势 / 小时 / 星期）的窗口与图上那行小字。
///
/// 两件事分开钉：一是"标题说的是不是真窗口"（旧前端把 `period_text` 与 `windowText`
/// 分成两句，就是因为「总计」在排行榜里念「全部」、在图里其实只数 30 天）；
/// 二是"窗口这把握尺子不许有第二份"（写死 30 天、或为星期分布扫第二遍库，都算分叉）。
#[cfg(test)]
mod chart_window_tests {
    use super::window_text;

    /// 生产文本 = 本文件 `#[cfg(test)]` 之前的部分，并且**去掉整行注释**：
    /// 接线守卫数的是调用点，注释里提一次函数名（带括号的写法很自然）不该把计数抬起来。
    /// 针脚仍旧拼出来，免得用例自己的字面量被它正在查的样式数进去。
    fn prod() -> String {
        let src = include_str!("data.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        src[..cut]
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 六颗周期胶囊各自数到哪一段，图上就怎么说。
    #[test]
    fn the_window_says_what_the_chart_actually_counts() {
        assert_eq!(window_text(-1), "今日", "今日那一档就一天");
        assert_eq!(window_text(7), "近 7 天");
        assert_eq!(window_text(15), "近 15 天");
        assert_eq!(window_text(30), "近 30 天");
        assert_eq!(window_text(365), "近 365 天");
        // 「总计」那一格数的是近 30 天（图上画的就是这 30 根柱子）。
        // 跟着排行榜那句写成「全部」，图就对着 30 根柱子夸口自己是全部历史。
        // ⚠ 卡片那句「日均」在这一档不吃这把尺子：它的分母是全历史里记到过次数的天数。
        assert_eq!(window_text(0), "近 30 天");
        assert_ne!(window_text(0), "全部", "这一栏不许借排行榜那句「全部」");
    }

    /// 接线腿：钉的是调用形态，不是函数名 —— 只数名字的话，"尺子调了、取数仍旧写死
    /// 30 天"这一种接法能整个蒙过去。
    #[test]
    fn the_charts_read_the_one_window_ruler() {
        let prod = prod();
        let ruler = ["chart_window_days", "("].concat();
        assert_eq!(
            prod.matches(ruler.as_str()).count(),
            2,
            "两处：取数那一头与图上那行小字，必须问同一个入口"
        );
        let hard_30 = ["get_daily_counts", "(30"].concat();
        assert_eq!(
            prod.matches(hard_30.as_str()).count(),
            0,
            "写死 30 天那一档不许回来：它会让「今日」画出 30 个与所选周期无关的点"
        );
        let second_scan = ["get_weekday_stats", "("].concat();
        assert_eq!(
            prod.matches(second_scan.as_str()).count(),
            0,
            "星期分布不再为同一个窗口扫第二遍库"
        );
        let reuse = ["aggregate_weekday", "("].concat();
        assert_eq!(
            prod.matches(reuse.as_str()).count(),
            1,
            "复用上面那串已查得的每日序列（core 那份文档就是为这个留的口子）"
        );
        for needle in [
            // 每日序列现在是**一趟查两段**（图 + 打卡条），入口改名 `daily_counts_windows`：
            // 旧的 `daily_counts_totals(chart_days)` 与 `get_daily_counts(370)` 两问合成一问，
            // 宽的那段把窄的整个盖住，同一天不该问两遍（core 里 `read_daily_pair`）。
            ["daily_counts_windows", "("].concat(),
            ["hourly_window_totals", "("].concat(),
            ["app_stats_totals", "("].concat(),
            ["device_totals", "("].concat(),
        ] {
            assert!(
                prod.contains(needle.as_str()),
                "少了 {needle} 这一问：那一栏就说不出「这一轮有没有读全」"
            );
        }
    }

    /// 周期选「今日」时不许把同一张 `app_usage` 为同一个窗口扫两遍。
    ///
    /// 悬浮窗那一格要的是"今天的前台秒数"，排行那一屏要的是"本周期的秒数" —— 周期就是
    /// 今日时这两个窗口**是同一个**，旧形状每拍各问一次（现量：多出来的那一臂每拍 3 条
    /// 语句，`crates/core/tests/perf_probe.rs`「每一臂自己几条语句」）。
    ///
    /// ⚠ 这一刀的判据钉的是"这一问还待在它该待的分支里"，不钉快慢：整拍毫秒实测没降
    /// （SQLite 的这几问本来就不是时间瓶颈），省下的是一趟重复读。
    /// 而分支被摘掉的症状不是红、是**数字错** —— 悬浮窗会把整个周期的秒数当"今日"念，
    /// 屏上没有任何东西会因此响一声。
    #[test]
    fn the_today_app_seconds_reuses_the_rank_read_when_the_window_is_the_same() {
        let prod = prod();
        let today_entry = ["get_app_stats", "(Some(1)"].concat();
        assert_eq!(
            prod.matches(today_entry.as_str()).count(),
            1,
            "今日那一问的入口只许一处；出现第二处就是又扫了一遍同一个窗口"
        );
        assert!(
            prod.contains(
                ["today_app_seconds = if days == Some(1)", " {"]
                    .concat()
                    .as_str()
            ),
            "「窗口相同就不另问」那道分支不见了 —— 剩下那一问会变成无条件重复扫。\
             （另一半不用钉：`else` 那支要是被删，`today_app_seconds` 就没定义，编不过）"
        );
    }
}
