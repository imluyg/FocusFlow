//! 全局热键：显示/隐藏主窗口。
//!
//! 支持运行时重新注册：设置页修改 `[hotkey] enabled/toggle_window` 后，
//! 通过 `reload_hotkey` 卸载旧的并重新注册，无需重启程序。

use tauri::{App, AppHandle, Manager};
use tauri_plugin_global_shortcut::GlobalShortcutExt;

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

/// 按当前配置注册全局热键（不先卸载；调用方负责先 unregister_all）。
/// 配置未启用或格式非法时直接返回，不注册。
fn register_current_hotkey(app: &AppHandle) {
    let config = focusflow_core::config::instance();
    let enabled = config.get_bool("hotkey", "enabled", false);
    if !enabled {
        tracing::info!("全局热键未启用，跳过注册");
        return;
    }
    let hotkey_str = config.get_or("hotkey", "toggle_window", "ctrl+shift+f");
    let s = hotkey_str.trim().to_lowercase();
    if s.is_empty() {
        tracing::warn!("热键组合为空，跳过注册");
        return;
    }

    let result = app
        .global_shortcut()
        .on_shortcut(s.as_str(), |app, _shortcut, _event| {
            toggle_main_window(app);
        });
    match result {
        Ok(()) => tracing::info!("全局热键已注册: {s}"),
        Err(e) => tracing::warn!("全局热键注册失败 ({s}): {e}"),
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
