//! 键鼠监听模块。
//!
//! 镜像 Python 版 `listener.py`：
//! - rdev 全局键盘/鼠标监听（键盘 + 鼠标点击/滚轮统一计数）
//! - 修饰键/功能键过滤
//! - 长按自动重复过滤（`_pressed` 集合 + stale 时长）
//! - 滚轮连续滚动合并（从上次计数起 `scroll_burst_window` 秒内同方向只计 1 次，默认 0.25s）
//! - Ctrl+字母控制字符还原为物理键（v1.2.1 行为）
//! - 暂停/恢复，事件回调（番茄钟 / 护眼提醒用）
//!
//! 暂停位是 `PauseFlag`（`Arc<AtomicBool>`）：本模块的 rdev 主链路与设备侧信道
//! （`device_stats`，Raw Input）共用同一份，两侧都在落库前过同一道闸。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rdev::{listen, Button, Event, EventType, Key};

use crate::config::FocusFlowConfig;
use crate::db::Database;

/// 暂停位：整条采集链路的**一份**真相，`Arc` 共享给所有采集侧。
///
/// 为什么必须是共享的 `Arc<AtomicBool>` 而不是各处各存一个布尔（或各持一份拷贝）：
/// 暂停是"别再采集了"的全局指令，而设备侧信道（`device_stats`，Raw Input）的线程
/// 结构上只持有 `Arc<DbWriter>`、拿不到 `InputListener`。各存一份的结果就是本轮修的
/// 那个 bug：托盘/设置点暂停后继续敲键盘，「今日计数」冻住了而「设备排行」还在涨，
/// 看起来正是"暂停失效"。所以标志位在组合根（`desktop/src/state.rs`）建一次，
/// 同时喂给 `InputListener::new` 与 `Database::init`（后者转交设备线程）。
pub type PauseFlag = Arc<AtomicBool>;

/// 新建暂停位（初值 = 未暂停）。由组合根创建后分发给各采集侧。
pub fn new_pause_flag() -> PauseFlag {
    Arc::new(AtomicBool::new(false))
}

/// 暂停闸的唯一读点：主链路与设备侧信道都必须经由它判定，两边才不会走岔。
///
/// Relaxed 足够：这个标志只守卫"记不记"这一个布尔决定，不发布任何需要
/// happens-before 的数据。（暂停瞬间已越过闸门的并发事件最多多记一次，
/// 与原 `Mutex<bool>` 实现的窗口期同形。）
pub(crate) fn is_paused_now(flag: &PauseFlag) -> bool {
    flag.load(Ordering::Relaxed)
}

/// 修饰键集合（用于过滤）
fn is_modifier(key: &Key) -> bool {
    matches!(
        key,
        Key::ShiftLeft
            | Key::ShiftRight
            | Key::ControlLeft
            | Key::ControlRight
            | Key::Alt
            | Key::AltGr
            | Key::MetaLeft
            | Key::MetaRight
    )
}

/// 功能键集合（F1-F12）
fn is_function_key(key: &Key) -> bool {
    matches!(
        key,
        Key::F1
            | Key::F2
            | Key::F3
            | Key::F4
            | Key::F5
            | Key::F6
            | Key::F7
            | Key::F8
            | Key::F9
            | Key::F10
            | Key::F11
            | Key::F12
    )
}

/// 特殊键映射表（rdev Key -> 中文显示名），对应 Python 版 `_SPECIAL_KEY_MAP`。
fn key_display_name(key: &Key) -> &'static str {
    match key {
        Key::Space => "空格",
        Key::Return => "回车",
        Key::Backspace => "退格",
        Key::Tab => "Tab",
        Key::ShiftLeft => "左Shift",
        Key::ShiftRight => "右Shift",
        Key::ControlLeft => "左Ctrl",
        Key::ControlRight => "右Ctrl",
        Key::Alt => "左Alt",
        Key::AltGr => "AltGr",
        Key::MetaLeft => "左Win",
        Key::MetaRight => "右Win",
        Key::CapsLock => "CapsLock",
        Key::Escape => "Esc",
        Key::Delete => "Delete",
        Key::Home => "Home",
        Key::End => "End",
        Key::PageUp => "PageUp",
        Key::PageDown => "PageDown",
        Key::Insert => "Insert",
        Key::NumLock => "NumLock",
        Key::ScrollLock => "ScrollLock",
        Key::PrintScreen => "PrintScreen",
        Key::Pause => "Pause",
        Key::UpArrow => "↑",
        Key::DownArrow => "↓",
        Key::LeftArrow => "←",
        Key::RightArrow => "→",
        Key::F1 => "F1",
        Key::F2 => "F2",
        Key::F3 => "F3",
        Key::F4 => "F4",
        Key::F5 => "F5",
        Key::F6 => "F6",
        Key::F7 => "F7",
        Key::F8 => "F8",
        Key::F9 => "F9",
        Key::F10 => "F10",
        Key::F11 => "F11",
        Key::F12 => "F12",
        // Unknown 键：无静态名，调用方用 Debug 格式
        Key::Unknown(_) => "Unknown",
        _ => "Unknown",
    }
}

/// 处理热路径：按键名（静态键名零分配，未知键才分配 Owned）。
pub fn normalize_key(key: &Key) -> std::borrow::Cow<'static, str> {
    use std::borrow::Cow;
    if let Some(n) = letter_name(key) {
        return Cow::Borrowed(n);
    }
    if let Some(n) = digit_name(key) {
        return Cow::Borrowed(n);
    }
    if let Some(n) = symbol_name(key) {
        return Cow::Borrowed(n);
    }
    if let Some(n) = kp_name(key) {
        return Cow::Borrowed(n);
    }
    let name = key_display_name(key);
    if name == "Unknown" {
        Cow::Owned(format!("{:?}", key))
    } else {
        Cow::Borrowed(name)
    }
}

/// 字母键映射：rdev `KeyA`..`KeyZ` -> "A".."Z"（零分配）
fn letter_name(key: &Key) -> Option<&'static str> {
    use Key::*;
    Some(match key {
        KeyA => "A",
        KeyB => "B",
        KeyC => "C",
        KeyD => "D",
        KeyE => "E",
        KeyF => "F",
        KeyG => "G",
        KeyH => "H",
        KeyI => "I",
        KeyJ => "J",
        KeyK => "K",
        KeyL => "L",
        KeyM => "M",
        KeyN => "N",
        KeyO => "O",
        KeyP => "P",
        KeyQ => "Q",
        KeyR => "R",
        KeyS => "S",
        KeyT => "T",
        KeyU => "U",
        KeyV => "V",
        KeyW => "W",
        KeyX => "X",
        KeyY => "Y",
        KeyZ => "Z",
        _ => return None,
    })
}

/// 数字键映射：`Num0`..`Num9` -> "0".."9"（零分配）
fn digit_name(key: &Key) -> Option<&'static str> {
    use Key::*;
    Some(match key {
        Num0 => "0",
        Num1 => "1",
        Num2 => "2",
        Num3 => "3",
        Num4 => "4",
        Num5 => "5",
        Num6 => "6",
        Num7 => "7",
        Num8 => "8",
        Num9 => "9",
        _ => return None,
    })
}

/// 其他符号键映射（零分配）
fn symbol_name(key: &Key) -> Option<&'static str> {
    use Key::*;
    Some(match key {
        BackQuote => "`",
        Minus => "-",
        Equal => "=",
        LeftBracket => "[",
        RightBracket => "]",
        SemiColon => ";",
        Quote => "'",
        BackSlash => "\\",
        IntlBackslash => "\\",
        Comma => ",",
        Dot => ".",
        Slash => "/",
        _ => return None,
    })
}

/// 小键盘键映射（零分配）
fn kp_name(key: &Key) -> Option<&'static str> {
    use Key::*;
    Some(match key {
        Kp0 => "数字键盘0",
        Kp1 => "数字键盘1",
        Kp2 => "数字键盘2",
        Kp3 => "数字键盘3",
        Kp4 => "数字键盘4",
        Kp5 => "数字键盘5",
        Kp6 => "数字键盘6",
        Kp7 => "数字键盘7",
        Kp8 => "数字键盘8",
        Kp9 => "数字键盘9",
        KpReturn => "数字键盘回车",
        KpMinus => "数字键盘-",
        KpPlus => "数字键盘+",
        KpMultiply => "数字键盘*",
        KpDivide => "数字键盘/",
        KpDelete => "Delete",
        _ => return None,
    })
}

/// 鼠标按键名映射（对应 `_MOUSE_BUTTON_MAP`）。
pub fn normalize_mouse_button(button: &Button) -> String {
    match button {
        Button::Left => "鼠标左键".to_string(),
        Button::Right => "鼠标右键".to_string(),
        Button::Middle => "鼠标中键".to_string(),
        Button::Unknown(code) => format!("鼠标{}", code),
    }
}

/// 滚轮方向：dy>0 向上，dy<0 向下
fn scroll_direction(dy: i64) -> &'static str {
    if dy > 0 {
        "上"
    } else {
        "下"
    }
}

/// 输入事件回调类型（番茄钟 / 护眼提醒等）。
type KeyCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// 监听器用到的配置快照（原子量）。
///
/// 热路径（每个键鼠事件）原本要锁全局配置 4-6 次，与设置写入/落盘竞争；
/// 快照为原子量后热路径零锁，配置变更时调用 `refresh_config` 刷新。
#[derive(Default)]
struct ListenerCfg {
    mouse_enabled: std::sync::atomic::AtomicBool,
    ignore_key_repeat: std::sync::atomic::AtomicBool,
    ignore_modifiers: std::sync::atomic::AtomicBool,
    ignore_functions: std::sync::atomic::AtomicBool,
    /// f64 按 bits 存（key_repeat_stale_seconds）
    key_repeat_stale: std::sync::atomic::AtomicU64,
    /// f64 按 bits 存（scroll_burst_window）
    scroll_burst_window: std::sync::atomic::AtomicU64,
}

/// `[listener]` 里"秒"这一类浮点配置的收敛。
///
/// 两个值最终都喂给 `Duration::from_secs_f64`，而那个函数对**非有限值**与超大值是
/// panic —— `get_float` 走 `parse::<f64>()`，`inf` / `nan` / `1e300` 都解析得过来。
/// 换算点在**每次按键**的热路径上（`is_new_press` 的长按判定、滚轮突发窗口），
/// release 是 `panic = "abort"` 且没有控制台：症状就是"正打字呢程序没了"，
/// 开机自启时变成"启动即崩"。
///
/// 夹在配置入口而不是各个 getter：新增一个读点也不会漏，且 getter 里的
/// `+ 1.0`（测试用）不会再把它推回溢出区。上限一天 —— 这两个都是"间隔"语义，
/// 长过一天等于永不触发，与写错同义；非有限值退回调用方给的默认值。
fn cfg_seconds(v: f64, fallback: f64, floor: f64) -> f64 {
    if v.is_finite() {
        v.clamp(floor, 86_400.0)
    } else {
        fallback
    }
}

impl ListenerCfg {
    fn reload(&self, config: &FocusFlowConfig) {
        use std::sync::atomic::Ordering::Relaxed;
        self.mouse_enabled
            .store(config.get_bool("listener", "mouse_enabled", true), Relaxed);
        self.ignore_key_repeat.store(
            config.get_bool("listener", "ignore_key_repeat", true),
            Relaxed,
        );
        self.ignore_modifiers.store(
            config.get_bool("listener", "ignore_modifier_keys", false),
            Relaxed,
        );
        self.ignore_functions.store(
            config.get_bool("listener", "ignore_function_keys", false),
            Relaxed,
        );
        self.key_repeat_stale.store(
            cfg_seconds(
                config.get_float("listener", "key_repeat_stale_seconds", 15.0),
                15.0,
                0.1,
            )
            .to_bits(),
            Relaxed,
        );
        self.scroll_burst_window.store(
            cfg_seconds(
                config.get_float("listener", "scroll_burst_window", 0.25),
                0.25,
                0.01,
            )
            .to_bits(),
            Relaxed,
        );
    }

    fn stale_secs(&self) -> f64 {
        f64::from_bits(
            self.key_repeat_stale
                .load(std::sync::atomic::Ordering::Relaxed),
        )
        .max(0.1)
    }

    fn burst_window(&self) -> f64 {
        f64::from_bits(
            self.scroll_burst_window
                .load(std::sync::atomic::Ordering::Relaxed),
        )
        .max(0.01)
    }
}

/// 输入监听器。
pub struct InputListener {
    config: &'static FocusFlowConfig,
    /// 热路径配置快照（见 ListenerCfg）
    cfg: ListenerCfg,
    /// 按下状态的按键集合：{按键名: 按下时刻}
    pressed: Mutex<HashMap<String, Instant>>,
    /// 滚轮合并状态：最近一次滚轮时刻（`None` = 尚无/已被重置，必然算新一轮）
    scroll: Mutex<(Option<Instant>, &'static str)>,
    /// 暂停状态（与设备侧信道共享同一份，见 [`PauseFlag`]）
    paused: PauseFlag,
    /// 事件回调（番茄钟 / 护眼提醒）
    key_callbacks: Mutex<Vec<KeyCallback>>,
    /// 监听线程是否存活
    alive: Arc<Mutex<bool>>,
}

impl InputListener {
    /// 建监听器。`paused` 是共享暂停位：调用方（组合根）必须把**同一个** flag
    /// 也交给 `Database::init`，否则设备侧信道看不到暂停状态。
    pub fn new(config: &'static FocusFlowConfig, paused: PauseFlag) -> Arc<Self> {
        let listener = Self {
            config,
            cfg: ListenerCfg::default(),
            pressed: Mutex::new(HashMap::new()),
            // None 表示「还没有滚过」，等价于上一次的合并窗口早已过期。
            // 不能写成 `Instant::now() - Duration::from_secs(10)`：开机不足 10 秒时
            // Instant 减法下溢 panic，而 release 的 panic=abort 会让开机自启变成启动即崩
            // （app_stats.rs / queries.rs 里对同一个坑留过告诫）。
            scroll: Mutex::new((None, "上")),
            paused,
            key_callbacks: Mutex::new(Vec::new()),
            alive: Arc::new(Mutex::new(false)),
        };
        listener.cfg.reload(config);
        Arc::new(listener)
    }

    /// 配置变更后刷新热路径快照（listener section 的键）。
    pub fn refresh_config(&self) {
        self.cfg.reload(self.config);
    }

    /// 注册输入回调（每个有效键鼠事件触发一次）。
    pub fn add_key_callback(&self, cb: Arc<dyn Fn(&str) + Send + Sync>) {
        self.key_callbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(cb);
    }

    pub fn is_paused(&self) -> bool {
        is_paused_now(&self.paused)
    }

    pub fn set_paused(&self, paused: bool) {
        // swap 而不是 load + store：一次原子操作就完成"确实变了才做副作用"的判断，
        // 托盘与设置页同时切换时不会各触发一次清按下的副作用（等价于原来整段持锁）。
        if self.paused.swap(paused, Ordering::SeqCst) == paused {
            return;
        }
        if paused {
            self.pressed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            let mut s = self.scroll.lock().unwrap_or_else(|e| e.into_inner());
            *s = (None, "上");
        }
        tracing::info!("监听已 {}", if paused { "暂停" } else { "恢复" });
    }

    pub fn toggle_pause(&self) -> bool {
        let new_state = !self.is_paused();
        self.set_paused(new_state);
        new_state
    }

    /// 长按自动重复过滤：按键已在按下集合中（且未超 stale 时长）视为重复，不计数。
    fn is_new_press(&self, key_name: &str) -> bool {
        let stale = self.cfg.stale_secs();
        let now = Instant::now();
        let mut pressed = self.pressed.lock().unwrap_or_else(|e| e.into_inner());
        // 安全阀：集合过大时清理超时残留
        if pressed.len() > 256 {
            pressed.retain(|_, t| now.duration_since(*t) <= Duration::from_secs_f64(stale));
        }
        match pressed.get(key_name) {
            None => {
                pressed.insert(key_name.to_string(), now);
                true
            }
            Some(last) if now.duration_since(*last) > Duration::from_secs_f64(stale) => {
                pressed.insert(key_name.to_string(), now);
                true
            }
            Some(_) => false,
        }
    }

    /// 滚轮连续滚动合并：从**上一次计数**起整个窗口内，同方向只计 1 次。
    ///
    /// 时间戳只在真的计数时刷新（`_at` 版是为了能把"每次间隔都短于窗口、总时长却远超
    /// 窗口"这个形状写成确定性用例）。原来是无条件 `*scroll = (Some(now), direction)`，
    /// 于是窗口变成跟随式的：截止点跟着每一格滚轮往后跑，连续滚动一整段只计 1 次，
    /// 必须**停手超过一个窗口**才可能有第二次 —— 与"窗口内只计 1 次"的措辞并不是一回事。
    fn is_new_scroll_burst_at(&self, direction: &'static str, now: Instant) -> bool {
        let window = Duration::from_secs_f64(self.cfg.burst_window());
        let mut scroll = self.scroll.lock().unwrap_or_else(|e| e.into_inner());
        let is_new = match scroll.0 {
            // 尚无记录（首次，或暂停后刚重置）：一定是新一轮
            None => true,
            Some(t) => now.duration_since(t) > window || scroll.1 != direction,
        };
        if is_new {
            *scroll = (Some(now), direction);
        }
        is_new
    }

    fn is_new_scroll_burst(&self, direction: &'static str) -> bool {
        self.is_new_scroll_burst_at(direction, Instant::now())
    }

    /// 处理单个键鼠事件：记录到数据库 + 触发回调。
    fn record_event(&self, db: &Database, key_name: &str) {
        // 暂停闸（与 device_stats 的设备侧信道读的是同一份 PauseFlag）：
        // 两侧都在"要落库的那一刻"判定，所以暂停后今日计数与设备排行一起停住。
        if self.is_paused() {
            return;
        }
        let ts = match now_ts_secs() {
            Some(ts) => ts,
            None => {
                warn_bad_clock_once();
                return;
            }
        };
        tracing::debug!("record_event: {key_name} ts={ts}");
        db.record_key(key_name, ts);
        // 记录 CPM（当前速度统计）
        crate::stats::cpm(self.config).record();
        // 通知回调（番茄钟 / 护眼提醒）：先 clone 出回调列表再放锁执行，
        // 避免持锁调用外部代码（回调可能耗时或重入）
        let callbacks: Vec<_> = self
            .key_callbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for cb in callbacks.iter() {
            cb(key_name);
        }
    }

    /// 处理一个事件（供监听线程调用）。
    fn process_event(&self, db: &Database, event: &Event) {
        use std::sync::atomic::Ordering::Relaxed;
        // 配置读取走原子快照：热路径零锁（每个事件原本要锁全局配置 4-6 次）
        let mouse_enabled = self.cfg.mouse_enabled.load(Relaxed);
        let ignore_key_repeat = self.cfg.ignore_key_repeat.load(Relaxed);
        let ignore_modifiers = self.cfg.ignore_modifiers.load(Relaxed);
        let ignore_functions = self.cfg.ignore_functions.load(Relaxed);

        match &event.event_type {
            EventType::KeyPress(key) => {
                // 过滤修饰键/功能键
                if ignore_modifiers && is_modifier(key) {
                    return;
                }
                if ignore_functions && is_function_key(key) {
                    return;
                }
                // 未知键不计数：rdev 映射表之外的虚拟键码（多媒体音量键、
                // 输入法合成事件、厂商 OEM 键等），无统计意义且会以
                // Unknown(N) 形式污染排行与数据库
                if matches!(key, Key::Unknown(_)) {
                    return;
                }
                let name = normalize_key(key);
                // 长按自动重复过滤
                if ignore_key_repeat && !self.is_new_press(&name) {
                    return;
                }
                self.record_event(db, &name);
            }
            EventType::KeyRelease(key) => {
                // 未知键从未进入 pressed 集合，直接跳过（同时避免 normalize_key 的格式化分配）
                if matches!(key, Key::Unknown(_)) {
                    return;
                }
                let name = normalize_key(key);
                self.pressed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(name.as_ref());
            }
            EventType::ButtonPress(button) => {
                if !mouse_enabled {
                    return;
                }
                // 鼠标侧键（rdev 上报为 Unknown(1/2)）与键盘未知键同样不计数
                if matches!(button, Button::Unknown(_)) {
                    return;
                }
                let name = normalize_mouse_button(button);
                self.record_event(db, &name);
            }
            EventType::ButtonRelease(_) => {}
            EventType::Wheel { delta_y, .. } => {
                if !mouse_enabled {
                    return;
                }
                if *delta_y == 0 {
                    // 亚格滚动（高分辨率/平滑滚轮，rdev 整除后只剩 0）：幅度信息
                    // 在 rdev 的 `delta / WHEEL_DELTA` 里已被销毁，事件只剩"动过"
                    // 而不知道动了多少 —— 记 1 会把一次平滑滚动刷成几百次，只能丢。
                    // 真要按格计亚格滚动，得绕开 rdev 直接读 Raw Input（设备侧
                    // 信道那套），那是另一个口径的工程。
                    return;
                }
                let direction = scroll_direction(*delta_y);
                // 连续滚动合并（窗口限速锚在上一次计数，见 is_new_scroll_burst_at）
                if !self.is_new_scroll_burst(direction) {
                    return;
                }
                // rdev 交付的 delta_y 已经是**格数**（原始 delta 被它整除过）。
                // 一条消息携带 N 格 = 物理上滚了 N 格（Windows/驱动的输入合并），
                // 按 N 计而不是 1；合并窗口照旧只判这一次 —— 窗口内被合并掉的
                // 事件属于同一段甩动，与「每满一个窗口最多计一批」的口径一致。
                // 上限只为挡异常驱动刷爆计数（消息里物理上限约 ±273 格）。
                const MAX_WHEEL_NOTCHES_PER_EVENT: i64 = 16;
                let notches = (*delta_y).abs().clamp(1, MAX_WHEEL_NOTCHES_PER_EVENT);
                let name = format!("滚轮{}滑", direction);
                for _ in 0..notches {
                    self.record_event(db, &name);
                }
            }
            EventType::MouseMove { .. } => {}
        }
    }

    /// 启动监听（在后台线程运行）。
    ///
    /// spawn 失败往上传（`Err` → `AppState::init` → 启动失败）：这是键鼠统计的
    /// 主链路，没了这个线程整个程序就是"看着在跑、什么都不记"。原来失败被
    /// `.map_err(日志).ok()` 吞掉，还带着两处连带的小错：`alive` 在 spawn
    /// **之前**就置 true（失败后运行状态看着像在跑）、失败也照打
    /// 「键鼠监听已启动」。一起修掉。
    pub fn start(self: &Arc<Self>, db: Arc<Database>) -> anyhow::Result<()> {
        let mut alive = self.alive.lock().unwrap_or_else(|e| e.into_inner());
        if *alive {
            return Ok(());
        }
        let this = Arc::clone(self);
        // 持锁跨 spawn：spawn 只是建线程不阻塞，锁到置位为止，
        // 防两个并发 start 都过了检查、双起线程。
        // 新线程首件事也是锁 alive —— 它会短暂等在这里，等我们置位后放行。
        thread::Builder::new()
            .name("input-listener".into())
            .spawn(move || loop {
                if !*this.alive.lock().unwrap_or_else(|e| e.into_inner()) {
                    break;
                }
                let this2 = Arc::clone(&this);
                let db2 = Arc::clone(&db);
                let result = listen(move |event: Event| {
                    this2.process_event(&db2, &event);
                });
                if !*this.alive.lock().unwrap_or_else(|e| e.into_inner()) {
                    break;
                }
                match result {
                    Ok(()) => tracing::warn!("键鼠监听意外结束，5 秒后自动重启"),
                    Err(e) => tracing::error!("键鼠监听失败，5 秒后自动重启: {e:?}"),
                }
                thread::sleep(Duration::from_secs(5));
            })
            .map_err(|e| {
                tracing::error!("启动输入监听线程失败: {e}");
                anyhow::anyhow!("启动输入监听线程失败: {e}")
            })?;
        *alive = true;
        drop(alive);
        tracing::info!(
            "键鼠监听已启动 (ignore_modifiers={}, ignore_functions={}, mouse_enabled={})",
            self.config
                .get_bool("listener", "ignore_modifier_keys", false),
            self.config
                .get_bool("listener", "ignore_function_keys", false),
            self.config.get_bool("listener", "mouse_enabled", true),
        );
        Ok(())
    }

    /// 停止监听。
    pub fn stop(&self) {
        *self.alive.lock().unwrap_or_else(|e| e.into_inner()) = false;
        tracing::info!("键鼠监听已停止");
    }
}

/// `SystemTime` → Unix 秒；时钟落在历元之前时返回 `None`。
///
/// 两处采集点原来都写 `duration_since(UNIX_EPOCH).unwrap_or(0)`：CMOS 电池没了、
/// 或系统时间被设到 1970 之前时，补出来的 0 会被分桶成"1970-01-01"，于是按键永久
/// 落进 `focusflow_1970.db` —— 而「总计」不按年截断，这些计数会一直留在总数里，
/// 那个空壳还要每年被备份/VACUUM/聚合各挨一遍。
/// 一条没有时间的事件本来就没法归属到任何一天，宁可不记。
pub(crate) fn unix_ts_secs(at: std::time::SystemTime) -> Option<i64> {
    match at.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => Some(d.as_secs() as i64),
        Err(_) => None,
    }
}

/// 此刻的 Unix 秒；时钟异常时 `None`（见 [`unix_ts_secs`]）。
pub(crate) fn now_ts_secs() -> Option<i64> {
    unix_ts_secs(std::time::SystemTime::now())
}

/// 时钟异常只说一次：这是每个按键都会走的路径，逐条 warn 会把日志刷爆。
fn warn_bad_clock_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        tracing::error!(
            "系统时间落在 1970 之前，本次运行的键鼠事件不再记录（否则会把计数写进一个\
             永不合并的 1970 年库）；把系统时间修正后重启 FocusFlow 即可恢复"
        );
    });
}

#[cfg(test)]
mod tests {
    /// 时钟换算的三条腿：正常值、历元整点、历元之前。
    ///
    /// 第三条是重点：老写法 `unwrap_or(0)` 会把"时间拿不到"变成一个**看起来合法**的
    /// 1970-01-01，按键因此永久落进 `focusflow_1970.db` 并留在「总计」里。
    #[test]
    fn pre_epoch_clock_is_not_turned_into_year_1970() {
        use std::time::{Duration, UNIX_EPOCH};
        assert_eq!(
            unix_ts_secs(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            Some(1_700_000_000)
        );
        assert_eq!(unix_ts_secs(UNIX_EPOCH), Some(0));
        assert_eq!(
            unix_ts_secs(UNIX_EPOCH - Duration::from_secs(1)),
            None,
            "历元之前的时钟必须报不出来"
        );
        // 生产路径：这台机器时钟正常，必须拿得到（拿不到就等于全程不记录）
        assert!(super::now_ts_secs().is_some());
    }
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::SystemTime;

    /// 测试用的 `&'static` 配置。
    ///
    /// `InputListener` 按 `&'static FocusFlowConfig` 持有配置（生产里它就是全局单例），
    /// 所以这份只能泄漏到进程结束 —— RAII 的 `TestAppDir` 用不上。也正因如此，草稿
    /// 不能放 `%TEMP%`：那里的名字带 pid，每跑一次测试就永久多留一个目录（本机攒到
    /// 179 个）。放到 `target/` 下的**固定路径**才是对的：跨运行复用同一个目录，
    /// 一次 `cargo clean` 全部带走，`%TEMP%` 不再增长。
    fn test_config() -> &'static FocusFlowConfig {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/ff_listener_cfg");
        std::fs::create_dir_all(&dir).ok();
        Box::leak(Box::new(
            FocusFlowConfig::load(dir.join("config.ini")).unwrap(),
        ))
    }

    /// `[listener] key_repeat_stale_seconds = inf` 这一句手写配置，改之前会让
    /// **下一次按键**在 `Duration::from_secs_f64` 上 panic（`get_float` 用的
    /// `parse::<f64>()` 认 `inf`/`nan`/`1e300`，而 `.max(0.1)` 只挡下限），
    /// release 是 panic=abort 且无控制台 —— 症状是"正打字呢程序没了"。
    /// 现在配置入口就该挡掉，并且挡完还得是一个能换算的有限值。
    #[test]
    fn absurd_second_values_never_reach_the_duration() {
        for raw in ["inf", "-inf", "nan", "1e300", "-5", "0"] {
            // 每个取值一个文件：cargo 并行跑用例，共用一份会互相盖
            let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join(format!("../../target/ff_listener_clamp_{raw}.ini"));
            std::fs::write(
                &path,
                format!(
                    "[listener]\nkey_repeat_stale_seconds = {raw}\nscroll_burst_window = {raw}\n"
                ),
            )
            .unwrap();
            let cfg = FocusFlowConfig::load(&path).unwrap();
            let c = ListenerCfg::default();
            c.reload(&cfg);
            let (stale, burst) = (c.stale_secs(), c.burst_window());
            assert!(
                (0.1..=86_400.0).contains(&stale),
                "`{raw}` 的 stale 该被夹进 0.1..=86400，实际 {stale}"
            );
            assert!(
                (0.01..=86_400.0).contains(&burst),
                "`{raw}` 的 burst 窗口该被夹进 0.01..=86400，实际 {burst}"
            );
            // 生产路径上真正会炸的那一步，连同测试里用的 `+ 1.0` 一起走一遍
            let _ = Duration::from_secs_f64(stale + 1.0);
            let _ = Duration::from_secs_f64(burst);
        }
    }

    #[test]
    fn unknown_key_name_format_is_stable() {
        // 未知键在 process_event 中被过滤、不计数；
        // KeyRelease 清理 pressed 依赖 KeyPress/KeyRelease 生成相同键名，
        // 此测试保证 Unknown(N) 格式契约不因实现调整而漂移
        assert_eq!(normalize_key(&Key::Unknown(173)), "Unknown(173)");
        assert_eq!(normalize_key(&Key::Unknown(0)), "Unknown(0)");
    }

    /// 长按去重：窗口内重复按下不计数，stale 超时后重新计数。
    #[test]
    fn is_new_press_filters_repeat_until_stale() {
        let l = InputListener::new(test_config(), new_pause_flag());
        assert!(l.is_new_press("A"));
        assert!(!l.is_new_press("A"), "窗口内第二次按下应视为长按重复");

        // 手工把按下时刻拨到 stale 窗口之外（默认 15 秒，避免真实等待）。
        // checked_sub：裸减法低于时钟原点会 panic。
        let stale_secs = l.cfg.stale_secs();
        let aged = Instant::now()
            .checked_sub(Duration::from_secs_f64(stale_secs + 1.0))
            .unwrap_or_else(Instant::now);
        l.pressed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("A".to_string(), aged);
        assert!(l.is_new_press("A"), "stale 超时后应重新计数");
    }

    /// 安全阀：pressed 集合超过 256 时清理 stale 残留。
    #[test]
    fn is_new_press_cleans_up_stale_entries_when_large() {
        let l = InputListener::new(test_config(), new_pause_flag());
        {
            let mut pressed = l.pressed.lock().unwrap_or_else(|e| e.into_inner());
            for i in 0..250 {
                pressed.insert(format!("Fresh{i}"), Instant::now());
            }
            let stale = Instant::now()
                .checked_sub(Duration::from_secs_f64(l.cfg.stale_secs() + 1.0))
                .unwrap_or_else(Instant::now);
            for i in 0..60 {
                pressed.insert(format!("Aged{i}"), stale);
            }
        }
        // 集合 310 > 256：触发 retain 清理，stale 键被移除后可重新计数
        assert!(l.is_new_press("Aged0"));
        let pressed = l.pressed.lock().unwrap_or_else(|e| e.into_inner());
        assert!(!pressed.contains_key("Aged1"), "stale 残留应被清理");
        assert!(pressed.contains_key("Fresh0"), "未超时按键应保留");
    }

    /// 滚轮合并：窗口内同方向只计 1 次；方向切换或窗口过期重新计数。
    #[test]
    fn is_new_scroll_burst_merges_same_direction_only() {
        let l = InputListener::new(test_config(), new_pause_flag());
        assert!(l.is_new_scroll_burst("上"));
        assert!(!l.is_new_scroll_burst("上"), "窗口内同方向应合并");
        assert!(l.is_new_scroll_burst("下"), "方向切换应立即计数");
        assert!(!l.is_new_scroll_burst("下"));

        // 造一个确实已超出合并窗口的时刻。用 checked_sub 而不是裸减法：
        // Instant 减法低于时钟原点（开机时刻）会 panic，正是生产路径这轮修掉的坑。
        let expired = Instant::now()
            .checked_sub(Duration::from_secs_f64(l.cfg.burst_window() + 1.0))
            .unwrap_or_else(Instant::now);
        *l.scroll.lock().unwrap_or_else(|e| e.into_inner()) = (Some(expired), "上");
        assert!(l.is_new_scroll_burst("上"), "窗口过期后应重新计数");
    }

    /// 真实手感那一刀：**每格间隔都短于窗口、但一直在滚**，总时长远超窗口时该计多次。
    ///
    /// 上面那条用例证明不了它 —— 它只测"背靠背"和"手工造一个过期时刻"。原实现把
    /// 时间戳无条件推到"现在"，于是截止点跟着每一格往后跑，10 格滚了 2.7 个窗口
    /// 仍然只计 1 次（要停手满一个窗口才可能有第二次）。锚在**上一次计数**之后：
    /// 同一轮里前 4 格合并，满了窗口才再计一次。
    #[test]
    fn scroll_burst_merges_from_the_last_count_not_the_last_event() {
        let l = InputListener::new(test_config(), new_pause_flag());
        let window = Duration::from_secs_f64(l.cfg.burst_window());
        let t0 = Instant::now();

        // 每 0.3 个窗口滚一格，共 10 格（跨 2.7 个窗口），全程没有停手
        let mut counted = 0;
        for i in 0..10u32 {
            if l.is_new_scroll_burst_at("上", t0 + window.mul_f32(0.3 * i as f32)) {
                counted += 1;
            }
        }
        assert_eq!(
            counted, 3,
            "锚在上次计数：0、>窗口、再>窗口 各计一次，其余合并"
        );

        // 反向对照：同一轮里密密麻麻的短间隔滚动（总时长不出一个窗口）仍然只计 1 次，
        // 合并本身没被削弱 —— 一次甩动不该变成好几次。
        let l2 = InputListener::new(test_config(), new_pause_flag());
        let mut rapid = 0;
        for i in 0..8u32 {
            if l2.is_new_scroll_burst_at("上", t0 + window.mul_f32(0.1 * i as f32)) {
                rapid += 1;
            }
        }
        assert_eq!(rapid, 1, "0.7 个窗口内的 8 格还是一次动作");
    }

    /// process_event 过滤分支：修饰键/功能键/未知键/鼠标关闭均不触发回调；
    /// ignore_key_repeat 生效时长按重复也不计数。
    #[test]
    fn process_event_filters_keys_and_respects_mouse_toggle() {
        let _lock = crate::paths::test_app_dir_lock();
        let dir = std::env::temp_dir().join(format!("ff_listener_db_{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        crate::paths::set_app_dir(&dir);
        let db = crate::db::Database::init_readonly();

        let l = InputListener::new(test_config(), new_pause_flag());
        l.cfg.ignore_modifiers.store(true, Ordering::Relaxed);
        l.cfg.ignore_functions.store(true, Ordering::Relaxed);
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_cb = Arc::clone(&hits);
        l.add_key_callback(Arc::new(move |_| {
            hits_cb.fetch_add(1, Ordering::Relaxed);
        }));

        let ev = |t: rdev::EventType| rdev::Event {
            event_type: t,
            name: None,
            time: SystemTime::now(),
        };
        l.process_event(&db, &ev(rdev::EventType::KeyPress(Key::ShiftLeft)));
        l.process_event(&db, &ev(rdev::EventType::KeyPress(Key::F5)));
        l.process_event(&db, &ev(rdev::EventType::KeyPress(Key::Unknown(173))));
        assert_eq!(
            hits.load(Ordering::Relaxed),
            0,
            "修饰键/功能键/未知键不应触发回调"
        );

        l.process_event(&db, &ev(rdev::EventType::KeyPress(Key::KeyA)));
        assert_eq!(hits.load(Ordering::Relaxed), 1);
        // ignore_key_repeat 默认开启：窗口内重复按下不计
        l.process_event(&db, &ev(rdev::EventType::KeyPress(Key::KeyA)));
        assert_eq!(hits.load(Ordering::Relaxed), 1, "长按重复不应计数");

        // 鼠标统计关闭：滚轮不触发；重新开启后同方向窗口内合并
        l.cfg.mouse_enabled.store(false, Ordering::Relaxed);
        l.process_event(
            &db,
            &ev(rdev::EventType::Wheel {
                delta_x: 0,
                delta_y: 120,
            }),
        );
        assert_eq!(
            hits.load(Ordering::Relaxed),
            1,
            "mouse_enabled=false 时滚轮不应计数"
        );

        l.cfg.mouse_enabled.store(true, Ordering::Relaxed);
        // delta_y 的语义是 rdev 整除后的**格数**（不是原始 delta）：1 = 一格
        l.process_event(
            &db,
            &ev(rdev::EventType::Wheel {
                delta_x: 0,
                delta_y: 1,
            }),
        );
        assert_eq!(hits.load(Ordering::Relaxed), 2);
        l.process_event(
            &db,
            &ev(rdev::EventType::Wheel {
                delta_x: 0,
                delta_y: 1,
            }),
        );
        assert_eq!(hits.load(Ordering::Relaxed), 2, "窗口内同方向滚轮应合并");
        l.process_event(
            &db,
            &ev(rdev::EventType::Wheel {
                delta_x: 0,
                delta_y: -1,
            }),
        );
        assert_eq!(hits.load(Ordering::Relaxed), 3, "方向切换应计数");

        crate::paths::set_app_dir(crate::paths::test_scratch_app_dir());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 多格合并上报（§九·2）：Windows/驱动会把快速滚动**合并进一条消息**
    /// （delta_y = 物理格数，rdev 交付的已是整除后的格数）。物理 3 格要计 3 次，
    /// 不能只计 1；窗口限速（§九）与亚格丢弃的口径不能被这刀破坏。
    #[test]
    fn wheel_event_carrying_multiple_notches_counts_each() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("listener_wheel_multi");
        let db = crate::db::Database::init_readonly();
        let l = InputListener::new(test_config(), new_pause_flag());
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_cb = Arc::clone(&hits);
        l.add_key_callback(Arc::new(move |_| {
            hits_cb.fetch_add(1, Ordering::Relaxed);
        }));
        let ev = |dy: i64| rdev::Event {
            event_type: rdev::EventType::Wheel {
                delta_x: 0,
                delta_y: dy,
            },
            name: None,
            time: SystemTime::now(),
        };

        // 一条消息 3 格：物理 3 格计 3 次
        l.process_event(&db, &ev(3));
        assert_eq!(hits.load(Ordering::Relaxed), 3, "多格消息应按格数计数");

        // 同方向 0.25s 窗口内的下一个事件照旧被合并（限速没被这刀削弱）
        l.process_event(&db, &ev(2));
        assert_eq!(
            hits.load(Ordering::Relaxed),
            3,
            "窗口内同方向事件应照旧合并"
        );

        // 反方向立刻计数（2 格计 2 次）
        l.process_event(&db, &ev(-2));
        assert_eq!(hits.load(Ordering::Relaxed), 5, "方向切换照旧按格计数");

        // 亚格事件（rdev 整除后为 0）：信息已销毁，丢弃且不改变窗口锚
        l.process_event(&db, &ev(0));
        assert_eq!(hits.load(Ordering::Relaxed), 5, "亚格事件不应计数");
    }

    /// 暂停位必须是**共享**的那一份：`set_paused`/`toggle_pause` 翻动的就是设备侧信道
    /// 读的同一个 `Arc<AtomicBool>`，两侧同停同起。
    ///
    /// 反着的形态就是那条回归：`InputListener` 自持 `Mutex<bool>` 时，设备线程只拿得到
    /// `Arc<DbWriter>`，结构上看不到暂停 → 托盘暂停后继续敲键盘，「今日计数」冻住而
    /// 「设备排行」还在涨，看着像"暂停失效"。这里两侧都验（主链路走回调，
    /// 设备侧走落库点 `record_device_event` —— rdev/Raw Input 回调在本机注入不了）。
    #[test]
    fn pause_flag_is_shared_by_listener_and_device_channel() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("listener_pause_shared");
        let db = crate::db::Database::init_readonly();
        let writer = crate::db::DbWriter::start(Duration::from_secs(3600))
            .expect("测试里 DB 写线程必须能启动");

        let paused = new_pause_flag();
        let l = InputListener::new(test_config(), Arc::clone(&paused));
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_cb = Arc::clone(&hits);
        l.add_key_callback(Arc::new(move |_| {
            hits_cb.fetch_add(1, Ordering::Relaxed);
        }));
        let ev = |t: rdev::EventType| rdev::Event {
            event_type: t,
            name: None,
            time: SystemTime::now(),
        };
        let device_side = |w: &crate::db::DbWriter| {
            crate::device_stats::record_device_event(
                w,
                &paused,
                r"HID#VID_046D&PID_C52B#7&1",
                "HID 鼠标 · 046D/C52B",
                "mouse",
                Some("鼠标左键"),
                now_ts_secs().unwrap_or(0),
            )
        };

        // 未暂停：两侧都记
        l.process_event(&db, &ev(EventType::KeyPress(Key::KeyA)));
        assert_eq!(hits.load(Ordering::Relaxed), 1, "未暂停时主链路应记录");
        assert!(device_side(&writer), "未暂停时设备侧应记录");
        assert!(writer.has_pending(), "未暂停时设备计数应落到写入器");

        // 暂停（走对外唯一入口）：共享位被翻起来，两侧一起停
        assert!(l.toggle_pause());
        assert!(
            paused.load(Ordering::Relaxed),
            "toggle_pause 改的必须是那份共享 Arc，而不是监听器自己的拷贝"
        );
        l.process_event(&db, &ev(EventType::KeyPress(Key::KeyB)));
        assert_eq!(
            hits.load(Ordering::Relaxed),
            1,
            "暂停后主链路不得记录（换个键名，绕开长按去重）"
        );
        assert!(!device_side(&writer), "暂停后设备侧不得记录");

        // 继续：同一份标志翻回去，两侧一起恢复
        l.set_paused(false);
        assert!(!l.is_paused() && !paused.load(Ordering::Relaxed));
        l.process_event(&db, &ev(EventType::KeyPress(Key::KeyC)));
        assert_eq!(hits.load(Ordering::Relaxed), 2, "恢复后主链路应重新记录");
        assert!(device_side(&writer), "恢复后设备侧应重新记录");

        // 线程退出要在临时目录被删之前（见 stop_and_wait 的注释）
        writer.stop_and_wait();
    }
}
