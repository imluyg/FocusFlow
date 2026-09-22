//! 键鼠监听模块。
//!
//! 镜像 Python 版 `listener.py`：
//! - rdev 全局键盘/鼠标监听（键盘 + 鼠标点击/滚轮统一计数）
//! - 修饰键/功能键过滤
//! - 长按自动重复过滤（`_pressed` 集合 + stale 时长）
//! - 滚轮连续滚动合并（0.8s 窗口内同方向只计 1 次）
//! - Ctrl+字母控制字符还原为物理键（v1.2.1 行为）
//! - 暂停/恢复，事件回调（番茄钟 / 护眼提醒用）

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use rdev::{listen, Button, Event, EventType, Key};

use crate::config::FocusFlowConfig;
use crate::db::Database;

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
            config
                .get_float("listener", "key_repeat_stale_seconds", 15.0)
                .to_bits(),
            Relaxed,
        );
        self.scroll_burst_window.store(
            config
                .get_float("listener", "scroll_burst_window", 0.8)
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
    /// 暂停状态
    paused: Mutex<bool>,
    /// 事件回调（番茄钟 / 护眼提醒）
    key_callbacks: Mutex<Vec<KeyCallback>>,
    /// 监听线程是否存活
    alive: Arc<Mutex<bool>>,
}

impl InputListener {
    pub fn new(config: &'static FocusFlowConfig) -> Arc<Self> {
        let listener = Self {
            config,
            cfg: ListenerCfg::default(),
            pressed: Mutex::new(HashMap::new()),
            // None 表示「还没有滚过」，等价于上一次的合并窗口早已过期。
            // 不能写成 `Instant::now() - Duration::from_secs(10)`：开机不足 10 秒时
            // Instant 减法下溢 panic，而 release 的 panic=abort 会让开机自启变成启动即崩
            // （app_stats.rs / queries.rs 里对同一个坑留过告诫）。
            scroll: Mutex::new((None, "上")),
            paused: Mutex::new(false),
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
        *self.paused.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_paused(&self, paused: bool) {
        let mut p = self.paused.lock().unwrap_or_else(|e| e.into_inner());
        if *p == paused {
            return;
        }
        *p = paused;
        drop(p);
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

    /// 滚轮连续滚动合并：窗口内同方向只计 1 次。
    fn is_new_scroll_burst(&self, direction: &'static str) -> bool {
        let window = self.cfg.burst_window();
        let now = Instant::now();
        let mut scroll = self.scroll.lock().unwrap_or_else(|e| e.into_inner());
        let is_new = match scroll.0 {
            // 尚无记录（首次，或暂停后刚重置）：一定是新一轮
            None => true,
            Some(t) => {
                now.duration_since(t) > Duration::from_secs_f64(window) || scroll.1 != direction
            }
        };
        *scroll = (Some(now), direction);
        is_new
    }

    /// 处理单个键鼠事件：记录到数据库 + 触发回调。
    fn record_event(&self, db: &Database, key_name: &str) {
        if self.is_paused() {
            return;
        }
        let ts = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
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
                    return;
                }
                let direction = scroll_direction(*delta_y);
                // 连续滚动合并
                if !self.is_new_scroll_burst(direction) {
                    return;
                }
                let name = format!("滚轮{}滑", direction);
                self.record_event(db, &name);
            }
            EventType::MouseMove { .. } => {}
        }
    }

    /// 启动监听（在后台线程运行）。
    pub fn start(self: &Arc<Self>, db: Arc<Database>) {
        let mut alive = self.alive.lock().unwrap_or_else(|e| e.into_inner());
        if *alive {
            return;
        }
        *alive = true;
        drop(alive);

        let this = Arc::clone(self);
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
            .expect("启动输入监听线程失败");
        tracing::info!(
            "键鼠监听已启动 (ignore_modifiers={}, ignore_functions={}, mouse_enabled={})",
            self.config
                .get_bool("listener", "ignore_modifier_keys", false),
            self.config
                .get_bool("listener", "ignore_function_keys", false),
            self.config.get_bool("listener", "mouse_enabled", true),
        );
    }

    /// 停止监听。
    pub fn stop(&self) {
        *self.alive.lock().unwrap_or_else(|e| e.into_inner()) = false;
        tracing::info!("键鼠监听已停止");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::SystemTime;

    fn test_config() -> &'static FocusFlowConfig {
        let dir = std::env::temp_dir().join(format!("ff_listener_cfg_{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        Box::leak(Box::new(
            FocusFlowConfig::load(dir.join("config.ini")).unwrap(),
        ))
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
        let l = InputListener::new(test_config());
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
        let l = InputListener::new(test_config());
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
        let l = InputListener::new(test_config());
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

    /// process_event 过滤分支：修饰键/功能键/未知键/鼠标关闭均不触发回调；
    /// ignore_key_repeat 生效时长按重复也不计数。
    #[test]
    fn process_event_filters_keys_and_respects_mouse_toggle() {
        let _lock = crate::paths::test_app_dir_lock();
        let dir = std::env::temp_dir().join(format!("ff_listener_db_{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        crate::paths::set_app_dir(&dir);
        let db = crate::db::Database::init_readonly();

        let l = InputListener::new(test_config());
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
        l.process_event(
            &db,
            &ev(rdev::EventType::Wheel {
                delta_x: 0,
                delta_y: 120,
            }),
        );
        assert_eq!(hits.load(Ordering::Relaxed), 2);
        l.process_event(
            &db,
            &ev(rdev::EventType::Wheel {
                delta_x: 0,
                delta_y: 120,
            }),
        );
        assert_eq!(hits.load(Ordering::Relaxed), 2, "窗口内同方向滚轮应合并");
        l.process_event(
            &db,
            &ev(rdev::EventType::Wheel {
                delta_x: 0,
                delta_y: -120,
            }),
        );
        assert_eq!(hits.load(Ordering::Relaxed), 3, "方向切换应计数");

        crate::paths::set_app_dir(std::env::temp_dir().join("ff_restore_nonexistent"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
