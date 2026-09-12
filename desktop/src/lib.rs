//! FocusFlow Tauri 桌面端后端。
//!
//! 复用 `focusflow-core` 的数据库/监听/统计/插件逻辑，
//! 通过 Tauri 命令暴露给 Web 前端；管理主窗口、悬浮窗、托盘与全局热键。

pub mod commands;
pub mod export;
pub mod hotkey;
pub mod plugins;
pub mod state;
pub mod tray;

use std::sync::Arc;

use tauri::Manager;

/// 初始化日志（复用 core 的 logger）。
pub fn init_logging() {
    focusflow_core::logger::init_logging();
}

/// 启动 Tauri 应用。
pub fn run() {
    init_logging();

    tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // 已有实例在运行时再次启动 exe：聚焦并显示主窗口，第二个进程自行退出。
            // 首个实例可能仍在启动阶段（AppState 尚未 manage）：延迟到就绪后再显示，
            // 避免启动早期在主线程建窗/阻塞 WebView2 初始化。
            let handle = app.clone();
            std::thread::Builder::new()
                .name("single-instance-delay".into())
                .spawn(move || {
                    for _ in 0..50 {
                        if handle.try_state::<Arc<crate::state::AppState>>().is_some() {
                            let h = handle.clone();
                            let _ = handle
                                .run_on_main_thread(move || crate::state::show_main_window(&h));
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(200));
                    }
                    tracing::warn!("单实例回调：AppState 长时间未就绪，放弃显示");
                })
                .expect("启动单实例延迟线程失败");
        }))
        .setup(|app| {
            // 初始化数据库、监听器、统计线程（复用 focusflow-core）
            state::AppState::init(app)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_live,
            commands::get_charts,
            commands::get_settings,
            commands::get_version,
            commands::get_vibrancy,
            commands::set_period,
            commands::get_config,
            commands::set_config,
            commands::toggle_pause,
            commands::is_paused,
            commands::show_main,
            commands::hide_main,
            commands::show_floating,
            commands::hide_floating,
            commands::flush_db,
            commands::vacuum_db,
            commands::quit,
            commands::get_plugins,
            commands::set_plugin_enabled,
            commands::plugins_watch,
            commands::dbg_log,
            commands::import_legacy,
            commands::export_report,
            plugins::get_plugin_view,
            plugins::plugin_action,
            plugins::plugin_set_field,
            commands::get_maintenance_info,
            commands::do_backup,
        ])
        .on_window_event(|window, event| {
            // 主窗口关闭 → 隐藏到托盘（500ms 后仍隐藏才销毁，见 state::hide_main_window），
            // 销毁可让任务管理器及时重新归类为后台进程；托盘"退出程序"才真正退出。
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    api.prevent_close();
                    state::hide_main_window(window.app_handle());
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("Tauri 应用构建失败")
        .run(|app_handle, event| {
            // 退出前优雅关闭数据库：flush + 备份 + 停止写线程
            if let tauri::RunEvent::Exit = event {
                if let Some(state) = app_handle.try_state::<Arc<state::AppState>>() {
                    state.db.shutdown(state.config);
                }
                // 配置去抖写盘：退出前强制落盘，避免丢失最后的设置
                let _ = focusflow_core::config::instance().save();
            }
        });
}
