//! 前台应用识别（反作弊安全设计）。
//!
//! 三条硬约束（docs/optimization-plan.md 批次六）：
//! 1. 绝不对其他进程调用 OpenProcess / ReadProcessMemory / WriteProcessMemory：
//!    PID→进程名解析只用 `CreateToolhelp32Snapshot` 全量快照（任务管理器同款 API），
//!    定时重建并缓存，采集时不打开任何游戏进程句柄；
//! 2. 只记进程名，不读窗口标题（反作弊会枚举窗口标题，我们不沾；也是隐私保护）；
//! 3. config.ini `[app_stats]` `exclude` 命中的进程完全不记录（隐私保险丝）。
//!
//! 职责边界：本线程**只回答「此刻前台是哪个进程」**，把结果写进写线程的 `current_app`。
//! 「用了多久」不在这里算 —— 时长由 `record` 在键鼠事件发生时按下一条
//! （`apps.entry(...) += 距上一事件的间隔`），与应用活跃时长共用 ACTIVE_GAP_SECS 门限。
//!
//! 为什么不再用「采样窗口累计」：那样必须先判断窗口是否落在活跃区间内，
//! 而活跃区间的上界是「最后一个键鼠事件」——于是只有覆盖到该事件的采样窗口才计秒，
//! 间隔 2~60 秒的静默期（看屏幕、思考）会被整段丢掉，应用总时长只剩活跃时长的四成。
//! 归属改到事件侧后，两者同源同门限：应用总时长恒 ≤ 今日活跃时长，
//! 差额只剩「当时没有已知前台应用」的时段（启动初期 / exclude 命中 / 采集瞬时失败）。
//!
//! 「恒」只对 2026-09-13 之后记的数据成立：这张表 09-12 才建（`0a317b3`），
//! 头两天换过两次口径（采样窗口 → 活跃窗口 `3fd42d2` → 事件侧 `403653d`），
//! 所以**历史库里 09-12 那天会出现 app_usage 合计 > daily_counts.seconds**
//! （采样窗口口径本来就把挂机时间算进去）。那是口径迁移的痕迹，不是有人在只让
//! `apps` 单独多写 —— 查这一族的差异前先对一下日期，别把它当数据损坏。

use std::sync::Arc;
use std::time::Duration;

use crate::config::FocusFlowConfig;
use crate::db::DbWriter;

/// 前台窗口采样周期（秒）。
///
/// 采样只做「取前台窗口 + PID 查快照」，成本极低，故取较密的 1 秒：
/// 切换应用后最多 1 秒就被感知，缩短「切过去就打字」时的归属错位窗口。
const SAMPLE_INTERVAL_SECS: u64 = 1;
/// 进程名快照刷新周期（秒）。
const SNAPSHOT_REFRESH_SECS: u64 = 30;
/// 快照「未命中后立即重建」的最小间隔（秒），防止异常 PID 造成高频重建。
const SNAPSHOT_FORCE_MIN_SECS: u64 = 1;

/// 读取 [app_stats] 配置：enabled（默认 true）+ exclude（逗号分隔的进程名清单）。
fn load_config(config: &FocusFlowConfig) -> (bool, Vec<String>) {
    (
        config.get_bool("app_stats", "enabled", true),
        parse_exclude(&config.get_or("app_stats", "exclude", "")).0,
    )
}

/// `exclude` 原始串 → 小写进程名清单（去空白、去空项）+ 被补全的项。
///
/// 比对对象是 `CreateToolhelp32Snapshot` 交回来的进程文件名，**恒带 `.exe`**；
/// 而 exclude 是 config.ini 里手写的隐私保险丝（设置页没有入口）。旧口径只精确匹配
/// 小写全名，于是少写后缀的项**静默不生效**：密码管理器全程被记录、一行提示都没有。
/// 现在缺后缀就补 `.exe`，并把补过的项交回调用方 warn 一条 —— 保险丝要么生效，
/// 要么明确说出它被补全过，不许静默。
fn parse_exclude(raw: &str) -> (Vec<String>, Vec<String>) {
    let mut names = Vec::new();
    let mut widened = Vec::new();
    for item in raw.split(',').map(|s| s.trim()) {
        if item.is_empty() {
            continue;
        }
        let lower = item.to_ascii_lowercase();
        if lower.ends_with(".exe") {
            names.push(lower);
        } else {
            names.push(format!("{lower}.exe"));
            widened.push(item.to_string());
        }
    }
    (names, widened)
}

/// 该前台应用是否可归属：exclude 命中的进程完全不记录（隐私保险丝）。
fn is_recordable(name: &str, exclude: &[String]) -> bool {
    !exclude.contains(&name.to_ascii_lowercase())
}

/// PID→进程名快照缓存（采样线程线程本地）。
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Default)]
pub(super) struct SnapshotCache {
    /// 上一次建立快照的时刻。`None` = 尚未建立（不能用 `Instant::now() - Duration`
    /// 伪造过去时刻：系统开机时间短于该 Duration 时 Instant 减法会下溢 panic）。
    at: Option<std::time::Instant>,
    names: std::collections::HashMap<u32, String>,
    /// 连续「快照取空」次数，0 = 上一次成功。用于只在本轮**第一次**失败时留痕。
    failures: u64,
    /// 最近一次失败的原因（`apply_snapshot` 的 `Err` 侧带回来的）。
    last_reason: &'static str,
}

/// 把一次进程快照的结果并进缓存。返回 `true` = **本轮第一次失败**（调用方据此 warn 一条）。
///
/// 快照取空只可能是 WinAPI 失败（`CreateToolhelp32Snapshot` 拿不到句柄、
/// `Process32FirstW` 起不来、内存/句柄压力），不存在"这台机器一个进程都没有"。
/// 旧写法是 `cache.1 = take_process_snapshot()` —— 一次瞬时失败就把整张
/// PID→名字表换成空 map，于是每个前台进程都查不到名字，应用排行长期空白，
/// 而它与「用户真的没在用任何应用」在界面上完全不可区分，错误本身一个字没露
/// （§二 B15「必须留痕」同族）。现在：旧表原样留着（名字稍旧好过全空），
/// 失败计数，并在第一次失败时 warn 一条带原因的。
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn apply_snapshot(
    cache: &mut SnapshotCache,
    fresh: Result<std::collections::HashMap<u32, String>, &'static str>,
) -> bool {
    match fresh {
        Ok(map) if !map.is_empty() => {
            cache.names = map;
            cache.failures = 0;
            false
        }
        Ok(_) => register_failure(cache, "快照里没有任何进程"),
        Err(reason) => register_failure(cache, reason),
    }
}

fn register_failure(cache: &mut SnapshotCache, reason: &'static str) -> bool {
    cache.failures += 1;
    cache.last_reason = reason;
    // 连续第 2、3… 次不再返回 true：持久失败时采样每轮都跑，
    // 逐条 warn 会把日志刷满，而情况本身并没有变化。
    cache.failures == 1
}

/// 启动前台应用识别线程（非 Windows 平台恒返回 None，等同于不归属）。
///
/// 返回自检结论（B15）：线程创建失败（机器长时间运行、句柄压力）只该少掉
/// 「应用归属」这一项，不该让整个程序在启动路上死掉 —— release 是
/// panic = "abort"，一条 expect 就是"双击没反应"。但**必须留痕**：
/// 原来结论吞在自己函数里，只有翻日志才知道应用统计悄悄没了。
pub fn start_sampler(writer: Arc<DbWriter>) -> crate::startup::CheckResult {
    let step = "前台应用采样线程";
    let (enabled, exclude) = load_config(crate::config::instance());
    if !enabled {
        tracing::info!("前台应用统计未启用（[app_stats] enabled=false），线程仍会起来等它被打开");
    }
    match std::thread::Builder::new()
        .name("app-usage-sampler".into())
        .spawn(move || {
            tracing::info!(
                "前台应用识别已启动（每 {SAMPLE_INTERVAL_SECS} 秒采样，进程快照每 {SNAPSHOT_REFRESH_SECS} 秒刷新）"
            );
            let mut exclude = exclude;
            let mut cached_raw = String::new();
            let mut was_enabled = enabled;
            if !was_enabled {
                writer.set_current_app(None);
            }
            loop {
                std::thread::sleep(Duration::from_secs(SAMPLE_INTERVAL_SECS));
                let config = crate::config::instance();
                // 每一轮都重读 `[app_stats]`。以前这两项只在 spawn 之前读一次，于是：
                // ① 事后把 `keepassxc.exe` 加进 exclude 完全不生效 —— 那款软件会继续
                //    被记录，而 exclude 的语义是"完全不记录"；一条隐私保险丝静默失效，
                //    日志里连"要重启"都不会提示；② 事后 enabled=false 也停不下来。
                // 配置是内存快照 + 去抖落盘，每秒读一次的成本可忽略；原始串没变就
                // 不必重复 split/分配。
                let raw = config.get_or("app_stats", "exclude", "");
                if raw != cached_raw {
                    let (names, widened) = parse_exclude(&raw);
                    // 只在原始串真的变过时 warn：每轮都 warn 就是一秒一条刷屏
                    if !widened.is_empty() {
                        tracing::warn!(
                            "[app_stats] exclude 里这些项没写 .exe，已按进程名补全后生效：{}",
                            widened.join(", ")
                        );
                    }
                    exclude = names;
                    cached_raw = raw;
                }
                let enabled_now = config.get_bool("app_stats", "enabled", true);
                if enabled_now != was_enabled {
                    tracing::info!(
                        "前台应用统计已切换为 {}",
                        if enabled_now { "启用" } else { "停用" }
                    );
                    was_enabled = enabled_now;
                }
                if !enabled_now {
                    // 停用的当下就清掉归属，否则这段时长会记到上一个应用头上
                    writer.set_current_app(None);
                    continue;
                }
                match collect::foreground_app_name() {
                    Some(n) if is_recordable(&n, &exclude) => writer.set_current_app(Some(&n)),
                    // exclude 命中：按其语义「完全不记录」，
                    // 同时清空归属，避免这段时长被记到上一个应用头上
                    Some(_) => writer.set_current_app(None),
                    // 采集失败（锁屏、窗口切换瞬间、进程名暂未解析）：同样清空归属。
                    // 宁可少记也不张冠李戴，差额会体现为「应用总时长 < 活跃时长」。
                    None => writer.set_current_app(None),
                }
            }
        })
    {
        Ok(_handle) => crate::startup::CheckResult::ok(
            step,
            if enabled {
                "已启动"
            } else {
                "未启用（[app_stats] enabled=false），线程在等它被打开"
            },
        ),
        Err(e) => {
            // 线程创建失败（机器长时间运行、句柄压力）只该少掉「应用归属」这一项，
            // 不该让整个程序在启动路上死掉 —— release 是 panic = "abort"，
            // 一条 expect 就是"双击没反应"。
            tracing::error!(
                "前台应用采样线程启动失败（{e}）：本次运行不再统计应用归属，键鼠统计不受影响"
            );
            crate::startup::CheckResult::fail(step, format!("{e}"))
        }
    }
}

/// 前台进程名采集（平台相关）。
mod collect {
    use super::{apply_snapshot, SnapshotCache, SNAPSHOT_FORCE_MIN_SECS, SNAPSHOT_REFRESH_SECS};
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

    /// PID→进程名，走线程本地的快照缓存。
    ///
    /// 缓存未命中时立即重建一次快照再查：快照每 30 秒才刷新一次，而窗口进程
    /// 常常是刚启动的（新开的窗口、UWP 的 ApplicationFrameHost 子进程），
    /// 只靠定期刷新会让这些应用在前 30 秒里查不到名字、整段不归属。
    #[cfg(windows)]
    fn pid_to_name(pid: u32) -> Option<String> {
        use std::cell::RefCell;
        thread_local! {
            static SNAPSHOT: RefCell<SnapshotCache> = RefCell::new(SnapshotCache::default());
        }
        SNAPSHOT.with(|slot| {
            let mut cache = slot.borrow_mut();
            let stale = cache
                .at
                .map(|t| t.elapsed() >= Duration::from_secs(SNAPSHOT_REFRESH_SECS))
                .unwrap_or(true);
            if stale {
                cache.at = Some(std::time::Instant::now());
                let first_failure = apply_snapshot(&mut cache, take_process_snapshot());
                warn_once(&cache, first_failure);
            }
            if let Some(name) = cache.names.get(&pid) {
                return Some(name.clone());
            }
            // 未命中 → 立即重建再查一次；带最小间隔节流，
            // 避免「PID 查不到」持续存在时每次都做一遍全进程遍历。
            let can_force = cache
                .at
                .map(|t| t.elapsed() >= Duration::from_secs(SNAPSHOT_FORCE_MIN_SECS))
                .unwrap_or(true);
            if !can_force {
                return None;
            }
            cache.at = Some(std::time::Instant::now());
            let first_failure = apply_snapshot(&mut cache, take_process_snapshot());
            warn_once(&cache, first_failure);
            cache.names.get(&pid).cloned()
        })
    }

    /// 采集侧唯一的留痕出口：只在连续失败的第一次 warn，之后由清零收尾。
    #[cfg(windows)]
    fn warn_once(cache: &SnapshotCache, first_failure: bool) {
        if first_failure {
            tracing::warn!(
                "进程名快照取空（{}）：应用归属暂沿用上一张表，键鼠统计不受影响",
                cache.last_reason
            );
        }
    }

    /// 全量进程快照（CreateToolhelp32Snapshot，任务管理器同款 API，
    /// 不对任何进程 OpenProcess 开句柄）。
    ///
    /// 失败侧要把原因交出去：`if let Ok(..)` / `.is_ok()` 那种写法两个错误变量都
    /// 不接名字，外面只看得见"查不到名字"，看不见是快照句柄没拿到还是遍历没起来。
    #[cfg(windows)]
    fn take_process_snapshot() -> Result<std::collections::HashMap<u32, String>, &'static str> {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        };
        let mut map = std::collections::HashMap::new();
        unsafe {
            let Ok(handle) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
                return Err("CreateToolhelp32Snapshot 失败");
            };
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            if Process32FirstW(handle, &mut entry).is_err() {
                let _ = CloseHandle(handle);
                return Err("Process32FirstW 失败");
            }
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
            let _ = CloseHandle(handle);
        }
        Ok(map)
    }

    /// 非 Windows 平台：暂不支持前台应用统计。
    #[cfg(not(windows))]
    pub fn foreground_app_name() -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_snapshot, is_recordable, parse_exclude, SnapshotCache};
    use std::collections::HashMap;

    /// exclude 命中即不归属，且大小写不敏感。
    #[test]
    fn exclude_blocks_matching_process() {
        let exclude = vec!["keepassxc.exe".to_string(), "taskmgr.exe".to_string()];
        assert!(is_recordable("Obsidian.exe", &exclude));
        assert!(!is_recordable("KeePassXC.exe", &exclude));
        assert!(!is_recordable("KEEPASSXC.EXE", &exclude));
        assert!(!is_recordable("taskmgr.exe", &exclude));
    }

    /// exclude 清单的解析口径：逗号分隔、去空白、统一小写、忽略空项。
    ///
    /// 采样循环现在每一轮都会重读这个串（隐私保险丝必须当场生效），所以解析这步
    /// 得自己站得住。
    #[test]
    fn parse_exclude_normalizes_entries() {
        let (ex, widened) = parse_exclude("KeePassXC.exe, taskmgr.exe");
        assert_eq!(
            ex,
            vec!["keepassxc.exe".to_string(), "taskmgr.exe".to_string()]
        );
        assert!(widened.is_empty(), "写全了后缀的项不该被报成补过");
        assert!(parse_exclude("").0.is_empty());
        assert!(parse_exclude(" , , ").0.is_empty());
        let ex = parse_exclude("  KEEPASSXC.EXE  ").0;
        assert!(!is_recordable("keepassxc.exe", &ex));
    }

    /// 少写 `.exe` 曾经是**静默**不生效（密码管理器全程被记录、零提示）：
    /// 现在补全后缀，并把补过的项交回调用方 warn 一条。
    #[test]
    fn exclude_entry_without_exe_suffix_is_widened_and_reported() {
        let (ex, widened) = parse_exclude("KeePassXC, taskmgr.exe , obsidian");
        assert_eq!(
            ex,
            vec![
                "keepassxc.exe".to_string(),
                "taskmgr.exe".to_string(),
                "obsidian.exe".to_string()
            ]
        );
        assert_eq!(
            widened,
            vec!["KeePassXC".to_string(), "obsidian".to_string()],
            "只有真的补过后缀的项进报告"
        );
        assert!(!is_recordable("KeePassXC.exe", &ex), "补全后必须真的拦住");
        assert!(
            is_recordable("keepass.exe", &ex),
            "补后缀不能顺手扩大命中面（前缀匹配会误伤同类进程）"
        );
    }

    /// 空 exclude（默认配置）时全部可归属。
    #[test]
    fn empty_exclude_allows_everything() {
        let exclude: Vec<String> = Vec::new();
        assert!(is_recordable("msedge.exe", &exclude));
    }

    /// 快照取空 = WinAPI 失败，绝不能把整张 PID→名字表换空：一次瞬时失败会让
    /// 应用排行长期空白，且与「用户真的没在用任何应用」不可区分（B15 同族）。
    #[test]
    fn empty_snapshot_keeps_previous_names_and_warns_once() {
        let mut cache = SnapshotCache::default();
        let mut base = HashMap::new();
        base.insert(1u32, "KeePassXC.exe".to_string());
        base.insert(2u32, "notepad.exe".to_string());
        assert!(!apply_snapshot(&mut cache, Ok(base.clone())));
        assert_eq!(cache.names.len(), 2);

        // 第一次失败：报出去（调用方据此 warn 一条），旧表原样留着
        assert!(apply_snapshot(
            &mut cache,
            Err("CreateToolhelp32Snapshot 失败")
        ));
        assert_eq!(cache.names, base, "取空不得清空上一张表");
        assert_eq!(cache.last_reason, "CreateToolhelp32Snapshot 失败");

        // 连续失败只计数，不再重复报：采样每轮都跑，逐条 warn 会把日志刷满
        assert!(!apply_snapshot(&mut cache, Err("Process32FirstW 失败")));
        assert_eq!(cache.failures, 2);
        assert_eq!(cache.names, base);

        // 恢复：换上新表、计数清零
        let mut fresh = HashMap::new();
        fresh.insert(7u32, "obsidian.exe".to_string());
        assert!(!apply_snapshot(&mut cache, Ok(fresh)));
        assert_eq!(cache.failures, 0);
        assert_eq!(cache.names.get(&1), None);
        assert_eq!(
            cache.names.get(&7).map(String::as_str),
            Some("obsidian.exe")
        );

        // Ok 但是空 map 同样按失败处理：机器上不可能一个进程都没有
        let mut cold = SnapshotCache::default();
        assert!(apply_snapshot(&mut cold, Ok(HashMap::new())));
        assert!(cold.names.is_empty(), "还没建立过表时缓存保持为空");
    }
}
