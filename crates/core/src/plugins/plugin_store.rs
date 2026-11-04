//! 每插件一份 JSON KV 存储（方案 3.5）。
//!
//! 盘上的形状：`<data_dir>/plugins/state/<stem>/state.json` 一份文件，里面是一个 JSON 对象，
//! 插件给的键就是这个对象的键名。
//!
//! **两条硬约束，写在这里而不是写在文档里**：
//!
//! 1. **键绝不参与路径拼接**。物理路径固定为 `state.json` 这一格，任何键都只进对象内部。
//!    否则插件写一个 `../grants.json` 就能给自己授权 —— 这一条不是审美，是整套权限模型
//!    成立的前提。全模块只有 `state_file` 一处算路径，而它只吃 `stem`。
//! 2. **超限报错，不静默截断**。截断的症状是"插件少了几条数据而没人知道"；宁可这一次写不进去。
//!
//! 与主库分开的理由：主库是**年度分库**，插件状态跨年，不该混进 `focusflow_<年>.db`，
//! 也不参与年度统计与跨年归档（`DATA_TABLES` 因此不扩）。
//!
//! 落盘走「改内存 + 300ms 去抖 + tmp→rename 原子替换」（复用 [`crate::config::atomic_write`]，
//! 与配置、设备别名那两条写路径同一份实现）。去抖意味着"改完的那一刻盘上还没动"，
//! 所以 [`flush`] 与 [`last_error`] 必须存在：调用方要能当场问出结局，
//! 而不是拿一句恒 `Ok` 的返回当"写好了"（配置那一头为这件事已经红过一次）。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::Value;

use crate::config::FocusFlowConfig;
use crate::paths;

/// 单插件容量默认值：1 MB（方案 3.5 的表）。
pub const DEFAULT_MAX_BYTES: usize = 1024 * 1024;
/// 单插件键数默认值：2000。
pub const DEFAULT_MAX_KEYS: usize = 2000;
/// 连续写合并成一轮的窗口。与 config-saver 用同一个数：两处各定一把的话，
/// "什么时候落盘"就变成两件要记的事。
const SAVE_DEBOUNCE: Duration = Duration::from_millis(300);
/// `state.json` 的文件名：路径里唯一由本模块决定的一格，插件给的键永远进不了路径。
const STATE_FILE: &str = "state.json";

/// 一个插件的配额。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_bytes: usize,
    pub max_keys: usize,
}

impl Limits {
    /// 从 `[plugins] store_max_bytes` / `store_max_keys` 读；写坏的值折回默认并出声。
    ///
    /// 每次改动都现读一遍（两条 `get_or`），不在进程里缓存：这样"改了 config 重启才对"
    /// 这一族问题不会出现在配额上，代价是两次字符串查表。
    pub fn from_config(config: &FocusFlowConfig) -> Self {
        fn one(config: &FocusFlowConfig, key: &str, def: usize) -> usize {
            let raw = config.get_or("plugins", key, "");
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return def;
            }
            match trimmed.parse::<i64>() {
                // 注意别写成 `n <= usize::MAX as i64`：那个 cast 在 64 位上是 -1，
                // 每一个正数都会被判成越界（同一个坑在本仓一天里踩过两次）。
                Ok(n) if n > 0 => match usize::try_from(n) {
                    Ok(v) => v,
                    Err(_) => {
                        tracing::warn!(
                            "[plugins] {key} = {n} 大到这台机器装不下，按默认值 {def} 走"
                        );
                        def
                    }
                },
                Ok(n) => {
                    tracing::warn!(
                        "[plugins] {key} = {n} 不是一个正整数，按默认值 {def} 走（超限报错那一句\
                         也跟着回到默认）"
                    );
                    def
                }
                Err(_) => {
                    tracing::warn!("[plugins] {key} =「{raw}」读不出数字，按默认值 {def} 走");
                    def
                }
            }
        }
        Limits {
            max_bytes: one(config, "store_max_bytes", DEFAULT_MAX_BYTES),
            max_keys: one(config, "store_max_keys", DEFAULT_MAX_KEYS),
        }
    }
}

/// 盘上那一份文件的位置：`data/plugins/state/<stem>/state.json`（**不建目录**）。
///
/// 名字越界的 `stem` 交 `None`（不落盘，由调用方说原因）：[`paths::plugin_state_dir`] 对
/// 越界名字交回的是**上一格**，直接 join 下去就会把某个插件的数据写进公共那格。
/// 合法性用 [`paths::is_safe_state_stem`] 那一把尺子判 —— 两处各写一遍的话，store 这关
/// 放宽一寸，`../grants` 就又变成"能碰别人的授权文件"。
///
/// 读的一侧走这里（不 mkdir）：开着插件页就给每个从不存东西的插件造一格空目录，
/// 是把只读查询做成了副作用。目录在**改动**这一刻才建（见 [`ensure_state_dir`]）。
fn state_file(stem: &str) -> Option<PathBuf> {
    if !paths::is_safe_state_stem(stem) {
        return None;
    }
    Some(paths::plugin_state_path(stem).join(STATE_FILE))
}

fn unsafe_stem_reason(stem: &str) -> String {
    format!(
        "插件名的这一格越界了（{stem}）：存储只认插件文件名，不许带路径分隔或 .. —— 本次没有落盘"
    )
}

/// 这一份 store 与盘的关系。
#[derive(Debug, Clone)]
enum Load {
    /// 还没碰过盘
    Untouched,
    /// 读过了（文件本来就不在 = 这个插件没写过东西，也是合法的空状态）
    Loaded,
    /// 读不动、或读出来不是合法 JSON：**这一格拒绝被写**，直到文件修好或进程重启。
    /// 折成"空的"再落盘，等于把用户插件的全部数据抹掉。
    Failed(String),
}

/// 一个插件的内存态。
struct Cell {
    load: Load,
    data: BTreeMap<String, Value>,
    /// 内存与盘不一致（改过还没落，或落了失败）
    dirty: bool,
    /// 最近一次落盘失败的原因（插件页要说这一句）
    last_error: Option<String>,
}

impl Cell {
    fn new() -> Self {
        Cell {
            load: Load::Untouched,
            data: BTreeMap::new(),
            dirty: false,
            last_error: None,
        }
    }
}

/// 进程内的登记表：按**状态文件的完整路径**分格。
///
/// 键用路径而不是 stem，是因为测试（以及"更改数据文件夹"之后）同一个 stem 会指向不同目录；
/// 按 stem 分格会让上一份数据串到下一个程序目录里。
type Registry = Mutex<HashMap<PathBuf, Arc<Mutex<Cell>>>>;

fn registry() -> &'static Registry {
    static R: OnceLock<Registry> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cell_for(path: &Path) -> Arc<Mutex<Cell>> {
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    reg.entry(path.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(Cell::new())))
        .clone()
}

/// 丢掉这一格的内存态（**只给用例**：模拟"进程重启后从盘上读回"）。
///
/// 生产不需要这个入口 —— 进程收尾时内存本来就没了。但用例都在同一个进程里跑，
/// 而登记表是唯一持有那份内存的地方：不放掉它，`get` 拿回的还是刚写进去的那一份内存，
/// "重启后还在"这句话就永远碰不到盘。放掉之后下一次读走的是 `ensure_loaded` 的
/// `Untouched` 那一支 —— 与真启动的第一次读写同一条路。
#[cfg(test)]
pub(crate) fn forget_for_test(stem: &str) {
    let Some(path) = state_file(stem) else {
        return;
    };
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&path);
}

/// 保证内存那一份来自盘（只在第一次碰时读一次；失败就记着，既不重试也不假装是空的）。
fn ensure_loaded(cell: &mut Cell, path: &Path) -> Result<(), String> {
    match &cell.load {
        Load::Loaded => return Ok(()),
        Load::Failed(why) => return Err(why.clone()),
        Load::Untouched => {}
    }
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<BTreeMap<String, Value>>(&bytes) {
            Ok(data) => {
                cell.data = data;
                cell.load = Load::Loaded;
                Ok(())
            }
            Err(e) => {
                let why = format!(
                    "存储文件读不出来（{}）：{e} —— 它可能只写了一半，或被人手工改坏；\
                     本进程不再往里写，以免覆盖还能看的旧内容",
                    path.display()
                );
                tracing::error!("{why}");
                cell.load = Load::Failed(why.clone());
                Err(why)
            }
        },
        Err(e) if path.exists() => {
            let why = format!(
                "存储文件打不开（{}）：{e} —— 这次没有读数，也没有把它当成空的",
                path.display()
            );
            tracing::error!("{why}");
            cell.load = Load::Failed(why.clone());
            Err(why)
        }
        Err(_) => {
            cell.load = Load::Loaded;
            Ok(())
        }
    }
}

/// 序列化这一格内存态（`BTreeMap` ⇒ 键有序，同一份内容每轮写出的字节相同）。
fn dump(data: &BTreeMap<String, Value>) -> Result<String, String> {
    serde_json::to_string(data).map_err(|e| format!("存储序列化失败：{e}"))
}

static SAVE_TX: OnceLock<mpsc::Sender<PathBuf>> = OnceLock::new();

/// 获取（必要时启动）去抖落盘线程的信号发送端。
fn saver_tx() -> &'static mpsc::Sender<PathBuf> {
    SAVE_TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<PathBuf>();
        let spawn = std::thread::Builder::new()
            .name("plugin-store-saver".into())
            .spawn(move || {
                // 发送端被丢掉（进程收尾）时 `recv` 交 Err，这一轮循环自然结束
                while let Ok(first) = rx.recv() {
                    // 300ms 窗口里再来的都并进这一轮；同一份文件只重写一次
                    let mut batch: BTreeSet<PathBuf> = BTreeSet::new();
                    batch.insert(first);
                    while let Ok(more) = rx.recv_timeout(SAVE_DEBOUNCE) {
                        batch.insert(more);
                    }
                    for path in batch {
                        if let Err(e) = flush_path(&path) {
                            tracing::error!("插件存储落盘失败（{}）：{e}", path.display());
                        }
                    }
                }
            });
        if let Err(e) = spawn {
            tracing::error!(
                "插件存储的保存线程起不来（{e}）：本进程的插件数据不会自动落盘，\
                 flush 与插件页会各自问出这句话"
            );
        }
        tx
    })
}

/// 改完之后安排一次去抖落盘。
fn schedule(path: &Path) {
    if let Err(e) = saver_tx().send(path.to_path_buf()) {
        // 线程收不到 ⇒ 这次改动只会停在内存里。留在 last_error 上，让 flush / 插件页问得出。
        let why = format!("插件存储的保存线程收不到信号（{e}）：本次改动不会落盘，重启程序可恢复");
        tracing::error!("{why}");
        if let Ok(mut cell) = cell_for(path).lock() {
            cell.last_error = Some(why);
        }
    }
}

/// 真要往里存东西了，才把那一格建出来。
///
/// 放在**改动**这一侧而不是落盘那一侧：去抖线程晚 300ms 才动手，如果那之前数据目录被换走
/// 或被清理掉，落盘时再 `create_dir_all` 就等于把已经不存在的那一格重新造出来
/// （门禁的临时目录腿就是这么逮到一次的：测试目录删掉之后又被写回来了）。
/// 现在让它自然地失败 —— 待写标记保住、原因记进 `last_error`，与配置那一头同一口径。
fn ensure_state_dir(stem: &str) {
    let _ = paths::plugin_state_dir(stem);
}

/// 把内存那份原子写到盘上，并更新状态。`Err` 的原因已经同时记进 `last_error`。
fn write_out(cell: &mut Cell, path: &Path) -> Result<(), String> {
    let text = dump(&cell.data)?;
    match crate::config::atomic_write(path, &text) {
        Ok(()) => {
            cell.dirty = false;
            cell.last_error = None;
            Ok(())
        }
        Err(e) => {
            let why = format!("写盘失败（{}）：{e:#}", path.display());
            cell.last_error = Some(why.clone());
            // dirty 保持 true：下一次 flush（或退出前那一轮）还会再试
            Err(why)
        }
    }
}

/// 真正落一次盘：脏才写。`Ok(true)` = 重写了文件，`Ok(false)` = 没有待写的改动。
fn flush_path(path: &Path) -> Result<bool, String> {
    let cell = cell_for(path);
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    // 盘上那份还没读出来过 ⇒ 内存可能根本是空的，此刻写下去就是覆盖。
    if let Load::Failed(why) = &guard.load {
        return Err(why.clone());
    }
    if !guard.dirty {
        return Ok(false);
    }
    write_out(&mut guard, path)?;
    Ok(true)
}

/// 读一个键。`Ok(None)` 只有一种含义：**这一份读全了，而这个键没写过**。
pub fn get(stem: &str, key: &str) -> Result<Option<Value>, String> {
    let path = state_file(stem).ok_or_else(|| unsafe_stem_reason(stem))?;
    let cell = cell_for(&path);
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    ensure_loaded(&mut guard, &path)?;
    Ok(guard.data.get(key).cloned())
}

/// 写一个键。超限时报错并保持原样（不截断、不动已有的键）。
pub fn set(stem: &str, key: &str, value: Value, limits: &Limits) -> Result<(), String> {
    let path = state_file(stem).ok_or_else(|| unsafe_stem_reason(stem))?;
    if key.is_empty() {
        return Err("存储的键不能是空串".to_string());
    }
    let cell = cell_for(&path);
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    ensure_loaded(&mut guard, &path)?;
    // 先在副本上试：任何一项超限都交 Err，内存里那一份一个字都不动
    let mut trial = guard.data.clone();
    trial.insert(key.to_string(), value);
    let text = dump(&trial)?;
    if trial.len() > limits.max_keys {
        return Err(format!(
            "键数超限：这一份写出去会有 {} 个键，上限 {} 个（config 的 [plugins] store_max_keys）。 \
             本次没有写入，也没有截掉多出来的 —— 记录多了请换地方存",
            trial.len(),
            limits.max_keys
        ));
    }
    if text.len() > limits.max_bytes {
        return Err(format!(
            "容量超限：这一份写出去是 {} 字节，上限 {} 字节（config 的 [plugins] store_max_bytes）。 \
             本次没有写入，也没有截断 —— state.json 是整文件重写，不该用来堆几千条记录",
            text.len(),
            limits.max_bytes
        ));
    }
    guard.data = trial;
    ensure_state_dir(stem);
    guard.dirty = true;
    schedule(&path);
    Ok(())
}

/// 删一个键；返回"是不是真删掉了一个"（键不存在 = `false`，不是错误）。
pub fn delete(stem: &str, key: &str) -> Result<bool, String> {
    let path = state_file(stem).ok_or_else(|| unsafe_stem_reason(stem))?;
    let cell = cell_for(&path);
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    ensure_loaded(&mut guard, &path)?;
    let removed = guard.data.remove(key).is_some();
    if removed {
        ensure_state_dir(stem);
        guard.dirty = true;
        schedule(&path);
    }
    Ok(removed)
}

/// 列全部键值（键有序）。
pub fn list(stem: &str) -> Result<Vec<(String, Value)>, String> {
    let path = state_file(stem).ok_or_else(|| unsafe_stem_reason(stem))?;
    let cell = cell_for(&path);
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    ensure_loaded(&mut guard, &path)?;
    Ok(guard
        .data
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect())
}

/// 清空这一份；返回删掉了几个键。
pub fn clear(stem: &str) -> Result<usize, String> {
    let path = state_file(stem).ok_or_else(|| unsafe_stem_reason(stem))?;
    let cell = cell_for(&path);
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    ensure_loaded(&mut guard, &path)?;
    let n = guard.data.len();
    guard.data.clear();
    if n > 0 {
        ensure_state_dir(stem);
        guard.dirty = true;
        schedule(&path);
    }
    Ok(n)
}

/// 当场把这一格写到盘上，并交出结局（见模块顶部那句"结局要当场问得出"）。
pub fn flush(stem: &str) -> Result<bool, String> {
    let path = state_file(stem).ok_or_else(|| unsafe_stem_reason(stem))?;
    flush_path(&path)
}

/// 退出前把还有改动的都落一次；交回没写成的那些（调用方决定怎么说）。
pub fn flush_all() -> Vec<String> {
    let pending: Vec<PathBuf> = {
        let reg = registry().lock().unwrap_or_else(|e| e.into_inner());
        reg.iter()
            .filter(|(_, cell)| cell.lock().map(|c| c.dirty).unwrap_or(false))
            .map(|(path, _)| path.clone())
            .collect()
    };
    let mut failed = Vec::new();
    for path in pending {
        if let Err(e) = flush_path(&path) {
            failed.push(format!("{}：{e}", path.display()));
        }
    }
    failed
}

/// 最近一次落盘失败的原因（插件页那一格用）；写成功之后就清了。
pub fn last_error(stem: &str) -> Option<String> {
    let path = state_file(stem)?;
    cell_for(&path)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .last_error
        .clone()
}

/// 这一份要写出去是多少字节（插件页那一格「占用」）。
///
/// 两种情况交 `None`：读不出来的那格，以及**这个插件从来没写过东西**（文件不在、内存也空）。
/// 后者必须没有读数 —— 空清单序列化出来是 `{}` 两个字节，照实报就成了"它占了 2 B"，
/// 而屏上那一格的本意是"你有没有往里存过东西"（第一版就是这么画的，屏证里三个插件
/// 有两个显示「占用 2 B」，而它们一个字都没存过）。
pub fn used_bytes(stem: &str) -> Option<usize> {
    let path = state_file(stem)?;
    let cell = cell_for(&path);
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    if ensure_loaded(&mut guard, &path).is_err() {
        return None;
    }
    if guard.data.is_empty() && !guard.dirty {
        return None;
    }
    Some(dump(&guard.data).unwrap_or_default().len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            max_bytes: DEFAULT_MAX_BYTES,
            max_keys: DEFAULT_MAX_KEYS,
        }
    }

    /// 值走一趟 JSON，形状要不变。
    fn put(stem: &str, key: &str, v: Value) -> Result<(), String> {
        set(stem, key, v, &limits())
    }

    fn state_of(stem: &str) -> PathBuf {
        paths::plugin_state_path(stem).join(STATE_FILE)
    }

    /// 造一份"盘上已经有这个文件"的夹具：目录自己建，不许借被测函数那条路的副作用来
    /// 把前提铺上（本仓抓过多次"夹具替产品把前提造好了"）。
    fn write_state(stem: &str, text: &str) -> PathBuf {
        let file = state_of(stem);
        std::fs::create_dir_all(file.parent().expect("状态文件总该有一格父目录")).unwrap();
        std::fs::write(&file, text).unwrap();
        file
    }

    /// 验收第 1 条要的那条真腿：**丢掉内存再从文件读回**。
    ///
    /// 相邻那条往返用例读回走的是同一份内存（末尾才 flush），所以它守不住"重开之后还在"
    /// 这句话。这一条先 flush，再把登记表里那一格摘掉，于是下一次 `get` 只能从盘上读 ——
    /// 摘掉的那一步就是"进程没了"那件事在本进程里唯一能做的等价物。
    #[test]
    fn a_value_survives_the_memory_being_dropped() {
        let _lock = paths::test_app_dir_lock();
        let _app = paths::test_app_dir("store_survives_restart");
        let stem = "restart_case";
        put(stem, "last_tab", Value::from("week")).unwrap();
        assert!(flush(stem).unwrap(), "先要真的落到盘上");
        forget_for_test(stem);
        assert_eq!(
            get(stem, "last_tab").unwrap(),
            Some(Value::from("week")),
            "内存丢掉之后，读回的必须是盘上那一份"
        );
    }

    /// 读的一侧不许建目录：开着插件页就把每个从不存东西的插件在数据目录里造出一格空目录，
    /// 是把一次只读查询做成了副作用（在真程序里现量到 5 个空目录、0 个文件）。
    #[test]
    fn reading_the_store_does_not_create_the_directory() {
        let _lock = paths::test_app_dir_lock();
        let _app = paths::test_app_dir("store_read_no_mkdir");
        let stem = "quiet_reader";
        assert_eq!(get(stem, "k").unwrap(), None);
        assert!(list(stem).unwrap().is_empty());
        assert_eq!(used_bytes(stem), None);
        assert_eq!(last_error(stem), None);
        assert!(
            !paths::plugin_state_path(stem).exists(),
            "只是读了一遍，却在数据目录里造出了一格目录"
        );
        // 真要写的时候，那一格才该被建出来
        put(stem, "k", Value::from(1)).unwrap();
        assert!(flush(stem).unwrap(), "第一次写也要落得下去");
        assert_eq!(
            std::fs::read_to_string(state_of(stem)).unwrap(),
            "{\"k\":1}",
            "落盘时建的那一格要装得下这份文件"
        );
    }

    /// 键**只当键名**：一个 `../grants.json` 写进 store，既不能碰到别人的文件，
    /// 也不该在状态目录里长出第二格。这条堵的是"插件给自己授权"那条路。
    #[test]
    fn a_key_never_becomes_a_path() {
        let _lock = paths::test_app_dir_lock();
        let _app = paths::test_app_dir("store_key_is_not_a_path");
        let stem = "tool";
        for key in ["../grants.json", "a/b", "c\\d", ".."] {
            put(stem, key, Value::String("值".into())).unwrap_or_else(|e| panic!("{key}: {e}"));
        }
        assert!(
            put(stem, "", Value::from(1)).is_err(),
            "空键要报错，不该悄悄写进去"
        );
        assert!(flush(stem).unwrap(), "改动必须真的落盘");
        // 盘上只有一份文件，就是那个 state.json
        let files: Vec<String> = std::fs::read_dir(paths::plugin_state_path(stem))
            .expect("状态目录该读得动")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            files,
            vec![STATE_FILE.to_string()],
            "多出来的文件都是键名变的路径"
        );
        // 那几个"越界"的键仍在对象里面，一个都没少
        let listed = list(stem).unwrap();
        assert_eq!(
            listed.len(),
            4,
            "四个越界键都该在 state.json 里: {listed:?}"
        );
        assert!(
            !paths::plugins_state_dir().join("grants.json").exists(),
            "../grants.json 变成了别人的文件名"
        );
    }

    /// 超限那一句必须报错，而且**已有的一份没有被截掉**：静默截断的症状是
    /// 插件少了几条数据而没人知道。
    #[test]
    fn over_quota_is_an_error_not_a_truncation() {
        let _lock = paths::test_app_dir_lock();
        let _app = paths::test_app_dir("store_quota");
        let stem = "quota";
        let tight = Limits {
            max_bytes: DEFAULT_MAX_BYTES,
            max_keys: 3,
        };
        for i in 0..3 {
            set(stem, &format!("k{i}"), Value::from(i), &tight).expect("前三个键都该写进去");
        }
        let e = set(stem, "k3", Value::from(99), &tight).expect_err("第四个键必须被挡下");
        assert!(e.contains("键数超限"), "报的得是配额这句话: {e}");
        assert_eq!(list(stem).unwrap().len(), 3, "挡下的同时不能动已有那三份");

        // 字节上限：超长值被挡在外面，短的那些一个字不少
        let small = Limits {
            max_bytes: 120,
            max_keys: DEFAULT_MAX_KEYS,
        };
        let e = set(stem, "big", Value::String("x".repeat(500)), &small)
            .expect_err("超字节的值必须被挡下");
        assert!(e.contains("容量超限"), "报的得是容量这句话: {e}");
        assert_eq!(list(stem).unwrap().len(), 3, "超容量那一次什么都没写");
        // 默认配额（1 MB / 2000 键）也得能报出来 —— 数值本身是方案 3.5 那张表的口径
        assert_eq!((DEFAULT_MAX_BYTES, DEFAULT_MAX_KEYS), (1024 * 1024, 2000));
    }

    /// 配额那两个数要真的从 config 读，而不是写死在代码里（方案 3.5 的表写着「config 可调」）。
    #[test]
    fn the_quota_comes_from_config_when_said_there() {
        let _lock = paths::test_app_dir_lock();
        let app = paths::test_app_dir("store_limits_from_config");
        std::fs::write(
            app.path().join("config.ini"),
            "[plugins]\nstore_max_keys = 42\nstore_max_bytes = 12345\n",
        )
        .unwrap();
        let cfg = FocusFlowConfig::load(app.path().join("config.ini")).unwrap();
        assert_eq!(
            Limits::from_config(&cfg),
            Limits {
                max_bytes: 12345,
                max_keys: 42
            }
        );
        // 写坏的值（0 / 非数字）折回默认，并说一句
        std::fs::write(
            app.path().join("config.ini"),
            "[plugins]\nstore_max_keys = 0\nstore_max_bytes = 很多\n",
        )
        .unwrap();
        let cfg2 = FocusFlowConfig::load(app.path().join("config.ini")).unwrap();
        let (limits2, logs) = crate::logger::capture_logs(|| Limits::from_config(&cfg2));
        assert_eq!(limits2, limits(), "读不出来的那两个数都该折回默认");
        assert!(
            logs.iter().any(|l| l.contains("store_max_keys"))
                && logs.iter().any(|l| l.contains("store_max_bytes")),
            "配额读不出来却没说一句: {logs:?}"
        );
    }

    /// 名字越界的 `stem` 不落盘，而且说的就是那一句原因。
    #[test]
    fn an_overreaching_stem_is_not_written_anywhere() {
        let _lock = paths::test_app_dir_lock();
        let _app = paths::test_app_dir("store_unsafe_stem");
        for bad in ["../grants", "a/b", ""] {
            let e = put(bad, "k", Value::from(1)).expect_err("越界的插件名不该被接受");
            assert!(e.contains("越界"), "要说清是哪一格的规矩: {e}");
        }
        let _ = flush("../grants").expect_err("flush 同一把尺子");
        // 公共那一格不能被当成某个插件的状态文件
        assert!(!paths::plugins_state_dir().join(STATE_FILE).exists());
    }

    /// 读不出来的那份**不许**被折成"空的"再写回去。
    ///
    /// 与 `manager.rs` 那一族同形（"把库读不出来印成没有"），这里还狠一层：
    /// 折成空的之后下一次落盘就把原件抹了，用户插件的数据从此没了。
    #[test]
    fn a_corrupt_state_file_is_reported_and_left_alone() {
        let _lock = paths::test_app_dir_lock();
        let _app = paths::test_app_dir("store_corrupt");
        let junk = "{ 这不是 JSON";
        let file = write_state("broken", junk);
        let e = get("broken", "k").expect_err("读不出来要交 Err，不是 None");
        assert!(e.contains("读不出来"), "得说是什么原因: {e}");
        // 之后再改、之后再 flush，也不许盖掉原件
        let _ = put("broken", "k", Value::from(1));
        let _ = flush("broken");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            junk,
            "读不出来的那份被覆盖了"
        );
        // 「占用」这一格也不能谎报成 0（读不出与没有是两件事）
        assert_eq!(used_bytes("broken"), None, "读不出来不该报成零字节");
    }

    /// 落盘的结局当场问得出，失败时说的是失败。
    ///
    /// 三条臂：`Ok(true)` 钉"真的有写"，`Ok(false)` 钉"没改动就别谎报又写了一次"，
    /// 障碍那条臂钉"失败要留在问得出的地方"（`last_error`），而且内存那份还能继续改、
    /// 下次 flush 还会再试。
    #[test]
    fn the_flush_outcome_is_askable_and_a_failed_write_is_kept() {
        let _lock = paths::test_app_dir_lock();
        let _app = paths::test_app_dir("store_flush");
        let stem = "askable";
        put(stem, "k", Value::from(7)).unwrap();
        assert!(flush(stem).unwrap(), "有改动就该真的写一次");
        assert_eq!(
            std::fs::read_to_string(state_of(stem)).unwrap(),
            "{\"k\":7}",
            "盘上那份就是刚写的"
        );
        assert!(!flush(stem).unwrap(), "没有改动时不该谎报\"又写了一次\"");
        assert!(last_error(stem).is_none(), "写成功了这句话就该清掉");

        // 障碍臂：把目标位置换成同名目录，rename 必然失败（本仓这一族的常用造法）
        std::fs::remove_file(state_of(stem)).unwrap();
        std::fs::create_dir(state_of(stem)).unwrap();
        put(stem, "k2", Value::from(1)).unwrap();
        let e = flush(stem).expect_err("写得动才见鬼");
        assert!(e.contains("写盘失败"), "要说清是没写成: {e}");
        let why = last_error(stem).expect("失败原因必须留在问得出的地方");
        assert!(why.contains("写盘失败"), "{why}");
        assert!(
            put(stem, "k3", Value::from(2)).is_ok(),
            "内存那一份还要能继续改（等下次 flush 再试）"
        );
        assert_eq!(get(stem, "k3").unwrap(), Some(Value::from(2)));
    }

    /// 连着写几次，盘上必须是**最后那一版**；后台那一轮自己也要能把改动落出去。
    ///
    /// 这条盯的是"合并窗口不许丢改动"：窗口里只处理第一格、或在内容更新之前就摘掉待写标记，
    /// 症状都是插件的最后一个值没进盘。
    /// ⚠ 它**量不到"合并成了几次写"**：进程里没有写次数的读数，把去抖窗口调成 0 这一样会绿
    ///     （变异探针验过）。所以这里只钉内容与"最终会落盘"，不声称"一轮只重写一次"。
    #[test]
    fn debounced_writes_carry_the_last_value() {
        let _lock = paths::test_app_dir_lock();
        let _app = paths::test_app_dir("store_debounce");
        let stem = "pomo";
        for n in [1, 2, 3] {
            put(stem, "n", Value::from(n)).unwrap();
        }
        // 后台那一轮与这里的 flush 谁先到都行：内容都取自同一份内存
        let _ = flush(stem);
        let on_disk = std::fs::read_to_string(state_of(stem)).unwrap();
        assert!(
            on_disk.contains("\"n\":3"),
            "去抖把最新的值弄丢了: {on_disk}"
        );
        assert_eq!(get(stem, "n").unwrap(), Some(Value::from(3)));

        // 后台那一轮自己也要能把盘写出来（不等调用方来问）：改一次、等过窗口、盘上要跟上
        put(stem, "later", Value::from("稍后")).unwrap();
        let until = std::time::Instant::now() + Duration::from_millis(2_000);
        let mut seen = String::new();
        while std::time::Instant::now() < until {
            seen = std::fs::read_to_string(state_of(stem)).unwrap_or_default();
            if seen.contains("later") {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            seen.contains("later"),
            "去抖线程没把这一轮的改动落盘（盘上只有: {seen}）"
        );
    }

    /// 路径这一头钉**形状**：整个模块只许有了一次 `join`，而它的实参是那个固定文件名常量。
    ///
    /// 上面那条行为用例盯得住"今天没有键变成路径"，盯不住"下一个人顺手加了一行
    /// `dir.join(key)`" —— 那种改动在这一族判据下照样全绿（本仓那条老教训：
    /// 守卫要钉调用形态，不是钉函数名在不在）。
    #[test]
    fn only_the_fixed_file_name_ever_enters_a_path() {
        let src = include_str!("plugin_store.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let joins: Vec<&str> = prod.match_indices(".join(").map(|(_, s)| s).collect();
        assert_eq!(
            joins.len(),
            1,
            "路径拼接从 1 处变成 {} 处了 —— 键参与拼路径的那条硬约束就在此处破",
            joins.len()
        );
        let flat: String = prod.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            flat.contains(".join(STATE_FILE)"),
            "唯一那一次拼接必须是那个固定文件名，而不是某个变量"
        );
        // 名字合不合法也只许问那一把共用的尺子：在这里另写一遍 `!stem.contains('/')`，
        // `paths::plugin_state_dir` 那一关放宽一寸也没人红（症状＝越界的名字又开始落盘了）
        assert!(
            flat.contains("if!paths::is_safe_state_stem(stem){returnNone;}"),
            "存储不再问 `paths::is_safe_state_stem` 这一把尺子了，而是自己判了一遍"
        );
        for forbidden in ["join(key", "join(&key", "push(key", "join(k.", "join(stem)"] {
            assert!(
                !flat.contains(forbidden),
                "存储里出现了把{forbidden}拼进路径的写法：插件给的键只当 JSON 的键名"
            );
        }
    }

    /// 落盘那一步**永远不许造目录**。
    ///
    /// 这条是门禁的临时目录腿逮出来的：待写的那一格还没落盘，测试目录已经被 Drop 删掉，
    /// 300ms 之后去抖线程把 `data/plugins/state/<stem>/state.json` 连目录一起写了回来
    /// （`%TEMP%` 里因此多出 5 格）。生产里同一件事的形状是"更改数据文件夹之后，
    /// 旧位置又被写回一份"。建目录归**改动**那一侧（[`ensure_state_dir`]），
    /// 落盘只写：目录没了就交失败、待写保住、原因留在 [`last_error`]。
    #[test]
    fn a_pending_write_never_recreates_a_deleted_directory() {
        let _lock = paths::test_app_dir_lock();
        let _app = paths::test_app_dir("store_no_resurrect");
        let stem = "gone";
        put(stem, "k", Value::from(1)).unwrap();
        assert!(flush(stem).unwrap(), "第一次正常落盘该写得出去");
        // 还没写第二版，整个数据目录被搬走 / 清掉
        std::fs::remove_dir_all(paths::data_dir()).unwrap();
        put(stem, "k2", Value::from(2)).unwrap();
        std::fs::remove_dir_all(paths::data_dir()).unwrap();
        let e = flush(stem).expect_err("目录已经不在了，这一句不能说成功");
        assert!(e.contains("写盘失败"), "得说是没写成: {e}");
        assert!(
            !paths::plugin_state_path(stem).exists(),
            "落盘那一步把删掉的目录又造回来了 —— 旧数据位置会因此复活"
        );
        assert!(
            last_error(stem).is_some(),
            "失败要留在问得出的地方（插件页那一格读的就是它）"
        );
    }

    /// 退出前那一句 `flush_all` 要真的把待写的都落下去，而且把落不成的那一格交回调用方。
    ///
    /// 只验自己那两格，不验"返回清单为空"：登记表是进程级的，同一轮里别的用例也可能
    /// 带着待写的格子（各自别的临时目录）。断言整体成功会红在隔壁那条用例上，
    /// 而这条真正要钉的是"我这个格子的改动有没有出去"。
    #[test]
    fn flush_all_writes_every_pending_cell_and_names_the_ones_that_failed() {
        let _lock = paths::test_app_dir_lock();
        let _app = paths::test_app_dir("store_flush_all");
        put("pending_a", "k", Value::from(1)).unwrap();
        put("pending_b", "k", Value::from(2)).unwrap();
        // 第三格先正常落一次，再把目标那一格换成同名目录：之后那次 rename 必然失败
        // （先手就把它做成目录的话，`ensure_loaded` 会把这一格认成"读不出来"而拒绝再写）
        put("blocked_c", "k", Value::from(3)).unwrap();
        assert!(flush("blocked_c").unwrap());
        std::fs::remove_file(state_of("blocked_c")).unwrap();
        std::fs::create_dir(state_of("blocked_c")).unwrap();
        put("blocked_c", "k", Value::from(4)).unwrap();

        let failed = flush_all();
        assert_eq!(
            std::fs::read_to_string(state_of("pending_a")).unwrap(),
            "{\"k\":1}",
            "退出前那一句没能把 a 落盘"
        );
        assert_eq!(
            std::fs::read_to_string(state_of("pending_b")).unwrap(),
            "{\"k\":2}",
            "退出前那一句没能把 b 落盘"
        );
        assert!(
            failed.iter().any(|f| f.contains("blocked_c")),
            "写不成的那一格必须被点名交回调用方（否则退出那句就成了假成功）: {failed:?}"
        );
        // 已经落过的两格再问一次不报错，也不该被重复计成失败
        // （格子名要挑得不与临时目录撞：`ff_store_flush_all_*` 里就含着 "flush_a" 这一串，
        //   第一版拿它做过子串判据，红得莫名其妙）
        let second = flush_all();
        assert!(
            !second
                .iter()
                .any(|f| f.contains("pending_a") || f.contains("pending_b")),
            "没有待写改动的格子不该再被报一次: {second:?}"
        );
    }

    /// 值的形状要走得动：字符串 / 数 / 布尔 / 表（数组与对象两种），以及删除与清空。
    #[test]
    fn values_survive_the_round_trip_and_clear_says_how_many() {
        let _lock = paths::test_app_dir_lock();
        let _app = paths::test_app_dir("store_shapes");
        let stem = "shapes";
        // 一个字都没存过 ⇒ 没有读数。空清单序列化是 `{}`（2 字节），照实报就成了
        // "这个插件占了 2 B"，而屏上那一格问的是"它存过东西没有"（屏证里抓到过这一条）。
        assert_eq!(used_bytes(stem), None, "没写过东西不该报一个占用");
        put(stem, "s", Value::String("文".into())).unwrap();
        put(stem, "n", Value::from(1.5)).unwrap();
        put(stem, "b", Value::Bool(true)).unwrap();
        put(
            stem,
            "arr",
            serde_json::json!(["一", 2, false, { "内": "层" }]),
        )
        .unwrap();
        assert_eq!(
            get(stem, "arr").unwrap(),
            Some(serde_json::json!(["一", 2, false, { "内": "层" }]))
        );
        assert_eq!(list(stem).unwrap().len(), 4);
        assert!(flush(stem).unwrap());
        assert!(
            used_bytes(stem).unwrap() > 2,
            "存过东西就要报得出字节数，不许停在那两个字节上"
        );
        assert!(std::fs::read_to_string(state_of(stem))
            .unwrap()
            .contains("\"内\""));
        assert!(delete(stem, "s").unwrap(), "删掉一个存在的键要回真");
        assert!(!delete(stem, "s").unwrap(), "再删一次就没有可删的了");
        assert_eq!(clear(stem).unwrap(), 3, "清空要说删了几个");
        assert!(list(stem).unwrap().is_empty());
        assert!(flush(stem).unwrap());
        assert_eq!(
            std::fs::read_to_string(state_of(stem)).unwrap(),
            "{}",
            "清空也要落到盘上，不能只在内存里"
        );
        assert_eq!(used_bytes(stem), None, "清空之后不该再报一个占用");
    }
}
