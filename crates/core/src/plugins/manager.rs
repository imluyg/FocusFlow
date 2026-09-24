//! 插件管理器：扫描、加载、卸载、热重载。
//!
//! 设计约束：mlua 的 `Lua` 不是 Send（含 Rc），因此所有 Lua 操作必须在
//! 同一线程（GUI 主线程）执行。`PluginManager` 不跨线程共享，直接在
//! GUI 线程持有。热重载检测线程只扫描文件并发送"重载请求"到 channel，
//! 由 GUI 线程调用 `poll_reload_requests()` 实际执行重载。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::SystemTime;

use mlua::{HookTriggers, Lua, LuaOptions, StdLib, VmState};

use crate::config::FocusFlowConfig;
use crate::db;
use crate::paths;
use crate::plugins::host;
use crate::plugins::PluginView;

/// 插件信息。
pub struct PluginInfo {
    pub name: String,
    pub desc: String,
    pub version: String,
    pub author: String,
    pub file_path: PathBuf,
    pub file_mtime: Option<SystemTime>,
    pub loaded: bool,
    pub error: Option<String>,
    /// 插件 Lua 环境（仅 GUI 线程访问）
    pub lua: Option<Lua>,
    /// 插件声明的视图（get_view() 结果缓存）
    pub view: Option<PluginView>,
}

/// 插件元数据（从 Lua 脚本读取）。
struct PluginMeta {
    name: String,
    desc: String,
    version: String,
    author: String,
    has_init: bool,
    has_view: bool,
}

/// 目录中发现的插件（含已停用的），供插件管理页展示。
pub struct DiscoveredPlugin {
    /// 展示名（PLUGIN_NAME，读取失败时回退为文件名）
    pub name: String,
    pub desc: String,
    pub version: String,
    pub author: String,
    /// 文件名（不含扩展名）：启用状态的持久化标识，插件改名不影响配置
    pub file: String,
    pub enabled: bool,
    /// 当前是否已加载进内存
    pub loaded: bool,
    /// 加载错误信息（启用但加载失败时展示）
    pub error: Option<String>,
}

/// 插件管理器（GUI 线程专用，不跨线程共享）。
pub struct PluginManager {
    config: &'static FocusFlowConfig,
    db: Arc<db::Database>,
    plugins: HashMap<String, PluginInfo>,
    /// 热重载停止标志
    stop_event: Arc<AtomicBool>,
    /// 热重载检测线程句柄
    hot_reload_thread: Option<std::thread::JoinHandle<()>>,
    /// 加载失败过的插件（按文件名记下最后一次错误）：
    /// 失败的文件不在 `plugins` 里，不另记一笔的话插件页就只能显示"没加载、也没原因"。
    load_errors: HashMap<String, String>,
    /// 重载请求接收端（GUI 线程 poll）
    reload_rx: mpsc::Receiver<String>,
    /// 重载请求发送端（检测线程用）
    reload_tx: mpsc::Sender<String>,
}

impl Drop for PluginManager {
    fn drop(&mut self) {
        // 停止热重载线程，避免退出后仍扫描
        self.stop_event.store(true, Ordering::SeqCst);
        if let Some(handle) = self.hot_reload_thread.take() {
            let _ = handle.join();
        }
        // 卸载所有插件（调用 cleanup，释放 Lua 环境）
        let names: Vec<String> = self.plugins.keys().cloned().collect();
        for name in names {
            self.unload_plugin(&name);
        }
    }
}

/// `[plugins] instruction_limit` 没写或写成 0 时用的默认预算（条 Lua 指令 / 每次调用）。
const DEFAULT_INSTRUCTION_LIMIT: i64 = 10_000_000;

/// 控件表允许的最大嵌套层数（见 `parse_widget`）。
const MAX_WIDGET_DEPTH: u32 = 32;

impl PluginManager {
    pub fn new(config: &'static FocusFlowConfig, db: Arc<db::Database>) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            config,
            db,
            plugins: HashMap::new(),
            load_errors: HashMap::new(),
            stop_event: Arc::new(AtomicBool::new(false)),
            hot_reload_thread: None,
            reload_rx: rx,
            reload_tx: tx,
        }
    }

    /// 创建沙箱化的 Lua 状态：
    /// - 剔除 `io` 库（任意文件读写）；
    /// - 移除 `os` 中可触达系统的高危函数（保留 date/time/clock 供插件使用）；
    /// - 禁用 `package.loadlib`/`cpath`（防加载任意 DLL）；
    /// - 摘掉 base 库的 `dofile`/`loadfile`（base 由 mlua 无条件打开，白名单挡不住）；
    /// - 不放行 `coroutine` 库（见下方 hook 说明）。
    ///
    /// 配合 `apply_lua_limits` 的内存/指令数限制构成完整沙箱。
    fn create_sandboxed_lua() -> mlua::Result<Lua> {
        // 显式白名单，不用 ALL_SAFE：后者含 io 库，且未来 mlua 加入新库时不会默默放行。
        //
        // 这里刻意没有 coroutine：Lua 的 debug hook 是 per-thread 的，
        // `set_hook` 只作用在主状态上，`coroutine.create` 出的线程不继承。
        // 于是 `coroutine.resume(coroutine.create(function() while true do end end))`
        // 能完整绕开指令数上限，而内存配额也拦不住不分配内存的死循环 ——
        // 所有 Lua 都跑在 Tauri 主线程上，这等价于永久冻死整个界面。
        // 与其去给新线程补 hook，不如关掉这个唯一的搬移入口（随附插件无一使用）。
        let libs = StdLib::TABLE
            | StdLib::STRING
            | StdLib::UTF8
            | StdLib::MATH
            | StdLib::PACKAGE
            | StdLib::OS;
        let lua = Lua::new_with(libs, LuaOptions::default())?;
        let globals = lua.globals();
        // base 库由 mlua 无条件打开（白名单里没有 BASE 也挡不住），所以按名字
        // 逐个摘掉能碰到磁盘的入口：随附插件没有一个用得到，留着只是多一条
        // "加载并执行任意路径下的 .lua" 的通道（跨插件代码混用、探测文件是否存在）。
        for name in ["dofile", "loadfile"] {
            globals.set(name, mlua::Value::Nil)?;
        }
        if let Ok(os) = globals.get::<mlua::Table>("os") {
            for name in [
                "execute",
                "exit",
                "getenv",
                "remove",
                "rename",
                "setlocale",
                "tmpname",
            ] {
                os.set(name, mlua::Value::Nil)?;
            }
        }
        if let Ok(pkg) = globals.get::<mlua::Table>("package") {
            pkg.set("loadlib", mlua::Value::Nil)?;
            pkg.set("cpath", "")?;
        }
        Ok(lua)
    }

    /// 为 Lua 状态施加资源限制（防 `while true do end` 冻结主线程）：
    /// - 内存上限：超限触发 `Error::MemoryError`；
    /// - 指令数 hook：每 N 条指令检查一次，超限直接中断执行。
    ///
    /// 配置项：config.ini [plugins] memory_limit_mb（默认 16）/ instruction_limit（默认 1000 万）。
    /// Lua 状态创建后调用一次即可覆盖该状态后续所有执行路径。
    fn apply_lua_limits(&self, lua: &Lua) {
        // 上限要夹：`memory_limit_mb` 来自可以手改的 config.ini，一个天文数字会在
        // `* 1024 * 1024` 处 wrap 成 0（release 不做溢出检查），于是 set_memory_limit(0)
        // ——每个插件立刻 MemoryError，而日志里只有一句 warn，看起来像"插件自己坏了"。
        // 4096MB 之外没有合理用途，超出就是笔误，按笔误处理。
        let mem_mb = self
            .config
            .get_int("plugins", "memory_limit_mb", 16)
            .clamp(1, 4096) as usize;
        if let Err(e) = lua.set_memory_limit(mem_mb * 1024 * 1024) {
            tracing::warn!("Lua 内存限制设置失败: {e}");
        }
        Self::arm_instruction_budget(lua, self.config);
    }

    /// 装（或**重新**装）一次指令钩子：把这一轮的预算重置为 N 条指令。
    ///
    /// 为什么要"每次进入插件代码之前重装"：Lua 的 count 计数器挂在 `lua_State` 上，
    /// 只有 `lua_sethook` 会把它复位，而回调是**无条件 Err** —— 所以"创建状态时装一次"
    /// 的真实语义是「这个状态**累计**跑满 N 条指令，就把当时那次调用掐死」，
    /// 而不是注释所写的「每次调用有 N 条预算」。
    ///
    /// 后果是可复现的：插件详情页每 2 秒渲染一次 `get_view()`，插件页挂着不动，
    /// 攒到 N 的那一刻一个完全正常的插件会被 `mark_plugin_error` 判成
    /// "疑似死循环"而停用 —— 界面停在旧数据、插件页一行红字，不重启（或停用再启用）
    /// 不自愈。本仓有用例把这条钉住：
    /// `crates/core/tests/plugin_lifecycle_test.rs::instruction_budget_is_per_call_not_per_state`
    /// （默认 1000 万条 + 记账那种规模的 get_view，几个小时到几天内必中一次）。
    ///
    /// 重装之后死循环仍然在**同一个调用**里被掐断（预算没变、只是不再跨调用累计）。
    /// `instruction_limit = 0`（有人当"不限制"写）以前被 `.clamp(1)` 变成 1 →
    /// 每条指令都超限、所有插件在第一条就中断，这里一并退回默认值。
    fn arm_instruction_budget(lua: &Lua, cfg: &FocusFlowConfig) {
        let instr = match cfg
            .get_int("plugins", "instruction_limit", DEFAULT_INSTRUCTION_LIMIT)
            .clamp(0, u32::MAX as i64)
        {
            0 => DEFAULT_INSTRUCTION_LIMIT as u32,
            n => n as u32,
        };
        lua.set_hook(
            HookTriggers::new().every_nth_instruction(instr),
            |_lua, _dbg| -> mlua::Result<VmState> {
                Err(mlua::Error::RuntimeError(
                    "插件执行超出指令数上限（疑似死循环），已中断".into(),
                ))
            },
        );
    }

    /// 将插件标记为错误并释放其 Lua 环境（保留条目供插件列表展示错误信息）。
    /// 超限后 Lua 状态不可信，跳过 cleanup 直接丢弃。
    fn mark_plugin_error(&mut self, name: &str, msg: String) {
        if let Some(info) = self.plugins.get_mut(name) {
            info.error = Some(msg.clone());
            info.lua = None;
            info.view = None;
            info.loaded = false;
        }
        tracing::error!("插件已停用: {name}: {msg}");
    }

    /// 插件目录。
    fn plugins_dir(&self) -> PathBuf {
        paths::plugins_dir()
    }

    /// 文件名（不含扩展名）：启用/停用配置的键。
    /// 用文件名而非 PLUGIN_NAME——后者插件作者可随时改，改了配置就失配。
    fn stem_of(path: &Path) -> String {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("plugin")
            .to_string()
    }

    /// 已停用的插件文件名列表（config.ini `[plugins] disabled`，逗号分隔）。
    fn disabled_list(&self) -> Vec<String> {
        self.config
            .get_or("plugins", "disabled", "")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// 该插件文件是否被停用。
    pub fn is_disabled(&self, stem: &str) -> bool {
        self.disabled_list().iter().any(|s| s.as_str() == stem)
    }

    /// 启用/停用插件（按文件名）。
    ///
    /// 停用会卸载已加载实例并调用其 cleanup；启用会立即加载。
    ///
    /// 返回值只表示**这次操作本身有没有做成**。`config.set` 只把改动排进 300ms 去抖的
    /// 持久化队列、永远返回 Ok，所以"写配置失败"那个分支是死代码 —— 它原先返回的是
    /// "操作后的启用状态"，于是**停用成功**时给出 false，调用方一句 `if !ok` 把成功
    /// 报成了「写入插件启用状态失败」（0a85386）。现在只有"启用但加载失败"才给 Err，
    /// 附带插件自己报的原因：那是真失败，而前端一直把 Ok 当成功弹「插件已启用」。
    pub fn set_enabled(&mut self, stem: &str, enabled: bool) -> Result<(), String> {
        // `stem` 会原样拼进 config.ini 的 `[plugins] disabled`（逗号分隔，写盘不做转义），
        // 而它是 IPC 传上来的字符串：名字里带一个 `,` 就能顺手停用别的插件，带换行的
        // 能往配置里注入任意 INI 行。只接受插件目录里真实存在的文件名。
        if stem.contains(',') || stem.contains('\n') || stem.contains('\r') {
            // 光"存在于磁盘"挡不住带逗号的文件名 —— 它自己就能把列表撑成两项，
            // 于是下一次启动会把别的插件一起"点亮"。这种文件直接不认。
            tracing::error!("插件名不能含逗号/换行（会破坏 [plugins] disabled 列表）: {stem}");
            return Err(format!("插件名不合法（含逗号或换行）: {stem}"));
        }
        if !self.discover().iter().any(|p| Self::stem_of(p) == stem) {
            // 以前这里是 `return Ok(())`：命令层把 Ok 当成功，前端弹「插件已启用」，
            // 而配置一个字没改 —— 对着一个不存在的插件报成功。
            tracing::warn!("忽略未知的插件文件名（插件目录里没有它）: {stem}");
            return Err(format!("插件文件不存在: {stem}"));
        }
        let mut list = self.disabled_list();
        if enabled {
            list.retain(|s| s.as_str() != stem);
        } else if !list.iter().any(|s| s.as_str() == stem) {
            list.push(stem.to_string());
        }
        if let Err(e) = self.config.set("plugins", "disabled", &list.join(",")) {
            tracing::error!("写入插件启用状态失败 ({stem}): {e}");
        }
        if enabled {
            // 只有当前未加载时才加载，避免重复初始化
            let already = self
                .plugins
                .values()
                .any(|p| Self::stem_of(&p.file_path) == stem);
            if !already {
                if let Some(path) = self
                    .discover()
                    .into_iter()
                    .find(|p| Self::stem_of(p) == stem)
                {
                    if let Err(e) = self.try_load(&path) {
                        tracing::warn!("启用插件失败 ({stem}): {e}");
                        return Err(format!("插件已记录为启用，但加载失败：{e}"));
                    }
                }
            }
        } else {
            // 先取出名字再卸载：避免在 if-let 条件里持有不可变借用、体内又要 &mut self
            let name = self
                .plugins
                .values()
                .find(|p| Self::stem_of(&p.file_path) == stem)
                .map(|p| p.name.clone());
            if let Some(n) = name {
                self.unload_plugin(&n);
            }
        }
        Ok(())
    }

    /// 列出目录中所有插件（含已停用的），停用的不执行其代码。
    ///
    /// 已加载的插件直接取内存中的信息，避免重复执行其顶层代码；
    /// 未加载的（停用或加载失败）才读取元数据。
    pub fn list_discovered(&self) -> Vec<DiscoveredPlugin> {
        let disabled = self.disabled_list();
        self.discover()
            .into_iter()
            .map(|path| {
                let stem = Self::stem_of(&path);
                let enabled = !disabled.iter().any(|s| s.as_str() == stem);
                if let Some(info) = self
                    .plugins
                    .values()
                    .find(|p| Self::stem_of(&p.file_path) == stem)
                {
                    return DiscoveredPlugin {
                        name: info.name.clone(),
                        desc: info.desc.clone(),
                        version: info.version.clone(),
                        author: info.author.clone(),
                        file: stem,
                        enabled,
                        loaded: info.loaded,
                        error: info.error.clone(),
                    };
                }
                match Self::scan_meta(&path) {
                    Ok((name, desc, version, author)) => DiscoveredPlugin {
                        name,
                        desc,
                        version,
                        author,
                        file: stem.clone(),
                        enabled,
                        loaded: false,
                        // 没加载、但加载失败过：原因得显示出来，不能只剩"未加载"
                        error: self.load_errors.get(&stem).cloned(),
                    },
                    Err(e) => DiscoveredPlugin {
                        name: stem.clone(),
                        desc: String::new(),
                        version: String::new(),
                        author: String::new(),
                        file: stem,
                        enabled,
                        loaded: false,
                        error: Some(e),
                    },
                }
            })
            .collect()
    }

    /// 扫描插件目录，返回 .lua 文件列表。
    pub fn discover(&self) -> Vec<PathBuf> {
        let dir = self.plugins_dir();
        std::fs::create_dir_all(&dir).ok();
        let mut files: Vec<PathBuf> = match std::fs::read_dir(&dir) {
            Ok(entries) => entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().map(|e| e == "lua").unwrap_or(false))
                .collect(),
            // 读不出来 **不等于** 没有插件。以前是 `.unwrap_or_default()`：
            // `plugins` 被建成同名文件、整份放在断线的网盘上、或替换目录的那一刻，
            // 插件页显示「暂无插件」而 5 个插件都还在盘上，日志里一个字都没有；
            // 此时点启用还会得到一句假原因「插件文件不存在」。
            // 桌面侧那份扫描早就区分了这两件事（见 desktop/src/plugins.rs 的读失败处理）。
            Err(e) => {
                tracing::error!(
                    "插件目录读不出来（{}），本次看不到任何插件 —— 这不是「没有插件」: {e}",
                    dir.display()
                );
                Vec::new()
            }
        };
        files.sort();
        files
    }

    /// 只做文本扫描读元数据，**不执行插件代码**：`(name, desc, version, author)`。
    ///
    /// `list_discovered` 原先回退到 `read_meta`，而 `read_meta` 是 `exec()` 整段脚本再取
    /// 全局变量 —— 于是「已停用」的插件每开一次插件管理页、每来一次 `plugins-reloaded`
    /// 都要在主线程上把顶层代码跑一遍（只有指令数上限兜着），与 `mod.rs` 里
    /// 「停用的不执行其代码」这句承诺正好相反，也白跑了加载失败那批的顶层代码。
    /// 这 4 个字段本来就是给人看的字符串字面量，扫一行足够；真正要执行顶层代码的
    /// 加载路径照旧用 `read_meta`。
    ///
    /// **判据必须与 `read_meta` 的实际口径一致**（扫出来的名字 = 加载后的名字），
    /// 否则插件管理页会拿一个假名字当键用：那页的「打开」按钮传的就是列表里的
    /// `p.name`（见 `desktop/ui/js/plugins.js` 的 `open-plugin`），名字对不上
    /// 就是点一次报一次「插件未提供视图」。所以要挡掉两类假命中：
    /// `--[[ ]]` 块注释里被注释掉的旧值、函数体里缩进的同名局部赋值。
    /// 规则写在 [`top_level_string_assign`] 与 [`strip_lua_non_code`] 里：
    /// 先把注释和长字符串整段抹成空格，再只认**从第 0 列起**正好是
    /// `KEY = "字面量"` 的行（`local KEY = …` 也不算 —— 那根本不会成为全局变量）。
    fn scan_meta(path: &Path) -> Result<(String, String, String, String), String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        Ok(Self::scan_meta_source(&text, path))
    }

    /// `scan_meta` 的全部判定（读文件之外的那部分，单独拿出来才测得动）。
    fn scan_meta_source(text: &str, path: &Path) -> (String, String, String, String) {
        let code = strip_lua_non_code(text);
        let field = |key: &str| -> Option<String> {
            code.lines()
                .find_map(|line| top_level_string_assign(line, key))
        };
        (
            field("PLUGIN_NAME").unwrap_or_else(|| Self::stem_of(path)),
            field("PLUGIN_DESC").unwrap_or_default(),
            field("PLUGIN_VERSION").unwrap_or_else(|| "1.0".to_string()),
            field("PLUGIN_AUTHOR").unwrap_or_default(),
        )
    }

    /// 从 Lua 脚本读取元数据（不执行 init）。
    fn read_meta(&self, path: &Path) -> Result<PluginMeta, String> {
        let lua = Self::create_sandboxed_lua().map_err(|e| e.to_string())?;
        self.apply_lua_limits(&lua);
        // 只注册空的 focusflow 占位表，避免扫描阶段触发宿主单例副作用；
        // 真正的宿主 API 在 load_plugin 时才注册。
        {
            let placeholder = lua.create_table().map_err(|e| e.to_string())?;
            lua.globals()
                .set("focusflow", placeholder)
                .map_err(|e| e.to_string())?;
        }
        let script = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        lua.load(&script)
            .set_name(
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("plugin"),
            )
            .exec()
            .map_err(|e| format!("Lua 执行失败: {e}"))?;

        let globals = lua.globals();
        let gstr = |key: &str, def: &str| -> String {
            globals
                .get::<mlua::String>(key)
                .ok()
                .map(|s| s.to_string_lossy())
                .unwrap_or_else(|| def.to_string())
        };
        let name = gstr(
            "PLUGIN_NAME",
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("plugin"),
        );
        let desc = gstr("PLUGIN_DESC", "");
        let version = gstr("PLUGIN_VERSION", "1.0");
        let author = gstr("PLUGIN_AUTHOR", "");
        let has_fn = |key: &str| -> bool { globals.get::<mlua::Function>(key).is_ok() };

        Ok(PluginMeta {
            name,
            desc,
            version,
            author,
            has_init: has_fn("init"),
            has_view: has_fn("get_view"),
        })
    }

    /// 加载单个插件文件。
    pub fn load_plugin(&mut self, path: &Path) -> Result<String, String> {
        let meta = self.read_meta(path)?;
        // 展示名（PLUGIN_NAME）是插件表的键。两个文件声明同一个名字时，原先是
        // 后来者静默覆盖前者：被顶掉的那个既不再接收键事件，也再没有 unload
        // 入口（cleanup 永不执行），而它的 .lua 还在盘上。拿复制模板改插件的人
        // 十有八九是忘了改 PLUGIN_NAME，所以这里明确报错、保留已加载的那个。
        //
        // **判据是源文件，不是展示名**：热重载现在"先加载新版本、成功才退休旧条目"
        // （见 `reload_plugin`），而加载那一下旧条目还在表里挂着 —— 按展示名判重的话，
        // 每个插件的自我重载都会被自己判成撞名，于是"坏不了的插件反而永远重载不动"
        // （上一场就是卡在这一步停手的）。同一个源文件的重复加载不算撞名：
        // 下面 insert 会顶掉它自己那一条，改了名的旧名字另外摘掉。
        let stem = Self::stem_of(path);
        if let Some(clash) = self
            .plugins
            .values()
            .find(|p| p.name == meta.name && !is_same_source(&p.file_path, path))
        {
            return Err(format!(
                "PLUGIN_NAME「{}」与已加载插件（文件 {}）重名，请改一个再启用",
                meta.name,
                Self::stem_of(&clash.file_path)
            ));
        }
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();

        let lua = Self::create_sandboxed_lua().map_err(|e| e.to_string())?;
        self.apply_lua_limits(&lua);
        // 宿主按文件名记账调度线程的使用者，所以这里传 stem 而不是展示名：
        // 两个插件的 PLUGIN_NAME 相同是常见的手误，展示名当键会串味。
        host::register_host_api(&lua, self.config, Arc::clone(&self.db), &stem)
            .map_err(|e| format!("宿主 API 注册失败: {e}"))?;
        let script = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        lua.load(&script)
            .set_name(&meta.name)
            .exec()
            .map_err(|e| format!("Lua 执行失败: {e}"))?;

        if meta.has_init {
            let init: mlua::Function = lua.globals().get("init").map_err(|e| e.to_string())?;
            let _: () = init.call(()).map_err(|e| format!("init() 失败: {e}"))?;
        }

        let view = if meta.has_view {
            Self::read_view(&lua).ok()
        } else {
            None
        };

        let info = PluginInfo {
            name: meta.name,
            desc: meta.desc,
            version: meta.version,
            author: meta.author,
            file_path: path.to_path_buf(),
            file_mtime: mtime,
            loaded: true,
            error: None,
            lua: Some(lua),
            view,
        };

        // 新实例装好了，才让同源的旧版本退休。同名的那条由 insert 顶掉；
        // 但这一版把 PLUGIN_NAME 改了名的话顶不掉，会留下"同一个文件、两份实例"的
        // 僵尸 —— 旧那条既不再收键事件、又没有 unload 入口（cleanup 永不执行），
        // 记在 stem 上的调度线程使用者也就再没人回收。这里刻意**不**跑 cleanup、
        // 也不动 owner 记账：重载语义见 `reload_plugin`。
        let renamed = self
            .plugins
            .iter()
            .find(|(name, p)| {
                name.as_str() != info.name.as_str() && is_same_source(&p.file_path, path)
            })
            .map(|(name, _)| name.clone());
        if let Some(old) = renamed {
            tracing::info!("插件改名，旧条目退休: {old} -> {}", info.name);
            self.plugins.remove(&old);
        }

        self.plugins.insert(info.name.clone(), info);
        tracing::info!("插件加载成功: {}", self.plugins.len());
        // 返回刚加载的插件名
        let name = self
            .plugins
            .values()
            .find(|p| p.file_path == path)
            .map(|p| p.name.clone())
            .ok_or_else(|| "加载后未找到插件".to_string())?;
        Ok(name)
    }

    /// `load_plugin` 的记账版：成功就清掉这个文件之前的失败原因，失败就留档，
    /// 好让插件页显示"为什么没加载"而不是只有一行未加载。
    fn try_load(&mut self, path: &Path) -> Result<String, String> {
        let stem = Self::stem_of(path);
        match self.load_plugin(path) {
            Ok(name) => {
                self.load_errors.remove(&stem);
                Ok(name)
            }
            Err(e) => {
                // 失败的那次尝试可能已经在调度线程上把这个文件名登记成使用者了 ——
                // `init()` 跑到一半才报错最常见（`focusflow.scheduler_tasks()` 是第一个
                // 会 claim 的宿主 API，插件往往先列任务再在下一行炸）。表里已经没有这个
                // 源文件的条目时，那份登记就成了无主孤儿：唯一的释放入口是插件 Lua 里的
                // `scheduler_shutdown()`，而那个状态已经跟着失败的回滚一起没了 →
                // 本进程的调度线程再也回收不掉（见 `reload_plugin` 的说明）。
                // **有活着的旧实例时一律不销**：那份账属于旧实例，重载失败要"什么都没
                // 发生"，销它等于把定时任务的线程从还在用它的插件脚下拆走。
                if !self
                    .plugins
                    .values()
                    .any(|p| is_same_source(&p.file_path, path))
                {
                    host::release_scheduler(&stem);
                }
                self.load_errors.insert(stem, e.clone());
                Err(e)
            }
        }
    }

    /// 从插件 Lua 环境读取 get_view() 返回值（声明式 UI 表）。
    fn read_view(lua: &Lua) -> mlua::Result<PluginView> {
        let get_view: mlua::Function = lua.globals().get("get_view")?;
        let view_val: mlua::Value = get_view.call(())?;
        let mut view = PluginView::default();
        if let mlua::Value::Table(t) = view_val {
            if let Ok(title) = t.get::<String>("title") {
                view.title = title;
            }
            if let Ok(widgets) = t.get::<mlua::Table>("widgets") {
                for (_, w) in widgets.pairs::<mlua::Value, mlua::Table>().flatten() {
                    match parse_widget(&w, 0) {
                        Ok(widget) => view.widgets.push(widget),
                        // 认不出来的控件以前是**静默丢掉**的：`type` 打错一个字母、
                        // headers 里混进一个数字，界面上就是少一块，而插件行不报红、
                        // 日志里一个字都没有 —— 这一类"看不见"正是上面那些契约问题
                        // 能藏很久的原因。仍然跳过（不能让一个坏控件毁掉整页），
                        // 但要留下为什么。
                        Err(e) => tracing::warn!("跳过无法解析的控件: {e}"),
                    }
                }
            }
        }
        Ok(view)
    }

    /// 卸载插件。
    pub fn unload_plugin(&mut self, name: &str) -> bool {
        let cfg = self.config;
        if let Some(mut info) = self.plugins.remove(name) {
            // 卸载 = 用户不要它了（停用/删除/重启前的一次回收），旧的加载失败原因
            // 不该继续挂在插件页上。
            self.load_errors.remove(&Self::stem_of(&info.file_path));
            if let Some(lua) = &mut info.lua {
                if let Ok(cleanup) = lua.globals().get::<mlua::Function>("cleanup") {
                    Self::arm_instruction_budget(lua, cfg);
                    let _: mlua::Result<()> = cleanup.call(());
                }
            }
            info.loaded = false;
            info.lua = None;
            tracing::info!("插件已卸载: {name}");
            true
        } else {
            false
        }
    }

    /// 按文件名（stem）卸载插件，供热重载处理「插件文件被删除」用。
    /// 插件表以展示名（PLUGIN_NAME）为键，所以要先反查。返回是否真卸载了一个。
    pub fn unload_by_stem(&mut self, stem: &str) -> bool {
        let name = self
            .plugins
            .values()
            .find(|p| Self::stem_of(&p.file_path) == stem)
            .map(|p| p.name.clone());
        match name {
            Some(n) => self.unload_plugin(&n),
            None => false,
        }
    }

    /// 重新加载插件（按文件名或插件名匹配）。
    ///
    /// **先加载新版本，成功之后才让旧条目退休**（退休那一步在 `load_plugin` 里，
    /// 因为改名时新旧展示名不同、光靠 insert 顶不掉）。旧实现是先 `plugins.remove(n)`
    /// 再 `try_load`，那是"把两步中可能失败的那步放在后面"：编辑器/网盘留下一个
    /// 半截的、语法错误的文件时（这类工具最爱这么写），旧实例已经离开表、而它的
    /// `cleanup()` 按设计没跑，新加载又失败 → 插件从 map 里消失、正打开的详情页
    /// 变成「插件未提供视图」，而它记在 `claim_scheduler` 上的 owner 成了无主孤儿
    /// （`host.rs` 里那份 owner 表只有 Lua 侧 `scheduler_shutdown()` 一个释放入口，
    /// 而那个 Lua 状态已经跟着旧条目没了）→ 本进程再也回收不掉，
    /// 除了重启没有别的办法把定时任务线程还回来。
    /// 现在失败路径**一个字都不动**旧实例：重载失败 = 什么都没发生（插件页多一条
    /// 加载失败原因，见 `try_load`），下一次存盘成功再换上来。
    pub fn reload_plugin(&mut self, key: &str) -> bool {
        // 先按插件名匹配，再按文件名匹配
        let path = self
            .plugins
            .get(key)
            .map(|p| p.file_path.clone())
            .or_else(|| {
                self.plugins
                    .values()
                    .find(|p| p.file_path.file_stem().and_then(|s| s.to_str()) == Some(key))
                    .map(|p| p.file_path.clone())
            });
        match path {
            Some(p) => {
                // 重载刻意**不**跑 cleanup()：这条路径是「存了个文件」触发的
                // （mtime 变化、构建脚本换目录、编辑器先截断再写入），而番茄插件的
                // cleanup 会 `pomodoro_stop()` —— 那段 `save_current()` 只要实际计时
                // ≥1 秒就给 `work_finished += 1`。于是按一次 Ctrl+S 就把用户跑到一半的
                // 番茄钟判成「完成一个」，还是静默的。调度线程同理，拆了要靠重新渲染
                // 定时任务面板才起得回来。
                // 所以这里只把条目换掉：旧 Lua 状态随旧条目一起释放，新的那份经宿主
                // API 复用同一批进程级单例（沙箱里没有 io/coroutine，插件本身持不住
                // 别的资源），owner 记账按 stem 走、两份实例同一个 stem 也就一次登记。
                // 真正的「停用/删文件」仍然走 unload_plugin → cleanup。
                self.try_load(&p).is_ok()
            }
            None => false,
        }
    }

    /// 加载所有未停用的插件（停用的跳过，其代码完全不执行）。
    pub fn load_all(&mut self) {
        let disabled = self.disabled_list();
        for path in self.discover() {
            let stem = Self::stem_of(&path);
            if disabled.iter().any(|s| s.as_str() == stem) {
                continue;
            }
            // 按文件名判重：插件展示名（PLUGIN_NAME）与文件名通常不同，
            // 用名字判重会导致每次扫描都重复加载。
            if self
                .plugins
                .values()
                .any(|p| Self::stem_of(&p.file_path) == stem)
            {
                continue;
            }
            let _ = self.try_load(&path);
        }
    }

    /// 获取插件列表。
    pub fn get_all_plugins(&self) -> Vec<&PluginInfo> {
        self.plugins.values().collect()
    }

    /// 获取单个插件。
    pub fn get_plugin(&self, name: &str) -> Option<&PluginInfo> {
        self.plugins.get(name)
    }

    /// 启用热重载：启动检测线程（只扫描文件，不执行 Lua）。
    pub fn enable_hot_reload(&mut self) {
        if self.hot_reload_thread.is_some() {
            return;
        }
        self.stop_event.store(false, Ordering::SeqCst);
        let stop = Arc::clone(&self.stop_event);
        let tx = self.reload_tx.clone();
        let dir = self.plugins_dir();
        let handle = std::thread::Builder::new()
            .name("plugin-hot-reload".into())
            .spawn(move || hot_reload_loop(dir, tx, stop))
            .map_err(|e| tracing::error!("启动热重载线程失败: {e}"))
            .ok();
        // spawn 失败时 .ok() 已经是 None，字段本身就该留 None
        self.hot_reload_thread = handle;
        tracing::info!("插件热重载已启用");
    }

    /// 禁用热重载。
    pub fn disable_hot_reload(&mut self) {
        self.stop_event.store(true, Ordering::SeqCst);
        if let Some(handle) = self.hot_reload_thread.take() {
            let _ = handle.join();
        }
        tracing::info!("插件热重载已禁用");
    }

    /// GUI 线程轮询：处理热重载请求。返回本次实际换上来的插件名列表
    /// （文件已被删除的那些，卸载成功后也算 —— 调用方要刷新列表的就是这批）。
    ///
    /// 删除这条在 core 这边原先是**检测到也没人处理**的：`reload_plugin` 按名字找不
    /// 到实例就返回 false，于是"删掉 .lua"要等下次重启才生效，而删除表达的恰恰是
    /// 「别再跑它了」（它的 cleanup 也就一直不执行）。桌面侧早就按这个口径做了，
    /// 见 `desktop/src/plugins.rs` 的 `reload_plugin_by_key`。
    pub fn poll_reload_requests(&mut self) -> Vec<String> {
        let mut reloaded = Vec::new();
        let dir = self.plugins_dir();
        while let Ok(name) = self.reload_rx.try_recv() {
            if !dir.join(format!("{name}.lua")).exists() {
                if self.unload_by_stem(&name) {
                    tracing::info!("插件文件已删除，已卸载: {name}");
                    reloaded.push(name);
                }
                continue;
            }
            if self.reload_plugin(&name) {
                reloaded.push(name);
            }
        }
        reloaded
    }

    /// 调用插件函数（GUI 线程）。
    /// 返回是否成功（函数存在且执行无错）。
    pub fn call_plugin_fn<R>(
        &mut self,
        name: &str,
        fn_name: &str,
        args: mlua::MultiValue,
    ) -> Result<R, String>
    where
        R: mlua::FromLua + mlua::IntoLua,
    {
        let info = self
            .plugins
            .get(name)
            .ok_or_else(|| format!("插件不存在: {name}"))?;
        let lua = info
            .lua
            .as_ref()
            .ok_or_else(|| format!("插件未加载: {name}"))?;
        let f: mlua::Function = lua
            .globals()
            .get(fn_name)
            .map_err(|e| format!("获取 {fn_name} 失败: {e}"))?;
        let ret: R = match f.call(args) {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("调用 {fn_name} 失败: {e}");
                if is_limit_error(&e) {
                    self.mark_plugin_error(name, msg.clone());
                }
                return Err(msg);
            }
        };
        Ok(ret)
    }

    /// 重新执行 get_view() 并更新视图缓存。
    /// 动作/输入回写会改变插件状态，前端拿到的视图必须是新鲜的，
    /// 否则会出现"点了没反应"（如分页不切换）。
    pub fn refresh_view(&mut self, name: &str) -> Result<(), String> {
        let cfg = self.config;
        let info = self
            .plugins
            .get(name)
            .ok_or_else(|| format!("插件不存在: {name}"))?;
        let lua = info
            .lua
            .as_ref()
            .ok_or_else(|| format!("插件未加载: {name}"))?;
        // 无 get_view 的插件没有视图，跳过
        if lua.globals().get::<mlua::Function>("get_view").is_err() {
            return Ok(());
        }
        Self::arm_instruction_budget(lua, cfg);
        let view = match Self::read_view(lua) {
            Ok(v) => v,
            Err(e) => {
                let msg = format!("get_view() 失败: {e}");
                if is_limit_error(&e) {
                    self.mark_plugin_error(name, msg.clone());
                }
                return Err(msg);
            }
        };
        if let Some(p) = self.plugins.get_mut(name) {
            p.view = Some(view);
        }
        Ok(())
    }

    /// 调用插件按钮动作（动作后刷新视图缓存）。
    ///
    /// 返回值只表示**动作**成不成功：`on_action` 已经跑完（任务删了、记录写了）之后
    /// 才失败的 `get_view()`，不能报成"动作失败"—— 那正是 `set_enabled` 修过的同一类
    /// 谎报（插件页每点一次「停用」弹一条失败，可插件确实已经卸载）。刷新失败留一条
    /// warn，视图沿用上一份，下一次 `get_plugin_view` 会再试并把错误摊开。
    pub fn plugin_action(&mut self, name: &str, action_id: &str) -> Result<(), String> {
        let cfg = self.config;
        let info = self
            .plugins
            .get(name)
            .ok_or_else(|| format!("插件不存在: {name}"))?;
        let lua = info
            .lua
            .as_ref()
            .ok_or_else(|| format!("插件未加载: {name}"))?;
        // 先检查是否有 on_action
        if let Ok(on_action) = lua.globals().get::<mlua::Function>("on_action") {
            Self::arm_instruction_budget(lua, cfg);
            if let Err(e) = on_action.call::<()>(action_id) {
                let msg = format!("on_action 失败: {e}");
                if is_limit_error(&e) {
                    self.mark_plugin_error(name, msg.clone());
                }
                return Err(msg);
            }
            if let Err(e) = self.refresh_view(name) {
                tracing::warn!("插件 {name} 动作已执行，但视图刷新失败: {e}");
            }
            return Ok(());
        }
        Err("插件未定义 on_action".to_string())
    }

    /// 向插件投递按键事件（番茄钟联动等）。
    pub fn plugin_key_event(&mut self, name: &str, key: &str) {
        let cfg = self.config;
        let info = match self.plugins.get(name) {
            Some(i) => i,
            None => return,
        };
        let lua = match &info.lua {
            Some(l) => l,
            None => return,
        };
        if let Ok(record_key) = lua.globals().get::<mlua::Function>("record_key") {
            Self::arm_instruction_budget(lua, cfg);
            if let Err(e) = record_key.call::<()>(key) {
                if is_limit_error(&e) {
                    self.mark_plugin_error(name, format!("record_key 失败: {e}"));
                }
            }
        }
    }

    /// 调用插件 set_field(field, value)（输入框回传，随后刷新视图缓存）。
    pub fn plugin_set_field(&mut self, name: &str, field: &str, value: &str) -> Result<(), String> {
        let cfg = self.config;
        let info = self
            .plugins
            .get(name)
            .ok_or_else(|| format!("插件不存在: {name}"))?;
        let lua = info
            .lua
            .as_ref()
            .ok_or_else(|| format!("插件未加载: {name}"))?;
        if let Ok(set_field) = lua.globals().get::<mlua::Function>("set_field") {
            Self::arm_instruction_budget(lua, cfg);
            if let Err(e) = set_field.call::<()>((field, value)) {
                let msg = format!("set_field 失败: {e}");
                if is_limit_error(&e) {
                    self.mark_plugin_error(name, msg.clone());
                }
                return Err(msg);
            }
            if let Err(e) = self.refresh_view(name) {
                tracing::warn!("插件 {name} 输入已写入，但视图刷新失败: {e}");
            }
            return Ok(());
        }
        Err("插件未定义 set_field".to_string())
    }
}

/// 判断两个路径指向的是不是**同一个源文件**。
///
/// 撞名闸改成按源文件判重（见 `load_plugin`）之后，这一步的准头直接决定"重载"和
/// "撞名"分不分得开：判成同一个文件 → 放行（那是自我重载，旧条目随后退休）；
/// 判成两个文件 → 拒。所以先按原样比，再 `canonicalize` 比一次（同一个文件可能以
/// 相对/绝对、大小写不同的写法传进来），都拿不到就保守地当成**两个**文件 ——
/// 宁可多拒一次撞名，也不能把"两个插件抢一个名字"放过去。
fn is_same_source(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// 判断 Lua 错误是否由资源限制触发（指令数超限 / 内存超限）。
/// 此类错误说明插件已不可信，应停用插件而非继续复用其 Lua 环境。
fn is_limit_error(e: &mlua::Error) -> bool {
    matches!(e, mlua::Error::MemoryError(_)) || e.to_string().contains("指令数上限")
}

/// 热重载检测循环：扫描插件目录 mtime，变更时发送重载请求。
/// 不执行 Lua（Lua 非 Send，只能在 GUI 线程）。
///
/// 三条保护是照桌面侧那份（`desktop/src/plugins.rs::start_hot_reload`）补的 ——
/// 那三条都是那边先踩过、写过注释才定下来的：
/// 1. 目录整体没读出来时**既不比对、也不清基线**（见 [`scan_plugin_mtimes`]）；
/// 2. **文件消失**也要发请求（见 [`reload_requests`]），否则删掉的插件要重启才停；
/// 3. 0 字节的 `.lua` 本轮不算变更（编辑器"先截断再写入"的一瞬间，见
///    [`scan_plugin_mtimes`]）—— 空的 .lua 会被 Lua 当成空块、一句错都不报，
///    插件就此变成没有函数的空壳。
fn hot_reload_loop(dir: PathBuf, tx: mpsc::Sender<String>, stop: Arc<AtomicBool>) {
    let mut last_mtime: HashMap<String, Option<SystemTime>> = HashMap::new();
    // 基线是否已经配平过（首轮只建基线不发事件，否则每次启动都把全部插件当"新文件"
    // 重载一遍）
    let mut primed = false;
    while !stop.load(Ordering::SeqCst) {
        for name in hot_reload_round(&dir, &mut last_mtime, &mut primed) {
            tracing::info!("热重载检测到变更: {name}");
            let _ = tx.send(name);
        }
        std::thread::sleep(std::time::Duration::from_millis(2000));
    }
}

/// 一轮扫描的全部判定（线程里除了 sleep 就只剩这一句，判定本身全在这里 —— 这样
/// 上面那三条保护才测得动，不用去起真线程、也不用等 2 秒一周期的 tick）。
/// 返回这一轮该发出去的重载请求；`last` / `primed` 是跨轮状态。
fn hot_reload_round(
    dir: &Path,
    last: &mut HashMap<String, Option<SystemTime>>,
    primed: &mut bool,
) -> Vec<String> {
    // `None` = 这一轮目录没读出来：基线与 `primed` 一个字都不动，下一轮再说。
    let Some(seen) = scan_plugin_mtimes(dir, last) else {
        return Vec::new();
    };
    let requests = reload_requests(last, &seen, *primed);
    *last = seen;
    *primed = true;
    requests
}

/// 扫一次插件目录，得到「文件名 → mtime」快照；**返回 `None` 表示目录没读出来**
/// （`plugins` 被建成同名文件、整份放在断线的网盘上、构建脚本正在替换目录……）。
/// 这条区分必须传出去：把"没读到"当"目录空了"参与比对，每个插件都会被判成
/// 「文件已删除」而全部卸载；把基线清空，则下一轮成功读取会把所有文件当成新变更，
/// 白重载一整轮。
///
/// `last` 只用来给 0 字节的文件沿用上一轮指纹（本轮既不算新增也不算消失）。
fn scan_plugin_mtimes(
    dir: &Path,
    last: &HashMap<String, Option<SystemTime>>,
) -> Option<HashMap<String, Option<SystemTime>>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::error!(
                "热重载扫描读不到插件目录（{}），本轮不判定任何变更 —— 这不是「没有插件」: {e}",
                dir.display()
            );
            return None;
        }
    };
    let mut seen: HashMap<String, Option<SystemTime>> = HashMap::new();
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.extension().map(|e| e == "lua").unwrap_or(false) {
            continue;
        }
        let Some(name) = p.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&p) else {
            continue;
        };
        if meta.len() == 0 {
            // 0 字节 = 还没写完的那一瞬间：沿用上一轮指纹（没有就不记），
            // 否则删除检测会把它误判成"文件消失"，而插件页会拿到一个空壳。
            if let Some(prev) = last.get(name) {
                seen.insert(name.to_string(), *prev);
            }
            continue;
        }
        seen.insert(name.to_string(), meta.modified().ok());
    }
    Some(seen)
}

/// 两轮快照之间该发哪些重载请求：mtime 变了、出现了新文件、**文件消失了**。
/// 只遍历 `seen` 会漏掉最后一种 —— 而删掉文件表达的意思恰恰是「别再跑它了」。
/// `primed == false`（首轮）什么都不发。返回排序后的稳定顺序，便于日志与测试对账。
fn reload_requests(
    last: &HashMap<String, Option<SystemTime>>,
    seen: &HashMap<String, Option<SystemTime>>,
    primed: bool,
) -> Vec<String> {
    if !primed {
        return Vec::new();
    }
    let mut out: Vec<String> = seen
        .iter()
        .filter(|(name, mtime)| last.get(*name) != Some(mtime))
        .map(|(name, _)| name.clone())
        .collect();
    for name in last.keys() {
        if !seen.contains_key(name) {
            out.push(name.clone());
        }
    }
    out.sort();
    out
}

/// 把 Lua 源码里**不是代码**的部分抹掉：`--` 行注释、`--[[ ]]` / `[==[ ]==]` 块注释、
/// `[[ ]]` 长字符串。目标是让"这一行的第一个代码记号"这件事可以用**列号**回答：
/// - 整段正好从行首开始 → 直接删掉（调用处连它后面的空白一起吞掉），于是
///   `--[[ 说明 ]] PLUGIN_NAME = "值"` 这种"注释打头、后面跟着真声明"的一行仍算顶层；
/// - 否则等长换成空格（保住后面那些字节的列号）；
/// - 换行一律原样保留：行号不能错位，而块注释/长字符串**内部**那些顶在第 0 列的
///   假赋值整段都在被抹掉的区间里，这正是这一轮要挡掉的东西。
///
/// 为什么不写成"正经的 Lua 词法分析器"：这里只需要回答"这一行是不是顶层的
/// `KEY = "字面量"`"，而扫错名字的代价已经写在 `PluginManager::scan_meta` 上了。
/// 引号内的 `--` 不当注释（`PLUGIN_DESC = "a -- b"` 整值被抹掉就是假阴性）；
/// 单行字符串到行尾还没闭合时后半段原样保留（半截文件本来就语法错，别再把手上
/// 唯一一处真名字也吃掉）。所有切片走 `.get()`：读的是外部文件，release 是 abort。
fn strip_lua_non_code(src: &str) -> String {
    let b = src.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    // 刚删掉一段行首的注释/长字符串：连它后面到下一个代码记号之间的空白一起吞
    let mut swallow = false;
    while i < b.len() {
        match b[i] {
            b'-' if b.get(i + 1) == Some(&b'-') => {
                // `--[[ … ]]` 抹到配平的 `]==]`（没有结尾就是文件被写坏，抹到文件尾）；
                // `-- …` 抹到行尾（换行本身留着）。
                let end = match long_bracket_open(b, i + 2) {
                    Some((level, after)) => {
                        long_bracket_close_from(b, after, level).unwrap_or(b.len())
                    }
                    None => newline_from(b, i).unwrap_or(b.len()),
                };
                swallow = blank_non_code(&mut out, &b[i..end]);
                i = end;
            }
            b'[' if long_bracket_open(b, i).is_some() => {
                // 长字符串里的 `PLUGIN_NAME = "…"` 是数据，不是声明
                let (level, after) = long_bracket_open(b, i).expect("上面刚判定过是长括号");
                let end = long_bracket_close_from(b, after, level).unwrap_or(b.len());
                swallow = blank_non_code(&mut out, &b[i..end]);
                i = end;
            }
            q @ (b'"' | b'\'') => {
                // 字符串照原样保留（值正是我们要取的东西），整段"吃掉"不再往里看
                swallow = false;
                out.push(q);
                let mut j = i + 1;
                while let Some(&x) = b.get(j) {
                    if x == b'\\' {
                        out.push(x);
                        if let Some(&y) = b.get(j + 1) {
                            out.push(y);
                        }
                        j += 2;
                        continue;
                    }
                    j += 1;
                    out.push(x);
                    if x == b'\n' || x == q {
                        break; // 闭合，或这一行根本没闭合
                    }
                }
                i = j;
            }
            b' ' | b'\t' if swallow => {
                i += 1;
            }
            c => {
                swallow = false;
                out.push(c);
                i += 1;
            }
        }
    }
    // 正常一定解得开（只写 ASCII 空白 + 原文的合法片段）；解不开也不 panic，退化成 lossy。
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// 把一段"不是代码"的字节追加进 `out`（见 [`strip_lua_non_code`] 的两种形状）。
/// 返回 `true` = 这一段是从行首整段删掉的，后面的空白该一起吞掉。
fn blank_non_code(out: &mut Vec<u8>, chunk: &[u8]) -> bool {
    if out.last().is_none_or(|&c| c == b'\n') {
        out.extend(chunk.iter().copied().filter(|&c| c == b'\n'));
        true
    } else {
        out.extend(chunk.iter().map(|&c| if c == b'\n' { b'\n' } else { b' ' }));
        false
    }
}

/// `i` 处是否是长括号起始 `[[` / `[=[` / `[==[`…：返回 (等号个数, 起始标记之后的下标)。
fn long_bracket_open(b: &[u8], i: usize) -> Option<(usize, usize)> {
    if b.get(i) != Some(&b'[') {
        return None;
    }
    let mut j = i + 1;
    let mut level = 0usize;
    while b.get(j) == Some(&b'=') {
        level += 1;
        j += 1;
    }
    if b.get(j) == Some(&b'[') {
        Some((level, j + 1))
    } else {
        None
    }
}

/// 从 `i` 起找配平的长括号结束标记 `]` + `=`×level + `]`，返回其之后的下标。
fn long_bracket_close_from(b: &[u8], mut i: usize, level: usize) -> Option<usize> {
    loop {
        i = b.get(i..)?.iter().position(|&c| c == b']')? + i;
        let mut j = i + 1;
        let mut n = 0usize;
        while b.get(j) == Some(&b'=') {
            n += 1;
            j += 1;
        }
        if n == level && b.get(j) == Some(&b']') {
            return Some(j + 1);
        }
        i += 1;
    }
}

/// `i` 起第一个换行的下标（`None` = 到文件尾）。
fn newline_from(b: &[u8], i: usize) -> Option<usize> {
    Some(b.get(i..)?.iter().position(|&c| c == b'\n')? + i)
}

/// 从一行**已抹掉注释**的 Lua 代码里取顶层元数据赋值 `KEY = "值"`（取不到 `None`）。
///
/// 刻意不做转义、不接受 `[[长字符串]]` 当值、也不接受 `KEY = 变量` 这类计算值：
/// 元数据这几个字段就是给人看的名字/简介/版本，扫不到时调用方回退成文件名，
/// 换来的是"列表刷新绝不执行插件代码"。
///
/// 两条判据都是为了"别把假名字当真的"（旧实现都不挡）：
/// - **不 trim 行首空白**：带缩进说明它写在某个块里，函数体里的
///   `PLUGIN_NAME = "…"` 只是个局部变量，加载后 `read_meta` 从 globals 里取不到它；
/// - **不接受 `local KEY = …`**：`local` 的同样不会成为全局变量。
fn top_level_string_assign(line: &str, key: &str) -> Option<String> {
    let after_key = line.strip_prefix(key)?.trim_start();
    let value = after_key.strip_prefix('=')?.trim_start();
    let quote = *value.as_bytes().first()?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let inner = value.get(1..)?;
    let end = inner.find(quote as char)?;
    Some(inner.get(..end)?.to_string())
}

/// 解析 Lua 表的 options 数组为 (value, label) 列表。
fn parse_options(t: &mlua::Table) -> mlua::Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    if let Ok(opts) = t.get::<mlua::Table>("options") {
        for (_, o) in opts.pairs::<mlua::Value, mlua::Table>().flatten() {
            out.push((
                o.get::<String>("value").unwrap_or_default(),
                o.get::<String>("label").unwrap_or_default(),
            ));
        }
    }
    Ok(out)
}

/// 解析 Lua 表为声明式控件。
fn parse_widget(w: &mlua::Table, depth: u32) -> mlua::Result<crate::plugins::Widget> {
    // 深度闸：`modal_form.widgets` 与 `row.children` 是递归下降解析的，而插件表是
    // **外部数据**。没有上限时一句 `local w={type="row",children={}}; w.children[1]=w`
    // 就成了自引用 → 递归无界 → 打穿线程栈。栈溢出是 SIGSEGV：不走 panic hook
    // （那场给 hook 加 flush 也救不了这条），release 又是 abort + 无控制台，
    // 用户看到的是"双击没反应/开着开着就没了"，连一行死因都没有。
    // 无环但要撞上限也只需要几千层，插件作者手滑（把上一行的 row 塞进新 row）即可命中。
    if depth > MAX_WIDGET_DEPTH {
        return Err(mlua::Error::RuntimeError(format!(
            "控件嵌套超过 {MAX_WIDGET_DEPTH} 层，已停止解析该控件（疑似自引用的控件表）"
        )));
    }
    let wtype: String = w.get("type")?;
    use crate::plugins::{FormField, Widget};
    match wtype.as_str() {
        "label" => Ok(Widget::Label(w.get("text").unwrap_or_default())),
        "heading" => Ok(Widget::Heading(w.get("text").unwrap_or_default())),
        "keyvalue" => Ok(Widget::KeyValue(
            w.get("key").unwrap_or_default(),
            w.get("value").unwrap_or_default(),
        )),
        "separator" => Ok(Widget::Separator),
        "textarea" => Ok(Widget::TextArea(w.get("text").unwrap_or_default())),
        "textinput" => Ok(Widget::TextInput {
            field: w.get("field").unwrap_or_default(),
            label: w
                .get("label")
                .or_else(|_| w.get("text"))
                .unwrap_or_default(),
            value: w.get("value").unwrap_or_default(),
        }),
        "button" => Ok(Widget::Button {
            id: w.get("id").unwrap_or_default(),
            text: w.get("text").unwrap_or_default(),
            disabled: w.get("disabled").unwrap_or(false),
            modal: w.get::<Option<String>>("modal").unwrap_or(None),
            sel: w.get("sel").unwrap_or(false),
            group: w.get("group").unwrap_or_default(),
        }),
        "select" => Ok(Widget::Select {
            field: w.get("field").unwrap_or_default(),
            label: w
                .get("label")
                .or_else(|_| w.get("text"))
                .unwrap_or_default(),
            value: w.get("value").unwrap_or_default(),
            options: parse_options(w)?,
            refresh: w.get("refresh").unwrap_or(false),
        }),
        "modal_form" => {
            let mut fields = Vec::new();
            if let Ok(fields_val) = w.get::<mlua::Table>("fields") {
                for (_, f) in fields_val.pairs::<mlua::Value, mlua::Table>().flatten() {
                    fields.push(FormField {
                        kind: f.get("kind").unwrap_or_else(|_| "text".into()),
                        field: f.get("field").unwrap_or_default(),
                        label: f.get("label").unwrap_or_default(),
                        value: f.get("value").unwrap_or_default(),
                        options: parse_options(&f)?,
                        refresh: f.get("refresh").unwrap_or(false),
                    });
                }
            }
            // 弹窗内自定义操作按钮（(动作 id, 文字)）
            let mut buttons = Vec::new();
            if let Ok(btns_val) = w.get::<mlua::Table>("buttons") {
                for (_, b) in btns_val.pairs::<mlua::Value, mlua::Table>().flatten() {
                    buttons.push((
                        b.get::<String>("id").unwrap_or_default(),
                        b.get::<String>("text").unwrap_or_default(),
                    ));
                }
            }
            // 弹窗主体内嵌控件（表格等）
            let mut widgets = Vec::new();
            if let Ok(ws_val) = w.get::<mlua::Table>("widgets") {
                for (_, c) in ws_val.pairs::<mlua::Value, mlua::Table>().flatten() {
                    if let Ok(widget) = parse_widget(&c, depth + 1) {
                        widgets.push(widget);
                    }
                }
            }
            Ok(Widget::ModalForm {
                id: w.get("id").unwrap_or_default(),
                title: w.get("title").unwrap_or_default(),
                submit: w.get("submit").unwrap_or_default(),
                submit_text: w.get("submit_text").unwrap_or_default(),
                cancel: w.get("cancel").unwrap_or_default(),
                content: w.get("content").unwrap_or_default(),
                open: w.get("open").unwrap_or(false),
                fields,
                buttons,
                widgets,
            })
        }
        "row" => {
            let mut children = Vec::new();
            if let Ok(children_val) = w.get::<mlua::Table>("children") {
                for (_, c) in children_val.pairs::<mlua::Value, mlua::Table>().flatten() {
                    if let Ok(widget) = parse_widget(&c, depth + 1) {
                        children.push(widget);
                    }
                }
            }
            Ok(Widget::Row { children })
        }
        "pager" => Ok(Widget::Pager {
            page: w.get("page").unwrap_or(1),
            pages: w.get("pages").unwrap_or(1),
            total: w.get("total").unwrap_or(0),
            prev_id: w.get("prev").unwrap_or_default(),
            next_id: w.get("next").unwrap_or_default(),
        }),
        "table" => {
            let headers: Vec<String> = w.get("headers").unwrap_or_default();
            let mut rows = Vec::new();
            if let Ok(rows_val) = w.get::<mlua::Table>("rows") {
                for (_, row) in rows_val.pairs::<mlua::Value, mlua::Table>().flatten() {
                    let mut cols = Vec::new();
                    for (_, v) in row.pairs::<mlua::Value, mlua::Value>().flatten() {
                        if let Ok(s) = v.to_string() {
                            cols.push(s);
                        }
                    }
                    rows.push(cols);
                }
            }
            let ids: Vec<String> = w
                .get::<Vec<mlua::Value>>("ids")
                .unwrap_or_default()
                .iter()
                .map(|v| match v {
                    mlua::Value::Integer(n) => n.to_string(),
                    mlua::Value::String(s) => s.to_string_lossy().to_string(),
                    mlua::Value::Number(f) => (*f as i64).to_string(),
                    _ => v.to_string().unwrap_or_default(),
                })
                .collect();
            let mut actions = Vec::new();
            if let Ok(actions_val) = w.get::<mlua::Table>("actions") {
                for (_, a) in actions_val.pairs::<mlua::Value, mlua::Table>().flatten() {
                    actions.push((
                        a.get::<String>("prefix").unwrap_or_default(),
                        a.get::<String>("text").unwrap_or_default(),
                    ));
                }
            }
            Ok(Widget::Table {
                headers,
                rows,
                ids,
                actions,
                group: w.get("group").unwrap_or_default(),
                onselect: w.get("onselect").unwrap_or_default(),
            })
        }
        _ => Ok(Widget::Label(format!("[未知控件: {wtype}]"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 往 `plugins/` 里放一个插件文件（路径按 `discover()` 的口径来）。
    fn write_lua(dir: &Path, file: &str, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(format!("{file}.lua"));
        std::fs::write(&p, body).unwrap();
        p
    }

    /// 一个用到调度线程、并按 `host.rs` 的说明在 cleanup 里归还的插件。
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

    /// 一个有视图、别的什么都不干的插件。
    fn view_only(name: &str) -> String {
        format!(
            "PLUGIN_NAME = \"{name}\"\nfunction get_view() return {{ title = \"{name}\", \
             widgets = {{ {{type=\"label\", text=\"v\"}} }} }} end\n"
        )
    }

    /// 每个用例一套**自己的** config：`config::instance()` 是另一个进程级全局，
    /// 一旦被哪个用例初始化，后面任何一次 `config.set` 都会把盘写到它那个已经删掉的
    /// 临时目录里（`%TEMP%` 就是这么堆起来的）。这里只 `load`，且用例一律不碰 `set`。
    fn manager_in(tmp: &Path) -> PluginManager {
        db::queries::invalidate_years_cache();
        let config: &'static FocusFlowConfig = Box::leak(Box::new(
            FocusFlowConfig::load(tmp.join("config.ini")).expect("临时配置应能载入"),
        ));
        PluginManager::new(config, db::Database::init_readonly())
    }

    // ---------- 第八节 A7：热重载失败不许把还能用的插件弄丢 ----------

    /// 把一个还能用的插件文件写成半截语法错误（编辑器自动保存、网盘同步到一半都会这样）：
    /// 重载必须"什么都没发生"，而不是旧实例已被摘除、新版本又装不回来。
    ///
    /// 旧实现是 `plugins.remove(n)` 之后才 `try_load`，于是那一刻：插件从列表里消失、
    /// 正打开的详情页变成「插件未提供视图」，而它记在 `claim_scheduler` 上的 owner
    /// 成了无主孤儿 —— 唯一的释放入口是插件 Lua 里的 `scheduler_shutdown()`，
    /// 而那份 Lua 状态已经跟着条目一起没了 → 本进程的调度线程再也还不回来。
    #[test]
    fn a_failed_reload_leaves_the_old_plugin_exactly_where_it_was() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("reload_keeps_old");
        let plugins = tmp.path().join("plugins");
        let p = write_lua(&plugins, "keep_old", &scheduler_user("重载别丢我"));
        let mut pm = manager_in(tmp.path());
        pm.load_plugin(&p).expect("好版本该能加载");
        assert_eq!(
            host::scheduler_state_for_test(),
            Some(1),
            "init() 用过调度线程就该把这个文件名登记成使用者"
        );

        // 半截的语法错误
        write_lua(
            &plugins,
            "keep_old",
            "PLUGIN_NAME = \"重载别丢我\"\nfunction init() 这行不是 Lua ((( \n",
        );
        assert!(
            !pm.reload_plugin("keep_old"),
            "坏版本不能让重载报成功（否则调用方以为换上了新的）"
        );

        // 旧实例还在表里，而且照常能渲染（详情页不该变成「插件未提供视图」）
        let info = pm
            .get_plugin("重载别丢我")
            .expect("旧实例必须还挂着：摘掉它的那一步只能在新版本装好之后发生");
        assert!(info.loaded, "旧实例不该被顺手标成未加载");
        assert_eq!(info.file_path, p);
        pm.refresh_view("重载别丢我")
            .expect("旧实例的 get_view 该照常能跑");
        assert!(pm.get_plugin("重载别丢我").unwrap().view.is_some());
        // owner 记账一个字都不许动 —— 这份账现在是旧实例的，不是孤儿的
        assert_eq!(
            host::scheduler_state_for_test(),
            Some(1),
            "加载失败时把旧实例的 owner 销掉，等于把定时任务的线程拆了"
        );

        // 反向腿：旧实例活着 ⇒ 停用就能回收。旧实现走到这里条目已经没了，
        // `unload_plugin` 返回 false、owner 永远留在表上（= 上面那句"再也回收不掉"）。
        assert!(pm.unload_plugin("重载别丢我"), "卸载应当成功");
        assert_eq!(
            host::scheduler_state_for_test(),
            None,
            "最后一个用户走了就该回收"
        );
    }

    /// 撞名闸按**源文件**判重之后，两个方向都要钉住：
    /// - 改了名的重载要真能换上来，并且把旧名字那条一起带走（不然同一个文件两份实例，
    ///   旧那份再没有 unload 入口、cleanup 永不执行）；
    /// - 撞上**别的文件**占用的名字仍然要拒，而且被拒的那次不许动旧实例。
    #[test]
    fn a_renamed_reload_retires_the_old_entry_and_still_respects_the_gate() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("reload_rename");
        let plugins = tmp.path().join("plugins");
        let p = write_lua(&plugins, "ren", &view_only("旧名"));
        let mut pm = manager_in(tmp.path());
        pm.load_plugin(&p).expect("加载失败");

        write_lua(&plugins, "ren", &view_only("新名"));
        assert!(pm.reload_plugin("ren"), "改名重载应当成功");
        assert!(pm.get_plugin("新名").is_some(), "新名字该进表");
        assert!(
            pm.get_plugin("旧名").is_none(),
            "insert 顶不掉改了名的旧条目，必须显式退休，否则留一份僵尸"
        );
        assert_eq!(
            pm.get_all_plugins().len(),
            1,
            "同一个文件不该同时有两份实例"
        );

        let other = write_lua(&plugins, "neighbour", &view_only("邻居"));
        pm.load_plugin(&other).expect("加载邻居失败");
        write_lua(&plugins, "ren", &view_only("邻居"));
        assert!(!pm.reload_plugin("ren"), "抢了别人的名字要被拒");
        assert!(
            pm.get_plugin("新名").is_some(),
            "被拒的那次重载不许把旧实例一起带走"
        );
        assert_eq!(pm.get_all_plugins().len(), 2);
    }

    /// `init()` 跑一半才失败（首次启用/启动补载这条路上没有"旧实例"可留）：
    /// 那次尝试在调度线程上登记的 owner 也要跟着回滚，不然同样留下一个本进程
    /// 再也回收不掉的孤儿 —— 而 `release_scheduler` 只有 Lua 侧一个调用点。
    #[test]
    fn a_failed_first_load_does_not_leave_an_orphan_scheduler_owner() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("failed_load_owner");
        let plugins = tmp.path().join("plugins");
        write_lua(
            &plugins,
            "orphan",
            "PLUGIN_NAME = \"跑一半就坏\"\nfunction init() focusflow.scheduler_tasks() \
             error(\"下一行才坏\") end\n",
        );
        let mut pm = manager_in(tmp.path());
        pm.load_all();
        assert!(
            pm.get_plugin("跑一半就坏").is_none(),
            "加载失败的插件不该在表里"
        );
        assert_eq!(
            host::scheduler_state_for_test(),
            None,
            "这次尝试登记下的 owner 必须销掉：表里没有它的插件，就再没人能归还"
        );
    }

    // ---------- 第八节 A8：元数据扫描只认真正的顶层赋值 ----------

    /// `--[[ ]]` 里被注释掉的旧值、函数体里缩进的同名赋值、`local PLUGIN_NAME`、
    /// 顶层长字符串里的那一行，都不许赢过真声明。
    ///
    /// 扫错的代价写在 `scan_meta` 上：插件管理页的「打开」按钮传的就是这里扫出来的
    /// 名字（`desktop/ui/js/plugins.js` 的 `open-plugin`），名字对不上就是点一次
    /// 报一次「插件未提供视图」；旧实现"取第一条匹配行"，只要注释那行带缩进就先命中。
    #[test]
    fn scan_meta_only_takes_top_level_assignments() {
        let path = Path::new("whatever.lua");
        let name_of = |src: &str| PluginManager::scan_meta_source(src, path).0;

        // 块注释里的假名正好顶在第 0 列 —— 旧实现就是它赢
        assert_eq!(
            name_of("--[[\nPLUGIN_NAME = \"注释掉的旧名\"\n--]]\nPLUGIN_NAME = \"真名\"\n",),
            "真名",
            "块注释里的行不能算声明"
        );
        // 同行块的 `--[[ … ]]`、以及 `--[==[ … ]==]` 这种带等号的形状
        assert_eq!(
            name_of("--[[ PLUGIN_NAME = \"同行的假名\" ]] PLUGIN_NAME = \"真名2\"\n"),
            "真名2"
        );
        assert_eq!(
            name_of("--[==[\nPLUGIN_NAME = \"等号块里的\"\n]==]\nPLUGIN_NAME = \"真名3\"\n"),
            "真名3"
        );
        // 函数体里缩进的同名赋值（那是个局部变量，加载后 globals 里根本没有它）
        assert_eq!(
            name_of(
                "function init()\n  PLUGIN_NAME = \"函数体里的\"\nend\nPLUGIN_NAME = \"顶层的\"\n",
            ),
            "顶层的",
            "带缩进的说明它写在某个块里"
        );
        // `local` 的同样不会成为全局变量
        assert_eq!(
            name_of("local PLUGIN_NAME = \"局部的\"\nPLUGIN_NAME = \"全局的\"\n"),
            "全局的"
        );
        // 顶层长字符串里的那一行是数据
        assert_eq!(
            name_of("local hint = [[\nPLUGIN_NAME = \"串里的\"\n]]\nPLUGIN_NAME = \"真名4\"\n"),
            "真名4"
        );
        // 行注释（旧实现靠"行首不是 --"侥幸挡住，现在由同一套规则挡住）
        assert_eq!(
            name_of("-- PLUGIN_NAME = \"行注释里的\"\nPLUGIN_NAME = \"真名5\"\n"),
            "真名5"
        );
    }

    /// 正常形状照旧扫得出来（含"简介里带 `--`"这种不能被注释规则吃掉的形状），
    /// 什么都没有时回退成文件名 —— 这两条不是新行为，是防上面那套规则改过头的网。
    #[test]
    fn scan_meta_still_reads_normal_fields() {
        let path = Path::new("whatever.lua");
        let (name, desc, version, author) = PluginManager::scan_meta_source(
            "PLUGIN_NAME = '单引号也行'\nPLUGIN_DESC = \"含 -- 破折号的简介\"\n\
             PLUGIN_VERSION = \"1.2.3\"\nPLUGIN_AUTHOR = \"某人\"\n",
            path,
        );
        assert_eq!(name, "单引号也行");
        assert_eq!(desc, "含 -- 破折号的简介", "引号里的 -- 不是注释");
        assert_eq!(version, "1.2.3");
        assert_eq!(author, "某人");

        let (name, desc, version, author) =
            PluginManager::scan_meta_source("function init() end\n", path);
        assert_eq!(name, "whatever", "扫不到就回退成文件名");
        assert_eq!(desc, "");
        assert_eq!(version, "1.0");
        assert_eq!(author, "");
    }

    // ---------- 第八节 A11：core 自己那份热重载扫描的三条保护 ----------

    /// 新文件要报、删文件也要报（旧实现只遍历本轮看到的，消失的那个永远没人提），
    /// 而 0 字节的 `.lua` 本轮不能算"有新版本"—— 空的 .lua 会被 Lua 当成空块、
    /// 一句错都不报，插件就此变成没有函数的空壳。
    #[test]
    fn hot_reload_round_reports_new_and_deleted_files_but_not_empty_ones() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("hr_new_del");
        let dir = tmp.path().join("plugins");
        write_lua(&dir, "a", "PLUGIN_NAME = \"甲\"\n");
        let mut last = HashMap::new();
        let mut primed = false;

        assert!(
            hot_reload_round(&dir, &mut last, &mut primed).is_empty(),
            "首轮只建基线，否则每次启动都把全部插件重载一遍"
        );
        assert!(primed);

        // 半截文件（编辑器"先截断再写入"的那一瞬间 / 网盘还没同步上来）
        write_lua(&dir, "b", "");
        assert!(
            hot_reload_round(&dir, &mut last, &mut primed).is_empty(),
            "0 字节的 b 不该被当成新版本，也不该被记进基线"
        );
        assert!(!last.contains_key("b"), "空壳不该有指纹");

        // 内容写上了 → 这才算一个新文件
        write_lua(&dir, "b", "PLUGIN_NAME = \"乙\"\n");
        assert_eq!(hot_reload_round(&dir, &mut last, &mut primed), ["b"]);

        // 已知插件被截成 0 字节：本轮不发请求，指纹沿用上一轮
        let known = last["a"];
        std::thread::sleep(std::time::Duration::from_millis(60));
        write_lua(&dir, "a", "");
        assert!(
            hot_reload_round(&dir, &mut last, &mut primed).is_empty(),
            "0 字节 = 还没写完，不能判成变更（旧实现在这里发请求，把空壳装上去）"
        );
        assert_eq!(last["a"], known, "删除检测不能把截断误判成文件消失");

        // 真删掉：必须报（旧实现漏这一整条）
        std::fs::remove_file(dir.join("a.lua")).unwrap();
        assert_eq!(hot_reload_round(&dir, &mut last, &mut primed), ["a"]);
        assert!(!last.contains_key("a"), "基线要跟着收缩");
    }

    /// 目录整体没读出来（`plugins` 被建成同名文件、整份放在断线的网盘上、构建脚本
    /// 正在替换目录）时既不判定任何变更，**也不许把基线清空** —— 清空的话下一轮
    /// 成功读取会把所有文件都当成新变更，白重载一整轮。
    #[test]
    fn hot_reload_round_keeps_the_baseline_when_the_dir_cannot_be_listed() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("hr_unreadable");
        let dir = tmp.path().join("plugins");
        write_lua(&dir, "a", "PLUGIN_NAME = \"甲\"\n");
        write_lua(&dir, "b", "PLUGIN_NAME = \"乙\"\n");
        let not_a_dir = write_lua(tmp.path(), "not_a_dir", "这是个文件，不是目录\n");
        let mut last = HashMap::new();
        let mut primed = false;
        assert!(hot_reload_round(&dir, &mut last, &mut primed).is_empty());
        let baseline = last.clone();

        assert!(
            scan_plugin_mtimes(&not_a_dir, &last).is_none(),
            "读不出来要当成「不知道」，不是「目录空了」"
        );
        assert!(
            hot_reload_round(&not_a_dir, &mut last, &mut primed).is_empty(),
            "读不出来的那一轮不该判成\"所有插件都被删了\""
        );
        assert_eq!(last, baseline, "基线一个字都不许动");
        assert!(
            hot_reload_round(&dir, &mut last, &mut primed).is_empty(),
            "下一轮成功读取不能把每个文件都当成新变更（旧实现在这里全员重载）"
        );
    }

    /// `poll_reload_requests` 这条路上，"文件已经不在盘上"要真的把插件卸掉。
    /// 扫描线程那边现在会发删除事件了，而 core 这头的处理原先是空的：
    /// `reload_plugin` 按名字找不到实例就返回 false → 删掉的插件要重启才停得下来，
    /// 而删除表达的意思恰恰是「别再跑它了」（桌面侧 `reload_plugin_by_key` 早就做了）。
    #[test]
    fn poll_unloads_a_plugin_whose_file_was_deleted() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("hr_poll_delete");
        let plugins = tmp.path().join("plugins");
        let p = write_lua(&plugins, "gone", &scheduler_user("会被删掉的"));
        let mut pm = manager_in(tmp.path());
        pm.load_plugin(&p).expect("加载失败");
        assert_eq!(host::scheduler_state_for_test(), Some(1));

        std::fs::remove_file(&p).unwrap();
        // 不经线程：直接往检测线程那条 channel 里塞一条请求
        pm.reload_tx.send("gone".to_string()).unwrap();
        assert_eq!(pm.poll_reload_requests(), ["gone"], "删除该被当成一次卸载");
        assert!(
            pm.get_plugin("会被删掉的").is_none(),
            "文件没了却还在跑 = cleanup 永不执行"
        );
        assert_eq!(
            host::scheduler_state_for_test(),
            None,
            "卸载走了 cleanup，owner 才还得回来"
        );
    }

    /// 死循环必须被指令数上限打断，且沙箱里不能留着"换个线程躲开 hook"的出口。
    ///
    /// 实测（去掉本修复前）：同样规模的主线程循环被 hook 中断，而包进
    /// `coroutine.create` 就一路跑完不报错 —— Lua 的 hook 是 per-thread 的。
    /// 用有界循环而不是 `while true`：绕过成立时测试会自己跑完，不会挂住 CI。
    #[test]
    fn instruction_limit_cannot_be_escaped_by_coroutines() {
        let lua = PluginManager::create_sandboxed_lua().unwrap();
        lua.set_hook(
            HookTriggers::new().every_nth_instruction(1_000),
            |_lua, _dbg| -> mlua::Result<VmState> {
                Err(mlua::Error::RuntimeError("超出指令数上限".into()))
            },
        );

        let err = lua
            .load("local n = 0 for i = 1, 200000 do n = n + i end")
            .exec()
            .expect_err("主线程死循环必须被 hook 中断");
        assert!(err.to_string().contains("指令数上限"), "{err}");

        assert!(
            lua.globals()
                .get::<mlua::Value>("coroutine")
                .unwrap()
                .is_nil(),
            "协程是唯一能把指令计数搬走的入口，必须整体不可用"
        );
        let escaped = lua
            .load(
                "coroutine.resume(coroutine.create(function() \
                 local n = 0 for i = 1, 200000 do n = n + i end end))",
            )
            .exec();
        assert!(
            escaped.is_err(),
            "协程入口被移除后这段要立刻报错，而不是安静跑完"
        );
    }

    /// 沙箱必须让 io 库不可用、os 高危函数与 package.loadlib 被移除，
    /// 同时保留 os.date 等插件在用的安全函数。
    #[test]
    fn sandboxed_lua_blocks_dangerous_stdlib() {
        let lua = PluginManager::create_sandboxed_lua().unwrap();
        // io 库整体不可用
        assert!(lua.globals().get::<mlua::Value>("io").unwrap().is_nil());
        // os 高危函数已移除
        for name in ["execute", "exit", "getenv", "remove", "rename", "tmpname"] {
            let v: mlua::Value = lua.load(format!("return os.{name}")).eval().unwrap();
            assert!(v.is_nil(), "os.{name} 应为 nil");
        }
        // package.loadlib 已禁用
        let loadlib: mlua::Value = lua.load("return package.loadlib").eval().unwrap();
        assert!(loadlib.is_nil());
        // base 库 mlua 会无条件打开（白名单里没有 BASE 也挡不住），能落盘的两个
        // 入口只能按名字摘掉；协程库则整体不在白名单内（hook 是 per-thread 的）
        for name in ["dofile", "loadfile", "coroutine"] {
            let v: mlua::Value = lua.globals().get(name).unwrap();
            assert!(v.is_nil(), "{name} 应为 nil");
        }
        // 插件在用的安全函数仍在
        for expr in [
            "return os.date",
            "return os.time",
            "return os.clock",
            "return string.format",
            "return math.floor",
        ] {
            lua.load(expr).exec().unwrap();
        }
        // 运行时兜底：io.open 即便被伪造调用也应报"未定义"
        let err = lua.load("return io.open('C:/x.txt','w')").exec();
        let Err(err) = err else {
            panic!("io 不可用时必须报错");
        };
        assert!(err.to_string().contains("io"));
    }
}
