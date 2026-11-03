//! 每插件定时器与事件的端到端用例（方案 3.6、验收 4 与 5）。
//!
//! 这里的判据全部落在**跑回调之后世界真的变了**那一侧：计数写进 store、写进视图，
//! 而不是"某个函数被调用过"。桶的算术（每分钟、跨过午夜）在 `plugin_timers.rs`
//! 自己的单元用例里造，这里只核三件事：
//! · 装了入口的插件真能排进去、真会被跑到；
//! · 停用之后回调次数不再涨（这一条必须有盘上的数才盯得住）；
//! · 跑回调的那根线程就是投递那一拍的线程。

use focusflow_core::config::FocusFlowConfig;
use focusflow_core::db;
use focusflow_core::plugins::manager::{PluginManager, PluginSource};
use focusflow_core::plugins::{plugin_store, plugin_timers};

mod common;

mod timers_round {
    use std::sync::Mutex;
    pub(super) fn guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 排一格事件、把回调次数写进 store 的夹具。
///
/// 计数走 `store` 而不是 Lua 全局：卸载之后那份 Lua 就没了，只有落过盘的那份数还能在
/// 停用之后继续比对 —— 验收 4 要的就是"停了以后不再涨"，只看活着时的数盯不住它。
fn timer_lua(name: &str, body: &str) -> String {
    format!(
        "PLUGIN_NAME = \"{name}\"\n\
         PLUGIN_API = 2\n\
         PLUGIN_PERMISSIONS = {{ \"timer\", \"storage.read\", \"storage.write\" }}\n\
         hits = 0\n\
         local function bump()\n\
         \x20 hits = hits + 1\n\
         \x20 focusflow.store.set(\"hits\", hits)\n\
         \x20 focusflow.store.flush()\n\
         end\n\
         _G.bump = bump\n\
         function init()\n  {body}\n end\n\
         function get_view()\n\
         \x20 return {{ title = \"t\", widgets = {{ {{ type = \"label\", text = \"hits=\" .. tostring(hits) }} }} }}\n\
         end\n"
    )
}

/// 把夹具放进数据目录那一格、**先落一份授权记录**、再按用户插件加载。
///
/// 授权要在加载之前：夹具在 `init()` 里就调 `focusflow.on(...)`，而 v2 的插件没授权过
/// 就会被拒（这一条本身另有用例）。这里要测的是定时器，所以把前置条件先造好 ——
/// 用真实的 `grants.json`，不是把闸关掉（关掉闸测等于没测这条路）。
fn load(app: &focusflow_core::paths::TestAppDir, tag: &str, body: &str) -> (PluginManager, String) {
    use focusflow_core::plugins::permissions::{self, Grants};
    let dir = focusflow_core::paths::plugins_user_dir();
    std::fs::create_dir_all(&dir).expect("建临时插件目录");
    let stem = format!("{tag}_timer");
    let file = dir.join(format!("{stem}.lua"));
    let src = timer_lua(&stem, body);
    std::fs::write(&file, &src).expect("写夹具");
    let declared: Vec<String> = ["timer", "storage.read", "storage.write"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    permissions::write_grants(
        &stem,
        &Grants {
            permission_hash: permissions::permission_hash(&declared),
            content_hash: permissions::content_hash(src.as_bytes()),
            granted: declared,
        },
    )
    .expect("夹具授权该落得下去");
    let config: &'static FocusFlowConfig = Box::leak(Box::new(
        FocusFlowConfig::load(app.path().join("config.ini")).expect("读取 config.ini 失败"),
    ));
    let mut pm = PluginManager::new(config, db::Database::init_readonly());
    let name = pm
        .load_plugin(&file, PluginSource::User)
        .unwrap_or_else(|e| panic!("{tag} 夹具该加载得起来: {e}"));
    (pm, name)
}

/// 读盘上那一格 store 里的 hits（没有就是 0）。
fn hits_on_disk(stem: &str) -> i64 {
    match plugin_store::get(stem, "hits") {
        Ok(Some(v)) => v.as_i64().unwrap_or(0),
        Ok(None) => 0,
        Err(e) => panic!("{stem} 的 store 读不出来: {e}"),
    }
}

/// 排进去的事件会被这一拍跑到，而**同一分钟的第二拍不再跑**（每分钟一次的语义）。
///
/// 反向腿在同一个用例里：一次都不跑（装不上）与跑到天荒地老（没记住桶）都得红。
#[test]
fn an_event_subscription_fires_on_the_first_poll_and_then_once_per_minute() {
    let _g = timers_round::guard();
    let app = common::isolated_app_dir("timer_tick");
    let (mut pm, name) = load(&app, "tick", "focusflow.on(\"tick\", bump)");
    let ran = pm.pump_timers();
    assert_eq!(
        ran.iter().map(|(_, n)| *n).sum::<usize>(),
        1,
        "第一拍该跑一格，实得 {ran:?}"
    );
    assert!(hits_on_disk("tick_timer") >= 1, "回调写的数没落到盘上");
    let again = pm.pump_timers();
    assert!(
        again.is_empty(),
        "同一分钟不该再跑一次（按桶判，不是按拍判）: {again:?}"
    );
    let before = hits_on_disk("tick_timer");
    for _ in 0..5 {
        pm.pump_timers();
    }
    assert_eq!(
        hits_on_disk("tick_timer"),
        before,
        "空转的几拍把次数攒上去了"
    );
    // 视图那一头也看得见同一个数（插件页那格读的就是它）
    pm.refresh_view(&name).expect("视图该刷得动");
}

/// `every(sec)` 真按时间跑到（这里等一秒多，验的是 `Instant` 那条臂 —— 桶的逻辑在单元用例里）。
#[test]
fn a_periodic_timer_fires_once_the_interval_elapses() {
    let _g = timers_round::guard();
    let app = common::isolated_app_dir("timer_every");
    let (mut pm, _name) = load(&app, "per", "focusflow.every(1, bump)");
    let early = pm.pump_timers();
    assert!(
        early.iter().map(|(_, n)| *n).sum::<usize>() == 0,
        "刚排上就跑到 = 周期没生效: {early:?}"
    );
    std::thread::sleep(std::time::Duration::from_millis(1_100));
    let late = pm.pump_timers();
    assert_eq!(
        late.iter().map(|(_, n)| *n).sum::<usize>(),
        1,
        "过了一秒该跑一次: {late:?}"
    );
    assert_eq!(hits_on_disk("per_timer"), 1);
    // 周期的那一格还在表上（一次性才会自己消失）
    assert_eq!(
        plugin_timers::board_for("per_timer").lock().unwrap().len(),
        1
    );
}

/// 取消是真的取消：句柄交回去之后不再跑。
#[test]
fn cancelling_a_timer_stops_it_from_firing() {
    let _g = timers_round::guard();
    let app = common::isolated_app_dir("timer_cancel");
    let (mut pm, _name) = load(
        &app,
        "canc",
        "local h = focusflow.every(1, bump)\n local ok = focusflow.cancel(h)\n focusflow.store.set(\"cancelled\", ok and 1 or 0)\n focusflow.store.flush()",
    );
    assert_eq!(hits_on_disk("canc_timer"), 0, "取消掉的那格不该跑到");
    assert_eq!(
        plugin_store::get("canc_timer", "cancelled"),
        Ok(Some(serde_json::json!(1))),
        "cancel 该交回 true（句柄真的在那一格表里）"
    );
    std::thread::sleep(std::time::Duration::from_millis(1_100));
    let ran = pm.pump_timers();
    assert!(ran.is_empty(), "取消之后仍有回调: {ran:?}");
}

/// 验收 4：停用之后回调次数不再增长，而盘上留得下"停用前跑过几次"。
///
/// 这一条盯的是"卸载即取消"必须真的发生在**排程表**上：插件的 Lua 已经没了，
/// 如果那张表还留着格子，下一拍就会拿着句柄去找一个不存在的回调，
/// 症状是日志里每拍一条失败而界面上写着已停用。
#[test]
fn a_disabled_plugin_stops_firing_forever() {
    let _g = timers_round::guard();
    let app = common::isolated_app_dir("timer_disable");
    let (mut pm, _name) = load(&app, "dead", "focusflow.on(\"day_change\", bump)");
    pm.pump_timers();
    let before = hits_on_disk("dead_timer");
    assert!(before >= 1, "停用之前该跑过至少一次: {before}");
    let board_had = plugin_timers::board_for("dead_timer").lock().unwrap().len();
    assert_eq!(board_had, 1, "夹具前提：这一格确实排着东西");
    pm.set_enabled("dead_timer", false).expect("停用该成功");
    assert_eq!(
        plugin_timers::forget("dead_timer"),
        0,
        "停用那一步已经把整格销掉了，这里不该还有"
    );
    let mut ran_after = 0usize;
    for _ in 0..3 {
        ran_after += pm.pump_timers().iter().map(|(_, n)| *n).sum::<usize>();
    }
    assert_eq!(ran_after, 0, "停用之后还在跑回调");
    assert_eq!(
        hits_on_disk("dead_timer"),
        before,
        "盘上的数也不许再涨（那才是用户重启后看到的）"
    );
}

/// 验收 5：跑回调的那根线程就是投递这一拍的线程（生产里那一拍住在 GUI 线程）。
///
/// 两条臂一起断才说明问题：等于调用方的 id ⇒ 回调没跑到别处去；
/// 不等于另一根线程的 id ⇒ 这个相等不是"谁都写同一个占位值"混出来的。
#[test]
fn the_callback_runs_on_the_thread_that_polled() {
    let _g = timers_round::guard();
    let app = common::isolated_app_dir("timer_thread");
    let (mut pm, _name) = load(&app, "thr", "focusflow.on(\"tick\", bump)");
    let other = std::thread::spawn(|| format!("{:?}", std::thread::current().id()))
        .join()
        .expect("取另一根线程的 id");
    let mine = format!("{:?}", std::thread::current().id());
    assert_ne!(other, mine, "夹具前提：这两根线程得真是两根");
    // 这一份插件自己那一格在跑之前必须是空的（按 owner 分格，别的用例的读数顶不掉它）
    assert_eq!(
        plugin_timers::last_fire_thread_for_test("thr_timer"),
        None,
        "还没投递就留下了记录 = 这一格被别人用过"
    );
    pm.pump_timers();
    assert_eq!(
        plugin_timers::last_fire_thread_for_test("thr_timer"),
        Some(mine),
        "回调不在投递那一拍的线程上跑，就是往另一个 VM 里伸手"
    );
    assert_ne!(
        plugin_timers::last_fire_thread_for_test("thr_timer"),
        Some(other),
        "读数与调用方线程相等这条如果谁都成立，就是量具坏了"
    );
}

/// 入口装在**两个视图**里都有（内置与用户都排得进定时器），而错误话说给作者听得懂的那一格。
///
/// 三条拒绝各点一个方向：第二参数不是函数 / 事件名不认 / 秒数低于下限。
/// 从前这一族只有"定时器没起来"这一种症状，没有任何一条用例说得清是哪一笔写错。
#[test]
fn the_timer_verbs_exist_in_both_views_and_refuse_with_a_reason() {
    let _g = timers_round::guard();
    let app = common::isolated_app_dir("timer_verbs");
    let (mut pm, name) = load(
        &app,
        "verbs",
        "local bad = {}\n\
         local function grab(verb)\n\
         \x20 local ok, err = pcall(verb)\n\
         \x20 if not ok then bad[#bad + 1] = tostring(err) end\n\
         end\n\
         grab(function() focusflow.every(1, \"不是函数\") end)\n\
         grab(function() focusflow.on(\"mouse_move\", bump) end)\n\
         grab(function() focusflow.every(0, bump) end)\n\
         focusflow.store.set(\"errs\", table.concat(bad, \"||\"))\n\
         focusflow.store.flush()",
    );
    pm.refresh_view(&name).expect("夹具本身该有视图");
    let errs = match plugin_store::get("verbs_timer", "errs") {
        Ok(Some(v)) => v.as_str().unwrap_or_default().to_string(),
        other => panic!("三次拒绝该写进 store，实得 {other:?}"),
    };
    let pieces: Vec<&str> = errs.split("||").collect();
    assert_eq!(pieces.len(), 3, "三笔都该被拒并留下话: {errs}");
    assert!(
        pieces[0].contains("第二参数要写函数") && pieces[0].contains("string"),
        "类型错了要指着那一个参数说: {}",
        pieces[0]
    );
    assert!(
        pieces[1].contains("tick") && pieces[1].contains("day_change"),
        "事件名不认要交出可写的那几个: {}",
        pieces[1]
    );
    assert!(
        pieces[2].contains("最短") && pieces[2].contains("PLUGIN_REFRESH_MS"),
        "太密的间隔要指路: {}",
        pieces[2]
    );
    // 内部那两格装完就摘掉：留着等于给插件一条绕过权限闸的后门
    pm.refresh_view(&name).expect("刷新该成功");
    let every_is_function = {
        let info = pm.get_plugin(&name).expect("插件该在");
        let lua = info.lua.as_ref().expect("夹具该是加载着的");
        let host: mlua::Table = lua
            .globals()
            .get("focusflow")
            .expect("focusflow 那张表该在");
        matches!(
            host.get::<mlua::Value>("every"),
            Ok(mlua::Value::Function(_))
        )
    };
    assert!(every_is_function, "focusflow.every 这一格必须是可调的函数");
    let internals_gone = {
        let info = pm.get_plugin(&name).expect("插件该在");
        let lua = info.lua.as_ref().expect("夹具该是加载着的");
        let host: mlua::Table = lua.globals().get("focusflow").unwrap();
        let arm = host.get::<mlua::Value>("__ff_arm");
        let cancel = host.get::<mlua::Value>("__ff_cancel");
        matches!(arm, Ok(mlua::Value::Nil)) && matches!(cancel, Ok(mlua::Value::Nil))
    };
    assert!(
        internals_gone,
        "内部那两格（__ff_arm / __ff_cancel）没摘干净：留着就是绕过权限闸的一条近路"
    );
}
