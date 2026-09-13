//! Tauri 命令：暴露统计/配置/监听/数据操作给前端。

use std::sync::Arc;

use tauri::{AppHandle, Emitter, Manager, State};

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
        "hotkey_enabled": c.get("hotkey", "enabled") == "true",
        "hotkey_str": c.get("hotkey", "toggle_window"),
        "floating_enabled": c.get("floating", "enabled") == "true",
    })
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

/// 显示悬浮窗。
#[tauri::command]
pub fn show_floating(app: AppHandle) {
    if let Some(win) = app.get_webview_window("floating") {
        crate::state::set_webview_rendering(&app, "floating", true);
        let _ = win.show();
        let _ = win.set_focus();
    }
}

/// 隐藏悬浮窗。
#[tauri::command]
pub fn hide_floating(app: AppHandle) {
    if let Some(win) = app.get_webview_window("floating") {
        let _ = win.hide();
        crate::state::set_webview_rendering(&app, "floating", false);
    }
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
        focusflow_core::db::maintenance::vacuum_all();
    })
    .await
    .map_err(|e| e.to_string())?;
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
        w.recompute_today_count();
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

        // 备份目录信息
        let mut backups: Vec<std::path::PathBuf> =
            std::fs::read_dir(focusflow_core::paths::backup_dir())
                .map(|it| {
                    it.flatten()
                        .map(|e| e.path())
                        .filter(|p| p.extension().is_some_and(|e| e == "db"))
                        .collect()
                })
                .unwrap_or_default();
        backups.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
        let latest = backups
            .last()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string());

        serde_json::json!({
            "last_vacuum": last_vacuum,
            "backup_count": backups.len(),
            "latest_backup": latest,
        })
    })
    .await
    .map_err(|e| e.to_string())
}

/// 立即备份数据库，返回备份文件路径（重 IO；后台线程执行）。
#[tauri::command]
pub async fn do_backup(state: State<'_, Arc<AppState>>) -> Result<String, String> {
    let max_backups = state.config.get_int("database", "max_backups", 5).max(1);
    let db = Arc::clone(&state.db);
    tauri::async_runtime::spawn_blocking(move || {
        db.flush(true);
        focusflow_core::db::maintenance::backup_database(max_backups)
    })
    .await
    .map_err(|e| e.to_string())?
    .ok_or_else(|| "备份失败".to_string())
    .map(|p| p.display().to_string())
}

/// 调试：前端写入日志（定位悬浮窗拖动问题）。发布版为 no-op。
#[tauri::command]
pub fn dbg_log(msg: String) {
    #[cfg(debug_assertions)]
    tracing::info!("[floating-debug] {msg}");
    #[cfg(not(debug_assertions))]
    let _ = msg;
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
    let ok = crate::plugins::with_manager(&state.db, |pm| pm.set_enabled(&file, enabled));
    if !ok {
        return Err("写入插件启用状态失败".to_string());
    }
    Ok(enabled)
}
