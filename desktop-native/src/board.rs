//! 看板落地页：四格 KPI、一句结论、两列 Top 3。
//!
//! 数字全部来自当前周期的 [`Snapshot`] —— 与键鼠/应用/设备排行读的是同一份，
//! 时间口径就是顶部那一排全局周期药丸。这里只做格式化，不碰库、不另开查询：
//! 从前这一页自带一套「今日/本周/本月」和三张图，和排行页讲同一批数却换了一套口径，
//! 屏上最突兀的就是那个"切页时同一排控件换了一套"。

use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;

use focusflow_core::format::{fmt_compact_duration, fmt_thousands};

use crate::data::Snapshot;
use crate::{content_top, V_BOARD};

/// 每列取前几名。整页只留这一档摘要，完整排行在对应页签里。
pub const TOP_N: usize = 3;

/// 已经念成人话的一页内容；页面只负责排版。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BoardView {
    pub kpis: [(&'static str, String); 4],
    pub sentence: String,
    /// 非空 = 这一轮没读全，页面要在结论下面点名。
    pub gaps: Option<String>,
    /// (名称, 已格式化的值)
    pub top_apps: Vec<(String, String)>,
    pub top_keys: Vec<(String, String)>,
}

pub fn board_view(snap: &Snapshot, period_label: &str) -> BoardView {
    let keys = snap.period_total.max(0);
    let active = snap.app_total.max(0);
    let streak = match snap.goal.as_ref() {
        Some(g) => format!("{} 天", g.streak),
        // 读不出来不许印成 0：那会变成"一天都没连过"。
        None => "读不出".to_string(),
    };
    let sentence = match snap.apps.first() {
        Some((name, _)) => format!(
            "{}共按键 {} 次，活跃 {}，最常用 {}",
            period_label,
            fmt_thousands(keys),
            fmt_compact_duration(active),
            name
        ),
        None => format!(
            "{}共按键 {} 次，活跃 {}，这段时间没有归到某个应用上的活动",
            period_label,
            fmt_thousands(keys),
            fmt_compact_duration(active)
        ),
    };
    let top_apps = snap
        .apps
        .iter()
        .take(TOP_N)
        .map(|(name, secs)| (name.clone(), fmt_compact_duration(*secs)))
        .collect();
    let top_keys = snap
        .rank
        .iter()
        .take(TOP_N)
        .map(|(name, count)| (name.clone(), fmt_thousands((*count).max(0))))
        .collect();
    let gaps = if snap.gaps.is_empty() {
        None
    } else {
        Some(snap.gaps.join("；"))
    };
    BoardView {
        kpis: [
            ("按键总数", fmt_thousands(keys)),
            ("活跃时长", fmt_compact_duration(active)),
            ("连续天数", streak),
            ("设备数", snap.devices.len().to_string()),
        ],
        sentence,
        gaps,
        top_apps,
        top_keys,
    }
}

/// 画与命中共用的几何：四格 KPI、一句结论、缺读那一行、两张 Top 3 卡片及其行。
pub struct BoardLayout {
    pub kpi: [D2D_RECT_F; 4],
    pub line: D2D_RECT_F,
    pub gaps: D2D_RECT_F,
    pub cards: [D2D_RECT_F; 2],
    pub links: [D2D_RECT_F; 2],
    pub rows: [[D2D_RECT_F; 3]; 2],
}

const M: f32 = 16.0;
const KPI_H: f32 = 78.0;
const KPI_GAP: f32 = 10.0;
const CARD_H: f32 = 150.0;
const HEADER_H: f32 = 34.0;
const ROW_H: f32 = 30.0;
const LINK_W: f32 = 76.0;

fn box_rect(x: f32, y: f32, w: f32, h: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}

pub fn layout(w: f32, _h: f32) -> BoardLayout {
    let top = content_top(V_BOARD);
    let kw = (w - M * 2.0 - KPI_GAP * 3.0) / 4.0;
    let mut kpi = [box_rect(0.0, 0.0, 0.0, 0.0); 4];
    for (i, slot) in kpi.iter_mut().enumerate() {
        *slot = box_rect(M + i as f32 * (kw + KPI_GAP), top, kw, KPI_H);
    }
    let line = box_rect(M, top + KPI_H + 10.0, w - M * 2.0, 20.0);
    let gaps = box_rect(M, line.top + 22.0, w - M * 2.0, 16.0);
    let cards_top = gaps.top + 16.0 + 10.0;
    let cw = (w - M * 2.0 - 12.0) / 2.0;
    let cards = [
        box_rect(M, cards_top, cw, CARD_H),
        box_rect(M + cw + 12.0, cards_top, cw, CARD_H),
    ];
    let mut links = [box_rect(0.0, 0.0, 0.0, 0.0); 2];
    let mut rows = [[box_rect(0.0, 0.0, 0.0, 0.0); 3]; 2];
    for (c, card) in cards.iter().enumerate() {
        links[c] = box_rect(card.right - 8.0 - LINK_W, card.top + 8.0, LINK_W, 18.0);
        for (r, slot) in rows[c].iter_mut().enumerate() {
            *slot = box_rect(
                card.left + 8.0,
                card.top + HEADER_H + r as f32 * ROW_H,
                card.right - card.left - 16.0,
                ROW_H - 2.0,
            );
        }
    }
    BoardLayout {
        kpi,
        line,
        gaps,
        cards,
        links,
        rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use focusflow_core::stats::GoalStatus;

    fn snapshot() -> Snapshot {
        Snapshot {
            period: 7,
            period_total: 42,
            app_total: 5400,
            apps: vec![
                ("编辑器".to_string(), 3000),
                ("浏览器".to_string(), 1800),
                ("终端".to_string(), 600),
                ("第四名".to_string(), 10),
            ],
            rank: vec![
                ("A".to_string(), 30),
                ("B".to_string(), 9),
                ("C".to_string(), 3),
                ("D".to_string(), 1),
            ],
            goal: Some(GoalStatus {
                goal: 1,
                today: 0,
                today_met: false,
                streak: 3,
                best: 5,
                days: Vec::new(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn the_landing_page_reads_the_current_period_and_top_three() {
        let v = board_view(&snapshot(), "近 7 天");
        assert_eq!(v.kpis[0], ("按键总数", "42".to_string()));
        assert_eq!(v.kpis[1], ("活跃时长", "1.5h".to_string()));
        assert_eq!(v.kpis[2], ("连续天数", "3 天".to_string()));
        assert_eq!(v.kpis[3], ("设备数", "0".to_string()));
        assert_eq!(v.top_apps.len(), TOP_N);
        assert_eq!(v.top_apps[0], ("编辑器".to_string(), "50m".to_string()));
        assert_eq!(v.top_apps[2].0, "终端");
        assert_eq!(v.top_keys.len(), TOP_N);
        assert_eq!(v.top_keys[0], ("A".to_string(), "30".to_string()));
        assert!(v.sentence.contains("近 7 天"));
        assert!(v.sentence.contains("最常用 编辑器"));
    }

    #[test]
    fn an_empty_period_says_so_and_a_missing_goal_is_not_read_as_zero() {
        let snap = Snapshot {
            apps: Vec::new(),
            rank: Vec::new(),
            goal: None,
            ..Default::default()
        };
        let v = board_view(&snap, "今日");
        assert!(v.top_apps.is_empty());
        assert!(v.top_keys.is_empty());
        assert!(v.sentence.contains("没有归到某个应用上的活动"));
        assert_eq!(v.kpis[2].1, "读不出");
    }

    #[test]
    fn the_unreadable_note_is_carried_to_the_page() {
        let snap = Snapshot {
            gaps: vec!["2025 年库每日次数读不出来：坏页".to_string()],
            ..Default::default()
        };
        let v = board_view(&snap, "全部");
        assert!(v.gaps.unwrap_or_default().contains("坏页"));
    }

    #[test]
    fn the_two_top_cards_fit_at_the_minimum_window() {
        let l = layout(960.0, 480.0);
        for card in l.cards {
            assert!(
                card.bottom <= 480.0 - 16.0 + 0.01,
                "Top 卡片画到窗口下面：{:?}",
                card
            );
        }
        for row in l.rows.iter().flatten() {
            assert!(row.bottom - row.top >= 24.0, "Top 行不够高：{row:?}");
        }
        for (card, link) in l.cards.iter().zip(l.links.iter()) {
            assert!(
                link.right <= card.right - 8.0 + 0.01 && link.left >= card.left,
                "查看排行那一格跑出卡片：{link:?}"
            );
        }
        // 宽窗只放宽，不改变纵向位置。
        let wide = layout(1200.0, 860.0);
        assert_eq!(wide.cards[0].top, l.cards[0].top);
    }
}
