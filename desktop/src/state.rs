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
    pub avg: i64,
    pub max_day: i64,
    /// 最高单日对应的日期（YYYY-MM-DD）
    pub max_day_date: String,
    pub rank: Vec<(String, i64)>,
    pub group: Vec<(String, i64)>,
    /// 鼠标使用总次数（含点击与滚轮）
    pub mouse_total: i64,
    /// 键盘使用总次数
    pub keyboard_total: i64,
    pub trend: Vec<(String, i64)>,
    pub trend30: Vec<(String, i64)>,
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

// ===== 主窗口懒创建 / 隐藏后卸载页面 状态（见 show_main_window / arm_main_unload）=====

/// 与悬浮窗一致的 WebView2 浏览器启动参数。
/// 注意：WebView2 环境由"首个创建的窗口"建立，之后创建的窗口参数会被忽略，
/// 因此懒创建的主窗口必须与悬浮窗保持同一份参数。
const WEBVIEW_BROWSER_ARGS: &str =
    "--disable-background-networking --disable-component-update --no-first-run --disable-domain-reliability --disable-features=MediaRouter";

/// 主窗口页面是否已被卸载到 about:blank（隐藏超时后卸载，释放页面内存）
static MAIN_UNLOADED: AtomicBool = AtomicBool::new(false);
/// 卸载前的页面 URL（重新显示时导航回去）
static MAIN_URL: Mutex<Option<String>> = Mutex::new(None);
/// 卸载任务是否已安排（显示时置 false 取消；任务唤醒后自行重置）
static MAIN_UNLOAD_ARMED: AtomicBool = AtomicBool::new(false);
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
}

impl AppState {
    /// 初始化：数据库、监听器、统计线程、托盘、热键、窗口可见性。
    pub fn init(app: &mut App) -> anyhow::Result<()> {
        let config = focusflow_core::config::instance();
        let db = focusflow_core::db::Database::init(config)?;
        // 每日维护：按配置自动 VACUUM（auto_vacuum_days 天一次）
        focusflow_core::db::maintenance::maybe_auto_vacuum(config.get_int(
            "database",
            "auto_vacuum_days",
            7,
        ));
        let listener = InputListener::new(config);
        listener.start(Arc::clone(&db));

        // 启动即加载插件（番茄钟/定时任务等随插件 init 运行，对齐 Python 版）
        crate::plugins::with_manager(&db, |_pm| {});
        // 插件热重载（Tauri 无 GUI 轮询循环，用独立扫描线程 + 主线程重载）
        crate::plugins::start_hot_reload(app.handle(), Arc::clone(&db));
        // 键事件 → 插件分发（番茄钟按键计数依赖此链路）
        crate::plugins::start_key_event_dispatch(app.handle(), Arc::clone(&db), &listener);

        let shared = Arc::new(Mutex::new(SharedStats::default()));
        // 默认周期 = 上次退出前选择的周期（前端切换时写入 gui.default_period）
        let default_period = config.get_int("gui", "default_period", -1);
        let period = Arc::new(AtomicI64::new(default_period));
        let refresh_now = Arc::new(AtomicBool::new(false));

        spawn_stats_worker(
            app.handle().clone(),
            Arc::clone(&db),
            config,
            Arc::clone(&shared),
            Arc::clone(&period),
            Arc::clone(&refresh_now),
        );

        let start_to_tray = config.get_bool("gui", "start_to_tray", true);

        let state = Arc::new(AppState {
            db,
            listener,
            config,
            shared,
            period,
            refresh_now,
            start_to_tray,
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
        .expect("启动悬浮窗尺寸修正线程失败");
}

/// 设置窗口初始可见性与悬浮窗位置。
fn setup_windows(app: &App, state: &AppState) {
    let config = focusflow_core::config::instance();

    // 主窗口：启动进托盘时隐藏
    if state.start_to_tray {
        if let Some(win) = app.get_webview_window("main") {
            let _ = win.hide();
        }
    }

    // 悬浮窗：默认顶部靠右（对齐 Python 版），位置可持久化
    if let Some(win) = app.get_webview_window("floating") {
        make_floating_tool_window(&win);
        // WebView2 创建控制器时会把窗口强制放宽到至少 120px（内容实际只需 ~81px），
        // 因此在窗口构建完成后主动缩回目标宽度，并延时重复几次兜底异步放大。
        enforce_floating_size(&win, config);
        let x = config.get_float("floating", "pos_x", f64::NAN);
        let y = config.get_float("floating", "pos_y", f64::NAN);
        if x.is_nan() || y.is_nan() {
            if let Ok(Some(mon)) = win.current_monitor() {
                let scale = win.scale_factor().unwrap_or(1.0);
                let w = mon.size().width as f64 / scale;
                let _ = win.set_position(tauri::LogicalPosition::new(w - 120.0, 60.0));
            }
        } else {
            let _ = win.set_position(tauri::LogicalPosition::new(x, y));
        }

        // 启动时按配置显示悬浮窗
        if config.get_bool("floating", "enabled", true) {
            let _ = win.show();
        }
    }

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
                for label in ["main", "floating"] {
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
        .expect("启动 WebView2 后台状态同步线程失败");
}

/// 设置各窗口 WebView2 内存档位：
/// 应用活跃（主窗口可见）→ Normal；仅托盘/悬浮窗（非活跃）→ Low。
/// WebView2 官方 MemoryUsageTargetLevel API，非活跃时设 Low 可显著降低内存占用。
/// 主窗口虽隐藏但其页面仍在运行，同样要降档才能把内存压下来。
/// 返回是否全部设置成功（WebView2 控制器未就绪时返回 false，调用方可重试）。
pub fn set_floating_memory_level(app: &tauri::AppHandle, low: bool) -> bool {
    let mut all_ok = true;
    for label in ["main", "floating"] {
        if let Some(win) = app.get_webview_window(label) {
            // with_webview 闭包无返回值，用共享标志记录是否真正设置成功
            let done = Arc::new(AtomicBool::new(false));
            let done_cb = Arc::clone(&done);
            let result = win.with_webview(move |webview| {
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
                            done_cb.store(
                                v19.SetMemoryUsageTargetLevel(level).is_ok(),
                                Ordering::SeqCst,
                            );
                        } else {
                            tracing::warn!(
                                "WebView2 运行时过旧，不支持内存档位 API（需 1.0.2390+）"
                            );
                        }
                    }
                    // CoreWebView2 未就绪：静默，等待调用方重试
                }
                #[cfg(not(windows))]
                {
                    let _ = low;
                    done_cb.store(true, Ordering::SeqCst);
                }
            });
            all_ok = all_ok && result.is_ok() && done.load(Ordering::SeqCst);
        }
    }
    all_ok
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
    // with_webview 闭包无返回值，用共享标志记录是否真正设置成功（同 set_floating_memory_level）
    let done = Arc::new(AtomicBool::new(false));
    let done_cb = Arc::clone(&done);
    let result = win.with_webview(move |webview| {
        #[cfg(windows)]
        unsafe {
            let controller = webview.controller();
            done_cb.store(controller.SetIsVisible(visible).is_ok(), Ordering::SeqCst);
        }
        #[cfg(not(windows))]
        {
            let _ = visible;
            done_cb.store(true, Ordering::SeqCst);
        }
    });
    result.is_ok() && done.load(Ordering::SeqCst)
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
            .expect("启动主窗口创建线程失败");
        return;
    }
    show_main_window_impl(app);
}

/// show_main_window 实现体（窗口已存在，或已确保在普通事件循环轮次执行）。
fn show_main_window_impl(app: &tauri::AppHandle) {
    // 取消待执行的"隐藏后卸载页面"任务（线程唤醒后会在锁内再次确认）
    MAIN_UNLOAD_ARMED.store(false, Ordering::SeqCst);
    // 显示/隐藏代次 +1：让仍在等待恢复的旧线程放弃（见 restore_main_after_load）
    MAIN_VIS_EPOCH.fetch_add(1, Ordering::SeqCst);

    if app.get_webview_window("main").is_none() {
        tracing::info!("show_main: 主窗口首次创建（懒创建）");
        // 全新窗口：清理可能残留的卸载状态（正常流程窗口常驻，此处仅为兜底）
        MAIN_UNLOADED.store(false, Ordering::SeqCst);
        MAIN_UNLOAD_ARMED.store(false, Ordering::SeqCst);
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
        .additional_browser_args(WEBVIEW_BROWSER_ARGS)
        .build();
        match &result {
            Ok(_) => tracing::info!("show_main: 主窗口创建成功"),
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
                Some(u) => {
                    let _ = win.navigate(u);
                    // 加载完成前不显示，避免白屏；由恢复线程轮询后恢复渲染并显示
                    restore_main_after_load(app);
                    return;
                }
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
/// 最小 5 秒）；显示路径会置 MAIN_UNLOAD_ARMED=false 取消任务。
/// 可通过 [gui] unload_hidden=false 关闭。
fn arm_main_unload(app: &tauri::AppHandle) {
    let config = focusflow_core::config::instance();
    if !config.get_bool("gui", "unload_hidden", true) {
        return;
    }
    // 已有任务在等待，不重复安排
    if MAIN_UNLOAD_ARMED.swap(true, Ordering::SeqCst) {
        return;
    }
    let delay = config.get_int("gui", "unload_hidden_delay", 60).max(5) as u64;
    let handle = app.clone();
    std::thread::Builder::new()
        .name("main-unload".into())
        .spawn(move || {
            std::thread::sleep(Duration::from_secs(delay));
            // 显示路径会置 ARM=false 取消任务
            if !MAIN_UNLOAD_ARMED.load(Ordering::SeqCst) {
                return;
            }
            let Some(win) = handle.get_webview_window("main") else {
                // 窗口尚不存在（懒创建前）：复位标志，允许后续重新安排卸载
                MAIN_UNLOAD_ARMED.store(false, Ordering::SeqCst);
                return;
            };
            let _guard = MAIN_UNLOAD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            // 锁内最终确认：任务未被取消、窗口仍隐藏
            if !MAIN_UNLOAD_ARMED.load(Ordering::SeqCst) || win.is_visible().unwrap_or(true) {
                return;
            }
            let Ok(url) = win.url() else {
                return;
            };
            // 记录原页面 URL（重新显示时导航回去），再卸载页面
            *MAIN_URL.lock().unwrap_or_else(|e| e.into_inner()) = Some(url.to_string());
            MAIN_UNLOADED.store(true, Ordering::SeqCst);
            let _ = win.navigate(tauri::Url::parse("about:blank").unwrap());
            tracing::info!("主窗口页面已卸载到 about:blank（释放页面内存）");
        })
        .expect("启动主窗口页面卸载线程失败");
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
            // 超时仍未加载：补一次导航兜底，避免 stuck 在 about:blank 白屏
            if !loaded {
                if let Some(win) = handle.get_webview_window("main") {
                    let url = MAIN_URL.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    if let Some(u) = url.and_then(|u| u.parse::<tauri::Url>().ok()) {
                        let _ = win.navigate(u);
                    }
                }
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
        .expect("启动主窗口恢复线程失败");
}

/// 悬浮窗周期重申置顶（对齐 Python 版方案）：
/// 任务栏本身是置顶窗口，鼠标指向时会把悬浮窗压到下面；
/// 每 500ms 用 SetWindowPos(HWND_TOPMOST) 把它重新抬到任务栏之上。
fn keep_floating_on_top(app: &App) {
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
        .spawn(move || loop {
            #[cfg(windows)]
            unsafe {
                use windows::Win32::Foundation::HWND;
                use windows::Win32::UI::WindowsAndMessaging::{
                    GetWindowLongW, IsWindow, SetWindowLongW, SetWindowPos, GWL_EXSTYLE,
                    HWND_TOPMOST, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
                    WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
                };
                let hwnd = HWND(raw_hwnd as *mut _);
                // 窗口已销毁：退出轮询，避免对失效 HWND 反复 SetWindowPos
                if !IsWindow(Some(hwnd)).as_bool() {
                    break;
                }
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
            std::thread::sleep(Duration::from_millis(500));
        })
        .expect("启动悬浮窗置顶线程失败");
}

/// 后台统计线程：
/// 快节奏 500ms 更新今日/CPM 并推送 `stats-live`；
/// 重聚合在周期切换/超时/强制时执行并推送 `stats-charts`。
fn spawn_stats_worker(
    app: AppHandle,
    db: Arc<Database>,
    config: &'static FocusFlowConfig,
    shared: Arc<Mutex<SharedStats>>,
    period: Arc<AtomicI64>,
    refresh_now: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("stats-worker".into())
        .spawn(move || {
            tracing::info!("统计线程已启动");
            let mut prev_logged_today: i64 = -1;
            // "统计更新"日志限频时间戳（打字时计数每秒变化，避免刷屏）
            let mut last_stats_log = Instant::now() - Duration::from_secs(61);
            let tick_ms = 500u64;

            let mut prev_period: i64 = i64::MIN;
            let mut last_heavy = Instant::now() - Duration::from_secs(3600);
            let mut prev_today_count: i64 = -1;
            let mut prev_cpm: i64 = -1;
            let mut prev_active: i64 = -1;
            // 上次重聚合时的今日计数：空闲且数据未变时跳过重聚合，避免无谓的整库查询
            let mut last_heavy_today: i64 = -1;
            // 各周期最高单日缓存：period -> (次数, 日期)；重聚合播种，今日破纪录时快节奏即时更新
            let mut period_max: std::collections::HashMap<i64, (i64, String)> =
                std::collections::HashMap::new();
            // "总计"(period=0) 聚合缓存与全历史最高单日缓存：
            // 两者都需要跨年度库全表扫描/全表 ORDER BY，active 的 2s 快节奏不触发重算，
            // 仅在 强制刷新 / 跨天 / 今日新增写入量 ≥ ALLTIME_RECALC_THRESHOLD 时失效重建。
            const ALLTIME_RECALC_THRESHOLD: i64 = 500;
            let mut alltime_agg: Option<ChartAgg> = None;
            let mut alltime_max: Option<(String, i64)> = None;
            let mut alltime_cache_day: i64 = -1; // 上次缓存构建时的日期（CE 天序号）
            let mut alltime_cache_today: i64 = -1; // 上次缓存构建时的今日计数

            loop {
                let period_val = period.load(Ordering::Relaxed);
                let forced = refresh_now.swap(false, Ordering::Relaxed);
                let period_changed = period_val != prev_period;
                let cur_today = db.writer().map(|w| w.today_count()).unwrap_or(0) as i64;
                let cur_active =
                    db.writer().map(|w| w.today_active_seconds()).unwrap_or(0) as i64;
                let active = cur_today != prev_today_count;
                let heavy_elapsed_ms = last_heavy.elapsed().as_millis() as u64;
                let day_ce = {
                    use chrono::Datelike;
                    chrono::Local::now().date_naive().num_days_from_ce()
                } as i64;
                let alltime_dirty = forced
                    || alltime_max.is_none()
                    || day_ce != alltime_cache_day
                    || (alltime_cache_today >= 0
                        && cur_today - alltime_cache_today >= ALLTIME_RECALC_THRESHOLD);

                // 重聚合节奏随主窗口可见性自适应：
                // - 主窗口打开：活跃（打字）时每 active_refresh_interval 秒刷新一次图表；
                //   空闲时按 full_refresh_interval（配置）刷新。
                // - 主窗口隐藏：悬浮窗/托盘只需要今日计数与速度（快节奏 500ms），
                //   重聚合完全停掉，只在强制/周期切换时执行（打开窗口会触发强制刷新）。
                let main_visible = app
                    .get_webview_window("main")
                    .map(|w| w.is_visible().unwrap_or(false))
                    .unwrap_or(false);
                let floating_visible = app
                    .get_webview_window("floating")
                    .map(|w| w.is_visible().unwrap_or(false))
                    .unwrap_or(false);
                let heavy_interval_ms = if !main_visible {
                    u64::MAX
                } else if active {
                    (config.get_int("gui", "active_refresh_interval", 2).max(1) as u64) * 1000
                } else if cur_today != last_heavy_today {
                    // 空闲但数据自上次重聚合后有变化：按空闲周期刷新
                    (config.get_int("gui", "full_refresh_interval", 10).max(1) as u64) * 1000
                } else {
                    // 空闲且数据未变：无需重算，等有输入或强制/周期切换
                    u64::MAX
                };
                let do_heavy = forced || period_changed || heavy_elapsed_ms >= heavy_interval_ms;

                if do_heavy {
                    // 全历史最高单日（全表 ORDER BY，最贵的单项查询）：仅缓存失效时重查
                    if alltime_dirty {
                        // 先把写线程内存中的增量落库，图表查询才能看到最新按键
                        if let Some(w) = db.writer() {
                            if w.has_pending() {
                                w.flush(true);
                            }
                        }
                        last_heavy = Instant::now();
                        last_heavy_today = cur_today;
                        let today_str = chrono::Local::now()
                            .date_naive()
                            .format("%Y-%m-%d")
                            .to_string();
                        alltime_max = Some(
                            focusflow_core::db::get_alltime_max_day()
                                .unwrap_or((today_str, 0)),
                        );
                        alltime_cache_day = day_ce;
                        alltime_cache_today = cur_today;
                    }

                    // period=0 且缓存有效：直接复用上次聚合结果，不再全库扫描。
                    // 今日破纪录等展示值由下方增量逻辑修正，轻量零查询。
                    let agg = if period_val == 0
                        && !period_changed
                        && !alltime_dirty
                        && alltime_agg.is_some()
                    {
                        alltime_agg.clone().unwrap()
                    } else {
                        let agg = compute_charts(period_val, alltime_max.clone());
                        if period_val == 0 {
                            alltime_agg = Some(agg.clone());
                        }
                        agg
                    };
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

                {
                    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                    s.today_count = today_count;
                    s.cpm = cpm;
                    s.active_seconds = cur_active;
                    s.period = period_val;
                    s.agg.max_day = max_day;
                    s.agg.max_day_date = max_day_date.clone();
                }

                let live_changed = today_count != prev_today_count
                    || cpm != prev_cpm
                    || cur_active != prev_active
                    || period_val != prev_period;
                if live_changed {
                    let live = LiveStats {
                        today_count,
                        cpm,
                        active_seconds: cur_active,
                        period: period_val,
                        max_day,
                        max_day_date,
                    };
                    // 事件定向推送：只发给实际可见的窗口。
                    // 隐藏的窗口渲染进程已停（SetIsVisible=false），不再被 500ms 事件唤醒。
                    if floating_visible {
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
        .expect("启动统计线程失败");
}

/// 重聚合：按周期查询数据库并计算全部图表数据。
///
/// 纯函数（输入周期、输出聚合结果），从统计线程拆出便于单测；
/// 数据一致性由读侧 busy_timeout 与写线程事务保证。
/// `alltime_max` 为统计线程维护的全历史最高单日缓存（全表 ORDER BY 太贵，不在此重查；
/// 传 None 时回退现查，供单测与无缓存路径使用）。
fn compute_charts(period_val: i64, alltime_max: Option<(String, i64)>) -> ChartAgg {
    let (total, key_stats) = match period_val {
        -1 => focusflow_core::db::get_stats_by_date(chrono::Local::now().date_naive()),
        0 => focusflow_core::db::get_stats(None, None),
        n => focusflow_core::db::get_stats(Some(n), None),
    };
    let mut rank: Vec<(String, i64)> = key_stats.iter().map(|(k, v)| (k.clone(), *v)).collect();
    rank.sort_by_key(|(_, v)| std::cmp::Reverse(*v));
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
    let mouse_total =
        groups.get("鼠标点击").copied().unwrap_or(0) + groups.get("滚轮").copied().unwrap_or(0);
    let keyboard_total = total - mouse_total;
    let daily_days = match period_val {
        -1 => 1,
        0 => 30,
        n if n > 0 => n,
        _ => 7,
    };
    let needed = daily_days.max(7).max(30);
    let daily_all = focusflow_core::db::get_daily_counts(needed, None);
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
        0
    } else {
        counts.iter().sum::<i64>() / counts.len() as i64
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
                let (d, c) =
                    focusflow_core::db::get_alltime_max_day().unwrap_or((today_str.clone(), 0));
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
    let wd = focusflow_core::db::queries::aggregate_weekday(&tail30);
    let mut weekday: Vec<(i64, i64)> = wd.into_iter().collect();
    weekday.sort_by_key(|(d, _)| *d);
    let hourly = focusflow_core::db::queries::get_hourly_stats(None);
    ChartAgg {
        total,
        avg,
        max_day,
        max_day_date,
        rank,
        group,
        mouse_total,
        keyboard_total,
        trend,
        trend30: tail30,
        hourly,
        weekday,
    }
}

#[cfg(test)]
mod compute_charts_tests {
    use super::compute_charts;

    #[test]
    fn empty_db_returns_zeroed_agg() {
        // 用隔离的临时程序目录，避免读到开发库
        let dir = std::env::temp_dir().join(format!("ff_compute_charts_{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        focusflow_core::paths::set_app_dir(&dir);
        let agg = compute_charts(0, None);
        assert_eq!(agg.total, 0);
        assert_eq!(agg.rank.len(), 0);
        assert_eq!(agg.mouse_total, 0);
        assert_eq!(agg.keyboard_total, 0);
        focusflow_core::paths::set_app_dir(std::env::temp_dir().join("ff_restore_nonexistent"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
