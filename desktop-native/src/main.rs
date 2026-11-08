// 产品形态是 GUI：不要控制台窗口。
#![windows_subsystem = "windows"]

//! FocusFlow 原生主界面 —— 原型（P0 平台 + P1a 合成渲染 + P1b 表格交互）
//! 不引入任何 UI 框架：窗口用 `CreateWindowExW`、界面 D3D11/D2D、文字 DirectWrite。
//! 托盘用 `tray-icon`、热键用 `global-hotkey`；行内改名用**原生 EDIT 子窗口**。
//!
//! 滚动容器、六个视图、截图覆盖层与贴图都已做完；剩下的缺口清单在
//! `docs/handoff-native-migration.md`。
mod clipboard_page;
mod clipboard_popup;
mod collect;
mod dashboard;
mod data;
mod floating;
mod floating_render;
mod snip;

use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::path::PathBuf;
use std::sync::atomic::AtomicI64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use focusflow_core::db::queries;
use focusflow_core::db::queries::DeviceDetail;
use focusflow_core::plugins::manager::PluginManager;
use focusflow_core::window_state::MainWindowState;
// 导出的三条规则各只有一份：CSV 字段转义（core 那份带 Excel 公式注入的前缀防护）、HTML
// 转义、紧凑时长（悬浮窗那一列的形状）。原生侧以前各抄了一份，其中 CSV 那份**漏了**
// 公式防护，而屏上「导出 CSV」那颗按钮走的就是漏掉的那一份。
use crate::floating_render::fmt_duration as duration_text;
use focusflow_core::format::{csv_field, html_escape};

use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
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
    BeginPaint, ClientToScreen, CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, EndPaint,
    FillRect, FrameRect, GetMonitorInfoW, GetStockObject, GetTextExtentPoint32W, InvalidateRect,
    LineTo, MonitorFromWindow, MoveToEx, Rectangle, ScreenToClient, SelectObject, SetBkColor,
    SetBkMode, SetTextColor, StretchDIBits, TextOutW, UpdateWindow, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS,
    FW_NORMAL, FW_SEMIBOLD, HBRUSH, HDC, HFONT, HGDIOBJ, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    NULL_BRUSH, OUT_DEFAULT_PRECIS, PAINTSTRUCT, PS_SOLID, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
// WM_MOUSELEAVE 在 UI::Controls 下（不在 WindowsAndMessaging）：写成裸名字会被当成
// 匹配任意值的绑定，把后面的滚轮/右键/Esc/关窗分支全变成不可达。
use windows::Win32::UI::Controls::{MARGINS, WM_MOUSELEAVE};
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, GetSystemMetricsForDpi, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetFocus, GetKeyState, ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent, TME_LEAVE,
    TRACKMOUSEEVENT,
};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows_numerics::Vector2;

use data::Snapshot;

const CLASS_NAME: PCWSTR = w!("FocusFlowNativeD2D");
/// 行内编辑的宿主窗类名（顶层弹出窗，见 [`edit_host_proc`] 与 `App::ensure_edit`）。
const EDIT_HOST_CLASS: PCWSTR = w!("FocusFlowEditHost");
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
/// 搜索框那个原生 EDIT 的焦点变化：`WPARAM` 1=拿到焦点、0=交出去。
/// 父窗要靠它决定"这一帧该不该在框里写提示文案"——控件有焦点时那块矩形归控件画。
const WM_SEARCH_FOCUS: u32 = WM_APP + 15;
/// 剪贴板管理页的搜索框焦点回执（与设置页那个搜索框分开：两页各有一份 EDIT）。
const WM_CLIP_SEARCH_FOCUS: u32 = WM_APP + 16;
/// 宿主 EDIT 的文字变了（EN_CHANGE）。宿主不做别的，只把"哪个 EDIT"转给主窗；
/// 主窗再看它是不是剪贴板页的搜索框 —— 是就地筛，不是等 Enter。
const WM_EDIT_CHANGED: u32 = WM_APP + 17;
/// 两个 EDIT 各用哪一个子类 id。子类过程收到的第 5 个参数就是这个 id，
/// 第 6 个参数是 `SetWindowSubclass` 的 `dwRefData`（两处都传 0，**不能拿它分角色**——
/// 从前拿它比 2，于是搜索框的焦点永远递不回来，提示文案照旧压在控件上，2026-11-01 屏上量到）。
const RENAME_EDIT_SUBCLASS: usize = 1;
const SEARCH_EDIT_SUBCLASS: usize = 2;
const CLIP_SEARCH_EDIT_SUBCLASS: usize = 3;

/// 哪一条消息要把焦点变化递回父窗：设置页与剪贴板页的两个搜索框。
///
/// 抽成纯函数是因为这条缺陷的形状是"参数拿错"——`edit_proc` 里比错一位，屏上只是"提示没让位"，
/// 编译、lint、现有腿全都不会红。id 与消息的对应关系放在这里就能被用例钉住。
fn search_focus_signal(id: usize, msg: u32) -> Option<bool> {
    if id != SEARCH_EDIT_SUBCLASS && id != CLIP_SEARCH_EDIT_SUBCLASS {
        return None;
    }
    match msg {
        WM_SETFOCUS => Some(true),
        WM_KILLFOCUS => Some(false),
        _ => None,
    }
}

/// 剪贴板页搜索框里，哪些键该转给页面而不是留给文本框。
///
/// 单行 EDIT 对这些键没有文本语义：Tab 是页脚写着的「切模式」，↑↓ / PgUp / PgDn 是列表
/// 导航。Delete / Home / End 留在文本框里（删字与移光标）；Enter / Esc 另有
/// `WM_EDIT_DONE` 那条回执。设置页的搜索框不在此列：它后面没有列表。
fn clip_search_key_goes_to_page(id: usize, msg: u32, vk: usize) -> bool {
    id == CLIP_SEARCH_EDIT_SUBCLASS
        && msg == WM_KEYDOWN
        && matches!(vk, 0x09 | 0x21 | 0x22 | 0x26 | 0x28)
}
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
/// 背板修复里那次 hide/show 会再发一轮 WM_SIZE：这个闩防止递归。
static BACKDROP_FIX: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// 主窗口这一趟的"最大化"意图：启动时从 `window_state.ini` 恢复，用户在屏上最大化/还原
/// 时跟着变。`WM_SIZE` 拿它判"是不是真的换了一档"，只在换档那一拍写一次存档。
static MAIN_MAXIMIZED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// 静默启动那一趟：存档说"上次是最大化"，但窗口建出来是隐藏的 —— 真正最大化要等第一次
/// 唤出时补（`ShowWindow(SW_MAXIMIZE)` 会连显示一起做，不能在建窗那一下调）。
static MAIN_MAXIMIZE_ON_SHOW: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

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
    /// 卡片/浮层的落影底色（逐环 alpha 见 [`SHADOW_RINGS`]）。alpha 为 0 = 这个主题不画阴影。
    pub shadow: D2D1_COLOR_F,
    /// 卡片/浮层顶部那条 1px 高光。alpha 为 0 = 不画（浅色实心档）。
    pub highlight: D2D1_COLOR_F,
    /// 表格斑马纹。**单开一格**：以前斑马纹、表头、悬停共用 `header`，
    /// 奇数行悬停时颜色一点不变，看着像"这一行点了没反应"。
    pub zebra: D2D1_COLOR_F,
    /// 行悬停底色（压在斑马纹之上，表格/设置/插件三处共用）。
    pub row_hover: D2D1_COLOR_F,
    /// 控件悬停叠加层：盖在按钮/药丸自己的底色上，白底与主色底上都成立。
    pub hover: D2D1_COLOR_F,
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

/// 投影 = 6 圈同心圆角矩形，外→内 alpha 递增，叠出来是一条软边。
///
/// 不用 `ID2D1Effect`（那是另一套设备资源，Mica 合成路径上拿不到，本仓也没有
/// effect 资源管线）；6 次 `FillRoundedRectangle` 每帧不到 100 次，不值得引新管线。
const SHADOW_RINGS: [(f32, f32); 6] = [
    (1.0, 0.030),
    (2.0, 0.024),
    (4.0, 0.018),
    (6.0, 0.013),
    (8.0, 0.009),
    (10.0, 0.006),
];

fn light_theme(glass: bool) -> Theme {
    let (page, card, header, border) = if glass {
        (
            color_a(0xF2, 0xF6, 0xFC, 0.86),
            color_a(0xFF, 0xFF, 0xFF, 0.62),
            color_a(0xF1, 0xF5, 0xFB, 0.72),
            color(0xE5, 0xEC, 0xF8),
        )
    } else {
        (
            color(0xF2, 0xF6, 0xFC),
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
        muted: color(0x59, 0x60, 0x74),
        accent: color(0x4D, 0x8C, 0xF7),
        accent_soft: color(0xE8, 0xF0, 0xFE),
        grid: color(0xE3, 0xEA, 0xF6),
        white: color(0xFF, 0xFF, 0xFF),
        success: color(0x10, 0xB9, 0x81),
        danger: color(0xE5, 0x48, 0x4D),
        modal: color(0xFF, 0xFF, 0xFF),
        overlay: color_a(0x18, 0x28, 0x50, 0.35),
        page,
        // 带一点蓝的深色：纯黑落在淡蓝页面上像脏污。
        shadow: color(0x1B, 0x2A, 0x4A),
        // 玻璃档的卡片是半透明的，阴影会从卡片里面透出来 ⇒ 只留顶部一条高光。
        highlight: if glass {
            color_a(0xFF, 0xFF, 0xFF, 0.30)
        } else {
            color_a(0xFF, 0xFF, 0xFF, 0.0)
        },
        zebra: color_a(0x4D, 0x8C, 0xF7, 0.045),
        row_hover: color_a(0x4D, 0x8C, 0xF7, 0.10),
        hover: color_a(0x0F, 0x17, 0x2A, if glass { 0.06 } else { 0.05 }),
        edit_bg: (0xFF, 0xFF, 0xFF),
        edit_fg: (0x1A, 0x1A, 0x2E),
    }
}

fn dark_theme(glass: bool) -> Theme {
    let (page, card, header, border) = if glass {
        (
            // 深色玻璃这一档沿用 §10b 调过的 0.86：壁纸只淡淡透出，界面仍是深色。
            color_a(0x10, 0x13, 0x1C, 0.86),
            color_a(0x21, 0x27, 0x36, 0.55),
            color_a(0x28, 0x30, 0x44, 0.70),
            color_a(0x2A, 0x30, 0x42, 0.90),
        )
    } else {
        (
            color(0x10, 0x13, 0x1C),
            color(0x21, 0x27, 0x36),
            color(0x28, 0x30, 0x44),
            color(0x2A, 0x30, 0x42),
        )
    };
    Theme {
        card,
        header,
        border,
        fg: color(0xE8, 0xEA, 0xEE),
        muted: color(0x98, 0xA1, 0xB5),
        accent: color(0x5B, 0x9C, 0xF7),
        accent_soft: color_a(0x22, 0x30, 0x4A, 0.95),
        grid: color(0x2E, 0x35, 0x48),
        white: color(0xFF, 0xFF, 0xFF),
        success: color(0x34, 0xD3, 0x99),
        danger: color(0xF8, 0x71, 0x71),
        modal: color(0x24, 0x2A, 0x3B),
        overlay: color_a(0x05, 0x07, 0x0B, 0.55),
        page,
        // 深色页面上黑影看不见，只会把卡片边缘糊掉 ⇒ 不画阴影，改顶部高光。
        shadow: color_a(0x00, 0x00, 0x00, 0.0),
        highlight: color_a(0xFF, 0xFF, 0xFF, if glass { 0.10 } else { 0.055 }),
        zebra: color_a(0xFF, 0xFF, 0xFF, 0.03),
        row_hover: color_a(0x5B, 0x9C, 0xF7, 0.14),
        hover: color_a(0xFF, 0xFF, 0xFF, if glass { 0.08 } else { 0.07 }),
        edit_bg: (0x21, 0x27, 0x36),
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
    /// 卡片落影（按 [`SHADOW_RINGS`] 从内到外各一把）与浮层用的加强版。
    shadow: Vec<ID2D1SolidColorBrush>,
    shadow_strong: Vec<ID2D1SolidColorBrush>,
    highlight: ID2D1SolidColorBrush,
    zebra: ID2D1SolidColorBrush,
    row_hover: ID2D1SolidColorBrush,
    hover: ID2D1SolidColorBrush,
}

impl Brushes {
    fn new(rt: &ID2D1RenderTarget, t: &Theme) -> windows::core::Result<Self> {
        let mk = |c: D2D1_COLOR_F| unsafe { rt.CreateSolidColorBrush(&c, None) };
        let rings = |k: f32| -> windows::core::Result<Vec<ID2D1SolidColorBrush>> {
            SHADOW_RINGS
                .iter()
                .map(|(_, a)| {
                    mk(D2D1_COLOR_F {
                        a: (t.shadow.a * a * k).min(1.0),
                        ..t.shadow
                    })
                })
                .collect()
        };
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
            shadow: rings(1.0)?,
            shadow_strong: rings(1.8)?,
            highlight: mk(t.highlight)?,
            zebra: mk(t.zebra)?,
            row_hover: mk(t.row_hover)?,
            hover: mk(t.hover)?,
        })
    }
}

struct Formats {
    h1: IDWriteTextFormat,
    value: IDWriteTextFormat,
    /// 次要信息的常规档（说明行、表头、脚注、图例、坐标刻度）。三档字重的分工见
    /// [`UI_WEIGHT_REG`] 那段：这一档是"第二眼才读"的字，与正文拉开的就是粗细本身。
    caption: IDWriteTextFormat,
    /// 同上，右对齐（坐标刻度 / 占比列 / 版本号）。
    caption_right: IDWriteTextFormat,
    /// 同上，居中（星期标签）。
    caption_center: IDWriteTextFormat,
    /// 设置页左侧未选中的分类：与选中项同字号，只差字重与颜色。
    nav_item: IDWriteTextFormat,
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
    /// 表格里名字那一列（键名 / 应用名 / 设备名 / 分组名）。比正文重一档：他 2026-10-30
    /// 点的"窗口界面字体都加粗，像键鼠排行的键鼠名、应用排行的应用名"。
    /// ⚠ 雅黑只有 400/700 两档，SEMI_BOLD(600) 在汉字上会落到 700 —— 这不是 bug，
    /// 想要"稍微粗一点"只能靠字号或颜色，见 `row_name` 那段注释。
    table_name: IDWriteTextFormat,
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
            // `topmost = false`：直接合成的内容要压在**子窗口之下**。行内编辑用的是原生
            // EDIT 子窗口，`true` 时这层 D2D 表面永远盖在它们上面 —— 字打进去了、
            // 光标也在，屏上却一个字都看不见，点「查询」提交后才由插件视图画出来。
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
    /// 出自内置那一处目录 = 画在「内置模块」那一栏。身份读的是 core 交回的
    /// `DiscoveredPlugin.source`（位置说了算，不看脚本自己写的常量）。
    builtin: bool,
    /// 常驻标签（内置由位置强制，用户装的认自己声明的 `PLUGIN_LIFECYCLE`）。
    resident: bool,
    /// 声明那一格的原料：`PLUGIN_API` / `PLUGIN_PERMISSIONS` / `PLUGIN_REFRESH_MS` /
    /// `PLUGIN_MIN_APP`，以及这一份 `state.json` 的占用与最近一次写不进去的原因。
    api: u32,
    permissions: usize,
    /// 声明的那几项权限的名字（要点授权的插件必须让用户**看得见要点什么**，
    /// 只报"权限 2 项"等于让人闭着眼睛点同意）。
    permission_names: Vec<String>,
    /// 授权那一头的结论，core 算好的那一份（页面不许自己再判一次）。
    grant_note: String,
    /// 需不需要用户点一颗「授权」：只有「没授权过」与「权限变了」这两档给按钮。
    needs_grant: bool,
    /// 这一格的面板被慢熔断降频了（方案 3.9 那一列「该插件较慢」）。
    ///
    /// 读的是 core 那份账本，页面不自己数耗时：两处各数一遍就会出现"页面上没标、
    /// 而它其实已经每秒只刷一次"这种说不清的落差。
    slow: bool,
    /// 这一格**为什么没活着**（`panel` 那一档在等面板、常驻额度排到了队尾）。
    note: Option<String>,
    refresh_ms: u64,
    min_app: String,
    store_bytes: Option<usize>,
    store_error: Option<String>,
    /// 这一格的耗时与历史那一句话（core 的台账算好的那一份）。
    ///
    /// `None` = 什么都没跑过，页面上**不画这一格**；画一句"还没有记录"进每一行，
    /// 就是把"没轮到它跑"与"跑过且没问题"折成同一句的那种噪声。
    health: Option<String>,
}

impl PluginRow {
    /// 第二格右边那一串「它自己声明了什么」。
    ///
    /// 只列**与缺省不同**的那些（`API 1`、`面板`、`0ms`、空权限都不出声），
    /// 于是随包那五个一字未改的脚本这一格是空的 —— 屏上不会多出五排没人看得懂的小字。
    /// 每一项都读 core 交回的那份清单，不是在这里再算一遍。
    fn declaration(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.resident {
            parts.push("常驻".to_string());
        }
        if self.api != 1 {
            parts.push(format!("API {}", self.api));
        }
        if self.permissions > 0 {
            parts.push(format!("权限 {} 项", self.permissions));
        }
        if self.refresh_ms > 0 {
            parts.push(format!("{}ms 自刷", self.refresh_ms));
        }
        if !self.min_app.is_empty() {
            parts.push(format!("需 {}", self.min_app));
        }
        if let Some(n) = self.store_bytes {
            if n > 0 {
                // 1 KB 以下就说字节，别显示"0 KB"（那是"没有"的读法）
                parts.push(if n < 1024 {
                    format!("占用 {n} B")
                } else {
                    format!("占用 {} KB", n.div_ceil(1024))
                });
            }
        }
        // 慢熔断那一档要说出来：作者看"为什么我的面板变慢了"，用户看"这一格被降频过"
        if self.slow {
            parts.push("该插件较慢".to_string());
        }
        // 台账那一句话排在最后：前面那几格说的是"这一项是什么配置"，而这一句说的是
        // "它跑起来怎么样、上一次为什么没跑成"。没可说的（`None`）就不画，不占位。
        if let Some(h) = &self.health {
            parts.push(h.clone());
        }
        parts.join(" · ")
    }

    /// 第二格那句话，外加"这是不是一句坏消息"（决定画danger还是muted）。
    ///
    /// 四种结局各有各的话，顺序不能反：加载失败 > 存储写不进去 > 启用了却没加载上 > 简介。
    /// `store_error` 这一句从前只在日志里，屏上一片安静 —— 用户以为插件存成功了，
    /// 重启才发现那一份没落盘（去抖那一头失败时唯一的通知就是这一格）。
    fn second_line(&self) -> (String, bool) {
        if let Some(e) = &self.error {
            (format!("加载失败：{}", one_line(e, 72)), true)
        } else if let Some(w) = &self.store_error {
            (format!("存储写不进去：{}", one_line(w, 72)), true)
        } else if self.needs_grant {
            // 授权这一格排在"已启用尚未加载"之前：那是用户下一步能做的事，
            // 而它必须带着**要点同意的到底是哪几项**（方案 3.7 那句"页面提示需要授权"，
            // 只说「需要授权」等于让人闭着眼睛点）。
            let names = if self.permission_names.is_empty() {
                self.grant_note.clone()
            } else {
                format!("{}：{}", self.grant_note, self.permission_names.join("、"))
            };
            (format!("{}（{names}）", self.desc), true)
        } else if let Some(n) = &self.note {
            // 「故意没加载」排在泛泛那句"已启用，尚未加载"**之前**：note 是更精确的那一句
            // （`panel` 那一档在等面板、常驻额度排到了队尾），两句同时出现等于页面上自相矛盾。
            (format!("{}（{n}）", self.desc), false)
        } else if self.enabled && !self.loaded {
            (format!("{}（已启用，尚未加载）", self.desc), false)
        } else {
            (self.desc.clone(), false)
        }
    }
}

/// 卸载确认框要记住的东西：`name` 给人看，`stem` 用来真删。
#[derive(Clone, Debug, PartialEq, Eq)]
struct UninstallPrompt {
    stem: String,
    name: String,
}

/// 插件页那一列从上到下的一格。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PluginSlot {
    /// 分栏表头（`扩展插件` 那一栏在没有条目时不画表头，见 [`plugin_slots`]）
    Header(&'static str),
    /// 第 N 行（下标指进 `&[PluginRow]`，命中与开关都按它算）
    Row(usize),
}

/// 把两份清单排成"内置模块 / 扩展插件"两栏的一列格子。
///
/// 为什么是一张**格子表**而不是画的时候各画两个循环：这一页有三个读者（画、鼠标命中、
/// 滚轮上限），三处都要知道"第 vis 格上有没有表头"。表头占一格行高，
/// 一屏能装的行数因此比原来少两格 —— 三处各算各的话，症状就是"点开关点到隔壁那一行"。
/// 分组只按 `builtin` 那一位切（= core 的 `source`），不看名字里有没有"内置"。
fn plugin_slots(rows: &[PluginRow]) -> Vec<PluginSlot> {
    let mut out: Vec<PluginSlot> = Vec::with_capacity(rows.len() + 2);
    for (group, title) in [(true, "内置模块"), (false, "扩展插件")] {
        let ids: Vec<usize> = (0..rows.len())
            .filter(|i| rows[*i].builtin == group)
            .collect();
        if ids.is_empty() {
            // 空栏不画表头：只有一栏有东西时，屏上不会出现一句孤零零的「扩展插件 (0)」
            continue;
        }
        out.push(PluginSlot::Header(title));
        for i in ids {
            out.push(PluginSlot::Row(i));
        }
    }
    out
}

/// 热重载这一轮换上来几格之后，这一拍还要做什么：`(要不要重扫这一页, 那句公告)`。
///
/// 两个症状各管一头，所以两头都得有断言：
/// - 不在这一页却每 2 秒全量重扫 ⇒ 白读两遍目录、再为每一格问一次存储占用；
/// - 就在这页却不重扫 ⇒ 屏上停在改之前那一行，而作者看到的症状恰恰是「我改了没生效」。
///
/// 公告那头管的是"有没有人看见"：换了名字就要留一行（这是唯一能事后对账的痕迹），
/// 而一次也没换过时不许留 —— 每拍写一行日志等于把日志刷成一秒十条的空话。
fn reload_followup(on_plugin_page: bool, changed: &[String]) -> (bool, Option<String>) {
    if changed.is_empty() {
        return (false, None);
    }
    let joined = changed.join("、");
    let note = if on_plugin_page {
        format!("插件热重载：{joined}（这一页跟着重扫了清单）")
    } else {
        format!("插件热重载：{joined}（不在这一页，清单留给下次进来重扫）")
    };
    // ASCII 尾巴是给验收脚本看的：这一句的主体是中文，脚本里嵌中文匹配会踩编码坑
    // （本仓那条「含 · 的整串在 shell 里比不中」的同族），数得上的部分留成纯 ASCII。
    (
        on_plugin_page,
        Some(format!("{note} hotreload={}", changed.len())),
    )
}

/// 行内编辑这一格是在改什么：设备改名、设置页的值，还是插件视图里的输入框。
#[derive(Clone, Debug)]
enum EditTarget {
    Device(String),
    Setting(usize),
    /// 插件视图里的 TextInput：(插件展示名, 字段名)
    PluginField(String, String),
    /// 剪贴板片段改名。
    SnippetName(i64),
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

/// 指针此刻压在哪个"稳定控件"上（手型光标与悬停底色共用）。
///
/// 只覆盖画图与命中测试读同一份几何的那几组：周期药丸、视图页签、分析子页签、
/// 排行页药丸、设置导航与行、插件清单行、可点的表格行。行内更细的按钮
/// （改名/恢复/授权/详情）各有自己的矩形，不复刻到这里 —— 它们保持箭头，
/// 因为"手型指着一个点不动的区域"比"少一处手型"更伤。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctl {
    WinBtn(u8),
    Period(u8),
    ViewTab(u8),
    /// 看板页那三个范围胶囊（今日/本周/本月）。手型指针要认它：胶囊是**点得动**的，
    /// 指过去还是箭头就是"这里好像不能按"。
    DashPill(u8),
    ATab(u8),
    RankPill(u8),
    SetNav(u8),
    /// 设置页里的一行（分组标题不算控件）。
    SetRow(u16),
    /// 排行榜/应用/设备里可点的那一行 —— 只有设备屏有下游动作。
    TableRow(u16),
    /// 插件清单里的一行。
    PluginList(u16),
    /// 插件页里能点的格子（返回、安装、视图控件、详情页按钮）。
    Pv,
    /// 剪贴板管理页：三个模式页签、工具钮、行、搜索框。
    ClipTab(u8),
    ClipRow(u16),
    ClipBtn(u8),
    ClipSearch,
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
    /// 剪贴板历史弹窗热键（`[hotkey] clipboard`）。剪贴板功能关掉时不注册。
    clipboard_hk: Option<HotKey>,
    /// 剪贴板监听线程交件的收件口与句柄；None = 库打不开或线程没起来。
    clip_rx: Option<std::sync::mpsc::Receiver<focusflow_core::clipboard::Captured>>,
    /// 上一次投给插件的剪贴板事件（内容哈希 + 时刻）。
    ///
    /// 系统剪贴板服务/历史面板会把同一段文本重复发布成好几次变化，监听线程因此会
    /// 连续交来好几件同一内容的 `Captured`（库里靠 dedupe 只动一行）。插件事件要是照单
    /// 全投，一次复制会变成两三次回调 —— 实测一拍里同一个 marker 收到 3 次。
    /// 这里按内容哈希压一个 1 秒窗：同内容不重复投，过后再复制同一段仍然算新事件。
    /// 只存哈希，不存明文。
    clip_event_last: Option<(String, std::time::Instant)>,
    /// 只为持有监听线程：`Drop` 时 `PostMessage(WM_CLOSE)` 并 join，进程退出前把隐藏窗口收掉。
    #[allow(dead_code)]
    clip_watcher: Option<focusflow_core::clipboard::Watcher>,
    /// 系统材质贴上了（Win11 Mica）。
    mica: bool,
    /// 现在真的走合成路径（透明/玻璃）。只有 `mica` 且合成成功才为 true。
    glass: bool,
    /// 正在拖拽缩放：这期间页面底色临时变不透明（DWM 背板不跟尺寸，见 `draw`）。
    resizing: bool,
    /// 正在系统的移动/缩放循环里（进入/退出各一条消息）。
    in_size_move: bool,
    /// 进入那个循环时的客户区尺寸：退出时据此判断"这一趟到底改没改大小"。
    size_at_enter: (f32, f32),
    /// 上一次尺寸变化后是不是最大化（用来只在"最大化/还原"那一刻修背板）。
    was_maximized: bool,
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
    /// 搜索框那对「宿主 → EDIT」里的顶层宿主窗。
    search_edit_host: Option<HWND>,
    /// 那个搜索框此刻有没有焦点（由 `edit_proc` 在 WM_SETFOCUS / WM_KILLFOCUS 上递回来）。
    /// 提示文案只在它为 `false` 时由父窗画。
    search_focused: bool,
    /// 剪贴板管理页（第 8 页）的状态：模式、筛选、列表、多选。
    clip_page: clipboard_page::ClipPage,
    /// 剪贴板管理页自己的搜索框（与设置页那个分开：两页的过滤词生命周期不同）。
    clip_search_edit: Option<HWND>,
    clip_search_host: Option<HWND>,
    clip_search_focused: bool,
    /// 后台操作把结果行塞这里，再投 `WM_OP_DONE` 让界面来取。
    ops: Arc<Mutex<Vec<String>>>,
    /// 内置看板页的状态（哪一档 + 上次问库的时刻 + 算好的那一页）。
    /// 数字本身在 `focusflow_core::dashboard`，这一页不写 SQL。
    dash: dashboard::Dash,
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
    /// 正在看哪一格的详情（None = 不在详情页）。
    ///
    /// 存的是 stem 而不是展示名：详情页要拿它去问台账，而展示名是插件自己写的常量，
    /// 两处对不上时（作者改名）会问到"另一格"的账上。与 `plugin_open` 互斥：
    /// 视图页画 VM 交回的控件，详情页画台账，同一时刻只该有一个。
    plugin_detail_open: Option<String>,
    /// 详情页顶上「来源」那一句（打开详情时读一次安装记录；None = 不在详情页）。
    ///
    /// 存在这里而不是画的时候读盘：那一页每 100ms 被重画一次，读盘要在打开那一下。
    plugin_detail_source: Option<String>,
    /// 这一格是不是通过安装进来的包（有安装记录才有包文件可卸）。
    plugin_detail_installed: bool,
    /// 卸载确认框（None = 没开）。
    plugin_uninstall: Option<UninstallPrompt>,
    /// 「清理 30 天前」的二次确认（不可撤销那一档的唯一入口）。
    cleanup_confirm: bool,
    /// 上一拍开着的那一页（用来认出"关掉/换页"这一个转变）。
    ///
    /// 为什么不在三处关闭动作里各写一遍卸载：那三处迟早会漂移（本仓为"同一个手势的多个出口
    /// 各写一份清理"改过好几次），而漏掉一处的代价是一份 VM 与一格常驻额度永远不回来。
    plugin_panel_was: Option<String>,
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
    /// 插件视图里"能点的格子"（悬停与手型共用）。几何与选中集无关，缓存键只要
    /// 视图名 + 尺寸 + 滚动位置 —— 选中只影响画出来的底色。
    pv_targets: Vec<PvTarget>,
    pv_targets_key: Option<(String, u32, u32, i32)>,
    /// 指针此刻压在插件视图的哪一格（行 / 控件）。
    pv_hover: Option<PvTarget>,
    /// 正在拖的滚动条滑块（None = 没在拖）。
    scroll_drag: Option<ScrollDrag>,
    /// 键盘焦点：当前页"可停住"那一列里的下标（None = 没有焦点）。
    kbd_focus: Option<usize>,
    /// 指针位置（DIP）与同一个悬停目标已经停了多久：tooltip 要等一会儿再出来。
    ptr: (f32, f32),
    hover_since: std::time::Instant,
    tooltip_ready: bool,
    /// 这一帧指针压着的、被截断的那一格全文（右键复制读它）。
    tooltip_text: Option<String>,
    hover: Option<usize>,
    /// 指针下的控件（药丸/页签/导航）：只影响底色与光标形状，不参与点击判定。
    ctl: Option<Ctl>,
    sel: Option<usize>,
    /// 行内编辑用的原生 EDIT：它挂在**顶层弹出宿主窗**下面，不是主窗的子窗口。
    /// 主窗走 DirectComposition 合成，挂在它下面的子窗口会被那层合成内容盖住
    /// （实测连原生按钮都只透出一点、EDIT 完全看不见：字打进去了屏上却一个字没有）。
    edit: Option<HWND>,
    /// EDIT 的顶层宿主窗（画主题底与描边，位置用屏幕坐标）。
    edit_host: Option<HWND>,
    /// 日期选择器（顶层弹出窗，懒创建）。
    date_pick: Option<HWND>,
    edit_target: Option<EditTarget>,
    edit_row: Option<usize>,
    /// 按下录入：正在录哪一条热键（`settings` 数组下标）。开着的时候主窗口必须握着焦点，
    /// 否则下一键会跑到别的应用那里去，而屏上还写着"请按下组合"。
    capturing: Option<usize>,
    ui_font: HFONT,
    edit_bg: HBRUSH,
    /// 行内 EDIT 宿主窗那圈描边（主色调）。
    edit_border: HBRUSH,
    /// EDIT 的文字色与底色（`WM_CTLCOLOREDIT` 用）。跟着主题走，与 `edit_bg` 同一份。
    edit_fg: COLORREF,
    edit_bg_color: COLORREF,
    /// 托盘里的「暂停采集」勾选项：状态变化时要把勾一起改掉。
    pause_item: Option<CheckMenuItem>,
    /// 托盘里"记录剪贴板"那一项的勾选态（与 `[clipboard] enabled` 同源）。
    clipboard_item: Option<CheckMenuItem>,
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
    file_log_failure: Option<String>,
) -> Option<String> {
    // 排在这条链上的是"这一刻屏上只能念一句"的先后：数据类的成败第一，热键其次，
    // **文件日志层没建成放最后** —— 它不改变程序行为（照常记录、照常退出），改变的是
    // "去哪里查原因"：`logs/` 是本仓每个故障的默认下一步，空目录会让人以为什么都没发生。
    // 所以它该说，但不该挤掉更要紧的那句。
    move_failure
        .or(config_corrupt)
        .or(data_home_fallback)
        .or(legacy_double_count)
        .or(moved_ok)
        .or(hotkey_warning)
        .or(file_log_failure)
}

/// 收进托盘：藏窗口，**并且**放掉渲染目标。
///
/// 为什么这一步要跟着隐藏走（2026-10-31 分臂现量）：面板被画过一次之后再收进托盘，
/// 静置 60 秒仍是 `WS 49.8 / 私有 57.7 / 57 线程`，与"正显示着"那一读数一字不差；
/// 而"面板从未画过"那一臂是 `39.8 / 41.5 / 21~23`。也就是说两张整窗 BGRA 背板、
/// 二十来支 D2D 画刷和三十几条图形栈线程会全天陪着进程，只因为用户开过一次面板 ——
/// 常驻私有提交因此多 **16.2 MB**，正是"开过一次就回不到起点"在原生版里剩下的那一份。
///
/// 显示那一边不用补什么：`ensure_target` 本来就按 `target.is_none()` 重建（与
/// `WM_DPICHANGED` 走同一条路），代价是唤回面板时重建一次渲染目标，那一下的毫秒数
/// 由屏证与读数说话，不在这里猜。
fn hide_to_tray(app: &mut App, hwnd: HWND) {
    // 弹窗没有 owner，主窗藏到托盘时它不会自己跟着藏，必须显式收掉。
    clipboard_popup::hide();
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
    app.drop_target();
}

/// 开机之后「全局热键」那一格显示开还是关：跟着**真正注册上的组合**走，不跟着配置键走。
///
/// 两个理由：① 配置说开而三条一条都没注册上（全被别的程序占了），屏上写"开"就是假话；
/// ② 这一格以前只看了唤出与截图两条，**漏了标注那条** —— 只注册上标注键时它显示"关"，
/// 而那颗键确实在占用全局快捷键。清单与 `COMBO_KEYS` 同一枚，不再手写第二遍。
fn hotkeys_on_after_boot(registered: [&Option<HotKey>; 4]) -> bool {
    registered.iter().any(|hk| hk.is_some())
}

/// 谁在看这些数：面板（可见且没最小化）、只有卡片，或者没人。
///
/// 托盘 tooltip 上那句"今日 N 次"不算读者——它只在鼠标悬停那一刻被读到，由
/// `TrayIconEvent::Enter` 现问采集侧那个原子计数，不需要轮询养着。
///
/// 为什么把"有人看"再分成两档：现量（`crates/core/tests/perf_probe.rs` 的 `pill_arms`）
/// 说卡片只要两个数，而它们里外都不查库或只查一问 —— 「活跃」当场问
/// `collect::today_total()`、「速度」问 `collect::cpm()`，只有「时长」需要
/// `get_app_stats(Some(1))`（0.4 ms／2 条）。而 `data::load()` 那一拍是为面板画的：
/// 两份排行、三份时间切面、设备榜、应用榜、打卡条、29 行设置清单
/// （今日档 6.3 ms／10 条、总计档 49.5 ms／17 条，唯一一份数字写在下面
/// `LONG_PERIOD_TICKS` 那一段，这里不抄第二份）。用户的常驻态恰好就是"只有卡片"，
/// 所以旧的那两档形状让整批面板查询全天在跑。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Readers {
    None,
    Card,
    Panel,
}

fn watching(main_visible: bool, main_iconic: bool, pill_visible: bool) -> Readers {
    if main_visible && !main_iconic {
        Readers::Panel
    } else if pill_visible {
        Readers::Card
    } else {
        Readers::None
    }
}

/// 没人看时用来等"有人回来"的那一小片：250ms 是"点开窗口到第一帧新数"的最坏等待，
/// 空转时的代价只是一次 `IsWindowVisible`。
const IDLE_WAKE_MS: u64 = 250;

/// 现问一次"谁在看"。判据在 `watching`（那条有单元测试），这里只取三个读数。
fn watching_now(hwnd: HWND) -> Readers {
    unsafe {
        watching(
            IsWindowVisible(hwnd).as_bool(),
            IsIconic(hwnd).as_bool(),
            floating::is_visible(),
        )
    }
}

/// 催一拍的那一位：谁改了"该看的东西"（把窗口唤到眼前、按一下键）就把它按下去，
/// 正在睡的快照线程因此提前醒一次，而不是把整段节拍熬完。
///
/// 为什么只留一位而不是当场查库：按的人在监听线程上，而一拍是 10 条语句／7.6ms
/// （数出自 `crates/core/tests/perf_probe.rs`）——每键都查会把 1 拍/秒抬成打字高峰的
/// 10 拍/秒（约 650ms/秒 CPU）。查这件事留在快照线程，催的人只负责把位按下去。
static SNAP_POKE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 睡这一拍时多久看一眼"有没有人催"：50ms 是唤窗到第一帧新数的最坏等待。
/// 没人催的时候这些片一次都不会真醒，空转代价仍只是每 250ms 一次 `IsWindowVisible`。
const POKE_SLICE_MS: u64 = 50;

/// 被催醒之后离上一次真拍至少要隔这么多毫秒才许再拍一次（节流）。
/// 10 键/秒因此合成 4 拍/秒，而不是每键一拍。
const POKE_MIN_GAP_MS: u64 = 250;

pub(crate) fn poke_snapshot() {
    SNAP_POKE.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// 还要补睡多少毫秒才许再拍一次：0 = 立刻拍。
fn poke_backlog_ms(since_query_ms: u64, min_gap_ms: u64) -> u64 {
    min_gap_ms.saturating_sub(since_query_ms)
}

/// 睡这么多毫秒，期间只要有人催过就立刻醒并把那位清掉。回话是"有没有被催"。
fn sleep_or_poke(ms: u64) -> bool {
    let mut left = ms;
    while left > 0 {
        let slice = POKE_SLICE_MS.min(left);
        std::thread::sleep(Duration::from_millis(slice));
        left -= slice;
        if SNAP_POKE.swap(false, std::sync::atomic::Ordering::Relaxed) {
            return true;
        }
    }
    false
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
            let fmt = build_formats(&dwrite)?;

            let (tray, pause_item, clipboard_item) = make_tray(&snap);
            let HotkeysAtBoot {
                manager: hotkeys,
                toggle: hotkey_hk,
                snip: snip_hk,
                annotate: snip_annotate_hk,
                clipboard: clipboard_hk,
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
                // 文件日志那一层没建成 ⇒ 屏上必须说出来：`logs/` 是本仓每个故障的默认
                // 下一步，而这一层一旦没装上，本进程一辈子都不会往里写（core 里那一格
                // 存的就是这件事，之前全仓零读点 —— 注释写着"由启动报告带出来"，那个
                // 读点随旧前端一起删了，只剩 `eprintln!`，而桌面端没有控制台）。
                focusflow_core::logger::file_log_failure(),
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
                // 与设置页正文同一档（`SETTINGS_TEXT_PT`）：框里敲的字要和框外念的字一样大。
                -((SETTINGS_TEXT_PT * dpi / 96.0).round() as i32),
                0,
                0,
                0,
                // 与 D2D 那一套同一个基准字重（`UI_WEIGHT`）：框里敲进去的字不该比框外的细一档。
                FW_SEMIBOLD.0 as i32,
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
                    // 上一次真拍是什么时候：被催醒的那一拍按 `POKE_MIN_GAP_MS` 节流就靠它。
                    let mut last_query = std::time::Instant::now();
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
                        let mut r = watching_now(hwnd);
                        if r == Readers::None {
                            // 没人看 ⇒ 整批聚合查询一拍都不跑，只按小片睡等有人回来。
                            if !idle {
                                // 只在**跨过边界**那一次出声：这条既是给他看的"程序没在空转"，
                                // 也是验收腿唯一的观察量（每拍都写就成了刷屏）。
                                tracing_note(
                                    "快照线程 idle=on：没有读者（窗口隐藏/最小化且卡片未显示），暂停查询",
                                );
                            }
                            idle = true;
                            // 小片地睡：唤窗那一下会催这一位，于是"点开窗口到第一帧新数"
                            // 从最坏 250ms 压到最坏一片 50ms；没人催还是每 250ms 问一次。
                            sleep_or_poke(IDLE_WAKE_MS);
                            continue;
                        }
                        if idle {
                            // 刚有人看：当场查一次。让他先看到上一次留下的旧数，正是这次
                            // 要修的那个症状换了个方向回来。
                            tracing_note("快照线程 idle=off：有读者了，当场查一次并恢复按拍刷新");
                            idle = false;
                        } else {
                            // 睡满这一拍；期间有人按键或唤窗就提前醒。被催醒**不等于**立刻再拍：
                            // 离上一次真拍不足 `POKE_MIN_GAP_MS` 就只补睡剩下的那一段，
                            // 于是打字高峰合成 4 拍/秒，而不是每键 10 条语句。
                            if sleep_or_poke(tick_ms) {
                                let left = poke_backlog_ms(
                                    last_query.elapsed().as_millis() as u64,
                                    POKE_MIN_GAP_MS,
                                );
                                if left > 0 {
                                    std::thread::sleep(Duration::from_millis(left));
                                }
                            }
                            r = watching_now(hwnd);
                            if r == Readers::None {
                                // 睡这一拍期间窗口又被藏起来了：整批查询省掉。
                                continue;
                            }
                        }
                        // 只有卡片一个读者 ⇒ 只问它那一问（现量 0.4 ms／2 条，而面板那一拍
                        // 今日 6.3 ms／10 条、总计 49.5 ms／17 条）。共享快照里只就地补写
                        // 卡片之外还有人读的那两格（托盘 tooltip 的退路、验收状态行、
                        // 启动那次 make_tray），其余一律不动 —— 那是面板现身那一拍的账，
                        // 而唤窗那一下会 `poke_snapshot()`，所以下一拍当场就是整批。
                        if r == Readers::Card {
                            let thin = data::load_card_only();
                            last_query = std::time::Instant::now();
                            publish_floating(&thin);
                            floating::refresh();
                            if let Ok(mut g) = s.lock() {
                                g.today = thin.today;
                                g.today_app_seconds = thin.today_app_seconds;
                            }
                            continue;
                        }
                        let next = data::load(p.load(std::sync::atomic::Ordering::Relaxed));
                        last_query = std::time::Instant::now();
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

            let (clip_rx, clip_watcher) = match focusflow_core::clipboard::init_db() {
                Ok(()) => match focusflow_core::clipboard::start() {
                    Ok((watcher, rx)) => (Some(rx), Some(watcher)),
                    Err(e) => {
                        tracing_note(&format!("剪贴板监听没起来：{e}"));
                        (None, None)
                    }
                },
                Err(e) => {
                    tracing_note(&format!("剪贴板库打不开：{e}"));
                    // 库打不开 ⇒ 监听也不会起：屏上要说出来，不能只在日志里。
                    focusflow_core::clipboard::report_watcher_fault(format!("剪贴板库打不开：{e}"));
                    (None, None)
                }
            };

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
                hotkeys_on: hotkeys_on_after_boot([
                    &hotkey_hk,
                    &snip_hk,
                    &snip_annotate_hk,
                    &clipboard_hk,
                ]),
                hotkey_hk,
                snip_hk,
                snip_annotate_hk,
                clipboard_hk,
                clip_rx,
                clip_watcher,
                clip_event_last: None,
                mica,
                glass: false,
                resizing: false,
                in_size_move: false,
                size_at_enter: (0.0, 0.0),
                was_maximized: false,
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
                search_edit_host: None,
                search_focused: false,
                clip_page: clipboard_page::ClipPage::new(),
                clip_search_edit: None,
                clip_search_host: None,
                clip_search_focused: false,
                ops: Arc::new(Mutex::new(Vec::new())),
                pm: None,
                plugin_rows: Vec::new(),
                plugin_err: None,
                plugins_loaded: false,
                dash: dashboard::Dash::new(),
                plugin_open: None,
                plugin_detail_open: None,
                plugin_detail_source: None,
                plugin_detail_installed: false,
                plugin_uninstall: None,
                cleanup_confirm: false,
                plugin_panel_was: None,
                pv_scroll: 0,
                pv_select: None,
                pv_modal: None,
                plugin_sel: HashMap::new(),
                pv_targets: Vec::new(),
                pv_targets_key: None,
                pv_hover: None,
                scroll_drag: None,
                kbd_focus: None,
                ptr: (0.0, 0.0),
                hover_since: std::time::Instant::now(),
                tooltip_ready: false,
                tooltip_text: None,
                hover: None,
                ctl: None,
                sel: None,
                edit: None,
                edit_host: None,
                date_pick: None,
                edit_target: None,
                edit_row: None,
                capturing: None,
                ui_font,
                edit_bg,
                edit_border: HBRUSH::default(),
                edit_fg,
                edit_bg_color,
                pause_item,
                clipboard_item,
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
                if !self.edit_border.is_invalid() {
                    unsafe {
                        let _ = DeleteObject(HGDIOBJ(self.edit_border.0));
                    }
                }
                self.edit_bg_color = COLORREF(rgb_ref(bg_r, bg_g, bg_b));
                self.edit_fg = COLORREF(rgb_ref(fg_r, fg_g, fg_b));
                self.edit_bg = unsafe { CreateSolidBrush(self.edit_bg_color) };
                let a = pal.accent;
                self.edit_border = unsafe {
                    CreateSolidBrush(COLORREF(rgb_ref(
                        (a.r * 255.0).round() as u8,
                        (a.g * 255.0).round() as u8,
                        (a.b * 255.0).round() as u8,
                    )))
                };
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
        // 拖拽缩放时临时把它变不透明：DWM 的背板不跟尺寸走，半透明页会露出旧背板的
        // 边界（屏上像"页面只铺到旧窗口右边"）。卡片本来就画在页面之上，所以只改这一处，
        // 主题、字色、卡片透明度都不动。
        let mut clear = pal.page;
        if self.resizing {
            clear.a = 1.0;
        }
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
                dw: &self.dwrite,
            };
            let kbd_rect = self.kbd_focus.and_then(|i| {
                self.kbd_items(size.width, size.height)
                    .get(i)
                    .map(|it| it.rect)
            });
            // 弹层把整页压住时不出 tooltip：否则会浮在对话框上面。
            let tooltip_on = self.tooltip_ready
                && self.detail.is_none()
                && !self.cleanup_confirm
                && self.plugin_uninstall.is_none()
                && self.pv_modal.is_none()
                && self.pv_select.is_none();
            let vs = ViewState {
                view: self.view,
                atab: self.atab,
                rank_tab: self.rank_tab,
                rank_filter: self.rank_filter,
                scroll: self.scroll,
                period: self.period.load(std::sync::atomic::Ordering::Relaxed),
                hover: self.hover,
                ctl: self.ctl,
                pv_hover: self.pv_hover,
                kbd_focus: kbd_rect,
                ptr: self.ptr,
                tooltip: tooltip_on,
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
                    self.clipboard_hk.is_some(),
                ],
                hotkey_on: self.hotkey_hk.is_some()
                    || self.snip_hk.is_some()
                    || self.snip_annotate_hk.is_some()
                    || self.clipboard_hk.is_some(),
                floating_on: floating::is_visible(),
                pin_count: PINS.lock().map(|v| v.len()).unwrap_or(0),
                dark: tokens.dark,
                zoomed: unsafe { IsZoomed(hwnd) }.as_bool(),
                search_focused: self.search_focused,
                clip_search_focused: self.clip_search_focused,
                collector_on: collect::is_listening(),
                collector_paused: collect::is_paused(),
                cpm: collect::cpm(),
                settings_cat: self.settings_cat,
                dash_range: self.dash.range,
            };
            let pv = self.plugin_open.as_ref().and_then(|name| {
                self.pm
                    .as_ref()
                    .and_then(|pm| pm.get_plugin(name))
                    .and_then(|p| p.view.as_ref().map(|v| (name.as_str(), v)))
            });
            // 刷新之后内容可能变短，旧偏移会把整页推到可见区上面：画出来是一张空卡片，
            // 要再滚一下滚轮才回来。每帧按当前内容夹一次，命中那一侧读的也是这个值。
            let pv_scroll = match pv {
                Some((_, view)) => clamp_plugin_scroll(
                    self.pv_scroll,
                    plugin_content_height(view, plugin_scroll_area(size.width, size.height)),
                    plugin_scroll_visible(size.height),
                ),
                None => self.pv_scroll,
            };
            // 画的时候顺手记下"指针压着哪个被截断的格子"：提示框与右键复制同源。
            let mut tip: Option<(String, D2D_RECT_F)> = None;
            draw_screen(
                &p,
                size.width,
                size.height,
                &snap,
                &self.dash.board,
                &self.clip_page,
                vs,
                self.detail.as_ref(),
                self.status.as_deref(),
                self.busy,
                &self.search,
                &self.plugin_rows,
                self.plugin_err.as_deref(),
                self.plugin_detail_open.as_deref(),
                self.plugin_detail_source.as_deref(),
                self.plugin_detail_installed,
                self.plugin_uninstall.as_ref(),
                self.cleanup_confirm,
                pv,
                pv_scroll,
                self.pv_select.as_ref(),
                self.pv_modal.as_deref(),
                &self.plugin_sel,
                &mut tip,
            );
            self.tooltip_text = tip.map(|(text, _)| text);
            self.pv_scroll = pv_scroll;
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

    /// 指针落在哪个可点区域上。顺序与 `on_click` 一致（药丸 → 页签 → 页内控件）。
    ///
    /// 插件视图开着时周期药丸与页签仍然点得动（`on_click` 本来就先处理它们），
    /// 所以这里不再因为"视图开着"整段早退 —— 那会让一排点得动的药丸全变回箭头。
    fn control_at(&mut self, w: f32, h: f32, x: f32, y: f32) -> Option<Ctl> {
        // 窗口按钮在插件视图与详情弹窗里也要亮：它不归页面，归窗口。
        match top_owner(self.detail.is_some(), w, x, y) {
            TopOwner::WinBtn(i) => return Some(Ctl::WinBtn(i)),
            TopOwner::DetailModal => return None,
            TopOwner::Page => {}
        }
        for (i, (_, r)) in period_pills().iter().enumerate() {
            if hit(*r, x, y) {
                return Some(Ctl::Period(i as u8));
            }
        }
        for (i, r) in view_tabs().iter().enumerate() {
            if hit(*r, x, y) {
                return Some(Ctl::ViewTab(i as u8));
            }
        }
        // 看板页的范围胶囊与页签同一行，手型指针要一视同仁（那三个是真点得动的）。
        if self.view == 6 {
            for (i, (_, pill)) in dashboard::range_pills().iter().enumerate() {
                if hit(*pill, x, y) {
                    return Some(Ctl::DashPill(i as u8));
                }
            }
        }
        match self.view {
            0..=2 => {
                if self.view == 0 {
                    let pills = rank_pills(left_card(w, h));
                    for (i, r) in pills.iter().enumerate() {
                        // 「分组」那一屏不画筛选三格（`draw_table` 同样只画前两颗）。
                        if self.rank_tab == 1 && i >= 2 {
                            break;
                        }
                        if hit(*r, x, y) {
                            return Some(Ctl::RankPill(i as u8));
                        }
                    }
                }
                let card = left_card(w, h);
                let g = table_geom(card);
                let count = self
                    .snap
                    .lock()
                    .map(|snap| left_rows(&snap, self.view, self.rank_tab, self.rank_filter).len())
                    .unwrap_or(0);
                if let Some((i, _)) = table_row_at(count, &g, card, self.view, self.scroll, x, y) {
                    return Some(Ctl::TableRow(i as u16));
                }
            }
            3 => {
                for (i, r) in atab_tabs().iter().enumerate() {
                    if hit(*r, x, y) {
                        return Some(Ctl::ATab(i as u8));
                    }
                }
            }
            4 => {
                let card = rect(16.0, 208.0, w - 32.0, h - 208.0 - 16.0);
                let back = rect(card.left + 16.0, card.top + 12.0, 64.0, 24.0);
                if let Some(stem) = self.plugin_detail_open.as_deref() {
                    if matches!(
                        plugin_page_of(None, Some(stem), &self.plugin_rows),
                        PluginPage::Detail(_)
                    ) {
                        if self.plugin_detail_installed && hit(detail_uninstall_rect(card), x, y) {
                            return Some(Ctl::Pv);
                        }
                        if hit(back, x, y) {
                            return Some(Ctl::Pv);
                        }
                        return None;
                    }
                }
                if self.plugin_open.is_some() {
                    if hit(back, x, y) {
                        return Some(Ctl::Pv);
                    }
                    let clip = plugin_content_clip(card);
                    let targets = self.pv_targets_update(w, h)?;
                    return pv_target_on(targets, clip, x, y).map(|_| Ctl::Pv);
                }
                if hit(plugin_install_rect(card), x, y) {
                    return Some(Ctl::Pv);
                }
                let (list_top, row_h, max_rows) = plugin_geom(w, h);
                let slots = plugin_slots(&self.plugin_rows);
                if let Some((i, _)) =
                    plugin_list_row_at(&slots, list_top, row_h, max_rows, w, self.scroll, x, y)
                {
                    return Some(Ctl::PluginList(i as u16));
                }
            }
            5 => {
                let g = settings_geom(w, h);
                for i in 0..SETTINGS_CATS.len() {
                    let nr = rect(
                        g.nav_x,
                        g.nav_y + i as f32 * g.cat_h,
                        g.nav_w,
                        g.cat_h - 4.0,
                    );
                    if hit(nr, x, y) {
                        return Some(Ctl::SetNav(i as u8));
                    }
                }
                let items = self
                    .snap
                    .lock()
                    .ok()
                    .map(|s| settings_items(&s, &self.search, self.settings_cat))
                    .unwrap_or_default();
                if let Some((i, _)) = settings_row_at(&items, &g, w, self.scroll, x, y) {
                    return Some(Ctl::SetRow(i as u16));
                }
            }
            7 => {
                let lay = clipboard_page::layout_for(w, h);
                if self.clip_page.mode != clipboard_page::Mode::Stats {
                    if clipboard_page::search_at(&lay, x, y) {
                        return Some(Ctl::ClipSearch);
                    }
                    if let Some(i) = clipboard_page::toolbar_at(&lay, x, y) {
                        return Some(Ctl::ClipBtn(i as u8));
                    }
                }
                if let Some(i) = clipboard_page::tab_at(&lay, x, y) {
                    return Some(Ctl::ClipTab(i as u8));
                }
                let len = self.clip_page.rows().len();
                if let Some(i) = clipboard_page::row_at(&lay, self.clip_page.scroll, len, x, y) {
                    return Some(Ctl::ClipRow(i as u16));
                }
            }
            _ => {}
        }
        None
    }

    /// 确保插件视图的"能点格子"与当前这一页对得上；键不变就复用。
    ///
    /// 只在视图/尺寸/滚动位置变化时重排一次，`WM_MOUSEMOVE` 与 `WM_SETCURSOR`
    /// 都读这份缓存 —— 否则每移动一个像素就要把整页控件连字符串一起重建一遍。
    fn pv_targets_update(&mut self, w: f32, h: f32) -> Option<&[PvTarget]> {
        let name = self.plugin_open.clone()?;
        let key = (name.clone(), w.to_bits(), h.to_bits(), self.pv_scroll);
        if self.pv_targets_key.as_ref() != Some(&key) {
            let view = self
                .pm
                .as_ref()
                .and_then(|pm| pm.get_plugin(&name))
                .and_then(|p| p.view.clone());
            let Some(view) = view else {
                self.pv_targets.clear();
                self.pv_targets_key = None;
                return None;
            };
            let card = rect(16.0, 208.0, w - 32.0, h - 208.0 - 16.0);
            let area = rect(
                card.left,
                card.top + 40.0,
                card.right - card.left,
                card.bottom - card.top,
            );
            let ops = plugin_view_ops(&view, area, self.pv_scroll as f32, &self.plugin_sel);
            self.pv_targets = pv_targets_of(&ops);
            self.pv_targets_key = Some(key);
        }
        Some(&self.pv_targets)
    }

    /// 当前这一页的滚动条几何：轨道 + 总数 + 一屏容量 + 当前偏移。
    ///
    /// 画、拖动、命中三处读同一份；`None` = 这一页没有可滚的东西。
    fn scroll_geom(&self, w: f32, h: f32) -> Option<(ScrollLane, D2D_RECT_F, usize, usize, usize)> {
        match self.view {
            0..=2 => {
                let card = left_card(w, h);
                let g = table_geom(card);
                let total = self
                    .snap
                    .lock()
                    .map(|s| left_rows(&s, self.view, self.rank_tab, self.rank_filter).len())
                    .unwrap_or(0);
                let track = rect(
                    card.right - 11.0,
                    g.list_top,
                    6.0,
                    g.max_rows as f32 * g.row_h,
                );
                Some((ScrollLane::Table, track, total, g.max_rows, self.scroll))
            }
            4 => {
                if self.plugin_open.is_some() {
                    let name = self.plugin_open.as_deref()?;
                    let view = self.pm.as_ref()?.get_plugin(name)?.view.as_ref()?;
                    let area = plugin_scroll_area(w, h);
                    let visible = plugin_scroll_visible(h);
                    let content = plugin_content_height(view, area);
                    let track = rect(w - 32.0 - 11.0, area.top, 6.0, visible);
                    Some((
                        ScrollLane::PluginView,
                        track,
                        content.ceil().max(0.0) as usize,
                        visible as usize,
                        self.pv_scroll.max(0) as usize,
                    ))
                } else {
                    let (list_top, row_h, max_rows) = plugin_geom(w, h);
                    let slots = plugin_slots(&self.plugin_rows);
                    let track = rect(w - 32.0 - 11.0, list_top, 6.0, max_rows as f32 * row_h);
                    Some((
                        ScrollLane::Plugins,
                        track,
                        slots.len(),
                        max_rows,
                        self.scroll,
                    ))
                }
            }
            5 => {
                let g = settings_geom(w, h);
                let total = self
                    .snap
                    .lock()
                    .ok()
                    .map(|s| settings_items(&s, &self.search, self.settings_cat).len())
                    .unwrap_or(0);
                let track = rect(
                    w - 32.0 - 11.0,
                    g.list_top,
                    6.0,
                    g.max_rows as f32 * g.row_h,
                );
                Some((ScrollLane::Settings, track, total, g.max_rows, self.scroll))
            }
            _ => None,
        }
    }

    /// 左键落在滑块上 = 开始拖。返回 true 表示这一下已被滚动条吃掉，不当作点击。
    fn begin_scroll_drag(&mut self, hwnd: HWND, px: f32, py: f32) -> bool {
        let s = self.scale(hwnd);
        let (w, h) = self.client_dip(hwnd);
        let (x, y) = (px / s, py / s);
        let Some((lane, track, total, visible, offset)) = self.scroll_geom(w, h) else {
            return false;
        };
        let Some(thumb) = scrollbar_thumb(track, total, visible, offset) else {
            return false;
        };
        if !hit(thumb, x, y) {
            return false;
        }
        self.scroll_drag = Some(ScrollDrag {
            lane,
            track,
            thumb_h: thumb.bottom - thumb.top,
            grab: y - thumb.top,
            total,
            visible,
        });
        unsafe {
            let _ = SetCapture(hwnd);
        }
        true
    }

    /// 拖动中：按滑块位置换算偏移。返回 false = 没有在拖。
    fn drag_scroll(&mut self, hwnd: HWND, _px: f32, py: f32) -> bool {
        let Some(d) = self.scroll_drag else {
            return false;
        };
        let s = self.scale(hwnd);
        let off = scroll_offset_for_y(&d, py / s);
        match d.lane {
            ScrollLane::PluginView => {
                self.pv_scroll = off as i32;
                self.pv_hover = None;
            }
            _ => self.scroll = off,
        }
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        true
    }

    /// 松手/捕获丢失：结束拖动。返回 true 表示刚才确实在拖（松手那一下不该再落成点击）。
    fn end_scroll_drag(&mut self) -> bool {
        self.scroll_drag.take().is_some()
    }

    /// 当前这一页键盘能停住的那些格子（周期 → 页签 → 本页）。
    ///
    /// 与画、点击读同一份几何：列表行用 `settings_row_rect` / `plugin_row_rect` /
    /// `table_row_band`，插件视图用 `pv_targets`。键鼠与应用两屏的行仍然不可点，
    /// 也就不进这张表 —— 焦点停过去再按 Enter 什么都不会发生，那是另一种假反馈。
    fn kbd_items(&self, w: f32, h: f32) -> Vec<KbdItem> {
        let mut out = Vec::new();
        for (_, r) in period_pills() {
            out.push(KbdItem {
                rect: r,
                group: 0,
                list_index: None,
            });
        }
        for r in view_tabs() {
            out.push(KbdItem {
                rect: r,
                group: 1,
                list_index: None,
            });
        }
        let mut page = |rect: D2D_RECT_F, list_index: Option<usize>| {
            out.push(KbdItem {
                rect,
                group: 2,
                list_index,
            })
        };
        match self.view {
            2 => {
                let card = left_card(w, h);
                let g = table_geom(card);
                let count = self
                    .snap
                    .lock()
                    .map(|s| left_rows(&s, 2, self.rank_tab, self.rank_filter).len())
                    .unwrap_or(0);
                let scroll = self.scroll.min(count.saturating_sub(g.max_rows));
                for i in scroll..count.min(scroll + g.max_rows) {
                    let vis = i - scroll;
                    page(
                        table_row_band(card, g.list_top + vis as f32 * g.row_h, g.row_h),
                        Some(i),
                    );
                }
            }
            3 => {
                for r in atab_tabs() {
                    page(r, None);
                }
            }
            4 => {
                if self.plugin_open.is_some() {
                    let card = rect(16.0, 208.0, w - 32.0, h - 208.0 - 16.0);
                    let clip = plugin_content_clip(card);
                    let view = self
                        .plugin_open
                        .as_deref()
                        .and_then(|name| self.pm.as_ref().and_then(|pm| pm.get_plugin(name)))
                        .and_then(|p| p.view.as_ref());
                    if let Some(view) = view {
                        let ops = plugin_view_ops(
                            view,
                            plugin_scroll_area(w, h),
                            self.pv_scroll as f32,
                            &self.plugin_sel,
                        );
                        for t in pv_targets_of(&ops) {
                            let cx = (t.rect.left + t.rect.right) / 2.0;
                            let cy = (t.rect.top + t.rect.bottom) / 2.0;
                            if hit(clip, cx, cy) {
                                page(t.rect, None);
                            }
                        }
                    }
                } else {
                    let (list_top, row_h, max_rows) = plugin_geom(w, h);
                    let slots = plugin_slots(&self.plugin_rows);
                    let scroll = self.scroll.min(slots.len().saturating_sub(max_rows));
                    for (slot_i, slot) in slots.iter().enumerate() {
                        if !matches!(slot, PluginSlot::Row(_)) {
                            continue;
                        }
                        if slot_i < scroll || slot_i >= scroll + max_rows {
                            continue;
                        }
                        let vis = slot_i - scroll;
                        page(
                            plugin_row_rect(w, list_top + vis as f32 * row_h, row_h),
                            Some(slot_i),
                        );
                    }
                }
            }
            5 => {
                let g = settings_geom(w, h);
                let items = self
                    .snap
                    .lock()
                    .ok()
                    .map(|s| settings_items(&s, &self.search, self.settings_cat))
                    .unwrap_or_default();
                let scroll = self.scroll.min(items.len().saturating_sub(g.max_rows));
                for (i, item) in items.iter().enumerate() {
                    if matches!(item, SItem::Header(_)) {
                        continue;
                    }
                    if i < scroll || i >= scroll + g.max_rows {
                        continue;
                    }
                    let vis = i - scroll;
                    page(
                        settings_row_rect(&g, w, g.list_top + vis as f32 * g.row_h),
                        Some(i),
                    );
                }
            }
            6 => {
                for (_, r) in dashboard::range_pills() {
                    page(r, None);
                }
            }
            _ => {}
        }
        out
    }

    /// 把焦点挪一格；`same_group` 见 [`kbd_next`]。焦点落在滚动列表之外时顺手滚过去。
    fn kbd_move(&mut self, hwnd: HWND, delta: isize, same_group: bool) {
        let (w, h) = self.client_dip(hwnd);
        let items = self.kbd_items(w, h);
        let groups: Vec<u8> = items.iter().map(|i| i.group).collect();
        let next = kbd_next(&groups, self.kbd_focus, delta, same_group);
        self.kbd_focus = next;
        if let Some(n) = next {
            let item = items[n];
            self.kbd_scroll_to(w, h, &item);
        }
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    fn kbd_scroll_to(&mut self, w: f32, h: f32, item: &KbdItem) {
        let Some(idx) = item.list_index else {
            return;
        };
        let max_rows = match self.view {
            2 => table_geom(left_card(w, h)).max_rows,
            4 => plugin_geom(w, h).2,
            5 => settings_geom(w, h).max_rows,
            _ => return,
        };
        if idx < self.scroll {
            self.scroll = idx;
        } else if idx >= self.scroll + max_rows {
            self.scroll = idx + 1 - max_rows;
        }
    }

    /// Enter：等于在焦点那一格的中心点一下 —— 命中、滚动与各页自己的处理都走同一条路。
    fn kbd_activate(&mut self, hwnd: HWND) {
        let Some(i) = self.kbd_focus else {
            return;
        };
        let (w, h) = self.client_dip(hwnd);
        let items = self.kbd_items(w, h);
        let Some(item) = items.get(i).copied() else {
            self.kbd_focus = None;
            return;
        };
        let s = self.scale(hwnd);
        let cx = (item.rect.left + item.rect.right) / 2.0 * s;
        let cy = (item.rect.top + item.rect.bottom) / 2.0 * s;
        self.on_click(hwnd, cx, cy);
    }

    /// Ctrl+F：切到设置页并把搜索框叫到屏上、拿到焦点。
    fn focus_search(&mut self, hwnd: HWND) {
        if self.view == 7 {
            self.focus_clip_search(hwnd);
            return;
        }
        if self.view != 5 {
            self.switch_view(hwnd, 5);
        }
        let edit = self.ensure_search_edit(hwnd);
        if let Some(host) = self.search_edit_host {
            if !edit.0.is_null() {
                self.place_search_edit(hwnd);
                unsafe {
                    let _ = ShowWindow(host, SW_SHOW);
                    let _ = SetFocus(Some(edit));
                }
            }
        }
    }

    /// 键盘导航：Tab / 上下 / 左右 / Enter。返回 true = 这一键已经被吃掉。
    ///
    /// 弹层开着时不接：焦点在页面背后按 Enter 会点到一个被盖住的控件。
    fn handle_nav_key(&mut self, hwnd: HWND, vk: usize) -> bool {
        if self.detail.is_some()
            || self.cleanup_confirm
            || self.plugin_uninstall.is_some()
            || self.pv_modal.is_some()
            || self.pv_select.is_some()
            || self.edit_target.is_some()
            || self.capturing.is_some()
        {
            return false;
        }
        let shift = (unsafe { GetKeyState(0x10) } as u16 & 0x8000) != 0;
        if self.view == 7 && self.handle_clip_key(hwnd, vk) {
            return true;
        }
        match vk {
            0x09 => {
                self.kbd_move(hwnd, if shift { -1 } else { 1 }, false);
                true
            }
            0x26 => {
                self.kbd_move(hwnd, -1, true);
                true
            }
            0x28 => {
                self.kbd_move(hwnd, 1, true);
                true
            }
            0x25 => {
                let v = if self.view == 0 { 7 } else { self.view - 1 };
                self.switch_view(hwnd, v);
                true
            }
            0x27 => {
                let v = (self.view + 1) % 8;
                self.switch_view(hwnd, v);
                true
            }
            0x0D => {
                self.kbd_activate(hwnd);
                true
            }
            _ => false,
        }
    }

    /// 切到第 `i` 个视图：鼠标点页签与键盘左右键共用这一份。
    fn switch_view(&mut self, hwnd: HWND, i: usize) {
        self.view = i;
        self.scroll = 0;
        self.sel = None;
        self.hover = None;
        self.heat_hover = None;
        self.chart_hover = None;
        self.pv_select = None;
        self.pv_modal = None;
        self.pv_hover = None;
        self.cleanup_confirm = false;
        self.kbd_focus = None;
        self.tooltip_ready = false;
        self.tooltip_text = None;
        // 录入态也一起清：热键录入吃掉 WM_KEYDOWN（连 Esc 都不再收窗），而它的
        // 提示画在设置页那一格里 —— 换页之后提示看不见、按键照旧被吞，
        // 用户只会觉得"这个键怎么没反应了"。
        self.capturing = None;
        if i == 5 {
            self.place_search_edit(hwnd);
        } else {
            self.hide_search_edit();
        }
        if i == 4 {
            self.reload_plugins();
        }
        // 进看板页就先问一次库（方案 3.2.3 第一行：切到这一页时查一次）。
        // 没这一句，第一次点进来看到的是上一次离开时那一页的旧数（或全空）。
        if i == 6 {
            self.reload_board();
        }
        if i == 7 {
            tracing_note("clip_page_open");
            self.clip_page.reload();
            self.place_clip_search_edit(hwnd);
        } else {
            self.hide_clip_search_edit();
        }
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    /// 第 8 页的点击：搜索框 → 模式页签 → 工具钮 → 行。
    fn on_clip_click(&mut self, hwnd: HWND, w: f32, h: f32, x: f32, y: f32) {
        let lay = clipboard_page::layout_for(w, h);
        if self.clip_page.mode != clipboard_page::Mode::Stats {
            if clipboard_page::search_at(&lay, x, y) {
                self.focus_clip_search(hwnd);
                return;
            }
            if let Some(i) = clipboard_page::toolbar_at(&lay, x, y) {
                self.act_clip_toolbar(hwnd, i);
                return;
            }
        }
        if let Some(i) = clipboard_page::tab_at(&lay, x, y) {
            let mode = clipboard_page::MODES[i];
            self.clip_page.set_mode(mode);
            self.clip_page.reload();
            if mode == clipboard_page::Mode::Stats {
                self.hide_clip_search_edit();
            } else {
                self.place_clip_search_edit(hwnd);
            }
            self.refresh_clip_page(hwnd);
            return;
        }
        let len = self.clip_page.rows().len();
        if let Some(i) = clipboard_page::row_at(&lay, self.clip_page.scroll, len, x, y) {
            let shift = (unsafe { GetKeyState(0x10) } as u16 & 0x8000) != 0;
            match clipboard_page::row_click_action(self.clip_page.multi, shift, i) {
                clipboard_page::RowClick::Select(row) => self.clip_page.sel = row,
                clipboard_page::RowClick::Toggle(row) => self.clip_page.toggle_checked(row),
                clipboard_page::RowClick::Range(row) => self.clip_page.check_range(row),
            }
            self.refresh_clip_page(hwnd);
        }
    }

    fn act_clip_toolbar(&mut self, hwnd: HWND, slot: usize) {
        match self.clip_page.mode {
            clipboard_page::Mode::History => match slot {
                0 => self.copy_clip_selected(hwnd),
                1 => self.pin_clip_selected(hwnd),
                2 => self.delete_clip_selected(hwnd),
                3 => self.save_clip_snippet(hwnd),
                4 => self.clear_clip_history(hwnd),
                5 => {
                    self.clip_page.toggle_multi();
                    tracing_note(&format!("clip_page_multi on={}", self.clip_page.multi));
                    self.refresh_clip_page(hwnd);
                }
                _ => {}
            },
            clipboard_page::Mode::Snippets => match slot {
                0 => self.copy_clip_selected(hwnd),
                1 => self.start_edit_snippet(hwnd, self.clip_page.sel),
                2 => self.delete_clip_selected(hwnd),
                5 => {
                    self.clip_page.toggle_multi();
                    tracing_note(&format!("clip_page_multi on={}", self.clip_page.multi));
                    self.refresh_clip_page(hwnd);
                }
                _ => {}
            },
            clipboard_page::Mode::Stats => {}
        }
    }

    fn copy_clip_selected(&mut self, hwnd: HWND) {
        let Some(row) = self.clip_page.selected().cloned() else {
            self.clip_page.status = Some("先选一条".to_string());
            self.refresh_clip_page(hwnd);
            return;
        };
        match focusflow_core::clipboard::copy_parts(
            row.kind,
            &row.content,
            row.image_path.as_deref(),
            &row.file_paths,
        ) {
            Ok(msg) => {
                if self.clip_page.mode == clipboard_page::Mode::Snippets {
                    let _ = focusflow_core::clipboard::touch_snippet(row.id);
                }
                tracing_note(&format!("clip_page_copy kind={:?}", row.kind));
                self.clip_page.status = Some(format!("{msg}（Ctrl+V 粘贴）"));
            }
            Err(e) => self.clip_page.status = Some(format!("复制失败：{e}")),
        }
        self.refresh_clip_page(hwnd);
    }

    fn pin_clip_selected(&mut self, hwnd: HWND) {
        let plan = self.clip_page.pin_plan();
        let (ids, target) = match plan {
            clipboard_page::PinPlan::WrongMode => {
                self.clip_page.status = Some("片段不能固定".to_string());
                self.refresh_clip_page(hwnd);
                return;
            }
            clipboard_page::PinPlan::Empty => {
                self.clip_page.status = Some("先选一条".to_string());
                self.refresh_clip_page(hwnd);
                return;
            }
            clipboard_page::PinPlan::Apply { ids, pinned } => (ids, pinned),
        };
        match focusflow_core::clipboard::set_pinned_many(&ids, target) {
            Ok(n) => {
                tracing_note(&format!("clip_page_pin n={n} pinned={target}"));
                self.clip_page.status = Some(if target {
                    format!("已固定 {n} 条（裁剪时不会删）")
                } else {
                    format!("已取消固定 {n} 条")
                });
            }
            Err(e) => self.clip_page.status = Some(format!("更新固定状态失败：{e}")),
        }
        self.clip_page.reload();
        self.refresh_clip_page(hwnd);
    }

    fn delete_clip_selected(&mut self, hwnd: HWND) {
        let ids = self.clip_page.action_ids();
        if ids.is_empty() {
            self.clip_page.status = Some("先选一条".to_string());
            self.refresh_clip_page(hwnd);
            return;
        }
        // 删除不可撤销：单条与批量都要先点一次头。固定项也会被删，标题里说清楚。
        let what = if self.clip_page.mode == clipboard_page::Mode::Snippets {
            "个片段"
        } else {
            "条记录"
        };
        let msg = if ids.len() == 1 {
            format!("删除这 1 {what}？（固定项也会被删）")
        } else {
            format!("删除选中的 {} {what}？（固定项也会被删）", ids.len())
        };
        let text = windows::core::HSTRING::from(msg);
        let answer = unsafe {
            MessageBoxW(
                Some(hwnd),
                windows::core::PCWSTR(text.as_ptr()),
                w!("删除剪贴板内容"),
                MESSAGEBOX_STYLE(MB_YESNO.0 | MB_ICONQUESTION.0),
            )
        };
        if answer != IDYES {
            return;
        }
        let result = if self.clip_page.mode == clipboard_page::Mode::Snippets {
            let mut n = 0usize;
            let mut why = None;
            for id in &ids {
                match focusflow_core::clipboard::delete_snippet(*id) {
                    Ok(true) => n += 1,
                    Ok(false) => {}
                    Err(e) => {
                        why = Some(e);
                        break;
                    }
                }
            }
            why.map_or(Ok(n), Err)
        } else {
            focusflow_core::clipboard::delete_many(&ids)
        };
        match result {
            Ok(n) => {
                tracing_note(&format!("clip_page_delete n={n}"));
                self.clip_page.status = Some(format!("已删除 {n} 条"));
                self.clip_page.clear_checked();
            }
            Err(e) => self.clip_page.status = Some(format!("删除失败：{e}")),
        }
        self.clip_page.reload();
        self.refresh_clip_page(hwnd);
    }

    fn save_clip_snippet(&mut self, hwnd: HWND) {
        let Some(row) = self.clip_page.selected().cloned() else {
            self.clip_page.status = Some("先选一条历史记录".to_string());
            self.refresh_clip_page(hwnd);
            return;
        };
        if row.kind != focusflow_core::clipboard::ClipKind::Text {
            self.clip_page.status = Some("图片/文件条目不能存为片段".to_string());
            self.refresh_clip_page(hwnd);
            return;
        }
        match focusflow_core::clipboard::add_snippet("", &row.content) {
            Ok(_) => self.clip_page.status = Some("已存为片段（Tab 切到片段）".to_string()),
            Err(e) => self.clip_page.status = Some(format!("存为片段失败：{e}")),
        }
        self.clip_page.reload();
        self.refresh_clip_page(hwnd);
    }

    fn clear_clip_history(&mut self, hwnd: HWND) {
        let answer = unsafe {
            MessageBoxW(
                Some(hwnd),
                w!("只清掉未固定的条目；固定项会留下。"),
                w!("清空剪贴板历史"),
                MESSAGEBOX_STYLE(MB_YESNO.0 | MB_ICONQUESTION.0),
            )
        };
        if answer != IDYES {
            return;
        }
        match focusflow_core::clipboard::clear_unpinned() {
            Ok(n) => self.clip_page.status = Some(format!("已清空 {n} 条未固定记录")),
            Err(e) => self.clip_page.status = Some(format!("清空失败：{e}")),
        }
        self.clip_page.clear_checked();
        self.clip_page.reload();
        self.refresh_clip_page(hwnd);
    }

    /// 片段改名：复用行内那套顶层 EDIT，落点按当前列表行算。
    fn start_edit_snippet(&mut self, hwnd: HWND, row: usize) {
        let Some(snippet) = self.clip_page.snippets.get(row).cloned() else {
            self.clip_page.status = Some("先选一个片段".to_string());
            self.refresh_clip_page(hwnd);
            return;
        };
        let (w, h) = self.client_dip(hwnd);
        let lay = clipboard_page::layout_for(w, h);
        let vis = row.saturating_sub(self.clip_page.scroll);
        if vis >= lay.rows {
            return;
        }
        let top = lay.list_top + vis as f32 * lay.row_h;
        let edit = self.ensure_edit(hwnd);
        let left = if self.clip_page.multi {
            lay.pad() + 34.0
        } else {
            lay.pad() + 12.0
        };
        let cell = rect(left, top + 4.0, 174.0, lay.row_h - 8.0);
        self.show_edit_cell(hwnd, cell);
        unsafe {
            let _ = SetWindowTextW(edit, &HSTRING::from(snippet.name.clone()));
            let _ = SetFocus(Some(edit));
            SendMessageW(edit, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
        }
        self.edit_target = Some(EditTarget::SnippetName(snippet.id));
        self.edit_row = Some(row);
    }

    fn refresh_clip_page(&mut self, hwnd: HWND) {
        let (w, h) = self.client_dip(hwnd);
        let lay = clipboard_page::layout_for(w, h);
        self.clip_page.clamp_selection(lay.rows);
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    /// 宿主 EDIT 的文字变了（EN_CHANGE 转进来的）：是剪贴板页的搜索框就当场筛。
    ///
    /// 每次按键重查一次库：剪贴板列表上限 500 行、`instr` 扫描，代价与一次翻页同量级。
    /// 同一个词不再重查 —— `place_clip_search_edit` 回填同一段文字也会触发 EN_CHANGE。
    fn live_clip_search(&mut self, hwnd: HWND, edit: HWND) {
        if self.view != 7 || self.clip_search_edit != Some(edit) {
            return;
        }
        let filter = read_hosted_edit_text(edit).trim().to_string();
        if filter == self.clip_page.filter {
            return;
        }
        self.clip_page.filter = filter;
        self.clip_page.sel = 0;
        self.clip_page.scroll = 0;
        // 片段页筛的是片段列表，得走整份 reload；历史页只重查历史，别连带重算片段与统计。
        if self.clip_page.mode == clipboard_page::Mode::Snippets {
            self.clip_page.reload();
        } else {
            self.clip_page.reload_items();
        }
        tracing_note(&format!(
            "clip_page_live_filter len={} hits={}",
            self.clip_page.filter.chars().count(),
            self.clip_page.rows().len()
        ));
        self.refresh_clip_page(hwnd);
    }

    /// 剪贴板页的搜索框：与设置页那个分开建，子类 id 不同，焦点回执也分开。
    fn ensure_clip_search_edit(&mut self, parent: HWND) -> HWND {
        if let Some(e) = self.clip_search_edit {
            return e;
        }
        let (host, h) = create_hosted_edit(parent, self.ui_font, CLIP_SEARCH_EDIT_SUBCLASS);
        tracing_note(&format!(
            "剪贴板搜索框已创建 host={} edit={}",
            host.0 as isize, h.0 as isize
        ));
        self.clip_search_host = Some(host);
        self.clip_search_edit = Some(h);
        h
    }

    fn place_clip_search_edit(&mut self, hwnd: HWND) {
        if self.view != 7 || self.clip_page.mode == clipboard_page::Mode::Stats {
            return;
        }
        let (w, h) = self.client_dip(hwnd);
        let lay = clipboard_page::layout_for(w, h);
        let edit = self.ensure_clip_search_edit(hwnd);
        let Some(host) = self.clip_search_host else {
            return;
        };
        if edit.0.is_null() {
            return;
        }
        if self.clip_search_focused {
            unsafe {
                let _ = SetWindowTextW(edit, &HSTRING::from(self.clip_page.filter.clone()));
            }
        }
        self.place_hosted_edit(hwnd, host, edit, lay.search, self.clip_search_focused);
    }

    fn hide_clip_search_edit(&mut self) {
        self.clip_search_focused = false;
        if let Some(e) = self.clip_search_edit {
            unsafe {
                let _ = ShowWindow(e, SW_HIDE);
            }
        }
        if let Some(h) = self.clip_search_host {
            unsafe {
                let _ = ShowWindow(h, SW_HIDE);
            }
        }
    }

    fn focus_clip_search(&mut self, hwnd: HWND) {
        if self.view != 7 || self.clip_page.mode == clipboard_page::Mode::Stats {
            return;
        }
        let edit = self.ensure_clip_search_edit(hwnd);
        self.clip_search_focused = true;
        self.place_clip_search_edit(hwnd);
        if !edit.0.is_null() {
            unsafe {
                let _ = SetFocus(Some(edit));
            }
        }
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    /// 搜索框按 Enter：把字变成筛选词并重查；Esc：清掉筛选。
    fn commit_clip_search(&mut self, hwnd: HWND, commit: bool) {
        let Some(edit) = self.clip_search_edit else {
            return;
        };
        let text = read_hosted_edit_text(edit);
        self.clip_page.filter = if commit {
            text.trim().to_string()
        } else {
            String::new()
        };
        if !commit {
            unsafe {
                let _ = SetWindowTextW(edit, w!(""));
            }
        }
        self.clip_page.sel = 0;
        self.clip_page.scroll = 0;
        self.clip_page.reload();
        tracing_note(&format!(
            "剪贴板搜索提交 commit={commit} qlen={} edit={}",
            self.clip_page.filter.chars().count(),
            edit.0 as isize
        ));
        self.clip_page.status = Some(if self.clip_page.filter.is_empty() {
            "筛选已清除".to_string()
        } else {
            format!("筛选：{}", self.clip_page.filter)
        });
        self.refresh_clip_page(hwnd);
    }

    /// 剪贴板页的键盘：↑↓/Enter/Del/Tab/Ctrl+P/Ctrl+1..6。
    fn handle_clip_key(&mut self, hwnd: HWND, vk: usize) -> bool {
        let (w, h) = self.client_dip(hwnd);
        let rows = clipboard_page::layout_for(w, h).rows;
        let ctrl = (unsafe { GetKeyState(0x11) } as u16 & 0x8000) != 0;
        const VK_TAB: usize = 0x09;
        if vk == VK_TAB {
            let next = match self.clip_page.mode {
                clipboard_page::Mode::History => clipboard_page::Mode::Snippets,
                clipboard_page::Mode::Snippets => clipboard_page::Mode::Stats,
                clipboard_page::Mode::Stats => clipboard_page::Mode::History,
            };
            self.clip_page.set_mode(next);
            self.clip_page.reload();
            if next == clipboard_page::Mode::Stats {
                self.hide_clip_search_edit();
            } else {
                self.place_clip_search_edit(hwnd);
            }
            self.refresh_clip_page(hwnd);
            return true;
        }
        if self.clip_page.mode == clipboard_page::Mode::Stats {
            return false;
        }
        match vk {
            0x26 => {
                self.clip_page.move_selection(-1, rows);
                self.refresh_clip_page(hwnd);
                true
            }
            0x28 => {
                self.clip_page.move_selection(1, rows);
                self.refresh_clip_page(hwnd);
                true
            }
            0x24 => {
                self.clip_page.sel = 0;
                self.clip_page.clamp_selection(rows);
                self.refresh_clip_page(hwnd);
                true
            }
            0x23 => {
                self.clip_page.sel = self.clip_page.rows().len().saturating_sub(1);
                self.clip_page.clamp_selection(rows);
                self.refresh_clip_page(hwnd);
                true
            }
            0x21 => {
                self.clip_page.move_selection(-(rows as i32), rows);
                self.refresh_clip_page(hwnd);
                true
            }
            0x22 => {
                self.clip_page.move_selection(rows as i32, rows);
                self.refresh_clip_page(hwnd);
                true
            }
            0x0D => {
                self.copy_clip_selected(hwnd);
                true
            }
            0x2E => {
                self.delete_clip_selected(hwnd);
                true
            }
            0x41 if ctrl => {
                // Ctrl+A：整页勾上（按当前筛选后的列表），并进入多选。
                self.clip_page.check_all();
                tracing_note(&format!(
                    "clip_page_check_all n={}",
                    self.clip_page.checked_count()
                ));
                self.refresh_clip_page(hwnd);
                true
            }
            0x50 if ctrl => {
                self.pin_clip_selected(hwnd);
                true
            }
            v @ 0x31..=0x36 if ctrl => {
                let idx = v - 0x31;
                if let Some(t) = focusflow_core::clipboard::Transform::ALL.get(idx).copied() {
                    let Some(row) = self.clip_page.selected().cloned() else {
                        return true;
                    };
                    if row.kind != focusflow_core::clipboard::ClipKind::Text {
                        self.clip_page.status = Some("只有文本条目能加工".to_string());
                        self.refresh_clip_page(hwnd);
                        return true;
                    }
                    match t.apply(&row.content) {
                        Ok(text) => match focusflow_core::clipboard::write_text(&text) {
                            Ok(()) => {
                                self.clip_page.status =
                                    Some(format!("已{}并复制到剪贴板", t.label()))
                            }
                            Err(e) => self.clip_page.status = Some(format!("复制失败：{e}")),
                        },
                        Err(e) => self.clip_page.status = Some(e),
                    }
                    self.refresh_clip_page(hwnd);
                }
                true
            }
            _ => false,
        }
    }

    /// 鼠标移动：排行页更新悬停行，分析页更新悬停键 / 悬停那一格；返回是否需要重画。
    fn on_move(&mut self, hwnd: HWND, px: f32, py: f32) -> bool {
        let mut changed = false;
        let (w, h) = self.client_dip(hwnd);
        let s = self.scale(hwnd);
        let (x, y) = (px / s, py / s);
        self.ptr = (x, y);
        // 设备详情弹窗开着 = 页面模态：这一屏除了右上角三颗什么都不许亮着。
        if self.detail.is_some() {
            let ctl = self.control_at(w, h, x, y);
            changed |= self.hover.take().is_some();
            changed |= self.heat_hover.take().is_some();
            changed |= self.chart_hover.take().is_some();
            changed |= self.pv_hover.take().is_some();
            changed |= self.tooltip_ready;
            self.tooltip_ready = false;
            changed |= ctl != self.ctl;
            self.ctl = ctl;
            return changed;
        }
        // 控件悬停只影响底色与光标，不参与点击判定；每一条早退分支之前算完，
        // 免得"在分析页上药丸不亮"。
        let ctl = self.control_at(w, h, x, y);
        // 换了一个控件/行：tooltip 的等待重新计时（同一个目标里挪一格不算）。
        if ctl != self.ctl {
            self.hover_since = std::time::Instant::now();
            if self.tooltip_ready {
                self.tooltip_ready = false;
                changed = true;
            }
        }
        changed |= ctl != self.ctl;
        self.ctl = ctl;

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

        // 行悬停与光标读同一份命中：`control_at` 已经把"哪一行"算出来了，这里不再
        // 各写一遍循环（从前三处各数一遍，行高一改就有一处漏改，亮一行点另一行）。
        changed |= self.heat_hover.take().is_some();
        changed |= self.chart_hover.take().is_some();
        let hover = match ctl {
            Some(Ctl::SetRow(i))
            | Some(Ctl::TableRow(i))
            | Some(Ctl::PluginList(i))
            | Some(Ctl::ClipRow(i)) => Some(i as usize),
            _ => None,
        };
        if hover != self.hover {
            self.hover = hover;
            changed = true;
        }
        // 插件视图上的悬停：弹窗/下拉/卸载确认开着时不亮（它们把整页压住了）。
        let pv_hover = if self.pv_select.is_some()
            || self.pv_modal.is_some()
            || self.plugin_uninstall.is_some()
        {
            None
        } else {
            match ctl {
                Some(Ctl::Pv) => {
                    let card = rect(16.0, 208.0, w - 32.0, h - 208.0 - 16.0);
                    let back = rect(card.left + 16.0, card.top + 12.0, 64.0, 24.0);
                    if hit(back, x, y) {
                        Some(PvTarget {
                            rect: back,
                            row: false,
                        })
                    } else {
                        let clip = plugin_content_clip(card);
                        self.pv_targets_update(w, h)
                            .and_then(|ts| pv_target_on(ts, clip, x, y))
                    }
                }
                _ => None,
            }
        };
        if pv_hover != self.pv_hover {
            self.pv_hover = pv_hover;
            changed = true;
        }
        changed
    }

    /// 右上角三颗：最小化 / 最大化-还原 / 关闭。
    /// 关闭走 `WM_CLOSE`——退出前的 flush 与备份那条路只在那一处。
    fn press_window_button(&mut self, hwnd: HWND, i: u8) {
        match i {
            0 => unsafe {
                let _ = ShowWindow(hwnd, SW_MINIMIZE);
            },
            1 => {
                let zoomed = unsafe { IsZoomed(hwnd) }.as_bool();
                unsafe {
                    let _ = ShowWindow(hwnd, if zoomed { SW_RESTORE } else { SW_MAXIMIZE });
                }
            }
            _ => {
                let _ = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
            }
        }
    }

    fn on_click(&mut self, hwnd: HWND, px: f32, py: f32) {
        // 点到编辑框外面：先把这一笔提交掉（与网页版一致，失焦即保存）。
        if self.edit_target.is_some() {
            self.finish_edit(hwnd, true);
        }
        let (w, h) = self.client_dip(hwnd);
        let s = self.scale(hwnd);
        let (x, y) = (px / s, py / s);

        // 最上面一层：窗口按钮 → 详情弹窗（点框外或点关闭都算关）→ 页面。
        match top_owner(self.detail.is_some(), w, x, y) {
            TopOwner::WinBtn(i) => {
                self.press_window_button(hwnd, i);
                return;
            }
            TopOwner::DetailModal => {
                let (dialog, close) = detail_rects(w, h);
                if hit(close, x, y) || !hit(dialog, x, y) {
                    self.close_detail(hwnd);
                }
                return;
            }
            TopOwner::Page => {}
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
                self.switch_view(hwnd, i);
                return;
            }
        }

        // 看板页那三个范围胶囊（就在页签右边；矩形与画/悬停同源，命中用全仓那一个 `hit`）。
        if self.view == 6 {
            for (range, pill) in dashboard::range_pills() {
                if !hit(pill, x, y) {
                    continue;
                }
                // `set_range` 答 false = 点的就是当前那一档：那时不重查，
                // 否则同一份数被"再点一下"白问一遍，而页面上什么都没变。
                if self.dash.set_range(range) {
                    tracing_note(&format!("看板换档 {} at ({x:.0},{y:.0})", range.label()));
                    self.reload_board();
                }
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                return;
            }
        }

        if self.view == 7 {
            self.on_clip_click(hwnd, w, h, x, y);
            return;
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
            tracing_note(&format!("点击插件页 at ({x:.0},{y:.0})"));
            // 卸载确认框开着：只认那三颗按钮（点遮罩不关，免得手一抖点到"删除设置"）。
            if let Some(prompt) = self.plugin_uninstall.clone() {
                let (_, buttons) = uninstall_prompt_geometry(w, h);
                if hit(buttons[0], x, y) {
                    self.finish_uninstall(hwnd, &prompt, false);
                } else if hit(buttons[1], x, y) {
                    self.finish_uninstall(hwnd, &prompt, true);
                } else if hit(buttons[2], x, y) {
                    self.plugin_uninstall = None;
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                return;
            }
            // 插件视图打开时：弹窗/下拉 → 返回 → 视图里的控件。
            if let Some(name) = self.plugin_open.clone() {
                let card = rect(16.0, 208.0, w - 32.0, h - 208.0 - 16.0);
                let pv = self.pm.as_ref().and_then(|pm| {
                    pm.get_plugin(&name)
                        .and_then(|p| p.view.as_ref())
                        .map(|v| (name.clone(), v.clone()))
                });
                let Some((n, view)) = pv else {
                    // 插件已经不在了（文件被删、热重载把它卸了）或视图取不出来。屏上这一拍
                    // 画的是**清单**，而点击还按"视图开着"处理就会被这里全吞掉：点哪里都没
                    // 反应，只有 Esc 能出来。先把状态收掉并说一句，下一次点击就落回清单。
                    self.plugin_open = None;
                    self.reset_plugin_view_state();
                    self.status = Some(format!(
                        "插件 {name} 的视图已经没了（文件被删或取不出来），已回到清单"
                    ));
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
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
                                    } else if f.kind == "date" {
                                        // 日期不再走自由文本：点开就是日历，选哪天回写哪天。
                                        self.open_date_picker(hwnd, &n, &f.field, &f.value, vb);
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
                // 谁认领这次点击就由谁处理（判据见 `pv_op_claims_click`）：表格不再
                // 把"落在它矩形外面"的点击也吞掉，排在它后面的分页条才点得到；
                // 而正文可见区之外（标题那一条）的点击谁都不归 —— 滚上去的控件看不见。
                let Some(op) = pv_claiming_op_in(&ops, plugin_content_clip(card), x, y) else {
                    return;
                };
                match op {
                    POp::Button {
                        id,
                        modal,
                        sel,
                        group,
                        ..
                    } => {
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
                        // 落在表头/空白行上的点击由这张表吞掉；表格矩形之外的点击
                        // 根本到不了这里（`pv_op_claims_click` 已经筛过）。
                        let Some(i) = (0..ids.len()).find(|i| hit(pv_table_row_rect(r, *i), x, y))
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
                    // 输入框：点值那一格 → 开同一个行内 EDIT，回车回写给插件
                    POp::Field {
                        rect: r,
                        value,
                        field,
                        ..
                    } => {
                        let vb = pv_field_value_rect(*r);
                        tracing_note(&format!(
                            "点中插件字段 {field} at ({x:.0},{y:.0}) rect=({:.0},{:.0})",
                            vb.left, vb.top
                        ));
                        self.start_plugin_field_edit(hwnd, &n, field, value, vb);
                        return;
                    }
                    _ => {}
                }
                return;
            }
            // 详情页开着：只认「返回」，别的点法一律吞掉 —— 否则一次点击会落到身后那张
            // 清单上（屏上画的是详情，点下去却打开了某个插件的视图）。
            if let Some(stem) = self.plugin_detail_open.clone() {
                // 与画那一支读同一份判断：这一格还在清单里，详情页才成立。
                if matches!(
                    plugin_page_of(None, Some(stem.as_str()), &self.plugin_rows),
                    PluginPage::Detail(_)
                ) {
                    let card = rect(16.0, 208.0, w - 32.0, h - 208.0 - 16.0);
                    // 有安装记录的那一格右上角才有「卸载…」：与画它的条件同一个布尔值。
                    if self.plugin_detail_installed && hit(detail_uninstall_rect(card), x, y) {
                        if let Some(name) = self
                            .plugin_rows
                            .iter()
                            .find(|r| r.stem == stem)
                            .map(|r| r.name.clone())
                        {
                            self.plugin_uninstall = Some(UninstallPrompt {
                                stem: stem.clone(),
                                name,
                            });
                            unsafe {
                                let _ = InvalidateRect(Some(hwnd), None, false);
                            }
                        }
                        return;
                    }
                    let back = rect(card.left + 16.0, card.top + 12.0, 64.0, 24.0);
                    if hit(back, x, y) {
                        self.close_plugin_detail(hwnd);
                    }
                    return;
                }
                // 那一格已经不在清单里了（文件被删/改名）：退回清单，让这一次点击按清单走。
                self.plugin_detail_open = None;
                self.plugin_detail_source = None;
                self.plugin_detail_installed = false;
            }
            // 清单页右上角的安装入口。
            let card = rect(16.0, 208.0, w - 32.0, h - 208.0 - 16.0);
            if hit(plugin_install_rect(card), x, y) {
                self.install_plugin_package(hwnd);
                return;
            }
            let (list_top, row_h, max_rows) = plugin_geom(w, h);
            // 命中读的是与画同一份格子表（[`plugin_slots`]）：表头占一格，
            // 少了这一格，装了内置模块之后每一次点击都会落到上面那一行去。
            let slots = plugin_slots(&self.plugin_rows);
            let scroll = self.scroll.min(slots.len().saturating_sub(max_rows));
            self.scroll = scroll;
            if let Some((idx, vis)) =
                plugin_list_row_at(&slots, list_top, row_h, max_rows, w, scroll, x, y)
            {
                let y0 = list_top + vis as f32 * row_h;
                let row_rect = plugin_row_rect(w, y0, row_h);
                // 验收脚本按坐标点插件行：这一句是那条点击唯一的对账痕迹
                // （点偏了与"按钮没反应"在屏上分不开）。
                tracing_note(&format!("点击插件行 {idx} at ({x:.0},{y:.0})"));
                self.sel = Some(idx);
                let vb = rect(row_rect.right - 76.0, y0 + 9.0, 60.0, 24.0);
                // 命中条件与画它的条件同一个（`needs_grant`）：只画一颗不点不动的按钮、
                // 或留一块看着能点其实点到"打开视图"的空格，都是这一页说假话的方式
                let grantable = self
                    .plugin_rows
                    .get(idx)
                    .map(|r| r.needs_grant)
                    .unwrap_or(false);
                if hit(vb, x, y) {
                    self.toggle_plugin(hwnd, idx);
                } else if grantable && hit(plugin_grant_rect(row_rect.right, y0), x, y) {
                    self.grant_plugin(hwnd, idx);
                } else if hit(plugin_detail_rect(row_rect.right, y0), x, y) {
                    self.open_plugin_detail(hwnd, idx);
                } else if let Some(name) = self.plugin_rows.get(idx).map(|r| r.name.clone()) {
                    // 点行本身 = 看这个插件的视图
                    self.open_plugin_view(hwnd, &name);
                }
                return;
            }
            self.sel = None;
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            return;
        }
        if self.view == 5 {
            // 清理确认开着：只认那两颗按钮；点框外/取消都算取消。
            if self.cleanup_confirm {
                let (dialog, buttons) = cleanup_prompt_geometry(w, h);
                let run = cleanup_choice_at(dialog, buttons, x, y);
                let inside = hit(dialog, x, y);
                let cancel_btn = hit(buttons[0], x, y);
                self.cleanup_confirm = false;
                if run {
                    tracing_note("cleanup-run=1");
                    self.run_action(hwnd, CLEANUP_ACTION);
                } else if !inside || cancel_btn {
                    tracing_note("cleanup-cancel=1");
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                return;
            }
            // 搜索框的输入层是顶层弹出窗：平时藏着（提示文案归主窗画），
            // 点框身这一下才把它叫到屏上并接住键盘。
            if hit(search_box_rect(w), x, y) {
                let edit = self.ensure_search_edit(hwnd);
                if let Some(host) = self.search_edit_host {
                    if !edit.0.is_null() {
                        self.place_search_edit(hwnd);
                        unsafe {
                            let _ = ShowWindow(host, SW_SHOW);
                            let _ = SetFocus(Some(edit));
                        }
                        return;
                    }
                }
            }
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
            if let Some((idx, vis)) = settings_row_at(&items, &g, w, scroll, x, y) {
                self.sel = Some(idx);
                let y0 = g.list_top + vis as f32 * g.row_h;
                match items[idx] {
                    SItem::Header(_) => {}
                    SItem::Action(a) if a == CLEANUP_ACTION => {
                        self.cleanup_confirm = true;
                        tracing_note("cleanup-confirm=1");
                        unsafe {
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                    }
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
                        self.status = Some(toggle_floating(hwnd, None));
                        unsafe {
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                    }
                    SItem::Basic(Basic::Collector) => self.toggle_collector(hwnd),
                    SItem::Basic(Basic::Pause) => self.toggle_pause(hwnd),
                    SItem::Basic(Basic::Theme) => {
                        self.status = Some(toggle_theme(hwnd));
                    }
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
                                    let note = config_write_note(
                                        &format!("热键 {key}"),
                                        focusflow_core::config::instance()
                                            .set_now(section, key, &v),
                                    );
                                    let msg = self.reload_hotkeys();
                                    // 这句留着 `key = 值` 那个 ASCII 形状（与
                                    // `register_combo` 同一形状）：验收脚本只能匹配
                                    // ASCII，中文夹在中间它就逮不到。
                                    tracing_note(&format!(
                                        "热键 {key} = {v:?} 由值框右边那颗钮改过：{msg}"
                                    ));
                                    self.status = Some(format!("{msg}{note}"));
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

        match table_row_at(count, &g, r, self.view, self.scroll, x, y) {
            Some((idx, vis)) => {
                self.sel = Some(idx);
                let y0 = g.list_top + vis as f32 * g.row_h;
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
            None => self.sel = None,
        }
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    /// 设置页底部那八颗「执行」：备份 / 压缩 / 清理 / 三种导出 / 导入 / 换数据文件夹，
    /// 都在后台线程跑。点下去那句"正在…"取自 `ACTIONS` 的第三栏（一颗钮一句，
    /// 借不得别人的话 —— 「正在清理」那句在屏上等同于"我正在删不可撤销的明细"）。
    fn run_action(&mut self, hwnd: HWND, which: usize) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.status = Some(action_busy_line(which).to_string());
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
                        let max = focusflow_core::config::instance().get_int(
                            "database",
                            "max_backups",
                            5,
                        );
                        let out = backup_database(max);
                        // 拷贝前那批增量没等到 ⇒ 这句"备份完成"必须带那半句（屏上唯一的读者）
                        let missed = out.missed_batch();
                        match out {
                            BackupOutcome::Done { count, failed, .. } => {
                                backup_status_line(count, &failed, missed)
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
                            // 快照那份锁读不动（只有别的线程正持着它 panic 过才会这样）。
                            // 这一支从前是一条光秃秃的 `return`：既没往 `ops` 里留一句回执，
                            // 也没投 `WM_OP_DONE`，而界面的 `busy` 只有那条消息会清 ⇒ 三颗
                            // 「执行」从此永远灰着，非重启不活。旁边的早退分支都做两件事，照做。
                            let target = HWND(hwnd_val as *mut core::ffi::c_void);
                            if let Ok(mut q) = ops.lock() {
                                q.push("导出没开始：这一轮的快照读不动".to_string());
                            }
                            let _ = unsafe {
                                PostMessageW(Some(target), WM_OP_DONE, WPARAM(0), LPARAM(0))
                            };
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
                        // 执行前快照没兜住那半句同样不许在这里掉（见 `cleanup_status_line`）。
                        let r = cleanup_old_data(30);
                        cleanup_status_line(30, &r)
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
                        // 导入是往库里写行、不产生任何键鼠事件，而屏上「今日」与「今日活跃」
                        // 读的是写线程那份内存缓存（`today_count_for_display` 只认缓存，
                        // 不把库值加回来）⇒ 不重锚就是"回执说了、屏上没改"，
                        // 今日那个数一直到下次重启才对上。
                        // 这句必须待在 data-op 这条后台线程上：它要给写线程投消息并最多等 3 秒，
                        // 挪进窗口过程就是把整扇主窗冻在那 3 秒里。
                        if collect::reanchor_today_after_external_write() {
                            tracing_note(
                                "导入后今日已按库值重锚（采集开着时屏上那个今日读的是写线程缓存）",
                            );
                        }
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
            [
                self.hotkey_hk,
                self.snip_hk,
                self.snip_annotate_hk,
                self.clipboard_hk,
            ],
        );
        self.hotkey_hk = None;
        self.snip_hk = None;
        self.snip_annotate_hk = None;
        self.clipboard_hk = None;
        if !self.hotkeys_on {
            return "全局热键已关闭：四条组合都不注册".to_string();
        }
        // 清单走 `COMBO_KEYS` 那一份（开机那条路读同一枚表），这里不抄第二遍：
        // 抄漏一条的症状是"开关关掉之后还剩一条热键在占键"。
        let clipboard_enabled =
            focusflow_core::config::instance().get_bool("clipboard", "enabled", true);
        let mut slots: [Option<HotKey>; 4] = [None, None, None, None];
        let mut parts: Vec<(&'static str, String, Option<String>)> = Vec::new();
        let mut offs: Vec<&'static str> = Vec::new();
        for (i, key) in combo_keys(true, clipboard_enabled).iter().enumerate() {
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
        self.clipboard_hk = slots[3];
        hotkey_status_line(&parts, &offs)
    }

    /// 全局热键总开关：三条绑定（唤出 / 截图 / 截图进标注）一起注册或一起取消，
    /// 配置键 `hotkey.enabled` 跟着写一份，**启动时也读它**（见 `make_hotkeys` 与
    /// `combo_keys`）⇒ 这一格关掉的，重启之后还是关的；设置页那一行的显示状态取自
    /// "真正注册上的组合"，所以屏上说的与按键实际会发生的是同一件事。
    fn toggle_hotkey(&mut self, hwnd: HWND) {
        self.hotkeys_on = !self.hotkeys_on;
        let mut msg = self.reload_hotkeys();
        msg.push_str(&config_write_note(
            "全局热键总开关",
            focusflow_core::config::instance().set_now(
                "hotkey",
                "enabled",
                if self.hotkeys_on { "true" } else { "false" },
            ),
        ));
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
        let note = config_write_note(
            &format!("热键 {key}"),
            focusflow_core::config::instance().set_now(section, key, &combo),
        );
        let msg = self.reload_hotkeys();
        tracing_note(&format!("热键 {key} = {combo:?} 由按下录入写入：{msg}"));
        self.status = Some(format!("{msg}{note}"));
        self.refresh_now();
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        true
    }

    /// 开/关采集。开是立即生效的；关的未来是「现在停止计数 + 下次启动不再初始化」——
    /// 写线程/应用采样/设备侧信道是 `Database::init` 起的一次性线程，core 没有对外停止接口
    /// （而且本轮不动 core），所以彻底退出要重启。
    ///
    /// ⚠ 「关掉再打开」以前在同一场里根本回不来：分支判的是 `is_started()`，而
    /// `stop_listening` 不清那个槽 ⇒ 每点一次都只走停用分支。现在按三态走
    /// （`collect::collector_toggle_action`），回来那一条只解暂停、不再起第二个钩子。
    fn toggle_collector(&mut self, hwnd: HWND) {
        let cfg = focusflow_core::config::instance();
        const ON_MSG: &str = "采集已启用：开始记录键鼠（全局，仅本机）";
        let msg = match collect::collector_toggle_action(
            collect::is_listening(),
            collect::is_started(),
        ) {
            collect::CollectorAction::Stop => {
                let note =
                    config_write_note("采集开关", cfg.set_now("collector", "enabled", "false"));
                collect::stop_listening();
                format!("采集已停用：键鼠与设备计数已停，重启后彻底退出{note}")
            }
            collect::CollectorAction::StartFresh => match collect::start(cfg) {
                Ok(()) => {
                    let note =
                        config_write_note("采集开关", cfg.set_now("collector", "enabled", "true"));
                    format!("{ON_MSG}{note}")
                }
                Err(e) => format!("采集没起来：{e}"),
            },
            collect::CollectorAction::Resume => match collect::resume_listening() {
                Ok(()) => {
                    let note =
                        config_write_note("采集开关", cfg.set_now("collector", "enabled", "true"));
                    format!("{ON_MSG}{note}")
                }
                Err(e) => format!("采集没起来：{e}"),
            },
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
    ///
    /// 采集被「停用」之后不许借这里把计数打开：停用只是不 `start()`，老监听还在跑，
    /// 解暂停就等于重新计数。判断读 `listening`，不读 `started`（后者停用后仍为 true）。
    fn toggle_pause(&mut self, hwnd: HWND) {
        let msg = if !collect::is_listening() {
            "采集没启用，先在上面那一行打开采集".to_string()
        } else {
            match collect::toggle_pause() {
                Some(true) => "采集已暂停：键鼠与设备计数都停了".to_string(),
                Some(false) => "采集已继续".to_string(),
                None => "这一拍没切成：采集状态锁被占住".to_string(),
            }
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

    /// 托盘里的「记录剪贴板」勾与配置同源（不读进程里某个缓存位：这个开关每一条
    /// 复制都要读一次配置，托盘只是它的第二个入口）。
    fn sync_clipboard_item(&self) {
        if let Some(it) = &self.clipboard_item {
            it.set_checked(focusflow_core::config::instance().get_bool(
                "clipboard",
                "enabled",
                true,
            ));
        }
    }

    /// 托盘「记录剪贴板」：写配置、对齐勾、重注册热键（关掉时第 4 条不再占用全局键）。
    fn toggle_clipboard(&mut self, hwnd: HWND) {
        let cfg = focusflow_core::config::instance();
        let now = !cfg.get_bool("clipboard", "enabled", true);
        let note = config_write_note(
            "剪贴板记录",
            cfg.set_now("clipboard", "enabled", if now { "true" } else { "false" }),
        );
        self.sync_clipboard_item();
        let hk = self.reload_hotkeys();
        let msg = format!(
            "剪贴板记录已{}；{hk}{note}",
            if now { "开启" } else { "暂停" }
        );
        self.status = Some(msg.clone());
        tracing_note(&msg);
        clipboard_popup::notify_changed();
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    /// 插件管理器懒创建：Lua 不是 Send，只能挂。
    fn ensure_plugin_manager(&mut self) {
        if self.pm.is_none() {
            let cfg = focusflow_core::config::instance();
            self.pm = Some(PluginManager::new(cfg, Arc::clone(&self._db)));
            // 建起来这一刻就把检测线程开掉：「改一行 Lua 要重启」这件症状跟用户去没去过
            // 插件页无关，而懒创建意味着"没人碰过插件"时本来就没有重载可做。
            // 线程只扫目录 mtime、不碰 Lua；真换上/卸下留在 GUI 线程的 `pump_plugin_reloads`。
            // 退出时不 join 它：那一轮 sleep 是 2 秒，join 会把退出拖成"点了没反应"，
            // 而进程一退，线程自己就没了（它不参与落盘，存储那一步在 quit 前已经问过了）。
            if let Some(pm) = self.pm.as_mut() {
                pm.enable_hot_reload();
            }
        }
    }

    /// 一问热重载通道，把盘上改过的脚本换上来（删除的那些走卸载）。
    /// 每拍一次，通道空着时就是一次空的 `try_recv`。
    fn pump_plugin_reloads(&mut self, on_plugin_page: bool) {
        let changed = match self.pm.as_mut() {
            Some(pm) => pm.poll_reload_requests(),
            None => return,
        };
        let (rescan, note) = reload_followup(on_plugin_page, &changed);
        if let Some(note) = note {
            tracing_note(&note);
        }
        if rescan {
            self.reload_plugins();
        }
    }

    /// 问一次内置看板页的那一屏数（切到这一页、换范围、以及停在这一页每 30 秒各来一次）。
    ///
    /// 「今日」那一档必须吃上还没落库的增量：主页顶部那颗卡片念的是"库值 + 增量"，
    /// 这一页只读库值的话，同一屏上就会有两个今日数（打卡条那颗点为这件事改过一次，
    /// 同一个症状不该在新页面上重犯）。代价也是同一份：落库那一瞬可能重复计一次，
    /// 那是既有读数一起承担的窗口，不是这一页新引入的。
    fn reload_board(&mut self) {
        let range = self.dash.range;
        self.dash.board = focusflow_core::dashboard::collect(range, collect::pending_today());
        self.dash.mark_loaded();
    }

    /// 重扫插件目录（进插件页、切换开关之后各来一次）。
    fn reload_plugins(&mut self) {
        self.ensure_plugin_manager();
        // 第一次进来先把「启用中的插件」真的加载一遍：只有加载才会产出错误。
        if !self.plugins_loaded {
            if let Some(pm) = self.pm.as_mut() {
                // 闩只认「这一轮清单真的拿到了」：`load_all` 在目录读不动时一个插件都没试，
                // 那时抬起闩等于把整个会话的加载机会一起关掉 —— 之后每次进页只重扫清单，
                // 行上写着「已启用，尚未加载」而不说原因，唯一出路是把每个插件关一下再开。
                self.plugins_loaded = pm.load_all();
            }
        }
        // 进这一页先把存储里攒着的改动落一次盘：这一格要说"占用多少"与"最近一次写没写成"，
        // 去抖那 300ms 里进来看，说的就是上一次的值。失败的原因不在这里重复出声 ——
        // 它已经写在每一行的第二格上（`store_error`）。清单也不每秒重扫：它在进页、
        // 开关之后，以及热重载真换上来东西时各重扫一次（见 `reload_followup`）。
        focusflow_core::plugins::plugin_store::flush_all();
        let Some(pm) = self.pm.as_ref() else { return };
        match pm.list_discovered() {
            Ok(list) => {
                self.plugin_rows = list
                    .into_iter()
                    .map(|p| {
                        // 先在 `p` 上问完（结构体字面量会把它部分移走）
                        let builtin = p.source == focusflow_core::plugins::PluginSource::Builtin;
                        let resident = p.lifecycle()
                            == focusflow_core::plugins::manager::PluginLifecycle::Always;
                        let stem = p.file.clone();
                        let store_bytes = focusflow_core::plugins::plugin_store::used_bytes(&stem);
                        let store_error = focusflow_core::plugins::plugin_store::last_error(&stem);
                        // 授权那一句读 core 算好的那份，页面不自己判"哈希对不对"
                        let grant_note = p.authorization.note();
                        let needs_grant = p.authorization.needs_action();
                        let focusflow_core::plugins::PluginManifest {
                            name,
                            desc,
                            version,
                            author,
                            api,
                            permissions,
                            refresh_ms,
                            min_app,
                            lifecycle: _,
                        } = p.manifest;
                        // 耗时与历史那一句话读 core 算好的那一份（每一格最多摸一次文件：
                        // 台账那一格在本进程第一次被问时才读盘，之后都在内存里）
                        let health = focusflow_core::plugins::plugin_health::summary_for(&stem);
                        PluginRow {
                            name,
                            desc,
                            version,
                            author,
                            enabled: p.enabled,
                            loaded: p.loaded,
                            error: p.error,
                            stem,
                            builtin,
                            resident,
                            api,
                            permission_names: permissions.clone(),
                            permissions: permissions.len(),
                            grant_note,
                            needs_grant,
                            slow: p.slow,
                            note: p.note.clone(),
                            refresh_ms,
                            min_app,
                            // 存储那一格按**文件名**问（键从来进不了路径，见 plugin_store）
                            store_bytes,
                            store_error,
                            health,
                        }
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
        // 打开着的那一页所属插件已经不在了（文件被删、热重载把它卸了）：把视图状态
        // 一起收掉。留着的话屏上画的是清单，点击却被 `on_click` 的视图分支吞掉 ——
        // 看起来像程序卡死，只有 Esc 能出来。
        if let Some(name) = self.plugin_open.clone() {
            let alive = self
                .pm
                .as_ref()
                .and_then(|pm| pm.get_plugin(&name))
                .and_then(|p| p.view.as_ref())
                .is_some();
            if !alive {
                self.plugin_open = None;
                self.reset_plugin_view_state();
            }
        }
    }

    /// 取一次攒批的按键增量，投给已加载的插件（钩子没声明的那個会在分发器里自己安静下来）。
    ///
    /// 这一跳必须留在 GUI 线程：`Lua` 不是 Send，插件的 VM 不能被别的线程碰。写线程那一侧
    /// 只把 `(键名, 这一批的增量)` 并进取件口，所以每 10 秒一批、每批每个键只进一次 VM ——
    /// 逐键进 VM 等于把"插件钩子死循环炸启动"那一族事故请回按键热路径。
    /// 到期定时器与事件回调：在这一拍跑（方案 3.6「GUI 线程 tick 到期回调」）。
    ///
    /// 放在按键攒批旁边是刻意的：两条都是"后台只投请求、进 Lua 的那一跳在 GUI 线程"，
    /// 同一拍里跑完，插件事件就不会与按键投递抢同一份 VM。
    /// 这一句只在真的有到期时出声，空表不占日志（一拍一次，一秒十条）。
    fn pump_plugin_timers(&mut self) {
        let Some(pm) = self.pm.as_mut() else {
            return;
        };
        let ran = pm.pump_timers();
        if ran.is_empty() {
            return;
        }
        let total: usize = ran.iter().map(|(_, n)| *n).sum();
        tracing_note(&format!(
            "定时器：{} 个插件跑了 {total} 格 timers_fired={total}",
            ran.len()
        ));
    }

    /// 收监听线程交来的复制件，入附属库。写库留在 UI 线程（单写者）。
    ///
    /// 通道里只有监听线程放进来的 `Captured`，一拍最多几十条；空的时候只花一次 `try_recv`。
    fn pump_clipboard(&mut self, hwnd: HWND) {
        // 关掉记录之后，通道里可能还躺着关掉前 100ms 内收到的一件：直接丢掉。
        let enabled = focusflow_core::config::instance().get_bool("clipboard", "enabled", true);
        let mut got = 0usize;
        let mut disconnected = false;
        if let Some(rx) = self.clip_rx.as_ref() {
            loop {
                match rx.try_recv() {
                    Ok(c) => {
                        if !enabled {
                            continue;
                        }
                        use focusflow_core::clipboard::{Captured, IngestOutcome};
                        let (outcome, log_line) = match &c {
                            Captured::Text {
                                text,
                                source_app,
                                copied_at_ms,
                            } => (
                                focusflow_core::clipboard::ingest(text, source_app, *copied_at_ms),
                                format!("clip_ingest len={}", text.len()),
                            ),
                            Captured::Image {
                                stored,
                                source_app,
                                copied_at_ms,
                            } => (
                                focusflow_core::clipboard::ingest_image(
                                    stored,
                                    source_app,
                                    *copied_at_ms,
                                ),
                                format!("clip_ingest_image {}x{}", stored.width, stored.height),
                            ),
                            Captured::Files {
                                paths,
                                source_app,
                                copied_at_ms,
                            } => (
                                focusflow_core::clipboard::ingest_files(
                                    paths,
                                    source_app,
                                    *copied_at_ms,
                                ),
                                format!("clip_ingest_files n={}", paths.len()),
                            ),
                        };
                        match outcome {
                            Ok(IngestOutcome::Stored { id, dedup }) => {
                                got += 1;
                                // 插件订阅（`on("clipboard")`）：只在有插件管理器时才投，
                                // 否则那口队列只会攒满再丢。投的是**文本**，与 API 同一版口径。
                                // 同一内容 1 秒内只投一次：剪贴板服务会把一次复制重复发布，
                                // 不压这一道，一个手势会变成两三次回调（实测 3 次）。
                                if self.pm.is_some() {
                                    if let Captured::Text { text, .. } = &c {
                                        let hash =
                                            focusflow_core::clipboard::sha256_hex(text.as_bytes());
                                        let now = std::time::Instant::now();
                                        if !clip_event_is_duplicate(
                                            self.clip_event_last.as_ref(),
                                            &hash,
                                            now,
                                        ) {
                                            self.clip_event_last = Some((hash, now));
                                            let kept = focusflow_core::plugins::plugin_events::post_clipboard_text(text);
                                            if !kept {
                                                tracing_note(
                                                    "剪贴板：待投插件的事件队列已满，丢了最旧的一条",
                                                );
                                            }
                                        }
                                    }
                                }
                                // 验收脚本靠这一行判"真的入库了"；只报类型/长度与条目标识，
                                // 内容一个字都不进日志。
                                tracing_note(&format!(
                                    "剪贴板收录 clip_ingest id={id} dedup={dedup} {log_line}"
                                ));
                            }
                            Ok(_) => {}
                            Err(e) => tracing_note(&format!("剪贴板入库失败：{e}")),
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        if disconnected {
            tracing_note("剪贴板监听线程已退出：本进程不再收录复制内容");
            self.clip_rx = None;
            // 故障要当场出现在屏上：弹窗开着就重载页脚，管理页开着就重画。
            clipboard_popup::notify_changed();
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
        }
        if got > 0 {
            // 弹窗开着就标脏，下一帧重查；没开时这一声只是一次原子读。
            clipboard_popup::notify_changed();
            if self.view == 7 {
                self.clip_page.reload();
                let (w, h) = self.client_dip(hwnd);
                self.clip_page
                    .clamp_selection(clipboard_page::layout_for(w, h).rows);
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
        }
    }

    fn pump_plugin_key_batches(&mut self) {
        let batches = focusflow_core::plugins::manager::take_key_batches();
        if batches.is_empty() {
            return;
        }
        let Some(pm) = self.pm.as_mut() else {
            // 管理器没建起来：放回去，别把这批数悄悄吃掉
            focusflow_core::plugins::manager::queue_key_batch(batches);
            return;
        };
        let names = pm.loaded_plugin_names();
        // 这一行只在真的有批的时候出声（一拍一次、每十秒最多一条），它把"件口有货"与
        // "有收件方"这两件事一次量清：缺了它，屏上那一栏是 0 时只能靠猜。
        // ASCII 尾巴同上：验收脚本靠 `keybatch_lines=` 匹配。
        tracing_note(&format!(
            "攒批投递：{} 条增量 × {} 个已加载插件 keybatch_lines={} plugins={}",
            batches.len(),
            names.len(),
            batches.len(),
            names.len()
        ));
        for (key, count) in &batches {
            for name in &names {
                pm.plugin_key_event(name, key, *count);
            }
        }
    }

    /// 启动即加载插件。以前管理器只在进插件页时才建，于是「本阶段键鼠」这一栏在没人
    /// 打开过插件页时永远是 0 —— 攒批的分发也就没有收件方。
    fn load_plugins_at_startup(&mut self) {
        self.ensure_plugin_manager();
        if !self.plugins_loaded {
            if let Some(pm) = self.pm.as_mut() {
                self.plugins_loaded = pm.load_all();
            }
        }
        // 这一行是给"面板那栏为什么还是 0"留的读数：攒批有没有收件方，全看这里加载了几个。
        // ASCII 尾巴 `plugins_loaded=` 是给验收脚本匹配的（PowerShell 5.1 读无 BOM 的 UTF-8
        // .ps1 当 ANSI，脚本里放中文正则会把匹配本身读坏）。
        let n = self
            .pm
            .as_ref()
            .map(|pm| pm.loaded_plugin_names().len())
            .unwrap_or(0);
        tracing_note(&format!("启动加载插件：{n} 个 plugins_loaded={n}"));
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
                // 「配置没写进去」这一档**已经按内存里那一档生效了**（停用会真的把
                // 插件卸下来），不重扫的话行上还写着改之前的状态 —— 而那正是本次修的
                // 那个症状的另一半。
                self.reload_plugins();
            }
        }
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    /// 插件授权：落一份 `grants.json`，并把这一份插件的闸**当场**翻开。
    ///
    /// 与「启用/停用」分家是有意的：那颗按钮改的是"这个插件参不参与"，这一颗改的是
    /// "它能不能用它声明的那几项能力"，而停用不动授权与数据（方案 3.1 那句"停用了数据还在"）。
    /// 落不下去必须说出来：屏上写着「已授权」而文件里没有，下一次启动又回到没授权那一档，
    /// 与 `toggle_plugin` 那条"落不了盘就不许回 Ok"是同一族缺陷。
    fn grant_plugin(&mut self, hwnd: HWND, idx: usize) {
        let Some(row) = self.plugin_rows.get(idx).cloned() else {
            return;
        };
        self.ensure_plugin_manager();
        let res = match self.pm.as_mut() {
            Some(pm) => pm.grant_plugin(&row.stem),
            None => Err("没有插件管理器".to_string()),
        };
        self.status = Some(match res {
            Ok(note) => format!("插件 {}：{note}", row.name),
            Err(e) => format!("插件授权失败：{e}"),
        });
        tracing_note(self.status.as_deref().unwrap_or(""));
        // 重扫一遍清单：这一行的「需要授权」那句要当场改掉，而不是等下一次进这一页
        self.reload_plugins();
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    /// 打开某个插件的视图：先让插件重算 get_view，再读缓存。
    /// 选一份插件包并安装。
    ///
    /// 对话框与落盘都在后台线程（与导出那一条同一套）：`rfd` 的同步对话框会自己跑一个
    /// 模态循环，摆到 GUI 线程上会让那扇 DComp 窗停在那儿等它。结果走 `ops` + `WM_OP_DONE`，
    /// 与备份/导出共用同一条回执路 —— 那边会把这一页重扫一遍。
    fn install_plugin_package(&mut self, hwnd: HWND) {
        let hwnd_val = hwnd.0 as isize;
        let ops = Arc::clone(&self.ops);
        let spawned = std::thread::Builder::new()
            .name("plugin-install".into())
            .spawn(move || {
                // 验收钩子：给了路径就不弹框（与 `FOCUSFLOW_EXPORT_PATH` 同一条约定）。
                let picked = match std::env::var("FOCUSFLOW_INSTALL_PLUGIN") {
                    Ok(p) => Some(PathBuf::from(p)),
                    Err(_) => rfd::FileDialog::new()
                        .set_title("安装插件包（.zip）")
                        .add_filter("插件包", &["zip"])
                        .pick_file(),
                };
                let line = match picked {
                    None => "安装已取消（没有选中任何包）".to_string(),
                    Some(path) => install_line(focusflow_core::plugins::install::install(&path)),
                };
                if let Ok(mut q) = ops.lock() {
                    q.push(line);
                }
                let target = HWND(hwnd_val as *mut core::ffi::c_void);
                let _ = unsafe { PostMessageW(Some(target), WM_OP_DONE, WPARAM(0), LPARAM(0)) };
            });
        if let Err(e) = spawned {
            // 线程起不来也要留一句：否则按了按钮什么都没发生，而用户不知道"什么都没发生"。
            self.status = Some(format!("安装没开始：后台线程起不来（{e}）"));
            tracing_note(self.status.as_deref().unwrap_or(""));
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
        }
    }

    /// 卸载确认框上的一次选择：`purge == true` 才连设置一起删。
    fn finish_uninstall(&mut self, hwnd: HWND, prompt: &UninstallPrompt, purge: bool) {
        let line = uninstall_line(focusflow_core::plugins::install::uninstall(
            &prompt.stem,
            purge,
        ));
        self.plugin_uninstall = None;
        // 那一格没了：详情页不能再停在那儿（它的「返回」还在，但底下那一行已经不存在）。
        self.plugin_detail_open = None;
        self.plugin_detail_source = None;
        self.plugin_detail_installed = false;
        self.status = Some(line.clone());
        // ASCII 尾巴给验收脚本读（与详情/热重载那两句同一条约定）。
        tracing_note(&format!("插件卸载：{line} uninstall_done=1"));
        self.reload_plugins();
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    /// 打开一格的详情页。
    ///
    /// **不加载插件**：详情读的是台账与清单，不是 VM 里的视图。所以这一条对停用的、
    /// 加载失败的格子同样成立 —— 恰恰是那两种格子最需要一个"它到底怎么了"的地方。
    fn open_plugin_detail(&mut self, hwnd: HWND, idx: usize) {
        let Some(stem) = self.plugin_rows.get(idx).map(|r| r.stem.clone()) else {
            return;
        };
        // 与视图互斥：屏上只有一张页，命中那一支也只认一个状态。
        self.plugin_open = None;
        self.plugin_detail_open = Some(stem.clone());
        // 读一次安装记录，两个结论一起拿：那一句话，以及"这一格有没有包可卸"。
        let meta = focusflow_core::plugins::install::meta_for(&stem);
        self.plugin_detail_installed = focusflow_core::plugins::install::is_installed(&meta);
        self.plugin_detail_source = Some(focusflow_core::plugins::install::source_line_of(&meta));
        // ASCII 尾巴给验收脚本读：主体是中文，脚本里嵌中文匹配会踩编码坑
        // （与热重载那一句同一条约定）。
        tracing_note(&format!("打开插件详情：{stem} detail_open=1"));
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    fn close_plugin_detail(&mut self, hwnd: HWND) {
        if self.plugin_detail_open.take().is_some() {
            tracing_note("关掉插件详情 detail_open=0");
        }
        self.plugin_detail_source = None;
        self.plugin_detail_installed = false;
        self.plugin_uninstall = None;
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    fn open_plugin_view(&mut self, hwnd: HWND, name: &str) {
        // 视图与详情互斥（见 `open_plugin_detail` 那一句）。
        self.plugin_detail_open = None;
        self.plugin_detail_source = None;
        self.ensure_plugin_manager();
        if let Some(pm) = self.pm.as_mut() {
            // `panel` 那一档是"打开这一页才装"（方案 3.14）：这一句就是那一步。
            // 已经活着的是空操作，所以常驻那一档不会被重复装一遍。
            if let Err(e) = pm.ensure_loaded(name) {
                self.status = Some(format!("插件 {name} 装不起来：{e}"));
                tracing_note(self.status.as_deref().unwrap_or(""));
            }
            if let Err(e) = pm.refresh_view(name) {
                self.status = Some(format!("插件 {name} 的视图取不出来：{e}"));
                tracing_note(self.status.as_deref().unwrap_or(""));
            }
        }
        self.plugin_open = Some(name.to_string());
        self.reset_plugin_view_state();
        // ASCII 尾巴给验收脚本读（与热重载那一句同一条约定）。
        tracing_note(&format!("打开插件视图：{name} view_open=1"));
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
        let edit = self.ensure_edit(hwnd);
        self.show_edit_cell(hwnd, cell);
        unsafe {
            let _ = SetWindowTextW(edit, &HSTRING::from(value));
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

    /// 打开日期选择器：`cell` 是字段框（主窗客户区 DIP），日历默认摆它下面，
    /// 下面放不下就翻到上面。
    fn open_date_picker(
        &mut self,
        hwnd: HWND,
        plugin: &str,
        field: &str,
        value: &str,
        cell: D2D_RECT_F,
    ) {
        let (year, month, selected) = dp_parse(value);
        let s = self.scale(hwnd);
        let (w, h) = dp_size();
        let dh = match self.date_pick.filter(|h| !h.0.is_null()) {
            Some(h) => h,
            None => {
                let hinst = unsafe { GetModuleHandleW(None) }
                    .map(|m| m.into())
                    .unwrap_or_default();
                unsafe {
                    let wc = WNDCLASSW {
                        style: CS_HREDRAW | CS_VREDRAW,
                        lpfnWndProc: Some(date_pick_proc),
                        hInstance: hinst,
                        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                        lpszClassName: DATE_PICK_CLASS,
                        ..Default::default()
                    };
                    let _ = RegisterClassW(&wc);
                }
                let dh = unsafe {
                    CreateWindowExW(
                        WS_EX_TOOLWINDOW,
                        DATE_PICK_CLASS,
                        w!(""),
                        WINDOW_STYLE(WS_POPUP.0 | WS_CLIPCHILDREN.0),
                        0,
                        0,
                        (w * s).round() as i32,
                        (h * s).round() as i32,
                        Some(hwnd),
                        None,
                        None,
                        None,
                    )
                }
                .unwrap_or_default();
                if dh.0.is_null() {
                    return;
                }
                self.date_pick = Some(dh);
                dh
            }
        };
        if let Ok(mut g) = DATE_PICK.lock() {
            *g = Some(DatePickState {
                owner: hwnd.0 as isize,
                plugin: plugin.to_string(),
                field: field.to_string(),
                year,
                month,
                selected,
                scale: s,
            });
        }
        let pw = (w * s).round() as i32;
        let ph = (h * s).round() as i32;
        let mut pt = POINT {
            x: (cell.left * s).round() as i32,
            y: (cell.bottom * s).round() as i32 + 2,
        };
        unsafe {
            let _ = ClientToScreen(hwnd, &mut pt);
            let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(mon, &mut mi).as_bool() {
                if pt.y + ph > mi.rcWork.bottom {
                    let mut top = POINT {
                        x: (cell.left * s).round() as i32,
                        y: (cell.top * s).round() as i32 - ph - 2,
                    };
                    let _ = ClientToScreen(hwnd, &mut top);
                    pt.y = top.y;
                }
                if pt.x + pw > mi.rcWork.right {
                    pt.x = mi.rcWork.right - pw - 8;
                }
                if pt.x < mi.rcWork.left {
                    pt.x = mi.rcWork.left + 8;
                }
            }
            let _ = SetWindowPos(dh, None, pt.x, pt.y, pw, ph, SWP_NOZORDER | SWP_SHOWWINDOW);
            let _ = InvalidateRect(Some(dh), None, true);
            let _ = SetForegroundWindow(dh);
            let _ = SetFocus(Some(dh));
        }
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
        self.pv_targets.clear();
        self.pv_targets_key = None;
        self.pv_hover = None;
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
    ///
    /// 走 `reveal::open_dir` 而不是在这里自己起 Explorer：`data_home` 是允许手写成正斜杠
    /// 形式的，而本机实测 `explorer C:/…`（含给它整串加引号那一版）开出来的是「文档」——
    /// 不报错也不提示，屏上只多一个看错地方的窗口。斜杠那把尺子 core 里已经有一份。
    fn open_data_dir(&self) {
        let dir = focusflow_core::data_location::current_data_home();
        match focusflow_core::reveal::open_dir(&dir) {
            Ok(_) => tracing_note(&format!("打开数据目录：{}", dir.display())),
            Err(e) => tracing_note(&format!("打不开数据目录：{e}")),
        }
    }

    /// 设置页里那个常驻的搜索框：整个进程只建一次。
    fn ensure_search_edit(&mut self, parent: HWND) -> HWND {
        if let Some(e) = self.search_edit {
            return e;
        }
        // 提示文案不交给控件：`EM_SETCUEBANNER` 要 comctl32 v6 的激活上下文，这个 exe
        // 没带那份 manifest —— 现量（2026-11-01）发完这条消息之后空框里什么都不画。
        // 所以提示由父窗画，但**只在控件没拿到焦点时画**：那块矩形属于子窗口，
        // 焦点一进框，父窗再写同样的字就是两块表面抢同一批像素（看得见提示、
        // 看不见光标，重绘交替时就是他报的那句"没有用户输入的字时字体闪烁"）。
        // 焦点状态由 `edit_proc` 在 WM_SETFOCUS / WM_KILLFOCUS 上递回来，
        // 靠的是这里传的子类 id（`SEARCH_EDIT_SUBCLASS`）——不是最后那个 dwRefData。
        // 复用同一个子类过程：为的是 Enter/Esc 也走 WM_EDIT_DONE 那条回执（搜索框这一路
        // `finish_edit` 看到 edit_target 为空会直接返回，不动任何一行）。
        let (host, h) = create_hosted_edit(parent, self.ui_font, SEARCH_EDIT_SUBCLASS);
        self.search_edit_host = Some(host);
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
        let Some(host) = self.search_edit_host else {
            return;
        };
        if edit.0.is_null() {
            return;
        }
        let (ex, ey, ew, eh) = search_edit_px(sb, s);
        self.place_hosted_edit(
            hwnd,
            host,
            edit,
            rect(ex as f32, ey as f32, ew as f32, eh as f32),
            self.search_focused,
        );
    }

    fn hide_search_edit(&mut self) {
        if let Some(e) = self.search_edit {
            unsafe {
                let _ = ShowWindow(e, SW_HIDE);
            }
        }
        if let Some(h) = self.search_edit_host {
            unsafe {
                let _ = ShowWindow(h, SW_HIDE);
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
        let (host, h) = create_hosted_edit(parent, self.ui_font, RENAME_EDIT_SUBCLASS);
        self.edit_host = Some(host);
        self.edit = Some(h);
        h
    }

    /// 把行内编辑摆到主窗客户区的 `cell`（DIP）上。
    ///
    /// 宿主是顶层窗 ⇒ 位置要用屏幕坐标；里面的 EDIT 缩 2 px，让宿主画的那圈描边
    /// 留在外面（EDIT 自己没边框，铺满会把整个框吃掉）。
    fn show_edit_cell(&self, parent: HWND, cell: D2D_RECT_F) {
        let (Some(host), Some(edit)) = (self.edit_host, self.edit) else {
            return;
        };
        let s = self.scale(parent);
        self.place_hosted_edit(
            parent,
            host,
            edit,
            rect(
                cell.left * s,
                cell.top * s,
                (cell.right - cell.left) * s,
                (cell.bottom - cell.top) * s,
            ),
            true,
        );
    }

    /// 宿主/EDIT 的摆位（客户区像素 → 屏幕坐标）。三个摆位点共用这一份几何。
    /// `show = false` 只挪位置不亮出来（搜索框未拿到焦点时，提示文案仍归主窗画）。
    fn place_hosted_edit(
        &self,
        parent: HWND,
        host: HWND,
        edit: HWND,
        cell: D2D_RECT_F,
        show: bool,
    ) {
        let s = self.scale(parent);
        let inset = (PV_FIELD_EDIT_INSET * s).round() as i32;
        let flags = if show {
            SWP_NOZORDER | SWP_SHOWWINDOW
        } else {
            SWP_NOZORDER
        };
        let w = (cell.right - cell.left).round() as i32;
        let h = (cell.bottom - cell.top).round() as i32;
        let mut pt = POINT {
            x: cell.left.round() as i32,
            y: cell.top.round() as i32,
        };
        unsafe {
            let _ = ClientToScreen(parent, &mut pt);
            let _ = SetWindowPos(host, None, pt.x, pt.y, w, h, flags);
            let _ = SetWindowPos(
                edit,
                None,
                inset,
                inset,
                (w - inset * 2).max(8),
                (h - inset * 2).max(8),
                flags,
            );
            if show {
                // 宿主是被拥有的顶层弹窗：光 `SetFocus` 不行 —— 焦点只能落到
                // **活动窗口**或它的子窗口上，而不成对地激活宿主时主窗仍是活动窗口，
                // 键盘于是全打到主窗（BUG 表现：字打进去了、EDIT 里其实是空的）。
                let _ = SetForegroundWindow(host);
            }
        }
    }

    fn hide_edit_cell(&self) {
        if let Some(edit) = self.edit {
            unsafe {
                let _ = ShowWindow(edit, SW_HIDE);
            }
        }
        if let Some(host) = self.edit_host {
            // 宿主拿着前台：隐藏它之前把前台还给主窗，否则这一次提交之后
            // 焦点会落到别的应用上（屏上像是"程序自己松开了"）。
            let owner = unsafe { GetParent(host) }.unwrap_or_default();
            unsafe {
                let _ = ShowWindow(host, SW_HIDE);
            }
            if !owner.0.is_null() {
                unsafe {
                    let _ = SetForegroundWindow(owner);
                }
            }
        }
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
        let g = table_geom(left_card(w, h));
        let vis = row.saturating_sub(self.scroll);
        if vis >= g.max_rows {
            return;
        }
        let y = g.list_top + vis as f32 * g.row_h;
        let cell = rect(
            g.x_name - 4.0,
            y + 1.0,
            device_name_width(&g) + 4.0,
            g.row_h - 3.0,
        );
        self.show_edit_cell(hwnd, cell);
        unsafe {
            let _ = SetWindowTextW(edit, &HSTRING::from(dev.name.clone()));
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
        let g = settings_geom(w, h);
        // 屏幕行号用列表下标（含分组标题），不是 `settings` 数组下标。
        let vis = item_idx.saturating_sub(self.scroll);
        if vis >= g.max_rows {
            return;
        }
        let edit = self.ensure_edit(hwnd);
        let y = g.list_top + vis as f32 * g.row_h;
        let cell = rect(g.value_x - 4.0, y + 8.0, g.value_w + 8.0, 28.0);
        self.show_edit_cell(hwnd, cell);
        unsafe {
            let _ = SetWindowTextW(edit, &HSTRING::from(value));
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
            // 当场落盘并问一次结局：`config::set` 恒回 `Ok(())`（真正写盘的是另一头的去抖
            // 线程），拿它的 `Err` 分支当"没写进去"永远是死路 —— 屏上读的是刚改的内存值，
            // `config.ini` 里还是老一档，两边分家要到下一次开机才露出来。
            let note = config_write_note(
                &format!("{section}.{key}"),
                focusflow_core::config::instance().set_now(section, key, next),
            );
            // 没落成也照样刷：内存里的值确实变了，这一格当场要显示新状态，
            // 而"只到本次运行"由那句 note 说清楚。
            self.refresh_now();
            let mut msg = format!("设置已切换 {section}.{key}={next}{note}");
            if section == "clipboard" && key == "enabled" {
                // 剪贴板开关会改变"第 4 条热键要不要占用全局键"以及托盘那颗勾。
                self.sync_clipboard_item();
                msg.push_str(&format!("；{}", self.reload_hotkeys()));
                clipboard_popup::notify_changed();
            }
            self.status = Some(msg.clone());
            tracing_note(&msg);
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
            // 先藏起来（连同顶层宿主）：藏窗口会触发 WM_KILLFOCUS，它又会投一次"提交"。
            // 而 `edit_target` 已经取空，把这一次挡掉。
            self.hide_edit_cell();
            if commit {
                // 长度现问：排除清单这类长设置超过 255 字时，固定缓冲会**静默截断**，
                // 用户保存的清单后一半根本没写进 config。
                let len = unsafe { GetWindowTextLengthW(e) }.max(0) as usize;
                let mut buf = vec![0u16; len + 1];
                let n = (unsafe { GetWindowTextW(e, &mut buf) }.max(0) as usize).min(len);
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
                                // 与 `toggle_setting` 同一把尺子：当场落盘、当场问结局。
                                // 这一支从前也写着 `Err` 分支，而 `config::set` 恒回 `Ok`，
                                // 那句"设置写入失败"永远走不到；热键那几行还会顺手把
                                // `reload_hotkeys` 的话盖在状态行上，看着像已经记住了。
                                let note = config_write_note(
                                    &format!("{section}.{key}"),
                                    focusflow_core::config::instance().set_now(section, key, &v),
                                );
                                if section == "hotkey" {
                                    let mut msg = self.reload_hotkeys();
                                    msg.push_str(&note);
                                    self.status = Some(msg);
                                } else {
                                    self.status =
                                        Some(format!("设置已写入 {section}.{key}={v:?}{note}"));
                                }
                                self.refresh_now();
                                if let Some(s) = &self.status {
                                    tracing_note(s);
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
                    EditTarget::SnippetName(id) => {
                        match focusflow_core::clipboard::rename_snippet(id, &text) {
                            Ok(true) => {
                                self.clip_page.reload();
                                self.clip_page.status = Some("片段已重命名".to_string());
                            }
                            Ok(false) => {
                                self.clip_page.status = Some("这个片段已经不在了".to_string())
                            }
                            Err(err) => self.clip_page.status = Some(format!("重命名失败：{err}")),
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
        // 这条查询在 UI 线程上跑，2026-10-30 按现量收口（旧注释写的是"正式版要挪到后台线程"，
        // 一句没量过的许愿）：他真实数据的副本上最贵的一档 1.8ms；`perf_probe` 那组合成数据
        // （比他真实库更密：365 天 × 150 键/天 × 5 设备 × 40 键/天）里
        // 「总计」32.8ms、「近 30 天」6.4ms、「今日」4.6ms。
        // 参照线是 >16ms 掉一帧、>100ms 可感知 ⇒ 眼下这一档只是点下去少几帧，
        // 真到要挪的那天得由 `perf_probe` 的数说话，不凭"看起来在 UI 线程上"动手。
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
    /// 量文字宽度用（页签下划线跟着文字走，见 `measure_text`）。
    dw: &'a IDWriteFactory,
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
        // 四档圆角是"观感档位"，不是"像素值"：8px 的圆点上 R_CONTROL 会变成方块套圆角，
        // 所以按短边的一半夹一刀 —— 小元素自动变成胶囊/圆点，调用点不用再单独挑档。
        let radius = radius
            .min((r.right - r.left).min(r.bottom - r.top) / 2.0)
            .max(0.0);
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

    /// 卡片落影：按 [`SHADOW_RINGS`] 从外到内叠 6 圈，每圈半径跟着外扩量一起长，
    /// 这样四角仍是同心弧、不会露出直角。
    ///
    /// **半透明卡片下面不画阴影**：玻璃档卡片 alpha < 1，阴影会从卡片里透出来，
    /// 整张卡看着像蒙了一层灰。深色主题 `shadow.a == 0`，同样不画。
    fn shadow(&self, r: D2D_RECT_F, radius: f32, strong: bool, dy: f32) {
        if self.pal.card.a < 0.99 || self.pal.shadow.a <= 0.0 {
            return;
        }
        let brushes = if strong {
            &self.br.shadow_strong
        } else {
            &self.br.shadow
        };
        for (i, (spread, _)) in SHADOW_RINGS.iter().enumerate().rev() {
            let Some(b) = brushes.get(i) else { continue };
            let rr = rect(
                r.left - spread,
                r.top - spread + dy,
                (r.right - r.left) + spread * 2.0,
                (r.bottom - r.top) + spread * 2.0,
            );
            self.round(rr, radius + spread, b, None);
        }
    }

    /// 卡片顶边那条 1px 高光：深色/玻璃档没有阴影，靠它把卡片从页面里"抬"起来。
    fn top_highlight(&self, r: D2D_RECT_F, radius: f32) {
        if self.pal.highlight.a <= 0.0 {
            return;
        }
        self.line(
            r.left + radius,
            r.top + 0.5,
            r.right - radius,
            r.top + 0.5,
            &self.br.highlight,
            1.0,
        );
    }

    /// 普通卡片：落影 + 卡底色 + 1px 边框 + 顶部高光。
    fn card(&self, r: D2D_RECT_F, radius: f32) {
        self.shadow(r, radius, false, 1.5);
        self.round(r, radius, &self.br.card, Some(&self.br.border));
        self.top_highlight(r, radius);
    }

    /// 浮层（弹窗 / 下拉）：阴影更重、底色用**不透明**的 `modal` —— 玻璃档拿半透明
    /// `card` 当弹窗底，下面的表格字会透上来。
    fn overlay_card(&self, r: D2D_RECT_F, radius: f32) {
        self.shadow(r, radius, true, 6.0);
        self.round(r, radius, &self.br.modal, Some(&self.br.border));
        self.top_highlight(r, radius);
    }

    /// 某串文字在给定档位下的宽度（DIP）。`P::text` 走的是 DrawText，拿不到度量，
    /// 所以单独建一次 layout —— 只在页签下划线那处用，一次重画最多问一次。
    fn measure(&self, s: &str, f: &IDWriteTextFormat) -> Option<f32> {
        measure_text(self.dw, s, f)
    }

    /// 把 `text` 夹到 `rect_w` 这一格真能画下的长度，量的是画它的那档字体 `f` 本身。
    /// 判据与退回档见 [`clip_to_cell`]。
    fn clip(&self, text: &str, f: &IDWriteTextFormat, rect_w: f32) -> String {
        clip_to_cell(self.dw, text, f, rect_w)
    }

    /// 折线下面的淡色面积：一条 2px 线孤零零挂在网格上，图卡看着就是个空框。
    /// 建几何失败就跳过（与 `P::brush` 同一策略：画不出就不画，不 panic）。
    fn area(&self, pts: &[Vector2], plot: D2D_RECT_F) {
        if pts.len() < 2 {
            return;
        }
        let Ok(factory) = (unsafe { self.rt.GetFactory() }) else {
            return;
        };
        let Ok(path) = (unsafe { factory.CreatePathGeometry() }) else {
            return;
        };
        let Ok(sink) = (unsafe { path.Open() }) else {
            return;
        };
        let last = pts[pts.len() - 1];
        unsafe {
            sink.BeginFigure(pts[0], D2D1_FIGURE_BEGIN_FILLED);
            for pt in &pts[1..] {
                sink.AddLine(*pt);
            }
            sink.AddLine(Vector2 {
                X: last.X,
                Y: plot.bottom,
            });
            sink.AddLine(Vector2 {
                X: pts[0].X,
                Y: plot.bottom,
            });
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            let _ = sink.Close();
        }
        if let Some(b) = self.brush(with_alpha(self.pal.accent, 0.10)) {
            unsafe {
                self.rt.FillGeometry(&path, &b, None);
            }
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
    ("高级", "键鼠、界面、剪贴板、应用与设备、调试"),
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
        "hotkey" | "snip" => 1,
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
            let (label, desc, _) = ACTIONS[i];
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

fn settings_geom(w: f32, h: f32) -> SettingsGeom {
    let card_top = 208.0;
    let list_top = card_top + 52.0;
    // 高级项两行：中文名 + 一句话说明；基础行/操作也跟着这个行高，整页不再挤成一列键名。
    let row_h = 44.0;
    let max_rows = (((h - 32.0 - 30.0 - list_top) / row_h).floor() as usize).max(1);
    // 值列右锚：宽窗时贴着卡片右边，不在中间留一大块死白；窗口再窄也先保住
    // 说明那一列最少 120 px 的宽度（真实下限 960 远在这条线右边）。
    let value_w = 150.0;
    let value_x = (w - 32.0 - value_w - 24.0).max(240.0 + value_w);
    SettingsGeom {
        list_top,
        row_h,
        max_rows,
        value_x,
        value_w,
        nav_x: 28.0,
        nav_y: list_top,
        nav_w: 190.0,
        cat_h: 44.0,
        body_left: 240.0,
    }
}

/// 设置页里一行可画 / 可悬停 / 可点的矩形（三个读者共用这一份）。
///
/// 悬停从前只判 `y`：指针压在左边的分类导航上、或落在右侧卡片的边距里，正文那一行
/// 也会亮起来 —— 亮的区域与点得到的区域不是同一个。宽度与画那一行读同一个表达式。
fn settings_row_rect(g: &SettingsGeom, w: f32, y: f32) -> D2D_RECT_F {
    rect(g.body_left, y, w - 32.0 - g.body_left, g.row_h - 2.0)
}

/// 设置页里指针落在哪一行（返回 `(items 下标, 屏上第几行)`）。分组标题只画不点，不算控件。
///
/// 悬停、手型、点击三个读者都走这里：从前三处各写一遍循环，行高一改就有人漏改，
/// 症状是"亮的是一行、点下去改的是另一行"。
fn settings_row_at(
    items: &[SItem],
    g: &SettingsGeom,
    w: f32,
    scroll: usize,
    x: f32,
    y: f32,
) -> Option<(usize, usize)> {
    let scroll = scroll.min(items.len().saturating_sub(g.max_rows));
    for vis in 0..g.max_rows {
        let idx = scroll + vis;
        let Some(item) = items.get(idx) else {
            break;
        };
        if matches!(item, SItem::Header(_)) {
            continue;
        }
        let y0 = g.list_top + vis as f32 * g.row_h;
        if hit(settings_row_rect(g, w, y0), x, y) {
            return Some((idx, vis));
        }
    }
    None
}

/// 设置行里说明文字那一列的宽度：从行内左边距（`r.left + 12`）画到值框/按钮之前，留 18 px。
///
/// 几行"基础项"从前各自写死 560 / 640：而这一列真正的空位只有 `720 - 240 - 12 - 18 ≈ 450`，
/// 于是 `data_home` 这种一条真实路径（~490 px）会整句画到右边「打开」按钮身上；写死宽度的
/// 那一侧又让 `clip_middle` 把长路径的尾巴当废物省掉 —— 屏上既看不到真正的家，也分不清字
/// 压在哪颗按钮上。高级项那一列早就是这么算的，这里收成同一把尺子。
fn settings_note_w(g: &SettingsGeom, r: D2D_RECT_F) -> f32 {
    (g.value_x - r.left - 30.0).max(120.0)
}

/// 设置页底部的数据操作（后台线程跑，结果回投 `WM_OP_DONE`）。
/// 设置页底部那八颗「执行」：标题 / 说明 / **点下去之后状态行那句"正在…"**。
///
/// 第三栏必须住在这张表里：从前它只有两栏，`run_action` 自己再 match 一遍 `which`，
/// 而那 match 只写了 `0`/`1` 两支、其余全落进 `_ => "正在清理 30 天前的数据…"`。
/// 于是点「导出 CSV」的人屏上看到的是"正在删 30 天以前的明细"——那正是这张表里唯一
/// 标着「不可撤销」那一档才会说的话；同时三颗钮被 `busy` 灰掉、存盘对话框还开在后台线程，
/// 人就在这时候强关软件。加一栏 ⇒ 少写一栏编译就不过，话不会再借到别的动作上。
const ACTIONS: [(&str, &str, &str); 8] = [
    ("立即备份", "把年度库各快照一份到 backup/", "正在备份…"),
    ("压缩数据库", "重建年度库，回收空闲页", "正在压缩数据库…"),
    (
        "清理 30 天前",
        "删掉 30 天以前的明细（不可撤销）",
        "正在清理 30 天前的数据…",
    ),
    ("导出 CSV", "把当前周期的数据写成表格", "正在导出 CSV…"),
    (
        "导出 HTML",
        "写成一份可直接打开的网页报告",
        "正在导出 HTML…",
    ),
    ("导出 JSON", "写成结构化数据（脚本可读）", "正在导出 JSON…"),
    (
        "导入旧数据…",
        "从旧版数据目录合并进来（先留档再覆盖）",
        "正在导入旧数据…",
    ),
    (
        "更改数据文件夹…",
        "把 data/ 与 backup/ 搬到新目录；重启后执行",
        "正在安排更换数据文件夹…",
    ),
];

/// 「清理 30 天前」在 `ACTIONS` 里的下标：唯一不可撤销的一颗，点击要先问一句。
const CLEANUP_ACTION: usize = 2;

/// 这一颗「执行」交回状态行的那句清理结论 = core 那一句话 + 执行前快照没兜住的那半句。
///
/// `snapshot_warning` 这一格在 core 里刻意**不进 `incomplete()`**（操作本身做完了，退码不该
/// 因为"退路没备好"而变），它的注释还写着"绝不能只进日志"。CLI 那两头都单独把它喊出去
/// （`crates/cli/src/main.rs` 里读 `report.snapshot_warning` 的两处），而屏上这一支从前只念
/// `cleanup_line` ⇒ 刚删掉的明细**没有回滚用的快照**这件事在界面上一字不留 ——
/// 而「不可撤销」正是这颗钮自己说明栏里写着的那两个字。
fn cleanup_status_line(
    days: i64,
    report: &focusflow_core::db::maintenance::MaintenanceReport,
) -> String {
    let mut line = focusflow_core::db::maintenance::cleanup_line(days, report);
    if let Some(w) = report.snapshot_warning.as_deref() {
        line.push('；');
        line.push_str(w);
    }
    line
}

/// 这一颗钮按下去，状态行该说的那一句。越界一支不冒充任何具体动作
/// （它只可能来自新加的调用方，而「正在清理」那种话借不得）。
fn action_busy_line(which: usize) -> &'static str {
    match ACTIONS.get(which) {
        Some((_, _, busy)) => busy,
        None => "正在执行…",
    }
}

/// 搜索框那块底框（真正的输入是叠在它上面的原生 EDIT，见 `ensure_search_edit`）。
fn search_box_rect(w: f32) -> D2D_RECT_F {
    rect(w - 16.0 - 16.0 - 260.0, 218.0, 260.0, 26.0)
}

/// 搜索框的提示文案。由父窗画，但只在控件**没有**焦点时画（见 `draw_settings` 那一处与
/// `edit_proc` 递回来的焦点位）：那块矩形属于子窗口，两块表面同时往上写就是闪烁。
const SEARCH_CUE: &str = "搜索设置（名称或段.键）…";

/// 叠在底框里的原生 EDIT 的位置与尺寸（物理像素）：底框内缩 8/3，两边各再留 8、上下各留 3。
///
/// 抽成独立函数是为了让"输入框必须跟着自己那块底框走"这条能被用例钉住 —— 底框是按
/// **客户区宽度右边**定位的，窗口一改尺寸它就挪，输入框不跟着重算就与底框分家
/// （屏上剩一个点不动的空框，占位文案还在原地跟着每一拍重绘闪）。
fn search_edit_px(sb: D2D_RECT_F, s: f32) -> (i32, i32, i32, i32) {
    (
        ((sb.left + 8.0) * s) as i32,
        ((sb.top + 3.0) * s) as i32,
        ((sb.right - sb.left - 16.0) * s) as i32,
        ((sb.bottom - sb.top - 6.0) * s) as i32,
    )
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
    p.card(card, R_CARD);
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

    // 搜索框：底框由这里画，输入由叠在上面的原生 EDIT 负责。提示文案只在**框里没字、
    // 且控件没拿到焦点**时写：那块矩形归子窗口，焦点一进框，父窗再往上写字就是两块表面
    // 抢同一批像素 —— 看得见提示、看不见光标，重绘交替时正是"没有用户输入的字时字体闪烁"
    // 那条报告（2026-11-01 现量：空框连拍 200 帧只有一种画面，打了字之后同一处出现两种，
    // 说明有焦点时那一层该让给控件）。
    let sbox = search_box_rect(w);
    p.round(sbox, R_CONTROL, &p.br.header, Some(&p.br.border));
    if filter.is_empty() && !vs.search_focused {
        p.text(
            SEARCH_CUE,
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
            p.round(nr, R_CONTROL, &p.br.accent_soft, None);
        } else if vs.ctl == Some(Ctl::SetNav(i as u8)) {
            p.round(nr, R_CONTROL, &p.br.row_hover, None);
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
                &p.fmt.nav_item
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
            &p.fmt.caption,
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

    if count == 0 {
        // 搜索没有命中时从前是一块空白：看不出"没匹配"还是"这一页坏了"。
        let x = g.body_left + 12.0;
        let width = (w - 32.0 - x - 12.0).max(120.0);
        let msg = if filter.trim().is_empty() {
            "这一分类没有可显示的设置项".to_string()
        } else {
            format!("没有匹配「{}」的设置项", filter.trim())
        };
        p.text(
            &p.clip(&msg, &p.fmt.text13_em, width),
            rect(x, g.list_top + 8.0, width, 20.0),
            &p.fmt.text13_em,
            &p.br.muted,
        );
        p.text(
            &p.clip("搜索的是名称、说明、配置键与当前值", &p.fmt.text12, width),
            rect(x, g.list_top + 32.0, width, 18.0),
            &p.fmt.text12,
            &p.br.muted,
        );
    }

    for vis in 0..g.max_rows {
        let idx = scroll + vis;
        if idx >= count {
            break;
        }
        let y = g.list_top + vis as f32 * g.row_h;
        let r = settings_row_rect(&g, w, y);
        let note_w = settings_note_w(&g, r);
        if vs.sel == Some(idx) {
            p.round(r, R_CONTROL, &p.br.accent_soft, None);
        } else if vs.hover == Some(idx) {
            p.fill(r, &p.br.row_hover);
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
                    rect(r.left + 12.0, y + 23.0, note_w, 17.0),
                    &p.fmt.text12,
                    &p.br.muted,
                );

                let vb = rect(g.value_x, y + 9.0, g.value_w, 26.0);
                if row.kind == 0 {
                    let on = row.value == "true";
                    p.round(
                        vb,
                        R_CONTROL,
                        if on { &p.br.success } else { &p.br.grid },
                        None,
                    );
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
                        R_CONTROL,
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
                        // 值框是定宽（150 DIP 再减按钮那一颗），而里面吃的是 config.ini 里
                        // 那一行的原文：`[app_stats] exclude` 就是"进程名逗号分隔"，五个进程
                        // 名 ~380 px 压进 ~96-134 px 的格子，越出去的字直接盖到右边那句说明上。
                        let val_w = vb.right - vb.left - if btn.is_some() { 54.0 } else { 16.0 };
                        p.text(
                            &p.clip(shown, &p.fmt.text13, val_w),
                            rect(vb.left + 8.0, vb.top, val_w, vb.bottom - vb.top),
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
                            R_MICRO,
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
                        &p.fmt.caption,
                        &p.br.danger,
                    );
                } else if row.live != "改完立即生效" {
                    // 只有"要重启/下次隐藏"这种例外才写在行上；绝大多数立即生效，不再每行重复。
                    p.text(
                        row.live,
                        rect(vb.right + 14.0, y + 13.0, 220.0, 16.0),
                        &p.fmt.caption,
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
                        p.round(
                            vb,
                            R_CONTROL,
                            if on { &p.br.success } else { &p.br.grid },
                            None,
                        );
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
                        p.round(
                            vb,
                            R_CONTROL,
                            if on { &p.br.success } else { &p.br.grid },
                            None,
                        );
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
                        p.round(vb, R_CONTROL, bg, None);
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
                        p.round(
                            vb,
                            R_CONTROL,
                            if on { &p.br.success } else { &p.br.grid },
                            None,
                        );
                        p.text(
                            if on { "开" } else { "关" },
                            vb,
                            &p.fmt.center13,
                            if on { &p.br.white } else { &p.br.muted },
                        );
                        // 三个组合名不在这儿再摆一遍：下面三行就是它们，字号变大后这条
                        // 会顶到右边那颗「开」按钮上。
                        p.text(
                            "总开关：开 = 注册下面三行；关 = 一条都不占用",
                            rect(r.left + 12.0, y + 23.0, note_w, 17.0),
                            &p.fmt.text12,
                            &p.br.muted,
                        );
                    }
                    Basic::Snip => {
                        p.round(vb, R_CONTROL, &p.br.accent, None);
                        p.text("进标注", vb, &p.fmt.center13, &p.br.white);
                        let annotate = snap_setting(snap, "hotkey", "snip_annotate");
                        p.text(
                            // 组合键是 config.ini 里那一行的原文（手改成什么样都收），
                            // 而这一句的尾巴"框好之后 Enter 或 Ctrl+S 出图"才是要点。
                            &p.clip(
                                &format!("等于按热键 {annotate}；框好之后 Enter 或 Ctrl+S 出图"),
                                &p.fmt.text12,
                                note_w,
                            ),
                            rect(r.left + 12.0, y + 23.0, note_w, 17.0),
                            &p.fmt.text12,
                            &p.br.muted,
                        );
                    }
                    Basic::SnipQuick => {
                        p.round(vb, R_CONTROL, &p.br.accent, None);
                        p.text("松手即存", vb, &p.fmt.center13, &p.br.white);
                        let snip = snap_setting(snap, "hotkey", "snip");
                        p.text(
                            &p.clip(
                                &format!(
                                    "等于按热键 {snip}；PNG 存 data/screenshots，同时复制到剪贴板"
                                ),
                                &p.fmt.text12,
                                note_w,
                            ),
                            rect(r.left + 12.0, y + 23.0, note_w, 17.0),
                            &p.fmt.text12,
                            &p.br.muted,
                        );
                    }
                    Basic::ClosePins => {
                        let n = vs.pin_count;
                        p.round(
                            vb,
                            R_CONTROL,
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
                            &p.clip(
                                &format!(
                                    "现在开着 {n} 张；拖动挪位、滚轮缩放、Esc/右键关单张（不另存）"
                                ),
                                &p.fmt.text12,
                                note_w,
                            ),
                            rect(r.left + 12.0, y + 23.0, note_w, 17.0),
                            &p.fmt.text12,
                            &p.br.muted,
                        );
                    }
                    Basic::Floating => {
                        let on = vs.floating_on;
                        p.round(
                            vb,
                            R_CONTROL,
                            if on { &p.br.success } else { &p.br.grid },
                            None,
                        );
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
                            R_CONTROL,
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
                        p.round(vb, R_CONTROL, &p.br.accent, None);
                        p.text("生成", vb, &p.fmt.center13, &p.br.white);
                        p.text(
                            "汇总上周一~周日写到 data/reports/weekly-YYYY-MM-DD.md，缺了启动时补",
                            rect(r.left + 12.0, y + 23.0, 600.0, 17.0),
                            &p.fmt.text12,
                            &p.br.muted,
                        );
                    }
                    Basic::DataDir => {
                        p.round(vb, R_CONTROL, &p.br.header, Some(&p.br.border));
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
                            &p.clip(&dir_line, &p.fmt.text12, note_w),
                            rect(r.left + 12.0, y + 23.0, note_w, 17.0),
                            &p.fmt.text12,
                            &p.br.muted,
                        );
                    }
                }
            }
            SItem::Action(a) => {
                // 数据操作行：左边是名字，右边一颗「执行」按钮，再右边一句后果说明。
                let (label, desc, _) = ACTIONS[a];
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
        // 预算按**这一格有多宽**算：搬迁没成那一句里有两个完整路径，实测能一路画到窗口
        // 外面（DrawText 不按矩形裁切），而句尾恰恰是"搬到哪"这件事本身。
        let status_w = card.right - card.left - 32.0;
        // "要人处理"那一句除了换颜色，还加一档字重：底部这一行平时是背景信息，
        // 红字加粗才是"现在就得看一眼"。夹取问的也是这一档自己：加粗比常规宽，
        // 按常规档量会放行屏上真装不下的字。
        let status_fmt = if warn {
            &p.fmt.text12_em
        } else {
            &p.fmt.text12
        };
        let line = p.clip(line, status_fmt, status_w);
        p.text(
            &line,
            rect(card.left + 16.0, card.bottom - 26.0, status_w, 18.0),
            status_fmt,
            if warn { &p.br.danger } else { &p.br.muted },
        );
    }

    draw_vscroll(
        p,
        rect(
            card.right - 11.0,
            g.list_top,
            6.0,
            g.max_rows as f32 * g.row_h,
        ),
        count,
        g.max_rows,
        scroll,
    );
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

/// 「日均」那张卡下面的一行：说清这一格**除过去的是哪个天数**。
///
/// 从前这里无条件写「按周期窗口」，而「总计」那一档除的是全历史里记到过次数的天数
/// （见 `data::avg_divisor`，data.rs 那段注释自己就点了"卡片下面那句写的是按周期窗口"）。
/// 数额是修对过的，话没说对着 —— 屏上那句话正是用户判断"这个日均算的是什么"的唯一依据。
/// 分母与这句话读同一个 `snap.avg_days`，两处不许各写各的。
fn avg_sub(snap: &Snapshot) -> String {
    let days = snap.avg_days.max(1);
    if snap.period == 0 {
        format!("按全历史 {days} 天")
    } else {
        format!("按窗口 {days} 天")
    }
}

/// 打卡条那七个点的达标态：**只有末格（今天）改成屏上那份权威今日读数**，其余六格照旧吃
/// `goal_status` 算出来的（它们讲的是已经落库的日子，本来就该读库）。
///
/// 为什么要改这一格：`g.days` 末位是"库值 + 未落库增量"，而 `flush_pending` 把一批增量从
/// 内存取走、还没写进 SQLite 的那一瞬，两边都没有它 —— `collect::today_total` 的注释就记着
/// 这个空窗（实测 `today = 5 → 0 → 5`）。进度条与「今日 x / 目标 y」读的是那条权威值，
/// 于是同一页出现**绿条配灰点**：屏上自己跟自己对着说"今天到底达没达标"。
///
/// 改之前先确认末格真是今天：快照比屏幕慢一拍、正好跨过午夜时末格还是昨天，那就一个字都不改
/// （否则午夜后那几秒，昨天那一格会被今天的读数点亮）。
fn goal_dot_states(
    g: &focusflow_core::stats::GoalStatus,
    today_str: &str,
    today_met: bool,
) -> Vec<bool> {
    let mut out: Vec<bool> = g.days.iter().map(|(_, _, m)| *m).collect();
    if let (Some((key, _, _)), Some(slot)) = (g.days.last(), out.last_mut()) {
        if key == today_str {
            *slot = today_met;
        }
    }
    out
}

/// 排行榜/应用/设备三屏空榜时的那句话。
///
/// 三句从前都把窗口写死成「今日」，而这三屏读的是 `window_days(period)` 那一段
/// （「总计」那一档是**全历史**）：切到近 30 天看到空榜，屏上说"今日还没有记录"，
/// 用户就去翻今天的数据 —— 与它无关。措辞跟着 `period_text` 走，那一份就是榜与卡片
/// 共用的周期说法（导出行、设备详情弹窗也念它）。`pending` 那一臂讲的是"还没落库的
/// 今日增量"，与窗口无关，所以只有它保留「今日」的含义。
///
/// `gaps` 那一支排在最前：这一栏有年份读不出来时，空榜**不是**"没有记录"——
/// 卡片与悬浮窗早就不把读失败折成零，榜这里同样不许折成"空"。
fn empty_board_line(
    tab: usize,
    window: &str,
    pending: i64,
    flush_secs: i64,
    gaps: &[String],
    lane: &str,
) -> String {
    let years = gap_years(gaps, lane);
    if !years.is_empty() {
        return format!("{window}的数据没读全（{years}没读全）——不一定是没有记录");
    }
    match tab {
        1 => format!("{window}还没有应用时长（[app_stats] enabled 关着？）"),
        2 => format!("{window}还没有设备计数（[device_stats] enabled 关着？）"),
        _ if pending > 0 => format!(
            "已记录 {} 条，还没落库（写线程每 {} 秒写一次，落库后就出现在这里）",
            thousands(pending as u64),
            flush_secs.max(1)
        ),
        _ => format!("{window}还没有记录（数据目录见左下角）"),
    }
}

/// 「今日活跃」那张卡下面的一行：念的是这一轮里出现过的键名**种数**。
///
/// 为什么单独立一个函数：这里从前用的是榜的长度，而榜只为面板列行截到上限
/// （data.rs 那个 RANK_DISPLAY_MAX）—— 键名一多，屏上那句就永远念成上限那个数，
/// 而 CLI 同一件事念的是未截断的种数，两边对着骂架。两份读数的出处见 data.rs 的 rank_split。
fn kinds_sub(snap: &Snapshot) -> String {
    format!("本周期 {} 个键位", snap.rank_kinds)
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
    p.overlay_card(dialog, R_MODAL);

    let x = dialog.left + 20.0;
    p.text(
        "设备详情",
        rect(x, dialog.top + 14.0, 160.0, 22.0),
        &p.fmt.body,
        &p.br.fg,
    );
    p.round(close, R_CONTROL, &p.br.header, None);
    p.text("✕", close, &p.fmt.center_small, &p.br.muted);

    // 弹窗里名字与副行共用同一个宽度：别名可到 24 个汉字，兜底名截到 40 字符，
    // 而"原名"那一行是把两者拼在一起说的 —— 不夹就越出卡片边、画到那层深色遮罩上。
    let detail_w = dialog.right - x - 20.0;
    p.text(
        &p.clip(&d.name, &p.fmt.body, detail_w),
        rect(x, dialog.top + 40.0, detail_w, 20.0),
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
        &p.clip(&sub, &p.fmt.caption, detail_w),
        rect(x, dialog.top + 62.0, detail_w, 18.0),
        &p.fmt.caption,
        &p.br.muted,
    );

    let col2 = x + (dialog.right - dialog.left) * 0.52;
    let row = |label: &str, value: &str, cx: f32, yy: f32| {
        // 标签格 76 px、值格吃到弹窗右边减 100 —— 两格只差 6 px，标签一长就压住值。
        p.text(
            &p.clip(label, &p.fmt.caption, 76.0),
            rect(cx, yy, 76.0, 18.0),
            &p.fmt.caption,
            &p.br.muted,
        );
        p.text(
            &p.clip(value, &p.fmt.body, dialog.right - cx - 100.0),
            rect(cx + 82.0, yy, dialog.right - cx - 100.0, 18.0),
            &p.fmt.body,
            &p.br.fg,
        );
    };

    let y0 = dialog.top + 96.0;
    let step = 26.0;
    // 分母是 core 在这一趟一起交回的设备侧周期总数（`period_total`），与分子 `period_count`
    // 同一把尺子 —— 它不是屏上那张卡片用的键鼠总数，两回事（见 `data::Snapshot::device_total`
    // 头上那句为什么）。
    let share = share_text(d.period_count, d.period_total);
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
    row("占比", &share, col2, y0 + step);
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
        &p.fmt.caption,
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
            p.round(rect(bx, plot.bottom - hh, bw, hh), R_MICRO, &b, None);
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
            &p.fmt.caption_right,
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
    /// 指针下的控件（悬停底色读它；与 `App::ctl` 同一份）。
    ctl: Option<Ctl>,
    /// 插件视图里指针压着的那一格（行 / 控件）。
    pv_hover: Option<PvTarget>,
    /// 键盘焦点要画的框（None = 没有焦点或这一帧没算）。
    kbd_focus: Option<D2D_RECT_F>,
    /// 指针位置（DIP）与"这一拍该不该出 tooltip"。
    ptr: (f32, f32),
    tooltip: bool,
    sel: Option<usize>,
    editing: Option<usize>,
    /// 按下录入开着的那条热键（`settings` 下标）；那一行的值框要改成"请按下组合…"。
    capturing: Option<usize>,
    heat_hover: Option<usize>,
    chart_hover: Option<usize>,
    /// 全局热键此刻是否真的注册着（不是配置键，是运行时状态）。
    hotkey_on: bool,
    /// 三条绑定各自注册上了没有，顺序同 `COMBO_KEYS`（0 唤出 / 1 松手即存 / 2 进标注 / 3 剪贴板）。
    hotkey_live: [bool; 4],
    /// 悬浮窗此刻是否真的显示着（同上：读运行时状态，不读配置键 ——
    /// 手改 `[floating] enabled` 并不会让卡片出现或消失）。
    floating_on: bool,
    /// 桌面上现在开着几张贴图（设置页那一行显示的就是这个数）。
    pin_count: usize,
    /// 当前主题是不是深色（设置页那一行显示"深色/浅色"）。
    dark: bool,
    /// 窗口现在是不是最大化（右上角那颗按钮画"最大化"还是"还原"）。
    zoomed: bool,
    /// 设置页那个搜索框（原生 EDIT）此刻有没有焦点。父窗只在它**没有**焦点时往框里写提示
    /// 文案 —— 有焦点时那块矩形归控件画，两边同时写就是"字体闪烁"（见 `SEARCH_CUE`）。
    search_focused: bool,
    /// 剪贴板管理页的搜索框此刻有没有焦点（同上：有焦点时提示文案不归父窗画）。
    clip_search_focused: bool,
    /// 采集真的起来了没有 / 是不是暂停着 / 最近 60 秒的次数。
    collector_on: bool,
    collector_paused: bool,
    cpm: i64,
    /// 设置页左侧选中的分类。
    settings_cat: usize,
    /// 看板页当前选中的时间范围（画胶囊高亮与命中回写读同一份）。
    dash_range: focusflow_core::db::queries::DashRange,
}

/// 左表格卡片的矩形：画图与命中测试共用。
fn left_card(w: f32, h: f32) -> D2D_RECT_F {
    let m = 16.0;
    let top = 208.0;
    let body_h = h - top - m;
    let left_w = (w - m * 2.0 - 12.0) * 0.6;
    rect(m, top, left_w, body_h)
}

/// 自绘标题栏的高度 = 现有顶栏那一条（应用名 + 右侧三颗窗口按钮）。
///
/// 为什么不在内容区上面另起一条：原生标题条一拆，客户区就从窗口顶边开始。
/// 把现有顶栏当标题栏用，所有版面（统计卡 y=84、页签 y=172…）与验收脚本的客户区坐标
/// 一个都不用动 —— 代价是"标题栏"比系统那条高，视觉上更像一个应用头部。
const TITLEBAR_H: f32 = 44.0;
/// 窗口按钮的宽高（DIP）。46 是 Windows 11 标题栏按钮的常见宽度。
const WIN_BTN_W: f32 = 46.0;
const WIN_BTN_H: f32 = 40.0;

/// 右上角三颗窗口按钮：最小化 / 最大化-还原 / 关闭。画图、命中、点击读同一份，
/// `Ctl::WinBtn(i)` 的下标就是这个顺序。
fn win_buttons(w: f32) -> [D2D_RECT_F; 3] {
    let x = w - WIN_BTN_W * 3.0;
    [
        rect(x, 0.0, WIN_BTN_W, WIN_BTN_H),
        rect(x + WIN_BTN_W, 0.0, WIN_BTN_W, WIN_BTN_H),
        rect(x + WIN_BTN_W * 2.0, 0.0, WIN_BTN_W, WIN_BTN_H),
    ]
}

/// 这一点压在哪个窗口按钮上（客户区 DIP）。
fn win_button_at(w: f32, x: f32, y: f32) -> Option<u8> {
    win_buttons(w)
        .iter()
        .position(|r| hit(*r, x, y))
        .map(|i| i as u8)
}

/// 客户区最上面一层归谁。
///
/// 原生标题条拆掉之后，"窗口级动作"也搬到客户区里来了，它必须排在页面模态之前：
/// 设备详情弹窗把整屏吞掉时，右上角三颗照样要点得动（拆条之前它们本来就在窗口外）。
/// 顺序写在这个函数里，画图、悬停、点击三条路读同一份判断。
#[derive(Debug, PartialEq, Eq)]
enum TopOwner {
    WinBtn(u8),
    DetailModal,
    Page,
}

fn top_owner(detail_open: bool, w: f32, x: f32, y: f32) -> TopOwner {
    if let Some(i) = win_button_at(w, x, y) {
        return TopOwner::WinBtn(i);
    }
    if detail_open {
        return TopOwner::DetailModal;
    }
    TopOwner::Page
}

/// 非客户区命中：边框带 → 窗口按钮 → 标题带 → 客户区，返回 `HT*` 码。
///
/// 边框带排在按钮之前：三颗按钮画到窗口右沿，右上角那一格本来归缩放与贴角，
/// 让按钮先吃就等于把角弄死（看得见却拖不动，原生标题条时代是系统给的优先级）。
/// 顺序写在这一个纯函数里，屏上能不能拖动能在单测里算出来。
fn nc_hit_code(zoomed: bool, cw: f32, ch: f32, bx: f32, by: f32, x: f32, y: f32) -> u32 {
    if !zoomed {
        let (left, right) = (x < bx, x >= cw - bx);
        let (top, bottom) = (y < by, y >= ch - by);
        let code = match (left, right, top, bottom) {
            (true, _, true, _) => HTTOPLEFT,
            (_, true, true, _) => HTTOPRIGHT,
            (true, _, _, true) => HTBOTTOMLEFT,
            (_, true, _, true) => HTBOTTOMRIGHT,
            (true, _, _, _) => HTLEFT,
            (_, true, _, _) => HTRIGHT,
            (_, _, true, _) => HTTOP,
            (_, _, _, true) => HTBOTTOM,
            _ => HTCLIENT,
        };
        if code != HTCLIENT {
            return code;
        }
    }
    // 三颗窗口按钮必须回 HTCLIENT 才收得到 WM_LBUTTONDOWN（悬停底色与点击都在客户区那套里）。
    if win_button_at(cw, x, y).is_some() {
        return HTCLIENT;
    }
    // 标题带 = 拖窗、双击最大化、贴边分屏，全交给系统的非客户区逻辑。
    if y < TITLEBAR_H {
        return HTCAPTION;
    }
    HTCLIENT
}

/// 周期药丸的位置：画图与命中测试共用。
fn period_pills() -> [(i64, D2D_RECT_F); 6] {
    let mut out = [(0i64, rect(0.0, 0.0, 0.0, 0.0)); 6];
    for (i, (p, _)) in PERIODS.iter().enumerate() {
        out[i] = (*p, rect(16.0 + 66.0 + i as f32 * 60.0, 48.0, 54.0, 26.0));
    }
    out
}

/// 第 4 个页签上该画哪一张。
///
/// 三张共用一个页签（清单 / 视图 / 详情），而"画哪一张"与"点哪儿有用"必须读同一份判断：
/// 各写一套的症状是屏上画着详情、点击却按清单走（本仓在"画在 A、命中在 B"上踩过好几次）。
#[derive(Clone, Copy, Debug)]
enum PluginPage<'a> {
    /// 某个插件的视图（VM 交回的控件）
    View(&'a str, &'a focusflow_core::plugins::PluginView),
    /// 某一格的详情；行取自清单，找不到就退回清单
    Detail(&'a PluginRow),
    List,
}

/// 视图 > 详情 > 清单。
///
/// 详情那一格按 **stem** 找回行：那一行若已经从清单里消失（文件被删/改名），
/// 就画清单 —— 否则屏上会停在一张再也退不出去的页上（它的「返回」不会出现，
/// 因为清单里已经没有它）。
fn plugin_page_of<'a>(
    view: Option<(&'a str, &'a focusflow_core::plugins::PluginView)>,
    detail: Option<&'a str>,
    rows: &'a [PluginRow],
) -> PluginPage<'a> {
    if let Some((name, view)) = view {
        return PluginPage::View(name, view);
    }
    if let Some(stem) = detail {
        if let Some(row) = rows.iter().find(|r| r.stem == stem) {
            return PluginPage::Detail(row);
        }
    }
    PluginPage::List
}

#[allow(clippy::too_many_arguments)] // 主区各视图共用这一个入口：拆开传会在每个调用点重排一遍同样的参数
fn draw_screen(
    p: &P<'_>,
    w: f32,
    h: f32,
    snap: &Snapshot,
    // 内置看板页算好的那一页（数字在 core 的 `dashboard`，这里只排版）
    dash: &focusflow_core::dashboard::DashBoard,
    // 剪贴板管理页的状态（列表/模式/多选都在这一份里）
    clip: &clipboard_page::ClipPage,
    vs: ViewState,
    detail: Option<&DeviceDetail>,
    status: Option<&str>,
    busy: bool,
    filter: &str,
    plugins: &[PluginRow],
    plugin_err: Option<&str>,
    // 详情页开着时的那一格（按 stem 找行；找不到就退回清单）
    plugin_detail: Option<&str>,
    plugin_detail_source: Option<&str>,
    plugin_detail_installed: bool,
    uninstall: Option<&UninstallPrompt>,
    cleanup_confirm: bool,
    plugin_view: Option<(&str, &focusflow_core::plugins::PluginView)>,
    pv_scroll: i32,
    pv_select: Option<&PvSelect>,
    pv_modal: Option<&str>,
    // 插件表格各分组当前选中的记录 id（选中行高亮与 `sel` 按钮都读它）。
    plugin_sel: &HashMap<String, Vec<String>>,
    // 画的时候顺手记下"指针压着哪个被截断的格子"：提示框与右键复制同源。
    tip: &mut Option<(String, D2D_RECT_F)>,
) {
    let m = 16.0;

    // --- 顶栏 ---
    p.text("FocusFlow", rect(m, 8.0, 160.0, 30.0), &p.fmt.h1, &p.br.fg);
    p.text(
        "效率追踪器",
        rect(m + 112.0, 18.0, 120.0, 20.0),
        &p.fmt.caption,
        &p.br.muted,
    );
    // 右上角三颗窗口按钮（标题栏就是这一条：原生标题条已经拆掉了）。
    draw_window_buttons(p, w, vs);
    // 插件页的回执（安装 / 卸载 / 动作失败）只写在 `status` 里，而那一页的正文没有
    // 画它的地方 —— 从前坏包点下去、插件动作抛错都表现成"点了没反应"。
    if let Some((line, warn, r)) = topbar_status_line(vs.view, status, w) {
        let fmt = &p.fmt.caption;
        p.text(
            &p.clip(&line, fmt, r.right - r.left),
            r,
            fmt,
            if warn { &p.br.danger } else { &p.br.muted },
        );
    }

    // --- 周期页签 ---
    p.text(
        "统计周期",
        rect(m, 50.0, 60.0, 24.0),
        &p.fmt.caption,
        &p.br.muted,
    );
    for (i, (period, label)) in PERIODS.iter().enumerate() {
        let active = vs.period == *period;
        let r = period_pills()[i].1;
        if active {
            p.round(r, R_CONTROL, &p.br.accent, None);
        } else {
            p.round(r, R_CONTROL, &p.br.card, Some(&p.br.border));
            // 悬停只叠一层淡色：白底、卡片底、主色底上都成立，所以三组药丸共用一个画法。
            if vs.ctl == Some(Ctl::Period(i as u8)) {
                p.round(r, R_CONTROL, &p.br.hover, None);
            }
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
            thousands(snap.today.max(0) as u64),
            kinds_sub(snap),
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
            avg_sub(snap),
        ),
        (
            "最高单日".into(),
            thousands(max_day.1.max(0) as u64),
            max_day.0.clone(),
        ),
    ];
    let gap = 10.0;
    let cw = (w - m * 2.0 - gap * 4.0) / 5.0;
    for (i, (label, value, sub)) in cards.iter().enumerate() {
        let cx = m + i as f32 * (cw + gap);
        let r = rect(cx, 84.0, cw, 78.0);
        p.card(r, R_CARD);
        p.text(
            label,
            rect(cx + 12.0, 94.0, cw - 24.0, 16.0),
            &p.fmt.caption,
            &p.br.muted,
        );
        p.text(
            value,
            rect(cx + 12.0, 110.0, cw - 24.0, 30.0),
            &p.fmt.value,
            &p.br.fg,
        );
        if !sub.is_empty() {
            // 卡片下面那一行同样是 `NO_WRAP` 的 DrawText：窗口拖窄时它不会自己省略，
            // 而是照画到卡外（本仓那句"动态文本必须自己带长度上界"）。预算与画它的
            // 矩形取同一个宽度表达式 `cw - 24.0`，两处不许各写各的数。
            let sub = p.clip(sub, &p.fmt.caption, cw - 24.0);
            p.text(
                &sub,
                rect(cx + 12.0, 142.0, cw - 24.0, 16.0),
                &p.fmt.caption,
                &p.br.muted,
            );
        }
    }

    // --- 视图页签 ---
    for (i, label) in VIEWS.iter().enumerate() {
        let r = view_tabs()[i];
        let active = vs.view == i;
        let hot = vs.ctl == Some(Ctl::ViewTab(i as u8));
        p.text(
            label,
            rect(r.left, r.top, r.right - r.left, r.bottom - r.top),
            &p.fmt.body,
            if active || hot { &p.br.fg } else { &p.br.muted },
        );
        if active || hot {
            // 下划线跟着文字走：固定 52px 时，"设置"（两个字）和"键鼠排行"（四个字）
            // 下面挂的是同一条线，看着像随手画的。量不出来就退回旧值。
            let tw = p
                .measure(label, &p.fmt.body)
                .unwrap_or(52.0)
                .clamp(24.0, r.right - r.left);
            p.round(
                rect(r.left, r.bottom + 2.0, tw, 2.0),
                R_MICRO,
                if active { &p.br.accent } else { &p.br.grid },
                None,
            );
        }
    }

    match vs.view {
        0..=2 => draw_dashboard(p, w, h, snap, vs, tip),
        3 => draw_analytics(p, w, h, snap, vs),
        // 视图 > 详情 > 清单，判断只有一份（[`plugin_page_of`]）。
        4 => match plugin_page_of(plugin_view, plugin_detail, plugins) {
            PluginPage::View(name, view) => {
                draw_plugin_view(p, w, h, name, view, pv_scroll, plugin_sel, vs, tip);
            }
            PluginPage::Detail(row) => {
                let detail = focusflow_core::plugins::plugin_health::detail_for(&row.stem);
                draw_plugin_detail(
                    p,
                    w,
                    h,
                    row,
                    plugin_detail_source,
                    plugin_detail_installed,
                    &detail,
                );
            }
            PluginPage::List => draw_plugins(p, w, h, plugins, plugin_err, vs, tip),
        },
        5 => draw_settings(p, w, h, snap, vs, status, busy, filter),
        // 第 7 个页签：内置看板页。它排在 `v => draw_placeholder` 前面是必须的 ——
        // 那一支是"还没做的页签"的兜底，漏进来就会把这一页画成一句占位文案。
        6 => draw_board_page(p, w, h, dash, vs),
        // 第 8 个页签：剪贴板管理页。
        7 => draw_clipboard_page(p, w, h, clip, vs),
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

    // 卸载确认框也压在正文上：它开着时点击只认那三颗按钮。
    if let Some(prompt) = uninstall {
        draw_uninstall_prompt(p, w, h, prompt);
    }
    if cleanup_confirm {
        draw_cleanup_prompt(p, w, h);
    }

    // 模态最后画：它压在所有东西上面。
    if let Some(d) = detail {
        draw_detail(p, w, h, d);
    }
    // 键盘焦点框压在最上面：两圈 1px 叠成 2px，比单线在浅色底上更看得见。
    if let Some(r) = vs.kbd_focus {
        stroke_rect(
            p,
            rect(
                r.left - 2.0,
                r.top - 2.0,
                r.right - r.left + 4.0,
                r.bottom - r.top + 4.0,
            ),
            &p.br.accent,
        );
        stroke_rect(p, r, &p.br.accent);
    }
    if let Some((text, cell)) = tip.as_ref() {
        draw_tooltip(p, w, h, *cell, text);
    }
}

/// 这一格被截断、指针又正好压在上面时，把它记成这一帧的 tooltip 候选。
fn note_tooltip(
    tip: &mut Option<(String, D2D_RECT_F)>,
    vs: ViewState,
    cell: D2D_RECT_F,
    full: &str,
    clipped: bool,
) {
    if clipped && tip.is_none() && vs.tooltip && hit(cell, vs.ptr.0, vs.ptr.1) {
        *tip = Some((full.to_string(), cell));
    }
}

/// 一格的小提示：全文、贴在那格下面（下面放不下就翻到上面），不越出窗口。
fn draw_tooltip(p: &P<'_>, w: f32, h: f32, anchor: D2D_RECT_F, text: &str) {
    let pad = 8.0;
    let max_w = (w - 32.0).min(460.0);
    let measured = p.measure(text, &p.fmt.caption).unwrap_or(max_w);
    let tw = (measured + pad * 2.0).min(max_w);
    let th = 26.0;
    let mut x = anchor.left;
    if x + tw > w - 12.0 {
        x = (w - 12.0 - tw).max(12.0);
    }
    let mut y = anchor.bottom + 4.0;
    if y + th > h - 12.0 {
        y = (anchor.top - 4.0 - th).max(12.0);
    }
    let r = rect(x, y, tw, th);
    p.overlay_card(r, R_CONTROL);
    p.text(
        &p.clip(text, &p.fmt.caption, tw - pad * 2.0),
        rect(x + pad, y + 4.0, tw - pad * 2.0, 18.0),
        &p.fmt.caption,
        &p.br.fg,
    );
}

/// 1px 方框（最大化/还原图标用）。四边各画一条线，比 `DrawRectangle` 更好控粗细。
fn stroke_rect(p: &P<'_>, r: D2D_RECT_F, b: &ID2D1SolidColorBrush) {
    p.line(r.left, r.top, r.right, r.top, b, 1.0);
    p.line(r.left, r.bottom, r.right, r.bottom, b, 1.0);
    p.line(r.left, r.top, r.left, r.bottom, b, 1.0);
    p.line(r.right, r.top, r.right, r.bottom, b, 1.0);
}

/// 右上角三颗窗口按钮：最小化 / 最大化-还原 / 关闭。
///
/// 关闭键悬停压主色红（Windows 的约定），另外两颗只叠一层淡色；字形一律用细线，
/// 与系统标题栏的笔画粗细接近。最大化时第二颗画"还原"（两个错开的方框）。
fn draw_window_buttons(p: &P<'_>, w: f32, vs: ViewState) {
    for (i, r) in win_buttons(w).iter().enumerate() {
        let hot = vs.ctl == Some(Ctl::WinBtn(i as u8));
        if hot {
            p.fill(
                *r,
                if i == 2 {
                    &p.br.danger
                } else {
                    &p.br.row_hover
                },
            );
        }
        let ink = if hot && i == 2 {
            &p.br.white
        } else {
            &p.br.muted
        };
        let cx = r.left + WIN_BTN_W / 2.0;
        let cy = r.top + WIN_BTN_H / 2.0;
        match i {
            0 => p.line(cx - 5.0, cy, cx + 5.0, cy, ink, 1.0),
            1 => {
                if vs.zoomed {
                    stroke_rect(p, rect(cx - 2.0, cy - 6.0, 8.0, 8.0), ink);
                    stroke_rect(p, rect(cx - 6.0, cy - 2.0, 8.0, 8.0), ink);
                } else {
                    stroke_rect(p, rect(cx - 5.0, cy - 5.0, 10.0, 10.0), ink);
                }
            }
            _ => {
                p.line(cx - 5.0, cy - 5.0, cx + 5.0, cy + 5.0, ink, 1.0);
                p.line(cx + 5.0, cy - 5.0, cx - 5.0, cy + 5.0, ink, 1.0);
            }
        }
    }
}

const VIEWS: [&str; 8] = [
    "键鼠排行",
    "应用排行",
    "设备排行",
    "活跃分析",
    "插件管理",
    "设置",
    // 第 7 个页签（方案 14 节第 5 项按默认执行）：位置排在最后，
    // 前面六个的索引与坐标一个字都不动 —— 它们各有命中测试与验收夹具按位置点。
    "看板",
    // 第 8 个页签：剪贴板管理页。看板那三颗范围胶囊从这一格右边起（见 `dashboard.rs`）。
    "剪贴板",
];

/// 顶部视图页签的位置：画图与命中测试共用。
fn view_tabs() -> [D2D_RECT_F; 8] {
    let mut out = [rect(0.0, 0.0, 0.0, 0.0); 8];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = rect(16.0 + i as f32 * 84.0, 172.0, 80.0, 26.0);
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
fn draw_dashboard(
    p: &P<'_>,
    w: f32,
    h: f32,
    snap: &Snapshot,
    vs: ViewState,
    tip: &mut Option<(String, D2D_RECT_F)>,
) {
    let m = 16.0;
    let left = left_card(w, h);
    let right_x = left.right + 12.0;
    let right_w = w - m - right_x;
    let body_h = left.bottom - left.top;

    draw_table(p, left, snap, vs, tip);

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
    p.card(r, R_CARD);
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
            &p.fmt.caption,
            &p.br.muted,
        );
        return;
    };
    let goal = g.goal.max(1);
    let today = snap.today.max(0);
    let met = today >= goal;

    p.text(
        // 这一句只给 150 DIP，进度条从 r.left + pad + 158 起画 —— 七位今日 + 七位目标
        // 就 ~170 px，数字会压在条子上。夹取留头也留尾，尾巴是那个目标值。
        &p.clip(
            &format!(
                "今日 {} / {}",
                thousands(today as u64),
                thousands(goal as u64)
            ),
            &p.fmt.body,
            150.0,
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
    p.round(bar, R_MICRO, &p.br.grid, None);
    let pct = (today as f32 / goal as f32).clamp(0.0, 1.0);
    if pct > 0.0 {
        p.round(
            rect(bar.left, bar.top, (bar.right - bar.left) * pct, 8.0),
            R_MICRO,
            if met { &p.br.success } else { &p.br.accent },
            None,
        );
    }

    p.text(
        // 右对齐 ⇒ 越界是往左跑，而左边 8 px 就是进度条：五位数打卡 + 五位数最长就顶到条上。
        &p.clip(
            &format!("连续打卡 {} 天 · 近一年最长 {} 天", g.streak, g.best),
            &p.fmt.caption_right,
            198.0,
        ),
        rect(dots_x - 206.0, r.top + 14.0, 198.0, 18.0),
        &p.fmt.caption_right,
        &p.br.muted,
    );

    // 今天那一格跟着进度条那份读数走（理由见 `goal_dot_states`）：绿条配灰点是屏上自相矛盾。
    // 今天那一格跟着进度条那份读数走（理由见 `goal_dot_states`）：绿条配灰点是屏上自相矛盾。
    let today_str = chrono::Local::now().format("%Y-%m-%d").to_string();
    for (i, dmet) in goal_dot_states(g, &today_str, met)
        .iter()
        .enumerate()
        .take(7)
    {
        let dot = rect(dots_x + i as f32 * 17.0, r.top + 16.0, 14.0, 14.0);
        p.round(
            dot,
            R_MICRO,
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
            p.round(r, R_CONTROL, &p.br.accent, None);
        } else {
            p.round(r, R_CONTROL, &p.br.card, Some(&p.br.border));
            if vs.ctl == Some(Ctl::ATab(i as u8)) {
                p.round(r, R_CONTROL, &p.br.hover, None);
            }
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

/// 柱顶那个数字要占的那一条带头（与下面画数字的 `- 18.0` 同一份）。
const WEEKDAY_VALUE_HEADROOM: f32 = 18.0;

/// 星期分布那一根柱的高度：给柱顶那个数字留出带头。
///
/// 为什么要有这一层：`plot` 从卡片 `r.top + 54` 起，而柱顶的数字画在 `bar.top - 18`。
/// 不留出带头时最高的那一根顶到 `plot.top`，数字就落在 `r.top + 36` —— 那正是
/// 「本栏…没读全 · 数据源：…」那一行的格子（只在有年份读不动时出现），而这个 UI
/// 没有"装不下"这回事：`p.text` 不带裁切，两句直接叠在一起。
/// `max <= 0` / `plot_h <= 0` 交出 0：没有可画的高度就是没有高度，不返回负数。
fn weekday_bar_height(v: f32, max: f32, plot_h: f32) -> f32 {
    if max <= 0.0 || plot_h <= WEEKDAY_VALUE_HEADROOM {
        return 0.0;
    }
    (v / max) * (plot_h - WEEKDAY_VALUE_HEADROOM)
}

/// 星期分布。
fn draw_weekday(p: &P<'_>, r: D2D_RECT_F, snap: &Snapshot) {
    p.card(r, R_CARD);
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
        &p.fmt.caption_right,
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
            &p.fmt.caption,
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
        let hh = weekday_bar_height(v, max, plot_height(plot));
        let x = plot.left + i as f32 * (bw + gap);
        let bar = rect(x, plot.bottom - hh, bw, hh);
        // 周末用绿色、工作日用强调色 —— 两套主题里都是这两个令牌。
        let base_c = if i >= 5 { p.pal.success } else { p.pal.accent };
        if let Some(b) = p.brush(with_alpha(base_c, 0.85)) {
            p.round(bar, R_MICRO, &b, None);
        }
        p.text(
            label,
            rect(x, plot.bottom + 6.0, bw, 16.0),
            &p.fmt.caption_center,
            &p.br.muted,
        );
        p.text(
            // 居中的一档越界是两头都跑，七位数字在窄窗口就会与邻柱的数字叠在一起。
            &p.clip(&thousands(count.max(0) as u64), &p.fmt.center_small, bw),
            rect(x, plot.bottom - hh - WEEKDAY_VALUE_HEADROOM, bw, 16.0),
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
    p.card(r, R_CARD);
    p.text(
        "键盘热力",
        rect(r.left + 16.0, r.top + 14.0, 160.0, 20.0),
        &p.fmt.body,
        &p.br.fg,
    );
    p.text(
        &format!(
            "按{}统计 · 前 {} 键",
            period_text(snap.period),
            data::RANK_DISPLAY_MAX
        ),
        rect(r.right - 16.0 - 260.0, r.top + 16.0, 260.0, 18.0),
        &p.fmt.caption_right,
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
    // 悬停那格的占比分母：与板上这些格子同一个出处（`snap.rank` 那一趟读回来的键鼠总数）。
    // 卡片那个 `period_total` 里还含着没落库的增量，而榜里的行不含 —— 拿它当分母，
    // 每个键帽的占比都按那批增量的比例偏小，且屏上不会说为什么。
    let total = snap.rank_share_base().max(0);

    for (i, k) in keys.iter().enumerate() {
        let count = counts.get(k.name).copied().unwrap_or(0);
        let base = rect(ox + k.x * u, oy + k.y * u, k.w * u - 2.0, k.h * u - 2.0);
        p.round(base, R_MICRO, &p.br.card, Some(&p.br.grid));
        if count > 0 {
            let heat = 0.06 + 0.8 * (count as f32 / max).min(1.0);
            if let Some(b) = p.brush(with_alpha(p.pal.accent, heat)) {
                p.round(base, R_MICRO, &b, None);
            }
        }
        if hover == Some(i) {
            p.round(base, R_MICRO, &p.br.card, Some(&p.br.accent));
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
        &p.fmt.caption,
        &p.br.muted,
    );
    for i in 0..5 {
        let t = i as f32 / 4.0;
        if let Some(b) = p.brush(with_alpha(p.pal.accent, 0.06 + 0.8 * t)) {
            p.round(
                rect(r.left + 54.0 + i as f32 * 18.0, ly + 4.0, 14.0, 14.0),
                R_MICRO,
                &b,
                None,
            );
        }
    }
    p.text(
        "少",
        rect(r.left + 54.0 + 5.0 * 18.0 + 2.0, ly + 4.0, 20.0, 14.0),
        &p.fmt.caption,
        &p.br.muted,
    );

    // 板上没摆的键汇总成一句：这一句的范围是**榜上那些行**（面板标题写的"前 N 键"就是它），
    // 榜外那些种类不在这里 —— 它们既进不了板上的格子，也不进这一句的个数与次数。
    // 原先这句注释写的是"不静默丢数据"，那是许愿：拿截断后的榜算"其它按键"本身就是丢。
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
            format!(
                "{} · {} 次 · {}",
                k.name,
                thousands(c.max(0) as u64),
                share_text(c, total)
            )
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
///
/// 三格**全是屏幕坐标**。光标那一格不能记客户区坐标：窗口跟着光标走，客户区原点也在走，
/// 拿它当基准等于每挪一次就换一次坐标系 —— 窗口会在"该在的位置"与"按下时那一格"之间
/// 来回跳，屏上看到的就是拖动时一直在抖/闪（量法见 `dragging_a_pin_follows...` 那条探针）。
#[derive(Clone, Copy)]
struct PinDrag {
    hwnd: isize,
    origin: (i32, i32),
    press: (i32, i32),
    /// 上一次已经摆到的窗口左上角：高回报率鼠标在同一像素上会连着来好几条，
    /// 目标没变就不必再 `SetWindowPos` 一次。
    last: (i32, i32),
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

/// 窗口所在显示器的**工作区**（已经扣掉任务栏那一条）。贴图夹取与主窗口几何还原共用。
fn work_area_near_window(hwnd: HWND) -> (i32, i32, i32, i32) {
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
fn open_pin(img: Vec<u8>, w: i32, h: i32, x: i32, y: i32) -> std::result::Result<HWND, String> {
    unsafe {
        if PINS.lock().map(|v| v.len()).unwrap_or(0) >= PIN_MAX_COUNT {
            tracing_note(&format!("贴图 pin-skip 已到上限 {PIN_MAX_COUNT} 张"));
            return Err(format!(
                "贴图最多同时钉 {PIN_MAX_COUNT} 张，先关掉几张（右键）再试"
            ));
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
                Ok(pin_hwnd)
            }
            Err(e) => {
                tracing_note(&format!("贴图 pin-fail 建窗失败：{e}"));
                Err(format!("建贴图窗口失败：{e}"))
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
        let (nx, ny) =
            pin_keep_close_visible((rc.left, rc.top), (w, h), work_area_near_window(hwnd));
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
        let _ = open_pin(img, w, h, 420 + (i as i32) * 40, 320 + (i as i32) * 30);
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
                // 基准取**屏幕坐标**：客户区坐标会跟着窗口自己漂（见 `PinDrag` 那段）。
                let mut screen = POINT::default();
                let _ = GetCursorPos(&mut screen);
                let origin = (rc.left, rc.top);
                if let Ok(mut d) = PIN_DRAG.lock() {
                    *d = Some(PinDrag {
                        hwnd: hwnd.0 as isize,
                        origin,
                        press: (screen.x, screen.y),
                        last: origin,
                    });
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let cx = (l.0 & 0xFFFF) as i16 as i32;
                let cy = ((l.0 >> 16) & 0xFFFF) as i16 as i32;
                // 在拖：目标位置每一步都按**当前真实光标**重算，不用这条消息里的客户区坐标
                // —— 那两格是相对窗口当时的左上角算的，而窗口自己正被这条消息挪走。
                let mut dragging = false;
                let mut drag_to = None;
                if let Ok(mut g) = PIN_DRAG.lock() {
                    if let Some(d) = g.as_mut().filter(|d| d.hwnd == hwnd.0 as isize) {
                        dragging = true;
                        let mut screen = POINT::default();
                        if GetCursorPos(&mut screen).is_ok() {
                            let t = (
                                d.origin.0 + (screen.x - d.press.0),
                                d.origin.1 + (screen.y - d.press.1),
                            );
                            if t != d.last {
                                d.last = t;
                                drag_to = Some(t);
                            }
                        }
                    }
                }
                if dragging {
                    if let Some((x, y)) = drag_to {
                        let _ = SetWindowPos(
                            hwnd,
                            None,
                            x,
                            y,
                            0,
                            0,
                            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                    }
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
    /// 图表（折线/柱状）。**没有点击分支**：路线图 3.2.2 本期不做交互，
    /// 它在命中测试那一圈里必须保持惰性（见 `plugin_view_hit_arms` 那条判据），
    /// 否则一块画图的矩形会吃掉它下面的按钮。
    Chart {
        rect: D2D_RECT_F,
        kind: String,
        labels: Vec<String>,
        points: Vec<i64>,
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
            | POp::Table { rect, .. }
            | POp::Chart { rect, .. } => *rect,
        }
    }
}

/// 插件视图里一条控件"认不认"这次点击。
///
/// 命中测试按 `ops` 的顺序逐条问，第一条认领的负责处理并吞掉点击。判据必须只看
/// "这次点击落在哪个控件上"：从前 `POp::Table` 对**任何**没落在行或行内按钮上的点击
/// 都直接 `return`，于是排在表格后面的控件（记账本的分页条就是紧挨着表格的那一颗）
/// 永远收不到点击 —— 屏上画得出来、点下去却没反应。表格只在自己那块矩形里认领。
fn pv_op_claims_click(op: &POp, x: f32, y: f32) -> bool {
    match op {
        POp::Button { rect, disabled, .. } => !*disabled && hit(*rect, x, y),
        // 行内操作按钮可能压在表格边上，先认它；其余落在表格矩形里的点击（表头、
        // 行与行之间）由这张表吞掉，免得穿透到下面的控件。
        POp::Table { rect, buttons, .. } => {
            hit(*rect, x, y) || buttons.iter().any(|(br, ..)| hit(*br, x, y))
        }
        POp::Select { rect, field, .. } => {
            // 没有 field 就写不回插件（与空 field 的输入框同一条）：菜单选完只会把
            // 值写进空字段、视图照旧，看起来像"选了不生效"。这种下拉按只读展示。
            !field.is_empty() && hit(select_value_rect(*rect), x, y)
        }
        POp::Field { rect, field, .. } => {
            !field.is_empty() && hit(pv_field_value_rect(*rect), x, y)
        }
        _ => false,
    }
}

/// 按 `ops` 的顺序问第一条认领这次点击的操作；没有就返回 `None`。
/// 与 `on_click` 里那一圈共用同一份判据，用例可以拿它验"分页条排在表格后面也能点到"。
fn pv_claiming_op(ops: &[POp], x: f32, y: f32) -> Option<&POp> {
    ops.iter().find(|op| pv_op_claims_click(op, x, y))
}

/// 正文可见区里这一次点击归谁。可见区之外一律不认领：绘制按 [`plugin_content_clip`]
/// 裁掉了滚出去的那一截，命中不裁就会"点标题那一条却触发了屏上没有的控件"。
fn pv_claiming_op_in(ops: &[POp], clip: D2D_RECT_F, x: f32, y: f32) -> Option<&POp> {
    if !hit(clip, x, y) {
        return None;
    }
    pv_claiming_op(ops, x, y)
}

/// 插件视图里一个"能点"的格子。`row = true` 用行悬停底色，否则用控件那种更淡的叠加。
#[derive(Clone, Copy, PartialEq, Debug)]
struct PvTarget {
    rect: D2D_RECT_F,
    row: bool,
}

/// 从排好版的控件里挑出真正认领点击的那几格 —— 判据与 [`pv_op_claims_click`] 同一份，
/// 否则会亮着一格、点下去落在别处。
///
/// 表格那张的**整块矩形**不进来：表头与行间空白虽然会吞点击，但那里没有可做的事，
/// 指过去亮起来等于许诺一次不存在的操作。
fn pv_targets_of(ops: &[POp]) -> Vec<PvTarget> {
    let mut out = Vec::new();
    for op in ops {
        match op {
            POp::Button { rect, disabled, .. } if !*disabled => out.push(PvTarget {
                rect: *rect,
                row: false,
            }),
            POp::Select { rect, field, .. } if !field.is_empty() => out.push(PvTarget {
                rect: select_value_rect(*rect),
                row: false,
            }),
            POp::Field { rect, field, .. } if !field.is_empty() => out.push(PvTarget {
                rect: pv_field_value_rect(*rect),
                row: false,
            }),
            POp::Table {
                rect, buttons, ids, ..
            } => {
                for (br, ..) in buttons {
                    out.push(PvTarget {
                        rect: *br,
                        row: false,
                    });
                }
                for i in 0..ids.len() {
                    out.push(PvTarget {
                        rect: pv_table_row_rect(rect, i),
                        row: true,
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// 指针落在哪一格。可见区之外不算（滚出去的内容看不见，也不该亮）。
fn pv_target_on(targets: &[PvTarget], clip: D2D_RECT_F, x: f32, y: f32) -> Option<PvTarget> {
    if !hit(clip, x, y) {
        return None;
    }
    targets.iter().copied().find(|t| hit(t.rect, x, y))
}

/// 键盘能停住的一格：矩形 + 组（0 周期 / 1 页签 / 2 本页）+ 滚动列表里的下标。
#[derive(Clone, Copy)]
struct KbdItem {
    rect: D2D_RECT_F,
    group: u8,
    /// `Some` = 这一格在一条可滚动的列表里；键盘移动要把它滚进可见区。
    list_index: Option<usize>,
}

/// 键盘焦点下一步落在哪一格。
///
/// `same_group` 是上下键：只在同一组里走（列表行之间），到组边界就绕回本组；
/// Tab 走整张表。抽成纯函数是为了能把"绕回"与"跳过别组"两条都钉住。
fn kbd_next(groups: &[u8], cur: Option<usize>, delta: isize, same_group: bool) -> Option<usize> {
    if groups.is_empty() {
        return None;
    }
    let n = groups.len();
    let Some(i) = cur.filter(|i| *i < n) else {
        if delta < 0 {
            return Some(n - 1);
        }
        if same_group {
            // 上下键从"本页"开始：从周期药丸起步的话，第一下 Down 会去改周期。
            return groups.iter().position(|g| *g == 2);
        }
        return Some(0);
    };
    if !same_group {
        return Some((i as isize + delta).rem_euclid(n as isize) as usize);
    }
    let g = groups[i];
    let mut j = i as isize;
    for _ in 0..n {
        j = (j + delta).rem_euclid(n as isize);
        if groups[j as usize] == g {
            return Some(j as usize);
        }
    }
    Some(i)
}

/// 这一条滚动条管的是哪一页（拖动松手前只改它自己的偏移）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ScrollLane {
    Table,
    Settings,
    Plugins,
    PluginView,
}

/// 正在拖的那根滑块：轨道、滑块高、按下点距滑块顶部多远、总数与一屏容量。
#[derive(Clone, Copy)]
struct ScrollDrag {
    lane: ScrollLane,
    track: D2D_RECT_F,
    thumb_h: f32,
    grab: f32,
    total: usize,
    visible: usize,
}

/// 滑块矩形（`None` = 装得下，不画）。最短 16 px，否则一百行时只剩一条线。
fn scrollbar_thumb(
    track: D2D_RECT_F,
    total: usize,
    visible: usize,
    offset: usize,
) -> Option<D2D_RECT_F> {
    if visible == 0 || total <= visible {
        return None;
    }
    let h = track.bottom - track.top;
    if h <= 0.0 {
        return None;
    }
    let thumb_h = (h * visible as f32 / total as f32).clamp(16.0, h);
    let max_scroll = (total - visible) as f32;
    let t = if max_scroll > 0.0 {
        (offset as f32 / max_scroll).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let y = track.top + (h - thumb_h) * t;
    Some(rect(track.left, y, track.right - track.left, thumb_h))
}

/// 拖到 `y` 时该滚到哪一格。
fn scroll_offset_for_y(d: &ScrollDrag, y: f32) -> usize {
    if d.total <= d.visible {
        return 0;
    }
    let span = (d.track.bottom - d.track.top) - d.thumb_h;
    if span <= 0.0 {
        return 0;
    }
    let t = ((y - d.grab - d.track.top) / span).clamp(0.0, 1.0);
    (t * (d.total - d.visible) as f32).round() as usize
}

/// 画一条竖向滚动条：轨道常驻（有东西可滚时才画），滑块跟着偏移走。
fn draw_vscroll(p: &P<'_>, track: D2D_RECT_F, total: usize, visible: usize, offset: usize) {
    let Some(thumb) = scrollbar_thumb(track, total, visible, offset) else {
        return;
    };
    p.round(track, R_MICRO, &p.br.header, None);
    p.round(thumb, R_MICRO, &p.br.accent, None);
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

/// 插件正文的滚动窗口：测量内容高、夹偏移、滚轮三处共用这一份几何。
fn plugin_scroll_area(w: f32, h: f32) -> D2D_RECT_F {
    rect(16.0, 208.0 + 40.0, w - 32.0, h - 208.0 - 16.0)
}

/// 滚动窗口当前能看见多高（卡片去掉标题条与下边距）。
fn plugin_scroll_visible(h: f32) -> f32 {
    (h - 208.0 - 16.0 - 42.0).max(1.0)
}

/// 把滚动偏移夹进 `[0, 内容高 - 可见高]`：刷新之后内容会变短，旧偏移不夹的话整页被推到
/// 可见区上面，屏上只剩一张空卡片 —— 要再滚一下滚轮才会回来。
fn clamp_plugin_scroll(offset: i32, content: f32, visible: f32) -> i32 {
    offset.clamp(0, (content - visible).max(0.0) as i32)
}

/// 插件正文的可见矩形（画与命中共用这一份几何）。滚出去的内容画不出来，
/// 也就不该继续在自己原来的坐标上认领点击。
fn plugin_content_clip(card: D2D_RECT_F) -> D2D_RECT_F {
    rect(
        card.left + 1.0,
        card.top + 40.0,
        card.right - card.left - 2.0,
        card.bottom - card.top - 42.0,
    )
}

/// 插件表格的行几何：表头与行高是画、命中、行内按钮三处共用的同一把尺子。
const PV_TABLE_HEAD_H: f32 = 24.0;
const PV_TABLE_ROW_H: f32 = 24.0;
/// 一张表最多画几行（面板没有表格内滚动，其余行这次就不出现）。
///
/// 单列成常量是因为 `rows` 与 `ids` 必须**一起**截：命中测试按 `ids.len()` 排行矩形，
/// 只截 `rows` 等于给画不出来的行留出一块点击区（见
/// `a_table_with_more_rows_than_drawn_has_no_invisible_hit_rows`）。
///
/// **上限不许小于一页的条数**：内置记账本按每页 10 条查、再靠分页条翻页，上限是 6 时
/// 每页后 4 条被画没而 `total` 照旧把它们算进去 —— 翻到哪一页都只看得到前 6 条，
/// 而且没有任何一处说这件事（见 `the_table_row_cap_does_not_hide_a_page_of_the_ledger`）。
///
/// 这一格是**画得动的安全阀，不是分页**：内置那两张长表（Edge 近 30 天、番茄历史 20 条）
/// 必须整张画得出来，所以上限按它们取，而不是按"一屏塞得下几行"。真超过上限时表下方
/// 会明说还有几行没画（`table_truncated_note`），不许悄悄少画。
const PV_TABLE_MAX_ROWS: usize = 50;
/// 一张插件图表占多高（卡片本体，含轴）。
///
/// 与内置页那张「每日活跃趋势」不同一处：那一块的高度跟着窗口走，这一块的正文没有滚动以外的
/// 自适应，所以钉死一档就行 —— 但必须是**一处**：布局算这个值、画的时候读同一份 plan，
/// 两边各写一遍就会出现卡片高度与绘图区高度对不上。
const PV_CHART_H: f32 = 150.0;
/// 横轴最多标几个刻度文本（面板宽约 900，一格 90 px 已经挤；90 个点全标就是糊成一片）。
const PV_CHART_MAX_LABELS: usize = 7;
/// `row` 里相邻两个控件之间的空隙。
const PV_ROW_GAP: f32 = 10.0;
/// `row` 里一个输入框/下拉要占的宽度：标签 140 + 值框 260。
///
/// 这两个数是画法与命中共同钉死的（值框在 `left + 146`、宽 260），所以横排时这一格
/// 必须正好是 406；少一格就会压到隔壁控件身上，而两个矩形叠着的时候点击只能中一个。
const PV_FIELD_W: f32 = 406.0;
/// 分页条：左边那句「第 x / y 页 · 共 n 条」占多宽，右边两颗按钮各多宽。
const PV_PAGER_LABEL_W: f32 = 190.0;
const PV_PAGER_BTN_W: f32 = 76.0;

/// 一张表被截断时，表下面那一句：明说总行数与画出来的行数。
fn table_truncated_note(total_rows: usize) -> String {
    format!(
        "这张表共 {total_rows} 行，只画了前 {} 行（面板没有表格内滚动；要翻页请用 pager）",
        PV_TABLE_MAX_ROWS.min(total_rows)
    )
}

/// 按钮宽度：按文字量算、不低于 96。画与命中都读这一个数（从前同一个算式在两处各写一遍）。
fn pv_button_width(text: &str, avail: f32) -> f32 {
    ((text.chars().count() as f32) * 15.0 + 28.0)
        .max(96.0)
        .min(avail)
}

/// 一格里行内操作按钮各占多宽（按 `actions` 的顺序）。
///
/// 自然宽按标签字数算，但整列最多占表宽的 55%：从前没有上限，插件写几个长标签
/// 就把数据列挤成 40 px 一格，按钮本身还会从卡片左边伸出去 —— 数据看得见一半、
/// 按钮点得着一半。压窄之后文字由画那一侧按矩形省略，不会画到按钮外面。
fn pv_row_action_widths(labels: &[String], table_w: f32) -> Vec<f32> {
    let n = labels.len() as f32;
    if n == 0.0 {
        return Vec::new();
    }
    let gap = 6.0;
    let natural: Vec<f32> = labels
        .iter()
        .map(|l| (l.chars().count() as f32 * 14.0 + 16.0).max(44.0))
        .collect();
    let total = natural.iter().sum::<f32>() + (n - 1.0) * gap + 12.0;
    let cap = (table_w * 0.55).max(0.0);
    if total <= cap {
        return natural;
    }
    let unit = ((cap - 12.0 - (n - 1.0) * gap) / n).max(20.0);
    natural.iter().map(|w| w.min(unit)).collect()
}

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
            emit(widget, x, w, y, ops, plugin_sel);
        }
    }

    /// 排一个控件。整列与 `row` 里共用这一份：横排时调用方把宽度缩到这一格的自然宽，
    /// 于是画与命中仍然读同一份矩形（`POp` 的 `rect` 是两边唯一的尺子）。
    fn emit(
        widget: &Widget,
        x: f32,
        w: f32,
        y: &mut f32,
        ops: &mut Vec<POp>,
        plugin_sel: &HashMap<String, Vec<String>>,
    ) {
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
                let lines = t.lines().count().clamp(1, PV_TEXTAREA_MAX_LINES) as f32;
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
                let bw = pv_button_width(text, w);
                ops.push(POp::Button {
                    rect: rect(x, *y, bw, 30.0),
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
                let shown: Vec<Vec<String>> =
                    rows.iter().take(PV_TABLE_MAX_ROWS).cloned().collect();
                // `ids` 的契约是"与 rows 平行"（见 `POp::Table` 那一句），而命中测试按
                // `ids.len()` 一路往下摆行矩形 ⇒ 从前只截 rows、ids 整份交回，表格下方
                // 那片空白和排在后面的按钮就被读成"选了第 7、8… 行"：写回一个屏上没有的
                // id（`onselect` 还会让插件按它刷新），并把那次点击吞掉。
                let shown_ids: Vec<String> = ids.iter().take(PV_TABLE_MAX_ROWS).cloned().collect();
                let h = PV_TABLE_HEAD_H + shown.len() as f32 * PV_TABLE_ROW_H;
                // 有行内操作时，右侧留出按钮列，数据列相应收窄
                let mut buttons: Vec<(D2D_RECT_F, String, String)> = Vec::new();
                let mut reserve = 0.0f32;
                if !actions.is_empty() {
                    let labels: Vec<String> = actions.iter().map(|(_, l)| l.clone()).collect();
                    let widths = pv_row_action_widths(&labels, w);
                    reserve =
                        widths.iter().sum::<f32>() + (actions.len() as f32 - 1.0) * 6.0 + 12.0;
                    for (ri, _) in shown.iter().enumerate() {
                        let rid = shown_ids.get(ri).cloned().unwrap_or_default();
                        let mut bx = x + w - 6.0;
                        for (k, (prefix, label)) in actions.iter().enumerate().rev() {
                            let bw = widths[k];
                            bx -= bw;
                            let by = *y + PV_TABLE_HEAD_H + ri as f32 * PV_TABLE_ROW_H + 1.0;
                            buttons.push((
                                rect(bx, by, bw, 22.0),
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
                    ids: shown_ids,
                    group: group.clone(),
                    onselect: onselect.clone(),
                    sel: plugin_sel.get(group.as_str()).cloned().unwrap_or_default(),
                });
                *y += h + 6.0;
                // 真超过上限时把"少画了几行"写在表下面：悄悄少画等于让用户以为表里就这么多，
                // 而插件那边 `total`/行数说的又是另一个数。内置那几张表都在上限以内，
                // 这一句是给第三方长表兜底的（要翻页请改用 pager）。
                if rows.len() > PV_TABLE_MAX_ROWS {
                    ops.push(POp::Label {
                        rect: rect(x, *y, w, 18.0),
                        text: table_truncated_note(rows.len()),
                    });
                    *y += 22.0;
                }
            }
            Widget::Pager {
                page,
                pages,
                total,
                prev_id,
                next_id,
            } => {
                // 分页条 = 一句读数 + 两颗按钮。从前只画了那句读数、把 `prev`/`next`
                // 直接丢掉：协议带着两个动作 id，屏上却一颗都点不到（记账本翻不了页，
                // 每页只能看到开头那几行）。到首页/末页时按钮画成禁用，点了不发动作 ——
                // 交给插件去夹也行，但"点了没反应"与"这一头已经没有下一页"是两件事。
                ops.push(POp::Label {
                    rect: rect(x, *y + 5.0, PV_PAGER_LABEL_W, 18.0),
                    text: format!("第 {page} / {pages} 页 · 共 {total} 条"),
                });
                let mut bx = x + PV_PAGER_LABEL_W + PV_ROW_GAP;
                let at_start = *page <= 1;
                let at_end = *page >= *pages;
                for (id, label, disabled) in
                    [(prev_id, "上一页", at_start), (next_id, "下一页", at_end)]
                {
                    if id.is_empty() {
                        continue;
                    }
                    ops.push(POp::Button {
                        rect: rect(bx, *y, PV_PAGER_BTN_W, 28.0),
                        text: label.to_string(),
                        id: id.clone(),
                        disabled,
                        modal: None,
                        sel: false,
                        group: String::new(),
                    });
                    bx += PV_PAGER_BTN_W + 8.0;
                }
                *y += 34.0;
            }
            Widget::Chart {
                kind,
                labels,
                points,
            } => {
                ops.push(POp::Chart {
                    rect: rect(x, *y, w, PV_CHART_H),
                    kind: kind.clone(),
                    labels: labels.clone(),
                    points: points.clone(),
                });
                *y += PV_CHART_H + 8.0;
            }
            Widget::Row { children } => {
                // 横排：按每个子控件的自然宽度从左往右摆，放不下就换行。
                // 从前这里是"先按纵向堆"—— 作者写的一整排按钮与筛选框于是各占一行：
                // 记账本那 8 颗操作按钮排成了八个单行，查询栏的五格也各占一行。
                let mut cx = x;
                let mut line_top = *y;
                let mut line_h = 0.0f32;
                for child in children {
                    let cw = row_child_width(child, w);
                    if cx > x + 0.01 && cx + cw > x + w + 0.01 {
                        *y = line_top + line_h + PV_ROW_GAP;
                        cx = x;
                        line_top = *y;
                        line_h = 0.0;
                    }
                    let mut cy = line_top;
                    emit(child, cx, cw, &mut cy, ops, plugin_sel);
                    line_h = line_h.max(cy - line_top);
                    cx += cw + PV_ROW_GAP;
                }
                *y = line_top + line_h + PV_ROW_GAP;
            }
            Widget::ModalForm { .. } => {
                // 弹窗不再按普通控件排在正文里：`draw_plugin_modal` 把它画成整屏遮罩上
                // 的一张卡片（点在按钮上会设 `pv_modal`，插件自己 `open=true` 的直接开）。
                // 留出 0 高度，正文不会因为它多一块空白。
            }
        }
    }

    /// 横排时一个子控件占多宽：按钮按文字量、输入框/下拉按值框那套固定几何，
    /// 其余（标题/表格/图/嵌套 row）要整行 —— 它们本来就没有"半行"这一档画法。
    fn row_child_width(widget: &Widget, avail: f32) -> f32 {
        match widget {
            Widget::Button { text, .. } => pv_button_width(text, avail),
            Widget::TextInput { .. } | Widget::Select { .. } => PV_FIELD_W.min(avail),
            _ => avail,
        }
    }

    walk(&view.widgets, x, w, &mut y, &mut ops, plugin_sel);
    ops
}

/// 插件视图标题那一格：从「返回」右边到卡片右边再留 16。
///
/// 标题与插件名都是插件写的，而这块 UI 不按矩形裁切（`DrawText` 不按矩形裁）——
/// 从前那格写死 420 宽、文本原样画出去，长标题会画到卡片右边甚至窗口外。
fn plugin_view_title_rect(card: D2D_RECT_F, back: D2D_RECT_F) -> D2D_RECT_F {
    let left = back.right + 12.0;
    let right = card.right - 16.0;
    rect(left, card.top + 14.0, (right - left).max(0.0), 20.0)
}

/// 画插件视图；返回"返回"按钮的矩形（点击时用同一份几何）。
#[allow(clippy::too_many_arguments)] // 画这一页就是这么多读数，拆开只是换个地方排
fn draw_plugin_view(
    p: &P<'_>,
    w: f32,
    h: f32,
    name: &str,
    view: &focusflow_core::plugins::PluginView,
    scroll: i32,
    plugin_sel: &HashMap<String, Vec<String>>,
    vs: ViewState,
    tip: &mut Option<(String, D2D_RECT_F)>,
) -> D2D_RECT_F {
    let card = rect(16.0, 208.0, w - 32.0, h - 208.0 - 16.0);
    p.card(card, R_CARD);

    let back = rect(card.left + 16.0, card.top + 12.0, 64.0, 24.0);
    p.round(back, R_CONTROL, &p.br.header, Some(&p.br.border));
    if vs.pv_hover.is_some_and(|h| h.rect == back) {
        p.round(back, R_CONTROL, &p.br.hover, None);
    }
    p.text("← 返回", back, &p.fmt.center_small, &p.br.fg);

    let title = if view.title.is_empty() || view.title == name {
        name.to_string()
    } else {
        format!("{name} · {}", view.title)
    };
    // 标题与插件名都是插件写的，而这块 UI 不按矩形裁切：从前写死 420 宽、文本原样
    // 画出去，长标题会越到卡片右边（甚至窗口外）。按卡片剩下的那一格夹。
    let title_rect = plugin_view_title_rect(card, back);
    p.text(
        &p.clip(&title, &p.fmt.body, title_rect.right - title_rect.left),
        title_rect,
        &p.fmt.body,
        &p.br.fg,
    );

    // 内容区裁剪：视图比卡片高时，滚动到一半的控件不能画到卡片外面去
    let clip = plugin_content_clip(card);
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
    // 行悬停铺在内容底下：叠在字上会把插件自己写的文字染成另一档颜色。
    if let Some(h) = vs.pv_hover.filter(|h| h.row) {
        p.fill(h.rect, &p.br.row_hover);
    }
    for op in &ops {
        match op {
            POp::Heading { rect: r, text } => {
                // 插件给的字符串长度是作者说了算，而这块 UI 不裁切 ⇒ 一律按自己那一格夹。
                p.text(
                    &p.clip(text, &p.fmt.body, r.right - r.left),
                    *r,
                    &p.fmt.body,
                    &p.br.fg,
                );
            }
            POp::Label { rect: r, text } => {
                p.text(
                    &p.clip(text, &p.fmt.caption, r.right - r.left),
                    *r,
                    &p.fmt.caption,
                    &p.br.muted,
                );
            }
            POp::Kv {
                rect: r,
                key,
                value,
            } => {
                // 键那一格定宽 140，值从 +146 起画：不夹就是两句互相叠上去。
                p.text(
                    &p.clip(key, &p.fmt.caption, 140.0),
                    rect(r.left, r.top, 140.0, 18.0),
                    &p.fmt.caption,
                    &p.br.muted,
                );
                p.text(
                    &p.clip(value, &p.fmt.small, r.right - r.left - 146.0),
                    rect(r.left + 146.0, r.top, r.right - r.left - 146.0, 18.0),
                    &p.fmt.small,
                    &p.br.fg,
                );
            }
            POp::Sep { rect: r } => p.fill(*r, &p.br.grid),
            POp::Body { rect: r, text } => {
                // 多行文本：行数上限与留高同一个常量（`emit` 那一侧按它算高度），
                // 每行再自己夹宽 —— 整段一起夹会把换行当普通字符，后面的行被整段吃掉。
                p.text(
                    &clip_text_lines(
                        p,
                        text,
                        &p.fmt.caption,
                        r.right - r.left,
                        PV_TEXTAREA_MAX_LINES,
                    ),
                    *r,
                    &p.fmt.caption,
                    &p.br.muted,
                );
            }
            POp::Field {
                rect: r,
                label,
                value,
                field,
            } => {
                p.text(
                    &p.clip(label, &p.fmt.caption, 140.0),
                    rect(r.left, r.top + 4.0, 140.0, 18.0),
                    &p.fmt.caption,
                    &p.br.muted,
                );
                let vb = pv_field_value_rect(*r);
                // 可改的给强调色边框：一眼能看出这格是输入框而不是只读值
                let border = if field.is_empty() {
                    &p.br.border
                } else {
                    &p.br.accent
                };
                p.round(vb, R_CONTROL, &p.br.card, Some(border));
                p.text(
                    &p.clip(value, &p.fmt.caption, vb.right - vb.left - 16.0),
                    rect(vb.left + 8.0, vb.top + 3.0, vb.right - vb.left - 16.0, 18.0),
                    &p.fmt.caption,
                    &p.br.fg,
                );
            }
            POp::Select {
                rect: r,
                label,
                value,
                field,
                ..
            } => {
                p.text(
                    &p.clip(label, &p.fmt.caption, 140.0),
                    rect(r.left, r.top + 4.0, 140.0, 18.0),
                    &p.fmt.caption,
                    &p.br.muted,
                );
                let vb = select_value_rect(*r);
                // 可改的给强调色边框与下拉箭头；没有 field 的与只读输入框同一套画法，
                // 免得屏上看着能选、点了却不生效。
                let editable = !field.is_empty();
                let border = if editable { &p.br.accent } else { &p.br.border };
                p.round(vb, R_CONTROL, &p.br.card, Some(border));
                let value_w = vb.right - vb.left - if editable { 28.0 } else { 16.0 };
                p.text(
                    &p.clip(value, &p.fmt.caption, value_w),
                    rect(vb.left + 8.0, vb.top + 3.0, value_w, 18.0),
                    &p.fmt.caption,
                    &p.br.fg,
                );
                if editable {
                    p.text(
                        "▾",
                        rect(vb.right - 20.0, vb.top + 3.0, 16.0, 18.0),
                        &p.fmt.center_small,
                        &p.br.muted,
                    );
                }
            }
            POp::Button {
                rect: r,
                text,
                disabled,
                ..
            } => {
                if *disabled {
                    p.round(*r, R_CONTROL, &p.br.grid, None);
                } else {
                    p.round(*r, R_CONTROL, &p.br.accent, None);
                }
                // 按钮文字是插件写的：宽度按字数估、还会被可用宽夹住，超长的一串
                // 必须自己带上界（这块 UI 不按矩形裁切）。
                p.text(
                    &p.clip(text, &p.fmt.center_small, r.right - r.left - 8.0),
                    *r,
                    &p.fmt.center_small,
                    if *disabled { &p.br.muted } else { &p.br.white },
                );
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
                // 表头与数据格都是插件给的字符串，列宽又是"插件给的总宽 ÷ 列数"——
                // 五列时一格只剩 ~220 px，而一条记录名可以远超它。卡片那层
                // PushAxisAlignedClip 只在卡片边切一刀（没有省略号），中间各列照叠。
                for (i, htxt) in headers.iter().enumerate() {
                    p.text(
                        &p.clip(htxt, &p.fmt.caption, cw - 12.0),
                        rect(r.left + 6.0 + i as f32 * cw, r.top + 4.0, cw - 12.0, 16.0),
                        &p.fmt.caption,
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
                        // 选中行用"主色淡底 + 主色描边"，与表格/设置页的选中行同一套语言：
                        // 实心主色只留给"当前分段"和主操作，不在一屏里出现第三种含义。
                        p.round(rr, R_MICRO, &p.br.accent_soft, Some(&p.br.accent));
                    }
                    let ink = &p.br.fg;
                    for (ci, text) in row.iter().enumerate().take(cols) {
                        let cell_rect = rect(
                            rr.left + 6.0 + ci as f32 * cw,
                            rr.top + 2.0,
                            cw - 12.0,
                            16.0,
                        );
                        let clipped = p.clip(text, &p.fmt.small, cw - 12.0);
                        note_tooltip(tip, vs, cell_rect, text, clipped != text.as_str());
                        p.text(&clipped, cell_rect, &p.fmt.small, ink);
                    }
                }
                // 行内操作按钮
                for (br, _, label) in buttons {
                    p.round(*br, R_MICRO, &p.br.accent, None);
                    p.text(
                        &p.clip(label, &p.fmt.center_small, br.right - br.left - 6.0),
                        *br,
                        &p.fmt.center_small,
                        &p.br.white,
                    );
                }
            }
            POp::Chart {
                rect: r,
                kind,
                labels,
                points,
            } => draw_plugin_chart(p, *r, kind, labels, points),
        }
    }
    // 控件（按钮/下拉/输入框/行内按钮）的悬停叠在最上面：它们本身就是实底色，
    // 铺在底下看不出来。用圆角半透明色，形状与画它们时那一档一致。
    if let Some(h) = vs.pv_hover.filter(|h| !h.row) {
        p.round(h.rect, R_CONTROL, &p.br.hover, None);
    }
    // 正文的滚动条：从前这一页能滚但屏上没有任何指示。
    let content_top = card.top + 40.0;
    let visible = plugin_scroll_visible(h);
    let content = ops
        .iter()
        .map(|op| op.bounds().bottom)
        .fold(content_top, f32::max)
        - content_top
        + scroll as f32;
    draw_vscroll(
        p,
        rect(card.right - 11.0, content_top, 6.0, visible),
        content.ceil().max(0.0) as usize,
        visible as usize,
        scroll.max(0) as usize,
    );

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

/// 输入框的值框：`POp::Field` 的绘制、命中与行内 EDIT 三处共用同一份几何。
fn pv_field_value_rect(r: D2D_RECT_F) -> D2D_RECT_F {
    rect(r.left + 146.0, r.top, 260.0, 24.0)
}

/// 行内 EDIT 相对宿主画的那一格往里缩多少。
///
/// EDIT 控件本身没有边框（`WS_CHILD | ES_AUTOHSCROLL`），铺满整格会把宿主画的
/// 主色描边连四个圆角一起盖掉：浅色主题下白底压白底，那一格看起来像"点一下就没了"。
/// 缩两格，边框与圆角留在外面，输入框始终看得出是个输入框。
const PV_FIELD_EDIT_INSET: f32 = 2.0;

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

/// 下拉菜单里那一句空态：没有可选项时不能只画一块空卡片。
///
/// `Widget::Select` 的 `options` 来自插件（可能查空了），空菜单在屏上只是一块 32px
/// 的小白卡——与「点了没反应 / 控件坏了」分不开。与图表空数据写「暂无数据」同一条。
fn empty_select_note(options: usize) -> Option<&'static str> {
    (options == 0).then_some("没有可选项")
}

fn draw_select_menu(p: &P<'_>, w: f32, h: f32, sel: &PvSelect) {
    let (menu, items, start) = select_menu_geom(sel.anchor, sel.options.len(), sel.scroll, w, h);
    p.overlay_card(menu, R_CONTROL);
    // 空下拉不许只留一块空白小卡：那一块与"点坏了"在屏上分不开（与图表空数据那条
    // 「暂无数据」同一个口径）。
    if let Some(note) = empty_select_note(sel.options.len()) {
        p.text(
            &p.clip(note, &p.fmt.caption, menu.right - menu.left - 12.0),
            rect(
                menu.left + 6.0,
                menu.top + 4.0,
                menu.right - menu.left - 12.0,
                18.0,
            ),
            &p.fmt.caption,
            &p.br.muted,
        );
        return;
    }
    for (i, ir) in items.iter().enumerate() {
        let Some((val, label)) = sel.options.get(start + i) else {
            break;
        };
        if *val == sel.value {
            p.fill(*ir, &p.br.accent_soft);
        }
        // 选项文字来自插件（分类名是用户建的，长起来没有边）：不夹就画到菜单外面。
        p.text(
            &p.clip(label, &p.fmt.caption, ir.right - ir.left - 12.0),
            rect(ir.left + 6.0, ir.top + 3.0, ir.right - ir.left - 12.0, 18.0),
            &p.fmt.caption,
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
    /// 字段多到装不下时，最后那一行的说明（还有几项没显示）。
    fields_note: Option<(D2D_RECT_F, String)>,
    /// 底部自定义按钮：(矩形, 文字)。
    buttons: Vec<(D2D_RECT_F, String)>,
    submit: D2D_RECT_F,
    cancel: D2D_RECT_F,
}

fn modal_field_box(row: D2D_RECT_F) -> D2D_RECT_F {
    let width = (row.right - row.left - 146.0).min(300.0);
    rect(row.left + 146.0, row.top + 3.0, width, 26.0)
}

/// 弹窗正文一行的高度，以及卡片里除正文外那几块的固定高度：几何与行数上限共用。
/// （`modal_geom` 的 `58 + content_h + fields + buttons + 52` 与这张表同源。）
const MODAL_LINE_H: f32 = 18.0;
const MODAL_HEAD_H: f32 = 58.0;
const MODAL_CONTENT_GAP_H: f32 = 10.0;
const MODAL_FIELDS_ROW_H: f32 = 38.0;
const MODAL_BUTTONS_H: f32 = 36.0;
const MODAL_BOTTOM_H: f32 = 52.0;
/// 弹窗高度最多到 `窗口高 - 这个边距`。
const MODAL_WINDOW_MARGIN: f32 = 60.0;

/// 弹窗正文最多排几行：以**窗口放得下**为准，而不是一个写死的 8。
/// 分类明细那种"行越多越说明记得多"的内容，写死上限会把后半截直接藏掉
/// （从前是画出去压按钮，修完绘制溢出之后也不能换成"只显示前 8 行"）。
fn modal_content_cap(m: &ModalRef<'_>, h: f32) -> usize {
    let fields_h = m.fields.len() as f32 * MODAL_FIELDS_ROW_H;
    let buttons_h = if m.buttons.is_empty() {
        0.0
    } else {
        MODAL_BUTTONS_H
    };
    let fixed = MODAL_HEAD_H + MODAL_CONTENT_GAP_H + fields_h + buttons_h + MODAL_BOTTOM_H;
    let avail = (h - MODAL_WINDOW_MARGIN) - fixed;
    (avail / MODAL_LINE_H).floor().max(1.0) as usize
}

/// 滚动文本区最多画几行：与弹窗正文同一套口径（高度与绘制读同一个常量）。
const PV_TEXTAREA_MAX_LINES: usize = 5;

/// 多行动态文本实际要排出来的那几行。行数一旦超过 `max`，最后一行让给
/// 「还有几行没显示」—— 悄悄少画等于让用户以为内容就这么多，而分类明细恰好
/// 是"越多行越说明本月记得多"的那一类内容。
fn capped_lines(content: &str, max: usize) -> Vec<String> {
    let all: Vec<&str> = content.lines().collect();
    let max = max.max(1);
    if all.len() <= max {
        return all.into_iter().map(str::to_string).collect();
    }
    let kept = max - 1;
    let mut out: Vec<String> = all[..kept].iter().map(|s| s.to_string()).collect();
    out.push(format!("…（还有 {} 行没显示）", all.len() - kept));
    out
}

fn modal_geom(m: &ModalRef<'_>, w: f32, h: f32) -> ModalGeom {
    let pad = 20.0;
    let dw = (w - 64.0).clamp(380.0, 560.0);
    let lines = if m.content.is_empty() {
        0
    } else {
        m.content.lines().count().min(modal_content_cap(m, h))
    };
    let content_h = if lines == 0 {
        0.0
    } else {
        lines as f32 * MODAL_LINE_H + MODAL_CONTENT_GAP_H
    };
    let fields_h = m.fields.len() as f32 * MODAL_FIELDS_ROW_H;
    let buttons_h = if m.buttons.is_empty() {
        0.0
    } else {
        MODAL_BUTTONS_H
    };
    let dh = (MODAL_HEAD_H + content_h + fields_h + buttons_h + MODAL_BOTTOM_H)
        .min(h - MODAL_WINDOW_MARGIN);
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
            lines as f32 * MODAL_LINE_H,
        ))
    };
    let mut y = dialog.top + 52.0 + content_h;
    // 字段是插件给的、数量没有上限，而 `dh` 被窗口高度夹住：从前照着
    // `m.fields.len()` 一路往下排，多出来的行画到「取消/确定」底下、再画到卡片外面。
    // 装得下几行就画几行，最后一行让给「还有 N 项没显示」——与正文 `capped_lines`
    // 同一套口径：可以少画，不许悄悄少画。
    let bottom_row_top = dialog.bottom - 48.0;
    let room = ((bottom_row_top - y) / MODAL_FIELDS_ROW_H).floor().max(0.0) as usize;
    let (shown, hidden) = if m.fields.len() <= room {
        (m.fields.len(), 0)
    } else {
        // 一行都放不下时连说明也没有落脚处：这时没有"说了也看得见"的选项。
        let keep = room.saturating_sub(1);
        (keep, m.fields.len() - keep)
    };
    let mut fields = Vec::new();
    for _ in 0..shown {
        fields.push(rect(dialog.left + pad, y, dw - pad * 2.0, 32.0));
        y += MODAL_FIELDS_ROW_H;
    }
    let fields_note = if hidden == 0 || shown == 0 {
        // 一行都放不下时连说明也没有落脚处（写在动作行上等于盖住按钮）：
        // 这一档只剩"窗口太矮"，不是字段太多。
        None
    } else {
        Some((
            rect(dialog.left + pad, y, dw - pad * 2.0, 30.0),
            format!("…（还有 {hidden} 项没显示）"),
        ))
    };
    let submit = rect(dialog.right - pad - 88.0, dialog.bottom - 48.0, 88.0, 28.0);
    let cancel = rect(submit.left - 8.0 - 72.0, dialog.bottom - 48.0, 72.0, 28.0);
    // 自定义按钮排在左半边。右边是「取消 / 确定」，而命中先问它们：伸过去的那一颗
    // 画得出来却永远点不到（按下去归了取消/确定）。宽度先按剩余空间均分，再夹自然宽；
    // 文字那一侧已经逐格夹过，被压窄也不会画到按钮外面。
    let start = dialog.left + pad;
    let row_right = if m.cancel.is_empty() {
        submit.left
    } else {
        cancel.left
    } - 8.0;
    let n = m.buttons.len() as f32;
    let gap = 8.0;
    let unit = if n > 0.0 {
        ((row_right - start - gap * (n - 1.0)) / n).max(28.0)
    } else {
        0.0
    };
    let mut buttons = Vec::new();
    let mut bx = start;
    for (_, label) in m.buttons {
        let natural = (label.chars().count() as f32 * 14.0 + 20.0).max(64.0);
        let bw = natural.min(unit).max(28.0);
        buttons.push((rect(bx, dialog.bottom - 48.0, bw, 28.0), label.clone()));
        bx += bw + gap;
    }
    ModalGeom {
        dialog,
        close,
        content,
        fields,
        fields_note,
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
    p.overlay_card(g.dialog, R_MODAL);
    p.text(
        // 标题是插件写的，弹窗宽夹在 380..560 之间 —— 长标题会一路画到右上角那颗「✕」上。
        &p.clip(m.title, &p.fmt.body, g.dialog.right - g.dialog.left - 72.0),
        rect(
            g.dialog.left + 20.0,
            g.dialog.top + 16.0,
            g.dialog.right - g.dialog.left - 72.0,
            22.0,
        ),
        &p.fmt.body,
        &p.br.fg,
    );
    p.round(g.close, R_CONTROL, &p.br.header, None);
    p.text("✕", g.close, &p.fmt.center_small, &p.br.muted);

    if let Some(cr) = g.content {
        // 每一行自己夹：正文是插件写的，长分类名／错误原因会一路画到卡片外面。
        let text = clip_text_lines(
            p,
            m.content,
            &p.fmt.caption,
            cr.right - cr.left,
            modal_content_cap(m, h),
        );
        p.text(&text, cr, &p.fmt.caption, &p.br.muted);
    }

    for (i, f) in m.fields.iter().enumerate() {
        let Some(row) = g.fields.get(i) else { break };
        p.text(
            // 弹窗里这两格也是插件给的字符串：标签 136 px，值框右边还要留给下拉箭头。
            &p.clip(&f.label, &p.fmt.caption, 136.0),
            rect(row.left, row.top + 7.0, 136.0, 18.0),
            &p.fmt.caption,
            &p.br.muted,
        );
        let vb = modal_field_box(*row);
        p.round(vb, R_CONTROL, &p.br.card, Some(&p.br.border));
        p.text(
            &p.clip(&f.value, &p.fmt.caption, vb.right - vb.left - 28.0),
            rect(vb.left + 8.0, vb.top + 3.0, vb.right - vb.left - 28.0, 18.0),
            &p.fmt.caption,
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

    if let Some((nr, note)) = &g.fields_note {
        // 字段没画全是插件那份表单超出窗口兜不住的情况：与正文同一句口径，明说还有几项。
        p.text(
            &p.clip(note, &p.fmt.caption, nr.right - nr.left),
            *nr,
            &p.fmt.caption,
            &p.br.muted,
        );
    }

    for (br, label) in &g.buttons {
        p.round(*br, R_CONTROL, &p.br.header, Some(&p.br.border));
        // 按钮文字来自插件：多颗长标签会互相压住（宽度按字数估，没有"装不下就不画"）。
        p.text(
            &p.clip(label, &p.fmt.center_small, br.right - br.left - 8.0),
            *br,
            &p.fmt.center_small,
            &p.br.fg,
        );
    }
    if !m.cancel.is_empty() {
        p.round(g.cancel, R_CONTROL, &p.br.header, Some(&p.br.border));
        p.text("取消", g.cancel, &p.fmt.center_small, &p.br.fg);
    }
    p.round(g.submit, R_CONTROL, &p.br.accent, None);
    p.text(
        &p.clip(
            if m.submit_text.is_empty() {
                "确定"
            } else {
                m.submit_text
            },
            &p.fmt.center_small,
            g.submit.right - g.submit.left - 8.0,
        ),
        g.submit,
        &p.fmt.center_small,
        &p.br.white,
    );
}

// ===== 插件 =====

/// 插件页那一列的几何。
///
/// ⚠ 第三个读数 `max_rows` 数的是**格子**（表头 + 行），不是行数 —— 两栏分组之后
/// 一屏能装的行数少了最多两格。三个读者（画 / 悬停 / 点击 / 滚轮上限）都必须按
/// [`plugin_slots`] 的那张表来数，各数各的就会"亮一行、点另一行"。
fn plugin_geom(_w: f32, h: f32) -> (f32, f32, usize) {
    let list_top = 208.0 + 44.0;
    let row_h = 46.0;
    let max_rows = ((((h - 32.0) - list_top) / row_h).floor() as usize).max(1);
    (list_top, row_h, max_rows)
}

/// 插件清单里一行可画 / 可悬停 / 可点的矩形。
///
/// 三个读者共用这一份：从前悬停只判 `y`，指针落在卡片左右 16px 的边距里也会亮起
/// 那一行 —— "亮的区域"与"点得到的区域"不是同一个。左右边界由卡片的内边距决定
/// （卡片从 16 起、内容再缩 16），与 [`plugin_geom`] 的纵向几何各管一维。
fn plugin_row_rect(w: f32, y: f32, row_h: f32) -> D2D_RECT_F {
    rect(32.0, y, w - 64.0, row_h - 4.0)
}

/// 插件清单里指针落在哪一行（返回 `(清单行下标, 屏上第几格)`）。
///
/// 表头占一格但不能点，与 [`plugin_slots`] 那张表同一口径：少算这一格，
/// 鼠标停在 N 行、亮/点的却是 N+1 行。
#[allow(clippy::too_many_arguments)] // 七个读数都是画那一列自己的几何，拆开传反而多一层转手
fn plugin_list_row_at(
    slots: &[PluginSlot],
    list_top: f32,
    row_h: f32,
    max_rows: usize,
    w: f32,
    scroll: usize,
    x: f32,
    y: f32,
) -> Option<(usize, usize)> {
    let scroll = scroll.min(slots.len().saturating_sub(max_rows));
    for vis in 0..max_rows {
        let Some(slot) = slots.get(scroll + vis) else {
            break;
        };
        let PluginSlot::Row(idx) = *slot else {
            continue;
        };
        let y0 = list_top + vis as f32 * row_h;
        if hit(plugin_row_rect(w, y0, row_h), x, y) {
            return Some((idx, vis));
        }
    }
    None
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

/// 「授权」那颗按钮的位置：画它的这一头与点它的（`on_click` 里的命中）那一头共用这一个表达式。
///
/// 两处各写一遍是本仓反复出现过的那类缺陷：画在 A、命中在 B ⇒ 屏上有一颗看得见的按钮
/// 点下去没反应，而没有任何一条用例说得清它为什么不动。摆在启用开关**左边**，
/// 同一行同一高度，用户视线不用跳。
fn plugin_grant_rect(row_right: f32, y: f32) -> D2D_RECT_F {
    rect(row_right - 76.0 - 66.0, y + 9.0, 58.0, 24.0)
}

/// 「详情」那颗按钮的位置：画它的这一头与点它的那一头共用这一个表达式。
///
/// 位置**不跟着授权按钮跳**：授权只在两档出现，这一颗每一行都有 —— 让它在两种行里
/// 摆两个位置，用户第二次找它就得重新找一遍。空出来的那一格留着，也是给"这一行有没有
/// 授权要办"留的一个固定读法。
///
/// 它也是这一页最左边的那颗按钮，第二行正文的宽度按它的左沿收（见 `draw_plugins`）。
fn plugin_detail_rect(row_right: f32, y: f32) -> D2D_RECT_F {
    rect(row_right - 76.0 - 66.0 - 66.0, y + 9.0, 58.0, 24.0)
}

/// 插件页右上角的「安装插件包…」按钮。画与命中读同一个表达式。
fn plugin_install_rect(card: D2D_RECT_F) -> D2D_RECT_F {
    rect(card.right - 16.0 - 132.0, card.top + 8.0, 132.0, 26.0)
}

/// 详情页右上角的「卸载…」按钮（只在有安装记录的那一档画）。
fn detail_uninstall_rect(card: D2D_RECT_F) -> D2D_RECT_F {
    rect(card.right - 16.0 - 88.0, card.top + 10.0, 88.0, 26.0)
}

/// 卸载确认框：框体 + 三颗按钮（左起：保留设置 / 删除设置 / 取消）。
///
/// 三颗按钮的宽度与间距写在这里一处 —— 画与命中都读它，错一位就是"点取消删了设置"。
fn uninstall_prompt_geometry(w: f32, h: f32) -> (D2D_RECT_F, [D2D_RECT_F; 3]) {
    let width = 460.0;
    let height = 190.0;
    let box_ = rect((w - width) / 2.0, (h - height) / 2.0, width, height);
    let bw = 132.0;
    let gap = 12.0;
    let total = bw * 3.0 + gap * 2.0;
    let left = box_.left + (width - total) / 2.0;
    let y = box_.bottom - 16.0 - 28.0;
    let buttons = [
        rect(left, y, bw, 28.0),
        rect(left + bw + gap, y, bw, 28.0),
        rect(left + (bw + gap) * 2.0, y, bw, 28.0),
    ];
    (box_, buttons)
}

/// 「清理 30 天前」的确认框：框体 + 两颗按钮（左起：取消 / 清理）。
///
/// 左边那颗是**默认结局**：遮罩、Esc、切页都走取消，只有显式点右边才真的删。
/// 与卸载确认同一条道理 —— 不可撤销那一档不该由"手一抖"完成。
fn cleanup_prompt_geometry(w: f32, h: f32) -> (D2D_RECT_F, [D2D_RECT_F; 2]) {
    let width = 460.0;
    let height = 170.0;
    let box_ = rect((w - width) / 2.0, (h - height) / 2.0, width, height);
    let bw = 132.0;
    let gap = 12.0;
    let left = box_.left + (width - (bw * 2.0 + gap)) / 2.0;
    let y = box_.bottom - 16.0 - 28.0;
    let buttons = [rect(left, y, bw, 28.0), rect(left + bw + gap, y, bw, 28.0)];
    (box_, buttons)
}

/// 确认框里这一下点的是不是"真的清理"那颗。框外、框内空白、取消都返回 false。
fn cleanup_choice_at(_dialog: D2D_RECT_F, buttons: [D2D_RECT_F; 2], x: f32, y: f32) -> bool {
    hit(buttons[1], x, y)
}

/// 安装那一次的结论那一句（纯函数：worker 线程拼好，GUI 只显示）。
fn install_line(
    result: std::result::Result<focusflow_core::plugins::install::Installed, String>,
) -> String {
    match result {
        Err(e) => format!("安装失败：{e}"),
        Ok(done) => {
            let mut line = format!(
                "{}：{}（{} 个文件，{} 字节）",
                if done.replaced {
                    "已更新"
                } else {
                    "已安装"
                },
                done.stem,
                done.entries,
                done.bytes
            );
            if done.problems > 0 {
                line.push_str(&format!(
                    "；校验器对这份包说了 {} 条问题（不阻塞安装）",
                    done.problems
                ));
            }
            line.push_str("；重启程序（或停用再启用）之后加载");
            line
        }
    }
}

/// 卸载那一次的结论那一句。三档数据结局各说各的。
fn uninstall_line(
    result: std::result::Result<focusflow_core::plugins::install::Uninstalled, String>,
) -> String {
    use focusflow_core::plugins::install::StateAfter;
    match result {
        Err(e) => format!("卸载失败：{e}"),
        Ok(done) => match done.state {
            StateAfter::Kept(path) => format!(
                "已卸载 {}（包已删除），设置还在 {}",
                done.stem,
                path.display()
            ),
            StateAfter::Purged => format!("已卸载 {}（包与设置都删了）", done.stem),
            StateAfter::PurgeFailed { path, why } => format!(
                "已卸载 {}（包已删除）；设置目录删不干净（{}：{why}）—— 设置还留在原处",
                done.stem,
                path.display()
            ),
            StateAfter::NeverExisted => format!("已卸载 {}（它没有留下设置目录）", done.stem),
        },
    }
}

fn draw_plugins(
    p: &P<'_>,
    w: f32,
    h: f32,
    rows: &[PluginRow],
    err: Option<&str>,
    vs: ViewState,
    tip: &mut Option<(String, D2D_RECT_F)>,
) {
    let card = rect(16.0, 208.0, w - 32.0, h - 208.0 - 16.0);
    p.card(card, R_CARD);
    p.text(
        "插件",
        rect(card.left + 16.0, card.top + 12.0, 80.0, 22.0),
        &p.fmt.body,
        &p.br.fg,
    );
    // 这一行说的必须是清单真正来自的那几处：抄一份"内置 + 数据目录"的字面量，
    // 用户开着回滚开关时就会照屏上那个目录放插件、放下去不加载。
    let dirs = focusflow_core::paths::plugin_scan_dirs(
        focusflow_core::config::instance().get_bool("plugins", "legacy_layout", false),
    )
    .iter()
    .map(|d| d.display().to_string())
    .collect::<Vec<_>>()
    .join(" / ");
    let cap_mb =
        focusflow_core::plugins::manager::memory_limit_mb(focusflow_core::config::instance())
            as u64;
    // 这一格的宽度：左边让给「插件」那两个字，右边让给安装按钮（148 + 12 的缝）。
    // 目录路径的尾巴才是"到底在哪儿"，所以 `p.clip` 夹中间、两头都留。
    let heading_w = card.right - card.left - 32.0 - 58.0 - 160.0;
    p.text(
        &p.clip(
            &plugin_heading(rows, cap_mb, &dirs),
            &p.fmt.caption,
            heading_w,
        ),
        rect(card.left + 58.0, card.top + 15.0, heading_w, 18.0),
        &p.fmt.caption,
        &p.br.muted,
    );
    // 安装入口的按钮位置固定在这一页右上角：与命中读同一份几何。
    let install = plugin_install_rect(card);
    p.round(install, R_CONTROL, &p.br.header, Some(&p.br.border));
    p.text("安装插件包…", install, &p.fmt.center_small, &p.br.fg);

    if let Some(e) = err {
        p.text(
            &p.clip(
                &format!("插件目录读不出来：{e}"),
                &p.fmt.caption,
                card.right - card.left - 32.0,
            ),
            rect(
                card.left + 16.0,
                card.top + 36.0,
                card.right - card.left - 32.0,
                18.0,
            ),
            &p.fmt.caption,
            &p.br.danger,
        );
        return;
    }

    let (list_top, row_h, max_rows) = plugin_geom(w, h);
    // 一列格子 = 两栏表头 + 各行（[`plugin_slots`]）。滚动与命中都按**格子**走，
    // 而 `vs.sel` / `vs.hover` 记的还是行号 —— 表头不能被选中，也不该顶掉一行的名额。
    let slots = plugin_slots(rows);
    let scroll = vs.scroll.min(slots.len().saturating_sub(max_rows));
    for vis in 0..max_rows {
        let Some(slot) = slots.get(scroll + vis) else {
            break;
        };
        let y = list_top + vis as f32 * row_h;
        let idx = match slot {
            PluginSlot::Header(title) => {
                p.text(
                    title,
                    rect(
                        card.left + 16.0,
                        y + 4.0,
                        card.right - card.left - 32.0,
                        18.0,
                    ),
                    &p.fmt.caption,
                    &p.br.muted,
                );
                continue;
            }
            PluginSlot::Row(i) => *i,
        };
        let row = &rows[idx];
        let r = plugin_row_rect(w, y, row_h);
        if vs.sel == Some(idx) {
            p.round(r, R_CONTROL, &p.br.accent_soft, None);
        } else if vs.hover == Some(idx) {
            p.fill(r, &p.br.row_hover);
        }
        let name_cell = rect(r.left + 8.0, y + 5.0, 240.0, 18.0);
        let clipped = p.clip(&row.name, &p.fmt.body, 240.0);
        note_tooltip(tip, vs, name_cell, &row.name, clipped != row.name);
        p.text(&clipped, name_cell, &p.fmt.body, &p.br.fg);
        p.text(
            &p.clip(
                &format!("v{} · {}", row.version, row.author),
                &p.fmt.caption,
                260.0,
            ),
            rect(r.left + 250.0, y + 7.0, 260.0, 16.0),
            &p.fmt.caption,
            &p.br.muted,
        );
        // 声明那一格：宽度与画它的矩形取同一个表达式（这一页的字体全是 NO_WRAP，
        // 而 `DrawText` 不按矩形裁切 —— 不夹就是画到窗口外）。右边留的是**最左那颗
        // 按钮**（详情）的左沿，不是开关的左沿：留成 92 时这一段会画到按钮底下。
        let decl_w = r.right - (r.left + 520.0) - 216.0;
        let decl = row.declaration();
        if !decl.is_empty() && decl_w > 20.0 {
            p.text(
                &p.clip(&decl, &p.fmt.caption, decl_w),
                rect(r.left + 520.0, y + 7.0, decl_w, 16.0),
                &p.fmt.caption,
                &p.br.muted,
            );
        }
        // 启用开关（只画；命中在 on_click 里用同一份几何）
        let vb = rect(r.right - 76.0, y + 9.0, 60.0, 24.0);
        p.round(
            vb,
            R_CONTROL,
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
        // 「授权」那颗只在真需要点的那两档出现（没授权过 / 权限变了）：给内置模块或
        // 已经授权过的插件画一颗点了没用的按钮，是这一页最容易让人怀疑"是不是坏了"的东西。
        if row.needs_grant {
            let gb = plugin_grant_rect(r.right, y);
            p.round(gb, R_CONTROL, &p.br.danger, None);
            p.text("授权", gb, &p.fmt.center_small, &p.br.white);
        }
        // 「详情」每一行都有：四类钩子的耗时与错误历史只在那一页看得到，而它不加载插件
        // （读的是台账与清单），所以给停用/加载失败的格子也留一条入口。
        let db = plugin_detail_rect(r.right, y);
        p.round(db, R_CONTROL, &p.br.header, Some(&p.br.border));
        p.text("详情", db, &p.fmt.center_small, &p.br.fg);
        // 第二行：坏消息说坏消息，否则说它是什么。
        // 分支收在 [`PluginRow::second_line`] 里（这一格与画它的矩形取同一个宽度表达式）——
        // `one_line` 那个 72 是"多少个字符"，不是"这一格装得下多少"：72 个汉字约 900 px，
        // 比格子宽一倍，所以夹取必须按 desc_w 算。
        // 第二行与声明那一格共用同一条右界（最左那颗按钮的左沿）：只留开关那 92 的话，
        // 一句长原因会画到「详情」「授权」两颗按钮底下 —— 这一条在加详情那颗时就该并进来。
        let desc_w = r.right - r.left - 216.0;
        let (line, bad) = row.second_line();
        p.text(
            &p.clip(&line, &p.fmt.caption, desc_w),
            rect(r.left + 8.0, y + 23.0, desc_w, 16.0),
            &p.fmt.caption,
            if bad { &p.br.danger } else { &p.br.muted },
        );
    }

    draw_vscroll(
        p,
        rect(w - 32.0 - 11.0, list_top, 6.0, max_rows as f32 * row_h),
        slots.len(),
        max_rows,
        scroll,
    );

    if rows.is_empty() {
        p.text(
            // 说的是**用户能写的那一格**：内置那一处随包发布、也不该由用户改。
            // 具体路径页顶那一行已经印着（与清单同一把尺子 `plugin_scan_dirs`），
            // 这一句只指方向，别在这里抄第二份路径。
            &p.clip(
                // 两条入口都说出来：右上角那颗按钮是"装进来的包"，`.lua` 是"手放的脚本"。
                &format!(
                    "还没有插件：点右上角安装插件包，或把一个 .lua 放进 {}",
                    focusflow_core::paths::plugins_user_dir().display()
                ),
                &p.fmt.center_small,
                card.right - card.left,
            ),
            rect(card.left, card.top + 120.0, card.right - card.left, 20.0),
            &p.fmt.center_small,
            &p.br.muted,
        );
    }
}

/// 插件页顶上那句话：「N 个 · 已加载 M 个常驻（每格内存上限 X MB）· 目录 …」。
///
/// 只报**数得清**的两样：常驻里**真装上了**的个数，与每格的配置上限。**不印合计占用** ——
/// mlua 只有 `set_memory_limit`、没有"这个 VM 此刻占多少"的读数；按进程私有字节摊出来的
/// 那个数不是每插件的事实，拿它当读数等于把估算印成测量（方案 ADR-10）。
fn plugin_heading(rows: &[PluginRow], cap_mb: u64, dirs: &str) -> String {
    let residents = rows.iter().filter(|r| r.resident && r.loaded).count();
    format!(
        "{} 个 · 已加载 {residents} 个常驻（每格内存上限 {cap_mb} MB） · 目录 {dirs}",
        rows.len()
    )
}

/// 详情页四类钩子那张表的表头 y（相对卡片顶边）与行高。
///
/// 抽成常量是为了让"表最后一行压到错误历史标题上"这种只在像素层看得出来的错，
/// 有一条测试能直接核的先后关系（见 `plugin_page_tests`）。
const DETAIL_TABLE_HEAD_Y: f32 = 150.0;
const DETAIL_ROW_H: f32 = 22.0;
const DETAIL_ERRORS_TITLE_Y: f32 = 266.0;
const DETAIL_ERRORS_TOP: f32 = 290.0;

/// 插件详情页：权限 / 占用 / 四类钩子耗时 / 错误历史（方案 21 步）。
///
/// 三条约定：
/// · 一个数都不在这里算 —— `detail` 是 core 的 `plugin_health::detail_for` 排好的那一份，
///   同一份账在行上与这一页才不会念成两个数；
/// · 每一格按**像素**再夹一次：core 那一把管"多少个字符"，这一把管"这一格画得下多少"
///   （本页字体全是 `NO_WRAP`，`DrawText` 不按矩形裁切）；
/// · 这一页**不加载插件**：它读的是台账与清单，所以停用、加载失败的格子也进得来。
fn draw_plugin_detail(
    p: &P<'_>,
    w: f32,
    h: f32,
    row: &PluginRow,
    source: Option<&str>,
    installed: bool,
    detail: &focusflow_core::plugins::plugin_health::PluginDetail,
) {
    let card = rect(16.0, 208.0, w - 32.0, h - 208.0 - 16.0);
    p.card(card, R_CARD);

    let back = rect(card.left + 16.0, card.top + 12.0, 64.0, 24.0);
    p.round(back, R_CONTROL, &p.br.header, Some(&p.br.border));
    p.text("← 返回", back, &p.fmt.center_small, &p.br.fg);
    // 有安装记录 = 盘上有一份包可卸；手工放进目录的单文件插件不画这颗
    // （`uninstall` 也只收拾安装进来的那一类）。
    if installed {
        let ub = detail_uninstall_rect(card);
        p.round(ub, R_CONTROL, &p.br.danger, None);
        p.text("卸载…", ub, &p.fmt.center_small, &p.br.white);
    }
    p.text(
        "插件详情",
        rect(back.right + 12.0, card.top + 14.0, 120.0, 20.0),
        &p.fmt.body,
        &p.br.fg,
    );

    let x = card.left + 16.0;
    let inner_w = card.right - card.left - 32.0;

    // 名字/版本/作者都是插件自己写的，长度不受控，所以按像素夹。
    let title = p.clip(
        &format!("{} · v{} · {}", row.name, row.version, row.author),
        &p.fmt.body,
        inner_w,
    );
    p.text(
        &title,
        rect(x, card.top + 44.0, inner_w, 20.0),
        &p.fmt.body,
        &p.br.fg,
    );

    // 第二行：身份、声明、占用、授权结论 —— 全部读 core 交回的那份清单。
    let mut meta: Vec<String> = vec![
        format!("API {}", row.api),
        if row.builtin {
            "内置模块".to_string()
        } else {
            "扩展插件".to_string()
        },
        if row.resident {
            "常驻".to_string()
        } else {
            "随面板".to_string()
        },
    ];
    if !row.enabled {
        meta.push("已停用".to_string());
    }
    if row.permissions > 0 {
        meta.push(if row.permission_names.is_empty() {
            format!("权限 {} 项", row.permissions)
        } else {
            format!(
                "权限 {} 项：{}",
                row.permissions,
                row.permission_names.join("、")
            )
        });
    }
    if let Some(n) = row.store_bytes {
        meta.push(if n < 1024 {
            format!("占用 {n} B")
        } else {
            format!("占用 {} KB", n.div_ceil(1024))
        });
    }
    // 内置那一档的 `grant_note` 是「内置模块，免授权」：身份那一格已经说了「内置模块」，
    // 照抄会念成「内置模块 · 常驻 · 内置模块，免授权」。这里只留它多出来的那半句。
    meta.push(if row.builtin {
        "免授权".to_string()
    } else {
        row.grant_note.clone()
    });
    let meta = p.clip(&meta.join(" · "), &p.fmt.caption, inner_w);
    p.text(
        &meta,
        rect(x, card.top + 68.0, inner_w, 16.0),
        &p.fmt.caption,
        &p.br.muted,
    );

    // 来源那一句（P2 第 19 步）：包从哪来、装进来的那一串字节的校验和前几位。
    // 三档（有记录 / 手放的 / 记录坏了）由 core 的 `source_line` 说，页面只画。
    if let Some(source) = source {
        p.text(
            &p.clip(source, &p.fmt.caption, inner_w),
            rect(x, card.top + 88.0, inner_w, 16.0),
            &p.fmt.caption,
            &p.br.muted,
        );
    }

    // 失败/停用/降频的计数；正文在下面那一栏。
    let note = p.clip(&detail.note, &p.fmt.caption, inner_w);
    p.text(
        &note,
        rect(x, card.top + 110.0, inner_w, 16.0),
        &p.fmt.caption,
        if detail.has_trouble {
            &p.br.danger
        } else {
            &p.br.muted
        },
    );

    // --- 四类钩子那张表 ---
    p.text(
        "四类钩子",
        rect(x, card.top + 128.0, 200.0, 18.0),
        &p.fmt.body,
        &p.br.fg,
    );
    let window_note = p.clip(&detail.window_note, &p.fmt.caption, inner_w - 90.0);
    p.text(
        &window_note,
        rect(x + 90.0, card.top + 130.0, inner_w - 90.0, 16.0),
        &p.fmt.caption,
        &p.br.muted,
    );

    // 六列的宽度：表头与每一行读同一个数组，两处各写一套就会让数字错列。
    let head_y = card.top + DETAIL_TABLE_HEAD_Y;
    let mut col_w: Vec<f32> = vec![120.0, 90.0, 90.0, 90.0, 100.0];
    let taken: f32 = col_w.iter().sum();
    col_w.push((inner_w - taken).max(60.0));
    let mut cx = x;
    for (i, label) in ["钩子", "次数", "最近", "p95", "超预算", "预算"]
        .into_iter()
        .enumerate()
    {
        p.text(
            &p.clip(label, &p.fmt.caption, col_w[i]),
            rect(cx, head_y, col_w[i], 16.0),
            &p.fmt.caption,
            &p.br.muted,
        );
        cx += col_w[i];
    }
    for (i, hr) in detail.hooks.iter().enumerate() {
        // 四行恒在（[`PluginDetail::hooks`] 一一对应四档）：少画一档，屏上读成"没跑过"。
        let y = head_y + 20.0 + i as f32 * DETAIL_ROW_H;
        let cells = [
            &hr.label, &hr.calls, &hr.last, &hr.p95, &hr.over, &hr.budget,
        ];
        let mut cx = x;
        for (j, cell) in cells.into_iter().enumerate() {
            let brush = if j == 4 && hr.over_flag {
                &p.br.danger
            } else if j == 0 {
                &p.br.fg
            } else {
                &p.br.muted
            };
            p.text(
                &p.clip(cell, &p.fmt.caption, col_w[j]),
                rect(cx, y, col_w[j], 16.0),
                &p.fmt.caption,
                brush,
            );
            cx += col_w[j];
        }
    }

    // --- 错误历史：新的在最上面 ---
    p.text(
        "错误历史（新的在最上面）",
        rect(x, card.top + DETAIL_ERRORS_TITLE_Y, inner_w, 18.0),
        &p.fmt.body,
        &p.br.fg,
    );
    // 行上那一格不只有"加载失败"：按键/视图/事件某一路被熔断停掉也写在这里，
    // 所以前缀不能写成「加载失败」。它不经过台账（加载失败时 VM 都没起来），
    // 单独占最前面一条；不写的话这一页只念台账，作者看到的就是"没有错误记录"。
    let mut lines: Vec<String> = Vec::new();
    if let Some(e) = &row.error {
        lines.push(format!("最近报错 · {}", one_line(e, 108)));
    }
    lines.extend(detail.errors.iter().cloned());
    if lines.is_empty() {
        p.text(
            "没有错误记录",
            rect(x, card.top + DETAIL_ERRORS_TOP, inner_w, 16.0),
            &p.fmt.caption,
            &p.br.muted,
        );
    } else {
        let room = ((card.bottom - 8.0 - (card.top + DETAIL_ERRORS_TOP)) / 18.0)
            .floor()
            .max(0.0) as usize;
        // 装不下时留最后一格说"还有几条"：悄悄少画等于让用户以为只有这几条。
        let shown = if lines.len() > room {
            room.saturating_sub(1)
        } else {
            lines.len()
        };
        for (i, line) in lines.iter().take(shown).enumerate() {
            p.text(
                &p.clip(line, &p.fmt.caption, inner_w),
                rect(
                    x,
                    card.top + DETAIL_ERRORS_TOP + i as f32 * 18.0,
                    inner_w,
                    16.0,
                ),
                &p.fmt.caption,
                &p.br.danger,
            );
        }
        if lines.len() > shown {
            let more = format!(
                "还有 {} 条没画出来（窗口高一些就能看全）",
                lines.len() - shown
            );
            p.text(
                &p.clip(&more, &p.fmt.caption, inner_w),
                rect(
                    x,
                    card.top + DETAIL_ERRORS_TOP + shown as f32 * 18.0,
                    inner_w,
                    16.0,
                ),
                &p.fmt.caption,
                &p.br.muted,
            );
        }
    }
}

/// 卸载确认框：遮罩 + 一张小卡 + 三颗按钮。
///
/// 三档结局写在按钮上（保留 / 删除 / 取消）—— 卸载时"问一句"是本方案第 17 步的硬要求，
/// 而把"删设置"做成默认那一颗是这一族里最常见的误删来源。
fn draw_uninstall_prompt(p: &P<'_>, w: f32, h: f32, prompt: &UninstallPrompt) {
    p.fill(rect(0.0, 0.0, w, h), &p.br.overlay);
    let (dialog, buttons) = uninstall_prompt_geometry(w, h);
    p.overlay_card(dialog, R_MODAL);

    let x = dialog.left + 20.0;
    let text_w = dialog.right - x - 20.0;
    p.text(
        &p.clip(&format!("卸载「{}」？", prompt.name), &p.fmt.body, text_w),
        rect(x, dialog.top + 16.0, text_w, 22.0),
        &p.fmt.body,
        &p.br.fg,
    );
    p.text(
        // 这两句要**整句画得下**：框内宽 420 px，`NO_WRAP` + 不按矩形裁切，
        // 夹中间会留下一个没闭合的括号（这一版第一张屏证就是这样）。
        &p.clip(
            "插件包会被删除；设置可以留下，也可以一并删掉。",
            &p.fmt.caption,
            text_w,
        ),
        rect(x, dialog.top + 46.0, text_w, 18.0),
        &p.fmt.caption,
        &p.br.muted,
    );
    p.text(
        &p.clip("「一并删除」不可恢复；默认是留下。", &p.fmt.caption, text_w),
        rect(x, dialog.top + 68.0, text_w, 18.0),
        &p.fmt.caption,
        &p.br.muted,
    );

    let labels = ["卸载，保留设置", "卸载并删除设置", "取消"];
    for (i, label) in labels.iter().enumerate() {
        let (bg, fg) = match i {
            1 => (&p.br.danger, &p.br.white),
            _ => (&p.br.header, &p.br.fg),
        };
        p.round(buttons[i], R_CONTROL, bg, Some(&p.br.border));
        p.text(label, buttons[i], &p.fmt.center_small, fg);
    }
}

/// 「清理 30 天前」确认框：遮罩 + 一张小卡 + 两颗按钮（取消在左，是默认结局）。
fn draw_cleanup_prompt(p: &P<'_>, w: f32, h: f32) {
    p.fill(rect(0.0, 0.0, w, h), &p.br.overlay);
    let (dialog, buttons) = cleanup_prompt_geometry(w, h);
    p.overlay_card(dialog, R_MODAL);

    let x = dialog.left + 20.0;
    let text_w = dialog.right - x - 20.0;
    p.text(
        &p.clip("清理 30 天以前的明细？", &p.fmt.body, text_w),
        rect(x, dialog.top + 16.0, text_w, 22.0),
        &p.fmt.body,
        &p.br.fg,
    );
    p.text(
        &p.clip(
            "只删 30 天以前的明细，现在的数据不动。",
            &p.fmt.caption,
            text_w,
        ),
        rect(x, dialog.top + 46.0, text_w, 18.0),
        &p.fmt.caption,
        &p.br.muted,
    );
    p.text(
        &p.clip(
            "不可撤销；执行前会试着留一份快照，兜不住时状态行会说明。",
            &p.fmt.caption,
            text_w,
        ),
        rect(x, dialog.top + 68.0, text_w, 18.0),
        &p.fmt.caption,
        &p.br.muted,
    );

    let labels = ["取消", "清理 30 天前"];
    for (i, label) in labels.iter().enumerate() {
        let (bg, fg) = if i == 1 {
            (&p.br.danger, &p.br.white)
        } else {
            (&p.br.header, &p.br.fg)
        };
        p.round(buttons[i], R_CONTROL, bg, Some(&p.br.border));
        p.text(label, buttons[i], &p.fmt.center_small, fg);
    }
}

/// 内置看板页：范围胶囊 + KPI 四格 + 一句话 + 趋势 + 两列排行（方案 3.2）。
///
/// 三条约定：
/// · 一个数字都不在这里算 —— 全部出自 `focusflow_core::dashboard`，那一半与「活跃分析」
///   用的是同一批读数函数。同一天的数在两页不一样，用户只会信小的那个；
/// · 趋势复用插件图表那一个画法（[`draw_plugin_chart`]）：纵轴顶格、绘图区边距、空数据
///   那句「暂无数据」都只有一份，同一份数据不该因为换了一页就画成另一个高度；
/// · 这一块 UI 的字体全是 `NO_WRAP` 且 `DrawText` 不按矩形裁切 —— 动态文本必须自己带
///   宽度上界，预算与画它的那块矩形取同一个表达式（本仓那条老规矩）。
fn draw_board_page(
    p: &P<'_>,
    w: f32,
    h: f32,
    dash: &focusflow_core::dashboard::DashBoard,
    vs: ViewState,
) {
    let m = 16.0;
    let top = 208.0;

    // --- 范围胶囊：与页签同一行、在七个页签右边；几何与命中都读 `dashboard::range_pills` ---
    for (i, (range, r)) in dashboard::range_pills().iter().enumerate() {
        let active = vs.dash_range == *range;
        if active {
            p.round(*r, R_CONTROL, &p.br.accent, None);
        } else {
            p.round(*r, R_CONTROL, &p.br.card, Some(&p.br.border));
            if vs.ctl == Some(Ctl::DashPill(i as u8)) {
                p.round(*r, R_CONTROL, &p.br.hover, None);
            }
        }
        p.text(
            range.label(),
            rect(r.left, r.top, r.right - r.left, r.bottom - r.top),
            &p.fmt.tab,
            if active { &p.br.white } else { &p.br.muted },
        );
    }

    // --- KPI 行：四格，顺序与文案都由 core 那一半定死 ---
    let kpi_gap = 10.0;
    let kw = (w - m * 2.0 - kpi_gap * 3.0) / 4.0;
    let row_h = 78.0;
    for (i, (name, value)) in dash.kpi.iter().enumerate() {
        let kx = m + i as f32 * (kw + kpi_gap);
        p.card(rect(kx, top, kw, row_h), R_CARD);
        let label_w = kw - 24.0;
        let shown_label = p.clip(name, &p.fmt.caption, label_w);
        p.text(
            &shown_label,
            rect(kx + 12.0, top + 10.0, label_w, 16.0),
            &p.fmt.caption,
            &p.br.muted,
        );
        let shown_value = p.clip(value, &p.fmt.value, label_w);
        p.text(
            &shown_value,
            rect(kx + 12.0, top + 30.0, label_w, 30.0),
            &p.fmt.value,
            &p.br.fg,
        );
    }

    // --- 一句话；缺读必须跟在它下面点名 ---
    let line_y = top + row_h + 10.0;
    let full_w = w - m * 2.0;
    let sentence = p.clip(&dash.sentence, &p.fmt.body, full_w);
    p.text(
        &sentence,
        rect(m, line_y, full_w, 20.0),
        &p.fmt.body,
        &p.br.fg,
    );
    if !dash.gaps.is_empty() {
        // 「这份数只是下界」不说出来，整页念「本周共按键 0 次」与"他这周一次都没按"
        // 在屏上就是同一句话（本仓修过好几次的那一类：读不出来不许印成没有）。
        let note = p.clip(
            &format!("这份读数这次没读全：{}", dash.gaps.join("；")),
            &p.fmt.caption,
            full_w,
        );
        p.text(
            &note,
            rect(m, line_y + 22.0, full_w, 16.0),
            &p.fmt.caption,
            &p.br.muted,
        );
    }

    // --- 趋势（缩略版：只画线，数值留给「活跃分析」那一页）---
    let layout = board_layout(h, line_y);
    let labels: Vec<String> = dash.trend.iter().map(|(d, _)| short_day(d)).collect();
    let points: Vec<i64> = dash.trend.iter().map(|(_, v)| *v).collect();
    draw_plugin_chart(
        p,
        rect(m, layout.trend_top, full_w, layout.trend_h),
        "line",
        &labels,
        &points,
    );

    // --- 两列排行 ---
    let rank_top = layout.rank_top;
    let rank_h = layout.rank_h;
    let col_w = (full_w - 12.0) / 2.0;
    draw_rank_card(
        p,
        rect(m, rank_top, col_w, rank_h),
        "Top 应用（时长）",
        &dash.top_apps,
        true,
    );
    draw_rank_card(
        p,
        rect(m + col_w + 12.0, rank_top, col_w, rank_h),
        "Top 按键（次数）",
        &dash.top_keys,
        false,
    );
}

/// 第 8 页：剪贴板管理。列表/模式/多选的状态在 `clipboard_page.rs`，这里只排版。
/// 把一份已解码的缩略图画进 `r`（等比缩放、居中）。建位图失败就当这一张没有，
/// 调用方会退回文字徽标 —— 缩略图是加分项，不该让整行画不出来。
fn draw_clip_thumb(p: &P<'_>, t: &clipboard_page::Thumb, r: D2D_RECT_F) -> bool {
    if t.w == 0 || t.h == 0 || r.right <= r.left || r.bottom <= r.top {
        return false;
    }
    // D2D 的预乘 alpha：直通会把半透明缩略图画成一圈黑边。
    let mut bgra = Vec::with_capacity(t.rgba.len());
    for px in t.rgba.as_chunks::<4>().0 {
        let a = px[3] as u32;
        let f = |c: u8| ((c as u32 * a + 127) / 255) as u8;
        bgra.extend_from_slice(&[f(px[2]), f(px[1]), f(px[0]), a as u8]);
    }
    let props = D2D1_BITMAP_PROPERTIES {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
    };
    unsafe {
        let Ok(bmp) = p.rt.CreateBitmap(
            D2D_SIZE_U {
                width: t.w,
                height: t.h,
            },
            Some(bgra.as_ptr() as *const core::ffi::c_void),
            t.w * 4,
            &props,
        ) else {
            return false;
        };
        let bw = r.right - r.left;
        let bh = r.bottom - r.top;
        let scale = (bw / t.w as f32).min(bh / t.h as f32);
        let (dw, dh) = (t.w as f32 * scale, t.h as f32 * scale);
        let dest = D2D_RECT_F {
            left: r.left + (bw - dw) / 2.0,
            top: r.top + (bh - dh) / 2.0,
            right: r.left + (bw + dw) / 2.0,
            bottom: r.top + (bh + dh) / 2.0,
        };
        p.rt.DrawBitmap(
            &bmp,
            Some(&dest),
            1.0,
            D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
            None,
        );
    }
    true
}

fn draw_clipboard_page(p: &P<'_>, w: f32, h: f32, clip: &clipboard_page::ClipPage, vs: ViewState) {
    use clipboard_page::Mode;

    let lay = clipboard_page::layout_for(w, h);

    // 三个模式页签：历史 / 片段 / 统计。
    for (i, mode) in clipboard_page::MODES.iter().enumerate() {
        let r = lay.tabs[i];
        let active = clip.mode == *mode;
        if active {
            p.round(r, R_CONTROL, &p.br.accent, None);
        } else {
            p.round(r, R_CONTROL, &p.br.card, Some(&p.br.border));
            if vs.ctl == Some(Ctl::ClipTab(i as u8)) {
                p.round(r, R_CONTROL, &p.br.hover, None);
            }
        }
        p.text(
            mode.label(),
            rect(r.left, r.top, r.right - r.left, r.bottom - r.top),
            &p.fmt.tab,
            if active { &p.br.white } else { &p.br.muted },
        );
    }

    // 搜索框：底框由主窗画，真正的输入是叠在上面的原生 EDIT（拿到焦点才显示）。
    if clip.mode != Mode::Stats {
        let sr = lay.search;
        p.round(sr, R_CONTROL, &p.br.card, Some(&p.br.border));
        let cue = if !clip.filter.trim().is_empty() {
            format!("筛选：{}", clip.filter)
        } else {
            "筛选历史 / 片段（边打边筛）".to_string()
        };
        if !vs.clip_search_focused {
            let cue = p.clip(&cue, &p.fmt.caption, sr.right - sr.left - 20.0);
            p.text(
                &cue,
                rect(
                    sr.left + 10.0,
                    sr.top,
                    sr.right - sr.left - 20.0,
                    sr.bottom - sr.top,
                ),
                &p.fmt.caption,
                if clip.filter.trim().is_empty() {
                    &p.br.muted
                } else {
                    &p.br.fg
                },
            );
        }
    }

    // 工具钮：历史与片段两套文案，同一份几何。
    if clip.mode != Mode::Stats {
        let labels: [&str; 6] = if clip.mode == Mode::History {
            ["复制", "固定", "删除", "存为片段", "清空", "多选"]
        } else {
            ["复制", "重命名", "删除", "", "", "多选"]
        };
        let has_row = clip.selected().is_some();
        for (i, label) in labels.iter().enumerate() {
            if label.is_empty() {
                continue;
            }
            let r = lay.toolbar[i];
            let enabled = has_row || *label == "清空" || *label == "多选";
            p.round(r, R_CONTROL, &p.br.card, Some(&p.br.border));
            if vs.ctl == Some(Ctl::ClipBtn(i as u8)) && enabled {
                p.round(r, R_CONTROL, &p.br.hover, None);
            }
            p.text(
                label,
                rect(r.left, r.top, r.right - r.left, r.bottom - r.top),
                &p.fmt.caption,
                if enabled { &p.br.fg } else { &p.br.muted },
            );
        }
        if clip.multi {
            let count = clip.checked_count();
            p.text(
                &format!("已选 {count} 条"),
                rect(
                    lay.toolbar[5].right + 10.0,
                    lay.toolbar[5].top,
                    160.0,
                    lay.toolbar[5].bottom - lay.toolbar[5].top,
                ),
                &p.fmt.caption,
                &p.br.accent,
            );
        }
    }

    // 列表卡片
    let card = rect(
        lay.pad(),
        lay.list_top - 8.0,
        w - lay.pad() * 2.0,
        lay.list_bottom - (lay.list_top - 8.0),
    );
    p.card(card, R_CARD);

    match clip.mode {
        Mode::Stats => draw_clipboard_stats(p, clip, lay, w),
        _ => {
            let rows = clip.rows();
            if rows.is_empty() {
                let msg = match clip.mode {
                    Mode::History if !clip.filter.trim().is_empty() => "没有匹配的记录",
                    Mode::History => "还没有记录：复制一段文字、图片或文件试试",
                    Mode::Snippets if !clip.filter.trim().is_empty() => "没有匹配的片段",
                    Mode::Snippets => "还没有片段：在历史里点「存为片段」",
                    Mode::Stats => "",
                };
                p.text(
                    msg,
                    rect(
                        lay.list_top,
                        lay.list_top + 16.0,
                        w - lay.pad() * 2.0 - 16.0,
                        24.0,
                    ),
                    &p.fmt.body,
                    &p.br.muted,
                );
            } else {
                let visible = lay.rows.min(rows.len().saturating_sub(clip.scroll));
                // 失效检查只为画出来的行做（见 `ClipPage::missing_files_for`）；这里
                // 数一下"这次现问了磁盘且真有失效"的行，验收脚本靠这行日志判屏上标了。
                let mut fresh_missing_rows = 0usize;
                for i in 0..visible {
                    let idx = clip.scroll + i;
                    let Some(row) = rows.get(idx) else { break };
                    let top = lay.list_top + i as f32 * lay.row_h;
                    let selected = !clip.multi && idx == clip.sel;
                    let checked = clip.multi && clip.checked.contains(&row.id);
                    let rr = rect(lay.pad() + 4.0, top, w - lay.pad() * 2.0 - 8.0, lay.row_h);
                    if selected || checked {
                        p.round(rr, R_CONTROL, &p.br.hover, None);
                    }
                    let mut left = rr.left + 8.0;
                    if clip.multi {
                        p.text(
                            if checked { "✓" } else { "○" },
                            rect(left, top + 9.0, 18.0, 20.0),
                            &p.fmt.caption,
                            if checked { &p.br.accent } else { &p.br.muted },
                        );
                        left += 22.0;
                    }
                    if clip.mode == Mode::History {
                        // 类型列：图片有缩略图就画图，否则画一个「图」「件」小签。
                        // 位置对所有行都留出来，时间/来源列不会随类型左右跳。
                        if row.kind != focusflow_core::clipboard::ClipKind::Text {
                            let slot = rect(left, top + 6.0, 28.0, rr.bottom - 6.0 - (top + 6.0));
                            let drew = row
                                .thumb_path
                                .as_deref()
                                .and_then(|path| clip.thumb(path))
                                .is_some_and(|t| draw_clip_thumb(p, &t, slot));
                            if !drew {
                                p.round(slot, R_CONTROL, &p.br.card, Some(&p.br.border));
                                p.text(
                                    clipboard_page::kind_badge(row.kind),
                                    slot,
                                    &p.fmt.caption,
                                    &p.br.accent,
                                );
                            }
                        }
                        left += 30.0;
                        p.text(
                            &row.time,
                            rect(left, top + 9.0, 86.0, 20.0),
                            &p.fmt.caption,
                            &p.br.muted,
                        );
                        let source = p.clip(&row.source, &p.fmt.caption, 112.0);
                        p.text(
                            &source,
                            rect(left + 88.0, top + 9.0, 112.0, 20.0),
                            &p.fmt.caption,
                            &p.br.muted,
                        );
                        let (missing, fresh) = clip.missing_files_for(row);
                        if fresh && missing > 0 {
                            fresh_missing_rows += 1;
                        }
                        let preview = clipboard_page::row_preview(row, missing);
                        let preview = p.clip(
                            &preview,
                            &p.fmt.body,
                            (rr.right - left - 88.0 - 112.0 - 26.0).max(20.0),
                        );
                        p.text(
                            &preview,
                            rect(
                                left + 202.0,
                                top + 8.0,
                                rr.right - left - 202.0 - 26.0,
                                22.0,
                            ),
                            &p.fmt.body,
                            &p.br.fg,
                        );
                        if row.pinned {
                            p.text(
                                "★",
                                rect(rr.right - 24.0, top + 8.0, 18.0, 20.0),
                                &p.fmt.caption,
                                &p.br.accent,
                            );
                        }
                    } else {
                        let name = p.clip(&row.name, &p.fmt.body, 180.0);
                        p.text(
                            &name,
                            rect(left, top + 8.0, 180.0, 22.0),
                            &p.fmt.body,
                            &p.br.fg,
                        );
                        let preview = clipboard_page::preview_text(&row.content);
                        let preview = p.clip(
                            &preview,
                            &p.fmt.caption,
                            (rr.right - left - 188.0).max(20.0),
                        );
                        p.text(
                            &preview,
                            rect(left + 188.0, top + 9.0, rr.right - left - 188.0, 20.0),
                            &p.fmt.caption,
                            &p.br.muted,
                        );
                    }
                }
                if fresh_missing_rows > 0 {
                    crate::tracing_note(&format!("clip_page_missing_files n={fresh_missing_rows}"));
                }
            }
        }
    }

    // 底部：状态 / 条数 / 暂停与筛选口径。
    let total = clip.rows().len();
    let selected = clip
        .checked_count()
        .max(usize::from(clip.selected().is_some()));
    let mut footer = if let Some(status) = &clip.status {
        status.clone()
    } else if clip.multi {
        format!("已选 {selected} 条 · 工具钮作用于勾住的行 · Tab 切模式 · Esc 退出多选")
    } else {
        match clip.mode {
            Mode::History => {
                "↑↓ 选择 · Enter 复制 · Del 删除 · Ctrl+P 固定 · Tab 切模式".to_string()
            }
            Mode::Snippets => "↑↓ 选择 · Enter 复制 · Del 删除 · Tab 切模式".to_string(),
            Mode::Stats => "Tab 切回历史 · 只统计次数，不展示内容".to_string(),
        }
    };
    if clip.mode != Mode::Stats {
        footer.push_str(&format!(" · {total} 条"));
        if !clip.filter.trim().is_empty() {
            footer.push_str(" · 已筛选");
        }
    }
    if focusflow_core::clipboard::is_paused() {
        footer.push_str(" · 记录已暂停");
    }
    if let Some(why) = focusflow_core::clipboard::watcher_fault() {
        // 排在最前面：页脚会被夹取，把结论放尾部会被省略号吃掉。原因限一句，
        // 全文在日志里。
        let why: String = why.chars().take(40).collect();
        footer = format!("监听未运行（复制不会收录）：{why} · {footer}");
    }
    let footer = p.clip(&footer, &p.fmt.caption, w - lay.pad() * 2.0);
    p.text(
        &footer,
        rect(lay.pad(), lay.footer_y, w - lay.pad() * 2.0, 20.0),
        &p.fmt.caption,
        &p.br.muted,
    );
    if clip.mode != Mode::Stats {
        let pos = if total == 0 {
            "0/0".to_string()
        } else {
            format!("{}/{}", (clip.scroll + lay.rows).min(total), total)
        };
        p.text(
            &pos,
            rect(w - lay.pad() - 70.0, lay.footer_y, 60.0, 20.0),
            &p.fmt.caption,
            &p.br.muted,
        );
    }
}

fn draw_clipboard_stats(
    p: &P<'_>,
    clip: &clipboard_page::ClipPage,
    lay: clipboard_page::Layout,
    w: f32,
) {
    let Some(s) = &clip.stats else {
        p.text(
            "统计读不出来",
            rect(lay.list_top, lay.list_top + 16.0, w - lay.pad() * 2.0, 24.0),
            &p.fmt.body,
            &p.br.muted,
        );
        return;
    };
    let lines = [
        format!("今日复制 {} 次", s.today),
        format!("最近 {} 天 {} 次", s.window_days, s.window_count),
        format!(
            "库里 {} 条（固定 {} 条） · 累计复制 {} 次",
            s.total_items, s.pinned_items, s.total_copies
        ),
    ];
    let mut y = lay.list_top + 8.0;
    for line in &lines {
        p.text(
            line,
            rect(lay.pad() + 8.0, y, w - lay.pad() * 2.0 - 16.0, 22.0),
            &p.fmt.body,
            &p.br.fg,
        );
        y += 26.0;
    }
    y += 8.0;
    p.text(
        "来源（最近 7 天）",
        rect(lay.pad() + 8.0, y, w - lay.pad() * 2.0, 20.0),
        &p.fmt.caption,
        &p.br.muted,
    );
    y += 24.0;
    if s.per_app.is_empty() {
        p.text(
            "还没有来源数据",
            rect(lay.pad() + 8.0, y, w - lay.pad() * 2.0, 20.0),
            &p.fmt.caption,
            &p.br.muted,
        );
        return;
    }
    for (app, count) in &s.per_app {
        let name = if app.is_empty() { "未知来源" } else { app };
        p.text(
            &p.clip(
                &format!("{name} · {count} 次"),
                &p.fmt.caption,
                w - lay.pad() * 2.0 - 16.0,
            ),
            rect(lay.pad() + 8.0, y, w - lay.pad() * 2.0 - 16.0, 20.0),
            &p.fmt.caption,
            &p.br.fg,
        );
        y += 22.0;
    }
}

struct BoardLayout {
    trend_top: f32,
    trend_h: f32,
    rank_top: f32,
    rank_h: f32,
}

/// 给窗口高度排下半部：窗口矮的时候**先缩趋势图**，给两张排行卡留出 120 高。
///
/// 固定的 200 + 120 在最小窗口（640：客户区被撑满整窗）下会把排行卡画到窗口底边
/// 以下 —— 圆角底边与最后几行被裁掉，而这一页没有滚动。`clamp(60, 200)` 的 60 是给
/// 矮到不正常的窗口兜底；640 这一档算出来是 154，绘图区仍有 80 高。
fn board_layout(h: f32, line_y: f32) -> BoardLayout {
    const M: f32 = 16.0;
    let trend_top = line_y + 44.0;
    let trend_h = (h - M - trend_top - 10.0 - 120.0).clamp(60.0, 200.0);
    let rank_top = trend_top + trend_h + 10.0;
    let rank_h = (h - M - rank_top).max(120.0);
    BoardLayout {
        trend_top,
        trend_h,
        rank_top,
        rank_h,
    }
}

/// 横轴日期串只留 `MM-DD`；不是这个形状（导入的旧库、带时间的日期）就原样交回，
/// 由那一格的夹取兜住宽度。
fn short_day(date: &str) -> String {
    match date.split_once('-') {
        Some((_, rest)) if rest.len() == 5 => rest.to_string(),
        _ => date.to_string(),
    }
}

/// 一列排行：标题 + 每行「名称 …… 数字」。名称先夹（右边那 84 px 是数字的位置）。
fn draw_rank_card(
    p: &P<'_>,
    card: D2D_RECT_F,
    title: &str,
    rows: &[(String, i64)],
    by_seconds: bool,
) {
    use focusflow_core::format::{fmt_compact_duration, fmt_thousands};
    let pad = 12.0;
    p.card(card, R_CARD);
    p.text(
        title,
        rect(
            card.left + pad,
            card.top + 10.0,
            card.right - card.left - pad * 2.0,
            18.0,
        ),
        &p.fmt.body,
        &p.br.fg,
    );
    if rows.is_empty() {
        p.text(
            "暂无数据",
            rect(
                card.left + pad,
                card.top + 34.0,
                card.right - card.left - pad * 2.0,
                18.0,
            ),
            &p.fmt.caption_center,
            &p.br.muted,
        );
        return;
    }
    let value_w = 84.0;
    let name_w = (card.right - card.left) - pad * 2.0 - value_w - 8.0;
    for (i, (name, value)) in rows.iter().enumerate() {
        let y = card.top + 34.0 + i as f32 * 20.0;
        // 画不进这张卡的那些行直接不画：这里没有点击区，但溢出卡外的文本比少一行更难解释
        if y + 18.0 > card.bottom - 2.0 {
            break;
        }
        let shown_name = p.clip(name, &p.fmt.caption, name_w);
        p.text(
            &shown_name,
            rect(card.left + pad, y, name_w, 18.0),
            &p.fmt.caption,
            &p.br.fg,
        );
        let shown_value = if by_seconds {
            fmt_compact_duration(*value)
        } else {
            fmt_thousands(*value)
        };
        p.text(
            &shown_value,
            rect(card.right - pad - value_w, y, value_w, 18.0),
            &p.fmt.caption_right,
            &p.br.muted,
        );
    }
}

/// 还没接上的页签兜底：以前这一支连"越界"都画不出来（`view` 是 usize，比 6 大的数
/// 会走到这里），所以它必须留在 `match vs.view` 的最后一支。
fn draw_placeholder(p: &P<'_>, w: f32, h: f32, view: usize) {
    let m = 16.0;
    let text = match view {
        5 => "设置",
        _ => "待迁移",
    };
    draw_placeholder_card(p, rect(m, 208.0, w - m * 2.0, h - 208.0 - m), text);
}

fn draw_placeholder_card(p: &P<'_>, r: D2D_RECT_F, text: &str) {
    p.card(r, R_CARD);
    p.text(
        text,
        rect(r.left, r.top + 120.0, r.right - r.left, 24.0),
        &p.fmt.center_small,
        &p.br.muted,
    );
}

/// 左表的几何：画图与命中测试共用同一份，避免两处各算一套坐标。
///
/// 数字列一律给**右沿**、按右对齐画：这块 UI 不按矩形裁切（`NO_WRAP` 且不设
/// `DWRITE_TRIMMING`），数一长就照画到下一列上。设备那一页「596,600」被「还原」
/// 按钮盖住、屏上只剩「596,6」，就是列宽按左沿安排的结果。
struct TableGeom {
    pad: f32,
    head_y: f32,
    list_top: f32,
    row_h: f32,
    max_rows: usize,
    x_rank: f32,
    x_name: f32,
    /// 次数列的右沿（键鼠 / 应用 / 分组三张榜）：右边 12 px 之外是占比列。
    x_count_right: f32,
    /// 设备页的次数列右沿：右边是「还原」那一整颗按钮，要再多让出 64 px。
    x_count_right_ops: f32,
    /// 「改名」按钮的左沿（也是设备页最后一列的左沿）。
    x_last: f32,
    /// 「占比」这一列的左沿。它的右沿与「改名」按钮的右沿同一条线（`x_last + 58`），
    /// 但这一列更窄（[`SHARE_COL_W`]），省下的宽度给名字列。
    x_share_left: f32,
}

/// 全套界面的圆角只有四档：卡片 / 浮层 / 控件 / 微元素。
///
/// 以前散着 11 种（1/2/3/4/5/6/7/8/12/14/16），同一个"按钮"在不同页面圆得不一样。
/// `P::round` 会按矩形短边再夹一刀，所以小元素（8px 圆点、4px 滚动条）照样是圆的。
const R_CARD: f32 = 14.0;
const R_MODAL: f32 = 16.0;
const R_CONTROL: f32 = 8.0;
const R_MICRO: f32 = 4.0;

/// 数字列（次数 / 占比）的宽度。七个数字加两个千分位逗号在 11 号字下约 55 px：
/// 次数 72 留得下，占比「100.0%」约 40 px、52 够。两列原来分别是 96 / 58，
/// 他 2026-10-30 说"占比太多、匀一点给名称"，就从这两格各收一刀 —— 收出来的宽度
/// 全进了名字列（`name_col_width`），数字列仍不许顶到下一列（那条用例还在）。
const NUM_COL_W: f32 = 72.0;
/// 占比列宽度（右沿与「改名」按钮的右沿对齐，见 `table_geom`）。
const SHARE_COL_W: f32 = 52.0;

/// 设备页「设备名」那一列的宽度：右边依次是类型 48 + 12、次数 `NUM_COL_W`、8、
/// 还原 58、6、改名 58。行文字、编辑中那圈框、原生 EDIT 的落点三处都读它。
fn device_name_width(g: &TableGeom) -> f32 {
    g.x_count_right_ops - NUM_COL_W - 12.0 - 48.0 - 12.0 - g.x_name
}

/// 键鼠 / 应用 / 分组三张榜的名字列宽度（次数列左边留 12 px）。
fn name_col_width(g: &TableGeom) -> f32 {
    g.x_count_right - NUM_COL_W - 12.0 - g.x_name
}

/// 胶囊的宽与横向步进（都相对组原点 `x0`）。最后一颗的右沿 = `x0 + rank_pill_group_w()`。
const PILL_W: f32 = 56.0;
const PILL_DX: [f32; 5] = [32.0, 90.0, 188.0, 246.0, 304.0];
/// 胶囊组右沿到卡片右边距的距离。
const PILL_RIGHT_GAP: f32 = 12.0;
/// 摘要格右沿到胶囊组原点之间留的缝：把"读数"和"按钮"分开，不许贴着脸。
const SUMMARY_PILL_GAP: f32 = 8.0;

/// 胶囊组相对原点的总宽度，由 `PILL_DX` 最后一档导出。
/// 写成表达式而不是第二个手抄常数：加一颗胶囊时这里跟着变，倒推出来的原点才不会又顶出去。
fn rank_pill_group_w() -> f32 {
    PILL_DX[PILL_DX.len() - 1] + PILL_W
}

/// 胶囊组原点：从**卡片右沿倒推**，画图与命中测试都读这一个表达式。
///
/// 原来这里写的是 `card.left + 322`，那颗数是按 1200px 窗口量出来的：窗口一窄，最后一颗就
/// 越出卡片右沿（960px 超 132px）。这块 UI 不按矩形裁切（`NO_WRAP`），越出去的字照画，症状是
/// 胶囊画到右边那张卡上、点还点不中。现量：1200px 下改成右对齐只挪 0.4px（698 → 697.6），
/// 默认宽度的验收坐标因此不动 —— 这是它优于"抬高窗口下限"的理由。
fn rank_pills_x0(card: D2D_RECT_F) -> f32 {
    card.right - PILL_RIGHT_GAP - rank_pill_group_w()
}

/// 排行页标题行里那排胶囊。下标 0..1 = 视图（排行 / 分组），2..4 = 筛选（全部 / 键盘 / 鼠标）。
/// 「分组」那一屏不读筛选，那时后面三格既不画也不接 —— 与画图共用这一份位置，
/// 照的是 `view_tabs` / `atab_tabs` 那个做法。
fn rank_pills(card: D2D_RECT_F) -> [D2D_RECT_F; 5] {
    let y = card.top + 8.0;
    let x0 = rank_pills_x0(card);
    let mut out = [rect(0.0, 0.0, 0.0, 0.0); 5];
    for (i, dx) in PILL_DX.iter().enumerate() {
        out[i] = rect(x0 + dx, y, PILL_W, 26.0);
    }
    out
}

/// 两排胶囊前那两句小字（视图 / 筛选），原点与 `rank_pills` 同一份。
fn rank_pill_captions(card: D2D_RECT_F) -> (D2D_RECT_F, D2D_RECT_F) {
    let y = card.top + 11.0;
    let x0 = rank_pills_x0(card);
    (rect(x0, y, 30.0, 16.0), rect(x0 + 148.0, y, 30.0, 16.0))
}

/// 标题行里那句摘要（共 N 次 / 共 时长）的矩形：右边界由胶囊组原点决定。
///
/// 宽度以前写死 220：窗口拖窄时胶囊往左走，这一格却不动，右沿就压到「视图」那两个字底下。
/// 抽成函数是因为判据盯的是这个矩形，而画图那一处没法单测。
fn rank_summary_box(card: D2D_RECT_F, pad: f32) -> D2D_RECT_F {
    let left = card.left + pad + 74.0;
    let w = (rank_pills_x0(card) - SUMMARY_PILL_GAP - left).max(0.0);
    rect(left, card.top + 10.0, w, 18.0)
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
    // 标题行下面还有一行「其中 鼠标 … · 键盘 …」（见 `draw_dashboard` 那段），所以表头
    // 比标题行内只有一句摘要时低 2 px，给那一行留出位置。
    let head_y = r.top + 44.0;
    let list_top = head_y + 22.0;
    let foot_h = 26.0;
    let max_rows = (((r.bottom - foot_h - list_top) / row_h).floor() as usize).max(1);
    let x_last = r.right - pad - 78.0;
    // 占比列的右沿与「改名」按钮的右沿对齐（按钮 58 宽，`rename_button` 用同一个 58）。
    let x_share_left = x_last + 58.0 - SHARE_COL_W;
    TableGeom {
        pad,
        head_y,
        list_top,
        row_h,
        max_rows,
        x_rank: r.left + pad + 10.0,
        x_name: r.left + pad + 56.0,
        x_count_right: x_share_left - 8.0,
        x_count_right_ops: x_last - 64.0 - 8.0,
        x_last,
        x_share_left,
    }
}

/// 排行表一行的命中/悬停带：横向是整张卡片（与点击一致），纵向是这一行。
///
/// 画那一行的高亮左右各缩 12（与列内容对齐），但"亮起来"与"点得到"必须是同一个
/// 区域：从前悬停只判 `y`，指针压在右侧趋势图上也会点亮左表那一行，而点下去什么
/// 都不会发生。
fn table_row_band(card: D2D_RECT_F, y: f32, row_h: f32) -> D2D_RECT_F {
    rect(card.left, y, card.right - card.left, row_h)
}

/// 排行/应用/设备三屏里指针落在哪一行（返回 `(榜里下标, 屏上第几行)`）。
///
/// **只有设备屏的行可点**（改名/恢复/看详情）。键鼠与应用两屏没有点击下游，
/// 从前它们也画悬停、也记选中，等于让高亮许诺一件不会发生的事 —— 这里直接返回
/// `None`，画与光标一起保持安静。
fn table_row_at(
    count: usize,
    g: &TableGeom,
    card: D2D_RECT_F,
    view: usize,
    scroll: usize,
    x: f32,
    y: f32,
) -> Option<(usize, usize)> {
    if view != 2 {
        return None;
    }
    let scroll = scroll.min(count.saturating_sub(g.max_rows));
    for vis in 0..g.max_rows {
        let idx = scroll + vis;
        if idx >= count {
            break;
        }
        let y0 = g.list_top + vis as f32 * g.row_h;
        if hit(table_row_band(card, y0, g.row_h), x, y) {
            return Some((idx, vis));
        }
    }
    None
}

fn rename_button(g: &TableGeom, y: f32) -> D2D_RECT_F {
    rect(g.x_last, y + 1.0, 58.0, 22.0)
}

fn restore_button(g: &TableGeom, y: f32) -> D2D_RECT_F {
    rect(g.x_last - 64.0, y + 1.0, 58.0, 22.0)
}

/// 缺口清单里到底有几**本**年度库读不动（不是几条读数）。
///
/// 一年一份文件（core 那一族各栏读的都是 `paths::year_db_path(year)`），而那一年读不动时
/// 按键统计／应用时长／设备统计三栏**各记一条**：年份相同、域名不同，`sort` + `dedup`
/// 也并不掉。从前周报直接念 `gaps.len()` ⇒ 一本读不动的库被说成三本，跨年那一周说成六本，
/// 而这句话正是"上面那些数字是不是下界"的凭据，数错了没人会去核对。
///
/// 形状对不上的那一条（core 改了那句话而没改这里）整串自成一类：宁可多念一本，
/// 不许把一次读不动吞掉 —— 吞掉等于把下界念成实测。
fn gap_year_count(gaps: &[String]) -> usize {
    let mut seen: Vec<&str> = Vec::new();
    for g in gaps {
        let head = g.split(" 年库").next().unwrap_or(g.as_str());
        let key = if !head.is_empty() && head.chars().all(|c| c.is_ascii_digit()) {
            head
        } else {
            g.as_str()
        };
        if !seen.contains(&key) {
            seen.push(key);
        }
    }
    seen.len()
}

/// Markdown 表格单元格的转义。
///
/// 键名来自旧版导入数据、设备名可以是用户手输的别名：`|` 在 Windows 文件名里不合法、
/// 在别名里完全合法，而它会把一行拆成多列；`<`/`>` 在允许内联 HTML 的 Markdown
/// 查看器里原样生效。CSV 与 HTML 两份导出各自有转义，周报这一份从前一格都没有。
fn md_cell(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('|', "\\|")
        .replace(['\r', '\n'], " ")
}

/// 周报开头那句「这一轮没读全」。**纯函数**：`generate_weekly_report` 要读库、要落文件，
/// 无头环境里跑不动，而这一句里的两个数（几本库／几条读数）正是最容易说错的地方。
fn weekly_gap_sentence(gaps: &[String]) -> String {
    if gaps.is_empty() {
        return String::new();
    }
    format!(
        "\n> 有 {} 个年度库没读全（{} 处读数）：{}",
        gap_year_count(gaps),
        gaps.len(),
        gaps.join("；")
    )
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

/// 周报数「达标」用的那把尺子。
///
/// 必须与面板/连续打卡是同一份：`stats::goal_status` 走的都是 `effective_goal`，而这里
/// 原来直接拿原始配置值比 ⇒ 同一个人把 `goal.daily_keys` 设成 0（或负数）时，报告里
/// 「达标天数 7 / 7」按 0 算（连零活动那天都算达标、每日活跃那栏还画 ✅），设置页里
/// 「连续 N 天」按 1 算。0 次不是"没有目标"，`effective_goal` 把它兜成 1，这条注释里
/// 说的两把尺子就是它存在的理由。
///
/// 单独成函数是为了让这两把尺子的相等性能被一条用例钉住（拿 `generate_weekly_report`
/// 整个函数没法在无头环境里跑：它要读库、要落文件）。
fn weekly_goal_met(count: i64, raw_goal: i64) -> bool {
    count >= focusflow_core::stats::effective_goal(raw_goal)
}

fn generate_weekly_report() -> std::result::Result<std::path::PathBuf, String> {
    use chrono::{Duration, Local};
    use focusflow_core::db::queries;
    use std::fmt::Write as _;

    let today = Local::now().date_naive();
    let start = last_week_start(today);
    let end = start + Duration::days(6);
    // ⚠ 这是**原始**配置值：判达标只许过 `weekly_goal_met`，显示目标那一句只许过
    // `effective_goal`（两者是同一个兜法）。直接拿它比就是"报告与面板两把尺子"。
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
    // 「设备输入合计」那一句念的是**整份**设备榜，不是排完序切下来那 10 行 ——
    // 所以求和必须排在 `truncate` 之前。顺序换过来的症状是那句安静地少报：
    // 一周里只用过十几次的设备被排到 10 名开外，报告照出、行数照少、没有一处对不上。
    let device_total: i64 = devices.iter().map(|(_, v)| *v).sum();
    devices.truncate(10);

    let max_day = by_day
        .iter()
        .max_by_key(|(_, v)| *v)
        .map(|(d, v)| (d.format("%m-%d").to_string(), *v))
        .unwrap_or_else(|| ("—".to_string(), 0));
    let hit = by_day
        .iter()
        .filter(|(_, v)| weekly_goal_met(*v, goal))
        .count();

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
        md_cell(&max_day.0)
    );
    let _ = writeln!(
        out,
        "- 达标天数：{} / 7（每日目标 {} 次）",
        hit,
        thousands(focusflow_core::stats::effective_goal(goal).max(0) as u64)
    );
    if device_total > 0 {
        let _ = writeln!(
            out,
            "- 设备输入合计：{} 次（独立口径，和上面的键鼠次数不直接相加）",
            thousands(device_total.max(0) as u64)
        );
    }
    let gap_say = weekly_gap_sentence(&gaps);
    if !gap_say.is_empty() {
        let _ = write!(out, "{gap_say}");
    }

    let _ = writeln!(out, "\n## 每日活跃\n\n| 日期 | 次数 |\n|---|---:|");
    for (d, n) in &by_day {
        let mark = if weekly_goal_met(*n, goal) {
            " ✅"
        } else {
            ""
        };
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
        let _ = writeln!(
            out,
            "| {} | {} |",
            md_cell(k),
            thousands((*n).max(0) as u64)
        );
    }
    if keys.is_empty() {
        let _ = writeln!(out, "| （这一周没有记录） | 0 |");
    }

    let _ = writeln!(
        out,
        "\n## 应用 TOP 10（前台时长）\n\n| 应用 | 时长 |\n|---|---:|"
    );
    for (name, secs) in &apps {
        let _ = writeln!(out, "| {} | {} |", md_cell(name), duration_text(*secs));
    }
    if apps.is_empty() {
        let _ = writeln!(out, "| （这一周没有记录） | 0s |");
    }

    let _ = writeln!(out, "\n## 设备 TOP 10\n\n| 设备 | 次数 |\n|---|---:|");
    for (name, n) in &devices {
        let _ = writeln!(
            out,
            "| {} | {} |",
            md_cell(name),
            thousands((*n).max(0) as u64)
        );
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

/// 一栏「占比」的唯一出处：分子那一行除以**与它同一次读回来的**总数，念成 `NN.N%`。
///
/// 分母由调用方给，而且只许给同源的数：屏上六处占比过去各写各的 `if total > 0 {}`，
/// 于是"分母该用哪一个"这件事在六处各答了一次 —— 其中四处（键名榜、分组榜、两份导出的
/// 键名列、热力板悬停那格）拿的是卡片那个含未落库增量的 `period_total`，而分子是已落库的
/// 榜行。这里把写法收成一处，分母选哪一把尺子就变成每个调用点上明摆着要回答的问题。
/// `total <= 0` 时念 `0.0%`：一个数都没有，就没有"占了百分之几"这回事。
fn share_text(count: i64, total: i64) -> String {
    if total <= 0 {
        return "0.0%".to_string();
    }
    let pct = count.max(0) as f32 * 100.0 / total as f32;
    format!("{pct:.1}%")
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
        let share = share_text(*v, s.rank_share_base());
        out.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{share}</td></tr>",
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
    if s.app_rows_total > s.apps.len() {
        out.push_str(&format!(
            "<p>应用太多，只列了前 {} 个（共 {} 个）。</p>\n",
            s.apps.len(),
            s.app_rows_total
        ));
    }

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
        // 三份导出要能互相核对：CSV 与 HTML 那两份由我们自己算好每栏的占比，分母各是
        // 下面这三个数（不是含未落库增量的 `period_total`）。JSON 过去只交出 `period_total`
        // ⇒ 拿 JSON 重算占比的人除出来的一定与同一次点击导出的 CSV 不一样：同一个问题、
        // 两个答案，而两份文件并排放着看不出谁对。
        "rank_total": s.rank_share_base(),
        "app_total": s.app_total,
        // `apps` 只列前 100 个：不交这个数，读 JSON 的人会把"前 100 名"当成"全部应用"。
        "app_rows_total": s.app_rows_total,
        "device_total": s.device_total,
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
        let share = share_text(*v, s.rank_share_base());
        out.push_str(&format!("{},{},{},{share}\r\n", i + 1, csv_field(k), v));
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
        // 分母是设备侧这一趟读回来的总数（`device_total`），不是卡片那个键鼠总数：
        // 设备计数走 Raw Input 侧信道，与主链路本来就两套口径（合成的键盘事件不计数、
        // 鼠标事件计），拿主链路的数去除，除出来的是两把尺子的商——同一份导出里
        // 「键鼠排行」那栏按自己的总数除到 100%，这一栏却只到九成出头。
        let share = share_text(d.count, s.device_total);
        out.push_str(&format!(
            "{},{},{},{},{share}\r\n",
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

fn draw_table(
    p: &P<'_>,
    r: D2D_RECT_F,
    snap: &Snapshot,
    vs: ViewState,
    tip: &mut Option<(String, D2D_RECT_F)>,
) {
    p.card(r, R_CARD);
    let g = table_geom(r);
    let tab = vs.view.min(2);
    // 这一屏该显示哪几行（键鼠那一屏会随「分组」与筛选换内容），命中测试读的是同一份。
    let rows = left_rows(snap, tab, vs.rank_tab, vs.rank_filter);
    let (title, total, count) = match tab {
        1 => ("应用排行", snap.app_total, rows.len()),
        2 => ("设备排行", snap.device_total, rows.len()),
        _ => ("键鼠排行", snap.period_total, rows.len()),
    };
    // 滚动位置按当前页签的行数夹一次：数据变少时别停在空白处。
    let scroll = vs.scroll.min(count.saturating_sub(g.max_rows));

    // 标题行比原来高 2px 起步：它下面还有一行鼠标/键盘（见下），那一行要落在表头上面。
    p.text(
        title,
        rect(r.left + g.pad, r.top + 8.0, 120.0, 20.0),
        &p.fmt.body,
        &p.br.fg,
    );
    let lane = match tab {
        1 => queries::DOMAIN_APPS,
        2 => queries::DOMAIN_DEVICES,
        _ => queries::DOMAIN_KEYS,
    };
    let summary = if tab == 1 {
        format!("共 {}", duration_text(total))
    } else {
        format!("共 {} 次", thousands(total.max(0) as u64))
    };
    // 键鼠那一屏多两个数：鼠标侧与键盘侧。写「其中」而不是把它们并成总数 ——
    // 未落库的那截（`pending`）还没归到任何一侧，折进键盘侧就是把没证的数报成实测。
    //
    // 这一句**单独一行**摆：它是三个数，比上面那句长得多，而下面那条判据说的是"摘要这一格
    // 按胶囊原点收窄" —— 把「其中」也塞进同一格，窄窗口里它才是先顶出去的那个。
    let split = if tab == 0 && !snap.rank.is_empty() {
        format!(
            "其中 鼠标 {} · 键盘 {}",
            thousands(snap.mouse_total.max(0) as u64),
            thousands(snap.keyboard_total.max(0) as u64)
        )
    } else {
        String::new()
    };
    // 本栏那句"没读全"不写在这里：这一格窄窗口下只剩几十像素，一条年度库的缺读理由就把它撑满，
    // 挤掉的是刚补的鼠标/键盘两个数。挪到同一张卡底下那条页脚去说（它宽得多，而且句子短了）。
    //
    // 预算按**这一格实际用的那档字**（`p.fmt.caption`）取，不是设置页说明行那一档：拿错档位
    // 时算出来的字数与屏上像素不是一把尺子，窄窗口下放行、照样画到胶囊底下。
    let sb = rank_summary_box(r, g.pad);
    let summary_text = p.clip(&summary, &p.fmt.caption, sb.right - sb.left);
    p.text(&summary_text, sb, &p.fmt.caption, &p.br.muted);
    if !split.is_empty() {
        p.text(
            &split,
            rect(
                r.left + g.pad,
                r.top + 28.0,
                r.right - r.left - g.pad * 2.0,
                14.0,
            ),
            &p.fmt.caption,
            &p.br.muted,
        );
    }
    if tab == 0 {
        let pills = rank_pills(r);
        let (cap1, cap2) = rank_pill_captions(r);
        p.text("视图", cap1, &p.fmt.caption, &p.br.muted);
        for (i, label) in ["排行", "分组"].iter().enumerate() {
            let active = vs.rank_tab == i;
            p.round(
                pills[i],
                R_CONTROL,
                if active { &p.br.accent } else { &p.br.card },
                if active { None } else { Some(&p.br.border) },
            );
            if !active && vs.ctl == Some(Ctl::RankPill(i as u8)) {
                p.round(pills[i], R_CONTROL, &p.br.hover, None);
            }
            p.text(
                label,
                pills[i],
                &p.fmt.center_small,
                if active { &p.br.white } else { &p.br.muted },
            );
        }
        // 分组那一屏读的是整份分组榜，筛选对它没有意义 ⇒ 连格子一起撤掉（旧版同做法）。
        if vs.rank_tab == 0 {
            p.text("筛选", cap2, &p.fmt.caption, &p.br.muted);
            for (i, label) in ["全部", "键盘", "鼠标"].iter().enumerate() {
                let active = vs.rank_filter == i;
                p.round(
                    pills[i + 2],
                    R_CONTROL,
                    if active { &p.br.accent } else { &p.br.card },
                    if active { None } else { Some(&p.br.border) },
                );
                if !active && vs.ctl == Some(Ctl::RankPill((i + 2) as u8)) {
                    p.round(pills[i + 2], R_CONTROL, &p.br.hover, None);
                }
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
    // 表头下面补一条分隔线：表头与斑马纹改用两个色之后，光靠底色已经分得开，
    // 但列表一长、眼睛从中间进来时，这条线才是"上面是表头"的那个锚。
    p.line(
        r.left + g.pad,
        g.head_y + 22.0,
        r.right - g.pad,
        g.head_y + 22.0,
        &p.br.border,
        1.0,
    );
    let heads: [&str; 4] = match (tab, vs.rank_tab) {
        (1, _) => ["排名", "应用", "时长", "占比"],
        (2, _) => ["排名", "设备名", "次数", "操作"],
        (0, 1) => ["序", "分组", "次数", "占比"],
        _ => ["排名", "键名", "次数", "占比"],
    };
    // 表头跟着数字那一列**右对齐**：列宽是定数，表头还从左沿摆的话会离自己那列远一截。
    let count_right = if tab == 2 {
        g.x_count_right_ops
    } else {
        g.x_count_right
    };
    p.text(
        heads[0],
        rect(g.x_rank, g.head_y + 3.0, 40.0, 18.0),
        &p.fmt.caption,
        &p.br.muted,
    );
    p.text(
        heads[1],
        rect(
            g.x_name,
            g.head_y + 3.0,
            count_right - NUM_COL_W - g.x_name,
            18.0,
        ),
        &p.fmt.caption,
        &p.br.muted,
    );
    p.text(
        heads[2],
        rect(count_right - NUM_COL_W, g.head_y + 3.0, NUM_COL_W, 18.0),
        &p.fmt.caption_right,
        &p.br.muted,
    );
    if tab == 2 {
        // 「操作」盖住那两颗按钮（还原 + 改名），而不是只盖住「改名」那一格。
        p.text(
            heads[3],
            rect(g.x_last - 64.0, g.head_y + 3.0, 58.0 + 6.0 + 58.0, 18.0),
            &p.fmt.center_small,
            &p.br.muted,
        );
    } else {
        p.text(
            heads[3],
            rect(g.x_share_left, g.head_y + 3.0, SHARE_COL_W, 18.0),
            &p.fmt.caption_right,
            &p.br.muted,
        );
    }

    if count == 0 {
        // 采集开着时列表会先空一段：排行读的是**已落库**的聚合表，而写线程按
        // `[database] flush_interval`（默认 10 秒）才落一次。这不是"没记上"，
        // 所以这里要说清楚还差哪一步 —— 不然"今日 5 次 + 空列表"看着像 bug。
        let pending = collect::pending_today();
        // 筛选把榜筛空时，别让他去翻数据目录：这条只可能是"换一下筛选"能解开的。
        let filtered_empty = tab == 0
            && vs.rank_tab == 0
            && vs.rank_filter != 0
            && !left_rows(snap, 0, vs.rank_tab, 0).is_empty();
        let empty = if filtered_empty {
            "当前筛选下没有记录（只键盘 / 只鼠标）——切回「全部」看看".to_string()
        } else {
            empty_board_line(
                tab,
                &period_text(snap.period),
                pending,
                focusflow_core::config::instance()
                    .get_int("database", "flush_interval", 10)
                    .max(1),
                &snap.gaps,
                lane,
            )
        };
        p.text(
            &empty,
            rect(
                r.left + g.pad + 10.0,
                g.list_top + 8.0,
                r.right - r.left - g.pad * 2.0,
                20.0,
            ),
            &p.fmt.caption,
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
            p.round(row, R_MICRO, &p.br.accent_soft, None);
        } else if vs.hover == Some(shown) {
            // 悬停与斑马纹分开：以前两者共用 `header`，奇数行悬停时一点变化都看不出来。
            p.fill(row, &p.br.row_hover);
        } else if vis % 2 == 1 {
            p.fill(row, &p.br.zebra);
        }
        p.text(
            &format!("{}", shown + 1),
            rect(g.x_rank, y + 4.0, 40.0, 18.0),
            &p.fmt.caption,
            if shown < 3 { &p.br.accent } else { &p.br.muted },
        );

        match tab {
            2 => {
                let d = &snap.devices[idx];
                // 类型与次数两格都从「次数列右沿」往回推，免得名字列一改宽、这两格还按旧数摆。
                let count_x = g.x_count_right_ops - NUM_COL_W;
                let kind_x = count_x - 12.0 - 48.0;
                if vs.editing == Some(idx) {
                    // 编辑中：这一格由原生 EDIT 盖着，这里只画一圈边框。
                    p.round(
                        rect(
                            g.x_name - 4.0,
                            y + 1.0,
                            device_name_width(&g) + 4.0,
                            g.row_h - 3.0,
                        ),
                        R_MICRO,
                        &p.br.card,
                        Some(&p.br.accent),
                    );
                } else {
                    let cell = rect(g.x_name, y + 3.0, device_name_width(&g), 18.0);
                    let clipped = p.clip(&d.name, &p.fmt.table_name, device_name_width(&g));
                    note_tooltip(tip, vs, cell, &d.name, clipped != d.name);
                    p.text(&clipped, cell, &p.fmt.table_name, &p.br.fg);
                }
                let kind = match d.kind.as_str() {
                    "mouse" => "鼠标",
                    "keyboard" => "键盘",
                    "hybrid" => "键鼠",
                    _ => "未知",
                };
                p.text(
                    kind,
                    rect(kind_x, y + 5.0, 48.0, 16.0),
                    &p.fmt.caption_right,
                    &p.br.muted,
                );
                p.text(
                    &thousands(d.count.max(0) as u64),
                    rect(count_x, y + 4.0, NUM_COL_W, 18.0),
                    &p.fmt.right,
                    &p.br.fg,
                );
                if d.has_alias {
                    let rb = restore_button(&g, y);
                    p.round(rb, R_MICRO, &p.br.accent_soft, None);
                    p.text("还原", rb, &p.fmt.center_small, &p.br.muted);
                }
                let btn = rename_button(&g, y);
                let hot = vs.hover == Some(idx) || vs.sel == Some(idx);
                p.round(
                    btn,
                    R_MICRO,
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
                let share = share_text(*secs, snap.app_total);
                // 应用名是库里原样存的，长度没有上限；这一格不夹就会一路画进右边的时长列
                // （设备详情那一列已经这么夹了，看板卡片也是）。
                let cell = rect(g.x_name, y + 3.0, name_col_width(&g), 18.0);
                let clipped = p.clip(app, &p.fmt.table_name, name_col_width(&g));
                note_tooltip(tip, vs, cell, app, clipped != app.as_str());
                p.text(&clipped, cell, &p.fmt.table_name, &p.br.fg);
                p.text(
                    &duration_text(*secs),
                    rect(g.x_count_right - NUM_COL_W, y + 4.0, NUM_COL_W, 18.0),
                    &p.fmt.right,
                    &p.br.fg,
                );
                p.text(
                    &share,
                    rect(g.x_share_left, y + 4.0, SHARE_COL_W, 18.0),
                    &p.fmt.caption_right,
                    &p.br.muted,
                );
            }
            _ if vs.rank_tab == 1 => {
                // 「分组」那一屏：八组的固定顺序，占比按**这一轮读回来的键鼠总数**
                // （榜里那些行的同一个出处）。这八组盖住全部键名，所以它们加起来正好 100%；
                // 分母一旦换成卡片那个含未落库增量的数，八组就只到 97% 而没人说得清为什么。
                let (name, n) = &snap.group[idx];
                let share = share_text(*n, snap.rank_share_base());
                p.text(
                    name,
                    rect(g.x_name, y + 3.0, name_col_width(&g), 18.0),
                    &p.fmt.table_name,
                    &p.br.fg,
                );
                p.text(
                    &thousands((*n).max(0) as u64),
                    rect(g.x_count_right - NUM_COL_W, y + 4.0, NUM_COL_W, 18.0),
                    &p.fmt.right,
                    &p.br.fg,
                );
                p.text(
                    &share,
                    rect(g.x_share_left, y + 4.0, SHARE_COL_W, 18.0),
                    &p.fmt.caption_right,
                    &p.br.muted,
                );
            }
            _ => {
                let (key, n) = &snap.rank[idx];
                let share = share_text(*n, snap.rank_share_base());
                // 键名与设备名同一条：旧库导入进来的名字可以很长，夹到这一列里。
                let cell = rect(g.x_name, y + 3.0, name_col_width(&g), 18.0);
                let clipped = p.clip(key, &p.fmt.table_name, name_col_width(&g));
                note_tooltip(tip, vs, cell, key, clipped != key.as_str());
                p.text(&clipped, cell, &p.fmt.table_name, &p.br.fg);
                p.text(
                    &thousands((*n).max(0) as u64),
                    rect(g.x_count_right - NUM_COL_W, y + 4.0, NUM_COL_W, 18.0),
                    &p.fmt.right,
                    &p.br.fg,
                );
                p.text(
                    &share,
                    rect(g.x_share_left, y + 4.0, SHARE_COL_W, 18.0),
                    &p.fmt.caption_right,
                    &p.br.muted,
                );
            }
        }
    }

    // 滚动条只在装不下时出现；拖动见 `begin_scroll_drag`（只拖滑块，不做点轨道翻页）。
    draw_vscroll(
        p,
        rect(r.right - 11.0, g.list_top, 6.0, g.max_rows as f32 * g.row_h),
        count,
        g.max_rows,
        scroll,
    );

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
        // 同一句 `data_home` 在设置页那格是夹过的，这一格从前没有：警告那半句本身就吃掉
        // 大半行，路径一长就越过卡片边、压到右边那张趋势卡上。夹取留头也留尾 —— 尾巴是路径。
        &p.clip(
            &table_footer_text(snap, lane, tab),
            &p.fmt.caption,
            r.right - r.left - g.pad * 2.0,
        ),
        rect(r.left + g.pad, foot_y, r.right - r.left - g.pad * 2.0, 16.0),
        &p.fmt.caption,
        &p.br.muted,
    );
}

/// 左表页脚那一句：应用榜被截断时把"只列了多少"摆在最前面，再念数据源/缺读。
///
/// 截断从前是静默的：100 个以外的应用既不在屏上、也不在导出里，而屏上没有任何
/// 一个字说这是一份前 100 名的榜。`footer_line` 本身不动（有用例钉着它的调用形态）。
fn table_footer_text(snap: &Snapshot, lane: &str, tab: usize) -> String {
    let foot = footer_line(&snap.data_home, &snap.gaps, lane);
    if tab == 1 && snap.app_rows_total > snap.apps.len() {
        return format!(
            "应用太多，只列了前 {} 个（共 {} 个） · {foot}",
            snap.apps.len(),
            snap.app_rows_total
        );
    }
    foot
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
    p.round(rect(x, y, w, 18.0), R_CONTROL, &p.br.header, None);
    p.text(
        // 气泡是定宽那颗（150 与卡片宽取小），而句子里吃的是库里的日期串与千分位数字：
        // "2026-11-01 · 1,234,567 次" 就到 ~165 px。不夹就是字越出圆角框自己的边。
        &p.clip(text, &p.fmt.small, w - 12.0),
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

/// `kind` 只认 `bar`（大小写与首尾空格都不挑），其它一律按折线画。
///
/// 作者把控件类型写成字符串是本协议里的常态，写错一档的代价不该是"整张图消失"或报错：
/// 折线照样看得出趋势，而这是那一族控件共用的口径（未知控件退化成一行可读的文本，
/// 不是一片空白）。
fn chart_is_bar(kind: &str) -> bool {
    kind.trim().eq_ignore_ascii_case("bar")
}

/// 一张**插件**图表的画法（纯函数，不碰 D2D）：绘图区、纵轴顶格、横轴标哪几格、空数据怎么说。
///
/// 抽出来是因为这几条都是"看着对、其实各算了一遍"的高发地：
/// · 点高与纵轴刻度必须共用 `axis_top` 那一个分母 —— 内置页就栽过"每档少报 12%"，
///   只有拿刻度反推高度才看得出来（`the_peak_point_reads_back_as_the_peak_count_through_the_ticks`）；
/// · 绘图区边距沿用 [`line_plot`]，两处各挑一套边距，同一份数据在插件面板与内置看板页
///   会画出两个不一样的高度；
/// · 顶格**永不取 0**：空数据时 `axis_top(0)` 自己兜成 4，除数那一头不留坑；
/// · 横轴文本从**最新那一格**往前每隔 `label_step` 标一个，所以最后一天一定有文本
///   （90 个点全标就是一片糊）。
#[derive(Debug)]
struct PluginChartPlan {
    plot: D2D_RECT_F,
    /// 纵轴顶格（也是点高的分母），恒 ≥ 4
    top: i64,
    /// 0 = 一个都不标（没有文本或没有数据）；否则从最后一格往前每这么多格标一个
    label_step: usize,
    empty: bool,
    bars: bool,
}

fn plugin_chart_plan(card: D2D_RECT_F, kind: &str, points: &[i64]) -> PluginChartPlan {
    let plot = line_plot(card);
    let peak = points.iter().copied().max().unwrap_or(0);
    PluginChartPlan {
        plot,
        top: axis_top(peak),
        label_step: if points.is_empty() {
            0
        } else {
            points.len().div_ceil(PV_CHART_MAX_LABELS)
        },
        empty: points.is_empty(),
        bars: chart_is_bar(kind),
    }
}

/// 第 `i` 格的横轴文本该不该标出来（`n` = 点数）。最后一格必标，其余往前每 `step` 格一个。
///
/// 单独一个函数是因为画与测都要问同一件事：按"索引整除"来标会让最后一天有没有文本
/// 取决于点数是不是步长的整数倍 —— 那正是趋势图最需要看的那一天。
fn chart_label_at(n: usize, step: usize, i: usize) -> bool {
    step > 0 && i < n && (n - 1 - i).is_multiple_of(step)
}

/// 第 `j` 个横轴文本画在哪一格。`labels` 与 `points` **尾对齐**（`Widget::Chart` 的
/// 文档就是这么写的）：文本比点少时，缺的是前面那些旧点的文本，而不是把最近几天的
/// 文本挤到最早几格去 —— 后者每个字都对、归属全错，只有数格子才看得出来。
fn chart_label_index(n: usize, len: usize, j: usize) -> usize {
    n.saturating_sub(len) + j
}

/// 横轴文本那一格的矩形：居中在它那个点上方，但**左右都不许越出绘图区**。
///
/// 最右那一格是右对齐画的，从前只把左边夹进绘图区：`cell_w` 上限 90 ⇒ 它的右端
/// 会伸到绘图区外面 45 px（比卡片右边距还宽），被内容区裁剪吃掉最后一个字。
fn chart_label_rect(plot: D2D_RECT_F, cx: f32, cell_w: f32) -> D2D_RECT_F {
    let left = (cx - cell_w / 2.0).clamp(plot.left, (plot.right - cell_w).max(plot.left));
    rect(left, plot.bottom + 4.0, cell_w, 14.0)
}

/// 画一张插件图表。线、柱、网格、纵轴数字全部复用内置页那一套几何函数。
fn draw_plugin_chart(p: &P<'_>, r: D2D_RECT_F, kind: &str, labels: &[String], points: &[i64]) {
    p.card(r, R_CARD);
    let plan = plugin_chart_plan(r, kind, points);
    let plot = plan.plot;
    let grid_weak = p.brush(with_alpha(p.pal.grid, 0.55));
    for i in 0..=4 {
        let y = line_grid_y(plot, i);
        if let Some(g) = &grid_weak {
            p.line(plot.left, y, plot.right, y, g, 1.0);
        }
    }
    if plan.empty {
        // 空数据不许留白（3.2.2）：网格与基线照画，中间说一句「暂无数据」。
        // 「面板是空的但没有任何提示」是这一族 UI 明令禁止的状态 —— 用户只能猜。
        p.text(
            "暂无数据",
            rect(
                plot.left,
                (plot.top + plot.bottom) / 2.0 - 9.0,
                plot_width(plot),
                18.0,
            ),
            &p.fmt.caption_center,
            &p.br.muted,
        );
        return;
    }
    for i in 0..=4 {
        let y = line_grid_y(plot, i);
        p.text(
            &thousands(line_tick_value(plan.top, i).max(0) as u64),
            rect(plot.left - 48.0, y - 7.0, 44.0, 14.0),
            &p.fmt.caption_right,
            &p.br.muted,
        );
    }
    let n = points.len();
    if plan.bars {
        let slot = plot_width(plot) / n.max(1) as f32;
        let bw = (slot * 0.6).max(2.0);
        for (i, v) in points.iter().enumerate() {
            let cx = line_point_x(plot, n, i);
            let y = line_point_y(plot, plan.top, (*v).max(0));
            p.round(
                rect(cx - bw / 2.0, y, bw, (plot.bottom - y).max(1.0)),
                R_MICRO,
                &p.br.accent,
                None,
            );
        }
    } else {
        let pt = |i: usize| Vector2 {
            X: line_point_x(plot, n, i),
            Y: line_point_y(plot, plan.top, points[i].max(0)),
        };
        let pts: Vec<Vector2> = (0..n).map(pt).collect();
        p.area(&pts, plot);
        for i in 1..n {
            let a = pt(i - 1);
            let b = pt(i);
            unsafe {
                p.rt.DrawLine(a, b, &p.br.accent, 2.0, None);
            }
        }
        if n == 1 {
            let c = pt(0);
            p.round(
                rect(c.X - 4.0, c.Y - 4.0, 8.0, 8.0),
                R_MICRO,
                &p.br.accent,
                None,
            );
        }
    }
    // 横轴：一格给 `slot × step` 宽（最后一格靠右、按右对齐收），文本自己带长度上界 ——
    // 这块 UI 不按矩形裁切，画得下与否取决于传进去几格宽。
    let slot = plot_width(plot) / n.max(1) as f32;
    let cell_w = (slot * plan.label_step as f32).clamp(28.0, 90.0);
    for (j, text) in labels.iter().enumerate() {
        let i = chart_label_index(n, labels.len(), j);
        if !chart_label_at(n, plan.label_step, i) {
            continue;
        }
        let cx = line_point_x(plot, n, i);
        let last = i + 1 == n;
        p.text(
            &p.clip(text, &p.fmt.caption, cell_w),
            chart_label_rect(plot, cx, cell_w),
            if last {
                &p.fmt.caption_right
            } else {
                &p.fmt.caption_center
            },
            &p.br.muted,
        );
    }
}

fn draw_line_chart(p: &P<'_>, r: D2D_RECT_F, snap: &Snapshot, hover: Option<usize>) {
    p.card(r, R_CARD);
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
        &p.fmt.caption_right,
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
            &p.fmt.caption,
            &p.br.muted,
        );
    }

    let plot = line_plot(r);
    // 网格压到 55% 透明：四条同样的线一起抢，折线反而不显眼。
    let grid_weak = p.brush(with_alpha(p.pal.grid, 0.55));
    for i in 0..=4 {
        let y = line_grid_y(plot, i);
        if let Some(g) = &grid_weak {
            p.line(plot.left, y, plot.right, y, g, 1.0);
        }
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
    let pts: Vec<Vector2> = (0..n).map(pt).collect();
    p.area(&pts, plot);
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
            R_MICRO,
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
            &p.fmt.caption_right,
            &p.br.muted,
        );
    }
    if let Some((first, _)) = series.first() {
        // 窗口起点就是序列第一项交回的那串日期，不在这儿按长度反推。
        let left = first.chars().skip(5).collect::<String>();
        p.text(
            // 这一格只有 60 px，而串是库里交回的那一项日期：不是 `YYYY-MM-DD` 形状时
            // （导入的旧库、带时间的日期）整串会画进第一根柱子里。
            &p.clip(&left, &p.fmt.caption, 60.0),
            rect(plot.left, plot.bottom + 4.0, 60.0, 14.0),
            &p.fmt.caption,
            &p.br.muted,
        );
    }
    p.text(
        "今天",
        rect(plot.right - 30.0, plot.bottom + 4.0, 30.0, 14.0),
        &p.fmt.caption_right,
        &p.br.muted,
    );
    // 光标压在哪一天：把那一点圈出来，读数贴着它画（旧 WebView 三张图都有这枚 tip，
    // 换原生时只留了热力板那一处）。
    if let Some(i) = hover {
        if let Some((d, v)) = series.get(i) {
            let c = pt(i);
            p.round(
                rect(c.X - 5.0, c.Y - 5.0, 10.0, 10.0),
                R_MICRO,
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
    p.card(r, R_CARD);
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
        &p.fmt.caption_right,
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
            &p.fmt.caption,
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
            p.round(bar, R_MICRO, &b, None);
        }
        // 压住的那一根：**只描一圈，不重填**。以前照抄热力板那套"填卡片底色 + 描边"，
        // 键帽本来就是空的所以没事，柱状图里这一填就比邻柱更淡，一眼像"这一小时没数据"。
        if hover == Some(i) {
            if let Some(ghost) = p.brush(with_alpha(p.pal.accent, 0.0)) {
                p.round(
                    rect(x - 2.0, plot.bottom - hp - 2.0, bw + 4.0, hp + 4.0),
                    R_MICRO,
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

fn make_tray(
    snap: &Arc<Mutex<Snapshot>>,
) -> (
    Option<TrayIcon>,
    Option<CheckMenuItem>,
    Option<CheckMenuItem>,
) {
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
    let clipboard_history = MenuItem::with_id("clipboard_history", "剪贴板历史", true, None);
    let clipboard_pause_15 =
        MenuItem::with_id("clipboard_pause_15", "暂停记录 15 分钟", true, None);
    let clipboard_pause_60 =
        MenuItem::with_id("clipboard_pause_60", "暂停记录 60 分钟", true, None);
    // 暂停只有两档入口时，点错一次只能等满或重启；这一项是它的反向出口。
    let clipboard_resume = MenuItem::with_id("clipboard_resume", "恢复剪贴板记录", true, None);
    let clipboard_item = CheckMenuItem::with_id(
        "clipboard_pause",
        "记录剪贴板",
        true,
        focusflow_core::config::instance().get_bool("clipboard", "enabled", true),
        None,
    );
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
        &clipboard_history,
        &clipboard_pause_15,
        &clipboard_pause_60,
        &clipboard_resume,
        &clipboard_item,
        &pause,
        &sep,
        &quit,
    ]) {
        tracing_note(&format!("托盘菜单构建失败：{e}"));
        return (None, None, None);
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
                    return (None, None, None);
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
            (Some(t), Some(pause), Some(clipboard_item))
        }
        Err(e) => {
            tracing_note(&format!("托盘创建失败：{e}"));
            (None, None, None)
        }
    }
}

/// 托盘 tooltip：今日次数 + 采集状态（采集开着时才有后半截）+ 剪贴板暂停截止时刻。
fn tray_tooltip(today: i64) -> String {
    tray_tooltip_with(
        today,
        collect::is_listening(),
        collect::is_paused(),
        collect::legacy_running(),
        focusflow_core::clipboard::pause_until_ms(),
    )
}

/// `tray_tooltip` 的纯核：五个读数各自从哪来由调用方给，排版本身可以被单测。
fn tray_tooltip_with(
    today: i64,
    listening: bool,
    paused: bool,
    legacy: bool,
    clip_pause_until: i64,
) -> String {
    // 次数是 `u64` 的写法：坏数据/外部导入留下的负数若直接 `as u64`，托盘上会念出
    // 一个二十位数。真数不会为负，夹到 0 与其余几处读数同一个口径。
    let mut tip = format!("FocusFlow · 今日 {}", thousands(today.max(0) as u64));
    if listening {
        tip.push_str(if paused {
            " · 已暂停"
        } else {
            " · 采集中"
        });
        if legacy {
            tip.push_str(" · 旧版在跑");
        }
    }
    if let Some(at) = clipboard_pause_label(clip_pause_until) {
        tip.push_str(&format!(" · 剪贴板暂停至 {at}"));
    }
    tip
}

/// 暂停截止时刻的 `HH:MM`（本地时区）。0 = 没暂停；时间戳读不出来也按"没暂停"处理。
/// 同一段剪贴板内容 1 秒内算同一次复制。
///
/// 剪贴板服务/历史面板会把一次复制重复发布成好几次变化，监听那一头只能照收（库里靠
/// dedupe 折成一行）；插件事件这边要的是"一次手势一次回调"，所以按内容哈希压一个短窗。
fn clip_event_is_duplicate(
    last: Option<&(String, std::time::Instant)>,
    hash: &str,
    now: std::time::Instant,
) -> bool {
    last.is_some_and(|(h, at)| {
        h == hash && now.duration_since(*at) < std::time::Duration::from_secs(1)
    })
}

fn clipboard_pause_label(until_ms: i64) -> Option<String> {
    if until_ms <= 0 {
        return None;
    }
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(until_ms)
        .single()
        .map(|dt| dt.format("%H:%M").to_string())
}

/// 托盘两档「暂停记录」共用的动作：写运行时暂停位、更新状态行与托盘提示。
fn pause_clipboard(hwnd: HWND, minutes: i64) {
    focusflow_core::clipboard::pause_for(minutes.saturating_mul(60));
    let at = clipboard_pause_label(focusflow_core::clipboard::pause_until_ms())
        .unwrap_or_else(|| "稍后".to_string());
    let msg = format!("剪贴板记录已暂停 {minutes} 分钟（至 {at}）");
    tracing_note(&msg);
    if let Some(app) = unsafe { app_ptr(hwnd) } {
        app.status = Some(msg);
        if let Some(t) = &app.tray {
            let today = app.snap.lock().map(|s| s.today).unwrap_or(0);
            let _ = t.set_tooltip(Some(tray_tooltip(today)));
        }
    }
    clipboard_popup::notify_changed();
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

/// 与 `pause_clipboard` 成对：点「恢复剪贴板记录」立刻回到记录状态。
fn resume_clipboard(hwnd: HWND) {
    let was_paused = focusflow_core::clipboard::is_paused();
    focusflow_core::clipboard::resume_now();
    let msg = if was_paused {
        "剪贴板记录已恢复"
    } else {
        "剪贴板记录本来就没暂停"
    };
    tracing_note(msg);
    if let Some(app) = unsafe { app_ptr(hwnd) } {
        app.status = Some(msg.to_string());
        if let Some(t) = &app.tray {
            let today = app.snap.lock().map(|s| s.today).unwrap_or(0);
            let _ = t.set_tooltip(Some(tray_tooltip(today)));
        }
    }
    clipboard_popup::notify_changed();
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
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
    rect(vb.right - 40.0, vb.top + 1.0, 34.0, 24.0)
}

/// `[hotkey]` 某个键的中文名（与设置页那一行的标签同一份）——撞车时要点名是哪一条。
fn hotkey_label(key: &str) -> &'static str {
    match key {
        "toggle_window" => "唤出窗口热键",
        "snip" => "区域截图热键",
        "snip_annotate" => "标注截图热键",
        "clipboard" => "剪贴板历史热键",
        _ => "未命名热键",
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
/// 设置页那颗「立即备份」交回状态行的那一句话。**纯函数**：屏上这句由两份独立的事实拼成
/// （哪些库没兜住 / 拷贝前那批增量落没落成），两半各有一条腿盯着，写在 match 分支里就测不到。
fn backup_status_line(count: usize, failed: &[String], missed_batch: bool) -> String {
    let head = if failed.is_empty() {
        format!("备份完成：{count} 份")
    } else {
        format!("备份部分失败：{count} 份，没兜住 {}", failed.join("、"))
    };
    if missed_batch {
        format!(
            "{head}，{}",
            focusflow_core::db::maintenance::batch_missed_note()
        )
    } else {
        head
    }
}

/// 配置写不进持久化队列时要补的那半句（空串 = 写进去了，什么都别说）。
///
/// 喂进来的必须是 `config::set_now` 的结局：它当场走完 `save()`，那个 `Err` 就是"这一笔
/// 没落盘"。`config::set` 只把改动投给去抖线程、**恒回 `Ok(()))`**，拿它的结局来拼这句话
/// 永远拼不出东西 —— 屏上那一格读运行时、`config.ini` 读的是老一档，两边分家到下次开机才露出来。
fn config_write_note(what: &str, r: std::result::Result<(), String>) -> String {
    match r {
        Ok(()) => String::new(),
        Err(e) => format!(
            "（{what}没写进 config.ini：{e}。这一次改动只到本次运行，重启会回到文件里那一档）"
        ),
    }
}

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

/// 这一格宽度**最多**能念多少个字符（按字符，不按字节），按**这一格真正用的那一档字号**
/// 换算，下限是 1。
///
/// 为什么要有这一句：这个 UI 没有"装不下"这回事 —— 每一档字体都是 `NO_WRAP`，而 `DrawText`
/// 不按传进来的矩形裁切。现量在案：把数据根与"已安排搬到"两个路径都换成 114 / 91 个字符，
/// 底部那句「注意：没能搬到 …」一路画到窗口右边界之外，用户看到的是括号都没闭合的半句。
///
/// 一个全角字占多少像素 = 这一档的字号：全中文时一个字吃掉约一个字号宽，路径与英文更窄，
/// 所以按它算出来的预算是保守的 —— 宁可少念几个字，也不会画到隔壁卡上。预算跟着矩形宽度走
/// （不是写死的常数）：窗口能拖窄，写死的数在窄窗口里照样顶出去。
///
/// 为什么下限不能兜到 8：排行页摘要那一格在 960px 窗口下只剩约 84px，8 个全角字要 100px ——
/// 兜底就成了"预算说装得下、屏上画到胶囊底下"。字号也不能借隔壁那档：两档不相等时
/// 这把尺子量的不是这一格。
///
/// ⚠ 这一句只按**字符数**折算，是"全角那一档"的上界：数字与拉丁字母实际只有约半个全角宽，
/// 光靠它会把屏上明明画得下的句子夹成省略号。屏上真装不装得下由 [`clip_to_cell`] 实测复核。
fn char_budget_at(rect_w: f32, per_char: f32) -> usize {
    if !rect_w.is_finite() || !per_char.is_finite() || rect_w <= 0.0 || per_char <= 0.0 {
        return 1;
    }
    ((rect_w / per_char).floor() as usize).max(1)
}

/// 中间省略：留头留尾，中间换成一个 `…`。
///
/// 为什么不砍尾巴：这一行里最要紧的两样是"在说什么事"（句首）与"哪个目录/哪个文件"
/// （路径的**末段**）。砍尾等于把 `…\迁移测试\data` 换成一个看不懂的省略号，而中间那段
/// `E:\mydata\agentdata\code\FocusFlow\.scratch\ab_longpath\Program Files (x86)\` 恰恰是
/// 信息量最低的一截。按字符走，不按字节 —— 中文在 UTF-8 里是三字节，按字节切会切坏字，
/// 而 release 是 `panic = "abort"`。
fn clip_middle(text: &str, max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return text.to_string();
    }
    // 省略号占一格，剩下的按 6:4 分给头与尾（句首比句尾更需要认得出来）
    let keep = max_chars.saturating_sub(1);
    let head = keep * 6 / 10;
    let tail = keep - head;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(chars[chars.len() - tail..].iter());
    out
}

/// 量宽度用的那一档：与 `f` **同字号同字重**的左起档。
///
/// 为什么不能直接拿 `f` 量：右对齐与居中那两档在 `maxWidth = ∞` 的布局里，交回来的
/// `widthIncludingTrailingWhitespace` 是 **0** —— 字是按盒子的右边／中线摆的，"宽度"这个
/// 字段整个塌掉。现量：同一句 21 个字用 `caption_right` 量回 0.0、用 `caption` 量是 181.0；
/// 星期分布柱顶那档 `center_small` 同样量回 0.0。拿 0 当"装得下"等于给这一族格子摘掉了夹取，
/// 症状不是报错，是字照画到邻居身上。
///
/// 副本走 `text_format`（本仓每一档都是它建的：同一个族名、同一个 locale、同一句 `NO_WRAP`），
/// 所以同字号同字重就是它自己，只差对齐。字号与字重都从 `f` 现取，不在这里重写常数。
fn measure_format(dw: &IDWriteFactory, f: &IDWriteTextFormat) -> Option<IDWriteTextFormat> {
    let (size, weight) = unsafe { (f.GetFontSize(), f.GetFontWeight()) };
    text_format(dw, size, weight).ok()
}

/// 夹到这一格**真能画下**：先问全角尺子，尺子说装得下就直接放行（一次 COM 都不问）；
/// 尺子说装不下时再用 DirectWrite 量一次实际宽度复核。
///
/// 为什么要复核：`char_budget_at` 按"每个字都占满一个字号"折算，那是**全角**那一档。
/// 而这一族格子里大半的句子是数字与日期 —— 拉丁字母、数字、逗号只有约半个全角宽，
/// 于是屏上明明画得下的句子也被夹出省略号（现量：「今日 9,618 / 20,000」实测 115.7 DIP，
/// 格子 150 DIP，旧尺子按 11 个字算，画出来是「今日 9,6…000」）。
///
/// 复核后确实装不下才夹，且仍夹到尺子那一档：这块 UI 每一档都是 `NO_WRAP` 且不设
/// `DWRITE_TRIMMING`，越界不是"被截断"而是照画到邻居身上，宁可少念也不放行。
/// 量不出来（工厂或布局建不起来）同样退回尺子。
///
/// 字号取**画这一格的那档字体自己**（`GetFontSize`），不在调用点重写常数：写两遍就是两把尺子。
/// 量的那一档是它的左起副本，见 [`measure_format`] —— 右对齐与居中直接量回来的是 0。
fn clip_to_cell(dw: &IDWriteFactory, text: &str, f: &IDWriteTextFormat, rect_w: f32) -> String {
    let budget = char_budget_at(rect_w, unsafe { f.GetFontSize() });
    if text.chars().count() <= budget {
        return text.to_string();
    }
    match measure_format(dw, f).and_then(|m| measure_text(dw, text, &m)) {
        Some(w) if w <= rect_w => text.to_string(),
        _ => clip_middle(text, budget),
    }
}

/// 多行动态文本：先按 [`capped_lines`] 定行数（超上限时最后一行说还有几行），
/// 再逐行夹宽。整段一起夹会把换行当普通字符量 —— 宽的那一行会把后面几行整段吃掉，
/// 而行数又不按高度裁，多出来的行直接画到下一个控件身上。
fn clip_text_lines(p: &P<'_>, text: &str, f: &IDWriteTextFormat, width: f32, max: usize) -> String {
    capped_lines(text, max)
        .iter()
        .map(|line| p.clip(line, f, width))
        .collect::<Vec<_>>()
        .join("\n")
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
        // 插件页那几条失败只在 `status` 里有一份（同一句话也被顶栏回执读走）：
        // 不列在这里，坏包、动作抛错在模块里就是一行灰色的"已安装"近邻。
        || line.starts_with("安装失败：")
        || line.starts_with("卸载失败：")
        || line.starts_with("插件动作失败：")
        || line.starts_with("插件字段回写失败：")
        || line.starts_with("插件开关失败：")
        || line.contains("剪贴板没进去")
}

/// 顶栏那条全局回执：只有插件页（`view == 4`）用得上。
///
/// 插件页的安装 / 卸载 / 动作失败都只写 `status`，而那一页的正文没有画它的位置
/// （设置页有卡片底部那一行，插件页没有）—— 从前坏包点下去、插件动作抛错在屏上
/// 都是"点了没反应"。`FocusFlow · 效率追踪器` 与三颗窗口按钮之间本来就空着。
fn topbar_status_line(
    view: usize,
    status: Option<&str>,
    w: f32,
) -> Option<(String, bool, D2D_RECT_F)> {
    if view != 4 {
        return None;
    }
    let line = status?.trim();
    if line.is_empty() {
        return None;
    }
    let left = 16.0 + 250.0;
    let right = win_buttons(w)[0].left - 12.0;
    let width = right - left;
    if width < 120.0 {
        return None;
    }
    Some((
        line.to_string(),
        status_is_warning(line),
        rect(left, 16.0, width, 20.0),
    ))
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
            return format!("四条热键都没挂上{off_tail}");
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
fn unregister_combos(manager: Option<&GlobalHotKeyManager>, list: [Option<HotKey>; 4]) {
    let Some(manager) = manager else { return };
    let all: Vec<HotKey> = list.into_iter().flatten().collect();
    if !all.is_empty() {
        let _ = manager.unregister_all(&all);
    }
}

/// 设置页里能改、启动时要注册的那几条热键（`[hotkey]` 段的键名）。
/// 这份清单只写一次：写入前的校验与改完重注册都读它，加一条不用改第二处。
const HOTKEY_KEYS: [&str; 4] = ["toggle_window", "snip", "snip_annotate", "clipboard"];

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

/// 四条组合的键名，**顺序就是它们各自落进哪个字段 / 状态行点名的顺序**：
/// 0＝`toggle_window`（唤出主窗口）、1＝`snip`（松手即存）、2＝`snip_annotate`（进标注）、
/// 3＝`clipboard`（剪贴板历史弹窗）。
const COMBO_KEYS: [&str; 4] = ["toggle_window", "snip", "snip_annotate", "clipboard"];

/// 这一次启动要挂哪几条组合：总开关 `[hotkey] enabled` 关掉 ⇒ 空表；剪贴板功能
/// `[clipboard] enabled` 关掉 ⇒ 只挂前三条（不占用它的键）。
///
/// 抽成纯函数是因为"关掉之后还剩几条在占键"这种形状编译器与现有腿都不响。
fn combo_keys(enabled: bool, clipboard_enabled: bool) -> &'static [&'static str] {
    if !enabled {
        &[]
    } else if clipboard_enabled {
        &COMBO_KEYS
    } else {
        &COMBO_KEYS[..3]
    }
}

/// 一次按键只会被处理一次：`global-hotkey` 在 Windows 上先发 Pressed、松手后再发
/// Released。两条都处理的话，唤出窗会开一下又藏、剪贴板弹窗开了又关。
fn hotkey_is_press(state: HotKeyState) -> bool {
    matches!(state, HotKeyState::Pressed)
}

/// 开机注册三条热键的结局。写成结构体而不是五元组：调用处按名字取值，
/// 也不用为了多带一句状态把返回类型越拧越复杂。
struct HotkeysAtBoot {
    manager: Option<GlobalHotKeyManager>,
    toggle: Option<HotKey>,
    snip: Option<HotKey>,
    annotate: Option<HotKey>,
    clipboard: Option<HotKey>,
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
    let cfg = focusflow_core::config::instance();
    let enabled = cfg.get_bool("hotkey", "enabled", true);
    let clipboard_enabled = cfg.get_bool("clipboard", "enabled", true);
    let mut slots: [Option<HotKey>; 4] = [None, None, None, None];
    let mut parts: Vec<(&'static str, String, Option<String>)> = Vec::new();
    let mut offs: Vec<&'static str> = Vec::new();
    for (i, key) in combo_keys(enabled, clipboard_enabled).iter().enumerate() {
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
        Some("全局热键没开（[hotkey] enabled = false）：四条组合都不注册".to_string())
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
        clipboard: slots[3],
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

/// 静默启动那一趟欠一次材质：DWM 在窗口从没显示过之前会把深色属性/背板丢掉，所以
/// `main()` 里不贴，等第一次真的唤出时补。只在这一条路上是 true。
static GLASS_ON_FIRST_SHOW: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 开机静默进托盘这个意图，今天有两个入口：开机自启那条快捷方式带的 `--hidden`
/// （`crates/core/src/autostart.rs` 写进 `.lnk` 的参数），和设置页那行 `[gui] start_to_tray`
/// （持久版）。两条在改之前都没人读 —— 参数全仓没有解析点，配置键只有界面那一行。
const HIDDEN_ARG: &str = "--hidden";

/// 命令行参数优先于配置：带 `--hidden` 的那一次一定静默，不管配置里写着什么。
fn hidden_at_startup(args: &[String], start_to_tray: bool) -> bool {
    args.iter().any(|a| a == HIDDEN_ARG) || start_to_tray
}

/// `[gui] start_to_tray` 的现量读点。默认**关**：接线之前 true 是个死值，接上之后还留 true
/// 就等于"每个新装用户第一次打开只看得见托盘图标、找不到程序窗口"。
fn start_to_tray_cfg() -> bool {
    focusflow_core::config::instance().get_bool("gui", "start_to_tray", false)
}

unsafe fn bring_to_front(hwnd: HWND) {
    // 显示**之前**先把渲染目标建好并画完整一帧。
    //
    // 为什么必须有这一步：收进托盘时 `hide_to_tray` 把目标放掉了，此刻 DWM 手里没有旧内容
    // 可以显示。先显示再等 `WM_PAINT` 的话，屏上会出现一小段"只有底色、没有内容"的面板 ——
    // 2026-10-31 连拍现量到 9 帧、t=19 ms 起约 110 ms（`.scratch/hide_show_first_frame.ps1`；
    // 同一块屏幕位置在窗口还藏着时 diff_vs_reference=0.449，证明那不是桌面透出来，
    // 而是一块画了底、没画内容的表面）。
    //
    // 代价由这一侧付：建目标 + 画一帧落在"按下唤出"与"窗口出现"之间，用户看到的是打开略慢，
    // 而不是一眼能看见的白面板 —— 这正是接这条改动时定下的取舍。
    //
    // `app_ptr` 交回的是共享引用，换写法与下面 `apply_glass` 那一处一致（同一时刻只有这条
    // 路径在改 App，且消息循环单线程）。
    if let Some(app) = app_ptr(hwnd) {
        if let Some(app) = (app as *const App as *mut App).as_mut() {
            app.draw(hwnd);
        }
    }
    let _ = ShowWindow(hwnd, SW_SHOW);
    // 只在**真的最小化**时还原：`SW_RESTORE` 对最大化的窗口是"还原成普通尺寸"，
    // 无条件调用会把用户最大化好的窗口在唤出时压回普通档。
    if unsafe { IsIconic(hwnd) }.as_bool() {
        let _ = ShowWindow(hwnd, SW_RESTORE);
    }
    // 静默启动时存档说"上次是最大化"：窗口一直藏着，最大化留到这一刻补。
    if MAIN_MAXIMIZE_ON_SHOW.swap(false, std::sync::atomic::Ordering::Relaxed) {
        let _ = ShowWindow(hwnd, SW_MAXIMIZE);
    }
    // 上屏**之后**才催：反过来线程可能先醒一次、问到的还是"没人看"，那一催就白花了，
    // 症状照旧是点开窗口先看见上一份旧数。
    poke_snapshot();
    let _ = SetForegroundWindow(hwnd);
    if GLASS_ON_FIRST_SHOW.swap(false, std::sync::atomic::Ordering::Relaxed) {
        // 补贴材质要把 `mica` 一起改：只改 DWM 不改那一位，界面会按不透明档画、底下却是
        // Mica，两层深色叠在一起。放在唤窗之后、消息返回之前 —— 首帧的 WM_PAINT 还在队列里。
        if let Some(app) = app_ptr(hwnd) {
            if let Some(app) = (app as *const App as *mut App).as_mut() {
                app.mica = apply_glass(hwnd);
            }
        }
        fix_backdrop(hwnd);
    }
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

/// 主窗口那一套字号/字重。单列出来是为了让 `text_tier_tests` 能建一份真的来问
/// `GetFontWeight()` —— 三档字重是本轮观感的骨架，光靠源码里看起来对不对不算数。
fn build_formats(dw: &IDWriteFactory) -> windows::core::Result<Formats> {
    Ok(Formats {
        h1: text_format(dw, 22.0, UI_WEIGHT_EM)?,
        value: text_format(dw, 25.0, DWRITE_FONT_WEIGHT_BOLD)?,
        caption: text_format(dw, CAPTION_PT, UI_WEIGHT_REG)?,
        caption_right: right_format_w(dw, CAPTION_PT, UI_WEIGHT_REG)?,
        caption_center: center_format_w(dw, CAPTION_PT, UI_WEIGHT_REG)?,
        nav_item: text_format(dw, SETTINGS_TEXT_PT, UI_WEIGHT_REG)?,
        body: text_format(dw, BODY_PT, UI_WEIGHT)?,
        small: text_format(dw, SMALL_PT, UI_WEIGHT)?,
        right: right_format(dw, SMALL_PT)?,
        center_small: center_format(dw, SMALL_PT)?,
        center_tiny: center_format(dw, 9.0)?,
        tab: center_format(dw, BODY_PT)?,
        settings_h: text_format(dw, 16.0, DWRITE_FONT_WEIGHT_BOLD)?,
        row_name: text_format(dw, 15.0, UI_WEIGHT)?,
        table_name: text_format(dw, BODY_PT, UI_WEIGHT)?,
        text13: text_format(dw, SETTINGS_TEXT_PT, UI_WEIGHT)?,
        text13_em: text_format(dw, SETTINGS_TEXT_PT, UI_WEIGHT_EM)?,
        text12: text_format(dw, SETTINGS_HELP_PT, UI_WEIGHT_REG)?,
        text12_em: text_format(dw, SETTINGS_HELP_PT, UI_WEIGHT_EM)?,
        center13: center_format_w(dw, SETTINGS_TEXT_PT, UI_WEIGHT)?,
    })
}

/// 量一行文字的实际宽度（DIP）。`P::text` 走 `DrawText`，拿不到度量，所以单独建一次
/// layout；只给页签下划线用，一次重画最多问一次。建不出来就交回 `None`（退回旧宽度）。
fn measure_text(dw: &IDWriteFactory, s: &str, f: &IDWriteTextFormat) -> Option<f32> {
    let u: Vec<u16> = s.encode_utf16().collect();
    let layout = unsafe { dw.CreateTextLayout(&u, f, f32::MAX, f32::MAX).ok()? };
    let mut m = DWRITE_TEXT_METRICS::default();
    unsafe { layout.GetMetrics(&mut m).ok()? };
    Some(m.widthIncludingTrailingWhitespace)
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

/// 主窗口所有文字的基准字重。他 2026-10-30 先点了"名字加粗"，看过之后又要**整体**加粗 ⇒
/// 常规那一档抬到 600。⚠ 雅黑只有 400/700 两档，汉字在 600 上会落到 700（真的粗），
/// 拉丁字母与数字落在 Segoe UI Semibold 上 —— 两边的"粗"程度本来就不同，这是字体本身的
/// 限制，不是两种尺子。
const UI_WEIGHT: DWRITE_FONT_WEIGHT = DWRITE_FONT_WEIGHT_SEMI_BOLD;
/// 强调档：比基准再重一级。拉丁字母上看得见差别；汉字两边都是 700，只能靠字号或颜色分。
const UI_WEIGHT_EM: DWRITE_FONT_WEIGHT = DWRITE_FONT_WEIGHT_BOLD;
/// 次要信息的常规档。三档字重是本轮观感的骨架：
/// 展示 700（`h1` / `value` / `settings_h`）→ 强档 600（正文、名字、要扫的数字）
/// → 常规 400（说明、表头、刻度、脚注）。
///
/// ⚠ 雅黑只有 400/700 两档：600 的汉字会落到 700，所以"600 与 700"在中文上没有区别，
/// 层级只能靠 **regular↔strong 这一跳 + 颜色**。次要文字若一律写 600，整屏就是一片同重的粗字。
const UI_WEIGHT_REG: DWRITE_FONT_WEIGHT = DWRITE_FONT_WEIGHT_NORMAL;

/// 设置页值框与正文那一档的字号（`text13` / `center13`），以及说明行那一档（`text12`）。
///
/// 说明行的宽度守卫（`data.rs` 的 `every_help_row_fits_before_its_value_box`）现在不再自己
/// 按字号换算：它拿 `build_formats` 交回的 `text12` 那一档实测宽度，所以字号一改判据跟着改。
/// 两档还是要单列成常量 —— 这块 UI 不按矩形裁切（`NO_WRAP`、不设 `DWRITE_TRIMMING`），
/// 说明越宽就照画进右边那颗值框，宽度只许有一把尺子。
pub(crate) const SETTINGS_TEXT_PT: f32 = 14.0;
pub(crate) const SETTINGS_HELP_PT: f32 = 13.5;
/// 次要信息那一档（`caption` / `caption_right` / `caption_center`）的字号。
///
/// 列出来是因为排行页摘要那一格的**字符预算**要按它换算。以前那格直接借了
/// `char_budget`（按 `SETTINGS_HELP_PT` 算）：两档字号不相等，预算与屏上像素就不是
/// 一把尺子 —— 窄窗口下按"装得下"放行、字照画到胶囊底下。
const CAPTION_PT: f32 = 13.0;
/// 正文那一档（`body` / `tab` / `table_name`）的字号。
///
/// 与前两档同一个理由：设备名、插件名这类**画在定宽格子里的动态文本**要按这一档算字符
/// 预算，字号写两遍就等于两把尺子（这块 UI 不按矩形裁切，超出去的字照画）。
const BODY_PT: f32 = 13.5;
/// 最小的一档（`small` / `right` / `center_small`）：图上悬停读数吃的是它，
/// 而那颗气泡只有 150 DIP 宽 —— 预算与字号同样得是同一把尺子。
const SMALL_PT: f32 = 13.0;

fn right_format(dw: &IDWriteFactory, size: f32) -> windows::core::Result<IDWriteTextFormat> {
    right_format_w(dw, size, UI_WEIGHT)
}

fn right_format_w(
    dw: &IDWriteFactory,
    size: f32,
    weight: DWRITE_FONT_WEIGHT,
) -> windows::core::Result<IDWriteTextFormat> {
    let f = text_format(dw, size, weight)?;
    unsafe {
        let _ = f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_TRAILING);
    }
    Ok(f)
}

fn center_format(dw: &IDWriteFactory, size: f32) -> windows::core::Result<IDWriteTextFormat> {
    center_format_w(dw, size, UI_WEIGHT)
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
        duration_unreadable: snap.today_app_unreadable,
    });
}

/// 显示/隐藏悬浮窗 —— 托盘菜单与设置页那一行共用的**唯一**入口。
///
/// 写的是 `[floating] enabled`，所以"关掉"能跨重启记住；但**不要求重启**：
/// 开关这一下就把卡片显示/藏起来（与旧版同一个口径）。
///
/// ⚠ 「能跨重启记住」这句以前只是承诺：写盘走 `config::set`，它恒回 `Ok(())`（真写的是
/// 另一头的去抖线程），落没落成这里问不出来，而去抖线程死了以后**每一次**改动都不落盘。
/// 现在走 `set_now` 当场验一次，结局拼进交回的那句状态话（`config_write_note`）。
/// 交回给状态行那一句话，说的是"屏上真出现的状态"：显示那一步没成就不念"显示"。
fn toggle_floating(main: HWND, want: Option<bool>) -> String {
    let want = want.unwrap_or(!floating::is_visible());
    let cfg = focusflow_core::config::instance();
    let note = config_write_note(
        "悬浮窗开关",
        cfg.set_now("floating", "enabled", if want { "true" } else { "false" }),
    );
    if want && !floating::is_alive() {
        // 线程第一次就没起来（或已经关掉）时，再点一次要真的能起回来。
        if let Err(e) = floating::spawn(main) {
            let msg = format!("悬浮窗起不来：{e}{note}");
            tracing_note(&msg);
            return msg;
        }
    }
    let _ = floating::set_visible(want);
    let msg = format!(
        "悬浮窗：{}{note}",
        if floating::is_visible() {
            "显示"
        } else {
            "隐藏"
        }
    );
    tracing_note(&msg);
    msg
}

/// 深色 / 浅色切换（设置页那一行）。写 `[gui] theme`，当下重画，悬浮窗也立刻换色。
///
/// 与 [`toggle_floating`] 同一口径：当场落盘、把结局拼进交回的那句状态话（这一项重启后
/// 才生效的项另说，屏上念的是本次运行里的样子）。
fn toggle_theme(hwnd: HWND) -> String {
    let next = if theme_name().trim() == "dark" {
        "light"
    } else {
        "dark"
    };
    let cfg = focusflow_core::config::instance();
    let msg = format!(
        "主题已切换：{next}{}",
        config_write_note("主题", cfg.set_now("gui", "theme", next))
    );
    // 标题栏是非客户区，不归 D2D 管：深浅得单独告诉 DWM 一次。
    apply_title_theme(hwnd);
    // 悬浮窗自己每秒重读配置，这里催一帧让它当场变色（与主界面同一刻）。
    floating::refresh();
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
    tracing_note(&msg);
    msg
}

/// 退出：托盘菜单与验收消息共用这一条。顺序有讲究 —— 先收卡片，再停采集并落库
/// （`collect::shutdown` 里是 flush → stop writer → backup_on_exit），最后才销毁主窗口。
///
/// 收尾还要显式 `logger::shutdown()`：日志走的是 non_blocking 通道，那个 `WorkerGuard`
/// 躺在 static 里不会自己析构，**退出前最后那几条（数据库已关闭、退出备份成没成）
/// 过去每次都留在缓冲区里没了**。CLI 那条路早就显式收（`crates/cli/src/main.rs` 里
/// 写着同样的理由），原生端当时漏了 —— 而排查崩溃与"退出没退干净"靠的正是日志的尾巴。
fn quit_app(hwnd: HWND) {
    // 插件存储是按 300ms 去抖落盘的：最后一次改动可能还停在内存里。退出前先把它们问一次，
    // 顺序不能反 —— 后面那句退出备份要把盘上的那份收进包里，内存里有而盘上没有，
    // 收进去的就是旧的那一份（用户看到的症状是"刚存的一条没了"）。
    for why in focusflow_core::plugins::plugin_store::flush_all() {
        tracing_note(&format!("退出前插件存储没落盘：{why}"));
    }
    // 台账那份同一个理由：常驻插件退出时不走卸载那条路，而 `health.json` 就是为了
    // "重启之后还查得到这一场跑了什么、坏在哪"。也要排在退出备份之前 —— `plugins/state/`
    // 会一起收进备份包，内存里有而盘上没有的，收进去的就是旧的那一份。
    for why in focusflow_core::plugins::plugin_health::flush_all() {
        tracing_note(&format!("退出前插件台账没落盘：{why}"));
    }
    // 最后补一次几何：拖完最后一下马上退出、最大化之后直接退出，都不经过 `WM_EXITSIZEMOVE`。
    save_main_geometry(hwnd);
    clipboard_popup::destroy();
    floating::shutdown();
    collect::shutdown(focusflow_core::config::instance());
    focusflow_core::logger::shutdown();
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

/// GUI 的诊断日志（没有控制台时的"留一份现场"）。
///
/// 它不是结构性日志：那份按天滚在 `logs/` 下、带时间戳与级别（`logger::init_logging`，
/// 保留最近 4 个文件）。这一份落在**进程当前目录**（快捷方式起始位置正常时就是程序目录），
/// 内容是心跳、点击、截图事件这类原始流水，给现场排查用。
///
/// **默认关**，开关是设置页「高级 → 诊断日志」（`[gui] trace_log`）：读点就是这里，
/// 改完下一行立刻生效。关着时一行都不写 —— 它是排查用的，不是产品功能。
///
/// 体积有上限：超过 `MAX_LOG_BYTES` 就把当前内容挪成 `focusflow.log.1`（覆盖上一份），
/// 再从空文件写起 —— 总量最多两倍。没有这道闸时它是纯追加的：心跳每 5 秒一行、插件攒批
/// 最多每 10 秒一行，一台常开的机器一天一二百 KB，几个月就是几 MB，而且没人会去删。
fn tracing_note(msg: &str) {
    use std::io::Write;
    if !focusflow_core::config::instance().get_bool("gui", "trace_log", false) {
        return;
    }
    /// 单文件上限：512 KiB 装得下约一天半的心跳与事件（心跳约 13 KB/小时）。
    const MAX_LOG_BYTES: u64 = 512 * 1024;
    // 多个线程都会写（UI / 截图 / 悬浮窗 / 采集）：轮转期间必须互斥，否则一个线程刚把
    // 文件改名，另一个还拿着旧句柄往"已经挪走"的那份里写 —— 新文件反而少了这一段。
    static WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = std::path::Path::new("focusflow.log");
    rotate_log_if_oversized(path, std::path::Path::new("focusflow.log.1"), MAX_LOG_BYTES);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{msg}");
    }
}

/// 超过上限就把 `path` 挪成 `rotated`（覆盖旧的）；挪不动就原地截断。
/// 两种结局都保证下一行从空文件写起，不再无限长。返回是否真的轮转过（测试用）。
fn rotate_log_if_oversized(
    path: &std::path::Path,
    rotated: &std::path::Path,
    max_bytes: u64,
) -> bool {
    let oversized = std::fs::metadata(path)
        .map(|m| m.len() >= max_bytes)
        .unwrap_or(false);
    if !oversized {
        return false;
    }
    match std::fs::remove_file(rotated) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {
            // 旧备份删不掉（被占用/是个目录）：这次改不了名，先原地截断。
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(path);
            return true;
        }
    }
    if std::fs::rename(path, rotated).is_err() {
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path);
    }
    true
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

/// 把一段文字放进剪贴板（CF_UNICODETEXT）。
///
/// 实现挪到 core 的 `clipboard::write_text`：那边与监听线程共用同一把"自写抑制"尺子
/// （序号在锁内、关闭之前记录），否则程序自己复制的内容会被剪贴板历史再收一遍。
/// 这里的函数名与调用形态保留给既有接线守卫。
fn copy_text_to_clipboard(_hwnd: HWND, text: &str) -> std::result::Result<(), String> {
    focusflow_core::clipboard::write_text(text)
}

/// 行内 EDIT 的顶层宿主窗：画主题底 + 一圈描边，真正的 EDIT 是它的子窗口。
///
/// 主窗走 DirectComposition 合成，挂在主窗下面的子窗口会被那层合成内容盖住
/// （实测：原生按钮只透出一点、EDIT 整块看不见——字打进去了、光标也在，屏上一个像素都不变）。
/// 顶层弹出窗不受这层合成影响，所以输入框挂在它下面；宿主本身只负责那圈边框与底色。
unsafe extern "system" fn edit_host_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                let owner = GetParent(hwnd).unwrap_or_default();
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                if let Some(app) = app_ptr(owner) {
                    if !app.edit_bg.is_invalid() {
                        let _ = FillRect(hdc, &rc, app.edit_bg);
                    }
                    if !app.edit_border.is_invalid() {
                        let _ = FrameRect(hdc, &rc, app.edit_border);
                    }
                }
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_CTLCOLOREDIT => {
                // EDIT 的父子链是 主窗(属主) → 宿主 → EDIT，颜色问的是宿主；
                // 主题色在主窗那头。
                let owner = GetParent(hwnd).unwrap_or_default();
                if let Some(app) = app_ptr(owner) {
                    let hdc = HDC(w.0 as *mut core::ffi::c_void);
                    SetTextColor(hdc, app.edit_fg);
                    SetBkColor(hdc, app.edit_bg_color);
                    return LRESULT(app.edit_bg.0 as isize);
                }
                DefWindowProcW(hwnd, msg, w, l)
            }
            WM_COMMAND => {
                // EN_CHANGE = 0x0300。EDIT 的 id 是 0（建的时候没给），所以只认通知码。
                // 只转"字变了"这一种：EN_SETFOCUS/EN_UPDATE 每敲一下会来好几条。
                let code = ((w.0 >> 16) & 0xFFFF) as u32;
                if code == 0x0300 {
                    let owner = GetParent(hwnd).unwrap_or_default();
                    if !owner.0.is_null() {
                        let child = GetWindow(hwnd, GW_CHILD).unwrap_or_default();
                        let _ = PostMessageW(
                            Some(owner),
                            WM_EDIT_CHANGED,
                            WPARAM(child.0 as usize),
                            LPARAM(0),
                        );
                    }
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }
}

/// 建一对「宿主 → EDIT」：宿主是顶层弹出窗，EDIT 缩在里面 2 px。
/// 行内编辑与搜索框各建一份（它们的子类 id 不同，焦点回执分得清）。
fn create_hosted_edit(parent: HWND, font: HFONT, subclass_id: usize) -> (HWND, HWND) {
    let hinst = unsafe { GetModuleHandleW(None) }
        .map(|m| m.into())
        .unwrap_or_default();
    unsafe {
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(edit_host_proc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_IBEAM).unwrap_or_default(),
            lpszClassName: EDIT_HOST_CLASS,
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);
    }
    let host = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            EDIT_HOST_CLASS,
            w!(""),
            WINDOW_STYLE(WS_POPUP.0 | WS_CLIPCHILDREN.0),
            0,
            0,
            100,
            24,
            Some(parent),
            None,
            None,
            None,
        )
    }
    .unwrap_or_default();
    let edit = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("EDIT"),
            w!(""),
            WINDOW_STYLE(WS_CHILD.0 | 0x0080), // WS_CHILD | ES_AUTOHSCROLL
            2,
            2,
            96,
            20,
            Some(host),
            None,
            None,
            None,
        )
    }
    .unwrap_or_default();
    unsafe {
        SendMessageW(
            edit,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
        let _ = SetWindowSubclass(edit, Some(edit_proc), subclass_id, 0);
    }
    (host, edit)
}

// ===== 日期选择器（插件模态里 kind = "date"） =====
//
// 记账本的「日期」原先是一个自由文本输入框：要手打 `2026-8-6`，格式错了插件还得回一句
// "这一条没生效"。这里给一个真日历：点开就是一格一格的月份，选哪天就回写哪天。
// 与行内 EDIT 一样，它必须是**顶层弹出窗**（主窗是 DComp 合成，子窗口会被盖住）。

const DATE_PICK_CLASS: PCWSTR = w!("FocusFlowDatePick");
const DP_W: f32 = 280.0;
const DP_PAD: f32 = 8.0;
const DP_HEAD_H: f32 = 34.0;
const DP_WEEK_H: f32 = 22.0;
const DP_ROW_H: f32 = 28.0;
const DP_FOOT_H: f32 = 28.0;
const DP_COLS: usize = 7;
const DP_ROWS: usize = 6;
const DP_WEEK_LABELS: [&str; 7] = ["一", "二", "三", "四", "五", "六", "日"];

fn dp_size() -> (f32, f32) {
    (
        DP_W,
        DP_HEAD_H + DP_WEEK_H + DP_ROW_H * DP_ROWS as f32 + DP_FOOT_H,
    )
}

fn dp_cell_w() -> f32 {
    (DP_W - DP_PAD * 2.0) / DP_COLS as f32
}

fn dp_cell_rect(i: usize, s: f32) -> D2D_RECT_F {
    let col = (i % DP_COLS) as f32;
    let row = (i / DP_COLS) as f32;
    rect(
        (DP_PAD + col * dp_cell_w()) * s,
        (DP_HEAD_H + DP_WEEK_H + row * DP_ROW_H) * s,
        dp_cell_w() * s,
        DP_ROW_H * s,
    )
}

fn dp_prev_rect(s: f32) -> D2D_RECT_F {
    rect(DP_PAD * s, 7.0 * s, 24.0 * s, 20.0 * s)
}

fn dp_next_rect(s: f32) -> D2D_RECT_F {
    rect((DP_W - DP_PAD - 24.0) * s, 7.0 * s, 24.0 * s, 20.0 * s)
}

fn dp_today_rect(s: f32) -> D2D_RECT_F {
    rect(
        DP_PAD * s,
        (DP_HEAD_H + DP_WEEK_H + DP_ROW_H * DP_ROWS as f32 + 3.0) * s,
        52.0 * s,
        20.0 * s,
    )
}

/// 一年 12 个月的加减（`delta` 按月算，跨年自动进位/借位）。
///
/// 年份来自插件交回 / 用户手输的字段值，可以写到 `i32::MAX` 这种量级：整年乘 12 在
/// `i32` 上会溢出 —— debug 是 panic，release 是 `panic = "abort"`，翻一下月整个程序
/// 就没了。用 i64 算完再夹回 `1..=9999`：这一档之外的年份日历画得出来也没有意义。
fn dp_month_add(year: i32, month: u32, delta: i32) -> (i32, u32) {
    let total = i64::from(year) * 12 + i64::from(month) - 1 + i64::from(delta);
    let y = total.div_euclid(12).clamp(1, 9999) as i32;
    let m = (total.rem_euclid(12) + 1) as u32;
    (y, m)
}

fn dp_days_in_month(year: i32, month: u32) -> u32 {
    let (ny, nm) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    match (
        chrono::NaiveDate::from_ymd_opt(year, month, 1),
        chrono::NaiveDate::from_ymd_opt(ny, nm, 1),
    ) {
        (Some(a), Some(b)) => (b - a).num_days() as u32,
        _ => 30,
    }
}

fn dp_first_weekday(year: i32, month: u32) -> usize {
    use chrono::Datelike;
    chrono::NaiveDate::from_ymd_opt(year, month, 1)
        .map(|d| d.weekday().num_days_from_monday() as usize)
        .unwrap_or(0)
}

fn dp_today() -> (i32, u32, u32) {
    use chrono::Datelike;
    let d = chrono::Local::now().date_naive();
    (d.year(), d.month(), d.day())
}

/// 从字段当前值猜出要看哪个月：认 `YYYY-M-D`（补不补零都行），认不出来就落在今天。
fn dp_parse(value: &str) -> (i32, u32, Option<(i32, u32, u32)>) {
    let mut it = value.trim().split('-');
    let year = it.next().and_then(|s| s.trim().parse::<i32>().ok());
    let month = it.next().and_then(|s| s.trim().parse::<u32>().ok());
    let day = it.next().and_then(|s| s.trim().parse::<u32>().ok());
    if let (Some(year), Some(month)) = (year, month) {
        // 年份也来自字段值：`999999999-01-01` 这种写法要退回今天，而不是把日历
        // 开在一个翻月会溢出的年份上（见 `dp_month_add` 那段）。
        if (1..=9999).contains(&year) && (1..=12).contains(&month) {
            let selected = day
                .filter(|d| (1..=dp_days_in_month(year, month)).contains(d))
                .map(|d| (year, month, d));
            return (year, month, selected);
        }
    }
    let (y, m, _) = dp_today();
    (y, m, None)
}

struct DatePickState {
    /// 主窗句柄（宿主属主）。存 isize 是为了让这份状态能放进 `static Mutex`。
    owner: isize,
    plugin: String,
    field: String,
    year: i32,
    month: u32,
    selected: Option<(i32, u32, u32)>,
    scale: f32,
}

static DATE_PICK: Mutex<Option<DatePickState>> = Mutex::new(None);

fn dp_close(hwnd: HWND) {
    if let Ok(mut g) = DATE_PICK.lock() {
        *g = None;
    }
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}

fn dp_text(hdc: HDC, font: HFONT, text: &str, r: D2D_RECT_F, color: COLORREF) {
    let wide = HSTRING::from(text);
    let mut sz = SIZE::default();
    unsafe {
        SelectObject(hdc, HGDIOBJ(font.0));
        SetTextColor(hdc, color);
        SetBkMode(hdc, TRANSPARENT);
        let _ = GetTextExtentPoint32W(hdc, &wide, &mut sz);
        let x = r.left + ((r.right - r.left) - sz.cx as f32) / 2.0;
        let y = r.top + ((r.bottom - r.top) - sz.cy as f32) / 2.0;
        let _ = TextOutW(hdc, x.round() as i32, y.round() as i32, &wide);
    }
}

unsafe extern "system" fn date_pick_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                let st = DATE_PICK.lock().ok().and_then(|g| {
                    g.as_ref()
                        .map(|s| (s.owner, s.year, s.month, s.selected, s.scale))
                });
                if let Some((owner, year, month, selected, scale)) = st {
                    let owner_hwnd = HWND(owner as *mut core::ffi::c_void);
                    if let Some(app) = app_ptr(owner_hwnd) {
                        let mut rc = RECT::default();
                        let _ = GetClientRect(hwnd, &mut rc);
                        let bg = app.edit_bg;
                        let border = app.edit_border;
                        let fg = app.edit_fg;
                        let white = COLORREF(0x00FF_FFFF);
                        let muted = COLORREF(0x0080_8080);
                        if !bg.is_invalid() {
                            let _ = FillRect(hdc, &rc, bg);
                        }
                        if !border.is_invalid() {
                            let _ = FrameRect(hdc, &rc, border);
                        }
                        dp_text(hdc, app.ui_font, "<", dp_prev_rect(scale), fg);
                        dp_text(hdc, app.ui_font, ">", dp_next_rect(scale), fg);
                        dp_text(
                            hdc,
                            app.ui_font,
                            &format!("{year} 年 {month} 月"),
                            rect(
                                (DP_PAD + 30.0) * scale,
                                7.0 * scale,
                                (DP_W - 2.0 * (DP_PAD + 30.0)) * scale,
                                20.0 * scale,
                            ),
                            fg,
                        );
                        for (i, label) in DP_WEEK_LABELS.iter().enumerate() {
                            dp_text(
                                hdc,
                                app.ui_font,
                                label,
                                rect(
                                    (DP_PAD + i as f32 * dp_cell_w()) * scale,
                                    (DP_HEAD_H + 2.0) * scale,
                                    dp_cell_w() * scale,
                                    18.0 * scale,
                                ),
                                muted,
                            );
                        }
                        let first = dp_first_weekday(year, month);
                        let days = dp_days_in_month(year, month) as i32;
                        let (ty, tm, td) = dp_today();
                        for i in 0..DP_ROWS * DP_COLS {
                            let day = i as i32 - first as i32 + 1;
                            let cell = dp_cell_rect(i, scale);
                            if day >= 1 && day <= days {
                                let picked = selected == Some((year, month, day as u32));
                                if picked && !border.is_invalid() {
                                    let _ = FillRect(hdc, &cell_inset(cell, 2.0 * scale), border);
                                } else if (year, month, day as u32) == (ty, tm, td)
                                    && !border.is_invalid()
                                {
                                    let _ = FrameRect(hdc, &cell_inset(cell, 2.0 * scale), border);
                                }
                                dp_text(
                                    hdc,
                                    app.ui_font,
                                    &day.to_string(),
                                    cell,
                                    if picked { white } else { fg },
                                );
                            }
                        }
                        dp_text(hdc, app.ui_font, "今天", dp_today_rect(scale), fg);
                    }
                }
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let x = (l.0 & 0xFFFF) as u16 as i16 as f32;
                let y = ((l.0 >> 16) & 0xFFFF) as u16 as i16 as f32;
                let st = DATE_PICK.lock().ok().and_then(|g| {
                    g.as_ref().map(|s| {
                        (
                            s.owner,
                            s.plugin.clone(),
                            s.field.clone(),
                            s.year,
                            s.month,
                            s.scale,
                        )
                    })
                });
                let Some((owner, plugin, field, year, month, scale)) = st else {
                    return LRESULT(0);
                };
                if hit(dp_prev_rect(scale), x, y) {
                    let (ny, nm) = dp_month_add(year, month, -1);
                    if let Ok(mut g) = DATE_PICK.lock() {
                        if let Some(s) = g.as_mut() {
                            s.year = ny;
                            s.month = nm;
                        }
                    }
                    let _ = InvalidateRect(Some(hwnd), None, true);
                    return LRESULT(0);
                }
                if hit(dp_next_rect(scale), x, y) {
                    let (ny, nm) = dp_month_add(year, month, 1);
                    if let Ok(mut g) = DATE_PICK.lock() {
                        if let Some(s) = g.as_mut() {
                            s.year = ny;
                            s.month = nm;
                        }
                    }
                    let _ = InvalidateRect(Some(hwnd), None, true);
                    return LRESULT(0);
                }
                if hit(dp_today_rect(scale), x, y) {
                    let (ty, tm, td) = dp_today();
                    let date = format!("{ty:04}-{tm:02}-{td:02}");
                    let owner_hwnd = HWND(owner as *mut core::ffi::c_void);
                    if let Some(app) = app_ptr(owner_hwnd) {
                        app.set_plugin_field(owner_hwnd, &plugin, &field, &date);
                    }
                    dp_close(hwnd);
                    let _ = SetForegroundWindow(owner_hwnd);
                    return LRESULT(0);
                }
                let first = dp_first_weekday(year, month);
                let days = dp_days_in_month(year, month) as i32;
                for i in 0..DP_ROWS * DP_COLS {
                    if !hit(dp_cell_rect(i, scale), x, y) {
                        continue;
                    }
                    let day = i as i32 - first as i32 + 1;
                    if day >= 1 && day <= days {
                        let date = format!("{year:04}-{month:02}-{day:02}");
                        let owner_hwnd = HWND(owner as *mut core::ffi::c_void);
                        if let Some(app) = app_ptr(owner_hwnd) {
                            app.set_plugin_field(owner_hwnd, &plugin, &field, &date);
                        }
                        dp_close(hwnd);
                        let _ = SetForegroundWindow(owner_hwnd);
                    }
                    return LRESULT(0);
                }
                LRESULT(0)
            }
            WM_KEYDOWN if w.0 == 0x1B => {
                dp_close(hwnd);
                LRESULT(0)
            }
            WM_ACTIVATE if (w.0 & 0xFFFF) == 0 => {
                dp_close(hwnd);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }
}

/// 把矩形四边各缩 `inset`（像素）：日历格子里那圈高亮/描边不贴到邻居格上。
fn cell_inset(r: D2D_RECT_F, inset: f32) -> RECT {
    RECT {
        left: (r.left + inset) as i32,
        top: (r.top + inset) as i32,
        right: (r.right - inset) as i32,
        bottom: (r.bottom - inset) as i32,
    }
}

/// EDIT 的子类过程：Enter 提交、Esc 取消、失焦提交。
///
/// 为什么用子类化而不是在父窗口里接 `WM_COMMAND`：单个 EDIT 的 Enter 不会变成
/// `EN_*` 通知，Esc 更不会；这两个键只能靠自己截。
///
/// 回执投给**根属主**（主窗），不是 `GetParent`：行内 EDIT 挂在顶层宿主窗下
/// （`edit_host_proc`），`GetParent` 拿到的是宿主，主窗永远收不到提交。
/// 读宿主 EDIT 里的文字。用 `WM_GETTEXT` 而不是 `GetWindowTextW`：这一格是宿主窗下的
/// 子 EDIT，跨窗口取文本走消息更直白。实时筛与按 Enter 提交共用这一份。
fn read_hosted_edit_text(edit: HWND) -> String {
    // 长度现问：固定 256 会把更长的筛选词**静默截掉** —— 框里看着是全的，列表却按
    // 前半截筛（粘贴一条长路径时最明显）。
    let len = unsafe { GetWindowTextLengthW(edit) }.max(0) as usize;
    let mut buf = vec![0u16; len + 1];
    let n = unsafe {
        SendMessageW(
            edit,
            0x000D,
            Some(WPARAM(buf.len())),
            Some(LPARAM(buf.as_mut_ptr() as isize)),
        )
        .0 as usize
    }
    .min(len);
    String::from_utf16_lossy(&buf[..n])
}

unsafe extern "system" fn edit_proc(
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
    id: usize,
    _data: usize,
) -> LRESULT {
    const VK_RETURN: usize = 0x0D;
    const VK_ESCAPE: usize = 0x1B;
    unsafe {
        // 剪贴板页搜索框：导航键转给主窗（单行 EDIT 会把它们吞掉，页脚的
        //「Tab 切模式 / ↑↓ 选择」就成了空话）。
        if clip_search_key_goes_to_page(id, msg, w.0) {
            let owner = GetAncestor(hwnd, GA_ROOTOWNER);
            if !owner.0.is_null() {
                let _ = PostMessageW(Some(owner), WM_KEYDOWN, w, l);
            }
            return LRESULT(0);
        }
        match msg {
            WM_KEYDOWN if w.0 == VK_RETURN || w.0 == VK_ESCAPE => {
                let code = if w.0 == VK_RETURN { 1 } else { 2 };
                let owner = GetAncestor(hwnd, GA_ROOTOWNER);
                if !owner.0.is_null() {
                    let _ = PostMessageW(Some(owner), WM_EDIT_DONE, WPARAM(code), LPARAM(0));
                }
                LRESULT(0)
            }
            WM_CHAR if w.0 == VK_RETURN || w.0 == VK_ESCAPE => LRESULT(0),
            // 焦点变化递回父窗：提示文案由父窗画，但只有控件没拿到焦点时才能画在那块矩形上
            // （见 `SEARCH_CUE` 与 `ensure_search_edit`）。哪个 EDIT 该递、哪条消息算"拿到"，
            // 全在 `search_focus_signal` 那一份判据里。
            WM_SETFOCUS | WM_KILLFOCUS => {
                if let Some(on) = search_focus_signal(id, msg) {
                    let owner = GetAncestor(hwnd, GA_ROOTOWNER);
                    if !owner.0.is_null() {
                        // 两页的搜索框各有一条回执：设置页与剪贴板页的提示文案分属两块矩形。
                        let out = if id == CLIP_SEARCH_EDIT_SUBCLASS {
                            WM_CLIP_SEARCH_FOCUS
                        } else {
                            WM_SEARCH_FOCUS
                        };
                        let _ = PostMessageW(Some(owner), out, WPARAM(on as usize), LPARAM(0));
                    }
                }
                DefSubclassProc(hwnd, msg, w, l)
            }
            // 不在这里做"失焦即提交"：窗口不是前台时 `SetFocus` 不会真正生效，
            // 而 EDIT 刚建出来就会收到一次假的 WM_KILLFOCUS，提交的会是预填的旧名字
            // （2026-10-26 实测）。提交由 Enter/Esc 和"点编辑框外面"两条路负责。
            _ => DefSubclassProc(hwnd, msg, w, l),
        }
    }
}

/// 客户区尺寸（DIP）。窗口边框那几条消息里没有 `App`，画图那边走 `App::client_dip`。
fn client_dip_of(hwnd: HWND) -> (f32, f32) {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    let dpi = unsafe { GetDpiForWindow(hwnd) } as f32;
    let s = if dpi <= 0.0 { 1.0 } else { dpi / 96.0 };
    (
        (rc.right - rc.left) as f32 / s,
        (rc.bottom - rc.top) as f32 / s,
    )
}

/// DPI → 缩放倍数。0（取不到）按 100% 算，别让一次失败的查询把窗口缩成一条缝。
fn dpi_scale_from(dpi: u32) -> f64 {
    if dpi == 0 {
        1.0
    } else {
        dpi as f64 / 96.0
    }
}

/// 窗口所在显示器当前的缩放倍数（PMv2：每块屏可以不同）。
fn dpi_scale_of(hwnd: HWND) -> f64 {
    dpi_scale_from(unsafe { GetDpiForWindow(hwnd) })
}

/// 存档（逻辑像素 + 存盘时那块屏的缩放）→ 物理像素矩形。
///
/// 缩放取自存档而不是"建完窗再问"：多屏缩放不同时，逻辑坐标可能先落在另一块屏上，
/// 那时问到的 DPI 是另一块的，窗口会还原到错误的屏、错误的尺寸。
fn physical_rect_of(st: &MainWindowState) -> (i32, i32, i32, i32) {
    let s = if st.scale.is_finite() && st.scale > 0.0 {
        st.scale
    } else {
        1.0
    };
    (
        (st.left * s).round() as i32,
        (st.top * s).round() as i32,
        (st.width * s).round() as i32,
        (st.height * s).round() as i32,
    )
}

/// 把窗口矩形夹进工作区：**先**让尺寸不超过工作区，**再**把左上角收进来。
///
/// 次序不能反：先夹坐标再夹尺寸的话，"存档比屏幕还宽"那种窗口会把左边顶到屏外。
/// 拆成纯函数是为了能测 —— 换分辨率、拔掉副屏、把窗口拖到只剩一条边这些情形，
/// 靠的全是这一条规则（窗口一旦整个落在屏幕外就再也拖不回来）。
fn clamp_main_rect(work: (i32, i32, i32, i32), rect: (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
    const MIN_W: i32 = 480;
    const MIN_H: i32 = 360;
    let (wl, wt, wr, wb) = work;
    let work_w = (wr - wl).max(1);
    let work_h = (wb - wt).max(1);
    let w = rect.2.clamp(MIN_W.min(work_w), work_w);
    let h = rect.3.clamp(MIN_H.min(work_h), work_h);
    let x = rect.0.clamp(wl, wr - w);
    let y = rect.1.clamp(wt, wb - h);
    (x, y, w, h)
}

/// 主窗口上一次的矩形与最大化状态，按逻辑像素写回 `window_state.ini`。
///
/// 连"量这组坐标时那块屏的缩放"一起存（与悬浮窗 `[floating] pos_x/pos_y` 同为逻辑像素）：
/// 还原按存档里那个倍数换回物理像素，不依赖"建完窗问到的 DPI" —— 多屏缩放不同时那一问
/// 会问到错误的屏。
///
/// 最小化时不写：那一拍的 `GetWindowRect` 是 (-32000, -32000) 那一套，存下去下次启动
/// 窗口就在屏幕外（而且没有入口能把它拖回来）。
fn save_main_geometry(hwnd: HWND) {
    unsafe {
        if IsIconic(hwnd).as_bool() {
            return;
        }
        let zoomed = IsZoomed(hwnd).as_bool();
        // 存档与"这一趟的最大化意图"在这一处对齐：拖到屏幕顶自动最大化那种收尾只走
        // `WM_EXITSIZEMOVE`，`WM_SIZE` 那一拍在拖拽循环里被跳过了，不在这记一笔，
        // 下一次还原就会拿着旧档位判断"没换档"而不写。
        MAIN_MAXIMIZED.store(zoomed, std::sync::atomic::Ordering::Relaxed);
        // 最大化时 `GetWindowRect` 给的是最大化矩形，正常位置沿用上一次存档里那份。
        let mut st = focusflow_core::window_state::load().unwrap_or_else(MainWindowState::defaults);
        if !zoomed {
            let mut rc = RECT::default();
            if GetWindowRect(hwnd, &mut rc).is_err() {
                tracing_note("主窗口几何读不出来，这次没有写回 window_state.ini");
                return;
            }
            let scale = dpi_scale_of(hwnd);
            st.left = (rc.left as f64 / scale).round();
            st.top = (rc.top as f64 / scale).round();
            st.width = ((rc.right - rc.left) as f64 / scale).round();
            st.height = ((rc.bottom - rc.top) as f64 / scale).round();
            // 这组逻辑坐标是在"这块屏这个缩放"下量的；还原时按同一个倍数换回物理像素。
            st.scale = scale;
        }
        st.maximized = zoomed;
        if let Err(e) = focusflow_core::window_state::save(&st) {
            // 写失败就是"下次启动跳回上一次的位置"——用户会以为是软件不稳。
            tracing_note(&format!(
                "主窗口位置没能写回 window_state.ini（{e}）：下次启动会回到上一次存下的位置"
            ));
        }
    }
}

/// 可缩放边框的厚度（DIP）：与系统同一个口径（`SM_CXSIZEFRAME + SM_CXPADDEDBORDER`），
/// 否则高 DPI 下手指按在边上却拖不动。
fn frame_thickness(hwnd: HWND) -> (f32, f32) {
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let s = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
    let fx = unsafe {
        GetSystemMetricsForDpi(SM_CXSIZEFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
    };
    let fy = unsafe {
        GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
    };
    (fx as f32 / s, fy as f32 / s)
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
        WM_NCCALCSIZE => {
            // wParam=0 是"问窗口矩形"，照系统默认；TRUE 才是"算客户区"。
            if w.0 == 0 {
                return unsafe { DefWindowProcW(hwnd, msg, w, l) };
            }
            // 客户区 = 整个窗口：原生标题条与可见边框一起让出来，顶栏自己当标题栏。
            // 最大化时窗口矩形比工作区大出一圈看不见的边框，不削掉就会把内容裁掉。
            if unsafe { IsZoomed(hwnd) }.as_bool() {
                let dpi = unsafe { GetDpiForWindow(hwnd) };
                let fx = unsafe {
                    GetSystemMetricsForDpi(SM_CXSIZEFRAME, dpi)
                        + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
                };
                let fy = unsafe {
                    GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi)
                        + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
                };
                let params = unsafe { &mut *(l.0 as *mut NCCALCSIZE_PARAMS) };
                params.rgrc[0].left += fx;
                params.rgrc[0].right -= fx;
                params.rgrc[0].top += fy;
                params.rgrc[0].bottom -= fy;
            }
            LRESULT(0)
        }
        WM_NCHITTEST => {
            let mut pt = POINT {
                x: (l.0 & 0xFFFF) as u16 as i16 as i32,
                y: ((l.0 >> 16) & 0xFFFF) as u16 as i16 as i32,
            };
            let _ = unsafe { ScreenToClient(hwnd, &mut pt) };
            let dpi = unsafe { GetDpiForWindow(hwnd) } as f32;
            let s = if dpi <= 0.0 { 1.0 } else { dpi / 96.0 };
            let (cw, ch) = client_dip_of(hwnd);
            let (x, y) = (pt.x as f32 / s, pt.y as f32 / s);
            let (bx, by) = frame_thickness(hwnd);
            let zoomed = unsafe { IsZoomed(hwnd) }.as_bool();
            LRESULT(nc_hit_code(zoomed, cw, ch, bx, by, x, y) as isize)
        }
        WM_NCACTIVATE => {
            // 没有非客户区要画；返回 TRUE 告诉系统"边框已更新"，顺手重画一次。
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(1)
        }
        WM_NCMOUSEMOVE => {
            // 从按钮（客户区）移到标题带（非客户区）不会再来 WM_MOUSEMOVE：
            // 悬停状态要在这里撤掉，否则按钮会一直亮着。
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                if app.ctl.take().is_some() {
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
            }
            unsafe { DefWindowProcW(hwnd, msg, w, l) }
        }
        WM_SIZE => {
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                // 拖拽循环里先不动：那会把用户正在进行的拖拽打断，等 `WM_EXITSIZEMOVE`。
                if !app.in_size_move {
                    let zoomed = unsafe { IsZoomed(hwnd) }.as_bool();
                    // 最大化/还原有两类来源不经过 `WM_EXITSIZEMOVE`：标题栏双击、标题栏那两颗
                    // 按钮，以及静默启动后第一次唤出。这一拍不记下来，下次启动就不知道上次
                    // 是最大化的。最小化那一拍跳过（见 `save_main_geometry`）。
                    if !unsafe { IsIconic(hwnd) }.as_bool()
                        && zoomed != MAIN_MAXIMIZED.load(std::sync::atomic::Ordering::Relaxed)
                    {
                        MAIN_MAXIMIZED.store(zoomed, std::sync::atomic::Ordering::Relaxed);
                        save_main_geometry(hwnd);
                    }
                    if app.mica && zoomed != app.was_maximized {
                        app.was_maximized = zoomed;
                        fix_backdrop(hwnd);
                    }
                    // 搜索框那块底框是按客户区宽度**右边**定位的（`search_box_rect`），
                    // 而叠在上面的原生 EDIT 从前只在点「设置」页签那一下摆位：窗口一改变
                    // 尺寸（拖大、最大化、换缩放）两者就分家 —— 屏上剩一个没有输入窗口的
                    // 底框，点下去没反应，占位文案留在原地跟每一拍重绘一起闪。
                    // 摆位不能挪到画图那条路上（在 WM_PAINT 里动窗位会重入）。
                    app.place_search_edit(hwnd);
                }
            }
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_ENTERSIZEMOVE => {
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                app.in_size_move = true;
                app.size_at_enter = client_dip_of(hwnd);
            }
            LRESULT(0)
        }
        WM_SIZING => {
            // 拖拽缩放开始：临时回不透明档（DWM 背板不跟尺寸，半透明页会露旧边界）。
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                if !app.resizing {
                    app.resizing = true;
                    tracing_note("拖拽缩放：临时切不透明档");
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
            }
            unsafe { DefWindowProcW(hwnd, msg, w, l) }
        }
        WM_EXITSIZEMOVE => {
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                app.in_size_move = false;
                let sized = app.resizing;
                if sized {
                    app.resizing = false;
                    tracing_note("拖拽结束：贴回玻璃并重算背板");
                }
                // 纯移动（没改大小）不重算背板：那会白闪一下。
                let now = client_dip_of(hwnd);
                let resized = sized || now != app.size_at_enter;
                if app.mica && resized {
                    fix_backdrop(hwnd);
                }
                if resized {
                    // 拖拽期间 WM_SIZE 那一条被 `in_size_move` 挡下了，摆位要在这一拍补：
                    // 不然用户拖着边框把窗口改宽，设置页那个搜索框的输入窗口还留在老位置，
                    // 底框与输入框分家之后的症状与「点搜索框没反应」是同一条。
                    app.place_search_edit(hwnd);
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
            }
            // 拖完（移动或缩放）就把矩形记下来：重启与退出都从这一份恢复。
            // 放在 `if let` 外面 —— 拿不到 `App` 时几何照样该记。
            save_main_geometry(hwnd);
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
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                if app.mica {
                    // 换屏同时换了尺寸：背板同样要按新矩形重算。
                    fix_backdrop(hwnd);
                }
            }
            LRESULT(0)
        }
        WM_GETMINMAXINFO => {
            // 先让系统填一遍（最大化尺寸/位置就在里面），再只补一条下限：
            // 窗口再窄下去，固定矩形版面会开始互相压字。
            let r = unsafe { DefWindowProcW(hwnd, msg, w, l) };
            let dpi = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0;
            let mmi = unsafe { &mut *(l.0 as *mut MINMAXINFO) };
            mmi.ptMinTrackSize.x = (960.0 * dpi) as i32;
            mmi.ptMinTrackSize.y = (640.0 * dpi) as i32;
            r
        }
        WM_TIMER if w.0 == TIMER_EVENTS => {
            // 心跳：只留证据，不刷屏（。
            static TICKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = TICKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n.is_multiple_of(50) {
                tracing_note(&format!("timer tick {n}"));
            }
            // 内置看板页的节奏（方案 3.2.3）：停在这一页、且窗口真的被看着时，每 30 秒
            // 重查一次。三条判据都住在 `dashboard::should_refresh`，那一条用例能测到
            // "最小化了还在问库"这一族浪费 —— 写在 UI 里就没人问得到。
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                let watching = matches!(watching_now(hwnd), Readers::Panel);
                if dashboard::should_refresh(app.dash.elapsed_secs(), app.view == 6, watching) {
                    app.reload_board();
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                // 同一个悬停目标停满一小会儿才出 tooltip：扫过去的鼠标不该每次弹框。
                if !app.tooltip_ready
                    && app.ctl.is_some()
                    && app.hover_since.elapsed() >= std::time::Duration::from_millis(600)
                {
                    app.tooltip_ready = true;
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
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
                        if collect::is_listening() { "on" } else { "off" },
                        if collect::is_paused() {
                            "true"
                        } else {
                            "false"
                        }
                    ));
                }
            }
            // 攒批的按键增量交给插件：写线程只出件，进 Lua 留在 GUI 线程这一侧（`Lua` 不是 Send）。
            // 件口多数时候是空的 —— 一拍一次互斥体取空向量，不换回每键一次 VM 的代价。
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                app.pump_plugin_key_batches();
                // 到期的定时器与事件也在这一拍跑：core 那侧的排程表是进程级的，
                // 少了这一行就是"排得进去、永远没人跑"，而插件页什么都看不出来。
                app.pump_plugin_timers();
                // 热重载的通道也在这一拍取空：检测线程只发请求，进 Lua 的那一跳必须在 GUI 线程。
                // 少了这两行，那条线程整晚在扫目录而没人处理它的话，屏上照旧是"改了要重启"。
                let on_plugin_page = app.view == 4;
                app.pump_plugin_reloads(on_plugin_page);
                // 剪贴板监听线程交来的复制件在这一拍入附属库（写库留在 UI 线程）。
                app.pump_clipboard(hwnd);
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
                        let msg = toggle_floating(hwnd, None);
                        // 托盘那一下点在主窗口之外：状态行要落在主窗口上，下次开窗才看得见。
                        if let Some(app) = unsafe { app_ptr(hwnd) } {
                            app.status = Some(msg);
                        }
                        unsafe {
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                    }
                    "clipboard_history" => {
                        tracing_note("托盘：打开剪贴板历史");
                        clipboard_popup::toggle(hwnd);
                    }
                    "clipboard_pause" => {
                        if let Some(app) = unsafe { app_ptr(hwnd) } {
                            app.toggle_clipboard(hwnd);
                        }
                    }
                    "clipboard_pause_15" => {
                        pause_clipboard(hwnd, 15);
                    }
                    "clipboard_pause_60" => {
                        pause_clipboard(hwnd, 60);
                    }
                    "clipboard_resume" => {
                        resume_clipboard(hwnd);
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
                // 一次按键会来两条事件（按下 + 松开）。只认按下：两条都处理的话热键会
                // 触发两次 —— 弹窗开了又关、唤出窗开一下又藏，屏上看起来像"没反应"。
                if !hotkey_is_press(ev.state) {
                    continue;
                }
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
                    } else if app.clipboard_hk.map(|h| h.id() == ev.id).unwrap_or(false) {
                        Some("clipboard")
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
                    Some("clipboard") => {
                        tracing_note("收到剪贴板历史热键：唤出弹窗");
                        clipboard_popup::toggle(hwnd);
                    }
                    Some("toggle") => {
                        tracing_note("收到热键事件，唤出窗口");
                        unsafe { bring_to_front(hwnd) };
                    }
                    _ => tracing_note(&format!("收到没注册的热键事件 id={}", ev.id)),
                }
            }
            // 插件视图按**这一页自己声明的节奏**重算（方案 3.9：`PLUGIN_REFRESH_MS`，缺省 0 = 只在
            // 动作之后刷）。番茄钟的倒计时是插件在 `get_view()` 里算出来的，不重算就停在那儿；
            // 而"打开面板就每秒全量重算"是把记账本那 17 ms 的坑每秒往主线程挖一次。
            // 判断（含慢熔断的降频）全在 core 的 `Cadence`，这一头只管跑与记。
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                if let Some(name) = app.plugin_open.clone() {
                    let due = app
                        .pm
                        .as_mut()
                        .map(|pm| pm.view_due(&name, std::time::Instant::now()))
                        .unwrap_or(false);
                    if due {
                        if let Some(pm) = app.pm.as_mut() {
                            let started = std::time::Instant::now();
                            let _ = pm.refresh_view(&name);
                            // 耗时要在跑完这一侧交回：那是慢熔断唯一的样本来源
                            pm.note_view_refreshed(
                                &name,
                                std::time::Instant::now(),
                                started.elapsed(),
                            );
                        }
                        unsafe {
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                    }
                }
            }
            // 关掉或换页的那一转变：把**上一**那一页按 `panel` 档位卸掉（方案 3.14）。
            // 只卸上一页，是为了切换面板时不会把刚打开的那一页一起关掉；
            // `always` 的那一档由 core 判，这里不参与。
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                let now_open = app.plugin_open.clone();
                if let Some(was) = app.plugin_panel_was.clone() {
                    if Some(was.clone()) != now_open {
                        if let Some(pm) = app.pm.as_mut() {
                            if pm.release_panel_of(&was) {
                                tracing_note(&format!("面板关掉，卸掉只服务那一页的插件：{was}"));
                                unsafe {
                                    let _ = InvalidateRect(Some(hwnd), None, false);
                                }
                            }
                        }
                    }
                }
                app.plugin_panel_was = now_open;
            }
            // 「当前速度」跟这 100ms 一拍走：数值变了才重画（否则等于 10Hz 全窗重绘）。
            let cpm_now = if collect::is_listening() && !collect::is_paused() {
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
            let mut dragging = false;
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                if app.scroll_drag.is_some() {
                    dragging = app.drag_scroll(hwnd, x, y);
                }
                if !dragging {
                    changed = app.on_move(hwnd, x, y);
                }
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
                app.tooltip_ready = false;
                app.tooltip_text = None;
                if app.hover.take().is_some()
                    | app.ctl.take().is_some()
                    | app.pv_hover.take().is_some()
                {
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
            }
            LRESULT(0)
        }
        WM_SETCURSOR => {
            // 标题栏/边框交给系统（缩放、移动那几种光标不能抢）；只接管客户区。
            if ((l.0 & 0xFFFF) as u32) != HTCLIENT {
                return unsafe { DefWindowProcW(hwnd, msg, w, l) };
            }
            let mut hand = false;
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                let mut pt = POINT::default();
                if unsafe { GetCursorPos(&mut pt) }.is_ok() {
                    let _ = unsafe { ScreenToClient(hwnd, &mut pt) };
                    let s = app.scale(hwnd);
                    let (cw, ch) = app.client_dip(hwnd);
                    hand = app
                        .control_at(cw, ch, pt.x as f32 / s, pt.y as f32 / s)
                        // 窗口按钮保持箭头：系统标题栏上那三颗也不是手型。
                        .is_some_and(|c| !matches!(c, Ctl::WinBtn(_)));
                }
            }
            let cur = if hand { IDC_HAND } else { IDC_ARROW };
            let _ = unsafe { SetCursor(Some(LoadCursorW(None, cur).unwrap_or_default())) };
            LRESULT(1)
        }
        WM_LBUTTONDOWN => {
            let x = (l.0 & 0xFFFF) as u16 as i16 as f32;
            let y = ((l.0 >> 16) & 0xFFFF) as u16 as i16 as f32;
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                // 落在滑块上就是拖滚动条：这一下不该同时当成一次行点击。
                if !app.begin_scroll_drag(hwnd, x, y) {
                    app.on_click(hwnd, x, y);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                if app.end_scroll_drag() {
                    let _ = unsafe { ReleaseCapture() };
                }
            }
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            // 捕获被别人拿走（切窗、弹系统框）：拖动到此为止，别拿旧轨道继续换算。
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                app.end_scroll_drag();
            }
            LRESULT(0)
        }
        WM_RBUTTONDOWN => {
            // 右键压在"被截断的那一格"上 = 复制它的全文（提示框里看到什么就复制什么）。
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                if let Some(text) = app.tooltip_text.clone() {
                    match copy_text_to_clipboard(hwnd, &text) {
                        Ok(()) => app.status = Some(format!("已复制：{text}")),
                        Err(e) => app.status = Some(format!("复制失败：{e}")),
                    }
                    tracing_note(app.status.as_deref().unwrap_or(""));
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
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
        WM_SEARCH_FOCUS => {
            // 焦点进/出搜索框：重画一帧，让提示文案跟着让位或回来。
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                if app.search_focused != (w.0 != 0) {
                    app.search_focused = w.0 != 0;
                    if app.view == 5 {
                        if app.search_focused {
                            // 输入层是顶层弹窗：拿到焦点才叫出来（平时藏着，
                            // 让主窗画的提示文案留在框里）。
                            app.place_search_edit(hwnd);
                        } else {
                            app.hide_search_edit();
                        }
                    }
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        WM_CLIP_SEARCH_FOCUS => {
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                if app.clip_search_focused != (w.0 != 0) {
                    app.clip_search_focused = w.0 != 0;
                    if app.view == 7 {
                        if app.clip_search_focused {
                            app.place_clip_search_edit(hwnd);
                        } else {
                            app.hide_clip_search_edit();
                        }
                    }
                    let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
                }
            }
            LRESULT(0)
        }
        WM_EDIT_DONE => {
            tracing_note(&format!("收到 WM_EDIT_DONE code={}", w.0));
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                // 剪贴板页的搜索框与行内改名共用同一条回执：看焦点的那个 EDIT 是谁。
                let clip_search = app.view == 7
                    && app
                        .clip_search_edit
                        .is_some_and(|e| unsafe { GetFocus() } == e);
                if clip_search {
                    app.commit_clip_search(hwnd, w.0 == 1);
                } else {
                    app.finish_edit(hwnd, w.0 == 1);
                }
            }
            LRESULT(0)
        }
        WM_EDIT_CHANGED => {
            // 宿主窗只转"哪个 EDIT 的字变了"；是不是剪贴板搜索框由主窗认。
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                let edit = HWND(w.0 as *mut core::ffi::c_void);
                app.live_clip_search(hwnd, edit);
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
                if app.view == 4 {
                    // 安装/卸载也是走这条回执的：插件目录变了，这一页要当场重扫。
                    app.reload_plugins();
                }
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
                // 剪贴板管理页自己有列表：滚轮先给它，别被下面那条"非排行/设置页"的闸吞掉。
                if app.view == 7 {
                    let (cw, ch) = app.client_dip(hwnd);
                    let rows = clipboard_page::layout_for(cw, ch).rows;
                    app.clip_page
                        .scroll_by(if delta > 0 { 1 } else { -1 }, rows);
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    return LRESULT(0);
                }
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
                    // 详情页没有可滚的内容：吞掉滚轮，别让它滚到身后那张清单上。
                    if app.plugin_detail_open.is_some() {
                        return LRESULT(0);
                    }
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
                            let content = plugin_content_height(&view, plugin_scroll_area(cw, ch));
                            let step = if delta > 0 { -60 } else { 60 };
                            let next = clamp_plugin_scroll(
                                app.pv_scroll + step,
                                content,
                                plugin_scroll_visible(ch),
                            );
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
                        4 => plugin_slots(&app.plugin_rows).len(),
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
            // Ctrl+F：切到设置页并聚焦搜索框（任何一个页面都能按）。
            let ctrl = (unsafe { GetKeyState(0x11) } as u16 & 0x8000) != 0;
            if ctrl && w.0 == 0x46 {
                if let Some(app) = unsafe { app_ptr(hwnd) } {
                    app.focus_search(hwnd);
                }
                return LRESULT(0);
            }
            // Tab / 上下 / 左右 / Enter：焦点在页面里走，弹层开着时不接。
            let nav = unsafe {
                match app_ptr(hwnd) {
                    Some(app) => app.handle_nav_key(hwnd, w.0),
                    None => false,
                }
            };
            if nav {
                return LRESULT(0);
            }
            // Esc：先关弹层；再撤键盘焦点；都没有才收进托盘（与现有版本同一口径）。
            if w.0 == 0x1B {
                if let Some(app) = unsafe { app_ptr(hwnd) } {
                    if app.detail.is_some() {
                        app.close_detail(hwnd);
                    } else if app.cleanup_confirm {
                        // 确认框开着时 Esc = 取消（那一颗是默认结局）。
                        app.cleanup_confirm = false;
                        tracing_note("cleanup-cancel=1");
                        unsafe {
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                    } else if app.plugin_uninstall.is_some() {
                        // 确认框开着时 Esc = 取消（不删任何东西）。
                        app.plugin_uninstall = None;
                        unsafe {
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                    } else if app.pv_select.take().is_some() {
                        unsafe {
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                    } else if app.plugin_detail_open.is_some() {
                        // 与「返回」那颗钮同一个结局：下一页是清单，不是托盘。
                        app.plugin_detail_open = None;
                        app.plugin_detail_source = None;
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
                            // 与「返回」那颗钮走同一份清理：`plugin_sel` 里躺着的是这一场
                            // 点过的记录 id，只清 pv_scroll/pv_select/pv_modal 会把选中集留下 ——
                            // 下一次开面板时它读回自己那一组，高亮的是用户这次没点过的行，
                            // 而一颗 `sel` 按钮会把那串陈旧 id 直接拼进动作 id 交给插件。
                            app.plugin_open = None;
                            app.reset_plugin_view_state();
                            unsafe {
                                let _ = InvalidateRect(Some(hwnd), None, false);
                            }
                        }
                    } else if app.kbd_focus.take().is_some() {
                        // 有键盘焦点时先撤焦点（弹层都关完了才轮到这一档），不直接藏进托盘。
                        app.tooltip_ready = false;
                        app.tooltip_text = None;
                        unsafe {
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                    } else if app.edit_target.is_none() {
                        hide_to_tray(app, hwnd);
                    }
                }
                return LRESULT(0);
            }
            unsafe { DefWindowProcW(hwnd, msg, w, l) }
        }
        WM_CLOSE => {
            // 关窗口 = 收进托盘（真正退出走托盘菜单），与旧版同一口径。
            // 收起来顺手放掉渲染目标：见 `hide_to_tray` 那一段现量（开过一次面板就全天
            // 多占 16.2 MB 私有提交 + 三十几条图形栈线程）。
            if let Some(app) = unsafe { app_ptr(hwnd) } {
                hide_to_tray(app, hwnd);
            } else {
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
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
    extend_frame(hwnd);
    tracing_note("Mica 已启用");
    true
}

/// 把 DWM 的玻璃区域铺满整个窗口。拆掉原生标题条之后这件事要**在尺寸变化后重做**：
/// 背板只按上一次调用时的窗口矩形铺，最大化/拉伸后旧背板会留在原地，屏上就是
/// "页面底色只铺到旧窗口右边、其余一片黑"。
fn extend_frame(hwnd: HWND) {
    let margins = MARGINS {
        cxLeftWidth: -1,
        cxRightWidth: -1,
        cyTopHeight: -1,
        cyBottomHeight: -1,
    };
    unsafe {
        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
    }
}

/// 逼 DWM 按**当前**窗口矩形重算 Mica 背板。
///
/// 实测：拆掉原生标题条之后，重设 `SYSTEMBACKDROP_TYPE`、重贴扩展边框、cloak、
/// `SWP_FRAMECHANGED`、重建交换链都不会让背板跟着新尺寸走；只有窗口重新"显示"一次
/// 才会重算。所以藏一下再显示（同一个消息里，通常不会真的闪一帧），再把前台还给它 ——
/// `ShowWindow(SW_SHOW)` 本身不激活窗口。
fn fix_backdrop(hwnd: HWND) {
    if BACKDROP_FIX.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let was_fg = unsafe { GetForegroundWindow() } == hwnd;
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
    if was_fg {
        unsafe {
            let _ = SetForegroundWindow(hwnd);
        }
    }
    BACKDROP_FIX.store(false, std::sync::atomic::Ordering::Relaxed);
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

/// 散文件那份图标在哪 —— `build.rs` 的输入，运行时只在**资源表读不到**时才走到这里。
///
/// 发布包不再随 exe 附 `assets\`：图标已经编进镜像的 `RT_GROUP_ICON`（id 1），托盘、标题栏
/// 与资源管理器都先读它。这里留两个落点是为了开发期直接跑 `target\...`（回到仓库那份）
/// 与非 msvc 构建（`build.rs` 不写资源）—— 两个都认，但**只写这一遍**：标题栏、任务栏与
/// 托盘要的是同一颗图，路径各解一遍迟早出现"托盘和标题栏不一样"。
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

/// 应用图标：先问**自己的镜像**，再问随包的散文件，最后才退系统默认图标。
///
/// `build.rs` 早就把 `assets/focusflow.ico` 编成了 `RT_GROUP_ICON`（资源管理器、快捷方式
/// 里那颗就是它），但这里以前只认散文件 —— 于是"只拷 exe、不拷 `assets\`"这种换版本的
/// 装法上，文件图标是对的、托盘与标题栏却是另一颗，看着就像程序自己画错了。
/// ⚠ `PCWSTR(1usize as *const u16)` 就是 `MAKEINTRESOURCEW(1)`：低位是资源 id、不是地址，
/// 这是 Win32 的约定而非笔误，所以按 lint 说的换成 `std::ptr::dangling` 反而会拿不到资源。
#[allow(clippy::manual_dangling_ptr)]
fn load_app_icon() -> HICON {
    unsafe {
        let hinst = GetModuleHandleW(None).map(Into::into).unwrap_or_default();
        // MAKEINTRESOURCEW(1)：资源表里那条 `RT_GROUP_ICON` 的 id 就是 1（见 build.rs）。
        if let Ok(h) = LoadIconW(Some(hinst), PCWSTR(1usize as *const u16)) {
            return h;
        }
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

/// 托盘那颗图：先取 exe 自己资源表里那份（`build.rs` 把 `assets/focusflow.ico` 编成了
/// `RT_GROUP_ICON`，id 就是 1），再退随包的散文件，两处都没有才退代码画的那颗圆点。
///
/// 现场起因：他把新 exe 单独拷进安装目录、没带 `assets\` —— 资源管理器里是蓝底白 F，
/// 托盘里却是另一颗，而同一颗图一直就在这个镜像里，只是运行时没人读它（"同一颗图两条
/// 取法，少接的那一条恰好是屏上走的那一条"）。
///
/// 按系统的"小图标"尺寸取档（100% 缩放下 16 像素，那份 `.ico` 里正好有 16 那一档 ⇒
/// 不是把 32 硬压小）。`from_resource` 与 `from_path` 用的是同一个解码器 ⇒ 颜色与 alpha
/// 不经过第二把尺子（贴图过曝那次事故的原因就是多了一道转换）。
fn tray_icon_image() -> std::result::Result<Icon, String> {
    let px = unsafe { GetSystemMetrics(SM_CXSMICON) };
    let side = if px > 0 { px as u32 } else { 16 };
    if let Ok(icon) = Icon::from_resource(1, Some((side, side))) {
        return Ok(icon);
    }
    let path =
        app_icon_file().ok_or_else(|| "镜像资源与 assets/focusflow.ico 都没读到".to_string())?;
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

    // 静默启动必须在**建窗之前**定下来：带着 WS_VISIBLE 建，创建那一下就已经画出来了。
    let hidden = hidden_at_startup(
        &std::env::args().skip(1).collect::<Vec<_>>(),
        start_to_tray_cfg(),
    );
    GLASS_ON_FIRST_SHOW.store(hidden, std::sync::atomic::Ordering::Relaxed);
    // 上一次的窗口矩形（逻辑像素）。没有文件 / 存档坏了就回默认那一组：新装用户看到的
    // 仍是接这根线之前的那副样子。
    let saved_window = focusflow_core::window_state::load();
    let start_maximized = saved_window.as_ref().is_some_and(|s| s.maximized);
    MAIN_MAXIMIZED.store(start_maximized, std::sync::atomic::Ordering::Relaxed);
    // 静默启动时窗口建出来是隐藏的：最大化不能在建窗这一下做（那会直接把窗口亮出来），
    // 欠的这一次由 `bring_to_front` 在第一次唤出时补。
    MAIN_MAXIMIZE_ON_SHOW.store(
        hidden && start_maximized,
        std::sync::atomic::Ordering::Relaxed,
    );
    // 存档里带着"量这组坐标时那块屏的缩放"，直接换成物理像素建窗；
    // 建完只需要把可能落在屏幕外的矩形夹回来。
    let (init_x, init_y, init_w, init_h) = match &saved_window {
        Some(st) => physical_rect_of(st),
        None => physical_rect_of(&MainWindowState::defaults()),
    };
    let style = if hidden {
        WS_OVERLAPPEDWINDOW
    } else {
        WS_OVERLAPPEDWINDOW | WS_VISIBLE
    };
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            CLASS_NAME,
            w!("FocusFlow"),
            style,
            init_x,
            init_y,
            init_w,
            init_h,
            None,
            None,
            Some(hinstance.into()),
            None,
        )?
    };

    // 创建窗口时那条 WM_NCCALCSIZE 的 wParam 是 FALSE（系统按"带标题条"给出的建议值），
    // 强制重算一次框架，才会走 TRUE 那条把客户区撑满整窗 —— 拆原生标题条的标准触发点。
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
    }

    // 存档里的逻辑像素 → 这块屏上的物理像素，再夹进工作区：换分辨率、拔掉副屏之后，
    // 上次那组坐标可能整块落在屏幕外面，不夹回来窗口就再也点不到。
    if let Some(st) = &saved_window {
        let target = clamp_main_rect(work_area_near_window(hwnd), physical_rect_of(st));
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                None,
                target.0,
                target.1,
                target.2,
                target.3,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    let app = Box::into_raw(Box::new(App::new(hwnd, false)?));
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, app as isize);
        if hidden {
            // 静默那一趟不唤窗、也不贴材质（贴了白贴，见 `GLASS_ON_FIRST_SHOW`），欠的这一次
            // 由 `bring_to_front` 在第一次真的唤出时补。托盘照旧在 —— 它是这扇窗唯一的入口。
            tracing_note("静默启动：只进托盘（命令行 --hidden 或 [gui] start_to_tray）");
        } else {
            // 存档说上次是最大化：`SW_SHOWMAXIMIZED` 连显示一起做，别先显示再最大化（会闪一下
            // 普通尺寸的版面）。
            let _ = ShowWindow(
                hwnd,
                if start_maximized {
                    SW_SHOWMAXIMIZED
                } else {
                    SW_SHOW
                },
            );
            // 材质必须在**显示之后**贴：窗口还没显示时 DWM 会把深色属性/背板丢掉。
            (*app).mica = apply_glass(hwnd);
        }
        // 主窗口**不置顶**：置顶留给悬浮窗（它要压在任务栏上）。原型期这里挂过
        // `HWND_TOPMOST`，理由是"自动化截图时会被浏览器盖住"——那是测试的麻烦，
        // 代价却是用户切到别的窗口时这块界面永远压在上面（他 2026-10-30 点的）。
        // 验收套件的截图走 `PrintWindow`、点击走 `PostMessage`，不依赖窗口在最上层。
        // 攒批的按键增量得有收件方：插件在启动这一趟就加载，不再等第一次进插件页。
        (*app).load_plugins_at_startup();
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

/// 托盘那颗图能不能只靠 exe 自己的资源表。
///
/// 现场起因：他把新 exe 单独拷进安装目录（没带 `assets\`），于是资源管理器里是蓝底白 F、
/// 托盘里却是代码画的那颗圆点 —— 同一颗图就在镜像里，只是那条链没接。
#[cfg(test)]
mod icon_resource_tests {
    use super::*;

    /// 把资源表那份拼回 `.ico` 后，必须与随包的原始文件**逐字节相同**。
    ///
    /// 这条是"托盘可以不再依赖散文件"的地基：字节相同 ⇒ 走的是 `Icon::from_path`
    /// 那条已经验证过的解码路径，颜色与 alpha 不经过第二把尺子（贴图过曝那次就是
    /// 多了一道转换）。
    /// 真的从**这个可执行文件自己的资源表**里取出那颗图。
    ///
    /// 前两条钉的是形状与顺序，这条把整条链跑完一遍：`build.rs` 写进镜像的
    /// `RT_GROUP_ICON`（id 1）能被 `Icon::from_resource` 解出来 ⇒ 托盘不再需要
    /// `assets\` 那个散文件也在。拿不到就是资源根本没链进镜像，那时这条要先红。
    #[test]
    fn the_tray_icon_resolves_from_this_executables_resource_table() {
        Icon::from_resource(1, Some((16, 16)))
            .unwrap_or_else(|e| panic!("镜像里那颗图取不出来：{e}"));
    }

    /// 取图顺序按调用形状钉：先镜像资源，再散文件，圆点只当最后的退路。
    /// 这条盯的是"少接一份"那一族：`build.rs` 那份资源一直在，只是运行时没人读它。
    #[test]
    fn the_tray_and_the_title_bar_both_ask_the_module_first() {
        let src = include_str!("main.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let at = prod
            .find("fn tray_icon_image")
            .expect("托盘取图函数的起点找不到了");
        let tray = &prod[at..prod[at..]
            .find("\nfn ")
            .map(|e| at + e)
            .unwrap_or(prod.len())];
        let m = tray
            .find("Icon::from_resource(1, Some((side, side)))")
            .expect("托盘要先问镜像里的 RT_GROUP_ICON");
        let f = tray.find("app_icon_file()").expect("散文件仍是第二顺位");
        assert!(m < f, "顺序反了：镜像资源该在散文件之前");
        assert!(
            prod.contains("LoadIconW(Some(hinst), PCWSTR(1usize as *const u16))"),
            "标题栏/任务栏那颗也要先取镜像里的 RT_GROUP_ICON"
        );
    }
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
            "fn draw_plugin_chart",
            "fn plugin_chart_plan",
            "fn draw_board_page",
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

    /// 插件图表与内置页那张折线**共用同一把纵轴尺子**：顶格由 [`axis_top`] 定，
    /// 最高点拿刻度反推必须回得到原值。
    ///
    /// 分两处算分母的后果内置页已经演过一遍：点高按 `峰 × 1.12`、刻度按另一套，
    /// 整条纵轴每档少报约 12%，而"峰刚好贴着顶格"看着完全像对的 ——
    /// 只有拿刻度反推高度才看得出来（本轮的插件图必须走同一条路，否则同一天的数字
    /// 在插件面板与内置看板页会画成两个高度）。
    #[test]
    fn a_plugin_chart_shares_the_axis_ruler_with_the_builtin_trend() {
        let card = rect(0.0, 0.0, 900.0, 150.0);
        let points: Vec<i64> = vec![10, 40, 25, 7];
        let plan = plugin_chart_plan(card, "line", &points);
        assert_eq!(
            plan.top,
            axis_top(40),
            "顶格必须是 axis_top(峰值)，不许自己乘系数"
        );
        assert!(plan.top >= 40, "顶格低于峰值会把最高点画到卡片外面");

        let y = line_point_y(plan.plot, plan.top, 40);
        let back = ((plan.plot.bottom - y) / plot_height(plan.plot) * plan.top as f32) as i64;
        assert!(
            (39..=41).contains(&back),
            "拿刻度反推最高点 = {back}，应为 40 ⇒ 点高与刻度分了两个分母"
        );

        let ticks: Vec<i64> = (0..5).map(|i| line_tick_value(plan.top, i)).collect();
        assert_eq!(ticks[0], plan.top, "最上面那条网格必须就是顶格");
        assert_eq!(ticks[4], 0, "最下面那条必须是 0（基线）");
        let step = plan.top / 4;
        for i in 1..5 {
            assert_eq!(
                ticks[i - 1] - ticks[i],
                step,
                "四档必须等差，否则刻度念得出、图上量不出来: {ticks:?}"
            );
        }
        for i in 0..5 {
            assert!(
                line_grid_y(plan.plot, i) <= plan.plot.bottom + 0.01,
                "网格线第 {i} 条落到绘图区外面"
            );
        }
    }

    /// 空数据、漏写 kind、写错 kind 这三种都要"仍然画出可读的一张图"，不是留白、不是消失。
    ///
    /// 3.2.2 把这条写成了硬要求（空数据画基线并显示「暂无数据」）；而 `points` 为空
    /// 是这一族最常见的真实状态：今天还没人按键、新装的第一周、插件刚启用还没数据。
    /// 顶格那一头还有一条隐藏要求：**不能取 0**，[`line_point_y`] 拿它做除数。
    #[test]
    fn an_empty_or_mis_kinded_plugin_chart_still_draws_something() {
        let card = rect(0.0, 0.0, 900.0, 150.0);
        let empty = plugin_chart_plan(card, "", &[]);
        assert!(empty.empty, "没有点 ⇒ 必须走「暂无数据」那一支，而不是留白");
        assert_eq!(empty.label_step, 0, "空数据不标横轴文本");
        assert_eq!(empty.top, axis_top(0), "顶格不许取 0：它是点高的除数");
        assert!(!empty.bars, "kind 漏写按折线画");

        for kind in ["bar", "BAR", " bar ", "Bar"] {
            assert!(chart_is_bar(kind), "{kind} 该按柱状画");
        }
        for kind in ["", "line", "LINE", "pie", "柱状", "histogram"] {
            assert!(
                !chart_is_bar(kind),
                "{kind} 不该被认成柱状：认错了会画成完全不同的图，而写的人看不出来"
            );
        }
    }

    /// 横轴文本必须**永远包含最后一格**，且总标注数不超过那一档上限。
    ///
    /// 按"索引整除"来标会让最后一天有没有文本取决于点数能否整除步长 —— 而趋势图最需要
    /// 认出的就是最新那一格（今天）。这条函数从尾部往前数，把这个依赖拿掉。
    #[test]
    fn the_newest_point_always_gets_an_axis_label() {
        for n in [1usize, 2, 3, 7, 8, 13, 30, 31, 60, 90] {
            let step = n.div_ceil(PV_CHART_MAX_LABELS);
            assert!(
                chart_label_at(n, step, n - 1),
                "{n} 个点时最后一格没有文本（step={step}）"
            );
            let marked: Vec<usize> = (0..n).filter(|i| chart_label_at(n, step, *i)).collect();
            assert!(
                marked.len() <= PV_CHART_MAX_LABELS,
                "{n} 个点标了 {} 格，超出这一档上限 {PV_CHART_MAX_LABELS} ⇒ 糊成一片",
                marked.len()
            );
            assert!(
                marked.iter().all(|i| *i < n),
                "标到了不存在的格: {marked:?}"
            );
        }
        assert!(
            !chart_label_at(5, 0, 4),
            "step=0（空数据/没文本）一个都不标"
        );
        assert!(!chart_label_at(5, 2, 5), "越界索引不标");
    }

    /// 文本比点少时，最后一个文本必须对最后一个点（文档承诺的「尾对齐」）。
    /// 按左对齐那版会把「最近三天的日期」画在最早三格上：数不变、字不差，归属全错，
    /// 除了数格子没有第二种发现方式。
    #[test]
    fn a_short_label_list_hangs_on_the_newest_points() {
        assert_eq!(chart_label_index(10, 3, 0), 7, "缺的是前面旧点的文本");
        assert_eq!(chart_label_index(10, 3, 2), 9, "最后一个文本对最后一个点");
        assert_eq!(chart_label_index(3, 3, 0), 0, "一样长时一格都不挪");
        assert_eq!(
            chart_label_index(10, 0, 0),
            10,
            "一个文本都没有时不许倒着算（下标越界由标记判据滤掉）"
        );
    }

    /// 横轴那一格：最右一格是右对齐画的，右端不许伸出绘图区。
    ///
    /// 从前只夹左端，`cell_w` 90 时最后一格右端落在 `plot.right + 45` —— 比卡片
    /// 右边距还宽，那截被内容区裁剪吃掉：日期少一个字，而图本身看着没坏。
    #[test]
    fn axis_label_boxes_stay_inside_the_plot() {
        let plot = rect(46.0, 46.0, 800.0, 76.0);
        for cx in [plot.left, plot.left + 123.0, plot.right] {
            for cell_w in [28.0f32, 60.0, 90.0] {
                let r = chart_label_rect(plot, cx, cell_w);
                assert!(
                    r.left >= plot.left - 0.01,
                    "横轴文本左端跑出绘图区: {r:?}（cx={cx}，格宽={cell_w}）"
                );
                assert!(
                    r.right <= plot.right + 0.01,
                    "横轴文本右端跑出绘图区: {r:?}（cx={cx}，格宽={cell_w}）"
                );
            }
        }
        // 中间那一格仍然居中：别为了夹边把整排文本整体挪走。
        let mid = chart_label_rect(plot, plot.left + 400.0, 60.0);
        assert!(
            (mid.left - (plot.left + 370.0)).abs() < 0.01,
            "中间那一格被挪了：{mid:?}"
        );
    }

    /// 布局这一头：图占自己那一格高度、进 ops、并跟着算进内容高度；
    /// 而点击那一圈**必须没有它**（本期不做交互，见 3.2.2）。
    ///
    /// 后半截是接线判据而不是行为判据：交互没做的时候，加一个 `POp::Chart` 的点击分支
    /// 不会让任何用走红，却会把这块矩形变成一块吃掉后续按钮的点击区（表格那一条
    /// `a_table_with_more_rows_than_drawn_has_no_invisible_hit_rows` 就是同族的教训）。
    #[test]
    fn a_plugin_chart_takes_its_own_row_and_stays_inert_to_clicks() {
        let view = focusflow_core::plugins::PluginView {
            title: "夹具".to_string(),
            widgets: vec![
                focusflow_core::plugins::Widget::Chart {
                    kind: "line".to_string(),
                    labels: vec!["一".to_string(), "二".to_string()],
                    points: vec![3, 9],
                },
                focusflow_core::plugins::Widget::Button {
                    id: "b".to_string(),
                    text: "刷新".to_string(),
                    disabled: false,
                    modal: None,
                    sel: false,
                    group: String::new(),
                },
            ],
        };
        let area = rect(0.0, 0.0, 900.0, 400.0);
        let ops = plugin_view_ops(&view, area, 0.0, &HashMap::new());
        let Some(POp::Chart {
            rect: r,
            kind,
            labels,
            points,
        }) = ops.first()
        else {
            panic!("夹具那张图没走到 POp::Chart，实得 {} 项", ops.len());
        };
        assert_eq!(r.bottom - r.top, PV_CHART_H, "卡片高度必须由那一处常量定");
        assert_eq!(kind, "line");
        assert_eq!(labels.len(), 2);
        assert_eq!(points, &vec![3, 9]);

        // 排在它后面的按钮必须被推下去，而不是压在图上（正文没有重叠布局）
        let Some(button_top) = ops.get(1).map(|op| op.bounds().top) else {
            panic!("按钮那一格没进布局：ops 只有 {} 项", ops.len());
        };
        assert!(
            button_top >= r.bottom,
            "图表后面的控件压在了图上: 按钮 top={button_top} 卡片 bottom={}",
            r.bottom
        );
        assert!(
            plugin_content_height(&view, area) >= PV_CHART_H,
            "内容高度没把这一格算进去 ⇒ 滚动位置会被夹过头，图的下半截看不见"
        );

        // 接线：`POp::Chart` 在生产段只出现在三处（bounds 那一条 | 列表、布局 push、画），
        // 第四处只可能是有人给它加了点击分支。
        let src = include_str!("main.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let flat: String = prod.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(
            flat.matches("POp::Chart{").count(),
            3,
            "图表这一族的落点不再是「bounds／布局／画」三处 —— \
             本期不做交互，多一处要先问它是不是把这块矩形变成了点击区"
        );
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
            prod.contains("let foot = footer_line(&snap.data_home, &snap.gaps, lane);"),
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
            3,
            "详情弹窗 / 分析页两个互斥分支 / 其余屏的统一收尾：三处，新增一屏要跟着补"
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
        // 2026-10-30 起那一拍睡的是"可被催醒的小片"，所以针脚换成催醒包装那一句：
        // 要钉的还是"分档出来的毫秒真的交给了睡眠"，换形状不等于换判据。
        let sleeps_on = ["sleep_or_poke(", "tick_ms", ")"].concat();
        assert_eq!(
            prod.matches(sleeps_on.as_str()).count(),
            1,
            "那一拍必须睡在分档出来的毫秒上（由 sleep_or_poke 代睡，见 POKE_SLICE_MS）"
        );
        let slice_sleep = ["Duration::from_millis(", "slice", ")"].concat();
        assert!(
            prod.contains(slice_sleep.as_str()),
            "催醒包装里没有真的睡那一片：tick_ms 交进去也没人等"
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
    ///
    /// 2026-10-31 起它不再回 bool 而是回 `Readers`：卡片一个人挂着时**必须**是 `Card`
    /// 而不是 `Panel` —— 判成 `Panel` 的症状是"整批面板查询全天在跑"（现量今日档 10 条
    /// 语句，而卡片只要 2 条），反过来判成 `None` 的症状是卡片数字冻住。
    #[test]
    fn polling_stops_only_when_nobody_is_watching() {
        assert_eq!(
            watching(true, false, false),
            Readers::Panel,
            "主窗口开着就要查整批"
        );
        assert_eq!(
            watching(true, false, true),
            Readers::Panel,
            "窗口与卡片都开着 ⇒ 按最宽的那个读者算"
        );
        assert_eq!(
            watching(false, false, true),
            Readers::Card,
            "主窗口收进托盘、卡片还开着 ⇒ 只查卡片那一问（卡片是直接看的那个）"
        );
        assert_eq!(watching(true, true, false), Readers::None, "最小化＝没人看");
        assert_eq!(
            watching(false, false, false),
            Readers::None,
            "隐藏且卡片关着＝一拍都不查"
        );
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
            4,
            "一处定义 + 快照循环里两处（睡前、睡完）+ 看板页那一处 —— 数量变了说明接线动过"
        );
        // 三档真的各有出口：`Card` 那一支必须把整批 `data::load` 换掉并当场 continue，
        // 少了那次 continue，卡片那一拍查完之后还是会跑整批 —— 数照旧、白加一问。
        let flat: String = prod.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            flat.contains("ifr==Readers::Card{")
                && flat.contains("letthin=data::load_card_only();"),
            "卡片单独在看时那一拍不再走薄臂了（`Readers::Card` 分支或那一问不见了）"
        );
        let card_at = flat.find("ifr==Readers::Card{").expect("Card 那一支不见了");
        let branch = &flat[card_at..card_at + 900.min(flat.len() - card_at)];
        assert!(
            branch.contains("continue;"),
            "Card 那一支没有当场收尾：整批查询还是会跟着跑"
        );
        // 就地补写那两格：托盘 tooltip 与验收状态行读的就是它们，漏了会念出启动时那份旧数
        assert!(
            branch.contains("g.today=thin.today;")
                && branch.contains("g.today_app_seconds=thin.today_app_seconds;"),
            "只有卡片那一拍不再把今日/时长写回共享快照：托盘与验收读数会停在旧值"
        );
        // 第四处是内置看板页那一档（30 秒重查）。它必须把"有没有人看"当成**入参**交进去，
        // 而不是在 UI 里另写一个时间比较：后者会最小化了还问库，而这一页看不见时问库
        // 没有任何读者（方案 3.2.3 那张表的最后一行）。
        assert!(
            flat.contains("letwatching=matches!(watching_now(hwnd),Readers::Panel)")
                && flat.contains(
                    "dashboard::should_refresh(app.dash.elapsed_secs(),app.view==6,watching)"
                ),
            "看板那一档不再把「有没有人看」当条件：判据那三个入参少了一个"
        );
    }

    /// 接线腿：导入那一支真的在写完库之后重锚了「今日」。
    ///
    /// 摘掉这一句的症状不是红、是屏上一个数字停在导入前的值：写线程那份内存缓存不会
    /// 因为库里多了几行就自己动，而回执那句「导入完成：共 N 条」照样说得出口 ——
    /// "说了、没改"是这一族最难自查的形状，所以它得被钉在**文本形状**上。
    ///
    /// ⚠ 钉的是调用形态与次序，不是函数名：只数 `reanchor_today_after_external_write`
    /// 会出现"注释里提一次就够数"的假绿。这里要的是「导入那一句之后、回执拼出来之前」
    /// 有且只有一处带括号的调用 —— 挪到别的按钮上、或漏在某个早退分支后面，都算红。
    ///
    /// **这条自己就假绿过一次，不是我预防到的**：本场把生产里那句调用整块注释掉来验区分性，
    /// 判据照样绿 —— 因为它在整份 `prod` 上找字，而那三行注释里那句话与针脚逐字相同。
    /// 所以这里先按行剥掉 `//` 之后的部分再数（同 `data.rs` 那批守卫的做法）；
    /// 剥完再跑同一次变异才第一次看到它红。
    #[test]
    fn the_import_branch_reanchors_today_right_after_writing_the_db() {
        let src = include_str!("main.rs");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod: String = src[..cut]
            .lines()
            .map(|line| match line.find("//") {
                Some(i) => &line[..i],
                None => line,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let call = ["collect::reanchor_today_after_external_write", "()"].concat();
        assert_eq!(
            prod.matches(call.as_str()).count(),
            1,
            "导入那一支要有且只有一处重锚调用（数量变了说明接线动过；注释里提一句不算调用）"
        );
        let imported_at = prod
            .find("import_legacy_data(&dir)")
            .expect("导入那一支不见了（改名或换了写法，判据的锚要跟着改）");
        let receipt_at = prod
            .find("导入完成")
            .expect("回执那句不见了（它是这条腿的次序右边界）");
        let call_at = prod.find(call.as_str()).expect("重锚调用不见了");
        assert!(
            call_at > imported_at && call_at < receipt_at,
            "重锚必须夹在「导入返回」与「拼回执」之间：导入={imported_at} 重锚={call_at} 回执={receipt_at}"
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
    /// 设置页一行的"亮"与"点"必须是同一个矩形：悬停从前只判 `y`，指针压在左边的
    /// 分类导航上也会点亮正文那一行（点导航只是切分类，不会打开那一行）。
    #[test]
    fn the_settings_hover_band_matches_the_click_band() {
        use super::{hit, settings_geom, settings_row_rect};
        let w = 960.0;
        let g = settings_geom(w, 860.0);
        let r = settings_row_rect(&g, w, g.list_top);
        assert_eq!(r.left, g.body_left);
        assert_eq!(r.right, w - 32.0);
        assert!(hit(r, g.body_left + 10.0, g.list_top + 5.0), "行内要命中");
        assert!(
            !hit(r, g.nav_x + 5.0, g.list_top + 5.0),
            "压着分类导航时正文那一行不该亮"
        );
        assert!(!hit(r, 20.0, g.list_top + 5.0), "卡片左边距里不该亮");
        // 画 / 悬停 / 点击三处读同一个函数。
        let src = include_str!("main.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        // 画那一行仍读同一个矩形；悬停与点击共用 `settings_row_at`（两处，少一处就是又分家）。
        assert!(
            prod.contains("let r = settings_row_rect(&g, w, y);"),
            "画那一行没有读 settings_row_rect"
        );
        assert!(
            prod.contains("if hit(settings_row_rect(g, w, y0), x, y) {"),
            "命中函数没有读画那一份矩形"
        );
        assert_eq!(
            prod.matches("settings_row_at(&items").count(),
            2,
            "悬停（control_at）与点击（on_click）都该走 settings_row_at，各留一份就是又各数一遍"
        );
        assert!(
            prod.contains("Some(Ctl::SetRow(i))"),
            "悬停没有从 control_at 那一个命中口径推导"
        );
    }

    /// 说明那一列必须停在值框/按钮之前：从前几行"基础项"写死 560 / 640，而 960 宽时
    /// 真实空位只有 `value_x - (body_left + 12) - 18 ≈ 450`，一条常见的数据目录路径
    /// 就会整句画到「打开」按钮身上。
    #[test]
    fn the_settings_note_column_stops_before_the_value_box() {
        use super::{settings_geom, settings_note_w, settings_row_rect};
        let w = 960.0;
        let g = settings_geom(w, 860.0);
        let r = settings_row_rect(&g, w, g.list_top);
        let note_w = settings_note_w(&g, r);
        assert_eq!(note_w, g.value_x - (r.left + 12.0) - 18.0);
        assert!(
            r.left + 12.0 + note_w <= g.value_x,
            "说明画到值框/按钮底下了：{} > {}",
            r.left + 12.0 + note_w,
            g.value_x
        );
        assert!(note_w < 560.0, "960 宽时这一列本来就没 560：实得 {note_w}");
        // 反向腿：窗口再窄，这一列也不许缩成负的或看不见。
        let narrow = settings_geom(480.0, 640.0);
        let nr = settings_row_rect(&narrow, 480.0, narrow.list_top);
        assert!(settings_note_w(&narrow, nr) >= 120.0);
    }

    /// 清理那一句要把"执行前快照没兜住"一起带上屏（那是删完才发现没有退路的那一半）。
    ///
    /// 三条腿：①`snapshot_warning` 为空 ⇒ 屏上就是 core 那一句话，一个字都不许多
    /// （把那句写死成"总在"就红在这儿）；②有 ⇒ core 的原话仍在前面、快照那半句跟着到场；
    /// ③生产段确实走这条出口 —— 旧的 `cleanup_line(30, &r)` 那种把快照结局丢掉的写法必须为零。
    #[test]
    fn the_cleanup_line_on_screen_carries_the_missing_safety_net() {
        use super::cleanup_status_line;
        use focusflow_core::db::maintenance::{cleanup_line, MaintenanceReport};
        let clean = MaintenanceReport::default();
        let base = cleanup_status_line(30, &clean);
        assert_eq!(
            base,
            cleanup_line(30, &clean),
            "快照兜住了就不许多说一个字: {base}"
        );

        let r = MaintenanceReport {
            snapshot_warning: Some("执行前快照只兜住了 1 份".to_string()),
            ..MaintenanceReport::default()
        };
        let said = cleanup_status_line(30, &r);
        assert!(
            said.starts_with(base.as_str()) && said.contains("只兜住了 1 份"),
            "core 的原话在前、快照那半句也要到场: {said}"
        );

        let src = include_str!("main.rs").replace("\r\n", "\n");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let flat: String = src[..cut]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .flat_map(|l| l.chars())
            .filter(|c| !c.is_whitespace())
            .collect();
        assert_eq!(
            flat.matches("cleanup_line(30,&r)").count(),
            0,
            "又一处把执行前快照的结局丢掉：清理那一句要走带快照的那条出口"
        );
        assert_eq!(
            flat.matches("cleanup_status_line(").count(),
            2,
            "定义一颗 + 那颗钮一处，实得 {}",
            flat.matches("cleanup_status_line(").count()
        );
    }

    /// 八颗「执行」每颗要有自己那句"正在…"，不许借「正在清理」那句话。
    ///
    /// 病状在屏上：从前 `run_action` 自己 match `which`，只写了 `0`/`1` 两支，其余六支
    /// （清理、三种导出、导入、换数据文件夹）全落进 `_ => "正在清理 30 天前的数据…"`。
    /// 点「导出 CSV」的人于是看到"正在删 30 天以前的明细"——这张表里唯一标着「不可撤销」
    /// 那一档的话；同时三颗钮被 busy 灰掉、存盘对话框还开在后台线程上，人就在这时候强关。
    ///
    /// 三条腿：①每颗钮交回的就是表里自己那一栏（把 busy 话收回 match 里就红）；
    /// ②除「清理」那一支以外，任何一颗都不许说"清理"（这一条是缺陷的正身）；
    /// ③表里每栏都得写成"正在…"起头且非空（新加一颗却漏写第三栏 ⇒ 编译先不过，
    ///   这条腿钉的是"第三栏不许是空串"那种绕过）。
    #[test]
    fn each_action_button_names_its_own_busy_line() {
        use super::{action_busy_line, ACTIONS};
        assert_eq!(ACTIONS.len(), 8, "动作表的本场现量是八颗");
        for (i, (title, _, busy)) in ACTIONS.iter().enumerate() {
            let line = action_busy_line(i);
            assert_eq!(
                line, *busy,
                "第 {i} 颗（{title}）交回的不是表里那一栏: {line}"
            );
            assert!(
                line.starts_with("正在") && line.chars().count() > 3,
                "忙时那句要按这一族的写法说清在做什么，不许是空串或一个词: {title} → {line}"
            );
            if i != 2 {
                assert!(
                    !line.contains("清理"),
                    "「{title}」点下去不许说成在删明细（那是不可撤销那一档的话）: {line}"
                );
            }
        }
        assert!(
            ACTIONS[2].0.contains("清理"),
            "第 2 颗必须还是那颗清理钮，否则上面那条判据就成了恒真: {}",
            ACTIONS[2].0
        );
        assert_eq!(
            action_busy_line(ACTIONS.len()),
            "正在执行…",
            "越界那一支不许冒充任何一个具体动作"
        );
    }

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

    /// 一次按键只处理一次。Windows 的全局热键实现会先发 Pressed、松手后再发 Released；
    /// 两条都当按下处理，就会"按一次触发两次"（弹窗开了又关、唤出窗开一下又藏）。
    #[test]
    fn a_hotkey_press_is_handled_once_not_twice() {
        assert!(hotkey_is_press(HotKeyState::Pressed));
        assert!(!hotkey_is_press(HotKeyState::Released));
    }

    /// 「备份完成」那一句话由两份独立的事实拼成，两半都要能被单独说出口。
    ///
    /// 屏上以前只看 `failed`：拷贝前那批增量没等到（`missed_batch`）在界面上与"全都兜住"
    /// 长得一模一样，而那份文件里其实少了最近几秒的按键。三条腿各钉一边：两份都干净＝原句
    /// 一个字都不许多说、只少一批＝带上那半句、既有库没兜住又少一批＝两样都在（不许后者顶掉前者）。
    #[test]
    fn the_backup_line_says_both_what_missed_and_what_is_short() {
        let clean = backup_status_line(5, &[], false);
        assert_eq!(
            clean, "备份完成：5 份",
            "两份都干净时一个字都不许多说：{clean}"
        );
        let short = backup_status_line(5, &[], true);
        assert!(
            short.contains("5 份") && short.contains("最近几秒"),
            "少了最近一批要当场说出来：{short}"
        );
        let holes = backup_status_line(1, &["focusflow_2025.db".to_string()], true);
        assert!(
            holes.contains("focusflow_2025.db") && holes.contains("最近几秒"),
            "两样毛病都要在屏上，不是后者盖掉前者：{holes}"
        );
        let partial = backup_status_line(1, &["device_stats".to_string()], false);
        assert!(
            partial.contains("部分失败")
                && partial.contains("device_stats")
                && !partial.contains("最近几秒"),
            "没兜住的库要说，而这批没少就不许瞎说少了最近几秒：{partial}"
        );
    }

    /// 配置写不进持久化队列那句"这次没落盘"必须带到状态行上。
    ///
    /// 三条腿：①纯函数那一半钉**空串那一支也要**（写进去了就多一个字都不许说），
    /// 只钉"出现过 `没写进` 三个字"的判据盯不到把 `Ok` 也念成出错的改法；
    /// ②接线那一半钉落点数目；③**钉喂给它的是哪一次调用** —— 这一半是这一笔补上的：
    /// `config::set` 恒回 `Ok(())`（它只改内存 + 叫醒去抖线程），把它的结局喂给
    /// `config_write_note` 时那个 `Err` 分支是**死路**，屏上那句"这次没落盘"永远说不出口，
    /// 而上一笔的判据照样绿。能落盘的只有当场走完 `save()` 的 `set_now`，
    /// 所以形状要钉到那一个名字上（`arrange_data_move` 是另一条已经自己补了 save 的路，
    /// 它的写法是 `cfg.set(...)` + 紧随的 `cfg.save()`，不在这三条判据的射程里，也不该在）。
    #[test]
    fn a_config_write_that_failed_is_said_out_loud() {
        assert_eq!(config_write_note("全局热键总开关", Ok(())), "");
        let said = config_write_note("全局热键总开关", Err("保存线程没了".to_string()));
        assert!(
            said.contains("全局热键总开关") && said.contains("保存线程没了"),
            "要说清是哪一样、为什么没落盘，实得: {said}"
        );
        assert!(
            said.contains("重启"),
            "还要说清这一句只到本次运行，否则用户以为改完了：{said}"
        );

        let src = include_str!("main.rs").replace("\r\n", "\n");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let flat: String = src[..cut]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .flat_map(|l| l.chars())
            .filter(|c| !c.is_whitespace())
            .collect();
        for forbidden in [
            "let_=cfg.set(",
            "let_=focusflow_core::config::instance()",
            "::instance().set(",
        ] {
            assert_eq!(
                flat.matches(forbidden).count(),
                0,
                "又一处把写盘结局折掉了：{forbidden}"
            );
        }
        // 十一个用户手势的写点（总开关一颗 + 采集三条 + 开关行 + 值框提交 + 悬浮窗 + 主题
        // + 热键那颗钮 + 按下录入 + 托盘「记录剪贴板」）各喂一次 `set_now`，加上定义那一行
        // = 十二处。
        assert_eq!(
            flat.matches("config_write_note(").count(),
            12,
            "落点数目变了，实得 {}",
            flat.matches("config_write_note(").count()
        );
        assert_eq!(
            flat.matches("set_now(").count(),
            11,
            "写点没都换成能问出结局的那一条调用，实得 {}",
            flat.matches("set_now(").count()
        );
    }

    /// 接线腿：换视图那一支必须把热键录入态一起清掉。
    ///
    /// 钉的是"与那一串状态清理同批"，不是"文件里出现过这句"——`capturing` 在 Esc、
    /// 那颗 ✕、`start_edit_setting` 里也清，光数出现次数盯不到换页这一支。
    /// 症状：录入提示画在设置页那一格里，换到别的页之后提示看不见、按键照旧被吞
    /// （连 Esc 都不再收窗），用户只觉得"这个键怎么没反应了"。
    #[test]
    fn switching_view_clears_the_hotkey_capture_latch() {
        let src = include_str!("main.rs").replace("\r\n", "\n");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let flat: String = src[..cut]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .flat_map(|l| l.chars())
            .filter(|c| !c.is_whitespace())
            .collect();
        let needle = "self.view=i;self.scroll=0;self.sel=None;self.hover=None;\
             self.heat_hover=None;self.chart_hover=None;self.pv_select=None;\
             self.pv_modal=None;self.pv_hover=None;self.cleanup_confirm=false;\
             self.kbd_focus=None;self.tooltip_ready=false;self.tooltip_text=None;\
             self.capturing=None;"
            .replace(" ", "");
        assert_eq!(
            flat.matches(needle.as_str()).count(),
            1,
            "换视图那一支不再顺手清录入态（或被改写成两份）"
        );
    }

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

    /// 小钮只该挂在那几条绑定上：其它热键段的键（`enabled`）与别的段的行都不许有。
    /// 挂错行的症状是"点了叉把某条不相干的配置清空了"。
    #[test]
    fn only_the_bindings_get_the_button() {
        assert_eq!(hotkey_slot("hotkey", "toggle_window"), Some(0));
        assert_eq!(hotkey_slot("hotkey", "snip"), Some(1));
        assert_eq!(hotkey_slot("hotkey", "snip_annotate"), Some(2));
        assert_eq!(hotkey_slot("hotkey", "clipboard"), Some(3));
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
            !hotkeys_on_after_boot([&None, &None, &None, &None]),
            "一条都没注册上就该显示关"
        );
        assert!(
            hotkeys_on_after_boot([&None, &None, &one, &None]),
            "只有标注那条注册上也得显示开：这一格以前漏算了第三条"
        );
        assert!(hotkeys_on_after_boot([&one, &None, &None, &None]));
        assert!(hotkeys_on_after_boot([&None, &one, &None, &None]));
        assert!(hotkeys_on_after_boot([&None, &None, &None, &one]));
    }

    /// 总开关决定开机挂哪几条：关掉 ⇒ 一条都不占全局键；打开 ⇒ 三条，而且顺序就是
    /// `make_hotkeys` 里落进 toggle / snip / annotate 三个字段的顺序（下标对应，错一位的
    /// 症状是"按截图键把主窗口唤出来了"）。
    #[test]
    fn the_switch_decides_which_combos_boot() {
        assert!(
            combo_keys(false, true).is_empty(),
            "开关关掉就该一条都不注册"
        );
        assert_eq!(
            combo_keys(true, true),
            &COMBO_KEYS[..],
            "开机清单与顺序：0=唤出、1=松手即存、2=进标注、3=剪贴板"
        );
        assert_eq!(
            combo_keys(true, false),
            &COMBO_KEYS[..3],
            "剪贴板记录关掉时第 4 条不占用全局键"
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
                say("日志文件建不出来"),
            )
        };
        let (a, b, c, d, e, f, g) = all();
        assert_eq!(
            boot_notice(
                a.clone(),
                b.clone(),
                c.clone(),
                d.clone(),
                e.clone(),
                f.clone(),
                g.clone()
            ),
            a
        );
        assert_eq!(
            boot_notice(
                None,
                b.clone(),
                c.clone(),
                d.clone(),
                e.clone(),
                f.clone(),
                g.clone()
            ),
            b
        );
        assert_eq!(
            boot_notice(
                None,
                None,
                c.clone(),
                d.clone(),
                e.clone(),
                f.clone(),
                g.clone()
            ),
            c
        );
        assert_eq!(
            boot_notice(None, None, None, d.clone(), e.clone(), f.clone(), g.clone()),
            d
        );
        assert_eq!(
            boot_notice(None, None, None, None, e.clone(), f.clone(), g.clone()),
            e
        );
        assert_eq!(
            boot_notice(None, None, None, None, None, f.clone(), g.clone()),
            f,
            "前面都没话时说得出「热键没注册上」，也不许被最后那格挤掉"
        );
        assert_eq!(
            boot_notice(None, None, None, None, None, None, g.clone()),
            g,
            "文件日志那一层没建成必须被念出来：之前 core 存了这件事、全仓却一个读点都没有"
        );
        assert_eq!(
            boot_notice(None, None, None, None, None, None, None),
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
            // 文件日志那一层没建成：core 存了这件事，读点以前随旧前端一起删了（注释还写着
            // "由启动报告带出来"，屏上却一个字不说）。这条针钉的是**调用点真把它喂进来**。
            ["logger", "::file_log_failure()"].concat(),
        ] {
            assert!(
                args.contains(needle.as_str()),
                "启动状态行没把这句降级喂进去（缺 {needle}）"
            );
        }
    }
}

#[cfg(test)]
mod window_say_tests {
    use super::*;

    /// 「日均」那张卡下面那半句要跟着它**实际除过去的天数**说话。
    ///
    /// 改前是无条件写「按周期窗口」：`总计` 那一档除的是全历史里记到过次数的天数
    /// （`data::avg_divisor`），数额早修对了、话还说反 —— 而屏上这半句就是用户判断
    /// "这个日均算的是什么"的唯一依据。三条腿：窗口档说窗口、总计档说全历史并且
    /// **不许**再出现"窗口"二字、分母为 0 时兜成 1（不许写出除不动的那句话）。
    #[test]
    fn the_average_card_says_which_days_it_divided_by() {
        let window = Snapshot {
            period: 30,
            avg_days: 30,
            ..Default::default()
        };
        let line = avg_sub(&window);
        assert!(
            line.contains("窗口") && line.contains("30"),
            "有限窗口要说窗口天数: {line}"
        );
        assert!(!line.contains("全历史"), "窗口那一档不许念成全历史: {line}");

        let all = Snapshot {
            period: 0,
            avg_days: 412,
            ..Default::default()
        };
        let line = avg_sub(&all);
        assert!(
            line.contains("全历史") && line.contains("412"),
            "总计档要说全历史那几个字与实际天数: {line}"
        );
        assert!(
            !line.contains("窗口"),
            "「按周期窗口」正是总计档说谎的那句话: {line}"
        );

        let zero = Snapshot {
            period: 0,
            avg_days: 0,
            ..Default::default()
        };
        assert!(
            avg_sub(&zero).contains('1'),
            "分母为 0 要兜成 1，不许写「按全历史 0 天」: {}",
            avg_sub(&zero)
        );
    }

    /// 空榜那三句里的窗口必须就是这三屏数的那一段。
    ///
    /// 改前把「今日」写死在三句话里，而这三屏读的是 `window_days(period)` 那一段
    /// （`总计` 那一档是全历史）：切到近 30 天看到空榜，屏上说"今日还没有记录"，
    /// 用户就去翻今天的数据 —— 与它无关。`pending` 那一臂讲的是还没落库的**今日**增量，
    /// 与窗口无关，所以只有它保留"今日"的含义，且不许同时冒出两个窗口。
    #[test]
    fn the_empty_board_line_names_the_window_it_counted() {
        let apps = empty_board_line(1, "近 30 天", 0, 10, &[], "应用时长");
        assert!(
            apps.contains("近 30 天") && !apps.contains("今日"),
            "应用那一屏读的是 30 天，不许说今日: {apps}"
        );
        let dev = empty_board_line(2, "全部", 0, 10, &[], "设备统计");
        assert!(
            dev.contains("全部") && !dev.contains("今日"),
            "总计档的榜是全历史: {dev}"
        );
        let keys = empty_board_line(0, "近 7 天", 0, 10, &[], "按键统计");
        assert!(
            keys.contains("近 7 天") && keys.contains("还没有记录"),
            "排行那一屏同样跟着窗口: {keys}"
        );
        let pending = empty_board_line(0, "近 30 天", 12, 10, &[], "按键统计");
        assert!(
            pending.contains("还没落库") && pending.contains("12"),
            "有增量时说的是落库节奏: {pending}"
        );
        assert!(
            !pending.contains("还没有记录"),
            "不许给两个窗口口径: {pending}"
        );
        // 读不出来 ≠ 没有：这一栏有年份缺读时，整句换成"下界"口径。
        let gaps = vec!["2025 年库按键统计读不出来: 盘睡了".to_string()];
        let keys_bad = empty_board_line(0, "近 7 天", 0, 10, &gaps, "按键统计");
        assert!(
            keys_bad.contains("没读全") && !keys_bad.contains("还没有记录"),
            "缺读不许印成没有: {keys_bad}"
        );
        // 别的栏缺读不许串到应用那一屏（`gap_years` 按域名筛）。
        let apps_ok = empty_board_line(1, "近 30 天", 0, 10, &gaps, "应用时长");
        assert!(
            apps_ok.contains("还没有应用时长"),
            "按键的缺读串到应用屏了: {apps_ok}"
        );
    }

    /// 打卡条今天那颗点不许与同一页的进度条对着说话。
    ///
    /// `g.days` 末位吃的是"库值 + 未落库增量"，而 `flush_pending` 把一批从内存取走、还没写进
    /// SQLite 的那一瞬两边都没有它（`collect::today_total` 的注释记着实测 `5 → 0 → 5`）；
    /// 进度条与「今日 x / 目标 y」读的是那条权威值 ⇒ 同一页绿条配灰点，屏上自己跟自己讲
    /// "今天到底达没达标"。四条腿：达标时末格跟着亮、没过线时末格跟着灭、
    /// 末格不是今天（刚跨午夜的那一拍旧快照）一个字都不改、其余六格永远不动（读库是对的）。
    #[test]
    fn the_newest_dot_follows_the_authoritative_today() {
        use focusflow_core::stats::GoalStatus;
        let g = |days: Vec<(String, i64, bool)>| GoalStatus {
            goal: 10,
            today: 0,
            today_met: false,
            streak: 0,
            best: 0,
            days,
        };
        let d = |s: &str, met: bool| (s.to_string(), 0, met);
        let week = vec![
            d("2026-10-28", true),
            d("2026-10-29", false),
            d("2026-10-30", true),
            d("2026-10-31", false),
            d("2026-11-01", true),
            d("2026-11-02", true),
            d("2026-11-03", false),
        ];

        let met = goal_dot_states(&g(week.clone()), "2026-11-03", true);
        assert_eq!(
            met,
            vec![true, false, true, false, true, true, true],
            "只有今天那一格能被进度条那份读数改写"
        );

        let mut last_lit = week.clone();
        last_lit[6].2 = true;
        let met = goal_dot_states(&g(last_lit), "2026-11-03", false);
        assert!(!met[6], "进度条没过线时，今天那颗不许自己亮着");
        assert_eq!(
            met[..6].to_vec(),
            vec![true, false, true, false, true, true],
            "其余六格读的是已经落库的日子，不该被今天的读数改动"
        );

        let met = goal_dot_states(&g(week.clone()), "2026-11-04", true);
        assert_eq!(
            met,
            vec![true, false, true, false, true, true, false],
            "末格是昨天（刚跨过午夜那一拍）时不许拿今天的读数去点它"
        );

        // 接线腿：画点那一圈必须吃这个函数，且喂进去的是进度条那份 `met`
        let src = include_str!("main.rs").replace("\r\n", "\n");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let flat: String = src[..cut]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .flat_map(|l| l.chars())
            .filter(|c| !c.is_whitespace())
            .collect();
        assert_eq!(
            flat.matches("goal_dot_states(g,&today_str,met)").count(),
            1,
            "点那一圈没接上进度条那份权威读数，实得 {}",
            flat.matches("goal_dot_states(g,&today_str,met)").count()
        );
        assert_eq!(
            flat.matches("for(i,(_,_,dmet))in g.days.iter()").count(),
            0,
            "画点又改回直接吃 g.days（今天那颗会与进度条对着说）"
        );

        assert!(
            goal_dot_states(&g(vec![]), "2026-11-03", true).is_empty(),
            "空清单不许 panic，也不许凭空造出一格"
        );
    }
}

#[cfg(test)]
mod weekly_report_tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    /// 周报是唯一没有转义的导出面：导入数据里的键名、用户起的设备别名会原样进表格。
    /// `|` 拆列、`<` 在允许内联 HTML 的查看器里生效，两种都必须在落盘前处理掉。
    #[test]
    fn the_weekly_markdown_escapes_cells_from_external_data() {
        assert_eq!(md_cell("Ctrl|C"), "Ctrl\\|C");
        assert_eq!(md_cell("<img src=x>"), "&lt;img src=x&gt;");
        assert_eq!(md_cell("A&B"), "A&amp;B");
        assert_eq!(md_cell("两行\n名字"), "两行 名字");

        // 生产段三处表格（键名 / 应用名 / 设备名）都要过它，外加"最高单日"那一句。
        let src = include_str!("main.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        assert_eq!(
            prod.matches("md_cell(").count(),
            5,
            "定义 1 处 + 四个使用点（最高单日 / 键名 / 应用名 / 设备名）；少一处就是那一列没转义"
        );
        for old in ["\"| {} | {} |\", k,", "\"| {} | {} |\", name,"] {
            assert!(!prod.contains(old), "还有一列把外部名字原样写进表格：{old}");
        }
    }

    /// 「有 N 个年度库没读全」数的是**几本库**，不是几条读数。
    ///
    /// 一年一份文件，而那一年读不动时按键统计／应用时长／设备统计三栏各记一条
    /// （年份相同、域名不同 ⇒ `sort` + `dedup` 也并不掉）。从前周报那句直接念
    /// `gaps.len()`：一本读不动的库被说成三本，跨年那一周说成六本 —— 而这句话正是
    /// "上面那些数字只是下界"的凭据，念错本数没人会去核对。
    ///
    /// 三条腿：①同年三条 ⇒ 1 本 / 3 处，并且**不许**出现"有 3 个"；②两个年份四条 ⇒ 2 本 / 4 处；
    /// ③形状对不上的那条整串自成一类（吞掉一次读不动＝把下界念成实测，并进某一年＝少报一本）。
    /// 最后再加一条"全都读到时不多一个字"，钉住空清单那一支。
    #[test]
    fn the_weekly_report_counts_libraries_not_domain_lines() {
        let one_year = vec![
            "2025 年库按键统计读不出来: 盘睡了".to_string(),
            "2025 年库应用时长读不出来: 盘睡了".to_string(),
            "2025 年库设备统计读不出来: 盘睡了".to_string(),
        ];
        let line = weekly_gap_sentence(&one_year);
        assert!(
            line.contains("有 1 个年度库") && line.contains("3 处读数"),
            "三条同年的缺读只有**一本**库，而清单长度也要一起给: {line}"
        );
        assert!(!line.contains("有 3 个"), "不许把一本念成三本: {line}");
        assert!(line.contains("盘睡了"), "原因尾巴得跟着走出去: {line}");

        let mut two_years = one_year.clone();
        two_years.push("2024 年库每日趋势读不出来: 打不开".to_string());
        let line = weekly_gap_sentence(&two_years);
        assert!(
            line.contains("有 2 个年度库") && line.contains("4 处读数"),
            "两个年份四条读数: {line}"
        );

        let odd = vec![
            "2025 年库按键统计读不出来: x".to_string(),
            "读不出是哪一年的那一句".to_string(),
        ];
        let line = weekly_gap_sentence(&odd);
        assert!(
            line.contains("有 2 个年度库") && line.contains("读不出是哪一年的那一句"),
            "形状对不上那条要自成一类、既不许被吞也不许并进任何一年: {line}"
        );

        assert_eq!(
            weekly_gap_sentence(&[]),
            "",
            "全都读到了就多一个字都不许说（空报告里冒出一句警告也是病）"
        );
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

    /// 接线腿：「设备输入合计」那一句必须求和**在截断之前**。
    ///
    /// 换到之后求和的症状不是红、是那句安静地少报：一周里只用过十几次的设备被排到 10 名
    /// 开外，报告照出、TOP 10 那张表照少，两个数各看各的都"像对的"。周报要真跑（读库、写文件、
    /// 按自然周取窗口）无头造不出来，所以钉的是同一份源码里的先后次序。
    #[test]
    fn the_weekly_report_sums_devices_before_it_cuts_the_list() {
        let src = include_str!("main.rs");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod: String = src[..cut]
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let sum_at = prod
            .find("let device_total: i64 = devices.iter().map(|(_, v)| *v).sum()")
            .expect("设备那一句「合计」的求和不见了（改名或换了写法，这条守卫的锚要跟着改）");
        let trunc_at = prod
            .find("devices.truncate(10)")
            .expect("TOP 10 那一刀不见了（它没了这条判据就没意义，要一起改）");
        assert!(
            sum_at < trunc_at,
            "求和必须排在截断之前：合计={sum_at} 截断={trunc_at} —— \
             先切 10 行再求和，那句「设备输入合计」就只念前 10 名"
        );
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

    /// 接线腿：按 Esc 关面板必须调用与「返回」那颗钮同一份清理，不许自己手抄一半。
    ///
    /// 三条腿各钉一件事 —— ①这一支确实过 `reset_plugin_view_state`（按调用形态比，
    /// 不是数函数名出现几次）；②生产段里那种"手抄三个字段"的写法不许再出现，因为它正是
    /// 漏掉 `plugin_sel` 的形状；③两份清理不能只有一份存在（返回那颗钮那一支也得在）。
    #[test]
    fn esc_and_the_back_button_share_one_plugin_view_cleanup() {
        let src = include_str!("main.rs").replace("\r\n", "\n");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let flat: String = src[..cut]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .flat_map(|l| l.chars())
            .filter(|c| !c.is_whitespace())
            .collect();
        assert_eq!(
            flat.matches("plugin_open=None;").count(),
            5,
            "关面板的入口数目变了：返回 + Esc + 视图没了时点击 + 重扫发现插件没了\
             （四个都既置 None 又清状态）；另有两处互斥入口各只做一半：打开详情只置 None、\
             打开视图只清状态，所以这一行与下一行同为 5"
        );
        assert_eq!(
            flat.matches("reset_plugin_view_state()").count(),
            5,
            "清理要么被手抄成几份、要么少了一处：实得 {}",
            flat.matches("reset_plugin_view_state()").count()
        );
        assert_eq!(
            flat.matches("app.pv_scroll=0;app.pv_select=None;app.pv_modal=None;")
                .count(),
            0,
            "按 Esc 那一支又回到手抄三个字段：这样会留下 plugin_sel 里的陈旧 id"
        );
    }

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

    /// `POp::Table` 上那句"与 `rows` 平行的记录 id"是契约，而点击命中按 `ids.len()` 排行
    /// 矩形 ⇒ 视图只画前 `PV_TABLE_MAX_ROWS` 行、ids 却整份交回时，**画不出来的行仍有点击区**：
    /// 表格下面那片空白、以及排在它后面的那颗按钮，会被读成"选了第 N 行"——写回一个用户
    /// 从没看见的 id（`onselect` 还会让插件按它刷新），同时把那次点击吞掉，按钮不再响应。
    #[test]
    fn a_table_with_more_rows_than_drawn_has_no_invisible_hit_rows() {
        // 夹具必须**超过**上限，否则这条只剩"ids 与 rows 一样长"，截断那一半没人守。
        let over = PV_TABLE_MAX_ROWS + 3;
        let view = focusflow_core::plugins::PluginView {
            title: "夹具".to_string(),
            widgets: vec![focusflow_core::plugins::Widget::Table {
                headers: vec!["名".to_string()],
                rows: (0..over).map(|i| vec![format!("行{i}")]).collect(),
                ids: (0..over).map(|i| format!("id{i}")).collect(),
                actions: vec![],
                group: "g".to_string(),
                onselect: "picked".to_string(),
            }],
        };
        let ops = plugin_view_ops(&view, rect(0.0, 0.0, 600.0, 400.0), 0.0, &HashMap::new());
        let Some(POp::Table {
            rect: r, rows, ids, ..
        }) = ops.first()
        else {
            panic!("夹具那张表没走到 POp::Table，实得 {} 项", ops.len());
        };
        assert_eq!(
            rows.len(),
            PV_TABLE_MAX_ROWS,
            "超过上限的行该被截掉（夹具本来就超过上限）"
        );
        assert_eq!(
            ids.len(),
            rows.len(),
            "ids 与画出来的行分家：多出来那些行没有像素，却有点击区"
        );
        // 截断不许悄悄发生：表下面要有一句说明总行数与画出来的行数。
        let Some(POp::Label { text, .. }) = ops.get(1) else {
            panic!("表被截断了却没有那一句说明（实得 {} 项）", ops.len());
        };
        assert!(
            text.contains(&over.to_string()) && text.contains(&PV_TABLE_MAX_ROWS.to_string()),
            "说明里要同时有总行数 {over} 与画出来的 {PV_TABLE_MAX_ROWS}：{text}"
        );
        assert_eq!(ops.len(), 2, "截断时只多这一句，不该多出别的控件");
        for i in 0..ids.len() {
            let row = pv_table_row_rect(r, i);
            assert!(
                row.bottom <= r.bottom + 0.001,
                "第 {i} 行的命中矩形掉到表格外面：{} > {}",
                row.bottom,
                r.bottom
            );
        }
    }

    /// 行内操作按钮的列宽要跟着表格宽走。插件给几个长标签时，从前自然宽没有上限：
    /// 数据列被挤到 40 px 一格、按钮本身还会从卡片左边伸出去。现在整列最多占 55%，
    /// 数据列至少留 45%，每颗按钮都落在表格矩形内。
    #[test]
    fn long_row_action_labels_cannot_eat_the_data_columns() {
        let long = "这是一个非常长的操作名称".to_string();
        let view = focusflow_core::plugins::PluginView {
            title: "夹具".to_string(),
            widgets: vec![focusflow_core::plugins::Widget::Table {
                headers: vec!["名".to_string(), "值".to_string()],
                rows: vec![vec!["a".to_string(), "1".to_string()]],
                ids: vec!["r0".to_string()],
                actions: vec![
                    ("run:".to_string(), long.clone()),
                    ("del:".to_string(), long.clone()),
                    ("more:".to_string(), long.clone()),
                ],
                group: String::new(),
                onselect: String::new(),
            }],
        };
        let ops = plugin_view_ops(&view, rect(0.0, 0.0, 600.0, 400.0), 0.0, &HashMap::new());
        let Some(POp::Table {
            rect: r,
            buttons,
            column_w,
            ..
        }) = ops.first()
        else {
            panic!("夹具那张表没走到 POp::Table，实得 {} 项", ops.len());
        };
        assert_eq!(buttons.len(), 3);
        let table_w = r.right - r.left;
        assert!(
            *column_w >= table_w * 0.44,
            "数据列被按钮列挤掉了：{column_w} / {table_w}"
        );
        for (br, _, _) in buttons {
            assert!(
                br.left >= r.left - 0.001 && br.right <= r.right + 0.001,
                "行内按钮伸到表格外面：{br:?}（表 {r:?}）"
            );
        }
        // 短标签不受影响：自然宽一个像素都不变。
        assert_eq!(
            pv_row_action_widths(&["跑".to_string(), "编辑".to_string()], 600.0),
            vec![44.0, 44.0]
        );
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

    /// 拖动贴图必须 1:1 跟着光标走，一步一步都不许落后。
    ///
    /// 这一条是量出来的：按下时如果记的是**客户区**坐标，之后又拿客户区坐标去减它，
    /// 那这个基准会跟着窗口自己走 —— 每挪一次窗口就换一次坐标系，窗口于是在"该在的位置"
    /// 与"按下时那一格"之间来回跳（屏上就是抖/闪）。
    /// 这里按操作系统的规矩把每条 WM_MOUSEMOVE 的 lParam 算成"那一刻光标在当前客户区里的
    /// 坐标"（窗口每步都会挪，所以这个值不是累计位移），再要求窗口落在 origin + 累计位移上。
    ///
    /// 会真的在桌面上摆出贴图窗口、还会挪真实光标，所以默认不跑：
    /// `cargo test -p focusflow-native --bins -- --ignored dragging_a_pin`
    #[test]
    #[ignore = "会真的在桌面上摆出贴图窗口并挪动真实光标"]
    fn dragging_a_pin_follows_the_cursor_one_to_one() {
        unsafe {
            let _serial = probe_guard();
            open_test_pins(1);
            pump(400);
            let hwnd = PINS
                .lock()
                .ok()
                .and_then(|v| v.first().map(|p| p.hwnd))
                .map(|h| HWND(h as *mut core::ffi::c_void))
                .expect("贴图没开出来");
            let mut rc = RECT::default();
            let _ = GetWindowRect(hwnd, &mut rc);
            let origin = (rc.left, rc.top);
            let mut home = POINT::default();
            let _ = GetCursorPos(&mut home);
            let press = (origin.0 + 10, origin.1 + 10);
            let _ = SetCursorPos(press.0, press.1);
            pump(150);
            post(hwnd, WM_LBUTTONDOWN, 0, lp(10, 10));
            pump(120);
            assert!(
                PIN_DRAG.lock().map(|d| d.is_some()).unwrap_or(false),
                "按下之后没进拖动状态：这条判据连控制点都没立起来"
            );

            let steps = 12;
            for i in 1..=steps {
                let target = (press.0 + 10 * i, press.1 + 6 * i);
                let _ = SetCursorPos(target.0, target.1);
                let mut cur = RECT::default();
                let _ = GetWindowRect(hwnd, &mut cur);
                // 操作系统算 lParam 的口径：光标这一刻在窗口客户区里的坐标。
                post(
                    hwnd,
                    WM_MOUSEMOVE,
                    0,
                    lp(target.0 - cur.left, target.1 - cur.top),
                );
                pump(60);
                let _ = GetWindowRect(hwnd, &mut rc);
                assert_eq!(
                    (rc.left, rc.top),
                    (origin.0 + 10 * i, origin.1 + 6 * i),
                    "第 {i} 步没跟上：该在 origin + ({}, {})，实际 ({}, {})",
                    10 * i,
                    6 * i,
                    rc.left - origin.0,
                    rc.top - origin.1
                );
            }

            post(hwnd, WM_LBUTTONUP, 0, lp(0, 0));
            pump(120);
            assert_eq!(close_all_pins(), 1, "收尾时该只有这一张");
            let _ = SetCursorPos(home.x, home.y);
        }
    }
}

#[cfg(test)]
mod pager_shape_tests {
    use super::*;
    use focusflow_core::plugins::{PluginView, Widget};

    fn pager_ops(page: i64, pages: i64, prev: &str, next: &str) -> Vec<POp> {
        let view = PluginView {
            title: "shape".to_string(),
            widgets: vec![Widget::Pager {
                page,
                pages,
                total: 12,
                prev_id: prev.to_string(),
                next_id: next.to_string(),
            }],
        };
        plugin_view_ops(&view, rect(0.0, 0.0, 800.0, 600.0), 0.0, &HashMap::new())
    }

    fn pager_buttons(ops: &[POp]) -> Vec<(&str, bool, D2D_RECT_F)> {
        ops.iter()
            .filter_map(|op| match op {
                POp::Button {
                    id, disabled, rect, ..
                } => Some((id.as_str(), *disabled, *rect)),
                _ => None,
            })
            .collect()
    }

    /// 分页条必须是「一句读数 + 两颗真按钮」：`prev` / `next` 是插件声明的动作 id，
    /// 从前整条被折成一条惰性标签（协议里的两个 id 被直接丢掉），记账本于是翻不了页。
    #[test]
    fn the_pager_bar_turns_the_declared_ids_into_clickable_buttons() {
        let ops = pager_ops(2, 3, "pg_prev", "pg_next");
        assert_eq!(ops.len(), 3, "一条读数 + 两颗按钮，实得 {} 条", ops.len());
        match &ops[0] {
            POp::Label { text, .. } => {
                assert!(text.contains("2 / 3"), "页码要在这一行里：{text}");
                assert!(text.contains("12"), "总条数要在这一行里：{text}");
            }
            _ => panic!("分页条第一格该是读数（POp::Label），不是别的控件"),
        }
        let buttons = pager_buttons(&ops);
        let ids: Vec<&str> = buttons.iter().map(|(id, ..)| *id).collect();
        assert_eq!(ids, ["pg_prev", "pg_next"], "按钮要照插件声明的 id 发动作");
        assert!(
            buttons.iter().all(|(_, disabled, _)| !disabled),
            "第 2 / 3 页两头都能翻，不该有禁用"
        );
        // 读数与按钮同一行，按钮从左往右排在读数右边（不是各占一行）。
        let label_top = match &ops[0] {
            POp::Label { rect, .. } => rect.top,
            _ => unreachable!(),
        };
        assert!(
            buttons
                .iter()
                .all(|(_, _, r)| (r.top - label_top).abs() < 12.0),
            "按钮与读数不在同一行：{:?}",
            buttons.iter().map(|(_, _, r)| r.top).collect::<Vec<_>>()
        );
        assert!(
            buttons[0].2.left < buttons[1].2.left,
            "上一页该在下一页左边"
        );
    }

    /// 首页的「上一页」、末页的「下一页」画成禁用（点了不发动作）；
    /// 插件没给那一侧 id 时干脆不画 —— 三档不许折成同一颗可点的按钮。
    #[test]
    fn the_pager_buttons_go_inert_at_the_bounds_and_absent_when_unnamed() {
        let first_ops = pager_ops(1, 3, "pg_prev", "pg_next");
        let first = pager_buttons(&first_ops);
        assert_eq!(first.len(), 2);
        assert!(first[0].1, "首页的「上一页」要禁用");
        assert!(!first[1].1, "首页的「下一页」该能点");

        let last_ops = pager_ops(3, 3, "pg_prev", "pg_next");
        let last = pager_buttons(&last_ops);
        assert!(!last[0].1, "末页的「上一页」该能点");
        assert!(last[1].1, "末页的「下一页」要禁用");

        let next_only_ops = pager_ops(2, 3, "", "pg_next");
        let only_next = pager_buttons(&next_only_ops);
        assert_eq!(only_next.len(), 1, "没给 prev 的那一侧不画按钮");
        assert_eq!(only_next[0].0, "pg_next");
    }

    /// `row` 是横排：子控件从左往右排在同一行；放不下时换到下一行（而不是压在一起）。
    ///
    /// 这一条对应的是屏上那一片「每个按钮各占一行」——布局不再把 `row` 当纵向堆。
    #[test]
    fn a_row_puts_its_children_side_by_side_and_wraps_when_the_line_is_full() {
        let child = |text: &str, id: &str| Widget::Button {
            id: id.to_string(),
            text: text.to_string(),
            disabled: false,
            modal: None,
            sel: false,
            group: String::new(),
        };
        let view = PluginView {
            title: "row".to_string(),
            widgets: vec![Widget::Row {
                children: vec![
                    child("甲", "a"),
                    child("乙", "b"),
                    Widget::Select {
                        field: "f".to_string(),
                        label: "分类".to_string(),
                        value: "全部".to_string(),
                        options: vec![],
                        refresh: false,
                    },
                ],
            }],
        };
        let wide = plugin_view_ops(&view, rect(0.0, 0.0, 1000.0, 600.0), 0.0, &HashMap::new());
        let tops: Vec<f32> = wide.iter().map(|op| op.bounds().top).collect();
        let lefts: Vec<f32> = wide.iter().map(|op| op.bounds().left).collect();
        assert_eq!(wide.len(), 3, "三个子控件都该排出来");
        assert!(
            tops.iter().all(|t| (t - tops[0]).abs() < 0.01),
            "同一行必须同一高度：{tops:?}"
        );
        assert!(
            lefts[0] < lefts[1] && lefts[1] < lefts[2],
            "从左往右排：{lefts:?}"
        );
        // 输入框那一格要占住自己那 406 px（标签 140 + 值框 260），否则会压到隔壁身上。
        let field_w = wide[2].bounds().right - wide[2].bounds().left;
        assert!(
            (field_w - PV_FIELD_W).abs() < 0.01,
            "下拉那一格宽度是 {field_w}，应是 {PV_FIELD_W}"
        );

        // 窄窗：第三格放不下就换行，前两格仍同一行。
        let narrow = plugin_view_ops(&view, rect(0.0, 0.0, 300.0, 600.0), 0.0, &HashMap::new());
        assert_eq!(narrow.len(), 3);
        let n_tops: Vec<f32> = narrow.iter().map(|op| op.bounds().top).collect();
        assert!(
            (n_tops[0] - n_tops[1]).abs() < 0.01 && n_tops[2] > n_tops[1] + 1.0,
            "窄窗里第三格该换行：{n_tops:?}"
        );
    }

    /// 内置那两张长表（Edge 近 30 天、番茄历史 20 条）都必须整张画得出来：上限比它们小
    /// 的话，表里永远只有前几行，而标题说的天数/条数照旧 —— 这是一处会"看着没问题"的少数据，
    /// 只有把随包 Lua 里那个数读出来对一下才盯得住。
    #[test]
    fn the_table_row_cap_covers_the_shipped_long_tables() {
        let call_arg = |src: &str, call: &str| -> usize {
            let at = src
                .find(call)
                .unwrap_or_else(|| panic!("{call} 那一处找不到了：这条判据失去锚点"));
            src[at + call.len()..]
                .split(')')
                .next()
                .and_then(|n| n.trim().parse().ok())
                .unwrap_or_else(|| panic!("{call} 后面那个数读不出来"))
        };
        let edge = include_str!("../../crates/core/plugins/edge_history_plugin.lua");
        let pomo = include_str!("../../crates/core/plugins/pomodoro_plugin.lua");
        for (who, rows) in [
            ("Edge 趋势", call_arg(edge, "focusflow.edge_counts(")),
            ("番茄历史", call_arg(pomo, "focusflow.pomodoro_sessions(")),
        ] {
            assert!(
                PV_TABLE_MAX_ROWS >= rows,
                "表格最多画 {PV_TABLE_MAX_ROWS} 行，而{who}要画 {rows} 行：后面那些永远看不到"
            );
            // 行为腿：那一整张表真的排出来，且没有多出"被截断"那句。
            let view = PluginView {
                title: who.to_string(),
                widgets: vec![Widget::Table {
                    headers: vec!["一".to_string()],
                    rows: (0..rows).map(|i| vec![format!("{i}")]).collect(),
                    ids: (0..rows).map(|i| format!("{i}")).collect(),
                    actions: vec![],
                    group: String::new(),
                    onselect: String::new(),
                }],
            };
            let ops = plugin_view_ops(&view, rect(0.0, 0.0, 800.0, 600.0), 0.0, &HashMap::new());
            let Some(POp::Table {
                rows: drawn, ids, ..
            }) = ops.first()
            else {
                panic!("{who} 的表格没排出来");
            };
            assert_eq!(drawn.len(), rows, "{who} 那一整张表该都画出来");
            assert_eq!(ids.len(), drawn.len(), "{who} 的 ids 与行要一起截");
            assert_eq!(ops.len(), 1, "{who} 没超上限，不该多出截断说明");
        }
    }

    /// 表格行数上限不许小于随包记账本一页的条数：上限更小时每页后几条永远不会画出来，
    /// 而分页条的 `total` 照旧把它们算进去 —— 翻到哪一页都看不全。
    #[test]
    fn the_table_row_cap_does_not_hide_a_page_of_the_ledger() {
        let lua = include_str!("../../crates/core/plugins/accounting_plugin.lua");
        let page_size = lua
            .lines()
            .find_map(|line| line.trim().strip_prefix("local PAGE_SIZE = "))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .expect("记账本里那行 `local PAGE_SIZE = N` 找不到了：这条判据失去锚点");
        assert!(
            PV_TABLE_MAX_ROWS >= page_size,
            "表格最多画 {PV_TABLE_MAX_ROWS} 行，而记账本每页 {page_size} 条：每页有 {} 条永远看不到",
            page_size.saturating_sub(PV_TABLE_MAX_ROWS)
        );
    }

    /// 命中按 `ops` 的顺序逐条问；表格那一支从前对任何没落在行/行内按钮上的点击都直接
    /// `return`，于是**排在表格后面**的分页条（记账本就是这条顺序）永远收不到点击：
    /// 屏上画着「下一页」，点下去连插件动作都不发。这里把那条顺序原样排出来，
    /// 点分页条中心时必须由那一颗按钮认领，而不是被表格吞掉。
    #[test]
    fn a_pager_below_a_table_owns_its_clicks() {
        let view = PluginView {
            title: "分页".to_string(),
            widgets: vec![
                Widget::Table {
                    headers: vec!["名".to_string()],
                    rows: (0..10).map(|i| vec![format!("行{i}")]).collect(),
                    ids: (0..10).map(|i| format!("id{i}")).collect(),
                    actions: vec![],
                    group: String::new(),
                    onselect: String::new(),
                },
                Widget::Pager {
                    page: 1,
                    pages: 2,
                    total: 10,
                    prev_id: "page_prev".to_string(),
                    next_id: "page_next".to_string(),
                },
            ],
        };
        let ops = plugin_view_ops(&view, rect(0.0, 0.0, 800.0, 600.0), 0.0, &HashMap::new());
        let next = ops
            .iter()
            .find_map(|op| match op {
                POp::Button { id, rect, .. } if id == "page_next" => Some(*rect),
                _ => None,
            })
            .expect("分页条的「下一页」没排出来：这条判据失去锚点");
        let (cx, cy) = (
            (next.left + next.right) / 2.0,
            (next.top + next.bottom) / 2.0,
        );
        match pv_claiming_op(&ops, cx, cy) {
            Some(POp::Button { id, .. }) => assert_eq!(
                id, "page_next",
                "分页条那一格被另一颗按钮认领了：点下去发的是别的动作"
            ),
            Some(_) => panic!("分页条的点击被非按钮控件认领了：表格吞了它（旧缺陷）"),
            None => panic!("分页条的点击没人认领：按钮画得出来却点不到"),
        }

        // 表格自己的行仍归表格：点击第一行时认领的必须是表格本身（选中/联动靠它）。
        let Some(POp::Table { rect: tr, .. }) = ops.first() else {
            panic!("夹具第一项该是表格");
        };
        let row = pv_table_row_rect(tr, 0);
        let claimed = pv_claiming_op(&ops, row.left + 4.0, row.top + 4.0)
            .unwrap_or_else(|| panic!("表格行上的点击没人认领"));
        assert!(
            std::ptr::eq(claimed, &ops[0]),
            "表格行上的点击被后面那一条认领了：顺序反了"
        );
        // 表头也在表格矩形里：归表格，不许穿透到下面。
        assert!(
            pv_op_claims_click(&ops[0], tr.left + 4.0, tr.top + 4.0),
            "表头那一带的点击没被表格认领（会穿透到下面的控件）"
        );
        // 表格矩形下沿之外（分页条那一带）不归表格。
        assert!(
            !pv_op_claims_click(&ops[0], cx, cy),
            "表格把分页条那一带的点击也认领了：分页条又会点不动"
        );
    }

    /// 没有 field 的下拉写不回插件，按只读展示：不认领点击（与空 field 的输入框同一条）。
    #[test]
    fn a_select_without_a_field_is_display_only() {
        let view = |field: &str| PluginView {
            title: "下拉".to_string(),
            widgets: vec![Widget::Select {
                field: field.to_string(),
                label: "分类".to_string(),
                value: "食品".to_string(),
                options: vec![("食品".to_string(), "食品".to_string())],
                refresh: false,
            }],
        };
        let ops = plugin_view_ops(
            &view(""),
            rect(0.0, 0.0, 800.0, 600.0),
            0.0,
            &HashMap::new(),
        );
        let Some(sel @ POp::Select { rect: r, .. }) = ops.first() else {
            panic!("下拉没排出来：这条判据失去锚点");
        };
        let vb = select_value_rect(*r);
        let (cx, cy) = ((vb.left + vb.right) / 2.0, (vb.top + vb.bottom) / 2.0);
        assert!(
            !pv_op_claims_click(sel, cx, cy),
            "没有 field 的下拉不该认领点击：选完写不回插件"
        );

        let ops = plugin_view_ops(
            &view("f_cat"),
            rect(0.0, 0.0, 800.0, 600.0),
            0.0,
            &HashMap::new(),
        );
        assert!(
            pv_op_claims_click(&ops[0], cx, cy),
            "有 field 的下拉要照常能点"
        );
    }

    /// 滚到一半的表格仍在自己的原坐标上认领点击，而绘制已经把它裁出可见区：
    /// 症状是"点标题那一条，选中了屏上看不见的一行"。
    #[test]
    fn a_control_scrolled_out_of_the_clip_does_not_claim_clicks() {
        let view = PluginView {
            title: "长表".to_string(),
            widgets: vec![Widget::Table {
                headers: vec!["名".to_string()],
                rows: (0..40).map(|i| vec![format!("行{i}")]).collect(),
                ids: (0..40).map(|i| format!("id{i}")).collect(),
                actions: vec![],
                group: String::new(),
                onselect: String::new(),
            }],
        };
        let card = rect(16.0, 208.0, 1200.0, 800.0);
        let area = rect(
            card.left,
            card.top + 40.0,
            card.right - card.left,
            card.bottom - card.top,
        );
        let ops = plugin_view_ops(&view, area, 42.0, &HashMap::new());
        let clip = plugin_content_clip(card);
        // 标题那一条：在卡片里，但在正文可见区外。
        let (hx, hy) = (card.left + 600.0, card.top + 20.0);
        assert!(!hit(clip, hx, hy), "前提：这一点该在正文可见区外");
        assert!(
            ops.iter().any(|op| pv_op_claims_click(op, hx, hy)),
            "夹具没造出「被裁掉了却还能认领」的形状，这条判据测不到东西"
        );
        assert!(
            pv_claiming_op_in(&ops, clip, hx, hy).is_none(),
            "看不见的控件认领了标题区的点击"
        );
        // 反面：可见区里那张表仍归它，别把整张表一起打死。
        assert!(
            pv_claiming_op_in(&ops, clip, card.left + 600.0, 400.0).is_some(),
            "可见区里的点击该有控件认领"
        );
    }

    /// 刷新把内容换短之后，旧的滚动偏移必须当场夹回来：不夹的话整段内容都在可见区
    /// 上面，屏上只剩一张空卡片，要等用户滚一下滚轮才恢复。
    #[test]
    fn a_stale_scroll_offset_is_clamped_after_the_content_shrinks() {
        let card = rect(16.0, 208.0, 1200.0, 800.0);
        let area = plugin_scroll_area(1200.0, 800.0);
        let visible = plugin_scroll_visible(800.0);
        let long = PluginView {
            title: "长表".to_string(),
            widgets: vec![Widget::Table {
                headers: vec!["名".to_string()],
                rows: (0..40).map(|i| vec![format!("行{i}")]).collect(),
                ids: (0..40).map(|i| format!("id{i}")).collect(),
                actions: vec![],
                group: String::new(),
                onselect: String::new(),
            }],
        };
        let short = PluginView {
            title: "短了".to_string(),
            widgets: vec![Widget::Label("只有一行".to_string())],
        };
        let content_long = plugin_content_height(&long, area);
        let content_short = plugin_content_height(&short, area);
        assert!(content_long > visible, "夹具前提：长内容要能滚起来");
        let scrolled = clamp_plugin_scroll(200, content_long, visible);
        assert_eq!(scrolled, 200, "内容没变短时偏移要留着");
        let clamped = clamp_plugin_scroll(scrolled, content_short, visible);
        assert_eq!(clamped, 0, "内容变短之后旧偏移必须夹回可见范围");
        // 行为腿：旧偏移下短内容整段在可见区上方（空卡片），夹回来之后才重新出现。
        let clip = plugin_content_clip(card);
        let stale = plugin_view_ops(&short, area, scrolled as f32, &HashMap::new());
        assert!(
            stale
                .iter()
                .all(|op| !hit(clip, op.bounds().left + 4.0, op.bounds().top + 4.0)),
            "旧偏移下不该有内容留在可见区里（夹具要造出空卡片）"
        );
        let fixed = plugin_view_ops(&short, area, clamped as f32, &HashMap::new());
        assert!(
            fixed
                .iter()
                .any(|op| hit(clip, op.bounds().left + 4.0, op.bounds().top + 4.0)),
            "夹回来之后内容要重新出现在可见区里"
        );
    }

    /// 滚动文本区：高度按上限留、绘制也只能画这么多行。从前高度按 5 行算，绘制把
    /// 全部行画出去，第 6 行起直接叠在下面的控件上；整段一起夹宽还会把换行后面
    /// 的内容当成"同一行的尾巴"夹掉。
    #[test]
    fn a_long_text_area_is_capped_to_the_height_it_reserves() {
        let content = (1..=9)
            .map(|i| format!("第{i}行"))
            .collect::<Vec<_>>()
            .join("\n");
        let view = PluginView {
            title: "文本".to_string(),
            widgets: vec![Widget::TextArea(content.clone())],
        };
        let ops = plugin_view_ops(&view, rect(0.0, 0.0, 800.0, 600.0), 0.0, &HashMap::new());
        let Some(POp::Body { rect: r, text }) = ops.first() else {
            panic!("文本区没排出来：这条判据失去锚点");
        };
        let rows = capped_lines(&content, PV_TEXTAREA_MAX_LINES);
        assert_eq!(rows.len(), PV_TEXTAREA_MAX_LINES, "超上限要有提示行顶替");
        assert_eq!(rows.last().unwrap(), "…（还有 5 行没显示）");
        assert!(
            (r.bottom - r.top - PV_TEXTAREA_MAX_LINES as f32 * 18.0).abs() < 0.01,
            "留高与行数上限不是同一个常量：{}",
            r.bottom - r.top
        );
        let _ = text;
    }

    /// 行内 EDIT 没有自己的边框，铺满整格会把宿主画的主色描边连圆角一起盖掉
    /// （浅色主题下白底压白底，那一格看着像"点一下就没了"）。EDIT 必须缩在值框里面，
    /// 四个边各留出至少 1 px 让那圈边框留在外面。
    #[test]
    fn the_inline_edit_leaves_the_drawn_field_border_visible() {
        let view = PluginView {
            title: "字段".to_string(),
            widgets: vec![Widget::TextInput {
                field: "f_kw".to_string(),
                label: "关键词".to_string(),
                value: String::new(),
            }],
        };
        let ops = plugin_view_ops(&view, rect(0.0, 0.0, 800.0, 600.0), 0.0, &HashMap::new());
        let Some(POp::Field {
            rect: field_rect, ..
        }) = ops.first()
        else {
            panic!("夹具那个输入框没排出来");
        };
        let drawn = pv_field_value_rect(*field_rect);
        let inset = PV_FIELD_EDIT_INSET;
        let edit = rect(
            drawn.left + inset,
            drawn.top + inset,
            (drawn.right - drawn.left) - inset * 2.0,
            (drawn.bottom - drawn.top) - inset * 2.0,
        );
        assert!(
            edit.left >= drawn.left + 1.0
                && edit.top >= drawn.top + 1.0
                && edit.right <= drawn.right - 1.0
                && edit.bottom <= drawn.bottom - 1.0,
            "EDIT 盖住了宿主画的边框：drawn={drawn:?} edit={edit:?}"
        );
        assert!(
            (edit.right - edit.left) > 100.0 && (edit.bottom - edit.top) > 10.0,
            "缩过头了，EDIT 里放不下字：{edit:?}"
        );
        // 点值框中心仍要能开编辑（缩边之后命中矩形没跟着搬走）。
        assert!(
            pv_op_claims_click(
                &ops[0],
                (drawn.left + drawn.right) / 2.0,
                (drawn.top + drawn.bottom) / 2.0
            ),
            "值框中心的点击没被输入框认领"
        );
    }
}

/// 日期选择器的纯逻辑：网格起点、月份步进、当前值解析。
#[cfg(test)]
mod date_pick_tests {
    use super::*;

    /// 日历网格按周一开头；2 月/闰年与上下月的步进都在这里钉住。
    #[test]
    fn the_calendar_grid_starts_on_monday_and_handles_leap_years() {
        // 2024-01-01 是周一、2024-02-01 是周四（周一记 0）。
        assert_eq!(dp_first_weekday(2024, 1), 0);
        assert_eq!(dp_first_weekday(2024, 2), 3);
        assert_eq!(dp_days_in_month(2026, 2), 28);
        assert_eq!(dp_days_in_month(2028, 2), 29);
        assert_eq!(dp_days_in_month(2026, 12), 31);
    }

    #[test]
    fn month_steps_cross_the_year_boundary_both_ways() {
        assert_eq!(dp_month_add(2026, 1, -1), (2025, 12));
        assert_eq!(dp_month_add(2026, 12, 1), (2027, 1));
        assert_eq!(dp_month_add(2026, 3, -14), (2025, 1));
    }

    /// 字段里的年份是外部输入：翻月时整年乘 12 不许溢出（release 是 abort，
    /// debug 是 panic），越界的年份打开时退回今天。
    #[test]
    fn a_typed_out_of_range_year_cannot_overflow_the_month_stepper() {
        assert_eq!(dp_month_add(i32::MAX, 12, 1), (9999, 1));
        assert_eq!(dp_month_add(i32::MIN, 1, -1), (1, 12));
        let (ty, tm, _) = dp_today();
        assert_eq!(dp_parse("999999999-01-01").0, ty, "越界年份要退回今天");
        assert_eq!(dp_parse("999999999-01-01").1, tm);
        assert_eq!(dp_parse("0000-01-01").0, ty, "0 年不是合法年份");
    }

    /// 打开时落在字段当前那个月；认不出的值退回今天，不能把选择器开在"2026 年 13 月"。
    #[test]
    fn the_current_value_decides_which_month_opens() {
        assert_eq!(dp_parse("2026-8-6"), (2026, 8, Some((2026, 8, 6))));
        assert_eq!(dp_parse("2026-02-31"), (2026, 2, None));
        let (ty, tm, _) = dp_today();
        assert_eq!(dp_parse("2026-13-01").0, ty);
        assert_eq!(dp_parse("2026-13-01").1, tm);
        assert_eq!(dp_parse("").1, tm);
    }
}

/// 热键状态行那条读数。截图的两条键就在这三条里，"按了没反应"时用户要先知道是哪一条没了。
#[cfg(test)]
mod hotkey_line_tests {
    use super::{hotkey_status_line, status_is_warning, topbar_status_line, win_buttons};

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
        assert!(line.contains("四条热键都没挂上"), "{line}");
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
        // 插件页那几条失败：与"没存盘"同属"要人处理"，而它们只在 status 里有一份
        assert!(status_is_warning("安装失败：不是 zip"));
        assert!(status_is_warning("卸载失败：目录被占用"));
        assert!(status_is_warning(
            "插件动作失败：attempt to index a nil value"
        ));
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

    /// 插件页的回执在顶栏：安装/卸载/动作失败都只写 `status`，而那一页的正文
    /// 没有画它的位置。设置页自己有卡片底部那一行 ⇒ 不重复；其余页没有这类回执。
    #[test]
    fn the_top_bar_carries_the_plugin_pages_receipts_only() {
        let w = 1200.0;
        assert!(
            topbar_status_line(5, Some("安装失败：不是 zip"), w).is_none(),
            "设置页已有卡片底部那一行，顶栏不再重复"
        );
        assert!(
            topbar_status_line(4, None, w).is_none(),
            "没有回执时顶栏那一格不许占"
        );
        assert!(
            topbar_status_line(4, Some("   "), w).is_none(),
            "全空白的回执不算话"
        );
        for view in [0usize, 1, 2, 3, 6] {
            assert!(topbar_status_line(view, Some("安装失败：不是 zip"), w).is_none());
        }
        let (line, warn, r) =
            topbar_status_line(4, Some("  安装失败：不是 zip  "), w).expect("插件页要看得见");
        assert_eq!(line, "安装失败：不是 zip", "两端空白要去掉");
        assert!(warn, "失败在顶栏也得是红的");
        assert!(
            r.right <= win_buttons(w)[0].left - 11.9,
            "顶栏那一格压到窗口按钮上：{r:?}"
        );
        assert!(r.left >= 16.0 + 250.0 - 0.01, "压到标题那两个字上：{r:?}");
        let (_, warn_ok, _) =
            topbar_status_line(4, Some("已安装：demo（2 个文件，128 字节）"), w).unwrap();
        assert!(!warn_ok, "成功回执不该用警告色");
        // 接线：画的那一头真的调了它（函数名单独出现不算 —— 定义与说明里都有）。
        // 拆开拼针，免得这条守卫命中测试文件自己的字面量。
        let needle = ["topbar_status_line(", "vs.view", ", status, w)"].concat();
        assert!(
            include_str!("main.rs").contains(&needle),
            "顶栏回执没接在 draw_screen 上：插件页的回执还是看不到"
        );
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

    /// 次数那一格画的是 `u64` 千分位：坏数据/外部导入留下的负数直接 `as u64` 会在托盘上
    /// 念出一个二十位数（与卡片、排行同一族；那几处早已夹 0）。
    #[test]
    fn a_negative_today_count_does_not_become_a_twenty_digit_tooltip() {
        let tip = super::tray_tooltip(-5);
        assert!(
            tip.starts_with("FocusFlow · 今日 0"),
            "负数不许折成天文数字: {tip}"
        );
    }

    #[test]
    fn the_same_copied_text_is_posted_once_per_second() {
        let t0 = std::time::Instant::now();
        let last = Some(("h".to_string(), t0));
        assert!(
            super::clip_event_is_duplicate(
                last.as_ref(),
                "h",
                t0 + std::time::Duration::from_millis(200)
            ),
            "同一段文本在一秒内的重复发布不算新事件"
        );
        assert!(
            !super::clip_event_is_duplicate(
                last.as_ref(),
                "h",
                t0 + std::time::Duration::from_millis(1500)
            ),
            "过了窗口再复制同一段应当算新事件"
        );
        assert!(
            !super::clip_event_is_duplicate(last.as_ref(), "other", t0),
            "不同内容永远算新事件"
        );
        assert!(!super::clip_event_is_duplicate(None, "h", t0));
    }

    #[test]
    fn clipboard_pause_label_is_hh_mm_and_zero_means_none() {
        assert_eq!(super::clipboard_pause_label(0), None);
        assert_eq!(super::clipboard_pause_label(-1), None, "负数按没暂停处理");
        let label = super::clipboard_pause_label(1_700_000_000_000).expect("有效时间戳应有标签");
        assert_eq!(label.len(), 5, "HH:MM，实际是 {label}");
        assert_eq!(&label[2..3], ":");
    }

    #[test]
    fn tooltip_carries_the_clipboard_pause_deadline() {
        let plain = super::tray_tooltip_with(3, false, false, false, 0);
        assert_eq!(plain, "FocusFlow · 今日 3");
        let paused = super::tray_tooltip_with(3, true, false, false, 1_700_000_000_000);
        assert!(
            paused.contains("剪贴板暂停至 "),
            "暂停截止时刻要进托盘提示: {paused}"
        );
        assert!(paused.contains("采集中"), "提示前两截不能被挤掉: {paused}");
    }
}

#[cfg(test)]
mod always_on_top_tests {
    /// 主窗口不置顶、悬浮窗与贴图仍置顶 —— 两半各钉一条。
    ///
    /// 主窗口那句 `SetWindowPos(hwnd, Some(HWND_TOPMOST), …)` 是原型期为了让自动化截图
    /// 不被浏览器盖住留下的，代价是用户切到别的窗口时这块界面永远压在上面（他 2026-10-30
    /// 点掉的）。而"悬浮窗压在任务栏上面"是另一条产品口径，不能跟着一起删。
    #[test]
    fn the_main_window_is_normal_and_the_card_still_floats_on_top() {
        let src = include_str!("main.rs");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod = &src[..cut];
        assert!(
            !prod.contains("Some(HWND_TOPMOST)"),
            "主窗口又被置顶了：切到别的窗口时它会一直压在人家上面"
        );
        let pins = prod
            .find("WS_EX_TOPMOST | WS_EX_TOOLWINDOW")
            .expect("贴图窗的置顶样式不见了");
        assert!(pins > 0, "贴图窗仍要置顶");
        let floating = include_str!("floating.rs");
        let fcut = floating
            .find("#[cfg(test)]")
            .expect("悬浮窗的测试模块找不到了");
        assert!(
            floating[..fcut].contains("wm::WS_EX_TOPMOST.0"),
            "悬浮窗不再置顶：任务栏会把它盖住"
        );
    }
}

#[cfg(test)]
mod hide_to_tray_tests {
    /// 收进托盘必须连渲染目标一起放掉。
    ///
    /// 依据是现量（`.scratch/cpu_arms.ps1` 的 A7 臂）：面板被画过一次之后，隐藏 60 秒仍是
    /// `私有 57.7 MB / 57 线程`，与"正显示着"那一读数一字不差；接上这一步之后同一臂是
    /// `45.1 MB / 23 线程`。也就是说这两张 BGRA 背板与整条图形栈会全天陪着进程，只因为
    /// 用户开过一次面板。
    ///
    /// 判据钉**调用形态**而不是函数名：两条隐藏出口都得出现 `hide_to_tray(app, hwnd)`；
    /// 而 `fix_backdrop` 那种"藏一下马上显示"的抖动补丁不许跟着学（它一藏一显在同一消息里，
    /// 照做等于把图形栈白拆一次）。
    #[test]
    fn every_tray_hide_releases_the_render_target() {
        let src = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs"));
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod: String = src[..cut].chars().filter(|c| !c.is_whitespace()).collect();
        let call = "hide_to_tray(app,hwnd)";
        let n = prod.matches(call).count();
        assert_eq!(
            n, 2,
            "Esc 与 WM_CLOSE 两条收进托盘的路径都要放掉渲染目标，实际 {n} 处"
        );
        // 函数体必须**收口**再比：本仓另有两处 `drop_target()`（掉设备重建、
        // `WM_DPICHANGED`），从定义处往后切一段无界文本的话，摘掉隐藏那一步的
        // `drop_target()` 照样绿 —— 2026-10-31 变异跑就是这么把这条假守卫抓出来的。
        let raw_at = src
            .find("fn hide_to_tray(app: &mut App, hwnd: HWND)")
            .expect("hide_to_tray 的定义不见了");
        let body = &src[raw_at..raw_at + src[raw_at..].find("\n}").expect("函数体没收尾")];
        assert!(
            body.contains("app.drop_target()"),
            "hide_to_tray 只剩个名字，不再放掉渲染目标了"
        );
        let bd_at = prod.find("fnfix_backdrop(").expect("fix_backdrop 不见了");
        let bd: String = prod[bd_at..].chars().take(1200).collect();
        assert!(
            !bd.contains(call),
            "fix_backdrop 不该放掉渲染目标：它只是闪一下就要还原回去"
        );
    }
}

#[cfg(test)]
mod startup_tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// 按键分发的接线形状：生产端得有调用方，而且**不许**站在每键热路径上。
    ///
    /// 逐键进 Lua 是这次刻意躲开的代价（插件钩子死循环炸过一次启动），所以这条钉的是位置：
    /// 分发只在 GUI 那一拍的取件里；`listener.rs` 的按键路径一行都不许沾它；写线程那一侧
    /// 只许有一个"出锁交件"的点。摘掉任一处，这条要红 —— 病状是"功能没坏、数不涨了"。
    #[test]
    fn the_key_dispatch_sits_on_the_batch_side_not_the_per_key_path() {
        let src = include_str!("main.rs").replace("\r\n", "\n");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod: String = src[..cut]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            prod.matches("pm.plugin_key_event(").count(),
            1,
            "攒批分发的调用点不是一份（两份就会重复投）"
        );
        // 钉**调用形态**：光找 pump_plugin_key_batches 会先撞上那句 `fn pump_...` 的定义，
        // 摘掉心跳里的调用照样绿（本场自己踩到的一次假绿，探针验出来的）。
        assert_eq!(
            prod.matches("app.pump_plugin_key_batches();").count(),
            1,
            "心跳里那一拍取件的调用不见了：件口没人收，键鼠栏照旧是 0"
        );
        assert!(
            prod.contains("take_key_batches()"),
            "GUI 线程取件那一跳不见了：件口没人收，键鼠栏照旧是 0"
        );
        assert!(
            prod.contains("load_plugins_at_startup()"),
            "启动即加载退回了进插件页才建管理器"
        );
        let listener = include_str!("../../crates/core/src/listener.rs").replace("\r\n", "\n");
        let lcut = listener.find("#[cfg(test)]").unwrap_or(listener.len());
        assert!(
            !listener[..lcut].contains("plugin_key_event"),
            "每键热路径上又出现了一次 Lua 分发"
        );
        let writer = include_str!("../../crates/core/src/db/writer.rs").replace("\r\n", "\n");
        let wcut = writer.find("#[cfg(test)]").unwrap_or(writer.len());
        assert_eq!(
            writer[..wcut].matches("queue_key_batch(").count(),
            1,
            "出锁交件的调用点该只有一份"
        );
        assert!(
            writer[..wcut].contains("merged_key_deltas()"),
            "按键增量没按键名合并就交了出去"
        );
    }

    /// 静默启动的两条入口必须各自成立，且**参数赢**：带 `--hidden` 的那一次不管配置写什么都静默
    /// （开机自启要的就是这个），配置那条是持久版。
    #[test]
    fn either_entry_hides_the_window_and_the_argument_wins() {
        assert!(
            hidden_at_startup(&args(&["--hidden"]), false),
            "参数那条没接上"
        );
        assert!(hidden_at_startup(&args(&[]), true), "配置那条没接上");
        assert!(
            hidden_at_startup(&args(&["--hidden"]), true),
            "两条都开更该静默"
        );
        assert!(
            !hidden_at_startup(&args(&[]), false),
            "两条都没开还静默：新装用户第一次打开就找不到窗口"
        );
        // argv[0] 是 exe 自己，不算参数（调用方在那里 skip(1)，这一条盯的是别把路径当成参数）
        assert!(!hidden_at_startup(&args(&["C:\\x\\FocusFlow.exe"]), false));
        // 只认这一个写法：`--hidden=true` / `-/hidden` 都不算，别给自己造第二条口径
        assert!(!hidden_at_startup(&args(&["--hidden=true"]), false));
    }

    /// 开机自启写进 `.lnk` 的那个参数，必须与原生端解析的是同一个字符串。
    ///
    /// 这条是本次缺陷的正身：`.lnk` 一直带 `--hidden`，而全仓没有解析点 —— 两边各写一份、
    /// 谁都没核对过，所以"开机静默进托盘"这个意图今天没有任何一条路能达成。抄错的形状就是
    /// 症状本身，所以钉的是字面量相等，不是"两边都出现过 hidden"。
    #[test]
    fn the_autostart_shortcut_and_the_parser_use_one_spelling() {
        let src = include_str!("../../crates/core/src/autostart.rs").replace("\r\n", "\n");
        let at = src
            .find("$s.Arguments = '")
            .expect("开机自启那条快捷方式的参数不见了");
        let rest = &src[at + "$s.Arguments = '".len()..];
        let written = &rest[..rest.find('\'').expect("参数没有收尾引号")];
        assert_eq!(
            written, HIDDEN_ARG,
            "开机自启带的参数与原生端解析的那一个不是同一个字符串"
        );
    }

    /// 默认值必须是关。接线之前 `true` 是个死值（屏上那行点了存了什么都不发生），接上之后
    /// 再留 `true` 就等于让每个新装用户第一次打开只看得见托盘图标。
    #[test]
    fn a_new_install_shows_the_window() {
        let cfg = include_str!("../../crates/core/src/config.rs").replace("\r\n", "\n");
        assert!(
            cfg.contains(r#"("start_to_tray", "false")"#),
            "默认值又翻回 true 了：新装用户第一次打开只看得见托盘"
        );
        assert!(
            !cfg.contains(r#"("start_to_tray", "true")"#),
            "默认表里出现了第二份 start_to_tray"
        );
        // 读点也得在（只改默认值不改读点，就是这次缺陷的另一半）
        let prod = {
            let src = include_str!("main.rs").replace("\r\n", "\n");
            let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
            src[..cut]
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(
            prod.matches("get_bool(\"gui\", \"start_to_tray\"").count(),
            1,
            "配置键的读点不见了或抄了两份"
        );
        assert_eq!(
            prod.matches("hidden_at_startup(").count(),
            2,
            "一份定义 + 一份调用；调用不见了就是又成了死键"
        );
        assert!(
            prod.contains("WS_OVERLAPPEDWINDOW | WS_VISIBLE"),
            "唤窗那条路整个不见了"
        );
        assert!(
            prod.contains("if hidden {"),
            "静默那条分支不见了：参数与配置都白读"
        );
    }
}

#[cfg(test)]
mod table_layout_tests {
    use super::*;

    /// 数字列（次数 / 占比）必须停在下一列的左沿之前，整张表也不许越过卡片右边距。
    ///
    /// 这块 UI 不按矩形裁切（`NO_WRAP`、不设 `DWRITE_TRIMMING`），列一旦比盒子宽，数字就
    /// 直接画到下一列上 —— 设备那一页「596,600」被「还原」按钮盖住、屏上只剩「596,6」，
    /// 就是他 2026-10-30 报的「直接被挡住了」。列宽改了却没人看这条时，症状只会是屏上那
    /// 几个数缺一半，日志一个字都没有。
    #[test]
    fn the_numeric_columns_stop_before_the_next_one() {
        for w in [1000.0f32, 1200.0, 1600.0] {
            let card = left_card(w, 860.0);
            let g = table_geom(card);
            let restore = restore_button(&g, 0.0);
            assert!(
                g.x_count_right_ops < restore.left,
                "w={w}: 次数列右沿 {} 顶到了「还原」{}",
                g.x_count_right_ops,
                restore.left
            );
            assert!(
                g.x_count_right < g.x_last,
                "w={w}: 次数列右沿 {} 顶到了占比列 {}",
                g.x_count_right,
                g.x_last
            );
            assert!(
                g.x_share_left + SHARE_COL_W <= card.right - g.pad,
                "w={w}: 占比列右沿 {} 越过了卡片右边距",
                g.x_share_left + SHARE_COL_W
            );
            // 名字列先被挤扁：窗口窄到它装不下一个设备名时，该改的是窗口下限而不是继续缩。
            assert!(
                device_name_width(&g) >= 100.0,
                "w={w}: 设备名那一列只剩 {} px",
                device_name_width(&g)
            );
            assert!(
                name_col_width(&g) >= 100.0,
                "w={w}: 名字列只剩 {} px",
                name_col_width(&g)
            );
        }
    }

    /// 胶囊组与它左边那句读数都不许越出左卡。这块 UI 不按矩形裁切（`NO_WRAP`），越出去的字
    /// 照画到右边那张卡上，点还点不中。
    ///
    /// 改前原点写死 `card.left + 322` ⇒ 最后一颗右沿 = `card.left + 682`，装得下要 w ≥ 约 1177：
    /// 1000px 超 108px、960px 超 132px ⇒ 这条对 w<1177 必红，就是它的区分性。1200px（默认宽度）
    /// 改前改后只差 0.4px（698 → 697.6），默认宽度的验收坐标不动。
    #[test]
    fn the_pill_group_and_its_summary_stop_at_the_card_edge() {
        for w in [960.0f32, 1000.0, 1100.0, 1177.0, 1200.0, 1400.0] {
            let card = left_card(w, 860.0);
            let pills = rank_pills(card);
            let x0 = rank_pills_x0(card);
            assert!(
                pills[4].right <= card.right - 8.0,
                "w={w}: 胶囊组右沿 {} 越过了卡片右沿减 8（{}）",
                pills[4].right,
                card.right - 8.0
            );
            let sb = rank_summary_box(card, table_geom(card).pad);
            assert!(
                sb.right <= x0,
                "w={w}: 摘要框右沿 {} 顶到了胶囊组原点 {x0}",
                sb.right
            );
            assert!(
                x0 - sb.right >= SUMMARY_PILL_GAP - 0.001,
                "w={w}: 读数与胶囊贴着脸（缝 {} 小于 {SUMMARY_PILL_GAP}）",
                x0 - sb.right
            );
            // 预算与这一格同宽：按预算换算出来的像素不许超过格子，再多一个字就该装不下。
            let box_w = sb.right - sb.left;
            let budget = char_budget_at(box_w, CAPTION_PT);
            assert!(
                budget as f32 * CAPTION_PT <= box_w + 0.001,
                "w={w}: 预算 {budget} 个字按 {CAPTION_PT}px 要 {} px，格子只有 {box_w} px",
                budget as f32 * CAPTION_PT
            );
            assert!(
                (budget + 1) as f32 * CAPTION_PT > box_w,
                "w={w}: 预算保守到白少念一个字"
            );
        }
    }

    /// 组宽度由胶囊表导出，原点只许有一份表达式。
    ///
    /// 钉的是**调用形态**而不是函数名：以前 `rank_pills` 与 `rank_pill_captions` 各写一遍
    /// `card.left + 322`，改一处忘一处 ⇒ 点中的是胶囊、画的是另一处。有人再手抄一份时这条要红。
    #[test]
    fn the_pill_group_origin_and_width_have_one_source_each() {
        assert_eq!(PILL_DX.len(), 5);
        assert_eq!(
            rank_pill_group_w(),
            PILL_DX[4] + PILL_W,
            "组宽度该由最后一颗的右沿导出，别写第二个常数"
        );
        let card = left_card(1200.0, 860.0);
        assert!(
            (rank_pills(card)[4].right - (rank_pills_x0(card) + rank_pill_group_w())).abs() < 0.001,
            "最后一颗右沿与 x0 + 组宽度不是同一个数"
        );
        assert!(
            (rank_pill_captions(card).0.left - rank_pills_x0(card)).abs() < 0.001,
            "两句小字的原点不是胶囊组那一份"
        );
        let src = include_str!("main.rs").replace("\r\n", "\n");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod: String = src[..cut]
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            prod.matches("fn rank_pills_x0(").count(),
            1,
            "倒推原点的表达式被人抄了第二份"
        );
        for f in [
            "fn rank_pills(",
            "fn rank_pill_captions(",
            "fn rank_summary_box(",
        ] {
            let at = prod.find(f).unwrap_or_else(|| panic!("{f} 不见了"));
            let body: String = prod[at..].chars().take(320).collect();
            assert!(
                body.contains("rank_pills_x0(card)"),
                "{f} 不再读那份倒推的原点：
{body}"
            );
        }
        assert!(
            !prod.contains("card.left + 322"),
            "又出现写死的胶囊原点：窄窗口下它会顶出卡片"
        );
    }

    /// 窄窗口下那句长读数**当场被夹**，而且夹完还在格子内；默认宽度下短句子一个字不动。
    ///
    /// 只把格子收窄不等于屏上不再顶出去 —— 判据要看见那串字确实变短。这条同时钉住字号取的
    /// 是 `caption` 那一档：借隔壁那档时，换算出来的字数与屏上像素不是一把尺子。
    #[test]
    fn a_long_summary_is_clipped_inside_the_narrow_box() {
        let card = left_card(960.0, 860.0);
        let sb = rank_summary_box(card, table_geom(card).pad);
        let box_w = sb.right - sb.left;
        let long = "共 1,478,603 次";
        assert!(
            long.chars().count() > char_budget_at(box_w, CAPTION_PT),
            "夹具前提：这句在窄窗口下本来就该长到装不下（格子 {box_w}px）"
        );
        let clipped = clip_middle(long, char_budget_at(box_w, CAPTION_PT));
        assert_ne!(clipped, long, "窄窗口下这句一个字都没被夹 = 夹取没接上");
        assert!(
            clipped.chars().count() <= char_budget_at(box_w, CAPTION_PT),
            "夹完还在预算外：{clipped}"
        );
        assert!(clipped.contains('…'), "中间那段要标成省略：{clipped}");
        // 默认宽度那一格：短句子不许被这条夹取误伤。
        let wide = rank_summary_box(
            left_card(1200.0, 860.0),
            table_geom(left_card(1200.0, 860.0)).pad,
        );
        assert_eq!(
            clip_middle(long, char_budget_at(wide.right - wide.left, CAPTION_PT)),
            long,
            "默认宽度下不该动字"
        );
    }
}

#[cfg(test)]
mod share_base_tests {
    use super::*;

    /// 生产文本（整行注释与 `///` 一并滤掉：这条守卫曾被"把调用整块注释掉"假绿过一次，
    /// 注释里那句话与针脚可以逐字相同）。
    fn prod() -> String {
        let src = include_str!("main.rs");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        src[..cut]
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 夹具：榜里这一轮读回来 100 次（字母 70 + 鼠标 30），另有 50 次还没落库；
    /// 设备那一趟是侧信道，只读回来 40 次；应用那趟 50 秒。
    fn snap() -> data::Snapshot {
        data::Snapshot {
            period: -1,
            period_total: 150,
            rank: vec![("a".to_string(), 70i64), ("b".to_string(), 30i64)],
            group: vec![
                ("字母键".to_string(), 70i64),
                ("鼠标点击".to_string(), 30i64),
            ],
            mouse_total: 30,
            keyboard_total: 70,
            device_total: 40,
            devices: vec![data::DevRow {
                key: "k1".to_string(),
                name: "鼠标".to_string(),
                auto_name: "鼠标".to_string(),
                has_alias: false,
                kind: "mouse".to_string(),
                count: 40,
            }],
            app_total: 50,
            apps: vec![("x".to_string(), 25i64)],
            ..Default::default()
        }
    }

    /// 纯算式那一条：给什么数除什么数，分母没答案就念 `0.0%`。
    #[test]
    fn share_text_divides_by_what_it_is_handed() {
        assert_eq!(share_text(30, 60), "50.0%");
        assert_eq!(share_text(1, 3), "33.3%");
        assert_eq!(
            share_text(5, 0),
            "0.0%",
            "一个数都没有，就没有「占了百分之几」这回事"
        );
        assert_eq!(share_text(5, -2), "0.0%", "分母是坏值也不许除出负占比");
        assert_eq!(share_text(-5, 100), "0.0%", "分子是历史坏行不许念成负占比");
    }

    /// 导出里每一栏的「占比」按**自己那一趟**除：键鼠那栏按榜的总数，设备那栏按设备那趟。
    ///
    /// 旧的形状是六处各写各的除法，其中四处（含这两栏）拿的是卡片那个含未落库增量的
    /// `period_total`：同一份 CSV 里「键鼠排行」按 150 除到 46.7%，而那一栏自己加起来
    /// 根本到不了 100%；设备那栏更绕，分子是 Raw Input 侧信道、分母是主链路总数，
    /// 除出来的是两把尺子的商。
    #[test]
    fn each_exported_column_divides_by_its_own_read() {
        let s = snap();
        let csv = export_csv_text(&s);
        let mut wrong: Vec<String> = Vec::new();
        for want in [
            "1,a,70,70.0%\r\n",
            "2,b,30,30.0%\r\n",
            "1,鼠标,mouse,40,100.0%\r\n",
        ] {
            if !csv.contains(want) {
                wrong.push(format!(
                    "导出里该有这一行（占比按自己那一趟的总数除）：{want}"
                ));
            }
        }
        // 反向：按卡片那个含增量的总数（150）除出来的三个数，一个字都不许再出现
        for never in ["46.7%", "20.0%", "26.7%"] {
            if csv.contains(never) {
                wrong.push(format!("导出里又出现按卡片总数除出来的 {never}"));
            }
        }
        assert!(wrong.is_empty(), "导出占比判据红:\n{}", wrong.join("\n"));

        let html = export_html_text(&s);
        assert!(
            html.contains("<td class=\"num\">70.0%</td>"),
            "HTML 那一份的键鼠占比也要按榜的总数除，实得:\n{html}"
        );
    }

    /// 接线腿：屏上那几处占比各自点名分母，谁都不许再碰卡片那个含增量的数。
    ///
    /// `draw_table` 与热力板那几处要 D2D 上下文，无头跑不了，所以钉的是调用形态；
    /// 真正会被算错的那几个数由上面那条导出用例端到端盯着。
    #[test]
    fn the_share_columns_each_name_their_own_base() {
        let prod = prod();
        let mut wrong: Vec<String> = Vec::new();
        for (needle, want, what) in [
            (
                "share_text(*n, snap.rank_share_base())",
                2,
                "键名榜与「分组」那一屏",
            ),
            (
                "share_text(*v, s.rank_share_base())",
                2,
                "两份导出的键鼠那栏",
            ),
            ("share_text(c, total)", 1, "键盘热力板悬停那一格"),
            ("share_text(d.count, s.device_total)", 1, "导出的设备那栏"),
            ("share_text(*secs, snap.app_total)", 1, "应用那一屏的占比"),
        ] {
            let got = prod.matches(needle).count();
            if got != want {
                wrong.push(format!(
                    "{what}的调用形态实得 {got} 处、应为 {want}：{needle}"
                ));
            }
        }
        if prod.contains(["period_total", " as f32"].concat().as_str()) {
            wrong.push("卡片那个含未落库增量的总数又回到除法里了".to_string());
        }
        let n = prod.matches(["100", ".0 /"].concat().as_str()).count();
        if n != 1 {
            wrong.push(format!(
                "「占比」的算式只许在 share_text 里写一次，实得 {n} 处"
            ));
        }
        assert!(wrong.is_empty(), "占比接线判据红:\n{}", wrong.join("\n"));
    }

    /// 卡片下面那一行念的是键名**种数**：榜只为面板列行截到显示上限，那一格的字不许跟着
    /// 截断走。旧写法用 `snap.rank.len()`，键名一多就永远念成上限那个数，
    /// 而 CLI 同一件事念的是未截断的种数 —— 两个屏对着骂架。
    #[test]
    fn the_card_sub_line_reads_the_kind_count_not_the_list_length() {
        let mut s = snap();
        s.rank = (0..data::RANK_DISPLAY_MAX)
            .map(|i| (format!("键 {i}"), 1i64))
            .collect();
        s.rank_kinds = data::RANK_DISPLAY_MAX + 37;
        let line = kinds_sub(&s);
        assert!(line.contains("137"), "种数没被念出来，实得一句「{line}」");
        assert!(
            !line.contains("100 个键位"),
            "那一格念的还是榜的行数（显示上限）：{line}"
        );
        // 没到上限时两种读法本来就该一致，这条不许被一起改坏
        let mut small = snap();
        small.rank_kinds = small.rank.len();
        assert_eq!(kinds_sub(&small), "本周期 2 个键位");
    }

    /// 三份导出必须互相核对得上：拿 JSON 交回的数重算占比，要得到与 CSV 同一个字符串。
    ///
    /// CSV 与 HTML 那两份由应用自己算好每栏的占比（分母各是 `rank_total` / `app_total` /
    /// `device_total`，都不是含未落库增量的 `period_total`）。JSON 过去只交出 `period_total`
    /// ⇒ 谁拿 JSON 重算，算出来的一定与同一次点击导出的那两份不一样：同一个问题两个答案，
    /// 三份文件并排放着还看不出谁对。
    #[test]
    fn the_json_export_hands_back_the_same_bases_the_csv_used() {
        let s = snap();
        let doc: serde_json::Value =
            serde_json::from_str(&export_json_text(&s)).expect("JSON 导出该是个能解析的对象");
        let mut wrong: Vec<String> = Vec::new();
        for (field, want) in [
            ("rank_total", 100i64),
            ("app_total", 50i64),
            ("device_total", 40i64),
        ] {
            let got = doc[field].as_i64();
            if got != Some(want) {
                wrong.push(format!(
                    "JSON 里 {field} 该是 {want}（与 CSV 用的同一个分母），实得 {got:?}"
                ));
            }
        }
        let csv = export_csv_text(&s);
        let csv_row = csv
            .lines()
            .find(|l| l.starts_with("1,a,70,"))
            .unwrap_or_default();
        let from_json = share_text(
            doc["rank"][0]["count"].as_i64().unwrap_or(-1),
            doc["rank_total"].as_i64().unwrap_or(-1),
        );
        assert!(
            csv_row.ends_with(from_json.as_str()),
            "同一行、两份文件：JSON 重算得 {from_json}，CSV 那一行却是「{csv_row}」"
        );
        assert!(wrong.is_empty(), "JSON 分母判据红:\n{}", wrong.join("\n"));
    }
}

#[cfg(test)]
mod exit_flush_tests {
    /// 生产文本：第一个 `#[cfg(test)]` 之前，剥掉整行注释，并把 CRLF 先归一。
    /// 归一这步不是洁癖 —— 这台机器 `core.autocrlf=true`，一份刚检出的工作树是 CRLF，
    /// 跨行针脚在干净检出上会红在"起点找不到了"那种与真相无关的话上（本仓踩过两次）。
    fn prod() -> String {
        let src = include_str!("main.rs").replace("\r\n", "\n");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        src[..cut]
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 接线腿：GUI 退出这条路上必须显式收一次日志，而且要排在停采集**之后**。
    ///
    /// 摘掉这一句的症状不在屏上、在排查现场：日志走 non_blocking 通道，`WorkerGuard`
    /// 躺在 static 里不会自己析构，于是「数据库已关闭」「退出备份成/没成」这几条
    /// 每次退出都留在缓冲区里没了 —— 而查崩溃与"退不干净"靠的正是日志的尾巴。
    /// CLI 那条路早就显式收了，原生端当时漏下。
    #[test]
    fn quitting_the_gui_flushes_the_log_after_the_writer_stops() {
        let prod = prod();
        let fn_at = prod
            .find("fn quit_app(hwnd: HWND)")
            .expect("quit_app 不见了（改名或换了写法，这条守卫的锚要跟着改）");
        let rest = &prod[fn_at..];
        let end = rest.find("\n}\n").expect("quit_app 的函数体切不出来");
        let body = &rest[..end];
        let need = ["focusflow_core::logger", "::shutdown();"].concat();
        assert_eq!(
            body.matches(need.as_str()).count(),
            1,
            "退出这条路上要且只有一处显式收日志"
        );
        let collect_at = body
            .find("collect::shutdown(")
            .expect("停采集那一步不见了（它没了这条次序判据就没意义）");
        let logger_at = body.find(need.as_str()).expect("收日志那一步不见了");
        assert!(
            logger_at > collect_at,
            "收日志必须排在停采集之后，否则「数据库已关闭」那几条根本没写进来：\
             collect={collect_at} logger={logger_at}"
        );
    }
}

#[cfg(test)]
mod export_ruler_tests {
    use super::*;

    /// 接线腿：导出这一支的早退必须与旁边的早退分支做同样两件事。
    ///
    /// 快照那份锁读不动时（只有别的线程正持着它 panic 过才会这样）这一支从前是一条光秃秃的
    /// `return`：既不往 `ops` 里留一句回执，也不投 `WM_OP_DONE`，而界面的 `busy` 只有那条
    /// 消息会清 ⇒ 三颗「执行」从此永远灰着，非重启不活。判据比的是**这一支里两件事都在**
    /// 的形状（`ops.lock()` 与 `WM_OP_DONE` 都要在这个 else 块里出现），不是数函数名。
    #[test]
    fn the_export_arm_that_gives_up_still_reports_done() {
        let src = include_str!("main.rs").replace("\r\n", "\n");
        let at = src
            .find("let Some(s) = snap_data else")
            .expect("导出那一支的快照早退不见了");
        let block: String = src[at..].chars().take(900).collect();
        let end = block.find("};").expect("这一支的 else 块没收尾");
        let body = &block[..end];
        assert!(
            body.contains("ops.lock()") && body.contains("WM_OP_DONE"),
            "这一支早退时既不留回执也不投 done，`busy` 会闩到重启：{body}"
        );
    }

    /// 键名/应用名/设备名各挑一个会咬人的形状。
    fn evil_snap() -> data::Snapshot {
        data::Snapshot {
            period: -1,
            period_total: 10,
            rank: vec![("-2+3".to_string(), 6i64), ("=1+1".to_string(), 4i64)],
            apps: vec![("@evil.exe".to_string(), 3661i64)],
            devices: vec![data::DevRow {
                key: "k".to_string(),
                name: "A&B<x>'q".to_string(),
                auto_name: "A&B<x>'q".to_string(),
                has_alias: false,
                kind: "mouse".to_string(),
                count: 10,
            }],
            ..Default::default()
        }
    }

    /// 屏上「导出 CSV」那颗按钮写的必须是 core 那一份转义：本地那份抄漏了公式前缀
    /// 防护，而减号键的名字就以 `-` 开头（core 的注释里写明了这一点），Excel 会求值。
    #[test]
    fn the_native_csv_export_blocks_formula_injection() {
        let csv = export_csv_text(&evil_snap());
        assert!(csv.contains("'-2+3"), "以 - 开头的键名要被挡住:\n{csv}");
        assert!(csv.contains("'=1+1"), "以 = 开头的键名要被挡住:\n{csv}");
        assert!(
            csv.contains("'@evil.exe"),
            "以 @ 开头的文件名要被挡住:\n{csv}"
        );
        // 紧凑时长也只剩一份实现（悬浮窗那一列同一个形状）
        assert!(csv.contains("1h 1m"), "3661 秒该念成紧凑式:\n{csv}");
    }

    /// HTML 导出同理：core 那一份连单引号一起转，本地那份不转。
    #[test]
    fn the_native_html_export_escapes_every_context() {
        let html = export_html_text(&evil_snap());
        assert!(
            html.contains("A&amp;B&lt;x&gt;&#39;q"),
            "设备名里的 & < > 与单引号都得转:\n{html}"
        );
        assert!(!html.contains("<x>"), "不许把名字当标记解析:\n{html}");
    }

    /// 这三条规则在原生侧都不许再长出第二份手抄。
    #[test]
    fn the_export_rules_have_one_owner() {
        let src = include_str!("main.rs");
        let cut = src.find("#[cfg(test)]").expect("找不到测试模块的起点了");
        let prod: String = src[..cut]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let flat: String = prod.chars().filter(|c| !c.is_whitespace()).collect();
        for banned in ["fncsv_field(", "fnhtml_escape(", "fnduration_text("] {
            assert!(!flat.contains(banned), "原生侧又长出第二份实现：{banned}");
        }
        assert!(
            flat.contains("usecrate::floating_render::fmt_durationasduration_text;"),
            "紧凑时长不再从悬浮窗那一份借用实现"
        );
        assert!(
            flat.contains("usefocusflow_core::format::{csv_field,html_escape};"),
            "CSV/HTML 转义不再从 core 那一份借用实现"
        );
    }
}

#[cfg(test)]
mod weekday_label_tests {
    use super::*;

    /// 柱顶那个数字必须留在绘图区里：最高的那一根也得给它留出带头。
    ///
    /// 改前的形状：`hh = (v / max) * plot_height(plot)` ⇒ 最高的柱顶到 `plot.top`，数字画在
    /// `bar.top - 18` 就落到 `r.top + 36`，而那正是「本栏…没读全 · 数据源：…」那一行的格子
    /// （`p.text` 不带裁切 ⇒ 两句直接叠在一起，屏幕上是糊的）。
    #[test]
    fn the_tallest_weekday_bar_leaves_room_for_its_number() {
        // 现量的绘图区高：卡片 300 − 上边带 54 − 下边带 42
        let plot_h = 204.0;
        assert_eq!(
            weekday_bar_height(plot_h, plot_h, plot_h),
            plot_h - WEEKDAY_VALUE_HEADROOM,
            "最高的那一根必须把带头让出来"
        );
        let hh = weekday_bar_height(1200.0, 1200.0, plot_h);
        assert!(
            plot_h - hh - WEEKDAY_VALUE_HEADROOM >= 0.0,
            "数字的上沿跑到绘图区外面了"
        );
        assert!(
            weekday_bar_height(600.0, 1200.0, plot_h) < hh,
            "柱高该跟着数走"
        );
        assert_eq!(weekday_bar_height(0.0, 1200.0, plot_h), 0.0);
        assert_eq!(weekday_bar_height(5.0, 0.0, plot_h), 0.0, "max 为 0 不许除");
        assert_eq!(
            weekday_bar_height(5.0, 10.0, 18.0),
            0.0,
            "绘图区连带头都不够就不画柱"
        );
    }

    /// 接线守卫：画柱与画数字**两处**都问同一把尺子——少一处就是叠字复发的形状。
    #[test]
    fn the_weekday_bar_and_its_number_share_one_headroom() {
        let src = include_str!("main.rs");
        let cut = src.find("#[cfg(test)]").expect("找不到测试模块的起点了");
        let flat: String = src[..cut].chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            flat.contains("lethh=weekday_bar_height(v,max,plot_height(plot));"),
            "星期分布的柱高不再问这把尺子了"
        );
        assert!(
            flat.contains("rect(x,plot.bottom-hh-WEEKDAY_VALUE_HEADROOM,bw,16.0)"),
            "柱顶数字的偏移不再与带头同一份"
        );
    }
}

#[cfg(test)]
mod clip_tests {
    use super::*;

    fn dw() -> IDWriteFactory {
        unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).expect("DWrite 工厂") }
    }

    /// 现量那条症状：两个 114 / 91 字符的路径拼进底部那一格，句子画到窗口外面，
    /// 用户看到的是一句括号都没闭合的半句。夹过之后必须**两头都在**。
    ///
    /// 走的是产品那一道（`clip_to_cell` + 真的 `text12` 档），不是自己复刻一把尺子：
    /// 复刻的尺子测不到"接线的是这一道"。
    #[test]
    fn a_long_status_line_keeps_its_head_and_its_tail() {
        let home = r"E:mydataagentdatacodeFocusFlow.scratchab_longpathProgram Files (x86)FocusFlow 便携版数据目录 2026-10-29 迁移测试data";
        let target = r"E:mydataagentdatacodeFocusFlow.scratchab_longpath新位置 OneDrive 同步中 请等它跑完再关 2026-10-29";
        let line = format!("注意：没能搬到 {home} ({target}（搬家没成，仍记在旧目录）");
        assert!(
            line.chars().count() > 120,
            "夹具前提：这句本来就该长到顶出窗口"
        );

        let dw = dw();
        let f = build_formats(&dw).expect("Formats");
        // 1136 = 默认宽度那一格；这一句实测远超它，所以复核后仍按全角尺子夹
        let budget = char_budget_at(1136.0, unsafe { f.text12.GetFontSize() });
        let clipped = clip_to_cell(&dw, &line, &f.text12, 1136.0);
        let n = clipped.chars().count();
        assert!(n <= budget, "夹完必须落在预算内：{n} > {budget}");
        assert!(
            measure_text(&dw, &clipped, &f.text12).unwrap_or(f32::MAX) <= 1136.0,
            "夹完还是画到窗口外面：{clipped}"
        );
        assert!(
            clipped.starts_with("注意：没能搬到"),
            "句首要认得出在说什么事"
        );
        assert!(
            clipped.ends_with("仍记在旧目录）"),
            "句尾（要做的那一步）不许被砍掉：{clipped}"
        );
        assert!(clipped.contains('…'), "中间那段要标成省略，实得: {clipped}");
    }

    /// 右对齐与居中那两档要按**真实宽度**复核，不许把 DirectWrite 在 `maxWidth = ∞` 下交回来的
    /// 0 当成"装得下"。
    ///
    /// 现量的坑：同一句 21 个字用 `caption` 量是 181 DIP、用 `caption_right` 量是 **0**；
    /// 星期分布柱顶那档 `center_small` 同样是 0。少了 `measure_format` 那个左起副本，症状不是
    /// 报错，是这两族格子的夹取被整个摘掉 —— 目标条右边那句压到进度条上、邻柱的数字叠在一起。
    /// 这条同时钉两半：真装不下的必须夹（摘掉副本就红），确实装得下的不许夹（改过头就红）。
    #[test]
    fn the_right_aligned_and_centered_tiers_measure_their_real_width() {
        let dw = dw();
        let f = build_formats(&dw).expect("Formats");
        let over = "连续打卡 123 天 · 近一年最长 365 天";
        assert!(
            measure_text(&dw, over, &f.caption).unwrap_or(0.0) > 198.0,
            "夹具前提：这句在 198 DIP 这一格里真装不下"
        );
        assert!(
            measure_text(&dw, over, &f.caption_right).unwrap_or(1.0).abs() < 0.001,
            "右起档量回来的不再是 0 —— DirectWrite 这一族布局变了，`measure_format` 的左起副本该重新评估"
        );
        assert_ne!(
            clip_to_cell(&dw, over, &f.caption_right, 198.0),
            over,
            "右起档把 0 读成装得下：这句会压到进度条上"
        );
        let fits = "连续打卡 0 天 · 近一年最长 17 天";
        assert_eq!(
            clip_to_cell(&dw, fits, &f.caption_right, 198.0),
            fits,
            "右起档里真装得下的那句被白夹了"
        );
        assert_eq!(
            clip_to_cell(&dw, "9,618", &f.center_small, 40.0),
            "9,618",
            "居中档里真装得下的数字被白夹了"
        );
        assert_ne!(
            clip_to_cell(&dw, "12,345,678", &f.center_small, 40.0),
            "12,345,678",
            "居中档的长数字越出去会叠到邻柱"
        );
    }

    /// 短的那句一个字都不许动 —— 否则这条夹取本身就成了新的信息损失。
    #[test]
    fn a_line_that_fits_is_left_alone() {
        for s in ["备份完成：3 份", "已取消", ""] {
            assert_eq!(clip_middle(s, 90), s, "不该动的被动了: {s}");
        }
        // 恰好等于预算：不动；多一个字才动
        let exact = "字".repeat(90);
        assert_eq!(clip_middle(&exact, 90), exact);
        let over = "字".repeat(91);
        assert_ne!(clip_middle(&over, 90), over);
    }

    /// 屏上那三处省略号的现量症状：句子按全角尺子算"装不下"，DirectWrite 量出来却装得下。
    ///
    /// 每条都先钉住"旧尺子本来会把它夹掉"（夹具前提），再钉"实测复核之后一个字都没动"。
    /// 少了前一条，这条就退化成"函数没被改坏"的空话 —— 拿本来就装得下的句子来测，
    /// 摘掉复核照样绿。宽度用的是画它的那一格的真宽度。
    #[test]
    fn a_number_heavy_line_that_fits_is_not_clipped() {
        let f = build_formats(&dw()).expect("Formats");
        let dw = dw();
        for (text, fmt, rect_w) in [
            // 活跃分析页顶部：今日 N / M 那一格 150 DIP
            ("今日 9,618 / 20,000", &f.body, 150.0),
            // 同一格右边：连续打卡那句 198 DIP
            ("连续打卡 0 天 · 近一年最长 17 天", &f.caption_right, 198.0),
            // 趋势图横轴起点：库里交回的那一项去掉 `YYYY-`
            ("10-02", &f.caption, 60.0),
            // 图上那枚读数气泡（150 减左右各 6）
            ("2026-11-01 · 9,618 次", &f.small, 138.0),
            // 排行页摘要那一格（960px 窗口下实测只剩 ~100 DIP）
            ("共 1,478,603 次", &f.caption, 100.0),
        ] {
            // 尺子按**画这一格的那档字体自己**换算，与 clip_to_cell 同一份：
            // 借隔壁那档时这条前提量的就不是屏上那一格。
            let budget = char_budget_at(rect_w, unsafe { fmt.GetFontSize() });
            assert!(
                text.chars().count() > budget,
                "夹具前提：「{text}」按全角尺子在 {rect_w} DIP 这一格里本来就装不下\
                 （预算 {budget} 个字）——换成装得下的句子来测，摘掉实测复核这条照样绿"
            );
            assert_eq!(
                clip_to_cell(&dw, text, fmt, rect_w),
                text,
                "DirectWrite 量着装得下，却被全角尺子夹掉了：{text}"
            );
        }
    }

    /// 复核不许变成"永不夹取"：真装不下的句子仍要夹，且夹完量一次确实进了格子。
    #[test]
    fn a_line_that_really_overflows_is_still_clipped() {
        let dw = dw();
        let f = build_formats(&dw).expect("Formats");
        let long = "字".repeat(40);
        assert!(
            measure_text(&dw, &long, &f.body).unwrap() > 150.0,
            "夹具前提：这串全角字在 150 DIP 这一格里真装不下"
        );
        let clipped = clip_to_cell(&dw, &long, &f.body, 150.0);
        assert_ne!(
            clipped, long,
            "越界的那句一个字都没被夹 = 复核把夹取整个吃掉了"
        );
        assert!(
            measure_text(&dw, &clipped, &f.body).unwrap() <= 150.0,
            "夹完还是画到邻居身上：{clipped}"
        );
    }

    /// 格子算出负数时（窄窗口下 `r.right - r.left - 92.0` 真能到负）不许整串放行，
    /// 也不许回 0 个字：念一个省略号，比画到邻居身上和"这一格空着"都强。
    #[test]
    fn a_negative_box_still_says_something() {
        let dw = dw();
        let f = build_formats(&dw).expect("Formats");
        assert_eq!(
            clip_to_cell(&dw, &"字".repeat(9), &f.body, -40.0),
            "…",
            "极窄格子里的出口不是整串放行"
        );
    }

    /// 预算跟着格子走，并且永远给得出一个能画的数。
    ///
    /// 不写死"560 → 46"：那个数跟着说明行的字号走（`SETTINGS_HELP_PT`），字号一调大
    /// 就成了"改字号还要回来改用例"。这里钉的是关系 —— 预算个字**装得下**，再多一个
    /// 就装不下（两边各留一条，右边那条防的是"预算保守到白少念好几个字"）。
    #[test]
    fn the_budget_follows_the_box_and_never_collapses() {
        let budget = char_budget_at(560.0, SETTINGS_HELP_PT);
        assert!(
            budget as f32 * SETTINGS_HELP_PT <= 560.0,
            "预算 {budget} 个字按 {SETTINGS_HELP_PT} px 一行要 {} px，比格子还宽",
            budget as f32 * SETTINGS_HELP_PT
        );
        assert!(
            (budget + 1) as f32 * SETTINGS_HELP_PT > 560.0,
            "预算太保守：还能再念一个字"
        );
        assert!(
            char_budget_at(1136.0, SETTINGS_HELP_PT) > char_budget_at(560.0, SETTINGS_HELP_PT),
            "宽格子该念更多"
        );
        // 窗口拖到极窄（甚至矩形算出负数）时也不许回 0：那样这一格一个字都不念，
        // 用户连"有事发生"都看不见。
        assert_eq!(char_budget_at(0.0, SETTINGS_HELP_PT), 1);
        assert_eq!(char_budget_at(-40.0, SETTINGS_HELP_PT), 1);
    }

    /// 接线腿：两处画长文本的地方都必须过实测复核那一道夹取，且状态行那处的预算与它画进去的
    /// 矩形是**同一个宽度**（写死一个常数的话，窗口拖窄就又顶出去了）。
    #[test]
    fn the_two_long_text_rows_are_clipped_to_their_own_box() {
        let src = include_str!("main.rs").replace("\r\n", "\n");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod: String = src[..cut]
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        // 压平空白 + 去掉 rustfmt 在多行参数列表尾上留的那个逗号：
        // 按行比会把"修复还在、只是被排版拆开"误判成夹取不见了。
        let flat: String = prod
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .replace(",)", ")");
        for needle in [
            "p.clip(&dir_line,&p.fmt.text12,note_w)",
            "p.clip(line,status_fmt,status_w)",
        ] {
            assert_eq!(
                flat.matches(needle).count(),
                1,
                "这一处夹取不见了或被改写成两份：{needle}",
            );
        }
        // 状态行那格：夹取吃的宽度必须就是画它的那个矩形的宽度（写死常数就又不跟窗口走）
        let at = flat.find("letstatus_w=").expect("status_w 不见了");
        let win: String = flat[at..].chars().take(300).collect();
        assert!(
            win.contains("status_w,") && win.contains("p.clip(line,status_fmt,status_w)"),
            "预算与矩形不是同一个宽度了：
{win}",
        );
        assert!(
            win.contains("letstatus_fmt=ifwarn"),
            "状态行的夹取不再问画它那一档字体自己（加粗那档比常规宽）：
{win}"
        );
    }

    /// 又一批定宽格子接上同一道夹取：设备名、左表页脚那句、插件名 / 版本作者 / 第二行 /
    /// 头部目录 / 目录读不动那句、设置页的值框、设备详情弹窗那两行、图上悬停读数、目标条那句、
    /// 热键那两行说明、趋势图横轴起点、星期分布柱顶数字、目标条右边那句连续打卡。
    ///
    /// 这块 UI 的文本是 `NO_WRAP` 且不设 `DWRITE_TRIMMING` —— 越界不是"被截断"，是照画到
    /// 邻居身上（设备那一列窄窗口只有 ~200 px，而别名上限 24 个汉字、兜底名截到 40 字符，
    /// 两路都能越过去；插件那两格吃的是作者自己写的字符串）。每条针都同时钉"过的是实测复核
    /// 那一道"与"夹取与矩形是同一个宽度表达式"，因为分家正是这一族的复发形状。比的是压平空白之后的
    /// 文本：rustfmt 会把长参数列表折行，按行比会把"修复还在、只是被排版拆开"误判成夹取不见了。
    #[test]
    fn four_more_fixed_width_cells_clip_their_own_text() {
        let src = include_str!("main.rs").replace("\r\n", "\n");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod: String = src[..cut]
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        // rustfmt 会在多行参数列表末尾留一个逗号 —— 压平之后它会让 "f(a,b,)" 与 "f(a,b)"
        // 变成两种写法，这里统一掉，免得判据红在排版上。
        let flat: String = prod
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .replace(",)", ")");
        for (needle, want) in [
            // 设备名：夹取与画它的那一格都用 device_name_width(&g)
            ("letcell=rect(g.x_name,y+3.0,device_name_width(&g),18.0);letclipped=p.clip(&d.name,&p.fmt.table_name,device_name_width(&g));", 1),
            // 页脚那句：同一个 r.right-r.left-g.pad*2.0
            ("p.clip(&table_footer_text(snap,lane,tab),&p.fmt.caption,r.right-r.left-g.pad*2.0),rect(r.left+g.pad,foot_y,r.right-r.left-g.pad*2.0,16.0)", 1),
            // 插件名那一格是定宽 240，后面 10 px 就是"版本 · 作者"
            ("letname_cell=rect(r.left+8.0,y+5.0,240.0,18.0);letclipped=p.clip(&row.name,&p.fmt.body,240.0);", 1),
            // ⚠ 压平空白也会压掉**字符串字面量里面**的空格，所以这一针里的 `v{}·{}`
            // 与源码里的 `v{} · {}` 是同一个东西 —— 别照着源码补空格。
            ("p.clip(&format!(\"v{}·{}\",row.version,row.author),&p.fmt.caption,260.0),rect(r.left+250.0,y+7.0,260.0,16.0)", 1),
            // 插件第二行：那句话的分支收进了 `PluginRow::second_line`，画只剩一处，
            // 夹取与画它的矩形仍取同一个 desc_w。
            ("p.clip(&line,&p.fmt.caption,desc_w),rect(r.left+8.0,y+23.0,desc_w,16.0)", 1),
            ("&p.fmt.caption,desc_w)", 1),
            // 插件声明那一格（常驻 / API / 权限数 / 自刷 / 版本要求 / 占用）：
            // 夹取与画它的矩形取同一个 decl_w，而 decl_w 又是从那一格的右边界算下来的
            ("p.clip(&decl,&p.fmt.caption,decl_w),rect(r.left+520.0,y+7.0,decl_w,16.0)", 1),
            // 插件页顶上那句：正文由 `plugin_heading` 拼（含常驻个数与每格上限），
            // 夹取与画它的矩形仍取同一个 520。
            ("p.clip(&plugin_heading(rows,cap_mb,&dirs),&p.fmt.caption,heading_w),rect(card.left+58.0,card.top+15.0,heading_w,18.0)", 1),
            // 详情页的「来源」那一句：来源路径 + 校验和是外部字符串，自带同一格宽度。
            ("p.clip(source,&p.fmt.caption,inner_w),rect(x,card.top+88.0,inner_w,16.0)", 1),
            ("p.clip(&format!(\"插件目录读不出来：{e}\"),&p.fmt.caption,card.right-card.left-32.0)", 1),
            // 值框：夹取与画它的矩形共用 val_w（那一格还要给右边的按钮让位）
            ("p.clip(shown,&p.fmt.text13,val_w),rect(vb.left+8.0,vb.top,val_w,vb.bottom-vb.top)", 1),
            // 设备详情弹窗：名字与副行同一个 detail_w
            ("p.clip(&d.name,&p.fmt.body,detail_w),rect(x,dialog.top+40.0,detail_w,20.0)", 1),
            ("p.clip(&sub,&p.fmt.caption,detail_w),rect(x,dialog.top+62.0,detail_w,18.0)", 1),
            // 图上悬停读数：夹的是气泡里面那一条
            ("p.clip(text,&p.fmt.small,w-12.0),rect(x+6.0,y+1.0,w-12.0,16.0)", 1),
            // 目标条那句"今日 N / M"：150 之后 8 px 就是进度条
            ("p.clip(&format!(\"今日{}/{}\",thousands(todayasu64),thousands(goalasu64)),&p.fmt.body,150.0),rect(r.left+pad,r.top+14.0,150.0,18.0)", 1),
            // 插件面板一族：作者给的字符串，宽度全是插件自己挑的
            ("p.clip(text,&p.fmt.body,r.right-r.left),*r,&p.fmt.body,&p.br.fg", 1),
            // 单行的那一格（Label）仍就地夹；多行的 Body 走 `clip_text_lines`（逐行 + 行数上限），
            // 两处各有自己的针，别把 Body 的夹取悄悄丢了。
            ("p.clip(text,&p.fmt.caption,r.right-r.left),*r,&p.fmt.caption,&p.br.muted", 1),
            (
                "clip_text_lines(p,text,&p.fmt.caption,r.right-r.left,PV_TEXTAREA_MAX_LINES)",
                1,
            ),
            (
                "clip_text_lines(p,m.content,&p.fmt.caption,cr.right-cr.left,modal_content_cap(m,h))",
                1,
            ),
            ("p.clip(key,&p.fmt.caption,140.0),rect(r.left,r.top,140.0,18.0)", 1),
            ("p.clip(value,&p.fmt.small,r.right-r.left-146.0),rect(r.left+146.0,r.top,r.right-r.left-146.0,18.0)", 1),
            // Field 与 Select 的标签同一格同一定宽 ⇒ 两处
            ("p.clip(label,&p.fmt.caption,140.0),rect(r.left,r.top+4.0,140.0,18.0)", 2),
            ("p.clip(value,&p.fmt.caption,vb.right-vb.left-16.0)", 1),
            // Select 的值格：可编辑时右边让 28 px 给箭头，只读时只留 16 —— 夹取与画它的
            // 矩形共用同一个 value_w（从前两处都写死 28，只读那一档会白留一段）。
            (
                "p.clip(value,&p.fmt.caption,value_w),rect(vb.left+8.0,vb.top+3.0,value_w,18.0)",
                1,
            ),
            ("p.clip(htxt,&p.fmt.caption,cw-12.0),rect(r.left+6.0+iasf32*cw,r.top+4.0,cw-12.0,16.0)", 1),
            ("letcell_rect=rect(rr.left+6.0+ciasf32*cw,rr.top+2.0,cw-12.0,16.0);letclipped=p.clip(text,&p.fmt.small,cw-12.0);", 1),
            // 下拉菜单的每一项：选项文字是插件给的（分类名可以很长）
            ("p.clip(label,&p.fmt.caption,ir.right-ir.left-12.0),rect(ir.left+6.0,ir.top+3.0,ir.right-ir.left-12.0,18.0)", 1),
            ("p.clip(&f.label,&p.fmt.caption,136.0),rect(row.left,row.top+7.0,136.0,18.0)", 1),
            ("p.clip(&f.value,&p.fmt.caption,vb.right-vb.left-28.0),rect(vb.left+8.0,vb.top+3.0,vb.right-vb.left-28.0,18.0)", 1),
            ("p.clip(m.title,&p.fmt.body,g.dialog.right-g.dialog.left-72.0)", 1),
            // 基础行的说明列不再写死 560/640：那比按钮前的实际空位宽出一百多像素，
            // 长路径/长热键会压到右边那颗按钮上。三处都吃 `note_w`，宽度与矩形同一个数。
            ("&p.fmt.text12,640.0)", 0),
            ("&p.fmt.text12,note_w)", 4),
            // 键名/应用名是库里的原文，长度没有上限：与设备名同一把夹取尺子
            (
                "letcell=rect(g.x_name,y+3.0,name_col_width(&g),18.0);letclipped=p.clip(app,&p.fmt.table_name,name_col_width(&g));",
                1,
            ),
            (
                "letcell=rect(g.x_name,y+3.0,name_col_width(&g),18.0);letclipped=p.clip(key,&p.fmt.table_name,name_col_width(&g));",
                1,
            ),
            // 趋势图横轴起点：60 px 那一格吃的是库里的日期串
            ("p.clip(&left,&p.fmt.caption,60.0),rect(plot.left,plot.bottom+4.0,60.0,14.0)", 1),
            // 星期分布柱顶数字：居中，越界两头都跑
            ("p.clip(&thousands(count.max(0)asu64),&p.fmt.center_small,bw),rect(x,plot.bottom-hh-WEEKDAY_VALUE_HEADROOM,bw,16.0)", 1),
            // 目标条右边那句：右对齐，左边 8 px 就是进度条
            ("p.clip(&format!(\"连续打卡{}天·近一年最长{}天\",g.streak,g.best),&p.fmt.caption_right,198.0),rect(dots_x-206.0,r.top+14.0,198.0,18.0)", 1),
            // 设备详情弹窗那一叠行：标签 76、值吃到弹窗右边减 100
            ("p.clip(label,&p.fmt.caption,76.0),rect(cx,yy,76.0,18.0)", 1),
            ("p.clip(value,&p.fmt.body,dialog.right-cx-100.0),rect(cx+82.0,yy,dialog.right-cx-100.0,18.0)", 1),
        ] {
            assert_eq!(
                flat.matches(needle).count(),
                want,
                "这一处夹取不见了、被改写成两份、或夹取与矩形分了家：{needle}（期望 {want} 处）"
            );
        }
    }

    /// 这一族的**中央**接线腿：生产段里只许有两处按字符折算的地方 ——
    /// `clip_middle` 的定义与 `clip_to_cell` 里那一次退回。
    ///
    /// 为什么钉这个：把"全角尺子"直接抄进新的格子，症状不是报错，是屏上明明画得下的句子
    /// 长出省略号（这一轮那三处就是这么来的）。新格子必须过 `p.clip`，让 DirectWrite
    /// 复核一次；`char_budget_at` 只留给 `clip_to_cell` 自己当退回档。
    #[test]
    fn no_new_cell_may_clip_by_the_full_width_ruler_alone() {
        let src = include_str!("main.rs").replace("\r\n", "\n");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod: String = src[..cut]
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let flat: String = prod.chars().filter(|c| !c.is_whitespace()).collect();
        // `fn clip_middle(` 与 `clip_to_cell` 里 `_=>clip_middle(text,budget)` 各一处
        assert_eq!(
            flat.matches("clip_middle(").count(),
            2,
            "生产段里按字符折算的入口多了：除定义与 clip_to_cell 那一次退回之外，\
             新格子要走 p.clip（实测复核），不要手抄全角尺子"
        );
        // `fn char_budget_at(` 与 `clip_to_cell` 里那一处 `letbudget=`
        assert_eq!(
            flat.matches("char_budget_at(").count(),
            2,
            "全角尺子被抄到了 clip_to_cell 外面"
        );
        assert_eq!(
            flat.matches("fnclip_to_cell(").count() + flat.matches("pubfnclip_to_cell(").count(),
            1,
            "实测复核那一道不见了"
        );
    }

    /// 接线腿：五张卡的副行也要过同一道夹取，且预算与画它的那个矩形用同一个宽度表达式。
    ///
    /// 现量的形状是卡片副行顶出格子：那句改成种数之后最长可到「本周期 1234 个键位」，
    /// 而窄窗口下一张卡只剩几十像素宽 —— `NO_WRAP` 的 DrawText 不会自己省略，
    /// 多出来的字照画到隔壁那张卡上（本仓已为路径行与状态行立过同一把尺子）。
    #[test]
    fn the_card_sub_lines_are_clipped_to_the_card_width() {
        let src = include_str!("main.rs").replace("\r\n", "\n");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let flat: String = src[..cut].chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(
            flat.matches("p.clip(sub,&p.fmt.caption,cw-24.0)").count(),
            1,
            "卡片副行那道夹取不见了，或被改写成两份"
        );
        assert_eq!(
            flat.matches("rect(cx+12.0,142.0,cw-24.0,16.0)").count(),
            1,
            "预算与矩形不再是同一个宽度表达式（cw-24.0 那一档）"
        );
    }

    /// 搜索框里那个原生 EDIT 的矩形必须是自己那块底框内缩而来，并跟着底框一起走。
    ///
    /// 底框按客户区宽度**右边**定位，所以窗口一改尺寸它就挪；输入框不跟着重算就分家 ——
    /// 屏上剩一个点下去没反应的空框，占位文案留在原地跟着每一拍重绘闪（他报的那两条症状
    /// 是同一条的两个面）。
    #[test]
    fn the_search_edit_rect_follows_its_own_box() {
        let box_at_1200 = search_box_rect(1200.0);
        let (x, y, ew, eh) = search_edit_px(box_at_1200, 1.0);
        assert_eq!((x, y), (916, 221), "底框内缩 8/3 那道边没了：{x},{y}");
        assert_eq!((ew, eh), (244, 20), "输入框的尺寸不再等于底框减去两边");
        // 窗口加宽 400：右边定位的底框整块往右挪 400，输入框必须一起挪
        let box_at_1600 = search_box_rect(1600.0);
        assert_eq!(
            search_edit_px(box_at_1600, 1.0).0 - x,
            400,
            "输入框没跟着自己的底框走"
        );
        // 缩放档位要一起进算式（150% 时交回的是物理像素）
        assert_eq!(search_edit_px(box_at_1200, 1.5).0, (916.0 * 1.5) as i32);
    }

    /// 接线腿：窗口几何变的那两条消息都必须重摆输入框。
    /// 上面那条纯函数只证明"算得对"，摆位没人调它照样分家。
    #[test]
    fn the_geometry_messages_replace_the_search_edit() {
        let src = include_str!("main.rs").replace("\r\n", "\n");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod = &src[..cut];
        for (name, until) in [
            ("WM_SIZE =>", "WM_ENTERSIZEMOVE =>"),
            ("WM_EXITSIZEMOVE =>", "WM_DPICHANGED =>"),
        ] {
            let at = prod.find(name).unwrap_or_else(|| panic!("{name} 不见了"));
            let end = prod[at..]
                .find(until)
                .map(|n| n + at)
                .unwrap_or_else(|| panic!("{until} 不见了"));
            let body = &prod[at..end];
            assert!(
                body.contains("app.place_search_edit(hwnd)"),
                "{name} 这一拍没有重摆搜索框的输入窗口：拖大窗口或最大化之后点它没反应"
            );
        }
        // 摆位一共六处：进设置页那一下、WM_SIZE、拖拽结束、点搜索框叫出输入层、
        // 焦点递回来那一下、Ctrl+F 那一下。多一处少一处都要红。
        assert_eq!(
            prod.matches("place_search_edit(hwnd)").count(),
            6,
            "搜索框的摆位调用点数变了"
        );
    }

    /// 接线腿：提示文案只在控件**没有**焦点时由父窗写，焦点变化必须从子类过程递回来。
    ///
    /// 这条缺陷的形状不是"算错了"，是两块表面往同一批像素上写字：那块矩形属于叠在底框上的
    /// 原生 EDIT，父窗有焦点时照样往那儿画提示，就出现"看得见提示、看不见光标、字在闪"。
    /// ⚠ 上一版这条腿把 `WM_SETFOCUS|WM_KILLFOCUSif_data==2` 原样钉了进来 —— **守卫把缺陷钉成了
    /// 规格**：子类过程的第 5 个参数才是子类 id，第 6 个是两处都传 0 的 `dwRefData`，那一支
    /// 永远不成立，于是修复上线后屏上仍是"焦点在 EDIT 上而提示照旧在画"
    /// （2026-11-01 用 `search_geom_probe.ps1` + 连拍抓屏现量）。所以这里钉的是**角色从哪儿取**。
    #[test]
    fn the_hint_yields_the_box_to_the_control_that_owns_it() {
        let src = include_str!("main.rs").replace("\r\n", "\n");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let flat: String = src[..cut].chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            flat.contains("iffilter.is_empty()&&!vs.search_focused"),
            "父窗又无条件把提示字画进控件的矩形里了：有焦点时就是闪烁"
        );
        assert!(
            flat.contains("search_focus_signal(id,msg)"),
            "子类过程不按第 5 个参数（子类 id）认角色了：焦点递不回来，提示永远压在控件上"
        );
        assert!(
            !flat.contains("if_data=="),
            "又拿 `dwRefData` 当子类 id 用了 —— 两处 SetWindowSubclass 传的都是 0"
        );
        assert!(
            flat.contains("app.search_focused=w.0!=0"),
            "收回来的焦点位没落到 App 上"
        );
        // 两个 EDIT 各占一个 id，且这两个 id 必须真的不同：相同就没有这条角色轴可言。
        assert_ne!(SEARCH_EDIT_SUBCLASS, RENAME_EDIT_SUBCLASS);
        assert!(
            flat.contains("SetWindowSubclass(edit,Some(edit_proc),subclass_id,0)"),
            "宿主化的 EDIT 没按传入的子类 id 注册：父窗分不出是谁在递焦点"
        );
        for (call, name) in [
            (
                "create_hosted_edit(parent,self.ui_font,SEARCH_EDIT_SUBCLASS)",
                "搜索框",
            ),
            (
                "create_hosted_edit(parent,self.ui_font,RENAME_EDIT_SUBCLASS)",
                "行内编辑",
            ),
        ] {
            assert!(
                flat.contains(call),
                "{name}那一支子类化没按 id 注册了：父窗分不出是谁在递焦点"
            );
        }
    }

    /// 焦点信号只属于搜索框那一支，两条消息的方向也不能反。
    ///
    /// 抽成纯函数就是为了这一条：`edit_proc` 是 `extern "system"`，无头造不出"子类过程收到一条
    /// WM_SETFOCUS"，而它偏偏是本场唯一在屏上量到过的缺陷。剩下的接线由上面那条腿钉。
    #[test]
    fn only_the_search_edit_reports_focus() {
        assert_eq!(
            search_focus_signal(SEARCH_EDIT_SUBCLASS, WM_SETFOCUS),
            Some(true),
            "焦点进框没递回'该让位'"
        );
        assert_eq!(
            search_focus_signal(SEARCH_EDIT_SUBCLASS, WM_KILLFOCUS),
            Some(false),
            "焦点出框没递回'该把提示画回来'"
        );
        assert_eq!(
            search_focus_signal(RENAME_EDIT_SUBCLASS, WM_SETFOCUS),
            None,
            "改名框开始递焦点了"
        );
        assert_eq!(
            search_focus_signal(RENAME_EDIT_SUBCLASS, WM_KILLFOCUS),
            None
        );
        assert_eq!(
            search_focus_signal(SEARCH_EDIT_SUBCLASS, WM_CHAR),
            None,
            "打字被当成焦点变化"
        );
        assert_eq!(
            search_focus_signal(0, WM_SETFOCUS),
            None,
            "没带 id 的调用被当成搜索框"
        );
    }

    /// 剪贴板页搜索框：导航键必须转给页面，改字键必须留给文本框。
    #[test]
    fn clip_search_forwards_navigation_keys_but_keeps_text_keys() {
        for vk in [0x09, 0x21, 0x22, 0x26, 0x28] {
            assert!(
                clip_search_key_goes_to_page(CLIP_SEARCH_EDIT_SUBCLASS, WM_KEYDOWN, vk),
                "0x{vk:02X} 应当转给页面（Tab/翻页/上下）"
            );
        }
        for vk in [0x2E, 0x24, 0x23] {
            assert!(
                !clip_search_key_goes_to_page(CLIP_SEARCH_EDIT_SUBCLASS, WM_KEYDOWN, vk),
                "0x{vk:02X} 是删字/移光标，必须留给文本框"
            );
        }
        assert!(
            !clip_search_key_goes_to_page(SEARCH_EDIT_SUBCLASS, WM_KEYDOWN, 0x09),
            "设置页搜索框后面没有列表，不许抢 Tab"
        );
        assert!(
            !clip_search_key_goes_to_page(CLIP_SEARCH_EDIT_SUBCLASS, WM_CHAR, 0x09),
            "只接 WM_KEYDOWN：WM_CHAR 是字符输入"
        );
    }

    /// 周报数「达标」的那把尺子必须与面板/连续打卡那一份同值。
    ///
    /// 现量症状（`goal.daily_keys = 0`）：周报里「达标天数 7 / 7（每日目标 0 次）」连零活动
    /// 那天都算达标、每日活跃那栏还画 ✅，而设置页的「连续 N 天」按 `effective_goal` 兜成 1
    /// 之后判成 0 天。这条对拍拿的是同一天、同一份原始目标 ⇒ 任何一边换掉尺子都会红。
    #[test]
    fn the_weekly_report_and_the_panel_count_goal_the_same_way() {
        use focusflow_core::stats::goal_status;
        let today = chrono::Local::now().date_naive();
        let day_key = |i: i64| {
            (today - chrono::Duration::days(i))
                .format("%Y-%m-%d")
                .to_string()
        };
        // 近 7 天（旧→新）：两个零活动日就是被"0 目标"念成达标的那两天
        let counts: [i64; 7] = [3, 0, 7, 0, 12, 1, 5];
        for raw_goal in [0i64, -5, 1, 5, 20000] {
            let rows: Vec<(String, i64)> = counts
                .iter()
                .enumerate()
                .map(|(i, c)| (day_key(6 - i as i64), *c))
                .collect();
            let st = goal_status(raw_goal, &rows, &day_key(0), 0);
            assert_eq!(st.days.len(), counts.len(), "夹具前提：近 7 天每天都有判定");
            for (i, (_, _, met)) in st.days.iter().enumerate() {
                assert_eq!(
                    weekly_goal_met(counts[i], raw_goal),
                    *met,
                    "原始目标 {raw_goal} 下第 {i} 天（{} 次）两把尺子判得不一样",
                    counts[i]
                );
            }
            if raw_goal <= 0 {
                assert!(
                    !weekly_goal_met(0, raw_goal),
                    "目标兜住之后，零活动那天不该算达标（原始值 {raw_goal}）"
                );
                assert!(
                    weekly_goal_met(1, raw_goal),
                    "兜成 1 就该按 1 判（原始值 {raw_goal}）"
                );
            }
        }
    }
}

#[cfg(test)]
mod theme_drift_tests {
    use super::*;

    fn rgb(c: D2D1_COLOR_F) -> (u8, u8, u8) {
        (
            (c.r * 255.0).round() as u8,
            (c.g * 255.0).round() as u8,
            (c.b * 255.0).round() as u8,
        )
    }

    /// 悬浮窗那份调色板是独立副本（`floating_render` 刻意不引 Win32 类型），所以它必须与
    /// 主窗口**同主题的 solid 档**逐值相等。漂了的症状不是报错，是同一个程序里两张卡
    /// 差半个色号 —— 边框已经分别漂过一次（悬停/斑马同色那个 bug 旁边就是它）。
    /// alpha 不比：悬浮卡片压在壁纸上是半透明的，主窗口 solid 档是不透明的。
    #[test]
    fn the_floating_card_uses_the_same_base_colours_as_the_main_window() {
        for (name, pal) in [
            ("light", floating_render::LIGHT),
            ("dark", floating_render::DARK),
        ] {
            let t = tokens_for(name).solid;
            for (field, got, want) in [
                ("card", (pal.card.r, pal.card.g, pal.card.b), rgb(t.card)),
                (
                    "border",
                    (pal.border.r, pal.border.g, pal.border.b),
                    rgb(t.border),
                ),
                ("fg", (pal.fg.r, pal.fg.g, pal.fg.b), rgb(t.fg)),
                (
                    "muted",
                    (pal.muted.r, pal.muted.g, pal.muted.b),
                    rgb(t.muted),
                ),
                (
                    "accent",
                    (pal.accent.r, pal.accent.g, pal.accent.b),
                    rgb(t.accent),
                ),
            ] {
                assert_eq!(got, want, "{name} 的悬浮窗 {field} 与主窗口不是同一个颜色");
            }
        }
    }
}

#[cfg(test)]
mod text_tier_tests {
    use super::*;

    fn dw() -> IDWriteFactory {
        unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).expect("DWrite 工厂") }
    }

    fn weight(f: &IDWriteTextFormat) -> DWRITE_FONT_WEIGHT {
        unsafe { f.GetFontWeight() }
    }

    fn size(f: &IDWriteTextFormat) -> f32 {
        unsafe { f.GetFontSize() }
    }

    /// 三档字重必须真的落在各自的槽位上：这条问的是 DWrite 交回来的值，不是源码里
    /// 写了什么。汉字在 600/700 上同形（雅黑只有两档），所以"次要文字回 400"是
    /// 整屏层级唯一看得见的那一跳；哪天有人把某一档改回 600，这条要红。
    #[test]
    fn the_three_weights_are_wired_to_the_right_slots() {
        let f = build_formats(&dw()).expect("Formats");
        for (slot, got) in [
            ("caption", weight(&f.caption)),
            ("caption_right", weight(&f.caption_right)),
            ("caption_center", weight(&f.caption_center)),
            ("nav_item", weight(&f.nav_item)),
            ("text12", weight(&f.text12)),
        ] {
            assert_eq!(got, UI_WEIGHT_REG, "{slot} 该是常规档（次要信息）");
        }
        for (slot, got) in [
            ("body", weight(&f.body)),
            ("table_name", weight(&f.table_name)),
            ("small", weight(&f.small)),
            ("right", weight(&f.right)),
            ("center_small", weight(&f.center_small)),
            ("tab", weight(&f.tab)),
        ] {
            assert_eq!(got, UI_WEIGHT, "{slot} 该是强档（要扫的字）");
        }
        for (slot, got) in [
            ("h1", weight(&f.h1)),
            ("value", weight(&f.value)),
            ("settings_h", weight(&f.settings_h)),
        ] {
            assert_eq!(got, UI_WEIGHT_EM, "{slot} 该是展示档（标题/大数）");
        }
    }

    /// 次要档这一轮抬到 13（原先 12.5 / 12 在 100% 缩放下偏小）；三处仍要同一个数。
    #[test]
    fn the_caption_tier_keeps_its_sizes() {
        let f = build_formats(&dw()).expect("Formats");
        assert_eq!(size(&f.caption), 13.0);
        // 夹取那一道现在直接问 `GetFontSize()`，但排行摘要那几条判据仍按 `CAPTION_PT` 折算：
        // 两边分家时判据量的就不是屏上那一格 —— 上面那条钉住 13，这条钉住"同一个数"。
        assert_eq!(
            CAPTION_PT,
            size(&f.caption),
            "摘要预算的字号档与 caption 那一档分家了"
        );
        assert_eq!(size(&f.caption_right), 13.0);
        assert_eq!(size(&f.caption_center), 13.0);
        assert_eq!(size(&f.nav_item), SETTINGS_TEXT_PT);
        assert_eq!(size(&f.text12), SETTINGS_HELP_PT);
    }
}

#[cfg(test)]
mod contrast_tests {
    use super::*;

    /// WCAG 的相对亮度。
    fn lum(c: D2D1_COLOR_F) -> f32 {
        let ch = |v: f32| {
            let v = v.clamp(0.0, 1.0);
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * ch(c.r) + 0.7152 * ch(c.g) + 0.0722 * ch(c.b)
    }

    fn ratio(a: D2D1_COLOR_F, b: D2D1_COLOR_F) -> f32 {
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    /// 半透明层压在底色上之后的实际颜色（只算 RGB，够用来比对比度）。
    fn over(fg: D2D1_COLOR_F, bg: D2D1_COLOR_F) -> D2D1_COLOR_F {
        let a = fg.a;
        D2D1_COLOR_F {
            r: fg.r * a + bg.r * (1.0 - a),
            g: fg.g * a + bg.g * (1.0 - a),
            b: fg.b * a + bg.b * (1.0 - a),
            a: 1.0,
        }
    }

    /// 两套主题、两个玻璃档：正文 ≥7:1、次要文字 ≥4.5:1、强调色 ≥3:1。
    ///
    /// 这一轮把 `muted` 调亮/调深、又给卡片加了半透明阴影，最容易出的事故是
    /// "看着更雅了，但说明行读不清"。对比度是能把这件事变成数字的那把尺子。
    #[test]
    fn text_colours_stay_readable_on_their_surfaces() {
        for glass in [false, true] {
            for (name, t) in [("light", light_theme(glass)), ("dark", dark_theme(glass))] {
                // 玻璃档卡片半透明：拿页面底当最坏背衬比。
                let card = if glass { t.page } else { t.card };
                let fg = ratio(t.fg, card);
                let muted = ratio(t.muted, card);
                let accent = ratio(t.accent, card);
                assert!(fg >= 7.0, "{name} glass={glass} 正文对比度只有 {fg:.2}:1");
                assert!(
                    muted >= 4.5,
                    "{name} glass={glass} 次要文字对比度只有 {muted:.2}:1"
                );
                assert!(
                    accent >= 3.0,
                    "{name} glass={glass} 强调色对比度只有 {accent:.2}:1"
                );
            }
        }
    }

    /// 斑马纹要看得见，悬停必须比斑马纹更明显 —— 这两条同时成立，"奇数行悬停没反应"
    /// 那个 bug 才算真的修掉（以前两者是同一个颜色）。
    #[test]
    fn zebra_is_visible_and_hover_is_stronger() {
        for glass in [false, true] {
            for (name, t) in [("light", light_theme(glass)), ("dark", dark_theme(glass))] {
                let card = if glass { t.page } else { t.card };
                let zebra = ratio(over(t.zebra, card), card);
                let hover = ratio(over(t.row_hover, card), card);
                assert!(
                    zebra > 1.02,
                    "{name} glass={glass} 斑马纹看不见（{zebra:.3}）"
                );
                assert!(
                    hover > zebra,
                    "{name} glass={glass} 悬停 {hover:.3} 不比斑马纹 {zebra:.3} 明显"
                );
            }
        }
    }
}

#[cfg(test)]
mod window_chrome_tests {
    use super::*;

    /// 三颗按钮必须贴在右上角、彼此不重叠、整条高度落在标题带里（不伸进内容区），
    /// 也不许压到周期药丸那一行（y=48 起）。
    #[test]
    fn the_three_caption_buttons_sit_at_the_top_right_without_overlapping() {
        let w = 1200.0;
        let b = win_buttons(w);
        assert_eq!(b[0].left, w - WIN_BTN_W * 3.0);
        assert_eq!(b[2].right, w);
        for (i, r) in b.iter().enumerate() {
            assert_eq!(r.top, 0.0);
            assert_eq!(r.bottom, WIN_BTN_H);
            assert_eq!(r.right - r.left, WIN_BTN_W);
            if i > 0 {
                assert_eq!(b[i - 1].right, r.left, "第 {i} 颗按钮与左边那颗重叠了");
            }
        }
        assert!(b[0].bottom <= TITLEBAR_H, "按钮比标题带还高");
        assert!(b[0].bottom <= 48.0, "按钮伸进了周期药丸那一行");
    }

    /// 命中按按钮自己的矩形来：右到左依次是关闭 / 最大化 / 最小化，
    /// 标题带左侧与按钮下方都不算按钮（否则拖窗会被按钮吃掉）。
    #[test]
    fn the_caption_buttons_answer_on_their_own_rects() {
        let w = 1200.0;
        assert_eq!(win_button_at(w, w - 20.0, 10.0), Some(2));
        assert_eq!(win_button_at(w, w - 60.0, 10.0), Some(1));
        assert_eq!(win_button_at(w, w - 110.0, 10.0), Some(0));
        assert_eq!(win_button_at(w, 300.0, 20.0), None);
        assert_eq!(win_button_at(w, w - 20.0, 60.0), None);
    }

    /// 设备详情弹窗是页面模态，不是窗口模态：右上角三颗必须照常归窗口。
    ///
    /// 拆掉原生标题条之前那三颗在窗口外面，弹窗再大也压不到它们；搬进客户区之后
    /// "谁在最上面"只剩这一段代码说了算，而当时弹窗分支排在了按钮之前——症状是弹窗
    /// 开着时点最小化/最大化/关闭只关掉弹窗，要关程序得点两下。
    #[test]
    fn the_detail_modal_does_not_swallow_the_window_buttons() {
        let w = 1200.0;
        // 三颗各自的中点，弹窗开着也照样点名到按钮。
        for (i, dx) in [(0u8, 115.0f32), (1, 69.0), (2, 23.0)] {
            assert_eq!(
                top_owner(true, w, w - dx, 20.0),
                TopOwner::WinBtn(i),
                "弹窗开着时第 {i} 颗按钮被吞掉了"
            );
        }
        // 弹窗仍然吃得到页面那一片：只有按钮那一格例外。
        assert_eq!(top_owner(true, w, 600.0, 400.0), TopOwner::DetailModal);
        assert_eq!(top_owner(true, w, 300.0, 20.0), TopOwner::DetailModal);
        // 弹窗没开时同一格是页面。
        assert_eq!(top_owner(false, w, 600.0, 400.0), TopOwner::Page);
        assert_eq!(top_owner(false, w, w - 23.0, 20.0), TopOwner::WinBtn(2));
        // 按钮带之外的标题带（y>=40）不算按钮：它要交回系统当拖动带。
        assert_eq!(top_owner(true, w, w - 23.0, 42.0), TopOwner::DetailModal);
    }

    /// 接线腿：这一层只许由 `top_owner` 一处决定，画图/悬停/点击三条路读同一份。
    /// 上面那条行为判据盯得住 `top_owner` 自己排序，盯不到"某条路绕过它又抄了一遍
    /// 弹窗判据"——那条只能钉调用形态。
    #[test]
    fn only_one_place_decides_who_is_on_top() {
        let src = include_str!("main.rs").replace("\r\n", "\n");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod = src[..cut]
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        // 一份定义 + 两处调用：`top_owner` 决定归谁，`WM_NCHITTEST` 只问"这颗按钮要不要
        // 交回客户区"。多出来的一眼就是有人绕过 top_owner 自己排了一遍弹窗。
        assert_eq!(
            prod.matches("win_button_at(").count(),
            3,
            "`win_button_at` 的生产调用点不再是 top_owner + WM_NCHITTEST 这两处了"
        );
        assert_eq!(
            prod.matches("top_owner(self.detail.is_some(), w, x, y)")
                .count(),
            2,
            "悬停与点击两条路里有一条不再问 top_owner 谁在最上面了"
        );
        // 点击那条路必须先在 top_owner 之后才碰弹窗矩形。
        let click = &prod[prod.find("fn on_click(").expect("on_click 不见了")..];
        let owner = click
            .find("top_owner(")
            .expect("on_click 不再问谁在最上面这一层");
        let rects = click
            .find("detail_rects(w, h)")
            .expect("on_click 里弹窗那一段不见了");
        assert!(
            owner < rects,
            "弹窗那一段又排回窗口按钮之前（top_owner 在 {owner}，detail_rects 在 {rects}）"
        );
    }

    /// 右上角那一格归缩放，不归按钮。
    ///
    /// 三颗按钮一直画到窗口右沿，而可拖动的边框带也是那 8 DIP：按钮先吃的话
    /// "右上角拖不动、顶边在按钮那一段也拖不动"，原生标题条时代这条优先级是系统给的。
    #[test]
    fn the_frame_band_grabs_the_corner_before_the_caption_buttons() {
        let (w, h) = (1200.0f32, 860.0f32);
        let (bx, by) = (8.0f32, 8.0f32);
        // 右沿 8 DIP 从上到下都是缩放带，压在关闭按钮那一段也一样。
        assert_eq!(nc_hit_code(false, w, h, bx, by, w - 4.0, 20.0), HTRIGHT);
        assert_eq!(nc_hit_code(false, w, h, bx, by, w - 4.0, 4.0), HTTOPRIGHT);
        // 顶边 8 DIP 压在最大化和最小化那两颗上：给 HTTOP（系统才有拖动/双击的入口）。
        assert_eq!(nc_hit_code(false, w, h, bx, by, w - 69.0, 3.0), HTTOP);
        assert_eq!(nc_hit_code(false, w, h, bx, by, w - 115.0, 3.0), HTTOP);
        // 按钮内侧照旧是客户区——验收脚本点的就是这几格。
        for dx in [20.0f32, 69.0, 115.0] {
            assert_eq!(
                nc_hit_code(false, w, h, bx, by, w - dx, 20.0),
                HTCLIENT,
                "离右沿 {dx} DIP 那点不再归按钮了"
            );
        }
        // 最大化时没有边框带：右沿那一格还给按钮。
        assert_eq!(nc_hit_code(true, w, h, bx, by, w - 4.0, 20.0), HTCLIENT);
        assert_eq!(nc_hit_code(true, w, h, bx, by, 4.0, 300.0), HTCLIENT);
        // 标题带与页面本体。
        assert_eq!(nc_hit_code(false, w, h, bx, by, 300.0, 20.0), HTCAPTION);
        assert_eq!(nc_hit_code(false, w, h, bx, by, 300.0, 300.0), HTCLIENT);
        assert_eq!(nc_hit_code(false, w, h, bx, by, 4.0, 300.0), HTLEFT);
        assert_eq!(nc_hit_code(false, w, h, bx, by, 300.0, h - 3.0), HTBOTTOM);
    }

    /// 接线腿：窗口过程只许问 `nc_hit_code` 这一份顺序（钉调用形态，不是钉函数名出现过）。
    #[test]
    fn the_window_procedure_asks_the_same_hit_order() {
        let src = include_str!("main.rs").replace("\r\n", "\n");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod = src[..cut]
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            prod.matches("nc_hit_code(zoomed, cw, ch, bx, by, x, y)")
                .count(),
            1,
            "窗口过程不再把命中顺序交给 nc_hit_code 了（或在别处又复刻了一遍）"
        );
        let ht = &prod[prod.find("WM_NCHITTEST => {").expect("WM_NCHITTEST 不见了")..];
        let ht = &ht[..ht
            .find("WM_NCACTIVATE =>")
            .expect("WM_NCHITTEST 的尾巴不见了")];
        assert!(
            !ht.contains("win_button_at(") && !ht.contains("HTTOPLEFT"),
            "窗口过程里又复刻了一遍命中顺序（按钮尺子或八向判断），nc_hit_code 不再是唯一那一份"
        );
        assert!(
            ht.contains("let zoomed = ") && ht.contains("frame_thickness(hwnd)"),
            "WM_NCHITTEST 没把\"最大化\"与边框厚度交给 nc_hit_code"
        );
    }
}

#[cfg(test)]
mod underline_tests {
    /// 页签下划线必须按文字实测的宽度画；固定 52px 时两个字和四个字挂同一条线。
    #[test]
    fn the_tab_underline_is_measured_not_fixed() {
        let src = include_str!("main.rs");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod = src[..cut]
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !prod.contains("r.left + 52.0"),
            "下划线又回到了固定 52px —— 它应该跟着文字宽度走"
        );
        assert!(
            prod.contains(".measure(label, &p.fmt.body)"),
            "下划线没有量文字宽度：`P::measure` 那一处调用不见了"
        );
    }
}

#[cfg(test)]
mod open_dir_tests {
    /// 「打开数据目录」这个按钮不许自己起 Explorer。
    ///
    /// 症状只在 `data_home` 写成正斜杠形式时露出来（README 里就那么写着，core 也一路容忍）：
    /// `explorer C:/…` 开出来的是「文档」，不报错、不提示，日志还写着「打开数据目录：C:/…」，
    /// 于是用户以为数据在文档里。本机 2026-10-30 实测三种写法，只有反斜杠那一版开到目标。
    /// 斜杠与引号那把尺子在 `focusflow_core::reveal` 里已经有一份了，这里只许调用。
    #[test]
    fn the_data_dir_button_reuses_the_explorer_ruler() {
        let src = include_str!("main.rs").replace("\r\n", "\n");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod = &src[..cut];
        let body = &prod[prod
            .find("fn open_data_dir(")
            .expect("open_data_dir 不见了")..];
        let body = &body[..body
            .find("\n    }")
            .expect("open_data_dir 的函数体结束不见了")];
        assert!(
            body.contains("reveal::open_dir("),
            "「打开数据目录」不再走 core 那把斜杠尺子: {body}"
        );
        assert!(
            !body.contains("Command::new(\"explorer\")"),
            "这里又自己起了一次 Explorer：斜杠与引号的规矩从此有两份"
        );
    }
}

#[cfg(test)]
mod snapshot_poke_tests {
    use super::*;

    /// 催醒那两半都要读到：位被按下去 ⇒ 第一片就醒并把位清掉；没人催 ⇒ 睡满并回"没被催"。
    ///
    /// 只测"能醒"那一半的话，把 `swap` 写成 `load`（醒过来但不清位）照样绿 —— 于是下一拍
    /// 又白醒一次，而这条判据看不见。
    #[test]
    fn a_poke_wakes_the_sleep_and_an_unpoked_sleep_runs_its_full_length() {
        poke_snapshot();
        let t = std::time::Instant::now();
        assert!(sleep_or_poke(4_000), "按过催位却没醒");
        assert!(
            t.elapsed().as_millis() < 1_000,
            "醒了却把整段睡完了：{:?}",
            t.elapsed()
        );
        let t2 = std::time::Instant::now();
        assert!(
            !sleep_or_poke(120),
            "没人催却回来说被催醒了：那一位没清干净"
        );
        assert!(
            t2.elapsed().as_millis() >= 100,
            "没被催也提前醒了：{:?}",
            t2.elapsed()
        );
    }

    /// 被催醒的那一拍要等满节流才许再查：等得不够就补睡剩下的那一段。
    ///
    /// 方向写反（`since - min_gap`）与不减（永远 0）在这里都会红：前者让刚查完的那一拍
    /// 立刻再查一次，后者等于没有节流——打字高峰就变回每键 10 条语句。
    #[test]
    fn a_poked_tick_waits_out_the_throttle_before_querying_again() {
        assert_eq!(
            poke_backlog_ms(0, POKE_MIN_GAP_MS),
            250,
            "刚查完就该补满这一档"
        );
        assert_eq!(poke_backlog_ms(249, POKE_MIN_GAP_MS), 1);
        assert_eq!(
            poke_backlog_ms(250, POKE_MIN_GAP_MS),
            0,
            "等满了还不许立刻拍"
        );
        assert_eq!(
            poke_backlog_ms(9_000, POKE_MIN_GAP_MS),
            0,
            "等超了不许减成负数"
        );
        assert_eq!(poke_backlog_ms(1_000, 0), 0);
    }

    /// 催拍真的接在两个"该看的东西变了"的现场，而且唤窗那一下接在**上屏之后**。
    ///
    /// 为什么钉源码而不是造一条验收腿：这条路的观察量是"第一帧新数比上一帧晚多少毫秒"，
    /// 无头侧量不到（快照线程不在测试进程里跑），而 GUI 套件要的是屏上形状不是节拍。
    /// 顺序那一半是硬的：先催后上屏，线程会醒在"还没人看"那一刻并把那一位花掉，
    /// 症状照旧是点开窗口先看见旧数。
    #[test]
    fn the_poke_is_wired_after_the_show_and_on_the_key_callback() {
        let src = include_str!("main.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let flat: String = prod.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            flat.contains("sleep_or_poke(IDLE_WAKE_MS)"),
            "空转那一段还在整片睡：唤窗催不动它"
        );
        assert!(
            flat.contains("sleep_or_poke(tick_ms)"),
            "按拍那一段还在整片睡：按键催不动它"
        );
        assert!(
            flat.contains("poke_backlog_ms(last_query.elapsed().as_millis()asu64,POKE_MIN_GAP_MS"),
            "被催醒那一拍不再节流（或节流用的是别的钟）"
        );
        let shown = flat
            .find("let_=ShowWindow(hwnd,SW_RESTORE)")
            .expect("唤窗那一步不见了");
        let poked = flat
            .find("poke_snapshot();")
            .expect("唤窗之后不再催拍：点开窗口还是先看见旧数");
        assert!(shown < poked, "催拍排在上屏之前：线程会醒在还没人看那一刻");
        // 隐藏时放掉了渲染目标 ⇒ 唤回必须先画好再显示，否则屏上是一段"有底色没内容"的面板
        // （现量 9 帧 / 约 110 ms，`.scratch/hide_show_first_frame.ps1` 的连拍腿）。
        // 次序在这里是唯一有意义的判据：反过来写照样编译、照样跑绿，症状只在人眼里。
        // ⚠ 必须收在 `bring_to_front` 那一身之内比：`app.draw(hwnd);` 在 `WM_PAINT` 那支也有，
        // 全文 `find` 会先撞上别人，那条顺序断言就变成比两句无关文本的先后 —— 空判据。
        let bf_at = prod
            .find("unsafe fn bring_to_front(hwnd: HWND)")
            .expect("bring_to_front 不见了");
        let bf = &prod[bf_at..bf_at + prod[bf_at..].find("\n}").expect("函数体没收尾")];
        let draw_at = bf
            .find("app.draw(hwnd);")
            .expect("唤回前不再先画一帧：那一步是连拍腿抓出来的");
        let show_at = bf
            .find("ShowWindow(hwnd, SW_SHOW)")
            .expect("唤窗的显示那一步不见了");
        assert!(
            draw_at < show_at,
            "画帧排到显示之后了：打开面板会先闪一块只有底色的表面，再等首帧内容"
        );
        // 针脚要挑**这一处**的调用形状：`refresh_now()` 里也有一句 `let next = data::load(`，
        // 只写前半会先撞上它，那条顺序断言就成了比两句话的先后而不是比这一拍。
        // 时刻要记在查库**之后**（否则那一拍自己的耗时被算进节流参照）。
        // 这里从前比的是两处文本的先后（`loaded < stamped`），2026-10-31 起它抓错了对象：
        // 卡片那一支的 `last_query` 在文件里更靠前，`find` 先撞上它，于是面板那条顺序
        // 变成"面板的查库 vs 卡片的记时刻"——一次假红。改成把**次序钉进相邻**的形状：
        // 两支各自"查完立刻记"，换不得也借得了别的分支才算红。（把 `find` 改成从
        // `loaded` 往后找会让这条永远成立 —— 那是把判据做成空的，不走。）
        assert!(
            flat.contains(
                "letnext=data::load(p.load(std::sync::atomic::Ordering::Relaxed));\
                 last_query=std::time::Instant::now();"
                    .replace([' ', '\n', '\r'], "")
                    .as_str()
            ),
            "面板那一拍不再「查完就记时刻」：节流没有参照，或时刻被记到查库之前"
        );
        assert!(
            flat.contains("letthin=data::load_card_only();last_query=std::time::Instant::now();"),
            "卡片那一拍不再「查完就记时刻」：被催醒的那一拍会照旧立刻再问一次"
        );

        let csrc = include_str!("collect.rs");
        let cflat: String = csrc.chars().filter(|c| !c.is_whitespace()).collect();
        // 那两句之间夹着注释，所以比的是先后而不是拼成一串：整串匹配会被注释挡掉（红过一次）。
        let rec = cflat
            .find("record_key_at(now_secs());")
            .expect("键回调里不再记账");
        let poked = cflat
            .find("crate::poke_snapshot();")
            .expect("键回调不再催这一拍");
        assert!(
            rec < poked,
            "催拍排在记账之前：数字还没进账就催，那一拍量到的是旧数"
        );
        assert_eq!(
            cflat.matches("crate::poke_snapshot();").count(),
            1,
            "collect.rs 里催拍的位置应当只有键回调那一处"
        );
    }
}

#[cfg(test)]
mod main_window_state_tests {
    use super::*;

    fn prod() -> String {
        let src = include_str!("main.rs").replace("\r\n", "\n");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        src[..cut]
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 还原那一步只有这条规则兜底：存下的矩形可能在屏幕外（拔了副屏、换了分辨率）。
    /// 夹的次序也是判据 —— 先夹坐标再夹尺寸，"比屏幕还宽"的存档会把左边顶出屏外。
    #[test]
    fn a_saved_rect_is_clamped_back_into_the_work_area() {
        let work = (0, 0, 1920, 1040);
        assert_eq!(
            clamp_main_rect(work, (300, 150, 1200, 860)),
            (300, 150, 1200, 860),
            "本来就完整可见的窗口不许被挪"
        );
        assert_eq!(
            clamp_main_rect(work, (5000, 5000, 1200, 860)),
            (720, 180, 1200, 860),
            "右下角之外的窗口要推回来，且右下边贴住工作区"
        );
        assert_eq!(
            clamp_main_rect(work, (-500, -400, 1200, 860)),
            (0, 0, 1200, 860),
            "拔掉左边那块屏之后，负坐标要被收回来"
        );
        assert_eq!(
            clamp_main_rect(work, (100, 100, 3000, 2000)),
            (0, 0, 1920, 1040),
            "比屏幕还大的存档要先缩到工作区、再贴左上角"
        );
        assert_eq!(
            clamp_main_rect(work, (100, 100, 10, 10)),
            (100, 100, 480, 360),
            "小到抓不住的尺寸要抬到下限"
        );
        let left_monitor = (-1920, 0, 0, 1040);
        assert_eq!(
            clamp_main_rect(left_monitor, (-2000, 100, 1200, 860)),
            (-1920, 100, 1200, 860),
            "副屏在主屏左边时负的原点是合法工作区"
        );
    }

    /// 存档是逻辑像素 + 存盘时的缩放：还原按存档里那个倍数换回物理像素，
    /// 不能拿建完窗问到的 DPI 去猜（多屏缩放不同时会猜错屏）。
    #[test]
    fn the_saved_scale_is_used_to_restore_physical_pixels() {
        let st = MainWindowState {
            left: 100.0,
            top: 50.0,
            width: 1000.0,
            height: 700.0,
            scale: 1.0,
            maximized: false,
        };
        assert_eq!(physical_rect_of(&st), (100, 50, 1000, 700));
        assert_eq!(
            physical_rect_of(&MainWindowState { scale: 1.5, ..st }),
            (150, 75, 1500, 1050)
        );
        assert_eq!(
            physical_rect_of(&MainWindowState { scale: 0.0, ..st }),
            (100, 50, 1000, 700),
            "缩放坏掉时按 100% 算，别把窗口缩成一条缝"
        );
    }

    /// 接线腿：启动要读存档、三条保存路径都要在，"退出前保存"还得排在销毁窗口之前。
    #[test]
    fn the_geometry_is_restored_at_startup_and_written_on_every_exit_path() {
        let prod = prod();
        assert_eq!(
            prod.matches("let saved_window = focusflow_core::window_state::load();")
                .count(),
            1,
            "启动读存档应当只有一处（另一处是保存时用来补正常位置的读）"
        );
        assert_eq!(
            prod.matches("save_main_geometry(hwnd);").count(),
            3,
            "保存点应当是「拖完 / 最大化换档 / 退出」各一处"
        );
        let at = prod
            .find("fn quit_app(hwnd: HWND)")
            .expect("quit_app 不见了");
        let rest = &prod[at..];
        let body = &rest[..rest.find("\n}\n").expect("quit_app 的函数体切不出来")];
        let saved = body
            .find("save_main_geometry(hwnd);")
            .expect("退出前不保存几何：拖完最后一下马上退出就白拖了");
        let destroyed = body
            .find("DestroyWindow(hwnd)")
            .expect("退出路径里没有 DestroyWindow（这条判据的锚变了）");
        assert!(
            saved < destroyed,
            "几何写在销毁窗口之后：那一次已经读不到矩形"
        );
        // 唤窗不再无条件 `SW_RESTORE`：那会把最大化好的窗口压回普通尺寸。
        let flat: String = prod.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            flat.contains("ifunsafe{IsIconic(hwnd)}.as_bool(){let_=ShowWindow(hwnd,SW_RESTORE);}"),
            "唤窗又变成无条件 SW_RESTORE 了：最大化好的窗口会被压回普通尺寸"
        );
    }
}

#[cfg(test)]
mod log_rotation_tests {
    use super::*;

    /// 这份诊断日志是纯追加的（心跳每 5 秒一行），不设上限就会一直长。
    /// 轮转只有两种结局：改名成功（旧内容整份进 `.1`）或原地截断；第三种"什么都不做、
    /// 继续往后写"就是缺陷本身。第二条判据盯的是一次只留一份备份 —— 绝不越堆越多。
    #[test]
    fn an_oversized_note_log_is_rotated_and_keeps_a_single_backup() {
        let dir = std::env::temp_dir().join(format!(
            "ff_logrot_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("focusflow.log");
        let rotated = dir.join("focusflow.log.1");

        std::fs::write(&path, vec![b'x'; 2048]).unwrap();
        assert!(
            !rotate_log_if_oversized(&path, &rotated, 4096),
            "没到上限不许动"
        );

        assert!(rotate_log_if_oversized(&path, &rotated, 1024));
        let rotated_len = std::fs::metadata(&rotated).unwrap().len();
        assert_eq!(rotated_len, 2048, "旧内容要整份进 .1");
        assert!(
            !path.exists() || std::fs::metadata(&path).unwrap().len() == 0,
            "轮转后主文件要重新开始，不能留着旧内容继续长"
        );

        // 再来一轮：旧备份被覆盖，而不是又长出 .2/.3。
        std::fs::write(&path, vec![b'y'; 2048]).unwrap();
        assert!(rotate_log_if_oversized(&path, &rotated, 1024));
        let again = std::fs::read(&rotated).unwrap();
        assert_eq!(again.len(), 2048);
        assert_eq!(again[0], b'y', "备份里应当是最近一轮的内容");
        assert!(!dir.join("focusflow.log.2").exists(), "只留一份备份");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod plugin_page_tests {
    use super::*;

    fn row(stem: &str, builtin: bool) -> PluginRow {
        PluginRow {
            name: stem.to_string(),
            desc: "简介".to_string(),
            version: "1.0".to_string(),
            author: "某人".to_string(),
            stem: stem.to_string(),
            enabled: true,
            loaded: true,
            error: None,
            builtin,
            resident: false,
            api: 1,
            permissions: 0,
            permission_names: Vec::new(),
            grant_note: "已授权".to_string(),
            needs_grant: false,
            slow: false,
            note: None,
            refresh_ms: 0,
            min_app: String::new(),
            store_bytes: None,
            store_error: None,
            health: None,
        }
    }

    /// 台账那一句话要真进得了这一行，而 `None`（什么都没跑过）不许在屏上留下痕迹。
    ///
    /// core 那边"汇总句拼得出来"是另一条判据；这一条盯的是**页面读了它** —— 少了读者，
    /// 落盘的 `health.json` 就是本仓普查点过的那一类"写了没人读"。
    #[test]
    fn the_row_prints_the_ledger_line_only_when_it_exists() {
        let quiet = row("health_row", false);
        let before = quiet.declaration();
        assert!(
            !before.contains("p95") && !before.contains("还没有记录"),
            "全新的格子不该带耗时那一句，也不该占一格：{before}"
        );

        let mut loud = row("health_row", false);
        loud.health = Some("失败记录 2 条 · 视图重建超预算 2 次（p95 3.0 ms）".to_string());
        let after = loud.declaration();
        assert!(
            after.contains("视图重建超预算 2 次"),
            "页面上没读到台账那一句：{after}"
        );
        assert!(
            after.ends_with("失败记录 2 条 · 视图重建超预算 2 次（p95 3.0 ms）"),
            "台账那一句排在既有那几格之后（配置在前、跑起来怎么样在后）：{after}"
        );
    }

    /// 「需要授权」那句必须点名**要点同意的到底是哪几项**，而且排在"已启用尚未加载"之前。
    ///
    /// 三条臂：新增权限那一句要看得见新增了什么；更硬的消息（加载失败）仍然排最前；
    /// 已授权的插件那一格不许冒出这句（否则每一行都写着要授权，用户就会一路点同意 ——
    /// 方案 3.3 写的正是这个下场）。
    #[test]
    fn a_row_needing_a_grant_names_the_permissions_before_anything_less_actionable() {
        let mut r = row("my_tool", false);
        r.needs_grant = true;
        r.grant_note = "权限变了，需要重新授权（新增了 storage.write）".to_string();
        r.permission_names = vec!["stats.read".to_string(), "storage.write".to_string()];
        let (line, bad) = r.second_line();
        assert!(bad, "需要授权是坏消息，得画 danger");
        assert!(
            line.contains("stats.read") && line.contains("storage.write"),
            "要点同意的哪几项必须看得见: {line}"
        );
        assert!(line.contains("新增"), "新增了哪一项要在句子里: {line}");
        // 更硬的一条消息仍然在前
        r.error = Some("语法错".to_string());
        let (worse, _) = r.second_line();
        assert!(worse.starts_with("加载失败"), "{worse}");
        // 已经授权过的不许插进来
        let ok = row("quiet_tool", false);
        let (quiet, quiet_bad) = ok.second_line();
        assert!(
            !quiet_bad && quiet == "简介",
            "已授权那一档不该自称坏消息: {quiet}"
        );
    }

    /// 按钮的几何只有一份：画它的这一头与点它的那一头共用同一个函数（命中条件也是同一句）。
    #[test]
    fn the_grant_button_geometry_is_not_copied() {
        let code = include_str!("../src/main.rs");
        let at = code
            .find("mod plugin_page_tests {")
            .expect("插件页那一套用例的模块起点找不到了");
        let prod = &code[..at];
        assert_eq!(
            prod.matches("fn plugin_grant_rect(").count(),
            1,
            "那颗按钮的几何函数应当只有一个定义"
        );
        assert_eq!(
            prod.matches("plugin_grant_rect(").count(),
            3,
            "定义一处 + 画一处 + 命中一处，多出来的一处就是手抄了第二遍坐标"
        );
        assert!(
            prod.contains("grantable && hit(plugin_grant_rect"),
            "命中的条件必须与画它的条件同一句（needs_grant），否则会出现点不动的按钮"
        );
    }

    /// 「详情」那颗按钮：画它的这一头与点它的那一头共用同一个表达式。
    ///
    /// 与授权那颗同一条纪律 —— 两处各写一遍坐标，症状是屏上有一颗看得见的按钮点不动。
    /// 计数固定成 3（定义 + 画 + 命中），多出来的一处就是有人抄了第二份几何。
    #[test]
    fn the_detail_button_is_drawn_and_hit_from_one_expression() {
        let code = include_str!("../src/main.rs");
        let at = code
            .find("mod plugin_page_tests {")
            .expect("插件页那一套用例的模块起点找不到了");
        let prod = &code[..at];
        assert_eq!(
            prod.matches("fn plugin_detail_rect(").count(),
            1,
            "详情那颗按钮的几何函数应当只有一个定义"
        );
        assert_eq!(
            prod.matches("plugin_detail_rect(").count(),
            3,
            "定义一处 + 画一处 + 命中一处，多出来的一处就是手抄了第二遍坐标"
        );
        assert!(
            prod.contains("hit(plugin_detail_rect(row_rect.right, y0), x, y)"),
            "命中读的必须是画它的那一份几何与同一个 y"
        );
    }

    /// 三颗按钮不许叠在一起：开关 / 授权 / 详情 各占一段，且留出 8px 的缝。
    ///
    /// 叠了也不会 panic —— 症状是"点开关点到了详情"或"授权那颗只有半边点得动"，
    /// 这种错在截图上看不出来，所以用一次算术把它钉住。
    #[test]
    fn the_row_buttons_do_not_overlap() {
        let right = 1000.0;
        let y = 0.0;
        let switch = rect(right - 76.0, y + 9.0, 60.0, 24.0);
        let grant = plugin_grant_rect(right, y);
        let detail = plugin_detail_rect(right, y);
        assert!(
            detail.right <= grant.left - 8.0,
            "详情与授权之间要留一条缝：详情右沿 {} / 授权左沿 {}",
            detail.right,
            grant.left
        );
        assert!(
            grant.right <= switch.left - 8.0,
            "授权与开关之间要留一条缝：授权右沿 {} / 开关左沿 {}",
            grant.right,
            switch.left
        );
    }

    /// 这一页的纵向次序：四行表格必须整段落在错误历史标题上面。
    ///
    /// 「最后一行压到下一段标题上」只在像素层看得出来，而这一页的字体不裁切 ——
    /// 两段叠在一起就是两句字画在同一行上。
    #[test]
    fn the_hook_table_ends_before_the_error_history_begins() {
        let table_bottom = DETAIL_TABLE_HEAD_Y + 20.0 + 4.0 * DETAIL_ROW_H;
        assert!(
            DETAIL_ERRORS_TITLE_Y >= table_bottom + 6.0,
            "错误历史的标题压在表格最后一行上：表到 {table_bottom}，标题在 {DETAIL_ERRORS_TITLE_Y}"
        );
        const {
            assert!(
                DETAIL_ERRORS_TOP >= DETAIL_ERRORS_TITLE_Y + 18.0,
                "the first error line must start below its heading"
            )
        };
    }

    /// 顶上那句只报数得清的两样：**已加载**的常驻个数与每格配置上限。
    ///
    /// 反方向也钉住：不许出现"合计占用"那一截 —— mlua 没有每 VM 的分配读数，摊出来的数
    /// 会把估算印成测量（方案 ADR-10）。"常驻但这一轮没装上"不算已加载。
    #[test]
    fn the_heading_reports_resident_count_and_the_cap_but_no_fabricated_total() {
        let mut resident_ok = row("resident_ok", false);
        resident_ok.resident = true;
        resident_ok.loaded = true;
        let mut resident_waiting = row("resident_waiting", false);
        resident_waiting.resident = true;
        resident_waiting.loaded = false;
        let panel_tool = row("panel_tool", false);

        let line = plugin_heading(
            &[resident_ok, resident_waiting, panel_tool],
            16,
            "/x/plugins",
        );
        assert!(line.starts_with("3 个"), "{line}");
        assert!(line.contains("已加载 1 个常驻"), "只数真装上的：{line}");
        assert!(line.contains("每格内存上限 16 MB"), "{line}");
        assert!(line.contains("/x/plugins"), "目录那一截要跟着：{line}");
        assert!(
            !line.contains("合计"),
            "合计占用是摊出来的数，不许印成读数：{line}"
        );
    }

    /// 安装 / 卸载 / 确认框三处的几何各只有一份：画与命中读同一个表达式。
    ///
    /// 与「详情」「授权」那两颗同一条纪律。确认框那三颗尤其要紧 —— 画错了顺序，
    /// 用户点「取消」落到的可能是「卸载并删除设置」。
    #[test]
    fn the_install_and_uninstall_buttons_have_one_geometry() {
        let code = include_str!("../src/main.rs");
        let at = code
            .find("mod plugin_page_tests {")
            .expect("插件页那一套用例的模块起点找不到了");
        let prod = &code[..at];
        for (name, head, want) in [
            ("plugin_install_rect", "fn plugin_install_rect(", 4),
            ("detail_uninstall_rect", "fn detail_uninstall_rect(", 4),
            (
                "uninstall_prompt_geometry",
                "fn uninstall_prompt_geometry(",
                3,
            ),
        ] {
            assert_eq!(prod.matches(head).count(), 1, "{name} 应当只有一个定义");
            assert_eq!(
                prod.matches(&format!("{name}(")).count(),
                want,
                "{name}：定义一处 + 画一处 + 命中一处（可点的还多一处光标），\
                 多出来的一处就是手抄了第二遍坐标"
            );
        }
    }

    /// 确认框那三颗按钮要整颗落在框里，且相互不重叠。
    #[test]
    fn the_uninstall_prompt_buttons_sit_inside_their_box_and_do_not_overlap() {
        let (b, buttons) = uninstall_prompt_geometry(1200.0, 860.0);
        for (i, btn) in buttons.iter().enumerate() {
            assert!(
                btn.left >= b.left
                    && btn.right <= b.right
                    && btn.top >= b.top
                    && btn.bottom <= b.bottom,
                "第 {i} 颗按钮跑出框外了：{btn:?} / {b:?}"
            );
        }
        for pair in buttons.windows(2) {
            assert!(
                pair[0].right <= pair[1].left,
                "两颗按钮叠在一起了：{:?} / {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    /// 安装/卸载的回执各说各的结局（更新 vs 新装、保留 vs 删除 vs 没存过）。
    #[test]
    fn the_install_and_uninstall_lines_name_the_outcome() {
        use focusflow_core::plugins::install::{Installed, StateAfter, Uninstalled};
        let base = Installed {
            stem: "demo".to_string(),
            path: PathBuf::from("/x/demo.zip"),
            replaced: false,
            entries: 2,
            bytes: 256,
            problems: 0,
        };
        let fresh = install_line(Ok(base.clone()));
        assert!(
            fresh.contains("已安装") && fresh.contains("demo") && fresh.contains("2 个文件"),
            "{fresh}"
        );
        assert!(!fresh.contains("问题"), "干净的包不许提示问题：{fresh}");
        let updated = install_line(Ok(Installed {
            replaced: true,
            problems: 3,
            ..base
        }));
        assert!(
            updated.contains("已更新") && updated.contains("3 条问题"),
            "{updated}"
        );
        assert!(install_line(Err("包坏了".into())).contains("安装失败"));

        let gone = |state: StateAfter| Uninstalled {
            stem: "demo".to_string(),
            removed_package: PathBuf::from("/x/demo.zip"),
            state,
        };
        assert!(
            uninstall_line(Ok(gone(StateAfter::Kept(PathBuf::from("/s/demo")))))
                .contains("设置还在")
        );
        assert!(uninstall_line(Ok(gone(StateAfter::Purged))).contains("都删了"));
        assert!(uninstall_line(Ok(gone(StateAfter::NeverExisted))).contains("没有留下设置"));
        // 第四种结局：包删了、设置没删干净。折成"卸载失败"会让用户以为插件还在。
        let partial = uninstall_line(Ok(gone(StateAfter::PurgeFailed {
            path: PathBuf::from("/s/demo"),
            why: "另一个程序正在使用此文件".to_string(),
        })));
        assert!(
            partial.contains("已卸载") && partial.contains("包已删除"),
            "包确实删掉了，得先说这件事：{partial}"
        );
        assert!(
            partial.contains("设置目录删不干净") && partial.contains("还留在原处"),
            "没删干净的那一半也要说：{partial}"
        );
        assert!(uninstall_line(Err("不在".into())).contains("卸载失败"));
    }

    /// 详情页的「卸载」只在**有安装记录**时出现：那个布尔值必须由安装记录算出来。
    #[test]
    fn the_uninstall_button_follows_the_install_record() {
        let code = include_str!("../src/main.rs");
        let at = code
            .find("mod plugin_page_tests {")
            .expect("插件页那一套用例的模块起点找不到了");
        let prod = &code[..at];
        let start = prod
            .find("fn open_plugin_detail(")
            .expect("打开详情那一支不见了");
        let rest = &prod[start..];
        let end = rest
            .find("\n    fn ")
            .expect("详情那一支后面没有别的函数了");
        let body = &rest[..end];
        assert!(
            body.contains("install::meta_for(") && body.contains("install::is_installed("),
            "「有没有包可卸」必须由安装记录算出来，不许按扩展名或猜"
        );
    }

    /// 三张页的先后：视图 > 详情 > 清单；详情那一格不在清单里就退回清单。
    ///
    /// 最后一条不是防御性代码：文件被删/改名之后清单会重扫，而 `plugin_detail_open`
    /// 还停在那个 stem 上 —— 不退回清单，屏上就是一张退不出去的页（「返回」只画在详情
    /// 那一张上，而清单已经找不到那一格，点哪儿都不动）。
    #[test]
    fn the_fourth_tab_picks_the_view_then_the_detail_then_the_list() {
        let rows = vec![row("alpha", false), row("beta", true)];
        let view = focusflow_core::plugins::PluginView {
            title: String::new(),
            widgets: Vec::new(),
        };
        match plugin_page_of(Some(("alpha", &view)), Some("beta"), &rows) {
            PluginPage::View(name, _) => assert_eq!(name, "alpha", "视图压过详情"),
            other => panic!("应当画视图，实得 {other:?}"),
        }
        match plugin_page_of(None, Some("beta"), &rows) {
            PluginPage::Detail(r) => assert_eq!(r.stem, "beta"),
            other => panic!("应当画详情，实得 {other:?}"),
        }
        assert!(
            matches!(plugin_page_of(None, Some("gone"), &rows), PluginPage::List),
            "那一格已经从清单里消失了，就画清单（详情页的「返回」没有落点）"
        );
        assert!(matches!(
            plugin_page_of(None, None, &rows),
            PluginPage::List
        ));
    }

    /// 详情页**不加载插件**：它读的是台账与清单。
    ///
    /// 这一条决定了"停用的、装不起来的格子也能点进详情"。哪天有人顺手在打开那一支里
    /// 加一句 `ensure_loaded`，这里会先红 —— 而不是等用户发现"点一下详情把一个插件装活了"。
    #[test]
    fn opening_the_detail_page_does_not_load_the_plugin() {
        let code = include_str!("../src/main.rs");
        let at = code
            .find("mod plugin_page_tests {")
            .expect("插件页那一套用例的模块起点找不到了");
        let prod = &code[..at];
        let start = prod
            .find("fn open_plugin_detail(")
            .expect("打开详情那一支不见了");
        let rest = &prod[start..];
        let end = rest
            .find("\n    fn ")
            .expect("详情那一支后面没有别的函数了");
        let body = &rest[..end];
        for forbidden in ["ensure_loaded", "refresh_view", "pump_timers"] {
            assert!(
                !body.contains(forbidden),
                "打开详情不许碰 VM（含 {forbidden}）：这一页对停用/加载失败的格子也要成立"
            );
        }
    }

    /// 慢熔断那一档要在页面那一格说出来，而没进熔断的不许标。
    ///
    /// 两条臂：`row()` 那个夹具默认是"没被降频"的，所以第一句钉的是"不许乱标"；
    /// 第二句钉的是"降了频要看得见" —— 少任何一句，另一边的缺陷都能悄悄活着。
    #[test]
    fn a_throttled_panel_says_so_in_the_declaration_cell() {
        let plain = row("quiet_tool", false);
        assert!(
            !plain.declaration().contains("较慢"),
            "没进熔断的不许标: {:?}",
            plain.declaration()
        );
        let mut slow = row("slow_tool", false);
        slow.slow = true;
        assert!(
            slow.declaration().contains("该插件较慢"),
            "降频之后页面上要看得见: {:?}",
            slow.declaration()
        );
    }

    /// 刷新节奏这一条链两头都在生产里：判断住在 core，那一拍只问一次、跑完只记一次。
    ///
    /// 与看板页 `should_refresh`、热重载那条链同一把尺子：规则写在 UI 里就没人问得到，
    /// 而"写死 1 秒"那种回退不会让任何一条用例红 —— 除非这里钉住它。
    #[test]
    fn the_refresh_cadence_chain_has_both_ends_in_production() {
        let code = include_str!("../src/main.rs");
        let at = code
            .find("mod plugin_page_tests {")
            .expect("插件页那一套用例的模块起点找不到了");
        let prod: String = code[..at]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            prod.contains("pm.view_due(&name, std::time::Instant::now())"),
            "那一拍不再问 core 的节奏 ⇒ 「缺省 0 = 只在动作后刷」这条声明又变成装饰"
        );
        assert!(
            prod.contains("pm.note_view_refreshed("),
            "跑完不记耗时 ⇒ 慢熔断永远攒不出样本，「该插件较慢」那句就没人能触发"
        );
        assert!(
            !prod.contains("last_pv_refresh"),
            "写死一秒的那份旧时钟还挂在生产里：节奏判断又回到 UI 这一头了"
        );
    }

    /// 定时器这一条链两头都在生产里：core 那侧排得进程序，GUI 那一拍真的去取。
    ///
    /// 少了投递那一行，症状是"插件排了定时器、页面什么都不说、也永远不跑" —— 与热重载
    /// 那条链同形（检测线程整晚在扫而没人处理），所以照同一把尺子钉。
    #[test]
    fn the_timer_chain_has_both_ends_in_production() {
        let code = include_str!("../src/main.rs");
        assert!(
            code.contains("fn pump_plugin_timers(&mut self)"),
            "投递那一步没写成方法"
        );
        let at = code
            .find("WM_TIMER if w.0 == TIMER_EVENTS")
            .expect("100ms 那一拍的起点找不到了");
        // 按**字符**取窗口：按字节切会在中文注释的多字节边界上 panic（本条用例第一版就是这么红的）
        let window: String = code[at..].chars().take(4_000).collect();
        // 注释掉的调用不算接线：留着 `// app.pump_plugin_timers();` 也过，等于给"改回不投递"
        // 留了一条无声的路（同一族缺陷在 manager 那道闸上被抓过一次）
        let prod: String = window
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            prod.contains("app.pump_plugin_timers();"),
            "那一拍里没有真的调用（被注释掉与从来没写都算没接上）= 排程永远没人跑"
        );
    }

    /// 「故意没加载」那句排在"已启用，尚未加载"之后，且不许画成坏消息。
    ///
    /// 顺序错的症状是页面上每一行 `panel` 档的插件都长得像出过错：那句"面板打开时才加载"
    /// 本来是解释，跟着 danger 色就变成指控。两边都钉，反向那条也一起：真失败时不许被说明盖掉。
    #[test]
    fn a_deliberately_unloaded_row_explains_itself_without_becoming_an_error() {
        let mut r = row("paneled_tool", false);
        r.loaded = false;
        r.note = Some("面板打开时才加载".to_string());
        let (line, bad) = r.second_line();
        assert!(line.contains("面板打开时才加载"), "{line}");
        assert!(!bad, "设计如此的没加载不许画成 danger 色");
        assert!(
            !line.contains("已启用，尚未加载"),
            "同一格不许同时说两句话: {line}"
        );
        r.error = Some("顶层就炸".to_string());
        let (worse, worse_bad) = r.second_line();
        assert!(
            worse.starts_with("加载失败") && worse_bad && !worse.contains("面板打开"),
            "真失败的原因不许被那句解释盖住: {worse}"
        );
    }

    /// 面板装卸这一条链两头都在生产里：打开那一格会装，关掉那一拍会卸。
    ///
    /// 少任何一头，`PLUGIN_LIFECYCLE` 就只是页面上那个「面板 / 常驻」标签：
    /// 只装不卸是一份 VM 永远不回来，只卸不装是点开一张空卡。
    #[test]
    fn the_panel_lifecycle_chain_has_both_ends_in_production() {
        let code = include_str!("../src/main.rs");
        let at = code
            .find("mod plugin_page_tests {")
            .expect("插件页那一套用例的模块起点找不到了");
        let prod: String = code[..at]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            prod.contains("pm.ensure_loaded(name)"),
            "打开面板不再装那一格 ⇒ `panel` 档永远打不开（清单上还会写着「面板打开时才加载」）"
        );
        assert_eq!(
            prod.matches("pm.release_panel_of(&was)").count(),
            1,
            "卸那一格只该有一个调用点：多个出口各写一遍，迟早有一处漏掉"
        );
        assert!(
            prod.contains("plugin_panel_was: None"),
            "上一拍开着哪一页没被记住 ⇒ 关掉与切换的那个转变认不出来"
        );
    }

    /// 两栏分组的格子表：表头在前、行在后，`Row` 里的下标仍是 `rows` 的行号。
    ///
    /// 这条钉三件事，都是"分组"这一步才会出现的形状：
    /// ① 顺序 = 内置栏在前（与 core 交出清单的那一序同一含义，同名的以随包那份为准）；
    /// ② 行号不重编（命中与开关都按它走，重编了就是"点开关关到隔壁那个插件"）；
    /// ③ 空栏不画表头（只有一栏有东西时，屏上不该出现一句孤零零的「扩展插件」）。
    #[test]
    fn the_two_columns_carry_headers_and_keep_the_row_numbers() {
        let mut rows = vec![row("pomodoro_plugin", true), row("stats_overview", true)];
        rows.push(row("my_tool", false));
        let slots = plugin_slots(&rows);
        assert_eq!(
            slots,
            vec![
                PluginSlot::Header("内置模块"),
                PluginSlot::Row(0),
                PluginSlot::Row(1),
                PluginSlot::Header("扩展插件"),
                PluginSlot::Row(2),
            ],
            "两栏的格子表长这样，行号必须还是原表的下标"
        );

        // 只有内置那一栏有东西 ⇒ 不画第二句表头
        let only_builtin = plugin_slots(&rows[..2]);
        assert_eq!(
            only_builtin
                .iter()
                .filter(|s| matches!(s, PluginSlot::Header(_)))
                .count(),
            1,
            "空的那一栏不该有表头: {only_builtin:?}"
        );
        // 只有用户装的 ⇒ 也不画「内置模块」
        let only_user = plugin_slots(&rows[2..]);
        assert_eq!(
            only_user[0],
            PluginSlot::Header("扩展插件"),
            "{only_user:?}"
        );
        assert!(
            plugin_slots(&[]).is_empty(),
            "一份插件都没有 ⇒ 一格都不该画"
        );
    }

    /// 三个消费点必须读同一份格子表（画 / 悬停命中 / 点击命中 / 滚轮上限）。
    ///
    /// 这条是本仓那条老教训的形状版：分组这件事一改，"一屏能装几行"就跟着变，
    /// 任何一处还按 `rows.len()` 数 ⇒ 屏上画的是这一行、点下去命中的是那一行。
    /// 判据数的是**调用形态**（`plugin_slots(` 出现的次数与那一句滚轮上限的写法），
    /// 只查函数名在不在会被注释顶班。
    #[test]
    fn every_plugin_page_consumer_reads_the_same_slot_table() {
        let src = include_str!("main.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let code: String = prod
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let uses = code.matches("plugin_slots(").count();
        assert!(
            uses >= 5,
            "格子表的读者从 5 处（定义 1 + 画 1 + 悬停 1 + 点击 1 + 滚轮 1）掉到 {uses} 处了"
        );
        assert!(
            code.contains("4 => plugin_slots(&app.plugin_rows).len()"),
            "滚轮上限不再按格子数了 ⇒ 到底那一屏还能不能再滚取决于表头被不算一行"
        );
        assert!(
            code.matches("let slots = plugin_slots(&self.plugin_rows);")
                .count()
                >= 2,
            "两处命中（悬停与点击）不再读同一份表了 ⇒ 亮的是一行、点的是另一行"
        );
        assert!(
            code.contains("let slots = plugin_slots(rows);"),
            "画这一页时读的不是这份表"
        );
        // 反向腿：旧的那把尺子不许还留在插件页的任何一个消费点上
        assert!(
            !code.contains("let count = self.plugin_rows.len();"),
            "命中那一头还在按行数（不算表头）夹滚动 ⇒ 最后两行会被表头挤掉"
        );
    }

    /// 一行"亮起来"的区域必须与"点得到"的区域是同一个：悬停从前只判 `y`，
    /// 指针落在卡片左右 16px 的边距里也会亮起那一行（而点下去什么都不发生）。
    #[test]
    fn the_hover_band_is_the_same_rect_as_the_click_band() {
        let w = 960.0;
        let row_h = 46.0;
        let r = plugin_row_rect(w, 300.0, row_h);
        assert_eq!((r.left, r.right), (32.0, w - 32.0));
        assert!(hit(r, 100.0, 310.0), "行中间要命中");
        assert!(!hit(r, 20.0, 310.0), "左边距里不该亮（点也点不到）");
        assert!(!hit(r, w - 20.0, 310.0), "右边距里不该亮（点也点不到）");
        assert!(!hit(r, 100.0, 300.0 + row_h - 3.0), "行间空档不归这一行");

        // 画 / 悬停 / 点击三处读同一个函数，不是各写一套坐标。
        let src = include_str!("main.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        for needle in [
            "let r = plugin_row_rect(w, y, row_h);",
            "let row_rect = plugin_row_rect(w, y0, row_h);",
            "if hit(plugin_row_rect(w, y0, row_h), x, y) {",
        ] {
            assert!(
                prod.contains(needle),
                "画/悬停/点击又各写一套行矩形：缺 {needle}"
            );
        }
    }

    /// 声明那一格只说**与缺省不同**的东西，随包那五个一字未改的脚本不该有字。
    #[test]
    fn the_declaration_cell_only_shows_what_differs_from_the_defaults() {
        let quiet = row("stats_overview", true);
        assert_eq!(
            quiet.declaration(),
            "",
            "全是缺省值时这一格该是空的: {quiet:?}"
        );

        let mut loud = row("my_tool", false);
        loud.resident = true;
        loud.api = 2;
        loud.permissions = 3;
        loud.refresh_ms = 1500;
        loud.min_app = "1.6.0".to_string();
        loud.store_bytes = Some(4096);
        let text = loud.declaration();
        for want in [
            "常驻",
            "API 2",
            "权限 3 项",
            "1500ms 自刷",
            "需 1.6.0",
            "占用 4 KB",
        ] {
            assert!(text.contains(want), "声明里少了「{want}」: {text}");
        }
        // 占用小于 1 KB 时说字节，不显示"0 KB"（那是"没有"的读法）
        let mut tiny = row("tiny", false);
        tiny.store_bytes = Some(512);
        assert!(
            tiny.declaration().contains("占用 512 B"),
            "{:?}",
            tiny.declaration()
        );
        let mut none_yet = row("fresh", false);
        none_yet.store_bytes = Some(0);
        assert_eq!(none_yet.declaration(), "", "还没写过东西不该报一个 0");
    }

    /// 第二格那句话的四条臂，各自要有各自的话。
    ///
    /// `store_error` 这一条最容易被漏：写盘失败发生在去抖之后的另一个线程上，
    /// 屏上不说，用户就只能等重启那天才发现那一份没落盘。
    #[test]
    fn the_second_line_names_whichever_thing_actually_went_wrong() {
        let mut r = row("t", false);
        assert_eq!(
            r.second_line(),
            ("简介".to_string(), false),
            "正常就报那个插件是什么"
        );

        r.enabled = true;
        r.loaded = false;
        let (text, bad) = r.second_line();
        assert!(text.contains("已启用，尚未加载") && !bad, "{text}");

        r.store_error = Some("写盘失败（X）：os error 5".to_string());
        let (text, bad) = r.second_line();
        assert!(
            text.contains("存储写不进去") && text.contains("os error 5"),
            "{text}"
        );
        assert!(bad, "存储没落盘是一句坏消息，不该画成灰色");

        r.error = Some("attempt to call a nil value".to_string());
        let (text, bad) = r.second_line();
        assert!(
            text.starts_with("加载失败：") && bad,
            "加载失败比存储失败更要紧，两样都在时先说前者: {text}"
        );
        assert!(
            !text.contains("存储写不进去"),
            "一句话里不该同时挤进两件事: {text}"
        );
    }

    /// 接线腿：`plugin_store::flush_all` 必须真被两个读者用到，而且顺序对。
    ///
    /// 摘掉退出前那一次 ⇒ 最后 300ms 的改动停在内存里，而随后那次退出备份收走的是盘上
    /// 旧的一份（症状是"刚存的一条没了"）；摘掉进插件页那一次 ⇒ 这一页的"占用"与
    /// "存储写不进去"说的都是上一次的值。判据数的是调用形态与先后，
    /// 只查函数名在不在会被注释与文档顶班。
    #[test]
    fn the_pending_store_writes_are_asked_before_the_page_and_before_exit() {
        let src = include_str!("main.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let code: String = prod
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            code.matches("plugin_store::flush_all").count(),
            2,
            "两个读者（退出前 / 进插件页）少了一个 —— 去抖那一头从此没人问了"
        );
        let at = code
            .find("fn quit_app(hwnd: HWND)")
            .expect("quit_app 不见了（改名或换了写法，这条守卫的锚要跟着改）");
        let rest = &code[at..];
        let body = &rest[..rest.find("\n}").expect("quit_app 的函数体切不出来")];
        let asked = body.find("plugin_store::flush_all");
        assert!(
            asked.is_some(),
            "退出前不再问一次插件存储的结局 ⇒ 最后那 300ms 的改动永远不落盘"
        );
        assert!(
            asked.unwrap() < body.find("save_main_geometry(hwnd)").unwrap(),
            "落盘那一问排到了后面几步之后 ⇒ 退出备份收走的还是旧的一份"
        );
    }

    /// 台账那一份也要在退出前落一次：常驻插件退出时不会走卸载那条路。
    ///
    /// 少了这一问，只有面板插件与出错停用过的格子能跨过重启；常驻那一族这一场的
    /// 耗时、错误历史与降频次数在退出那一刻清零（而 health.json 存在的理由正是这个）。
    #[test]
    fn the_health_ledger_is_flushed_before_the_exit_backup() {
        let src = include_str!("main.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let code: String = prod
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let at = code
            .find("fn quit_app(hwnd: HWND)")
            .expect("quit_app 不见了（改名或换了写法，这条守卫的锚要跟着改）");
        let rest = &code[at..];
        let body = &rest[..rest.find("\n}").expect("quit_app 的函数体切不出来")];
        let asked = body
            .find("plugin_health::flush_all")
            .expect("退出前不问一次台账 ⇒ 常驻插件这一场的账清零");
        let backup = body
            .find("collect::shutdown")
            .expect("退出备份那一步找不到了");
        assert!(
            asked < backup,
            "台账要在退出备份之前落盘（plugins/state/ 会被一起收进备份包）"
        );
    }

    /// 插件页顶上那句「每格内存上限 N MB」要读 core 那个夹过的数。
    ///
    /// 页面自己 parse 原始字符串时不夹：config 写 999999 就会说"999999 MB"，而 VM
    /// 实际只有 4096；写 0 时说 0、VM 是 1 —— 两处各说一套。
    #[test]
    fn the_plugin_page_takes_the_memory_cap_from_core() {
        let src = include_str!("main.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        assert!(
            prod.contains("plugins::manager::memory_limit_mb("),
            "页面没读 core 那个上限（自己 parse 一份就是两套账）"
        );
        assert!(
            !prod.contains("get_or(\"plugins\", \"memory_limit_mb\""),
            "页面还在自己读原始值"
        );
    }

    /// 热重载之后"要不要重扫这一页 / 要不要留一行"的四档判定。
    ///
    /// 这两半各钉一个症状，缺任何一半都是假绿：只断"在页 ⇒ 重扫"照不出"不在页也每 2 秒
    /// 全量重扫"（白读两遍目录、逐格问一次存储）；只断"有变更 ⇒ 出声"照不出"每拍都留一行"
    /// —— 通道空着是本分的常态，那种日志一秒钟就有十条废话。
    #[test]
    fn the_reload_followup_decides_rescan_and_notice_separately() {
        let one = vec!["pomodoro".to_string()];
        let (rescan, note) = reload_followup(true, &one);
        assert!(
            rescan,
            "就在插件页却换了东西 ⇒ 清单得跟着重扫，否则屏上停在改之前那一行"
        );
        let note = note.expect("真换上来了就该留一行可对账的痕迹");
        assert!(note.contains("pomodoro"), "那行要说清是哪一格: {note}");
        assert!(
            note.contains("重扫了清单"),
            "在页那一档的措辞要写明已经重扫: {note}"
        );
        // 验收脚本靠这一段 ASCII 数（中文主体在 shell/PowerShell 里匹配不稳）
        assert!(note.contains("hotreload=1"), "少了 ASCII 那截计数: {note}");

        let (rescan, note) = reload_followup(false, &one);
        assert!(
            !rescan,
            "不在这一页时别全量重扫 ⇒ 每 2 秒白读目录、逐格问存储"
        );
        let note = note.expect("没在看也要留痕迹：作者关心的正是'存盘到底进没进去'");
        assert!(
            note.contains("下次进来"),
            "这一档要老实说清单还没重扫: {note}"
        );

        let (rescan, note) = reload_followup(true, &[]);
        assert!(!rescan && note.is_none(), "一次也没换 ⇒ 既不重扫也不出声");

        let both = vec!["a".to_string(), "b".to_string()];
        let (_, note) = reload_followup(true, &both);
        let note = note.expect("两格都换了");
        assert!(
            note.contains("a、b"),
            "一轮里多格变更要一次说完，不是只提第一个: {note}"
        );
        assert!(note.contains("hotreload=2"), "计数要说几格: {note}");
    }

    /// 接线腿：那条检测线程必须两头都接上，只接一头等于没接。
    ///
    /// 摘掉头一头 ⇒ 线程根本不跑（改 `.lua` 照旧要重启，而 core 那边用例全绿，
    /// 病状只在屏上）；摘掉头二头 ⇒ 线程整晚在扫目录、发请求，而没人处理，屏上还是旧代码。
    /// 判据钉的是调用形态与所在函数，只查函数名会被注释与文档顶班。
    #[test]
    fn the_hot_reload_chain_has_both_ends_in_production() {
        let src = include_str!("main.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let code: String = prod
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        let at = code
            .find("fn ensure_plugin_manager(&mut self)")
            .expect("懒创建那个函数不见了（改名或换了写法，这条守卫的锚要跟着改）");
        let rest = &code[at..];
        let body = &rest[..rest.find("\n    }").expect("懒创建的函数体切不出来")];
        assert!(
            body.contains("pm.enable_hot_reload()"),
            "管理器建起来时不再起检测线程 ⇒ 「改一行 Lua 要重启」回来了"
        );

        let at = code
            .find("fn pump_plugin_reloads(&mut self, on_plugin_page: bool)")
            .expect("GUI 线程那一问不见了");
        let rest = &code[at..];
        let body = &rest[..rest.find("\n    }").expect("那一问的函数体切不出来")];
        assert!(
            body.contains("pm.poll_reload_requests()"),
            "接了线程却不问通道 ⇒ 请求在里头堆着，屏上永远停在旧代码"
        );
        assert!(
            body.contains("if rescan {") && body.contains("self.reload_plugins()"),
            "重扫那一支不再走判定 ⇒ 回到两头各数各的"
        );

        assert!(
            code.contains("app.pump_plugin_reloads(on_plugin_page);"),
            "那一拍不再问通道了（被摘掉、或搬去了别的非 UI 线程 —— 后者更糟：`Lua` 不是 Send）"
        );
        assert!(
            code.contains("let on_plugin_page = app.view == 4;"),
            "在不在这一页不再现量当前页签 ⇒ 判定吃的是一条猜出来的输入"
        );
    }

    /// 退出时不许 join 那条检测线程：它一轮 sleep 两秒，join 会把"点了退出"拖成两秒没反应。
    ///
    /// 这条是反向腿（钉一个**不该存在**的调用）：线程不参与落盘 —— 存储那一步在
    /// `quit_app` 开头已经问过了 —— 所以没有任何一致性理由为它等这两秒。
    #[test]
    fn quitting_does_not_wait_for_the_watcher_thread() {
        let src = include_str!("main.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let at = prod
            .find("fn quit_app(hwnd: HWND)")
            .expect("quit_app 不见了（改名或换了写法，这条守卫的锚要跟着改）");
        let rest = &prod[at..];
        let body = &rest[..rest.find("\n}").expect("quit_app 的函数体切不出来")];
        assert!(
            !body.contains("disable_hot_reload"),
            "退出时 join 了检测线程 ⇒ 那一轮 sleep 是 2 秒，用户看到的是点了没反应"
        );
    }
}

/// 弹窗正文的行数计划与留高必须同源：绘制那一侧不许再自己数一遍行、画到底。
#[cfg(test)]
mod modal_content_tests {
    use super::*;

    fn modal<'a>(
        content: &'a str,
        fields: &'a [focusflow_core::plugins::FormField],
        buttons: &'a [(String, String)],
    ) -> ModalRef<'a> {
        ModalRef {
            id: "m",
            title: "统计结果",
            submit: "s",
            submit_text: "",
            cancel: "",
            content,
            open: true,
            fields,
            buttons,
        }
    }

    /// 正文行数上限按**窗口装得下多少行**算，不是一个写死的 8：
    /// 月度汇总分类一多就是十几行，8 行上限会把后半截藏掉（只显示前 8 类）。
    /// 窗口矮到装不下时才截断，且最后一行明说还有几行没显示。
    #[test]
    fn a_long_modal_body_is_capped_to_what_the_window_holds() {
        let content = (1..=20)
            .map(|i| format!("第{i}行"))
            .collect::<Vec<_>>()
            .join("\n");
        let m = modal(&content, &[], &[]);

        // 高窗口：20 行整段看得见，行数与内容区高度同一份。
        let tall = modal_geom(&m, 1200.0, 900.0);
        let tall_cap = modal_content_cap(&m, 900.0);
        assert!(tall_cap > 8, "高窗口还卡在 8 行：cap={tall_cap}");
        let rows = capped_lines(m.content, tall_cap);
        assert_eq!(rows.len(), 20, "900 高的窗口该装得下 20 行");
        let cr = tall.content.expect("有正文就该留出内容区");
        assert!(
            (cr.bottom - cr.top - rows.len() as f32 * MODAL_LINE_H).abs() < 0.01,
            "内容区高度要装得下这几行：{}",
            cr.bottom - cr.top
        );
        assert!(
            cr.bottom <= tall.dialog.bottom - 48.0,
            "正文压到了底部按钮那一排"
        );

        // 矮窗口：装不下才截，最后一行换成"还有几行"。
        let short_h = 300.0;
        let cap = modal_content_cap(&m, short_h);
        assert!(cap < 20, "夹具前提：矮窗口装不下 20 行（cap={cap}）");
        let g = modal_geom(&m, 1200.0, short_h);
        let rows = capped_lines(m.content, cap);
        assert_eq!(rows.len(), cap, "画出来的行数要等于留高的行数");
        assert_eq!(rows[0], "第1行");
        assert!(
            rows.last().unwrap().starts_with("…（还有 "),
            "{:?}",
            rows.last()
        );
        let cr = g.content.expect("有正文就该留出内容区");
        assert!(
            cr.bottom <= g.dialog.bottom - 48.0,
            "正文压到了底部按钮那一排"
        );
    }

    /// 短正文原样画，不许为了"统一"把最后一行换成提示。
    #[test]
    fn a_short_modal_body_is_drawn_verbatim() {
        let m = modal("第一行\n第二行", &[], &[]);
        let rows = capped_lines(m.content, modal_content_cap(&m, 900.0));
        assert_eq!(rows, vec!["第一行".to_string(), "第二行".to_string()]);
        let g = modal_geom(&m, 1200.0, 900.0);
        let cr = g.content.expect("两行正文也该留出内容区");
        assert!((cr.bottom - cr.top - 2.0 * MODAL_LINE_H).abs() < 0.01);
    }

    /// 自定义按钮多了/长了不许伸到「取消 / 确定」底下：命中先问那两颗，压住的按钮
    /// 画得出来却永远点不到（按下去等于取消了这次提交）。
    #[test]
    fn custom_buttons_never_run_under_the_action_row() {
        let buttons: Vec<(String, String)> = (0..6)
            .map(|i| (format!("b{i}"), format!("一个很长的自定义动作 {i}")))
            .collect();
        // 带取消的那一档最紧：取消与确定两颗的真实命中区都在右边。
        let m = ModalRef {
            cancel: "cancel_all",
            ..modal("", &[], &buttons)
        };
        let g = modal_geom(&m, 900.0, 500.0);
        assert_eq!(g.buttons.len(), 6);
        for (br, _) in &g.buttons {
            assert!(
                br.right <= g.cancel.left - 7.9,
                "自定义按钮伸进了取消/确定那一带：{br:?} vs cancel {:?}",
                g.cancel
            );
            assert!(
                br.right - br.left >= 27.9,
                "按钮被压得连一颗字都放不下：{br:?}"
            );
        }
    }

    /// 字段数量是插件给的、没有上限，而弹窗高度被窗口夹住：装不下的字段行不许
    /// 画到「取消 / 确定」底下，也不许在卡片外面留一块看不见的命中区。
    /// 最后一行改成「还有几项没显示」——与正文那条同一套口径。
    #[test]
    fn extra_fields_stop_above_the_action_row_and_say_how_many_are_hidden() {
        let fields: Vec<focusflow_core::plugins::FormField> = (0..40)
            .map(|i| focusflow_core::plugins::FormField {
                kind: "text".into(),
                field: format!("f{i}"),
                label: format!("字段{i}"),
                value: String::new(),
                options: Vec::new(),
                refresh: false,
            })
            .collect();
        let m = modal("", &fields, &[]);
        let g = modal_geom(&m, 900.0, 900.0);
        assert!(
            g.fields.len() < fields.len(),
            "四十项都排进去了：这一跑没走到截断那一支"
        );
        assert!(!g.fields.is_empty(), "九百高的窗口至少放得下几项");
        for row in &g.fields {
            assert!(
                row.bottom <= g.dialog.bottom - 48.0 + 0.01,
                "字段行画到了底部动作那一排：{row:?} vs dialog {:?}",
                g.dialog
            );
        }
        let (note_rect, note) = g.fields_note.as_ref().expect("装不下就要有说明行");
        let hidden = fields.len() - g.fields.len();
        assert!(
            note.contains(&hidden.to_string()),
            "说明要说出还有几项：{note}（实有 {hidden} 项没画）"
        );
        assert!(
            note_rect.bottom <= g.dialog.bottom - 48.0 + 0.01,
            "说明行也压到了底部按钮：{note_rect:?}"
        );
        // 命中只认 `g.fields` 里那几行：说明行不是字段，点它不该开编辑框。
        let below_drawn = g.fields.last().unwrap().bottom + 1.0;
        assert!(
            !matches!(
                modal_hit(&m, &g, g.dialog.left + 200.0, below_drawn),
                Some(ModalHit::Field(_))
            ),
            "没画出来的字段还留着看不见的点击区"
        );
    }

    /// 装得下的时候一个都不许截：这条守住上一条别把正常表单也吞掉。
    #[test]
    fn a_short_form_is_not_capped_and_gets_no_note() {
        let fields: Vec<focusflow_core::plugins::FormField> = (0..4)
            .map(|i| focusflow_core::plugins::FormField {
                kind: "text".into(),
                field: format!("f{i}"),
                label: format!("字段{i}"),
                value: String::new(),
                options: Vec::new(),
                refresh: false,
            })
            .collect();
        let m = modal("", &fields, &[]);
        let g = modal_geom(&m, 900.0, 900.0);
        assert_eq!(g.fields.len(), fields.len(), "正常表单被截了");
        assert!(g.fields_note.is_none(), "正常表单多了说明行");
    }
}

/// 视图没了的时候必须把 `plugin_open` 收掉：那一拍屏上画的是**清单**，而点击仍按
/// "视图开着"处理就会被吞 —— 点哪里都没反应，看起来像程序卡死（只剩 Esc 能出来）。
/// 两个入口都要收：点击时发现、重扫插件目录时发现。
#[cfg(test)]
mod plugin_view_lifecycle_tests {
    fn prod() -> String {
        let src = include_str!("main.rs").replace("\r\n", "\n");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        src[..cut]
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect()
    }

    #[test]
    fn a_missing_view_closes_the_open_state_in_both_doors() {
        let flat = prod();
        assert!(
            flat.contains(
                "letSome((n,view))=pvelse{self.plugin_open=None;self.reset_plugin_view_state();"
            ),
            "点击那一头：视图取不出来时没有收掉 plugin_open —— 清单会被吞掉所有点击"
        );
        assert!(
            flat.contains("if!alive{self.plugin_open=None;self.reset_plugin_view_state();}"),
            "重扫那一头：插件没了没有收掉 plugin_open —— 屏上是清单，点击全被吞"
        );
    }
}

/// 屏上「采集开着吗」必须读**在计**那一位（`listening`），不是「运行时起来过」
/// （`started`）：停用之后后者仍是 true，读错它会让设置页、托盘与悬浮窗把「停用」
/// 念成「已启用 / 已暂停」，而点一下暂停开关还能让计数偷偷恢复。
#[cfg(test)]
mod collector_state_tests {
    fn flat(src: &str) -> String {
        src.replace("\r\n", "\n")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect()
    }

    #[test]
    fn the_switch_pause_row_and_tray_read_the_listening_bit() {
        let src = include_str!("main.rs");
        let code = flat(src);
        for needle in [
            "collector_on:collect::is_listening(),",
            "letmsg=if!collect::is_listening(){",
            "ifcollect::is_listening(){tip.push_str",
            "letcpm_now=ifcollect::is_listening()&&!collect::is_paused()",
            "ifcollect::is_listening(){\"on\"}else{\"off\"}",
        ] {
            assert!(
                code.contains(needle),
                "这一处又读回了 is_started：{needle}（停用之后它仍为 true）"
            );
        }
    }

    #[test]
    fn the_floating_pill_reads_the_listening_bit() {
        let src = include_str!("floating.rs");
        let code = flat(src);
        assert!(
            code.contains("crate::collect::is_listening(),"),
            "悬浮窗把「起来过」当成「在计」"
        );
        assert!(!code.contains("crate::collect::is_started(),"));
    }
}

/// 看板页下半部的纵向布局：最小窗口下两张排行卡必须整张落在窗口里。
#[cfg(test)]
mod board_layout_tests {
    use super::board_layout;

    /// 顶栏页签在 208，KPI 那一行 78 高、下面那句话说 20 高 ⇒ 下半部的起点。
    const LINE_Y: f32 = 208.0 + 78.0 + 10.0;

    #[test]
    fn the_rank_cards_fit_inside_the_smallest_window() {
        // 最小窗口 640（客户区被撑满整窗），窗口底边那一圈留 16。
        let h = 640.0;
        let l = board_layout(h, LINE_Y);
        assert!(
            l.trend_h < 200.0,
            "最小窗口还占满 200 高的趋势：排行卡没位置了"
        );
        assert!(
            l.rank_top + l.rank_h <= h - 16.0 + 0.01,
            "排行卡画到窗口下面（这一页没有滚动）：rank_top={} rank_h={} h={h}",
            l.rank_top,
            l.rank_h
        );
        assert!(l.rank_h >= 120.0, "排行卡被压到看不懂：{}", l.rank_h);
        assert!(l.trend_h >= 60.0, "趋势图被压得画不出来：{}", l.trend_h);

        // 常态窗口照旧用满 200：别为了矮窗口把高窗口也缩了。
        let tall = board_layout(860.0, LINE_Y);
        assert_eq!(tall.trend_h, 200.0);
        assert!(tall.rank_top + tall.rank_h <= 860.0 - 16.0 + 0.01);
    }

    /// 接线：画的那一头读的是这份布局，不是又自己写一遍 200。
    #[test]
    fn the_board_page_reads_the_shared_layout() {
        let src = include_str!("main.rs");
        let needle = ["letlayout=", "board_layout(h,line_y);"].concat();
        let flat: String = src
            .replace("\r\n", "\n")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        assert!(
            flat.contains(&needle),
            "看板页没读 board_layout：排行卡的高度又各写一遍"
        );
        // 函数本体仍在生产段（第一个 #[cfg(test)] 之前）。
        let cut = src.find("#[cfg(test)]").expect("测试段起点找不到");
        assert!(
            src[..cut].contains("fn board_layout("),
            "board_layout 落到了测试段后面：接线守卫开始少扫生产码"
        );
    }
}

/// 排行表的行带：亮的区域与点得到的区域必须是同一个。
#[cfg(test)]
mod table_band_tests {
    use super::{hit, left_card, table_row_band};

    #[test]
    fn the_rank_row_band_covers_the_card_not_the_neighbours() {
        let (w, h) = (1000.0, 860.0);
        let card = left_card(w, h);
        let r = table_row_band(card, 300.0, 24.0);
        assert_eq!((r.left, r.right), (card.left, card.right));
        assert!(
            hit(r, card.left + 5.0, 310.0),
            "卡片内边距里点得到，悬停也要亮"
        );
        assert!(
            !hit(r, card.right + 20.0, 310.0),
            "指针在右侧趋势图上，不许点亮左表那一行"
        );
        assert!(!hit(r, card.left - 10.0, 310.0), "卡片外面不许点亮");
        assert!(!hit(r, card.left + 5.0, 325.0), "行间空档不归这一行");

        // 悬停与点击读同一个函数，不是各写一套坐标。
        let src = include_str!("main.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        assert!(
            prod.contains("if hit(table_row_band(card, y0, g.row_h), x, y) {"),
            "命中函数没有读那一份行带"
        );
        for call in [
            "table_row_at(count, &g, card, self.view, self.scroll, x, y)",
            "table_row_at(count, &g, r, self.view, self.scroll, x, y)",
        ] {
            assert!(
                prod.contains(call),
                "悬停或点击没有走 table_row_at：缺 {call}"
            );
        }
    }
}

/// 插件视图标题：长标题不许画到卡片外面（这块 UI 不按矩形裁切）。
#[cfg(test)]
mod plugin_view_title_tests {
    use super::{plugin_view_title_rect, rect};

    #[test]
    fn the_plugin_view_title_is_clipped_to_the_card_not_a_fixed_420() {
        let card = rect(16.0, 208.0, 900.0, 500.0);
        let back = rect(card.left + 16.0, card.top + 12.0, 64.0, 24.0);
        let t = plugin_view_title_rect(card, back);
        assert_eq!(t.left, back.right + 12.0);
        assert!(
            t.right <= card.right - 16.0 + 0.01,
            "标题格超出卡片右边：{t:?}"
        );
        assert!(
            t.right - t.left >= 400.0,
            "900 宽的卡片给标题的格子太窄：{}",
            t.right - t.left
        );

        // 接线：画那一头按这一格夹，而不是把长文本原样画出去。
        let src = include_str!("main.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        assert!(
            prod.contains("let title_rect = plugin_view_title_rect(card, back);"),
            "标题格没抽成共用的那一份"
        );
        assert!(
            prod.contains("&p.clip(&title, &p.fmt.body, title_rect.right - title_rect.left)"),
            "插件视图标题没夹宽：长标题会画到卡片外"
        );
    }
}

/// 空下拉：一块空白小卡在屏上与「点坏了」分不开，要写一句话。
#[cfg(test)]
mod select_menu_tests {
    use super::{empty_select_note, rect, select_menu_geom};

    #[test]
    fn an_empty_select_menu_says_so_instead_of_drawing_a_blank_card() {
        assert_eq!(empty_select_note(0), Some("没有可选项"));
        assert_eq!(empty_select_note(1), None, "有选项时不该多出这一句");

        // 空菜单那一格装得下那行字：菜单高 = 1×24 + 8，文字 18 高、顶部留 4。
        let (menu, items, _) =
            select_menu_geom(rect(100.0, 100.0, 260.0, 24.0), 0, 0, 1200.0, 860.0);
        assert!(items.is_empty(), "空 options 不该排出任何选项格");
        assert!(
            menu.bottom - menu.top >= 4.0 + 18.0,
            "空菜单那一格装不下一行字：{menu:?}"
        );

        // 接线：画的那一头用这句空态（函数名在别处出现不算）。
        let src = include_str!("main.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        assert!(
            prod.contains("if let Some(note) = empty_select_note(sel.options.len()) {"),
            "空下拉又画成一块空白卡"
        );
    }
}

/// 这一轮体验改动的算术与文案：点击区、值列、滚动条、确认框、截断提示、插件格子。
#[cfg(test)]
mod ux_polish_tests {
    use super::*;

    /// 独立的点击区一律 ≥24 DIP；行内那几颗受行高限制，钉它们不越出行。
    #[test]
    fn standalone_controls_are_at_least_24_dip_tall() {
        for (name, r) in [
            ("视图页签", view_tabs()[0]),
            (
                "排行筛选胶囊",
                rank_pills(rect(0.0, 208.0, 900.0, 600.0))[0],
            ),
            ("周期药丸", period_pills()[0].1),
        ] {
            assert!(
                r.bottom - r.top >= 24.0,
                "{name} 只有 {} 高，鼠标点得手酸",
                r.bottom - r.top
            );
        }
        // 常量比较在编译期就能算出来，但这条腿要的是"别被改回去"。
        #[allow(clippy::assertions_on_constants)]
        {
            assert!(PV_TABLE_ROW_H >= 24.0, "插件表格行太矮");
        }
        for (name, r) in [
            ("授权", plugin_grant_rect(1000.0, 0.0)),
            ("详情", plugin_detail_rect(1000.0, 0.0)),
            ("热键清空", hotkey_btn_rect(rect(700.0, 260.0, 150.0, 26.0))),
        ] {
            assert!(
                r.bottom - r.top >= 24.0,
                "{name} 只有 {} 高",
                r.bottom - r.top
            );
        }
        // 行内按钮：高度只许占满那一行，不许压到邻行。
        let card = rect(0.0, 0.0, 900.0, 600.0);
        let g = table_geom(card);
        for (name, r) in [
            ("改名", rename_button(&g, g.list_top)),
            ("恢复", restore_button(&g, g.list_top)),
        ] {
            assert!(
                r.bottom - r.top >= 22.0,
                "{name} 太矮：{}",
                r.bottom - r.top
            );
            assert!(
                r.top >= g.list_top && r.bottom <= g.list_top + g.row_h,
                "{name} 越出了自己那一行"
            );
        }
    }

    /// 值列右锚：宽窗贴着卡片右边，窄窗先保住说明那一列。
    #[test]
    fn the_settings_value_column_follows_the_window_width() {
        let a = settings_geom(960.0, 860.0);
        let b = settings_geom(1200.0, 860.0);
        assert_eq!(a.value_x, 960.0 - 32.0 - a.value_w - 24.0);
        assert_eq!(b.value_x, 1200.0 - 32.0 - b.value_w - 24.0);
        assert!(
            a.value_x + a.value_w <= 960.0 - 32.0,
            "960 下值框越过卡片右边"
        );
        assert!(b.value_x + b.value_w <= 1200.0 - 32.0);
        let r = settings_row_rect(&a, 960.0, a.list_top);
        assert!(settings_note_w(&a, r) >= 120.0, "说明那一列被挤没了");
    }

    /// 设置页分组标题只画不点：命中要跳过它，否则标题会亮起来。
    #[test]
    fn settings_row_hit_skips_group_headers() {
        let g = settings_geom(960.0, 860.0);
        let items = vec![SItem::Header("数据操作"), SItem::Action(0)];
        let x = g.body_left + 10.0;
        assert!(
            settings_row_at(&items, &g, 960.0, 0, x, g.list_top + g.row_h * 0.5).is_none(),
            "分组标题不该算控件"
        );
        assert_eq!(
            settings_row_at(&items, &g, 960.0, 0, x, g.list_top + g.row_h * 1.5),
            Some((1, 1)),
            "标题下面那一行要能命中，且屏上行号是 1"
        );
    }

    /// 滑块比例、最短长度与"装得下不画"。
    #[test]
    fn the_scrollbar_thumb_is_a_fraction_of_the_track() {
        let track = rect(100.0, 0.0, 6.0, 200.0);
        assert!(scrollbar_thumb(track, 5, 10, 0).is_none(), "装得下不该画");
        let top = scrollbar_thumb(track, 40, 10, 0).expect("该画");
        assert!((top.bottom - top.top - 50.0).abs() < 0.01);
        assert_eq!(top.top, 0.0, "偏移 0 时滑块贴顶");
        let bottom = scrollbar_thumb(track, 40, 10, 30).expect("该画");
        assert!((bottom.bottom - 200.0).abs() < 0.01, "滚到底时滑块贴底");
        let tiny = scrollbar_thumb(track, 1000, 10, 0).expect("该画");
        assert!((tiny.bottom - tiny.top - 16.0).abs() < 0.01, "最短 16");
    }

    /// 拖滑块换算偏移：夹在 [0, 总数-一屏] 里，按下点的那一点手感不跳。
    #[test]
    fn dragging_the_thumb_maps_to_rows_and_stays_in_range() {
        let track = rect(0.0, 0.0, 6.0, 200.0);
        let d = ScrollDrag {
            lane: ScrollLane::Table,
            track,
            thumb_h: 20.0,
            grab: 10.0,
            total: 100,
            visible: 10,
        };
        assert_eq!(scroll_offset_for_y(&d, 10.0), 0, "按在滑块原处不该滚");
        assert_eq!(scroll_offset_for_y(&d, 190.0), 90, "拖到底 = 最大偏移");
        assert_eq!(scroll_offset_for_y(&d, -500.0), 0, "拖出上界要夹住");
        assert_eq!(scroll_offset_for_y(&d, 5000.0), 90, "拖出下界要夹住");
    }

    /// 清理确认：取消在左、命中判定与默认结局一致。
    #[test]
    fn the_cleanup_confirmation_defaults_to_cancel() {
        let (dialog, buttons) = cleanup_prompt_geometry(1200.0, 860.0);
        assert!(buttons[0].left < buttons[1].left, "取消要在左边");
        for b in buttons {
            assert!(
                b.left >= dialog.left && b.right <= dialog.right,
                "按钮跑出框"
            );
            assert!(b.bottom <= dialog.bottom, "按钮跑出框");
        }
        let mid = |r: D2D_RECT_F| ((r.left + r.right) / 2.0, (r.top + r.bottom) / 2.0);
        let (cx, cy) = mid(buttons[0]);
        assert!(
            !cleanup_choice_at(dialog, buttons, cx, cy),
            "点取消不算执行"
        );
        let (cx, cy) = mid(buttons[1]);
        assert!(
            cleanup_choice_at(dialog, buttons, cx, cy),
            "点危险按钮才算执行"
        );
        assert!(
            !cleanup_choice_at(dialog, buttons, dialog.left + 8.0, dialog.top + 8.0),
            "框内空白什么都不做"
        );
        assert!(
            !cleanup_choice_at(dialog, buttons, 0.0, 0.0),
            "点遮罩 = 取消"
        );
    }

    /// 应用榜截断时把"只列了前 N 个"摆出来；别的屏不借这句。
    #[test]
    fn a_truncated_app_ranking_says_how_many_were_left_out() {
        let mut s = Snapshot {
            data_home: "C:\\data".to_string(),
            apps: vec![("a".to_string(), 10)],
            app_rows_total: 1,
            ..Default::default()
        };
        let foot = table_footer_text(&s, "应用时长", 1);
        assert!(!foot.contains("只列了前"), "没截断不该多一句: {foot}");
        s.app_rows_total = 250;
        let foot = table_footer_text(&s, "应用时长", 1);
        assert!(
            foot.contains("只列了前 1 个") && foot.contains("250"),
            "截断要同时念两个数: {foot}"
        );
        let keys = table_footer_text(&s, "按键统计", 0);
        assert!(!keys.contains("只列了前"), "键鼠那一屏不借这句: {keys}");
    }

    /// 插件视图里"亮着的格子"必须正好是"点得动的格子"。
    #[test]
    fn plugin_targets_are_the_parts_that_claim_clicks() {
        use focusflow_core::plugins::{PluginView, Widget};
        let view = PluginView {
            title: "t".to_string(),
            widgets: vec![
                Widget::Button {
                    id: "go".to_string(),
                    text: "去".to_string(),
                    disabled: false,
                    modal: None,
                    sel: false,
                    group: String::new(),
                },
                Widget::Table {
                    headers: vec!["a".to_string()],
                    rows: vec![vec!["x".to_string()]],
                    ids: vec!["7".to_string()],
                    actions: vec![("edit_".to_string(), "编辑".to_string())],
                    group: String::new(),
                    onselect: String::new(),
                },
            ],
        };
        let clip = rect(0.0, 0.0, 800.0, 600.0);
        let ops = plugin_view_ops(&view, clip, 0.0, &HashMap::new());
        let targets = pv_targets_of(&ops);
        assert!(targets.iter().any(|t| !t.row), "按钮/行内按钮没进目标表");
        assert!(targets.iter().any(|t| t.row), "表格行没进目标表");
        for t in &targets {
            let cx = (t.rect.left + t.rect.right) / 2.0;
            let cy = (t.rect.top + t.rect.bottom) / 2.0;
            assert!(
                pv_claiming_op_in(&ops, clip, cx, cy).is_some(),
                "亮着的格子点下去没人认领: {t:?}"
            );
        }
        assert!(
            pv_target_on(&targets, clip, -50.0, 10.0).is_none(),
            "可见区之外不该亮"
        );
    }

    /// 键盘焦点：Tab 走整张表，上下键只在同组里绕。
    #[test]
    fn keyboard_focus_moves_inside_a_group_or_across_the_whole_page() {
        let groups = [0u8, 0, 1, 2, 2, 2];
        assert_eq!(kbd_next(&groups, None, 1, false), Some(0));
        assert_eq!(kbd_next(&groups, None, -1, false), Some(5));
        assert_eq!(
            kbd_next(&groups, None, 1, true),
            Some(3),
            "上下键从本页开始，不从周期药丸开始"
        );
        assert_eq!(
            kbd_next(&[0u8, 1], None, 1, true),
            None,
            "本页没有可停的格子时上下键什么都不做"
        );
        assert_eq!(
            kbd_next(&groups, Some(5), 1, false),
            Some(0),
            "Tab 绕回开头"
        );
        assert_eq!(kbd_next(&groups, Some(0), -1, false), Some(5));
        assert_eq!(kbd_next(&groups, Some(0), -1, true), Some(1), "本组绕回");
        assert_eq!(kbd_next(&groups, Some(3), 1, true), Some(4));
        assert_eq!(
            kbd_next(&groups, Some(5), 1, true),
            Some(3),
            "列表末行绕回本组首行"
        );
        assert_eq!(kbd_next(&groups, Some(2), 1, true), Some(2), "组里只有一格");
        assert_eq!(kbd_next(&[], None, 1, false), None);
    }

    /// 键盘与右键复制的接线：光有函数名不算，得真的挂在消息循环上。
    #[test]
    fn keyboard_and_copy_are_wired_to_the_message_loop() {
        let src = include_str!("main.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        for needle in [
            "app.handle_nav_key(hwnd, w.0)",
            "app.focus_search(hwnd)",
            "copy_text_to_clipboard(hwnd, &text)",
            "note_tooltip(tip, vs, name_cell, &row.name, clipped != row.name)",
            "draw_tooltip(p, w, h, *cell, text)",
        ] {
            assert!(prod.contains(needle), "缺 {needle}");
        }
    }
}
