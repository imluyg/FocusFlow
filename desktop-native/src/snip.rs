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
use std::time::Instant;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateCompatibleDC, CreateDIBSection, CreateFontW, CreatePen, CreateRectRgn,
    CreateSolidBrush, DeleteDC, DeleteObject, EndPaint, ExtCreatePen, GetDC, GetStockObject,
    GetTextExtentPoint32W, InvalidateRect, LineTo, MoveToEx, Rectangle, ReleaseDC, SelectClipRgn,
    SelectObject, SetBkMode, SetDIBitsToDevice, SetTextColor, TextOutW, BITMAPINFO, BS_SOLID,
    CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS,
    FW_NORMAL, HDC, HPEN, LOGBRUSH, NULL_BRUSH, OUT_DEFAULT_PRECIS, PAINTSTRUCT, PS_ENDCAP_ROUND,
    PS_GEOMETRIC, PS_JOIN_ROUND, PS_SOLID, TRANSPARENT,
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
/// 覆盖层**闲置**这么多毫秒就自己退（截图是"顺手按一次"的工具，忘了它整张桌面会一直被压住）。
/// 这是闲置闸：鼠标动一下、键按一下就重新计时，所以正在画的人不会被它掐掉。
const SNIP_TIMEOUT_MS: u32 = 180_000;
/// 绝对上限：标注这一条最多挂 10 分钟，松手即存那条最多 60 秒（旧 Webview 版同一对数）。
/// 有闲置闸就必须还有这一条，否则"手一直放着鼠标动"能把覆盖层挂一下午。
const SNIP_CEILING_ANNOTATE_MS: u64 = 600_000;
const SNIP_CEILING_QUICK_MS: u64 = 60_000;
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
    /// 搬第 N 笔（按下时已把当前状态压进撤销栈，所以一次拖动 = 一步撤销）
    Pick(usize),
    /// 拖第 N 笔的某个角手柄（0=左上 1=右上 2=右下 3=左下）改它的大小
    Endpoint(usize, u8),
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
    ("pick", "选", 36, true),
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
const TOOLS: [&str; 7] = ["rect", "arrow", "pen", "mosaic", "block", "text", "pick"];

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
    /// 上一次把工具条落点亮进日志时的那份选区（`None` = 这一场还没亮过）。
    ///
    /// 工具条贴在选区下面，选区一改它的落点就跟着改。日志是验收脚本唯一的瞄准依据
    /// （脚本注释写着"不自己抄一遍排版"），所以几何必须跟着选区一起更新：
    /// 原来只在选区确认那一次打，拖过角、按过方向键之后屏上的按钮已经换了地方，
    /// 而日志里还是旧的那一份 —— 按旧落点提交点到选区里，症状长得像"改完尺寸出不了图"。
    geom_sel: Option<(i32, i32, i32, i32)>,
    /// 正在拖：拖法 + 起点；`cur` 是当前点，`pen_pts` 是画笔已走过的点（选区本地坐标）。
    drag: Option<(Drag, i32, i32)>,
    cur: (i32, i32),
    pen_pts: Vec<(i32, i32)>,
    /// 悬停命中的窗口矩形（Select 阶段的亮区预览）。
    hover: Option<(i32, i32, i32, i32)>,
    /// 冻结那一刻的窗口清单（Z 序顶→底）。一次成图，鼠标移动不再 EnumWindows。
    hits: Vec<Candidate>,
    marks: Vec<Mark>,
    /// 撤销/重做按**快照**存：加一笔、搬一笔、删一笔都先压一份当前状态，
    /// 于是三条改动路共用同一个"退回去"的定义（原来只存"最后一笔"，搬/删没法撤销）。
    undo_stack: Vec<Vec<Mark>>,
    redo_stack: Vec<Vec<Mark>>,
    /// 被选中的那一笔（下标，只在"选择"工具下有意义）。
    selected: Option<usize>,
    /// 按下"搬/改角"时先记着这一笔，**真改到东西才存进撤销栈**（空点击不吃撤销步）。
    undo_pending: bool,
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
    /// 拉起这次截图的主窗口（各处传进来的都是它）：出图结果那句话说给它挂进状态读数。
    owner: isize,
    /// 出图没成时写在屏上的那句原因（覆盖层为此**不退**，Enter 再试、Esc 放弃）。
    notice: Option<String>,
    /// 冻结时把悬浮窗藏了：退出时要按原样还回去。
    pill_was: bool,
    /// 自退时限的两把表：`started_at` 只对绝对上限，`last_input` 只对闲置闸。
    /// 定时器按 [`snip_tick`] 算出的间隔反复重挂，所以动一下鼠标就把闲置那一头续上。
    started_at: Option<Instant>,
    last_input: Option<Instant>,
    ceiling_ms: u64,
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
        geom_sel: None,
        drag: None,
        cur: (0, 0),
        pen_pts: Vec::new(),
        hover: None,
        hits: Vec::new(),
        marks: Vec::new(),
        undo_stack: Vec::new(),
        redo_stack: Vec::new(),
        selected: None,
        undo_pending: false,
        tool: 0,
        color: PALETTE[0].0,
        width: WIDTHS[0].0,
        font: FONTS[1].0,
        text_edit: 0,
        text_at: (0, 0),
        overlay: 0,
        owner: 0,
        notice: None,
        pill_was: false,
        started_at: None,
        last_input: None,
        ceiling_ms: SNIP_CEILING_QUICK_MS,
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

/// 这一笔的包围盒（选区本地坐标）。文字按"字号 × 字数"估一个框：够让人点着它就行。
pub fn mark_bbox(m: &Mark) -> Option<(i32, i32, i32, i32)> {
    match &m.shape {
        Shape::Rect(x, y, w, h) | Shape::Block(x, y, w, h) | Shape::Mosaic(x, y, w, h) => {
            Some((*x, *y, *x + *w, *y + *h))
        }
        Shape::Arrow(x0, y0, x1, y1) => Some((
            (*x0).min(*x1),
            (*y0).min(*y1),
            (*x0).max(*x1),
            (*y0).max(*y1),
        )),
        Shape::Pen(pts) => {
            let (mut l, mut t, mut r, mut b) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
            for (x, y) in pts {
                l = l.min(*x);
                t = t.min(*y);
                r = r.max(*x);
                b = b.max(*y);
            }
            if pts.is_empty() {
                None
            } else {
                Some((l, t, r, b))
            }
        }
        Shape::Text(x, y, s) => {
            // 至少占一个字号宽：空串也不许缩成一个点，否则选中它得像素级准
            let f = m.style.font.max(1);
            let w = (s.chars().count() as i32 * f).max(f);
            Some((*x, *y, x + w, y + f))
        }
    }
}

/// 这一点压在这一笔上了吗。空心形状按"包围盒 + 容差"：粗笔本来就该宽松点得到。
pub fn mark_hit(m: &Mark, x: i32, y: i32) -> bool {
    let tol = (m.style.w / 2).max(4);
    match &m.shape {
        Shape::Rect(..) | Shape::Arrow(..) | Shape::Pen(..) => match mark_bbox(m) {
            Some((l, t, r, b)) => x >= l - tol && x <= r + tol && y >= t - tol && y <= b + tol,
            None => false,
        },
        Shape::Block(l, t, w, h) | Shape::Mosaic(l, t, w, h) => {
            x >= *l && x < *l + *w && y >= *t && y < *t + *h
        }
        Shape::Text(..) => match mark_bbox(m) {
            Some((l, t, r, b)) => x >= l && x <= r && y >= t && y <= b,
            None => false,
        },
    }
}

/// 从最上面那笔往下找：后画的盖在前面那笔上，取消时也该先被拿到。
pub fn topmost_hit(marks: &[Mark], x: i32, y: i32) -> Option<usize> {
    marks
        .iter()
        .enumerate()
        .rev()
        .find(|(_, m)| mark_hit(m, x, y))
        .map(|(i, _)| i)
}

/// 把一笔整体搬 (dx,dy)。坐标都是选区局部 ⇒ 搬完直接就是最终位置，落盘不必再换算。
pub fn move_mark(m: &mut Mark, dx: i32, dy: i32) {
    match &mut m.shape {
        Shape::Rect(x, y, ..) | Shape::Block(x, y, ..) | Shape::Mosaic(x, y, ..) => {
            *x += dx;
            *y += dy;
        }
        Shape::Arrow(x0, y0, x1, y1) => {
            *x0 += dx;
            *y0 += dy;
            *x1 += dx;
            *y1 += dy;
        }
        Shape::Pen(pts) => {
            for p in pts.iter_mut() {
                p.0 += dx;
                p.1 += dy;
            }
        }
        Shape::Text(x, y, _) => {
            *x += dx;
            *y += dy;
        }
    }
}

/// 选中那一笔的四个角手柄（选区本地坐标）。文字没有角：它跟着字号走。
pub fn endpoint_points(m: &Mark) -> Option<[(i32, i32); 4]> {
    match m.shape {
        Shape::Text(..) => None,
        _ => mark_bbox(m).map(|(l, t, r, b)| [(l, t), (r, t), (r, b), (l, b)]),
    }
}

/// 压中了选中那一笔的哪个角（与选区手柄同一套 8 像素命中框）。
pub fn hit_endpoint(m: &Mark, x: i32, y: i32) -> Option<u8> {
    let half = HANDLE_PX / 2;
    for (i, (hx, hy)) in endpoint_points(m)?.iter().enumerate() {
        if x >= hx - half && x <= hx + half && y >= hy - half && y <= hy + half {
            return Some(i as u8);
        }
    }
    None
}

/// 把选中那一笔的某个角拖到 (x,y)。按形状的语义改：块/框/马赛克改包围盒（不许翻面，
/// 最小留 1 像素），箭头按左右两侧改起点或终点，画笔改首/尾那个点，文字不动。
pub fn resize_mark(m: &mut Mark, idx: u8, x: i32, y: i32) {
    match &mut m.shape {
        Shape::Rect(l, t, w, h) | Shape::Block(l, t, w, h) | Shape::Mosaic(l, t, w, h) => {
            let (r0, b0) = (*l + *w, *t + *h);
            let (mut nl, mut nt, mut nr, mut nb) = (*l, *t, r0, b0);
            match idx {
                0 => {
                    nl = x;
                    nt = y;
                }
                1 => {
                    nr = x;
                    nt = y;
                }
                2 => {
                    nr = x;
                    nb = y;
                }
                _ => {
                    nl = x;
                    nb = y;
                }
            }
            *l = nl.min(nr);
            *t = nt.min(nb);
            *w = (nr.max(nl) - *l).max(1);
            *h = (nb.max(nt) - *t).max(1);
        }
        Shape::Arrow(x0, y0, x1, y1) => match idx {
            0 | 3 => {
                *x0 = x;
                *y0 = y;
            }
            _ => {
                *x1 = x;
                *y1 = y;
            }
        },
        Shape::Pen(pts) => {
            let target = if idx < 2 {
                pts.first_mut()
            } else {
                pts.last_mut()
            };
            if let Some(p) = target {
                p.0 = x;
                p.1 = y;
            }
        }
        Shape::Text(..) => {}
    }
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

unsafe fn draw_chip(hdc: HDC, x: i32, y: i32, text: &str, fg: u32) {
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
        let _ = SetTextColor(hdc, COLORREF(fg));
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
        "pick" => tool == 6,
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
        // 选区一改，工具条就跟着换地方。日志里那份几何是验收脚本唯一的瞄准依据，
        // 所以改过之后要重新亮一次；只在真的变了时打，每帧都打会把日志刷满。
        let republish = SNIP
            .lock()
            .map(|mut s| {
                if s.geom_sel == Some(sel) {
                    false
                } else {
                    s.geom_sel = Some(sel);
                    true
                }
            })
            .unwrap_or(false);
        if republish {
            for b in &btns {
                tracing_note(&format!(
                    "snip-btn {} {} {} {} {}",
                    b.id, b.x, b.y, b.w, b.h
                ));
            }
        }
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

/// 把当前会话的选区与标注合成出最终像素（屏上预览与出图共用这一条合成路）。
/// 返回烤好的 BGRA（自上而下）与宽高；没有有效选区、或选区越出冻结画面都回 `None`。
///
/// 单独成一段是为了"所见即所得"能被机器判：屏上探针拿这一刻的像素写进**临时目录**再解回来对，
/// 不必碰真实的 `data/screenshots`，也不必覆盖用户的剪贴板（那两条只能在独立进程里跑）。
fn baked_selection() -> Option<(Vec<u8>, i32, i32)> {
    let (base, base_w, sel, marks) = match SNIP.lock() {
        Ok(s) => (s.base.clone(), s.base_w, s.sel, s.marks.clone()),
        Err(_) => return None,
    };
    let (l, t, r, b) = sel;
    let (w, h) = (r - l, b - t);
    if w < MIN_SEL || h < MIN_SEL || base.is_empty() {
        tracing_note("截图取消：没有有效选区");
        return None;
    }
    match crop_rect(&base, base_w, l, t, w, h) {
        Some(img) => Some((render_marks(&img, w, h, &marks), w, h)),
        None => {
            tracing_note("截图失败：选区越出冻结画面");
            None
        }
    }
}

/// BGRA → PNG 字节（走 core 那一份：alpha 钉成 255，否则写出来是一张全透明的图）。
fn png_bytes(buf: &[u8], w: i32, h: i32) -> std::result::Result<Vec<u8>, String> {
    let mut rgba = buf.to_vec();
    focusflow_core::capture::bgra_to_rgba(&mut rgba);
    focusflow_core::capture::encode_png(&rgba, w as u32, h as u32)
}

/// 把已编好的 PNG 落进 `dir`，返回写出去的路径。
///
/// 目录由调用方给：产品用 `screenshots_dir()`，屏上探针用临时目录。
/// 文件名带宽高（`snip_<时间戳>_<宽>x<高>.png`），同秒连按靠 `_2` 区分 ——
/// 只到秒的旧命名会让两张同名互相覆盖，连按两次只剩一张。
fn write_png_named(
    dir: &std::path::Path,
    png: &[u8],
    w: i32,
    h: i32,
) -> std::result::Result<std::path::PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("建目录失败：{e}"))?;
    let listing: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
    let name = out_name(&ts, w as u32, h as u32, &listing);
    let path = dir.join(&name);
    std::fs::write(&path, png).map_err(|e| format!("写盘失败：{e}"))?;
    Ok(path)
}

/// 一次出图之后覆盖层留不留，只看这两件事的成败。
///
/// 病状是"失败了却不吭声，还顺手把唯一一次重试机会关掉"：原来无论成败都 `DestroyWindow`，
/// 写盘失败只在日志里留一行，屏上什么都没了——人只知道"没截到"，不知道为什麼，也无法再按一次。
/// 落盘失败 ⇒ 这张图真的没留下，必须留在覆盖层上让他再试；只丢剪贴板 ⇒ 文件已在，
/// 但"以为复制上了"会让他粘到别处才发现是空的，所以也要说出来。
pub fn commit_verdict(
    save: std::result::Result<(), String>,
    clip: std::result::Result<(), String>,
) -> Verdict {
    match (save, clip) {
        (Ok(()), Ok(())) => Verdict::Done,
        (Err(e), _) => Verdict::Stay(format!("没存盘：{e} —— 改好了再按 Enter，Esc 放弃")),
        (Ok(()), Err(e)) => Verdict::Stay(format!("已存盘，但剪贴板没进去：{e} —— Enter 再试")),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    /// 两件事都成：覆盖层可以退了
    Done,
    /// 有一件没成：覆盖层留着，把这句原因写在屏上
    Stay(String),
}

/// 松手那一下的三种结局（旧 Webview 版同一口径）：**拖够了就出图**、**没拖够但压着别的窗口
/// 就截那个窗口**、**两边都不成才算这次取消**。
///
/// 写成纯函数是因为这两条边界都容易在改动里被写反：差一个像素就不算"框住"，
/// 而"随手一按"绝不该等于"取消"——那正是用户想要的"截这个窗口"。拖够时优先出图，
/// 否则人拖完框手一抖压到别的窗口，选区会被换掉。
pub fn release_outcome(sel: (i32, i32, i32, i32), hit: Option<(i32, i32, i32, i32)>) -> Release {
    if sel.2 - sel.0 >= MIN_DRAG && sel.3 - sel.1 >= MIN_DRAG {
        Release::Commit
    } else if let Some(rc) = hit {
        Release::PickWindow(rc)
    } else {
        Release::Cancel
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Release {
    /// 拖出来的区域够大：按现在这套去向出图
    Commit,
    /// 没拖出区域但压着一个窗口：把它当选区
    PickWindow((i32, i32, i32, i32)),
    /// 两边都不成：这次截图取消
    Cancel,
}

/// 出图成了之后给状态读数那一行的话：文件名、尺寸、第二个去处。
///
/// 旧 Webview 版是主窗口一个 toast 说同一件事（`snip-done`），原生没有 toast——用它已有那条
/// 状态读数，"周报已生成 / 备份失败：…" 已经显示在那儿。写成纯函数是因为这句话要能被断言：
/// 只写在日志里的话，用户永远得自己去翻。
pub fn snip_receipt(file: &str, w: i32, h: i32, pinned: bool) -> String {
    // 走到这句话时"落盘 + 进剪贴板"两件都已确认成了（`pin` 那条还多钉了一张），
    // 所以话里的每一样都得说，也别多出没做的那样。
    let tail = if pinned { "，并钉在桌面上" } else { "" };
    format!("截图已存 {file}（{w}x{h}），同时复制到剪贴板{tail}")
}

/// 一次出图：合成 → 落盘 → 进剪贴板（CF_DIB 与 `PNG` 两份，编码只做一次）。
/// `pin` = 顺手把这一张钉在桌面上（不再多落一个文件）。
///
/// 去向由调用方给：产品传真实的 `screenshots_dir()` 且 `clipboard = true`；
/// 屏上探针传一个临时目录且 `clipboard = false`——那条路不许往用户目录里写，
/// 也不许盖掉用户的剪贴板（这两件事只能在独立进程的验收套件里判）。
/// `clipboard = false` 时那一路当作成功，判的就只是"落盘成不成"这一半。
fn commit_into(dir: &std::path::Path, clipboard: bool, pin: bool) -> Verdict {
    let (origin, sel, owner) =
        SNIP.lock()
            .map(|s| (s.origin, s.sel, s.owner))
            .unwrap_or(((0, 0), (0, 0, 0, 0), 0));
    let Some((buf, w, h)) = baked_selection() else {
        return Verdict::Stay("没有可出的图：先框一个区域，再按 Enter".to_string());
    };
    let png = match png_bytes(&buf, w, h) {
        Ok(p) => p,
        Err(e) => {
            tracing_note(&format!("截图编码失败：{e}"));
            return Verdict::Stay(format!("图没编出来：{e}"));
        }
    };

    // 落成的文件名要带给状态读数那句，所以先把它接住；绝对路径另外接一份给 CF_HDROP
    let mut saved: Option<String> = None;
    let mut saved_abs: Option<std::path::PathBuf> = None;
    let save = match write_png_named(dir, &png, w, h) {
        Ok(path) => {
            tracing_note(&format!("截图已保存：{} ({w}x{h})", path.display()));
            saved = path.file_name().map(|n| n.to_string_lossy().to_string());
            saved_abs = Some(path);
            Ok(())
        }
        Err(e) => {
            tracing_note(&format!("截图写盘失败：{e}"));
            Err(e)
        }
    };
    let clip = if !clipboard {
        Ok(())
    } else {
        match focusflow_core::capture::win::write_image_to_clipboard(
            &buf,
            w as u32,
            h as u32,
            &png,
            saved_abs.as_deref(),
        ) {
            Ok(formats) => {
                // 报的是**实际进去的那几种**：CF_HDROP 进不去时症状是"往文件夹里粘不出文件"，
                // 这一句要能看出它没进去。
                tracing_note(&format!(
                    "截图已进剪贴板：{w}x{h} {}",
                    if formats.is_empty() {
                        "（一种都没写进去）".to_string()
                    } else {
                        formats.join("+")
                    }
                ));
                Ok(())
            }
            Err(e) => {
                tracing_note(&format!("截图进剪贴板失败：{e}"));
                Err(e)
            }
        }
    };
    if pin {
        // 贴图这条以"钉上了"为成交：文件/剪贴板没成都已在日志里出声，
        // 而钉好的那张还在桌上可以右键再处理，不该把覆盖层压着不放。
        open_pin(buf, w, h, origin.0 + sel.0, origin.1 + sel.1);
        if let Some(name) = &saved {
            crate::note_snip_result(
                HWND(owner as *mut core::ffi::c_void),
                snip_receipt(name, w, h, true),
            );
        }
        return Verdict::Done;
    }
    let v = commit_verdict(save, clip);
    // 成了才出声；没成走的是 `Stay` 那一路——那句原因由调用点在屏上写给用户看
    if let (Verdict::Done, Some(name)) = (&v, &saved) {
        crate::note_snip_result(
            HWND(owner as *mut core::ffi::c_void),
            snip_receipt(name, w, h, false),
        );
    }
    v
}

/// 出图并据此收场：成则关窗，败则把原因写在屏上、覆盖层留着等第二次 Enter。
/// 五个调用点（工具条"保存"与"贴图"、松手即存、Enter/Ctrl+S、P）都走这条，去向都是真实的。
unsafe fn commit_and_close(hwnd: HWND, pin: bool) {
    let dir = focusflow_core::paths::screenshots_dir();
    commit_and_close_into(hwnd, pin, &dir, true);
}

/// 上面那条的实现体，只把"落到哪个目录、要不要动剪贴板"留给调用方。
/// 屏上探针要靠它端到端判"写盘失败不退、写成就退"这两条，而真实去向会往用户的
/// `data/screenshots` 落一张、还会盖掉他的剪贴板——那两件事只能在独立进程的验收套件里做。
unsafe fn commit_and_close_into(hwnd: HWND, pin: bool, dir: &std::path::Path, clipboard: bool) {
    match commit_into(dir, clipboard, pin) {
        Verdict::Done => {
            let _ = DestroyWindow(hwnd);
        }
        Verdict::Stay(msg) => {
            tracing_note(&format!("截图没出图：{msg}"));
            let mut owner = 0isize;
            if let Ok(mut s) = SNIP.lock() {
                s.notice = Some(msg.clone());
                // 快速那条本来"松手即存"就完事：正因为没存上才把它留在标注阶段，
                // 否则屏上没有一个"再试一次"的落点，等于把这句原因也一起吞了。
                s.phase = Phase::Adjust;
                s.drag = None;
                // 这一场事实上已经变成标注场：绝对上限得跟着改。快速那条只有 60 秒，
                // 不跟着改的话，用户还在读那句原因、还在腾磁盘，第一记定时器就把覆盖层收走了。
                s.ceiling_ms = snip_ceiling_ms(true);
                owner = s.owner;
            }
            // 同一句话也递给主窗口那条状态读数：覆盖层早晚要退，原因得留个影子
            crate::note_snip_result(HWND(owner as *mut core::ffi::c_void), msg);
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
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
    /// 搬第 N 笔
    Pick(usize),
    /// 改第 N 笔的某个角
    Endpoint(usize, u8),
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
                if let Ok(mut s) = SNIP.lock() {
                    s.last_input = Some(Instant::now());
                }
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
                                if s.tool == 6 {
                                    // 先判选中那一笔的角（改形状），再判压住了哪一笔（搬它）
                                    if let Some(i) = s.selected {
                                        if let Some(mk) = s.marks.get(i) {
                                            if let Some(e) =
                                                hit_endpoint(mk, x - s.sel.0, y - s.sel.1)
                                            {
                                                return Press::Endpoint(i, e);
                                            }
                                        }
                                    }
                                    if let Some(i) = topmost_hit(&s.marks, x - s.sel.0, y - s.sel.1)
                                    {
                                        return Press::Pick(i);
                                    }
                                }
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
                    Press::Pick(i) => {
                        if let Ok(mut s) = SNIP.lock() {
                            // 快照要等**真拖动了**再存：点在色块上又原样松开（想看看它选中没）
                            // 不该吃掉一步撤销。
                            s.undo_pending = true;
                            s.selected = Some(i);
                            s.drag = Some((Drag::Pick(i), x, y));
                            s.cur = (x, y);
                        }
                        let _ = SetCapture(hwnd);
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    Press::Endpoint(i, e) => {
                        if let Ok(mut s) = SNIP.lock() {
                            s.undo_pending = true;
                            // 按住角就等于还选中着它：push_undo 会把选中清掉，那四个角会跟着不见
                            s.selected = Some(i);
                            s.drag = Some((Drag::Endpoint(i, e), x, y));
                            s.cur = (x, y);
                        }
                        let _ = SetCapture(hwnd);
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    Press::Mark => {
                        let _ = SetCapture(hwnd);
                        if let Ok(mut s) = SNIP.lock() {
                            if s.tool == 6 {
                                s.selected = None;
                            }
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
                    // 动过鼠标就算"人还在"：闲置那把表从这一刻重新数（绝对上限不受影响）
                    s.last_input = Some(Instant::now());
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
                        Some((Drag::Endpoint(i, e), _, _)) => {
                            if s.cur != (x, y) {
                                s.cur = (x, y);
                                // 拖不出选区：局部坐标夹进 [0, 宽高]，越界只会停在边上
                                let (sw, sh) = (s.sel.2 - s.sel.0, s.sel.3 - s.sel.1);
                                let lx = (x - s.sel.0).clamp(0, sw);
                                let ly = (y - s.sel.1).clamp(0, sh);
                                if i < s.marks.len() {
                                    push_undo_if_pending(&mut s);
                                    resize_mark(&mut s.marks[i], e, lx, ly);
                                    changed = true;
                                }
                            }
                        }
                        Some((Drag::Pick(i), _, _)) => {
                            if s.cur != (x, y) {
                                let (dx, dy) = (x - s.cur.0, y - s.cur.1);
                                s.cur = (x, y);
                                if i < s.marks.len() {
                                    push_undo_if_pending(&mut s);
                                    move_mark(&mut s.marks[i], dx, dy);
                                    changed = true;
                                }
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
                        let (sel, annotate, hit) = SNIP
                            .lock()
                            .map(|s| {
                                (
                                    s.sel,
                                    s.annotate,
                                    hover_at(
                                        &s.hits,
                                        GetCurrentProcessId(),
                                        s.origin,
                                        s.base_w,
                                        s.base_h,
                                        x,
                                        y,
                                    ),
                                )
                            })
                            .unwrap_or(((0, 0, 0, 0), true, None));
                        match release_outcome(sel, hit) {
                            Release::Commit => finish_selection(hwnd, annotate),
                            Release::PickWindow(rc) => {
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
                            Release::Cancel => {
                                tracing_note("截图取消：这一笔既没框出区域，也没点到别的窗口");
                                let _ = DestroyWindow(hwnd);
                            }
                        }
                    }
                    Some((Drag::Handle(_), _, _))
                    | Some((Drag::Pick(_), _, _))
                    | Some((Drag::Endpoint(_, _), _, _)) => {
                        if let Ok(mut s) = SNIP.lock() {
                            s.drag = None;
                            // 一路没动过就把那个"待存"的快照机会收掉：一次空点击不算一步撤销
                            s.undo_pending = false;
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
                                push_undo(&mut s);
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
                // 选区还没定：右键＝放弃这一次（旧口径）。选区已定、正在标注：右键不许把已经
                // 画好的这一整堆丢掉——旧 Webview 版在 annotating 时直接 return，这里同一口径；
                // 要退有 Esc 和工具条上那颗取消。
                let adjusting = SNIP
                    .lock()
                    .map(|s| s.phase == Phase::Adjust)
                    .unwrap_or(false);
                if adjusting {
                    tracing_note("截图右键在标注阶段不打断（要退用 Esc 或工具条的取消）");
                } else {
                    tracing_note("截图取消：右键");
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                if let Ok(mut s) = SNIP.lock() {
                    s.last_input = Some(Instant::now());
                }
                on_key(hwnd, w.0);
                LRESULT(0)
            }
            WM_EDIT_DONE => {
                // `lParam` 带的是"哪一个输入框"，只有还是当前那一个才处理：
                // 旧框在 open_text_edit 里已经同步收掉并落好了字，它那条 WM_KILLFOCUS 事后才投到这里，
                // 而此刻 `text_edit` 已经是新框 —— 不认句柄就会把刚开出来的输入框当成旧的销毁掉。
                let (te, at) = SNIP
                    .lock()
                    .map(|s| (s.text_edit, s.text_at))
                    .unwrap_or((0, (0, 0)));
                let h = HWND(l.0 as *mut core::ffi::c_void);
                if te != 0 && h.0 as isize == te {
                    finish_text_edit(h, at, w.0 == 1);
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
                    let next = SNIP
                        .lock()
                        .map(|s| match (s.started_at, s.last_input) {
                            (Some(opened), Some(act)) => Some(snip_tick(
                                SNIP_TIMEOUT_MS as i64 - act.elapsed().as_millis() as i64,
                                s.ceiling_ms as i64 - opened.elapsed().as_millis() as i64,
                            )),
                            // 表没上过（覆盖层不是 start() 建的）：按闲置到点退掉，别让它长挂
                            _ => None,
                        })
                        .unwrap_or(None);
                    match next {
                        Some(SnipTick::Wait(ms)) => {
                            // 重挂同一把表：闲置那一路由此从当下重新数，正在画的人不会被掐掉
                            let _ = SetTimer(Some(hwnd), SNIP_TIMEOUT_TIMER, ms, None);
                        }
                        Some(SnipTick::Absolute) => {
                            tracing_note("截图 snip-timeout 挂到绝对上限了，覆盖层自己退");
                            let _ = DestroyWindow(hwnd);
                        }
                        _ => {
                            tracing_note("截图 snip-timeout 三分钟没动，覆盖层自己退");
                            let _ = DestroyWindow(hwnd);
                        }
                    }
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
                    s.undo_stack.clear();
                    s.redo_stack.clear();
                    s.selected = None;
                    s.undo_pending = false;
                    s.hits.clear();
                    s.text_edit = 0;
                    s.overlay = 0;
                    s.owner = 0;
                    s.notice = None;
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
        t @ ("rect" | "arrow" | "pen" | "mosaic" | "block" | "text" | "pick") => {
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
        "undo" => undo(),
        "redo" => redo_mark(),
        "save" => commit_and_close(hwnd, false),
        "pin" => commit_and_close(hwnd, true),
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
        commit_and_close(hwnd, false);
        return;
    }
    if let Ok(mut s) = SNIP.lock() {
        s.drag = None;
        s.pen_pts.clear();
        s.phase = Phase::Adjust;
        s.hover = None;
        s.marks.clear();
        s.undo_stack.clear();
        s.redo_stack.clear();
        s.selected = None;
    }
    // 把工具条每颗按钮的落点亮进日志：验收脚本据此点按钮，而不是自己抄一遍排版。
    // 这一句只覆盖"刚进标注"那一刻；之后选区每改一次，`draw_bar` 再补一份新的落点。
    if let Ok(mut s) = SNIP.lock() {
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
        s.geom_sel = Some(s.sel);
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
    /// 删掉当前选中的那一笔（没选中就等于没这个动作）
    DeleteSelected,
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
        0x2E => KeyAct::DeleteSelected,
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
            commit_and_close(hwnd, false);
        }
        KeyAct::Pin => {
            commit_and_close(hwnd, true);
        }
        KeyAct::Undo => undo(),
        KeyAct::Redo => redo_mark(),
        KeyAct::DeleteSelected => {
            let mut del = false;
            if let Ok(mut s) = SNIP.lock() {
                if let Some(i) = s.selected {
                    if i < s.marks.len() {
                        push_undo(&mut s);
                        s.marks.remove(i);
                        del = true;
                    }
                }
                s.selected = None;
            }
            if del {
                tracing_note("截图标注 delete 删掉选中的那一笔");
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
        }
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

/// 改动任何一笔之前先压一份当前状态：一次加笔、一次拖动、一次删除都恰好是一步撤销。
fn push_undo(s: &mut SnipState) {
    s.undo_stack.push(s.marks.clone());
    s.redo_stack.clear();
    s.selected = None;
}

/// "按下手等拖动"这一族（搬一笔、拖一个角）的快照闩：第一次真的改到东西才存撤销快照。
/// 原样松开就吃掉一步撤销是不对的——用户按了一下只想看它选中没有，
/// 撤销栈里却多了一层"和现在一模一样"的状态，下一次撤销按下去屏幕什么都不变。
fn push_undo_if_pending(s: &mut SnipState) {
    if s.undo_pending {
        s.undo_pending = false;
        s.undo_stack.push(s.marks.clone());
        s.redo_stack.clear();
    }
}

fn undo() {
    if let Ok(mut s) = SNIP.lock() {
        if let Some(prev) = s.undo_stack.pop() {
            let cur = s.marks.clone();
            s.redo_stack.push(cur);
            s.marks = prev;
            s.selected = None;
        }
    }
}

fn redo_mark() {
    if let Ok(mut s) = SNIP.lock() {
        if let Some(next) = s.redo_stack.pop() {
            let cur = s.marks.clone();
            s.undo_stack.push(cur);
            s.marks = next;
            s.selected = None;
        }
    }
}

/// 把输入框里的字落成一笔标注（`keep=false` 只丢不留，对应 Esc），然后收掉那个框。
/// 调用时不许持着 `SNIP` 的锁：既要用 `GetWindowTextW` 问那个框，也要改 `marks`。
unsafe fn finish_text_edit(h: HWND, at: (i32, i32), keep: bool) {
    unsafe {
        if keep {
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
                    push_undo(&mut s);
                    s.marks.push(Mark {
                        shape: Shape::Text(at.0, at.1, text),
                        style: st,
                    });
                }
            }
        }
        let _ = DestroyWindow(h);
    }
}

/// 在覆盖层上开一个文字输入框（原生 EDIT，自带中文输入法）。落点存的是选区本地坐标。
unsafe fn open_text_edit(overlay: HWND, x: i32, y: i32) {
    unsafe {
        let old = SNIP
            .lock()
            .map(|s| (s.text_edit, s.text_at))
            .unwrap_or((0, (0, 0)));
        if old.0 != 0 {
            // 上一个框还挂着：先把它打好的字落下来再收（旧版靠 blur 提交，直接 DestroyWindow
            // 会把已经打进去的那行字吃掉——点在画布上本来是"换个地方再写一条"的意思）
            finish_text_edit(HWND(old.0 as *mut core::ffi::c_void), old.1, true);
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

/// EDIT 的子类过程：Enter 提交、Esc 只丢这一条文字、失焦按提交算（旧版 blur 就落字）。
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
                    let _ = PostMessageW(
                        Some(parent),
                        WM_EDIT_DONE,
                        WPARAM(code),
                        LPARAM(hwnd.0 as isize),
                    );
                }
                LRESULT(0)
            }
            WM_KILLFOCUS => {
                // 只有**打过字**的失焦才当提交。空框的失焦不提交也不销毁：
                // 隔壁那颗行内重命名的 EDIT 实测过"刚建出来就收到一次假 WM_KILLFOCUS"
                // （main.rs 里那条注释，2026-10-26），照这里无条件收就会把刚点开的输入框
                // 当场毁掉——症状是"文字工具点了没框"。真要把空框丢掉，用 Esc。
                if GetWindowTextLengthW(hwnd) > 0 {
                    if let Ok(parent) = GetParent(hwnd) {
                        let _ = PostMessageW(
                            Some(parent),
                            WM_EDIT_DONE,
                            WPARAM(1),
                            LPARAM(hwnd.0 as isize),
                        );
                    }
                }
                LRESULT(0)
            }
            WM_CHAR if w.0 == 0x0D || w.0 == 0x1B => LRESULT(0),
            // 敲字也算"人还在"：这些键落在子窗口身上，覆盖层自己一条 WM_KEYDOWN 都收不到。
            // 不补这一下，认真打一行标题超过 180 秒的人会被闲置闸连框带字一起掐掉。
            WM_KEYDOWN | WM_CHAR => {
                if let Ok(mut s) = SNIP.lock() {
                    s.last_input = Some(Instant::now());
                }
                DefSubclassProc(hwnd, msg, w, l)
            }
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
            Some((Drag::New, _, _))
            | Some((Drag::Handle(_), _, _))
            | Some((Drag::Mark, _, _))
            | Some((Drag::Pick(_), _, _))
            | Some((Drag::Endpoint(_, _), _, _)) => Some(sel),
            None => {
                if phase == Phase::Adjust {
                    Some(sel)
                } else {
                    hover
                }
            }
        };
        if let Some(rc) = bright {
            let rg = CreateRectRgn(rc.0, rc.1, rc.2, rc.3);
            let _ = SelectClipRgn(hdc, Some(rg));
            blit_bgra(hdc, &base, bw, bh, 0, 0);
            // 已画的标注：与烤图同一条路（色块/马赛克按像素算，其余走 GDI）
            for m in &marks {
                draw_live(hdc, &base, bw, bh, rc, m);
            }
            // 选中的那一笔套一个白框：只在屏上画，不进合成结果
            if let Some(i) = SNIP.lock().ok().and_then(|s| s.selected) {
                if let Some(m) = marks.get(i) {
                    if let Some((l, t, r, b)) = mark_bbox(m) {
                        let pen = CreatePen(PS_SOLID, 1, COLORREF(0x00FF_FFFF));
                        let oldp = SelectObject(hdc, pen.into());
                        let null = GetStockObject(NULL_BRUSH);
                        let oldb = SelectObject(hdc, null);
                        let _ =
                            Rectangle(hdc, rc.0 + l - 3, rc.1 + t - 3, rc.0 + r + 3, rc.1 + b + 3);
                        SelectObject(hdc, oldb);
                        SelectObject(hdc, oldp);
                        let _ = DeleteObject(pen.into());
                        // 角上有四个小白块：不看也知道能拖哪里（与选区手柄同一形状，小一档）
                        for (hx, hy) in endpoint_points(m).unwrap_or([(0, 0); 4]) {
                            let brush = CreateSolidBrush(COLORREF(0x00FF_FFFF));
                            let old = SelectObject(hdc, brush.into());
                            let hp = CreatePen(PS_SOLID, 1, COLORREF(ACCENT));
                            let oldpen = SelectObject(hdc, hp.into());
                            let _ = Rectangle(
                                hdc,
                                rc.0 + hx - 3,
                                rc.1 + hy - 3,
                                rc.0 + hx + 3,
                                rc.1 + hy + 3,
                            );
                            SelectObject(hdc, oldpen);
                            SelectObject(hdc, old);
                            let _ = DeleteObject(hp.into());
                            let _ = DeleteObject(brush.into());
                        }
                    }
                }
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
            // 选区外框 + 手柄：**先退出裁剪区**再画。
            // `SelectClipRgn(hdc, None)` 才是"回到可见区域"；别拿 `GetClipRgn` 复制出来的那份去
            // 恢复 —— BeginPaint 的 DC 上它常返回 0 且区域是**空的**，选回去等于把边框、手柄、
            // 尺寸读数与整条工具条全部裁掉（这条由屏上探针逮到：整幅里一个橙/白像素都没有）。
            let _ = SelectClipRgn(hdc, None);
            let _ = DeleteObject(rg.into());
            // 尺寸读数先画：它的深色底盒会从选区上沿往下压几个像素，后画的话会把左上角
            // 那颗手柄糊掉（屏上探针量到那颗是 [0,0,0]）。
            draw_chip(
                hdc,
                rc.0,
                rc.1 - 24,
                &format!("{} × {}", rc.2 - rc.0, rc.3 - rc.1),
                0x00FF_FFFF,
            );
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
        }
        if phase == Phase::Adjust {
            draw_bar(hdc, sel, bw, bh);
        }
        // 出图没成的那句写在选区上沿外面（贴到屏幕顶就改画在选区里侧）：这一句是给人看的，
        // 只在日志里出声等于没出声——覆盖层正是为它才不退的。
        let notice = SNIP.lock().ok().and_then(|s| s.notice.clone());
        if let Some(msg) = notice {
            let ny = if sel.1 - 48 >= 0 {
                sel.1 - 48
            } else {
                sel.1 + 26
            };
            draw_chip(hdc, sel.0, ny, &msg, rgb(255, 60, 60));
        }
        // 还没拖、也还没定选区时把这一场的用法写在屏上（旧 Webview 版有这么一行，重写时丢了）：
        // 覆盖层吃掉整个屏幕，人要的是"接下来能干什么"，不是去翻日志。
        if phase == Phase::Select && drag.is_none() {
            draw_chip(
                hdc,
                24,
                12,
                "拖框选区 · 单击选窗口 · 右键或 Esc 取消 · 定下后可用方向键挪 1 像素",
                0x00FF_FFFF,
            );
        }
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

/// 覆盖层那把自退表怎么读：**闲置**与**绝对**两条一起判（旧 Webview 版就是这个双闸）。
///
/// 只有闲置闸，一个不时动一下鼠标的人能把覆盖层挂一整天；只有绝对闸，认真标注的人会被
/// 三分钟那一刀掐掉。两个都还剩 ⇒ 取更近的那一段重挂定时器，于是绝对那一条能按点落地，
/// 而闲置那一条每被续一次就从当下重新数。
pub fn snip_tick(idle_left: i64, absolute_left: i64) -> SnipTick {
    if absolute_left <= 0 {
        SnipTick::Absolute
    } else if idle_left <= 0 {
        SnipTick::Idle
    } else {
        SnipTick::Wait(idle_left.min(absolute_left) as u32)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SnipTick {
    /// 还早，这么多毫秒之后再看一次
    Wait(u32),
    /// 闲置到点：没人碰它，自己退
    Idle,
    /// 绝对上限到点：哪怕正在动也要退
    Absolute,
}

/// 这一次会话的**绝对上限**给多少毫秒：标注模式 10 分钟、松手即存 60 秒。
/// 写成一个小函数是因为这两个数与闲置闸 180 秒的关系是硬的：
/// 标注的上限必须比闲置大（否则"续表"没意义），快速的必须比闲置小（否则第一枪不是它）。
pub fn snip_ceiling_ms(annotate: bool) -> u64 {
    if annotate {
        SNIP_CEILING_ANNOTATE_MS
    } else {
        SNIP_CEILING_QUICK_MS
    }
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
                    s.geom_sel = None;
                    s.drag = None;
                    s.cur = (0, 0);
                    s.pen_pts.clear();
                    s.hover = None;
                    s.marks.clear();
                    s.undo_stack.clear();
                    s.redo_stack.clear();
                    s.selected = None;
                    s.undo_pending = false;
                    s.hits = hits;
                    s.overlay = ov.0 as isize;
                    s.owner = owner.0 as isize;
                    s.text_edit = 0;
                    s.base = base;
                    s.dim = dim;
                    s.pill_was = pill_was;
                    let now = Instant::now();
                    s.started_at = Some(now);
                    s.last_input = Some(now);
                    s.ceiling_ms = snip_ceiling_ms(annotate);
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
                // 第一枪取两把表里更近的那一段：快速那条 60 秒就到顶，不该等到 180 秒才发现。
                let ceiling = SNIP.lock().map(|s| s.ceiling_ms).unwrap_or(0);
                let first = SNIP_TIMEOUT_MS.min(ceiling as u32);
                let _ = SetTimer(Some(ov), SNIP_TIMEOUT_TIMER, first, None);
                tracing_note(&format!("截图 snip-arm-timeout {SNIP_TIMEOUT_MS}"));
                tracing_note(&format!("截图 snip-arm-ceiling {ceiling} first {first}"));
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
                None,
            ) {
                Ok(_) => tracing_note("截图测试：已进剪贴板"),
                Err(e) => tracing_note(&format!("截图测试：剪贴板失败 {e}")),
            }
        }
        Err(e) => tracing_note(&format!("截图测试失败：{e}")),
    }
}

#[cfg(test)]
mod render_probe {
    use super::*;
    use windows::Win32::Graphics::Gdi::{BitBlt, CreateCompatibleBitmap, GetDIBits, SRCCOPY};
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };

    /// 把消息跑掉一段**有界**时间：测试不许因为一条没来的消息挂在那儿。
    unsafe fn pump(ms: u32) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms as u64);
        while std::time::Instant::now() < deadline {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// 投一条合成鼠标消息，同时把**真实光标**也停在同一个点上。
    ///
    /// 覆盖层铺满整块虚拟屏，机器上任何一次真实光标移动（别的程序、隔壁测试收尾时
    /// `SetCursorPos` 归位那一下）都会变成一条真 `WM_MOUSEMOVE` 打进这个线程的队列，
    /// 把刚设好的 hover 或刚起步的拖动增量改掉——探针就这样无规律跳红（实测连跑三次红三次，
    /// 单独跑则全绿）。让真光标始终跟着合成手势走，多出来的那条消息坐标相同，
    /// 对拖动来说是 `s.cur == (x,y)` 的幂等，对悬停来说是同一个候选，就不再改状态。
    unsafe fn post_mouse(hwnd: HWND, msg: u32, x: i32, y: i32) {
        let origin = SNIP.lock().map(|s| s.origin).unwrap_or((0, 0));
        let _ = SetCursorPos(origin.0 + x, origin.1 + y);
        let lp = (((y & 0xFFFF) as isize) << 16) | ((x & 0xFFFF) as isize);
        let _ = PostMessageW(Some(hwnd), msg, WPARAM(0), LPARAM(lp));
    }

    /// 窗口自己长什么样：从窗口 DC 直接 BitBlt 一块 32bpp 位图读回像素。
    ///
    /// 用 `PrintWindow` 反而拿不到：那条 API 在 windows-rs 0.61 挂在 `Storage::Xps` 下，
    /// 而覆盖层是普通 GDI 窗口（不是分层/合成窗口），窗口 DC 里就是它画上去的那些像素。
    unsafe fn print_window(hwnd: HWND) -> Option<(Vec<u8>, i32, i32)> {
        let mut rc = RECT::default();
        let _ = GetWindowRect(hwnd, &mut rc);
        let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
        if w <= 0 || h <= 0 {
            return None;
        }
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, bmp.into());
        let wd = GetDC(Some(hwnd));
        let ok = BitBlt(mem, 0, 0, w, h, Some(wd), 0, 0, SRCCOPY).is_ok();
        ReleaseDC(Some(hwnd), wd);
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
            return None;
        }
        Some((buf, w, h))
    }

    /// 客户区坐标 -> 截图里的偏移（截图含窗口边框；这个窗口是 WS_POPUP 无边框，
    /// 但 DWM 给的位置与客户区原点仍可能差一截 —— 所以偏移由**调用处实测**得到）。
    fn at(shot: &[u8], sw: i32, x: i32, y: i32) -> [u8; 3] {
        let i = ((y as usize) * sw as usize + x as usize) * 4;
        [shot[i], shot[i + 1], shot[i + 2]]
    }

    /// 点工具条上的一颗：落点仍由产品自己算（`bar_rects`），测试不抄第二把尺子。
    unsafe fn click_bar(ov: HWND, id: &str) {
        let (btns, _) = SNIP
            .lock()
            .map(|s| bar_rects(s.sel, s.base_w, s.base_h))
            .ok()
            .unwrap_or_default();
        let b = btns.iter().find(|x| x.id == id).expect("工具条缺按钮");
        post_mouse(ov, WM_MOUSEMOVE, b.x + b.w / 2, b.y + b.h / 2);
        post_mouse(ov, WM_LBUTTONDOWN, b.x + b.w / 2, b.y + b.h / 2);
        post_mouse(ov, WM_LBUTTONUP, b.x + b.w / 2, b.y + b.h / 2);
    }

    /// 某颗按钮此刻的**底色**：取左上角往里 3 像素那一点，避开标签的墨迹。
    /// 落点仍由产品自己算（`bar_rects`），测试不抄第二把尺子。
    unsafe fn btn_fill(shot: &[u8], sw: i32, id: &str) -> [u8; 3] {
        let (btns, _) = SNIP
            .lock()
            .map(|s| bar_rects(s.sel, s.base_w, s.base_h))
            .unwrap_or_default();
        let b = btns.iter().find(|x| x.id == id).expect("工具条缺按钮");
        pixel3(shot, sw, b.x + 3, b.y + 3)
    }

    /// 出图那句结果到底投递了没有：拿一个 1x1、开在屏幕外的顶层窗口当"拉起截图的那个窗口"，
    /// 数它收到几条 `WM_SNIP_NOTE`。owner 传 null（探针里那些独立会话）时产品按设计不出声，
    /// 所以这条只能这么判——光看函数返回值证不到"话送出去了"。
    static NOTE_HITS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    unsafe extern "system" fn note_sink_proc(
        hwnd: HWND,
        msg: u32,
        w: WPARAM,
        l: LPARAM,
    ) -> LRESULT {
        if msg == crate::WM_SNIP_NOTE {
            NOTE_HITS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, w, l)
    }

    unsafe fn new_note_sink() -> HWND {
        let hinst = GetModuleHandleW(None).map(|m| m.into()).unwrap_or_default();
        let cls = HSTRING::from("FocusFlowSnipNoteSink");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(note_sink_proc),
            hInstance: hinst,
            lpszClassName: PCWSTR(cls.as_ptr()),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);
        CreateWindowExW(
            Default::default(),
            &cls,
            &HSTRING::from("snip note sink"),
            WINDOW_STYLE(WS_POPUP.0),
            -32000,
            -32000,
            1,
            1,
            None,
            None,
            Some(hinst),
            None,
        )
        .unwrap_or(HWND(std::ptr::null_mut()))
    }

    /// 逐像素唯一的合成画面（BGRA）。随便往哪个方向错开一格，颜色就对不上。
    fn synth(w: i32, h: i32) -> Vec<u8> {
        let mut v = vec![0u8; (w.max(0) * h.max(0)) as usize * 4];
        for y in 0..h {
            for x in 0..w {
                let i = ((y as usize) * w as usize + x as usize) * 4;
                v[i] = ((x as u32 * 3 + y as u32 * 5) % 251) as u8;
                v[i + 1] = ((x as u32 * 7 + y as u32) % 249) as u8;
                v[i + 2] = ((x as u32 + y as u32 * 11) % 253) as u8;
                v[i + 3] = 255;
            }
        }
        v
    }

    fn pixel3(buf: &[u8], stride: i32, x: i32, y: i32) -> [u8; 3] {
        let i = ((y as usize) * stride as usize + x as usize) * 4;
        [buf[i], buf[i + 1], buf[i + 2]]
    }

    /// 合成结果里 (x,y)（选区局部坐标）是否还等于冻结缓冲的同一点。
    fn same_as_base(
        baked: &[u8],
        stride: i32,
        base: &[u8],
        base_w: i32,
        sel: (i32, i32, i32, i32),
        x: i32,
        y: i32,
    ) -> bool {
        pixel3(baked, stride, x, y) == pixel3(base, base_w, sel.0 + x, sel.1 + y)
    }

    /// 覆盖层**真的画出来了**吗：选区里的像素要等于手里那份冻结图，选区外要等于压暗的那份，
    /// 八个手柄与选区边框、工具条底色都要在它们该在的位置上。
    ///
    /// 为什么要这条：GUI 验收套件要起第二个进程，而那份进程抢不到单实例互斥体就会直接退 ——
    /// 用户自己的 app 在跑的时候，那是唯一还能看见屏上形状的办法。
    /// 它同时钉住"裁的位置对不对"这一族（旧账：覆盖层带 caption 时整图偏左上 8×1）。
    ///
    /// 会在全屏盖一层 topmost 窗口约一秒，所以默认不跑：
    /// `cargo test -p focusflow-native --bins -- --ignored the_overlay`
    #[test]
    #[ignore = "会真的在屏幕上盖一层全屏覆盖层"]
    fn the_overlay_paints_the_frozen_pixels_inside_and_the_veil_outside() {
        unsafe {
            let _serial = crate::probe_guard();
            // 这条探针会替用户搬光标（见 post_mouse 为什么要对齐真实坐标），收尾搬回去
            let mut home = POINT::default();
            let _ = GetCursorPos(&mut home);
            let owner = HWND(std::ptr::null_mut());
            start(owner, true);
            pump(400);
            let ov = SNIP
                .lock()
                .map(|s| HWND(s.overlay as *mut core::ffi::c_void))
                .expect("SNIP 取不到");
            assert_ne!(ov.0, std::ptr::null_mut(), "覆盖层没建出来");
            let (base_w, base_h) = SNIP.lock().map(|s| (s.base_w, s.base_h)).unwrap();
            assert!(
                base_w > 700 && base_h > 520,
                "这块屏容不下这条判据：{base_w}x{base_h}"
            );
            // 这条会话是标注版：绝对上限得真挂成 600 秒。`snip-arm-ceiling` 那句报的就是它，
            // 而验收套件超时那一轮正是按这个数字挑的 annotate 模式。
            assert_eq!(
                SNIP.lock().map(|s| s.ceiling_ms).unwrap_or(0),
                600_000,
                "标注会话的上限没按 600 秒挂"
            );
            // 拿真实桌面当对照会被"纯色区域"喂成假绿：那种地方像素本来就处处相同，
            // 裁错 8 格也照样相等（实测把 crop 整体偏移 +8,+1 时这条探针没反应）。
            // 所以先把冻结缓冲换成**逐像素唯一**的合成图案，任何偏移都会改变判据。
            let base = synth(base_w, base_h);
            let dim = dim_bgra(&base);
            if let Ok(mut s) = SNIP.lock() {
                s.base = base.clone();
                s.dim = dim.clone();
                // 那条提示腿要靠"背景是压暗的一份"才数得出白字：start() 已经按真实光标算出
                // 一个 hover（常常就是整块屏那层窗口），亮区盖住哪里，哪里就不再是压暗值。
                // 悬停那一条下面会自己把候选与 hover 重新设回来。
                s.hits.clear();
                s.hover = None;
            }
            let _ = InvalidateRect(Some(ov), None, false);
            pump(200);

            // 屏上那行用法提示（旧版有、重写时丢过一次）：Select 阶段左上角那条带里必须有白字。
            // 数像素而不是数颜色种类——压暗层本来就是一片接近黑的深色。
            let shot_hint = print_window(ov).expect("带提示的那一张没拿到");
            let mut hint_px = 0;
            for y in 14..34 {
                for x in 26..360 {
                    let p = pixel3(&shot_hint.0, shot_hint.1, x, y);
                    if p[0] > 170 && p[1] > 170 && p[2] > 170 {
                        hint_px += 1;
                    }
                }
            }
            // 对照腿：屏幕右上角同一高度那条带里必须一个白字都没有。
            // 没有这一头，"数到白像素"可能数的根本不是提示——实测 hover 没清干净时
            // 整块屏都走亮区，白像素 226 个照样"通过"，就是这条对照腿把它揭出来的。
            let mut control_px = 0;
            for y in 14..34 {
                for x in (shot_hint.1 - 340)..(shot_hint.1 - 40) {
                    let p = pixel3(&shot_hint.0, shot_hint.1, x, y);
                    if p[0] > 170 && p[1] > 170 && p[2] > 170 {
                        control_px += 1;
                    }
                }
            }
            assert_eq!(control_px, 0, "右上角那条带不该有白字：亮区没被选区夹住");
            assert!(
                hint_px > 20,
                "左上角那行用法提示没画出来（白像素 {hint_px}）"
            );

            // 悬停预览：塞一条"别人进程"的候选进去，鼠标移到它身上就该亮。
            // 走的是产品自己的 `hover_at`（不是直接把亮区写进状态），所以命中/换算/夹边都被判到。
            let origin = SNIP.lock().map(|s| s.origin).unwrap();
            let hot = (300, 250, 700, 450);
            if let Ok(mut s) = SNIP.lock() {
                s.hits = vec![Candidate {
                    pid: 999_999,
                    visible: true,
                    rect: (
                        origin.0 + hot.0,
                        origin.1 + hot.1,
                        origin.0 + hot.2,
                        origin.1 + hot.3,
                    ),
                }];
            }
            post_mouse(ov, WM_MOUSEMOVE, 500, 350);
            pump(200);
            let hov = SNIP.lock().map(|s| s.hover).unwrap();
            assert_eq!(hov, Some(hot), "悬停没命中那条候选");
            let shot0 = print_window(ov).expect("悬停态截图没拿到");
            assert_eq!(
                pixel3(&shot0.0, shot0.1, 500, 350),
                pixel3(&base, base_w, 500, 350),
                "悬停命中的那块没亮"
            );
            assert_eq!(
                pixel3(&shot0.0, shot0.1, 100, 100),
                pixel3(&dim, base_w, 100, 100),
                "悬停时选区外也该是压暗的那一份"
            );

            // 拖一个选区（客户区坐标 = 虚拟屏坐标 - 虚拟屏原点）
            post_mouse(ov, WM_LBUTTONDOWN, 240, 200);
            post_mouse(ov, WM_MOUSEMOVE, 640, 460);
            post_mouse(ov, WM_LBUTTONUP, 640, 460);
            pump(500);

            let (sel, phase) = SNIP.lock().map(|s| (s.sel, s.phase)).unwrap();
            assert_eq!(phase, Phase::Adjust, "松手后没进标注阶段");
            let shot = print_window(ov).expect("PrintWindow 没拿到像素");
            let (sh, sw, sh_h) = shot;
            assert!(sw > 0 && sh_h > 0);
            // 这个窗口是 WS_POPUP 无边框，PrintWindow 拿到的就是客户区本身 ⇒ 不需要偏移换算；
            // 真要出现偏移，下面那条 assert 的报错里会直接指出差多少（旧账是整图偏左上 8×1）。
            let inside = at(&sh, sw, (sel.0 + sel.2) / 2, (sel.1 + sel.3) / 2);
            let want_in = {
                let x = (sel.0 + sel.2) / 2;
                let y = (sel.1 + sel.3) / 2;
                let i = ((y as usize) * base_w as usize + x as usize) * 4;
                [base[i], base[i + 1], base[i + 2]]
            };
            assert_eq!(inside, want_in, "选区里画的不该是原图还是什么？");
            let outside = at(&sh, sw, 12, 12);
            let want_out = {
                let i = (12usize * base_w as usize + 12usize) * 4;
                [dim[i], dim[i + 1], dim[i + 2]]
            };
            assert_eq!(outside, want_out, "选区外画的是压暗后的那一份");
            // 手柄：八个中心都该是实心白
            let mut bright = 0;
            let mut seen = Vec::new();
            let mut whites = 0i32;
            let mut oranges = 0i32;
            for yy in (sel.1 - 30)..(sel.3 + 30) {
                for xx in (sel.0 - 30)..(sel.2 + 30) {
                    if xx < 0 || yy < 0 || xx >= sw || yy >= sh_h {
                        continue;
                    }
                    let p = at(&sh, sw, xx, yy);
                    if p == [255, 255, 255] {
                        whites += 1;
                    }
                    if (p[0] as i32 - 91).abs() <= 6
                        && (p[1] as i32 - 156).abs() <= 6
                        && (p[2] as i32 - 247).abs() <= 6
                    {
                        oranges += 1;
                    }
                }
            }
            for (hx, hy) in handle_points(sel) {
                let p = at(&sh, sw, hx, hy);
                if p.iter().all(|v| *v > 150) {
                    bright += 1;
                }
                seen.push(format!("({hx},{hy})={p:?}"));
            }
            assert_eq!(bright, 8, "八个手柄没全画出来（只看到 {bright} 个）: {seen:?} sel={sel:?} 整幅里白色={whites:?} 橙色={oranges:?}");
            // 四条框都要真画出来：右边与下边最容易整条丢掉（它们落在被裁掉的那一半上）
            let want_edge = [
                ((ACCENT >> 16) & 0xFF) as u8,
                ((ACCENT >> 8) & 0xFF) as u8,
                (ACCENT & 0xFF) as u8,
            ];
            for (x, y, label) in [
                (sel.0, sel.1 + 20, "左"),
                (sel.2 - 1, sel.1 + 20, "右"),
                (sel.0 + 20, sel.1, "上"),
                (sel.0 + 20, sel.3 - 1, "下"),
            ] {
                let p = at(&sh, sw, x, y);
                let near = (p[0] as i32 - want_edge[0] as i32).abs() <= 8
                    && (p[1] as i32 - want_edge[1] as i32).abs() <= 8
                    && (p[2] as i32 - want_edge[2] as i32).abs() <= 8;
                assert!(near, "选区的{label}边没画出来：{p:?} 应为 {want_edge:?}");
            }
            // 工具条：整条底色在它该在的地方（选区下方放不下就翻上方）
            let (btns, bar) = bar_rects(sel, base_w, SNIP.lock().map(|s| s.base_h).unwrap_or(0));
            let p = at(&sh, sw, bar.0 + 1, bar.1 + 1);
            let want = [
                ((BAR_BG >> 16) & 0xFF) as u8,
                ((BAR_BG >> 8) & 0xFF) as u8,
                (BAR_BG & 0xFF) as u8,
            ];
            assert_eq!(p, want, "工具条没画在 ({},{})", bar.0, bar.1);
            // 按钮本身有内容：中心不许还是条底色
            let b = &btns[0];
            let q = at(&sh, sw, b.x + b.w / 2, b.y + b.h / 2);
            assert_ne!(q, want, "按钮区还是工具条底色：一颗都没画");
            // 实时预览这一路（draw_live）也要看得见：按工具条上的"色块"，在选区里拖一笔，
            // 屏上那一块就必须是色块的颜色 —— 与落盘后的像素判据互补（一个证屏上，一个证文件）。
            let (btns2, _) = bar_rects(sel, base_w, SNIP.lock().map(|s| s.base_h).unwrap_or(0));
            let block = btns2
                .iter()
                .find(|b| b.id == "block")
                .expect("工具条没有色块按钮");
            post_mouse(
                ov,
                WM_MOUSEMOVE,
                block.x + block.w / 2,
                block.y + block.h / 2,
            );
            post_mouse(
                ov,
                WM_LBUTTONDOWN,
                block.x + block.w / 2,
                block.y + block.h / 2,
            );
            post_mouse(
                ov,
                WM_LBUTTONUP,
                block.x + block.w / 2,
                block.y + block.h / 2,
            );
            pump(200);
            post_mouse(ov, WM_LBUTTONDOWN, sel.0 + 20, sel.1 + 20);
            post_mouse(ov, WM_MOUSEMOVE, sel.0 + 120, sel.1 + 120);
            post_mouse(ov, WM_LBUTTONUP, sel.0 + 120, sel.1 + 120);
            pump(300);
            let shot2 = print_window(ov).expect("第二张截图没拿到");
            let (sh2, sw2, _) = shot2;
            let red = [
                ((PALETTE[0].0 >> 16) & 0xFF) as u8,
                ((PALETTE[0].0 >> 8) & 0xFF) as u8,
                (PALETTE[0].0 & 0xFF) as u8,
            ];
            for (x, y) in [(sel.0 + 60, sel.1 + 60), (sel.0 + 100, sel.1 + 100)] {
                let p = at(&sh2, sw2, x, y);
                assert_eq!(p, red, "拖出来的色块没实时显示在 ({x},{y})");
            }
            // 一笔都没落的地方仍然是原图：预览不许糊满整块选区
            let keep = at(&sh2, sw2, sel.0 + 300, sel.1 + 200);
            let i = (((sel.1 + 200) as usize) * base_w as usize + (sel.0 + 300) as usize) * 4;
            assert_eq!(
                keep,
                [base[i], base[i + 1], base[i + 2]],
                "色块外的像素被改了"
            );

            // 所见即所得的另一半：把这一刻的选区+标注合成、编码、落盘（写进**临时目录**，
            // 不碰真实 data/screenshots，也不碰用户剪贴板），再把 PNG 解回来对像素。
            // 这条盯的是"文件里那张 == 屏上那一批像素"：色块那块必须红，块外必须还是原图。
            let (baked, outw, outh) = baked_selection().expect("没有可出的图");
            assert_eq!(
                (outw, outh),
                (sel.2 - sel.0, sel.3 - sel.1),
                "出图尺寸不等于选区"
            );
            let png = png_bytes(&baked, outw, outh).expect("编码失败");
            let tmp = std::env::temp_dir().join(format!("ff_snip_probe_{}_1", std::process::id()));
            let _ = std::fs::remove_dir_all(&tmp);
            let path = write_png_named(&tmp, &png, outw, outh).expect("写盘失败");
            let nm = path.file_name().unwrap().to_string_lossy().to_string();
            assert!(
                nm.starts_with("snip_") && nm.contains(&format!("{outw}x{outh}")),
                "文件名要带宽高：{nm}"
            );
            let (rgba, dw, dh) = focusflow_core::capture::decode_png_rgba(&png).unwrap();
            assert_eq!((dw as i32, dh as i32), (outw, outh));
            let px = |x: i32, y: i32| {
                let i = ((y as usize) * dw as usize + x as usize) * 4;
                [rgba[i], rgba[i + 1], rgba[i + 2]]
            };
            assert_eq!(px(60, 60), [255, 45, 45], "色块那块没进文件");
            assert_eq!(px(100, 100), [255, 45, 45], "色块那块没进文件");
            // 块外（局部 300,200）必须等于冻结缓冲里同一屏幕坐标那一个像素：
            // 对不上就是"裁的位置不对"或"标注画到别处去了"。
            let gx = (sel.0 + 300) as usize;
            let gy = (sel.1 + 200) as usize;
            let i = (gy * base_w as usize + gx) * 4;
            assert_eq!(
                px(300, 200),
                [base[i + 2], base[i + 1], base[i]],
                "块外的像素被改了，或裁的坐标算错了一格"
            );
            let _ = std::fs::remove_dir_all(&tmp);

            // 八向手柄端到端：拖右下角 +40/+40，出图尺寸必须跟着长。
            post_mouse(ov, WM_MOUSEMOVE, sel.2, sel.3);
            post_mouse(ov, WM_LBUTTONDOWN, sel.2, sel.3);
            pump(150);
            post_mouse(ov, WM_MOUSEMOVE, sel.2 + 40, sel.3 + 40);
            post_mouse(ov, WM_LBUTTONUP, sel.2 + 40, sel.3 + 40);
            pump(300);
            let (grown, gw, gh) = baked_selection().expect("改完大小就没有图了");
            assert_eq!(
                (gw, gh),
                ((sel.2 - sel.0) + 40, (sel.3 - sel.1) + 40),
                "手柄没把选区改大：出图还是旧尺寸"
            );
            let _ = grown;

            // 马赛克：格内处处相等，且那个值等于**测试自己**从冻结缓冲按格算出的平均
            // （不复用产品的 mosaic_block，否则是自我循环论证）。
            click_bar(ov, "mosaic");
            post_mouse(ov, WM_LBUTTONDOWN, sel.0 + 150, sel.1 + 30);
            post_mouse(ov, WM_MOUSEMOVE, sel.0 + 250, sel.1 + 110);
            post_mouse(ov, WM_LBUTTONUP, sel.0 + 250, sel.1 + 110);
            pump(250);
            let (mos, mw, _) = baked_selection().expect("打了马赛克就没图了");
            let first = pixel3(&mos, mw, 150, 30);
            for (x, y) in [(159, 30), (150, 39), (159, 39)] {
                assert_eq!(pixel3(&mos, mw, x, y), first, "格子 ({x},{y}) 与格头不一致");
            }
            let mut acc = [0u64; 3];
            for dy in 0..MOSAIC_CELL {
                for dx in 0..MOSAIC_CELL {
                    let j = (((sel.1 + 30 + dy) as usize) * base_w as usize
                        + (sel.0 + 150 + dx) as usize)
                        * 4;
                    for k in 0..3 {
                        acc[k] += base[j + k] as u64;
                    }
                }
            }
            let n = (MOSAIC_CELL * MOSAIC_CELL) as u64;
            let want = [(acc[0] / n) as u8, (acc[1] / n) as u8, (acc[2] / n) as u8];
            assert_eq!(first, want, "马赛克取的不是那块原图的平均");

            // 撤销 / 重做也在合成结果的像素上判（不落盘、不碰剪贴板）
            click_bar(ov, "undo");
            pump(200);
            let (und, uw, _) = baked_selection().expect("撤销后没图了");
            assert!(
                same_as_base(&und, uw, &base, base_w, sel, 150, 30),
                "撤销没把那一笔拿掉"
            );
            click_bar(ov, "redo");
            pump(200);
            let (red, rw, _) = baked_selection().expect("重做后没图了");
            assert_eq!(pixel3(&red, rw, 150, 30), want, "重做没把那一笔放回来");

            // 文字那一笔也要真进合成：点"文字"、在选区里点一下（开原生 EDIT），把字塞进去
            // 再投"编辑完成"那条消息 —— 这条判的是文字工具落进了 marks 并被烤出像素。
            click_bar(ov, "text");
            post_mouse(ov, WM_LBUTTONDOWN, sel.0 + 200, sel.1 + 150);
            post_mouse(ov, WM_LBUTTONUP, sel.0 + 200, sel.1 + 150);
            pump(250);
            let ed = SNIP.lock().map(|s| s.text_edit).unwrap_or(0);
            assert_ne!(ed, 0, "文字工具没开出输入框");
            let edh = HWND(ed as *mut core::ffi::c_void);
            let txt: Vec<u16> = "测试".encode_utf16().chain(std::iter::once(0)).collect();
            let _ = SetWindowTextW(edh, PCWSTR(txt.as_ptr()));
            let _ = PostMessageW(Some(ov), WM_EDIT_DONE, WPARAM(1), LPARAM(ed));
            pump(250);
            let (tex, tw, _) = baked_selection().expect("打完字就没图了");
            let mut differs = false;
            for y in 0..24 {
                for x in 0..80 {
                    if !same_as_base(&tex, tw, &base, base_w, sel, 200 + x, 150 + y) {
                        differs = true;
                    }
                }
            }
            assert!(differs, "那一行文字没烤进图");

            // 方向键挪选区（旧 Webview 版没有这条）：整块搬 3 像素，尺寸不许变，
            // 而"局部同一点"必须开始对应另一个屏幕像素 —— 两头一起判才挡得住"没搬"和"搬错"。
            let before = SNIP.lock().map(|s| s.sel).unwrap();
            for _ in 0..3 {
                let _ = PostMessageW(Some(ov), WM_KEYDOWN, WPARAM(0x27), LPARAM(0));
                // VK_RIGHT
            }
            pump(250);
            let now = SNIP.lock().map(|s| s.sel).unwrap();
            assert_eq!(
                now,
                (before.0 + 3, before.1, before.2 + 3, before.3),
                "方向键没把选区挪 3 像素"
            );
            let (moved, mw2, mh2) = baked_selection().expect("挪完就没图了");
            assert_eq!(
                (mw2, mh2),
                (before.2 - before.0, before.3 - before.1),
                "搬选区不该改尺寸"
            );
            assert!(
                same_as_base(&moved, mw2, &base, base_w, now, 300, 200),
                "搬过之后局部同一点该对应新的屏幕像素"
            );
            assert!(
                !same_as_base(&moved, mw2, &base, base_w, before, 300, 200),
                "还在按旧位置裁就是错的"
            );

            // 选中并搬动某一笔（旧 Webview 版做不到）：切"选择"工具、压住色块拖 +30/+30。
            // 之后的判据都用**当前**选区换算：前面那两条腿改过大小也挪过位置。
            let cur = SNIP.lock().map(|s| s.sel).unwrap();
            click_bar(ov, "pick");
            let (px0, py0) = (cur.0 + 60, cur.1 + 60);
            post_mouse(ov, WM_MOUSEMOVE, px0, py0);
            post_mouse(ov, WM_LBUTTONDOWN, px0, py0);
            pump(120);
            post_mouse(ov, WM_MOUSEMOVE, px0 + 30, py0 + 30);
            post_mouse(ov, WM_LBUTTONUP, px0 + 30, py0 + 30);
            pump(250);
            let (mv, mw3, _) = baked_selection().expect("搬完就没图了");
            assert!(
                same_as_base(&mv, mw3, &base, base_w, cur, 25, 25),
                "搬走之后旧位置没让开"
            );
            assert_eq!(
                pixel3(&mv, mw3, 140, 140),
                [45, 45, 255],
                "新落点没染上色块"
            );

            // Delete 删掉选中的那一笔，撤销再把它退回来（撤销按快照，删与搬共用同一条路）
            let _ = PostMessageW(Some(ov), WM_KEYDOWN, WPARAM(0x2E), LPARAM(0)); // VK_DELETE
            pump(250);
            let (del, dw3, _) = baked_selection().expect("删完就没图了");
            assert!(
                same_as_base(&del, dw3, &base, base_w, cur, 140, 140),
                "Delete 没删掉选中的那一笔"
            );
            click_bar(ov, "undo");
            pump(250);
            let (re, rw3, _) = baked_selection().expect("撤销删除后没图了");
            assert_eq!(
                pixel3(&re, rw3, 140, 140),
                [45, 45, 255],
                "撤销没把删除退回去"
            );
            click_bar(ov, "undo");
            pump(250);
            let (re2, rw4, _) = baked_selection().expect("第二次撤销后没图了");
            assert!(
                same_as_base(&re2, rw4, &base, base_w, cur, 140, 140),
                "第二次撤销该把搬动也退掉"
            );
            assert_eq!(
                pixel3(&re2, rw4, 25, 25),
                [45, 45, 255],
                "搬动退掉后原位置该重新红起来"
            );

            // 选中之后还能改形状：再点住色块把它选中，然后拖它的右下角 +50/+50
            click_bar(ov, "pick");
            post_mouse(ov, WM_MOUSEMOVE, cur.0 + 60, cur.1 + 60);
            post_mouse(ov, WM_LBUTTONDOWN, cur.0 + 60, cur.1 + 60);
            pump(120);
            post_mouse(ov, WM_LBUTTONUP, cur.0 + 60, cur.1 + 60);
            pump(200);
            post_mouse(ov, WM_MOUSEMOVE, cur.0 + 120, cur.1 + 120);
            pump(120);
            let n_marks = SNIP.lock().map(|s| s.marks.len()).unwrap_or(0);
            post_mouse(ov, WM_LBUTTONDOWN, cur.0 + 120, cur.1 + 120);
            pump(120);
            post_mouse(ov, WM_MOUSEMOVE, cur.0 + 170, cur.1 + 170);
            post_mouse(ov, WM_LBUTTONUP, cur.0 + 170, cur.1 + 170);
            pump(250);
            // 改形不许变成"又画了一笔"：角没接住时这一拖会落下一个新的色块，
            // 上面那三条像素判据全都照样绿，只有笔数会露馅。
            let n_after = SNIP.lock().map(|s| s.marks.len()).unwrap_or(0);
            assert_eq!(n_after, n_marks, "拖角变成了新画一笔");
            let (rs, rsw, _) = baked_selection().expect("改形后就没图了");
            assert_eq!(
                pixel3(&rs, rsw, 140, 140),
                [45, 45, 255],
                "拖角没把这块扩进来"
            );
            // 左上角那块必须还红着：这一条把"改形"和"整块被搬走"分开——
            // 拖角若退化成了搬动，140,140 照样会红，只有这一头能露馅。
            assert_eq!(
                pixel3(&rs, rsw, 25, 25),
                [45, 45, 255],
                "拖角把色块搬走了，不是扩它"
            );
            assert!(
                same_as_base(&rs, rsw, &base, base_w, cur, 300, 200),
                "改形改到别的地方去了"
            );

            // 空点击不许吃掉一步撤销：真落一笔 → 原样点它一下松开 → 按一次撤销就该把那一笔退掉。
            // 旧写法按下就存快照，那一次空点击在栈里留下一层"和现在一模一样"，
            // 用户按撤销时屏幕什么都不变，要按第二次才见效。
            click_bar(ov, "block");
            post_mouse(ov, WM_LBUTTONDOWN, cur.0 + 300, cur.1 + 30);
            post_mouse(ov, WM_MOUSEMOVE, cur.0 + 340, cur.1 + 70);
            post_mouse(ov, WM_LBUTTONUP, cur.0 + 340, cur.1 + 70);
            pump(200);
            let n_drawn = SNIP.lock().map(|s| s.marks.len()).unwrap_or(0);
            assert!(n_drawn > 0, "那一笔没落下来，后面的判据就是空的");
            click_bar(ov, "pick");
            post_mouse(ov, WM_LBUTTONDOWN, cur.0 + 320, cur.1 + 50);
            post_mouse(ov, WM_LBUTTONUP, cur.0 + 320, cur.1 + 50);
            pump(200);
            click_bar(ov, "undo");
            pump(250);
            assert_eq!(
                SNIP.lock().map(|s| s.marks.len()).unwrap_or(0),
                n_drawn - 1,
                "一次撤销没退掉刚画的那一笔：空点击把撤销步吃掉了"
            );

            // 标注阶段的一记右键不该把已经画好的这一整堆丢掉（旧版在 annotating 时直接 return）
            let n_before_rc = SNIP.lock().map(|s| s.marks.len()).unwrap_or(0);
            post_mouse(ov, WM_RBUTTONDOWN, cur.0 + 40, cur.1 + 40);
            post_mouse(ov, WM_RBUTTONUP, cur.0 + 40, cur.1 + 40);
            pump(200);
            assert!(
                IsWindow(Some(ov)).as_bool(),
                "标注阶段一记右键就把覆盖层删了"
            );
            let n_after_rc = SNIP.lock().map(|s| s.marks.len()).unwrap_or(0);
            assert_eq!(n_after_rc, n_before_rc, "右键把已经画好的标注丢了");

            // 文字：打完之后焦点走开就该把这一行落下来（旧版 blur 提交；原来是直接销毁输入框，
            // 已经打进去的字一个字都不留）
            click_bar(ov, "text");
            post_mouse(ov, WM_LBUTTONDOWN, cur.0 + 60, cur.1 + 60);
            pump(200);
            let edit = SNIP.lock().map(|s| s.text_edit).unwrap_or(0);
            assert_ne!(edit, 0, "按文字工具没开出输入框");
            let phrase = HSTRING::from("失焦落的字");
            let _ = SetWindowTextW(HWND(edit as *mut core::ffi::c_void), &phrase);
            let _ = SetFocus(Some(ov));
            pump(250);
            let committed = SNIP
                .lock()
                .map(|s| {
                    s.marks
                        .iter()
                        .any(|m| matches!(&m.shape, Shape::Text(_, _, t) if t == "失焦落的字"))
                })
                .unwrap_or(false);
            assert!(committed, "输入框失焦没把那一行字落成一笔");
            let edit_after = SNIP.lock().map(|s| s.text_edit).unwrap_or(-1);
            assert_eq!(edit_after, 0, "落完字之后输入框还挂在状态里");

            // 空框的失焦不许把它毁掉（隔壁那颗行内编辑实测过"刚建好就收到一次假 KILLFOCUS"）：
            // 再点开一个不写字，把焦点挪走，框必须还在、也不许多出一笔。
            let n_before_empty = SNIP.lock().map(|s| s.marks.len()).unwrap_or(0);
            post_mouse(ov, WM_LBUTTONDOWN, cur.0 + 150, cur.1 + 150);
            pump(200);
            let empty_edit = SNIP.lock().map(|s| s.text_edit).unwrap_or(0);
            assert_ne!(empty_edit, 0, "第二个输入框没开出来");
            // 敲字也要续上闲置那把表：先把表拨回 100 秒前，投一个字符，表必须回到"刚刚"。
            // 这些键落在子窗口身上，覆盖层自己一条 WM_KEYDOWN 都收不到 ⇒ 漏掉这一路的话，
            // 认真打一行标题超过 180 秒的人会被连框带字一起掐掉。
            if let Ok(mut s) = SNIP.lock() {
                s.last_input = Some(Instant::now() - std::time::Duration::from_secs(100));
            }
            let _ = PostMessageW(
                Some(HWND(empty_edit as *mut core::ffi::c_void)),
                WM_CHAR,
                WPARAM(0x41),
                LPARAM(0),
            );
            pump(200);
            let aged = SNIP
                .lock()
                .map(|s| s.last_input.map(|t| t.elapsed().as_secs()).unwrap_or(9_999))
                .unwrap_or(9_999);
            assert!(aged < 60, "打字没续上闲置那把表（表上还记着 {aged} 秒）");
            // 把刚投进去那个字符抹掉：下半段判的还得是"空框"
            let blank = [0u16; 1];
            let _ = SetWindowTextW(
                HWND(empty_edit as *mut core::ffi::c_void),
                PCWSTR(blank.as_ptr()),
            );
            pump(120);
            let _ = SetFocus(Some(ov));
            pump(250);
            assert!(
                SNIP.lock().map(|s| s.text_edit).unwrap_or(0) != 0,
                "空输入框被一次失焦毁掉了（症状＝文字工具点了没框）"
            );
            assert_eq!(
                SNIP.lock().map(|s| s.marks.len()).unwrap_or(0),
                n_before_empty,
                "空框失焦却多落了一笔"
            );
            // 收尾：Esc 把这一个空框丢掉，别把框留到后面的判据里
            let esc = SNIP.lock().map(|s| s.text_edit).unwrap_or(0);
            let _ = PostMessageW(
                Some(HWND(esc as *mut core::ffi::c_void)),
                WM_KEYDOWN,
                WPARAM(0x1B),
                LPARAM(0),
            );
            pump(200);
            assert_eq!(
                SNIP.lock().map(|s| s.text_edit).unwrap_or(-1),
                0,
                "Esc 没丢掉那个空框"
            );

            // 出图没成的那句必须**真画在屏上**：覆盖层为它不退，只在日志里出声等于没出声。
            // 这条是"病状腿 + 对照腿"一对——把原因抹掉之后那一片必须不红，
            // 否则上面那条数到的红可能是橙框/红字标注顶的。
            if let Ok(mut s) = SNIP.lock() {
                s.notice = Some("没存盘：夹具造的那条原因".to_string());
            }
            let _ = InvalidateRect(Some(ov), None, false);
            pump(220);
            let sel_now = SNIP.lock().map(|s| s.sel).unwrap();
            let band = |shot: &[u8], sw: i32| -> i32 {
                let mut n = 0;
                for y in (sel_now.1 - 46)..(sel_now.1 - 27) {
                    for x in (sel_now.0 + 4)..(sel_now.0 + 140) {
                        let p = pixel3(shot, sw, x, y);
                        if p[2] > 150 && p[0] < 110 && p[1] < 110 {
                            n += 1;
                        }
                    }
                }
                n
            };
            let shot_n = print_window(ov).expect("带着原因的屏没拿到");
            let reds = band(&shot_n.0, shot_n.1);
            assert!(reds > 20, "那句原因没画到屏上（红色像素只有 {reds}）");
            if let Ok(mut s) = SNIP.lock() {
                s.notice = None;
            }
            let _ = InvalidateRect(Some(ov), None, false);
            pump(220);
            let shot_c = print_window(ov).expect("对照那一张没拿到");
            assert_eq!(
                band(&shot_c.0, shot_c.1),
                0,
                "没有原因那一片却还红着：上面的判据不是这句字画出来的"
            );

            // 出图的"去向"能注进来之后，这一对才是本轮新行为真正的端到端证据：
            // 写盘失败 ⇒ 覆盖层不退并且把原因留下；写盘成功 ⇒ 落一张 PNG 并且自己退。
            // 目录指着真实路径会往用户 `data/screenshots` 写、还会盖掉他的剪贴板，
            // 所以这里走临时目录且 `clipboard = false`。
            let ctmp = std::env::temp_dir().join(format!("ff_snip_commit_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&ctmp);
            std::fs::create_dir_all(&ctmp).expect("临时目录建不出来，后面两条判据就是空的");
            let (bw, bh) = SNIP
                .lock()
                .map(|s| (s.sel.2 - s.sel.0, s.sel.3 - s.sel.1))
                .unwrap();
            let n_before = SNIP.lock().map(|s| s.marks.len()).unwrap_or(0);
            // 拿一个**普通文件**占住"目录"那个位置 ⇒ `create_dir_all` 必失败 ⇒ 这一趟落盘必失败
            let blocked = ctmp.join("不是一个目录");
            std::fs::write(&blocked, b"x").expect("占位文件写不出去");
            commit_and_close_into(ov, false, &blocked, false);
            pump(250);
            assert!(
                IsWindow(Some(ov)).as_bool(),
                "写盘失败却把覆盖层关了：那张图连一次重试的机会都没有"
            );
            let why = SNIP.lock().map(|s| s.notice.clone()).unwrap_or(None);
            let why = why.unwrap_or_default();
            assert!(why.contains("没存盘"), "屏上那句没说到点上：{why:?}");
            assert_eq!(
                SNIP.lock().map(|s| s.marks.len()).unwrap_or(0),
                n_before,
                "出图失败还把已经画好的标注吞了"
            );

            // 换成能写的目录 ⇒ 同一份会话必须落一张、文件名带宽高、尺寸对得上。
            // 这里先不关窗：关窗会把整屏缓冲交出去，后面"同秒连按"那条判据就读不到同一份会话了。
            let out = ctmp.join("out");
            std::fs::create_dir_all(&out).expect("可写目录建不出来");
            let first = commit_into(&out, false, false);
            assert!(matches!(first, Verdict::Done), "第一张没成：{first:?}");
            let files: Vec<String> = std::fs::read_dir(&out)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            assert_eq!(files.len(), 1, "临时目录里该正好落一张：{files:?}");
            let nm = &files[0];
            assert!(
                nm.starts_with("snip_") && nm.contains(&format!("{bw}x{bh}")),
                "文件名没带上选区宽高：{nm}"
            );
            let raw = std::fs::read(out.join(nm)).expect("那张 PNG 读不回来");
            let (rgba, dw, dh) = focusflow_core::capture::decode_png_rgba(&raw).unwrap();
            assert_eq!((dw as i32, dh as i32), (bw, bh), "落盘那张的尺寸不等于选区");
            // 走产品自己这条出路落的那张里，已经画好的标注也得在：
            // 整幅中至少有一个"色块那种红"的像素（PNG 解回来是 RGBA 顺序）
            assert!(
                rgba.as_chunks::<4>()
                    .0
                    .iter()
                    .any(|p| p[0] == 255 && p[1] == 45 && p[2] == 45),
                "落盘那张里没有已画的标注"
            );

            // 同秒连按不许互相覆盖：他自己日志里就有两次"截图已保存"写着同一个文件名，
            // 目录里只剩一张 —— 旧命名只到秒，那一张是真丢了。
            let again = commit_into(&out, false, false);
            assert!(matches!(again, Verdict::Done), "第二张没成：{again:?}");
            let files2: Vec<String> = std::fs::read_dir(&out)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            assert_eq!(files2.len(), 2, "同秒两张该是两个文件：{files2:?}");
            // 判"两个名字互不相同"而不是硬要 `_2` 后缀：跨到下一秒时两个名字本来就不一样，
            // 那也算对——这条要防的只是"名字撞在一起把前一张盖掉"。
            assert_ne!(files2[0], files2[1], "两张撞了同一个名字：{files2:?}");
            assert!(
                files2.iter().all(|n| n.starts_with("snip_")),
                "落盘的名字不都带 snip_ 前缀：{files2:?}"
            );

            // 贴图那一条（P / 工具条"贴图"）是出图的第二个去向，以前一次都没被判过：
            // 钉上来的位置必须**逐边等于选区在屏幕上的那一块**、尺寸等于选区。
            // 这一族错过（覆盖层带 caption 时整图偏左上 8×1），所以判矩形而不是判"有没有窗口"。
            let (org2, sel2) = SNIP
                .lock()
                .map(|s| (s.origin, s.sel))
                .unwrap_or(((0, 0), (0, 0, 0, 0)));
            // 前提腿：桌面上原本一张贴图都没有，后面 `FindWindowW` 找到的才是这一笔钉出来的。
            // 没有这条，别处（比如另一份实例）留着的贴图会把下面的矩形判据顶得莫名其妙。
            assert!(
                FindWindowW(w!("FocusFlowPin"), PCWSTR::null()).is_err(),
                "前提不成立：桌面上已经有一张贴图，这条要在干净桌面上跑"
            );
            let pinned = commit_into(&out, false, true);
            assert!(
                matches!(pinned, Verdict::Done),
                "贴图这条没成交：{pinned:?}"
            );
            pump(250);
            let pin = FindWindowW(w!("FocusFlowPin"), PCWSTR::null()).unwrap_or_default();
            assert!(!pin.0.is_null(), "贴图窗口没建出来");
            let mut prc = RECT::default();
            let _ = GetWindowRect(pin, &mut prc);
            assert_eq!(
                (prc.left, prc.top),
                (org2.0 + sel2.0, org2.1 + sel2.1),
                "贴图没钉在选区那一块上（屏幕坐标）"
            );
            assert_eq!(
                (prc.right - prc.left, prc.bottom - prc.top),
                (bw, bh),
                "贴图的尺寸不等于选区"
            );
            let _ = DestroyWindow(pin);
            pump(150);
            assert!(
                FindWindowW(w!("FocusFlowPin"), PCWSTR::null()).is_err(),
                "贴图没收尾干净"
            );

            // 最后才判"成则关窗"：上面那几次提交都故意留着窗口，为的是同一份会话能连出多张
            commit_and_close_into(ov, false, &out, false);
            pump(250);
            assert!(!IsWindow(Some(ov)).as_bool(), "出图成功了覆盖层却没退");

            let _ = std::fs::remove_dir_all(&ctmp);

            // "随手一按 = 截光标下那个窗口"这条手势到底接上没有（纯函数改了 wndproc 不会跟着红）。
            // 起一个全新的标注会话（上一段刚把覆盖层出图关掉），塞一条离工具条很远的候选，
            // 在它身上按一下就松手：选区必须换成那条候选、必须进标注那一格、覆盖层必须还在。
            start(HWND(std::ptr::null_mut()), true);
            pump(400);
            let ov3 = SNIP
                .lock()
                .map(|s| HWND(s.overlay as *mut core::ffi::c_void))
                .unwrap_or(HWND(std::ptr::null_mut()));
            assert!(IsWindow(Some(ov3)).as_bool(), "随手一按那段没建出覆盖层");
            let org3 = SNIP.lock().map(|s| s.origin).unwrap_or((0, 0));
            let far = (650, 250, 900, 400);
            if let Ok(mut s) = SNIP.lock() {
                s.hits = vec![Candidate {
                    pid: 999_998,
                    visible: true,
                    rect: (
                        org3.0 + far.0,
                        org3.1 + far.1,
                        org3.0 + far.2,
                        org3.1 + far.3,
                    ),
                }];
                s.hover = None;
            }
            pump(120);
            post_mouse(ov3, WM_LBUTTONDOWN, 700, 300);
            post_mouse(ov3, WM_LBUTTONUP, 700, 300);
            pump(250);
            let got = SNIP
                .lock()
                .map(|s| (s.sel, s.phase, IsWindow(Some(ov3)).as_bool()))
                .unwrap_or(((0, 0, 0, 0), Phase::Select, false));
            assert_eq!(got.0, far, "随手一按没把那个窗口收成选区");
            assert_eq!(got.1, Phase::Adjust, "点选之后没进标注那一格");
            assert!(got.2, "标注版点选完却把覆盖层关了");

            // 贴底那一格：选区下沿离屏幕底放不下整条工具条时，工具条必须翻到选区**上方**。
            // `bar_placement` 有纯函数判据，但"翻上去之后画在哪、画没画"在屏上一次没证过；
            // 而这条腿先把"选区真的贴到底"当前提量出来，否则翻不翻都无从谈起。
            let bh3 = SNIP.lock().map(|s| s.base_h).unwrap_or(0);
            post_mouse(ov3, WM_LBUTTONDOWN, 200, bh3 - 60);
            post_mouse(ov3, WM_MOUSEMOVE, 620, bh3 - 14);
            post_mouse(ov3, WM_LBUTTONUP, 620, bh3 - 14);
            pump(250);
            let (sel3, bar3) = SNIP
                .lock()
                .map(|s| (s.sel, bar_rects(s.sel, s.base_w, s.base_h).1))
                .unwrap_or(((0, 0, 0, 0), (0, 0, 0, 0)));
            assert!(
                sel3.3 > bh3 - 120 && sel3.3 <= bh3,
                "前提不成立：选区没贴到屏幕底 sel={sel3:?} base_h={bh3}"
            );
            assert!(
                bar3.1 + bar3.3 <= sel3.1 + 1,
                "选区贴底时工具条没翻到上方：bar={bar3:?} sel={sel3:?}"
            );
            let shot3 = print_window(ov3).expect("贴底那一格没拍到");
            let want3 = [
                ((BAR_BG >> 16) & 0xFF) as u8,
                ((BAR_BG >> 8) & 0xFF) as u8,
                (BAR_BG & 0xFF) as u8,
            ];
            assert_eq!(
                pixel3(&shot3.0, shot3.1, bar3.0 + 1, bar3.1 + 1),
                want3,
                "翻上去的工具条没画出来：bar={bar3:?}"
            );

            // 工具按钮的高亮必须跟着当前工具走——"我现在拿的是哪支笔"这件事得看得见。
            // 比的是两支点来点去之后两颗粒子**互换**，静态的一片橙色喂不饱这条判据。
            click_bar(ov3, "rect");
            pump(200);
            let s1 = print_window(ov3).expect("rect 高亮那张没拿到");
            let rect1 = btn_fill(&s1.0, s1.1, "rect");
            let block1 = btn_fill(&s1.0, s1.1, "block");
            click_bar(ov3, "block");
            pump(200);
            let s2 = print_window(ov3).expect("block 高亮那张没拿到");
            let rect2 = btn_fill(&s2.0, s2.1, "rect");
            let block2 = btn_fill(&s2.0, s2.1, "block");
            fn is_accent(p: [u8; 3]) -> bool {
                p[2] > 200 && p[1] > 120 && p[0] < 140
            }
            assert!(
                is_accent(rect1) && !is_accent(block1),
                "选着方框时高亮不在方框那颗上：{rect1:?} {block1:?}"
            );
            assert!(
                is_accent(block2) && !is_accent(rect2),
                "换到色块之后高亮没跟过去：{rect2:?} {block2:?}"
            );

            // Esc 是标注阶段唯一的退路（右键被刻意让开了）：必须真把这一场收掉，
            // 而且屏上那句红字、标注、整屏缓冲都得跟着归零——留着下一次热键会接着看见旧东西。
            let _ = PostMessageW(Some(ov3), WM_KEYDOWN, WPARAM(0x1B), LPARAM(0));
            pump(250);
            assert!(!IsWindow(Some(ov3)).as_bool(), "Esc 没关掉标注阶段的覆盖层");
            let cleared = SNIP
                .lock()
                .map(|s| (s.active, s.marks.len(), s.base.len(), s.notice.is_some()))
                .unwrap_or((true, 1, 1, true));
            assert_eq!(
                cleared,
                (false, 0, 0, false),
                "Esc 之后状态没清干净（在飞标志／标注／整屏缓冲／那句红字都得归零）"
            );
            let _ = DestroyWindow(ov3);
            pump(200);

            // 快速那条（松手即存）被失败留下来之后，这一场实际就是标注场：绝对上限得跟着变成 10 分钟。
            // 不跟着改的话，它自己那 60 秒那一档会在用户还在读那句原因、还在腾磁盘时把覆盖层收走。
            // 这一段全程不投"松手"——松手会走产品那条真实去向（往用户目录写一张、盖掉他的剪贴板），
            // 只把选区拖出来，然后直接调注入版出图。
            // owner 给一个真实的（1x1、开在屏幕外的）顶层窗口：这一段的会话不是 null owner，
            // 于是"成/败那句话该投递给拉起截图的窗口"这条也顺带被数到了（见 NOTE_HITS）。
            let sink = new_note_sink();
            let hits0 = NOTE_HITS.load(std::sync::atomic::Ordering::SeqCst);
            start(sink, false);
            pump(400);
            let ov2 = SNIP
                .lock()
                .map(|s| HWND(s.overlay as *mut core::ffi::c_void))
                .unwrap_or(HWND(std::ptr::null_mut()));
            assert!(IsWindow(Some(ov2)).as_bool(), "快速那条会话没建出覆盖层");
            assert_eq!(
                SNIP.lock().map(|s| s.ceiling_ms).unwrap_or(0),
                60_000,
                "快速会话一开始该顶着 60 秒那档"
            );
            post_mouse(ov2, WM_LBUTTONDOWN, 40, 40);
            post_mouse(ov2, WM_MOUSEMOVE, 240, 200);
            pump(150);
            let ctmp2 = std::env::temp_dir().join(format!("ff_snip_quick_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&ctmp2);
            std::fs::create_dir_all(&ctmp2).expect("第二个临时目录建不出来");
            let blocked2 = ctmp2.join("不是一个目录");
            std::fs::write(&blocked2, b"x").expect("占位文件写不出去");
            commit_and_close_into(ov2, false, &blocked2, false);
            pump(250);
            // 没成那句也得递出去（覆盖层上写一份、主窗口的状态读数再留一份影子）
            assert_eq!(
                NOTE_HITS.load(std::sync::atomic::Ordering::SeqCst),
                hits0 + 1,
                "出图失败那句话没投递给拉起截图的窗口"
            );
            let after = SNIP
                .lock()
                .map(|s| (s.phase, s.ceiling_ms, IsWindow(Some(ov2)).as_bool()))
                .unwrap_or((Phase::Select, 0, false));
            assert!(after.2, "快速那条失败也没留下覆盖层");
            assert_eq!(after.0, Phase::Adjust, "失败之后没进可以重试的那一格");
            assert_eq!(
                after.1, 600_000,
                "留下来的会话还顶着快速那 60 秒的上限：读句子的时候会被收走"
            );
            // 成了那句要一样递出去：这一张写进临时目录（不动用户目录，也不碰剪贴板）
            let ok2 = ctmp2.join("out2");
            std::fs::create_dir_all(&ok2).expect("第三个临时目录建不出来");
            let v2 = commit_into(&ok2, false, false);
            assert!(matches!(v2, Verdict::Done), "补的这张没成：{v2:?}");
            pump(250);
            assert_eq!(
                NOTE_HITS.load(std::sync::atomic::Ordering::SeqCst),
                hits0 + 2,
                "出图成功那句话没投递给拉起截图的窗口"
            );
            let _ = DestroyWindow(ov2);
            pump(200);
            let _ = DestroyWindow(sink);
            let _ = std::fs::remove_dir_all(&ctmp2);

            let _ = DestroyWindow(ov);
            pump(200);
            // 会话结束必须把整屏那两份缓冲交出去：1920x1080 一份就是 8 MB，
            // 挂在静态里等于每次截图之后常驻翻倍（这一族仓里记过两次账）。
            let (still_base, still_dim, still_active) = SNIP
                .lock()
                .map(|s| (s.base.len(), s.dim.len(), s.active))
                .unwrap_or((0, 0, true));
            assert_eq!(
                (still_base, still_dim, still_active),
                (0, 0, false),
                "覆盖层关了却没把冻结缓冲放掉"
            );
            let _ = SetCursorPos(home.x, home.y);
        }
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

    fn mk(shape: Shape, w: i32) -> Mark {
        Mark {
            shape,
            style: Style {
                rgb: rgb(255, 45, 45),
                w,
                font: 30,
            },
        }
    }

    #[test]
    fn a_mark_is_grabbable_where_a_user_would_aim() {
        // 实心块：块内算、块外不算，右边界是开区间（与画出来的像素一致）
        let block = mk(Shape::Block(20, 20, 100, 80), 3);
        assert!(mark_hit(&block, 60, 60));
        assert!(!mark_hit(&block, 120, 60), "右边界不该算块内");
        assert!(mark_hit(&block, 119, 99));
        // 空心框：整框包围盒都认（挑那条边太难），粗笔再放宽容差
        let rect = mk(Shape::Rect(0, 0, 50, 50), 7);
        assert!(mark_hit(&rect, 3, 25));
        assert!(mark_hit(&rect, 25, 25), "框内也认：这样才搬得动");
        // 箭头与画笔：按走过点的包围盒
        let arrow = mk(Shape::Arrow(10, 10, 90, 50), 3);
        assert!(mark_hit(&arrow, 12, 12));
        assert!(!mark_hit(&arrow, 200, 200));
        let pen = mk(Shape::Pen(vec![(5, 5), (50, 40)]), 3);
        assert!(mark_hit(&pen, 5, 5) && mark_hit(&pen, 50, 40) && !mark_hit(&pen, 300, 300));
        assert!(
            !mark_hit(&mk(Shape::Pen(vec![]), 3), 0, 0),
            "空折线不该被选中"
        );
        // 文字：空串也要占一个字号宽，否则永远选不中
        assert!(mark_hit(&mk(Shape::Text(10, 10, String::new()), 3), 20, 20));
        let txt = mk(Shape::Text(10, 10, "四个字".to_string()), 3);
        assert_eq!(mark_bbox(&txt), Some((10, 10, 100, 40)));
        assert!(!mark_hit(&txt, 200, 20));
    }

    #[test]
    fn the_topmost_mark_wins_and_move_shifts_every_point() {
        let marks = vec![
            mk(Shape::Block(0, 0, 100, 100), 3),
            mk(Shape::Block(20, 20, 40, 40), 3),
        ];
        // 后画的盖在上面 ⇒ 压着小块时该选中后者；小块之外仍归大块
        assert_eq!(topmost_hit(&marks, 30, 30), Some(1));
        assert_eq!(topmost_hit(&marks, 5, 5), Some(0));
        assert_eq!(topmost_hit(&marks, 500, 500), None);

        let mut pen = mk(Shape::Pen(vec![(1, 2), (3, 4)]), 3);
        move_mark(&mut pen, 10, -2);
        assert_eq!(mark_bbox(&pen), Some((11, 0, 13, 2)));
        let mut txt = mk(Shape::Text(5, 5, "x".to_string()), 3);
        move_mark(&mut txt, 2, 3);
        assert_eq!(mark_bbox(&txt), Some((7, 8, 37, 38)));
    }

    #[test]
    fn a_selected_mark_can_be_reshaped_from_its_four_corners() {
        let mut m = mk(Shape::Block(20, 20, 100, 100), 3);
        assert_eq!(
            endpoint_points(&m),
            Some([(20, 20), (120, 20), (120, 120), (20, 120)])
        );
        assert_eq!(hit_endpoint(&m, 120, 120), Some(2));
        assert_eq!(hit_endpoint(&m, 21, 21), Some(0));
        assert_eq!(hit_endpoint(&m, 70, 70), None, "块中间不是角：那里是搬动");
        // 右下角拖出去
        resize_mark(&mut m, 2, 160, 140);
        assert_eq!(mark_bbox(&m), Some((20, 20, 160, 140)));
        // 左上角拖过右下角 ⇒ 折回来：宽高不许变负（负宽高就是裁出 0 像素那一类错）
        resize_mark(&mut m, 0, 300, 300);
        assert_eq!(mark_bbox(&m), Some((160, 140, 300, 300)));
        // 箭头：左半改起点、右半改终点
        let mut a = mk(Shape::Arrow(10, 10, 90, 90), 3);
        resize_mark(&mut a, 0, 5, 6);
        resize_mark(&mut a, 2, 7, 8);
        assert_eq!(mark_bbox(&a), Some((5, 6, 7, 8)));
        // 画笔改首尾点（中间的点不动：拖角改折线本来就该是两头）
        let mut p = mk(Shape::Pen(vec![(1, 1), (5, 5), (9, 9)]), 3);
        resize_mark(&mut p, 0, 0, 3);
        resize_mark(&mut p, 3, 12, 12);
        assert_eq!(mark_bbox(&p), Some((0, 3, 12, 12)));
        // 文字没有角：它跟字号走
        assert_eq!(
            endpoint_points(&mk(Shape::Text(1, 1, "字".to_string()), 3)),
            None
        );
    }

    #[test]
    fn the_overlay_waits_for_the_nearer_of_the_two_clocks() {
        // 两条都还剩：等更近的那一段。只按闲置算的话，"绝对上限"那条腿会一直看不见，
        // 手一直放着鼠标动的人就能把覆盖层挂下去。
        assert_eq!(snip_tick(179_000, 599_000), SnipTick::Wait(179_000));
        assert_eq!(snip_tick(120_000, 60_000), SnipTick::Wait(60_000));
        // 闲置到点但绝对上限还没到：退（这才是"没人碰它"那一条）
        assert_eq!(snip_tick(0, 400_000), SnipTick::Idle);
        // 人一直在动（闲置还剩很多）但绝对上限到点：照样退，而且要按 Absolute 报，
        // 日志里那句"三分钟没动"在这种情况下就是假话。
        assert_eq!(snip_tick(170_000, 0), SnipTick::Absolute);
        // 两条同时到点：按绝对算，别把"挂到顶了"念成"没动"
        assert_eq!(snip_tick(-1, -1), SnipTick::Absolute);
        // 负数但绝对还早 ⇒ 闲置到点
        assert_eq!(snip_tick(-5, 30_000), SnipTick::Idle);
    }

    #[test]
    fn the_two_clocks_keep_their_documented_relationship() {
        // 与旧 Webview 版同一对数：标注最多 10 分钟、松手即存最多 60 秒
        assert_eq!(snip_ceiling_ms(true), 600_000);
        assert_eq!(snip_ceiling_ms(false), 60_000);
        // 标注的上限必须比闲置闸大：否则"动一下续表"根本没有意义，绝对闸先落下就等于没有续表
        assert!(snip_ceiling_ms(true) > SNIP_TIMEOUT_MS as u64);
        // 快速那条必须比闲置小 ⇒ 第一枪就该是它（start() 挂的是两者里更近的那段）
        assert!(snip_ceiling_ms(false) < SNIP_TIMEOUT_MS as u64);
    }

    #[test]
    fn a_failed_save_keeps_the_overlay_and_says_why() {
        let disk = Err("磁盘满了".to_string());
        let clip = Err("打开剪贴板失败".to_string());
        assert_eq!(commit_verdict(Ok(()), Ok(())), Verdict::Done);
        // 没存上 ⇒ 覆盖层必须留着，而且原因不许被话术吞掉（原来这里成败都关窗）
        match commit_verdict(disk, Ok(())) {
            Verdict::Stay(m) => {
                assert!(m.contains("磁盘满了"), "原因没写进屏上那句：{m}");
                assert!(m.contains("Enter"), "要告诉他怎么再试：{m}");
            }
            Verdict::Done => panic!("没存盘却判定可以关窗：这张图就真没了"),
        }
        // 两件都没成 ⇒ 说的是"没存盘"那一半，它才决定这张图到底留下没有
        match commit_verdict(Err("磁盘满了".to_string()), clip) {
            Verdict::Stay(m) => {
                assert!(m.contains("没存盘") && m.contains("磁盘满了"), "{m}");
            }
            Verdict::Done => panic!("两个都没成还判 Done"),
        }
        // 只丢剪贴板 ⇒ 文件已在，话要说成"已存盘但…"，否则他会以为整张没截上而重按
        match commit_verdict(Ok(()), Err("打开剪贴板失败".to_string())) {
            Verdict::Stay(m) => {
                assert!(m.contains("已存盘") && m.contains("打开剪贴板失败"), "{m}");
            }
            Verdict::Done => panic!("剪贴板没成却一声不吭"),
        }
    }

    #[test]
    fn a_release_decides_between_commit_window_pick_and_cancel() {
        // 两边都刚好 `MIN_DRAG`：算框住了
        assert_eq!(
            release_outcome((0, 0, MIN_DRAG, MIN_DRAG), None),
            Release::Commit
        );
        // 差一个像素就不算：这时"随手一按"要走点选那一条，而不是取消
        assert_eq!(
            release_outcome((0, 0, MIN_DRAG - 1, 900), None),
            Release::Cancel
        );
        assert_eq!(
            release_outcome((0, 0, 900, MIN_DRAG - 1), None),
            Release::Cancel
        );
        // 没拖够但压着一个窗口 ⇒ 截那个窗口（旧版同一手势语义）
        assert_eq!(
            release_outcome((5, 5, 6, 6), Some((10, 10, 200, 150))),
            Release::PickWindow((10, 10, 200, 150))
        );
        // 拖够了就不再看点：出图优先。否则人拖完框手一抖压到别的窗口，选区会被换掉
        assert_eq!(
            release_outcome((0, 0, 400, 300), Some((10, 10, 200, 150))),
            Release::Commit
        );
    }

    #[test]
    fn the_receipt_names_the_file_the_size_and_every_destination() {
        let plain = snip_receipt("snip_20261028_120000_300x200.png", 300, 200, false);
        assert!(
            plain.contains("snip_20261028_120000_300x200.png"),
            "文件名得原样在里面：{plain}"
        );
        assert!(plain.contains("300x200"), "尺寸得说出来：{plain}");
        assert!(plain.contains("剪贴板"), "进了剪贴板这件事要说：{plain}");
        assert!(!plain.contains("钉"), "没钉就别声称钉了：{plain}");
        let pin = snip_receipt("snip_a.png", 10, 20, true);
        assert!(pin.contains("钉在桌面"), "贴图那条要说全：{pin}");
        assert!(pin.contains("剪贴板"), "贴图这条其实也上了剪贴板：{pin}");
        assert_ne!(plain, pin, "两句一模一样，等于没说");
    }

    #[test]
    fn an_empty_click_on_a_mark_does_not_consume_an_undo_step() {
        let mut s = default_state();
        s.marks.push(mk(Shape::Block(20, 20, 40, 40), 3));
        // 按下"搬这一笔"：此刻还什么都不该存
        s.undo_pending = true;
        s.selected = Some(0);
        assert!(s.undo_stack.is_empty(), "按下就把撤销栈推了一层");
        // 原样松开
        s.undo_pending = false;
        s.drag = None;
        assert!(s.undo_stack.is_empty(), "一次空点击吃掉了一步撤销");
        // 再按一次并真拖动：第一次改动才存，之后同一次拖动不再存
        s.undo_pending = true;
        push_undo_if_pending(&mut s);
        assert_eq!(s.undo_stack.len(), 1, "真改动了却没存快照");
        push_undo_if_pending(&mut s);
        push_undo_if_pending(&mut s);
        assert_eq!(
            s.undo_stack.len(),
            1,
            "一次拖动该是一步撤销，不是每像素一步"
        );
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
        // Delete = 删选中的那一笔（没选中时 on_key 自己不动作）
        assert_eq!(key_action(0x2E, false, false, true), KeyAct::DeleteSelected);
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
