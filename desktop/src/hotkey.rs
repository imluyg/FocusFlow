//! 全局热键：一张表管所有绑定。
//!
//! 支持运行时重新注册：设置页修改 `[hotkey]` 任一条目后，通过 `reload_hotkey`
//! 卸载旧的并按最新配置重新注册，无需重启程序。
//!
//! 为什么**必须**做成表，而不是"要加功能就在别处再 `on_shortcut` 一条"：
//! `reload_hotkey` 的卸载动作是 `unregister_all()` —— 它摘掉的是这个进程注册的
//! **全部**快捷键。挂在别处的热键会在用户改一次设置页热键后被悄悄卸掉，
//! 症状是"昨天还好好的，今天按截图没反应了"。所以全局热键只能从 `BINDINGS` 出生。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use tauri::{App, AppHandle, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

/// 每条绑定的注册失败原因，按 config 键名分格存。
///
/// 两个理由决定了它的形状：
/// 1. **必须存住而不能只发事件**：启动时的注册早于前端首次读设置，事件那时没有监听者；
///    而组合键被其它程序占用是最常见的静默失败 —— 设置页显示着「已启用」，
///    按下去却毫无反应，用户没有任何线索。
/// 2. **必须按 key 分格**：两条热键可以一条成功、一条被占用。共用一格的话两行都会
///    显示同一个原因；而关掉开关留下的过期报错，会让人以为另一条好的也坏了。
static LAST_ERRORS: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn set_last_error(key: &str, msg: Option<String>) {
    let mut map = LAST_ERRORS.lock().unwrap_or_else(|e| e.into_inner());
    match msg {
        Some(m) => {
            map.insert(key.to_string(), m);
        }
        // 不注册的两条路（总开关关着 / 输入框被清空）都算"没有报错"：
        // 关掉开关后不能留着上一次的占用报错，否则设置页会显示一条已过期的提示。
        None => {
            map.remove(key);
        }
    }
}

/// 供设置页展示的最近一次注册失败原因（按 config 键名取，`None` = 正常）。
pub fn last_error(key: &str) -> Option<String> {
    LAST_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
        .cloned()
}

/// 显示/隐藏主窗口的回调处理。
fn toggle_main_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        if win.is_visible().unwrap_or(false) {
            crate::state::hide_main_window(app);
        } else {
            crate::state::show_main_window(app);
        }
    } else {
        crate::state::show_main_window(app);
    }
}

/// 区域截图的回调。按了没反应必须留下一条能查到的原因，最常见的一条是"上一张还挂着"。
fn trigger_snip(app: &AppHandle) {
    if let Err(e) = crate::snip::trigger(app, false) {
        tracing::warn!("截图热键未能开始：{e}");
    }
}

/// 带标注的区域截图：同一套抓屏与覆盖层，只是松手后进标注态而不是立刻结案。
fn trigger_snip_annotate(app: &AppHandle) {
    if let Err(e) = crate::snip::trigger(app, true) {
        tracing::warn!("标注截图热键未能开始：{e}");
    }
}

/// 一条全局热键绑定。
struct Binding {
    /// `[hotkey]` 段里的键名：设置页读写的键名、失败原因的索引、默认组合的出处。
    key: &'static str,
    /// 给人看的用途，出现在日志与撞车提示里。
    what: &'static str,
    handler: fn(&AppHandle),
}

/// 全部全局热键。**加功能要往这张表里加，不要在别处调 `on_shortcut`**（理由见文件头）。
const BINDINGS: &[Binding] = &[
    Binding {
        key: "toggle_window",
        what: "显示/隐藏主窗口",
        handler: toggle_main_window,
    },
    Binding {
        key: "snip",
        what: "区域截图",
        handler: trigger_snip,
    },
    Binding {
        key: "snip_annotate",
        what: "区域截图（带标注）",
        handler: trigger_snip_annotate,
    },
];

/// 表里的键名，**从 `BINDINGS` 推**（给用例做"逐条覆盖"的循环用；不再抄第二份清单）。
pub fn binding_keys() -> Vec<&'static str> {
    BINDINGS.iter().map(|b| b.key).collect()
}

/// 这个 `[hotkey]` 键是不是一条热键组合。设置页改完要不要重注册，判据只有这一处。
pub fn is_binding_key(key: &str) -> bool {
    BINDINGS.iter().any(|b| b.key == key)
}

/// 该注册成哪个热键；`None` = 本次不注册（未启用，或用户把这一条的输入框清空了）。
///
/// 刻意用 `get` 而不是 `get_or(..., 默认组合)`：`get_or` 把**空串当"没配"**
/// （见 `config.rs::get_or`），于是在设置页里清空热键框 —— 那是"我不想让程序占用
/// 任何全局快捷键"的正常表达 —— 会被读成默认组合：框是空的、开关还勾着、
/// `is_empty()` 那条分支永远到不了，而组合键已经被悄悄全局占用。
///
/// 默认组合本来就写在 `default_config()` 里、首次落盘一定带上，所以"键不存在"与
/// "用户主动清空"这两件事用 `get` 才分得开，这里不必再兜一次默认值。
fn pending_spec(config: &focusflow_core::config::FocusFlowConfig, key: &str) -> Option<String> {
    if !config.get_bool("hotkey", "enabled", false) {
        return None;
    }
    let spec = config.get("hotkey", key).trim().to_lowercase();
    if spec.is_empty() {
        return None;
    }
    Some(spec)
}

/// 找出"被两条以上绑定共用"的组合键 → 共用它的键名。
///
/// 不查的后果是难查：同一组合键注册两次，第二次必然失败，而报错写的是
/// "可能被其它程序占用" —— 真正的原因是用户自己把两条热键填重了，
/// 占用者就是本程序半秒前刚注册的那一条。
fn collisions(
    config: &focusflow_core::config::FocusFlowConfig,
) -> HashMap<String, Vec<&'static str>> {
    let mut by_spec: HashMap<String, Vec<&'static str>> = HashMap::new();
    for b in BINDINGS {
        if let Some(spec) = pending_spec(config, b.key) {
            by_spec.entry(spec).or_default().push(b.key);
        }
    }
    by_spec.retain(|_, keys| keys.len() > 1);
    by_spec
}

/// 一条热键事件该不该跑 handler —— **只有 `Pressed`**。
///
/// 这不是防御性代码，是 Windows 上实测出来的事件形状：`global-hotkey 0.8` 收到 `WM_HOTKEY`
/// 之后会**另起一个线程每 50ms 轮询 `GetAsyncKeyState`**，等键松开时再补发一条 `Released`
/// （见其 `platform_impl/windows/mod.rs` 的 `global_hotkey_proc`）。插件两种都转给回调、
/// 自己不过滤，它自己的 README 里就明写要判 `state == ShortcutState::Pressed`。
///
/// 不判的后果按绑定各不一样，而且都不响亮：
/// - 截图：第二遍撞上 `IN_FLIGHT` 被拒 ⇒ 真机日志里 45 条"已有一张截图在进行中"，
///   而"13 次开始对 13 次拒绝"那个 1:1 就是它 —— 人手不会每次都双击得这么准。
/// - 显示/隐藏主窗口：按下 = 显示，松手 = 立刻隐藏 ⇒ **净效果就是"热键按了没反应"**。
///   这条今天没暴露，只是因为他的 `toggle_window` 是空的（那是"别占用这个键"的正常表达）。
/// - 更险的一格：如果第一遍在松手前就完事（截图很快），第二遍会**再起一张新截图**，
///   症状是覆盖层闪两下 —— 正是 `IN_FLIGHT` 那条评论当初描述过的现象。
fn should_run_on(state: ShortcutState) -> bool {
    matches!(state, ShortcutState::Pressed)
}

/// 按当前配置注册表里的每一条（不先卸载；调用方负责先 `unregister_all`）。
fn register_all(app: &AppHandle) {
    let config = focusflow_core::config::instance();
    let clash = collisions(config);
    // 日志在**这一层**记，不在 `pending_spec` 里记：那个函数同时被 `collisions` 和
    // 注册循环调用，在里面记一句就会同一件事打两行（真机日志里就这么吵过一次，
    // 看着像配置被读了两遍，白耗人一轮）。
    let master_on = config.get_bool("hotkey", "enabled", false);
    if !master_on {
        tracing::info!("全局热键未启用，本次不注册任何热键");
    }
    for b in BINDINGS {
        let Some(spec) = pending_spec(config, b.key) else {
            // 不注册的两条路都要清掉这一格自己的旧报错，别让它留在设置页上。
            set_last_error(b.key, None);
            if master_on {
                tracing::warn!(
                    "热键 {}（{}）的组合键是空的，本次不注册它（要恢复请在设置页把组合键填回去）",
                    b.key,
                    b.what
                );
            }
            continue;
        };
        if let Some(shares) = clash.get(&spec) {
            set_last_error(
                b.key,
                Some(format!(
                    "{spec} 同时被 {} 用着，两条都没注册（改一条的组合键）",
                    shares.join(" / ")
                )),
            );
            tracing::warn!("热键 {} = {spec} 与其它条目撞车，跳过注册", b.key);
            continue;
        }
        let handler = b.handler;
        let result =
            app.global_shortcut()
                .on_shortcut(spec.as_str(), move |app, _shortcut, event| {
                    if should_run_on(event.state) {
                        (handler)(app);
                    }
                });
        match result {
            Ok(()) => {
                set_last_error(b.key, None);
                tracing::info!("全局热键已注册: {spec}（{}）", b.what);
            }
            Err(e) => {
                let msg = format!("{spec} 注册失败，可能被其它程序占用：{e}");
                tracing::warn!("全局热键注册失败 ({} {spec}): {e}", b.what);
                set_last_error(b.key, Some(msg));
            }
        }
    }
}

/// 启动时注册全局热键。
pub fn setup_hotkey(app: &App) {
    register_all(app.handle());
}

/// 运行时重新加载热键：先卸载全部，再按最新配置重新注册表里的每一条。设置页改动热键后调用。
///
/// 这里敢用 `unregister_all()` 的前提是文件头那条纪律：本程序的全局热键**只**从
/// `BINDINGS` 出生。以后若有人在别处 `on_shortcut`，这条就会把它一起摘掉。
pub fn reload_hotkey(app: &AppHandle) {
    // 先卸载旧的，避免重复注册同一快捷键
    match app.global_shortcut().unregister_all() {
        Ok(()) => tracing::debug!("已卸载全部全局热键"),
        Err(e) => tracing::warn!("卸载全局热键失败: {e}"),
    }
    register_all(app);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 写一份临时 config.ini 并加载。
    ///
    /// 放 `target/` 下按 tag 固定命名：`%TEMP%` 里的随机名每跑一次漏一个目录
    /// （本仓量过这个账），而 `cargo clean` 会连 `target/` 一起清掉。
    /// 每条用例一个名字 —— 并行跑的用例共用同一个文件会互相盖。
    fn cfg(tag: &str, body: &str) -> focusflow_core::config::FocusFlowConfig {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../target")
            .join(format!("ff_hotkey_{tag}.ini"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("{body}\n")).unwrap();
        focusflow_core::config::FocusFlowConfig::load(&path).unwrap()
    }

    /// `default_config()` 里 `[hotkey]` 的某个键。测试要的默认值一律从这里取，
    /// 不在断言里再抄一遍字面量 —— 抄了就有了"改了默认值而用例还在守旧值"的余地。
    fn default_spec(key: &str) -> String {
        focusflow_core::config::default_config()
            .get("hotkey")
            .and_then(|s| s.get(key))
            .cloned()
            .unwrap_or_default()
    }

    /// 一次物理按下会来**两条**事件（`Pressed`，加上松手时那个 50ms 轮询补发的 `Released`），
    /// handler 只能跑一遍。
    ///
    /// 为什么单独钉它：不判状态的旧写法在截图那条上只是刷日志（被 `IN_FLIGHT` 挡下），
    /// 在"显示/隐藏主窗口"那条上却是**自己把自己关掉** —— 按下显示、松手隐藏，
    /// 用户看到的就是"热键没反应"，而代码一行错都没有。这种"第二遍才致命"的形状
    /// 没有用例就看不见。区分性：把 `should_run_on` 改成无条件 `true` 这条当场红。
    #[test]
    fn only_pressed_events_run_the_handler() {
        use tauri_plugin_global_shortcut::ShortcutState;
        assert!(should_run_on(ShortcutState::Pressed), "按下必须跑 handler");
        assert!(
            !should_run_on(ShortcutState::Released),
            "松手那条不该再跑一遍 —— 跑两遍等于热键自己把自己关掉"
        );
    }

    /// 每条绑定都得有默认组合：缺键时 `get` 返回空串，那条热键就**永远注册不上**，
    /// 而日志里只有"已被清空"一行，看起来像用户自己清的。
    #[test]
    fn every_binding_has_a_default_spec() {
        for b in BINDINGS {
            assert!(
                !default_spec(b.key).is_empty(),
                "BINDINGS 里的 {} 在 default_config 的 [hotkey] 段没有默认组合",
                b.key
            );
        }
    }

    /// 出厂默认值之间不许互相撞车，否则第一帧就有一两条热键静默失效。
    #[test]
    fn default_specs_do_not_collide() {
        let mut seen: Vec<String> = Vec::new();
        for b in BINDINGS {
            let spec = default_spec(b.key);
            assert!(
                !seen.contains(&spec),
                "两条绑定默认都是 {spec}，会被撞车检查一起挡掉"
            );
            seen.push(spec);
        }
    }

    /// 三态（清空 / 只有空格 / 压根没这一行）+ 总开关关闭，对**每一条**绑定都成立。
    ///
    /// 清空热键输入框 = 不占用任何全局快捷键。旧写法 `get_or(..., "ctrl+shift+f")`
    /// 把空串当"没配"，所以框清空后实际注册的是默认组合：界面显示空白、开关还勾着，
    /// 组合键却被程序占住。
    #[test]
    fn cleared_absent_and_off_states_hold_for_every_binding() {
        for b in BINDINGS {
            let ini = format!("[hotkey]\nenabled = true\n{} = ", b.key);
            assert_eq!(
                pending_spec(&cfg(&format!("cleared-{}", b.key), &ini), b.key),
                None,
                "被清空的输入框不该被读成默认组合（{}）",
                b.key
            );

            let ini = format!("[hotkey]\nenabled = true\n{} =    ", b.key);
            assert_eq!(
                pending_spec(&cfg(&format!("blank-{}", b.key), &ini), b.key),
                None,
                "只有空格也算清空（{}）",
                b.key
            );

            // 与"清空"相对：文件里**压根没有**这一行时要退回默认值。`load()` 是从
            // `default_config()` 起步再被文件覆盖的，所以缺行不等于空串 —— 老配置文件
            // 升级上来仍然有热键可用。以前 `get_or` 把这两件事压成同一件。
            let expect = default_spec(b.key);
            assert_eq!(
                pending_spec(
                    &cfg(&format!("absent-{}", b.key), "[hotkey]\nenabled = true"),
                    b.key
                )
                .as_deref(),
                Some(expect.as_str()),
                "缺行 ≠ 清空：仍该用 default_config 里那句默认组合（{}）",
                b.key
            );

            let ini = format!("[hotkey]\nenabled = false\n{} = ctrl+alt+m", b.key);
            assert_eq!(
                pending_spec(&cfg(&format!("off-{}", b.key), &ini), b.key),
                None,
                "总开关关掉时不该碰任何组合键（{}）",
                b.key
            );
        }
    }

    /// 正常路径仍然生效，并且大小写/空白都被归一。
    #[test]
    fn configured_spec_is_used_verbatim_after_normalize() {
        let c = cfg(
            "normal",
            "[hotkey]\nenabled = true\ntoggle_window = Ctrl+Alt+M",
        );
        assert_eq!(
            pending_spec(&c, "toggle_window").as_deref(),
            Some("ctrl+alt+m"),
            "注册前要小写化并去掉首尾空白"
        );
        // `enabled = 1` 也是"启用"：这一族在 get_bool 里是认的
        let c = cfg("one", "[hotkey]\nenabled = 1\ntoggle_window = ctrl+alt+j");
        assert_eq!(
            pending_spec(&c, "toggle_window").as_deref(),
            Some("ctrl+alt+j"),
            "1/yes/on 与 true 同义（get_bool 的口径）"
        );
    }

    /// 两条热键填同一个组合键时，两条都要被认出来（而不是"后注册的那条报占用"）。
    #[test]
    fn same_spec_on_two_keys_is_reported_as_a_clash() {
        let c = cfg(
            "clash",
            "[hotkey]\nenabled = true\ntoggle_window = Ctrl+Shift+X\nsnip = ctrl+shift+x",
        );
        let got = collisions(&c);
        let keys = got.get("ctrl+shift+x").expect("大小写不同也该认出撞车");
        assert_eq!(keys.len(), 2, "两条都该进撞车名单，实际 {keys:?}");
    }

    /// 只有一条用某个组合键时不算撞车 —— 否则"改成一个冷门组合"会被无故挡下。
    #[test]
    fn a_unique_spec_is_not_a_clash() {
        let c = cfg(
            "unique",
            "[hotkey]\nenabled = true\ntoggle_window = ctrl+shift+f\nsnip = shift+f1",
        );
        assert!(collisions(&c).is_empty(), "{:?}", collisions(&c));
    }

    /// 撞车名单里不该出现"某一条被清空"的组合：清空 = 不注册 = 不占位。
    #[test]
    fn a_cleared_key_does_not_join_a_clash() {
        let c = cfg(
            "clash-cleared",
            "[hotkey]\nenabled = true\ntoggle_window = shift+f1\nsnip = ",
        );
        assert!(
            collisions(&c).is_empty(),
            "snip 已被清空，不该因为它把 toggle_window 也判成撞车"
        );
    }

    /// `LAST_ERRORS` 是**进程级**的一张表，而 cargo 默认并行跑用例 ⇒ 碰它的用例必须串行。
    /// 不串行时不是"偶发红"而是"互相看得见对方写的格"：2026-09-29 门禁实测 10 次红 1 次，
    /// `errors_are_keyed_per_binding` 读到 `toggle_window = Some("B 占用了")` ——
    /// 那串只有隔壁 `two_errors_coexist_without_overwriting` 会写。
    fn last_errors_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 失败原因必须按 key 分格，且"这一条不注册"会清掉**它自己**的旧报错。
    #[test]
    fn errors_are_keyed_per_binding() {
        let _lock = last_errors_lock();
        set_last_error("snip", Some("被占用".to_string()));
        assert_eq!(last_error("snip").as_deref(), Some("被占用"));
        assert_eq!(
            last_error("toggle_window"),
            None,
            "截图那条的报错不该串到主窗口那条上"
        );
        set_last_error("snip", None);
        assert_eq!(last_error("snip"), None, "不注册之后不能留下过期报错");
    }

    /// 两条同时报错时各自可读 —— 设置页两行显示同一个原因就等于没说。
    ///
    /// 结束时把两格都清掉：这几条用例共用同一张进程级表，不清会把状态漏给下一条。
    #[test]
    fn two_errors_coexist_without_overwriting() {
        let _lock = last_errors_lock();
        set_last_error("snip", Some("A 占用了".to_string()));
        set_last_error("toggle_window", Some("B 占用了".to_string()));
        assert_eq!(last_error("snip").as_deref(), Some("A 占用了"));
        assert_eq!(
            last_error("toggle_window").as_deref(),
            Some("B 占用了"),
            "后写的一条把先写的那条盖掉了"
        );
        set_last_error("snip", None);
        set_last_error("toggle_window", None);
    }
}
