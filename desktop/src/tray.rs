//! 系统托盘：显示主界面 / 暂停记录（可勾选）/ 显示悬浮窗 / 打开截图文件夹 / 退出程序。
//! 对齐 Python 版：tooltip 每 5 秒刷新今日活跃与速度，暂停时切换图标并勾选菜单。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
    App, AppHandle, Emitter, Manager,
};

use crate::state::AppState;

const MENU_SHOW: &str = "show";
const MENU_PAUSE: &str = "pause";
const MENU_FLOATING: &str = "floating";
const MENU_SNIP: &str = "snip_folder";
const MENU_QUIT: &str = "quit";

/// 托盘控制器（供后台更新线程使用）。
static TRAY: Mutex<Option<TrayIcon<tauri::Wry>>> = Mutex::new(None);
static PAUSE_ITEM: Mutex<Option<CheckMenuItem<tauri::Wry>>> = Mutex::new(None);
/// 托盘上**已经确认换上的**那个形状对应"是否暂停"（避免每次重设图标）。
///
/// 它的语义是"已认领"，不是"上次读到的值"：只有那次 `set_icon` 成功才推进，
/// 失败的一拍留着原值，下一拍自动重试（见 [`apply_pause_icon`]）。
static LAST_PAUSED: AtomicBool = AtomicBool::new(false);
/// 换图标连续失败已经报过没有（报一次就够：这一轮不成功下一轮还会再试，
/// 每 5 秒一条会把日志本身刷成噪声 —— 与本文件对 `UPDATING` 的口径一致）
static ICON_FAILURE_NOTED: AtomicBool = AtomicBool::new(false);
/// 托盘更新是否进行中（防止 set_tooltip/set_icon 卡住时下一轮再抢锁）
static UPDATING: AtomicBool = AtomicBool::new(false);

/// 一次"把暂停状态换成托盘图标"的尝试结果。
#[derive(Debug, PartialEq, Eq)]
enum IconRound {
    /// 屏上已经是这个形状，什么都不用做
    Nothing,
    /// 那次调用成功了 —— 只有到这里才允许把新状态认领下来
    Done,
    /// 没换成（托盘没建出来，或那次 COM 调用回错），原因原样带回去
    Failed(String),
}

/// 把"换图标"这一步抽出来的唯一目的：**认领排在这次调用成功之后**。
///
/// `set` 就是那次真的换图标调用（不是形状检查、也不会假装成功）—— 这条函数的意义
/// 就在于把它的结果原样交回调用方，所以用例能拿一个计数器盯住"它到底被不被调用、
/// 调了几次"。旧写法是 `LAST_PAUSED.swap(paused) != paused`：边沿在**拿 stats 锁的时候**
/// 就被消费掉，之后才知道托盘在不在、调用成没成，而那两处结果都被 `let _ =` 咽掉 ⇒
/// 一次换不上就要等用户下一次开关才补得回来（可能几天），症状是鼠标悬浮写着「已暂停」、
/// 图标还是录制那个形状 —— 而图标才是这个功能给人看的那一眼。
fn apply_pause_icon(
    acked: bool,
    paused: bool,
    set: &mut dyn FnMut(bool) -> Result<(), String>,
) -> IconRound {
    if acked == paused {
        return IconRound::Nothing;
    }
    match set(paused) {
        Ok(()) => IconRound::Done,
        Err(why) => IconRound::Failed(why),
    }
}

/// 设置托盘图标 + 菜单 + 事件 + 后台 tooltip/图标更新。
pub fn setup_tray(app: &mut App) -> anyhow::Result<()> {
    let show_item = MenuItem::with_id(app, MENU_SHOW, "显示统计面板", true, None::<&str>)?;
    let pause_item =
        CheckMenuItem::with_id(app, MENU_PAUSE, "暂停记录", true, false, None::<&str>)?;
    let floating_item = MenuItem::with_id(app, MENU_FLOATING, "显示悬浮窗", true, None::<&str>)?;
    let snip_item = MenuItem::with_id(app, MENU_SNIP, "打开截图文件夹", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, MENU_QUIT, "退出程序", true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[
            &show_item,
            &pause_item,
            &tauri::menu::PredefinedMenuItem::separator(app)?,
            &floating_item,
            &snip_item,
            &tauri::menu::PredefinedMenuItem::separator(app)?,
            &quit_item,
        ],
    )?;

    // 图标：编译期内嵌 PNG（正常/暂停）
    let icon = tauri::include_image!("assets/focusflow.png");
    let paused_icon = tauri::include_image!("assets/focusflow_paused.png");

    let tray = TrayIconBuilder::with_id("focusflow")
        .menu(&menu)
        .icon(icon)
        .tooltip("FocusFlow - 效率追踪器")
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| handle_menu(app, event.id().as_ref()))
        .on_tray_icon_event(|tray, event| {
            // 左键单击/双击 → 显示主窗口
            match event {
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
                | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                } => show_main(tray.app_handle()),
                _ => {}
            }
        })
        .build(app)?;

    *TRAY.lock().unwrap_or_else(|e| e.into_inner()) = Some(tray);
    *PAUSE_ITEM.lock().unwrap_or_else(|e| e.into_inner()) = Some(pause_item);

    spawn_tray_updater(app, paused_icon);
    Ok(())
}

/// 后台更新：每 5 秒刷新 tooltip（今日活跃/速度/暂停状态），
/// 暂停时切换图标并勾选菜单项（对齐 Python 版）。
fn spawn_tray_updater(app: &App, paused_icon: tauri::image::Image<'static>) {
    let normal_icon = tauri::include_image!("assets/focusflow.png");
    let handle = app.handle().clone();

    std::thread::Builder::new()
        .name("tray-updater".into())
        .spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            // 上一轮还没完成（托盘 COM 调用被卡住）则跳过本轮，
            // 绝不让托盘调用阻塞其他线程（stats worker / get_stats）。
            if UPDATING.swap(true, Ordering::SeqCst) {
                continue;
            }
            let Some(state) = handle.try_state::<Arc<AppState>>() else {
                UPDATING.store(false, Ordering::SeqCst);
                continue;
            };
            // 锁内只拷贝数据，锁外才调用托盘 COM API
            let (tooltip, acked, paused, tray) = {
                let s = state.shared.lock().unwrap_or_else(|e| e.into_inner());
                let paused = state.listener.is_paused();
                // 读「屏上已经确认是哪个形状」，**不**在这里消费掉它（见 `apply_pause_icon`）
                let acked = LAST_PAUSED.load(Ordering::SeqCst);
                let active = s.active_seconds;
                let active_str = if active > 0 {
                    format!(" · 活跃 {}时{}分", active / 3600, (active % 3600) / 60)
                } else {
                    String::new()
                };
                // 启动自检有失败项时在 tooltip 里带上计数（B15）：
                // start_to_tray 时主窗口可能根本没开，前端 toast 看不见，
                // 托盘是常驻的唯一出口。更新线程本来就每 5 秒重写 tooltip，
                // 挂在这里就不会跟别处抢写。
                let fails = state.startup_report.iter().filter(|c| !c.ok).count();
                let check_str = if fails > 0 {
                    format!(" · ⚠ 自检 {fails} 项异常")
                } else {
                    String::new()
                };
                let tooltip = format!(
                    "FocusFlow - 今日 {} · 速度 {}/分{}{}{}",
                    s.today_count,
                    s.cpm,
                    active_str,
                    if paused { " · 已暂停" } else { "" },
                    check_str,
                );
                (
                    tooltip,
                    acked,
                    paused,
                    TRAY.lock().unwrap_or_else(|e| e.into_inner()).clone(),
                )
            };

            if let Some(tray) = tray {
                let _ = tray.set_tooltip(Some(&tooltip));
                // 这次调用没成就**不**推进 `LAST_PAUSED`：下一拍（5 秒后）会自己再试一次，
                // 而不是等用户再开关一回暂停。出声也只出一次（`ICON_FAILURE_NOTED`）。
                let round = apply_pause_icon(acked, paused, &mut |p| {
                    tray.set_icon(Some(if p {
                        paused_icon.clone()
                    } else {
                        normal_icon.clone()
                    }))
                    .map_err(|e| e.to_string())
                });
                match round {
                    IconRound::Done => {
                        LAST_PAUSED.store(paused, Ordering::SeqCst);
                        ICON_FAILURE_NOTED.store(false, Ordering::SeqCst);
                    }
                    IconRound::Failed(why) => {
                        if !ICON_FAILURE_NOTED.swap(true, Ordering::SeqCst) {
                            tracing::warn!(
                                "托盘图标没换成{}形状（{why}）：不认领这次的状态，\
                                 下一拍还会再试（后面同样的失败不再重复报）",
                                if paused { "已暂停" } else { "录制" }
                            );
                        }
                    }
                    IconRound::Nothing => {}
                }
            } else if acked != paused && !ICON_FAILURE_NOTED.swap(true, Ordering::SeqCst) {
                tracing::warn!(
                    "托盘图标还没建出来，暂停状态换不上形状：等它出现的那一拍再试（同样的失败不再重复报）"
                );
            }
            if let Some(item) = PAUSE_ITEM.lock().unwrap_or_else(|e| e.into_inner()).clone() {
                let _ = item.set_checked(paused);
            }
            UPDATING.store(false, Ordering::SeqCst);
        })
        .map_err(|e| tracing::error!("启动托盘更新线程失败（该功能不可用）: {e}"))
        .ok();
}

fn handle_menu(app: &AppHandle, id: &str) {
    match id {
        MENU_SHOW => show_main(app),
        MENU_PAUSE => toggle_pause(app),
        MENU_FLOATING => toggle_floating(app),
        // 点下去"没反应"是本功能唯一的故障形态，所以成败都要落日志（吞掉就等于
        // 让用户以为截图目录不存在）。
        MENU_SNIP => match crate::reveal::reveal_last_screenshot() {
            Ok(line) => tracing::info!("托盘打开截图文件夹：{line}"),
            Err(e) => tracing::error!("托盘打开截图文件夹失败：{e}"),
        },
        MENU_QUIT => {
            app.exit(0);
        }
        _ => {}
    }
}

fn show_main(app: &AppHandle) {
    crate::state::show_main_window(app);
}

fn toggle_pause(app: &AppHandle) {
    if let Some(state) = app.try_state::<Arc<AppState>>() {
        // 走 AppState 的这一层：暂停时它会把写线程的活跃锚清零（见 `AppState::toggle_pause`）。
        let paused = state.toggle_pause();
        // 同步前端暂停状态
        if let Some(win) = app.get_webview_window("main") {
            let _ = win.emit("pause-changed", paused);
        }
        if let Some(win) = app.get_webview_window("floating") {
            let _ = win.emit("pause-changed", paused);
        }
    }
}

fn toggle_floating(app: &tauri::AppHandle) {
    // 走与设置页同一个动作，并把结果写回 [floating] enabled：
    // 托盘点了不算数、下次启动又按老配置冒出来，是同一个开关两处口径不一致。
    let now_visible = app
        .get_webview_window("floating")
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false);
    let want = !now_visible;
    let _ = focusflow_core::config::instance().set(
        "floating",
        "enabled",
        if want { "true" } else { "false" },
    );
    crate::state::set_floating_visible(app, want);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// 边沿只能在**那次换图标的调用成功之后**被认领掉。
    ///
    /// 旧写法在锁里就 `swap`，之后才知道托盘在不在、调用成没成，而那两处的结果都被
    /// `let _ =` 咽掉 ⇒ 一次换不上就要等用户再开关一回暂停才补得回来（可能几天），
    /// 症状是悬浮提示写着「已暂停」、图标还是录制形状。这里用计数器盯住那个副作用：
    /// `set` 是真被调用的那一个，不是"假装成功"的替身。
    #[test]
    fn the_pause_edge_is_only_acked_after_the_icon_call_lands() {
        let calls = Cell::new(0u32);

        // 没有边沿：一次都不该碰 COM（这是"避免每次重设图标"那半条老规矩，别丢）
        let mut untouched = |_: bool| -> Result<(), String> {
            calls.set(calls.get() + 1);
            Ok(())
        };
        assert_eq!(
            apply_pause_icon(true, true, &mut untouched),
            IconRound::Nothing,
            "屏上已经是这个形状时什么都不该做"
        );
        assert_eq!(calls.get(), 0, "没边沿那一拍不该调用换图标");

        // 换成功了 ⇒ 允许认领
        let mut ok = |_: bool| -> Result<(), String> {
            calls.set(calls.get() + 1);
            Ok(())
        };
        assert_eq!(apply_pause_icon(false, true, &mut ok), IconRound::Done);
        assert_eq!(calls.get(), 1, "有边沿时那次调用必须真的发生一次");

        // 换不上：绝不能回 Done（那等于把边沿认领掉、下一拍不再试）
        let mut err = |_: bool| -> Result<(), String> {
            calls.set(calls.get() + 1);
            Err("句柄被占用".to_string())
        };
        match apply_pause_icon(false, true, &mut err) {
            IconRound::Failed(why) => assert!(
                why.contains("句柄被占用"),
                "失败原因要原样交给调用方，实得 {why}"
            ),
            other => panic!("换不上时不能把边沿认领掉，实得 {other:?}"),
        }
        assert_eq!(calls.get(), 2, "这一拍也该真的去试一次");

        // 反向腿：换成功了就回 Done —— 上面那条 Failed 不是恒真
        let mut ok2 = |_: bool| -> Result<(), String> {
            calls.set(calls.get() + 1);
            Ok(())
        };
        assert_eq!(apply_pause_icon(true, false, &mut ok2), IconRound::Done);
        assert_eq!(calls.get(), 3);
    }

    /// 接线守卫：更新线程必须**经** `apply_pause_icon` 换图标，并且只在 Done 那一臂里推进标记。
    ///
    /// 上面那条纯用例管的是这一步的规则，它红不到线程体里 —— 那个循环每 5 秒睡一次、
    /// 还握着真托盘。所以这里钉调用形态。判据先把 `//` 开头的行摘掉：本文件的注释里
    /// 引用过旧写法那句 `LAST_PAUSED.swap(paused)`，不摘就会在正确代码上误报（本仓踩过）。
    #[test]
    fn the_updater_acks_the_pause_edge_only_after_the_round_succeeds() {
        let src = include_str!("tray.rs");
        let raw = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let prod = raw
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut wrong: Vec<String> = Vec::new();
        let n = prod.matches("apply_pause_icon(acked, paused,").count();
        if n != 1 {
            wrong.push(format!("换图标那一笔必须恰好经一次规则函数，实得 {n} 次"));
        }
        if prod.contains("LAST_PAUSED.swap(") {
            wrong.push("标记又被读的时候就消费掉了（换没换成还不知道）".to_string());
        }
        let done_at = prod
            .find("IconRound::Done =>")
            .expect("Done 那一臂不见了（推进标记的地方就没规矩可依赖）");
        let store_at = prod
            .find("LAST_PAUSED.store(paused, Ordering::SeqCst)")
            .expect("成功之后没人推进标记了");
        let arm_start = done_at + "IconRound::Done =>".len();
        let arm_end = prod[arm_start..]
            .find("IconRound::")
            .map(|x| arm_start + x)
            .unwrap_or(prod.len());
        if !(store_at > done_at && store_at < arm_end) {
            wrong.push("推进标记没住在 Done 那一臂里".to_string());
        }
        if prod.matches("LAST_PAUSED.store(").count() != 1 {
            wrong.push("标记只该在成功那一处被推进".to_string());
        }
        assert!(
            wrong.is_empty(),
            "托盘更新的接线守卫红:\n{}",
            wrong.join("\n")
        );
    }
}
