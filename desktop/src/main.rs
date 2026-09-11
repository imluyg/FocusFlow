//! FocusFlow Tauri 桌面端入口。
//!
//! 逻辑在 `lib.rs`（便于测试与复用）；此处仅启动应用。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    focusflow_desktop::run();
}
