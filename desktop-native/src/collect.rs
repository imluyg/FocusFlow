//! 采集接线：把 core 的 `Database` 与 `InputListener` 接进原生端。
//!
//! **默认关闭**（`[collector] enabled`，默认 `false`）：装完先当只读查看器，
//! 用户明确打开之后才开始采集。理由不是技术上的 —— `Database::init` 一起来就会
//! 全局钩键盘鼠标，那是要用户点头的事（见 docs/webview-removal-plan.md §10p）。
//!
//! 打开之后这里负责四件事：
//! 1. `Database::init(&cfg, paused)`：写线程、启动自检、在线备份、前台应用采样、
//!    设备侧信道（后两个都吃同一个 `paused`，暂停才能一起停）；
//! 2. `InputListener::start(db)`：rdev 主链路；
//! 3. 键回调只做一件事：给「次/分」记一笔（60 秒滑窗，见 [`cpm`]）；
//! 4. 暂停/停止：`PauseFlag` 是 `Arc<AtomicBool>`，与设备侧信道共用一份。
//!
//! **今日数字的口径**（`data.rs` 用）：`queries::get_today_count()` 只是**已落库**那部分，
//! 显示值要加 [`pending_today`]（`DbWriter::today_pending_count()`）；落库后 writer 自己会
//! 从 pending 里减掉，不会重复计。
//!
//! 为什么把状态放在静态里而不是 `App` 字段：`data.rs` 的快照线程也要读 pending，
//! 而它拿不到 `App`。这里的内容一次进程只初始化一次，静态是最小共享面。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use focusflow_core::config::FocusFlowConfig;
use focusflow_core::db::Database;
use focusflow_core::listener::{new_pause_flag, InputListener, PauseFlag};

/// 一份采集运行时。`enabled` 是**配置**，这里是"真的起来了"。
struct Runtime {
    db: Arc<Database>,
    listener: Arc<InputListener>,
    paused: PauseFlag,
    /// 这一场里采集还开着吗。`stop_listening` 置 false、`resume_listening` 置回 true。
    /// 与 [`is_started`] 不是一回事：那个槽不会被清空（写线程与应用采样是
    /// `Database::init` 起的一次性线程，core 没有对外停止接口），所以关掉之后
    /// `is_started()` 一直是 true —— 拿它判"要不要开"就成了单向开关。
    listening: AtomicBool,
}

static RUNTIME: OnceLock<Mutex<Option<Runtime>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<Runtime>> {
    RUNTIME.get_or_init(|| Mutex::new(None))
}

/// 「次/分」的时间戳（Unix 秒）。60 秒滑窗，见 [`cpm`]。
static KEY_TICKS: Mutex<VecDeque<i64>> = Mutex::new(VecDeque::new());

/// 滑窗长度（秒）。
const CPM_WINDOW_SECS: i64 = 60;

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 丢掉窗口之外的旧时间戳。抽出来是为了能单测（不依赖真时钟）。
fn prune(q: &mut VecDeque<i64>, now: i64) {
    while q
        .front()
        .is_some_and(|s| now.saturating_sub(*s) >= CPM_WINDOW_SECS)
    {
        q.pop_front();
    }
}

fn record_key_at(sec: i64) {
    if let Ok(mut q) = KEY_TICKS.lock() {
        q.push_back(sec);
        prune(&mut q, sec);
    }
}

/// 最近 60 秒的按键次数，即卡片上的「当前速度（次/分）」。
pub fn cpm() -> i64 {
    let now = now_secs();
    if let Ok(mut q) = KEY_TICKS.lock() {
        prune(&mut q, now);
        return q.len() as i64;
    }
    0
}

/// 配置里允不允许采集（`[collector] enabled`，默认 **true** —— 2026-10-27 用户要求默认开）。
pub fn enabled_in_config(cfg: &FocusFlowConfig) -> bool {
    cfg.get_bool("collector", "enabled", true)
}

/// 采集真的起来了没有（配置说开 ≠ 起来了：起失败时这里是 false）。
pub fn is_started() -> bool {
    slot().lock().map(|g| g.is_some()).unwrap_or(false)
}

/// 采集现在**在计**吗（起来过、且没被「关采集」停掉）。
pub fn is_listening() -> bool {
    slot()
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|r| r.listening.load(Ordering::SeqCst)))
        .unwrap_or(false)
}

/// 开关那一下该走哪条路（纯判据，便于把"关过一次就再也开不回来"钉住）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CollectorAction {
    /// 正在计 ⇒ 停
    Stop,
    /// 从没起来过 ⇒ 整条链起一次
    StartFresh,
    /// 起来过但被关掉 ⇒ 只解暂停（见 [`resume_listening`]：不能再起一个监听线程）
    Resume,
}

pub fn collector_toggle_action(listening: bool, started: bool) -> CollectorAction {
    match (listening, started) {
        (true, _) => CollectorAction::Stop,
        (false, false) => CollectorAction::StartFresh,
        (false, true) => CollectorAction::Resume,
    }
}

/// 把关掉的采集重新开起来：**只解暂停，绝不再起一个监听线程**。
///
/// 为什么不走 `listener.start()`：`stop()` 只是把 `alive` 置 false，而 rdev 那个线程还挂在
/// 钩子消息泵里（本仓记着"rdev 钩子没有 Unhook 的口子"这条账）—— 回调其实照旧在跑。
/// 这时候再 start 就是**第二个钩子**，而 `alive` 与 `PauseFlag` 都是共享的那一枚 ⇒
/// 每次按键被记两遍。计数的闸只有一枚（`PauseFlag`），所以回来走的也是这一枚。
pub fn resume_listening() -> Result<(), String> {
    let guard = slot().lock();
    let Ok(g) = guard else {
        return Err("采集状态锁被别处占住，这一拍没恢复成".to_string());
    };
    let Some(rt) = g.as_ref() else {
        return Err("采集从没起来过，这条不该走".to_string());
    };
    rt.listener.set_paused(false);
    rt.listening.store(true, Ordering::SeqCst);
    crate::tracing_note("采集已恢复：键鼠与设备计数重新开跑（没有重开监听线程，所以不会双记）");
    Ok(())
}

pub fn is_paused() -> bool {
    slot()
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|r| r.paused.load(Ordering::SeqCst)))
        .unwrap_or(false)
}

/// 未落库的今日增量（只在采集开着时有意义）。
pub fn pending_today() -> i64 {
    slot()
        .lock()
        .ok()
        .and_then(|g| {
            g.as_ref()
                .and_then(|r| r.db.writer().map(|w| w.today_pending_count()))
        })
        .unwrap_or(0)
}

/// 今日总数的**权威值**：写线程缓存（含未落库与正在落库的那批，且跨午夜自己归零）。
///
/// 为什么不再用「库值 + pending」：`flush_pending` 会把一批增量从 `agg` 取走、再在锁外
/// 写 SQLite；这中间 `agg` 与库两边都没有它，状态行就会瞬降一次（实测 `today=5 → 0 → 5`）。
/// 写线程的 `today_count` 是按事件累加的、落库前后都不动，正好没有这个空窗。
/// 采集没开时返回 `None`，调用方退回只读库。
pub fn today_total() -> Option<i64> {
    slot().lock().ok().and_then(|g| {
        g.as_ref().and_then(|r| {
            r.db.writer()
                .map(|w| w.today_count_for_display().min(i64::MAX as u64) as i64)
        })
    })
}

/// 外部往库里写了「今日」那些行之后（设置页那颗「导入旧版库」按钮），把写线程的今日基准
/// 按库里的真值重锚一次。返回**这次到底重锚了没有**。
///
/// 为什么非得有人调它：屏上「今日」与「今日活跃」读的是写线程的内存缓存
/// （[`today_total`] 走 `today_count_for_display`，它**只认缓存、不把库值加回来**），
/// 而那份缓存只在三处动 —— 启动时写线程建一次基准、每条键鼠事件往上加、
/// 写线程自己那一次跨午夜重锚。导入是往库里写行、不产生任何事件，所以导入进来的
/// 今日那些行**不会自己上屏**：症状是回执写着「导入完成：共 N 条」，屏上那个今日
/// 还是导入前的数，一直到下次重启才对上。
///
/// 采集没开着（只读模式，或起采集失败）时回 `false` 且什么都不做：那一档下
/// `data.rs` 本来就退回查库（`today_total()` 回 `None` 才走 `get_today_count`），
/// 屏上已经是真值，这里多此一举只会白付一次 flush 的等待。
///
/// ⚠ 它会走 `flush_confirmed(true)`：给写线程投一条消息并**最多等 3 秒**。
/// 所以只许在后台线程上调（`main.rs` 的 `data-op` 那一支就是为此存在的），
/// 不许从窗口过程里调 —— 那会把整扇主窗冻在那 3 秒里，而且屏上看不出为什么。
pub fn reanchor_today_after_external_write() -> bool {
    slot()
        .lock()
        .ok()
        .and_then(|g| {
            g.as_ref().and_then(|r| r.db.writer()).map(|w| {
                w.recompute_today_totals();
                true
            })
        })
        .unwrap_or(false)
}

/// `FOCUSFLOW_LOG_KEYS=1`：把每个按键写一行日志。
///
/// 给验收用：注入键（`keybd_event`/`SendInput`）与真键在钩子这一层长得一样，
/// 而界面上看到的是**落库之后的聚合数** —— "没计数"到底卡在钩子、过滤、还是落库，
/// 只有这一行能分清。默认关闭（它在监听线程上，每个键一次文件写）。
static LOG_KEYS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 启动时扫到旧版采集器（`FocusFlow.exe`）在跑。只作提示，不改行为。
static LEGACY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 旧版采集器是不是在跑（UI/托盘拿它提醒"双计"）。
pub fn legacy_running() -> bool {
    LEGACY.load(Ordering::Relaxed)
}

/// 起采集。`pause_now` = 起来之后先停在暂停态（设置页里"先开采集但不马上收"用不到，
/// 这里给测试与将来留口子）。
///
/// 返回 `Err` 时**不留半启动状态**：写线程起了但监听起不来，也把 listener 停掉、
/// 状态清回未启动，调用方可以重试。
pub fn start(cfg: &'static FocusFlowConfig) -> Result<(), String> {
    if is_started() {
        return Ok(());
    }
    LOG_KEYS.store(
        std::env::var("FOCUSFLOW_LOG_KEYS").is_ok(),
        Ordering::Relaxed,
    );
    // core 的日志与崩溃兜底：采集这条链上的错误（监听起不来、写线程重试、钟坏了）
    // 都只走 `tracing`，不初始化就是无声的。旧版 Tauri 那边也是同一套。
    focusflow_core::logger::init_logging();
    focusflow_core::logger::install_panic_hook();
    let paused = new_pause_flag();
    // 1) 写线程 + 自检 + 在线备份 + 应用采样 + 设备侧信道
    let db = Database::init(cfg, Arc::clone(&paused))
        .map_err(|e| format!("数据库初始化失败（采集没起来）：{e}"))?;
    let checks = db.take_startup_checks();
    for c in &checks {
        crate::tracing_note(&format!(
            "采集自检：{} {}（{}）",
            if c.ok { "OK" } else { "失败" },
            c.step,
            c.detail
        ));
    }
    let bad = checks.iter().filter(|c| !c.ok).count();
    if bad > 0 {
        crate::tracing_note(&format!("采集自检：{bad} 项异常，采集会缺这几块数据"));
    }
    // 2) rdev 主链路
    let listener = InputListener::new(cfg, Arc::clone(&paused));
    // 键回调只记账，不做任何 IO：它在监听线程上，每按一下都要跑。
    // （`FOCUSFLOW_LOG_KEYS=1` 时那行日志例外，测完记得关。）
    listener.add_key_callback(Arc::new(|key: &str| {
        if LOG_KEYS.load(Ordering::Relaxed) {
            crate::tracing_note(&format!("按键：{key}"));
        }
        record_key_at(now_secs());
    }));
    if let Err(e) = listener.start(Arc::clone(&db)) {
        // 监听起不来 = 一条都不记：把刚起的写线程也停掉，状态回未启动。
        db.shutdown(cfg);
        return Err(format!("键鼠监听起不来：{e}"));
    }
    if let Ok(mut g) = slot().lock() {
        *g = Some(Runtime {
            db,
            listener,
            paused,
            listening: AtomicBool::new(true),
        });
    }
    crate::tracing_note("采集已启动（键鼠监听 + 写线程 + 应用采样 + 设备侧信道）");
    if legacy_collector_running() {
        LEGACY.store(true, Ordering::Relaxed);
        crate::tracing_note("注意：检测到旧版 FocusFlow.exe 还在跑，两个采集器同时写会让数字翻倍");
    }
    Ok(())
}

/// 暂停/继续。暂停同时闸住 rdev 主链路与设备侧信道（共用 `PauseFlag`）。
pub fn set_paused(paused: bool) -> bool {
    let guard = slot().lock();
    let Ok(g) = guard else { return false };
    let Some(rt) = g.as_ref() else { return false };
    rt.listener.set_paused(paused);
    // 暂停时把「次/分」清空：否则恢复的那一刻会把暂停前的 60 秒算进来。
    if paused {
        if let Ok(mut q) = KEY_TICKS.lock() {
            q.clear();
        }
    }
    crate::tracing_note(if paused {
        "采集已暂停（键鼠与设备计数一起停）"
    } else {
        "采集已继续"
    });
    true
}

pub fn toggle_pause() -> bool {
    let next = !is_paused();
    if set_paused(next) {
        next
    } else {
        false
    }
}

/// 停掉采集（配置里关掉时用）。
///
/// 注意：这里只停**键鼠监听**（外加暂停位），写线程/应用采样/设备侧信道是
/// `Database::init` 起的一次性线程，没有对外停止接口（core 不在本轮改动范围内）。
/// 所以"关掉采集"的完整形态是：现在停止计数 + 下次启动不再初始化。
pub fn stop_listening() {
    let guard = slot().lock();
    let Ok(g) = guard else { return };
    let Some(rt) = g.as_ref() else { return };
    rt.listener.stop();
    rt.listener.set_paused(true);
    rt.listening.store(false, Ordering::SeqCst);
    if let Ok(mut q) = KEY_TICKS.lock() {
        q.clear();
    }
    crate::tracing_note("采集已停用：键鼠与设备计数已停（前台应用时长要重启后才停）");
}

/// 让 listener 重新读一遍 `[listener] *`。
///
/// 设置页那几行写的是配置文件，而 listener 的热路径读的是启动时拍下的原子快照
/// （`refresh_config()` 才会重拍）。主窗口每 2 秒的快照刷新顺带调一次，界面上
/// "改完立即生效"这句话才是真的；采集没开时这里是空操作。
pub fn refresh_config() {
    let guard = slot().lock();
    let Ok(g) = guard else { return };
    if let Some(rt) = g.as_ref() {
        rt.listener.refresh_config();
    }
}

/// 退出：停监听 + 落库 + 备份。`shutdown` 会 flush(true) → stop() → backup_on_exit。
pub fn shutdown(cfg: &'static FocusFlowConfig) {
    let guard = slot().lock();
    let Ok(g) = guard else { return };
    let Some(rt) = g.as_ref() else { return };
    rt.listener.stop();
    rt.db.shutdown(cfg);
}

/// 旧版 Tauri 版还在跑吗？（它和本端现在都叫 `FocusFlow.exe`）
///
/// 本端改名之后，按进程名扫会先扫到**自己**，所以必须跳过当前 PID；否则每次启动都报
/// "旧版在跑"，那条红字与双计提示就全成了噪声。
///
/// 只做**提示**不做拦截：同名进程也可能只是躺在任务栏里的旧版托盘。两个采集器各写一份
/// 会让数字翻倍，所以这件事必须出现在日志与设置页里，而不是悄悄发生。
fn legacy_collector_running() -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::GetCurrentProcessId;
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return false;
        };
        let me = GetCurrentProcessId();
        let mut pe: PROCESSENTRY32W = std::mem::zeroed();
        pe.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = false;
        if Process32FirstW(snap, &mut pe).is_ok() {
            loop {
                let raw = &pe.szExeFile;
                let end = raw.iter().position(|c| *c == 0).unwrap_or(raw.len());
                let name = String::from_utf16_lossy(&raw[..end]);
                if pe.th32ProcessID != me && name.eq_ignore_ascii_case("FocusFlow.exe") {
                    found = true;
                    break;
                }
                if Process32NextW(snap, &mut pe).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 三态开关：「起来过但被关掉」那一态必须走"再打开"，不许再说一遍"已停用"。
    ///
    /// 旧形状：分支判的是 `is_started()`，而 `stop_listening` 不清那个槽 ⇒ 关过一次之后
    /// 每次点击都走停用分支，这一场里再也开不回来（只能重启）。
    #[test]
    fn a_stopped_collector_toggles_back_on_instead_of_off_again() {
        assert_eq!(
            collector_toggle_action(false, false),
            CollectorAction::StartFresh
        );
        assert_eq!(collector_toggle_action(true, true), CollectorAction::Stop);
        assert_eq!(
            collector_toggle_action(false, true),
            CollectorAction::Resume,
            "关过一次的采集要能再打开，而不是原地再说一遍已停用"
        );
        // 不该出现的组合也得有答案，别一路 fallthrough 到停用
        assert_eq!(collector_toggle_action(true, false), CollectorAction::Stop);
    }

    /// 接线守卫：「再打开」那一条**绝不再起一个监听线程** —— rdev 的钩子没有 Unhook 的口子，
    /// 老线程还挂在消息泵里，再 start 就是第二个钩子，而 `alive` 与 `PauseFlag` 是共享的 ⇒
    /// 每次按键被记两遍。回来走的门只有暂停那一枚。
    #[test]
    fn resuming_never_starts_a_second_listener() {
        let src = include_str!("collect.rs");
        // 先摘注释行再比：本文件的文档里就写着「不走 `listener.start()`」这类反例，
        // 不摘干净的话守卫会被自己的说明文字命中（这仓里为这一条栽过两次）。
        let flat = |s: &str| -> String {
            s.lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("")
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect()
        };
        let at = src
            .find("fn resume_listening")
            .expect("resume_listening 不见了");
        // 段边界用「下一个函数」切，不拿字节窗口：`&src[a..a+1400]` 会切进中文字符里，
        // 直接 panic（本文件通篇是中文注释）。
        let seg = |from: usize| -> String {
            let rest = &src[from..];
            let cut = ["\nfn ", "\npub fn ", "\npub(crate) fn "]
                .iter()
                .filter_map(|p| rest.find(p))
                .min()
                .unwrap_or(rest.len());
            flat(&rest[..cut])
        };
        let body = seg(at);
        assert!(
            !body.contains("listener.start("),
            "恢复那条路又去起监听线程了：那是第二个钩子，会双记"
        );
        assert!(
            body.contains("rt.listener.set_paused(false)"),
            "恢复那条路没解暂停，等于什么都没做"
        );
        let stop_at = src
            .find("pub fn stop_listening")
            .expect("stop_listening 不见了");
        let stop = seg(stop_at);
        assert!(
            stop.contains("listening.store(false,"),
            "关掉采集没翻状态位，开关就还是单向的"
        );
    }

    #[test]
    fn the_sliding_window_drops_ticks_older_than_a_minute() {
        let mut q: VecDeque<i64> = VecDeque::new();
        for s in 0..60 {
            q.push_back(1000 + s);
        }
        // 同一秒里读：整窗都在
        prune(&mut q, 1059);
        assert_eq!(q.len(), 60);

        // 到 1060：1000 那一秒正好满 60 秒，丢掉
        prune(&mut q, 1060);
        assert_eq!(q.len(), 59);
        assert_eq!(q.front(), Some(&1001));

        // 一整分钟没人按键：清空
        prune(&mut q, 2000);
        assert!(q.is_empty());
    }

    #[test]
    fn the_window_is_exactly_sixty_seconds_wide() {
        let mut q: VecDeque<i64> = VecDeque::new();
        q.push_back(100);
        prune(&mut q, 159); // 差 59 秒：还算在窗口里
        assert_eq!(q.len(), 1);
        prune(&mut q, 160); // 差 60 秒：出窗
        assert!(q.is_empty());
    }

    #[test]
    fn clock_going_backwards_does_not_panic() {
        let mut q: VecDeque<i64> = VecDeque::new();
        q.push_back(5000);
        // 系统时间被拨回去：saturating_sub 之后是负数，不该 panic 也不该误删
        prune(&mut q, 10);
        assert_eq!(q.len(), 1);
    }

    /// 采集没起来时，那句"外部写完库之后重锚今日"必须报**什么都没做**。
    ///
    /// 为什么单独钉这一臂：`reanchor_today_after_external_write()` 内部走
    /// `flush_confirmed(true)` —— 给写线程投一条消息并**最多等 3 秒**。只读模式（采集没开、
    /// 或起采集失败）那一档下屏上本来就现查库、数是对的，白等一次不是"无害"：它把 3 秒
    /// 压在后台线程上，还会把"已重锚"这句假话说给日志听。
    ///
    /// ⚠ 前提是**量出来的**，不是假设：单元测试这个二进制里没有任何用例调过 `collect::start`
    /// （那两个调用点都在 `main.rs` 的生产路径上），所以静态槽恒为空。哪天有人加了一条
    /// 会起采集的用例，第一句断言会先叫住，而不是让这条腿悄悄退化成恒真。
    #[test]
    fn reanchoring_without_a_collector_reports_that_it_did_nothing() {
        assert!(
            !is_started(),
            "夹具前提：这条腿要的是「采集没起来」那一臂，槽里已经有运行时了"
        );
        assert!(
            !reanchor_today_after_external_write(),
            "采集没开着时要报 false —— 屏上本来就查库，不该白等一次 flush，更不该装作重锚过"
        );
        // 同一条链上的读数也必须是 `None`：那正是 `data.rs` 退回 `get_today_count` 的条件
        assert_eq!(today_total(), None, "采集没起来时「今日」没有权威值");
    }
}
