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

use std::time::{Duration, Instant};

use tauri::Manager;

/// 自动重启参数：新进程先在门口等着，直到旧进程真正退出再初始化。
///
/// 旧进程退出前仍持有单实例守卫（互斥体 + 隐藏窗口）与 WebView2 用户数据目录。
/// 新进程若在此刻初始化，会被单实例插件当成"第二个实例"转发后自杀，而旧进程
/// 正在退出、收到转发也显示不出窗口——结果是两个进程都没了（表现为程序消失、
/// 面板点不开）。旧进程退出耗时取决于 flush/备份，长短不定，所以这是个概率性
/// 故障：有时重启成功、有时整个程序都没了。
const ARG_WAIT_PID: &str = "--wait-pid";
/// 自动重启参数：启动后自动显示主窗口（自动重启本就是"用户想打开面板"触发的）。
const ARG_SHOW_MAIN: &str = "--show-main";
/// 等旧进程退出的上限：正常情况下只需几十毫秒，超时说明旧进程卡在退出路径上。
const WAIT_PID_TIMEOUT: Duration = Duration::from_secs(15);

/// 初始化日志（复用 core 的 logger）。
pub fn init_logging() {
    focusflow_core::logger::init_logging();
}

/// 取命令行 `--wait-pid <pid>` 里的 PID。
fn wait_pid_arg() -> Option<u32> {
    let mut args = std::env::args();
    while let Some(arg) = args.next() {
        if arg == ARG_WAIT_PID {
            return args.next().and_then(|v| v.parse().ok());
        }
    }
    None
}

/// 进程是否仍在运行（Win32 `STILL_ACTIVE`）。
#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    /// Win32 `STILL_ACTIVE`（未退出时 GetExitCodeProcess 返回的哨兵值）
    const STILL_ACTIVE: u32 = 259;
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            // 打不开句柄即进程已不存在（无权查询同样按"已退出"处理，比空等更好）
            return false;
        };
        let mut code = 0u32;
        let alive = GetExitCodeProcess(handle, &mut code).is_ok() && code == STILL_ACTIVE;
        let _ = CloseHandle(handle);
        alive
    }
}

#[cfg(not(windows))]
fn process_alive(_pid: u32) -> bool {
    false
}

/// 等旧进程退出（自动重启专用，见 `ARG_WAIT_PID`）。
fn wait_previous_process_exit(pid: u32) {
    let start = Instant::now();
    while start.elapsed() < WAIT_PID_TIMEOUT {
        if !process_alive(pid) {
            tracing::info!(
                "自动重启：旧进程 {pid} 已退出（等待 {} ms），开始初始化",
                start.elapsed().as_millis()
            );
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    tracing::warn!(
        "自动重启：等待旧进程 {pid} 退出超时（{} 秒），继续启动（可能被单实例守卫拦截）",
        WAIT_PID_TIMEOUT.as_secs()
    );
}

/// 启动 Tauri 应用。
pub fn run() {
    init_logging();

    // 自动重启链上的新进程：先等旧进程走干净再往下走
    if let Some(pid) = wait_pid_arg() {
        wait_previous_process_exit(pid);
    }
    let show_main_on_start = std::env::args().any(|arg| arg == ARG_SHOW_MAIN);

    tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // 已有实例在运行时再次启动 exe：显示主窗口，第二个进程自行退出。
            // 首个实例可能仍在启动阶段（AppState 尚未 manage）：延迟到就绪后再显示，
            // 避免启动早期在主线程建窗/阻塞 WebView2 初始化。
            state::show_main_window_when_ready(app.clone(), Duration::ZERO);
        }))
        .setup(move |app| {
            // 初始化数据库、监听器、统计线程（复用 focusflow-core）
            state::AppState::init(app)?;
            // 自动重启（--show-main）：用户点了面板才触发的重启，
            // 起来后隔 2 秒把面板显示出来，免得用户对着托盘再点一次。
            if show_main_on_start {
                state::show_main_window_when_ready(app.handle().clone(), Duration::from_secs(2));
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_live,
            commands::get_charts,
            commands::get_settings,
            commands::get_version,
            commands::get_vibrancy,
            commands::set_period,
            commands::set_device_alias,
            commands::get_device_detail,
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
                if let Some(state) = app_handle.try_state::<std::sync::Arc<state::AppState>>() {
                    state.db.shutdown(state.config);
                }
                // 配置去抖写盘：退出前强制落盘，避免丢失最后的设置
                let _ = focusflow_core::config::instance().save();
            }
        });
}
