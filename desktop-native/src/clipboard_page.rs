//! 剪贴板管理页的状态、几何与纯函数。
//!
//! 与看板页同一分工：数字从 core 的 `clipboard` 要，这里只留"现在选了什么、
//! 列表滚到哪、哪几条被打勾"，以及画/命中共用的那份几何。画在 `main.rs` 的
//! `draw_clipboard_page` 里（渲染器与主题那些东西不为一个模块改成 public）。

use std::collections::HashSet;

use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;

use focusflow_core::clipboard::{self, ClipItem, ClipKind, ClipStats, Snippet};

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

/// 历史列表读失败时状态条那句话的开头。错误正文带着原因（`：{e}`），比对整串会让
/// "恢复后清掉这句话"永远不成立。
const HISTORY_LOAD_ERROR: &str = "读剪贴板库失败";

fn is_history_load_error(status: Option<&str>) -> bool {
    status.is_some_and(|s| s.starts_with(HISTORY_LOAD_ERROR))
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
    /// 文件条目里已经不在磁盘上的路径数（显示时明说，别让人以为还能粘）。
    pub missing_files: usize,
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
    let missing_files = if item.kind == ClipKind::File {
        clipboard::split_alive_paths(&item.file_paths).1.len()
    } else {
        0
    };
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
        missing_files,
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
        missing_files: 0,
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

/// 行内那句话：文件条目把"有几个已经失效"写在后面。
pub fn row_preview(row: &Row) -> String {
    let text = preview_text(&row.content);
    if row.kind == ClipKind::File && row.missing_files > 0 {
        format!("{text}（{} 个已失效）", row.missing_files)
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
                // 失效文件的条数进日志（验收脚本靠它判"界面上确实标出来了"；
                // 屏上那句话由 `row_preview` 拼，同源）。
                let missing = self.items.iter().filter(|r| r.missing_files > 0).count();
                if missing > 0 {
                    crate::tracing_note(&format!("clip_page_missing_files n={missing}"));
                }
                if is_history_load_error(self.status.as_deref()) {
                    self.status = None;
                }
            }
            Err(e) => {
                self.items.clear();
                self.status = Some(format!("{HISTORY_LOAD_ERROR}：{e}"));
            }
        }
    }

    pub fn reload(&mut self) {
        let filter = self.filter.trim().to_string();
        self.reload_items();
        let mut snippets: Vec<Row> = match clipboard::snippets() {
            Ok(v) => v.into_iter().map(row_from_snippet).collect(),
            Err(e) => {
                if self.status.is_none() {
                    self.status = Some(format!("读片段失败：{e}"));
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
        self.clamp_selection(1);
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

    pub fn toggle_checked(&mut self, row_idx: usize) {
        let Some(row) = self.rows().get(row_idx) else {
            return;
        };
        let id = row.id;
        if !self.checked.remove(&id) {
            self.checked.insert(id);
        }
        self.anchor = Some(row_idx);
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
        self.multi = !self.multi;
        if !self.multi {
            self.checked.clear();
            self.anchor = None;
        }
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
        let pinned = ids
            .iter()
            .any(|id| rows.iter().find(|r| r.id == *id).is_some_and(|r| !r.pinned));
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
            missing_files: 0,
        }
    }

    #[test]
    fn kind_labels_and_missing_file_note() {
        let mut row = test_row(1, "2 个文件：a.txt, b.txt");
        row.kind = ClipKind::File;
        row.missing_files = 1;
        assert_eq!(row_preview(&row), "2 个文件：a.txt, b.txt（1 个已失效）");
        row.missing_files = 0;
        assert_eq!(row_preview(&row), "2 个文件：a.txt, b.txt");
        assert_eq!(kind_badge(ClipKind::Image), "图");
        assert_eq!(kind_badge(ClipKind::File), "件");
        assert_eq!(kind_badge(ClipKind::Text), "");
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
}
