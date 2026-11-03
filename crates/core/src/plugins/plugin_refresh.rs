//! 面板刷新节奏与慢熔断（方案 3.9、路线图第 10 步）。
//!
//! 为什么这件事要住在 core 而不是画它的那一页：本页所有"什么时候该重算"的判断都必须是
//! 一条能脱离窗口测到的规则（看板那一页的 `should_refresh` 同一手法）。写在 UI 里，
//! "声明了 0 还在每秒刷"与"慢插件把主线程拖成幻灯片"这两族缺陷就没有任何一条用例摸得到。
//!
//! 三条规则，都对着方案 3.9 那张表：
//! · 每个插件按自己声明的 `PLUGIN_REFRESH_MS` 自刷，**缺省 0 = 只在动作之后刷**；
//! · 最近 20 次里有 3 次超过 5 ms ⇒ 降频到 5 s，并在页面上标"该插件较慢"；
//! · 样本窗口自己会滚掉 ⇒ 慢熔断是会**自动解除**的（方案只给坏熔断写了"持续到手动或重启"，
//!   慢的那一档没写终止条件，而一个偶发卡顿被永久标成"慢"显然不是本意）。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// 慢的阈值（方案 3.9：超过 5 ms 肉眼可见，60 fps 一帧才 16.7 ms）。
pub const SLOW_MS: u64 = 5;

/// 样本窗口长度与触发次数（20 次约 20 秒，够排除偶发抖动；3 次才降频）。
pub const SAMPLE_WINDOW: usize = 20;
pub const SLOW_TRIPS: usize = 3;

/// 进了慢熔断之后的间隔。
pub const THROTTLED_MS: u64 = 5_000;

/// 缺省自刷间隔：`0` = 只在动作之后刷（方案 3.9 那一行"缺省 0"）。
pub const DEFAULT_REFRESH_MS: u64 = 0;

/// 一个插件面板的刷新账本。
#[derive(Debug, Clone)]
pub struct Cadence {
    /// 插件自己声明的那一档（`PLUGIN_REFRESH_MS`）
    declared_ms: u64,
    /// 最近若干次"这一刷慢不慢"，最旧在前面，长度封顶 [`SAMPLE_WINDOW`]
    samples: VecDeque<bool>,
    /// 上一次真的重算是什么时候
    last: Option<Instant>,
    /// `focusflow.invalidate()` 或动作后置的那面旗
    dirty: bool,
}

impl Cadence {
    pub fn new(declared_ms: u64) -> Self {
        Cadence {
            declared_ms,
            samples: VecDeque::new(),
            last: None,
            dirty: false,
        }
    }

    /// 现在该不该重算这一格。
    ///
    /// 返回真的同时把"上次重算"推到 `now` 吗？**不**：那由 [`Self::record`] 在真的跑完
    /// `get_view()` 之后做。分开的理由是这一族的缺陷来源 —— 先说"该刷"再刷失败，
    /// 时间戳却已经推进去，症状是"这一格永远不重算，而页面上写着刚才刷过"。
    pub fn due(&self, now: Instant) -> bool {
        if self.dirty {
            return true;
        }
        // 缺省 0 = 不自动刷：动作之外，这一格就该一直停在那份旧视图上
        if self.declared_ms == 0 {
            return false;
        }
        match self.last {
            None => true,
            Some(at) => {
                now.saturating_duration_since(at) >= Duration::from_millis(self.interval_ms())
            }
        }
    }

    /// 下一次真正重算之后要交出去的那一格：跑完 `get_view()` 才调这里。
    ///
    /// `elapsed` 是这一次重算花了多久；同时把 `dirty` 清掉、把时间戳推进去。
    pub fn record(&mut self, now: Instant, elapsed: Duration) {
        self.last = Some(now);
        self.dirty = false;
        self.samples
            .push_back(elapsed > Duration::from_millis(SLOW_MS));
        while self.samples.len() > SAMPLE_WINDOW {
            self.samples.pop_front();
        }
    }

    /// 主动要求重算（`focusflow.invalidate()` 与"动作之后立刻刷"都走这里）。
    pub fn invalidate(&mut self) {
        self.dirty = true;
    }

    /// 脏旗有没有被点起来（用例与日志用）。
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// 这一段是不是慢（页面上要标"该插件较慢"）。
    pub fn is_slow(&self) -> bool {
        self.samples.iter().filter(|slow| **slow).count() >= SLOW_TRIPS
    }

    /// 现在实际按多少毫秒自刷：慢熔断期间至少 5 s。
    ///
    /// 是 `max` 不是"直接 5000"：作者声明 8000 的插件被降频不该刷得更勤 ——
    /// 降频的意思是**往慢的方向挪**，不是把声明值覆盖掉。
    pub fn interval_ms(&self) -> u64 {
        if self.is_slow() {
            self.declared_ms.max(THROTTLED_MS)
        } else {
            self.declared_ms
        }
    }

    pub fn declared_ms(&self) -> u64 {
        self.declared_ms
    }

    /// 窗口里现在有几个慢样本（诊断那一格要显示"最近耗时"，这条也给用例当观察量）。
    pub fn slow_samples(&self) -> usize {
        self.samples.iter().filter(|slow| **slow).count()
    }

    pub fn samples_len(&self) -> usize {
        self.samples.len()
    }

    /// 声明变了（作者改了 `.lua` 并重载）要换一份账本，别让旧样本压着新声明。
    pub fn declared_is(&self, raw: u64) -> bool {
        self.declared_ms == raw
    }
}

/// 进程级的"这个插件要求重算"的件口。
///
/// 为什么不在 Lua 里直接改 `Cadence`：`focusflow.invalidate()` 那个闭包能碰到的只有
/// 它捕获的那份字符串，而账本住在管理器里（GUI 线程专用，不跨线程共享）。与按键攒批
/// 同一形状：投递方只写一个名字，取件那一跳在 GUI 线程。
static DIRTY: std::sync::OnceLock<std::sync::Mutex<Vec<String>>> = std::sync::OnceLock::new();

fn dirty_slot() -> &'static std::sync::Mutex<Vec<String>> {
    DIRTY.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// 插件说"我改了，下一次该重算"。
pub fn mark_dirty(stem: &str) {
    let mut q = dirty_slot().lock().unwrap_or_else(|e| e.into_inner());
    if !q.iter().any(|s| s == stem) {
        q.push(stem.to_string());
    }
}

/// 取走当前要求重算的那批名字（管理器按文件名对到账本上）。
pub fn take_dirty() -> Vec<String> {
    let mut q = dirty_slot().lock().unwrap_or_else(|e| e.into_inner());
    std::mem::take(&mut *q)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(offset_ms: u64) -> Instant {
        // Instant 不能构造绝对时间：从同一个起点往前推，就够用整段用例
        let base = Instant::now();
        base + Duration::from_millis(offset_ms)
    }

    /// 缺省那一档就是"别自动刷"：作者没写 `PLUGIN_REFRESH_MS` 时，视图停在加载与动作后那一份。
    ///
    /// 这一条挡住的是今天的行为（面板打开就每秒全量重算）。加载与"打开这一页"那两处
    /// 本来就会算一遍（`load_plugin` 里的 `read_view` 与 `open_plugin_view` 里的
    /// `refresh_view`），所以这一格连第一次都不必点头。
    #[test]
    fn declared_zero_never_refreshes_on_its_own() {
        let base = Instant::now();
        let mut c = Cadence::new(0);
        assert!(
            !c.due(base),
            "声明 0 就不该自己刷：第一次是加载与开页那两步给的，不是这一格点的头"
        );
        c.invalidate();
        assert!(
            c.due(base + Duration::from_secs(60)),
            "invalidate 之后要立刻刷"
        );
        c.record(base + Duration::from_secs(60), Duration::from_millis(1));
        assert!(
            !c.due(base + Duration::from_secs(120)),
            "刷过之后回到不自动刷"
        );
        assert!(!c.is_dirty(), "刷过之后那面旗要落下来");
    }

    /// 按声明的间隔到点才刷；差 1 ms 不算到点。
    #[test]
    fn a_declared_interval_fires_on_the_tick_not_before() {
        let base = Instant::now();
        let mut c = Cadence::new(1_500);
        assert!(c.due(base));
        c.record(base, Duration::from_millis(2));
        assert!(
            !c.due(base + Duration::from_millis(1_499)),
            "差 1 ms 不该刷"
        );
        assert!(c.due(base + Duration::from_millis(1_500)));
        // 没跑过一次 record 之前，时间戳不该自己前进（否则刷失败就永远不再重算）
        assert!(c.due(base + Duration::from_millis(9_000)));
    }

    /// 慢熔断：最近 20 次里 3 次超 5 ms ⇒ 间隔至少 5 s，并在页面上该标"较慢"。
    ///
    /// 两侧都要断：第 2 次不许触发（偶发抖动），第 3 次必须触发（否则这一格白设）；
    /// 触发之后样本滚掉要能自动解除（方案只给"坏"那一档写了终止条件）。
    #[test]
    fn three_slow_runs_out_of_twenty_trip_the_throttle_and_the_window_clears_it() {
        let mut c = Cadence::new(1_000);
        let slow = Duration::from_millis(SLOW_MS + 1);
        let fast = Duration::from_millis(1);
        c.record(at(0), fast);
        c.record(at(0), slow);
        assert!(!c.is_slow(), "两次慢不该降频（偶发抖动）");
        assert_eq!(c.interval_ms(), 1_000);
        c.record(at(0), slow);
        c.record(at(0), slow);
        assert!(c.is_slow(), "第三次慢该降频");
        assert_eq!(
            c.interval_ms(),
            THROTTLED_MS,
            "降频到 5 s，实得 {}",
            c.interval_ms()
        );
        // 恰好等于阈值不算慢（"> 5 ms" 那句是写死的）
        let mut edge = Cadence::new(1_000);
        for _ in 0..3 {
            edge.record(at(0), Duration::from_millis(SLOW_MS));
        }
        assert!(!edge.is_slow(), "等于 5 ms 不该判成慢");
        // 窗口只有 20 格：往后再灌 20 个快样本，那 3 个慢的就该滚掉
        for _ in 0..20 {
            c.record(at(0), fast);
        }
        assert_eq!(c.samples_len(), SAMPLE_WINDOW, "样本窗口不许无限长");
        assert!(!c.is_slow(), "慢样本滚掉之后要自动解除");
        assert_eq!(c.interval_ms(), 1_000, "解除后回到声明值");
    }

    /// 降频是"往慢的方向挪"，不是把声明值覆盖掉：声明 8 s 的插件不该被刷得更勤。
    #[test]
    fn the_throttle_never_makes_a_slower_declaration_faster() {
        let mut c = Cadence::new(8_000);
        let slow = Duration::from_millis(9_000);
        for _ in 0..SLOW_TRIPS {
            c.record(at(0), slow);
        }
        assert!(c.is_slow());
        assert_eq!(
            c.interval_ms(),
            8_000,
            "声明比 5 s 还慢，降频不该反过来催它: {}",
            c.interval_ms()
        );
    }

    /// 件口只记名字不记 VM：同一个插件连着要求重算也只排一格。
    ///
    /// 这一条是防"每秒 push 一条"：取件那一跳按名字去重，攒久了就是一片重复重算。
    #[test]
    fn the_dirty_slot_carries_one_entry_per_plugin() {
        let _lock = crate::paths::test_app_dir_lock();
        let before = take_dirty();
        mark_dirty("refresh_probe_a");
        mark_dirty("refresh_probe_a");
        mark_dirty("refresh_probe_b");
        let got = take_dirty();
        assert_eq!(got, vec!["refresh_probe_a", "refresh_probe_b"], "{got:?}");
        assert!(
            take_dirty().is_empty(),
            "取过就该空，第二遍拿到的是重复投递: {:?}",
            take_dirty()
        );
        let _ = before;
    }

    /// 阈值那几个数字是判断"插件慢不慢"的唯一尺子：改动必须在这里被看见。
    #[test]
    fn the_ruler_names_its_own_numbers() {
        assert_eq!(SLOW_MS, 5, "方案 3.9 定的是 5 ms");
        assert_eq!((SAMPLE_WINDOW, SLOW_TRIPS), (20, 3), "20 次里 3 次");
        assert_eq!(THROTTLED_MS, 5_000, "降频到 5 s");
        assert_eq!(DEFAULT_REFRESH_MS, 0, "缺省只在动作后刷");
    }
}
