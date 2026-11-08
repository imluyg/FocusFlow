//! 列表重查之后"选中哪一条、滚到哪"的结算（管理页与弹窗共用）。
//!
//! 重查前记下选中那条的 id，重查后按 id 找回来。只 clamp 下标是不够的：新复制插在
//! 列表顶部会让后面每一条的下标整体后移，光标（以及弹窗里紧接着按下的 Enter）就落到
//! 了另一条上 —— 用户看到的是"粘错了"。

/// 这次重查为什么发生。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadIntent {
    /// 同一批数据里多/少了一条（新复制、固定、删除）：选中跟 id，视口尽量不动。
    Keep,
    /// 数据换了一批（进页、换模式、换筛选）：回到第一行、滚动归零。
    ResetTop,
}

/// 选中行与滚动位置，都是相对**过滤后**列表的下标。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Viewport {
    pub sel: usize,
    pub scroll: usize,
}

/// 结算重查后的视口。
///
/// - `ids`：重查后的完整 id 列表（顺序即屏幕顺序）。
/// - `prev_id`：重查前选中那条的 id；空列表或本来没选中时为 `None`。
/// - `prev`：重查前的位置。
/// - `was_at_top`：重查前视口就在顶部（`scroll == 0`）——此时新条目插进来就该直接看见。
/// - `visible`：一次能画多少行；`0` 当成退化输入处理。
pub fn settle(
    ids: &[i64],
    prev_id: Option<i64>,
    prev: Viewport,
    was_at_top: bool,
    visible: usize,
    intent: ReloadIntent,
) -> Viewport {
    if ids.is_empty() || visible == 0 || intent == ReloadIntent::ResetTop {
        return Viewport::default();
    }
    let visible = visible.max(1);
    let len = ids.len();
    // id 不在了（被删）就落在原下标上，别把光标甩回顶部。
    let sel = prev_id
        .and_then(|id| ids.iter().position(|x| *x == id))
        .unwrap_or_else(|| prev.sel.min(len - 1));
    let max_scroll = len.saturating_sub(visible);
    let mut scroll = if was_at_top {
        0
    } else {
        prev.scroll.min(max_scroll)
    };
    if sel < scroll {
        scroll = sel;
    } else if sel >= scroll + visible {
        scroll = sel + 1 - visible;
    }
    Viewport {
        sel,
        scroll: scroll.min(max_scroll),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEEP: ReloadIntent = ReloadIntent::Keep;
    const RESET: ReloadIntent = ReloadIntent::ResetTop;

    #[test]
    fn reset_top_always_goes_back_to_the_first_row() {
        let vp = settle(
            &[1, 2, 3, 4],
            Some(3),
            Viewport { sel: 2, scroll: 1 },
            false,
            2,
            RESET,
        );
        assert_eq!(vp, Viewport::default());
    }

    #[test]
    fn a_new_entry_at_the_top_leaves_the_selection_on_its_own_row() {
        // 在顶部看 [1,2,3]、选中 2；新复制 9 插到最前面。
        let ids = [9, 1, 2, 3];
        let vp = settle(&ids, Some(2), Viewport { sel: 1, scroll: 0 }, true, 3, KEEP);
        assert_eq!(ids[vp.sel], 2, "光标要跟着 id 走，不能停在原下标");
        assert_eq!(vp.scroll, 0, "本来在顶部：新条目应当直接可见");
    }

    #[test]
    fn a_new_entry_while_scrolled_down_does_not_move_the_viewport() {
        let prev: Vec<i64> = (0..10).collect();
        let mut ids: Vec<i64> = prev.clone();
        ids.insert(0, 99);
        let vp = settle(
            &ids,
            Some(prev[6]),
            Viewport { sel: 6, scroll: 4 },
            false,
            3,
            KEEP,
        );
        assert_eq!(ids[vp.sel], 6, "选中仍是同一条");
        assert_eq!(vp.sel - vp.scroll, 2, "已经下滚：选中行在屏幕上的位置不动");
        assert!(
            (vp.scroll..vp.scroll + 3).contains(&vp.sel),
            "选中行必须在视口里"
        );
    }

    #[test]
    fn a_pin_reorder_follows_the_id_instead_of_the_index() {
        // 固定把 2 顶到最前面：同一批数据，只是顺序变了。
        let ids = [2, 1, 3];
        let vp = settle(&ids, Some(2), Viewport { sel: 1, scroll: 0 }, true, 3, KEEP);
        assert_eq!(vp.sel, 0);
        assert_eq!(ids[vp.sel], 2);
        assert_eq!(vp.scroll, 0);
    }

    #[test]
    fn deleting_the_selected_row_lands_on_the_same_index() {
        let vp = settle(
            &[1, 3],
            Some(2),
            Viewport { sel: 1, scroll: 0 },
            true,
            3,
            KEEP,
        );
        assert_eq!(
            vp,
            Viewport { sel: 1, scroll: 0 },
            "第 2 条被删，光标留在原位指向下一条"
        );
    }

    #[test]
    fn deleting_the_last_row_clamps_to_the_new_last_row() {
        let vp = settle(
            &[1, 2],
            Some(3),
            Viewport { sel: 2, scroll: 0 },
            true,
            3,
            KEEP,
        );
        assert_eq!(vp, Viewport { sel: 1, scroll: 0 });
    }

    #[test]
    fn an_empty_list_degrades_to_the_top() {
        let vp = settle(&[], Some(4), Viewport { sel: 3, scroll: 2 }, false, 5, KEEP);
        assert_eq!(vp, Viewport::default());
    }

    #[test]
    fn a_zero_visible_row_count_degrades_to_the_top() {
        let vp = settle(
            &[1, 2],
            Some(2),
            Viewport { sel: 1, scroll: 0 },
            false,
            0,
            KEEP,
        );
        assert_eq!(vp, Viewport::default());
    }

    #[test]
    fn one_visible_row_keeps_scroll_equal_to_selection() {
        let ids: Vec<i64> = (0..10).collect();
        let vp = settle(
            &ids,
            Some(7),
            Viewport { sel: 7, scroll: 7 },
            false,
            1,
            KEEP,
        );
        assert_eq!(vp.sel, vp.scroll, "只有一行可见时选中行就是那一行");
        assert_eq!(vp.sel, 7);
    }

    #[test]
    fn a_reappearing_id_far_away_pulls_the_viewport_to_it() {
        // 排序换了：同一条 id 掉到了列表底部。
        let ids = [1, 2, 3, 4, 5];
        let vp = settle(&ids, Some(5), Viewport { sel: 0, scroll: 0 }, true, 2, KEEP);
        assert_eq!(ids[vp.sel], 5);
        assert_eq!(vp.scroll, 3, "选中行滚回视口，不能留在屏幕外");
    }

    #[test]
    fn a_shrunken_list_clamps_the_old_scroll() {
        let vp = settle(
            &[1, 2, 3],
            Some(4),
            Viewport { sel: 7, scroll: 7 },
            false,
            3,
            KEEP,
        );
        assert_eq!(vp.sel, 2);
        assert_eq!(vp.scroll, 0);
    }
}
