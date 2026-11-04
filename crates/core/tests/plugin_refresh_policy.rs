//! 刷新节奏与慢熔断端到端（方案 3.9、路线图第 10 步）。
//!
//! `Cadence` 自己的算术在 `plugin_refresh.rs` 里核过。这里核的是**接线**：
//! 声明有没有真的走到那一格账本上、`focusflow.invalidate()` 那句有没有把旗竖起来、
//! 降频之后的间隔与页面上那句「该插件较慢」是不是同一份来源。
//! 这几件事全在 UI 里数时间的话，一条用例都摸不到（与看板页 `should_refresh` 同一手法）。

use focusflow_core::config::FocusFlowConfig;
use focusflow_core::db;
use focusflow_core::paths;
use focusflow_core::plugins::manager::{PluginManager, PluginSource};
use focusflow_core::plugins::plugin_health;
use std::time::{Duration, Instant};

mod common;

mod refresh_round {
    use std::sync::Mutex;
    pub(super) fn guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 一份只声明刷新节奏的夹具；`body` 是 `get_view()` 里要先跑的语句（可以为空）。
fn refresh_lua(name: &str, refresh_ms: u64, api: &str, body: &str) -> String {
    format!(
        "PLUGIN_NAME = \"{name}\"\n\
         PLUGIN_API = {api}\n\
         PLUGIN_REFRESH_MS = {refresh_ms}\n\
         function get_view()\n  {body}\n\
         \x20 return {{ title = \"t\", widgets = {{ {{ type = \"label\", text = \"v\" }} }} }}\n\
         end\n"
    )
}

/// 把夹具放进用户那一格并加载。
fn load(
    app_dir: &std::path::Path,
    tag: &str,
    refresh_ms: u64,
    body: &str,
) -> (PluginManager, String) {
    let dir = paths::plugins_user_dir();
    std::fs::create_dir_all(&dir).expect("建临时插件目录");
    let stem = format!("{tag}_refresh");
    let file = dir.join(format!("{stem}.lua"));
    std::fs::write(&file, refresh_lua(&stem, refresh_ms, "1", body)).expect("写夹具");
    let config: &'static FocusFlowConfig = Box::leak(Box::new(
        FocusFlowConfig::load(app_dir.join("config.ini")).expect("读取 config.ini 失败"),
    ));
    let mut pm = PluginManager::new(config, db::Database::init_readonly());
    let name = pm
        .load_plugin(&file, PluginSource::User)
        .unwrap_or_else(|e| panic!("{tag} 夹具该加载得起来: {e}"));
    (pm, name)
}

/// 声明 `PLUGIN_REFRESH_MS = 0` 的一页不许自己刷；声明 1500 的按 1500 刷。
///
/// 这是今天行为与目标行为的分界：从前所有面板都是每秒一次（记账本每次 17 ms）。
/// 少了这一条，把节奏改回写死一秒不会有任何用例会红。
#[test]
fn each_panel_follows_its_own_declared_interval() {
    let _g = refresh_round::guard();
    let app = common::isolated_app_dir("refresh_declared");
    let (mut quiet, qname) = load(app.path(), "quiet", 0, "local x = 1");
    let now = Instant::now();
    assert!(
        !quiet.view_due(&qname, now),
        "声明 0 的一页不该自动重算（加载与开页各算一次已经够了）"
    );
    assert_eq!(quiet.view_interval_ms(&qname), 0, "缺省间隔要说得出是 0");

    let (mut loud, lname) = load(app.path(), "loud", 1_500, "local x = 1");
    assert!(loud.view_due(&lname, now), "第一拍该算一遍");
    loud.note_view_refreshed(&lname, now, Duration::from_millis(2));
    assert!(
        !loud.view_due(&lname, now + Duration::from_millis(1_499)),
        "差 1 ms 不算到点"
    );
    assert!(loud.view_due(&lname, now + Duration::from_millis(1_500)));
}

/// 插件自己说"我改了"（`focusflow.invalidate()`）要真的让下一拍重算，哪怕声明是 0。
///
/// 这条是唯一能从外面推动那一格账本的入口，所以两头都要断：
/// 调用没登记 ⇒ 页面上永远停在旧数据（作者只能改代码试）；
/// 登记了但没传到管理器 ⇒ 症状与前者一模一样，只有这条用例分得开。
#[test]
fn a_plugin_calling_invalidate_forces_the_next_refresh() {
    let _g = refresh_round::guard();
    let app = common::isolated_app_dir("refresh_invalidate");
    let (mut pm, name) = load(app.path(), "inv", 0, "local ok = focusflow.invalidate()");
    let now = Instant::now();
    // 加载时那一次 `get_view()` 已经跑过 invalidate ⇒ 旗应该已经竖起来了
    assert!(
        pm.view_due(&name, now),
        "插件调过 focusflow.invalidate()，下一拍就该重算"
    );
    pm.note_view_refreshed(&name, now, Duration::from_millis(1));
    assert!(
        !pm.view_due(&name, now + Duration::from_millis(50)),
        "刷过一次之后那面旗要落下来，不然就退化成每秒全量重算"
    );
}

/// 三次超过 5 ms 的一刷 ⇒ 这一页降到至少 5 s，并且页面上该说「该插件较慢」。
///
/// `list_discovered` 那一头的 `slow` 也在同一条里核：页面读的是它，
/// 如果只有账本有数、清单没带出来，屏幕上就永远看不到那句提示。
#[test]
fn three_slow_runs_throttle_the_panel_and_reach_the_page() {
    let _g = refresh_round::guard();
    let app = common::isolated_app_dir("refresh_slow");
    let (mut pm, name) = load(app.path(), "slw", 1_000, "local x = 1");
    let now = Instant::now();
    pm.note_view_refreshed(&name, now, Duration::from_millis(9));
    assert!(!pm.view_is_slow(&name), "一次慢不该降频");
    assert_eq!(pm.view_interval_ms(&name), 1_000);
    pm.note_view_refreshed(&name, now, Duration::from_millis(9));
    pm.note_view_refreshed(&name, now, Duration::from_millis(9));
    assert!(pm.view_is_slow(&name), "20 次里第 3 次慢该降频");
    assert_eq!(
        pm.view_interval_ms(&name),
        5_000,
        "降频到 5 s，实得 {}",
        pm.view_interval_ms(&name)
    );
    assert!(
        !pm.view_due(&name, now + Duration::from_millis(4_999)),
        "降频之后 1 秒那一拍不该再刷"
    );
    assert!(pm.view_due(&name, now + Duration::from_millis(5_000)));
    let listed = pm
        .list_discovered()
        .expect("清单该读得动")
        .into_iter()
        .find(|p| p.file == "slw_refresh")
        .expect("这一份夹具该在清单里");
    assert!(listed.slow, "页面上那句「该插件较慢」的来源就是这一格");
}

/// 翻进降频的那一次要进台账，而且只算一次。
///
/// 页面上的「降频 N 次」与详情页顶上那句读的都是 `Health::throttle_flips`，而它的写手
/// 只有 `note_throttle`。少了这条腿，把那次调用摘掉照样全绿 —— 症状是屏上一边写着
/// 「该插件较慢」、一边写着「降频 0 次」，两条账各说各的。
#[test]
fn a_throttle_flip_reaches_the_ledger_once_and_survives_the_grid_being_dropped() {
    let _g = refresh_round::guard();
    let app = common::isolated_app_dir("refresh_flip");
    let (mut pm, name) = load(app.path(), "flip", 1_000, "local x = 1");
    let stem = "flip_refresh";
    let now = Instant::now();
    let flips = || {
        plugin_health::board_for(stem)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .throttle_flips()
    };

    for _ in 0..2 {
        pm.note_view_refreshed(&name, now, Duration::from_millis(9));
    }
    assert_eq!(flips(), 0, "两次慢还不算降频");
    pm.note_view_refreshed(&name, now, Duration::from_millis(9));
    assert!(pm.view_is_slow(&name), "夹具前提：第三次慢该降频");
    assert_eq!(flips(), 1, "翻进降频的那一次没进台账");
    pm.note_view_refreshed(&name, now, Duration::from_millis(9));
    assert_eq!(flips(), 1, "已经在降频里，再慢一次不是新的一次降频");

    let line = plugin_health::summary_for(stem).expect("这一格已经有账，行上要念一句");
    assert!(
        line.contains("降频 1 次"),
        "页面上那句念不出降频次数：{line}"
    );

    plugin_health::forget(stem);
    assert_eq!(flips(), 1, "落盘之后从盘上读回，降频次数不该清零");

    // 慢样本滚掉会自动解除；下一次再翻进慢，才是新的一次。
    for _ in 0..20 {
        pm.note_view_refreshed(&name, now, Duration::from_millis(1));
    }
    assert!(!pm.view_is_slow(&name), "夹具前提：慢样本滚掉之后自动解除");
    for _ in 0..3 {
        pm.note_view_refreshed(&name, now, Duration::from_millis(9));
    }
    assert_eq!(flips(), 2, "解除之后再次翻进慢，是第二次降频");
}

/// 停用再启用 = 新实例，上一份慢记录不该压着它。
///
/// 这一条防的是"账本按展示名留着"：卸载时不丢的话，用户重新启用后第一次刷新
/// 就被降频成 5 s，而那一刻这个插件已经是一个全新的 VM 了。
#[test]
fn re_enabling_a_plugin_starts_with_a_clean_ledger() {
    let _g = refresh_round::guard();
    let app = common::isolated_app_dir("refresh_reset");
    let (mut pm, name) = load(app.path(), "rst", 1_000, "local x = 1");
    let now = Instant::now();
    for _ in 0..3 {
        pm.note_view_refreshed(&name, now, Duration::from_millis(9));
    }
    assert!(pm.view_is_slow(&name), "夹具前提：这一页已经进了慢熔断");
    pm.set_enabled("rst_refresh", false).expect("停用该成功");
    assert!(
        !pm.view_is_slow(&name),
        "停用之后账本要跟着这份实例一起丢掉"
    );
    pm.set_enabled("rst_refresh", true).expect("再启用该成功");
    // 账本是按需建的：新实例第一拍先问一次，问完才谈得上"按的是哪一档"
    pm.view_due(&name, now + Duration::from_secs(1));
    assert_eq!(
        pm.view_interval_ms(&name),
        1_000,
        "新实例回到声明的那一档，不带旧样本"
    );
    assert!(!pm.view_is_slow(&name), "新实例不该被标成慢");
}

/// 作者改了 `PLUGIN_REFRESH_MS` 存盘（热重载那条路），间隔要跟着改口。
///
/// 这一格单独钉是因为它有一个很安静的失败方式：账本里存着旧的 `declared_ms`，
/// 而清单上那一行显示的已经是新值 —— 屏上说的与实际跑的是两回事。
#[test]
fn a_reloaded_declaration_replaces_the_ledger() {
    let _g = refresh_round::guard();
    let app = common::isolated_app_dir("refresh_reload");
    let (mut pm, name) = load(app.path(), "chg", 1_000, "local x = 1");
    let now = Instant::now();
    pm.note_view_refreshed(&name, now, Duration::from_millis(1));
    assert_eq!(pm.view_interval_ms(&name), 1_000);
    // 改文件里那一个数字，然后走热重载那一条路
    let file = paths::plugins_user_dir().join("chg_refresh.lua");
    let edited = std::fs::read_to_string(&file)
        .expect("读回夹具")
        .replace("PLUGIN_REFRESH_MS = 1000", "PLUGIN_REFRESH_MS = 7000");
    std::fs::write(&file, &edited).expect("写回夹具");
    assert!(
        pm.reload_plugin("chg_refresh"),
        "改一个数字的那次重载该成功"
    );
    assert!(pm.view_due(&name, now), "换了一份新账本，第一拍该重算一次");
    assert_eq!(
        pm.view_interval_ms(&name),
        7_000,
        "新声明要接住（旧账本压着新值就是屏上说的与实际跑的不是一回事），实得 {}",
        pm.view_interval_ms(&name)
    );
}
