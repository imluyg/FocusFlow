//! 前台应用使用时长统计（反作弊安全设计）。
//!
//! 三条硬约束（docs/optimization-plan.md 批次六）：
//! 1. 绝不对其他进程调用 OpenProcess / ReadProcessMemory / WriteProcessMemory：
//!    PID→进程名解析只用 `CreateToolhelp32Snapshot` 全量快照（任务管理器同款 API），
//!    每 30 秒重建一次并缓存，采集时不打开任何游戏进程句柄；
//! 2. 只记进程名，不读窗口标题（反作弊会枚举窗口标题，我们不沾；也是隐私保护）；
//! 3. config.ini `[app_stats]` `exclude` 命中的进程完全不记录（隐私保险丝）。

use std::sync::Arc;
use std::time::{Duration, Instant};

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
            // 基准取自循环开始（sleep 之前），首个窗口长度自然 = 一个采样周期
            let mut last_sample = Instant::now();
            loop {
                std::thread::sleep(Duration::from_secs(SAMPLE_INTERVAL_SECS));
                // 用真实经过时间而非常量：sleep 有漂移，按实际窗口折算才不会系统性多计
                let elapsed = last_sample.elapsed().as_secs() as i64;
                last_sample = Instant::now();
                let Some(name) = collect::foreground_app_name() else {
                    continue;
                };
                // 只累计"活跃时长已覆盖"的秒数：活跃时长记到最后一个键鼠事件为止，
                // 因此本采样窗口 [now-elapsed, now) 与 [.., last_event_ts] 取交集，
                // 停手后的空档（挂机）不再白送时长，应用总时长恒 ≤ 今日活跃时长。
                let now = queries::now_ts();
                let seconds = overlap_seconds(now, elapsed, writer.last_event_ts());
                if seconds <= 0 {
                    continue;
                }
                let lower = name.to_ascii_lowercase();
                if exclude.contains(&lower) {
                    continue;
                }
                let dk = queries::day_key_of_ts(now);
                writer.add_app_seconds(dk, &name, seconds);
            }
        })
        .expect("启动前台应用采集线程失败");
}

/// 本采样窗口 `[now - elapsed, now)` 与活跃时长已覆盖区间 `[.., last_event_ts]` 的交集秒数。
///
/// 活跃时长在最后一个键鼠事件处停止累计，所以超出 `last_event_ts` 的部分一律不计，
/// 停手后的空档最多多算一个采样周期（≤ 采样粒度），不再白送 60 秒。
fn overlap_seconds(now: i64, elapsed: i64, last_event_ts: i64) -> i64 {
    if elapsed <= 0 || last_event_ts <= 0 {
        return 0;
    }
    let start = now - elapsed;
    // 窗口整体落在最后事件之后 → 已脱离活跃区间
    if start >= last_event_ts {
        return 0;
    }
    (now.min(last_event_ts) - start).clamp(0, elapsed)
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

#[cfg(test)]
mod tests {
    use super::overlap_seconds;

    /// 采样窗口完全落在最后事件之前：整窗计入（活跃时长已覆盖）。
    #[test]
    fn overlap_full_window_when_still_active() {
        // 窗口 [100, 102)，最后事件在 110（尚未到达，说明刚有键鼠活动）
        assert_eq!(overlap_seconds(102, 2, 110), 2);
    }

    /// 最后事件落在窗口内：只计到事件时刻，尾巴不计。
    #[test]
    fn overlap_clips_to_last_event() {
        // 窗口 [100, 102)，最后事件 101 → 交集 1 秒
        assert_eq!(overlap_seconds(102, 2, 101), 1);
    }

    /// 窗口完全在最后事件之后（停手/挂机）：不计。
    #[test]
    fn overlap_zero_after_idle() {
        assert_eq!(overlap_seconds(200, 2, 100), 0);
        // 边界：窗口起点恰为最后事件时刻
        assert_eq!(overlap_seconds(102, 2, 100), 0);
    }

    /// 无事件或非法窗口：不计。
    #[test]
    fn overlap_zero_without_events() {
        assert_eq!(overlap_seconds(102, 2, 0), 0);
        assert_eq!(overlap_seconds(102, 0, 200), 0);
    }

    /// 折算结果永不超过窗口长度（防止重复/溢出累计）。
    #[test]
    fn overlap_never_exceeds_window() {
        for elapsed in 1..=5i64 {
            for last in 0..=210i64 {
                let s = overlap_seconds(200, elapsed, last);
                assert!((0..=elapsed).contains(&s), "elapsed={elapsed} last={last} s={s}");
            }
        }
    }
}
