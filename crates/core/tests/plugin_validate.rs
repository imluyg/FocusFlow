//! 校验器（路线图第 14 步 / 方案 3.7 的"声明必用"那一半）。
//!
//! 判据的两条纪律在本场是硬要求：
//! · 每条都断到**行号**——校验器买的正是"作者不用通读整份文件"，只断"有没有说话"等于把
//!   这一份价值判没了；
//! · 每条都带**反向腿**——同一段代码在另一种声明/位置上不该说那句话。少了这一半，
//!   判据会把"永远出声"当成"判得准"。

use focusflow_core::plugins::validate::{self, Level, Report};
use focusflow_core::plugins::PluginSource;

fn lua(body: &str) -> String {
    format!("PLUGIN_NAME = \"探针\"\n{body}")
}

fn report(body: &str, source: PluginSource) -> Report {
    validate::validate("probe", source, &lua(body))
}

/// 找到第一条**提到**某个词的话（消息里出现该词，且带行号）。
fn saying<'a>(r: &'a Report, word: &str) -> Option<&'a validate::Finding> {
    r.findings.iter().find(|f| f.message.contains(word))
}

/// 声明了权限而整份代码没用到 ⇒ 提示，并且指回声明那一行（方案验收第 16 条的形状）。
#[test]
fn a_declared_permission_that_no_call_needs_is_flagged_with_its_line() {
    // 拼出来之后：第 1 行 PLUGIN_NAME、第 2 行 PLUGIN_API、第 3 行 PLUGIN_PERMISSIONS、
    // 第 4 行函数头、第 5 行那次 `focusflow.stats` 调用
    let r = report(
        "PLUGIN_API = 2\nPLUGIN_PERMISSIONS = { \"stats.read\", \"notify\" }\n\
         function get_view()\n  local s = focusflow.stats(7)\n  return { type = \"text\", text = tostring(s) }\nend\n",
        PluginSource::User,
    );
    let f =
        saying(&r, "\"notify\"").expect("多要的那一格必须说出来：用户点的「同意」比插件能做的宽");
    assert_eq!(
        f.level,
        Level::Note,
        "声明必用只是 lint，不是「跑起来会不对」"
    );
    assert_eq!(
        f.line,
        Some(3),
        "要指回声明那一行，否则作者得自己找 PLUGIN_PERMISSIONS 在哪"
    );
    // 反向腿：stats.read 真的被用到了，就不该挨这一句
    assert!(
        saying(&r, "\"stats.read\"").is_none(),
        "用到了的权限被判成多要 ⇒ 用例：{:?}",
        r.findings
    );
}

/// 剪贴板那一格：动词名会折成权限名（read_text → clipboard.read），
/// 用错了动词、或声明少了另一半，都要在调用点说出来。
#[test]
fn clipboard_verbs_map_to_permissions_and_typos_are_flagged() {
    // 正腿：只调 read_text、只声明 clipboard.read ⇒ 不该有多余的提示。
    let ok = report(
        "PLUGIN_API = 3\nPLUGIN_PERMISSIONS = { \"clipboard.read\" }\nfunction init()\n  local t = focusflow.clipboard.read_text()\nend\n",
        PluginSource::User,
    );
    assert!(
        !ok.findings.iter().any(|f| f.level == Level::Error),
        "read_text 与 clipboard.read 应当对得上: {:?}",
        ok.findings
    );
    assert!(
        !ok.findings
            .iter()
            .any(|f| f.message.contains("\"clipboard.read\"")),
        "用到了的权限被判成多要: {:?}",
        ok.findings
    );

    // 声明少一半：write_text 要 clipboard.write，行号是调用点那一行。
    let missing = report(
        "PLUGIN_API = 3\nPLUGIN_PERMISSIONS = { \"clipboard.read\" }\nfunction init()\n  focusflow.clipboard.write_text(\"x\")\nend\n",
        PluginSource::User,
    );
    let f = saying(&missing, "clipboard.write").expect("没声明的写着");
    assert_eq!(f.level, Level::Error);
    assert_eq!(f.line, Some(5), "要指到调用点（PLUGIN_NAME 占第 1 行）");

    // 动词拼错：read_txt 不是这一格认得的名字。
    let typo = report(
        "PLUGIN_API = 3\nPLUGIN_PERMISSIONS = { \"clipboard.read\" }\nfunction init()\n  focusflow.clipboard.read_txt()\nend\n",
        PluginSource::User,
    );
    assert!(
        typo.findings
            .iter()
            .any(|f| f.message.contains("clipboard.read_txt")),
        "拼错的动词要当场点出来: {:?}",
        typo.findings
    );
}

/// 用了一格而没声明 ⇒ 问题，行号是**调用点**那一行（不是声明那一行）。
#[test]
fn using_an_api_whose_permission_is_not_declared_is_an_error_at_the_call() {
    // 第 2 行只声明 stats.read，第 4 行却写了 store.set（要 storage.write）
    let r = report(
        "PLUGIN_API = 2\nPLUGIN_PERMISSIONS = { \"stats.read\" }\nfunction init()\n  focusflow.store.set(\"a\", 1)\nend\n",
        PluginSource::User,
    );
    let f = saying(&r, "storage.write").expect("用了没声明的这一格必须点名是哪一项权限");
    assert_eq!(f.level, Level::Error);
    assert_eq!(f.line, Some(5), "要说的是那一行调用会吃 DENIED");
    // 反向腿：同一个调用补上声明之后，这一句就该消失
    let ok = report(
        "PLUGIN_API = 2\nPLUGIN_PERMISSIONS = { \"stats.read\", \"storage.write\" }\nfunction init()\n  focusflow.store.set(\"a\", 1)\nend\n",
        PluginSource::User,
    );
    assert!(
        ok.findings
            .iter()
            .all(|x| !(x.message.contains("没声明") && x.level == Level::Error)),
        "补上声明还在喊拒绝：{:?}",
        ok.findings
    );
}

/// 清单外的权限：说清宿主没有这一项，并把认得的清单念出来。
#[test]
fn a_permission_outside_the_frozen_list_names_the_known_ones() {
    let r = report(
        "PLUGIN_API = 2\nPLUGIN_PERMISSIONS = { \"storage.write\", \"net\" }\nfunction init()\n  focusflow.store.set(\"a\", 1)\nend\n",
        PluginSource::User,
    );
    let f = saying(&r, "\"net\"").expect("清单外的声明会被折掉，运行时不会为它装闸");
    assert_eq!(f.level, Level::Error);
    assert_eq!(f.line, Some(3));
    assert!(
        f.message.contains("stats.read"),
        "要把认得的那几项念出来: {}",
        f.message
    );
    // 反向腿：`events.app` 在清单里（2026-11-04 起 `on("app_change")` 真的用它），
    // 所以它不该被念成"宿主没有这一项权限"
    let listed = report(
        "PLUGIN_API = 2\nPLUGIN_PERMISSIONS = { \"events.app\" }\nfunction get_view()\n  return { type = \"text\", text = \"x\" }\nend\n",
        PluginSource::User,
    );
    assert!(
        !listed
            .findings
            .iter()
            .any(|x| x.message.contains("宿主没有这一项权限")),
        "events.app 是清单内的一项，只是暂时没格子用它: {:?}",
        listed.findings
    );
}

/// 会被折回的字段值：说清"写了却像没写"，行号落在字段那一行。
#[test]
fn a_field_value_that_gets_folded_away_is_named_with_the_value() {
    let r = report(
        "PLUGIN_LIFECYCLE = \"Always\"\nfunction get_view()\n  return { type = \"text\", text = \"x\" }\nend\n",
        PluginSource::User,
    );
    let f = saying(&r, "Always").expect("大小写差一个字母就整句不生效");
    assert_eq!(f.level, Level::Error);
    assert_eq!(f.line, Some(2));
    // 这一条把"校验器的话"与"加载侧的结局"钉在一起：折回的是缺省那一档
    assert_eq!(
        r.manifest.lifecycle,
        focusflow_core::plugins::PluginLifecycle::Panel,
        "说会被折回缺省，就得当场证明折回的是哪一档"
    );

    // 数字那一档：PLUGIN_API = 0 折回缺省 1，比宿主上限还高的那一档另说一句
    let zero = report(
        "PLUGIN_API = 0\nfunction get_view()\n  return { type = \"text\", text = \"x\" }\nend\n",
        PluginSource::User,
    );
    let f = saying(&zero, "PLUGIN_API")
        .or(zero.findings.iter().find(|x| x.message.contains("正整数")))
        .expect("API 写成 0 要出声");
    assert_eq!(f.line, Some(2));
    assert_eq!(zero.manifest.api, 1, "缺省折回的是 1");

    let too_new = report(
        "PLUGIN_API = 9\nfunction get_view()\n  return { type = \"text\", text = \"x\" }\nend\n",
        PluginSource::User,
    );
    let f = saying(&too_new, "更高版本").expect("比宿主认得的还高要说出来");
    assert_eq!(f.level, Level::Error);
    assert_eq!(f.line, Some(2));
    // 反向腿：写到 2 是本宿主认得的最高档，不该挨这一句
    assert!(
        report("PLUGIN_API = 2\nfunction get_view()\n  return { type = \"text\", text = \"x\" }\nend\n", PluginSource::User)
            .findings
            .iter()
            .all(|x| !x.message.contains("更高版本")),
        "PLUGIN_API = 2 被判成「要更高版本」"
    );
}

/// 内置专属那一格：扩展位是问题、内置位一句不说（同一份文本，两条臂各查一次）。
#[test]
fn a_builtin_only_api_is_an_error_only_in_the_extension_slot() {
    let body = "PLUGIN_API = 2\nfunction get_view()\n  local s = focusflow.pomodoro_state()\n  return { type = \"text\", text = tostring(s) }\nend\n";
    let as_user = report(body, PluginSource::User);
    let f = saying(&as_user, "pomodoro_state").expect("这一格在扩展位会被摘掉，调用得到 nil");
    assert_eq!(f.level, Level::Error);
    assert_eq!(f.line, Some(4));

    let as_builtin = report(body, PluginSource::Builtin);
    assert!(
        as_builtin.findings.is_empty(),
        "内置位用内置那一格是本分，不该说一句: {:?}",
        as_builtin.findings
    );
}

/// 宿主根本没有的那一格：要说清调用会是 nil，并给行号。
#[test]
fn an_api_the_host_does_not_have_is_flagged() {
    let r = report(
        "PLUGIN_API = 2\nfunction init()\n  focusflow.teleport(1)\nend\n",
        PluginSource::User,
    );
    let f = saying(&r, "teleport").expect("没有这一格要说出来");
    assert_eq!(f.level, Level::Error);
    assert_eq!(
        f.line,
        Some(4),
        "第 1 行是 PLUGIN_NAME、第 2 行 PLUGIN_API、第 3 行函数头"
    );
    assert!(
        f.message.contains("nil"),
        "要说清症状是拿到 nil: {}",
        f.message
    );
    // 反向腿：`store.set` 是认得的动词，不该被念成"没有这一格"
    let known = report(
        "PLUGIN_API = 2\nPLUGIN_PERMISSIONS = { \"storage.write\" }\nfunction init()\n  focusflow.store.set(\"a\", 1)\nend\n",
        PluginSource::User,
    );
    assert!(
        !known
            .findings
            .iter()
            .any(|x| x.message.contains("没有这一格") || x.message.contains("没有这个动词")),
        "`store.set` 是认得的形状: {:?}",
        known.findings
    );
    // 动词那一档：`store.put` 不存在
    let bad_verb = report(
        "PLUGIN_API = 2\nfunction init()\n  focusflow.store.put(\"a\", 1)\nend\n",
        PluginSource::User,
    );
    assert!(
        bad_verb
            .findings
            .iter()
            .any(|x| x.message.contains("store.put")),
        "认不得的动词要说清是哪一处: {:?}",
        bad_verb.findings
    );
}

/// 注释里的调用**不算用过**：静态扫的是抹掉注释之后的那一份。
///
/// 少了这一条，"把用到的那一格注释掉"就会同时骗过两条判据 —— 声明必用那一半不再出声，
/// 而"没有这一格"那一半反而多出一句。
#[test]
fn a_call_inside_a_comment_counts_as_never_made() {
    // notify 只在注释里出现：既不该算"用到了"，也不该被念成"没有这一格"
    let r = report(
        "PLUGIN_API = 2\nPLUGIN_PERMISSIONS = { \"notify\" }\nfunction init()\n  -- focusflow.notify(\"到点了\")\n  focusflow.log(\"x\")\nend\n",
        PluginSource::User,
    );
    let f = saying(&r, "\"notify\"").expect("只在注释里用过的权限仍是多要的那一份");
    assert_eq!(f.level, Level::Note);
    assert_eq!(
        f.line,
        Some(3),
        "PLUGIN_PERMISSIONS 落在第 3 行（第 1 行是名字、第 2 行是版本）"
    );
    assert!(
        !r.findings.iter().any(|x| x.message.contains("没有这一格")),
        "注释里的那一格不该被当成一次真调用（那样还会多念一句）: {:?}",
        r.findings
    );
}

/// 内置模块写了 `PLUGIN_LIFECYCLE = "panel"`：位置压过声明，所以要提示"这句不生效"。
#[test]
fn a_builtin_module_asking_for_the_panel_tier_hears_that_it_is_always_on() {
    let body = "PLUGIN_LIFECYCLE = \"panel\"\nfunction get_view()\n  return { type = \"text\", text = \"x\" }\nend\n";
    let as_builtin = report(body, PluginSource::Builtin);
    let f = saying(&as_builtin, "不生效").expect("内置位一律常驻，写了 panel 也不生效");
    assert_eq!(f.level, Level::Note);
    assert_eq!(f.line, Some(2));
    // 反向腿：同一个声明在扩展位是真的生效，不该挨这一句
    assert!(
        report(body, PluginSource::User)
            .findings
            .iter()
            .all(|x| !x.message.contains("一律常驻")),
        "扩展位的 panel 是真的随面板装卸"
    );
}

/// 真数据腿：随包发布的那五份脚本按内置位校验，一条"问题"都不许有。
///
/// 这一条比任何夹具都值钱：它拿的是**今天真在跑**的那五份，而校验器第一次上线最容易
/// 把老脚本判成一堆错——那样作者拿到的第一个工具就是错的。
#[test]
fn the_five_shipped_scripts_have_nothing_to_complain_about() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("随包脚本的目录读不出来") {
        let path = entry.expect("目录条目").path();
        if path.extension().and_then(|e| e.to_str()) != Some("lua") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("随包脚本读不出来");
        let stem = path.file_stem().unwrap().to_string_lossy().to_string();
        let r = validate::validate(&stem, PluginSource::Builtin, &text);
        let errors: Vec<String> = r
            .findings
            .iter()
            .filter(|f| f.level == Level::Error)
            .map(|f| format!("{}: {f:?}", path.display()))
            .collect();
        assert!(
            errors.is_empty(),
            "{} 是随包发布的脚本，校验器对它说的「问题」就是它自己的缺陷：\n{}",
            stem,
            errors.join("\n")
        );
        checked += 1;
    }
    assert!(
        checked >= 5,
        "只扫到 {checked} 份随包脚本 —— 目录变了？这一条就白跑了"
    );
}

/// 校验器认得 `on` 这一格按**事件名**分的权限（与运行时那把尺子同源）。
///
/// 这一条盯的是两处分家：运行时的闸把事件名一起交给 `required_for`，而校验器的静态扫
/// 只认 `focusflow.<格>` 的话，`on("app_change")` 会被读成"用了 timer 那一格"——
/// 于是声明了 `events.app` 的插件被念成"多要的一份权限"，而只声明 timer 的那一份
/// 反而一切正常，直到运行时被 DENIED。
#[test]
fn the_lint_splits_the_on_api_by_event_name() {
    // 声明 events.app 且真的用它：两条都不该说
    let good = report(
        "PLUGIN_API = 2\nPLUGIN_PERMISSIONS = { \"events.app\" }\nfunction init()\n  focusflow.on(\"app_change\", function(name) end)\nend\n",
        PluginSource::User,
    );
    assert!(
        good.findings.is_empty(),
        "声明与用法对得上还说话: {:?}",
        good.findings
    );

    // 只声明 timer 却订 app_change：提前一行说清运行时会吃 DENIED
    let missing = report(
        "PLUGIN_API = 2\nPLUGIN_PERMISSIONS = { \"timer\" }\nfunction init()\n  focusflow.on(\"app_change\", function(name) end)\nend\n",
        PluginSource::User,
    );
    let f = saying(&missing, "events.app").expect("按事件名要的权限，校验器也要按事件名报");
    assert_eq!(f.level, Level::Error);
    assert_eq!(
        f.line,
        Some(5),
        "要说的是那一行订阅（1 名字 / 2 版本 / 3 声明 / 4 init 头）"
    );

    // 反向：只声明 events.app 却订 tick —— 那一格要的是 timer
    let other = report(
        "PLUGIN_API = 2\nPLUGIN_PERMISSIONS = { \"events.app\" }\nfunction init()\n  focusflow.on(\"tick\", function() end)\nend\n",
        PluginSource::User,
    );
    let f = saying(&other, "timer").expect("tick 要的是 timer，不能因为都是 on 就混过去");
    assert_eq!(f.level, Level::Error);

    // 事件名不是字面量的那一类：静态读不出来，就闭嘴（漏报比误报代价低）
    let dynamic = report(
        "PLUGIN_API = 2\nPLUGIN_PERMISSIONS = { \"timer\" }\nfunction init()\n  local which = \"app_change\"\n  focusflow.on(which, function(name) end)\nend\n",
        PluginSource::User,
    );
    assert!(
        !dynamic
            .findings
            .iter()
            .any(|x| x.message.contains("events.app") && x.level == Level::Error),
        "拼出来的事件名读不出来，不许编一个权限出来念: {:?}",
        dynamic.findings
    );
}

/// 干净的一份扩展插件：一条话都不许说。
///
/// 这一条是上面那批反向腿的合订本 —— 少了它，"永远出声"的实现可以一路绿着通过所有
/// 单向判据（本仓抓过这种假绿机器）。
#[test]
fn a_well_declared_extension_plugin_hears_nothing() {
    let r = report(
        "PLUGIN_API = 2\nPLUGIN_PERMISSIONS = { \"stats.read\", \"timer\" }\nfunction init()\n  focusflow.every(60, function() end)\nend\nfunction get_view()\n  local s = focusflow.stats(7)\n  return { type = \"text\", text = tostring(s) }\nend\n",
        PluginSource::User,
    );
    assert!(
        r.findings.is_empty(),
        "这份插件声明与用法都对得上，校验器却说: {:?}",
        r.findings
    );
}

/// 校验器必须走加载侧那一份解析器，而不是自己再抄一把尺子。
#[test]
fn the_validator_reuses_the_load_side_parser() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/plugins/validate.rs"),
    )
    .expect("validate.rs 读不出来");
    let body = src
        .split("#[cfg(test)]")
        .next()
        .expect("测试模块起点")
        .to_string();
    assert!(
        body.contains("declared_fields("),
        "校验器不走加载侧那份取法，两侧就会对同一份文件读出两个结果"
    );
    assert!(
        body.contains("build_manifest("),
        "折叠那一步要共用：校验器说的「会被折回缺省」必须真是加载侧的结局"
    );
    // 反向：不许自己再定义一套"顶层赋值怎么算"
    for forbidden in ["fn top_level_", "strip_prefix(\"PLUGIN_"] {
        assert!(
            !body.contains(forbidden),
            "校验器里又抄了一把字段尺子（{forbidden}）"
        );
    }
}
