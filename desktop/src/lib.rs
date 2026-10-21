//! FocusFlow Tauri 桌面端后端。
//!
//! 复用 `focusflow-core` 的数据库/监听/统计/插件逻辑，
//! 通过 Tauri 命令暴露给 Web 前端；管理主窗口、悬浮窗、托盘与全局热键。

pub mod commands;
pub mod export;
pub mod hotkey;
pub mod pin;
pub mod plugins;
pub mod reveal;
pub mod snip;
pub mod state;
pub mod tray;

/// 测试专用：凡是调用 `paths::set_app_dir` 的用例都必须持有此锁跑完全程。
///
/// app_dir 是进程级全局，而 cargo 默认并行跑用例：不串行时 A 用例刚把目录换成
/// 自己造的年度库，B 用例正在查的就已经是 A 的数据（本 crate 曾因此出现
/// 「空库应为 0」读到 42 万的偶发失败）。core 侧的同名锁是 `#[cfg(test)]`，
/// 跨 crate 拿不到，所以这里各留一份。
#[cfg(test)]
pub(crate) fn app_dir_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

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

/// 一次进程状态探测的三种结局。
///
/// `Unknown` 必须单独存在：旧写法只有 `bool`，把"查不到"折进"没活着"，于是那条轮询
/// 一遇到查不动的 pid 就当它已经退出（见 [`open_process_says_gone`]）。
#[derive(Debug)]
enum Probe {
    /// 确认已经退出
    Gone,
    /// 确认还活着
    Alive,
    /// 查不动（句柄开不了、调用失败），原因带在字符串里
    Unknown(String),
}

/// `OpenProcess` 失败时，这个错误码是不是"那个进程确实不在了"。
///
/// 只有 `ERROR_INVALID_PARAMETER`（87）算数：旧进程退出之后 `OpenProcess` 就是这么失败，
/// 它是这条轮询**正常收尾的信号**。其余失败说明不了"退出了" —— 最典型的是
/// `ERROR_ACCESS_DENIED`（5，目标是提权或受保护的进程，句柄开不了），把它折成
/// "已退出"就等于"一查不动就当场往下初始化"，撞上新进程被单实例守卫转发后自行退出、
/// 旧进程又在退出路上收不到转发 —— 上面 `ARG_WAIT_PID` 那段记的正是"两个进程都没了"。
/// 与同仓 `app_stats.rs` 的 `toolhelp_next_is_end` 同一个口径：**只有一个错误码算走到头**。
fn open_process_says_gone(code: u32) -> bool {
    code == 87
}

/// 进程是否仍在运行（Win32 `STILL_ACTIVE`）。
#[cfg(windows)]
fn process_probe(pid: u32) -> Probe {
    use windows::Win32::Foundation::{CloseHandle, GetLastError};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    /// Win32 `STILL_ACTIVE`（未退出时 GetExitCodeProcess 返回的哨兵值）
    const STILL_ACTIVE: u32 = 259;
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            let code = GetLastError().0;
            return if open_process_says_gone(code) {
                Probe::Gone
            } else {
                Probe::Unknown(format!("OpenProcess 失败（错误码 {code}）"))
            };
        };
        let mut code = 0u32;
        // `GetExitCodeProcess` 的返回值在这里是**证据**，不能只拿来当"成功过"用：
        // 旧写法 `is_ok() && code == STILL_ACTIVE` 把调用失败也折成"没活着"。
        // 另一头的已知毛边留着（并说明过）：真退出码恰好是 259 的进程会被当成还活着，
        // 那只会让这一次多等到超时再出声，比反过来早放行安全。
        let verdict = match GetExitCodeProcess(handle, &mut code) {
            Ok(()) if code == STILL_ACTIVE => Probe::Alive,
            Ok(()) => Probe::Gone,
            Err(e) => Probe::Unknown(format!("GetExitCodeProcess 失败：{e}")),
        };
        let _ = CloseHandle(handle);
        verdict
    }
}

#[cfg(not(windows))]
fn process_probe(_pid: u32) -> Probe {
    // 非 Windows 上没有那套互斥体 + 隐藏窗口的单实例守卫，没有要等的东西
    Probe::Gone
}

/// 等旧进程退出（自动重启专用，见 `ARG_WAIT_PID`）。
fn wait_previous_process_exit(pid: u32) {
    let start = Instant::now();
    // 最后一直查不到 ≠ 确认它还活着：收尾那两句话得分开说
    let mut last_unknown: Option<String> = None;
    while start.elapsed() < WAIT_PID_TIMEOUT {
        match process_probe(pid) {
            Probe::Gone => {
                tracing::info!(
                    "自动重启：旧进程 {pid} 已退出（等待 {} ms），开始初始化",
                    start.elapsed().as_millis()
                );
                return;
            }
            Probe::Alive => last_unknown = None,
            Probe::Unknown(why) => last_unknown = Some(why),
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    match last_unknown {
        Some(why) => tracing::warn!(
            "自动重启：等旧进程 {pid} 等到上限（{} 秒），而最后几次连它的状态都查不出来（{why}）；\
             这次**没能确认**它退出，继续启动可能被单实例守卫拦下",
            WAIT_PID_TIMEOUT.as_secs()
        ),
        None => tracing::warn!(
            "自动重启：等待旧进程 {pid} 退出超时（{} 秒），继续启动（可能被单实例守卫拦截）",
            WAIT_PID_TIMEOUT.as_secs()
        ),
    }
}

#[cfg(test)]
mod restart_wait_tests {
    use super::open_process_says_gone;

    /// 能宣布"旧进程已经退出"的失败**只有一个错误码**。
    ///
    /// 这一判据决定新进程是当场往下初始化（早一步就是"两个进程都没了"那条故障），
    /// 还是继续等到上限并如实说出"没能确认"。87 是文档里那条：pid 已经不是有效进程；
    /// 5（句柄开不了：目标是提权/受保护进程，或者那个 pid 已经被别的进程占用）
    /// 讲的是"我查不动"，不是"它退了"。
    #[test]
    fn only_one_open_process_failure_means_the_pid_is_gone() {
        let mut wrong: Vec<String> = Vec::new();
        if !open_process_says_gone(87) {
            wrong.push("87 号（pid 已失效）必须算「已退出」，否则这条轮询要空等 15 秒".to_string());
        }
        // 0 = 压根没有错误信息；2 = 找不到文件；1412 = 句柄已失效。三者都推不出"退了"
        for code in [5u32, 0, 2, 1412] {
            if open_process_says_gone(code) {
                wrong.push(format!("{code} 号不该被说成「已退出」"));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }
}

/// 启动 Tauri 应用。
pub fn run() {
    init_logging();

    // 自动重启链上的新进程：先等旧进程走干净再往下走
    if let Some(pid) = wait_pid_arg() {
        wait_previous_process_exit(pid);
    }
    // 数据目录待搬运？「更改数据文件夹」那个按钮只写配置然后重启，真正的搬运在这里。
    // 位置是刻意的：必须在上面等到旧进程**真退出之后**、又在任何东西开库之前 ——
    // 搬的才是旧进程写完整、备份完整的最终结果，而不是搬一半它还在往里补写。
    focusflow_core::data_location::run_pending_migration();
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
            // setup 跑在主线程：记下它，插件管理器据此发现自己被从别的线程调用了
            crate::plugins::note_main_thread();
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
            commands::get_goal_status,
            commands::get_weekly_report,
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
            commands::change_data_dir,
            commands::export_report,
            plugins::get_plugin_view,
            plugins::plugin_action,
            plugins::plugin_set_field,
            commands::get_maintenance_info,
            commands::do_backup,
            commands::get_startup_report,
            snip::do_snip,
            snip::do_snip_annotate,
            snip::snip_take,
            snip::snip_commit,
            snip::snip_cancel,
            pin::pin_take,
            pin::pin_resize,
            pin::pin_close,
            pin::pin_close_all,
        ])
        .on_window_event(|window, event| {
            // 主窗口关闭 → 隐藏到托盘（500ms 后仍隐藏才销毁，见 state::hide_main_window），
            // 销毁可让任务管理器及时重新归类为后台进程；托盘"退出程序"才真正退出。
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    api.prevent_close();
                    state::hide_main_window(window.app_handle());
                }
                // 截图覆盖层被 Alt+F4 收走：按"取消"收尾（清会话、恢复悬浮窗），并且
                // **必须拦住真正的关闭** —— 这个窗口是懒创建的，页面已经加载过一次；
                // 让它真销毁就得到"下次截图再建一个控制器"那条不可靠的路（见 snip.rs 顶部）。
                if window.label() == snip::SNIP_LABEL {
                    api.prevent_close();
                    snip::on_snip_close_requested(window.app_handle());
                }
            }
            // 贴图窗口销毁之后才收回它那份底图（`CloseRequested` 时窗口还在，那时收就早了）。
            // 与覆盖层相反：贴图**不**拦关闭 —— 每张贴图都是新 label、走的是覆盖层已经验证过
            // 的那条建窗路，销毁之后可以重建；拦下来反而得到「✕ 点不动」那种关不掉的东西。
            // 非贴图的窗口在这里直接早退（`on_destroyed` 自己筛 label），不必在这儿再写一遍判据。
            if matches!(event, tauri::WindowEvent::Destroyed) {
                pin::on_destroyed(window.app_handle(), window.label());
            }
        })
        .build(tauri::generate_context!())
        .expect("Tauri 应用构建失败")
        .run(|app_handle, event| {
            // 退出前优雅关闭数据库：flush + 备份 + 停止写线程
            if let tauri::RunEvent::Exit = event {
                // 配置去抖写盘：退出前强制落盘，避免丢失最后的设置。
                // 这里原先是 `let _ =`，是全仓唯一不记日志的配置写失败 —— 杀软或另一份
                // 实例握着 config.ini 时，最后这次改动没了，日志里一个字都没有。
                if let Err(e) = focusflow_core::config::instance().save() {
                    tracing::warn!("退出前落盘配置失败，最后一次改动可能丢失: {e}");
                }

                if let Some(state) = app_handle.try_state::<std::sync::Arc<state::AppState>>() {
                    let db = std::sync::Arc::clone(&state.db);
                    let config = state.config;
                    // 在独立线程里做关闭，主线程限时等待。
                    //
                    // 不能直接丢给后台线程就返回：`RunEvent::Exit` 返回后进程即终止，
                    // 未跑完的 flush/备份会被直接掐断 —— 那是真丢数据。
                    // 也不能裸在主线程跑：内部是 flush(true)（≤3s）+ 全量备份 + stop()（≤3s），
                    // 用户看到的就是"点了退出卡 6 秒"。折中成"后台线程 + 限时 join"：
                    // 正常情况下主线程等它跑完（行为与之前一致、数据不丢），
                    // 只有异常卡死时才放弃等待，避免窗口永远关不掉。
                    let handle = std::thread::Builder::new()
                        .name("db-shutdown".into())
                        .spawn(move || db.shutdown(config));
                    match handle {
                        Ok(h) => {
                            let deadline =
                                std::time::Instant::now() + std::time::Duration::from_secs(20);
                            while !h.is_finished() && std::time::Instant::now() < deadline {
                                std::thread::sleep(std::time::Duration::from_millis(20));
                            }
                            if h.is_finished() {
                                let _ = h.join();
                            } else {
                                tracing::error!("数据库关闭超时（20 秒），放弃等待直接退出");
                            }
                        }
                        Err(e) => {
                            // 线程都起不来（极端资源耗尽）：退回同步关闭，宁可慢不可丢
                            tracing::error!("关闭线程启动失败（{e}），改为主线程同步关闭");
                            state.db.shutdown(state.config);
                        }
                    }
                }
                // 放在所有关闭动作之后：上面这些过程本身也要留日志，而非阻塞
                // 日志 worker 的缓冲区不显式释放，进程一退最后几条就没了
                pin::log_on_quit(app_handle);
                focusflow_core::logger::shutdown();
            }
        });
}

/// 接线审计：前端叫的命令、程序建的窗口，与 Rust 侧注册表 / capabilities 必须对得上。
///
/// 为什么要写成用例而不是靠 review：这两件事坏掉时的症状都是**"页面做了个动作，什么都没发生"**，
/// 而且现场不留痕迹 —— `snip` 那次就是覆盖层窗口的 label 没进 `capabilities/default.json`，
/// 结果页面第一个 `invoke` 就被拒，而编译、用例、门禁全绿，只能靠人去点。命令名写错一个字母同理：
/// `invoke("get_plugin_views")` 照样能编译、能跑，只是永远 reject。
///
/// 两条断言各管一个方向，都是**扫源码**而不是维护第二份清单（清单一旦要人手同步，
/// 就回到"改一漏一"那个老账上）。
#[cfg(test)]
mod wiring_audit {
    use std::path::{Path, PathBuf};

    /// 程序会创建的**全部**窗口 label。新增窗口必须登记到这里 —— 没登记时第二条断言会红，
    /// 红消息里会写出是在哪个文件建出来的。
    ///
    /// 多实例窗口（贴图）登记的是它的**通配形状**，与 `capabilities/default.json` 里那条
    /// 字符串一模一样：`pin-*` 覆盖 `pin-1`、`pin-2`…（tauri 侧确实是按 glob 匹配的，
    /// `tauri-utils` 的 `acl/resolved.rs` 把每条编成 `glob::Pattern`，`ipc/authority.rs`
    /// 拿 `.matches(label)` 判）。
    const APP_WINDOW_LABELS: [&str; 4] = ["main", "floating", "snip", "pin-*"];

    fn read(rel: &str) -> String {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("读 {} 失败：{e}", p.display()))
    }

    /// 递归收集 `ui/` 下的 .js（前端源码都在那儿，`snip.js` 在 `ui/` 根而不是 `ui/js/`）。
    fn js_files(dir: &Path, out: &mut Vec<PathBuf>) {
        walk(dir, "js", out);
    }

    /// 递归收集 `src/` 下的 .rs（建窗点可能出现在任何模块）。
    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        walk(dir, "rs", out);
    }

    fn walk(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
        for entry in
            std::fs::read_dir(dir).unwrap_or_else(|e| panic!("读 {} 失败：{e}", dir.display()))
        {
            let path = entry.expect("读目录项失败").path();
            if path.is_dir() {
                walk(&path, ext, out);
            } else if path.extension().is_some_and(|e| e == ext) {
                out.push(path);
            }
        }
    }

    /// 取 `name[...]` 之后与配平的 `]` 之间那一段（`[` `]` 计数，能穿过嵌套）。
    fn bracket_block(text: &str, opener: &str) -> Option<String> {
        let start = text.find(opener)? + opener.len();
        let bytes: Vec<char> = text[start..].chars().collect();
        let mut depth = 1usize;
        let mut i = 0usize;
        while i < bytes.len() {
            match bytes[i] {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(text[start..start + chars_upto(&bytes, i)].to_string());
                    }
                }
                _ => {}
            }
            i += 1;
        }
        None
    }

    /// `chars` 前 i 个字符在原串里占多少 byte —— 用 ASCII 之外的中文注释把索引对回去。
    fn chars_upto(bytes: &[char], i: usize) -> usize {
        bytes[..i].iter().map(|c| c.len_utf8()).sum()
    }

    fn registered_commands() -> Vec<String> {
        let src = read("src/lib.rs");
        let block =
            bracket_block(&src, "generate_handler![").expect("lib.rs 里找不到 generate_handler!");
        let mut names: Vec<String> = block
            .split(',')
            .map(|s| {
                s.lines()
                    .filter(|l| !l.trim_start().starts_with("//"))
                    .collect::<String>()
            })
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(|s| s.rsplit("::").next().unwrap_or_default().to_string())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    fn invoked_commands() -> (Vec<String>, Vec<String>) {
        let mut files = Vec::new();
        js_files(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ui"),
            &mut files,
        );
        assert!(
            !files.is_empty(),
            "一个 .js 都没找到 —— 扫描路径错了，这条断言就没有观察量"
        );
        let mut names = Vec::new();
        let mut unresolved = Vec::new();
        for f in files {
            let text = std::fs::read_to_string(&f).unwrap();
            let rel = f.file_name().unwrap().to_string_lossy().to_string();
            for chunk in text.split("invoke(").skip(1) {
                let head = chunk.split([',', ')', '\n']).next().unwrap_or("").trim();
                let quoted = head
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .or_else(|| head.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')));
                match quoted {
                    Some(n) if !n.is_empty() => names.push(n.to_string()),
                    // 动态命令名（`invoke(cmd)`）以前没有过，出现了就要人来看一眼：
                    // 静默跳过会让这条审计悄悄失去覆盖面。
                    _ => unresolved.push(format!("{rel}: invoke({head}…)")),
                }
            }
        }
        names.sort();
        names.dedup();
        (names, unresolved)
    }

    fn capability_windows() -> Vec<String> {
        let json = read("capabilities/default.json");
        let Some(at) = json.find("\"windows\"") else {
            panic!("capabilities/default.json 里没有 windows 字段");
        };
        let block = json[at..]
            .split_once('[')
            .and_then(|(_, rest)| rest.split_once(']'))
            .map(|(inner, _)| inner.to_string())
            .expect("windows 字段不是数组");
        block
            .split('"')
            .skip(1)
            .step_by(2)
            .map(|s| s.to_string())
            .collect()
    }

    /// 递归收集 `ui/` 下的 .html（页面骨架与 .js 是同级的，`js/` 子目录里没有 html）。
    fn html_files(dir: &Path, out: &mut Vec<PathBuf>) {
        walk(dir, "html", out);
    }

    /// 抓 `call(...)` 第 `idx` 个参数里的那个**字符串字面量**。不是字面量的（`getElementById(v)`）
    /// 跳过，但调用方要拿到它们的名字 —— 覆盖面静默缩掉比红一条更难查，所以照
    /// `invoked_commands` 那样把解不出的原样交出去。
    ///
    /// `idx` 不为 0 是 `emit_to(窗口, "事件名", …)` 那种第一位另有其物的形状。
    ///
    /// 参数边界靠**配平括号**数出来，不是"到第一个 `,` 或换行为止"：`state.rs` 那处
    /// `emit_to(` 的实参横跨三行，按行截断会把 `stats-charts` 整条漏掉（第一版就是这么写的，
    /// 于是它报出一句假缺口）；而 `listen("x", (e) => { … })` 的箭头体里有逗号与括号，
    /// 不按深度切就会把第二个实参算进第一个。
    fn literal_call_arg(text: &str, call: &str, idx: usize) -> (Vec<String>, Vec<String>) {
        let pat = format!("{call}(");
        let mut got = Vec::new();
        let mut dynamic = Vec::new();
        let mut from = 0usize;
        while let Some(rel) = text[from..].find(&pat) {
            let open = from + rel + pat.len();
            from = open;
            let b: Vec<char> = text[open..].chars().collect();
            // 走到与这个 `(` 配平的 `)`
            let mut depth = 1usize;
            let mut end = b.len();
            for (i, c) in b.iter().enumerate() {
                match c {
                    '(' | '[' | '{' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = i;
                            break;
                        }
                    }
                    ']' | '}' if depth > 1 => depth -= 1,
                    _ => {}
                }
            }
            // 只在顶层（depth==1）切逗号
            let mut args: Vec<String> = Vec::new();
            let mut depth = 1usize;
            let mut start = 0usize;
            for (i, c) in b[..end].iter().enumerate() {
                match c {
                    '(' | '[' | '{' => depth += 1,
                    ')' | ']' | '}' => depth -= 1,
                    ',' if depth == 1 => {
                        args.push(b[start..i].iter().collect());
                        start = i + 1;
                    }
                    _ => {}
                }
            }
            args.push(b[start..end].iter().collect());
            let head = match args.get(idx) {
                Some(a) => a.trim().to_string(),
                None => {
                    dynamic.push(format!("{call}(…只给了 {} 个参数)", args.len()));
                    continue;
                }
            };
            match head
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| head.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
            {
                Some(v) if !v.is_empty() => got.push(v.to_string()),
                _ => dynamic.push(format!("{call}({head}…)")),
            }
        }
        (got, dynamic)
    }

    /// 抓 `id="…"` / `id='…'` 这种**属性式**的声明。与上面 `literal_args` 是两种形状：
    /// 那一个认的是 `f("x")` 的调用参数，`id` 后面跟的是 `=` 不是 `(`，不能共用。
    ///
    /// 前一个字符必须是分隔符，否则 `data-id="shot"` 与 `valid_id = "shot"` 会冒充成
    /// `id="shot"` 把真缺口盖掉。`.` 要放行：`el.id = "toast"` 那种"用到时现造"的写法
    /// （`utils.js` 的 toast、`floating.js` 的休息提示）就是合法声明，拦掉它会报两条假缺口。
    fn declared_ids(text: &str) -> Vec<String> {
        let b: Vec<char> = text.chars().collect();
        let mut out = Vec::new();
        for i in 0..b.len() {
            if i + 2 > b.len() || b[i] != 'i' || b[i + 1] != 'd' {
                continue;
            }
            let prev_ok = i == 0
                || matches!(
                    b[i - 1],
                    ' ' | '\t' | '\n' | '\r' | '<' | '"' | '\'' | '`' | '.'
                );
            if !prev_ok {
                continue;
            }
            let mut j = i + 2;
            while j < b.len() && matches!(b[j], ' ' | '\t') {
                j += 1;
            }
            if j >= b.len() || (b[j] != '=' && b[j] != ':') {
                continue;
            }
            j += 1;
            while j < b.len() && matches!(b[j], ' ' | '\t') {
                j += 1;
            }
            if j >= b.len() || (b[j] != '"' && b[j] != '\'' && b[j] != '`') {
                continue;
            }
            let quote = b[j];
            if let Some(end) = b[j + 1..].iter().position(|c| *c == quote) {
                out.push(b[j + 1..j + 1 + end].iter().collect());
            }
        }
        out
    }

    /// 前端每个字面量 `getElementById("x")` 都必须有人在某处写下 `id="x"`。
    ///
    /// 这是 `every_window_label_has_a_capability` 的第三种同一族缺口：ids 与 label 一样是
    /// **跨语言/跨文件的字符串约定**，错了不会编译失败、不会让任何一条现有用例变红，症状只是
    /// "那个页面什么都不会发生"。本仓已经在 `snip` 的 capabilities 上栽过一次（真机才发现），
    /// 而浏览器夹具（`.scratch/*-harness.html`）不进 CI ⇒ 只能在这儿钉。
    ///
    /// `id` 也从 .js 里收集：设置页不少元素是 JS 现拼的模板（`views.js`），只扫 html 会误报。
    #[test]
    fn every_literal_getelementbyid_has_a_matching_id() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ui");
        let mut js = Vec::new();
        js_files(&root, &mut js);
        let mut html = Vec::new();
        html_files(&root, &mut html);
        assert!(
            !js.is_empty() && !html.is_empty(),
            "一个 .js 或 .html 都没找到（扫描路径 {:?}）—— 这条审计就没有观察量",
            root
        );

        // 声明侧：所有 html + js 里出现过的 id="…"
        let mut declared: Vec<String> = Vec::new();
        for f in js.iter().chain(html.iter()) {
            let text = std::fs::read_to_string(f).unwrap();
            let rel = f.file_name().unwrap().to_string_lossy().to_string();
            for id in declared_ids(&text) {
                if id.is_empty() {
                    panic!("{rel} 里有个空 id 声明，扫描器要跟着改");
                }
                declared.push(id);
            }
        }
        declared.sort();
        declared.dedup();
        assert!(
            declared.len() > 20,
            "只量到 {} 个 id 声明 —— 扫描器多半没生效，这条就成了假绿",
            declared.len()
        );

        let mut missing = Vec::new();
        let mut dynamic = Vec::new();
        let mut wanted = 0usize;
        for f in &js {
            let text = std::fs::read_to_string(f).unwrap();
            let rel = f.file_name().unwrap().to_string_lossy().to_string();
            let (want, dyn_want) = literal_call_arg(&text, "getElementById", 0);
            wanted += want.len();
            for id in want {
                if !declared.contains(&id) {
                    missing.push(format!("{rel}: getElementById(\"{id}\")"));
                }
            }
            for d in dyn_want {
                dynamic.push(format!("{rel}: {d}"));
            }
        }
        // 两头都要有量：任一为空都说明这条在空转
        assert!(
            wanted >= 10,
            "只量到 {wanted} 个字面量 getElementById —— 抓取形状变了，这条是假绿"
        );
        assert!(
            missing.is_empty(),
            "页面脚本要的 id 没有任何地方声明 —— `getElementById` 返回 null，\
             下一行就是 TypeError，整个页面的启动脚本一起死（症状：那扇窗口是块纯色）：\n{}",
            missing.join("\n")
        );
        // 动态 id 不是错误，但要说出来：那正是"审计悄悄失去覆盖面"的形状
        if !dynamic.is_empty() {
            eprintln!(
                "注意：这些 getElementById 的参数不是字面量，本条审计不覆盖它们：\n{}",
                dynamic.join("\n")
            );
        }
    }

    /// 去掉行注释（`//` 与 `///` 都算）：本模块的注释里就写着 `.emit("名字", …)` 与
    /// `getElementById(v)` 这种示例，连注释一起扫会**自己匹配自己** —— 与本仓
    /// `pin.rs::code_only` 同一个处理。代价是 `https://…` 字符串所在行的后半截会被截掉，
    /// 那对"从调用里抠字符串字面量"没有影响（要抠的调用在前头）。
    fn code_only(src: &str) -> String {
        src.lines()
            .map(|l| match l.find("//") {
                Some(i) => &l[..i],
                None => l,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Rust 广播的每个事件名，前端必须有人听；前端听的每个事件名，必须有人广播。
    ///
    /// 同一族缺口的第四条：命令名（`every_invoked_command_is_registered`）、窗口 label
    /// （`every_window_label_has_a_capability`）、页面元素 id，加上这条的事件名 ——
    /// 全是**跨语言写一遍字符串**的约定。事件名这一条最阴：写错了 `emit` 本身**成功**
    /// （tauri 不校验有没有监听者），所以连 `截图结果发不回前端` 那句 warn 都不会打，
    /// 症状只是"图截好了、盘上也有了，但那句提示永远不出现"。
    ///
    /// 两个方向都判：只查"没人听"会漏掉前端在等一个从没广播过的事件（那才是真断链），
    /// 只查"没人 broadcast"会漏掉改名后留下的孤儿广播。
    #[test]
    fn event_names_match_between_rust_and_the_pages() {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut rs = Vec::new();
        rust_files(&manifest.join("src"), &mut rs);
        let mut js = Vec::new();
        js_files(&manifest.join("ui"), &mut js);
        assert!(!rs.is_empty() && !js.is_empty(), "扫描路径下什么都没有");

        let collect = |files: &Vec<PathBuf>, calls: &[(&str, usize)]| {
            let mut names: Vec<String> = Vec::new();
            let mut dynamic = Vec::new();
            for f in files {
                let text = code_only(&std::fs::read_to_string(f).unwrap());
                let rel = f.file_name().unwrap().to_string_lossy().to_string();
                for (call, idx) in calls {
                    let (got, dyn_got) = literal_call_arg(&text, call, *idx);
                    for n in got {
                        names.push(n);
                    }
                    for d in dyn_got {
                        dynamic.push(format!("{rel}: {d}"));
                    }
                }
            }
            names.sort();
            names.dedup();
            (names, dynamic)
        };

        // Rust 侧 `.emit("名字", …)` 与 `.emit_to(窗口, "名字", …)`；JS 侧 `emit("名字", …)`
        let (mut emitted, mut dyn_e) =
            collect(&rs, &[("emit", 0), ("emit_to", 1), ("emit_filtered", 1)]);
        let (js_emitted, mut dyn_je) = collect(&js, &[("emit", 0)]);
        emitted.extend(js_emitted);
        emitted.sort();
        emitted.dedup();
        dyn_e.append(&mut dyn_je);

        // 监听侧只在前端（本仓 Rust 不用 `app.listen`，出现了会掉进 dynamic 里被说出来）
        let (listened, dyn_l) = collect(&js, &[("listen", 0), ("listen_to", 1)]);

        // 两条腿都得有量，否则"没有缺口"只是因为"什么都没扫到"
        assert!(
            emitted.len() >= 5,
            "只量到 {} 个广播事件名 —— 扫描形状变了，这条是假绿",
            emitted.len()
        );
        assert!(
            listened.len() >= 5,
            "只量到 {} 个监听事件名 —— 扫描形状变了，这条是假绿",
            listened.len()
        );

        let no_listener: Vec<&String> = emitted.iter().filter(|n| !listened.contains(n)).collect();
        let no_emitter: Vec<&String> = listened.iter().filter(|n| !emitted.contains(n)).collect();
        assert!(
            no_listener.is_empty(),
            "广播了但前端没人听（多半是前端那侧改了名或删了监听）：{:?}\n广播侧共 {} 个",
            no_listener,
            emitted.len()
        );
        assert!(
            no_emitter.is_empty(),
            "前端在等一个从来没人广播的事件 —— 那个界面永远不会被更新：{:?}\n监听侧共 {} 个",
            no_emitter,
            listened.len()
        );
        if !dyn_e.is_empty() || !dyn_l.is_empty() {
            eprintln!(
                "注意：这些事件名不是字面量，本条审计不覆盖它们：\n{}",
                [dyn_e, dyn_l].concat().join("\n")
            );
        }
    }

    /// 前端叫到的每个命令，都必须在 `invoke_handler` 的注册表里。
    #[test]
    fn every_invoked_command_is_registered() {
        let (called, dynamic) = invoked_commands();
        assert!(
            dynamic.is_empty(),
            "出现了动态命令名，这条审计不再覆盖它们，请改写法或显式登记：\n{}",
            dynamic.join("\n")
        );
        let registered = registered_commands();
        let missing: Vec<&String> = called.iter().filter(|c| !registered.contains(c)).collect();
        assert!(
            missing.is_empty(),
            "前端 invoke 了没注册的命令（会被拒，症状是\"点了没反应\"）：{:?}\n注册表有 {} 条",
            missing,
            registered.len()
        );
        // 反向不判：注册了但前端没叫的（`quit` / `flush_db` / `dbg_log`）是留给插件面板与
        // 兜底路径用的，当成死代码报会一直红 —— 那是另一件事，不该混进这条断言里。
    }

    /// 程序建的每个窗口 label 都必须在 capabilities 里，否则那个窗口拿不到 `core:default`，
    /// 它发出的第一个 `invoke` 就会被拒（`snip` 那次真机就是这样）。
    #[test]
    fn every_window_label_has_a_capability() {
        let caps = capability_windows();
        for label in APP_WINDOW_LABELS {
            assert!(
                caps.iter().any(|w| w == label),
                "窗口 {label} 不在 capabilities/default.json 的 windows 里 —— 它的 invoke 会全被拒"
            );
        }

        // 建窗侧：扫 builder 的第二参数（字面量或 `&str` 常量），不许出现没登记的 label。
        // 扫整个 src/ 而不是写死几个文件名 —— 写死清单等于"改一漏一"，新模块建窗就漏掉了。
        let mut builders = Vec::new();
        rust_files(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut builders,
        );
        let mut found: Vec<(String, String)> = Vec::new();
        for f in builders {
            let src = std::fs::read_to_string(&f).unwrap();
            let body = strip_line_comments(&src);
            let rel = f.file_name().unwrap().to_string_lossy().to_string();
            for chunk in body.split(&format!("{BUILDER_MARK}(")).skip(1) {
                // 第二个逗号段就是 label：`new(app, "main", WebviewUrl::…)` 或 `new(app, SNIP_LABEL, …)`
                let second = chunk
                    .split(',')
                    .nth(1)
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                let label = match second.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
                    Some(q) => q.to_string(),
                    // 常量（如 SNIP_LABEL）：回到同一个文件里查它等于哪个串
                    None => lookup_const(&src, &second)
                        .or_else(|| dynamic_label(&src, &second))
                        .unwrap_or_else(|| {
                            panic!(
                                "{rel} 里用 `{second}` 建窗，但解不出这个常量的值，请改审计或登记"
                            )
                        }),
                };
                found.push((label, rel.clone()));
            }
        }
        assert!(
            !found.is_empty(),
            "一个建窗点都没扫到 —— 扫描写法过期了，别让它假装通过"
        );
        for (label, where_) in &found {
            assert!(
                APP_WINDOW_LABELS.contains(&label.as_str()),
                "{where_} 建了窗口 \"{label}\"，但没登记进 APP_WINDOW_LABELS（新窗口要同步 capabilities）"
            );
        }

        // 配置侧：tauri.conf.json 预声明的窗口同样要在 capabilities 里。
        let conf = read("tauri.conf.json");
        for chunk in conf.split("\"label\"").skip(1) {
            let Some(rest) = chunk.split(':').nth(1) else {
                continue;
            };
            let label = rest
                .trim_start()
                .strip_prefix('"')
                .and_then(|s| s.split('"').next())
                .unwrap_or_default()
                .to_string();
            if label.is_empty() {
                continue;
            }
            assert!(
                caps.contains(&label),
                "tauri.conf.json 里的窗口 \"{label}\" 不在 capabilities 的 windows 里"
            );
        }
    }

    /// 建窗调用的前缀。扫的是这个串再拼上 `(`（所以整串在源码文本里不成形），写成运行时拼接
    /// 是因为第一版被**自己注释里抄的那段示例**喂了一个假建窗点。
    const BUILDER_MARK: &str = "WebviewWindowBuilder::new";

    /// 丢掉行注释（只用于扫描，不影响编译）。粗到"整行 `//` 之后全切"，代价可能改坏
    /// 某行里的 URL 字符串 —— 那不影响本审计，因为它只找 builder 标记和紧跟其后的引号。
    fn strip_line_comments(src: &str) -> String {
        src.lines()
            .map(|l| match l.find("//") {
                Some(i) => &l[..i],
                None => l,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 在同一个文件里找 `const NAME: &str = "value"`。
    fn lookup_const(src: &str, name: &str) -> Option<String> {
        let at = src.find(&format!("const {name}"))?;
        let value = src[at..]
            .split('"')
            .nth(1)
            .filter(|s| !s.is_empty())?
            .to_string();
        Some(value)
    }

    /// 动态 label：`format!("{PIN_LABEL_PREFIX}{seq}")` —— 多实例窗口（贴图）就是这么命名的。
    ///
    /// 只认这一种形状，且解出来的是**登记用的通配**（`pin-*`），与 capabilities 里那条字符串
    /// 同一个串。三条限制各有理由：
    /// - `{名字}` 之前必须没有别的文本：通配只能表达「以某串开头」，`format!("x{C}1")`
    ///   那种拼接匹不出来的东西不该被当成已登记；
    /// - 那个常量必须以 `-` 结尾：前缀不带分隔符（`pin`）会让 `pin-*` 谁也匹不到，
    ///   症状正好是当年 snip 那个「窗口有了、调什么都被拒」；
    /// - 解不出就 `None`，由调用点 panic —— 静默跳过等于这条审计悄悄失去覆盖面。
    fn dynamic_label(src: &str, expr: &str) -> Option<String> {
        let inner = expr.strip_prefix("format!(\"")?.strip_suffix("\")")?;
        let (head, rest) = inner.split_once('{')?;
        if !head.is_empty() {
            return None;
        }
        let name = rest.split('}').next()?;
        let prefix = lookup_const(src, name)?;
        if !prefix.ends_with('-') {
            return None;
        }
        Some(format!("{prefix}*"))
    }

    /// 自动重启那条轮询不许把"查不到"折成"已退出"。
    ///
    /// 上面那条纯用例钉的是**判据**（哪个错误码算退出），它红不到轮询本身 ——
    /// 这段代码要的是"句柄开不了的进程 + 十五秒的真实等待"，用例造不出可达的输入。
    /// 所以这里钉调用形态：`OpenProcess` 失败那一步必须过 `open_process_says_gone`，
    /// 而旧的两处折法（`!process_alive(pid)` 那个 bool、`is_ok() &&` 那句拿返回值当证据）
    /// 不许回来。
    #[test]
    fn the_restart_wait_never_reports_an_unanswerable_query_as_exit() {
        let src = read("src/lib.rs");
        // 切到第一个测试模块之前：本文件里的 `#[cfg(test)]` 字样也出现在文档注释里，
        // 按那一句切会把整段产品代码切没（针脚于是永远"找不到"，红在正确代码上）。
        let cut = src
            .find("#[cfg(test)]\nmod restart_wait_tests")
            .expect("restart_wait_tests 的起点找不到了");
        let prod = &src[..cut];
        let mut wrong: Vec<String> = Vec::new();
        // 两条针脚合起来才钉得住"错误码是从 GetLastError 现取的、并且过了一次判据"：
        // 单看 `open_process_says_gone(实参)` 那一行会被 rustfmt 拆掉换行，
        // 那种针脚在正确代码上永远找不到（本仓上一轮刚踩过一次）。
        let n = prod.matches("let code = GetLastError().0;").count();
        if n != 1 {
            wrong.push(format!(
                "OpenProcess 失败后必须恰好一次现读错误码，实得 {n} 次"
            ));
        }
        let m = prod.matches("open_process_says_gone(code)").count();
        if m != 1 {
            wrong.push(format!(
                "那份错误码必须恰好过一次「哪个码才算退出」的判据，实得 {m} 次"
            ));
        }
        for gone_old in [
            "!process_alive(pid)",
            "fn process_alive",
            "GetExitCodeProcess(handle, &mut code).is_ok()",
        ] {
            if prod.contains(gone_old) {
                wrong.push(format!("旧写法回来了：{gone_old}（查不到＝已退出）"));
            }
        }
        let arms = prod.matches("Probe::Gone =>").count();
        if arms != 1 {
            wrong.push(format!("轮询里「确认已退出」那一臂应恰好一处，实得 {arms}"));
        }
        if !prod.contains("Probe::Unknown(why) => last_unknown = Some(why)") {
            wrong.push("查不动那一臂没把原因留给收尾那句 warn".to_string());
        }
        assert!(
            wrong.is_empty(),
            "重启轮询的接线守卫红:\n{}",
            wrong.join("\n")
        );
    }

    /// 启动报告必须问一次"文件日志那一层建起来没有"。
    ///
    /// 这条缺陷的形状是"故障只往 stderr 说话，而桌面端没有控制台"：文件层没建起来时
    /// 本进程一辈子都没有 `logs/`（subscriber 只装一次），而 `logs/` 是本仓每个故障的
    /// 默认下一步。所以这件事必须由**用户看得见的那条道**（启动报告 → toast → 托盘 ⚠）
    /// 带出来。core 那边钉的是两个臂，这里钉的是接线。
    #[test]
    fn the_startup_report_carries_the_file_log_outcome() {
        let src = read("src/state.rs");
        // 摘掉注释行：判据的字样也写在这条用例的说明里，不摘就会在正确代码上误报
        let prod = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let needle = "focusflow_core::logger::file_log_failure()";
        let n = prod.matches(needle).count();
        assert_eq!(
            n, 1,
            "启动报告要恰好问一次文件日志那一层的结局，实得 {n} 次"
        );
        // 问完还得把两种结局都带进报告：失败那一臂必须是 fail（全绿静默、出事要显性化）
        let at = prod.find(needle).expect("上面数过了，这里不该找不到");
        let rest = &prod[at..];
        let window = &rest[..rest.len().min(900)];
        assert!(
            window.contains("CheckResult::ok") && window.contains("CheckResult::fail"),
            "两种结局都得进报告，实得片段: {window}"
        );
        assert!(
            window.contains("\"文件日志\""),
            "报告里那一行要写清是哪个子系统，实得片段: {window}"
        );
    }
}
