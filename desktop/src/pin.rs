//! 贴图（钉图）：把一次截图提交产出的那张 PNG 摆在一个不透明小窗口里，钉在桌面上。
//!
//! 三条口径写在这里，别在调用方各写一遍：
//!
//! 1. **贴图算一次截图的一个出口**（2026-09-30 定的 A 案）：点「贴图」走的仍然是完整的
//!    `snip_commit` —— 存盘、进剪贴板那两步一行没改，然后才把**刚编好的那一份** png 交给
//!    新窗口。这里既不重新编码，也**不再写第二次盘**（有一条用例按文本形状钉着：本模块
//!    生产代码里不许出现 `fs::write`）。所以贴图不会在正常产物之外多出一个文件。
//! 2. **贴图窗口必须不透明**。理由与 `snip.rs` 的 `ensure_overlay` 是同一条，不是审美问题：
//!    Windows 上每个透明窗口都会建一个 softbuffer 表面并立刻 `surface.resize(..).unwrap()`，
//!    而本包 release 是 `panic = "abort"` —— 那次 unwrap 一失败就是整个进程闪退。
//!    不要为了阴影、圆角或半透明去碰它。
//! 3. **贴图会被抓进下一次截图的底图**（同日确认：保持现状）。`capture::snap_candidate` 按
//!    进程号排掉了本进程的窗口，所以它吸不中候选；但抓屏是整块屏的 BitBlt，屏幕上有的就拍得进去。
//!    这是有意的：贴图就是「摆在桌面上的东西」，「参照着它截旁边那块」正是它的用法。
//!    反面做法（抓屏前把贴图窗藏掉、等 DWM 撤掉再拍）要把 `snip.rs` 那套按一个悬浮窗定的
//!    `HIDE_SETTLE` 改成按 N 个窗口定，还要把恢复挂到 `abort_session` 那条出口上 —— 等真出现
//!    「脏图」再谈。
//!
//! 为什么不能一个窗口装多张贴图：贴图的定义就是「每块图各自能拖、各自能关、各自有 z-order」，
//! 而一个窗口只有一个矩形。所以这里是**多实例**：label 是 `pin-1`、`pin-2`… 单调递增、
//! **只增不复用**（复用一个刚请求关闭、可能还没真销毁的 label，会得到「窗口存在但里面什么都没有」）。
//! 代价也说清楚：每张贴图是一个独立的 WebView2 窗口，所以有 `MAX_PIN_WINDOWS` 挡在门口。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use base64::Engine as _;
use serde::Serialize;
use tauri::{AppHandle, Manager, PhysicalPosition, PhysicalSize, WebviewWindow};

use focusflow_core::capture::MonitorRect;

/// label 前缀。`capabilities/default.json` 的 `windows` 里那条 `pin-*` 就是照这个串写的，
/// 有一条用例盯着两边一致 —— 改这里不改那边，新窗口会像当年 snip 那样「调什么都被拒」。
pub const PIN_LABEL_PREFIX: &str = "pin-";

/// 贴图页面文件名，相对前端资源根。
const PIN_PAGE: &str = "pin.html";

/// 同时最多开几张贴图。**这是拍的数**，不是量出来的：每张是一个独立 WebView2 窗口，
/// 真正的上限要看真机内存与体感。到点拒绝并出一句人话，比让它无限开下去好收拾。
const MAX_PIN_WINDOWS: usize = 12;

/// 槽里最多留几份底图。底图是整块选区的 PNG base64（全屏那一档一张就有几 MB），
/// 窗口一多就要有个天花板。超出时丢**最老**的那份：已经画出来的窗口不受影响，
/// 只有那张老窗口真去重新加载页面时才会取不到图 —— 那种情况会有一行 warn。
///
/// 「不受影响」要有东西兜着：这份天花板比 `MAX_PIN_WINDOWS` 低，所以**尺寸**不能跟着
/// 像素一起被挤掉，否则最老那几扇还活着的窗口连滚轮缩放都会报「底图不在了」。
/// 尺寸记在下面的 `DIMS` 里，与像素分开。
const MAX_PIN_IMAGES: usize = 8;

/// `open()` 等主线程把窗口建回来的上限。正常是几十毫秒；给到 5 秒是因为它一旦超时，
/// 用户面前是「截好了但没贴图」，宁可多等一会儿也要把真正的原因带回去，而不是先说成功。
const BUILD_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

/// 贴图窗口滚轮能用到的缩放档位（相对 100%）。档位是离散的：连续缩放会让「1:1」这一档
/// 再也回不去，而 1:1 恰好是这张图唯一不失真的看法。
const ZOOM_STEPS: [f64; 7] = [0.25, 0.5, 0.75, 1.0, 1.5, 2.0, 3.0];

/// 槽里的一份底图。`width`/`height` 是**物理像素**，与 png 的像素尺寸严格一致 ——
/// 窗口尺寸与页面绘制都从这两个数算，所以「尺寸恰好等于图」只在这一处定义。
#[derive(Debug, Clone)]
struct PinSlot {
    png_base64: String,
    width: u32,
    height: u32,
}

/// 页面 `pin_take` 拿到的东西。
#[derive(Debug, Serialize)]
pub struct PinPayload {
    pub id: u32,
    pub png_base64: String,
    pub width: u32,
    pub height: u32,
}

static PIN_SEQ: AtomicU32 = AtomicU32::new(0);
/// 用 `Vec` 而不是 `BTreeMap`：要的就是「插入顺序 = 新旧顺序」，满了直接从头上丢。
static PENDING: Mutex<Vec<(u32, PinSlot)>> = Mutex::new(Vec::new());

/// 尺寸账：`seq` → 那张图的物理宽高。两个数，没有 base64。
///
/// 为什么不与 `PENDING` 同一份：像素那份有 `MAX_PIN_IMAGES` 的天花板，而窗口有
/// `MAX_PIN_WINDOWS` 个 —— 一起丢就会让「还活着的第 9 扇窗口」的滚轮缩放报错，
/// 而缩放根本不需要那几个字节（`pin_resize` 只要宽高）。
/// 天花板取 `MAX_PIN_WINDOWS`：`open()` 在已经有 `MAX_PIN_WINDOWS` 扇活着时就拒了，
/// 所以正常路径上这里丢不到还在用的那扇。
static DIMS: Mutex<Vec<(u32, (u32, u32))>> = Mutex::new(Vec::new());

/// `seq` → 窗口 label（日志与回收用）。建窗那一点不叫它，见 `build_pin` 里的说明。
fn pin_label(seq: u32) -> String {
    format!("{PIN_LABEL_PREFIX}{seq}")
}

/// label 是不是贴图窗口的（`on_window_event` 与 `pin_close_all` 都靠它筛）。
/// 要求前缀后面全是数字且非空，否则 `pin-abc` 这种也会被当成贴图收走。
fn is_pin_label(label: &str) -> bool {
    let Some(rest) = label.strip_prefix(PIN_LABEL_PREFIX) else {
        return false;
    };
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
}

/// label → `seq`。不是贴图 label 就 `None`。
fn pin_id_of_label(label: &str) -> Option<u32> {
    if !is_pin_label(label) {
        return None;
    }
    label[PIN_LABEL_PREFIX.len()..].parse().ok()
}

/// 贴图窗口的绝对落点 = 那块屏的原点 + 选区在屏内的偏移。
///
/// ⚠ `capture::css_rect_to_physical` 出来的 `x`/`y` 是**屏内偏移**，不含 `mon.x`（见
/// `capture.rs` 那函数的注释：副屏的负原点已由「缓冲区原点 = 那块屏自己」消化掉了）。
/// 直接把 phys.x/phys.y 当绝对坐标用的话，主屏上看着完全正常，副屏贴图会跑到主屏左上角去 ——
/// 而截图本身是对的，这种偏最难归到这一步。
fn pin_origin(monitor: &MonitorRect, phys_x: i32, phys_y: i32) -> (i32, i32) {
    (monitor.x + phys_x, monitor.y + phys_y)
}

/// 把缩放倍数落成窗口的物理尺寸。
///
/// 1.0 那一档必须**逐像素等于图**：这是「贴图就是那块图本身」这条定义的全部内容，
/// 所以这里只做一次 `round`，不夹取也不做任何「看着差不多」。
/// 倍数不可用（0、负、NaN、8 倍以上）一律 `Err`：贴图缩成 0x0 就是 `snip.rs` 里少给
/// `SWP_NOSIZE` 那次的形状，别再犯一次。
fn pin_size(width: u32, height: u32, scale: f64) -> Result<(u32, u32), String> {
    if !scale.is_finite() || scale <= 0.0 || scale > 8.0 {
        return Err(format!("缩放倍数不可用（{scale}）"));
    }
    let w = (f64::from(width) * scale).round().max(1.0) as u32;
    let h = (f64::from(height) * scale).round().max(1.0) as u32;
    Ok((w, h))
}

/// 滚轮该落到哪一档。`dir > 0` 是放大。
///
/// 三条规矩：
/// - **两端夹住、不回绕**：回绕会让「再缩一档」在 0.25 那里突然变成 3.0，人看到的是图跳一下；
/// - **不在档位上的先回到最近一档**，这一次不消耗方向：页面正常只会传回 Rust 自己给过的那
///   几个数，走到这一支说明有东西错了；错的时候把人放到最近的一个真实档位上，比「吸附完再
///   顺手走一步」少吃掉一格、也比继续算下去更容易看出来；
/// - 倍数不是有限数就回 1.0（那是「逐像素等于图」那一档，最不容易把人绕进去）。
fn zoom_step(current: f64, dir: i32) -> f64 {
    if !current.is_finite() {
        return 1.0;
    }
    let mut best = 0usize;
    for (i, s) in ZOOM_STEPS.iter().enumerate() {
        if (s - current).abs() < (ZOOM_STEPS[best] - current).abs() {
            best = i;
        }
    }
    if (ZOOM_STEPS[best] - current).abs() > 1e-6 {
        return ZOOM_STEPS[best];
    }
    let next = if dir > 0 {
        (best + 1).min(ZOOM_STEPS.len() - 1)
    } else if dir < 0 {
        best.saturating_sub(1)
    } else {
        best
    };
    ZOOM_STEPS[next]
}

/// 放一份底图进槽，返回被挤出去的那个 id（没有就是 `None`）。
///
/// 一次调用只多塞一份，所以"超限"最多超一个 —— 这里写 `if` 而不是 `while`：
/// 那个循环永远不会真的转第二圈（clippy 的 `never_loop` 就是在说这件事），
/// 而「挤一份」是本模块的语义，不是「清到 cap 以下」。
fn slot_put(slots: &mut Vec<(u32, PinSlot)>, id: u32, slot: PinSlot, cap: usize) -> Option<u32> {
    slots.push((id, slot));
    if slots.len() > cap {
        let (old, _) = slots.remove(0);
        return Some(old);
    }
    None
}

/// 取一份底图的**副本**（不拿走）。
///
/// 为什么不学 `snip.rs` 的 `png_base64.take()`：那边「取走即空」是看门狗唯一的观察量，
/// 而覆盖层一条会话只加载一次。贴图窗口会重载（刷新、WebView2 崩了重来），取走即空的
/// 后果是「重载之后贴图变成白框」。回收改由窗口销毁触发（见 `on_destroyed`）。
fn slot_take(slots: &[(u32, PinSlot)], id: u32) -> Option<PinPayload> {
    slots
        .iter()
        .find(|(k, _)| *k == id)
        .map(|(k, s)| PinPayload {
            id: *k,
            png_base64: s.png_base64.clone(),
            width: s.width,
            height: s.height,
        })
}

fn slot_drop(slots: &mut Vec<(u32, PinSlot)>, id: u32) -> bool {
    let before = slots.len();
    slots.retain(|(k, _)| *k != id);
    slots.len() != before
}

/// 记下一张贴图的物理宽高（同 id 重写，不会积累两份）。
fn dim_put(dims: &mut Vec<(u32, (u32, u32))>, id: u32, width: u32, height: u32) {
    dims.retain(|(k, _)| *k != id);
    dims.push((id, (width, height)));
    if dims.len() > MAX_PIN_WINDOWS {
        dims.remove(0);
    }
}

fn dim_get(dims: &[(u32, (u32, u32))], id: u32) -> Option<(u32, u32)> {
    dims.iter().find(|(k, _)| *k == id).map(|(_, v)| *v)
}

fn dim_drop(dims: &mut Vec<(u32, (u32, u32))>, id: u32) -> bool {
    let before = dims.len();
    dims.retain(|(k, _)| *k != id);
    dims.len() != before
}

/// 当前活着的贴图窗口（`webview_windows()` 是唯一真相，不另建一份登记表）。
fn live_pins(app: &AppHandle) -> Vec<(String, WebviewWindow)> {
    app.webview_windows()
        .into_iter()
        .filter(|(label, _)| is_pin_label(label))
        .collect()
}

/// 把窗口摆成给定尺寸，并**量回来**。
///
/// 量不是形式主义：`state.rs` 记着 WebView2 创建控制器时会把窗口强制放宽到至少 120px。
/// 两侧偏差分开处理，因为它们是两种坏：
/// - **比请求小**＝图被切掉一块，那是真故障，`warn`；
/// - **比请求大**（就是上面那 120px 地板）＝多出一条底色边，图本身仍然 1:1
///   （页面按图像物理尺寸画，不铺满视口，见 pin.js），所以只 `debug`，
///   免得每滚一次轮就刷一行 warn。
fn apply_pin_size(win: &WebviewWindow, width: u32, height: u32) {
    if let Err(e) = win.set_size(PhysicalSize::new(width, height)) {
        tracing::warn!("贴图窗口设尺寸失败：{e}");
        return;
    }
    let got = match win.inner_size() {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!("读不到贴图窗口客户区尺寸：{e}");
            return;
        }
    };
    if got.width == width && got.height == height {
        return;
    }
    if got.width < width || got.height < height {
        tracing::warn!(
            "贴图窗口客户区 {}x{} 比请求的 {width}x{height} 小，图会被切一块",
            got.width,
            got.height
        );
    } else {
        tracing::debug!(
            "贴图窗口客户区 {}x{} 比请求的 {width}x{height} 宽（WebView2 的窗口最小宽度地板），\
             多出来的是底色边，图仍按原尺寸画",
            got.width,
            got.height
        );
    }
}

/// 在主线程建出贴图窗口。**只能由 `open()` 经 `run_on_main_thread` 调进来。**
///
/// 建窗要回主线程，且不能在主线程的派发栈里同步建（`state.rs:581-586` 记着那会挂死），
/// 所以调用链固定是「后台线程 → 等退栈 → run_on_main_thread → build」，与覆盖层同一条。
///
/// ⚠ 第二个参数的写法有讲究：`lib.rs` 的窗口审计按 `format!("{PIN_LABEL_PREFIX}{…}")`
/// 这个形状解出动态 label（解不出就 panic 而不是静默漏掉）。换成先 `let label = …`
/// 再把变量传进来，那条审计就看不见这个建窗点了。
fn build_pin(app: &AppHandle, seq: u32, x: i32, y: i32) -> Result<WebviewWindow, String> {
    let win = tauri::WebviewWindowBuilder::new(
        app,
        format!("{PIN_LABEL_PREFIX}{seq}"),
        tauri::WebviewUrl::App(PIN_PAGE.into()),
    )
    .title("FocusFlow 贴图")
    // 占位尺寸，随后由取图那一步按图像物理尺寸定形；此刻仍不可见，不会闪一个错尺寸的框。
    .inner_size(240.0, 180.0)
    .resizable(false)
    .maximizable(false)
    .minimizable(false)
    .visible(false)
    .decorations(false)
    // 这里不要开透明，理由见模块头第 2 条（panic=abort 那次闪退）。
    .always_on_top(true)
    // 只走 `skip_taskbar`（tauri/tao 用的是 ITaskbarList::DeleteTab，不是 WS_EX_TOOLWINDOW），
    // 不手动加工具窗样式：与 `snip.rs` 的 `raise_topmost` 同一个取舍 —— 工具窗口可能被拒绝
    // 前台激活，而贴图要收得到 Esc。代价是它会出现在 Alt+Tab 里。
    .skip_taskbar(true)
    .focused(true)
    .additional_browser_args(crate::state::WEBVIEW_BROWSER_ARGS)
    .build()
    .map_err(|e| format!("创建贴图窗口失败：{e}"))?;

    if let Err(e) = win.set_position(PhysicalPosition::new(x, y)) {
        return Err(format!("设置贴图位置失败：{e}"));
    }
    win.show().map_err(|e| format!("显示贴图失败：{e}"))?;
    if let Err(e) = win.set_focus() {
        // 拿不到键盘不算失败：窗口右上角的 ✕ 是不依赖键盘的退路。
        tracing::warn!("贴图未能取得焦点（Esc 可能要先点一下才生效）：{e}");
    }
    Ok(win)
}

/// 开一张贴图。**必须在非主线程调用**（现调用点是 `snip_commit` 的 `spawn_blocking` 线程）。
///
/// 传进来的是「选区在那块屏里的偏移」(`phys`) 与「那块屏本身」(`monitor`)，绝对落点在这里
/// 算（见 `pin_origin`）：坐标空间的那一步收进本模块，调用方就没机会忘了加原点。
///
/// 里面有一次 `recv_timeout`：主线程建完把结果送回来自报成功失败，「已贴图」这句话才有依据。
/// 之所以敢等：调用线程不是主线程。真被人在主线程调进来，最坏是白等一个 `BUILD_DEADLINE`
/// 再收到一句超时的 `Err`，不是死锁。
pub fn open(
    app: &AppHandle,
    png: &[u8],
    phys: &MonitorRect,
    monitor: &MonitorRect,
) -> Result<u32, String> {
    let alive = live_pins(app).len();
    if alive >= MAX_PIN_WINDOWS {
        return Err(format!(
            "已经有 {alive} 张贴图，先关掉一些（上限 {MAX_PIN_WINDOWS} 张）"
        ));
    }
    let (width, height) = (phys.width, phys.height);
    let (x, y) = pin_origin(monitor, phys.x, phys.y);

    let seq = PIN_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    let slot = PinSlot {
        png_base64: base64::engine::general_purpose::STANDARD.encode(png),
        width,
        height,
    };
    let evicted = {
        let mut slots = PENDING.lock().map_err(|_| "贴图槽锁不可用")?;
        slot_put(&mut slots, seq, slot, MAX_PIN_IMAGES)
    };
    {
        let mut dims = DIMS.lock().map_err(|_| "贴图尺寸账锁不可用")?;
        dim_put(&mut dims, seq, width, height);
    }
    if let Some(old) = evicted {
        tracing::warn!(
            "贴图槽满了 {MAX_PIN_IMAGES} 份，收回最老的 {} 的底图（那张窗口若重新加载会取不到图，\
             但已经画出来的那张照样能拖、能缩放）",
            pin_label(old)
        );
    }

    let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let handle = app.clone();
    let t_build = std::time::Instant::now();
    if let Err(e) = app.run_on_main_thread(move || {
        let result = build_pin(&handle, seq, x, y).map(|win| {
            apply_pin_size(&win, width, height);
        });
        if let Err(e) = tx.send(result) {
            // 通道挂了只可能是调用方已经不在了（进程正在退）：窗口留给系统收，别再重试。
            tracing::warn!("贴图窗口建完但结果送不回去：{e:?}");
        }
    }) {
        let mut slots = PENDING.lock().map_err(|_| "贴图槽锁不可用")?;
        slot_drop(&mut slots, seq);
        if let Ok(mut dims) = DIMS.lock() {
            dim_drop(&mut dims, seq);
        }
        return Err(format!("无法把建贴图派发到主线程：{e}"));
    }
    let built = rx
        .recv_timeout(BUILD_DEADLINE)
        .map_err(|e| format!("等主线程建贴图窗口没有回音（{e}）"))
        .and_then(|r| r);
    if let Err(e) = built {
        let mut slots = PENDING.lock().map_err(|_| "贴图槽锁不可用")?;
        slot_drop(&mut slots, seq);
        if let Ok(mut dims) = DIMS.lock() {
            dim_drop(&mut dims, seq);
        }
        return Err(e);
    }

    tracing::info!(
        "贴图已开窗：{}（{}x{} 物理 @ ({},{})，PNG {} 字节，建窗 {} ms，当前活着 {} 张）",
        pin_label(seq),
        width,
        height,
        x,
        y,
        png.len(),
        t_build.elapsed().as_millis(),
        live_pins(app).len()
    );
    Ok(seq)
}

/// 页面启动时来取自己的底图。**id 取自调用方窗口的 label，不从页面传参** ——
/// 传参的话任何窗口都能声称自己是 `pin-7`、把别人的图拿走。
///
/// 为什么是 `async`：返回的那份 base64 有几 MB，克隆加序列化不能压在 `commands::with_manager`
/// 注释里说的那个主线程上（同步命令跑在主线程）。同量的 `snip_take` 早就是 async，同一个理由。
#[tauri::command]
pub async fn pin_take(win: WebviewWindow) -> Result<PinPayload, String> {
    let id =
        pin_id_of_label(win.label()).ok_or_else(|| format!("不是贴图窗口：{}", win.label()))?;
    let payload = {
        let slots = PENDING.lock().map_err(|_| "贴图槽锁不可用")?;
        slot_take(&slots, id).ok_or_else(|| {
            format!("贴图 {id} 的底图不在了（超过 {MAX_PIN_IMAGES} 张时被收回，或窗口已被关闭）")
        })?
    };
    // 这一次才是真正确定形状的地方：页面都跑起来了，说明控制器已建好，
    // 而 WebView2 那个强制放宽正是发生在控制器创建那一刻（见 `apply_pin_size`）。
    //
    // 摆尺寸要回主线程（`show_overlay` 就是这个写法），而这份 base64 的**克隆与序列化留给
    // async 运行时**：几 MB 压在同步命令上就是压在主线程（`plugins.rs` 那条注释写明了
    // 「Tauri 同步命令在主线程」）。两个诉求拆开，各走自己那条在本仓已经跑通过的路。
    let w = win.clone();
    let (width, height) = (payload.width, payload.height);
    if let Err(e) = win.run_on_main_thread(move || apply_pin_size(&w, width, height)) {
        // 摆不成不算失败：建窗那一次已经按同样的尺寸摆过，页面照样有图可画。
        tracing::warn!("贴图取图时重申尺寸没派发到主线程：{e}（沿用建窗时那一次）");
    }
    tracing::info!(
        "贴图已取图：{}（{}x{}，base64 {} 字节）",
        win.label(),
        payload.width,
        payload.height,
        payload.png_base64.len()
    );
    Ok(payload)
}

/// 滚轮缩放：方向由页面给，档位表与尺寸都由这里算。
///
/// 为什么不放页面自己去 `setSize`：那要给 `capabilities/default.json` 加
/// `core:window:allow-set-size`，而那份能力是 `main`/`floating`/`snip` 共用的 ——
/// 为贴图的缩放给所有窗口开一项写权限不划算。档位表也只在这边留一份。
///
/// 尺寸从 `DIMS` 读而不是从 `PENDING` 读：这里要的只是那两个数。读像素槽的话，
/// 「底图超过 `MAX_PIN_IMAGES` 份被收回」会连带把**还活着**的那扇窗口的滚轮判成失败，
/// 而模块头对那次收回的承诺是「已经画出来的窗口不受影响」。
#[tauri::command]
pub fn pin_resize(win: WebviewWindow, scale: f64, dir: i32) -> Result<(f64, u32, u32), String> {
    let id =
        pin_id_of_label(win.label()).ok_or_else(|| format!("不是贴图窗口：{}", win.label()))?;
    let (width, height) = {
        let dims = DIMS.lock().map_err(|_| "贴图尺寸账锁不可用")?;
        dim_get(&dims, id)
            .ok_or_else(|| format!("贴图 {id} 的尺寸账不在了（窗口已关，或建窗那一步没成）"))?
    };
    let applied = zoom_step(scale, dir);
    let (w, h) = pin_size(width, height, applied)?;
    apply_pin_size(&win, w, h);
    Ok((applied, w, h))
}

/// 关掉当前这张贴图。`close()` 失败就退成 `hide()` 并出声 —— 一张关不掉的贴图比丢一张
/// 截图烦人得多：它一直盖在桌面上。
///
/// 这是**页面里 ✕ 与 Esc 走的那条**，另外两条不依赖页面的退路是 Alt+F4（本模块不拦关闭，
/// 见 `on_destroyed` 的说明）与 `pin_close_all`。
#[tauri::command]
pub fn pin_close(win: WebviewWindow) -> Result<(), String> {
    let label = win.label().to_string();
    match win.close() {
        Ok(()) => Ok(()),
        Err(e) => {
            if let Err(e2) = win.hide() {
                tracing::error!("关不掉 {label}，连隐藏也失败：{e2}（原始错误：{e}）");
                return Err(format!("关不掉 {label}：{e}"));
            }
            tracing::error!("关不掉 {label}，已先藏起来（原始错误：{e}）");
            Err(format!("{label} 关不掉，已隐藏：{e}"))
        }
    }
}

/// 关掉所有贴图：设置页上一条**不依赖任何贴图页面**的出口。
///
/// 留这条的理由正是「关不掉怎么办」那一问：某张贴图的脚本挂了、✕ 点不动的时候，
/// 主窗口里还得有一个能把桌面清空的地方。
#[tauri::command]
pub fn pin_close_all(app: AppHandle) -> usize {
    let pins = live_pins(&app);
    let n = pins.len();
    for (label, win) in pins {
        if let Err(e) = win.close() {
            tracing::error!("关掉贴图 {label} 失败：{e}");
        }
    }
    tracing::info!("贴图已全部关闭（请求 {n} 张）");
    n
}

/// 贴图窗口真被销毁时的收尾：收回底图，并记下还剩几张。
///
/// 覆盖层那边必须 `prevent_close`（懒创建的常驻窗口销毁后再建控制器不可靠），这里**不**拦：
/// 贴图每次都是新 label、走的是覆盖层已经验证过的那条建窗路，销毁之后可以重建。
pub fn on_destroyed(app: &AppHandle, label: &str) {
    let Some(id) = pin_id_of_label(label) else {
        return;
    };
    if let Ok(mut slots) = PENDING.lock() {
        slot_drop(&mut slots, id);
    }
    if let Ok(mut dims) = DIMS.lock() {
        dim_drop(&mut dims, id);
    }
    tracing::info!(
        "贴图已关闭：{label}（还活着 {} 张，图已经在盘上，没有未保存的东西）",
        live_pins(app).len()
    );
}

/// 进程要退出时的一句账：贴图窗口里没有任何「没存下来的东西」（模块头第 1 条），
/// 所以这里只出声、不挽留、不阻塞退出。
pub fn log_on_quit(app: &AppHandle) {
    let n = live_pins(app).len();
    if n > 0 {
        tracing::info!("退出程序时仍有 {n} 张贴图：每张都已落盘，窗口随进程一起关闭");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(x: i32, y: i32) -> MonitorRect {
        MonitorRect::new(x, y, 1920, 1080).unwrap()
    }

    #[test]
    fn labels_are_prefix_plus_digits_only() {
        assert_eq!(pin_label(7), "pin-7");
        assert!(is_pin_label("pin-7"));
        assert!(is_pin_label("pin-12"));
        assert!(!is_pin_label("pin-"), "光有前缀不算");
        assert!(!is_pin_label("pin-7x"), "尾巴带字母不算");
        assert!(!is_pin_label("snip"), "别的窗口不能被前缀误伤");
        assert!(!is_pin_label("main"));
        assert_eq!(pin_id_of_label("pin-12"), Some(12));
        assert_eq!(pin_id_of_label("pin-7x"), None);
        assert_eq!(pin_id_of_label("pin-"), None);
    }

    /// 前缀、capabilities 里那条通配、页面文件是三处「字符串约定」。
    /// 错一个的症状与当年 snip 一样（窗口建出来了、调什么都被拒），所以钉在可验的期。
    #[test]
    fn prefix_agrees_with_capability_pattern_and_page_exists() {
        let cap = include_str!("../capabilities/default.json");
        let at = cap
            .find("\"windows\"")
            .expect("capabilities 里没有 windows 字段");
        let inner = cap[at..]
            .split_once('[')
            .and_then(|(_, r)| r.split_once(']'))
            .map(|(i, _)| i.to_string())
            .expect("windows 不是数组");
        let patterns: Vec<&str> = inner.split('"').skip(1).step_by(2).collect();
        let glob = patterns
            .iter()
            .find(|p| p.ends_with("-*"))
            .unwrap_or_else(|| panic!("capabilities 的 windows 里没有任何 `xxx-*` 通配项"));
        assert_eq!(
            *glob,
            &format!("{PIN_LABEL_PREFIX}*"),
            "capabilities 里的通配与前缀对不上：改前缀要同步改 capabilities"
        );
        // 生成的 label 真落在那条通配里（按前缀自己数一遍，不依赖 glob 库）
        let head = glob.trim_end_matches('*');
        assert!(pin_label(3).starts_with(head), "pin-3 不在 {glob} 里");

        let page = include_str!("../ui/pin.html");
        assert!(
            page.contains("pin.js"),
            "pin.html 没加载 pin.js，页面只是个空框"
        );
    }

    /// 副屏负原点：绝对落点必须加过那块屏的原点。
    /// 忘了加的症状是「在副屏截的图贴出来跑到主屏左上角」，而截图本身是对的。
    #[test]
    fn origin_adds_the_monitor_corner() {
        assert_eq!(pin_origin(&mon(0, 0), 120, 240), (120, 240));
        assert_eq!(
            pin_origin(&mon(-1920, 0), 10, 20),
            (-1910, 20),
            "副屏负原点要落回副屏"
        );
        assert_eq!(pin_origin(&mon(0, 1080), 5, 7), (5, 1087));
    }

    #[test]
    fn full_scale_is_the_image_itself() {
        assert_eq!(pin_size(1200, 800, 1.0).unwrap(), (1200, 800));
        // 1.0 之外只做一次 round，不夹取
        assert_eq!(pin_size(1201, 401, 0.5).unwrap(), (601, 201));
        assert_eq!(pin_size(40, 20, 3.0).unwrap(), (120, 60));
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY, 9.0] {
            assert!(pin_size(100, 100, bad).is_err(), "倍数 {bad} 不可用还不报");
        }
    }

    #[test]
    fn zoom_clamps_at_both_ends_and_snaps_off_ladder_values() {
        assert_eq!(zoom_step(1.0, 1), 1.5);
        assert_eq!(zoom_step(1.5, 1), 2.0);
        assert_eq!(zoom_step(1.0, -1), 0.75);
        assert_eq!(zoom_step(3.0, 1), 3.0, "顶上不回绕");
        assert_eq!(zoom_step(0.25, -1), 0.25, "底下不回绕");
        // 不在档位上：先回到最近的一档，这一次不消耗方向（两个动作不叠一起做）
        assert_eq!(zoom_step(1.2, 1), 1.0, "不在表上的先吸附到最近一档");
        assert_eq!(zoom_step(1.2, -1), 1.0, "吸附那一趟不分方向");
        assert_eq!(
            zoom_step(1.0000001, 1),
            1.5,
            "档位上的浮点噪声不该被当成离档"
        );
        assert_eq!(zoom_step(f64::NAN, 1), 1.0);
        assert_eq!(zoom_step(1.0, 0), 1.0, "没方向就该原地不动");
    }

    #[test]
    fn take_keeps_the_image_so_a_reload_still_works() {
        let mut slots = Vec::new();
        let s = || PinSlot {
            png_base64: "AAA".to_string(),
            width: 10,
            height: 20,
        };
        assert!(slot_put(&mut slots, 1, s(), 8).is_none());
        assert!(slot_take(&slots, 1).is_some());
        assert!(
            slot_take(&slots, 1).is_some(),
            "取过一次之后还在：重载要还能拿到图"
        );
        assert!(slot_take(&slots, 2).is_none(), "未知 id 必须报没有");
        assert!(slot_drop(&mut slots, 1));
        assert!(!slot_drop(&mut slots, 1), "重复收尾不该报错");
        assert!(slot_take(&slots, 1).is_none(), "窗口销毁后底图要收回");
    }

    #[test]
    fn slot_cap_evicts_the_oldest_one_at_a_time() {
        let mut slots = Vec::new();
        let s = |n: u32| PinSlot {
            png_base64: n.to_string(),
            width: n,
            height: n,
        };
        assert!(slot_put(&mut slots, 1, s(1), 3).is_none());
        assert!(slot_put(&mut slots, 2, s(2), 3).is_none());
        assert!(slot_put(&mut slots, 3, s(3), 3).is_none());
        assert_eq!(slots.len(), 3, "刚到上限不该丢");
        assert_eq!(slot_put(&mut slots, 4, s(4), 3), Some(1), "满了该丢最老的");
        assert_eq!(slot_take(&slots, 4).unwrap().png_base64, "4");
        assert!(slot_take(&slots, 1).is_none());
        assert_eq!(slots.len(), 3, "一次只挤掉一份");
    }

    /// 挤掉最老那份**像素**时，尺寸账必须留着：模块头对那次回收的承诺是「已经画出来的
    /// 窗口不受影响」，而滚轮缩放读的要是像素槽，那么贴图开到第 9 张起，最老那几扇还
    /// 活着的窗口一滚滚轮就只会得到一句「底图不在了」——图明明还在桌上。
    #[test]
    fn evicted_pixels_leave_the_size_ledger_behind() {
        let mut slots = Vec::new();
        let mut dims = Vec::new();
        let s = |n: u32| PinSlot {
            png_base64: "AAA".to_string(),
            width: n,
            height: n / 2,
        };
        for i in 1..=4u32 {
            slot_put(&mut slots, i, s(i * 200), 3);
            dim_put(&mut dims, i, i * 200, i * 100);
        }
        assert!(
            slot_take(&slots, 1).is_none(),
            "夹具前提：第 1 份像素已经被挤掉了"
        );
        let (w, h) = dim_get(&dims, 1).expect("尺寸账不该跟着像素一起没了");
        assert_eq!((w, h), (200, 100));
        // 缩放要算的就是「档位 × 图像尺寸」，那几 MB 的 base64 全程没参与
        let applied = zoom_step(1.0, 1);
        assert_eq!(pin_size(w, h, applied).unwrap(), (300, 150));

        // 账本自己的天花板按窗口数计，且从最老一条起丢 —— `open()` 在已经有
        // `MAX_PIN_WINDOWS` 扇活着时就拒了，所以正常路径丢不到还在用的那扇。
        for i in 5..=(MAX_PIN_WINDOWS as u32 + 4) {
            dim_put(&mut dims, i, 10, 10);
        }
        assert_eq!(dims.len(), MAX_PIN_WINDOWS);
        assert!(dim_get(&dims, 1).is_none(), "账本满了也从最老的那条起丢");
        assert!(dim_get(&dims, MAX_PIN_WINDOWS as u32 + 4).is_some());
        assert!(dim_drop(&mut dims, 5), "窗口销毁要把这条一起带走");
        assert!(!dim_drop(&mut dims, 5), "重复收尾不该报错");
    }

    /// `pin_resize` 只许读尺寸账。回到读 `PENDING` 的话上面那条夹具就管不住它了 ——
    /// 「底图被收回」与「这扇窗口的滚轮坏了」之间只差一行，而命令本体要真窗口才跑得动，
    /// 所以按文本形状钉（与 `pin_windows_are_opaque_and_never_touch_the_disk` 同族）。
    #[test]
    fn pin_resize_reads_the_size_ledger_not_the_pixels() {
        let all = code_only(include_str!("pin.rs"));
        let prod = &all[..all.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let body = &prod[prod.find("fn pin_resize").expect("缩放函数找不到了")
            ..prod.find("fn pin_close").expect("下一条命令的起点找不到了")];
        assert!(body.contains("dim_get("), "缩放没在读尺寸账：{body}");
        assert!(
            !body.contains("PENDING"),
            "缩放又去读像素槽了：底图被收回时滚轮会跟着一起坏"
        );
        assert!(
            !body.contains("png_base64"),
            "缩放不需要那几个字节的 base64"
        );
    }

    /// 「同步」是最顺手也最容易做错的改法：`open()` 里挤掉一份像素时，顺手把那条尺寸
    /// 也 `dim_drop` 掉，两份账就重新变成一个了 —— 于是第 9 张贴图一滚滚轮就报错。
    /// 这条盯的是回收那一段，而不是 `pin_resize` 那一段（上一条管不到这里）。
    #[test]
    fn evicting_a_slot_does_not_evict_its_size_entry() {
        let all = code_only(include_str!("pin.rs"));
        let prod = &all[..all.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let at = prod
            .find("if let Some(old) = evicted {")
            .expect("回收底图那一段找不到了（判据要跟着改）");
        let evict = &prod[at..prod[at..]
            .find("let (tx, rx)")
            .expect("派发建窗那一段的起点")
            + at];
        assert!(
            evict.contains("tracing::warn"),
            "切片是空的或对错了位置，这条就成了假绿：{evict}"
        );
        assert!(
            !evict.contains("dim_drop"),
            "挤像素的那一段动到尺寸账了：还活着的窗口会连缩放一起失去依据"
        );
    }

    /// 去掉行注释（`//` 与 `///` 都在内）：本模块的模块头写的就是「不许出现 `fs::write`」「不要
    /// 开透明」，连注释一起读会**自己匹配自己** —— 与本仓 `lib.rs` 那条窗口审计同一个处理。
    fn code_only(src: &str) -> String {
        src.lines()
            .map(|l| match l.find("//") {
                Some(i) => &l[..i],
                None => l,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// **贴图窗口不许是透明窗口**（模块头第 2 条那次闪退），以及**本模块一行盘都不写**
    /// （模块头第 1 条那条「贴图不多落一个文件」）。
    ///
    /// 只能按文本形状钉：前者要真窗口才验得出；后者写出来就是「没有报错、盘上多一张图」
    /// 那种谁都不说话的坏。只读 `#[cfg(test)]` 之前那一段，且先去掉注释。
    #[test]
    fn pin_windows_are_opaque_and_never_touch_the_disk() {
        let all = code_only(include_str!("pin.rs"));
        let prod = &all[..all.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let body = &prod[prod.find("fn build_pin").expect("建窗函数找不到了")
            ..prod.find("fn open(").expect("open 函数找不到了")];

        assert!(
            !body.contains("transparent("),
            "贴图窗口被加了透明：Windows 上每个透明窗口都会建 softbuffer 表面并 unwrap，\
             而本包 release 是 panic=abort，一失败就是整个进程闪退"
        );
        assert!(body.contains(".always_on_top(true)"), "贴图要盖在别人上面");
        assert!(body.contains(".skip_taskbar(true)"), "贴图不该占任务栏");
        assert!(
            body.contains(".decorations(false)"),
            "贴图没有标题栏：拖拽与关闭都在页面里"
        );
        assert!(
            body.contains(".visible(false)"),
            "建窗要先不可见，摆好位置尺寸再 show（覆盖层那次 0x0 就是这么避开的）"
        );

        assert!(
            !prod.contains("fs::write"),
            "贴图模块里出现了一次写盘 —— 贴图算的是「截图的一个出口」，\
             文件只该由提交那一步产出，不然一次手势落两张"
        );
    }

    /// 建窗点的 label 是动态的，而 `lib.rs` 的窗口审计只认这一个形状。
    /// 这条盯的是「换写法之后审计静默失去覆盖面」。
    #[test]
    fn builder_label_is_the_shape_the_window_audit_resolves() {
        let all = code_only(include_str!("pin.rs"));
        let prod = &all[..all.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let body = &prod[prod.find("fn build_pin").expect("建窗函数找不到了")
            ..prod.find("fn open(").expect("open 函数找不到了")];
        assert!(
            body.contains("format!(\"{PIN_LABEL_PREFIX}{seq}\")"),
            "建窗的 label 不是 `format!(\"{{PIN_LABEL_PREFIX}}{{seq}}\")` 这个形状，\
             `lib.rs` 的建窗扫描解不出来 —— 那条审计会 panic，而不是报「没登记」"
        );
    }
}
