//! 权限那一族的端到端用例：声明 → 授权 → 运行时那道闸（方案 3.7、验收 10/11/15/21）。
//!
//! 这里跑的是**经管理器加载的真插件**，不是直接调 `Gate::check`（那一层在
//! `permissions.rs` 自己的用例里）。理由与 `plugin_api_wiring.rs` 一样：症状出现在
//! 插件真调一句 `focusflow.xxx` 的那一刻，而"闸装上了但装错地方""授权之后没翻开"
//! 这类缺陷只有从这里才看得见。

use focusflow_core::config::FocusFlowConfig;
use focusflow_core::db;
use focusflow_core::paths;
use focusflow_core::plugins::manager::{PluginManager, PluginSource};
use focusflow_core::plugins::permissions::{Authorization, Grants};
use std::path::Path;

mod common;

/// 本文件的用例都动进程级 `app_dir`（授权文件落点从那儿算），与别的套互斥。
mod permissions_round {
    use std::sync::Mutex;
    pub(super) fn guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 一份会真去调宿主 API 的插件：调得动就把数写进 label，调不动就让整次取视图抛错。
///
/// `verb` 为空 = 问 `today_count`（要 `stats.read`）；非空 = 走 `store` 那一格
/// （`set` 要 `storage.write`、`get` 要 `storage.read`）。
fn probe_lua(name: &str, api: &str, permissions: &str, verb: &str) -> String {
    let call = if verb.is_empty() {
        "local n = focusflow.today_count()".to_string()
    } else {
        format!("local v = focusflow.store.{verb}(\"probe\", 7)")
    };
    format!(
        "PLUGIN_NAME = \"{name}\"\n\
         PLUGIN_API = {api}\n\
         PLUGIN_PERMISSIONS = {{ {permissions} }}\n\
         function get_view()\n  {call}\n\
         \x20 return {{ title = \"t\", widgets = {{ {{ type = \"label\", text = \"调到了\" }} }} }}\n\
         end\n"
    )
}

/// 建一份只放着这一个夹具的目录并加载它，交回（管理器, 插件名, 文件路径）。
///
/// 夹具落在**这一路真会被扫到的那一格**（用户 → `data_dir/plugins/`、内置 →
/// `app_dir/plugins/builtin/`）：`list_discovered()` 走的是 `discover()`，落在别的目录里
/// 的清单一条都读不到，用例就会在"这一份夹具该在清单里"上红，而那是夹具的问题不是产品的。
fn load(
    app: &Path,
    tag: &str,
    source: PluginSource,
    api: &str,
    permissions: &str,
    verb: &str,
) -> (PluginManager, String, std::path::PathBuf) {
    let dir = match source {
        PluginSource::User => paths::plugins_user_dir(),
        PluginSource::Builtin => paths::plugins_builtin_dir(),
        PluginSource::Legacy => paths::plugins_dir(),
    };
    std::fs::create_dir_all(&dir).expect("建临时插件目录");
    let file = dir.join(format!("{tag}_probe.lua"));
    let src = probe_lua(&format!("{tag}_probe"), api, permissions, verb);
    std::fs::write(&file, &src).expect("写夹具插件");
    let config: &'static FocusFlowConfig = Box::leak(Box::new(
        FocusFlowConfig::load(app.join("config.ini")).expect("读取 config.ini 失败"),
    ));
    let mut pm = PluginManager::new(config, db::Database::init_readonly());
    let name = pm
        .load_plugin(&file, source)
        .unwrap_or_else(|e| panic!("{tag} 那份夹具该加载得起来: {e}"));
    (pm, name, file)
}

/// 这一条插件在管理器里的授权读数（已加载的那一条取加载时算好的那份）。
fn auth_of(pm: &PluginManager, name: &str) -> Authorization {
    pm.get_plugin(name)
        .unwrap_or_else(|| panic!("{name} 该是加载着的"))
        .authorization
        .clone()
}

/// 用例 10：调一个**没声明**的 API ⇒ 被拒，且原因是"没声明"，而不是让人去点一个
/// 点了也没用的「授权」。
#[test]
fn an_undeclared_call_is_denied_naming_the_declaration() {
    let _g = permissions_round::guard();
    let app = common::isolated_app_dir("perm_undeclared");
    let (mut pm, name, _) = load(app.path(), "und", PluginSource::User, "2", "", "");
    let e = pm
        .refresh_view(&name)
        .expect_err("没声明 stats.read 就调 today_count，必须被拒");
    assert!(
        e.contains("DENIED") && e.contains("没在 PLUGIN_PERMISSIONS 里声明"),
        "被拒的这句话要指到作者能改的那一格，实得: {e}"
    );
    assert!(
        e.contains("stats.read"),
        "必须点名是哪一项权限，否则作者只能猜: {e}"
    );
    assert!(
        !e.contains("插件页点"),
        "没声明的事不许说成没授权（用户点了也没用）: {e}"
    );
}

/// 声明了但用户没点同意 ⇒ 照常加载、这一问被拒、话说的是「还没授权」。
/// 方案 3.7 那句"未授权不是拒绝加载"在这里核的是同一件事。
#[test]
fn a_declared_but_ungranted_call_still_loads_and_says_grant_first() {
    let _g = permissions_round::guard();
    let app = common::isolated_app_dir("perm_ungranted");
    let (mut pm, name, _) = load(
        app.path(),
        "ungr",
        PluginSource::User,
        "2",
        "\"stats.read\"",
        "",
    );
    let e = pm
        .refresh_view(&name)
        .expect_err("没授权过就该被拒，而不是回一个 0 装作读到了");
    assert!(
        e.contains("还没授权") && e.contains("stats.read"),
        "这句话得让用户知道下一步是点授权: {e}"
    );
    assert_eq!(
        auth_of(&pm, &name),
        Authorization::NotGranted,
        "已加载的这一条要念得出「需要授权」"
    );
    assert!(
        auth_of(&pm, &name).needs_action(),
        "这一档要给「授权」那颗按钮"
    );
    // 停用之后走的是**另一条算路**（未加载 ⇒ 现读文件算），两处必须同一句话：
    // 分家了的症状是同一个插件在页面上有两张脸，而用户按其中一张点了按钮。
    pm.set_enabled("ungr_probe", false).expect("停用该成功");
    let off = pm
        .list_discovered()
        .expect("清单该读得动")
        .into_iter()
        .find(|p| p.file == "ungr_probe")
        .expect("停用不等于不在清单里")
        .authorization;
    assert_eq!(
        off,
        Authorization::NotGranted,
        "未加载那一条与已加载那一条分家了"
    );
}

/// 点一次「授权」：当场放行、当场落盘，而落的那一格必须是备份会扫到的那一棵。
///
/// 这一条同时是验收 11（内置不弹授权）的另一半：内置那一档同样的调用**不被拦**。
#[test]
fn granting_once_from_the_manager_opens_the_gate_and_lands_on_disk() {
    let _g = permissions_round::guard();
    let app = common::isolated_app_dir("perm_grant");
    let (mut pm, name, _) = load(
        app.path(),
        "grt",
        PluginSource::User,
        "2",
        "\"stats.read\"",
        "",
    );
    pm.refresh_view(&name).expect_err("先确认没授权时是被拒的");
    let note = pm
        .grant_plugin("grt_probe")
        .expect("落一份授权记录不该失败");
    assert_eq!(note, "已授权", "点完当场就该改口: {note}");
    pm.refresh_view(&name)
        .expect("授权之后同一个调用该放行，不该要求重新加载");
    let landed = paths::plugins_state_dir()
        .join("grt_probe")
        .join("grants.json");
    assert!(
        landed.is_file(),
        "授权没落在备份扫的那一棵子树里，换机器就等于所有人都重新点一遍: {}",
        landed.display()
    );
    // 内置模块同一句调用不经过闸（免授权，方案 3.7）
    let (mut bmp, bname, _) = load(app.path(), "builtin", PluginSource::Builtin, "2", "", "");
    bmp.refresh_view(&bname).expect("内置模块不该被权限闸拦住");
    let listed_auth = auth_of(&bmp, &bname);
    assert_eq!(
        listed_auth,
        Authorization::Exempt,
        "内置那一档页面上要写免授权，而不是「需要授权」"
    );
}

/// 只改代码（字节变了、权限集合没变）⇒ 授权保留 + 页面提示「插件已更新」（验收 15）。
///
/// 这一条是方案 3.3 那条取舍的全部代价所在：绑全文件哈希的话，作者每修一个 typo
/// 就要让用户重新点一次同意，结局是用户不看提示一路点过去。
#[test]
fn editing_only_the_code_keeps_the_grant_and_flags_the_content() {
    let _g = permissions_round::guard();
    let app = common::isolated_app_dir("perm_content");
    let (mut pm, name, file) = load(
        app.path(),
        "cont",
        PluginSource::User,
        "2",
        "\"stats.read\"",
        "",
    );
    pm.grant_plugin("cont_probe").expect("授权该落得下去");
    pm.refresh_view(&name).expect("刚授完就该放行");
    // 改一行注释（权限一字未动），再重载一次 = 用户自己更新插件那一步。
    // 注释符号是 `--`：这是 Lua，写成 `//` 会让下面那条「重载该成功」变成语法错误。
    let edited = std::fs::read_to_string(&file).expect("读回夹具").replacen(
        "PLUGIN_API",
        "-- 修了一个小问题\nPLUGIN_API",
        1,
    );
    std::fs::write(&file, &edited).expect("写回夹具");
    assert!(
        pm.reload_plugin("cont_probe"),
        "重载本身该成功（改的只是一行注释）"
    );
    pm.refresh_view(&name)
        .expect("权限集合没变 ⇒ 授权必须留着，同一个调用该照样放行");
    let listed_auth = auth_of(&pm, &name);
    assert_eq!(
        listed_auth,
        Authorization::ContentChanged,
        "页面上要能说出「代码变了但权限没变」这句，而不是让人以为还要再授权"
    );
    assert!(
        !listed_auth.needs_action(),
        "这一档不该给「授权」按钮（没有东西可点）"
    );
}

/// 加一项权限 ⇒ 旧授权作废，点名新增了哪一项（验收 21 的前半截）。
#[test]
fn adding_a_permission_voids_the_grant_and_names_the_new_one() {
    let _g = permissions_round::guard();
    let app = common::isolated_app_dir("perm_regrant");
    let (mut pm, name, file) = load(
        app.path(),
        "regr",
        PluginSource::User,
        "2",
        "\"stats.read\"",
        "",
    );
    pm.grant_plugin("regr_probe").expect("授权该落得下去");
    // 作者把声明扩到 storage.write，并改用 store.set（这一次调用要的是新那项）
    let widened = std::fs::read_to_string(&file)
        .expect("读回夹具")
        .replace("\"stats.read\"", "\"stats.read\", \"storage.write\"")
        .replace(
            "local n = focusflow.today_count()",
            "local v = focusflow.store.set(\"probe\", 7)",
        );
    std::fs::write(&file, &widened).expect("写回夹具");
    assert!(pm.reload_plugin("regr_probe"), "改过声明的那份该重载成功");
    let e = pm
        .refresh_view(&name)
        .expect_err("权限集合变了，旧授权不许继续生效");
    assert!(e.contains("还没授权") && e.contains("storage.write"), "{e}");
    let listed_auth = auth_of(&pm, &name);
    assert_eq!(
        listed_auth,
        Authorization::PermissionChanged {
            newly: vec!["storage.write".to_string()]
        },
        "页面上要点出新增的是哪一项，而不是只说「要重新授权」"
    );
    assert!(listed_auth.needs_action(), "这一档要给按钮");
}

/// 老插件（没写 `PLUGIN_API` = v1）不弹授权、行为不变（方案 5 那张迁移表）。
///
/// 这一条挡住的是"一上线把所有存量脚本一次停掉"：今天满盘的 `.lua` 都没写这一项，
/// 若按 v2 处理，症状是升级之后每个插件都写着「需要授权」而它们一个权限都没要过。
#[test]
fn a_v1_user_plugin_is_not_asked_to_grant_anything() {
    let _g = permissions_round::guard();
    let app = common::isolated_app_dir("perm_v1");
    let (mut pm, name, _) = load(app.path(), "v1", PluginSource::User, "1", "", "");
    pm.refresh_view(&name)
        .expect("v1 那份调用不该被权限闸拦住（行为与今天一致）");
    let listed_auth = auth_of(&pm, &name);
    assert_eq!(
        listed_auth,
        Authorization::LegacyApi,
        "页面上要说「按旧版接口运行」，别写成需要授权"
    );
}

/// 回滚开关那一档（旧平铺目录）仍然按今天的全量行为跑：用例 20 的另一半。
///
/// `legacy_layout = true` 的意义就是"线出问题时退回旧行为"，而旧行为里第三方脚本
/// 看得见内置专属那一组、也不走授权 —— 在这一档偷偷加闸，等于让回滚开关本身变成一次变更。
#[test]
fn the_legacy_rollback_branch_stays_exempt() {
    let _g = permissions_round::guard();
    let app = common::isolated_app_dir("perm_legacy");
    let (mut pm, name, _) = load(
        app.path(),
        "lg",
        PluginSource::Legacy,
        "2",
        "\"stats.read\"",
        "",
    );
    pm.refresh_view(&name).expect("回滚那一档不该被权限闸拦住");
    let listed_auth = auth_of(&pm, &name);
    assert_eq!(listed_auth, Authorization::Exempt);
}

/// 撤销授权必须真的回到"没授权过"那一档 —— 与「停用插件」是两件事。
///
/// 为什么单列：`revoke` 只删文件不翻闸的话，屏上写着「需要授权」而插件继续能调，
/// 那句提示就成了装饰；反过来翻闸不删文件，重启又活了（本仓为"屏上说了盘上没动"
/// 修过一整个族）。两条都要断。
#[test]
fn revoking_a_grant_closes_the_gate_and_clears_the_file() {
    let _g = permissions_round::guard();
    let app = common::isolated_app_dir("perm_revoke");
    let (mut pm, name, _) = load(
        app.path(),
        "rev",
        PluginSource::User,
        "2",
        "\"storage.read\"",
        "get",
    );
    pm.grant_plugin("rev_probe").expect("授权该落得下去");
    pm.refresh_view(&name)
        .expect("store.get 要的是 storage.read，刚授完该放行");
    let note = pm.revoke_plugin("rev_probe").expect("撤销该成功");
    assert!(note.contains("需要授权"), "撤销之后当场改口: {note}");
    let e = pm
        .refresh_view(&name)
        .expect_err("闸必须跟着合上，不能只删文件等下一次重载");
    assert!(e.contains("还没授权"), "{e}");
    let landed = paths::plugins_state_dir()
        .join("rev_probe")
        .join("grants.json");
    assert!(!landed.is_file(), "撤销没落到盘上: {}", landed.display());
}

/// 那份记录是**给人读也机读**的：两个哈希各管一件事，字段名与方案 3.7 写死的一致。
#[test]
fn the_grants_record_carries_both_hashes_and_the_named_set() {
    let _g = permissions_round::guard();
    let app = common::isolated_app_dir("perm_record");
    std::fs::create_dir_all(app.path()).expect("建临时目录");
    let declared = vec!["stats.read".to_string()];
    let record = Grants {
        permission_hash: focusflow_core::plugins::permissions::permission_hash(&declared),
        content_hash: focusflow_core::plugins::permissions::content_hash(b"local x = 1"),
        granted: declared.clone(),
    };
    focusflow_core::plugins::permissions::write_grants("record_probe", &record)
        .expect("写一份记录");
    let raw = std::fs::read_to_string(
        paths::plugins_state_dir()
            .join("record_probe")
            .join("grants.json"),
    )
    .expect("读回原文");
    for key in ["permission_hash", "content_hash", "granted"] {
        assert!(
            raw.contains(key),
            "字段名要和方案里那一段一致，少了 {key}: {raw}"
        );
    }
    assert!(raw.starts_with('{'), "整份是 JSON 对象: {raw}");
}
