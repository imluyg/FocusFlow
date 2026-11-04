//! 四类钩子的耗时与错误历史是不是**真接到了那四个落点上**（方案的预算表 + health.json）。
//!
//! 台账本体（`plugins::plugin_health`）自己那八条用例是纯函数层的：给定时长与预算比大小。
//! 这一族问的是另一件事：**跑一次钩子，账上有没有那一格**。少了这一族，
//! "把 `note_hook_time` 从 `refresh_view` 里摘掉"这种回归照样全绿 —— 而屏上那一行
//! 「最近一次 p95」就永远是"还没有记录"。
//!
//! 四段落点各有自己的腿：视图重建、`on_action`、`set_field`、按键批次，外加事件回调那一路
//! （它与排程共用 `pump_timers` 那一拍）。落盘那一条走的是真文件：卸载之后 `health.json`
//! 里要能读到原因，重启之后 `board_for` 要能把账读回来。

#![cfg(feature = "test-utils")]

use focusflow_core::config::FocusFlowConfig;
use focusflow_core::db;
use focusflow_core::paths;
use focusflow_core::plugins::{
    permissions, plugin_health, plugin_health::HookKind, PluginManager, PluginSource,
};

fn guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// 四类钩子都写了一份、且只花宿主 API 里不要权限的那几格的插件。
///
/// `record_key` 里那次抛错是第 5 条腿的夹具：它要能让错误历史里出现一行原因。
fn hook_lua(stem: &str) -> String {
    format!(
        "PLUGIN_NAME = \"{stem}\"\n\
         PLUGIN_API = 2\n\
         PLUGIN_PERMISSIONS = {{ \"timer\" }}\n\
         function init()\n\
         \x20 focusflow.log(\"起来\")\n\
         \x20 focusflow.on(\"tick\", function() focusflow.log(\"一拍\") end)\n\
         end\n\
         function get_view()\n\
         \x20 return {{ title = \"{stem}\", widgets = {{ {{ type = \"label\", text = \"卡片\" }} }} }}\n\
         end\n\
         function on_action(id)\n\
         \x20 focusflow.log(\"动作 \" .. id)\n\
         end\n\
         function set_field(f, v)\n\
         \x20 focusflow.log(f .. \"=\" .. v)\n\
         end\n\
         function record_key(k, n)\n\
         \x20 if k == \"boom\" then error(\"按键那一路写坏了\") end\n\
         end\n"
    )
}

fn load(stem: &str) -> (paths::TestAppDir, PluginManager, String) {
    let app = paths::test_app_dir(stem);
    let dir = paths::plugins_user_dir();
    std::fs::create_dir_all(&dir).expect("临时插件目录");
    let file = dir.join(format!("{stem}.lua"));
    let src = hook_lua(stem);
    std::fs::write(&file, &src).expect("写夹具");
    let declared = vec!["timer".to_string()];
    permissions::write_grants(
        stem,
        &permissions::Grants {
            permission_hash: permissions::permission_hash(&declared),
            content_hash: permissions::content_hash(src.as_bytes()),
            granted: declared,
        },
    )
    .expect("夹具授权该落得下去");
    let config: &'static FocusFlowConfig = Box::leak(Box::new(
        FocusFlowConfig::load(app.path().join("config.ini")).expect("读配置失败"),
    ));
    let mut pm = PluginManager::new(config, db::Database::init_readonly());
    let name = pm
        .load_plugin(&file, PluginSource::User)
        .unwrap_or_else(|e| panic!("{stem} 夹具该加载得起来: {e}"));
    (app, pm, name)
}

fn calls(stem: &str, kind: HookKind) -> u64 {
    plugin_health::board_for(stem)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .samples(kind)
        .map(|s| s.calls())
        .unwrap_or(0)
}

/// 四个落点各自要把一次耗时记进**自己那一档**：视图 / 动作 / 字段 / 按键批次。
///
/// 判据按档分开的理由与预算表一样：共用一格的话，"按键那一路没接线"会被视图那一档的
/// 计数掩盖掉，屏上仍然是一片"有数"。
#[test]
fn each_hook_site_records_into_its_own_bucket() {
    let _g = guard();
    let stem = "hooks_sites";
    let (_app, mut pm, name) = load(stem);

    pm.refresh_view(&name).expect("视图该刷得动");
    assert_eq!(calls(stem, HookKind::View), 1, "视图重建那一次没进台账");

    pm.plugin_action(&name, "do_it")
        .expect("动作该跑得动（它自己会再刷一次视图）");
    assert_eq!(calls(stem, HookKind::Action), 1, "on_action 那一次没进台账");

    pm.plugin_set_field(&name, "cat", "work")
        .expect("字段该写得动");
    assert_eq!(
        calls(stem, HookKind::Action),
        2,
        "set_field 与 on_action 同一档"
    );

    pm.plugin_key_event(&name, "a", 3);
    assert_eq!(calls(stem, HookKind::KeyEvent), 1, "按键批次那一次没进台账");

    // 事件回调那一路：pump_timers 那一拍里 tick 的回调要落进「事件回调」那一档
    let ran = pm.pump_timers();
    assert!(
        !ran.is_empty(),
        "夹具这一拍本该跑掉 tick 回调，结果一格都没跑"
    );
    assert!(
        calls(stem, HookKind::Event) >= 1,
        "回调那一次没进台账：{}",
        calls(stem, HookKind::Event)
    );
}

/// 按键那一路抛的错要进错误历史，而插件**不停用**（非限额的那一类原有口径）。
#[test]
fn a_non_limit_key_failure_lands_in_the_error_history() {
    let _g = guard();
    let stem = "hooks_errors";
    let (_app, mut pm, name) = load(stem);

    pm.plugin_key_event(&name, "boom", 1);
    let board = plugin_health::board_for(stem);
    let (errs, stops) = {
        // 守卫要绑住：`errors()` 回的是台账里的引用，临时守卫一析构就没了出处
        let guard = board.lock().unwrap_or_else(|e| e.into_inner());
        let notes: Vec<(String, String)> = guard
            .errors()
            .iter()
            .map(|e| (e.at.clone(), e.msg.clone()))
            .collect();
        (notes, guard.to_doc().stops)
    };
    assert_eq!(errs.len(), 1, "按键那一路的原因没进历史：{errs:?}");
    assert!(
        errs[0].1.contains("按键那一路写坏了"),
        "历史里要留着插件自己抛的那句话：{}",
        errs[0].1
    );
    assert!(
        !errs[0].0.is_empty(),
        "每条原因要带一个时刻（排障要往回看第几次发生在什么时候）"
    );
    // 这一类不停用：账上不该有"停用"那一格
    assert_eq!(stops, 0, "非限额的按键错误不该算停用");
}

/// 卸载那一步要把账落盘，而下一次 `board_for` 要从盘上把历史读回来 ——
/// 这一条就是"重启清零"那个缺陷的验收：基线从"清零"变成"还在"。
#[test]
fn the_ledger_outlives_the_instance_it_came_from() {
    let _g = guard();
    let stem = "hooks_persist";
    let (_app, mut pm, name) = load(stem);
    pm.refresh_view(&name).expect("视图该刷得动");
    pm.plugin_key_event(&name, "boom", 1);

    assert!(
        pm.unload_plugin(&name),
        "夹具这一下卸载该成功（它得先活着才谈得上卸载）"
    );
    let path = paths::plugin_state_path(stem).join("health.json");
    assert!(path.exists(), "卸载了却没落盘：{}", path.display());

    // 卸载那一步已经落盘并摘掉了本进程那一格，所以下一次 `board_for` 就是从盘上重开 ——
    // 这一步模拟的正是"进程换了"。
    let board = plugin_health::board_for(stem);
    let restored = board.lock().unwrap_or_else(|e| e.into_inner()).to_doc();
    assert_eq!(
        restored.errors.len(),
        1,
        "错误历史跟着实例没了 —— 那这一格白落盘"
    );
    let view = restored
        .hooks
        .iter()
        .find(|h| h.kind == HookKind::View.keyword())
        .expect("落盘的账里该有视图那一档");
    assert!(view.calls >= 1, "视图那一次的次数没落盘");
    let line = plugin_health::summary_line(&restored);
    assert!(
        line.contains("视图重建") && line.contains("失败记录 1 条"),
        "页面上要一句话念得出「哪几档跑过」与「有过几条失败」：{line}"
    );
    assert!(
        !line.contains("按键那一路写坏了"),
        "原因正文属于详情页，不该挤进行上那一格：{line}"
    );
}

/// 接线守卫：四个落点必须**带着自己那一档**去调用台账（钉调用形态，不钉函数名）。
///
/// 只核"调用过 `note_hook_time`"的那把尺子，会把四处全折成一档照样绿。
#[test]
fn the_four_sites_pass_their_own_kind_to_the_ledger() {
    let src = include_str!("../src/plugins/manager.rs");
    for (where_, kind) in [
        ("pub fn refresh_view", "View"),
        ("pub fn plugin_action", "Action"),
        ("pub fn plugin_set_field", "Action"),
        ("pub fn plugin_key_event", "KeyEvent"),
    ] {
        let at = src
            .find(where_)
            .unwrap_or_else(|| panic!("{where_} 不见了"));
        let rest = &src[at..];
        let body = &rest[..rest
            .find("\n    pub fn ")
            .or_else(|| rest.find("\n    fn "))
            .unwrap_or(rest.len())];
        assert!(
            body.contains(&format!("plugin_health::HookKind::{kind}"))
                || body.contains(&format!("HookKind::{kind}")),
            "{where_} 没把「{kind}」这一档交给台账：那一档的样本从哪一格来"
        );
        assert!(
            body.contains("note_hook_time(") || body.contains("HookKind::Event"),
            "{where_} 根本没调用台账"
        );
    }
    // 事件那一路在 fire_hook（关联函数，拿不到 &self，直接按 stem 落）
    let at = src.find("fn fire_hook").expect("fire_hook 不见了");
    let rest = &src[at..];
    let body = &rest[..rest
        .find("\n    pub fn ")
        .expect("fire_hook 之后找不到下一个方法")];
    assert!(
        body.contains("plugin_health::board_for(stem)") && body.contains("HookKind::Event"),
        "回调投递那一路没把耗时按事件档记进台账"
    );
}
