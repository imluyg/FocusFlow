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
        period_total: s.agg.period_total,
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
    let data_home = focusflow_core::paths::data_home();
    let app_dir = focusflow_core::paths::app_dir();
    serde_json::json!({
        "theme": c.get("gui", "theme"),
        "paused": state.listener.is_paused(),
        // 最近一次配置落盘的失败原因（空/缺省 = 上一次写盘是成功的）。
        // 必须带出来：`set_config` 那条命令永远返回 Ok（真正写盘的是 300ms 之后的
        // 去抖线程），界面拿不到这一句就只能一直显示"已经改好了"，而重启后全部回退。
        "config_save_error": c.last_save_error().unwrap_or_default(),
        // 布尔项一律走 `get_bool`，不能拿字符串比 "==true"：`get_bool` 认
        // `true/1/yes/on`，而这里以前只认字面量 "true" —— 手改 config.ini 写成
        // `enabled = 1` 时功能是**开的**（hotkey.rs / state.rs 都用 get_bool 读），
        // 设置页的勾却是灭的，两个入口对同一件事两把尺子。默认值与权威读者对齐：
        // [hotkey] enabled 的默认是 false，[floating] enabled 的默认是 true。
        "hotkey_enabled": c.get_bool("hotkey", "enabled", false),
        "hotkey_str": c.get("hotkey", "toggle_window"),
        // 已启用但注册失败时的原因（空串 = 正常）：组合键常被其它程序占用，
        // 只有日志的话用户看到的是「开关打开着、按了没反应」
        "hotkey_error": crate::hotkey::last_error("toggle_window").unwrap_or_default(),
        // 截图热键：与上面同一条 `enabled` 总开关管着，但失败原因各记各的格
        "snip_hotkey_str": c.get("hotkey", "snip"),
        "snip_hotkey_error": crate::hotkey::last_error("snip").unwrap_or_default(),
        // 标注版截图热键：同一个总开关、同样各记各的失败原因
        "snip_annotate_hotkey_str": c.get("hotkey", "snip_annotate"),
        "snip_annotate_hotkey_error": crate::hotkey::last_error("snip_annotate").unwrap_or_default(),
        // 截图落盘位置：报**当前真正生效**的那个（与 data_home 同一口径，见下面那行）
        "screenshots_dir": focusflow_core::paths::screenshots_dir().display().to_string(),
        "floating_enabled": c.get_bool("floating", "enabled", true),
        // 备份开关：退出时备份 / 运行中定时备份（0 小时 = 关闭）
        "backup_on_exit": c.get_bool("database", "backup_on_exit", true),
        "backup_online_hours": c.get_int("database", "online_backup_interval_hours", 24),
        "max_backups": c.get_int("database", "max_backups", 5),
        // 每日目标次数（连续打卡的判定线）
        "goal_daily_keys": c.get_int("goal", "daily_keys", 20000),
        // 数据目录：报**当前真正生效**的那个，而不是 config.ini 里写的那个 ——
        // 配的目录建不出来时 paths 会回落程序目录并记 error（见 resolve_data_home），
        // 照配置文件显示就会写着"数据在 D:\x"而其实一直写在程序目录里。
        "data_home": data_home.display().to_string(),
        // 数据与程序是否还在同一个目录（= 从没挪出去过）；前端据此给一句可选提示
        "data_home_shared_with_app": data_home == app_dir,
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
    // 尺子在 `stats::effective_goal` 里（周报那半边过的是同一个函数）：
    // 各自写 `.max(1)` / 不写，就是"同一页两把尺子"的成因。
    let goal =
        focusflow_core::stats::effective_goal(app.config.get_int("goal", "daily_keys", 20000));
    // 先取按日序列、后读未落库增量：两步之间若恰好有一次 flush 落库，增量已进库、
    // 此刻读到的 pending 就是 0 —— 最坏少算一个瞬时切片，绝不会把同一段计两遍
    // （反过来先读 pending 再查库，中间落库就是双重计数）。
    // 与主卡片（compute_charts / total_for_display）同一族口径：顶部「今日」是
    // 实时值，这里的达标/连续/最长若只认库值，用户刚跨线的一分钟里两处数字打架。
    //
    // 先问这一窗口的可读性：`get_daily_counts` 对打不开的那一年是**静默跳过**（只记一行日志），
    // 于是那一年每一天都是 0，`stats::goal_status` 把 0 读成"那天没达标" ⇒「连续打卡 N 天」
    // 悄悄缩到跨年之后的数，而横条照旧是绿色。导出、周报与 CLI 那几条读命令都过这道闸，
    // 界面这一头当时漏下了。只问窗口里那几个库：更早那一年坏了不该让这条自动刷新的横条
    // 整块变红。前端已经有红色的通道（`views.js` 那句「连续打卡读不出来：…」），
    // 所以"拒绝"不是没有落点，而是把它本来就该说的话说明白。
    focusflow_core::db::queries::check_daily_counts_readable(
        focusflow_core::stats::GOAL_LOOKBACK_DAYS,
        None,
    )
    .map_err(|e| format!("连续打卡的账本读不全：{e}"))?;
    let rows = focusflow_core::db::queries::get_daily_counts(
        focusflow_core::stats::GOAL_LOOKBACK_DAYS,
        None,
    );
    let pending = app
        .db
        .writer()
        .map(|w| w.today_pending_count())
        .unwrap_or(0);
    let today = chrono::Local::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string();
    let s = focusflow_core::stats::goal_status(goal, &rows, &today, pending);
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
    // 热键配置变更后运行时重新注册，无需重启。判据在 `must_reload_hotkey` 里，
    // 且键名集合是从 `hotkey::BINDINGS` 推的 —— 见那里关于截图热键的说明。
    if must_reload_hotkey(&section, &key) {
        crate::hotkey::reload_hotkey(&app);
    }
    // 悬浮窗开关：这个键原先只落盘不生效（全仓唯一的读者是启动时的 setup_windows），
    // 所以设置页勾完要到下次重启才看得见变化 —— 而"立即隐藏"按钮是马上生效的，
    // 两个入口给两种反馈，勾一次没反应的那条会被当成坏了。这里跟着即时显示/隐藏。
    if section == "floating" && key == "enabled" {
        crate::state::set_floating_visible(&app, value == "true");
    }
    // 主题改了要重贴窗口材质：那一层的深浅是按"贴的那一刻"定死的（见 refresh_window_vibrancy）
    if must_reapply_vibrancy(&section, &key) {
        crate::state::refresh_window_vibrancy(&app);
    }
    Ok(())
}

/// 设置页刚写完 `[hotkey]` 的某个键，要不要重注册全局热键。
///
/// 纯函数只为一个原因：让用例能把"`[hotkey]` 里每一条组合键都要触发重注册"钉住。
/// 这条判据原先写死成 `key == "enabled" || key == "toggle_window"`，v0.4.3 加截图热键时
/// 没人回来补 `snip` —— 症状是"改了组合键：旧键还占着、新键按了没反应、设置页毫无报错"
/// （不重注册就没人去写 `LAST_ERRORS` 那一格），而且编译、用例、门禁全绿，只能靠人手按。
/// 现在键名集合从 `hotkey::BINDINGS` 推，加一条绑定两处自动一起长。
fn must_reload_hotkey(section: &str, key: &str) -> bool {
    section == "hotkey" && (key == "enabled" || crate::hotkey::is_binding_key(key))
}

/// 设置页写完一个键之后，要不要重贴窗口材质。
///
/// 会改变材质外观的只有 `[gui] theme`：它既决定 Mica 那条分支传给
/// `DWMWA_USE_IMMERSIVE_DARK_MODE` 的档位，也直接决定 Acrylic 那条分支的着色 tint。
/// 判据抽成纯函数是为了让用例把"哪些键要重贴、哪些不许白贴一遍"钉成一张表
/// （与 [`must_reload_hotkey`] 同一个手法：那条写死两个键时漏掉了后来新增的截图热键）。
fn must_reapply_vibrancy(section: &str, key: &str) -> bool {
    section == "gui" && key == "theme"
}

/// 切换暂停记录，返回新的暂停状态，并广播给前端（与托盘切换保持一致）。
#[tauri::command]
pub fn toggle_pause(state: State<'_, Arc<AppState>>, app: AppHandle) -> bool {
    // 走 AppState 的这一层：暂停时它会把写线程的活跃锚清零（见 `AppState::toggle_pause`）。
    let paused = state.toggle_pause();
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
pub async fn vacuum_db(state: State<'_, Arc<AppState>>) -> Result<String, String> {
    let db = Arc::clone(&state.db);
    let line = tauri::async_runtime::spawn_blocking(move || {
        db.flush(true);
        let report = focusflow_core::db::maintenance::vacuum_all();
        // 以前这里 `let _ =`：哪个库没压缩成、甚至一个库都没枚举到，界面都照旧显示
        // 「压缩完成」。前端 doVacuum 的 catch 会把这句原样打在设置页的消息位上。
        //
        // 成功那句现在也由 core 的 `vacuum_line` 说：它按**真的压成了几套**说话，
        // 于是一套库都没扫到时界面拿到的不是「压缩完成」，而是「没有可压缩的年度库」——
        // 与 CLI 的 `--vacuum` 共用同一句（两侧各写一遍时漏的就是这条臂）。
        if report.incomplete() {
            Err(report.why_incomplete())
        } else {
            Ok(focusflow_core::db::maintenance::vacuum_line(&report))
        }
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(line)
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

    let text = import_summary_text(&summary);
    // 部分失败必须走 Err：前端 `doImport` 的成功分支把整句渲染成**绿色**"完成"，
    // 于是"2025 年度键鼠: 1200 条；错误: 2024 年度库导入失败"看着就是一次成功导入 ——
    // 用户不会再重试那一年，那年的历史从此缺一块。`import_legacy_data` 从不返回 Err
    // （每个年度库失败只往 summary.errors 里 push），所以结论只能在这里判。
    let outcome = import_outcome(text, &summary.errors);
    match &outcome {
        Ok(t) => tracing::info!("导入完成: 来源={} 结果={}", dir.display(), t),
        Err(t) => tracing::error!("导入部分失败: 来源={} {}", dir.display(), t),
    }
    outcome
}

/// 把导入的统计文本与失败清单合成命令返回值：**有失败项就是 Err**。
///
/// 把 `ImportSummary` 合成界面上那句话。抽成纯函数的原因与 [`import_outcome`] 一样：
/// `import_legacy` 要先弹目录选择框，测试起不来那条路。
///
/// 「未发现可导入的数据」这一句加了 **`errors` 为空**这个前提：每个年度库都失败时
/// `year_dbs`/`copied_aux` 同样是空的，那句话会把"导入失败"说成"那个目录本来就没东西"
/// —— 用户于是不会再重试那一年，那年的历史从此缺一块。CLI 侧同一判据早就补了
/// `&& !failed`（`import_report`），桌面这侧是漏下的另一半。
fn import_summary_text(summary: &focusflow_core::migration::ImportSummary) -> String {
    let mut lines: Vec<String> = Vec::new();
    if summary.year_dbs.is_empty() && summary.copied_aux.is_empty() && summary.errors.is_empty() {
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
    // 跳过清单里每一项都是"这一年/这一份没并进来东西"，不显示就等于没说：
    // `records_by_year` 只会把它们排成「2025 年度键鼠: 0 条」，与"那一年本来没数据"
    // 完全同形（CLI 那侧早就在打印 `summary.skipped`，桌面侧漏了这半边）。
    for note in &summary.skipped {
        lines.push(format!("跳过: {note}"));
    }
    lines.join("；")
}

/// 抽成纯函数只有一个原因：`import_legacy` 要先弹目录选择框，测试起不来那条路，
/// 而"部分失败不能报成成功"这一判据正好是这条命令里唯一会骗人的地方。
fn import_outcome(text: String, errors: &[String]) -> Result<String, String> {
    if errors.is_empty() {
        return Ok(text);
    }
    Err(format!("{text}｜失败项：{}", errors.join("；")))
}

/// 更改数据目录：选文件夹 → 写 `[paths] data_home` 与 `data_migrate_from` → 自动重启；
/// 真正的搬运由**新进程**在开库之前做（见 `data_location::run_pending_migration`）。
/// 日志、插件、config.ini 留在程序目录不动（分工见 `paths::data_home`）。
///
/// 为什么不当场就把目录搬走：本进程从解析出数据目录那一刻起就一直按它写 —— 写线程、
/// 备份/采样/统计线程都是常驻的。当场搬完之后到进程真退出之间还会往旧目录落一截增量，
/// 新目录里没有那一份，表现就是"搬完还差一截"，而且旧目录从此变成一个"看着还在被用、
/// 其实早就停了"的陷阱。让旧进程把自己那套写完整、备份完整再死，新进程搬走的就是完整结果。
///
/// 为什么这里要先 `validate_new_data_home` 判一次：不合法的目录（同一个目录、选进了
/// 正在被搬的子树、里面已有无关文件、根本建不出来）原本要等到重启后搬运时才暴露，
/// 那时界面早没了，用户只会觉得"点了没反应"。当场判，话就能显示在设置页上。
#[tauri::command]
pub async fn change_data_dir(
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
) -> Result<String, String> {
    let picked = tauri::async_runtime::spawn_blocking(|| {
        rfd::FileDialog::new()
            .set_title("选择新的数据文件夹（data 与 backup 会整体搬进这里）")
            .pick_folder()
    })
    .await
    .map_err(|e| e.to_string())?;
    let Some(dir) = picked else {
        return Ok("已取消".to_string());
    };

    let config = state.config;
    let handle = app.clone();
    let dir_for_thread = dir.clone();
    let (from, scheduled) = tauri::async_runtime::spawn_blocking(
        move || -> Result<(std::path::PathBuf, bool), String> {
            let from = focusflow_core::data_location::current_data_home();
            // 顺序是故意的：先写标记、再写新目录。万一中间落盘失败，留下的状态是
            // "标记指向旧目录 + data_home 仍是旧目录"，新进程一看两者相同，只会清掉标记
            // 并记一条 warn，不会搬运、也不会把数据指到一个空目录上。
            // 校验用的就是刚算出来的这一个 `from`：它与后面真正要搬走的源必须是同一个串。
            focusflow_core::data_location::validate_new_data_home(&from, &dir_for_thread)
                .map_err(|e| format!("{e}；数据目录没有改动，也不会重启"))?;
            config
                .set("paths", "data_migrate_from", &from.to_string_lossy())
                .map_err(|e| format!("写入迁移标记失败: {e}；数据目录没有改动，也不会重启"))?;
            config
                .set("paths", "data_home", &dir_for_thread.to_string_lossy())
                .map_err(|e| format!("写入新数据目录失败: {e}；请重新再点一次更改"))?;
            // 同步落盘，不等去抖的 config-saver：新进程要不要搬运、搬到哪儿，全看这两行
            // 在旧进程退出前真的写进了 config.ini。
            config
                .save()
                .map_err(|e| format!("配置落盘失败: {e}；数据目录没有改动，也不会重启"))?;
            let scheduled = crate::state::schedule_app_restart(&handle, true);
            Ok((from, scheduled))
        },
    )
    .await
    .map_err(|e| e.to_string())??;

    if !scheduled {
        // 配置写好、进程没重启 —— 这时候说"程序即将重启"就是句谎话，用户会干等着。
        // 实话是：下次真的打开程序时会自动把这一趟补完（run_pending_migration 认配置里
        // 的标记），而在此之前按键照旧记在旧目录里，一段都不会丢。
        tracing::error!(
            "更改数据目录：配置已写入（{} → {}），但重启没能安排，需要用户重新启动一次",
            from.display(),
            dir.display()
        );
        return Err(format!(
            "配置已写入（{} → {}），但这次没能安排成重启：请退出程序后重新打开，届时会自动完成搬运",
            from.display(),
            dir.display()
        ));
    }

    let msg = format!(
        "已安排把数据从 {} 搬到 {}：程序即将重启，重启时先复制并逐文件核对，确认无误才删旧目录；搬运期间请不要输入",
        from.display(),
        dir.display()
    );
    tracing::info!(
        "更改数据目录: {} → {}（搬运在新进程启动时进行）",
        from.display(),
        dir.display()
    );
    Ok(msg)
}

/// 导出统计报告（CSV / HTML）。
/// 对话框 + 全历史扫描 + 写文件均为重 IO，整体放后台线程执行。
#[tauri::command]
pub async fn export_report(fmt: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        // 先问可读性，再挑文件名：`available_years()` 把 `read_dir` 失败折成空列表、
        // `stats_multi_year` 把打不开的那一年静默跳过，所以移动盘没插、或某年的库被
        // 同步盘换成占位文件时，原先这条路会写出一份"总活跃次数 0"或明显偏小的报告，
        // 而界面报绿色成功。CLI 那半边早就有这句话了（判据在 core，两边共用一把尺子）。
        if let Err(e) = focusflow_core::db::queries::check_years_readable() {
            return Err(format!("导出被拒绝：{e}"));
        }
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

/// 备份文件的时间（抽成具名函数：注入给 `pick_latest_backup`，用例才能造出"这一份问不出"）。
fn file_mtime(p: &std::path::Path) -> std::io::Result<std::time::SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified())
}

/// 备份异常体检留下的说明原文。
fn read_note_text(p: &std::path::Path) -> std::io::Result<String> {
    std::fs::read_to_string(p)
}

/// 从这些 `.db` 备份里挑最新的一条，并交出「最新」这条说不准的原因。
///
/// 原先是 `backups.sort_by_key(|p| metadata(p).and_then(|m| m.modified()).ok())` ——
/// `Err` 折成 `None`，而 `None` 在这个键序里排**最前** ⇒ 正被杀软实时扫描/同步盘握着的那一份
/// 自动降级成"最旧"，界面那句「最新」念的是另一个文件，份数却照样把它算进去，一处线索都不留。
/// 问不出的那几份**可能**正是最新的一份，所以既不假装没有最新，也不照念问得出的那个，
/// 而是把"只在问得出的 N 份里挑"说出来。（上一次修的"哪个时间戳算新"是另一回事。）
fn pick_latest_backup(
    db_files: &[std::path::PathBuf],
    mtime: impl Fn(&std::path::Path) -> std::io::Result<std::time::SystemTime>,
) -> (Option<String>, Option<String>) {
    let mut timed: Vec<(std::time::SystemTime, &std::path::PathBuf)> = Vec::new();
    let mut unknown = 0usize;
    let mut first_error = String::new();
    for p in db_files {
        match mtime(p) {
            Ok(t) => timed.push((t, p)),
            Err(e) => {
                unknown += 1;
                if first_error.is_empty() {
                    first_error = e.to_string();
                }
            }
        }
    }
    timed.sort_by_key(|(t, _)| *t);
    let latest = timed
        .last()
        .and_then(|(_, p)| p.file_name())
        .map(|n| n.to_string_lossy().to_string());
    let error = (unknown > 0).then(|| {
        format!(
            "有 {unknown} 份的时间问不出（{first_error}），下面这条只在问得出的 {} 份里挑",
            timed.len()
        )
    });
    (latest, error)
}

/// 备份异常说明（`SUSPECT-*.txt`）：取最近两条，读不动的那条要点名。
///
/// 原先是 `if let Ok(text) = read_to_string(p)` —— 那份说明是"轮转被冻住"的唯一现场证据，
/// 打不开就等于"没有异常"，界面一个字都不提。页面最多念两条，所以**被截断也要说出来**，
/// 否则"只剩两条"与"只有两条"又是一个形状。
fn suspect_notes(
    mut files: Vec<std::path::PathBuf>,
    read: impl Fn(&std::path::Path) -> std::io::Result<String>,
) -> Vec<String> {
    files.sort();
    let hidden = files.len().saturating_sub(2);
    let mut out: Vec<String> = files
        .iter()
        .rev()
        .take(2)
        .map(|p| match read(p) {
            // 只取前两行（第一行结论 + 时间），够提示即可
            Ok(text) => text.lines().take(2).collect::<Vec<_>>().join("｜"),
            Err(e) => {
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                format!("{name} 这份说明读不出来: {e}")
            }
        })
        .collect();
    if hidden > 0 {
        out.push(format!(
            "另有 {hidden} 条更早的异常说明没列出（这里只念最近两条）"
        ));
    }
    out
}

/// 维护信息：上次压缩时间 / 备份数量 / 最新备份（读目录与 DB，后台线程执行）。
#[tauri::command]
pub async fn get_maintenance_info() -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(|| {
        // 上次 VACUUM 时间（meta 表）。"查不到"与"从没压过"要分开说：原先两者都折成
        // `null`，界面一律显示"尚未压缩"，于是"当年库被同步盘换成占位文件了"被报成
        // "一次都没压缩"——与下面备份目录那一族同一个毛病（那边已修）。
        let (last_vacuum, last_vacuum_error) =
            match focusflow_core::db::maintenance::last_vacuum_stamp(
                &focusflow_core::paths::current_year_db_path(),
            ) {
                Ok(stamp) => (stamp, String::new()),
                Err(e) => (None, e),
            };

        // 备份目录信息。读不出来必须与"目录里真的没有备份"分开说：便携包放在
        // 休眠的移动盘上、OneDrive 占位、`backup` 是个普通文件，read_dir 都会失败，
        // 而原先照样显示"备份数量 0"，看起来就像一次都没备份过
        // （同一族已在 vacuum_all / reset / backup_database 修过）。
        let backup_dir = focusflow_core::paths::backup_dir();
        // 一次列目录就够：原先数份数与挑说明**各 read_dir 一次**，第二次失败被
        // `unwrap_or_default()` 折成"没有异常说明"，而 `backup_error` 只覆盖第一次那一个问。
        let (all, backup_error) = match std::fs::read_dir(&backup_dir) {
            Ok(it) => (
                it.flatten().map(|e| e.path()).collect::<Vec<_>>(),
                String::new(),
            ),
            Err(e) => (
                Vec::new(),
                format!("备份目录读不出来（{}）: {e}", backup_dir.display()),
            ),
        };
        let db_files: Vec<std::path::PathBuf> = all
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "db"))
            .cloned()
            .collect();
        let notes: Vec<std::path::PathBuf> = all
            .iter()
            .filter(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().starts_with("SUSPECT-"))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        let (latest, latest_backup_error) = pick_latest_backup(&db_files, file_mtime);
        let suspect_notes = suspect_notes(notes, read_note_text);

        serde_json::json!({
            "last_vacuum": last_vacuum,
            "last_vacuum_error": last_vacuum_error,
            "backup_count": db_files.len(),
            "latest_backup": latest,
            "backup_error": backup_error,
            "latest_backup_error": latest_backup_error,
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
    tauri::async_runtime::spawn_blocking(move || {
        let flushed = db.flush_confirmed(true);
        backup_outcome_with_flush(
            backup_message(&focusflow_core::db::maintenance::backup_database(
                max_backups,
            )),
            flushed,
        )
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 「立即备份」的最终结论：备份文件产出来了，还要确认它**包含**此刻内存里的增量。
///
/// `flush(true)` 只等 3 秒：写线程正卡在 SQLite 的 busy_timeout（15 秒）或跨年建库时
/// 会超时，而备份照样能成功读出库 —— 于是那份备份少了最后一批还没落库的统计，界面却
/// 报「备份完成」。这与退出备份那边修过的「少了最后一截」是同一件事，只是入口不同。
/// 备份本身照做（文件总比没有强），但结论必须是 Err，把「等几秒再备一次」说出去。
fn backup_outcome_with_flush(
    backup: Result<String, String>,
    flushed: bool,
) -> Result<String, String> {
    // 备份本身失败时原错误直接上抛：那句已经说清缺的是哪个库，再叠一句落盘警告只会
    // 把真正的失败原因挤到后面。
    let line = backup?;
    if flushed {
        return Ok(line);
    }
    Err(format!(
        "{line}；但写入线程没有确认落盘（等回音超时，或那批里有年度库最终没写进去），这份备份可能少了最后一批还没进库的统计 —— 等几秒再备份一次"
    ))
}

/// 「立即备份」这句结论的**唯一**判据：只有全都兜住才许是 Ok。
///
/// 部分失败原先也返回 `Ok`，把失败清单拼在成功串后面 —— 而前端 `doBackup()`
/// 拿到 Ok 就无条件渲染成绿字「备份完成: 」+ 那句尾巴（`views.js`），于是用户看到的
/// 是一句成功，而尾巴上写的恰恰是"统计库这次没备份出去"。同一个形状在导入那侧
/// 早就改成 `import_outcome`（errors 非空 ⇒ Err），这是漏下的另一半。
///
/// 判据不重写一遍：走 core 的 `BackupOutcome::is_complete()`。那个函数在 core 里
/// 有用例钉着、却没有任何生产调用方，"完成与否"到两个界面上各写了一遍
/// `failed.is_empty()` —— 本仓最常见的那把抄了两处的尺子。
fn backup_message(
    outcome: &focusflow_core::db::maintenance::BackupOutcome,
) -> Result<String, String> {
    use focusflow_core::db::maintenance::BackupOutcome as B;
    match outcome {
        B::Done {
            first,
            count,
            failed,
        } => {
            let line = format!("{}（共 {count} 份）", first.display());
            if outcome.is_complete() {
                Ok(line)
            } else {
                // 前端把 Err 的整句话渲染成红字并冠以「备份失败: 」。这句话要自己
                // 就能说清"拿到过几份、缺的是哪一个库"，而不是只喊一次失败。
                Err(format!(
                    "只兜住 {count} 份（最新 {}），但这些库没备份出去: {failed:?}",
                    first.display()
                ))
            }
        }
        // 没东西可备份不算成功也不算损坏：按钮要的是"这次有没有落盘"这句话
        B::NothingToDo => {
            Err("没有需要备份的数据库（新建或刚重置过，或历史库都与上次一致）".to_string())
        }
        B::Failed { reason } => Err(reason.clone()),
    }
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
///
/// 目录读不出来时回 `Err(原因)` 而不是空列表：空列表在页面上画的是「暂无插件」，
/// 那句话在这种时候是假的（插件还在盘上），而且用户会照着它去重传插件。
/// 前端这一头的拒绝通道本来就有（`plugins.js` 的 catch），只是过去只会画「加载失败」。
#[tauri::command]
pub fn get_plugins(state: State<'_, Arc<AppState>>) -> Result<Vec<PluginMeta>, String> {
    crate::plugins::with_manager(&state.db, |pm| {
        let list = pm.list_discovered()?;
        Ok(list
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
            .collect::<Vec<_>>())
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

#[cfg(test)]
mod tests {
    use super::{
        backup_message, backup_outcome_with_flush, import_outcome, import_summary_text,
        must_reload_hotkey, pick_latest_backup, suspect_notes,
    };
    use focusflow_core::db::maintenance::BackupOutcome;

    /// 备份部分失败时设置页不能拿到绿色那句「备份完成」。
    ///
    /// 与下面那条导入的同族用例同一个形状：前端只看 `Ok`/`Err` 两个分支，
    /// 把失败清单拼进 `Ok` 的字符串里，它照样会印成成功。判据走 core 的
    /// `is_complete()`，这里同时钉"没兜住的库必须出现在这句话里"——
    /// 用户要能在红字上看出**是哪个库**没备份出去。
    #[test]
    fn a_partial_backup_is_not_returned_as_success() {
        let all = BackupOutcome::Done {
            first: std::path::PathBuf::from("backup/focusflow_2026_1.db"),
            count: 3,
            failed: vec![],
        };
        let ok = backup_message(&all).expect("全都兜住时才是 Ok");
        assert!(
            ok.contains("共 3 份") && ok.contains("focusflow_2026_1.db"),
            "成功串要带份数与位置：{ok}"
        );

        let partial = BackupOutcome::Done {
            first: std::path::PathBuf::from("backup/focusflow_2026_1.db"),
            count: 1,
            failed: vec!["focusflow_2025.db".to_string()],
        };
        let err = backup_message(&partial).expect_err("五库里只成一个绝不能是 Ok");
        assert!(
            err.contains("focusflow_2025.db") && err.contains("只兜住 1 份"),
            "红字里必须同时说清没兜住的是谁、已经拿到几份：{err}"
        );

        // 三态各归各位：无事可做与真失败都不是成功
        assert!(backup_message(&BackupOutcome::NothingToDo).is_err());
        assert_eq!(
            backup_message(&BackupOutcome::Failed {
                reason: "备份目录建不出来".to_string()
            })
            .unwrap_err(),
            "备份目录建不出来"
        );
    }

    /// 「立即备份」还要确认这份快照**包含**内存里的最后一批增量。
    ///
    /// `flush(true)` 只等 3 秒，超时后备份仍会成功读出一个偏旧的库；只回一句
    /// 「备份完成」等于把「少了最后一批」藏起来（与退出备份那条「少了最后一截」
    /// 同一族）。三腿：确认落盘 ⇒ 原句；没确认 ⇒ Err 且两句都在；备份本身失败 ⇒
    /// 原错误不被这句警告顶掉。
    #[test]
    fn a_backup_whose_flush_was_not_confirmed_is_not_a_clean_success() {
        let complete = Ok("backup/focusflow_2026_x.db（共 3 份）".to_string());
        assert_eq!(
            backup_outcome_with_flush(complete.clone(), true),
            Ok("backup/focusflow_2026_x.db（共 3 份）".to_string()),
            "确认落盘时结论原样交给界面"
        );
        let warned =
            backup_outcome_with_flush(complete, false).expect_err("没确认落盘不许报成干净成功");
        assert!(
            warned.contains("共 3 份"),
            "已经拿到的文件不能说丢: {warned}"
        );
        assert!(
            warned.contains("没有确认落盘"),
            "缺的那一段必须说出来: {warned}"
        );
        // 这句现在盖**两种**成因：等回音超时，以及写线程回了话但那批里有年度库没写进去
        // （`flush_pending` 现在把结论随回音交出来）。写死成"3 秒超时"那一种的文案，
        // 会让"库写不进去"这种更该重试的情况被说成"再等几秒就好"。
        assert!(
            warned.contains("等回音超时") && warned.contains("没写进去"),
            "两种成因都要点到，不能只报那一种: {warned}"
        );
        assert_eq!(
            backup_outcome_with_flush(Err("只兜住 1 份".to_string()), false),
            Err("只兜住 1 份".to_string()),
            "备份本身失败时原错误不能被一句警告顶掉"
        );
    }

    /// 接线腿：`do_backup` 必须先拿到 flush 的确认结果，再谈备份的结论。
    ///
    /// 纯函数用例盖不到"命令里到底调没调"。退回 `db.flush(true)`（无返回值）时
    /// 这条会红 —— 那正是这条缺陷原来的形状。
    #[test]
    fn do_backup_confirms_the_flush_before_claiming_success() {
        let src = include_str!("commands.rs");
        let head = src
            .find("pub async fn do_backup")
            .expect("立即备份命令找不到了");
        let body = &src[head..head
            + src[head..]
                .find("\n#[tauri::command]")
                .expect("do_backup 之后没有别的命令，切片前提变了")];
        assert!(
            !body.contains("db.flush(true)"),
            "退回无返回值的 flush ⇒ 超时又一次被藏起来"
        );
        let flush_at = body
            .find("flush_confirmed(true)")
            .expect("do_backup 没有确认落盘（flush 的返回值被丢了）");
        let decide_at = body
            .find("backup_outcome_with_flush(")
            .expect("do_backup 没走统一结论");
        assert!(
            flush_at < decide_at,
            "必须先确认落盘、再下结论（flush 在 {flush_at}、结论在 {decide_at}）"
        );
    }

    /// 时间问不出的那几份备份不许被排成"最旧"：「最新」要么说得出，要么承认是挑出来的。
    ///
    /// 旧形状 `sort_by_key(|p| metadata(p).and_then(|m| m.modified()).ok())` 把 `Err` 折成
    /// `None`，而 `None` 在这个键序里排最前 ⇒ 正被杀软/同步盘握着的那一份自动降级成最旧，
    /// 设置页那句「最新」念的是另一个文件，份数却照样算上它，一处线索都不留。
    /// 失败臂靠注入造（本机造不出真实的 stat 拒绝），两种句柄形状各管一条链。
    #[test]
    fn the_latest_backup_admits_when_a_mtime_cannot_be_read() {
        use std::collections::HashMap;
        let p = |n: &str| std::path::PathBuf::from(n);
        let files = vec![p("a_old.db"), p("b_unknown.db"), p("c_newer.db")];
        let times: HashMap<&str, Option<u64>> = HashMap::from([
            ("a_old.db", Some(1_000)),
            ("b_unknown.db", None), // 这一份其实是最大的那份
            ("c_newer.db", Some(2_000)),
        ]);
        let (latest, err) = pick_latest_backup(&files, |f| {
            match times[f.file_name().unwrap().to_str().unwrap()] {
                Some(secs) => Ok(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs)),
                None => Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "被占用",
                )),
            }
        });
        assert_eq!(
            latest.as_deref(),
            Some("c_newer.db"),
            "只能在问得出时间的那些里挑最新"
        );
        let why = err.expect("时间问不出时必须说清「最新」是挑出来的");
        assert!(
            why.contains("1 份") && why.contains("被占用") && why.contains("2 份"),
            "要点名有几份问不出、为什么，以及在几份里挑的: {why}"
        );
    }

    /// 对照腿：时间全问得出时不许念那句"说不准"，一份都没有时两样都不许有。
    ///
    /// 这一半单独成一条用例，是为了让"过度声明"那种改法有地方红 ——
    /// 它与上面那条住在同一个函数里时，第一个断言就先 panic，这半边等于没被盯。
    #[test]
    fn a_fully_readable_backup_set_claims_a_plain_latest() {
        use std::path::PathBuf;
        let p = |n: &str| PathBuf::from(n);
        let (l2, e2) = pick_latest_backup(&[p("a_old.db"), p("c_newer.db")], |f| {
            let secs = if f == p("a_old.db") { 1_000 } else { 2_000 };
            Ok(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
        });
        assert_eq!(l2.as_deref(), Some("c_newer.db"), "问得出时挑的就是真最新");
        assert!(e2.is_none(), "问得出时间时不该说「说不准」，实得 {e2:?}");

        let (l3, e3) = pick_latest_backup(&[], |_| Err(std::io::Error::other("用不上")));
        assert!(l3.is_none() && e3.is_none(), "一份都没有时两样都不该有");
    }

    /// 备份异常说明：读不动的那条要点名，被截断的条数也要说出来。
    ///
    /// 旧形状 `if let Ok(text) = read_to_string(p)` 把"这份说明打不开"折成"没有这条说明"：
    /// 那份说明正是"轮转被冻住"的唯一现场证据，读不动时界面一个字都不提。
    #[test]
    fn unreadable_and_hidden_suspect_notes_are_named() {
        let p = |n: &str| std::path::PathBuf::from(n);
        let files = vec![
            p("SUSPECT-2026-10-01.txt"),
            p("SUSPECT-2026-10-02.txt"),
            p("SUSPECT-2026-10-03.txt"),
        ];
        let out = suspect_notes(files, |f| match f.file_name().unwrap().to_str().unwrap() {
            "SUSPECT-2026-10-03.txt" => {
                Ok("骤降 12%\n2026-10-03 12:00｜时间\n第三行不进页面".to_string())
            }
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "被杀软握着",
            )),
        });
        assert_eq!(out.len(), 3, "最近两条 + 一条截断声明，实得 {out:?}");
        assert!(
            out[0].contains("骤降 12%") && out[0].contains("｜") && !out[0].contains("第三行"),
            "只取前两行并用竖线连: {}",
            out[0]
        );
        assert!(
            out[1].contains("这份说明读不出来") && out[1].contains("SUSPECT-2026-10-02.txt"),
            "读不动的那份要点名: {}",
            out[1]
        );
        assert!(
            out[2].contains("另有 1 条更早的"),
            "被截断要说出条数: {out:?}"
        );
    }

    /// 对照腿：都读得动、也没超过两条 ⇒ 就是两条，不多一句。
    ///
    /// 同样单独成条，好让"见谁都喊截断/见谁都喊读不出来"那种改法有地方红。
    #[test]
    fn readable_suspect_notes_are_shown_without_padding() {
        let p = |n: &str| std::path::PathBuf::from(n);
        let out2 = suspect_notes(vec![p("SUSPECT-a.txt"), p("SUSPECT-b.txt")], |_| {
            Ok("结论\n时间".to_string())
        });
        assert_eq!(
            out2,
            vec!["结论｜时间".to_string(), "结论｜时间".to_string()],
            "两页都读得动时不该多出一句"
        );
    }

    /// 接线腿：这条命令里只能有**一次**列目录，两处折损形状都不许回来。
    ///
    /// 纯函数用例盖不到"命令里到底调没调"——第二次 `read_dir(...).unwrap_or_default()`
    /// 与 `sort_by_key(...ok())` 都住在命令体里。
    #[test]
    fn maintenance_info_lists_the_backup_dir_only_once() {
        let src = include_str!("commands.rs");
        let head = src
            .find("pub async fn get_maintenance_info")
            .expect("维护信息命令找不到了");
        let body = &src[head..head
            + src[head..]
                .find("\n#[tauri::command]")
                .expect("它后面没有别的命令，切片前提变了")];
        // 先摘注释行：那些说明文字里就写着 `read_dir`、`unwrap_or_default()` 与 `.ok())`
        let code: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            code.matches("std::fs::read_dir(").count(),
            1,
            "列目录该只问一次（第二次的失败原先被折成「没有异常说明」）: {code}"
        );
        assert!(
            !code.contains("unwrap_or_default()"),
            "第二次列目录不许再折成空清单"
        );
        assert!(
            !code.contains(".ok())"),
            "时间/说明的读取失败不许再折成 None 参与排序"
        );
        assert!(
            code.contains("pick_latest_backup(&db_files, file_mtime)"),
            "挑最新要走那把问不出就承认的尺子"
        );
        assert!(
            code.contains("suspect_notes(notes, read_note_text)"),
            "异常说明要走那条会点名的画法"
        );
    }

    /// `[hotkey]` 里每一条组合键都必须触发重注册。判据的键名集合取自 `hotkey::BINDINGS`，
    /// 所以这张表以后加第三条绑定，这条用例自动跟着长 —— 把判据退回成写死的键名列表会立刻红
    /// （截图热键当年就是这么漏的）。
    #[test]
    fn every_hotkey_binding_key_retriggers_registration() {
        let keys = crate::hotkey::binding_keys();
        assert!(
            keys.len() >= 2,
            "绑定表读出来是空的，这条用例就没有观察量：{keys:?}"
        );
        for key in keys {
            assert!(
                must_reload_hotkey("hotkey", key),
                "设置页改了 {key} 却不重注册：新组合按了没反应、旧组合还占着、页面毫无报错"
            );
        }
        assert!(
            must_reload_hotkey("hotkey", "enabled"),
            "总开关必须能重注册"
        );
        // 反向两格：本段里不是热键的键、以及别的段的同名键，都不该惊动全局热键。
        assert!(!must_reload_hotkey("hotkey", "some_other_key"));
        assert!(!must_reload_hotkey("floating", "enabled"));
    }

    /// 导入只要有失败项就不能报成成功：前端把 `Ok` 的整句话渲染成绿色"完成"，
    /// 而 `import_legacy_data` 自己从不返回 Err。
    #[test]
    fn import_with_errors_is_not_reported_as_success() {
        let ok =
            import_outcome("2025 年度键鼠: 1200 条".to_string(), &[]).expect("没有失败项时该是 Ok");
        assert_eq!(ok, "2025 年度键鼠: 1200 条");
        let err = import_outcome(
            "2025 年度键鼠: 1200 条".to_string(),
            &["2024 年度库导入失败: 库被占用".to_string()],
        )
        .expect_err("有失败项必须是 Err，否则那一句走的是绿字");
        // 已经成功的那半也要留在消息里：用户得知道哪些年进来了、哪些没进来
        assert!(err.contains("1200 条"), "已导入的部分不能丢: {err}");
        assert!(err.contains("2024"), "失败项要能定位到是哪一年: {err}");
    }

    /// 「未发现可导入的数据」只能在**既没导进东西、也没报错**时说。
    ///
    /// 每个年度库都失败时 `year_dbs`/`copied_aux` 同样是空的，旧写法于是给出
    /// 「未发现可导入的数据｜失败项：2024 年度库导入失败」—— 前半句把失败说成"那目录
    /// 本来就没东西"，用户看完不会再去重试那一年，那年的历史从此缺一块。
    /// CLI 的 `import_report` 早就带了 `&& !failed`，桌面这一侧是漏下的另一半。
    #[test]
    fn an_all_failed_import_does_not_say_there_was_nothing_to_import() {
        use focusflow_core::migration::ImportSummary;

        let all_failed = ImportSummary {
            errors: vec!["2024 年度库导入失败: 库被占用".to_string()],
            ..ImportSummary::default()
        };
        let text = import_summary_text(&all_failed);
        assert!(
            !text.contains("未发现可导入的数据"),
            "有失败项时不许说那目录本来就没东西: {text}"
        );
        let outcome = import_outcome(text, &all_failed.errors).expect_err("有失败项必须是 Err");
        assert!(
            outcome.contains("2024 年度库导入失败"),
            "失败项本身仍然要留在消息里: {outcome}"
        );

        // 正向腿一：真的是空目录（无所获也无失败）时那句话仍然要说
        let nothing = ImportSummary::default();
        assert!(
            import_summary_text(&nothing).contains("未发现可导入的数据"),
            "无所获又无失败时那句不能丢"
        );

        // 正向腿二：有记录就列条数与跳过项，不提"未发现"
        let with_rows = ImportSummary {
            year_dbs: vec![2025],
            records_by_year: vec![(2025, 1200)],
            skipped: vec!["2024 年度库: 无数据".to_string()],
            ..ImportSummary::default()
        };
        let text2 = import_summary_text(&with_rows);
        assert!(
            text2.contains("2025 年度键鼠: 1200 条")
                && text2.contains("跳过: 2024")
                && !text2.contains("未发现可导入的数据"),
            "{text2}"
        );
    }

    /// 导出必须先问年度库可读，再去做聚合。
    ///
    /// 命令本体会弹真文件对话框、也要真库才跑得动，无头环境进不去，所以按文本形状钉
    /// （与 `crates/cli/src/main.rs` 里"七条读命令都要过这把尺子"同一族做法）。
    /// 钉的是**次序**：闸排在 `get_stats` 之后就等于已经拿少一年的总数写完了报告。
    #[test]
    fn export_asks_readability_before_it_aggregates() {
        let src = include_str!("commands.rs");
        let head = src
            .find("pub async fn export_report")
            .expect("导出命令找不到了（判据要跟着改）");
        let body = &src[head..head
            + src[head..]
                .find("\n#[tauri::command]")
                .expect("导出命令之后没有别的命令，切片前提变了")];

        let gate = body
            .find("check_years_readable()")
            .expect("导出没问可读性 —— 读不到的那一年会被静默跳过");
        let agg = body
            .find("get_stats(None, None)")
            .expect("导出不再聚合？判据要跟着改");
        assert!(
            gate < agg,
            "可读性闸必须排在聚合之前（闸在 {gate}、聚合在 {agg}）"
        );
        assert!(
            body.contains("导出被拒绝"),
            "拒绝时要给前端一句能显示的话，不能只 return Err(空)"
        );
        // 夹具自己的前提：切片真盖住了整个命令体，不是切在中间让上面两条空转
        assert!(
            body.contains("export_csv") && body.contains("export_html"),
            "切片没盖住导出的两个格式分支，上面三条是假绿"
        );
    }

    /// 打卡条也要先问**这一窗口**的可读性，再去取按日序列。
    ///
    /// 命令本体要真库才跑得动（跨 370 天扫两个年度库），无头环境进不去 ⇒ 按文本形状钉次序：
    /// 闸排在 `get_daily_counts` 之后就等于已经拿少一年的 0 判定完连续天数。
    /// 还钉"问的是同一个窗口"：拿全库口径去问会拦下这条横条压根不读的旧年份，
    /// 而它是每 60 秒自动刷新的，红成一片比数字偏小更难解释。
    #[test]
    fn goal_status_asks_readability_for_its_own_window_before_it_aggregates() {
        let src = include_str!("commands.rs");
        let head = src
            .find("fn goal_status_json")
            .expect("打卡条的命令体找不到了（判据要跟着改）");
        let body = &src[head..head + src[head..].find("\n}\n").expect("命令体切不出来")];
        let gate = body
            .find("check_daily_counts_readable(")
            .expect("打卡条没问可读性：坏掉那一年会被静默读成每天 0");
        let agg = body
            .find("get_daily_counts(")
            .expect("打卡条不再取按日序列？判据要跟着改");
        assert!(
            gate < agg,
            "可读性闸必须排在取数之前（闸在 {gate}、取数在 {agg}）"
        );
        let from_gate: String = body[gate..].chars().take(220).collect();
        assert!(
            from_gate.contains("GOAL_LOOKBACK_DAYS"),
            "闸问的窗口要和取数那一个一致，否则拦的是不相干的年份: {from_gate}"
        );
        assert!(
            !body.contains("check_years_readable()"),
            "这里该问窗口那一条，不是全部年度库那一条: 旧年份坏了不该让自动刷新的横条整块变红"
        );
        // 切片前提：这段真盖住了打卡条的判定，不是切在半路让上面几条空转
        assert!(
            body.contains("goal_status(") && body.contains("effective_goal("),
            "切片没盖住打卡条的判定，上面几条是假绿"
        );
    }

    /// 暂停的每个入口都要走 `AppState::toggle_pause`（它清活跃锚）。
    ///
    /// 只调 `listener.toggle_pause()` 的话，恢复后的第一条事件会把暂停那段
    /// （≤60 秒）算成活跃时间，并记到暂停前那个应用头上 —— 与"暂停=不统计"相反。
    #[test]
    fn pause_goes_through_the_app_state_so_the_active_anchor_is_reset() {
        // 被禁的调用形态在运行时拼出来，而且带上前面的点：本用例与生产代码的
        // 注释里都有 `listener.toggle_pause()` 这段文字（没有点），直接搜全形会假红。
        let banned = format!("{}{}", ".listener.", "toggle_pause()");
        for (name, src) in [
            ("commands.rs", include_str!("commands.rs")),
            ("tray.rs", include_str!("tray.rs")),
        ] {
            assert!(
                src.contains("state.toggle_pause()"),
                "{name} 没走 AppState::toggle_pause"
            );
            assert!(
                !src.contains(&banned),
                "{name} 还直接调 listener.toggle_pause —— 暂停那段会被算成活跃"
            );
        }
        // `AppState::toggle_pause` 自己也得真的清锚，否则上面两条只是"走对了门"。
        let src = include_str!("state.rs");
        let at = src
            .find("pub fn toggle_pause")
            .expect("AppState::toggle_pause 找不到了");
        let body = &src[at..at + src[at..].find("\n    }").expect("方法体切不出来")];
        assert!(
            body.contains("reset_active_anchor()"),
            "AppState::toggle_pause 暂停时没清活跃锚"
        );
    }

    /// 命令层必须把"插件目录读不出来"原样交给页面。
    ///
    /// 这一层的空列表只有一种画法（「暂无插件」），所以 core 那三条用例守住的结论
    /// 一旦在这里被折回去，就到不了用户眼前。先例：
    /// `export_asks_readability_before_it_aggregates` 钉的是同一条"闸要排在取数之前"。
    #[test]
    fn the_plugin_list_command_hands_an_unreadable_dir_to_the_page() {
        let src = include_str!("commands.rs");
        let head = src
            .find("pub fn get_plugins(")
            .expect("get_plugins 找不到了（判据要跟着改）");
        let body = &src[head..head
            + src[head..]
                .find("\n#[tauri::command]")
                .expect("get_plugins 之后没有别的命令，切片前提变了")];

        assert!(
            body.contains("-> Result<Vec<PluginMeta>, String>"),
            "签名回裸 Vec 就等于把\"读不出来\"与\"一个都没有\"并成一种结局"
        );
        assert!(
            body.contains("list_discovered()?"),
            "core 交出的原因必须原样往上抛，不许在这一层就地折成空列表"
        );
        // 夹具自己的前提：切片真盖住了这个命令的取数与映射，不是切在中间让上面两条空转
        assert!(
            body.contains("with_manager") && body.contains("PluginMeta {"),
            "切片没盖住这个命令的取数与映射，上面两条是空转"
        );
    }

    /// 插件页上两种结局长得不一样：空列表才是「暂无插件」，被拒绝要写原因。
    ///
    /// 这条只能按文本形状钉（无头环境里这一页要真窗口才画得出来），
    /// 「断开插件目录后页面上那行红字真的说得出原因」登记为眼验项。
    #[test]
    fn the_plugin_page_keeps_missing_plugins_and_unreadable_dir_apart() {
        let js = include_str!("../ui/js/plugins.js");
        // 先摘掉注释行：本文件（和这段说明本身）里就会出现「暂无插件」这几个字，
        // 带着注释判"读不出来那一支不许写它"会红在注释上、绿在该改的地方。
        let code = js
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let at = code
            .find("export async function renderPlugins")
            .expect("renderPlugins 找不到了");
        let body = &code[at..at
            + code[at..]
                .find("export async function togglePlugin")
                .expect("下一个函数切不出来，切片前提变了")];
        let catch_at = body
            .find("catch (e) {")
            .expect("renderPlugins 里没有 catch 分支了 —— 拒绝通道断在哪一步？");
        let catch_body = &body[catch_at..];

        assert!(
            catch_body.contains("插件列表读不出来"),
            "被拒绝时要说清是\"没读出来\"，「加载失败」那种含糊话分不清是没装还是读不动"
        );
        assert!(
            catch_body.contains("escapeHtml"),
            "原因里带着目录路径，裸拼进 innerHTML 等于开一条注入通道"
        );
        assert!(
            !catch_body.contains("暂无插件"),
            "读不出来那一支不许又画成「暂无插件」—— 那正是本次要分开的两句"
        );
        let empty_at = body
            .find("暂无插件")
            .expect("空列表那一支的文案不见了，两种结局又并成一种了");
        assert!(empty_at < catch_at, "「暂无插件」必须只在空列表那一支");
    }
}

#[cfg(test)]
mod vibrancy_wiring {
    use super::must_reapply_vibrancy;

    /// 会改变窗口材质外观的只有 `[gui] theme`：改它必须重贴，改别的键不许白贴一遍。
    ///
    /// 反向腿是有意列全的：`[gui]` 里那些键（周期、悬浮窗指标、主题之外的显示项）
    /// 走的都是同一个写入口，判据一旦松成 `section == "gui"`，每次改设置都会重贴一次材质。
    #[test]
    fn only_the_theme_key_reapplies_the_window_material() {
        assert!(must_reapply_vibrancy("gui", "theme"), "切主题必须重贴材质");
        for (section, key) in [
            ("gui", "default_period"),
            ("gui", "floating_metric"),
            ("stats", "cpm_window"),
            ("floating", "theme"),
            ("hotkey", "theme"),
            ("app_stats", "enabled"),
        ] {
            assert!(
                !must_reapply_vibrancy(section, key),
                "[{section}] {key} 不改变材质，不该触发重贴"
            );
        }
    }

    /// 接线腿：`set_config` 必须把这条判据接到 `refresh_window_vibrancy` 上。
    ///
    /// 材质是贴在真窗口上的，headless 起不来窗口 ⇒ 这里只钉接线形状，
    /// 「切主题后那层系统底色真的跟着变」登记为眼验项，不许写成"有用例守着"。
    #[test]
    fn the_theme_write_site_reapplies_the_material() {
        let src = include_str!("commands.rs");
        let at = src.find("pub fn set_config(").expect("set_config 找不到了");
        let body = &src[at..at
            + src[at..]
                .find("\nfn ")
                .expect("set_config 之后没有顶层函数了")];
        assert!(
            body.contains("must_reapply_vibrancy(&section, &key)"),
            "set_config 没走那条判据"
        );
        assert!(
            body.contains("refresh_window_vibrancy(&app)"),
            "判据成立却没真的重贴材质"
        );
    }
}
