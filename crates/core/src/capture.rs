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
            // 这一支要到 `u32::MAX` 才走得到（同名的先例文件得凑满 42 亿个），用例造不出来，
            // 所以只能就地写对：名字必须仍是 `.<ext>` 结尾 —— 写成空格就成一个没有扩展名的
            // 文件，截图功能唯一的出口那次会落出一张谁都不认得的图。
            Some(v) if v == u32::MAX => return format!("{base}_{}.{ext}", u32::MAX),
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

/// 标注层解码的内存上限（字节）。`png` 的默认值是 64 MiB，而一层覆盖 8K 屏的 RGBA
/// 就要 132 MB —— 标注层最大不过一块屏，所以放到 256 MiB；再大就不是标注层而是攻击面了。
const MAX_LAYER_BYTES: usize = 256 * 1024 * 1024;

/// 诊断消息里带的头字节个数：够认出签名对不对，又不会把整张图喷进日志。
const HEAD_DUMP: usize = 8;

/// PNG 字节 → RGBA8 像素，返回 `(像素, 宽, 高)`。
///
/// 存在的唯一理由：标注层是页面 `canvas.toDataURL('image/png')` 交回来的那一层透明像素，
/// Rust 要把它 1:1 合成到裁剪后的 BGRA 上。**只接 8 位 RGBA，其他色型直接报错、不做转换**：
/// 灰度/调色板/16 位都要先解释一遍才能落进缓冲，而"页面所见 = 落盘像素"正是这条链路唯一
/// 要保证的事 —— 宁可不画也不猜。页面自己产的那张 PNG 必然是 RGBA8，所以正常路径不受影响。
///
/// 不引新依赖：解码走 `encode_png` 用的那个 `png` crate（它自带解码，不是 image 全家桶）。
pub fn decode_png_rgba(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_limits(png::Limits {
        bytes: MAX_LAYER_BYTES,
    });
    let mut reader = decoder.read_info().map_err(|e| {
        format!(
            "PNG 头读不出来（前 {} 字节 {}）：{e}",
            HEAD_DUMP,
            head_hex(bytes)
        )
    })?;
    let info = reader.info();
    let (w, h) = (info.width, info.height);
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err(format!(
            "标注层只支持 8 位 RGBA PNG，这份是 {:?} / {} 位",
            info.color_type, info.bit_depth as u8
        ));
    }
    if w == 0 || h == 0 || w > MonitorRect::MAX_SIDE || h > MonitorRect::MAX_SIDE {
        return Err(format!(
            "PNG 尺寸不可用（{w}x{h}，单轴上限 {}）",
            MonitorRect::MAX_SIDE
        ));
    }
    let want = (w as usize)
        .checked_mul(h as usize)
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| format!("PNG 像素数溢出（{w}x{h}）"))?;
    let mut buf = vec![0u8; want];
    let out = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("PNG 数据解码失败（{w}x{h}）：{e}"))?;
    if out.buffer_size() != want {
        // 解码器算出来的行数和我们要的不是一回事：宁可整层不画，也不返回半张图。
        return Err(format!(
            "PNG 解码缓冲不符：解码器给了 {} 字节，按 {w}x{h} RGBA 应是 {want}",
            out.buffer_size()
        ));
    }
    Ok((buf, w, h))
}

/// 取前 `HEAD_DUMP` 个字节的十六进制。
fn head_hex(bytes: &[u8]) -> String {
    let n = bytes.len().min(HEAD_DUMP);
    let mut s = String::with_capacity(n * 3);
    for b in &bytes[..n] {
        s.push_str(&format!("{b:02x} "));
    }
    if bytes.len() > n {
        s.push('…');
    }
    s.trim_end().to_string()
}

/// 把一层 RGBA 标注**就地**合成到 BGRA 缓冲上。
///
/// `dst` 是裁剪出来的那块像素（[`Shot::crop`] 给的 BGRA，剪贴板 CF_DIB 用的就是它本体），
/// `layer` 是页面回传的标注层（RGBA），两者都必须严格等于 `width × height × 4` 字节 ——
/// 尺寸不符是 `Err`，绝不缩放：缩放会把"页面所见 = 落盘像素"变成一次重采样。
///
/// 三件事是刻意的，动之前先读：
/// 1. **R/B 显式换序**：layer 是 RGBA、dst 是 BGRA，红与蓝落在 0 和 2 两个下标上正好相反。
///    写反了的图只是"颜色差一点"，谁也都说不出哪里不对，所以用例
///    `composite_over_puts_rgba_red_onto_bgra_blue` 专门断颜色（只断 alpha 三档照不出来）。
/// 2. **dst 的 alpha 字节一个都不碰**：GDI 交回的第四字节恒为 0，[`bgra_to_rgba`] 在编码 PNG
///    那一刻才钉成 255。在这里写 alpha 会顺手改掉 CF_DIB 那份的字节。
/// 3. **alpha=0 整像素跳过**：这就是"未被标注覆盖的像素逐字节相等"那条红线的实现方式，
///    也比算一遍混合再取回原值更省 —— 且不给四舍五入留机会。
pub fn composite_over(dst: &mut [u8], layer: &[u8], width: u32, height: u32) -> Result<(), String> {
    let want = (width as usize)
        .checked_mul(height as usize)
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| format!("图像尺寸溢出（{width}x{height}）"))?;
    if dst.len() != want {
        return Err(format!(
            "底图像素长度不符：{} 字节，应为 {want}（{width}x{height}）",
            dst.len()
        ));
    }
    if layer.len() != want {
        return Err(format!(
            "标注层像素长度不符：{} 字节，应为 {want}（{width}x{height}）",
            layer.len()
        ));
    }
    for (d, s) in dst
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(layer.as_chunks::<4>().0)
    {
        let a = u32::from(s[3]);
        if a == 0 {
            continue;
        }
        if a == 255 {
            d[0] = s[2];
            d[1] = s[1];
            d[2] = s[0];
            continue;
        }
        // 非预乘混合：src·a + dst·(1-a)，四舍五入用 +127（= 255/2 取整）而不是 +128。
        let inv = 255 - a;
        let mix =
            |s: u8, d: u8| -> u8 { ((u32::from(s) * a + u32::from(d) * inv + 127) / 255) as u8 };
        d[0] = mix(s[2], d[0]);
        d[1] = mix(s[1], d[1]);
        d[2] = mix(s[0], d[2]);
    }
    Ok(())
}

#[cfg(windows)]
pub mod win {
    //! 真正摸 Win32 的部分：问出鼠标所在那块屏的矩形，把它抓进内存，以及给剪贴板备料。

    use std::cell::Cell;
    use std::sync::OnceLock;
    use std::time::Duration;

    use windows::core::{w, BOOL};
    use windows::Win32::Foundation::{
        GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
    };
    use windows::Win32::Graphics::Dwm::{
        DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
    };
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, GetDeviceCaps, GetMonitorInfoW, MonitorFromPoint, ReleaseDC, SelectObject,
        BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DESKTOPHORZRES, DESKTOPVERTRES, DIB_RGB_COLORS,
        HBITMAP, HDC, HGDIOBJ, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST, SRCCOPY,
    };
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
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
        CreateWindowExW, DefWindowProcW, EnumWindows, GetCursorPos, GetWindowLongPtrW,
        GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible,
        RegisterClassW, GWL_EXSTYLE, HWND_MESSAGE, WNDCLASSW, WS_EX_TOOLWINDOW,
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
    /// ⚠ **不要拿它去否决截图。** 这句注释以前写的是"两者不一致就必须停下来"，而真机上
    /// 照它写的硬守卫把整条功能全挡死过（日志里六次截图六次被拒，而代价只是"可能糊一点"）——
    /// 现在 `snip.rs` 的 `run_capture` 只记日志等级。它留在树上的用处是让 Windows 侧的探针
    /// 能把「抓到的尺寸」与「显示器声称的尺寸」摆在同一行里，不是当闸用。
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

    /// 校验 BGRA 与它声称的尺寸是否自洽。**不碰剪贴板**，所以尺寸不符时在动用户剪贴板之前就失败。
    fn check_bgra_size(bgra: &[u8], width: u32, height: u32) -> Result<usize, String> {
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
        Ok(bytes)
    }

    /// 拼**剪贴板专用**的 CF_DIB 负载：正高度（bottom-up）+ BGRA 行序倒过来。
    ///
    /// 为什么不复用 `dib_header` 的负高度：那一版是给 `GetDIBits` 用的（负高度在它那里表示
    /// 自上而下），而剪贴板的读者按 Windows 自己 PrtScn 交出的约定来。实测一份 top-down 的
    /// CF_DIB 让 .NET 的 `Clipboard.GetImage()` 抛 `NullReferenceException`（也就是走 WinForms /
    /// WPF 那一路的粘帖目标全拿不到图），症状就是"截图存了盘、粘出去什么都没有"。
    /// 纯函数，所以"方向"能脱离真实剪贴板被断言 —— 注意断言必须用**能区分上下**的图形，
    /// 1x1 的夹具里倒不倒序都看不出差别。
    pub fn dib_payload_bottom_up(bgra: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
        check_bgra_size(bgra, width, height)?;
        let mut header = dib_header(width, height);
        header.biHeight = height as i32; // 正 = bottom-up
        let row = (width as usize) * 4;
        let mut flipped = Vec::with_capacity(bgra.len());
        for r in (0..height as usize).rev() {
            flipped.extend_from_slice(&bgra[r * row..(r + 1) * row]);
        }
        Ok(dib_bytes(&header, &flipped))
    }

    /// 注册 `PNG` 剪贴板格式：浏览器与网页编辑器读 `image/png`，它们**不看** CF_DIB。
    /// 返回 `None` 表示系统没这个格式 ⇒ 只交 CF_DIB，不影响主路径。
    fn png_format() -> Option<u32> {
        let id = unsafe { RegisterClipboardFormatW(w!("PNG")) };
        if id == 0 {
            tracing::warn!("注册剪贴板格式 PNG 失败，本次只交 CF_DIB");
            None
        } else {
            Some(id)
        }
    }

    /// 把截图放进剪贴板：CF_DIB（bottom-up）+ `PNG`（`png` 非空时）。
    ///
    /// 线程亲和：`Open → Empty → Set… → Close` 必须整段在同一个线程里跑完，所以这里不拆成三个
    /// 公开函数；调用方把整个调用放进一个 `spawn_blocking` 闭包就是对的。
    pub fn write_image_to_clipboard(
        bgra: &[u8],
        width: u32,
        height: u32,
        png: &[u8],
    ) -> Result<(), String> {
        // 先备料再开窗：校验失败时必须原样留下用户剪贴板里的东西。
        let payload = dib_payload_bottom_up(bgra, width, height)?;
        let png_fmt = if png.is_empty() { None } else { png_format() };

        let owner = clip_owner_hwnd()?;
        let mut last = String::from("未尝试");
        for attempt in 1..=5u32 {
            match unsafe { OpenClipboard(Some(owner)) } {
                Ok(()) => {
                    let result = unsafe { write_formats(&payload, png_fmt.zip(Some(png))) };
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

    /// 把字节搬进一块 `GMEM_MOVEABLE` 全局内存。调用方负责之后的 `SetClipboardData` 或释放。
    unsafe fn prep_global(bytes: &[u8]) -> Result<HGLOBAL, String> {
        let hg: HGLOBAL = match GlobalAlloc(GMEM_MOVEABLE, bytes.len()) {
            Ok(h) => h,
            Err(e) => return Err(format!("分配剪贴板内存失败（{} 字节）：{e}", bytes.len())),
        };
        let dst = GlobalLock(hg);
        if dst.is_null() {
            let _ = GlobalFree(Some(hg));
            return Err(format!("GlobalLock 返回空指针（{} 字节）", bytes.len()));
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst as *mut u8, bytes.len());
        // GlobalUnlock 的返回值在这里**必须丢掉**：锁计数归零时它就返回 0，而 0 正是"已经解锁"
        // 这个我们想要的结果。windows 把 BOOL 包成 Result，于是"成功解锁"被翻译成 Err，而它又不写
        //LastError ⇒ 报出来是那句自相矛盾的"失败：操作成功完成。(0x00000000)"。
        // 当成错误提前返回会让 SetClipboardData 永远不被调用：截图照样存盘，但粘不出任何东西
        // （这条成功路径原本没有任何用例覆盖，所以它在生产里错了三个版本）。
        let _ = GlobalUnlock(hg);
        Ok(hg)
    }

    /// 已持有剪贴板时的写入段。调用方负责 `CloseClipboard`。
    unsafe fn write_formats(dib: &[u8], png: Option<(u32, &[u8])>) -> Result<(), String> {
        // 顺序刻意是「先把内存备料完成，最后才 EmptyClipboard」：备料任何一步失败都不该动用户
        // 已有的剪贴板内容。旧写法先 Empty 再备料，失败就留下一个**空剪贴板** —— 症状比"没复制上"
        // 更糟，是把他原本要粘的东西擦掉了。
        let h_dib = prep_global(dib)?;
        let h_png = match png {
            Some((fmt, bytes)) => match prep_global(bytes) {
                Ok(h) => Some((fmt, h)),
                Err(e) => {
                    let _ = GlobalFree(Some(h_dib));
                    return Err(e);
                }
            },
            None => None,
        };

        // 清空失败 ⇒ 用户的剪贴板还原样在、这两块内存仍是我们自己的：必须释放。
        // （`SetClipboardData` 只有**成功**才接手所有权 —— 这一族 bug 的固定结局是双释放。）
        if let Err(e) = EmptyClipboard() {
            if let Some((_, hp)) = h_png {
                let _ = GlobalFree(Some(hp));
            }
            let _ = GlobalFree(Some(h_dib));
            return Err(format!("清空剪贴板失败：{e}"));
        }
        // 从这一行起用户原来的内容**已经没了**。所以后面失败的症状不是"没复制上"，
        // 而是"原内容被擦掉、新内容也没进去" —— 这个残口按第 63 轮的口径**接受**：
        // 回灌要在每次复制都留一份全局内存副本，代价大于它救的那几个场景。
        // 但必须出声，否则用户只知道"粘出来是空的"。
        if let Err(e) = SetClipboardData(CF_DIB, Some(HANDLE(h_dib.0))) {
            if let Some((_, hp)) = h_png {
                let _ = GlobalFree(Some(hp));
            }
            let _ = GlobalFree(Some(h_dib));
            tracing::error!(
                "剪贴板已清空却什么都没写进去（用户原来的内容没了，这次截图也没复制上）：{e}"
            );
            return Err(format!("写入剪贴板失败：{e}"));
        }
        // PNG 是加分项：它失败不该让整次截图算失败（CF_DIB 已经进了剪贴板）。
        if let Some((fmt, hp)) = h_png {
            if let Err(e) = SetClipboardData(fmt, Some(HANDLE(hp.0))) {
                tracing::warn!("CF_DIB 已进剪贴板，但 PNG 没放进去：{e}");
                let _ = GlobalFree(Some(hp));
            }
        }
        Ok(())
    }

    /// 一个窗口能被枚举到的全部事实。纯数据 ⇒ 过滤规则能脱离真实桌面窗口被断言：
    /// `snap_candidate` 有用例钉着，`snap_targets` 只负责把这些事实取回来。
    #[derive(Debug, Clone, Copy, Default)]
    pub struct WinFacts {
        pub visible: bool,
        pub iconic: bool,
        pub cloaked: bool,
        pub tool_window: bool,
        pub own_process: bool,
        /// 物理边界 (left, top, right, bottom)；取不到就是 `None`。
        pub bounds: Option<(i32, i32, i32, i32)>,
    }

    /// 这个窗口能不能当"点一下选中整窗"的候选；能的话给出**裁进这块屏**的矩形。
    ///
    /// 每条过滤都对应一种真实噪声：
    /// - 不可见 / 最小化：框上去是一张空图。
    /// - DWM cloaked：UWP、Edge 那些"`IsWindowVisible` 说 TRUE、其实根本没在画"的窗口。
    /// - `WS_EX_TOOLWINDOW`：菜单、tooltip、自动补全弹层 —— 它们恰恰挂在最上层，
    ///   不排掉的话鼠标一停就吸到一条 8 像素高的悬浮条上。
    /// - 本进程的窗口：截图覆盖层自己铺满整屏，选中它等于"点一下＝整屏"，功能当场作废。
    ///   代价是程序自己的主窗口/悬浮窗也只能拖框 —— 绕不开，tauri 的窗口类名是 WebView2
    ///   那套通用名，认不出"哪个 hwnd 是我"。
    ///
    /// 裁剪不是可选项：窗口经常露出一屏外（贴边、跨屏），而 `Shot::crop` 对越界选区是**报错**，
    /// 不裁就成了"点一下整窗，结果截图失败"。小于 8x8 的一律不要 —— 选中只产出废图。
    pub fn snap_candidate(f: &WinFacts, monitor: &MonitorRect) -> Option<MonitorRect> {
        if !f.visible || f.iconic || f.cloaked || f.tool_window || f.own_process {
            return None;
        }
        let (l, t, r, b) = f.bounds?;
        let left = l.max(monitor.x);
        let top = t.max(monitor.y);
        let right = r.min(monitor.x + monitor.width as i32);
        let bottom = b.min(monitor.y + monitor.height as i32);
        let w = (right - left).max(0) as u32;
        let h = (bottom - top).max(0) as u32;
        if w < 8 || h < 8 {
            return None;
        }
        MonitorRect::new(left, top, w, h).ok()
    }

    /// 按 **z-order 从最上往下**列出这块屏上可吸附的窗口矩形（第一个包住光标的就是用户看到的）。
    ///
    /// `EnumWindows` 的枚举顺序就是桌面 z 序（顶层在前）。这件事对以后做贴图有实际作用：
    /// 贴图窗口是 topmost，它盖在目标之上时**应该吸到贴图**，按 z 序取第一个就自然成立，
    /// 不用为它写任何特例。
    ///
    /// 边界优先取 `DWMWA_EXTENDED_FRAME_BOUNDS`：Win10/11 的 `GetWindowRect` 含一圈不可见的
    /// 调整边框（每边约 7~8 像素），拿它当"这个窗口长什么样"会明显偏大一圈。
    pub fn snap_targets(monitor: &MonitorRect) -> Vec<MonitorRect> {
        struct Ctx {
            monitor: MonitorRect,
            own_pid: u32,
            out: Vec<MonitorRect>,
        }

        unsafe extern "system" fn each(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let ctx = &mut *(lparam.0 as *mut Ctx);
            let mut r = RECT::default();
            let mut pid: u32 = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            let mut cloaked: u32 = 0;
            let bounds = if DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                &mut r as *mut RECT as *mut core::ffi::c_void,
                core::mem::size_of::<RECT>() as u32,
            )
            .is_ok()
            {
                Some((r.left, r.top, r.right, r.bottom))
            } else if GetWindowRect(hwnd, &mut r).is_ok() {
                // DWM 拿不到（没合成、老驱动）就退回 GDI 那份：偏一点总比没有强。
                Some((r.left, r.top, r.right, r.bottom))
            } else {
                None
            };
            let facts = WinFacts {
                visible: IsWindowVisible(hwnd).as_bool(),
                iconic: IsIconic(hwnd).as_bool(),
                cloaked: DwmGetWindowAttribute(
                    hwnd,
                    DWMWA_CLOAKED,
                    &mut cloaked as *mut u32 as *mut core::ffi::c_void,
                    core::mem::size_of::<u32>() as u32,
                )
                .is_ok()
                    && cloaked != 0,
                tool_window: (GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOOLWINDOW.0 as isize)
                    != 0,
                own_process: pid != 0 && pid == ctx.own_pid,
                bounds,
            };
            if let Some(rect) = snap_candidate(&facts, &ctx.monitor) {
                ctx.out.push(rect);
            }
            true.into()
        }

        let mut ctx = Ctx {
            monitor: *monitor,
            own_pid: std::process::id(),
            out: Vec::new(),
        };
        // 枚举失败不是错误而只是"这次没有候选"：底图已经抓好了，点选退化成拖框就行。
        if let Err(e) = unsafe { EnumWindows(Some(each), LPARAM(&mut ctx as *mut Ctx as isize)) } {
            tracing::warn!("枚举窗口失败，本次截图没有吸附候选：{e}");
            return Vec::new();
        }
        ctx.out
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

    // ---------- 标注层：解码 / 合成（M0 的地基） ----------

    /// 现场编一张任意色型/位深的 PNG 喂解码器 —— 本仓不存外部图片文件（夹具的既有纪律）。
    fn png_of(color: png::ColorType, depth: png::BitDepth, data: &[u8], w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(color);
            enc.set_depth(depth);
            let mut writer = enc.write_header().expect("写头");
            writer.write_image_data(data).expect("写数据");
        }
        out
    }

    /// 伪随机底图：值域压到 0..=199，alpha 一律 0（GDI 交回的就是 0）。
    /// 上限不到 200 是有意的 —— 后面几条用例把「纯红」当作标注的记号，底图里不能撞到。
    fn noise_bgra(w: u32, h: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity((w as usize) * (h as usize) * 4);
        let mut s = 0x2545_F491_u32;
        for _ in 0..(w as usize * h as usize) {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let n = (s >> 16) as u8;
            v.extend_from_slice(&[n % 200, n / 2 % 200, n.wrapping_add(37) % 200, 0]);
        }
        v
    }

    #[test]
    fn decode_png_rgba_roundtrips_this_repos_encoder() {
        let mut rgba = Vec::new();
        for i in 0..6u8 {
            rgba.extend_from_slice(&[10 + i, 200 - i, 40 + i, if i % 2 == 0 { 255 } else { 0 }]);
        }
        let bytes = encode_png(&rgba, 3, 2).unwrap();
        let (got, w, h) = decode_png_rgba(&bytes).unwrap();
        assert_eq!((w, h), (3, 2), "尺寸要照着 IHDR 报");
        assert_eq!(got, rgba, "编解码必须逐字节闭合");
        assert_eq!(
            got[7], 0,
            "透明格解码回来还是透明（当成不透明会把整张截图盖掉）"
        );
    }

    /// 垃圾输入要 `Err` 而不是 panic：release 是 panic=abort，崩一次就是整个程序没了。
    #[test]
    fn decode_png_rgba_rejects_garbage_instead_of_panicking() {
        let cases: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"nonsense".to_vec(),
            b"\x89PNG\r\n\x1a\n".to_vec(),
            vec![0u8; 4096],
        ];
        for bytes in cases {
            assert!(
                decode_png_rgba(&bytes).is_err(),
                "垃圾字节要报错，不能 panic 也不能返回半成品"
            );
        }
        // 截断的真 PNG：头能读、数据读不完 —— 同样只能 Err（半张标注层比不画更糟）
        let mut rgba = vec![0u8; 4 * 20 * 20];
        for px in rgba.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
        let full = encode_png(&rgba, 20, 20).unwrap();
        let truncated = full[..full.len() / 2].to_vec();
        assert!(
            decode_png_rgba(&truncated).is_err(),
            "截断的 PNG 不能返回半成品"
        );
    }

    #[test]
    fn decode_png_rgba_rejects_other_color_types_and_depths() {
        let gray = png_of(
            png::ColorType::Grayscale,
            png::BitDepth::Eight,
            &[7, 8, 9, 10, 11, 12],
            3,
            2,
        );
        let e = decode_png_rgba(&gray).unwrap_err();
        assert!(e.contains("Grayscale"), "报错要把实际色型写进消息：{e}");

        let rgb = png_of(
            png::ColorType::Rgb,
            png::BitDepth::Eight,
            &[1, 2, 3, 4, 5, 6, 7, 8, 9],
            3,
            1,
        );
        let e = decode_png_rgba(&rgb).unwrap_err();
        assert!(e.contains("只支持"), "非 RGBA 要直接拒：{e}");

        let deep = png_of(
            png::ColorType::Rgba,
            png::BitDepth::Sixteen,
            &[0u8; 3 * 2 * 8],
            3,
            2,
        );
        let e = decode_png_rgba(&deep).unwrap_err();
        assert!(
            e.contains("16"),
            "16 位要出声（悄悄降到 8 位就是一次重采样）：{e}"
        );
    }

    #[test]
    fn composite_over_blends_alpha_zero_half_and_full() {
        // 底色 BGRA (10, 20, 30) + alpha 0（GDI 的那第四字节）
        let mut dst = Vec::new();
        for _ in 0..3 {
            dst.extend_from_slice(&[10, 20, 30, 0]);
        }
        let mut layer = Vec::new();
        layer.extend_from_slice(&[255, 0, 0, 0]); // 全透明红
        layer.extend_from_slice(&[255, 0, 0, 128]); // 半透明红
        layer.extend_from_slice(&[255, 0, 0, 255]); // 不透明红
        composite_over(&mut dst, &layer, 3, 1).unwrap();

        assert_eq!(&dst[0..4], &[10, 20, 30, 0], "alpha=0 那一格必须逐字节不动");
        // 非预乘：src·a + dst·(255-a)，再 +127 除 255 取整。三格分别手算过：
        //   B = (0·128 + 10·127 + 127)/255 = 5
        //   G = (0·128 + 20·127 + 127)/255 = 10
        //   R = (255·128 + 30·127 + 127)/255 = 143
        assert_eq!(&dst[4..8], &[5, 10, 143, 0], "半透明要走非预乘混合");
        assert_eq!(&dst[8..12], &[0, 0, 255, 0], "alpha=255 就是整格覆盖");
        assert_eq!(
            dst[3], 0,
            "底图的 alpha 字节谁都不许改（CF_DIB 那份要看它）"
        );
    }

    /// 通道序是第一号坑：解码层是 RGBA、底图是 BGRA，写反了的图只是「颜色差一点」，
    /// 谁也说不出哪里不对。只断 alpha 三档照不出红蓝互换，所以这里一路断到落盘那张 PNG。
    #[test]
    fn composite_over_puts_rgba_red_onto_bgra_blue() {
        let mut dst = vec![9u8, 9, 9, 0];
        composite_over(&mut dst, &[255, 0, 0, 255], 1, 1).unwrap();
        assert_eq!(
            &dst[..3],
            &[0, 0, 255],
            "页面画的纯红要落成 BGR (0,0,255)；落成 (255,0,0) 就是通道写反了"
        );
        assert_eq!(dst[3], 0);

        // 闭环：合成后的 BGRA 走产品那条路（bgra_to_rgba → encode → decode）颜色不该变
        let mut rgba = dst.clone();
        bgra_to_rgba(&mut rgba);
        assert_eq!(rgba, vec![255, 0, 0, 255], "绕一圈回到 RGBA 还是那抹红");
        let (back, w, h) = decode_png_rgba(&encode_png(&rgba, 1, 1).unwrap()).unwrap();
        assert_eq!((w, h), (1, 1));
        assert_eq!(back, rgba, "落盘的像素就是合成出来的那一份");
    }

    #[test]
    fn composite_over_rejects_size_mismatch() {
        let mut dst = vec![0u8; 4 * 4 * 2];
        let before = dst.clone();
        assert!(composite_over(&mut dst, &[0u8; 4 * 4 * 2 - 4], 4, 2).is_err());
        assert!(composite_over(&mut dst, &[0u8; 4 * 4 * 3], 4, 2).is_err());
        assert!(composite_over(&mut dst, &[0u8; 4 * 4 * 2], 4, 3).is_err());
        assert_eq!(dst, before, "报错的那一次一个字节都不该落到 1:1 的画布上");
        let mut one = [0u8; 4];
        assert!(composite_over(&mut one, &[0u8; 4], 1, 1).is_ok());
    }

    /// M0 红线第一半：未被标注覆盖的像素必须逐字节相等。
    #[test]
    fn composite_over_leaves_every_uncovered_pixel_byte_identical() {
        let (w, h) = (40u32, 30u32);
        let base = noise_bgra(w, h);

        // 先测最纯的形式：整层全透明 ⇒ 底图一个字节都不动
        let transparent = vec![0u8; (w as usize) * (h as usize) * 4];
        let mut dst = base.clone();
        composite_over(&mut dst, &transparent, w, h).unwrap();
        assert_eq!(dst, base, "空标注层不许碰到底图任何一个字节");

        // 再测只画一小块：块外逐字节相等，块内逐字节等于覆盖值，且改动像素数刚好等于块面积
        let (x0, y0, bw, bh) = (12usize, 7usize, 5usize, 4usize);
        let mut layer = transparent.clone();
        for y in y0..(y0 + bh) {
            for x in x0..(x0 + bw) {
                let i = (y * w as usize + x) * 4;
                layer[i..i + 4].copy_from_slice(&[200, 30, 90, 255]);
            }
        }
        let mut dst = base.clone();
        composite_over(&mut dst, &layer, w, h).unwrap();

        let mut changed = 0usize;
        for (idx, (got, old)) in dst
            .as_chunks::<4>()
            .0
            .iter()
            .zip(base.as_chunks::<4>().0.iter())
            .enumerate()
        {
            let (x, y) = (idx % w as usize, idx / w as usize);
            let inside = x >= x0 && x < x0 + bw && y >= y0 && y < y0 + bh;
            if inside {
                assert_eq!(
                    got,
                    &[90, 30, 200, 0],
                    "块内是不透明覆盖（BGR 换序、alpha 不动）"
                );
                changed += 1;
            } else {
                assert_eq!(got, old, "块外第 {x},{y} 格被标注层碰到了");
            }
        }
        assert_eq!(changed, bw * bh, "改动到的像素必须刚好是那一块的面积");
    }

    /// M0 红线第二半：标注位置在最终 PNG 里与页面所见误差 ≤1px。
    /// 走的是真链路：带分数的 CSS 选区 → `css_rect_to_physical` → `crop` → 合成 → 编码 → 解码。
    /// 画笔矩形的位置也同一个函数算（探针对照选区做差），不在用例里再写一套四舍五入。
    #[test]
    fn annotation_lands_where_the_page_drew_it() {
        let dpr = 1.5;
        let monitors = [mon(400, 300), MonitorRect::new(-1920, 0, 400, 300).unwrap()];
        for m in monitors {
            let sel = CssRect {
                x: 41.33,
                y: 23.67,
                w: 100.33,
                h: 60.67,
            };
            let sel_phys = css_rect_to_physical(&sel, dpr, &m).unwrap();
            let shot = Shot::new(m, noise_bgra(m.width, m.height)).unwrap();
            let cropped = shot
                .crop(
                    sel_phys.x as u32,
                    sel_phys.y as u32,
                    sel_phys.width,
                    sel_phys.height,
                )
                .unwrap();

            // 页面在选区内画的笔（CSS 相对选区原点）：换算成画布内的物理像素
            let pen = CssRect {
                x: sel.x + 10.5,
                y: sel.y + 7.25,
                w: 20.0,
                h: 12.0,
            };
            let pen_phys = css_rect_to_physical(&pen, dpr, &m).unwrap();
            let (bx, by) = (
                (pen_phys.x - sel_phys.x) as i64,
                (pen_phys.y - sel_phys.y) as i64,
            );
            let (bw, bh) = (pen_phys.width as i64, pen_phys.height as i64);
            assert!(bx >= 0 && by >= 0, "画笔必须落在选区画布内：{bx},{by}");

            let (cw, ch) = (sel_phys.width as usize, sel_phys.height as usize);
            let mut layer = vec![0u8; cw * ch * 4];
            for y in by..(by + bh) {
                for x in bx..(bx + bw) {
                    let i = (y as usize * cw + x as usize) * 4;
                    // 页面画的是纯红 RGBA
                    layer[i..i + 4].copy_from_slice(&[255, 0, 0, 255]);
                }
            }

            let mut dst = cropped.bgra.clone();
            composite_over(&mut dst, &layer, sel_phys.width, sel_phys.height).unwrap();
            // 落盘那一步（snip.rs 里就是这么编的）
            let mut encoded = dst.clone();
            bgra_to_rgba(&mut encoded);
            let png = encode_png(&encoded, sel_phys.width, sel_phys.height).unwrap();
            let (final_px, fw, fh) = decode_png_rgba(&png).unwrap();
            assert_eq!(
                (fw, fh),
                (sel_phys.width, sel_phys.height),
                "最终 PNG 的尺寸就是选区的物理尺寸"
            );

            // 量一遍标注块在最终 PNG 里的实际位置
            let mut min_x = usize::MAX;
            let mut min_y = usize::MAX;
            let mut max_x = 0usize;
            let mut max_y = 0usize;
            let mut hits = 0usize;
            for (idx, px) in final_px.as_chunks::<4>().0.iter().enumerate() {
                if px != &[255, 0, 0, 255] {
                    continue;
                }
                let (x, y) = (idx % cw, idx / cw);
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
                hits += 1;
            }
            assert!(hits > 0, "最终 PNG 里一个标注像素都没有 —— 合成没生效");
            let report = format!(
                "屏 {}x{} 选区 {bw}x{bh} 标注块实测 ({min_x},{min_y})..({max_x},{max_y}) 期望 ({bx},{by})..({},{}), 命中 {hits}",
                m.width,
                m.height,
                bx + bw - 1,
                by + bh - 1
            );
            assert_eq!(hits, (bw * bh) as usize, "标注块像素数不符：{report}");
            assert_eq!(min_x as i64, bx, "左边界偏了：{report}");
            assert_eq!(min_y as i64, by, "上边界偏了：{report}");
            assert_eq!(max_x as i64, bx + bw - 1, "右边界偏了：{report}");
            assert_eq!(max_y as i64, by + bh - 1, "下边界偏了：{report}");

            // 红线第一半在这条链路末端再钉一次：块外逐字节等于未标注时那张
            let mut untouched = cropped.bgra.clone();
            bgra_to_rgba(&mut untouched);
            for (idx, (got, old)) in final_px
                .as_chunks::<4>()
                .0
                .iter()
                .zip(untouched.as_chunks::<4>().0.iter())
                .enumerate()
            {
                let (x, y) = (idx % cw, idx / cw);
                let inside = (x as i64) >= bx
                    && (x as i64) < bx + bw
                    && (y as i64) >= by
                    && (y as i64) < by + bh;
                if !inside {
                    assert_eq!(got, old, "标注块之外的一格被改了：屏 {:?} 处 {x},{y}", m.x);
                }
            }
        }
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

        /// 平台前提（不是功能测试）：`GlobalUnlock` 在**锁计数归零**时返回 0，而它不写
        /// LastError —— 于是 windows 把 BOOL 包成 `Result`（判据 `!=0`）之后，"成功解锁"
        /// 变成 `Err(操作成功完成 0x0)`。
        ///
        /// 这条钉的是 `prep_global` 为什么**必须忽略** `GlobalUnlock` 的返回值：当年把它当错误
        /// 提前返回，造成每次截图清空用户剪贴板却什么都不复制，错着走了三个版本。
        /// 它只 lock/unlock 自己分配的一块全局内存、**不碰剪贴板**，所以能进门禁。
        /// 哪天封装改了约定（真的返回 Ok），这条会红，要重看的是 `prep_global` 的写法。
        #[test]
        fn global_unlock_reports_the_lock_count_not_an_error() {
            use windows::Win32::Foundation::{GetLastError, GlobalFree, SetLastError, WIN32_ERROR};
            use windows::Win32::System::Memory::{
                GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
            };
            unsafe {
                SetLastError(WIN32_ERROR(0));
                let hg = GlobalAlloc(GMEM_MOVEABLE, 64).expect("GlobalAlloc 该成功");
                assert!(!GlobalLock(hg).is_null(), "刚分配的全局内存锁不住？");
                SetLastError(WIN32_ERROR(0));
                let unlocked = GlobalUnlock(hg);
                let last = GetLastError();
                assert!(
                    unlocked.is_err(),
                    "windows 现在把返回 0 当成功了？那 prep_global 里「忽略返回值」的写法要重评"
                );
                assert_eq!(
                    last,
                    WIN32_ERROR(0),
                    "锁计数归零不是错误，不该留下错误码 —— 日志里那句「失败：操作成功完成」就是这么来的"
                );
                let _ = GlobalFree(Some(hg));
            }
        }

        /// 吸附候选的过滤与裁剪，逐条钉住 —— 每条过滤都对应一种真实噪声，
        /// 合并成一个 `if` 就看不出哪条还活着，所以分开断言。
        #[test]
        fn snap_candidate_filters_noise_and_clamps_to_the_monitor() {
            let mon = crate::capture::MonitorRect::new(0, 0, 1920, 1080).unwrap();
            let base = win::WinFacts {
                visible: true,
                bounds: Some((100, 100, 600, 500)),
                ..Default::default()
            };
            let reject = |f: win::WinFacts| {
                assert!(
                    win::snap_candidate(&f, &mon).is_none(),
                    "这类窗口不该进候选：{f:?}"
                );
            };

            let got = win::snap_candidate(&base, &mon).expect("正常窗口该是候选");
            assert_eq!((got.x, got.y, got.width, got.height), (100, 100, 500, 400));

            reject(win::WinFacts {
                visible: false,
                ..base
            });
            reject(win::WinFacts {
                iconic: true,
                ..base
            });
            // UWP/Edge 那些"IsWindowVisible 说 TRUE 其实没在画"的
            reject(win::WinFacts {
                cloaked: true,
                ..base
            });
            // 菜单、tooltip、自动补全弹层：它们恰恰挂在最上层
            reject(win::WinFacts {
                tool_window: true,
                ..base
            });
            // 自己进程不排掉的话，铺满整屏的覆盖层永远是第一个候选 ⇒ 点一下＝整屏
            reject(win::WinFacts {
                own_process: true,
                ..base
            });
            reject(win::WinFacts {
                bounds: None,
                ..base
            });
            // 5x400 的边条：选中只会产出一张废图
            reject(win::WinFacts {
                bounds: Some((100, 100, 104, 504)),
                ..base
            });
            // 完全在这块屏之外
            reject(win::WinFacts {
                bounds: Some((-4000, -4000, -3000, -3000)),
                ..base
            });

            // 露出一屏外要**裁进来**：`Shot::crop` 对越界选区是报错，不裁就成了"点一下反而失败"
            let got = win::snap_candidate(
                &win::WinFacts {
                    bounds: Some((-100, -100, 3000, 3000)),
                    ..base
                },
                &mon,
            )
            .expect("跨边窗口裁进来后仍是候选");
            assert_eq!((got.x, got.y, got.width, got.height), (0, 0, 1920, 1080));

            // 副屏（负原点）上算的也是它自己那块矩形，不被原点的符号带偏
            let sec = crate::capture::MonitorRect::new(-1920, 0, 1920, 1080).unwrap();
            let got = win::snap_candidate(
                &win::WinFacts {
                    bounds: Some((-1500, 20, -1000, 520)),
                    ..base
                },
                &sec,
            )
            .expect("副屏上的窗口该能吸附");
            assert_eq!((got.x, got.y, got.width, got.height), (-1500, 20, 500, 500));
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

        /// 剪贴板那份 DIB 的方向：**正高度**，且行序倒过来（我们的 BGRA 是自上而下的）。
        /// 夹具必须能区分上下 —— 1x1 里倒不倒序都得到同一份字节，那是条假绿。
        #[test]
        fn dib_payload_bottom_up_flips_the_rows() {
            // 2x2：顶行 8 个字节全是 1，底行全是 2
            let bgra: Vec<u8> = vec![1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2];
            let bytes = win::dib_payload_bottom_up(&bgra, 2, 2).unwrap();
            assert_eq!(bytes.len(), 40 + 16);
            assert_eq!(&bytes[..4], &40u32.to_le_bytes()[..], "biSize 恒为 40");
            assert_eq!(
                &bytes[8..12],
                &2i32.to_le_bytes()[..],
                "剪贴板的 biHeight 必须是正数：负高度那份实测让 .NET 的 Clipboard.GetImage 抛异常"
            );
            assert_eq!(&bytes[40..48], &[2u8; 8][..], "紧跟头的该是**底行**");
            assert_eq!(&bytes[48..56], &[1u8; 8][..], "第二块该是顶行");
        }

        #[test]
        fn dib_payload_rejects_inputs_that_do_not_match_their_own_size() {
            for (buf, w, h) in [
                (&[0u8; 3][..], 1u32, 1u32),
                (&[][..], 0u32, 0u32),
                (&[0u8; 8][..], 2u32, 2u32),
            ] {
                let err = win::dib_payload_bottom_up(buf, w, h).unwrap_err();
                assert!(
                    err.contains("剪贴板"),
                    "错误要指得出是哪一层拒的，实际：{err}"
                );
            }
        }

        /// 只读地问一句"剪贴板里现在有没有这个格式"。读不出来给 Err，**不折成"没有"** ——
        /// 这条 helper 存在的意义就是给下面的用例一个真的观察量。
        fn clip_has_fmt(fmt: u32) -> Result<bool, String> {
            use windows::Win32::System::DataExchange::{
                CloseClipboard, IsClipboardFormatAvailable, OpenClipboard,
            };
            let mut last = String::from("未尝试");
            for attempt in 1..=10u32 {
                match unsafe { OpenClipboard(None) } {
                    Ok(()) => {
                        let has = unsafe { IsClipboardFormatAvailable(fmt) }.is_ok();
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

        /// 上面那条的 CF_DIB 专用写法（三处用例都在问它）。
        fn clip_has_dib() -> Result<bool, String> {
            clip_has_fmt(win::CF_DIB)
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
                assert!(win::write_image_to_clipboard(buf, w, h, &[]).is_err());
            }
            assert_eq!(
                clip_has_dib().expect("调用后又读不到剪贴板了"),
                before,
                "校验失败却动了用户的剪贴板"
            );
        }

        /// 成功路径真的落到剪贴板上：写一张 1x1 的 CF_DIB，再问"现在有没有 CF_DIB"。
        ///
        /// `#[ignore]` 的原因是本机的硬约束 —— 它会**覆盖用户的剪贴板内容**，只能他点头才跑。
        /// 但它盯的正是那条此前零覆盖的成功路径：旧写法在 `GlobalUnlock` 处把"锁计数归零"
        /// 当成失败返回（实测原始返回 0、last error 也是 0，windows 把它翻成
        /// `Err("操作成功完成 (0x0)")`），截图照样存盘却永远粘不出东西，在生产里错着走了三个版本。
        /// 想跑：`cargo test -p focusflow-core --lib -- --ignored capture::tests::good_input`
        #[test]
        #[ignore = "会覆盖用户剪贴板内容：只在用户明确同意时用 --ignored 跑"]
        fn good_input_actually_lands_on_the_clipboard() {
            use windows::core::w;
            use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
            let bgra = [12u8, 40, 66, 255]; // 1x1 不透明
                                            // 这一层不校验 PNG 内容（编码在 encode_png 那层已经保证），所以给一段假字节即可
            let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
            if let Err(e) = win::write_image_to_clipboard(&bgra, 1, 1, &png) {
                panic!("1x1 的图应该写得进剪贴板，却报：{e}");
            }
            assert!(
                clip_has_dib().expect("写完读不到剪贴板，这条用例就没有观察量了"),
                "写入返回成功，剪贴板里却没有 CF_DIB"
            );
            let png_fmt = unsafe { RegisterClipboardFormatW(w!("PNG")) };
            assert_ne!(
                png_fmt, 0,
                "这台机器上连 PNG 注册格式都拿不到，那条腿没有观察量"
            );
            assert!(
                clip_has_fmt(png_fmt).expect("读不到剪贴板"),
                "CF_DIB 进去了但 PNG 没进去：浏览器/网页编辑器只认 PNG，症状照样是\"粘不出图\""
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
