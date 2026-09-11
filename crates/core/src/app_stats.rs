//! 前台应用使用时长统计（反作弊安全设计）。
//!
//! 三条硬约束（docs/optimization-plan.md 批次六）：
//! 1. 绝不对其他进程调用 OpenProcess / ReadProcessMemory / WriteProcessMemory：
//!    PID→进程名解析只用 `CreateToolhelp32Snapshot` 全量快照（任务管理器同款 API），
//!    每 30 秒重建一次并缓存，采集时不打开任何游戏进程句柄；
//! 2. 只记进程名，不读窗口标题（反作弊会枚举窗口标题，我们不沾；也是隐私保护）；
//! 3. config.ini `[app_stats]` `exclude` 命中的进程完全不记录（隐私保险丝）。

use std::sync::Arc;
use std::time::Duration;

use crate::config::FocusFlowConfig;
use crate::db::queries;
use crate::db::DbWriter;

/// 采集周期：每 2 秒给当前前台应用累计 2 秒。
const SAMPLE_INTERVAL_SECS: u64 = 2;
/// 进程名快照刷新周期（秒）。
const SNAPSHOT_REFRESH_SECS: u64 = 30;

/// 读取 [app_stats] 配置：enabled（默认 true）+ exclude（逗号分隔的进程名清单）。
fn load_config(config: &FocusFlowConfig) -> (bool, Vec<String>) {
    let exclude = config
        .get_or("app_stats", "exclude", "")
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    (config.get_bool("app_stats", "enabled", true), exclude)
}

/// 启动前台应用采集线程（非 Windows 平台为空实现）。
///
/// 独立线程每 2 秒采样一次前台窗口，把秒数累计进写线程内存聚合，
/// 随写线程周期 flush 落库到 app_usage 表。
pub fn start_sampler(writer: Arc<DbWriter>) {
    let config = crate::config::instance();
    let (enabled, exclude) = load_config(config);
    if !enabled {
        tracing::info!("前台应用统计未启用（[app_stats] enabled=false）");
        return;
    }
    std::thread::Builder::new()
        .name("app-usage-sampler".into())
        .spawn(move || {
            tracing::info!(
                "前台应用采集已启动（每 {SAMPLE_INTERVAL_SECS} 秒采样，进程快照每 {SNAPSHOT_REFRESH_SECS} 秒刷新）"
            );
            loop {
                std::thread::sleep(Duration::from_secs(SAMPLE_INTERVAL_SECS));
                let Some(name) = collect::foreground_app_name() else {
                    continue;
                };
                let lower = name.to_ascii_lowercase();
                if exclude.contains(&lower) {
                    continue;
                }
                let dk = queries::day_key_of_ts(queries::now_ts());
                writer.add_app_seconds(dk, &name, SAMPLE_INTERVAL_SECS as i64);
            }
        })
        .expect("启动前台应用采集线程失败");
}

/// 前台进程名采集（平台相关）。
mod collect {
    use super::SNAPSHOT_REFRESH_SECS;
    use std::time::Duration;

    /// 前台窗口所属进程的 exe 文件名（不读窗口标题）。
    #[cfg(windows)]
    pub fn foreground_app_name() -> Option<String> {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{
            GetForegroundWindow, GetWindowThreadProcessId,
        };
        unsafe {
            let hwnd: HWND = GetForegroundWindow();
            if hwnd.0.is_null() {
                return None;
            }
            let mut pid: u32 = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == 0 {
                return None;
            }
            pid_to_name(pid)
        }
    }

    /// PID→进程名，走线程本地的 30 秒快照缓存。
    #[cfg(windows)]
    fn pid_to_name(pid: u32) -> Option<String> {
        use std::cell::RefCell;
        thread_local! {
            static SNAPSHOT: RefCell<(std::time::Instant, std::collections::HashMap<u32, String>)> =
                RefCell::new((
                    std::time::Instant::now() - Duration::from_secs(SNAPSHOT_REFRESH_SECS * 2),
                    std::collections::HashMap::new(),
                ));
        }
        SNAPSHOT.with(|cache| {
            let mut cache = cache.borrow_mut();
            if cache.0.elapsed() >= Duration::from_secs(SNAPSHOT_REFRESH_SECS) {
                cache.0 = std::time::Instant::now();
                cache.1 = take_process_snapshot();
            }
            cache.1.get(&pid).cloned()
        })
    }

    /// 全量进程快照（CreateToolhelp32Snapshot，任务管理器同款 API，
    /// 不对任何进程 OpenProcess 开句柄）。
    #[cfg(windows)]
    fn take_process_snapshot() -> std::collections::HashMap<u32, String> {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        };
        let mut map = std::collections::HashMap::new();
        unsafe {
            if let Ok(handle) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
                let mut entry = PROCESSENTRY32W {
                    dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                    ..Default::default()
                };
                if Process32FirstW(handle, &mut entry).is_ok() {
                    loop {
                        let len = entry
                            .szExeFile
                            .iter()
                            .position(|c| *c == 0)
                            .unwrap_or(entry.szExeFile.len());
                        let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
                        if !name.is_empty() {
                            map.insert(entry.th32ProcessID, name);
                        }
                        if Process32NextW(handle, &mut entry).is_err() {
                            break;
                        }
                    }
                }
                let _ = CloseHandle(handle);
            }
        }
        map
    }

    /// 非 Windows 平台：暂不支持前台应用统计。
    #[cfg(not(windows))]
    pub fn foreground_app_name() -> Option<String> {
        None
    }
}
