//! 每插件的定时器与事件（方案 3.6）。
//!
//! 与 `scheduler_*` 那一组（内置定时任务：到点启动一个进程）**分家**：那一组语义不对、
//! 而且是进程级单例，第三方插件不能用。这一份是 per-plugin 的：
//!
//! · `every(sec, fn)` 周期回调，`after(sec, fn)` 一次性，`cancel(handle)` 主动取消；
//! · `on("tick", fn)` 每分钟一次、`on("day_change", fn)` 日期变了那一拍一次；
//! · **卸载/出错一律全取消**，不依赖插件自己写 `cleanup`（3.6 那句"不依赖"是重点：
//!   约定归还这一族在本仓已经踩过一次坑，见 `host.rs` 顶上那段 owner 计数的注释）。
//!
//! 回调函数本身**不住在这里**：`mlua::Function` 不是 `Send`，而这份表要能被进程级的
//! 投递路径问到，所以这里只存"哪个 owner 的哪个句柄、什么时候到期"，函数留在插件自己的
//! Lua 里那一格 `_ff_timers[handle]`（见 `host::install_timer_api`）。
//! 跑回调的那一步在 GUI 线程（`PluginManager::pump_timers`），与按键攒批同一个模型 ——
//! `Lua` 不是 `Send`，跨线程只能投请求，不能投 VM。
//!
//! 时钟不进内部状态：[`Board::collect_due`] 收的是调用方算好的 `now`（`Instant`）与
//! `minute_key`/`day_key` 两个整数。这样"每分钟一次""跨过午夜那一次"这类判据能在用例里
//! 直接造出来，而不是等日历走到那天（与 `dashboard::summarize` 那种"纯的那一半"同一个手法）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// 一个插件最多挂几个定时器（方案 3.6 那张表）。
///
/// 上限是给"忘了 cancel 的插件"准备的：没有它，一个每 0.1 秒自刷一次的写法会把
/// 每一拍的回调排成一片，而界面上看不出是谁。
pub const MAX_TIMERS_PER_PLUGIN: usize = 8;

/// 最短间隔（秒）。方案 3.6 定的是 `N >= 1`。
pub const MIN_INTERVAL_SECS: u64 = 1;

/// 一个句柄是哪一类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 每 N 秒
    Every,
    /// N 秒后来一次，然后自己消失
    After,
    /// 每分钟一次（`on("tick", fn)`）
    Tick,
    /// 日期变了那一次（`on("day_change", fn)`）
    DayChange,
    /// 前台应用换了名字那一次（`on("app_change", fn)`）。
    ///
    /// 与 `Tick` / `DayChange` 不同，它**不由墙钟决定**：触发它的是采样线程投进件口的那个
    /// 名字，所以 [`Board::collect_due`] 对它一律不回（那一拍没有新名字就是没变化），
    /// 取走它的载荷走 [`Board::collect_app_change`] 那一条路。
    AppChange,
}

/// 一格定时器。
#[derive(Debug, Clone)]
struct Entry {
    handle: u64,
    kind: Kind,
    /// `Every` 的周期（秒）；其余用不上
    secs: u64,
    /// 到这一刻就跑。`Tick` / `DayChange` 走的是墙钟，不看这个字段
    due: Option<std::time::Instant>,
    /// 上一次跑的时候的分钟桶 / 日桶（事件类用它判"这一分钟、这一天已经跑过了"）
    last_minute: Option<i64>,
    last_day: Option<i64>,
    /// AppChange 用来判"这个名字已经给过这一格了"
    last_app: Option<String>,
}

/// 一个插件的定时器表。
#[derive(Debug, Default)]
pub struct Board {
    entries: HashMap<u64, Entry>,
    next_handle: u64,
    /// 本次运行跑过多少个回调。**用例 4 的观察量**：停用之后这个数不许再涨。
    /// 没有这一格，"回调停了"这件事只能靠往 Lua 里塞一个全局计数器来证明（那是造假）。
    fired: u64,
}

impl Board {
    pub fn new() -> Self {
        Self::default()
    }

    /// 排一格定时器，交回句柄。三种拒绝各有各的话，不许折成同一个 `Err`：
    /// 作者看到的是"我写的哪一个字错了"，而不是"定时器没起来"。
    pub fn arm(&mut self, kind: Kind, secs: u64, now: std::time::Instant) -> Result<u64, String> {
        if !matches!(kind, Kind::Tick | Kind::DayChange | Kind::AppChange)
            && secs < MIN_INTERVAL_SECS
        {
            return Err(format!(
                "定时器最短 {MIN_INTERVAL_SECS} 秒（收到 {secs}）：要更密的刷新请走 PLUGIN_REFRESH_MS"
            ));
        }
        if self.entries.len() >= MAX_TIMERS_PER_PLUGIN {
            return Err(format!(
                "一个插件最多挂 {} 格定时器，这一份已经挂满了：请 cancel 掉不要的那一格",
                MAX_TIMERS_PER_PLUGIN
            ));
        }
        self.next_handle += 1;
        let handle = self.next_handle;
        let due = match kind {
            Kind::Every => Some(now + std::time::Duration::from_secs(secs)),
            Kind::After => Some(now + std::time::Duration::from_secs(secs)),
            Kind::Tick | Kind::DayChange | Kind::AppChange => None,
        };
        self.entries.insert(
            handle,
            Entry {
                handle,
                kind,
                secs,
                due,
                last_minute: None,
                last_day: None,
                last_app: None,
            },
        );
        Ok(handle)
    }

    pub fn cancel(&mut self, handle: u64) -> bool {
        self.entries.remove(&handle).is_some()
    }

    /// 取走这一拍该跑的句柄：一次性跑完即摘，周期的重新排到下一轮。
    ///
    /// 交回的是**句柄清单**而不是一批闭包：跑回调必须回到那份 Lua 上，而这里没有 Lua
    /// （见模块顶那句）。
    pub fn collect_due(
        &mut self,
        now: std::time::Instant,
        minute_key: i64,
        day_key: i64,
    ) -> Vec<u64> {
        let mut due: Vec<u64> = self
            .entries
            .values()
            .filter(|e| match e.kind {
                Kind::Every | Kind::After => e.due.map(|d| d <= now).unwrap_or(false),
                Kind::Tick => e.last_minute != Some(minute_key),
                Kind::DayChange => e.last_day != Some(day_key),
                // 不是墙钟决定的：载荷没来就是这一拍什么都没变（见 collect_app_change）
                Kind::AppChange => false,
            })
            .map(|e| e.handle)
            .collect();
        // 顺序稳定：插件挂了多格时"这一拍先跑谁"不该随 HashMap 的哈希序变
        // （本仓为同一件事在排行与看板那两处各收过一次，`cmp_rank_desc` 那一份）。
        due.sort_unstable();
        for handle in &due {
            let Some(entry) = self.entries.get_mut(handle) else {
                continue;
            };
            match entry.kind {
                Kind::After => {
                    // 先摘再跑：跑回调的那一刻插件可能又 arm 一格新的，摘在后面会把新的也算进去
                    self.entries.remove(handle);
                }
                Kind::Every => {
                    entry.due = Some(now + std::time::Duration::from_secs(entry.secs));
                }
                Kind::Tick => entry.last_minute = Some(minute_key),
                Kind::DayChange => entry.last_day = Some(day_key),
                Kind::AppChange => {}
            }
        }
        self.fired += due.len() as u64;
        due
    }

    /// 前台应用换了名字这一拍该跑的句柄：**只回没见过这个名字的那几格**。
    ///
    /// 去重按格而不是按件口：两个插件都订了 app_change 时，件口那边只投得下一次
    /// （同名不再投），而这里要保证**每一格都拿到那一次** —— 所以记的是"这一格见过谁"。
    pub fn collect_app_change(&mut self, app: &str) -> Vec<u64> {
        let mut due: Vec<u64> = self
            .entries
            .values()
            .filter(|e| e.kind == Kind::AppChange && e.last_app.as_deref() != Some(app))
            .map(|e| e.handle)
            .collect();
        due.sort_unstable();
        for handle in &due {
            if let Some(entry) = self.entries.get_mut(handle) {
                entry.last_app = Some(app.to_string());
            }
        }
        self.fired += due.len() as u64;
        due
    }
    /// 本次运行跑过多少个回调（观察量，见 `Board::fired` 那段）。
    pub fn fired(&self) -> u64 {
        self.fired
    }

    /// 还挂着几格（插件页那一格要说"这个插件占了几格定时器"）。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 这个句柄还在排程表里吗。
    ///
    /// `collect_due` 会把一次性（`After`）的格子摘掉，而 Lua 那一侧的
    /// `_ff_timers[句柄]` 仍攥着那个闭包 —— 调用方据此知道哪一格已经跑完、
    /// 该把 Lua 表里那一项也清掉（否则每次 `after` 都留下一个永远不再用的函数）。
    pub fn is_armed(&self, handle: u64) -> bool {
        self.entries.contains_key(&handle)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 进程级的登记表，按**插件文件名（stem）**分格。
///
/// 与 `plugin_store` 同一族设计：键是文件名而不是展示名（两个插件同名展示名是常见手误），
/// 也不是插件给的句柄。卸载与出错那两条路都从这里摘掉整格 —— 这是"自动全部取消"
/// 唯一落得了地的地方：插件的 Lua 状态都没了，它自己写不写 `cleanup` 都取消不了什么。
type Registry = HashMap<String, Arc<Mutex<Board>>>;

static BOARDS: std::sync::OnceLock<Mutex<Registry>> = std::sync::OnceLock::new();

fn registry() -> &'static Mutex<Registry> {
    BOARDS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 这一份插件的定时器表（没有就建一格空的）。
pub fn board_for(owner: &str) -> Arc<Mutex<Board>> {
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    reg.entry(owner.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(Board::new())))
        .clone()
}

/// 摘掉这一份插件的全部定时器（卸载、出错、重载换字节之前）。
///
/// 交回摘掉了几格：`unload` 那一条用例要能看见"整格没了"，而不只是"函数跑过了"。
pub fn forget(owner: &str) -> usize {
    let removed = {
        let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
        reg.remove(owner)
    };
    match removed {
        Some(board) => board.lock().map(|b| b.len()).unwrap_or(0),
        None => 0,
    }
}

/// 本进程现在挂着定时器的插件清单（诊断与用例用；按名字排序，别随哈希序变）。
#[cfg(any(test, feature = "test-utils"))]
pub fn owners_for_test() -> Vec<String> {
    let mut names: Vec<String> = registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .cloned()
        .collect();
    names.sort();
    names
}

/// 跑回调的那一根线程，**按插件分格**（每次跑之前记一笔）。
///
/// 这不是给产品用的，是给验收 5 那句「回调在线程 id == 主线程」用的：Lua 那一头拿不到
/// 线程身份，而"哪一段代码在哪根线程上跑"这件事只能在 Rust 侧留证。
/// 按 owner 分格而不是全局一格：并行的用例各自跑各自的插件，共用一格会让别人的读数
/// 顶掉自己的（那样红的是量具，不是产品）。
static FIRE_THREADS: std::sync::OnceLock<Mutex<HashMap<String, String>>> =
    std::sync::OnceLock::new();

fn fire_thread_slot() -> &'static Mutex<HashMap<String, String>> {
    FIRE_THREADS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn note_fire_thread(owner: &str) {
    let id = format!("{:?}", std::thread::current().id());
    fire_thread_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(owner.to_string(), id);
}

/// 这一份插件最近一次被跑的回调在哪根线程（从没跑过就是 `None`，不是"某个假定的默认值"）。
#[cfg(any(test, feature = "test-utils"))]
pub fn last_fire_thread_for_test(owner: &str) -> Option<String> {
    fire_thread_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(owner)
        .cloned()
}

/// 墙钟的两个桶：分钟与日。`pump_timers` 每次算一遍，事件类按它判"这一分钟跑过没有"。
///
/// 为什么按桶而不是按"距上次跑了多久"：跨过午夜那一拍如果按间隔算，一个恰好挂在
/// 23:59:30 的订阅会在同一天里跑两次 `day_change`（第一次算"今天"，第二次算"明天"），
/// 而按日桶就是变一次跑一次。
pub fn wall_buckets(now: &chrono::DateTime<chrono::Local>) -> (i64, i64) {
    use chrono::{Datelike, Timelike};
    let minute_key = now.date_naive().num_days_from_ce() as i64 * 1440
        + (now.hour() as i64) * 60
        + now.minute() as i64;
    let day_key = now.date_naive().num_days_from_ce() as i64;
    (minute_key, day_key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn at(seconds_from_now: u64) -> Instant {
        Instant::now() + Duration::from_secs(seconds_from_now)
    }

    /// 到点才跑；周期那格跑完自动排下一轮，一次性那格跑完就没了。
    #[test]
    fn every_refires_while_after_fires_once() {
        let now = Instant::now();
        let mut board = Board::new();
        let every = board.arm(Kind::Every, 2, now).expect("arm every");
        let after = board.arm(Kind::After, 2, now).expect("arm after");
        assert!(board.collect_due(now, 0, 0).is_empty(), "还没到点不该跑");
        let later = at(0);
        // 用同一时刻的"已过 2 秒"来推进：Instant 不能构造绝对时间，靠 now 的位移即可
        let due = board.collect_due(later + Duration::from_secs(2), 0, 0);
        assert_eq!(due, vec![every, after], "两格都该到期，且顺序是句柄序");
        assert_eq!(board.len(), 1, "一次性那格该自己消失");
        assert_eq!(board.fired(), 2);
        // 下一轮：周期的再次到期，一次性已经不在表里
        let due2 = board.collect_due(later + Duration::from_secs(4), 0, 0);
        assert_eq!(due2, vec![every]);
        assert_eq!(board.fired(), 3);
    }

    /// 一次性跑完就不再 armed：调用方据此清掉 Lua 那张回调表里的闭包
    /// （插件反复 `after` 时那张表只增不减）。周期与事件那几格必须仍然 armed。
    #[test]
    fn a_fired_one_shot_is_no_longer_armed() {
        let now = Instant::now();
        let mut board = Board::new();
        let after = board.arm(Kind::After, 1, now).unwrap();
        let every = board.arm(Kind::Every, 1, now).unwrap();
        let tick = board.arm(Kind::Tick, 0, now).unwrap();
        assert!(board.is_armed(after) && board.is_armed(every) && board.is_armed(tick));
        let _ = board.collect_due(now + Duration::from_secs(2), 10, 20);
        assert!(!board.is_armed(after), "一次性跑完必须从排程表摘掉");
        assert!(board.is_armed(every), "周期那格要留着");
        assert!(board.is_armed(tick), "事件那格要留着");
    }

    /// 句柄顺序稳定：同一拍里先跑谁不能随哈希序变（挂了多格的插件每次跑的顺序不同，
    /// 症状就是"有时候存了两次"这类没法复现的问题）。
    #[test]
    fn the_due_list_comes_back_in_handle_order() {
        let now = Instant::now();
        let mut board = Board::new();
        let a = board.arm(Kind::Every, 1, now).expect("a");
        let b = board.arm(Kind::Every, 1, now).expect("b");
        let c = board.arm(Kind::Every, 1, now).expect("c");
        for _ in 0..20 {
            let mut fresh = Board::new();
            let x = fresh.arm(Kind::Every, 1, now).unwrap();
            let y = fresh.arm(Kind::Every, 1, now).unwrap();
            let z = fresh.arm(Kind::Every, 1, now).unwrap();
            let _ = (x, y, z);
        }
        let due = board.collect_due(now + Duration::from_secs(1), 0, 0);
        assert_eq!(due, vec![a, b, c], "{due:?} 不是句柄序");
    }

    /// 两类事件按**桶**判：同一分钟重复问不再跑，跨过午夜那一次只跑一次。
    #[test]
    fn the_events_fire_once_per_bucket_not_once_per_poll() {
        let now = Instant::now();
        let mut board = Board::new();
        let tick = board.arm(Kind::Tick, 0, now).expect("tick");
        let day = board.arm(Kind::DayChange, 0, now).expect("day_change");
        // 第一拍就把当前桶记下来（与"已经跑过"同一含义），所以第一次是 2 格都跑
        let first = board.collect_due(now, 100, 7);
        assert_eq!(first, vec![tick, day], "{first:?}");
        assert!(
            board.collect_due(now, 100, 7).is_empty(),
            "同一分钟、同一天不该再跑一次"
        );
        assert_eq!(
            board.collect_due(now, 101, 7),
            vec![tick],
            "下一分钟只该跑 tick"
        );
        assert_eq!(
            board.collect_due(now, 102, 8),
            vec![tick, day],
            "跨天那一拍两格都该跑"
        );
        assert!(board.collect_due(now, 102, 8).is_empty());
    }

    /// 三种拒绝各有各的话，且都不许悄悄成功。
    #[test]
    fn arming_refuses_too_fast_and_the_ninth_slot_saying_which() {
        let now = Instant::now();
        let mut board = Board::new();
        let e = board.arm(Kind::Every, 0, now).unwrap_err();
        assert!(
            e.contains("最短") && e.contains("PLUGIN_REFRESH_MS"),
            "太密的间隔要说清边界在哪、该走哪一条：{e}"
        );
        for i in 1..=MAX_TIMERS_PER_PLUGIN {
            board
                .arm(Kind::Every, i as u64 + 1, now)
                .unwrap_or_else(|e| panic!("第 {i} 格该排得进去: {e}"));
        }
        assert_eq!(board.len(), MAX_TIMERS_PER_PLUGIN);
        let full = board.arm(Kind::Every, 30, now).unwrap_err();
        assert!(
            full.contains("最多") && full.contains("cancel"),
            "挂满要说清上限和出路，不能只说失败: {full}"
        );
        assert_eq!(board.len(), MAX_TIMERS_PER_PLUGIN, "被拒的那一格不许占位");
    }

    /// `cancel` 摘掉是真的少了格子，句柄不复用（拿旧句柄再 cancel 是 false 而不是误杀新格）。
    #[test]
    fn cancelling_removes_the_slot_and_handles_are_never_reused() {
        let now = Instant::now();
        let mut board = Board::new();
        let a = board.arm(Kind::Every, 5, now).expect("a");
        assert!(board.cancel(a), "刚排进去的该取消得掉");
        assert!(!board.cancel(a), "取消两次是空操作");
        assert_eq!(board.len(), 0);
        let b = board.arm(Kind::Every, 5, now).expect("b");
        assert_ne!(a, b, "句柄复用会让插件拿旧句柄杀掉新定时器");
    }

    /// 进程级那一份表按 owner 分格，而 `forget` 是**整格**摘掉 —— 这就是"卸载即取消"
    /// 不依赖插件写 cleanup 的那一句。
    #[test]
    fn forgetting_an_owner_removes_the_whole_slot() {
        let _lock = crate::paths::test_app_dir_lock();
        let now = Instant::now();
        let board = board_for("timer_owner_a");
        board
            .lock()
            .unwrap()
            .arm(Kind::Every, 30, now)
            .expect("arm");
        board.lock().unwrap().arm(Kind::Tick, 0, now).expect("arm");
        let other = board_for("timer_owner_b");
        other
            .lock()
            .unwrap()
            .arm(Kind::Every, 30, now)
            .expect("arm");
        assert!(
            owners_for_test().contains(&"timer_owner_a".to_string()),
            "登记不进去，后面那两条断言都在测别的东西"
        );
        assert_eq!(forget("timer_owner_a"), 2, "该交回摘掉了几格");
        assert!(
            !owners_for_test().contains(&"timer_owner_a".to_string()),
            "摘完名字还留在表上"
        );
        assert!(
            owners_for_test().contains(&"timer_owner_b".to_string()),
            "摘自己那一格不许连别人的一起带走"
        );
        assert_eq!(forget("timer_owner_b"), 1);
        assert_eq!(forget("never_armed_owner"), 0, "本来就没有格子，不该报错");
    }

    /// 桶的算术：同一分钟同一天 ⇒ 同一对桶；跨分钟、跨天各自只动对应那一项。
    ///
    /// `wall_buckets` 吃的是调用方给的墙钟，所以这些边界不用等日历走到那天。
    #[test]
    fn the_wall_buckets_move_by_minute_and_by_day_only() {
        use chrono::{TimeZone, Utc};
        let local = |y: i32, m: u32, d: u32, hh: u32, mm: u32| {
            chrono::Local
                .with_ymd_and_hms(y, m, d, hh, mm, 0)
                .single()
                .unwrap_or_else(|| panic!("{y}-{m}-{d} {hh}:{mm} 这个本地时刻取不出来"))
        };
        let (min_a, day_a) = wall_buckets(&local(2026, 11, 4, 23, 59));
        let (min_b, day_b) = wall_buckets(&local(2026, 11, 4, 23, 59));
        assert_eq!((min_a, day_a), (min_b, day_b), "同一分钟该给出同一对桶");
        let (min_c, day_c) = wall_buckets(&local(2026, 11, 5, 0, 0));
        assert_eq!(
            min_c,
            min_a + 1,
            "跨分钟桶 +1，实得 {min_c} vs {}",
            min_a + 1
        );
        assert_ne!(day_c, day_a, "跨过午夜日桶必须变");
        // 同一天的两次读数必须恰好差 1440 以内（日桶不变 ⇒ 分钟桶单调 +1）
        assert!(day_c > day_a);
        let _ = Utc::now();
    }
}
