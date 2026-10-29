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

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicI32, AtomicIsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory, ID2D1SolidColorBrush,
    D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT,
    D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_RENDER_TARGET_USAGE_NONE,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
    DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING,
    DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_TEXT_METRICS, DWRITE_WORD_WRAPPING_NO_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi as g;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging as wm;
use windows_numerics::Vector2;

use crate::floating_render as fr;
use fr::{Color, Palette, Rect};

/// 悬浮窗线程唯一的一份句柄。0 = 还没建起来或已经销毁。
static HWND_RAW: AtomicIsize = AtomicIsize::new(0);
/// 主窗口句柄：卡片上双击要把它叫出来。
static MAIN_RAW: AtomicIsize = AtomicIsize::new(0);
/// 用户放下的那一个 top，**逻辑像素**。卡片按内容长高时窗口矩形里的 top 会被往上让
/// （[`grown_top`]），收缩回去必须照这一格回，不能照当前矩形回。
static ANCHOR_TOP: AtomicI32 = AtomicI32::new(60);
/// 最近一次**我们自己**摆出去的 top，物理像素（取 `i32::MIN` 表示"还没摆过"）。
///
/// 只有它才能把"矩形里的 top 与锚点差一格"分成两件事：差成这一格 ⇒ 是长高让位，下一拍要照锚点回；
/// 差成别的任何一格 ⇒ 是别人搬的（用户拖拽、别的程序搬窗、DPI 换算给的矩形）—— 那种时候跟它较劲，
/// 用户看见的就是"卡片拖到一半自己跳回原高度"。
static APPLIED_TOP: AtomicI32 = AtomicI32::new(i32::MIN);

/// 重画节拍。以前写 1 秒的理由是"瞟一眼用的数字，1 秒够"，但那是在今日那一格还跟着
/// 快照线程每 2 秒读库的时候量的：现在那一格当场问原子计数，这个定时器就是**唯一**的
/// 延迟来源，于是它成了全部（0–1 秒，平均半秒）。收到 500ms 的代价是每半秒一次
/// 小位图提交（约 200×48），换来的是"我打了字，卡片立刻动"。
///
/// ⚠ 这里**没有**"内容没变就跳过提交"那道闸：`repaint` 的尾段还要按当前尺寸重锚窗口
/// （拔屏/换缩放后把卡片找回来的那 2 像素门槛），跳过一拍就是跳过那次纠正。
/// 省下来的只是那点提交，赔进去的是"卡片跑位"那一族最难复现的缺陷。
const TICK_MS: u32 = 500;
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
/// 2026-10-27 定稿回旧 WebView 的 90（后来定 104）宽度：标签和值紧挨着、值左对齐，
/// 132 右边那一大截空白就不需要了。104 让「14,114」和「41/分」都留得下边距。
const DEFAULT_W: f64 = 104.0;
/// `[floating] height` 的默认值（逻辑像素）；运行时还会被任务栏高度抬到"至少一样高"。
const DEFAULT_H: f64 = 48.0;

/// 卡片要显示的三格数。由主窗口读库后推过来（[`publish`]）。
///
/// 为什么不让卡片自己查库：读库在每次刷新时已经做过一遍了（主窗口 2 秒一拍），
/// 再让卡片每秒查一次就是把同一份 SQL 跑两遍 —— 而悬浮窗的价值恰恰是"不额外花代价"。
#[derive(Clone, Copy, Default)]
pub struct Live {
    pub today: i64,
    pub duration_sec: i64,
}

static LIVE: Mutex<Live> = Mutex::new(Live {
    today: 0,
    duration_sec: 0,
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

/// 任务栏的逻辑高度（左右停靠时取厚度）。取不到就按 48（100% 缩放的默认高度）。
///
/// 卡片至少要和它一样高 —— 用户的原话是"至少和任务栏高度差不多"。用系统给的
/// `ABM_GETTASKBARPOS` 而不是写死 48：125%/150% 或"大任务栏"时也能对齐。
fn taskbar_height_logical(scale: f64) -> i32 {
    use windows::Win32::UI::Shell::{SHAppBarMessage, ABM_GETTASKBARPOS, APPBARDATA};
    unsafe {
        let mut data = APPBARDATA {
            cbSize: core::mem::size_of::<APPBARDATA>() as u32,
            ..Default::default()
        };
        if SHAppBarMessage(ABM_GETTASKBARPOS, &mut data) != 0 {
            // ABE_LEFT = 0、ABE_RIGHT = 2：竖着停靠时"厚度"是宽。
            let thickness = match data.uEdge {
                0 | 2 => data.rc.right - data.rc.left,
                _ => data.rc.bottom - data.rc.top,
            };
            let logical = (thickness as f64 / scale).round() as i32;
            if (24..=160).contains(&logical) {
                return logical;
            }
        }
    }
    48
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

/// 把三格数拼成卡片内容（活跃 / 时长 / 速度）。
fn current_content() -> fr::PillContent {
    let cfg = focusflow_core::config::instance();
    let metric = fr::metric_from(&cfg.get("gui", "floating_metric"));
    let live = live_now();
    // 今日这一格直接问采集侧那个原子计数：`LIVE` 里那份是快照线程每 2 秒推过来的缓存，
    // 而卡片是他**当场盯着看**的东西 —— 走缓存等于在 2 秒之外再叠一层延迟。
    // 采集没起来时（`None`）退回那份缓存，与主界面同一个口径，不另起一把尺子。
    let today = crate::collect::today_total().unwrap_or(live.today);
    fr::build_content(
        metric,
        today,
        live.duration_sec,
        crate::collect::is_started(),
        crate::collect::is_paused(),
        crate::collect::cpm(),
    )
}

/// 值后面的小字单位（速度行的 `/分`）：位置要等数字量完宽度才知道。
struct Suffix {
    text: String,
    color: Color,
    font_px: i32,
    face: &'static str,
}

/// 一段要画的文字：内容、矩形、颜色、字号、字体、是否右对齐，以及可选的小字单位。
struct Run {
    text: String,
    rc: Rect,
    color: Color,
    font_px: i32,
    face: &'static str,
    weight: i32,
    right: bool,
    suffix: Option<Suffix>,
}

/// 值用哪种字体：纯 ASCII（数字/`h m`/`%`/`—`）用 Segoe UI，中文（如「暂停」）用雅黑。
/// GDI 的字体链接在 13px 下不一定可靠，显式选一下更稳。
fn value_face(s: &str) -> &'static str {
    if s.is_ascii() {
        "Segoe UI"
    } else {
        "Microsoft YaHei UI"
    }
}

fn text_runs(c: &fr::PillContent, w: i32, scale: f64, pal: Palette) -> Vec<Run> {
    let mut out = Vec::new();
    let rows = fr::layout_rows(w, scale, c.rows.len());
    for (row, lay) in c.rows.iter().zip(rows.iter()) {
        // 标签用雅黑（中文），值用 Segoe UI —— 与旧 WebView 的
        // `--font: "Segoe UI", "Microsoft YaHei"` 同一套回退顺序，字号同为 13px。
        out.push(Run {
            text: row.label.to_string(),
            rc: lay.label,
            color: pal.muted,
            font_px: fr::LABEL_PX,
            face: "Microsoft YaHei UI",
            weight: 400,
            right: false,
            suffix: None,
        });
        out.push(Run {
            text: row.value.clone(),
            rc: lay.value,
            color: if row.accent { pal.accent } else { pal.fg },
            font_px: fr::VALUE_PX,
            face: value_face(&row.value),
            weight: 400,
            right: false,
            suffix: if row.suffix.is_empty() {
                None
            } else {
                // `41` 用 13px Segoe + 强调色，`/分` 用 11px 雅黑 muted。
                Some(Suffix {
                    text: row.suffix.to_string(),
                    color: pal.muted,
                    font_px: fr::LABEL_PX - 2,
                    face: "Microsoft YaHei UI",
                })
            },
        });
    }
    out
}

/// 悬浮窗文字用 DirectWrite 画（与旧 WebView 同一个渲染栈）。
///
/// 原来走 GDI：13px 在透明底上只有灰度 AA，比浏览器（DirectWrite）毛糙一档；而且
/// GDI 里标签写死 `Microsoft YaHei UI`，数字也归雅黑。现在统一 Segoe UI + 系统字体回退，
/// 中文照样落到雅黑，数字/英文与主界面、旧 WebView 一致。
struct TextCtx {
    rt: ID2D1DCRenderTarget,
    dwrite: IDWriteFactory,
    brush: ID2D1SolidColorBrush,
    /// 只为保活：DCRenderTarget 背后还引用着工厂。
    _factory: ID2D1Factory,
}

thread_local! {
    static TEXT: RefCell<Option<TextCtx>> = const { RefCell::new(None) };
    static TEXT_TRIED: Cell<bool> = const { Cell::new(false) };
}

/// 建一次（每个悬浮窗线程一份）。失败就退回 GDI 路径，不让"字画不出来"变成空卡片。
unsafe fn ensure_text_ctx() -> bool {
    unsafe {
        if TEXT_TRIED.with(|t| t.get()) {
            return TEXT.with(|c| c.borrow().is_some());
        }
        TEXT_TRIED.with(|t| t.set(true));
        let made = (|| -> windows::core::Result<TextCtx> {
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_IGNORE,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let rt = factory.CreateDCRenderTarget(&props)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let brush = rt.CreateSolidColorBrush(
                &D2D1_COLOR_F {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 1.0,
                },
                None,
            )?;
            Ok(TextCtx {
                rt,
                dwrite,
                brush,
                _factory: factory,
            })
        })()
        .ok();
        let ok = made.is_some();
        TEXT.with(|c| *c.borrow_mut() = made);
        crate::tracing_note(if ok {
            "悬浮窗文字：DirectWrite（Segoe UI，中文走系统回退）"
        } else {
            "悬浮窗文字：DirectWrite 起不来，退回 GDI"
        });
        ok
    }
}

/// 用 DirectWrite 把一行白字画进蒙版（黑底），返回行宽（物理像素）。
///
/// 字号与坐标都按物理像素传：渲染目标固定 96 dpi，1 DIP = 1 px。
#[allow(clippy::too_many_arguments)] // 目标尺寸 + 矩形 + 字号 + 字体 + 对齐：一行字的输入就是这些
unsafe fn dw_line(
    ctx: &TextCtx,
    hdc: g::HDC,
    w: i32,
    h: i32,
    text: &str,
    rc: Rect,
    font_px: i32,
    face: &str,
    right: bool,
) -> Option<f32> {
    unsafe {
        if rc.w <= 0 || rc.h <= 0 {
            return None;
        }
        ctx.rt
            .BindDC(
                hdc,
                &RECT {
                    left: 0,
                    top: 0,
                    right: w,
                    bottom: h,
                },
            )
            .ok()?;
        let family = wide(face);
        let locale = wide("zh-cn");
        let fmt: IDWriteTextFormat = ctx
            .dwrite
            .CreateTextFormat(
                windows::core::PCWSTR(family.as_ptr()),
                None,
                DWRITE_FONT_WEIGHT(400),
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                font_px as f32,
                windows::core::PCWSTR(locale.as_ptr()),
            )
            .ok()?;
        let _ = fmt.SetTextAlignment(if right {
            DWRITE_TEXT_ALIGNMENT_TRAILING
        } else {
            DWRITE_TEXT_ALIGNMENT_LEADING
        });
        let _ = fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
        let _ = fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
        let text16: Vec<u16> = text.encode_utf16().collect();
        let layout = ctx
            .dwrite
            .CreateTextLayout(&text16, &fmt, rc.w as f32, rc.h as f32)
            .ok()?;
        let mut metrics = DWRITE_TEXT_METRICS::default();
        layout.GetMetrics(&mut metrics).ok()?;

        ctx.rt.BeginDraw();
        ctx.rt.Clear(Some(&D2D1_COLOR_F {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        }));
        ctx.rt
            .SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        ctx.rt.DrawTextLayout(
            Vector2 {
                X: rc.x as f32,
                Y: rc.y as f32,
            },
            &layout,
            &ctx.brush,
            D2D1_DRAW_TEXT_OPTIONS_CLIP,
        );
        ctx.rt.EndDraw(None, None).ok()?;
        Some(metrics.widthIncludingTrailingWhitespace)
    }
}

/// DirectWrite 版本的整卡文字。返回 false 表示这一拍根本没画（上下文起不来）。
#[allow(clippy::too_many_arguments)] // 目标尺寸 + 蒙版 + 卡片 + 内容 + 缩放 + 调色板：一次整卡文字的输入
unsafe fn draw_text_dw(
    hdc: g::HDC,
    mask: &mut Surface,
    card: &mut Surface,
    content: &fr::PillContent,
    w: i32,
    h: i32,
    scale: f64,
    pal: Palette,
) {
    TEXT.with(|cell| {
        let Ok(cell) = cell.try_borrow() else {
            return;
        };
        let Some(ctx) = cell.as_ref() else {
            return;
        };
        for run in text_runs(content, w, scale, pal) {
            mask.buf().fill(0);
            let Some(adv) = dw_line(
                ctx,
                hdc,
                w,
                h,
                &run.text,
                run.rc,
                fr::px(run.font_px, scale),
                run.face,
                run.right,
            ) else {
                continue;
            };
            fr::overlay_text_coverage(card.buf(), mask.buf(), run.color);
            if let Some(sfx) = &run.suffix {
                let sx =
                    (run.rc.x + adv.round() as i32 + fr::px(1, scale)).min(run.rc.x + run.rc.w);
                let srect = Rect {
                    x: sx,
                    y: run.rc.y,
                    w: (run.rc.x + run.rc.w - sx).max(0),
                    h: run.rc.h,
                };
                mask.buf().fill(0);
                if dw_line(
                    ctx,
                    hdc,
                    w,
                    h,
                    &sfx.text,
                    srect,
                    fr::px(sfx.font_px, scale),
                    sfx.face,
                    false,
                )
                .is_some()
                {
                    fr::overlay_text_coverage(card.buf(), mask.buf(), sfx.color);
                }
            }
        }
    });
}

/// GDI 兜底：DirectWrite 上下文起不来时才走（老机器/极端权限）。
/// 画的仍是"白字黑底"蒙版，之后按 R 通道叠色 —— 抗锯齿只有灰度。
#[allow(clippy::too_many_arguments)] // 与 draw_text_dw 同一份输入（它不看不用高度，只为对称留着）
unsafe fn draw_text_gdi(
    hdc: g::HDC,
    mask: &mut Surface,
    card: &mut Surface,
    content: &fr::PillContent,
    w: i32,
    _h: i32,
    scale: f64,
    pal: Palette,
) {
    for run in text_runs(content, w, scale, pal) {
        let Some(font) = make_font(run.font_px, scale, run.face, run.weight) else {
            crate::tracing_note(&format!("悬浮窗建字体失败，这一段没画出来: {}", run.text));
            continue;
        };
        let adv = {
            mask.buf().fill(0);
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
            // 多送一个 0 就是多一个字符 —— 它会占掉一个字形宽度。
            let mut text: Vec<u16> = run.text.encode_utf16().collect();
            let mut adv = SIZE::default();
            let _ = g::GetTextExtentPoint32W(hdc, &text, &mut adv);
            let mut flags =
                g::DT_SINGLELINE.0 | g::DT_VCENTER.0 | g::DT_NOPREFIX.0 | g::DT_END_ELLIPSIS.0;
            if run.right {
                flags |= g::DT_RIGHT.0;
            }
            let _ = g::DrawTextW(hdc, &mut text, &raw mut rc, g::DRAW_TEXT_FORMAT(flags));
            let _ = g::SelectObject(hdc, prev_font);
            adv.cx
        };
        let _ = g::DeleteObject(g::HGDIOBJ(font.0));
        fr::overlay_text_coverage(card.buf(), mask.buf(), run.color);

        if let Some(sfx) = &run.suffix {
            let Some(sfont) = make_font(sfx.font_px, scale, sfx.face, 400) else {
                continue;
            };
            mask.buf().fill(0);
            let prev = g::SelectObject(hdc, g::HGDIOBJ(sfont.0));
            let _ = g::SetBkMode(hdc, g::BACKGROUND_MODE(1));
            let _ = g::SetTextColor(hdc, COLORREF(0x00FF_FFFF));
            let sx = (run.rc.x + adv + fr::px(1, scale)).min(run.rc.x + run.rc.w);
            let mut rc = RECT {
                left: sx,
                top: run.rc.y,
                right: run.rc.x + run.rc.w,
                bottom: run.rc.y + run.rc.h,
            };
            let mut text: Vec<u16> = sfx.text.encode_utf16().collect();
            let _ = g::DrawTextW(
                hdc,
                &mut text,
                &raw mut rc,
                g::DRAW_TEXT_FORMAT(
                    g::DT_SINGLELINE.0 | g::DT_VCENTER.0 | g::DT_NOPREFIX.0 | g::DT_END_ELLIPSIS.0,
                ),
            );
            let _ = g::SelectObject(hdc, prev);
            let _ = g::DeleteObject(g::HGDIOBJ(sfont.0));
            fr::overlay_text_coverage(card.buf(), mask.buf(), sfx.color);
        }
    }
}

unsafe fn make_font(logical_px: i32, scale: f64, face: &str, weight: i32) -> Option<g::HFONT> {
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

        // 尺寸只有一份式子：启动那一次钳位走同一个 `card_logical_size`。
        let (cfg_w, h_logical) = card_logical_size(
            cfg.get_float("floating", "width", DEFAULT_W),
            cfg.get_float("floating", "height", DEFAULT_H),
            content.rows.len(),
            taskbar_height_logical(scale),
        );
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
                        let a = ring as u16 * pal.border.a as u16 / 255;
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

        // 2) 文字。优先 DirectWrite（与旧 WebView 同一渲染栈）；上下文起不来再退回 GDI。
        //    两条路都画进"白字黑底"蒙版，R 通道即覆盖度，再按颜色叠到卡片上。
        if ensure_text_ctx() {
            draw_text_dw(hdc, &mut mask, &mut card, &content, w, h, scale, pal);
        } else {
            draw_text_gdi(hdc, &mut mask, &mut card, &content, w, h, scale, pal);
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
            // 判断本身在 `anchor_now` 里（那几格有单元测试盯着），这里只负责把它落到全局上。
            let chosen = anchor_now(rc.top, anchor, APPLIED_TOP.load(Ordering::SeqCst), scale);
            if chosen != anchor {
                ANCHOR_TOP.store(chosen, Ordering::SeqCst);
            }
            let anchor = chosen;
            // 按**锚点**那一行问屏幕，不按当前矩形：搬一次之后再读回矩形，等于让上一次的结果
            // 决定下一次的输入。锚点由建窗、拖完与"别人搬过"写，所以同一份内容 + 同一个锚点
            // 永远算出同一个 top。
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
                APPLIED_TOP.store(top, Ordering::SeqCst);
            }
            // 卡片与停靠条那一条重叠 ⇒ 每拍抢一次层级，**不碰坐标**。
            // 只答 `WM_WINDOWPOSCHANGING` 不够：没被问到的那一刻没人替我们答，而任务栏同为
            // topmost，它在我们的锚点之后被按过一次就会把我们盖在下面（21:00 现场报的那一种）。
            if top + h > mon.bar_bottom {
                keep_above_bar(hwnd);
            }
        }
        true
    }
}

unsafe fn get_rect(hwnd: HWND) -> Option<RECT> {
    let mut rc = RECT::default();
    unsafe { wm::GetWindowRect(hwnd, &mut rc).is_ok() }.then_some(rc)
}

/// 一块屏幕的**整屏**矩形（`MONITORINFO.rcMonitor`），物理像素，另带停靠条那一条的下边界。
///
/// 落点与"长高让位"都按整屏问；`bar_bottom` 只用来判断"这张卡片压没压在任务栏那一条上"，
/// 压着就改层级、不改坐标。
///
/// ⚠ 口径变更（2026-10-27 深夜他实测报的）：把卡片放到任务栏那一条上，它自己跑了 ⇒
/// "别乱动我放的位置"优先，"被任务栏盖住"那一格由 [`keep_above_bar`] 用层级解决。
/// 上一版把尺子换成工作区（`30aa624`）确实让卡片不再落进带里，代价是用户放好的位置会被
/// 启动与长高两条路悄悄抬走 —— 按现量撤回。没有停靠条的那块屏 `rcWork == rcMonitor`，
/// 所以 `bar_bottom` 那一格在那儿永远不会判成"压在栏上"。
#[derive(Clone, Copy)]
struct Mon {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    /// 工作区底边（物理像素）：停靠条占掉的那一条的下沿。
    bar_bottom: i32,
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
            bar_bottom: mi.rcWork.bottom,
        })
    }
}

/// 离这个物理坐标最近的那块屏的整屏矩形；拿不到就退回主屏尺寸。
///
/// 退回那条只有 `SM_CXSCREEN`/`SM_CYSCREEN`（都是整屏），拿不到工作区，于是 `bar_bottom`
/// 只能填成整屏底边 ⇒ 那一刻不会去抢层级，但**也不会搬坐标**——宁可少压一层，不要乱动位置。
fn nearest_monitor(px_x: i32, px_y: i32) -> Mon {
    unsafe { monitor_at(px_x, px_y, g::MONITOR_DEFAULTTONEAREST) }.unwrap_or_else(|| unsafe {
        let h = wm::GetSystemMetrics(wm::SM_CYSCREEN).max(1);
        Mon {
            x: 0,
            y: 0,
            w: wm::GetSystemMetrics(wm::SM_CXSCREEN).max(1),
            h,
            bar_bottom: h,
        }
    })
}

/// 存过的坐标该落在哪：物理落点 `(x, y)` 与逻辑锚点 top。
fn landed_position(
    x: f64,
    y: f64,
    scale: f64,
    mon: &Mon,
    win_w: f64,
    win_h: f64,
) -> (i32, i32, i32) {
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
    (
        fr::px(cx.round() as i32, scale),
        fr::px(anchor, scale),
        anchor,
    )
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

/// 这一拍该用哪一个锚点（**逻辑像素**）。把判断从 `repaint` 里提出来，好让这几格有用例盯着。
///
/// - **别人搬走了**：矩形里的 top 既不是锚点那一行、也不是我们自己上一拍摆出去的那一格
///   ⇒ 认它，锚点跟着人走。不认的后果就是第十场量到的"拖到一半卡片自己跳回原高度"。
/// - **长高让位**：那一格正是我们自己摆的（`applied`）⇒ 别认，否则缩回时回不到用户放的那一行 ——
///   这是 [`ANCHOR_TOP`] 存在的理由，也是这条判断唯一不能顺手放宽的地方。
/// - **差不到 2 像素**：物理→逻辑→物理来回取整的误差，不是搬动（与 `pulled_position` 同一口径，
///   放宽到 1 像素就等于承认每拍挪一格的慢速漂移）。
/// - `applied == i32::MIN`（还没摆过）⇒ 不认：分不清是谁动的，宁可照锚点。
fn anchor_now(cur_top: i32, anchor: i32, applied: i32, scale: f64) -> i32 {
    // 哨兵先挡掉：`cur_top - i32::MIN` 在 debug 里直接 panic、在 release 里**静默绕回**，
    // 绕回来的那个数看着像"差得很远"，于是这条判断就变成看运气的了。
    if applied == i32::MIN {
        return anchor;
    }
    // 差值按 i64 算：`cur_top` 与锚点/已摆值都来自外部（配置里的坐标、别的程序搬窗），
    // 两端相减可以超出 i32 —— 这里宁可算对，不要绕回。
    let d_anchor = cur_top as i64 - fr::px(anchor, scale) as i64;
    let d_applied = cur_top as i64 - applied as i64;
    if d_anchor.abs() >= 2 && d_applied.abs() >= 2 {
        (cur_top as f64 / scale).round() as i32
    } else {
        anchor
    }
}

/// 卡片的**逻辑**尺寸：重画那一拍与启动那一次钳位必须算同一个式子。
///
/// 两条路以前各写一遍自己的：宽的下限一边是 `MIN_W`（90）、一边是手写的 `40.0`；高的
/// 那一维一边抬到"至少和任务栏一样高"、一边根本没抬。后果是启动那次拿**较短**的高度去
/// 夹位置——大任务栏/缩放下卡片真高 64 而夹取按 48 算，存过的 `pos_y` 落在屏底以下，
/// 第一拍画出一截在屏外、下一拍真高度算出来才自己跳回去（正是「卡片自己跑位」那一族
/// 的形状），而手改 `width = 50` 的人每次启动都被按 40 而不是 90 夹。
fn card_logical_size(width_cfg: f64, height_cfg: f64, rows: usize, taskbar_h: i32) -> (i32, i32) {
    (
        width_cfg.max(fr::MIN_W as f64) as i32,
        // 高度取「配置值」与「内容需要值」的大者，再至少和任务栏一样高：两行内容 48
        // 正好对齐 100% 缩放下的任务栏，而分层窗口没有滚动条 —— 画不下就是看不见。
        fr::pill_height(height_cfg, rows).max(taskbar_h),
    )
}

/// 初始位置：配置里的逻辑坐标；没存过就落右上角附近。
fn initial_position(scale: f64) -> (i32, i32, i32) {
    let cfg = focusflow_core::config::instance();
    let x = cfg.get_float("floating", "pos_x", f64::NAN);
    let y = cfg.get_float("floating", "pos_y", f64::NAN);
    if x.is_nan() || y.is_nan() {
        let sw = unsafe { wm::GetSystemMetrics(wm::SM_CXSCREEN) }.max(1) as f64;
        (
            fr::px((sw / scale - (DEFAULT_W + 16.0)).max(0.0) as i32, scale),
            fr::px(60, scale),
            60,
        )
    } else {
        // 存过的坐标必须过一遍夹取：拔掉副屏再启动时那对数字可能整张落在屏幕之外，
        // 而卡片看不见就拖不到，界面上没有别的入口能把它召回来。
        let mon = nearest_monitor(
            fr::px(x.round() as i32, scale),
            fr::px(y.round() as i32, scale),
        );
        let metric = fr::metric_from(&cfg.get("gui", "floating_metric"));
        // 与重画那一拍**同一个式子**（见 `card_logical_size`）：以前这里自己写 `.max(40.0)`
        // 也不抬任务栏高度，于是启动那次夹的是比真卡片短的一截。
        let (win_w, win_h) = card_logical_size(
            cfg.get_float("floating", "width", DEFAULT_W),
            cfg.get_float("floating", "height", DEFAULT_H),
            fr::row_count(metric),
            taskbar_height_logical(scale),
        );
        landed_position(x, y, scale, &mon, win_w as f64, win_h as f64)
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
        let Some(rc) = get_rect(hwnd) else {
            return false;
        };
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
        // 这一搬也是自己搬的：不登记的话，下一拍会把"拔屏找回"当成用户搬动而重设锚点。
        APPLIED_TOP.store(y, Ordering::SeqCst);
        true
    }
}

/// 把自己重新按到 topmost 队列的最前面：**不碰坐标、不碰大小、不抢焦点**。
///
/// 为什么每拍都要问一次：任务栏同为 topmost，谁后被按过谁就在上面 —— 只在 `WM_WINDOWPOSCHANGING`
/// 里答一次，等于赌"系统还会再问我们"，而卡片停在带里不动时它不再问。
///
/// # Safety
/// 必须在拥有 `hwnd` 的那个线程上调用。
unsafe fn keep_above_bar(hwnd: HWND) {
    unsafe {
        let _ = wm::SetWindowPos(
            hwnd,
            Some(wm::HWND_TOPMOST),
            0,
            0,
            0,
            0,
            wm::SWP_NOMOVE | wm::SWP_NOSIZE | wm::SWP_NOACTIVATE,
        );
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
                    let _ =
                        wm::PostMessageW(Some(target), crate::WM_SHOW_MAIN, WPARAM(0), LPARAM(0));
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
                    // 系统给的矩形就是"我们自己照新缩放比摆的"，登记它，别让它被下一拍
                    // 当成用户搬动。锚点这一格照旧不动：动它就是改 DPI 那条链路的行为，
                    // 而那台 125%/150% 的机器本场量不到（见 README 的 DPI 未回归），不该顺手改。
                    APPLIED_TOP.store(rc.top, Ordering::SeqCst);
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
        // 建窗这一格也是"我们自己摆的"，不登记的话第一拍会把它当成用户搬动。
        APPLIED_TOP.store(y, Ordering::SeqCst);
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

#[cfg(test)]
mod anchor_tests {
    // 本测试模是选择性导入（不是 `use super::*`）：新用例引用生产项要在这里补名字。
    use super::{anchor_now, card_logical_size};
    use crate::floating_render as fr;

    /// 用户在拖：这一格既不是锚点、也不是我们自己摆出去的 ⇒ 锚点认它。
    /// 不认就是第十场量到的那个缺陷：竖向被下一拍拽回旧锚点、横向留着。
    #[test]
    fn an_external_move_becomes_the_anchor() {
        assert_eq!(
            anchor_now(500, 140, 140, 1.0),
            500,
            "还没落定的竖向搬动必须在下一拍站住"
        );
    }

    /// 长高让位：那一格正是我们自己上一拍摆出去的 ⇒ 不能认，否则缩回时回不到用户放的那一行。
    /// 这是整个改动唯一不能放宽的地方（也是 `ANCHOR_TOP` 存在的理由）。
    #[test]
    fn our_own_clamped_top_is_not_a_user_move() {
        assert_eq!(
            anchor_now(1014, 1031, 1014, 1.0),
            1031,
            "让位那一格是自己摆的，缩回要照锚点回"
        );
    }

    /// 阈值口径与 `pulled_position` 一致：来回取整差一格不算，差两格才算搬动。
    #[test]
    fn the_threshold_matches_the_drift_guard() {
        assert_eq!(
            anchor_now(1030, 1031, 1014, 1.0),
            1031,
            "差一格是物理→逻辑→物理取整的误差，认它等于承认每挪一格的慢速漂移"
        );
        assert_eq!(
            anchor_now(1029, 1031, 1014, 1.0),
            1029,
            "差两格就是真搬动，锚点要跟上去"
        );
    }

    #[test]
    fn an_unapplied_sentinel_never_overwrites_the_anchor() {
        assert_eq!(
            anchor_now(500, 140, i32::MIN, 1.0),
            140,
            "还没摆过时分不清是谁动的，宁可照锚点"
        );
    }

    /// 两端相减超出 i32 的那一档：`cur_top` 与锚点都可能来自外部（配置里的坐标、别的程序搬窗）。
    /// 用 i32 相减，`MAX - MIN` 绕回成 `-1`，绝对值 1 便"不算搬动"—— 一次真实的搬动被吞掉；
    /// 按 i64 算才是它本来的样子。
    #[test]
    fn extreme_rects_do_not_wrap_the_difference() {
        assert_eq!(
            anchor_now(i32::MAX, i32::MIN, 0, 1.0),
            i32::MAX,
            "相减超出 i32 也要按真差值判，不能绕回成没搬动"
        );
    }

    #[test]
    fn the_adopted_anchor_is_logical_pixels() {
        // 150% 下物理 900 = 逻辑 600：锚点存逻辑那一格（与 persist_position 同一口径）。
        assert_eq!(
            anchor_now(900, 140, 210, 1.5),
            600,
            "认下来的锚点要按缩放比折回逻辑像素"
        );
    }

    /// 卡片尺寸那把尺子的形状：宽的下限是 `MIN_W`，高至少到任务栏。
    #[test]
    fn the_card_size_floors_at_min_width_and_the_taskbar() {
        // 手改 `[floating] width = 50`：卡片真宽是 90，夹位置也得按 90 算
        // （旧的那条路自己写了一个更小的下限，于是每次启动位置都被多推一截）
        assert_eq!(card_logical_size(50.0, 40.0, 2, 48).0, fr::MIN_W);
        // 大任务栏（物理 64 @100%）：两行内容算出来 48，卡片必须是 64 高
        // （旧的那条路根本没抬，启动那次按 48 夹 ⇒ 第一拍有一截落在屏外）
        assert_eq!(card_logical_size(280.0, 40.0, 2, 64).1, 64);
        // 内容比任务栏高时听内容的（`metric=both` 三行）
        assert!(
            card_logical_size(280.0, 40.0, 3, 48).1 > 48,
            "三行内容的高度不该被任务栏那一段压住"
        );
        // 配置值高于两者时原样保住用户要的
        assert_eq!(card_logical_size(400.0, 120.0, 2, 48), (400, 120));
    }

    /// 接线守卫：重画那一拍与启动那一次钳位**都得**问这同一把尺子。
    ///
    /// 上面那条只证明函数自己算得对；这两处调用点不接进来，两条缺陷照样会复发。
    #[test]
    fn both_floating_paths_ask_the_same_card_size() {
        let src = include_str!("floating.rs");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let flat: String = src[..cut].chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(
            flat.matches("card_logical_size(").count(),
            3,
            "该是一处定义加两处调用（重画、启动钳位），实际 {}",
            flat.matches("card_logical_size(").count()
        );
        assert!(
            flat.contains("let(cfg_w,h_logical)=card_logical_size("),
            "重画那一拍不再问这把尺子了"
        );
        assert!(
            flat.contains("let(win_w,win_h)=card_logical_size("),
            "启动那次钳位不再问这把尺子了"
        );
    }
}
