//! 插件生命周期与进程级单例的回归测试。
//!
//! 这三件事原先都没有测试覆盖（`manager.rs` 只测了沙箱与指令上限），
//! 而它们恰好是"错了也不报错"的那类：调度线程被别的插件拆掉是无声的，
//! 存一次文件把用户的番茄钟判成"完成一个"也是无声的。

#![cfg(feature = "test-utils")]

use std::sync::Mutex;

use focusflow_core::config::FocusFlowConfig;
use focusflow_core::db;
use focusflow_core::paths;
use focusflow_core::plugins::host;
use focusflow_core::plugins::manager::PluginManager;
use focusflow_core::pomodoro;

/// app_dir / 进程级单例都是全局状态，整组用例串行；poison 后继续跑别的用例。
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

/// 每个用例用**自己**的 config 对象：`config::instance()` 是另一个进程级全局，
/// 断言若去读它就只是在读一个没被写过的对象。
fn manager_in(tmp: &std::path::Path) -> (PluginManager, &'static FocusFlowConfig) {
    db::queries::invalidate_years_cache();
    let config: &'static FocusFlowConfig = Box::leak(Box::new(
        FocusFlowConfig::load(tmp.join("config.ini")).expect("临时配置应能载入"),
    ));
    (
        PluginManager::new(config, db::Database::init_readonly()),
        config,
    )
}

/// 一个用日程 API、并在 cleanup 里按文档回收调度线程的插件。
fn scheduler_user(name: &str) -> String {
    format!(
        r###"
PLUGIN_NAME = "{name}"
function init() focusflow.scheduler_tasks() end
function cleanup() focusflow.scheduler_shutdown() end
function get_view() return {{ title = "{name}", widgets = {{ {{type="label", text="ok"}} }} }} end
"###
    )
}

/// 停用**其中一个**插件不能把别的插件还在用的调度线程一起拆掉。
///
/// 旧实现是 `stop_scheduler()` 无条件 take：cleanup 里照 `host.rs` 的说明调
/// `scheduler_shutdown()` 是文档推荐写法，于是任何一个 such 插件被停用/删除，
/// 内置「定时任务」就到点不再启动程序，而且没有任何提示。
#[test]
fn unloading_one_scheduler_user_keeps_it_running_for_the_others() {
    let _g = guard();
    let tmp = paths::test_app_dir("sched_users");
    let a = write_plugin(tmp.path(), "sched_a", &scheduler_user("调度甲"));
    let b = write_plugin(tmp.path(), "sched_b", &scheduler_user("调度乙"));
    let (mut pm, _) = manager_in(tmp.path());

    pm.load_plugin(&a).expect("加载甲失败");
    assert_eq!(
        host::scheduler_state_for_test(),
        Some(1),
        "甲用过之后应当在跑"
    );
    pm.load_plugin(&b).expect("加载乙失败");
    assert_eq!(
        host::scheduler_state_for_test(),
        Some(2),
        "两个用户都该被记上"
    );

    assert!(pm.unload_plugin("调度甲"), "甲应被卸载");
    assert_eq!(
        host::scheduler_state_for_test(),
        Some(1),
        "只剩乙时线程必须还在跑（这里旧代码给出 None，就是那个无声的定时任务失效）"
    );

    assert!(pm.unload_plugin("调度乙"), "乙应被卸载");
    assert_eq!(
        host::scheduler_state_for_test(),
        None,
        "最后一个用户走了才回收"
    );
}

/// 热重载不该跑 cleanup：番茄插件的 cleanup 会 `pomodoro_stop()`，而那段
/// `save_current()` 只要实际计时 ≥1 秒就给 `work_finished += 1` —— 用户只是存了
/// 一次文件，跑到一半的番茄钟就被判成"完成一个"。真正的停用/删文件仍然要跑。
#[test]
fn reloading_a_plugin_does_not_finish_the_running_tomato() {
    let _g = guard();
    let tmp = paths::test_app_dir("pomo_reload");
    let body = r###"
PLUGIN_NAME = "番茄重载入"
function init() focusflow.pomodoro_start_work() end
function cleanup() focusflow.pomodoro_stop() focusflow.pomodoro_shutdown() end
function get_view() return { title = "t", widgets = { {type="label", text="v"} } } end
"###;
    let p = write_plugin(tmp.path(), "pomo_reload", body);
    let (mut pm, _) = manager_in(tmp.path());

    pm.load_plugin(&p).expect("加载失败");
    // 计时线程每秒 tick 一次，elapsed 要到 ≥1 秒才会真的落一条会话记录
    std::thread::sleep(std::time::Duration::from_millis(1400));

    assert!(pm.reload_plugin("pomo_reload"), "重载应当成功");
    assert_eq!(
        pomodoro::get_recent_sessions(10).len(),
        0,
        "存一次文件不该落出一条已完成的番茄记录"
    );

    // 反向腿：卸载（= 停用）必须照旧回收，否则上面那条断言只是"什么都没写"的假绿
    assert!(pm.unload_plugin("番茄重载入"), "卸载应当成功");
    let rows = pomodoro::get_recent_sessions(10);
    assert_eq!(
        rows.len(),
        1,
        "cleanup 跑了就该留下这一段（同时证明这条测试不是空库假绿）"
    );
    assert!(rows[0].actual_seconds >= 1, "{:?}", rows[0].actual_seconds);
}

/// 动作已经执行完、只有渲染出错时，不能报成"动作失败"。
///
/// 与 `set_enabled` 修过的同一类谎报：插件页点了「删除任务」，任务确实删了，
/// 界面却弹「插件动作失败」。
#[test]
fn action_success_is_not_reported_as_failure_when_only_the_view_breaks() {
    let _g = guard();
    let tmp = paths::test_app_dir("action_view");
    let body = r###"
PLUGIN_NAME = "渲染会坏"
local acted = false
function on_action(id)
  if id == "go" then acted = true end
  if id == "boom" then error("动作本身失败") end
end
function get_view()
  if acted then error("渲染炸了") end
  return { title = "t", widgets = { {type="label", text="v"} } }
end
"###;
    let p = write_plugin(tmp.path(), "action_view", body);
    let (mut pm, _) = manager_in(tmp.path());
    let name = pm.load_plugin(&p).expect("加载失败");

    assert!(
        pm.plugin_action(&name, "go").is_ok(),
        "动作成功、只有 get_view 报错时不该返回 Err"
    );
    assert!(
        pm.get_plugin(&name).expect("插件应还在").loaded,
        "只有渲染出错不该把插件标成不可用"
    );
    // 反向腿：动作本身失败时仍然必须报错
    assert!(pm.plugin_action(&name, "boom").is_err());
}

/// `[plugins] disabled` 是逗号分隔、写盘不转义的，所以文件名必须真实存在：
/// 一个带 `,` 的入参能顺手停用别的插件，带换行的能往 config.ini 注入任意行。
#[test]
fn only_real_plugin_files_can_be_persisted() {
    let _g = guard();
    let tmp = paths::test_app_dir("set_enabled_guard");
    write_plugin(tmp.path(), "real_one", "PLUGIN_NAME = \"真插件\"\n");
    let (mut pm, config) = manager_in(tmp.path());
    pm.load_all();

    pm.set_enabled("real_one", false).expect("停用不该报错");
    assert!(pm.is_disabled("real_one"), "真实文件名应当被记下并生效");
    assert_eq!(config.get_or("plugins", "disabled", ""), "real_one");

    // 注入腿：不合法的入参一律**报错**，不再"忽略后回 Ok"。
    // 以前这里是 `return Ok(())`，而命令层把 Ok 当成功、前端弹「插件已启用」——
    // 对着一个插件目录里根本没有的名字报成功，配置一个字没改。
    for bad in [
        "evil\n[injected]\nx = 1",
        "real_one,another",
        "not_a_plugin",
    ] {
        let r = pm.set_enabled(bad, false);
        assert!(
            r.is_err(),
            "非法/未知插件名必须报错，不该静默成功: {bad:?} -> {r:?}"
        );
        assert_eq!(
            config.get_or("plugins", "disabled", ""),
            "real_one",
            "{bad:?} 不该改动停用列表"
        );
    }
    // 启用方向的同一道闸：不能靠传假名字把列表洗空
    assert!(pm.set_enabled("ghost", true).is_err());
    assert_eq!(config.get_or("plugins", "disabled", ""), "real_one");
}

/// 启用一个"加载就会失败"的插件：必须返回 Err 并带上插件自己报的原因。
///
/// 前端那句 toast 一直把 Ok 当成成功（plugins.js 弹「插件已启用」）。以前
/// set_enabled 拿不到加载结果，所以带语法错误的插件会"启用成功"，只有下面那行
/// 小字写着加载失败 —— 与刚修掉的「停用误报失败」是同一个开关的两个方向。
#[test]
fn enabling_a_broken_plugin_reports_the_reason() {
    let _g = guard();
    let tmp = paths::test_app_dir("enable_broken");
    write_plugin(
        tmp.path(),
        "broken_enable",
        "PLUGIN_NAME = \"启用即失败\"\nerror(\"这行不该通过\")\n",
    );
    let (mut pm, _) = manager_in(tmp.path());

    let err = pm
        .set_enabled("broken_enable", true)
        .expect_err("加载失败必须报给调用方");
    assert!(err.contains("加载失败"), "{err}");
    assert!(err.contains("这行不该通过"), "原因要原样带出来: {err}");
    assert!(
        pm.list_discovered()
            .iter()
            .any(|p| p.file == "broken_enable" && !p.loaded),
        "该插件不该被当成已加载"
    );
}

/// 两个文件声明同一个 PLUGIN_NAME 时，原先是后来者静默顶掉前者：被顶掉的那个
/// 不再接收键事件、也没有 unload 入口（cleanup 永不执行），文件却还在盘上。
#[test]
fn duplicate_plugin_name_is_rejected_instead_of_evicting() {
    let _g = guard();
    let tmp = paths::test_app_dir("dup_name");
    let first = write_plugin(tmp.path(), "dup_a", "PLUGIN_NAME = \"撞名\"\n");
    let second = write_plugin(
        tmp.path(),
        "dup_b",
        "PLUGIN_NAME = \"撞名\"\nfunction get_view() return { title = \"b\" } end\n",
    );
    let (mut pm, _) = manager_in(tmp.path());

    assert_eq!(pm.load_plugin(&first).expect("加载甲失败"), "撞名");
    let err = pm.load_plugin(&second).expect_err("撞名的第二个应当被拒");
    assert!(err.contains("重名"), "{err}");
    assert_eq!(
        pm.get_plugin("撞名").map(|p| p.file_path.clone()),
        Some(first),
        "已加载的那个不该被顶掉"
    );
    // 同一个文件重新加载不算撞名
    assert!(pm.reload_plugin("dup_a"), "重载自身不该被当成撞名");
}

/// 插件管理页列表不该执行插件代码。
///
/// `list_discovered` 原先对"没在内存里的"插件回退到 `read_meta`，而那是 `exec()`
/// 整段脚本：已停用的插件、加载失败的插件，每开一次插件页、每来一次
/// `plugins-reloaded` 都要在主线程上把顶层代码再跑一遍 —— 与 `mod.rs` 里
/// 「停用的不执行其代码」的承诺正好相反。
#[test]
fn listing_plugins_never_runs_their_code() {
    let _g = guard();
    let tmp = paths::test_app_dir("list_no_exec");
    // 顶层直接 error：这段一旦被执行，read_meta 就把这条错误当成插件的"状态"显示出来
    write_plugin(
        tmp.path(),
        "top_level_boom",
        "PLUGIN_NAME = \"顶层会炸\"\nerror(\"不该被执行\")\n",
    );
    let (mut pm, config) = manager_in(tmp.path());
    // 停用它：连加载都不该发生
    pm.set_enabled("top_level_boom", false)
        .expect("停用不该报错");
    assert!(config
        .get_or("plugins", "disabled", "")
        .contains("top_level_boom"));

    let listed = pm.list_discovered();
    let entry = listed
        .iter()
        .find(|p| p.file == "top_level_boom")
        .expect("停用的插件也该出现在列表里");
    assert_eq!(entry.name, "顶层会炸", "元数据要靠扫描拿到");
    assert!(!entry.enabled);
    assert!(
        entry.error.is_none(),
        "只是列个表，不该有插件报错（报错说明顶层代码被执行了）: {:?}",
        entry.error
    );
}

/// 加载失败的原因必须能在列表里看到：改用扫描取元数据之后，`read_meta` 那条
/// 顺带报错的路子没了，不另记一笔的话插件页就只剩"未加载"三个字。
#[test]
fn load_failure_is_visible_in_the_list() {
    let _g = guard();
    let tmp = paths::test_app_dir("load_error_visible");
    write_plugin(
        tmp.path(),
        "broken",
        "PLUGIN_NAME = \"坏掉的\"\nerror(\"顶层就是炸\")\n",
    );
    let (mut pm, _) = manager_in(tmp.path());
    pm.load_all();

    let entry = pm
        .list_discovered()
        .into_iter()
        .find(|p| p.file == "broken")
        .expect("应列出该文件");
    assert_eq!(entry.name, "坏掉的", "展示名照样扫得出来");
    assert!(!entry.loaded);
    let err = entry
        .error
        .expect("加载失败的原因必须显示出来，不能只有一行未加载");
    assert!(
        err.contains("顶层就是炸") || err.contains("执行失败"),
        "{err}"
    );
}

/// `[plugins] instruction_limit` 必须是**每次调用**的预算，不是这个 Lua 状态
/// 一辈子的累计账。
///
/// 钩子是 `every_nth_instruction(N)` + 回调**无条件** Err，而 Lua 的 count 计数器
/// 挂在 `lua_State` 上、只有 `lua_sethook` 会把它重置 —— 所以"创建状态时装一次"
/// 的语义是：这个状态**累计**跑满 N 条指令，就把当时那次调用掐死。
/// 插件详情页每 2 秒渲染一次 `get_view()`，挂机开着迟早踩中，然后
/// `mark_plugin_error` 把一个完全正常的插件判成"疑似死循环"停用 ——
/// 界面停在旧数据、插件页一行红字，不重启/不停用再启用就不自愈。
#[test]
fn instruction_budget_is_per_call_not_per_state() {
    let _g = guard();
    let tmp = paths::test_app_dir("instr_budget");
    std::fs::write(
        tmp.path().join("config.ini"),
        "[plugins]\nenabled = true\ninstruction_limit = 20000\n",
    )
    .unwrap();
    let p = write_plugin(
        tmp.path(),
        "busy_but_fine",
        r###"
PLUGIN_NAME = "忙碌但正常"
function get_view()
  local s = 0
  for i = 1, 200 do s = s + i end
  return { title = "t", widgets = { {type="label", text=tostring(s)} } }
end
"###,
    );
    let (mut pm, _) = manager_in(tmp.path());
    pm.load_plugin(&p).expect("加载这个插件本该成功");

    let mut first_fail: Option<(u32, String)> = None;
    for k in 1..=60u32 {
        if let Err(e) = pm.refresh_view("忙碌但正常") {
            first_fail = Some((k, e));
            break;
        }
    }
    if let Some((k, e)) = first_fail {
        panic!(
            "第 {k} 次渲染就被判定超限: {e}\n\
             每次 get_view() 的工作量都很小（200 次加法 + 一张表），\
             超限只该发生在**单次调用**跑满预算时；累计计数会把长会话里的正常插件误杀。"
        );
    }
}

/// 自引用的控件表必须被深度闸拦下，而不是把线程栈打穿。
///
/// `row.children` / `modal_form.widgets` 是递归下降解析的，而那张表来自插件文件
/// = 外部数据。修之前没有任何深度上限：`w.children[1] = w` 就是无界递归 → 栈溢出
/// 是 SIGSEGV —— 不走 panic hook（那场刚给 hook 加的 flush 也救不了它），
/// release 又是 abort + 无控制台，症状是"开着开着程序没了"且零线索。
#[test]
fn a_self_referencing_widget_table_is_truncated_not_fatal() {
    let _g = guard();
    let tmp = paths::test_app_dir("widget_cycle");
    let p = write_plugin(
        tmp.path(),
        "selfref",
        r###"
PLUGIN_NAME = "自引用控件"
function get_view()
  local w = { type = "row", children = {} }
  w.children[1] = w
  return { title = "t", widgets = { w } }
end
"###,
    );
    let (mut pm, _) = manager_in(tmp.path());
    pm.load_plugin(&p).expect("加载本身不该失败");
    assert!(
        pm.refresh_view("自引用控件").is_ok(),
        "渲染必须能返回，而不是把栈打穿"
    );
    assert!(
        pm.get_plugin("自引用控件").is_some(),
        "一个控件解析不了不该把整个插件判成错误"
    );
}
