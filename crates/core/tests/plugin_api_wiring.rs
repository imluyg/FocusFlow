//! 接线审计：内置插件写的每一个 `focusflow.xxx`，宿主必须真的注册着。
//!
//! 家族同 `desktop/src/lib.rs` 里的窗口 / capabilities / 事件名审计：名字改一处，
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
use mlua::Lua;

/// 串行锁：本文件用 `current_dir()` 当 app_dir，与同 crate 其它动 app_dir 的测试互斥。
mod round7 {
    use std::sync::Mutex;
    pub(super) fn guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 真环境里注册着 API 表 `focusflow`，把它的键名全列出来。
fn registered_names() -> Vec<String> {
    let dir = std::env::current_dir().expect("测试的 cwd 是 crate 根目录");
    paths::set_app_dir(&dir);
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
    let registered = registered_names();
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

    let dir = paths::plugins_dir();
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
