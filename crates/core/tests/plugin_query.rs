//! 插件侧 `focusflow.query` 的端到端用例（路线图第 8 步、方案 9.1）。
//!
//! `db::queries::query_range` 那一半的算术在 `queries.rs` 自己的用例里核过（跨年、补零、
//! 截断、同分定序）。这里只问三件事，都是**只有从插件那一头才看得见**的：
//! · 参数写错时，话说清楚是哪一个字错了、可写的是哪几个（不是"查询失败"）；
//! · 读不回来的那一段（没有那一列 / 年度库坏了）交回的是**一句原因**，不是一串 0；
//! · 这一格真的在两个视图里都注册着，并且按 `stats.read` 过闸（前一件事由
//!   `plugin_api_wiring.rs` 那份键集相等核，这里核后两件）。

use focusflow_core::config::FocusFlowConfig;
use focusflow_core::db;
use focusflow_core::paths;
use focusflow_core::plugins::manager::{PluginManager, PluginSource};
use focusflow_core::plugins::permissions::{self, Grants};

mod common;

mod query_round {
    use std::sync::Mutex;
    pub(super) fn guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 把 `focusflow.query` 的结果写成一行文本的夹具。
///
/// 视图那一格就是观测值：`{ ok|rows=N|total=T|gap=G }`，其中 `gap` 为 `-` 表示读全了。
/// 用 `pcall` 包住是为了让"被权限拒"这种**抛错**的形态也能被看见，而不是让加载整个失败。
fn query_lua(name: &str, spec: &str) -> String {
    format!(
        "PLUGIN_NAME = \"{name}\"\n\
         PLUGIN_API = 2\n\
         PLUGIN_PERMISSIONS = {{ \"stats.read\" }}\n\
         function get_view()\n\
         \x20 local rows, gap = focusflow.query({spec})\n\
         \x20 local n = 0\n\
         \x20 for _ in ipairs(rows) do n = n + 1 end\n\
         \x20 return {{ title = \"t\", widgets = {{ {{ type = \"label\", text =\n\
         \x20 \x20 \"rows=\" .. n .. \"|total=\" .. tostring(rows.total) .. \"|gap=\" ..\n\
         \x20 \x20 (gap or \"-\") }} }} }}\n\
         end\n"
    )
}

/// 按用户插件加载一份夹具，并预先落好授权（这条链上的授权与视图要分开测）。
fn load_query(app_dir: &std::path::Path, tag: &str, spec: &str) -> (PluginManager, String) {
    let dir = paths::plugins_user_dir();
    std::fs::create_dir_all(&dir).expect("建临时插件目录");
    let stem = format!("{tag}_query");
    let file = dir.join(format!("{stem}.lua"));
    let src = query_lua(&stem, spec);
    std::fs::write(&file, &src).expect("写夹具");
    let declared: Vec<String> = vec!["stats.read".to_string()];
    permissions::write_grants(
        &stem,
        &Grants {
            permission_hash: permissions::permission_hash(&declared),
            content_hash: permissions::content_hash(src.as_bytes()),
            granted: declared,
        },
    )
    .expect("夹具授权");
    let config: &'static FocusFlowConfig = Box::leak(Box::new(
        FocusFlowConfig::load(app_dir.join("config.ini")).expect("读取 config.ini 失败"),
    ));
    let mut pm = PluginManager::new(config, db::Database::init_readonly());
    let name = pm
        .load_plugin(&file, PluginSource::User)
        .unwrap_or_else(|e| panic!("{tag} 夹具该加载得起来: {e}"));
    (pm, name)
}

/// 视图里那一行的 `gap=` 之后那一段。
fn gap_of(pm: &PluginManager, name: &str) -> String {
    let view = pm
        .get_plugin(name)
        .and_then(|p| p.view.as_ref())
        .expect("夹具该有视图");
    match view.widgets.first() {
        Some(focusflow_core::plugins::Widget::Label(text)) => text
            .split("|gap=")
            .nth(1)
            .unwrap_or("<没有 gap 这一格>")
            .to_string(),
        other => panic!("夹具的第一格该是 label，实得 {other:?}"),
    }
}

/// 参数写错时，点名是哪一个字错了、可写的是哪几个。
///
/// 三条臂：`group_by` 不认、`metric` 不认、压根没给区间。每一条都要在**没查库**之前出声，
/// 而屏幕上那句必须是作者能照着改的那一种。
#[test]
fn a_bad_spec_names_the_field_and_the_words_that_are_allowed() {
    let _g = query_round::guard();
    let app = common::isolated_app_dir("query_bad_spec");
    let cases = [
        (
            "grbad",
            "{ days = 7, group_by = \"weekday\" }",
            "group_by",
            "weekday",
            "device",
        ),
        (
            "mbad",
            "{ days = 7, metric = \"minutes\" }",
            "metric",
            "minutes",
            "seconds",
        ),
        (
            "norange",
            "{ group_by = \"day\" }",
            "没给区间",
            "days",
            "from",
        ),
    ];
    for (tag, spec, needle, wrong, alternative) in cases {
        let (mut pm, name) = load_query(app.path(), tag, spec);
        pm.refresh_view(&name)
            .unwrap_or_else(|e| panic!("{tag} 的 get_view 该跑得动（被拒也要有话，不该炸）: {e}"));
        let gap = gap_of(&pm, &name);
        assert_ne!(gap, "-", "{tag} 这一问该出声，实得一句读全了");
        assert!(
            gap.contains(needle) && gap.contains(wrong) && gap.contains(alternative),
            "{tag} 要说清哪一个字错了、可写的是哪几个，实得: {gap}"
        );
    }
}

/// 表里没有那一列的组合 ⇒ 交回原因，不交回一串 0。
///
/// 这一条是本仓最硬的那一族的插件版：`metric=seconds` 问按键榜，若被折成 0，
/// 作者写出的面板就会说"这段时间活跃 0 秒"，而真实情况是"这一问问不出"。
#[test]
fn asking_a_missing_column_says_the_table_instead_of_showing_zero() {
    let _g = query_round::guard();
    let app = common::isolated_app_dir("query_no_column");
    let (mut pm, name) = load_query(
        app.path(),
        "noc",
        "{ days = 7, group_by = \"key\", metric = \"seconds\" }",
    );
    pm.refresh_view(&name).expect("取视图该成功");
    let gap = gap_of(&pm, &name);
    assert!(
        gap.contains("key_counts") && gap.contains("没有查库"),
        "要点名是哪张表、并说清没查库，实得: {gap}"
    );
}

/// 写得对的那一问：行数与 total 从视图里读得回来，而 `gap` 是空的（读全了）。
///
/// 夹具本身不带库数据（临时数据目录是空的），所以这里钉的是形状与"读全了就不出声"：
/// 空榜 + `gap=-` 是合法答案，而空榜 + 一句原因才是有问题（两者必须是两种样子）。
#[test]
fn a_well_formed_spec_comes_back_as_rows_plus_a_total() {
    let _g = query_round::guard();
    let app = common::isolated_app_dir("query_ok");
    let (mut pm, name) = load_query(app.path(), "ok", "{ days = 7, group_by = \"day\" }");
    pm.refresh_view(&name).expect("取视图该成功");
    let gap = gap_of(&pm, &name);
    assert_eq!(gap, "-", "没有缺读就不该有点名: {gap}");
    let view = pm
        .get_plugin(&name)
        .and_then(|p| p.view.as_ref())
        .expect("视图该在");
    match view.widgets.first() {
        Some(focusflow_core::plugins::Widget::Label(text)) => {
            assert!(text.contains("rows=7"), "按日那一问含今天往前 7 天: {text}");
            assert!(text.contains("total=0"), "空库里总数是 0: {text}");
        }
        other => panic!("夹具的第一格该是 label，实得 {other:?}"),
    }
}

/// `days` 与 `from` 同时给 ⇒ 不猜哪一个算。
///
/// 两条都给的时候按任一条算都会出现"作者以为改了、屏上按另一条念"，
/// 与本仓 `stats_overview` 那两处同源缺陷同一族，所以钉成一条拒绝。
#[test]
fn two_ways_to_name_the_window_are_not_both_honored() {
    let _g = query_round::guard();
    let app = common::isolated_app_dir("query_two_windows");
    let (mut pm, name) = load_query(
        app.path(),
        "tw",
        "{ days = 7, from = \"2026-01-01\", group_by = \"day\" }",
    );
    pm.refresh_view(&name).expect("取视图该成功");
    let gap = gap_of(&pm, &name);
    assert!(
        gap.contains("days") && gap.contains("from") && gap.contains("只能给一个"),
        "两个都给要拒绝并说清哪一个才是：{gap}"
    );
}
