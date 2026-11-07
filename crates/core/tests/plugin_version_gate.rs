//! 能力版本那一档的门（方案 3.10 / 验收第 7 条）。
//!
//! `PLUGIN_API` 是插件对宿主说"我要的那几格存在"的版本号。比宿主还高的那一档，
//! 过去是**照样加载**：作者以为拿到了新格子，实际每一格都是 nil，症状散在运行时
//! 各处（一个都不报错的那种最难查）。现在加载之前就拒绝，而且拒绝发生在**执行脚本之前**。

#![cfg(feature = "test-utils")]

use std::sync::Mutex;

use focusflow_core::config::FocusFlowConfig;
use focusflow_core::db;
use focusflow_core::paths;
use focusflow_core::plugins::manager::PluginManager;
use focusflow_core::plugins::PluginSource;

static LOCK: Mutex<()> = Mutex::new(());

fn guard() -> std::sync::MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn write_plugin(dir: &std::path::Path, file: &str, body: &str) -> std::path::PathBuf {
    let p = dir.join("plugins").join(format!("{file}.lua"));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, body).unwrap();
    p
}

fn manager_in(tmp: &std::path::Path) -> PluginManager {
    db::queries::invalidate_years_cache();
    let config: &'static FocusFlowConfig = Box::leak(Box::new(
        FocusFlowConfig::load(tmp.join("config.ini")).expect("临时配置应能载入"),
    ));
    PluginManager::new(config, db::Database::init_readonly())
}

/// 比宿主认得的还高那一档：拒绝，且**脚本一行都没跑**。
///
/// 夹具把 `error(...)` 放在顶层：门如果设在执行之后，先炸的就是这一句、Err 里念的是它；
/// 现在念的必须是那句"需要更新版本"。这一条把"拒绝在哪一步"钉成可观察的事实，
/// 而不是只在注释里承诺。
#[test]
fn a_plugin_asking_for_a_newer_api_is_rejected_before_its_code_runs() {
    let _g = guard();
    let tmp = paths::test_app_dir("api_too_new");
    let p = write_plugin(
        tmp.path(),
        "wants_v4",
        "PLUGIN_NAME = \"要 v4 的插件\"\nPLUGIN_API = 4\nerror(\"脚本被执行了\")\n",
    );
    let mut pm = manager_in(tmp.path());
    let e = pm
        .load_plugin(&p, PluginSource::User)
        .expect_err("API 比宿主还高的这一份要在加载这一步就被拒绝");
    assert!(
        e.contains("需要更新版本"),
        "要说清是版本不够，而不是笼统一句加载失败: {e}"
    );
    assert!(
        e.contains("4") && e.contains("3"),
        "要把\"它要几\"与\"这里到几\"都念出来: {e}"
    );
    assert!(
        !e.contains("脚本被执行了"),
        "顶层那句 error 是\"门装在执行之后\"的探针，它不许被触发: {e}"
    );
}

/// 同一份文件在门之内的那几档不许被误伤（缺省 / 1 / 2 都得照常加载）。
#[test]
fn the_tiers_this_host_knows_still_load() {
    let _g = guard();
    let tmp = paths::test_app_dir("api_known_tiers");
    let mut pm = manager_in(tmp.path());
    for (file, body) in [
        ("no_api", "PLUGIN_NAME = \"没写版本\"\n"),
        ("v1", "PLUGIN_NAME = \"一\"\nPLUGIN_API = 1\n"),
        ("v2", "PLUGIN_NAME = \"二\"\nPLUGIN_API = 2\n"),
        ("v3", "PLUGIN_NAME = \"三\"\nPLUGIN_API = 3\n"),
    ] {
        let p = write_plugin(tmp.path(), file, body);
        pm.load_plugin(&p, PluginSource::User)
            .unwrap_or_else(|e| panic!("{file} 是本宿主认得的一档，不该被拦: {e}"));
    }
    assert_eq!(
        pm.list_discovered().expect("清单").len(),
        4,
        "四份都该在页上"
    );
}

/// 拒绝要走到插件页那一行（验收第 7 条说的是"页面显示"，不是日志里一句话）。
///
/// 两档生命周期各有各的路，两条都查：`always` 那一档在启动时就撞门，`panel` 那一档
/// 要到用户点开面板（`ensure_loaded`）才撞 —— 后者如果只在点开的瞬间返回一句错、
/// 页面上那一行仍然是空的，用户就只看到"它没反应"。
#[test]
fn the_rejection_shows_up_on_the_plugin_page_row() {
    let _g = guard();
    let tmp = paths::test_app_dir("api_row_page");
    write_plugin(
        tmp.path(),
        "wants_v9_always",
        "PLUGIN_NAME = \"要 v9 常驻\"\nPLUGIN_API = 9\nPLUGIN_LIFECYCLE = \"always\"\n",
    );
    write_plugin(
        tmp.path(),
        "wants_v9_panel",
        "PLUGIN_NAME = \"要 v9 面板\"\nPLUGIN_API = 9\nfunction get_view() return { title = \"x\", widgets = {} } end\n",
    );
    let mut pm = manager_in(tmp.path());
    pm.load_all();
    let find = |pm: &mut PluginManager, file: &str| {
        pm.list_discovered()
            .expect("清单")
            .into_iter()
            .find(|r| r.file == file)
            .expect("被拒绝的插件也要在页上有一行——不然用户只看到它凭空消失")
    };
    let row = find(&mut pm, "wants_v9_always");
    assert!(!row.loaded, "被拒绝的不该有实例挂着");
    let note = row
        .error
        .clone()
        .expect("启动撞过门的那一行要写明为什么没加载，不能只剩\"未加载\"");
    assert!(note.contains("需要更新版本"), "页面上念的是哪一句: {note}");

    // `panel` 那一档：启动时根本不试（这是 3.14 的设计），所以撞门发生在点开那一下
    let before = find(&mut pm, "wants_v9_panel");
    assert!(
        before.error.is_none(),
        "面板档在启动时不该被试，更不该提前喊: {:?}",
        before.error
    );
    let e = pm
        .ensure_loaded("wants_v9_panel")
        .expect_err("点开面板同样要撞门，且要说清是哪一档不够");
    assert!(e.contains("需要更新版本"), "点开时念的是哪一句: {e}");
    let after = find(&mut pm, "wants_v9_panel");
    assert!(
        after
            .error
            .clone()
            .unwrap_or_default()
            .contains("需要更新版本"),
        "撞过门之后页面上那一行要留下这句话: {:?}",
        after.error
    );
}

/// 门只认顶层那一行的数字：注释掉的那一句不算声明。
///
/// 这一条守的是两侧共用同一把尺子这件事（`scan_meta` 与加载后的清单同源）。校验器
/// 说"这一项会被折掉"用的是同一批谓词，门要是另抄一套，同一份文件就会出现
/// "校验器放行、加载拒绝"或反过来。
#[test]
fn a_commented_out_declaration_is_not_a_declaration() {
    let _g = guard();
    let tmp = paths::test_app_dir("api_commented");
    let p = write_plugin(
        tmp.path(),
        "commented",
        "PLUGIN_NAME = \"注释掉的版本\"\n-- PLUGIN_API = 7\nlocal PLUGIN_API = 9\n",
    );
    let mut pm = manager_in(tmp.path());
    pm.load_plugin(&p, PluginSource::User)
        .expect("注释与函数体里的赋值都不算声明，缺省是 1");
    let rows = pm.list_discovered().expect("清单");
    let row = rows.iter().find(|r| r.file == "commented").expect("这一行");
    assert_eq!(row.manifest.api, 1, "缺省折回的是 1");
}
