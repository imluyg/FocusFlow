//! 设备别名：把自动解析出来的设备名（`HID 鼠标 · 24AE/1464`）换成用户看得懂的名字。
//!
//! 存储：`data/device_aliases.json`，形如 `{ "HID#VID_24AE&PID_1464&MI_00#7&...": "新鼠标" }`。
//! 纯文本、可直接手改；解析侧按文件 mtime 变化自动重载（无需重启）。
//!
//! 归属是**用户数据**而不是配置：它住在 `data/` 下、随数据目录整体迁移，`--reset` 与
//! 按日期清理都不碰它；每次备份还会顺带快照一份到 `backup/focusflow_aliases_*.json`
//! （内容没变不重复留，见 `db::maintenance::backup_alias_snapshot`）。理由是重建代价
//! 不对称 —— `config.ini` 丢了能靠默认值跑回来，别名丢了找不回来（库里只有自动名）。
//!
//! 匹配分两层（`AliasTable::resolve`）：
//! 1. **精确匹配** device_key（Raw Input 设备实例路径）
//! 2. 未命中时按 **型号 + 接口** 回退 —— 同型号设备换 USB 口、接收器重插后实例路径会变，
//!    但型号与接口不变，别名仍然生效。复合设备（一个接收器的键盘面 `&MI_00` 与
//!    鼠标面 `&MI_01`）按接口分开，不共用同一个回退键。
//!    同一回退键下出现多个**不同**别名时视为歧义，该键不再参与回退（避免张冠李戴）。
//!
//! 注意：别名只影响展示，不改动统计口径，也不回写 `devices` 登记表。

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

use crate::db::queries::parse_vid_pid;
use crate::paths;

/// 别名长度上限（字符数）：超长会在 UI 里撑破表格。
pub const MAX_ALIAS_CHARS: usize = 24;

/// 别名表（精确 + 型号两层索引，查找 O(1)）。
#[derive(Default, Clone)]
pub struct AliasTable {
    exact: HashMap<String, String>,
    /// VID/PID -> 别名；None 表示该型号有多个不同别名（歧义，不参与回退）
    model: HashMap<String, Option<String>>,
}

impl AliasTable {
    fn from_map(map: &BTreeMap<String, String>) -> Self {
        let mut exact: HashMap<String, String> = HashMap::new();
        let mut model: HashMap<String, Option<String>> = HashMap::new();
        for (key, alias) in map {
            // 长度闸必须在读侧：文件按模块头注释「可直接手改」，只在 `set()` 里钳
            // 挡不住手写进来的超长值 —— 那样 MAX_ALIAS_CHARS 那条"撑破表格"的防线
            // 从下一次读起就失效。`clamp_alias` 顺带去首尾空白，与 `set()` 存的形态一致。
            let alias = clamp_alias(alias);
            if alias.is_empty() {
                continue;
            }
            // 索引一律按**硬件身份键**建：文件里的键可能是历史形态的完整实例路径
            // （`migrate_exact_keys_to_identity` 之前写下的，或历史库没迁成时界面写下的），
            // 而界面交回来的查询键是新形态。不在这里归一，同一台设备的别名就只认
            // 其中一种形态 —— 换个统计周期名字消失、点「还原」还删不掉另一种形态。
            let identity = hardware_identity_key(key);
            let won = match exact.entry(identity.clone()) {
                // 同一身份的多个键形态：序在前的赢（与 `migrate_exact_keys_to_identity`
                // 同一套取舍；BTreeMap 迭代有序，完整路径以枚举器名开头必在前）
                std::collections::hash_map::Entry::Occupied(existing) => {
                    if existing.get() != &alias {
                        tracing::warn!(
                            "别名文件里 {key} 与另一条同身份键的别名不同，保留「{}」、忽略「{alias}」",
                            existing.get()
                        );
                    }
                    false
                }
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(alias.clone());
                    true
                }
            };
            // 型号回退只收"赢下来的那条"：被忽略的另一形态本就不参与展示，
            // 拿它去判歧义会把一个好端端的回退白白打掉。
            if !won {
                continue;
            }
            if let Some(model_key) = model_key(&identity) {
                match model.get(&model_key) {
                    None => {
                        model.insert(model_key, Some(alias));
                    }
                    // 同型号已有不同别名 → 标记歧义
                    Some(Some(existing)) if *existing != alias => {
                        model.insert(model_key, None);
                    }
                    Some(_) => {}
                }
            }
        }
        Self { exact, model }
    }

    /// 是否为空（无任何别名）。
    pub fn is_empty(&self) -> bool {
        self.exact.is_empty()
    }

    /// 解析某设备的展示名：精确（身份键）→ 型号回退，都没有则 None（调用方用自动名）。
    ///
    /// 传进来的可以是任意键形态（完整实例路径或身份段）：先归一成身份键再查，
    /// 所以读侧不再关心库里存的是哪一种。
    pub fn resolve(&self, device_key: &str) -> Option<&str> {
        let identity = hardware_identity_key(device_key);
        if let Some(alias) = self.exact.get(&identity) {
            return Some(alias.as_str());
        }
        let model_key = model_key(&identity)?;
        self.model.get(&model_key)?.as_deref()
    }

    /// 已有别名（供 UI / CLI 展示与编辑回填）。
    pub fn exact_alias(&self, device_key: &str) -> Option<&str> {
        self.exact
            .get(&hardware_identity_key(device_key))
            .map(|s| s.as_str())
    }
}

/// 型号级回退的键：VID/PID，外加接口/集合段（如果路径里有）。
///
/// 只用 VID/PID 会把复合设备并成一个：一个无线二合一接收器在 Windows 里是
/// `VID_xxxx&PID_yyyy&MI_00`（键盘）和 `&MI_01`（鼠标）两个设备实例，型号键相同。
/// 给键盘起名"办公键盘"之后，鼠标换个 USB 口（实例路径变了、精确匹配落空）就会
/// 顶着同一个名字。分开之后代价是：同一逻辑接口从"带 MI 段"变成"不带 MI 段"
/// 这种跨形态漂移时，回退不再命中，界面退回自动名 —— 少个别名比张冠李戴好。
fn model_key(device_key: &str) -> Option<String> {
    let (vid, pid) = parse_vid_pid(device_key)?;
    Some(match interface_tag(device_key) {
        Some(tag) => format!("{vid}/{pid}#{tag}"),
        None => format!("{vid}/{pid}"),
    })
}

/// 设备实例路径里的接口段：USB 复合设备是 `&MI_00`，蓝牙 HID 集合是 `&Col03`。
fn interface_tag(device_key: &str) -> Option<String> {
    let upper = device_key.to_ascii_uppercase();
    for marker in ["&MI_", "&COL"] {
        let Some(idx) = upper.find(marker) else {
            continue;
        };
        let tail = &upper[idx + marker.len()..];
        let value: String = tail
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        if !value.is_empty() {
            return Some(format!("{}{value}", &marker[1..]));
        }
    }
    None
}

/// 设备**归组键**（B14-2）：实例路径 `#` 分段后的硬件身份段。
///
/// 完整实例路径是 `枚举器#硬件ID#实例号` 三段（如
/// `HID#VID_046D&PID_C52B&MI_00#7&1f126e19&0&0000`），原来整串当 device_key：
/// 第三段的连接拓扑实例号换 USB 口就变 —— 同一台设备被拆成多行，计数从零开始、
/// 别名不跟。真正的硬件身份是中间那段 `VID_xxx&PID_yyy&MI_00`（型号 + 接口；
/// 接口段区分复合设备的键鼠两面，理由同 `model_key`）。
///
/// **硬取舍**（写在这里，别处别再猜）：Raw Input 拿不到 USB 序列号，同型号且
/// 同接口的多台设备（两只一样的鼠标、同一接收器下的多设备）在这个颗粒度
/// 下必然并成一台 —— 这是「最细稳定粒度」，不是「唯一粒度」。反过来，
/// 「只认完整路径」的旧键则是最细的**不稳定**粒度，换一次口就丢一段历史。
///
/// 非 `A#B#C` 三段形态的键原样返回：形状认不准时不并（宁可保持原状，
/// 也不把两台不同的设备错并到一起）。已迁移过的键（本身就是身份段，无 `#`）
/// 原样返回 → 本函数幂等，重复迁移是空操作。
pub fn hardware_identity_key(device_key: &str) -> String {
    let parts: Vec<&str> = device_key.split('#').collect();
    if parts.len() >= 3 && !parts[1].trim().is_empty() {
        parts[1].to_string()
    } else {
        device_key.to_string()
    }
}

/// 一次性迁移：把别名文件里挂在**完整实例路径**上的精确别名改挂到身份键上
/// （B14-2 的配套 —— device_key 换了，别名跟着走）。
///
/// 两个旧路径迁成同一个身份键（同一台设备换过口，或本来就是并组取舍内的
/// 同型号设备）而别名不同时，保留 BTreeMap 序在前的那个（确定性），
/// 被挤掉的记一条 warn —— 型号级回退（`model_key`）对两种键都还能命中，
/// 丢的只是「分口精确别名」这一层。
///
/// 幂等：身份键经 [`hardware_identity_key`] 原样返回，重复跑是空操作。
/// 读不出文件（占用/损坏）时不动：等下次启动再试，别在内容可疑时覆盖。
pub fn migrate_exact_keys_to_identity() {
    // 迁移本身也是「读全表 → 整表写回」，与 set()/clear() 共用同一把串行锁，
    // 否则启动迁移与界面上的一次改名交错，就是一次整表覆盖。
    let _serial = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = alias_path();
    let map = match try_read_map(&path) {
        AliasRead::Good(m) => m,
        // Broken：原文已另存 .json.bad，从空表开始与 set() 的语义一致 ——
        // 没有别名可迁，直接返回。
        AliasRead::Broken => return,
        AliasRead::Unreadable => {
            tracing::warn!("设备别名迁移跳过：别名文件此刻读不出来，下次启动重试");
            return;
        }
    };
    let mut migrated: BTreeMap<String, String> = BTreeMap::new();
    let mut changed = false;
    for (key, alias) in &map {
        // 迁移是「原样读 → 原样写回」，不过长度闸就等于把超长别名再落一次盘
        let alias = clamp_alias(alias);
        if alias.is_empty() {
            continue;
        }
        let identity = hardware_identity_key(key);
        if identity == *key {
            // 已是身份键的那条，同样可能撞上前面「完整路径迁过来」的那条：
            // 必须走下面同一套判定。无条件 insert 是**后来者覆盖** —— 完整路径以
            // 枚举器名开头（`HID#`、`USB#`），身份键以中间那段 `VID_` 开头，
            // BTreeMap 序里前者必在前，于是用户的别名会被无声换掉，而函数头
            // 承诺的是「序在前的赢 + 被挤掉的记 warn」。
            match migrated.get(&identity) {
                Some(existing) if *existing != alias => {
                    tracing::warn!(
                        "别名迁移：{key} 与已迁移条目并到同一身份键 {identity}，保留「{existing}」、丢弃「{alias}」"
                    );
                }
                Some(_) => {}
                None => {
                    migrated.insert(identity, alias);
                }
            }
            continue;
        }
        changed = true;
        match migrated.get(&identity) {
            // 身份键已存在且来自别的旧路径：键序在前的赢（BTreeMap 迭代有序）
            Some(existing) if *existing != alias => {
                tracing::warn!(
                    "别名迁移：{key} 与已有条目并到同一身份键 {identity}，保留「{existing}」、丢弃「{alias}」"
                );
            }
            Some(_) => {
                // 同名别名：并入即可，不用记
            }
            None => {
                migrated.insert(identity, alias);
            }
        }
    }
    if !changed {
        return;
    }
    if let Err(e) = write_map(&migrated) {
        tracing::error!("设备别名迁移写回失败（原文件未动）: {e}");
    } else {
        tracing::info!("设备别名迁移：精确键已从完整实例路径改挂到硬件身份键");
    }
}

/// 别名文件路径。
fn alias_path() -> PathBuf {
    paths::data_dir().join("device_aliases.json")
}

/// 别名文件路径（对外只读）。备份模块要拿它做快照 —— 别处不要自己拼这个文件名。
pub fn file_path() -> PathBuf {
    alias_path()
}

/// 缓存：`(文件 mtime, 别名表)`；mtime 未变则复用。
static CACHE: Mutex<Option<(Option<SystemTime>, AliasTable, PathBuf)>> = Mutex::new(None);

fn file_mtime(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

/// 读取别名表（带 mtime 缓存；文件不存在或损坏时返回空表，不报错）。
pub fn table() -> AliasTable {
    let path = alias_path();
    let mtime = file_mtime(&path);
    {
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((cached_mtime, table, cached_path)) = cache.as_ref() {
            if *cached_mtime == mtime && *cached_path == path {
                return table.clone();
            }
        }
    }
    let map = match try_read_map(&path) {
        AliasRead::Good(m) => m,
        // 读不出可信内容时只这一次返回空表，**不写缓存**：否则一次同步盘占用会让
        // "这个用户没有任何别名"这个假象一直挂到文件 mtime 变化为止。
        _ => return AliasTable::from_map(&BTreeMap::new()),
    };
    let table = AliasTable::from_map(&map);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    *cache = Some((mtime, table.clone(), path));
    table
}

/// 读别名文件的结果 —— 分三类，因为只有"读不出、但内容可能完好"这一类该拒绝写回。
///
/// `set()` 的做法是"读全表 → 改一行 → 整体写回"，所以旧实现把几种失败全并成
/// "空表"就有问题：一次改名会**静默抹掉其余全部别名**，函数还照旧返回 Ok。
/// 触发路径都不罕见：
/// - `Unreadable`：OneDrive/杀软正占着文件（共享冲突）、权限问题 —— 内容很可能是好的，
///   覆盖就是真丢数据，必须停手；
/// - `Broken`：非法 UTF-8（记事本"另存为 ANSI"）或 JSON 截断 —— 这份内容我们自己已经
///   用不了，另存一份 `.json.bad` 之后可以从空表重新开始（既有测试断言的
///   "坏文件后仍可正常写入"就是这一类，语义保留）；
/// - `Good`：正常，或文件本来不存在。
enum AliasRead {
    Good(BTreeMap<String, String>),
    Broken,
    Unreadable,
}

fn try_read_map(path: &std::path::Path) -> AliasRead {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return AliasRead::Good(BTreeMap::new());
        }
        Err(e) => {
            tracing::error!("读取设备别名文件失败（内容可能完好），本次不据此覆盖: {e}");
            return AliasRead::Unreadable;
        }
    };
    match serde_json::from_slice::<BTreeMap<String, String>>(&bytes) {
        Ok(map) => AliasRead::Good(map),
        Err(e) => {
            tracing::error!("设备别名文件解析失败，原文另存为 .json.bad 后重新开始: {e}");
            // 留一份原文，名字还有机会救回来；只留第一次，别把目录刷成一堆备份
            let bad = path.with_extension("json.bad");
            if !bad.exists() {
                let _ = std::fs::write(&bad, &bytes);
            }
            AliasRead::Broken
        }
    }
}

/// 读不出可信内容时给调用方（界面）的原因文案。
fn alias_unreadable(path: &std::path::Path) -> anyhow::Error {
    anyhow::anyhow!(
        "设备别名文件此刻读不出来（常被同步盘或杀软短暂占用），已取消本次改动：\
         继续写会把其余别名一起抹掉。稍等几秒再试一次即可；文件位置 {}",
        path.display()
    )
}

/// 读-改-写的串行锁（同进程内）。
///
/// `set()`/`clear()` 的形态是「读全表 → 动一行 → 整表写回」，所以两次并发改名
/// 各自读到同一份旧快照时，后写的那一份会把先写的整表**整个盖掉** —— 先改的那个
/// 名字静默消失，两个函数还都照旧返回 Ok。界面上连按两次回车、两条 IPC 并发进来
/// 就是这一种；改名是低频动作，串行化的代价可以忽略。
///
/// 锁只兜得住本进程。与 `focusflow-cli --rename-device` 同时写仍是
/// last-writer-wins：没上锁文件，因为进程被强杀留下的锁会把改名**永久**卡死，
/// 那比现在这个偶发覆盖更糟。读侧不受影响（`table()` 每次按 mtime 现读，
/// 手改文件的内容不会被下一次改名抹掉）。
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// 抹掉别名表里挂在**同一台设备其他键形态**上的条目，返回抹掉的条数。
///
/// `set()`/`clear()` 一律按身份键落盘，而文件里可能还留着历史形态的完整实例路径
/// （`migrate_exact_keys_to_identity` 跑之前写的，或某个历史库没迁成时界面写下的）。
/// 不清它们的话有两个后果：改一次名留下两条同身份条目（谁生效取决于这次查的是哪种
/// 形态），以及点「还原」只删得掉当前那一条 —— 另一条继续顶着旧名显示，名字赖着不走。
/// 同一身份按 [`hardware_identity_key`] 的定义就是同一台设备，所以并掉不是张冠李戴。
fn prune_other_forms(map: &mut BTreeMap<String, String>, identity: &str) -> usize {
    let stale: Vec<String> = map
        .keys()
        .filter(|k| k.as_str() != identity && hardware_identity_key(k) == identity)
        .cloned()
        .collect();
    for key in &stale {
        tracing::warn!("别名按身份键收拢：{key} 与 {identity} 是同一台设备，旧形态条目已移除");
        map.remove(key);
    }
    stale.len()
}

/// 写入别名：`alias` 为空白时等同删除。返回写入后的数量。
///
/// 键一律先归一成硬件身份段（见 `prune_other_forms`），所以库里存的是完整实例路径
/// 还是身份段、界面交回来的是哪一种，都不影响最终显示与「还原」。
pub fn set(device_key: &str, alias: &str) -> anyhow::Result<usize> {
    let _serial = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = alias_path();
    let mut map = match try_read_map(&path) {
        AliasRead::Good(m) => m,
        AliasRead::Broken => BTreeMap::new(),
        AliasRead::Unreadable => return Err(alias_unreadable(&path)),
    };
    let identity = hardware_identity_key(device_key);
    prune_other_forms(&mut map, &identity);
    let trimmed = alias.trim();
    if trimmed.is_empty() {
        map.remove(&identity);
    } else {
        map.insert(identity, clamp_alias(trimmed));
    }
    write_map(&map)?;
    Ok(map.len())
}

/// 删除某设备别名，返回是否确有删除。
pub fn clear(device_key: &str) -> anyhow::Result<bool> {
    let _serial = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = alias_path();
    let mut map = match try_read_map(&path) {
        AliasRead::Good(m) => m,
        AliasRead::Broken => BTreeMap::new(),
        AliasRead::Unreadable => return Err(alias_unreadable(&path)),
    };
    let identity = hardware_identity_key(device_key);
    let dropped_other_forms = prune_other_forms(&mut map, &identity);
    let removed = map.remove(&identity).is_some() || dropped_other_forms > 0;
    if removed {
        write_map(&map)?;
    }
    Ok(removed)
}

/// 截断到 [`MAX_ALIAS_CHARS`] 个字符（按字符而非字节，避免切坏 UTF-8）。
pub fn clamp_alias(alias: &str) -> String {
    let trimmed = alias.trim();
    if trimmed.chars().count() <= MAX_ALIAS_CHARS {
        return trimmed.to_string();
    }
    trimmed.chars().take(MAX_ALIAS_CHARS).collect()
}

/// 原子写回（同目录临时文件 + fsync + rename），并立即失效缓存。
///
/// 写的是 `device_aliases.json` —— 用户手工录入、库里没有第二份的名字。
/// 原来这里自己写了半套：临时文件名固定（两个并发写会互相把对方的临时文件
/// rename 走，然后一方报"找不到文件"）、且**没有 fsync**（掉电/崩溃留下的
/// 半截 JSON 下一次读会被判成 Broken，从空表重新开始 = 全部别名没了）。
/// 现在与 `config.ini` 共用 [`crate::config::atomic_write`]，同一套纪律一份实现。
fn write_map(map: &BTreeMap<String, String>) -> anyhow::Result<()> {
    let path = alias_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(map)?;
    if let Err(e) = crate::config::atomic_write(&path, &json) {
        // 换一层自己的说法再上抛：共享实现里那句是「配置文件原子替换失败」，
        // 而这条错误文案会一路走到界面上的改名失败提示里，指着 config.ini 说事
        // 会把人引到另一个文件去。
        return Err(e.context("设备别名文件原子替换失败"));
    }
    // mtime 精度可能不足（同秒内多次改），直接清缓存保证下次读到新值
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    Ok(())
}

/// 清空缓存（测试与数据目录切换时用）。
pub fn invalidate_cache() {
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 串行锁 + 隔离目录，`f` 的返回值照旧透出。
    ///
    /// 目录改由 `TestAppDir` 的 Drop 回收：原来收尾那行手写 `remove_dir_all(..).ok()`
    /// 在只读连接池还握着句柄时会静默失败，每跑一次就在 %TEMP% 留一份库。
    fn with_temp_dir<T>(name: &str, f: impl FnOnce() -> T) -> T {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir(&format!("alias_{name}"));
        invalidate_cache();
        let out = f();
        invalidate_cache();
        out
    }

    /// 设置 → 精确命中 → 删除 → 落回自动名。
    #[test]
    fn set_resolve_clear_roundtrip() {
        with_temp_dir("rt", || {
            let key = "HID#VID_24AE&PID_1464&MI_00#7&1f126e19&0&0000";
            assert!(table().is_empty());
            assert!(table().resolve(key).is_none());

            set(key, "新鼠标").unwrap();
            assert_eq!(table().resolve(key), Some("新鼠标"));
            assert_eq!(table().exact_alias(key), Some("新鼠标"));
            // 文件确实落盘且可读
            let text = std::fs::read_to_string(alias_path()).unwrap();
            assert!(text.contains("新鼠标"), "别名应写入 JSON: {text}");

            clear(key).unwrap();
            assert!(table().resolve(key).is_none());
        });
    }

    /// 型号回退：实例路径变了（换 USB 口），同型号的别名仍生效。
    #[test]
    fn model_fallback_matches_replugged_port() {
        with_temp_dir("model", || {
            let old_key = "HID#VID_046D&PID_C52B&MI_00#7&OLDPORT&0&0000";
            let new_key = "HID#VID_046D&PID_C52B&MI_00#7&NEWPORT&0&0000";
            set(old_key, "新鼠标").unwrap();
            assert_eq!(table().resolve(old_key), Some("新鼠标"));
            assert_eq!(
                table().resolve(new_key),
                Some("新鼠标"),
                "同型号换端口后别名应命中"
            );
            // 换口后的路径与旧路径是**同一台设备**（同一身份段），所以在 new_key 上
            // 再起名就是改这台设备的名字，不是"另开一条精确别名压过回退"。
            // B14-2 之前这两条是两个键、可以并存两个名字；现在文件里只该留一条。
            set(new_key, "第二只 G304").unwrap();
            assert_eq!(table().resolve(new_key), Some("第二只 G304"));
            assert_eq!(
                table().resolve(old_key),
                Some("第二只 G304"),
                "同一台设备只有一个名字"
            );
            let text = std::fs::read_to_string(alias_path()).unwrap();
            let after: BTreeMap<String, String> = serde_json::from_str(&text).unwrap();
            assert_eq!(after.len(), 1, "两种键形态该收拢成一条: {text}");
            assert!(
                after.contains_key("VID_046D&PID_C52B&MI_00"),
                "留下的那条必须是身份键: {text}"
            );
        });
    }

    /// 二合一接收器：同一型号的键盘面与鼠标面不共用一个型号回退键。
    ///
    /// 旧实现的回退键只有 `VID/PID`，所以给键盘面起名"办公键盘"之后，鼠标面换
    /// 一个 USB 口（精确匹配落空 → 走到回退）就顶着同一个名字显示。
    #[test]
    fn composite_device_interfaces_do_not_share_the_model_key() {
        with_temp_dir("composite", || {
            let kb = "HID#VID_24AE&PID_1464&MI_00#7&1111&0&0000";
            let mouse = "HID#VID_24AE&PID_1464&MI_01#7&2222&0&0001";
            set(kb, "办公键盘").unwrap();
            assert_eq!(table().resolve(kb), Some("办公键盘"));
            let mouse_replugged = "HID#VID_24AE&PID_1464&MI_01#7&9999&0&0001";
            assert_eq!(
                table().resolve(mouse_replugged),
                None,
                "另一个接口的设备不该被型号回退带进键盘的名字"
            );
            // 同接口换端口照常回退命中（这才是型号回退存在的理由）
            assert_eq!(
                table().resolve("HID#VID_24AE&PID_1464&MI_00#7&8888&0&0000"),
                Some("办公键盘"),
                "同一接口换端口仍应回退命中"
            );
            // 鼠标面自己起了名字之后，两面各自独立
            set(mouse, "游戏鼠标").unwrap();
            assert_eq!(table().resolve(mouse_replugged), Some("游戏鼠标"));
            assert_eq!(table().resolve(kb), Some("办公键盘"));
        });
    }

    /// 蓝牙 HID 用 `&Colxx` 分集合，与 USB 的 `&MI_xx` 同等对待。
    #[test]
    fn bluetooth_collection_tags_split_the_model_key() {
        with_temp_dir("btcol", || {
            let c1 = r"HID#{00001812-0000-1000-8000-00805f9b34fb}_Dev_VID&0107d7_PID&efff_REV&0120_d46d51083b12&Col01#9&aaaa&0&0001";
            assert_eq!(interface_tag(c1).as_deref(), Some("COL01"));
            assert_eq!(interface_tag("HID#VID_1234&PID_5678#x"), None);
            set(c1, "键盘集合").unwrap();
            let c2 = c1.replace("&Col01", "&Col02");
            assert_eq!(table().resolve(c1), Some("键盘集合"));
            assert_eq!(table().resolve(&c2), None, "另一个集合不该命中");
        });
    }

    /// 同型号多个不同别名 → 视为歧义，不参与型号回退（避免张冠李戴）。
    ///
    /// 三个键必须是**三台不同设备**才谈得上歧义：B14-2 之后 `#A/#B/#C` 那种
    /// 「同身份换个实例号」已经被有意并成一台（见 `hardware_identity_key` 的硬取舍），
    /// 拿它们造歧义造不出来 —— 三次 `set` 落在同一个身份键上，最后一次说话。
    /// 这里用两个真会分身的身份形态：一个带 `&MI_00`、一个在同型号上再挂个 `&Col01`，
    /// 两者的型号回退键都是 `046D/C52B#MI_00`（`interface_tag` 先认 `&MI_`）。
    #[test]
    fn ambiguous_model_aliases_do_not_fallback() {
        with_temp_dir("ambiguous", || {
            let a = "VID_046D&PID_C52B&MI_00";
            let b = "VID_046D&PID_C52B&MI_00&Col01";
            assert_ne!(a, b, "夹具得是两个不同身份，否则并成一台就没有歧义可判");
            assert_eq!(
                model_key(a).as_deref(),
                model_key(b).as_deref(),
                "夹具的两个身份必须共用一个型号回退键"
            );
            set(a, "旧鼠标").unwrap();
            set(b, "新鼠标").unwrap();
            // 第三台：型号回退键一样，但没有任何一条精确别名 —— 这才轮到回退说话。
            // （注意别用 `HID#VID_046D&PID_C52B&MI_00#…` 那种完整路径当"陌生设备"：
            // 它归一之后就是 a 自己，命中的是精确别名，不是回退。）
            let unknown = "VID_046D&PID_C52B&MI_00&Col03";
            assert_eq!(
                model_key(unknown).as_deref(),
                model_key(a).as_deref(),
                "夹具的陌生设备必须与 a 共用型号回退键"
            );
            assert_eq!(table().resolve(unknown), None, "歧义型号不应回退");
            // 但精确匹配照常
            assert_eq!(table().resolve(a), Some("旧鼠标"));
            assert_eq!(table().resolve(b), Some("新鼠标"));
            // 换口的 a 走精确（身份段一致），不受歧义影响
            assert_eq!(
                table().resolve("HID#VID_046D&PID_C52B&MI_00#7&9999&0&0000"),
                Some("旧鼠标")
            );
        });
    }

    /// 空白别名等同删除；超长别名按字符截断；坏文件不影响主流程。
    #[test]
    fn edge_cases_do_not_break() {
        with_temp_dir("edge", || {
            let key = "HID#VID_1234&PID_5678#x";
            set(key, "   ").unwrap();
            assert!(table().is_empty(), "全空白别名应视为删除");

            let long = "鼠".repeat(MAX_ALIAS_CHARS + 10);
            set(key, &long).unwrap();
            let got = table().resolve(key).unwrap().to_string();
            assert_eq!(got.chars().count(), MAX_ALIAS_CHARS);
            assert_eq!(clamp_alias("  短名  "), "短名");

            std::fs::write(alias_path(), "{ 坏掉的 json").unwrap();
            invalidate_cache();
            assert!(table().is_empty(), "坏文件应退化为空表而不是 panic");
            // 坏文件后仍可正常写入
            set(key, "恢复").unwrap();
            assert_eq!(table().resolve(key), Some("恢复"));
        });
    }

    /// 无 VID/PID 的设备（触控板、PS/2、ACPI 键盘）：只按身份段认，没有型号回退。
    ///
    /// 口径与 B14-2 一致 —— 换个实例号（`#5&36f79095&0&0000` 那一段）还是同一台设备，
    /// 别名跟着走；而"同一段设备 ID 的另一个接口"（`&Col02`）是另一台，不落。
    /// 老用例在这里断言的是"换个实例路径就不认"，那是完整实例路径当主键时代的口径，
    /// 归组键换轨之后已经反过来了。
    #[test]
    fn no_vid_pid_devices_match_by_identity_only() {
        with_temp_dir("novid", || {
            set("HID#MSFT0001&Col01#5&36f79095&0&0000", "触控板").unwrap();
            assert_eq!(
                table().resolve("HID#MSFT0001&Col01#5&36f79095&0&0000"),
                Some("触控板")
            );
            assert_eq!(
                table().resolve("HID#MSFT0001&Col01#7&9999&0&0001"),
                Some("触控板"),
                "同一台设备换个实例号，别名必须跟着走"
            );
            assert_eq!(
                table().resolve("HID#MSFT0001&Col02#5&36f79095&0&0001"),
                None,
                "另一个接口是另一台设备"
            );
            // 没有 VID/PID 就没有型号回退键可用
            assert_eq!(model_key("MSFT0001&Col01"), None);
        });
    }

    /// 别名与键形态无关：写、认、还原都只看身份键。
    ///
    /// 这一条盯的是"设备名称改名之后失效"那组形态 —— 库里存完整实例路径
    /// （历史库没迁成）而界面交回来的是身份键，或别名文件是照 README 手改的：
    /// ① 别名只认写入时那一种形态 → 换个统计周期名字消失；
    /// ② 「还原」只删当前行键 → 另一形态那条赖在文件里，名字删不掉。
    #[test]
    fn rename_lookup_and_restore_ignore_the_key_form() {
        with_temp_dir("forms", || {
            let legacy = "HID#VID_046D&PID_C52B&MI_00#7&OLDPORT&0&0000";
            let ident = "VID_046D&PID_C52B&MI_00";

            // 手改文件（README 说"纯文本，可直接手改"）写的是历史形态
            let map = BTreeMap::from([(legacy.to_string(), "手写的名字".to_string())]);
            std::fs::create_dir_all(alias_path().parent().unwrap()).unwrap();
            std::fs::write(alias_path(), serde_json::to_string(&map).unwrap()).unwrap();
            invalidate_cache();
            assert_eq!(
                table().resolve(ident),
                Some("手写的名字"),
                "历史形态写的别名，按身份键也该认"
            );
            // 「还原」按当前行键（身份形态）来，历史形态那条必须跟着走
            assert!(
                clear(ident).unwrap(),
                "换一种键形态就删不掉，名字会赖在文件里"
            );
            assert!(table().is_empty(), "还原之后不该留任何形态的条目");

            // 改名落在历史形态的键上，查询用身份键
            set(legacy, "办公鼠标").unwrap();
            assert_eq!(table().resolve(ident), Some("办公鼠标"));
            // 再在身份键上改一次：文件里只该有一条，不能两种形态各留一个名字
            set(ident, "家里那把").unwrap();
            let text = std::fs::read_to_string(alias_path()).unwrap();
            let after: BTreeMap<String, String> = serde_json::from_str(&text).unwrap();
            assert_eq!(after.len(), 1, "同一台设备在文件里只该有一条: {text}");
            assert_eq!(after.get(ident).map(|s| s.as_str()), Some("家里那把"));
            assert!(!text.contains(legacy), "旧形态不该再留在文件里: {text}");
        });
    }

    /// B14-2：身份键的三段提取 + 幂等性。
    #[test]
    fn hardware_identity_key_extracts_the_middle_segment() {
        // 标准三段：取中间
        assert_eq!(
            hardware_identity_key("HID#VID_046D&PID_C52B&MI_00#7&1f126e19&0&0000"),
            "VID_046D&PID_C52B&MI_00"
        );
        // 换 USB 口只动第三段：身份不变 —— 这就是归组键要的稳定性
        assert_eq!(
            hardware_identity_key("HID#VID_046D&PID_C52B&MI_00#8&2c5f77d4&0&0001"),
            "VID_046D&PID_C52B&MI_00"
        );
        // 接口段在键里：复合设备的键盘面与鼠标面不并组
        assert_eq!(
            hardware_identity_key("HID#VID_24AE&PID_1464&MI_01#7&2222&0&0001"),
            "VID_24AE&PID_1464&MI_01"
        );
        // 已是身份键：原样返回（幂等，迁移重跑是空操作）
        assert_eq!(
            hardware_identity_key("VID_046D&PID_C52B&MI_00"),
            "VID_046D&PID_C52B&MI_00"
        );
        // 形状认不准（只有两段 / 无 #）：不改 —— 宁可保持原状也不错并
        assert_eq!(hardware_identity_key("HID#ORPHAN"), "HID#ORPHAN");
        assert_eq!(hardware_identity_key("RDP_MOU"), "RDP_MOU");
    }

    /// 照**旧版本的写法**把别名原样落盘（不经 `set()` 的键归一）。
    ///
    /// `set()` 现在总是把键收拢到身份段，所以"两种键形态并存"的文件只可能来自
    /// 旧版本或用户手改 —— 那正是下面两条迁移用例要处理的对象，用 `set()` 造不出来。
    fn write_raw_alias_file(entries: &[(&str, &str)]) {
        let map: BTreeMap<String, String> = entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        std::fs::create_dir_all(alias_path().parent().unwrap()).unwrap();
        std::fs::write(alias_path(), serde_json::to_string(&map).unwrap()).unwrap();
        invalidate_cache();
    }

    /// B14-2：别名文件的精确键跟着 device_key 换轨。
    #[test]
    fn alias_exact_keys_migrate_to_identity() {
        with_temp_dir("migrate_keys", || {
            let old_a = "HID#VID_046D&PID_C52B&MI_00#7&OLD&0&0000";
            let old_b = "HID#VID_046D&PID_C52B&MI_00#8&NEW&0&0001";
            let kb = "VID_1B1C&PID_1B2D"; // 已是身份形态：必须原样保留
            write_raw_alias_file(&[(old_a, "办公鼠标"), (old_b, "家里那把"), (kb, "办公键盘")]);

            migrate_exact_keys_to_identity();

            // 两个旧路径同身份：保留一个（键序在前的），另一个被挤掉要留日志
            let text = std::fs::read_to_string(alias_path()).unwrap();
            assert!(
                text.contains("VID_046D&PID_C52B&MI_00"),
                "别名应改挂到身份键: {text}"
            );
            assert!(
                !text.contains("#OLD") && !text.contains("#NEW"),
                "完整路径键不该再留在别名文件里: {text}"
            );
            assert!(
                text.contains("VID_1B1C&PID_1B2D"),
                "已是身份键的原样保留: {text}"
            );
            // 新键上解析得到别名（换口后仍然命中，不再只靠型号回退）
            assert_eq!(
                table().resolve("VID_046D&PID_C52B&MI_00"),
                Some("办公鼠标"),
                "键序在前的赢（BTreeMap 序：#8 开头的键排在 #7 之后）"
            );
            assert_eq!(table().resolve(kb), Some("办公键盘"));

            // 幂等：再跑一遍不再变化
            migrate_exact_keys_to_identity();
            let text2 = std::fs::read_to_string(alias_path()).unwrap();
            assert_eq!(text, text2, "第二次迁移应是空操作");
        });
    }

    /// A①（§十五 第 15 条）：迁移撞上**已有身份键**时，序在前的赢，被挤掉的必须留 warn。
    ///
    /// 完整路径 `HID#…` 在 BTreeMap 里必排在身份键 `VID_…` 之前，所以"已是身份键"
    /// 那一支的无条件 insert 是后来者覆盖：用户的别名被无声换掉、一条日志都没有。
    #[test]
    fn migration_keeps_the_first_alias_and_warns_about_the_dropped_one() {
        with_temp_dir("migrate_collide", || {
            let full = "HID#VID_046D&PID_C52B&MI_00#7&1f126e19&0&0000";
            let ident = "VID_046D&PID_C52B&MI_00";
            // 直接写文件：`set()` 已经把两种形态收拢成一条，用它造不出"旧版遗留的
            // 两条并存"这份输入。
            write_raw_alias_file(&[(full, "办公鼠标"), (ident, "家里那把")]);

            let ((), logs) = crate::logger::capture_logs(migrate_exact_keys_to_identity);

            let text = std::fs::read_to_string(alias_path()).unwrap();
            let after: BTreeMap<String, String> = serde_json::from_str(&text).unwrap();
            assert_eq!(
                after.len(),
                1,
                "两条应并成一条身份键条目（而不是留下两个键）: {text}"
            );
            assert_eq!(
                after.get(ident).map(|s| s.as_str()),
                Some("办公鼠标"),
                "承诺的是「序在前的赢」，不是后来者覆盖: {text}"
            );
            assert!(
                logs.iter().any(|l| l.contains("丢弃")),
                "被挤掉的那条必须留 warn，否则用户不知道名字什么时候换的: {logs:?}"
            );
        });
    }

    /// A②（§十五 第 16 条）：长度闸不能只在 `set()` 里 —— 手改的文件读进来也要钳。
    #[test]
    fn hand_edited_overlong_alias_is_clamped_when_read() {
        with_temp_dir("clamp_read", || {
            let key = "HID#VID_046D&PID_C52B&MI_00#7&1f126e19&0&0000";
            let long = "鼠".repeat(MAX_ALIAS_CHARS + 36);
            let map = BTreeMap::from([(key.to_string(), long.clone())]);
            std::fs::create_dir_all(alias_path().parent().unwrap()).unwrap();
            std::fs::write(alias_path(), serde_json::to_string(&map).unwrap()).unwrap();
            invalidate_cache();

            // 夹具必须是真超长的，否则这条用例什么都没测
            let text = std::fs::read_to_string(alias_path()).unwrap();
            assert!(text.contains(&long), "文件里应留着超长原文: {text}");

            let cached = table();
            let got = cached.resolve(key).expect("手改的别名应能读到");
            assert_eq!(
                got.chars().count(),
                MAX_ALIAS_CHARS,
                "读侧必须过长度闸（撑破表格的防线不能只在写侧）: {got}"
            );
            // 型号回退用的是同一份索引，也必须是被钳过的值
            let replugged = "HID#VID_046D&PID_C52B&MI_00#9&2c5f77d4&0&0001";
            assert_eq!(
                table().resolve(replugged).map(|s| s.chars().count()),
                Some(MAX_ALIAS_CHARS),
                "回退命中时不该拿出未钳的别名"
            );
        });
    }

    /// A② 的另一半：迁移「读原样 → 写回」时不得把超长别名再落一次盘。
    #[test]
    fn migration_writes_aliases_through_the_length_gate() {
        with_temp_dir("clamp_migrate", || {
            let full = "HID#VID_1B1C&PID_1B2D#7&1111&0&0000";
            let long = "键".repeat(MAX_ALIAS_CHARS + 20);
            let map = BTreeMap::from([(full.to_string(), long.clone())]);
            std::fs::create_dir_all(alias_path().parent().unwrap()).unwrap();
            std::fs::write(alias_path(), serde_json::to_string(&map).unwrap()).unwrap();
            invalidate_cache();

            migrate_exact_keys_to_identity();

            let text = std::fs::read_to_string(alias_path()).unwrap();
            let after: BTreeMap<String, String> = serde_json::from_str(&text).unwrap();
            let kept = after
                .get("VID_1B1C&PID_1B2D")
                .expect("完整路径应改挂到身份键");
            assert_eq!(
                kept.chars().count(),
                MAX_ALIAS_CHARS,
                "迁移写回要过长度闸: {kept}"
            );
            assert!(
                !text.contains(&long),
                "超长原文不该被迁移原样再落一次盘: {text}"
            );
        });
    }

    /// 并发改名不得互相吃掉。
    ///
    /// `set()` 的形态是「读全表 → 动一行 → 整表写回」：没有串行锁时这一批线程
    /// 各自读到同一份旧快照，最后落盘的那一份只剩自己那一条，其余 N-1 个名字
    /// **静默消失**而每次调用都返回 Ok。用 Barrier 把起点钉在同一刻，
    /// 这条用例才是"盯着交错"而不是"碰巧没交错"。
    #[test]
    fn concurrent_renames_do_not_eat_each_other() {
        with_temp_dir("concurrent_set", || {
            const N: usize = 24;
            let gate = std::sync::Arc::new(std::sync::Barrier::new(N));
            let handles: Vec<_> = (0..N)
                .map(|i| {
                    let gate = std::sync::Arc::clone(&gate);
                    std::thread::spawn(move || {
                        gate.wait();
                        set(&format!("VID_000{i}&PID_{i:04}"), &format!("设备{i}"))
                    })
                })
                .collect();
            let errs: Vec<String> = handles
                .into_iter()
                .flat_map(|h| h.join())
                .filter_map(|r| r.err().map(|e| e.to_string()))
                .collect();
            assert!(errs.is_empty(), "并发改名每一条都该成功: {errs:?}");

            let text = std::fs::read_to_string(alias_path()).unwrap();
            let after: BTreeMap<String, String> = serde_json::from_str(&text).unwrap();
            assert_eq!(after.len(), N, "{N} 个名字都得留在文件里: {text}");
            for i in 0..N {
                assert_eq!(
                    after
                        .get(&format!("VID_000{i}&PID_{i:04}"))
                        .map(|s| s.as_str()),
                    Some(format!("设备{i}")).as_deref(),
                    "第 {i} 条被并发改名吃掉了: {text}"
                );
            }
            // 临时文件不得残留：固定名的 .tmp 会被另一个写者的 rename 抢走，
            // 剩下那一方直接报"找不到文件"，改名就失败
            let leftovers: Vec<String> = temp_names_with(".tmp");
            assert!(leftovers.is_empty(), "写完之后不留临时文件: {leftovers:?}");
        });
    }

    /// 写回失败那一次：必须报错、原文件一字不动、临时文件不留。
    ///
    /// 失败面用只读位造 —— 这是本机唯一稳定的「读得动、就是 rename 不进去」的形态
    /// （同一手法见 `config.rs` 的 `a_failed_save_keeps_the_pending_marks`）。
    /// 原来这条路径是 `std::fs::rename(&tmp, &path)?` 直接上抛：错误是报了，
    /// 但同目录留一个 `device_aliases.json.tmp` 没人清；改名再多次也只是往那里堆。
    #[cfg(windows)]
    #[test]
    fn a_failed_alias_rewrite_reports_and_leaves_no_temp_file() {
        // 只读位用 `attrib` 设：`PermissionsExt::set_readonly` 在本工具链还没稳定
        // （E0658，issue #152956）
        let attrib_ro = |p: &std::path::Path, on: bool| {
            let flag = if on { "+R" } else { "-R" };
            std::process::Command::new("cmd")
                .args(["/C", "attrib", flag, &p.to_string_lossy()])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        // panic 路径也要撤掉只读位，否则 TestAppDir::drop 删不动目录（%TEMP% 泄漏）
        struct RoGuard<'a>(
            &'a std::path::Path,
            &'a dyn Fn(&std::path::Path, bool) -> bool,
        );
        impl Drop for RoGuard<'_> {
            fn drop(&mut self) {
                let _ = (self.1)(self.0, false);
            }
        }

        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("alias_write_fails");
        invalidate_cache();
        set("VID_0001&PID_0001", "第一只").unwrap();
        set("VID_0002&PID_0002", "第二只").unwrap();
        let path = alias_path();
        let good = std::fs::read(&path).unwrap();
        if !attrib_ro(&path, true) {
            eprintln!("attrib +R 没生效，跳过（夹具做不出来）");
            return;
        }
        let _ro = RoGuard(&path, &attrib_ro);

        let err = set("VID_0003&PID_0003", "第三只")
            .expect_err("目标写不进去时必须报错，不能静默当成写好了");
        assert!(
            err.to_string().contains("设备别名文件"),
            "错误要说清楚是哪个文件（界面上就把这句给用户）: {err}"
        );
        assert_eq!(
            std::fs::read(&path).expect("原文件必须还在"),
            good,
            "失败的写回绝不能碰原文件"
        );
        let leftovers: Vec<String> = temp_names_with(".tmp");
        assert!(
            leftovers.is_empty(),
            "写回失败要把临时文件清掉: {leftovers:?}"
        );
        // 撤掉只读位后同一次改名要能落成，且原有两条一条不少
        assert!(attrib_ro(&path, false), "撤掉只读位该成功");
        drop(_ro);
        let n = set("VID_0003&PID_0003", "第三只").expect("解锁后应当写得动");
        assert_eq!(n, 3, "原有的两条别名必须还在: {n}");
        assert_eq!(table().resolve("VID_0002&PID_0002"), Some("第二只"));
    }

    /// 数据目录里名字含指定片段的文件（临时文件残留的观察量）。
    fn temp_names_with(part: &str) -> Vec<String> {
        std::fs::read_dir(alias_path().parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(part))
            .collect()
    }
}

#[cfg(test)]
mod unreadable_file_tests {
    use super::*;

    /// 「文件被占用」与「文件内容坏了」是两件事：前者绝不能覆盖，后者要能自救。
    ///
    /// 旧实现把两者都当成"空表"，于是一次改名会连带**静默抹掉其余全部别名**并返回 Ok。
    #[test]
    fn unreadable_file_is_not_overwritten_but_broken_file_self_heals() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("alias_unreadable");
        set("k1", "办公键盘").expect("正常写入应当成功");
        set("k2", "游戏鼠标").expect("正常写入应当成功");
        let path = alias_path();
        let good = std::fs::read(&path).expect("别名文件应存在");
        assert!(String::from_utf8_lossy(&good).contains("办公键盘"));

        // ① 被别的程序独占打开（同步盘/杀软那一类，内容本身是好的）：必须停手
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let hold = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0) // 拒绝一切共享：后续 open 直接报共享冲突
                .open(&path)
                .expect("占位句柄应能打开");
            let err = set("k3", "新名字").expect_err("读不出来时必须拒绝整体写回");
            assert!(err.to_string().contains("读不出来"), "{err}");
            assert!(
                !path.with_extension("json.bad").exists(),
                "内容没问题，不该被当成坏文件另存"
            );
            drop(hold);
            // 内容比对要放在释放占位句柄之后：share_mode(0) 之下连测试自己也读不到它
            assert_eq!(
                std::fs::read(&path).expect("文件应还在"),
                good,
                "被拒绝的写回绝不能碰原文件"
            );
            // 反向腿：占用结束后同一次改名要能成功，且原有别名一条不少
            let n = set("k3", "新名字").expect("释放占用后应当能写");
            assert_eq!(n, 3, "原有的两条别名必须还在: {n}");
            assert_eq!(table().resolve("k1"), Some("办公键盘"));
            assert_eq!(table().resolve("k3"), Some("新名字"));
        }

        // ② 内容真的坏了（非法 UTF-8 / 不是合法 JSON）：另存原文，从空表重新开始
        std::fs::write(&path, b"{ not json \xff\xfe ").expect("写坏文件失败");
        invalidate_cache();
        assert!(table().is_empty(), "坏文件不该让读路径 panic");
        set("k4", "重新开始").expect("坏文件应当可以自救（既有测试的语义保留）");
        assert!(
            path.with_extension("json.bad").is_file(),
            "原文要留一份，名字还有机会救回来"
        );
        assert_eq!(table().resolve("k4"), Some("重新开始"));
    }
}
