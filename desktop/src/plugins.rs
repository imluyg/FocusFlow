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
    // 首轮只建立 mtime 基线不触发事件：否则每次打开插件管理页都会把全部文件
    // 当作"变更"连发 plugins-reloaded，列表反复重建会吞掉用户正在进行的点击。
    let scan_ok = std::thread::Builder::new()
        .name("plugin-hot-reload-scan".into())
        .spawn(move || {
            let dir = focusflow_core::paths::plugins_dir();
            let mut last: HashMap<String, Option<SystemTime>> = HashMap::new();
            let mut watching = false;
            let mut primed = false;
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
                let mut seen: HashMap<String, Option<SystemTime>> = HashMap::new();
                let mut listed = true;
                if let Ok(entries) = std::fs::read_dir(&dir) {
                    for entry in entries.flatten() {
                        let p = entry.path();
                        if !p.extension().map(|e| e == "lua").unwrap_or(false) {
                            continue;
                        }
                        let Some(name) = p.file_stem().and_then(|s| s.to_str()).map(str::to_string)
                        else {
                            continue;
                        };
                        let Ok(meta) = std::fs::metadata(&p) else {
                            continue;
                        };
                        // 编辑器常有「先截断再写入」：0 字节的 .lua 会被解析成空块而且
                        // 不报错，插件就此变成没有任何函数的空壳。本轮当作「还没写完」，
                        // 沿用上一轮的指纹（否则删除检测会把它误判成文件消失），
                        // 等下一次轮询到非空内容再触发重载。
                        if meta.len() == 0 {
                            if let Some(prev) = last.get(&name) {
                                seen.insert(name, *prev);
                            }
                            continue;
                        }
                        seen.insert(name, meta.modified().ok());
                    }
                } else {
                    listed = false;
                }
                // 目录整体没读到（被占用、或构建脚本正在替换整个目录）时不能拿空的
                // seen 参与比较：否则每个插件都会被判定成「文件已删除」而全部卸载。
                // 同理也不能覆盖基线 —— 那样下一轮成功读取会把所有文件当成新变更，
                // 触发一整轮无谓的重载。保持上一轮基线，等下一轮再说。
                if listed && primed {
                    for (name, mtime) in &seen {
                        if last.get(name) != Some(mtime) {
                            let _ = tx.send(name.clone());
                        }
                    }
                    // 删除也要发事件：只遍历 seen 会漏掉消失的文件，插件就得重启
                    // 才停得下来 —— 而删掉文件表达的意思恰恰是「别再跑它了」。
                    for name in last.keys() {
                        if !seen.contains_key(name) {
                            let _ = tx.send(name.clone());
                        }
                    }
                }
                if listed {
                    primed = true;
                    last = seen;
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

/// 按插件文件名（stem）重载插件（主线程调用）。
/// 文件已消失 → 卸载：删除插件文件表达的就是「别再跑它了」，只 reload 不 remove
/// 的话它会一直跑到下次重启（其 cleanup 也就一直不执行）。
/// 未找到已加载实例 → 补扫一次目录：兜底加载运行期间新放入的插件文件。
fn reload_plugin_by_key(key: &str, db: &Arc<Database>) {
    with_manager(db, |pm| {
        if !focusflow_core::paths::plugins_dir()
            .join(format!("{key}.lua"))
            .exists()
        {
            if pm.unload_by_stem(key) {
                tracing::info!("插件文件已删除，已卸载: {key}");
            }
            return;
        }
        if pm.reload_plugin(key) {
            tracing::info!("插件已热重载: {key}");
        } else {
            tracing::info!("未找到已加载插件 {key}，尝试补载新插件");
            pm.load_all();
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
