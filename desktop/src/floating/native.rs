//! 原生悬浮窗：分层窗口（`WS_EX_LAYERED`）+ `UpdateLayeredWindow` 逐像素 alpha。
//!
//! 为什么不用 WebView2：一个 90×46 的卡片要养一整套浏览器进程树（实测约占本程序总占用的
//! 九成，见 `docs/memory-baseline.md`）。小尺寸置顶卡片的通行做法是自己画窗口 ——
//! 输入法候选窗、截图工具的选择层都是这么做的，而不是养一个浏览器。
//! 这里用 GDI 而不是 Direct2D：卡片只有两行文字与一个圆角底，`UpdateLayeredWindow` 已经够，
//! 不必再引入 COM 依赖。
//!
//! 消息循环跑在本模块自己的线程（`floating-native`）上，窗口句柄放进原子量供 façade 读写。
//! Tauri 的窗口集合里**没有**这个窗口，所以任何"按 label 找悬浮窗"的旧代码都必须改走
//! `super` 的 façade —— 这是原生化最容易漏的一类接线，`mod.rs` 里逐个点名。
//!
//! 像素合成、版面、数字格式全在 `super::render`（无头可测）；这一层只剩"把字节交给 DWM"。

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Mutex, OnceLock};

use tauri::AppHandle;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi as g;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging as wm;

use super::render::{
    self, metric_from, palette_for, Color, Palette, PillContent, Rect, BADGE_FONT_PX, BORDER,
    FONT_PX, NOTICE_FONT_PX, PAUSED_DEN, PAUSED_NUM,
};
use super::{live_snapshot, paused_now, rest_notice_text};

/// 悬浮窗线程唯一的一份句柄。0 = 还没建起来或已经销毁。
static HWND_RAW: AtomicIsize = AtomicIsize::new(0);
/// 上一次交给 DWM 的内容指纹：没变就不再合成一遍（分层窗口的位图不会自己失效）。
static LAST_FP: Mutex<u64> = Mutex::new(0);
static APP: OnceLock<AppHandle> = OnceLock::new();

/// 重画节拍。数字是"瞟一眼"用的，1 秒够；WebView2 那条路是 500ms 推送，但省的那头在推送，
/// 这里读的是同一格 parked 快照，读得再勤也不会更准。
const TICK_MS: u32 = 1000;
const TIMER_ID: usize = 0xF0F1;
/// façade 催一次重画用的私有消息。
///
/// 为什么不用 `InvalidateRect`：分层窗口的像素不是 WM_PAINT 画出来的，是
/// `UpdateLayeredWindow` 交上去的 —— 把区域标脏只会什么都发生。而重画必须跑在
/// **拥有这个窗口的线程**上（GDI DC 与分层位图都是线程绑定的），所以这里投一条消息，
/// 由 `wndproc` 在正确的线程里画。
const WM_APP_REFRESH: u32 = 0x8001;
const CLASS_NAME: &str = "FocusFlowFloatingPill";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn scale_of(hwnd: HWND) -> f64 {
    unsafe { (GetDpiForWindow(hwnd).max(96)) as f64 / 96.0 }
}

/// 一块 32bpp 顶向 DIB。`bits` 指向预乘 BGRA，长度 `w*h*4`。
///
/// `BITMAPINFO` 只在建位图那一步用（GDI 把描述复制进段里），所以不留在结构里 ——
/// 留着一个没人读的字段，下一个人就会去猜"它是不是该参与删除"。
struct Surface {
    bmp: g::HBITMAP,
    bits: *mut u8,
    w: i32,
    h: i32,
}

impl Surface {
    /// # Safety
    /// `hdc` 必须是有效的内存 DC；返回的 `bits` 由 GDI 拥有，必须在本对象 `delete` 之前
    /// 先从 DC 上摘下来，且不得在位图删除后再碰 `bits`。
    unsafe fn new(hdc: g::HDC, w: i32, h: i32) -> Option<Self> {
        let info = g::BITMAPINFO {
            bmiHeader: g::BITMAPINFOHEADER {
                biSize: std::mem::size_of::<g::BITMAPINFOHEADER>() as u32,
                biWidth: w,
                // 负高度 = 顶向：第 0 行就是屏幕最上面那一行，与 render 的 y 轴同向
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: g::BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let bmp = g::CreateDIBSection(
            Some(hdc),
            &raw const info,
            g::DIB_RGB_COLORS,
            &mut bits,
            None,
            0,
        )
        .ok()?;
        if bits.is_null() {
            return None;
        }
        Some(Self {
            bmp,
            bits: bits as *mut u8,
            w,
            h,
        })
    }

    fn buf(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.bits, (self.w * self.h * 4) as usize) }
    }

    fn clear(&mut self) {
        let len = (self.w * self.h * 4) as usize;
        unsafe { std::ptr::write_bytes(self.bits, 0, len) };
    }

    /// 摘下来再删。删掉仍被 DC 选中的位图，Windows 不报错，但那块内存要等 DC 一起没 ——
    /// 每秒一次重画就是每小时几十 MB 的稳定漏。
    unsafe fn delete(self, hdc: g::HDC) {
        let _ = g::SelectObject(hdc, g::HGDIOBJ(std::ptr::null_mut()));
        let _ = g::DeleteObject(g::HGDIOBJ(self.bmp.0));
    }
}

fn hwnd() -> Option<HWND> {
    let raw = HWND_RAW.load(Ordering::SeqCst);
    (raw != 0).then_some(HWND(raw as *mut _))
}

pub fn is_alive() -> bool {
    hwnd().is_some()
}

pub fn is_visible() -> bool {
    match hwnd() {
        Some(h) => unsafe { wm::IsWindowVisible(h).as_bool() },
        None => false,
    }
}

/// 把三格 parked 数据拼成卡片内容。统计线程还没出第一拍时全 0（与前端 `Number(n||0)` 同口径，
/// 而不是画一块空白）。
fn current_content() -> PillContent {
    let cfg = focusflow_core::config::instance();
    let metric = metric_from(&cfg.get("gui", "floating_metric"));
    let live = live_snapshot();
    let (today, cpm, active) = match &live {
        Some(l) => (l.today_count, l.cpm, l.active_seconds),
        None => (0, 0, 0),
    };
    render::build_content(metric, today, cpm, active, paused_now(), rest_notice_text())
}

/// 内容 + 主题 + 尺寸指纹。**不能包含"何时画的"**：否则每拍都算变化，1 秒一次全表合成
/// 白烧 CPU，而这正是原生化要省的东西之一。
fn fingerprint(c: &PillContent, dark: bool, w: i32, h: i32) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut s = std::collections::hash_map::DefaultHasher::new();
    for r in &c.rows {
        r.label.hash(&mut s);
        r.value.hash(&mut s);
        r.accent.hash(&mut s);
    }
    c.paused.hash(&mut s);
    c.notice.hash(&mut s);
    dark.hash(&mut s);
    (w, h).hash(&mut s);
    s.finish()
}

/// 一段要画的文字：内容、矩形、颜色、字号。
struct Run {
    text: String,
    rc: Rect,
    color: Color,
    font_px: i32,
    /// 这一段背后要先铺的实色（通知带），`None` = 直接写在卡片上。
    band: Option<Color>,
}

/// 白字（通知带上的反白）与「已暂停」角标的色不在 palette 里，单独给。
const ON_BAND: Color = Color::rgba(255, 255, 255, 255);
/// 通知带底色：抄自 `floating.js:151` 那句 `background:rgba(220,120,30,.92)`。
const BAND: Color = Color::rgba(220, 120, 30, 235);

fn text_runs(c: &PillContent, w: i32, scale: f64, pal: Palette) -> Vec<Run> {
    let mut out = Vec::new();
    if let Some(text) = &c.notice {
        out.push(Run {
            text: text.clone(),
            rc: render::notice_rect(w, scale),
            color: ON_BAND,
            font_px: NOTICE_FONT_PX,
            band: Some(BAND),
        });
    }
    let rows = render::layout_rows(w, scale, c.rows.len(), usize::from(c.notice.is_some()));
    for (row, lay) in c.rows.iter().zip(rows.iter()) {
        out.push(Run {
            text: row.label.to_string(),
            rc: lay.label,
            color: pal.muted,
            font_px: FONT_PX,
            band: None,
        });
        let value_rect = if let Some(suf) = row.suffix {
            let (head, tail) = render::reserve_suffix(lay.value, scale, suf);
            out.push(Run {
                text: suf.to_string(),
                rc: tail,
                color: pal.muted,
                font_px: BADGE_FONT_PX,
                band: None,
            });
            head
        } else {
            lay.value
        };
        out.push(Run {
            text: row.value.clone(),
            rc: value_rect,
            color: if row.accent { pal.accent } else { pal.fg },
            font_px: FONT_PX,
            band: None,
        });
    }
    if c.paused {
        out.push(Run {
            text: "已暂停".to_string(),
            rc: render::badge_rect(w, scale),
            color: pal.warning,
            font_px: BADGE_FONT_PX,
            band: None,
        });
    }
    out
}

fn paint_band(buf: &mut [u8], w: i32, h: i32, rc: &Rect, c: Color) {
    for y in rc.y.max(0)..(rc.y + rc.h).min(h) {
        for x in rc.x.max(0)..(rc.x + rc.w).min(w) {
            render::put_premultiplied(buf, (y * w + x) as usize, c, c.a);
        }
    }
}

/// 一次完整重画并提交给 DWM。返回 false = 这一拍什么都没画出来（调用方出声）。
///
/// # Safety
/// 必须在拥有 `hwnd` 的那个线程上调用（GDI DC 与分层窗口位图都是线程绑定的）。
unsafe fn repaint(hwnd: HWND) -> bool {
    let cfg = focusflow_core::config::instance();
    let scale = scale_of(hwnd);
    let dark = cfg.get("gui", "theme") == "dark";
    let pal = palette_for(if dark { "dark" } else { "light" });
    let content = current_content();

    let cfg_w = cfg.get_float("floating", "width", 90.0).max(40.0) as i32;
    let cfg_h = cfg.get_float("floating", "height", 46.0).max(24.0) as i32;
    let need_h = render::content_height(content.rows.len(), content.notice.is_some());
    // 高度取"配置值与内容需要值"的大者：`metric=both` 或通知带出现时多一行，
    // 而分层窗口没有滚动条 —— 画不下就是看不见。
    let w = render::px(cfg_w, scale).max(1);
    let h = render::px(cfg_h, scale).max(render::px(need_h, scale));

    let fp = fingerprint(&content, dark, w, h);
    let unchanged = *LAST_FP.lock().unwrap_or_else(|e| e.into_inner()) == fp;
    if unchanged {
        let mut rc = RECT::default();
        if get_rect(hwnd, &mut rc) && (rc.right - rc.left) == w && (rc.bottom - rc.top) == h {
            return true;
        }
    }

    // `GetDC` 在 0.61 里返回裸 HDC（失败是空句柄，不是 Err）—— 判错形状就会拿一个空 DC
    // 继续往下建 DC/位图，症状是"每一拍都画不上而日志什么都不说"。
    let screen = g::GetDC(Some(hwnd));
    if screen.0.is_null() {
        tracing::warn!("悬浮窗取不到屏幕 DC，这一拍没画上");
        return false;
    }
    let hdc = g::CreateCompatibleDC(Some(screen));
    if hdc.0.is_null() {
        let _ = g::ReleaseDC(Some(hwnd), screen);
        tracing::warn!("悬浮窗建内存 DC 失败，这一拍没画上");
        return false;
    }
    let Some(mut card) = Surface::new(hdc, w, h) else {
        let _ = g::DeleteDC(hdc);
        let _ = g::ReleaseDC(Some(hwnd), screen);
        tracing::warn!("悬浮窗建位图失败（尺寸 {w}×{h}），这一拍没画上");
        return false;
    };
    let Some(mut mask) = Surface::new(hdc, w, h) else {
        card.delete(hdc);
        let _ = g::DeleteDC(hdc);
        let _ = g::ReleaseDC(Some(hwnd), screen);
        tracing::warn!("悬浮窗建文字蒙版失败，这一拍没画上");
        return false;
    };
    let _ = g::SelectObject(hdc, g::HGDIOBJ(mask.bmp.0));

    // 1) 卡片底 + 1px 边框（都按圆角覆盖率裁，边框那一圈同样不能漏到角外）
    let border_px = render::px(BORDER, scale).max(1);
    {
        let buf = card.buf();
        for y in 0..h {
            for x in 0..w {
                let a = render::card_alpha_at(x, y, w, h, render::RADIUS, pal.card.a);
                if a == 0 {
                    continue;
                }
                let on_border =
                    x < border_px || y < border_px || x >= w - border_px || y >= h - border_px;
                let c = if on_border { pal.border } else { pal.card };
                render::put_premultiplied(buf, (y * w + x) as usize, c, a);
            }
        }
    }

    // 2) 文字。GDI 往 32bpp DIB 里画字只写 RGB、把 alpha 留在原值，所以不能直接往卡片上画
    //    （会得到一堆 alpha=0 的像素）。改成每段先画进"白字黑底"的蒙版，R 通道即覆盖度，
    //    再按覆盖度叠成目标色 —— 代价是抗锯齿只能用灰度，ClearType 的子像素在这条路上是彩边。
    for run in text_runs(&content, w, scale, pal) {
        if let Some(bg) = run.band {
            paint_band(card.buf(), w, h, &run.rc, bg);
        }
        mask.clear();
        let Some(font) = make_font(run.font_px, scale) else {
            tracing::warn!("悬浮窗建字体失败，这一段文字没画出来: {}", run.text);
            continue;
        };
        let prev_font = g::SelectObject(hdc, g::HGDIOBJ(font.0));
        // BACKGROUND_MODE(1) = wingdi.h 的 TRANSPARENT：不填背景，蒙版上没字的地方
        // 保持黑色（覆盖度 0）。留成 OPAQUE 会把整个矩形涂白，整行都变成"字"。
        let _ = g::SetBkMode(hdc, g::BACKGROUND_MODE(1));
        let _ = g::SetTextColor(hdc, COLORREF(0x00FF_FFFF));
        let mut rc = RECT {
            left: run.rc.x,
            top: run.rc.y,
            right: run.rc.x + run.rc.w,
            bottom: run.rc.y + run.rc.h,
        };
        let mut text = wide(&run.text);
        let _ = g::DrawTextW(
            hdc,
            &mut text,
            &raw mut rc,
            g::DRAW_TEXT_FORMAT(
                g::DT_SINGLELINE.0 | g::DT_VCENTER.0 | g::DT_NOPREFIX.0 | g::DT_END_ELLIPSIS.0,
            ),
        );
        let _ = g::SelectObject(hdc, prev_font);
        let _ = g::DeleteObject(g::HGDIOBJ(font.0));
        render::overlay_text_coverage(card.buf(), mask.buf(), run.color);
    }

    // 3) CSS `opacity:.55` 的语义：整组（含已经叠上去的字）等比缩。预乘格式下四个通道一起乘。
    if content.paused {
        render::scale_opacity(card.buf(), PAUSED_NUM, PAUSED_DEN);
    }

    // 交出去之前必须把卡片选回 DC：上面那段文字循环把蒙版选了上去，而蒙版的 alpha 全是 0
    // （GDI 画字不写 alpha，见上面第 2 步）。少这一句，`UpdateLayeredWindow` 提交的就是那张
    // 覆盖度层，`ULW_ALPHA` 下整面透明 —— 窗口在、置顶在、返回值成功，屏幕上却什么都没有。
    let _ = g::SelectObject(hdc, g::HGDIOBJ(card.bmp.0));

    let blend = g::BLENDFUNCTION {
        BlendOp: g::AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: g::AC_SRC_ALPHA as u8,
    };
    let size = SIZE { cx: w, cy: h };
    let src = POINT { x: 0, y: 0 };
    let ok = wm::UpdateLayeredWindow(
        hwnd,
        None,
        None,
        Some(&raw const size),
        Some(hdc),
        Some(&raw const src),
        COLORREF(0),
        Some(&raw const blend),
        wm::ULW_ALPHA,
    );

    let _ = g::SelectObject(hdc, g::HGDIOBJ(std::ptr::null_mut()));
    mask.delete(hdc);
    card.delete(hdc);
    let _ = g::DeleteDC(hdc);
    let _ = g::ReleaseDC(Some(hwnd), screen);

    if ok.is_err() {
        tracing::warn!("悬浮窗提交位图给 DWM 失败，这一拍没画上");
        return false;
    }
    *LAST_FP.lock().unwrap_or_else(|e| e.into_inner()) = fp;
    // 内容长高/缩回时窗口要跟着改（尺寸由内容决定，配置值只是下限）
    let mut rc = RECT::default();
    if get_rect(hwnd, &mut rc) && (rc.bottom - rc.top) != h {
        let _ = wm::SetWindowPos(
            hwnd,
            None,
            rc.left,
            rc.top,
            w,
            h,
            wm::SWP_NOACTIVATE | wm::SWP_NOZORDER,
        );
    }
    true
}

fn make_font(logical_px: i32, scale: f64) -> Option<g::HFONT> {
    unsafe {
        let mut lf: g::LOGFONTW = std::mem::zeroed();
        // 负高度 = 按字符高度（不含内部 Leading），与 CSS font-size 同口径
        lf.lfHeight = -render::px(logical_px, scale);
        lf.lfWeight = 400;
        lf.lfCharSet = g::DEFAULT_CHARSET;
        lf.lfQuality = g::ANTIALIASED_QUALITY;
        let face = wide("Microsoft YaHei UI");
        for (i, c) in face.iter().take(31).enumerate() {
            lf.lfFaceName[i] = *c;
        }
        let h = g::CreateFontIndirectW(&raw const lf);
        (!h.0.is_null()).then_some(h)
    }
}

/// 拖完之后把位置写回 `[floating] pos_x/pos_y`，**按逻辑像素**存 ——
/// 与前端 `floating.js:100` 那条 `outerPosition()/scaleFactor` 同一口径，
/// 两种渲染器才能共用同一份配置和同一个 `clamp_floating_into_screen`。
fn persist_position(hwnd: HWND) {
    unsafe {
        let mut rc = RECT::default();
        if !get_rect(hwnd, &mut rc) {
            tracing::warn!("悬浮窗拖完读不到位置，这次没写回配置");
            return;
        }
        let scale = scale_of(hwnd);
        let cfg = focusflow_core::config::instance();
        let mut failed = Vec::new();
        for (key, value) in [
            ("pos_x", (rc.left as f64 / scale).round().to_string()),
            ("pos_y", (rc.top as f64 / scale).round().to_string()),
        ] {
            if let Err(e) = cfg.set("floating", key, &value) {
                failed.push(format!("{key}={value}（{e}）"));
            }
        }
        if !failed.is_empty() {
            // 不写成功就是"下次启动卡片跳回老位置"——用户会以为是软件不稳，
            // 所以这一句必须点到"没写回配置"这件事。
            tracing::warn!(
                "悬浮窗位置没能写回配置：{}；下次启动会回到上次存下的位置",
                failed.join("、")
            );
        }
    }
}

/// 初始位置：配置里的逻辑坐标；没存过就落右上角附近（与 WebView2 那条路径
/// `screen_w - 120, 60` 同一意图，那条路径用的是逻辑像素）。
fn initial_position(scale: f64) -> (i32, i32) {
    let cfg = focusflow_core::config::instance();
    let x = cfg.get_float("floating", "pos_x", f64::NAN);
    let y = cfg.get_float("floating", "pos_y", f64::NAN);
    if x.is_nan() || y.is_nan() {
        let sw = unsafe { wm::GetSystemMetrics(wm::SM_CXSCREEN) }.max(1) as f64;
        (
            render::px((sw / scale - 120.0).max(0.0) as i32, scale),
            render::px(60, scale),
        )
    } else {
        (render::px(x as i32, scale), render::px(y as i32, scale))
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        wm::WM_TIMER => {
            // 隐藏期间不画：分层窗口没有可见像素，画了也没人看，而这一拍是每秒一次。
            // 重新显示时由 `set_visible` 补一次，用户不会看到旧数字。
            if is_visible() {
                let _ = repaint(hwnd);
            }
            LRESULT(0)
        }
        WM_APP_REFRESH => {
            // 藏起来的时候不画：等 `set_visible(true)` 再投一条，那时才是真要看的时刻。
            // 少这一问的话，改一次配置就在一个看不见的窗口上做一遍全表合成。
            if is_visible() {
                let _ = repaint(hwnd);
            }
            LRESULT(0)
        }
        // 整张卡片都可以拖：交回 HTCAPTION 让系统搬窗口，零 IPC。
        // 代价是普通鼠标消息不再进客户区，所以双击要挂在 WM_NCLBUTTONDBLCLK 上 ——
        // 挂在 WM_LBUTTONDBLCLK 上的话，"双击打开主界面"这条手势会静默失效。
        wm::WM_NCHITTEST => LRESULT(wm::HTCAPTION as isize),
        wm::WM_NCLBUTTONDBLCLK => {
            if let Some(app) = APP.get() {
                crate::state::show_main_window(app);
            }
            LRESULT(0)
        }
        wm::WM_WINDOWPOSCHANGING => {
            // 顶住"鼠标指向任务栏就被压下去"那一族：在被问到的这一刻直接答 topmost。
            // WebView2 那条路要靠每 500ms 一次 `SetWindowPos(HWND_TOPMOST)` 才做到同样的事，
            // 原生窗口不需要那个轮询线程。
            if let Some(wp) = (lparam.0 as *mut wm::WINDOWPOS).as_mut() {
                wp.hwndInsertAfter = wm::HWND_TOPMOST;
                wp.flags |= wm::SWP_NOACTIVATE;
            }
            LRESULT(0)
        }
        wm::WM_DPICHANGED => {
            // 系统给了按新缩放比换算过的建议矩形，照它搬一次；宽高由下一拍按内容重算
            if let Some(rc) = (lparam.0 as *const RECT).as_ref() {
                let _ = wm::SetWindowPos(
                    hwnd,
                    None,
                    rc.left,
                    rc.top,
                    rc.right - rc.left,
                    rc.bottom - rc.top,
                    wm::SWP_NOZORDER | wm::SWP_NOACTIVATE,
                );
            }
            let _ = repaint(hwnd);
            LRESULT(0)
        }
        wm::WM_EXITSIZEMOVE => {
            persist_position(hwnd);
            LRESULT(0)
        }
        wm::WM_ERASEBKGND => LRESULT(1),
        wm::WM_CLOSE => {
            HWND_RAW.store(0, Ordering::SeqCst);
            let _ = wm::DestroyWindow(hwnd);
            LRESULT(0)
        }
        wm::WM_DESTROY => {
            HWND_RAW.store(0, Ordering::SeqCst);
            wm::PostQuitMessage(0);
            LRESULT(0)
        }
        _ => wm::DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 起原生悬浮窗。返回时窗口已建好并交出了第一帧（或带着原因失败）。
///
/// `title` 与 WebView2 那条路径的窗口标题一致，供任务管理器/枚举窗口对账。
pub fn spawn(app: AppHandle, title: &str) -> Result<(), String> {
    if is_alive() {
        return Ok(());
    }
    let _ = APP.set(app);
    let class = wide(CLASS_NAME);
    let caption = wide(title);
    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);
    std::thread::Builder::new()
        .name("floating-native".into())
        .spawn(move || unsafe { run_thread(class, caption, &tx) })
        .map_err(|e| format!("启动悬浮窗线程失败：{e}"))?;
    rx.recv_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| format!("悬浮窗线程没有回话：{e}"))?
}

unsafe fn run_thread(class: Vec<u16>, caption: Vec<u16>, tx: &SyncSender<Result<(), String>>) {
    let hmodule = match GetModuleHandleW(None) {
        Ok(h) => h,
        Err(e) => {
            let _ = tx.send(Err(format!("取模块句柄失败：{e}")));
            return;
        }
    };
    let cursor = wm::LoadCursorW(None, wm::IDC_ARROW).unwrap_or_default();
    let wc = wm::WNDCLASSEXW {
        cbSize: std::mem::size_of::<wm::WNDCLASSEXW>() as u32,
        style: wm::CS_HREDRAW | wm::CS_VREDRAW,
        lpfnWndProc: Some(wndproc),
        hInstance: hmodule.into(),
        hCursor: cursor,
        lpszClassName: windows::core::PCWSTR(class.as_ptr()),
        ..Default::default()
    };
    if wm::RegisterClassExW(&raw const wc) == 0 {
        let _ = tx.send(Err(format!(
            "注册悬浮窗窗口类失败（最后错误 {}）",
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        )));
        return;
    }

    let hwnd = match wm::CreateWindowExW(
        wm::WINDOW_EX_STYLE(wm::WS_EX_LAYERED.0 | wm::WS_EX_TOPMOST.0 | wm::WS_EX_TOOLWINDOW.0),
        windows::core::PCWSTR(class.as_ptr()),
        windows::core::PCWSTR(caption.as_ptr()),
        wm::WS_POPUP,
        0,
        0,
        1,
        1,
        None,
        None,
        Some(hmodule.into()),
        None,
    ) {
        Ok(h) if !h.0.is_null() => h,
        Ok(_) => {
            let _ = tx.send(Err("CreateWindowExW 返回空句柄".to_string()));
            return;
        }
        Err(e) => {
            let _ = tx.send(Err(format!("创建原生悬浮窗失败：{e}")));
            return;
        }
    };
    HWND_RAW.store(hwnd.0 as isize, Ordering::SeqCst);

    let scale = scale_of(hwnd);
    let cfg = focusflow_core::config::instance();
    let w = render::px(
        cfg.get_float("floating", "width", 90.0).max(40.0) as i32,
        scale,
    );
    let h = render::px(
        cfg.get_float("floating", "height", 46.0).max(24.0) as i32,
        scale,
    );
    let (x, y) = initial_position(scale);
    // 位置与尺寸在第一帧之前摆好：否则用户会先看见它在左上角闪一下
    let _ = wm::SetWindowPos(hwnd, Some(wm::HWND_TOPMOST), x, y, w, h, wm::SWP_NOACTIVATE);
    let _ = wm::SetTimer(Some(hwnd), TIMER_ID, TICK_MS, None);
    let painted = repaint(hwnd);
    if !painted {
        // 第一帧就没画上：这张窗口留着也没用，而且**必须把句柄清掉** ——
        // 留着的话 façade 会以为卡片还在，之后每次 show 都在往一个没有消息循环的
        // 窗口发指令（调用方卡在系统调用里等那个已经不存在的线程）。
        HWND_RAW.store(0, Ordering::SeqCst);
        let _ = wm::KillTimer(Some(hwnd), TIMER_ID);
        let _ = wm::DestroyWindow(hwnd);
        let _ = tx.send(Err("原生悬浮窗建起来了但一帧都没画上".to_string()));
        return;
    }
    let _ = tx.send(Ok(()));

    let mut msg = wm::MSG::default();
    loop {
        let got = wm::GetMessageW(&raw mut msg, None, 0, 0);
        if got.0 == 0 {
            break; // WM_QUIT
        }
        if got.0 == -1 {
            tracing::error!("悬浮窗消息循环被中断（GetMessage 返回 -1），卡片停更");
            break;
        }
        let _ = wm::TranslateMessage(&raw const msg);
        let _ = wm::DispatchMessageW(&raw const msg);
    }
    let _ = wm::KillTimer(Some(hwnd), TIMER_ID);
    super::note_thread_exit();
}

/// 显示/隐藏。隐藏用 `SW_HIDE`，显示用 **`SW_SHOWNA`**：后者不抢前台 ——
/// 悬浮窗是"瞟一眼"的卡片，把它显示出来这件事不该把用户正在打字的应用的焦点抢走。
/// WebView2 那条路径靠 `.show()` 会抢，这是原生这一侧顺手改掉的小毛病。
pub fn set_visible(visible: bool) -> bool {
    let Some(h) = hwnd() else { return false };
    unsafe {
        let _ = wm::ShowWindow(h, if visible { wm::SW_SHOWNA } else { wm::SW_HIDE });
        if visible {
            // 隐藏期间不画（没有可见像素就没必要），这里催一次重画：
            // 否则重新显示时最长 1 秒看到的是隐藏前那一拍的旧数字。
            // 必须是**投消息**而不是在这里直接 repaint：这张窗口属于 `floating-native` 线程，
            // 内存 DC 与分层位图的提交都绑线程，从调用方直接画等于两个线程同时改
            // 同一张窗口表面的东西 —— 症状会是"偶尔显示出来是空的"，最难复现的那一类。
            let _ = wm::PostMessageW(Some(h), WM_APP_REFRESH, WPARAM(0), LPARAM(0));
        }
    }
    true
}

/// façade 在配置变化（主题/口径/尺寸/暂停）时催一次重画，而不是等下一拍。
/// 投消息不直接画：`repaint` 必须在拥有窗口的线程上跑（见 `WM_APP_REFRESH` 那段）。
pub fn refresh() {
    if let Some(h) = hwnd() {
        unsafe {
            let _ = wm::PostMessageW(Some(h), WM_APP_REFRESH, WPARAM(0), LPARAM(0));
        }
    }
}

/// 退出时收掉窗口（消息循环收到 WM_CLOSE 后自然退出）。
pub fn shutdown() {
    if let Some(h) = hwnd() {
        unsafe {
            let _ = wm::PostMessageW(Some(h), wm::WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

// 这几个 wrapper 只是为了让上面的代码少写 `unsafe {}` 块 —— 它们本身仍是 unsafe Win32 调用，
// 集中在这里也方便审计"到底碰了哪些用户32/Gdi 入口"。
unsafe fn get_rect(hwnd: HWND, rc: &mut RECT) -> bool {
    wm::GetWindowRect(hwnd, rc).is_ok()
}

#[cfg(test)]
mod wiring {
    /// 交给 DWM 的那一张必须是卡片本身，不是文字蒙版 —— 蒙版的 alpha 全是 0，
    /// 交错了症状是「窗口在、置顶在、调用返回成功，屏幕上却什么都没有」。
    #[test]
    fn the_card_is_selected_back_before_the_surface_is_submitted() {
        let src = include_str!("native.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let flat: String = prod
            .lines()
            .map(|l| l.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let mask_at = flat
            .find("g::SelectObject(hdc,g::HGDIOBJ(mask.bmp.0))")
            .expect("把文字蒙版选上 DC 那一步不见了");
        let card_at = flat
            .find("g::SelectObject(hdc,g::HGDIOBJ(card.bmp.0))")
            .expect("提交前把卡片选回 DC 那一步不见了");
        let ulw_at = flat
            .find("wm::UpdateLayeredWindow(")
            .expect("提交分层表面那一步不见了");
        assert!(
            card_at > mask_at,
            "卡片必须在蒙版之后选回去：最后选上去的那一张才是会被交出去的"
        );
        assert!(
            card_at < ulw_at,
            "提交之前没把卡片选回 DC ⇒ 交上去的是 alpha 全 0 的蒙版，卡片在屏幕上彻底不出现"
        );
    }
}
