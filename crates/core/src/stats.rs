//! 实时统计：CPM（每分钟操作数）计算。
//!
//! 镜像 Python 版 `stats.py`：
//! - 滑动时间窗口 + deque 上限安全阀
//! - 写入/查询分离，查询时惰性清理过期数据
//! - 结果缓存（TTL 500ms）
//! - 线程安全

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::FocusFlowConfig;

/// CPM 内部状态（时间戳队列 + 结果缓存合并为单锁，减少热路径锁争用）。
struct CpmState {
    /// 时间戳队列（单调时钟 Instant）
    timestamps: VecDeque<Instant>,
    /// 缓存结果
    cached_count: i64,
    /// 算出 `cached_count` 的时刻；`None` = 缓存无效，下次查询重算。
    ///
    /// 用 `Option` 而不是「把时刻伪造成 now - 10s」来表示过期：`Instant` 减法在
    /// 开机时间短于该时长时下溢 panic，而 release 的 panic=abort 会让开机自启
    /// 撞上「登录后台刚起来、一敲键盘整个程序就没了」。
    /// 同一个坑在 app_stats.rs / db/queries.rs 都留过告诫。
    cached_at: Option<Instant>,
}

/// CPM 计算器。
pub struct CpmCalculator {
    /// 窗口（秒）
    window: f64,
    state: Mutex<CpmState>,
}

impl CpmCalculator {
    pub fn new(window: f64) -> Self {
        Self {
            window: window.max(1.0),
            state: Mutex::new(CpmState {
                timestamps: VecDeque::with_capacity(4096),
                cached_count: 0,
                cached_at: None,
            }),
        }
    }

    /// 记录一次操作时间戳。
    pub fn record(&self) {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.timestamps.push_back(now);
        // 硬上限安全阀（对应 Python maxlen=100000）
        if state.timestamps.len() > 100_000 {
            state.timestamps.pop_front();
        }
        // 顺带清理窗口外旧数据。判定写成 `now - front > window` 而不是
        // `front < now - window`：后者要算出那个过去时刻，开机不足窗口长度时
        // Instant 减法会下溢 panic（release 下 panic=abort，每次按键都走这里）。
        let window = Duration::from_secs_f64(self.window);
        while let Some(&front) = state.timestamps.front() {
            if now.duration_since(front) > window {
                state.timestamps.pop_front();
            } else {
                break;
            }
        }
        // 写入使缓存失效
        state.cached_at = None;
    }

    /// 获取当前 CPM（窗口内事件数）。
    pub fn get_cpm(&self) -> i64 {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = state.cached_at {
            if at.elapsed() < Duration::from_millis(500) {
                return state.cached_count;
            }
        }
        let window = Duration::from_secs_f64(self.window);
        while let Some(&front) = state.timestamps.front() {
            if now.duration_since(front) > window {
                state.timestamps.pop_front();
            } else {
                break;
            }
        }
        let count = state.timestamps.len() as i64;
        state.cached_count = count;
        state.cached_at = Some(now);
        count
    }

    /// 重置（清除当前时间戳）。
    pub fn reset(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.timestamps.clear();
        state.cached_count = 0;
        state.cached_at = None;
    }
}

/// 全局 CPM 单例。
static CPM: std::sync::OnceLock<Arc<CpmCalculator>> = std::sync::OnceLock::new();

/// 获取全局 CPM 计算器。
pub fn cpm(config: &'static FocusFlowConfig) -> Arc<CpmCalculator> {
    Arc::clone(CPM.get_or_init(|| {
        let window = config.get_float("stats", "cpm_window", 60.0);
        Arc::new(CpmCalculator::new(window))
    }))
}

/// 久坐提醒的判定状态（`[rest]` 那一节配置的实现）。
///
/// 输入只要一样东西：**今天的键鼠事件累计数**（写入线程的内存缓存，单调递增、
/// 跨零点归零）。按采样时刻留一小段历史，就能回答"最近 `window_minutes` 分钟里
/// 发生了多少次事件"，不必给采集侧再接一条回调链。
///
/// 三道闸都是为了别让人烦：
/// - 采样必须铺满整个窗口才开始判定 —— 刚开机/刚睡醒那半小时本来就还没坐够；
/// - 发过一次就清空采样，下一次要再攒满一整段窗口；
/// - `cooldown_minutes` 是两次提醒之间的最小间隔。
///
/// 参数每次判定从配置现读，所以改 `config.ini` 立刻生效（与 `[pomodoro]` 那种
/// 启动时读一次的不一样：这个线程本来每 tick 都在跑）。
#[derive(Debug, Default)]
pub struct RestMonitor {
    /// (采样时刻, 当时的今日累计事件数)
    samples: VecDeque<(Instant, i64)>,
    last_check: Option<Instant>,
    last_sent: Option<Instant>,
}

/// 一条久坐提醒（推给前端的载荷）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RestNotice {
    /// 判定窗口（分钟），来自 `[rest] window_minutes`
    pub window_minutes: i64,
    /// 窗口内的事件数
    pub events_in_window: i64,
    /// 建议休息时长（秒），来自 `[rest] rest_seconds`
    pub rest_seconds: i64,
}

/// `[rest]` 的参数（越界值一律夹住，别让一句手写的 0 把提醒变成每 tick 一次）。
struct RestParams {
    enabled: bool,
    window: Duration,
    key_threshold: i64,
    cooldown: Duration,
    check_interval: Duration,
    rest_seconds: i64,
}

impl RestParams {
    fn load(cfg: &FocusFlowConfig) -> Self {
        let secs = |min: i64| Duration::from_secs(min.max(1) as u64 * 60);
        Self {
            enabled: cfg.get_bool("rest", "enabled", true),
            window: secs(cfg.get_int("rest", "window_minutes", 30)),
            key_threshold: cfg.get_int("rest", "key_threshold", 10_000).max(1),
            cooldown: secs(cfg.get_int("rest", "cooldown_minutes", 10)),
            check_interval: Duration::from_secs(
                cfg.get_int("rest", "check_interval", 10).clamp(1, 600) as u64,
            ),
            rest_seconds: cfg.get_int("rest", "rest_seconds", 20).clamp(1, 3600),
        }
    }
}

impl RestMonitor {
    /// 统计线程每 tick 调一次；到点且判定为久坐时返回一条提醒（自带节流）。
    pub fn observe(
        &mut self,
        now: Instant,
        today_total: i64,
        cfg: &FocusFlowConfig,
    ) -> Option<RestNotice> {
        let p = RestParams::load(cfg);
        if !p.enabled {
            // 关掉期间不攒样本：重新打开时不该拿"关着的那半小时"当久坐证据
            self.samples.clear();
            self.last_check = None;
            return None;
        }
        // `checked_duration_since` 而非减法：`Instant` 相减下溢会 panic，而 release
        // 是 panic=abort（改小 check_interval 后旧时刻还可能"在未来"）。拿不到差值
        // 就当"早就到点了"；从没发生过（None）同样是"早过了"。
        let since = |at: Option<Instant>| -> Duration {
            match at {
                Some(t) => now.checked_duration_since(t).unwrap_or(Duration::ZERO),
                None => Duration::MAX,
            }
        };
        if since(self.last_check) < p.check_interval {
            return None;
        }
        self.last_check = Some(now);

        // 今日计数变小 = 跨了零点（或库被清理/恢复），旧样本与新数字没有可比性
        if self.samples.back().is_some_and(|(_, c)| *c > today_total) {
            self.samples.clear();
        }
        self.samples.push_back((now, today_total));
        while let Some(&(at, _)) = self.samples.front() {
            if since(Some(at)) > p.window {
                self.samples.pop_front();
            } else {
                break;
            }
        }
        let &(first_at, first_count) = self.samples.front()?;
        // 窗口没铺满，"这一整段一直在用"就还不成立
        if since(Some(first_at)) < p.window {
            return None;
        }
        let events = today_total.saturating_sub(first_count);
        if events < p.key_threshold || since(self.last_sent) < p.cooldown {
            return None;
        }
        self.last_sent = Some(now);
        self.samples.clear();
        Some(RestNotice {
            window_minutes: p.window.as_secs() as i64 / 60,
            events_in_window: events,
            rest_seconds: p.rest_seconds,
        })
    }
}

/// 打卡判定的回看窗口（天）。调用方取按日序列时必须用同一个值，否则
/// `best` 会在数据边界上被截断，而两处各自写死数字迟早对不上。
pub const GOAL_LOOKBACK_DAYS: i64 = 370;

/// 每日目标与连续打卡的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct GoalStatus {
    pub goal: i64,
    /// 今日次数（尚未落库的增量不在内，统计线程会补）
    pub today: i64,
    /// 今日是否已达标
    pub today_met: bool,
    /// 连续达标天数。今天还没达标时，从昨天往回数——否则每天零点一到
    /// 连续记录就会清零，用户会看到「连了 20 天突然变 0」。
    pub streak: i64,
    /// 回看窗口内的最长连续纪录
    pub best: i64,
    /// 最近 7 天（旧→新）：(日期, 次数, 是否达标)
    pub days: Vec<(String, i64, bool)>,
}

/// 由「按日计数」序列算出每日目标达成与连续打卡。
///
/// `rows` 来自 `db::get_daily_counts`（升序、只含库里有的日期）；缺口日期按 0 处理。
/// 纯函数，便于覆盖跨年、缺口、今天未达标这些真实会踩到的边界。
pub fn goal_status(goal: i64, rows: &[(String, i64)], today: &str) -> GoalStatus {
    use chrono::{Local, NaiveDate};
    let map: std::collections::HashMap<&str, i64> =
        rows.iter().map(|(d, c)| (d.as_str(), *c)).collect();
    let goal = goal.max(1);
    let count_of = |d: NaiveDate| -> i64 {
        map.get(d.format("%Y-%m-%d").to_string().as_str())
            .copied()
            .unwrap_or(0)
    };
    let today_d =
        NaiveDate::parse_from_str(today, "%Y-%m-%d").unwrap_or_else(|_| Local::now().date_naive());

    let today_count = count_of(today_d);
    let today_met = today_count >= goal;

    // 连续天数：今天达标则 +1，再从昨天往回数。今天还没达标不清零——
    // 否则每天零点一到，昨天的纪录就凭空消失，用户会看到「连了 20 天突然变 0」。
    let mut streak = if today_met { 1 } else { 0 };
    let mut day = today_d - chrono::Duration::days(1);
    while count_of(day) >= goal {
        streak += 1;
        day -= chrono::Duration::days(1);
    }

    // 最长纪录：与 streak 一样在**日期轴**上数，不能直接遍历 rows。零活动的那天
    // 压根没有 daily_counts 行，按行遍历等于把缺失的间隔当成连续 ——
    // 9/1 和 9/3 各达标一次会被数成「连续 2 天」，而 streak 用日期轴算出来的是 1，
    // 两个口径互相打架（设置页显示的"最长"因此可能比真实值大）。
    let mut best = 0i64;
    let mut run = 0i64;
    let mut d = today_d - chrono::Duration::days(GOAL_LOOKBACK_DAYS - 1);
    while d <= today_d {
        if count_of(d) >= goal {
            run += 1;
            best = best.max(run);
        } else {
            run = 0;
        }
        d += chrono::Duration::days(1);
    }

    let days = (0..7i64)
        .rev()
        .map(|i| {
            let d = today_d - chrono::Duration::days(i);
            let key = d.format("%Y-%m-%d").to_string();
            let c = count_of(d);
            (key, c, c >= goal)
        })
        .collect();

    GoalStatus {
        goal,
        today: today_count,
        today_met,
        streak,
        best,
        days,
    }
}

/// 「刚结束的那一周」= 上一个完整的周一~周日。
///
/// 周报只能在整周结束后生成，所以锚点是本周一：不管今天周二还是周日，
/// 算出来的都是同一个区间，进程重启也不会多生成一份别的周。
pub fn last_finished_week(today: chrono::NaiveDate) -> (chrono::NaiveDate, chrono::NaiveDate) {
    use chrono::{Datelike, Duration, Weekday};
    let since_monday = match today.weekday() {
        Weekday::Mon => 0,
        Weekday::Tue => 1,
        Weekday::Wed => 2,
        Weekday::Thu => 3,
        Weekday::Fri => 4,
        Weekday::Sat => 5,
        Weekday::Sun => 6,
    };
    let this_monday = today - Duration::days(since_monday);
    (
        this_monday - Duration::days(7),
        this_monday - Duration::days(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpm_window() {
        let calc = CpmCalculator::new(60.0);
        for _ in 0..10 {
            calc.record();
        }
        assert_eq!(calc.get_cpm(), 10);
        calc.reset();
        assert_eq!(calc.get_cpm(), 0);
    }

    /// 回归：窗口长度超过机器开机时长时不得 panic。
    ///
    /// 原先 record()/get_cpm() 用 `Instant::now() - Duration::from_secs_f64(window)`
    /// 算窗口起点，而默认窗口 60 秒 —— 开机自启后一分钟内第一次敲键就下溢 panic，
    /// release 的 panic=abort 让它表现为「刚开机、一动键盘程序就消失」。
    /// 这里把窗口取成必然大于任何开机时长的秒数，与机器实际开了多久无关：
    /// 改成 `now - front > window` 判定后只是永远不淘汰时间戳，不再 panic。
    #[test]
    fn cpm_window_longer_than_uptime_does_not_panic() {
        let calc = CpmCalculator::new(1e12);
        for _ in 0..5 {
            calc.record();
        }
        assert_eq!(calc.get_cpm(), 5, "窗口远超开机时长：没有一条记录算过期");
        calc.reset();
        assert_eq!(calc.get_cpm(), 0);
    }

    fn rows(v: &[(&str, i64)]) -> Vec<(String, i64)> {
        v.iter().map(|(d, c)| (d.to_string(), *c)).collect()
    }

    /// 今天还没达标时，已有的连续纪录不得被清零。
    #[test]
    fn streak_survives_unfinished_today() {
        let s = goal_status(
            20000,
            &rows(&[
                ("2026-09-19", 30000),
                ("2026-09-20", 25000),
                ("2026-09-21", 500),
            ]),
            "2026-09-21",
        );
        assert!(!s.today_met);
        assert_eq!(s.streak, 2, "零点不该把昨天的纪录抹掉");
        assert_eq!(s.best, 2);
    }

    /// 达标日必须连续：中间断一天就重新计数，但最长纪录仍记住断点之前。
    #[test]
    fn streak_breaks_on_gap_and_best_keeps_longest_run() {
        let s = goal_status(
            20000,
            &rows(&[
                ("2026-09-15", 40000),
                ("2026-09-16", 40000),
                ("2026-09-17", 40000),
                ("2026-09-18", 1),
                ("2026-09-19", 1),
                ("2026-09-20", 40000),
                ("2026-09-21", 40000),
            ]),
            "2026-09-21",
        );
        assert!(s.today_met);
        assert_eq!(s.streak, 2, "断了两天之后不该接上更早的三连");
        assert_eq!(s.best, 3);
        assert_eq!(s.days.len(), 7);
        assert_eq!(s.days[6], ("2026-09-21".to_string(), 40000, true));
        assert_eq!(s.days[4].0, "2026-09-19");
        assert!(!s.days[4].2);
    }

    /// 空库 / 没有当日记录：全 0，不 panic、不除零；非法目标值按 1 处理。
    #[test]
    fn goal_status_handles_empty_history() {
        let s = goal_status(20000, &[], "2026-09-21");
        assert_eq!((s.today, s.streak, s.best, s.today_met), (0, 0, 0, false));
        assert_eq!(s.days.len(), 7);
        assert!(s.days.iter().all(|(_, c, met)| *c == 0 && !*met));
        assert_eq!(
            goal_status(0, &rows(&[("2026-09-21", 1)]), "2026-09-21").streak,
            1,
            "目标 0 会让任何非零天数都永不达标，按 1 兜住"
        );
    }

    /// 缺口日期必须按 0 算：零活动的那天没有 daily_counts 行，如果按"行"数连续，
    /// 9/1 与 9/3 两次达标会被报成「最长 2 天」—— 而 streak 走的是日期轴，
    /// 两个口径会自相矛盾。
    #[test]
    fn goal_best_counts_calendar_days_not_rows() {
        let goal = 20000;
        let s = goal_status(
            goal,
            &rows(&[
                ("2026-08-30", 25000),
                // 8/31 整天没有活动 → 库里没有这一行
                ("2026-09-01", 25000),
                ("2026-09-02", 25000),
                ("2026-09-03", 25000),
            ]),
            "2026-09-03",
        );
        assert_eq!(s.streak, 3, "9/1..9/3 是真连续");
        assert_eq!(s.best, 3, "缺口把 8/30 与 9/1 隔开，最长只能是 3");
    }

    /// 周报锚点：永远是「上一个完整周」，且同一周内天天算出同一个区间。
    #[test]
    fn last_finished_week_is_the_previous_full_week() {
        use chrono::NaiveDate as D;
        let d = |y, m, day| D::from_ymd_opt(y, m, day).unwrap();
        // 2026-09-22 是周二 → 上周一 09-14 ~ 上周日 09-20
        assert_eq!(
            last_finished_week(d(2026, 9, 22)),
            (d(2026, 9, 14), d(2026, 9, 20))
        );
        // 周一当天：本周一 09-21，上一个整周仍是 09-14~09-20（与周二同区间 → 一周只生成一次）
        assert_eq!(
            last_finished_week(d(2026, 9, 21)),
            (d(2026, 9, 14), d(2026, 9, 20))
        );
        // 周日当天不能把自己这一周算进去
        assert_eq!(
            last_finished_week(d(2026, 9, 20)),
            (d(2026, 9, 7), d(2026, 9, 13))
        );
        // 跨年
        assert_eq!(
            last_finished_week(d(2026, 1, 1)),
            (d(2025, 12, 22), d(2025, 12, 28))
        );
    }

    /// 写一份配置到 `target/` 下（固定文件名，跨运行复用、`cargo clean` 带走，
    /// 不进 `%TEMP%`），并读回来。绕开 `instance()`/`set()`：那会叫醒进程级的
    /// 去抖保存线程，把文件写进别的用例已经删掉的目录里。
    /// `tag` 必须每个用例各不相同 ——  cargo 并行跑用例，共用一个文件就是互相盖。
    fn cfg_from(tag: &str, body: &str) -> FocusFlowConfig {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../../target/ff_rest_cfg_{tag}.ini"));
        std::fs::write(&path, body).unwrap();
        FocusFlowConfig::load(&path).unwrap()
    }

    /// 30 分钟窗口 / 100 次阈值 / 10 分钟冷却 / 每 60 秒最多判一次
    const REST_30: &str = "[rest]\nenabled = true\nwindow_minutes = 30\nkey_threshold = 100\n\
                           cooldown_minutes = 10\nrest_seconds = 20\ncheck_interval = 60\n";

    /// 久坐提醒必须"铺满整个窗口 + 事件数过阈值"两个条件同时成立才发。
    #[test]
    fn rest_monitor_needs_a_full_window_of_sustained_work() {
        let cfg = cfg_from("full_window", REST_30);
        let mut m = RestMonitor::default();
        let t0 = Instant::now();
        let at = |mins: u64| t0 + Duration::from_secs(mins * 60);
        for i in 0..6u64 {
            assert!(
                m.observe(at(i * 5), (i * 100) as i64, &cfg).is_none(),
                "第 {i} 次：30 分钟的窗口还没铺满，不该提醒（刚开机就催人休息是最烦的那种）"
            );
        }
        let n = m
            .observe(at(30), 600, &cfg)
            .expect("窗口铺满且事件数过阈值，该提醒了");
        assert_eq!(
            (n.window_minutes, n.events_in_window, n.rest_seconds),
            (30, 600, 20),
            "载荷里的三个数都要来自配置"
        );
    }

    /// 冷却期到点前不得重复提醒；提醒之后窗口要重新攒满。
    #[test]
    fn rest_monitor_respects_cooldown() {
        let cfg = cfg_from(
            "cooldown",
            "[rest]\nenabled = true\nwindow_minutes = 1\nkey_threshold = 10\n\
             cooldown_minutes = 10\nrest_seconds = 20\ncheck_interval = 10\n",
        );
        let mut m = RestMonitor::default();
        let t0 = Instant::now();
        let fired: Vec<u64> = (1..=12u64)
            .filter(|&k| {
                m.observe(t0 + Duration::from_secs(k * 60), (k * 50) as i64, &cfg)
                    .is_some()
            })
            .collect();
        assert_eq!(
            fired,
            vec![2, 12],
            "第 2 分钟才可能第一次发（先要铺满一个 1 分钟的窗口）；之后每分钟阈值都够，\
             但 10 分钟冷却压着，直到第 12 分钟才再发一次"
        );
    }

    /// `[rest] enabled = false` 是真开关；今日计数跨零点归零不能当成"刚才很活跃"。
    #[test]
    fn rest_monitor_honours_the_switch_and_a_midnight_reset() {
        let off = cfg_from(
            "switch_off",
            "[rest]\nenabled = false\nwindow_minutes = 1\nkey_threshold = 10\n\
             cooldown_minutes = 1\nrest_seconds = 20\ncheck_interval = 10\n",
        );
        let mut m = RestMonitor::default();
        let t0 = Instant::now();
        assert!(m.observe(t0, 0, &off).is_none());
        assert!(
            m.observe(t0 + Duration::from_secs(600), 999_999, &off)
                .is_none(),
            "enabled=false 时事件数再高也不该提醒（否则又是一个改了就生效不了的假开关）"
        );

        // 同一个 monitor 换配置：参数每次判定现读，改文件不必重启
        let cfg = cfg_from("midnight", REST_30);
        let mut m = RestMonitor::default();
        for i in 0..6u64 {
            m.observe(t0 + Duration::from_secs(i * 300), (i * 100) as i64, &cfg);
        }
        assert!(
            m.observe(t0 + Duration::from_secs(1800), 10, &cfg)
                .is_none(),
            "计数归零的那一 tick 不能拿昨天的累计当证据"
        );
        assert!(
            m.observe(t0 + Duration::from_secs(2100), 60, &cfg)
                .is_none(),
            "归零后窗口要重新铺满"
        );
        assert!(
            m.observe(t0 + Duration::from_secs(3600), 700, &cfg)
                .is_some(),
            "重新攒满 30 分钟之后应当能提醒"
        );
    }
}
