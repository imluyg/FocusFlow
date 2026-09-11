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

/// 插件管理器（GUI 线程专用，不跨线程共享）。
pub struct PluginManager {
    config: &'static FocusFlowConfig,
    db: Arc<db::Database>,
    plugins: HashMap<String, PluginInfo>,
    /// 热重载停止标志
    stop_event: Arc<AtomicBool>,
    /// 热重载检测线程句柄
    hot_reload_thread: Option<std::thread::JoinHandle<()>>,
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
            stop_event: Arc::new(AtomicBool::new(false)),
            hot_reload_thread: None,
            reload_rx: rx,
            reload_tx: tx,
        }
    }

    /// 创建沙箱化的 Lua 状态：
    /// - 剔除 `io` 库（任意文件读写）；
    /// - 移除 `os` 中可触达系统的高危函数（保留 date/time/clock 供插件使用）；
    /// - 禁用 `package.loadlib`/`cpath`（防加载任意 DLL）。
    ///
    /// 配合 `apply_lua_limits` 的内存/指令数限制构成完整沙箱。
    fn create_sandboxed_lua() -> mlua::Result<Lua> {
        // 显式白名单，不用 ALL_SAFE：后者含 io 库，且未来 mlua 加入新库时不会默默放行。
        let libs = StdLib::COROUTINE
            | StdLib::TABLE
            | StdLib::STRING
            | StdLib::UTF8
            | StdLib::MATH
            | StdLib::PACKAGE
            | StdLib::OS;
        let lua = Lua::new_with(libs, LuaOptions::default())?;
        let globals = lua.globals();
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
    /// 配置项：config.ini [plugins] memory_limit_mb（默认 16）/ instruction_limit（默认 1000 万）。
    /// Lua 状态创建后调用一次即可覆盖该状态后续所有执行路径。
    fn apply_lua_limits(&self, lua: &Lua) {
        let mem_mb = self.config.get_int("plugins", "memory_limit_mb", 16).max(1) as usize;
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
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();

        let lua = Self::create_sandboxed_lua().map_err(|e| e.to_string())?;
        self.apply_lua_limits(&lua);
        host::register_host_api(&lua, self.config, Arc::clone(&self.db))
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
                    if let Ok(widget) = parse_widget(&w) {
                        view.widgets.push(widget);
                    }
                }
            }
        }
        Ok(view)
    }

    /// 卸载插件。
    pub fn unload_plugin(&mut self, name: &str) -> bool {
        if let Some(mut info) = self.plugins.remove(name) {
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
                // 找到插件名用于卸载
                let pname = self
                    .plugins
                    .values()
                    .find(|x| x.file_path == p)
                    .map(|x| x.name.clone());
                if let Some(n) = &pname {
                    self.unload_plugin(n);
                }
                self.load_plugin(&p).is_ok()
            }
            None => false,
        }
    }

    /// 加载所有插件。
    pub fn load_all(&mut self) {
        for path in self.discover() {
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("plugin")
                .to_string();
            if !self.plugins.contains_key(&name) {
                let _ = self.load_plugin(&path);
            }
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
            .expect("启动热重载线程失败");
        self.hot_reload_thread = Some(handle);
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
            self.refresh_view(name)?;
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
            self.refresh_view(name)?;
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
        let err = lua
            .load("return io.open('C:/x.txt','w')")
            .exec()
            .err()
            .expect("io 不可用时必须报错");
        assert!(err.to_string().contains("io"));
    }
}
