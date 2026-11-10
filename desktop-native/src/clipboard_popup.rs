//! 剪贴板历史弹窗（历史 / 片段 / 统计；文本、图片与文件条目都能复制）。
//!
//! 形态：`WS_POPUP` + `WS_EX_TOOLWINDOW | WS_EX_TOPMOST`，**不设 owner**。设了 owner 之后
//! 主窗藏进托盘会把弹窗一起藏掉；而主窗可能是隐藏的，热键仍应能把历史叫出来。
//!
//! 焦点：打开时记住 `GetForegroundWindow`，Enter/Esc 关闭时还回去；被别的窗口点走
//! （`WM_ACTIVATE` 失活）只隐藏、不抢回焦点。全局热键唤起时 `SetForegroundWindow`
//! 可能被前台锁拒绝，用 `AttachThreadInput` 兜一次。
//!
//! 搜索框必须是弹窗的 `WS_CHILD` EDIT，不能复用主窗那套 `create_hosted_edit`：
//! 那个 host 是顶层窗，`place_hosted_edit` 会 `SetForegroundWindow(host)`，弹窗会立刻
//! 收到失活而自动关闭；`edit_proc` 还会把 `WM_EDIT_DONE` 递给主窗。

use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect,
    FrameRect, GetMonitorInfoW, InvalidateRect, MonitorFromPoint, SelectObject, SetBkColor,
    SetBkMode, SetTextColor, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
    DEFAULT_PITCH, DRAW_TEXT_FORMAT, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE,
    DT_VCENTER, FONT_WEIGHT, FW_NORMAL, FW_SEMIBOLD, HBRUSH, HDC, HFONT, HGDIOBJ, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, OUT_DEFAULT_PRECIS, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetFocus, GetKeyState, SendInput, SetFocus, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
    KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END,
    VK_ESCAPE, VK_F1, VK_HOME, VK_NEXT, VK_PRIOR, VK_RETURN, VK_TAB, VK_UP, VK_V,
};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    GetClientRect, GetCursorPos, GetForegroundWindow, GetParent, GetWindowLongPtrW,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow,
    IsWindowVisible, KillTimer, MessageBoxW, RegisterClassW, SendMessageW, SetForegroundWindow,
    SetTimer, SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow, ShowWindowAsync,
    TrackPopupMenu, CS_DBLCLKS, ES_AUTOHSCROLL, GWLP_USERDATA, HMENU, HWND_TOPMOST, IDYES,
    MB_ICONQUESTION, MB_YESNO, MESSAGEBOX_STYLE, MF_POPUP, MF_SEPARATOR, MF_STRING, SWP_SHOWWINDOW,
    SW_HIDE, SW_RESTORE, SW_SHOW, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_TOPALIGN,
    TRACK_POPUP_MENU_FLAGS, WA_INACTIVE, WINDOW_STYLE, WM_ACTIVATE, WM_CLOSE, WM_COMMAND,
    WM_CTLCOLOREDIT, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN,
    WM_MOUSEWHEEL, WM_PAINT, WM_RBUTTONDOWN, WM_SETFONT, WM_SIZE, WM_SYSKEYDOWN, WM_TIMER,
    WNDCLASSW, WS_CHILD, WS_CLIPCHILDREN, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};

use focusflow_core::clipboard::{self, ClipItem, ClipKind, ClipStats, Snippet, Transform};

use crate::clip_list::{self, ReloadIntent, Viewport};

const EDIT_ID: usize = 1;
/// `EM_SETCUEBANNER`：EDIT 空的时候显示提示字。
const EM_SETCUEBANNER: u32 = 0x1501;
const EN_CHANGE: u16 = 0x0300;

const MENU_COPY: u32 = 1;
const MENU_PIN: u32 = 2;
const MENU_DELETE: u32 = 3;
const MENU_SNIPPET: u32 = 4;
const MENU_STATS: u32 = 5;
const MENU_CLEAR: u32 = 6;
const MENU_PAUSE_15: u32 = 7;
const MENU_PAUSE_60: u32 = 8;
const MENU_RESUME: u32 = 9;
const MENU_TRANSFORM_BASE: u32 = 100;
/// 抢前台的重试定时器（只给弹窗自己用，和主窗的 100ms 心跳不冲突）。
const ACTIVATE_TIMER: usize = 0x0C1A;
/// 关闭弹窗后"还焦点（并按需粘贴）"的重试定时器。
const RESTORE_TIMER: usize = 0x0C1B;
const RESTORE_TICK_MS: u32 = 25;
/// 12 跳 ≈ 300ms：足够前台锁松开或最小化窗口还原，又不会在用户已经走开时一直抢。
const RESTORE_MAX_TICKS: u8 = 12;
/// 连续这么多跳前台都是第三方窗口就放弃 —— 用户刚点了别处，不再把焦点硬拽回去。
const RESTORE_MAX_STRAY: u8 = 4;
/// 历史列表一次加载多少条。库默认上限也是 500；再往上只该靠搜索，不该靠滚动。
const HISTORY_LIMIT: i64 = 500;

/// 最近一次"没能把焦点还回去 / 没能粘贴"的原因。弹窗已经关了，写日志没人看；
/// 下次打开时放到状态条。
static LAST_RESTORE_ISSUE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

static PALETTE_HWND: AtomicIsize = AtomicIsize::new(0);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    History,
    Snippets,
    Stats,
}

#[derive(Clone)]
struct Row {
    id: i64,
    /// 历史条目是内容；片段是片段内容。
    content: String,
    /// 片段名；历史条目为空。
    name: String,
    source: String,
    time: String,
    pinned: bool,
    kind: ClipKind,
    image_path: Option<String>,
    file_paths: Vec<String>,
    /// 文件条目里已经不在磁盘上的路径数。`None` = 还没查（只对可视行查，避免
    /// 500 行 × 每条最多 20 个路径 × 每次击键上万次磁盘 stat）。
    missing_files: Option<usize>,
}

/// 关闭弹窗后的还焦点任务：每一跳都确认目标还活着、前台真的换过去了，再决定粘不粘。
#[derive(Clone, Copy)]
struct RestoreJob {
    target: HWND,
    paste: bool,
    ticks_left: u8,
    stray_ticks: u8,
    /// 还能"硬抢"几次前台。正常粘贴第一跳必须抢一次；之后用户如果已经点去别处，
    /// 再每跳硬拽就是把焦点从人手里抢回来。
    claim_left: u8,
}

struct Palette {
    bg: COLORREF,
    card: COLORREF,
    text: COLORREF,
    muted: COLORREF,
    accent: COLORREF,
    line: COLORREF,
    selected: COLORREF,
}

impl Palette {
    fn for_theme(dark: bool) -> Self {
        fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
            COLORREF(r as u32 | ((g as u32) << 8) | ((b as u32) << 16))
        }
        if dark {
            Self {
                bg: rgb(0x1B, 0x1F, 0x2A),
                card: rgb(0x23, 0x28, 0x38),
                text: rgb(0xE8, 0xEC, 0xF4),
                muted: rgb(0x9A, 0xA4, 0xB8),
                accent: rgb(0x5B, 0x9C, 0xF7),
                line: rgb(0x2E, 0x35, 0x48),
                selected: rgb(0x2A, 0x3B, 0x55),
            }
        } else {
            Self {
                bg: rgb(0xF5, 0xF7, 0xFB),
                card: rgb(0xFF, 0xFF, 0xFF),
                text: rgb(0x1B, 0x1F, 0x2A),
                muted: rgb(0x66, 0x70, 0x85),
                accent: rgb(0x25, 0x63, 0xEB),
                line: rgb(0xD6, 0xDC, 0xE6),
                selected: rgb(0xE3, 0xEC, 0xFC),
            }
        }
    }
}

struct State {
    edit: HWND,
    prev_fg: HWND,
    mode: Mode,
    query: String,
    history: Vec<Row>,
    snippets: Vec<Row>,
    stats: Option<ClipStats>,
    sel: usize,
    scroll: usize,
    status: Option<String>,
    /// 右键菜单开着时不要把 `WM_ACTIVATE` 失活当成"用户点走了" —— 菜单自己的弹开会
    /// 让弹窗短暂失去激活，不挡一下会在菜单出现的同时把窗藏掉。
    menu_open: bool,
    /// 确认框（`MessageBoxW`）开着时同理：它也是本窗自己的模态子窗，
    /// 失活那一下不是用户点走了。不挡的话弹窗会连同确认框一起消失，
    /// 删除在看不见的地方继续，用户只看到"按 Del 后程序卡住"。
    modal_open: bool,
    /// 这一轮显示里有没有真正拿到过激活。没有的话，刚 `ShowWindow` 时那条失活消息
    /// 不是"用户点走了"，不能据此关窗（否则热键按下去只会闪一个隐藏窗）。
    activated: bool,
    /// 抢前台的重试还剩几次。
    activate_tries: u8,
    /// 关闭弹窗后的还焦点/粘贴任务。
    restore: Option<RestoreJob>,
    dpi: f32,
    font: HFONT,
    font_small: HFONT,
    /// 搜索框的底色刷子：EDIT 自己画背景，父窗在 `WM_CTLCOLOREDIT` 里把这把刷子交给它。
    edit_bg: HBRUSH,
    pal: Palette,
}

impl Drop for State {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(self.font.0));
            let _ = DeleteObject(HGDIOBJ(self.font_small.0));
            let _ = DeleteObject(HGDIOBJ(self.edit_bg.0));
        }
    }
}

impl State {
    fn rows(&self) -> &[Row] {
        match self.mode {
            Mode::History => &self.history,
            Mode::Snippets => &self.snippets,
            Mode::Stats => &[],
        }
    }

    fn selected(&self) -> Option<&Row> {
        self.rows().get(self.sel)
    }

    fn clamp_selection(&mut self) {
        let len = self.rows().len();
        if len == 0 {
            self.sel = 0;
            self.scroll = 0;
        } else {
            self.sel = self.sel.min(len - 1);
            self.scroll = self.scroll.min(self.sel);
        }
    }

    fn reload_history(&mut self, hwnd: HWND, intent: ReloadIntent) {
        let visible = self.visible_rows(hwnd);
        let prev_id = self.history.get(self.sel).map(|r| r.id);
        let prev = Viewport {
            sel: self.sel,
            scroll: self.scroll,
        };
        let was_at_top = self.scroll == 0;
        let old = std::mem::take(&mut self.history);
        match load_history(&self.query) {
            Ok(mut rows) => {
                inherit_missing(&old, &mut rows);
                self.history = rows;
                self.status = None;
            }
            Err(e) => {
                self.history.clear();
                self.status = Some(e);
            }
        }
        if self.mode == Mode::History {
            let ids: Vec<i64> = self.history.iter().map(|r| r.id).collect();
            let vp = clip_list::settle(&ids, prev_id, prev, was_at_top, visible, intent);
            self.sel = vp.sel;
            self.scroll = vp.scroll;
        }
    }

    fn reload_all(&mut self, hwnd: HWND, intent: ReloadIntent) {
        self.reload_history(hwnd, intent);
        let visible = self.visible_rows(hwnd);
        let prev_id = self.snippets.get(self.sel).map(|r| r.id);
        let prev = Viewport {
            sel: self.sel,
            scroll: self.scroll,
        };
        let was_at_top = self.scroll == 0;
        let old = std::mem::take(&mut self.snippets);
        match clipboard::snippets() {
            Ok(items) => {
                let mut rows: Vec<Row> = items.into_iter().map(row_from_snippet).collect();
                inherit_missing(&old, &mut rows);
                self.snippets = rows;
            }
            Err(e) => {
                if self.status.is_none() {
                    self.status = Some(format!("读片段失败：{e}"));
                }
            }
        }
        if self.mode == Mode::Snippets {
            let ids: Vec<i64> = self.snippets.iter().map(|r| r.id).collect();
            let vp = clip_list::settle(&ids, prev_id, prev, was_at_top, visible, intent);
            self.sel = vp.sel;
            self.scroll = vp.scroll;
        }
        self.stats = clipboard::stats(7).ok();
        self.clamp_selection();
    }

    fn move_selection(&mut self, hwnd: HWND, delta: i32) {
        let len = self.rows().len();
        if len == 0 {
            self.sel = 0;
            self.scroll = 0;
            return;
        }
        let next = (self.sel as i32 + delta).clamp(0, len as i32 - 1) as usize;
        self.sel = next;
        let (w, h) = client_size(hwnd);
        let lay = layout_for(w, h, self.dpi);
        if self.sel < self.scroll {
            self.scroll = self.sel;
        } else if self.sel >= self.scroll + lay.rows {
            self.scroll = self.sel + 1 - lay.rows;
        }
    }

    fn move_selection_to(&mut self, hwnd: HWND, idx: usize) {
        let len = self.rows().len();
        if len == 0 {
            self.sel = 0;
            self.scroll = 0;
            return;
        }
        let idx = idx.min(len - 1);
        let delta = idx as i32 - self.sel as i32;
        self.move_selection(hwnd, delta);
    }

    /// 当前布局一次能画多少行（翻页与滚轮夹取都按它算）。
    fn visible_rows(&self, hwnd: HWND) -> usize {
        let (w, h) = client_size(hwnd);
        layout_for(w, h, self.dpi).rows
    }

    /// 只补**可视行**的失效检查：`Path::exists` 是磁盘 IO，不能跟着每一条历史走。
    fn fill_missing_visible(&mut self, first: usize, visible: usize) {
        let rows = match self.mode {
            Mode::History => &mut self.history,
            Mode::Snippets => &mut self.snippets,
            Mode::Stats => return,
        };
        for row in rows.iter_mut().skip(first).take(visible) {
            if row.kind == ClipKind::File && row.missing_files.is_none() {
                row.missing_files = Some(clipboard::split_alive_paths(&row.file_paths).1.len());
            }
        }
    }

    fn copy_selected(&mut self) -> bool {
        let Some(row) = self.selected().cloned() else {
            return false;
        };
        // 文本/图片/文件三条路都在 core：图片写回会顺带报出实际交了哪几种格式。
        match clipboard::copy_parts(
            row.kind,
            &row.content,
            row.image_path.as_deref(),
            &row.file_paths,
        ) {
            Ok(_) => {
                if self.mode == Mode::Snippets {
                    let _ = clipboard::touch_snippet(row.id);
                }
                true
            }
            Err(e) => {
                self.status = Some(format!("复制失败：{e}"));
                crate::tracing_note(&format!("剪贴板弹窗复制到剪贴板失败：{e}"));
                false
            }
        }
    }

    fn toggle_pin(&mut self, hwnd: HWND) {
        let Some(row) = self.selected().cloned() else {
            return;
        };
        match clipboard::set_pinned(row.id, !row.pinned) {
            Ok(true) => {
                self.reload_all(hwnd, ReloadIntent::Keep);
                self.status = Some(if row.pinned {
                    "已取消固定".to_string()
                } else {
                    "已固定（裁剪时不会删）".to_string()
                });
            }
            Ok(false) => self.status = Some("这一条已经不在了".to_string()),
            Err(e) => self.status = Some(format!("更新固定状态失败：{e}")),
        }
    }

    fn delete_selected(&mut self, hwnd: HWND) {
        let Some(row) = self.selected().cloned() else {
            return;
        };
        let result = match self.mode {
            Mode::Snippets => clipboard::delete_snippet(row.id),
            _ => clipboard::delete(row.id),
        };
        match result {
            Ok(true) => {
                self.reload_all(hwnd, ReloadIntent::Keep);
                self.status = Some("已删除".to_string());
            }
            Ok(false) => self.status = Some("这一条已经不在了".to_string()),
            Err(e) => self.status = Some(format!("删除失败：{e}")),
        }
    }

    fn save_snippet(&mut self, hwnd: HWND) {
        let Some(row) = self.selected().cloned() else {
            return;
        };
        if row.kind != ClipKind::Text {
            self.status = Some("图片/文件条目不能存为片段".to_string());
            return;
        }
        match clipboard::add_snippet("", &row.content) {
            Ok(_) => {
                self.reload_all(hwnd, ReloadIntent::Keep);
                self.status = Some("已存为片段（Tab 切到片段列表）".to_string());
            }
            Err(e) => self.status = Some(format!("存为片段失败：{e}")),
        }
    }

    fn apply_transform(&mut self, t: Transform) -> bool {
        let Some(row) = self.selected().cloned() else {
            return false;
        };
        if row.kind != ClipKind::Text {
            self.status = Some("只有文本条目能加工".to_string());
            return false;
        }
        match t.apply(&row.content) {
            Ok(text) => match clipboard::write_text(&text) {
                Ok(()) => true,
                Err(e) => {
                    self.status = Some(format!("加工后写入剪贴板失败：{e}"));
                    false
                }
            },
            Err(e) => {
                self.status = Some(e);
                false
            }
        }
    }
}

fn row_from_item(item: ClipItem) -> Row {
    Row {
        id: item.id,
        content: item.content,
        name: String::new(),
        source: if item.source_app.is_empty() {
            "未知来源".to_string()
        } else {
            item.source_app
        },
        time: short_time(item.copied_at),
        pinned: item.pinned,
        kind: item.kind,
        image_path: item.image_path,
        file_paths: item.file_paths,
        // 失效与否只对可视行现查（见 `fill_missing_visible`）。
        missing_files: None,
    }
}

/// 历史列表的唯一读入口。抽出来是为了让"一次加载 500 条"能被单测看见
/// （旧实现把 100 写死在这里）。
fn load_history(query: &str) -> Result<Vec<Row>, String> {
    clipboard::search(query, HISTORY_LIMIT)
        .map(|items| items.into_iter().map(row_from_item).collect())
        .map_err(|e| format!("读剪贴板库失败：{e}"))
}

fn row_from_snippet(s: Snippet) -> Row {
    Row {
        id: s.id,
        content: s.content,
        name: if s.name.trim().is_empty() {
            "(未命名片段)".to_string()
        } else {
            s.name
        },
        source: String::new(),
        time: short_time(s.created_at),
        pinned: false,
        kind: ClipKind::Text,
        image_path: None,
        file_paths: Vec::new(),
        missing_files: None,
    }
}

/// 重建 Row 时按 id 继承上一轮的失效检查结果。新复制每来一条都会重查整份列表，
/// 不继承的话可视的文件行每次都要重新 stat 一遍 —— 那正是"每复制一下就卡"的来源。
fn inherit_missing(old: &[Row], rows: &mut [Row]) {
    let mut known: std::collections::HashMap<i64, Option<usize>> =
        old.iter().map(|r| (r.id, r.missing_files)).collect();
    for row in rows.iter_mut() {
        if let Some(v) = known.remove(&row.id) {
            row.missing_files = v;
        }
    }
}

/// 行内文案：图片/文件在预览前加一个类型词；文件失效时把失效数放在**行首**，
/// 否则窄弹窗里右端的省略号会把它吃掉（实测 560 px 宽就看不见了）。
fn row_label(row: &Row) -> String {
    let text = preview_text(&row.content);
    match row.kind {
        ClipKind::Text => text,
        ClipKind::Image => format!("［图］{text}"),
        ClipKind::File if row.missing_files.unwrap_or(0) > 0 => {
            format!("［件·{} 个失效］{text}", row.missing_files.unwrap_or(0))
        }
        ClipKind::File => format!("［件］{text}"),
    }
}

fn short_time(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.format("%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// 行里只画一行：换行折成空格，超长交给 `DT_END_ELLIPSIS`。纯函数，便于单测。
fn preview_text(content: &str) -> String {
    content.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Debug, Clone, Copy)]
struct Layout {
    pad: i32,
    search: RECT,
    list_top: i32,
    row_h: i32,
    rows: usize,
    footer_top: i32,
}

fn layout_for(w: i32, h: i32, dpi: f32) -> Layout {
    let s = |v: f32| (v * dpi / 96.0).round().max(1.0) as i32;
    let pad = s(16.0);
    let search = RECT {
        left: pad,
        top: s(46.0),
        right: (w - pad).max(pad + s(80.0)),
        bottom: s(80.0),
    };
    let list_top = s(92.0);
    let footer_top = (h - s(34.0)).max(list_top);
    let row_h = s(36.0);
    let rows = (((footer_top - list_top) / row_h).max(1)) as usize;
    Layout {
        pad,
        search,
        list_top,
        row_h,
        rows,
        footer_top,
    }
}

fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut r = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut r);
    }
    ((r.right - r.left).max(1), (r.bottom - r.top).max(1))
}

unsafe fn state_ptr(hwnd: HWND) -> Option<*mut State> {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
    if p.is_null() {
        None
    } else {
        Some(p)
    }
}

/// 取弹窗自己的状态。调用方必须传本模块创建的弹窗句柄（`PALETTE_HWND`）；
/// 拿别的窗口来问只会得到 `None`，不会解引用到别处。
fn with_state<R>(hwnd: HWND, f: impl FnOnce(&mut State) -> R) -> Option<R> {
    unsafe { state_ptr(hwnd) }.map(|p| f(unsafe { &mut *p }))
}

fn dark_theme() -> bool {
    !focusflow_core::config::instance()
        .get_or("gui", "theme", "dark")
        .eq_ignore_ascii_case("light")
}

fn create_font(size_pt: f32, dpi: f32, weight: FONT_WEIGHT) -> HFONT {
    unsafe {
        CreateFontW(
            -((size_pt * dpi / 96.0).round() as i32),
            0,
            0,
            0,
            weight.0 as i32,
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

fn register_class() -> Result<(), String> {
    static CLASS: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    CLASS
        .get_or_init(|| {
            let hinstance =
                unsafe { GetModuleHandleW(None) }.map_err(|e| format!("取模块句柄失败：{e}"))?;
            let class = WNDCLASSW {
                style: CS_DBLCLKS,
                lpfnWndProc: Some(popup_proc),
                hInstance: hinstance.into(),
                lpszClassName: w!("FocusFlowClipboardPalette"),
                ..Default::default()
            };
            if unsafe { RegisterClassW(&class) } == 0 {
                return Err("注册剪贴板弹窗类失败".to_string());
            }
            Ok(())
        })
        .clone()
}

/// 光标所在显示器的工作区里居中；尺寸按系统 DPI 缩放。
fn placement() -> (i32, i32, i32, i32, f32) {
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    let monitor = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let work = if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        info.rcWork
    } else {
        RECT {
            left: 0,
            top: 0,
            right: 1280,
            bottom: 800,
        }
    };
    let dpi = {
        let d = unsafe { GetDpiForSystem() } as f32;
        if d <= 0.0 {
            96.0
        } else {
            d
        }
    };
    let w = ((560.0 * dpi / 96.0).round() as i32).min(work.right - work.left);
    // 560 高在 96/120dpi 下都是 12 行（旧 440 只有 8 行）；工作区太矮时仍按工作区截断。
    let h = ((560.0 * dpi / 96.0).round() as i32).min(work.bottom - work.top);
    let x = work.left + ((work.right - work.left - w) / 2).max(0);
    let y = work.top + ((work.bottom - work.top - h) / 2).max(0);
    (x, y, w, h, dpi)
}

pub fn is_open() -> bool {
    let hwnd = PALETTE_HWND.load(Ordering::Acquire);
    if hwnd == 0 {
        return false;
    }
    unsafe { IsWindowVisible(HWND(hwnd as *mut core::ffi::c_void)).as_bool() }
}

pub fn toggle(_main_hwnd: HWND) {
    if is_open() {
        // 热键再按一次是"收起来"：与 Esc 一样要把前台还给打开前那个窗口。
        // 走 `hide()` 的 false 分支会把焦点留在被藏起来的弹窗上，系统随后
        // 随便挑一个窗口激活——用户下一次按键就打到别的应用里去了。
        let raw = PALETTE_HWND.load(Ordering::Acquire);
        if raw != 0 {
            hide_inner(HWND(raw as *mut core::ffi::c_void), true);
        }
    } else {
        show();
    }
}

pub fn show() {
    let existing = PALETTE_HWND.load(Ordering::Acquire);
    if existing != 0 {
        let hwnd = HWND(existing as *mut core::ffi::c_void);
        if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            reveal(hwnd);
            return;
        }
        PALETTE_HWND.store(0, Ordering::Release);
    }
    if let Err(e) = create_and_show() {
        crate::tracing_note(&format!("剪贴板弹窗打不开：{e}"));
    }
}

fn create_and_show() -> Result<(), String> {
    register_class()?;
    let (x, y, w, h, dpi) = placement();
    let hinstance =
        unsafe { GetModuleHandleW(None) }.map_err(|e| format!("取模块句柄失败：{e}"))?;
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            w!("FocusFlowClipboardPalette"),
            w!("FocusFlow 剪贴板"),
            WS_POPUP | WS_CLIPCHILDREN,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(hinstance.into()),
            None,
        )
    }
    .map_err(|e| format!("创建剪贴板弹窗失败：{e}"))?;

    let lay = layout_for(w, h, dpi);
    let edit = unsafe {
        CreateWindowExW(
            Default::default(),
            w!("EDIT"),
            w!(""),
            WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | ES_AUTOHSCROLL as u32),
            lay.search.left + 8,
            lay.search.top + 5,
            (lay.search.right - lay.search.left - 16).max(20),
            (lay.search.bottom - lay.search.top - 10).max(16),
            Some(hwnd),
            Some(HMENU(EDIT_ID as *mut core::ffi::c_void)),
            Some(hinstance.into()),
            None,
        )
    }
    .map_err(|e| {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        format!("创建搜索框失败：{e}")
    })?;

    let font = create_font(15.0, dpi, FW_SEMIBOLD);
    let font_small = create_font(12.0, dpi, FW_NORMAL);
    let pal = Palette::for_theme(dark_theme());
    let edit_bg = unsafe { CreateSolidBrush(pal.card) };
    let state = Box::new(State {
        edit,
        prev_fg: unsafe { GetForegroundWindow() },
        mode: Mode::History,
        query: String::new(),
        history: Vec::new(),
        snippets: Vec::new(),
        stats: None,
        sel: 0,
        scroll: 0,
        status: None,
        menu_open: false,
        modal_open: false,
        activated: false,
        activate_tries: 0,
        restore: None,
        dpi,
        font,
        font_small,
        edit_bg,
        pal,
    });
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);
        SendMessageW(
            edit,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
        let cue: Vec<u16> = "搜索：空格分词，可加 img:/file:/app:\0"
            .encode_utf16()
            .collect();
        SendMessageW(
            edit,
            EM_SETCUEBANNER,
            Some(WPARAM(1)),
            Some(LPARAM(cue.as_ptr() as isize)),
        );
        let _ = SetWindowSubclass(edit, Some(edit_proc), 1, 0);
    }
    PALETTE_HWND.store(hwnd.0 as isize, Ordering::Release);

    with_state(hwnd, |st| {
        st.prev_fg = unsafe { GetForegroundWindow() };
        st.activated = false;
        st.activate_tries = 2;
        st.reload_all(hwnd, ReloadIntent::ResetTop);
        if let Some(msg) = take_restore_issue() {
            st.status = Some(msg);
        }
    });
    unsafe {
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_SHOWWINDOW);
        claim_foreground(hwnd);
        let _ = SetFocus(Some(edit));
        let _ = SetTimer(Some(hwnd), ACTIVATE_TIMER, 80, None);
        let focus = GetFocus();
        crate::tracing_note(&format!(
            "剪贴板弹窗焦点 edit={} focus={}",
            edit.0 as isize, focus.0 as isize
        ));
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
    crate::tracing_note(&format!("剪贴板弹窗已创建 hwnd={}", hwnd.0 as isize));
    Ok(())
}

fn reveal(hwnd: HWND) {
    // 上一次复制后的还焦点/粘贴任务不能让它在这次重开后突然发一发 Ctrl+V。
    cancel_restore(hwnd);
    let (x, y, w, h, _) = placement();
    // 先记焦点再显示：`SWP_SHOWWINDOW` 之后 `GetForegroundWindow` 可能已经是弹窗自己。
    let prev_fg = unsafe { GetForegroundWindow() };
    unsafe {
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_SHOWWINDOW);
    }
    if let Some(edit) = with_state(hwnd, |st| {
        st.mode = Mode::History;
        st.sel = 0;
        st.scroll = 0;
        st.query.clear();
        st.prev_fg = prev_fg;
        st.activated = false;
        st.activate_tries = 2;
        st.reload_all(hwnd, ReloadIntent::ResetTop);
        if let Some(msg) = take_restore_issue() {
            st.status = Some(msg);
        }
        unsafe {
            let _ = SetWindowTextW(st.edit, w!(""));
        }
        st.edit
    }) {
        update_edit_visibility(hwnd);
        unsafe {
            claim_foreground(hwnd);
            let _ = SetFocus(Some(edit));
            let _ = SetTimer(Some(hwnd), ACTIVATE_TIMER, 80, None);
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
}

fn claim_foreground(hwnd: HWND) {
    unsafe {
        if SetForegroundWindow(hwnd).as_bool() {
            return;
        }
        let fg = GetForegroundWindow();
        if fg.0.is_null() || fg == hwnd {
            return;
        }
        let fg_thread = GetWindowThreadProcessId(fg, None);
        let me = GetCurrentThreadId();
        if fg_thread != 0 && fg_thread != me && AttachThreadInput(fg_thread, me, true).as_bool() {
            let _ = SetForegroundWindow(hwnd);
            let _ = AttachThreadInput(fg_thread, me, false);
        }
    }
}

/// 复制成功后的收尾：关窗，然后由 `RESTORE_TIMER` 一跳一跳地把焦点还给原窗口，
/// 确认前台真的换过去了才注入 Ctrl+V。
///
/// 失败不回滚、也不重开弹窗：内容已经在剪贴板里，退路就是让用户自己按一下 Ctrl+V；
/// 失败原因写日志，并在下次打开弹窗时放到状态条（弹窗已经关了，当场没人看得到）。
fn finish_copy(hwnd: HWND, copied: bool) {
    if !copied {
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        return;
    }
    let auto = clipboard::auto_paste_enabled();
    let prev = with_state(hwnd, |st| st.prev_fg).unwrap_or_default();
    let has_target = !prev.0.is_null() && unsafe { IsWindow(Some(prev)) }.as_bool();
    let is_self = has_target && clipboard::window_belongs_to_us(prev.0 as isize);
    if auto {
        if let Err(why) = autopaste_gate(has_target, is_self, true) {
            // 拒绝时窗口留着：原因要当场看得见，而且内容已经在剪贴板了 ——
            // 藏起来的唯一后果是用户以为没复制上，下次打开才看到一句话。
            crate::tracing_note(&format!("clip_autopaste ok=false why={why} kept=1"));
            let msg = kept_message(why);
            with_state(hwnd, |st| st.status = Some(msg));
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            return;
        }
    } else if !has_target {
        // auto_paste 关着且没有目标：复制完就关窗（保持既有行为）。
        hide_palette(hwnd);
        return;
    }
    hide_palette(hwnd);
    start_restore(hwnd, prev, auto);
}

/// 自动粘贴被拒绝、窗口留在原地时那句状态。`why` 来自 `autopaste_gate`。
fn kept_message(why: &str) -> String {
    match why {
        "没有可粘贴的目标窗口" => "已复制 · 没有可粘贴的窗口（Esc 关闭）".to_string(),
        "目标是本程序自己的窗口" => {
            "已复制 · 目标是本程序窗口，未自动粘贴（Ctrl+V 粘贴，Esc 关闭）".to_string()
        }
        other => format!("已复制 · 未自动粘贴：{other}；请按 Ctrl+V"),
    }
}

/// 目标窗口能不能作为粘贴对象。纯函数：三条否决理由各自能被单测点到。
fn autopaste_gate(
    has_target: bool,
    is_self: bool,
    foreground_ok: bool,
) -> Result<(), &'static str> {
    if !has_target {
        return Err("没有可粘贴的目标窗口");
    }
    if is_self {
        return Err("目标是本程序自己的窗口");
    }
    if !foreground_ok {
        return Err("焦点没能还给原窗口（前台锁或提权窗口）");
    }
    Ok(())
}

fn start_restore(hwnd: HWND, target: HWND, paste: bool) {
    with_state(hwnd, |st| {
        st.restore = Some(RestoreJob {
            target,
            paste,
            ticks_left: RESTORE_MAX_TICKS,
            stray_ticks: 0,
            claim_left: 1,
        });
    });
    unsafe {
        let _ = SetTimer(Some(hwnd), RESTORE_TIMER, RESTORE_TICK_MS, None);
    }
    // 第一跳立刻做：多数机器上隐藏弹窗后前台当场就是目标窗口，不必等 25ms。
    restore_tick(hwnd);
}

fn clear_restore(hwnd: HWND) {
    with_state(hwnd, |st| st.restore = None);
    unsafe {
        let _ = KillTimer(Some(hwnd), RESTORE_TIMER);
    }
}

/// 重新打开弹窗时取消还没跑完的任务：否则那一发 Ctrl+V 可能落进搜索框。
fn cancel_restore(hwnd: HWND) {
    let pending = with_state(hwnd, |st| st.restore.is_some()).unwrap_or(false);
    if pending {
        clear_restore(hwnd);
        crate::tracing_note("剪贴板自动粘贴被新的弹窗取消");
    }
}

/// 一跳的结论。抽成纯函数：目标没了 / 前台换了 / 该不该继续等，都能脱离窗口句柄单测。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreAction {
    TargetGone,
    ForegroundReady,
    ClaimAndWait,
    GiveUpStray,
    GiveUpTimeout,
    Wait,
    WaitStray,
}

/// 一跳的结论。`can_claim` = 这次还允许硬抢前台：第一跳必须抢一次（正常粘贴
/// 靠它），之后若用户已经点去第三方窗口，就只数 stray、不再每跳硬拽。
fn next_restore_action(
    target_alive: bool,
    foreground_is_target: bool,
    foreground_is_self: bool,
    ticks_left: u8,
    stray_ticks: u8,
    can_claim: bool,
) -> RestoreAction {
    if !target_alive {
        return RestoreAction::TargetGone;
    }
    if foreground_is_target {
        return RestoreAction::ForegroundReady;
    }
    if can_claim {
        return RestoreAction::ClaimAndWait;
    }
    if !foreground_is_self && stray_ticks >= RESTORE_MAX_STRAY {
        return RestoreAction::GiveUpStray;
    }
    if ticks_left == 0 {
        return RestoreAction::GiveUpTimeout;
    }
    if foreground_is_self {
        RestoreAction::Wait
    } else {
        RestoreAction::WaitStray
    }
}

/// 一跳：确认目标还活着 → 最小化就还原 → 抢前台 → 前台真的换了才粘贴。
fn restore_tick(hwnd: HWND) {
    let Some(job) = with_state(hwnd, |st| st.restore).flatten() else {
        unsafe {
            let _ = KillTimer(Some(hwnd), RESTORE_TIMER);
        }
        return;
    };
    let target_alive = !job.target.0.is_null() && unsafe { IsWindow(Some(job.target)) }.as_bool();
    if target_alive && unsafe { IsIconic(job.target) }.as_bool() {
        unsafe {
            let _ = ShowWindowAsync(job.target, SW_RESTORE);
        }
    }
    let fg = unsafe { GetForegroundWindow() };
    let action = next_restore_action(
        target_alive,
        target_alive && fg == job.target,
        fg == hwnd,
        job.ticks_left,
        job.stray_ticks,
        job.claim_left > 0,
    );
    match action {
        RestoreAction::TargetGone => {
            finish_restore(hwnd, job, Err("原窗口已经关闭".to_string()));
        }
        RestoreAction::ClaimAndWait => {
            claim_foreground(job.target);
            with_state(hwnd, |st| {
                if let Some(j) = st.restore.as_mut() {
                    j.claim_left = j.claim_left.saturating_sub(1);
                    j.ticks_left = j.ticks_left.saturating_sub(1);
                }
            });
        }
        RestoreAction::ForegroundReady => {
            if job.paste {
                match send_ctrl_v() {
                    Ok(()) => crate::tracing_note("clip_autopaste ok=true"),
                    Err(e) => {
                        crate::tracing_note(&format!("clip_autopaste ok=false why={e}"));
                        note_restore_issue(
                            "提权窗口拦下了模拟按键；内容已在剪贴板，请按 Ctrl+V".to_string(),
                        );
                    }
                }
            }
            clear_restore(hwnd);
        }
        RestoreAction::GiveUpStray => {
            finish_restore(hwnd, job, Err("前台已经切到别的窗口".to_string()));
        }
        RestoreAction::GiveUpTimeout => {
            let why = if job.paste {
                "没能把焦点还给原窗口（前台锁或提权窗口）；内容已在剪贴板，请按 Ctrl+V".to_string()
            } else {
                "没能把焦点还给原窗口（前台锁或提权窗口）".to_string()
            };
            finish_restore(hwnd, job, Err(why));
        }
        RestoreAction::Wait | RestoreAction::WaitStray => {
            let stray = action == RestoreAction::WaitStray;
            with_state(hwnd, |st| {
                if let Some(j) = st.restore.as_mut() {
                    j.ticks_left = j.ticks_left.saturating_sub(1);
                    j.stray_ticks = if stray {
                        j.stray_ticks.saturating_add(1)
                    } else {
                        0
                    };
                }
            });
        }
    }
}

fn finish_restore(hwnd: HWND, job: RestoreJob, outcome: Result<(), String>) {
    if let Err(why) = outcome {
        if job.paste {
            crate::tracing_note(&format!("clip_autopaste ok=false why={why}"));
        } else {
            crate::tracing_note(&format!("剪贴板弹窗关闭后没能把焦点还给原窗口：{why}"));
        }
        note_restore_issue(why);
    }
    clear_restore(hwnd);
}

fn note_restore_issue(msg: String) {
    if let Ok(mut slot) = LAST_RESTORE_ISSUE.lock() {
        *slot = Some(msg);
    }
}

fn take_restore_issue() -> Option<String> {
    LAST_RESTORE_ISSUE
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
}

/// 往当前前台窗口注入一次 Ctrl+V。返回值是"插入了几个事件"，被 UIPI 拦下时是 0。
fn send_ctrl_v() -> Result<(), String> {
    fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: if up {
                        KEYEVENTF_KEYUP
                    } else {
                        KEYBD_EVENT_FLAGS(0)
                    },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }
    let inputs = [
        key(VK_CONTROL, false),
        key(VK_V, false),
        key(VK_V, true),
        key(VK_CONTROL, true),
    ];
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        return Err(format!(
            "SendInput 只插入 {sent}/{} 个事件（提权窗口或 UIPI 拦截）",
            inputs.len()
        ));
    }
    Ok(())
}

pub fn hide() {
    let hwnd = PALETTE_HWND.load(Ordering::Acquire);
    if hwnd == 0 {
        return;
    }
    hide_inner(HWND(hwnd as *mut core::ffi::c_void), false);
}

pub fn destroy() {
    let hwnd = PALETTE_HWND.load(Ordering::Acquire);
    if hwnd == 0 {
        return;
    }
    unsafe {
        let _ = DestroyWindow(HWND(hwnd as *mut core::ffi::c_void));
    }
    PALETTE_HWND.store(0, Ordering::Release);
}

pub fn notify_changed() {
    let hwnd = PALETTE_HWND.load(Ordering::Acquire);
    if hwnd == 0 {
        return;
    }
    let hwnd = HWND(hwnd as *mut core::ffi::c_void);
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return;
    }
    with_state(hwnd, |st| st.reload_all(hwnd, ReloadIntent::Keep));
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

/// 隐藏弹窗的唯一入口。顺手停掉"抢前台"重试定时器：只 `ShowWindow(SW_HIDE)`
/// 会让那个定时器继续每 80ms 抢一次前台，用户看不到窗却觉得键盘被抢走。
fn hide_palette(hwnd: HWND) {
    unsafe {
        let _ = KillTimer(Some(hwnd), ACTIVATE_TIMER);
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}

fn hide_inner(hwnd: HWND, restore_focus: bool) {
    let prev = with_state(hwnd, |st| st.prev_fg).unwrap_or_default();
    hide_palette(hwnd);
    if restore_focus
        && !prev.0.is_null()
        && prev != hwnd
        && unsafe { IsWindow(Some(prev)) }.as_bool()
    {
        start_restore(hwnd, prev, false);
    }
}

// ---------------------------------------------------------------------------
// 窗口过程
// ---------------------------------------------------------------------------

unsafe extern "system" fn popup_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_SIZE => {
            if let Some((edit, lay)) = with_state(hwnd, |st| {
                let (cw, ch) = client_size(hwnd);
                (st.edit, layout_for(cw, ch, st.dpi))
            }) {
                unsafe {
                    let _ = SetWindowPos(
                        edit,
                        None,
                        lay.search.left + 8,
                        lay.search.top + 5,
                        (lay.search.right - lay.search.left - 16).max(20),
                        (lay.search.bottom - lay.search.top - 10).max(16),
                        Default::default(),
                    );
                }
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let is_edit = with_state(hwnd, |st| st.edit.0 as isize == l.0).unwrap_or(false);
            if is_edit && ((w.0 as u32 >> 16) as u16) == EN_CHANGE {
                on_query_changed(hwnd);
            }
            LRESULT(0)
        }
        WM_CTLCOLOREDIT => {
            let brush = with_state(hwnd, |st| {
                let hdc = HDC(w.0 as *mut core::ffi::c_void);
                unsafe {
                    SetTextColor(hdc, st.pal.text);
                    SetBkColor(hdc, st.pal.card);
                }
                st.edit_bg
            });
            LRESULT(brush.map(|b| b.0 as isize).unwrap_or(0))
        }
        WM_KEYDOWN => {
            let vk = w.0 as u32;
            let ctrl = (unsafe { GetKeyState(VK_CONTROL.0 as i32) } as u16 & 0x8000) != 0;
            if ctrl && vk == VK_RETURN.0 as u32 {
                copy_without_closing(hwnd);
            } else {
                handle_key(hwnd, vk);
            }
            LRESULT(0)
        }
        WM_SYSKEYDOWN => {
            if handle_sys_key(hwnd, w.0 as u32) {
                LRESULT(0)
            } else {
                DefWindowProcW(hwnd, msg, w, l)
            }
        }
        WM_MOUSEWHEEL => {
            let delta = ((w.0 as u32 >> 16) as u16) as i16 as i32;
            with_state(hwnd, |st| {
                let len = st.rows().len();
                let (cw, ch) = client_size(hwnd);
                let lay = layout_for(cw, ch, st.dpi);
                let max_scroll = len.saturating_sub(lay.rows);
                let step = if delta > 0 { -2 } else { 2 };
                let next = (st.scroll as i32 + step).clamp(0, max_scroll as i32) as usize;
                st.scroll = next;
                if len > 0 {
                    // 滚动之后选中项必须还在可视区，否则 Enter 会复制到看不见的那一行。
                    let last = (next + lay.rows).saturating_sub(1).min(len - 1);
                    st.sel = st.sel.clamp(next, last);
                }
            });
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            // 单击只选中：窗口不关、剪贴板不动。执行粘贴要双击或回车 ——
            // 与列表类软件的通用习惯一致，也避免想看一眼却误粘进原窗口。
            let (x, y) = mouse_xy(l);
            if let Some(idx) = row_at(hwnd, x, y) {
                with_state(hwnd, |st| {
                    st.sel = idx;
                    // 窗口不关了，键盘焦点必须留在搜索框：点完一行还能接着打字筛。
                    if st.mode == Mode::History {
                        unsafe {
                            let _ = SetFocus(Some(st.edit));
                        }
                    }
                });
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            let (x, y) = mouse_xy(l);
            if let Some(idx) = row_at(hwnd, x, y) {
                let copied = with_state(hwnd, |st| {
                    st.sel = idx;
                    st.copy_selected()
                })
                .unwrap_or(false);
                finish_copy(hwnd, copied);
            }
            LRESULT(0)
        }
        WM_RBUTTONDOWN => {
            let (x, y) = mouse_xy(l);
            // 空白处也要能开菜单：全部是列表项时，想清空历史/暂停记录还得先右键到
            // 某一行上，空列表里则根本没有入口。
            if let Some(idx) = row_at(hwnd, x, y) {
                with_state(hwnd, |st| st.sel = idx);
            }
            open_menu(hwnd, x, y);
            LRESULT(0)
        }
        WM_ACTIVATE => {
            let code = w.0 as u32 & 0xFFFF;
            if code == WA_INACTIVE {
                let (activated, menu_open, modal_open) =
                    with_state(hwnd, |st| (st.activated, st.menu_open, st.modal_open))
                        .unwrap_or((false, false, false));
                // 第一次激活还没拿到时的失活不是"用户点走了"：那一下关窗，热键就像没反应。
                if should_hide_on_deactivate(activated, menu_open, modal_open) {
                    hide_inner(hwnd, false);
                }
            } else {
                with_state(hwnd, |st| st.activated = true);
            }
            LRESULT(0)
        }
        WM_TIMER if w.0 == RESTORE_TIMER => {
            restore_tick(hwnd);
            LRESULT(0)
        }
        WM_TIMER if w.0 == ACTIVATE_TIMER => {
            unsafe {
                let _ = KillTimer(Some(hwnd), ACTIVATE_TIMER);
            }
            if unsafe { GetForegroundWindow() } != hwnd {
                claim_foreground(hwnd);
                if let Some(edit) = with_state(hwnd, |st| st.edit) {
                    unsafe {
                        let _ = SetFocus(Some(edit));
                    }
                }
                let left = with_state(hwnd, |st| {
                    st.activate_tries = st.activate_tries.saturating_sub(1);
                    st.activate_tries
                })
                .unwrap_or(0);
                if unsafe { GetForegroundWindow() } != hwnd {
                    if left > 0 {
                        unsafe {
                            let _ = SetTimer(Some(hwnd), ACTIVATE_TIMER, 80, None);
                        }
                    } else {
                        crate::tracing_note(
                            "剪贴板弹窗没能抢到前台：键盘搜索可能收不到（前台锁/提权窗口）",
                        );
                    }
                }
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            hide_inner(hwnd, true);
            LRESULT(0)
        }
        WM_DESTROY => {
            if let Some(p) = state_ptr(hwnd) {
                drop(Box::from_raw(p));
            }
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            PALETTE_HWND.store(0, Ordering::Release);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

unsafe extern "system" fn edit_proc(
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if msg == WM_SYSKEYDOWN {
        let parent = GetParent(hwnd).unwrap_or_default();
        if handle_sys_key(parent, w.0 as u32) {
            return LRESULT(0);
        }
    }
    if msg == WM_KEYDOWN {
        let parent = GetParent(hwnd).unwrap_or_default();
        let vk = w.0 as u32;
        let ctrl = (GetKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
        // Ctrl+Enter 只复制不关窗；必须在 `handle_key` 之前判，否则回车会被当成粘贴。
        if ctrl && vk == VK_RETURN.0 as u32 {
            copy_without_closing(parent);
            return LRESULT(0);
        }
        // Delete：Ctrl+Del 总是删当前行；空搜索框里的 Del 也是（空框里它本来什么
        // 都不做）；有字又没有 Ctrl 时留给文本框删字符；Shift+Del 整个不碰。
        if vk == VK_DELETE.0 as u32 {
            let shift = (GetKeyState(0x10) as u16 & 0x8000) != 0;
            let len = GetWindowTextLengthW(hwnd).max(0) as usize;
            if !shift && delete_key_goes_to_list(ctrl, len) {
                delete_row(parent);
                return LRESULT(0);
            }
        }
        if edit_key_goes_to_list(vk) && handle_key(parent, vk) {
            return LRESULT(0);
        }
        // Ctrl+1..6：右击菜单里那六种文本加工也给了键盘入口（菜单标签上带着 \tCtrl+N）。
        if ctrl && (0x31..=0x36).contains(&vk) {
            let idx = (vk - 0x31) as usize;
            if let Some(t) = Transform::ALL.get(idx).copied() {
                let copied = with_state(parent, |st| st.apply_transform(t)).unwrap_or(false);
                finish_copy(parent, copied);
            }
            return LRESULT(0);
        }
        // Ctrl+P 固定：`handle_key` 不接修饰键，单独判一次。
        if ctrl && vk == 0x50 {
            with_state(parent, |st| st.toggle_pin(parent));
            let _ = InvalidateRect(Some(parent), None, false);
            return LRESULT(0);
        }
    }
    DefSubclassProc(hwnd, msg, w, l)
}

/// 搜索框里哪些键交给列表（导航与执行）。`Delete` 不在这一组：它自己有
/// `delete_key_goes_to_list` 的三档规则（空框 / Ctrl / Shift）。
fn edit_key_goes_to_list(vk: u32) -> bool {
    [
        VK_UP, VK_DOWN, VK_PRIOR, VK_NEXT, VK_HOME, VK_END, VK_RETURN, VK_ESCAPE, VK_TAB, VK_F1,
    ]
    .map(|k| k.0 as u32)
    .contains(&vk)
}

/// 搜索框里的 Delete 什么时候归列表：`Ctrl+Del` 总是归（有字也能删这一条）；
/// 不带修饰时只在搜索框为空才归 —— 空框里 Delete 在 EDIT 里本来就是空操作。
/// `Shift+Del` 由调用方先挡掉（保留文本框的剪切语义），所以这里不看 Shift。
fn delete_key_goes_to_list(ctrl: bool, edit_len: usize) -> bool {
    ctrl || edit_len == 0
}

/// 删除这一条之前要不要问一句：片段与固定记录会弹确认框，普通未固定历史直接删。
/// 以前一律不问，Del 一下少一条；现在固定项也不会被误删。
fn needs_confirm(mode: Mode, pinned: bool) -> bool {
    mode == Mode::Snippets || pinned
}

/// 失活这一下到底是不是"用户点走了"。菜单与确认框都是本窗自己的模态子窗，
/// 它们出现时也会发失活；把这两类当成点走，窗会在用户还没回答前消失。
fn should_hide_on_deactivate(activated: bool, menu_open: bool, modal_open: bool) -> bool {
    activated && !menu_open && !modal_open
}

/// 列表里的"删除当前行"唯一入口：键盘 Del、菜单「删除」都走这里。
fn delete_row(hwnd: HWND) {
    let Some((ask, what)) = with_state(hwnd, |st| {
        let row = st.selected()?;
        let what = if st.mode == Mode::Snippets {
            "个片段"
        } else if row.pinned {
            "条固定记录"
        } else {
            "条记录"
        };
        Some((needs_confirm(st.mode, row.pinned), what))
    })
    .flatten() else {
        return;
    };
    if ask {
        let msg = format!("删除这 1 {what}？");
        let text = windows::core::HSTRING::from(msg);
        // 确认框是本窗的模态子窗：它出现的失活不是"用户点走了"（见 `should_hide_on_deactivate`）。
        with_state(hwnd, |st| st.modal_open = true);
        let answer = unsafe {
            MessageBoxW(
                Some(hwnd),
                windows::core::PCWSTR(text.as_ptr()),
                w!("删除剪贴板内容"),
                MESSAGEBOX_STYLE(MB_YESNO.0 | MB_ICONQUESTION.0),
            )
        };
        with_state(hwnd, |st| st.modal_open = false);
        if answer != IDYES {
            return;
        }
    }
    with_state(hwnd, |st| st.delete_selected(hwnd));
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

/// 返回 true = 这个键被弹窗吃掉了。
fn handle_key(hwnd: HWND, vk: u32) -> bool {
    match vk {
        x if x == VK_UP.0 as u32 => {
            with_state(hwnd, |st| st.move_selection(hwnd, -1));
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            true
        }
        x if x == VK_DOWN.0 as u32 => {
            with_state(hwnd, |st| st.move_selection(hwnd, 1));
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            true
        }
        x if x == VK_PRIOR.0 as u32 || x == VK_NEXT.0 as u32 => {
            let dir = if x == VK_PRIOR.0 as u32 { -1 } else { 1 };
            with_state(hwnd, |st| {
                let step = st.visible_rows(hwnd).saturating_sub(1).max(1) as i32;
                st.move_selection(hwnd, dir * step);
            });
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            true
        }
        x if x == VK_HOME.0 as u32 || x == VK_END.0 as u32 => {
            // 搜索框有文字时 Home/End 是光标键，不能抢。
            let caret = with_state(hwnd, |st| {
                !st.query.is_empty() && unsafe { GetFocus() } == st.edit
            })
            .unwrap_or(false);
            if caret {
                return false;
            }
            let end = x == VK_END.0 as u32;
            with_state(hwnd, |st| {
                let idx = if end {
                    st.rows().len().saturating_sub(1)
                } else {
                    0
                };
                st.move_selection_to(hwnd, idx);
            });
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            true
        }
        x if x == VK_RETURN.0 as u32 => {
            let copied = with_state(hwnd, |st| st.copy_selected()).unwrap_or(false);
            crate::tracing_note(&format!("剪贴板弹窗回车 clip_enter copied={copied}"));
            finish_copy(hwnd, copied);
            true
        }
        x if x == VK_ESCAPE.0 as u32 => {
            hide_inner(hwnd, true);
            true
        }
        x if x == VK_TAB.0 as u32 => {
            with_state(hwnd, |st| {
                st.mode = match st.mode {
                    Mode::History => Mode::Snippets,
                    Mode::Snippets => Mode::Stats,
                    Mode::Stats => Mode::History,
                };
                st.sel = 0;
                st.scroll = 0;
                st.status = None;
                st.clamp_selection();
            });
            update_edit_visibility(hwnd);
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            true
        }
        x if x == VK_DELETE.0 as u32 => {
            delete_row(hwnd);
            true
        }
        x if x == VK_F1.0 as u32 => {
            with_state(hwnd, |st| {
                st.mode = if st.mode == Mode::Stats {
                    Mode::History
                } else {
                    Mode::Stats
                };
                st.sel = 0;
                st.scroll = 0;
            });
            update_edit_visibility(hwnd);
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            true
        }
        _ => false,
    }
}

/// Ctrl+Enter：只复制、不关窗、不粘贴。给"还要接着翻历史"的人一个出口。
fn copy_without_closing(hwnd: HWND) -> bool {
    let copied = with_state(hwnd, |st| {
        let ok = st.copy_selected();
        if ok {
            st.status = Some("已复制，窗口保持打开".to_string());
        }
        ok
    })
    .unwrap_or(false);
    crate::tracing_note(&format!("剪贴板弹窗 Ctrl+Enter copied={copied}"));
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
    true
}

/// Alt+1..9：选中当前可视第 n 行并走正常复制路径。`WM_SYSKEYDOWN` 才收得到。
fn handle_sys_key(hwnd: HWND, vk: u32) -> bool {
    if !(0x31..=0x39).contains(&vk) {
        return false;
    }
    let picked = with_state(hwnd, |st| {
        let idx = st.scroll + (vk - 0x31) as usize;
        if idx >= st.rows().len() {
            return false;
        }
        st.sel = idx;
        true
    })
    .unwrap_or(false);
    if picked {
        let copied = with_state(hwnd, |st| st.copy_selected()).unwrap_or(false);
        crate::tracing_note(&format!("剪贴板弹窗 Alt+{} copied={copied}", vk - 0x30));
        finish_copy(hwnd, copied);
    }
    true
}

fn update_edit_visibility(hwnd: HWND) {
    if let Some((edit, mode)) = with_state(hwnd, |st| (st.edit, st.mode)) {
        unsafe {
            // 只有历史页有搜索框。片段页与统计页把它藏起来：留着却不过滤片段/统计，
            // 比看不见更像坏了。切回历史时顺手把焦点还给它。
            if mode == Mode::History {
                let _ = ShowWindow(edit, SW_SHOW);
                let _ = SetFocus(Some(edit));
            } else {
                let _ = ShowWindow(edit, SW_HIDE);
                let _ = SetFocus(Some(hwnd));
            }
        }
    }
}

fn on_query_changed(hwnd: HWND) {
    let st = match unsafe { state_ptr(hwnd) } {
        Some(p) => p,
        None => return,
    };
    let text = unsafe { edit_text((*st).edit) };
    unsafe {
        (*st).query = text;
        (*st).sel = 0;
        (*st).scroll = 0;
        (*st).reload_history(hwnd, ReloadIntent::ResetTop);
        crate::tracing_note(&format!(
            "剪贴板搜索 clip_search qlen={} n={}",
            (*st).query.chars().count(),
            (*st).history.len()
        ));
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

unsafe fn edit_text(hwnd: HWND) -> String {
    let len = GetWindowTextLengthW(hwnd).max(0) as usize;
    let mut buf = vec![0u16; len + 1];
    let n = GetWindowTextW(hwnd, &mut buf).max(0) as usize;
    String::from_utf16_lossy(&buf[..n.min(len)])
}

fn mouse_xy(l: LPARAM) -> (i32, i32) {
    let raw = l.0 as u32;
    let x = (raw & 0xFFFF) as u16 as i16 as i32;
    let y = ((raw >> 16) & 0xFFFF) as u16 as i16 as i32;
    (x, y)
}

fn row_at(hwnd: HWND, x: i32, y: i32) -> Option<usize> {
    with_state(hwnd, |st| {
        let (cw, ch) = client_size(hwnd);
        let lay = layout_for(cw, ch, st.dpi);
        let in_list = x >= lay.pad
            && x <= cw - lay.pad
            && y >= lay.list_top
            && y < lay.list_top + lay.row_h * lay.rows as i32;
        if !in_list {
            return None;
        }
        let idx = st.scroll + ((y - lay.list_top) / lay.row_h) as usize;
        if idx < st.rows().len() {
            Some(idx)
        } else {
            None
        }
    })
    .flatten()
}

// ---------------------------------------------------------------------------
// 绘制
// ---------------------------------------------------------------------------

fn paint(hwnd: HWND) {
    let mut ps = windows::Win32::Graphics::Gdi::PAINTSTRUCT::default();
    let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
    unsafe {
        let _ = with_state(hwnd, |st| draw(hdc, hwnd, st));
        let _ = EndPaint(hwnd, &ps);
    }
}

unsafe fn draw(hdc: HDC, hwnd: HWND, st: &mut State) {
    let (w, h) = client_size(hwnd);
    let lay = layout_for(w, h, st.dpi);
    let bg = CreateSolidBrush(st.pal.bg);
    let card = CreateSolidBrush(st.pal.card);
    let line = CreateSolidBrush(st.pal.line);
    let accent = CreateSolidBrush(st.pal.accent);
    let selected = CreateSolidBrush(st.pal.selected);

    fill(hdc, 0, 0, w, h, bg);

    // 标题 + 当前模式
    let title = match st.mode {
        Mode::History => "剪贴板历史",
        Mode::Snippets => "片段",
        Mode::Stats => "剪贴板统计",
    };
    let count = match st.mode {
        Mode::History => format!("{} 条", st.history.len()),
        Mode::Snippets => format!("{} 条", st.snippets.len()),
        Mode::Stats => String::new(),
    };
    draw_str(
        hdc,
        title,
        &rect(lay.pad, 12, 240, 24),
        st.pal.text,
        st.font,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
    );
    if !count.is_empty() {
        draw_str(
            hdc,
            &count,
            &rect(w - lay.pad - 120, 14, 120, 20),
            st.pal.muted,
            st.font_small,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
    }

    // 搜索框底（EDIT 子窗自己画字）
    if st.mode == Mode::History {
        fill(
            hdc,
            lay.search.left,
            lay.search.top,
            lay.search.right,
            lay.search.bottom,
            card,
        );
        let _ = FrameRect(hdc, &lay.search, line);
    }

    match st.mode {
        Mode::Stats => draw_stats(hdc, st, lay, w),
        _ => draw_rows(hdc, st, lay, w, card, selected),
    }

    // 底部提示 / 状态
    let mut footer = if let Some(status) = &st.status {
        status.clone()
    } else {
        match st.mode {
            Mode::History => {
                "单击选中 · 双击/Enter 粘贴 · Ctrl+Enter 只复制 · Tab 片段 · Esc 关闭".to_string()
            }
            Mode::Snippets => "Enter 复制片段 · Del 删除 · Tab 统计 · Esc 关闭".to_string(),
            Mode::Stats => "Tab 回历史 · Esc 关闭".to_string(),
        }
    };
    if focusflow_core::clipboard::is_paused() {
        footer.push_str(" · 记录已暂停");
    }
    if let Some(note) =
        focusflow_core::clipboard::capture_notice(focusflow_core::clipboard::now_ms())
    {
        // 这一笔没收进来的原因：最多占 60 秒，成功收录一次就清。
        footer = format!("{note} · {footer}");
    }
    let muted = st.pal.muted;
    draw_str(
        hdc,
        &footer,
        &rect(lay.pad, lay.footer_top, w - lay.pad * 2, 20),
        muted,
        st.font_small,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
    );
    let right_note = if !focusflow_core::config::instance().get_bool("clipboard", "enabled", true) {
        Some("记录已关闭（只显示已有条目）".to_string())
    } else {
        // 监听线程起不来/退出：旧记录还画得出来，但新的复制一条都不会进来。
        focusflow_core::clipboard::watcher_fault().map(|why| format!("监听未运行：{why}"))
    };
    if let Some(note) = right_note {
        draw_str(
            hdc,
            &note,
            &rect(w - lay.pad - 260, lay.footer_top, 260, 20),
            st.pal.accent,
            st.font_small,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
    }

    let _ = DeleteObject(HGDIOBJ(bg.0));
    let _ = DeleteObject(HGDIOBJ(card.0));
    let _ = DeleteObject(HGDIOBJ(line.0));
    let _ = DeleteObject(HGDIOBJ(accent.0));
    let _ = DeleteObject(HGDIOBJ(selected.0));
}

unsafe fn draw_rows(hdc: HDC, st: &mut State, lay: Layout, w: i32, card: HBRUSH, selected: HBRUSH) {
    let list_bottom = lay.list_top + lay.row_h * lay.rows as i32;
    fill(hdc, lay.pad, lay.list_top, w - lay.pad, list_bottom, card);

    let len = st.rows().len();
    if len == 0 {
        let msg = if st.mode == Mode::History && !st.query.trim().is_empty() {
            "没有匹配的记录"
        } else if st.mode == Mode::Snippets {
            "还没有片段：在历史条目上右键「存为片段」"
        } else {
            "还没有记录：复制一段文字试试"
        };
        let text = st.pal.muted;
        let font = st.font_small;
        draw_str(
            hdc,
            msg,
            &rect(lay.pad + 8, lay.list_top + 18, w - lay.pad * 2 - 16, 24),
            text,
            font,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
        return;
    }

    let visible = lay.rows.min(len.saturating_sub(st.scroll));
    // 只对将要画出来的文件行问磁盘：500 条历史每条最多 20 个路径，全查会卡住每一键。
    st.fill_missing_visible(st.scroll, visible);
    let rows = match st.mode {
        Mode::History => &st.history,
        Mode::Snippets => &st.snippets,
        Mode::Stats => return,
    };
    for i in 0..visible {
        let idx = st.scroll + i;
        let Some(row) = rows.get(idx) else { break };
        let top = lay.list_top + i as i32 * lay.row_h;
        if idx == st.sel {
            fill(hdc, lay.pad, top, w - lay.pad, top + lay.row_h, selected);
        }
        let text_color = st.pal.text;
        let muted = st.pal.muted;
        let pin_color = st.pal.accent;
        let font = st.font_small;
        if st.mode == Mode::History {
            let time_w = 86;
            let source_w = 118;
            draw_str(
                hdc,
                &row.time,
                &rect(lay.pad + 8, top + 9, time_w, 18),
                muted,
                font,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
            draw_str(
                hdc,
                &row.source,
                &rect(lay.pad + 8 + time_w, top + 9, source_w, 18),
                muted,
                font,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
            let preview = row_label(row);
            draw_str(
                hdc,
                &preview,
                &rect(
                    lay.pad + 8 + time_w + source_w,
                    top + 9,
                    (w - lay.pad * 2 - 16 - time_w - source_w - 28).max(20),
                    18,
                ),
                text_color,
                font,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
            if row.pinned {
                draw_str(
                    hdc,
                    "★",
                    &rect(w - lay.pad - 28, top + 8, 20, 20),
                    pin_color,
                    font,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
                );
            }
        } else {
            let name_w = 160;
            draw_str(
                hdc,
                &row.name,
                &rect(lay.pad + 8, top + 9, name_w, 18),
                text_color,
                font,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
            let preview = preview_text(&row.content);
            draw_str(
                hdc,
                &preview,
                &rect(
                    lay.pad + 8 + name_w,
                    top + 9,
                    (w - lay.pad * 2 - 16 - name_w).max(20),
                    18,
                ),
                muted,
                font,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
        }
    }

    // 细分割线 + 滚动指示
    let line = CreateSolidBrush(st.pal.line);
    for i in 1..=visible {
        let y = lay.list_top + i as i32 * lay.row_h;
        // `fill` 的四个参数是左/上/右/下；用 `rect(左, 上, 宽, 高)` 会把宽高再当坐标加一遍，
        // 症状是一整块灰色矩形盖住列表（屏证逮到的）。
        fill(hdc, lay.pad + 8, y, w - lay.pad - 8, y + 1, line);
    }
    let _ = DeleteObject(HGDIOBJ(line.0));
    if rows.len() > lay.rows {
        let label = format!("{}/{}", st.scroll + visible, rows.len());
        draw_str(
            hdc,
            &label,
            &rect(w - lay.pad - 70, lay.footer_top - 18, 60, 16),
            st.pal.muted,
            st.font_small,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
    }
}

fn draw_stats(hdc: HDC, st: &mut State, lay: Layout, w: i32) {
    let Some(s) = &st.stats else {
        draw_str(
            hdc,
            "统计读不出来",
            &rect(lay.pad + 8, lay.list_top + 18, w - lay.pad * 2, 24),
            st.pal.muted,
            st.font_small,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
        return;
    };
    let lines = vec![
        format!("今日复制 {} 次", s.today),
        format!("最近 {} 天 {} 次", s.window_days, s.window_count),
        format!(
            "库里 {} 条（固定 {} 条） · 累计复制 {} 次",
            s.total_items, s.pinned_items, s.total_copies
        ),
    ];
    let mut y = lay.list_top + 10;
    for line in &lines {
        draw_str(
            hdc,
            line,
            &rect(lay.pad + 8, y, w - lay.pad * 2 - 16, 22),
            st.pal.text,
            st.font_small,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
        y += 26;
    }
    y += 8;
    draw_str(
        hdc,
        "来源（最近 7 天）",
        &rect(lay.pad + 8, y, w - lay.pad * 2, 20),
        st.pal.muted,
        st.font_small,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
    );
    y += 24;
    if s.per_app.is_empty() {
        draw_str(
            hdc,
            "还没有来源数据",
            &rect(lay.pad + 8, y, w - lay.pad * 2, 20),
            st.pal.muted,
            st.font_small,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
    } else {
        for (app, count) in &s.per_app {
            let name = if app.is_empty() { "未知来源" } else { app };
            let line = format!("{name} · {count} 次");
            draw_str(
                hdc,
                &line,
                &rect(lay.pad + 8, y, w - lay.pad * 2 - 16, 20),
                st.pal.text,
                st.font_small,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
            y += 22;
        }
    }
}

fn rect(left: i32, top: i32, width: i32, height: i32) -> RECT {
    RECT {
        left,
        top,
        right: left + width.max(0),
        bottom: top + height.max(0),
    }
}

fn fill(hdc: HDC, left: i32, top: i32, right: i32, bottom: i32, brush: HBRUSH) {
    let r = RECT {
        left,
        top,
        right,
        bottom,
    };
    unsafe {
        let _ = FillRect(hdc, &r, brush);
    }
}

fn draw_str(hdc: HDC, text: &str, r: &RECT, color: COLORREF, font: HFONT, flags: DRAW_TEXT_FORMAT) {
    if text.is_empty() {
        return;
    }
    unsafe {
        let old = SelectObject(hdc, HGDIOBJ(font.0));
        SetTextColor(hdc, color);
        SetBkMode(hdc, TRANSPARENT);
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        let mut rr = *r;
        DrawTextW(hdc, &mut wide, &mut rr, flags);
        SelectObject(hdc, old);
    }
}

// ---------------------------------------------------------------------------
// 右键菜单
// ---------------------------------------------------------------------------

fn open_menu(hwnd: HWND, x: i32, y: i32) {
    let (mode, pinned, has_row) = with_state(hwnd, |st| {
        (
            st.mode,
            st.selected().map(|r| r.pinned).unwrap_or(false),
            st.selected().is_some(),
        )
    })
    .unwrap_or((Mode::History, false, false));
    let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
        return;
    };
    unsafe {
        // 行相关的那几项只在真有行时出现；空白处/空列表只剩全局项。
        if has_row {
            let _ = AppendMenuW(menu, MF_STRING, MENU_COPY as usize, w!("复制"));
            if mode == Mode::History {
                let pin_label = if pinned {
                    w!("取消固定")
                } else {
                    w!("固定")
                };
                let _ = AppendMenuW(menu, MF_STRING, MENU_PIN as usize, pin_label);
            }
            let _ = AppendMenuW(menu, MF_STRING, MENU_DELETE as usize, w!("删除"));
        }
        if has_row && mode == Mode::History {
            let _ = AppendMenuW(menu, MF_STRING, MENU_SNIPPET as usize, w!("存为片段"));
            if let Ok(sub) = CreatePopupMenu() {
                for (i, t) in Transform::ALL.iter().enumerate() {
                    let label: Vec<u16> = format!("{}\tCtrl+{}", t.label(), i + 1)
                        .encode_utf16()
                        .chain(std::iter::once(0))
                        .collect();
                    let _ = AppendMenuW(
                        sub,
                        MF_STRING,
                        (MENU_TRANSFORM_BASE + i as u32) as usize,
                        windows::core::PCWSTR(label.as_ptr()),
                    );
                }
                let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, w!("文本加工（加工后复制）"));
            }
        }
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, w!(""));
        let _ = AppendMenuW(menu, MF_STRING, MENU_STATS as usize, w!("统计"));
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_PAUSE_15 as usize,
            w!("暂停记录 15 分钟"),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_PAUSE_60 as usize,
            w!("暂停记录 60 分钟"),
        );
        // 暂停着的时候才出现：这就是"点错了怎么回去"的唯一入口。
        if clipboard::is_paused() {
            let _ = AppendMenuW(
                menu,
                MF_STRING,
                MENU_RESUME as usize,
                w!("恢复记录（立即）"),
            );
        }
        if mode == Mode::History {
            let _ = AppendMenuW(
                menu,
                MF_STRING,
                MENU_CLEAR as usize,
                w!("清空历史（保留固定项）"),
            );
        }
    }

    let mut pt = POINT { x, y };
    unsafe {
        let _ = windows::Win32::Graphics::Gdi::ClientToScreen(hwnd, &mut pt);
    }
    let flags = TRACK_POPUP_MENU_FLAGS(TPM_RETURNCMD.0 | TPM_LEFTALIGN.0 | TPM_TOPALIGN.0);
    with_state(hwnd, |st| st.menu_open = true);
    let cmd = unsafe { TrackPopupMenu(menu, flags, pt.x, pt.y, None, hwnd, None) }.0 as u32;
    unsafe {
        let _ = DestroyMenu(menu);
    }
    // `0` = 没选中任何项（点外面取消 / 菜单没拿到键盘）。这一行是"菜单点了没反应"
    // 唯一能区分"没开"与"开在别的窗口上"的现场读数。
    crate::tracing_note(&format!("clip_menu cmd={cmd} has_row={has_row}"));

    match cmd {
        MENU_COPY => {
            let copied = with_state(hwnd, |st| st.copy_selected()).unwrap_or(false);
            finish_copy(hwnd, copied);
        }
        MENU_PIN => {
            with_state(hwnd, |st| st.toggle_pin(hwnd));
        }
        MENU_DELETE => {
            delete_row(hwnd);
        }
        MENU_SNIPPET => {
            with_state(hwnd, |st| st.save_snippet(hwnd));
        }
        MENU_STATS => {
            with_state(hwnd, |st| {
                st.mode = Mode::Stats;
                st.sel = 0;
                st.scroll = 0;
            });
            update_edit_visibility(hwnd);
        }
        MENU_PAUSE_15 => {
            clipboard::pause_for(15 * 60);
            with_state(hwnd, |st| {
                st.status = Some("记录已暂停 15 分钟".to_string())
            });
        }
        MENU_PAUSE_60 => {
            clipboard::pause_for(60 * 60);
            with_state(hwnd, |st| {
                st.status = Some("记录已暂停 60 分钟".to_string())
            });
        }
        MENU_RESUME => {
            clipboard::resume_now();
            with_state(hwnd, |st| st.status = Some("记录已恢复".to_string()));
        }
        MENU_CLEAR => {
            let answer = unsafe {
                MessageBoxW(
                    Some(hwnd),
                    w!("只清掉未固定的条目；固定项会留下。"),
                    w!("清空剪贴板历史"),
                    MESSAGEBOX_STYLE(MB_YESNO.0 | MB_ICONQUESTION.0),
                )
            };
            if answer == IDYES {
                match clipboard::clear_unpinned() {
                    Ok(n) => {
                        with_state(hwnd, |st| {
                            st.reload_all(hwnd, ReloadIntent::Keep);
                            st.status = Some(format!("已清空 {n} 条未固定记录"));
                        });
                    }
                    Err(e) => {
                        with_state(hwnd, |st| st.status = Some(format!("清空失败：{e}")));
                    }
                }
            }
        }
        c if c >= MENU_TRANSFORM_BASE => {
            let idx = (c - MENU_TRANSFORM_BASE) as usize;
            if let Some(t) = Transform::ALL.get(idx).copied() {
                let copied = with_state(hwnd, |st| st.apply_transform(t)).unwrap_or(false);
                finish_copy(hwnd, copied);
            }
        }
        _ => {}
    }
    // 菜单（以及「清空」那个确认框）都关掉之后才恢复失活判定；菜单自己的弹开会
    // 让弹窗短暂失活，提前恢复会在确认框还开着时把窗藏掉。
    with_state(hwnd, |st| st.menu_open = false);
    let still_visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
    if still_visible {
        if unsafe { GetForegroundWindow() } != hwnd {
            // 菜单开着的时候用户点去别处了：照旧藏起来。
            hide_palette(hwnd);
        } else if mode == Mode::History {
            // 菜单关掉后焦点回到搜索框，否则"接着打字没反应"。
            if let Some(edit) = with_state(hwnd, |st| st.edit) {
                unsafe {
                    let _ = SetFocus(Some(edit));
                }
            }
        }
    }
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_leaves_room_for_rows_and_footer() {
        let lay = layout_for(1120, 880, 192.0);
        assert!(lay.rows >= 3, "高 DPI 下也要留出至少三行");
        assert!(lay.search.bottom < lay.list_top);
        assert!(lay.footer_top > lay.list_top);
        assert!(lay.search.right > lay.search.left);
    }

    #[test]
    fn layout_survives_a_tiny_window() {
        let lay = layout_for(10, 10, 96.0);
        assert_eq!(lay.rows, 1);
        assert!(lay.search.right > lay.search.left);
    }

    #[test]
    fn preview_flattens_newlines_and_spaces() {
        assert_eq!(preview_text("  a\n\n b\tc  "), "a b c");
        assert_eq!(preview_text("没有空白"), "没有空白");
        assert_eq!(preview_text("   "), "");
    }

    #[test]
    fn short_time_has_a_readable_shape() {
        let t = short_time(1_700_000_000_000);
        assert_eq!(t.len(), 11, "MM-DD HH:MM");
        assert_eq!(&t[2..3], "-");
        assert_eq!(&t[5..6], " ");
        assert_eq!(&t[8..9], ":");
    }

    #[test]
    fn autopaste_gate_names_each_refusal() {
        assert_eq!(autopaste_gate(true, false, true), Ok(()));
        assert_eq!(
            autopaste_gate(false, false, true),
            Err("没有可粘贴的目标窗口")
        );
        assert_eq!(
            autopaste_gate(true, true, true),
            Err("目标是本程序自己的窗口")
        );
        assert_eq!(
            autopaste_gate(true, false, false),
            Err("焦点没能还给原窗口（前台锁或提权窗口）")
        );
    }

    #[test]
    fn delete_in_the_search_box_edits_text_instead_of_deleting_history() {
        assert!(
            !edit_key_goes_to_list(VK_DELETE.0 as u32),
            "Delete 在搜索框里必须留给文本框；交给列表会在改字时删掉一条历史（无确认框）"
        );
        for key in [
            VK_UP, VK_DOWN, VK_PRIOR, VK_NEXT, VK_HOME, VK_END, VK_RETURN, VK_ESCAPE, VK_TAB, VK_F1,
        ] {
            assert!(
                edit_key_goes_to_list(key.0 as u32),
                "导航/执行键 {} 应当交给列表",
                key.0
            );
        }
    }

    fn label_row(kind: ClipKind, missing: usize) -> Row {
        Row {
            id: 1,
            content: "1 个文件：a.txt".to_string(),
            name: String::new(),
            source: String::new(),
            time: String::new(),
            pinned: false,
            kind,
            image_path: None,
            file_paths: vec!["C:\\a.txt".to_string()],
            missing_files: Some(missing),
        }
    }

    #[test]
    fn layout_at_the_new_default_height_shows_twelve_rows() {
        let lay = layout_for(560, 560, 96.0);
        assert_eq!(lay.rows, 12, "560 高应比旧 440 多出四行");
    }

    #[test]
    fn history_loads_past_the_old_hundred_item_cap() {
        let _lock = focusflow_core::paths::test_app_dir_lock();
        let _dir = focusflow_core::paths::test_app_dir("clip_popup_limit");
        focusflow_core::clipboard::init_db().unwrap();
        let t = focusflow_core::clipboard::now_ms();
        for i in 0..120 {
            focusflow_core::clipboard::ingest(&format!("item {i}"), "", t + i).unwrap();
        }
        assert_eq!(
            load_history("").unwrap().len(),
            120,
            "旧实现把上限写死成 100，这里必须是 120"
        );
    }

    #[test]
    fn restore_action_covers_every_exit() {
        assert_eq!(
            next_restore_action(false, false, false, 5, 0, false),
            RestoreAction::TargetGone
        );
        assert_eq!(
            next_restore_action(true, true, false, 5, 0, false),
            RestoreAction::ForegroundReady
        );
        assert_eq!(
            next_restore_action(true, false, false, 5, RESTORE_MAX_STRAY, false),
            RestoreAction::GiveUpStray,
            "用户已经点了别的窗口就不再抢"
        );
        assert_eq!(
            next_restore_action(true, false, false, 0, 0, false),
            RestoreAction::GiveUpTimeout
        );
        assert_eq!(
            next_restore_action(true, false, true, 5, 0, false),
            RestoreAction::Wait,
            "前台还是刚隐藏的弹窗：等它让位"
        );
        assert_eq!(
            next_restore_action(true, false, false, 5, 0, false),
            RestoreAction::WaitStray
        );
    }

    #[test]
    fn the_restore_job_claims_the_foreground_exactly_once() {
        // 第一跳无条件抢一次：正常粘贴靠它。
        assert_eq!(
            next_restore_action(true, false, true, 5, 0, true),
            RestoreAction::ClaimAndWait
        );
        assert_eq!(
            next_restore_action(true, false, false, 5, RESTORE_MAX_STRAY, true),
            RestoreAction::ClaimAndWait,
            "抢的那一跳比 stray 上限优先"
        );
        // 抢过之后（claim_left = 0）不再每跳硬拽：第三方窗口只累计 stray。
        assert_eq!(
            next_restore_action(true, false, false, 5, 0, false),
            RestoreAction::WaitStray
        );
        assert_eq!(
            next_restore_action(true, false, false, 5, RESTORE_MAX_STRAY, false),
            RestoreAction::GiveUpStray
        );
    }

    #[test]
    fn a_refused_autopaste_says_why_the_palette_stayed() {
        assert_eq!(
            kept_message("目标是本程序自己的窗口"),
            "已复制 · 目标是本程序窗口，未自动粘贴（Ctrl+V 粘贴，Esc 关闭）"
        );
        assert_eq!(
            kept_message("没有可粘贴的目标窗口"),
            "已复制 · 没有可粘贴的窗口（Esc 关闭）"
        );
        assert_eq!(
            kept_message("焦点没能还给原窗口（前台锁或提权窗口）"),
            "已复制 · 未自动粘贴：焦点没能还给原窗口（前台锁或提权窗口）；请按 Ctrl+V"
        );
    }

    #[test]
    fn the_delete_key_routing_has_three_tiers() {
        assert!(
            delete_key_goes_to_list(false, 0),
            "空搜索框里的 Del 在文本框里什么也不做，归列表"
        );
        assert!(
            !delete_key_goes_to_list(false, 3),
            "有字又没有 Ctrl：留给文本框删字符"
        );
        assert!(
            delete_key_goes_to_list(true, 3),
            "Ctrl+Del 总是删当前行，搜索框有字也一样"
        );
        assert!(delete_key_goes_to_list(true, 0));
    }

    #[test]
    fn only_pinned_rows_and_snippets_ask_before_deleting() {
        assert!(!needs_confirm(Mode::History, false), "普通历史直接删");
        assert!(
            needs_confirm(Mode::History, true),
            "固定项要问一句：删了就没了"
        );
        assert!(needs_confirm(Mode::Snippets, false), "片段要问一句");
        assert!(needs_confirm(Mode::Snippets, true));
    }

    #[test]
    fn missing_files_warning_stays_at_the_front() {
        // 窄弹窗里右端的省略号会吃掉行尾；失效标记必须排在最前面才看得见。
        let label = row_label(&label_row(ClipKind::File, 1));
        assert!(label.starts_with("［件·1 个失效］"), "实际是 {label}");
        assert_eq!(
            row_label(&label_row(ClipKind::File, 0)),
            "［件］1 个文件：a.txt"
        );
        assert_eq!(
            row_label(&label_row(ClipKind::Image, 0)),
            "［图］1 个文件：a.txt"
        );
        assert_eq!(row_label(&label_row(ClipKind::Text, 0)), "1 个文件：a.txt");
    }

    #[test]
    fn a_new_copy_does_not_move_the_popup_selection() {
        let _lock = focusflow_core::paths::test_app_dir_lock();
        let _dir = focusflow_core::paths::test_app_dir("clip_popup_keep_by_id");
        focusflow_core::clipboard::init_db().unwrap();
        let t = focusflow_core::clipboard::now_ms();
        for (i, text) in ["alpha", "beta", "gamma"].iter().enumerate() {
            focusflow_core::clipboard::ingest(text, "test.exe", t + i as i64).unwrap();
        }

        let rows = load_history("").unwrap();
        let sel = 1;
        let prev_id = rows[sel].id;
        let prev = Viewport { sel, scroll: 0 };

        // 新复制插到顶部：`reload_history` 里做的正是"重查 + 按 id 结算"这两步。
        focusflow_core::clipboard::ingest("delta", "test.exe", t + 9_000).unwrap();
        let mut fresh = load_history("").unwrap();
        inherit_missing(&rows, &mut fresh);
        let ids: Vec<i64> = fresh.iter().map(|r| r.id).collect();
        let vp = clip_list::settle(&ids, Some(prev_id), prev, true, 12, ReloadIntent::Keep);

        assert_eq!(
            fresh[vp.sel].id, prev_id,
            "列表顶部插进新条目后，弹窗的 Enter 必须还落在原来那条上"
        );
        assert_eq!(vp.scroll, 0);
    }

    #[test]
    fn missing_file_counts_survive_a_row_rebuild() {
        let mut old = vec![label_row(ClipKind::File, 2)];
        old[0].id = 7;
        let mut fresh = vec![label_row(ClipKind::File, 0)];
        fresh[0].id = 7;
        fresh[0].missing_files = None;
        inherit_missing(&old, &mut fresh);
        assert_eq!(
            fresh[0].missing_files,
            Some(2),
            "重查整份历史不该把可视文件的失效结果清掉（否则每次复制都要重新 stat）"
        );
    }

    /// 失活只有在"真的点走了"时才该关窗：菜单与确认框也是本窗的模态子窗。
    #[test]
    fn a_modal_of_our_own_is_not_clicking_away() {
        assert!(
            !should_hide_on_deactivate(false, false, false),
            "还没真正激活过时的失活不是点走（否则热键按下去只闪一下）"
        );
        assert!(
            should_hide_on_deactivate(true, false, false),
            "正常点走要把弹窗收起来"
        );
        assert!(
            !should_hide_on_deactivate(true, true, false),
            "右键菜单开着：菜单自己造成的失活不许关窗"
        );
        assert!(
            !should_hide_on_deactivate(true, false, true),
            "删除确认框开着：它造成的失活不许关窗"
        );
        assert!(!should_hide_on_deactivate(true, true, true));
    }
}
