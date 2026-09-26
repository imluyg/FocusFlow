//! Tauri 命令：暴露统计/配置/监听/数据操作给前端。

use std::sync::Arc;

use tauri::{AppHandle, Emitter, State};

use crate::state::{AppState, ChartsStats, LiveStats};

/// 获取轻量实时数据（今日/速度/周期）。
#[tauri::command]
pub fn get_live(state: State<'_, Arc<AppState>>) -> LiveStats {
    let s = state.shared.lock().unwrap_or_else(|e| e.into_inner());
    LiveStats {
        today_count: s.today_count,
        cpm: s.cpm,
        active_seconds: s.active_seconds,
        period: s.period,
        max_day: s.agg.max_day,
        max_day_date: s.agg.max_day_date.clone(),
        alltime_total: s.agg.alltime_total,
        period_total: s.agg.total,
    }
}

/// 获取重量级图表数据（排行/趋势/分布）。
/// async：同步命令在主线程执行，ChartAgg 的深拷贝+序列化恰好发生在
/// 打开主窗口的瞬间，会造成可感知停顿；挪到异步运行时线程执行。
#[tauri::command]
pub async fn get_charts(state: State<'_, Arc<AppState>>) -> Result<ChartsStats, String> {
    let s = state.shared.lock().unwrap_or_else(|e| e.into_inner());
    Ok(ChartsStats {
        period: s.period,
        agg: s.agg.clone(),
    })
}

/// 一次性返回设置页所需全部配置（替代多次 get_config 轮询）。
#[tauri::command]
pub fn get_settings(state: State<'_, Arc<AppState>>) -> serde_json::Value {
    let c = state.config;
    serde_json::json!({
        "theme": c.get("gui", "theme"),
        "paused": state.listener.is_paused(),
        // 布尔项一律走 `get_bool`，不能拿字符串比 "==true"：`get_bool` 认
        // `true/1/yes/on`，而这里以前只认字面量 "true" —— 手改 config.ini 写成
        // `enabled = 1` 时功能是**开的**（hotkey.rs / state.rs 都用 get_bool 读），
        // 设置页的勾却是灭的，两个入口对同一件事两把尺子。默认值与权威读者对齐：
        // [hotkey] enabled 的默认是 false，[floating] enabled 的默认是 true。
        "hotkey_enabled": c.get_bool("hotkey", "enabled", false),
        "hotkey_str": c.get("hotkey", "toggle_window"),
        // 已启用但注册失败时的原因（空串 = 正常）：组合键常被其它程序占用，
        // 只有日志的话用户看到的是「开关打开着、按了没反应」
        "hotkey_error": crate::hotkey::last_error().unwrap_or_default(),
        "floating_enabled": c.get_bool("floating", "enabled", true),
        // 备份开关：退出时备份 / 运行中定时备份（0 小时 = 关闭）
        "backup_on_exit": c.get_bool("database", "backup_on_exit", true),
        "backup_online_hours": c.get_int("database", "online_backup_interval_hours", 24),
        "max_backups": c.get_int("database", "max_backups", 5),
        // 每日目标次数（连续打卡的判定线）
        "goal_daily_keys": c.get_int("goal", "daily_keys", 20000),
    })
}

/// 每日目标与连续打卡。
///
/// async：判定要读近 370 天的按日序列（可能跨两个年度库），同步命令会把这次
/// 扫描压在主线程上 —— 与 `get_charts` 同一个理由挪到运行时线程。
#[tauri::command]
pub async fn get_goal_status(state: State<'_, Arc<AppState>>) -> Result<serde_json::Value, String> {
    let app = Arc::clone(&state);
    // async 命令体跑在 tokio 运行时线程上，而这条要在里面做跨年度库的同步扫描
    // （近 370 天按日序列）—— 挪到 spawn_blocking，别占着运行时线程（同 vacuum_db /
    // import_legacy 的套路）。
    tauri::async_runtime::spawn_blocking(move || goal_status_json(&app))
        .await
        .map_err(|e| e.to_string())?
}

fn goal_status_json(app: &AppState) -> Result<serde_json::Value, String> {
    let goal = app.config.get_int("goal", "daily_keys", 20000).max(1);
    let rows = focusflow_core::db::queries::get_daily_counts(
        focusflow_core::stats::GOAL_LOOKBACK_DAYS,
        None,
    );
    let today = chrono::Local::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string();
    let s = focusflow_core::stats::goal_status(goal, &rows, &today);
    Ok(serde_json::json!({
        "goal": s.goal,
        "today": s.today,
        "todayMet": s.today_met,
        "streak": s.streak,
        "best": s.best,
        "days": s
            .days
            .iter()
            .map(|(d, c, m)| serde_json::json!({ "date": d, "count": c, "met": m }))
            .collect::<Vec<_>>(),
    }))
}

/// 立即生成「上一个完整周」的周报，返回文件路径。
///
/// 平时由统计线程在启动/跨周时自动触发（见 state::spawn_stats_worker）；
/// 这个入口给设置页的「立即生成」按钮用。
#[tauri::command]
pub async fn get_weekly_report() -> Result<Option<String>, String> {
    // 生成周报要扫上一周 + 再往前 14 天的按日序列（跨两个年度库），并在里面读文件、
    // 压缩图片、写库：和 `get_goal_status` 同一个理由，别占着 tokio 运行时线程。
    tauri::async_runtime::spawn_blocking(|| {
        crate::export::write_weekly_report()
            .map(|p| p.map(|p| p.display().to_string()))
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 返回应用版本号（单一来源：Cargo 包版本）。
#[tauri::command]
pub fn get_version() -> String {
    focusflow_core::paths::APP_VERSION.to_string()
}

/// 主窗口是否已启用系统级窗口材质（前端据此切换 body.glass 半透明令牌）。
#[tauri::command]
pub fn get_vibrancy() -> bool {
    crate::state::MAIN_GLASS.load(std::sync::atomic::Ordering::SeqCst)
}

/// 设置统计周期（-1=今日, N=天数, 0=总计）并触发即时重聚合。
///
/// 非法值（如 -2、超大天数）必须拒绝：它会经 `gui.default_period` 持久化，
/// 并在统计线程里进入日期运算导致 panic（release 下 panic=abort → 永久无法启动）。
#[tauri::command]
pub fn set_period(state: State<'_, Arc<AppState>>, period: i64) {
    if !focusflow_core::db::queries::is_valid_period(period) {
        tracing::warn!("拒绝非法统计周期: {period}");
        return;
    }
    state
        .period
        .store(period, std::sync::atomic::Ordering::Relaxed);
    state
        .refresh_now
        .store(true, std::sync::atomic::Ordering::Relaxed);
}

/// 读取配置值。
#[tauri::command]
pub fn get_config(state: State<'_, Arc<AppState>>, section: String, key: String) -> String {
    state.config.get(&section, &key)
}

/// 写入配置值并持久化。
#[tauri::command]
pub fn set_config(
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
    section: String,
    key: String,
    value: String,
) -> Result<(), String> {
    state
        .config
        .set(&section, &key, &value)
        .map_err(|e| e.to_string())?;
    // 监听器热路径读配置快照（原子量），listener 配置变更后即时刷新
    if section == "listener" {
        state.listener.refresh_config();
    }
    // 热键配置（enabled / toggle_window）变更后运行时重新注册，无需重启。
    if section == "hotkey" && (key == "enabled" || key == "toggle_window") {
        crate::hotkey::reload_hotkey(&app);
    }
    // 悬浮窗开关：这个键原先只落盘不生效（全仓唯一的读者是启动时的 setup_windows），
    // 所以设置页勾完要到下次重启才看得见变化 —— 而"立即隐藏"按钮是马上生效的，
    // 两个入口给两种反馈，勾一次没反应的那条会被当成坏了。这里跟着即时显示/隐藏。
    if section == "floating" && key == "enabled" {
        crate::state::set_floating_visible(&app, value == "true");
    }
    Ok(())
}

/// 切换暂停记录，返回新的暂停状态，并广播给前端（与托盘切换保持一致）。
#[tauri::command]
pub fn toggle_pause(state: State<'_, Arc<AppState>>, app: AppHandle) -> bool {
    let paused = state.listener.toggle_pause();
    let _ = app.emit("pause-changed", paused);
    paused
}

/// 是否已暂停。
#[tauri::command]
pub fn is_paused(state: State<'_, Arc<AppState>>) -> bool {
    state.listener.is_paused()
}

/// 显示主窗口（不存在时重建）。
#[tauri::command]
pub fn show_main(app: AppHandle) {
    crate::state::show_main_window(&app);
}

/// 隐藏主窗口（到托盘）：销毁窗口以便任务管理器及时重新分类为后台进程。
#[tauri::command]
pub fn hide_main(app: AppHandle) {
    crate::state::hide_main_window(&app);
}

/// 显示悬浮窗（并把这个意图写回 `[floating] enabled`，否则下次启动又按旧配置来）。
#[tauri::command]
pub fn show_floating(state: State<'_, Arc<AppState>>, app: AppHandle) {
    let _ = state.config.set("floating", "enabled", "true");
    crate::state::set_floating_visible(&app, true);
}

/// 隐藏悬浮窗（同 `show_floating`：可见性变了就落盘，让设置页那个勾和现实一致）。
#[tauri::command]
pub fn hide_floating(state: State<'_, Arc<AppState>>, app: AppHandle) {
    let _ = state.config.set("floating", "enabled", "false");
    crate::state::set_floating_visible(&app, false);
}

/// 立即 flush 数据库（写线程排空队列）。
#[tauri::command]
pub fn flush_db(state: State<'_, Arc<AppState>>) {
    state.db.flush(false);
}

/// 压缩所有年度数据库（重 IO；同步命令跑在主线程会冻结 UI，故放后台线程）。
#[tauri::command]
pub async fn vacuum_db(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let db = Arc::clone(&state.db);
    tauri::async_runtime::spawn_blocking(move || {
        db.flush(true);
        let report = focusflow_core::db::maintenance::vacuum_all();
        // 以前这里 `let _ =`：哪个库没压缩成、甚至一个库都没枚举到，界面都照旧显示
        // 「压缩完成」。前端 doVacuum 的 catch 会把这句原样打在设置页的消息位上。
        if report.incomplete() {
            Err(report.why_incomplete())
        } else {
            Ok(())
        }
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(())
}

/// 退出应用（托盘"退出程序"）。
#[tauri::command]
pub fn quit(app: AppHandle) {
    app.exit(0);
}

/// 插件元数据（前端插件管理列表）。
#[derive(serde::Serialize)]
pub struct PluginMeta {
    pub name: String,
    pub desc: String,
    pub version: String,
    pub author: String,
    /// 文件名（不含扩展名）：启用开关的作用对象
    pub file: String,
    /// 是否启用（停用的插件不加载、不执行、不出现在已加载列表）
    pub enabled: bool,
    /// 当前是否已加载进内存
    pub loaded: bool,
    /// 加载错误（启用但失败时展示）
    pub error: Option<String>,
}

/// 导入旧版 FocusFlow 数据（选择目录 → 后台导入）。
/// 对话框与导入全程在后台线程执行：对话框挂起多久都不占用 runtime 线程。
#[tauri::command]
pub async fn import_legacy(state: State<'_, Arc<AppState>>) -> Result<String, String> {
    let picked = tauri::async_runtime::spawn_blocking(|| {
        rfd::FileDialog::new()
            .set_title("选择旧版 FocusFlow 数据目录（data 文件夹）")
            .pick_folder()
    })
    .await
    .map_err(|e| e.to_string())?;
    let Some(dir) = picked else {
        return Ok("已取消".to_string());
    };

    let db = Arc::clone(&state.db);
    let dir_for_thread = dir.clone();
    let summary = tauri::async_runtime::spawn_blocking(move || {
        focusflow_core::migration::import_legacy_data(&dir_for_thread)
    })
    .await
    .map_err(|e| e.to_string())?;

    // 导入直接写库，重建缓存与今日计数保持一致
    focusflow_core::db::queries::invalidate_years_cache();
    if let Some(w) = db.writer() {
        w.recompute_today_totals();
    }
    state
        .refresh_now
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let mut lines: Vec<String> = Vec::new();
    if summary.year_dbs.is_empty() && summary.copied_aux.is_empty() {
        lines.push("未发现可导入的数据".to_string());
    }
    for (year, count) in &summary.records_by_year {
        lines.push(format!("{year} 年度键鼠: {count} 条"));
    }
    if !summary.copied_aux.is_empty() {
        lines.push(format!("附属数据: {}", summary.copied_aux.join(", ")));
    }
    // 被覆盖的现有附属库已留档：必须让用户知道去哪儿找回旧数据
    for kept in &summary.backed_up_aux {
        lines.push(format!("原数据已留档: {kept}"));
    }
    for e in &summary.errors {
        lines.push(format!("错误: {e}"));
    }
    tracing::info!("导入完成: 来源={} 结果={}", dir.display(), lines.join("；"));
    Ok(lines.join("；"))
}

/// 导出统计报告（CSV / HTML）。
/// 对话框 + 全历史扫描 + 写文件均为重 IO，整体放后台线程执行。
#[tauri::command]
pub async fn export_report(fmt: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let ext = if fmt == "csv" { "csv" } else { "html" };
        let file = rfd::FileDialog::new()
            .set_title("导出统计报告")
            .set_file_name(format!("focusflow_export.{ext}"))
            .add_filter(if fmt == "csv" { "CSV" } else { "HTML" }, &[ext])
            .save_file();
        let Some(path) = file else {
            return Ok("已取消".to_string());
        };

        let (total, stats) = focusflow_core::db::get_stats(None, None);
        let result = if fmt == "csv" {
            crate::export::export_csv(&path, total, &stats)
        } else {
            crate::export::export_html(&path, total, &stats)
        };
        result
            .map(|_| path.display().to_string())
            .map_err(|e| format!("导出失败: {e}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 维护信息：上次压缩时间 / 备份数量 / 最新备份（读目录与 DB，后台线程执行）。
#[tauri::command]
pub async fn get_maintenance_info() -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(|| {
        // 上次 VACUUM 时间（meta 表）
        let last_vacuum = focusflow_core::db::connection::with_ro_conn(
            &focusflow_core::paths::current_year_db_path(),
            |conn| {
                conn.query_row("SELECT value FROM meta WHERE key='last_vacuum'", [], |r| {
                    r.get::<_, String>(0)
                })
                .ok()
            },
        )
        .flatten();

        // 备份目录信息。读不出来必须与"目录里真的没有备份"分开说：便携包放在
        // 休眠的移动盘上、OneDrive 占位、`backup` 是个普通文件，read_dir 都会失败，
        // 而原先照样显示"备份数量 0"，看起来就像一次都没备份过
        // （同一族已在 vacuum_all / reset / backup_database 修过）。
        let backup_dir = focusflow_core::paths::backup_dir();
        let (mut backups, backup_error) = match std::fs::read_dir(&backup_dir) {
            Ok(it) => (
                it.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|e| e == "db"))
                    .collect::<Vec<_>>(),
                String::new(),
            ),
            Err(e) => (
                Vec::new(),
                format!("备份目录读不出来（{}）: {e}", backup_dir.display()),
            ),
        };
        backups.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
        let latest = backups
            .last()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string());

        // 备份异常体检留下的说明（backup/SUSPECT-*.txt），最近 2 条
        let suspect_notes: Vec<String> = {
            let mut notes: Vec<String> = Vec::new();
            let mut files: Vec<std::path::PathBuf> =
                std::fs::read_dir(focusflow_core::paths::backup_dir())
                    .map(|it| {
                        it.flatten()
                            .map(|e| e.path())
                            .filter(|p| {
                                p.file_name()
                                    .map(|n| n.to_string_lossy().starts_with("SUSPECT-"))
                                    .unwrap_or(false)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
            files.sort();
            for p in files.iter().rev().take(2) {
                if let Ok(text) = std::fs::read_to_string(p) {
                    // 只取前两行（第一行结论 + 时间），够提示即可
                    notes.push(text.lines().take(2).collect::<Vec<_>>().join("｜"));
                }
            }
            notes
        };

        serde_json::json!({
            "last_vacuum": last_vacuum,
            "backup_count": backups.len(),
            "latest_backup": latest,
            "backup_error": backup_error,
            "suspect_notes": suspect_notes,
        })
    })
    .await
    .map_err(|e| e.to_string())
}

/// 启动自检汇总（B15）：init 路径上各子系统「起没起来」的结论。
/// 全绿静默 —— 前端只在有失败项时 toast；托盘 tooltip 也会带 ⚠ 计数。
/// 静态数据（init 完成后不再变），直接同步返回。
#[tauri::command]
pub fn get_startup_report(
    state: State<'_, Arc<AppState>>,
) -> Vec<focusflow_core::startup::CheckResult> {
    state.startup_report.clone()
}

/// 立即备份数据库，返回备份文件路径（重 IO；后台线程执行）。
#[tauri::command]
pub async fn do_backup(state: State<'_, Arc<AppState>>) -> Result<String, String> {
    let max_backups = state.config.get_int("database", "max_backups", 5).max(1);
    let db = Arc::clone(&state.db);
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        db.flush(true);
        use focusflow_core::db::maintenance::BackupOutcome;
        match focusflow_core::db::maintenance::backup_database(max_backups) {
            BackupOutcome::Done { first, count } => {
                Ok(format!("{}（共 {count} 份）", first.display()))
            }
            BackupOutcome::NothingToDo => Err("没有需要备份的数据库".to_string()),
            BackupOutcome::Failed { reason } => Err(reason),
        }
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(outcome)
}

/// 调试：前端写入日志（定位悬浮窗拖动问题）。发布版为 no-op。
#[tauri::command]
pub fn dbg_log(msg: String) {
    #[cfg(debug_assertions)]
    tracing::info!("[floating-debug] {msg}");
    #[cfg(not(debug_assertions))]
    let _ = msg;
}

/// 设置设备别名（`alias` 为空则清除），返回最终保存的别名。
///
/// 别名存在 `data/device_aliases.json`（与统计库解耦，可手改），
/// 写完后触发一次重聚合，界面立即显示新名字。
#[tauri::command]
pub fn set_device_alias(
    state: State<'_, Arc<AppState>>,
    key: String,
    alias: String,
) -> Result<String, String> {
    if key.trim().is_empty() {
        return Err("设备标识为空".to_string());
    }
    let saved = focusflow_core::device_alias::clamp_alias(&alias);
    focusflow_core::device_alias::set(&key, &saved).map_err(|e| e.to_string())?;
    state
        .refresh_now
        .store(true, std::sync::atomic::Ordering::Relaxed);
    tracing::info!("设备别名已更新: {} -> {}", key, saved);
    Ok(saved)
}

/// 设备详情（点击「设备排行」某一行时按需查询）。
/// `period` 与统计视图一致：-1 今日 / 0 全部 / n 近 n 天。
///
/// async：核心实现要跨每个年度库跑 4 趟（设备日序列 + 设备键名明细 +
/// 设备排行 + 类型回退），同步命令会在主线程跑完，年份一多点击就卡窗口。
#[tauri::command]
pub async fn get_device_detail(key: String, period: i64) -> Result<serde_json::Value, String> {
    if key.trim().is_empty() {
        return Err("设备标识为空".to_string());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let detail = focusflow_core::db::get_device_detail(&key, period);
        serde_json::to_value(detail).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 插件管理页打开/关闭时切换热重载监听（打开才扫描，平时零后台开销）。
#[tauri::command]
pub fn plugins_watch(watch: bool) {
    crate::plugins::set_watch(watch);
}

/// 列出插件（含已停用的，供管理页显示开关）。
#[tauri::command]
pub fn get_plugins(state: State<'_, Arc<AppState>>) -> Vec<PluginMeta> {
    crate::plugins::with_manager(&state.db, |pm| {
        pm.list_discovered()
            .into_iter()
            .map(|p| PluginMeta {
                name: p.name,
                desc: p.desc,
                version: p.version,
                author: p.author,
                file: p.file,
                enabled: p.enabled,
                loaded: p.loaded,
                error: p.error,
            })
            .collect()
    })
}

/// 启用/停用插件（按文件名）。
///
/// 停用：卸载已加载实例（调用 cleanup），之后不再加载、不再接收键事件；
/// 启用：立即加载并持久化到 config.ini `[plugins] disabled`。
#[tauri::command]
pub fn set_plugin_enabled(
    state: State<'_, Arc<AppState>>,
    file: String,
    enabled: bool,
) -> Result<bool, String> {
    // 只有"启用但加载失败"才会 Err（配置写入本身是去抖队列，永远 Ok）。
    // 原先这里丢掉结果、回一句 Ok(enabled)，于是插件带着 Lua 语法错误也能弹「插件已启用」。
    crate::plugins::with_manager(&state.db, |pm| pm.set_enabled(&file, enabled))?;
    Ok(enabled)
}
