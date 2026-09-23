//! 设备别名：把自动解析出来的设备名（`HID 鼠标 · 24AE/1464`）换成用户看得懂的名字。
//!
//! 存储：`data/device_aliases.json`，形如 `{ "HID#VID_24AE&PID_1464&MI_00#7&...": "新鼠标" }`。
//! 纯文本、可直接手改；解析侧按文件 mtime 变化自动重载（无需重启）。
//!
//! 匹配分两层（`AliasTable::resolve`）：
//! 1. **精确匹配** device_key（Raw Input 设备实例路径）
//! 2. 未命中时按 **VID/PID 型号** 回退 —— 同型号设备换 USB 口、接收器重插后实例路径会变，
//!    但型号不变，别名仍然生效。
//!    同型号出现多个**不同**别名时视为歧义，该型号不再参与回退（避免张冠李戴）。
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
            if let Some((vid, pid)) = parse_vid_pid(key) {
                let model_key = format!("{vid}/{pid}");
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
        let (vid, pid) = parse_vid_pid(device_key)?;
        self.model.get(&format!("{vid}/{pid}"))?.as_deref()
    }

    /// 已有别名（供 UI / CLI 展示与编辑回填）。
    pub fn exact_alias(&self, device_key: &str) -> Option<&str> {
        self.exact.get(device_key).map(|s| s.as_str())
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
    let map = read_map(&path);
    let table = AliasTable::from_map(&map);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    *cache = Some((mtime, table.clone(), path));
    table
}

/// 读取文件成有序 map（解析失败返回空，坏文件不影响主流程）。
fn read_map(path: &std::path::Path) -> BTreeMap<String, String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    match serde_json::from_str::<BTreeMap<String, String>>(&text) {
        Ok(map) => map,
        Err(e) => {
            tracing::error!("设备别名文件解析失败（已忽略）: {e}");
            BTreeMap::new()
        }
    }
}

/// 写入别名：`alias` 为空白时等同删除。返回写入后的数量。
pub fn set(device_key: &str, alias: &str) -> anyhow::Result<usize> {
    let mut map = read_map(&alias_path());
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
    let mut map = read_map(&alias_path());
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
}
