//! 第三条事件：`on("app_change")`（方案 3.6 那一格里最后接上的那一条）。
//!
//! 这一族判据盯三件事，缺一件都算没测到：
//! · 载荷真的到了回调手里（不是"回调跑过一次"，是"那一格 store 里躺着投进去的那个名字"）；
//! · 权限按**事件名**分：`app_change` 要 `events.app`，`tick` 要 `timer`，两条臂各红一次；
//! · 两头都接在生产码上：投的那一条在采样线程、取的那一条在 GUI 那一拍 —— 少一头就是
//!   "单元测试绿、真程序永远收不到"那种形状（本仓为同一件事写过三条 both_ends 守卫）。

#![cfg(feature = "test-utils")]

use std::sync::Mutex;

use focusflow_core::config::FocusFlowConfig;
use focusflow_core::db;
use focusflow_core::paths;
use focusflow_core::plugins::manager::PluginManager;
use focusflow_core::plugins::{permissions, plugin_events, plugin_store, PluginSource};

fn guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// 件口是进程级的：每个用例起头与收尾都清一次，否则上一条用例投出去的名字会替本条"答"。
fn clear_slot() {
    plugin_events::reset_app_change();
}

fn app_change_lua(stem: &str, declared: &[&str]) -> String {
    let perms = declared
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "PLUGIN_NAME = \"{stem}\"\n\
         PLUGIN_API = 2\n\
         PLUGIN_PERMISSIONS = {{ {perms} }}\n\
         function init()\n\
         \x20 focusflow.on(\"app_change\", function(name)\n\
         \x20   focusflow.store.set(\"last\", name)\n\
         \x20   focusflow.store.flush()\n\
         \x20 end)\n\
         end\n\
         function get_view()\n\
         \x20 return {{ title = \"t\", widgets = {{ {{ type = \"label\", text = \"x\" }} }} }}\n\
         end\n"
    )
}

/// 放进数据目录那一格、按声明落一份真授权、再按用户插件加载。
///
/// 授权要在加载之前（夹具的 `init()` 里就调 `on`），而且用的是真 `grants.json` ——
/// 把闸关掉来"测这条路"等于没走这条路。
fn load(app: &paths::TestAppDir, stem: &str, declared: &[&str]) -> (PluginManager, String) {
    let dir = paths::plugins_user_dir();
    std::fs::create_dir_all(&dir).expect("建临时插件目录");
    let file = dir.join(format!("{stem}.lua"));
    let src = app_change_lua(stem, declared);
    std::fs::write(&file, &src).expect("写夹具");
    let declared_vec: Vec<String> = declared.iter().map(|s| s.to_string()).collect();
    if declared.contains(&"events.app") || declared.contains(&"timer") {
        permissions::write_grants(
            stem,
            &permissions::Grants {
                permission_hash: permissions::permission_hash(&declared_vec),
                content_hash: permissions::content_hash(src.as_bytes()),
                granted: declared_vec,
            },
        )
        .expect("夹具授权该落得下去");
    }
    let config: &'static FocusFlowConfig = Box::leak(Box::new(
        FocusFlowConfig::load(app.path().join("config.ini")).expect("读配置失败"),
    ));
    let mut pm = PluginManager::new(config, db::Database::init_readonly());
    let name = pm
        .load_plugin(&file, PluginSource::User)
        .unwrap_or_else(|e| panic!("{stem} 夹具该加载得起来: {e}"));
    (pm, name)
}

fn read_cell(stem: &str, key: &str) -> Option<String> {
    match plugin_store::get(stem, key) {
        Ok(Some(v)) => v.as_str().map(|s| s.to_string()),
        Ok(None) => None,
        Err(e) => panic!("{stem} 的 store 读不出来: {e}"),
    }
}

fn last_on_disk(stem: &str) -> Option<String> {
    read_cell(stem, "last")
}

/// 投出去的那个名字，要能在插件落盘的那一格裡读到；同名不再投、换名字再给一次。
#[test]
fn the_posted_app_name_reaches_the_callback_payload() {
    let _g = guard();
    clear_slot();
    let app = paths::test_app_dir("app_change_payload");
    let stem = "ac_payload";
    let (mut pm, name) = load(&app, stem, &["events.app", "storage.read", "storage.write"]);

    assert!(
        plugin_events::post_app_change("记事本.exe"),
        "第一次投该算变了"
    );
    let ran = pm.pump_timers();
    assert!(
        ran.iter().any(|(n, _)| n == &name),
        "这一拍该跑到那个插件: {ran:?}"
    );
    assert_eq!(
        last_on_disk(stem).as_deref(),
        Some("记事本.exe"),
        "回调拿到的必须是投进去的那个名字，而不是一个空串或句柄号"
    );

    // 前台没换：件口里没有新东西，插件那一格也不该再被跑（这一条挡住"每一拍都回调"）
    let _ = pm.pump_timers();
    assert_eq!(
        last_on_disk(stem).as_deref(),
        Some("记事本.exe"),
        "同一个应用不该反复投"
    );
    // 换一个应用：再给一次，且给的是新的那个名字
    assert!(plugin_events::post_app_change("浏览器.exe"));
    let _ = pm.pump_timers();
    assert_eq!(
        last_on_disk(stem).as_deref(),
        Some("浏览器.exe"),
        "换应用这件事要能在盘上看出来"
    );
    clear_slot();
}

/// 没声明 `events.app` 的那一份：订阅当场被拒，而且话说的是这一项权限。
#[test]
fn subscribing_without_events_app_is_denied_by_name() {
    let _g = guard();
    clear_slot();
    let app = paths::test_app_dir("app_change_denied");
    let dir = paths::plugins_user_dir();
    std::fs::create_dir_all(&dir).expect("建临时插件目录");
    // 只声明 timer（外加 store 那两格）：`on("app_change")` 要的是 events.app
    let stem = "ac_denied";
    let file = dir.join(format!("{stem}.lua"));
    let src = app_change_lua(stem, &["timer", "storage.read", "storage.write"]);
    std::fs::write(&file, &src).expect("写夹具");
    let declared: Vec<String> = ["timer", "storage.read", "storage.write"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    permissions::write_grants(
        stem,
        &permissions::Grants {
            permission_hash: permissions::permission_hash(&declared),
            content_hash: permissions::content_hash(src.as_bytes()),
            granted: declared,
        },
    )
    .expect("授权该落得下去");
    let config: &'static FocusFlowConfig = Box::leak(Box::new(
        FocusFlowConfig::load(app.path().join("config.ini")).expect("读配置失败"),
    ));
    let mut pm = PluginManager::new(config, db::Database::init_readonly());
    let e = pm
        .load_plugin(&file, PluginSource::User)
        .expect_err("订一格没声明的事件必须被拒");
    assert!(
        e.contains("events.app"),
        "要点名是哪一项权限没声明，而不是笼统一句 DENIED: {e}"
    );
    assert!(e.contains("DENIED"), "运行时那一套错误码要说出口: {e}");
    clear_slot();
}

/// 反向腿：`on("tick")` 仍是 `timer` 那一格的事，不许被顺手并进 events.app。
///
/// 权限按事件名拆完之后，最容易写错的就是这一支——把 `on` 整格判成 events.app，
/// 症状是老插件（只声明 timer）一夜之间全被拒。
#[test]
fn subscribing_to_tick_still_needs_only_timer() {
    let _g = guard();
    clear_slot();
    let app = paths::test_app_dir("tick_still_timer");
    let dir = paths::plugins_user_dir();
    std::fs::create_dir_all(&dir).expect("建临时插件目录");
    let stem = "tk_only_timer";
    let file = dir.join(format!("{stem}.lua"));
    let src = format!(
        "PLUGIN_NAME = \"{stem}\"\n\
         PLUGIN_API = 2\n\
         PLUGIN_PERMISSIONS = {{ \"timer\", \"storage.read\", \"storage.write\" }}\n\
         function init()\n\
         \x20 focusflow.on(\"tick\", function(name)\n\
         \x20   focusflow.store.set(\"tick_seen\", tostring(name))\n\
         \x20   focusflow.store.flush()\n\
         \x20 end)\n\
         end\n"
    );
    std::fs::write(&file, &src).expect("写夹具");
    let declared: Vec<String> = ["timer", "storage.read", "storage.write"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    permissions::write_grants(
        stem,
        &permissions::Grants {
            permission_hash: permissions::permission_hash(&declared),
            content_hash: permissions::content_hash(src.as_bytes()),
            granted: declared,
        },
    )
    .expect("授权该落得下去");
    let config: &'static FocusFlowConfig = Box::leak(Box::new(
        FocusFlowConfig::load(app.path().join("config.ini")).expect("读配置失败"),
    ));
    let mut pm = PluginManager::new(config, db::Database::init_readonly());
    pm.load_plugin(&file, PluginSource::User)
        .expect("只声明 timer 的插件订 tick 该照常能订");
    // 件口里放一个名字：tick 那一格跑的是墙钟，不该因此收到任何载荷
    plugin_events::post_app_change("记事本.exe");
    let _ = pm.pump_timers();
    assert_eq!(
        read_cell(stem, "tick_seen").as_deref(),
        Some("nil"),
        "tick 那一格这一拍跑了（分钟桶第一次见），但它收到的必须是「没有载荷」而不是那个应用名"
    );
    assert!(
        read_cell(stem, "last").is_none(),
        "app_change 的载荷不该喂给只订 tick 的那一格，实得 {:?}",
        read_cell(stem, "last")
    );
    clear_slot();
}

/// 两头都要在生产码上：投在采样线程、取在 GUI 那一拍，少一头就是"单测绿、真程序收不到"。
#[test]
fn the_app_change_chain_has_both_ends_in_production() {
    let read = |rel: &str| {
        std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel))
            .unwrap_or_else(|e| panic!("读 {rel} 失败: {e}"))
    };
    let at = |src: &str, anchor: &str| -> String {
        let i = src
            .find(anchor)
            .unwrap_or_else(|| panic!("锚点不见了: {anchor}"));
        // 取字节切片之后再按字符限长：`find` 交回的是字节位置，而文件里有中文
        src[i..].chars().take(4000).collect()
    };

    // 投的那一头：采样线程在"可归属"那一支里投，并在关掉统计时清件口
    let sampler = read("src/app_stats.rs");
    let body = at(&sampler, "if is_recordable(&n, &exclude) {");
    let flat: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        flat.contains("plugin_events::post_app_change(&n)"),
        "采样线程不再投，插件那一侧就永远收不到 app_change"
    );
    let off = at(&sampler, "if !enabled_now {");
    let off_flat: String = off.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        off_flat.contains("plugin_events::reset_app_change()"),
        "关掉统计之后件口里那份旧名字还会被投给插件"
    );

    // 取的那一头：GUI 那一拍的 pump 里
    let mgr = read("src/plugins/manager.rs");
    let pump = at(&mgr, "pub fn pump_timers(");
    let pump_flat: String = pump.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        pump_flat.contains("plugin_events::take_app_change()"),
        "pump 不取件口 = 事件永远堆在投递方那侧"
    );
    assert!(
        pump_flat.contains(".collect_app_change("),
        "取到了名字却不按格分发 = 订了的插件也收不到"
    );

    // Lua 那一层认这一种事件（不认的话插件写 on("app_change") 只会得到一句"只认…"）
    let host = read("src/plugins/host.rs");
    let host_flat: String = host.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        host_flat.contains("\"app_change\"=>Kind::AppChange"),
        "Rust 那一头的排程入口没认这一种事件"
    );
    assert!(
        host_flat.contains("name==\"app_change\"and\"app_change\""),
        "Lua 那一层的 on 没放开这一种事件"
    );
    // 闸必须把事件名一起交给判定（否则按整格判，拆开的权限就白拆了）
    assert!(
        host_flat.contains("guard(key,tostring(name))"),
        "on 这一格的闸没把事件名交出去，events.app 与 timer 会分不开"
    );
}

/// 卸载之后不再有回调：件口照投，但那一份排程表已经随插件销掉（验收 4 同一条形状）。
#[test]
fn unsubscribing_stops_the_app_change_callbacks() {
    let _g = guard();
    clear_slot();
    let app = paths::test_app_dir("app_change_unload");
    let stem = "ac_unload";
    let (mut pm, name) = load(&app, stem, &["events.app", "storage.read", "storage.write"]);
    plugin_events::post_app_change("甲.exe");
    let _ = pm.pump_timers();
    assert_eq!(
        last_on_disk(stem).as_deref(),
        Some("甲.exe"),
        "先证明这一条链本来通"
    );

    assert!(pm.unload_plugin(&name), "卸载该成功");
    plugin_events::post_app_change("乙.exe");
    let ran = pm.pump_timers();
    assert!(
        !ran.iter().any(|(n, _)| n == &name),
        "卸载之后这一拍不该再跑到它: {ran:?}"
    );
    assert_eq!(
        last_on_disk(stem).as_deref(),
        Some("甲.exe"),
        "卸载之后件口里投的东西不该还能写进它那一格"
    );
    clear_slot();
}
