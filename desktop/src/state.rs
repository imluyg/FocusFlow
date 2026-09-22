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
        // 悬浮窗本来就是透明窗口：Acrylic 生效时卡片背后是真实的系统级模糊；
        // 失败则维持原有"半透明卡片"外观，无需前端配合
        let dark = config.get("gui", "theme") == "dark";
        apply_window_vibrancy(&win, dark);
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
fn apply_window_vibrancy(win: &tauri::WebviewWindow, dark: bool) -> bool {
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
fn apply_window_vibrancy(_win: &tauri::WebviewWindow, _dark: bool) -> bool {
    false
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
            .expect("启动主窗口创建线程失败");
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
            let _guard = MAIN_UNLOAD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            // 锁内二次确认：令牌未过期、窗口仍隐藏
            if MAIN_UNLOAD_EPOCH.load(Ordering::SeqCst) != token || win.is_visible().unwrap_or(true)
            {
                return;
            }
            // 页面已经卸载过：绝不能再次记录恢复 URL。此刻 win.url() 是 about:blank，
            // 覆盖进去会让主窗口永远恢复不出来（见 MAIN_UNLOAD_EPOCH 注释）。
            if MAIN_UNLOADED.load(Ordering::SeqCst) {
                return;
            }
            let Ok(url) = win.url() else {
                return;
            };
            // 兜底：窗口本来就停在空白页（没有可恢复的页面），不做无意义的"卸载"
            if url.as_str() == "about:blank" {
                return;
            }
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
                schedule_app_restart(&handle);
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
        .expect("启动主窗口恢复线程失败");
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

/// 自动重启应用（WebView2 环境损坏等无法在线恢复的场景）：
/// 后台线程延迟几秒后拉起新进程（当前 exe，数据目录解析与本次一致），
/// 当前进程干净退出——退出路径完成 flush/备份，未落库增量由恢复文件兜底。
///
/// 新进程带 `--wait-pid <本进程 PID>`：等本进程真正退出后再初始化，
/// 否则会撞上单实例守卫（新进程被当成第二个实例直接退出，结果是两个进程都没了）
/// 与 WebView2 用户数据目录；带 `--show-main`：用户本来就是在等面板打开，
/// 重启后直接把面板显示出来。
fn schedule_app_restart(app: &tauri::AppHandle) {
    if RESTART_SCHEDULED.swap(true, Ordering::SeqCst) {
        return;
    }
    let depth = restart_depth_from_args();
    if depth >= MAX_RESTART_DEPTH {
        tracing::error!("自动重启已达上限（深度 {depth}），放弃重启：直接显示空白窗口");
        reveal_main_window(app);
        return;
    }
    let handle = app.clone();
    std::thread::Builder::new()
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
                .arg((depth + 1).to_string())
                .arg("--show-main")
                .spawn()
            {
                Ok(_) => {
                    tracing::info!(
                        "应用自动重启：新进程已启动（深度 {}），当前进程退出",
                        depth + 1
                    );
                    handle.exit(0);
                }
                Err(e) => {
                    tracing::error!("自动重启失败（拉起新进程）: {e}，应用保持运行");
                    reveal_main_window(&handle);
                }
            }
        })
        .expect("启动应用重启线程失败");
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
        .expect("启动显示主窗口线程失败");
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
        .spawn(move || {
            // 默认隐藏态轮询间隔（3s），可见时每轮覆写为 500ms
            let mut sleep_ms: u64 = 3000;
            loop {
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
                    if IsWindowVisible(hwnd).as_bool() {
                        sleep_ms = 500;
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
            // 缓存构建时的全历史总次数（-1 = 尚未构建）
            let mut alltime_total_base: i64 = -1;
            let mut alltime_cache_day: i64 = -1; // 上次缓存构建时的日期（CE 天序号）
            let mut alltime_cache_today: i64 = -1; // 上次缓存构建时的今日计数
                                                   // period != 0 图表的落库序号指纹：DB 自上次聚合后没有新落库（序号未变）
                                                   // 时，重聚合只会重复算出同样结果——增量都在写线程内存里，flush 前不进库。
            let mut charts_seq: u64 = u64::MAX;
            let mut charts_period: i64 = i64::MIN;
            // 上次生成自动周报的锚点（上一个整周周一的 CE 天序号）
            let mut last_report_week: i64 = 0;

            loop {
                let period_val = period.load(Ordering::Relaxed);
                let forced = refresh_now.swap(false, Ordering::Relaxed);
                let period_changed = period_val != prev_period;
                let cur_today = db.writer().map(|w| w.today_count()).unwrap_or(0) as i64;
                let cur_active = db.writer().map(|w| w.today_active_seconds()).unwrap_or(0) as i64;
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

                // 自动周报：进程启动后的第一轮、以及跨进新一周后的第一轮，为
                // 「刚结束的那个整周」生成一次 Markdown 报告。锚点取本周一，
                // 所以同一周内 marker 恒定 —— 每周最多一次，重启也只是重写同一份
                // 文件。生成要跑 7 天的聚合查询，放进独立线程，不占用本线程的
                // 500ms 快节奏。
                let week_marker = {
                    use chrono::Datelike;
                    let (f, _) = focusflow_core::stats::last_finished_week(
                        chrono::Local::now().date_naive(),
                    );
                    f.num_days_from_ce() as i64
                };
                if week_marker != last_report_week {
                    last_report_week = week_marker;
                    std::thread::Builder::new()
                        .name("weekly-report".into())
                        .spawn(|| match crate::export::write_weekly_report() {
                            Ok(Some(p)) => tracing::info!("自动周报已生成: {}", p.display()),
                            // 新装/长期没用：那一周没有记录，本来就没有可报告的
                            Ok(None) => tracing::debug!("上一个整周没有记录，跳过自动周报"),
                            Err(e) => tracing::warn!("自动周报生成失败: {e}"),
                        })
                        .ok();
                }

                if do_heavy {
                    let flush_seq = db.writer().map(|w| w.flush_seq()).unwrap_or(0);
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
                            last_heavy = Instant::now();
                            last_heavy_today = cur_today;
                            let today_str = chrono::Local::now()
                                .date_naive()
                                .format("%Y-%m-%d")
                                .to_string();
                            // 总计与最高单日同源同失效条件：一次跨库遍历同时取回
                            let (total_all, max_all) = focusflow_core::db::get_alltime_summary();
                            alltime_total_base = total_all;
                            alltime_max = Some(max_all.unwrap_or((today_str, 0)));
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
                        charts_period = period_val;
                        charts_seq = db.writer().map(|w| w.flush_seq()).unwrap_or(flush_seq);
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

                let (live_alltime_total, period_total) = {
                    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                    s.today_count = today_count;
                    s.cpm = cpm;
                    s.active_seconds = cur_active;
                    s.period = period_val;
                    s.agg.max_day = max_day;
                    s.agg.max_day_date = max_day_date.clone();
                    s.agg.alltime_total = alltime_total;
                    // period 与两个总数在同一把锁内快照：前端拿到的永远是自洽的一组
                    (s.agg.alltime_total, s.agg.total)
                };

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
                        alltime_total: live_alltime_total,
                        period_total,
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
    // 前台应用使用时长排行（秒数），周期选择与按键统计联动。
    // 保留查询返回的总秒数作为占比分母（未截断，与 apps 之和不是一回事）。
    let (app_total, mut apps): (i64, Vec<(String, i64)>) = {
        let (total, map) = match period_val {
            -1 => focusflow_core::db::get_app_stats_by_date(chrono::Local::now().date_naive()),
            0 => focusflow_core::db::get_app_stats(None, None),
            n => focusflow_core::db::get_app_stats(Some(n), None),
        };
        (total, map.into_iter().collect())
    };
    apps.sort_by_key(|(_, s)| std::cmp::Reverse(*s));
    apps.truncate(RANK_LIMIT);
    // 设备维度统计（Raw Input 侧信道，独立口径：键盘按下 + 鼠标按键 + 滚轮）。
    // 查询侧已按次数降序、显示名去重，这里只截断。
    let (device_total, mut devices) = match period_val {
        -1 => focusflow_core::db::get_device_stats_by_date(chrono::Local::now().date_naive()),
        0 => focusflow_core::db::get_device_stats(None, None),
        n => focusflow_core::db::get_device_stats(Some(n), None),
    };
    devices.truncate(RANK_LIMIT);
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
    // 星期分布天然稠密：上面的 needed 恒 >=30 天，而 get_daily_counts
    // 会把整个窗口零填充，故 7 个星期几必定都有值（可能为 0）。
    let wd = focusflow_core::db::queries::aggregate_weekday(&tail30);
    let mut weekday: Vec<(i64, i64)> = wd.into_iter().collect();
    weekday.sort_by_key(|(d, _)| *d);
    let hourly = focusflow_core::db::queries::get_hourly_stats(None);
    ChartAgg {
        total,
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
    use super::{alltime_total_now, compute_charts};

    #[test]
    fn empty_db_returns_zeroed_agg() {
        // app_dir 是进程级全局：与周报用例（会往自己目录里种年度库）并行跑时，
        // 不持锁就会读到对方的数据（表现为「空库应为 0」拿到 42 万）。
        let _serial = crate::app_dir_lock();
        // 用隔离的临时程序目录，避免读到开发库
        let dir = std::env::temp_dir().join(format!("ff_compute_charts_{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        focusflow_core::paths::set_app_dir(&dir);
        let agg = compute_charts(0, None);
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
        focusflow_core::paths::set_app_dir(std::env::temp_dir().join("ff_restore_nonexistent"));
        std::fs::remove_dir_all(&dir).ok();
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
}
