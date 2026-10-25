//! 悬浮窗的**门面**：两种渲染器（WebView2 / 原生分层窗口）共用一张入口表。
//!
//! 为什么需要这一层：`tauri.conf.json` 里那张 `windows` 表是静态的 —— 只要 `floating`
//! 写在里面，Tauri 就在启动时无条件建出那个 WebView2 窗口，`[floating] enabled=false`
//! 也只是"建出来再藏"（上一轮实测：关掉悬浮窗仍然 6 个渲染进程、129 MB 私有提交）。
//! 所以原生化必须同时把配置里的声明摘掉、改成由这里按开关建窗，否则 native 模式一点内存
//! 都省不下来。
//!
//! ⚠ 原生窗口**不在** Tauri 的窗口集合里：任何 `app.get_webview_window("floating")`
//! 在 native 模式下都拿到 None，于是那条支路静默失效（不报错、不日志、功能没了）。
//! 读悬浮窗的地方一律走本模块；判据
//! `tests::no_caller_reaches_for_the_floating_webview_directly` 盯着这件事。
//!
//! 开关：`[gui] floating_renderer = webview | native`，改了要重启。
//! 不做运行期热切换：那意味着同一时刻存在两张卡片，而"哪一张是真的"这个问题没人答得起
//! （托盘、设置页、截图前藏窗恢复的都是 label，不是渲染器）。

mod native;
pub(crate) mod render;

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Mutex;

use tauri::{AppHandle, Emitter, Manager};

use crate::state::{AppState, LiveStats};

pub use render::notice_text;

/// 窗口 label。WebView2 那一支用它建窗、用它找窗；capabilities 里必须同名。
pub const FLOATING_LABEL: &str = "floating";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Renderer {
    Webview,
    Native,
}

impl Renderer {
    fn as_u8(self) -> u8 {
        match self {
            Renderer::Webview => 1,
            Renderer::Native => 2,
        }
    }

    fn from_u8(v: u8) -> Option<Renderer> {
        match v {
            1 => Some(Renderer::Webview),
            2 => Some(Renderer::Native),
            _ => None,
        }
    }
}

/// 读 `[gui] floating_renderer`。**认不出的值一律按 webview**：
/// 那是一直以来能用的那一套，写错一个字不该让用户的悬浮窗直接消失。
pub fn renderer_of(cfg: &focusflow_core::config::FocusFlowConfig) -> Renderer {
    match cfg
        .get("gui", "floating_renderer")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "native" => Renderer::Native,
        _ => Renderer::Webview,
    }
}

/// 本次运行**实际**在用的渲染器。原生那一套建不起来会退回 WebView2，
/// 这时配置里仍然写着 native，而所有后续动作必须按实际的那一套走 ——
/// 所以这一格是"生效值"，不是配置的复读。
static ACTIVE: AtomicU8 = AtomicU8::new(0);

fn active_renderer() -> Renderer {
    Renderer::from_u8(ACTIVE.load(Ordering::SeqCst))
        // setup 之前（或没跑过 setup 的测试里）按配置答
        .unwrap_or_else(|| renderer_of(focusflow_core::config::instance()))
}

// ===== parked 数据：统计线程写，悬浮窗线程读 =====
//
// 为什么不用事件：WebView2 那一支靠 `emit_to("floating", "stats-live")` 推，而推这件事
// 要求窗口在 Tauri 的注册表里；原生窗口不在，事件发过去就是"没人收"。
// 读同一格比再造一条 IPC 少一处会坏的地方，而且隐藏时原生窗口本来就不画。

static LIVE: Mutex<Option<LiveStats>> = Mutex::new(None);
static PAUSED: AtomicBool = AtomicBool::new(false);

/// 统计线程在内容有变化时挂一次。
pub fn publish_live(live: &LiveStats) {
    *LIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(live.clone());
}

pub fn live_snapshot() -> Option<LiveStats> {
    LIVE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// 暂停态由 `AppState::toggle_pause` 写（所有入口都走那一个，见它的注释）。
pub fn publish_paused(paused: bool) {
    PAUSED.store(paused, Ordering::SeqCst);
}

pub fn paused_now() -> bool {
    PAUSED.load(Ordering::SeqCst)
}

/// 久坐提醒那一句：读托盘用的同一格（过期就没有）。两种渲染器因此念同一句话，
/// 而不是"webview 收事件、原生自己再算一遍"。
pub fn rest_notice_text() -> Option<String> {
    let n = crate::state::rest_notice_now()?;
    Some(notice_text(n.window_minutes, n.rest_seconds))
}

/// 原生窗口线程退出后留一行话。卡片从此不再更新，而这件事只有日志能说明白。
fn note_thread_exit() {
    tracing::info!("原生悬浮窗线程已退出，卡片不再更新（重新打开悬浮窗可恢复）");
}

/// 启动时按开关建窗。**唯一**的建窗入口。
pub fn setup(app: &tauri::App, state: &AppState) {
    let cfg = focusflow_core::config::instance();
    publish_paused(state.listener.is_paused());
    match renderer_of(cfg) {
        Renderer::Native => {
            let handle = app.handle().clone();
            match native::spawn(handle, "FocusFlow 悬浮窗") {
                Ok(()) => {
                    ACTIVE.store(Renderer::Native.as_u8(), Ordering::SeqCst);
                    if cfg.get_bool("floating", "enabled", true) {
                        native::set_visible(true);
                    }
                    tracing::info!("悬浮窗已用原生分层窗口创建（floating_renderer=native）");
                }
                Err(e) => {
                    // 建不起来就退回 WebView2，而不是留一块空白：用户勾着"显示悬浮窗"
                    // 却什么都看不见，是这个开关最难查的坏法。
                    tracing::error!("原生悬浮窗创建失败，退回 WebView2 那一套：{e}");
                    ACTIVE.store(Renderer::Webview.as_u8(), Ordering::SeqCst);
                    build_webview_pill(&app.handle().clone(), cfg);
                }
            }
        }
        Renderer::Webview => {
            ACTIVE.store(Renderer::Webview.as_u8(), Ordering::SeqCst);
            build_webview_pill(&app.handle().clone(), cfg);
        }
    }
}

/// 建 WebView2 那一支的悬浮窗：把 `tauri.conf.json` 原先替它供的字段逐项补齐。
///
/// 少一项都是"看着对、其实坏"：漏 `visible:false` 会在启动时闪一个默认尺寸的框；
/// 漏 `title` 变成 "Tauri App"；漏 `transparent` 卡片就没有圆角外的透明边；
/// 漏 `additional_browser_args` 而它又恰好是**第一个**建出来的窗口，
/// 整套浏览器参数就以 wry 的默认值立起来，主窗口/覆盖层后面写的同一份参数会被忽略
/// （wry 是替换而非追加）。
fn build_webview_pill(app: &AppHandle, cfg: &focusflow_core::config::FocusFlowConfig) {
    if app.get_webview_window(FLOATING_LABEL).is_some() {
        return; // 已经建过：重复建同名 label 只会报错，报的还是"窗口已存在"这种莫名话
    }
    let w = cfg.get_float("floating", "width", 90.0).max(40.0);
    let h = cfg.get_float("floating", "height", 46.0).max(24.0);
    let built = tauri::WebviewWindowBuilder::new(
        app,
        FLOATING_LABEL,
        tauri::WebviewUrl::App("floating.html".into()),
    )
    .title("FocusFlow 悬浮窗")
    .inner_size(w, h)
    .resizable(false)
    .maximizable(false)
    .minimizable(false)
    .decorations(false)
    .transparent(true)
    .always_on_top(true)
    .skip_taskbar(true)
    .visible(false)
    .focused(true)
    .additional_browser_args(crate::state::WEBVIEW_BROWSER_ARGS)
    .build();
    let win = match built {
        Ok(win) => win,
        Err(e) => {
            // 以前这张卡片由配置声明，build 失败会一路 `?` 出 setup() 让所有人看见；
            // 懒创建之后失败只剩一行日志 + 设置页那个还勾着的勾（它读的是配置，不是现场）。
            // 所以这一句必须点名的东西比日志多：用户能做什么。
            tracing::error!(
                "创建悬浮窗失败：{e}（设置页的勾选仍然写着开着，重新打开一次悬浮窗会再试）"
            );
            return;
        }
    };
    crate::state::finish_floating_setup(&win, cfg);
    if cfg.get_bool("floating", "enabled", true) {
        set_visible(app, true);
    }
}

/// 显示/隐藏悬浮窗（两种渲染器同一个入口）。
/// 返回 false = 窗口压根不存在（建窗失败过），调用方要把这件事说出去，
/// 而不是留一次"点了没反应"。
pub fn set_visible(app: &AppHandle, visible: bool) -> bool {
    let ok = match active_renderer() {
        Renderer::Native => native::set_visible(visible),
        Renderer::Webview => match app.get_webview_window(FLOATING_LABEL) {
            Some(win) => {
                if visible {
                    // 先抬渲染再显示：反过来的话 WebView2 不会重绘，用户看见一块黑
                    crate::state::set_webview_rendering(app, FLOATING_LABEL, true);
                    let _ = win.show();
                    let _ = win.set_focus();
                } else {
                    let _ = win.hide();
                    crate::state::set_webview_rendering(app, FLOATING_LABEL, false);
                }
                true
            }
            None => false,
        },
    };
    // 广播给设置页那个勾选：入口有三个（勾选、页内按钮、托盘菜单），
    // 不广播就会互相看不见 —— 从托盘切完之后，页上的勾和屏幕上有没有卡片对不上。
    let _ = app.emit("floating-changed", visible);
    if !ok {
        tracing::warn!("悬浮窗不存在，这次可见性切换没做成 (visible={visible})");
    }
    ok
}

/// 悬浮窗此刻在不在眼前。
pub fn is_visible(app: &AppHandle) -> bool {
    match active_renderer() {
        Renderer::Native => native::is_visible(),
        Renderer::Webview => app
            .get_webview_window(FLOATING_LABEL)
            .map(|w| w.is_visible().unwrap_or(false))
            .unwrap_or(false),
    }
}

/// 统计线程该不该往 `"floating"` 推事件：**只有 WebView2 那一支收得到事件**。
/// 不问这一句就照 `is_visible()` 推，每条 emit 都在往一个不存在的窗口发，
/// 而那一族的失败历史上是被 `let _ =` 咽下去的。
pub fn pushes_events() -> bool {
    matches!(active_renderer(), Renderer::Webview)
}

/// 截图前藏掉悬浮窗，返回"这张卡片现在被藏好了、结束时请恢复它"。
/// 结束时必须**原样**恢复，而不是按 `[floating] enabled` 恢复：用户完全可以
/// 关着悬浮窗截图，也可以开着截图，两种都要回到原样。
/// 藏失败时答 false —— 它没被藏起来，收尾就不该再去 show() 一个一直亮着的窗口。
pub fn hide_for_capture(app: &AppHandle) -> bool {
    if !is_visible(app) {
        return false;
    }
    match active_renderer() {
        Renderer::Native => {
            native::set_visible(false);
            true
        }
        Renderer::Webview => match app.get_webview_window(FLOATING_LABEL) {
            Some(win) => match win.hide() {
                Ok(()) => {
                    // 卡片藏起来了才按：窗口还亮着就按住渲染 = 用户盯着一块不再重绘的卡片。
                    // 按的是**那两个开关**（停渲染 + 降档）—— 实测那 −62.9 MB 是两个一起买到的，
                    // 只做一件等于白做一半。整段截图期间（抓屏、编码、人选区）这张卡片
                    // 不用再养着一条渲染管线。
                    //
                    // 按失败只记一行：它代表"这一张没省到"，不代表卡片没藏住 ——
                    // 这一格答案如果被拿去当返回值，症状就是"截图之后卡片再也不出现"。
                    if !crate::state::set_window_webview_active(app, FLOATING_LABEL, false) {
                        tracing::warn!(
                            "截图期间按住卡片的渲染/档位没成功：这一张省不到内存，但收尾照样会把它放回来"
                        );
                    }
                    true
                }
                Err(e) => {
                    // 没藏住 = 那张卡片会被截进图里。要说清后果，不能只留一句"失败"。
                    tracing::warn!("藏悬浮窗失败，截图里可能带上它：{e}");
                    false
                }
            },
            None => false,
        },
    }
}

/// 恢复成触发前的样子。
pub fn restore_after_capture(app: &AppHandle, was_visible: bool) {
    if !was_visible {
        return;
    }
    // 抬渲染与抬档位必须排在 `show()` **之前**，而且两件一起抬（`set_visible` 里还有一次
    // 抬渲染，重复设一次是幂等的）。顺序反了的症状是"卡片回来了但那是一块旧画面/一块黑"。
    // 抬渲染与抬档位必须排在 `show()` **之前**，而且两件一起抬（`set_visible` 里还有一次
    // 抬渲染，重复设一次是幂等的）。顺序反了的症状是"卡片回来了但那是一块旧画面/一块黑"。
    if matches!(active_renderer(), Renderer::Webview)
        && !crate::state::set_window_webview_active(app, FLOATING_LABEL, true)
    {
        tracing::warn!("恢复卡片前抬起渲染/档位没全成：可能出现一帧不重绘的卡片");
    }
    if !set_visible(app, true) {
        tracing::warn!("恢复悬浮窗：窗口不存在，用户需要自己再打开一次");
    }
}

/// 主题变化时跟上。WebView2 那一支要重贴系统材质；原生那一支自己读配置。
pub fn apply_theme(app: &AppHandle, dark: bool) {
    match active_renderer() {
        Renderer::Native => native::refresh(),
        Renderer::Webview => {
            if let Some(win) = app.get_webview_window(FLOATING_LABEL) {
                crate::state::apply_window_vibrancy(&win, dark);
            }
        }
    }
}

/// 口径/尺寸这类配置改了之后催一次重画。
pub fn refresh(app: &AppHandle) {
    match active_renderer() {
        Renderer::Native => native::refresh(),
        // WebView2 那一支的页面自己每 2 秒问一次 `get_config`，不需要催
        Renderer::Webview => {
            let _ = app;
        }
    }
}

/// 500ms 置顶轮询还要不要跑：原生窗口在 `WM_WINDOWPOSCHANGING` 里当场答 topmost。
pub fn needs_topmost_poll() -> bool {
    matches!(active_renderer(), Renderer::Webview)
}

/// 退出程序时收掉卡片。
pub fn shutdown(app: &AppHandle) {
    match active_renderer() {
        Renderer::Native => native::shutdown(),
        Renderer::Webview => {
            if let Some(win) = app.get_webview_window(FLOATING_LABEL) {
                let _ = win.close();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gui_value(section: &str, key: &str) -> String {
        focusflow_core::config::instance().get(section, key)
    }

    #[test]
    fn unknown_renderer_value_stays_on_the_webview_path() {
        // 夹具自己写配置必须写成功，否则下面每一条断言测的都是"没写进去的那个值"。
        let put = |value: &str| {
            assert!(
                focusflow_core::config::instance()
                    .set("gui", "floating_renderer", value)
                    .is_ok(),
                "夹具没能把 {value:?} 写进配置，这条用例的前提就没了"
            );
        };
        put("");
        assert_eq!(
            renderer_of(focusflow_core::config::instance()),
            Renderer::Webview
        );
        put("NATIVE");
        assert_eq!(
            renderer_of(focusflow_core::config::instance()),
            Renderer::Native,
            "大小写不该改变渲染器"
        );
        put("原生");
        assert_eq!(
            renderer_of(focusflow_core::config::instance()),
            Renderer::Webview,
            "写错一个字不该让悬浮窗直接没了"
        );
        put("webview");
        assert_eq!(gui_value("gui", "floating_renderer"), "webview");
    }

    /// 原生窗口不在 Tauri 的窗口集合里 ⇒ 任何 `get_webview_window("floating")` 都是静默失效。
    /// 这条判据把"必须走 façade"钉住：以后再加读悬浮窗的地方，会在这里红。
    ///
    /// 为什么不扫全仓：`state.rs` 里那一处**就是** WebView2 分支自己的实现
    /// （`finish_floating_setup` / 尺寸修正线程），门面当然要能拿自己的窗口。
    /// 被禁的是"消费侧"——那些只想知道卡片在不在、该不该显示的模块。
    #[test]
    fn no_caller_reaches_for_the_floating_webview_directly() {
        let consumers = [
            "snip.rs",
            "tray.rs",
            "commands.rs",
            "hotkey.rs",
            "pin.rs",
            "plugins.rs",
            "lib.rs",
            "export.rs",
            "reveal.rs",
        ];
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut hits = Vec::new();
        for name in consumers {
            let path = root.join(name);
            let Ok(src) = std::fs::read_to_string(&path) else {
                continue; // 某个模块被删掉不是这条判据要管的事
            };
            let body: String = src
                .lines()
                .map(|l| l.split("//").next().unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n");
            for (i, _) in body.match_indices("get_webview_window(\"floating\")") {
                hits.push(format!("{name}:{}", body[..i].matches('\n').count() + 1));
            }
            for (i, _) in body.match_indices("get_webview_window(FLOATING_LABEL)") {
                hits.push(format!("{name}:{}", body[..i].matches('\n').count() + 1));
            }
        }
        assert!(
            hits.is_empty(),
            "这些点还在直接找悬浮窗的 webview，native 模式下会静默失效：{hits:?}"
        );
    }

    /// 两种渲染器都必须由同一处建窗：`tauri.conf.json` 里不许再留着那个声明，
    /// 否则 native 模式下 WebView2 照建，内存一点都省不下来。
    #[test]
    fn the_pill_is_not_declared_in_the_config_anymore() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json");
        let conf = std::fs::read_to_string(root).expect("读不到 tauri.conf.json");
        assert!(
            !conf.contains("\"floating.html\""),
            "tauri.conf.json 还在启动时声明悬浮窗：那会无条件建出一整套 WebView2"
        );
    }

    /// parked 那几格的写者必须真的在写：少一处就是"卡片停在旧数字上而且没人报错"。
    #[test]
    fn the_parked_slots_have_a_real_writer_on_the_stats_path() {
        let state = std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/state.rs"),
        )
        .expect("读不到 state.rs");
        let body: String = state
            .lines()
            .map(|l| l.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        let prod = &body[..body.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        assert_eq!(
            prod.match_indices("floating::publish_live(").count(),
            1,
            "统计线程应当挂一次 parked 快照，多出来的是往某个分支里补做"
        );
        assert!(
            prod.contains("floating::publish_paused("),
            "暂停态没挂进 parked 那格：原生卡片上「已暂停」永远不会出现"
        );
    }

    /// 截图藏窗那一步：按住渲染/档位的失败**不许**顶掉"藏没藏住"这个答案。
    /// 病症是"截图之后卡片再也不出现" —— 那个答案决定收尾会不会去 `show()`，一旦被人
    /// 写成返回按住的结果，卡片就在没人察觉的情况下永久消失（原生模式下连日志都没有）。
    #[test]
    fn the_capture_park_cannot_answer_for_the_hide() {
        let src = include_str!("mod.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let at = prod
            .find("pub fn hide_for_capture")
            .expect("藏窗那一步不见了");
        let body = &prod[at..at + prod[at..].find("\n}\n").expect("函数没有收尾")];
        let flat: String = body.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            flat.contains("if!crate::state::set_window_webview_active(app,FLOATING_LABEL,false)"),
            "那两个开关要么没按（只做半件等于白做），要么它的结果被拿去当返回值了：{flat}"
        );
        let park_at = flat
            .find("crate::state::set_window_webview_active(app,FLOATING_LABEL,false)")
            .expect("那两个开关没按：只做半件等于白做");
        // 夹具自己的前提：这条判据真在看"藏住那一支"的返回值，而不是切片切空让上面那条空转
        assert!(
            flat[..park_at].contains("Ok(())=>"),
            "按住那一步不在『藏住了』那一支里（窗口还亮着就按渲染 = 用户盯着一块不重绘的卡片）：{flat}"
        );
        assert!(
            flat[park_at..].contains("true}Err("),
            "藏住那一支的返回值不再是字面量 true —— 按住的结果被拿去当『藏没藏住』的答案了，\
             症状就是截图之后卡片再也不出现：{}",
            &flat[park_at..]
        );
    }

    /// 恢复卡片：抬渲染与抬档位必须排在 `set_visible`（`show()` 在它里面）之前。
    /// 顺序反了的症状是"卡片回来了，可那是一块不重绘的旧画面"。
    #[test]
    fn the_unpark_happens_before_the_show() {
        let src = include_str!("mod.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let at = prod
            .find("pub fn restore_after_capture")
            .expect("恢复那一步不见了");
        let body = &prod[at..at + prod[at..].find("\n}\n").expect("函数没有收尾")];
        let flat: String = body.chars().filter(|c| !c.is_whitespace()).collect();
        let unpark = flat
            .find("set_window_webview_active(app,FLOATING_LABEL,true)")
            .expect("恢复时没抬那两个开关：档位会一直停在 LOW");
        let show = flat
            .find("set_visible(app,true)")
            .expect("恢复没走那个唯一入口，判据要跟不动了");
        assert!(
            unpark < show,
            "抬档位排在 show 之后了（{unpark} vs {show}）：用户会先看见一帧不重绘的卡片"
        );
    }

    /// 事件推送必须问"这一支收不收得到"。钉的是**成对出现的形状**：
    /// 每一处往 `"floating"` 的推送都得包在那道问句里 —— 数一遍两边相等，
    /// 比"往回找最近的 if"可靠（`emit_to` 前面紧邻的 `if` 常常是 `if let Err(e)`，
    /// 那样会红在错误的地方，红一次人就把它删了）。
    #[test]
    fn every_floating_event_push_asks_whether_the_pill_listens() {
        let state = std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/state.rs"),
        )
        .expect("读不到 state.rs");
        let prod = prod_flat(&state);
        let pushes = prod.match_indices("emit_to(\"floating\"").count();
        let guards = prod
            .match_indices("iffloating_visible&&crate::floating::pushes_events(){")
            .count();
        assert!(
            pushes >= 2,
            "统计线程已经完全不往悬浮窗推事件了（现在 {pushes} 处）：那 WebView2 那一支也拿不到更新了"
        );
        assert_eq!(
            guards, pushes,
            "有 {pushes} 处往悬浮窗推事件、却只有 {guards} 处问了渲染器收不收得到：\
             native 模式下多出来的那些每条都是空发，而且没人知道"
        );
    }

    /// 剪掉注释与空白：这些调用点会被 rustfmt 折行，拿原始文本比会因为一次无关的格式化而红。
    fn prod_flat(src: &str) -> String {
        let body = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        body.lines()
            .map(|l| l.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect()
    }

    /// 置顶轮询是 WebView2 那一条路的补丁（每 500ms 一次 `SetWindowPos`）。
    /// 原生窗口在 `WM_WINDOWPOSCHANGING` 里当场答 topmost，所以那条线程必须**根本不启动** ——
    /// 钉的是"问句排在建窗查询之前"，顺序反了就等于照旧起一条每秒两次的轮询。
    #[test]
    fn the_topmost_poller_is_not_armed_for_the_native_pill() {
        let state = std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/state.rs"),
        )
        .expect("读不到 state.rs");
        let flat = prod_flat(&state);
        let at = flat
            .find("fnkeep_floating_on_top(app:&App){")
            .expect("keep_floating_on_top 的形状变了，判据要跟着改");
        let body = &flat[at..at + 400];
        let gate = body
            .find("if!crate::floating::needs_topmost_poll(){")
            .expect("没有问渲染器这一句：native 模式下也照旧起 500ms 轮询线程");
        let lookup = body
            .find("get_webview_window(\"floating\")")
            .expect("后面该有拿窗口那一步");
        assert!(
            gate < lookup,
            "问句排在拿窗口之后：那条轮询线程已经起来了才被发现不该起"
        );
    }
}
