//! 剪贴板管理页的状态、几何与纯函数。
//!
//! 与看板页同一分工：数字从 core 的 `clipboard` 要，这里只留"现在选了什么、
//! 列表滚到哪、哪几条被打勾"，以及画/命中共用的那份几何。画在 `main.rs` 的
//! `draw_clipboard_page` 里（渲染器与主题那些东西不为一个模块改成 public）。

use std::collections::HashSet;

use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;

use focusflow_core::clipboard::{self, ClipItem, ClipStats, Snippet};

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
    }
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
    pub status: Option<String>,
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
            status: None,
        }
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

    pub fn reload(&mut self) {
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
                if self.status.as_deref() == Some("读剪贴板库失败") {
                    self.status = None;
                }
            }
            Err(e) => {
                self.items.clear();
                self.status = Some(format!("读剪贴板库失败：{e}"));
            }
        }
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
    }

    pub fn toggle_multi(&mut self) {
        self.multi = !self.multi;
        if !self.multi {
            self.checked.clear();
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

    pub fn checked_count(&self) -> usize {
        self.checked.len()
    }

    pub fn clear_checked(&mut self) {
        self.checked.clear();
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
            .map(|i| Row {
                id: i as i64,
                content: format!("v{i}"),
                name: String::new(),
                source: String::new(),
                time: String::new(),
                pinned: false,
            })
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
        p.items = vec![
            Row {
                id: 1,
                content: "a".into(),
                name: String::new(),
                source: String::new(),
                time: String::new(),
                pinned: false,
            },
            Row {
                id: 2,
                content: "b".into(),
                name: String::new(),
                source: String::new(),
                time: String::new(),
                pinned: false,
            },
        ];
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
    fn preview_flattens_whitespace() {
        assert_eq!(preview_text(" a\n\n b\tc "), "a b c");
        assert_eq!(preview_text("   "), "");
    }
}
