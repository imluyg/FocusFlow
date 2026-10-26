//! 原生悬浮窗：分层窗口（`WS_EX_LAYERED`）+ `UpdateLayeredWindow` 逐像素 alpha。
//!
//! 为什么是一个**独立线程 + 独立窗口**：悬浮窗要跟着托盘常驻、主窗口收起时也在，
//! 它自己的消息循环（GDI DC 与分层位图都是线程绑定的）最省事。主窗口收起/退出都不会
//! 把它卡住，反过来它也不占主线程的一帧。
//!
//! 为什么是 GDI 而不是 Direct2D：卡片只有两行文字和一个圆角底，`UpdateLayeredWindow` 就够；
//! 主窗口那套 D3D11+DComp 在这里是杀鸡用牛刀（还得为一个小窗口再养一条交换链）。
//!
//! 这一份从 P7 删掉的 `desktop/src/floating/native.rs` 搬来，改了三处：
//! 1. 数据不再是 parked 的 `LiveStats`（原生端没有采集进程），改成主窗口读库后
//!    `publish()` 过来的三格（今日次数 / 今日时长 / 目标百分比），见 [`Live`]；
//! 2. 没有了暂停、久坐提醒、`速度`（都没有数据源），卡片改画「今日目标」；
//! 3. 双击卡片投的是本进程的 `WM_SHOW_MAIN`，不再是 Tauri 的 `show_main_window`。
//!
//! **点击策略**（旧版就是这个口径，本轮沿用）：整张卡片就是标题栏 ——
//! `WM_NCHITTEST` 一律回 `HTCAPTION`，按住任意位置即可拖动，双击叫出主窗口。
//! 不做 `WS_EX_TRANSPARENT` 式穿透：穿透之后卡片既拖不动也点不到，而收起来的入口
//! 只有托盘与设置页（确实要穿透的话，那是另一个"锁定"开关的活，不该默认开）。

use std::sync::atomic::{AtomicI32, AtomicIsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi as g;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging as wm;

use crate::floating_render as fr;
use fr::{Color, Palette, Rect};

/// 悬浮窗线程唯一的一份句柄。0 = 还没建起来或已经销毁。
static HWND_RAW: AtomicIsize = AtomicIsize::new(0);
/// 主窗口句柄：卡片上双击要把它叫出来。
static MAIN_RAW: AtomicIsize = AtomicIsize::new(0);
/// 用户放下的那一个 top，**逻辑像素**。卡片按内容长高时窗口矩形里的 top 会被往上让
/// （[`grown_top`]），收缩回去必须照这一格回，不能照当前矩形回。
static ANCHOR_TOP: AtomicI32 = AtomicI32::new(60);

/// 重画节拍。"瞟一眼"用的数字，1 秒够；主窗口那边是 2 秒一拍读库。
const TICK_MS: u32 = 1000;
const TIMER_ID: usize = 0xF0F1;
/// 让卡片线程立刻重画一次（改了配置之后用）。
///
/// 为什么不用 `InvalidateRect`：分层窗口的像素不是 `WM_PAINT` 画出来的，是
/// `UpdateLayeredWindow` 交上去的 —— 把区域标脏只会什么都不发生。而重画必须跑在
/// 拥有这个窗口的线程上，所以这里投一条消息。
const WM_APP_REFRESH: u32 = 0x8001;
const CLASS_NAME: &str = "FocusFlowFloatingPill";

/// `[floating] width` 的默认值（逻辑像素）。
///
/// 2026-10-27 从 104 加到 148：104 是照旧版「活跃 1,234 / 速度 45/分」量的，标签与值之间
/// 没有空隙；换成 16px 值 + 11px 标签之后，148 才让「14,114」和「3h 30m」都留得下边距。
/// 配置里存着旧默认 104 的，按 [`fr::MIN_W`] 抬到 136，不被省略号切掉。
const DEFAULT_W: f64 = 148.0;
/// `[floating] height` 的默认值（逻辑像素）。两行 60 是内容需要的高度，见 `content_height`。
const DEFAULT_H: f64 = 60.0;

/// 卡片要显示的三格数。由主窗口读库后推过来（[`publish`]）。
///
/// 为什么不让卡片自己查库：读库在每次刷新时已经做过一遍了（主窗口 2 秒一拍），
/// 再让卡片每秒查一次就是把同一份 SQL 跑两遍 —— 而悬浮窗的价值恰恰是"不额外花代价"。
#[derive(Clone, Copy, Default)]
pub struct Live {
    pub today: i64,
    pub duration_sec: i64,
    /// 今日目标完成百分比；`None` = 没设目标或没算出来（卡片上画「—」）。
    pub goal_pct: Option<i64>,
}

static LIVE: Mutex<Live> = Mutex::new(Live {
    today: 0,
    duration_sec: 0,
    goal_pct: None,
});

/// 主窗口每次刷新后调一次。第一条快照到之前卡片显示 0 / —（不画空白）。
pub fn publish(live: Live) {
    if let Ok(mut g) = LIVE.lock() {
        *g = live;
    }
}

fn live_now() -> Live {
    LIVE.lock().map(|g| *g).unwrap_or_default()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn scale_of(hwnd: HWND) -> f64 {
    unsafe { (GetDpiForWindow(hwnd).max(96)) as f64 / 96.0 }
}

fn hwnd() -> Option<HWND> {
    let raw = HWND_RAW.load(Ordering::SeqCst);
    (raw != 0).then_some(HWND(raw as *mut core::ffi::c_void))
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

/// 一块 32bpp 顶向 DIB。`bits` 指向预乘 BGRA，长度 `w*h*4`。
struct Surface {
    bmp: g::HBITMAP,
    bits: *mut u8,
    w: i32,
    h: i32,
}

impl Surface {
    /// # Safety
    /// `hdc` 必须是有效的内存 DC；返回的 `bits` 由 GDI 拥有，必须在本对象删除之前
    /// 先从 DC 上摘下来。
    unsafe fn new(hdc: g::HDC, w: i32, h: i32) -> Option<Self> {
        let info = g::BITMAPINFO {
            bmiHeader: g::BITMAPINFOHEADER {
                biSize: std::mem::size_of::<g::BITMAPINFOHEADER>() as u32,
                biWidth: w,
                // 负高度 = 顶向：第 0 行就是屏幕最上面那一行，与版面的 y 轴同向
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: g::BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let bmp = g::CreateDIBSection(Some(hdc), &raw const info, g::DIB_RGB_COLORS, &mut bits, None, 0)
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

    /// # Safety
    /// 创建它的那个 DC 还活着时调用（GDI 只要求对象已经不在 DC 上）。
    unsafe fn delete(&mut self) {
        let _ = g::DeleteObject(g::HGDIOBJ(self.bmp.0));
        self.bits = std::ptr::null_mut();
    }
}

fn palette_now() -> Palette {
    let theme = focusflow_core::config::instance().get("gui", "theme");
    fr::palette_for(&theme)
}

/// 把三格数拼成卡片内容。
fn current_content() -> fr::PillContent {
    let cfg = focusflow_core::config::instance();
    let metric = fr::metric_from(&cfg.get("gui", "floating_metric"));
    let live = live_now();
    fr::build_content(metric, live.today, live.duration_sec, live.goal_pct)
}

/// 一段要画的文字：内容、矩形、颜色、字号、字体、是否右对齐。
struct Run {
    text: String,
    rc: Rect,
    color: Color,
    font_px: i32,
    face: &'static str,
    weight: i32,
    right: bool,
}

fn text_runs(c: &fr::PillContent, w: i32, scale: f64, pal: Palette) -> Vec<Run> {
    let mut out = Vec::new();
    let rows = fr::layout_rows(w, scale, c.rows.len());
    for (row, lay) in c.rows.iter().zip(rows.iter()) {
        // 标签用雅黑（中文），值用 Segoe UI Semibold（数字/单位）—— 一格里两种字，
        // 中文标签小、数字大且右对齐，扫一眼先看到数字。
        out.push(Run {
            text: row.label.to_string(),
            rc: lay.label,
            color: pal.muted,
            font_px: fr::LABEL_PX,
            face: "Microsoft YaHei UI",
            weight: 400,
            right: false,
        });
        out.push(Run {
            text: row.value.clone(),
            rc: lay.value,
            color: if row.accent { pal.accent } else { pal.fg },
            font_px: fr::VALUE_PX,
            face: "Segoe UI",
            weight: 600,
            right: true,
        });
    }
    out
}

unsafe fn make_font(
    logical_px: i32,
    scale: f64,
    face: &str,
    weight: i32,
) -> Option<g::HFONT> {
    unsafe {
        let mut lf: g::LOGFONTW = std::mem::zeroed();
        // 负高度 = 按字符高度（不含内部 Leading），与 CSS font-size 同口径
        lf.lfHeight = -fr::px(logical_px, scale);
        lf.lfWeight = weight;
        lf.lfCharSet = g::DEFAULT_CHARSET;
        lf.lfQuality = g::ANTIALIASED_QUALITY;
        let face = wide(face);
        for (i, c) in face.iter().take(31).enumerate() {
            lf.lfFaceName[i] = *c;
        }
        let h = g::CreateFontIndirectW(&raw const lf);
        (!h.0.is_null()).then_some(h)
    }
}

/// 一次完整重画并提交给 DWM。返回 false = 这一拍什么都没画出来（调用方出声）。
///
/// # Safety
/// 必须在拥有 `hwnd` 的那个线程上调用。
unsafe fn repaint(hwnd: HWND) -> bool {
    unsafe {
        let cfg = focusflow_core::config::instance();
        let scale = scale_of(hwnd);
        let pal = palette_now();
        let content = current_content();

        let cfg_w = cfg
            .get_float("floating", "width", DEFAULT_W)
            .max(fr::MIN_W as f64) as i32;
        // 高度取「配置值」与「内容需要值」的大者：`metric=both` 多一行，
        // 而分层窗口没有滚动条 —— 画不下就是看不见。
        let h_logical = fr::pill_height(cfg.get_float("floating", "height", DEFAULT_H), content.rows.len());
        let w = fr::px(cfg_w, scale).max(1);
        let h = fr::px(h_logical, scale).max(1);

        // `GetDC` 在 0.61 里返回裸 HDC（失败是空句柄，不是 Err）。
        let screen = g::GetDC(Some(hwnd));
        if screen.0.is_null() {
            crate::tracing_note("悬浮窗取不到屏幕 DC，这一拍没画上");
            return false;
        }
        let hdc = g::CreateCompatibleDC(Some(screen));
        if hdc.0.is_null() {
            let _ = g::ReleaseDC(Some(hwnd), screen);
            crate::tracing_note("悬浮窗建内存 DC 失败，这一拍没画上");
            return false;
        }
        let Some(mut card) = Surface::new(hdc, w, h) else {
            let _ = g::DeleteDC(hdc);
            let _ = g::ReleaseDC(Some(hwnd), screen);
            crate::tracing_note(&format!("悬浮窗建位图失败（尺寸 {w}×{h}），这一拍没画上"));
            return false;
        };
        let Some(mut mask) = Surface::new(hdc, w, h) else {
            card.delete();
            let _ = g::DeleteDC(hdc);
            let _ = g::ReleaseDC(Some(hwnd), screen);
            crate::tracing_note("悬浮窗建文字蒙版失败，这一拍没画上");
            return false;
        };
        let _ = g::SelectObject(hdc, g::HGDIOBJ(mask.bmp.0));

        // 1) 卡片底 + 1px 边框。两层圆角覆盖率：外层 = 整张卡，内层 = 缩进一个边框，
        //    两者之差就是边框那一圈 —— 四个角因此是抗锯齿的，不再是 1px 台阶。
        let border_px = fr::px(fr::BORDER, scale).max(1);
        let radius_px = fr::px(fr::RADIUS, scale);
        {
            let buf = card.buf();
            let (iw, ih) = (w - border_px * 2, h - border_px * 2);
            let inner_radius = (radius_px - border_px).max(0);
            for y in 0..h {
                for x in 0..w {
                    let cov_out = fr::rounded_rect_coverage(x, y, w, h, radius_px);
                    if cov_out == 0 {
                        continue;
                    }
                    let cov_in = if iw > 0 && ih > 0 {
                        fr::rounded_rect_coverage(
                            x - border_px,
                            y - border_px,
                            iw,
                            ih,
                            inner_radius,
                        )
                    } else {
                        0
                    };
                    let idx = (y * w + x) as usize;
                    if cov_in > 0 {
                        let a = (cov_in as u16 * pal.card.a as u16 / 255) as u8;
                        fr::put_premultiplied(buf, idx, pal.card, a);
                    }
                    let ring = cov_out.saturating_sub(cov_in);
                    if ring > 0 {
                        let a = (ring as u16 * pal.border.a as u16 / 255) as u16;
                        fr::over(
                            buf,
                            idx,
                            pal.border.r as u16 * a / 255,
                            pal.border.g as u16 * a / 255,
                            pal.border.b as u16 * a / 255,
                            a,
                        );
                    }
                }
            }
        }

        // 2) 文字。GDI 往 32bpp DIB 里画字只写 RGB、把 alpha 留在原值，所以不能直接往卡片上画
        //    （会得到一堆 alpha=0 的像素）。改成每段先画进"白字黑底"的蒙版，R 通道即覆盖度，
        //    再按覆盖度叠成目标色 —— 代价是抗锯齿只能用灰度。
        for run in text_runs(&content, w, scale, pal) {
            mask.buf().fill(0);
            let Some(font) = make_font(run.font_px, scale, run.face, run.weight) else {
                crate::tracing_note(&format!("悬浮窗建字体失败，这一段没画出来: {}", run.text));
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
            // 注意：这里的文本**不能**带结尾 NUL。`DrawTextW` 按切片长度认字符数，
            // 多送一个 0 就是多一个字符 —— 它会占掉一个字形宽度，把本来刚好放得下的
            // 「活跃」「14,114」挤成「活…」「14,11…」，还在末尾画出一个方框。
            let mut text: Vec<u16> = run.text.encode_utf16().collect();
            let mut flags = g::DT_SINGLELINE.0
                | g::DT_VCENTER.0
                | g::DT_NOPREFIX.0
                | g::DT_END_ELLIPSIS.0;
            if run.right {
                flags |= g::DT_RIGHT.0;
            }
            let _ = g::DrawTextW(hdc, &mut text, &raw mut rc, g::DRAW_TEXT_FORMAT(flags));
            let _ = g::SelectObject(hdc, prev_font);
            let _ = g::DeleteObject(g::HGDIOBJ(font.0));
            fr::overlay_text_coverage(card.buf(), mask.buf(), run.color);
        }

        // 交出去之前必须把卡片选回 DC：上面那段文字循环把蒙版选了上去，而蒙版的 alpha 全是 0。
        // 少这一句，`UpdateLayeredWindow` 提交的就是那张覆盖度层，`ULW_ALPHA` 下整面透明 ——
        // 窗口在、置顶在、返回值成功，屏幕上却什么都没有。
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
        mask.delete();
        card.delete();
        let _ = g::DeleteDC(hdc);
        let _ = g::ReleaseDC(Some(hwnd), screen);

        if ok.is_err() {
            crate::tracing_note("悬浮窗提交位图给 DWM 失败，这一拍没画上");
            return false;
        }

        // 尺寸由内容决定（配置值只是下限），长高/缩回时窗口要跟着改，而且**改的这一刻就要安放**。
        if let Some(rc) = get_rect(hwnd) {
            let anchor = ANCHOR_TOP.load(Ordering::SeqCst);
            // 按**锚点**那一行问屏幕，不按当前矩形：搬一次之后再读回矩形，等于让上一次的结果
            // 决定下一次的输入。锚点只由建窗与拖完写，所以同一份内容 + 同一个锚点永远算出同一个 top。
            let mon = nearest_monitor(rc.left, fr::px(anchor, scale));
            let top = grown_top(anchor, scale, h, &mon);
            if (rc.bottom - rc.top) != h || rc.right - rc.left != w || rc.top != top {
                let _ = wm::SetWindowPos(
                    hwnd,
                    None,
                    rc.left,
                    top,
                    w,
                    h,
                    wm::SWP_NOACTIVATE | wm::SWP_NOZORDER,
                );
            }
        }
        true
    }
}

unsafe fn get_rect(hwnd: HWND) -> Option<RECT> {
    let mut rc = RECT::default();
    unsafe { wm::GetWindowRect(hwnd, &mut rc).is_ok() }.then_some(rc)
}

/// 一块屏幕的整块矩形（不是工作区：判据用整屏，理由在 `clamp_into_screen` 上）。
#[derive(Clone, Copy)]
struct Mon {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

unsafe fn monitor_at(px_x: i32, px_y: i32, flags: g::MONITOR_FROM_FLAGS) -> Option<Mon> {
    unsafe {
        let hmon = g::MonitorFromPoint(POINT { x: px_x, y: px_y }, flags);
        // MONITORINFO 靠 cbSize 自报版本，没有可用的 Default，先清零。
        let mut mi: g::MONITORINFO = std::mem::zeroed();
        mi.cbSize = core::mem::size_of::<g::MONITORINFO>() as u32;
        if hmon.is_invalid() || !g::GetMonitorInfoW(hmon, &raw mut mi).as_bool() {
            return None;
        }
        Some(Mon {
            x: mi.rcMonitor.left,
            y: mi.rcMonitor.top,
            w: (mi.rcMonitor.right - mi.rcMonitor.left).max(1),
            h: (mi.rcMonitor.bottom - mi.rcMonitor.top).max(1),
        })
    }
}

/// 离这个物理坐标最近的那块屏；拿不到就退回主屏尺寸。
fn nearest_monitor(px_x: i32, px_y: i32) -> Mon {
    unsafe { monitor_at(px_x, px_y, g::MONITOR_DEFAULTTONEAREST) }.unwrap_or_else(|| unsafe {
        Mon {
            x: 0,
            y: 0,
            w: wm::GetSystemMetrics(wm::SM_CXSCREEN).max(1),
            h: wm::GetSystemMetrics(wm::SM_CYSCREEN).max(1),
        }
    })
}

/// 存过的坐标该落在哪：物理落点 `(x, y)` 与逻辑锚点 top。
fn landed_position(x: f64, y: f64, scale: f64, mon: &Mon, win_w: f64, win_h: f64) -> (i32, i32, i32) {
    let (cx, cy) = fr::clamp_into_screen(
        x,
        y,
        &fr::Bounds {
            left: mon.x as f64 / scale,
            top: mon.y as f64 / scale,
            screen_w: mon.w as f64 / scale,
            screen_h: mon.h as f64 / scale,
            win_w,
            win_h,
        },
    );
    // 逻辑像素取整再乘倍数：`x as i32` 那种截断会让 1.5 倍屏上的落点比夹取值少半像素。
    let anchor = cy.round() as i32;
    (fr::px(cx.round() as i32, scale), fr::px(anchor, scale), anchor)
}

/// 内容长高之后，这张卡片的 top 该落在哪（物理像素）。
fn grown_top(anchor: i32, scale: f64, height: i32, mon: &Mon) -> i32 {
    fr::fit_top(
        fr::px(anchor, scale) as f64,
        height as f64,
        mon.y as f64,
        (mon.y + mon.h) as f64,
    ) as i32
}

/// 初始位置：配置里的逻辑坐标；没存过就落右上角附近。
fn initial_position(scale: f64) -> (i32, i32, i32) {
    let cfg = focusflow_core::config::instance();
    let x = cfg.get_float("floating", "pos_x", f64::NAN);
    let y = cfg.get_float("floating", "pos_y", f64::NAN);
    if x.is_nan() || y.is_nan() {
        let sw = unsafe { wm::GetSystemMetrics(wm::SM_CXSCREEN) }.max(1) as f64;
        (
            fr::px(
                (sw / scale - (DEFAULT_W + 16.0)).max(0.0) as i32,
                scale,
            ),
            fr::px(60, scale),
            60,
        )
    } else {
        // 存过的坐标必须过一遍夹取：拔掉副屏再启动时那对数字可能整张落在屏幕之外，
        // 而卡片看不见就拖不到，界面上没有别的入口能把它召回来。
        let mon = nearest_monitor(fr::px(x.round() as i32, scale), fr::px(y.round() as i32, scale));
        let metric = fr::metric_from(&cfg.get("gui", "floating_metric"));
        let win_w = cfg.get_float("floating", "width", DEFAULT_W).max(40.0);
        let win_h =
            fr::pill_height(cfg.get_float("floating", "height", DEFAULT_H), fr::row_count(metric)) as f64;
        landed_position(x, y, scale, &mon, win_w, win_h)
    }
}

/// 算出来的落点与现在这一刻差多少才算"真的需要搬"。
///
/// 落点要经过一次「物理→逻辑→取整→物理」的往返，倍数不是 1.0 时来回不一定严格相等；
/// 差一个像素就搬，等于每拍挪一像素的慢速漂移 —— 那正是"卡片自己跑位"。
fn pulled_position(cur_x: i32, cur_y: i32, want_x: i32, want_y: i32) -> (i32, i32) {
    if (want_x - cur_x).abs() >= 2 || (want_y - cur_y).abs() >= 2 {
        (want_x, want_y)
    } else {
        (cur_x, cur_y)
    }
}

/// 卡片现在这个位置还看得见吗？看不见（或只剩一条边）就整张搬回最近那块屏。
///
/// 启动时那一次夹取只保得住启动那一刻：运行期拔掉那块屏，卡片就停在旧坐标上。
/// 这条可以每拍都跑 —— 已经"看得见"的位置原样返回，不会把用户放好的位置挪走。
///
/// # Safety
/// 必须在拥有 `hwnd` 的那个线程上调用。
unsafe fn ensure_visible_on_screen(hwnd: HWND) -> bool {
    unsafe {
        let Some(rc) = get_rect(hwnd) else { return false };
        let scale = scale_of(hwnd);
        let w = rc.right - rc.left;
        let h = rc.bottom - rc.top;
        let mon = nearest_monitor(rc.left, rc.top);
        let (x, y, _) = landed_position(
            rc.left as f64 / scale,
            rc.top as f64 / scale,
            scale,
            &mon,
            w as f64 / scale,
            h as f64 / scale,
        );
        let (x, y) = pulled_position(rc.left, rc.top, x, y);
        if x == rc.left && y == rc.top {
            return false;
        }
        let _ = wm::SetWindowPos(
            hwnd,
            None,
            x,
            y,
            0,
            0,
            wm::SWP_NOSIZE | wm::SWP_NOZORDER | wm::SWP_NOACTIVATE,
        );
        crate::tracing_note(&format!(
            "悬浮窗原本整个在屏幕之外（{}x{}），已搬回 ({},{})",
            rc.left, rc.top, x, y
        ));
        true
    }
}

/// 拖完之后把位置写回 `[floating] pos_x/pos_y`，**按逻辑像素**存（与旧版同一口径）。
fn persist_position(hwnd: HWND) {
    unsafe {
        let Some(rc) = get_rect(hwnd) else {
            crate::tracing_note("悬浮窗拖完读不到位置，这次没写回配置");
            return;
        };
        let scale = scale_of(hwnd);
        // 拖完之后锚点就是眼前这一个矩形：用户搬的是他看见的卡片。
        ANCHOR_TOP.store((rc.top as f64 / scale).round() as i32, Ordering::SeqCst);
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
            // 不写成功就是"下次启动卡片跳回老位置"——用户会以为是软件不稳。
            crate::tracing_note(&format!(
                "悬浮窗位置没能写回配置：{}；下次启动会回到上次存下的位置",
                failed.join("、")
            ));
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            wm::WM_TIMER => {
                // 隐藏期间不画：分层窗口没有可见像素，画了也没人看。
                // 重新显示时由 `set_visible` 补一次，用户不会看到旧数字。
                if is_visible() {
                    let _ = repaint(hwnd);
                }
                LRESULT(0)
            }
            WM_APP_REFRESH => {
                if is_visible() {
                    let _ = repaint(hwnd);
                }
                LRESULT(0)
            }
            // 整张卡片都可以拖：交回 HTCAPTION 让系统搬窗口，零自绘拖动代码。
            // 代价是普通鼠标消息不再进客户区，所以双击要挂在 WM_NCLBUTTONDBLCLK 上。
            wm::WM_NCHITTEST => LRESULT(wm::HTCAPTION as isize),
            wm::WM_NCLBUTTONDBLCLK => {
                let main = MAIN_RAW.load(Ordering::SeqCst);
                if main != 0 {
                    let target = HWND(main as *mut core::ffi::c_void);
                    let _ = wm::PostMessageW(Some(target), crate::WM_SHOW_MAIN, WPARAM(0), LPARAM(0));
                }
                LRESULT(0)
            }
            wm::WM_WINDOWPOSCHANGING => {
                // 顶住"鼠标指向任务栏就被压下去"那一族：在被问到的这一刻直接答 topmost。
                if let Some(wp) = (lparam.0 as *mut wm::WINDOWPOS).as_mut() {
                    wp.hwndInsertAfter = wm::HWND_TOPMOST;
                    wp.flags |= wm::SWP_NOACTIVATE;
                }
                LRESULT(0)
            }
            // 屏幕拓扑变了（拔副屏、改分辨率、扩展桌面）：卡片停在旧坐标上就可能整张在屏幕之外。
            wm::WM_DISPLAYCHANGE => {
                let _ = ensure_visible_on_screen(hwnd);
                let _ = repaint(hwnd);
                LRESULT(0)
            }
            wm::WM_DPICHANGED => {
                // 系统给了按新缩放比换算过的建议矩形，照它搬一次；宽高由下一拍按内容重算。
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
}

/// 起原生悬浮窗。返回时窗口已建好并交出了第一帧（或带着原因失败）。
pub fn spawn(main: HWND) -> Result<(), String> {
    if is_alive() {
        return Ok(());
    }
    MAIN_RAW.store(main.0 as isize, Ordering::SeqCst);
    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);
    std::thread::Builder::new()
        .name("floating-native".into())
        .spawn(move || unsafe { run_thread(&tx) })
        .map_err(|e| format!("启动悬浮窗线程失败：{e}"))?;
    rx.recv_timeout(Duration::from_secs(5))
        .map_err(|e| format!("悬浮窗线程没有回话：{e}"))?
}

unsafe fn run_thread(tx: &std::sync::mpsc::SyncSender<Result<(), String>>) {
    unsafe {
        let hmodule = match GetModuleHandleW(None) {
            Ok(h) => h,
            Err(e) => {
                let _ = tx.send(Err(format!("取模块句柄失败：{e}")));
                return;
            }
        };
        let cursor = wm::LoadCursorW(None, wm::IDC_ARROW).unwrap_or_default();
        let class = wide(CLASS_NAME);
        let caption = wide("FocusFlow 悬浮窗");
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
        let (x, y, anchor) = initial_position(scale);
        ANCHOR_TOP.store(anchor, Ordering::SeqCst);
        // 位置与尺寸在第一帧之前摆好：否则用户会先看见它在左上角闪一下。
        let _ = wm::SetWindowPos(hwnd, Some(wm::HWND_TOPMOST), x, y, 1, 1, wm::SWP_NOACTIVATE);
        let _ = wm::SetTimer(Some(hwnd), TIMER_ID, TICK_MS, None);
        let painted = repaint(hwnd);
        if !painted {
            // 第一帧就没画上：这张窗口留着也没用，而且**必须把句柄清掉** ——
            // 留着的话外面会以为卡片还在，每次 show 都在往一个没有消息循环的窗口发指令。
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
                crate::tracing_note("悬浮窗消息循环被中断（GetMessage 返回 -1），卡片停更");
                break;
            }
            let _ = wm::TranslateMessage(&raw const msg);
            let _ = wm::DispatchMessageW(&raw const msg);
        }
        let _ = wm::KillTimer(Some(hwnd), TIMER_ID);
    }
}

/// 显示/隐藏。显示用 **`SW_SHOWNA`**：悬浮窗是"瞟一眼"的卡片，显示它不该把
/// 用户正在打字的应用的焦点抢走。
pub fn set_visible(visible: bool) -> bool {
    let Some(h) = hwnd() else { return false };
    unsafe {
        let _ = wm::ShowWindow(h, if visible { wm::SW_SHOWNA } else { wm::SW_HIDE });
        if visible {
            // 隐藏期间不画（见 WM_TIMER），这里催一次重画：否则重新显示时最长 1 秒
            // 看到的是隐藏前那一拍的旧数字。必须是**投消息**而不是在这里直接 repaint ——
            // 这张窗口属于 `floating-native` 线程，内存 DC 与分层位图的提交都绑线程，
            // 从调用方直接画等于两个线程同时改同一张窗口表面。
            let _ = wm::PostMessageW(Some(h), WM_APP_REFRESH, WPARAM(0), LPARAM(0));
        }
    }
    true
}

/// 让卡片立刻重画（改了 `[gui] theme` / `[floating] *` 之后用）。
pub fn refresh() {
    let Some(h) = hwnd() else { return };
    unsafe {
        let _ = wm::PostMessageW(Some(h), WM_APP_REFRESH, WPARAM(0), LPARAM(0));
    }
}

/// 退出前把窗口关掉（消息循环随之结束，线程自然退出）。
pub fn shutdown() {
    let Some(h) = hwnd() else { return };
    unsafe {
        let _ = wm::PostMessageW(Some(h), wm::WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}
