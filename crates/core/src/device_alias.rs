//! 设备别名：把自动解析出来的设备名（`HID 鼠标 · 24AE/1464`）换成用户看得懂的名字。
//!
//! 存储：`data/device_aliases.json`，形如 `{ "HID#VID_24AE&PID_1464&MI_00#7&...": "新鼠标" }`。
//! 纯文本、可直接手改；解析侧按文件 mtime 变化自动重载（无需重启）。
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
        let mut exact = HashMap::new();
        let mut model: HashMap<String, Option<String>> = HashMap::new();
        for (key, alias) in map {
            if alias.trim().is_empty() {
                continue;
            }
            exact.insert(key.clone(), alias.clone());
            if let Some(model_key) = model_key(key) {
                match model.get(&model_key) {
                    None => {
                        model.insert(model_key, Some(alias.clone()));
                    }
                    // 同型号已有不同别名 → 标记歧义
                    Some(Some(existing)) if existing != alias => {
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

    /// 解析某设备的展示名：精确 key → 型号回退，都没有则 None（调用方用自动名）。
    pub fn resolve(&self, device_key: &str) -> Option<&str> {
        if let Some(alias) = self.exact.get(device_key) {
            return Some(alias.as_str());
        }
        let model_key = model_key(device_key)?;
        self.model.get(&model_key)?.as_deref()
    }

    /// 已有别名（供 UI / CLI 展示与编辑回填）。
    pub fn exact_alias(&self, device_key: &str) -> Option<&str> {
        self.exact.get(device_key).map(|s| s.as_str())
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
/// 接口段区分复合设备的键鼠两面，理由同 [`model_key`]）。
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
        let identity = hardware_identity_key(key);
        if identity == *key {
            migrated.insert(key.clone(), alias.clone());
            continue;
        }
        changed = true;
        match migrated.get(&identity) {
            // 身份键已存在且来自别的旧路径：键序在前的赢（BTreeMap 迭代有序）
            Some(existing) if existing != alias => {
                tracing::warn!(
                    "别名迁移：{key} 与已有条目并到同一身份键 {identity}，保留「{existing}」、丢弃「{alias}」"
                );
            }
            Some(_) => {
                // 同名别名：并入即可，不用记
                let _ = &identity;
            }
            None => {
                migrated.insert(identity, alias.clone());
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

/// 写入别名：`alias` 为空白时等同删除。返回写入后的数量。
pub fn set(device_key: &str, alias: &str) -> anyhow::Result<usize> {
    let path = alias_path();
    let mut map = match try_read_map(&path) {
        AliasRead::Good(m) => m,
        AliasRead::Broken => BTreeMap::new(),
        AliasRead::Unreadable => return Err(alias_unreadable(&path)),
    };
    let trimmed = alias.trim();
    if trimmed.is_empty() {
        map.remove(device_key);
    } else {
        map.insert(device_key.to_string(), clamp_alias(trimmed));
    }
    write_map(&map)?;
    Ok(map.len())
}

/// 删除某设备别名，返回是否确有删除。
pub fn clear(device_key: &str) -> anyhow::Result<bool> {
    let path = alias_path();
    let mut map = match try_read_map(&path) {
        AliasRead::Good(m) => m,
        AliasRead::Broken => BTreeMap::new(),
        AliasRead::Unreadable => return Err(alias_unreadable(&path)),
    };
    let removed = map.remove(device_key).is_some();
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

/// 原子写回（临时文件 + rename），并立即失效缓存。
fn write_map(map: &BTreeMap<String, String>) -> anyhow::Result<()> {
    let path = alias_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(map)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)?;
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

    /// 型号回退：实例路径变了（换 USB 口），同 VID/PID 的别名仍生效。
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
                "同型号换端口后别名应回退命中"
            );
            // 精确别名不覆盖型号回退
            set(new_key, "第二只 G304").unwrap();
            assert_eq!(table().resolve(new_key), Some("第二只 G304"));
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
    #[test]
    fn ambiguous_model_aliases_do_not_fallback() {
        with_temp_dir("ambiguous", || {
            set("HID#VID_046D&PID_C52B#A", "旧鼠标").unwrap();
            set("HID#VID_046D&PID_C52B#B", "新鼠标").unwrap();
            let unknown = "HID#VID_046D&PID_C52B#C";
            assert_eq!(table().resolve(unknown), None, "歧义型号不应回退");
            // 但精确匹配照常
            assert_eq!(table().resolve("HID#VID_046D&PID_C52B#A"), Some("旧鼠标"));
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

    /// 无 VID/PID 的设备（触控板）只支持精确匹配。
    #[test]
    fn no_vid_pid_devices_only_exact_match() {
        with_temp_dir("novid", || {
            set("HID#MSFT0001&Col01#5&36f79095&0&0000", "触控板").unwrap();
            assert_eq!(
                table().resolve("HID#MSFT0001&Col01#5&36f79095&0&0000"),
                Some("触控板")
            );
            assert_eq!(table().resolve("HID#MSFT0001&Col01#另一个实例"), None);
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

    /// B14-2：别名文件的精确键跟着 device_key 换轨。
    #[test]
    fn alias_exact_keys_migrate_to_identity() {
        with_temp_dir("migrate_keys", || {
            let old_a = "HID#VID_046D&PID_C52B&MI_00#7&OLD&0&0000";
            let old_b = "HID#VID_046D&PID_C52B&MI_00#8&NEW&0&0001";
            let kb = "VID_1B1C&PID_1B2D"; // 已是身份形态：必须原样保留
            set(old_a, "办公鼠标").unwrap();
            set(old_b, "家里那把").unwrap();
            set(kb, "办公键盘").unwrap();
            invalidate_cache();

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
