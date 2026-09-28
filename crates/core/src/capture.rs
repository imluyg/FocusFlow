//! 屏幕截取：把「鼠标所在的那块屏」冻结成一份内存里的位图，并按选区裁出 PNG。
//!
//! 职责边界：这里**只负责取像素和算坐标** —— 不开窗口、不写盘、不驱动剪贴板。
//! 覆盖层窗口与会话状态在 `desktop/src/snip.rs`。
//!
//! 为什么放 core 不放 desktop：`windows` 依赖的 `Win32_Graphics_Gdi` 特性只在 core 开着，
//! 而且这样纯函数（裁剪、换算、命名）能脱离 Tauri 上下文被单测覆盖。
//!
//! 一条贯穿本模块的取值约定：**缓冲区的原点是那块屏自己的左上角**，不是虚拟屏幕原点。
//! 副屏的 `x` 是负数（显示器排在主屏左边时），把它带进 buffer 偏移里就会裁出空白；
//! 所以 `Shot` 只把 `rect` 当元数据、`bgra` 永远从 (0,0) 起算，选区换算统一走
//! [`css_rect_to_physical`]，它输出的是屏内相对偏移。

use serde::{Deserialize, Serialize};

/// 一块屏的物理矩形（像素；左上角是虚拟屏幕坐标系下的有符号原点）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl MonitorRect {
    /// 单轴像素上限。真实 8K 屏是 7680，留出倍数余量；再大说明传进来的矩形是垃圾数据
    /// （未初始化的 0xFFFF…，或单位写错成了 DPI）。
    const MAX_SIDE: u32 = 32_768;

    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Result<Self, String> {
        if width == 0 || height == 0 {
            return Err(format!("尺寸为 0（{width}x{height}），无法截图"));
        }
        if width > Self::MAX_SIDE || height > Self::MAX_SIDE {
            return Err(format!(
                "尺寸异常（{width}x{height}），单轴超过上限 {}",
                Self::MAX_SIDE
            ));
        }
        Ok(Self {
            x,
            y,
            width,
            height,
        })
    }

    /// 右/下边界（像素坐标，开区间）。
    pub fn right(&self) -> i64 {
        self.x as i64 + self.width as i64
    }

    pub fn bottom(&self) -> i64 {
        self.y as i64 + self.height as i64
    }

    /// 缓冲区字节数（每像素 4 字节）；乘法溢出时报错而不是 wrap。
    pub fn buffer_bytes(&self) -> Result<usize, String> {
        (self.width as usize)
            .checked_mul(self.height as usize)
            .and_then(|px| px.checked_mul(4))
            .ok_or_else(|| format!("像素数溢出（{}x{}）", self.width, self.height))
    }
}

/// 一次冻结的成品：整块屏的 BGRA 像素 + 它来自哪块屏。
///
/// 存 BGRA 而不是 RGBA，因为 GDI 交回来的就是 BGRA，而剪贴板要的 CF_DIB 也要 BGRA。
/// RGBA 只在编码 PNG 的那一刻按需生成 —— 4K 整屏换一次要 33 MB，按需换比常驻两份便宜。
#[derive(Debug, Clone)]
pub struct Shot {
    pub rect: MonitorRect,
    pub bgra: Vec<u8>,
}

impl Shot {
    pub fn new(rect: MonitorRect, bgra: Vec<u8>) -> Result<Self, String> {
        let want = rect.buffer_bytes()?;
        if bgra.len() != want {
            return Err(format!(
                "位图长度不符：{} 字节，应为 {want}（{}x{}）",
                bgra.len(),
                rect.width,
                rect.height
            ));
        }
        Ok(Self { rect, bgra })
    }

    /// 整屏转 RGBA（覆盖层出图用）。
    pub fn rgba(&self) -> Vec<u8> {
        let mut buf = self.bgra.clone();
        bgra_to_rgba(&mut buf);
        buf
    }

    /// 按**屏内相对**像素矩形裁出一块新的 `Shot`（提交选区用）。
    ///
    /// 越界是 `Err`，不是悄悄钳小：钳小会让用户拿到比框的更小的一张图而毫不知情。
    pub fn crop(&self, rel_x: u32, rel_y: u32, w: u32, h: u32) -> Result<Shot, String> {
        let sub = MonitorRect::new(rel_x as i32, rel_y as i32, w, h)?;
        if rel_x as u64 + w as u64 > self.rect.width as u64
            || rel_y as u64 + h as u64 > self.rect.height as u64
        {
            return Err(format!(
                "选区 {w}x{h} @(rel {rel_x},{rel_y}) 超出屏幕 {}x{}",
                self.rect.width, self.rect.height
            ));
        }
        let src_stride = (self.rect.width as usize) * 4;
        let dst_stride = (w as usize) * 4;
        let mut out = vec![0u8; dst_stride * (h as usize)];
        for row in 0..(h as usize) {
            let src_start = (rel_y as usize + row) * src_stride + (rel_x as usize) * 4;
            let src_end = src_start + dst_stride;
            let slice = self.bgra.get(src_start..src_end).ok_or_else(|| {
                format!(
                    "裁剪第 {row} 行越界（请求 {src_start}..{src_end}，缓冲 {} 字节）",
                    self.bgra.len()
                )
            })?;
            out[row * dst_stride..(row + 1) * dst_stride].copy_from_slice(slice);
        }
        Ok(Shot {
            rect: sub,
            bgra: out,
        })
    }
}

/// BGRA → RGBA，就地交换 R/B 并把 alpha 钉成 255。
///
/// alpha 那一步不是洁癖：GDI 交回的第四字节恒为 0，直接当 RGBA 编码会写出**全透明** PNG
/// —— 文件存在、尺寸对、粘出去是黑的，而整条链路一行错误都不报。这是本模块最难发现的一类失败。
/// 尾段不足 4 字节的部分原样留着（不该发生；真发生了这样最容易看出来，而不是悄悄错位）。
pub fn bgra_to_rgba(buf: &mut [u8]) {
    for px in buf.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
        px[3] = 255;
    }
}

/// 页面回报的选区：CSS 像素，原点是那块屏的左上角。
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct CssRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// CSS 像素选区 + DPR → 屏内相对物理像素矩形。
///
/// 三个坑都在这里处理完，别在调用方重复：
/// 1. **反向拖框**（从右下往左上按）给出负 `w`/`h`，先归一化成左上角 + 正宽高；否则负偏移
///    会绕过边界检查、裁到上一行去；
/// 2. **一律 `round()`**：125% 下 1 CSS 像素 = 1.25 物理像素，一处 floor 一处 ceil 会让裁出的
///    图比看着的位置偏 1 像素；
/// 3. **钳制顺序**是先左上角再宽高 —— 反过来会把右/下边各吃掉一像素。
///
/// 输出的 `x`/`y` 是非负的**屏内偏移**，不含 `mon.x`：副屏的负原点已由「缓冲区原点 = 那块屏
/// 自己」消化掉了，这里再加一次就裁偏。`mon` 只用来定边界。
///
/// `dpr` 由调用方负责先对账（页面的 `devicePixelRatio` vs Rust 的 `scale_factor()`，
/// 差超过 0.01 就该报错而不是取其中一个继续算）。
pub fn css_rect_to_physical(
    r: &CssRect,
    dpr: f64,
    mon: &MonitorRect,
) -> Result<MonitorRect, String> {
    if !dpr.is_finite() || dpr <= 0.0 {
        return Err(format!("缩放倍数不可用（{dpr}）"));
    }
    for v in [r.x, r.y, r.w, r.h] {
        if !v.is_finite() {
            return Err(format!("选区坐标不是有限数（{v}）"));
        }
    }
    let (left, top, w, h) = normalize(r.x, r.y, r.w, r.h);
    if w < 1.0 || h < 1.0 {
        return Err("选区不足 1 像素，什么都没框住".to_string());
    }

    let px = |v: f64| -> Result<i64, String> {
        let scaled = (v * dpr).round();
        if scaled > i64::MAX as f64 || scaled < i64::MIN as f64 {
            return Err(format!("选区坐标超出可表示范围（{v} × {dpr}）"));
        }
        Ok(scaled as i64)
    };
    let mut x = px(left)?;
    let mut y = px(top)?;
    let mut cw = px(w)?;
    let mut ch = px(h)?;

    // 框到屏外（覆盖层理论上盖满整屏，但切换显示器/DPI 变化时坐标会飘）：钳住而不是报错，
    // 因为用户看到的就是一块屏，"框到边框外面"是正常的拖拽过头。
    x = x.clamp(0, mon.width as i64 - 1);
    y = y.clamp(0, mon.height as i64 - 1);
    cw = cw.clamp(1, mon.width as i64 - x);
    ch = ch.clamp(1, mon.height as i64 - y);

    MonitorRect::new(x as i32, y as i32, cw as u32, ch as u32)
        .map_err(|e| format!("选区换算结果不可用：{e}"))
}

/// 把可能反向拖出来的矩形折成「左上角 + 正宽高」。
fn normalize(x: f64, y: f64, w: f64, h: f64) -> (f64, f64, f64, f64) {
    let left = if w < 0.0 { x + w } else { x };
    let top = if h < 0.0 { y + h } else { y };
    (left, top, w.abs(), h.abs())
}

/// 由「已格式化的时间戳 + 尺寸」拼文件名主干。时间戳由调用方给，本模块不读时钟。
pub fn name_stem(ts: &str, width: u32, height: u32) -> String {
    format!("snip_{ts}_{width}x{height}")
}

/// 同一秒内连按两次时的去重后缀：`base` 已在 `listing` 里就试 `_2`、`_3`……
///
/// 目录清单由调用方传入，本函数不碰文件系统也不碰时钟 —— 它必须能脱离真实日期和真实磁盘
/// 被断言（这仓里有过一次「用例只在某个月份成立」的账）。
pub fn unique_name(base: &str, ext: &str, listing: &[String]) -> String {
    let mut n: Option<u32> = None;
    loop {
        let name = match n {
            None => format!("{base}.{ext}"),
            Some(n) => format!("{base}_{n}.{ext}"),
        };
        if !listing.iter().any(|existing| existing == &name) {
            return name;
        }
        match n {
            None => n = Some(2),
            Some(v) if v == u32::MAX => return format!("{base}_{}.  {ext}", u32::MAX),
            Some(v) => n = Some(v + 1),
        }
    }
}

/// RGBA → PNG 字节。只用 `png` 编码，不引 `image` 那种全家桶。
pub fn encode_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let want = (width as usize)
        .checked_mul(height as usize)
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| format!("图像尺寸溢出（{width}x{height}）"))?;
    if rgba.len() != want {
        return Err(format!(
            "编码前像素长度不符：{} 字节，应为 {want}（{width}x{height}）",
            rgba.len()
        ));
    }
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("PNG 头写入失败：{e}"))?;
        writer
            .write_image_data(rgba)
            .map_err(|e| format!("PNG 数据写入失败：{e}"))?;
    }
    if out.is_empty() {
        return Err("PNG 编码返回空字节".to_string());
    }
    Ok(out)
}

#[cfg(windows)]
pub mod win {
    //! 真正摸 Win32 的部分：问出鼠标所在那块屏的矩形，把它抓进内存，以及给剪贴板备料。

    use std::cell::Cell;
    use std::sync::OnceLock;
    use std::time::Duration;

    use windows::core::w;
    use windows::Win32::Foundation::{
        GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
    };
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, GetDeviceCaps, GetMonitorInfoW, MonitorFromPoint, ReleaseDC, SelectObject,
        BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DESKTOPHORZRES, DESKTOPVERTRES, DIB_RGB_COLORS,
        HBITMAP, HDC, HGDIOBJ, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST, SRCCOPY,
    };
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::UI::HiDpi::{
        AreDpiAwarenessContextsEqual, GetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT,
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        DPI_AWARENESS_CONTEXT_SYSTEM_AWARE, DPI_AWARENESS_CONTEXT_UNAWARE,
        DPI_AWARENESS_CONTEXT_UNAWARE_GDISCALED,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, GetCursorPos, IsWindow, RegisterClassW, HWND_MESSAGE,
        WNDCLASSW,
    };

    use super::{MonitorRect, Shot};

    /// CF_DIB 的格式编号。
    ///
    /// `windows` 把这个常量放在 `Win32_System_Ole`，为一个数开整个模块不值，而
    /// `SetClipboardData` 收的就是 `u32`。值来自 WinUser.h：`#define CF_DIB 8`。
    /// 公开是为了让用例能盯同一个格式号 —— 别再在测试里抄一遍 8（这仓里栽过同一个常量写两遍的账）。
    pub const CF_DIB: u32 = 8;

    /// 鼠标所在屏的矩形。副屏 `x` 为负是正确结果，不是异常。
    pub fn monitor_at_cursor() -> Result<MonitorRect, String> {
        unsafe {
            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt).map_err(|e| format!("取不到鼠标位置：{e}"))?;
            let hmon: HMONITOR = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            if hmon.is_invalid() {
                return Err(format!("鼠标位置 ({},{}) 找不到对应显示器", pt.x, pt.y));
            }
            // MONITORINFO 没有可用的 Default（Win32 结构体靠 cbSize 自报版本），先清零。
            let mut mi: MONITORINFO = std::mem::zeroed();
            mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
                return Err("GetMonitorInfoW 未返回显示器信息".to_string());
            }
            let rc: &RECT = &mi.rcMonitor;
            MonitorRect::new(
                rc.left,
                rc.top,
                (rc.right - rc.left).max(0) as u32,
                (rc.bottom - rc.top).max(0) as u32,
            )
        }
    }

    /// 整个虚拟屏幕的并集矩形，用来做「各屏是否落在同一片坐标系里」的自洽检查。
    pub fn virtual_screen() -> MonitorRect {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
            SM_YVIRTUALSCREEN,
        };
        unsafe {
            let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let width = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(0) as u32;
            let height = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(0) as u32;
            // 取不到桌面时（无会话/服务态）这些会是 0，调用方按 Err 处理，不返回一个假矩形。
            MonitorRect::new(x, y, width, height).unwrap_or(MonitorRect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            })
        }
    }

    /// 本机主显的**物理**分辨率（GDI 视角，本进程 DPI 感知状态下看到的那一份）。
    ///
    /// 与 [`monitor_at_cursor`] 对账用：两者不一致就说明进程 DPI 感知不是 Per-Monitor V2，
    /// 那时截出来的图尺寸失真，必须停下来而不是出一张"看着正常但偏小"的图。
    pub fn desktop_caps_size() -> (i32, i32) {
        unsafe {
            let hdc = GetDC(None);
            let w = GetDeviceCaps(Some(hdc), DESKTOPHORZRES);
            let h = GetDeviceCaps(Some(hdc), DESKTOPVERTRES);
            ReleaseDC(None, hdc);
            (w, h)
        }
    }

    /// 本线程（= 进程默认）的 DPI 感知状态，一句给人读的话。
    ///
    /// 为什么不是 `GetWindowDpiAwarenessContext(GetDesktopWindow())`：那问的是**桌面窗口
    /// 所在线程**的上下文，跟本进程无关。真机上正是这个错判把一个已经是 Per-Monitor V2
    /// 的进程报成"未启用"，而我把它当硬失败用了 —— 于是一次截图都没成。
    /// `GetThreadDpiAwarenessContext` 才是问自己：新建的线程继承进程默认值，
    /// tao 在事件循环创建时设的那次 `SetProcessDpiAwarenessContext` 就在这里体现。
    pub fn dpi_awareness_text() -> String {
        unsafe {
            let ctx = GetThreadDpiAwarenessContext();
            let eq = |c: DPI_AWARENESS_CONTEXT| AreDpiAwarenessContextsEqual(ctx, c).as_bool();
            if eq(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) {
                "per-monitor-v2".to_string()
            } else if eq(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE) {
                "per-monitor".to_string()
            } else if eq(DPI_AWARENESS_CONTEXT_SYSTEM_AWARE) {
                "system".to_string()
            } else if eq(DPI_AWARENESS_CONTEXT_UNAWARE) {
                "unaware".to_string()
            } else if eq(DPI_AWARENESS_CONTEXT_UNAWARE_GDISCALED) {
                "unaware-gdiscaled".to_string()
            } else {
                // 走到这里说明系统回了一个我们不认识的上下文：说出来，不要折成上面任何一种
                format!("未知(0x{:x})", ctx.0 as isize)
            }
        }
    }

    /// 是否处于 Per-Monitor V2 感知。**只用来决定日志等级**，不用来否决截图，理由写在
    /// `desktop/src/snip.rs` 的调用处。
    pub fn is_per_monitor_v2() -> bool {
        dpi_awareness_text() == "per-monitor-v2"
    }

    /// 抓取 `rect` 那块区域，返回**屏内相对**、原点在 (0,0) 的 BGRA 缓冲。
    ///
    /// `biHeight` 取**负值**让 `GetDIBits` 自上而下填行 —— 那正是 PNG 和裁剪代码要的朝向；
    /// 写成正值会得到上下颠倒的图，而且它"看起来仍是张正常的图"，只有粘出去才发觉。
    ///
    /// 32bpp 下每行 `width*4` 字节，天然 4 字节对齐，所以不需要 DIB 的行填充逻辑。
    pub fn grab(rect: &MonitorRect) -> Result<Vec<u8>, String> {
        let w = rect.width as i32;
        let h = rect.height as i32;
        let mut buf: Vec<u8> = vec![0u8; rect.buffer_bytes()?];
        unsafe {
            let hdc_screen: HDC = GetDC(None);
            if hdc_screen.is_invalid() {
                return Err("GetDC(屏幕) 失败：拿不到屏幕设备上下文".to_string());
            }
            let hdc_mem: HDC = CreateCompatibleDC(Some(hdc_screen));
            if hdc_mem.is_invalid() {
                ReleaseDC(None, hdc_screen);
                return Err("CreateCompatibleDC 失败".to_string());
            }
            let hbitmap: HBITMAP = CreateCompatibleBitmap(hdc_screen, w, h);
            if hbitmap.is_invalid() {
                let _ = DeleteDC(hdc_mem);
                ReleaseDC(None, hdc_screen);
                return Err(format!("CreateCompatibleBitmap({w}x{h}) 失败：资源不足"));
            }
            let old: HGDIOBJ = SelectObject(hdc_mem, hbitmap.into());
            if old.is_invalid() {
                let _ = DeleteObject(hbitmap.into());
                let _ = DeleteDC(hdc_mem);
                ReleaseDC(None, hdc_screen);
                return Err("SelectObject 未能挂上位图".to_string());
            }

            // BitBlt 的源坐标是**虚拟屏幕**坐标，所以副屏的负 x/y 要原样带上，
            // 不能像对缓冲区那样规整成非负 —— 那是两件事。
            let blt = BitBlt(
                hdc_mem,
                0,
                0,
                w,
                h,
                Some(hdc_screen),
                rect.x,
                rect.y,
                SRCCOPY,
            );

            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader = dib_header(rect.width, rect.height);
            let lines = if blt.is_ok() {
                GetDIBits(
                    hdc_mem,
                    hbitmap,
                    0,
                    h as u32,
                    Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
                    &mut bmi,
                    DIB_RGB_COLORS,
                )
            } else {
                0
            };

            SelectObject(hdc_mem, old);
            let _ = DeleteObject(hbitmap.into());
            let _ = DeleteDC(hdc_mem);
            ReleaseDC(None, hdc_screen);

            // 全黑（DRM protected 内容）是 BitBlt 成功且 lines==h 的正常结果，不报错。
            // 这里只拦"根本没拿到行"，那时 buf 还是全零，绝不能带着它继续往下走。
            if let Err(e) = blt {
                return Err(format!("屏幕截取失败（BitBlt）：{e}"));
            }
            if lines != h {
                return Err(format!("屏幕取回 {lines} 行，应为 {h} 行（GetDIBits）"));
            }
        }
        Ok(buf)
    }

    /// 组合：截鼠标所在屏，产出一张 [`Shot`]。上层一般只要调这个。
    pub fn capture_at_cursor() -> Result<Shot, String> {
        let rect = monitor_at_cursor()?;
        let bgra = grab(&rect)?;
        Shot::new(rect, bgra)
    }

    /// 32bpp 自上而下的 DIB 头。`GetDIBits` 与剪贴板的 CF_DIB **共用这一份定义**，
    /// 免得两处各写一遍、改一漏一（这仓里栽过同一个常量写两遍的账）。
    pub fn dib_header(width: u32, height: u32) -> BITMAPINFOHEADER {
        BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            // 负高度 = 自上而下；GDI 与剪贴板两侧都按这个约定读。
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        }
    }

    /// CF_DIB 负载 = DIB 头的原始内存 + BGRA 行。
    ///
    /// `BITMAPINFOHEADER` 是 `#[repr(C)]` 且无内部填充，所以它的字节布局**就是** DIB 头，
    /// 直接按内存拷出来，不再手写一遍 40 字节的字段顺序。
    pub fn dib_bytes(header: &BITMAPINFOHEADER, bgra: &[u8]) -> Vec<u8> {
        let head = unsafe {
            std::slice::from_raw_parts(
                header as *const BITMAPINFOHEADER as *const u8,
                std::mem::size_of::<BITMAPINFOHEADER>(),
            )
        };
        let mut out = Vec::with_capacity(head.len() + bgra.len());
        out.extend_from_slice(head);
        out.extend_from_slice(bgra);
        out
    }

    // 剪贴板所有者窗口（**每线程一个**，数值缓存；0 = 本线程还没建过）。
    thread_local! {
        static CLIP_OWNER: Cell<isize> = const { Cell::new(0) };
    }

    // 窗口类只在进程内注册一次：类名是进程级命名空间，多线程各自注册会撞
    // `ERROR_CLASS_ALREADY_EXISTS`，而那看起来像失败。
    static CLIP_CLASS: OnceLock<Result<(), String>> = OnceLock::new();

    /// 所有者窗口只做消息转发：我们从不给它发消息，类能挂上即可。
    /// （`WNDPROC` 要的是 `unsafe extern "system" fn`，不能直接把 `DefWindowProcW` 塞进
    /// `Some(..)` —— 签名里少了 ABI 前缀就换不成那个函数指针类型。）
    unsafe extern "system" fn clip_wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    fn clip_class_registered() -> Result<(), String> {
        CLIP_CLASS
            .get_or_init(|| unsafe {
                let Ok(hinstance) = GetModuleHandleW(None) else {
                    return Err(format!(
                        "GetModuleHandleW 失败：{}",
                        windows::core::Error::from_win32()
                    ));
                };
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(clip_wnd_proc),
                    hInstance: hinstance.into(),
                    lpszClassName: w!("FocusFlowSnipClip"),
                    ..Default::default()
                };
                if RegisterClassW(&wc) == 0 {
                    Err(format!(
                        "窗口类注册失败：{}",
                        windows::core::Error::from_win32()
                    ))
                } else {
                    Ok(())
                }
            })
            .clone()
    }

    /// 拿到本线程的剪贴板所有者窗口，必要时新建。
    ///
    /// 为什么要自己造一个窗口：`OpenClipboard(NULL)` 打开的剪贴板**没有关联窗口**，
    /// 紧随其后的 `EmptyClipboard` 按 Win32 规矩会失败；而剪贴板所有权又记在
    /// **调用线程**的窗口上。core 这层没有窗口（窗口都在 desktop 的 Tauri 侧），
    /// 所以照 `device_stats.rs` 的先例造一个 message-only 窗口专门当所有者，
    /// 于是剪贴板写入可以在任意线程完成（上层整块丢进 `spawn_blocking` 即可，不必回主线程）。
    ///
    /// 这个窗口不泵消息：我们不需要 `WM_DESTROYCLIPBOARD` 那类通知，系统只是把消息排进
    /// 本线程队列。代价是每用过一次剪贴板的线程留下一个隐藏窗口 —— `spawn_blocking`
    /// 的线程池是有界且复用的，所以数量不随截图次数增长。
    ///
    /// 公开它有两个用处：上层可以在启动时先把窗口备好；用例也能在不碰剪贴板内容的前提下
    /// 验一遍「类注册 + 建窗」这条路（所有写入用例都在参数校验处提前返回，覆盖不到它）。
    pub fn clip_owner_hwnd() -> Result<HWND, String> {
        let cached = CLIP_OWNER.get();
        if cached != 0 {
            let hwnd = HWND(cached as *mut core::ffi::c_void);
            // 线程被回收后窗口可能已销毁，句柄数值会被复用 —— 每次先验它还活着。
            if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
                return Ok(hwnd);
            }
            CLIP_OWNER.set(0);
        }
        clip_class_registered().map_err(|e| format!("剪贴板窗口类不可用：{e}"))?;
        let hinstance =
            unsafe { GetModuleHandleW(None) }.map_err(|e| format!("GetModuleHandleW 失败：{e}"))?;
        let hwnd = unsafe {
            CreateWindowExW(
                Default::default(),
                w!("FocusFlowSnipClip"),
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
            )
        }
        .map_err(|e| format!("创建剪贴板所有者窗口失败：{e}"))?;
        CLIP_OWNER.set(hwnd.0 as isize);
        Ok(hwnd)
    }

    /// 校验输入并拼出 CF_DIB 负载。**不碰剪贴板**，所以尺寸不符时在清空用户剪贴板之前就失败。
    pub fn dib_payload(bgra: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
        if width == 0 || height == 0 {
            return Err(format!("剪贴板图像尺寸为 0（{width}x{height}）"));
        }
        let bytes = (width as usize)
            .checked_mul(height as usize)
            .and_then(|px| px.checked_mul(4))
            .ok_or_else(|| format!("剪贴板图像尺寸溢出（{width}x{height}）"))?;
        if bgra.len() != bytes {
            return Err(format!(
                "剪贴板图像长度不符：{} 字节，应为 {bytes}（{width}x{height}）",
                bgra.len()
            ));
        }
        Ok(dib_bytes(&dib_header(width, height), bgra))
    }

    /// 把 BGRA 图作为 CF_DIB 放进剪贴板。
    ///
    /// 线程亲和：`Open → Empty → Set → Close` 必须整段在同一个线程里跑完，所以这里不拆成
    /// 三个公开函数；调用方把整个调用放进一个 `spawn_blocking` 闭包就是对的。
    pub fn write_dib_to_clipboard(bgra: &[u8], width: u32, height: u32) -> Result<(), String> {
        // 先备料再开窗：校验失败时必须原样留下用户剪贴板里的东西。
        let payload = dib_payload(bgra, width, height)?;

        let owner = clip_owner_hwnd()?;
        let mut last = String::from("未尝试");
        for attempt in 1..=5u32 {
            match unsafe { OpenClipboard(Some(owner)) } {
                Ok(()) => {
                    let result = unsafe { write_while_open(&payload) };
                    // 无论成败都要关：不关的话其它程序从此读不到剪贴板，症状比本次失败严重得多。
                    if let Err(e) = unsafe { CloseClipboard() } {
                        tracing::warn!("关闭剪贴板失败（本次结果仍按 {result:?} 上报）：{e}");
                    }
                    return result;
                }
                Err(e) => last = format!("{e}"),
            }
            if attempt < 5 {
                // 别的程序正开着剪贴板（浏览器、输入法、远端桌面都常见）：短促重试而不是立刻认输。
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        Err(format!("剪贴板被其它程序占用，重试 5 次后放弃：{last}"))
    }

    /// 已持有剪贴板时的写入段。调用方负责 `CloseClipboard`。
    unsafe fn write_while_open(payload: &[u8]) -> Result<(), String> {
        EmptyClipboard().map_err(|e| format!("清空剪贴板失败：{e}"))?;
        let hg: HGLOBAL = match GlobalAlloc(GMEM_MOVEABLE, payload.len()) {
            Ok(h) => h,
            Err(e) => return Err(format!("分配剪贴板内存失败（{} 字节）：{e}", payload.len())),
        };
        let dst = GlobalLock(hg);
        if dst.is_null() {
            let _ = GlobalFree(Some(hg));
            return Err(format!("GlobalLock 返回空指针（{} 字节）", payload.len()));
        }
        std::ptr::copy_nonoverlapping(payload.as_ptr(), dst as *mut u8, payload.len());
        if let Err(e) = GlobalUnlock(hg) {
            let _ = GlobalFree(Some(hg));
            return Err(format!("GlobalUnlock 失败：{e}"));
        }
        // 从这一行起所有权交给系统：之后再 GlobalFree 就是双释放，这是这一族 bug 的固定结局。
        if let Err(e) = SetClipboardData(CF_DIB, Some(HANDLE(hg.0))) {
            let _ = GlobalFree(Some(hg));
            return Err(format!("写入剪贴板失败：{e}"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(w: u32, h: u32) -> MonitorRect {
        MonitorRect::new(0, 0, w, h).unwrap()
    }

    #[test]
    fn monitor_rect_rejects_zero_and_absurd_sizes() {
        assert!(MonitorRect::new(0, 0, 0, 1080).is_err());
        assert!(MonitorRect::new(0, 0, 1920, 0).is_err());
        assert!(MonitorRect::new(0, 0, 1_000_000, 100).is_err());
        assert_eq!(MonitorRect::new(0, 0, 32_768, 100).unwrap().width, 32_768);
    }

    #[test]
    fn buffer_bytes_multiplies_by_four_and_negative_origin_is_kept() {
        let m = MonitorRect::new(-1920, 0, 1920, 1080).unwrap();
        assert_eq!(m.buffer_bytes().unwrap(), 1920 * 1080 * 4);
        assert_eq!(m.right(), 0);
        assert_eq!(m.x, -1920);
    }

    #[test]
    fn shot_rejects_short_buffer() {
        let m = mon(4, 2);
        assert!(Shot::new(m, vec![0u8; 4 * 4 * 2 - 1]).is_err());
        assert!(Shot::new(m, vec![0u8; 4 * 4 * 2]).is_ok());
    }

    /// GDI 交回的 alpha 恒为 0。不钉成 255 就会写出全透明 PNG —— 尺寸正常、无错误、
    /// 粘出去是黑的，这是本模块最难发现的一类失败。
    #[test]
    fn bgra_to_rgba_forces_opaque_alpha() {
        let mut buf = vec![0u8, 10, 20, 0, 255, 128, 1, 0];
        bgra_to_rgba(&mut buf);
        assert_eq!(buf, vec![20u8, 10, 0, 255, 1, 128, 255, 255]);
        assert!(buf.iter().skip(3).step_by(4).all(|&a| a == 255));
    }

    #[test]
    fn bgra_to_rgba_leaves_ragged_tail_untouched() {
        let mut buf = vec![1u8, 2, 3, 4, 5];
        bgra_to_rgba(&mut buf);
        assert_eq!(buf, vec![3u8, 2, 1, 255, 5]);
    }

    #[test]
    fn crop_extracts_the_right_pixels_by_row() {
        // 2x2 屏，每像素一个可辨认的标记值：左上 0、右上 1、左下 2、右下 3
        let px = |v: u8| vec![v, v, v, 0];
        let mut bgra = Vec::new();
        bgra.extend(px(0));
        bgra.extend(px(1));
        bgra.extend(px(2));
        bgra.extend(px(3));
        let shot = Shot::new(mon(2, 2), bgra).unwrap();

        let cropped = shot.crop(1, 1, 1, 1).unwrap();
        assert_eq!(cropped.bgra, px(3));
        assert_eq!((cropped.rect.width, cropped.rect.height), (1, 1));

        // 全屏覆盖必须逐字节相同
        let whole = shot.crop(0, 0, 2, 2).unwrap();
        assert_eq!(whole.bgra, shot.bgra);
    }

    #[test]
    fn crop_refuses_out_of_range_instead_of_clamping() {
        let shot = Shot::new(mon(2, 2), vec![0u8; 2 * 2 * 4]).unwrap();
        assert!(shot.crop(1, 1, 2, 2).is_err());
        assert!(shot.crop(0, 0, 3, 1).is_err());
    }

    #[test]
    fn crop_is_row_accurate_on_a_negative_origin_monitor() {
        // 副屏原点 x=-1920 不该影响裁剪：缓冲区原点始终是那块屏自己
        let rect = MonitorRect::new(-1920, 0, 4, 2).unwrap();
        let mut bgra = vec![0u8; 4 * 2 * 4];
        for (i, chunk) in bgra.chunks_mut(4).enumerate() {
            chunk[0] = i as u8;
        }
        let shot = Shot::new(rect, bgra).unwrap();
        let c = shot.crop(2, 1, 2, 1).unwrap();
        assert_eq!(c.bgra[0], 6);
        assert_eq!(c.bgra[4], 7);
    }

    #[test]
    fn css_rect_to_physical_rounds_at_every_common_scale() {
        let m = mon(1920, 1080);
        let r = |x, y, w, h| CssRect { x, y, w, h };
        for (dpr, expect) in [(1.0f64, 100i32), (1.25, 125), (1.5, 150), (1.75, 175)] {
            let got = css_rect_to_physical(&r(0.0, 0.0, 100.0, 50.0), dpr, &m).unwrap();
            assert_eq!(got.width as i32, expect, "dpr={dpr}");
        }
        // 0.6 × 1.25 = 0.75 → round 成 1（不是截断成 0）
        let got = css_rect_to_physical(&r(0.6, 0.6, 1.0, 1.0), 1.25, &m).unwrap();
        assert_eq!(
            (got.x, got.y, got.width, got.height),
            (1, 1, 1, 1),
            "1px 选区在 125% 下不能算成 0"
        );
    }

    #[test]
    fn css_rect_to_physical_normalizes_drag_from_bottom_right() {
        let m = mon(1920, 1080);
        let forward = css_rect_to_physical(
            &CssRect {
                x: 100.0,
                y: 200.0,
                w: 50.0,
                h: 40.0,
            },
            1.0,
            &m,
        )
        .unwrap();
        let backward = css_rect_to_physical(
            &CssRect {
                x: 150.0,
                y: 240.0,
                w: -50.0,
                h: -40.0,
            },
            1.0,
            &m,
        )
        .unwrap();
        assert_eq!(forward, backward);
    }

    #[test]
    fn css_rect_to_physical_clamps_without_losing_the_last_pixel() {
        let m = mon(100, 100);
        // 右与下各超出 50
        let got = css_rect_to_physical(
            &CssRect {
                x: 60.0,
                y: 60.0,
                w: 90.0,
                h: 90.0,
            },
            1.0,
            &m,
        )
        .unwrap();
        assert_eq!((got.x, got.y), (60, 60));
        assert_eq!(
            (got.width, got.height),
            (40, 40),
            "钳到边界时应保住 100-60 而不是少一像素"
        );
        // 完全在屏外：给出合法的 1px，别 Err 让覆盖层白闪一下
        let off = css_rect_to_physical(
            &CssRect {
                x: 500.0,
                y: 500.0,
                w: 10.0,
                h: 10.0,
            },
            1.0,
            &m,
        )
        .unwrap();
        assert_eq!((off.width, off.height), (1, 1));
        assert!(off.x < 100 && off.y < 100);
    }

    #[test]
    fn css_rect_to_physical_rejects_bogus_dpr_and_nan() {
        let m = mon(100, 100);
        for dpr in [0.0f64, -1.0, f64::NAN, f64::INFINITY] {
            assert!(
                css_rect_to_physical(
                    &CssRect {
                        x: 0.0,
                        y: 0.0,
                        w: 10.0,
                        h: 10.0
                    },
                    dpr,
                    &m
                )
                .is_err(),
                "dpr={dpr} 不该被接受"
            );
        }
        assert!(css_rect_to_physical(
            &CssRect {
                x: f64::NAN,
                y: 0.0,
                w: 10.0,
                h: 10.0
            },
            1.0,
            &m
        )
        .is_err());
        assert!(css_rect_to_physical(
            &CssRect {
                x: 0.0,
                y: 0.0,
                w: 0.5,
                h: 10.0
            },
            1.0,
            &m
        )
        .is_err());
    }

    #[test]
    fn unique_name_appends_counter_from_the_supplied_listing() {
        let listing = vec![
            "snip_a.png".to_string(),
            "snip_a_2.png".to_string(),
            "unrelated.txt".to_string(),
        ];
        assert_eq!(unique_name("snip_b", "png", &listing), "snip_b.png");
        assert_eq!(unique_name("snip_a", "png", &listing), "snip_a_3.png");
        assert_eq!(unique_name("snip_a", "png", &[]), "snip_a.png");
    }

    #[test]
    fn name_stem_carries_timestamp_and_size() {
        assert_eq!(
            name_stem("20260928_141530", 800, 600),
            "snip_20260928_141530_800x600"
        );
    }

    #[test]
    fn encode_png_roundtrips_dimensions_and_keeps_alpha() {
        let mut rgba = vec![0u8; 4 * 3 * 2];
        for px in rgba.as_chunks_mut::<4>().0 {
            px[0] = 7;
            px[3] = 255;
        }
        let bytes = encode_png(&rgba, 3, 2).unwrap();
        assert_eq!(&bytes[1..4], b"PNG");

        let decoder = png::Decoder::new(std::io::Cursor::new(&bytes));
        let mut reader = decoder.read_info().unwrap();
        assert_eq!((reader.info().width, reader.info().height), (3, 2));
        assert_eq!(reader.info().bit_depth, png::BitDepth::Eight);
        let first_row = &rgba[..3 * 4];
        for _ in 0..2 {
            let got = reader.next_row().unwrap().expect("还有行");
            assert_eq!(got.data(), first_row);
        }
        assert!(reader.next_row().unwrap().is_none());
    }

    /// 透明像素必须真的透明：哪天 alpha 被无条件钉成 255（比如在 encode 之前又跑了一遍
    /// bgra_to_rgba），这条会红 —— 而"全透明的截图文件"正是本模块最贵的那种失败。
    #[test]
    fn encode_png_does_not_invent_alpha() {
        let rgba = vec![1u8, 2, 3, 0];
        let bytes = encode_png(&rgba, 1, 1).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(&bytes));
        let mut reader = decoder.read_info().unwrap();
        let mut row = vec![0u8; 4];
        let got = reader.next_row().unwrap().expect("有 1 行");
        row.copy_from_slice(got.data());
        assert_eq!(row, vec![1, 2, 3, 0]);
    }

    #[test]
    fn encode_png_rejects_mismatched_buffer() {
        assert!(encode_png(&[0u8; 10], 3, 2).is_err());
        assert!(encode_png(&[0u8; 24], 1, 1).is_err());
    }

    #[cfg(windows)]
    mod on_windows {
        use std::time::Duration;

        use super::super::win;
        use windows::Win32::Graphics::Gdi::BI_RGB;

        /// 真机测量：抓一次鼠标所在屏，核对三件事是否落在同一个坐标系里 ——
        /// 显示器矩形、像素缓冲长度、虚拟屏幕并集。
        ///
        /// 没有可达桌面时（服务态/无会话）打印一行后跳过，不静默：这条用例覆盖的正是
        /// 纯函数测不到的那一段。运行方式 `cargo test -p focusflow-core --lib capture -- --nocapture`。
        #[test]
        fn grabbed_pixels_match_the_monitor_rect_one_to_one() {
            let report = format!(
                "DPI={} 主显物理={}x{} 虚拟屏幕={:?}",
                win::dpi_awareness_text(),
                win::desktop_caps_size().0,
                win::desktop_caps_size().1,
                win::virtual_screen()
            );
            let rect = match win::monitor_at_cursor() {
                Ok(r) => r,
                Err(e) => {
                    println!("跳过真实截取：{e}｜{report}");
                    return;
                }
            };
            let shot = win::grab(&rect).expect("真实截取失败");
            assert_eq!(
                shot.len(),
                rect.buffer_bytes().unwrap(),
                "BitBlt 行数与显示器矩形不符：{report}"
            );

            let vs = win::virtual_screen();
            if vs.width > 0 {
                assert!(
                    rect.x as i64 >= vs.x as i64
                        && rect.y as i64 >= vs.y as i64
                        && rect.right() <= vs.right()
                        && rect.bottom() <= vs.bottom(),
                    "显示器矩形跑出了虚拟屏幕并集：rect={rect:?} vs={vs:?}"
                );
                assert!(
                    (rect.width as u64 * rect.height as u64)
                        <= (vs.width as u64 * vs.height as u64),
                    "单屏面积大于虚拟屏幕：rect={rect:?} vs={vs:?}"
                );
            }
            println!("实测：显示器 {rect:?} → {} 字节｜{report}", shot.len());
        }

        /// DPI 探针要问的是**自己**，不是别的进程的窗口。
        ///
        /// 上一版用 `GetWindowDpiAwarenessContext(GetDesktopWindow())`，问的是桌面窗口
        /// 所属线程 → 一个 PMv2 的进程被报成"未启用"，而上层把它当硬失败，六次截图六次被挡。
        /// 这条用例钉三件事：探针不依赖窗口存在（本测试进程就没建过窗口）、回的是认得的
        /// 状态名、`is_per_monitor_v2` 与该字符串同源（不许两把尺子）。
        #[test]
        fn dpi_probe_asks_our_own_thread_not_some_window() {
            let text = win::dpi_awareness_text();
            let known = [
                "per-monitor-v2",
                "per-monitor",
                "system",
                "unaware",
                "unaware-gdiscaled",
            ];
            assert!(
                known.iter().any(|k| text == *k) || text.starts_with("未知("),
                "探针回了一个不成话的状态：{text:?}"
            );
            assert_eq!(
                win::is_per_monitor_v2(),
                text == "per-monitor-v2",
                "布尔值必须与状态名同源"
            );
            println!("本测试进程的 DPI 感知 = {text}");
        }

        #[test]
        fn dib_header_is_top_down_32bpp_with_correct_size() {
            let h = win::dib_header(800, 600);
            assert_eq!(h.biSize, 40);
            assert_eq!(h.biWidth, 800);
            assert_eq!(h.biHeight, -600);
            assert_eq!(h.biBitCount, 32);
            assert_eq!(h.biPlanes, 1);
            assert_eq!(h.biCompression, BI_RGB.0);
        }

        #[test]
        fn dib_bytes_are_header_then_raw_bgra() {
            let h = win::dib_header(1, 1);
            let bgra = vec![9u8, 8, 7, 0];
            let bytes = win::dib_bytes(&h, &bgra);
            assert_eq!(bytes.len(), 40 + 4);
            // 头的前 4 字节就是 biSize，且是小端
            assert_eq!(&bytes[..4], &40u32.to_le_bytes()[..]);
            assert_eq!(&bytes[4..8], &1i32.to_le_bytes()[..]); // biWidth
            assert_eq!(&bytes[8..12], &(-1i32).to_le_bytes()[..]); // biHeight 自上而下
            assert_eq!(&bytes[40..], &bgra[..]);
        }

        #[test]
        fn dib_payload_is_header_plus_pixels_for_the_given_size() {
            let bytes = win::dib_payload(&[0u8; 4 * 2 * 2], 2, 2).unwrap();
            assert_eq!(bytes.len(), 40 + 16);
        }

        #[test]
        fn dib_payload_rejects_inputs_that_do_not_match_their_own_size() {
            for (buf, w, h) in [
                (&[0u8; 3][..], 1u32, 1u32),
                (&[][..], 0u32, 0u32),
                (&[0u8; 8][..], 2u32, 2u32),
            ] {
                let err = win::dib_payload(buf, w, h).unwrap_err();
                assert!(
                    err.contains("剪贴板"),
                    "错误要指得出是哪一层拒的，实际：{err}"
                );
            }
        }

        /// 只读地问一句"剪贴板里现在有没有 CF_DIB"。读不出来给 Err，**不折成"没有"** ——
        /// 这条 helper 存在的意义就是给下面那条用例一个真的观察量。
        fn clip_has_dib() -> Result<bool, String> {
            use windows::Win32::System::DataExchange::{
                CloseClipboard, IsClipboardFormatAvailable, OpenClipboard,
            };
            let mut last = String::from("未尝试");
            for attempt in 1..=10u32 {
                match unsafe { OpenClipboard(None) } {
                    Ok(()) => {
                        let has = unsafe { IsClipboardFormatAvailable(win::CF_DIB) }.is_ok();
                        if let Err(e) = unsafe { CloseClipboard() } {
                            return Err(format!("读完了却关不上剪贴板：{e}"));
                        }
                        return Ok(has);
                    }
                    Err(e) => last = format!("{e}"),
                }
                if attempt < 10 {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            Err(format!("重试 10 次仍读不到剪贴板：{last}"))
        }

        /// 坏输入一定在碰剪贴板之前被挡掉：用户原有的剪贴板内容不能因为一次错误调用而消失。
        ///
        /// 观察量是"CF_DIB 的有无在调用前后不变"。如果把校验挪到 `OpenClipboard` 之后
        /// （旧写法很容易犯的次序错），这条会当场红：坏负载也会被塞进剪贴板。
        #[test]
        fn bad_input_never_touches_the_clipboard() {
            let before = clip_has_dib()
                .expect("这台机器上读不到剪贴板 —— 这条用例就没有观察量了，别让它假装通过");
            for (buf, w, h) in [
                (&[0u8; 3][..], 1u32, 1u32),
                (&[][..], 0u32, 0u32),
                (&[0u8; 8][..], 2u32, 2u32),
            ] {
                assert!(win::write_dib_to_clipboard(buf, w, h).is_err());
            }
            assert_eq!(
                clip_has_dib().expect("调用后又读不到剪贴板了"),
                before,
                "校验失败却动了用户的剪贴板"
            );
        }

        /// 剪贴板所有者窗口建得起来，且同一线程第二次调用走缓存拿到同一个窗口。
        ///
        /// 建窗与类注册不被任何写入用例覆盖（它们都在参数校验处提前返回了），而这条一旦坏掉，
        /// 症状是"截图存了盘却什么都粘不出去" —— 所以单独盯它。本用例只建窗，不动剪贴板内容。
        #[test]
        fn clip_owner_window_is_created_once_per_thread() {
            let hwnd = win::clip_owner_hwnd().expect("剪贴板所有者窗口建不起来");
            assert!(
                unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd)) }.as_bool(),
                "建出来的句柄不活着：{hwnd:?}"
            );
            let again = win::clip_owner_hwnd().expect("第二次调用建不起来");
            assert_eq!(hwnd.0, again.0, "thread_local 缓存没生效");
        }
    }
}
