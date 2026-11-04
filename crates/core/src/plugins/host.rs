//! 插件宿主 API：将核心功能注册为 Lua 全局表 `focusflow`。
//!
//! 插件通过 `focusflow.*` 访问：
//! - `focusflow.stats(period)` -> total, keys 表
//! - `focusflow.today_count()` -> 今日计数
//! - `focusflow.now()` -> Unix 秒
//! - `focusflow.log(msg)` -> 日志
//! - `focusflow.daily_counts(days)` -> 每日统计
//! - `focusflow.available_years()` -> 年份列表
//! - `focusflow.config_get("section.key")` -> 配置值
//! - `focusflow.store.get/set/delete/list/clear/flush` -> 这个插件自己的 KV（`state.json`）

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use mlua::{Lua, Table};

use crate::accounting;
use crate::config::FocusFlowConfig;
use crate::db;
use crate::edge_history;
use crate::plugins::plugin_store;
use crate::plugins::resources;
use crate::pomodoro::{self, PomodoroTimer};
use crate::scheduler;

// ---- 番茄钟 / 调度器的进程级单例 ----
//
// 用 `Mutex<Option<Arc<_>>>` 而不是 `OnceLock`：停用插件时要能拆掉后台线程，
// 而 OnceLock 占用后永远换不进去，用户重新启用插件时线程不会再起来。
static POMODORO: Mutex<Option<Arc<PomodoroTimer>>> = Mutex::new(None);
/// 哪一份番茄钟库已经建过 schema。**按路径记**，不是"本进程建过一次就永远算"。
///
/// 数据目录是会换的（每个用例一套临时目录；产品侧的换目录/回滚也一样）。只记一个
/// boolean 会让换过去之后的那份库永远没人建表 —— 症状是本进程里每次 `save_session`
/// 都报 "no such table"，而用户只说"这几周一条都没存下"。
static POMODORO_DB: LazyLock<Mutex<Option<PathBuf>>> = LazyLock::new(|| Mutex::new(None));

/// 调度线程额外记一份「哪些插件在用它」。
///
/// 单例只有一个线程，用户却可能装了多个各自调 `scheduler_*` 的插件。若
/// `scheduler_shutdown` 无条件拆线程，那么**停用任何一个**在 cleanup 里照
/// `host.rs` 文档写了 `scheduler_shutdown()` 的插件，就会把别的插件（含内置
/// 「定时任务」）正在依赖的调度线程一起停掉，而且是无声的：任务列表还在，
/// 到点却再也不启动程序，只有重新打开定时任务面板才会把线程唤回来。
/// 所以改成按持有者计数，最后一个用户走了才真的停。
struct SchedulerSlot {
    runner: Option<Arc<scheduler::Scheduler>>,
    users: HashSet<String>,
}
static SCHEDULER: LazyLock<Mutex<SchedulerSlot>> = LazyLock::new(|| {
    Mutex::new(SchedulerSlot {
        runner: None,
        users: HashSet::new(),
    })
});

fn pomodoro_timer() -> Arc<PomodoroTimer> {
    let mut slot = POMODORO.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(t) = slot.as_ref() {
        // 缓存的是"当前会话"，不是"当前那一份库"：数据目录换了之后，新路径上的
        // 那份库仍然需要自己的 schema（见 `POMODORO_DB` 的注释）。
        ensure_pomodoro_db();
        // 上次 spawn 失败时实例照样缓存着；每次取用都再试一次（否则整个进程里
        // 倒计时永远不动，而面板每 2 秒读一次 `pomodoro_state` 正好当重试点）。
        if let Err(e) = t.ensure_tick_thread() {
            tracing::error!("番茄钟计时线程仍起不来，开始/休息会被拒绝: {e}");
        }
        return Arc::clone(t);
    }
    ensure_pomodoro_db();
    let t = PomodoroTimer::new();
    // config.ini 里的 [pomodoro] 三键必须真的起作用（见 apply_config 的注释）
    t.apply_config(crate::config::instance());
    *slot = Some(Arc::clone(&t));
    t
}

/// 只保证库建好，不启动计时线程：读历史/汇总的接口用。
///
/// **只在成功时才落闩**。原来是 `get_or_init(|| { let _ = init_db(); })`：
/// `open_local()` 已经把文件建出来了，所以只要有一次 `CREATE TABLE` 失败
/// （24 小时在线备份线程正持着库、杀软占着 `-wal`、第二个实例正在退出），
/// 闩就落下了、schema 永不重试 —— 之后整个进程里每次 `save_session` 都是
/// "no such table"，而他不重启的话表现是"这好几周的番茄钟一条都没存下"。
fn ensure_pomodoro_db() {
    let want = pomodoro::db_path();
    {
        let latch = POMODORO_DB.lock().unwrap_or_else(|e| e.into_inner());
        if latch.as_deref() == Some(want.as_path()) {
            return;
        }
    }
    match pomodoro::init_db() {
        Ok(()) => {
            *POMODORO_DB.lock().unwrap_or_else(|e| e.into_inner()) = Some(want);
        }
        Err(e) => tracing::error!("番茄钟库初始化失败，下次调用会重试: {e}"),
    }
}

/// 测试专用：清掉番茄钟的单例与建库闩。
///
/// 用例各用一套临时数据目录，而这两样都是进程级的；不清的话，"上一个用例把单例
/// 建在了它自己的目录里"会让当前用例看到"调用 API 之后库没建出来"——红的是当前
/// 这条的断言，坏的是跨用例共享状态。
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub fn reset_pomodoro_for_test() {
    *POMODORO.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *POMODORO_DB.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// 番茄钟的持有者（插件文件名，与 `claim_scheduler` 同一个口径）。
///
/// 为什么只记"是谁在用"而不计数：一个 `PomodoroTimer` 就是一个「当前番茄会话」的状态，
/// 两个插件同时计时本就无意义（见 `shutdown_pomodoro` 的注释）。但**记名字**是必需的：
/// 出错回收时如果不分主人就拆，调度插件自己超限会把用户正在跑的番茄钟一起打断——
/// 那比"线程留在原地跑"更糟。
static POMODORO_OWNER: LazyLock<Mutex<Option<String>>> = LazyLock::new(|| Mutex::new(None));

pub(crate) fn claim_pomodoro(owner: &str) {
    *POMODORO_OWNER.lock().unwrap_or_else(|e| e.into_inner()) = Some(owner.to_string());
}

/// 出错兜底：只有当这个插件确实是番茄钟的主人时才回收计时线程，交回有没有动手。
pub fn shutdown_pomodoro_if_owned_by(owner: &str) -> bool {
    {
        let mut slot = POMODORO_OWNER.lock().unwrap_or_else(|e| e.into_inner());
        if slot.as_deref() != Some(owner) {
            return false;
        }
        *slot = None;
    }
    shutdown_pomodoro();
    true
}

/// 测试/诊断用：番茄钟此刻登记在谁名下。
///
/// 与调度线程那条同理由：全局槽会被并行用例踩，判据只问"我这个名字还在不在登记上"。
#[cfg(any(test, feature = "test-utils"))]
pub fn pomodoro_owner_for_test() -> Option<String> {
    POMODORO_OWNER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// 回收番茄钟：落盘进行中的阶段并结束计时线程（重新启用插件会再起）。
///
/// 由番茄钟插件在自己的 `cleanup()` 里调用 —— 谁用资源谁负责释放，
/// 核心层不必知道哪个插件在用。
///
/// 刻意**不**按持有者计数（与调度线程不同）：一个 PomodoroTimer 就是一个
/// 「当前番茄会话」的状态，两个插件同时计时本就无意义，所以它是单持有者资源，
/// 拆掉就是拆掉。多个插件共用、且拆错会无声改变行为的是调度线程，见下面 `SchedulerSlot`。
pub fn shutdown_pomodoro() {
    *POMODORO_OWNER.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let timer = POMODORO.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(t) = timer {
        t.shutdown();
    }
}

/// 取用调度线程，并把 `owner`（插件文件名，见 `PluginManager::stem_of`）记为用户之一。
///
/// 走 [`crate::plugins::resources`] 的名册登记：归还那一路要按同一份名册叫完，
/// 所以"谁在用"这一侧也必须出自同一处（只写 `release` 的登记表没有意义）。
pub(crate) fn claim_scheduler(owner: &str) {
    let mut slot = SCHEDULER.lock().unwrap_or_else(|e| e.into_inner());
    if slot.runner.is_none() {
        slot.runner = Some(scheduler::Scheduler::start());
    }
    slot.users.insert(owner.to_string());
}

/// 归还调度线程：只有最后一个用户走了才真的停线程。
pub fn release_scheduler(owner: &str) {
    release_scheduler_checked(owner);
}

/// 同一件事，交回"这一趟有没有把线程拆掉"：名册的 `release` 要说得出这句话，
/// 而"跑过了"与"拆掉了"是两句话（`resources::HostResource::release` 的注释）。
pub(crate) fn release_scheduler_checked(owner: &str) -> bool {
    let mut slot = SCHEDULER.lock().unwrap_or_else(|e| e.into_inner());
    slot.users.remove(owner);
    if slot.users.is_empty() {
        if let Some(s) = slot.runner.take() {
            s.stop();
            return true;
        }
    }
    false
}

/// 测试/诊断用：调度线程当前状态（`None` = 线程已回收，`Some(n)` = 在跑且有 n 个用户）。
#[cfg(any(test, feature = "test-utils"))]
pub fn scheduler_state_for_test() -> Option<usize> {
    let slot = SCHEDULER.lock().unwrap_or_else(|e| e.into_inner());
    slot.runner.as_ref().map(|_| slot.users.len())
}

/// 测试用：某个 owner（插件文件名 stem）此刻还占着调度线程吗。
///
/// 只问 owner、不问"线程还在不在"：调度槽是**进程全局**的，并行跑的别的用例也会各占
/// 一个用户，拿 `users.len()` 或 `runner.is_some()` 当判据会互相踩。这条缺陷的判据本来就
/// 是「我这个插件的登记没了没有」，所以按名字问。
#[cfg(any(test, feature = "test-utils"))]
pub fn scheduler_has_owner_for_test(owner: &str) -> bool {
    SCHEDULER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .users
        .contains(owner)
}

/// 内置模块专属 API 的名字前缀（方案 3.4）。
///
/// 判据用前缀而不是把 45 个名字抄成清单：抄的那一份加一个漏一个是必然的，而漏掉那一条的
/// 症状是「用户装的脚本照样调得动 `accounting_add`」—— 权限模型第一天就失效，且没有一条
/// 用例说得清它坏在哪。前缀这一条规则 + 一条「扩展视图的键集必须等于通用清单」的用例
/// （`plugin_api_wiring.rs`）把这件事变成"新增 API 时必须当场回答它属于哪一侧"。
pub const BUILTIN_ONLY_API_PREFIXES: &[&str] = &["pomodoro_", "accounting_", "scheduler_", "edge_"];

/// 扩展插件（用户自己装的那些）允许看见的通用能力 —— 方案 3.4 那棵树的第一到第五枝。
///
/// 这一份是**审计用的那一面**，不是运行时的白名单：运行时按前缀摘，用例按这份核。
/// 两边不一致（例如新加一个既不带内置前缀、又不该给扩展用的 API）由
/// `the_extension_view_leaves_only_the_general_apis` 逮住，而不是靠读代码的人记得。
pub const EXTENSION_API_NAMES: &[&str] = &[
    "after",
    "app_info",
    "available_years",
    "cancel",
    "config_get",
    "daily_counts",
    "every",
    "invalidate",
    "log",
    "now",
    "off",
    "on",
    "query",
    "stats",
    "store",
    "timers",
    "today_count",
];

/// 这个名字是不是内置模块专属（扩展插件看不见）。
pub fn is_builtin_only_api(name: &str) -> bool {
    BUILTIN_ONLY_API_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// 把 `focusflow` 收口成扩展视图：摘掉 [`BUILTIN_ONLY_API_PREFIXES`] 命中的那些键。
///
/// 摘掉之后插件取到的是 `nil`，与"压根没注册"在 Lua 那一头是同一个观测结果（方案 3.4 要的就是
/// 这个：`focusflow.pomodoro_start()` 在第三方插件里就是 nil，不是靠文档劝阻）。
/// 刻意不做成"注册时按视图分支"：注册这一步现在没有任何副作用（建库与 spawn 都推迟到首次调用，
/// 见 `ensure_pomodoro_db` / `claim_scheduler` 那一族），而分支要动十几处调用点 ——
/// 那些点位以后还会新增，漏改一处的代价是"悄悄少摘一组"，屏上什么都正常。
///
/// 交回摘掉的名字，让调用方与用例能核对**摘了几组**，而不是只信"这个函数跑过了"。
pub fn hide_builtin_only_api(host: &Table) -> mlua::Result<Vec<String>> {
    let doomed: Vec<String> = host
        .clone()
        .pairs::<mlua::Value, mlua::Value>()
        .filter_map(|item| item.ok())
        .filter_map(|(key, _)| match key {
            mlua::Value::String(s) => Some(s.to_string_lossy().to_string()),
            _ => None,
        })
        .filter(|name| is_builtin_only_api(name))
        .collect();
    for name in &doomed {
        host.set(name.as_str(), mlua::Value::Nil)?;
    }
    Ok(doomed)
}

/// 给这一张 `focusflow` 装上权限闸（方案 3.7 的"用必声明 + 授权绑权限哈希"落到注册表上）。
///
/// 做法是给每个**需要权限的**名字换一层 Lua 壳：壳先问 Rust 那头的 `Gate`，被拒就 `error()`
/// 把原因原样抛出去（方案 9.1 定的就是"API 失败时抛错"，而抛出来的这句会走既有的
/// 「插件报了什么」那条路进插件页 —— 不新增一种沉默），放行才 `return f(...)`。
///
/// 为什么壳写在 Lua 而不是 Rust：`...` 的**转发**在 Rust 侧要么逐个已知元数、要么把返回值
/// 重新拼一遍，两种都会改变插件看到的形状（本仓为"多一个返回值"专门写过注释）。
/// 判定逻辑一行都不留在 Lua —— 那里只剩"问一次、转一次"，`permissions.rs` 因此不出现
/// 任何 mlua 类型（方案 3.4 那条判据在权限这一族的照法，`guard_ships_the_decision_not_the_wording`
/// 钉着）。
///
/// 交回装了壳的名字清单：调用方与用例都要能核对"该拦的真的拦了"，而不是只信函数跑完了。
///
/// **前提**：必须在 [`register_host_api`] 之后调用，且 `host` 就是它返回的那张表 ——
/// 那段 Lua 读的是全局 `focusflow`，而全局与这张表是同一个对象（注册那一步末尾设的）。
pub fn install_permission_guard(
    lua: &Lua,
    host: &Table,
    gate: Arc<crate::plugins::permissions::Gate>,
    names: &[&'static str],
    store_verbs: &[&'static str],
) -> mlua::Result<Vec<String>> {
    let check = lua.create_function(move |_lua, (api, verb): (String, Option<String>)| {
        let perm = crate::plugins::permissions::required_for(&api, verb.as_deref());
        let verdict = match perm {
            // 名单里登记了、这里读不出权限 = 两份清单分家了，宁可放行也不许把话说到别处去
            None => (true, String::new()),
            Some(p) => match gate.check(&api, p) {
                Ok(()) => (true, String::new()),
                Err(why) => (false, why),
            },
        };
        if !verdict.0 {
            tracing::warn!("{}", verdict.1);
        }
        Ok(verdict)
    })?;
    // 这个名字只在装壳这一段里活着，装完摘掉：留着就是给插件一个能自己问闸的口子
    host.set("__permission_guard", check)?;

    // 名单由调用方给（Rust 侧那份 `EXTENSION_API_NAMES` 与权限表同源），Lua 这段只认它交给的名字
    let mut name_array = Vec::new();
    for n in names {
        name_array.push((*n).to_string());
    }
    let mut verb_array = Vec::new();
    for v in store_verbs {
        verb_array.push((*v).to_string());
    }
    host.set("__guard_names", name_array)?;
    host.set("__guard_verbs", verb_array)?;
    let wrapped: Vec<String> = lua
        .load(
            r#"
            local guard = focusflow.__permission_guard
            local names = focusflow.__guard_names
            local verbs = focusflow.__guard_verbs
            focusflow.__permission_guard, focusflow.__guard_names, focusflow.__guard_verbs = nil, nil, nil
            local out = {}
            local function wrap(box, key, label)
              local f = box[key]
              if type(f) ~= "function" then return end
              box[key] = function(...)
                local ok, why = guard(label.name, label.verb)
                if not ok then error(why, 0) end
                return f(...)
              end
              out[#out + 1] = (label.verb and (label.name .. "." .. label.verb)) or label.name
            end
            -- on 这一格特殊：权限按**事件名**分（app_change 要 events.app，tick / day_change
            -- 要 timer），所以不能按"整格一个名字"包，得把第一个参数一起交给闸去判。
            local function wrap_event(key, need_verb)
              local f = focusflow[key]
              if type(f) ~= "function" then return end
              focusflow[key] = function(name, ...)
                local ok, why = guard(key, tostring(name))
                if not ok then error(why, 0) end
                return f(name, ...)
              end
              out[#out + 1] = need_verb
            end
            for _, n in ipairs(names) do
              if n == "on" then
                wrap_event("on", "on(event)")
              else
                wrap(focusflow, n, { name = n })
              end
            end
            if type(focusflow.store) == "table" then
              for _, v in ipairs(verbs) do wrap(focusflow.store, v, { name = "store", verb = v }) end
            end
            return out
            "#,
        )
        .eval()?;
    Ok(wrapped)
}

/// 把 `focusflow.every` / `after` / `cancel` / `on` / `off` 装进这张表（方案 3.6）。
///
/// Rust 这一头只管**排程**（`__ff_arm` / `__ff_cancel`，两个内部名字装完就摘掉），
/// 回调函数留在插件自己 Lua 里那一格 `_ff_timers[handle]` —— `mlua::Function` 不是 `Send`，
/// 而排程表要活在进程级那一份登记里（`plugin_timers`），两边只能这么分。
///
/// 类型检查放在 Lua 那一层是有意的：`every(5, "print")` 这种写法要被拒 in place，
/// 并指着**那一个参数**说话；交给 Rust 只能报"第二参数读不出函数"。
pub fn install_timer_api(lua: &Lua, host: &Table, owner: &str) -> mlua::Result<()> {
    use crate::plugins::plugin_timers::{self, Kind};
    let board = plugin_timers::board_for(owner);
    let for_arm = Arc::clone(&board);
    let arm = lua.create_function(move |_lua, (kind, secs): (String, i64)| {
        let kind = match kind.as_str() {
            "every" => Kind::Every,
            "after" => Kind::After,
            "tick" => Kind::Tick,
            "day_change" => Kind::DayChange,
            "app_change" => Kind::AppChange,
            other => {
                return Ok((
                    0u64,
                    Some(format!(
                        "认不得的定时器类型「{other}」：可写 every / after / tick / day_change / app_change"
                    )),
                ))
            }
        };
        if secs < 0 {
            return Ok((0u64, Some(format!("定时器的秒数不许是负数（收到 {secs}）"))));
        }
        let mut guard = for_arm.lock().unwrap_or_else(|e| e.into_inner());
        match guard.arm(kind, secs as u64, std::time::Instant::now()) {
            Ok(handle) => Ok((handle, None)),
            Err(why) => Ok((0u64, Some(why))),
        }
    })?;
    let for_cancel = Arc::clone(&board);
    let cancel = lua.create_function(move |_lua, handle: u64| {
        let mut guard = for_cancel.lock().unwrap_or_else(|e| e.into_inner());
        Ok(guard.cancel(handle))
    })?;
    host.set("__ff_arm", arm)?;
    host.set("__ff_cancel", cancel)?;
    // 计数那一格给插件页与用例读：`focusflow.timers()` 说"我还挂着几格"
    let for_count = Arc::clone(&board);
    let count = lua.create_function(move |_lua, ()| {
        Ok(for_count.lock().unwrap_or_else(|e| e.into_inner()).len())
    })?;
    host.set("timers", count)?;

    lua.load(
        r#"
        local arm = focusflow.__ff_arm
        local cancel = focusflow.__ff_cancel
        _ff_timers = {}
        local function one(kind, secs, fn, verb)
          if type(fn) ~= "function" then
            error(verb .. " 的第二参数要写函数（这里收到的是 " .. type(fn) .. "）", 0)
          end
          local h, why = arm(kind, secs)
          if why then error(verb .. " 没排上：" .. why, 0) end
          _ff_timers[h] = fn
          return h
        end
        focusflow.every = function(sec, fn) return one("every", sec, fn, "focusflow.every(sec, fn)") end
        focusflow.after = function(sec, fn) return one("after", sec, fn, "focusflow.after(sec, fn)") end
        focusflow.on = function(name, fn)
          local kind = (name == "tick" and "tick")
            or (name == "day_change" and "day_change")
            or (name == "app_change" and "app_change")
          if not kind then
            error("focusflow.on 这一版只认 tick / day_change / app_change（收到「" .. tostring(name) .. "」）", 0)
          end
          return one(kind, 0, fn, "focusflow.on(event, fn)")
        end
        local function drop(h)
          if type(h) ~= "number" then
            error("取消要交回排期时拿到的那个句柄（收到 " .. type(h) .. "）", 0)
          end
          local ok = cancel(h)
          _ff_timers[h] = nil
          return ok
        end
        focusflow.cancel = drop
        focusflow.off = drop
        focusflow.__ff_arm, focusflow.__ff_cancel = nil, nil
        "#,
    )
    .set_name("focusflow-timers")
    .exec()?;
    Ok(())
}

/// 注册宿主 API 到 Lua 全局表 `focusflow`，返回该表。
///
/// `owner` 是调用方的插件文件名（stem）：调度线程按持有者计数，
/// 所以每个会用到它的闭包都要知道自己代表哪个插件。
///
/// 这里注册的是**全量视图**（内置模块与 `[plugins] legacy_layout` 回滚分支）。
/// 用户装的脚本插件要在拿到表之后再过一道 [`hide_builtin_only_api`]，由
/// `PluginManager::load_plugin` 按来源决定 —— 身份由位置决定，不由脚本自己写什么决定（3.1）。
pub fn register_host_api(
    lua: &Lua,
    config: &'static FocusFlowConfig,
    database: Arc<db::Database>,
    owner: &str,
) -> mlua::Result<Table> {
    let host = lua.create_table()?;
    let owner = owner.to_string();

    // 统计查询：period = -1 今日, 0 总计, N 天数。
    // 返回 (total, **按次数降序**的 `{name=,count=}` 数组, 键鼠种类数, 这次没读全的原因)。
    //
    // 为什么由 Rust 排序、且返回数组而不是"名字->次数"的表：`get_stats` 里是
    // `HashMap`，SQL 那句 `ORDER BY cnt DESC` 在返回之前就被抹平了，而插件用
    // `pairs()` 遍历取前十 —— 于是「键鼠排行（Top 10）」其实是**任意十个键**
    // （他机器上不同键名有一百多个，空格这种高频键可能整周都不上榜）。
    // 第三个返回值是真实种类数：以前插件把"榜单行数"当种类数，永远显示 10。
    let stats_fn = lua.create_function(move |lua, period: i64| {
        // 读的是带 `unreadable` 的那一份：从前走 `get_stats*` 那两条二元组口子，
        // 它们把"哪一年没答上来"丢掉了（`queries.rs` 的 ⚠ 就写着这件事），于是移动盘没醒、
        // `-wal` 被握着那一轮，面板把**下界**念成完整答案 —— 而界面那一头早有
        // 「这次没读全」的分栏可以说这句话（`Snapshot::gaps`）。
        // 第四个返回值形状照本文件 `accounting_categories` 的 `(清单, 原因)`：
        // 空串 = 读全了；多一个返回值对旧脚本无害（Lua 取不到就多 `nil`）。
        let totals = match period {
            -1 => db::stats_totals_on(chrono::Local::now().date_naive()),
            0 => db::stats_totals(None, None),
            n if crate::db::queries::is_valid_period(n) => db::stats_totals(Some(n), None),
            // 不合法的周期不许折成"一天"：CLI 那条入口早就用 `is_valid_period` 拦过，
            // 而这里从前是 `-1 / 0 / 其余` 三分 —— `-2` 掉进 `Some(-2)`，`clamp_query_days`
            // 把它抬成 1 ⇒ 面板拿到的是一个**看起来干净**的一日数，`gap` 还是空串。
            n => {
                let gap = format!(
                    "focusflow.stats 收到不合法的周期 {n}（可用：-1 今日 / 0 总计 / 正整数天数）：\
                     这次没有查库"
                );
                tracing::warn!("{gap}");
                let rows = lua.create_table()?;
                return Ok((0i64, rows, 0usize, gap));
            }
        };
        let total = totals.total;
        let gap = totals.unreadable.join("；");
        let key_stats = totals.rows;
        let distinct = key_stats.len();
        let mut ordered: Vec<(&String, &i64)> = key_stats.iter().collect();
        ordered.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        let rows = lua.create_table()?;
        for (idx, (name, count)) in ordered.iter().enumerate() {
            let row = lua.create_table()?;
            row.set("name", name.as_str())?;
            row.set("count", **count)?;
            rows.set(idx + 1, row)?;
        }
        Ok((total, rows, distinct, gap))
    })?;
    host.set("stats", stats_fn)?;

    // 今日计数
    let db_today = Arc::clone(&database);
    let today_fn = lua.create_function(move |_, ()| {
        Ok(db::get_today_count(db_today.writer().map(|w| w.as_ref())))
    })?;
    host.set("today_count", today_fn)?;

    // 当前 Unix 秒
    let now_fn = lua.create_function(|_, ()| Ok(chrono::Utc::now().timestamp()))?;
    host.set("now", now_fn)?;

    // 日志
    let log_fn = lua.create_function(|_, msg: String| {
        tracing::info!("[plugin] {msg}");
        Ok(())
    })?;
    host.set("log", log_fn)?;

    // 每日统计（趋势）：第二个返回值是"这次没读全"的原因（空串 = 读全了），
    // 与上面 `stats` 同一形状 —— `get_daily_counts` 那条二元组口子同样丢掉了 `unreadable`。
    //
    // 天数也走 `stats` 那道入口闸：`days.max(1)` 只管下界，`0`（插件作者从 `stats`
    // 那边抄来的「总计」写法）与 `-5` 都会被抬成一天，交回一段**看起来干净**的稠密序列、
    // 原因还是空串。上限在这里也不是"炸不了"就算过关 —— 传 40 000 会被悄悄折成
    // `MAX_QUERY_DAYS`，趋势图上那一段到底覆盖了几天，插件自己说不上来。
    let daily_fn = lua.create_function(|lua, days: i64| {
        if !(1..=crate::db::queries::MAX_QUERY_DAYS).contains(&days) {
            let gap = format!(
                "focusflow.daily_counts 收到不合法的天数 {days}（可用：1..{} 的整数；\
                 本接口没有「今日/总计」这两档，要今日请传 1）：这次没有查库",
                crate::db::queries::MAX_QUERY_DAYS
            );
            tracing::warn!("{gap}");
            let rows = lua.create_table()?;
            return Ok((rows, gap));
        }
        let read = db::daily_counts_totals(days, None);
        let t = lua.create_table()?;
        for (i, (date, count)) in read.series.iter().enumerate() {
            let row = lua.create_table()?;
            row.set("date", date.as_str())?;
            row.set("count", *count)?;
            t.set(i + 1, row)?;
        }
        Ok((t, read.unreadable.join("；")))
    })?;
    host.set("daily_counts", daily_fn)?;

    // 可用年份
    let years_fn = lua.create_function(|_, ()| Ok(db::available_years()))?;
    host.set("available_years", years_fn)?;

    // 配置读取：key = "section.key"
    let cfg_fn = lua.create_function(move |_, key: String| {
        let (section, key) = key.split_once('.').unwrap_or(("", &key));
        let v = config.get(section, key);
        Ok(v)
    })?;
    host.set("config_get", cfg_fn)?;

    // 调试：返回环境信息
    let info_fn = lua.create_function(|_, ()| {
        Ok(format!(
            "FocusFlow {} · {}",
            crate::paths::APP_VERSION,
            crate::paths::app_dir().display()
        ))
    })?;
    host.set("app_info", info_fn)?;

    // ---- 番茄钟 API ----
    // 共享番茄钟实例（进程级单例，见模块顶部），**惰性初始化**：
    // 只有插件真正调用番茄钟 API 时才建库、才起计时线程。早前这里是
    // 「注册即 init_db + spawn」，于是启用任意插件（哪怕与番茄钟无关）
    // 都会把 focusflow_pomodoro.db 建出来，番茄钟插件的停用开关形同虚设。

    let pomo_state_fn = lua.create_function(|lua, ()| {
        let info = pomodoro_timer().get_state_info();
        let t = lua.create_table()?;
        for (k, v) in &info {
            t.set(k.as_str(), *v)?;
        }
        Ok(t)
    })?;
    host.set("pomodoro_state", pomo_state_fn)?;

    let work_owner = owner.clone();
    host.set(
        "pomodoro_start_work",
        lua.create_function(move |_, ()| {
            let t = pomodoro_timer();
            t.start_work().map_err(mlua::Error::RuntimeError)?;
            resources::acquire(resources::POMODORO, &work_owner);
            Ok(())
        })?,
    )?;

    let break_owner = owner.clone();
    host.set(
        "pomodoro_start_break",
        lua.create_function(move |_, ()| {
            let t = pomodoro_timer();
            t.start_break().map_err(mlua::Error::RuntimeError)?;
            resources::acquire(resources::POMODORO, &break_owner);
            Ok(())
        })?,
    )?;

    host.set(
        "pomodoro_toggle_pause",
        lua.create_function(|_, ()| Ok(pomodoro_timer().toggle_pause()))?,
    )?;

    // 「停止」把结局交回面板：`(阶段类型或 nil, 实际秒数, 有没有落库)`。
    // 面板原先无条件说「这一段按完成落库，会算进今日完成」，而没在计时、一秒都没计到、
    // 写库没成这三种情况下它都不成立；休息那半段落了库也不计入今日完成，所以类型也交出。
    let pomo_stop = lua.create_function(|_, ()| {
        let out = match pomodoro_timer().stop() {
            pomodoro::Stopped::Nothing => (None, 0i64, false),
            pomodoro::Stopped::Recorded { rtype, seconds } => (Some(rtype), seconds, true),
            pomodoro::Stopped::SaveFailed { rtype, seconds } => (Some(rtype), seconds, false),
        };
        Ok(out)
    })?;
    host.set("pomodoro_stop", pomo_stop)?;

    // 与 pomodoro_stop 的区别：stop 只结束当前番茄会话、线程继续跑；
    // 这个是插件停用/卸载时在 cleanup 里回收计时线程用。
    host.set(
        "pomodoro_shutdown",
        lua.create_function(|_, ()| {
            shutdown_pomodoro();
            Ok(())
        })?,
    )?;

    // 语义与 pomodoro_stop 不同：skip 是"这段作废"，当前阶段不落库、不计入今日番茄数。
    host.set(
        "pomodoro_skip",
        lua.create_function(|_, ()| {
            pomodoro_timer().skip();
            Ok(())
        })?,
    )?;

    host.set(
        "pomodoro_set_durations",
        lua.create_function(|_, (work, brk): (i64, i64)| {
            pomodoro_timer().set_durations(work, brk);
            Ok(())
        })?,
    )?;

    // 番茄钟历史
    let sessions_fn = lua.create_function(|lua, limit: i64| {
        ensure_pomodoro_db();
        // 第二个返回值是"这一轮没读全"的原因（空串 = 读全了）：从前库打不开也回一张空表，
        // 面板就在「历史记录（最近 20 条）」下面什么都不念 —— 与真的一条没做过长得一样。
        let (sessions, gap) = pomodoro::get_recent_sessions(limit.clamp(1, 100));
        let t = lua.create_table()?;
        for (i, s) in sessions.iter().enumerate() {
            let row = lua.create_table()?;
            row.set("id", s.id)?;
            row.set("type", s.rtype.as_str())?;
            row.set("start", s.start_time.as_str())?;
            row.set("end", s.end_time.as_str())?;
            row.set("actual", s.actual_seconds)?;
            row.set("keys", s.key_count)?;
            t.set(i + 1, row)?;
        }
        Ok((t, gap))
    })?;
    host.set("pomodoro_sessions", sessions_fn)?;

    let summary_fn = lua.create_function(|_, ()| {
        ensure_pomodoro_db();
        // 第四个返回值同上（只取前三个的旧脚本一字不改照样拿到数）
        let (count, keys, secs, gap) = pomodoro::today_summary();
        Ok((count, keys, secs, gap))
    })?;
    host.set("pomodoro_summary", summary_fn)?;

    // 番茄钟按键联动（监听器回调调用）
    host.set(
        "pomodoro_record_key",
        // 第二个形参是这一批的增量：宿主按批投，不逐键投。只传键名的旧写法按一次算。
        lua.create_function(|_, (key, count): (String, Option<i64>)| {
            pomodoro_timer().record_key(&key, count.unwrap_or(1));
            Ok(())
        })?,
    )?;

    // ---- 定时任务 API ----
    // 调度器（进程级单例，见模块顶部），**惰性初始化**：
    // `Scheduler::start` 会 spawn 一个常驻线程并建库，早前在注册时就执行，
    // 于是日程插件被禁用时线程照跑、focusflow_scheduler.db 照样生成。
    // 现在改成首次调用任一日程 API 时才启动，停用插件时经 scheduler_shutdown 回收。
    //
    // 注意连「列出任务」也要先启动：程序重启后 UI 往往只是渲染任务列表，
    // 若读任务不启动调度器，恢复上来的定时任务就永远不会被执行。

    let tasks_owner = owner.clone();
    let tasks_fn = lua.create_function(move |lua, ()| {
        resources::acquire(resources::SCHEDULER, &tasks_owner);
        // 第二个返回值 = 这一轮没读全的原因（空串 = 读全了）。只回清单的话，库打不开时
        // 面板拿到的是一张空表，而"没建过任务"与"读不出来"在界面上长得一模一样。
        let (tasks, gap) = scheduler::all_tasks_read();
        let t = lua.create_table()?;
        for (i, task) in tasks.iter().enumerate() {
            let row = lua.create_table()?;
            row.set("id", task.id)?;
            row.set("name", task.name.as_str())?;
            row.set("target", task.target_path.as_str())?;
            row.set("args", task.args.as_str())?;
            row.set("type", task.schedule_type.as_str())?;
            row.set("time", task.schedule_time.as_str())?;
            row.set("enabled", task.enabled)?;
            row.set("last_run", task.last_run.clone().unwrap_or_default())?;
            row.set(
                "desc",
                scheduler::describe_schedule(&task.schedule_type, &task.schedule_time),
            )?;
            t.set(i + 1, row)?;
        }
        Ok((t, gap))
    })?;
    host.set("scheduler_tasks", tasks_fn)?;

    // 添加：返回 (id, 失败原因)。只回 -1 的话，第三方插件拿到的就是"没成功也没解释"，
    // 而目标被白名单拒 / 调度格式无效 / 库写不进去是三件完全不同的事。
    let add_owner = owner.clone();
    let add_fn = lua.create_function(
        move |_,
              (name, target, args, stype, stime, enabled): (
            String,
            String,
            String,
            String,
            String,
            bool,
        )| {
            resources::acquire(resources::SCHEDULER, &add_owner);
            match scheduler::add_task(&name, &target, &args, &stype, &stime, enabled) {
                Ok(id) => Ok((id, String::new())),
                Err(e) => Ok((-1i64, e.to_string())),
            }
        },
    )?;
    host.set("scheduler_add", add_fn)?;

    // 预检目标 + 参数：返回 (是否可用, 被拒原因)。
    // 让界面能把「为什么点添加没反应」说清楚，而不是只写进日志。
    let check_fn = lua.create_function(|_, (target, args): (String, String)| {
        Ok(match scheduler::check_target(&target, &args) {
            Ok(()) => (true, String::new()),
            Err(e) => (false, e),
        })
    })?;
    host.set("scheduler_check_target", check_fn)?;

    let update_owner = owner.clone();
    let update_fn = lua.create_function(
        move |_,
              (id, name, target, args, stype, stime, enabled): (
            i64,
            String,
            String,
            String,
            String,
            String,
            bool,
        )| {
            resources::acquire(resources::SCHEDULER, &update_owner);
            let r = scheduler::update_task(
                id,
                Some(&name),
                Some(&target),
                Some(&args),
                Some(&stype),
                Some(&stime),
                Some(enabled),
            );
            match r {
                Ok(()) => Ok((true, String::new())),
                Err(e) => Ok((false, e.to_string())),
            }
        },
    )?;
    host.set("scheduler_update", update_fn)?;

    let delete_owner = owner.clone();
    // 与 scheduler_update 同形状：返回 (是否删到, 没删到的原因)。
    // 以前只有一个 bool，"库里读不出来"也会被插件说成"任务不存在"。
    let delete_fn = lua.create_function(move |_, id: i64| {
        resources::acquire(resources::SCHEDULER, &delete_owner);
        let reason = match scheduler::delete_task(id) {
            Ok(true) => String::new(),
            Ok(false) => format!("任务 #{id} 不存在（可能已经被删掉了）"),
            Err(e) => e.to_string(),
        };
        Ok((reason.is_empty(), reason))
    })?;
    host.set("scheduler_delete", delete_fn)?;

    let toggle_owner = owner.clone();
    // 与 scheduler_delete 同形状：返回 (是否改成, 没改成的原因)。
    // 以前这个闭包恒回 Ok(())，"库里没这条"和"库被占住写不进去"都被插件说成"已启用"。
    let toggle_fn = lua.create_function(move |_, (id, enabled): (i64, bool)| {
        resources::acquire(resources::SCHEDULER, &toggle_owner);
        let reason = match scheduler::toggle_task(id, enabled) {
            Ok(true) => String::new(),
            Ok(false) => format!("任务 #{id} 不存在（可能已经被删掉了）"),
            Err(e) => e.to_string(),
        };
        Ok((reason.is_empty(), reason))
    })?;
    host.set("scheduler_toggle", toggle_fn)?;

    // 纯格式校验：不需要库、也不需要调度线程，保持惰性
    let validate_fn = lua.create_function(|_, (stype, stime): (String, String)| {
        let (ok, msg) = scheduler::validate_schedule(&stype, &stime);
        Ok((ok, msg))
    })?;
    host.set("scheduler_validate", validate_fn)?;

    // 插件停用/卸载时在 cleanup 里回收调度线程，否则定时任务会在插件
    // 显示为「已停用」的状态下继续触发。重新启用插件时首次调用任一日程
    // API 会经 claim_scheduler 再把线程起回来。
    // 只回收**本插件**那一份：还有别的插件在用就不停（见 `SchedulerSlot`）。
    let shutdown_owner = owner.clone();
    host.set(
        "scheduler_shutdown",
        lua.create_function(move |_, ()| {
            resources::release(resources::SCHEDULER, &shutdown_owner);
            Ok(())
        })?,
    )?;

    // ---- 记账本 API ----
    // 与番茄钟同理：记账库也改成惰性建库，禁用记账插件时不再碰它的文件。
    //
    // 而且**只在 init_db 成功时才落闩**（同 `ensure_pomodoro_db`）：原来这里是
    // `get_or_init(|| { let _ = accounting::init_db(); })`，闭包永远返回 ()，
    // 所以哪怕一次 `CREATE TABLE` 失败（24 小时在线备份线程正持着库、杀软占着
    // `-wal`、第二个实例正在退出），闩也照样落下、schema 永不重试 —— 之后整个
    // 进程里每次 `accounting_add` 都是 "no such table"，界面却照样回一个 id，
    // 他不重启的话表现是"这几周的账一条都没存下"。
    static ACCOUNTING_DB: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    fn ensure_accounting_db() {
        if ACCOUNTING_DB.get().is_some() {
            return;
        }
        match accounting::init_db() {
            Ok(()) => {
                let _ = ACCOUNTING_DB.set(());
            }
            Err(e) => tracing::error!("记账库初始化失败，下次调用会重试: {e}"),
        }
    }

    let acc_add = lua.create_function(
        |_,
         (rtype, item, store, date, amount, cat, sub, note): (
            String,
            String,
            String,
            String,
            f64,
            String,
            String,
            String,
        )| {
            ensure_accounting_db();
            // 交回 (新行 id 或 -1, 为什么没写进去)：面板以前对 `-1` 只会念「库正被占用或
            // 打不开」，而日期不认、类型不合法也走同一个码 —— 填了 `2026-13-01` 的人
            // 被告知去查数据库。
            Ok(
                match accounting::add_expense_outcome(
                    &rtype,
                    &item,
                    if store.is_empty() {
                        None
                    } else {
                        Some(store.as_str())
                    },
                    &date,
                    amount,
                    if cat.is_empty() {
                        None
                    } else {
                        Some(cat.as_str())
                    },
                    if sub.is_empty() {
                        None
                    } else {
                        Some(sub.as_str())
                    },
                    if note.is_empty() {
                        None
                    } else {
                        Some(note.as_str())
                    },
                ) {
                    Ok(id) => (id, String::new()),
                    Err(why) => (-1i64, why),
                },
            )
        },
    )?;
    host.set("accounting_add", acc_add)?;

    // 分页 + 筛选查询：返回 (records, total)
    let acc_query = lua.create_function(
        |lua,
         (page, page_size, cat, sub, kw, date_from, date_to): (
            i64,
            i64,
            String,
            String,
            String,
            String,
            String,
        )| {
            ensure_accounting_db();
            let (records, total) = accounting::get_expenses_page(
                page,
                page_size,
                if cat.is_empty() {
                    None
                } else {
                    Some(cat.as_str())
                },
                if sub.is_empty() {
                    None
                } else {
                    Some(sub.as_str())
                },
                if kw.is_empty() {
                    None
                } else {
                    Some(kw.as_str())
                },
                if date_from.is_empty() {
                    None
                } else {
                    Some(date_from.as_str())
                },
                if date_to.is_empty() {
                    None
                } else {
                    Some(date_to.as_str())
                },
            );
            let t = lua.create_table()?;
            for (i, e) in records.iter().enumerate() {
                let row = lua.create_table()?;
                row.set("id", e.id)?;
                row.set("type", e.rtype.as_str())?;
                row.set("item", e.item_name.as_str())?;
                row.set("store", e.store.clone().unwrap_or_default())?;
                row.set("date", e.purchase_date.as_str())?;
                row.set("amount", e.amount)?;
                row.set("category", e.category.clone().unwrap_or_default())?;
                row.set("subcategory", e.subcategory.clone().unwrap_or_default())?;
                row.set("note", e.note.clone().unwrap_or_default())?;
                t.set(i + 1, row)?;
            }
            Ok((t, total))
        },
    )?;
    host.set("accounting_query", acc_query)?;

    // 分类列表：`(名字数组, 这次没读全的原因)`，原因空串 = 读全了。
    // 第二个值必须交出去：面板拿这份清单去过「用户筛的分类还在不在」，
    // 读不动的那一轮清单是残缺的，据它下结论会把用户的筛选条件当成悬空清掉。
    let acc_cats = lua.create_function(|lua, ()| {
        ensure_accounting_db();
        let (cats, why) = accounting::get_all_categories();
        let t = lua.create_table()?;
        for (i, c) in cats.iter().enumerate() {
            t.set(i + 1, c.name.as_str())?;
        }
        Ok((t, why))
    })?;
    host.set("accounting_categories", acc_cats)?;

    // 分类管理：添加分类（返回 (id, 失败原因)；成功时 id > 0、原因为空串）
    let acc_cat_add = lua.create_function(|_, (name, ctype): (String, String)| {
        ensure_accounting_db();
        let id = accounting::add_category(&name, &ctype, &[]);
        // 面板读的是 `local ok, msg = focusflow.accounting_category_add(...)`，而这里
        // 原来只回一个 id ⇒ msg 恒为 nil，界面只能说"添加分类失败："四个字。
        let trimmed = name.trim();
        let msg = if id >= 0 {
            String::new()
        } else if trimmed.is_empty() {
            "分类名不能是空的（也别只打空格）".to_string()
        } else {
            // 这一问只是给"为什么没写进去"挑话说法，本身不改变结果：
            // 读不出来的时候**两种说法都不许说** —— 既不能说"已经存在"（那是假的可能让人去改类型），
            // 也不能把原因全推给"写入失败"而不交代这一轮连查重都没做成。
            match accounting::category_type(trimmed) {
                accounting::CategoryType::Value(_) => {
                    format!("分类 [{trimmed}] 已经存在，要改类型请用「修改分类」")
                }
                accounting::CategoryType::Absent => {
                    "记账库没打开或写入失败（详见日志）".to_string()
                }
                accounting::CategoryType::Unreadable(why) => {
                    format!("写入失败，而这一轮说不出它是否已存在：{why}")
                }
            }
        };
        Ok((id, msg))
    })?;
    host.set("accounting_category_add", acc_cat_add)?;

    // 重命名分类（同步历史记录）：(ok, msg)；第三个参数修改类型，空串表示保持原类型
    let acc_cat_rename =
        lua.create_function(|_, (old_name, new_name, ctype): (String, String, String)| {
            ensure_accounting_db();
            let ctype = if ctype.is_empty() {
                None
            } else {
                Some(ctype.as_str())
            };
            Ok(accounting::update_category(&old_name, &new_name, ctype))
        })?;
    host.set("accounting_category_rename", acc_cat_rename)?;

    // 删除分类：(ok, msg)
    let acc_cat_del = lua.create_function(|_, name: String| {
        ensure_accounting_db();
        Ok(accounting::delete_category(&name))
    })?;
    host.set("accounting_category_delete", acc_cat_del)?;

    // 查询分类类型（expense/income/both），返回 `(类型, 没答上的原因)`：
    // 原样只回一个字符串时，「这个分类没设类型」与「这一轮读不出来」在插件里是同一个空串，
    // 分类表那一格于是被印成「双向」，而「修改分类」弹窗会留着上一个分类的类型显示、
    // 用户按保存就把旧值写进去。第二个值空串 = 这一轮答上了。
    //
    // 注意：这里曾同时注册过一个「修改分类类型」的同名写接口，被本块覆盖成
    // 不可达死代码 —— 而 Lua 插件按注释把它当读接口用（返回值是类型字符串，
    // 解包成 `(ok, msg)` 会误判成功）。修改类型现由
    // `accounting_category_rename(old, new, ctype)` 的第三个参数承担。
    //
    // ensure 不可省：category_type 内部走 accounting::open()，而它是
    // Connection::open —— 库文件不存在时会顺手建出一个没有任何表的空库。
    let acc_cat_type = lua.create_function(|_, name: String| {
        ensure_accounting_db();
        Ok(match accounting::category_type(&name) {
            accounting::CategoryType::Value(v) => (v, String::new()),
            accounting::CategoryType::Absent => (String::new(), String::new()),
            accounting::CategoryType::Unreadable(why) => (String::new(), why),
        })
    })?;
    host.set("accounting_category_type", acc_cat_type)?;

    // 添加子分类：(ok, msg)
    let acc_sub_add = lua.create_function(|_, (cat, sub): (String, String)| {
        ensure_accounting_db();
        Ok(accounting::add_subcategory(&cat, &sub))
    })?;
    host.set("accounting_subcategory_add", acc_sub_add)?;

    // 重命名子分类：(ok, msg)
    let acc_sub_rename =
        lua.create_function(|_, (cat, old_sub, new_sub): (String, String, String)| {
            ensure_accounting_db();
            Ok(accounting::update_subcategory(&cat, &old_sub, &new_sub))
        })?;
    host.set("accounting_subcategory_rename", acc_sub_rename)?;

    // 删除子分类：(ok, msg)
    let acc_sub_del = lua.create_function(|_, (cat, sub): (String, String)| {
        ensure_accounting_db();
        Ok(accounting::delete_subcategory(&cat, &sub))
    })?;
    host.set("accounting_subcategory_delete", acc_sub_del)?;

    // 子分类列表：`(名字数组, 这一轮没答上来的原因)`，与上面 accounting_categories 同形状。
    // 第二个值必须交出去：面板的下拉框与「分类管理」右侧那张表只有这一份清单可看，
    // 读不动的那一轮交回的是**残缺**清单，据它印"这个分类没有子分类"就是假读数。
    let acc_subs = lua.create_function(|lua, cat: String| {
        ensure_accounting_db();
        let (subs, why) = accounting::subcategories_readout(&cat);
        let t = lua.create_table()?;
        for (i, s) in subs.iter().enumerate() {
            t.set(i + 1, s.as_str())?;
        }
        Ok((t, why))
    })?;
    host.set("accounting_subcategories", acc_subs)?;

    // 按 id 查询
    let acc_get = lua.create_function(|lua, id: i64| {
        ensure_accounting_db();
        let Some(e) = accounting::get_expense_by_id(id) else {
            return Ok(mlua::Value::Nil);
        };
        let row = lua.create_table()?;
        row.set("id", e.id)?;
        row.set("type", e.rtype.as_str())?;
        row.set("item", e.item_name.as_str())?;
        row.set("store", e.store.clone().unwrap_or_default())?;
        row.set("date", e.purchase_date.as_str())?;
        row.set("amount", e.amount)?;
        row.set("category", e.category.clone().unwrap_or_default())?;
        row.set("subcategory", e.subcategory.clone().unwrap_or_default())?;
        row.set("note", e.note.clone().unwrap_or_default())?;
        Ok(mlua::Value::Table(row))
    })?;
    host.set("accounting_get", acc_get)?;

    // 更新记录（含渠道/子分类/备注）
    let acc_update = lua.create_function(
        |_,
         (id, rtype, item, store, date, amount, cat, sub, note): (
            i64,
            String,
            String,
            String,
            String,
            f64,
            String,
            String,
            String,
        )| {
            ensure_accounting_db();
            let e = accounting::Expense {
                id,
                rtype,
                item_name: item,
                store: if store.is_empty() { None } else { Some(store) },
                purchase_date: date,
                amount,
                category: if cat.is_empty() { None } else { Some(cat) },
                subcategory: if sub.is_empty() { None } else { Some(sub) },
                delivery_date: None,
                record_time: String::new(),
                note: if note.is_empty() { None } else { Some(note) },
            };
            // 与 `accounting_add` 同形状：第二个返回值是「哪条规则拒的」，空串才算改成
            // 了。只回一个 bool 时面板把「日期不认」「类型不合法」「这条已经不在」
            // 「库打不开」念成同一句，而第一种是用户自己改一个字就能好的。
            Ok(match accounting::update_expense_outcome(id, &e) {
                Ok(()) => (true, String::new()),
                Err(why) => (false, why),
            })
        },
    )?;
    host.set("accounting_update", acc_update)?;

    // 月度汇总（含分类明细）：返回 (支出, 收入, 条数, [{category, net}], 没读全的原因)。
    // 最后那个值空串才许面板把这几个数当成这个月的结论 —— 读不出来的账不能印成
    // 「总支出 0.00 / 记录数 0 /（本月暂无分类明细）」。
    let acc_monthly = lua.create_function(|lua, ym: String| {
        ensure_accounting_db();
        let (expense, income, count, cat_stats, gap) = accounting::monthly_summary_detail(&ym);
        let t = lua.create_table()?;
        for (i, (cat, net)) in cat_stats.iter().enumerate() {
            let row = lua.create_table()?;
            row.set("category", cat.as_str())?;
            row.set("net", *net)?;
            t.set(i + 1, row)?;
        }
        Ok((expense, income, count, t, gap))
    })?;
    host.set("accounting_monthly_detail", acc_monthly)?;

    // 分类盈亏：返回 ([{category, invested, earned, count}], 没读全的原因)
    let acc_cat_profit = lua.create_function(|lua, ()| {
        ensure_accounting_db();
        let (data, gap) = accounting::category_profit_loss();
        let t = lua.create_table()?;
        for (i, (cat, inv, earn, cnt)) in data.iter().enumerate() {
            let row = lua.create_table()?;
            row.set("category", cat.as_str())?;
            row.set("invested", *inv)?;
            row.set("earned", *earn)?;
            row.set("count", *cnt)?;
            t.set(i + 1, row)?;
        }
        Ok((t, gap))
    })?;
    host.set("accounting_category_profit", acc_cat_profit)?;

    // 细分盈亏：返回 ([{subcategory, invested, earned, count}], 没读全的原因)
    let acc_sub_profit = lua.create_function(|lua, cat: String| {
        ensure_accounting_db();
        let (data, gap) = accounting::subcategory_profit_loss(&cat);
        let t = lua.create_table()?;
        for (i, (sub, inv, earn, cnt)) in data.iter().enumerate() {
            let row = lua.create_table()?;
            row.set("subcategory", sub.as_str())?;
            row.set("invested", *inv)?;
            row.set("earned", *earn)?;
            row.set("count", *cnt)?;
            t.set(i + 1, row)?;
        }
        Ok((t, gap))
    })?;
    host.set("accounting_subcategory_profit", acc_sub_profit)?;

    // 距今多久：入参 id 数组，返回 ([{id, years, days}], [「哪一条没答上、为什么」])。
    // 第二个值必须有：面板原先在没命中时连结果弹窗都不开，"没填日期"与"库被占住"
    // 与"这条已被删掉"全是同一个"按钮按了没反应"。
    let acc_days_ago = lua.create_function(|lua, ids: Vec<i64>| {
        ensure_accounting_db();
        let (data, misses) = accounting::days_ago(&ids);
        let t = lua.create_table()?;
        for (i, (id, years, days)) in data.iter().enumerate() {
            let row = lua.create_table()?;
            row.set("id", *id)?;
            row.set("years", *years)?;
            row.set("days", *days)?;
            t.set(i + 1, row)?;
        }
        Ok((t, misses))
    })?;
    host.set("accounting_days_ago", acc_days_ago)?;

    let acc_list = lua.create_function(|lua, limit: i64| {
        ensure_accounting_db();
        // 上限放宽到 10000：记账本分页/查询需要全量记录（Lua 侧过滤 + 分页）
        let expenses = accounting::get_all_expenses(limit.clamp(1, 10000));
        let t = lua.create_table()?;
        for (i, e) in expenses.iter().enumerate() {
            let row = lua.create_table()?;
            row.set("id", e.id)?;
            row.set("type", e.rtype.as_str())?;
            row.set("item", e.item_name.as_str())?;
            row.set("date", e.purchase_date.as_str())?;
            row.set("amount", e.amount)?;
            row.set("category", e.category.clone().unwrap_or_default())?;
            row.set("subcategory", e.subcategory.clone().unwrap_or_default())?;
            t.set(i + 1, row)?;
        }
        Ok(t)
    })?;
    host.set("accounting_list", acc_list)?;

    let acc_delete = lua.create_function(|_, id: i64| {
        ensure_accounting_db();
        Ok(accounting::delete_expense(id))
    })?;
    host.set("accounting_delete", acc_delete)?;

    let acc_summary = lua.create_function(|_, ym: String| {
        ensure_accounting_db();
        let (expense, income) = accounting::monthly_summary(&ym);
        Ok((expense, income))
    })?;
    host.set("accounting_summary", acc_summary)?;

    // ---- Edge 历史 API ----
    // 插件跑在主线程（Lua 状态机非 Send），而 Edge 库一次同步读取最坏要等
    // 300ms busy 超时 + 三轮 ≤100MB 整文件复制，会把界面整个冻住。
    // 因此 edge_update_today 只**启动**后台刷新并返回是否启动成功；
    // 结果落在本地缓存库，渲染时用 edge_saved_today / edge_saved_total 取。
    let edge_update = lua.create_function(|_, ()| Ok(edge_history::spawn_update_today()))?;
    host.set("edge_update_today", edge_update)?;

    // 后台刷新状态："idle" | "running" | "ok" | "fail"
    let edge_state = lua.create_function(|_, ()| Ok(edge_history::refresh_state()))?;
    host.set("edge_refresh_state", edge_state)?;

    let edge_counts = lua.create_function(|lua, days: i64| {
        // 读不出本地缓存与"从没刷过"对第三方仍是同一张空表（签名不动），但**第二个返回值**
        // 交出原因：内置挂件据此把峰值画成"读不出来"而不是 "—"。日志照旧要出声，
        // 因为第三方脚本只看第一个值，它拿到空表时不会知道自己问不出结果。
        let (data, why) = match edge_history::get_edge_history_counts(days.clamp(1, 90)) {
            Ok(rows) => (rows, None::<String>),
            Err(e) => {
                tracing::error!("Edge 趋势读不出来: {e}");
                (Vec::new(), Some(e))
            }
        };
        let t = lua.create_table()?;
        for (i, (date, count)) in data.iter().enumerate() {
            let row = lua.create_table()?;
            row.set("date", date.as_str())?;
            row.set("count", *count)?;
            t.set(i + 1, row)?;
        }
        Ok((t, why))
    })?;
    host.set("edge_counts", edge_counts)?;

    // 今日数 / 总数也取自本地缓存库：这两个名字是早先的直接读 Edge 版本，第三方
    // 脚本在用，所以保留签名（永远返回整数、没刷新过就是 0），但绝不碰 Edge 文件。
    // 否则插件一调它们，就把 ea7b2d3 刚修掉的主线程卡顿（300ms busy + 三轮
    // ≤100MB 复制）原样请回来。
    //
    // 「问不出来」也只会交回 0（签名不许动），所以那一支必须同时**出声**：
    // 否则日志里一个字没有，而面板上是一个看着像真话的 0。
    let edge_today = lua.create_function(|_, ()| {
        let saved = match edge_history::get_edge_history_saved_today() {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("Edge 今日数读不出来（这个通道只会交回 0）: {e}");
                None
            }
        };
        Ok(saved.unwrap_or(0))
    })?;
    host.set("edge_today_count", edge_today)?;

    let edge_total = lua.create_function(|_, ()| {
        let saved = match edge_history::get_edge_history_saved_total() {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("Edge 总数读不出来（这个通道只会交回 0）: {e}");
                None
            }
        };
        Ok(saved.unwrap_or(0))
    })?;
    host.set("edge_total_count", edge_total)?;

    // 本地缓存（上次刷新保存的值），插件重启后恢复显示。
    //
    // 这里多交回**第二个值**（说不出时的原因）：Lua 的 `local x = f()` 只取第一个返回值，
    // 第三方脚本一字不改照样拿到数值或 nil；内置挂件读第二个值，才画得出"读不出来"
    // 与"从没刷过"这两种不同结局。
    let edge_saved_today =
        lua.create_function(|_, ()| match edge_history::get_edge_history_saved_today() {
            Ok(v) => Ok((v, None::<String>)),
            Err(e) => Ok((None::<i64>, Some(e))),
        })?;
    host.set("edge_saved_today", edge_saved_today)?;

    let edge_saved_total =
        lua.create_function(|_, ()| match edge_history::get_edge_history_saved_total() {
            Ok(v) => Ok((v, None::<String>)),
            Err(e) => Ok((None::<i64>, Some(e))),
        })?;
    host.set("edge_saved_total", edge_saved_total)?;

    // ---- 通用存储 `focusflow.store.*`（方案 3.5）----
    //
    // 六个函数都交 `(结果, 原因?)`：第二个值是"读不出来 / 写不进去"这句话，
    // 与上面 `edge_saved_today` 一族同一个口径。老脚本只取第一个返回值照样能用，
    // 而**三态分得开**：`get` 交回 `(nil, nil)` 是"这个键没写过"，`(nil, "…")` 是
    // "这份存储读不出来" —— 把后者画成前者，就是本仓修过两轮的那个症状（把"读不出来"
    // 印成"没有"）。
    //
    // 键在这里只是一个字符串参数，进 `plugin_store` 之后只当 JSON 的键名用；
    // 路径那一头永远固定为 `state.json`，插件给什么都拼不出第二格（3.5 的硬约束）。
    let store = lua.create_table()?;
    let owner_for_get = owner.clone();
    store.set(
        "get",
        lua.create_function(move |lua, key: String| {
            let owner = owner_for_get.clone();
            match plugin_store::get(&owner, &key) {
                Ok(Some(v)) => Ok((json_to_lua(lua, &v)?, None::<String>)),
                Ok(None) => Ok((mlua::Value::Nil, None::<String>)),
                Err(e) => Ok((mlua::Value::Nil, Some(e))),
            }
        })?,
    )?;
    let owner_for_set = owner.clone();
    let set_config: &'static FocusFlowConfig = config;
    store.set(
        "set",
        lua.create_function(move |_, (key, value): (String, mlua::Value)| {
            let owner = owner_for_set.clone();
            let limits = plugin_store::Limits::from_config(set_config);
            let json = match lua_to_json(value, 0) {
                Ok(v) => v,
                // 转不动的值（函数、userdata、NaN）算**这次没写**，并把原因说清；
                // 悄悄写成 null 的话，插件下次读回来只会看到一个说不清的假值。
                Err(e) => return Ok((false, Some(e))),
            };
            match plugin_store::set(&owner, &key, json, &limits) {
                Ok(()) => Ok((true, None::<String>)),
                Err(e) => Ok((false, Some(e))),
            }
        })?,
    )?;
    let owner_for_del = owner.clone();
    store.set(
        "delete",
        lua.create_function(move |_, key: String| {
            let owner = owner_for_del.clone();
            match plugin_store::delete(&owner, &key) {
                Ok(removed) => Ok((removed, None::<String>)),
                Err(e) => Ok((false, Some(e))),
            }
        })?,
    )?;
    let owner_for_list = owner.clone();
    store.set(
        "list",
        lua.create_function(move |lua, ()| {
            let owner = owner_for_list.clone();
            match plugin_store::list(&owner) {
                Ok(pairs) => {
                    let t = lua.create_table()?;
                    for (k, v) in pairs {
                        t.set(k.as_str(), json_to_lua(lua, &v)?)?;
                    }
                    Ok((t, None::<String>))
                }
                Err(e) => Ok((lua.create_table()?, Some(e))),
            }
        })?,
    )?;
    let owner_for_clear = owner.clone();
    store.set(
        "clear",
        lua.create_function(move |_, ()| {
            let owner = owner_for_clear.clone();
            match plugin_store::clear(&owner) {
                Ok(n) => Ok((n as i64, None::<String>)),
                Err(e) => Ok((0i64, Some(e))),
            }
        })?,
    )?;
    // 第六个：当场问一次落盘的结局。去抖意味着 `set` 返回时盘上还没动，
    // 少了这一条，"写好了"这句话在插件作者那一头就永远没有证据（配置那一头红过一次）。
    let owner_for_flush = owner.clone();
    store.set(
        "flush",
        lua.create_function(move |_, ()| {
            let owner = owner_for_flush.clone();
            match plugin_store::flush(&owner) {
                Ok(wrote) => Ok((wrote, None::<String>)),
                Err(e) => Ok((false, Some(e))),
            }
        })?,
    )?;
    host.set("store", store)?;

    // 通用的一问（方案 3.4 / 9.1）：`focusflow.query{ from=, to=, group_by=, metric=, limit= }`。
    //
    // 这一格是把 `stats` / `daily_counts` 那几条各自为政的读法收成一个入口，
    // 而它**不写自己的 SQL**：全部走 `db::queries::query_range`，与看板页、活跃分析同一批读手。
    // 交回 `(行数组, 原因)` 两值，与 `stats` 那条同一形状：读不出来的那一半必须能说出来，
    // 不许折成"榜是空的"。
    let query_fn = lua.create_function(|lua, spec: mlua::Value| {
        use crate::db::queries::{self, GroupBy, Metric};
        let empty = lua.create_table()?;
        let mlua::Value::Table(table) = spec else {
            return Ok((
                empty,
                Some(
                    "focusflow.query 要一张参数表，例如 { from=\"2026-10-01\", to=\"2026-11-03\", \
                     group_by=\"day\", metric=\"count\" }"
                        .to_string(),
                ),
            ));
        };
        let text = |key: &str| -> Option<String> {
            match table.get::<mlua::Value>(key) {
                Ok(mlua::Value::String(s)) => Some(s.to_string_lossy()),
                Ok(mlua::Value::Integer(n)) => Some(n.to_string()),
                _ => None,
            }
        };
        let group = match text("group_by").as_deref() {
            None => GroupBy::Day,
            Some(raw) => match GroupBy::parse(raw) {
                Some(g) => g,
                None => {
                    return Ok((
                        empty,
                        Some(format!(
                            "group_by=「{raw}」认不得：可写 day / hour / app / device / key"
                        )),
                    ));
                }
            },
        };
        let metric = match text("metric").as_deref() {
            None => Metric::Count,
            Some(raw) => match Metric::parse(raw) {
                Some(m) => m,
                None => {
                    return Ok((
                        empty,
                        Some(format!("metric=「{raw}」认不得：可写 count / seconds")),
                    ));
                }
            },
        };
        // 区间：`days = N`（含今天往回数 N 天）与 `from`/`to` 二选一，两个都给不许猜哪一个
        let today = chrono::Local::now().date_naive();
        let span = match (text("days"), text("from")) {
            (Some(_), Some(_)) => {
                return Ok((
                    empty,
                    Some(
                        "focusflow.query 的 days 与 from 只能给一个：一个说「含今天往回数几天」，\
                         一个说「从哪一天起」"
                            .to_string(),
                    ),
                ))
            }
            // `days` 已经决定了区间的**两端**（今天往回数 N 天），再给一个 `to` 只有两种
            // 处理法：拒绝，或悄悄丢掉 —— 后者返回的窗口不按作者写的结束日，而 `gap` 还是
            // 空串、图照画，作者没有任何办法看出那个 `to` 没被采纳。
            (Some(_), None) if text("to").is_some() => return Ok((
                empty,
                Some(
                    "focusflow.query 的 days 与 to 只能给一个：days 说的是「含今天往回数几天」，\
                         结束日已经定在今天了 —— 要点名结束日请改用 from/to"
                        .to_string(),
                ),
            )),
            (Some(raw), None) => match raw.parse::<i64>() {
                Ok(n) if (1..=crate::db::queries::MAX_QUERY_DAYS).contains(&n) => {
                    // `n` 已经夹在 1..=上限里；`date_minus_days` 只在减穿历元时才交 None
                    let from = crate::db::queries::date_minus_days(today, n - 1).unwrap_or(today);
                    (from, today)
                }
                Ok(n) => {
                    return Ok((
                        empty,
                        Some(format!(
                            "days={n} 不在 1..={} 天里：这一问不查库（要全部历史请用 stats(0)）",
                            crate::db::queries::MAX_QUERY_DAYS
                        )),
                    ))
                }
                Err(_) => {
                    return Ok((empty, Some(format!("days=「{raw}」读不出天数：要写正整数"))))
                }
            },
            (None, Some(raw)) => match chrono::NaiveDate::parse_from_str(&raw, "%Y-%m-%d") {
                Ok(from) => {
                    let to = match text("to") {
                        Some(t) => match chrono::NaiveDate::parse_from_str(&t, "%Y-%m-%d") {
                            Ok(d) => d,
                            Err(e) => {
                                return Ok((
                                    empty,
                                    Some(format!("to=「{t}」不是 YYYY-MM-DD（{e}）：这一问不查库")),
                                ))
                            }
                        },
                        None => today,
                    };
                    (from, to)
                }
                Err(e) => {
                    return Ok((
                        empty,
                        Some(format!(
                            "from=「{raw}」不是 YYYY-MM-DD（{e}）：这一问不查库"
                        )),
                    ))
                }
            },
            (None, None) => {
                return Ok((
                    empty,
                    Some(
                        "focusflow.query 没给区间：请写 days = N，或 from = \"YYYY-MM-DD\" \
                         （to 可以省，省了就是今天）"
                            .to_string(),
                    ),
                ))
            }
        };
        let (from, to) = span;
        let limit = match text("limit") {
            Some(raw) => match raw.parse::<i64>() {
                Ok(n) if n >= 0 => n as usize,
                Ok(n) => {
                    return Ok((
                        empty,
                        Some(format!("limit={n} 是负数：要 0（不截）或正整数")),
                    ))
                }
                Err(_) => {
                    return Ok((
                        empty,
                        Some(format!("limit=「{raw}」读不出整数：要 0（不截）或正整数")),
                    ))
                }
            },
            None => 0,
        };
        let rows = queries::query_range(group, metric, from, to, limit);
        let out = lua.create_table()?;
        for (idx, (label, value)) in rows.rows.iter().enumerate() {
            let row = lua.create_table()?;
            row.set("label", label.as_str())?;
            row.set("value", *value)?;
            out.set(idx + 1, row)?;
        }
        out.set("total", rows.total)?;
        // 缺读那句原样交回：非空 ⇒ 上面所有数只是**已读部分**
        let gap = if rows.unreadable.is_empty() {
            None
        } else {
            Some(rows.unreadable.join("；"))
        };
        Ok((out, gap))
    })?;
    host.set("query", query_fn)?;

    // `focusflow.invalidate()`（方案 3.9）：插件自己说"我改了，下一次该重算这一格"。
    //
    // 只往件口里放一个文件名，不在这里碰账本：账本（`Cadence`）住在管理器上，而那是
    // GUI 线程专用的。与按键攒批同一形状 —— 投递方永远不需要跨线程碰 VM。
    let invalidate_owner = owner.clone();
    host.set(
        "invalidate",
        lua.create_function(move |_lua, ()| {
            crate::plugins::plugin_refresh::mark_dirty(&invalidate_owner);
            Ok(true)
        })?,
    )?;

    // 注册为全局 `focusflow`
    lua.globals().set("focusflow", host.clone())?;
    Ok(host)
}

/// 一次转换最多往下走几层（与控件解析 `MAX_WIDGET_DEPTH` 同一族防护）。
///
/// 为什么必须有：`local t = {}; t.self = t` 是自引用的表，无界递归会打穿线程栈 ——
/// 栈溢出是 SIGSEGV，不走 panic hook，release 又是 abort + 无控制台，
/// 用户看到的是"插件一存东西程序就没了"。
const MAX_STORE_DEPTH: u32 = 8;

/// 一层里最多转换多少个条目（配额最终兜住总量，这里防的是一次转换把内存吃掉）。
const MAX_STORE_ITEMS: usize = 4000;

/// Lua 值 → JSON 值。转不动的东西交 `Err` 并说清是哪一个，不许折成 `null`。
///
/// 表的两种形状按 Lua 自己的样子分：键正好是 `1..n` 的整数才是数组，其余当对象。
/// 反过来说也成立 —— 读回来时数组是 `{1,2,3}`、对象是 `{a=1}`，插件不需要猜。
fn lua_to_json(value: mlua::Value, depth: u32) -> Result<serde_json::Value, String> {
    use serde_json::Value as J;
    if depth > MAX_STORE_DEPTH {
        return Err(format!(
            "存储的值嵌套超过 {MAX_STORE_DEPTH} 层，本次没有写入：state.json 是给设置项用的，\
             不是记录库（要存很多条请换地方）"
        ));
    }
    match value {
        mlua::Value::Nil => Err("存储不许写 nil：要删掉一个键请调 store.delete(key)".to_string()),
        mlua::Value::Boolean(b) => Ok(J::Bool(b)),
        mlua::Value::Integer(n) => Ok(J::from(n)),
        mlua::Value::Number(f) => {
            if f.is_finite() {
                Ok(J::from(f))
            } else {
                // JSON 里没有 NaN / inf；写成 null 会让下一次读回来变成一个说不清的假值
                Err(format!("存储不许写非有限的数（{f}）"))
            }
        }
        mlua::Value::String(s) => Ok(J::String(s.to_string_lossy())),
        mlua::Value::Table(t) => {
            let mut entries: Vec<(mlua::Value, mlua::Value)> = Vec::new();
            for kv in t.pairs::<mlua::Value, mlua::Value>() {
                let (k, v) = kv.map_err(|e| format!("表读不动：{e}"))?;
                entries.push((k, v));
                if entries.len() > MAX_STORE_ITEMS {
                    return Err(format!("一层里超过 {MAX_STORE_ITEMS} 个条目，本次没有写入"));
                }
            }
            let n = entries.len();
            let is_array = n > 0
                && entries.iter().enumerate().all(|(i, (k, _))| match k {
                    mlua::Value::Integer(x) => *x == (i as i64) + 1,
                    _ => false,
                });
            if is_array {
                let mut out = Vec::with_capacity(n);
                for (_, v) in entries {
                    out.push(lua_to_json(v, depth + 1)?);
                }
                Ok(J::Array(out))
            } else {
                let mut map = serde_json::Map::new();
                for (k, v) in entries {
                    let key = match k {
                        mlua::Value::String(s) => s.to_string_lossy(),
                        mlua::Value::Integer(n) => n.to_string(),
                        mlua::Value::Number(f) => f.to_string(),
                        mlua::Value::Boolean(b) => b.to_string(),
                        _ => return Err("存储的键名只能是字符串或数".to_string()),
                    };
                    map.insert(key, lua_to_json(v, depth + 1)?);
                }
                Ok(J::Object(map))
            }
        }
        mlua::Value::Function(_) => Err("存储不许写函数（函数不会被序列化）".to_string()),
        // 兜住其余一切"落不了盘的东西"：userdata、协程、错误对象，以及 mlua 里那个
        // 说不清的 `Other`。逐条列会漏变体（编译器只在这条 match 上穷尽），
        // 而漏出去的那一支就是"写进去了一个说不清的值"。
        _ => Err("存储只收字符串、数、布尔与表；这一样落不了盘".to_string()),
    }
}

/// JSON 值 → Lua 值。
fn json_to_lua(lua: &Lua, value: &serde_json::Value) -> mlua::Result<mlua::Value> {
    use serde_json::Value as J;
    Ok(match value {
        J::Null => mlua::Value::Nil,
        J::Bool(b) => mlua::Value::Boolean(*b),
        J::Number(n) => match n.as_i64() {
            Some(i) => mlua::Value::Integer(i),
            None => mlua::Value::Number(n.as_f64().unwrap_or(0.0)),
        },
        J::String(s) => mlua::Value::String(lua.create_string(s)?),
        J::Array(items) => {
            let t = lua.create_table()?;
            for (i, item) in items.iter().enumerate() {
                t.set(i + 1, json_to_lua(lua, item)?)?;
            }
            mlua::Value::Table(t)
        }
        J::Object(map) => {
            let t = lua.create_table()?;
            for (k, v) in map {
                t.set(k.as_str(), json_to_lua(lua, v)?)?;
            }
            mlua::Value::Table(t)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 不合法的周期必须在入口被拒绝，不许折成"一天"。
    ///
    /// `queries::is_valid_period` 的注释写着"周期值必须在入口拦截"，而 CLI 之外还有插件
    /// 这一道：`stats` 的 match 从前只有 `-1 / 0 / 其余` 三支，`-2` 掉进 `Some(-2)`，
    /// `clamp_query_days` 把它抬成 1 ⇒ 面板交回的是一份**看起来干净**的一日数，第四个
    /// 返回值（"这次没读全"）还是空串 —— 数字错了却没有一处会响。
    ///
    /// 三条读数都按插件自己看到的样子问（多返回值取第 N 个），不在 Rust 侧复刻一把尺子。
    #[test]
    fn an_invalid_period_is_refused_instead_of_becoming_one_day() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("host_period");
        let lua = Lua::new();
        let database = db::Database::init_readonly();
        register_host_api(&lua, crate::config::instance(), database, "period_test")
            .expect("注册宿主 API");

        let total: i64 = lua
            .load("local t = focusflow.stats(-2) return t")
            .eval()
            .expect("stats(-2) 要能调动");
        assert_eq!(total, 0, "被拒绝的这一问不许交出任何数字");
        let rows: i64 = lua
            .load("local _, r = focusflow.stats(-2) return #r")
            .eval()
            .expect("stats(-2) 的榜单要能读");
        assert_eq!(rows, 0, "被拒绝的这一问不许交出榜单");
        let gap: String = lua
            .load("local _, _, _, g = focusflow.stats(-2) return g")
            .eval()
            .expect("stats(-2) 的原因要能读");
        assert!(
            gap.contains("不合法的周期") && gap.contains("-2"),
            "拒绝要说清是哪一问（空原因＝把错数字念成完整答案），实得: {gap}"
        );

        // 反向腿：合法那三支不许一起被拒（把守卫写成恒真就红在这儿）。
        for period in ["-1", "0", "30"] {
            let gap: String = lua
                .load(format!(
                    "local _, _, _, g = focusflow.stats({period}) return g"
                ))
                .eval()
                .unwrap_or_else(|e| panic!("周期 {period} 调不动: {e}"));
            assert!(
                !gap.contains("不合法的周期"),
                "合法周期 {period} 被当成不合法拒了，实得: {gap}"
            );
        }
    }

    /// `daily_counts` 的天数是同一道闸：不合法的那一支不许被 `max(1)` 抬成"一天"。
    ///
    /// 上一笔只把 `stats` 的三分 match 设了闸，而这条绑定的写法是 `days.max(1)` ——
    /// 它守得住下界（`-5`）与上限（`read_daily_pair` 里还有一道 `clamp_query_days`），
    /// 守不住的是"传进来几天就交回几天"：`0`（插件作者从 `stats` 抄来的「总计」写法）、
    /// `-5`、`40 000` 都会换回一段**看起来干净**的稠密序列，第二个返回值还是空串。
    ///
    /// 三条腿各自盯一件事：被拒的那几问行数必须为 0（光有原因没坏处不算修好）、
    /// 原因要点名收到的是哪个数、合法那支的行数必须**正好等于问的天数**
    /// （把守卫写成恒真就红在最后这条，只查"没说不合法"盯不住它）。
    #[test]
    fn daily_counts_refuses_a_window_it_cannot_honour() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("host_daily_period");
        let lua = Lua::new();
        let database = db::Database::init_readonly();
        register_host_api(&lua, crate::config::instance(), database, "daily_test")
            .expect("注册宿主 API");

        for raw in ["-5", "0", "40000"] {
            let rows: i64 = lua
                .load(format!("local t = focusflow.daily_counts({raw}) return #t"))
                .eval()
                .unwrap_or_else(|e| panic!("daily_counts({raw}) 调不动: {e}"));
            assert_eq!(rows, 0, "daily_counts({raw}) 被拒了这一问不许交出序列");
            let gap: String = lua
                .load(format!(
                    "local _, g = focusflow.daily_counts({raw}) return g"
                ))
                .eval()
                .unwrap_or_else(|e| panic!("daily_counts({raw}) 的原因要能读: {e}"));
            assert!(
                gap.contains("不合法的天数") && gap.contains(raw),
                "拒绝要说清收到的是哪个数（空原因＝把一段折出来的序列念成完整答案），实得: {gap}"
            );
        }

        // 反向腿：合法窗口按问的天数交回，一段不多一段不少。
        for (raw, want) in [("1", 1i64), ("3", 3), ("36500", 36500)] {
            let (rows, gap): (i64, String) = lua
                .load(format!(
                    "local t, g = focusflow.daily_counts({raw}) return #t, g or ''"
                ))
                .eval()
                .unwrap_or_else(|e| panic!("daily_counts({raw}) 调不动: {e}"));
            assert!(
                !gap.contains("不合法的天数"),
                "合法天数 {raw} 被当成不合法拒了，实得: {gap}"
            );
            assert_eq!(rows, want, "问 {want} 天就该交回 {want} 行，实得 {rows} 行");
        }
    }

    /// 注册宿主 API 必须**无副作用**：三个附属库都要等到首次调用才建。
    ///
    /// 早前 register_host_api 在注册时就跑 `init_db`（日程还会 spawn 常驻线程），
    /// 于是只要启用任意一个插件，番茄钟/日程/记账的库就全被建出来 ——
    /// 表现是：插件明明停用了，数据目录里却躺着它的空库。
    #[test]
    fn registering_host_api_creates_no_aux_databases() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("host_lazy");
        // 并行用例可能已经把番茄钟单例建在它自己的目录里了；这一条要问的是
        // "当前这一份目录里该建什么"，不是"上一个用例留下了什么"。
        reset_pomodoro_for_test();

        let lua = Lua::new();
        let database = db::Database::init_readonly();
        register_host_api(&lua, crate::config::instance(), database, "lazy_test")
            .expect("注册宿主 API");

        for (name, path) in [
            ("pomodoro", crate::pomodoro::db_path()),
            ("scheduler", crate::scheduler::db_path()),
            ("accounting", crate::accounting::db_path()),
        ] {
            assert!(
                !path.exists(),
                "注册 API 不应创建 {name} 库: {}",
                path.display()
            );
        }

        // 调用番茄钟 API 之后，只应有番茄钟库被建出来
        lua.load("return focusflow.pomodoro_state()")
            .eval::<mlua::Table>()
            .expect("调用番茄钟 API");
        assert!(
            crate::pomodoro::db_path().exists(),
            "调用番茄钟 API 后应建库"
        );
        assert!(
            !crate::scheduler::db_path().exists(),
            "日程库必须等日程 API 被调用才建"
        );
        assert!(
            !crate::accounting::db_path().exists(),
            "记账库必须等记账 API 被调用才建"
        );
    }

    /// 换了数据目录之后，番茄钟库必须在新路径上重新建 schema。
    ///
    /// 这条盯的是两个进程级缓存的合成症状：单例里还坐着上一个目录的 timer、
    /// 建库闩又记得"建过了" ⇒ 新目录那份库永远没人建表。并行跑用例时正是这个形状
    /// （每条一套临时目录），而症状会红在隔壁那条"调用 API 后应建库"上。
    #[test]
    fn the_pomodoro_db_follows_a_data_dir_switch() {
        let _lock = crate::paths::test_app_dir_lock();
        {
            let _a = crate::paths::test_app_dir("pomo_dir_a");
            let lua = Lua::new();
            let database = db::Database::init_readonly();
            register_host_api(&lua, crate::config::instance(), database, "pomo_dir_a")
                .expect("注册宿主 API");
            lua.load("return focusflow.pomodoro_state()")
                .eval::<mlua::Table>()
                .expect("调用番茄钟 API");
            assert!(crate::pomodoro::db_path().exists(), "第一份目录要先建出来");
        }
        // 上一份的 Drop 把 app_dir 撤到了哨兵；这里换到第二份。
        let _b = crate::paths::test_app_dir("pomo_dir_b");
        let lua = Lua::new();
        let database = db::Database::init_readonly();
        register_host_api(&lua, crate::config::instance(), database, "pomo_dir_b")
            .expect("注册宿主 API");
        lua.load("return focusflow.pomodoro_state()")
            .eval::<mlua::Table>()
            .expect("调用番茄钟 API");
        assert!(
            crate::pomodoro::db_path().exists(),
            "换了数据目录，当前这一份也要有自己的库（单例还活着不等于库还在）"
        );
        reset_pomodoro_for_test();
    }

    /// 番茄钟那两条读数要把"没读全"的话交到 Lua 手上，而随包面板也确实接住了那一位。
    ///
    /// 只改 core 不够：宿主从前交回的是 `(count, keys, secs)` 与一张表，面板拿它们直接印
    /// 「今日完成 0 个」和一张空的「历史记录（最近 20 条）」——库打不开与"今天真没做过"
    /// 在屏上是同一句话。这一条钉两件事：多出来的那一位**是字符串而不是 nil**（面板写的是
    /// `gap ~= ""`，nil 时那个判断永不成立，等于白改），以及随包
    /// `crates/core/plugins/pomodoro_plugin.lua` 真的读了那两位并念"没读全"。
    /// 「库好时它是空串」那条腿住在 `pomodoro.rs` 的用例里——那里的临时目录是真隔离的。
    #[test]
    fn the_pomodoro_host_api_hands_the_missing_reading_to_the_panel() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("host_pomo_gap");
        let lua = Lua::new();
        let database = db::Database::init_readonly();
        register_host_api(&lua, crate::config::instance(), database, "pomo_gap_test")
            .expect("注册宿主 API");

        for (src, which) in [
            (
                "local _, _, _, g = focusflow.pomodoro_summary() return g",
                "今日汇总",
            ),
            (
                "local _, g = focusflow.pomodoro_sessions(5) return g",
                "历史记录",
            ),
        ] {
            let got: mlua::Result<String> = lua.load(src).eval();
            match got {
                // 「库好时它是空串」那条腿住在 core 的用例里（那里 per-test 目录是真的隔离）。
                // 这一头不能那么断：`ensure_pomodoro_db` 的闩是进程级 OnceCell，
                // 同进程里先跑过的那条用例已经把闩落下、它那份临时目录又被删了
                // ⇒ 这里读不到库是**执行顺序**决定的，不是缺陷（顺序依赖的断言会翻脸）。
                // 这一支只钉两件事：那一位是字符串（不是 nil），以及非空时它得是一句原因。
                Ok(g) => assert!(
                    g.is_empty()
                        || g.contains("打不开")
                        || g.contains("读不出来")
                        || g.contains("读不动"),
                    "{which} 交回的那一位要么是空串、要么得是一句能给人看的原因，实得: {g}"
                ),
                Err(e) => panic!(
                    "{which} 多交回的那一位必须是字符串（拿到 nil 说明宿主没把它交出去，\
                     面板那句 ~= 空串 的判断就永远不成立）: {e}"
                ),
            }
        }

        let panel = include_str!("../../plugins/pomodoro_plugin.lua").replace("\r\n", "\n");
        assert!(
            panel.contains("summary_gap") && panel.contains("hist_gap") && panel.contains("没读全"),
            "面板没接住宿主多交回的那两位：读不出来时它照样只念 0 与一张空表"
        );
    }

    /// 番茄钟的同一条链在日程这一头：宿主 `scheduler_tasks` 只回清单 ⇒ 库读不出来时
    /// 插件页拿到一张空表，渲染成「他一条任务都没建过」，而那些定时任务其实再也不触发了。
    /// 这里钉三件事：多交回的那一位**是字符串而不是 nil**（nil 的话面板那句
    /// `gap ~= ""` 永远不成立甚至直接报错）、坏库里它得是一句能给人看的原因、
    /// 以及随包的那份面板真的读了它。
    #[test]
    fn the_scheduler_host_api_hands_the_missing_reading_to_the_panel() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("host_sched_gap");
        let lua = Lua::new();
        let database = db::Database::init_readonly();
        register_host_api(&lua, crate::config::instance(), database, "sched_gap_test")
            .expect("注册宿主 API");
        // 表要建在**本用例自己的**临时目录里：调度线程那份建表闩可能已经被隔壁用例的
        // 目录落下了（进程级），不补这一步，"健康那一支的原因必须是空串"就成了顺序依赖。
        crate::scheduler::init_db().expect("建表");

        let read = "local t, g = focusflow.scheduler_tasks() return t, g, #t";

        // ① 库好好的：原因必须是空串（永远挂一句「没读全」也是缺陷）
        let (_, gap, n): (mlua::Table, String, usize) = lua
            .load(read)
            .eval()
            .expect("scheduler_tasks 该回（表, 字符串, 行数）");
        assert_eq!(n, 0, "全新库里没有任务");
        assert_eq!(gap, "", "这一轮读全了不许有话，实得: {gap}");

        // ② 库整个打不开：清单还是空的，但那一位必须写成一句能给人看的原因。
        //    用的是"同名目录"而不是 DROP TABLE —— 调度线程会自愈建表，删表这条夹具
        //    撞上一次自愈就永远绿（等于没测）。
        {
            let _: mlua::Result<()> = lua.load("focusflow.scheduler_shutdown()").eval();
            let p = crate::scheduler::db_path();
            // 连接池现在也握着这份文件：不清掉本线程那一份，下面 20 次重试照样删不动
            crate::db::connection::clear_aux_cache();
            // 把文件挪开要让调度线程一次（它每 30 秒开一次库，可能正握在那一瞬）：
            // 不让就是只有连跑才红的随机红。
            let mut moved = false;
            for _ in 0..20 {
                match std::fs::remove_file(&p) {
                    Ok(()) => {
                        moved = true;
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        moved = true;
                        break;
                    }
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
                }
            }
            assert!(moved, "库文件一直被人握着，夹具摆不出来");
            std::fs::create_dir(&p).unwrap();
            let got: mlua::Result<(mlua::Table, String, usize)> = lua.load(read).eval();
            let (_, gap, n) = match got {
                Ok(v) => v,
                Err(e) => panic!(
                    "读不出来时多交回的那一位也必须还是字符串（拿到 nil 说明宿主没把它\
                     交出去，面板那句 ~= 空串 的判断就永远不成立）: {e}"
                ),
            };
            assert_eq!(n, 0, "读不出来时清单是空的，这正是症状的源头");
            assert!(
                gap.contains("没读出来") && gap.contains("库打不开"),
                "要说清是读不出来、败在哪一步，实得: {gap}"
            );
            std::fs::remove_dir(&p).ok();
            let _ = crate::scheduler::init_db();
        }

        // 收尾必须把调度线程的登记还回去：`scheduler_tasks` 每次调用都按 owner 登记，
        // 而那张表是进程级的。留着不还，隔壁那批「owner 数」用例（`a_failed_first_load_
        // does_not_leave_an_orphan_scheduler_owner` 等四条）就在我这份目录删掉之后读到
        // 一个多出来的用户 —— 单跑绿、连跑红。
        let _: mlua::Result<()> = lua.load("focusflow.scheduler_shutdown()").eval();

        // ③ 面板接住它：只改宿主不改面板，屏上念的还是那张空表
        let panel = include_str!("../../plugins/scheduler_plugin.lua").replace("\r\n", "\n");
        assert!(
            panel.contains("local tasks, gap = focusflow.scheduler_tasks()")
                && panel.contains("这份清单这次没读全：")
                && panel.contains("_not_in_list(tid, gap)"),
            "日程面板没接住宿主多交回的那一位：读不出来时它照样只念一张空表"
        );
    }

    /// 番茄钟按**主人**归还：不是主人的那个插件出错，不许把别人正在跑的会话拆掉；
    /// 是主人就拆，并且登记要一起清掉（否则下次同名出错时第二次"拆"会假报成动过手）。
    #[test]
    fn the_pomodoro_is_returned_only_to_its_owner() {
        use super::{claim_pomodoro, pomodoro_owner_for_test, shutdown_pomodoro_if_owned_by};
        claim_pomodoro("甲");
        assert_eq!(pomodoro_owner_for_test().as_deref(), Some("甲"));
        assert!(
            !shutdown_pomodoro_if_owned_by("乙"),
            "乙 不是主人，不该动手"
        );
        assert_eq!(
            pomodoro_owner_for_test().as_deref(),
            Some("甲"),
            "乙 出错不能把主人的登记抹掉"
        );
        assert!(shutdown_pomodoro_if_owned_by("甲"), "甲 是自己会话的主人");
        assert_eq!(pomodoro_owner_for_test(), None, "归还后登记要清掉");
        assert!(
            !shutdown_pomodoro_if_owned_by("甲"),
            "没持有归还第二次是无操作，不能假报动过手"
        );
    }

    /// 把插件那一头的环境搭起来（存储用的是**这个** config，不是进程全局那一份）。
    ///
    /// 配额写在传进来的那份 config.ini 里，所以这条路径能测到"超限那一句在 Lua 侧长什么样"，
    /// 而不必依赖哪个用例先初始化了 `config::instance()`。
    fn lua_with_store(cfg_path: &std::path::Path, owner: &str) -> Lua {
        let config: &'static FocusFlowConfig = Box::leak(Box::new(
            FocusFlowConfig::load(cfg_path).expect("临时配置应能载入"),
        ));
        let lua = Lua::new();
        register_host_api(&lua, config, db::Database::init_readonly(), owner)
            .expect("注册宿主 API");
        lua
    }

    /// 值的形状要按插件给的样子回来：数组是数组、对象是对象，嵌套也在。
    ///
    /// 这条盯的是适配器（`lua_to_json` / `json_to_lua`）而不是存储本体：
    /// 折叠成"一切都是对象"或"一切都是数组"在这种写法上都能过一遍，
    /// 症状却是插件下次读回 `#x == 0`（数组没了）—— 上面存储本体的用例摸不到这一层。
    #[test]
    fn store_values_come_back_in_the_shape_they_went_in() {
        let _lock = crate::paths::test_app_dir_lock();
        let tmp = crate::paths::test_app_dir("host_store_shapes");
        std::fs::write(tmp.path().join("config.ini"), "[plugins]\n").unwrap();
        let lua = lua_with_store(&tmp.path().join("config.ini"), "shape_lua");
        let got: String = lua
            .load(
                r#"
                focusflow.store.set("s", "文本")
                focusflow.store.set("n", 42)
                focusflow.store.set("f", 1.5)
                focusflow.store.set("b", true)
                focusflow.store.set("arr", { "一", "二", "三" })
                focusflow.store.set("obj", { k = "v", deep = { 1, 2 } })
                local wrote = focusflow.store.flush()
                local parts = {}
                parts[#parts + 1] = tostring(wrote)
                parts[#parts + 1] = tostring(focusflow.store.get("s"))
                parts[#parts + 1] = tostring(#focusflow.store.get("arr"))
                parts[#parts + 1] = tostring(focusflow.store.get("obj").k)
                parts[#parts + 1] = tostring(focusflow.store.get("obj").deep[2])
                parts[#parts + 1] = tostring(focusflow.store.get("f"))
                return table.concat(parts, "|")
                "#,
            )
            .eval()
            .expect("这一段 Lua 跑不通");
        assert_eq!(
            got, "true|文本|3|v|2|1.5",
            "盘上那份与读回来的形状不对（写出结局|字符串|数组长度|对象取值|嵌套|小数）"
        );
        // 盘上那份是 JSON，键名照原样
        let on_disk =
            std::fs::read_to_string(crate::paths::plugin_state_dir("shape_lua").join("state.json"))
                .expect("state.json 该在插件自己的那一格里");
        assert!(
            on_disk.contains("\"arr\":[\"一\",\"二\",\"三\"]"),
            "{on_disk}"
        );
        assert!(
            on_disk.contains("\"obj\":{\"deep\":[1,2],\"k\":\"v\"}"),
            "{on_disk}"
        );
    }

    /// 被拒的那一次要说清原因，而且什么都没写进去。
    ///
    /// 三样都从 Lua 那一头问：超容量（config 里把上限压到 200 字节）、自引用的表
    /// （不打穿栈）、写 nil（要删就该调 delete）。三条都必须是 `false` + 一句原因，
    /// 而不是把 `true` 交回去 —— 恒 `Ok` 的那一类缺陷就是在这里被抓出来的。
    #[test]
    fn a_refused_write_says_why_and_leaves_the_store_untouched() {
        let _lock = crate::paths::test_app_dir_lock();
        let tmp = crate::paths::test_app_dir("host_store_refused");
        std::fs::write(
            tmp.path().join("config.ini"),
            "[plugins]\nstore_max_bytes = 200\n",
        )
        .unwrap();
        let lua = lua_with_store(&tmp.path().join("config.ini"), "refused_lua");
        let got: String = lua
            .load(
                r#"
                local ok1, why1 = focusflow.store.set("big", string.rep("x", 400))
                local self_t = {}
                self_t.me = self_t
                local ok2, why2 = focusflow.store.set("loop", self_t)
                local ok3, why3 = focusflow.store.set("nada", nil)
                local ok4, why4 = focusflow.store.set("fn", function() end)
                local kept = focusflow.store.set("small", "留下")
                local listed = focusflow.store.list()
                local n = 0
                for _ in pairs(listed) do n = n + 1 end
                return table.concat({
                    tostring(ok1), why1 or "", tostring(ok2), why2 or "",
                    tostring(ok3), why3 or "", tostring(ok4), why4 or "",
                    tostring(kept), tostring(n)
                }, "|")
                "#,
            )
            .eval()
            .expect("这一段 Lua 跑不通");
        let cells: Vec<&str> = got.split('|').collect();
        assert_eq!(cells.len(), 10, "读数形状变了: {got}");
        assert_eq!(cells[0], "false", "超容量的那一次没被挡下");
        assert!(cells[1].contains("容量超限"), "要说清是容量: {}", cells[1]);
        assert_eq!(cells[2], "false", "自引用的表把栈打穿了（该按层数拒绝）");
        assert!(cells[3].contains("嵌套"), "要说清是嵌套: {}", cells[3]);
        assert_eq!(cells[4], "false", "写 nil 被当成了成功");
        assert!(cells[5].contains("delete"), "要指路 delete: {}", cells[5]);
        assert_eq!(cells[6], "false", "写函数被当成了成功");
        assert!(
            cells[7].contains("函数"),
            "函数那一问也要说一句: {}",
            cells[7]
        );
        assert_eq!(cells[8], "true", "被拒的那些不该顺手把合法的也拒了");
        assert_eq!(cells[9], "1", "只有那一条写进去了: {got}");
    }

    /// 「这个键没写过」与「这一份读不出来」在 Lua 侧必须是两个答案。
    ///
    /// 这是本仓修过两轮的那一族（把"读不出来"印成"没有"）：`get` 只回一个 nil 时，
    /// 插件面板只能两句都说成"你从没存过东西"。三态各问一次。
    #[test]
    fn a_missing_key_and_an_unreadable_store_are_two_different_answers() {
        let _lock = crate::paths::test_app_dir_lock();
        let tmp = crate::paths::test_app_dir("host_store_three_states");
        std::fs::write(tmp.path().join("config.ini"), "[plugins]\n").unwrap();
        let cfg = tmp.path().join("config.ini");
        let lua = lua_with_store(&cfg, "three_states");
        let ask = |lua: &Lua, src: &str| -> String {
            lua.load(src).eval::<String>().expect("这一段 Lua 跑不通")
        };
        // 第一态：文件不在 = 读全了而这个键没写过 ⇒ nil + 空原因
        assert_eq!(
            ask(
                &lua,
                "local v, why = focusflow.store.get('从没写过') return tostring(v) .. '|' .. (why or '')"
            ),
            "nil|",
            "没写过的那一问该是 nil 加一句空原因"
        );
        // 第二态：写过 ⇒ 值 + 空原因
        lua.load("focusflow.store.set('有', 1)")
            .eval::<()>()
            .unwrap();
        assert_eq!(
            ask(
                &lua,
                "local v, why = focusflow.store.get('有') return tostring(v) .. '|' .. (why or '')"
            ),
            "1|",
            "写了的那一问该读得到值"
        );
        // 第三态：这一份读不出来 ⇒ nil + 一句原因，而且 list 也要说同一句
        std::fs::write(
            crate::paths::plugin_state_dir("corrupt_lua").join("state.json"),
            "{ 这不是 JSON",
        )
        .unwrap();
        let lua2 = lua_with_store(&cfg, "corrupt_lua");
        let third = ask(
            &lua2,
            "local v, why = focusflow.store.get('k') return tostring(v) .. '|' .. (why or '')",
        );
        assert!(
            third.starts_with("nil|") && third.contains("读不出来"),
            "读不出来必须与「没写过」分开，实得: {third}"
        );
        let listed = ask(
            &lua2,
            "local t, why = focusflow.store.list() return (#t == 0 and '空' or '有货') .. '|' .. (why or '')",
        );
        assert!(
            listed.contains("读不出来"),
            "列全表那一问也得把同一句原因交回来，实得: {listed}"
        );
        // 写的一侧同样不许假装成功
        let wrote = ask(
            &lua2,
            "local ok, why = focusflow.store.set('k', 1) return tostring(ok) .. '|' .. (why or '')",
        );
        assert!(
            wrote.starts_with("false|"),
            "读不出来的那一份不许被悄悄改写: {wrote}"
        );
    }
}
