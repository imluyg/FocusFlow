//! 剪贴板管理页的状态、几何与纯函数。
//!
//! 与看板页同一分工：数字从 core 的 `clipboard` 要，这里只留"现在选了什么、
//! 列表滚到哪、哪几条被打勾"，以及画/命中共用的那份几何。画在 `main.rs` 的
//! `draw_clipboard_page` 里（渲染器与主题那些东西不为一个模块改成 public）。

use std::collections::HashSet;

use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;

use focusflow_core::clipboard::{self, ClipItem, ClipKind, ClipStats, Snippet};

use crate::clip_list::{self, ReloadIntent, Viewport};

/// 列表一次最多取多少条。弹窗是 100；管理页给到 500，够翻旧记录，也不至于把
/// 一次切页变成整库扫描。
pub const LIST_LIMIT: i64 = 500;

const PAD: f32 = 16.0;
const TAB_Y: f32 = 204.0;
const TAB_W: f32 = 80.0;
const TAB_H: f32 = 26.0;
const TAB_PITCH: f32 = 84.0;
const SEARCH_W: f32 = 260.0;
const TOOL_Y: f32 = 242.0;
const TOOL_H: f32 = 28.0;
const TOOL_W: f32 = 76.0;
const TOOL_GAP: f32 = 8.0;
const LIST_TOP: f32 = 282.0;
const ROW_H: f32 = 40.0;
const FOOTER_H: f32 = 34.0;

/// 两块清单读失败时状态条那句话的开头。错误正文带着原因（`：{e}`），比对整串会让
/// "恢复后清掉这句话"永远不成立。
const HISTORY_LOAD_ERROR: &str = "读剪贴板库失败";
const SNIPPETS_LOAD_ERROR: &str = "读片段失败";

fn clear_load_error(status: &mut Option<String>, prefix: &str) {
    if status.as_deref().is_some_and(|s| s.starts_with(prefix)) {
        *status = None;
    }
}

fn box_rect(x: f32, y: f32, w: f32, h: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    History,
    Snippets,
    Stats,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::History => "历史",
            Mode::Snippets => "片段",
            Mode::Stats => "统计",
        }
    }
}

pub const MODES: [Mode; 3] = [Mode::History, Mode::Snippets, Mode::Stats];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: i64,
    pub content: String,
    /// 片段名；历史条目为空。
    pub name: String,
    pub source: String,
    pub time: String,
    pub pinned: bool,
    pub kind: ClipKind,
    pub image_path: Option<String>,
    pub thumb_path: Option<String>,
    pub file_paths: Vec<String>,
}

/// 行被点了一下时该做什么。抽成纯函数：`Shift` 与多选两个开关的组合各有一条用例。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowClick {
    /// 单选模式：把光标挪到这一行。
    Select(usize),
    /// 多选、普通点击：翻这一条的勾。
    Toggle(usize),
    /// 多选 + Shift：从锚点整段勾上。
    Range(usize),
}

pub fn row_click_action(multi: bool, shift: bool, row: usize) -> RowClick {
    if !multi {
        RowClick::Select(row)
    } else if shift {
        RowClick::Range(row)
    } else {
        RowClick::Toggle(row)
    }
}

/// 批量固定的计划。片段表没有 `pinned` 列，而 `action_ids` 交的是**当前模式**下的 id：
/// 拿着片段 id 去改历史表，改到的是同号那条历史记录（或干脆一条没改）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinPlan {
    /// 当前模式没有"固定"这个动作。
    WrongMode,
    /// 没有可作用的目标。
    Empty,
    Apply {
        ids: Vec<i64>,
        pinned: bool,
    },
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
        thumb_path: item.thumb_path,
        file_paths: item.file_paths,
    }
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
        thumb_path: None,
        file_paths: Vec::new(),
    }
}

/// 类型徽标（没有缩略图时画在行首）。文本条目不画，保持旧版面干净。
pub fn kind_badge(kind: ClipKind) -> &'static str {
    match kind {
        ClipKind::Text => "",
        ClipKind::Image => "图",
        ClipKind::File => "件",
    }
}

/// 行内那句话：文件条目把"有几个已经失效"写在后面。失效数由调用方只对**要画出来的**
/// 行现问（`ClipPage::missing_files_for`），不在建行时对 500 条记录逐条 stat。
pub fn row_preview(row: &Row, missing_files: usize) -> String {
    let text = preview_text(&row.content);
    if row.kind == ClipKind::File && missing_files > 0 {
        format!("{text}（{missing_files} 个已失效）")
    } else {
        text
    }
}

/// 已解码的缩略图像素。缓存只放 RGBA：位图属于渲染目标，每帧按需建、用完即放。
pub struct Thumb {
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
}

pub fn short_time(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.format("%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// 暂停还剩多久的短写法。`until_ms` 已经过去（或本来就 0）返回 `None`：页脚不该
/// 写「已暂停（剩 0 秒）」。向上取整到秒，剩 500ms 说「1 秒」而不是「0 秒」。
pub fn format_remaining(until_ms: i64, now_ms: i64) -> Option<String> {
    let left = until_ms.saturating_sub(now_ms);
    if left <= 0 {
        return None;
    }
    let secs = (left + 999) / 1000;
    Some(if secs >= 60 {
        format!("{}:{:02}", secs / 60, secs % 60)
    } else {
        format!("{secs} 秒")
    })
}

/// 行里只画一行：换行折成空格，超长交给调用侧的夹取。
pub fn preview_text(content: &str) -> String {
    content.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub struct ClipPage {
    pub mode: Mode,
    pub filter: String,
    pub items: Vec<Row>,
    pub snippets: Vec<Row>,
    pub stats: Option<ClipStats>,
    /// 当前光标所在行（相对**过滤后**的列表）。
    pub sel: usize,
    pub scroll: usize,
    pub multi: bool,
    /// 多选模式下勾住的条目 id。
    pub checked: HashSet<i64>,
    /// 上一次点过的行：`Shift+点击` 从这里开始整段勾选。
    anchor: Option<usize>,
    pub status: Option<String>,
    /// 缩略图解码缓存。解一份 PNG 几百微秒，但一帧要画十来行、拖滚动条时每帧都画；
    /// 缓存 64 份、按先来先出，内存上界约 64 × 160×160×4 ≈ 6 MB。
    thumbs: std::cell::RefCell<std::collections::HashMap<String, Option<std::rc::Rc<Thumb>>>>,
    thumb_order: std::cell::RefCell<std::collections::VecDeque<String>>,
    /// 文件失效数缓存（条目 id → 失效路径数）。`Path::exists` 是磁盘 IO：一次列表重查
    /// 就对最多 500 条文件记录逐条 stat，会让实时筛选的每一键都卡一下 —— 只问**要画出来的**
    /// 那几行（与弹窗同一条口径）。
    missing: std::cell::RefCell<std::collections::HashMap<i64, usize>>,
}

impl Default for ClipPage {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipPage {
    pub fn new() -> Self {
        Self {
            mode: Mode::History,
            filter: String::new(),
            items: Vec::new(),
            snippets: Vec::new(),
            stats: None,
            sel: 0,
            scroll: 0,
            multi: false,
            checked: HashSet::new(),
            anchor: None,
            status: None,
            thumbs: std::cell::RefCell::new(std::collections::HashMap::new()),
            thumb_order: std::cell::RefCell::new(std::collections::VecDeque::new()),
            missing: std::cell::RefCell::new(std::collections::HashMap::new()),
        }
    }

    /// 取一份已解码的缩略图。读不出来就记一个 `None`，同一张坏图不会每帧都去读盘。
    pub fn thumb(&self, path: &str) -> Option<std::rc::Rc<Thumb>> {
        if let Some(hit) = self.thumbs.borrow().get(path) {
            return hit.clone();
        }
        let decoded = std::fs::read(path)
            .ok()
            .and_then(|bytes| clipboard::decode_png_rgba_loose(&bytes).ok())
            .map(|(rgba, w, h)| std::rc::Rc::new(Thumb { rgba, w, h }));
        self.thumbs
            .borrow_mut()
            .insert(path.to_string(), decoded.clone());
        let mut order = self.thumb_order.borrow_mut();
        if !order.iter().any(|p| p == path) {
            order.push_back(path.to_string());
        }
        while order.len() > 64 {
            if let Some(old) = order.pop_front() {
                self.thumbs.borrow_mut().remove(&old);
            }
        }
        decoded
    }

    pub fn rows(&self) -> &[Row] {
        match self.mode {
            Mode::History => &self.items,
            Mode::Snippets => &self.snippets,
            Mode::Stats => &[],
        }
    }

    /// 这一行的失效路径数；`true` = 这次真的问了磁盘（调用方据此只记一次日志）。
    ///
    /// 只对将要画出来的行调用：500 条历史 × 每条最多 20 个路径，一次筛选上万次 stat
    /// 会把实时搜索卡住。
    pub fn missing_files_for(&self, row: &Row) -> (usize, bool) {
        if row.kind != ClipKind::File {
            return (0, false);
        }
        if let Some(n) = self.missing.borrow().get(&row.id).copied() {
            return (n, false);
        }
        let n = clipboard::split_alive_paths(&row.file_paths).1.len();
        self.missing.borrow_mut().insert(row.id, n);
        (n, true)
    }

    pub fn selected(&self) -> Option<&Row> {
        self.rows().get(self.sel)
    }

    /// 只重查历史列表。实时筛选每敲一个键都要走这里：片段与统计不该跟着重算。
    pub fn reload_items(&mut self) {
        // 库读不出来与"没有记录"是两件事：前者写进 status，列表留空但会有一句话。
        let filter = self.filter.trim().to_string();
        let loaded = if filter.is_empty() {
            clipboard::list_recent(LIST_LIMIT)
        } else {
            clipboard::search(&filter, LIST_LIMIT)
        };
        match loaded {
            Ok(items) => {
                self.items = items.into_iter().map(row_from_item).collect();
                clear_load_error(&mut self.status, HISTORY_LOAD_ERROR);
            }
            Err(e) => {
                self.items.clear();
                self.status = Some(format!("{HISTORY_LOAD_ERROR}：{e}"));
            }
        }
    }

    /// 重查整页。`intent` 决定选中/视口怎么落（见 `clip_list::settle`）：
    /// 新复制、固定、删除用 `Keep`，进页/换模式/换筛选用 `ResetTop`。
    pub fn reload(&mut self, intent: ReloadIntent, visible: usize) {
        let prev_id = self.selected().map(|r| r.id);
        let prev = Viewport {
            sel: self.sel,
            scroll: self.scroll,
        };
        let was_at_top = self.scroll == 0;
        let filter = self.filter.trim().to_string();
        self.reload_items();
        let mut snippets: Vec<Row> = match clipboard::snippets() {
            Ok(v) => {
                clear_load_error(&mut self.status, SNIPPETS_LOAD_ERROR);
                v.into_iter().map(row_from_snippet).collect()
            }
            Err(e) => {
                if self.status.is_none() {
                    self.status = Some(format!("{SNIPPETS_LOAD_ERROR}：{e}"));
                }
                Vec::new()
            }
        };
        if !filter.is_empty() {
            let f = filter.to_ascii_lowercase();
            snippets.retain(|r| {
                r.name.to_ascii_lowercase().contains(&f)
                    || r.content.to_ascii_lowercase().contains(&f)
            });
        }
        self.snippets = snippets;
        self.stats = clipboard::stats(7).ok();
        // 下标已经换了含义：旧的 Shift 锚点不能再拿去整段勾。
        self.anchor = None;
        let ids: Vec<i64> = self.rows().iter().map(|r| r.id).collect();
        let vp = clip_list::settle(&ids, prev_id, prev, was_at_top, visible, intent);
        self.sel = vp.sel;
        self.scroll = vp.scroll;
    }

    /// 盘上可能变了的时刻（进页、换模式）才丢失效缓存。`reload` 不丢：新复制进来时
    /// 其余行的文件失效状态不会因此改变，重 stat 一遍只会让每次复制都卡一下。
    pub fn reset_probes(&mut self) {
        self.missing.borrow_mut().clear();
    }

    /// 把当前视口里那几行的失效检查补上（`Path::exists` 只问要画出来的行）。
    /// 返回这次真的问了磁盘的行数 —— 调用方据此在重画之前记一条日志。
    pub fn warm_missing(&self, visible: usize) -> usize {
        let mut fresh = 0;
        for row in self.rows().iter().skip(self.scroll).take(visible) {
            if self.missing_files_for(row).1 {
                fresh += 1;
            }
        }
        fresh
    }

    pub fn set_mode(&mut self, mode: Mode) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.sel = 0;
        self.scroll = 0;
        self.checked.clear();
        self.anchor = None;
        self.multi = false;
        self.status = None;
        self.reset_probes();
        self.clamp_selection(1);
    }

    pub fn clamp_selection(&mut self, visible_rows: usize) {
        let len = self.rows().len();
        if len == 0 {
            self.sel = 0;
            self.scroll = 0;
            return;
        }
        self.sel = self.sel.min(len - 1);
        let visible = visible_rows.max(1);
        if self.sel < self.scroll {
            self.scroll = self.sel;
        } else if self.sel >= self.scroll + visible {
            self.scroll = self.sel + 1 - visible;
        }
        self.scroll = self.scroll.min(len.saturating_sub(visible));
    }

    pub fn move_selection(&mut self, delta: i32, visible_rows: usize) {
        let len = self.rows().len();
        if len == 0 {
            self.sel = 0;
            self.scroll = 0;
            return;
        }
        self.sel = (self.sel as i32 + delta).clamp(0, len as i32 - 1) as usize;
        self.clamp_selection(visible_rows);
    }

    pub fn scroll_by(&mut self, notches: i32, visible_rows: usize) {
        let len = self.rows().len();
        if len == 0 {
            self.scroll = 0;
            return;
        }
        let max = len.saturating_sub(visible_rows.max(1));
        let delta = if notches > 0 { -2 } else { 2 };
        self.scroll = (self.scroll as i32 + delta).clamp(0, max as i32) as usize;
        // 滚轮之后光标仍在可见范围里：滚出屏幕的选中行会让 Enter 复制到看不见的东西。
        self.sel = self.sel.clamp(
            self.scroll,
            (self.scroll + visible_rows.max(1) - 1).min(len - 1),
        );
    }

    /// 翻一行的勾。多选态里光靠 ✓ 认不出"现在键盘在谁身上"（↑↓ 只挪焦点），
    /// 所以焦点行也跟着这次点击走。
    pub fn toggle_checked(&mut self, row_idx: usize) {
        let Some(row) = self.rows().get(row_idx) else {
            return;
        };
        let id = row.id;
        if !self.checked.remove(&id) {
            self.checked.insert(id);
        }
        self.anchor = Some(row_idx);
        self.sel = row_idx;
    }

    /// `Shift+点击`：从锚点到这一行整段勾上（只加不减，避免"点一下没了一半"）。
    /// 没有锚点（刚进多选、或列表刚换过）时就等同于普通点一下。
    pub fn check_range(&mut self, row_idx: usize) {
        let ids: Vec<i64> = self.rows().iter().map(|r| r.id).collect();
        let Some(from) = self.anchor.filter(|a| *a < ids.len()) else {
            self.toggle_checked(row_idx);
            return;
        };
        if row_idx >= ids.len() {
            return;
        }
        self.multi = true;
        let (a, b) = if from <= row_idx {
            (from, row_idx)
        } else {
            (row_idx, from)
        };
        for id in &ids[a..=b] {
            self.checked.insert(*id);
        }
        self.anchor = Some(row_idx);
        self.sel = row_idx;
    }

    /// `Ctrl+A`：把当前列表（历史或片段，按现在的筛选）全部勾上，并进入多选。
    pub fn check_all(&mut self) {
        let ids: Vec<i64> = self.rows().iter().map(|r| r.id).collect();
        if ids.is_empty() {
            return;
        }
        self.multi = true;
        for id in ids {
            self.checked.insert(id);
        }
        self.anchor = Some(self.rows().len() - 1);
    }

    pub fn toggle_multi(&mut self) {
        if self.multi {
            self.exit_multi();
        } else {
            self.multi = true;
        }
    }

    /// 退出多选：勾选与 Shift 锚点一起清掉。`sel`（焦点行）留着 —— 用户只是不再
    /// 批量操作，不是把光标挪走。
    pub fn exit_multi(&mut self) {
        self.multi = false;
        self.checked.clear();
        self.anchor = None;
    }

    /// 筛选变了：勾住的 id 可能已经不在列表里。继续留着它们，删除/固定会作用到
    /// 屏幕上看不见的行，所以退多选、清勾选、回顶。
    pub fn on_filter_changed(&mut self) {
        self.exit_multi();
        self.sel = 0;
        self.scroll = 0;
    }

    /// 这次动作作用在哪些 id 上：多选且有勾 ⇒ 勾住的；否则当前行。
    pub fn action_ids(&self) -> Vec<i64> {
        if self.multi && !self.checked.is_empty() {
            let mut ids: Vec<i64> = self.checked.iter().copied().collect();
            ids.sort_unstable();
            ids
        } else {
            self.selected().map(|r| vec![r.id]).unwrap_or_default()
        }
    }

    /// `Ctrl+P` / 「固定」按钮的目标状态。
    ///
    /// 一条没固定就整批固定；只有全都固定了才整批取消 —— 混选时按第一行的状态取反，
    /// 会把已经固定的那些悄悄取消掉。
    pub fn pin_plan(&self) -> PinPlan {
        if self.mode != Mode::History {
            return PinPlan::WrongMode;
        }
        let ids = self.action_ids();
        if ids.is_empty() {
            return PinPlan::Empty;
        }
        let rows = self.rows();
        // 勾住的 id 可能来自上一轮筛选、这一轮不在 `rows` 里：拿不准就当"没固定"，
        // 往**保护**的方向走 —— 多固定一次只是多一层保险，误取消会把用户显式
        // 保护的内容暴露给裁剪。
        let pinned = ids
            .iter()
            .any(|id| rows.iter().find(|r| r.id == *id).is_none_or(|r| !r.pinned));
        PinPlan::Apply { ids, pinned }
    }

    pub fn checked_count(&self) -> usize {
        self.checked.len()
    }

    pub fn clear_checked(&mut self) {
        self.checked.clear();
        self.anchor = None;
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Layout {
    pub tabs: [D2D_RECT_F; 3],
    pub search: D2D_RECT_F,
    pub toolbar: [D2D_RECT_F; 6],
    pub list_top: f32,
    pub row_h: f32,
    pub rows: usize,
    pub list_bottom: f32,
    pub footer_y: f32,
}

impl Layout {
    pub fn pad(&self) -> f32 {
        PAD
    }
}

pub fn layout_for(w: f32, h: f32) -> Layout {
    let mut tabs = [box_rect(0.0, 0.0, 0.0, 0.0); 3];
    for (i, slot) in tabs.iter_mut().enumerate() {
        *slot = box_rect(PAD + i as f32 * TAB_PITCH, TAB_Y, TAB_W, TAB_H);
    }
    let search = box_rect((w - PAD - SEARCH_W).max(PAD), TAB_Y, SEARCH_W, TAB_H);
    let mut toolbar = [box_rect(0.0, 0.0, 0.0, 0.0); 6];
    for (i, slot) in toolbar.iter_mut().enumerate() {
        *slot = box_rect(PAD + i as f32 * (TOOL_W + TOOL_GAP), TOOL_Y, TOOL_W, TOOL_H);
    }
    let footer_y = (h - FOOTER_H).max(LIST_TOP + ROW_H);
    let list_bottom = footer_y - 10.0;
    let rows = (((list_bottom - LIST_TOP) / ROW_H).floor().max(1.0)) as usize;
    Layout {
        tabs,
        search,
        toolbar,
        list_top: LIST_TOP,
        row_h: ROW_H,
        rows,
        list_bottom,
        footer_y,
    }
}

/// 空状态/统计读不出来那一行的矩形。贴着列表卡片的内容左边距（`pad + 8`）画，
/// 不按 `list_top` 起——那是卡片内的纵向起点，横向会落到卡片外面去。
pub fn empty_text_rect(lay: &Layout, w: f32) -> D2D_RECT_F {
    box_rect(
        lay.pad() + 8.0,
        lay.list_top + 16.0,
        (w - lay.pad() * 2.0 - 16.0).max(20.0),
        24.0,
    )
}

pub fn tab_at(lay: &Layout, x: f32, y: f32) -> Option<usize> {
    lay.tabs
        .iter()
        .position(|r| x >= r.left && x <= r.right && y >= r.top && y <= r.bottom)
}

pub fn toolbar_at(lay: &Layout, x: f32, y: f32) -> Option<usize> {
    lay.toolbar
        .iter()
        .position(|r| x >= r.left && x <= r.right && y >= r.top && y <= r.bottom)
}

pub fn row_at(lay: &Layout, scroll: usize, len: usize, x: f32, y: f32) -> Option<usize> {
    if x < PAD || y < lay.list_top || y >= lay.list_bottom {
        return None;
    }
    let vis = ((y - lay.list_top) / lay.row_h) as usize;
    if vis >= lay.rows {
        return None;
    }
    let idx = scroll + vis;
    (idx < len).then_some(idx)
}

pub fn search_at(lay: &Layout, x: f32, y: f32) -> bool {
    x >= lay.search.left && x <= lay.search.right && y >= lay.search.top && y <= lay.search.bottom
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_row(id: i64, content: &str) -> Row {
        Row {
            id,
            content: content.to_string(),
            name: String::new(),
            source: String::new(),
            time: String::new(),
            pinned: false,
            kind: ClipKind::Text,
            image_path: None,
            thumb_path: None,
            file_paths: Vec::new(),
        }
    }

    #[test]
    fn kind_labels_and_missing_file_note() {
        let mut row = test_row(1, "2 个文件：a.txt, b.txt");
        row.kind = ClipKind::File;
        assert_eq!(row_preview(&row, 1), "2 个文件：a.txt, b.txt（1 个已失效）");
        assert_eq!(row_preview(&row, 0), "2 个文件：a.txt, b.txt");
        assert_eq!(kind_badge(ClipKind::Image), "图");
        assert_eq!(kind_badge(ClipKind::File), "件");
        assert_eq!(kind_badge(ClipKind::Text), "");
    }

    #[test]
    fn missing_file_check_is_lazy_and_cached_until_reload() {
        let _lock = focusflow_core::paths::test_app_dir_lock();
        let dir = focusflow_core::paths::test_app_dir("clip_page_missing_lazy");
        focusflow_core::clipboard::init_db().unwrap();
        let file = dir.path().join("gone.txt");
        std::fs::write(&file, b"x").unwrap();
        focusflow_core::clipboard::ingest_files(
            &[file.to_string_lossy().to_string()],
            "explorer.exe",
            focusflow_core::clipboard::now_ms(),
        )
        .unwrap();
        std::fs::remove_file(&file).unwrap();

        let mut page = ClipPage::new();
        page.reload(ReloadIntent::ResetTop, 8);
        let row = page
            .items
            .iter()
            .find(|r| r.kind == ClipKind::File)
            .cloned()
            .expect("文件条目应当读回来");

        assert_eq!(
            page.missing_files_for(&row),
            (1, true),
            "第一次问要真的看盘：文件已经删了"
        );
        assert_eq!(
            page.missing_files_for(&row),
            (1, false),
            "同一行第二次问走缓存，不再碰磁盘"
        );
        assert_eq!(
            page.missing_files_for(&test_row(9, "文本")),
            (0, false),
            "文本行不做磁盘检查"
        );

        page.reload(ReloadIntent::Keep, 8);
        assert_eq!(
            page.missing_files_for(&row),
            (1, false),
            "新复制进来自动 refresh 时不该把可视行的失效检查全部重 stat 一遍"
        );

        page.reset_probes();
        let row = page
            .items
            .iter()
            .find(|r| r.kind == ClipKind::File)
            .cloned()
            .unwrap();
        assert_eq!(
            page.missing_files_for(&row),
            (1, true),
            "进页/换模式（reset_probes）才是'盘上可能变了'的时刻，要重新问一次"
        );
    }

    #[test]
    fn layout_keeps_tabs_search_toolbar_and_rows_apart() {
        let lay = layout_for(1200.0, 860.0);
        assert!(lay.tabs[2].right < lay.search.left, "页签压住搜索框");
        assert!(lay.search.top == lay.tabs[0].top);
        for i in 1..6 {
            assert!(
                lay.toolbar[i].left >= lay.toolbar[i - 1].right,
                "工具钮互相重叠"
            );
        }
        assert!(lay.toolbar[0].bottom < lay.list_top, "工具钮压住列表");
        assert!(lay.rows >= 8, "常见窗口下至少八行");
        assert!(lay.list_bottom > lay.list_top);
    }

    #[test]
    fn layout_survives_the_minimum_window() {
        let lay = layout_for(960.0, 640.0);
        assert!(lay.rows >= 4, "最小窗口也要有几行可看");
        assert!(lay.search.right <= 960.0 - PAD + 0.1);
        assert!(lay.footer_y > lay.list_top);
    }

    #[test]
    fn row_hit_maps_visible_rows_to_the_filtered_list() {
        let lay = layout_for(1200.0, 860.0);
        let first = lay.list_top + 4.0;
        assert_eq!(row_at(&lay, 0, 10, PAD + 10.0, first), Some(0));
        assert_eq!(
            row_at(&lay, 3, 10, PAD + 10.0, lay.list_top + lay.row_h + 4.0),
            Some(4)
        );
        assert_eq!(row_at(&lay, 0, 0, PAD + 10.0, first), None);
        assert_eq!(row_at(&lay, 0, 10, PAD + 10.0, lay.list_bottom + 1.0), None);
    }

    #[test]
    fn selection_clamps_and_scrolls_into_view() {
        let mut p = ClipPage::new();
        p.items = (0..10)
            .map(|i| test_row(i as i64, &format!("v{i}")))
            .collect();
        p.sel = 9;
        p.clamp_selection(5);
        assert_eq!(p.sel, 9);
        assert_eq!(p.scroll, 5, "最后一行要滚进五行的视口");

        p.sel = 0;
        p.clamp_selection(5);
        assert_eq!(p.scroll, 0, "回到第一行要把滚动也带回去");

        p.move_selection(-1, 5);
        assert_eq!(p.sel, 0, "第一行再上一格仍然停在第一行");
        p.move_selection(99, 5);
        assert_eq!(p.sel, 9);
    }

    #[test]
    fn action_ids_prefer_checked_rows_in_multi_mode() {
        let mut p = ClipPage::new();
        p.items = vec![test_row(1, "a"), test_row(2, "b")];
        p.sel = 1;
        assert_eq!(p.action_ids(), vec![2]);
        p.multi = true;
        p.checked.insert(1);
        p.checked.insert(2);
        assert_eq!(p.action_ids(), vec![1, 2], "多选时按 id 排好交回");
        p.multi = false;
        assert_eq!(p.action_ids(), vec![2], "关掉多选后回到当前行");
    }

    #[test]
    fn shift_range_and_select_all_fill_the_checked_set() {
        let mut p = ClipPage::new();
        p.items = (1..=6).map(|i| test_row(i, &format!("v{i}"))).collect();

        p.check_all();
        assert!(p.multi, "Ctrl+A 要顺带进入多选");
        assert_eq!(p.action_ids(), vec![1, 2, 3, 4, 5, 6]);

        p.toggle_multi();
        p.toggle_multi();
        assert!(p.checked.is_empty(), "关掉多选要清空勾选");

        // 点第 1 行（id 2）立锚，再 Shift 点第 4 行（id 5）：2..=5 整段。
        p.toggle_checked(1);
        p.check_range(4);
        assert_eq!(p.action_ids(), vec![2, 3, 4, 5]);

        // 反向 Shift：锚点移到第 0 行，1..=5 都该在。
        p.check_range(0);
        assert_eq!(p.action_ids(), vec![1, 2, 3, 4, 5]);

        // 没有锚点时等同普通点一下（不会把整段都勾上）。
        p.clear_checked();
        p.toggle_multi();
        p.toggle_multi();
        p.check_range(3);
        assert_eq!(p.action_ids(), vec![4]);
    }

    #[test]
    fn row_click_action_covers_the_three_gestures() {
        assert_eq!(row_click_action(false, false, 2), RowClick::Select(2));
        assert_eq!(
            row_click_action(false, true, 2),
            RowClick::Select(2),
            "单选下 Shift 不改语义"
        );
        assert_eq!(row_click_action(true, false, 2), RowClick::Toggle(2));
        assert_eq!(row_click_action(true, true, 2), RowClick::Range(2));
    }

    #[test]
    fn switching_mode_clears_multi_selection() {
        let mut p = ClipPage::new();
        p.multi = true;
        p.checked.insert(7);
        p.sel = 3;
        p.set_mode(Mode::Snippets);
        assert_eq!(p.mode, Mode::Snippets);
        assert!(!p.multi);
        assert!(p.checked.is_empty());
        assert_eq!(p.sel, 0);
    }

    #[test]
    fn pin_plan_refuses_snippet_mode_and_fixes_mixed_state() {
        let mut p = ClipPage::new();
        p.items = vec![test_row(1, "a"), test_row(2, "b")];
        p.items[0].pinned = true;
        p.multi = true;
        p.checked.insert(1);
        p.checked.insert(2);
        assert_eq!(
            p.pin_plan(),
            PinPlan::Apply {
                ids: vec![1, 2],
                pinned: true
            },
            "混选里还有一条没固定 ⇒ 整批固定；不能按第一行的状态取反"
        );

        p.items[1].pinned = true;
        assert_eq!(
            p.pin_plan(),
            PinPlan::Apply {
                ids: vec![1, 2],
                pinned: false
            },
            "全都固定了才整批取消"
        );

        p.set_mode(Mode::Snippets);
        p.snippets = vec![test_row(1, "snippet")];
        assert_eq!(
            p.pin_plan(),
            PinPlan::WrongMode,
            "片段 id 不能拿去改历史表（同号会改到别的记录）"
        );

        p.set_mode(Mode::History);
        p.items.clear();
        assert_eq!(p.pin_plan(), PinPlan::Empty);
    }

    #[test]
    fn pin_plan_treats_unknown_checked_ids_as_unpinned() {
        let mut p = ClipPage::new();
        p.items = vec![test_row(1, "only")];
        p.items[0].pinned = true;
        p.multi = true;
        p.checked.insert(1);
        p.checked.insert(99); // 上一轮筛选勾住、这一轮已经不在列表里
        assert_eq!(
            p.pin_plan(),
            PinPlan::Apply {
                ids: vec![1, 99],
                pinned: true
            },
            "状态拿不准时往保护方向走：误固定只是多一层保险，误取消会暴露给裁剪"
        );
    }

    #[test]
    fn snippet_load_error_also_clears_after_a_successful_reload() {
        let _lock = focusflow_core::paths::test_app_dir_lock();
        let _dir = focusflow_core::paths::test_app_dir("clip_page_snip_status");
        focusflow_core::clipboard::init_db().unwrap();
        let mut p = ClipPage::new();
        p.status = Some(format!("{SNIPPETS_LOAD_ERROR}：disk I/O error"));
        p.reload(ReloadIntent::ResetTop, 8);
        assert!(
            p.status.is_none(),
            "片段读回来之后，上一次那句错不该继续留在状态条上（实际：{:?}）",
            p.status
        );
    }

    #[test]
    fn history_error_status_clears_after_a_successful_reload() {
        let _lock = focusflow_core::paths::test_app_dir_lock();
        let _dir = focusflow_core::paths::test_app_dir("clip_page_status");
        focusflow_core::clipboard::init_db().unwrap();
        let mut p = ClipPage::new();
        p.status = Some(format!("{HISTORY_LOAD_ERROR}：database is locked"));
        p.reload_items();
        assert!(
            p.status.is_none(),
            "读失败恢复之后，上一次那句错不该继续留在状态条上（实际：{:?}）",
            p.status
        );
    }

    #[test]
    fn preview_flattens_whitespace() {
        assert_eq!(preview_text(" a\n\n b\tc "), "a b c");
        assert_eq!(preview_text("   "), "");
    }

    #[test]
    fn a_new_copy_at_the_top_keeps_the_cursor_on_the_same_row() {
        let _lock = focusflow_core::paths::test_app_dir_lock();
        let _dir = focusflow_core::paths::test_app_dir("clip_page_keep_by_id");
        focusflow_core::clipboard::init_db().unwrap();
        let t = focusflow_core::clipboard::now_ms();
        for (i, text) in ["first", "second", "third"].iter().enumerate() {
            focusflow_core::clipboard::ingest(text, "test.exe", t + i as i64).unwrap();
        }

        let mut page = ClipPage::new();
        page.reload(ReloadIntent::ResetTop, 8);
        assert_eq!(page.items[0].content, "third", "最新的在最上面");
        page.sel = 1;
        let before = page.selected().unwrap().content.clone();
        assert_eq!(before, "second");

        focusflow_core::clipboard::ingest("fourth", "test.exe", t + 9_000).unwrap();
        page.reload(ReloadIntent::Keep, 8);

        assert_eq!(
            page.selected().map(|r| r.content.clone()).as_deref(),
            Some(before.as_str()),
            "新复制插到顶部后下标整体后移，光标要跟着 id 走：Enter 不能粘到别的条目"
        );
        assert_eq!(page.scroll, 0, "本来在顶部：新条目应当直接可见");
    }

    #[test]
    fn toggling_a_row_moves_the_focus_row_too() {
        let mut p = ClipPage::new();
        p.items = vec![test_row(1, "a"), test_row(2, "b"), test_row(3, "c")];
        p.multi = true;
        p.toggle_checked(2);
        assert_eq!(p.sel, 2, "多选里点一行，焦点行也要跟过去");
        assert_eq!(p.action_ids(), vec![3], "删除/固定只认勾选");
        p.toggle_checked(2);
        assert!(p.checked.is_empty());
        assert_eq!(p.sel, 2, "取消勾选不挪焦点行");
    }

    #[test]
    fn exiting_multi_clears_the_checked_set_but_keeps_the_focus_row() {
        let mut p = ClipPage::new();
        p.items = vec![test_row(1, "a"), test_row(2, "b")];
        p.multi = true;
        p.checked.insert(1);
        p.checked.insert(2);
        p.anchor = Some(1);
        p.sel = 1;
        p.exit_multi();
        assert!(!p.multi);
        assert!(p.checked.is_empty(), "退出多选不能留下勾选");
        assert_eq!(p.anchor, None);
        assert_eq!(p.sel, 1, "焦点行不是批量目标，退出多选不该把它挪走");
    }

    #[test]
    fn a_filter_change_drops_multi_selection_and_goes_back_to_the_top() {
        let mut p = ClipPage::new();
        p.items = vec![test_row(1, "a"), test_row(2, "b")];
        p.multi = true;
        p.checked.insert(1);
        p.checked.insert(2);
        p.anchor = Some(1);
        p.sel = 1;
        p.scroll = 1;
        p.on_filter_changed();
        assert!(!p.multi, "筛选变了就退多选");
        assert!(p.checked.is_empty(), "屏幕外的勾选不能留到下一次删除");
        assert_eq!(p.anchor, None);
        assert_eq!((p.sel, p.scroll), (0, 0), "换了一批数据要回顶");
    }

    #[test]
    fn remaining_pause_time_reads_as_minutes_and_seconds() {
        assert_eq!(format_remaining(0, 1_000), None, "没暂停就没有剩余时间");
        assert_eq!(format_remaining(1_000, 1_000), None, "刚好到点不显示");
        assert_eq!(
            format_remaining(500, 0).as_deref(),
            Some("1 秒"),
            "剩 500ms 说 1 秒，不说 0 秒"
        );
        assert_eq!(format_remaining(59_000, 0).as_deref(), Some("59 秒"));
        assert_eq!(format_remaining(60_000, 0).as_deref(), Some("1:00"));
        assert_eq!(format_remaining(3_600_000, 0).as_deref(), Some("60:00"));
    }

    #[test]
    fn the_empty_state_text_sits_inside_the_card() {
        let lay = layout_for(1200.0, 860.0);
        let r = empty_text_rect(&lay, 1200.0);
        assert_eq!(r.left, lay.pad() + 8.0, "和列表内容同一个左边距");
        assert!(
            r.right <= 1200.0 - lay.pad() - 8.0 + 0.01,
            "右侧要留在卡片里（实际 {:.1}）",
            r.right
        );
        assert!(r.top > lay.list_top, "在卡片顶部之下");
    }
}
