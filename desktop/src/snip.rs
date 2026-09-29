//! 区域截图：会话状态机、覆盖层窗口驱动、给前端的命令。
//!
//! 采集与裁剪的算法在 `focusflow_core::capture`；这里只管三件事：**什么时候截**、
//! **图怎么到覆盖层**、**结果怎么说给人听**。
//!
//! 一次会话的状态存在进程级静态里而不是 `AppState`：截图是"同时最多一次"的手势，
//! 而 `AppState` 的字段都是统计口径的常驻状态，混进去会让两边生命周期纠缠
//! （悬浮窗/主窗口的隐藏恢复要跨线程传 `Arc<AppState>`，而会话其实一样都不需要）。
//!
//! 覆盖层窗口是**懒创建后常驻**的（label `snip`），与主窗口同一个套路：
//! 不写进 `tauri.conf.json` 的 `windows`，因为那会让一个偶发功能白占一整个 WebView2
//! 渲染进程（`state.rs:577-579` 记着主窗口为什么改成懒创建 —— 50~100MB）。
//! 也不每次截图新建再销毁：`state.rs:581-586` 记着在 WebView2 消息派发栈里同步建窗会挂死，
//! 所以建窗一律走「后台线程 → 等退栈 → `run_on_main_thread`」，建完就留着复用。
//!
//! 复用带来的第二个问题是页面不会再跑一遍启动脚本，于是"底图怎么送到页面"必须是双路：
//! 首次创建时页面自己 `snip_take` 拉（此时会话已在槽里）；之后每次显示由 Rust 广播
//! `snip-ready`，页面收到再拉。只靠广播会丢（页面还没加载完就没有监听者，同 `hotkey.rs:13-15`
//! 记的那个坑），只靠拉取则第二次以后没人会再拉。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewWindow};

use focusflow_core::capture::{self, MonitorRect, Shot};

/// 覆盖层窗口的 label，必须与 `capabilities/default.json` 里的字符串一致。
///
/// 两处各写一遍是这个模块最容易静默坏掉的地方：capabilities 里缺这个 label 时窗口拿不到
/// core 权限，页面第一个 `invoke` 就失败，症状是"屏幕暗一下然后什么都没有"。
/// 有一条用例盯着这个一致性（`snip_label_is_granted_core_permissions`）。
pub const SNIP_LABEL: &str = "snip";

/// 覆盖层页面文件名，相对前端资源根。
const SNIP_PAGE: &str = "snip.html";

/// 藏掉悬浮窗后等 DWM 真的撤掉它的时间。不等待就会被烘进冻结图里（截图上多一块小卡片）。
const HIDE_SETTLE: std::time::Duration = std::time::Duration::from_millis(90);

/// 覆盖层显示后多久没被页面取图就算失败（页面没起来/图解码不了）。
///
/// 给到 8 秒而不是 3 秒：第一次截图要连 WebView2 控制器一起建（见 `ensure_overlay`），
/// 控制器就绪到页面跑起来这段在这条计时之内，卡太紧会把"第一次截图"稳定误判成失败。
const TAKE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(8);

/// 一次冻结的会话。
struct Session {
    epoch: u64,
    /// 整屏原始 BGRA，提交时按选区裁剪。这是全程唯一一份全分辨率副本。
    shot: Shot,
    /// 覆盖层底图（整屏 PNG 的 base64）。页面 `snip_take` 时**取走**：
    /// 取走即空，之后任何一次 `snip_take` 都算陈旧会话，同时这也是看门狗判断
    /// "页面到底拉过图没有"的唯一观察量。
    png_base64: Option<String>,
    /// 这块屏上当时可吸附的窗口矩形（z-order 顶→底），随底图一起交给覆盖层。
    /// 抓一次就固定下来：截图期间桌面本来就被冻结了，页面反复拉取也不该看到不同的清单。
    windows: Vec<MonitorRect>,
    /// 从"按下热键"那一刻起算，用来把首帧耗时打进日志（4K/高缩放的④项要靠它出真数，
    /// 现在手上只有 1080p 的两次读数）。
    started: std::time::Instant,
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);
static EPOCH: AtomicU64 = AtomicU64::new(0);
/// 已有会话在进行中：热键连按时不要并发抓屏（症状是覆盖层闪两下、选区错乱）。
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);
/// 触发本次会话前悬浮窗是否可见 —— 结束时**原样**恢复，而不是按 `[floating] enabled` 恢复。
/// 用户完全可以在开着悬浮窗的时候截图，也可以在关着的时候截图，两种都要回到原样。
static FLOATING_WAS_VISIBLE: AtomicBool = AtomicBool::new(false);

/// 页面回报的选区（CSS 像素）+ 它自己看到的缩放倍数。
#[derive(Debug, Deserialize)]
pub struct SnipSelection {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub dpr: f64,
    pub epoch: u64,
    /// 页面自己的可视区（CSS 像素）。**只用于对账日志**，不参与裁剪 ——
    /// 上一版拿这类尺寸差当硬失败用，1.3% 的取整噪声就把功能判死。
    pub viewport_w: Option<f64>,
    pub viewport_h: Option<f64>,
}

/// 覆盖层启动时要的底图。
#[derive(Debug, Serialize)]
pub struct SnipPayload {
    pub epoch: u64,
    pub png_base64: String,
    /// 窗口所在屏的物理矩形，页面据此知道自己盖住了哪块屏（回报选区只用得上相对坐标）。
    pub monitor: MonitorRect,
    /// Rust 侧认定的缩放倍数，页面拿来和自己的 `devicePixelRatio` 对账。
    pub dpr: f64,
    /// 可吸附窗口（物理像素，z-order 顶→底）。**跟着这份 payload 走而不是另开命令**：
    /// 覆盖层那回 label 不在任何 capability 里、第一个 `invoke` 就失败，所以这里
    /// 刻意不加 IPC 面，页面也拿不到"半新半旧"的两份数据。
    pub windows: Vec<MonitorRect>,
}

/// 提交结果。两个下游动作**分开**上报，不合并成一个"成功/失败"：
/// 存盘成功而剪贴板被占用是真会发生的（形状照 `plugins/host.rs` 的 `(值, 原因串)` 约定）。
#[derive(Debug, Serialize)]
pub struct SnipOutcome {
    pub path: String,
    pub saved: bool,
    pub save_reason: String,
    pub clipboard: bool,
    pub clipboard_reason: String,
    /// 一句可直接展示的话。
    pub line: String,
}

/// 页面 `devicePixelRatio` 与 Rust 侧 `scale_factor()` 对账。
///
/// 两边算的是同一件事，差得明显就说明其中一边读到了错的屏（多屏混排、或 DPI 感知不是
/// Per-Monitor V2）。这时**报错停下**，不要取其中一个继续裁 —— 裁偏了没人看得出来。
fn check_dpr(page_dpr: f64, window_scale: f64) -> Result<(), String> {
    if !page_dpr.is_finite() || page_dpr <= 0.0 {
        return Err(format!("页面回报的缩放倍数不可用：{page_dpr}"));
    }
    if !window_scale.is_finite() || window_scale <= 0.0 {
        return Err(format!("窗口侧的缩放倍数不可用：{window_scale}"));
    }
    let diff = (page_dpr - window_scale).abs();
    if diff > 0.01 {
        return Err(format!(
            "缩放倍数对不上（页面 {page_dpr}，窗口 {window_scale}），已停止截图以免裁偏"
        ));
    }
    Ok(())
}

/// 把两个独立成败的下游动作汇成一句给人看的话。
///
/// 刻意不把"剪贴板失败"折进"成功"：用户按了截图却发现粘不出来，必须在这句话里看得见原因。
fn outcome_line(saved: bool, save_reason: &str, clipboard: bool, clip_reason: &str) -> String {
    match (saved, clipboard) {
        (true, true) => "已存盘并复制到剪贴板".to_string(),
        (true, false) => format!("已存盘，但未复制到剪贴板（{clip_reason}）"),
        (false, true) => format!("已复制到剪贴板，但未存盘（{save_reason}）"),
        (false, false) => format!("截图未完成：存盘（{save_reason}）；剪贴板（{clip_reason}）"),
    }
}

/// 占用会话槽位；已经有会话在进行中时返回 false（调用方直接放弃这次触发）。
fn claim_session(
    shot: Shot,
    png_base64: String,
    windows: Vec<MonitorRect>,
    started: std::time::Instant,
) -> u64 {
    let epoch = EPOCH.fetch_add(1, Ordering::SeqCst) + 1;
    if let Ok(mut slot) = SESSION.lock() {
        *slot = Some(Session {
            epoch,
            shot,
            png_base64: Some(png_base64),
            windows,
            started,
        });
    }
    epoch
}

fn current_epoch() -> u64 {
    SESSION
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|s| s.epoch))
        .unwrap_or(0)
}

/// 清空会话（所有出口都走这里，包括失败路径）。
fn clear_session() {
    if let Ok(mut slot) = SESSION.lock() {
        *slot = None;
    }
}

/// 会话不属于当前 epoch 时给出的原因（陈旧页面/重复提交都归到这里）。
fn stale_reason(epoch: u64) -> String {
    format!(
        "本次截图已结束（请求 epoch={epoch}，当前 epoch={}）",
        current_epoch()
    )
}

/// 恢复悬浮窗到触发前的样子。
///
/// 用 `WebviewWindow::show()` 而不是 `commands::show_floating`：后者会写
/// `[floating] enabled = true`，于是"关着悬浮窗截一张图"会把用户的开关悄悄打开。
fn restore_floating(app: &AppHandle) {
    if !FLOATING_WAS_VISIBLE.swap(false, Ordering::SeqCst) {
        return;
    }
    let Some(win) = app.get_webview_window("floating") else {
        tracing::warn!("恢复悬浮窗：窗口不存在，用户需要自己再打开一次");
        return;
    };
    if let Err(e) = win.show() {
        tracing::error!("恢复悬浮窗失败：{e}");
    }
}

/// 抓屏 + 编码 + 入槽 + 开覆盖层。在专用线程上跑（`trigger` 派下来的）。
fn run_capture(app: &AppHandle) -> Result<(), String> {
    // DPI 感知只记录、**不否决**。理由：感知等级影响的是"抓到的画面是不是原生分辨率"
    // （非 Per-Monitor V2 时系统给的是缩放后的那份，糊一点），而坐标换算仍然成立 ——
    // 缓冲区与覆盖层窗口在同一个进程的虚拟化视图里，两边始终 1:1。
    // 上一版把这件事当硬失败处理，结果是把整条功能全挡死（真机日志里六次截图六次被拒），
    // 而代价只是"可能糊一点"。
    let t0 = std::time::Instant::now();
    let awareness = capture::win::dpi_awareness_text();
    if capture::win::is_per_monitor_v2() {
        tracing::info!("截图开始，DPI 感知 = {awareness}");
    } else {
        tracing::warn!(
            "截图开始，DPI 感知 = {awareness}（不是 Per-Monitor V2：抓到的可能是缩放后的画面，\
             但选区换算不受影响，继续）"
        );
    }
    let shot = capture::win::capture_at_cursor()?;
    // 吸附候选在**冻结这一刻**取：这时覆盖层还没显示，z 序就是用户此刻看到的顺序。
    let windows = capture::win::snap_targets(&shot.rect);
    let png = capture::encode_png(&shot.rgba(), shot.rect.width, shot.rect.height)?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
    let epoch = claim_session(shot, b64, windows, t0);

    // 覆盖层的几何与显示都在主线程做：窗口方法从工作线程调用会阻塞在主循环上等待，
    // 而"设尺寸 → 设位置 → 显示 → 抢前台"必须是原子的一个轮次。
    let handle = app.clone();
    if let Err(e) = app.run_on_main_thread(move || {
        if let Err(e) = show_overlay(&handle, epoch) {
            tracing::error!("显示截图覆盖层失败：{e}");
            abort_session(&handle, &e);
        }
    }) {
        return Err(format!("无法把覆盖层派发到主线程：{e}"));
    }

    // 看门狗：覆盖层显示后页面迟迟没来取图（页面没加载、图解码失败），
    // 就当作失败收尾 —— 否则整屏 topmost 窗口会一直盖在用户桌面上。
    //
    // 判据是"**这个 epoch 还在槽里、且图还没被取走**"。只看"图不在了"会把一次
    // 三秒内就框完的正常截图误判成失败：会话早就清空了，5 秒后再弹一句
    // "覆盖层没能显示内容"。
    let handle = app.clone();
    std::thread::Builder::new()
        .name("snip-watchdog".into())
        .spawn(move || {
            std::thread::sleep(TAKE_DEADLINE);
            let still_waiting_for_image = SESSION
                .lock()
                .ok()
                .and_then(|g| {
                    g.as_ref()
                        .map(|s| s.epoch == epoch && s.png_base64.is_some())
                })
                .unwrap_or(false);
            if still_waiting_for_image {
                tracing::error!("截图覆盖层 {TAKE_DEADLINE:?} 内没有被页面取图，按失败收尾");
                abort_session(&handle, "截图覆盖层没能显示内容（页面未取图），已自动关闭");
            }
        })
        .map_err(|e| format!("启动截图看门狗失败：{e}"))?;
    Ok(())
}

/// 拿到覆盖层窗口；不存在时建一个。**必须在主线程调用**（由 `show_overlay` 的调用点保证）。
///
/// 第二个返回值是"这次是不是刚建的"。它决定要不要广播 `snip-ready`：刚建的窗口里页面还没跑
/// 启动脚本，会自己去拉图；这时再广播一次，页面就可能拉第二次而拿到"底图已被取走"，
/// 于是把一次正常截图当成失败取消掉。复用已存在的窗口时才需要广播。
fn ensure_overlay(app: &AppHandle) -> Result<(WebviewWindow, bool), String> {
    if let Some(win) = app.get_webview_window(SNIP_LABEL) {
        return Ok((win, false));
    }
    tracing::info!("截图覆盖层首次创建（懒创建，之后常驻复用）");
    let win =
        tauri::WebviewWindowBuilder::new(app, SNIP_LABEL, tauri::WebviewUrl::App(SNIP_PAGE.into()))
            .title("FocusFlow 截图")
            // 页面加载的两个阶段各记一行，与下面 `snip_take` 的日志配对，才能把
            // 「页面没加载」和「页面加载了但脚本没跑起来（模块 import 抛错 / `__TAURI__` 不在）」
            // 分开。只有 8 秒看门狗时这两种情况看起来一模一样，白耗一轮真机测试。
            .on_page_load(|_win, payload| {
                let stage = match payload.event() {
                    tauri::webview::PageLoadEvent::Started => "开始",
                    tauri::webview::PageLoadEvent::Finished => "完成",
                };
                tracing::info!("截图覆盖层页面加载{stage}: {:?}", payload.url());
            })
            // 先给占位尺寸，下一步立刻按截到的那块屏改成物理尺寸；此刻仍是 visible(false)，
            // 不会闪出一个错尺寸的框。
            .inner_size(640.0, 360.0)
            .resizable(false)
            .maximizable(false)
            .minimizable(false)
            .visible(false)
            .decorations(false)
            // **不要**设 `.transparent(true)`。tauri-runtime-wry 在 Windows 上对每个透明窗口
            // 都会建一个 softbuffer 表面并立刻 `surface.resize(..).unwrap()`
            // （`tauri-runtime-wry-2.11.4/src/window/windows.rs:59`；`Event::RedrawRequested`
            // 每次还会再来一遍），而本包 release profile 是 `panic = "abort"` ——
            // 那次 unwrap 一失败就是整个进程闪退。真机踩过：按截图热键 → 卡死 → 闪退，
            // 日志停在「截图覆盖层首次创建」，写线程只来得及留下 agg_recovery.json。
            // 覆盖层显示的是盖满每一像素的冻结图，本来就不需要透明；
            // 底图到位前由 CSS 的实心近黑背景兜住，那正好也是"已冻结"该有的观感。
            .always_on_top(true)
            .skip_taskbar(true)
            .focused(true)
            .additional_browser_args(crate::state::WEBVIEW_BROWSER_ARGS)
            .build()
            .map_err(|e| format!("创建截图覆盖层失败：{e}"))?;
    Ok((win, true))
}

/// 覆盖层的客户区到底算不算"铺满了这块屏"——判据是**覆盖**，不是逐像素相等。
///
/// 旧判据写的是 `==`。他真机日志里 27 次截图有 1 次量到：`set_size(1920x1080)` 成功返回之后
/// `inner_size()` 读回来是 1920x**1087**——多出的 7 个像素落在屏幕外，对用户毫无影响，
/// 却被这条判据当成"没铺满"，于是整次截图被 `abort_session` 掉、热键白按一次。
/// 反过来，真正要拦的那种（`SetWindowPos` 少给 `SWP_NOSIZE` 把窗口缩成 0×0、或者短了一截）
/// 在这里照样拦得住：小的那一侧才是要命的一侧。
fn covers_screen(got_w: u32, got_h: u32, screen_w: u32, screen_h: u32) -> bool {
    got_w >= screen_w && got_h >= screen_h
}

/// 把覆盖层摆到目标屏并抢前台。只在主线程执行。
fn show_overlay(app: &AppHandle, epoch: u64) -> Result<(), String> {
    let rect = SESSION
        .lock()
        .ok()
        .and_then(|g| g.as_ref().filter(|s| s.epoch == epoch).map(|s| s.shot.rect))
        .ok_or_else(|| stale_reason(epoch))?;

    let (win, created_now) = ensure_overlay(app)?;

    // 先定尺寸再定位置：反过来时窗口会先用旧尺寸落在新位置上，WebView2 重排一下更晃眼。
    win.set_size(PhysicalSize::new(rect.width, rect.height))
        .map_err(|e| format!("设置覆盖层尺寸失败：{e}"))?;
    win.set_position(PhysicalPosition::new(rect.x, rect.y))
        .map_err(|e| format!("设置覆盖层位置失败：{e}"))?;
    win.show().map_err(|e| format!("显示覆盖层失败：{e}"))?;
    win.set_focus()
        .map_err(|e| format!("覆盖层取焦点失败：{e}"))?;
    raise_topmost(&win);

    // 摆完核对一遍：客户区必须**盖住**那块屏、窗口必须真的可见。
    // 这条专治"窗口存在、页面正常加载、底图也取走了，但用户面前什么都没有"——
    // 那种失败在日志里和成功长得一模一样，只有量过才发现。真机上就是这么栽的：
    // `SetWindowPos` 少给 `SWP_NOSIZE` 把窗口缩成了 0×0。
    let got = win
        .inner_size()
        .map_err(|e| format!("读不到覆盖层客户区尺寸：{e}"))?;
    if !covers_screen(got.width, got.height, rect.width, rect.height) {
        return Err(format!(
            "覆盖层没铺满这块屏：客户区 {}x{}，屏幕 {}x{}",
            got.width, got.height, rect.width, rect.height
        ));
    }
    if !win.is_visible().unwrap_or(false) {
        return Err("覆盖层 show() 之后仍处于不可见状态".to_string());
    }

    if !created_now {
        // 复用的窗口：页面早就跑完启动脚本了，不在这里出声就没人再去拉新的底图。
        if let Err(e) = app.emit_to(SNIP_LABEL, "snip-ready", epoch) {
            return Err(format!("无法通知覆盖层取图：{e}"));
        }
    }
    Ok(())
}

/// 抬到所有窗口之上。
///
/// 照 `state.rs` 里悬浮窗的 `SetWindowPos(HWND_TOPMOST)` 写法，但**不加** `WS_EX_TOOLWINDOW`：
/// 工具窗口可能被拒绝前台激活，而覆盖层必须拿到键盘（Esc 是唯一的取消途径）。
#[cfg(windows)]
fn raise_topmost(win: &WebviewWindow) {
    let Ok(hwnd) = win.hwnd() else {
        tracing::warn!("覆盖层取不到 hwnd，只能依赖 alwaysOnTop 属性");
        return;
    };
    unsafe {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{
            SetForegroundWindow, SetWindowPos, HWND_TOPMOST, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
        };
        let hwnd = HWND(hwnd.0 as *mut _);
        // `SWP_NOMOVE | SWP_NOSIZE` 不是可选项：不给这两个标志，下面那四个 0 就是
        // **把窗口搬到 (0,0) 并缩成 0×0**。真机上踩过一次——窗口存在、页面正常加载、
        // 底图也取走了，但没人能看见它、它也收不到任何鼠标键盘事件。
        if let Err(e) = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        ) {
            tracing::warn!("覆盖层置顶失败（alwaysOnTop 仍在）：{e}");
        }
        // 热键回调有权利把窗口带到前台，所以这一步通常成功；失败只意味着要按一下鼠标。
        if !SetForegroundWindow(hwnd).as_bool() {
            tracing::warn!("覆盖层未能成为前台窗口（Esc 可能要先点一下才生效）");
        }
    }
}

#[cfg(not(windows))]
fn raise_topmost(_win: &WebviewWindow) {}

/// 失败收尾：关窗、清会话、恢复悬浮窗、给前端一句话。
fn abort_session(app: &AppHandle, reason: &str) {
    clear_session();
    IN_FLIGHT.store(false, Ordering::SeqCst);
    if let Some(win) = app.get_webview_window(SNIP_LABEL) {
        if let Err(e) = win.hide() {
            tracing::error!("关闭截图覆盖层失败：{e}");
        }
    }
    restore_floating(app);
    if let Err(e) = app.emit("snip-done", reason) {
        tracing::warn!("截图结果发不回前端：{e}");
    }
}

/// 结束一次会话的正常出口（提交与取消共用）。
fn finish_session(app: &AppHandle) {
    clear_session();
    IN_FLIGHT.store(false, Ordering::SeqCst);
    if let Some(win) = app.get_webview_window(SNIP_LABEL) {
        if let Err(e) = win.hide() {
            // 关不掉覆盖层比丢一张图严重：整屏被盖住。出声，但不 panic。
            tracing::error!("隐藏截图覆盖层失败：{e}");
        }
    }
    restore_floating(app);
}

/// 开始一次截图。由全局热键（主线程）与设置页按钮调用。
///
/// 抓屏、PNG 编码这些重活一律不在主线程做 —— 热键回调就跑在主线程上，
/// 在那儿 BitBlt 一次会把整个事件循环卡住两三百毫秒。
pub fn trigger(app: &AppHandle) -> Result<(), String> {
    // swap 的返回值是"进去之前是不是已经有人了"。已经有人时**不要**把它改回 false：
    // 那个 true 属于上一次会话，被这次失败的触发清掉的话，下一次就能并发抓第二张屏。
    if IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return Err("已有一张截图在进行中，先完成它（Esc 取消或框选一块）".to_string());
    }

    // 悬浮窗的可见性必须在主线程读、也必须先藏起来再抓屏，顺序反了就把悬浮窗截进图里。
    match app.get_webview_window("floating") {
        Some(floating) => {
            let visible = floating.is_visible().unwrap_or(false);
            FLOATING_WAS_VISIBLE.store(visible, Ordering::SeqCst);
            if visible {
                if let Err(e) = floating.hide() {
                    tracing::warn!("藏悬浮窗失败，截图里可能带上它：{e}");
                }
            }
        }
        // 悬浮窗不存在时不能什么都不做：FLOATING_WAS_VISIBLE 得留在 false，
        // 否则上一张截图留下的 true 会让收尾去 show() 一个不存在的窗口。
        None => FLOATING_WAS_VISIBLE.store(false, Ordering::SeqCst),
    }

    let handle = app.clone();
    std::thread::Builder::new()
        .name("snip-capture".into())
        .spawn(move || {
            // 等 DWM 把悬浮窗真的撤掉（上面的 hide 只是发了消息）。
            std::thread::sleep(HIDE_SETTLE);
            if let Err(e) = run_capture(&handle) {
                tracing::error!("截图失败：{e}");
                abort_session(&handle, &e);
            }
        })
        .map_err(|e| {
            IN_FLIGHT.store(false, Ordering::SeqCst);
            restore_floating(app);
            format!("启动截图线程失败：{e}")
        })?;
    Ok(())
}

/// 覆盖层页面启动时来取底图。**图被取走**（见 `Session::png_base64`），
/// 于是"看门狗有没有看到页面拉过图"这件事有了一个真的观察量。
#[tauri::command]
pub async fn snip_take(app: AppHandle) -> Result<SnipPayload, String> {
    // epoch 与图必须在**同一次加锁**里取：分两次锁的话，中间若换了会话，就会把新会话的图
    // 配着旧 epoch 交出去 —— 页面之后回报的 epoch 永远对不上，症状是"框完点提交没反应"。
    let (epoch, png, rect, windows, started) = {
        let mut slot = SESSION.lock().map_err(|_| "截图会话锁不可用")?;
        let s = slot
            .as_mut()
            .ok_or_else(|| "没有进行中的截图".to_string())?;
        (
            s.epoch,
            s.png_base64.take(),
            s.shot.rect,
            s.windows.clone(),
            s.started,
        )
    };
    let png = png.ok_or_else(|| "本次截图的底图已被取走过，请重新触发一次截图".to_string())?;
    // 这一行与 `on_page_load` 的"完成"配对看：加载完成却没有这行 = 页面脚本没跑起来。
    // 带上"距触发多少毫秒"是为了让首帧延迟这件事有真数可看（抓屏 + PNG 编码 + base64 +
    // 派发 + 页面拉取全在这段里），而不是靠人肉对两条日志的时间戳。
    tracing::info!(
        "截图覆盖层已取走底图（epoch {epoch}，base64 {} 字节，屏 {}x{}，距触发 {} ms）",
        png.len(),
        rect.width,
        rect.height,
        started.elapsed().as_millis()
    );
    let win = app
        .get_webview_window(SNIP_LABEL)
        .ok_or_else(|| format!("覆盖层窗口 {SNIP_LABEL} 不存在"))?;
    let dpr = win
        .scale_factor()
        .map_err(|e| format!("取缩放倍数失败：{e}"))?;
    Ok(SnipPayload {
        epoch,
        png_base64: png,
        monitor: rect,
        dpr,
        windows,
    })
}

/// 提交选区：裁剪 → 编码 → 写盘 → 进剪贴板。
#[tauri::command]
pub async fn snip_commit(app: AppHandle, sel: SnipSelection) -> Result<SnipOutcome, String> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || commit_blocking(&handle, sel))
        .await
        .map_err(|e| format!("截图任务被中断：{e}"))?
}

fn commit_blocking(app: &AppHandle, sel: SnipSelection) -> Result<SnipOutcome, String> {
    // 1) 两把尺子先对齐再动手（页面自己数的 CSS 像素 vs 窗口认定的缩放倍数）。
    let win = app
        .get_webview_window(SNIP_LABEL)
        .ok_or_else(|| format!("覆盖层窗口 {SNIP_LABEL} 不存在"))?;
    let scale = win
        .scale_factor()
        .map_err(|e| format!("取缩放倍数失败：{e}"))?;
    check_dpr(sel.dpr, scale)?;

    // 2) 在锁内**直接裁**：既不 clone 整屏（4K 是 33 MB），也不把底图取走 ——
    //    这一步之后任何失败都保留会话与覆盖层，让用户重新框一次就能再提交，
    //    而不是"失败一次就得重新触发截图"。
    let (cropped, phys) = {
        let slot = SESSION.lock().map_err(|_| "截图会话锁不可用")?;
        let s = slot
            .as_ref()
            .ok_or_else(|| "没有进行中的截图".to_string())?;
        if s.epoch != sel.epoch {
            return Err(stale_reason(sel.epoch));
        }
        // 覆盖层可视区与"显示器物理尺寸 / 缩放倍数"差太远时只记一行。差得远意味着
        // 窗口没盖满这块屏（或倍数读错了），那时选区会整体偏移 —— 但要让它出声，
        // 不要拿它当拦截：这类守卫连着把功能判死过两次。
        if let (Some(vw), Some(vh)) = (sel.viewport_w, sel.viewport_h) {
            let ew = f64::from(s.shot.rect.width) / scale;
            let eh = f64::from(s.shot.rect.height) / scale;
            let off = ((vw - ew).abs() / ew.max(1.0)).max((vh - eh).abs() / eh.max(1.0));
            if off > 0.02 {
                tracing::warn!(
                    "截图：覆盖层可视区 {vw:.0}x{vh:.0} 与显示器 {ew:.0}x{eh:.0} 差 {:.1}%\
                     （换算仍按 dpr，不拦这次截图）",
                    off * 100.0
                );
            }
        }
        let phys = capture::css_rect_to_physical(
            &capture::CssRect {
                x: sel.x,
                y: sel.y,
                w: sel.w,
                h: sel.h,
            },
            sel.dpr,
            &s.shot.rect,
        )?;
        let cropped = s
            .shot
            .crop(phys.x as u32, phys.y as u32, phys.width, phys.height)?;
        (cropped, phys)
    };

    // 3) 只把裁出来的小块转 RGBA 编码（CF_DIB 那份仍用原始 BGRA）。
    let png = capture::encode_png(
        &{
            let mut b = cropped.bgra.clone();
            capture::bgra_to_rgba(&mut b);
            b
        },
        phys.width,
        phys.height,
    )?;

    // 4) 写盘。失败不阻断剪贴板（反过来也一样），两个结果分开交出去。
    let dir = focusflow_core::paths::screenshots_dir();
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
    let stem = capture::name_stem(&ts, phys.width, phys.height);
    let listing: Vec<String> = std::fs::read_dir(&dir)
        .map(|it| {
            it.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    let file = dir.join(capture::unique_name(&stem, "png", &listing));
    let (saved, save_reason) = match std::fs::write(&file, &png) {
        Ok(()) => {
            tracing::info!("截图已存盘：{}（{} 字节）", file.display(), png.len());
            (true, String::new())
        }
        Err(e) => {
            tracing::error!("截图写盘失败（{}）：{e}", file.display());
            (false, e.to_string())
        }
    };

    // 5) 剪贴板：CF_DIB 要的正是裁剪出来那份 BGRA（不用再解码 PNG），PNG 就是刚编好的那一份。
    //    两个格式都交 —— CF_DIB 给传统 GDI 目标，PNG 给浏览器/网页编辑器（它们不看 CF_DIB）。
    //    线程亲和：整段在同一个 spawn_blocking 线程里跑完（见 core 侧注释）。
    let (clipboard, clipboard_reason) = match capture::win::write_image_to_clipboard(
        &cropped.bgra,
        phys.width,
        phys.height,
        &png,
    ) {
        Ok(()) => {
            // 成功也要出声：只记失败的话，日志里"提交并写进了剪贴板"与"根本没走到这一步"
            // 长得一模一样（今天我就是因此误判了一次他的操作）。
            tracing::info!(
                "截图已进剪贴板（CF_DIB bottom-up + PNG，{}x{}）",
                phys.width,
                phys.height
            );
            (true, String::new())
        }
        Err(e) => {
            tracing::warn!("截图进剪贴板失败：{e}");
            (false, e)
        }
    };

    let line = outcome_line(saved, &save_reason, clipboard, &clipboard_reason);
    finish_session(app);
    if let Err(e) = app.emit("snip-done", line.clone()) {
        tracing::warn!("截图结果发不回前端：{e}");
    }
    Ok(SnipOutcome {
        path: file.display().to_string(),
        saved,
        save_reason,
        clipboard,
        clipboard_reason,
        line,
    })
}

/// 取消一次截图：什么都不留下（不落文件、不动剪贴板）。
///
/// `reason` 由页面在"出错所以取消"时带上。必须落到日志里：页面上的失败（图没解码、
/// 尺寸对不上、脚本抛异常）原本在这边完全看不见，只剩一条 8 秒后的看门狗超时，
/// 白烧掉一轮真机测试。
#[tauri::command]
pub fn snip_cancel(app: AppHandle, reason: Option<String>) {
    match reason.as_deref() {
        Some(r) if !r.is_empty() => tracing::warn!("截图被页面取消：{r}"),
        _ => tracing::debug!("截图已取消（用户主动）"),
    }
    // 收尾是幂等的（无会话时也照样把窗口藏好、悬浮窗复位），所以不在这里分叉
    finish_session(&app);
}

/// 设置页"立即截图"：不依赖热键也能走完整条路（也是手动验证的入口）。
#[tauri::command]
pub async fn do_snip(app: AppHandle) -> Result<String, String> {
    trigger(&app)?;
    Ok("已进入截图：拖框选区后自动存盘并复制，按 Esc 取消".to_string())
}

/// 覆盖层被 Alt+F4 / 关闭按钮收走时的收尾。`lib.rs` 的 `on_window_event` 调用。
///
/// 必须 `prevent_close`：这个窗口是声明式常驻的，真被销毁后就没有可靠的办法在进程内
/// 重建 WebView2 控制器（见 `state.rs` 里那条挂死复现），下次截图就永远开不了。
pub fn on_snip_close_requested(app: &AppHandle) {
    finish_session(app);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 覆盖层自检的判据：**多出来可以，少了不行**。
    ///
    /// 这条钉的是那次真机误判：`set_size(1920x1080)` 之后 `inner_size()` 报 1920x1087，
    /// 旧的 `==` 判据把一整次截图判死（27 次里 1 次）。而它必须仍然拦得住真正出事的那两种
    /// ——0×0（`SWP_NOSIZE` 漏给的那次）和短一截。
    #[test]
    fn overlay_geometry_accepts_overcoverage_but_not_undercoverage() {
        // 正好铺满
        assert!(covers_screen(1920, 1080, 1920, 1080));
        // 真机量到的那一例：多 7 个像素落在屏幕外
        assert!(
            covers_screen(1920, 1087, 1920, 1080),
            "超出屏幕的富余不该把功能判死"
        );
        // 副屏负坐标时屏宽也可能是别的值
        assert!(covers_screen(2560, 1440, 2560, 1440));

        // 下面这些才是真故障
        assert!(
            !covers_screen(0, 0, 1920, 1080),
            "0x0 必须拦（SWP_NOSIZE 那一栽）"
        );
        assert!(
            !covers_screen(1919, 1080, 1920, 1080),
            "横向少 1 像素也要拦"
        );
        assert!(
            !covers_screen(1920, 1079, 1920, 1080),
            "纵向少 1 像素也要拦"
        );
        assert!(!covers_screen(960, 540, 1920, 1080), "只铺了四分之一必须拦");
    }

    #[test]
    fn dpr_must_agree_within_a_rounding_slack() {
        assert!(check_dpr(1.0, 1.0).is_ok());
        assert!(check_dpr(1.25, 1.25).is_ok());
        assert!(check_dpr(1.5, 1.5004).is_ok(), "半像素级的差是可容忍的舍入");
        for (a, b) in [(1.0, 1.25), (1.25, 1.0), (1.0, 1.5)] {
            let err = check_dpr(a, b).unwrap_err();
            assert!(
                err.contains("对不上"),
                "要说清是两把尺子不一致，实际：{err}"
            );
        }
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(check_dpr(bad, 1.0).is_err());
            assert!(check_dpr(1.0, bad).is_err());
        }
    }

    /// 两个下游动作分开上报：任何一边失败都要在这句话里看得见，不许"整体成功"。
    #[test]
    fn outcome_line_keeps_both_downstream_results_separate() {
        assert_eq!(outcome_line(true, "", true, ""), "已存盘并复制到剪贴板");
        let only_file = outcome_line(true, "", false, "被占用");
        assert!(only_file.contains("未复制") && only_file.contains("被占用"));
        let only_clip = outcome_line(false, "磁盘只读", true, "");
        assert!(only_clip.contains("未存盘") && only_clip.contains("磁盘只读"));
        let both = outcome_line(false, "A", false, "B");
        assert!(
            both.contains("A") && both.contains("B"),
            "两条原因都要出现在话里：{both}"
        );
        assert!(!both.contains("已存盘"));
    }

    /// label 与页面文件这两处"字符串约定"必须成立。
    ///
    /// 两个失败症状是一样的（屏幕暗一下就没了），所以都钉在编译/用例期：
    ///     - capabilities 里没有这个 label ⇒ 懒创建出来的窗口拿不到 core 权限，
    ///       页面第一个 `invoke` 就失败；
    ///     - 页面文件不存在 ⇒ `WebviewUrl::App("snip.html")` 加载失败，
    ///       `include_str!` 让它在**编译期**就暴露，而不是等用户按键。
    #[test]
    fn snip_label_is_granted_core_permissions_and_page_exists() {
        let cap = include_str!("../capabilities/default.json");
        assert!(
            cap.contains(&format!("\"{SNIP_LABEL}\"")),
            "capabilities/default.json 的 windows 数组里没有 {SNIP_LABEL}"
        );
        let page = include_str!("../ui/snip.html");
        assert!(
            page.contains("snip.js"),
            "snip.html 没有加载 snip.js，页面只是个空框"
        );
    }

    /// 覆盖层不能被声明进 `tauri.conf.json`：那等于为偶发功能常驻一个渲染进程。
    /// 这条用例盯的是"有人顺手把它加回声明里"这个可预见的回潮。
    #[test]
    fn overlay_window_is_not_declared_eagerly_in_config() {
        let conf = include_str!("../tauri.conf.json");
        assert!(
            !conf.contains(&format!("\"label\": \"{SNIP_LABEL}\"")),
            "{SNIP_LABEL} 被写进了 tauri.conf.json 的 windows —— 它会启动就建一个 \
             WebView2 渲染进程（50~100MB），与本仓主窗口/悬浮窗的懒创建口径相反"
        );
    }

    #[test]
    fn stale_reason_names_both_epochs() {
        let r = stale_reason(7);
        assert!(r.contains("epoch=7"), "要带上被拒的 epoch：{r}");
    }
}
