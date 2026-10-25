//! 应用共享状态：数据库、监听器、统计快照与后台统计线程。

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{App, AppHandle, Emitter, Manager};

use focusflow_core::config::FocusFlowConfig;
use focusflow_core::db::Database;
use focusflow_core::format::{classify_key, KEY_GROUPS};
use focusflow_core::listener::InputListener;

/// 图表聚合数据（重聚合产出）。SharedStats 与 ChartsStats 共用，
/// serde(flatten) 保证两种事件的外层 JSON 结构不变（前端无感知）。
#[derive(Clone, Default, Serialize)]
pub struct ChartAgg {
    pub total: i64,
    /// 全历史总次数（跨年度库）：今日周期下「总计」卡片用它，避免与「今日活跃」重复。
    /// 由统计线程按 alltime 缓存 + 今日增量修正后写入，`total` 仍是所选周期的总数。
    pub alltime_total: i64,
    /// 所选周期的**展示值**：`total`（纯库值）补上今日尚未落库的增量。
    /// 前端与 `get_live`/`get_charts` 只读这个字段；`total` 留着当统计线程下一轮
    /// 修正的基准，所以两者必须是两个字段（写进同一个字段会双重计数）。
    pub period_total: i64,
    pub avg: i64,
    pub max_day: i64,
    /// 最高单日对应的日期（YYYY-MM-DD）
    pub max_day_date: String,
    pub rank: Vec<(String, i64)>,
    /// 前台应用使用时长排行（秒数降序，随周期联动）
    pub apps: Vec<(String, i64)>,
    /// 周期内前台应用时长总和（秒）：排行占比的分母。
    /// 不能拿 apps 求和代替——apps 已截断到 RANK_LIMIT，长周期下会显著高估占比。
    pub app_total: i64,
    /// 设备维度统计（Raw Input 侧信道，独立口径），随周期联动。
    /// name 含 VID/PID，同型号设备已去重；kind: mouse/keyboard/hybrid。
    pub devices: Vec<focusflow_core::db::DeviceStat>,
    /// 周期内设备输入总次数（占比分母，未截断）
    pub device_total: i64,
    /// 周期内设备台数（未截断）。`devices` 只留前 `RANK_LIMIT` 台，
    /// 界面上的「设备数」必须读这个、不能读 `devices.len()`。
    pub device_count: i64,
    /// 这一轮**没读全**的那些问，每项一句人话（哪一年、哪个库、为什么）。
    ///
    /// 空 = 三个排行榜都读全了。非空时上面那些数只是**下界**：界面必须把它说出来，
    /// 不能把"2026 年库此刻打不开"念成"今日 0 次、暂无数据"——那是同一页上
    /// 「总计」(走三态口子) 与「排行」(过去折成 0) 互相打脸的根源。
    pub read_gaps: Vec<String>,
    pub group: Vec<(String, i64)>,
    /// 鼠标使用总次数（含点击与滚轮）
    pub mouse_total: i64,
    /// 键盘使用总次数
    pub keyboard_total: i64,
    pub trend: Vec<(String, i64)>,
    pub trend30: Vec<(String, i64)>,
    /// 与顶部周期同窗口的每日序列（「总计」那一档是近 30 天，见 `chart_window_days`）。
    /// `trend` / `trend30` 是趋势图那两个固定档，留着是因为它们各自还有别的用处；
    /// 「跟随周期」这一档读的是下面这一格。
    pub trend_period: Vec<(String, i64)>,
    pub hourly: Vec<i64>,
    pub weekday: Vec<(i64, i64)>,
}

/// 后台线程产出的统计数据（前端只读快照，UI 线程零 DB 查询）。
#[derive(Clone, Default, Serialize)]
pub struct SharedStats {
    pub today_count: i64,
    pub cpm: i64,
    /// 今日活跃秒数（事件间隔 ≤ 60s 视为连续活跃）
    pub active_seconds: i64,
    pub period: i64, // -1=今日, 0=总计, N=天数
    #[serde(flatten)]
    pub agg: ChartAgg,
}

/// 轻量实时数据（500ms 变化，事件 `stats-live` 推送）。
#[derive(Clone, Serialize)]
pub struct LiveStats {
    pub today_count: i64,
    pub cpm: i64,
    /// 今日活跃秒数（事件间隔 ≤ 60s 视为连续活跃）
    pub active_seconds: i64,
    pub period: i64, // -1=今日, 0=总计, N=天数
    /// 当前周期最高单日（今日破纪录时随快节奏即时更新）
    pub max_day: i64,
    /// 最高单日对应的日期（YYYY-MM-DD）
    pub max_day_date: String,
    /// 全历史总次数（今日周期下「总计」卡片显示它，随打字即时增长）
    pub alltime_total: i64,
    /// 所选周期的总次数。与 `period` 在同一把锁里快照，两者恒对应同一周期，
    /// 前端切周期时不会出现「新周期标签 + 旧周期数值」的错配。
    pub period_total: i64,
}

/// 重量级图表数据（周期切换 / 定时重聚合，事件 `stats-charts` 推送）。
#[derive(Clone, Serialize)]
pub struct ChartsStats {
    pub period: i64,
    #[serde(flatten)]
    pub agg: ChartAgg,
}

/// 键鼠排行显示上限。
const RANK_LIMIT: usize = 100;

/// 榜单排序：次数降序，**同分时按名字升序**。
///
/// 以前只排次数（`sort_by_key(Reverse)`），而 `get_stats`/`get_app_stats` 返回的是
/// `HashMap` —— `into_iter().collect()` 出来的输入序每进程都不一样（SipHash 随机种子），
/// 稳定排序只是把这份随机原样留在结果里。于是同分键次的先后每次重启都换，
/// 而紧跟其后的是 `truncate(RANK_LIMIT)`：**谁进榜**也变成随机的。
/// 他真实库里 113 个键名，第 99~102 名并列 2 次，正好被 100 这条线切进那组 ——
/// 每次重启有两个键上榜、两个出局，界面末尾那一截每次都在跳（每 2 秒推送一次，
/// 按 index 上色的话还会闪）。
///
/// 宿主侧给插件的同一份榜单早就补了这个破平（`plugins/host.rs` 里
/// `.then_with(|| a.0.cmp(b.0))` 并写了理由），这里是漏掉的那一处。
/// 比较器本身在 `focusflow_core::format::cmp_rank_desc` —— 桌面导出的三张表与 **CLI**
/// 那四个排名用的是同一把尺子，而 CLI 够不着本 crate，所以唯一的一份放在 core。
/// 那里还记着"少了同分破平会怎样"的完整理由（`HashMap` 每进程随机 + 排序之后就是
/// `truncate`/`take` ⇒ 谁进榜是随机的）。
fn sort_rank_desc(items: &mut [(String, i64)]) {
    items.sort_by(|a, b| focusflow_core::format::cmp_rank_desc(&a.0, &a.1, &b.0, &b.1));
}

// ===== 主窗口懒创建 / 隐藏后卸载页面 状态（见 show_main_window / arm_main_unload）=====

/// 与悬浮窗一致的 WebView2 浏览器启动参数。
/// 注意：WebView2 环境由"首个创建的窗口"建立，之后创建的窗口参数会被忽略，
/// 因此懒创建的主窗口必须与悬浮窗保持同一份参数。
///
/// `pub(crate)`：截图覆盖层（`snip.rs`）同样是懒创建的窗口，必须共用这一份 ——
/// 参数各处写一遍，就会出现"有的窗口在后台联网、有的没有"这种查不出来的差别。
pub(crate) const WEBVIEW_BROWSER_ARGS: &str =
    "--disable-background-networking --disable-component-update --no-first-run --disable-domain-reliability --disable-features=MediaRouter";

/// 由「应用活跃与否」统一同步 WebView2 两个开关（渲染、内存档位）的窗口。
///
/// **截图覆盖层不在这里，这不是漏项**：这张表的档位由 `main_visible` 决定，而用户拖着选区
/// 的时候恰恰是 `main_visible == false` —— 对一个正在交互的全屏页面报 LOW 是反效果。
/// 覆盖层的两个开关跟着它自己的显示/隐藏走，见 [`set_window_webview_active`]。
///
/// **贴图 `pin-*` 也不在这里**：贴图按定义就是可见的窗口，对它报 LOW 是错误信号。
pub(crate) const APP_MANAGED_LABELS: [&str; 2] = ["main", "floating"];

/// 主窗口页面是否已被卸载到 about:blank（隐藏超时后卸载，释放页面内存）
static MAIN_UNLOADED: AtomicBool = AtomicBool::new(false);
/// 卸载前的页面 URL（重新显示时导航回去）
static MAIN_URL: Mutex<Option<String>> = Mutex::new(None);
/// 卸载任务代次：安排卸载时 +1 并把新值作为该任务的令牌，显示路径再 +1 作废在途任务。
///
/// 必须是"代次"而不是"是否已安排"的布尔标志：布尔标志会被随后的第二次隐藏重新置真，
/// 让第一次隐藏留下的旧任务复活并二次卸载。此刻页面已是 about:blank，二次卸载会把
/// 恢复 URL 写成 about:blank，主窗口便再也恢复不出来——表现就是"面板点不开，
/// 空等 16 秒后自动重启"。日志里每次"主窗口页面恢复失败"之前都有这种连续两次卸载。
static MAIN_UNLOAD_EPOCH: AtomicU64 = AtomicU64::new(0);
/// 卸载/恢复决策互斥：防止"卸载线程"与"显示路径"竞态
static MAIN_UNLOAD_LOCK: Mutex<()> = Mutex::new(());
/// 主窗口显示/隐藏代次：恢复线程捕获后若期间又发生 hide/show 则放弃恢复，避免误弹出
static MAIN_VIS_EPOCH: AtomicU64 = AtomicU64::new(0);
/// 主窗口懒创建任务是否在途（防重复调度）
static MAIN_CREATING: AtomicBool = AtomicBool::new(false);

/// 应用状态（由 Tauri manage 持有）。
pub struct AppState {
    pub db: Arc<Database>,
    pub listener: Arc<InputListener>,
    pub config: &'static FocusFlowConfig,
    pub shared: Arc<Mutex<SharedStats>>,
    pub period: Arc<AtomicI64>,
    pub refresh_now: Arc<AtomicBool>,
    /// 主窗口是否启动即进托盘
    pub start_to_tray: bool,
    /// 启动自检汇总（B15）：init 路径上各子系统「起没起来」。
    /// 全绿静默；有失败项时前端 toast（`get_startup_report`）、
    /// 托盘 tooltip 带 ⚠ 计数、报告全文在 `get_startup_report`。
    pub startup_report: Vec<focusflow_core::startup::CheckResult>,
}

impl AppState {
    /// 初始化：数据库、监听器、统计线程、托盘、热键、窗口可见性。
    pub fn init(app: &mut App) -> anyhow::Result<()> {
        let config = focusflow_core::config::instance();
        // 暂停位在这里建一次：它是整个采集层的**同一份**真相，同时交给
        // Database::init（转交设备侧信道线程）与 InputListener（rdev 主链路）。
        // 组合根是唯一同时握有两者的地方，所以只能由它持有原始 Arc。
        let paused = focusflow_core::listener::new_pause_flag();
        let db = focusflow_core::db::Database::init(config, Arc::clone(&paused))?;
        // 每日维护：按配置自动 VACUUM（auto_vacuum_days 天一次）
        focusflow_core::db::maintenance::maybe_auto_vacuum(config.get_int(
            "database",
            "auto_vacuum_days",
            7,
        ));
        let listener = InputListener::new(config, Arc::clone(&paused));
        // 启动自检（B15）：DB 侧三项（备份/采样/设备统计）已收在 Database 里，
        // 键鼠监听是致命项（失败直接 ? 让启动失败），成功也记一行备查。
        let mut startup_report = db.take_startup_checks();
        // 文件日志这一层排在最前面：它要是没建起来，本进程**一辈子**都不会再建
        // （subscriber 只装一次），而 `logs/` 是本仓每个故障的默认下一步。
        // 原先这件事只往 stderr 印一句，桌面端是 windows 子系统、没有控制台 ⇒ 完全无声：
        // 用户按提示去翻 logs/ 翻到空目录，谁也不知道少了一层。
        match focusflow_core::logger::file_log_failure() {
            None => startup_report.push(focusflow_core::startup::CheckResult::ok(
                "文件日志",
                "按天轮转，最多保留 4 份",
            )),
            Some(why) => startup_report.push(focusflow_core::startup::CheckResult::fail(
                "文件日志",
                format!(
                    "{why} —— 本次运行不会往 logs/ 落任何东西，这次排查别指望日志；\
                     修好那个目录后重启才有文件日志"
                ),
            )),
        }
        // 数据目录搬运的结论（只有失败才有话，成功只进日志）：跟着启动报告一起 toast，
        // 免得"没搬成、还在旧目录记着"这件事只有翻日志的人才知道。
        if let Some(notice) = focusflow_core::data_location::take_startup_notice() {
            startup_report.push(notice);
        }
        listener.start(Arc::clone(&db))?;
        startup_report.push(focusflow_core::startup::CheckResult::ok(
            "键鼠监听线程",
            "已启动（断线 5 秒自动重启）",
        ));

        // 启动即加载插件（番茄钟/定时任务等随插件 init 运行，对齐 Python 版）。
        // 加载失败的插件从 manager 拿清单 —— 插件页能看到只是一半，
        // 启动报告要让它跟其他子系统一样显性化。
        crate::plugins::with_manager(&db, |pm| {
            let failures = pm.load_failures();
            if failures.is_empty() {
                startup_report.push(focusflow_core::startup::CheckResult::ok(
                    "插件加载",
                    "已加载的全部成功",
                ));
            } else {
                for (name, err) in failures {
                    startup_report.push(focusflow_core::startup::CheckResult::fail(
                        "插件加载",
                        format!("{name}: {err}"),
                    ));
                }
            }
        });
        // 插件热重载（Tauri 无 GUI 轮询循环，用独立扫描线程 + 主线程重载）
        if crate::plugins::start_hot_reload(app.handle(), Arc::clone(&db)) {
            startup_report.push(focusflow_core::startup::CheckResult::ok(
                "插件热重载线程",
                "已启动（监听随插件管理页开关）",
            ));
        } else {
            startup_report.push(focusflow_core::startup::CheckResult::fail(
                "插件热重载线程",
                "扫描/应用线程启动失败，该功能本次运行不可用（重启可恢复）",
            ));
        }
        // 键事件 → 插件分发（番茄钟按键计数依赖此链路）
        if crate::plugins::start_key_event_dispatch(app.handle(), Arc::clone(&db), &listener) {
            startup_report.push(focusflow_core::startup::CheckResult::ok(
                "插件键事件分发线程",
                "已启动",
            ));
        } else {
            startup_report.push(focusflow_core::startup::CheckResult::fail(
                "插件键事件分发线程",
                "分发线程启动失败，插件收不到按键（重启可恢复）",
            ));
        }

        let shared = Arc::new(Mutex::new(SharedStats::default()));
        // 默认周期 = 上次退出前选择的周期（前端切换时写入 gui.default_period）。
        // 该值用户可手改，非法值必须在入口拦掉：它会进入日期运算并 panic（panic=abort）。
        let default_period = config.get_int("gui", "default_period", -1);
        let default_period = if focusflow_core::db::queries::is_valid_period(default_period) {
            default_period
        } else {
            tracing::warn!("gui.default_period 非法（{default_period}），回退为今日");
            -1
        };
        let period = Arc::new(AtomicI64::new(default_period));
        let refresh_now = Arc::new(AtomicBool::new(false));

        if !spawn_stats_worker(
            app.handle().clone(),
            Arc::clone(&db),
            config,
            Arc::clone(&shared),
            Arc::clone(&period),
            Arc::clone(&refresh_now),
        ) {
            startup_report.push(focusflow_core::startup::CheckResult::fail(
                "统计线程",
                "启动失败，界面数字不会更新（重启可恢复）",
            ));
        }

        let start_to_tray = config.get_bool("gui", "start_to_tray", true);

        let state = Arc::new(AppState {
            db,
            listener,
            config,
            shared,
            period,
            refresh_now,
            start_to_tray,
            startup_report,
        });

        // 悬浮窗默认位置（持久化）
        setup_windows(app, &state);

        // 悬浮窗周期重申置顶，防止被任务栏/其他置顶窗口遮挡
        keep_floating_on_top(app);

        app.manage(state);

        crate::tray::setup_tray(app)?;
        crate::hotkey::setup_hotkey(app);

        Ok(())
    }

    /// 暂停/继续记录，返回切换后的状态。
    ///
    /// 暂停那一刻要把写线程的**活跃锚**清零：恢复后的第一条事件用
    /// `timestamp - last_ts` 算活跃时长，锚不清就把整段暂停（≤60 秒）算成活跃，
    /// 还记到暂停前那个应用头上。暂停的语义是"这段时间不统计"。
    /// 所有入口（设置页 / 托盘）都走这里，别各自调 `listener.toggle_pause`。
    pub fn toggle_pause(&self) -> bool {
        let paused = self.listener.toggle_pause();
        if paused {
            if let Some(w) = self.db.writer() {
                w.reset_active_anchor();
            }
        }
        // 挂给悬浮窗：WebView2 那一支靠 `pause-changed` 事件，原生那张卡片读的是这一格
        // （它收不到事件）。两个读者都不会漏，前提是所有暂停入口都走这一个函数 ——
        // 这句话原本就写在本函数的注释里。
        crate::floating::publish_paused(paused);
        paused
    }
}

/// 悬浮窗改为工具窗口（对齐 Python 版 WS_EX_TOOLWINDOW）：
/// 任务管理器把"只有工具窗口可见"的进程归类为后台进程，而非应用。
/// 注意：tauri 的 skipTaskbar 只调用 TaskbarList::DeleteTab 去掉任务栏按钮，
/// 并不会设置 WS_EX_TOOLWINDOW，所以需要手动加扩展样式。
fn make_floating_tool_window(win: &tauri::WebviewWindow) {
    #[cfg(windows)]
    {
        let Ok(hwnd) = win.hwnd() else {
            return;
        };
        unsafe {
            use windows::Win32::Foundation::HWND;
            use windows::Win32::UI::WindowsAndMessaging::{
                GetWindowLongW, SetWindowLongW, SetWindowPos, GWL_EXSTYLE, HWND_TOPMOST,
                SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, WS_EX_APPWINDOW,
                WS_EX_TOOLWINDOW,
            };
            let hwnd = HWND(hwnd.0 as *mut _);
            let style = GetWindowLongW(hwnd, GWL_EXSTYLE);
            // 置 TOOLWINDOW 并清 APPWINDOW：任务管理器据此归类为后台进程
            let want = (style | WS_EX_TOOLWINDOW.0 as i32) & !WS_EX_APPWINDOW.0 as i32;
            if style != want {
                SetWindowLongW(hwnd, GWL_EXSTYLE, want);
            }
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
    }
}

/// 悬浮窗目标尺寸（逻辑像素）。
/// 注意：WebView2 在创建控制器时会把窗口强制拉宽到至少 120px，
/// 导致 tauri.conf.json 里小于 120 的宽度被静默放大、右侧出现大片空白；
/// 这里在窗口+WebView 构建完成后主动 set_size 缩回目标尺寸。
/// 尺寸可在 config.ini 的 [floating] 段用 width/height 覆盖（免重编译）。
fn enforce_floating_size(win: &tauri::WebviewWindow, config: &FocusFlowConfig) {
    let w = config.get_float("floating", "width", 90.0);
    let h = config.get_float("floating", "height", 46.0);
    let _ = win.set_size(tauri::LogicalSize::new(w, h));
    // WebView2 控制器是异步创建的，若放大发生在 setup 之后需要兜底；
    // 启动后 4 秒内每秒重申一次（窗口不可手动缩放，不会与用户冲突）。
    let handle = win.app_handle().clone();
    std::thread::Builder::new()
        .name("floating-size-enforce".into())
        .spawn(move || {
            for _ in 0..4 {
                std::thread::sleep(Duration::from_secs(1));
                if let Some(win_h) = handle.get_webview_window("floating") {
                    let _ = win_h.set_size(tauri::LogicalSize::new(w, h));
                }
            }
        })
        .map_err(|e| tracing::error!("启动悬浮窗尺寸修正线程失败（该功能不可用）: {e}"))
        .ok();
}

/// 设置窗口初始可见性与悬浮窗位置。
fn setup_windows(app: &App, state: &AppState) {
    // 主窗口：`start_to_tray`（默认 true）＝启动进托盘；关掉它的人要的是"启动就看得见"。
    // 主窗口是**懒创建**的（tauri.conf.json 里没有它），启动时本来就不存在 ——
    // 原来只写了"进托盘就 hide()"，于是 false 那一支等于什么都不做，
    // 窗口直到点托盘/按热键才被建出来，这个设置读起来像是没生效。
    if state.start_to_tray {
        if let Some(win) = app.get_webview_window("main") {
            let _ = win.hide();
        }
    } else {
        show_main_window_when_ready(app.handle().clone(), Duration::ZERO);
    }

    // 悬浮窗：由 `floating` 那层按 `[gui] floating_renderer` 决定建哪一套。
    // 以前这一段直接摸 `app.get_webview_window("floating")`，而那个窗口是
    // `tauri.conf.json` 在启动时无条件建出来的 —— 于是即便选了原生渲染器，
    // 整套 WebView2 照样起着，内存一点都省不下来。现在配置里那张表是空的。
    crate::floating::setup(app, state);

    // 启动即同步 WebView2 后台状态（对齐 hide_main_window 的行为）：
    // 1) 渲染管线：主窗口/悬浮窗隐藏时控制器仍认为自己可见、维持渲染合成管线，
    //    必须 SetIsVisible(false) 停掉渲染，内存才能压到最低（用户实测：启动进托盘时
    //    内存偏高，开一次主界面再关闭后才降到最低——根因就是启动时只设了内存档位、
    //    没停主窗口渲染）；
    // 2) 内存档位：主窗口隐藏（非活跃）→ Low，可见 → Normal。
    // 注意：WebView2 控制器是异步创建的，setup 阶段直接调用会静默失败，
    // 因此后台线程重试直到渲染状态与内存档位全部就绪（每次重试前重新判断窗口可见性，
    // 用户可能已打开主界面，此时应恢复渲染并保持 Normal）。
    let handle = app.handle().clone();
    std::thread::Builder::new()
        .name("webview-bg-state".into())
        .spawn(move || {
            // 冷启动 WebView2 环境创建可能较慢，最多重试 60 秒
            for i in 0..60 {
                let mut all_ok = true;
                // 渲染管线随窗口实际可见性同步（控制器未就绪时返回 false，继续重试；
                // 主窗口懒创建：尚未创建的窗口跳过，不阻塞其他窗口的同步）
                for label in APP_MANAGED_LABELS {
                    let Some(win) = handle.get_webview_window(label) else {
                        continue;
                    };
                    let visible = win.is_visible().unwrap_or(false);
                    all_ok = set_webview_rendering(&handle, label, visible) && all_ok;
                }
                let main_visible = handle
                    .get_webview_window("main")
                    .map(|w| w.is_visible().unwrap_or(false))
                    .unwrap_or(false);
                all_ok = set_floating_memory_level(&handle, !main_visible) && all_ok;
                if all_ok {
                    tracing::info!(
                        "WebView2 后台状态已同步 (main_visible={}, 重试 {} 次)",
                        main_visible,
                        i
                    );
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            tracing::warn!("WebView2 后台状态同步失败：控制器长时间未就绪");
        })
        .map_err(|e| tracing::error!("启动 WebView2 后台状态同步线程失败（该功能不可用）: {e}"))
        .ok();
}

/// 显示/隐藏悬浮窗，并把 WebView2 的渲染状态一起跟上。
///
/// 三条入口（设置页的开关、设置页的「显示/隐藏」按钮、托盘菜单）原先各写一份，
/// 而且都不落盘：`[floating] enabled` 全仓只有 `setup_windows` 一个读者，且只在启动时
/// 跑一次 —— 于是勾了开关要到重启才看得见，而用托盘或按钮改了可见性后，下次启动又按
/// 老配置显示出来，两头的意图互相覆盖。这里只负责"照开关做动作"，落盘由调用方做。
///
/// 窗口不存在（构建失败）时以前是静默什么都不发生，这里至少留一条日志。
pub fn set_floating_visible(app: &tauri::AppHandle, visible: bool) {
    // 两种渲染器都走 façade：它同时负责"渲染开关跟着可见性走"（WebView2 那一支）
    // 与 `floating-changed` 广播（设置页那个勾选靠它同步）。
    // 窗口不存在时 façade 自己会出声，这里不再叠一条同源的 warn —— 两条讲的是同一件事，
    // 读日志的人会以为是两个不同的故障。
    crate::floating::set_visible(app, visible);
}

/// WebView2 那一支悬浮窗的一次性设置。整段是从前的 `setup_windows` 里搬过来的，
/// 搬的理由：窗口现在由 `floating` 那层建，这些动作必须紧跟在建窗之后，
/// 而不是"启动时如果它存在"。少做一件都是"看着对、其实坏"：
/// 漏工具窗样式 → 任务管理器把它当应用；漏尺寸修正 → WebView2 把 90px 撑成 120px；
/// 漏位置夹取 → 存过的坐标在拔掉一块屏之后落在屏幕外，卡片看不见也点不到。
pub(crate) fn finish_floating_setup(win: &tauri::WebviewWindow, config: &FocusFlowConfig) {
    make_floating_tool_window(win);
    // 悬浮窗本来就是透明窗口：Acrylic 生效时卡片背后是真实的系统级模糊；
    // 失败则维持原有"半透明卡片"外观，无需前端配合
    let dark = config.get("gui", "theme") == "dark";
    apply_window_vibrancy(win, dark);
    // WebView2 创建控制器时会把窗口强制放宽到至少 120px（内容实际只需 ~81px），
    // 因此在窗口构建完成后主动缩回目标宽度，并延时重复几次兜底异步放大。
    enforce_floating_size(win, config);
    let x = config.get_float("floating", "pos_x", f64::NAN);
    let y = config.get_float("floating", "pos_y", f64::NAN);
    if x.is_nan() || y.is_nan() {
        if let Ok(Some(mon)) = win.current_monitor() {
            let scale = win.scale_factor().unwrap_or(1.0);
            let w = mon.size().width as f64 / scale;
            let _ = win.set_position(tauri::LogicalPosition::new(w - 120.0, 60.0));
        }
    } else {
        // 存过的坐标只在"基本看不见"时才夹回来（判据与为什么不用工作区，
        // 都写在 `clamp_floating_into_screen` 上）。
        let (cx, cy) = match win.current_monitor() {
            Ok(Some(mon)) => {
                let scale = win.scale_factor().unwrap_or(1.0);
                clamp_floating_into_screen(
                    x,
                    y,
                    FloatingBounds {
                        left: mon.position().x as f64 / scale,
                        top: mon.position().y as f64 / scale,
                        screen_w: mon.size().width as f64 / scale,
                        screen_h: mon.size().height as f64 / scale,
                        win_w: config.get_float("floating", "width", 90.0),
                        win_h: config.get_float("floating", "height", 46.0),
                    },
                )
            }
            // 拿不到显示器信息就不动它：瞎夹一次可能把一个好位置弄坏
            _ => (x, y),
        };
        let _ = win.set_position(tauri::LogicalPosition::new(cx, cy));
    }
}

/// 设置各窗口 WebView2 内存档位：
/// 在某个窗口的 WebView2 上执行一段闭包，并**等它真的执行完**，返回它给出的结果。
///
/// 为什么要等：`with_webview` 在主线程上是同步执行，在别的线程上只是把消息投递给
/// 主循环（tauri-runtime-wry 的 `send_user_message`：只有 `current_thread().id() ==
/// main_thread_id` 才 inline 跑，否则走 `proxy.send_event` 立刻返回 Ok）。
/// 原先两处都写成"投递完马上读一个共享 bool"，于是从后台线程调用时**永远**读到 false：
/// `webview-bg-state` 那条同步循环每轮都判失败，白跑满 60 轮（每轮还要再投两次 COM
/// 调用），并且每次启动都在日志里留下一条「控制器长时间未就绪」的假警报 —— 真出故障时
/// 这条日志已经没人信了。副作用本身是落地的，所以除了噪音和 60 秒空转没人受害，
/// 但这条假警报会把后来查内存问题的人带到错的方向上。
///
/// 主线程调用时闭包已经跑完，`recv` 立即返回；投递不出去/主循环卡住时最多等 2 秒，
/// 报 false 让调用方按原样重试。
fn with_webview_sync(
    win: &tauri::WebviewWindow,
    f: impl FnOnce(tauri::webview::PlatformWebview) -> bool + Send + 'static,
) -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    if win
        .with_webview(move |webview| {
            let _ = tx.send(f(webview));
        })
        .is_err()
    {
        return false;
    }
    matches!(rx.recv_timeout(Duration::from_secs(2)), Ok(true))
}

/// 应用活跃（主窗口可见）→ Normal；仅托盘/悬浮窗（非活跃）→ Low。
/// WebView2 官方 MemoryUsageTargetLevel API，非活跃时设 Low 可显著降低内存占用。
/// 主窗口虽隐藏但其页面仍在运行，同样要降档才能把内存压下来。
/// 返回是否全部设置成功（WebView2 控制器未就绪时返回 false，调用方可重试）。
pub fn set_floating_memory_level(app: &tauri::AppHandle, low: bool) -> bool {
    let mut all_ok = true;
    for label in APP_MANAGED_LABELS {
        all_ok = set_webview_memory_level(app, label, low) && all_ok;
    }
    all_ok
}

/// 单个窗口的 WebView2 内存档位。
/// 窗口不存在 ⇒ `true`：懒创建的窗口还没建起来，不算失败。
/// 控制器未就绪 ⇒ `false`，调用方重试；运行时太旧 ⇒ `true`（永久性不可用，重试也不会变好）。
fn set_webview_memory_level(app: &tauri::AppHandle, label: &str, low: bool) -> bool {
    let Some(win) = app.get_webview_window(label) else {
        return true;
    };
    with_webview_sync(&win, move |webview| {
        #[cfg(windows)]
        unsafe {
            use webview2_com::Microsoft::Web::WebView2::Win32::{
                ICoreWebView2_19, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW,
                COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL,
            };
            use windows::core::Interface;
            let controller = webview.controller();
            if let Ok(core) = controller.CoreWebView2() {
                if let Ok(v19) = core.cast::<ICoreWebView2_19>() {
                    let level = if low {
                        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW
                    } else {
                        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL
                    };
                    return v19.SetMemoryUsageTargetLevel(level).is_ok();
                }
                tracing::warn!("WebView2 运行时过旧，不支持内存档位 API（需 1.0.2390+）");
                // 运行时太旧是**永久性**的不可用，重试到 60 秒之外也不会变好
                return true;
            }
            // CoreWebView2 未就绪：报 false，让调用方重试
            false
        }
        #[cfg(not(windows))]
        {
            let _ = low;
            true
        }
    })
}

/// 一个窗口自己隐藏/显示时，把 WebView2 的两个开关一起跟上：
/// 隐藏 ⇒ 停渲染合成 + 降内存档位；显示 ⇒ 恢复渲染 + Normal，且必须排在 `show()` **之前**
/// （见 [`set_webview_rendering`] 的注释：不先恢复就不重绘）。
///
/// 为什么绑成一个动作而不是两处各写一遍：2026-10-06 那次实测里，主窗口隐藏那一刻省下的
/// **−62.9 MB 工作集**是这两个开关共同买到的；单独一项没测过，而"页面卸载到 about:blank"
/// 那条机制实测 ≈0（−16.8 MB 工作集、私有提交在噪声内反升）。少做一件就是白做一半。
pub fn set_window_webview_active(app: &tauri::AppHandle, label: &str, active: bool) -> bool {
    let rendering = set_webview_rendering(app, label, active);
    let level = set_webview_memory_level(app, label, !active);
    rendering && level
}

/// 设置 WebView2 控制器可见性（IsVisible）。
/// 窗口隐藏时 WebView2 控制器并不知道自身不可见，仍会维持渲染合成管线；
/// 调用 put_IsVisible(false) 可停止渲染、进一步释放内存与 CPU（WebView2 官方建议）。
/// 显示窗口前必须先恢复 true，否则内容不会重绘。
/// 返回是否真正设置成功（控制器未就绪时返回 false，调用方可重试）。
pub fn set_webview_rendering(app: &tauri::AppHandle, label: &str, visible: bool) -> bool {
    let Some(win) = app.get_webview_window(label) else {
        return false;
    };
    with_webview_sync(&win, move |webview| {
        #[cfg(windows)]
        unsafe {
            let controller = webview.controller();
            controller.SetIsVisible(visible).is_ok()
        }
        #[cfg(not(windows))]
        {
            let _ = visible;
            true
        }
    })
}

/// 显示主窗口（不存在则懒创建）。
///
/// 懒创建：启动时不建主窗口（tauri.conf.json 已移除），首次显示才创建，
/// 启动阶段省掉一个常驻的隐藏渲染进程（50~100MB）。窗口创建后常驻，
/// 之后隐藏/显示复用，不再销毁。
///
/// 重要：窗口不存在时的创建动作必须推迟到"普通事件循环轮次"执行——
/// WebviewWindowBuilder::build 会被 Tauri 调度回主线程创建 WebView2 控制器；
/// 若主线程此刻正卡在 WebView2 消息派发栈内（如悬浮窗 JS 触发 show_main 的
/// invoke 处理中），同步建窗会挂死（复现：启动后直接双击悬浮窗呼出主界面 →
/// 点不开，再点托盘整个程序卡死）。后台线程 + run_on_main_thread 让创建
/// 发生在 WebView2 消息派发栈退栈之后的事件循环迭代里。
/// 主窗口是否已启用系统级窗口材质（前端据此切换半透明玻璃令牌）。
pub static MAIN_GLASS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 应用系统级窗口材质（液态玻璃）：
/// - Win11 优先 Mica（壁纸取色，功耗低）；
/// - 回退 Acrylic（Win10/11，带主题底色的实时模糊）；
/// - 都失败返回 false：窗口保持 CSS 不透明外观，与未开启一致。
///
/// 前端收到 `vibrancy-on` 事件（或启动时查询 get_vibrancy）后切换
/// body.glass 半透明令牌，让材质透出。
#[cfg(windows)]
pub(crate) fn apply_window_vibrancy(win: &tauri::WebviewWindow, dark: bool) -> bool {
    use window_vibrancy::{apply_acrylic, apply_mica};
    if apply_mica(win, Some(dark)).is_ok() {
        tracing::info!("窗口材质已启用: Mica ({})", win.label());
        return true;
    }
    let tint = if dark {
        (20, 22, 31, 96)
    } else {
        (245, 247, 250, 96)
    };
    if apply_acrylic(win, Some(tint)).is_ok() {
        tracing::info!("窗口材质已启用: Acrylic ({})", win.label());
        return true;
    }
    tracing::info!(
        "窗口材质不可用（系统版本/透明效果设置限制）: {}",
        win.label()
    );
    false
}

#[cfg(not(windows))]
pub(crate) fn apply_window_vibrancy(_win: &tauri::WebviewWindow, _dark: bool) -> bool {
    false
}

/// 主题变了就把系统材质重贴一次。
///
/// `apply_window_vibrancy` 那个 `dark` 不是摆设：Mica 分支拿它设
/// `DWMWA_USE_IMMERSIVE_DARK_MODE`（系统按这个档位挑 Mica 的深浅底），Acrylic 分支直接
/// 用它挑着色 tint。而这两处原先只在**建窗那一刻**各贴一次 ⇒ 设置页从亮切暗后 CSS 跟着变了，
/// 窗口那层系统底色却还停在亮色，整圈边缘与标题区留着旧主题的颜色（悬浮窗同一件事）。
/// 材质贴在窗口上，所以两个窗都得走一遍；主窗口还没建起来时这里是空操作，
/// 建窗那条本来就读的是当时的主题，不会贴错。
pub fn refresh_window_vibrancy(app: &tauri::AppHandle) {
    let dark = focusflow_core::config::instance().get("gui", "theme") == "dark";
    if let Some(win) = app.get_webview_window("main") {
        let ok = apply_window_vibrancy(&win, dark);
        MAIN_GLASS.store(ok, Ordering::SeqCst);
        use tauri::Emitter;
        // 前端那层 body.glass 是"材质真的在"才加的半透明令牌：这次贴不成就得摘掉，
        // 否则窗口已经不是玻璃、CSS 还留着透出桌面的半透明底。
        // 两条分支各写一次 emit_to、事件名紧跟在第二个实参位上：`wiring_audit` 那条
        // 是按 `emit_to("窗口", "事件名"` 的形状认账的，把名字算成表达式（这里是 if/else）
        // 它就读成"前端在等一个从来没人广播的事件"——与本仓那处 emit 跨三行的漏读同一个形状。
        if ok {
            let _ = app.emit_to("main", "vibrancy-on", true);
        } else {
            let _ = app.emit_to("main", "vibrancy-off", false);
        }
    }
    // 悬浮窗那一支交给 façade：WebView2 那张卡片要重贴系统材质，
    // 原生那张卡片没有材质可贴（分层窗口拿不到 Mica/Acrylic），它下一拍自己读 `[gui] theme`。
    crate::floating::apply_theme(app, dark);
}

pub fn show_main_window(app: &tauri::AppHandle) {
    if app.get_webview_window("main").is_none() {
        if MAIN_CREATING.swap(true, Ordering::SeqCst) {
            return; // 已有创建任务在途，避免重复
        }
        let handle = app.clone();
        std::thread::Builder::new()
            .name("main-create".into())
            .spawn(move || {
                std::thread::sleep(Duration::from_millis(50)); // 等当前 IPC 回调退栈
                let h = handle.clone();
                if handle
                    .run_on_main_thread(move || {
                        MAIN_CREATING.store(false, Ordering::SeqCst);
                        show_main_window_impl(&h);
                    })
                    .is_err()
                {
                    // 调度失败（如应用退出中）：复位标志，避免后续创建被永久屏蔽
                    MAIN_CREATING.store(false, Ordering::SeqCst);
                }
            })
            .map_err(|e| {
                // 线程压根就没起来 ⇒ 线程体里那两处复位永远轮不到执行。不在这里补上，
                // MAIN_CREATING 永久为真，之后每次点面板都在最上面直接 return，
                // 主窗口再也打不开——而这次失败本身只留下一行日志。
                MAIN_CREATING.store(false, Ordering::SeqCst);
                tracing::error!("启动主窗口创建线程失败（该功能不可用）: {e}")
            })
            .ok();
        return;
    }
    show_main_window_impl(app);
}

/// show_main_window 实现体（窗口已存在，或已确保在普通事件循环轮次执行）。
fn show_main_window_impl(app: &tauri::AppHandle) {
    // 作废待执行的"隐藏后卸载页面"任务（线程唤醒后会在锁内二次确认令牌）
    MAIN_UNLOAD_EPOCH.fetch_add(1, Ordering::SeqCst);
    // 显示/隐藏代次 +1：让仍在等待恢复的旧线程放弃（见 restore_main_after_load）
    MAIN_VIS_EPOCH.fetch_add(1, Ordering::SeqCst);

    if app.get_webview_window("main").is_none() {
        tracing::info!("show_main: 主窗口首次创建（懒创建）");
        // 全新窗口：清理可能残留的卸载状态（正常流程窗口常驻，此处仅为兜底）
        MAIN_UNLOADED.store(false, Ordering::SeqCst);
        *MAIN_URL.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let result = tauri::WebviewWindowBuilder::new(
            app,
            "main",
            tauri::WebviewUrl::App("index.html".into()),
        )
        .title("FocusFlow - 效率追踪器")
        .inner_size(1100.0, 760.0)
        .min_inner_size(820.0, 560.0)
        .resizable(true)
        .visible(false)
        // 透明窗口：系统材质（Mica/Acrylic）生效时玻璃透出；
        // 材质不可用时 CSS 保持不透明令牌，外观与不透明窗口一致
        .transparent(true)
        .additional_browser_args(WEBVIEW_BROWSER_ARGS)
        .build();
        match &result {
            Ok(win) => {
                tracing::info!("show_main: 主窗口创建成功");
                let dark = focusflow_core::config::instance().get("gui", "theme") == "dark";
                if apply_window_vibrancy(win, dark) {
                    MAIN_GLASS.store(true, Ordering::SeqCst);
                    use tauri::Emitter;
                    let _ = app.emit_to("main", "vibrancy-on", true);
                }
            }
            Err(e) => tracing::error!("show_main: 主窗口创建失败: {e}"),
        }
    }

    // 主窗口打开：立即触发一次重聚合，图表数据马上刷新（隐藏期间重聚合已停用）
    if let Some(state) = app.try_state::<Arc<AppState>>() {
        state.refresh_now.store(true, Ordering::Relaxed);
    }
    // 应用进入活跃状态：悬浮窗回到 Normal 内存档位
    set_floating_memory_level(app, false);

    // 卸载/恢复决策与"隐藏后卸载"线程互斥，防止竞态：
    // - 卸载线程拿锁后再次确认窗口仍隐藏才会导航 about:blank；
    // - 这里拿锁后发现页面已被卸载，先导航回原页面、加载完成后再显示。
    let _guard = MAIN_UNLOAD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if MAIN_UNLOADED.swap(false, Ordering::SeqCst) {
        if let Some(win) = app.get_webview_window("main") {
            match MAIN_URL
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .and_then(|u| u.parse::<tauri::Url>().ok())
            {
                Some(u) => match win.navigate(u) {
                    Ok(()) => {
                        // 加载完成前不显示，避免白屏；由恢复线程轮询后恢复渲染并显示
                        restore_main_after_load(app);
                        return;
                    }
                    // 导航请求没被接受 ⇒ 页面还停在 about:blank。上面那句 swap 已经把
                    // 「已卸载」摘成 false，不立回去的话下一次呼出走的就是普通路径，
                    // 而普通路径只会看到 about:blank 并照原样显示 ⇒
                    // 用户拿到一个永远空白的面板，再按热键也不会好。
                    // 与 `weekly_report_marking` / `restart_slot_after_spawn` 同一条规矩：
                    // 事情没做成，不许推进"只做一次"的凭据。这里不 return，
                    // 接着往下走的普通路径会因为 url 仍是空白而交给恢复线程那套
                    // 「重试两次 → 还不行就重启程序」的兜底。
                    Err(e) => {
                        MAIN_UNLOADED.store(true, Ordering::SeqCst);
                        tracing::warn!(
                            "主窗口恢复导航没被接受，页面还是空白: {e} —— 把「已卸载」标记立回去，\
                             下次呼出再试一次（可恢复的 URL 仍然留着）"
                        );
                    }
                },
                None => tracing::warn!("show_main: 恢复 URL 无效，按普通路径显示"),
            }
        }
    }
    // 正常路径：恢复 WebView2 渲染（隐藏时已停用），再显示窗口。
    // 页面仍为空白（about:blank，如恢复线程尚未完成）→ 等加载完成再显示。
    let blank = app
        .get_webview_window("main")
        .map(|w| {
            w.url()
                .map(|u| u.as_str() == "about:blank")
                .unwrap_or(false)
        })
        .unwrap_or(false);
    if blank {
        restore_main_after_load(app);
        return;
    }
    set_webview_rendering(app, "main", true);
    if let Some(win) = app.get_webview_window("main") {
        // 恢复任务栏按钮（隐藏时切走过）
        let _ = win.set_skip_taskbar(false);
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

/// 隐藏主窗口（到托盘）：仅隐藏，不销毁。
///
/// 说明：WebView2 在同一进程内"销毁后重建控制器"不可靠（0x8007139F，
/// 与是否先 Close() 无关），所以放弃销毁方案，窗口常驻内存、显示即恢复。
/// 任务管理器重新分类用任务栏注册表项切换触发（见 hide_main_window）。
pub fn hide_main_window(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.hide();
        // 停止 WebView2 渲染合成，释放渲染管线内存（窗口仍常驻，显示时再恢复）
        set_webview_rendering(app, "main", false);
        // 促使任务管理器重新评估"应用/后台进程"：隐藏窗口不会触发窗口销毁
        // 通知，TM 不会重新分类；AddTab/DeleteTab 产生 shell 事件，
        // 让 TM 重新枚举（窗口已隐藏，切换无视觉影响）。
        let _ = win.set_skip_taskbar(false);
        let _ = win.set_skip_taskbar(true);
    }
    // 应用转入非活跃状态：悬浮窗降为 Low 内存档位
    set_floating_memory_level(app, true);
    // 显示/隐藏代次 +1：让等待恢复的旧线程放弃（见 restore_main_after_load）
    MAIN_VIS_EPOCH.fetch_add(1, Ordering::SeqCst);
    // 隐藏超时后卸载主窗口页面（about:blank），释放页面 JS 堆/DOM 内存
    arm_main_unload(app);
}

/// 主窗口隐藏后延时卸载页面（navigate about:blank），释放页面内存。
/// 防抖：默认隐藏 60 秒后仍隐藏才卸载（config [gui] unload_hidden_delay，
/// 最小 5 秒）；显示路径会推进 MAIN_UNLOAD_EPOCH 作废在途任务。
/// 可通过 [gui] unload_hidden=false 关闭。
fn arm_main_unload(app: &tauri::AppHandle) {
    let config = focusflow_core::config::instance();
    if !config.get_bool("gui", "unload_hidden", true) {
        return;
    }
    // 本次任务的令牌：任何后续操作（显示/再次隐藏）都会让它失效
    let token = MAIN_UNLOAD_EPOCH.fetch_add(1, Ordering::SeqCst) + 1;
    let delay = config.get_int("gui", "unload_hidden_delay", 60).max(5) as u64;
    let handle = app.clone();
    std::thread::Builder::new()
        .name("main-unload".into())
        .spawn(move || {
            std::thread::sleep(Duration::from_secs(delay));
            // 令牌已过期：期间发生过显示，或又安排了一次新的卸载（后者会自己负责卸载）
            if MAIN_UNLOAD_EPOCH.load(Ordering::SeqCst) != token {
                return;
            }
            let Some(win) = handle.get_webview_window("main") else {
                return;
            };
            // 两个 getter 必须在拿锁**之前**读。`is_visible()`/`url()` 在
            // tauri-runtime-wry 里是"把请求投进主循环、然后无超时等回话"；而主线程的
            // show_main 正等着同一把 MAIN_UNLOAD_LOCK —— 锁里等主循环 = 两边互等死锁，
            // 表现是主窗口再也打不开，统计线程随后也卡在自己的 is_visible() 上。
            // 拿锁后只用原子量复核：显示路径会推进 MAIN_UNLOAD_EPOCH，让本轮作废。
            let visible = win.is_visible().unwrap_or(true);
            let page_url = win.url().ok();
            if visible {
                return;
            }
            let _guard = MAIN_UNLOAD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            // 锁内二次确认：令牌未过期（期间发生过显示/重新安排卸载）
            if MAIN_UNLOAD_EPOCH.load(Ordering::SeqCst) != token {
                return;
            }
            // 页面已经卸载过：绝不能再次记录恢复 URL。此刻页面就是 about:blank，
            // 覆盖进去会让主窗口永远恢复不出来（见 MAIN_UNLOAD_EPOCH 注释）。
            if MAIN_UNLOADED.load(Ordering::SeqCst) {
                return;
            }
            let Some(url) = page_url else {
                return;
            };
            // 兜底：窗口本来就停在空白页（没有可恢复的页面），不做无意义的"卸载"
            if url.as_str() == "about:blank" {
                return;
            }
            // 记录原页面 URL（重新显示时导航回去），再卸载页面。
            //
            // 两件事都必须排在 `navigate` 的回话之后：`navigate` 是"投递给主循环"，
            // 它的 Err 说的是这次请求压根没被接受（窗口已经没了/通道断了）。
            // 原先写的是 `let _ = win.navigate(..)` 然后无条件立 MAIN_UNLOADED、无条件打
            // 「页面已卸载到 about:blank（释放页面内存）」⇒ 投递失败的那次，
            // 日志说内存已经释放而其实没有，而下一次 show 又照着 MAIN_UNLOADED 去
            // "导航回原页面" —— 把一个活得好好的页面白重刷一遍，设置页里正在输入的东西
            // 就是这么被清掉的（而这是卸载路径，本来不该碰用户的页面）。
            match win.navigate(tauri::Url::parse("about:blank").unwrap()) {
                Ok(()) => {
                    *MAIN_URL.lock().unwrap_or_else(|e| e.into_inner()) = Some(url.to_string());
                    MAIN_UNLOADED.store(true, Ordering::SeqCst);
                    tracing::info!("主窗口页面已提交卸载到 about:blank（释放页面内存）");
                }
                Err(e) => {
                    *MAIN_URL.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    tracing::warn!("主窗口页面卸载请求没被接受，页面继续留着（内存未释放）: {e}");
                }
            }
        })
        .map_err(|e| tracing::error!("启动主窗口页面卸载线程失败（该功能不可用）: {e}"))
        .ok();
}

/// 页面卸载后重新打开主窗口：轮询等待页面加载完成（最多 2 秒），
/// 再恢复 WebView2 渲染并显示，避免白屏闪烁。
/// 若等待期间又发生 hide/show（代次变化），放弃本次恢复，交给最新操作。
fn restore_main_after_load(app: &tauri::AppHandle) {
    let epoch = MAIN_VIS_EPOCH.load(Ordering::SeqCst);
    let handle = app.clone();
    std::thread::Builder::new()
        .name("main-restore".into())
        .spawn(move || {
            let mut loaded = false;
            // 最多等 10 秒（冷启动 WebView2 初始化可能很慢）；期间代次变化立即放弃
            for _ in 0..200 {
                if MAIN_VIS_EPOCH.load(Ordering::SeqCst) != epoch {
                    return;
                }
                loaded = handle
                    .get_webview_window("main")
                    .map(|w| {
                        w.url()
                            .map(|u| u.as_str() != "about:blank")
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if loaded {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            // 超时仍未加载：补导航兜底（再试两次，间隔等待）。若全部失败，
            // 判定 WebView2 环境已损坏（典型场景：长时间休眠中浏览器进程死亡，
            // 唤醒后控制器状态失效，此后所有 navigate 都无效），窗口只能显示
            // 空白（about:blank + 透明窗口 → 用户看到纯材质底色）。此时自动
            // 重启应用：干净退出走 flush/备份（恢复文件兜底未落库增量），
            // 比留给用户一个永远空白的窗口好。
            if !loaded {
                for _ in 0..2 {
                    if MAIN_VIS_EPOCH.load(Ordering::SeqCst) != epoch {
                        return;
                    }
                    if let Some(win) = handle.get_webview_window("main") {
                        let url = MAIN_URL.lock().unwrap_or_else(|e| e.into_inner()).clone();
                        // 记不到可恢复的页面 URL：重试导航无从下手，直接进入重启兜底
                        match url.as_deref() {
                            Some(u) if u != "about:blank" => {
                                if let Ok(u) = u.parse::<tauri::Url>() {
                                    let _ = win.navigate(u);
                                }
                            }
                            other => {
                                tracing::error!(
                                    "主窗口恢复 URL 无效（{other:?}），跳过导航重试"
                                );
                                break;
                            }
                        }
                    }
                    for _ in 0..60 {
                        if MAIN_VIS_EPOCH.load(Ordering::SeqCst) != epoch {
                            return;
                        }
                        loaded = handle
                            .get_webview_window("main")
                            .map(|w| {
                                w.url()
                                    .map(|u| u.as_str() != "about:blank")
                                    .unwrap_or(false)
                            })
                            .unwrap_or(false);
                        if loaded {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    if loaded {
                        break;
                    }
                }
            }
            if !loaded {
                let url = MAIN_URL
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                    .unwrap_or_else(|| "<none>".into());
                tracing::error!(
                    "主窗口页面恢复失败（恢复目标 {url}，疑似 WebView2 环境损坏），3 秒后自动重启应用"
                );
                schedule_app_restart(&handle, false);
                return;
            }
            // 显示前最终确认：代次未变、窗口仍隐藏（把 TOCTOU 窗口缩到最小）
            if MAIN_VIS_EPOCH.load(Ordering::SeqCst) != epoch {
                return;
            }
            let Some(win) = handle.get_webview_window("main") else {
                return;
            };
            if win.is_visible().unwrap_or(true) {
                return;
            }
            set_webview_rendering(&handle, "main", true);
            let _ = win.set_skip_taskbar(false);
            let _ = win.show();
            let _ = win.unminimize();
            let _ = win.set_focus();
            // 显示后复检代次：若期间用户又隐藏了窗口，立即收回去
            if MAIN_VIS_EPOCH.load(Ordering::SeqCst) != epoch {
                let _ = win.hide();
            }
        })
        .map_err(|e| tracing::error!("启动主窗口恢复线程失败（该功能不可用）: {e}")).ok();
}

/// 自动重启是否已安排（一个进程只允许安排一次：多个恢复线程同时超时会各拉起一个
/// 新进程，几个新进程抢同一把单实例锁、互相把对方挤掉）。
static RESTART_SCHEDULED: AtomicBool = AtomicBool::new(false);

/// 自动重启链最大深度：A 起 B、B 又起 C…… 每层都要重新等 3 秒 + 冷启动，
/// 若环境始终恢复不了就会无限重启（应用反复自尽）。到顶后放弃重启，
/// 把窗口直接显示出来（空白也比"进程还在、面板永远打不开"好排查）。
const MAX_RESTART_DEPTH: u32 = 2;

/// 当前进程的重启链深度：由命令行 `--restart-depth <n>` 传入（见 desktop/src/lib.rs）。
fn restart_depth_from_args() -> u32 {
    let mut args = std::env::args();
    while let Some(a) = args.next() {
        if a == "--restart-depth" {
            return args.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        }
    }
    0
}

/// [`restart_decision`] 的结论。
#[derive(Debug)]
enum RestartDecision {
    /// 该安排这一跳，`next_depth` 是新进程要带的重启链深度（名额已占住）
    Proceed { next_depth: u32 },
    /// 名额被**真在途**的那次占着，这次不重复安排
    AlreadyScheduled,
    /// 自动重启链到顶：放弃，并且**不占**名额
    DepthExhausted { depth: u32 },
}

/// 这一跳到底要不要安排 —— 判据顺序本身就是修复：**先过深度闸，再占名额**。
///
/// 反过来写（先 `swap` 占名额、后判深度）时，一次"深度到顶、压根没打算拉起新进程"
/// 的自动重试会把本进程唯一的重启名额吃掉；之后用户主动换目录那一跳就被同一个
/// 名额以"本进程已经安排过一次重启"为由拒绝：配置已写、进程不重启，新目录要等
/// 下次自然启动才生效，而给出的理由指向的是一次从未真正尝试过的安排。
///
/// `scheduled` 由参数注入（而不是直接引用静态量），是为了让"到顶那次没占名额"
/// 这件事在无头、无 AppHandle 的条件下可断言。
fn restart_decision(scheduled: &AtomicBool, user_initiated: bool, depth: u32) -> RestartDecision {
    if !user_initiated && depth >= MAX_RESTART_DEPTH {
        return RestartDecision::DepthExhausted { depth };
    }
    let next_depth = if user_initiated { 0 } else { depth + 1 };
    if scheduled.swap(true, Ordering::SeqCst) {
        return RestartDecision::AlreadyScheduled;
    }
    RestartDecision::Proceed { next_depth }
}

/// 重启线程没起来 ⇒ 把一次性名额还回去。
///
/// 名额的含义是「已经有一个线程会去拉起新进程」，而线程压根没起来就没有任何东西在途。
/// 不还的话，本进程后续任何一次重启（包括用户主动换数据目录那一跳）都会被
/// 「本进程已经安排过一次重启」挡掉，而那句指向的是一次从未开始的尝试 —— 与
/// [`restart_decision`] 里「被深度闸拒绝的那一次不占名额」是同一条规矩的另一半。
fn restart_slot_after_spawn(scheduled: &AtomicBool, spawned: bool) -> bool {
    if !spawned {
        scheduled.store(false, Ordering::SeqCst);
    }
    spawned
}

/// 自动重启应用（WebView2 环境损坏等无法在线恢复的场景）：
/// 后台线程延迟几秒后拉起新进程（当前 exe，数据目录解析与本次一致），
/// 当前进程干净退出——退出路径完成 flush/备份，未落库增量由恢复文件兜底。
///
/// 新进程带 `--wait-pid <本进程 PID>`：等本进程真正退出后再初始化，
/// 否则会撞上单实例守卫（新进程被当成第二个实例直接退出，结果是两个进程都没了）
/// 与 WebView2 用户数据目录；带 `--show-main`：用户本来就是在等面板打开，
/// 重启后直接把面板显示出来。
///
/// `user_initiated = false`（既有的恢复路径）：受 [`MAX_RESTART_DEPTH`] 约束，
/// 环境始终恢复不了时不至于无限自尽。
/// `user_initiated = true`（用户主动换数据目录）：不该被深度上限拒绝——那会让
/// 配置已写、进程却不重启，新目录要等到下次自然启动才生效，而中间这段按键全落在
/// 旧目录里；它同时把重启链深度归零，因为这一跳是"新起点"而不是又一轮恢复重试，
/// 不该把新进程里的恢复重启额度提前吃掉。两条路径共用 `RESTART_SCHEDULED`：
/// 一个进程只准拉起一个新进程，这个不变量与触发原因无关；但**被深度闸拒绝的那一次
/// 不占名额**——它压根不会拉起任何进程，占名额等于把这一跳的额度白送给一次
/// 不存在的尝试（判据顺序见 [`restart_decision`]）。
///
/// 返回值只说明"这一跳有没有安排上"（占住重启序号、重启线程起得来）。线程里
/// 3 秒后真的拉不起新进程是调用方拿不到的 —— 那种情况下程序继续按旧目录记录，
/// 而配置里的迁移标记留着，下次自然启动照样会把数据搬过去。
pub(crate) fn schedule_app_restart(app: &tauri::AppHandle, user_initiated: bool) -> bool {
    let depth = restart_depth_from_args();
    let next_depth = match restart_decision(&RESTART_SCHEDULED, user_initiated, depth) {
        RestartDecision::AlreadyScheduled => {
            tracing::warn!("本进程已经安排过一次重启，这次不再重复安排");
            return false;
        }
        RestartDecision::DepthExhausted { depth } => {
            tracing::error!("自动重启已达上限（深度 {depth}），放弃重启：直接显示空白窗口");
            reveal_main_window(app);
            return false;
        }
        RestartDecision::Proceed { next_depth } => next_depth,
    };
    let handle = app.clone();
    let spawned = std::thread::Builder::new()
        .name("app-restart".into())
        .spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            let exe = match std::env::current_exe() {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!("自动重启失败：无法定位当前 exe: {e}");
                    reveal_main_window(&handle);
                    return;
                }
            };
            match std::process::Command::new(&exe)
                .arg("--wait-pid")
                .arg(std::process::id().to_string())
                .arg("--restart-depth")
                .arg(next_depth.to_string())
                .arg("--show-main")
                .spawn()
            {
                Ok(_) => {
                    tracing::info!("应用自动重启：新进程已启动（深度 {next_depth}），当前进程退出");
                    handle.exit(0);
                }
                Err(e) => {
                    tracing::error!("自动重启失败（拉起新进程）: {e}，应用保持运行");
                    reveal_main_window(&handle);
                }
            }
        })
        .map(|_| true)
        .map_err(|e| {
            tracing::error!("启动应用重启线程失败（该功能不可用，重启名额已退回）: {e}");
            false
        })
        .unwrap_or(false);
    restart_slot_after_spawn(&RESTART_SCHEDULED, spawned)
}

/// 兜底显示主窗口：恢复/重启都失败时至少让窗口可见，
/// 用户可以据此确认程序还在（托盘菜单仍可退出/重开）。
fn reveal_main_window(app: &tauri::AppHandle) {
    set_webview_rendering(app, "main", true);
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.set_skip_taskbar(false);
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

/// 等应用状态就绪后显示主窗口（推迟到下一轮事件循环执行，避免在 IPC/建窗栈内再建窗）。
///
/// 两个入口共用：用户重复启动 exe（单实例回调转发过来）、自动重启带 `--show-main` 启动。
/// `delay`：启动后先等一会儿再显示（自动重启场景让 WebView2 环境先就绪）。
pub fn show_main_window_when_ready(app: AppHandle, delay: Duration) {
    std::thread::Builder::new()
        .name("show-main-later".into())
        .spawn(move || {
            std::thread::sleep(delay);
            // 启动早期 AppState 可能尚未 manage：最多再等 10 秒
            for _ in 0..50 {
                if app.try_state::<Arc<AppState>>().is_some() {
                    let h = app.clone();
                    let _ = app.run_on_main_thread(move || show_main_window(&h));
                    return;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            tracing::warn!("显示主窗口：应用状态长时间未就绪，放弃");
        })
        .map_err(|e| tracing::error!("启动显示主窗口线程失败（该功能不可用）: {e}"))
        .ok();
}

/// 悬浮窗周期重申置顶（对齐 Python 版方案）：
/// 任务栏本身是置顶窗口，鼠标指向时会把悬浮窗压到下面；
/// 每 500ms 用 SetWindowPos(HWND_TOPMOST) 把它重新抬到任务栏之上。
fn keep_floating_on_top(app: &App) {
    // 只有 WebView2 那一支需要这个轮询：原生窗口在 `WM_WINDOWPOSCHANGING` 里当场答
    // topmost（`floating/native.rs`），不用每秒两次 `SetWindowPos`。
    if !crate::floating::needs_topmost_poll() {
        tracing::info!("悬浮窗走原生渲染，不启动 500ms 置顶轮询");
        return;
    }
    let Some(win) = app.get_webview_window("floating") else {
        return;
    };
    let Ok(hwnd) = win.hwnd() else {
        return;
    };
    // HWND 含裸指针不可跨线程，先转 isize 再在线程内还原
    let raw_hwnd = hwnd.0 as isize;
    std::thread::Builder::new()
        .name("floating-topmost".into())
        .spawn(move || {
            // 间隔每轮现算（可见 500ms、隐藏 3s），所以这里**不**给初值：
            // 给了就是死值 —— 第一轮任何读取之前它都会先被赋值一次
            // （`clippy::unused_assignments` 会把它打成 -D warnings 失败）。
            // 非 Windows 分支问不到可见性，固定 3s；那条路径上本函数早在
            // `win.hwnd()` 就 return 了，这里只是为了让这个文件在非 Windows
            // 下也过得去。
            let mut sleep_ms: u64;
            loop {
                #[cfg(not(windows))]
                {
                    sleep_ms = 3000;
                }
                #[cfg(windows)]
                unsafe {
                    use windows::Win32::Foundation::HWND;
                    use windows::Win32::UI::WindowsAndMessaging::{
                        GetWindowLongW, IsWindow, IsWindowVisible, SetWindowLongW, SetWindowPos,
                        GWL_EXSTYLE, HWND_TOPMOST, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE,
                        SWP_NOSIZE, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
                    };
                    let hwnd = HWND(raw_hwnd as *mut _);
                    // 窗口已销毁：退出轮询，避免对失效 HWND 反复 SetWindowPos
                    if !IsWindow(Some(hwnd)).as_bool() {
                        break;
                    }
                    // 隐藏时跳过置顶与样式维护（保持 3s 轮询）：
                    // 悬浮窗关闭/禁用后不再每 500ms 白做系统调用（后台功耗）。
                    //
                    // 间隔每轮都按当前可见性重算。原来只有这一行 `sleep_ms = 500` 而没有
                    // 回落分支（全函数 `3000` 只出现在初始化那一行）—— 于是**只要悬浮窗
                    // 打开过一次**，这个线程就永远以 2 次/秒跑下去，关掉了也不停，
                    // 与本文件上面那句注释（"隐藏时保持 3s 轮询"）正好相反。
                    let visible = IsWindowVisible(hwnd).as_bool();
                    sleep_ms = if visible { 500 } else { 3000 };
                    if visible {
                        // 工具窗口样式：tao 的 skip_taskbar 只做 DeleteTab，仍会带 WS_EX_APPWINDOW，
                        // 任务管理器会把它当"应用"；这里每轮重申：置 TOOLWINDOW、清 APPWINDOW，
                        // 进程即可归类为后台进程（对齐 Python 版方案）。
                        let style = GetWindowLongW(hwnd, GWL_EXSTYLE);
                        let want = (style | WS_EX_TOOLWINDOW.0 as i32) & !WS_EX_APPWINDOW.0 as i32;
                        if style != want {
                            SetWindowLongW(hwnd, GWL_EXSTYLE, want);
                            let _ = SetWindowPos(
                                hwnd,
                                Some(HWND_TOPMOST),
                                0,
                                0,
                                0,
                                0,
                                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                            );
                        }
                        let _ = SetWindowPos(
                            hwnd,
                            Some(HWND_TOPMOST),
                            0,
                            0,
                            0,
                            0,
                            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                        );
                    }
                }
                std::thread::sleep(Duration::from_millis(sleep_ms));
            }
        })
        .map_err(|e| tracing::error!("启动悬浮窗置顶线程失败（该功能不可用）: {e}"))
        .ok();
}

/// 自动周报这一趟的结局：**四种分开**，别把"没做成"与"那一周本来没记录"并成一类。
///
/// 立这一族的原因：这一支原先只认"线程起没起来"。而生成被拒绝时（某一年的库读不动，
/// 周报宁可不出半份）`Err` 只留下一行 warn，锚点照推进 ⇒ 那一周的报告从此没有，
/// 设置页与托盘都不说，用户只能自己发现"周报怎么不出了"。上一版补上了"起不来不推进"，
/// 这次补的是另一半：**做没做成要由这一趟的实际结局说，而不是由线程的生命周期说**。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WeeklyRound {
    /// 文件真写出来了（带上路径，日志里要说的是它）
    Written(String),
    /// 那一周确实没有记录：这是答案，不是故障（新装、长期没用）
    NoRecord,
    /// 试过而没做成，原因原样带回去（读不动、写不出去都算这一支）
    Failed(String),
    /// 线程压根没起来（资源紧张）：同样没做成，同样要再试
    NotSpawned(String),
}

/// 这一周算不算「做完了」：做完了才推进锚点，没做完到点再试。
///
/// `NoRecord` 也算做完 —— 为"那一周本来没记录"每十分钟重跑一遍十四天的聚合只是占机器，
/// 而界面上本来就该什么都没有（这与"没有那一年库要说成读不出来"是同一条口径的另一头）。
fn weekly_round_settles(round: &WeeklyRound) -> bool {
    matches!(round, WeeklyRound::Written(_) | WeeklyRound::NoRecord)
}

/// 「上一次为这一周起过线程」到现在隔了多少秒 ⇒ 现在该不该再试一次。
///
/// `None` = 本进程还没试过（启动后那一拍立刻试，与旧口径一致）。间隔是为了不让"读不动的
/// 那一年"变成每 500ms 一次的十四天聚合 —— 那是这条判据要防的第二种病。
/// 参数已经是秒数而不是 `Instant`：`Instant - Duration` 在开机时间短于那个间隔时会 panic，
/// 而 release 是 `panic = "abort"`。
fn weekly_retry_due(since_last_attempt_secs: Option<u64>) -> bool {
    match since_last_attempt_secs {
        None => true,
        Some(secs) => secs >= WEEKLY_RETRY_SECS,
    }
}

/// 没做成时再试的间隔（秒）。一周一份的报告，十分钟的补救窗口足够接住
/// "移动盘还没醒/杀软首扫正握着"那一类瞬时的问不出。
const WEEKLY_RETRY_SECS: u64 = 600;

/// 「这一周的失败报过没有」：交回 `(这是第一次吗, 新的已报记录)`。
///
/// 同一周每十分钟重试一次，只在第一次走 error，之后降到 debug —— 否则日志被刷成噪声
/// （与托盘 tooltip 写失败那一处同一族规矩：出声只出一次，但**不许不出声**）。
fn weekly_speaks_once(reported_week: i64, this_week: i64) -> (bool, i64) {
    (reported_week != this_week, this_week)
}

/// 日志该走哪一档（调用方按这个选 `info!`／`debug!`／`error!`，判据只算"是哪一档"）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WeeklyLog {
    Info,
    Debug,
    Error,
}

/// 收到上一趟回话之后，这一拍要改哪些状态。抽成纯函数是因为这次要修的正是**状态推进**：
/// 上一版的病不在算式，而在"做成没做成"这一问被"线程起没起来"替掉了，而那两种改法
/// 在没有真线程的测试里长得一模一样。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WeeklyReconcile {
    /// 那一趟还没回来（或从来没跑过）：什么都不动
    Nothing,
    /// 这一周做完了：推进锚点、摘掉托盘那句"欠着"，并出这一条日志
    Settled {
        week: i64,
        line: String,
        level: WeeklyLog,
    },
    /// 这一周还欠着：挂上那句"欠着"并出这一条日志（同一周第二次起降到 debug）
    Outstanding {
        week: i64,
        line: String,
        level: WeeklyLog,
    },
}

/// 上一趟的回话落到哪一格。`reported = None` 是"还在跑"，不许被折成 `Nothing` 之外的一格
/// —— 折成"做完了"就是这次要修的那个洞。
fn weekly_reconcile(
    reported: Option<(i64, WeeklyRound)>,
    error_week: i64,
) -> (WeeklyReconcile, i64) {
    let Some((week, round)) = reported else {
        return (WeeklyReconcile::Nothing, error_week);
    };
    if weekly_round_settles(&round) {
        let (line, level) = match round {
            WeeklyRound::Written(p) => {
                (format!("自动周报已生成（{week} 周）: {p}"), WeeklyLog::Info)
            }
            _ => (
                "上一个整周没有记录，跳过自动周报".to_string(),
                WeeklyLog::Debug,
            ),
        };
        return (WeeklyReconcile::Settled { week, line, level }, error_week);
    }
    let why = match &round {
        WeeklyRound::Failed(w) | WeeklyRound::NotSpawned(w) => w.clone(),
        // 走到这里不可能（settle 的两支上面已经交出去了），但交回空串比 panic 便宜
        _ => String::new(),
    };
    let (first_time, next_error) = weekly_speaks_once(error_week, week);
    let level = if first_time {
        WeeklyLog::Error
    } else {
        WeeklyLog::Debug
    };
    let line = if first_time {
        format!(
            "自动周报没生成（{week} 周）：{why} —— 每 {WEEKLY_RETRY_SECS} 秒再试一次；\
             托盘那句「周报没生成」会说这一刻还欠着，设置页那颗按钮也点得出完整原因"
        )
    } else {
        format!("自动周报还是没生成（{week} 周）：{why}")
    };
    (
        WeeklyReconcile::Outstanding { week, line, level },
        next_error,
    )
}

/// 上一趟的结局：写者是周报线程（与"起线程就失败"那一支的调用方），读者是统计线程的下一拍。
/// **读走即清** —— 同一趟结果被算两次就会把锚点推进两回。
static WEEKLY_ROUND: Mutex<Option<(i64, WeeklyRound)>> = Mutex::new(None);
/// 「此刻是否还欠着一份周报」：托盘那句话读它（唯一的常驻出口 —— 启动进托盘是这个程序的
/// 默认形态，前端 toast 看不见的时候只剩这里能说）。完整原因在日志，也在设置页那颗按钮
/// 点下去之后的红字里。
static WEEKLY_OUTSTANDING: AtomicBool = AtomicBool::new(false);

/// 周报线程把结局交回统计线程的那一步。
pub(crate) fn publish_weekly_round(week: i64, round: WeeklyRound) {
    *WEEKLY_ROUND.lock().unwrap_or_else(|e| e.into_inner()) = Some((week, round));
}

/// 统计线程收一次回话：拿不到就是那一趟还在跑（或从来没跑过）。
fn take_weekly_round() -> Option<(i64, WeeklyRound)> {
    WEEKLY_ROUND
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
}

/// 托盘读的那一问：还欠着一份周报吗。
pub(crate) fn weekly_report_outstanding() -> bool {
    WEEKLY_OUTSTANDING.load(Ordering::SeqCst)
}

/// 挂上/摘掉那句"欠着"。摘的时机有两个：这一周真做完了，以及**为新一周起新趟之前**
/// （旧那一周的欠账不该被这一周继续念着 —— 那是念旧账）。
fn mark_weekly_outstanding(outstanding: bool) {
    WEEKLY_OUTSTANDING.store(outstanding, Ordering::SeqCst);
}

/// 窗口此刻是否「在用户眼前」：**最小化也算没在眼前**。
///
/// `is_visible()` 对最小化的窗口仍返回 true（Windows 的 `WS_VISIBLE` 还在，只是窗口被
/// 最小化了）。热键那条已经按同一判据修过一次（`hotkey::hotkey_should_hide`：只看
/// `is_visible()` 会让「最小化着按热键」变成再藏一次），统计线程这两处是漏下的另一半：
/// 最小化挂着的面板照样每 2 秒跑一次重聚合，还被 `stats-live` 事件唤醒渲染进程。
fn window_in_front(visible: bool, minimized: bool) -> bool {
    visible && !minimized
}

/// 可见性的**上升沿**：隐藏期间 `prev_*` 照旧在更新，而 emit 被可见性挡掉，于是呼出
/// 那一刻屏上仍是「藏起来那一拍」的数 —— 藏了一夜就是昨天的一整个「今日」，
/// 要等用户再碰一下键鼠才纠正。上升沿强制补推一拍。
fn just_shown(now_visible: bool, was_visible: &mut bool) -> bool {
    let rising = now_visible && !*was_visible;
    *was_visible = now_visible;
    rising
}

/// 后台统计线程：
/// 快节奏 500ms 更新今日/CPM 并推送 `stats-live`；
/// 重聚合在周期切换/超时/强制时执行并推送 `stats-charts`。
///
/// 返回线程是否起来了（启动自检 B15 用；失败已记 error 日志）。
fn spawn_stats_worker(
    app: AppHandle,
    db: Arc<Database>,
    config: &'static FocusFlowConfig,
    shared: Arc<Mutex<SharedStats>>,
    period: Arc<AtomicI64>,
    refresh_now: Arc<AtomicBool>,
) -> bool {
    std::thread::Builder::new()
        .name("stats-worker".into())
        .spawn(move || {
            tracing::info!("统计线程已启动");
            let mut prev_logged_today: i64 = -1;
            // "统计更新"日志限频时间戳（打字时计数每秒变化，避免刷屏）。
            // 初值用 Instant::now() 而非 now()-61s：开机初期（系统运行时间不足 61 秒）
            // Instant 减法会下溢 panic，release 下 panic=abort 直接崩进程。
            let mut last_stats_log = Instant::now();
            let tick_ms = 500u64;

            let mut prev_period: i64 = i64::MIN;
            // checked_sub：系统开机不足 1 小时时 Instant 减法会下溢 panic（panic=abort）。
            let mut last_heavy = Instant::now()
                .checked_sub(Duration::from_secs(3600))
                .unwrap_or_else(Instant::now);
            let mut prev_today_count: i64 = -1;
            let mut prev_cpm: i64 = -1;
            let mut prev_active: i64 = -1;
            // 上一拍的两个可见性：只用来算上升沿（呼出那一刻补推一拍并做一次重聚合）
            let mut prev_main_visible = false;
            let mut prev_floating_visible = false;
            // 上次重聚合时的今日计数：空闲且数据未变时跳过重聚合，避免无谓的整库查询
            let mut last_heavy_today: i64 = -1;
            // 各周期最高单日缓存：period -> (次数, 日期)；重聚合播种，今日破纪录时快节奏即时更新
            let mut period_max: std::collections::HashMap<i64, (i64, String)> =
                std::collections::HashMap::new();
            // 久坐提醒（`[rest]`）：判定与自节流都在 RestMonitor 里，
            // 这里每 tick 只喂一个"今日累计事件数"。
            let mut rest_monitor = focusflow_core::stats::RestMonitor::default();
            // "总计"(period=0) 聚合缓存与全历史最高单日缓存：
            // 两者都需要跨年度库全表扫描/全表 ORDER BY，active 的 2s 快节奏不触发重算，
            // 仅在 强制刷新 / 跨天 / 今日新增写入量 ≥ ALLTIME_RECALC_THRESHOLD 时失效重建。
            const ALLTIME_RECALC_THRESHOLD: i64 = 500;
            let mut alltime_agg: Option<ChartAgg> = None;
            let mut alltime_max: Option<(String, i64)> = None;
            // 缓存构建时的全历史总次数（-1 = 尚未构建）
            let mut alltime_total_base: i64 = -1;
            let mut alltime_cache_day: i64 = -1; // 上次缓存构建时的日期（CE 天序号）
                                                 // 显示锚：`alltime_total_base` 里**已经含到的**那份今日（今日计数 - 未落库增量）。
            let mut alltime_cache_today: i64 = -1;
            // 重建那一刻的今日计数：只用来量"自上次重建以来又攒了多少新键"。
            // 这两把尺子必须分开：拿显示锚当触发量，年度库整天写不进去时未落库的那一堆
            // 就是当天全部按键 —— `今日 - 锚` 恒久超过阈值，于是每次重聚合都要跑一遍
            // 跨全部年度库的全历史扫描，恰恰在最糟的那一天把机器打满。
            let mut alltime_stamp_today: i64 = -1;
            // period != 0 图表的落库序号指纹：DB 自上次聚合后没有新落库（序号未变）
            // 时，重聚合只会重复算出同样结果——增量都在写线程内存里，flush 前不进库。
            let mut charts_seq: u64 = u64::MAX;
            let mut charts_period: i64 = i64::MIN;
            // 自动周报：**做成过**的那一周的锚点（写出来了，或确认那一周没有记录）。
            // 旧口径记的是"起过线程的那一周"—— 那不等于做成了，失败那一周从此不再试。
            let mut settled_report_week: i64 = 0;
            // 上一次为这一周起线程的时刻（None = 本进程还没试过）：再试的间隔由它算
            let mut weekly_attempt_at: Option<Instant> = None;
            // 「哪一周的失败已经报过」：失败那一周只 error 一条，之后降到 debug
            let mut weekly_error_week: i64 = i64::MIN;

            loop {
                let period_val = period.load(Ordering::Relaxed);
                let forced = refresh_now.swap(false, Ordering::Relaxed);
                let period_changed = period_val != prev_period;
                // 跨过午夜、今天还没按下第一个键时，写线程的今日缓存仍是昨天的
                // （它在**下一条事件**到来时才清零）。照抄会让悬浮窗/面板把昨天一整天的
                // 数当成"今日"挂着，直到用户碰一下键鼠；显示侧按日期对一次。
                let cur_today = db
                    .writer()
                    .map(|w| w.today_count_for_display())
                    .unwrap_or(0) as i64;
                let cur_active = db
                    .writer()
                    .map(|w| w.today_active_for_display())
                    .unwrap_or(0) as i64;
                let active = cur_today != prev_today_count;
                // 写线程刚落过一批：`pending` 会被清空，而 `agg.total` 仍是上一轮聚合的
                // 库值 —— 这拍不重聚合就推送的话，周期总数会当场少掉这批，且空闲时
                // `last_heavy_today == cur_today` 会让下一轮判成"数据未变"而永远不补。
                let flush_seq_now = db.writer().map(|w| w.flush_seq()).unwrap_or(0);
                let heavy_elapsed_ms = last_heavy.elapsed().as_millis() as u64;
                let day_ce = {
                    use chrono::Datelike;
                    chrono::Local::now().date_naive().num_days_from_ce()
                } as i64;
                let alltime_dirty = forced
                    || alltime_max.is_none()
                    || day_ce != alltime_cache_day
                    || (alltime_stamp_today >= 0
                        // 写线程的今日计数只在**新的一天第一次按键**时才归零，所以跨过
                        // 零点后的第一轮重建会把"昨天的计数"当成今天的基准 stamp 进去
                        // —— 之后 cur_today 一直小于它，`cur_today - 基准` 恒为 0，
                        // 「总计」卡片整整一天都不再动。计数往回走本身就说明换了天，
                        // 用它再触发一次重建，把基准重新钉在新的一天上。
                        && (cur_today - alltime_stamp_today >= ALLTIME_RECALC_THRESHOLD
                            || cur_today < alltime_stamp_today));

                // 重聚合节奏随主窗口可见性自适应：
                // - 主窗口打开：活跃（打字）时每 active_refresh_interval 秒刷新一次图表；
                //   空闲时按 full_refresh_interval（配置）刷新。
                // - 主窗口隐藏：悬浮窗/托盘只需要今日计数与速度（快节奏 500ms），
                //   重聚合完全停掉，只在强制/周期切换时执行（打开窗口会触发强制刷新）。
                let main_visible = app
                    .get_webview_window("main")
                    .map(|w| {
                        window_in_front(
                            w.is_visible().unwrap_or(false),
                            w.is_minimized().unwrap_or(false),
                        )
                    })
                    .unwrap_or(false);
                // 悬浮窗是不带标题栏的工具窗（`make_floating_tool_window`），没有最小化
                // 这一态，所以只有主窗口需要认 `is_minimized()`；这里少问一次也少一次
                // 「投递主线程再等回话」。
                // 走 façade 而不是 `get_webview_window("floating")`：原生那张卡片不在 Tauri 的
                // 窗口集合里，按 label 找会静默拿到 None（`floating` 模块头点名了这类接线）。
                let floating_visible = crate::floating::is_visible(&app);
                let main_just_shown = just_shown(main_visible, &mut prev_main_visible);
                let floating_just_shown = just_shown(floating_visible, &mut prev_floating_visible);
                let heavy_interval_ms = if !main_visible {
                    u64::MAX
                } else if active {
                    (config.get_int("gui", "active_refresh_interval", 2).max(1) as u64) * 1000
                } else if idle_refresh_needed(
                    cur_today,
                    last_heavy_today,
                    flush_seq_now,
                    charts_seq,
                ) {
                    // 空闲但今日计数变了、或库在上一轮聚合之后又落过一批：按空闲周期刷新。
                    // 后一半不能漏：flush 把 pending 清零而库值要等重聚合才进 `agg.total`，
                    // 只看今日计数的话，最后一轮聚合之后落的那批就再也补不上。
                    (config.get_int("gui", "full_refresh_interval", 10).max(1) as u64) * 1000
                } else {
                    // 空闲且数据未变：无需重算，等有输入或强制/周期切换
                    u64::MAX
                };
                // 呼出主窗口也算强制重聚合：最小化期间重聚合停着，恢复后不能等下一个周期
                // 才把图表补上（"卡片先回昨天、图表还是藏起来那一刻的样子"是两件事）。
                let do_heavy = forced
                    || period_changed
                    || main_just_shown
                    || heavy_elapsed_ms >= heavy_interval_ms
                    // 只在窗口可见时补这一拍：隐藏/最小化时重聚合本来就停着（成本考虑），
                    // 落库不需要把它叫醒；等 `main_just_shown` 那一拍会连同最新库值一起算。
                    || (main_visible && flush_seq_now != charts_seq);

                // 自动周报：进程启动后的第一轮、以及跨进新一周后的第一轮，为
                // 「刚结束的那个整周」生成一次 Markdown 报告。锚点取本周一，所以同一周内
                // marker 恒定 —— 做成一次就不再重做。生成要跑 7 天的聚合查询，
                // 放进独立线程，不占用本线程的 500ms 快节奏。
                //
                // 三件事分开：**写出来了**、**那一周确实没有记录**（那是答案，安静）、
                // **没做成**（原因进日志、托盘那句"周报没生成"挂上、按 `WEEKLY_RETRY_SECS` 再试）。
                // 旧写法只认"线程起没起来"：一次生成被拒绝（某一年的库读不动时周报宁可不出半份）
                // 只留下一行 warn，锚点照推进 ⇒ 那一周的报告从此没有，而设置页与托盘一个字都不说 ——
                // 用户只能自己发现"周报怎么不出了"。
                let week_marker = {
                    use chrono::Datelike;
                    let (f, _) = focusflow_core::stats::last_finished_week(
                        chrono::Local::now().date_naive(),
                    );
                    f.num_days_from_ce() as i64
                };
                // 先把上一趟的回话收走（收不到就是那一趟还在跑）
                let (reconcile, next_error_week) =
                    weekly_reconcile(take_weekly_round(), weekly_error_week);
                match reconcile {
                    WeeklyReconcile::Nothing => {}
                    WeeklyReconcile::Settled { week, line, level } => {
                        settled_report_week = week;
                        mark_weekly_outstanding(false);
                        match level {
                            WeeklyLog::Info => tracing::info!("{line}"),
                            _ => tracing::debug!("{line}"),
                        }
                    }
                    WeeklyReconcile::Outstanding {
                        week: _,
                        line,
                        level,
                    } => {
                        mark_weekly_outstanding(true);
                        match level {
                            WeeklyLog::Error => tracing::error!("{line}"),
                            _ => tracing::debug!("{line}"),
                        }
                    }
                }
                weekly_error_week = next_error_week;
                let since_attempt = weekly_attempt_at.map(|t| t.elapsed().as_secs());
                if week_marker != settled_report_week && weekly_retry_due(since_attempt) {
                    // 起新趟之前先把那句"欠着"摘掉：上一周那笔不该被这一周继续挂着念
                    mark_weekly_outstanding(false);
                    weekly_attempt_at = Some(Instant::now());
                    let spawned = std::thread::Builder::new()
                        .name("weekly-report".into())
                        .spawn(move || {
                            let round = match crate::export::write_weekly_report() {
                                Ok(Some(p)) => WeeklyRound::Written(p.display().to_string()),
                                // 新装/长期没用：那一周没有记录，本来就没有可报告的
                                Ok(None) => WeeklyRound::NoRecord,
                                Err(e) => WeeklyRound::Failed(e.to_string()),
                            };
                            publish_weekly_round(week_marker, round);
                        });
                    if let Err(e) = spawned {
                        // 线程压根没起来也算这一周没做成：挂上原因、到点再试，不许咽掉
                        publish_weekly_round(week_marker, WeeklyRound::NotSpawned(e.to_string()));
                    }
                }

                if do_heavy {
                    let flush_seq = db.writer().map(|w| w.flush_seq()).unwrap_or(0);
                    // 今日未落库的增量：日均/周期总数两处展示值都要补上它
                    // （读取与 flush_seq 同刻，聚合基于的库状态才自洽）。
                    let pending_today = db.writer().map(|w| w.today_pending_count()).unwrap_or(0);
                    // period != 0 且库内容未变：跳过整轮重算与推送（活跃打字时每 2s
                    // 到期的重聚合，在没有新落库时全部是重复计算，这里是主要开销）
                    let charts_unchanged = !forced
                        && !period_changed
                        && !alltime_dirty
                        && period_val != 0
                        && period_val == charts_period
                        && flush_seq == charts_seq;
                    if charts_unchanged {
                        last_heavy = Instant::now();
                    } else {
                        // 只要真的走了重算这一支，就得记下"这次重算的时刻与当时的今日计数"。
                        // 原先只在 `alltime_dirty` 里记：而 `charts_unchanged` 带着
                        // `period_val != 0` 的条件，于是**总计周期（period=0，也是
                        // [gui] default_period 的默认值）永远走不到那条"空闲且数据未变就
                        // u64::MAX"的分支** —— 节奏一旦到期就每 500ms 重算+推送一次，
                        // 人走了也不停（`last_heavy_today` 同样从不更新，永远"数据有变化"）。
                        last_heavy = Instant::now();
                        last_heavy_today = cur_today;
                        // 全历史最高单日（全表 ORDER BY，最贵的单项查询）：仅缓存失效时重查
                        if alltime_dirty {
                            // 请求写线程落库（非阻塞：只发信号，不在这里等）。
                            //
                            // 这里曾用 flush(true)：写线程若正处重试退避（500ms/1s）或库被锁，
                            // 统计线程会阻塞最长 3 秒，期间 stats-live 推不出去 ——
                            // 悬浮窗数字肉眼可见地冻住。图表类的总数/最高单日只是展示值：
                            // 最多一个 flush 周期（10 秒）后追上最新落库，
                            // 期间 alltime_total_now 用今日增量自行修正，不需要阻塞等待。
                            if let Some(w) = db.writer() {
                                if w.has_pending() {
                                    w.flush(false);
                                }
                            }
                            let today_str = chrono::Local::now()
                                .date_naive()
                                .format("%Y-%m-%d")
                                .to_string();
                            // 总计与最高单日同源同失效条件：一次跨库遍历同时取回
                            let totals = focusflow_core::db::get_alltime_summary();
                            // 换基线的条件见 `alltime_baseline_adoptable`：有某年没读出来
                            // 而手上又有上一份时，宁可继续用上一份，也不把残缺的总数当成
                            // 新事实 —— 缓存要到跨天/今日阈值才重建，那会是一整天。
                            if alltime_baseline_adoptable(
                                totals.unreadable.is_empty(),
                                alltime_total_base >= 0,
                            ) {
                                alltime_total_base = totals.total;
                                alltime_max = Some(totals.max_day.unwrap_or((today_str, 0)));
                                // 锚取的是「base 里已经含到的那一份今日」，不是此刻的今日计数：
                                // `totals.total` 来自 daily_counts 的 SUM，只到上次落库为止，而今日
                                // 计数是内存里那份（含未落库增量，默认落后最多一个 flush 周期）。
                                // 读取次序是有意排的：先库（t1）后内存（t2>t1），中间落库的那批
                                // 只会让显示值暂时偏低、下一次重建就追上；反过来读会把同一批**双计**，
                                // 「总计」先涨后跌 —— 比偏低更难查，因为用户会以为自己在掉数据。
                                alltime_cache_today = db
                                    .writer()
                                    .map(|w| {
                                        alltime_anchor_today(
                                            w.today_count_for_display() as i64,
                                            w.today_pending_count(),
                                        )
                                    })
                                    .unwrap_or(cur_today);
                                alltime_stamp_today = cur_today;
                            }
                            // 两支都要推进「哪天重建过」：沿用上一份那一支不更新基准，靠
                            // `alltime_stamp_today` 没动 → 今日增量攒到阈值再试一次。
                            alltime_cache_day = day_ce;
                        }

                        // period=0 且缓存有效：直接复用上次聚合结果，不再全库扫描。
                        // 今日破纪录等展示值由下方增量逻辑修正，轻量零查询。
                        let mut agg = if period_val == 0
                            && !period_changed
                            && !alltime_dirty
                            && alltime_agg.is_some()
                        {
                            alltime_agg.clone().unwrap()
                        } else {
                            let agg =
                                compute_charts(period_val, alltime_max.clone(), pending_today);
                            if period_val == 0 {
                                alltime_agg = Some(agg.clone());
                            }
                            agg
                        };
                        charts_period = period_val;
                        // 记的是"这一份聚合数据对应到哪一次落库"，所以必须用**取数之前**
                        // 读到的那个 `flush_seq`。原来在这里重新读一次写线程的计数器：
                        // `compute_charts` 本身要跑几十毫秒到几百毫秒，期间写线程完全可能
                        // 又完成一次 flush —— 于是那一批增量被记成"已经反映过了"，
                        // 而同一轮又把 `last_heavy_today = cur_today` 写成"数据没变化"，
                        // 结果是这批数据在趋势图/排行里**缺席**，直到用户再动一次键鼠
                        // 才有机会补上（停手时就一直停在这个偏小的值上）。
                        charts_seq = flush_seq;
                        // 图表推送也必须带上"缓存基准 + 今日增量"的那个总计：`compute_charts`
                        // 里的 alltime_total 是硬编码 0（重聚合不付一次全历史扫描），而前端
                        // applyTotal 在「今日」周期取的就是这个字段 —— 于是每来一次图表推送，
                        // 「总计」卡片就被写成 0，只有下一次 live 推送才改回来（停止输入后就
                        // 停在 0 上）。下面那段补丁只改了共享快照，晚于这里的 emit。
                        agg.alltime_total =
                            alltime_total_now(alltime_total_base, alltime_cache_today, cur_today);
                        // 同一个口径补到展示值上：否则图表推送里「周期总数」停在纯库值，
                        // 而同一条推送里的「日均」已经补过今日未落库的增量（两把尺子），
                        // 且这一推送晚于 live 推送时卡片会偏低（见 `apply_display_total`）。
                        apply_display_total(
                            period_val,
                            agg.alltime_total,
                            alltime_total_base >= 0,
                            pending_today,
                            &mut agg,
                        );
                        period_max.insert(period_val, (agg.max_day, agg.max_day_date.clone()));
                        {
                            let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                            s.agg = agg.clone();
                        }
                        // 图表数据仅主窗口使用：主窗口隐藏时跳过推送，
                        // 避免每轮重聚合都唤醒隐藏的渲染进程（打开窗口时 refresh_now 会强制重聚合）
                        if main_visible {
                            let _ = app.emit_to(
                                "main",
                                "stats-charts",
                                ChartsStats {
                                    period: period_val,
                                    agg,
                                },
                            );
                        }
                    }
                }

                let today_count = cur_today;
                let cpm = focusflow_core::stats::cpm(config).get_cpm();

                // 增量维护各周期最高单日：今日计数超过纪录立即更新（零 DB 查询）。
                // 今日包含在一切周期窗口内，一次比较对所有周期成立。
                if today_count > 0 {
                    let today_str = chrono::Local::now()
                        .date_naive()
                        .format("%Y-%m-%d")
                        .to_string();
                    for (v, d) in period_max.values_mut() {
                        if today_count > *v {
                            *v = today_count;
                            *d = today_str.clone();
                        }
                    }
                }
                let (max_day, max_day_date) = period_max
                    .get(&period_val)
                    .cloned()
                    .unwrap_or((0, String::new()));

                // 全历史总计由缓存基准 + 今日增量修正，与最高单日一样零 DB 查询
                let alltime_total =
                    alltime_total_now(alltime_total_base, alltime_cache_today, cur_today);

                // 今日尚未落库的那部分增量 —— 「近 N 天」的窗口和只到上次 flush 为止，
                // 不在这里补上，那张卡片就只能每 10 秒跳一次（「总计」「今日」同理走这条快路径）。
                let pending_today = db.writer().map(|w| w.today_pending_count()).unwrap_or(0);
                let (live_alltime_total, period_total) = {
                    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                    s.today_count = today_count;
                    s.cpm = cpm;
                    s.active_seconds = cur_active;
                    s.period = period_val;
                    s.agg.max_day = max_day;
                    s.agg.max_day_date = max_day_date.clone();
                    s.period = period_val;
                    // period 与两个总数在同一把锁内快照：前端拿到的永远是自洽的一组。
                    // 修正只在这里做（快照的 `agg.total` 仍是原始库基准，见
                    // `apply_display_total`）。
                    apply_display_total(
                        period_val,
                        alltime_total,
                        alltime_total_base >= 0,
                        pending_today,
                        &mut s.agg,
                    );
                    (s.agg.alltime_total, s.agg.period_total)
                };

                let live_changed = today_count != prev_today_count
                    || cpm != prev_cpm
                    || cur_active != prev_active
                    || period_val != prev_period;
                // 呼出那一刻即使数字没变也要补推一次：隐藏期间这些 prev_* 照样在更新，
                // 只看 `live_changed` 的话"刚呼出的窗口"永远收不到它缺的那一拍。
                if live_changed || main_just_shown || floating_just_shown {
                    let live = LiveStats {
                        today_count,
                        cpm,
                        active_seconds: cur_active,
                        period: period_val,
                        max_day,
                        max_day_date,
                        alltime_total: live_alltime_total,
                        period_total,
                    };
                    // 事件定向推送：只发给实际可见的窗口。
                    // 隐藏的窗口渲染进程已停（SetIsVisible=false），不再被 500ms 事件唤醒。
                    //
                    // 原生那张卡片收不到事件（它不在 Tauri 的窗口注册表里），改读同一格
                    // parked 快照；所以推送这一支要先问"这一轮是不是 WebView2 在渲染"，
                    // 否则每条 emit 都在往一个不存在的窗口发，而 `let _ =` 会把这件事咽下去。
                    crate::floating::publish_live(&live);
                    if floating_visible && crate::floating::pushes_events() {
                        let _ = app.emit_to("floating", "stats-live", &live);
                    }
                    if main_visible {
                        let _ = app.emit_to("main", "stats-live", live);
                    }
                }
                prev_today_count = today_count;
                prev_cpm = cpm;
                prev_active = cur_active;
                prev_period = period_val;

                // 久坐提醒：不受"有没有变化"影响，每个 tick 都喂一次判定
                // （RestMonitor 自己按 [rest] check_interval 节流）。
                if let Some(notice) = rest_monitor.observe(Instant::now(), cur_today, config) {
                    tracing::info!(
                        "久坐提醒: 近 {} 分钟 {} 次键鼠事件，建议休息 {} 秒",
                        notice.window_minutes,
                        notice.events_in_window,
                        notice.rest_seconds
                    );
                    // 先挂到托盘读得到的那一份，**再**判窗口在不在眼前：下面两支都要看可见性，
                    // 而"两支都不成立"正是这条提醒整条无声消失的那一档 —— 面板没开、悬浮窗
                    // 关掉或隐藏着（渲染进程停着，收了也不会画），用户能拿到的只剩日志里那一句，
                    // 而日志是这个程序最后才有人翻的地方。托盘是常驻的，理由与启动自检的 ⚠ 计数
                    // 挂在 tooltip 上那一条相同。
                    publish_rest_notice(&notice);
                    // 与 stats-live 同一套定向推送：隐藏的窗口渲染进程是停着的
                    if main_visible {
                        // 送不到要点名：`let _ =` 会把"没人收"读成"发成功了"
                        if let Err(e) = app.emit_to("main", "rest-reminder", &notice) {
                            tracing::warn!("久坐提醒没能送到主窗口：{e}（托盘那一句还会说）");
                        }
                    }
                    if floating_visible && crate::floating::pushes_events() {
                        if let Err(e) = app.emit_to("floating", "rest-reminder", &notice) {
                            tracing::warn!("久坐提醒没能送到悬浮窗：{e}（托盘那一句还会说）");
                        }
                    }
                }

                if today_count != prev_logged_today {
                    // 限频：打字时今日计数每秒都在变，每 60 秒最多记一条，避免日志刷屏
                    if last_stats_log.elapsed() >= Duration::from_secs(60) {
                        tracing::info!("统计更新: today={today_count} cpm={cpm}");
                        last_stats_log = Instant::now();
                    }
                    prev_logged_today = today_count;
                }

                std::thread::sleep(Duration::from_millis(tick_ms));
            }
        })
        .map_err(|e| tracing::error!("启动统计线程失败（该功能不可用）: {e}"))
        .is_ok()
}

/// 空闲档要不要再来一轮重聚合。
///
/// 只看"今日计数变没变"会漏掉一种：最后一轮聚合之后、人停手之前落进库的那一批。
/// flush 把写线程的 pending 清零，库里的值却要等重聚合才进 `agg.total`；若这拍
/// 判成"数据未变"，周期总数就会少掉那批且一直不补（要等下一次输入才回头重算）。
fn idle_refresh_needed(
    cur_today: i64,
    last_heavy_today: i64,
    flush_seq: u64,
    charts_seq: u64,
) -> bool {
    cur_today != last_heavy_today || flush_seq != charts_seq
}

/// 全历史总计 = 缓存基准 + 今日自缓存构建以来的增量（零 DB 查询）。
///
/// 今日是唯一会实时增长的部分：跨天、导入、清库都会让 `alltime_dirty` 重建基准。
/// 增量取 `max(0)`——数据被删除或跨天重置后今日计数可能小于缓存时的值，
/// 此时宁可显示略旧的基准，也不能把总计减成负数。
fn alltime_total_now(base: i64, cached_today: i64, cur_today: i64) -> i64 {
    if base < 0 {
        // 基准尚未构建（首轮重聚合前）：宁可显示 0，也不显示残缺的总计
        return 0;
    }
    base + (cur_today - cached_today).max(0)
}

/// 重建基准时，`alltime_total_now` 的那个 `cached_today` 该钉在哪个数上。
///
/// 锚必须是**已经算进 `base` 里**的那份今日，不能是此刻的今日计数。两者差的正是
/// 「还没落库的增量」（`today_pending_count`）：拿后者当锚，重建当场 `cur - 锚` 恒为 0，
/// 显示值就等于刚读出来的 `base`，少的就是那批增量；而上一轮缓存在此之前已经把
/// 这些键当作增量加过一次，于是**用户正在打字，「总计」却当场退回一截**
/// （每满 `ALLTIME_RECALC_THRESHOLD` 次按键、每次跨天或强制刷新都会重演一次）。
/// 锚取 `今日计数 - 未落库` 之后，重建前后的值连续，且当场就等于真值。
fn alltime_anchor_today(cur_today: i64, pending_today: i64) -> i64 {
    (cur_today - pending_today).max(0)
}

/// 这一次跨库汇总能不能当成新的全历史基线。
///
/// 规则只有一句：**读全了才换**。有年度库没读出来时，`total` 只是已读部分之和（下界），
/// 而这份基线一旦写下去就要撑到下次跨天/今日增量达阈值/强制刷新 —— 于是一次瞬时文件锁
/// （另一个实例握着库、记事本开着 config、杀软首扫）就把「总计」卡片永久改低，用户看到的是
/// "我在掉历史"，而事实只是那一次没读到。
///
/// 例外是手上根本没有上一份（`alltime_total_base` 还是 `-1` 哨兵）：这时只能先用这份残缺值。
/// 留 `alltime_max = None` 不是选项 —— `alltime_dirty` 的条件里就有 `alltime_max.is_none()`，
/// 那一轮之后每轮都算脏，统计线程会一直重扫全部年度库。
fn alltime_baseline_adoptable(read_all_years: bool, have_previous: bool) -> bool {
    read_all_years || !have_previous
}

/// 悬浮窗"至少还得露出这么多像素"才算看得见；低于这个才认为它跑丢了。
const FLOATING_MIN_VISIBLE: f64 = 20.0;

/// 一块屏幕的位置与尺寸 + 悬浮卡片尺寸（合成一个参数，避开 `too_many_arguments`）。
struct FloatingBounds {
    left: f64,
    top: f64,
    screen_w: f64,
    screen_h: f64,
    win_w: f64,
    win_h: f64,
}

/// 把存过的悬浮窗坐标夹回屏幕，但**只在卡片基本看不见时才动它**。
///
/// 要防的是那一件事：换显示器/拔副屏之后坐标落在屏幕之外，而下面 `win.show()` 照样执行
/// —— 表现是"设置里勾了悬浮窗却看不见"，界面上没有任何入口能召回（只能去改 config.ini）。
///
/// 判据不能用**工作区**：悬浮窗是 top-most 工具窗，压在任务栏上照样看得见。
/// 拿工作区夹会把人故意放在屏幕底边的位置每次启动往上拽一截，而且是拽到一个确定值
/// （他实测：1920x1080、工作区底边 1032、卡片高 46、存的 `pos_y = 1031` → 被夹到 992，
/// 于是"我放好了，重开又跑到固定位置"）。所以用整块屏幕的边界，并允许卡片只要
/// 露出 `FLOATING_MIN_VISIBLE` 就原样保留 —— 越界时也只朝看不见的方向挪最小的一步。
fn clamp_floating_into_screen(x: f64, y: f64, b: FloatingBounds) -> (f64, f64) {
    let min_x = b.left - b.win_w + FLOATING_MIN_VISIBLE;
    let max_x = b.left + b.screen_w - FLOATING_MIN_VISIBLE;
    let min_y = b.top - b.win_h + FLOATING_MIN_VISIBLE;
    let max_y = b.top + b.screen_h - FLOATING_MIN_VISIBLE;
    // max 可能小于 min（屏幕比卡片还小，比如 640x480 的副屏配大悬浮窗）→ 夹成单点而不是 panic
    (
        x.clamp(min_x, max_x.max(min_x)),
        y.clamp(min_y, max_y.max(min_y)),
    )
}

/// 「总计」那张卡片该显示哪个数（前端只读 `period_total`，除了「今日」页读
/// `alltime_total`）。抽成纯函数是为了把这条规则钉住 —— 它出错过一次：
///
/// `applyTotal` 在 `period == -1`（今日）取 `alltime_total`，其余周期取
/// `period_total`（= `agg.total`）。而**标签写着「总计」的那一页正是 `period == 0`**：
/// 它显示的是上次重算时从库里读出来的数，重算要等 `alltime_dirty`（每 500 次按键 /
/// 跨天 / 强制刷新）才发生 —— 于是同一个「总计」，切到「今日」逐键跳动、
/// 切到「总计」一整天不动。
///
/// `compute_charts(0).total` 与 `alltime_total_base` 是同一个量（都是全跨年库的
/// Σ`daily_counts.count`），所以 period=0 时直接用那个被"基准 + 今日增量"修正过的值。
/// 基准还没建立时**不能**覆盖：那时 `alltime_total_now` 回 0，会把一个本来正确的
/// 库值清零（首轮重聚合之前恰好就是这个状态）。
fn total_for_display(
    period_val: i64,
    agg_total: i64,
    alltime_total: i64,
    base_ready: bool,
    pending_today: i64,
) -> i64 {
    if period_val == 0 {
        // 总计 = 全历史，与「今日」页用的是同一个每 tick 修正的量
        return if base_ready { alltime_total } else { agg_total };
    }
    if period_val > 0 {
        // 近 N 天的窗口和里，今日那部分只到上次落库为止；补上内存里还没落库的
        // 那份才是完整的今日。库里长出同一批时内存里那项已被清空，不会重复计。
        return agg_total + pending_today;
    }
    agg_total
}

/// 一轮重算/每 tick 快照都要产出的两个展示值写在一起，只这一个入口：
/// `alltime_total`（缓存基准 + 今日增量）与 `period_total`（所选周期的展示值）。
///
/// `agg.total` **保持纯库值**：它就是共享快照里那份基准，live 推送每轮在它上面
/// 加一次今日未落库的增量。把展示值也写回 `total`，下一轮就成 `raw + 2×pending`
/// 的双重计数 —— 所以两个数必须是两个字段（原先图表推送为了避免双重计数只能
/// 恒传 pending=0，代价是那张卡片比 live 推送和同一条推送里的「日均」少一截）。
fn apply_display_total(
    period_val: i64,
    alltime_total: i64,
    base_ready: bool,
    pending_today: i64,
    agg: &mut ChartAgg,
) {
    agg.alltime_total = alltime_total;
    agg.period_total = total_for_display(
        period_val,
        agg.total,
        alltime_total,
        base_ready,
        pending_today,
    );
}

/// 重聚合：按周期查询数据库并计算全部图表数据。
///
/// 纯函数（输入周期、输出聚合结果），从统计线程拆出便于单测；
/// 数据一致性由读侧 busy_timeout 与写线程事务保证。
/// `alltime_max` 为统计线程维护的全历史最高单日缓存（全表 ORDER BY 太贵，不在此重查；
/// 传 None 时回退现查，供单测与无缓存路径使用）。
fn compute_charts(
    period_val: i64,
    alltime_max: Option<(String, i64)>,
    pending_today: i64,
) -> ChartAgg {
    // 三个排行榜各自交回"这一轮没答上来的那些问"；合起来交给前端说（见 `ChartAgg::read_gaps`）。
    let key_totals = match period_val {
        -1 => focusflow_core::db::stats_totals_on(chrono::Local::now().date_naive()),
        0 => focusflow_core::db::stats_totals(None, None),
        n => focusflow_core::db::stats_totals(Some(n), None),
    };
    let (total, key_stats) = (key_totals.total, key_totals.rows);
    let key_gaps = key_totals.unreadable;
    // 前台应用使用时长排行（秒数），周期选择与按键统计联动。
    // 保留查询返回的总秒数作为占比分母（未截断，与 apps 之和不是一回事）。
    let (app_total, mut apps, app_gaps): (i64, Vec<(String, i64)>, Vec<String>) = {
        let read = match period_val {
            -1 => focusflow_core::db::app_stats_totals_on(chrono::Local::now().date_naive()),
            0 => focusflow_core::db::app_stats_totals(None, None),
            n => focusflow_core::db::app_stats_totals(Some(n), None),
        };
        (read.total, read.rows.into_iter().collect(), read.unreadable)
    };
    sort_rank_desc(&mut apps);
    apps.truncate(RANK_LIMIT);
    // 设备维度统计（Raw Input 侧信道，独立口径：键盘按下 + 鼠标按键 + 滚轮）。
    // 查询侧已按次数降序、显示名去重，这里只截断。
    let device_read = match period_val {
        -1 => focusflow_core::db::device_totals_on(chrono::Local::now().date_naive()),
        0 => focusflow_core::db::device_totals(None, None),
        n => focusflow_core::db::device_totals(Some(n), None),
    };
    let (device_total, mut devices, device_gaps) = (
        device_read.total,
        device_read.devices,
        device_read.unreadable,
    );
    let device_count = devices.len() as i64;
    devices.truncate(RANK_LIMIT);
    let mut rank: Vec<(String, i64)> = key_stats.iter().map(|(k, v)| (k.clone(), *v)).collect();
    sort_rank_desc(&mut rank);
    rank.truncate(RANK_LIMIT);
    let mut groups: std::collections::HashMap<&'static str, i64> = std::collections::HashMap::new();
    for (k, v) in &key_stats {
        let g = classify_key(k);
        *groups.entry(g).or_insert(0) += v;
    }
    let group: Vec<(String, i64)> = KEY_GROUPS
        .iter()
        .filter_map(|g| groups.get(*g).map(|c| (g.to_string(), *c)))
        .collect();
    let mouse_total: i64 = focusflow_core::format::MOUSE_GROUPS
        .iter()
        .map(|g| groups.get(*g).copied().unwrap_or(0))
        .sum();
    let keyboard_total = total - mouse_total;
    let daily_days = focusflow_core::db::queries::chart_window_days(period_val);
    let needed = daily_days.max(7).max(30);
    // 每日趋势走带原因的读数：读不全时这条曲线只是下界，前端要把"这一轮没读全"说出来
    let daily_read = focusflow_core::db::daily_counts_totals(needed, None);
    let daily_all = daily_read.series;
    let total_days = daily_all.len() as usize;
    let counts: Vec<i64> = if total_days >= daily_days as usize {
        daily_all[total_days - daily_days as usize..]
            .iter()
            .map(|(_, c)| *c)
            .collect()
    } else {
        Vec::new()
    };
    let avg = if counts.is_empty() {
        // 窗口里还没有任何落库行（刚装好/今天还没落过）：今天若有未落库增量，
        // 它就是窗口里唯一"有数据的一天"，日均即其本身（没数据 ≠ 0 的口径）。
        pending_today.max(0)
    } else {
        // 「日均不跟 pending 走」（§七 记档的不完美）：窗口一定以今天收尾，
        // 把今天未落库的增量补进分子，分母不动 —— 与「总计」「近 N 天」卡片
        // 的 total_for_display 同一口径。
        (counts.iter().sum::<i64>() + pending_today) / counts.len() as i64
    };
    // 最高单日：今日/总计 = 全历史纪录（含日期）；N天 = 窗口内最大（含日期）。
    // 窗口值从 daily_all 取（已含落库后的今日），历史纪录跨库取。
    let today_str = chrono::Local::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string();
    let (max_day, max_day_date) = if period_val == -1 || period_val == 0 {
        match alltime_max {
            Some((d, c)) => (c, d),
            None => {
                // 缓存还没建起来时当场跨库取一次；没有纪录就沿用「今天 0 次」这个占位。
                let (d, c) = focusflow_core::db::get_alltime_summary()
                    .max_day
                    .unwrap_or((today_str.clone(), 0));
                (c, d)
            }
        }
    } else {
        let window: Vec<(String, i64)> = if total_days >= daily_days as usize {
            daily_all[total_days - daily_days as usize..].to_vec()
        } else {
            daily_all.clone()
        };
        window
            .iter()
            .max_by_key(|(_, c)| *c)
            .map(|(d, c)| (*c, d.clone()))
            .unwrap_or((0, today_str.clone()))
    };
    let trend: Vec<(String, i64)> = if total_days >= 7 {
        daily_all[total_days - 7..].to_vec()
    } else {
        daily_all.clone()
    };
    let tail30: Vec<(String, i64)> = if total_days >= 30 {
        daily_all[total_days - 30..].to_vec()
    } else {
        daily_all.clone()
    };
    // 与顶部周期同窗口的那一段：三张图与「日均」卡片问的是同一个天数（`chart_window_days`）。
    // `needed` 本来就取到 `daily_days.max(7).max(30)`，所以这里不多问一次库。
    let trend_period: Vec<(String, i64)> = if total_days >= daily_days as usize {
        daily_all[total_days - daily_days as usize..].to_vec()
    } else {
        daily_all.clone()
    };
    // 星期分布在这里就铺成固定 7 格（周一..周日，窗口里没有的那天填 0）。
    // 原先交回的是"只有数据的那几天"，靠的是"窗口恒 >=30 天"这个巧合：今日周期只有 1 天，
    // 而前端按数组下标贴标签 —— 周四的数会挂在「周一」那一格上。密度由这里保证。
    let agg_wd = focusflow_core::db::queries::aggregate_weekday(&trend_period);
    let weekday: Vec<(i64, i64)> = (0..7i64)
        .map(|d| (d, *agg_wd.get(&d).unwrap_or(&0)))
        .collect();
    // 每小时分布跟着所选窗口：写死"今天"的时候，顶部选了「近 1 年」而图上念的还是"今日"
    let hourly_read = focusflow_core::db::hourly_window_totals(daily_days, None);
    let hourly = hourly_read.hours;
    // 星期分布是从每日趋势那一段里聚合出来的（`aggregate_weekday(&trend_period)`），
    // 所以它不另问一次库 —— 每日那一栏的 gap 就是这张图的 gap，前端按域名去挑。
    ChartAgg {
        total,
        // 展示值要等统计线程拿到 alltime 缓存与本轮 pending 才能定（apply_display_total）；
        // 这里先放纯库值，任何直接消费 ChartAgg 的路径都拿不到没算过的数。
        period_total: total,
        // 全历史总计要跨年度库汇总，不在此重复查询：统计线程每轮用
        // alltime 缓存 + 今日增量写入（见 alltime_total_now）
        alltime_total: 0,
        avg,
        max_day,
        max_day_date,
        rank,
        apps,
        app_total,
        devices,
        device_total,
        device_count,
        read_gaps: [
            key_gaps,
            app_gaps,
            device_gaps,
            daily_read.unreadable,
            hourly_read.unreadable,
        ]
        .concat(),
        group,
        mouse_total,
        keyboard_total,
        trend,
        trend30: tail30,
        trend_period,
        hourly,
        weekday,
    }
}

/// 最近一条久坐提醒，与它还"值得再说一遍"的截止时刻（unix 秒；0 = 一条都没有）。
///
/// 写者是统计线程，读者是托盘更新线程（每 5 秒一轮）。这两个量必须成对读写：只挂提醒
/// 不挂截止，托盘就会在半小时后继续念一条早就不成立的建议；只挂截止不挂提醒则什么也说不出。
static REST_NOTICE: Mutex<Option<focusflow_core::stats::RestNotice>> = Mutex::new(None);
static REST_NOTICE_UNTIL: AtomicI64 = AtomicI64::new(0);

/// 现在这一刻，这条提醒还算不算"眼前的话"。
///
/// 抽成纯函数是为了能把边界钉死（截止那一秒起就不许再说），`until = 0` 是"从来没有过"，
/// 与"曾经有过但已过期"同样交回 false，但两者都不该让托盘念旧账。
fn rest_is_fresh(now: i64, until: i64) -> bool {
    until > 0 && now < until
}

/// 统计线程发出一条提醒时先调这个：把提醒和它的可见时长挂到托盘读得到的地方。
///
/// 挂在可见性判断**之前**，见调用处那段 —— 这一步是整个"送达"里唯一不依赖窗口的一支。
fn publish_rest_notice(notice: &focusflow_core::stats::RestNotice) {
    let secs = notice.shown_seconds.max(0);
    // 先挂截止再挂内容：反过来的话中间那一拍读者会拿到"旧内容 + 新截止"，
    // 念的是上一条提醒却说不出它早该消失。
    REST_NOTICE_UNTIL.store(
        chrono::Local::now().timestamp().saturating_add(secs),
        Ordering::SeqCst,
    );
    *REST_NOTICE.lock().unwrap_or_else(|e| e.into_inner()) = Some(notice.clone());
}

/// 托盘要念的那一条：过期或没有都交回 `None`（不许念旧账）。
pub(crate) fn rest_notice_now() -> Option<focusflow_core::stats::RestNotice> {
    let until = REST_NOTICE_UNTIL.load(Ordering::SeqCst);
    if !rest_is_fresh(chrono::Local::now().timestamp(), until) {
        return None;
    }
    REST_NOTICE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

#[cfg(test)]
mod compute_charts_tests {
    use super::{
        alltime_anchor_today, alltime_total_now, apply_display_total, clamp_floating_into_screen,
        compute_charts, idle_refresh_needed, sort_rank_desc, total_for_display, ChartAgg,
        FloatingBounds, FLOATING_MIN_VISIBLE, RANK_LIMIT,
    };

    /// 他这台机器的形状：1920x1080 单屏（任务栏 48px，故工作区底边 1032）、卡片 90x46。
    fn primary() -> FloatingBounds {
        FloatingBounds {
            left: 0.0,
            top: 0.0,
            screen_w: 1920.0,
            screen_h: 1080.0,
            win_w: 90.0,
            win_h: 46.0,
        }
    }

    /// 他实测的那个症状：卡片故意放在屏幕底边（压在任务栏上，看得见），
    /// 旧判据拿**工作区**夹它 → 每次启动都被拽到一个确定值，看起来像"位置被固定了"。
    #[test]
    fn floating_position_is_left_alone_when_the_card_is_still_visible() {
        // 1920x1080 的屏（工作区底边 1032）、卡片 90x46、存的 pos = (82, 1031)
        let (x, y) = clamp_floating_into_screen(82.0, 1031.0, primary());
        assert_eq!(
            (x, y),
            (82.0, 1031.0),
            "底边那格卡片还露出 49px，看得见 → 一个字都不该改（旧判据在这里夹到 992）"
        );

        // 临界：正好露出 20px 不动，再往外 1px 才算看不见
        let ok = clamp_floating_into_screen(82.0, 1060.0, primary());
        assert_eq!(ok.1, 1060.0, "恰好露 20px 是允许的");
        let over = clamp_floating_into_screen(82.0, 1061.0, primary());
        assert_eq!(over.1, 1060.0, "多 1px 就往回挪最小的一步");
    }

    /// 原来那一刀要防的故障仍然得防住：拔了副屏之后坐标落在屏幕外，
    /// 勾着悬浮窗却看不见，界面上没有任何入口能召回。
    #[test]
    fn offscreen_floating_position_gets_pulled_back_to_visible() {
        // 原来放在右边那块副屏上（x=2100），副屏拔掉后只剩 1920 宽
        let (x, _) = clamp_floating_into_screen(2100.0, 300.0, primary());
        assert_eq!(x, 1920.0 - FLOATING_MIN_VISIBLE, "至少得露出 20px");
        // 左边界外与上边界外
        let (lx, _) = clamp_floating_into_screen(-500.0, 300.0, primary());
        assert_eq!(lx, -90.0 + FLOATING_MIN_VISIBLE);
        let (_, ty) = clamp_floating_into_screen(82.0, -500.0, primary());
        assert_eq!(ty, -46.0 + FLOATING_MIN_VISIBLE);
        // 副屏在左边（负偏移屏幕）时，落在该屏内的位置不能被判成越界
        let left_monitor = FloatingBounds {
            left: -1920.0,
            ..primary()
        };
        let (nx, ny) = clamp_floating_into_screen(-1900.0, 100.0, left_monitor);
        assert_eq!(
            (nx, ny),
            (-1900.0, 100.0),
            "负偏移屏幕上放在副屏的窗口不该被拽走"
        );
        // 屏幕比卡片还小的退化形状：不 panic，夹成一个确定值
        let tiny = FloatingBounds {
            screen_w: 60.0,
            screen_h: 50.0,
            ..primary()
        };
        let (sx, sy) = clamp_floating_into_screen(500.0, 500.0, tiny);
        assert!(sx.is_finite() && sy.is_finite());
    }

    /// 「总计」页的卡片必须和「今日」页一样实时更新。
    ///
    /// 回归：前端 `applyTotal` 只在 `period == -1` 时用带今日增量的 `alltime_total`，
    /// 其余周期用 `period_total` = `agg.total`（上次重算的库值，而 period=0 的重算
    /// 要等每 500 次按键才发生）—— 于是**标签写着「总计」的那一页**数字不动，
    /// 切到「今日」却逐键跳。作者原话：「选今日时今日活跃和总计都快速变动，
    /// 选了其它周期总计就不变了」。
    #[test]
    fn total_tab_card_uses_the_live_total_not_the_last_recompute() {
        // period=0 且基准已建立 → 用"基准 + 今日增量"的修正值（每 tick 现算的那个）
        assert_eq!(
            total_for_display(0, 1_000_000, 1_000_300, true, 0),
            1_000_300
        );
        // 基准还没建立（首轮重聚合前 alltime_total_now 回 0）→ 保住库值，别清零
        assert_eq!(total_for_display(0, 1_000_000, 0, false, 77), 1_000_000);
        // 「总计」页即便有 pending 也不加第二遍：alltime_total 里的今日增量已含未落库部分
        assert_eq!(
            total_for_display(0, 1_000_000, 1_000_300, true, 300),
            1_000_300
        );
        // 「今日」页前端根本不读这个字段
        assert_eq!(total_for_display(-1, 500, 1_000_300, true, 300), 500);
        // 近 N 天：库里的窗口和只到上次落库为止，得把今日未落库那份补上
        assert_eq!(total_for_display(30, 4242, 1_000_300, true, 17), 4259);
        assert_eq!(
            total_for_display(7, 900, 1_000_300, true, 0),
            900,
            "没有 pending 时原样"
        );
    }

    /// 图表推送、live 推送与 `get_live`/`get_charts` 命令得给同一个口径：
    /// 近 N 天的展示值 = 纯库窗口和 + 今日未落库增量，而基准 `total` 不被改写
    /// （统计线程每轮都在它上面加一次 pending）。
    ///
    /// 回归：图表推送那段为了不把 pending 计两遍只能恒传 0（`agg.total` 一个字段两用），
    /// 于是每来一次图表推送，「周期总数」卡片就掉回纯库值 —— 停手时最后一推正是它，
    /// 数字偏低一整条 pending，而同一条推送里的「日均」却是补过 pending 的。
    #[test]
    fn charts_push_carries_the_same_pending_corrected_total_as_the_live_push() {
        let mut agg = ChartAgg {
            total: 900,
            ..Default::default()
        };
        apply_display_total(7, 1_000_300, true, 37, &mut agg);
        assert_eq!(agg.period_total, 937, "展示值要补上今日未落库的那 37");
        assert_eq!(agg.total, 900, "基准必须仍是纯库值");
        assert_eq!(agg.alltime_total, 1_000_300);

        // 下一轮拿快照里的基准再算一次：不能变成 974（双重计数的判据）
        let mut next = agg.clone();
        apply_display_total(7, 1_000_300, true, 37, &mut next);
        assert_eq!(
            (next.total, next.period_total),
            (900, 937),
            "每轮幂等，展示值不随轮数累积"
        );

        // 与「日均」一致的那一侧：pending=0 时展示值就是库值
        let mut quiet = ChartAgg {
            total: 900,
            ..Default::default()
        };
        apply_display_total(7, 1_000_300, true, 0, &mut quiet);
        assert_eq!(quiet.period_total, 900);

        // 总计周期走「基准 + 今日增量」那个量，今日增量里已含未落库部分，不加第二遍
        let mut all = ChartAgg {
            total: 1_000_000,
            ..Default::default()
        };
        apply_display_total(0, 1_000_300, true, 300, &mut all);
        assert_eq!((all.total, all.period_total), (1_000_000, 1_000_300));
    }

    /// 同分必须有确定的先后：否则"谁进前 100 名"随进程重启而变。
    #[test]
    fn rank_ties_break_by_name_so_the_truncation_is_stable() {
        // 三个并列 2 次的键，正好卡在 RANK_LIMIT=100 的切割线上
        let mut items: Vec<(String, i64)> = (0..99)
            .map(|i| (format!("k{i:03}"), (100 - i) as i64))
            .collect();
        for name in ["zz", "aa", "mm"] {
            items.push((name.to_string(), 2));
        }
        // 打乱输入序（模拟 HashMap 的随机迭代序），结果必须每次一样
        let mut a = items.clone();
        let mut b = items.clone();
        b.reverse();
        sort_rank_desc(&mut a);
        sort_rank_desc(&mut b);
        a.truncate(100);
        b.truncate(100);
        assert_eq!(a, b, "输入序不同不该改变截断后的榜单");
        // 99 条的计数从 100 递减到 2，所以第 97 位是 (k097, 3)，
        // 并列 2 次的那一组 [aa, k098, mm, zz] 只有前两条活到截断之后。
        assert_eq!(a.len(), 100);
        assert_eq!(
            a[97..],
            [
                ("k097".to_string(), 3),
                ("aa".to_string(), 2),
                ("k098".to_string(), 2),
            ],
            "并列的次数该按名字升序取舍，而不是随 HashMap 迭代序"
        );
        assert!(
            !a.iter().any(|(n, _)| n == "mm" || n == "zz"),
            "被切掉的该是名字排在后面的那两只"
        );
        assert_eq!(a[0], ("k000".to_string(), 100), "次数降序是第一位的");
    }

    #[test]
    fn empty_db_returns_zeroed_agg() {
        // app_dir 是进程级全局：与周报用例（会往自己目录里种年度库）并行跑时，
        // 不持锁就会读到对方的数据（表现为「空库应为 0」拿到 42 万）。
        let _serial = crate::app_dir_lock();
        // 用隔离的临时程序目录，避免读到开发库（目录由返回值的 Drop 回收）
        let _app = focusflow_core::paths::test_app_dir("compute_charts");
        let agg = compute_charts(0, None, 0);
        assert_eq!(agg.total, 0);
        assert_eq!(agg.alltime_total, 0, "基准未构建时总计应为 0");
        assert_eq!(agg.rank.len(), 0);
        assert_eq!(agg.mouse_total, 0);
        assert_eq!(agg.keyboard_total, 0);
        // 星期分布必须稠密且按 0..=6 有序：前端按下标对位「周一…周日」标签，
        // 窗口一旦短于 7 天（见 compute_charts 里 needed 的下限）标签就会错位。
        assert_eq!(
            agg.weekday,
            (0..7).map(|d| (d, 0)).collect::<Vec<(i64, i64)>>(),
            "星期分布应为 0..=6 七个槽位且按下标有序"
        );
        focusflow_core::paths::set_app_dir(focusflow_core::paths::test_scratch_app_dir());
    }

    /// 「日均」必须把今天未落库的增量补进分子（§七 记档的"日均不跟 pending 走"）：
    /// 窗口以今天收尾，与「总计」「近 N 天」卡片走 total_for_display 的口径一致。
    #[test]
    fn daily_avg_includes_today_pending() {
        use chrono::Datelike;
        let _serial = crate::app_dir_lock();
        let _app = focusflow_core::paths::test_app_dir("avg_pending");
        let today = chrono::Local::now().date_naive();
        let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        let dk_today = (today - epoch).num_days();
        let dk_yesterday = dk_today - 1;
        {
            let path = focusflow_core::paths::year_db_path(today.year());
            let conn = focusflow_core::db::connection::open_rw(&path).unwrap();
            focusflow_core::db::connection::ensure_schema(&conn, today.year()).unwrap();
            for (dk, n) in [(dk_today, 100), (dk_yesterday, 50)] {
                conn.execute(
                    &format!(
                        "INSERT INTO daily_counts (date_key, count, seconds) VALUES ({dk}, {n}, 0)"
                    ),
                    [],
                )
                .unwrap();
            }
        }
        focusflow_core::db::invalidate_years_cache();

        // 今日周期：窗口 = [今天落库 100]，补上未落库 37 → 日均 137
        let agg = compute_charts(-1, None, 37);
        assert_eq!(agg.avg, 137, "今日日均 = 落库 + 未落库");
        // 30 天窗口：分母 30（零填充稠密），分子 = 落库 150 + 未落库 37
        let agg = compute_charts(0, None, 37);
        assert_eq!(agg.avg, (150 + 37) / 30, "30 天日均的分子含未落库增量");

        focusflow_core::paths::set_app_dir(focusflow_core::paths::test_scratch_app_dir());
    }

    /// 「设备数」不能用截断后的 `devices.len()`：列表只留前 RANK_LIMIT 台，
    /// 101 台时界面会把设备数说成 100。台数必须与次数分母一样在后端截断前取。
    #[test]
    fn device_count_survives_the_rank_limit_truncation() {
        use chrono::Datelike;
        let _serial = crate::app_dir_lock();
        let _app = focusflow_core::paths::test_app_dir("device_count");
        let today = chrono::Local::now().date_naive();
        let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        let dk = (today - epoch).num_days();
        {
            let path = focusflow_core::paths::year_db_path(today.year());
            let conn = focusflow_core::db::connection::open_rw(&path).unwrap();
            focusflow_core::db::connection::ensure_schema(&conn, today.year()).unwrap();
            for i in 0..(RANK_LIMIT + 1) {
                // 每台换一个 PID 才算另一台设备（只换实例后缀会被 B14-2 按硬件身份合并）
                let key = format!(
                    "HID#VID_{:04X}&PID_{:04X}&MI_00#inst",
                    0x1000 + i,
                    0x2000 + i
                );
                conn.execute(
                    "INSERT INTO devices (device_key, name, kind) VALUES (?1, ?2, 'mouse')",
                    [key.as_str(), &format!("HID 鼠标 {i}")],
                )
                .unwrap();
                let id = focusflow_core::db::connection::device_id_of(&conn, &key)
                    .expect("设备刚登记就该有 id");
                conn.execute(
                    "INSERT INTO device_counts (date_key, device_id, count) VALUES (?1, ?2, 1)",
                    [dk, id],
                )
                .unwrap();
            }
        }
        focusflow_core::db::invalidate_years_cache();

        let agg = compute_charts(0, None, 0);
        assert_eq!(agg.devices.len(), RANK_LIMIT, "列表本身按上限截断");
        assert_eq!(
            agg.device_count,
            (RANK_LIMIT + 1) as i64,
            "设备台数必须是截断前的总数，否则第 101 台会被界面说成不存在"
        );
        assert_eq!(
            agg.device_total,
            (RANK_LIMIT + 1) as i64,
            "次数分母同样未被截断"
        );

        focusflow_core::paths::set_app_dir(focusflow_core::paths::test_scratch_app_dir());
    }

    /// 总计增量修正：基准未构建时为 0，之后按今日新增累加，
    /// 今日计数回退（删数据/跨天重置）时不得减出负数。
    #[test]
    fn alltime_total_tracks_today_delta_without_going_negative() {
        assert_eq!(alltime_total_now(-1, 0, 0), 0, "基准未构建：显示 0");
        assert_eq!(alltime_total_now(1000, 100, 100), 1000, "无新增：等于基准");
        assert_eq!(alltime_total_now(1000, 100, 130), 1030, "新增 30 次即计入");
        assert_eq!(alltime_total_now(1000, 100, 0), 1000, "计数回退不减基准");
    }

    /// 空闲重聚合的判据也要看"库在上一轮聚合之后落过库没有"。
    #[test]
    fn idle_refresh_notices_a_flush_after_the_last_aggregation() {
        assert!(!idle_refresh_needed(100, 100, 5, 5), "什么都没变：别重算");
        assert!(idle_refresh_needed(101, 100, 5, 5), "今日计数变了要重算");
        assert!(
            idle_refresh_needed(100, 100, 6, 5),
            "库刚落过一批、pending 已清零：不重算就会少掉这批且一直不补"
        );
        // 接线腿：判据要被循环真的用上，且当拍就要重聚合（不是只把空闲周期改短）。
        // 只取测试模块之前的生产半边，否则下面的断言字符串会匹配到自己；比较前先去空白，
        // 因为 rustfmt 会把长调用拆行。
        let src = include_str!("state.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let flat: String = prod.split_whitespace().collect();
        assert!(
            flat.contains(
                "idle_refresh_needed(cur_today,last_heavy_today,flush_seq_now,charts_seq"
            ),
            "空闲间隔没接这条判据"
        );
        assert!(
            flat.contains("main_visible&&flush_seq_now!=charts_seq"),
            "do_heavy 没把刚落库的一批算进去（且要限定在可见时），推送会当场少一批"
        );
    }

    /// 重建「总计」基准的那一次，显示值既不许少掉还没落库的增量，也不许往回跳。
    ///
    /// 锚用成此刻的今日计数时：重建后 `base` 只到上次落库、锚又等于此刻的 cur，
    /// 于是 `cur - 锚 = 0`，卡片当场退回「未落库」那么多 —— 而重建前这些键已经作为
    /// 增量显示过一次了。表现就是**用户正在打字，「总计」却每 500 次按键缩水一次**。
    #[test]
    fn rebuilding_the_alltime_base_never_shrinks_the_displayed_total() {
        // 第一轮：库里今日 970（base 含它），内存里今日 1000，其中 30 次还没落库
        let anchor1 = alltime_anchor_today(1000, 30);
        let shown1 = alltime_total_now(970, anchor1, 1000);
        // 又打 500 次，其间落库 480（含上次那 30）⇒ 库里今日 1450、内存里 1500、未落库 50
        let shown_before_rebuild = alltime_total_now(970, anchor1, 1500);
        let shown2 = alltime_total_now(1450, alltime_anchor_today(1500, 50), 1500);
        // 三条腿攒齐了一次断：头一条先 panic 的话，后面两条腿有没有牙齿就量不出来
        let mut bad: Vec<String> = Vec::new();
        if anchor1 != 970 {
            bad.push(format!("锚该落在已落库的那份今日上（970），实际 {anchor1}"));
        }
        if shown1 != 1000 {
            bad.push(format!(
                "第一次重建当场显示 {shown1}，真值 1000：少的正是还没落库的那 30 次"
            ));
        }
        if shown2 != 1500 || shown2 < shown_before_rebuild {
            bad.push(format!(
                "第二次重建把「总计」从 {shown_before_rebuild} 变成 {shown2}（真值 1500）：\
                 用户还在打字，总数却不再等于真值、甚至往回跳"
            ));
        }
        assert!(bad.is_empty(), "{bad:?}");
    }

    /// 周期 → 天数的尺子：三张图与「日均」卡片问的是同一个问题，必须只有一个答案。
    #[test]
    fn chart_window_days_matches_the_cards_on_the_same_page() {
        use focusflow_core::db::queries::chart_window_days;
        assert_eq!(chart_window_days(-1), 1, "今日就是一天");
        assert_eq!(
            chart_window_days(0),
            30,
            "「总计」按近 30 天算 —— 卡片那句「日均(近30天)」用的是这把尺子，\
             图要横跨全部历史，同一页的均值就对不上"
        );
        assert_eq!(chart_window_days(7), 7);
        assert_eq!(chart_window_days(365), 365);
        assert_eq!(
            chart_window_days(36_600),
            36_600,
            "合法上限原样交出，夹取留给查询层"
        );
        assert_eq!(
            chart_window_days(-2),
            7,
            "说不清的周期退回 7 天，与抽出来之前那个 match 一字不差"
        );
    }

    /// 三张图读的是同一个窗口，而星期分布必须是固定 7 格。
    ///
    /// 后半条是这次改动真正的牙：原先后端交回"只有数据的那几天"，前端按数组下标贴 `WD`
    /// 标签 —— 窗口一短（今日周期只有 1 天）就会把周四的数挂在「周一」那一格上。
    /// 旧写法靠"窗口恒 >=30 天"躲过了这件事，所以这条只能在窗口变短之后才照得出来。
    #[test]
    fn the_three_charts_share_one_window_and_weekday_is_always_seven_slots() {
        use chrono::Datelike;
        let _serial = crate::app_dir_lock();
        let _app = focusflow_core::paths::test_app_dir("charts_window");
        let today = chrono::Local::now().date_naive();
        let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        let dk_today = (today - epoch).num_days();
        {
            let conn = focusflow_core::db::connection::open_rw(
                &focusflow_core::paths::year_db_path(today.year()),
            )
            .unwrap();
            focusflow_core::db::connection::ensure_schema(&conn, today.year()).unwrap();
            conn.execute(
                "INSERT INTO daily_counts (date_key, count, seconds) VALUES (?1, 400, 0)",
                [dk_today],
            )
            .unwrap();
            // 两条小时记录：一条在今天、一条在 40 天前 —— 窗口拉不拉宽，差别就在后者
            for (dk, n) in [(dk_today, 7i64), (dk_today - 40, 3i64)] {
                conn.execute(
                    &format!(
                        "INSERT INTO hourly_counts (date_key, hour, count) VALUES ({dk}, 14, {n})"
                    ),
                    [],
                )
                .unwrap();
            }
        }
        focusflow_core::db::invalidate_years_cache();

        let agg = compute_charts(-1, None, 0);
        assert_eq!(agg.trend_period.len(), 1, "今日周期的趋势窗口就是一天");
        assert_eq!(
            agg.weekday.len(),
            7,
            "星期分布必须铺满 7 格，前端才贴得对标签"
        );
        let today_slot = today.weekday().num_days_from_monday() as usize;
        assert!(
            agg.weekday[today_slot].1 == 400
                && agg
                    .weekday
                    .iter()
                    .enumerate()
                    .all(|(i, (_, v))| if i == today_slot { *v == 400 } else { *v == 0 }),
            "只有今天那一格有数，实得 {:?}",
            agg.weekday
        );
        assert_eq!(agg.hourly[14], 7, "今日窗口只数今天那笔小时记录");

        // 近 60 天：40 天前那笔才进账（这条防的是"每小时永远只数今天"）
        let wide = compute_charts(60, None, 0);
        assert_eq!(wide.trend_period.len(), 60, "趋势窗口跟着周期到 60 天");
        assert_eq!(
            wide.hourly[14], 10,
            "近 60 天要含上 40 天前那笔，实得 {}",
            wide.hourly[14]
        );

        // 「总计」这一档按近 30 天：与卡片那句「日均(近30天)」同一把尺子，不是全部历史
        let all = compute_charts(0, None, 0);
        assert_eq!(all.trend_period.len(), 30, "总计的趋势窗口是近 30 天");
        assert_eq!(all.hourly[14], 7, "近 30 天里不含 40 天前那笔");
    }

    /// 接线判据：这三处问的是刚立的那把尺子，不许有人把某一处又写回固定窗口。
    #[test]
    fn the_charts_ask_the_one_window_ruler() {
        let src = include_str!("state.rs");
        let at = src
            .find("fn compute_charts(")
            .expect("compute_charts 找不到了");
        let body = &src[at..at + src[at..].find("\nfn ").expect("后面没有顶层函数了")];
        assert!(
            body.contains("chart_window_days(period_val)"),
            "周期到天数那把尺子没被用上，说明有人本地又抄了一份 match"
        );
        assert!(
            body.contains("hourly_window_totals(daily_days, None)"),
            "每小时分布还在问某一天，而不是所选窗口"
        );
        assert!(
            body.contains("daily_counts_totals(needed, None)"),
            "每日趋势回到了说不出原因的数组口子（那一栏的 gap 会整块消失）"
        );
        assert!(
            !body.contains("get_hourly_totals(") && !body.contains("get_daily_counts("),
            "面板又用回二元组那两条：数组交不回「这一轮没读全」"
        );
        assert!(
            !body.contains("get_hourly_stats(None)"),
            "写死今日那一问回到了分析页（周报用的那一问不许动）"
        );
        assert!(
            body.contains("aggregate_weekday(&trend_period)"),
            "星期分布还钉在 tail30 上"
        );
    }

    /// 面板拿得到"这一轮没读全"：库读不出来时 `read_gaps` 非空，
    /// 而不是把一份下界念成"今日 0 次、暂无数据"。
    #[test]
    fn the_panel_is_told_which_readings_are_incomplete() {
        let _lock = crate::app_dir_lock();
        let _tmp = focusflow_core::paths::test_app_dir("panel_read_gaps");
        std::fs::create_dir_all(focusflow_core::paths::data_dir()).unwrap();
        let year = {
            use chrono::Datelike;
            chrono::Local::now().year()
        };
        std::fs::write(
            focusflow_core::paths::year_db_path(year),
            b"this is not a sqlite database at all",
        )
        .unwrap();

        let agg = compute_charts(0, None, 0);
        assert_eq!(agg.total, 0, "读不出来时交回的总数是下界，这里就是 0");
        assert!(
            agg.read_gaps.len() >= 5,
            "三个排行榜加两条时间切面各该说一条没读全，实得 {:?}",
            agg.read_gaps
        );
        for domain in focusflow_core::db::queries::PANEL_READ_DOMAINS {
            assert!(
                agg.read_gaps.iter().any(|g| g.contains(domain)),
                "{domain} 那一栏的 gap 没进面板，实得 {:?}",
                agg.read_gaps
            );
        }
    }

    /// 域名清单三处同源：core 那份 `READ_DOMAINS` 是唯一出处，面板与周报都按它逐条比。
    ///
    /// 三个方向都查 —— 少一个方向就留着一个"加一栏只改一处"的口子：
    /// 一、core 生产半边每个域名只许出现一次（就是它那行 `pub const DOMAIN_*` 的定义）。
    ///    多出来的一次就是有人在清单之外又抄了一份字面量，改一处漏一处正是这一族的病根；
    /// 二、定义了 `pub const DOMAIN_x` 就必须登记进 `READ_DOMAINS`，清单上每条也要真有定义；
    /// 三、面板按 `gapNote(s, "域名")` 点的名字必须在清单上，清单上面板该接的每一栏也不能缺；
    ///    面板没有的那条（当日小时分布）必须由周报按常量名认领。
    ///
    /// 探针（都实测过红）：把 `gather_device_totals` 的 gap 换回硬写「设备统计」⇒ 第一方向红；
    /// 新加一条 `pub const DOMAIN_WEEK = "星期分布"` 而不登记 ⇒ 第二方向红；
    /// 把 views.js 里 `gapNote(s, "每日趋势")` 的一处换成清单外的词 ⇒ 第三方向红，删掉一处 ⇒ 也红。
    #[test]
    fn the_domain_list_is_the_single_source_for_every_side() {
        use focusflow_core::db::queries::{PANEL_READ_DOMAINS, READ_DOMAINS};
        let js = include_str!("../ui/js/views.js");
        let weekly = include_str!("export.rs");
        let core_src = include_str!("../../crates/core/src/db/queries.rs");
        // 生产半边，并把整行注释摘掉：这一段自己的说明文字里就写着这些域名，
        // 带着注释判"只许出现一次"会红在文档上、绿在真正的第二份字面量之上。
        let prod: String = core_src[..core_src
            .find("#[cfg(test)]")
            .expect("core 测试模块的起点找不到了")]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        // core 里定义了哪些 `pub const DOMAIN_x = "…"`：名字与串都要拿到，
        // 后者供"清单"比对，前者供周报那一侧按名认领。
        let mut defined: Vec<(String, String)> = Vec::new();
        for line in prod.lines() {
            let Some(rest) = line.trim().strip_prefix("pub const DOMAIN_") else {
                continue;
            };
            let name = format!("DOMAIN_{}", rest.split(':').next().unwrap_or("").trim());
            let Some(value) = line.split('"').nth(1) else {
                panic!("常量 {name} 的定义里读不出一串字面量: {}", line.trim());
            };
            defined.push((name, value.to_string()));
        }
        assert!(
            !defined.is_empty(),
            "core 里一个 `pub const DOMAIN_*` 都没扫到 —— 域名清单搬家了，这条判据的锚点也要跟着搬"
        );

        // 二、定义与清单互为充要：加一条常量不登记 ⇒ 红；清单凭空列一条 ⇒ 也红
        for (name, value) in &defined {
            assert!(
                READ_DOMAINS.contains(&value.as_str()),
                "{name}＝「{value}」没登记进 `READ_DOMAINS`：加一栏要连着清单一起加"
            );
        }
        for domain in READ_DOMAINS.iter().copied() {
            assert!(
                defined.iter().any(|(_, v)| v.as_str() == domain),
                "清单上的「{domain}」没有对应的 `pub const DOMAIN_*` 定义"
            );
        }

        // 一、core 生产半边不许在清单之外再抄一份字面量
        for domain in READ_DOMAINS.iter().copied() {
            let hits = prod.matches(domain).count();
            assert_eq!(
                hits, 1,
                "「{domain}」在 core 生产代码里出现 {hits} 次：清单之外又抄了一份，\
                 改一处漏一处就长这样"
            );
        }

        // 三、面板按什么名过滤 gap，必须是清单上的名
        let mut panel_named: Vec<String> = Vec::new();
        let mut rest = js;
        while let Some(at) = rest.find("gapNote(s, \"") {
            let tail = &rest[at + "gapNote(s, \"".len()..];
            let Some(end) = tail.find('"') else { break };
            panel_named.push(tail[..end].to_string());
            rest = &tail[end..];
        }
        assert!(
            !panel_named.is_empty(),
            "面板一处 gapNote 都没扫到 —— views.js 的调用形状变了，这条判据要先搬锚点"
        );
        for name in &panel_named {
            assert!(
                PANEL_READ_DOMAINS.contains(&name.as_str()),
                "面板按「{name}」过滤 gap，可它不在面板该接的域名清单上（core 交不回这种句子）"
            );
        }
        for domain in PANEL_READ_DOMAINS.iter().copied() {
            assert!(
                READ_DOMAINS.contains(&domain),
                "面板清单上的「{domain}」不在总清单上"
            );
            assert!(
                panel_named.iter().any(|n| n.as_str() == domain),
                "面板没有接「{domain}」的那一栏了：`gapNote(s, \"{domain}\")` 一处都找不到"
            );
        }

        // 面板不接的那几条，必须由周报按常量名认领 —— 否则这一栏加进清单却没人显示它
        for (name, value) in &defined {
            if PANEL_READ_DOMAINS.contains(&value.as_str()) {
                continue;
            }
            assert!(
                weekly.contains(name.as_str()),
                "域名「{value}」面板没有对应栏位，周报也没按 `{name}` 认领它：\
                 加一栏得有人在界面上说得出这句话"
            );
        }
    }
}

#[cfg(test)]
mod restart_gate_tests {
    use super::{restart_decision, RestartDecision, MAX_RESTART_DEPTH};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// 到深度上限的那一次**不占名额**。
    ///
    /// 回归：原来先 `RESTART_SCHEDULED.swap(true)` 再判深度，于是"深度到顶、压根
    /// 不会拉起新进程"的自动重试把本进程唯一的重启名额白吃了；之后用户主动换
    /// 数据目录那一跳被同一个名额以"本进程已经安排过一次重启"顶回去 —— 配置已写、
    /// 进程不重启，新目录要等下次自然启动才生效，而拒绝理由指向的是一次从未发生的安排。
    #[test]
    fn a_depth_refused_attempt_leaves_the_one_restart_slot_unused() {
        let slot = AtomicBool::new(false);
        let refused = restart_decision(&slot, false, MAX_RESTART_DEPTH);
        assert!(
            matches!(refused, RestartDecision::DepthExhausted { depth }
                     if depth == MAX_RESTART_DEPTH),
            "深度到顶该判成 DepthExhausted，实际 {refused:?}"
        );
        assert!(
            !slot.load(Ordering::SeqCst),
            "这一次不会拉起任何进程，不许占住本进程唯一的重启名额"
        );
        // 名额还在 ⇒ 用户主动那一跳照样安排得上，并把深度归零（新起点）
        let user = restart_decision(&slot, true, MAX_RESTART_DEPTH);
        assert!(
            matches!(user, RestartDecision::Proceed { next_depth: 0 }),
            "用户主动换目录不该被深度上限拒绝，实际 {user:?}"
        );
    }

    /// 自动重启链：放行的一跳占住名额并把深度 +1 带进新进程，同进程第二次不再安排
    /// （多个恢复线程各拉一个进程会互相撞单实例锁，结果两个进程都没了）。
    #[test]
    fn an_auto_attempt_burns_the_slot_and_increments_depth() {
        let slot = AtomicBool::new(false);
        let first = restart_decision(&slot, false, 0);
        assert!(
            matches!(first, RestartDecision::Proceed { next_depth: 1 }),
            "深度 0 的自动重试应放行并带 next_depth=1，实际 {first:?}"
        );
        assert!(slot.load(Ordering::SeqCst), "真在途的这一次必须占住名额");
        let second = restart_decision(&slot, false, 0);
        assert!(
            matches!(second, RestartDecision::AlreadyScheduled),
            "名额已占，同一进程不再安排第二次，实际 {second:?}"
        );
        let third = restart_decision(&slot, true, 0);
        assert!(
            matches!(third, RestartDecision::AlreadyScheduled),
            "「一个进程只准拉起一个新进程」与触发原因无关，实际 {third:?}"
        );
    }

    /// 深度闸的边界：正好差一跳（`MAX-1`）时放行，且那一跳会把新进程带到上限；
    /// 上限那一跳本身被拒绝。
    #[test]
    fn depth_gate_trips_on_the_last_hop_only() {
        let slot = AtomicBool::new(false);
        let last = restart_decision(&slot, false, MAX_RESTART_DEPTH - 1);
        assert!(
            matches!(last, RestartDecision::Proceed { next_depth }
                     if next_depth == MAX_RESTART_DEPTH),
            "还差一跳时该放行，实际 {last:?}"
        );
        let slot2 = AtomicBool::new(false);
        assert!(
            matches!(
                restart_decision(&slot2, false, MAX_RESTART_DEPTH),
                RestartDecision::DepthExhausted { .. }
            ),
            "到了上限就该停下"
        );
    }

    /// 重启线程**没起来**的那一次也不占名额：名额的含义是「已经有一个线程会去
    /// 拉起新进程」，线程压根没起来就没有任何东西在途。
    ///
    /// 回归：原来 `.is_ok()` 直接把结果当返回值交出去，失败时名额仍留着 ——
    /// 之后用户主动换数据目录那一跳被「本进程已经安排过一次重启」顶回去，
    /// 而那句指向的是一次从未开始的尝试（与上面深度闸那一条是同一条规矩的两半）。
    #[test]
    fn a_failed_restart_thread_gives_the_slot_back() {
        use super::restart_slot_after_spawn;

        // 模拟 restart_decision 已放行（占住名额）之后的两种落点
        let slot = AtomicBool::new(true);
        assert!(
            !restart_slot_after_spawn(&slot, false),
            "线程没起来就该报「没安排上」"
        );
        assert!(
            !slot.load(Ordering::SeqCst),
            "线程没起来时名额必须退回，否则本进程再也重启不了"
        );

        let slot2 = AtomicBool::new(true);
        assert!(
            restart_slot_after_spawn(&slot2, true),
            "线程起来了就该报「安排上了」"
        );
        assert!(
            slot2.load(Ordering::SeqCst),
            "真有一个线程在途时名额要留着（一个进程只准拉起一个新进程）"
        );
    }

    /// 自动周报的四种结局：**做没做成由这一趟的实际结局说，不由线程的生命周期说**。
    ///
    /// 回归的两代病：第一代在 `spawn` 之前就推进锚点、末尾 `.ok()` 咽掉失败 ⇒ 一次资源紧张
    /// 就让那一周永不再试且日志零字；第二代（上一版修完那一半之后）只认"线程起没起来"，
    /// 于是**生成被拒绝**（某一年的库读不动时周报宁可不出半份）照旧推进锚点，只剩一行 warn，
    /// 设置页与托盘都不说。这两种都必须落进"没做成"，而"那一周确实没有记录"不许跟着一起重试。
    #[test]
    fn the_weekly_round_separates_written_from_absent_record_and_two_kinds_of_failure() {
        use super::{weekly_round_settles, WeeklyRound};

        assert!(
            weekly_round_settles(&WeeklyRound::Written("r.md".into())),
            "写出来了就是做完了"
        );
        assert!(
            weekly_round_settles(&WeeklyRound::NoRecord),
            "那一周确实没有记录也是做完了：没有是答案，为它每十分钟重跑十四天聚合只是占机器"
        );
        assert!(
            !weekly_round_settles(&WeeklyRound::Failed("这些年度库打不开".into())),
            "生成被拒绝不许算成做完——那一周的报告还欠着"
        );
        assert!(
            !weekly_round_settles(&WeeklyRound::NotSpawned("资源不足".into())),
            "线程压根没起来同样没做成（这一半是上一版立的，别退回去）"
        );
    }

    /// 没做成时要再试，但不能每 500ms 试一次：再试的间隔由这把尺子答。
    #[test]
    fn the_weekly_retry_waits_instead_of_hammering() {
        use super::{weekly_retry_due, WEEKLY_RETRY_SECS};

        assert!(
            weekly_retry_due(None),
            "本进程还没试过：启动后那一拍就该试（与旧口径一致）"
        );
        assert!(
            !weekly_retry_due(Some(0)),
            "刚起过一趟就每拍再 spawn 一次 = 十四天聚合跑在 500ms 节奏上"
        );
        assert!(
            !weekly_retry_due(Some(WEEKLY_RETRY_SECS - 1)),
            "间隔没到就再试，这条判据等于没立"
        );
        assert!(
            weekly_retry_due(Some(WEEKLY_RETRY_SECS)),
            "到点就该再试：移动盘刚醒/杀软首扫那一类是过得去的"
        );
    }

    /// 同一周的重试只在第一次走 error，之后降到 debug；**但第一次不许省**。
    #[test]
    fn the_weekly_failure_is_said_once_per_week() {
        use super::weekly_speaks_once;

        let (speak, next) = weekly_speaks_once(i64::MIN, 100);
        assert!(speak && next == 100, "这一周第一次失败必须出声");
        let (speak2, next2) = weekly_speaks_once(100, 100);
        assert!(!speak2 && next2 == 100, "同一周第二次不许刷屏");
        let (speak3, next3) = weekly_speaks_once(100, 107);
        assert!(speak3 && next3 == 107, "换了一周就是新的欠账，要再出声一次");
    }

    /// 状态的推进本身：**做完了才动锚点，没做成才挂"欠着"**，四种结局各走各的。
    ///
    /// 这一条钉的是这次要修的那件事：上一版的病不在算式，而在"做成没做成"被
    /// "线程起没起来"替掉 —— 而那两种写法在没有真线程的测试里长得一模一样，
    /// 所以判据必须落在纯函数这一层。
    #[test]
    fn the_weekly_state_moves_only_when_the_round_actually_finished() {
        use super::{weekly_reconcile, WeeklyLog, WeeklyReconcile, WeeklyRound};

        // 还没回话：什么都不动，`error_week` 也不动
        let (rec, err) = weekly_reconcile(None, i64::MIN);
        assert!(
            matches!(rec, WeeklyReconcile::Nothing) && err == i64::MIN,
            "那一趟还在跑时不许推进任何东西，实得 {rec:?}/{err}"
        );

        // 写出来了：推进锚点 + Info 那一句
        let (rec, err) =
            weekly_reconcile(Some((100, WeeklyRound::Written("r.md".into()))), i64::MIN);
        match &rec {
            WeeklyReconcile::Settled { week, line, level } => {
                assert_eq!(*week, 100, "做完了要把这一周的锚点推上去");
                assert_eq!(*level, WeeklyLog::Info, "写出来了走 info");
                assert!(line.contains("r.md"), "路径要说得出，实得 {line}");
            }
            other => panic!("写出来那一支该落 Settled，实得 {other:?}"),
        }
        assert_eq!(err, i64::MIN, "做成那一趟不占用失败记录");

        // 那一周确实没有记录：也算做完，但不许用 info 吓唬人
        let (rec, _) = weekly_reconcile(Some((100, WeeklyRound::NoRecord)), i64::MIN);
        match rec {
            WeeklyReconcile::Settled { level, line, .. } => {
                assert_eq!(level, WeeklyLog::Debug, "没有记录是答案，不是成绩");
                assert!(line.contains("没有记录"), "实得 {line}");
            }
            other => panic!("NoRecord 那一支该落 Settled，实得 {other:?}"),
        }

        // 生成被拒绝：不推进、挂"欠着"、第一次走 error 且把原因带在句子里
        let why = "这一轮没读全（2026 年库打不开）";
        let (rec, err) = weekly_reconcile(Some((100, WeeklyRound::Failed(why.into()))), i64::MIN);
        match &rec {
            WeeklyReconcile::Outstanding { week, line, level } => {
                assert_eq!(*week, 100);
                assert_eq!(*level, WeeklyLog::Error, "第一次必须走得响一点");
                assert!(line.contains(why), "原因要原样在句子里，实得 {line}");
                assert!(line.contains("再试"), "要说这是会重试的，实得 {line}");
            }
            other => panic!("生成被拒绝那一支必须落 Outstanding，实得 {other:?}"),
        }
        assert_eq!(err, 100, "这一周的失败已经记下了");

        // 同一周第二次（十分钟后再试还是失败）：仍然挂著，但降到 debug
        let (rec, err2) = weekly_reconcile(Some((100, WeeklyRound::Failed(why.into()))), 100);
        match rec {
            WeeklyReconcile::Outstanding { level, line, .. } => {
                assert_eq!(level, WeeklyLog::Debug, "同一周不许把日志刷成噪声");
                assert!(line.contains(why), "降档不许把原因一起降掉，实得 {line}");
            }
            other => panic!("还是失败，实得 {other:?}"),
        }
        assert_eq!(err2, 100);

        // 线程压根没起来：同一格（这一周没做成），换到新的一周要再出声一次
        let (rec, err3) =
            weekly_reconcile(Some((107, WeeklyRound::NotSpawned("资源不足".into()))), 100);
        match rec {
            WeeklyReconcile::Outstanding { level, .. } => {
                assert_eq!(level, WeeklyLog::Error, "换了一周就是新的欠账")
            }
            other => panic!("NotSpawned 不许算成做完，实得 {other:?}"),
        }
        assert_eq!(err3, 107);
    }

    /// 接线判据：那一趟的**结果**必须被交回统计线程，三种结局各交一次，一处都不许咽。
    ///
    /// 行为用例钉得住判据本身，钉不住"线程体里到底有没有把结局交回来"——那一支在无头环境里
    /// 要真起线程、真跑十四天聚合才走得到，所以这里按形状钉：`publish_weekly_round` 在
    /// 写出来／没记录／失败／起不来四处都出现过，锚点与那句"欠着"只由 `weekly_reconcile`
    /// 那一问推进，不再看 `spawn` 的 `is_ok()`。
    #[test]
    fn the_weekly_thread_reports_its_outcome_on_every_arm() {
        let src = include_str!("state.rs");
        let at = src.find("fn spawn_stats_worker").expect("统计线程找不到了");
        let body = &src[at..at + src[at..].find("\nfn ").expect("后面没有顶层函数了")];
        // 剪掉整行注释：这一段自己的说明文字里就写着"旧写法只认线程起没起来"，
        // 带着注释判"不许再看 is_ok()"会红在该改的地方之外。
        let code: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for arm in [
            "WeeklyRound::Written(",
            "WeeklyRound::NoRecord,",
            "WeeklyRound::Failed(",
            "WeeklyRound::NotSpawned(",
        ] {
            assert!(
                code.contains(arm),
                "周报那一趟的结局少了「{arm}」这一支 —— 少一支就是一种新的静默"
            );
        }
        assert!(
            code.matches("publish_weekly_round(").count() >= 2,
            "线程体与「起线程就失败」那一支都要把结局交回来，实得 {}",
            code.matches("publish_weekly_round(").count()
        );
        assert!(
            code.contains("weekly_reconcile(take_weekly_round("),
            "锚点的推进必须问「这一趟做成了吗」，不是问「线程起来了吗」"
        );
        assert!(
            !code.contains("result.is_ok()"),
            "又只看 spawn 成没成来推进锚点 —— 生成被拒绝那一支会重新变回静默"
        );
        assert!(
            code.contains("mark_weekly_outstanding(false)")
                && code.contains("mark_weekly_outstanding(true)"),
            "做成要摘掉托盘那句、没做成要挂上 —— 少一边就是念旧账或永远不说"
        );
    }

    /// 接线腿：两处 spawn 的结果都必须被看见，不许 `.ok()` 咽掉。
    #[test]
    fn both_spawn_sites_observe_their_result() {
        let src = include_str!("state.rs");
        let at = src
            .find("fn schedule_app_restart")
            .expect("schedule_app_restart 找不到了");
        let body = &src[at..at + src[at..].find("\nfn ").expect("后面没有顶层函数了")];
        assert!(
            body.contains("restart_slot_after_spawn(&RESTART_SCHEDULED"),
            "重启线程的成败没落到名额判据上"
        );

        let at2 = src.find("fn spawn_stats_worker").expect("统计线程找不到了");
        let body2 = &src[at2..at2 + src[at2..].find("\nfn ").expect("后面没有顶层函数了")];
        assert!(
            body2.contains("if let Err(e) = spawned"),
            "周报线程起不来时要把原因交回去（NotSpawned 那一支），不许 `.ok()` 咽掉"
        );
    }
}

#[cfg(test)]
mod today_display_wiring {
    /// 接线腿：统计线程必须走"按日对一次"的显示口径。
    ///
    /// 纯函数用例盖不到调用点；退回 `w.today_count()` / `today_active_seconds()`
    /// 会让程序跨过午夜、今天还没输入时把昨天一整天的数显示成"今日"。
    #[test]
    fn the_stats_worker_uses_the_day_checked_today_counters() {
        let src = include_str!("state.rs");
        let at = src.find("fn spawn_stats_worker").expect("统计线程找不到了");
        let body = &src[at..at + src[at..].find("\nfn ").expect("统计线程之后没有顶层函数了")];
        assert!(
            body.contains("today_count_for_display()")
                && body.contains("today_active_for_display()"),
            "统计线程没走按日对一次的显示口径（午夜后会把昨天当成今日）"
        );
        assert!(
            !body.contains("w.today_count()") && !body.contains("w.today_active_seconds()"),
            "统计线程里还有直接照抄今日缓存的地方"
        );
    }
}

#[cfg(test)]
mod alltime_anchor_wiring {
    /// 接线腿：重建基准时那把锚必须由 `alltime_anchor_today` 交出来。
    ///
    /// 上面那条纯函数用例钉的是规则本身；调用点写回 `alltime_cache_today = cur_today`
    /// 它照样是绿的 —— 而"总计在重建当场掉一截"只发生在调用点上，所以这里按源码形状
    /// 钉住那一次赋值（比对前先摘注释行，本文件的注释会点名隔壁变量名）。
    #[test]
    fn the_rebuild_anchors_on_the_landed_today_count() {
        let src = include_str!("state.rs");
        let at = src.find("fn spawn_stats_worker").expect("统计线程找不到了");
        let body = &src[at..at + src[at..].find("\nfn ").expect("统计线程之后没有顶层函数了")];
        let code: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let needle = "alltime_cache_today =";
        assert_eq!(
            code.matches(needle).count(),
            1,
            "「总计」基准的赋值处不是恰好一份：{}",
            code.matches(needle).count()
        );
        let at2 = code.find(needle).unwrap() + needle.len();
        let assign = &code[at2..at2 + code[at2..].find(';').expect("这条赋值没有结尾分号")];
        assert!(
            assign.contains("alltime_anchor_today("),
            "锚没走「已落库的今日」那条判据，退回此刻的今日计数会让「总计」在重建当场缩水"
        );
    }

    /// 接线腿：显示锚与「该重建了吗」那把尺子必须是两个变量，不许并成一把。
    ///
    /// 并成锚那一侧的后果不是数字差一点，而是**最坏那天把机器打满**：年度库整天写不进去时
    /// 未落库的增量就是当天全部按键，`今日 - 锚` 恒久越过阈值 ⇒ 每 500ms 一次跨全部年度库的
    /// 全历史扫描。反过来（触发用锚、显示用 stamp）则是卡片当场少一截，两条都要钉。
    #[test]
    fn the_rebuild_trigger_and_the_display_anchor_are_two_separate_rulers() {
        let src = include_str!("state.rs");
        let at = src.find("fn spawn_stats_worker").expect("统计线程找不到了");
        let body = &src[at..at + src[at..].find("\nfn ").expect("统计线程之后没有顶层函数了")];
        let code: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let trig_at = code
            .find("let alltime_dirty =")
            .expect("那条「该重建了吗」的判据不见了");
        let trig_end = code[trig_at..].find(";\n").expect("判据切不出来");
        let trig = &code[trig_at..trig_at + trig_end];
        assert!(
            !trig.contains("alltime_cache_today"),
            "重建触发量用上了显示锚：未落库量一旦超过阈值就会每 tick 重跑一次全历史扫描"
        );
        assert!(
            trig.contains("alltime_stamp_today"),
            "重建触发量必须用「重建那一刻的今日计数」"
        );
        assert_eq!(
            code.matches("alltime_total_now(alltime_total_base, alltime_cache_today, cur_today)")
                .count(),
            2,
            "「总计」展示值的两处调用都得传显示锚（基准那份不算触发尺子）"
        );
    }

    /// 全历史基线只许在「每一年都读到了」的时候更换。
    #[test]
    fn a_partial_alltime_read_keeps_the_previous_baseline() {
        use super::alltime_baseline_adoptable;
        assert!(alltime_baseline_adoptable(true, true), "读全了就换");
        assert!(alltime_baseline_adoptable(true, false), "读全了，首轮也换");
        assert!(
            !alltime_baseline_adoptable(false, true),
            "有年没读出来而手上已有上一份：残缺的那份会把「总计」写低一整天"
        );
        assert!(
            alltime_baseline_adoptable(false, false),
            "首轮且读不全只能先用这份：留 None 会让重建判据每轮都成立，统计线程一直重扫"
        );
    }

    /// 接线腿：换基线那几行必须真的过这把尺子（纯函数对了，调用点无条件覆盖照样是绿的）。
    ///
    /// 比对用"去掉全部空白"的形态：`alltime_total_base >= 0,` 这个形状在本函数里还被
    /// `apply_display_total` 用过两次，按整行 `contains` 会被它们顶班，等于没钉。
    #[test]
    fn the_alltime_baseline_write_is_gated_on_the_read_being_complete() {
        let src = include_str!("state.rs");
        let at = src.find("fn spawn_stats_worker").expect("统计线程找不到了");
        let body = &src[at..at + src[at..].find("\nfn ").expect("统计线程之后没有顶层函数了")];
        let code: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let flat: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        let gate = flat
            .find("ifalltime_baseline_adoptable(totals.unreadable.is_empty(),alltime_total_base>=0")
            .expect("换基线没过这把尺子（或两问被换掉了）");
        let write = flat
            .find("alltime_total_base=totals.total;")
            .expect("全历史基线那次赋值不见了");
        assert!(write > gate, "基线赋值必须排在那把尺子之后");
        assert_eq!(
            flat.matches("alltime_total_base=totals.total;").count(),
            1,
            "全历史基线只许有一处写入，否则第二处绕开了尺子"
        );
    }
}

#[cfg(test)]
mod unload_marker_wiring {
    /// 接线腿：「页面已卸载」这两件事（`MAIN_UNLOADED` 与那句日志）必须排在
    /// `navigate` 的回话之后，而且不许把回话丢掉。
    ///
    /// 卸载与恢复都要真窗口，headless 起不来 ⇒ 这里钉的是顺序与"必须看见 Err"，
    /// 「投递失败时页面确实还留着」那半登记为眼验项，别写成有用例守着。
    #[test]
    fn the_unload_marker_waits_for_the_navigate_reply() {
        let src = include_str!("state.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let at = prod
            .find("fn arm_main_unload(")
            .expect("页面卸载那条路找不到了");
        let rest = &prod[at..];
        let end = rest.find("\nfn ").expect("卸载函数之后没有顶层函数了");
        // 注释要先摘掉：本文件的注释写着旧形状 `let _ = win.navigate(..)`，
        // 拿原文比就会让守卫被自己的说明文字喂绿。
        let code: String = rest[..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        // 三条腿攒齐了一次断：头一条先 panic 就把后面两条腿的敏感度藏起来了
        let mut bad: Vec<String> = Vec::new();
        if code.contains("let _ = win.navigate") {
            bad.push(
                "navigate 的回话被丢掉（let _ = ..）：Err 那支没人看，日志与标记都会说谎"
                    .to_string(),
            );
        }
        match (
            code.find("win.navigate("),
            code.find("MAIN_UNLOADED.store(true"),
        ) {
            (Some(nav), Some(mark)) => {
                if nav >= mark {
                    bad.push(format!(
                        "标记排在导航回话之前（navigate@{nav}、store@{mark}）：投递失败时下一次 show \
                         会把活着的页面白重刷一次"
                    ));
                }
            }
            (nav, mark) => bad.push(format!("两个位置少了一个：navigate={nav:?} store={mark:?}")),
        }
        if !code.contains("没被接受") {
            bad.push("投递失败那一支在日志里查不到手".to_string());
        }
        assert!(bad.is_empty(), "{bad:?}");
    }

    /// 接线腿：恢复路径上那次导航也不许把回话丢掉，被拒绝时要把标记立回去。
    ///
    /// 「标记已摘 + 导航又没成」= 一个再按热键也不会好的空白面板；这条规矩与
    /// 「线程没起来不许推进一次性 marker／名额」是同一个（`weekly_report_marking`、
    /// `restart_slot_after_spawn`）。导航真被拒绝时的行为只能真机按出来 ⇒ 眼验项照旧。
    #[test]
    fn a_refused_restore_navigation_keeps_the_unload_marker() {
        let src = include_str!("state.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let at = prod
            .find("MAIN_UNLOADED.swap(false")
            .expect("那条「发现页面被卸载过」的分支找不到了");
        let rest = &prod[at..];
        let end = rest
            .find("set_webview_rendering(app, \"main\", true)")
            .expect("普通路径的起点找不到了");
        let code: String = rest[..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !code.contains("let _ = win.navigate(u)"),
            "恢复导航的回话被丢掉：页面会停在空白页，而标记已经被摘走"
        );
        assert!(
            code.contains("MAIN_UNLOADED.store(true"),
            "导航被拒绝时要把「已卸载」标记立回去，否则下一次呼出走普通路径、显示的仍是空白"
        );
    }
}

#[cfg(test)]
mod startup_window_wiring {
    /// 接线腿：`start_to_tray = false` 必须在启动时把主窗口建出来并显示。
    ///
    /// 主窗口是懒创建的；原来那一支只处理 true（hide），false 什么都不做 ——
    /// 用户改完配置反而只剩托盘图标，这个设置读起来像没生效。
    #[test]
    fn start_to_tray_false_shows_the_lazily_created_main_window() {
        let src = include_str!("state.rs");
        let at = src
            .find("fn setup_windows")
            .expect("setup_windows 找不到了");
        let body = &src[at..at
            + src[at..]
                .find("\nfn ")
                .expect("setup_windows 之后没有顶层函数了")];
        assert!(
            body.contains("show_main_window_when_ready("),
            "start_to_tray=false 没有主动显示主窗口（懒创建的窗口没人建）"
        );
        assert!(
            body.contains("if state.start_to_tray"),
            "托盘开关的分支不见了（判据要跟着改）"
        );
    }
}

#[cfg(test)]
mod visibility_wiring {
    use super::{just_shown, window_in_front};

    /// 最小化也算「没在眼前」—— 与 `hotkey::hotkey_should_hide` 同一把尺子。
    /// 只看 `is_visible()` 的话，最小化挂着的面板照样每 2 秒跑一次重聚合，
    /// 还被 `stats-live` 事件唤回渲染进程（那一族已经为热键修过一次，这是漏下的另一半）。
    #[test]
    fn a_minimized_window_is_not_in_front() {
        assert!(window_in_front(true, false), "显示且未最小化才算在眼前");
        assert!(
            !window_in_front(true, true),
            "最小化不算在眼前（WS_VISIBLE 还在）"
        );
        assert!(!window_in_front(false, false), "藏起来不算");
        assert!(!window_in_front(false, true), "藏起来且带最小化标记也不算");
    }

    /// 上升沿只在「从没在眼前到在眼前」那一拍为真，之后不复现。
    /// 隐藏期间 `prev_*` 照旧在更新，呼出的窗口就是靠这一拍拿到它缺的那一拍。
    #[test]
    fn the_rising_edge_fires_once_per_show() {
        let mut was = false;
        assert!(just_shown(true, &mut was), "呼出那一拍必须为真");
        assert!(!just_shown(true, &mut was), "一直在眼前时不该重复触发");
        assert!(!just_shown(false, &mut was), "藏起来那一拍不算上升沿");
        assert!(
            just_shown(true, &mut was),
            "再呼出一次还要补推（这期间数字可能已经变了）"
        );
    }

    /// 接线腿：统计线程读可见性时问的是「在眼前」而不是 `is_visible()`，
    /// 且补推与重聚合都认上升沿。纯函数用例盖不到调用点。
    #[test]
    fn the_stats_worker_asks_minimization_and_pushes_on_show() {
        let src = include_str!("state.rs");
        let at = src.find("fn spawn_stats_worker").expect("统计线程找不到了");
        let body = &src[at..at + src[at..].find("\nfn ").expect("统计线程之后没有顶层函数了")];
        assert!(
            body.contains("window_in_front("),
            "主窗口的可见性判据没走「在眼前」那把尺子"
        );
        assert!(
            body.contains("w.is_minimized().unwrap_or(false)"),
            "没有真的问最小化（注释里提一次不算）"
        );
        assert!(
            body.contains("just_shown(main_visible, &mut prev_main_visible)")
                && body.contains("just_shown(floating_visible, &mut prev_floating_visible)"),
            "两个窗口的上升沿没都算出来"
        );
        assert!(
            body.contains("live_changed || main_just_shown || floating_just_shown"),
            "推送条件里没有上升沿（呼出后先显示藏起来那一刻的数）"
        );
        assert!(
            body.contains("|| main_just_shown") && body.contains("let do_heavy"),
            "呼出主窗口没顺带做一次重聚合"
        );
    }

    /// 接线腿：按「应用活跃与否」统一同步两个开关的表里只有主窗口与悬浮窗，
    /// **覆盖层与贴图必须在表外，而且各自的理由要写在表上方** —— 否则下一个人把它当漏项补上，
    /// 补出来的正是这轮想避免的反效果：用户拖着选区时 `main_visible` 恰好是假，
    /// 拿这张表去降档等于对一张正在交互的全屏页面报 LOW。
    #[test]
    fn the_app_active_table_holds_main_and_floating_only() {
        let src = include_str!("state.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let at = prod
            .find("APP_MANAGED_LABELS: [&str; 2] = [")
            .expect("那张表不见了（改名要连判据一起改）");
        let table = &prod[at..at + prod[at..].find("];").expect("表没有收尾") + 2];
        assert!(
            table.contains("\"main\"") && table.contains("\"floating\""),
            "表里少了这两个中的一个：{table}"
        );
        assert!(
            !table.contains("snip"),
            "覆盖层被塞进按应用活跃降档的表里了：它归 `set_window_webview_active` 管"
        );
        assert!(
            !table.contains("pin"),
            "贴图被塞进来了：可见窗口对它报 LOW 是错误信号"
        );
        // 理由必须落在紧邻这张表上方那一段文档注释里（整文件 contains 会让别处的同词顶班）。
        // 剪到**行首**为止：`at` 指向的是常量名，那一行前面还挂着 `pub(crate) const `，
        // 直接从 at 往回切 lines() 会把这半行当成"第一行"，take_while 当场停下，
        // 于是注释块永远是空的、这条判据红得让人想删掉它。
        let before = &prod[..at];
        let head = &before[..before.rfind('\n').unwrap_or(0)];
        let doc: Vec<&str> = head
            .lines()
            .rev()
            .take_while(|l| l.trim_start().starts_with("///"))
            .collect();
        let doc: String = doc.into_iter().rev().collect::<Vec<_>>().join("\n");
        assert!(
            doc.contains("覆盖层") && doc.contains("贴图"),
            "表上方没写明这两类窗口为什么不在这里"
        );
    }

    /// 接线腿：按住/抬起一个窗口时，**两个开关都要设**，且档位是渲染的反向。
    /// 少设一个就是白做一半 —— 主窗口那次实测的 −62.9 MB 工作集是两个开关共同买到的。
    #[test]
    fn the_park_helper_sets_both_switches_in_the_same_move() {
        let src = include_str!("state.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let at = prod
            .find("fn set_window_webview_active")
            .expect("那个助手不见了");
        let body = &prod[at..at + prod[at..].find("\n}").expect("助手没有收尾")];
        let flat: String = body
            .lines()
            .map(|l| l.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        assert!(
            flat.contains("set_webview_rendering(app,label,active)"),
            "没设渲染（停渲染是省内存的那一半）：{flat}"
        );
        assert!(
            flat.contains("set_webview_memory_level(app,label,!active)"),
            "没设内存档位，或者方向写反了（应当与渲染反向）：{flat}"
        );
    }
}

#[cfg(test)]
mod rest_delivery_tests {
    use super::rest_is_fresh;

    /// 到点就不该再念：`until = 0` 是"从来没有过"，`now >= until` 是"说过就完了"。
    #[test]
    fn a_rest_notice_stops_being_fresh_at_its_deadline() {
        assert!(!rest_is_fresh(100, 0), "一条都没有时不许编出一句提醒");
        assert!(rest_is_fresh(99, 100), "还在有效期内就该说得出");
        assert!(!rest_is_fresh(100, 100), "到期的那一秒起就不再是眼前的话");
        assert!(!rest_is_fresh(101, 100), "过期之后一律不再念旧账");
    }

    /// 接线判据：提醒要**先**挂进托盘读得到的那一份，再走到"窗口在不在眼前"那两支。
    ///
    /// 这条的病症是无声的：面板没开、悬浮窗关掉或藏着时，旧写法只剩日志里一行
    /// 「建议休息」。牙不在屏幕上（headless 起不来窗口），在**顺序**上 ——
    /// 把挂载那一步挪进 `if main_visible` 里面，屏幕上一切照旧、等于没修。
    #[test]
    fn the_notice_is_parked_before_the_visibility_branches() {
        let src = include_str!("state.rs");
        let at = src.find("fn spawn_stats_worker").expect("统计线程找不到了");
        let body = &src[at..at + src[at..].find("\nfn ").expect("统计线程后面没有顶层函数了")];
        // 判据要落在**提醒这一块之内**：整段线程体里 `if main_visible` 出现过好几处，
        // 光比"publish 排在 emit 前面"的话，把挂载挪进 `if main_visible {` 里面照样满足
        // —— 探针实测过：那样这条守卫仍然绿，而那时两扇窗都不在眼前，提醒又只剩一行日志。
        let block_at = body
            .find("if let Some(notice) = rest_monitor.observe(")
            .expect("久坐提醒那一块找不到了");
        let block = &body[block_at..];
        let parked = block
            .find("publish_rest_notice(&notice)")
            .expect("提醒没有挂到托盘读得到的那一份");
        let gated = block.find("if main_visible").expect("可见性那一问不见了");
        assert!(
            parked < gated,
            "挂载落进了可见性分支里面：面板与悬浮窗都不在眼前时，这条提醒只剩一行日志"
        );
        assert_eq!(
            block.match_indices("publish_rest_notice(&notice)").count(),
            1,
            "挂载只能有一处：多出来的那些等于把它塞进了某个分支里补做"
        );
        for label in ["main", "floating"] {
            let needle = format!(r#"emit_to("{label}", "rest-reminder""#);
            let sent = block.find(&needle).expect("久坐提醒的定向推送不见了");
            assert!(parked < sent, "必须先挂托盘、再判{label}在不在眼前");
        }
        // "没人收"不许折成"发成功了"
        assert!(
            !body.contains(r#"let _ = app.emit_to("main", "rest-reminder""#),
            "主窗口那一支又把失败咽回去了"
        );
        assert!(body.contains("久坐提醒没能送到主窗口"), "送到失败要点名");
        assert!(body.contains("久坐提醒没能送到悬浮窗"), "两支各说各的");
    }

    /// 可见时长的算式只许在 core 那一处（托盘与悬浮窗读同一格）。
    #[test]
    fn the_visible_window_is_spoken_once() {
        let core = include_str!("../../crates/core/src/stats.rs");
        let at = core.find("pub struct RestNotice").expect("载荷找不到了");
        let body = &core[at..core
            .find("const REST_SHOWN_MIN_SECONDS")
            .expect("下限常量不见了")];
        assert!(
            body.contains("pub shown_seconds: i64"),
            "载荷里没有那一格，两个出口就只能各算各的"
        );
        let js = include_str!("../ui/floating.js");
        assert!(
            js.contains("n.shown_seconds"),
            "悬浮窗那条横条还在自己算存活时长"
        );
        assert!(
            !js.contains("}, 20000)"),
            "写死的 20 秒回来了：建议 60 秒时它念着 60 秒却只亮 20 秒"
        );
    }
}
