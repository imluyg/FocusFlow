//! 设备维度统计（Raw Input 侧信道）。
//!
//! 职责边界：本线程只回答「这次输入来自哪个设备」并按设备归属计数。
//! 主统计（键名、排行、活跃时长）仍由 rdev 低级钩子负责，两条链路并行：
//! - rdev 低级钩子：全设备统一、含 SendInput 合成输入，但拿不到设备身份
//! - Raw Input（本模块）：携带设备句柄、只收真实硬件输入（合成输入不产生 WM_INPUT）
//!
//! 由此，设备计数是**独立口径**（键盘按下 + 鼠标按键按下 + 滚轮事件，
//! 不做长按去重/修饰键过滤），数字不追求与键鼠统计相等。
//!
//! 暂停（托盘/设置的「暂停记录」）对两条链路的口径必须一致：本模块**不持有**暂停位，
//! 而是与 `InputListener` 共享同一份 `listener::PauseFlag`（见 `record_device_event`）。
//! 各存一份会出"今日计数冻住、设备排行还在涨"的假象，正是这条闸最初缺失的形态。
//!
//! 性能约定（重要，不要退化成轮询）：
//! - 完全事件驱动：无输入时线程在 GetMessageW 阻塞，零消耗
//! - 设备登记（取设备路径 + 查注册表 FriendlyName）只在**每个设备的首个事件**
//!   执行一次，结果缓存在线程本地 HashMap，此后每事件只查内存
//! - 鼠标移动等非计数事件走快速出口（读标志位后立即返回）

use std::sync::Arc;

use crate::db::DbWriter;
use crate::listener::{is_paused_now, PauseFlag};

/// 去掉设备路径的 `\\?\` 前缀，得到用作主键的实例路径。
pub(crate) fn strip_device_prefix(path: &str) -> &str {
    path.strip_prefix(r"\\?\").unwrap_or(path)
}

/// 设备路径 → 注册表 `Enum` 实例路径。
///
/// Raw Input 给的是 `HID#VID_24AE&PID_1464&MI_00#7&1f126e19&0&0000#{378de44c-...}`：
/// - `#` 是路径分隔符（注册表里是 `\`）
/// - 末尾 `#{GUID}` 是设备接口类 GUID，不属于 Enum 树，必须去掉
pub(crate) fn registry_instance_path(device_path: &str) -> String {
    let stripped = strip_device_prefix(device_path);
    let without_iface = match stripped.rfind("#{") {
        Some(idx) => &stripped[..idx],
        None => stripped,
    };
    without_iface.replace('#', "\\")
}

/// 常见通用设备名中文化（注册表 DeviceDesc 的英文兜底串；未命中原样返回）。
pub(crate) fn localize_generic_name(name: &str) -> String {
    match name {
        "HID-compliant mouse" => "HID 鼠标".to_string(),
        "HID-compliant keyboard" => "HID 键盘".to_string(),
        "HID Keyboard Device" => "HID 键盘".to_string(),
        "Standard PS/2 Keyboard" => "PS/2 键盘".to_string(),
        "Microsoft PS/2 Mouse" => "PS/2 鼠标".to_string(),
        "USB Input Device" => "USB 输入设备".to_string(),
        _ => name.to_string(),
    }
}

/// 无 VID/PID 设备（触控板、PS/2、ACPI 键盘等）的短标识：
/// 取实例路径中 `#` 分隔的第 2 段（设备 ID 段），如 `MSFT0001&Col01`。
pub(crate) fn short_device_tag(path: &str) -> String {
    let stripped = strip_device_prefix(path);
    let seg = stripped.split('#').nth(1).unwrap_or(stripped);
    let tag: String = seg.chars().take(24).collect();
    if tag.is_empty() {
        stripped.chars().take(24).collect()
    } else {
        tag
    }
}

/// 组设备显示名：`设备名 · VID/PID`，无注册表名时回退 `HID 设备 · VID/PID`。
///
/// 三类设备的显示串（与真机实测形态对应）：
/// - USB/2.4G：`HID 鼠标 · 24AE/1464`
/// - 蓝牙：`HID 鼠标 · 07D7/EFFF`
/// - 无 VID/PID（触控板 / PS/2 / ACPI）：`HID 鼠标 · MSFT0001&Col01`（短标识兜底，
///   避免多台无名设备并排时显示成同一串）
pub(crate) fn display_name(path: &str, friendly: Option<&str>) -> String {
    let vid_pid = crate::db::queries::parse_vid_pid(path);
    let suffix = match vid_pid {
        Some((vid, pid)) => format!("{vid}/{pid}"),
        None => short_device_tag(path),
    };
    match friendly {
        Some(f) => format!("{} · {suffix}", localize_generic_name(f)),
        None => format!("HID 设备 · {suffix}"),
    }
}

/// Raw Input 事件分类（自持常量镜像 Win32 定义，保证跨平台可测）。
pub(crate) mod classify {
    const RI_KEY_BREAK: u16 = 0x0001;
    const RI_MOUSE_LEFT_BUTTON_DOWN: u16 = 0x0001;
    const RI_MOUSE_RIGHT_BUTTON_DOWN: u16 = 0x0004;
    const RI_MOUSE_MIDDLE_BUTTON_DOWN: u16 = 0x0010;
    const RI_MOUSE_WHEEL: u16 = 0x0400;

    /// 键盘事件是否计数：按下计（RI_KEY_MAKE），释放（RI_KEY_BREAK）不计。
    pub fn keyboard_counts(flags: u16) -> bool {
        flags & RI_KEY_BREAK == 0
    }

    /// 鼠标事件是否计数：任意按键按下（左/右/中）或滚轮滚动计，移动不计。
    pub fn mouse_counts(flags: u16) -> bool {
        flags
            & (RI_MOUSE_LEFT_BUTTON_DOWN
                | RI_MOUSE_RIGHT_BUTTON_DOWN
                | RI_MOUSE_MIDDLE_BUTTON_DOWN
                | RI_MOUSE_WHEEL)
            != 0
    }

    /// 未识别键的归类名（键名明细里避免长尾一次性条目）。
    pub const OTHER_KEY: &str = "其他按键";

    const LETTERS: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    const DIGITS: &str = "0123456789";
    const NUMPAD: [&str; 10] = [
        "数字键盘0",
        "数字键盘1",
        "数字键盘2",
        "数字键盘3",
        "数字键盘4",
        "数字键盘5",
        "数字键盘6",
        "数字键盘7",
        "数字键盘8",
        "数字键盘9",
    ];
    const FKEYS: [&str; 24] = [
        "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13", "F14",
        "F15", "F16", "F17", "F18", "F19", "F20", "F21", "F22", "F23", "F24",
    ];

    /// Windows 虚拟键码 → 显示名（`RAWKEYBOARD.VKey`）。
    ///
    /// 命名与主统计（`listener`）同风格：字母/数字/符号原样，功能键用中文
    /// （空格/回车/左Shift…），小键盘加「数字键盘」前缀。未覆盖的返回 `None`。
    pub fn vkey_name(vk: u16) -> Option<&'static str> {
        Some(match vk {
            0x08 => "退格",
            0x09 => "Tab",
            0x0D => "回车",
            0x10 | 0xA0 => "左Shift",
            0xA1 => "右Shift",
            0x11 | 0xA2 => "左Ctrl",
            0xA3 => "右Ctrl",
            0x12 | 0xA4 => "左Alt",
            0xA5 => "右Alt",
            0x13 => "Pause",
            0x14 => "大写锁定",
            0x1B => "Esc",
            0x20 => "空格",
            0x21 => "PageUp",
            0x22 => "PageDown",
            0x23 => "End",
            0x24 => "Home",
            0x25 => "左方向键",
            0x26 => "上方向键",
            0x27 => "右方向键",
            0x28 => "下方向键",
            0x2C => "PrintScreen",
            0x2D => "Insert",
            0x2E => "Delete",
            0x30..=0x39 => &DIGITS[(vk - 0x30) as usize..(vk - 0x30) as usize + 1],
            0x41..=0x5A => &LETTERS[(vk - 0x41) as usize..(vk - 0x41) as usize + 1],
            0x5B => "左Win",
            0x5C => "右Win",
            0x5D => "菜单键",
            0x60..=0x69 => NUMPAD[(vk - 0x60) as usize],
            0x6A => "数字键盘*",
            0x6B => "数字键盘+",
            0x6D => "数字键盘-",
            0x6E => "数字键盘.",
            0x6F => "数字键盘/",
            0x70..=0x87 => FKEYS[(vk - 0x70) as usize],
            0x90 => "数字锁定",
            0x91 => "滚动锁定",
            0xBA => ";",
            0xBB => "=",
            0xBC => ",",
            0xBD => "-",
            0xBE => ".",
            0xBF => "/",
            0xC0 => "`",
            0xDB => "[",
            0xDC => "\\",
            0xDD => "]",
            0xDE => "'",
            _ => return None,
        })
    }

    /// 鼠标动作名：按键优先于滚轮；滚轮方向由 `usButtonData` 的符号决定（正=向上）。
    /// 命名与主统计一致（鼠标左键 / 滚轮上滑 …）。不计数的事件返回 `None`。
    pub fn mouse_action_name(flags: u16, data: i16) -> Option<&'static str> {
        if flags & RI_MOUSE_LEFT_BUTTON_DOWN != 0 {
            return Some("鼠标左键");
        }
        if flags & RI_MOUSE_RIGHT_BUTTON_DOWN != 0 {
            return Some("鼠标右键");
        }
        if flags & RI_MOUSE_MIDDLE_BUTTON_DOWN != 0 {
            return Some("鼠标中键");
        }
        if flags & RI_MOUSE_WHEEL != 0 {
            return Some(if data >= 0 {
                "滚轮上滑"
            } else {
                "滚轮下滑"
            });
        }
        None
    }
}

/// 设备统计线程回给启动路径的那一句实话。
///
/// 为什么要有它：`start_device_stats` 原先是 `spawn` 一成功就报
/// `CheckResult::ok("已启动（Raw Input 侧信道）")`，而线程本体 `win::run` 里有四条
/// API 早退（`GetModuleHandleW` / `RegisterClassW` / `CreateWindowExW` /
/// `RegisterRawInputDevices`）—— 那四条**全部**只 `tracing::error!` 然后 `return`。
/// 于是启动自检与线程的真实状态脱节：报告写着"已启动"，界面上设备排行整块空、
/// 一条设备计数都不涨，唯一的线索在 `logs/` 里。本仓对这一族"结论与现场脱节"
/// 已经修过好几处（定时备份线程、写线程、`--backup` 的结论），这是剩下的一角。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ThreadReport {
    Ready,
    Failed(String),
}

/// 启动那头等握手时看到的三种结局。"没等到"与"等到了坏消息"分开，
/// 是因为两句要说的话不一样，而把前者写成"已启动"就是这条缺陷的原始形状。
#[derive(Debug, PartialEq)]
pub(crate) enum StartupSignal {
    Confirmed,
    Refused(String),
    Unanswered,
}

/// 握手 → 启动自检那一条。**纯函数**：`startup.rs` 模块头告诫过别把结论放进进程级
/// 静态（测试里几十个用例各 init 一次 `Database`，静态会互相污染），所以这里只吃信号。
fn startup_check(step: &str, signal: StartupSignal) -> crate::startup::CheckResult {
    match signal {
        StartupSignal::Confirmed => {
            crate::startup::CheckResult::ok(step, "已启动（Raw Input 侧信道）")
        }
        StartupSignal::Refused(why) => {
            crate::startup::CheckResult::fail(step, format!("线程起来了但没注册成功：{why}"))
        }
        StartupSignal::Unanswered => {
            crate::startup::CheckResult::fail(step, "没等到设备统计线程的回报（超时或线程已退出）")
        }
    }
}

/// 等一次握手：超时与发送端被丢弃都算 `Unanswered`。
fn wait_for_report(
    rx: &std::sync::mpsc::Receiver<ThreadReport>,
    wait: std::time::Duration,
) -> StartupSignal {
    match rx.recv_timeout(wait) {
        Ok(ThreadReport::Ready) => StartupSignal::Confirmed,
        Ok(ThreadReport::Failed(why)) => StartupSignal::Refused(why),
        Err(_) => StartupSignal::Unanswered,
    }
}

/// 启动设备统计线程（`[device_stats] enabled=false` 可关停；非 Windows 无操作）。
///
/// `paused` 必须与 `InputListener` 持有的是同一份（见 `listener::PauseFlag`）：
/// 设备侧信道只有 `Arc<DbWriter>`，拿不到监听器，暂停状态只能靠这份共享标志传进来。
///
/// 返回自检结论（B15）：spawn 失败不让启动失败（设备排行悄悄消失比程序起不来轻），
/// 但不再吞进 `.ok()` —— 结论交启动报告显性化。
pub fn start_device_stats(writer: Arc<DbWriter>, paused: PauseFlag) -> crate::startup::CheckResult {
    let step = "设备统计线程";
    let config = crate::config::instance();
    if !config.get_bool("device_stats", "enabled", true) {
        tracing::info!("设备统计未启用（[device_stats] enabled=false）");
        return crate::startup::CheckResult::ok(step, "未启用（[device_stats] enabled=false）");
    }
    let (tx, rx) = std::sync::mpsc::channel::<ThreadReport>();
    match std::thread::Builder::new()
        .name("device-stats".into())
        .spawn(move || {
            #[cfg(windows)]
            win::run(writer, paused, tx);
            #[cfg(not(windows))]
            {
                let _ = (writer, paused, tx);
                tracing::info!("非 Windows 平台不支持设备统计");
            }
        }) {
        Ok(_handle) => {
            // 等一句实话，不等一个"看起来起来了"。正常是几毫秒（线程一注册完 Raw Input
            // 就回 Ready），给到 300ms 是为了不在句柄压力/杀软首扫那种时刻误报。
            let signal = wait_for_report(&rx, std::time::Duration::from_millis(300));
            let result = startup_check(step, signal);
            if !result.ok {
                tracing::error!("设备统计未确认启动: {}", result.detail);
            }
            result
        }
        Err(e) => {
            tracing::error!("启动设备统计线程失败: {e}");
            crate::startup::CheckResult::fail(step, format!("{e}"))
        }
    }
}

/// 解析不出实例路径的设备句柄，隔多久再试一次。
const REG_RETRY_GAP_SECS: u64 = 60;

/// 这一句判决只吃"距离上次失败多少秒"，所以能被用例钉住（拿 `Instant` 进来就不确定）。
///
/// 为什么要有节流：`Sink::on_input` 跑在 Raw Input 的消息循环线程上，而
/// `register_device` 一次要两趟 `GetRawInputDeviceInfoW` 加一趟开注册表键。
/// 以前解析失败时**什么都不记**，于是那个句柄的每一个键鼠事件都重跑一遍这套查询 ——
/// 与本模块头"只在每个设备的首个事件执行一次"的承诺正好相反；而且这台设备就此
/// 不出现在排行里、日志里一个字都没有。现在按句柄记账：首次出声并记时间，
/// `REG_RETRY_GAP_SECS` 之内不再重试（注册表/即插即用可能只是忙一下，到点再给一次机会）。
#[cfg_attr(not(windows), allow(dead_code))] // 非 Windows 下调用方（mod win）不存在
fn reg_retry_allowed(seconds_since_last_failure: Option<u64>) -> bool {
    match seconds_since_last_failure {
        None => true,
        Some(s) => s >= REG_RETRY_GAP_SECS,
    }
}

/// 记一次设备输入（次数 + 键名明细），返回是否真的记了。
///
/// 暂停闸就在这里、且只在这里判一次：读的是与 rdev 主链路 `record_event` **同一份**
/// `PauseFlag`，所以「今日计数冻住、设备排行还在涨」这个形态从结构上不可能再出现。
/// 位置也与主链路对齐（分类/登记都做完、即将写库时），暂停只是不落库。
///
/// 写成自由函数而不是埋在 `Sink::on_input` 里：Raw Input 回调在本机没法注入
/// （没人按键），把决定抽出来才能直接用例钉住这条闸。
#[cfg_attr(not(windows), allow(dead_code))] // 非 Windows 下调用方（mod win）不存在
pub(crate) fn record_device_event(
    writer: &DbWriter,
    paused: &PauseFlag,
    device_key: &str,
    name: &str,
    kind: &'static str,
    key_name: Option<&str>,
    ts: i64,
) -> bool {
    if is_paused_now(paused) {
        return false;
    }
    writer.record_device(device_key, name, kind, ts);
    // 键名明细：未识别的键归入「其他按键」，避免明细表被长尾撑爆
    writer.record_device_key(device_key, key_name.unwrap_or(classify::OTHER_KEY), ts);
    true
}

/// Windows 实现：消息窗口 + Raw Input 注册 + 消息循环。
#[cfg(windows)]
mod win {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::Arc;

    use windows::core::w;
    use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    // Raw Input API 位于 UI::Input 顶层（windows 0.61），不在 KeyboardAndMouse 子模块
    use windows::Win32::UI::Input::{
        GetRawInputData, GetRawInputDeviceInfoW, RegisterRawInputDevices, HRAWINPUT, RAWINPUT,
        RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK, RIDI_DEVICENAME, RID_INPUT,
        RIM_TYPEKEYBOARD, RIM_TYPEMOUSE,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW,
        TranslateMessage, HWND_MESSAGE, MSG, WM_INPUT, WNDCLASSW,
    };

    use super::classify;
    use super::{display_name, record_device_event, strip_device_prefix, PauseFlag};
    use crate::db::DbWriter;

    /// WndProc 与消息循环同线程，状态走线程本地。
    struct Sink {
        writer: Arc<DbWriter>,
        /// 与 rdev 主链路共享的暂停位（同一份 `Arc`，见 `listener::PauseFlag`）
        paused: PauseFlag,
        /// (hDevice 指针值, 事件类型) -> (device_key, 显示名)：设备首个事件登记一次，此后只读。
        /// 拔插后系统会分配新句柄，新句柄重新登记一次（路径相同 → device_key 不变，计数连续）。
        ///
        /// key 必须带上事件类型：句柄数值会被回收复用 —— Windows 关闭移除设备的句柄后，
        /// 同一数值可能发给另一台设备。只按数值缓存时，鼠标句柄的旧值被键盘复用就会把
        /// 键盘事件整段记到那只鼠标名下。按 (数值, 类型) 缓存零成本挡掉这类跨类型串号；
        /// 同类型复用仍挡不住（识别它要先解析路径，而那正是这层缓存省掉的开销）。
        devices: HashMap<(isize, &'static str), (String, String)>,
        /// 路径解析不出来的句柄 -> 上次尝试的时刻。只为节流存在（见 `reg_retry_allowed`）：
        /// 不记就变成"每个事件重跑一遍注册表查询"，记成永久失败又不给恢复的机会。
        failed_probe: HashMap<(isize, &'static str), std::time::Instant>,
    }

    thread_local! {
        static SINK: RefCell<Option<Sink>> = const { RefCell::new(None) };
    }

    /// 一条早退：既留下日志，也把原因递回启动路径那头的回音通道。
    fn refuse(report: &std::sync::mpsc::Sender<super::ThreadReport>, why: &str) {
        tracing::error!("设备统计：{why}");
        let _ = report.send(super::ThreadReport::Failed(why.to_string()));
    }

    /// 消息循环主入口（在专用线程上运行）。
    ///
    /// `report` 是把"到底起没起来"送回启动路径的那根线：下面四条 API 早退原先只留
    /// 一行日志，而 `start_device_stats` 已经在报"已启动"了 —— 现在这头由它等这份回音。
    /// `send` 失败只可能是启动那头不再等（超时或丢弃），不影响线程照常走下去。
    pub fn run(
        writer: Arc<DbWriter>,
        paused: PauseFlag,
        report: std::sync::mpsc::Sender<super::ThreadReport>,
    ) {
        unsafe {
            let Ok(hinstance) = GetModuleHandleW(None) else {
                refuse(&report, "GetModuleHandleW 失败");
                return;
            };
            let class_name = w!("FocusFlowDeviceStats");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wnd_proc),
                hInstance: hinstance.into(),
                lpszClassName: class_name,
                ..Default::default()
            };
            if RegisterClassW(&wc) == 0 {
                refuse(&report, "窗口类注册失败");
                return;
            }
            // 消息专用窗口（HWND_MESSAGE 父窗口）：不可见、不进枚举、只收消息
            let hwnd = match CreateWindowExW(
                Default::default(),
                class_name,
                w!(""),
                Default::default(),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(hinstance.into()),
                None,
            ) {
                Ok(hwnd) => hwnd,
                Err(e) => {
                    refuse(&report, &format!("创建消息窗口失败: {e}"));
                    return;
                }
            };
            // 注册鼠标(usage 2) + 键盘(usage 6)（Generic Desktop 页）；
            // RIDEV_INPUTSINK：无焦点时也接收输入（后台统计的前提）
            let rid = [
                RAWINPUTDEVICE {
                    usUsagePage: 0x01,
                    usUsage: 0x02,
                    dwFlags: RIDEV_INPUTSINK,
                    hwndTarget: hwnd,
                },
                RAWINPUTDEVICE {
                    usUsagePage: 0x01,
                    usUsage: 0x06,
                    dwFlags: RIDEV_INPUTSINK,
                    hwndTarget: hwnd,
                },
            ];
            if let Err(e) =
                RegisterRawInputDevices(&rid, std::mem::size_of::<RAWINPUTDEVICE>() as u32)
            {
                refuse(&report, &format!("Raw Input 注册失败: {e}"));
                return;
            }
            // 到这一步才算真的"起来了"：上面四条早退 any 一条，启动报告拿到的都是
            // 坏消息而不是"已启动"这句空话。
            let _ = report.send(super::ThreadReport::Ready);

            SINK.with(|s| {
                *s.borrow_mut() = Some(Sink {
                    writer,
                    paused,
                    devices: HashMap::new(),
                    failed_probe: HashMap::new(),
                })
            });
            tracing::info!("设备统计已启动（Raw Input，鼠标+键盘）");

            let mut msg = MSG::default();
            loop {
                // GetMessageW 返回 BOOL：0 = WM_QUIT（退出），-1 = 致命错误
                let r = GetMessageW(&mut msg, None, 0, 0).0;
                if r == 0 || r == -1 {
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            tracing::info!("设备统计线程已退出");
        }
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_INPUT {
            SINK.with(|s| {
                if let Some(sink) = s.borrow_mut().as_mut() {
                    sink.on_input(lparam);
                }
            });
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    impl Sink {
        fn on_input(&mut self, lparam: LPARAM) {
            // `mut` 不是可有可无的：`GetRawInputData` 是**输出**参数，它要往这块内存里写。
            // 原来写成 `let raw` + `&raw as *const _ as *mut c_void`，等于把共享引用转成
            // 可变指针再写进去 —— 那是 UB（编译器有权假设共享引用背后不变），
            // 下面 `&raw.header` / `raw.data.*` 读的又是刚被写过的字节。
            let mut raw: RAWINPUT = unsafe { std::mem::zeroed() };
            let hr = HRAWINPUT(lparam.0 as *mut core::ffi::c_void);
            let mut size = std::mem::size_of::<RAWINPUT>() as u32;
            let copied = unsafe {
                GetRawInputData(
                    hr,
                    RID_INPUT,
                    Some(&mut raw as *mut RAWINPUT as *mut core::ffi::c_void),
                    &mut size,
                    std::mem::size_of::<RAWINPUTHEADER>() as u32,
                )
            };
            if copied == 0 || copied == u32::MAX {
                return;
            }
            let header = &raw.header;
            // hDevice 为 0：无法归属设备（极少见的无源输入），跳过不计数
            if header.hDevice.is_invalid() {
                return;
            }
            // 事件分类（不计数事件走快速出口：读标志位后立即返回）
            let (counts, kind, key_name) =
                match windows::Win32::UI::Input::RID_DEVICE_INFO_TYPE(header.dwType) {
                    RIM_TYPEMOUSE => {
                        let mouse = unsafe { raw.data.mouse.Anonymous.Anonymous };
                        (
                            classify::mouse_counts(mouse.usButtonFlags),
                            "mouse",
                            // 滚轮方向看 usButtonData 的符号（正=向上），与主统计口径一致
                            classify::mouse_action_name(
                                mouse.usButtonFlags,
                                mouse.usButtonData as i16,
                            ),
                        )
                    }
                    RIM_TYPEKEYBOARD => {
                        let kb = unsafe { raw.data.keyboard };
                        let flags = kb.Flags as u16;
                        (
                            classify::keyboard_counts(flags),
                            "keyboard",
                            // VKey 为 255（无有效键码）时不记明细，只累加次数
                            classify::vkey_name(kb.VKey),
                        )
                    }
                    _ => return,
                };
            if !counts {
                return;
            }
            // 设备登记：首个事件查一次（路径 + 注册表名），此后全部缓存命中
            let cache_key = (header.hDevice.0 as isize, kind);
            let entry = match self.devices.get(&cache_key) {
                Some(e) => e.clone(),
                None => match register_device(header.hDevice) {
                    Some(e) => {
                        tracing::info!("设备统计：登记新设备 {}", e.0);
                        self.devices.insert(cache_key, e.clone());
                        e
                    }
                    None => {
                        // 无法解析路径：宁可不计也不张冠李戴。但不能每来一个事件就把
                        // 两趟 Win32 查询加一次开注册表键再跑一遍 —— 这里跑在
                        // Raw Input 的消息循环线程上，而模块头承诺的是"首个事件查一次"。
                        // 现在按句柄节流，并且**出声一次**：这台设备不会出现在排行里，
                        // 一声不响就没人知道它为什么不见。
                        let now = std::time::Instant::now();
                        let since = self
                            .failed_probe
                            .get(&cache_key)
                            .map(|t| t.elapsed().as_secs());
                        if super::reg_retry_allowed(since) {
                            self.failed_probe.insert(cache_key, now);
                            tracing::warn!(
                                "设备统计：设备句柄 {:#x}（{}）的实例路径解析不出来，本次不计；\
                                 每 {} 秒再试一次（拔插进行中或注册表正被占用时就是这个形态）",
                                cache_key.0,
                                cache_key.1,
                                super::REG_RETRY_GAP_SECS
                            );
                        }
                        return;
                    }
                },
            };
            let ts = match crate::listener::now_ts_secs() {
                Some(ts) => ts,
                // 时钟在历元之前：补 0 会把这条设备计数写进 focusflow_1970.db
                // 并永久留在「总计」里，见 `listener::unix_ts_secs` 的注释
                None => return,
            };
            // 落库（含暂停闸）统一走 record_device_event，与主链路同一份真相
            record_device_event(
                &self.writer,
                &self.paused,
                &entry.0,
                &entry.1,
                kind,
                key_name,
                ts,
            );
        }
    }

    /// 登记设备：实例路径（主键）+ 显示名。失败返回 None。
    fn register_device(handle: HANDLE) -> Option<(String, String)> {
        let path = device_path(handle)?;
        // 归组键 = 硬件身份段（B14-2）：原来是完整实例路径，换 USB 口就变 ——
        // 同一台设备新起一行、计数从零开始、别名不跟。现在取 `#` 分段后的
        // 型号+接口段，换口不换键；完整路径仍用于注册表名与 VID/PID 显示名。
        let device_key = crate::device_alias::hardware_identity_key(strip_device_prefix(&path));
        let friendly = registry_device_name(&path);
        let name = display_name(&path, friendly.as_deref());
        Some((device_key, name))
    }

    /// 取设备实例路径（如 `\\?\HID#VID_046D&PID_C52B&MI_00#8&...&0000`）。
    pub(super) fn device_path(handle: HANDLE) -> Option<String> {
        unsafe {
            let mut size: u32 = 0;
            // 第一次调用只取所需长度（pdata = None）
            let _ = GetRawInputDeviceInfoW(Some(handle), RIDI_DEVICENAME, None, &mut size);
            if size == 0 {
                return None;
            }
            let mut buf = vec![0u16; size as usize];
            let copied = GetRawInputDeviceInfoW(
                Some(handle),
                RIDI_DEVICENAME,
                Some(buf.as_mut_ptr().cast()),
                &mut size,
            );
            if copied == 0 || copied == u32::MAX {
                return None;
            }
            let len = buf
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(copied as usize)
                .min(buf.len());
            Some(String::from_utf16_lossy(&buf[..len]))
        }
    }

    /// 设备可读名（`HKLM\SYSTEM\CurrentControlSet\Enum\<实例>` 下）。
    ///
    /// 真机实测（2026-09-20）：HID 子设备的 `FriendlyName` **恒为空**，
    /// 名字在 `DeviceDesc` 里且是 INF 资源串形式
    /// （`@msmouse.inf,%hid.mousedevice%;HID-compliant mouse`）——
    /// 取最后一个 `;` 之后那段才是可读名。两条都失败才返回 None。
    pub(super) fn registry_device_name(device_path: &str) -> Option<String> {
        let hk = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE);
        let key = hk
            .open_subkey(format!(
                r"SYSTEM\CurrentControlSet\Enum\{}",
                super::registry_instance_path(device_path)
            ))
            .ok()?;
        if let Ok(name) = key.get_value::<String, _>("FriendlyName") {
            let t = name.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
        let desc: String = key.get_value("DeviceDesc").ok()?;
        let t = desc.rsplit(';').next().unwrap_or("").trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 启动结论必须照线程的真话说：三条判决各说各的，谁都不许替谁撒谎。
    ///
    /// 旧写法是 `spawn` 一成功就 `CheckResult::ok("已启动")`，而线程内部四条 API
    /// 早退只写日志 ⇒ 自检报告与"设备排行整块空、一条计数不涨"这个现场脱节。
    /// `Unanswered` 那一档特意也判 fail：没等到回音**不是**证明起来了的证据。
    #[test]
    fn the_startup_report_follows_the_thread_handshake() {
        let up = startup_check("设备统计线程", StartupSignal::Confirmed);
        assert!(up.ok && up.detail.contains("已启动"), "{up:?}");

        let refused = startup_check(
            "设备统计线程",
            StartupSignal::Refused("Raw Input 注册失败: 拒绝访问".to_string()),
        );
        assert!(!refused.ok, "线程自己报了失败，结论不能是 ok：{refused:?}");
        assert!(
            refused.detail.contains("Raw Input 注册失败"),
            "原因要原样递到启动报告，否则读报告的人只剩一句\"没起来\": {refused:?}"
        );

        let quiet = startup_check("设备统计线程", StartupSignal::Unanswered);
        assert!(!quiet.ok, "没等到回音不能写成已启动: {quiet:?}");
        assert!(
            quiet.detail.contains("没等到"),
            "这一档要说清是\"没等到\"，与\"确认失败\"是两件事: {quiet:?}"
        );
    }

    /// 回音通道的三种结局各归哪一档 —— 钉的是"等"那一步，不是判决表。
    #[test]
    fn the_handshake_maps_channel_outcomes_to_signals() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(ThreadReport::Ready).unwrap();
        assert_eq!(
            wait_for_report(&rx, std::time::Duration::from_millis(500)),
            StartupSignal::Confirmed
        );

        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(ThreadReport::Failed("窗口类注册失败".to_string()))
            .unwrap();
        assert_eq!(
            wait_for_report(&rx, std::time::Duration::from_millis(500)),
            StartupSignal::Refused("窗口类注册失败".to_string())
        );

        // 线程退场却什么都没说（发送端被丢弃）与真超时，都归"没等到"
        let (tx, rx) = std::sync::mpsc::channel::<ThreadReport>();
        drop(tx);
        assert_eq!(
            wait_for_report(&rx, std::time::Duration::from_millis(500)),
            StartupSignal::Unanswered
        );
        let (_keep, rx) = std::sync::mpsc::channel::<ThreadReport>();
        assert_eq!(
            wait_for_report(&rx, std::time::Duration::from_millis(30)),
            StartupSignal::Unanswered
        );
    }

    /// 线程本体的每条早退都必须回音 —— 钉的是接线，不是判决表。
    ///
    /// 为什么要在源码上数：`win::run` 里那段要真建消息窗口、真注册 Raw Input，还会
    /// 一路 `GetMessageW` 卡住不放，用例起不了它；而"纯函数绿、调用方没接上"正是
    /// 这类缺陷的盲点（`--device-keys` 那一次就是这么漏的）。四条 `return` 对四条
    /// `refuse` 数不齐就红 —— 以后再加一条早退而忘了回音，红点会指在这里。
    #[test]
    fn every_early_exit_in_the_thread_body_reports_back() {
        let src = include_str!("device_stats.rs");
        let at = src.find("pub fn run(").expect("win::run 不见了");
        let rest = &src[at..];
        let body = &rest[..rest.find("SINK.with").expect("线程本体的锚点不见了")];
        let exits = body.match_indices("return;").count();
        let echoes = body.match_indices("refuse(&report").count();
        assert!(
            exits >= 4,
            "线程本体该有四条早退，现在数到 {exits} 条 —— 说明这一段被改过了，重新对一遍"
        );
        assert_eq!(
            exits, echoes,
            "{exits} 条 return 只配了 {echoes} 条回音 ⇒ 少掉的那条会让启动报告说谎"
        );
        assert_eq!(
            body.match_indices("ThreadReport::Ready").count(),
            1,
            "Ready 只许在 Raw Input 注册成功之后发一次"
        );
    }

    /// 解析不出实例路径的设备句柄：首个事件要试、六十秒内不许每个事件都试、到点再给一次机会。
    ///
    /// 旧写法是 `None => return`，什么都不记 ⇒ 那个句柄的每个键鼠事件都在 Raw Input 的
    /// 消息循环线程上重跑两趟 `GetRawInputDeviceInfoW` + 一次开注册表键，与本模块头
    /// "只在每个设备的首个事件执行一次"的承诺正好相反；而那台设备永远不会出现在排行里，
    /// 日志里一个字都没有。
    #[test]
    fn an_unresolvable_device_is_retried_on_a_budget_not_every_event() {
        assert!(
            reg_retry_allowed(None),
            "首个事件必须试一次，否则新设备永远进不了排行"
        );
        assert!(
            !reg_retry_allowed(Some(0)),
            "同一秒内的后续事件不能再跑一遍注册表查询"
        );
        assert!(!reg_retry_allowed(Some(59)));
        assert!(
            reg_retry_allowed(Some(60)),
            "到点要再给一次机会：注册表/即插即用可能只是忙一下"
        );
    }

    /// 接线守卫：那条早退必须真的走节流并出声（纯函数绿不代表调用方读了它）。
    #[test]
    fn the_unresolvable_branch_actually_uses_the_budget() {
        let src = include_str!("device_stats.rs");
        let start = src.find("fn on_input(").expect("Sink::on_input 不见了");
        let end = src[start..]
            .find("fn register_device(")
            .map(|n| n + start)
            .expect("register_device 不见了");
        let body = &src[start..end];
        assert!(
            body.contains("reg_retry_allowed(since)"),
            "解析失败那支又回到每个事件重跑一遍注册表查询"
        );
        assert!(
            body.contains("tracing::warn!"),
            "这台设备不会出现在排行里，至少要说一句它为什么不见"
        );
    }

    /// 路径前缀剥离：`\\?\` 前缀去掉，无前缀原样返回。
    #[test]
    fn strip_prefix_variants() {
        assert_eq!(
            strip_device_prefix(r"\\?\HID#VID_046D&PID_C52B"),
            "HID#VID_046D&PID_C52B"
        );
        assert_eq!(strip_device_prefix("HID#X"), "HID#X");
    }

    /// 显示名组合：FriendlyName + VID/PID；通用英文名中文化；无名回退；无 VID/PID 用短标识。
    #[test]
    fn display_name_combinations() {
        assert_eq!(
            display_name(
                r"\\?\HID#VID_046D&PID_C52B&MI_00#x",
                Some("HID-compliant mouse")
            ),
            "HID 鼠标 · 046D/C52B"
        );
        assert_eq!(
            display_name(r"\\?\HID#VID_046D&PID_C52B&MI_00#x", None),
            "HID 设备 · 046D/C52B"
        );
        // 蓝牙形态（真机实测路径）
        assert_eq!(
            display_name(
                r"\\?\HID#{00001812-0000-1000-8000-00805f9b34fb}_Dev_VID&0107d7_PID&efff_REV&0120_x&Col03#9&20337b89&0&0002#{378de44c-56ef-11d1-bc8c-00a0c91405dd}",
                Some("HID Keyboard Device")
            ),
            "HID 键盘 · 07D7/EFFF"
        );
        // 无 VID/PID（触控板）：短标识兜底，不裸奔成同一个名字
        assert_eq!(
            display_name(
                r"\\?\HID#MSFT0001&Col01#5&36f79095&0&0000#{378de44c-56ef-11d1-bc8c-00a0c91405dd}",
                Some("HID-compliant mouse")
            ),
            "HID 鼠标 · MSFT0001&Col01"
        );
        // 非通用名（厂商名）原样保留
        assert_eq!(
            display_name(r"\\?\HID#VID_046D&PID_C52B#x", Some("USB 接收器")),
            "USB 接收器 · 046D/C52B"
        );
        assert_eq!(
            display_name(
                r"\\?\ACPI#MSFT0001#4&f25ce6e&0",
                Some("Standard PS/2 Keyboard")
            ),
            "PS/2 键盘 · MSFT0001"
        );
    }

    /// 注册表实例路径转换：`#`→`\`，末尾接口 `#{GUID}` 段剥离。
    #[test]
    fn registry_instance_path_conversion() {
        assert_eq!(
            registry_instance_path(
                r"\\?\HID#VID_24AE&PID_1464&MI_00#7&1f126e19&0&0000#{378de44c-56ef-11d1-bc8c-00a0c91405dd}"
            ),
            r"HID\VID_24AE&PID_1464&MI_00\7&1f126e19&0&0000"
        );
        // 无接口 GUID 段时原样转换
        assert_eq!(
            registry_instance_path(r"\\?\ACPI#MSFT0001#4&f25ce6e&0"),
            r"ACPI\MSFT0001\4&f25ce6e&0"
        );
    }

    /// 通用英文设备名中文化；未知名不篡改。
    #[test]
    fn generic_names_localized() {
        assert_eq!(localize_generic_name("HID-compliant mouse"), "HID 鼠标");
        assert_eq!(localize_generic_name("HID Keyboard Device"), "HID 键盘");
        assert_eq!(localize_generic_name("Standard PS/2 Keyboard"), "PS/2 键盘");
        assert_eq!(localize_generic_name("USB Receiver"), "USB Receiver");
    }

    /// 键盘分类：按下计、释放不计。
    #[test]
    fn keyboard_flags_classification() {
        assert!(classify::keyboard_counts(0), "按下（无标志）计");
        assert!(
            classify::keyboard_counts(0x0002 | 0x0004),
            "E0/E1 前缀的按下也计"
        );
        assert!(!classify::keyboard_counts(0x0001), "释放不计");
        assert!(!classify::keyboard_counts(0x0001 | 0x0002), "E0 释放不计");
    }

    /// 鼠标分类：三类按键按下与滚轮计，移动（无标志）与释放不计。
    #[test]
    fn mouse_flags_classification() {
        assert!(classify::mouse_counts(0x0001), "左键按下");
        assert!(classify::mouse_counts(0x0004), "右键按下");
        assert!(classify::mouse_counts(0x0010), "中键按下");
        assert!(classify::mouse_counts(0x0400), "滚轮");
    }

    /// 键名映射：常见键覆盖 + 未识别返回 None（调用方归入「其他按键」）
    #[test]
    fn key_name_mapping_covers_common_keys() {
        assert_eq!(classify::vkey_name(0x41), Some("A"));
        assert_eq!(classify::vkey_name(0x5A), Some("Z"));
        assert_eq!(classify::vkey_name(0x31), Some("1"));
        assert_eq!(classify::vkey_name(0x20), Some("空格"));
        assert_eq!(classify::vkey_name(0x0D), Some("回车"));
        assert_eq!(classify::vkey_name(0x1B), Some("Esc"));
        assert_eq!(classify::vkey_name(0xA0), Some("左Shift"));
        assert_eq!(classify::vkey_name(0x25), Some("左方向键"));
        assert_eq!(classify::vkey_name(0x70), Some("F1"));
        assert_eq!(classify::vkey_name(0x87), Some("F24"));
        assert_eq!(classify::vkey_name(0x61), Some("数字键盘1"));
        assert_eq!(classify::vkey_name(0xDC), Some("\\"));
        assert_eq!(classify::vkey_name(0xFF), None, "无有效键码");
        assert_eq!(classify::vkey_name(0x07), None, "未覆盖的键");
    }

    /// 鼠标动作名：三键优先 + 滚轮方向（正=上滑）；抬起事件不记明细
    #[test]
    fn mouse_action_names() {
        assert_eq!(classify::mouse_action_name(0x0001, 0), Some("鼠标左键"));
        assert_eq!(classify::mouse_action_name(0x0004, 0), Some("鼠标右键"));
        assert_eq!(classify::mouse_action_name(0x0010, 0), Some("鼠标中键"));
        assert_eq!(classify::mouse_action_name(0x0400, 120), Some("滚轮上滑"));
        assert_eq!(classify::mouse_action_name(0x0400, -120), Some("滚轮下滑"));
        assert_eq!(classify::mouse_action_name(0x0002, 0), None, "按键抬起");
        assert_eq!(classify::mouse_action_name(0x0008, 0), None, "移动不记明细");
        assert!(
            classify::mouse_counts(0x0004 | 0x0008),
            "右键按下+释放（一次事件同报两个标志）"
        );
        assert!(!classify::mouse_counts(0), "纯移动不计");
        assert!(!classify::mouse_counts(0x0002), "左键释放不计");
        assert!(!classify::mouse_counts(0x0008), "右键释放不计");
        assert!(!classify::mouse_counts(0x0020), "中键释放不计");
    }

    /// 暂停必须同样闸住设备侧信道（回归「设备侧只持 `Arc<DbWriter>`、结构上拿不到暂停位」）：
    /// 共享位置位时一次都不记、清掉后照常记。判的是真实 `DbWriter` 的待落库增量，
    /// 所以「今日计数冻住、设备排行还在涨」这个形态被钉在这里。
    ///
    /// 判定用 `record_device_event` 这个落库点：Raw Input 回调在本机注入不了（没人按键），
    /// 而 `Sink::on_input` 落库前唯一经过的就是这个函数。
    #[test]
    fn device_records_are_gated_by_the_shared_pause_flag() {
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("dev_pause_gate");
        let paused = crate::listener::new_pause_flag();
        let ts = crate::listener::now_ts_secs().expect("本机时钟应正常");
        let key = r"HID#VID_046D&PID_C52B#7&1f126e19&0&0000";
        let name = "HID 鼠标 · 046D/C52B";
        // 一次设备输入的等价调用；writer 的 flush 间隔给到一小时，
        // 用例期间不会有后台落库把增量取走（has_pending 才是"这一条记没记"的判据）
        let write = |w: &DbWriter| {
            record_device_event(w, &paused, key, name, "mouse", Some("鼠标左键"), ts)
        };

        // 未暂停：次数 + 键名明细两条增量都进了写入器（尚未落库 → has_pending 为真）
        let running =
            DbWriter::start(Duration::from_secs(3600)).expect("测试里 DB 写线程必须能启动");
        assert!(write(&running), "未暂停时应记录");
        assert!(
            running.has_pending(),
            "未暂停时设备计数必须落到写入器，否则设备排行永远是空的"
        );

        // 暂停：一条都不记。另用一个 writer —— 复用上面那个的话 has_pending 恒为真，
        // 就测不出"这一条到底记没记"。
        let stopped =
            DbWriter::start(Duration::from_secs(3600)).expect("测试里 DB 写线程必须能启动");
        paused.store(true, Ordering::Relaxed);
        assert!(!write(&stopped), "暂停时应拒绝记录");
        assert!(
            !stopped.has_pending(),
            "暂停后设备侧信道不得再产生增量（这正是原 bug：托盘暂停后「设备排行」继续涨）"
        );

        // 解除暂停：同一份共享位翻回来，设备侧信道立刻跟着恢复
        paused.store(false, Ordering::Relaxed);
        assert!(write(&stopped), "恢复后应重新记录");
        assert!(stopped.has_pending(), "恢复后设备计数应重新落进写入器");

        // 收尾：线程必须在临时目录被删之前退出（见 DbWriter::stop_and_wait 的注释）
        running.stop_and_wait();
        stopped.stop_and_wait();
    }

    /// 手动冒烟：枚举本机 Raw Input 设备并打印解析出的展示名，
    /// 用于在真机上确认「设备排行」里会出现的行（CI 无输入设备，默认不跑）。
    /// 运行：`cargo test -p focusflow-core -- --ignored --nocapture smoke_list_local_devices`
    #[cfg(windows)]
    #[test]
    #[ignore = "需要本机真实键鼠设备，手动运行"]
    fn smoke_list_local_devices() {
        use windows::Win32::UI::Input::{GetRawInputDeviceList, RAWINPUTDEVICELIST};
        unsafe {
            let mut count: u32 = 0;
            let size = std::mem::size_of::<RAWINPUTDEVICELIST>() as u32;
            assert_ne!(
                GetRawInputDeviceList(None, &mut count, size),
                u32::MAX,
                "设备枚举失败"
            );
            assert!(count > 0, "本机应至少有一个 Raw Input 设备");
            let mut list = vec![RAWINPUTDEVICELIST::default(); count as usize];
            let got = GetRawInputDeviceList(Some(list.as_mut_ptr()), &mut count, size);
            assert_ne!(got, u32::MAX, "设备列表读取失败");
            println!("本机 Raw Input 设备（type / 实例路径 / 展示名）：");
            for d in list.iter().take(count as usize) {
                let kind = if d.dwType == windows::Win32::UI::Input::RIM_TYPEMOUSE {
                    "mouse"
                } else if d.dwType == windows::Win32::UI::Input::RIM_TYPEKEYBOARD {
                    "keyboard"
                } else {
                    "other"
                };
                let path = win::device_path(d.hDevice).unwrap_or_default();
                let key = strip_device_prefix(&path).to_string();
                let name = display_name(&path, win::registry_device_name(&path).as_deref());
                println!("{kind:9} | {name}\n            {key}");
            }
        }
    }
}
