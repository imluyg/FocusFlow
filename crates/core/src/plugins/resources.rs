//! 共享资源的登记与归还（方案 3.11）。
//!
//! 这里要解决的是一族反复出现过的缺陷：**约定归还**。调度线程从前按 owner 计数、番茄钟按
//! 单持有者记名、定时器按 owner 分格 —— 三套各自为政的机制，同一个症状：某个回收路径漏调
//! 其中一处，于是插件页写着「已停用」，而那条线程/那份状态还在为它跑（调度线程还会到点
//! `CreateProcess` 启动程序，`host.rs` 顶上写的就是那次事故）。
//!
//! 规矩改成一条：共享资源在这里登记一次，卸载与出错那两条路由 [`release_all`] 一次叫完。
//! 新增一份要参与这件事的资源时，只改这一处，不必再去每个回收点补一行 ——
//! 补一行的那一种写法，早晚会有第四个资源漏掉第五个调用点。
//!
//! `acquire` 也走这里：只写 `release` 的登记表没有意义，「谁在用」与「谁该还」必须是同一份名册。

use std::sync::OnceLock;

/// 一份共享资源。**必须是 `Send + Sync`**：注册表是进程级的，而投递方（写线程、检测线程）
/// 与归还方（GUI 线程）不是同一根线程 —— 资源本身怎么锁由它自己决定，这一层只保证叫得动。
pub trait HostResource: Send + Sync {
    /// 页面上、日志里说的那一个名字。
    fn label(&self) -> &'static str;
    /// 记下"这个 owner 在用我"。幂等（重复叫同一个人不产生第二份）。
    fn acquire(&self, owner: &str);
    /// 归还。**只有真的动过手才交回 `true`** —— 这句是给日志与用例看的：
    /// "release 跑过了"与"这条线程确实被拆了"是两件事，前者不许念成后者。
    fn release(&self, owner: &str) -> bool;
}

/// 调度线程（多个插件共用，按 owner 计数，最后一个走了才停）。
struct SchedulerResource;

impl HostResource for SchedulerResource {
    fn label(&self) -> &'static str {
        "调度线程"
    }

    fn acquire(&self, owner: &str) {
        super::host::claim_scheduler(owner);
    }

    fn release(&self, owner: &str) -> bool {
        super::host::release_scheduler_checked(owner)
    }
}

/// 番茄钟计时线程（单持有者：一个计时会话本就只属于一个插件）。
struct PomodoroResource;

impl HostResource for PomodoroResource {
    fn label(&self) -> &'static str {
        "番茄钟"
    }

    fn acquire(&self, owner: &str) {
        super::host::claim_pomodoro(owner);
    }

    fn release(&self, owner: &str) -> bool {
        super::host::shutdown_pomodoro_if_owned_by(owner)
    }
}

/// 这个插件挂着的定时器与事件（`plugin_timers` 那张表）。
///
/// 它与上面两份的差别只在"要不要记名"：句柄与到期时刻都在 owner 那一格里，
/// 卸载就是把整格摘掉，所以 `acquire` 什么都不用做 —— 排程本身即登记。
/// 这一份留在名册里是必须的：从前它散在卸载、出错、加载失败三处，
/// 少补一处就是"每一拍还在为一个跑不了的回调排队"。
struct TimerResource;

impl HostResource for TimerResource {
    fn label(&self) -> &'static str {
        "定时器"
    }

    fn acquire(&self, _owner: &str) {}

    fn release(&self, owner: &str) -> bool {
        super::plugin_timers::forget(owner) > 0
    }
}

/// 名册。写死在这三件套上而不是开一个 `register()` 让别人来自报：
/// 自报式的注册表要回答"谁在什么时候注册"，而这三份资源都是进程级单例、
/// 与插件系统同生共死 —— 一个 `Vec` 加一条 `OnceLock` 换来的是更早的失败与更少的可测性。
/// 新增一份就在这个函数里加一行，`release_all` 自动带上它。
static REGISTRY: OnceLock<Vec<Box<dyn HostResource>>> = OnceLock::new();

pub fn registry() -> &'static [Box<dyn HostResource>] {
    REGISTRY.get_or_init(|| {
        vec![
            Box::new(SchedulerResource) as Box<dyn HostResource>,
            Box::new(PomodoroResource),
            Box::new(TimerResource),
        ]
    })
}

/// 名册里某一份资源的名字（`acquire` 与 `release_named` 按它找）。
pub const SCHEDULER: &str = "调度线程";
pub const POMODORO: &str = "番茄钟";
pub const TIMERS: &str = "定时器";

/// 记下"这个 owner 在用这一份资源"。认不得的名字出声，不静默吞掉：
/// 拼错一份资源的名字，症状就是它永远不会被归还。
pub fn acquire(label: &str, owner: &str) {
    match registry().iter().find(|r| r.label() == label) {
        Some(res) => res.acquire(owner),
        None => tracing::error!(
            "共享资源名册里没有「{label}」这一份：这次登记没生效，\
             可写的名字见 resources::registry()（新增一份要在名册里加一行）"
        ),
    }
}

/// 归还**某一份**资源（Lua 侧那句 `scheduler_shutdown()` 走这里：名字与名册同一处）。
///
/// 交回有没有真的动过手，调用方要是不接这个值就等于把"跑过了"念成"拆掉了"。
pub fn release(label: &str, owner: &str) -> bool {
    match registry().iter().find(|r| r.label() == label) {
        Some(res) => res.release(owner),
        None => {
            tracing::error!(
                "共享资源名册里没有「{label}」这一份：这次归还没生效，\
                 可写的名字见 resources::registry()"
            );
            false
        }
    }
}

/// 这个 owner 走了：把它占着的每一份资源都叫一遍归还。
///
/// 交回**真的动过手**的那些资源名，给日志与用例当观察量。不交回"跑过了哪几份"，
/// 因为"叫过了"与"拆掉了"是两句话（`release` 那句判据就为这件事写在 trait 上）。
pub fn release_all(owner: &str) -> Vec<&'static str> {
    let mut done: Vec<&'static str> = Vec::new();
    for res in registry().iter() {
        if res.release(owner) {
            done.push(res.label());
        }
    }
    done
}

/// 这一份资源此刻还登记着这个 owner 吗（用例与详情页用）。
#[cfg(any(test, feature = "test-utils"))]
pub fn holds_for_test(label: &str, owner: &str) -> bool {
    match label {
        SCHEDULER => super::host::scheduler_has_owner_for_test(owner),
        POMODORO => super::host::pomodoro_owner_for_test().as_deref() == Some(owner),
        TIMERS => super::plugin_timers::board_for(owner)
            .lock()
            .map(|b| !b.is_empty())
            .unwrap_or(false),
        other => {
            tracing::error!("名册里没有「{other}」这一份资源，问不到它的登记");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 名册里每一份都能被叫到，且 `release_all` 交回的是"真的动过手"那几份。
    ///
    /// 这一条同时是防"名册加了一份、它的 release 从来没人调"：叫一遍就叫到了，
    /// 少一份就是 `done` 里少一格，而格子数与登记的份数必须对得上。
    #[test]
    fn the_roster_answers_for_every_registered_resource() {
        let _lock = crate::paths::test_app_dir_lock();
        let labels: Vec<&str> = registry().iter().map(|r| r.label()).collect();
        assert_eq!(
            labels,
            vec![SCHEDULER, POMODORO, TIMERS],
            "名册里的资源必须一个个数得出，实得 {labels:?}"
        );
        let owner = "roster_probe";
        acquire(TIMERS, owner);
        crate::plugins::plugin_timers::board_for(owner)
            .lock()
            .unwrap()
            .arm(
                crate::plugins::plugin_timers::Kind::Every,
                30,
                std::time::Instant::now(),
            )
            .expect("arm");
        acquire(POMODORO, owner);
        assert!(
            holds_for_test(POMODORO, owner),
            "番茄钟该登记在这个 owner 名下"
        );
        let released = release_all(owner);
        assert!(
            released.contains(&TIMERS) && released.contains(&POMODORO),
            "这两份都为它动过手，实得 {released:?}"
        );
        assert!(
            !released.contains(&SCHEDULER),
            "它没占过调度线程，不许报成「为它拆了线程」: {released:?}"
        );
        assert!(!holds_for_test(POMODORO, owner), "归还之后名字要没了");
        // 第二次归还必须是空操作：不许把别人的登记一起带走
        assert!(
            release_all(owner).is_empty(),
            "同一个 owner 第二次归还该是空的"
        );
    }

    /// 名册外的名字不许悄悄成功：拼错一份资源名的症状是"它永远不会被归还"。
    #[test]
    fn an_unknown_label_is_refused_loudly() {
        let _lock = crate::paths::test_app_dir_lock();
        // 只断"不 panic、且不登记到任何一份上"：出声那一侧走日志，用例里不抢日志闸
        acquire("没有这一份资源", "roster_typo");
        assert!(!holds_for_test("没有这一份资源", "roster_typo"));
    }
}
