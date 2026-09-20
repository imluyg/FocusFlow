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
//! 性能约定（重要，不要退化成轮询）：
//! - 完全事件驱动：无输入时线程在 GetMessageW 阻塞，零消耗
//! - 设备登记（取设备路径 + 查注册表 FriendlyName）只在**每个设备的首个事件**
//!   执行一次，结果缓存在线程本地 HashMap，此后每事件只查内存
//! - 鼠标移动等非计数事件走快速出口（读标志位后立即返回）

use std::sync::Arc;

use crate::db::DbWriter;

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
}

/// 启动设备统计线程（[device_stats] enabled=false 可关停；非 Windows 无操作）。
pub fn start_device_stats(writer: Arc<DbWriter>) {
    let config = crate::config::instance();
    if !config.get_bool("device_stats", "enabled", true) {
        tracing::info!("设备统计未启用（[device_stats] enabled=false）");
        return;
    }
    std::thread::Builder::new()
        .name("device-stats".into())
        .spawn(move || {
            #[cfg(windows)]
            win::run(writer);
            #[cfg(not(windows))]
            {
                let _ = writer;
                tracing::info!("非 Windows 平台不支持设备统计");
            }
        })
        .expect("启动设备统计线程失败");
}

/// Windows 实现：消息窗口 + Raw Input 注册 + 消息循环。
#[cfg(windows)]
mod win {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

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
    use super::{display_name, strip_device_prefix};
    use crate::db::DbWriter;

    /// WndProc 与消息循环同线程，状态走线程本地。
    struct Sink {
        writer: Arc<DbWriter>,
        /// hDevice 指针值 -> (device_key, 显示名)：设备首个事件登记一次，此后只读。
        /// 拔插后系统会分配新句柄，新句柄重新登记一次（路径相同 → device_key 不变，计数连续）。
        devices: HashMap<isize, (String, String)>,
    }

    thread_local! {
        static SINK: RefCell<Option<Sink>> = const { RefCell::new(None) };
    }

    /// 消息循环主入口（在专用线程上运行）。
    pub fn run(writer: Arc<DbWriter>) {
        unsafe {
            let Ok(hinstance) = GetModuleHandleW(None) else {
                tracing::error!("设备统计：GetModuleHandleW 失败");
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
                tracing::error!("设备统计：窗口类注册失败");
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
                    tracing::error!("设备统计：创建消息窗口失败: {e}");
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
                tracing::error!("设备统计：Raw Input 注册失败: {e}");
                return;
            }

            SINK.with(|s| {
                *s.borrow_mut() = Some(Sink {
                    writer,
                    devices: HashMap::new(),
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
            let raw: RAWINPUT = unsafe { std::mem::zeroed() };
            let hr = HRAWINPUT(lparam.0 as *mut core::ffi::c_void);
            let mut size = std::mem::size_of::<RAWINPUT>() as u32;
            let copied = unsafe {
                GetRawInputData(
                    hr,
                    RID_INPUT,
                    Some(&raw as *const RAWINPUT as *mut core::ffi::c_void),
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
            let (counts, kind) =
                match windows::Win32::UI::Input::RID_DEVICE_INFO_TYPE(header.dwType) {
                    RIM_TYPEMOUSE => {
                        let flags = unsafe { raw.data.mouse.Anonymous.Anonymous.usButtonFlags };
                        (classify::mouse_counts(flags), "mouse")
                    }
                    RIM_TYPEKEYBOARD => {
                        let flags = unsafe { raw.data.keyboard.Flags } as u16;
                        (classify::keyboard_counts(flags), "keyboard")
                    }
                    _ => return,
                };
            if !counts {
                return;
            }
            // 设备登记：首个事件查一次（路径 + 注册表名），此后全部缓存命中
            let handle_key = header.hDevice.0 as isize;
            let entry = match self.devices.get(&handle_key) {
                Some(e) => e.clone(),
                None => match register_device(header.hDevice) {
                    Some(e) => {
                        tracing::info!("设备统计：登记新设备 {}", e.0);
                        self.devices.insert(handle_key, e.clone());
                        e
                    }
                    None => return, // 无法解析路径：宁可不计也不张冠李戴
                },
            };
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            self.writer.record_device(&entry.0, &entry.1, kind, ts);
        }
    }

    /// 登记设备：实例路径（主键）+ 显示名。失败返回 None。
    fn register_device(handle: HANDLE) -> Option<(String, String)> {
        let path = device_path(handle)?;
        let device_key = strip_device_prefix(&path).to_string();
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
            display_name(
                r"\\?\HID#VID_046D&PID_C52B#x",
                Some("Logitech USB Receiver")
            ),
            "Logitech USB Receiver · 046D/C52B"
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
        assert_eq!(localize_generic_name("Rapoo Receiver"), "Rapoo Receiver");
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
        assert!(
            classify::mouse_counts(0x0004 | 0x0008),
            "右键按下+释放（一次事件同报两个标志）"
        );
        assert!(!classify::mouse_counts(0), "纯移动不计");
        assert!(!classify::mouse_counts(0x0002), "左键释放不计");
        assert!(!classify::mouse_counts(0x0008), "右键释放不计");
        assert!(!classify::mouse_counts(0x0020), "中键释放不计");
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
