// 产品形态是 GUI：不要控制台窗口。
#![windows_subsystem = "windows"]

//! FocusFlow 原生主界�?—�?原型（P0 平台 + P1a 合成渲染 + P1b 表格交互）�?//!
//! 不引入任�?UI 框架：窗�?`CreateWindowExW`、界�?D3D11/D2D、文�?DirectWrite�?//! 托盘 `tray-icon`、热�?`global-hotkey`；行内改名用**原生 EDIT 子窗�?*�?//!
//! 还没做（�?`docs/webview-removal-plan.md`）：滚动容器、其余五个视图、截图覆盖层�?
mod collect;
mod data;
mod floating;
mod floating_render;

use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::sync::atomic::AtomicI64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePen,
    CreateSolidBrush, DeleteDC, DeleteObject, EndPaint, GetDC, GetDIBits, GetStockObject,
    InvalidateRect, LineTo, MoveToEx, Rectangle, ReleaseDC, SelectObject, SetBkColor, SetBkMode,
    SetDIBits, SetDIBitsToDevice, SetTextColor, TextOutW, UpdateWindow, BI_RGB, BITMAPINFO,
    BITMAPINFOHEADER, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH,
    DIB_RGB_COLORS, FW_NORMAL, HBRUSH, HDC, HFONT, HGDIOBJ, NULL_BRUSH, OUT_DEFAULT_PRECIS,
    PAINTSTRUCT,
    PS_SOLID, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Controls::MARGINS;
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
/// 行内编辑结束：`wparam` 1=提交。
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

// ===== 主题令牌（对�?desktop/ui/tokens.css �?dark 档）=====

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
    /// 弹窗底色�?*始终不透明**。玻璃模式下 `card` 是半透明的，
    /// 拿它当弹窗底会把下面的表格文字透出来（tokens.css �?--modal-bg 就是为这个单开的）。
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
    /// 键帽上那种小一号的字（窄键放不�?11px）。
center_tiny: IDWriteTextFormat,
    tab: IDWriteTextFormat,
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
    /// 必须�?先放�?：后台缓冲还被它引用�?`ResizeBuffers` 会返�?INVALID_CALL。
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
            let _ = self.ctx.SetTarget(None);
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
                    // 先放掉位图：它持有对后台缓冲的引用，不放�?ResizeBuffers 必失败。
c.bitmap = None;
                    let r = c
                        .swap
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

// ===== 应用状�?=====

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
    /// 系统材质贴上了（Win11 Mica）。
mica: bool,
    /// 现在真的走合成路径（透明/玻璃）。只�?`mica` 且合成成功才�?true。
glass: bool,
    /// 只读库句柄：`init_readonly` 的副作用是全局年度缓存。
_db: Arc<focusflow_core::db::Database>,
    /// 顶部视图�?=键鼠排行 1=应用排行 2=设备排行 3=活跃分析 4=插件 5=设置。
view: usize,
    /// 活跃分析页内的子页签�?=趋势 1=小时 2=星期 3=热力。
atab: usize,
    /// 列表滚动到第几行�? = 顶部）。
scroll: usize,
    /// 当前统计周期；后台刷新线程也要读，所以放在原子量里。
period: Arc<AtomicI64>,
    /// 设备详情弹窗（None = 没开）。打开时是模态：点击/滚动都被它接走。
detail: Option<DeviceDetail>,
    /// 热力页上光标下的键（`heat_keys()` 的下标）。
heat_hover: Option<usize>,
    /// 设置页底部那条状态：上一次数据操作的结果 / "正在�?。
status: Option<String>,
    /// 有后台操作在跑（按钮置灰，避免连点）。
busy: bool,
    /// 设置页的搜索词（搜索框里的文字，过滤清单用）。
search: String,
    /// 设置页左侧当前分类（0=常规…，见 [`SETTINGS_CATS`]）。
    settings_cat: usize,
    /// 设置页里那个常驻的搜索框（与行内编辑�?EDIT 不是同一个）。
search_edit: Option<HWND>,
    /// 后台操作把结果行塞这里，再投 `WM_OP_DONE` 让界面来取。
ops: Arc<Mutex<Vec<String>>>,
    /// 插件管理器（Lua 不是 Send，只能待�?UI 线程）。
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
    /// 上一次重算插件视图的时间�? 秒一次就够，别跟着 100ms 心跳跑）。
last_pv_refresh: Option<std::time::Instant>,
    hover: Option<usize>,
    sel: Option<usize>,
    /// 行内编辑用的原生 EDIT 子窗口（懒创建，一直复用）。
edit: Option<HWND>,
    edit_target: Option<EditTarget>,
    edit_row: Option<usize>,
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
            };

            let (tray, pause_item) = make_tray(&snap);
            let (hotkeys, hotkey_hk, snip_hk) = make_hotkeys();

            // 启动时这条状态行只放一句话，按"要紧的先说"排：
            // 搬迁没成 > 双计提醒 > 刚搬完告知搬到了哪。
            let boot_status = move_notice
                .as_ref()
                .map(|n| format!("注意：{}（{}）", n.detail, n.step))
                .or_else(|| {
                    if collect::is_started() && collect::legacy_running() {
                        Some("注意：检测到旧版 FocusFlow.exe 在跑，两处采集会让数字翻倍；建议只留一个".to_string())
                    } else {
                        moved.as_ref().map(|r| {
                            format!(
                                "数据目录已搬到 {}（{} 个文件 / 旧目录剩 {} 项）",
                                r.to.display(),
                                r.summary.copied,
                                r.leftover_in_source
                            )
                        })
                    }
                });

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
                w!("Microsoft YaHei UI"),
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
                .spawn(move || loop {
                    std::thread::sleep(Duration::from_secs(2));
                    let next = data::load(p.load(std::sync::atomic::Ordering::Relaxed));
                    // 悬浮窗读的是这几格，先推过去再让主界面拿新快照。
                    publish_floating(&next);
                    if let Ok(mut g) = s.lock() {
                        *g = next;
                    }
                    let target = HWND(hwnd_val as *mut core::ffi::c_void);
                    let _ = PostMessageW(Some(target), WM_REFRESH, WPARAM(0), LPARAM(0));
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
            hotkeys_on: hotkey_hk.is_some() || snip_hk.is_some(),
            hotkey_hk,
            snip_hk,
                mica,
                glass: false,
                _db: db,
                view: 0,
                atab: 0,
                scroll: 0,
                period,
                detail: None,
                heat_hover: None,
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
                last_pv_refresh: None,
                hover: None,
                sel: None,
                edit: None,
                edit_target: None,
                edit_row: None,
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
        let Some(t) = self.target.as_ref() else { return };
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
        let Some(t) = self.target.as_ref() else { return };
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
                scroll: self.scroll,
                period: self.period.load(std::sync::atomic::Ordering::Relaxed),
                hover: self.hover,
                sel: self.sel,
                editing: self.edit_row,
                heat_hover: self.heat_hover,
                hotkey_on: self.hotkey_hk.is_some() || self.snip_hk.is_some(),
                floating_on: floating::is_visible(),
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

    /// 鼠标移动：排行页更新悬停行，热力页更新悬停键；返回是否需要重画。
fn on_move(&mut self, hwnd: HWND, px: f32, py: f32) -> bool {
        let mut changed = false;
        // 弹窗、插。
if self.detail.is_some() || (self.view == 3 && self.atab != 3) {
            changed |= self.hover.take().is_some();
            changed |= self.heat_hover.take().is_some();
            return changed;
        }
        let (w, h) = self.client_dip(hwnd);
        let s = self.scale(hwnd);
        let (x, y) = (px / s, py / s);

        if self.view == 3 {
            changed |= self.hover.take().is_some();
            let content = analytics_content(w, h);
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
            return changed;
        }

        if self.view == 4 {
            changed |= self.heat_hover.take().is_some();
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

        // 排行�?        changed |= self.heat_hover.take().is_some();
        let g = table_geom(left_card(w, h));
        let count = self
            .snap
            .lock()
            .map(|snap| match self.view {
                1 => snap.apps.len(),
                2 => snap.devices.len(),
                _ => snap.rank.len(),
            })
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
                        self.pv_scroll = 0;
                        self.pv_select = None;
                        self.pv_modal = None;
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
                    self.pv_scroll = 0;
                    self.pv_select = None;
                    self.pv_modal = None;
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    return;
                }
                // 4) 正文控件
                let ops = plugin_view_ops(
                    &view,
                    rect(
                        card.left,
                        card.top + 40.0,
                        card.right - card.left,
                        card.bottom - card.top,
                    ),
                    self.pv_scroll as f32,
                );
                for op in &ops {
                    match op {
                        POp::Button {
                            rect: r,
                            id,
                            disabled,
                            modal,
                            ..
                        } => {
                            if !*disabled && hit(*r, x, y) {
                                if let Some(mid) = modal {
                                    self.pv_select = None;
                                    self.pv_modal = Some(mid.clone());
                                    tracing_note(&format!("插件弹窗：{n} / {mid}"));
                                    unsafe {
                                        let _ = InvalidateRect(Some(hwnd), None, false);
                                    }
                                } else {
                                    self.run_plugin_action(hwnd, &n, id);
                                }
                                return;
                            }
                        }
                        // 表格行内操作按钮
                        POp::Table { buttons, .. } => {
                            for (br, id, _) in buttons {
                                if hit(*br, x, y) {
                                    self.run_plugin_action(hwnd, &n, id);
                                    return;
                                }
                            }
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
                let row_rect = rect(
                    g.body_left,
                    y0,
                    w - 32.0 - g.body_left,
                    g.row_h - 2.0,
                );
                if hit(row_rect, x, y) {
                    self.sel = Some(idx);
                    match items[idx] {
                        SItem::Header(_) => {}
                        SItem::Action(a) => self.run_action(hwnd, a),
                        SItem::Basic(Basic::Autostart) => self.toggle_autostart(hwnd),
                        SItem::Basic(Basic::Hotkey) => self.toggle_hotkey(hwnd),
                        SItem::Basic(Basic::Snip) => start_snip(hwnd),
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

        let r = left_card(w, h);
        let g = table_geom(r);
        let count = self
            .snap
            .lock()
            .map(|snap| match self.view {
                1 => snap.apps.len(),
                2 => snap.devices.len(),
                _ => snap.rank.len(),
            })
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
                    // 点行本身（不在按钮上�? 看详情。
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
                    backup_database, cleanup_old_data, vacuum_all, vacuum_line, BackupOutcome,
                };
                let line = match which {
                    0 => {
                        let max = focusflow_core::config::instance()
                            .get_int("database", "max_backups", 5);
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
                            BackupOutcome::NothingToDo => "没有可备份的库（还没有数据）".to_string(),
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
                                            PostMessageW(Some(target), WM_OP_DONE, WPARAM(0), LPARAM(0))
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
                        match std::fs::write(&path, body) {
                            Ok(()) => format!("已导出：{}", path.display()),
                            Err(e) => format!("导出失败：{e}"),
                        }
                    }
                    2 => {
                        let r = cleanup_old_data(30);
                        let mut s = format!("已清理 {} 条（保留 30 天）", r.deleted);
                        if !r.failed_years.is_empty() {
                            s.push_str(&format!("；未处理的年份：{:?}", r.failed_years));
                        }
                        if let Some(e) = &r.dir_error {
                            s.push_str(&format!("；{e}"));
                        }
                        s
                    }
                    6 => {
                        let dir = match std::env::var("FOCUSFLOW_IMPORT_DIR") {
                            // 测试钩子：给了目录就不弹框
                            Ok(p) => std::path::PathBuf::from(p),
                            Err(_) => {
                                let picked = rfd::FileDialog::new()
                                    .set_title("选择旧版数据目录（含 focusflow_YYYY.db 的那个 data 目录）")
                                    .pick_folder();
                                match picked {
                                    Some(p) => p,
                                    None => {
                                        if let Ok(mut q) = ops.lock() {
                                            q.push("已取消".to_string());
                                        }
                                        let target = HWND(hwnd_val as *mut core::ffi::c_void);
                                        let _ = unsafe {
                                            PostMessageW(Some(target), WM_OP_DONE, WPARAM(0), LPARAM(0))
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
                                            PostMessageW(Some(target), WM_OP_DONE, WPARAM(0), LPARAM(0))
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
            self.status = Some(format!(
                "开机自启已{}",
                if want { "打开" } else { "关闭" }
            ));
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
        unregister_combos(self.hotkeys.as_ref(), [self.hotkey_hk, self.snip_hk]);
        self.hotkey_hk = None;
        self.snip_hk = None;
        if !self.hotkeys_on {
            return "全局热键已关闭：两条组合都不注册".to_string();
        }
        let toggle = register_combo(self.hotkeys.as_ref(), "toggle_window");
        self.hotkey_hk = toggle;
        let snip = register_combo(self.hotkeys.as_ref(), "snip");
        self.snip_hk = snip;
        match (toggle, snip) {
            (Some(_), Some(_)) => "两条热键已按配置注册".to_string(),
            (None, None) => "热键没能注册上（组合无效或被别的程序占用，详见日志）".to_string(),
            _ => "只注册上一半热键（哪一条失败写在日志里）".to_string(),
        }
    }

    /// 全局热键总开关：两条绑定（唤出 / 截图）一起注册或一起取消，
    /// 配置键跟着写一份留给读它的人（启动时是否照它注册是另一回事，见交接文档）。
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
            let _ = it.set_checked(collect::is_paused());
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
        self.pv_scroll = 0;
        self.pv_select = None;
        self.pv_modal = None;
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
                tracing_note(&format!(
                    "插件字段已回写：{plugin} / {field} = {value:?}"
                ));
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
    fn close_plugin_modal(
        &mut self,
        hwnd: HWND,
        plugin: &str,
        m: &ModalRef<'_>,
    ) {
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
        let row = self
            .snap
            .lock()
            .ok()
            .and_then(|s| s.settings.get(snapshot_idx).map(|r| (r.value.clone(), r.kind)));
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
        let row = self
            .snap
            .lock()
            .ok()
            .and_then(|s| s.settings.get(idx).map(|r| (r.section, r.key, r.value.clone())));
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
        let Some(target) = self.edit_target.take() else { return };
        if let Some(e) = self.edit {
            // 先藏起来：藏窗口会触�?WM_KILLFOCUS，它又会投一�?提交"�?            // �?`edit_target` 已经取空把这一次挡掉。
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
                            // 空串是既有的"恢复默认"手势，默认值本身一定可用，不用验。
                            let hotkey_err = if section == "hotkey"
                                && matches!(key, "toggle_window" | "snip")
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
                                let v = if value.is_empty() { default } else { value };
                                match focusflow_core::config::instance().set(section, key, &v) {
                                    Ok(()) => {
                                        if section == "hotkey" {
                                            let msg = self.reload_hotkeys();
                                            self.status = Some(msg);
                                        }
                                        self.refresh_now();
                                        tracing_note(&format!("设置已写入 {section}.{key}={v:?}"));
                                    }
                                    Err(err) => {
                                        tracing_note(&format!("设置写入失败 {section}.{key}: {err}"))
                                    }
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
        // 注：这条查询�?UI 线程上跑（探针库很小）。正式版要挪到后台线程再投消息回来。
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
            let _ = self.rt.FillRectangle(&r, b);
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
            let _ = self.rt.FillRoundedRectangle(&rr, fill);
            if let Some(bd) = border {
                let _ = self.rt.DrawRoundedRectangle(&rr, bd, 1.0, None);
            }
        }
    }

    fn line(&self, x0: f32, y0: f32, x1: f32, y1: f32, b: &ID2D1SolidColorBrush, w: f32) {
        unsafe {
            let _ = self.rt.DrawLine(
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
            let _ = self.rt.DrawText(
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

// ===== 设置�?=====

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
        Basic::Snip => "区域截图",
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
        Basic::Hotkey | Basic::Snip => 1,
        Basic::Floating => 2,
        Basic::WeeklyReport => 3,
        Basic::DataDir => 4,
    }
}

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
    for b in [
        Basic::Autostart,
        Basic::Collector,
        Basic::Pause,
        Basic::Theme,
        Basic::Hotkey,
        Basic::Snip,
        Basic::Floating,
        Basic::DataDir,
        Basic::WeeklyReport,
    ] {
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
    ("更改数据文件夹…", "把 data/ 与 backup/ 搬到新目录；重启后执行"),
];

/// 设置页右上角那个搜索框（底框；真正的输入是叠上去的原。
fn search_box_rect(w: f32) -> D2D_RECT_F {
    rect(w - 16.0 - 16.0 - 260.0, 218.0, 260.0, 26.0)
}

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
        rect(card.left + 16.0, card.top + 12.0, 120.0, 22.0),
        &p.fmt.body,
        &p.br.fg,
    );
    p.text(
        "左侧分类；改动写进 config.ini，标「重启」的项要重启程序",
        rect(card.left + 66.0, card.top + 15.0, 560.0, 18.0),
        &p.fmt.small,
        &p.br.muted,
    );

    // 搜索框：底框画在这里，真正的输入是叠在上面的原生 EDIT（见 place_search_edit）。
let sbox = search_box_rect(w);
    p.round(sbox, 7.0, &p.br.header, Some(&p.br.border));
    if filter.is_empty() {
        p.text(
            "搜索设置（名称或段.键）…",
            rect(sbox.left + 10.0, sbox.top + 4.0, sbox.right - sbox.left - 20.0, 18.0),
            &p.fmt.small,
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
            rect(nr.left + 12.0, nr.top + 4.0, nr.right - nr.left - 20.0, 18.0),
            &p.fmt.body,
            if active { &p.br.accent } else { &p.br.fg },
        );
        p.text(
            desc,
            rect(nr.left + 12.0, nr.top + 22.0, nr.right - nr.left - 20.0, 14.0),
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
                    rect(r.left + 12.0, y + 3.0, 420.0, 18.0),
                    &p.fmt.body,
                    &p.br.fg,
                );
                p.text(
                    row.help,
                    rect(
                        r.left + 12.0,
                        y + 21.0,
                        (g.value_x - r.left - 30.0).max(120.0),
                        15.0,
                    ),
                    &p.fmt.small,
                    &p.br.muted,
                );

                let vb = rect(g.value_x, y + 9.0, g.value_w, 26.0);
                if row.kind == 0 {
                    let on = row.value == "true";
                    p.round(vb, 6.0, if on { &p.br.success } else { &p.br.grid }, None);
                    p.text(
                        if on { "开" } else { "关" },
                        vb,
                        &p.fmt.center_small,
                        if on { &p.br.white } else { &p.br.muted },
                    );
                } else {
                    let editing = vs.editing == Some(idx);
                    p.round(
                        vb,
                        6.0,
                        &p.br.card,
                        Some(if editing { &p.br.accent } else { &p.br.border }),
                    );
                    if !editing {
                        p.text(
                            &row.value,
                            rect(
                                vb.left + 8.0,
                                vb.top,
                                vb.right - vb.left - 16.0,
                                vb.bottom - vb.top,
                            ),
                            &p.fmt.small,
                            &p.br.fg,
                        );
                    }
                }
                // 只有"要重启/下次隐藏"这种例外才写在行上；绝大多数立即生效，不再每行重复。
                if row.live != "改完立即生效" {
                    p.text(
                        row.live,
                        rect(vb.right + 14.0, y + 13.0, 220.0, 16.0),
                        &p.fmt.small,
                        &p.br.muted,
                    );
                }
            }
            SItem::Header(title) => {
                p.text(
                    title,
                    rect(r.left + 12.0, y + 12.0, 360.0, 18.0),
                    &p.fmt.small,
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
                    rect(r.left + 12.0, y + 3.0, 240.0, 18.0),
                    &p.fmt.body,
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
                            &p.fmt.center_small,
                            if on { &p.br.white } else { &p.br.muted },
                        );
                        p.text(
                            "改完立即生效（现读启动文件夹）",
                            rect(r.left + 12.0, y + 21.0, 300.0, 18.0),
                            &p.fmt.small,
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
                            &p.fmt.center_small,
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
                            rect(r.left + 12.0, y + 21.0, 520.0, 18.0),
                            &p.fmt.small,
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
                        p.text(label, vb, &p.fmt.center_small, fg);
                        p.text(
                            if !vs.collector_on {
                                "采集没启用，暂停无从谈起"
                            } else if vs.collector_paused {
                                "已暂停；点一下继续（键鼠与设备计数一起恢复）"
                            } else {
                                "点一下暂停：键鼠与设备计数一起停，不写配置、重启即恢复"
                            },
                            rect(r.left + 12.0, y + 21.0, 520.0, 18.0),
                            &p.fmt.small,
                            &p.br.muted,
                        );
                    }
                    Basic::Hotkey => {
                        let on = vs.hotkey_on;
                        p.round(vb, 6.0, if on { &p.br.success } else { &p.br.grid }, None);
                        p.text(
                            if on { "开" } else { "关" },
                            vb,
                            &p.fmt.center_small,
                            if on { &p.br.white } else { &p.br.muted },
                        );
                        let toggle = snap_setting(snap, "hotkey", "toggle_window");
                        let snip = snap_setting(snap, "hotkey", "snip");
                        p.text(
                            &format!(
                                "总开关：开 = 注册下面两行（{toggle} / {snip}）；关 = 一条都不占用"
                            ),
                            rect(r.left + 12.0, y + 21.0, 520.0, 18.0),
                            &p.fmt.small,
                            &p.br.muted,
                        );
                    }
                    Basic::Snip => {
                        p.round(vb, 6.0, &p.br.accent, None);
                        p.text("截图…", vb, &p.fmt.center_small, &p.br.white);
                        let snip = snap_setting(snap, "hotkey", "snip");
                        p.text(
                            &format!(
                                "也可按「区域截图热键」（{snip}）；PNG 存到 data/screenshots 并复制到剪贴板"
                            ),
                            rect(r.left + 12.0, y + 21.0, 600.0, 15.0),
                            &p.fmt.small,
                            &p.br.muted,
                        );
                    }
                    Basic::Floating => {
                        let on = vs.floating_on;
                        p.round(vb, 6.0, if on { &p.br.success } else { &p.br.grid }, None);
                        p.text(
                            if on { "显示" } else { "隐藏" },
                            vb,
                            &p.fmt.center_small,
                            if on { &p.br.white } else { &p.br.muted },
                        );
                        p.text(
                            "拖到哪记哪；双击卡片叫回主窗口（[floating] 段）",
                            rect(r.left + 12.0, y + 21.0, 420.0, 18.0),
                            &p.fmt.small,
                            &p.br.muted,
                        );
                    }
                    Basic::Theme => {
                        let dark = vs.dark;
                        p.round(vb, 6.0, if dark { &p.br.header } else { &p.br.accent }, Some(&p.br.border));
                        p.text(
                            if dark { "深色" } else { "浅色" },
                            vb,
                            &p.fmt.center_small,
                            &p.br.fg,
                        );
                        p.text(
                            "点一下切换；配色照 tokens.css 的两套令牌（[gui] theme）",
                            rect(r.left + 12.0, y + 21.0, 420.0, 18.0),
                            &p.fmt.small,
                            &p.br.muted,
                        );
                    }
                    Basic::WeeklyReport => {
                        p.round(vb, 6.0, &p.br.accent, None);
                        p.text("生成", vb, &p.fmt.center_small, &p.br.white);
                        p.text(
                            "汇总上周一~周日，写到 data/reports/weekly-YYYY-MM-DD.md（只存本机）",
                            rect(r.left + 12.0, y + 21.0, 600.0, 15.0),
                            &p.fmt.small,
                            &p.br.muted,
                        );
                    }
                    Basic::DataDir => {
                        p.round(vb, 6.0, &p.br.header, Some(&p.br.border));
                        p.text(
                            "打开",
                            vb,
                            &p.fmt.center_small,
                            &p.br.fg,
                        );
                        let dir_line = if snap.pending_move.is_empty() {
                            format!("{}（改目录要用下面的「更改数据文件夹」，重启后生效）", snap.data_home)
                        } else {
                            // 安排过但还没重启：这句话就是"为什么现在还在旧目录"的读数。
                            format!(
                                "现在：{}｜已安排搬到：{}（重启后执行，重启前请留着旧目录）",
                                snap.data_home, snap.pending_move
                            )
                        };
                        p.text(
                            &dir_line,
                            rect(r.left + 12.0, y + 21.0, 560.0, 18.0),
                            &p.fmt.small,
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
                    rect(r.left + 12.0, y + 3.0, 240.0, 18.0),
                    &p.fmt.body,
                    &p.br.fg,
                );
                let vb = rect(g.value_x, y + 9.0, 80.0, 26.0);
                let enabled = !busy;
                p.round(vb, 6.0, if enabled { &p.br.accent } else { &p.br.grid }, None);
                p.text(
                    if busy { "…" } else { "执行" },
                    vb,
                    &p.fmt.center_small,
                    if enabled { &p.br.white } else { &p.br.muted },
                );
                p.text(
                    desc,
                    rect(r.left + 12.0, y + 21.0, 360.0, 18.0),
                    &p.fmt.small,
                    &p.br.muted,
                );
            }
        }
    }

    // 状态行
    if let Some(line) = status {
        // 「注意：…」是双计/采集异常这类要人处理的事，用危险色；其余是普通状态行。
        let warn = line.starts_with("注意");
        p.text(
            line,
            rect(card.left + 16.0, card.bottom - 26.0, card.right - card.left - 32.0, 18.0),
            &p.fmt.small,
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
        p.text(
            label,
            rect(cx, yy, 76.0, 18.0),
            &p.fmt.small,
            &p.br.muted,
        );
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
    row("近 30 天", &thousands(d.month.max(0) as u64), x, y0 + step * 2.0);
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
        if d.first_date.is_empty() { "—" } else { &d.first_date },
        x,
        y0 + step * 5.0,
    );
    row(
        "最近使用",
        if d.last_date.is_empty() { "—" } else { &d.last_date },
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

// ===== 一屏内�?=====

/// 一屏里会变的交互状态。画图和命中测试必须读同一份。
#[derive(Clone, Copy, Default)]
struct ViewState {
    view: usize,
    atab: usize,
    scroll: usize,
    period: i64,
    hover: Option<usize>,
    sel: Option<usize>,
    editing: Option<usize>,
    heat_hover: Option<usize>,
    /// 全局热键此刻是否真的注册着（不是配置键，是运行时状态）。
hotkey_on: bool,
    /// 悬浮窗此刻是否真的显示着（同上：读运行时状态，不读配置键 ——
    /// 手改 `[floating] enabled` 并不会让卡片出现或消失）。
    floating_on: bool,
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
    let max_day = snap
        .max_day
        .clone()
        .unwrap_or_else(|| ("—".to_string(), 0));
    // 「当前速度」有三种状态：没开采集（只读）、开着但暂停、正在采集。
    // 没数据时不写内部代号，也不留一个看起来只是暂时没数的空位。
    let (speed_value, speed_sub) = if !vs.collector_on {
        ("—".to_string(), "本端只读 · 未接采集".to_string())
    } else if vs.collector_paused {
        ("—".to_string(), "采集已暂停".to_string())
    } else {
        (thousands(vs.cpm.max(0) as u64), "次/分 · 近 60 秒".to_string())
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
                draw_plugin_view(p, w, h, name, view, pv_scroll);
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
    draw_line_chart(p, rect(right_x, left.top, right_w, chart_h), snap);
    draw_hourly(
        p,
        rect(
            right_x,
            left.top + chart_h + 12.0,
            right_w,
            body_h - chart_h - 12.0,
        ),
        snap,
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
            rect(r.left + pad, r.top + 14.0, r.right - r.left - pad * 2.0, 18.0),
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
let bar = rect(
        bar_x,
        r.top + 19.0,
        (dots_x - bar_x - 214.0).max(40.0),
        8.0,
    );
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
            // 趋势图：复用排行页那张，只是给它整块宽度。
let plot = rect(content.left, content.top, content.right - content.left, 300.0);
            draw_line_chart(p, plot, snap);
        }
        1 => {
            let plot = rect(content.left, content.top, content.right - content.left, 300.0);
            draw_hourly(p, plot, snap);
        }
        2 => {
            let plot = rect(content.left, content.top, content.right - content.left, 300.0);
            draw_weekday(p, plot, snap);
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
        &format!("按{}统计", period_text(snap.period)),
        rect(r.right - pad - 120.0, r.top + 16.0, 120.0, 18.0),
        &p.fmt.right,
        &p.br.muted,
    );

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
    for i in 0..n {
        let v = snap.weekday[i] as f32;
        let hh = (v / max) * plot_height(plot);
        let x = plot.left + i as f32 * (bw + gap);
        let bar = rect(x, plot.bottom - hh, bw, hh);
        // 周末用绿色、工作日用强调色 —— 两套主题里都是这两个令牌。
        let base_c = if i >= 5 { p.pal.success } else { p.pal.accent };
        if let Some(b) = p.brush(with_alpha(base_c, 0.85)) {
            p.round(bar, 4.0, &b, None);
        }
        p.text(
            labels[i],
            rect(x, plot.bottom + 6.0, bw, 16.0),
            &p.fmt.center_small,
            &p.br.muted,
        );
        p.text(
            &thousands(snap.weekday[i].max(0) as u64),
            rect(x, plot.bottom - hh - 18.0, bw, 16.0),
            &p.fmt.center_small,
            &p.br.muted,
        );
    }
}

// ===== 键盘热力（物理键位，移植�?desktop/ui/js/charts.js�?=====

struct HeatKey {
    name: &'static str,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    label: &'static str,
}

/// 主区 6 行：[。
const HEAT_MAIN: &[(f32, &[(&str, f32, f32)])] = &[
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
        .and_then(|i| keys.get(i).map(|k| (k, counts.get(k.name).copied().unwrap_or(0))))
        .map(|(k, c)| {
            let pct = if total > 0 {
                c as f32 * 100.0 / total as f32
            } else {
                0.0
            };
            format!(
                "{} · {} 次 · {pct:.1}%",
                k.name,
                thousands(c.max(0) as u64)
            )
        })
        .unwrap_or_else(|| {
            format!("其它按键：{other_n} 个 · {other_c} 次（板上没摆的键）")
        });
    p.text(
        &line,
        rect(r.left + 170.0, ly + 4.0, r.right - r.left - 194.0, 16.0),
        &p.fmt.small,
        &p.br.muted,
    );
}


// ===== 截图（选区 �?PNG�?=====

/// 屏幕区域截图的缓冲（BGRA、自上而下）。
fn capture_rect(x: i32, y: i32, w: i32, h: i32) -> std::result::Result<Vec<u8>, String> {
    if w <= 0 || h <= 0 {
        return Err(format!("选区太小：{w}×{h}"));
    }
    unsafe {
        let screen = GetDC(None);
        if screen.0.is_null() {
            return Err("拿不到屏幕 DC".into());
        }
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, bmp.into());
        let ok = BitBlt(mem, 0, 0, w, h, Some(screen), x, y, SRCCOPY).is_ok();

        let mut info = BITMAPINFO::default();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = w;
        // 负高�?= 自上而下，省一次翻。
info.bmiHeader.biHeight = -h;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB.0;
        let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
        let lines = GetDIBits(
            mem,
            bmp,
            0,
            h as u32,
            Some(buf.as_mut_ptr() as *mut _),
            &mut info,
            DIB_RGB_COLORS,
        );

        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);

        if !ok || lines == 0 {
            return Err("BitBlt/GetDIBits 没成功".into());
        }
        Ok(buf)
    }
}

/// BGRA 。
fn save_png(path: &std::path::Path, w: u32, h: u32, bgra: &[u8]) -> std::result::Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| format!("建文件失败：{e}"))?;
    let writer = std::io::BufWriter::new(file);
    let mut enc = png::Encoder::new(writer, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut png_writer = enc.write_header().map_err(|e| format!("PNG 头失败：{e}"))?;
    let mut rgba = Vec::with_capacity(bgra.len());
    for px in bgra.chunks_exact(4) {
        rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
    }
    png_writer
        .write_image_data(&rgba)
        .map_err(|e| format!("PNG 数据失败：{e}"))?;
    Ok(())
}

/// 把一张 BGRA（自上而下）塞进剪贴板，格式 CF_DIB（自下而上，Windows 位图的老规矩）。
fn copy_bgra_to_clipboard(bgra: &[u8], w: i32, h: i32) -> std::result::Result<(), String> {
    const CF_DIB: u32 = 8;
    unsafe {
        if OpenClipboard(None).is_err() {
            return Err("剪贴板被别的程序占着".into());
        }
        let _ = EmptyClipboard();
        let header = std::mem::size_of::<BITMAPINFOHEADER>();
        let image = (w as usize) * (h as usize) * 4;
        let total = header + image;
        let hmem = match GlobalAlloc(GMEM_MOVEABLE, total) {
            Ok(h) => h,
            Err(e) => {
                let _ = CloseClipboard();
                return Err(format!("GlobalAlloc 失败：{e}"));
            }
        };
        let ptr = GlobalLock(hmem) as *mut u8;
        if ptr.is_null() {
            let _ = CloseClipboard();
            return Err("GlobalLock 失败".into());
        }
        let bi = BITMAPINFOHEADER {
            biSize: header as u32,
            biWidth: w,
            biHeight: h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        std::ptr::copy_nonoverlapping(&bi as *const _ as *const u8, ptr, header);
        // CF_DIB 是自下而上的：逐行倒着拷
        let dst = ptr.add(header);
        for row in 0..h as usize {
            let src_row = h as usize - 1 - row;
            std::ptr::copy_nonoverlapping(
                bgra.as_ptr().add(src_row * w as usize * 4),
                dst.add(row * w as usize * 4),
                w as usize * 4,
            );
        }
        let _ = GlobalUnlock(hmem);
        if SetClipboardData(CF_DIB, Some(HANDLE(hmem.0))).is_err() {
            let _ = CloseClipboard();
            return Err("SetClipboardData 失败".into());
        }
        let _ = CloseClipboard();
    }
    Ok(())
}

/// 截一块屏幕并落盘（顺带进剪贴板），返回文件路径。
///
/// 现在的主流程是「选区 → 编辑窗口 → 保存」，这一条留给"不想编辑、直接截"的
/// 快捷路径（P6 收尾时接到 `[hotkey] snip_direct`）。
#[allow(dead_code)]
fn snip_region_to_file(x: i32, y: i32, w: i32, h: i32) -> std::result::Result<std::path::PathBuf, String> {
    let buf = capture_rect(x, y, w, h)?;
    let dir = focusflow_core::paths::screenshots_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("建截图目录失败：{e}"))?;
    let name = format!(
        "screenshot_{}.png",
        chrono::Local::now().format("%Y%m%d_%H%M%S")
    );
    let path = dir.join(name);
    save_png(&path, w as u32, h as u32, &buf)?;
    // 剪贴板失败不算截图失败：文件已经在盘上了。
    match copy_bgra_to_clipboard(&buf, w, h) {
        Ok(()) => tracing_note("截图已进剪贴板"),
        Err(e) => tracing_note(&format!("截图进了文件，但剪贴板没写进去：{e}")),
    }
    Ok(path)
}

/// 一条标注。坐标都是图像本地坐标。
#[derive(Clone, Debug)]
enum SnipMark {
    /// 方框：(x, y, w, h)
    Rect(i32, i32, i32, i32),
    /// 箭头：(x0, y0, x1, y1)
    Arrow(i32, i32, i32, i32),
    /// 文字：(x, y, 文本)
    Text(i32, i32, String),
}

/// 截图覆盖层的拖选状态（同一时刻只允许一个）。
struct SnipState {
    active: bool,
    start: (i32, i32),
    cur: (i32, i32),
    /// 编辑阶段：冻结下来的那一块画面（BGRA，自上而下）与尺寸。
    editing: bool,
    img: Vec<u8>,
    img_w: i32,
    img_h: i32,
    /// 已画的标注（图像本地坐标）。
    marks: Vec<SnipMark>,
    /// 当前工具：0 方框 / 1 箭头 / 2 文字。
    tool: u8,
    /// 正在拖的那一笔的起点。
    drag: Option<(i32, i32)>,
    /// 编辑窗口句柄（存 isize：静态量要 Send）。
    editor: isize,
    /// 文字标注用的那个原生 EDIT（编辑窗口的子窗口）。
    text_edit: isize,
    /// 文字落点（点下去那一刻记下）。
    text_at: (i32, i32),
}

static SNIP: Mutex<SnipState> = Mutex::new(SnipState {
    active: false,
    start: (0, 0),
    cur: (0, 0),
    editing: false,
    img: Vec::new(),
    img_w: 0,
    img_h: 0,
    marks: Vec::new(),
    tool: 0,
    drag: None,
    editor: 0,
    text_edit: 0,
    text_at: (0, 0),
});

/// 编辑窗口底部工具条的高度。
const SNIP_BAR_H: i32 = 40;

fn virtual_screen_origin() -> (i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
        )
    }
}

unsafe extern "system" fn snip_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_LBUTTONDOWN => {
                let x = (l.0 & 0xFFFF) as i16 as i32;
                let y = ((l.0 >> 16) & 0xFFFF) as i16 as i32;
                if let Ok(mut s) = SNIP.lock() {
                    s.active = true;
                    s.start = (x, y);
                    s.cur = (x, y);
                }
                let _ = SetCapture(hwnd);
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let x = (l.0 & 0xFFFF) as i16 as i32;
                let y = ((l.0 >> 16) & 0xFFFF) as i16 as i32;
                let mut changed = false;
                if let Ok(mut s) = SNIP.lock() {
                    if s.active && s.cur != (x, y) {
                        s.cur = (x, y);
                        changed = true;
                    }
                }
                if changed {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let x = (l.0 & 0xFFFF) as i16 as i32;
                let y = ((l.0 >> 16) & 0xFFFF) as i16 as i32;
                let (sx, sy) = SNIP.lock().map(|s| s.start).unwrap_or((0, 0));
                let _ = ReleaseCapture();
                let rx = sx.min(x);
                let ry = sy.min(y);
                let rw = (sx - x).abs();
                let rh = (sy - y).abs();
                // 覆盖层起点是虚拟屏幕左上角，转成屏幕坐标再截
                let (vx, vy) = virtual_screen_origin();
                let _ = DestroyWindow(hwnd);
                if rw >= 4 && rh >= 4 {
                    match capture_rect(vx + rx, vy + ry, rw, rh) {
                        Ok(buf) => {
                            if let Ok(mut s) = SNIP.lock() {
                    s.img = buf;
                    s.img_w = rw;
                    s.img_h = rh;
                    s.marks.clear();
                    s.tool = 0;
                    s.drag = None;
                    s.text_edit = 0;
                }
                            // 编辑窗口贴在选区原位，底下露出的就是刚截的那块
                            open_snip_editor(vx + rx, vy + ry, rw, rh);
                        }
                        Err(e) => {
                            if let Ok(mut s) = SNIP.lock() {
                                s.active = false;
                            }
                            tracing_note(&format!("截图失败：{e}"));
                        }
                    }
                } else {
                    if let Ok(mut s) = SNIP.lock() {
                        s.active = false;
                    }
                    tracing_note("截图取消：选区太小");
                }
                LRESULT(0)
            }
            WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_KEYDOWN if w.0 == 0x1B => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_PAINT => {
                // 只画选区边框：底色是整窗统一。
let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                if let Ok(s) = SNIP.lock() {
                    if s.active {
                        let pen = CreatePen(PS_SOLID, 2, COLORREF(0x00F7_9C_5B));
                        let old = SelectObject(hdc, pen.into());
                        let null = GetStockObject(NULL_BRUSH);
                        let oldb = SelectObject(hdc, null);
                        let _ = Rectangle(
                            hdc,
                            s.start.0.min(s.cur.0),
                            s.start.1.min(s.cur.1),
                            s.start.0.max(s.cur.0),
                            s.start.1.max(s.cur.1),
                        );
                        SelectObject(hdc, oldb);
                        SelectObject(hdc, old);
                        let _ = DeleteObject(pen.into());
                    }
                }
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_DESTROY => LRESULT(0),
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }
}

/// 编辑窗口底部按钮：左边三个工具（方框/箭头/文字），右边三个动作（撤销/保存/取消）。
fn snip_buttons(client_w: i32, client_h: i32) -> Vec<(&'static str, (i32, i32, i32, i32))> {
    let bw = 64;
    let bh = 26;
    let y = client_h - SNIP_BAR_H + 7;
    let mut out = Vec::new();
    for (i, label) in ["方框", "箭头", "文字"].iter().enumerate() {
        out.push((*label, (8 + i as i32 * 72, y, bw, bh)));
    }
    let mut x = client_w - 8 - bw;
    for label in ["取消", "保存", "撤销"] {
        out.push((label, (x, y, bw, bh)));
        x -= bw + 8;
    }
    out
}

fn hit_box(b: (i32, i32, i32, i32), x: i32, y: i32) -> bool {
    x >= b.0 && x < b.0 + b.2 && y >= b.1 && y < b.1 + b.3
}

/// 画一个箭头（线 + 两条 25° 的头）。
fn draw_arrow(hdc: HDC, x0: i32, y0: i32, x1: i32, y1: i32) {
    unsafe {
        let _ = MoveToEx(hdc, x0, y0, None);
        let _ = LineTo(hdc, x1, y1);
        let dx = (x1 - x0) as f32;
        let dy = (y1 - y0) as f32;
        let len = (dx * dx + dy * dy).sqrt().max(1.0);
        let (ux, uy) = (-dx / len, -dy / len);
        let a = 25f32.to_radians();
        for s in [1.0f32, -1.0] {
            let (c, sn) = (a.cos(), a.sin() * s);
            let rx = ux * c - uy * sn;
            let ry = ux * sn + uy * c;
            let _ = MoveToEx(hdc, x1, y1, None);
            let _ = LineTo(hdc, (x1 as f32 + rx * 14.0) as i32, (y1 as f32 + ry * 14.0) as i32);
        }
    }
}

/// 把标注烤进像素：内存 DC 贴图 → 画标注 → 读回。
fn render_marks(img: &[u8], w: i32, h: i32, marks: &[SnipMark]) -> Vec<u8> {
    let mut out = img.to_vec();
    if marks.is_empty() || w <= 0 || h <= 0 {
        return out;
    }
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, bmp.into());
        let mut info = BITMAPINFO::default();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = w;
        info.bmiHeader.biHeight = -h;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB.0;
        SetDIBits(
            Some(mem),
            bmp,
            0,
            h as u32,
            img.as_ptr() as *const _,
            &info,
            DIB_RGB_COLORS,
        );

        let pen = CreatePen(PS_SOLID, 2, COLORREF(0x00F7_9C_5B));
        let oldp = SelectObject(mem, pen.into());
        let null = GetStockObject(NULL_BRUSH);
        let oldb = SelectObject(mem, null);
        let font = CreateFontW(
            20,
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
        let oldf = SelectObject(mem, font.into());
        let _ = SetTextColor(mem, COLORREF(0x00F7_9C_5B));
        let _ = SetBkMode(mem, TRANSPARENT);
        for m in marks {
            match m {
                SnipMark::Rect(x, y, rw, rh) => {
                    let _ = Rectangle(mem, *x, *y, x + rw, y + rh);
                }
                SnipMark::Arrow(x0, y0, x1, y1) => draw_arrow(mem, *x0, *y0, *x1, *y1),
                SnipMark::Text(x, y, s) => {
                    let _ = TextOutW(mem, *x, *y, &s.encode_utf16().collect::<Vec<u16>>());
                }
            }
        }
        SelectObject(mem, oldf);
        SelectObject(mem, oldb);
        SelectObject(mem, oldp);
        let _ = DeleteObject(font.into());
        let _ = DeleteObject(pen.into());
        GetDIBits(
            mem,
            bmp,
            0,
            h as u32,
            Some(out.as_mut_ptr() as *mut _),
            &mut info,
            DIB_RGB_COLORS,
        );
        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
    }
    out
}

/// 保存当前编辑结果（标注烤进图）→ PNG + 剪贴板，然后关窗。
fn save_snip_edit() {
    let (img, w, h, marks, editor) = match SNIP.lock() {
        Ok(s) => (s.img.clone(), s.img_w, s.img_h, s.marks.clone(), s.editor),
        Err(_) => return,
    };
    if img.is_empty() {
        return;
    }
    let buf = render_marks(&img, w, h, &marks);
    let dir = focusflow_core::paths::screenshots_dir();
    let _ = std::fs::create_dir_all(&dir);
    let name = format!(
        "screenshot_{}.png",
        chrono::Local::now().format("%Y%m%d_%H%M%S")
    );
    let path = dir.join(name);
    match save_png(&path, w as u32, h as u32, &buf) {
        Ok(()) => tracing_note(&format!("截图已保存：{}", path.display())),
        Err(e) => tracing_note(&format!("截图保存失败：{e}")),
    }
    match copy_bgra_to_clipboard(&buf, w, h) {
        Ok(()) => tracing_note("截图已进剪贴板"),
        Err(e) => tracing_note(&format!("剪贴板没写进去：{e}")),
    }
    if editor != 0 {
        let _ = unsafe { DestroyWindow(HWND(editor as *mut core::ffi::c_void)) };
    }
}

/// 在编辑窗口里开一个文字输入框（原生 EDIT，自带中文输入法）。
fn open_text_edit(editor: HWND, x: i32, y: i32) {
    unsafe {
        let old = SNIP.lock().map(|s| s.text_edit).unwrap_or(0);
        if old != 0 {
            let _ = DestroyWindow(HWND(old as *mut core::ffi::c_void));
        }
        let cls = HSTRING::from("EDIT");
        let empty = HSTRING::from("");
        let created = CreateWindowExW(
            WS_EX_NOPARENTNOTIFY,
            &cls,
            &empty,
            WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | 0x0080), // WS_CHILD | VISIBLE | ES_AUTOHSCROLL
            x,
            y,
            240,
            28,
            Some(editor),
            None,
            None,
            None,
        );
        if let Ok(h) = created {
            let font = CreateFontW(
                20,
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
            SendMessageW(h, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
            let _ = SetWindowSubclass(h, Some(edit_proc), 3, 0);
            let _ = SetFocus(Some(h));
            if let Ok(mut s) = SNIP.lock() {
                s.text_edit = h.0 as isize;
                s.text_at = (x, y);
            }
        }
    }
}

unsafe extern "system" fn snip_edit_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                let (img, iw, ih, marks, drag) = match SNIP.lock() {
                    Ok(s) => (s.img.clone(), s.img_w, s.img_h, s.marks.clone(), s.drag),
                    Err(_) => (Vec::new(), 0, 0, Vec::new(), None),
                };
                if !img.is_empty() {
                    let mut info = BITMAPINFO::default();
                    info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
                    info.bmiHeader.biWidth = iw;
                    info.bmiHeader.biHeight = -ih;
                    info.bmiHeader.biPlanes = 1;
                    info.bmiHeader.biBitCount = 32;
                    info.bmiHeader.biCompression = BI_RGB.0;
                    let _ = SetDIBitsToDevice(
                        hdc,
                        0,
                        0,
                        iw as u32,
                        ih as u32,
                        0,
                        0,
                        0,
                        ih as u32,
                        img.as_ptr() as *const _,
                        &info,
                        DIB_RGB_COLORS,
                    );
                }
                // 工具条
                let brush = CreateSolidBrush(COLORREF(0x0024_1814));
                let oldb = SelectObject(hdc, brush.into());
                let _ = Rectangle(hdc, 0, ih, iw + 2, ih + SNIP_BAR_H);
                SelectObject(hdc, oldb);
                let _ = DeleteObject(brush.into());

                // 已画的标注 + 正在拖的那一笔
                let pen = CreatePen(PS_SOLID, 2, COLORREF(0x00F7_9C_5B));
                let oldp = SelectObject(hdc, pen.into());
                let null = GetStockObject(NULL_BRUSH);
                let oldnull = SelectObject(hdc, null);
                let mark_font = CreateFontW(
                    20,
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
                let oldmf = SelectObject(hdc, mark_font.into());
                let _ = SetTextColor(hdc, COLORREF(0x00F7_9C_5B));
                for m in &marks {
                    match m {
                        SnipMark::Rect(mx, my, mw, mh) => {
                            let _ = Rectangle(hdc, *mx, *my, mx + mw, my + mh);
                        }
                        SnipMark::Arrow(x0, y0, x1, y1) => draw_arrow(hdc, *x0, *y0, *x1, *y1),
                        SnipMark::Text(tx, ty, s) => {
                            let _ = TextOutW(
                                hdc,
                                *tx,
                                *ty,
                                &s.encode_utf16().collect::<Vec<u16>>(),
                            );
                        }
                    }
                }
                if let Some((dx, dy)) = drag {
                    let (cx, cy, tool) = match SNIP.lock() {
                        Ok(s) => (s.cur.0, s.cur.1, s.tool),
                        Err(_) => (dx, dy, 0),
                    };
                    if tool == 1 {
                        draw_arrow(hdc, dx, dy, cx, cy);
                    } else {
                        let _ = Rectangle(hdc, dx.min(cx), dy.min(cy), dx.max(cx), dy.max(cy));
                    }
                }
                SelectObject(hdc, oldmf);
                let _ = DeleteObject(mark_font.into());
                SelectObject(hdc, oldnull);
                SelectObject(hdc, oldp);
                let _ = DeleteObject(pen.into());

                // 按钮
                let font = CreateFontW(
                    13,
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
                let _ = SetTextColor(hdc, COLORREF(0x00F0_F0_F0));
                let _ = SetBkMode(hdc, TRANSPARENT);
                // 窄窗口不画提示，否则会压在「撤销」按钮上
                if iw > 560 {
                    let _ = TextOutW(
                        hdc,
                        12,
                        ih + 12,
                        &"拖出方框做标注 · Ctrl+S 保存 · Esc 取消"
                            .encode_utf16()
                            .collect::<Vec<u16>>(),
                    );
                }
                let cur_tool = SNIP.lock().map(|s| s.tool).unwrap_or(0);
                for (label, b) in snip_buttons(iw, ih + SNIP_BAR_H) {
                    let is_tool = (label == "方框" && cur_tool == 0)
                        || (label == "箭头" && cur_tool == 1)
                        || (label == "文字" && cur_tool == 2);
                    let bg = if label == "保存" || is_tool {
                        COLORREF(0x00F7_9C_5B)
                    } else {
                        COLORREF(0x0035_2E_2E)
                    };
                    let bmp = CreateSolidBrush(bg);
                    let old = SelectObject(hdc, bmp.into());
                    let _ = Rectangle(hdc, b.0, b.1, b.0 + b.2, b.1 + b.3);
                    SelectObject(hdc, old);
                    let _ = DeleteObject(bmp.into());
                    let _ = SetTextColor(
                        hdc,
                        if label == "保存" || is_tool {
                            COLORREF(0x00FF_FF_FF)
                        } else {
                            COLORREF(0x00F0_F0_F0)
                        },
                    );
                    let _ = TextOutW(
                        hdc,
                        b.0 + 18,
                        b.1 + 6,
                        &label.encode_utf16().collect::<Vec<u16>>(),
                    );
                }
                SelectObject(hdc, oldf);
                let _ = DeleteObject(font.into());
                let _ = EndPaint(hwnd, &mut ps);
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let x = (l.0 & 0xFFFF) as i16 as i32;
                let y = ((l.0 >> 16) & 0xFFFF) as i16 as i32;
                let (iw, ih) = match SNIP.lock() {
                    Ok(s) => (s.img_w, s.img_h),
                    Err(_) => (0, 0),
                };
                for (label, b) in snip_buttons(iw, ih + SNIP_BAR_H) {
                    if hit_box(b, x, y) {
                        match label {
                            "方框" | "箭头" | "文字" => {
                                let tool = match label {
                                    "方框" => 0,
                                    "箭头" => 1,
                                    _ => 2,
                                };
                                if let Ok(mut s) = SNIP.lock() {
                                    s.tool = tool;
                                }
                                let _ = InvalidateRect(Some(hwnd), None, false);
                            }
                            "保存" => save_snip_edit(),
                            "撤销" => {
                                if let Ok(mut s) = SNIP.lock() {
                                    s.marks.pop();
                                }
                                let _ = InvalidateRect(Some(hwnd), None, false);
                            }
                            _ => {
                                let _ = DestroyWindow(hwnd);
                            }
                        }
                        return LRESULT(0);
                    }
                }
                if y < ih {
                    let tool = SNIP.lock().map(|s| s.tool).unwrap_or(0);
                    if tool == 2 {
                        open_text_edit(hwnd, x, y);
                    } else {
                        if let Ok(mut s) = SNIP.lock() {
                            s.drag = Some((x, y));
                            s.cur = (x, y);
                        }
                        let _ = SetCapture(hwnd);
                    }
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let x = (l.0 & 0xFFFF) as i16 as i32;
                let y = ((l.0 >> 16) & 0xFFFF) as i16 as i32;
                let mut changed = false;
                if let Ok(mut s) = SNIP.lock() {
                    if s.drag.is_some() && s.cur != (x, y) {
                        s.cur = (x, y);
                        changed = true;
                    }
                }
                if changed {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let x = (l.0 & 0xFFFF) as i16 as i32;
                let y = ((l.0 >> 16) & 0xFFFF) as i16 as i32;
                let _ = ReleaseCapture();
                if let Ok(mut s) = SNIP.lock() {
                    if let Some((dx, dy)) = s.drag.take() {
                        let rw = (x - dx).abs();
                        let rh = (y - dy).abs();
                        if rw >= 4 && rh >= 4 {
                            let mark = if s.tool == 1 {
                                SnipMark::Arrow(dx, dy, x, y)
                            } else {
                                SnipMark::Rect(dx.min(x), dy.min(y), rw, rh)
                            };
                            s.marks.push(mark);
                        }
                    }
                }
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_EDIT_DONE => {
                // 文字标注：EDIT 回车/失焦后由 edit_proc 投过来
                let (te, at) = SNIP
                    .lock()
                    .map(|s| (s.text_edit, s.text_at))
                    .unwrap_or((0, (0, 0)));
                if te != 0 {
                    let h = HWND(te as *mut core::ffi::c_void);
                    if w.0 == 1 {
                        let mut buf = [0u16; 256];
                        let n = GetWindowTextW(h, &mut buf).max(0) as usize;
                        let text = String::from_utf16_lossy(&buf[..n]);
                        if !text.trim().is_empty() {
                            if let Ok(mut s) = SNIP.lock() {
                                s.marks.push(SnipMark::Text(at.0, at.1, text));
                            }
                        }
                    }
                    let _ = DestroyWindow(h);
                    if let Ok(mut s) = SNIP.lock() {
                        s.text_edit = 0;
                    }
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_KEYDOWN if w.0 == 0x1B => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_KEYDOWN if w.0 == 0x53 && (GetKeyState(0x11) as u16 & 0x8000) != 0 => {
                save_snip_edit();
                LRESULT(0)
            }
            WM_DESTROY => {
                if let Ok(mut s) = SNIP.lock() {
                    s.editing = false;
                    s.editor = 0;
                    s.drag = None;
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }
}

/// 打开标注窗口：贴在选区原位，底部一条工具条。
fn open_snip_editor(x: i32, y: i32, w: i32, h: i32) {
    unsafe {
        let hinst = GetModuleHandleW(None).map(|m| m.into()).unwrap_or_default();
        let cls = HSTRING::from("FocusFlowSnipEditor");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(snip_edit_proc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_CROSS).unwrap_or_default(),
            lpszClassName: PCWSTR(cls.as_ptr()),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);
        let created = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            &cls,
            &HSTRING::from("FocusFlow 标注"),
            WINDOW_STYLE(WS_POPUP.0 | WS_VISIBLE.0),
            x,
            y,
            w,
            h + SNIP_BAR_H,
            None,
            None,
            Some(hinst),
            None,
        );
        match created {
            Ok(ed) => {
                if let Ok(mut s) = SNIP.lock() {
                    s.editor = ed.0 as isize;
                }
                let _ = SetForegroundWindow(ed);
                let _ = SetFocus(Some(ed));
                tracing_note("截图编辑窗口已打开（拖方框标注，按钮保存/撤销/取消）");
            }
            Err(_) => {
                // 开不出来就退回「选完直接存」
                tracing_note("编辑窗口创建失败，直接保存");
                save_snip_edit();
            }
        }
    }
}

/// 打开全屏截图覆盖层（托盘菜单/热键触发）。
fn start_snip(owner: HWND) {
    if let Ok(s) = SNIP.lock() {
        if s.active {
            return;
        }
    }
    unsafe {
        let hinst = GetModuleHandleW(None).map(|m| m.into()).unwrap_or_default();
        let cls = HSTRING::from("FocusFlowSnipOverlay");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(snip_proc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_CROSS).unwrap_or_default(),
            lpszClassName: PCWSTR(cls.as_ptr()),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);

        let (vx, vy) = virtual_screen_origin();
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        let created = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            &cls,
            &HSTRING::from("FocusFlow 截图"),
            WINDOW_STYLE(WS_POPUP.0 | WS_VISIBLE.0),
            vx,
            vy,
            vw,
            vh,
            Some(owner),
            None,
            Some(hinst),
            None,
        );
        match created {
            Ok(ov) => {
                let _ = SetLayeredWindowAttributes(ov, COLORREF(0), 110, LWA_ALPHA);
                let _ = ShowWindow(ov, SW_SHOW);
                let _ = SetForegroundWindow(ov);
                let _ = SetFocus(Some(ov));
                tracing_note("截图覆盖层已打开（拖选区域，右键/Esc 取消）");
            }
            Err(_) => tracing_note("截图覆盖层创建失败"),
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
    },
    Table {
        rect: D2D_RECT_F,
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
        /// 行内操作按钮：(矩形, 动作 id, 按钮文字)
        buttons: Vec<(D2D_RECT_F, String, String)>,
        /// 数据列可用宽度（右侧留给操作按钮）
        column_w: f32,
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
            | POp::Table { rect, .. } => *rect,        }
    }
}

/// 插件视图内容有多高（用于把滚动位置夹在合法范围内）。
fn plugin_content_height(
    view: &focusflow_core::plugins::PluginView,
    area: D2D_RECT_F,
) -> f32 {
    let ops = plugin_view_ops(view, area, 0.0);
    let bottom = ops
        .iter()
        .map(|op| op.bounds().bottom)
        .fold(0.0f32, f32::max);
    (bottom - area.top).max(0.0)
}

/// 把声明式视图排成一条条操作（内容区宽度，纵向堆叠）。
fn plugin_view_ops(
    view: &focusflow_core::plugins::PluginView,
    area: D2D_RECT_F,
    offset: f32,
) -> Vec<POp> {
    use focusflow_core::plugins::Widget;
    let mut ops = Vec::new();
    let x = area.left + 16.0;
    let w = (area.right - area.left) - 32.0;
    let mut y = area.top + 12.0 - offset;

    fn walk(widgets: &[Widget], x: f32, w: f32, y: &mut f32, ops: &mut Vec<POp>) {
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
                    ..
                } => {
                    let bw = ((text.chars().count() as f32) * 15.0 + 28.0).max(96.0);
                    ops.push(POp::Button {
                        rect: rect(x, *y, bw.min(w), 30.0),
                        text: text.clone(),
                        id: id.clone(),
                        disabled: *disabled,
                        modal: modal.clone(),
                    });
                    *y += 38.0;
                }
                Widget::Table {
                    headers,
                    rows,
                    ids,
                    actions,
                    ..
                } => {
                    let shown: Vec<Vec<String>> = rows.iter().take(6).cloned().collect();
                    let h = 24.0 + shown.len() as f32 * 20.0;
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
                                let by = *y + 24.0 + ri as f32 * 20.0 + 1.0;
                                buttons.push((rect(bx, by, bw, 18.0), format!("{prefix}{rid}"), label.clone()));
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
                    });
                    *y += h + 6.0;
                }
                Widget::Pager {
                    page,
                    pages,
                    total,
                    ..
                } => {
                    ops.push(POp::Label {
                        rect: rect(x, *y, w, 18.0),
                        text: format!("第 {page} / {pages} 页 · 共 {total} 条"),
                    });
                    *y += 22.0;
                }
                Widget::Row { children } => {
                    // 横向容器先按纵向堆（子控件宽度有限，硬拆列反而更难读）。
walk(children, x, w, y, ops);
                }
                Widget::ModalForm { .. } => {
                    // 弹窗不再按普通控件排在正文里：`draw_plugin_modal` 把它画成整屏遮罩上
                    // 的一张卡片（点在按钮上会设 `pv_modal`，插件自己 `open=true` 的直接开）。
                    // 留出 0 高度，正文不会因为它多一块空白。
                }
            }
        }
    }

    walk(&view.widgets, x, w, &mut y, &mut ops);
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
    let _ = unsafe { p.rt.PushAxisAlignedClip(&clip, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE) };

    let ops = plugin_view_ops(
        view,
        rect(
            card.left,
            card.top + 40.0,
            card.right - card.left,
            card.bottom - card.top,
        ),
        scroll as f32,
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
            } => {
                p.fill(rect(r.left, r.top, r.right - r.left, 24.0), &p.br.header);
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
                    let ry = r.top + 24.0 + ri as f32 * 20.0;
                    for (ci, cell) in row.iter().enumerate().take(cols) {
                        p.text(
                            cell,
                            rect(r.left + 6.0 + ci as f32 * cw, ry + 2.0, cw - 12.0, 16.0),
                            &p.fmt.small,
                            &p.br.fg,
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

    let _ = unsafe { p.rt.PopAxisAlignedClip() };

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
        let Some(m) = modal_from_widget(w) else { continue };
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
            rect(
                vb.left + 8.0,
                vb.top + 3.0,
                vb.right - vb.left - 28.0,
                18.0,
            ),
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

fn draw_plugins(
    p: &P<'_>,
    w: f32,
    h: f32,
    rows: &[PluginRow],
    err: Option<&str>,
    vs: ViewState,
) {
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
fn generate_weekly_report() -> std::result::Result<std::path::PathBuf, String> {
    use chrono::{Datelike, Duration, Local};
    use focusflow_core::db::queries;
    use std::fmt::Write as _;

    let today = Local::now().date_naive();
    let since_monday = today.weekday().num_days_from_monday() as i64;
    let start = today - Duration::days(since_monday + 7);
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
    keys.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    keys.truncate(10);
    let mut apps: Vec<(String, i64)> = apps.into_iter().collect();
    apps.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    apps.truncate(10);
    let mut devices: Vec<(String, i64)> = devices.into_iter().collect();
    devices.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
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
        let _ = writeln!(out, "\n> 有 {} 个年度库没读全：{}", gaps.len(), gaps.join("；"));
    }

    let _ = writeln!(out, "\n## 每日活跃\n\n| 日期 | 次数 |\n|---|---:|");
    for (d, n) in &by_day {
        let mark = if *n >= goal { " ✅" } else { "" };
        let _ = writeln!(
            out,
            "| {} | {} |",
            d.format("%Y-%m-%d"),
            format!("{}{}", thousands((*n).max(0) as u64), mark)
        );
    }

    let _ = writeln!(out, "\n## 键鼠 TOP 10\n\n| 键 | 次数 |\n|---|---:|");
    for (k, n) in &keys {
        let _ = writeln!(out, "| {} | {} |", k, thousands((*n).max(0) as u64));
    }
    if keys.is_empty() {
        let _ = writeln!(out, "| （这一周没有记录） | 0 |");
    }

    let _ = writeln!(out, "\n## 应用 TOP 10（前台时长）\n\n| 应用 | 时长 |\n|---|---:|");
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

    let dir = focusflow_core::paths::data_dir().join("reports");
    std::fs::create_dir_all(&dir).map_err(|e| format!("建报告目录失败：{e}"))?;
    let file = dir.join(format!("weekly-{}.md", start.format("%Y-%m-%d")));
    std::fs::write(&file, out).map_err(|e| format!("写周报失败：{e}"))?;
    Ok(file)
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

/// 把当前快照写成一�?CSV（概�?+ 四个排行 + 每日/每小时）�?///
/// 注：这是原生。
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
        "<h1>FocusFlow 导出</h1><p>生成时间 {} · 统计周期 {}</p>\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        html_escape(&period_text(s.period))
    ));
    out.push_str("<table><tr><th>今日</th><th>周期总计</th><th>日均</th></tr>");
    out.push_str(&format!(
        "<tr><td>{}</td><td>{}</td><td>{}</td></tr></table>\n",
        s.today, s.period_total, s.avg
    ));

    out.push_str("<h2>键鼠排行</h2><table><tr><th>排名</th><th>键名</th><th>次数</th><th>占比</th></tr>");
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

    out.push_str("<h2>设备排行</h2><table><tr><th>排名</th><th>设备</th><th>类型</th><th>次数</th></tr>");
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

    out.push_str("<h2>每日次数（近 30 天）</h2><table><tr><th>日期</th><th>次数</th></tr>");
    let today = chrono::Local::now().date_naive();
    let n = s.daily.len();
    for (i, v) in s.daily.iter().enumerate() {
        let d = today - chrono::Duration::days(n.saturating_sub(1 + i) as i64);
        out.push_str(&format!(
            "<tr><td>{}</td><td class=\"num\">{v}</td></tr>",
            d.format("%Y-%m-%d")
        ));
    }
    out.push_str("</table>\n");

    out.push_str("<h2>每小时合计</h2><table><tr><th>小时</th><th>次数</th></tr>");
    for (h, v) in s.hourly.iter().enumerate() {
        out.push_str(&format!("<tr><td>{h}:00</td><td class=\"num\">{v}</td></tr>"));
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
    let today = chrono::Local::now().date_naive();
    let n = s.daily.len();
    let daily: Vec<serde_json::Value> = s
        .daily
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let d = today - chrono::Duration::days(n.saturating_sub(1 + i) as i64);
            json!({"date": d.format("%Y-%m-%d").to_string(), "count": v})
        })
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

fn export_csv_text(s: &Snapshot) -> String {
    let mut out = String::new();
    out.push_str("FocusFlow 导出\r\n");
    out.push_str(&format!(
        "生成时间,{}\r\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    ));
    out.push_str(&format!("统计周期,{}\r\n", period_text(s.period)));
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
        out.push_str(&format!(
            "{},{},{},{share:.1}%\r\n",
            i + 1,
            csv_field(k),
            v
        ));
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

    out.push_str("\r\n[每日次数(近30天)]\r\n日期,次数\r\n");
    let today = chrono::Local::now().date_naive();
    let n = s.daily.len();
    for (i, v) in s.daily.iter().enumerate() {
        let d = today - chrono::Duration::days((n.saturating_sub(1 + i)) as i64);
        out.push_str(&format!("{},{}\r\n", d.format("%Y-%m-%d"), v));
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
    let (title, total, count) = match tab {
        1 => ("应用排行", snap.app_total, snap.apps.len()),
        2 => (
            "设备排行",
            snap.devices.iter().map(|d| d.count).sum(),
            snap.devices.len(),
        ),
        _ => ("键鼠排行", snap.period_total, snap.rank.len()),
    };
    // 滚动位置按当前页签的行数夹一次：数据变少时别停在空白处。
let scroll = vs.scroll.min(count.saturating_sub(g.max_rows));

    p.text(
        title,
        rect(r.left + g.pad, r.top + 10.0, 120.0, 20.0),
        &p.fmt.body,
        &p.br.fg,
    );
    p.text(
        &if tab == 1 {
            format!("共 {}", duration_text(total))
        } else {
            format!("共 {} 次", thousands(total.max(0) as u64))
        },
        rect(r.left + g.pad + 74.0, r.top + 12.0, 200.0, 18.0),
        &p.fmt.small,
        &p.br.muted,
    );

    // 表头
    p.fill(
        rect(r.left + g.pad, g.head_y, r.right - r.left - g.pad * 2.0, 22.0),
        &p.br.header,
    );
    let heads = match tab {
        1 => ["排名", "应用", "时长", "占比"],
        2 => ["排名", "设备名", "次数", "操作"],
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
                focusflow_core::config::instance().get_int("database", "flush_interval", 10).max(1)
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
        let idx = scroll + vis;
        if idx >= count {
            break;
        }
        let y = g.list_top + vis as f32 * g.row_h;
        let row = rect(r.left + g.pad, y, r.right - r.left - g.pad * 2.0, g.row_h - 1.0);
        if vs.sel == Some(idx) {
            p.round(row, 4.0, &p.br.accent_soft, None);
        } else if vs.hover == Some(idx) || vis % 2 == 1 {
            p.fill(row, &p.br.header);
        }
        p.text(
            &format!("{}", idx + 1),
            rect(g.x_rank, y + 4.0, 40.0, 18.0),
            &p.fmt.small,
            if idx < 3 { &p.br.accent } else { &p.br.muted },
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
    let gap_note = if snap.gaps > 0 {
        format!(" · {} 个年度库没读上（上述数字是下界）", snap.gaps)
    } else {
        String::new()
    };
    p.text(
        &format!("数据源：{}{gap_note}", snap.data_home),
        rect(r.left + g.pad, foot_y, r.right - r.left - g.pad * 2.0, 16.0),
        &p.fmt.small,
        &p.br.muted,
    );
}

fn draw_line_chart(p: &P<'_>, r: D2D_RECT_F, snap: &Snapshot) {
    p.round(r, 14.0, &p.br.card, Some(&p.br.border));
    let pad = 12.0;
    p.text(
        "每日活跃趋势",
        rect(r.left + pad, r.top + pad, 160.0, 20.0),
        &p.fmt.body,
        &p.br.fg,
    );
    p.text(
        "近 30 天",
        rect(r.right - pad - 70.0, r.top + pad + 2.0, 70.0, 18.0),
        &p.fmt.right,
        &p.br.muted,
    );

    let plot = rect(
        r.left + 46.0,
        r.top + 46.0,
        (r.right - r.left) - 62.0,
        (r.bottom - r.top) - 74.0,
    );
    for i in 0..=4 {
        let y = plot.top + plot_height(plot) * i as f32 / 4.0;
        p.line(plot.left, y, plot.right, y, &p.br.grid, 1.0);
    }

    let values = &snap.daily;
    let peak = values.iter().cloned().max().unwrap_or(0).max(1) as f64;
    let max = peak * 1.12;
    let n = values.len().max(2);
    let pt = |i: usize| {
        let v = values.get(i).cloned().unwrap_or(0) as f64;
        let x = plot.left + (i as f32 / (n - 1) as f32) * plot_width(plot);
        let y = plot.bottom - (v / max) as f32 * plot_height(plot);
        Vector2 { X: x, Y: y }
    };
    for i in 1..n {
        let a = pt(i - 1);
        let b = pt(i);
        unsafe {
            let _ = p.rt.DrawLine(a, b, &p.br.accent, 2.0, None);
        }
    }
    // 纵轴：从上往下是「峰。
for i in 0..=4 {
        let frac = 1.0 - i as f32 / 4.0;
        let y = plot.top + plot_height(plot) * (1.0 - frac);
        p.text(
            &thousands((peak * frac as f64) as u64),
            rect(plot.left - 48.0, y - 7.0, 44.0, 14.0),
            &p.fmt.right,
            &p.br.muted,
        );
    }
    p.text(
        "30 天前",
        rect(plot.left, plot.bottom + 4.0, 60.0, 14.0),
        &p.fmt.small,
        &p.br.muted,
    );
    p.text(
        "今天",
        rect(plot.right - 30.0, plot.bottom + 4.0, 30.0, 14.0),
        &p.fmt.right,
        &p.br.muted,
    );
}

fn draw_hourly(p: &P<'_>, r: D2D_RECT_F, snap: &Snapshot) {
    p.round(r, 14.0, &p.br.card, Some(&p.br.border));
    let pad = 12.0;
    p.text(
        "小时分布",
        rect(r.left + pad, r.top + pad, 120.0, 20.0),
        &p.fmt.body,
        &p.br.fg,
    );
    p.text(
        &period_text(snap.period),
        rect(r.right - pad - 60.0, r.top + pad + 2.0, 60.0, 18.0),
        &p.fmt.right,
        &p.br.muted,
    );

    let plot = rect(
        r.left + pad,
        r.top + 42.0,
        (r.right - r.left) - pad * 2.0,
        (r.bottom - r.top) - 62.0,
    );
    let values = &snap.hourly;
    let max = values.iter().cloned().fold(0_i64, i64::max).max(1) as f32;
    let n = values.len().max(1);
    let gap = 3.0;
    let bw = ((plot_width(plot) - gap * (n as f32 - 1.0)) / n as f32).max(1.0);
    for (i, v) in values.iter().enumerate() {
        let hp = (*v as f32 / max) * plot_height(plot);
        let x = plot.left + i as f32 * (bw + gap);
        let bar = rect(x, plot.bottom - hp, bw, hp);
        if let Some(b) = p.brush(p.pal.accent) {
            p.round(bar, 2.0, &b, None);
        }
    }
    for i in (0..n).step_by(4) {
        let x = plot.left + i as f32 * (bw + gap) + bw / 2.0;
        p.text(
            &format!("{i}"),
            rect(x - 12.0, plot.bottom + 2.0, 24.0, 14.0),
            &p.fmt.center_small,
            &p.br.muted,
        );
    }
}

// ===== 托盘 / 热键 / 单实�?=====

fn make_tray(snap: &Arc<Mutex<Snapshot>>) -> (Option<TrayIcon>, Option<CheckMenuItem>) {
    let today = snap.lock().map(|s| s.today).unwrap_or(0);
    let menu = Menu::new();
    let show = MenuItem::with_id("show", "显示主窗口", true, None);
    let snip = MenuItem::with_id("snip", "截图（选区）", true, None);
    let floating_item = MenuItem::with_id("floating", "悬浮窗", true, None);
    // 暂停是运行时开关（不写配置）：它只停"计数"，不改变"是否启用采集"这件事。
    let pause = CheckMenuItem::with_id("pause", "暂停采集", true, collect::is_paused(), None);
    let quit = MenuItem::with_id("quit", "退出", true, None);
    let sep = PredefinedMenuItem::separator();
    if let Err(e) = menu.append_items(&[&show, &snip, &floating_item, &pause, &sep, &quit]) {
        tracing_note(&format!("托盘菜单构建失败：{e}"));
        return (None, None);
    }

    let icon = match Icon::from_rgba(tray_icon_rgba(32), 32, 32) {
        Ok(i) => i,
        Err(e) => {
            tracing_note(&format!("托盘图标构建失败：{e}"));
            return (None, None);
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
/// `other_raw` 是同段里另一条热键的当前值：两条撞在一起时第二条必然注册失败，
/// 那会让界面显示"已经设上了"、实际却按不出来，所以在这里就拒掉。
fn combo_check(key: &str, raw: &str, other_raw: &str) -> Option<String> {
    let value = raw.trim();
    let Some(hk) = parse_combo(value) else {
        return Some(format!(
            "{value:?} 不成一个组合；要写成一个修饰键加一个主键，如 ctrl+shift+f"
        ));
    };
    let other_label = if key == "toggle_window" {
        "区域截图热键"
    } else {
        "唤出窗口热键"
    };
    if parse_combo(other_raw) == Some(hk) {
        return Some(format!("{value} 已经被「{other_label}」占用，两条热键要分开"));
    }
    None
}

/// `[hotkey]` 里这一条组合当前配置的原串（文件里没有或写着空 → 核心默认值）。
/// 设置页显示的就是它，注册的键是它解析出来的，所以两边不会分叉。
fn hotkey_config_raw(key: &str) -> String {
    let defaults = focusflow_core::config::default_config();
    let def = defaults
        .get("hotkey")
        .and_then(|s| s.get(key))
        .cloned()
        .unwrap_or_default();
    focusflow_core::config::instance().get_or("hotkey", key, &def)
}

/// 拿配置里的另一条热键做重复检查（`combo_check` 的那份判据）。
fn combo_reject_reason(key: &str, raw: &str) -> Option<String> {
    let other = if key == "toggle_window" {
        "snip"
    } else {
        "toggle_window"
    };
    combo_check(key, raw, &hotkey_config_raw(other))
}

/// 读 `[hotkey] <key>` 并注册它；成功返回组合，失败返回 `None`（这一条不占用全局键）。
/// 日志里带 `key = 原串` 这段 ASCII，验收脚本据此核对"配置写的"和"注册的"是不是同一个。
fn register_combo(manager: Option<&GlobalHotKeyManager>, key: &str) -> Option<HotKey> {
    let manager = manager?;
    let raw = hotkey_config_raw(key);
    let hk = match parse_combo(&raw) {
        Some(hk) => hk,
        None => {
            tracing_note(&format!("热键 {key} = {raw:?} 解析失败：这一条没有注册"));
            return None;
        }
    };
    match manager.register(hk) {
        Ok(()) => {
            tracing_note(&format!("热键已注册 {key} = {raw}"));
            Some(hk)
        }
        Err(e) => {
            tracing_note(&format!("热键注册失败 {key} = {raw}：{e}"));
            None
        }
    }
}

/// 注销此刻真正注册着的组合（总开关关掉、或改完要重注册时先走这一步）。
fn unregister_combos(manager: Option<&GlobalHotKeyManager>, list: [Option<HotKey>; 2]) {
    let Some(manager) = manager else { return };
    let all: Vec<HotKey> = list.into_iter().flatten().collect();
    if !all.is_empty() {
        let _ = manager.unregister_all(&all);
    }
}

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

fn make_hotkeys() -> (
    Option<GlobalHotKeyManager>,
    Option<HotKey>,
    Option<HotKey>,
) {
    let manager = match GlobalHotKeyManager::new() {
        Ok(m) => Some(m),
        Err(e) => {
            tracing_note(&format!("热键管理器创建失败：{e}"));
            None
        }
    };
    let toggle = register_combo(manager.as_ref(), "toggle_window");
    let snip = register_combo(manager.as_ref(), "snip");
    (manager, toggle, snip)
}

/// 托盘图标：一个蓝色圆点。真实版本换。
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

// ===== 小工�?=====

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
    let f = text_format(dw, size, DWRITE_FONT_WEIGHT_NORMAL)?;
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
        if i > 0 && (s.len() - i) % 3 == 0 {
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
    tracing_note(if want { "悬浮窗：显示" } else { "悬浮窗：隐藏" });
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
/// 为什么用子类化而不是在父窗口里�?`WM_COMMAND`：单�?EDIT �?Enter 不会变成
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
        // 不在这里�?失焦即提�?：窗口不是前台时 `SetFocus` 不会真正生效�?        // EDIT 刚建出来就会收到一次假�?WM_KILLFOCUS，提交的会是预填的旧名字
        // �?026-10-26 实测）。提交由 Enter/Esc �?点编辑框外面"两条路负责。
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
            if n % 50 == 0 {
                tracing_note(&format!("timer tick {n}"));
            }
            // 验收模式（FOCUSFLOW_ACCEPTANCE=1）下每秒写一行机器可读的状态：
            // 脚本不用 OCR 就能断言「今日涨了几个」「暂停期间有没有停」。
            // 键名用 ASCII，值也只有数字/on-off，解析起来不用管编码。
            if n % 10 == 0 && ACCEPTANCE.load(std::sync::atomic::Ordering::Relaxed) {
                if let Some(app) = unsafe { app_ptr(hwnd) } {
                    let today = app.snap.lock().map(|s| s.today).unwrap_or(0);
                    tracing_note(&format!(
                        "验收状态 today={today} cpm={} collector={} paused={}",
                        collect::cpm(),
                        if collect::is_started() { "on" } else { "off" },
                        if collect::is_paused() { "true" } else { "false" }
                    ));
                }
            }
            // 托盘点击 / 菜单 / 热键只在事件通道里，靠这 100ms 一拍取出来。
while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
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
                    "snip" => start_snip(hwnd),
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
                // 两个热键共用一个通道：按 id 分流，否则截图的键会把窗口唤出来。
                // 组合键现在能被用户改掉，所以只响应"此刻真正注册着的那两个 id"，
                // 刚注销/换掉的旧 id 投过来的事件一律不认。
                let role = unsafe { app_ptr(hwnd) }.and_then(|app| {
                    if app.snip_hk.map(|h| h.id() == ev.id).unwrap_or(false) {
                        Some("snip")
                    } else if app.hotkey_hk.map(|h| h.id() == ev.id).unwrap_or(false) {
                        Some("toggle")
                    } else {
                        None
                    }
                });
                match role {
                    Some("snip") => {
                        tracing_note("收到截图热键");
                        start_snip(hwnd);
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
            start_snip(hwnd);
            LRESULT(0)
        }
        WM_OP_DONE => {            if let Some(app) = unsafe { app_ptr(hwnd) } {
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
                        let next =
                            (sel.scroll as i32 + step).clamp(0, max as i32) as usize;
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
                            pm.get_plugin(&name)
                                .and_then(|p| p.view.as_ref())
                                .cloned()
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
        WM_KEYDOWN => {
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
                            pm.get_plugin(&name)
                                .and_then(|p| p.view.as_ref())
                                .cloned()
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
            // 关窗�?= 收进托盘（真正退出走托盘菜单），与现有版本同一口径。
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

/// 贴系统材质：成功就整窗透明 + 卡片半透明；失败（Win10/远程桌面/透明效果关闭。
/// 就保持不透明深色 —�?与现�?Tauri �?`apply_window_vibrancy` 同一口径�?///
/// 注意：透明只是"贴上了材。
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

/// 应用图标：优先用程序目录下的 `assets/focusflow.ico`（打包时随 exe 一起放），
/// 没有就退回系统的默认图标——不因为缺一个图标就起不来。
fn load_app_icon() -> HICON {
    unsafe {
        let candidates = [
            focusflow_core::paths::app_dir()
                .join("assets")
                .join("focusflow.ico"),
            std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(|d| d.join("assets").join("focusflow.ico")))
                .unwrap_or_default(),
        ];
        for p in candidates {
            if !p.exists() {
                continue;
            }
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
                .filter_map(|s| s.trim().parse().ok())
                .collect();
            if nums.len() != 4 {
                tracing_note("截图测试：参数要写成 x,y,w,h;out.png");
                return;
            }
            match capture_rect(nums[0], nums[1], nums[2], nums[3]) {
                Ok(buf) => {
                    match save_png(&out, nums[2] as u32, nums[3] as u32, &buf) {
                        Ok(()) => tracing_note(&format!("截图测试：已写 {}", out.display())),
                        Err(e) => tracing_note(&format!("截图测试写盘失败：{e}")),
                    }
                    // 走同一条剪贴板路径，验收才能一起看
                    match copy_bgra_to_clipboard(&buf, nums[2], nums[3]) {
                        Ok(()) => tracing_note("截图测试：已进剪贴板"),
                        Err(e) => tracing_note(&format!("截图测试：剪贴板失败 {e}")),
                    }
                }
                Err(e) => tracing_note(&format!("截图测试失败：{e}")),
            }
        });
    }
    if std::env::var("FOCUSFLOW_SNIP_OVERLAY").is_ok() {
        let raw = hwnd.0 as isize;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1500));
            // 窗口必须建在跑消息循环的线程上，所以这里只投一条消息过去。
            let target = HWND(raw as *mut core::ffi::c_void);
            let _ = unsafe { PostMessageW(Some(target), WM_SNIP_START, WPARAM(0), LPARAM(0)) };
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

#[cfg(test)]
mod hotkey_tests {
    use super::*;

    /// 设置页里显示的就是核心默认值，注册时解析的也是它：默认值解析不了 = 一开机就分叉。
    #[test]
    fn the_core_defaults_parse_into_hotkeys() {
        let defaults = focusflow_core::config::default_config();
        let hotkey = defaults.get("hotkey").expect("核心默认里有 [hotkey] 段");
        for key in ["toggle_window", "snip", "snip_annotate"] {
            let raw = hotkey.get(key).unwrap_or_else(|| panic!("[hotkey] 默认缺 {key}"));
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
        let why = combo_check("snip", "control+shift+f", "ctrl+shift+f").expect("换写法躲不掉判重");
        assert!(why.contains("占用"), "要说清被谁占用：{why}");
        assert!(why.contains("唤出窗口热键"), "要点名另一条的中文名：{why}");
    }

    #[test]
    fn two_different_combos_are_accepted() {
        assert_eq!(combo_check("snip", "shift+f2", "shift+f1"), None);
        assert_eq!(combo_check("toggle_window", "ctrl+alt+9", "shift+f1"), None);
        // 三个修饰键也算一个合法组合，别把它当坏值拒了。
        assert!(parse_combo("ctrl+shift+alt+f").is_some());
    }

    /// 裸键能解析成 HotKey，但注册上去会在整个系统里吞掉那个键，必须当坏值拒掉。
    #[test]
    fn a_bare_key_is_refused_and_explained() {
        assert_eq!(parse_combo("f1"), None, "裸键 f1 不该当成可用的全局热键");
        for bad in ["", "   ", "f1", "ctrl+shift", "ctrl++f", "shift+f1+f2", "乱码"] {
            let why = combo_check("toggle_window", bad, "shift+f1")
                .unwrap_or_else(|| panic!("{bad:?} 应该被拒"));
            assert!(why.contains("修饰键"), "{bad:?} 的说明要给能照写的格式：{why}");
        }
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
