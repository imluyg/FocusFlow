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
/// 托盘上**已经确认写上去了**的那句 tooltip（"已认领"，不是"这一轮想写的"）。
///
/// 与 `LAST_PAUSED` 同一族规矩：只有那次 `set_tooltip` 真成功才推进，失败的一拍留着
/// 旧值、下一轮自动重写。原先这里是 `let _ = tray.set_tooltip(...)` —— 写没写上没人知道，
/// 于是"今日数字停在十分钟前"与"刚刚更新过"在界面上长得一样，而 tooltip 恰恰是这个
/// 常驻程序唯一会在屏的那几行字。
static TOOLTIP_ACKED: Mutex<Option<String>> = Mutex::new(None);
/// tooltip 写失败已经报过没有（报一次就够，5 秒一条会把日志刷成噪声）
static TOOLTIP_FAILURE_NOTED: AtomicBool = AtomicBool::new(false);

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

/// 一次"把 tooltip 换成这一轮想说的话"的尝试结果 —— 形状与 [`IconRound`] 同一族。
#[derive(Debug, PartialEq, Eq)]
enum TextRound {
    /// 屏上已经是这一句，什么都不用做
    Nothing,
    /// 那次调用成功了，这一句才允许被认领下来
    Done,
    /// 没换成（托盘没建出来，或那次 COM 调用回错），原因原样带回去
    Failed(String),
}

/// 把"换 tooltip"这一步抽出来的唯一目的：**认领排在这次调用成功之后**。
///
/// `set` 就是那次真的写 tooltip 的调用，不会假装成功。这一句在这个程序里是"今天到现在
/// 记了多少"唯一的常驻读数，而它每 5 秒被整段重算一次：内容没变时不必再调一次 COM
/// （白跑），内容变了却没换成时必须**不**推进 `TOOLTIP_ACKED`，让下一拍自己补回来 ——
/// 原先 `let _ =` 直接把这两件事都抹平成"看起来一直在更新"。
fn apply_tooltip_text(
    acked: Option<&str>,
    want: &str,
    set: &mut dyn FnMut(&str) -> Result<(), String>,
) -> TextRound {
    if acked == Some(want) {
        return TextRound::Nothing;
    }
    match set(want) {
        Ok(()) => TextRound::Done,
        Err(why) => TextRound::Failed(why),
    }
}

/// 托盘 tooltip 上那一句久坐提醒（没有提醒时交回空串，整句照常）。
///
/// 只说"该活动几秒"这一件事：窗口数、建议时长都在悬浮窗/面板那两处，tooltip 的位置
/// 本来就挤（今日、速度、活跃时长、已暂停、自检异常已经占了五段）。
fn rest_segment(notice: Option<&focusflow_core::stats::RestNotice>) -> String {
    match notice {
        Some(n) => format!(" · ⚠ 久坐，建议活动 {} 秒", n.rest_seconds),
        None => String::new(),
    }
}

/// 托盘 tooltip 上那一句"这一周的周报还没生成"（不欠着时交回空串，整句照常）。
///
/// 只答"欠不欠"这一件事，不念原因也不念周数：原因有整句那么长（"这些年度库打不开…"），
/// 而这一格每 5 秒被整段重算一次，位置是今日/速度那两行的位置。
/// 挂着就代表此刻确实还欠着一份 —— 统计线程在那一周做成之后、以及为新一周起新趟之前
/// 都会把它摘掉，所以它不会念旧账（那两处的顺序由 `state.rs` 的接线判据钉）。
fn weekly_segment(outstanding: bool) -> String {
    if outstanding {
        " · ⚠ 周报没生成".to_string()
    } else {
        String::new()
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
                // 周报还欠着那一句：与久坐同一族道理 —— 启动进托盘是这个程序的默认形态，
                // 主窗口可能压根没开，前端那句红字没人看见，而自动周报是**后台**失败的，
                // 用户唯一能察觉的是"这周的周报怎么没出现"。完整原因在日志与设置页那颗按钮，
                // tooltip 的位置本来就挤（今日、速度、活跃、已暂停、自检、久坐已经占了六段），
                // 这里只说"欠着"这一件事。
                let weekly_str = weekly_segment(crate::state::weekly_report_outstanding());
                // 久坐提醒那一句：面板与悬浮窗都不在眼前时，托盘是唯一还会说话的出口。
                // "还在不在有效期"这一问只由 `rest_notice_now()` 答一次（过期就不交回来），
                // 这里不许再判一遍时间 —— 两处各判就会有两把尺子。
                let rest_str = rest_segment(crate::state::rest_notice_now().as_ref());
                let tooltip = format!(
                    "FocusFlow - 今日 {} · 速度 {}/分{}{}{}{}{}",
                    s.today_count,
                    s.cpm,
                    active_str,
                    if paused { " · 已暂停" } else { "" },
                    check_str,
                    weekly_str,
                    rest_str,
                );
                (
                    tooltip,
                    acked,
                    paused,
                    TRAY.lock().unwrap_or_else(|e| e.into_inner()).clone(),
                )
            };

            if let Some(tray) = tray {
                // 先读「屏上已经确认是哪一句」，**不**在这里把它消费掉（与图标那一问同规矩）：
                // 没换成就不推进 `TOOLTIP_ACKED`，下一拍（5 秒后）自己补回来，
                // 而不是等用户再开关一回什么才重画。出声只出一次（`TOOLTIP_FAILURE_NOTED`）。
                let text_acked = TOOLTIP_ACKED
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                let text_round = apply_tooltip_text(
                    text_acked.as_deref(),
                    &tooltip,
                    &mut |t| tray.set_tooltip(Some(t)).map_err(|e| e.to_string()),
                );
                match text_round {
                    TextRound::Done => {
                        *TOOLTIP_ACKED.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(tooltip.clone());
                        TOOLTIP_FAILURE_NOTED.store(false, Ordering::SeqCst);
                    }
                    TextRound::Failed(why) => {
                        if !TOOLTIP_FAILURE_NOTED.swap(true, Ordering::SeqCst) {
                            tracing::warn!(
                                "托盘那句读数没换成（{why}）：不认领这次的内容，\
                                 下一拍还会再试（同样的失败不再重复报）"
                            );
                        }
                    }
                    TextRound::Nothing => {}
                }
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
        // 走 AppState 的这一层：暂停时它会把写线程的活跃锚清零（见 `AppState::toggle_pause`），
        // 并把暂停态挂给悬浮窗那一格（原生卡片收不到事件，读的就是这一格）。
        let paused = state.toggle_pause();
        // 全局广播一次，两个 webview 都收到 —— 与 `commands::toggle_pause` 同一条口径。
        // 这里原先是"主窗口发一次、悬浮窗发一次"：那种逐窗口的写法正是会在
        // "悬浮窗不再是 webview"的那天悄悄少发一份的形状。
        let _ = app.emit("pause-changed", paused);
    }
}

fn toggle_floating(app: &tauri::AppHandle) {
    // 走与设置页同一个动作，并把结果写回 [floating] enabled：
    // 托盘点了不算数、下次启动又按老配置冒出来，是同一个开关两处口径不一致。
    let now_visible = crate::floating::is_visible(app);
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

#[cfg(test)]
mod rest_tooltip_tests {
    use super::{apply_tooltip_text, rest_segment, weekly_segment, TextRound};
    use std::cell::Cell;

    /// 没有活的提醒时那一段必须是空串：前面几段照旧，不许拖出一个「 · 」的尾巴。
    #[test]
    fn the_rest_segment_only_appears_while_a_notice_is_live() {
        assert_eq!(rest_segment(None), "", "没有提醒时不该留下分隔符");
        let n = focusflow_core::stats::RestNotice {
            window_minutes: 30,
            events_in_window: 1200,
            rest_seconds: 45,
            shown_seconds: 45,
        };
        let line = rest_segment(Some(&n));
        assert!(
            line.starts_with(" · "),
            "与前几段同一个分隔写法，实得：{line}"
        );
        assert!(line.contains("45 秒"), "要说得出该活动几秒，实得：{line}");
    }

    /// 周报欠着时托盘要说那一句，不欠时要闭嘴 —— 同样不许拖出分隔符的尾巴。
    ///
    /// 这一句是"自动周报没生成"唯一的常驻出口：主窗口可能压根没开（启动进托盘是默认形态），
    /// 而这一支的失败发生在后台，人不看日志就永远不知道少了什么。
    /// ⚠ 屏上到底重画没重画是眼验账（与久坐那一条同一族），这条用例钉的是那句话的**有无**。
    #[test]
    fn the_weekly_segment_appears_only_while_a_report_is_outstanding() {
        assert_eq!(
            weekly_segment(false),
            "",
            "做成或还没试过都不许说话，别拖出一个「 · 」的尾巴"
        );
        let line = weekly_segment(true);
        assert!(
            line.starts_with(" · ") && line.contains("周报没生成"),
            "欠着一份周报时托盘要说出这一件事，实得：{line}"
        );
    }

    /// 「写了」不等于「屏上是这句」：只有那次调用成功才允许认领；内容没变不许白跑一次 COM。
    #[test]
    fn the_tooltip_is_acked_only_after_the_call_succeeded() {
        let calls = Cell::new(0usize);
        let round = apply_tooltip_text(Some("今日 10"), "今日 10", &mut |t| {
            calls.set(calls.get() + 1);
            let _ = t;
            Ok(())
        });
        assert_eq!(round, TextRound::Nothing, "内容没变不必再写一遍");
        assert_eq!(calls.get(), 0, "Nothing 那一支连调用都不该发生");
        assert_eq!(
            apply_tooltip_text(Some("今日 10"), "今日 11", &mut |_| Ok(())),
            TextRound::Done,
            "换了句子且写成 ⇒ 这一句才可以被认领下来"
        );
        assert_eq!(
            apply_tooltip_text(None, "今日 12", &mut |_| Err("托盘还没建出来".to_string())),
            TextRound::Failed("托盘还没建出来".to_string()),
            "没写成必须把原因原样交回，绝不能算成 Done"
        );
        assert_eq!(
            apply_tooltip_text(None, "", &mut |_| Ok(())),
            TextRound::Done,
            "从没认领过（acked=None）而这一句是空串 ⇒ 仍要真去写一次"
        );
    }
}
