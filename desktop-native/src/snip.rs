//! 截图：热键一按就**冻结整块虚拟屏**，在冻结下来的画面上拖选/点选，
//! 松手即存（`[hotkey] snip`）或就地标注后存盘 + 进剪贴板 + 贴图（`[hotkey] snip_annotate`）。
//!
//! 为什么必须先冻结再选区（而不是"选好之后再抓屏"）：
//! * 悬停才出现的东西（菜单、tooltip、右键菜单）在第二次抓屏时早就没了；
//! * 蒙版压着的画面与最终出图的画面必须同源，否则用户选的不是他拿到的那张；
//! * 出图从冻结缓冲里逐行裁，不再依赖"松手那一刻屏幕恰好没变"。
//!
//! 选区一旦定下（标注模式），覆盖层**不退**：八向手柄改大小、方向键挪 1 像素、
//! Shift+方向键改 1 像素、工具条贴在选区下方，画上去的每一笔实时压在真图上，
//! 撤销/重做都有。所有裁决（几何、压暗、马赛克、工具条排版、命名去重）都是纯函数，
//! 脱离窗口就能被用例断言。

use std::sync::Mutex;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateCompatibleDC, CreateDIBSection, CreateFontW, CreatePen, CreateRectRgn,
    CreateSolidBrush, DeleteDC, DeleteObject, EndPaint, ExtCreatePen, GetClipRgn, GetDC,
    GetStockObject, GetTextExtentPoint32W, InvalidateRect, LineTo, MoveToEx, Rectangle, ReleaseDC,
    SelectClipRgn, SelectObject, SetBkMode, SetDIBitsToDevice, SetTextColor, TextOutW, BITMAPINFO,
    BS_SOLID, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH,
    DIB_RGB_COLORS, FW_NORMAL, HDC, HPEN, LOGBRUSH, NULL_BRUSH, OUT_DEFAULT_PRECIS, PAINTSTRUCT,
    PS_ENDCAP_ROUND, PS_GEOMETRIC, PS_JOIN_ROUND, PS_SOLID, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, VK_CONTROL, VK_SHIFT,
};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::{floating, open_pin, tracing_note, WM_EDIT_DONE};

/// 蒙版压暗系数：选区外每个通道乘 `VEIL_MUL/255`。255 = 不压暗，0 = 全黑。
const VEIL_MUL: u32 = 140;
/// 马赛克一格多大（物理像素）。与旧 Webview 版同一把尺子。
const MOSAIC_CELL: i32 = 10;
/// 松手至少要拖出这么大才算"框了一个区域"，否则按"单击选窗口"那条手势处理。
const MIN_DRAG: i32 = 4;
/// 选区/一笔的最小尺寸。
const MIN_SEL: i32 = 2;
/// 八向手柄的命中边长（中心向四周各扩一半）。
const HANDLE_PX: i32 = 8;
/// 工具条一行的高、按钮间距、组间距、条内边距。
const BAR_ROW_H: i32 = 26;
const BAR_GAP: i32 = 6;
const BAR_GROUP_GAP: i32 = 14;
const BAR_PAD: i32 = 6;
/// 覆盖层挂满这么多毫秒就自己退（截图是"顺手按一次"的工具，忘了它整张桌面会一直被压住）。
const SNIP_TIMEOUT_MS: u32 = 180_000;
/// 那个定时器 id（`0x534E` = 'SN'）。验收脚本按同一个数投一条 WM_TIMER 过来催它响。
const SNIP_TIMEOUT_TIMER: usize = 0x534E;
/// 覆盖层的窗口类名：验收夹具按它等窗口，改名要连着改脚本。
const OVERLAY_CLASS: &str = "FocusFlowSnipOverlay";
/// 选区边框与高亮用的橙色（旧 Webview 版同一色，#F79C5B）。
const ACCENT: u32 = rgb(247, 156, 91);
/// 工具条底色 / 按钮底色 / 按钮描边。
const BAR_BG: u32 = rgb(26, 20, 18);
const BTN_BG: u32 = rgb(53, 46, 46);
const BTN_EDGE: u32 = rgb(80, 69, 64);

/// RGB 拼成 GDI 的 COLORREF。
///
/// COLORREF 的布局是 `0x00BBGGRR`（最低字节是红），按 `#RRGGBB` 的直觉去写就会把红蓝对调 ——
/// 红色画出来是蓝色，而"这条线是橙色的"这种说法在代码里看不出来，只有像素能拆穿。
const fn rgb(r: u32, g: u32, b: u32) -> u32 {
    (b << 16) | (g << 8) | r
}

/// 一条标注的画笔参数。颜色/粗细/字号必须**跟着每一笔存**，否则事后换调色板会把已画的笔迹染色。
#[derive(Clone, Copy, Debug)]
pub struct Style {
    /// GDI COLORREF（0x00BBGGRR）
    pub rgb: u32,
    /// 线宽（物理像素）
    pub w: i32,
    /// 字号（物理像素，用作 CreateFontW 的高度绝对值）
    pub font: i32,
}

/// 标注的形状。坐标是**选区本地坐标**（0..选区宽 / 0..选区高）：
/// 选区事后被搬动或改大小，笔迹跟着图走，落盘时也不必再换算。
#[derive(Clone, Debug)]
pub enum Shape {
    /// 空心方框 (x, y, w, h)
    Rect(i32, i32, i32, i32),
    /// 箭头 (x0, y0, x1, y1)
    Arrow(i32, i32, i32, i32),
    /// 画笔：一条折线
    Pen(Vec<(i32, i32)>),
    /// 色块：实心填充 (x, y, w, h)
    Block(i32, i32, i32, i32),
    /// 马赛克：对**原始底图**那一块做格子平均 (x, y, w, h)
    Mosaic(i32, i32, i32, i32),
    /// 文字 (x, y, 文本)
    Text(i32, i32, String),
}

#[derive(Clone, Debug)]
pub struct Mark {
    pub shape: Shape,
    pub style: Style,
}

/// 正在拖的是哪一种。`Mark` 是"在选区里落一笔"，不是新的选区。
#[derive(Clone, Copy, Debug, PartialEq)]
enum Drag {
    /// 重新框一个选区
    New,
    /// 八向手柄之一（0=左上 1=上 2=右上 3=右 4=右下 5=下 6=左下 7=左）
    Handle(u8),
    /// 标注工具正在画
    Mark,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    /// 还没有选区：悬停预览窗口边界，拖出即选区
    Select,
    /// 选区已定（标注模式）：手柄、工具条、落笔都在这一个阶段
    Adjust,
}

/// 覆盖层上"单击选窗口"看到的一条候选：Z 序里的一个顶层窗口，只带判得动的那三件事。
#[derive(Clone, Copy, Debug)]
struct Candidate {
    pid: u32,
    visible: bool,
    /// left, top, right, bottom（屏幕坐标）
    rect: (i32, i32, i32, i32),
}

/// 工具条上的一颗按钮（覆盖层客户区坐标）。
#[derive(Clone, Debug)]
pub struct BarBtn {
    /// ASCII 的稳定 id：验收脚本按它找按钮，中文标签改了脚本也不会瞎。
    pub id: &'static str,
    pub label: &'static str,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// 工具条定义（顺序 = 屏上顺序）。第四项 `true` = 与上一组之间多留一段间隔。
const BAR_SPECS: &[(&str, &str, i32, bool)] = &[
    ("rect", "方框", 44, true),
    ("arrow", "箭头", 44, false),
    ("pen", "画笔", 44, false),
    ("mosaic", "马赛克", 56, false),
    ("block", "色块", 44, false),
    ("text", "文字", 44, false),
    ("c1", "红", 30, true),
    ("c2", "黄", 30, false),
    ("c3", "黑", 30, false),
    ("c4", "白", 30, false),
    ("w1", "细", 32, true),
    ("w2", "粗", 32, false),
    ("f1", "小", 32, true),
    ("f2", "中", 32, false),
    ("f3", "大", 32, false),
    ("undo", "撤销", 52, true),
    ("redo", "重做", 52, false),
    ("pin", "贴图", 52, false),
    ("save", "完成", 52, false),
    ("cancel", "取消", 52, false),
];

/// 调色板（与旧 Webview 版同一四色，默认红）。
const PALETTE: [(u32, &str); 4] = [
    (rgb(255, 45, 45), "c1"),
    (rgb(255, 212, 0), "c2"),
    (rgb(0, 0, 0), "c3"),
    (rgb(255, 255, 255), "c4"),
];
/// 线宽两档（细/粗）。
const WIDTHS: [(i32, &str); 2] = [(3, "w1"), (7, "w2")];
/// 字号三档（物理像素高）。
const FONTS: [(i32, &str); 3] = [(18, "f1"), (30, "f2"), (48, "f3")];
/// 工具 id -> `tool` 编号。
const TOOLS: [&str; 6] = ["rect", "arrow", "pen", "mosaic", "block", "text"];

/// 一次截图的全部状态（同一时刻只允许一个覆盖层，所以是一个静态量）。
pub struct SnipState {
    /// 覆盖层在不在：`start` 用它挡住"热键连按"，`WM_DESTROY` 负责复位。
    active: bool,
    /// 这一次松手后进标注还是直接存。
    annotate: bool,
    phase: Phase,
    /// 覆盖层窗口在屏幕上的原点（= 建它那一刻的虚拟屏原点）：客户区坐标 + 它 = 屏幕坐标。
    origin: (i32, i32),
    /// 冻结下来的整块虚拟屏（BGRA、自上而下）与它的尺寸；压暗后的第二份用于蒙版。
    base: Vec<u8>,
    dim: Vec<u8>,
    base_w: i32,
    base_h: i32,
    /// 选区（覆盖层客户区坐标，与 base 的像素 1:1）。
    sel: (i32, i32, i32, i32),
    /// 正在拖：拖法 + 起点；`cur` 是当前点，`pen_pts` 是画笔已走过的点（选区本地坐标）。
    drag: Option<(Drag, i32, i32)>,
    cur: (i32, i32),
    pen_pts: Vec<(i32, i32)>,
    /// 悬停命中的窗口矩形（Select 阶段的亮区预览）。
    hover: Option<(i32, i32, i32, i32)>,
    /// 冻结那一刻的窗口清单（Z 序顶→底）。一次成图，鼠标移动不再 EnumWindows。
    hits: Vec<Candidate>,
    marks: Vec<Mark>,
    redo: Vec<Mark>,
    /// 当前工具（[`TOOLS`] 下标）。
    tool: u8,
    color: u32,
    width: i32,
    font: i32,
    /// 文字标注的原生 EDIT 子窗口（存 isize：静态量要 Send）与落点（选区本地坐标）。
    text_edit: isize,
    text_at: (i32, i32),
    /// 当前覆盖层的句柄数值（静态量要 Send）：再按一次标注热键要靠它放弃在飞的这一次。
    overlay: isize,
    /// 冻结时把悬浮窗藏了：退出时要按原样还回去。
    pill_was: bool,
}

const fn default_state() -> SnipState {
    SnipState {
        active: false,
        annotate: false,
        phase: Phase::Select,
        origin: (0, 0),
        base: Vec::new(),
        dim: Vec::new(),
        base_w: 0,
        base_h: 0,
        sel: (0, 0, 0, 0),
        drag: None,
        cur: (0, 0),
        pen_pts: Vec::new(),
        hover: None,
        hits: Vec::new(),
        marks: Vec::new(),
        redo: Vec::new(),
        tool: 0,
        color: PALETTE[0].0,
        width: WIDTHS[0].0,
        font: FONTS[1].0,
        text_edit: 0,
        text_at: (0, 0),
        overlay: 0,
        pill_was: false,
    }
}

static SNIP: Mutex<SnipState> = Mutex::new(default_state());
static SNIP_HITS: Mutex<Vec<Candidate>> = Mutex::new(Vec::new());

// ===== 纯几何 / 像素裁决 =====

/// 任意两点折成「左上 + 右下」，返回 (l, t, r, b)。
pub fn norm_rect(x0: i32, y0: i32, x1: i32, y1: i32) -> (i32, i32, i32, i32) {
    (x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1))
}

/// 把 (l,t,r,b) 夹进 [0..w, 0..h]，并保证不退化成负面积。
pub fn clamp_rect(rc: (i32, i32, i32, i32), w: i32, h: i32) -> (i32, i32, i32, i32) {
    let l = rc.0.clamp(0, w);
    let t = rc.1.clamp(0, h);
    let r = rc.2.clamp(0, w);
    let b = rc.3.clamp(0, h);
    (l.min(r), t.min(b), r.max(l), b.max(t))
}

/// 八个手柄的中心（顺序与 [`Drag::Handle`] 的编号一致）。
pub fn handle_points(rc: (i32, i32, i32, i32)) -> [(i32, i32); 8] {
    let (l, t, r, b) = rc;
    let (mx, my) = ((l + r) / 2, (t + b) / 2);
    [
        (l, t),
        (mx, t),
        (r, t),
        (r, my),
        (r, b),
        (mx, b),
        (l, b),
        (l, my),
    ]
}

/// 命中哪个手柄（没有就 `None`）。只在选区已定的阶段用。
pub fn hit_handle(rc: (i32, i32, i32, i32), x: i32, y: i32) -> Option<u8> {
    let half = HANDLE_PX / 2;
    for (i, (hx, hy)) in handle_points(rc).iter().enumerate() {
        if x >= hx - half && x <= hx + half && y >= hy - half && y <= hy + half {
            return Some(i as u8);
        }
    }
    None
}

/// 点在选区里吗（边界上算里面）。
pub fn inside_rect(rc: (i32, i32, i32, i32), x: i32, y: i32) -> bool {
    x >= rc.0 && x <= rc.2 && y >= rc.1 && y <= rc.3
}

/// 拖某个手柄到 (nx, ny) 之后的新选区：只动该动的那条边，夹住屏幕，不许翻面也不许缩成 0。
pub fn resize_rect(
    rc: (i32, i32, i32, i32),
    idx: u8,
    nx: i32,
    ny: i32,
    w: i32,
    h: i32,
) -> (i32, i32, i32, i32) {
    let (mut l, mut t, mut r, mut b) = rc;
    let min = MIN_SEL;
    match idx {
        0 => {
            l = nx.min(r - min);
            t = ny.min(b - min);
        }
        1 => t = ny.min(b - min),
        2 => {
            r = nx.max(l + min);
            t = ny.min(b - min);
        }
        3 => r = nx.max(l + min),
        4 => {
            r = nx.max(l + min);
            b = ny.max(t + min);
        }
        5 => b = ny.max(t + min),
        6 => {
            l = nx.min(r - min);
            b = ny.max(t + min);
        }
        _ => l = nx.min(r - min),
    }
    clamp_rect((l, t, r, b), w, h)
}

/// 整体搬动选区（保持大小），夹在屏幕里；选区比屏幕还宽时退回贴左/贴顶。
pub fn move_rect(
    rc: (i32, i32, i32, i32),
    dx: i32,
    dy: i32,
    w: i32,
    h: i32,
) -> (i32, i32, i32, i32) {
    let sw = rc.2 - rc.0;
    let sh = rc.3 - rc.1;
    let l = if w >= sw {
        (rc.0 + dx).clamp(0, w - sw)
    } else {
        0
    };
    let t = if h >= sh {
        (rc.1 + dy).clamp(0, h - sh)
    } else {
        0
    };
    (l, t, l + sw, t + sh)
}

/// 方向键挪一步。
pub fn nudge_rect(
    rc: (i32, i32, i32, i32),
    dx: i32,
    dy: i32,
    w: i32,
    h: i32,
) -> (i32, i32, i32, i32) {
    move_rect(rc, dx, dy, w, h)
}

/// Shift+方向键改大小：只动右下两条边，夹屏幕、夹最小尺寸。
pub fn grow_rect(
    rc: (i32, i32, i32, i32),
    dx: i32,
    dy: i32,
    w: i32,
    h: i32,
) -> (i32, i32, i32, i32) {
    resize_rect(
        rc,
        4,
        (rc.2 + dx).clamp(0, w),
        (rc.3 + dy).clamp(0, h),
        w,
        h,
    )
}

/// 蒙版：整幅 BGRA 每通道乘 [`VEIL_MUL`]，alpha 钉成 255。
///
/// 单独写成纯函数是因为"选区里外看到的是不是同一张图"这件事只能在这里判：
/// 压暗的一份铺满全屏，亮区再把原图 clip 出来，两处用的是同一批像素。
pub fn dim_bgra(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len());
    for px in src.as_chunks::<4>().0 {
        out.push((px[0] as u32 * VEIL_MUL / 255) as u8);
        out.push((px[1] as u32 * VEIL_MUL / 255) as u8);
        out.push((px[2] as u32 * VEIL_MUL / 255) as u8);
        out.push(255);
    }
    out
}

/// 把 src 里 (x,y,w,h) 这一块按**绝对**格网做格子平均，返回 w*h*4 的 BGRA（越界填 0）。
///
/// 格子锚在图像原点而不是这一块的左上角：两块马赛克挨着时缝不能错位，
/// 而"块起点不整格"是唯一能测出锚点的形状。
#[allow(clippy::too_many_arguments)] // 源图 + 源尺寸 + 矩形 + 格宽：拆开传反而要在调用点各抄一遍
pub fn mosaic_block(
    src: &[u8],
    src_w: i32,
    src_h: i32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    cell: i32,
) -> Vec<u8> {
    let mut out = vec![0u8; (w.max(0) as usize) * (h.max(0) as usize) * 4];
    if w <= 0 || h <= 0 || cell <= 0 || src_w <= 0 || src_h <= 0 || out.is_empty() {
        return out;
    }
    let mut cy = (y.max(0) / cell) * cell;
    while cy < y + h {
        let mut cx = (x.max(0) / cell) * cell;
        while cx < x + w {
            let (gx0, gy0) = (cx.max(x).max(0), cy.max(y).max(0));
            let (gx1, gy1) = (
                (cx + cell).min(x + w).min(src_w),
                (cy + cell).min(y + h).min(src_h),
            );
            if gx1 > gx0 && gy1 > gy0 {
                let (mut sb, mut sg, mut sr, mut n) = (0u64, 0u64, 0u64, 0u64);
                for sy in gy0..gy1 {
                    for sx in gx0..gx1 {
                        let i = (sy as usize * src_w as usize + sx as usize) * 4;
                        if i + 3 < src.len() {
                            sb += src[i] as u64;
                            sg += src[i + 1] as u64;
                            sr += src[i + 2] as u64;
                            n += 1;
                        }
                    }
                }
                if n > 0 {
                    let (ab, ag, ar) = (
                        sb.checked_div(n).unwrap_or(0) as u8,
                        sg.checked_div(n).unwrap_or(0) as u8,
                        sr.checked_div(n).unwrap_or(0) as u8,
                    );
                    for py in gy0..gy1 {
                        for px in gx0..gx1 {
                            let oi = (((py - y) as usize) * w as usize + (px - x) as usize) * 4;
                            if oi + 3 < out.len() {
                                out[oi] = ab;
                                out[oi + 1] = ag;
                                out[oi + 2] = ar;
                                out[oi + 3] = 255;
                            }
                        }
                    }
                }
            }
            cx += cell;
        }
        cy += cell;
    }
    out
}

/// 从整幅缓冲里逐行裁一块；越界判失败（`None`），不许静默少裁几行。
pub fn crop_rect(src: &[u8], src_w: i32, x: i32, y: i32, w: i32, h: i32) -> Option<Vec<u8>> {
    if src_w <= 0 || w <= 0 || h <= 0 || x < 0 || y < 0 {
        return None;
    }
    let row_bytes = src_w as usize * 4;
    if row_bytes == 0 || !src.len().is_multiple_of(row_bytes) {
        return None;
    }
    let src_h = src.len() / row_bytes;
    if x + w > src_w || y + h > src_h as i32 {
        return None;
    }
    let mut out = Vec::with_capacity((w as usize) * (h as usize) * 4);
    for row in 0..h as usize {
        let from = ((y as usize + row) * src_w as usize + x as usize) * 4;
        out.extend_from_slice(&src[from..from + w as usize * 4]);
    }
    Some(out)
}

/// 色块 = 一块纯色像素（w×h 个，一个不多一个不少）。
///
/// 不走 GDI 的 `Rectangle`：一是粗笔会往块外溢 1 像素（打码盖到旁边的桌面上），
/// 二是实测在 DIB 段里它只落下了左上角那一个像素 —— 纯色填充本来就是纯像素运算，没必要绕。
pub fn fill_block(w: i32, h: i32, color: u32) -> Vec<u8> {
    let (b, g, r) = (
        ((color >> 16) & 0xFF) as u8,
        ((color >> 8) & 0xFF) as u8,
        (color & 0xFF) as u8,
    );
    let mut out = vec![0u8; (w.max(0) as usize) * (h.max(0) as usize) * 4];
    for px in out.as_chunks_mut::<4>().0 {
        px[0] = b;
        px[1] = g;
        px[2] = r;
        px[3] = 255;
    }
    out
}

/// 工具条排版：按最大宽度流式换行，返回**相对条内原点**的按钮与整条的宽、高。
pub fn bar_flow(max_w: i32) -> (Vec<BarBtn>, i32, i32) {
    let mut btns: Vec<BarBtn> = Vec::new();
    let (mut x, mut row, mut row_w, mut total_w) = (0i32, 0i32, 0i32, 0i32);
    for (i, (id, label, w, group)) in BAR_SPECS.iter().enumerate() {
        if i > 0 {
            x += if *group { BAR_GROUP_GAP } else { BAR_GAP };
        }
        if x > 0 && x + w > max_w {
            total_w = total_w.max(row_w);
            row += 1;
            x = 0;
        }
        btns.push(BarBtn {
            id,
            label,
            x,
            y: row * (BAR_ROW_H + BAR_GAP),
            w: *w,
            h: BAR_ROW_H,
        });
        x += *w;
        row_w = x;
    }
    total_w = total_w.max(row_w);
    let rows = row + 1;
    (btns, total_w, rows * BAR_ROW_H + (rows - 1) * BAR_GAP)
}

/// 工具条整条的落点：默认贴选区下方 6 像素；底下放不下翻到选区上方，
/// 再把横向夹回屏幕（四周各留 4 像素）。
pub fn bar_placement(
    sel: (i32, i32, i32, i32),
    screen_w: i32,
    screen_h: i32,
    bar_w: i32,
    bar_h: i32,
) -> (i32, i32) {
    let mut y = sel.3 + 6;
    if y + bar_h > screen_h - 4 {
        y = (sel.1 - bar_h - 6).max(4);
    }
    if y < 4 {
        y = 4;
    }
    let mut x = sel.0;
    if x + bar_w > screen_w - 4 {
        x = (screen_w - 4 - bar_w).max(0);
    }
    if x < 4 {
        x = 4;
    }
    (x, y)
}

/// 工具条按钮（覆盖层客户区绝对坐标）+ 整条的外框。
pub fn bar_rects(
    sel: (i32, i32, i32, i32),
    screen_w: i32,
    screen_h: i32,
) -> (Vec<BarBtn>, (i32, i32, i32, i32)) {
    let min_screen = (screen_w - 8).max(120);
    let max_w = ((sel.2 - sel.0) * 2).clamp(360, min_screen.max(360));
    let (mut btns, bw, bh) = bar_flow(max_w);
    let (ox, oy) = bar_placement(sel, screen_w, screen_h, bw + 2 * BAR_PAD, bh + 2 * BAR_PAD);
    for b in btns.iter_mut() {
        b.x += ox + BAR_PAD;
        b.y += oy + BAR_PAD;
    }
    (btns, (ox, oy, bw + 2 * BAR_PAD, bh + 2 * BAR_PAD))
}

fn hit_btn(btns: &[BarBtn], x: i32, y: i32) -> Option<usize> {
    btns.iter()
        .position(|b| x >= b.x && x < b.x + b.w && y >= b.y && y < b.y + b.h)
}

/// 出图文件名：`snip_<时间戳>_<宽>x<高>.png`，同秒连按靠 `_2` 后缀区分。
///
/// 时间戳与目录清单都从外面喂进来，所以这条判据不依赖真实时钟和真实磁盘。
/// 同秒不区分就是**覆盖**：连按两次只剩一张，那正是旧命名（只到秒）丢图的方式。
pub fn out_name(ts: &str, w: u32, h: u32, listing: &[String]) -> String {
    let base = focusflow_core::capture::name_stem(ts, w, h);
    focusflow_core::capture::unique_name(&base, "png", listing)
}

/// 拖笔的起点/当前点折成选区本地坐标里的一笔；太小（或太近）返回 `None` 不落 mark。
pub fn shape_from_drag(
    tool: u8,
    start: (i32, i32),
    cur: (i32, i32),
    pen_pts: &[(i32, i32)],
    sel: (i32, i32, i32, i32),
) -> Option<Shape> {
    let to_local = |p: (i32, i32)| (p.0 - sel.0, p.1 - sel.1);
    let (sx, sy) = to_local(start);
    let (cx, cy) = to_local(cur);
    let (w, h) = (sel.2 - sel.0, sel.3 - sel.1);
    match tool {
        0 | 3 | 4 => {
            let (l, t) = (sx.min(cx).clamp(0, w), sy.min(cy).clamp(0, h));
            let (r, b) = (sx.max(cx).clamp(0, w), sy.max(cy).clamp(0, h));
            if r - l < MIN_SEL || b - t < MIN_SEL {
                return None;
            }
            Some(match tool {
                0 => Shape::Rect(l, t, r - l, b - t),
                3 => Shape::Mosaic(l, t, r - l, b - t),
                _ => Shape::Block(l, t, r - l, b - t),
            })
        }
        1 => {
            if (cx - sx).abs() + (cy - sy).abs() < MIN_SEL {
                return None;
            }
            Some(Shape::Arrow(sx, sy, cx, cy))
        }
        2 => {
            if pen_pts.is_empty() {
                return None;
            }
            Some(Shape::Pen(pen_pts.to_vec()))
        }
        _ => None,
    }
}

// ===== GDI 绘制 =====

/// 把一块 BGRA 写进 DIB 段的内存：越出图像的部分丢掉，绝不许写花相邻的行。
/// `bytes` 的长度必须是 w*h*4（`mosaic_block` 与 `fill_block` 都按这个承诺返回）。
unsafe fn poke_pixels(
    pv: *mut core::ffi::c_void,
    img_w: i32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    bytes: &[u8],
) {
    unsafe {
        let dst = pv as *mut u8;
        for row in 0..h.max(0) {
            let dy = y + row;
            if dy < 0 {
                continue;
            }
            for col in 0..w.max(0) {
                let dx = x + col;
                if dx < 0 || dx >= img_w {
                    continue;
                }
                let si = ((row * w.max(0)) + col) as usize * 4;
                if si + 3 >= bytes.len() {
                    continue;
                }
                let di = (dy as usize * img_w as usize + dx as usize) * 4;
                std::ptr::copy_nonoverlapping(bytes.as_ptr().add(si), dst.add(di), 4);
            }
        }
    }
}

unsafe fn blit_bgra(hdc: HDC, buf: &[u8], w: i32, h: i32, dx: i32, dy: i32) {
    if buf.is_empty() || w <= 0 || h <= 0 {
        return;
    }
    let info = BITMAPINFO {
        bmiHeader: focusflow_core::capture::win::dib_header(w as u32, h as u32),
        ..Default::default()
    };
    let _ = SetDIBitsToDevice(
        hdc,
        dx,
        dy,
        w as u32,
        h as u32,
        0,
        0,
        0,
        h as u32,
        buf.as_ptr() as *const _,
        &info,
        DIB_RGB_COLORS,
    );
}

/// 粗线用几何笔（圆头圆角），细线用普通实线笔。
///
/// 为什么分两支：GDI 的 cosmetic 笔**无视宽度**，`CreatePen(PS_SOLID, 7, ..)` 画出来还是 1 像素，
/// "细/粗"两档在屏幕上会长得一模一样。
/// 屏上实时画一笔（与烤图同源：色块/马赛克都是像素运算，别的走 GDI）。
unsafe fn draw_live(
    hdc: HDC,
    base: &[u8],
    base_w: i32,
    base_h: i32,
    rc: (i32, i32, i32, i32),
    m: &Mark,
) {
    unsafe {
        match &m.shape {
            Shape::Mosaic(x, y, mw, mh) => {
                let block = mosaic_block(
                    base,
                    base_w,
                    base_h,
                    rc.0 + *x,
                    rc.1 + *y,
                    *mw,
                    *mh,
                    MOSAIC_CELL,
                );
                blit_bgra(hdc, &block, *mw, *mh, rc.0 + *x, rc.1 + *y);
            }
            Shape::Block(x, y, mw, mh) => {
                let block = fill_block(*mw, *mh, m.style.rgb);
                blit_bgra(hdc, &block, *mw, *mh, rc.0 + *x, rc.1 + *y);
            }
            other => draw_shape(hdc, other, m.style, rc.0, rc.1),
        }
    }
}

unsafe fn make_pen(st: Style) -> HPEN {
    unsafe {
        if st.w > 1 {
            let brush = LOGBRUSH {
                lbStyle: BS_SOLID,
                lbColor: COLORREF(st.rgb),
                lbHatch: 0,
            };
            ExtCreatePen(
                PS_GEOMETRIC | PS_ENDCAP_ROUND | PS_JOIN_ROUND,
                st.w as u32,
                &brush,
                None,
            )
        } else {
            CreatePen(PS_SOLID, 1, COLORREF(st.rgb))
        }
    }
}

unsafe fn draw_arrow(hdc: HDC, x0: i32, y0: i32, x1: i32, y1: i32) {
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
            let _ = LineTo(
                hdc,
                (x1 as f32 + rx * 14.0) as i32,
                (y1 as f32 + ry * 14.0) as i32,
            );
        }
    }
}

/// 在 hdc 上画一笔（马赛克除外）。`ox/oy` 把选区本地坐标搬到目标原点。
unsafe fn draw_shape(hdc: HDC, shape: &Shape, st: Style, ox: i32, oy: i32) {
    unsafe {
        let pen = make_pen(st);
        let old = SelectObject(hdc, pen.into());
        let null = GetStockObject(NULL_BRUSH);
        let oldb = SelectObject(hdc, null);
        match shape {
            Shape::Rect(x, y, w, h) => {
                let _ = Rectangle(hdc, ox + *x, oy + *y, ox + *x + *w, oy + *y + *h);
            }
            Shape::Arrow(x0, y0, x1, y1) => draw_arrow(hdc, ox + *x0, oy + *y0, ox + *x1, oy + *y1),
            Shape::Pen(pts) => {
                for pair in pts.windows(2) {
                    let _ = MoveToEx(hdc, ox + pair[0].0, oy + pair[0].1, None);
                    let _ = LineTo(hdc, ox + pair[1].0, oy + pair[1].1);
                }
                // 只有一个点（原样单击）也要留个看得见的痕迹
                if pts.len() == 1 {
                    let _ = MoveToEx(hdc, ox + pts[0].0, oy + pts[0].1, None);
                    let _ = LineTo(hdc, ox + pts[0].0 + 1, oy + pts[0].1);
                }
            }
            Shape::Block(..) => {
                // 纯色填充走像素路径（见 fill_block），这一支只负责描边以外的形状
            }
            Shape::Text(x, y, s) => {
                let font = CreateFontW(
                    -st.font.max(1),
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
                let _ = SetTextColor(hdc, COLORREF(st.rgb));
                let _ = SetBkMode(hdc, TRANSPARENT);
                let wide: Vec<u16> = s.encode_utf16().collect();
                let _ = TextOutW(hdc, ox + *x, oy + *y, &wide);
                SelectObject(hdc, oldf);
                let _ = DeleteObject(font.into());
            }
            // 马赛克是像素运算，不走 DC：预览与烤图两处各自调 mosaic_block
            Shape::Mosaic(..) => {}
        }
        SelectObject(hdc, oldb);
        SelectObject(hdc, old);
        let _ = DeleteObject(pen.into());
    }
}

/// 把标注烤进像素：DIB 段贴底图 → 逐笔画上去（马赛克直接写格子平均）→ 读回。
///
/// 用 DIB 段而不是 `CreateCompatibleBitmap`：马赛克要**读原图、写结果**，只有 DIB 段能同时
/// 拿到那块内存；顺带省掉一次 `GetDIBits` 往返。马赛克永远从传进来的 `img` 取样，
/// 所以压在它下面的笔迹会被它盖掉而不是被它打码 —— 与旧 Webview 版一致。
pub fn render_marks(img: &[u8], w: i32, h: i32, marks: &[Mark]) -> Vec<u8> {
    let need = (w.max(0) as usize) * (h.max(0) as usize) * 4;
    let mut out = img.to_vec();
    if marks.is_empty() || w <= 0 || h <= 0 || img.len() != need {
        return out;
    }
    unsafe {
        let screen = GetDC(None);
        let info = BITMAPINFO {
            bmiHeader: focusflow_core::capture::win::dib_header(w as u32, h as u32),
            ..Default::default()
        };
        let mut pv: *mut core::ffi::c_void = std::ptr::null_mut();
        let Ok(dib) = CreateDIBSection(Some(screen), &info, DIB_RGB_COLORS, &mut pv, None, 0)
        else {
            ReleaseDC(None, screen);
            return out;
        };
        if pv.is_null() {
            let _ = DeleteObject(dib.into());
            ReleaseDC(None, screen);
            return out;
        }
        std::ptr::copy_nonoverlapping(img.as_ptr(), pv as *mut u8, need);
        let mem = CreateCompatibleDC(Some(screen));
        let old = SelectObject(mem, dib.into());
        for m in marks {
            match &m.shape {
                Shape::Mosaic(x, y, mw, mh) => {
                    let block = mosaic_block(img, w, h, *x, *y, *mw, *mh, MOSAIC_CELL);
                    poke_pixels(pv, w, *x, *y, *mw, *mh, &block);
                }
                Shape::Block(x, y, mw, mh) => {
                    poke_pixels(pv, w, *x, *y, *mw, *mh, &fill_block(*mw, *mh, m.style.rgb));
                }
                other => draw_shape(mem, other, m.style, 0, 0),
            }
        }
        SelectObject(mem, old);
        let _ = DeleteDC(mem);
        out = std::slice::from_raw_parts(pv as *const u8, need).to_vec();
        let _ = DeleteObject(dib.into());
        ReleaseDC(None, screen);
    }
    out
}

unsafe fn draw_chip(hdc: HDC, x: i32, y: i32, text: &str) {
    unsafe {
        let font = yaihe(20, false);
        let oldf = SelectObject(hdc, font.into());
        let wide: Vec<u16> = text.encode_utf16().collect();
        let mut sz = SIZE { cx: 0, cy: 0 };
        let _ = GetTextExtentPoint32W(hdc, &wide, &mut sz);
        let brush = CreateSolidBrush(COLORREF(0x0014_1414));
        let old = SelectObject(hdc, brush.into());
        let _ = Rectangle(hdc, x, y, x + sz.cx + 12, y + sz.cy + 6);
        SelectObject(hdc, old);
        let _ = DeleteObject(brush.into());
        let _ = SetTextColor(hdc, COLORREF(0x00FF_FFFF));
        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = TextOutW(hdc, x + 6, y + 3, &wide);
        SelectObject(hdc, oldf);
        let _ = DeleteObject(font.into());
    }
}

unsafe fn yaihe(height: i32, bold: bool) -> windows::Win32::Graphics::Gdi::HFONT {
    unsafe {
        CreateFontW(
            height,
            0,
            0,
            0,
            if bold { 700 } else { FW_NORMAL.0 as i32 },
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            DEFAULT_PITCH.0 as u32,
            w!("Microsoft YaHei UI"),
        )
    }
}

/// 某颗按钮现在是不是"按下状态"（当前工具 / 当前色 / 当前粗细 / 当前字号）。
pub fn btn_active(id: &str, tool: u8, color: u32, width: i32, font: i32) -> bool {
    match id {
        "rect" => tool == 0,
        "arrow" => tool == 1,
        "pen" => tool == 2,
        "mosaic" => tool == 3,
        "block" => tool == 4,
        "text" => tool == 5,
        c => {
            PALETTE.iter().any(|(v, id2)| *id2 == c && *v == color)
                || WIDTHS.iter().any(|(v, id2)| *id2 == c && *v == width)
                || FONTS.iter().any(|(v, id2)| *id2 == c && *v == font)
        }
    }
}

unsafe fn draw_bar(hdc: HDC, sel: (i32, i32, i32, i32), screen_w: i32, screen_h: i32) {
    unsafe {
        let (tool, color, width, font) = SNIP
            .lock()
            .map(|s| (s.tool, s.color, s.width, s.font))
            .unwrap_or((0, PALETTE[0].0, WIDTHS[0].0, FONTS[1].0));
        let (btns, bar) = bar_rects(sel, screen_w, screen_h);
        let bg = CreateSolidBrush(COLORREF(BAR_BG));
        let old = SelectObject(hdc, bg.into());
        let _ = Rectangle(hdc, bar.0, bar.1, bar.0 + bar.2, bar.1 + bar.3);
        SelectObject(hdc, old);
        let _ = DeleteObject(bg.into());
        let hfont = yaihe(14, false);
        let oldf = SelectObject(hdc, hfont.into());
        let _ = SetBkMode(hdc, TRANSPARENT);
        for b in &btns {
            let active = btn_active(b.id, tool, color, width, font);
            let fill = if active {
                COLORREF(ACCENT)
            } else {
                COLORREF(BTN_BG)
            };
            let br = CreateSolidBrush(fill);
            let oldb = SelectObject(hdc, br.into());
            let p = CreatePen(PS_SOLID, 1, COLORREF(BTN_EDGE));
            let oldp = SelectObject(hdc, p.into());
            let _ = Rectangle(hdc, b.x, b.y, b.x + b.w, b.y + b.h);
            SelectObject(hdc, oldp);
            SelectObject(hdc, oldb);
            let _ = DeleteObject(p.into());
            let _ = DeleteObject(br.into());
            if let Some((c, _)) = PALETTE.iter().find(|(_, id)| *id == b.id) {
                // 色块按钮涂成它代表的那色，比汉字好认
                let sw = CreateSolidBrush(COLORREF(*c));
                let olds = SelectObject(hdc, sw.into());
                let _ = Rectangle(hdc, b.x + 4, b.y + b.h - 9, b.x + b.w - 4, b.y + b.h - 4);
                SelectObject(hdc, olds);
                let _ = DeleteObject(sw.into());
            }
            let _ = SetTextColor(
                hdc,
                if active {
                    COLORREF(0x00FF_FFFF)
                } else {
                    COLORREF(0x00E8_E8E8)
                },
            );
            let wide: Vec<u16> = b.label.encode_utf16().collect();
            let mut sz = SIZE { cx: 0, cy: 0 };
            let _ = GetTextExtentPoint32W(hdc, &wide, &mut sz);
            let _ = TextOutW(hdc, b.x + (b.w - sz.cx) / 2, b.y + (b.h - sz.cy) / 2, &wide);
        }
        SelectObject(hdc, oldf);
        let _ = DeleteObject(hfont.into());
    }
}

// ===== 出图 =====

/// 一次出图：从冻结缓冲裁出选区 → 烤标注 → 落盘 + 进剪贴板（CF_DIB 与 `PNG` 两份）。
/// `pin` = 顺手把这一张钉在桌面上（不再多落一个文件）。
fn commit(pin: bool) {
    let (base, base_w, origin, sel, marks) = match SNIP.lock() {
        Ok(s) => (s.base.clone(), s.base_w, s.origin, s.sel, s.marks.clone()),
        Err(_) => return,
    };
    let (l, t, r, b) = sel;
    let (w, h) = (r - l, b - t);
    if w < MIN_SEL || h < MIN_SEL || base.is_empty() {
        tracing_note("截图取消：没有有效选区");
        return;
    }
    let Some(img) = crop_rect(&base, base_w, l, t, w, h) else {
        tracing_note("截图失败：选区越出冻结画面");
        return;
    };
    let buf = render_marks(&img, w, h, &marks);
    let mut rgba = buf.clone();
    focusflow_core::capture::bgra_to_rgba(&mut rgba);
    let png = match focusflow_core::capture::encode_png(&rgba, w as u32, h as u32) {
        Ok(p) => p,
        Err(e) => {
            tracing_note(&format!("截图编码失败：{e}"));
            return;
        }
    };

    let dir = focusflow_core::paths::screenshots_dir();
    let _ = std::fs::create_dir_all(&dir);
    let listing: Vec<String> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
    let name = out_name(&ts, w as u32, h as u32, &listing);
    let path = dir.join(&name);
    match std::fs::write(&path, &png) {
        Ok(()) => tracing_note(&format!("截图已保存：{} ({w}x{h})", path.display())),
        Err(e) => tracing_note(&format!("截图写盘失败：{e}")),
    }
    match focusflow_core::capture::win::write_image_to_clipboard(&buf, w as u32, h as u32, &png) {
        Ok(()) => tracing_note(&format!("截图已进剪贴板：{w}x{h} CF_DIB+PNG")),
        Err(e) => tracing_note(&format!("截图进剪贴板失败：{e}")),
    }
    if pin {
        open_pin(buf, w, h, origin.0 + l, origin.1 + t);
    }
}

// ===== 窗口清单（单击选窗口 / 悬停预览）=====

fn collect_candidates() -> Vec<Candidate> {
    if let Ok(mut v) = SNIP_HITS.lock() {
        v.clear();
    }
    unsafe {
        let _ = EnumWindows(Some(collect_hit), LPARAM(0));
    }
    SNIP_HITS.lock().map(|v| v.clone()).unwrap_or_default()
}

unsafe extern "system" fn collect_hit(hwnd: HWND, _l: LPARAM) -> BOOL {
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    let mut rc = RECT::default();
    unsafe {
        // DWM 那份边界才是"看见的那块"：GetWindowRect 还带着一圈透明的可调边框，
        // 照它截会在窗口外面多带一截桌面。拿不到（非合成窗口）才退回窗口矩形。
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rc as *mut RECT as *mut core::ffi::c_void,
            std::mem::size_of::<RECT>() as u32,
        )
        .is_err()
        {
            let _ = GetWindowRect(hwnd, &mut rc);
        }
    }
    if let Ok(mut v) = SNIP_HITS.lock() {
        v.push(Candidate {
            pid,
            visible: unsafe { IsWindowVisible(hwnd) }.as_bool(),
            rect: (rc.left, rc.top, rc.right, rc.bottom),
        });
    }
    TRUE
}

/// 从候选里挑第一个"别人进程的、可见的、还有面积的、罩住光标"的窗口矩形，并夹回虚拟屏幕。
///
/// 排掉自己进程是这条手势的关键：覆盖层自己就是全屏、就在最上面，不排掉它每次都挑中蒙版本身。
/// 夹回虚拟屏幕是因为窗口可以探出屏幕外（负原点的副屏、被拖出界的窗口），探出去的那部分
/// 屏幕上根本没有像素可截。夹完没有面积可截就接着往下找。
fn window_hit(
    candidates: &[Candidate],
    self_pid: u32,
    screen: (i32, i32, i32, i32),
    x: i32,
    y: i32,
) -> Option<(i32, i32, i32, i32)> {
    for c in candidates {
        if c.pid == self_pid || !c.visible {
            continue;
        }
        let (l, t, r, b) = c.rect;
        if r - l < 2 || b - t < 2 {
            continue;
        }
        if x < l || x >= r || y < t || y >= b {
            continue;
        }
        let l = l.max(screen.0);
        let t = t.max(screen.1);
        let r = r.min(screen.2);
        let b = b.min(screen.3);
        if r - l < 2 || b - t < 2 {
            continue;
        }
        return Some((l, t, r, b));
    }
    None
}

/// 客户区点 -> 命中的候选窗口（返回的也是客户区坐标，直接当亮区画）。
fn hover_at(
    hits: &[Candidate],
    self_pid: u32,
    origin: (i32, i32),
    cw: i32,
    ch: i32,
    x: i32,
    y: i32,
) -> Option<(i32, i32, i32, i32)> {
    let screen = (origin.0, origin.1, origin.0 + cw, origin.1 + ch);
    window_hit(hits, self_pid, screen, origin.0 + x, origin.1 + y).map(|(l, t, r, b)| {
        clamp_rect(
            (l - origin.0, t - origin.1, r - origin.0, b - origin.1),
            cw,
            ch,
        )
    })
}

// ===== 覆盖层 =====

/// 按下那一刻该做什么（先在持锁时判完，出锁再动手：`Mutex` 不可重入，
/// 持着锁去调还会取锁的 `commit`/工具条动作就是把界面线程焊死）。
enum Press {
    Bar(&'static str),
    Handle(u8),
    /// 在选区里落一笔（文字工具走 `Text`）
    Mark,
    /// 开文字框：客户区落点
    Text(i32, i32),
    /// 重新框选区
    New(i32, i32),
}

unsafe extern "system" fn overlay_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_ERASEBKGND => LRESULT(1),
            WM_LBUTTONDOWN => {
                let x = (l.0 & 0xFFFF) as i16 as i32;
                let y = ((l.0 >> 16) & 0xFFFF) as i16 as i32;
                let press = SNIP
                    .lock()
                    .map(|s| {
                        if s.phase == Phase::Adjust {
                            let (btns, _) = bar_rects(s.sel, s.base_w, s.base_h);
                            if let Some(i) = hit_btn(&btns, x, y) {
                                return Press::Bar(btns[i].id);
                            }
                            if let Some(idx) = hit_handle(s.sel, x, y) {
                                return Press::Handle(idx);
                            }
                            if inside_rect(s.sel, x, y) {
                                return if s.tool == 5 {
                                    Press::Text(x, y)
                                } else {
                                    Press::Mark
                                };
                            }
                        }
                        Press::New(x, y)
                    })
                    .unwrap_or(Press::New(x, y));
                match press {
                    Press::Bar(id) => bar_action(id, hwnd),
                    Press::Handle(idx) => {
                        if let Ok(mut s) = SNIP.lock() {
                            s.drag = Some((Drag::Handle(idx), x, y));
                        }
                        let _ = SetCapture(hwnd);
                    }
                    Press::Mark => {
                        let _ = SetCapture(hwnd);
                        if let Ok(mut s) = SNIP.lock() {
                            s.drag = Some((Drag::Mark, x, y));
                            s.cur = (x, y);
                            s.pen_pts = if s.tool == 2 {
                                vec![(x - s.sel.0, y - s.sel.1)]
                            } else {
                                Vec::new()
                            };
                        }
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    Press::Text(x, y) => open_text_edit(hwnd, x, y),
                    Press::New(x, y) => {
                        if let Ok(mut s) = SNIP.lock() {
                            s.phase = Phase::Select;
                            s.drag = Some((Drag::New, x, y));
                            s.sel = (x, y, x, y);
                            s.hover = None;
                            s.cur = (x, y);
                        }
                        let _ = SetCapture(hwnd);
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let x = (l.0 & 0xFFFF) as i16 as i32;
                let y = ((l.0 >> 16) & 0xFFFF) as i16 as i32;
                let mut changed = false;
                if let Ok(mut s) = SNIP.lock() {
                    match s.drag {
                        Some((Drag::New, sx, sy)) => {
                            let nr = clamp_rect(norm_rect(sx, sy, x, y), s.base_w, s.base_h);
                            if nr != s.sel {
                                s.sel = nr;
                                changed = true;
                            }
                        }
                        Some((Drag::Handle(i), _, _)) => {
                            let nr = resize_rect(s.sel, i, x, y, s.base_w, s.base_h);
                            if nr != s.sel {
                                s.sel = nr;
                                changed = true;
                            }
                        }
                        Some((Drag::Mark, sx, sy)) => {
                            if s.cur != (x, y) {
                                s.cur = (x, y);
                                if s.tool == 2 {
                                    let p = (x - s.sel.0, y - s.sel.1);
                                    if s.pen_pts.last() != Some(&p) {
                                        s.pen_pts.push(p);
                                    }
                                }
                                changed = true;
                            }
                            let _ = sx;
                            let _ = sy;
                        }
                        None => {
                            if s.phase == Phase::Select {
                                let hv = hover_at(
                                    &s.hits,
                                    GetCurrentProcessId(),
                                    s.origin,
                                    s.base_w,
                                    s.base_h,
                                    x,
                                    y,
                                );
                                if hv != s.hover {
                                    s.hover = hv;
                                    changed = true;
                                }
                            }
                        }
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
                let drag = SNIP.lock().map(|s| s.drag).unwrap_or(None);
                match drag {
                    Some((Drag::New, _, _)) => {
                        let (sel, annotate) = SNIP
                            .lock()
                            .map(|s| (s.sel, s.annotate))
                            .unwrap_or(((0, 0, 0, 0), true));
                        if sel.2 - sel.0 >= MIN_DRAG && sel.3 - sel.1 >= MIN_DRAG {
                            finish_selection(hwnd, annotate);
                        } else {
                            // 没拖出区域就是旧版那条手势：单击选中光标下面那个窗口
                            let hv = SNIP
                                .lock()
                                .map(|s| {
                                    hover_at(
                                        &s.hits,
                                        GetCurrentProcessId(),
                                        s.origin,
                                        s.base_w,
                                        s.base_h,
                                        x,
                                        y,
                                    )
                                })
                                .ok()
                                .flatten();
                            match hv {
                                Some(rc) => {
                                    let origin = s_origin(hwnd);
                                    // 日志里报的是屏幕坐标（与旧版同一口径），客户区坐标只有画的时候用得上
                                    tracing_note(&format!(
                                        "截图 snip-pick {}x{} at {},{}",
                                        rc.2 - rc.0,
                                        rc.3 - rc.1,
                                        origin.0 + rc.0,
                                        origin.1 + rc.1
                                    ));
                                    if let Ok(mut s) = SNIP.lock() {
                                        s.sel = rc;
                                        s.drag = None;
                                    }
                                    finish_selection(hwnd, annotate);
                                }
                                None => {
                                    tracing_note("截图取消：这一笔既没框出区域，也没点到别的窗口");
                                    let _ = DestroyWindow(hwnd);
                                }
                            }
                        }
                    }
                    Some((Drag::Handle(_), _, _)) => {
                        if let Ok(mut s) = SNIP.lock() {
                            s.drag = None;
                        }
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    Some((Drag::Mark, sx, sy)) => {
                        let mark = SNIP
                            .lock()
                            .map(|s| {
                                let shape =
                                    shape_from_drag(s.tool, (sx, sy), (x, y), &s.pen_pts, s.sel);
                                shape.map(|sh| Mark {
                                    shape: sh,
                                    style: Style {
                                        rgb: s.color,
                                        w: s.width,
                                        font: s.font,
                                    },
                                })
                            })
                            .ok()
                            .flatten();
                        if let Some(m) = mark {
                            if let Ok(mut s) = SNIP.lock() {
                                s.redo.clear();
                                s.marks.push(m);
                            }
                        }
                        if let Ok(mut s) = SNIP.lock() {
                            s.drag = None;
                            s.pen_pts.clear();
                        }
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    None => {
                        let _ = x;
                        let _ = y;
                    }
                }
                LRESULT(0)
            }
            WM_SETCURSOR => {
                if ((l.0 & 0xFFFF) as u32) != HTCLIENT {
                    return DefWindowProcW(hwnd, msg, w, l);
                }
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let origin = s_origin(hwnd);
                let (cx, cy) = (pt.x - origin.0, pt.y - origin.1);
                let mut cur = IDC_CROSS;
                if let Ok(s) = SNIP.lock() {
                    if s.phase == Phase::Adjust {
                        let (btns, bar) = bar_rects(s.sel, s.base_w, s.base_h);
                        let in_bar =
                            cx >= bar.0 && cx < bar.0 + bar.2 && cy >= bar.1 && cy < bar.1 + bar.3;
                        if in_bar || hit_btn(&btns, cx, cy).is_some() {
                            cur = IDC_ARROW;
                        } else if let Some(i) = hit_handle(s.sel, cx, cy) {
                            cur = match i {
                                0 | 4 => IDC_SIZENWSE,
                                2 | 6 => IDC_SIZENESW,
                                1 | 5 => IDC_SIZENS,
                                _ => IDC_SIZEWE,
                            };
                        } else if inside_rect(s.sel, cx, cy) {
                            cur = IDC_HAND;
                        }
                    }
                }
                let _ = SetCursor(Some(LoadCursorW(None, cur).unwrap_or_default()));
                LRESULT(1)
            }
            WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
                tracing_note("截图取消：右键");
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_KEYDOWN => {
                on_key(hwnd, w.0);
                LRESULT(0)
            }
            WM_EDIT_DONE => {
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
                                let st = Style {
                                    rgb: s.color,
                                    w: s.width,
                                    font: s.font,
                                };
                                s.redo.clear();
                                s.marks.push(Mark {
                                    shape: Shape::Text(at.0, at.1, text),
                                    style: st,
                                });
                            }
                        }
                    }
                    let _ = DestroyWindow(h);
                    if let Ok(mut s) = SNIP.lock() {
                        s.text_edit = 0;
                    }
                    let _ = SetFocus(Some(hwnd));
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_TIMER => {
                // 覆盖层只挂了一个定时器，但 id 照样要对上：别的 WM_TIMER 不该被当成超时。
                if w.0 == SNIP_TIMEOUT_TIMER {
                    tracing_note("截图 snip-timeout 三分钟没动，覆盖层自己退");
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_PAINT => {
                paint(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                // 覆盖层没了 = 这一次截图结束。标志位与两份全屏缓冲都必须在这里复位：
                // 漏掉标志会把下一次热键吃掉（旧账），漏掉缓冲会把十几 MB 一直占到重启。
                if let Ok(mut s) = SNIP.lock() {
                    s.active = false;
                    s.phase = Phase::Select;
                    s.drag = None;
                    s.pen_pts.clear();
                    s.hover = None;
                    s.sel = (0, 0, 0, 0);
                    s.marks.clear();
                    s.redo.clear();
                    s.hits.clear();
                    s.text_edit = 0;
                    s.overlay = 0;
                    s.base = Vec::new();
                    s.dim = Vec::new();
                    s.base_w = 0;
                    s.base_h = 0;
                    if s.pill_was {
                        s.pill_was = false;
                        let _ = floating::set_visible(true);
                    }
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }
}

fn s_origin(_hwnd: HWND) -> (i32, i32) {
    SNIP.lock().map(|s| s.origin).unwrap_or((0, 0))
}

/// 工具条某一颗按钮的落地动作。调用时**不许持着 `SNIP` 的锁**。
unsafe fn bar_action(id: &'static str, hwnd: HWND) {
    match id {
        t @ ("rect" | "arrow" | "pen" | "mosaic" | "block" | "text") => {
            if let Some(i) = TOOLS.iter().position(|x| *x == t) {
                if let Ok(mut s) = SNIP.lock() {
                    s.tool = i as u8;
                }
            }
        }
        c @ ("c1" | "c2" | "c3" | "c4") => {
            if let Some((color, _)) = PALETTE.iter().find(|(_, id2)| *id2 == c) {
                if let Ok(mut s) = SNIP.lock() {
                    s.color = *color;
                }
            }
        }
        wd @ ("w1" | "w2") => {
            if let Some((v, _)) = WIDTHS.iter().find(|(_, id2)| *id2 == wd) {
                if let Ok(mut s) = SNIP.lock() {
                    s.width = *v;
                }
            }
        }
        f @ ("f1" | "f2" | "f3") => {
            if let Some((v, _)) = FONTS.iter().find(|(_, id2)| *id2 == f) {
                if let Ok(mut s) = SNIP.lock() {
                    s.font = *v;
                }
            }
        }
        "undo" => {
            if let Ok(mut s) = SNIP.lock() {
                if let Some(m) = s.marks.pop() {
                    s.redo.push(m);
                }
            }
        }
        "redo" => {
            if let Ok(mut s) = SNIP.lock() {
                if let Some(m) = s.redo.pop() {
                    s.marks.push(m);
                }
            }
        }
        "save" => {
            commit(false);
            let _ = DestroyWindow(hwnd);
        }
        "pin" => {
            commit(true);
            let _ = DestroyWindow(hwnd);
        }
        "cancel" => {
            tracing_note("截图取消：点了取消");
            let _ = DestroyWindow(hwnd);
        }
        _ => {}
    }
    let _ = InvalidateRect(Some(hwnd), None, false);
}

/// 选区定了：快速版直接出图并关窗，标注版留在覆盖层上（阶段 = Adjust）。
unsafe fn finish_selection(hwnd: HWND, annotate: bool) {
    if !annotate {
        tracing_note("截图 snip-quick：松手即存，不进标注窗");
        commit(false);
        let _ = DestroyWindow(hwnd);
        return;
    }
    if let Ok(mut s) = SNIP.lock() {
        s.drag = None;
        s.pen_pts.clear();
        s.phase = Phase::Adjust;
        s.hover = None;
        s.marks.clear();
        s.redo.clear();
    }
    // 把工具条每颗按钮的落点亮进日志：验收脚本据此点按钮，而不是自己抄一遍排版。
    if let Ok(s) = SNIP.lock() {
        let (btns, bar) = bar_rects(s.sel, s.base_w, s.base_h);
        tracing_note(&format!(
            "截图 snip-annotate 进标注：选区 {}x{} 工具条 {}x{} at {},{}",
            s.sel.2 - s.sel.0,
            s.sel.3 - s.sel.1,
            bar.2,
            bar.3,
            bar.0,
            bar.1
        ));
        for b in &btns {
            tracing_note(&format!(
                "snip-btn {} {} {} {} {}",
                b.id, b.x, b.y, b.w, b.h
            ));
        }
    }
    let _ = InvalidateRect(Some(hwnd), None, false);
}

/// 键盘裁决（纯判据，便于用例断言）：返回这一次按键要不要重画/出图/关窗。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum KeyAct {
    Nothing,
    Cancel,
    /// 完成并存盘后关窗
    Commit,
    /// 贴图并存盘后关窗
    Pin,
    Undo,
    Redo,
    /// 选区偏移（搬 / 改大小由 shift 决定）
    Nudge(i32, i32, bool),
}

/// 键码 + 修饰键 -> 动作。`in_adjust` 为假时只有"放弃"有效：还没定选区就没有可搬的东西。
pub fn key_action(vk: usize, ctrl: bool, shift: bool, in_adjust: bool) -> KeyAct {
    let arrow = match vk {
        0x25 => Some((-1, 0)),
        0x26 => Some((0, -1)),
        0x27 => Some((1, 0)),
        0x28 => Some((0, 1)),
        _ => None,
    };
    if let Some((dx, dy)) = arrow {
        return if in_adjust {
            KeyAct::Nudge(dx, dy, shift)
        } else {
            KeyAct::Nothing
        };
    }
    match vk {
        0x1B => KeyAct::Cancel,
        0x08 if !ctrl => KeyAct::Undo,
        0x5A if ctrl && !shift => KeyAct::Undo,
        0x59 if ctrl => KeyAct::Redo,
        0x0D if in_adjust => KeyAct::Commit,
        0x53 if in_adjust && ctrl => KeyAct::Commit,
        0x50 if in_adjust => KeyAct::Pin,
        _ => KeyAct::Nothing,
    }
}

unsafe fn on_key(hwnd: HWND, vk: usize) {
    let ctrl = (GetKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
    let shift = (GetKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0;
    let in_adjust = SNIP
        .lock()
        .map(|s| s.phase == Phase::Adjust)
        .unwrap_or(false);
    match key_action(vk, ctrl, shift, in_adjust) {
        KeyAct::Nothing => {}
        KeyAct::Cancel => {
            tracing_note("截图取消：Esc");
            let _ = DestroyWindow(hwnd);
        }
        KeyAct::Commit => {
            commit(false);
            let _ = DestroyWindow(hwnd);
        }
        KeyAct::Pin => {
            commit(true);
            let _ = DestroyWindow(hwnd);
        }
        KeyAct::Undo => undo(),
        KeyAct::Redo => redo_mark(),
        KeyAct::Nudge(dx, dy, by_size) => {
            if let Ok(mut s) = SNIP.lock() {
                let (w, h) = (s.base_w, s.base_h);
                s.sel = if by_size {
                    grow_rect(s.sel, dx, dy, w, h)
                } else {
                    nudge_rect(s.sel, dx, dy, w, h)
                };
            }
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
}

fn undo() {
    if let Ok(mut s) = SNIP.lock() {
        if let Some(m) = s.marks.pop() {
            s.redo.push(m);
        }
    }
}

fn redo_mark() {
    if let Ok(mut s) = SNIP.lock() {
        if let Some(m) = s.redo.pop() {
            s.marks.push(m);
        }
    }
}

/// 在覆盖层上开一个文字输入框（原生 EDIT，自带中文输入法）。落点存的是选区本地坐标。
unsafe fn open_text_edit(overlay: HWND, x: i32, y: i32) {
    unsafe {
        let old = SNIP.lock().map(|s| s.text_edit).unwrap_or(0);
        if old != 0 {
            let _ = DestroyWindow(HWND(old as *mut core::ffi::c_void));
        }
        let font = SNIP.lock().map(|s| s.font).unwrap_or(30);
        let (sl, st) = SNIP.lock().map(|s| (s.sel.0, s.sel.1)).unwrap_or((0, 0));
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
            font + 8,
            Some(overlay),
            None,
            None,
            None,
        );
        if let Ok(h) = created {
            let f = CreateFontW(
                font,
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
            SendMessageW(h, WM_SETFONT, Some(WPARAM(f.0 as usize)), Some(LPARAM(1)));
            let _ = SetWindowSubclass(h, Some(text_edit_proc), 7, 0);
            let _ = SetFocus(Some(h));
            if let Ok(mut s) = SNIP.lock() {
                s.text_edit = h.0 as isize;
                s.text_at = (x - sl, y - st);
            }
        }
    }
}

/// EDIT 的子类过程：Enter 提交、Esc 只丢这一条文字。
unsafe extern "system" fn text_edit_proc(
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    unsafe {
        match msg {
            WM_KEYDOWN if w.0 == 0x0D || w.0 == 0x1B => {
                let code = if w.0 == 0x0D { 1 } else { 2 };
                if let Ok(parent) = GetParent(hwnd) {
                    let _ = PostMessageW(Some(parent), WM_EDIT_DONE, WPARAM(code), LPARAM(0));
                }
                LRESULT(0)
            }
            WM_CHAR if w.0 == 0x0D || w.0 == 0x1B => LRESULT(0),
            _ => DefSubclassProc(hwnd, msg, w, l),
        }
    }
}

unsafe fn paint(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let snap = match SNIP.lock() {
            Ok(s) => Some((
                s.base.clone(),
                s.dim.clone(),
                s.base_w,
                s.base_h,
                s.sel,
                s.phase,
                s.hover,
                s.marks.clone(),
                s.drag,
                s.cur,
                s.pen_pts.clone(),
                s.tool,
                s.color,
                s.width,
                s.font,
            )),
            Err(_) => None,
        };
        let Some((
            base,
            dim,
            bw,
            bh,
            sel,
            phase,
            hover,
            marks,
            drag,
            cur,
            pen_pts,
            tool,
            color,
            width,
            font,
        )) = snap
        else {
            let _ = EndPaint(hwnd, &ps);
            return;
        };
        if base.is_empty() || bw <= 0 || bh <= 0 {
            // 冻结失败也要出声：一片深色比"看起来没反应"好判断
            let brush = CreateSolidBrush(COLORREF(0x0014_1414));
            let old = SelectObject(hdc, brush.into());
            let _ = Rectangle(hdc, 0, 0, bw.max(1), bh.max(1));
            SelectObject(hdc, old);
            let _ = DeleteObject(brush.into());
            let _ = EndPaint(hwnd, &ps);
            return;
        }
        // 1) 压暗的那一份铺满全屏
        blit_bgra(hdc, &dim, bw, bh, 0, 0);
        // 2) 亮区：拖框中或选区已定用选区，Select 阶段用悬停命中的窗口
        let bright = match drag {
            Some((Drag::New, _, _)) | Some((Drag::Handle(_), _, _)) | Some((Drag::Mark, _, _)) => {
                Some(sel)
            }
            None => {
                if phase == Phase::Adjust {
                    Some(sel)
                } else {
                    hover
                }
            }
        };
        let clip_saved = CreateRectRgn(0, 0, 0, 0);
        let _ = GetClipRgn(hdc, clip_saved);
        if let Some(rc) = bright {
            let rg = CreateRectRgn(rc.0, rc.1, rc.2, rc.3);
            let _ = SelectClipRgn(hdc, Some(rg));
            blit_bgra(hdc, &base, bw, bh, 0, 0);
            // 已画的标注：与烤图同一条路（色块/马赛克按像素算，其余走 GDI）
            for m in &marks {
                draw_live(hdc, &base, bw, bh, rc, m);
            }
            // 正在拖的那一笔
            if let Some((Drag::Mark, sx, sy)) = drag {
                let st = Style {
                    rgb: color,
                    w: width,
                    font,
                };
                if tool == 3 {
                    // 马赛克的实时预览只画虚线感的空心框：格子平均留给出图那一步
                    let (l, t) = (sx.min(cur.0), sy.min(cur.1));
                    let (r, b) = (sx.max(cur.0), sy.max(cur.1));
                    let p = CreatePen(PS_SOLID, 1, COLORREF(ACCENT));
                    let oldp = SelectObject(hdc, p.into());
                    let null = GetStockObject(NULL_BRUSH);
                    let oldb = SelectObject(hdc, null);
                    let _ = Rectangle(hdc, l, t, r, b);
                    SelectObject(hdc, oldb);
                    SelectObject(hdc, oldp);
                    let _ = DeleteObject(p.into());
                } else if let Some(sh) = shape_from_drag(tool, (sx, sy), cur, &pen_pts, rc) {
                    draw_live(
                        hdc,
                        &base,
                        bw,
                        bh,
                        rc,
                        &Mark {
                            shape: sh,
                            style: st,
                        },
                    );
                }
            }
            // 选区外框 + 手柄
            let pen = CreatePen(PS_SOLID, 2, COLORREF(ACCENT));
            let oldp = SelectObject(hdc, pen.into());
            let null = GetStockObject(NULL_BRUSH);
            let oldb = SelectObject(hdc, null);
            let _ = Rectangle(hdc, rc.0, rc.1, rc.2, rc.3);
            SelectObject(hdc, oldb);
            SelectObject(hdc, oldp);
            let _ = DeleteObject(pen.into());
            if phase == Phase::Adjust {
                for (hx, hy) in handle_points(rc) {
                    let brush = CreateSolidBrush(COLORREF(0x00FF_FFFF));
                    let old = SelectObject(hdc, brush.into());
                    let hp = CreatePen(PS_SOLID, 1, COLORREF(ACCENT));
                    let oldpen = SelectObject(hdc, hp.into());
                    let _ = Rectangle(
                        hdc,
                        hx - HANDLE_PX / 2,
                        hy - HANDLE_PX / 2,
                        hx + HANDLE_PX / 2,
                        hy + HANDLE_PX / 2,
                    );
                    SelectObject(hdc, oldpen);
                    SelectObject(hdc, old);
                    let _ = DeleteObject(hp.into());
                    let _ = DeleteObject(brush.into());
                }
            }
            let _ = SelectClipRgn(hdc, Some(clip_saved));
            let _ = DeleteObject(rg.into());
            // 3) 尺寸读数（选区左上角上方 22 像素，物理像素计数）—— 不夹在选区里
            draw_chip(
                hdc,
                rc.0,
                rc.1 - 24,
                &format!("{} × {}", rc.2 - rc.0, rc.3 - rc.1),
            );
        }
        if phase == Phase::Adjust {
            draw_bar(hdc, sel, bw, bh);
        }
        let _ = DeleteObject(clip_saved.into());
        let _ = EndPaint(hwnd, &ps);
    }
}

/// 抓一块屏幕（BGRA、自上而下）。冻结整块虚拟屏与调试钩子共用这一份。
pub fn grab_rect(x: i32, y: i32, w: i32, h: i32) -> std::result::Result<Vec<u8>, String> {
    if w <= 0 || h <= 0 {
        return Err(format!("选区太小：{w}×{h}"));
    }
    unsafe {
        use windows::Win32::Graphics::Gdi::{BitBlt, CreateCompatibleBitmap, GetDIBits, SRCCOPY};
        let screen = GetDC(None);
        if screen.0.is_null() {
            return Err("拿不到屏幕 DC".into());
        }
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, bmp.into());
        let ok = BitBlt(mem, 0, 0, w, h, Some(screen), x, y, SRCCOPY).is_ok();
        // 负高度 = 自上而下：读回来的行序与屏幕一致，后面所有换算都按这一条走。
        let mut info = BITMAPINFO {
            bmiHeader: focusflow_core::capture::win::dib_header(w as u32, h as u32),
            ..Default::default()
        };
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

fn virtual_origin() -> (i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
        )
    }
}

// ===== 对外入口 =====

/// 覆盖层已经开着的时候又按了一次热键，该怎么办。
///
/// 旧 Webview 版的口径：**再按一次标注热键 = 放弃在飞的这一次**（重开一次新的），
/// 而另一条热键（松手即存）重按只出声不重开 —— 它的语义是"我要立刻出一张"，
/// 在飞的选区被吃掉反而更糟。写成纯函数是因为"哪条键能打断"最容易写反。
pub fn repress_action(active: bool, pressed_annotate: bool) -> Repress {
    if !active {
        Repress::Open
    } else if pressed_annotate {
        Repress::AbandonAndOpen
    } else {
        Repress::Busy
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Repress {
    /// 没有在飞的会话，正常开
    Open,
    /// 有在飞的会话，但按的是标注键：先放弃它再开新的
    AbandonAndOpen,
    /// 有在飞的会话且不该打断
    Busy,
}

/// 打开截图覆盖层（托盘菜单/设置页按钮/两条截图热键/测试钩子都走这里）。
///
/// `annotate` = 松手后就地标注；`false` = 松手即存（PNG + 剪贴板），与旧版两条热键的分工一致。
///
/// 顺序是这条路的要害：先藏悬浮窗 → 冻结整块虚拟屏 → 再建覆盖层。
/// 反过来做的话覆盖层自己会进冻结图（它是全屏 topmost），而卡片会留在截图里。
pub fn start(owner: HWND, annotate: bool) {
    let (active, inflight) = SNIP
        .lock()
        .map(|s| (s.active, s.overlay))
        .unwrap_or((false, 0));
    match repress_action(active, annotate) {
        Repress::Open => {}
        Repress::Busy => {
            tracing_note("截图 snip-busy 已有一张在进行中");
            return;
        }
        Repress::AbandonAndOpen => {
            tracing_note("截图 snip-repress 再按一次标注热键：放弃在飞的这一次，重开");
            if inflight != 0 {
                unsafe {
                    let _ = DestroyWindow(HWND(inflight as *mut core::ffi::c_void));
                }
            }
        }
    }
    let (vx, vy) = virtual_origin();
    let (vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if vw <= 0 || vh <= 0 {
        tracing_note("截图失败：拿不到虚拟屏尺寸");
        return;
    }
    let pill_was = floating::is_visible();
    if pill_was {
        crate::floating::set_visible(false);
        // 让隐藏真的落到合成器上，否则卡片还会带进这一帧。
        std::thread::sleep(std::time::Duration::from_millis(90));
    }
    let base = match grab_rect(vx, vy, vw, vh) {
        Ok(b) => b,
        Err(e) => {
            tracing_note(&format!("截图失败：冻结画面没成：{e}"));
            if pill_was {
                crate::floating::set_visible(true);
            }
            return;
        }
    };
    let dim = dim_bgra(&base);
    let hits = collect_candidates();
    if pill_was {
        crate::floating::set_visible(true);
    }

    unsafe {
        let hinst = GetModuleHandleW(None).map(|m| m.into()).unwrap_or_default();
        let cls = HSTRING::from(OVERLAY_CLASS);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(overlay_proc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_CROSS).unwrap_or_default(),
            lpszClassName: PCWSTR(cls.as_ptr()),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);
        // 不再是分层窗口：自己把冻结图与蒙版画满全屏，才能"选区里看到的就是最终拿到的那一份"。
        let created = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            &cls,
            &HSTRING::from("FocusFlow 截图"),
            WINDOW_STYLE(WS_POPUP.0 | WS_VISIBLE.0 | WS_CLIPCHILDREN.0),
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
                if let Ok(mut s) = SNIP.lock() {
                    s.active = true;
                    s.annotate = annotate;
                    s.phase = Phase::Select;
                    s.origin = (vx, vy);
                    s.base_w = vw;
                    s.base_h = vh;
                    s.sel = (0, 0, 0, 0);
                    s.drag = None;
                    s.cur = (0, 0);
                    s.pen_pts.clear();
                    s.hover = None;
                    s.marks.clear();
                    s.redo.clear();
                    s.hits = hits;
                    s.overlay = ov.0 as isize;
                    s.text_edit = 0;
                    s.base = base;
                    s.dim = dim;
                    s.pill_was = pill_was;
                }
                // 一开层就把光标下面那块屏的窗口亮出来：旧版是移一下鼠标才亮，
                // 但"按下热键时光标已经指在他要截的东西上"才是最常见的形状。
                let mut pt = POINT::default();
                if GetCursorPos(&mut pt).is_ok() {
                    let hv = SNIP
                        .lock()
                        .map(|s| {
                            hover_at(
                                &s.hits,
                                GetCurrentProcessId(),
                                s.origin,
                                s.base_w,
                                s.base_h,
                                pt.x - vx,
                                pt.y - vy,
                            )
                        })
                        .ok()
                        .flatten();
                    if let Ok(mut s) = SNIP.lock() {
                        s.hover = hv;
                    }
                }
                let _ = ShowWindow(ov, SW_SHOW);
                let _ = SetForegroundWindow(ov);
                let _ = SetFocus(Some(ov));
                let _ = SetTimer(Some(ov), SNIP_TIMEOUT_TIMER, SNIP_TIMEOUT_MS, None);
                tracing_note(&format!("截图 snip-arm-timeout {SNIP_TIMEOUT_MS}"));
                tracing_note("截图覆盖层已打开（拖选区域或单击选窗口，右键/Esc 取消）");
            }
            Err(_) => {
                tracing_note("截图覆盖层创建失败");
                if let Ok(mut s) = SNIP.lock() {
                    s.base = Vec::new();
                    s.dim = Vec::new();
                    s.active = false;
                    if s.pill_was {
                        s.pill_was = false;
                        crate::floating::set_visible(true);
                    }
                }
            }
        }
    }
}

/// 测试钩子那条路：按固定区域抓屏、写 PNG、进剪贴板（与主流程共用 core 的输出实现）。
pub fn test_capture_to(rect: (i32, i32, i32, i32), out: &std::path::Path) {
    let (x, y, w, h) = rect;
    match grab_rect(x, y, w, h) {
        Ok(buf) => {
            let mut rgba = buf.clone();
            focusflow_core::capture::bgra_to_rgba(&mut rgba);
            match focusflow_core::capture::encode_png(&rgba, w as u32, h as u32) {
                Ok(png) => match std::fs::write(out, &png) {
                    Ok(()) => tracing_note(&format!("截图测试：已写 {}", out.display())),
                    Err(e) => tracing_note(&format!("截图测试写盘失败：{e}")),
                },
                Err(e) => tracing_note(&format!("截图测试编码失败：{e}")),
            }
            match focusflow_core::capture::win::write_image_to_clipboard(
                &buf,
                w as u32,
                h as u32,
                &[],
            ) {
                Ok(()) => tracing_note("截图测试：已进剪贴板"),
                Err(e) => tracing_note(&format!("截图测试：剪贴板失败 {e}")),
            }
        }
        Err(e) => tracing_note(&format!("截图测试失败：{e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(pid: u32, visible: bool, rect: (i32, i32, i32, i32)) -> Candidate {
        Candidate { pid, visible, rect }
    }

    const SCREEN: (i32, i32, i32, i32) = (0, 0, 1920, 1080);

    /// 光标同时压着两层时取 Z 序里靠前的那个（`EnumWindows` 就是从最上面往下枚举）。
    #[test]
    fn takes_the_topmost_candidate_covering_the_point() {
        let list = [
            cand(7, true, (100, 100, 300, 260)),
            cand(8, true, (50, 50, 600, 600)),
        ];
        assert_eq!(
            window_hit(&list, 1, SCREEN, 150, 150),
            Some((100, 100, 300, 260))
        );
    }

    /// 自己进程必须排掉：覆盖层自己就是全屏、就在最上面，不排掉它每次都挑中那层蒙版。
    #[test]
    fn skips_our_own_process_even_when_it_is_the_topmost() {
        let list = [
            cand(1, true, (0, 0, 1920, 1080)),
            cand(9, true, (400, 400, 700, 500)),
        ];
        assert_eq!(
            window_hit(&list, 1, SCREEN, 450, 450),
            Some((400, 400, 700, 500))
        );
    }

    /// 不可见的候选排掉：最小化或隐藏的窗口标志位还挂在那儿，截它只会截到一张旧画面。
    /// 两层都要罩住光标，否则"跳过不可见"这条根本没被走到——点在第二层外面时连排掉都不必发生。
    #[test]
    fn skips_invisible_candidates() {
        let list = [
            cand(2, false, (0, 0, 1920, 1080)),
            cand(3, true, (10, 10, 20, 20)),
        ];
        assert_eq!(window_hit(&list, 1, SCREEN, 15, 15), Some((10, 10, 20, 20)));
    }

    /// 退化矩形（宽或高不足 2 像素）不算一层，接着往下找；全是退化的就什么都没选中。
    #[test]
    fn skips_degenerate_rects() {
        let list = [cand(4, true, (0, 0, 1, 900)), cand(5, true, (0, 0, 30, 30))];
        assert_eq!(window_hit(&list, 1, SCREEN, 5, 5), Some((0, 0, 30, 30)));
        assert_eq!(
            window_hit(&[cand(4, true, (0, 0, 1, 900))], 1, SCREEN, 0, 0),
            None
        );
    }

    /// 探出虚拟屏幕的那一圈根本没有像素可截，要夹回屏幕；夹完仍有面积才算选中。
    #[test]
    fn clamps_a_window_sticking_out_of_the_virtual_screen() {
        let list = [cand(6, true, (1900, 1060, 2400, 1400))];
        assert_eq!(
            window_hit(&list, 1, SCREEN, 1905, 1070),
            Some((1900, 1060, 1920, 1080))
        );
    }

    /// 副屏常在负原点：夹的左边是虚拟屏幕那条负边，不是 0。
    #[test]
    fn clamps_against_a_negative_screen_origin() {
        let wide = (-1280, 0, 1280, 1024);
        let list = [cand(6, true, (-1300, 100, -1000, 400))];
        assert_eq!(
            window_hit(&list, 1, wide, -1200, 200),
            Some((-1280, 100, -1000, 400))
        );
    }

    /// 点在空白处（没有一层罩住它）返回 None：覆盖层就此取消，什么都不截、什么都不存。
    #[test]
    fn nothing_is_picked_when_no_candidate_covers_the_point() {
        let list = [cand(7, true, (100, 100, 200, 200))];
        assert_eq!(window_hit(&list, 1, SCREEN, 900, 900), None);
    }

    #[test]
    fn negative_origin_screen_offsets_are_understood_in_client_coords() {
        // 候选是屏幕坐标，亮区要画在覆盖层自己的客户区坐标上：差一个 origin。
        let wide = (-1280, 0, 1920, 1080);
        let list = [cand(4, true, (-1000, 100, -700, 400))];
        assert_eq!(
            window_hit(&list, 7, wide, -900, 200),
            Some((-1000, 100, -700, 400))
        );
        // 屏幕坐标 -900 => 覆盖层（原点 -1280）客户区 380
        let client = hover_at(&list, 7, (-1280, 0), 3200, 1080, 380, 200);
        assert_eq!(client, Some((280, 100, 580, 400)));
    }

    #[test]
    fn norm_rect_folds_every_drag_direction() {
        assert_eq!(norm_rect(10, 20, 30, 40), (10, 20, 30, 40));
        assert_eq!(norm_rect(30, 40, 10, 20), (10, 20, 30, 40));
        assert_eq!(norm_rect(-5, 7, 3, -2), (-5, -2, 3, 7));
    }

    #[test]
    fn handles_sit_on_corners_and_edge_midpoints() {
        let pts = handle_points((100, 100, 200, 300));
        assert_eq!(
            pts,
            [
                (100, 100),
                (150, 100),
                (200, 100),
                (200, 200),
                (200, 300),
                (150, 300),
                (100, 300),
                (100, 200)
            ]
        );
        assert_eq!(hit_handle((100, 100, 200, 300), 201, 99), Some(2));
        assert_eq!(hit_handle((100, 100, 200, 300), 150, 300), Some(5));
        assert_eq!(hit_handle((100, 100, 200, 300), 150, 200), None);
    }

    #[test]
    fn each_handle_moves_only_its_own_edges() {
        let base = (100, 100, 300, 300);
        assert_eq!(resize_rect(base, 0, 80, 90, 1920, 1080), (80, 90, 300, 300));
        assert_eq!(
            resize_rect(base, 1, 200, 90, 1920, 1080),
            (100, 90, 300, 300)
        );
        assert_eq!(
            resize_rect(base, 2, 350, 120, 1920, 1080),
            (100, 120, 350, 300)
        );
        assert_eq!(
            resize_rect(base, 3, 350, 200, 1920, 1080),
            (100, 100, 350, 300)
        );
        assert_eq!(
            resize_rect(base, 4, 350, 350, 1920, 1080),
            (100, 100, 350, 350)
        );
        assert_eq!(
            resize_rect(base, 5, 200, 350, 1920, 1080),
            (100, 100, 300, 350)
        );
        assert_eq!(
            resize_rect(base, 6, 80, 350, 1920, 1080),
            (80, 100, 300, 350)
        );
        assert_eq!(
            resize_rect(base, 7, 80, 200, 1920, 1080),
            (80, 100, 300, 300)
        );
    }

    #[test]
    fn dragging_a_handle_past_the_opposite_edge_stops_at_the_minimum() {
        let base = (100, 100, 300, 300);
        // 左上角被拖到右下角之外：只能收到最小尺寸，不许翻面（翻面 = 宽高负号 = 裁出 0 像素）
        assert_eq!(
            resize_rect(base, 0, 500, 500, 1920, 1080),
            (298, 298, 300, 300)
        );
        assert_eq!(
            resize_rect(base, 4, 20, 20, 1920, 1080),
            (100, 100, 102, 102)
        );
    }

    #[test]
    fn handles_clamp_to_the_screen() {
        let base = (1900, 1060, 1920, 1080);
        assert_eq!(
            resize_rect(base, 4, 2100, 2100, 1920, 1080),
            (1900, 1060, 1920, 1080)
        );
    }

    #[test]
    fn moving_keeps_the_size_and_never_leaves_the_screen() {
        let base = (100, 100, 300, 300);
        assert_eq!(move_rect(base, 50, -20, 1920, 1080), (150, 80, 350, 280));
        assert_eq!(move_rect(base, -5000, -5000, 1920, 1080), (0, 0, 200, 200));
        assert_eq!(
            move_rect(base, 5000, 5000, 1920, 1080),
            (1720, 880, 1920, 1080)
        );
    }

    #[test]
    fn shift_arrow_grows_only_the_bottom_right() {
        let base = (100, 100, 300, 300);
        assert_eq!(grow_rect(base, 1, 1, 1920, 1080), (100, 100, 301, 301));
        assert_eq!(grow_rect(base, -1, -1, 1920, 1080), (100, 100, 299, 299));
    }

    #[test]
    fn the_veil_darkens_every_channel_and_keeps_the_pixel_opaque() {
        let src = vec![255, 128, 0, 0, 0, 0, 0, 0];
        let dim = dim_bgra(&src);
        // 255 * 140 / 255 = 140；128 * 140 / 255 = 70；0 还是 0
        assert_eq!(dim[..4], [140, 70, 0, 255]);
        assert_eq!(dim[4..], [0, 0, 0, 255]);
        // 判据对"忘了压暗"敏感：原样拷贝会在这里红
        assert_ne!(dim[0], src[0]);
    }

    #[test]
    fn mosaic_averages_each_cell_over_the_whole_cell() {
        // 4x1 图，两格（cell=2）：第一格 (0,1)=(10,30)、第二格 (50,70)
        let src: Vec<u8> = vec![10, 10, 10, 0, 30, 30, 30, 0, 50, 50, 50, 0, 70, 70, 70, 0];
        let out = mosaic_block(&src, 4, 1, 0, 0, 4, 1, 2);
        assert_eq!(&out[0..4], &[20, 20, 20, 255]);
        assert_eq!(&out[0..4], &out[4..8]);
        assert_eq!(&out[8..12], &[60, 60, 60, 255]);
        assert_eq!(&out[8..12], &out[12..16]);
    }

    #[test]
    fn mosaic_cells_are_anchored_to_the_image_not_to_the_block() {
        // 同一张 4x1 图，从 x=1 开始要 2 像素。绝对格网是 0..2 / 2..4：
        // 块里第一列只摊到图上的 x=1（值 30），第二列只摊到 x=2（值 50）。
        // 若格子改成"从块头重新数"，第一列会变成 (30+50)/2=40 —— 两块的接缝就会对不齐。
        let src: Vec<u8> = vec![10, 10, 10, 0, 30, 30, 30, 0, 50, 50, 50, 0, 70, 70, 70, 0];
        let out = mosaic_block(&src, 4, 1, 1, 0, 2, 1, 2);
        assert_eq!(&out[0..4], &[30, 30, 30, 255]);
        assert_eq!(&out[4..8], &[50, 50, 50, 255]);
    }

    #[test]
    fn mosaic_clamps_to_the_image_and_fills_the_rest_with_zero() {
        let src: Vec<u8> = vec![10, 10, 10, 0, 30, 30, 30, 0];
        let out = mosaic_block(&src, 2, 1, 1, 0, 3, 1, 2);
        // 块里第一列还在图里（取到 x=1），后两列图里根本没有 => 留 0，不许拿越界字节平均
        assert_eq!(&out[0..4], &[30, 30, 30, 255]);
        assert_eq!(&out[4..8], &[0, 0, 0, 0]);
        assert_eq!(&out[8..12], &[0, 0, 0, 0]);
    }

    #[test]
    fn crop_takes_rows_and_refuses_out_of_bounds() {
        // 4x2 图，行首分别是 0、1、2、3 / 4、5、6、7
        let src: Vec<u8> = (0..32u8).collect();
        let c = crop_rect(&src, 4, 1, 0, 2, 2).unwrap();
        assert_eq!(c.len(), 16);
        assert_eq!(&c[0..4], &[4, 5, 6, 7]);
        assert_eq!(&c[4..8], &[8, 9, 10, 11]);
        assert_eq!(&c[8..12], &[20, 21, 22, 23]);
        assert!(crop_rect(&src, 4, 3, 0, 2, 2).is_none());
        assert!(crop_rect(&src, 4, 0, 1, 4, 2).is_none());
        assert!(crop_rect(&src, 4, 0, 0, 0, 2).is_none());
    }

    #[test]
    fn the_bar_wraps_instead_of_running_off_screen() {
        let (wide, w, h) = bar_flow(2000);
        assert_eq!(wide.len(), BAR_SPECS.len());
        assert_eq!(h, BAR_ROW_H);
        assert!(w > 0);
        let (narrow, nw, nh) = bar_flow(200);
        assert!(nw <= 200, "换行后不该比可用宽度还宽：{nw}");
        assert!(nh > BAR_ROW_H, "装不下就得摊成多行");
        // 每一行内的按钮不许重叠，且都落在 [0, max_w]
        for b in &narrow {
            assert!(b.x >= 0 && b.x + b.w <= 200 + 1, "{:?} 越出可用宽度", b.id);
        }
        let mut rows: std::collections::HashMap<i32, (i32, i32)> = std::collections::HashMap::new();
        for b in &narrow {
            let e = rows.entry(b.y).or_insert((i32::MAX, i32::MIN));
            e.0 = e.0.min(b.x);
            e.1 = e.1.max(b.x + b.w);
            assert!(e.1 <= 200);
        }
        assert!(rows.len() > 1);
    }

    #[test]
    fn the_bar_flips_above_the_selection_when_there_is_no_room_below() {
        let sel = (100, 900, 400, 1000);
        let (x, y) = bar_placement(sel, 1920, 1080, 600, 70);
        assert_eq!(y, 1000 + 6);
        assert_eq!(x, 100);
        let sel2 = (100, 950, 400, 1076);
        let (_, y2) = bar_placement(sel2, 1920, 1080, 600, 70);
        assert_eq!(y2, 950 - 70 - 6);
        // 上下都没位置（一条 100 高的屏 + 顶到上沿的选区）：贴 4 像素，不许跑到负坐标
        let sel3 = (100, 0, 400, 90);
        let (_, y3) = bar_placement(sel3, 1920, 100, 600, 70);
        assert_eq!(y3, 4);
        // 横向夹回屏幕
        let (x4, _) = bar_placement((1700, 300, 1900, 500), 1920, 1080, 600, 70);
        assert_eq!(x4, 1920 - 4 - 600);
    }

    #[test]
    fn bar_buttons_land_where_the_placement_says() {
        let sel = (200, 200, 500, 400);
        let (btns, bar) = bar_rects(sel, 1920, 1080);
        assert_eq!(btns.len(), BAR_SPECS.len());
        for b in &btns {
            assert!(
                b.x >= bar.0 && b.x + b.w <= bar.0 + bar.2 + 1,
                "{:?} 越出条外",
                b.id
            );
            assert!(b.y >= bar.1 && b.y + b.h <= bar.1 + bar.3 + 1);
        }
        // id 是验收脚本的抓手：改名之前先确认它们都在
        for id in [
            "rect", "mosaic", "block", "c1", "w2", "f3", "undo", "redo", "pin", "save", "cancel",
        ] {
            assert!(btns.iter().any(|b| b.id == id), "工具条少了 {id}");
        }
    }

    #[test]
    fn two_snips_in_the_same_second_do_not_overwrite_each_other() {
        let listing = vec!["snip_20261028_120000_300x200.png".to_string()];
        assert_eq!(
            out_name("20261028_120000", 300, 200, &[]),
            "snip_20261028_120000_300x200.png"
        );
        assert_eq!(
            out_name("20261028_120000", 300, 200, &listing),
            "snip_20261028_120000_300x200_2.png"
        );
        // 尺寸不同的两张本来就不撞名，不该被加后缀
        assert_eq!(
            out_name("20261028_120000", 640, 480, &listing),
            "snip_20261028_120000_640x480.png"
        );
    }

    /// 拖一笔成什么形状；空串 = 这一笔不落。选区固定成 200x200 在 (100,100)。
    fn dragged(tool: u8, a: (i32, i32), b: (i32, i32), pts: &[(i32, i32)]) -> String {
        let sel = (100, 100, 300, 300);
        shape_from_drag(tool, a, b, pts, sel)
            .map(shape_id)
            .unwrap_or_default()
    }

    #[test]
    fn a_drag_becomes_the_shape_the_tool_promises() {
        assert_eq!(dragged(0, (110, 110), (210, 190), &[]), "rect:10,10,100,80");
        // 反向拖出来的也要是正宽高
        assert_eq!(dragged(0, (210, 190), (110, 110), &[]), "rect:10,10,100,80");
        assert_eq!(
            dragged(1, (110, 110), (210, 190), &[]),
            "arrow:10,10,110,90"
        );
        assert_eq!(
            dragged(3, (110, 110), (210, 190), &[]),
            "mosaic:10,10,100,80"
        );
        assert_eq!(
            dragged(4, (110, 110), (210, 190), &[]),
            "block:10,10,100,80"
        );
        // 太短的一笔不落（单击在标注里不是"画一个 0 宽方框"）
        assert_eq!(dragged(0, (110, 110), (111, 111), &[]), "");
        // 拖出选区外的部分被裁进来，不留下越界的笔
        assert_eq!(dragged(0, (0, 0), (400, 400), &[]), "rect:0,0,200,200");
        assert_eq!(
            dragged(2, (100, 100), (103, 104), &[(1, 2), (3, 4)]),
            "pen:2"
        );
        assert_eq!(dragged(2, (100, 100), (103, 104), &[]), "");
        // 文字工具不在这里成笔（由 EDIT 提交）
        assert_eq!(dragged(5, (110, 110), (210, 190), &[]), "");
    }

    fn shape_id(s: Shape) -> String {
        match s {
            Shape::Rect(x, y, w, h) => format!("rect:{x},{y},{w},{h}"),
            Shape::Arrow(x0, y0, x1, y1) => format!("arrow:{x0},{y0},{x1},{y1}"),
            Shape::Pen(p) => format!("pen:{}", p.len()),
            Shape::Block(x, y, w, h) => format!("block:{x},{y},{w},{h}"),
            Shape::Mosaic(x, y, w, h) => format!("mosaic:{x},{y},{w},{h}"),
            Shape::Text(x, y, t) => format!("text:{x},{y},{t}"),
        }
    }

    #[test]
    fn a_baked_block_replaces_only_the_pixels_under_it() {
        // 4x4 全 10 的一张图，中间盖一块红色（BGR 序里的 (0,45,255)）
        let img = vec![10u8; 4 * 4 * 4];
        let red = rgb(255, 45, 45);
        let marks = vec![Mark {
            shape: Shape::Block(1, 1, 2, 2),
            style: Style {
                rgb: red,
                w: 3,
                font: 30,
            },
        }];
        let out = render_marks(&img, 4, 4, &marks);
        let at = |x: i32, y: i32| {
            let i = ((y * 4 + x) as usize) * 4;
            [out[i], out[i + 1], out[i + 2]]
        };
        // 缓冲是 BGRA：内存里的字节序就是 B, G, R
        let want = [
            ((red >> 16) & 0xFF) as u8,
            ((red >> 8) & 0xFF) as u8,
            (red & 0xFF) as u8,
        ];
        assert_eq!(at(1, 1), want);
        assert_eq!(at(2, 2), want);
        // 块外面一格都不许动（烤图的判据不是"图变了"，而是"只有该变的地方变了"）
        assert_eq!(at(0, 0), [10, 10, 10]);
        assert_eq!(at(3, 3), [10, 10, 10]);
        assert_eq!(at(1, 0), [10, 10, 10]);
    }

    #[test]
    fn a_baked_mosaic_averages_the_original_pixels_under_it() {
        // 2x1 图（10 与 30）盖一格马赛克：两格合并成平均值，而不是留下原图
        let img: Vec<u8> = vec![10, 10, 10, 0, 30, 30, 30, 0];
        let marks = vec![Mark {
            shape: Shape::Mosaic(0, 0, 2, 1),
            style: Style {
                rgb: 0,
                w: 3,
                font: 30,
            },
        }];
        let out = render_marks(&img, 2, 1, &marks);
        assert_eq!(&out[0..3], &[20, 20, 20]);
        assert_eq!(&out[4..7], &[20, 20, 20]);
    }

    #[test]
    fn a_filled_block_is_exactly_the_pixels_promise() {
        let c = rgb(255, 45, 45);
        let b = fill_block(2, 1, c);
        assert_eq!(b.len(), 8);
        // BGRA：低字节是红，写进内存的顺序是 B, G, R, 255
        assert_eq!(&b[0..4], &[45, 45, 255, 255]);
        assert_eq!(&b[4..8], &[45, 45, 255, 255]);
        assert!(fill_block(0, 3, c).is_empty());
        assert!(fill_block(3, -1, c).is_empty());
    }

    #[test]
    fn baking_no_marks_returns_the_same_pixels() {
        // 快速版（没画任何东西）必须与原图逐字节相同：这条挡住"烤图顺手改了没标注的图"
        let img: Vec<u8> = (0..4 * 4 * 4).map(|i| i as u8).collect();
        assert_eq!(render_marks(&img, 4, 4, &[]), img);
    }

    #[test]
    fn repressing_the_annotate_key_abandons_but_the_quick_key_does_not() {
        // 没有在飞的会话：两条键都正常开
        assert_eq!(repress_action(false, true), Repress::Open);
        assert_eq!(repress_action(false, false), Repress::Open);
        // 在飞：标注键打断并重来，快速键不打断（它的语义是"立刻出一张"，吃掉选区更糟）
        assert_eq!(repress_action(true, true), Repress::AbandonAndOpen);
        assert_eq!(repress_action(true, false), Repress::Busy);
    }

    #[test]
    fn keys_are_decided_before_any_window_work() {
        // 未定选区：只有 Esc 有用，其它键不许"提交一个空选区"
        assert_eq!(key_action(0x1B, false, false, false), KeyAct::Cancel);
        assert_eq!(key_action(0x0D, false, false, false), KeyAct::Nothing);
        assert_eq!(key_action(0x50, false, false, false), KeyAct::Nothing);
        assert_eq!(key_action(0x27, false, false, false), KeyAct::Nothing);
        // 选区已定：Enter/P/Ctrl+S 三条出图路，Esc 放弃
        assert_eq!(key_action(0x0D, false, false, true), KeyAct::Commit);
        assert_eq!(key_action(0x53, true, false, true), KeyAct::Commit);
        // 光按 S（没 Ctrl）不是"保存"：在选区里打字时会误交
        assert_eq!(key_action(0x53, false, false, true), KeyAct::Nothing);
        assert_eq!(key_action(0x50, false, false, true), KeyAct::Pin);
        // 撤销三条：Backspace、Ctrl+Z；重做 Ctrl+Y
        assert_eq!(key_action(0x08, false, false, true), KeyAct::Undo);
        assert_eq!(key_action(0x5A, true, false, true), KeyAct::Undo);
        assert_eq!(key_action(0x5A, false, false, true), KeyAct::Nothing);
        assert_eq!(key_action(0x59, true, false, true), KeyAct::Redo);
        // 方向键：搬 1 像素，Shift 改大小
        assert_eq!(
            key_action(0x25, false, false, true),
            KeyAct::Nudge(-1, 0, false)
        );
        assert_eq!(
            key_action(0x28, false, true, true),
            KeyAct::Nudge(0, 1, true)
        );
    }

    #[test]
    fn the_active_button_follows_the_current_choice() {
        assert!(btn_active(
            "mosaic",
            3,
            PALETTE[0].0,
            WIDTHS[0].0,
            FONTS[1].0
        ));
        assert!(!btn_active(
            "mosaic",
            0,
            PALETTE[0].0,
            WIDTHS[0].0,
            FONTS[1].0
        ));
        assert!(btn_active("c3", 0, PALETTE[2].0, 3, 30));
        assert!(!btn_active("c3", 0, PALETTE[0].0, 3, 30));
        assert!(btn_active("w2", 0, PALETTE[0].0, 7, 30));
        assert!(btn_active("f1", 0, PALETTE[0].0, 3, 18));
        assert!(!btn_active("undo", 0, PALETTE[0].0, 3, 18));
    }
}
