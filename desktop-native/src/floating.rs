//! 原生悬浮窗：分层窗口（`WS_EX_LAYERED`）+ `UpdateLayeredWindow` 逐像素 alpha。
//!
//! 为什么是一个**独立线程 + 独立窗口**：悬浮窗要跟着托盘常驻、主窗口收起时也在，
//! 它自己的消息循环（GDI DC 与分层位图都是线程绑定的）最省事。主窗口收起/退出都不会
//! 把它卡住，反过来它也不占主线程的一帧。
//!
//! 为什么是 GDI 表面而不是主窗口那套 D3D11+DComp：卡片只有几行文字和一个圆角底，
//! `UpdateLayeredWindow` 就够 —— 为它再养一条交换链是杀鸡用牛刀。
//! 但**文字本身走 Direct2D**：一条绑在这张 GDI 蒙版上的 DC 渲染目标，档位钉 `SOFTWARE`
//! （`DEFAULT` 那一档在本机值 36 MB 私有工作集，而画出来的字节与软件档逐像素相同，
//! 现量与判据见 [`ensure_text_ctx`] 那段、台账 §十）。
//!
//! 这一份从 P7 删掉的 `desktop/src/floating/native.rs` 搬来，当时改了三处：
//! 1. 数据不再是 parked 的 `LiveStats`（原生端没有采集进程），改成主窗口读库后
//!    `publish()` 过来的三格（今日次数 / 今日时长 / 目标百分比），见 [`Live`]；
//! 2. 暂停、久坐提醒、`速度` 都没有数据源，卡片改画「今日目标」；
//! 3. 双击卡片投的是本进程的 `WM_SHOW_MAIN`，不再是 Tauri 的 `show_main_window`。
//!
//! ⚠ 上面第 2 条只剩下一半还成立（2026-10-31 按现量改口）：采集接上之后，暂停与速度
//! 都回到卡片上了 —— `current_content()` 现读 `collect::is_paused()` 与 `collect::cpm()`，
//! 暂停那一格把速度写成「暂停」并去掉强调色，目标百分比回到主窗口。
//! **只有久坐提醒那条横带原生端始终没有实现**：`[rest]` 那六个键因此零读点，2026-11-01
//! 按"每次落盘、全仓零读点"这把尺子把设置页那六格与默认值一起收掉了（老文件里的那几行
//! 在 load 时清掉，口径见 core 的 `config::DEPRECATED_CONFIG`）。哪天把横带接回来，
//! 就是重新加一节配置 + 六行的活。
//!
//! **点击策略**（旧版就是这个口径，本轮沿用）：整张卡片就是标题栏 ——
//! `WM_NCHITTEST` 一律回 `HTCAPTION`，按住任意位置即可拖动，双击叫出主窗口。
//! 不做 `WS_EX_TRANSPARENT` 式穿透：穿透之后卡片既拖不动也点不到，而收起来的入口
//! 只有托盘与设置页（确实要穿透的话，那是另一个"锁定"开关的活，不该默认开）。

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicI32, AtomicIsize, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory, ID2D1SolidColorBrush,
    D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT,
    D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE, D2D1_RENDER_TARGET_TYPE_SOFTWARE,
    D2D1_RENDER_TARGET_USAGE_NONE, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, IDWriteTextLayout,
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING,
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
    /// 时长那一读有没有读全：读不全时卡片念「读不出」，不许折成 `0s`
    /// （那与"今天真的没有前台时长"长得一模一样）。
    pub duration_unreadable: bool,
}

static LIVE: Mutex<Live> = Mutex::new(Live {
    today: 0,
    duration_sec: 0,
    duration_unreadable: false,
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
        live.duration_unreadable,
        crate::collect::is_listening(),
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
///
/// 档位是 **SOFTWARE**，不是 `DEFAULT`。这不是顺手一改，是 2026-11-01 那轮归因的落点：
/// `DEFAULT` 那一档的 DC 渲染目标在本机值 **37 796 KB** 私有工作集（`ctx_split_tests`），
/// 而卡片那 38 MB 的账整个挂在这一格参数上；换成 `SOFTWARE` 是 **1 448 KB**，
/// 并且**画出来的字节完全相同** —— 三档内容 × 两档缩放共六格，`identical=true`、
/// 差像素 0（判据 `text_path_ab_tests::pill_dw_software_matches_default`，同一把尺子
/// 在 DWrite-vs-GDI 那一腿报的是 1085 个差像素，所以这个 0 不是量具看不见）。
/// 每帧的绘制成本也顺带从 1.213–1.432 ms 降到 0.352–0.486 ms（release，同一格夹具）。
unsafe fn ensure_text_ctx() -> bool {
    ensure_text_ctx_kind(D2D1_RENDER_TARGET_TYPE_SOFTWARE)
}

/// 同上，只是把渲染目标的**档位**交出来。
///
/// 为什么留这个口子：两档必须能在同一个进程里各画一张卡对拍（见
/// `text_path_ab_tests::pill_dw_software_matches_default`），而那份上下文是 thread_local
/// 且只建一次 —— 不参数化就没法把"另一档"装进第二条腿。
unsafe fn ensure_text_ctx_kind(kind: D2D1_RENDER_TARGET_TYPE) -> bool {
    unsafe {
        if TEXT_TRIED.with(|t| t.get()) {
            return TEXT.with(|c| c.borrow().is_some());
        }
        TEXT_TRIED.with(|t| t.set(true));
        let made = (|| -> windows::core::Result<TextCtx> {
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: kind,
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

/// 值的矩形与小字单位的矩形：值止于 `sx`、单位从 `sx` 起，**由构造保证互不重叠**。
///
/// 为什么单独拆成纯函数：一帧里所有文字现在一次画进同一张蒙版，之后要按矩形分别叠色；
/// 而小字单位的版心本来就落在值的版心里头（值的矩形一路到行右端）。两边一旦重叠，
/// 症状是"`/分` 的颜色糊到数字上"这种屏上没人会报的错 —— 所以那条不重叠必须是算出来的，
/// 不是看着对的（用例在下面 `ink_split_tests`）。
///
/// `adv_px` 是排版量出来的用掉宽度；越界一律夹进 `[rc.x, rc.x+rc.w]`，
/// 于是"挤没了"那一档自然得到零宽的单位矩形（什么都不画），而不是负宽度的矩形。
fn split_overlay_rects(rc: Rect, adv_px: f32, scale: f64) -> (Rect, Rect) {
    // 全程 saturating：`adv_px` 来自 DWrite 的度量，遇到异常值（`f32::MAX`、NaN 取整）
    // `as i32` 会得到 `i32::MAX`，而 `rc.x + i32::MAX` 在 debug 下是**加法溢出 panic**、
    // 在 release 是回绕成一个负数落点 —— 2026-11-01 由 `ink_split_tests` 抓到。
    let right = rc.x.saturating_add(rc.w);
    let raw =
        rc.x.saturating_add(adv_px.round() as i32)
            .saturating_add(fr::px(1, scale));
    let sx = raw.clamp(rc.x.min(right), right.max(rc.x));
    (
        Rect {
            x: rc.x,
            y: rc.y,
            w: (sx - rc.x).max(0),
            h: rc.h,
        },
        Rect {
            x: sx,
            y: rc.y,
            w: (right - sx).max(0),
            h: rc.h,
        },
    )
}

/// 一段字的排版：字体 + 版心 + 度量。**不画** —— 一帧里所有段先各自排好，再由
/// `dw_draw_all` 一次画进同一张蒙版，于是 `BeginDraw/EndDraw` 从"每段一对"降到"每帧一对"
/// （现量：每段那一对约 1 ms、一帧约 6 对，正是 `text` 段里最厚的 6.172／7.486 ms）。
///
/// 字号与坐标都按物理像素传：渲染目标固定 96 dpi，1 DIP = 1 px。
/// 回 `(layout, 用掉的宽度)` —— 那个宽度决定小字单位落在哪，不能估。
unsafe fn dw_layout(
    ctx: &TextCtx,
    text: &str,
    rc: Rect,
    font_px: i32,
    face: &str,
    right: bool,
) -> Option<(IDWriteTextLayout, f32)> {
    unsafe {
        if rc.w <= 0 || rc.h <= 0 {
            return None;
        }
        let l0 = Instant::now();
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
        PAINT_TEXT_LAYOUT.fetch_add(l0.elapsed().as_nanos() as u64, Ordering::Relaxed);
        Some((layout, metrics.widthIncludingTrailingWhitespace))
    }
}

/// 一帧一对：挂 DC → 清黑 → 把所有段画进同一张蒙版 → 收尾。
///
/// 清黑靠的是这次 `Clear` 而不是从前那句 `mask.buf().fill(0)`：DC 已经绑在蒙版上，
/// 两者做的是同一件事，旧写法每段既 `fill` 又 `Clear` 等于清两遍（那一格现在归
/// `mask_clear`，DWrite 这一路应当恒 0，只有 GDI 兜底还在用它）。
///
/// 回 false = 这一帧没画上（调用方因此不叠色）。`EndDraw` 的返回值必须看：
/// 它失败而蒙版留着上帧的字，叠色就会把旧字再染一次。
unsafe fn dw_draw_all(
    ctx: &TextCtx,
    hdc: g::HDC,
    w: i32,
    h: i32,
    items: &[(&IDWriteTextLayout, Rect)],
) -> bool {
    unsafe {
        let d0 = Instant::now();
        if ctx
            .rt
            .BindDC(
                hdc,
                &RECT {
                    left: 0,
                    top: 0,
                    right: w,
                    bottom: h,
                },
            )
            .is_err()
        {
            return false;
        }
        ctx.rt.BeginDraw();
        ctx.rt.Clear(Some(&D2D1_COLOR_F {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        }));
        ctx.rt
            .SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        for (layout, rc) in items {
            ctx.rt.DrawTextLayout(
                Vector2 {
                    X: rc.x as f32,
                    Y: rc.y as f32,
                },
                // 解一层引用：`DrawTextLayout` 的参数要 `Param<IDWriteTextLayout>`，
                // 而 `items` 是 `&[(&IDWriteTextLayout, Rect)]`，模式里拿到的是 `&&_`。
                *layout,
                &ctx.brush,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
            );
        }
        let ok = ctx.rt.EndDraw(None, None).is_ok();
        PAINT_TEXT_DRAW.fetch_add(d0.elapsed().as_nanos() as u64, Ordering::Relaxed);
        ok
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
        // 1) 先排版，一段都不画：每行的值、以及它后面那串小字单位，各排一次。
        //    两遍不能并成一遍 —— 单位的落点要等值的宽度量出来才定。
        let mut items: Vec<(IDWriteTextLayout, Rect, Color)> = Vec::new();
        for run in text_runs(content, w, scale, pal) {
            let Some((layout, adv)) = dw_layout(
                ctx,
                &run.text,
                run.rc,
                fr::px(run.font_px, scale),
                run.face,
                run.right,
            ) else {
                continue;
            };
            let mut vrect = run.rc;
            let mut srect = None;
            if run.suffix.is_some() {
                let (v, s) = split_overlay_rects(run.rc, adv, scale);
                vrect = v;
                srect = Some(s);
            }
            items.push((layout, vrect, run.color));
            if let (Some(sfx), Some(sr)) = (&run.suffix, srect) {
                if sr.w <= 0 {
                    continue; // 挤没了：单位不画，也不许它把整行染成自己的颜色
                }
                if let Some((lay, _)) = dw_layout(
                    ctx,
                    &sfx.text,
                    sr,
                    fr::px(sfx.font_px, scale),
                    sfx.face,
                    false,
                ) {
                    items.push((lay, sr, sfx.color));
                }
            }
        }
        if items.is_empty() {
            return;
        }
        // 2) 一次画进同一张蒙版：整帧清黑是 `dw_draw_all` 里那次 `Clear` 干的
        //    （DC 绑的就是这张蒙版）。旧写法每段先 `fill(0)` 再 `Clear`，等于把同一帧
        //    清两遍 —— 所以 `mask_clear` 那一格在 DWrite 这一路应当恒 0。
        let refs: Vec<(&IDWriteTextLayout, Rect)> = items.iter().map(|(l, r, _)| (l, *r)).collect();
        if !dw_draw_all(ctx, hdc, w, h, &refs) {
            return;
        }
        // 3) 分段叠色：每段只叠自己的矩形。一帧一次画完 ⇒ 蒙版里同时有好几段的字，
        //    再拿"整帧 + 单色"去叠就会把别人的字也涂了（互不重叠由 `split_overlay_rects` 保证）。
        for (_layout, rc, color) in &items {
            timed(&PAINT_MASK_OVERLAY, || {
                fr::overlay_text_coverage_rect(
                    card.buf(),
                    mask.buf(),
                    *color,
                    w.max(0) as usize,
                    *rc,
                )
            });
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
            timed(&PAINT_MASK_CLEAR, || mask.buf().fill(0));
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
        timed(&PAINT_MASK_OVERLAY, || {
            fr::overlay_text_coverage(card.buf(), mask.buf(), run.color)
        });

        if let Some(sfx) = &run.suffix {
            let Some(sfont) = make_font(sfx.font_px, scale, sfx.face, 400) else {
                continue;
            };
            timed(&PAINT_MASK_CLEAR, || mask.buf().fill(0));
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
            timed(&PAINT_MASK_OVERLAY, || {
                fr::overlay_text_coverage(card.buf(), mask.buf(), sfx.color)
            });
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

/// 一帧里各段的耗时累加（纳秒）与帧数 —— 卡片这一族的**第一个读位**。
///
/// 为什么要写进生产码、而不是只在测试里编译的属性：本文件下面那条源文本判据取生产码的
/// 边界是"测试模块起点之前"（`floating.rs` 里那条 `cut`），往 `repaint` 中段插一段带那个
/// 属性的代码，会让判据**少扫半截重画函数 —— 只假绿、不红**，正是本仓记过的形状。
/// （更狠的是：连注释里原样写出那个字面量都会把边界往前挪 —— 2026-10-31 本场就因此连红
/// 两条判据，`data.rs` 那句"判据用的针要拼出来"说的就是这件事。）
/// 先例是 `snip.rs` 的 `last_moved`：要量的东西必须在真帧上量得着。
///
/// 代价：一帧 6 次 `Instant::now()` + 6 次 `fetch_add`（卡片最多 3 帧/秒），纳秒级。
/// 读数走 [`paint_counters`]，默认没有任何用户可见行为。
#[allow(dead_code)] // 只有文件末尾那条探针用例读它
#[derive(Debug, Clone, Copy, Default)]
pub struct PaintCounters {
    /// 走进 `repaint` 的次数（含中途失败没提交的那些帧 —— 那同样花了 CPU）
    pub frames: u64,
    /// 配置读数 + 版面尺寸 + 内容拼装
    pub prep_ns: u64,
    /// `GetDC` + `CreateCompatibleDC` + 两块整卡 DIB section
    pub surfaces_ns: u64,
    /// 逐像素两层圆角覆盖率（整张卡，每像素两次）
    pub pixels_ns: u64,
    /// 文字：每个 run 一次 DWrite 排版 + 蒙版清零 + 全帧覆盖度叠加
    pub text_ns: u64,
    /// `text` 的子段：`CreateTextFormat` + `CreateTextLayout` + `GetMetrics`（DWrite 那条路）
    pub text_layout_ns: u64,
    /// `text` 的子段：`BeginDraw` → `Clear` → `DrawTextLayout` → `EndDraw`（DWrite 那条路）
    pub text_draw_ns: u64,
    /// `text` 的子段：蒙版整帧清零（DWrite 与 GDI 兜底两条路都记在这里）
    pub mask_clear_ns: u64,
    /// `text` 的子段：整帧覆盖度叠加 `overlay_text_coverage`（每个 run 与每个单位各一次）
    pub mask_overlay_ns: u64,
    /// `UpdateLayeredWindow` 与两块 DIB 的销毁
    pub submit_ns: u64,
    /// 尾段几何：`GetWindowRect` + 最近屏 + 重锚 + 抢层级
    pub tail_ns: u64,
}

static PAINT_FRAMES: AtomicU64 = AtomicU64::new(0);
static PAINT_PREP: AtomicU64 = AtomicU64::new(0);
static PAINT_SURFACES: AtomicU64 = AtomicU64::new(0);
static PAINT_PIXELS: AtomicU64 = AtomicU64::new(0);
static PAINT_TEXT: AtomicU64 = AtomicU64::new(0);
static PAINT_SUBMIT: AtomicU64 = AtomicU64::new(0);
static PAINT_TAIL: AtomicU64 = AtomicU64::new(0);
static PAINT_TEXT_LAYOUT: AtomicU64 = AtomicU64::new(0);
static PAINT_TEXT_DRAW: AtomicU64 = AtomicU64::new(0);
static PAINT_MASK_CLEAR: AtomicU64 = AtomicU64::new(0);
static PAINT_MASK_OVERLAY: AtomicU64 = AtomicU64::new(0);

/// 把一次调用夹进某个计数格 —— 蒙版那两处（整帧清零、整帧叠加）散在两条绘制路里共八处，
/// 用包一层的写法比在每个站点摆 `ProbeMark` 少动结构，也就少一处改坏的可能。
fn timed<T>(slot: &'static AtomicU64, f: impl FnOnce() -> T) -> T {
    let at = Instant::now();
    let out = f();
    slot.fetch_add(at.elapsed().as_nanos() as u64, Ordering::Relaxed);
    out
}

/// 一段结束就把这段耗时记进对应格子，并就地起下一段。
struct ProbeMark(&'static AtomicU64, Instant);

impl ProbeMark {
    fn at(slot: &'static AtomicU64) -> Self {
        Self(slot, Instant::now())
    }
    fn next(self, slot: &'static AtomicU64) -> Self {
        self.finish();
        Self::at(slot)
    }
    fn finish(self) {
        self.0
            .fetch_add(self.1.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
}

/// 现取一份累计读数（进程启动以来）。
#[allow(dead_code)] // 同上：只有探针读它
pub fn paint_counters() -> PaintCounters {
    let g = |s: &AtomicU64| s.load(Ordering::Relaxed);
    PaintCounters {
        frames: g(&PAINT_FRAMES),
        prep_ns: g(&PAINT_PREP),
        surfaces_ns: g(&PAINT_SURFACES),
        pixels_ns: g(&PAINT_PIXELS),
        text_ns: g(&PAINT_TEXT),
        text_layout_ns: g(&PAINT_TEXT_LAYOUT),
        text_draw_ns: g(&PAINT_TEXT_DRAW),
        mask_clear_ns: g(&PAINT_MASK_CLEAR),
        mask_overlay_ns: g(&PAINT_MASK_OVERLAY),
        submit_ns: g(&PAINT_SUBMIT),
        tail_ns: g(&PAINT_TAIL),
    }
}

/// 上一帧的**成品像素**与它的签名。签名是"卡片上那几个字节由哪些输入决定"的完整清单：
/// 尺寸、缩放、主题、内容。少一格就会把旧数字留在屏上，所以多一格也不要省。
struct CachedFrame {
    w: i32,
    h: i32,
    scale_bits: u64,
    theme: String,
    content: fr::PillContent,
    px: Vec<u8>,
    /// 已经连吃了几拍。到 [`CACHE_MAX_HITS`] 就强制重算一次。
    hits: u32,
}

/// 连吃这么多拍就无论如何重算一次（约 30 秒 @ 2 帧/秒）。
///
/// 为什么要有这一格：文字那一段里 `dw_line` 排版失败是 `continue` 过去的（一处失败只会
/// 让卡片少一个字，下一帧本来会自愈）。接了缓存之后，"同样的内容"就不再等于"重新画一遍"
/// —— 少画的那一格会被原样复用下去，直到数字变化才恢复。这一格把最坏情况钉在 30 秒内，
/// 而 99% 的帧仍然命中（卡片全天不变的那些分钟正是省下来的那一段）。
const CACHE_MAX_HITS: u32 = 60;

thread_local! {
    static FRAME: RefCell<Option<CachedFrame>> = const { RefCell::new(None) };
}

/// 签名一致时把缓存的成品像素直接写进位图，回 `true` = 这一帧不用从头算。
///
/// 与已经被否掉的"内容没变就跳过提交"（B3）**不是一件事**：那一条否的是跳过一拍 ——
/// 跳一拍等于跳过尾段那次几何重锚，也就是拔掉"拔屏/别的程序搬窗"唯一的找回路径。
/// 这里只省"把同样的像素再算一遍"：提交、重锚、抢层级照旧每一拍都做。
/// 依据是现量（`pill_paint_cost_per_frame_is_printed`）：一帧 15.868 ms，
/// 其中逐像素圆角覆盖率 5.908 + 文字与蒙版 8.971 —— 93% 花在重算上，而卡片全天 2~3 帧/秒。
unsafe fn frame_cache_paste(
    w: i32,
    h: i32,
    scale: f64,
    theme: &str,
    content: &fr::PillContent,
    dst: &mut Surface,
) -> bool {
    let n = (w * h * 4) as usize;
    if dst.buf().len() != n {
        return false;
    }
    FRAME.with(|cell| {
        let mut cell = cell.borrow_mut();
        let Some(f) = cell.as_mut() else { return false };
        if f.w != w
            || f.h != h
            || f.scale_bits != scale.to_bits()
            || f.theme != theme
            || &f.content != content
            || f.px.len() != n
            || f.hits >= CACHE_MAX_HITS
        {
            return false;
        }
        unsafe { std::ptr::copy_nonoverlapping(f.px.as_ptr(), dst.bits, n) };
        f.hits += 1;
        true
    })
}

/// 把这一帧的成品像素连同签名存下来，供下一拍命中。
unsafe fn frame_cache_store(
    w: i32,
    h: i32,
    scale: f64,
    theme: &str,
    content: &fr::PillContent,
    src: &Surface,
) {
    let n = (w * h * 4) as usize;
    let px = unsafe { std::slice::from_raw_parts(src.bits, n) }.to_vec();
    FRAME.with(|cell| {
        *cell.borrow_mut() = Some(CachedFrame {
            w,
            h,
            scale_bits: scale.to_bits(),
            theme: theme.to_string(),
            content: content.clone(),
            px,
            hits: 0,
        });
    });
}

/// 底板模板：圆角那一层的成品像素。
///
/// 它只由 `(w, h, scale, 主题)` 决定 —— **与卡片上那几个数字无关**，所以这一份缓存没有
/// "旧数字留在屏上"那一族风险：内容变了它照样是对的。
/// 依据是现量的第二段：`pixels` 一帧 3.930 ms（逐像素两层圆角覆盖率），
/// 而这一块正是"内容一变就整帧从头再算"里最白的一笔。
struct BgTemplate {
    w: i32,
    h: i32,
    scale_bits: u64,
    theme: String,
    px: Vec<u8>,
}

thread_local! {
    static BG: RefCell<Option<BgTemplate>> = const { RefCell::new(None) };
}

/// 模板命中就把底板像素写进位图，回 `true` = 这一帧不用逐像素重算圆角。
unsafe fn bg_cache_paste(w: i32, h: i32, scale: f64, theme: &str, dst: &mut Surface) -> bool {
    let n = (w * h * 4) as usize;
    if dst.buf().len() != n {
        return false;
    }
    BG.with(|cell| {
        let b = cell.borrow();
        let Some(t) = b.as_ref() else { return false };
        if t.w != w || t.h != h || t.scale_bits != scale.to_bits() || t.theme != theme {
            return false;
        }
        unsafe { std::ptr::copy_nonoverlapping(t.px.as_ptr(), dst.bits, n) };
        true
    })
}

/// 逐像素算完一次就存下模板。这里**不设**"连吃 N 拍强制重算"那道兜底：
/// 上面那份整帧缓存要它，是因为排版失败会被同样的内容永久复现；
/// 底板这一段没有失败分支，纯算术，算错只会一直错在同一条路上，而那条路已经量过了。
unsafe fn bg_cache_store(w: i32, h: i32, scale: f64, theme: &str, src: &Surface) {
    let n = (w * h * 4) as usize;
    let px = unsafe { std::slice::from_raw_parts(src.bits, n) }.to_vec();
    BG.with(|cell| {
        *cell.borrow_mut() = Some(BgTemplate {
            w,
            h,
            scale_bits: scale.to_bits(),
            theme: theme.to_string(),
            px,
        });
    });
}

/// 一次完整重画并提交给 DWM。返回 false = 这一拍什么都没画出来（调用方出声）。
///
/// # Safety
/// 必须在拥有 `hwnd` 的那个线程上调用。
unsafe fn repaint(hwnd: HWND) -> bool {
    unsafe {
        PAINT_FRAMES.fetch_add(1, Ordering::Relaxed);
        let mut pm = ProbeMark::at(&PAINT_PREP);
        let cfg = focusflow_core::config::instance();
        let scale = scale_of(hwnd);
        // 主题在这一拍只读一次：它既挑调色板，又是缓存签名的一格（改了主题必须重算）。
        let theme = cfg.get("gui", "theme");
        let pal = fr::palette_for(&theme);
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
        pm = pm.next(&PAINT_SURFACES);

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
        // 签名一致 ⇒ 整段重算都不用做，连文字蒙版也不必建（见 `frame_cache_paste`）。
        let hit = frame_cache_paste(w, h, scale, &theme, &content, &mut card);
        let mut mask: Option<Surface> = None;
        if !hit {
            let Some(m) = Surface::new(hdc, w, h) else {
                card.delete();
                let _ = g::DeleteDC(hdc);
                let _ = g::ReleaseDC(Some(hwnd), screen);
                crate::tracing_note("悬浮窗建文字蒙版失败，这一拍没画上");
                return false;
            };
            let _ = g::SelectObject(hdc, g::HGDIOBJ(m.bmp.0));
            mask = Some(m);
        }
        pm = pm.next(&PAINT_PIXELS);

        // 1) 卡片底 + 1px 边框。两层圆角覆盖率：外层 = 整张卡，内层 = 缩进一个边框，
        //    两者之差就是边框那一圈 —— 四个角因此是抗锯齿的，不再是 1px 台阶。
        let border_px = fr::px(fr::BORDER, scale).max(1);
        let radius_px = fr::px(fr::RADIUS, scale);
        if !hit && !bg_cache_paste(w, h, scale, &theme, &mut card) {
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
            // 这里不需要任何"先放开借用"的写法：`card.buf()` 交回的是 `&mut [u8]`，
            // 循环最后一次用它之后借用就结束了（NLL），下面重新借 `&card` 才编得过。
            // 曾经写了一句 `drop(buf);` 当作必要步骤 —— 那是对引用的 drop，什么都不做，
            // 被 clippy 的 `dropping_references` 抓到了。
            bg_cache_store(w, h, scale, &theme, &card);
        }

        pm = pm.next(&PAINT_TEXT);
        // 2) 文字。优先 DirectWrite（与旧 WebView 同一渲染栈）；上下文起不来再退回 GDI。
        //    两条路都画进"白字黑底"蒙版，R 通道即覆盖度，再按颜色叠到卡片上。
        //    蒙版只在**没命中缓存**那一拍才存在（`mask` 是 Option），命中时整段跳过。
        if let Some(mask) = mask.as_mut() {
            if ensure_text_ctx() {
                draw_text_dw(hdc, mask, &mut card, &content, w, h, scale, pal);
            } else {
                draw_text_gdi(hdc, mask, &mut card, &content, w, h, scale, pal);
            }
            // 这一帧是全新生成的：连同它的签名存一份，下一拍同样的内容就不用再算。
            frame_cache_store(w, h, scale, &theme, &content, &card);
        }

        pm = pm.next(&PAINT_SUBMIT);
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
        // 蒙版只在重算那一拍存在；删它之前必须已经不在 DC 上（`Surface::delete` 的 Safety 条件），
        // 上面那句选回 null 就是为此。
        if let Some(mut m) = mask {
            m.delete();
        }
        card.delete();
        let _ = g::DeleteDC(hdc);
        let _ = g::ReleaseDC(Some(hwnd), screen);

        if ok.is_err() {
            crate::tracing_note("悬浮窗提交位图给 DWM 失败，这一拍没画上");
            return false;
        }

        pm = pm.next(&PAINT_TAIL);
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
        pm.finish();
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
            if let Err(e) = cfg.set_now("floating", key, &value) {
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

    /// 底板模板必须接在逐像素那一段**前面**，而不是把它换掉。
    ///
    /// 为什么钉形状：底板只由 `(尺寸, 缩放, 主题)` 决定，命中时那一帧 3.930 ms 的逐像素
    /// 圆角覆盖率应该整段跳过；写歪的两种形状都跑得起来 —— 一是把循环删了（换个缩放或
    /// 主题就画不出来了），二是缓存在循环**之后**才 paste（等于白算一遍再盖掉，
    /// 一分钱没省，屏上还看不出来）。所以钉三件事：paste 在条件里、循环还在、store 在循环后。
    #[test]
    fn the_background_template_gates_the_pixel_loop_without_replacing_it() {
        let src = include_str!("floating.rs");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod = &src[..cut];
        assert!(
            prod.contains("if !hit && !bg_cache_paste(w, h, scale, &theme, &mut card) {"),
            "底板模板不再挡在逐像素那一段前面：内容一变就连底板一起重算"
        );
        assert!(
            prod.contains("let cov_out = fr::rounded_rect_coverage(x, y, w, h, radius_px);"),
            "逐像素圆角那一段不见了：底板模板一旦失效（换缩放/主题）就没东西可画了"
        );
        let store = prod
            .find("bg_cache_store(w, h, scale, &theme, &card);")
            .expect("算完不再存底板模板");
        let loop_at = prod
            .find("let cov_out = fr::rounded_rect_coverage(x, y, w, h, radius_px);")
            .expect("逐像素那一段不见了");
        assert!(
            store > loop_at,
            "存模板排在逐像素之前：存进去的是没画底板的空位图，下一拍命中就是白屏"
        );
    }
}

#[cfg(test)]
mod ink_split_tests {
    use super::split_overlay_rects;
    use crate::floating_render::Rect;

    /// 值与小字单位的叠色矩形必须**首尾相接且不重叠**，并且并起来还是那一行。
    ///
    /// 这条不是洁癖：一帧所有文字现在画进同一张蒙版，之后按矩形分别叠色 ——
    /// 两块一旦重叠，重叠区会被后叠的那一段按自己的颜色再涂一次，
    /// 屏上就是"`/分` 的颜色糊到数字上"，而没人会为此报缺陷。
    #[test]
    fn value_and_suffix_rects_tile_the_row_without_overlap() {
        let rc = Rect {
            x: 10,
            y: 4,
            w: 90,
            h: 18,
        };
        for adv in [0.0f32, 1.0, 44.4, 89.6, 90.0, 500.0, -7.0, f32::MAX] {
            let (v, s) = split_overlay_rects(rc, adv, 1.0);
            assert!(v.w >= 0 && s.w >= 0, "adv={adv} 不该出负宽：{v:?} {s:?}");
            assert_eq!(v.x, rc.x, "值永远从行首起");
            assert_eq!(v.x + v.w, s.x, "adv={adv}：值与小字单位之间不许有缝");
            assert_eq!(
                s.x + s.w,
                rc.x + rc.w,
                "adv={adv}：两块并起来必须正好是那一行"
            );
            let overlap = (v.x + v.w).min(s.x + s.w) - v.x.max(s.x);
            assert!(overlap <= 0, "adv={adv} 两块重叠 {overlap} 像素");
        }
    }

    /// 两头都要夹住：`adv` 小到 0 ⇒ 值那一格宽 0（不画）；大到超出行宽 ⇒ 单位那一格宽 0。
    /// 后者就是"挤没了"那一档：单位不画，也**不许**它拿整行当矩形去染自己的颜色。
    #[test]
    fn extreme_advances_degenerate_one_side_instead_of_leaking() {
        let rc = Rect {
            x: 0,
            y: 0,
            w: 40,
            h: 12,
        };
        // 量为 0 时值那一格只剩"值与单位之间那 1 物理像素的间距"，单位仍然从间距之后起。
        let (v, s) = split_overlay_rects(rc, 0.0, 1.0);
        assert_eq!((v.w, s.w), (1, 39), "adv=0：值几乎不占位，单位仍占满其余");
        let (v, s) = split_overlay_rects(rc, 9999.0, 1.0);
        assert_eq!(
            (v.w, s.w),
            (40, 0),
            "挤没了：单位零宽，值占整行（不许反过来）"
        );
    }

    /// 缩放走同一把尺子：`px(1, scale)` 那一格的间距在 200% 下必须是 2 物理像素。
    #[test]
    fn the_gap_between_value_and_suffix_scales() {
        let rc = Rect {
            x: 0,
            y: 0,
            w: 60,
            h: 12,
        };
        for (scale, want) in [(1.0f64, 1i32), (1.5, 2), (2.0, 2)] {
            let (v, s) = split_overlay_rects(rc, 20.0, scale);
            assert_eq!(v.w, 20 + want, "scale={scale}：值占 adv + 那一格间距");
            assert_eq!(s.x, rc.x + 20 + want, "scale={scale}：单位从间距之后起");
        }
    }
}

#[cfg(test)]
mod paint_order_tests {
    /// 蒙版 → 画字 → 卡片选回 → 才提交。这条链是 2026-10-26 那个"卡片整个看不见"的
    /// 根因留下的疤：`repaint` 把文字蒙版选在 DC 之后忘了换回来，于是
    /// `UpdateLayeredWindow` 交出去的是那张 **alpha 全 0** 的覆盖度层 —— 窗口在、置顶在、
    /// 返回值成功、日志一声不出，屏幕上什么都没有。
    ///
    /// 本轮把文字改成"一帧一次画进同一张蒙版"，正好动在这条链上，所以把它补成判据
    /// （此前只有注释在守）。钉的是**索引次序**：换序即红，删句即红。
    #[test]
    fn the_card_comes_back_before_anything_is_submitted() {
        let src = include_str!("floating.rs");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod = &src[..cut];
        // 次序只能在 `repaint` 那一身之内比：全文 `find` 会先撞上别的函数的**定义**
        // —— 本场第一版就是这么错的，它拿 `dw_draw_all` 定义的位置去跟 `repaint` 里的
        // 选位比，红了，却红在两个不相干的地方。
        let at = prod
            .find("unsafe fn repaint(hwnd: HWND)")
            .expect("repaint 不见了");
        let rp = &prod[at..at + prod[at..].find("\n}").expect("repaint 的函数体没收尾")];
        let mask_on = rp
            .find("SelectObject(hdc, g::HGDIOBJ(m.bmp.0))")
            .expect("蒙版没被选上 DC：那文字无处可画（也可能这一步改名了）");
        let text = rp.find("draw_text_dw(").expect("画文字那一步不见了");
        let card_back = rp
            .find("SelectObject(hdc, g::HGDIOBJ(card.bmp.0))")
            .expect("卡片没有选回 DC：提交出去的会是 alpha 全 0 的蒙版（`9b93617` 那一刀）");
        let submitted = rp
            .find("wm::UpdateLayeredWindow(")
            .expect("提交那一步不见了");
        assert!(
            mask_on < text && text < card_back && card_back < submitted,
            "次序变了：蒙版上 DC={mask_on} 画文字={text} 卡片选回={card_back} 提交={submitted}"
        );
        // 一帧一对：多出来的 BeginDraw/EndDraw 就是"每段一对"那一族回来了（现量每段约 1 ms）。
        let begins = prod.matches("ctx.rt.BeginDraw();").count();
        let ends = prod.matches("ctx.rt.EndDraw(").count();
        assert_eq!(
            (begins, ends),
            (1, 1),
            "D2D 的绘制对变成了 {begins} 开 {ends} 关：一帧应当只有一对"
        );
        // 整帧清黑的责任在 `dw_draw_all` 那次 `Clear` 身上：DWrite 这一路不该再有 `fill(0)`
        //（旧写法每段先 `fill` 再 `Clear`，等于把同一帧清两遍）。函数体收口再比 ——
        // GDI 兜底那一路仍然要 `fill(0)`，那是它对的地方，不该被这条判据一起挡掉。
        let dw_at = prod
            .find("unsafe fn draw_text_dw(")
            .expect("DWrite 那条绘制路径不见了");
        let body = &prod[dw_at..dw_at + prod[dw_at..].find("\n}\n").expect("函数体没收尾")];
        assert!(
            !body.contains("mask.buf().fill(0)"),
            "DWrite 这一路又开始每段 fill(0)：与那次 Clear 重复，等于每帧清两遍"
        );
        assert!(
            body.contains("dw_draw_all(ctx, hdc, w, h, &refs)"),
            "一次画完那一步没接上：文字又回到每段一对绘制（每段那一对现量约 1 ms）"
        );
        assert!(
            body.contains("overlay_text_coverage_rect("),
            "叠色不再按矩形走：一次画完的蒙版上同时有好几段字，整帧叠色会互相涂色"
        );
    }
}
#[cfg(test)]
mod paint_probe_tests {
    // 本测试模同样选择性导入：引用生产项要在这里补名字。
    use super::PaintCounters;
    use super::{get_rect, paint_counters, refresh, set_visible, shutdown, spawn, HWND};

    /// 卡片一帧各段花多少 —— 分**两段**读：重算那一帧与命中缓存那一帧。
    ///
    /// 起因：2026-10-31 分臂现量，「只开卡片、采集/热键/统计全关」那一臂就是
    /// **2.38% 单核**（953.1 ms/40 s，三条样本形状全等），而整批定时器在"全关"臂上的
    /// 天花板是 0.052%。也就是说卡片这一族的成本此前从未上过表 —— 本文件里
    /// `Instant::now()` 与测试探针当时都是零命中。第一次上表读到的形状：
    /// 一帧 15.868 ms，其中逐像素圆角覆盖率 5.908 + 文字与蒙版 8.971 = **93% 是重算**。
    ///
    /// 两段用同一个窗口，只差"内容变不变"：
    /// · MISS 段每拍翻一次 `collect::set_paused` ⇒ 签名每拍都变 ⇒ 量的是重算那一帧；
    /// · HIT 段内容不动 ⇒ 量的是缓存命中那一帧（成品像素直接 memcpy 进位图）。
    /// 这条打印不断言收益，它的产物是"下一刀砍哪一段"的依据；断言只有帧数一条。
    #[test]
    #[ignore = "要起真窗口与真 DWrite/D2D 上下文：cargo test -p focusflow-native --bins -- --ignored pill_paint_cost"]
    fn pill_paint_cost_per_frame_is_printed() {
        let _serial = crate::probe_guard();
        let dir = std::env::temp_dir().join(format!("ff_pill_paint_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("临时程序目录建不出来");
        focusflow_core::paths::set_app_dir(dir.clone());

        spawn(HWND::default()).expect("卡片线程没起来");
        assert!(
            set_visible(true),
            "卡片没显示出来：一帧都不会画，这条读不到东西"
        );
        std::thread::sleep(std::time::Duration::from_millis(500));

        let hw = hwnd_raw_probe_rect();
        println!(
            "PILL_PAINT size={}x{} pixels={}",
            hw.map(|(w, _)| w).unwrap_or(0),
            hw.map(|(_, h)| h).unwrap_or(0),
            hw.map(|(w, h)| (w * h) as u64).unwrap_or(0)
        );
        for (tag, want, drive) in [
            ("MISS 重算那一帧（翻主题）", 16u64, 1u8),
            ("MISS 只变内容（打字那一头）", 16u64, 2u8),
            ("HIT  命中那一帧", 16u64, 0u8),
        ] {
            let d = run_phase(want, drive);
            assert!(
                d.frames >= want,
                "[{tag}] 只等到 {} 帧（要 {want} 帧）：样本不够，这条不许出数",
                d.frames
            );
            let n = d.frames as f64;
            let ms = |v: u64| v as f64 / 1e6 / n;
            println!(
                "  {tag} frames={}｜prep {:.3} surfaces {:.3} pixels {:.3} text {:.3} submit {:.3} tail {:.3}｜TOTAL {:.3} ms/帧",
                d.frames,
                ms(d.prep_ns),
                ms(d.surfaces_ns),
                ms(d.pixels_ns),
                ms(d.text_ns),
                ms(d.submit_ns),
                ms(d.tail_ns),
                ms(d.prep_ns
                    + d.surfaces_ns
                    + d.pixels_ns
                    + d.text_ns
                    + d.submit_ns
                    + d.tail_ns)
            );
            // 断言才是"缓存没把旧帧留住 / 模板没把底板省下来"的证据，两种歪法方向相反：
            // 翻主题那一腿必须**重算**（签名少带主题 ⇒ 改了主题卡片还是旧颜色，靠眼睛要等用户报）；
            // 只变内容那一腿必须**跳过底板逐像素**（打字那一头底板外观没变，重算就是白烧 CPU）。
            if drive == 1 {
                assert!(
                    d.pixels_ns > 0 && d.text_ns > 0,
                    "[{tag}] 翻了主题却没有重算（pixels={} text={}）⇒ 缓存签名没跟着主题走，屏上会留旧帧",
                    d.pixels_ns,
                    d.text_ns
                );
            }
            if drive == 2 {
                let per_text = d.text_ns / d.frames.max(1);
                let per_px = d.pixels_ns / d.frames.max(1);
                assert!(
                    per_text > 0,
                    "[{tag}] 内容变了却没重画文字：帧缓存把旧数字留在屏上了"
                );
                // 逐像素那一段本机实测 2.9~3.9 ms/帧；paste 命中只有 memcpy（十微秒级）。
                // 门槛放 0.2 ms/帧：命中要成立，没命中（0.5 ms 也远远越不过去）要红。
                assert!(
                    per_px < 200_000,
                    "[{tag}] 外观没变却仍在逐像素算底板（pixels={} ns/帧，实测循环一帧约 3_000_000）\
                     ⇒ 底板模板没接上或键里带了会跟着内容变的东西",
                    per_px
                );
            }
            println!("  {tag} 里 text 那一段的子段（排版/绘制/蒙版清零/整帧叠加）：");
            println!(
                "      layout {:.3}｜draw {:.3}｜mask_clear {:.3}｜mask_overlay {:.3}｜段总 {:.3} ms/帧",
                ms(d.text_layout_ns),
                ms(d.text_draw_ns),
                ms(d.mask_clear_ns),
                ms(d.mask_overlay_ns),
                ms(d.text_ns)
            );
        }
        shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 跑够 `want` 帧，返回这一段的各纳秒增量（帧数也放在同一结构里）。
    ///
    /// `drive` 决定这一腿怎么让帧发生变化。三种是分开的，因为两把缓存的键不一样：
    /// · `1` 翻 `[gui] theme`（light↔dark）—— 主题同时是**两把**缓存的键，所以这一腿读到
    ///   的必然是"全都重算"。采集没起来时翻暂停位改不动卡片那一格（2026-10-31 第一版就是
    ///   这么错的：所谓 MISS 段读出 `pixels 0.000 / text 0.000`，其实全部命中）。
    /// · `2` 只变内容：`publish_floating` 推一个新的"今日"数过去（这个测试进程里采集没起，
    ///   `collect::today_total()` 回 None，卡片那一格读的就是推过去的值）⇒ 整帧缓存必失配、
    ///   而底板外观没变 ⇒ **底板模板真正的验收位在这一腿**。
    /// · `0` 什么都不变：两把缓存都命中。
    fn run_phase(want: u64, drive: u8) -> PaintCounters {
        let t0 = paint_counters();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(12);
        let mut flips = 0u64;
        while paint_counters().frames - t0.frames < want && std::time::Instant::now() < deadline {
            match drive {
                1 => {
                    flips += 1;
                    let _ = focusflow_core::config::instance().set(
                        "gui",
                        "theme",
                        if flips.is_multiple_of(2) {
                            "light"
                        } else {
                            "dark"
                        },
                    );
                }
                2 => {
                    flips += 1;
                    crate::publish_floating(&crate::data::Snapshot {
                        today: 1000 + flips as i64,
                        ..Default::default()
                    });
                }
                _ => {}
            }
            refresh();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let t1 = paint_counters();
        PaintCounters {
            frames: t1.frames - t0.frames,
            prep_ns: t1.prep_ns - t0.prep_ns,
            surfaces_ns: t1.surfaces_ns - t0.surfaces_ns,
            pixels_ns: t1.pixels_ns - t0.pixels_ns,
            text_ns: t1.text_ns - t0.text_ns,
            text_layout_ns: t1.text_layout_ns - t0.text_layout_ns,
            text_draw_ns: t1.text_draw_ns - t0.text_draw_ns,
            mask_clear_ns: t1.mask_clear_ns - t0.mask_clear_ns,
            mask_overlay_ns: t1.mask_overlay_ns - t0.mask_overlay_ns,
            submit_ns: t1.submit_ns - t0.submit_ns,
            tail_ns: t1.tail_ns - t0.tail_ns,
        }
    }

    fn hwnd_raw_probe_rect() -> Option<(i32, i32)> {
        let h = super::hwnd()?;
        let rc = unsafe { get_rect(h) }?;
        Some((rc.right - rc.left, rc.bottom - rc.top))
    }
}

#[cfg(test)]
mod mem_probe_tests {
    use super::{ensure_text_ctx, set_visible, shutdown, spawn, HWND};
    use windows::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;

    /// 本进程的私有工作集（KB）—— 口径跟台账 §一 一致：跨臂比较以**私有**为主。
    /// 读不到就回 0，调用方必须把 0 当成"没读到"，不许当成"没花钱"。
    ///
    /// `pub(super)` 是给隔壁 `ctx_split_tests` 复用的：那一条要把"入场费"再切成
    /// D2D 与 DWrite 两半，两半必须用**同一把尺子**量，不许抄第二份 psapi 调用。
    pub(super) fn private_kb() -> u64 {
        unsafe {
            // `PrivateUsage` 那一格只在 _EX 结构上有（psapi 按 `cb` 决定填不填它），
            // 而函数签名收的是老结构的指针 —— 这是 psapi 本来的形状。
            let cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
            let mut pmc = PROCESS_MEMORY_COUNTERS_EX {
                cb,
                ..Default::default()
            };
            let ptr = &mut pmc as *mut PROCESS_MEMORY_COUNTERS_EX as *mut PROCESS_MEMORY_COUNTERS;
            match GetProcessMemoryInfo(GetCurrentProcess(), ptr, cb) {
                Ok(()) => (pmc.PrivateUsage as u64) / 1024,
                Err(_) => 0,
            }
        }
    }

    /// 卡片那 +38.6 MB 私有（A0→A1 现量 39 530 KB）到底是谁的：这条把两个边界分开读。
    ///
    /// · 第一段只干一件事 —— 在当前（没有卡片的）线程上建 `ensure_text_ctx()`：
    ///   D2D1 DC 渲染目标 + DWrite 工厂 + 一支画笔。这一格的差值就是"**一份图形上下文的
    ///   入场费**"，四轮现量 36.6–38.1 MB —— **它自己就已经等于 A0→A1 那 38.6 MB**，
    ///   所以归因落在 D2D/DWrite 上，不在分层窗口、DIB 或字体上。
    /// · 第二段才把卡片线程起起来并让它画第一帧。它建的是**第二份**上下文（`TEXT` 是
    ///   thread_local，一线程一份），外加真窗口与两块整卡 DIB，实测 13.1–14.0 MB，
    ///   只有第一段的一半：图形栈的代码页与堆已经被第一份热过一遍了。
    ///
    /// 两段之和（≈50 MB）**大于** A0→A1 是形状使然，不是"对不上"：探针这一进程前后建了
    /// 两份上下文，真进程只有卡片线程那一份。
    /// 任何一段读不出来（`private_kb` 回 0）都算前提不成立，不许拿"差值 0"当"D2D 不值钱"的结论。
    #[test]
    #[ignore = "要建真 D2D/DWrite 上下文与真卡片窗口：cargo test -p focusflow-native --bins -- --ignored pill_memory_attribution"]
    fn pill_memory_attribution_is_printed() {
        let _serial = crate::probe_guard();
        let p0 = private_kb();
        assert!(p0 > 0, "读不到本进程的私有提交：这条没有量具，不许出数");

        let built = unsafe { ensure_text_ctx() };
        let p1 = private_kb();
        println!(
            "MEMCTX built={built} private_kb {p0} -> {p1} delta_kb={}",
            p1 as i64 - p0 as i64
        );
        assert!(
            built,
            "D2D/DWrite 上下文在这个进程里根本没建起来：第一段量的是空气"
        );
        assert!(p1 > 0, "第二段读数丢了：不许把 0 当成没花钱");

        let dir = std::env::temp_dir().join(format!("ff_pill_mem_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("临时程序目录建不出来");
        focusflow_core::paths::set_app_dir(dir.clone());
        spawn(HWND::default()).expect("卡片线程没起来");
        let shown = set_visible(true);
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let p2 = private_kb();
        println!(
            "PILL visible={shown} private_kb {p1} -> {p2} delta_kb={}",
            p2 as i64 - p1 as i64
        );
        assert!(shown, "卡片没显示出来：第二段也就没画过帧，这条读不到东西");
        shutdown();
        let _ = std::fs::remove_dir_all(&dir);
        println!(
            "MEM_ATTRIBUTION ctx={} pill={} sum={} (A0->A1 现量约 39 530 KB)",
            p1 as i64 - p0 as i64,
            p2 as i64 - p1 as i64,
            p2 as i64 - p0 as i64
        );
    }
}

#[cfg(test)]
mod text_path_ab_tests {
    use super::mem_probe_tests::private_kb;
    use super::{
        draw_text_dw, draw_text_gdi, ensure_text_ctx_kind, fr, g, Palette, Surface, TEXT,
        TEXT_TRIED,
    };
    use windows::Win32::Graphics::Direct2D::{
        D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_RENDER_TARGET_TYPE_SOFTWARE,
    };

    /// 卡片文字的三条路：DirectWrite + 默认档渲染目标（现在生产走的）、DirectWrite + 软件档
    /// 渲染目标（那条 36 MB 的候选省法），以及纯 GDI 兜底。
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum TextPath {
        Dw,
        DwSoft,
        Gdi,
    }

    /// 一张卡的初始底：整张涂成不透明的 `pal.card`。
    ///
    /// 为什么是平涂、不照 `repaint` 再算一遍圆角与边框：两条腿的**底板是同一份字节**，
    /// 差值就只能来自文字。圆角那一圈两条腿都会原样带过去，在这里重算一遍只是给量具自己加误差。
    fn flat_base(w: i32, h: i32, pal: Palette) -> Vec<u8> {
        let mut buf = vec![0u8; (w * h * 4) as usize];
        for idx in 0..(w * h) as usize {
            fr::put_premultiplied(&mut buf, idx, pal.card, 255);
        }
        buf
    }

    /// 一次对拍的现场：内容 + 物理尺寸 + 缩放 + 调色板 + 两条腿共用的那块底板。
    ///
    /// 为什么收成结构体而不是七个参数：两条腿、三档内容、两档缩放都要靠它对齐，
    /// 参数一多就会有一处把 `scale` 与 `pal` 接错顺序 —— 接错了不报错，只是两腿比的
    /// 不是同一张卡。收成一个值，错不出来了。
    struct Scene<'a> {
        content: &'a fr::PillContent,
        w: i32,
        h: i32,
        scale: f64,
        pal: Palette,
        base: &'a [u8],
    }

    /// 同一份现场，按指定那一条腿画一次，交回卡片缓冲的字节（就是屏上会显示的那些）。
    ///
    /// `fresh` 决定要不要先把 thread_local 那份上下文拆掉重建：比"两档渲染目标"那种
    /// 一腿一个档的用法必须 `true`（否则第二条腿拿的是第一条腿留下的那个 RT，比的是同一档
    /// 画两遍）；而量"画一帧多少钱"必须 `false` —— 重建本身值几十毫秒，混进去就不是每帧成本了。
    fn render(path: TextPath, s: &Scene, fresh: bool) -> Vec<u8> {
        let (w, h, scale, pal) = (s.w, s.h, s.scale, s.pal);
        unsafe {
            let screen = g::GetDC(None);
            assert!(!screen.0.is_null(), "取不到屏幕 DC：这条没有量具，不许出数");
            let hdc = g::CreateCompatibleDC(Some(screen));
            assert!(!hdc.0.is_null(), "建不出内存 DC：同上");
            let mut card = Surface::new(hdc, w, h).expect("建不出卡片 DIB");
            let mut mask = Surface::new(hdc, w, h).expect("建不出文字蒙版 DIB");
            card.buf().copy_from_slice(s.base);
            // 文字一律画在蒙版上 —— `repaint` 也是先把蒙版选上 DC 才调这两条路的。
            let _ = g::SelectObject(hdc, g::HGDIOBJ(mask.bmp.0));
            match path {
                TextPath::Dw | TextPath::DwSoft => {
                    let kind = match path {
                        TextPath::DwSoft => D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                        _ => D2D1_RENDER_TARGET_TYPE_DEFAULT,
                    };
                    if fresh {
                        // 那份上下文是 thread_local 且只建一次：不先拆掉，第二条腿拿到的
                        // 还是第一条腿那个渲染目标，比的就不是"两档"而是同一档画两遍。
                        TEXT.with(|c| *c.borrow_mut() = None);
                        TEXT_TRIED.with(|t| t.set(false));
                        assert!(
                            ensure_text_ctx_kind(kind),
                            "D2D/DWrite 上下文在这个进程里没建起来：那这一腿量的是空气"
                        );
                    } else {
                        assert!(
                            TEXT.with(|c| c.borrow().is_some()),
                            "没先 `fresh=true` 就画：这一腿连上下文都没有，画出来必然是空的"
                        );
                    }
                    draw_text_dw(hdc, &mut mask, &mut card, s.content, w, h, scale, pal);
                }
                TextPath::Gdi => {
                    draw_text_gdi(hdc, &mut mask, &mut card, s.content, w, h, scale, pal)
                }
            }
            let out = card.buf().to_vec();
            let _ = g::SelectObject(hdc, g::HGDIOBJ(std::ptr::null_mut()));
            mask.delete();
            card.delete();
            let _ = g::DeleteDC(hdc);
            let _ = g::ReleaseDC(None, screen);
            out
        }
    }

    /// 相对底板被**碰过**的像素数，以及那格按最大单通道偏差落进哪一档（0 不算）。
    ///
    /// 五档是这么定的：1–8＝8 位深度下看不出来那一头，9–31／32–95／96–190／191–255
    /// 依次是"淡边／中灰／重／接近换色"。判决线写在用例的文档注释里，跑之前就在这。
    fn touched_by_delta(buf: &[u8], base: &[u8]) -> (usize, [usize; 5]) {
        let mut buckets = [0usize; 5];
        let mut n = 0usize;
        for px in 0..buf.len() / 4 {
            let i = px * 4;
            let mut dmax = 0u32;
            for ch in 0..4 {
                dmax = dmax.max(buf[i + ch].abs_diff(base[i + ch]) as u32);
            }
            if dmax == 0 {
                continue;
            }
            n += 1;
            let slot = match dmax {
                1..=8 => 0,
                9..=31 => 1,
                32..=95 => 2,
                96..=190 => 3,
                _ => 4,
            };
            buckets[slot] += 1;
        }
        (n, buckets)
    }

    /// 两腿之间差多少：差像素数、其中偏差 >8 的像素数、以及最大单通道偏差。
    fn diff(a: &[u8], b: &[u8]) -> (usize, usize, u32) {
        let mut n = 0usize;
        let mut over8 = 0usize;
        let mut dmax_all = 0u32;
        for px in 0..a.len() / 4 {
            let i = px * 4;
            let mut d = 0u32;
            for ch in 0..4 {
                d = d.max(a[i + ch].abs_diff(b[i + ch]) as u32);
            }
            if d == 0 {
                continue;
            }
            n += 1;
            dmax_all = dmax_all.max(d);
            if d > 8 {
                over8 += 1;
            }
        }
        (n, over8, dmax_all)
    }

    /// 把那张卡按最近邻放大 8 倍写成 PNG（预乘 BGRA → 直 alpha RGBA）。
    ///
    /// 为什么放大：104×48 上抗锯齿那一圈只有几个像素，放到 8 倍才谈得上眼验。
    /// 落在 `.scratch/` 而不是 `%TEMP%`：这台机器的 `%TEMP%` 是门禁那条 `TEMP_LEAK`
    /// 判据的观察对象，别往里留东西。
    fn dump(tag: &str, buf: &[u8], w: i32, h: i32) -> std::path::PathBuf {
        const K: usize = 8;
        let (nw, nh) = (w as usize * K, h as usize * K);
        let mut rgba = vec![0u8; nw * nh * 4];
        for y in 0..nh {
            for x in 0..nw {
                let si = ((y / K) * w as usize + (x / K)) * 4;
                let di = (y * nw + x) * 4;
                let (b, gg, r, a) = (buf[si], buf[si + 1], buf[si + 2], buf[si + 3]);
                let un = |c: u8| {
                    if a == 0 {
                        0
                    } else {
                        ((c as u32 * 255) / a as u32).min(255) as u8
                    }
                };
                rgba[di] = un(r);
                rgba[di + 1] = un(gg);
                rgba[di + 2] = un(b);
                rgba[di + 3] = a;
            }
        }
        let png =
            focusflow_core::capture::encode_png(&rgba, nw as u32, nh as u32).expect("PNG 编码失败");
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.scratch");
        std::fs::create_dir_all(&dir).expect("建不出 .scratch");
        let path = dir.join(format!("textpath-{tag}.png"));
        std::fs::write(&path, &png).expect("写不出那张放大图");
        path
    }

    /// 三档内容：数字＋小字单位（最常驻的那一态）、中文「暂停」（走的是雅黑那一档字体）、
    /// 「—」（采集没起来，最短的一格）。**两腿吃的是同一个 `PillContent`**。
    fn fixtures() -> Vec<(&'static str, fr::PillContent)> {
        let m = fr::Metric::Both;
        vec![
            (
                "nums",
                fr::build_content(m, 12345, 3 * 3600 + 30 * 60, false, true, false, 41),
            ),
            ("paused", fr::build_content(m, 7, 95, false, true, true, 0)),
            ("dash", fr::build_content(m, 7, 95, false, false, false, 0)),
        ]
    }

    /// 卡片走 GDI 而不是 DirectWrite 到底会改观感多少 —— 归因（`mem_probe_tests`）说那份
    /// D2D/DWrite 上下文值 36.6–38.1 MB 私有，这条是它的另一半：**省这笔要付什么**。
    ///
    /// 判决线（跑之前定的）：`over8 == 0` 且两腿被碰过的像素数相对差 < 2%
    /// ⇒ 8 位深度下这两条路给不出可见差，"卡片永远走 GDI"是一笔纯赚；
    /// 否则把最大偏差与那一档像素数报出来，由他看图。
    ///
    /// 两条腿的**底板字节相同**（`flat_base`），所以差值只可能来自文字本身；
    /// 任何一腿"一个字都没画"都算前提不成立，`touched == 0` 直接红，不许念成"两条路一致"。
    #[test]
    #[ignore = "要真 DC + 两条文字路各画一张卡：cargo test -p focusflow-native --bin FocusFlow --release -- --ignored pill_text_path_ab"]
    fn pill_text_path_ab_is_printed() {
        let _serial = crate::probe_guard();
        let pal = fr::LIGHT;
        for (tag, content) in fixtures() {
            for scale in [1.0f64, 1.5] {
                let w_logical = 104i32;
                let h_logical = fr::content_height(content.rows.len());
                let w = fr::px(w_logical, scale).max(1);
                let h = fr::px(h_logical, scale).max(1);
                let base = flat_base(w, h, pal);
                let scene = Scene {
                    content: &content,
                    w,
                    h,
                    scale,
                    pal,
                    base: &base,
                };
                let dw = render(TextPath::Dw, &scene, true);
                let gdi = render(TextPath::Gdi, &scene, true);
                let (n_dw, b_dw) = touched_by_delta(&dw, &base);
                let (n_gdi, b_gdi) = touched_by_delta(&gdi, &base);
                let (d_all, d_over8, d_max) = diff(&dw, &gdi);
                assert!(
                    n_dw > 0 && n_gdi > 0,
                    "有一腿一个字都没画（dw={n_dw} gdi={n_gdi}）：那'两条路一致'是假的"
                );
                let rel = if n_gdi > 0 {
                    (n_dw as f64 - n_gdi as f64).abs() / n_gdi as f64
                } else {
                    1.0
                };
                println!(
                    "TEXT_PATH_AB tag={tag} scale={scale} card={w}x{h} touched dw={n_dw} gdi={n_gdi} rel={:.3} diff_pixels={d_all} diff_gt8={d_over8} max_delta={d_max}",
                    rel
                );
                println!(
                    "  档位 dw=[≤8:{}, ≤31:{}, ≤95:{}, ≤190:{}, =255:{}] gdi=[≤8:{}, ≤31:{}, ≤95:{}, ≤190:{}, =255:{}]",
                    b_dw[0], b_dw[1], b_dw[2], b_dw[3], b_dw[4], b_gdi[0], b_gdi[1], b_gdi[2],
                    b_gdi[3], b_gdi[4]
                );
                let verdict = if d_over8 == 0 && rel < 0.02 {
                    "不可见（8 位深度）"
                } else {
                    "有可见差"
                };
                println!("  VERDICT={verdict}");
                if scale == 1.0 {
                    let p1 = dump(&format!("{tag}-dw"), &dw, w, h);
                    let p2 = dump(&format!("{tag}-gdi"), &gdi, w, h);
                    println!("  PNG {} {}", p1.display(), p2.display());
                }
            }
        }
    }

    /// 生产那一条 `ensure_text_ctx()` 必须**显式**走 SOFTWARE 档 —— 钉的是调用形态，不是注释。
    ///
    /// 为什么这条值得常驻套件每轮跑：换回 DEFAULT 档之后屏上什么都不会变（上面那条对拍
    /// 六格就是字节相同、差像素 0），唯一的症状是常驻私有悄悄涨回 36 MB ——
    /// **那种回归没有眼睛抓得到，只有判据抓得到**。所以这条不 `#[ignore]`。
    ///
    /// 比较范围收在函数体之内（全文 `find` 会撞上别的函数的定义，本仓红过一次）。
    #[test]
    fn the_production_text_ctx_uses_the_software_dc_target() {
        let src = include_str!("floating.rs");
        let cut = src
            .find("#[cfg(test)]")
            .expect("测试模块的起点不见了：那生产码的边界就没法切");
        let prod = &src[..cut];
        let at = prod
            .find("unsafe fn ensure_text_ctx() -> bool")
            .expect("ensure_text_ctx 不见了：卡片文字走哪条路没人定了");
        let body = &prod[at..at + prod[at..].find("\n}").expect("函数体没收尾")];
        assert!(
            body.contains("ensure_text_ctx_kind(D2D1_RENDER_TARGET_TYPE_SOFTWARE)"),
            "生产那一条不再显式走 SOFTWARE 档：卡片常驻私有会悄悄涨回 36 MB，而屏上看不出来"
        );
        assert!(
            !body.contains("D2D1_RENDER_TARGET_TYPE_DEFAULT"),
            "生产那一条又引用了 DEFAULT 档：§十 那一刀的收益就此没了（改法见 pill_dw_software_matches_default）"
        );
    }

    /// 两档 D2D 渲染目标的像素对拍。`ctx_split_tests` 现量：DEFAULT 那一档的 DC 渲染目标
    /// 值 **37 796 KB** 私有工作集，SOFTWARE 那一档只值 **1 448 KB** —— 那 36 MB 能不能省，
    /// 就看这一条给不给"一样"。
    ///
    /// 判决线（跑之前定的）：**每一格都 `identical=true`** 才算"换档免费、这 36 MB 是纯省"；
    /// 只要有一格不等，就把差像素数与最大偏差报出来、图留下由他看，不许把"差不多"念成"一样"。
    ///
    /// 这把尺子的灵敏度不是假设来的：同一份夹具、同一个 `diff`，DWrite 对 GDI 那一腿报的是
    /// 1085 个差像素（其中 1048 个偏差 >8，见 `pill_text_path_ab_is_printed`）⇒ 这里读出 0
    /// 是"真的一个字节都没差"，不是"量具看不见"。
    #[test]
    #[ignore = "两档 D2D 渲染目标各画一张卡：cargo test -p focusflow-native --bin FocusFlow --release -- --ignored pill_dw_software_matches_default"]
    fn pill_dw_software_matches_default() {
        let _serial = crate::probe_guard();
        let pal = fr::LIGHT;
        for (tag, content) in fixtures() {
            for scale in [1.0f64, 1.5] {
                let w = fr::px(104, scale).max(1);
                let h = fr::px(fr::content_height(content.rows.len()), scale).max(1);
                let base = flat_base(w, h, pal);
                let scene = Scene {
                    content: &content,
                    w,
                    h,
                    scale,
                    pal,
                    base: &base,
                };
                // 顺手把两腿各自"建起来花了多少"读出来：这是**控制点**。
                // 没有它，"两腿字节相同"也兼容"第二条腿根本没换档、拿的是第一条腿那个
                // 渲染目标"这一种解释 —— 而那才是这条判决真正怕的东西。
                let m0 = private_kb();
                let def = render(TextPath::Dw, &scene, true);
                let m1 = private_kb();
                let soft = render(TextPath::DwSoft, &scene, true);
                let m2 = private_kb();
                // 每帧几毫秒的量不敢一次定：先 `fresh` 画一帧把该档的上下文建起来（那一次
                // 不进计时），再连画 20 帧取均值。这一格量的是**画**，不含入场费 ——
                // 含进去就变成"重建上下文值多少钱"，那是另一个问题（def 那一档确实贵几十毫秒）。
                let ms_each = |path: TextPath, n: usize| -> f64 {
                    let _ = render(path, &scene, true);
                    let t = std::time::Instant::now();
                    for _ in 0..n {
                        let _ = render(path, &scene, false);
                    }
                    t.elapsed().as_secs_f64() * 1000.0 / n as f64
                };
                let ms_def = ms_each(TextPath::Dw, 20);
                let ms_soft = ms_each(TextPath::DwSoft, 20);
                let (n_def, _) = touched_by_delta(&def, &base);
                let (n_soft, _) = touched_by_delta(&soft, &base);
                assert!(
                    n_def > 0 && n_soft > 0,
                    "有一档一个字都没画（default={n_def} software={n_soft}）：那'两档一致'是假的"
                );
                let (d_all, d_over8, d_max) = diff(&def, &soft);
                println!(
                    "DW_RT_AB tag={tag} scale={scale} card={w}x{h} touched def={n_def} soft={n_soft} identical={} diff_pixels={d_all} diff_gt8={d_over8} max_delta={d_max} mem_kb def={} soft={} ms_per_frame def={ms_def:.3} soft={ms_soft:.3}",
                    def == soft,
                    m1 as i64 - m0 as i64,
                    m2 as i64 - m1 as i64
                );
                if scale == 1.0 {
                    let p1 = dump(&format!("{tag}-def"), &def, w, h);
                    let p2 = dump(&format!("{tag}-soft"), &soft, w, h);
                    println!("  PNG {} {}", p1.display(), p2.display());
                }
            }
        }
    }
}

#[cfg(test)]
mod ctx_split_tests {
    use super::mem_probe_tests::private_kb;
    use super::wide;
    use windows::Win32::Graphics::Direct2D::Common::{
        D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
    };
    use windows::Win32::Graphics::Direct2D::{
        D2D1CreateFactory, ID2D1Factory, D2D1_FACTORY_TYPE_SINGLE_THREADED,
        D2D1_FEATURE_LEVEL_DEFAULT, D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE,
        D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_RENDER_TARGET_TYPE_SOFTWARE,
        D2D1_RENDER_TARGET_USAGE_NONE,
    };
    use windows::Win32::Graphics::DirectWrite::{
        DWriteCreateFactory, IDWriteFactory, DWRITE_FACTORY_TYPE_SHARED,
        DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
        DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING,
        DWRITE_WORD_WRAPPING_NO_WRAP,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

    /// `ensure_text_ctx()` 那份 36.6–38.1 MB 的入场费里，D2D 与 DWrite 各占多少。
    ///
    /// 为什么要切开：卡片文字的第三条出路不是"退回 GDI"（那条已经量过，观感是可见降级，
    /// 见 `text_path_ab_tests`），而是**只留 DWrite、去掉 D2D** —— 用
    /// `IDWriteGdiInterop` 把字形扫进 GDI 蒙版，观感与现在同源，而 D2D 工厂＋DC 渲染目标
    /// 那一整套可能整块省掉。值不值，取决于这一格的答案。
    ///
    /// ⚠ **各条腿必须各自一个进程跑**：同一个进程里先建的那一半会把图形栈的代码页与
    /// 字形缓存热起来，后一条腿就只量得到增量。所以各带 `#[ignore]`，分开点：
    /// `-- --ignored d2d_half_cost` / `-- --ignored dwrite_half_cost`。
    ///
    /// 现量（本机，KB 私有工作集）：**D2D 37 076｜DWrite 136** ⇒ 那笔入场费几乎全是
    /// Direct2D 的，DirectWrite 只值 0.13 MB。
    fn print_d2d_cost(kind: &str, r#type: D2D1_RENDER_TARGET_TYPE) {
        let p0 = private_kb();
        assert!(p0 > 0, "读不到本进程的私有工作集：这条没有量具，不许出数");
        unsafe {
            // 与 `ensure_text_ctx` 同一串调用、同一份属性 —— 差的就是没有 DWrite。
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)
                .expect("D2D 工厂建不起来：这一腿量的是空气");
            let p1 = private_kb();
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_IGNORE,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let rt = factory
                .CreateDCRenderTarget(&props)
                .expect("DC 渲染目标建不起来");
            let _brush = rt
                .CreateSolidColorBrush(
                    &D2D1_COLOR_F {
                        r: 1.0,
                        g: 1.0,
                        b: 1.0,
                        a: 1.0,
                    },
                    None,
                )
                .expect("画笔建不起来");
            // 读数要在三样东西都还活着的时候取：先拆再读就变成"建起来又放手"的那笔回收。
            let p2 = private_kb();
            println!(
                "CTX_SPLIT half=d2d kind={kind} factory_kb={} rt_kb={} all_kb={} (起点 {p0})",
                p1 as i64 - p0 as i64,
                p2 as i64 - p1 as i64,
                p2 as i64 - p0 as i64
            );
        }
    }

    /// 默认那一档 —— `ensure_text_ctx` 现在用的就是它。工厂与渲染目标分开读：
    /// 如果 36 MB 挂在**工厂**上，换渲染目标类型就省不掉；挂在渲染目标上才有的谈。
    #[test]
    #[ignore = "只建 D2D 那一半，要和 DWrite 那腿分进程跑：cargo test -p focusflow-native --bin FocusFlow --release -- --ignored d2d_half_cost"]
    fn d2d_half_cost_is_printed() {
        let _serial = crate::probe_guard();
        print_d2d_cost("default", D2D1_RENDER_TARGET_TYPE_DEFAULT);
    }

    /// 软件渲染目标那一档：同一套 D2D 排版与灰度抗锯齿，设备级缓存值多少。
    ///
    /// ⚠ 这条只回答"多少钱"，不回答"画出来一样不一样" —— 后者要另做像素对拍，
    /// 不许拿这一格的便宜直接当成可用的改法。
    #[test]
    #[ignore = "软件档的 D2D 渲染目标值多少：cargo test -p focusflow-native --bin FocusFlow --release -- --ignored d2d_software_cost"]
    fn d2d_software_cost_is_printed() {
        let _serial = crate::probe_guard();
        print_d2d_cost("software", D2D1_RENDER_TARGET_TYPE_SOFTWARE);
    }

    /// 只建 DWrite 那一半：工厂 + 一份文本格式 + 一次排版 + 度量，用的就是卡片那几串字。
    #[test]
    #[ignore = "只建 DWrite 那一半，要和 D2D 那腿分进程跑：cargo test -p focusflow-native --bin FocusFlow --release -- --ignored dwrite_half_cost"]
    fn dwrite_half_cost_is_printed() {
        let _serial = crate::probe_guard();
        let p0 = private_kb();
        assert!(p0 > 0, "读不到本进程的私有工作集：这条没有量具，不许出数");
        unsafe {
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)
                .expect("DWrite 工厂建不起来：这一腿量的是空气");
            let mut kept = Vec::new();
            for (face, text) in [
                ("Microsoft YaHei UI", "活跃"),
                ("Segoe UI", "12,345"),
                ("Microsoft YaHei UI", "速度"),
                ("Segoe UI", "41"),
                ("Microsoft YaHei UI", "/分"),
            ] {
                let family = wide(face);
                let locale = wide("zh-cn");
                let fmt = dwrite
                    .CreateTextFormat(
                        windows::core::PCWSTR(family.as_ptr()),
                        None,
                        DWRITE_FONT_WEIGHT(400),
                        DWRITE_FONT_STYLE_NORMAL,
                        DWRITE_FONT_STRETCH_NORMAL,
                        13.0,
                        windows::core::PCWSTR(locale.as_ptr()),
                    )
                    .unwrap_or_else(|e| panic!("{face} 这一档字体建不出文本格式：{e}"));
                let _ = fmt.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
                let _ = fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
                let _ = fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
                let u16: Vec<u16> = text.encode_utf16().collect();
                let layout = dwrite
                    .CreateTextLayout(&u16, &fmt, 96.0, 18.0)
                    .unwrap_or_else(|e| panic!("「{text}」排版失败：{e}"));
                let mut m = windows::Win32::Graphics::DirectWrite::DWRITE_TEXT_METRICS::default();
                layout.GetMetrics(&mut m).expect("度量问不出来");
                assert!(m.width > 0.0, "「{text}」量出来是零宽：这一腿没真排过版");
                kept.push(layout);
            }
            let p1 = private_kb();
            println!(
                "CTX_SPLIT half=dwrite private_kb {p0} -> {p1} delta_kb={} layouts={}",
                p1 as i64 - p0 as i64,
                kept.len()
            );
        }
    }
}

/// 卡片值那一格的宽度判据：值这一路是 NO_WRAP + CLIP（溢出连省略号都没有），
/// 所以"放得下"必须是量出来的，不是数出来的。用生产格式化函数取那串字，用卡片
/// 同一档字体（13px Segoe UI）排版，再跟**最窄**卡片（`MIN_W`）下的值格比。
#[cfg(test)]
mod count_width_tests {
    use super::*;
    use crate::floating_render as fr;

    unsafe fn text_width(dwrite: &IDWriteFactory, text: &str) -> f32 {
        unsafe {
            let family = wide("Segoe UI");
            let locale = wide("zh-cn");
            let fmt: IDWriteTextFormat = dwrite
                .CreateTextFormat(
                    windows::core::PCWSTR(family.as_ptr()),
                    None,
                    DWRITE_FONT_WEIGHT(400),
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    fr::VALUE_PX as f32,
                    windows::core::PCWSTR(locale.as_ptr()),
                )
                .expect("Segoe UI 建不出文本格式");
            let _ = fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
            let u16: Vec<u16> = text.encode_utf16().collect();
            let layout = dwrite
                .CreateTextLayout(&u16, &fmt, 200.0, fr::ROW_H as f32)
                .expect("值那一串排版失败");
            let mut m = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut m).expect("度量问不出来");
            m.widthIncludingTrailingWhitespace
        }
    }

    #[test]
    fn a_six_digit_count_fits_the_narrowest_card() {
        unsafe {
            let dwrite: IDWriteFactory =
                DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).expect("DWrite 工厂建不起来");
            let cell = (fr::MIN_W - fr::PAD_X - fr::LABEL_W - fr::GAP) as f32;
            for n in [99_999i64, 123_456, 999_999] {
                let text = fr::fmt_count(n);
                let w = text_width(&dwrite, &text);
                assert!(w > 0.0, "「{text}」量出来是零宽：这条没真排过版");
                assert!(
                    w < cell,
                    "「{text}」宽 {w:.2}px，最窄卡片的值格只有 {cell:.0}px"
                );
            }
            // 反向那一半：七位数确实放不下，所以 `fmt_count` 从百万起退回紧凑写法。
            let w7 = text_width(&dwrite, "1,234,567");
            assert!(
                w7 > cell,
                "七位数只量到 {w7:.2}px（值格 {cell:.0}px），那这条判据挡不住东西"
            );
        }
    }
}
