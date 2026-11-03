//! 实例生命周期：`panel` 随面板装卸、`always` 常驻（方案 3.14、验收 8 与 9）。
//!
//! 这一族判据盯的都是"装卸有没有真的发生"：
//! · 卸掉 = VM 没了、定时器没了、资源还了（不是只把视图清掉）；
//! · 常驻 = 关掉面板之后按键投递还在继续（番茄钟那类断统计就防在这里）。
//! 观察量都取自盘外可见的东西（清单里的 `loaded`、`plugin_timers` 那张表、
//! `get_view()` 里念出来的计数），不是"某个函数跑过了"。

use focusflow_core::config::FocusFlowConfig;
use focusflow_core::db;
use focusflow_core::paths;
use focusflow_core::plugins::manager::{PluginLifecycle, PluginManager};
use focusflow_core::plugins::{plugin_timers, resources};

mod common;

mod lifecycle_round {
    use std::sync::Mutex;
    pub(super) fn guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 写一份用户插件到数据目录那一格，交回文件路径。
fn write_plugin(tag: &str, src: &str) -> std::path::PathBuf {
    let dir = paths::plugins_user_dir();
    std::fs::create_dir_all(&dir).expect("建临时插件目录");
    let file = dir.join(format!("{tag}.lua"));
    std::fs::write(&file, src).expect("写夹具");
    file
}

fn manager(app_dir: &std::path::Path) -> PluginManager {
    let config: &'static FocusFlowConfig = Box::leak(Box::new(
        FocusFlowConfig::load(app_dir.join("config.ini")).expect("读取 config.ini 失败"),
    ));
    PluginManager::new(config, db::Database::init_readonly())
}

/// 验收 8：`panel` 那一档启动时装都不装，打开面板才装，关掉面板连 VM 与定时器一起卸。
///
/// 三个观察量各钉一格：清单上那句"面板打开时才加载"（页面看得见）、
/// `ensure_loaded` 之后真的活着、`release_panel_of` 之后名册里那格定时器没了。
/// 只断"loaded 变 false"盯不住"VM 还在"，那才是这份方案要买的性质。
#[test]
fn a_panel_plugin_is_loaded_on_open_and_unloaded_on_close() {
    let _g = lifecycle_round::guard();
    let app = common::isolated_app_dir("life_panel");
    write_plugin(
        "paneled",
        "PLUGIN_NAME = \"随面板那一格\"\n\
         PLUGIN_LIFECYCLE = \"panel\"\n\
         function init()\n  h = focusflow.every(30, function() end)\n\
         end\n\
         function get_view()\n\
         \x20 return { title = \"t\", widgets = { { type = \"label\", text = \"v\" } } }\n\
         end\n",
    );
    let mut pm = manager(app.path());
    assert!(pm.load_all(), "清单该读得动");
    let row = pm
        .list_discovered()
        .expect("清单该读得动")
        .into_iter()
        .find(|p| p.file == "paneled")
        .expect("这一份夹具该在清单里");
    assert!(!row.loaded, "启动时不该把只服务一页的插件装起来");
    assert_eq!(
        row.note.as_deref(),
        Some("面板打开时才加载"),
        "页面上要说清它为什么没活着，而不是留一句安静的未加载"
    );
    assert_eq!(row.lifecycle(), PluginLifecycle::Panel);

    pm.ensure_loaded("随面板那一格")
        .expect("打开面板该把它装起来");
    let after = pm.get_plugin("随面板那一格").expect("装起来之后该在表里");
    assert!(after.loaded && after.lua.is_some(), "VM 该真的建起来了");
    assert_eq!(
        plugin_timers::board_for("paneled").lock().unwrap().len(),
        1,
        "init 里那一格定时器该排着"
    );
    // 再叫一次是空操作（不该把已经活着的重装一遍）
    pm.ensure_loaded("paneled").expect("按文件名也该认得");

    assert!(pm.release_panel_of("随面板那一格"), "关掉这一页该真的卸掉");
    assert!(
        pm.get_plugin("随面板那一格").is_none(),
        "卸掉之后条目要离开表（VM 跟着 drop）"
    );
    assert_eq!(
        resources::release_all("paneled").len(),
        0,
        "整格定时器该随卸载一起消失，不该留下待跑的句柄"
    );
    assert!(
        !pm.release_panel_of("随面板那一格"),
        "第二关是空操作：不许对没开的页交回\"卸掉了\""
    );
}

/// 验收 9：`always` 那一档关掉面板也不动，按键投递继续，番茄计数不断。
#[test]
fn an_always_plugin_survives_the_panel_close_and_keeps_receiving_keys() {
    let _g = lifecycle_round::guard();
    let app = common::isolated_app_dir("life_always");
    write_plugin(
        "resident",
        "PLUGIN_NAME = \"常驻那一格\"\n\
         PLUGIN_LIFECYCLE = \"always\"\n\
         seen = 0\n\
         function record_key(k, n) seen = seen + n end\n\
         function get_view()\n\
         \x20 return { title = \"t\", widgets = { { type = \"label\", text = \"seen=\" .. tostring(seen) } } }\n\
         end\n",
    );
    let mut pm = manager(app.path());
    assert!(pm.load_all(), "清单该读得动");
    assert!(
        !pm.release_panel_of("常驻那一格"),
        "常驻的那一格关掉面板不该被卸（卸了统计就断在这里）"
    );
    assert!(
        pm.get_plugin("常驻那一格")
            .map(|p| p.loaded)
            .unwrap_or(false),
        "关掉面板之后它还得活着"
    );
    pm.plugin_key_event("常驻那一格", "A", 4);
    pm.refresh_view("常驻那一格").expect("视图该刷得动");
    let view = pm
        .get_plugin("常驻那一格")
        .and_then(|p| p.view.clone())
        .expect("视图该在");
    match view.widgets.first() {
        Some(focusflow_core::plugins::Widget::Label(text)) => {
            assert_eq!(text, "seen=4", "按键投递要真的进到常驻那一格的 record_key");
        }
        other => panic!("夹具的第一格该是 label，实得 {other:?}"),
    }
}

/// 内置那一格即使脚本自己写 `panel` 也强制常驻（身份由位置决定，3.1 与 3.3 那条）。
#[test]
fn a_bundled_plugin_cannot_ask_to_be_unloaded_with_the_panel() {
    let _g = lifecycle_round::guard();
    let app = common::isolated_app_dir("life_builtin");
    let dir = paths::plugins_builtin_dir();
    std::fs::create_dir_all(&dir).expect("建内置那一格");
    let file = dir.join("asking_panel.lua");
    std::fs::write(
        &file,
        "PLUGIN_NAME = \"自称随面板的内置\"\nPLUGIN_LIFECYCLE = \"panel\"\n",
    )
    .expect("写夹具");
    let mut pm = manager(app.path());
    assert!(pm.load_all());
    let row = pm
        .list_discovered()
        .expect("清单该读得动")
        .into_iter()
        .find(|p| p.file == "asking_panel")
        .expect("这一份夹具该在清单里");
    assert_eq!(
        row.lifecycle(),
        PluginLifecycle::Always,
        "内置那一格由位置强制常驻，不看脚本写了什么"
    );
    assert!(row.loaded, "它该在启动那一轮就装起来");
    assert!(
        !pm.release_panel_of("自称随面板的内置"),
        "关掉面板不许把内置模块卸掉 —— 那会让一次升级打断统计"
    );
}

/// 验收 22：第 9 个常驻插件标「待加载」，不建 VM，也给得出路。
#[test]
fn the_ninth_always_plugin_is_queued_not_loaded() {
    let _g = lifecycle_round::guard();
    let app = common::isolated_app_dir("life_cap");
    for i in 1..=9 {
        write_plugin(
            &format!("res{i}"),
            &format!(
                "PLUGIN_NAME = \"常驻第{i}格\"\nPLUGIN_LIFECYCLE = \"always\"\n\
                 function get_view()\n\
                 \x20 return {{ title = \"t\", widgets = {{ {{ type = \"label\", text = \"v\" }} }} }}\n\
                 end\n"
            ),
        );
    }
    let mut pm = manager(app.path());
    pm.load_all();
    let rows = pm.list_discovered().expect("清单该读得动");
    // 随包那五本也是常驻的（`isolated_app_dir` 把它们放进内置那一格），所以额度是先被它们占掉的：
    // 这里钉的是"总共正好 8 格活着"与"我这份第 9 个排到了队尾"，不去数具体是哪几格。
    let loaded_all: Vec<&str> = rows
        .iter()
        .filter(|p| p.loaded)
        .map(|p| p.file.as_str())
        .collect();
    assert_eq!(
        loaded_all.len(),
        focusflow_core::plugins::manager::MAX_ALWAYS_PLUGINS,
        "活着的格数该正好等于常驻额度，实得 {loaded_all:?}"
    );
    let ninth = rows
        .iter()
        .find(|p| p.file == "res9")
        .expect("第九格该在清单里");
    assert!(!ninth.loaded, "第九个常驻不该被装起来");
    let note = ninth.note.clone().unwrap_or_default();
    assert!(
        note.contains("常驻额度") && note.contains("待加载") && note.contains("停用"),
        "被挤掉的那一格要说清为什么、以及出路：{note}"
    );
    // 出路：用户点开它（一个明确的手势）不受额度限制 —— 挤掉一个在跑的常驻更糟
    pm.ensure_loaded("res9").expect("点开的那一格该装得起来");
    assert!(
        pm.get_plugin("常驻第9格")
            .map(|p| p.loaded)
            .unwrap_or(false),
        "ensure_loaded 是这一档唯一的出路，它得真能装上"
    );
}
