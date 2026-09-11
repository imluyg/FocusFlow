//! 配置管理。
//!
//! 镜像 Python 版 `config.py`：
//! - 读取/生成 `config.ini`（与 Python 版同格式，兼容用户既有配置）
//! - 缺失的 section/key 自动补默认值并回写
//! - 提供类型化读取 API 与线程安全的写入 API
//!
//! 注意：Python 版配置大量使用中文值与按键名，config crate 需按 UTF-8 处理。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::Duration;

use crate::paths;

/// 与 Python 版 `config.py` 中 DEFAULT_CONFIG 一致的默认配置。
pub fn default_config() -> HashMap<String, HashMap<String, String>> {
    let mut map = HashMap::new();
    let mut s = |section: &str, items: &[(&str, &str)]| {
        let inner: HashMap<String, String> = items
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        map.insert(section.to_string(), inner);
    };

    s(
        "database",
        &[
            ("flush_interval", "10"),
            ("backup_on_exit", "true"),
            ("online_backup_interval_hours", "24"),
            ("max_backups", "5"),
            ("auto_vacuum_days", "7"),
            ("yearly_archive", "true"),
        ],
    );
    s("stats", &[("cpm_window", "60")]);
    s(
        "app_stats",
        &[
            // 前台应用统计：enabled=false 关停；exclude 命中的进程完全不记录
            ("enabled", "true"),
            ("exclude", ""),
        ],
    );
    s(
        "listener",
        &[
            ("ignore_modifier_keys", "false"),
            ("ignore_function_keys", "false"),
            ("ignore_key_repeat", "true"),
            ("key_repeat_stale_seconds", "15"),
            ("mouse_enabled", "true"),
            ("scroll_burst_window", "0.8"),
        ],
    );
    s(
        "gui",
        &[
            ("refresh_interval", "2"),
            ("full_refresh_interval", "10"),
            ("show_first_run_tip", "true"),
            ("theme", "light"),
            ("show_trend_chart", "true"),
            ("show_key_groups", "true"),
            ("start_to_tray", "true"),
            ("font", "hei"),
        ],
    );
    s(
        "hotkey",
        &[("enabled", "false"), ("toggle_window", "ctrl+shift+f")],
    );
    s("floating", &[("enabled", "true"), ("opacity", "0.85")]);
    s("tray", &[("tooltip_interval", "5")]);
    s(
        "pomodoro",
        &[
            ("enabled", "true"),
            ("work_minutes", "25"),
            ("break_minutes", "5"),
            ("auto_break", "true"),
        ],
    );
    s(
        "rest",
        &[
            ("enabled", "true"),
            ("window_minutes", "30"),
            ("key_threshold", "10000"),
            ("cooldown_minutes", "10"),
            ("rest_seconds", "20"),
            ("check_interval", "10"),
        ],
    );
    map
}

/// 明确废弃的配置键（section -> [key...]）：load 时仅清理这些键，
/// 保留所有其他键（含运行时动态写入的合法键，如 [floating] width/height/pos_x/pos_y）。
const DEPRECATED_CONFIG: &[(&str, &[&str])] = &[
    // 已移除：今日计数用写入线程内存缓存，此键不再读取
    ("stats", &["today_count_cache_ttl"]),
];

/// 线程安全的配置管理器。
///
/// 通过 `FocusFlowConfig::instance()` 获得进程级单例（镜像 Python 的全局 `config`）。
pub struct FocusFlowConfig {
    /// section -> (key -> value)
    values: Mutex<HashMap<String, HashMap<String, String>>>,
    path: PathBuf,
}

impl FocusFlowConfig {
    /// 从指定路径加载配置；`path` 不存在时生成默认配置并保存。
    pub fn load(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = path.into();
        let defaults = default_config();
        let mut values = defaults.clone();

        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    // 解析 INI：兼容 Python configparser 的 `#`/`;` 注释与 `key = value` 语法。
                    let mut current_section: Option<String> = None;
                    for line in text.lines() {
                        let line = line.trim();
                        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                            continue;
                        }
                        if line.starts_with('[') && line.ends_with(']') {
                            current_section = Some(line[1..line.len() - 1].trim().to_string());
                            continue;
                        }
                        let Some(section) = current_section.clone() else {
                            continue;
                        };
                        if let Some(eq) = line.find('=') {
                            let key = line[..eq].trim().to_string();
                            let val = line[eq + 1..].trim().to_string();
                            // 去掉可能带有的引号
                            let val = val
                                .strip_prefix('"')
                                .and_then(|v| v.strip_suffix('"'))
                                .unwrap_or(&val)
                                .to_string();
                            values.entry(section.clone()).or_default().insert(key, val);
                        }
                    }
                }
                Err(e) => {
                    // 文件存在但读取失败（编码损坏/被占用）：先把原文件改名备份，
                    // 避免后续 save() 用默认值覆盖后用户配置彻底丢失。
                    let ts = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let mut name = path
                        .file_name()
                        .map(|s| s.to_os_string())
                        .unwrap_or_default();
                    name.push(format!(".corrupt-{ts}"));
                    let backup = path.with_file_name(name);
                    match std::fs::rename(&path, &backup) {
                        Ok(_) => tracing::error!(
                            "配置文件读取失败（{e}），原文件已备份到 {}，本次使用默认值",
                            backup.display()
                        ),
                        Err(_) => {
                            tracing::error!("配置文件读取失败（{e}）且备份失败，本次使用默认值")
                        }
                    }
                }
            }
        }

        // 仅清理明确废弃的配置键。注意：不能按"默认配置白名单"清理，
        // 否则会误删运行时动态写入的合法键（如 [floating] width/height/pos_x/pos_y），
        // 导致悬浮窗位置/尺寸无法在重启后保留。
        for (section, keys) in DEPRECATED_CONFIG {
            if let Some(map) = values.get_mut(*section) {
                for k in *keys {
                    map.remove(*k);
                }
            }
        }

        let cfg = Self {
            values: Mutex::new(values),
            path,
        };
        cfg.save()?;
        Ok(cfg)
    }

    /// 保存当前配置到文件（缺失 section/key 已补默认值）。
    ///
    /// 仅在锁内做快照，序列化与写盘在锁外完成：
    /// 落盘期间的磁盘 IO 不会阻塞热路径（键鼠监听/统计线程）的配置读取。
    pub fn save(&self) -> anyhow::Result<()> {
        let snapshot: HashMap<String, HashMap<String, String>> = self
            .values
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut out = String::new();
        // 固定 section 顺序，与 Python 版一致，便于阅读与 diff。
        let order = [
            "database", "stats", "listener", "gui", "hotkey", "floating", "tray", "pomodoro",
            "rest",
        ];
        let mut sections: Vec<&String> = snapshot.keys().collect();
        sections.sort_by_key(|s| order.iter().position(|o| o == s).unwrap_or(usize::MAX));
        for section in sections {
            out.push_str(&format!("[{section}]\n"));
            let mut keys: Vec<&String> = snapshot[section].keys().collect();
            keys.sort();
            for key in keys {
                out.push_str(&format!("{} = {}\n", key, snapshot[section][key]));
            }
            out.push('\n');
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&self.path, out)?;
        Ok(())
    }

    /// 仅内存的配置实例（加载/落盘失败时的兜底，保证应用可用，只损失持久化）。
    fn in_memory(path: PathBuf) -> Self {
        Self {
            values: Mutex::new(default_config()),
            path,
        }
    }

    // ---------- 读取 API ----------

    fn get_raw(&self, section: &str, key: &str) -> String {
        let values = self.values.lock().unwrap_or_else(|e| e.into_inner());
        values
            .get(section)
            .and_then(|s| s.get(key))
            .cloned()
            .unwrap_or_default()
    }

    /// 读取字符串值，缺省返回空串。
    pub fn get(&self, section: &str, key: &str) -> String {
        self.get_raw(section, key)
    }

    /// 读取字符串值，缺省返回 `default`。
    pub fn get_or(&self, section: &str, key: &str, default: &str) -> String {
        let v = self.get_raw(section, key);
        if v.is_empty() {
            default.to_string()
        } else {
            v
        }
    }

    /// 读取整数。
    pub fn get_int(&self, section: &str, key: &str, default: i64) -> i64 {
        self.get_raw(section, key)
            .trim()
            .parse::<i64>()
            .unwrap_or(default)
    }

    /// 读取浮点数。
    pub fn get_float(&self, section: &str, key: &str, default: f64) -> f64 {
        self.get_raw(section, key)
            .trim()
            .parse::<f64>()
            .unwrap_or(default)
    }

    /// 读取布尔值（`true/1/yes/on` 视为真）。
    pub fn get_bool(&self, section: &str, key: &str, default: bool) -> bool {
        let v = self.get_raw(section, key).trim().to_lowercase();
        match v.as_str() {
            "true" | "1" | "yes" | "on" => true,
            "false" | "0" | "no" | "off" => false,
            "" => default,
            _ => default,
        }
    }

    // ---------- 写入 API ----------

    /// 设置字符串值并持久化。
    pub fn set(&self, section: &str, key: &str, value: &str) -> anyhow::Result<()> {
        {
            let mut values = self.values.lock().unwrap_or_else(|e| e.into_inner());
            values
                .entry(section.to_string())
                .or_default()
                .insert(key.to_string(), value.to_string());
        }
        // 去抖持久化：合并 300ms 窗口内的多次写入，避免高频调用（如悬浮窗位置）频繁整文件重写。
        let _ = saver_tx().send(());
        Ok(())
    }
}

/// 全局配置单例（与 Python 版全局 `config` 对应）。
///
/// 首次访问时加载，仅一次。
pub static INSTANCE: OnceLock<FocusFlowConfig> = OnceLock::new();

/// 配置保存信号通道：`set` 写入内存后向后台线程发信号，去抖后落盘。
static SAVE_TX: OnceLock<mpsc::Sender<()>> = OnceLock::new();

/// 获取（必要时启动）配置保存线程，返回信号发送端。
fn saver_tx() -> &'static mpsc::Sender<()> {
    SAVE_TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<()>();
        std::thread::Builder::new()
            .name("config-saver".into())
            .spawn(move || loop {
                if rx.recv().is_err() {
                    break;
                }
                // 收集 300ms 内的连续写请求，合并为一次落盘
                while rx.recv_timeout(Duration::from_millis(300)).is_ok() {}
                if let Some(cfg) = INSTANCE.get() {
                    let _ = cfg.save();
                }
            })
            .ok();
        tx
    })
}

/// 获取全局配置实例；未初始化时用默认路径加载并初始化。
///
/// 加载失败（权限/磁盘满等）不 panic：降级为仅内存默认值并记录错误日志，
/// 保证应用仍可启动；`set` 的落盘重试会继续尝试恢复持久化。
pub fn instance() -> &'static FocusFlowConfig {
    INSTANCE.get_or_init(|| {
        let path = paths::config_path();
        match FocusFlowConfig::load(&path) {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::error!("配置加载失败，使用默认值继续运行: {e:#}");
                FocusFlowConfig::in_memory(path)
            }
        }
    })
}

/// 显式初始化配置（供需要自定义路径/错误处理的场景）。
pub fn init_with_path(path: impl AsRef<Path>) -> anyhow::Result<&'static FocusFlowConfig> {
    // 先构造，再放入 OnceLock
    let cfg = FocusFlowConfig::load(path.as_ref().to_path_buf())?;
    let _ = INSTANCE.set(cfg);
    Ok(INSTANCE.get().expect("INSTANCE 刚刚设置必然存在"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn defaults_and_overrides() {
        let dir = std::env::temp_dir().join("ff_rs_cfg_test");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("config.ini");
        std::fs::write(&path, "[gui]\ntheme = dark\nrefresh_interval = 5\n").unwrap();

        let cfg = FocusFlowConfig::load(&path).unwrap();
        // 覆盖值
        assert_eq!(cfg.get("gui", "theme"), "dark");
        assert_eq!(cfg.get_int("gui", "refresh_interval", 0), 5);
        // 默认值补齐
        assert!(cfg.get_bool("listener", "ignore_key_repeat", false));
        assert_eq!(cfg.get("hotkey", "toggle_window"), "ctrl+shift+f");
        assert_eq!(cfg.get_int("rest", "key_threshold", 0), 10000);
        assert_eq!(cfg.get_float("floating", "opacity", 0.0), 0.85);

        let _ = Arc::new(());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn runtime_keys_preserved_and_deprecated_removed() {
        // 模拟真实 config.ini：含运行时动态键（悬浮窗位置/尺寸）
        // 与已废弃键（today_count_cache_ttl）。
        let dir = std::env::temp_dir().join("ff_rs_cfg_prune_test");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("config.ini");
        std::fs::write(
            &path,
            "[stats]
today_count_cache_ttl = 10
             [floating]
pos_x = 1488
pos_y = 165
width = 90
height = 46
             [gui]
theme = light
",
        )
        .unwrap();

        let cfg = FocusFlowConfig::load(&path).unwrap();

        // 运行时合法键必须被保留（否则悬浮窗位置/尺寸无法持久化）
        assert_eq!(cfg.get_float("floating", "pos_x", f64::NAN), 1488.0);
        assert_eq!(cfg.get_float("floating", "pos_y", f64::NAN), 165.0);
        assert_eq!(cfg.get_float("floating", "width", f64::NAN), 90.0);
        assert_eq!(cfg.get_float("floating", "height", f64::NAN), 46.0);

        // 已废弃键应从内存移除
        assert!(cfg.get("stats", "today_count_cache_ttl").is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }
}
