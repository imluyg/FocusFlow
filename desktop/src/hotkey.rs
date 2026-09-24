//! 全局热键：显示/隐藏主窗口。
//!
//! 支持运行时重新注册：设置页修改 `[hotkey] enabled/toggle_window` 后，
//! 通过 `reload_hotkey` 卸载旧的并重新注册，无需重启程序。

use std::sync::Mutex;

use tauri::{App, AppHandle, Manager};
use tauri_plugin_global_shortcut::GlobalShortcutExt;

/// 最近一次热键注册失败的原因（`None` = 注册成功或已关闭热键）。
///
/// 必须存住而不能只发事件：启动时的注册早于前端首次读设置，事件那时没有
/// 监听者；而组合键被其它程序占用是最常见的静默失败 —— 设置页显示着
/// 「已启用」，按下去却毫无反应，用户没有任何线索。
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

fn set_last_error(msg: Option<String>) {
    *LAST_ERROR.lock().unwrap_or_else(|e| e.into_inner()) = msg;
}

/// 供设置页展示的最近一次注册失败原因。
pub fn last_error() -> Option<String> {
    LAST_ERROR.lock().unwrap_or_else(|e| e.into_inner()).clone()
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

/// 该注册成哪个热键；`None` = 本次不注册（未启用，或用户把输入框清空了）。
///
/// 刻意用 `get` 而不是 `get_or(..., "ctrl+shift+f")`：`get_or` 把**空串当"没配"**
/// （见 `config.rs::get_or`），于是在设置页里清空热键框 —— 那是"我不想让程序占用
/// 任何全局快捷键"的正常表达 —— 会被读成默认组合：框是空的、开关还勾着、
/// `is_empty()` 那条分支永远到不了，而 Ctrl+Shift+F 已经被悄悄全局占用。
///
/// 默认组合本来就写在 `default_config()` 里、首次落盘一定带上，所以"键不存在"与
/// "用户主动清空"这两件事用 `get` 才分得开，这里也不必再兜一次默认值。
fn pending_hotkey(config: &focusflow_core::config::FocusFlowConfig) -> Option<String> {
    if !config.get_bool("hotkey", "enabled", false) {
        tracing::info!("全局热键未启用，跳过注册");
        return None;
    }
    let spec = config.get("hotkey", "toggle_window").trim().to_lowercase();
    if spec.is_empty() {
        tracing::warn!("热键组合已被清空，本次不注册任何全局热键（要恢复请把组合键填回去）");
        return None;
    }
    Some(spec)
}

/// 按当前配置注册全局热键（不先卸载；调用方负责先 unregister_all）。
/// 配置未启用或被清空时直接返回，不注册。
fn register_current_hotkey(app: &AppHandle) {
    let config = focusflow_core::config::instance();
    let Some(s) = pending_hotkey(config) else {
        // 不注册的两条路都算"没有报错"：关掉开关后不能留着上一次的失败提示，
        // 否则设置页会显示一条已过期的占用报错。
        set_last_error(None);
        return;
    };

    let result = app
        .global_shortcut()
        .on_shortcut(s.as_str(), |app, _shortcut, _event| {
            toggle_main_window(app);
        });
    match result {
        Ok(()) => {
            set_last_error(None);
            tracing::info!("全局热键已注册: {s}");
        }
        Err(e) => {
            let msg = format!("{s} 注册失败，可能被其它程序占用：{e}");
            tracing::warn!("全局热键注册失败 ({s}): {e}");
            set_last_error(Some(msg));
        }
    }
}

/// 启动时注册全局热键。
pub fn setup_hotkey(app: &App) {
    register_current_hotkey(app.handle());
}

/// 运行时重新加载热键：先卸载全部（本程序只注册一个全局热键），
/// 再按最新配置重新注册。设置页改动热键后调用。
pub fn reload_hotkey(app: &AppHandle) {
    // 先卸载旧的，避免重复注册同一快捷键
    match app.global_shortcut().unregister_all() {
        Ok(()) => tracing::debug!("已卸载全部全局热键"),
        Err(e) => tracing::warn!("卸载全局热键失败: {e}"),
    }
    register_current_hotkey(app);
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

    /// 清空热键输入框 = 不占用任何全局快捷键。
    ///
    /// 旧写法 `get_or(..., "ctrl+shift+f")` 把空串当"没配"，所以框清空后
    /// 实际注册的是默认组合：界面显示空白、开关还勾着，Ctrl+Shift+F 却被程序占住。
    #[test]
    fn cleared_or_absent_spec_means_no_hotkey() {
        assert_eq!(
            pending_hotkey(&cfg(
                "cleared",
                "[hotkey]\nenabled = true\ntoggle_window = "
            )),
            None,
            "被清空的输入框不该被读成默认组合"
        );
        assert_eq!(
            pending_hotkey(&cfg(
                "blank",
                "[hotkey]\nenabled = true\ntoggle_window =    "
            )),
            None,
            "只有空格也算清空"
        );
        // 与"清空"相对：文件里**压根没有**这一行时要退回默认值。`load()` 是从
        // `default_config()` 起步再被文件覆盖的，所以缺行不等于空串 —— 老配置文件
        // 升级上来仍然有热键可用。以前 `get_or` 把这两件事压成同一件。
        assert_eq!(
            pending_hotkey(&cfg("absent", "[hotkey]\nenabled = true")).as_deref(),
            Some("ctrl+shift+f"),
            "缺行 ≠ 清空：仍该用 default_config 里那句默认组合"
        );
    }

    /// 正常路径仍然生效，并且大小写/空白都被归一。
    #[test]
    fn configured_spec_is_used_verbatim_after_normalize() {
        assert_eq!(
            pending_hotkey(&cfg(
                "normal",
                "[hotkey]\nenabled = true\ntoggle_window = Ctrl+Alt+M"
            ))
            .as_deref(),
            Some("ctrl+alt+m"),
            "注册前要小写化并去掉首尾空白"
        );
        assert_eq!(
            pending_hotkey(&cfg(
                "off",
                "[hotkey]\nenabled = false\ntoggle_window = ctrl+alt+m"
            )),
            None,
            "开关关掉时不该碰组合键"
        );
        // `enabled = 1` 也是"启用"：这一族在 get_bool 里是认的
        assert_eq!(
            pending_hotkey(&cfg(
                "one",
                "[hotkey]\nenabled = 1\ntoggle_window = ctrl+alt+j"
            ))
            .as_deref(),
            Some("ctrl+alt+j"),
            "1/yes/on 与 true 同义（get_bool 的口径）"
        );
    }
}
