//! 宿主投给插件的进程级事件件口（方案 3.6 的第三条事件：`app_change`）。
//!
//! 形状与 `plugin_refresh` 那个"谁脏了"的件口一致：**写的人只投一个名字，读的人在
//! GUI 那一拍取走**。为什么不能直接回调：前台应用是采样线程看见的，而回调函数住在插件
//! 自己的 Lua 里 —— `mlua::Function` 与 `Lua` 都不是 `Send`，跨线程只能投请求，不能投 VM
//! （与按键攒批同一条约束）。
//!
//! **只留最新那一个，不做队列**：件口存的是"此刻前台是谁"这个事实，不是"发生过几次切换"
//! 的历史。把十次切换排成队列表面上是"一次不漏"，实际交给用户的是十个他早已切走的应用名，
//! 而插件回调拿到的载荷本来就该是"现在前台是哪个"。丢掉的中间态不是丢数据。
//!
//! 隐私那一条按 `[app_stats] exclude` 的既有语义走：被排除的进程**连名字都不投**
//! （调用方在投之前就判过），所以"完全不记录"这句话对插件这一侧同样成立。

use std::sync::Mutex;

/// 件口的两个格子：`current` 是"上一次投出去的是谁"（用来去重），
/// `pending` 是"GUI 还没取走的那一个"。合成一个 `Mutex` 是为了让"变了才投"这一步
/// 读与写之间没有缝 —— 分两把锁的话，两次同名投递可能都被判成"变了"。
#[derive(Default)]
struct Slot {
    current: Option<String>,
    pending: Option<String>,
}

static SLOT: Mutex<Slot> = Mutex::new(Slot {
    current: None,
    pending: None,
});

/// 采样线程投一次"前台换成 `name` 了"。同名不重复投，交回**这次到底投没投**。
///
/// 交回布尔值不是为了好看：调用方（`app_stats` 那条循环）由此能在日志里说清"这一轮投了
/// 还是没投"，而"没投出去"与"投了但没人取"是两种不同的事。
pub fn post_app_change(name: &str) -> bool {
    let mut slot = SLOT.lock().unwrap_or_else(|e| e.into_inner());
    if slot.current.as_deref() == Some(name) {
        return false;
    }
    slot.current = Some(name.to_string());
    slot.pending = Some(name.to_string());
    true
}

/// GUI 那一拍取走（取完即空）。没有新事件时回 `None`。
///
/// 取走**不清** `current`：那是"上一次投出去的是谁"，去重要靠它。前台一直停在同一个应用时
/// 不该每一拍都给插件一次回调。
pub fn take_app_change() -> Option<String> {
    SLOT.lock()
        .unwrap_or_else(|e| e.into_inner())
        .pending
        .take()
}

/// 清空件口：给用例与"关掉应用统计"那一路用（关掉之后不许还有旧名字等着被投给插件）。
pub fn reset_app_change() {
    let mut slot = SLOT.lock().unwrap_or_else(|e| e.into_inner());
    *slot = Slot::default();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 去重与"取走不清 current"这两件事必须一起成立：
    /// 停在同一个应用上时不该每一拍都回调，而切走再切回来要能再投一次。
    #[test]
    fn the_same_app_is_posted_once_until_it_changes() {
        // 件口是进程级的：本模块的用例串行跑，起头都先清一次
        reset_app_change();
        assert!(post_app_change("甲.exe"), "第一次投该算变了");
        assert!(!post_app_change("甲.exe"), "同名再投不算变");
        assert_eq!(take_app_change().as_deref(), Some("甲.exe"));
        assert!(
            take_app_change().is_none(),
            "取走之后再取是空，不是把同一个名字再给一遍"
        );
        assert!(
            !post_app_change("甲.exe"),
            "前台没换过，取走也不算又变了一次"
        );
        assert!(post_app_change("乙.exe"), "换成别的应用就是变了");
        assert_eq!(take_app_change().as_deref(), Some("乙.exe"));
        reset_app_change();
        assert!(post_app_change("甲.exe"), "清过件口之后重新算一次");
        reset_app_change();
    }

    /// 件口只留最新那一个：投三次不同名字，取到的是第三次那个事实。
    #[test]
    fn the_slot_keeps_the_latest_fact_not_the_history() {
        reset_app_change();
        for name in ["一.exe", "二.exe", "三.exe"] {
            assert!(post_app_change(name));
        }
        assert_eq!(
            take_app_change().as_deref(),
            Some("三.exe"),
            "排队给插件的是十个早已切走的名字，不是「现在是谁」"
        );
        assert!(take_app_change().is_none());
        reset_app_change();
    }
}
