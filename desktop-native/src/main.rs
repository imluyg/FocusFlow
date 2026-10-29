// 产品形态是 GUI：不要控制台窗口。
#![windows_subsystem = "windows"]

//! FocusFlow 原生主界面 —— 原型（P0 平台 + P1a 合成渲染 + P1b 表格交互）
//! 不引入任何 UI 框架：窗口用 `CreateWindowExW`、界面 D3D11/D2D、文字 DirectWrite。
//! 托盘用 `tray-icon`、热键用 `global-hotkey`；行内改名用**原生 EDIT 子窗口**。
//!
//! 滚动容器、六个视图、截图覆盖层与贴图都已做完；剩下的缺口清单在
//! `docs/handoff-native-migration.md`。
mod collect;
mod data;
mod floating;
mod floating_render;
mod snip;

use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::sync::atomic::AtomicI64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use focusflow_core::db::queries;
use focusflow_core::db::queries::DeviceDetail;
use focusflow_core::plugins::manager::PluginManager;

use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager};
use muda::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dwm::{
    DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE,
};
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, EndPaint, GetMonitorInfoW,
    GetStockObject, InvalidateRect, LineTo, MonitorFromWindow, MoveToEx, Rectangle, SelectObject,
    SetBkColor, SetBkMode, SetTextColor, StretchDIBits, TextOutW, UpdateWindow, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
    DEFAULT_PITCH, DIB_RGB_COLORS, FW_NORMAL, HBRUSH, HDC, HFONT, HGDIOBJ, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, NULL_BRUSH, OUT_DEFAULT_PRECIS, PAINTSTRUCT, PS_SOLID, SRCCOPY,
    TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
// WM_MOUSELEAVE 在 UI::Controls 下（不在 WindowsAndMessaging）：写成裸名字会被当成
// 匹配任意值的绑定，把后面的滚轮/右键/Esc/关窗分支全变成不可达。
use windows::Win32::UI::Controls::{MARGINS, WM_MOUSELEAVE};
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows_numerics::Vector2;

use data::Snapshot;

const CLASS_NAME: PCWSTR = w!("FocusFlowNativeD2D");
const WM_REFRESH: u32 = WM_APP + 1;
/// 自动化验收用：等价于托盘「显示主窗口。
const WM_ACCEPTANCE_SHOW: u32 = WM_APP + 2;
/// 行内编辑结束：`wparam` 1=提交、2=丢弃；`lparam` 只有截图那条读，带的是输入框自己的句柄。
const WM_EDIT_DONE: u32 = WM_APP + 3;
/// 数据操作（备。
const WM_OP_DONE: u32 = WM_APP + 4;
/// 让主线程去开截图遮罩（窗口必须建在有消息循环的线程上，否则收不到任何消息）。
const WM_SNIP_START: u32 = WM_APP + 6;
/// 悬浮窗上双击：把主窗口叫出来（不受验收开关约束，这是产品行为）。
const WM_SHOW_MAIN: u32 = WM_APP + 7;
/// 自动化验收用：切一次「暂停采集」。采集是全局钩子，脚本没法点托盘菜单，
/// 而设置页那一行要靠坐标点，够不到"暂停期间一个键都不记"这种断言。
const WM_ACCEPTANCE_PAUSE: u32 = WM_APP + 8;
/// 自动化验收用：走一遍「退出」（= 托盘菜单的退出），用来验"退出前 flush + 退出备份"。
const WM_ACCEPTANCE_QUIT: u32 = WM_APP + 9;
/// 测试钩子 `FOCUSFLOW_PLUGIN_SEL`：按记录 id 走一遍"点表格一行 + 按一颗 sel 按钮"。
const WM_SEL_TEST: u32 = WM_APP + 10;
/// 启动几秒后补一份上周周报（与设置页那颗「上周周报」同一条生成路）。
const WM_WEEKLY_REPORT: u32 = WM_APP + 11;
/// 验收钩子：开 N 张合成贴图（LPARAM = N）。
const WM_PIN_TEST: u32 = WM_APP + 12;
/// 验收钩子：关掉所有贴图（与设置页那一行同一条路）。
const WM_PIN_CLOSE_ALL: u32 = WM_APP + 13;
/// 截图出图的结果（成与败都说一句）：由 `snip` 填进 `SNIP_NOTE`，投这条让主窗口取走。
/// `pub(crate)` 是因为屏上探针要拿这条消息的 id 数一数量"话到底投递了没有"。
pub(crate) const WM_SNIP_NOTE: u32 = WM_APP + 14;
const TIMER_EVENTS: usize = 1;
/// EDIT 。
const EM_SETSEL: u32 = 0x00B1;
/// 统计周期。
const PERIODS: [(i64, &str); 6] = [
    (-1, "今日"),
    (7, "7天"),
    (15, "15天"),
    (30, "30天"),
    (365, "1年"),
    (0, "总计"),
];
/// windows-rs 没导。
const WM_MOUSELEAVE_MSG: u32 = 0x02A3;

static ACCEPTANCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// ===== 主题令牌（对照旧 WebView 那份 ui/tokens.css 的 dark 档）=====

fn color(r: u8, g: u8, b: u8) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: 1.0,
    }
}
fn color_a(r: u8, g: u8, b: u8, a: f32) -> D2D1_COLOR_F {
    let mut c = color(r, g, b);
    c.a = a;
    c
}

/// `COLORREF` 是 `0x00BBGGRR`（与网页里的 RRGGBB 反着）—— GDI 那几个入口只收这个。
fn rgb_ref(r: u8, g: u8, b: u8) -> u32 {
    ((b as u32) << 16) | ((g as u32) << 8) | r as u32
}

fn rect(x: f32, y: f32, w: f32, h: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}

fn hit(r: D2D_RECT_F, x: f32, y: f32) -> bool {
    x >= r.left && x <= r.right && y >= r.top && y <= r.bottom
}

// ===== 主题令牌 =====
//
// 两套颜色，逐项照抄旧 `desktop/ui/tokens.css`：`:root` 是浅色、`body.dark` 是深色；
// `body.glass` / `body.glass.dark` 只把**页面底 / 卡片底 / 标题栏底 / 边框**改成半透明，
// 文字与强调色不随档变。
// 切换只认 `[gui] theme`：`dark` 走深色，其余（含读不到）一律浅色 —— 与旧版默认一致。

/// 一套主题的颜色。
#[derive(Clone, Copy)]
pub struct Theme {
    pub card: D2D1_COLOR_F,
    pub header: D2D1_COLOR_F,
    pub border: D2D1_COLOR_F,
    pub fg: D2D1_COLOR_F,
    pub muted: D2D1_COLOR_F,
    pub accent: D2D1_COLOR_F,
    pub accent_soft: D2D1_COLOR_F,
    pub grid: D2D1_COLOR_F,
    pub white: D2D1_COLOR_F,
    /// 达标/周末用的绿色。
    pub success: D2D1_COLOR_F,
    /// 危险/失败用的红色（插件加载失败、开机自启没生效）。
    pub danger: D2D1_COLOR_F,
    /// 弹窗底色：**始终不透明**。玻璃模式下 `card` 是半透明的，
    /// 拿它当弹窗底会把下面的表格文字透出来（tokens.css 的 `--modal-bg` 就是为这个单开的）。
    pub modal: D2D1_COLOR_F,
    /// 模态遮罩：压在整屏上的半透明层。
    pub overlay: D2D1_COLOR_F,
    /// 页面底色（清屏色）。
    pub page: D2D1_COLOR_F,
    /// 原生 EDIT 的底色与文字色：EDIT 不认 D2D，只能单独喂给 `WM_CTLCOLOREDIT`。
    pub edit_bg: (u8, u8, u8),
    pub edit_fg: (u8, u8, u8),
}

/// 一个主题的两档：玻璃（Mica 可用）与不透明（材质不可用）。
pub struct Tokens {
    pub dark: bool,
    pub glass: Theme,
    pub solid: Theme,
}

impl Tokens {
    pub fn pick(&self, glass: bool) -> &Theme {
        if glass {
            &self.glass
        } else {
            &self.solid
        }
    }
}

fn light_theme(glass: bool) -> Theme {
    let (page, card, header, border) = if glass {
        (
            color_a(0xF7, 0xFA, 0xFE, 0.86),
            color_a(0xFF, 0xFF, 0xFF, 0.62),
            color_a(0xF1, 0xF5, 0xFB, 0.72),
            color(0xE5, 0xEC, 0xF8),
        )
    } else {
        (
            color(0xF7, 0xFA, 0xFE),
            color(0xFF, 0xFF, 0xFF),
            color(0xF1, 0xF5, 0xFB),
            color(0xE5, 0xEC, 0xF8),
        )
    };
    Theme {
        card,
        header,
        border,
        fg: color(0x1A, 0x1A, 0x2E),
        muted: color(0x4A, 0x4A, 0x6A),
        accent: color(0x4D, 0x8C, 0xF7),
        accent_soft: color(0xE8, 0xF0, 0xFE),
        grid: color(0xE3, 0xEA, 0xF6),
        white: color(0xFF, 0xFF, 0xFF),
        success: color(0x10, 0xB9, 0x81),
        danger: color(0xE5, 0x48, 0x4D),
        modal: color(0xFF, 0xFF, 0xFF),
        overlay: color_a(0x18, 0x28, 0x50, 0.35),
        page,
        edit_bg: (0xFF, 0xFF, 0xFF),
        edit_fg: (0x1A, 0x1A, 0x2E),
    }
}

fn dark_theme(glass: bool) -> Theme {
    let (page, card, header, border) = if glass {
        (
            // 深色玻璃这一档沿用 §10b 调过的 0.86：壁纸只淡淡透出，界面仍是深色。
            color_a(0x10, 0x13, 0x1C, 0.86),
            color_a(0x1E, 0x23, 0x31, 0.55),
            color_a(0x23, 0x2A, 0x3C, 0.70),
            color_a(0x2A, 0x30, 0x42, 0.90),
        )
    } else {
        (
            color(0x10, 0x13, 0x1C),
            color(0x1E, 0x23, 0x31),
            color(0x23, 0x2A, 0x3C),
            color(0x2A, 0x30, 0x42),
        )
    };
    Theme {
        card,
        header,
        border,
        fg: color(0xE8, 0xEA, 0xEE),
        muted: color(0x8A, 0x92, 0xA6),
        accent: color(0x5B, 0x9C, 0xF7),
        accent_soft: color_a(0x22, 0x30, 0x4A, 0.95),
        grid: color(0x2E, 0x35, 0x48),
        white: color(0xFF, 0xFF, 0xFF),
        success: color(0x34, 0xD3, 0x99),
        danger: color(0xF8, 0x71, 0x71),
        modal: color(0x1E, 0x23, 0x31),
        overlay: color_a(0x05, 0x07, 0x0B, 0.55),
        page,
        edit_bg: (0x1E, 0x23, 0x31),
        edit_fg: (0xE8, 0xEA, 0xEE),
    }
}

pub fn tokens_for(name: &str) -> Tokens {
    let dark = name.trim() == "dark";
    let (solid, glass) = if dark {
        (dark_theme(false), dark_theme(true))
    } else {
        (light_theme(false), light_theme(true))
    };
    Tokens { dark, solid, glass }
}

/// 现在配置说的是哪一套（`[gui] theme`）。
pub fn theme_name() -> String {
    focusflow_core::config::instance().get("gui", "theme")
}

/// 同色换一个 alpha（图表柱/热力块按强度叠色要用）。
fn with_alpha(c: D2D1_COLOR_F, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { a, ..c }
}

// ===== 绘制资源 =====

struct Brushes {
    card: ID2D1SolidColorBrush,
    header: ID2D1SolidColorBrush,
    border: ID2D1SolidColorBrush,
    fg: ID2D1SolidColorBrush,
    muted: ID2D1SolidColorBrush,
    accent: ID2D1SolidColorBrush,
    accent_soft: ID2D1SolidColorBrush,
    grid: ID2D1SolidColorBrush,
    white: ID2D1SolidColorBrush,
    /// 达标/周末用的绿色。
    success: ID2D1SolidColorBrush,
    /// 弹窗底色**始终不透明**。玻璃模式下 `card` 是半透明的，
    /// 拿它当弹窗底会把下面的表格文字透出来（旧版 tokens.css 的 --modal-bg 就是为这个单开的）。
    modal: ID2D1SolidColorBrush,
    /// 危险/失败用的红色（插件加载失败、开机自启没生效）。
    danger: ID2D1SolidColorBrush,
    /// 模态遮罩：压在整屏上的半透明层。
    overlay: ID2D1SolidColorBrush,
}

impl Brushes {
    fn new(rt: &ID2D1RenderTarget, t: &Theme) -> windows::core::Result<Self> {
        let mk = |c: D2D1_COLOR_F| unsafe { rt.CreateSolidColorBrush(&c, None) };
        Ok(Self {
            card: mk(t.card)?,
            header: mk(t.header)?,
            border: mk(t.border)?,
            fg: mk(t.fg)?,
            muted: mk(t.muted)?,
            accent: mk(t.accent)?,
            accent_soft: mk(t.accent_soft)?,
            grid: mk(t.grid)?,
            white: mk(t.white)?,
            success: mk(t.success)?,
            danger: mk(t.danger)?,
            modal: mk(t.modal)?,
            overlay: mk(t.overlay)?,
        })
    }
}

struct Formats {
    h1: IDWriteTextFormat,
    value: IDWriteTextFormat,
    label: IDWriteTextFormat,
    body: IDWriteTextFormat,
    small: IDWriteTextFormat,
    right: IDWriteTextFormat,
    center_small: IDWriteTextFormat,
    /// 键帽上那种小一号的字（窄键放不下 11px）。
    center_tiny: IDWriteTextFormat,
    tab: IDWriteTextFormat,
    /// 设置页专用的一整套，尺寸逐项照旧 WebView `desktop/ui/styles.css`：
    /// `.ff-h2` 15/700、`.ff-name` 14/600（继承 body 14）、`.ff-desc`/`.ff-foot` 12/400、
    /// `.ff-nav button` 13/400 且 `.on` 600、`.ff-search` 与 `.ff-item .btn`/`input` 13。
    /// 三处比旧版再重一档（他 2026-10-28 点的）：按钮那颗章 13/600、页内分组小标题
    /// 与"要人处理"的状态行 12/600 —— 白字压在饱和底色上时 400 看着比实际细。
    ///
    /// 为什么单开一档而不是改 `body`/`small`：那两档主界面、表格、图表刻度都在用，
    /// 动它们等于把每个版面一起推挤（那些地方是固定矩形，字一大就裁切）。而设置页
    /// 以前整页借的是 12.5/400 与 11/400 —— 比旧版小一号又细一号，就是"字体还有差距"。
    settings_h: IDWriteTextFormat,
    row_name: IDWriteTextFormat,
    text13: IDWriteTextFormat,
    text13_em: IDWriteTextFormat,
    text12: IDWriteTextFormat,
    text12_em: IDWriteTextFormat,
    center13: IDWriteTextFormat,
}

// ===== 渲染目标 =====

/// 两条路都归到 `ID2D1RenderTarget` 上画图。
///
/// - `Comp`：D3D11 + `CreateSwapChainForComposition` + DirectComposition。
struct Target {
    rt: ID2D1RenderTarget,
    kind: Kind,
}

enum Kind {
    Comp(Comp),
    Hwnd(ID2D1HwndRenderTarget),
}

struct Comp {
    swap: IDXGISwapChain1,
    ctx: ID2D1DeviceContext,
    /// 必须先放掉它再重建：后台缓冲还被它引用时 `ResizeBuffers` 会返回 INVALID_CALL。
    bitmap: Option<ID2D1Bitmap1>,
    /// 这三个必须活着：visual 挂着 swapchain，target 挂着 visual。
    _dcomp: IDCompositionDevice,
    _dcomp_target: IDCompositionTarget,
    _visual: IDCompositionVisual,
    size: (u32, u32),
}

impl Drop for Comp {
    fn drop(&mut self) {
        // 视觉树上的根要显式摘掉：`SetRoot` 持有引用，只 drop 本地指针
        // 旧画面还会被合进窗口（缩放过一次就会看到两层错位）。
        unsafe {
            self.ctx.SetTarget(None);
            let _ = self._dcomp_target.SetRoot(None);
            let _ = self._dcomp.Commit();
        }
    }
}

impl Target {
    fn new_comp(hwnd: HWND, dpi: f32, w: u32, h: u32) -> Option<Self> {
        unsafe {
            let levels = [
                D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_10_1,
                D3D_FEATURE_LEVEL_10_0,
            ];
            let mut device: Option<ID3D11Device> = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
            .ok()?;
            let device = device?;
            let dxgi_device: IDXGIDevice = device.cast().ok()?;
            let adapter = dxgi_device.GetAdapter().ok()?;
            let factory: IDXGIFactory2 = adapter.GetParent().ok()?;

            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: w,
                Height: h,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                Stereo: BOOL(0),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                Flags: 0,
            };
            let swap = factory
                .CreateSwapChainForComposition(&device, &desc, None)
                .ok()?;

            let dcomp: IDCompositionDevice = DCompositionCreateDevice(&dxgi_device).ok()?;
            let dcomp_target = dcomp.CreateTargetForHwnd(hwnd, true).ok()?;
            let visual = dcomp.CreateVisual().ok()?;
            visual.SetContent(&swap).ok()?;
            dcomp_target.SetRoot(&visual).ok()?;
            dcomp.Commit().ok()?;

            let factory1: ID2D1Factory1 =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).ok()?;
            let d2d_device = factory1.CreateDevice(&dxgi_device).ok()?;
            let ctx = d2d_device
                .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)
                .ok()?;
            let bitmap = target_bitmap(&ctx, &swap, dpi)?;
            ctx.SetTarget(&bitmap);
            let rt: ID2D1RenderTarget = ctx.cast().ok()?;
            Some(Self {
                rt,
                kind: Kind::Comp(Comp {
                    swap,
                    ctx,
                    bitmap: Some(bitmap),
                    _dcomp: dcomp,
                    _dcomp_target: dcomp_target,
                    _visual: visual,
                    size: (w, h),
                }),
            })
        }
    }

    fn new_hwnd(factory: &ID2D1Factory, hwnd: HWND, dpi: f32, w: u32, h: u32) -> Option<Self> {
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_IGNORE,
            },
            dpiX: dpi,
            dpiY: dpi,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let hprops = D2D1_HWND_RENDER_TARGET_PROPERTIES {
            hwnd,
            pixelSize: D2D_SIZE_U {
                width: w,
                height: h,
            },
            presentOptions: D2D1_PRESENT_OPTIONS_NONE,
        };
        unsafe {
            let hwnd_rt = factory.CreateHwndRenderTarget(&props, &hprops).ok()?;
            let rt: ID2D1RenderTarget = hwnd_rt.cast().ok()?;
            Some(Self {
                rt,
                kind: Kind::Hwnd(hwnd_rt),
            })
        }
    }

    fn resize(&mut self, dpi: f32, w: u32, h: u32) {
        match &mut self.kind {
            Kind::Hwnd(rt) => {
                let size = D2D_SIZE_U {
                    width: w,
                    height: h,
                };
                unsafe {
                    let _ = rt.Resize(&size);
                }
            }
            Kind::Comp(c) => {
                if c.size == (w, h) {
                    return;
                }
                unsafe {
                    c.ctx.SetTarget(None);
                    // 先放掉位图：它持有对后台缓冲的引用，不放掉 ResizeBuffers 必失败。
                    c.bitmap = None;
                    let r =
                        c.swap
                            .ResizeBuffers(0, w, h, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0));
                    if r.is_err() {
                        tracing_note(&format!("ResizeBuffers({w}x{h}) 失败：{r:?}，下一帧重试"));
                        return;
                    }
                    if let Some(bmp) = target_bitmap(&c.ctx, &c.swap, dpi) {
                        c.ctx.SetTarget(&bmp);
                        c.bitmap = Some(bmp);
                        c.size = (w, h);
                        // 合成树也得重提一次：ResizeBuffers 换了后台缓冲。
                        let _ = c._visual.SetContent(&c.swap);
                        let _ = c._dcomp.Commit();
                    }
                }
            }
        }
    }

    fn present(&self) -> bool {
        match &self.kind {
            Kind::Hwnd(_) => true,
            Kind::Comp(c) => unsafe { c.swap.Present(1, DXGI_PRESENT(0)).is_ok() },
        }
    }
}

fn target_bitmap(
    ctx: &ID2D1DeviceContext,
    swap: &IDXGISwapChain1,
    dpi: f32,
) -> Option<ID2D1Bitmap1> {
    unsafe {
        let surface: IDXGISurface = swap.GetBuffer(0).ok()?;
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: dpi,
            dpiY: dpi,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            colorContext: ManuallyDrop::new(None),
        };
        ctx.CreateBitmapFromDxgiSurface(&surface, Some(&props)).ok()
    }
}

// ===== 应用状态 =====

/// 插件页的一行。
#[derive(Clone, Debug)]
struct PluginRow {
    name: String,
    desc: String,
    version: String,
    author: String,
    /// 文件名（不含扩展名）：启用状态的持久化标识。
    stem: String,
    enabled: bool,
    loaded: bool,
    error: Option<String>,
}

/// 行内编辑这一格是在改什么：设备改名、设置页的值，还是插件视图里的输入框。
#[derive(Clone, Debug)]
enum EditTarget {
    Device(String),
    Setting(usize),
    /// 插件视图里的 TextInput：(插件展示名, 字段名)
    PluginField(String, String),
}

/// 插件视图里打开的一个下拉。
///
/// 选项在打开那一刻从视图里拍下来：视图每秒都会重算，菜单开着时锚点不能跟着跳；
/// 选中之后走 `set_field`，插件自己重算视图。
#[derive(Clone)]
struct PvSelect {
    /// 属于哪个插件（切走/换插件时据此丢弃）。
    plugin: String,
    field: String,
    value: String,
    options: Vec<(String, String)>,
    /// 值框的矩形（客户区 DIP）——菜单挂在它下面，放不下就翻到上面。
    anchor: D2D_RECT_F,
    /// 选项很多时滚到第几项。
    scroll: usize,
}

struct App {
    d2d: ID2D1Factory,
    /// 只为持有生命周期：文本格式对象各自持有自己的引用，但工厂得活着。
    #[allow(dead_code)]
    dwrite: IDWriteFactory,
    target: Option<Target>,
    br: Option<Brushes>,
    /// 现有的 `br` 是按哪套主题、哪个玻璃档建的：`[gui] theme` 一改就要重建。
    br_pick: Option<(String, bool)>,
    fmt: Formats,
    snap: Arc<Mutex<Snapshot>>,
    tray: Option<TrayIcon>,
    /// 必须持有，否则注册的热键会在 drop 时被撤销。
    #[allow(dead_code)]
    /// 全局热键管理器与当前注册的组合键（None = 没注册）。
    hotkeys: Option<GlobalHotKeyManager>,
    /// 总开关：开 = 按 `[hotkey]` 里两行注册，关 = 一条都不占用全局键。
    /// 它是运行时状态（设置页那一行显示的就是它），改组合也走它这道闸。
    hotkeys_on: bool,
    hotkey_hk: Option<HotKey>,
    /// 截图热键（`[hotkey] snip`）。与唤出窗口的那个分开记，才能按事件 id 分流。
    snip_hk: Option<HotKey>,
    /// 标注截图热键（`[hotkey] snip_annotate`）：选区后进标注窗，而 `snip` 是松手即存。
    snip_annotate_hk: Option<HotKey>,
    /// 系统材质贴上了（Win11 Mica）。
    mica: bool,
    /// 现在真的走合成路径（透明/玻璃）。只有 `mica` 且合成成功才为 true。
    glass: bool,
    /// 只读库句柄：`init_readonly` 的副作用是全局年度缓存。
    _db: Arc<focusflow_core::db::Database>,
    /// 顶部视图：0=键鼠排行 1=应用排行 2=设备排行 3=活跃分析 4=插件 5=设置。
    view: usize,
    /// 活跃分析页内的子页签：0=趋势 1=小时 2=星期 3=热力。
    atab: usize,
    /// 键鼠榜那一屏的页内视图：0=排行 1=分组（旧 WebView 版的两颗 tab，同源同周期）。
    rank_tab: usize,
    /// 排行那一屏的筛选：0=全部 1=键盘 2=鼠标。
    rank_filter: usize,
    /// 列表滚动到第几行（0 = 顶部）。
    scroll: usize,
    /// 当前统计周期；后台刷新线程也要读，所以放在原子量里。
    period: Arc<AtomicI64>,
    /// 设备详情弹窗（None = 没开）。打开时是模态：点击/滚动都被它接走。
    detail: Option<DeviceDetail>,
    /// 热力页上光标下的键（`heat_keys()` 的下标）。
    heat_hover: Option<usize>,
    /// 分析页趋势/小时图上光标下的那一格（下标跟着子页签换序列）。
    chart_hover: Option<usize>,
    /// 设置页底部那条状态：上一次数据操作的结果 / "正在…"。
    status: Option<String>,
    /// 有后台操作在跑（按钮置灰，避免连点）。
    busy: bool,
    /// 设置页的搜索词（搜索框里的文字，过滤清单用）。
    search: String,
    /// 设置页左侧当前分类（0=常规…，见 [`SETTINGS_CATS`]）。
    settings_cat: usize,
    /// 设置页里那个常驻的搜索框（与行内编辑用的那个 EDIT 不是同一个）。
    search_edit: Option<HWND>,
    /// 后台操作把结果行塞这里，再投 `WM_OP_DONE` 让界面来取。
    ops: Arc<Mutex<Vec<String>>>,
    /// 插件管理器（Lua 不是 Send，只能待在 UI 线程）。
    pm: Option<PluginManager>,
    /// 插件列表（从 plugins/ 目录扫出来的）。
    plugin_rows: Vec<PluginRow>,
    /// 扫描插件目录失败的原因。
    plugin_err: Option<String>,
    /// 这一轮进程里有没有把启用的插件真正加载过（加载才会出错，插件页要给原因）。
    plugins_loaded: bool,
    /// 正在看哪个插件的视图（None = 插件清单）。
    plugin_open: Option<String>,
    /// 插件视图的纵向滚动（像素）。
    pv_scroll: i32,
    /// 插件视图里打开的下拉（None = 没开）。
    pv_select: Option<PvSelect>,
    /// 本地打开的 `modal_form` id（插件自己 `open=true` 的弹窗不走这里）。
    pv_modal: Option<String>,
    /// 插件表格的选中行：键是分组名（不分组用空串），值是记录 id，按点击顺序。
    /// `sel` 按钮拼的动作 id 与行高亮都读这一份；开/关视图时清掉（旧版前端口径：
    /// 关掉面板就不留选中），切换分组互不影响。
    plugin_sel: HashMap<String, Vec<String>>,
    /// 上一次重算插件视图的时间戳（1 秒一次就够，别跟着 100ms 心跳跑）。
    last_pv_refresh: Option<std::time::Instant>,
    hover: Option<usize>,
    sel: Option<usize>,
    /// 行内编辑用的原生 EDIT 子窗口（懒创建，一直复用）。
    edit: Option<HWND>,
    edit_target: Option<EditTarget>,
    edit_row: Option<usize>,
    /// 按下录入：正在录哪一条热键（`settings` 数组下标）。开着的时候主窗口必须握着焦点，
    /// 否则下一键会跑到别的应用那里去，而屏上还写着"请按下组合"。
    capturing: Option<usize>,
    ui_font: HFONT,
    edit_bg: HBRUSH,
    /// EDIT 的文字色与底色（`WM_CTLCOLOREDIT` 用）。跟着主题走，与 `edit_bg` 同一份。
    edit_fg: COLORREF,
    edit_bg_color: COLORREF,
    /// 托盘里的「暂停采集」勾选项：状态变化时要把勾一起改掉。
    pause_item: Option<CheckMenuItem>,
    /// 上一次画出来的「次/分」：100ms 心跳里对一次，变了才重画。
    last_cpm: i64,
}

/// 启动时主窗口那一格状态行说的话：六句候选里只留最要紧的一句（`None` = 一切正常，留空）。
///
/// 顺序就是优先级，写在这里是为了能用例盯着 —— 原先这串 `or_else` 直接长在建窗代码里，
/// 一次证据都没有。排法的道理：
/// 搬迁没成（数据可能一分为二，最贵的后果）> 配置整份没读出来（本次跑的全是默认值，
/// 连热键都是关的）> 数据目录回落（真历史在原地没人用，界面看着就是"数字全没了"）>
/// 双计提醒 > 刚搬完告知搬到哪 > 某条热键没注册上。
/// 后两条原先就在，前三条里新增的那两句此前**只有日志一行**，屏上零信号。
fn boot_notice(
    move_failure: Option<String>,
    config_corrupt: Option<String>,
    data_home_fallback: Option<String>,
    legacy_double_count: Option<String>,
    moved_ok: Option<String>,
    hotkey_warning: Option<String>,
) -> Option<String> {
    move_failure
        .or(config_corrupt)
        .or(data_home_fallback)
        .or(legacy_double_count)
        .or(moved_ok)
        .or(hotkey_warning)
}

/// 开机之后「全局热键」那一格显示开还是关：跟着**真正注册上的组合**走，不跟着配置键走。
///
/// 两个理由：① 配置说开而三条一条都没注册上（全被别的程序占了），屏上写"开"就是假话；
/// ② 这一格以前只看了唤出与截图两条，**漏了标注那条** —— 只注册上标注键时它显示"关"，
/// 而那颗键确实在占用全局快捷键。清单与 `COMBO_KEYS` 同一枚，不再手写第二遍。
fn hotkeys_on_after_boot(registered: [&Option<HotKey>; 3]) -> bool {
    registered.iter().any(|hk| hk.is_some())
}

/// 有没有人在看这些数。读者只有三个：主窗口（可见且没最小化）、桌面那张卡片，
/// 以及托盘 tooltip 上那句"今日 N 次"——它只在鼠标悬停那一刻被读到，所以由
/// `TrayIconEvent::Enter` 现问，不需要轮询养着。
///
/// 三者都不成立 ⇒ 快照线程那一整批聚合查询一拍都不该跑。以前这里**没有任何**可见性
/// 判断（`IsWindowVisible`/`IsIconic` 全文零命中）：程序收进托盘、卡片也关着，
/// 仍然每 2 秒查一轮。那一拍值多少毫秒、多少条语句，只在下面 `LONG_PERIOD_TICKS`
/// 那一段写一处（数出自 `crates/core/tests/perf_probe.rs`），这里不抄第二份。
fn watching(main_visible: bool, main_iconic: bool, pill_visible: bool) -> bool {
    (main_visible && !main_iconic) || pill_visible
}

/// 没人看时用来等"有人回来"的那一小片：250ms 是"点开窗口到第一帧新数"的最坏等待，
/// 空转时的代价只是一次 `IsWindowVisible`。
const IDLE_WAKE_MS: u64 = 250;

/// 现问一次"有没有人在看"。判据在 `watching`（那条有单元测试），这里只取三个读数。
fn watching_now(hwnd: HWND) -> bool {
    unsafe {
        watching(
            IsWindowVisible(hwnd).as_bool(),
            IsIconic(hwnd).as_bool(),
            floating::is_visible(),
        )
    }
}

/// 短档基准那一拍睡多少**秒**，取 `[gui] active_refresh_interval`（设置页那一行的说法是
/// "改完立即生效"，所以每一拍重新读一次，不在启动时读死）。倍率见 `snapshot_tick_ms`。
///
/// 笔误不许把界面打死：0、负数、超过 600 秒（有人写 999999）一律退回默认 1 秒。
/// 放任大数的症状是"数字再也不动"，比按默认值刷新更糟，而且看起来像程序坏了而不是配置坏了。
fn snapshot_sleep_secs(raw: i64) -> u64 {
    if (1..=600).contains(&raw) {
        raw as u64
    } else {
        DEFAULT_SNAPSHOT_SECS
    }
}

/// 快照线程这一拍实际睡多少**毫秒**：短档＝基准，长档＝基准 × `LONG_PERIOD_TICKS`。
///
/// 为什么要分档：一拍 `load()` 的查询面由"选了哪个周期"决定 —— 今日那一档只数今天，
/// 总计那一档要跨全部年度库。一个数顾不住两头：给总计 1 秒是为一格不会自己变的数常驻
/// 5% CPU；给今日 3 秒又成了"直接看的那个反而最慢"。
/// 两档各值多少毫秒、多少条语句，写在下面 `LONG_PERIOD_TICKS` 那一处（别在这里抄第二份）。
///
/// ⚠ 这一带以前引用的是「今日 7.1ms、总计 50.8ms」，那两个数出自探针里那份**已经漂开的**
/// 复刻清单（它还在调 `get_weekday_stats`、缺 `get_app_stats(Some(1))`）。本轮把复刻清单
/// 钉回 `data::load()`（`tick_replica_matches_the_production_load_body` 守着）之后，在同
/// 一份夹具上重量的是今日 8.0ms／总计 49.8ms（本轮四刀动手之前的数）—— 与旧账差一成上下，
/// 说明漂的是"哪几臂"而不是"一共多少"，**分档结论不变**；引用它的措辞换成量得准的那一份。
///
/// 周期按 `PERIODS` 的取值口径算（`-1` 今日 / `7` / `15` / `30` / `365` 一年 / `0` 总计），
/// 不是界面上那六颗胶囊的顺序。
fn snapshot_tick_ms(raw: i64, period: i64) -> u64 {
    let long = period == 0 || period >= 365;
    snapshot_sleep_secs(raw) * if long { LONG_PERIOD_TICKS } else { 1 } * 1000
}

/// 长档相对短档的倍率。**这两个数在本仓只写这一处**（`snapshot_tick_ms` 讲为什么分档，
/// `watching` 与 core 的 `[gui]` 默认表都指向这里，不再各自抄一份数字）。
///
/// 量法：`cargo test -p focusflow-core --test perf_probe -- --ignored --nocapture`，
/// 夹具是"他机器量级"那两份年度库（各 365 天、150 键/天、5 设备 × 40 键/天），
/// 复刻体 `tick_arms` 与生产 `data::load()` 的查询臂清单由一条源文本判据钉成同一份，
/// 条数由挂在 `open_ro` 上的 `connection::stmt_meter` 现量（不是手算）。
///
/// 读数（2026-10-29，本轮四刀之后）：今日 6.5ms／10 条语句，总计 48.8ms／17 条语句。
/// ⇒ 今日 1 秒基准占 0.65% CPU；总计按 3 秒占 1.6%，而按 1 秒要 4.9%。
const LONG_PERIOD_TICKS: u64 = 3;

/// 原生端短档基准的兜底值（`[gui] active_refresh_interval` 写错时用这个）。
const DEFAULT_SNAPSHOT_SECS: u64 = 1;

impl App {
    fn new(hwnd: HWND, mica: bool) -> windows::core::Result<Self> {
        // 配置单例：读 `config.ini`（没有就落一份默认值）。原型只用来定位数据目录。
        let cfg = focusflow_core::config::instance();
        // 「更改数据文件夹」是上一回按下的：标记留在 `[paths] data_migrate_from`，
        // 由**这个**进程在启动早期兑现 —— 早于采集与任何读库，否则旧进程还会往旧目录
        // 落增量，搬过去就少一截。搬不成时 core 会把数据根钉回旧目录并留一条启动说明。
        let moved = focusflow_core::data_location::run_pending_migration();
        let move_notice = focusflow_core::data_location::take_startup_notice();
        if let Some(r) = &moved {
            tracing_note(&format!(
                "数据目录已搬运：{} → {}（{} 个文件 / {} 字节，{}）",
                r.from.display(),
                r.to.display(),
                r.summary.copied,
                r.summary.bytes,
                if r.leftover_in_source > 0 {
                    format!("旧目录还剩 {} 项删不掉", r.leftover_in_source)
                } else {
                    "旧目录已清空".to_string()
                }
            ));
        }
        if let Some(n) = &move_notice {
            tracing_note(&format!("数据目录迁移：{}：{}", n.step, n.detail));
        }
        // 采集按 `[collector] enabled`（默认开）：它一起来就是全局键鼠钩子。
        // 打开时这里先起采集，再读第一份快照 —— 这样「今日」从第一帧
        // 就带着未落库增量（否则首帧会少一截）。
        if collect::enabled_in_config(cfg) {
            match collect::start(cfg) {
                Ok(()) => tracing_note("采集已启用（[collector] enabled = true）"),
                Err(e) => tracing_note(&format!("采集没起来：{e}")),
            }
        } else {
            tracing_note("采集未启用（[collector] enabled = false，可在设置页开启）");
        }
        let db = focusflow_core::db::Database::init_readonly();
        let period = Arc::new(AtomicI64::new(-1));
        let first = data::load(-1);
        publish_floating(&first);
        let snap = Arc::new(Mutex::new(first));

        unsafe {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let fmt = Formats {
                h1: text_format(&dwrite, 20.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)?,
                value: text_format(&dwrite, 23.0, DWRITE_FONT_WEIGHT_BOLD)?,
                label: text_format(&dwrite, 11.5, DWRITE_FONT_WEIGHT_NORMAL)?,
                body: text_format(&dwrite, 12.5, DWRITE_FONT_WEIGHT_NORMAL)?,
                small: text_format(&dwrite, 11.0, DWRITE_FONT_WEIGHT_NORMAL)?,
                right: right_format(&dwrite, 11.0)?,
                center_small: center_format(&dwrite, 11.0)?,
                center_tiny: center_format(&dwrite, 8.5)?,
                tab: center_format(&dwrite, 12.5)?,
                settings_h: text_format(&dwrite, 15.0, DWRITE_FONT_WEIGHT_BOLD)?,
                row_name: text_format(&dwrite, 14.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)?,
                text13: text_format(&dwrite, 13.0, DWRITE_FONT_WEIGHT_NORMAL)?,
                text13_em: text_format(&dwrite, 13.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)?,
                text12: text_format(&dwrite, 12.0, DWRITE_FONT_WEIGHT_NORMAL)?,
                text12_em: text_format(&dwrite, 12.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)?,
                center13: center_format_w(&dwrite, 13.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)?,
            };

            let (tray, pause_item) = make_tray(&snap);
            let HotkeysAtBoot {
                manager: hotkeys,
                toggle: hotkey_hk,
                snip: snip_hk,
                annotate: snip_annotate_hk,
                warning: hotkey_warning,
            } = make_hotkeys();

            // 启动时这条状态行只放一句话，按"要紧的先说"排（见 `boot_notice`）。
            // 这两条降级原先只有一行日志，而它们的症状恰好都是"程序看着正常、数字/热键不对"：
            // 配置整份没读出来 ⇒ 本次跑的全是默认值（含默认关掉的热键）；
            // 配置的数据目录用不了 ⇒ 本次在一个新建的空库里记账，真历史在原地没人用。
            // 这两句直接写在参数上、不先落局部变量：接线守卫钉的就是"调用形态"，
            // 换成变量名之后它就看不见这两句是从哪儿来的了。
            let legacy_running = if collect::is_started() && collect::legacy_running() {
                Some(
                    "注意：检测到旧版 FocusFlow.exe 在跑，两处采集会让数字翻倍；建议只留一个"
                        .to_string(),
                )
            } else {
                None
            };
            let moved_note = moved.as_ref().map(|r| {
                format!(
                    "数据目录已搬到 {}（{} 个文件 / 旧目录剩 {} 项）",
                    r.to.display(),
                    r.summary.copied,
                    r.leftover_in_source
                )
            });
            let boot_status = boot_notice(
                move_notice
                    .as_ref()
                    .map(|n| format!("注意：{}（{}）", n.detail, n.step)),
                cfg.corrupt_note(),
                focusflow_core::paths::data_home_fallback_notice(),
                legacy_running,
                moved_note,
                hotkey_warning,
            );

            // 悬浮窗：独立线程 + 独立分层窗口，主窗口收起时也常驻。
            // 起不来只记一句 —— 主界面不该因为"小卡片起不来"而失败。
            if focusflow_core::config::instance().get_bool("floating", "enabled", true) {
                match floating::spawn(hwnd) {
                    Ok(()) => {
                        floating::set_visible(true);
                        tracing_note("悬浮窗已启动");
                    }
                    Err(e) => tracing_note(&format!("悬浮窗启动失败：{e}")),
                }
            } else {
                tracing_note("悬浮窗未启动（[floating] enabled = false）");
            }

            // 原生 EDIT 的字体与底色：EDIT 不认 D2D 主题，只能单独喂。
            let dpi = GetDpiForWindow(hwnd) as f32;
            let dpi = if dpi <= 0.0 { 96.0 } else { dpi };
            let ui_font = CreateFontW(
                -((13.0 * dpi / 96.0).round() as i32),
                0,
                0,
                0,
                FW_NORMAL.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                DEFAULT_PITCH.0 as u32,
                // 和 D2D 那几档同一个开头（见 `text_format`）：原来这里直接写雅黑，
                // 搜索框与值框里敲进去的数字、拉丁字母比框外的同档字粗一号。
                w!("Segoe UI"),
            );
            // EDIT 的字体与颜色：EDIT 不认 D2D 主题，只能单独喂。
            // 颜色取当前主题的那一档（主题在设置页里一改，`ensure_brushes` 会重建这把刷子）。
            let boot_tokens = tokens_for(&theme_name());
            let boot_pal = boot_tokens.pick(true);
            let (bg_r, bg_g, bg_b) = boot_pal.edit_bg;
            let (fg_r, fg_g, fg_b) = boot_pal.edit_fg;
            let edit_bg_color = COLORREF(rgb_ref(bg_r, bg_g, bg_b));
            let edit_fg = COLORREF(rgb_ref(fg_r, fg_g, fg_b));
            let edit_bg = CreateSolidBrush(edit_bg_color);

            let s = Arc::clone(&snap);
            let p = Arc::clone(&period);
            let hwnd_val = hwnd.0 as isize;
            std::thread::Builder::new()
                .name("snapshot".into())
                .spawn(move || {
                    let mut idle = false;
                    loop {
                        // `HWND` 不是 `Send`（里面是裸指针），所以线程只带 `hwnd_val`，
                        // 每一拍就地包一个句柄出来问可见性。
                        let hwnd = HWND(hwnd_val as *mut core::ffi::c_void);
                        // 一拍睡多久，每一拍重新问一次配置（`[gui] active_refresh_interval`）：
                        // 设置页那一行写着"改完立即生效"，读死在启动时就成了一句许愿。
                        // 基准之上再按周期分档 —— 选了"总计"那一格，就不该和"今日"一样勤。
                        let tick_ms = snapshot_tick_ms(
                            focusflow_core::config::instance().get_int(
                                "gui",
                                "active_refresh_interval",
                                DEFAULT_SNAPSHOT_SECS as i64,
                            ),
                            p.load(std::sync::atomic::Ordering::Relaxed),
                        );
                        if !watching_now(hwnd) {
                            // 没人看 ⇒ 整批聚合查询一拍都不跑，只按小片睡等有人回来。
                            if !idle {
                                // 只在**跨过边界**那一次出声：这条既是给他看的"程序没在空转"，
                                // 也是验收腿唯一的观察量（每拍都写就成了刷屏）。
                                tracing_note(
                                    "快照线程 idle=on：没有读者（窗口隐藏/最小化且卡片未显示），暂停查询",
                                );
                            }
                            idle = true;
                            std::thread::sleep(Duration::from_millis(IDLE_WAKE_MS));
                            continue;
                        }
                        if idle {
                            // 刚有人看：当场查一次。让他先看到上一次留下的旧数，正是这次
                            // 要修的那个症状换了个方向回来。
                            tracing_note("快照线程 idle=off：有读者了，当场查一次并恢复按拍刷新");
                            idle = false;
                        } else {
                            std::thread::sleep(Duration::from_millis(tick_ms));
                            if !watching_now(hwnd) {
                                // 睡这一拍期间窗口又被藏起来了：整批查询省掉。
                                continue;
                            }
                        }
                        let next = data::load(p.load(std::sync::atomic::Ordering::Relaxed));
                        // 悬浮窗读的是这几格，先推过去再让主界面拿新快照。
                        publish_floating(&next);
                        // 推完要**催**一下：主界面这一拍是靠下面那条 `WM_REFRESH` 立刻重画的，
                        // 而卡片只有一个每秒自走的 `SetTimer` —— 不催就是"直接看的那个反而最慢"，
                        // 数字平均要多等半秒到一秒（`set_visible` 当年为同样的理由投这条消息）。
                        floating::refresh();
                        if let Ok(mut g) = s.lock() {
                            *g = next;
                        }
                        let _ = PostMessageW(Some(hwnd), WM_REFRESH, WPARAM(0), LPARAM(0));
                    }
                })
                .ok();

            Ok(Self {
                d2d,
                dwrite,
                target: None,
                br: None,
                br_pick: None,
                fmt,
                snap,
                tray,
                hotkeys,
                hotkeys_on: hotkeys_on_after_boot([&hotkey_hk, &snip_hk, &snip_annotate_hk]),
                hotkey_hk,
                snip_hk,
                snip_annotate_hk,
                mica,
                glass: false,
                _db: db,
                view: 0,
                atab: 0,
                rank_tab: 0,
                rank_filter: 0,
                scroll: 0,
                period,
                detail: None,
                heat_hover: None,
                chart_hover: None,
                status: boot_status,
                busy: false,
                search: String::new(),
                settings_cat: 0,
                search_edit: None,
                ops: Arc::new(Mutex::new(Vec::new())),
                pm: None,
                plugin_rows: Vec::new(),
                plugin_err: None,
                plugins_loaded: false,
                plugin_open: None,
                pv_scroll: 0,
                pv_select: None,
                pv_modal: None,
                plugin_sel: HashMap::new(),
                last_pv_refresh: None,
                hover: None,
                sel: None,
                edit: None,
                edit_target: None,
                edit_row: None,
                capturing: None,
                ui_font,
                edit_bg,
                edit_fg,
                edit_bg_color,
                pause_item,
                last_cpm: -1,
            })
        }
    }

    /// 目标不存在就建（先试合成路径，失败退。
    fn ensure_target(&mut self, hwnd: HWND) {
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(hwnd, &mut rc);
        }
        let w = (rc.right - rc.left).max(1) as u32;
        let h = (rc.bottom - rc.top).max(1) as u32;
        let dpi = unsafe { GetDpiForWindow(hwnd) } as f32;
        let dpi = if dpi <= 0.0 { 96.0 } else { dpi };

        if let Some(t) = &mut self.target {
            t.resize(dpi, w, h);
            return;
        }

        let created = if self.mica {
            Target::new_comp(hwnd, dpi, w, h)
        } else {
            None
        };
        let (t, comp_ok) = match created {
            Some(t) => (Some(t), true),
            None => (Target::new_hwnd(&self.d2d, hwnd, dpi, w, h), false),
        };
        let Some(t) = t else {
            tracing_note("渲染目标创建失败（合成与 Hwnd 两条路都起不来）");
            return;
        };
        self.glass = self.mica && comp_ok;
        // 画笔等下一帧按主题建（`ensure_brushes`）：主题名与玻璃档都在那里对账。
        self.br = None;
        self.br_pick = None;
        tracing_note(&format!(
            "渲染目标就绪：{}，glass={}",
            if comp_ok { "DComp" } else { "Hwnd" },
            self.glass
        ));
        self.target = Some(t);
    }

    /// 画笔按当前主题建；主题或玻璃档变了就重建一次（设置页里改主题当下生效）。
    fn ensure_brushes(&mut self, name: &str) {
        let stale = self.br.is_none()
            || self
                .br_pick
                .as_ref()
                .map(|(n, g)| n != name || *g != self.glass)
                .unwrap_or(true);
        if !stale {
            return;
        }
        let Some(t) = self.target.as_ref() else {
            return;
        };
        let tokens = tokens_for(name);
        let pal = tokens.pick(self.glass);
        match Brushes::new(&t.rt, pal) {
            Ok(br) => {
                self.br = Some(br);
                self.br_pick = Some((name.to_string(), self.glass));
                // EDIT 的底色/文字色跟着同一套令牌走：它不吃 D2D，只能重建 GDI 刷子。
                let (bg_r, bg_g, bg_b) = pal.edit_bg;
                let (fg_r, fg_g, fg_b) = pal.edit_fg;
                if !self.edit_bg.is_invalid() {
                    unsafe {
                        let _ = DeleteObject(HGDIOBJ(self.edit_bg.0));
                    }
                }
                self.edit_bg_color = COLORREF(rgb_ref(bg_r, bg_g, bg_b));
                self.edit_fg = COLORREF(rgb_ref(fg_r, fg_g, fg_b));
                self.edit_bg = unsafe { CreateSolidBrush(self.edit_bg_color) };
            }
            Err(e) => tracing_note(&format!("画刷创建失败：{e}")),
        }
    }

    fn drop_target(&mut self) {
        self.target = None;
        self.br = None;
        self.br_pick = None;
        self.glass = false;
    }

    fn draw(&mut self, hwnd: HWND) {
        self.ensure_target(hwnd);
        let name = theme_name();
        self.ensure_brushes(&name);
        let tokens = tokens_for(&name);
        let pal = tokens.pick(self.glass);
        let Some(t) = self.target.as_ref() else {
            return;
        };
        let Some(br) = self.br.as_ref() else { return };
        let snap = self.snap.lock().map(|s| s.clone()).unwrap_or_default();

        let size = unsafe { t.rt.GetSize() };
        // 页面底就是主题里的 `--page-bottom`：玻璃档带 alpha，让 Mica 的壁纸透出来。
        let clear = pal.page;
        unsafe {
            t.rt.BeginDraw();
            t.rt.Clear(Some(&clear));
        }
        {
            let p = P {
                rt: &t.rt,
                br,
                fmt: &self.fmt,
                pal,
            };
            let vs = ViewState {
                view: self.view,
                atab: self.atab,
                rank_tab: self.rank_tab,
                rank_filter: self.rank_filter,
                scroll: self.scroll,
                period: self.period.load(std::sync::atomic::Ordering::Relaxed),
                hover: self.hover,
                sel: self.sel,
                editing: self.edit_row,
                capturing: self.capturing,
                heat_hover: self.heat_hover,
                chart_hover: self.chart_hover,
                // 三条绑定此刻**真的**注册着哪几条（顺序同 `COMBO_KEYS`）：设置页那三行
                // 右边那句"没注册上"读的就是它。只报总开关会把"被别的程序占了"和
                // "他自己清空了"都糊成"开"。
                hotkey_live: [
                    self.hotkey_hk.is_some(),
                    self.snip_hk.is_some(),
                    self.snip_annotate_hk.is_some(),
                ],
                hotkey_on: self.hotkey_hk.is_some()
                    || self.snip_hk.is_some()
                    || self.snip_annotate_hk.is_some(),
                floating_on: floating::is_visible(),
                pin_count: PINS.lock().map(|v| v.len()).unwrap_or(0),
                dark: tokens.dark,
                collector_on: collect::is_started(),
                collector_paused: collect::is_paused(),
                cpm: collect::cpm(),
                settings_cat: self.settings_cat,
            };
            let pv = self.plugin_open.as_ref().and_then(|name| {
                self.pm
                    .as_ref()
                    .and_then(|pm| pm.get_plugin(name))
                    .and_then(|p| p.view.as_ref().map(|v| (name.as_str(), v)))
            });
            draw_screen(
                &p,
                size.width,
                size.height,
                &snap,
                vs,
                self.detail.as_ref(),
                self.status.as_deref(),
                self.busy,
                &self.search,
                &self.plugin_rows,
                self.plugin_err.as_deref(),
                pv,
                self.pv_scroll,
                self.pv_select.as_ref(),
                self.pv_modal.as_deref(),
                &self.plugin_sel,
            );
        }
        let ok = unsafe { t.rt.EndDraw(None, None).is_ok() } && t.present();
        if !ok {
            self.drop_target();
        }
    }

    fn scale(&self, hwnd: HWND) -> f32 {
        let dpi = unsafe { GetDpiForWindow(hwnd) } as f32;
        if dpi <= 0.0 {
            1.0
        } else {
            dpi / 96.0
        }
    }

    /// 客户区尺寸，换算。
    fn client_dip(&self, hwnd: HWND) -> (f32, f32) {
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(hwnd, &mut rc);
        }
        let s = self.scale(hwnd);
        (
            (rc.right - rc.left) as f32 / s,
            (rc.bottom - rc.top) as f32 / s,
        )
    }

    /// 鼠标移动：排行页更新悬停行，分析页更新悬停键 / 悬停那一格；返回是否需要重画。
    fn on_move(&mut self, hwnd: HWND, px: f32, py: f32) -> bool {
        let mut changed = false;
        // 设备详情弹窗开着 = 模态：这一屏什么都不许亮着。
        if self.detail.is_some() {
            changed |= self.hover.take().is_some();
            changed |= self.heat_hover.take().is_some();
            changed |= self.chart_hover.take().is_some();
            return changed;
        }
        let (w, h) = self.client_dip(hwnd);
        let s = self.scale(hwnd);
        let (x, y) = (px / s, py / s);

        if self.view == 3 {
            changed |= self.hover.take().is_some();
            let content = analytics_content(w, h);
            if self.atab == 3 {
                changed |= self.chart_hover.take().is_some();
                let (u, ox, oy) = heat_geom(content);
                let idx = heat_keys().iter().position(|k| {
                    let kx = ox + k.x * u;
                    let ky = oy + k.y * u;
                    x >= kx && x <= kx + k.w * u - 2.0 && y >= ky && y <= ky + k.h * u - 2.0
                });
                if idx != self.heat_hover {
                    self.heat_hover = idx;
                    changed = true;
                }
            } else {
                changed |= self.heat_hover.take().is_some();
                let card = chart_card(content);
                let idx = match self.atab {
                    0 => {
                        let n = self.snap.lock().map(|g| g.daily.len()).unwrap_or(0);
                        line_nearest(line_plot(card), n, x, y)
                    }
                    1 => {
                        let n = self.snap.lock().map(|g| g.hourly.len()).unwrap_or(0);
                        slot_nearest(hourly_plot(card), n, HOURLY_GAP, x, y)
                    }
                    // 星期那一屏不接悬停：那七根的数本来就在柱顶上。
                    _ => None,
                };
                if idx != self.chart_hover {
                    self.chart_hover = idx;
                    changed = true;
                }
            }
            return changed;
        }

        if self.view == 4 {
            changed |= self.heat_hover.take().is_some();
            changed |= self.chart_hover.take().is_some();
            let (list_top, row_h, max_rows) = plugin_geom(w, h);
            let count = self.plugin_rows.len();
            self.scroll = self.scroll.min(count.saturating_sub(max_rows));
            let mut row = None;
            for vis in 0..max_rows {
                let idx = self.scroll + vis;
                if idx >= count {
                    break;
                }
                let y0 = list_top + vis as f32 * row_h;
                if y >= y0 && y < y0 + row_h - 4.0 {
                    row = Some(idx);
                    break;
                }
            }
            if row != self.hover {
                self.hover = row;
                changed = true;
            }
            return changed;
        }

        if self.view == 5 {
            changed |= self.heat_hover.take().is_some();
            changed |= self.chart_hover.take().is_some();
            let g = settings_geom(w, h);
            let count = self
                .snap
                .lock()
                .map(|s| settings_items(&s, &self.search, self.settings_cat).len())
                .unwrap_or(0);
            self.scroll = self.scroll.min(count.saturating_sub(g.max_rows));
            let mut row = None;
            for vis in 0..g.max_rows {
                let idx = self.scroll + vis;
                if idx >= count {
                    break;
                }
                let y0 = g.list_top + vis as f32 * g.row_h;
                if y >= y0 && y < y0 + g.row_h {
                    row = Some(idx);
                    break;
                }
            }
            if row != self.hover {
                self.hover = row;
                changed = true;
            }
            return changed;
        }

        // 排行页：这条分支既不在热力图上、也不在分析页那两张图上，残留的高亮在这里撤掉。
        changed |= self.heat_hover.take().is_some();
        changed |= self.chart_hover.take().is_some();
        let g = table_geom(left_card(w, h));
        let count = self
            .snap
            .lock()
            .map(|snap| left_rows(&snap, self.view, self.rank_tab, self.rank_filter).len())
            .unwrap_or(0);
        self.scroll = self.scroll.min(count.saturating_sub(g.max_rows));

        let mut row = None;
        for vis in 0..g.max_rows {
            let idx = self.scroll + vis;
            if idx >= count {
                break;
            }
            let y0 = g.list_top + vis as f32 * g.row_h;
            if y >= y0 && y < y0 + g.row_h {
                row = Some(idx);
                break;
            }
        }
        if row != self.hover {
            self.hover = row;
            changed = true;
        }
        changed
    }

    fn on_click(&mut self, hwnd: HWND, px: f32, py: f32) {
        // 点到编辑框外面：先把这一笔提交掉（与网页版一致，失焦即保存）。
        if self.edit_target.is_some() {
            self.finish_edit(hwnd, true);
        }
        let (w, h) = self.client_dip(hwnd);
        let s = self.scale(hwnd);
        let (x, y) = (px / s, py / s);

        // 模态优先：点关闭按钮或遮罩都算关，点对话框内部不穿透。
        if self.detail.is_some() {
            let (dialog, close) = detail_rects(w, h);
            if hit(close, x, y) || !hit(dialog, x, y) {
                self.close_detail(hwnd);
            }
            return;
        }
        for (period, pill) in period_pills() {
            if hit(pill, x, y) {
                self.set_period(hwnd, period);
                return;
            }
        }

        // 顶部视图页签。
        for (i, tab) in view_tabs().iter().enumerate() {
            if hit(*tab, x, y) {
                tracing_note(&format!("点击视图 {i} at ({x:.0},{y:.0})"));
                self.view = i;
                self.scroll = 0;
                self.sel = None;
                self.hover = None;
                self.heat_hover = None;
                self.chart_hover = None;
                self.pv_select = None;
                self.pv_modal = None;
                if i == 5 {
                    self.place_search_edit(hwnd);
                } else {
                    self.hide_search_edit();
                }
                if i == 4 {
                    self.reload_plugins();
                }
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                return;
            }
        }

        if self.view == 3 {
            for (i, tab) in atab_tabs().iter().enumerate() {
                if hit(*tab, x, y) {
                    self.atab = i;
                    self.heat_hover = None;
                    self.chart_hover = None;
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    return;
                }
            }
            return;
        }
        if self.view == 4 {
            // 插件视图打开时：弹窗/下拉 → 返回 → 视图里的控件。
            if let Some(name) = self.plugin_open.clone() {
                let card = rect(16.0, 208.0, w - 32.0, h - 208.0 - 16.0);
                let pv = self.pm.as_ref().and_then(|pm| {
                    pm.get_plugin(&name)
                        .and_then(|p| p.view.as_ref())
                        .map(|v| (name.clone(), v.clone()))
                });
                let Some((n, view)) = pv else {
                    // 视图取不出来（插件加载失败/返回空）时，至少「返回」还能用。
                    let back = rect(card.left + 16.0, card.top + 12.0, 64.0, 24.0);
                    if hit(back, x, y) {
                        self.plugin_open = None;
                        self.reset_plugin_view_state();
                        unsafe {
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                    }
                    return;
                };
                // 1) 下拉菜单在最上层：点菜单里就选，点外面就关掉（这次点击继续往下走）。
                if let Some(sel) = self.pv_select.clone().filter(|s| s.plugin == n) {
                    let (menu, items, start) =
                        select_menu_geom(sel.anchor, sel.options.len(), sel.scroll, w, h);
                    if hit(menu, x, y) {
                        for (i, ir) in items.iter().enumerate() {
                            if hit(*ir, x, y) {
                                if let Some((val, _)) = sel.options.get(start + i).cloned() {
                                    self.pv_select = None;
                                    self.set_plugin_field(hwnd, &n, &sel.field, &val);
                                }
                                return;
                            }
                        }
                        return;
                    }
                    // 再点一次值框 = 收起（不然会"关了又立刻打开"）。
                    if hit(sel.anchor, x, y) {
                        self.pv_select = None;
                        unsafe {
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                        return;
                    }
                    self.pv_select = None;
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                // 2) 模态：开着就只认它（点遮罩也吞掉，不穿透到后面的控件）。
                if let Some(m) = find_modal(&view, self.pv_modal.as_deref()) {
                    let g = modal_geom(&m, w, h);
                    match modal_hit(&m, &g, x, y) {
                        Some(ModalHit::Close) | Some(ModalHit::Cancel) => {
                            self.close_plugin_modal(hwnd, &n, &m);
                        }
                        Some(ModalHit::Submit) => {
                            let submit = m.submit.to_string();
                            self.pv_modal = None;
                            self.pv_select = None;
                            // 值改完还没回车就点保存：先把 EDIT 里的内容提交掉。
                            if self.edit_target.is_some() {
                                self.finish_edit(hwnd, true);
                            }
                            self.run_plugin_action(hwnd, &n, &submit);
                        }
                        Some(ModalHit::Button(i)) => {
                            if let Some((id, _)) = m.buttons.get(i).cloned() {
                                self.run_plugin_action(hwnd, &n, &id);
                            }
                        }
                        Some(ModalHit::Field(i)) => {
                            if let Some(f) = m.fields.get(i).cloned() {
                                if let Some(row) = g.fields.get(i) {
                                    let vb = modal_field_box(*row);
                                    if f.kind == "select" {
                                        self.pv_select = Some(PvSelect {
                                            plugin: n.clone(),
                                            field: f.field.clone(),
                                            value: f.value.clone(),
                                            options: f.options.clone(),
                                            anchor: vb,
                                            scroll: 0,
                                        });
                                        unsafe {
                                            let _ = InvalidateRect(Some(hwnd), None, false);
                                        }
                                    } else {
                                        self.start_plugin_field_edit(
                                            hwnd, &n, &f.field, &f.value, vb,
                                        );
                                    }
                                }
                            }
                        }
                        None => {}
                    }
                    return;
                }
                // 3) 返回按钮
                let back = rect(card.left + 16.0, card.top + 12.0, 64.0, 24.0);
                if hit(back, x, y) {
                    self.plugin_open = None;
                    self.reset_plugin_view_state();
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    return;
                }
                // 4) 正文控件
                let sel_map = self.plugin_sel.clone();
                let ops = plugin_view_ops(
                    &view,
                    rect(
                        card.left,
                        card.top + 40.0,
                        card.right - card.left,
                        card.bottom - card.top,
                    ),
                    self.pv_scroll as f32,
                    &sel_map,
                );
                for op in &ops {
                    match op {
                        POp::Button {
                            rect: r,
                            text: _,
                            id,
                            disabled,
                            modal,
                            sel,
                            group,
                        } => {
                            if !*disabled && hit(*r, x, y) {
                                if let Some(mid) = modal {
                                    self.pv_select = None;
                                    self.pv_modal = Some(mid.clone());
                                    tracing_note(&format!("插件弹窗：{n} / {mid}"));
                                    unsafe {
                                        let _ = InvalidateRect(Some(hwnd), None, false);
                                    }
                                } else if *sel {
                                    self.press_sel_button(hwnd, &n, id, group);
                                } else {
                                    self.run_plugin_action(hwnd, &n, id);
                                }
                                return;
                            }
                        }
                        // 表格：先认行内操作按钮，再认"点了哪一行"（选中/取消）。
                        POp::Table {
                            rect: r,
                            buttons,
                            ids,
                            group,
                            onselect,
                            ..
                        } => {
                            for (br, id, _) in buttons {
                                if hit(*br, x, y) {
                                    self.run_plugin_action(hwnd, &n, id);
                                    return;
                                }
                            }
                            if ids.is_empty() {
                                return;
                            }
                            let Some(i) =
                                (0..ids.len()).find(|i| hit(pv_table_row_rect(r, *i), x, y))
                            else {
                                return;
                            };
                            self.select_plugin_row(hwnd, &n, group, onselect, &ids[i]);
                            return;
                        }
                        POp::Select {
                            rect: r,
                            value,
                            field,
                            options,
                            ..
                        } => {
                            let vb = select_value_rect(*r);
                            if hit(vb, x, y) {
                                self.pv_select = Some(PvSelect {
                                    plugin: n.clone(),
                                    field: field.clone(),
                                    value: value.clone(),
                                    options: options.clone(),
                                    anchor: vb,
                                    scroll: 0,
                                });
                                unsafe {
                                    let _ = InvalidateRect(Some(hwnd), None, false);
                                }
                                return;
                            }
                        }
                        // 输入框：点值那一格 → 开同一个行内 EDIT，回车回写给插件
                        POp::Field {
                            rect: r,
                            value,
                            field,
                            ..
                        } => {
                            let vb = rect(r.left + 146.0, r.top, 260.0, 24.0);
                            if !field.is_empty() && hit(vb, x, y) {
                                tracing_note(&format!(
                                    "点中插件字段 {field} at ({x:.0},{y:.0}) rect=({:.0},{:.0})",
                                    vb.left, vb.top
                                ));
                                self.start_plugin_field_edit(hwnd, &n, field, value, vb);
                                return;
                            }
                        }
                        _ => {}
                    }
                }
                return;
            }
            let (list_top, row_h, max_rows) = plugin_geom(w, h);
            let count = self.plugin_rows.len();
            let scroll = self.scroll.min(count.saturating_sub(max_rows));
            self.scroll = scroll;
            for vis in 0..max_rows {
                let idx = scroll + vis;
                if idx >= count {
                    break;
                }
                let y0 = list_top + vis as f32 * row_h;
                let row_rect = rect(32.0, y0, w - 64.0, row_h - 4.0);
                if hit(row_rect, x, y) {
                    self.sel = Some(idx);
                    let vb = rect(row_rect.right - 76.0, y0 + 9.0, 60.0, 22.0);
                    if hit(vb, x, y) {
                        self.toggle_plugin(hwnd, idx);
                    } else if let Some(name) = self.plugin_rows.get(idx).map(|r| r.name.clone()) {
                        // 点行本身 = 看这个插件的视图
                        self.open_plugin_view(hwnd, &name);
                    }
                    return;
                }
            }
            self.sel = None;
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            return;
        }
        if self.view == 5 {
            let g = settings_geom(w, h);
            // 左侧分类：点标题切分类，滚动回到顶部。
            for (i, _) in SETTINGS_CATS.iter().enumerate() {
                let nr = rect(
                    g.nav_x,
                    g.nav_y + i as f32 * g.cat_h,
                    g.nav_w,
                    g.cat_h - 4.0,
                );
                if hit(nr, x, y) {
                    if self.settings_cat != i {
                        self.settings_cat = i;
                        self.scroll = 0;
                        self.sel = None;
                        unsafe {
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                    }
                    return;
                }
            }
            let items = self
                .snap
                .lock()
                .ok()
                .map(|s| settings_items(&s, &self.search, self.settings_cat))
                .unwrap_or_default();
            let count = items.len();
            let scroll = self.scroll.min(count.saturating_sub(g.max_rows));
            self.scroll = scroll;
            for vis in 0..g.max_rows {
                let idx = scroll + vis;
                if idx >= count {
                    break;
                }
                // 注意变量名：这里的行顶端不能。
                let y0 = g.list_top + vis as f32 * g.row_h;
                let row_rect = rect(g.body_left, y0, w - 32.0 - g.body_left, g.row_h - 2.0);
                if hit(row_rect, x, y) {
                    self.sel = Some(idx);
                    match items[idx] {
                        SItem::Header(_) => {}
                        SItem::Action(a) => self.run_action(hwnd, a),
                        SItem::Basic(Basic::Autostart) => self.toggle_autostart(hwnd),
                        SItem::Basic(Basic::Hotkey) => self.toggle_hotkey(hwnd),
                        SItem::Basic(b @ (Basic::Snip | Basic::SnipQuick)) => {
                            if let Some(annotate) = snip_mode_of(b) {
                                snip::start(hwnd, annotate);
                            }
                        }
                        SItem::Basic(Basic::ClosePins) => self.close_pins(hwnd),
                        SItem::Basic(Basic::WeeklyReport) => self.make_weekly_report(hwnd),
                        SItem::Basic(Basic::Floating) => {
                            toggle_floating(hwnd, None);
                            unsafe {
                                let _ = InvalidateRect(Some(hwnd), None, false);
                            }
                        }
                        SItem::Basic(Basic::Collector) => self.toggle_collector(hwnd),
                        SItem::Basic(Basic::Pause) => self.toggle_pause(hwnd),
                        SItem::Basic(Basic::Theme) => toggle_theme(hwnd),
                        SItem::Basic(Basic::DataDir) => self.open_data_dir(),
                        SItem::Setting(i) => {
                            // 先问值框右沿那颗小钮：点它不该顺手把值框打开。
                            let hk = self.snap.lock().ok().and_then(|s| {
                                s.settings
                                    .get(i)
                                    .map(|r| (r.section, r.key, r.default.clone(), r.value.clone()))
                            });
                            let vb = rect(g.value_x, y0 + 9.0, g.value_w, 26.0);
                            if let Some((section, key, default, value)) = hk {
                                if hotkey_slot(section, key).is_some() {
                                    if hit(hotkey_btn_rect(vb), x, y) {
                                        // 点那颗钮先把录入态收掉：不然他刚清空的那一条，
                                        // 会被下一键随手又绑回去。
                                        self.capturing = None;
                                        // 有值 → 清空（这条从此不注册）；空着 → 写回默认组合。
                                        let v = if value.trim().is_empty() {
                                            default
                                        } else {
                                            String::new()
                                        };
                                        match focusflow_core::config::instance()
                                            .set(section, key, &v)
                                        {
                                            Ok(()) => {
                                                let msg = self.reload_hotkeys();
                                                // 这句留着 `key = 值` 那个 ASCII 形状（与
                                                // `register_combo` 同一形状）：验收脚本只能匹配
                                                // ASCII，中文夹在中间它就逮不到。
                                                tracing_note(&format!(
                                                    "热键 {key} = {v:?} 由值框右边那颗钮改过：{msg}"
                                                ));
                                                self.status = Some(msg);
                                            }
                                            Err(e) => {
                                                tracing_note(&format!("热键 {key} 写入失败：{e}"));
                                                self.status = Some(format!("热键没改：{e}"));
                                            }
                                        }
                                        self.refresh_now();
                                        unsafe {
                                            let _ = InvalidateRect(Some(hwnd), None, false);
                                        }
                                        return;
                                    }
                                    if hit(vb, x, y) {
                                        // 值框里点一下＝开始录入。这里**不再**开文本框：
                                        // 他要的正是"别让我手打字母"，而两种入口同时开着，
                                        // 屏上就没法说清这一格现在等按键还是等文字。
                                        self.capturing = Some(i);
                                        self.edit_row = None;
                                        unsafe {
                                            // 录入靠主窗口自己收键；焦点不在这里就是收不到。
                                            let _ = SetFocus(Some(hwnd));
                                            let _ = InvalidateRect(Some(hwnd), None, false);
                                        }
                                        return;
                                    }
                                }
                            }
                            let kind = self
                                .snap
                                .lock()
                                .ok()
                                .and_then(|s| s.settings.get(i).map(|r| r.kind))
                                .unwrap_or(3);
                            tracing_note(&format!("设置行 idx={i} kind={kind} at y={y:.0}"));
                            if kind == 0 {
                                self.toggle_setting(hwnd, i);
                            } else {
                                self.start_edit_setting(hwnd, i, idx);
                            }
                            unsafe {
                                let _ = InvalidateRect(Some(hwnd), None, false);
                            }
                        }
                    }
                    return;
                }
            }
            self.sel = None;
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            return;
        }
        if self.view > 2 {
            return;
        }

        // 排行页那两排胶囊先接：它们与表行共用这张卡片，位置在标题那一行里。
        if self.view == 0 {
            let pills = rank_pills(left_card(w, h));
            for (i, pill) in pills.iter().take(2).enumerate() {
                if self.rank_tab != i && hit(*pill, x, y) {
                    self.rank_tab = i;
                    self.scroll = 0;
                    self.sel = None;
                    self.hover = None;
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    return;
                }
            }
            // 后三格是筛选：分组那一屏不读它（旧版也是整个不画），所以那时不接这一笔。
            if self.rank_tab == 0 {
                for (i, pill) in pills.iter().skip(2).enumerate() {
                    if self.rank_filter != i && hit(*pill, x, y) {
                        self.rank_filter = i;
                        self.scroll = 0;
                        self.sel = None;
                        self.hover = None;
                        unsafe {
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                        return;
                    }
                }
            }
        }

        let r = left_card(w, h);
        let g = table_geom(r);
        let count = self
            .snap
            .lock()
            .map(|snap| left_rows(&snap, self.view, self.rank_tab, self.rank_filter).len())
            .unwrap_or(0);
        self.scroll = self.scroll.min(count.saturating_sub(g.max_rows));

        let mut row = None;
        for vis in 0..g.max_rows {
            let idx = self.scroll + vis;
            if idx >= count {
                break;
            }
            let y0 = g.list_top + vis as f32 * g.row_h;
            if y >= y0 && y < y0 + g.row_h && x >= r.left && x <= r.right {
                row = Some(idx);
                break;
            }
        }

        match row {
            Some(idx) => {
                self.sel = Some(idx);
                if self.view == 2 {
                    let y0 = g.list_top + (idx - self.scroll) as f32 * g.row_h;
                    let has_alias = self
                        .snap
                        .lock()
                        .ok()
                        .and_then(|s| s.devices.get(idx).map(|d| d.has_alias))
                        .unwrap_or(false);
                    if has_alias && hit(restore_button(&g, y0), x, y) {
                        self.restore_device(hwnd, idx);
                        return;
                    }
                    if hit(rename_button(&g, y0), x, y) {
                        self.start_edit(hwnd, idx);
                        return;
                    }
                    // 点行本身（不在按钮上）= 看详情。
                    self.open_detail(hwnd, idx);
                    return;
                }
            }
            None => self.sel = None,
        }
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    /// 设置页底部那三颗「执行」：备份 / 压缩 / 清理，都在后台线程跑。
    fn run_action(&mut self, hwnd: HWND, which: usize) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.status = Some(
            match which {
                0 => "正在备份…",
                1 => "正在压缩数据库…",
                _ => "正在清理 30 天前的数据…",
            }
            .to_string(),
        );
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        let ops = Arc::clone(&self.ops);
        let hwnd_val = hwnd.0 as isize;
        // 导出要在后台线程里拼文件，先把这一份快照拷出来。
        let snap_data = if which >= 3 {
            self.snap.lock().ok().map(|s| s.clone())
        } else {
            None
        };
        let spawned = std::thread::Builder::new()
            .name("data-op".into())
            .spawn(move || {
                use focusflow_core::db::maintenance::{
                    backup_database, cleanup_line, cleanup_old_data, vacuum_all, vacuum_line,
                    BackupOutcome,
                };
                let line = match which {
                    0 => {
                        let max = focusflow_core::config::instance().get_int(
                            "database",
                            "max_backups",
                            5,
                        );
                        match backup_database(max) {
                            BackupOutcome::Done { count, failed, .. } => {
                                if failed.is_empty() {
                                    format!("备份完成：{count} 份")
                                } else {
                                    format!(
                                        "备份部分失败：{count} 份，没兜住 {}",
                                        failed.join("、")
                                    )
                                }
                            }
                            BackupOutcome::Failed { reason } => format!("备份失败：{reason}"),
                            BackupOutcome::NothingToDo => {
                                "没有可备份的库（还没有数据）".to_string()
                            }
                        }
                    }
                    1 => vacuum_line(&vacuum_all()),
                    3..=5 => {
                        let Some(s) = snap_data else {
                            return;
                        };
                        let (ext, filter) = match which {
                            3 => ("csv", "CSV"),
                            4 => ("html", "HTML"),
                            _ => ("json", "JSON"),
                        };
                        let path = match std::env::var("FOCUSFLOW_EXPORT_PATH") {
                            // 测试钩子：给定了路径就不弹框；按格式换后缀，三种格式互不覆盖
                            Ok(base) => std::path::PathBuf::from(base).with_extension(ext),
                            Err(_) => {
                                let file = rfd::FileDialog::new()
                                    .set_title("导出统计报告")
                                    .set_file_name(format!(
                                        "focusflow_export_{}.{ext}",
                                        chrono::Local::now().format("%Y%m%d")
                                    ))
                                    .add_filter(filter, &[ext])
                                    .save_file();
                                match file {
                                    Some(p) => p,
                                    None => {
                                        if let Ok(mut q) = ops.lock() {
                                            q.push("已取消".to_string());
                                        }
                                        let target = HWND(hwnd_val as *mut core::ffi::c_void);
                                        let _ = unsafe {
                                            PostMessageW(
                                                Some(target),
                                                WM_OP_DONE,
                                                WPARAM(0),
                                                LPARAM(0),
                                            )
                                        };
                                        return;
                                    }
                                }
                            }
                        };
                        let body = match ext {
                            "csv" => export_csv_text(&s),
                            "html" => export_html_text(&s),
                            _ => export_json_text(&s),
                        };
                        match write_user_text(&path, &body) {
                            Ok(()) => format!("已导出：{}", path.display()),
                            Err(e) => format!("导出失败：{e}"),
                        }
                    }
                    2 => {
                        // 这句话由 core 说一遍（`cleanup_line`，与 CLI 的 `--cleanup` 同一出处）。
                        // 以前这里自己拼「已清理 N 条（保留 30 天）」，于是同一次清理在命令行说
                        // 「清理未完成」、在按钮上说的是「已清理」，而单位（每天每键一行）谁都没讲。
                        let r = cleanup_old_data(30);
                        cleanup_line(30, &r)
                    }
                    6 => {
                        let dir = match std::env::var("FOCUSFLOW_IMPORT_DIR") {
                            // 测试钩子：给了目录就不弹框
                            Ok(p) => std::path::PathBuf::from(p),
                            Err(_) => {
                                let picked = rfd::FileDialog::new()
                                    .set_title(
                                        "选择旧版数据目录（含 focusflow_YYYY.db 的那个 data 目录）",
                                    )
                                    .pick_folder();
                                match picked {
                                    Some(p) => p,
                                    None => {
                                        if let Ok(mut q) = ops.lock() {
                                            q.push("已取消".to_string());
                                        }
                                        let target = HWND(hwnd_val as *mut core::ffi::c_void);
                                        let _ = unsafe {
                                            PostMessageW(
                                                Some(target),
                                                WM_OP_DONE,
                                                WPARAM(0),
                                                LPARAM(0),
                                            )
                                        };
                                        return;
                                    }
                                }
                            }
                        };
                        let s = focusflow_core::migration::import_legacy_data(&dir);
                        let mut line = format!("导入完成：{} 个年度库", s.year_dbs.len());
                        if !s.records_by_year.is_empty() {
                            let total: i64 = s.records_by_year.iter().map(|(_, n)| *n).sum();
                            line.push_str(&format!("，共 {total} 条"));
                        }
                        if !s.copied_aux.is_empty() {
                            line.push_str(&format!("；附属库 {} 份", s.copied_aux.len()));
                        }
                        if !s.skipped.is_empty() {
                            line.push_str(&format!("；跳过 {} 项", s.skipped.len()));
                        }
                        if !s.errors.is_empty() {
                            line.push_str(&format!("；错误：{}", s.errors.join("；")));
                        }
                        line
                    }
                    7 => {
                        let src = focusflow_core::data_location::current_data_home();
                        let dst = match std::env::var("FOCUSFLOW_DATA_HOME_PICK") {
                            // 测试钩子：给了目录就不弹框，验收才能重复跑
                            Ok(p) => std::path::PathBuf::from(p),
                            Err(_) => {
                                let picked = rfd::FileDialog::new()
                                    .set_title("选择新的数据根目录（data/ 与 backup/ 会搬进去）")
                                    .pick_folder();
                                match picked {
                                    Some(p) => p,
                                    None => {
                                        if let Ok(mut q) = ops.lock() {
                                            q.push("已取消".to_string());
                                        }
                                        let target = HWND(hwnd_val as *mut core::ffi::c_void);
                                        let _ = unsafe {
                                            PostMessageW(
                                                Some(target),
                                                WM_OP_DONE,
                                                WPARAM(0),
                                                LPARAM(0),
                                            )
                                        };
                                        return;
                                    }
                                }
                            }
                        };
                        match arrange_data_move(&src, &dst) {
                            Ok(line) => line,
                            Err(e) => format!("没能安排搬迁：{e}"),
                        }
                    }
                    _ => "未知操作".to_string(),
                };
                if let Ok(mut q) = ops.lock() {
                    q.push(line);
                }
                let target = HWND(hwnd_val as *mut core::ffi::c_void);
                let _ = unsafe { PostMessageW(Some(target), WM_OP_DONE, WPARAM(0), LPARAM(0)) };
            });
        if spawned.is_err() {
            self.busy = false;
            self.status = Some("起不了后台线程".to_string());
        }
    }

    /// 开机自启：改完必须**现读**回来对得上才算成功（与旧。
    fn toggle_autostart(&mut self, hwnd: HWND) {
        let want = !self.snap.lock().map(|s| s.autostart).unwrap_or(false);
        let (ok, msg) = if want {
            focusflow_core::autostart::enable_autostart()
        } else {
            focusflow_core::autostart::disable_autostart()
        };
        if !ok {
            self.status = Some(format!("开机自启没改成：{msg}"));
        } else if focusflow_core::autostart::is_autostart_enabled() != want {
            self.status = Some(
                "开机自启没有生效：调用报了成功，但启动文件夹里对不上（可能被杀软/同步盘持有）"
                    .to_string(),
            );
        } else {
            self.status = Some(format!("开机自启已{}", if want { "打开" } else { "关闭" }));
        }
        tracing_note(self.status.as_deref().unwrap_or(""));
        self.refresh_now();
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    /// 让「真正注册的键」重新等于 `[hotkey]` 里那两行：先注销此刻注册着的，
    /// 再按总开关与配置重注册。返回一行给状态栏。
    fn reload_hotkeys(&mut self) -> String {
        unregister_combos(
            self.hotkeys.as_ref(),
            [self.hotkey_hk, self.snip_hk, self.snip_annotate_hk],
        );
        self.hotkey_hk = None;
        self.snip_hk = None;
        self.snip_annotate_hk = None;
        if !self.hotkeys_on {
            return "全局热键已关闭：三条组合都不注册".to_string();
        }
        // 清单走 `COMBO_KEYS` 那一份（开机那条路读同一枚表），这里不抄第二遍：
        // 抄漏一条的症状是"开关关掉之后还剩一条热键在占键"。
        let mut slots: [Option<HotKey>; 3] = [None, None, None];
        let mut parts: Vec<(&'static str, String, Option<String>)> = Vec::new();
        let mut offs: Vec<&'static str> = Vec::new();
        for (i, key) in combo_keys(true).iter().enumerate() {
            if hotkey_is_off(key) {
                offs.push(hotkey_label(key));
                continue;
            }
            let combo = register_combo(self.hotkeys.as_ref(), key);
            parts.push(combo.part(hotkey_label(key)));
            slots[i] = combo.hk;
        }
        self.hotkey_hk = slots[0];
        self.snip_hk = slots[1];
        self.snip_annotate_hk = slots[2];
        hotkey_status_line(&parts, &offs)
    }

    /// 全局热键总开关：三条绑定（唤出 / 截图 / 截图进标注）一起注册或一起取消，
    /// 配置键 `hotkey.enabled` 跟着写一份，**启动时也读它**（见 `make_hotkeys` 与
    /// `combo_keys`）⇒ 这一格关掉的，重启之后还是关的；设置页那一行的显示状态取自
    /// "真正注册上的组合"，所以屏上说的与按键实际会发生的是同一件事。
    fn toggle_hotkey(&mut self, hwnd: HWND) {
        self.hotkeys_on = !self.hotkeys_on;
        let msg = self.reload_hotkeys();
        let _ = focusflow_core::config::instance().set(
            "hotkey",
            "enabled",
            if self.hotkeys_on { "true" } else { "false" },
        );
        tracing_note(&msg);
        self.status = Some(msg);
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    /// 录入态收到的一击：`true` = 这条按键已经被吃掉，别再走正常流程（否则 Esc 会顺手
    /// 把主窗口收进托盘——那是 `WM_KEYDOWN` 原本的去处）。
    ///
    /// 修饰键走 `GetKeyState`（同步态），所以只有**真按键**或 `keybd_event`/`SendInput`
    /// 那种进输入队列的注入能录出带修饰的组合；从别的进程 `PostMessage` 一条
    /// `WM_KEYDOWN` 是录不出 `ctrl+…` 的——写验收腿时别拿它当夹具。
    fn capture_key(&mut self, hwnd: HWND, vk: usize) -> bool {
        let Some(setting_idx) = self.capturing else {
            return false;
        };
        const ESC: usize = 0x1B;
        if vk == ESC {
            self.capturing = None;
            self.status = Some("录入已取消：这条热键没改".to_string());
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            return true;
        }
        let combo = unsafe {
            combo_from_key(
                vk as u32,
                GetKeyState(0x11) < 0,                          // VK_CONTROL
                GetKeyState(0x12) < 0,                          // VK_MENU
                GetKeyState(0x10) < 0,                          // VK_SHIFT
                GetKeyState(0x5B) < 0 || GetKeyState(0x5C) < 0, // VK_LWIN / VK_RWIN
            )
        };
        let Some(combo) = combo else {
            // 只按了修饰键 ⇒ 继续等；不支持的键 ⇒ 说清为什么没录上。
            if let Some(why) = capture_refusal(vk as u32) {
                self.status = Some(why.to_string());
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            return true;
        };
        self.capturing = None;
        let row = self
            .snap
            .lock()
            .ok()
            .and_then(|s| s.settings.get(setting_idx).map(|r| (r.section, r.key)));
        let Some((section, key)) = row else {
            return true;
        };
        // 生成的串先过同一道校验（含"两条撞在一起"那一条）：`key_name` 那张表是手写的，
        // 万一某个名字解析器不认，症状会是"存进去一个注册不上的串"——正是要防的那族。
        if let Some(why) = combo_reject_reason(key, &combo) {
            self.status = Some(format!("热键没有改：{why}"));
            tracing_note(&format!("热键 {key} = {combo:?} 录入被拒：{why}"));
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            return true;
        }
        match focusflow_core::config::instance().set(section, key, &combo) {
            Ok(()) => {
                let msg = self.reload_hotkeys();
                tracing_note(&format!("热键 {key} = {combo:?} 由按下录入写入：{msg}"));
                self.status = Some(msg);
                self.refresh_now();
            }
            Err(e) => {
                tracing_note(&format!("热键 {key} 录入写入失败：{e}"));
                self.status = Some(format!("热键没改：{e}"));
            }
        }
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        true
    }

    /// 开/关采集。开是立即生效的；关的未来是「现在停止计数 + 下次启动不再初始化」——
    /// 写线程/应用采样/设备侧信道是 `Database::init` 起的一次性线程，core 没有对外停止接口
    /// （而且本轮不动 core），所以彻底退出要重启。
    fn toggle_collector(&mut self, hwnd: HWND) {
        let cfg = focusflow_core::config::instance();
        let msg = if collect::is_started() {
            let _ = cfg.set("collector", "enabled", "false");
            collect::stop_listening();
            "采集已停用：键鼠与设备计数已停，重启后彻底退出".to_string()
        } else {
            match collect::start(cfg) {
                Ok(()) => {
                    let _ = cfg.set("collector", "enabled", "true");
                    "采集已启用：开始记录键鼠（全局，仅本机）".to_string()
                }
                Err(e) => format!("采集没起来：{e}"),
            }
        };
        // 暂停勾选项要跟着新状态走（托盘与设置页共用同一份运行时）。
        self.sync_pause_item();
        tracing_note(&msg);
        self.status = Some(msg);
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    /// 暂停/继续采集（运行时开关，不写配置）。
    fn toggle_pause(&mut self, hwnd: HWND) {
        let msg = if !collect::is_started() {
            "采集未启用，先在上面那一行打开采集".to_string()
        } else if collect::toggle_pause() {
            "采集已暂停：键鼠与设备计数都停了".to_string()
        } else {
            "采集已继续".to_string()
        };
        self.sync_pause_item();
        tracing_note(&msg);
        self.status = Some(msg);
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    /// 托盘里的勾与真实暂停位对齐。
    fn sync_pause_item(&self) {
        if let Some(it) = &self.pause_item {
            it.set_checked(collect::is_paused());
        }
    }

    /// 插件管理器懒创建：Lua 不是 Send，只能挂。
    fn ensure_plugin_manager(&mut self) {
        if self.pm.is_none() {
            let cfg = focusflow_core::config::instance();
            self.pm = Some(PluginManager::new(cfg, Arc::clone(&self._db)));
        }
    }

    /// 重扫插件目录（进插件页、切换开关之后各来一次）。
    fn reload_plugins(&mut self) {
        self.ensure_plugin_manager();
        // 第一次进来先把「启用中的插件」真的加载一遍：只有加载才会产出错误。
        if !self.plugins_loaded {
            if let Some(pm) = self.pm.as_mut() {
                pm.load_all();
                self.plugins_loaded = true;
            }
        }
        let Some(pm) = self.pm.as_ref() else { return };
        match pm.list_discovered() {
            Ok(list) => {
                self.plugin_rows = list
                    .into_iter()
                    .map(|p| PluginRow {
                        name: p.name,
                        desc: p.desc,
                        version: p.version,
                        author: p.author,
                        stem: p.file,
                        enabled: p.enabled,
                        loaded: p.loaded,
                        error: p.error,
                    })
                    .collect();
                self.plugin_err = None;
                tracing_note(&format!("插件扫描：{} 个", self.plugin_rows.len()));
            }
            Err(e) => {
                tracing_note(&format!("插件目录读不出来：{e}"));
                self.plugin_err = Some(e);
                self.plugin_rows.clear();
            }
        }
    }

    /// 插件启用/停用：写。
    fn toggle_plugin(&mut self, hwnd: HWND, idx: usize) {
        let Some(row) = self.plugin_rows.get(idx).cloned() else {
            return;
        };
        let want = !row.enabled;
        self.ensure_plugin_manager();
        let res = match self.pm.as_mut() {
            Some(pm) => pm.set_enabled(&row.stem, want),
            None => Err("没有插件管理器".to_string()),
        };
        match res {
            Ok(()) => {
                self.status = Some(format!(
                    "插件 {} 已{}",
                    row.name,
                    if want { "启用" } else { "停用" }
                ));
                tracing_note(self.status.as_deref().unwrap_or(""));
                self.reload_plugins();
            }
            Err(e) => {
                self.status = Some(format!("插件开关失败：{e}"));
                tracing_note(self.status.as_deref().unwrap_or(""));
            }
        }
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    /// 打开某个插件的视图：先让插件重算 get_view，再读缓存。
    fn open_plugin_view(&mut self, hwnd: HWND, name: &str) {
        self.ensure_plugin_manager();
        if let Some(pm) = self.pm.as_mut() {
            if let Err(e) = pm.refresh_view(name) {
                self.status = Some(format!("插件 {name} 的视图取不出来：{e}"));
                tracing_note(self.status.as_deref().unwrap_or(""));
            }
        }
        self.plugin_open = Some(name.to_string());
        self.reset_plugin_view_state();
        tracing_note(&format!("打开插件视图：{name}"));
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    /// 插件视图里的输入框：把行内 EDIT 摆到那一格上，回车后走 `set_field`。
    fn start_plugin_field_edit(
        &mut self,
        hwnd: HWND,
        plugin: &str,
        field: &str,
        value: &str,
        cell: D2D_RECT_F,
    ) {
        let s = self.scale(hwnd);
        let edit = self.ensure_edit(hwnd);
        unsafe {
            let _ = SetWindowTextW(edit, &HSTRING::from(value));
            let _ = SetWindowPos(
                edit,
                None,
                ((cell.left - 2.0) * s) as i32,
                ((cell.top - 2.0) * s) as i32,
                ((cell.right - cell.left + 4.0) * s) as i32,
                ((cell.bottom - cell.top + 4.0) * s) as i32,
                SWP_NOZORDER | SWP_SHOWWINDOW,
            );
            let _ = SetFocus(Some(edit));
            SendMessageW(edit, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
        }
        self.edit_target = Some(EditTarget::PluginField(
            plugin.to_string(),
            field.to_string(),
        ));
        self.edit_row = None;
        tracing_note(&format!("开始编辑插件字段：{plugin} / {field}"));
    }

    /// 下拉选中 / 任何字段回写：交给插件，再让插件重算视图。
    fn set_plugin_field(&mut self, hwnd: HWND, plugin: &str, field: &str, value: &str) {
        let res = match self.pm.as_mut() {
            Some(pm) => pm.plugin_set_field(plugin, field, value),
            None => Err("没有插件管理器".to_string()),
        };
        match res {
            Ok(()) => {
                if let Some(pm) = self.pm.as_mut() {
                    let _ = pm.refresh_view(plugin);
                }
                tracing_note(&format!("插件字段已回写：{plugin} / {field} = {value:?}"));
            }
            Err(e) => {
                self.status = Some(format!("插件字段回写失败：{e}"));
                tracing_note(self.status.as_deref().unwrap_or(""));
            }
        }
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    /// 关掉插件弹窗：有取消动作就先交给插件（它要清编辑状态），本地打开的直接收起。
    fn close_plugin_modal(&mut self, hwnd: HWND, plugin: &str, m: &ModalRef<'_>) {
        if self.edit_target.is_some() {
            self.finish_edit(hwnd, false);
        }
        let cancel = m.cancel.to_string();
        self.pv_modal = None;
        self.pv_select = None;
        if cancel.is_empty() {
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
        } else {
            self.run_plugin_action(hwnd, plugin, &cancel);
        }
    }

    /// 点了一行：更新这一组的选中集，再按 `onselect` 联动写字段。
    /// 真点击与测试钩子都走这一个方法，所以钩子验过的就是点出来的那条路径。
    /// 日志里带 ASCII 的 `sel-row`，验收脚本按它核对（中文日志在 ASCII 脚本里不好匹配）。
    fn select_plugin_row(
        &mut self,
        hwnd: HWND,
        plugin: &str,
        group: &str,
        onselect: &str,
        id: &str,
    ) {
        let cur: Vec<String> = self.plugin_sel.get(group).cloned().unwrap_or_default();
        let next = apply_row_select(group, &cur, id);
        if next.is_empty() {
            self.plugin_sel.remove(group);
        } else {
            self.plugin_sel.insert(group.to_string(), next);
        }
        let count = self.plugin_sel.get(group).map(|v| v.len()).unwrap_or(0);
        tracing_note(&format!(
            "插件表格点行 sel-row group={group:?} id={id:?} 选中数={count}"
        ));
        if onselect.is_empty() {
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
        } else {
            self.set_plugin_field(hwnd, plugin, onselect, id);
        }
    }

    /// 按一颗 `button.sel`：动作 id = 按钮 id 拼上该分组的选中行 id，再交给插件。
    /// 没选中、或"一次只吃一条"的按钮选多了 ⇒ 不发，把原因摆到状态行：
    /// 让 `"1,2"` 进 `tonumber` 的结局就是"点了没反应"。
    fn press_sel_button(&mut self, hwnd: HWND, plugin: &str, button_id: &str, group: &str) {
        match sel_action_id(button_id, group, &self.plugin_sel) {
            Ok(action) => self.run_plugin_action(hwnd, plugin, &action),
            Err(why) => {
                tracing_note(&format!("插件按钮 {button_id} 没发送 sel-skip：{why}"));
                self.status = Some(why);
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
        }
    }

    /// 插件视图里的按钮：把动作 id 交给插件，再把视图重算一遍。
    fn run_plugin_action(&mut self, hwnd: HWND, name: &str, action: &str) {
        let res = match self.pm.as_mut() {
            Some(pm) => pm.plugin_action(name, action),
            None => Err("没有插件管理器".to_string()),
        };
        match res {
            Ok(()) => {
                if let Some(pm) = self.pm.as_mut() {
                    let _ = pm.refresh_view(name);
                }
                tracing_note(&format!("插件动作：{name} / {action}"));
            }
            Err(e) => {
                self.status = Some(format!("插件动作失败：{e}"));
                tracing_note(self.status.as_deref().unwrap_or(""));
            }
        }
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    /// 换视图看 / 关掉视图时，把视图里的临时状态清干净：滚动、下拉、弹窗、表格选中行。
    /// 选中态不跨视图留存（与旧版前端一致：关掉面板就不留选中），分组之间也一次全清。
    fn reset_plugin_view_state(&mut self) {
        self.pv_scroll = 0;
        self.pv_select = None;
        self.pv_modal = None;
        self.plugin_sel.clear();
    }

    /// 设置页「关掉所有贴图」：一次收掉桌面上开着的贴图。已存的 PNG 一个字都不动。
    fn close_pins(&mut self, hwnd: HWND) {
        let n = close_all_pins();
        self.status = Some(if n == 0 {
            "桌面上没有开着的贴图".to_string()
        } else {
            format!("已关掉 {n} 张贴图（截图文件都还在 data/screenshots）")
        });
        tracing_note(self.status.as_deref().unwrap_or(""));
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    /// 数据目录：交给资源管理器打开（不自己实现文件浏览）。
    /// 打开的是**当前生效**的数据根，不是程序目录 —— 两者一旦分开（用户改过数据文件夹），
    /// 按程序目录打开会让人以为数据没搬过去。
    fn open_data_dir(&self) {
        let dir = focusflow_core::data_location::current_data_home();
        match std::process::Command::new("explorer").arg(&dir).spawn() {
            Ok(_) => tracing_note(&format!("打开数据目录：{}", dir.display())),
            Err(e) => tracing_note(&format!("打不开数据目录：{e}")),
        }
    }

    /// 设置页里那个常驻的搜索框：整个进程只建一次。
    fn ensure_search_edit(&mut self, parent: HWND) -> HWND {
        if let Some(e) = self.search_edit {
            return e;
        }
        let cls = HSTRING::from("EDIT");
        let empty = HSTRING::from("");
        let created = unsafe {
            CreateWindowExW(
                WS_EX_NOPARENTNOTIFY,
                &cls,
                &empty,
                WINDOW_STYLE(WS_CHILD.0 | WS_CLIPSIBLINGS.0 | 0x0080), // WS_CHILD | CLIPSIBLINGS | ES_AUTOHSCROLL
                0,
                0,
                100,
                24,
                Some(parent),
                None,
                None,
                None,
            )
        };
        let Ok(h) = created else {
            return HWND(std::ptr::null_mut());
        };
        unsafe {
            SendMessageW(
                h,
                WM_SETFONT,
                Some(WPARAM(self.ui_font.0 as usize)),
                Some(LPARAM(1)),
            );
            // 复用同一个子类过程：只为了拿到深色底与字色（Enter/Esc 在这里没有意义，
            // `finish_edit` 看到 edit_target 为空会直接返回）。
            let _ = SetWindowSubclass(h, Some(edit_proc), 2, 0);
        }
        self.search_edit = Some(h);
        h
    }

    /// 把搜索框摆到设置页右上角那块底框里（只在设置页可见）。
    fn place_search_edit(&mut self, hwnd: HWND) {
        if self.view != 5 {
            return;
        }
        let (w, _h) = self.client_dip(hwnd);
        let s = self.scale(hwnd);
        let sb = search_box_rect(w);
        let edit = self.ensure_search_edit(hwnd);
        if edit.0.is_null() {
            return;
        }
        unsafe {
            let _ = SetWindowPos(
                edit,
                None,
                ((sb.left + 8.0) * s) as i32,
                ((sb.top + 3.0) * s) as i32,
                ((sb.right - sb.left - 16.0) * s) as i32,
                ((sb.bottom - sb.top - 6.0) * s) as i32,
                SWP_NOZORDER,
            );
            let _ = ShowWindow(edit, SW_SHOW);
        }
    }

    fn hide_search_edit(&mut self) {
        if let Some(e) = self.search_edit {
            unsafe {
                let _ = ShowWindow(e, SW_HIDE);
            }
        }
    }

    /// 还原设备名：清别。
    fn restore_device(&mut self, hwnd: HWND, idx: usize) {
        let key = self
            .snap
            .lock()
            .ok()
            .and_then(|s| s.devices.get(idx).map(|d| d.key.clone()));
        if let Some(key) = key {
            match focusflow_core::device_alias::clear(&key) {
                Ok(_) => {
                    focusflow_core::device_alias::invalidate_cache();
                    self.refresh_now();
                    tracing_note("设备名已还原");
                }
                Err(e) => tracing_note(&format!("还原设备名失败：{e}")),
            }
        }
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    /// 懒创建那个原。
    fn ensure_edit(&mut self, parent: HWND) -> HWND {
        if let Some(h) = self.edit {
            return h;
        }
        let h = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("EDIT"),
                w!(""),
                WINDOW_STYLE(WS_CHILD.0 | 0x0080), // WS_CHILD | ES_AUTOHSCROLL
                0,
                0,
                100,
                20,
                Some(parent),
                None,
                None,
                None,
            )
        }
        .unwrap_or_default();
        unsafe {
            SendMessageW(
                h,
                WM_SETFONT,
                Some(WPARAM(self.ui_font.0 as usize)),
                Some(LPARAM(1)),
            );
            let _ = SetWindowSubclass(h, Some(edit_proc), 1, 0);
        }
        self.edit = Some(h);
        h
    }

    fn start_edit(&mut self, hwnd: HWND, row: usize) {
        let dev = self
            .snap
            .lock()
            .ok()
            .and_then(|s| s.devices.get(row).cloned());
        let Some(dev) = dev else { return };
        let edit = self.ensure_edit(hwnd);
        let (w, h) = self.client_dip(hwnd);
        let s = self.scale(hwnd);
        let g = table_geom(left_card(w, h));
        let vis = row.saturating_sub(self.scroll);
        if vis >= g.max_rows {
            return;
        }
        let y = g.list_top + vis as f32 * g.row_h;
        let cell = rect(
            g.x_name - 4.0,
            y + 1.0,
            g.x_count - g.x_name - 52.0,
            g.row_h - 3.0,
        );
        unsafe {
            let _ = SetWindowTextW(edit, &HSTRING::from(dev.name.clone()));
            let _ = SetWindowPos(
                edit,
                None,
                (cell.left * s) as i32,
                (cell.top * s) as i32,
                ((cell.right - cell.left) * s) as i32,
                ((cell.bottom - cell.top) * s) as i32,
                SWP_NOZORDER | SWP_SHOWWINDOW,
            );
            let _ = SetFocus(Some(edit));
            SendMessageW(edit, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
        }
        self.edit_target = Some(EditTarget::Device(dev.key));
        self.edit_row = Some(row);
        self.sel = Some(row);
        tracing_note(&format!("开始编辑 row={row}"));
    }

    /// 设置页的一行：开关直接翻，其它类型开一个原。
    fn start_edit_setting(&mut self, hwnd: HWND, snapshot_idx: usize, item_idx: usize) {
        let row = self.snap.lock().ok().and_then(|s| {
            s.settings
                .get(snapshot_idx)
                .map(|r| (r.value.clone(), r.kind))
        });
        let Some((value, kind)) = row else { return };
        if kind == 0 {
            return;
        }
        let (w, h) = self.client_dip(hwnd);
        let s = self.scale(hwnd);
        let g = settings_geom(w, h);
        // 屏幕行号用列表下标（含分组标题），不是 `settings` 数组下标。
        let vis = item_idx.saturating_sub(self.scroll);
        if vis >= g.max_rows {
            return;
        }
        let edit = self.ensure_edit(hwnd);
        let y = g.list_top + vis as f32 * g.row_h;
        let cell = rect(g.value_x - 4.0, y + 8.0, g.value_w + 8.0, 28.0);
        unsafe {
            let _ = SetWindowTextW(edit, &HSTRING::from(value));
            let _ = SetWindowPos(
                edit,
                None,
                (cell.left * s) as i32,
                (cell.top * s) as i32,
                ((cell.right - cell.left) * s) as i32,
                ((cell.bottom - cell.top) * s) as i32,
                SWP_NOZORDER | SWP_SHOWWINDOW,
            );
            let _ = SetFocus(Some(edit));
            SendMessageW(edit, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
        }
        self.edit_target = Some(EditTarget::Setting(snapshot_idx));
        self.edit_row = Some(item_idx);
        // 同一时刻只有一种输入态：开了文本框就不可能还在等按键。
        self.capturing = None;
    }

    /// 设置页「上周周报」：把上一个完整周（周一~周日）汇总成 Markdown。
    fn make_weekly_report(&mut self, hwnd: HWND) {
        match generate_weekly_report() {
            Ok(path) => self.status = Some(format!("周报已生成：{}", path.display())),
            Err(e) => self.status = Some(format!("周报生成失败：{e}")),
        }
        if let Some(s) = &self.status {
            tracing_note(s);
        }
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    /// 开关类设置：读当前。
    fn toggle_setting(&mut self, hwnd: HWND, idx: usize) {
        let row = self.snap.lock().ok().and_then(|s| {
            s.settings
                .get(idx)
                .map(|r| (r.section, r.key, r.value.clone()))
        });
        if let Some((section, key, cur)) = row {
            let next = if cur == "true" { "false" } else { "true" };
            match focusflow_core::config::instance().set(section, key, next) {
                Ok(()) => {
                    self.refresh_now();
                    tracing_note(&format!("设置已切换 {section}.{key}={next}"));
                }
                Err(e) => tracing_note(&format!("设置写入失败 {section}.{key}: {e}")),
            }
        }
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    fn finish_edit(&mut self, hwnd: HWND, commit: bool) {
        let Some(target) = self.edit_target.take() else {
            return;
        };
        if let Some(e) = self.edit {
            // 先藏起来：藏窗口会触发 WM_KILLFOCUS，它又会投一次"提交"。
            // 而 `edit_target` 已经取空，把这一次挡掉。
            unsafe {
                let _ = ShowWindow(e, SW_HIDE);
            }
            if commit {
                let mut buf = [0u16; 256];
                let n = unsafe { GetWindowTextW(e, &mut buf) }.max(0) as usize;
                let text = String::from_utf16_lossy(&buf[..n]);
                match target {
                    EditTarget::Device(key) => {
                        let alias = focusflow_core::device_alias::clamp_alias(&text);
                        let result = if alias.is_empty() {
                            focusflow_core::device_alias::clear(&key).map(|_| ())
                        } else {
                            focusflow_core::device_alias::set(&key, &alias).map(|_| ())
                        };
                        match result {
                            Ok(()) => {
                                focusflow_core::device_alias::invalidate_cache();
                                self.refresh_now();
                                tracing_note(&format!("设备别名已保存：{alias:?}"));
                            }
                            Err(err) => tracing_note(&format!("设备别名保存失败：{err}")),
                        }
                    }
                    EditTarget::Setting(idx) => {
                        let row = self.snap.lock().ok().and_then(|s| {
                            s.settings
                                .get(idx)
                                .map(|r| (r.section, r.key, r.kind, r.default.clone()))
                        });
                        if let Some((section, key, kind, default)) = row {
                            let value = text.trim().to_string();
                            // 数字格子先验一遍再写：写进。
                            let numeric_ok = match kind {
                                1 => value.parse::<i64>().is_ok(),
                                2 => value.parse::<f64>().is_ok(),
                                _ => true,
                            };
                            // 热键组合同样先验：写进去解析不了的值会让界面显示的和注册的各说各话。
                            // 空串只有热键那几行是合法值（＝这条不注册），其余项的空串仍是
                            // "恢复默认"那个既有的手势。
                            let hotkey_err = if section == "hotkey"
                                && HOTKEY_KEYS.contains(&key)
                                && !value.is_empty()
                            {
                                combo_reject_reason(key, &value)
                            } else {
                                None
                            };
                            if !numeric_ok {
                                tracing_note(&format!(
                                    "设置 {section}.{key} 的值 {value:?} 不是数字，未写入"
                                ));
                            } else if let Some(why) = hotkey_err {
                                self.status = Some(format!("热键没有改：{why}"));
                                tracing_note(&format!("热键 {key} 未写入：{why}"));
                            } else {
                                // 热键那几行的空串是**他要的状态**（点了叉＝这条不注册），
                                // 退回默认串就等于把那一叉吞掉；其余设置项保持既有手势：清空＝恢复默认。
                                let v = match (value.is_empty(), section) {
                                    (true, "hotkey") => String::new(),
                                    (true, _) => default,
                                    (false, _) => value,
                                };
                                match focusflow_core::config::instance().set(section, key, &v) {
                                    Ok(()) => {
                                        if section == "hotkey" {
                                            let msg = self.reload_hotkeys();
                                            self.status = Some(msg);
                                        }
                                        self.refresh_now();
                                        tracing_note(&format!("设置已写入 {section}.{key}={v:?}"));
                                    }
                                    Err(err) => tracing_note(&format!(
                                        "设置写入失败 {section}.{key}: {err}"
                                    )),
                                }
                            }
                        }
                    }
                    EditTarget::PluginField(plugin, field) => {
                        // 把这一格交回插件（set_field），再让插件重算视图
                        let res = match self.pm.as_mut() {
                            Some(pm) => pm.plugin_set_field(&plugin, &field, &text),
                            None => Err("没有插件管理器".to_string()),
                        };
                        match res {
                            Ok(()) => {
                                if let Some(pm) = self.pm.as_mut() {
                                    let _ = pm.refresh_view(&plugin);
                                }
                                tracing_note(&format!(
                                    "插件字段已回写：{plugin} / {field} = {text:?}"
                                ));
                            }
                            Err(err) => {
                                self.status = Some(format!("插件字段回写失败：{err}"));
                                tracing_note(self.status.as_deref().unwrap_or(""));
                            }
                        }
                    }
                }
            }
        }
        self.edit_row = None;
        unsafe {
            let _ = SetFocus(Some(hwnd));
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    fn refresh_now(&mut self) {
        let next = data::load(self.period.load(std::sync::atomic::Ordering::Relaxed));
        publish_floating(&next);
        // 数据变了（或刚改了设置），让卡片马上重画一帧，不用等它那一秒。
        floating::refresh();
        if let Ok(mut g) = self.snap.lock() {
            *g = next;
        }
    }

    fn set_period(&mut self, hwnd: HWND, period: i64) {
        self.period
            .store(period, std::sync::atomic::Ordering::Relaxed);
        self.scroll = 0;
        self.sel = None;
        self.hover = None;
        // 换周期就是换窗口：趋势图那个下标原本指哪天，现在指别的日子了 ⇒ 当场撤掉，
        // 不然光标没动、读数会跟着周期自己换内容。
        self.chart_hover = None;
        self.refresh_now();
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    fn open_detail(&mut self, hwnd: HWND, idx: usize) {
        let key = self
            .snap
            .lock()
            .ok()
            .and_then(|s| s.devices.get(idx).map(|d| d.key.clone()));
        let Some(key) = key else { return };
        let period = self.period.load(std::sync::atomic::Ordering::Relaxed);
        // 注：这条查询在 UI 线程上跑（探针库很小）。正式版要挪到后台线程再投消息回来。
        self.detail = Some(focusflow_core::db::get_device_detail(&key, period));
        tracing_note(&format!("打开设备详情 key={key:?} period={period}"));
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    fn close_detail(&mut self, hwnd: HWND) {
        if self.detail.take().is_some() {
            tracing_note("关闭设备详情");
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
        }
    }
}

/// 画布：把 Direct2D 的调用收成几个能读的动词。
struct P<'a> {
    rt: &'a ID2D1RenderTarget,
    br: &'a Brushes,
    fmt: &'a Formats,
    /// 当前主题的原始颜色：图表柱/热力块要按强度现调 alpha，画笔里没有这一档。
    pal: &'a Theme,
}

impl P<'_> {
    fn fill(&self, r: D2D_RECT_F, b: &ID2D1SolidColorBrush) {
        unsafe {
            self.rt.FillRectangle(&r, b);
        }
    }

    /// 现调一支笔（图表柱、热力块要按强度改 alpha）。建失败就当这一块不画，不 panic。
    fn brush(&self, c: D2D1_COLOR_F) -> Option<ID2D1SolidColorBrush> {
        unsafe { self.rt.CreateSolidColorBrush(&c, None).ok() }
    }

    fn round(
        &self,
        r: D2D_RECT_F,
        radius: f32,
        fill: &ID2D1SolidColorBrush,
        border: Option<&ID2D1SolidColorBrush>,
    ) {
        let rr = D2D1_ROUNDED_RECT {
            rect: r,
            radiusX: radius,
            radiusY: radius,
        };
        unsafe {
            self.rt.FillRoundedRectangle(&rr, fill);
            if let Some(bd) = border {
                self.rt.DrawRoundedRectangle(&rr, bd, 1.0, None);
            }
        }
    }

    fn line(&self, x0: f32, y0: f32, x1: f32, y1: f32, b: &ID2D1SolidColorBrush, w: f32) {
        unsafe {
            self.rt.DrawLine(
                Vector2 { X: x0, Y: y0 },
                Vector2 { X: x1, Y: y1 },
                b,
                w,
                None,
            );
        }
    }

    fn text(&self, s: &str, r: D2D_RECT_F, f: &IDWriteTextFormat, b: &ID2D1SolidColorBrush) {
        let u: Vec<u16> = s.encode_utf16().collect();
        unsafe {
            self.rt.DrawText(
                &u,
                f,
                &r,
                b,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }
}

// ===== 设置页 =====

struct SettingsGeom {
    list_top: f32,
    row_h: f32,
    max_rows: usize,
    /// 高级项的值框（两行排版，左边留给中文名与说明）。
    value_x: f32,
    value_w: f32,
    /// 左侧分类导航与右侧内容区的分界。
    nav_x: f32,
    nav_y: f32,
    nav_w: f32,
    cat_h: f32,
    body_left: f32,
}

/// 设置页里的一行是什么。
#[derive(Clone, Copy)]
enum SItem {
    /// 分组小标题（不可点）：基础 / 高级设置 / 数据操作。
    Header(&'static str),
    Basic(Basic),
    Setting(usize),
    Action(usize),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Basic {
    Autostart,
    Collector,
    Pause,
    Hotkey,
    Snip,
    SnipQuick,
    ClosePins,
    Floating,
    Theme,
    DataDir,
    WeeklyReport,
}

fn basic_label(b: Basic) -> &'static str {
    match b {
        Basic::Autostart => "开机自启",
        Basic::Collector => "采集",
        Basic::Pause => "暂停采集",
        Basic::Hotkey => "全局热键",
        Basic::Snip => "区域截图（进标注）",
        Basic::SnipQuick => "区域截图（松手即存）",
        Basic::ClosePins => "关掉所有贴图",
        Basic::Floating => "悬浮窗",
        Basic::Theme => "主题",
        Basic::DataDir => "数据目录",
        Basic::WeeklyReport => "上周周报",
    }
}

/// 设置页左侧分类（与旧 WebView 的侧栏同一组语义，按原生端现有的项收敛成 6 类）。
const SETTINGS_CATS: [(&str, &str); 6] = [
    ("常规", "启动、采集、主题"),
    ("热键与截图", "呼出窗口、区域截图"),
    ("悬浮窗", "桌面上的小卡片"),
    ("目标", "每日目标次数"),
    ("数据", "目录、备份、导入导出"),
    ("高级", "键鼠、休息、刷新、应用与设备"),
];

fn basic_cat(b: Basic) -> usize {
    match b {
        Basic::Autostart | Basic::Collector | Basic::Pause | Basic::Theme => 0,
        Basic::Hotkey | Basic::Snip | Basic::SnipQuick | Basic::ClosePins => 1,
        Basic::Floating => 2,
        Basic::WeeklyReport => 3,
        Basic::DataDir => 4,
    }
}

/// 设置页里哪几行是截图入口、各开哪一档（真 = 选区后进标注窗，假 = 松手即存）。
/// 旧 Webview 版这两颗都在页上，换原生时只剩标注那颗。点下去的分支读这张表，
/// 为的是"哪一行开哪一档"有用例可判——写在 wndproc 里时，把两颗都改成同一档，
/// 现有腿一条都不会红。
fn snip_mode_of(b: Basic) -> Option<bool> {
    match b {
        Basic::Snip => Some(true),
        Basic::SnipQuick => Some(false),
        _ => None,
    }
}

/// 设置页上"按钮型"的行，按显示顺序。漏在这一张表里的变体不会出现在屏上，
/// 而画它的分支、点它的分支照旧编得过——所以这两张表各由一条用例钉着。
const BASIC_ROWS: [Basic; 11] = [
    Basic::Autostart,
    Basic::Collector,
    Basic::Pause,
    Basic::Theme,
    Basic::Hotkey,
    Basic::Snip,
    Basic::SnipQuick,
    Basic::ClosePins,
    Basic::Floating,
    Basic::DataDir,
    Basic::WeeklyReport,
];

fn setting_cat(section: &str) -> usize {
    match section {
        "hotkey" => 1,
        "goal" => 3,
        "database" => 4,
        _ => 5,
    }
}

/// 快照里某一行设置的当前值。画面上要说"现在用的是哪个组合"时读它，
/// 别再自己抄一遍硬编码——抄一次就已经错过一次。
fn snap_setting(snap: &Snapshot, section: &str, key: &str) -> String {
    snap.settings
        .iter()
        .find(|r| r.section == section && r.key == key)
        .map(|r| r.value.clone())
        .unwrap_or_else(|| "（没有这一行）".to_string())
}

/// 当前分类 / 搜索词下要显示的行。
fn settings_items(snap: &Snapshot, filter: &str, cat: usize) -> Vec<SItem> {
    let f = filter.trim().to_lowercase();
    let searching = !f.is_empty();
    let mut out = Vec::new();
    for b in BASIC_ROWS {
        if !searching && basic_cat(b) != cat {
            continue;
        }
        if f.is_empty() || basic_label(b).to_lowercase().contains(&f) {
            out.push(SItem::Basic(b));
        }
    }
    for (i, row) in snap.settings.iter().enumerate() {
        if !searching && setting_cat(row.section) != cat {
            continue;
        }
        let hay = format!(
            "{} {} {}.{} {}",
            row.label, row.help, row.section, row.key, row.value
        )
        .to_lowercase();
        if f.is_empty() || hay.contains(&f) {
            out.push(SItem::Setting(i));
        }
    }
    let actions: Vec<usize> = (0..ACTIONS.len())
        .filter(|&i| {
            if !searching && cat != 4 {
                return false;
            }
            if f.is_empty() {
                return true;
            }
            let (label, desc) = ACTIONS[i];
            label.to_lowercase().contains(&f) || desc.to_lowercase().contains(&f)
        })
        .collect();
    if !actions.is_empty() {
        if !searching {
            out.push(SItem::Header("数据操作"));
        }
        for i in actions {
            out.push(SItem::Action(i));
        }
    }
    out
}

fn settings_geom(_w: f32, h: f32) -> SettingsGeom {
    let card_top = 208.0;
    let list_top = card_top + 52.0;
    // 高级项两行：中文名 + 一句话说明；基础行/操作也跟着这个行高，整页不再挤成一列键名。
    let row_h = 44.0;
    let max_rows = (((h - 32.0 - 30.0 - list_top) / row_h).floor() as usize).max(1);
    SettingsGeom {
        list_top,
        row_h,
        max_rows,
        value_x: 720.0,
        value_w: 150.0,
        nav_x: 28.0,
        nav_y: list_top,
        nav_w: 190.0,
        cat_h: 44.0,
        body_left: 240.0,
    }
}

/// 设置页底部的数据操作（后台线程跑，结果回投 `WM_OP_DONE`）。
const ACTIONS: [(&str, &str); 8] = [
    ("立即备份", "把年度库各快照一份到 backup/"),
    ("压缩数据库", "重建年度库，回收空闲页"),
    ("清理 30 天前", "删掉 30 天以前的明细（不可撤销）"),
    ("导出 CSV", "把当前周期的数据写成表格"),
    ("导出 HTML", "写成一份可直接打开的网页报告"),
    ("导出 JSON", "写成结构化数据（脚本可读）"),
    ("导入旧数据…", "从旧版数据目录合并进来（先留档再覆盖）"),
    (
        "更改数据文件夹…",
        "把 data/ 与 backup/ 搬到新目录；重启后执行",
    ),
];

/// 设置页右上角那个搜索框的底框（真正的输入是叠在它上面的原生 EDIT，见 `ensure_search_edit`）。
fn search_box_rect(w: f32) -> D2D_RECT_F {
    rect(w - 16.0 - 16.0 - 260.0, 218.0, 260.0, 26.0)
}

#[allow(clippy::too_many_arguments)] // 一屏要画的东西就是这些参数；打包成结构体只会让调用点多抄一遍
fn draw_settings(
    p: &P<'_>,
    w: f32,
    h: f32,
    snap: &Snapshot,
    vs: ViewState,
    status: Option<&str>,
    busy: bool,
    filter: &str,
) {
    let card = rect(16.0, 208.0, w - 32.0, h - 208.0 - 16.0);
    p.round(card, 14.0, &p.br.card, Some(&p.br.border));
    p.text(
        "设置",
        rect(card.left + 16.0, card.top + 10.0, 160.0, 24.0),
        &p.fmt.settings_h,
        &p.br.fg,
    );
    // 这句是标题旁边的说明：顶边比 15/700 的标题低 3 像素，两行才在同一条基线上。
    p.text(
        "左侧分类；改动写进 config.ini，标「重启」的项要重启程序",
        rect(card.left + 66.0, card.top + 13.0, 620.0, 18.0),
        &p.fmt.text12,
        &p.br.muted,
    );

    // 搜索框：底框画在这里，真正的输入是叠在上面的原生 EDIT（见 place_search_edit）。
    let sbox = search_box_rect(w);
    p.round(sbox, 7.0, &p.br.header, Some(&p.br.border));
    if filter.is_empty() {
        p.text(
            "搜索设置（名称或段.键）…",
            rect(
                sbox.left + 10.0,
                sbox.top + 4.0,
                sbox.right - sbox.left - 20.0,
                18.0,
            ),
            &p.fmt.text13,
            &p.br.muted,
        );
    }

    let g = settings_geom(w, h);
    let items = settings_items(snap, filter, vs.settings_cat);
    let count = items.len();
    let scroll = vs.scroll.min(count.saturating_sub(g.max_rows));

    // 左侧分类导航。
    for (i, (label, desc)) in SETTINGS_CATS.iter().enumerate() {
        let nr = rect(
            g.nav_x,
            g.nav_y + i as f32 * g.cat_h,
            g.nav_w,
            g.cat_h - 4.0,
        );
        let active = i == vs.settings_cat;
        if active {
            p.round(nr, 8.0, &p.br.accent_soft, None);
        }
        p.text(
            label,
            rect(
                nr.left + 12.0,
                nr.top + 4.0,
                nr.right - nr.left - 20.0,
                18.0,
            ),
            // 选中的分类加一档字重（旧版 `.ff-nav button.on { font-weight: 600 }`），
            // 未选中的与正文同档 13/400 —— 以前两者都是 12.5/400，只能靠底色区分。
            if active {
                &p.fmt.text13_em
            } else {
                &p.fmt.text13
            },
            if active { &p.br.accent } else { &p.br.fg },
        );
        p.text(
            desc,
            rect(
                nr.left + 12.0,
                nr.top + 23.0,
                nr.right - nr.left - 20.0,
                15.0,
            ),
            &p.fmt.small,
            &p.br.muted,
        );
    }
    p.line(
        g.body_left - 14.0,
        g.list_top,
        g.body_left - 14.0,
        card.bottom - 40.0,
        &p.br.grid,
        1.0,
    );

    for vis in 0..g.max_rows {
        let idx = scroll + vis;
        if idx >= count {
            break;
        }
        let y = g.list_top + vis as f32 * g.row_h;
        let r = rect(
            g.body_left,
            y,
            card.right - 16.0 - g.body_left,
            g.row_h - 2.0,
        );
        if vs.sel == Some(idx) {
            p.round(r, 6.0, &p.br.accent_soft, None);
        } else if vs.hover == Some(idx) {
            p.fill(r, &p.br.header);
        }
        match items[idx] {
            SItem::Setting(i) => {
                let row = &snap.settings[i];
                // 第一行中文名，第二行一句话说明（说明以「分组 ·」开头）。
                p.text(
                    row.label,
                    rect(r.left + 12.0, y + 2.0, 460.0, 20.0),
                    &p.fmt.row_name,
                    &p.br.fg,
                );
                p.text(
                    row.help,
                    rect(
                        r.left + 12.0,
                        y + 23.0,
                        (g.value_x - r.left - 30.0).max(120.0),
                        17.0,
                    ),
                    &p.fmt.text12,
                    &p.br.muted,
                );

                let vb = rect(g.value_x, y + 9.0, g.value_w, 26.0);
                if row.kind == 0 {
                    let on = row.value == "true";
                    p.round(vb, 6.0, if on { &p.br.success } else { &p.br.grid }, None);
                    p.text(
                        if on { "开" } else { "关" },
                        vb,
                        &p.fmt.center13,
                        if on { &p.br.white } else { &p.br.muted },
                    );
                } else {
                    let editing = vs.editing == Some(idx);
                    let capturing = vs.capturing == Some(i);
                    p.round(
                        vb,
                        6.0,
                        &p.br.card,
                        Some(if editing || capturing {
                            &p.br.accent
                        } else {
                            &p.br.border
                        }),
                    );
                    // 热键那三行在值框右沿挂一颗小钮：有值时是「✕」（清空＝这条不注册），
                    // 空着时是「默认」（把默认组合写回去）。一颗钮两种样子，是因为这两个
                    // 动作永远不会同时需要。
                    let btn = hotkey_slot(row.section, row.key).map(|_| hotkey_btn_rect(vb));
                    let off = row.value.trim().is_empty();
                    if !editing {
                        let shown: &str = if capturing {
                            "请按下组合…"
                        } else if off {
                            "（未启用）"
                        } else {
                            &row.value
                        };
                        p.text(
                            shown,
                            rect(
                                vb.left + 8.0,
                                vb.top,
                                vb.right - vb.left - if btn.is_some() { 54.0 } else { 16.0 },
                                vb.bottom - vb.top,
                            ),
                            &p.fmt.text13,
                            if capturing {
                                &p.br.accent
                            } else if off {
                                &p.br.muted
                            } else {
                                &p.br.fg
                            },
                        );
                    }
                    if let Some(b) = btn {
                        p.round(
                            b,
                            4.0,
                            if off { &p.br.accent } else { &p.br.header },
                            Some(&p.br.border),
                        );
                        p.text(
                            if off { "默认" } else { "✕" },
                            b,
                            &p.fmt.center13,
                            if off { &p.br.white } else { &p.br.fg },
                        );
                    }
                }
                // 热键那行还要说清"配了但没挂上"：值框里只有他配的那一串，看不出被谁占了。
                let hk_note = hotkey_slot(row.section, row.key)
                    .filter(|slot| !row.value.trim().is_empty() && !vs.hotkey_live[*slot])
                    .map(|_| "这条没注册上");
                if let Some(note) = hk_note {
                    p.text(
                        note,
                        rect(vb.right + 14.0, y + 13.0, 220.0, 16.0),
                        &p.fmt.small,
                        &p.br.danger,
                    );
                } else if row.live != "改完立即生效" {
                    // 只有"要重启/下次隐藏"这种例外才写在行上；绝大多数立即生效，不再每行重复。
                    p.text(
                        row.live,
                        rect(vb.right + 14.0, y + 13.0, 220.0, 16.0),
                        &p.fmt.small,
                        &p.br.muted,
                    );
                }
            }
            SItem::Header(title) => {
                // 分组小标题：以前借 11/400 的灰字，跟它下面那些行的说明几乎分不出来，
                // 扫一眼看不出这里是分组的起点。
                p.text(
                    title,
                    rect(r.left + 12.0, y + 12.0, 360.0, 18.0),
                    &p.fmt.text12_em,
                    &p.br.muted,
                );
                p.line(
                    r.left + 12.0,
                    y + g.row_h - 1.0,
                    r.right - 12.0,
                    y + g.row_h - 1.0,
                    &p.br.grid,
                    1.0,
                );
            }
            SItem::Basic(b) => {
                p.text(
                    basic_label(b),
                    rect(r.left + 12.0, y + 2.0, 240.0, 20.0),
                    &p.fmt.row_name,
                    &p.br.fg,
                );
                let vb = rect(g.value_x, y + 9.0, 80.0, 26.0);
                match b {
                    Basic::Autostart => {
                        let on = snap.autostart;
                        p.round(vb, 6.0, if on { &p.br.success } else { &p.br.grid }, None);
                        p.text(
                            if on { "开" } else { "关" },
                            vb,
                            &p.fmt.center13,
                            if on { &p.br.white } else { &p.br.muted },
                        );
                        p.text(
                            "改完立即生效（现读启动文件夹）",
                            rect(r.left + 12.0, y + 23.0, 300.0, 17.0),
                            &p.fmt.text12,
                            &p.br.muted,
                        );
                    }
                    // 采集：默认关；打开就是全局键鼠监听（设置页那一行是唯一的入口）。
                    Basic::Collector => {
                        let on = vs.collector_on;
                        let legacy = on && collect::legacy_running();
                        p.round(vb, 6.0, if on { &p.br.success } else { &p.br.grid }, None);
                        p.text(
                            if on { "已启用" } else { "未启用" },
                            vb,
                            &p.fmt.center13,
                            if on { &p.br.white } else { &p.br.muted },
                        );
                        p.text(
                            if legacy {
                                // 双计是"数字会悄悄翻倍"的那种错，必须钉在这一行上；
                                // 底部状态行会被别的操作覆盖掉。
                                "检测到旧版 FocusFlow.exe 也在采集：两处相加会让数字翻倍，只留一个"
                            } else if on {
                                "全局键鼠监听已开；停用后要重启才彻底退出（[collector] enabled）"
                            } else {
                                "已关闭；点一下开始采集（全局键鼠，只存本机）"
                            },
                            rect(r.left + 12.0, y + 23.0, 520.0, 17.0),
                            &p.fmt.text12,
                            if legacy { &p.br.danger } else { &p.br.muted },
                        );
                    }
                    Basic::Pause => {
                        let (label, bg, fg) = if !vs.collector_on {
                            ("—", &p.br.grid, &p.br.muted)
                        } else if vs.collector_paused {
                            ("已暂停", &p.br.accent, &p.br.white)
                        } else {
                            ("采集中", &p.br.success, &p.br.white)
                        };
                        p.round(vb, 6.0, bg, None);
                        p.text(label, vb, &p.fmt.center13, fg);
                        p.text(
                            if !vs.collector_on {
                                "采集没启用，暂停无从谈起"
                            } else if vs.collector_paused {
                                "已暂停；点一下继续（键鼠与设备计数一起恢复）"
                            } else {
                                "点一下暂停：键鼠与设备计数一起停，不写配置、重启即恢复"
                            },
                            rect(r.left + 12.0, y + 23.0, 520.0, 17.0),
                            &p.fmt.text12,
                            &p.br.muted,
                        );
                    }
                    Basic::Hotkey => {
                        let on = vs.hotkey_on;
                        p.round(vb, 6.0, if on { &p.br.success } else { &p.br.grid }, None);
                        p.text(
                            if on { "开" } else { "关" },
                            vb,
                            &p.fmt.center13,
                            if on { &p.br.white } else { &p.br.muted },
                        );
                        let toggle = snap_setting(snap, "hotkey", "toggle_window");
                        let snip = snap_setting(snap, "hotkey", "snip");
                        let annotate = snap_setting(snap, "hotkey", "snip_annotate");
                        p.text(
                            &format!(
                                "总开关：开 = 注册下面三行（{toggle} / {snip} / {annotate}）；关 = 一条都不占用"
                            ),
                            rect(r.left + 12.0, y + 23.0, 560.0, 17.0),
                            &p.fmt.text12,
                            &p.br.muted,
                        );
                    }
                    Basic::Snip => {
                        p.round(vb, 6.0, &p.br.accent, None);
                        p.text("进标注", vb, &p.fmt.center13, &p.br.white);
                        let annotate = snap_setting(snap, "hotkey", "snip_annotate");
                        p.text(
                            &format!("这颗按钮立刻开标注场，等价于按热键 {annotate}；框好之后 Enter 或 Ctrl+S 出图"),
                            rect(r.left + 12.0, y + 23.0, 640.0, 17.0),
                            &p.fmt.text12,
                            &p.br.muted,
                        );
                    }
                    Basic::SnipQuick => {
                        p.round(vb, 6.0, &p.br.accent, None);
                        p.text("松手即存", vb, &p.fmt.center13, &p.br.white);
                        let snip = snap_setting(snap, "hotkey", "snip");
                        p.text(
                            &format!(
                                "这颗按钮不进标注，等价于按热键 {snip}；PNG 存 data/screenshots 并复制到剪贴板"
                            ),
                            rect(r.left + 12.0, y + 23.0, 640.0, 17.0),
                            &p.fmt.text12,
                            &p.br.muted,
                        );
                    }
                    Basic::ClosePins => {
                        let n = vs.pin_count;
                        p.round(
                            vb,
                            6.0,
                            if n == 0 { &p.br.grid } else { &p.br.accent },
                            None,
                        );
                        p.text(
                            if n == 0 { "没有贴图" } else { "关掉" },
                            vb,
                            &p.fmt.center13,
                            if n == 0 { &p.br.muted } else { &p.br.white },
                        );
                        p.text(
                            &format!(
                                "现在开着 {n} 张；按住拖动、滚轮缩放、Esc 或右键关掉单张。贴图只在内存里，截图文件仍留在 data/screenshots"
                            ),
                            rect(r.left + 12.0, y + 23.0, 640.0, 17.0),
                            &p.fmt.text12,
                            &p.br.muted,
                        );
                    }
                    Basic::Floating => {
                        let on = vs.floating_on;
                        p.round(vb, 6.0, if on { &p.br.success } else { &p.br.grid }, None);
                        p.text(
                            if on { "显示" } else { "隐藏" },
                            vb,
                            &p.fmt.center13,
                            if on { &p.br.white } else { &p.br.muted },
                        );
                        p.text(
                            "拖到哪记哪；双击卡片叫回主窗口（[floating] 段）",
                            rect(r.left + 12.0, y + 23.0, 420.0, 17.0),
                            &p.fmt.text12,
                            &p.br.muted,
                        );
                    }
                    Basic::Theme => {
                        let dark = vs.dark;
                        p.round(
                            vb,
                            6.0,
                            if dark { &p.br.header } else { &p.br.accent },
                            Some(&p.br.border),
                        );
                        p.text(
                            if dark { "深色" } else { "浅色" },
                            vb,
                            &p.fmt.center13,
                            &p.br.fg,
                        );
                        p.text(
                            "点一下切换；配色照 tokens.css 的两套令牌（[gui] theme）",
                            rect(r.left + 12.0, y + 23.0, 420.0, 17.0),
                            &p.fmt.text12,
                            &p.br.muted,
                        );
                    }
                    Basic::WeeklyReport => {
                        p.round(vb, 6.0, &p.br.accent, None);
                        p.text("生成", vb, &p.fmt.center13, &p.br.white);
                        p.text(
                            "汇总上周一~周日，写到 data/reports/weekly-YYYY-MM-DD.md（只存本机）；启动时缺这一份会自动补",
                            rect(r.left + 12.0, y + 23.0, 600.0, 17.0),
                            &p.fmt.text12,
                            &p.br.muted,
                        );
                    }
                    Basic::DataDir => {
                        p.round(vb, 6.0, &p.br.header, Some(&p.br.border));
                        p.text("打开", vb, &p.fmt.center13, &p.br.fg);
                        let dir_line = if snap.pending_move.is_empty() {
                            format!(
                                "{}（改目录要用下面的「更改数据文件夹」，重启后生效）",
                                snap.data_home
                            )
                        } else {
                            // 安排过但还没重启：这句话就是"为什么现在还在旧目录"的读数。
                            format!(
                                "现在：{}｜已安排搬到：{}（重启后执行，重启前请留着旧目录）",
                                snap.data_home, snap.pending_move
                            )
                        };
                        p.text(
                            &dir_line,
                            rect(r.left + 12.0, y + 23.0, 560.0, 17.0),
                            &p.fmt.text12,
                            &p.br.muted,
                        );
                    }
                }
            }
            SItem::Action(a) => {
                // 数据操作行：左边是名字，右边一颗「执行」按钮，再右边一句后果说明。
                let (label, desc) = ACTIONS[a];
                p.text(
                    label,
                    rect(r.left + 12.0, y + 2.0, 240.0, 20.0),
                    &p.fmt.row_name,
                    &p.br.fg,
                );
                let vb = rect(g.value_x, y + 9.0, 80.0, 26.0);
                let enabled = !busy;
                p.round(
                    vb,
                    6.0,
                    if enabled { &p.br.accent } else { &p.br.grid },
                    None,
                );
                p.text(
                    if busy { "…" } else { "执行" },
                    vb,
                    &p.fmt.center13,
                    if enabled { &p.br.white } else { &p.br.muted },
                );
                p.text(
                    desc,
                    rect(r.left + 12.0, y + 23.0, 360.0, 17.0),
                    &p.fmt.text12,
                    &p.br.muted,
                );
            }
        }
    }

    // 状态行
    if let Some(line) = status {
        // 哪些行算"要人处理"只写在 `status_is_warning` 一处，这里不抄第二份判据
        let warn = status_is_warning(line);
        p.text(
            line,
            rect(
                card.left + 16.0,
                card.bottom - 26.0,
                card.right - card.left - 32.0,
                18.0,
            ),
            // "要人处理"那一句除了换颜色，还加一档字重：底部这一行平时是背景信息，
            // 红字加粗才是"现在就得看一眼"。
            if warn {
                &p.fmt.text12_em
            } else {
                &p.fmt.text12
            },
            if warn { &p.br.danger } else { &p.br.muted },
        );
    }

    if count > g.max_rows {
        let track = rect(
            card.right - 9.0,
            g.list_top,
            4.0,
            g.max_rows as f32 * g.row_h,
        );
        p.round(track, 2.0, &p.br.header, None);
        let frac = g.max_rows as f32 / count as f32;
        let thumb_h = ((track.bottom - track.top) * frac).max(16.0);
        let max_scroll = (count - g.max_rows) as f32;
        let t = if max_scroll > 0.0 {
            scroll as f32 / max_scroll
        } else {
            0.0
        };
        let y = track.top + ((track.bottom - track.top) - thumb_h) * t;
        p.round(rect(track.left, y, 4.0, thumb_h), 2.0, &p.br.accent, None);
    }
}

// ===== 设备详情弹层 =====

/// 对话框与关闭按钮的位置：画图与命中测试共用。
fn detail_rects(w: f32, h: f32) -> (D2D_RECT_F, D2D_RECT_F) {
    let dw = 540.0_f32.min(w - 40.0);
    let dh = 420.0_f32.min(h - 40.0);
    let dialog = rect((w - dw) / 2.0, (h - dh) / 2.0, dw, dh);
    let close = rect(dialog.right - 42.0, dialog.top + 12.0, 26.0, 26.0);
    (dialog, close)
}

fn period_text(period: i64) -> String {
    match period {
        -1 => "今日".into(),
        0 => "全部".into(),
        n => format!("近 {n} 天"),
    }
}

/// 三份导出共用的那一句"这一轮没读全"：文件里没这句，拿到导出的人就把下界当成了实测上限。
/// 这里**念全句**（哪一年、哪一栏、为什么），屏上那几格只念到年份 —— 文件没有宽度上限，
/// 而屏幕上是一行不换行的字。同一个判据（`snap.gaps` 是否非空）在两处说两种长度。
fn gap_line(gaps: &[String]) -> String {
    if gaps.is_empty() {
        return String::new();
    }
    format!(
        "注意：{} 条读数没读全，这些数字是下界（{}）",
        gaps.len(),
        gaps.join("；")
    )
}

/// 屏上那一格最多念几个缺读的年份，多出来的折成"等 N 年"。
const GAP_NOTE_YEARS: usize = 3;

/// 这一栏缺读的那几个年份，只取屏上念得下的那一小截。
///
/// core 交回的每条长这样 ——「2025 年库每日趋势读不出来: {原因}」（那个形状是它自己的文档
/// 写下的）。冒号后面那截原因不上屏：屏上这几格都是一行，而每一档字体都是 `NO_WRAP`、
/// `DrawText` 又不按传入的矩形裁切（试过给它设 `DWRITE_TRIMMING`，实测仍然照画过去），
/// 于是照抄原因的那句会一路画到隔壁卡上、甚至画出窗口外。原因本身在 error 日志里有。
/// 年份最多念三个，多出来的折成"等 N 年" —— 这句话因此有个上界。
fn gap_years(gaps: &[String], domain: &str) -> String {
    let mine: Vec<&str> = gaps
        .iter()
        .map(String::as_str)
        .filter(|g| g.contains(domain))
        .collect();
    if mine.is_empty() {
        return String::new();
    }
    let named: Vec<String> = mine
        .iter()
        .take(GAP_NOTE_YEARS)
        .map(|g| {
            let head = g.split(" 年库").next().unwrap_or(g);
            if head.is_empty() || !head.chars().all(|c| c.is_ascii_digit()) {
                // 形状对不上（core 改了那句话而没改这里）：念它的前半截，别把这一条吞掉。
                return head.to_string();
            }
            format!("{head} 年")
        })
        .collect();
    let mut s = named.join("、");
    if mine.len() > GAP_NOTE_YEARS {
        s.push_str(&format!(" 等 {} 年", mine.len()));
    }
    s
}

/// 念不全的那一句，供三张图自己那格用：年份由 [`gap_years`] 挑，域名不写第二遍
/// （那一格本身就是那一栏）。
fn gap_note(gaps: &[String], domain: &str) -> String {
    let years = gap_years(gaps, domain);
    if years.is_empty() {
        return String::new();
    }
    format!(" · {years}没读全")
}

/// 左表那一屏的页脚。三件事按"越要紧的越靠前"排：这一栏缺哪几年 → 整屏数字只是下界
/// → 数据源路径。那一格只有一行、装不下时被截掉的是句尾，所以路径必须排在最后 ——
/// 它设置在页与日志里都还有，而前面那句警告没有第二个地方会念。
fn footer_line(data_home: &str, gaps: &[String], lane: &str) -> String {
    if gaps.is_empty() {
        return format!("数据源：{data_home}");
    }
    let lane_years = gap_years(gaps, lane);
    let mine = if lane_years.is_empty() {
        String::new()
    } else {
        format!("本栏{lane_years}没读全 · ")
    };
    format!(
        "{mine}{} 条读数没读全（上面那些数字是下界） · 数据源：{data_home}",
        gaps.len()
    )
}

fn draw_detail(p: &P<'_>, w: f32, h: f32, d: &DeviceDetail) {
    // 遮罩：整屏压暗，点击它等于关闭。
    p.fill(rect(0.0, 0.0, w, h), &p.br.overlay);
    let (dialog, close) = detail_rects(w, h);
    p.round(dialog, 16.0, &p.br.modal, Some(&p.br.border));

    let x = dialog.left + 20.0;
    p.text(
        "设备详情",
        rect(x, dialog.top + 14.0, 160.0, 22.0),
        &p.fmt.body,
        &p.br.fg,
    );
    p.round(close, 6.0, &p.br.header, None);
    p.text("✕", close, &p.fmt.center_small, &p.br.muted);

    p.text(
        &d.name,
        rect(x, dialog.top + 40.0, dialog.right - x - 20.0, 20.0),
        &p.fmt.body,
        &p.br.fg,
    );

    let kind = match d.kind.as_str() {
        "mouse" => "鼠标",
        "keyboard" => "键盘",
        "hybrid" => "键鼠",
        _ => "未知",
    };
    let sub = if d.has_alias {
        format!("{kind} · 原名 {}", d.auto_name)
    } else {
        kind.to_string()
    };
    p.text(
        &sub,
        rect(x, dialog.top + 62.0, dialog.right - x - 20.0, 18.0),
        &p.fmt.small,
        &p.br.muted,
    );

    let col2 = x + (dialog.right - dialog.left) * 0.52;
    let row = |label: &str, value: &str, cx: f32, yy: f32| {
        p.text(label, rect(cx, yy, 76.0, 18.0), &p.fmt.small, &p.br.muted);
        p.text(
            value,
            rect(cx + 82.0, yy, dialog.right - cx - 100.0, 18.0),
            &p.fmt.body,
            &p.br.fg,
        );
    };

    let y0 = dialog.top + 96.0;
    let step = 26.0;
    let share = if d.period_total > 0 {
        d.period_count as f32 * 100.0 / d.period_total as f32
    } else {
        0.0
    };
    row("今日", &thousands(d.today.max(0) as u64), x, y0);
    row("近 7 天", &thousands(d.week.max(0) as u64), x, y0 + step);
    row(
        "近 30 天",
        &thousands(d.month.max(0) as u64),
        x,
        y0 + step * 2.0,
    );
    row("全部", &thousands(d.all.max(0) as u64), x, y0 + step * 3.0);

    row(
        &period_text(d.period),
        &thousands(d.period_count.max(0) as u64),
        col2,
        y0,
    );
    row("占比", &format!("{share:.1}%"), col2, y0 + step);
    row(
        "排名",
        &if d.rank > 0 {
            format!("{} / {}", d.rank, d.device_count)
        } else {
            "—".to_string()
        },
        col2,
        y0 + step * 2.0,
    );
    row(
        "同类排名",
        &if d.kind_rank > 0 {
            format!("{} / {}", d.kind_rank, d.kind_count)
        } else {
            "—".to_string()
        },
        col2,
        y0 + step * 3.0,
    );

    row(
        "活跃天数",
        &format!("{} 天", d.active_days),
        x,
        y0 + step * 4.0,
    );
    row(
        "活跃日均",
        &format!("{:.0}", d.avg_per_active_day),
        col2,
        y0 + step * 4.0,
    );
    row(
        "首次使用",
        if d.first_date.is_empty() {
            "—"
        } else {
            &d.first_date
        },
        x,
        y0 + step * 5.0,
    );
    row(
        "最近使用",
        if d.last_date.is_empty() {
            "—"
        } else {
            &d.last_date
        },
        col2,
        y0 + step * 5.0,
    );

    // 。
    let ty = dialog.bottom - 74.0;
    p.text(
        "近 30 天分布",
        rect(x, ty - 20.0, 160.0, 16.0),
        &p.fmt.small,
        &p.br.muted,
    );
    let plot = rect(x, ty, dialog.right - dialog.left - 40.0, 54.0);
    let max = d.trend.iter().map(|(_, v)| *v).max().unwrap_or(0).max(1) as f32;
    let n = d.trend.len().max(1);
    let bw = ((plot_width(plot) - (n as f32 - 1.0) * 2.0) / n as f32).max(1.0);
    for (i, (_, v)) in d.trend.iter().enumerate() {
        let hh = (*v as f32 / max) * plot_height(plot);
        let bx = plot.left + i as f32 * (bw + 2.0);
        if let Some(b) = p.brush(p.pal.accent) {
            p.round(rect(bx, plot.bottom - hh, bw, hh), 1.0, &b, None);
        }
    }

    if !d.unreadable.is_empty() {
        p.text(
            &format!("有 {} 项没读上来（数字是下界）", d.unreadable.len()),
            rect(
                dialog.left + 120.0,
                dialog.top + 16.0,
                dialog.right - dialog.left - 186.0,
                16.0,
            ),
            &p.fmt.right,
            &p.br.muted,
        );
    }
}

// ===== 一屏内容 =====

/// 一屏里会变的交互状态。画图和命中测试必须读同一份。
#[derive(Clone, Copy, Default)]
struct ViewState {
    view: usize,
    atab: usize,
    rank_tab: usize,
    rank_filter: usize,
    scroll: usize,
    period: i64,
    hover: Option<usize>,
    sel: Option<usize>,
    editing: Option<usize>,
    /// 按下录入开着的那条热键（`settings` 下标）；那一行的值框要改成"请按下组合…"。
    capturing: Option<usize>,
    heat_hover: Option<usize>,
    chart_hover: Option<usize>,
    /// 全局热键此刻是否真的注册着（不是配置键，是运行时状态）。
    hotkey_on: bool,
    /// 三条绑定各自注册上了没有，顺序同 `COMBO_KEYS`（0 唤出 / 1 松手即存 / 2 进标注）。
    hotkey_live: [bool; 3],
    /// 悬浮窗此刻是否真的显示着（同上：读运行时状态，不读配置键 ——
    /// 手改 `[floating] enabled` 并不会让卡片出现或消失）。
    floating_on: bool,
    /// 桌面上现在开着几张贴图（设置页那一行显示的就是这个数）。
    pin_count: usize,
    /// 当前主题是不是深色（设置页那一行显示"深色/浅色"）。
    dark: bool,
    /// 采集真的起来了没有 / 是不是暂停着 / 最近 60 秒的次数。
    collector_on: bool,
    collector_paused: bool,
    cpm: i64,
    /// 设置页左侧选中的分类。
    settings_cat: usize,
}

/// 左表格卡片的矩形：画图与命中测试共用。
fn left_card(w: f32, h: f32) -> D2D_RECT_F {
    let m = 16.0;
    let top = 208.0;
    let body_h = h - top - m;
    let left_w = (w - m * 2.0 - 12.0) * 0.6;
    rect(m, top, left_w, body_h)
}

/// 周期药丸的位置：画图与命中测试共用。
fn period_pills() -> [(i64, D2D_RECT_F); 6] {
    let mut out = [(0i64, rect(0.0, 0.0, 0.0, 0.0)); 6];
    for (i, (p, _)) in PERIODS.iter().enumerate() {
        out[i] = (*p, rect(16.0 + 66.0 + i as f32 * 60.0, 48.0, 54.0, 26.0));
    }
    out
}

#[allow(clippy::too_many_arguments)] // 主区各视图共用这一个入口：拆开传会在每个调用点重排一遍同样的参数
fn draw_screen(
    p: &P<'_>,
    w: f32,
    h: f32,
    snap: &Snapshot,
    vs: ViewState,
    detail: Option<&DeviceDetail>,
    status: Option<&str>,
    busy: bool,
    filter: &str,
    plugins: &[PluginRow],
    plugin_err: Option<&str>,
    plugin_view: Option<(&str, &focusflow_core::plugins::PluginView)>,
    pv_scroll: i32,
    pv_select: Option<&PvSelect>,
    pv_modal: Option<&str>,
    // 插件表格各分组当前选中的记录 id（选中行高亮与 `sel` 按钮都读它）。
    plugin_sel: &HashMap<String, Vec<String>>,
) {
    let m = 16.0;

    // --- 顶栏 ---
    p.text("FocusFlow", rect(m, 8.0, 160.0, 30.0), &p.fmt.h1, &p.br.fg);
    p.text(
        "效率追踪器",
        rect(m + 112.0, 18.0, 120.0, 20.0),
        &p.fmt.small,
        &p.br.muted,
    );
    p.text(
        "v0.5.0 · Direct2D",
        rect(w - m - 220.0, 18.0, 220.0, 20.0),
        &p.fmt.right,
        &p.br.muted,
    );

    // --- 周期页签 ---
    p.text(
        "统计周期",
        rect(m, 50.0, 60.0, 24.0),
        &p.fmt.small,
        &p.br.muted,
    );
    for (i, (period, label)) in PERIODS.iter().enumerate() {
        let active = vs.period == *period;
        let r = period_pills()[i].1;
        if active {
            p.round(r, 7.0, &p.br.accent, None);
        } else {
            p.round(r, 7.0, &p.br.card, Some(&p.br.border));
        }
        p.text(
            label,
            rect(r.left, r.top, r.right - r.left, r.bottom - r.top),
            &p.fmt.tab,
            if active { &p.br.white } else { &p.br.muted },
        );
    }

    // --- 五张统计卡（真数据）---
    let max_day = snap.max_day.clone().unwrap_or_else(|| ("—".to_string(), 0));
    // 「当前速度」有三种状态：没开采集（只读）、开着但暂停、正在采集。
    // 没数据时不写内部代号，也不留一个看起来只是暂时没数的空位。
    let (speed_value, speed_sub) = if !vs.collector_on {
        ("—".to_string(), "本端只读 · 未接采集".to_string())
    } else if vs.collector_paused {
        ("—".to_string(), "采集已暂停".to_string())
    } else {
        (
            thousands(vs.cpm.max(0) as u64),
            "次/分 · 近 60 秒".to_string(),
        )
    };
    let cards: Vec<(String, String, String)> = vec![
        (
            "今日活跃".into(),
            thousands(snap.today as u64),
            format!("共 {} 个键位", snap.rank.len()),
        ),
        ("当前速度".into(), speed_value, speed_sub),
        (
            "总计".into(),
            thousands(snap.period_total.max(0) as u64),
            if snap.period == 0 {
                "全部历史".into()
            } else {
                "当前周期".into()
            },
        ),
        (
            "日均".into(),
            thousands(snap.avg.max(0) as u64),
            "按周期窗口".into(),
        ),
        (
            "最高单日".into(),
            thousands(max_day.1 as u64),
            max_day.0.clone(),
        ),
    ];
    let gap = 10.0;
    let cw = (w - m * 2.0 - gap * 4.0) / 5.0;
    for (i, (label, value, sub)) in cards.iter().enumerate() {
        let cx = m + i as f32 * (cw + gap);
        let r = rect(cx, 84.0, cw, 78.0);
        p.round(r, 12.0, &p.br.card, Some(&p.br.border));
        p.text(
            label,
            rect(cx + 12.0, 94.0, cw - 24.0, 16.0),
            &p.fmt.label,
            &p.br.muted,
        );
        p.text(
            value,
            rect(cx + 12.0, 110.0, cw - 24.0, 30.0),
            &p.fmt.value,
            &p.br.fg,
        );
        if !sub.is_empty() {
            p.text(
                sub,
                rect(cx + 12.0, 142.0, cw - 24.0, 16.0),
                &p.fmt.small,
                &p.br.muted,
            );
        }
    }

    // --- 视图页签 ---
    for (i, label) in VIEWS.iter().enumerate() {
        let r = view_tabs()[i];
        let active = vs.view == i;
        p.text(
            label,
            rect(r.left, r.top, r.right - r.left, r.bottom - r.top),
            &p.fmt.body,
            if active { &p.br.fg } else { &p.br.muted },
        );
        if active {
            p.line(
                r.left,
                r.bottom + 2.0,
                r.left + 52.0,
                r.bottom + 2.0,
                &p.br.accent,
                2.0,
            );
        }
    }

    match vs.view {
        0..=2 => draw_dashboard(p, w, h, snap, vs),
        3 => draw_analytics(p, w, h, snap, vs),
        4 => match plugin_view {
            Some((name, view)) => {
                draw_plugin_view(p, w, h, name, view, pv_scroll, plugin_sel);
            }
            None => draw_plugins(p, w, h, plugins, plugin_err, vs),
        },
        5 => draw_settings(p, w, h, snap, vs, status, busy, filter),
        v => draw_placeholder(p, w, h, v),
    }

    // 插件弹窗/下拉压在正文上面：弹窗先画，下拉再压在弹窗上面（模态里的 select 要用）。
    if let Some((name, view)) = plugin_view {
        if let Some(m) = find_modal(view, pv_modal) {
            draw_plugin_modal(p, w, h, &m);
        }
        if let Some(sel) = pv_select.filter(|s| s.plugin == name) {
            draw_select_menu(p, w, h, sel);
        }
    }

    // 模态最后画：它压在所有东西上面。
    if let Some(d) = detail {
        draw_detail(p, w, h, d);
    }
}

const VIEWS: [&str; 6] = [
    "键鼠排行",
    "应用排行",
    "设备排行",
    "活跃分析",
    "插件管理",
    "设置",
];

/// 顶部视图页签的位置：画图与命中测试共用。
fn view_tabs() -> [D2D_RECT_F; 6] {
    let mut out = [rect(0.0, 0.0, 0.0, 0.0); 6];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = rect(16.0 + i as f32 * 84.0, 172.0, 80.0, 22.0);
    }
    out
}

/// 活跃分析页的子页签（趋势 / 小时 / 星期 / 热力）。
const ATABS: [&str; 4] = ["趋势", "小时", "星期", "热力"];

fn atab_tabs() -> [D2D_RECT_F; 4] {
    let mut out = [rect(0.0, 0.0, 0.0, 0.0); 4];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = rect(16.0 + i as f32 * 68.0, 258.0, 64.0, 24.0);
    }
    out
}

/// 排行页：左表。
fn draw_dashboard(p: &P<'_>, w: f32, h: f32, snap: &Snapshot, vs: ViewState) {
    let m = 16.0;
    let left = left_card(w, h);
    let right_x = left.right + 12.0;
    let right_w = w - m - right_x;
    let body_h = left.bottom - left.top;

    draw_table(p, left, snap, vs);

    let chart_h = (body_h - 12.0) * 0.55;
    // 排行页右侧那两张小图不接悬停：这一屏的"光标下是什么"归左边那张表（分析页才读
    // `vs.chart_hover`），两处同时亮会给不出"我压的是哪一行"。
    draw_line_chart(p, rect(right_x, left.top, right_w, chart_h), snap, None);
    draw_hourly(
        p,
        rect(
            right_x,
            left.top + chart_h + 12.0,
            right_w,
            body_h - chart_h - 12.0,
        ),
        snap,
        None,
    );
}

/// 活跃分析页的内容区（子页签下面那一大块）。
fn analytics_content(w: f32, h: f32) -> D2D_RECT_F {
    rect(16.0, 292.0, w - 32.0, h - 292.0 - 16.0)
}

/// 每日目标条的位置（分析页顶部）。
fn goal_strip_rect(w: f32) -> D2D_RECT_F {
    rect(16.0, 204.0, w - 32.0, 46.0)
}

/// 每日目标条：今日进度 / 连续打卡 / 。
fn draw_goal_strip(p: &P<'_>, r: D2D_RECT_F, snap: &Snapshot) {
    p.round(r, 12.0, &p.br.card, Some(&p.br.border));
    let pad = 14.0;
    let Some(g) = &snap.goal else {
        p.text(
            "每日目标读不出来（这一轮统计没答上来）",
            rect(
                r.left + pad,
                r.top + 14.0,
                r.right - r.left - pad * 2.0,
                18.0,
            ),
            &p.fmt.small,
            &p.br.muted,
        );
        return;
    };
    let goal = g.goal.max(1);
    let today = snap.today.max(0);
    let met = today >= goal;

    p.text(
        &format!(
            "今日 {} / {}",
            thousands(today as u64),
            thousands(goal as u64)
        ),
        rect(r.left + pad, r.top + 14.0, 150.0, 18.0),
        &p.fmt.body,
        &p.br.fg,
    );

    let dots_w = 7.0 * 14.0 + 6.0 * 3.0;
    let dots_x = r.right - pad - dots_w;
    let bar_x = r.left + pad + 158.0;
    // 右边留给「连续打。
    let bar = rect(bar_x, r.top + 19.0, (dots_x - bar_x - 214.0).max(40.0), 8.0);
    p.round(bar, 4.0, &p.br.grid, None);
    let pct = (today as f32 / goal as f32).clamp(0.0, 1.0);
    if pct > 0.0 {
        p.round(
            rect(bar.left, bar.top, (bar.right - bar.left) * pct, 8.0),
            4.0,
            if met { &p.br.success } else { &p.br.accent },
            None,
        );
    }

    p.text(
        &format!("连续打卡 {} 天 · 近一年最长 {} 天", g.streak, g.best),
        rect(dots_x - 206.0, r.top + 14.0, 198.0, 18.0),
        &p.fmt.right,
        &p.br.muted,
    );

    for (i, (_, _, dmet)) in g.days.iter().enumerate().take(7) {
        let dot = rect(dots_x + i as f32 * 17.0, r.top + 16.0, 14.0, 14.0);
        p.round(
            dot,
            3.0,
            if *dmet { &p.br.success } else { &p.br.grid },
            None,
        );
    }
}

/// 活跃分析页：四个子页。
fn draw_analytics(p: &P<'_>, w: f32, h: f32, snap: &Snapshot, vs: ViewState) {
    draw_goal_strip(p, goal_strip_rect(w), snap);
    let m = 16.0;
    for (i, label) in ATABS.iter().enumerate() {
        let r = atab_tabs()[i];
        let active = vs.atab == i;
        if active {
            p.round(r, 7.0, &p.br.accent, None);
        } else {
            p.round(r, 7.0, &p.br.card, Some(&p.br.border));
        }
        p.text(
            label,
            rect(r.left, r.top, r.right - r.left, r.bottom - r.top),
            &p.fmt.tab,
            if active { &p.br.white } else { &p.br.muted },
        );
    }

    let content = analytics_content(w, h);
    match vs.atab {
        0 => {
            // 趋势图：与排行页右侧那张同一个画函数，这里只是给它整块宽度。
            draw_line_chart(p, chart_card(content), snap, vs.chart_hover);
        }
        1 => {
            draw_hourly(p, chart_card(content), snap, vs.chart_hover);
        }
        2 => {
            // 星期分布不接悬停：那七根的数本来就已经印在每根柱顶上了，压上去再读一遍
            // 是装饰，不是读数。
            draw_weekday(p, chart_card(content), snap);
        }
        _ => {
            draw_heat(p, content, snap, vs.heat_hover);
        }
    }
    let _ = m;
}

/// 星期分布。
fn draw_weekday(p: &P<'_>, r: D2D_RECT_F, snap: &Snapshot) {
    p.round(r, 14.0, &p.br.card, Some(&p.br.border));
    let pad = 16.0;
    p.text(
        "星期分布",
        rect(r.left + pad, r.top + 14.0, 160.0, 20.0),
        &p.fmt.body,
        &p.br.fg,
    );
    p.text(
        &format!("按{}统计", data::window_text(snap.period)),
        rect(r.right - pad - 140.0, r.top + 16.0, 140.0, 18.0),
        &p.fmt.right,
        &p.br.muted,
    );
    // 星期分布是从每日序列聚合出来的 ⇒ 它认领的是每日趋势那一句。
    let weekday_gaps = gap_note(&snap.gaps, queries::DOMAIN_DAILY);
    if !weekday_gaps.is_empty() {
        p.text(
            &weekday_gaps,
            rect(
                r.left + pad,
                r.top + 36.0,
                (r.right - r.left) - pad * 2.0,
                14.0,
            ),
            &p.fmt.small,
            &p.br.muted,
        );
    }

    let plot = rect(
        r.left + pad,
        r.top + 54.0,
        (r.right - r.left) - pad * 2.0,
        (r.bottom - r.top) - 96.0,
    );
    let max = snap.weekday.iter().cloned().max().unwrap_or(0).max(1) as f32;
    let n = 7;
    let gap = 12.0;
    let bw = ((plot_width(plot) - gap * (n as f32 - 1.0)) / n as f32).max(1.0);
    let labels = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];
    for (i, (&label, &count)) in labels.iter().zip(snap.weekday.iter()).enumerate() {
        let v = count as f32;
        let hh = (v / max) * plot_height(plot);
        let x = plot.left + i as f32 * (bw + gap);
        let bar = rect(x, plot.bottom - hh, bw, hh);
        // 周末用绿色、工作日用强调色 —— 两套主题里都是这两个令牌。
        let base_c = if i >= 5 { p.pal.success } else { p.pal.accent };
        if let Some(b) = p.brush(with_alpha(base_c, 0.85)) {
            p.round(bar, 4.0, &b, None);
        }
        p.text(
            label,
            rect(x, plot.bottom + 6.0, bw, 16.0),
            &p.fmt.center_small,
            &p.br.muted,
        );
        p.text(
            &thousands(count.max(0) as u64),
            rect(x, plot.bottom - hh - 18.0, bw, 16.0),
            &p.fmt.center_small,
            &p.br.muted,
        );
    }
}

// ===== 键盘热力（物理键位，移植自旧 WebView 的 ui/js/charts.js）=====

struct HeatKey {
    name: &'static str,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    label: &'static str,
}

/// 主区一行：（这一行的 y 偏移，行内键位清单）。
type HeatRow = (f32, &'static [(&'static str, f32, f32)]);

const HEAT_MAIN: &[HeatRow] = &[
    (
        0.0,
        &[
            ("Esc", 0.0, 1.0),
            ("F1", 1.5, 1.0),
            ("F2", 2.5, 1.0),
            ("F3", 3.5, 1.0),
            ("F4", 4.5, 1.0),
            ("F5", 6.0, 1.0),
            ("F6", 7.0, 1.0),
            ("F7", 8.0, 1.0),
            ("F8", 9.0, 1.0),
            ("F9", 10.5, 1.0),
            ("F10", 11.5, 1.0),
            ("F11", 12.5, 1.0),
            ("F12", 13.5, 1.0),
        ],
    ),
    (
        1.2,
        &[
            ("`", 0.0, 1.0),
            ("1", 1.0, 1.0),
            ("2", 2.0, 1.0),
            ("3", 3.0, 1.0),
            ("4", 4.0, 1.0),
            ("5", 5.0, 1.0),
            ("6", 6.0, 1.0),
            ("7", 7.0, 1.0),
            ("8", 8.0, 1.0),
            ("9", 9.0, 1.0),
            ("0", 10.0, 1.0),
            ("-", 11.0, 1.0),
            ("=", 12.0, 1.0),
            ("退格", 13.0, 2.0),
        ],
    ),
    (
        2.4,
        &[
            ("Tab", 0.0, 1.5),
            ("Q", 1.5, 1.0),
            ("W", 2.5, 1.0),
            ("E", 3.5, 1.0),
            ("R", 4.5, 1.0),
            ("T", 5.5, 1.0),
            ("Y", 6.5, 1.0),
            ("U", 7.5, 1.0),
            ("I", 8.5, 1.0),
            ("O", 9.5, 1.0),
            ("P", 10.5, 1.0),
            ("[", 11.5, 1.0),
            ("]", 12.5, 1.0),
            ("\\", 13.5, 1.5),
        ],
    ),
    (
        3.6,
        &[
            ("A", 1.75, 1.0),
            ("S", 2.75, 1.0),
            ("D", 3.75, 1.0),
            ("F", 4.75, 1.0),
            ("G", 5.75, 1.0),
            ("H", 6.75, 1.0),
            ("J", 7.75, 1.0),
            ("K", 8.75, 1.0),
            ("L", 9.75, 1.0),
            (";", 10.75, 1.0),
            ("'", 11.75, 1.0),
            ("回车", 12.75, 2.25),
        ],
    ),
    (
        4.8,
        &[
            ("左Shift", 0.0, 2.25),
            ("Z", 2.25, 1.0),
            ("X", 3.25, 1.0),
            ("C", 4.25, 1.0),
            ("V", 5.25, 1.0),
            ("B", 6.25, 1.0),
            ("N", 7.25, 1.0),
            ("M", 8.25, 1.0),
            (",", 9.25, 1.0),
            (".", 10.25, 1.0),
            ("/", 11.25, 1.0),
            ("右Shift", 12.25, 2.75),
        ],
    ),
    (
        6.0,
        &[
            ("左Ctrl", 0.0, 1.25),
            ("左Win", 1.25, 1.25),
            ("左Alt", 2.5, 1.25),
            ("空格", 3.75, 6.25),
            ("右Alt", 10.0, 1.25),
            ("右Win", 11.25, 1.25),
            ("右Ctrl", 13.75, 1.25),
        ],
    ),
];

/// 编辑。
const HEAT_EXTRA: &[(&str, f32, f32, f32, f32, &str)] = &[
    ("Insert", 16.0, 1.2, 1.0, 1.0, "Ins"),
    ("Home", 17.1, 1.2, 1.0, 1.0, "Home"),
    ("PageUp", 18.2, 1.2, 1.0, 1.0, "PgUp"),
    ("Delete", 16.0, 2.4, 1.0, 1.0, "Del"),
    ("End", 17.1, 2.4, 1.0, 1.0, "End"),
    ("PageDown", 18.2, 2.4, 1.0, 1.0, "PgDn"),
    ("↑", 17.1, 3.9, 1.0, 1.0, "↑"),
    ("←", 16.0, 5.1, 1.0, 1.0, "←"),
    ("↓", 17.1, 5.1, 1.0, 1.0, "↓"),
    ("→", 18.2, 5.1, 1.0, 1.0, "→"),
    ("滚轮上滑", 21.38, 1.2, 0.9, 0.85, "滚↑"),
    ("鼠标左键", 20.3, 2.1, 0.9, 1.9, "左键"),
    ("鼠标中键", 21.38, 2.1, 0.9, 0.85, "中键"),
    ("鼠标右键", 22.46, 2.1, 0.9, 1.9, "右键"),
    ("滚轮下滑", 21.38, 3.0, 0.9, 0.85, "滚↓"),
];

const KH_W: f32 = 23.4;
const KH_H: f32 = 7.15;

fn heat_keys() -> Vec<HeatKey> {
    let mut out = Vec::with_capacity(96);
    for &(y, keys) in HEAT_MAIN {
        for &(name, x, w) in keys {
            out.push(HeatKey {
                name,
                x,
                y,
                w,
                h: 1.0,
                label: name,
            });
        }
    }
    for &(name, x, y, w, h, label) in HEAT_EXTRA {
        out.push(HeatKey {
            name,
            x,
            y,
            w,
            h,
            label,
        });
    }
    out
}

/// 版面原点与单位：整块板子等比缩放后居中。
fn heat_geom(r: D2D_RECT_F) -> (f32, f32, f32) {
    let avail_w = (r.right - r.left) - 40.0;
    let avail_h = (r.bottom - r.top) - 104.0;
    let u = (avail_w / KH_W).min(avail_h / KH_H).max(1.0);
    let board_w = KH_W * u;
    let board_h = KH_H * u;
    let ox = r.left + ((r.right - r.left) - board_w) / 2.0;
    let oy = r.top + 64.0 + ((avail_h - board_h) / 2.0).max(0.0);
    (u, ox, oy)
}

fn draw_heat(p: &P<'_>, r: D2D_RECT_F, snap: &Snapshot, hover: Option<usize>) {
    p.round(r, 14.0, &p.br.card, Some(&p.br.border));
    p.text(
        "键盘热力",
        rect(r.left + 16.0, r.top + 14.0, 160.0, 20.0),
        &p.fmt.body,
        &p.br.fg,
    );
    p.text(
        &format!("按{}统计 · 前 100 键", period_text(snap.period)),
        rect(r.right - 16.0 - 260.0, r.top + 16.0, 260.0, 18.0),
        &p.fmt.right,
        &p.br.muted,
    );

    // 次数：AltGr 与「右Alt」是同一个物理键，合并（charts.js 里的 KH_ALIASES）。
    let mut counts: HashMap<&str, i64> = HashMap::new();
    for (k, v) in &snap.rank {
        let name: &str = if k == "AltGr" { "右Alt" } else { k.as_str() };
        *counts.entry(name).or_insert(0) += *v;
    }
    let keys = heat_keys();
    let max = keys
        .iter()
        .map(|k| counts.get(k.name).copied().unwrap_or(0))
        .max()
        .unwrap_or(0)
        .max(1) as f32;
    let (u, ox, oy) = heat_geom(r);
    let total = snap.period_total.max(0);

    for (i, k) in keys.iter().enumerate() {
        let count = counts.get(k.name).copied().unwrap_or(0);
        let base = rect(ox + k.x * u, oy + k.y * u, k.w * u - 2.0, k.h * u - 2.0);
        p.round(base, 3.0, &p.br.card, Some(&p.br.grid));
        if count > 0 {
            let heat = 0.06 + 0.8 * (count as f32 / max).min(1.0);
            if let Some(b) = p.brush(with_alpha(p.pal.accent, heat)) {
                p.round(base, 3.0, &b, None);
            }
        }
        if hover == Some(i) {
            p.round(base, 3.0, &p.br.card, Some(&p.br.accent));
        }
        // 键帽窄（鼠标簇、方向键）就换小一号字。
        let fmt = if base.right - base.left < 34.0 {
            &p.fmt.center_tiny
        } else {
            &p.fmt.center_small
        };
        p.text(
            k.label,
            base,
            fmt,
            if count > 0 { &p.br.fg } else { &p.br.muted },
        );
    }

    // 图例
    let ly = r.bottom - 40.0;
    p.text(
        "次数",
        rect(r.left + 16.0, ly + 4.0, 34.0, 16.0),
        &p.fmt.small,
        &p.br.muted,
    );
    for i in 0..5 {
        let t = i as f32 / 4.0;
        if let Some(b) = p.brush(with_alpha(p.pal.accent, 0.06 + 0.8 * t)) {
            p.round(
                rect(r.left + 54.0 + i as f32 * 18.0, ly + 4.0, 14.0, 14.0),
                3.0,
                &b,
                None,
            );
        }
    }
    p.text(
        "少",
        rect(r.left + 54.0 + 5.0 * 18.0 + 2.0, ly + 4.0, 20.0, 14.0),
        &p.fmt.small,
        &p.br.muted,
    );

    // 板上没摆的键（输入法标点、合成键名）汇总成一句，不静默丢数据。
    let board: std::collections::HashSet<&str> = keys.iter().map(|k| k.name).collect();
    let (mut other_n, mut other_c) = (0usize, 0i64);
    for (k, v) in &snap.rank {
        let name: &str = if k == "AltGr" { "右Alt" } else { k.as_str() };
        if !board.contains(name) {
            other_n += 1;
            other_c += *v;
        }
    }
    let line = hover
        .and_then(|i| {
            keys.get(i)
                .map(|k| (k, counts.get(k.name).copied().unwrap_or(0)))
        })
        .map(|(k, c)| {
            let pct = if total > 0 {
                c as f32 * 100.0 / total as f32
            } else {
                0.0
            };
            format!("{} · {} 次 · {pct:.1}%", k.name, thousands(c.max(0) as u64))
        })
        .unwrap_or_else(|| format!("其它按键：{other_n} 个 · {other_c} 次（板上没摆的键）"));
    p.text(
        &line,
        rect(r.left + 170.0, ly + 4.0, r.right - r.left - 194.0, 16.0),
        &p.fmt.small,
        &p.br.muted,
    );
}

// ===== 贴图（pin） =====

/// 一张贴图：内存里那份烤好的 BGRA。贴图自己不落盘、也不回头解码 PNG ——
/// 截图那一步已经存过一次，这里只是把同一份像素再摆到桌面上。
#[derive(Clone)]
struct PinImage {
    hwnd: isize,
    img: Vec<u8>,
    w: i32,
    h: i32,
    /// 缩放档：滚轮按 [`PIN_ZOOM_STEPS`] 走档（不是连乘），夹在 [`PIN_MIN_SCALE`]..=[`PIN_MAX_SCALE`]。
    scale: f32,
    /// 缩放角标的显示起点：`None` = 不显示。滚轮每按一下重开一秒二。
    badge: Option<std::time::Instant>,
}

/// 正在被拖的那一张：窗口、按下时的窗口左上角、按下时的光标坐标。
#[derive(Clone, Copy)]
struct PinDrag {
    hwnd: isize,
    origin: (i32, i32),
    cursor: (i32, i32),
}

/// 截图出图那一句（成与败都出一句）：`snip` 把它填在这里，`WM_SNIP_NOTE` 让主窗口的 wndproc
/// 取走挂进状态读数那一行。旧 Webview 版是主窗口一个 toast 说同样的事——同一处地方已经在显示
/// "周报已生成 / 备份失败：…"，他要问"刚才那张去哪了"也是在那儿看。
static SNIP_NOTE: Mutex<Option<String>> = Mutex::new(None);

/// 把一句话递给主窗口的状态读数。`target` 是主窗口：截图覆盖层各处都是被它拉起来的
/// （`snip::start(hwnd, ..)` 传进来的就是它）。句柄为空（测试里那条独立会话）就什么都不做。
///
/// 走"填一格 + 投一条"而不是直接改 `App`：覆盖层的 wndproc 与主窗口跑在同一条线程上，
/// 正在处理自己消息的时候去改别人的状态就是重入——这套代码里已经吃过一次锁重入的亏。
pub(crate) fn note_snip_result(target: HWND, line: String) {
    unsafe {
        if target.0.is_null() {
            return;
        }
        if let Ok(mut q) = SNIP_NOTE.lock() {
            *q = Some(line);
        }
        let _ = PostMessageW(Some(target), WM_SNIP_NOTE, WPARAM(0), LPARAM(0));
    }
}

/// 桌面上开着的贴图。用 `Vec` 而不是 `HashMap`：`Mutex::new(Vec::new())` 才进得了 static。
static PINS: Mutex<Vec<PinImage>> = Mutex::new(Vec::new());
/// 屏上探针共用的串行锁：两条都会动**真实光标**、都往桌面上盖真窗口。
/// 并行跑时那条全屏覆盖层会把贴图挡在后面 —— 悬停判据于是假红（实测过）。
///
/// 这里用 `allow(dead_code)` 而不是"只在测试构建里存在"那个属性：main.rs 有一条守卫用例
/// 按文件里**第一次出现**那个属性来界定"生产代码到此为止"，在生产区中间写它（连注释里
/// 写出那个字面量都不行）会把它的扫描截短，两条不相干的用例一起红（实测踩过）。
#[allow(dead_code)] // 只有测试里的探针取它
pub(crate) static SCREEN_PROBE: Mutex<()> = Mutex::new(());
/// 取探针锁：前一位可能 panic 过，中毒不该把这一位也判死。
#[allow(dead_code)] // 同上
pub(crate) fn probe_guard() -> std::sync::MutexGuard<'static, ()> {
    SCREEN_PROBE.lock().unwrap_or_else(|e| e.into_inner())
}
static PIN_DRAG: Mutex<Option<PinDrag>> = Mutex::new(None);

/// 悬停在哪张贴图上：只有它显示 ✕（旧 Webview 版同一口径 —— 不悬停时不拿关闭按钮抢画面）。
static PIN_HOVER: Mutex<Option<isize>> = Mutex::new(None);

/// ✕ 的边长，以及它离右上角的内缩。
const PIN_CLOSE_SIZE: i32 = 24;
const PIN_CLOSE_INSET: i32 = 2;
/// 缩放角标显示多久（毫秒）：旧 Webview 版是 1.2 秒，跟着它。
const PIN_BADGE_MS: u32 = 1200;
/// 那个一次性定时器 id（`0x5042` = 'PB'）：时间到就重画一次，把角标抹掉。
const PIN_BADGE_TIMER: usize = 0x5042;

/// ✕ 的落点（客户区左上角 + 边长）。图太小放不下就缩着贴，缩到 1 像素也仍在右上角，
/// 不许算出负坐标 —— 负坐标的按钮永远点不到，等于把这条退路偷偷关掉。
fn pin_close_box(w: i32, h: i32) -> (i32, i32, i32, i32) {
    let side = PIN_CLOSE_SIZE
        .min((w / 3).max(1))
        .min((h / 3).max(1))
        .max(1);
    let inset = PIN_CLOSE_INSET
        .min((w - side).max(0))
        .min((h - side).max(0));
    (w - side - inset, inset, side, side)
}

/// 角标上的字：`125%`（四舍五入到整百分点）。
fn pin_badge_text(scale: f32) -> String {
    format!("{}%", (scale * 100.0).round() as i32)
}

/// 放大之后把窗口拉回工作区，让右上角那颗 ✕ 还够得着。
///
/// 只夹"够得着"这一件事：贴住边就停，不强行整窗可见 —— 一张比屏还高的贴图本来就该
/// 能贴着上沿放，把整窗硬塞进来会让用户以为贴图跑掉了。
fn pin_keep_close_visible(
    pos: (i32, i32),
    size: (i32, i32),
    work: (i32, i32, i32, i32),
) -> (i32, i32) {
    let mut x = pos.0;
    let mut y = pos.1;
    if x + size.0 > work.2 {
        x = work.2 - size.0;
    }
    if x < work.0 {
        x = work.0;
    }
    if y + size.1 > work.3 {
        y = work.3 - size.1;
    }
    if y < work.1 {
        y = work.1;
    }
    (x, y)
}

/// 这张贴图所在显示器的**工作区**（已经扣掉任务栏那一条）。
fn pin_work_area(hwnd: HWND) -> (i32, i32, i32, i32) {
    unsafe {
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let mut mi = mi;
        if GetMonitorInfoW(mon, &mut mi).as_bool() {
            return (
                mi.rcWork.left,
                mi.rcWork.top,
                mi.rcWork.right,
                mi.rcWork.bottom,
            );
        }
        (
            0,
            0,
            GetSystemMetrics(SM_CXSCREEN),
            GetSystemMetrics(SM_CYSCREEN),
        )
    }
}

/// 缩放的两个界**由档表首末导出**（[`PIN_ZOOM_STEPS`]）：写成两个字面量迟早会和档表漂开，
/// 漂开之后的症状是"滚到顶了，可界还没到"。
const PIN_MIN_SCALE: f32 = PIN_ZOOM_STEPS[0];
const PIN_MAX_SCALE: f32 = PIN_ZOOM_STEPS[PIN_ZOOM_STEPS.len() - 1];
/// 同时最多几张：每张都是一份完整像素，拍个上限免得贴满桌面后吃光内存。
const PIN_MAX_COUNT: usize = 12;

fn pin_find(hwnd: isize) -> Option<PinImage> {
    PINS.lock()
        .ok()
        .and_then(|v| v.iter().find(|p| p.hwnd == hwnd).cloned())
}

/// 开一张贴图：置顶、不进任务栏（toolwindow）、按住拖动、滚轮缩放、Esc / 右键关掉这一张。
fn open_pin(img: Vec<u8>, w: i32, h: i32, x: i32, y: i32) -> Option<HWND> {
    unsafe {
        if PINS.lock().map(|v| v.len()).unwrap_or(0) >= PIN_MAX_COUNT {
            tracing_note(&format!("贴图 pin-skip 已到上限 {PIN_MAX_COUNT} 张"));
            return None;
        }
        let hinst = GetModuleHandleW(None).map(|m| m.into()).unwrap_or_default();
        let cls = HSTRING::from("FocusFlowPin");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(pin_proc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: PCWSTR(cls.as_ptr()),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);
        let created = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            &cls,
            &HSTRING::from("FocusFlow 贴图"),
            WINDOW_STYLE(WS_POPUP.0 | WS_VISIBLE.0),
            x,
            y,
            w,
            h,
            None,
            None,
            Some(hinst),
            None,
        );
        match created {
            Ok(pin_hwnd) => {
                if let Ok(mut v) = PINS.lock() {
                    v.push(PinImage {
                        hwnd: pin_hwnd.0 as isize,
                        img,
                        w,
                        h,
                        scale: 1.0,
                        badge: None,
                    });
                }
                let _ = SetFocus(Some(pin_hwnd));
                tracing_note(&format!("贴图 pin-open {w}x{h} at {x},{y}"));
                // 把 ✕ 的尺寸与角标时长告诉外面：验收脚本按这一行找那颗按钮，
                // 不自己抄第二把尺子（这仓里"同一个常量写两遍"栽过不止一次）。
                tracing_note(&format!(
                    "贴图 pin-chrome close_size={PIN_CLOSE_SIZE} close_inset={PIN_CLOSE_INSET} badge_ms={PIN_BADGE_MS}"
                ));
                Some(pin_hwnd)
            }
            Err(e) => {
                tracing_note(&format!("贴图 pin-fail 建窗失败：{e}"));
                None
            }
        }
    }
}

/// 滚轮走的是**档位**，不是"当前档 ×1.1"：连乘会让 100% 越走越回不去
/// （1.0 → 1.1 → 退一格是 0.99），而"图上一行像素＝屏上一行像素"这一档恰恰是最该能落准的。
/// 旧 Webview 版给的就是一串含 1.0 的离散档，这里按同一口径搬回来。
const PIN_ZOOM_STEPS: [f32; 13] = [
    0.25, 0.33, 0.5, 0.67, 0.75, 0.9, 1.0, 1.1, 1.25, 1.5, 2.0, 3.0, 4.0,
];

/// 从 `current` 往上/下走 `notches` 格，一格＝**该方向上相邻的那一档**。
/// 值不在档上时（旧档位留下的、手改配置的、浮点误差的）先走到它前面的那一档，而不跨过——
/// 所以从 0.99 往上一格是 1.0，"退一格回到 100%"这句话现在是真的。
/// 首末两档就是 [`PIN_MIN_SCALE`] 与 [`PIN_MAX_SCALE`]，滚到头上停住。
fn pin_next_scale(current: f32, notches: f32) -> f32 {
    let steps = notches.abs().round().max(1.0) as usize;
    let top = PIN_MAX_SCALE;
    let bottom = PIN_MIN_SCALE;
    let mut v = current;
    for _ in 0..steps {
        v = if notches >= 0.0 {
            PIN_ZOOM_STEPS
                .iter()
                .copied()
                .find(|s| *s > v + 1e-6)
                .unwrap_or(top)
        } else {
            PIN_ZOOM_STEPS
                .iter()
                .copied()
                .rev()
                .find(|s| *s < v - 1e-6)
                .unwrap_or(bottom)
        };
    }
    v
}

/// 某档缩放下的窗口尺寸（像素）。至少 1×1，否则窗口建不出来、图也画不出。
fn pin_scaled_size(w: i32, h: i32, scale: f32) -> (i32, i32) {
    (
        ((w as f32 * scale).round() as i32).max(1),
        ((h as f32 * scale).round() as i32).max(1),
    )
}

fn pin_set_scale(hwnd: HWND, scale: f32) {
    let Some(p) = pin_find(hwnd.0 as isize) else {
        return;
    };
    if let Ok(mut v) = PINS.lock() {
        if let Some(item) = v.iter_mut().find(|x| x.hwnd == hwnd.0 as isize) {
            item.scale = scale;
        }
    }
    let (w, h) = pin_scaled_size(p.w, p.h, scale);
    unsafe {
        let _ = SetWindowPos(hwnd, None, 0, 0, w, h, SWP_NOMOVE | SWP_NOZORDER);
        // 放大后右上角的 ✕ 可能顶到屏幕外：把窗口拉回工作区，让那颗按钮还够得着。
        let mut rc = RECT::default();
        let _ = GetWindowRect(hwnd, &mut rc);
        let (nx, ny) = pin_keep_close_visible((rc.left, rc.top), (w, h), pin_work_area(hwnd));
        if nx != rc.left || ny != rc.top {
            let _ = SetWindowPos(hwnd, None, nx, ny, 0, 0, SWP_NOSIZE | SWP_NOZORDER);
            tracing_note(&format!("贴图 pin-move 为保证 ✕ 可见挪到 {nx},{ny}"));
        }
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

/// 验收钩子：开 N 张合成贴图（棋盘格）。数窗口、验「关掉所有贴图」与「贴图不落盘」用它，
/// 不需要真去拖一次框。
fn open_test_pins(count: usize) {
    let (w, h) = (120i32, 80i32);
    for i in 0..count {
        let mut img = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let on = ((x / 12) + (y / 12) + i as i32) % 2 == 0;
                let px: [u8; 4] = if on {
                    [0x5B, 0x9C, 0xF7, 0xFF]
                } else {
                    [0x1F, 0x28, 0x3B, 0xFF]
                };
                let o = ((y * w + x) * 4) as usize;
                img[o..o + 4].copy_from_slice(&px);
            }
        }
        open_pin(img, w, h, 420 + (i as i32) * 40, 320 + (i as i32) * 30);
    }
}

/// 关掉所有贴图（设置页那一行 / 验收钩子）。已存的 PNG 一个字都不动。
fn close_all_pins() -> usize {
    let list: Vec<isize> = PINS
        .lock()
        .map(|v| v.iter().map(|p| p.hwnd).collect())
        .unwrap_or_default();
    let n = list.len();
    for raw in list {
        unsafe {
            let _ = DestroyWindow(HWND(raw as *mut core::ffi::c_void));
        }
    }
    tracing_note(&format!("贴图 pin-close-all {n}"));
    n
}

unsafe extern "system" fn pin_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                if let Some(p) = pin_find(hwnd.0 as isize) {
                    let mut rc = RECT::default();
                    let _ = GetClientRect(hwnd, &mut rc);
                    let mut info = BITMAPINFO::default();
                    info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
                    info.bmiHeader.biWidth = p.w;
                    info.bmiHeader.biHeight = -p.h;
                    info.bmiHeader.biPlanes = 1;
                    info.bmiHeader.biBitCount = 32;
                    info.bmiHeader.biCompression = BI_RGB.0;
                    // 画满客户区：窗口尺寸就是"原图 × 缩放档"，所以这里不需要再乘一遍。
                    let _ = StretchDIBits(
                        hdc,
                        0,
                        0,
                        rc.right - rc.left,
                        rc.bottom - rc.top,
                        0,
                        0,
                        p.w,
                        p.h,
                        Some(p.img.as_ptr() as *const _),
                        &info,
                        DIB_RGB_COLORS,
                        SRCCOPY,
                    );
                    let cw = rc.right - rc.left;
                    let ch = rc.bottom - rc.top;
                    // 悬停才显示 ✕：贴图常常一排贴在桌面角落，常驻的关闭按钮会挡住图本身。
                    let hovered = PIN_HOVER
                        .lock()
                        .map(|h| *h == Some(hwnd.0 as isize))
                        .unwrap_or(false);
                    if hovered {
                        let (bx, by, bs, _) = pin_close_box(cw, ch);
                        let bg = CreateSolidBrush(COLORREF(0x0020_2020));
                        let oldb = SelectObject(hdc, bg.into());
                        let pen = CreatePen(PS_SOLID, 1, COLORREF(0x00E0_E0E0));
                        let oldp = SelectObject(hdc, pen.into());
                        let _ = Rectangle(hdc, bx, by, bx + bs, by + bs);
                        let pad = (bs / 4).max(2);
                        let _ = MoveToEx(hdc, bx + pad, by + pad, None);
                        let _ = LineTo(hdc, bx + bs - pad, by + bs - pad);
                        let _ = MoveToEx(hdc, bx + bs - pad, by + pad, None);
                        let _ = LineTo(hdc, bx + pad, by + bs - pad);
                        SelectObject(hdc, oldp);
                        SelectObject(hdc, oldb);
                        let _ = DeleteObject(pen.into());
                        let _ = DeleteObject(bg.into());
                    }
                    // 缩放角标：只在滚轮之后的一秒二里出现（旧版同款，量完就自己抹掉）。
                    let show_badge = p
                        .badge
                        .map(|t| {
                            t.elapsed() < std::time::Duration::from_millis(PIN_BADGE_MS as u64)
                        })
                        .unwrap_or(false);
                    if show_badge {
                        let font = CreateFontW(
                            16,
                            0,
                            0,
                            0,
                            FW_NORMAL.0 as i32,
                            0,
                            0,
                            0,
                            DEFAULT_CHARSET,
                            OUT_DEFAULT_PRECIS,
                            CLIP_DEFAULT_PRECIS,
                            CLEARTYPE_QUALITY,
                            DEFAULT_PITCH.0 as u32,
                            w!("Microsoft YaHei UI"),
                        );
                        let oldf = SelectObject(hdc, font.into());
                        let bg = CreateSolidBrush(COLORREF(0x0020_2020));
                        let oldb = SelectObject(hdc, bg.into());
                        let _ = Rectangle(hdc, 4, ch - 26, 74, ch - 4);
                        let null = GetStockObject(NULL_BRUSH);
                        let _ = SelectObject(hdc, null);
                        let _ = SetTextColor(hdc, COLORREF(0x00FF_FFFF));
                        let _ = SetBkMode(hdc, TRANSPARENT);
                        let text = pin_badge_text(p.scale);
                        let wide: Vec<u16> = text.encode_utf16().collect();
                        let _ = TextOutW(hdc, 10, ch - 22, &wide);
                        SelectObject(hdc, oldb);
                        SelectObject(hdc, oldf);
                        let _ = DeleteObject(bg.into());
                        let _ = DeleteObject(font.into());
                    }
                }
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let _ = SetFocus(Some(hwnd));
                let cur = (
                    (l.0 & 0xFFFF) as i16 as i32,
                    ((l.0 >> 16) & 0xFFFF) as i16 as i32,
                );
                let mut cr = RECT::default();
                let _ = GetClientRect(hwnd, &mut cr);
                let (bx, by, bs, _) = pin_close_box(cr.right - cr.left, cr.bottom - cr.top);
                let hovered = PIN_HOVER
                    .lock()
                    .map(|h| *h == Some(hwnd.0 as isize))
                    .unwrap_or(false);
                if hovered && cur.0 >= bx && cur.0 < bx + bs && cur.1 >= by && cur.1 < by + bs {
                    // 点 ✕ = 关掉这一张：与右键、Esc 是同一条退路，只多了一个看得见的入口。
                    // 只有按钮**画出来了**的那一张才认这一下 —— 否则用户想拖贴图、
                    // 按在右上角就把它关掉了，那是不存在的按钮咬人。
                    tracing_note("贴图 pin-close-btn 点了 ✕");
                    let _ = DestroyWindow(hwnd);
                    return LRESULT(0);
                }
                let _ = SetCapture(hwnd);
                let mut rc = RECT::default();
                let _ = GetWindowRect(hwnd, &mut rc);
                if let Ok(mut d) = PIN_DRAG.lock() {
                    *d = Some(PinDrag {
                        hwnd: hwnd.0 as isize,
                        origin: (rc.left, rc.top),
                        cursor: cur,
                    });
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let cx = (l.0 & 0xFFFF) as i16 as i32;
                let cy = ((l.0 >> 16) & 0xFFFF) as i16 as i32;
                let drag = PIN_DRAG.lock().ok().and_then(|d| *d);
                if let Some(d) = drag.filter(|d| d.hwnd == hwnd.0 as isize) {
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        d.origin.0 + (cx - d.cursor.0),
                        d.origin.1 + (cy - d.cursor.1),
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOZORDER,
                    );
                    return LRESULT(0);
                }
                // 不在拖动：这一趟只判"光标是不是压在 ✕ 上"，压着才把按钮画出来。
                if pin_find(hwnd.0 as isize).is_none() {
                    return LRESULT(0);
                }
                let mut cr = RECT::default();
                let _ = GetClientRect(hwnd, &mut cr);
                let (bx, by, bs, _) = pin_close_box(cr.right - cr.left, cr.bottom - cr.top);
                let over = cx >= bx && cx < bx + bs && cy >= by && cy < by + bs;
                let changed = PIN_HOVER
                    .lock()
                    .map(|mut h| {
                        let prev = *h;
                        *h = if over {
                            Some(hwnd.0 as isize)
                        } else if prev == Some(hwnd.0 as isize) {
                            None
                        } else {
                            prev
                        };
                        prev != *h
                    })
                    .unwrap_or(false);
                let _ = TrackMouseEvent(&mut TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                });
                if changed {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                let changed = PIN_HOVER
                    .lock()
                    .map(|mut h| {
                        if *h == Some(hwnd.0 as isize) {
                            *h = None;
                            true
                        } else {
                            false
                        }
                    })
                    .unwrap_or(false);
                if changed {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_TIMER => {
                // 只挂了一个定时器（角标到期重画），id 照样要对上：别的 WM_TIMER 不该被当成它。
                if w.0 == PIN_BADGE_TIMER {
                    let _ = KillTimer(Some(hwnd), PIN_BADGE_TIMER);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let _ = ReleaseCapture();
                if let Ok(mut d) = PIN_DRAG.lock() {
                    *d = None;
                }
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                let notches = (w.0 >> 16) as i16 as f32 / 120.0;
                let base = pin_find(hwnd.0 as isize).map(|p| p.scale).unwrap_or(1.0);
                let next = pin_next_scale(base, notches);
                pin_set_scale(hwnd, next);
                // 角标开一秒二：缩放档只有配着数字才知道现在是多少。
                if let Ok(mut v) = PINS.lock() {
                    if let Some(item) = v.iter_mut().find(|x| x.hwnd == hwnd.0 as isize) {
                        item.badge = Some(std::time::Instant::now());
                    }
                }
                let _ = SetTimer(Some(hwnd), PIN_BADGE_TIMER, PIN_BADGE_MS, None);
                tracing_note(&format!(
                    "贴图 pin-scale {}%",
                    (next * 100.0).round() as i32
                ));
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_KEYDOWN if w.0 == 0x1B => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                if let Ok(mut v) = PINS.lock() {
                    v.retain(|p| p.hwnd != hwnd.0 as isize);
                }
                // 悬停标记必须跟着清：句柄数值会被系统复用，留着它会让**下一张**贴图
                // 一开出来就顶着 ✕（而这张贴图还没被碰过）。
                if let Ok(mut h) = PIN_HOVER.lock() {
                    if *h == Some(hwnd.0 as isize) {
                        *h = None;
                    }
                }
                let _ = KillTimer(Some(hwnd), PIN_BADGE_TIMER);
                tracing_note("贴图 pin-close");
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }
}

// ===== 插件视图（widget 树） =====

/// 插件视图里一条渲染操作：绘制与命中测试共用同一份（避免两套坐标）。
enum POp {
    Heading {
        rect: D2D_RECT_F,
        text: String,
    },
    Label {
        rect: D2D_RECT_F,
        text: String,
    },
    Kv {
        rect: D2D_RECT_F,
        key: String,
        value: String,
    },
    Sep {
        rect: D2D_RECT_F,
    },
    Body {
        rect: D2D_RECT_F,
        text: String,
    },
    Field {
        rect: D2D_RECT_F,
        label: String,
        value: String,
        /// 回写给插件的字段名（`set_field`）；空串 = 只读。
        field: String,
    },
    /// 下拉：值框 + 菜单（菜单本体由 [`draw_select_menu`] 画在最上层）。
    Select {
        rect: D2D_RECT_F,
        label: String,
        value: String,
        field: String,
        options: Vec<(String, String)>,
    },
    Button {
        rect: D2D_RECT_F,
        text: String,
        id: String,
        disabled: bool,
        /// 非空 = 点它只打开这个 `modal_form`，不触发插件动作（与 Web 端 `button.modal` 同义）。
        modal: Option<String>,
        /// `button.sel`：动作 id = 本按钮 id 直接拼上该分组选中行的记录 id（无分隔符）。
        sel: bool,
        /// `sel` 按钮读哪个分组的选中行；空 = 读不分组的表（旧版前端的 `__main` 一格）。
        group: String,
    },
    Table {
        rect: D2D_RECT_F,
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
        /// 行内操作按钮：(矩形, 动作 id, 按钮文字)
        buttons: Vec<(D2D_RECT_F, String, String)>,
        /// 数据列可用宽度（右侧留给操作按钮）
        column_w: f32,
        /// 与 `rows` 平行的记录 id；非空 = 这一行可点选。
        ids: Vec<String>,
        /// 分组名：非空 = 单选，且选中集只属于这一组；空 = 多选自成一格。
        group: String,
        /// 行选中时要写回的插件字段名（空 = 不写）。
        onselect: String,
        /// 当前选中的记录 id（画图用的那一份高亮）。
        sel: Vec<String>,
    },
}

impl POp {
    fn bounds(&self) -> D2D_RECT_F {
        match self {
            POp::Heading { rect, .. }
            | POp::Label { rect, .. }
            | POp::Kv { rect, .. }
            | POp::Sep { rect }
            | POp::Body { rect, .. }
            | POp::Field { rect, .. }
            | POp::Select { rect, .. }
            | POp::Button { rect, .. }
            | POp::Table { rect, .. } => *rect,
        }
    }
}

/// 插件视图内容有多高（用于把滚动位置夹在合法范围内）。
fn plugin_content_height(view: &focusflow_core::plugins::PluginView, area: D2D_RECT_F) -> f32 {
    let ops = plugin_view_ops(view, area, 0.0, &HashMap::new());
    let bottom = ops
        .iter()
        .map(|op| op.bounds().bottom)
        .fold(0.0f32, f32::max);
    (bottom - area.top).max(0.0)
}

/// 插件表格的行几何：表头与行高是画、命中、行内按钮三处共用的同一把尺子。
const PV_TABLE_HEAD_H: f32 = 24.0;
const PV_TABLE_ROW_H: f32 = 20.0;

/// 第 `i` 行的整行矩形（画图高亮与点击命中都从这里算，别各写一遍）。
fn pv_table_row_rect(r: &D2D_RECT_F, i: usize) -> D2D_RECT_F {
    rect(
        r.left,
        r.top + PV_TABLE_HEAD_H + i as f32 * PV_TABLE_ROW_H,
        r.right - r.left,
        PV_TABLE_ROW_H,
    )
}

/// 点一行之后这一组的选中 id。分组表格（`group` 非空）是单选：点谁就只留谁，
/// 再点同一行还是它；不分组的表是多选：再点同一行取消。顺序按点击先后 ——
/// `sel` 按钮拼 id 时要按这个顺序逗号连接。
fn apply_row_select(group: &str, current: &[String], id: &str) -> Vec<String> {
    if !group.is_empty() {
        return vec![id.to_string()];
    }
    let mut out: Vec<String> = current.to_vec();
    match out.iter().position(|x| x == id) {
        Some(i) => {
            out.remove(i);
        }
        None => out.push(id.to_string()),
    }
    out
}

/// `button.sel` 要点火的动作 id：按钮 id 直接拼上该分组选中的记录 id（无分隔符，
/// 多个用逗号 —— 这是插件侧 `on_action` 的契约，记账本 `del_42,43` 就是这么吃的）。
/// 没选中、或"一次只吃一条"的按钮选多了 ⇒ `Err(说明)`：宁可不发也不让插件把
/// `"1,2"` 念成空，那是"点了没反应"的老症状。
fn sel_action_id(
    button_id: &str,
    group: &str,
    plugin_sel: &HashMap<String, Vec<String>>,
) -> std::result::Result<String, String> {
    let ids = plugin_sel.get(group).cloned().unwrap_or_default();
    if ids.is_empty() {
        return Err("先在表格里点选一行，再按这颗按钮".to_string());
    }
    if ids.len() > 1 && button_id == "edit_" {
        return Err("「修改」一次只改一条：请先只选中一行".to_string());
    }
    Ok(format!("{button_id}{}", ids.join(",")))
}

/// 把声明式视图排成一条条操作（内容区宽度，纵向堆叠）。
/// `sel` 是各分组当前的选中行 id（只用来把选中态带到画法上；布局与它无关，
/// 所以量内容高度时传空表就行）。
fn plugin_view_ops(
    view: &focusflow_core::plugins::PluginView,
    area: D2D_RECT_F,
    offset: f32,
    plugin_sel: &HashMap<String, Vec<String>>,
) -> Vec<POp> {
    use focusflow_core::plugins::Widget;
    let mut ops = Vec::new();
    let x = area.left + 16.0;
    let w = (area.right - area.left) - 32.0;
    let mut y = area.top + 12.0 - offset;

    fn walk(
        widgets: &[Widget],
        x: f32,
        w: f32,
        y: &mut f32,
        ops: &mut Vec<POp>,
        plugin_sel: &HashMap<String, Vec<String>>,
    ) {
        for widget in widgets {
            match widget {
                Widget::Heading(t) => {
                    ops.push(POp::Heading {
                        rect: rect(x, *y, w, 24.0),
                        text: t.clone(),
                    });
                    *y += 30.0;
                }
                Widget::Label(t) => {
                    ops.push(POp::Label {
                        rect: rect(x, *y, w, 20.0),
                        text: t.clone(),
                    });
                    *y += 22.0;
                }
                Widget::KeyValue(k, v) => {
                    ops.push(POp::Kv {
                        rect: rect(x, *y, w, 20.0),
                        key: k.clone(),
                        value: v.clone(),
                    });
                    *y += 22.0;
                }
                Widget::Separator => {
                    ops.push(POp::Sep {
                        rect: rect(x, *y + 8.0, w, 1.0),
                    });
                    *y += 18.0;
                }
                Widget::TextArea(t) => {
                    let lines = t.lines().count().clamp(1, 5) as f32;
                    ops.push(POp::Body {
                        rect: rect(x, *y, w, lines * 18.0),
                        text: t.clone(),
                    });
                    *y += lines * 18.0 + 8.0;
                }
                Widget::TextInput {
                    field,
                    label,
                    value,
                } => {
                    ops.push(POp::Field {
                        rect: rect(x, *y, w, 26.0),
                        label: label.clone(),
                        value: value.clone(),
                        field: field.clone(),
                    });
                    *y += 30.0;
                }
                Widget::Select {
                    field,
                    label,
                    value,
                    options,
                    ..
                } => {
                    ops.push(POp::Select {
                        rect: rect(x, *y, w, 26.0),
                        label: label.clone(),
                        value: value.clone(),
                        field: field.clone(),
                        options: options.clone(),
                    });
                    *y += 30.0;
                }
                Widget::Button {
                    id,
                    text,
                    disabled,
                    modal,
                    sel,
                    group,
                } => {
                    let bw = ((text.chars().count() as f32) * 15.0 + 28.0).max(96.0);
                    ops.push(POp::Button {
                        rect: rect(x, *y, bw.min(w), 30.0),
                        text: text.clone(),
                        id: id.clone(),
                        disabled: *disabled,
                        modal: modal.clone(),
                        sel: *sel,
                        group: group.clone(),
                    });
                    *y += 38.0;
                }
                Widget::Table {
                    headers,
                    rows,
                    ids,
                    actions,
                    group,
                    onselect,
                    ..
                } => {
                    let shown: Vec<Vec<String>> = rows.iter().take(6).cloned().collect();
                    let h = PV_TABLE_HEAD_H + shown.len() as f32 * PV_TABLE_ROW_H;
                    // 有行内操作时，右侧留出按钮列，数据列相应收窄
                    let mut buttons: Vec<(D2D_RECT_F, String, String)> = Vec::new();
                    let mut reserve = 0.0f32;
                    if !actions.is_empty() {
                        let bw = actions
                            .iter()
                            .map(|(_, l)| (l.chars().count() as f32 * 14.0 + 16.0).max(44.0))
                            .sum::<f32>()
                            + (actions.len() as f32 - 1.0) * 6.0
                            + 12.0;
                        reserve = bw;
                        for (ri, _) in shown.iter().enumerate() {
                            let rid = ids.get(ri).cloned().unwrap_or_default();
                            let mut bx = x + w - 6.0;
                            for (prefix, label) in actions.iter().rev() {
                                let bw = (label.chars().count() as f32 * 14.0 + 16.0).max(44.0);
                                bx -= bw;
                                let by = *y + PV_TABLE_HEAD_H + ri as f32 * PV_TABLE_ROW_H + 1.0;
                                buttons.push((
                                    rect(bx, by, bw, 18.0),
                                    format!("{prefix}{rid}"),
                                    label.clone(),
                                ));
                                bx -= 6.0;
                            }
                        }
                    }
                    ops.push(POp::Table {
                        rect: rect(x, *y, w, h),
                        headers: headers.clone(),
                        rows: shown,
                        buttons,
                        column_w: w - reserve,
                        ids: ids.clone(),
                        group: group.clone(),
                        onselect: onselect.clone(),
                        sel: plugin_sel.get(group.as_str()).cloned().unwrap_or_default(),
                    });
                    *y += h + 6.0;
                }
                Widget::Pager {
                    page, pages, total, ..
                } => {
                    ops.push(POp::Label {
                        rect: rect(x, *y, w, 18.0),
                        text: format!("第 {page} / {pages} 页 · 共 {total} 条"),
                    });
                    *y += 22.0;
                }
                Widget::Row { children } => {
                    // 横向容器先按纵向堆（子控件宽度有限，硬拆列反而更难读）。
                    walk(children, x, w, y, ops, plugin_sel);
                }
                Widget::ModalForm { .. } => {
                    // 弹窗不再按普通控件排在正文里：`draw_plugin_modal` 把它画成整屏遮罩上
                    // 的一张卡片（点在按钮上会设 `pv_modal`，插件自己 `open=true` 的直接开）。
                    // 留出 0 高度，正文不会因为它多一块空白。
                }
            }
        }
    }

    walk(&view.widgets, x, w, &mut y, &mut ops, plugin_sel);
    ops
}

/// 画插件视图；返回"返回"按钮的矩形（点击时用同一份几何）。
fn draw_plugin_view(
    p: &P<'_>,
    w: f32,
    h: f32,
    name: &str,
    view: &focusflow_core::plugins::PluginView,
    scroll: i32,
    plugin_sel: &HashMap<String, Vec<String>>,
) -> D2D_RECT_F {
    let card = rect(16.0, 208.0, w - 32.0, h - 208.0 - 16.0);
    p.round(card, 14.0, &p.br.card, Some(&p.br.border));

    let back = rect(card.left + 16.0, card.top + 12.0, 64.0, 24.0);
    p.round(back, 6.0, &p.br.header, Some(&p.br.border));
    p.text("← 返回", back, &p.fmt.center_small, &p.br.fg);

    let title = if view.title.is_empty() || view.title == name {
        name.to_string()
    } else {
        format!("{name} · {}", view.title)
    };
    p.text(
        &title,
        rect(back.right + 12.0, card.top + 14.0, 420.0, 20.0),
        &p.fmt.body,
        &p.br.fg,
    );

    // 内容区裁剪：视图比卡片高时，滚动到一半的控件不能画到卡片外面去
    let clip = rect(
        card.left + 1.0,
        card.top + 40.0,
        card.right - card.left - 2.0,
        card.bottom - card.top - 42.0,
    );
    unsafe {
        p.rt.PushAxisAlignedClip(&clip, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
    }

    let ops = plugin_view_ops(
        view,
        rect(
            card.left,
            card.top + 40.0,
            card.right - card.left,
            card.bottom - card.top,
        ),
        scroll as f32,
        plugin_sel,
    );
    for op in &ops {
        match op {
            POp::Heading { rect: r, text } => {
                p.text(text, *r, &p.fmt.body, &p.br.fg);
            }
            POp::Label { rect: r, text } => {
                p.text(text, *r, &p.fmt.small, &p.br.muted);
            }
            POp::Kv {
                rect: r,
                key,
                value,
            } => {
                p.text(
                    key,
                    rect(r.left, r.top, 140.0, 18.0),
                    &p.fmt.small,
                    &p.br.muted,
                );
                p.text(
                    value,
                    rect(r.left + 146.0, r.top, r.right - r.left - 146.0, 18.0),
                    &p.fmt.small,
                    &p.br.fg,
                );
            }
            POp::Sep { rect: r } => p.fill(*r, &p.br.grid),
            POp::Body { rect: r, text } => {
                p.text(text, *r, &p.fmt.small, &p.br.muted);
            }
            POp::Field {
                rect: r,
                label,
                value,
                field,
            } => {
                p.text(
                    label,
                    rect(r.left, r.top + 4.0, 140.0, 18.0),
                    &p.fmt.small,
                    &p.br.muted,
                );
                let vb = rect(r.left + 146.0, r.top, 260.0, 24.0);
                // 可改的给强调色边框：一眼能看出这格是输入框而不是只读值
                let border = if field.is_empty() {
                    &p.br.border
                } else {
                    &p.br.accent
                };
                p.round(vb, 6.0, &p.br.card, Some(border));
                p.text(
                    value,
                    rect(vb.left + 8.0, vb.top + 3.0, vb.right - vb.left - 16.0, 18.0),
                    &p.fmt.small,
                    &p.br.fg,
                );
            }
            POp::Select {
                rect: r,
                label,
                value,
                ..
            } => {
                p.text(
                    label,
                    rect(r.left, r.top + 4.0, 140.0, 18.0),
                    &p.fmt.small,
                    &p.br.muted,
                );
                let vb = select_value_rect(*r);
                p.round(vb, 6.0, &p.br.card, Some(&p.br.accent));
                p.text(
                    value,
                    rect(vb.left + 8.0, vb.top + 3.0, vb.right - vb.left - 28.0, 18.0),
                    &p.fmt.small,
                    &p.br.fg,
                );
                p.text(
                    "▾",
                    rect(vb.right - 20.0, vb.top + 3.0, 16.0, 18.0),
                    &p.fmt.center_small,
                    &p.br.muted,
                );
            }
            POp::Button {
                rect: r,
                text,
                disabled,
                ..
            } => {
                if *disabled {
                    p.round(*r, 6.0, &p.br.grid, None);
                    p.text(text, *r, &p.fmt.center_small, &p.br.muted);
                } else {
                    p.round(*r, 6.0, &p.br.accent, None);
                    p.text(text, *r, &p.fmt.center_small, &p.br.white);
                }
            }
            POp::Table {
                rect: r,
                headers,
                rows,
                buttons,
                column_w,
                ids,
                sel: chosen,
                ..
            } => {
                p.fill(
                    rect(r.left, r.top, r.right - r.left, PV_TABLE_HEAD_H),
                    &p.br.header,
                );
                let cols = headers.len().max(1);
                let cw = (*column_w).max(40.0) / cols as f32;
                for (i, htxt) in headers.iter().enumerate() {
                    p.text(
                        htxt,
                        rect(r.left + 6.0 + i as f32 * cw, r.top + 4.0, cw - 12.0, 16.0),
                        &p.fmt.small,
                        &p.br.muted,
                    );
                }
                for (ri, row) in rows.iter().enumerate() {
                    let rr = pv_table_row_rect(r, ri);
                    // 选中行整条压主色底（不另开勾选列：布局与内容高度都不该因选中而变）。
                    let picked = ids
                        .get(ri)
                        .map(|id| chosen.iter().any(|c| c == id))
                        .unwrap_or(false);
                    if picked {
                        p.round(rr, 3.0, &p.br.accent, None);
                    }
                    let ink = if picked { &p.br.white } else { &p.br.fg };
                    for (ci, cell) in row.iter().enumerate().take(cols) {
                        p.text(
                            cell,
                            rect(
                                rr.left + 6.0 + ci as f32 * cw,
                                rr.top + 2.0,
                                cw - 12.0,
                                16.0,
                            ),
                            &p.fmt.small,
                            ink,
                        );
                    }
                }
                // 行内操作按钮
                for (br, _, label) in buttons {
                    p.round(*br, 4.0, &p.br.accent, None);
                    p.text(label, *br, &p.fmt.center_small, &p.br.white);
                }
            }
        }
    }

    unsafe { p.rt.PopAxisAlignedClip() };

    if ops.is_empty() {
        p.text(
            "这个插件没有可显示的视图（可能未启用、加载失败，或 get_view 返回空）",
            rect(card.left, card.top + 120.0, card.right - card.left, 20.0),
            &p.fmt.center_small,
            &p.br.muted,
        );
    }
    back
}

/// 下拉值框：视图里 `POp::Select` 与模态里的 select 字段共用同一份几何。
fn select_value_rect(r: D2D_RECT_F) -> D2D_RECT_F {
    rect(r.left + 146.0, r.top, 260.0, 24.0)
}

const SELECT_ITEM_H: f32 = 24.0;
const SELECT_MAX_VISIBLE: usize = 10;

/// 下拉菜单的位置与每一项的矩形（画和命中都用这一份）。
///
/// 返回 `(菜单, 可见项矩形, 起始下标)`：选项多于 [`SELECT_MAX_VISIBLE`] 时只画一屏，
/// `scroll` 决定从第几项开始；下面放不下就翻到值框上面。
fn select_menu_geom(
    anchor: D2D_RECT_F,
    n: usize,
    scroll: usize,
    w: f32,
    h: f32,
) -> (D2D_RECT_F, Vec<D2D_RECT_F>, usize) {
    let mw = (anchor.right - anchor.left).max(200.0);
    let visible = n.clamp(1, SELECT_MAX_VISIBLE);
    let mh = visible as f32 * SELECT_ITEM_H + 8.0;
    let mut x = anchor.left;
    if x + mw > w - 8.0 {
        x = (w - 8.0 - mw).max(8.0);
    }
    let mut y = anchor.bottom + 2.0;
    if y + mh > h - 8.0 {
        y = (anchor.top - 2.0 - mh).max(8.0);
    }
    let menu = rect(x, y, mw, mh);
    let start = scroll.min(n.saturating_sub(visible));
    let mut items = Vec::new();
    for i in 0..visible {
        if start + i >= n {
            break;
        }
        items.push(rect(
            menu.left + 4.0,
            menu.top + 4.0 + i as f32 * SELECT_ITEM_H,
            mw - 8.0,
            SELECT_ITEM_H,
        ));
    }
    (menu, items, start)
}

fn draw_select_menu(p: &P<'_>, w: f32, h: f32, sel: &PvSelect) {
    let (menu, items, start) = select_menu_geom(sel.anchor, sel.options.len(), sel.scroll, w, h);
    p.round(menu, 6.0, &p.br.modal, Some(&p.br.border));
    for (i, ir) in items.iter().enumerate() {
        let Some((val, label)) = sel.options.get(start + i) else {
            break;
        };
        if *val == sel.value {
            p.fill(*ir, &p.br.accent_soft);
        }
        p.text(
            label,
            rect(ir.left + 6.0, ir.top + 3.0, ir.right - ir.left - 12.0, 18.0),
            &p.fmt.small,
            &p.br.fg,
        );
    }
    if start > 0 {
        p.text(
            "▲",
            rect(menu.right - 18.0, menu.top + 2.0, 14.0, 12.0),
            &p.fmt.center_tiny,
            &p.br.muted,
        );
    }
    if start + items.len() < sel.options.len() {
        p.text(
            "▼",
            rect(menu.right - 18.0, menu.bottom - 14.0, 14.0, 12.0),
            &p.fmt.center_tiny,
            &p.br.muted,
        );
    }
}

/// 一个 `modal_form` 的只读视图（借用插件视图里的字段，不复制）。
struct ModalRef<'a> {
    id: &'a str,
    title: &'a str,
    submit: &'a str,
    submit_text: &'a str,
    cancel: &'a str,
    content: &'a str,
    open: bool,
    fields: &'a [focusflow_core::plugins::FormField],
    buttons: &'a [(String, String)],
}

fn modal_from_widget(w: &focusflow_core::plugins::Widget) -> Option<ModalRef<'_>> {
    use focusflow_core::plugins::Widget;
    match w {
        Widget::ModalForm {
            id,
            title,
            submit,
            submit_text,
            cancel,
            content,
            open,
            fields,
            buttons,
            ..
        } => Some(ModalRef {
            id,
            title,
            submit,
            submit_text,
            cancel,
            content,
            open: *open,
            fields,
            buttons,
        }),
        _ => None,
    }
}

/// 当前该显示哪个弹窗：本地点开的优先，其次插件自己 `open=true` 的第一个。
fn find_modal<'a>(
    view: &'a focusflow_core::plugins::PluginView,
    local: Option<&str>,
) -> Option<ModalRef<'a>> {
    let mut first_open = None;
    for w in &view.widgets {
        let Some(m) = modal_from_widget(w) else {
            continue;
        };
        if local == Some(m.id) {
            return Some(m);
        }
        if m.open && first_open.is_none() {
            first_open = Some(m);
        }
    }
    first_open
}

struct ModalGeom {
    dialog: D2D_RECT_F,
    close: D2D_RECT_F,
    content: Option<D2D_RECT_F>,
    /// 每个字段一行（label 在左，值框用 [`modal_field_box`]）。
    fields: Vec<D2D_RECT_F>,
    /// 底部自定义按钮：(矩形, 文字)。
    buttons: Vec<(D2D_RECT_F, String)>,
    submit: D2D_RECT_F,
    cancel: D2D_RECT_F,
}

fn modal_field_box(row: D2D_RECT_F) -> D2D_RECT_F {
    let width = (row.right - row.left - 146.0).min(300.0);
    rect(row.left + 146.0, row.top + 3.0, width, 26.0)
}

fn modal_geom(m: &ModalRef<'_>, w: f32, h: f32) -> ModalGeom {
    let pad = 20.0;
    let dw = (w - 64.0).clamp(380.0, 560.0);
    let lines = if m.content.is_empty() {
        0
    } else {
        m.content.lines().count().min(8)
    };
    let content_h = if lines == 0 {
        0.0
    } else {
        lines as f32 * 18.0 + 10.0
    };
    let fields_h = m.fields.len() as f32 * 38.0;
    let buttons_h = if m.buttons.is_empty() { 0.0 } else { 36.0 };
    let dh = (58.0 + content_h + fields_h + buttons_h + 52.0).min(h - 60.0);
    let left = (w - dw) / 2.0;
    let top = ((h - dh) / 2.0).max(30.0);
    let dialog = rect(left, top, dw, dh);
    let close = rect(dialog.right - pad - 24.0, dialog.top + 14.0, 24.0, 24.0);
    let content = if lines == 0 {
        None
    } else {
        Some(rect(
            dialog.left + pad,
            dialog.top + 52.0,
            dw - pad * 2.0,
            lines as f32 * 18.0,
        ))
    };
    let mut y = dialog.top + 52.0 + content_h;
    let mut fields = Vec::new();
    for _ in m.fields {
        fields.push(rect(dialog.left + pad, y, dw - pad * 2.0, 32.0));
        y += 38.0;
    }
    let mut buttons = Vec::new();
    let mut bx = dialog.left + pad;
    for (_, label) in m.buttons {
        let bw = (label.chars().count() as f32 * 14.0 + 20.0).max(64.0);
        buttons.push((rect(bx, dialog.bottom - 48.0, bw, 28.0), label.clone()));
        bx += bw + 8.0;
    }
    let submit = rect(dialog.right - pad - 88.0, dialog.bottom - 48.0, 88.0, 28.0);
    let cancel = rect(submit.left - 8.0 - 72.0, dialog.bottom - 48.0, 72.0, 28.0);
    ModalGeom {
        dialog,
        close,
        content,
        fields,
        buttons,
        submit,
        cancel,
    }
}

enum ModalHit {
    Close,
    Submit,
    Cancel,
    Field(usize),
    Button(usize),
}

fn modal_hit(m: &ModalRef<'_>, g: &ModalGeom, x: f32, y: f32) -> Option<ModalHit> {
    if hit(g.close, x, y) {
        return Some(ModalHit::Close);
    }
    if hit(g.submit, x, y) {
        return Some(ModalHit::Submit);
    }
    if !m.cancel.is_empty() && hit(g.cancel, x, y) {
        return Some(ModalHit::Cancel);
    }
    for (i, (br, _)) in g.buttons.iter().enumerate() {
        if hit(*br, x, y) {
            return Some(ModalHit::Button(i));
        }
    }
    for (i, row) in g.fields.iter().enumerate() {
        if hit(modal_field_box(*row), x, y) {
            return Some(ModalHit::Field(i));
        }
    }
    None
}

fn draw_plugin_modal(p: &P<'_>, w: f32, h: f32, m: &ModalRef<'_>) {
    p.fill(rect(0.0, 0.0, w, h), &p.br.overlay);
    let g = modal_geom(m, w, h);
    p.round(g.dialog, 16.0, &p.br.modal, Some(&p.br.border));
    p.text(
        m.title,
        rect(
            g.dialog.left + 20.0,
            g.dialog.top + 16.0,
            g.dialog.right - g.dialog.left - 72.0,
            22.0,
        ),
        &p.fmt.body,
        &p.br.fg,
    );
    p.round(g.close, 6.0, &p.br.header, None);
    p.text("✕", g.close, &p.fmt.center_small, &p.br.muted);

    if let Some(cr) = g.content {
        p.text(m.content, cr, &p.fmt.small, &p.br.muted);
    }

    for (i, f) in m.fields.iter().enumerate() {
        let Some(row) = g.fields.get(i) else { break };
        p.text(
            &f.label,
            rect(row.left, row.top + 7.0, 136.0, 18.0),
            &p.fmt.small,
            &p.br.muted,
        );
        let vb = modal_field_box(*row);
        p.round(vb, 6.0, &p.br.card, Some(&p.br.border));
        p.text(
            &f.value,
            rect(vb.left + 8.0, vb.top + 3.0, vb.right - vb.left - 28.0, 18.0),
            &p.fmt.small,
            &p.br.fg,
        );
        if f.kind == "select" {
            p.text(
                "▾",
                rect(vb.right - 20.0, vb.top + 3.0, 16.0, 18.0),
                &p.fmt.center_small,
                &p.br.muted,
            );
        }
    }

    for (br, label) in &g.buttons {
        p.round(*br, 6.0, &p.br.header, Some(&p.br.border));
        p.text(label, *br, &p.fmt.center_small, &p.br.fg);
    }
    if !m.cancel.is_empty() {
        p.round(g.cancel, 6.0, &p.br.header, Some(&p.br.border));
        p.text("取消", g.cancel, &p.fmt.center_small, &p.br.fg);
    }
    p.round(g.submit, 6.0, &p.br.accent, None);
    p.text(
        if m.submit_text.is_empty() {
            "确定"
        } else {
            m.submit_text
        },
        g.submit,
        &p.fmt.center_small,
        &p.br.white,
    );
}

// ===== 插件 =====

fn plugin_geom(_w: f32, h: f32) -> (f32, f32, usize) {
    let list_top = 208.0 + 44.0;
    let row_h = 46.0;
    let max_rows = ((((h - 32.0) - list_top) / row_h).floor() as usize).max(1);
    (list_top, row_h, max_rows)
}

/// 插件错误串里常带 `\nstack traceback:…`：只留第一行并截断，否则会把下一行顶穿。
fn one_line(s: &str, max: usize) -> String {
    let first = s.lines().next().unwrap_or("").trim();
    let mut out: String = first.chars().take(max).collect();
    if first.chars().count() > max {
        out.push('…');
    }
    out
}

fn draw_plugins(p: &P<'_>, w: f32, h: f32, rows: &[PluginRow], err: Option<&str>, vs: ViewState) {
    let card = rect(16.0, 208.0, w - 32.0, h - 208.0 - 16.0);
    p.round(card, 14.0, &p.br.card, Some(&p.br.border));
    p.text(
        "插件",
        rect(card.left + 16.0, card.top + 12.0, 80.0, 22.0),
        &p.fmt.body,
        &p.br.fg,
    );
    p.text(
        &format!(
            "{} 个 · 目录 {}",
            rows.len(),
            focusflow_core::paths::plugins_dir().display()
        ),
        rect(card.left + 58.0, card.top + 15.0, 520.0, 18.0),
        &p.fmt.small,
        &p.br.muted,
    );

    if let Some(e) = err {
        p.text(
            &format!("插件目录读不出来：{e}"),
            rect(
                card.left + 16.0,
                card.top + 36.0,
                card.right - card.left - 32.0,
                18.0,
            ),
            &p.fmt.small,
            &p.br.danger,
        );
        return;
    }

    let (list_top, row_h, max_rows) = plugin_geom(w, h);
    let scroll = vs.scroll.min(rows.len().saturating_sub(max_rows));
    for vis in 0..max_rows {
        let idx = scroll + vis;
        if idx >= rows.len() {
            break;
        }
        let row = &rows[idx];
        let y = list_top + vis as f32 * row_h;
        let r = rect(
            card.left + 16.0,
            y,
            card.right - card.left - 32.0,
            row_h - 4.0,
        );
        if vs.hover == Some(idx) {
            p.fill(r, &p.br.header);
        } else if vs.sel == Some(idx) {
            p.round(r, 6.0, &p.br.accent_soft, None);
        }
        p.text(
            &row.name,
            rect(r.left + 8.0, y + 5.0, 240.0, 18.0),
            &p.fmt.body,
            &p.br.fg,
        );
        p.text(
            &format!("v{} · {}", row.version, row.author),
            rect(r.left + 250.0, y + 7.0, 260.0, 16.0),
            &p.fmt.small,
            &p.br.muted,
        );
        // 启用开关（只画；命中在 on_click 里用同一份几何）
        let vb = rect(r.right - 76.0, y + 9.0, 60.0, 22.0);
        p.round(
            vb,
            6.0,
            if row.enabled {
                &p.br.success
            } else {
                &p.br.grid
            },
            None,
        );
        p.text(
            if row.enabled { "开" } else { "关" },
            vb,
            &p.fmt.center_small,
            if row.enabled {
                &p.br.white
            } else {
                &p.br.muted
            },
        );
        // 第二行：加载失败说失败原因，否则说它是什。
        match &row.error {
            Some(e) => {
                p.text(
                    &format!("加载失败：{}", one_line(e, 72)),
                    rect(r.left + 8.0, y + 23.0, r.right - r.left - 92.0, 16.0),
                    &p.fmt.small,
                    &p.br.danger,
                );
            }
            None => {
                // 启用了却没加载上（还没轮到、或加载被静默跳过）要说出来。
                let text = if row.enabled && !row.loaded {
                    format!("{}（已启用，尚未加载）", row.desc)
                } else {
                    row.desc.clone()
                };
                p.text(
                    &text,
                    rect(r.left + 8.0, y + 23.0, r.right - r.left - 92.0, 16.0),
                    &p.fmt.small,
                    &p.br.muted,
                );
            }
        }
    }

    if rows.is_empty() {
        p.text(
            "插件目录是空的：把一个 .lua 放进 plugins/ 就会出现在这里",
            rect(card.left, card.top + 120.0, card.right - card.left, 20.0),
            &p.fmt.center_small,
            &p.br.muted,
        );
    }
}

fn draw_placeholder(p: &P<'_>, w: f32, h: f32, view: usize) {
    let m = 16.0;
    let text = match view {
        5 => "设置",
        _ => "待迁移",
    };
    draw_placeholder_card(p, rect(m, 208.0, w - m * 2.0, h - 208.0 - m), text);
}

fn draw_placeholder_card(p: &P<'_>, r: D2D_RECT_F, text: &str) {
    p.round(r, 14.0, &p.br.card, Some(&p.br.border));
    p.text(
        text,
        rect(r.left, r.top + 120.0, r.right - r.left, 24.0),
        &p.fmt.center_small,
        &p.br.muted,
    );
}

/// 左表的几何：画图与命中测试共用同一份，避免两处各算一套坐标。
struct TableGeom {
    pad: f32,
    head_y: f32,
    list_top: f32,
    row_h: f32,
    max_rows: usize,
    x_rank: f32,
    x_name: f32,
    x_count: f32,
    x_last: f32,
}

/// 排行页标题行里那两排胶囊。下标 0..1 = 视图（排行 / 分组），2..4 = 筛选（全部 / 键盘 / 鼠标）。
/// 「分组」那一屏不读筛选，那时后面三格既不画也不接 —— 与画图共用这一份位置，
/// 照的是 `view_tabs` / `atab_tabs` 那个做法。
fn rank_pills(card: D2D_RECT_F) -> [D2D_RECT_F; 5] {
    let y = card.top + 8.0;
    // 322 而不是 306：标题行左边那句摘要（共 N 次 · 其中 鼠标 … · 键盘 …）画到 card.left+306 为止，
    // 胶囊原来正好贴在其右沿上，看着像同一串字。留出 16px 才分得开"读数"与"按钮"。
    let x0 = card.left + 322.0;
    let xs = [32.0, 90.0, 188.0, 246.0, 304.0];
    let mut out = [rect(0.0, 0.0, 0.0, 0.0); 5];
    for (i, dx) in xs.iter().enumerate() {
        out[i] = rect(x0 + dx, y, 56.0, 22.0);
    }
    out
}

/// 两排胶囊前那两句小字（视图 / 筛选），原点与 `rank_pills` 同一份。
fn rank_pill_captions(card: D2D_RECT_F) -> (D2D_RECT_F, D2D_RECT_F) {
    let y = card.top + 11.0;
    let x0 = card.left + 322.0;
    (rect(x0, y, 30.0, 16.0), rect(x0 + 148.0, y, 30.0, 16.0))
}

/// 左表这一屏该显示哪几行，交回的是 `snap` 里那份榜的下标。
///
/// 屏上行号 → 榜里下标只许有这一条路：画图、悬停、点击三处都读它。两边各筛一遍就会
/// 出现"点第 3 行、选中第 5 条"。应用榜与设备榜没有筛选，交回恒等下标。
fn left_rows(snap: &Snapshot, view: usize, rank_tab: usize, rank_filter: usize) -> Vec<usize> {
    if view != 0 {
        let n = match view {
            1 => snap.apps.len(),
            2 => snap.devices.len(),
            _ => 0,
        };
        return (0..n).collect();
    }
    if rank_tab == 1 {
        return (0..snap.group.len()).collect();
    }
    snap.rank
        .iter()
        .enumerate()
        .filter(|(_, (k, _))| match rank_filter {
            1 => !data::rank_is_mouse(k),
            2 => data::rank_is_mouse(k),
            _ => true,
        })
        .map(|(i, _)| i)
        .collect()
}

fn table_geom(r: D2D_RECT_F) -> TableGeom {
    let pad = 12.0;
    let row_h = 24.0;
    let head_y = r.top + 42.0;
    let list_top = head_y + 22.0;
    let foot_h = 26.0;
    let max_rows = (((r.bottom - foot_h - list_top) / row_h).floor() as usize).max(1);
    TableGeom {
        pad,
        head_y,
        list_top,
        row_h,
        max_rows,
        x_rank: r.left + pad + 10.0,
        x_name: r.left + pad + 56.0,
        x_count: r.right - pad - 168.0,
        x_last: r.right - pad - 78.0,
    }
}

fn rename_button(g: &TableGeom, y: f32) -> D2D_RECT_F {
    rect(g.x_last, y + 3.0, 58.0, 18.0)
}

fn restore_button(g: &TableGeom, y: f32) -> D2D_RECT_F {
    rect(g.x_last - 64.0, y + 3.0, 58.0, 18.0)
}

/// 应用时长：秒 。
/// 生成「上周」周报（周一~周日），返回 Markdown 路径。旧 WebView 版在每周一自动生成，
/// 原生端先做成设置页「目标 → 上周周报」里的一颗手动按钮，内容口径与旧版一致。
/// 上一个完整周的周一（周报覆盖它到 +6 天）。
fn last_week_start(today: chrono::NaiveDate) -> chrono::NaiveDate {
    use chrono::Datelike;
    let since_monday = today.weekday().num_days_from_monday() as i64;
    today - chrono::Duration::days(since_monday + 7)
}

/// 上周那份周报该落在哪：`<data>/reports/weekly-<上周一>.md`。
/// 手动生成与"启动后要不要补一份"都读这一个地址，日期算法只有一份。
fn weekly_report_path(today: chrono::NaiveDate) -> std::path::PathBuf {
    focusflow_core::paths::data_dir()
        .join("reports")
        .join(format!(
            "weekly-{}.md",
            last_week_start(today).format("%Y-%m-%d")
        ))
}

/// 启动后补一份上周周报（旧版口径：每周一 / 启动时生成上一个完整周）。
/// 判据只有"该在的那一份在不在"：在就一个字都不动，缺才生成，且生成走的是设置页
/// 那颗按钮同一条路。日志里的 `weekly-report` 是 ASCII 标记，给验收脚本用。
fn ensure_weekly_report() {
    let today = chrono::Local::now().date_naive();
    let path = weekly_report_path(today);
    if path.exists() {
        tracing_note(&format!("weekly-report exists {}", path.display()));
        return;
    }
    match generate_weekly_report() {
        Ok(p) => tracing_note(&format!("weekly-report auto {}", p.display())),
        Err(e) => tracing_note(&format!("weekly-report failed：{e}")),
    }
}

fn generate_weekly_report() -> std::result::Result<std::path::PathBuf, String> {
    use chrono::{Duration, Local};
    use focusflow_core::db::queries;
    use std::fmt::Write as _;

    let today = Local::now().date_naive();
    let start = last_week_start(today);
    let end = start + Duration::days(6);
    let goal = focusflow_core::config::instance().get_int("goal", "daily_keys", 20000);

    let mut by_day: Vec<(chrono::NaiveDate, i64)> = Vec::new();
    let mut keys: HashMap<String, i64> = HashMap::new();
    let mut apps: HashMap<String, i64> = HashMap::new();
    let mut devices: HashMap<String, i64> = HashMap::new();
    let mut gaps: Vec<String> = Vec::new();
    let mut total = 0i64;

    for i in 0..7 {
        let d = start + Duration::days(i);
        let t = queries::stats_totals_on(d);
        total += t.total;
        by_day.push((d, t.total));
        gaps.extend(t.unreadable);
        for (k, v) in t.rows {
            *keys.entry(k).or_insert(0) += v;
        }
        let a = queries::app_stats_totals_on(d);
        gaps.extend(a.unreadable);
        for (k, v) in a.rows {
            *apps.entry(k).or_insert(0) += v;
        }
        let dev = queries::device_totals_on(d);
        gaps.extend(dev.unreadable);
        for dstat in dev.devices {
            *devices.entry(dstat.name).or_insert(0) += dstat.count;
        }
    }
    gaps.sort();
    gaps.dedup();

    let mut keys: Vec<(String, i64)> = keys.into_iter().collect();
    keys.sort_by(|a, b| focusflow_core::format::cmp_rank_desc(&a.0, &a.1, &b.0, &b.1));
    keys.truncate(10);
    let mut apps: Vec<(String, i64)> = apps.into_iter().collect();
    apps.sort_by(|a, b| focusflow_core::format::cmp_rank_desc(&a.0, &a.1, &b.0, &b.1));
    apps.truncate(10);
    let mut devices: Vec<(String, i64)> = devices.into_iter().collect();
    devices.sort_by(|a, b| focusflow_core::format::cmp_rank_desc(&a.0, &a.1, &b.0, &b.1));
    devices.truncate(10);

    let max_day = by_day
        .iter()
        .max_by_key(|(_, v)| *v)
        .map(|(d, v)| (d.format("%m-%d").to_string(), *v))
        .unwrap_or_else(|| ("—".to_string(), 0));
    let hit = by_day.iter().filter(|(_, v)| *v >= goal).count();
    let device_total: i64 = devices.iter().map(|(_, v)| *v).sum();

    let mut out = String::new();
    let _ = writeln!(
        out,
        "# FocusFlow 周报 · {} ~ {}\n",
        start.format("%Y-%m-%d"),
        end.format("%Y-%m-%d")
    );
    let _ = writeln!(out, "- 总活跃：{} 次", thousands(total.max(0) as u64));
    let _ = writeln!(out, "- 日均：{} 次", thousands((total / 7).max(0) as u64));
    let _ = writeln!(
        out,
        "- 最高单日：{} 次（{}）",
        thousands(max_day.1.max(0) as u64),
        max_day.0
    );
    let _ = writeln!(
        out,
        "- 达标天数：{} / 7（每日目标 {} 次）",
        hit,
        thousands(goal.max(0) as u64)
    );
    if device_total > 0 {
        let _ = writeln!(
            out,
            "- 设备输入合计：{} 次（独立口径，和上面的键鼠次数不直接相加）",
            thousands(device_total.max(0) as u64)
        );
    }
    if !gaps.is_empty() {
        let _ = writeln!(
            out,
            "\n> 有 {} 个年度库没读全：{}",
            gaps.len(),
            gaps.join("；")
        );
    }

    let _ = writeln!(out, "\n## 每日活跃\n\n| 日期 | 次数 |\n|---|---:|");
    for (d, n) in &by_day {
        let mark = if *n >= goal { " ✅" } else { "" };
        let _ = writeln!(
            out,
            "| {} | {}{} |",
            d.format("%Y-%m-%d"),
            thousands((*n).max(0) as u64),
            mark
        );
    }

    let _ = writeln!(out, "\n## 键鼠 TOP 10\n\n| 键 | 次数 |\n|---|---:|");
    for (k, n) in &keys {
        let _ = writeln!(out, "| {} | {} |", k, thousands((*n).max(0) as u64));
    }
    if keys.is_empty() {
        let _ = writeln!(out, "| （这一周没有记录） | 0 |");
    }

    let _ = writeln!(
        out,
        "\n## 应用 TOP 10（前台时长）\n\n| 应用 | 时长 |\n|---|---:|"
    );
    for (name, secs) in &apps {
        let _ = writeln!(out, "| {} | {} |", name, duration_text(*secs));
    }
    if apps.is_empty() {
        let _ = writeln!(out, "| （这一周没有记录） | 0s |");
    }

    let _ = writeln!(out, "\n## 设备 TOP 10\n\n| 设备 | 次数 |\n|---|---:|");
    for (name, n) in &devices {
        let _ = writeln!(out, "| {} | {} |", name, thousands((*n).max(0) as u64));
    }
    if devices.is_empty() {
        let _ = writeln!(out, "| （这一周没有记录） | 0 |");
    }

    let _ = writeln!(
        out,
        "\n---\n\n生成时间：{} · 数据只来自本机，未上传。\n",
        Local::now().format("%Y-%m-%d %H:%M:%S")
    );

    let file = weekly_report_path(today);
    match file.parent() {
        Some(dir) => std::fs::create_dir_all(dir).map_err(|e| format!("建报告目录失败：{e}"))?,
        None => return Err("周报路径没有父目录".to_string()),
    }
    write_user_text(&file, &out).map_err(|e| format!("写周报失败：{e}"))?;
    Ok(file)
}

/// 把一份文本落到**用户挑的路径**上：走 core 那份原子替换（同目录临时文件 + rename），
/// 所以目标上原来那一份要么整份被换掉、要么原样留着 —— 直接写会先把旧内容截断，
/// 中途失败（盘满、权限变了）就只剩一个半截文件，而旧的已经没了。
/// 重写成原生的时候这两条路掉回了直接写，`user_file_write_tests` 把它钉住。
fn write_user_text(path: &std::path::Path, body: &str) -> std::result::Result<(), String> {
    focusflow_core::config::atomic_write(path, body).map_err(|e| format!("{e}"))
}

fn duration_text(secs: i64) -> String {
    let s = secs.max(0);
    if s >= 3600 {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

/// CSV 字段转义：逗号/引号/换行都用标准双引号包法。
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// 转义 HTML 文本。
/// 注：CSV 侧的转义是另一套规则，见 `csv_field`。
/// HTML 报告里要转义：键名/应用名可能带 `<`、`&`。
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 自包含的 HTML 报告：暗色、无外部依赖，双击就能看。
fn export_html_text(s: &Snapshot) -> String {
    let mut out = String::new();
    out.push_str("<!doctype html><meta charset=\"utf-8\"><title>FocusFlow 导出</title>\n");
    out.push_str("<style>body{font:14px/1.6 \"Microsoft YaHei UI\",sans-serif;background:#0f1219;color:#e8ecf4;padding:24px;max-width:900px;margin:0 auto}h1{font-size:20px}h2{font-size:15px;margin-top:26px;color:#9aa4b8}table{border-collapse:collapse;width:100%;margin-top:8px}th,td{border-bottom:1px solid #2e3548;padding:6px 8px;text-align:left;font-size:13px}th{color:#9aa4b8;font-weight:500}td.num{text-align:right}</style>\n");
    out.push_str(&format!(
        "<h1>FocusFlow 导出</h1><p>生成时间 {} · 统计周期 {} · 时间切面 {}</p>\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        html_escape(&period_text(s.period)),
        html_escape(&data::window_text(s.period))
    ));
    let gap_say = gap_line(&s.gaps);
    if !gap_say.is_empty() {
        out.push_str(&format!("<p>{}</p>\n", html_escape(&gap_say)));
    }
    out.push_str("<table><tr><th>今日</th><th>周期总计</th><th>日均</th></tr>");
    out.push_str(&format!(
        "<tr><td>{}</td><td>{}</td><td>{}</td></tr></table>\n",
        s.today, s.period_total, s.avg
    ));

    out.push_str(
        "<h2>键鼠排行</h2><table><tr><th>排名</th><th>键名</th><th>次数</th><th>占比</th></tr>",
    );
    for (i, (k, v)) in s.rank.iter().enumerate() {
        let share = if s.period_total > 0 {
            *v as f32 * 100.0 / s.period_total as f32
        } else {
            0.0
        };
        out.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{share:.1}%</td></tr>",
            i + 1,
            html_escape(k),
            v
        ));
    }
    out.push_str("</table>\n");

    out.push_str("<h2>应用排行</h2><table><tr><th>排名</th><th>应用</th><th>时长</th></tr>");
    for (i, (k, v)) in s.apps.iter().enumerate() {
        out.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
            i + 1,
            html_escape(k),
            duration_text(*v)
        ));
    }
    out.push_str("</table>\n");

    out.push_str(
        "<h2>设备排行</h2><table><tr><th>排名</th><th>设备</th><th>类型</th><th>次数</th></tr>",
    );
    for (i, d) in s.devices.iter().enumerate() {
        out.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td class=\"num\">{}</td></tr>",
            i + 1,
            html_escape(&d.name),
            html_escape(&d.kind),
            d.count
        ));
    }
    out.push_str("</table>\n");

    out.push_str(&format!(
        "<h2>每日次数（{}）</h2><table><tr><th>日期</th><th>次数</th></tr>",
        data::window_text(s.period)
    ));
    for (d, v) in &s.daily {
        out.push_str(&format!(
            "<tr><td>{}</td><td class=\"num\">{v}</td></tr>",
            html_escape(d)
        ));
    }
    out.push_str("</table>\n");

    out.push_str("<h2>每小时合计</h2><table><tr><th>小时</th><th>次数</th></tr>");
    for (h, v) in s.hourly.iter().enumerate() {
        out.push_str(&format!(
            "<tr><td>{h}:00</td><td class=\"num\">{v}</td></tr>"
        ));
    }
    out.push_str("</table>\n");
    out
}

/// 结构化 JSON：脚本/其它工具可直接读。
fn export_json_text(s: &Snapshot) -> String {
    use serde_json::json;
    let rank: Vec<serde_json::Value> = s
        .rank
        .iter()
        .enumerate()
        .map(|(i, (k, v))| json!({"rank": i + 1, "key": k, "count": v}))
        .collect();
    let apps: Vec<serde_json::Value> = s
        .apps
        .iter()
        .enumerate()
        .map(|(i, (k, v))| json!({"rank": i + 1, "app": k, "seconds": v}))
        .collect();
    let devices: Vec<serde_json::Value> = s
        .devices
        .iter()
        .enumerate()
        .map(|(i, d)| json!({"rank": i + 1, "name": d.name, "kind": d.kind, "count": d.count}))
        .collect();
    let daily: Vec<serde_json::Value> = s
        .daily
        .iter()
        .map(|(d, v)| json!({"date": d, "count": v}))
        .collect();
    let hourly: Vec<serde_json::Value> = s
        .hourly
        .iter()
        .enumerate()
        .map(|(h, v)| json!({"hour": h, "count": v}))
        .collect();
    let doc = json!({
        "generated_at": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        "period": period_text(s.period),
        // `daily` 与 `hourly` 数的是哪一段：与屏上三张图同一句（`period` 那一项对"总计"
        // 说的是「全部」，按到这里会把 30 天的窗口夸成全部历史）。
        "chart_window": data::window_text(s.period),
        // 交回的总数在下界这一件事必须跟着文件走：读的人拿不到"哪一年没答上来"，
        // 就会把一份残缺的榜当成"那一年真的一个键都没按"。
        "read_gaps": s.gaps,
        "today": s.today,
        "period_total": s.period_total,
        "avg": s.avg,
        "rank": rank,
        "apps": apps,
        "devices": devices,
        "daily": daily,
        "hourly": hourly,
    });
    serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
}

/// 把当前快照写成一份 CSV（概览 + 四个排行 + 每日/每小时）。
fn export_csv_text(s: &Snapshot) -> String {
    let mut out = String::new();
    out.push_str("FocusFlow 导出\r\n");
    out.push_str(&format!(
        "生成时间,{}\r\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    ));
    out.push_str(&format!("统计周期,{}\r\n", period_text(s.period)));
    out.push_str(&format!("时间切面,{}\r\n", data::window_text(s.period)));
    let gap_say = gap_line(&s.gaps);
    if !gap_say.is_empty() {
        out.push_str(&format!("读数完整性,{}\r\n", csv_field(&gap_say)));
    }
    out.push_str(&format!("今日次数,{}\r\n", s.today));
    out.push_str(&format!("周期总计,{}\r\n", s.period_total));
    out.push_str(&format!("日均,{}\r\n", s.avg));

    out.push_str("\r\n[键鼠排行]\r\n排名,键名,次数,占比\r\n");
    for (i, (k, v)) in s.rank.iter().enumerate() {
        let share = if s.period_total > 0 {
            *v as f32 * 100.0 / s.period_total as f32
        } else {
            0.0
        };
        out.push_str(&format!("{},{},{},{share:.1}%\r\n", i + 1, csv_field(k), v));
    }

    out.push_str("\r\n[应用排行]\r\n排名,应用,时长(秒),时长\r\n");
    for (i, (k, v)) in s.apps.iter().enumerate() {
        out.push_str(&format!(
            "{},{},{},{}\r\n",
            i + 1,
            csv_field(k),
            v,
            duration_text(*v)
        ));
    }

    out.push_str("\r\n[设备排行]\r\n排名,设备,类型,次数,占比\r\n");
    for (i, d) in s.devices.iter().enumerate() {
        let share = if s.period_total > 0 {
            d.count as f32 * 100.0 / s.period_total as f32
        } else {
            0.0
        };
        out.push_str(&format!(
            "{},{},{},{},{share:.1}%\r\n",
            i + 1,
            csv_field(&d.name),
            csv_field(&d.kind),
            d.count
        ));
    }

    out.push_str(&format!(
        "\r\n[每日次数({})]\r\n日期,次数\r\n",
        data::window_text(s.period)
    ));
    for (d, v) in &s.daily {
        out.push_str(&format!("{},{}\r\n", d, v));
    }

    out.push_str("\r\n[每小时合计]\r\n小时,次数\r\n");
    for (h, v) in s.hourly.iter().enumerate() {
        out.push_str(&format!("{h},{v}\r\n"));
    }
    out
}

fn draw_table(p: &P<'_>, r: D2D_RECT_F, snap: &Snapshot, vs: ViewState) {
    p.round(r, 14.0, &p.br.card, Some(&p.br.border));
    let g = table_geom(r);
    let tab = vs.view.min(2);
    // 这一屏该显示哪几行（键鼠那一屏会随「分组」与筛选换内容），命中测试读的是同一份。
    let rows = left_rows(snap, tab, vs.rank_tab, vs.rank_filter);
    let (title, total, count) = match tab {
        1 => ("应用排行", snap.app_total, rows.len()),
        2 => (
            "设备排行",
            snap.devices.iter().map(|d| d.count).sum(),
            rows.len(),
        ),
        _ => ("键鼠排行", snap.period_total, rows.len()),
    };
    // 滚动位置按当前页签的行数夹一次：数据变少时别停在空白处。
    let scroll = vs.scroll.min(count.saturating_sub(g.max_rows));

    p.text(
        title,
        rect(r.left + g.pad, r.top + 10.0, 120.0, 20.0),
        &p.fmt.body,
        &p.br.fg,
    );
    let lane = match tab {
        1 => queries::DOMAIN_APPS,
        2 => queries::DOMAIN_DEVICES,
        _ => queries::DOMAIN_KEYS,
    };
    let mut summary = if tab == 1 {
        format!("共 {}", duration_text(total))
    } else {
        format!("共 {} 次", thousands(total.max(0) as u64))
    };
    // 键鼠那一屏多两个数：鼠标侧与键盘侧。写「其中」而不是把它们并成总数 ——
    // 未落库的那截（`pending`）还没归到任何一侧，折进键盘侧就是把没证的数报成实测。
    if tab == 0 && !snap.rank.is_empty() {
        summary.push_str(&format!(
            " · 其中 鼠标 {} · 键盘 {}",
            thousands(snap.mouse_total.max(0) as u64),
            thousands(snap.keyboard_total.max(0) as u64)
        ));
    }
    // 本栏那句"没读全"不写在这里：标题行这一格只有 220px，一条年度库的缺读理由就把它撑满，
    // 挤掉的是刚补的鼠标/键盘两个数。挪到同一张卡底下那条页脚去说（它宽得多，而且句子短了）。
    p.text(
        &summary,
        rect(r.left + g.pad + 74.0, r.top + 12.0, 220.0, 18.0),
        &p.fmt.small,
        &p.br.muted,
    );
    if tab == 0 {
        let pills = rank_pills(r);
        let (cap1, cap2) = rank_pill_captions(r);
        p.text("视图", cap1, &p.fmt.small, &p.br.muted);
        for (i, label) in ["排行", "分组"].iter().enumerate() {
            let active = vs.rank_tab == i;
            p.round(
                pills[i],
                6.0,
                if active { &p.br.accent } else { &p.br.card },
                if active { None } else { Some(&p.br.border) },
            );
            p.text(
                label,
                pills[i],
                &p.fmt.center_small,
                if active { &p.br.white } else { &p.br.muted },
            );
        }
        // 分组那一屏读的是整份分组榜，筛选对它没有意义 ⇒ 连格子一起撤掉（旧版同做法）。
        if vs.rank_tab == 0 {
            p.text("筛选", cap2, &p.fmt.small, &p.br.muted);
            for (i, label) in ["全部", "键盘", "鼠标"].iter().enumerate() {
                let active = vs.rank_filter == i;
                p.round(
                    pills[i + 2],
                    6.0,
                    if active { &p.br.accent } else { &p.br.card },
                    if active { None } else { Some(&p.br.border) },
                );
                p.text(
                    label,
                    pills[i + 2],
                    &p.fmt.center_small,
                    if active { &p.br.white } else { &p.br.muted },
                );
            }
        }
    }

    // 表头
    p.fill(
        rect(
            r.left + g.pad,
            g.head_y,
            r.right - r.left - g.pad * 2.0,
            22.0,
        ),
        &p.br.header,
    );
    let heads: [&str; 4] = match (tab, vs.rank_tab) {
        (1, _) => ["排名", "应用", "时长", "占比"],
        (2, _) => ["排名", "设备名", "次数", "操作"],
        (0, 1) => ["序", "分组", "次数", "占比"],
        _ => ["排名", "键名", "次数", "占比"],
    };
    let xs = [g.x_rank, g.x_name, g.x_count, g.x_last];
    for (i, t) in heads.iter().enumerate() {
        p.text(
            t,
            rect(xs[i], g.head_y + 3.0, 90.0, 18.0),
            &p.fmt.small,
            &p.br.muted,
        );
    }

    if count == 0 {
        // 采集开着时列表会先空一段：排行读的是**已落库**的聚合表，而写线程按
        // `[database] flush_interval`（默认 10 秒）才落一次。这不是"没记上"，
        // 所以这里要说清楚还差哪一步 —— 不然"今日 5 次 + 空列表"看着像 bug。
        let pending = collect::pending_today();
        let empty = match tab {
            1 => "今日还没有应用时长（[app_stats] enabled 关着？）".to_string(),
            2 => "今日还没有设备计数（[device_stats] enabled 关着？）".to_string(),
            _ if pending > 0 => format!(
                "已记录 {} 条，还没落库（写线程每 {} 秒写一次，落库后就出现在这里）",
                thousands(pending as u64),
                focusflow_core::config::instance()
                    .get_int("database", "flush_interval", 10)
                    .max(1)
            ),
            _ => "今日还没有记录（数据目录见左下角）".to_string(),
        };
        p.text(
            &empty,
            rect(
                r.left + g.pad + 10.0,
                g.list_top + 8.0,
                r.right - r.left - g.pad * 2.0,
                20.0,
            ),
            &p.fmt.small,
            &p.br.muted,
        );
    }

    for vis in 0..g.max_rows {
        let shown = scroll + vis;
        if shown >= count {
            break;
        }
        // `shown` 是屏上行号（选中、悬停、序号都按它），`idx` 是榜里的下标（筛选会跳过几条）。
        let idx = rows[shown];
        let y = g.list_top + vis as f32 * g.row_h;
        let row = rect(
            r.left + g.pad,
            y,
            r.right - r.left - g.pad * 2.0,
            g.row_h - 1.0,
        );
        if vs.sel == Some(shown) {
            p.round(row, 4.0, &p.br.accent_soft, None);
        } else if vs.hover == Some(shown) || vis % 2 == 1 {
            p.fill(row, &p.br.header);
        }
        p.text(
            &format!("{}", shown + 1),
            rect(g.x_rank, y + 4.0, 40.0, 18.0),
            &p.fmt.small,
            if shown < 3 { &p.br.accent } else { &p.br.muted },
        );

        match tab {
            2 => {
                let d = &snap.devices[idx];
                if vs.editing == Some(idx) {
                    // 编辑中：这一格由原生 EDIT 盖着，这里只画一圈边框。
                    p.round(
                        rect(
                            g.x_name - 4.0,
                            y + 1.0,
                            g.x_count - g.x_name - 52.0,
                            g.row_h - 3.0,
                        ),
                        4.0,
                        &p.br.card,
                        Some(&p.br.accent),
                    );
                } else {
                    p.text(
                        &d.name,
                        rect(g.x_name, y + 3.0, g.x_count - g.x_name - 60.0, 18.0),
                        &p.fmt.body,
                        &p.br.fg,
                    );
                }
                let kind = match d.kind.as_str() {
                    "mouse" => "鼠标",
                    "keyboard" => "键盘",
                    "hybrid" => "键鼠",
                    _ => "未知",
                };
                p.text(
                    kind,
                    rect(g.x_count - 54.0, y + 5.0, 48.0, 16.0),
                    &p.fmt.small,
                    &p.br.muted,
                );
                p.text(
                    &thousands(d.count.max(0) as u64),
                    rect(g.x_count, y + 4.0, 70.0, 18.0),
                    &p.fmt.small,
                    &p.br.fg,
                );
                if d.has_alias {
                    let rb = restore_button(&g, y);
                    p.round(rb, 4.0, &p.br.accent_soft, None);
                    p.text("还原", rb, &p.fmt.center_small, &p.br.muted);
                }
                let btn = rename_button(&g, y);
                let hot = vs.hover == Some(idx) || vs.sel == Some(idx);
                p.round(
                    btn,
                    4.0,
                    if hot { &p.br.accent } else { &p.br.accent_soft },
                    None,
                );
                p.text(
                    "改名",
                    btn,
                    &p.fmt.center_small,
                    if hot { &p.br.white } else { &p.br.accent },
                );
            }
            1 => {
                let (app, secs) = &snap.apps[idx];
                let share = if snap.app_total > 0 {
                    *secs as f32 * 100.0 / snap.app_total as f32
                } else {
                    0.0
                };
                p.text(
                    app,
                    rect(g.x_name, y + 3.0, g.x_count - g.x_name - 12.0, 18.0),
                    &p.fmt.body,
                    &p.br.fg,
                );
                p.text(
                    &duration_text(*secs),
                    rect(g.x_count, y + 4.0, 80.0, 18.0),
                    &p.fmt.small,
                    &p.br.fg,
                );
                p.text(
                    &format!("{share:.1}%"),
                    rect(g.x_last, y + 4.0, 60.0, 18.0),
                    &p.fmt.small,
                    &p.br.muted,
                );
            }
            _ if vs.rank_tab == 1 => {
                // 「分组」那一屏：八组的固定顺序，占比按这一轮的总数。
                let (name, n) = &snap.group[idx];
                let share = if snap.period_total > 0 {
                    *n as f32 * 100.0 / snap.period_total as f32
                } else {
                    0.0
                };
                p.text(
                    name,
                    rect(g.x_name, y + 3.0, 300.0, 18.0),
                    &p.fmt.body,
                    &p.br.fg,
                );
                p.text(
                    &thousands(*n as u64),
                    rect(g.x_count, y + 4.0, 80.0, 18.0),
                    &p.fmt.small,
                    &p.br.fg,
                );
                p.text(
                    &format!("{share:.1}%"),
                    rect(g.x_last, y + 4.0, 60.0, 18.0),
                    &p.fmt.small,
                    &p.br.muted,
                );
            }
            _ => {
                let (key, n) = &snap.rank[idx];
                let share = if snap.period_total > 0 {
                    *n as f32 * 100.0 / snap.period_total as f32
                } else {
                    0.0
                };
                p.text(
                    key,
                    rect(g.x_name, y + 3.0, 300.0, 18.0),
                    &p.fmt.body,
                    &p.br.fg,
                );
                p.text(
                    &thousands(*n as u64),
                    rect(g.x_count, y + 4.0, 80.0, 18.0),
                    &p.fmt.small,
                    &p.br.fg,
                );
                p.text(
                    &format!("{share:.1}%"),
                    rect(g.x_last, y + 4.0, 60.0, 18.0),
                    &p.fmt.small,
                    &p.br.muted,
                );
            }
        }
    }

    // 滚动条：只有装不下时才画。拖动没做，滚轮与点击空白处先够用。
    if count > g.max_rows {
        let track = rect(r.right - 9.0, g.list_top, 4.0, g.max_rows as f32 * g.row_h);
        p.round(track, 2.0, &p.br.header, None);
        let frac = g.max_rows as f32 / count as f32;
        let thumb_h = ((track.bottom - track.top) * frac).max(16.0);
        let max_scroll = (count - g.max_rows) as f32;
        let t = if max_scroll > 0.0 {
            scroll as f32 / max_scroll
        } else {
            0.0
        };
        let y = track.top + ((track.bottom - track.top) - thumb_h) * t;
        p.round(rect(track.left, y, 4.0, thumb_h), 2.0, &p.br.accent, None);
    }

    let foot_y = r.bottom - 26.0 + 4.0;
    p.line(
        r.left + g.pad,
        foot_y - 6.0,
        r.right - g.pad,
        foot_y - 6.0,
        &p.br.border,
        1.0,
    );
    p.text(
        &footer_line(&snap.data_home, &snap.gaps, lane),
        rect(r.left + g.pad, foot_y, r.right - r.left - g.pad * 2.0, 16.0),
        &p.fmt.small,
        &p.br.muted,
    );
}

/// 分析页那张图卡：画图与命中测试算的是同一个矩形，两边不许各摆一份数字。
fn chart_card(content: D2D_RECT_F) -> D2D_RECT_F {
    rect(
        content.left,
        content.top,
        content.right - content.left,
        300.0,
    )
}

/// 趋势图的绘图区：卡片之内、标题与坐标字之外。
fn line_plot(card: D2D_RECT_F) -> D2D_RECT_F {
    rect(
        card.left + 46.0,
        card.top + 46.0,
        (card.right - card.left) - 62.0,
        (card.bottom - card.top) - 74.0,
    )
}

/// 趋势图第 `i` 个点的横坐标。只有一个点时落在绘图区正中 —— 除零之外没有第二种口径，
/// 画线的除法、命中测试的除法与那条"最近点"的取整共用这一个函数。
fn line_point_x(plot: D2D_RECT_F, n: usize, i: usize) -> f32 {
    if n < 2 {
        return plot.left + plot_width(plot) / 2.0;
    }
    plot.left + (i as f32 / (n - 1) as f32) * plot_width(plot)
}

/// 小时图的绘图区。
fn hourly_plot(card: D2D_RECT_F) -> D2D_RECT_F {
    let pad = 12.0;
    rect(
        card.left + pad,
        card.top + 42.0,
        (card.right - card.left) - pad * 2.0,
        (card.bottom - card.top) - 62.0,
    )
}

/// 小时图柱与柱之间的空档（逻辑像素）：画柱与命中测试都读这一份，不许两边各写一个数。
const HOURLY_GAP: f32 = 3.0;

/// 等宽柱排的柱位：第 `i` 根的 `(x, 宽)`，柱宽由根数与空档导出 ——
/// 画柱与命中测试走的是同一句除法。
fn bar_slot(plot: D2D_RECT_F, n: usize, i: usize, gap: f32) -> (f32, f32) {
    let n = n.max(1);
    let bw = ((plot_width(plot) - gap * (n as f32 - 1.0)) / n as f32).max(1.0);
    (plot.left + i as f32 * (bw + gap), bw)
}

/// 小时图的柱位（那一排 24 根）。
fn hourly_bar_slot(plot: D2D_RECT_F, n: usize, i: usize) -> (f32, f32) {
    bar_slot(plot, n, i, HOURLY_GAP)
}

/// 光标落在绘图区里时命中的那一根柱；**柱与柱之间的空档算没命中** ——
/// 只按取整反推下标的话，空档会被并进左边那根，"未命中"这一半就再也测不出来了。
fn slot_nearest(plot: D2D_RECT_F, n: usize, gap: f32, x: f32, y: f32) -> Option<usize> {
    if n == 0 || y < plot.top || y > plot.bottom || x < plot.left || x > plot.right {
        return None;
    }
    let i = ((x - plot.left) / (bar_slot(plot, n, 0, gap).1 + gap)).floor();
    if i < 0.0 || i as usize >= n {
        return None;
    }
    let (x0, bw) = bar_slot(plot, n, i as usize, gap);
    if x > x0 + bw {
        return None;
    }
    Some(i as usize)
}

/// 光标落在绘图区里时，横坐标最近的那个点；区外不算命中。
fn line_nearest(plot: D2D_RECT_F, n: usize, x: f32, y: f32) -> Option<usize> {
    if n == 0 || y < plot.top || y > plot.bottom || x < plot.left || x > plot.right {
        return None;
    }
    if n < 2 {
        return Some(0);
    }
    let t = (x - plot.left) / plot_width(plot);
    let i = (t * (n - 1) as f32).round();
    Some(i.clamp(0.0, (n - 1) as f32) as usize)
}

/// 图上那枚读数小条：贴着命中的那个点或那根柱的顶画，出界就往里收。
/// 三张图共用这一个画法，免得长出"趋势有、小时没有"这种半边货。
fn chart_tip(p: &P<'_>, card: D2D_RECT_F, at: Vector2, text: &str) {
    let w = (150.0f32).min((card.right - card.left) - 16.0).max(60.0);
    let lo = card.left + 8.0;
    let x = (at.X + 10.0).clamp(lo, (card.right - w - 8.0).max(lo));
    let y = (at.Y - 26.0).clamp(card.top + 4.0, (card.bottom - 20.0).max(card.top + 4.0));
    p.round(rect(x, y, w, 18.0), 5.0, &p.br.header, None);
    p.text(
        text,
        rect(x + 6.0, y + 1.0, w - 12.0, 16.0),
        &p.fmt.small,
        &p.br.fg,
    );
}

/// 纵轴的顶格：**点高与刻度共用这一个分母**（旧前端 `charts.js` 里 `axisTop` 那条规则搬过来）。
/// 取 4 的倍数、不小于最大值、最多只让出 3 —— 五条等距网格线才每一档都念得出整数、
/// 而且步长相等。分母要是分两处（点高一个、刻度一个），刻度就整体比实际低一截。
fn axis_top(max: i64) -> i64 {
    let m = max.max(1);
    (m.saturating_add(3) / 4 * 4).max(4)
}

/// 第 `i` 条网格线对应的次数（0 = 顶格，往下四等分）。顶格是 4 的倍数 ⇒ 每档都是整数。
fn line_tick_value(top: i64, i: usize) -> i64 {
    top - top / 4 * i as i64
}

/// 网格线（与贴在它上面的刻度）的纵坐标：两处必须走这一个函数，
/// 否则"刻度贴在自己的网格线上"这件事只靠两边各写一遍同样的算式碰巧一致。
fn line_grid_y(plot: D2D_RECT_F, i: usize) -> f32 {
    plot.top + plot_height(plot) * i as f32 / 4.0
}

/// 某个次数在绘图区里的纵坐标：与刻度共用 `top` 做分母，从高度反推才回得到原值。
fn line_point_y(plot: D2D_RECT_F, top: i64, value: i64) -> f32 {
    plot.bottom - (value as f64 / top as f64) as f32 * plot_height(plot)
}

fn draw_line_chart(p: &P<'_>, r: D2D_RECT_F, snap: &Snapshot, hover: Option<usize>) {
    p.round(r, 14.0, &p.br.card, Some(&p.br.border));
    let pad = 12.0;
    p.text(
        "每日活跃趋势",
        rect(r.left + pad, r.top + pad, 160.0, 20.0),
        &p.fmt.body,
        &p.br.fg,
    );
    p.text(
        &data::window_text(snap.period),
        rect(r.right - pad - 90.0, r.top + pad + 2.0, 90.0, 18.0),
        &p.fmt.right,
        &p.br.muted,
    );
    let daily_gaps = gap_note(&snap.gaps, queries::DOMAIN_DAILY);
    if !daily_gaps.is_empty() {
        p.text(
            &daily_gaps,
            rect(
                r.left + pad,
                r.top + 32.0,
                (r.right - r.left) - pad * 2.0,
                14.0,
            ),
            &p.fmt.small,
            &p.br.muted,
        );
    }

    let plot = line_plot(r);
    for i in 0..=4 {
        let y = line_grid_y(plot, i);
        p.line(plot.left, y, plot.right, y, &p.br.grid, 1.0);
    }

    let series = &snap.daily;
    let peak = series.iter().map(|(_, v)| *v).max().unwrap_or(0).max(1);
    // 点高与纵轴刻度共用这一个顶格：以前点高按 `峰 × 1.12`、刻度按 `峰 × 分数` 念，
    // 于是顶格那条线写着峰值、实际对应的是 1.12 倍 —— 整条纵轴每一档都少报约 12%，
    // 而"峰刚好贴着顶格"看着像对的，只有拿刻度反推高度才看得出来。
    let top = axis_top(peak);
    let n = series.len();
    let pt = |i: usize| {
        let v = series.get(i).map(|(_, v)| *v).unwrap_or(0);
        Vector2 {
            X: line_point_x(plot, n, i),
            Y: line_point_y(plot, top, v),
        }
    };
    for i in 1..n {
        let a = pt(i - 1);
        let b = pt(i);
        unsafe {
            p.rt.DrawLine(a, b, &p.br.accent, 2.0, None);
        }
    }
    // 「今日」那一档窗口只有一天：线画不出来，点必须画 —— 否则这一档整张图是空的，
    // 看着像"没数据"而不是"一天看不出趋势"。
    if n == 1 {
        let c = pt(0);
        p.round(
            rect(c.X - 4.0, c.Y - 4.0, 8.0, 8.0),
            4.0,
            &p.br.accent,
            None,
        );
    }
    // 纵轴：从上往下是顶格 → 0，四等分。刻度与网格线走同一个 `line_grid_y`。
    for i in 0..=4 {
        let y = line_grid_y(plot, i);
        p.text(
            &thousands(line_tick_value(top, i).max(0) as u64),
            rect(plot.left - 48.0, y - 7.0, 44.0, 14.0),
            &p.fmt.right,
            &p.br.muted,
        );
    }
    if let Some((first, _)) = series.first() {
        // 窗口起点就是序列第一项交回的那串日期，不在这儿按长度反推。
        let left = first.chars().skip(5).collect::<String>();
        p.text(
            &left,
            rect(plot.left, plot.bottom + 4.0, 60.0, 14.0),
            &p.fmt.small,
            &p.br.muted,
        );
    }
    p.text(
        "今天",
        rect(plot.right - 30.0, plot.bottom + 4.0, 30.0, 14.0),
        &p.fmt.right,
        &p.br.muted,
    );
    // 光标压在哪一天：把那一点圈出来，读数贴着它画（旧 WebView 三张图都有这枚 tip，
    // 换原生时只留了热力板那一处）。
    if let Some(i) = hover {
        if let Some((d, v)) = series.get(i) {
            let c = pt(i);
            p.round(
                rect(c.X - 5.0, c.Y - 5.0, 10.0, 10.0),
                5.0,
                &p.br.accent,
                None,
            );
            chart_tip(
                p,
                r,
                c,
                &format!("{} · {} 次", d, thousands((*v).max(0) as u64)),
            );
        }
    }
}

fn draw_hourly(p: &P<'_>, r: D2D_RECT_F, snap: &Snapshot, hover: Option<usize>) {
    p.round(r, 14.0, &p.br.card, Some(&p.br.border));
    let pad = 12.0;
    p.text(
        "小时分布",
        rect(r.left + pad, r.top + pad, 120.0, 20.0),
        &p.fmt.body,
        &p.br.fg,
    );
    p.text(
        &data::window_text(snap.period),
        rect(r.right - pad - 90.0, r.top + pad + 2.0, 90.0, 18.0),
        &p.fmt.right,
        &p.br.muted,
    );
    let hourly_gaps = gap_note(&snap.gaps, queries::DOMAIN_HOURLY);
    if !hourly_gaps.is_empty() {
        p.text(
            &hourly_gaps,
            rect(
                r.left + pad,
                r.top + 30.0,
                (r.right - r.left) - pad * 2.0,
                14.0,
            ),
            &p.fmt.small,
            &p.br.muted,
        );
    }

    let plot = hourly_plot(r);
    let values = &snap.hourly;
    let max = values.iter().cloned().fold(0_i64, i64::max).max(1) as f32;
    let n = values.len().max(1);
    for (i, v) in values.iter().enumerate() {
        let hp = (*v as f32 / max) * plot_height(plot);
        let (x, bw) = hourly_bar_slot(plot, n, i);
        let bar = rect(x, plot.bottom - hp, bw, hp);
        if let Some(b) = p.brush(p.pal.accent) {
            p.round(bar, 2.0, &b, None);
        }
        // 压住的那一根：**只描一圈，不重填**。以前照抄热力板那套"填卡片底色 + 描边"，
        // 键帽本来就是空的所以没事，柱状图里这一填就比邻柱更淡，一眼像"这一小时没数据"。
        if hover == Some(i) {
            if let Some(ghost) = p.brush(with_alpha(p.pal.accent, 0.0)) {
                p.round(
                    rect(x - 2.0, plot.bottom - hp - 2.0, bw + 4.0, hp + 4.0),
                    4.0,
                    &ghost,
                    Some(&p.br.accent),
                );
            }
        }
    }
    if let Some(i) = hover {
        if let Some(v) = values.get(i) {
            let (x, bw) = hourly_bar_slot(plot, n, i);
            let hp = (*v as f32 / max) * plot_height(plot);
            chart_tip(
                p,
                r,
                Vector2 {
                    X: x + bw / 2.0,
                    Y: plot.bottom - hp,
                },
                &format!("{i}时 · {} 次", thousands((*v).max(0) as u64)),
            );
        }
    }
    for i in (0..n).step_by(4) {
        let (x, bw) = hourly_bar_slot(plot, n, i);
        p.text(
            &format!("{i}"),
            rect(x + bw / 2.0 - 12.0, plot.bottom + 2.0, 24.0, 14.0),
            &p.fmt.center_small,
            &p.br.muted,
        );
    }
}

// ===== 托盘 / 热键 / 单实例 =====

fn make_tray(snap: &Arc<Mutex<Snapshot>>) -> (Option<TrayIcon>, Option<CheckMenuItem>) {
    let today = snap.lock().map(|s| s.today).unwrap_or(0);
    let menu = Menu::new();
    let show = MenuItem::with_id("show", "显示主窗口", true, None);
    let snip = MenuItem::with_id("snip", "截图（选区）", true, None);
    // 旧 Webview 版在设置页上有两颗：立即截图 / 立即标注截图。换原生时只留了标注那颗，
    // "松手即存"这条就只剩热键一个入口了——他日志里用得最多的偏偏是这一条。
    let snip_quick = MenuItem::with_id("snip_quick", "截图（松手即存）", true, None);
    // 直达最近那张：截图之后不用专门去文件夹里挑（旧 Webview 版有这一项，换原生时丢了）。
    let snip_folder = MenuItem::with_id("snip_folder", "打开截图文件夹", true, None);
    let floating_item = MenuItem::with_id("floating", "悬浮窗", true, None);
    // 暂停是运行时开关（不写配置）：它只停"计数"，不改变"是否启用采集"这件事。
    let pause = CheckMenuItem::with_id("pause", "暂停采集", true, collect::is_paused(), None);
    let quit = MenuItem::with_id("quit", "退出", true, None);
    let sep = PredefinedMenuItem::separator();
    if let Err(e) = menu.append_items(&[
        &show,
        &snip,
        &snip_quick,
        &snip_folder,
        &floating_item,
        &pause,
        &sep,
        &quit,
    ]) {
        tracing_note(&format!("托盘菜单构建失败：{e}"));
        return (None, None);
    }

    // 以前这里直接用的是代码画的那颗蓝色圆点（函数上还挂着"真实版本换"），于是托盘那颗
    // 和我们自己的标题栏/任务栏图标都不是一件事。现在两处同一份文件，读不到才退回圆点。
    let icon = match tray_icon_image() {
        Ok(i) => i,
        Err(why) => {
            tracing_note(&format!("托盘图标没读成（{why}）：这一格退回画的那颗圆点"));
            match Icon::from_rgba(tray_icon_rgba(32), 32, 32) {
                Ok(i) => i,
                Err(e) => {
                    tracing_note(&format!("托盘图标构建失败：{e}"));
                    return (None, None);
                }
            }
        }
    };
    match TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_icon(icon)
        .with_tooltip(tray_tooltip(today))
        .build()
    {
        Ok(t) => {
            tracing_note("托盘已创建");
            (Some(t), Some(pause))
        }
        Err(e) => {
            tracing_note(&format!("托盘创建失败：{e}"));
            (None, None)
        }
    }
}

/// 托盘 tooltip：今日次数 + 采集状态（采集开着时才有后半截）。
fn tray_tooltip(today: i64) -> String {
    let mut tip = format!("FocusFlow · 今日 {}", thousands(today as u64));
    if collect::is_started() {
        tip.push_str(if collect::is_paused() {
            " · 已暂停"
        } else {
            " · 采集中"
        });
        if collect::legacy_running() {
            tip.push_str(" · 旧版在跑");
        }
    }
    tip
}

/// 一条组合字符串解析成 `HotKey`；坏值/空值/没有修饰键的裸键返回 `None`。
/// 设置页写入前与注册时都用它，两边同源才不会"显示的是一个键、注册的是另一个"。
fn parse_combo(raw: &str) -> Option<HotKey> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let hk = trimmed.parse::<HotKey>().ok()?;
    // 裸键（`f1` 这种）解析得过，但注册上去等于把这个键在整个系统里吞掉，
    // 别的应用再也按不到它 —— 全局热键必须带至少一个修饰键。
    if hk.mods.is_empty() {
        return None;
    }
    Some(hk)
}

/// 这一条组合能不能用。`None` = 能用；`Some(原因)` = 拒写并解释。
/// `others` 是同段里**其余**每条热键的 `(中文名, 当前值)`：两条撞在一起时后注册的那条
/// 必然失败，那会让界面显示"已经设上了"、实际却按不出来，所以在这里就拒掉。
/// 判据与注册同源（都走 `parse_combo`），所以"设置页显示的组合"就是"注册的那把"。
fn combo_check(raw: &str, others: &[(&str, &str)]) -> Option<String> {
    let value = raw.trim();
    let Some(hk) = parse_combo(value) else {
        return Some(format!(
            "{value:?} 不成一个组合；要写成一个修饰键加一个主键，如 ctrl+shift+f"
        ));
    };
    for (label, other_raw) in others {
        if parse_combo(other_raw) == Some(hk) {
            return Some(format!("{value} 已经被「{label}」占用，两条热键要分开"));
        }
    }
    None
}

/// 这一行是不是三条热键绑定之一；是的话返回它在 `COMBO_KEYS` 里的下标
/// （0＝唤出、1＝松手即存、2＝进标注）。画、命中、读数三处都问这一个函数，
/// 免得"哪几行有小钮"这件事在三个地方各写一遍名单。
fn hotkey_slot(section: &str, key: &str) -> Option<usize> {
    if section != "hotkey" {
        return None;
    }
    COMBO_KEYS.iter().position(|k| *k == key)
}

/// 值框右沿那颗小钮的位置。**画与命中共用这一份**：两处各算一遍迟早漂开，
/// 而漂开的症状是"看得见却点不到"或"点 A 改的是 B"。
fn hotkey_btn_rect(vb: D2D_RECT_F) -> D2D_RECT_F {
    rect(vb.right - 40.0, vb.top + 4.0, 34.0, 18.0)
}

/// `[hotkey]` 某个键的中文名（与设置页那一行的标签同一份）——撞车时要点名是哪一条。
fn hotkey_label(key: &str) -> &'static str {
    match key {
        "toggle_window" => "唤出窗口热键",
        "snip" => "区域截图热键",
        _ => "标注截图热键",
    }
}

/// `[hotkey]` 里这一条组合当前配置的原串（文件里没有或写着空 → 核心默认值）。
/// 设置页显示的就是它，注册的键是它解析出来的，所以两边不会分叉。
/// 清空过的那一条返回**空串**而不是默认值：否则"它现在不绑定"这件事在重复检查里
/// 会被算成"它绑着默认那一串"，另一条想用默认组合就会被无故拒掉。
fn hotkey_config_raw(key: &str) -> String {
    if hotkey_is_off(key) {
        return String::new();
    }
    let defaults = focusflow_core::config::default_config();
    let def = defaults
        .get("hotkey")
        .and_then(|s| s.get(key))
        .cloned()
        .unwrap_or_default();
    focusflow_core::config::instance().get_or("hotkey", key, &def)
}

/// 这一条热键是不是被**清空了**（设置页那颗叉）。
///
/// 与"文件里没这一行"不是一回事：没这一行 ⇒ 按默认组合注册（全新安装就是这一态）；
/// 有这一行但值是空 ⇒ 他刚说"这条我不想用"，那就**不注册**，绝不能悄悄退回默认值。
/// 空值能不能跨重启留住，由 core 的 `an_emptied_key_stays_present_and_empty_through_a_save_and_reload`
/// 钉着（`get_or` 本来会把这两种捏成一个）。
fn hotkey_is_off(key: &str) -> bool {
    let cfg = focusflow_core::config::instance();
    hotkey_off_of(cfg.has_key("hotkey", key), &cfg.get("hotkey", key))
}

/// 「这一条算不算被他停用了」的唯一判据：**文件里有这一行**，且**值是空**。
/// 少了前半个条件，"没配过"（该按默认组合注册）就会被念成"不注册"——
/// 那是全新安装的症状，不是他点过叉。
fn hotkey_off_of(present: bool, raw: &str) -> bool {
    present && raw.trim().is_empty()
}

/// 拿配置里其余几条热键做重复检查（还是 `combo_check` 那一份判据）。
fn combo_reject_reason(key: &str, raw: &str) -> Option<String> {
    let others: Vec<(&'static str, String)> = HOTKEY_KEYS
        .iter()
        .filter(|k| **k != key)
        .map(|k| (hotkey_label(k), hotkey_config_raw(k)))
        .collect();
    let refs: Vec<(&str, &str)> = others.iter().map(|(l, v)| (*l, v.as_str())).collect();
    combo_check(raw, &refs)
}

/// 一条热键绑定的注册结局。
struct Combo {
    /// 注册上的组合（没注册上是 None）——注销时要按它原样撤回
    hk: Option<HotKey>,
    /// 配置里的原串：状态栏要念给用户看他配的到底是哪一串
    raw: String,
    /// 失败原因；成功时是 None
    why: Option<String>,
}

impl Combo {
    /// 转成 `hotkey_status_line` 用的三元组（中文标签在调用处按 `hotkey_label` 给）。
    fn part<'a>(&self, label: &'a str) -> (&'a str, String, Option<String>) {
        (label, self.raw.clone(), self.why.clone())
    }
}

/// 一个虚拟键码在配置里该写成什么名字（`parse_combo` 那一侧认得的写法）。
///
/// 只覆盖"能当主键"的那些键；修饰键本身走 `combo_from_key` 的四个布尔位。
/// 认不得的键返回 `None` ⇒ 录入侧原样回一句"这一键还不支持"，而不是写进去一个
/// 注册不上的串（那正是"界面显示的和注册的各说各话"那一族）。
fn key_name(vk: u32) -> Option<&'static str> {
    let name = match vk {
        0x08 => "backspace",
        0x09 => "tab",
        0x0D => "enter",
        0x10 => "shift",
        0x11 => "ctrl",
        0x12 => "alt",
        0x13 => "pause",
        0x14 => "capslock",
        0x1B => "esc",
        0x20 => "space",
        0x21 => "pageup",
        0x22 => "pagedown",
        0x23 => "end",
        0x24 => "home",
        0x25 => "left",
        0x26 => "up",
        0x27 => "right",
        0x28 => "down",
        0x2A => "printscreen",
        0x2C => "printscreen",
        0x2D => "insert",
        0x2E => "delete",
        // 0x2F VK_HELP 不在表里：解析器不认 "help" 这个名字（枚举用例逮到的），
        // 而这个键在现成键盘上基本不存在 ⇒ 宁可不录，也不写一个注册不上的串。
        0x44 => "numlock",
        0x45 => "scrolllock",
        0x60..=0x69 => {
            return Some(
                [
                    "numpad0", "numpad1", "numpad2", "numpad3", "numpad4", "numpad5", "numpad6",
                    "numpad7", "numpad8", "numpad9",
                ][(vk - 0x60) as usize],
            );
        }
        // 小键盘的运算符（* + - . /）不在表里：解析器不认 "numpad*" 这一类写法（枚举用例
        // 一次把它们全念出来了），而拿不准它到底要什么名字就不该写进配置——
        // "存进去一个注册不上的串"正是这套录入要防的那件事。数字键 0-9 是认的。
        0x70..=0x79 => {
            // F1..F10：`vk - 0x6F` 是 1..10，拼进这张表而不是算字符串——算出来的
            // "f{}" 与 `parse_combo` 认的名字大小写无关，但字面量表一眼能核对。
            return Some(
                ["f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10"][(vk - 0x70) as usize],
            );
        }
        0x7A..=0x7B => {
            return Some(["f11", "f12"][(vk - 0x7A) as usize]);
        }
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
        0xE4..=0xE8 => return None, // 未分配/ OEM 具体键：不猜
        // 字母与主键盘数字：VK_A..VK_Z / VK_0..VK_9 就是它们自己。
        _ if (0x41..=0x5A).contains(&vk) => {
            return Some(
                [
                    "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p",
                    "q", "r", "s", "t", "u", "v", "w", "x", "y", "z",
                ][(vk - 0x41) as usize],
            );
        }
        _ if (0x30..=0x39).contains(&vk) => {
            return Some(["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"][(vk - 0x30) as usize]);
        }
        _ => return None,
    };
    Some(name)
}

/// 按下录入的那一击 → 配置里的那一串。`None` = 这一键不能当主键（含只按修饰键）。
///
/// 顺序照 `parse_combo` 认的样子：`ctrl+alt+shift+win+主键`。为什么固定这个次序而不是
/// 按按下先后——先后次序会因手而生出 `shift+ctrl+f` 与 `ctrl+shift+f` 两种写法，
/// 屏上、文件里、日志里就得跟着多出一把对不齐的账。
///
/// `esc` 一律不当主键：它是"取消录入"那个手势（也是截图覆盖层的退出键），
/// 让它能被绑上就等于让取消动作自己变成一个可绑项。
fn combo_from_key(vk: u32, ctrl: bool, alt: bool, shift: bool, win: bool) -> Option<String> {
    if vk == 0x1B {
        return None;
    }
    // 修饰键自己不算主键：只按 Ctrl 时屏上该继续等，而不是写一个注册不上的 "ctrl"。
    if matches!(vk, 0x10 | 0x11 | 0x12 | 0x5B | 0x5C) {
        return None;
    }
    let key = key_name(vk)?;
    let mut parts: Vec<&str> = Vec::with_capacity(5);
    if ctrl {
        parts.push("ctrl");
    }
    if alt {
        parts.push("alt");
    }
    if shift {
        parts.push("shift");
    }
    if win {
        // 解析器认的是 `super`/`cmd`/`command`，**不认** `win` —— 写成 "win+f" 会存进一个
        // 注册不上的串（这条是枚举用例逮到的：`生成的串解析不回来：win+backspace`）。
        parts.push("super");
    }
    // 与 `parse_combo` 同一条闸：裸键当全局热键等于把这个键在整个系统里吞掉。
    if parts.is_empty() {
        return None;
    }
    parts.push(key);
    Some(parts.join("+"))
}

/// 录入那一拍现在屏上该说什么。`None` = 这一击可以走正常流程（不吞键）。
/// 抽出来是因为"为什么没录上"这句话必须和判据同源，不然它又是一句许愿。
fn capture_refusal(vk: u32) -> Option<&'static str> {
    match vk {
        0x10 | 0x11 | 0x12 | 0x5B | 0x5C => Some("只按修饰键不算一个组合"),
        0x1B => None, // Esc 是取消，不算拒绝
        _ if key_name(vk).is_none() => Some("这一键还不支持，改用手输或换一键"),
        _ => None,
    }
}

/// 读 `[hotkey] <key>` 并注册它。成功返回组合，失败返回原因（这一条不占用全局键）。
/// 日志里带 `key = 原串` 这段 ASCII，验收脚本据此核对"配置写的"和"注册的"是不是同一个。
fn register_combo(manager: Option<&GlobalHotKeyManager>, key: &str) -> Combo {
    let manager = match manager {
        Some(m) => m,
        None => {
            return Combo {
                hk: None,
                raw: hotkey_config_raw(key),
                why: Some("拿不到热键管理器".to_string()),
            }
        }
    };
    let raw = hotkey_config_raw(key);
    let hk = match parse_combo(&raw) {
        Some(hk) => hk,
        None => {
            tracing_note(&format!("热键 {key} = {raw:?} 解析失败：这一条没有注册"));
            return Combo {
                hk: None,
                raw,
                why: Some("这串组合解析不出来".to_string()),
            };
        }
    };
    match manager.register(hk) {
        Ok(()) => {
            tracing_note(&format!("热键已注册 {key} = {raw}"));
            Combo {
                hk: Some(hk),
                raw,
                why: None,
            }
        }
        Err(e) => {
            tracing_note(&format!("热键注册失败 {key} = {raw}：{e}"));
            Combo {
                hk: None,
                raw,
                why: Some(format!("系统拒了：{e}")),
            }
        }
    }
}

/// 状态行里哪些该用危险色画。"注意…" 是历史形状（双计提醒、搬迁没成那一类），
/// 另外这几类也必须算：一条热键没注册上、以及截图那句"没存盘 / 图没编出来 / 剪贴板没进去"。
/// 这些都要人去做点什么，用灰字画出来会被当成普通回执掠过去——旧 Webview 版给热键失败
/// 就是红色的那一档。
fn status_is_warning(line: &str) -> bool {
    line.starts_with("注意")
        || line.starts_with("没注册上：")
        || line.starts_with("没存盘：")
        || line.starts_with("图没编出来：")
        || line.starts_with("没有可出的图：")
        || line.contains("剪贴板没进去")
}

/// 三条绑定的注册结局拼成状态栏那一行。**点名到哪一条**是这条读数的全部意义：
/// "截图的热键被别的程序占了"和"唤出的热键写错了"是两件完全不同的事，用户要改的东西不一样，
/// 而只报"注册上 N 条（详见日志）"等于让人去翻日志。全成都把三条当前组合念出来。
///
/// `offs` 是他**主动清空**的那几条（值还在文件里，只是空的）：那不算失败，用红色喊出来
/// 就是骗他去做点什么；但也不能不提——不然"三条热键已注册"这句话本身就是假的。
fn hotkey_status_line(items: &[(&str, String, Option<String>)], offs: &[&str]) -> String {
    let mut failed: Vec<String> = Vec::new();
    for (label, raw, why) in items {
        if let Some(why) = why {
            failed.push(format!("{label} {raw}：{why}"));
        }
    }
    let off_tail = if offs.is_empty() {
        String::new()
    } else {
        format!("；未启用 {} 条（{}）", offs.len(), offs.join("、"))
    };
    if failed.is_empty() {
        let live: Vec<String> = items
            .iter()
            .map(|(l, raw, _)| format!("{l} {raw}"))
            .collect();
        if live.is_empty() {
            return format!("三条热键都没挂上{off_tail}");
        }
        return format!("已按配置注册：{}", live.join(" · ")) + &off_tail;
    }
    let ok = items.len() - failed.len();
    let mut line = format!("没注册上：{}", failed.join("；"));
    if ok > 0 {
        line.push_str(&format!("；其余 {ok} 条已注册"));
    }
    line.push_str(&off_tail);
    line
}

/// 注销此刻真正注册着的组合（总开关关掉、或改完要重注册时先走这一步）。
fn unregister_combos(manager: Option<&GlobalHotKeyManager>, list: [Option<HotKey>; 3]) {
    let Some(manager) = manager else { return };
    let all: Vec<HotKey> = list.into_iter().flatten().collect();
    if !all.is_empty() {
        let _ = manager.unregister_all(&all);
    }
}

/// 设置页里能改、启动时要注册的那几条热键（`[hotkey]` 段的键名）。
/// 这份清单只写一次：写入前的校验与改完重注册都读它，加一条不用改第二处。
const HOTKEY_KEYS: [&str; 3] = ["toggle_window", "snip", "snip_annotate"];

/// 「更改数据文件夹」要落的那两行，以及它们的**顺序**：先写待搬运标记，再改 `data_home`。
/// 反过来说明中间崩掉会留下哪种半态：目录已改、标记没写 ⇒ 新进程按空的
/// 新目录起步，旧历史永远不会再被搬；而标记已写、目录还没改是安全的 —— core 的
/// `plan_migration` 认得这一种，清掉标记就行（旧目录里的数据一个都没动过）。
/// 抽成纯函数就是为了让这条顺序能被单测钉住。
fn data_move_writes(
    src: &std::path::Path,
    dst: &std::path::Path,
) -> [(&'static str, &'static str, String); 2] {
    [
        ("paths", "data_migrate_from", src.display().to_string()),
        ("paths", "data_home", dst.display().to_string()),
    ]
}

/// 安排一次数据目录搬迁：先用 core 的同一把尺子校验目标，再把那两行按顺序写下并**当场落盘**。
/// 本进程不动任何数据文件，仍然照旧往旧目录写满；搬运由下一个进程在启动早期兑现。
/// （返回类型写全：`Result` 这个名字在本文件里是 windows-result 的那一个。）
fn arrange_data_move(
    src: &std::path::Path,
    dst: &std::path::Path,
) -> std::result::Result<String, String> {
    focusflow_core::data_location::validate_new_data_home(src, dst)?;
    let cfg = focusflow_core::config::instance();
    for (section, key, value) in data_move_writes(src, dst) {
        cfg.set(section, key, &value).map_err(|e| e.to_string())?;
    }
    // 去抖那 300 毫秒不一定赶得上：用户点完可能马上就退出，标记得先在磁盘上。
    cfg.save().map_err(|e| e.to_string())?;
    Ok(format!(
        "已安排：重启后把 data/ 与 backup/ 从 {} 搬到 {}",
        src.display(),
        dst.display()
    ))
}

/// 测试钩子 `FOCUSFLOW_PLUGIN_SEL="<插件名>|<行 id 或 first>|<sel 按钮 id 可空>"`：
/// 按记录 id 选中插件表格里的一行，再按一颗 `sel` 按钮 —— 调的就是点击那条路径上的
/// 同两个方法（`select_plugin_row` / `press_sel_button`）。
/// 为什么按 id 不按坐标：注入的点击要算像素，而表格行的位置随插件视图与滚动变化；
/// 这里要验的是"Lua 表里的 ids/group/onselect 有没有真的走到"，坐标那一段仍归眼验。
fn run_plugin_sel_test(hwnd: HWND) {
    let Ok(spec) = std::env::var("FOCUSFLOW_PLUGIN_SEL") else {
        return;
    };
    let mut it = spec.split('|');
    let plugin = it.next().unwrap_or("").to_string();
    let want = it.next().unwrap_or("first").to_string();
    let button = it.next().unwrap_or("").to_string();
    if plugin.is_empty() {
        tracing_note("sel-test bad-spec");
        return;
    }
    let Some(app) = (unsafe { app_ptr(hwnd) }) else {
        return;
    };
    // 插件是切到「插件管理」那一页才加载的；钩子直接来就得自己先把那一步补上，
    // 否则 `get_plugin` 拿到的是空清单。
    app.reload_plugins();
    if app.plugin_open.as_deref() != Some(plugin.as_str()) {
        app.open_plugin_view(hwnd, &plugin);
    }
    let Some(view) = app
        .pm
        .as_ref()
        .and_then(|pm| pm.get_plugin(&plugin))
        .and_then(|p| p.view.clone())
    else {
        tracing_note(&format!("sel-test no-view {plugin}"));
        return;
    };
    let (w, h) = app.client_dip(hwnd);
    let card = rect(16.0, 208.0, w - 32.0, h - 208.0 - 16.0);
    let area = rect(
        card.left,
        card.top + 40.0,
        card.right - card.left,
        card.bottom - card.top,
    );
    let sel_map = app.plugin_sel.clone();
    let ops = plugin_view_ops(&view, area, 0.0, &sel_map);
    let Some((ids, group, onselect)) = ops.iter().find_map(|op| match op {
        POp::Table {
            ids,
            group,
            onselect,
            ..
        } if !ids.is_empty() => Some((ids.clone(), group.clone(), onselect.clone())),
        _ => None,
    }) else {
        tracing_note(&format!("sel-test no-table {plugin}"));
        return;
    };
    let picked = if want == "first" {
        ids[0].clone()
    } else {
        want.clone()
    };
    if !ids.contains(&picked) {
        tracing_note(&format!("sel-test missing id {picked}"));
        return;
    }
    app.select_plugin_row(hwnd, &plugin, &group, &onselect, &picked);
    if !button.is_empty() {
        app.press_sel_button(hwnd, &plugin, &button, &group);
    }
}

/// 三条组合的键名，**顺序就是它们各自落进哪个字段 / 状态行点名的顺序**：
/// 0＝`toggle_window`（唤出主窗口）、1＝`snip`（松手即存）、2＝`snip_annotate`（进标注）。
const COMBO_KEYS: [&str; 3] = ["toggle_window", "snip", "snip_annotate"];

/// 这一次启动要挂哪几条组合：总开关 `[hotkey] enabled` 关掉 ⇒ 空表，三条一起不占用全局键。
///
/// 以前这条只有运行中那颗开关做得到：`toggle_hotkey` 会注销并把 `hotkey.enabled` 写进配置，
/// 可启动侧没人读它 ⇒ 重启后 `make_hotkeys` 照配置全注册回去，而屏上那一行显示的还是"开"。
/// 抽成纯函数是因为"关掉之后还剩几条在占键"这种形状编译器与现有腿都不响。
fn combo_keys(enabled: bool) -> &'static [&'static str] {
    if enabled {
        &COMBO_KEYS
    } else {
        &[]
    }
}

/// 开机注册三条热键的结局。写成结构体而不是五元组：调用处按名字取值，
/// 也不用为了多带一句状态把返回类型越拧越复杂。
struct HotkeysAtBoot {
    manager: Option<GlobalHotKeyManager>,
    toggle: Option<HotKey>,
    snip: Option<HotKey>,
    annotate: Option<HotKey>,
    /// 有条没注册上时给状态栏的那句话（全成都成功是 None）
    warning: Option<String>,
}

fn make_hotkeys() -> HotkeysAtBoot {
    let manager = match GlobalHotKeyManager::new() {
        Ok(m) => Some(m),
        Err(e) => {
            tracing_note(&format!("热键管理器创建失败：{e}"));
            None
        }
    };
    let enabled = focusflow_core::config::instance().get_bool("hotkey", "enabled", true);
    let mut slots: [Option<HotKey>; 3] = [None, None, None];
    let mut parts: Vec<(&'static str, String, Option<String>)> = Vec::new();
    let mut offs: Vec<&'static str> = Vec::new();
    for (i, key) in combo_keys(enabled).iter().enumerate() {
        if hotkey_is_off(key) {
            offs.push(hotkey_label(key));
            continue;
        }
        let combo = register_combo(manager.as_ref(), key);
        parts.push(combo.part(hotkey_label(key)));
        slots[i] = combo.hk;
    }
    let warning = if !enabled {
        tracing_note("热键未注册（[hotkey] enabled = false）：本次不占用任何全局键");
        Some("全局热键没开（[hotkey] enabled = false）：三条组合都不注册".to_string())
    } else if parts.iter().any(|(_, _, why)| why.is_some()) {
        Some(hotkey_status_line(&parts, &offs))
    } else {
        None
    };
    HotkeysAtBoot {
        manager,
        toggle: slots[0],
        snip: slots[1],
        annotate: slots[2],
        warning,
    }
}

/// 代码画的那颗蓝色圆点：**只当兜底**（`assets/focusflow.ico` 读不到时才用它，见 `make_tray`）。
/// 正常形态下托盘那颗是 `.ico` 里的真图——留在这里是因为"缺一个图标"不该让整个托盘起不来，
/// 但屏上必须看得出来那是退路（它跟别的图标明显不一样）。
fn tray_icon_rgba(size: u32) -> Vec<u8> {
    let mut px = vec![0u8; (size * size * 4) as usize];
    let c = (size as f32 - 1.0) / 2.0;
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - c;
            let dy = y as f32 - c;
            let d = (dx * dx + dy * dy).sqrt() / c;
            let alpha = if d <= 1.0 { 255 } else { 0 };
            let i = ((y * size + x) * 4) as usize;
            px[i] = 0x5B;
            px[i + 1] = 0x9C;
            px[i + 2] = 0xF7;
            px[i + 3] = alpha;
        }
    }
    px
}

unsafe fn bring_to_front(hwnd: HWND) {
    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = ShowWindow(hwnd, SW_RESTORE);
    let _ = SetForegroundWindow(hwnd);
}

/// 一个实例只留一个：抢不到互斥体就把已有窗口叫出来，自己退出。
fn already_running() -> Option<HANDLE> {
    let handle = unsafe { CreateMutexW(None, true, w!("Global\\FocusFlow.Native.P0")) }.ok()?;
    // GetLastError 必须紧跟着读：中间任何一。
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        if let Ok(existing) = unsafe { FindWindowW(CLASS_NAME, None) } {
            if !existing.is_invalid() {
                unsafe { bring_to_front(existing) };
            }
        }
        return None;
    }
    Some(handle)
}

// ===== 小工具 =====

fn plot_width(r: D2D_RECT_F) -> f32 {
    r.right - r.left
}
fn plot_height(r: D2D_RECT_F) -> f32 {
    r.bottom - r.top
}

fn text_format(
    dw: &IDWriteFactory,
    size: f32,
    weight: DWRITE_FONT_WEIGHT,
) -> windows::core::Result<IDWriteTextFormat> {
    unsafe {
        // 与旧 WebView 的 `--font: "Segoe UI", "Microsoft YaHei", "微软雅黑", sans-serif`
        // 同一顺序：拉丁字母与数字用 Segoe UI，中文由 DirectWrite 的字体回退落到雅黑。
        // 原来这里直接写 "Microsoft YaHei UI"，把数字也交给了雅黑 —— 主界面的 14,114、
        // 图表刻度、表格数字全比旧版粗一档，就是"字体看着不对"的来源。
        let f = dw.CreateTextFormat(
            w!("Segoe UI"),
            None,
            weight,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            w!("zh-cn"),
        )?;
        let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
        Ok(f)
    }
}

fn right_format(dw: &IDWriteFactory, size: f32) -> windows::core::Result<IDWriteTextFormat> {
    let f = text_format(dw, size, DWRITE_FONT_WEIGHT_NORMAL)?;
    unsafe {
        let _ = f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_TRAILING);
    }
    Ok(f)
}

fn center_format(dw: &IDWriteFactory, size: f32) -> windows::core::Result<IDWriteTextFormat> {
    center_format_w(dw, size, DWRITE_FONT_WEIGHT_NORMAL)
}

/// 居中且带字重的一档：设置页那些章（「开」「执行」「没有贴图」）是白字压在饱和底色上，
/// 400 那一档在这种底色上看着比实际细。
/// ⚠ 汉字这一侧要留意：雅黑只有 400/700 两档，600 会被就近挑到 700，
/// 所以"稍微粗一点"只对拉丁字母与数字成立。
fn center_format_w(
    dw: &IDWriteFactory,
    size: f32,
    weight: DWRITE_FONT_WEIGHT,
) -> windows::core::Result<IDWriteTextFormat> {
    let f = text_format(dw, size, weight)?;
    unsafe {
        let _ = f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
        let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
    }
    Ok(f)
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

// ===== 悬浮窗接线 =====

/// 把快照里"悬浮窗要看的那两格"推给卡片线程；速度由卡片自己读 `collect::cpm()`。
fn publish_floating(snap: &Snapshot) {
    floating::publish(floating::Live {
        today: snap.today,
        duration_sec: snap.today_app_seconds,
    });
}

/// 显示/隐藏悬浮窗 —— 托盘菜单与设置页那一行共用的**唯一**入口。
///
/// 写的是 `[floating] enabled`，所以"关掉"能跨重启记住；但**不要求重启**：
/// 开关这一下就把卡片显示/藏起来（与旧版同一个口径）。
fn toggle_floating(main: HWND, want: Option<bool>) -> bool {
    let want = want.unwrap_or(!floating::is_visible());
    let cfg = focusflow_core::config::instance();
    if let Err(e) = cfg.set("floating", "enabled", if want { "true" } else { "false" }) {
        tracing_note(&format!("悬浮窗开关写配置失败：{e}"));
    }
    if want && !floating::is_alive() {
        // 线程第一次就没起来（或已经关掉）时，再点一次要真的能起回来。
        if let Err(e) = floating::spawn(main) {
            tracing_note(&format!("悬浮窗重启失败：{e}"));
            return false;
        }
    }
    let _ = floating::set_visible(want);
    let visible = floating::is_visible();
    tracing_note(if want {
        "悬浮窗：显示"
    } else {
        "悬浮窗：隐藏"
    });
    visible
}

/// 深色 / 浅色切换（设置页那一行）。写 `[gui] theme`，当下重画，悬浮窗也立刻换色。
fn toggle_theme(hwnd: HWND) {
    let next = if theme_name().trim() == "dark" {
        "light"
    } else {
        "dark"
    };
    let cfg = focusflow_core::config::instance();
    match cfg.set("gui", "theme", next) {
        Ok(()) => tracing_note(&format!("主题已切换：{next}")),
        Err(e) => tracing_note(&format!("主题写入失败：{e}")),
    }
    // 标题栏是非客户区，不归 D2D 管：深浅得单独告诉 DWM 一次。
    apply_title_theme(hwnd);
    // 悬浮窗自己每秒重读配置，这里催一帧让它当场变色（与主界面同一刻）。
    floating::refresh();
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

/// 退出：托盘菜单与验收消息共用这一条。顺序有讲究 —— 先收卡片，再停采集并落库
/// （`collect::shutdown` 里是 flush → stop writer → backup_on_exit），最后才销毁主窗口。
fn quit_app(hwnd: HWND) {
    floating::shutdown();
    collect::shutdown(focusflow_core::config::instance());
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

/// GUI 程序没有控制台，出错时留一份文件，别静默消失。
fn tracing_note(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("focusflow.log")
    {
        let _ = writeln!(f, "{msg}");
    }
}

// ===== 窗口过程 =====

unsafe fn app_ptr(hwnd: HWND) -> Option<&'static mut App> {
    let p = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut App;
    if p.is_null() {
        None
    } else {
        Some(unsafe { &mut *p })
    }
}

/// EDIT 的子类过程：Enter 提交、Esc 取消、失焦提交。
///
/// 为什么用子类化而不是在父窗口里接 `WM_COMMAND`：单个 EDIT 的 Enter 不会变成
/// `EN_*` 通知，Esc 更不会；这两个键只能靠自己截。
unsafe extern "system" fn edit_proc(
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    const VK_RETURN: usize = 0x0D;
    const VK_ESCAPE: usize = 0x1B;
    unsafe {
        match msg {
            WM_KEYDOWN if w.0 == VK_RETURN || w.0 == VK_ESCAPE => {
                let code = if w.0 == VK_RETURN { 1 } else { 2 };
                if let Ok(parent) = GetParent(hwnd) {
                    let _ = PostMessageW(Some(parent), WM_EDIT_DONE, WPARAM(code), LPARAM(0));
                }
                LRESULT(0)
            }
            WM_CHAR if w.0 == VK_RETURN || w.0 == VK_ESCAPE => LRESULT(0),
            // 不在这里做"失焦即提交"：窗口不是前台时 `SetFocus` 不会真正生效，
            // 而 EDIT 刚建出来就会收到一次假的 WM_KILLFOCUS，提交的会是预填的旧名字
            // （2026-10-26 实测）。提交由 Enter/Esc 和"点编辑框外面"两条路负责。
            _ => DefSubclassProc(hwnd, msg, w, l),
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let _hdc = unsafe { BeginPaint(hwnd, &mut ps) };
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                app.draw(hwnd);
            }
            unsafe {
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_SIZE => {
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_DPICHANGED => {
            // 换屏/改缩放：渲染目标里钉着。
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                app.drop_target();
            }
            let suggested = unsafe { &*(l.0 as *const RECT) };
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    suggested.left,
                    suggested.top,
                    suggested.right - suggested.left,
                    suggested.bottom - suggested.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_TIMER if w.0 == TIMER_EVENTS => {
            // 心跳：只留证据，不刷屏（。
            static TICKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = TICKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n.is_multiple_of(50) {
                tracing_note(&format!("timer tick {n}"));
            }
            // 验收模式（FOCUSFLOW_ACCEPTANCE=1）下每秒写一行机器可读的状态：
            // 脚本不用 OCR 就能断言「今日涨了几个」「暂停期间有没有停」。
            // 键名用 ASCII，值也只有数字/on-off，解析起来不用管编码。
            if n.is_multiple_of(10) && ACCEPTANCE.load(std::sync::atomic::Ordering::Relaxed) {
                if let Some(app) = unsafe { app_ptr(hwnd) } {
                    let today = app.snap.lock().map(|s| s.today).unwrap_or(0);
                    tracing_note(&format!(
                        "验收状态 today={today} cpm={} collector={} paused={}",
                        collect::cpm(),
                        if collect::is_started() { "on" } else { "off" },
                        if collect::is_paused() {
                            "true"
                        } else {
                            "false"
                        }
                    ));
                }
            }
            // 托盘点击 / 菜单 / 热键只在事件通道里，靠这 100ms 一拍取出来。
            while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
                // 悬停那一刻现问一次"今日"。隐藏期间快照线程不查（见 `watching`），
                // tooltip 会停在最后一次显示时的数；而他读到那句话的时机正是鼠标压上去
                // 这一瞬 ⇒ 用采集侧那个原子计数当场补一次（一条原子读，不碰 SQL）。
                // 采集没开着就沿用最后一次快照的值——那时今日这一格本来也不会再动。
                if matches!(ev, TrayIconEvent::Enter { .. }) {
                    if let Some(app) = unsafe { app_ptr(hwnd) } {
                        let today = match collect::today_total() {
                            Some(n) => n,
                            None => app.snap.lock().map(|s| s.today).unwrap_or(0),
                        };
                        if let Some(t) = app.tray.as_ref() {
                            let _ = t.set_tooltip(Some(tray_tooltip(today)));
                        }
                    }
                }
                if let TrayIconEvent::Click {
                    button: tray_icon::MouseButton::Left,
                    button_state: tray_icon::MouseButtonState::Up,
                    ..
                } = ev
                {
                    unsafe { bring_to_front(hwnd) };
                }
            }
            while let Ok(ev) = MenuEvent::receiver().try_recv() {
                match ev.id.as_ref() {
                    "show" => unsafe { bring_to_front(hwnd) },
                    "snip" => snip::start(hwnd, true),
                    // 松手即存：与快速那条热键同一个入口（`wparam = 0` 的 `WM_SNIP_START` 走的就是它）
                    "snip_quick" => snip::start(hwnd, false),
                    "snip_folder" => {
                        // 成与败都写日志：托盘点下去"没反应"是这条功能唯一的故障形态，
                        // 不吭声就等于把失败吞掉（旧 Webview 版同一条口径）。
                        match focusflow_core::reveal::reveal_last_screenshot() {
                            Ok(msg) => tracing_note(&format!("截图目录：{msg}")),
                            Err(e) => tracing_note(&format!("截图目录打不开：{e}")),
                        }
                    }
                    "floating" => {
                        toggle_floating(hwnd, None);
                        unsafe {
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                    }
                    "pause" => {
                        if let Some(app) = unsafe { app_ptr(hwnd) } {
                            app.toggle_pause(hwnd);
                        }
                    }
                    "quit" => quit_app(hwnd),
                    _ => {}
                }
            }
            while let Ok(ev) = GlobalHotKeyEvent::receiver().try_recv() {
                // 三条热键共用一个通道：按 id 分流，否则截图的键会把窗口唤出来。
                // 组合键现在能被用户改掉，所以只响应"此刻真正注册着的那三个 id"，
                // 刚注销/换掉的旧 id 投过来的事件一律不认。
                let role = unsafe { app_ptr(hwnd) }.and_then(|app| {
                    if app
                        .snip_annotate_hk
                        .map(|h| h.id() == ev.id)
                        .unwrap_or(false)
                    {
                        Some("snip-annotate")
                    } else if app.snip_hk.map(|h| h.id() == ev.id).unwrap_or(false) {
                        Some("snip")
                    } else if app.hotkey_hk.map(|h| h.id() == ev.id).unwrap_or(false) {
                        Some("toggle")
                    } else {
                        None
                    }
                });
                match role {
                    Some("snip-annotate") => {
                        tracing_note("收到标注截图热键：选区后进标注窗");
                        snip::start(hwnd, true);
                    }
                    Some("snip") => {
                        tracing_note("收到截图热键：松手即存");
                        snip::start(hwnd, false);
                    }
                    Some("toggle") => {
                        tracing_note("收到热键事件，唤出窗口");
                        unsafe { bring_to_front(hwnd) };
                    }
                    _ => tracing_note(&format!("收到没注册的热键事件 id={}", ev.id)),
                }
            }
            // 插件视图要按 1 秒重算一次：番茄钟的倒计时、键鼠计数都是插件自己在
            // get_view() 里算出来的，不重算就停在那儿。
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                if let Some(name) = app.plugin_open.clone() {
                    let due = app
                        .last_pv_refresh
                        .map(|t| t.elapsed() >= Duration::from_millis(1000))
                        .unwrap_or(true);
                    if due {
                        app.last_pv_refresh = Some(std::time::Instant::now());
                        if let Some(pm) = app.pm.as_mut() {
                            let _ = pm.refresh_view(&name);
                        }
                        unsafe {
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                    }
                }
            }
            // 「当前速度」跟这 100ms 一拍走：数值变了才重画（否则等于 10Hz 全窗重绘）。
            let cpm_now = if collect::is_started() && !collect::is_paused() {
                collect::cpm()
            } else {
                0
            };
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                if app.last_cpm != cpm_now {
                    app.last_cpm = cpm_now;
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
            }
            LRESULT(0)
        }
        WM_REFRESH => {
            // 设置页里改的 `[listener] *` 要真的生效：listener 读的是原子快照，
            // 不重拍就得重启（而界面上写着"改完立即生效"）。
            collect::refresh_config();
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                let n = app.snap.lock().map(|s| s.today).unwrap_or(0);
                if let Some(t) = &app.tray {
                    // tooltip 里带上采集状态：暂停时界面上看得出来。
                    let _ = t.set_tooltip(Some(tray_tooltip(n)));
                }
            }
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let x = (l.0 & 0xFFFF) as u16 as i16 as f32;
            let y = ((l.0 >> 16) & 0xFFFF) as u16 as i16 as f32;
            let mut changed = false;
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                changed = app.on_move(hwnd, x, y);
            }
            if changed {
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            // 没有这一条就收不。
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            unsafe {
                let _ = TrackMouseEvent(&mut tme);
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE_MSG => {
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                if app.hover.is_some() {
                    app.hover = None;
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let x = (l.0 & 0xFFFF) as u16 as i16 as f32;
            let y = ((l.0 >> 16) & 0xFFFF) as u16 as i16 as f32;
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                app.on_click(hwnd, x, y);
            }
            LRESULT(0)
        }
        WM_CTLCOLOREDIT => {
            // 原生 EDIT 不认 D2D 主题：文字色/底色只能走这条老路。
            // 值跟着主题走（`ensure_brushes` 每次换主题时把它们与刷子一起重建）。
            let hdc = HDC(w.0 as *mut core::ffi::c_void);
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                unsafe {
                    SetTextColor(hdc, app.edit_fg);
                    SetBkColor(hdc, app.edit_bg_color);
                }
                return LRESULT(app.edit_bg.0 as isize);
            }
            LRESULT(0)
        }
        WM_EDIT_DONE => {
            tracing_note(&format!("收到 WM_EDIT_DONE code={}", w.0));
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                app.finish_edit(hwnd, w.0 == 1);
            }
            LRESULT(0)
        }
        WM_SNIP_START => {
            // WPARAM: 0 = 松手即存（`[hotkey] snip` 那条路），非 0 = 选区后进标注窗。
            snip::start(hwnd, w.0 != 0);
            LRESULT(0)
        }
        WM_SEL_TEST => {
            run_plugin_sel_test(hwnd);
            LRESULT(0)
        }
        WM_WEEKLY_REPORT => {
            ensure_weekly_report();
            LRESULT(0)
        }
        WM_PIN_TEST => {
            open_test_pins(l.0.max(0) as usize);
            LRESULT(0)
        }
        WM_PIN_CLOSE_ALL => {
            close_all_pins();
            LRESULT(0)
        }
        WM_SNIP_NOTE => {
            // 截图刚出完一张（或没出成）：把那一句话说给状态读数。这里不改快照、不重画数据——
            // 截图跟库里的数字无关，动一下反而会把用户正在看的那页刷掉。
            if let Some(line) = SNIP_NOTE.lock().ok().and_then(|mut q| q.take()) {
                // `snip-note` 是给验收脚本找的 ASCII 探针：状态读数那行是 D2D 画的，脚本读不到字，
                // 但"有没有把这句话递给主窗口"这件事必须能在独立进程里判。
                tracing_note(&format!("snip-note {line}"));
                if let Some(app) = unsafe { app_ptr(hwnd) } {
                    app.status = Some(line);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        WM_OP_DONE => {
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                let lines: Vec<String> = app
                    .ops
                    .lock()
                    .map(|mut q| q.drain(..).collect())
                    .unwrap_or_default();
                if let Some(last) = lines.last() {
                    tracing_note(&format!("数据操作：{last}"));
                    app.status = Some(last.clone());
                }
                app.busy = false;
                tracing_note(&format!("OP_DONE 前 scroll={}", app.scroll));
                // 备份/压缩/清理都可能改了库，重读一遍快照。
                app.refresh_now();
                tracing_note(&format!("OP_DONE 后 scroll={}", app.scroll));
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            // 搜索框的 EN_CHANGE：过滤清单。EN_CHANGE = 0x0300。
            let code = (w.0 >> 16) as u16;
            if code == 0x0300 {
                if let Some(app) = unsafe { app_ptr(hwnd) } {
                    let from_search = app
                        .search_edit
                        .map(|se| l.0 as usize == se.0 as usize)
                        .unwrap_or(false);
                    if from_search {
                        let mut buf = [0u16; 128];
                        let n = match app.search_edit {
                            Some(se) => unsafe { GetWindowTextW(se, &mut buf) }.max(0) as usize,
                            None => 0,
                        };
                        app.search = String::from_utf16_lossy(&buf[..n]);
                        app.scroll = 0;
                        unsafe {
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                    }
                }
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            // delta 。
            let delta = ((w.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                // 弹窗打开时滚动归弹窗（它自己没内容可滚，直接吞掉）；
                // 分析/插件的图表页没有可滚动的列表。
                if app.detail.is_some() || (app.view > 2 && app.view != 5 && app.view != 4) {
                    return LRESULT(0);
                }
                if app.edit_target.is_some() {
                    app.finish_edit(hwnd, true);
                }
                // 下拉开着时滚轮是滚选项，不是滚正文。
                if app.pv_select.is_some() {
                    if let Some(sel) = app.pv_select.as_mut() {
                        let visible = sel.options.len().min(SELECT_MAX_VISIBLE);
                        let max = sel.options.len().saturating_sub(visible);
                        let step = if delta > 0 { -1 } else { 1 };
                        let next = (sel.scroll as i32 + step).clamp(0, max as i32) as usize;
                        if next != sel.scroll {
                            sel.scroll = next;
                            unsafe {
                                let _ = InvalidateRect(Some(hwnd), None, false);
                            }
                        }
                    }
                    return LRESULT(0);
                }
                // 插件视图：滚的是视图内容，不是插件清单
                if app.view == 4 {
                    if let Some(name) = app.plugin_open.clone() {
                        let (cw, ch) = app.client_dip(hwnd);
                        let view = app.pm.as_ref().and_then(|pm| {
                            pm.get_plugin(&name).and_then(|p| p.view.as_ref()).cloned()
                        });
                        if let Some(view) = view {
                            // 弹窗开着：滚轮不滚正文（弹窗自己没内容可滚）。
                            if find_modal(&view, app.pv_modal.as_deref()).is_some() {
                                return LRESULT(0);
                            }
                            let area = rect(16.0, 208.0 + 40.0, cw - 32.0, ch - 208.0 - 16.0);
                            let content = plugin_content_height(&view, area);
                            let visible = (ch - 208.0 - 16.0 - 42.0).max(1.0);
                            let max_scroll = (content - visible).max(0.0) as i32;
                            let step = if delta > 0 { -60 } else { 60 };
                            let next = (app.pv_scroll + step).clamp(0, max_scroll);
                            if next != app.pv_scroll {
                                app.pv_scroll = next;
                                unsafe {
                                    let _ = InvalidateRect(Some(hwnd), None, false);
                                }
                            }
                        }
                        return LRESULT(0);
                    }
                }
                let (cw, ch) = app.client_dip(hwnd);
                let g = table_geom(left_card(cw, ch));
                let sg = settings_geom(cw, ch);
                let count = app
                    .snap
                    .lock()
                    .map(|snap| match app.view {
                        4 => app.plugin_rows.len(),
                        5 => settings_items(&snap, &app.search, app.settings_cat).len(),
                        1 => snap.apps.len(),
                        2 => snap.devices.len(),
                        _ => snap.rank.len(),
                    })
                    .unwrap_or(0);
                let max_rows = match app.view {
                    4 => plugin_geom(cw, ch).2,
                    5 => sg.max_rows,
                    _ => g.max_rows,
                };
                let max_scroll = count.saturating_sub(max_rows);
                let step = 3usize;
                let next = if delta > 0 {
                    app.scroll.saturating_sub(step)
                } else {
                    (app.scroll + step).min(max_scroll)
                };
                if next != app.scroll {
                    app.scroll = next;
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
            }
            LRESULT(0)
        }
        // 录入态下焦点被抢走 ⇒ 当场取消。屏上写着"请按下组合"而键打到别的应用，
        // 是这一族里最容易让人以为"程序吃了我一下键"的形状。
        WM_KILLFOCUS => {
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                if app.capturing.take().is_some() {
                    app.status = Some("录入已取消：窗口失去了焦点".to_string());
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
            }
            unsafe { DefWindowProcW(hwnd, msg, w, l) }
        }
        // 录入态要接住 Alt 组合：带 Alt 的那一击系统发的是 `WM_SYSKEYDOWN`，
        // 只接 `WM_KEYDOWN` 的话"ctrl+alt+f1"永远录不出来（而且 Alt 还会顺手叫出菜单）。
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let handled = unsafe {
                match app_ptr(hwnd) {
                    Some(app) => app.capture_key(hwnd, w.0),
                    None => false,
                }
            };
            if handled {
                return LRESULT(0);
            }
            // Esc：先关弹层；没有弹层就收进托盘（与现有版本同一口径）。
            if w.0 == 0x1B {
                if let Some(app) = unsafe { app_ptr(hwnd) } {
                    if app.detail.is_some() {
                        app.close_detail(hwnd);
                    } else if app.pv_select.take().is_some() {
                        unsafe {
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                    } else if let Some(name) = app.plugin_open.clone() {
                        let view = app.pm.as_ref().and_then(|pm| {
                            pm.get_plugin(&name).and_then(|p| p.view.as_ref()).cloned()
                        });
                        let mut handled = false;
                        if let Some(v) = view.as_ref() {
                            if let Some(m) = find_modal(v, app.pv_modal.as_deref()) {
                                app.close_plugin_modal(hwnd, &name, &m);
                                handled = true;
                            }
                        }
                        if !handled {
                            app.plugin_open = None;
                            app.pv_scroll = 0;
                            app.pv_select = None;
                            app.pv_modal = None;
                            unsafe {
                                let _ = InvalidateRect(Some(hwnd), None, false);
                            }
                        }
                    } else if app.edit_target.is_none() {
                        unsafe {
                            let _ = ShowWindow(hwnd, SW_HIDE);
                        }
                    }
                }
                return LRESULT(0);
            }
            unsafe { DefWindowProcW(hwnd, msg, w, l) }
        }
        WM_CLOSE => {
            // 关窗口 = 收进托盘（真正退出走托盘菜单），与旧版同一口径。
            unsafe {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            LRESULT(0)
        }
        WM_ACCEPTANCE_SHOW if ACCEPTANCE.load(std::sync::atomic::Ordering::Relaxed) => {
            tracing_note("验收消息：唤出窗口");
            unsafe { bring_to_front(hwnd) };
            LRESULT(0)
        }
        WM_SHOW_MAIN => {
            // 悬浮窗上双击。产品行为，不受验收开关约束。
            tracing_note("悬浮窗双击：唤出主窗口");
            unsafe { bring_to_front(hwnd) };
            LRESULT(0)
        }
        WM_ACCEPTANCE_PAUSE if ACCEPTANCE.load(std::sync::atomic::Ordering::Relaxed) => {
            tracing_note("验收消息：切换暂停采集");
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                app.toggle_pause(hwnd);
            }
            LRESULT(0)
        }
        WM_ACCEPTANCE_QUIT if ACCEPTANCE.load(std::sync::atomic::Ordering::Relaxed) => {
            tracing_note("验收消息：退出（走与托盘菜单同一条路）");
            quit_app(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, w, l) },
    }
}

/// window-vibrancy 只收 `raw_window_handle::HasWindowHandle`；原生窗口手里只。
struct HwndWrap(HWND);

impl raw_window_handle::HasWindowHandle for HwndWrap {
    fn window_handle(
        &self,
    ) -> std::result::Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError>
    {
        let raw = raw_window_handle::Win32WindowHandle::new(
            std::num::NonZeroIsize::new(self.0 .0 as isize)
                .ok_or(raw_window_handle::HandleError::Unavailable)?,
        );
        Ok(unsafe {
            raw_window_handle::WindowHandle::borrow_raw(raw_window_handle::RawWindowHandle::Win32(
                raw,
            ))
        })
    }
}

/// 贴系统材质：成功就整窗透明 + 卡片半透明；失败（Win10 / 远程桌面 / 关了透明效果）
/// 就保持不透明深色 —— 与旧 Tauri 版的 `apply_window_vibrancy` 同一口径。
///
/// 注意：让整窗透明只是手段，材质真透得出来才算成功（返回值说的就是这件事）。
fn apply_glass(hwnd: HWND) -> bool {
    // 测试钩子：Win10 / 远程桌面 / 关掉透明效果时拿不到 Mica，走的是不透明那一档。
    // 有了它，自动化才能把那条路也跑一遍 —— 否则"退回档也能看"永远只是一句话。
    if std::env::var("FOCUSFLOW_NO_GLASS").is_ok() {
        tracing_note("FOCUSFLOW_NO_GLASS：强制不走 Mica/DComp，走不透明档");
        apply_title_theme(hwnd);
        return false;
    }
    if window_vibrancy::apply_mica(HwndWrap(hwnd), Some(true)).is_err() {
        tracing_note("Mica 不可用，退回不透明深色");
        apply_title_theme(hwnd);
        return false;
    }
    // 标题栏深浅跟着 `[gui] theme` 走 —— 浅色界面顶着一条深色标题栏是最扎眼的不一致。
    apply_title_theme(hwnd);
    let margins = MARGINS {
        cxLeftWidth: -1,
        cxRightWidth: -1,
        cyTopHeight: -1,
        cyBottomHeight: -1,
    };
    unsafe {
        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
    }
    tracing_note("Mica 已启用");
    true
}

/// 标题栏深浅（DWM 的 immersive dark mode）。
///
/// 贴材质时调一次，主题切换时再调一次：否则浅色界面会顶着一条深色标题栏。
/// 注：这条只影响**非客户区**（标题栏、边框），客户区是自己画的。
fn apply_title_theme(hwnd: HWND) {
    let dark: u32 = u32::from(theme_name().trim() == "dark");
    unsafe {
        let set = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const _ as *const core::ffi::c_void,
            4,
        );
        tracing_note(&format!("标题栏深浅已设置（dark={dark}）：{set:?}"));
    }
}

/// 我们那份图标文件在哪。打包脚本把 `assets/focusflow.ico` 随 exe 一起放；开发期直接跑
/// `target/...` 时 `app_dir()` 也指得回仓库 ⇒ 两个落点都认，但**只写这一遍**：
/// 标题栏、任务栏与托盘要的是同一颗图，路径各解一遍迟早出现"托盘和标题栏不一样"。
fn app_icon_file() -> Option<std::path::PathBuf> {
    [
        focusflow_core::paths::app_dir()
            .join("assets")
            .join("focusflow.ico"),
        std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.join("assets").join("focusflow.ico")))
            .unwrap_or_default(),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

/// 应用图标：优先用程序目录下的 `assets/focusflow.ico`（打包时随 exe 一起放），
/// 没有就退回系统的默认图标——不因为缺一个图标就起不来。
fn load_app_icon() -> HICON {
    unsafe {
        if let Some(p) = app_icon_file() {
            let wide: Vec<u16> = p
                .to_string_lossy()
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            if let Ok(h) = LoadImageW(
                None,
                PCWSTR(wide.as_ptr()),
                IMAGE_ICON,
                0,
                0,
                LR_LOADFROMFILE | LR_DEFAULTSIZE,
            ) {
                return HICON(h.0);
            }
        }
        LoadIconW(None, IDI_APPLICATION).unwrap_or_default()
    }
}

/// 托盘那颗图：读的是与标题栏同一份 `.ico`，按系统的"小图标"尺寸取档
/// （100% 缩放下 16 像素，而那份 `.ico` 里正好有 16 那一档 ⇒ 不是把 32 硬压小）。
/// 读不到才退回代码画的那颗圆点：缺一个图标不该让托盘整个起不来。
fn tray_icon_image() -> std::result::Result<Icon, String> {
    let path = app_icon_file().ok_or_else(|| "找不到 assets/focusflow.ico".to_string())?;
    let px = unsafe { GetSystemMetrics(SM_CXSMICON) };
    let side = if px > 0 { px as u32 } else { 16 };
    Icon::from_path(path, Some((side, side))).map_err(|e| format!("{e}"))
}

fn main() -> windows::core::Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    // 单实例：互斥体活到进程结束（句柄 drop 即关闭，退出时自然释放）。
    let Some(_mutex) = already_running() else {
        return Ok(());
    };

    ACCEPTANCE.store(
        std::env::var("FOCUSFLOW_ACCEPTANCE").as_deref() == Ok("1"),
        std::sync::atomic::Ordering::Relaxed,
    );

    let hinstance = unsafe { GetModuleHandleW(None)? };
    let app_icon = load_app_icon();
    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wndproc),
        hInstance: hinstance.into(),
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? },
        hIcon: app_icon,
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };
    unsafe {
        if RegisterClassW(&wc) == 0 {
            return Err(windows::core::Error::from_win32());
        }
    }

    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            CLASS_NAME,
            w!("FocusFlow"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            40,
            40,
            1200,
            860,
            None,
            None,
            Some(hinstance.into()),
            None,
        )?
    };

    let app = Box::into_raw(Box::new(App::new(hwnd, false)?));
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, app as isize);
        let _ = ShowWindow(hwnd, SW_SHOW);
        // 材质必须在**显示之后**贴：窗口还没显示时 DWM 会把深色属性/背板丢掉。
        (*app).mica = apply_glass(hwnd);
        // 原型期置顶：自动化截图时窗口会被浏览器盖住。产品窗口不置顶（P1 删）。
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
        let _ = SetTimer(Some(hwnd), TIMER_EVENTS, 100, None);
        let _ = UpdateWindow(hwnd);
    }

    // 截图的两条测试钩子：一条直接截固定区域落盘，一条把覆盖层打开等外部截图。
    if let Ok(spec) = std::env::var("FOCUSFLOW_SNIP_TEST") {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1500));
            let mut parts = spec.split(';');
            let rect_part = parts.next().unwrap_or("");
            let out = parts
                .next()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::env::temp_dir().join("focusflow_snip_test.png"));
            let nums: Vec<i32> = rect_part
                .split(',')
                .filter_map(|v| v.trim().parse().ok())
                .collect();
            if nums.len() != 4 {
                tracing_note("截图测试：参数要写成 x,y,w,h;out.png");
                return;
            }
            snip::test_capture_to((nums[0], nums[1], nums[2], nums[3]), &out);
        });
    }
    if let Ok(mode) = std::env::var("FOCUSFLOW_SNIP_OVERLAY") {
        let raw = hwnd.0 as isize;
        // 值写成 annotate 才走标注那条路；其它值（含 1）都是"松手即存"那条。
        let annotate = mode.trim().eq_ignore_ascii_case("annotate");
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1500));
            // 窗口必须建在跑消息循环的线程上，所以这里只投一条消息过去。
            let target = HWND(raw as *mut core::ffi::c_void);
            let _ = unsafe {
                PostMessageW(
                    Some(target),
                    WM_SNIP_START,
                    WPARAM(annotate as usize),
                    LPARAM(0),
                )
            };
        });
    }
    if std::env::var("FOCUSFLOW_PLUGIN_SEL").is_ok() {
        let raw = hwnd.0 as isize;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1500));
            let target = HWND(raw as *mut core::ffi::c_void);
            let _ = unsafe { PostMessageW(Some(target), WM_SEL_TEST, WPARAM(0), LPARAM(0)) };
        });
    }
    // 上周周报：旧版是"每周一 / 启动时"生成上一个完整周。这里在消息循环起来几秒后补一次，
    // 该在的那份已经在就什么都不做（生成走设置页那颗按钮同一条路）。
    {
        let raw = hwnd.0 as isize;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(4000));
            let target = HWND(raw as *mut core::ffi::c_void);
            let _ = unsafe { PostMessageW(Some(target), WM_WEEKLY_REPORT, WPARAM(0), LPARAM(0)) };
        });
    }

    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = KillTimer(Some(hwnd), TIMER_EVENTS);
        drop(Box::from_raw(app));
    }
    Ok(())
}

/// 纵轴刻度与点高是不是同一把尺子。这四条是从旧前端那条真浏览器镜像腿（`chart-axis-harness`）
/// 搬进仓内的：那条腿断言的是**已删前端的副本**，产品改坏了它不会红；这几条抓得到产品。
/// 逐条对得上那条镜像腿自己的判据名：整数刻度、步长相等、顶格不小于最大值、
/// 顶格不整格整格地空让、最高点按刻度反推等于真实最大值、刻度贴在自己的网格线上。
#[cfg(test)]
mod chart_axis_mirror_tests {
    use super::*;

    #[test]
    fn the_axis_top_rounds_up_without_wasting_a_whole_step() {
        assert_eq!(axis_top(0), 4, "一天都没记也不许顶格是 0（反推要除它）");
        assert_eq!(axis_top(1), 4);
        assert_eq!(axis_top(5), 8);
        assert_eq!(axis_top(7), 8);
        assert_eq!(axis_top(100), 100);
        assert_eq!(axis_top(101), 104);
        for max in [1i64, 2, 6, 7, 9, 40, 999, 12_345] {
            let top = axis_top(max);
            assert!(top >= max, "顶格不能低于最大值：max={max} top={top}");
            assert!(top - max < 4, "顶格不许整格整格地空让：max={max} top={top}");
            assert_eq!(
                top % 4,
                0,
                "顶格必须是 4 的倍数，四等分才每档都是整数：top={top}"
            );
        }
        // 天文数字不许绕回：症状是顶格变成负数、整条轴翻过来画。
        assert!(axis_top(i64::MAX) > 0);
    }

    #[test]
    fn the_five_ticks_are_integers_with_an_equal_step() {
        for max in [1i64, 7, 8, 100, 1001] {
            let top = axis_top(max);
            let vals: Vec<i64> = (0..=4).map(|i| line_tick_value(top, i)).collect();
            assert_eq!(vals[0], top, "顶格那条线念的就是顶格");
            assert_eq!(vals[4], 0, "最底那条是 0");
            let steps: Vec<i64> = vals.windows(2).map(|w| w[0] - w[1]).collect();
            assert!(
                steps.iter().all(|s| *s == steps[0]) && steps[0] > 0,
                "等距网格线必须等值：{vals:?}"
            );
            assert_eq!(steps[0], top / 4, "步长就是顶格的四等分：{vals:?}");
        }
    }

    /// 这条镜像腿最有牙的一条：把最高点的高度**按刻度反推**回次数，必须等于真实最大值。
    /// 点高与刻度一旦分家（历史症状：点高按 `峰×1.12`、刻度按 `峰×分数`），这条立刻红。
    #[test]
    fn the_peak_point_reads_back_as_the_peak_count_through_the_ticks() {
        let plot = rect(0.0, 0.0, 300.0, 200.0);
        for max in [1i64, 7, 33, 500, 9_999] {
            let top = axis_top(max);
            let y = line_point_y(plot, top, max);
            let implied = (plot.bottom - y) as f64 / plot_height(plot) as f64 * top as f64;
            // 容差按 f32 往返来定（高度是 f32，反推回来的相对误差在 1e-7 量级），
            // 而这条要抓的是"分母分了两处"那种 12% 级别的分家 —— 相对 1e-5 既不误报，
            // 也留着四个数量级的牙齿。
            assert!(
                (implied - max as f64).abs() <= max as f64 * 1e-5,
                "最高点按刻度反推是 {implied}，真实最大值是 {max} ⇒ 分母分在了两处"
            );
            let top_y = line_grid_y(plot, 0);
            let value_at_top = (plot.bottom - top_y) as f64 / plot_height(plot) as f64 * top as f64;
            assert!(
                (value_at_top - line_tick_value(top, 0) as f64).abs() < 1e-6,
                "顶格那条刻度写着 {}，可那条线对应的是 {value_at_top}",
                line_tick_value(top, 0)
            );
        }
    }

    #[test]
    fn each_tick_label_sits_on_its_own_grid_line() {
        let plot = rect(10.0, 20.0, 200.0, 160.0);
        for i in 0..=4 {
            // 期望值独立写一遍：两边以前各抄一次同一条算式，抄歪一次就飘。
            let want = plot.top + plot_height(plot) * i as f32 / 4.0;
            assert_eq!(
                line_grid_y(plot, i),
                want,
                "第 {i} 条网格线不在自己的位置上"
            );
        }
        assert_eq!(line_grid_y(plot, 4), plot.bottom, "第四条线就是绘图区下沿");
    }

    /// 生产文本的边界：`include_str!` 那批接线守卫都按"第一个 `#[cfg(test)]` 之前"取生产码。
    /// 测试模块一旦插到文件中段，边界就往前挪，后面那些守卫扫到的"生产码"凭空少一截 ——
    /// 症状不是红，而是**少数了几处调用还照样绿**（这一条就是被 such 一次插错位置逼出来的）。
    #[test]
    fn the_prod_scan_boundary_stays_below_the_production_code() {
        let src = include_str!("main.rs");
        let cut = src.find("#[cfg(test)]").expect("找不到测试模块的起点了");
        let prod = &src[..cut];
        for what in [
            "fn draw_table",
            "fn draw_line_chart",
            "fn left_rows",
            "fn gap_note",
            "fn axis_top",
        ] {
            assert!(
                prod.contains(what),
                "{what} 落到了第一个 #[cfg(test)] 后面：有测试模块插在中段，\
                 接线守卫会开始少扫生产码（少扫不会红，只会假绿）"
            );
        }
    }
}

#[cfg(test)]
mod cleanup_readout_tests {
    /// 「按日期清理」这颗按钮不许自己拼结论。同一次清理在命令行说「清理未完成」、
    /// 在按钮上说「已清理 N 条」，就是两把尺子的形状；而那句还漏了单位
    /// （`deleted` 数的是"每天每键一行"，2026-10-26 实测对应 1,166,643 次活动）。
    /// 查的是**调用形态**：句子由 core 的 `cleanup_line` 说一遍，这里只许调用。
    #[test]
    fn the_cleanup_button_defers_to_the_core_sentence() {
        let src = include_str!("main.rs");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod = &src[..cut];
        let local = ["format!(\"", "已清理"].concat();
        assert!(
            !prod.contains(local.as_str()),
            "生产代码里又出现本地拼的那句清理结论了：要走 cleanup_line"
        );
        let call = ["cleanup_line", "("].concat();
        assert_eq!(
            prod.matches(call.as_str()).count(),
            1,
            "只许有那一处调用（按钮），数量变了说明接线动过"
        );
    }
}

/// 「这一轮没读全」在屏上怎么说：每一栏只认领自己那个域名。
#[cfg(test)]
mod gap_note_tests {
    use super::*;

    #[test]
    fn each_chart_claims_only_its_own_domain() {
        let gaps = vec![
            "2025 年库每日趋势读不出来: 库被占用".to_string(),
            "2026 年库每小时分布读不出来: 打不开".to_string(),
        ];
        assert_eq!(
            gap_note(&gaps, queries::DOMAIN_DAILY),
            " · 2025 年没读全",
            "每日那一栏只该说自己那一条，别的栏位的话不许顶到它这儿来"
        );
        assert_eq!(
            gap_note(&gaps, queries::DOMAIN_KEYS),
            "",
            "没轮到它的栏位，一个字都不许出声"
        );
        assert_eq!(
            gap_note(&[], queries::DOMAIN_HOURLY),
            "",
            "读全了就没有后面那一截"
        );
        // 同一栏缺了两个年度库 ⇒ 两个年份都要念出来，不是只报第一个。
        let two = vec![
            "2024 年库每日趋势读不出来: a".to_string(),
            "2025 年库每日趋势读不出来: b".to_string(),
        ];
        assert_eq!(
            gap_note(&two, queries::DOMAIN_DAILY),
            " · 2024 年、2025 年没读全"
        );
    }

    /// 那句"没读全"必须有个长度上界：屏上这几格都是一行，`NO_WRAP` 的字体装不下就会
    /// 画到隔壁卡上去 —— 这一条钉的就是"缺多少年都不会把句子拉长"。
    #[test]
    fn the_note_stays_short_however_many_years_are_missing() {
        let many: Vec<String> = (2000..2030)
            .map(|y| format!("{y} 年库每日趋势读不出来: 原因"))
            .collect();
        let note = gap_note(&many, queries::DOMAIN_DAILY);
        assert_eq!(
            note, " · 2000 年、2001 年、2002 年 等 30 年没读全",
            "念前三个、把总数折成一句，才既说得清又有上界"
        );
        assert!(
            note.chars().count() <= 40,
            "这句长度没有上界，早晚会画出自己那格：{note}"
        );
        // 形状对不上（core 改了那句话）时宁可念原样，也不许把这一条悄悄吞掉。
        let odd = vec!["一整句没有年份形状的话".to_string()];
        assert_eq!(
            gap_note(&odd, "没有的域名"),
            "",
            "按域名认领这一步先于形状：不认领就不出声"
        );
        let odd2 = vec!["2025 年库每日趋势读不出来".to_string()];
        assert_eq!(
            gap_note(&odd2, queries::DOMAIN_DAILY),
            " · 2025 年没读全",
            "没有冒号后面那截原因，也照样念得出年份"
        );
        // 认领这一步用的是"这句话里点过这一栏"，所以域名写在原因位置也一样认得出来。
        let tail = vec!["2025 年库读不出来: 每日趋势这一栏断了".to_string()];
        assert_eq!(
            gap_note(&tail, queries::DOMAIN_DAILY),
            " · 2025 年没读全",
            "认领只看这句话点没点这一栏，压缩只看年份那一段的形状"
        );
        // 年份位置不是数字（core 改了那句话）时念原样的前半截 —— 宁可啰嗦，不许吞掉这一条。
        let odd3 = vec!["前年 年库每日趋势读不出来: x".to_string()];
        assert_eq!(
            gap_note(&odd3, queries::DOMAIN_DAILY),
            " · 前年没读全",
            "形状对不上也要出声，只是说不出是哪一年"
        );
    }

    /// 面板认领的域名必须都在 core 那份清单上，而且必须是"该由面板接"的那几栏。
    /// 新增一条域名而这里没接，红在缺登记那一句；面板自创一个名字，红在第一句。
    #[test]
    fn the_panel_claims_only_registered_domains() {
        let src = include_str!("main.rs");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        // 整行注释先滤掉：这条数的是"代码里真的点了一栏"，文档里提一句函数名不算认领。
        let prod = src[..cut]
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        // 域名常量名与它那个值的对照：core 加一条而这里没登记 ⇒ 下面按名字查表时会红。
        let table = [
            ("DOMAIN_KEYS", queries::DOMAIN_KEYS),
            ("DOMAIN_APPS", queries::DOMAIN_APPS),
            ("DOMAIN_DEVICES", queries::DOMAIN_DEVICES),
            ("DOMAIN_DAILY", queries::DOMAIN_DAILY),
            ("DOMAIN_HOURLY", queries::DOMAIN_HOURLY),
            ("DOMAIN_DAY_HOURS", queries::DOMAIN_DAY_HOURS),
        ];
        let prefix = ["queries::", "DOMAIN_"].concat();
        let mut claimed: Vec<&str> = Vec::new();
        let mut from = 0usize;
        while let Some(i) = prod[from..].find(prefix.as_str()) {
            let after = from + i + prefix.len();
            let mut end = after;
            for (k, b) in prod.as_bytes()[after..].iter().enumerate() {
                if b.is_ascii_alphanumeric() || *b == b'_' {
                    end = after + k + 1;
                } else {
                    break;
                }
            }
            if end == after {
                // 前缀后面接的不是标识符（比如写了个通配），那一次不算认领。
                from = end + 1;
                continue;
            }
            // 名字要连着 `DOMAIN_` 一起取：对照表用的就是常量全名。
            let name = &prod[after - "DOMAIN_".len()..end];
            if !claimed.contains(&name) {
                claimed.push(name);
            }
            from = end;
        }
        assert!(
            !claimed.is_empty(),
            "生产代码里一处域名都没点：这条判据就变成永远绿的空转"
        );
        // 名字 → 值：查不到就是界面点了一个 core 清单上没有的名字（域名分叉）。
        let claimed_values: Vec<&str> = claimed
            .iter()
            .map(|name| {
                let (_, value) = table
                    .iter()
                    .find(|(n, _)| n == name)
                    .unwrap_or_else(|| panic!("面板点了 {name}，可 core 那份清单上查不到它"));
                *value
            })
            .collect();
        for value in claimed_values.iter() {
            assert!(
                queries::PANEL_READ_DOMAINS.contains(value),
                "面板认领了「{value}」，但它不在该由面板接的那一栏清单上"
            );
        }
        // 反向的那一半不许数"域名名字出现过"：`lane` 那个 match 里三个榜的域名本来就在，
        // 就算提示从没照它调过一次，名字也照样全 —— 这条最初就是这么写的，变异跑（把调用
        // 写死成 KEYS）当场证明它骗人。所以这里钉的是**调用形态**。
        for (form, want, why) in [
            (
                ["footer_line(&snap.data_home, &snap.gaps, lane"].concat(),
                1usize,
                "左表那一屏按榜分栏说",
            ),
            (
                ["gap_note(&snap.gaps, queries::DOMAIN_DAILY"].concat(),
                2,
                "趋势图 + 星期分布（后者从每日序列聚合，继承同一句）",
            ),
            (
                ["gap_note(&snap.gaps, queries::DOMAIN_HOURLY"].concat(),
                1,
                "小时图那一栏",
            ),
        ] {
            assert_eq!(
                prod.matches(form.as_str()).count(),
                want,
                "{why}：这个形态少一处，就是有一栏的\"没读全\"再没人调用"
            );
        }
        // 光有 `gap_note(..., lane)` 一句还不够：`lane` 三个分支要是都指同一个域名，
        // 上面那条照样绿，另外两栏却永远不会出声。
        for arm in [
            "1 => queries::DOMAIN_APPS,",
            "2 => queries::DOMAIN_DEVICES,",
            "_ => queries::DOMAIN_KEYS,",
        ] {
            assert!(
                prod.contains(arm),
                "`lane` 里少了 {arm}：三个榜说的是同一栏的话，另外两栏的提示永远不出现"
            );
        }
        // 页脚那句现在是一个真函数（`footer_line`），它的排法由下面那组用例直接钉；
        // 这里只保证画的那一处仍然调它 —— 换回手写格式串，用例就再也看不见屏上那句。
        assert!(
            prod.contains("&footer_line("),
            "页脚不再走 footer_line：那句的排法没人算了，用例也就成了摆设"
        );
    }

    /// 页脚那句的排法：装不下时被截掉的是句尾，所以路径必须在最后、警告必须在最前。
    #[test]
    fn the_footer_puts_the_warning_before_the_path() {
        let path = "E:\\somewhere\\FocusFlow";
        assert_eq!(
            footer_line(path, &[], "按键统计"),
            format!("数据源：{path}"),
            "读全了就只报数据源，一个字都不多加"
        );
        let gaps = vec![
            "2025 年库按键统计读不出来: 打不开".to_string(),
            "2024 年库应用排行读不出来: 打不开".to_string(),
        ];
        let mine = footer_line(path, &gaps, queries::DOMAIN_KEYS);
        assert_eq!(
            mine,
            format!("本栏2025 年没读全 · 2 条读数没读全（上面那些数字是下界） · 数据源：{path}")
        );
        // 别的栏缺了、本栏没缺：只说整屏那句，不许冒认一个年份。
        let other = footer_line(path, &gaps, queries::DOMAIN_APPS);
        assert!(
            other.starts_with("2 条读数没读全"),
            "本栏没缺读时它不该抢那句：{other}"
        );
        // 三段的先后顺序就是这条用例的全部意思。
        let at_note = mine.find("本栏").expect("本栏那句不见了");
        let at_bound = mine.find("下界").expect("下界那句不见了");
        let at_path = mine.find("数据源：").expect("数据源不见了");
        assert!(
            at_note < at_bound && at_bound < at_path,
            "页脚排成了：{mine}"
        );
    }
}

/// 三张图上那枚悬停读数的命中判据：画图与命中测试必须算同一份几何。
///
/// 两条腿都在 —— 命中的下标，与**没命中**（区外、柱与柱之间那段空档）。只断"命中"的话，
/// "整屏什么时候都算命中"那种写法照样能蒙过去。
#[cfg(test)]
mod chart_hover_tests {
    use super::*;

    fn plot() -> D2D_RECT_F {
        rect(0.0, 0.0, 100.0, 50.0)
    }

    #[test]
    fn the_line_picks_the_nearest_point_and_nothing_outside() {
        let p = plot();
        // 三个点落在 0 / 50 / 100：这条除法与画线用的是同一个函数。
        assert_eq!(line_point_x(p, 3, 1), 50.0);
        assert_eq!(line_nearest(p, 3, 0.0, 25.0), Some(0));
        assert_eq!(
            line_nearest(p, 3, 49.0, 25.0),
            Some(1),
            "差一点就该归最近那一个"
        );
        assert_eq!(line_nearest(p, 3, 100.0, 25.0), Some(2));
        assert_eq!(line_nearest(p, 3, 50.0, -1.0), None, "绘图区上面不接");
        assert_eq!(line_nearest(p, 3, 50.0, 51.0), None, "绘图区下面不接");
        assert_eq!(line_nearest(p, 3, 101.0, 25.0), None, "右边轴外不接");
        assert_eq!(
            line_nearest(p, 0, 50.0, 25.0),
            None,
            "序列是空的，什么都不能命中"
        );
        // 「今日」那一档只有一个点：落在正中，左右都算它。
        assert_eq!(line_point_x(p, 1, 0), 50.0);
        assert_eq!(line_nearest(p, 1, 10.0, 25.0), Some(0));
    }

    #[test]
    fn the_hourly_bars_answer_and_the_gap_between_them_does_not() {
        let p = plot();
        // 四根柱、空档 3 ⇒ 柱宽 (100 - 3×3) / 4 = 22.75（这一批数都是精确的二进制值）。
        assert_eq!(bar_slot(p, 4, 1, HOURLY_GAP), (25.75, 22.75));
        assert_eq!(slot_nearest(p, 4, HOURLY_GAP, 10.0, 25.0), Some(0));
        assert_eq!(slot_nearest(p, 4, HOURLY_GAP, 30.0, 25.0), Some(1));
        assert_eq!(slot_nearest(p, 4, HOURLY_GAP, 99.0, 25.0), Some(3));
        assert_eq!(
            slot_nearest(p, 4, HOURLY_GAP, 24.0, 25.0),
            None,
            "柱画到 22.75 就停了，24 落在空档里：不许并进左边那根"
        );
        assert_eq!(
            slot_nearest(p, 4, HOURLY_GAP, 50.0, 60.0),
            None,
            "轴下方不接"
        );
        assert_eq!(slot_nearest(p, 0, HOURLY_GAP, 10.0, 25.0), None);
    }

    /// 接线腿：这个下标真的走到了画图那两处，而且每一处"这一屏没有图"的分支都撤了它。
    /// 漏撤的那一屏会亮着一格谁都没指的读数 —— 那比没有读数更糟。
    #[test]
    fn the_hover_state_reaches_the_draw_and_is_reset_wherever_it_cannot_apply() {
        let src = include_str!("main.rs");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod = src[..cut]
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let to_draw = ["vs.", "chart_hover"].concat();
        assert_eq!(
            prod.matches(to_draw.as_str()).count(),
            2,
            "趋势与小时两处画图都读这个下标，少一处就是那张图接不上"
        );
        assert_eq!(
            prod.matches(["self.chart_hover = idx"].concat().as_str())
                .count(),
            1,
            "只有一个地方写它（鼠标那一问）"
        );
        assert_eq!(
            prod.matches(["self.chart_hover.", "take()"].concat().as_str())
                .count(),
            5,
            "弹窗 / 热力 / 插件 / 设置 / 排行这五处早退各撤一次：新增一屏时要跟着补"
        );
        assert_eq!(
            prod.matches(["self.chart_hover = None"].concat().as_str())
                .count(),
            3,
            "换视图、换子页签、换周期这三处必须当场清掉"
        );
    }
}

#[cfg(test)]
mod snapshot_rate_tests {
    use super::*;

    /// 刷新间隔的取值：正常值原样用；笔误那一支退回默认。放任 0 是"每秒狂转"，
    /// 放任天文数字是"数字再也不动"——两种都比按默认值刷新更糟，而且看着像程序坏了。
    #[test]
    fn the_refresh_interval_clamps_instead_of_freezing_the_ui() {
        assert_eq!(snapshot_sleep_secs(7), 7);
        assert_eq!(snapshot_sleep_secs(1), 1);
        assert_eq!(snapshot_sleep_secs(600), 600);
        assert_eq!(
            snapshot_sleep_secs(0),
            DEFAULT_SNAPSHOT_SECS,
            "写 0 是笔误，不是「每秒都不停」"
        );
        assert_eq!(snapshot_sleep_secs(-3), DEFAULT_SNAPSHOT_SECS);
        assert_eq!(
            snapshot_sleep_secs(999_999),
            DEFAULT_SNAPSHOT_SECS,
            "写天文数字的症状是数字再也不动"
        );
    }

    /// 分档：短档（今日 / 7 / 15 / 30 天）按基准，长档（1 年 / 总计）按基准 × 3。
    ///
    /// ⚠ 这一条钉的是 `PERIODS` **现有六格**各自落在哪一档；新增一格会走短档那一支而这条
    /// 不会红 —— 所以同一条里另钉了格子数，多一颗胶囊时必须回来点一次档。
    #[test]
    fn the_long_periods_tick_three_times_slower() {
        assert_eq!(
            PERIODS.len(),
            6,
            "周期胶囊多了一格：这条用例里的分档期望要跟着补一行"
        );
        for (period, label) in PERIODS.iter() {
            let long = *period == 0 || *period >= 365;
            let want = if long { 3_000 } else { 1_000 };
            assert_eq!(
                snapshot_tick_ms(1, *period),
                want,
                "「{label}」这一档（period={period}）的节拍归错了"
            );
        }
        // 基准仍然是用户写的那一行，倍率是乘在它上面而不是把它吃掉
        assert_eq!(snapshot_tick_ms(2, -1), 2_000);
        assert_eq!(snapshot_tick_ms(2, 0), 6_000);
        // 笔误那一支：基准退回默认，倍率照乘（"写错一个数"不该变成"再也不刷新"）
        assert_eq!(
            snapshot_tick_ms(0, 0),
            DEFAULT_SNAPSHOT_SECS * LONG_PERIOD_TICKS * 1000,
            "写 0 是笔误，退回默认基准再按长档乘"
        );
        assert_eq!(snapshot_tick_ms(-3, -1), DEFAULT_SNAPSHOT_SECS * 1000);
    }

    /// 接线腿：那一拍真的睡在**分档之后**的毫秒上。钉的是调用形态而不是函数名 ——
    /// 只数 `snapshot_sleep_secs(` 出现几次的话，"函数写了、循环仍旧按老的秒数睡"这种
    /// 接法照样能蒙过去。
    #[test]
    fn the_snapshot_loop_sleeps_on_the_tiered_tick() {
        let src = include_str!("main.rs");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod = &src[..cut];
        let key = ["\"", "active_refresh_interval", "\""].concat();
        assert_eq!(
            prod.matches(key.as_str()).count(),
            1,
            "生产代码里读这一键的位置应当只有快照线程那一处"
        );
        let tick = ["snapshot_tick_ms", "("].concat();
        assert_eq!(
            prod.matches(tick.as_str()).count(),
            2,
            "一处定义 + 循环里一处调用，数量变了说明接线动过"
        );
        let sleeps_on = ["Duration::from_millis(", "tick_ms", ")"].concat();
        assert_eq!(
            prod.matches(sleeps_on.as_str()).count(),
            1,
            "那一拍必须睡在分档出来的毫秒上"
        );
        let stale = ["Duration::from_secs(", "secs", ")"].concat();
        assert!(
            !prod.contains(stale.as_str()),
            "循环里还留着按秒睡的那一句，说明分档没真的接上"
        );
        let base = ["snapshot_sleep_secs", "("].concat();
        assert_eq!(
            prod.matches(base.as_str()).count(),
            2,
            "一处定义 + 分档函数里一处调用（循环不再直接用它）"
        );
    }

    /// 三个读者都不在 ⇒ 那一拍不查。这条判据写歪的两种症状方向相反、都不好抓：
    /// 多认一个"看得见"⇒ 收进托盘还在空转（省不下任何东西）；少认一个 ⇒ 卡片开着却不更新
    /// （数字冻住，比慢更糟）。所以五种组合逐个钉。
    #[test]
    fn polling_stops_only_when_nobody_is_watching() {
        assert!(watching(true, false, false), "主窗口开着就要查");
        assert!(watching(true, false, true), "窗口与卡片都开着");
        assert!(
            watching(false, false, true),
            "主窗口收进托盘、卡片还开着 ⇒ 仍然要查（卡片是直接看的那个）"
        );
        assert!(!watching(true, true, false), "最小化＝没人看");
        assert!(!watching(false, false, false), "隐藏且卡片关着＝一拍都不查");
    }

    /// 接线腿：那把判据真的被快照线程用上了，而且**睡完还要再问一次**（睡这一拍的期间
    /// 窗口可能又被藏起来，那就该把这批查询省掉）。只在纯函数里算而循环不读 ⇒ 等于没做。
    #[test]
    fn the_snapshot_loop_asks_whether_anyone_is_watching() {
        let src = include_str!("main.rs");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod = &src[..cut];
        let call = ["watching_now", "("].concat();
        assert_eq!(
            prod.matches(call.as_str()).count(),
            3,
            "一处定义 + 循环里两处（睡前、睡完）——数量变了说明接线动过"
        );
    }

    /// 卡片那一拍不能只等它自己的每秒定时器：推完数据要**投一条重画消息**。
    /// 摘掉这一句的症状是"数字平均慢半秒到一秒"，而它在任何单元测试里都看不出来
    /// （缓存照样被写），所以钉的是接线形状：每一处推快照，紧跟一次催画。
    #[test]
    fn every_snapshot_push_nudges_the_pill_to_repaint() {
        let src = include_str!("main.rs");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod = &src[..cut];
        let push = ["publish_floating(&", "next"].concat();
        let nudge = ["floating::refresh", "("].concat();
        let mut at = 0;
        let mut sites = 0;
        while let Some(i) = prod[at..].find(push.as_str()) {
            sites += 1;
            let from = at + i + push.len();
            // 按字符取窗口：按字节切可能正好落在一个汉字的中间，那会直接 panic。
            let window: String = prod[from..].chars().take(900).collect();
            assert!(
                window.contains(nudge.as_str()),
                "第 {sites} 处推完快照没有催卡片重画：卡片会等到自己那一秒，数字看着就是慢"
            );
            at = from;
        }
        assert_eq!(
            sites, 2,
            "推快照的落点应是「快照线程那一拍」与「refresh_now」两处，数量变了要来看这条"
        );
    }
}

#[cfg(test)]
mod settings_type_tests {
    /// 设置页这一整套字号是照旧 WebView `desktop/ui/styles.css` 逐项量出来的（见 `Formats`
    /// 上那段注释）。这里守的不是像素，而是"别再借全局那两档"：以前整页画的是 `body`
    /// 12.5/400 与 `small` 11/400，比旧版小一号又细一号——借档在源码里不留任何痕迹，
    /// 只在屏上看得见，所以拿源码范围钉住。
    #[test]
    fn the_settings_page_draws_with_its_own_scale() {
        let src = include_str!("main.rs");
        let from = src
            .find("fn draw_settings(")
            .expect("设置页的绘制入口找不到了");
        let rest = &src[from..];
        let to = from
            + rest
                .find("\nfn draw_detail(")
                .expect("设置页的终点（下一个函数）找不到了");
        let page = &src[from..to];
        for slot in ["settings_h", "row_name", "text12", "text13_em", "center13"] {
            let wired = ["&p.fmt.", slot].concat();
            assert!(page.contains(wired.as_str()), "这一档没接上：{wired}");
        }
        for borrowed in ["&p.fmt.body", "&p.fmt.center_small"] {
            assert!(
                !page.contains(borrowed),
                "设置页又借了全局那一档 {borrowed}：那一档比旧版小一号，主界面也在用，不该跟着动"
            );
        }
    }

    /// 同一个字体栈要两处一起改。D2D 那几档打头是 Segoe UI（中文由 DirectWrite 回退到雅黑），
    /// 而叠在设置页上的原生 EDIT（搜索框、值框）走的是 GDI，得单独喂一把 HFONT：
    /// 它原先直接写雅黑，于是同一行里框外的数字与框里敲进去的数字是两种字重。
    #[test]
    fn the_edit_font_shares_the_directwrite_family() {
        let src = include_str!("main.rs");
        let from = src
            .find("let ui_font = CreateFontW(")
            .expect("主窗口那把 EDIT 字体的构造点找不到了");
        let rest = &src[from..];
        let to = rest.find(");").expect("那把字体的构造没收尾");
        let block = &rest[..to];
        assert!(
            block.contains("Segoe UI"),
            "EDIT 要用与 D2D 同一开头的字体栈"
        );
        assert!(
            !block.contains("Microsoft YaHei UI"),
            "EDIT 又退回整段雅黑了：数字与拉丁字母会比框外的同档字粗一号"
        );
    }
}

#[cfg(test)]
mod hotkey_tests {
    use super::*;

    /// 「他点过叉」与「这台机器没配过这一条」必须是两件事。
    /// 混起来的症状很难看：全新安装里 `[hotkey]` 一行都没写，若按"值是空＝不注册"读，
    /// 三条热键一条都不会挂上，而屏上连"未启用"都显示不出来（他没做过任何操作）。
    #[test]
    fn an_emptied_line_is_not_the_same_as_a_missing_one() {
        assert!(hotkey_off_of(true, ""), "有这一行且空 ⇒ 不注册");
        assert!(hotkey_off_of(true, "   "), "只有空白也算空");
        assert!(!hotkey_off_of(true, "ctrl+shift+f"), "有值 ⇒ 照值注册");
        assert!(
            !hotkey_off_of(false, ""),
            "文件里没这一行 ⇒ 用默认值，不是停用"
        );
    }

    /// 小钮只该挂在那三条绑定上：其它热键段的键（`enabled`）与别的段的行都不许有。
    /// 挂错行的症状是"点了叉把某条不相干的配置清空了"。
    #[test]
    fn only_the_three_bindings_get_the_button() {
        assert_eq!(hotkey_slot("hotkey", "toggle_window"), Some(0));
        assert_eq!(hotkey_slot("hotkey", "snip"), Some(1));
        assert_eq!(hotkey_slot("hotkey", "snip_annotate"), Some(2));
        assert_eq!(hotkey_slot("hotkey", "enabled"), None, "总开关不是绑定");
        assert_eq!(hotkey_slot("gui", "toggle_window"), None, "别的段不算");
    }

    /// 那颗钮必须还在值框**里面**：画与命中虽然共用一个函数，但函数本身写错
    /// （比如跑到框外）时两处会一起错，只有几何断言能逮住。
    #[test]
    fn the_button_stays_inside_its_value_box() {
        let vb = rect(720.0, 100.0, 150.0, 26.0);
        let b = hotkey_btn_rect(vb);
        assert!(
            b.left >= vb.left && b.right <= vb.right,
            "小钮跑出值框了：{b:?}"
        );
        assert!(
            b.top >= vb.top && b.bottom <= vb.bottom,
            "小钮跑出值框了：{b:?}"
        );
        assert!(b.left - vb.left >= 60.0, "小钮不许压住框里那几个字");
    }

    /// 录入侧生成的每一种串，都必须能被注册侧那台解析器认回来。
    /// `key_name` 那张表是手抄的，抄错一个名字的症状是"录进去一个注册不上的串"，
    /// 而那只有在他按下那一键之后才看得见 ⇒ 拿全量枚举钉，不挑几个样本。
    #[test]
    fn every_generated_combo_parses_back() {
        let mut n = 0;
        let mut bad: Vec<String> = Vec::new();
        // 修饰位固定按 ctrl：要验的是**主键名字**抄得对不对，16 种修饰组合只会把同一处
        // 错报 16 遍。修饰那四个写法由 `combo_from_key` 那条次序断言看着。
        for vk in 0x08..=0xDEu32 {
            let Some(s) = combo_from_key(vk, true, false, false, false) else {
                continue;
            };
            n += 1;
            if parse_combo(&s).is_none() {
                bad.push(s);
            }
            // 加上 alt/shift/super 那一圈也要能解析（名字表里 ctrl 与它们拼出来的串不同形）。
            for mods in 1..16u8 {
                if let Some(t) = combo_from_key(
                    vk,
                    mods & 1 != 0,
                    mods & 2 != 0,
                    mods & 4 != 0,
                    mods & 8 != 0,
                ) {
                    if parse_combo(&t).is_none() {
                        bad.push(t);
                    }
                }
            }
        }
        // 一次跑完把**所有**对不上的名字念出来：逐条 assert 只露出第一个，而手抄那张表与
        // 解析器名字表之间的差异是一批，不是一个。
        bad.sort();
        bad.dedup();
        assert!(bad.is_empty(), "这些生成的串解析不回来：{bad:?}");
        assert!(
            n > 60,
            "只枚举到 {n} 个可录主键，像是 key_name 那张表没被走到"
        );
    }

    /// 三种"不该录上"的形态：裸键（会把那个键在整个系统里吞掉）、只按修饰键（还在等）、
    /// Esc（它是取消录入那个手势）。外加一条正向锚：顺序必须是 ctrl+alt+shift+win+主键。
    #[test]
    fn capture_refuses_bare_keys_modifiers_and_esc() {
        assert!(
            combo_from_key(0x41, false, false, false, false).is_none(),
            "裸 A 不许当全局热键"
        );
        assert!(
            combo_from_key(0x11, true, false, false, false).is_none(),
            "只按 Ctrl 不算一击（哪怕同时按下了 Ctrl 自己）"
        );
        assert!(
            combo_from_key(0x1B, true, true, true, true).is_none(),
            "Esc 永远不当主键"
        );
        assert_eq!(
            combo_from_key(0x70, true, true, true, false).as_deref(),
            Some("ctrl+alt+shift+f1"),
            "次序漂了屏上、文件里、日志里就要多出一把对不齐的账"
        );
        assert_eq!(capture_refusal(0x11), Some("只按修饰键不算一个组合"));
        assert_eq!(capture_refusal(0x1B), None, "Esc 是取消，不是拒绝");
    }

    /// 那一格跟着**注册结果**走：三条里有一条真在占全局键，显示就该是"开"。
    /// 以前它只数了唤出与截图两条 —— 只有标注那条注册上时屏上写"关"，而那颗键确实在被占用。
    #[test]
    fn the_row_follows_what_actually_registered() {
        let one = parse_combo("shift+f1");
        assert!(one.is_some(), "夹具那串组合要能解析");
        assert!(
            !hotkeys_on_after_boot([&None, &None, &None]),
            "一条都没注册上就该显示关"
        );
        assert!(
            hotkeys_on_after_boot([&None, &None, &one]),
            "只有标注那条注册上也得显示开：这一格以前漏算了第三条"
        );
        assert!(hotkeys_on_after_boot([&one, &None, &None]));
        assert!(hotkeys_on_after_boot([&None, &one, &None]));
    }

    /// 总开关决定开机挂哪几条：关掉 ⇒ 一条都不占全局键；打开 ⇒ 三条，而且顺序就是
    /// `make_hotkeys` 里落进 toggle / snip / annotate 三个字段的顺序（下标对应，错一位的
    /// 症状是"按截图键把主窗口唤出来了"）。
    #[test]
    fn the_switch_decides_which_combos_boot() {
        assert!(combo_keys(false).is_empty(), "开关关掉就该一条都不注册");
        assert_eq!(
            combo_keys(true),
            &COMBO_KEYS[..],
            "开机清单与顺序：0=唤出、1=松手即存、2=进标注"
        );
    }

    /// 总开关的默认值必须是"开"。这条以前写的是 `false`，而那时启动侧没人读它，
    /// 所以谁都没察觉；现在它被读了，默认值就直接决定新装的人按不按得出热键。
    #[test]
    fn the_hotkey_switch_defaults_to_on() {
        let defaults = focusflow_core::config::default_config();
        let enabled = defaults
            .get("hotkey")
            .and_then(|h| h.get("enabled"))
            .map(|s| s.as_str());
        assert_eq!(
            enabled,
            Some("true"),
            "[hotkey] enabled 的默认值不是开 ⇒ 随包配置里没有这一行的人一装好就没有热键"
        );
    }

    /// 接线腿：开机与运行中重注册两条路必须共用 `combo_keys` 那一枚清单，不许再在调用处
    /// 把键名写成字面量——抄漏一条的症状是"开关明明关掉了，还剩一条键在占全局"。
    #[test]
    fn both_hotkey_paths_share_one_combo_list() {
        let src = include_str!("main.rs");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod = &src[..cut];
        let call = ["combo_keys", "("].concat();
        assert_eq!(
            prod.matches(call.as_str()).count(),
            3,
            "一处定义 + 开机与重注册两处调用，数量变了说明接线动过"
        );
        // 调用形态这一半查的是"每一处注册调用都从清单里取 key"：写字面量的形状＝某一条
        // 不受总开关管，而编译器与上面的数量判据都不会响（第一版按"整个生产段里
        // `snip_annotate` 只许出现一次"来查，量到 4 次——设置页那几行也在按名字读配置，
        // 那条判据根本不是它要防的东西，改成逐行看调用）。
        let reg = ["register_combo", "("].concat();
        let mut calls = 0;
        for line in prod.lines() {
            let t = line.trim_start();
            if !t.contains(reg.as_str()) {
                continue;
            }
            if t.starts_with("fn ") {
                continue; // 定义那一行不算调用
            }
            calls += 1;
            assert!(
                t.contains("key") && !t.contains('"'),
                "注册调用必须递进清单里的那枚 key，不许写死名字：{t}"
            );
        }
        assert_eq!(
            calls, 2,
            "开机与运行中重注册各一处调用，数量变了说明接线动过"
        );
    }

    /// 设置页里显示的就是核心默认值，注册时解析的也是它：默认值解析不了 = 一开机就分叉。
    #[test]
    fn the_core_defaults_parse_into_hotkeys() {
        let defaults = focusflow_core::config::default_config();
        let hotkey = defaults.get("hotkey").expect("核心默认里有 [hotkey] 段");
        for key in ["toggle_window", "snip", "snip_annotate"] {
            let raw = hotkey
                .get(key)
                .unwrap_or_else(|| panic!("[hotkey] 默认缺 {key}"));
            assert!(
                parse_combo(raw).is_some(),
                "默认组合 {key} = {raw:?} 解析不了，设置页显示的值和注册的键会分叉"
            );
        }
    }

    /// 判重按解析结果比，不按字符串：换个写法（control / ctrl、多加空格）也是同一条键。
    #[test]
    fn the_same_combo_is_detected_across_spellings() {
        assert_eq!(
            parse_combo("ctrl+shift+f"),
            parse_combo("control + shift + f"),
            "ctrl 与 control 应当算同一个修饰键"
        );
        let why = combo_check(
            "control+shift+f",
            &[
                ("唤出窗口热键", "ctrl+shift+f"),
                ("标注截图热键", "shift+f2"),
            ],
        )
        .expect("换写法躲不掉判重");
        assert!(why.contains("占用"), "要说清被谁占用：{why}");
        assert!(why.contains("唤出窗口热键"), "要点名另一条的中文名：{why}");
    }

    #[test]
    fn two_different_combos_are_accepted() {
        assert_eq!(
            combo_check(
                "shift+f2",
                &[
                    ("唤出窗口热键", "ctrl+shift+f"),
                    ("区域截图热键", "shift+f1")
                ]
            ),
            None
        );
        assert_eq!(
            combo_check(
                "ctrl+alt+9",
                &[
                    ("唤出窗口热键", "ctrl+shift+f"),
                    ("区域截图热键", "shift+f1")
                ]
            ),
            None
        );
        // 三个修饰键也算一个合法组合，别把它当坏值拒了。
        assert!(parse_combo("ctrl+shift+alt+f").is_some());
    }

    /// 裸键能解析成 HotKey，但注册上去会在整个系统里吞掉那个键，必须当坏值拒掉。
    #[test]
    fn a_bare_key_is_refused_and_explained() {
        assert_eq!(parse_combo("f1"), None, "裸键 f1 不该当成可用的全局热键");
        for bad in [
            "",
            "   ",
            "f1",
            "ctrl+shift",
            "ctrl++f",
            "shift+f1+f2",
            "乱码",
        ] {
            let why = combo_check(bad, &[("区域截图热键", "shift+f1")])
                .unwrap_or_else(|| panic!("{bad:?} 应该被拒"));
            assert!(
                why.contains("修饰键"),
                "{bad:?} 的说明要给能照写的格式：{why}"
            );
        }
    }
}

#[cfg(test)]
mod boot_notice_tests {
    use super::*;

    fn say(tag: &str) -> Option<String> {
        Some(tag.to_string())
    }

    /// 优先级整条逐格走一遍：每一句都要能在"上面都没话"时顶上，也要能在"上面有话"时让位。
    /// 只断言头尾两格的写法照不出中间换位（把两个参数对调，它照样绿）。
    #[test]
    fn the_boot_line_says_only_the_most_urgent_thing() {
        let all = || {
            (
                say("搬不成"),
                say("配置没读出来"),
                say("数据目录回落"),
                say("两处采集"),
                say("搬完了"),
                say("热键没注册上"),
            )
        };
        let (a, b, c, d, e, f) = all();
        assert_eq!(
            boot_notice(
                a.clone(),
                b.clone(),
                c.clone(),
                d.clone(),
                e.clone(),
                f.clone()
            ),
            a
        );
        assert_eq!(
            boot_notice(None, b.clone(), c.clone(), d.clone(), e.clone(), f.clone()),
            b
        );
        assert_eq!(
            boot_notice(None, None, c.clone(), d.clone(), e.clone(), f.clone()),
            c
        );
        assert_eq!(
            boot_notice(None, None, None, d.clone(), e.clone(), f.clone()),
            d
        );
        assert_eq!(boot_notice(None, None, None, None, e.clone(), f.clone()), e);
        assert_eq!(boot_notice(None, None, None, None, None, f.clone()), f);
        assert_eq!(
            boot_notice(None, None, None, None, None, None),
            None,
            "一切正常时这一格该是空的"
        );
    }

    /// 接线腿：那两句降级得真的走进这一格。纯函数排好位而调用方不喂，屏上依旧零信号 ——
    /// 这条缺陷的形状正是"日志里说了，屏上没说"，所以钉的是调用形态而不是函数名。
    #[test]
    fn the_boot_line_is_fed_both_silent_degradations() {
        let src = include_str!("main.rs");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod = &src[..cut];
        // 钉"调用形态"而不是函数名：光找 `boot_notice(` 会先撞上那句 `fn boot_notice(`
        // 的定义，把定义自身的参数表当成调用点读一遍（假红）。
        let call = ["= ", "boot_notice", "("].concat();
        let at = prod
            .find(call.as_str())
            .expect("生产代码里没有启动状态行的调用点");
        // 只看这一处调用的参数区：整文件 contains 会让别处同名的词顶班。
        // 按字符取窗口，不按字节 —— 这一段里全是中文，`&prod[at..at + 900]` 会切进
        // 一个汉字的中间直接 panic（本机第一次跑就这样红在自己身上）。
        let args: String = prod[at..].chars().take(1200).collect();
        for needle in [
            ["corrupt_note", "()"].concat(),
            ["data_home_fallback_notice", "()"].concat(),
        ] {
            assert!(
                args.contains(needle.as_str()),
                "启动状态行没把这句降级喂进去（缺 {needle}）"
            );
        }
    }
}

#[cfg(test)]
mod weekly_report_tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    /// 周一边界最容易判错：周一当天要的是"刚过去的那一周"（上周一），不是今天。
    #[test]
    fn last_week_starts_on_monday_of_the_previous_week() {
        let cases = [
            (d(2026, 10, 26), d(2026, 10, 19)), // 周一 → 上周一
            (d(2026, 10, 27), d(2026, 10, 19)), // 周二
            (d(2026, 11, 1), d(2026, 10, 19)),  // 周日（本周还没过完）
            (d(2026, 11, 2), d(2026, 10, 26)),  // 跨月的下一个周一
            (d(2027, 1, 1), d(2026, 12, 21)),   // 跨年
        ];
        for (today, want) in cases {
            assert_eq!(last_week_start(today), want, "{today} 的上周一算错");
        }
    }

    /// 文件名里的日期就是上周一：手动生成与"要不要自动生成"共用这一份算法。
    #[test]
    fn the_report_file_name_carries_the_week_start_date() {
        let path = weekly_report_path(d(2026, 10, 26));
        assert!(
            path.ends_with("weekly-2026-10-19.md"),
            "实际：{}",
            path.display()
        );
        assert!(path.to_string_lossy().contains("reports"));
    }
}

#[cfg(test)]
mod data_move_tests {
    use super::*;

    /// 那两行的顺序是"先待搬运标记、后新数据根"。反过来的半态是"目录已改、标记没写"：
    /// 新进程按空的新目录起步，旧历史永远不会再被搬 —— 这一条不可逆，所以钉住顺序。
    #[test]
    fn the_marker_is_written_before_the_new_home() {
        let writes = data_move_writes(
            std::path::Path::new("E:/ff/old"),
            std::path::Path::new("E:/ff/new"),
        );
        for (section, _, _) in writes.iter() {
            assert_eq!(section, &"paths", "两行都写在 [paths] 段");
        }
        assert_eq!(writes[0].1, "data_migrate_from", "第一行必须是待搬运标记");
        assert_eq!(writes[0].2, "E:/ff/old", "标记记的是搬走的来源");
        assert_eq!(writes[1].1, "data_home", "第二行才是新的数据根");
        assert_eq!(writes[1].2, "E:/ff/new");
    }
}

#[cfg(test)]
mod plugin_sel_tests {
    use super::*;

    fn map(pairs: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(g, ids)| {
                (
                    g.to_string(),
                    ids.iter().map(|s| s.to_string()).collect::<Vec<String>>(),
                )
            })
            .collect()
    }

    /// 不分组 = 多选（点第二行留两行、再点第一行取消一行），分组 = 单选（只留最后点的）。
    #[test]
    fn ungrouped_toggles_and_grouped_replaces() {
        assert_eq!(apply_row_select("", &[], "1"), ["1"].to_vec());
        let two = apply_row_select("", &["1".to_string()], "2");
        assert_eq!(
            two,
            ["1", "2"].to_vec(),
            "多选按点击顺序留着，拼 id 要用这个顺序"
        );
        assert_eq!(
            apply_row_select("", &two, "1"),
            ["2"].to_vec(),
            "再点已选中的行是取消"
        );
        assert_eq!(
            apply_row_select("mcat", &["a".to_string(), "b".to_string()], "c"),
            ["c"].to_vec(),
            "分组表格是单选"
        );
    }

    /// 选中集只属于自己那一组：另一组的 id 不该混进拼出来的动作 id。
    #[test]
    fn each_group_reads_only_its_own_selection() {
        let sel = map(&[("mcat", &["食品饮料"]), ("msub", &["零食"])]);
        assert_eq!(
            sel_action_id("m_edit_cat_sel", "mcat", &sel).unwrap(),
            "m_edit_cat_sel食品饮料"
        );
        assert_eq!(
            sel_action_id("m_del_sub_sel", "msub", &sel).unwrap(),
            "m_del_sub_sel零食"
        );
        // 不分组那一格（旧版前端的 __main）现在是空的 ⇒ 不许发按钮
        let err = sel_action_id("del_", "", &sel).unwrap_err();
        assert!(err.contains("点选一行"), "要说清下一步做什么：{err}");
    }

    /// 多选拼逗号（插件按 `gmatch("([^,]+)")` 拆）；只吃一条的按钮选多了要拦下来，
    /// 不能把 "1,2" 发给 `tonumber` —— 那是"点了没反应"的老症状。
    #[test]
    fn multi_ids_join_with_commas_and_single_only_buttons_are_blocked() {
        let sel = map(&[("", &["42", "43"])]);
        assert_eq!(sel_action_id("del_", "", &sel).unwrap(), "del_42,43");
        assert_eq!(sel_action_id("days_", "", &sel).unwrap(), "days_42,43");
        let why = sel_action_id("edit_", "", &sel).unwrap_err();
        assert!(why.contains("只改一条"), "{why}");
        let one = map(&[("", &["42"])]);
        assert_eq!(sel_action_id("edit_", "", &one).unwrap(), "edit_42");
    }
}

/// 写「用户挑的路径」那两条路（导出 CSV/HTML/JSON、生成周报）必须走原子替换。
/// 回归场景正是 `atomic_write` 头上那段说的：目标上已经躺着上一周的报告，直接写会先把
/// 旧的截断，中途失败就只剩半截文件而旧的也没了 —— 重写成原生时这两处掉回了直接写。
#[cfg(test)]
mod user_file_write_tests {
    use super::write_user_text;

    fn sandbox(name: &str) -> std::path::PathBuf {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!("ff_user_write_{name}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// 覆盖一份已有的用户文件：写完只剩目标本身，不许留下 `.tmp-*` 那种半成品。
    #[test]
    fn overwriting_a_user_file_leaves_no_stray_temp() {
        let dir = sandbox("clean");
        let target = dir.join("weekly.md");
        write_user_text(&target, "上一周的那一份").unwrap();
        write_user_text(&target, "这一周的新内容").unwrap();
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "这一周的新内容",
            "覆盖写要交出新的整份内容"
        );
        let strays: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n != "weekly.md")
            .collect();
        assert!(
            strays.is_empty(),
            "原子替换之后不该再有临时文件: {strays:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 接线腿：这两条路不许哪天又直接落盘。只扫 `#[cfg(test)]` 之前的生产代码，
    /// 否则这条用例自己的字面量会被它正在查的样式数进去（`listener.rs` 同一手）。
    #[test]
    fn the_user_file_paths_go_through_the_atomic_writer() {
        let src = include_str!("main.rs");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod = &src[..cut];
        let direct = ["std::fs::write", "("].concat();
        assert!(
            !prod.contains(direct.as_str()),
            "main.rs 的生产代码里出现了直接落盘：写用户文件要走 write_user_text"
        );
        let wired = ["write_user_text", "("].concat();
        assert_eq!(
            prod.matches(wired.as_str()).count(),
            3,
            "两处调用（导出、周报）加一处定义，数量变了说明接线动过"
        );
    }
}

#[cfg(test)]
mod pin_tests {
    use super::*;

    /// 贴图窗口的主人是本线程，所以测试线程自己把消息跑掉一段**有界**时间。
    unsafe fn pump(ms: u64) {
        let until = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        while std::time::Instant::now() < until {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn lp(x: i32, y: i32) -> LPARAM {
        LPARAM((((y & 0xFFFF) as isize) << 16) | ((x & 0xFFFF) as isize))
    }

    fn post(h: HWND, msg: u32, wp: usize, l: LPARAM) {
        unsafe {
            let _ = PostMessageW(Some(h), msg, WPARAM(wp), l);
        }
    }

    /// 屏上判据之外，贴图这一族也要有机器证据：✕ 只在悬停时出现也只在悬停时咬人、
    /// 点它只关那一张、滚轮放大同时把角标开起来、`close_all_pins` 收得干净。
    /// 会真的在桌面上摆出贴图窗口，所以默认不跑：
    /// `cargo test -p focusflow-native --bins -- --ignored a_pin_shows`
    #[test]
    #[ignore = "会真的在桌面上摆出贴图窗口"]
    fn a_pin_shows_its_cross_on_hover_and_closes_only_itself() {
        unsafe {
            let _serial = probe_guard();
            open_test_pins(3);
            pump(400);
            let list = PINS
                .lock()
                .map(|v| v.iter().map(|p| p.hwnd).collect::<Vec<_>>())
                .unwrap();
            assert_eq!(list.len(), 3, "三张贴图没都开出来");
            let h0 = HWND(list[0] as *mut core::ffi::c_void);
            let mut rc = RECT::default();
            let _ = GetWindowRect(h0, &mut rc);
            let (bx, by, bs, _) = pin_close_box(rc.right - rc.left, rc.bottom - rc.top);
            let (cx, cy) = (bx + bs / 2, by + bs / 2);

            // 悬停必须用**真实光标**驱动：只投合成的 WM_MOUSEMOVE 时，`TrackMouseEvent`
            // 会立刻补一个 WM_MOUSELEAVE（真光标本来不在窗上），刚点亮的悬停马上又被清掉。
            let mut home = POINT::default();
            let _ = GetCursorPos(&mut home);
            let (sx, sy) = (rc.left + cx, rc.top + cy);
            let _ = SetCursorPos(sx, sy);
            pump(400);
            let hov_now = PIN_HOVER.lock().map(|x| *x).unwrap_or(None);
            assert_eq!(hov_now, Some(list[0]), "光标压在 ✕ 上却没点亮悬停");
            post(h0, WM_LBUTTONDOWN, 0, lp(cx, cy));
            pump(120);
            post(h0, WM_LBUTTONUP, 0, lp(cx, cy));
            pump(400);
            let left = PINS
                .lock()
                .map(|v| v.iter().map(|p| p.hwnd).collect::<Vec<_>>())
                .unwrap();
            assert_eq!(left.len(), 2, "点 ✕ 没关掉这一张（或关多了）");
            assert!(!left.contains(&list[0]), "关掉的不是被点的那一张");

            // 没悬停的那一张：✕ 根本没画出来，点那个位置也不许关（不存在的按钮不许咬人）
            let _ = SetCursorPos(4, 4);
            pump(300);
            assert_eq!(
                PIN_HOVER.lock().map(|x| *x).unwrap_or(Some(0)),
                None,
                "光标离开后悬停标记没清"
            );
            let h1 = HWND(left[0] as *mut core::ffi::c_void);
            let _ = GetWindowRect(h1, &mut rc);
            let (bx2, by2, bs2, _) = pin_close_box(rc.right - rc.left, rc.bottom - rc.top);
            post(h1, WM_LBUTTONDOWN, 0, lp(bx2 + bs2 / 2, by2 + bs2 / 2));
            pump(120);
            post(h1, WM_LBUTTONUP, 0, lp(bx2 + bs2 / 2, by2 + bs2 / 2));
            pump(300);
            assert_eq!(
                PINS.lock().map(|v| v.len()).unwrap_or(0),
                2,
                "没悬停时点 ✕ 的位置居然关掉了贴图"
            );

            // 滚轮往上：窗口变大，同时角标开起来（那个数与屏上角标同源）
            let _ = GetWindowRect(h1, &mut rc);
            let before = rc.right - rc.left;
            post(h1, WM_MOUSEWHEEL, (120usize) << 16, lp(0, 0));
            pump(300);
            let _ = GetWindowRect(h1, &mut rc);
            assert!(rc.right - rc.left > before, "滚轮往上没放大");
            assert!(
                PINS.lock()
                    .map(|v| v.iter().any(|p| p.badge.is_some()))
                    .unwrap_or(false),
                "缩放后角标没开"
            );

            let n = close_all_pins();
            assert_eq!(n, 2, "收尾时该只剩两张");
            pump(200);
            assert_eq!(
                PINS.lock().map(|v| v.len()).unwrap_or(0),
                0,
                "关掉所有贴图没清干净"
            );
            let _ = SetCursorPos(home.x, home.y);
        }
    }

    /// ✕ 的落点：常规尺寸贴右上角；图缩到放不下也必须还在窗内。
    #[test]
    fn the_close_box_stays_inside_even_a_tiny_pin() {
        assert_eq!(pin_close_box(120, 80), (94, 2, 24, 24));
        assert_eq!(pin_close_box(30, 20), (22, 2, 6, 6));
        let (x, y, s, _) = pin_close_box(1, 1);
        assert!(s >= 1 && x >= 0 && y >= 0 && x + s <= 1 && y + s <= 1);
    }

    #[test]
    fn the_badge_reads_the_scale_as_whole_percent() {
        assert_eq!(pin_badge_text(1.0), "100%");
        assert_eq!(pin_badge_text(1.1), "110%");
        assert_eq!(pin_badge_text(0.25), "25%");
        assert_eq!(pin_badge_text(4.0), "400%");
    }

    /// 放大后把窗口拉回工作区，为的是右上角那颗 ✕ 还够得着。
    #[test]
    fn a_zoomed_pin_is_pulled_back_so_the_close_button_reaches() {
        let work = (0, 0, 1920, 1040);
        assert_eq!(
            pin_keep_close_visible((1700, 100), (400, 300), work),
            (1520, 100)
        );
        assert_eq!(
            pin_keep_close_visible((100, 900), (200, 300), work),
            (100, 740)
        );
        // 比工作区还宽：贴住左边，不许跟着越界跑
        assert_eq!(
            pin_keep_close_visible((100, 100), (2400, 300), work),
            (0, 100)
        );
        // 本来就在里面：一动不动
        assert_eq!(
            pin_keep_close_visible((100, 100), (200, 200), work),
            (100, 100)
        );
    }

    /// 往上滚一格必须变大、往下滚一格必须变小：写反了症状是"想放大结果糊成一条线"。
    #[test]
    fn wheel_up_grows_and_wheel_down_shrinks() {
        assert!(pin_next_scale(1.0, 1.0) > 1.0, "向上滚要变大");
        assert!(pin_next_scale(1.0, -1.0) < 1.0, "向下滚要变小");
        assert!((pin_next_scale(1.0, 1.0) - 1.1).abs() < 1e-6);
        // 退回来必须**正好**落在 1.0 上：旧写法连乘会停在 0.99，
        // 于是"图上一行＝屏上一行"这一档再也滚不回去（角标显示 99%）。
        assert_eq!(pin_next_scale(1.1, -1.0), 1.0);
    }

    /// 一整圈滚上去再滚回来，必须一格不差地回到 100%（离散档的意义就在这儿）。
    #[test]
    fn a_full_zoom_round_trip_lands_back_on_exactly_one() {
        let mut s = 1.0f32;
        for _ in 0..4 {
            s = pin_next_scale(s, 1.0);
        }
        assert_eq!(s, 2.0, "往上四格该走到 200%");
        for _ in 0..4 {
            s = pin_next_scale(s, -1.0);
        }
        assert_eq!(s, 1.0, "滚回来该正好是 100%，不是 0.99 那种");
        // 不在档上的值先吸到最近档：0.99 往上一格是 1.0，往下一格是 0.9
        assert_eq!(pin_next_scale(0.99, 1.0), 1.0);
        assert_eq!(pin_next_scale(0.99, -1.0), 0.9);
    }

    /// 连续滚不会滚成 0（窗口塌掉）也不会滚成天文数字（StretchDIBits 画不动）。
    #[test]
    fn the_scale_stays_inside_its_bounds() {
        let mut up = 1.0;
        for _ in 0..80 {
            up = pin_next_scale(up, 1.0);
        }
        assert_eq!(up, PIN_MAX_SCALE, "连滚 80 格要停在上限");
        let mut down = 1.0;
        for _ in 0..80 {
            down = pin_next_scale(down, -1.0);
        }
        assert_eq!(down, PIN_MIN_SCALE, "连滚 80 格要停在下限");
        // 已经在上/下限外面（手改配置、浮点误差）也要被夹回来。
        assert_eq!(pin_next_scale(99.0, 1.0), PIN_MAX_SCALE);
        assert_eq!(pin_next_scale(0.001, -1.0), PIN_MIN_SCALE);
        // 档表里必须有 1.0 这一档：整圈滚回来的判据靠它，删掉之后"回到原样"就只是句好听的话
        assert!(PIN_ZOOM_STEPS.contains(&1.0), "档表里丢了 100% 这一档");
    }

    /// 窗口尺寸 = 原图 × 档；四舍五入后至少 1×1，否则窗口建不出来。
    #[test]
    fn the_window_size_follows_the_scale_and_never_collapses() {
        assert_eq!(pin_scaled_size(120, 80, 1.0), (120, 80));
        assert_eq!(pin_scaled_size(120, 80, 0.5), (60, 40));
        assert_eq!(pin_scaled_size(120, 80, 4.0), (480, 320));
        assert_eq!(
            pin_scaled_size(1, 1, PIN_MIN_SCALE),
            (1, 1),
            "1×1 不能滚成 0×0"
        );
        assert_eq!(
            pin_scaled_size(3, 2, 0.25),
            (1, 1),
            "半像素要进到 1，不是截成 0"
        );
    }
}

#[cfg(test)]
mod pager_shape_tests {
    use super::*;
    use focusflow_core::plugins::{PluginView, Widget};

    /// 分页条的**现状读数**：协议带着 `prev_id` / `next_id`，而 `plugin_view_ops` 把整个
    /// `Pager` 折成一条 `POp::Label`。标签不在 `on_click` 认的那四条臂（Button / Table /
    /// Select / Field）里，所以这一行结构上就点不动 —— 旧 WebView 那对「上一页/下一页」没了。
    /// 接上 Pager 的那天这条要**先红**，红了之后把它改成"点下一页真的换页"的判据。
    #[test]
    fn the_pager_bar_is_one_label_row_and_nothing_clickable() {
        let view = PluginView {
            title: "shape".to_string(),
            widgets: vec![Widget::Pager {
                page: 2,
                pages: 3,
                total: 12,
                prev_id: "pg_prev".to_string(),
                next_id: "pg_next".to_string(),
            }],
        };
        let ops = plugin_view_ops(&view, rect(0.0, 0.0, 800.0, 600.0), 0.0, &HashMap::new());
        assert_eq!(ops.len(), 1, "分页条今天只产出一条控件");
        match &ops[0] {
            POp::Label { text, .. } => {
                assert!(text.contains("2 / 3"), "页码要在这一行里：{text}");
                assert!(text.contains("12"), "总条数要在这一行里：{text}");
            }
            // 标签之外的任何一种都能被 `on_click` 命中：说明 Pager 接上了。
            _ => panic!("分页条不再是纯标签，这条读数该改成新判据"),
        }
    }
}

/// 热键状态行那条读数。截图的两条键就在这三条里，"按了没反应"时用户要先知道是哪一条没了。
#[cfg(test)]
mod hotkey_line_tests {
    use super::{hotkey_status_line, status_is_warning};

    fn item(
        label: &'static str,
        raw: &str,
        why: Option<&str>,
    ) -> (&'static str, String, Option<String>) {
        (label, raw.to_string(), why.map(|w| w.to_string()))
    }

    #[test]
    fn all_three_registered_reads_back_what_is_live() {
        let line = hotkey_status_line(
            &[
                item("唤出窗口热键", "ctrl+f12", None),
                item("区域截图热键", "ctrl+f1", None),
                item("标注截图热键", "ctrl+f2", None),
            ],
            &[],
        );
        assert!(line.contains("已按配置注册"), "{line}");
        // 三条当前组合都要念出来：屏幕上能看见"现在按 ctrl+f1 是截图"才是这条读数的用处
        for raw in ["ctrl+f12", "ctrl+f1", "ctrl+f2"] {
            assert!(line.contains(raw), "少了 {raw}：{line}");
        }
    }

    #[test]
    fn one_failed_combo_is_named_with_its_own_reason() {
        let line = hotkey_status_line(
            &[
                item("唤出窗口热键", "ctrl+f12", None),
                item(
                    "区域截图热键",
                    "ctrl+f1",
                    Some("系统拒了：另一个程序已占用"),
                ),
                item("标注截图热键", "ctrl+f2", None),
            ],
            &[],
        );
        assert!(line.contains("区域截图热键"), "没点名是哪一条：{line}");
        assert!(line.contains("ctrl+f1"), "没说他配的是哪一串：{line}");
        // 原因不许被聚合话术吞掉——旧写法只剩"只注册上 2 条（详见日志）"
        assert!(line.contains("另一个程序已占用"), "原因被吞了：{line}");
        assert!(line.contains("其余 2 条已注册"), "{line}");
        assert!(
            !line.contains("标注截图热键"),
            "没失败的那条别一起报：{line}"
        );
    }

    #[test]
    fn all_failed_says_so_without_counting_the_rest() {
        let line = hotkey_status_line(
            &[
                item("唤出窗口热键", "", Some("这串组合解析不出来")),
                item("区域截图热键", "", Some("拿不到热键管理器")),
                item("标注截图热键", "", Some("拿不到热键管理器")),
            ],
            &[],
        );
        assert!(line.contains("没注册上"), "{line}");
        assert!(!line.contains("其余"), "三条都没成却还说其余：{line}");
    }

    /// 他主动清空的那一条：要说出来（不然"已注册"那句就成了假话），但不许走
    /// "没注册上"那一档 —— 那一档是要人去做点什么的，而这一条正是他刚做完的样子。
    #[test]
    fn an_emptied_binding_is_reported_as_off_not_as_a_failure() {
        let line = hotkey_status_line(
            &[
                item("唤出窗口热键", "ctrl+f12", None),
                item("区域截图热键", "ctrl+f1", None),
            ],
            &["标注截图热键"],
        );
        assert!(line.contains("未启用 1 条（标注截图热键）"), "{line}");
        assert!(!line.contains("没注册上"), "主动停用被念成了失败：{line}");
        assert!(!status_is_warning(&line), "主动停用不该用红色喊：{line}");
    }

    /// 三条都清空 ⇒ 不许还说"已按配置注册"。全挂着的时候那句是真话，一条都不挂还说
    /// 就是凭空给一个"热键都在"的印象。
    #[test]
    fn all_three_off_says_nothing_is_bound() {
        let line = hotkey_status_line(&[], &["唤出窗口热键", "区域截图热键", "标注截图热键"]);
        assert!(line.contains("三条热键都没挂上"), "{line}");
        assert!(line.contains("未启用 3 条"), "{line}");
        assert!(!line.contains("已按配置注册"), "{line}");
    }

    #[test]
    fn only_the_lines_that_need_action_are_painted_red() {
        // 要人做点什么的才用危险色：历史形状"注意…"（双计、搬迁没成那一类）、
        // 热键一条没注册上、以及截图那句"没存盘 / 图没编出来 / 剪贴板没进去 / 先框一个区域"。
        assert!(status_is_warning("注意：检测到旧版 FocusFlow.exe 在跑"));
        assert!(status_is_warning(
            "没注册上：区域截图热键 ctrl+f1：系统拒了：占用"
        ));
        assert!(status_is_warning(
            "没存盘：建目录失败：os error 5 —— 改好了再按 Enter"
        ));
        assert!(status_is_warning(
            "已存盘，但剪贴板没进去：打开剪贴板失败 —— Enter 再试"
        ));
        assert!(status_is_warning("图没编出来：png 编码失败"));
        assert!(status_is_warning("没有可出的图：先框一个区域，再按 Enter"));
        // 回执与"一切正常"不能红：满屏红等于没有红，而且会让人以为截图丢了
        assert!(!status_is_warning(
            "截图已存 snip_a.png（300x200），同时复制到剪贴板"
        ));
        assert!(!status_is_warning(
            "三条热键已按配置注册：唤出窗口热键 ctrl+f12 · 区域截图热键 ctrl+f1"
        ));
        assert!(!status_is_warning(
            "数据目录已搬到 E 盘（12 个文件 / 旧目录剩 0 项）"
        ));
    }
}

/// 设置页上"区域截图"的两颗入口。旧 Webview 版有"立即截图 / 立即标注截图"两颗，
/// 换原生时只剩标注那颗。这两条用例判的是"行在不在屏上、各开哪一档"。
#[cfg(test)]
mod snip_entry_tests {
    use super::{basic_cat, basic_label, snip_mode_of, Basic, BASIC_ROWS, SETTINGS_CATS};

    /// 两颗各开一档，且只有这两颗是截图入口。
    #[test]
    fn two_snip_entries_open_different_modes() {
        assert_eq!(
            snip_mode_of(Basic::Snip),
            Some(true),
            "标注那颗开成了松手即存"
        );
        assert_eq!(
            snip_mode_of(Basic::SnipQuick),
            Some(false),
            "松手那颗开成了标注"
        );
        // 别的行都不许是截图入口：否则一颗普通开关点下去会把全屏覆盖层弹出来。
        for b in BASIC_ROWS {
            if matches!(b, Basic::Snip | Basic::SnipQuick) {
                continue;
            }
            assert_eq!(snip_mode_of(b), None, "{} 不该是截图入口", basic_label(b));
        }
    }

    /// 加了变体、画它的分支和点它的分支都写了，唯独没排进 `BASIC_ROWS`——屏上就没有这颗按钮，
    /// 而编译器一声不吭。这一条把"屏上的行"与"截图入口"对齐。
    #[test]
    fn both_snip_entries_are_rows_on_the_settings_page() {
        for b in [Basic::Snip, Basic::SnipQuick] {
            assert!(
                BASIC_ROWS.contains(&b),
                "{} 没排进设置页的行表",
                basic_label(b)
            );
            let cat = basic_cat(b);
            assert_eq!(cat, basic_cat(Basic::Hotkey), "两颗入口不在同一分类");
            assert!(
                SETTINGS_CATS[cat].0.contains("截图"),
                "它们挂在「{}」这一类，用户不会去那儿找截图",
                SETTINGS_CATS[cat].0
            );
            assert!(
                basic_label(b).contains("截图"),
                "搜索框按「截图」找不到：{}",
                basic_label(b)
            );
        }
        assert_ne!(
            basic_label(Basic::Snip),
            basic_label(Basic::SnipQuick),
            "两行长得一模一样，屏上分不出点哪颗"
        );
    }
}

/// 托盘上每一条菜单项都得有人接。加了一项却忘了在 `ev.id` 那个 `match` 里写臂，症状恰好是
/// 这条功能登记过的唯一故障形态："点下去什么反应都没有"——屏上没痕迹、日志也不出声。
/// 刚加"截图（松手即存）"这一项时最容易犯的就是这个，所以留一条扫源码的守卫。
#[cfg(test)]
mod tray_menu_tests {
    /// 托盘那颗读的是随包的 `assets/focusflow.ico`。这条用例不测渲染，测的是那个文件
    /// **真的是个图标容器**：把 PNG 改名成 `.ico` 会让 `Icon::from_path` 静默失败，
    /// 屏上退回代码画的那颗圆点，而日志里只有一行——正是"图标看着不对"最难查的那条路。
    #[test]
    fn the_shipped_icon_file_is_a_real_icon_container() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/focusflow.ico");
        let bytes = std::fs::read(path).expect("随包的图标文件读不出来");
        assert!(bytes.len() > 64, "图标文件只有 {} 字节", bytes.len());
        assert_eq!(
            &bytes[0..4],
            &[0x00, 0x00, 0x01, 0x00],
            "这不是 ICO 容器（前 4 字节应是 00 00 01 00）：托盘会静默退回那颗圆点"
        );
        let entries = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
        assert!(entries >= 2, "ICO 里只有 {entries} 档尺寸");
        // 托盘按系统"小图标"那一档取（100% 缩放下 16 像素）。没有 16 这一档就是拿大档硬压，
        // 症状正是"图标糊"——所以这一条也算判据，不是顺手数个数。
        let has_16 = (0..entries).any(|i| bytes[6 + i * 16] == 16);
        assert!(has_16, "这份 ICO 里没有 16 像素那一档，托盘只能靠缩放");
    }

    #[test]
    fn every_tray_menu_id_has_a_handler() {
        let src = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs"));
        // 只扫托盘那一段：别的控件也有 id，扫全文会连这条用例自己的源码一起读进来
        // （那句 needle 的字面量本身就长得像一次声明）。
        let from = match src.find("fn make_tray") {
            Some(i) => i,
            None => panic!("托盘构造函数改名了，这条守卫要跟着改"),
        };
        let body = &src[from..];
        let body = match body.find("\nfn ") {
            Some(end) => &body[..end],
            None => body,
        };
        let head = "with_id";
        let head = format!("{head}(");
        let mut ids: Vec<String> = Vec::new();
        let mut at = 0usize;
        while let Some(i) = body[at..].find(head.as_str()) {
            let start = at + i + head.len();
            if !body[start..].starts_with('"') {
                at = start;
                continue;
            }
            let start = start + 1;
            match body[start..].find('"') {
                Some(k) => {
                    let id = &body[start..start + k];
                    if !id.is_empty() && !ids.iter().any(|x| x == id) {
                        ids.push(id.to_string());
                    }
                    at = start + k;
                }
                None => break,
            }
        }
        assert!(
            ids.len() >= 6,
            "托盘项只扫到 {ids:?}：源码形状变了，这条守卫要改"
        );
        for id in &ids {
            let arm = format!("\"{id}\" =>");
            assert!(
                src.contains(&arm),
                "托盘项 {id} 没有接它的那条臂（点下去不会有任何反应）"
            );
        }
    }
}
