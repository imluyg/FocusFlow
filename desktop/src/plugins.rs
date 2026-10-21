//! 插件交互：PluginManager 含 Lua（非 Send），只能留在主线程。
//! Tauri 同步命令跑在主线程（WebView2 IPC），用 thread_local 持久持有，
//! 保证 get_view / 按钮动作 / 输入框回写之间共享插件 Lua 状态。

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::Serialize;
use tauri::{Emitter, State};

use focusflow_core::db::Database;
use focusflow_core::listener::InputListener;
use focusflow_core::plugins::manager::PluginManager;
use focusflow_core::plugins::Widget;

use crate::state::AppState;

thread_local! {
    static PM: RefCell<Option<PluginManager>> = const { RefCell::new(None) };
}

/// 记下主线程 id（`setup` 里调用一次），供 `with_manager` 自查。
pub fn note_main_thread() {
    let _ = MAIN_THREAD.set(std::thread::current().id());
}

static MAIN_THREAD: std::sync::OnceLock<std::thread::ThreadId> = std::sync::OnceLock::new();

/// 获取（必要时初始化）主线程插件管理器并执行操作。
pub fn with_manager<T>(db: &Arc<Database>, f: impl FnOnce(&mut PluginManager) -> T) -> T {
    if let Some(id) = MAIN_THREAD.get() {
        // `PM` 是 thread_local：从别的线程调进来不会报错，而是**再造一个完整的
        // PluginManager**（第二次 load_all、第二批 Lua 状态， yet 共用同一批进程级
        // 单例：调度线程、番茄钟、事件投递）。今天所有调用方都在主线程（Tauri 同步命令
        // 与 run_on_main_thread），所以这条只是护栏 —— 一旦以后有人图省事把某个插件命令
        // 改成 async，这里会先喊出来，而不是让人去查"为什么插件收了两遍键事件"。
        if std::thread::current().id() != *id {
            tracing::error!("with_manager 被非主线程调用，将另建一份 PluginManager");
        }
    }
    PM.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            let mut pm = PluginManager::new(focusflow_core::config::instance(), Arc::clone(db));
            pm.load_all();
            *slot = Some(pm);
        }
        f(slot.as_mut().unwrap())
    })
}

/// 热重载监听开关（前端打开/关闭插件管理页时切换）。
/// 只在插件管理页打开时扫描目录，平时零后台开销。
static WATCH_TX: std::sync::OnceLock<std::sync::mpsc::Sender<bool>> = std::sync::OnceLock::new();

/// 设置是否监听插件目录变更（true=打开插件管理页，false=离开）。
pub fn set_watch(watch: bool) {
    if let Some(tx) = WATCH_TX.get() {
        let _ = tx.send(watch);
    }
}

/// 目录快照：`插件文件名(stem) → mtime`。
///
/// 读不到目录时返回 `listed=false`，调用方必须保留旧基线、一个事件都不发：
/// 否则拿一份空的 `seen` 去比，每个插件都会被判定成「文件已删除」而全部卸载 ——
/// 而目录整体读不到恰恰是构建脚本正在整份替换、或杀软首扫握着句柄的时候。
fn scan_plugin_dir(
    dir: &std::path::Path,
    prev: &HashMap<String, Option<SystemTime>>,
) -> (bool, HashMap<String, Option<SystemTime>>) {
    let mut seen: HashMap<String, Option<SystemTime>> = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (false, seen);
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.extension().map(|e| e == "lua").unwrap_or(false) {
            continue;
        }
        let Some(name) = p.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
            continue;
        };
        let Ok(meta) = std::fs::metadata(&p) else {
            continue;
        };
        // 编辑器常有「先截断再写入」：0 字节的 .lua 会被解析成空块而且不报错，
        // 插件就此变成没有任何函数的空壳。本轮当作「还没写完」，沿用上一轮的指纹
        // （否则删除检测会把它误判成文件消失），等下一次轮询到非空内容再触发重载。
        if meta.len() == 0 {
            if let Some(prev_mtime) = prev.get(&name) {
                seen.insert(name, *prev_mtime);
            }
            continue;
        }
        seen.insert(name, meta.modified().ok());
    }
    (true, seen)
}

/// 热重载的 mtime 基线。
///
/// **必须在扫描线程启动时就武装一次**，不能等插件管理页第一次打开：
/// 首扫"只建基线不发事件"是为了避免每次开页都把全部文件当变更连发（列表反复重建会
/// 吞掉用户正在点的行），但把武装也推到第一次开页，就留下一个窗口 ——
/// 程序启动之后、第一次开页之前放进 `plugins/` 的那个 .lua 会被当成基线本身吸收掉，
/// 于是列表里它写着已启用、实际从没被 `load_all` 加载过，点「打开」只有一句
/// 「插件未提供视图」，而唯一让它生效的办法是重启程序。
#[derive(Default)]
struct PluginBaseline {
    map: HashMap<String, Option<SystemTime>>,
    primed: bool,
}

impl PluginBaseline {
    /// 用一轮扫描结果更新基线，返回本轮该发事件的文件名（升序，方便用例断言）。
    ///
    /// 未武装的那一轮（启动首扫）只建基线不发事件；`listed=false` 的那轮既不发言、
    /// 也不替换基线（保持上一轮的，等下一次读到再说）。
    fn apply(&mut self, listed: bool, seen: HashMap<String, Option<SystemTime>>) -> Vec<String> {
        let mut changed: Vec<String> = Vec::new();
        if !listed {
            return changed;
        }
        if self.primed {
            for (name, mtime) in &seen {
                if self.map.get(name) != Some(mtime) {
                    changed.push(name.clone());
                }
            }
            // 删除也要发事件：只遍历 seen 会漏掉消失的文件，插件就得重启才停得下来
            // —— 而删掉文件表达的意思恰恰是「别再跑它了」。
            for name in self.map.keys() {
                if !seen.contains_key(name) {
                    changed.push(name.clone());
                }
            }
        }
        self.map = seen;
        self.primed = true;
        changed.sort();
        changed
    }
}

/// 启动插件热重载（Tauri 版）。
/// 核心 PluginManager 自带的热重载需要 GUI 每帧 poll_reload_requests，
/// Tauri 没有这个循环，这里用独立实现：扫描线程只在插件管理页打开时
/// 监视 plugins/ 的 mtime 变更，变更通过 channel 交给投递线程，
/// 再用 run_on_main_thread 回到主线程重载（Lua 非 Send，只能主线程操作）。
///
/// 返回两条线程是否都起来了（启动自检 B15 用；失败各自已记 error 日志）。
pub fn start_hot_reload(app: &tauri::AppHandle, db: Arc<Database>) -> bool {
    let (ctl_tx, ctl_rx) = std::sync::mpsc::channel::<bool>();
    let _ = WATCH_TX.set(ctl_tx);
    let (tx, rx) = std::sync::mpsc::channel::<String>();

    // 扫描线程：仅监听状态下每 2 秒比对 mtime，变更时发送插件文件名。
    // 基线在线程启动时就武装（见 `PluginBaseline`）：只把"第一次成功扫描"当作基线，
    // 而不是等插件管理页打开 —— 后者会让启动之后才放进目录的插件被当成基线吸收掉，
    // 于是列表写着已启用、实际从没加载。
    let scan_ok = std::thread::Builder::new()
        .name("plugin-hot-reload-scan".into())
        .spawn(move || {
            let dir = focusflow_core::paths::plugins_dir();
            let mut bl = PluginBaseline::default();
            // 武装基线：这一步在"要不要监听"那道闸**之前**。
            // 读不到目录时 apply() 不置 `primed`，循环里的首轮会接替武装（宁可多等一轮，
            // 也不要拿一份空基线去比出"全部删除"）。
            let (listed0, snap0) = scan_plugin_dir(&dir, &bl.map);
            let _ = bl.apply(listed0, snap0);
            let mut watching = false;
            loop {
                while let Ok(w) = ctl_rx.try_recv() {
                    watching = w;
                    if w {
                        tracing::debug!("插件热重载监听已开启");
                    } else {
                        tracing::debug!("插件热重载监听已暂停");
                    }
                }
                if !watching {
                    std::thread::sleep(Duration::from_millis(300));
                    continue;
                }
                let (listed, seen) = scan_plugin_dir(&dir, &bl.map);
                for name in bl.apply(listed, seen) {
                    let _ = tx.send(name);
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        })
        .map_err(|e| tracing::error!("启动插件热重载扫描线程失败（该功能不可用）: {e}"))
        .is_ok();

    // 投递线程：收到变更 → 主线程执行重载，并通知前端刷新插件列表
    let app_owned = app.clone();
    let apply_ok = std::thread::Builder::new()
        .name("plugin-hot-reload-apply".into())
        .spawn(move || {
            while let Ok(name) = rx.recv() {
                let app = app_owned.clone();
                let app_emit = app.clone();
                let db = Arc::clone(&db);
                let _ = app.run_on_main_thread(move || {
                    tracing::info!("热重载检测到变更: {name}");
                    reload_plugin_by_key(&name, &db);
                    let _ = app_emit.emit("plugins-reloaded", name);
                });
            }
        })
        .map_err(|e| tracing::error!("启动插件热重载应用线程失败（该功能不可用）: {e}"))
        .is_ok();
    // spawn 失败时上面各自已 error，别再谎报「已就绪」
    if scan_ok && apply_ok {
        tracing::info!("插件热重载已就绪（监听随插件管理页开关）");
    }
    scan_ok && apply_ok
}

/// 某个插件文件"在不在"的三种结局。
///
/// 刻意不用 `Path::exists()`：那句话对**任何** stat 失败都回 false，于是"查不动"与
/// "确实被删了"在调用方眼里长得一模一样 —— 而这条道上两种结局要做的事相反。
/// 本文件上面 `scan_plugin_dir` 那段已经把目录级的那一半钉住了（"整个目录读不到时
/// 保留旧基线、一个事件都不发，因为读不到恰恰是构建脚本正在整份替换、或杀软首扫
/// 握着句柄的时候"）；这里是同一条规则缺的那一半：单个文件。
#[derive(Debug)]
enum FileState {
    /// 是个读得到的普通文件
    Present,
    /// 确实没有（`NotFound`）：删除检测的唯一合法证据
    Missing,
    /// 查不动，或那个名字上挂着的不是普通文件 —— **不能据此卸载**
    Unreadable(String),
}

fn probe_plugin_file(dir: &std::path::Path, key: &str) -> FileState {
    let path = dir.join(format!("{key}.lua"));
    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_file() => FileState::Present,
        Ok(_) => FileState::Unreadable(format!(
            "{} 不是普通文件（同步占位、或被换成目录）",
            path.display()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileState::Missing,
        Err(e) => FileState::Unreadable(format!("{}: {e}", path.display())),
    }
}

/// 按插件文件名（stem）重载插件（主线程调用）。
/// 文件已消失 → 卸载：删除插件文件表达的就是「别再跑它了」，只 reload 不 remove
/// 的话它会一直跑到下次重启（其 cleanup 也就一直不执行）。
/// 未找到已加载实例 → 补扫一次目录：兜底加载运行期间新放入的插件文件。
///
/// "查不动"那一臂**两边都不做**，只把原因说出来：原先这里是 `!exists()` ⇒ 卸载，
/// 一次杀软首扫握着句柄就把还在跑的插件停掉（cleanup 也一并跑掉），而列表上它仍写着
/// 已启用；日志还多说一句它没证明过的「文件已删除」。更要紧的是这一轮的判定已经被
/// `PluginBaseline::apply` 连同新 mtime 一起提交了，下一轮看不见变化 ⇒ 这个插件会
/// **一直**停在卸载态直到重启。留到下一轮重新问一次，代价只是这一轮什么都不做。
fn reload_plugin_by_key(key: &str, db: &Arc<Database>) {
    let dir = focusflow_core::paths::plugins_dir();
    with_manager(db, |pm| match probe_plugin_file(&dir, key) {
        FileState::Missing => {
            if pm.unload_by_stem(key) {
                tracing::info!("插件文件已删除，已卸载: {key}");
            }
        }
        FileState::Unreadable(why) => {
            tracing::warn!(
                "插件 {key} 这一轮两边都不做：文件在不在都查不出来（{why}），\
                 它仍按上一份代码运行 —— 等下一次目录扫描再判，不据此卸载"
            );
        }
        FileState::Present => {
            if pm.reload_plugin(key) {
                tracing::info!("插件已热重载: {key}");
            } else {
                tracing::info!("未找到已加载插件 {key}，尝试补载新插件");
                pm.load_all();
            }
        }
    })
}

/// 键事件通道：listener 钩子线程投递键名，主线程分发给插件（番茄钟计数等）。
static KEY_EVENT_TX: std::sync::OnceLock<std::sync::mpsc::SyncSender<String>> =
    std::sync::OnceLock::new();

/// 接通键事件回调链。
/// mlua 的 Lua 非 Send，插件分发只能在主线程：钩子线程回调只往 channel 投递键名
/// （零阻塞），独立分发线程批量取出后经 run_on_main_thread 回主线程，
/// 对每个已加载插件调用 plugin_key_event。
///
/// 返回分发线程是否起来了（启动自检 B15 用；失败已记 error 日志）。
pub fn start_key_event_dispatch(
    app: &tauri::AppHandle,
    db: Arc<Database>,
    listener: &Arc<InputListener>,
) -> bool {
    // 有界通道：主线程（Lua 分发）被卡住（插件失控/模态框）时丢弃新事件，
    // 防止无界队列随时间无限积压。按键计数语义允许少量丢失，
    // 权威计数由 DB 写线程的内存聚合负责，这里只服务插件联动。
    let (tx, rx) = std::sync::mpsc::sync_channel::<String>(4096);
    let _ = KEY_EVENT_TX.set(tx);
    listener.add_key_callback(Arc::new(|key| {
        if let Some(tx) = KEY_EVENT_TX.get() {
            let _ = tx.try_send(key.to_string());
        }
    }));

    let app_owned = app.clone();
    let dispatch_ok = std::thread::Builder::new()
        .name("plugin-key-dispatch".into())
        .spawn(move || {
            // 单次主线程投递最多合并的按键数：连打时减少跨线程消息数量
            const MAX_BATCH: usize = 32;
            while let Ok(first) = rx.recv() {
                let mut keys = Vec::with_capacity(4);
                keys.push(first);
                while keys.len() < MAX_BATCH {
                    match rx.try_recv() {
                        Ok(k) => keys.push(k),
                        Err(_) => break,
                    }
                }
                let app = app_owned.clone();
                let db = Arc::clone(&db);
                let dispatch = move || {
                    with_manager(&db, |pm| {
                        let names: Vec<String> = pm
                            .get_all_plugins()
                            .iter()
                            .filter(|p| p.loaded)
                            .map(|p| p.name.clone())
                            .collect();
                        for name in names {
                            for key in &keys {
                                pm.plugin_key_event(&name, key);
                            }
                        }
                    });
                };
                if let Err(e) = app.run_on_main_thread(dispatch) {
                    tracing::debug!("键事件主线程分发失败（事件循环可能已退出）: {e}");
                    break;
                }
            }
        })
        .map_err(|e| tracing::error!("启动插件键事件分发线程失败（该功能不可用）: {e}"))
        .is_ok();
    // spawn 失败时上面已 error，别再谎报「已接通」
    if dispatch_ok {
        tracing::info!("插件键事件分发已接通");
    }
    dispatch_ok
}

/// 下拉/单选选项 (value, label)。
#[derive(Serialize, Clone, Default)]
pub struct OptionDto {
    pub value: String,
    pub label: String,
}

/// 弹窗表单字段（modal_form 控件内使用）。
#[derive(Serialize, Clone, Default)]
pub struct FormFieldDto {
    /// text | select | date
    pub kind: String,
    pub field: String,
    pub label: String,
    pub value: String,
    pub options: Option<Vec<OptionDto>>,
    /// 改动后重建视图（分类 → 子分类联动）。缺这一项时插件那边设了
    /// `refresh = true` 也会被 DTO 层吃掉，弹窗里的下拉就成了"改了没反应"。
    pub refresh: bool,
}

/// 表格行内操作按钮描述。
#[derive(Serialize, Clone, Default)]
pub struct TableActionDto {
    /// 动作 id 前缀（前端拼接 前缀+记录id）
    pub prefix: String,
    pub text: String,
}

/// 前端可序列化的控件描述。
#[derive(Serialize, Clone, Default)]
pub struct WidgetDto {
    pub kind: String,
    /// modal_form 弹窗标题（与 text 分开，避免前端渲染成按钮）
    pub title: Option<String>,
    pub text: Option<String>,
    pub label: Option<String>,
    pub key: Option<String>,
    pub value: Option<String>,
    pub headers: Option<Vec<String>>,
    pub rows: Option<Vec<Vec<String>>>,
    pub ids: Option<Vec<String>>,
    pub actions: Option<Vec<TableActionDto>>,
    pub id: Option<String>,
    pub field: Option<String>,
    pub disabled: Option<bool>,
    pub open: Option<bool>,
    /// button：点击打开的弹窗 id（替代插件动作）
    pub modal: Option<String>,
    /// button：点击动作 id 拼接表格选中行 id
    pub sel: Option<bool>,
    /// 表格/按钮的行选中分组名
    pub group: Option<String>,
    /// 表格：行选中时写入的插件字段名（联动刷新）
    pub onselect: Option<String>,
    /// select：变更后是否整页刷新
    pub refresh: Option<bool>,
    /// modal_form：提交按钮的插件动作 id
    pub submit: Option<String>,
    pub submit_text: Option<String>,
    /// modal_form：取消按钮（✕/取消）触发的插件动作 id
    pub cancel: Option<String>,
    /// modal_form：只读文本内容（统计结果等）
    pub content: Option<String>,
    pub fields: Option<Vec<FormFieldDto>>,
    /// select 选项
    pub options: Option<Vec<OptionDto>>,
    /// row 容器的子控件
    pub children: Option<Vec<WidgetDto>>,
    /// pager 数据
    pub page: Option<i64>,
    pub pages: Option<i64>,
    pub total: Option<i64>,
    pub prev: Option<String>,
    pub next: Option<String>,
}

/// 前端可序列化的插件视图。
#[derive(Serialize, Clone)]
pub struct PluginViewDto {
    pub title: String,
    pub widgets: Vec<WidgetDto>,
}

/// 控件转 DTO（递归支持 row 容器）。
fn widget_dto(w: &focusflow_core::plugins::Widget) -> WidgetDto {
    match w {
        Widget::Label(t) => WidgetDto {
            kind: "label".into(),
            text: Some(t.clone()),
            ..Default::default()
        },
        Widget::Heading(t) => WidgetDto {
            kind: "heading".into(),
            text: Some(t.clone()),
            ..Default::default()
        },
        Widget::KeyValue(k, v) => WidgetDto {
            kind: "keyvalue".into(),
            key: Some(k.clone()),
            value: Some(v.clone()),
            ..Default::default()
        },
        Widget::Table {
            headers,
            rows,
            ids,
            actions,
            group,
            onselect,
        } => WidgetDto {
            kind: "table".into(),
            headers: Some(headers.clone()),
            rows: Some(rows.clone()),
            ids: Some(ids.clone()),
            group: Some(group.clone()),
            onselect: Some(onselect.clone()),
            actions: Some(
                actions
                    .iter()
                    .map(|(p, t)| TableActionDto {
                        prefix: p.clone(),
                        text: t.clone(),
                    })
                    .collect(),
            ),
            ..Default::default()
        },
        Widget::Button {
            id,
            text,
            disabled,
            modal,
            sel,
            group,
        } => WidgetDto {
            kind: "button".into(),
            id: Some(id.clone()),
            text: Some(text.clone()),
            disabled: Some(*disabled),
            modal: modal.clone(),
            sel: Some(*sel),
            group: Some(group.clone()),
            ..Default::default()
        },
        Widget::Separator => WidgetDto {
            kind: "separator".into(),
            ..Default::default()
        },
        Widget::TextArea(t) => WidgetDto {
            kind: "textarea".into(),
            text: Some(t.clone()),
            ..Default::default()
        },
        Widget::TextInput {
            field,
            label,
            value,
        } => WidgetDto {
            kind: "textinput".into(),
            field: Some(field.clone()),
            label: Some(label.clone()),
            value: Some(value.clone()),
            ..Default::default()
        },
        Widget::Select {
            field,
            label,
            value,
            options,
            refresh,
        } => WidgetDto {
            kind: "select".into(),
            field: Some(field.clone()),
            label: Some(label.clone()),
            value: Some(value.clone()),
            refresh: Some(*refresh),
            options: Some(
                options
                    .iter()
                    .map(|(v, l)| OptionDto {
                        value: v.clone(),
                        label: l.clone(),
                    })
                    .collect(),
            ),
            ..Default::default()
        },
        Widget::ModalForm {
            id,
            title,
            submit,
            submit_text,
            cancel,
            content,
            open,
            fields,
            buttons,
            widgets,
        } => WidgetDto {
            kind: "modal_form".into(),
            id: Some(id.clone()),
            title: Some(title.clone()),
            submit: Some(submit.clone()),
            submit_text: Some(submit_text.clone()),
            cancel: Some(cancel.clone()),
            content: Some(content.clone()),
            open: Some(*open),
            children: Some(widgets.iter().map(widget_dto).collect()),
            actions: Some(
                buttons
                    .iter()
                    .map(|(prefix, text)| TableActionDto {
                        prefix: prefix.clone(),
                        text: text.clone(),
                    })
                    .collect(),
            ),
            fields: Some(
                fields
                    .iter()
                    .map(|f| FormFieldDto {
                        kind: f.kind.clone(),
                        field: f.field.clone(),
                        label: f.label.clone(),
                        value: f.value.clone(),
                        refresh: f.refresh,
                        options: Some(
                            f.options
                                .iter()
                                .map(|(v, l)| OptionDto {
                                    value: v.clone(),
                                    label: l.clone(),
                                })
                                .collect(),
                        ),
                    })
                    .collect(),
            ),
            ..Default::default()
        },
        Widget::Row { children } => WidgetDto {
            kind: "row".into(),
            children: Some(children.iter().map(widget_dto).collect()),
            ..Default::default()
        },
        Widget::Pager {
            page,
            pages,
            total,
            prev_id,
            next_id,
        } => WidgetDto {
            kind: "pager".into(),
            page: Some(*page),
            pages: Some(*pages),
            total: Some(*total),
            prev: Some(prev_id.clone()),
            next: Some(next_id.clone()),
            ..Default::default()
        },
    }
}

impl From<&focusflow_core::plugins::PluginView> for PluginViewDto {
    fn from(v: &focusflow_core::plugins::PluginView) -> Self {
        let widgets = v.widgets.iter().map(widget_dto).collect();
        PluginViewDto {
            title: v.title.clone(),
            widgets,
        }
    }
}

/// 插件视图转 DTO（插件无视图时返回 None）。
fn view_dto(pm: &PluginManager, name: &str) -> Option<PluginViewDto> {
    pm.get_plugin(name)
        .and_then(|p| p.view.as_ref())
        .map(PluginViewDto::from)
}

/// 获取插件视图（每次调用重新执行 get_view()，保证视图新鲜，
/// 番茄钟倒计时等动态内容依赖此刷新）。
#[tauri::command]
pub fn get_plugin_view(state: State<'_, Arc<AppState>>, name: String) -> Option<PluginViewDto> {
    with_manager(&state.db, |pm| {
        if let Err(e) = pm.refresh_view(&name) {
            // 渲染报错时退回上一次成功的视图。原先这里直接给 None，前端就把整页
            // 换成「插件未提供视图」—— 那是一句假话（插件明明有视图，只是这一次
            // get_view() 抛了错），还会顺手丢掉用户正在操作的分页/选中行。
            // 宁可让数字停在推开前的样子，下一次刷新成功就自己跟上；真·没有视图
            // （插件未加载/没有 get_view）时才由下面的 None 给出那句话。
            tracing::warn!("插件视图刷新失败 ({name})，沿用上一次视图: {e}");
        }
        view_dto(pm, &name)
    })
}

/// 触发插件按钮动作，返回刷新后的视图。
#[tauri::command]
pub fn plugin_action(
    state: State<'_, Arc<AppState>>,
    name: String,
    id: String,
) -> Result<Option<PluginViewDto>, String> {
    with_manager(&state.db, |pm| {
        pm.plugin_action(&name, &id)?;
        Ok(view_dto(pm, &name))
    })
}

/// 插件输入框回写，返回刷新后的视图。
#[tauri::command]
pub fn plugin_set_field(
    state: State<'_, Arc<AppState>>,
    name: String,
    field: String,
    value: String,
) -> Result<Option<PluginViewDto>, String> {
    with_manager(&state.db, |pm| {
        pm.plugin_set_field(&name, &field, &value)?;
        Ok(view_dto(pm, &name))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(items: &[(&str, u64)]) -> HashMap<String, Option<SystemTime>> {
        items
            .iter()
            .map(|(name, secs)| {
                (
                    name.to_string(),
                    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(*secs)),
                )
            })
            .collect()
    }

    /// 启动之后、第一次打开插件页之前放进目录的插件必须被发到事件里。
    ///
    /// 这就是被修掉的那个窗口：旧写法把"第一次成功扫描"当成武装基线，而第一次扫描
    /// 发生在开页之后 —— 于是那个窗口里新增的文件被当成基线本身吸收掉，列表写着
    /// 已启用、实际从没被 `load_all` 加载过，点「打开」只有一句「插件未提供视图」，
    /// 唯一生效办法是重启程序。（本用例只跑 `PluginBaseline`，防不住"把武装挪回闸后"
    /// 那种回潮 —— 那一半由下面那条源码形状用例守着。）
    #[test]
    fn a_plugin_dropped_before_the_first_page_open_still_fires() {
        let mut bl = PluginBaseline::default();
        // 线程启动时：目录里只有 stats_overview
        let fired = bl.apply(true, snap(&[("stats_overview", 100)]));
        assert!(
            fired.is_empty(),
            "武装那一轮不该发事件（否则每次开页全量重发）"
        );
        // 用户在没人开页的时候放进一个新插件，然后第一次开页
        let fired = bl.apply(true, snap(&[("stats_overview", 100), ("my_widget", 200)]));
        assert_eq!(fired, vec!["my_widget".to_string()], "新文件要发事件");
    }

    /// 目录读不到那一轮：既不发言，也不许替换基线。
    ///
    /// 拿一份空的 `seen` 去比，每个插件都会被判成「文件已删除」而全部卸载；
    /// 覆盖基线则下一轮成功读取会把全部文件当新变更，再来一整轮无谓重载。
    #[test]
    fn an_unreadable_round_changes_nothing() {
        let mut bl = PluginBaseline::default();
        bl.apply(true, snap(&[("a", 100), ("b", 200)]));
        assert!(
            bl.apply(false, HashMap::new()).is_empty(),
            "读不到目录的那一轮一个事件都不许发"
        );
        // 基线还是上一轮那两份 ⇒ 下一轮成功读取且内容没变，仍然不发事件
        assert!(bl.apply(true, snap(&[("a", 100), ("b", 200)])).is_empty());
    }

    /// 文件消失也要发事件：删掉插件文件表达的是「别再跑它了」。
    #[test]
    fn a_vanished_plugin_fires_so_it_can_be_unloaded() {
        let mut bl = PluginBaseline::default();
        bl.apply(true, snap(&[("a", 100), ("gone", 150)]));
        let fired = bl.apply(true, snap(&[("a", 100)]));
        assert_eq!(fired, vec!["gone".to_string()]);
    }

    /// 0 字节的 .lua 是"编辑器截断后还没写完"的中间态：沿用上一轮指纹。
    ///
    /// 判成 mtime 变了就会重载出一个没有任何函数的空壳；判成文件消失更糟 ——
    /// 直接把插件卸载掉。两种都比"这一轮什么都不做"坏。
    #[test]
    fn a_zero_byte_file_keeps_the_previous_fingerprint() {
        struct Tmp(std::path::PathBuf);
        impl Drop for Tmp {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let dir = std::env::temp_dir().join(format!("ff_plugin_scan_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("临时目录该建得出来");
        let _tmp = Tmp(dir.clone());
        let file = dir.join("w.lua");
        std::fs::write(&file, "function focusflow.card() end").unwrap();

        let (listed, first) = scan_plugin_dir(&dir, &HashMap::new());
        assert!(listed, "目录读到了");
        assert!(first.contains_key("w"), "有内容的 .lua 要进快照");

        // 截断成 0 字节：指纹必须原样留着（而不是变成 None、更不是消失）
        std::fs::write(&file, "").unwrap();
        let (_, second) = scan_plugin_dir(&dir, &first);
        assert_eq!(
            second.get("w"),
            first.get("w"),
            "0 字节那一轮要沿用上一轮的指纹"
        );
        assert!(second.contains_key("w"), "更不能把它判成文件已删除");

        // 全新文件正好是 0 字节：这一轮干脆不提它（上一轮没有指纹可沿用），
        // 等它有内容了再进事件 —— 否则基线里会出现一条 None 的空壳记录。
        std::fs::remove_file(&file).unwrap();
        std::fs::write(dir.join("fresh.lua"), "").unwrap();
        let (_, third) = scan_plugin_dir(&dir, &HashMap::new());
        assert!(
            !third.contains_key("fresh"),
            "没见过的 0 字节文件不该进这一轮快照"
        );
    }

    /// 形状守卫：武装基线那一步必须排在"要不要监听"那道闸**之前**。
    ///
    /// 上面那条行为用例跑的是抽出来的 `PluginBaseline`，它测不到线程体里"什么时候武装" ——
    /// 而这场缺陷恰恰就是武装的时机。只能按文本顺序钉（与本仓 `snip.rs`、
    /// `cli/main.rs` 那几条同族）。
    #[test]
    fn the_scan_thread_primes_the_baseline_before_the_watch_gate() {
        let src = include_str!("plugins.rs");
        let head = src
            .find("fn start_hot_reload")
            .expect("启动函数找不到了（判据要跟着改）");
        let body = &src[head..head + src[head..].find("\n}\n").expect("启动函数的结尾")];
        let prime = body
            .find("bl.apply(listed0, snap0)")
            .expect("扫描线程不再在启动时武装基线了");
        let gate = body
            .find("if !watching")
            .expect("监听那道闸不见了（判据要跟着改）");
        assert!(
            prime < gate,
            "武装必须排在闸之前（武装在第 {prime} 字、闸在第 {gate} 字）—— \
             挪回闸后就又留下「启动后、第一次开页前放进去的插件永不加载」那个窗口"
        );
        // 夹具自己的前提：切片真盖住了线程体，不是切成空串让上面两条空转
        assert!(
            body.contains("PluginBaseline::default()") && body.contains("ctl_rx.try_recv()"),
            "切片没盖住扫描线程，上面那条次序断言是假绿"
        );
    }

    /// 「这个插件文件还在不在」要分三种结局，`Path::exists()` 把前两种并成一件。
    ///
    /// 症状：一次杀软首扫/同步盘握着句柄让 stat 失败，`!exists()` 成立 ⇒ 正在跑的插件被
    /// `unload_by_stem` 停掉（cleanup 一并跑掉），列表上还写着已启用，而日志多说一句
    /// 「文件已删除」；这一轮的判定又已经被 `PluginBaseline::apply` 连同新 mtime 提交了，
    /// 下一轮看不见变化 ⇒ 它会一直停在卸载态直到重启。
    /// 「那个名字上挂着目录」这条腿是本仓现成的夹具手法（把路径换成目录，`metadata` 成功
    /// 但 `is_file()` 为假），也是 `exists()` 那一版唯一会**漏掉**的方向。
    #[test]
    fn an_unanswerable_file_check_is_not_read_as_a_deletion() {
        struct Tmp(std::path::PathBuf);
        impl Drop for Tmp {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let dir = std::env::temp_dir().join(format!("ff_plugin_probe_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("临时目录该建得出来");
        let _tmp = Tmp(dir.clone());
        std::fs::write(dir.join("good.lua"), "function focusflow.card() end").unwrap();
        std::fs::create_dir_all(dir.join("weird.lua")).unwrap();

        match probe_plugin_file(&dir, "good") {
            FileState::Present => {}
            other => panic!("读得到的普通文件要判成 Present，实得 {other:?}"),
        }
        match probe_plugin_file(&dir, "gone") {
            FileState::Missing => {}
            other => panic!("确实没有的那个要判成 Missing，实得 {other:?}"),
        }
        let weird = probe_plugin_file(&dir, "weird");
        match &weird {
            FileState::Unreadable(why) => assert!(
                why.contains("weird.lua"),
                "查不动那一臂要带上是哪个文件，实得 {why}"
            ),
            other => panic!("挂在目录上的名字不能算「文件在」，更不能算「已删除」，实得 {other:?}"),
        }

        // 反向腿：`exists()` 会被那个目录骗过去（它确实"存在"），所以判据必须来自 metadata
        assert!(
            dir.join("weird.lua").exists(),
            "夹具前提：那条名字本身是 exists() 为真的"
        );
    }

    /// 接线守卫：卸载只能挂在 `FileState::Missing` 那一臂上。
    ///
    /// 上面那条纯用例钉的是探测函数怎么分三态，它盯不到调用点拿这三态做了什么 ——
    /// 而这场缺陷的落点恰恰在调用点（`!exists()` ⇒ 卸载）。重载路径要真管理器 + 主线程，
    /// 用例进不去，所以钉调用形态。
    #[test]
    fn only_a_confirmed_deletion_unloads_a_plugin() {
        let src = include_str!("plugins.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let at = prod
            .find("fn reload_plugin_by_key")
            .expect("重载入口找不到了（判据要跟着改）");
        let body = &prod[at..at + prod[at..].find("\n}\n").expect("重载入口的结尾")];
        let mut wrong: Vec<String> = Vec::new();
        if !body.contains("probe_plugin_file(&dir, key)") {
            wrong.push("调用点不再过三态探测了".to_string());
        }
        if body.contains(".exists()") {
            wrong.push("调用点又用回 `exists()`（查不动＝已删除）".to_string());
        }
        // 卸载那句必须住在 Missing 那一臂里，而不是三态之外
        let missing = body
            .find("FileState::Missing")
            .expect("Missing 那一臂不见了");
        // 从**这一臂的标签之后**找下一个 `FileState::`：从 missing 本身找会立刻
        // 撞上标签自己，切出空串让下面两条断言空转（假绿）
        let arm_start = missing + "FileState::Missing".len();
        let arm_end = body[arm_start..]
            .find("FileState::")
            .map(|n| arm_start + n)
            .unwrap_or(body.len());
        if !body[missing..arm_end].contains("unload_by_stem(key)") {
            wrong.push("卸载没挂在 Missing 那一臂里".to_string());
        }
        if !body.contains("FileState::Unreadable(why)") {
            wrong.push("「查不动」那一臂没人接住 ⇒ 编译不过或已被折掉".to_string());
        }
        assert!(
            !body[missing..arm_end].contains("load_all"),
            "Missing 那一臂不该顺手补载插件"
        );
        assert!(
            wrong.is_empty() && body.contains("with_manager(db, |pm| match"),
            "重载入口的接线守卫红（切片没盖住函数体＝假绿）:\n{}",
            wrong.join("\n")
        );
    }
}
