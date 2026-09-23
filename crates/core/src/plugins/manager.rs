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
        let instr = self
            .config
            .get_int("plugins", "instruction_limit", 10_000_000)
            .clamp(1, u32::MAX as i64) as u32;
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
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().map(|e| e == "lua").unwrap_or(false))
                    .collect()
            })
            .unwrap_or_default();
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
    fn scan_meta(path: &Path) -> Result<(String, String, String, String), String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let field = |key: &str| -> Option<String> {
            text.lines()
                .find_map(|line| literal_after_assign(line, key))
        };
        Ok((
            field("PLUGIN_NAME").unwrap_or_else(|| Self::stem_of(path)),
            field("PLUGIN_DESC").unwrap_or_default(),
            field("PLUGIN_VERSION").unwrap_or_else(|| "1.0".to_string()),
            field("PLUGIN_AUTHOR").unwrap_or_default(),
        ))
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
        let stem = Self::stem_of(path);
        if let Some(clash) = self
            .plugins
            .get(&meta.name)
            .filter(|p| Self::stem_of(&p.file_path) != stem)
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
        host::register_host_api(
            &lua,
            self.config,
            Arc::clone(&self.db),
            &Self::stem_of(path),
        )
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
                    match parse_widget(&w) {
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
        if let Some(mut info) = self.plugins.remove(name) {
            // 卸载 = 用户不要它了（停用/删除/重启前的一次回收），旧的加载失败原因
            // 不该继续挂在插件页上。
            self.load_errors.remove(&Self::stem_of(&info.file_path));
            if let Some(lua) = &mut info.lua {
                if let Ok(cleanup) = lua.globals().get::<mlua::Function>("cleanup") {
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
                // 找到插件名用于摘除
                let pname = self
                    .plugins
                    .values()
                    .find(|x| x.file_path == p)
                    .map(|x| x.name.clone());
                // 重载刻意**不**跑 cleanup()：这条路径是「存了个文件」触发的
                // （mtime 变化、构建脚本换目录、编辑器先截断再写入），而番茄插件的
                // cleanup 会 `pomodoro_stop()` —— 那段 `save_current()` 只要实际计时
                // ≥1 秒就给 `work_finished += 1`。于是按一次 Ctrl+S 就把用户跑到一半的
                // 番茄钟判成「完成一个」，还是静默的。调度线程同理，拆了要靠重新渲染
                // 定时任务面板才起得回来。
                // 摘掉条目即可：旧 Lua 状态随它一起释放，新的那份经宿主 API 复用
                // 同一批进程级单例（沙箱里没有 io/coroutine，插件本身持不住别的资源）。
                // 真正的「停用/删文件」仍然走 unload_plugin → cleanup。
                if let Some(n) = &pname {
                    self.plugins.remove(n);
                }
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

    /// GUI 线程轮询：处理热重载请求。返回本次重载的插件名列表。
    pub fn poll_reload_requests(&mut self) -> Vec<String> {
        let mut reloaded = Vec::new();
        while let Ok(name) = self.reload_rx.try_recv() {
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
        let info = match self.plugins.get(name) {
            Some(i) => i,
            None => return,
        };
        let lua = match &info.lua {
            Some(l) => l,
            None => return,
        };
        if let Ok(record_key) = lua.globals().get::<mlua::Function>("record_key") {
            if let Err(e) = record_key.call::<()>(key) {
                if is_limit_error(&e) {
                    self.mark_plugin_error(name, format!("record_key 失败: {e}"));
                }
            }
        }
    }

    /// 调用插件 set_field(field, value)（输入框回传，随后刷新视图缓存）。
    pub fn plugin_set_field(&mut self, name: &str, field: &str, value: &str) -> Result<(), String> {
        let info = self
            .plugins
            .get(name)
            .ok_or_else(|| format!("插件不存在: {name}"))?;
        let lua = info
            .lua
            .as_ref()
            .ok_or_else(|| format!("插件未加载: {name}"))?;
        if let Ok(set_field) = lua.globals().get::<mlua::Function>("set_field") {
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

/// 判断 Lua 错误是否由资源限制触发（指令数超限 / 内存超限）。
/// 此类错误说明插件已不可信，应停用插件而非继续复用其 Lua 环境。
fn is_limit_error(e: &mlua::Error) -> bool {
    matches!(e, mlua::Error::MemoryError(_)) || e.to_string().contains("指令数上限")
}

/// 热重载检测循环：扫描插件目录 mtime，变更时发送重载请求。
/// 不执行 Lua（Lua 非 Send，只能在 GUI 线程）。
fn hot_reload_loop(dir: PathBuf, tx: mpsc::Sender<String>, stop: Arc<AtomicBool>) {
    let mut last_mtime: HashMap<String, Option<SystemTime>> = HashMap::new();
    let mut first_scan = true;
    while !stop.load(Ordering::SeqCst) {
        // 扫描目录
        let mut seen: HashMap<String, Option<SystemTime>> = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().map(|e| e == "lua").unwrap_or(false) {
                    let name = p
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string();
                    if !name.is_empty() {
                        let mtime = std::fs::metadata(&p).and_then(|m| m.modified()).ok();
                        seen.insert(name, mtime);
                    }
                }
            }
        }
        if first_scan {
            last_mtime = seen;
            first_scan = false;
        } else {
            // 检测变更（mtime 变化或新文件）
            for (name, mtime) in &seen {
                let changed = match last_mtime.get(name) {
                    Some(prev) => *prev != *mtime,
                    None => true, // 新文件
                };
                if changed {
                    tracing::info!("热重载检测到变更: {name}");
                    let _ = tx.send(name.clone());
                }
            }
            last_mtime = seen;
        }
        std::thread::sleep(std::time::Duration::from_millis(2000));
    }
}

/// 从一行 Lua 源码里取 `KEY = "值"` 的字面量值（取不到返回 `None`）。
///
/// 刻意不做转义、不支持 `[[长字符串]]`、也不接受 `KEY = 变量` 这类计算值：
/// 元数据这几个字段就是给人看的名字/简介/版本，扫不到时调用方回退成文件名，
/// 换来的是"列表刷新绝不执行插件代码"。
fn literal_after_assign(line: &str, key: &str) -> Option<String> {
    let trimmed = line.trim();
    let body = trimmed.strip_prefix("local ").unwrap_or(trimmed);
    let after_key = body.strip_prefix(key)?.trim_start();
    let value = after_key.strip_prefix('=')?.trim_start();
    let quote = *value.as_bytes().first()?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let inner = &value[1..];
    let end = inner.find(quote as char)?;
    Some(inner[..end].to_string())
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
fn parse_widget(w: &mlua::Table) -> mlua::Result<crate::plugins::Widget> {
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
                    if let Ok(widget) = parse_widget(&c) {
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
                    if let Ok(widget) = parse_widget(&c) {
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
