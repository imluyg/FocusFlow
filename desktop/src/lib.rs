//! FocusFlow Tauri 桌面端后端。
//!
//! 复用 `focusflow-core` 的数据库/监听/统计/插件逻辑，
//! 通过 Tauri 命令暴露给 Web 前端；管理主窗口、悬浮窗、托盘与全局热键。

pub mod commands;
pub mod export;
pub mod hotkey;
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
    const APP_WINDOW_LABELS: [&str; 3] = ["main", "floating", "snip"];

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
                    None => lookup_const(&src, &second).unwrap_or_else(|| {
                        panic!("{rel} 里用 `{second}` 建窗，但解不出这个常量的值，请改审计或登记")
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
}
