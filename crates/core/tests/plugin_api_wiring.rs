//! 接线审计：内置插件写的每一个 `focusflow.xxx`，宿主必须真的注册着。
//!
//! 家族同旧桌面端入口里那份"窗口 / capabilities / 事件名"对齐审计（那棵树已随 WebView 删了，
//! 现在这条审计是这一族仅剩的一份）：名字改一处，
//! 另一处不会红，表现是"按钮点下去没反应""面板空白"—— mlua 报
//! `attempt to call a nil value`，而 manager 只在 `init()` / `get_view()` 真跑到那句时
//! 才出声（宿主 API 名散在 5 个 `.lua` 与 `host.rs` 两处，人手对齐迟早会漏）。
//!
//! 注册侧取自**真跑起来的 Lua 环境**（`register_host_api` 之后读全局 `focusflow` 表），
//! 不是 grep `host.rs` 的源码：那里的 `host.set("名字",` 有多处实参跨行，
//! 按行解析会静默漏掉（本场就错过一次，漏出来的是一份假的"8 个 API 没人注册"）。

use focusflow_core::config::FocusFlowConfig;
use focusflow_core::db;
use focusflow_core::paths;
use focusflow_core::plugins::host;
use focusflow_core::plugins::PluginSource;
use mlua::Lua;
use std::path::Path;

mod common;

/// 串行锁：本文件要把进程级 `app_dir` 锚到自己的临时程序目录，与同 crate
/// 其它动 app_dir 的测试互斥。
mod round7 {
    use std::sync::Mutex;
    pub(super) fn guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 真环境里注册着 API 表 `focusflow`，把它的键名全列出来。
fn registered_names(dir: &Path) -> Vec<String> {
    let config: &'static FocusFlowConfig = Box::leak(Box::new(
        FocusFlowConfig::load(dir.join("config.ini")).expect("读取 config.ini 失败"),
    ));
    let lua = Lua::new();
    host::register_host_api(&lua, config, db::Database::init_readonly(), "wiring_audit")
        .expect("注册宿主 API 失败");
    // 用 Lua 自己列键名：`register_host_api` 结尾会把表设成全局 `focusflow`，
    // 而插件读到的正是那个全局 —— 审计对象必须是插件实际看到的东西。
    let names: Vec<String> = lua
        .load(
            "local out = {} for k in pairs(focusflow) do out[#out + 1] = k end table.sort(out) return out",
        )
        .eval()
        .expect("列出 focusflow 表的键名失败");
    names
}

/// 扫一份 Lua 源码，取出它调用的宿主 API 名；注释与字符串字面量里的不算。
///
/// 为什么要剥注释：`accounting_plugin.lua` 的文件头写着"全部通过 focusflow.accounting_*
/// 一组 API"，字符串 `"数据来自 focusflow.stats API"` 也在 `stats_overview.lua` 里 ——
/// 不剥就会把文档句子当成调用，报出一堆假红。
fn used_names(src: &str) -> Vec<String> {
    let b = src.as_bytes();
    let prefix = b"focusflow.";
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        // `--[[ 长注释 ]]` 与 `-- 行注释`
        if b[i..].starts_with(b"--") {
            let after = i + 2;
            if b[after.min(b.len())..].starts_with(b"[[") {
                // 只认 `--[[`：`-- [x]` 这种行注释不能当成块注释，否则会把后面的代码
                // 一路吞到下一个 `]]`（那才是真的假绿）。
                match find_bytes(&b[after..], b"]]") {
                    Some(rel) => {
                        i = after + rel + 2;
                        continue;
                    }
                    None => break,
                }
            }
            match find_bytes(&b[after..], b"\n") {
                Some(rel) => i = after + rel,
                None => break,
            }
            continue;
        }
        // 字符串字面量（单/双引号，含转义）整体跳过
        if b[i] == b'"' || b[i] == b'\'' {
            let quote = b[i];
            let mut j = i + 1;
            while j < b.len() {
                if b[j] == b'\\' {
                    j += 2;
                    continue;
                }
                if b[j] == quote {
                    break;
                }
                j += 1;
            }
            i = j.saturating_add(1).min(b.len());
            continue;
        }
        // `[[ 长字符串 ]]`
        if b[i..].starts_with(b"[[") {
            match find_bytes(&b[i + 2..], b"]]") {
                Some(rel) => {
                    i = i + 2 + rel + 2;
                    continue;
                }
                None => break,
            }
        }
        if b[i..].starts_with(prefix) {
            let start = i + prefix.len();
            let mut j = start;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            if j > start {
                out.push(String::from_utf8_lossy(&b[start..j]).to_string());
            }
            i = j.max(i + 1);
            continue;
        }
        // 方括号写法 `focusflow["stats"](…)`：漏了它就是假绿
        if b[i..].starts_with(b"focusflow[") {
            let after = i + b"focusflow[".len();
            if after < b.len() && (b[after] == b'"' || b[after] == b'\'') {
                let quote = b[after];
                let mut j = after + 1;
                while j < b.len() && b[j] != quote {
                    j += 1;
                }
                out.push(String::from_utf8_lossy(&b[after + 1..j]).to_string());
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out.sort();
    out.dedup();
    out
}

fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[test]
fn bundled_plugins_only_call_registered_host_apis() {
    let _g = round7::guard();
    // 夹具必须活到本用例结束：`paths::plugins_builtin_dir()` 读的是进程级 app_dir，
    // 而注册表也是在同一个目录里建起来的。
    let app = common::isolated_app_dir("wiring_audit");
    let registered = registered_names(app.path());
    // 反向断言：注册表必须真的列出来了。空表会让下面每一条都"全对"，那是假绿。
    assert!(
        registered.len() >= 40,
        "focusflow 表只列出 {} 个键名，审计本身失效: {registered:?}",
        registered.len()
    );
    for anchor in ["stats", "today_count", "log", "pomodoro_state"] {
        assert!(
            registered.iter().any(|n| n == anchor),
            "注册表里连 {anchor} 都没有 ⇒ 是列举环节坏了，不是插件写错名: {registered:?}"
        );
    }

    let dir = paths::plugins_builtin_dir();
    let files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("插件目录读不出来（{}）: {e}", dir.display()))
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "lua").unwrap_or(false))
        .collect();
    assert!(
        files.len() >= 5,
        "内置插件应当有 5 份，实际 {} 份（{}）⇒ 夹具没装进来，审计会是空的",
        files.len(),
        dir.display()
    );

    let mut total_used: Vec<String> = Vec::new();
    for path in &files {
        let src = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("读 {} 失败: {e}", path.display()));
        let used = used_names(&src);
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            !used.is_empty(),
            "{name} 一个宿主 API 都没调用？多半是扫描器被改坏了（那等于这条用例失效）"
        );
        for api in &used {
            total_used.push(api.clone());
            assert!(
                registered.iter().any(|r| r == api),
                "{name} 调用 focusflow.{api}()，而宿主注册表里没有这个名字 —— \
                 运行时会是 attempt to call a nil value（界面上就是点了没反应）。注册名共 {} 个",
                registered.len()
            );
        }
    }
    total_used.sort();
    total_used.dedup();
    assert!(
        total_used.len() >= 15,
        "内置插件合起来只认出 {} 个 API 名，扫描器覆盖面可疑: {total_used:?}",
        total_used.len()
    );
}

/// 扫描器自己的两条腿：注释与字符串里的名字必须被剥掉，代码里的必须留下。
///
/// 这条不是给"代码更漂亮"用的：判据全靠这个解析器，它认错一次就是一次假绿或假红。
#[test]
fn the_lua_scanner_ignores_comments_and_strings() {
    let src = "
-- 说明：全部通过 focusflow.accounting_summary 一组 API
local t = \"数据来自 focusflow.stats API\"
local u = focusflow.today_count()
local v = focusflow[\"config_get\"](x)
--[[ 块注释里写着 focusflow.pomodoro_skip ]]
function init() focusflow.log(\"已初始化\") end
";
    let got = used_names(src);
    assert!(got.contains(&"today_count".to_string()), "{got:?}");
    assert!(
        got.contains(&"config_get".to_string()),
        "方括号写法必须认出来，否则是假绿: {got:?}"
    );
    assert!(got.contains(&"log".to_string()), "{got:?}");
    assert!(
        !got.contains(&"accounting_summary".to_string()),
        "行注释里的名字不该算调用: {got:?}"
    );
    assert!(
        !got.contains(&"stats".to_string()),
        "字符串里的名字不该算调用: {got:?}"
    );
    assert!(
        !got.contains(&"pomodoro_skip".to_string()),
        "块注释里的名字不该算调用: {got:?}"
    );

    // 反向腿：`-- [x]` 这种"注释里带方括号"的行注释不能被当成块注释起点，
    // 否则扫描器会一路吞到下一个 `]]`，把中间真正的调用整个漏掉（假绿）。
    let tricky = "
-- [ ] 待办：以后接 focusflow.scheduler_delete
local w = focusflow.available_years()
";
    let got2 = used_names(tricky);
    assert!(
        got2.contains(&"available_years".to_string()),
        "`-- [ ]` 之后的调用必须还能扫到: {got2:?}"
    );
    assert!(
        !got2.contains(&"scheduler_delete".to_string()),
        "行注释里的名字不该算调用: {got2:?}"
    );
}

/// 存储那一格必须在**插件实际看到的 Lua 环境**里是六个能调的函数（方案 3.5 那张表）。
///
/// 为什么单列一条：`store` 是表里的表，上面那份审计只列 `focusflow` 的顶层键名，
/// 看不见 `focusflow.store` 里缺了哪个动词。缺一个的症状还是本文件顶部那句话 ——
/// `attempt to call a nil value`，界面上就是"插件存不了东西"而宿主日志里一个字都没有。
/// 反向腿同样重要：`focusflow.store` 整格不在时也照样是一条红，不是一句"没测到"。
#[test]
fn the_store_subtable_exposes_every_verb() {
    let _g = round7::guard();
    let app = common::isolated_app_dir("store_wiring");
    let config: &'static FocusFlowConfig = Box::leak(Box::new(
        FocusFlowConfig::load(app.path().join("config.ini")).expect("读取 config.ini 失败"),
    ));
    let lua = Lua::new();
    host::register_host_api(&lua, config, db::Database::init_readonly(), "store_wiring")
        .expect("注册宿主 API 失败");
    let missing: String = lua
        .load(
            r#"
            if type(focusflow.store) ~= "table" then return "整格不是表" end
            local want = { "get", "set", "delete", "list", "clear", "flush" }
            local out = {}
            for _, k in ipairs(want) do
                if type(focusflow.store[k]) ~= "function" then out[#out + 1] = k end
            end
            return table.concat(out, ",")
            "#,
        )
        .eval()
        .expect("这一段 Lua 跑不通");
    assert_eq!(
        missing, "",
        "focusflow.store 这一格里缺动词（或少了整格）: {missing}"
    );
}

// ---- 两个视图（方案 3.4、路线图 P1 第 15 步、验收用例 14）----

/// 建一份真环境，列出 `focusflow` 的键名；`extension` 为真时先过一道收口。
///
/// 交回 `Lua` 本身：用例还要在那个环境里问 `type(focusflow.xxx)`，而键名清单不足以说明
/// 「摘掉之后剩下的那一组还能不能调」。
fn view_names(dir: &Path, owner: &str, extension: bool) -> (Vec<String>, Lua) {
    let config: &'static FocusFlowConfig = Box::leak(Box::new(
        FocusFlowConfig::load(dir.join("config.ini")).expect("读取 config.ini 失败"),
    ));
    let lua = Lua::new();
    let api = host::register_host_api(&lua, config, db::Database::init_readonly(), owner)
        .expect("注册宿主 API 失败");
    // 定时器与事件那一组是**装完表之后**由管理器补装的第二批（`load_plugin` 同一条顺序）：
    // 审计读的是插件实际看到的那张表，漏装这一批就会把六个名字算成"没人注册"。
    host::install_timer_api(&lua, &api, owner).expect("定时器入口装不上");
    if extension {
        host::hide_builtin_only_api(&api).expect("收口失败");
    }
    let names: Vec<String> = lua
        .load(
            "local out = {} for k in pairs(focusflow) do out[#out + 1] = k end table.sort(out) return out",
        )
        .eval()
        .expect("列出 focusflow 表的键名失败");
    (names, lua)
}

/// 量具腿（**期望为绿**）：全量视图里必须看得见内置专属那一组。
///
/// 这一条不测产品，测的是"列举真的列得出东西"。没有它，下面那条收口用例可以靠
/// 「键名列举坏掉 ⇒ 两视图都是空 ⇒ 相等」假绿过去 —— 本文件顶部那条反向断言就是为同样的
/// 形状写的，这里把它拆成单独一条，好让红点说得清是量具坏了还是边界没了。
#[test]
fn the_full_view_still_ships_the_builtin_only_apis() {
    let _g = round7::guard();
    let app = common::isolated_app_dir("view_full");
    let (names, _lua) = view_names(app.path(), "view_full", false);
    for anchor in [
        "pomodoro_state",
        "accounting_add",
        "scheduler_tasks",
        "edge_counts",
    ] {
        assert!(
            names.iter().any(|n| n == anchor),
            "全量视图里连 {anchor} 都没有 ⇒ 是注册或列举环节坏了: {names:?}"
        );
    }
    assert!(
        names.len() >= 40,
        "全量视图只列出 {} 个键名，审计本身失效: {names:?}",
        names.len()
    );
}

/// 扩展视图必须**正好**剩下通用那一组（3.4），而且多出来的一个都不许漏。
///
/// 三条腿各有分工，缺一条就是假绿：
/// · 相等那条防"少摘"（漏摘一组内置 API）与"多摘"（把 `store` 摘没了）两个方向；
/// · `is_builtin_only_api` 那条防"新加一个不带内置前缀、又不该给扩展用的 API"——
///   名字不在通用清单里、又不带前缀，收口按前缀摘不到它，而相等那条会红，
///   这一条负责把红点指到"哪一侧"这个具体问题上；
/// · 最后一条问的是**类型**：`nil` 与"存在但不是函数"（写错成一个值）是两个不同的病。
#[test]
fn the_extension_view_leaves_only_the_general_apis() {
    let _g = round7::guard();
    let app = common::isolated_app_dir("view_extension");
    let (full, _full_lua) = view_names(app.path(), "view_extension", false);
    let (kept, lua) = view_names(app.path(), "view_extension", true);

    let mut want = host::EXTENSION_API_NAMES.to_vec();
    want.sort();
    assert_eq!(
        kept, want,
        "扩展视图的键集不等于通用清单（少了 = 摘过头，多了 = 摘不够）。实得: {kept:?}"
    );

    let leaked: Vec<&String> = kept
        .iter()
        .filter(|n| host::is_builtin_only_api(n))
        .collect();
    assert!(
        leaked.is_empty(),
        "扩展视图里还留着按前缀判为内置专属的 API: {leaked:?}"
    );

    let removed = full
        .iter()
        .filter(|n| !kept.contains(n))
        .collect::<Vec<_>>();
    assert!(
        removed.len() >= 40,
        "收口只摘掉 {} 个键名，内置专属那一组有 45 个量级 —— 八成是前缀规则没覆盖到: {removed:?}",
        removed.len()
    );

    let not_nil: String = lua
        .load(
            r#"
            local want = { "pomodoro_state", "accounting_add", "scheduler_tasks", "edge_counts" }
            local out = {}
            for _, k in ipairs(want) do
                if focusflow[k] ~= nil then out[#out + 1] = k .. "=" .. type(focusflow[k]) end
            end
            if type(focusflow.store) ~= "table" then out[#out + 1] = "store 整格没了" end
            if type(focusflow.stats) ~= "function" then out[#out + 1] = "stats 不是函数" end
            return table.concat(out, ",")
            "#,
        )
        .eval()
        .expect("这一段 Lua 跑不通");
    assert_eq!(
        not_nil, "",
        "扩展视图里这些名字还在（或通用能力被摘坏了）: {not_nil}"
    );
}

/// 一份把"看得见哪些 API"写进视图的插件：文本就是观测值。
fn visibility_lua(body: &str) -> String {
    format!(
        "PLUGIN_NAME = \"{}\"\n\
         function get_view()\n\
           local seen = {{}}\n\
           for _, k in ipairs({{ \"accounting_add\", \"pomodoro_state\", \"scheduler_tasks\", \"edge_counts\" }}) do\n\
             if focusflow[k] ~= nil then seen[#seen + 1] = k end\n\
           end\n\
           local ok = type(focusflow.log) == \"function\"\n\
           return {{ title = \"t\", widgets = {{ {{ type = \"label\", text = (#seen .. \"|\" .. tostring(ok) .. \"|\" .. table.concat(seen, \",\")) }} }} }}\n\
         end\n",
        body
    )
}

/// 读一份视图里那颗 label 的文本。
fn label_of(manager: &focusflow_core::plugins::PluginManager, name: &str) -> String {
    manager
        .get_plugin(name)
        .and_then(|p| p.view.as_ref())
        .and_then(|v| v.widgets.first())
        .map(|w| match w {
            focusflow_core::plugins::Widget::Label(t) => t.clone(),
            other => format!("<不是 label: {other:?}>"),
        })
        .unwrap_or_else(|| "<没有视图>".to_string())
}

/// 经管理器加载时，来源决定看得见哪一组（3.4 的落地那一半）。
///
/// 为什么不只测上一条纯函数：上一条核的是 `hide_builtin_only_api` 自己，而**接线**才是缺陷所在 ——
/// 管理器传错来源、或者只在 `load_all` 那条路上传对，插件页那颗「启用」和热重载就会走另一条。
/// 两条臂各钉一边：用户臂钉"摘了"，内置臂钉"没多摘"（少了后者，把整条链都摘空也能全绿）。
#[test]
fn the_manager_hides_builtin_apis_only_for_user_plugins() {
    let _g = round7::guard();
    let app = common::isolated_app_dir("view_by_source");
    let config: &'static FocusFlowConfig = Box::leak(Box::new(
        FocusFlowConfig::load(app.path().join("config.ini")).expect("读取 config.ini 失败"),
    ));
    for (tag, source, want) in [
        ("user", PluginSource::User, "0|true|"),
        (
            "builtin",
            PluginSource::Builtin,
            "4|true|accounting_add,pomodoro_state,scheduler_tasks,edge_counts",
        ),
    ] {
        let dir = app.path().join(format!("plugins_{tag}"));
        std::fs::create_dir_all(&dir).expect("建临时插件目录");
        let file = dir.join(format!("{tag}_probe.lua"));
        std::fs::write(&file, visibility_lua(&format!("{tag}_probe"))).expect("写夹具插件");
        let mut manager =
            focusflow_core::plugins::PluginManager::new(config, db::Database::init_readonly());
        manager
            .load_plugin(&file, source)
            .unwrap_or_else(|e| panic!("{tag} 臂加载失败: {e}"));
        let got = label_of(&manager, &format!("{tag}_probe"));
        assert_eq!(
            got, want,
            "{tag} 臂（{source:?}）看到的内置专属 API 数量不对"
        );
    }
}

/// 热重载不许把边界洗掉：来源留在条目上，重载一次不该还回内置专属那一组。
///
/// 这一条是上一条管不到的那一半：重载的入参只有键（文件名/展示名），来源必须从**已加载的那份**
/// 带过来。写成"重载时重新扫目录"看着也行，但那一刻目录可能正读不动 —— 读不动就退回全量视图，
/// 症状是"改了个文件，沙箱边界没了"，而屏上什么都正常。
#[test]
fn reloading_a_user_plugin_keeps_the_extension_view() {
    let _g = round7::guard();
    let app = common::isolated_app_dir("view_reload");
    let config: &'static FocusFlowConfig = Box::leak(Box::new(
        FocusFlowConfig::load(app.path().join("config.ini")).expect("读取 config.ini 失败"),
    ));
    let dir = app.path().join("plugins_reload");
    std::fs::create_dir_all(&dir).expect("建临时插件目录");
    let file = dir.join("reload_probe.lua");
    std::fs::write(&file, visibility_lua("reload_probe")).expect("写夹具插件");
    let mut manager =
        focusflow_core::plugins::PluginManager::new(config, db::Database::init_readonly());
    manager
        .load_plugin(&file, PluginSource::User)
        .expect("加载失败");
    assert_eq!(label_of(&manager, "reload_probe"), "0|true|");
    assert!(
        manager.reload_plugin("reload_probe"),
        "夹具插件应当能按文件名重载"
    );
    assert_eq!(
        label_of(&manager, "reload_probe"),
        "0|true|",
        "重载之后用户插件又看见了内置专属 API = 来源没跟着条目走"
    );
}
