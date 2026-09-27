//! 配置管理。
//!
//! 镜像 Python 版 `config.py`：
//! - 读取/生成 `config.ini`（与 Python 版同格式，兼容用户既有配置）
//! - 缺失的 section/key 自动补默认值并回写
//! - 提供类型化读取 API 与线程安全的写入 API
//!
//! 注意：Python 版配置大量使用中文值与按键名，config crate 需按 UTF-8 处理。

use std::collections::{HashMap, HashSet};
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
            // 已停用插件的数据默认不备份（它们的库不再被写入，备了只占轮转名额）；
            // 想连它们一起备就设 true。代码注释里提了这个开关，但默认值里没有，
            // 不读源码的人不知道能设（同 default_period 那一类）。
            ("backup_disabled_plugins", "false"),
        ],
    );
    s("stats", &[("cpm_window", "60")]);
    // 数据目录（`data/` 与 `backup/` 挂在哪里）。留空 = 跟程序目录走，即"拷走整个
    // 文件夹即迁移"的默认形态；填一个绝对路径就把数据挪到那儿，改完由程序自己
    // 拷贝已有数据并重启生效（读者：paths::data_home）。日志/插件/配置仍留在程序目录。
    s(
        "paths",
        &[
            ("data_home", ""),
            // 待搬运的**旧**数据根：非空 = 下次启动要先把它下面的 data/ 与 backup/
            // 搬进 data_home（读者：data_location::run_pending_migration）。
            // 「更改数据文件夹」只写这两个键然后重启；搬成功就清零，失败就留着，
            // 下次启动重试 —— 源在搬成功并逐文件核对之前一个字都不动。
            ("data_migrate_from", ""),
        ],
    );
    // 插件沙箱的两个上限（manager.rs 读；超限的插件会被标记为不可用）
    s(
        "plugins",
        &[("memory_limit_mb", "16"), ("instruction_limit", "10000000")],
    );
    // 设备维度采集的总开关（device_stats.rs 读它；关掉只停采集，历史不动）
    s("device_stats", &[("enabled", "true")]);
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
            ("scroll_burst_window", "0.25"),
        ],
    );
    s(
        "gui",
        &[
            // 图表刷新节奏：打字时每 active_refresh_interval 秒、空闲时每
            // full_refresh_interval 秒（两个都有读点，见 desktop/src/state.rs）
            ("active_refresh_interval", "2"),
            ("full_refresh_interval", "10"),
            ("theme", "light"),
            ("start_to_tray", "true"),
            // 主窗口隐藏够久就卸载渲染进程（省几百 MB；false = 一直留着）
            ("unload_hidden", "true"),
            ("unload_hidden_delay", "60"),
            // 悬浮窗显示口径：times / duration / both（desktop/ui/floating.js 读）
            ("floating_metric", "times"),
            // 启动时选中的统计周期（-1 = 今日）：前端每次切换都写回它，
            // 用来"记住上次退出前看的周期"（desktop/src/state.rs 读，非法值在入口拦掉）。
            // 和 floating_metric 同一类：代码一直在读、默认值里却没有，
            // 等于只有会手改 config.ini 的人知道有这么个开关。
            ("default_period", "-1"),
        ],
    );
    s(
        "hotkey",
        &[("enabled", "false"), ("toggle_window", "ctrl+shift+f")],
    );
    s("floating", &[("enabled", "true")]);
    s(
        "pomodoro",
        &[
            ("work_minutes", "25"),
            ("break_minutes", "5"),
            ("auto_break", "true"),
        ],
    );
    // 久坐提醒：六个键全部有读点（core::stats::RestMonitor，判定在统计线程里跑，
    // 改完不必重启）。番茄钟的"休息"是工作周期的一部分，这里是**没开番茄钟时**
    // 也会催你一下的那一条。
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
    // 以下这些是「每次启动都写进 config.ini、但全仓一个读点都没有」的假开关：
    // 用户改了没反应，比压根没有这个键更糟（同一类问题上一场已经处理过
    // [pomodoro] 的时长三键）。老文件里的这些键在 load 时清掉。
    (
        "gui",
        &[
            "refresh_interval",
            "show_first_run_tip",
            "show_trend_chart",
            "show_key_groups",
            "font",
        ],
    ),
    ("floating", &["opacity"]),
    ("tray", &["tooltip_interval"]),
    // 番茄钟真正的开关是 `[plugins] disabled`（整节插件停用），这个键从来没被读过
    ("pomodoro", &["enabled"]),
    ("database", &["batch_size"]),
];

/// 解析 INI：兼容 Python configparser 的 `#`/`;` 注释与 `key = value` 语法。
///
/// 对 crate 内开放：`paths::data_home` 要在配置单例就绪之前从 config.ini 里抠出
/// `[paths] data_home` 一个键，用这份现成的解析器（含 BOM、引号、注释的处理）而不是
/// 再手搓一个——两者对同一个文件给出不同结论，比只有一个解析器更糟。
///
/// 开头的 BOM 必须先剥掉：PowerShell 5.1 的 `>`/`Out-File`、记事本另存为 UTF-8 都会
/// 写一个 U+FEFF，而它**不算空白**（`char::is_whitespace` 为 false），`trim()` 去不掉。
/// 一行是不是节头；是的话返回节名。
///
/// 判据原来两处都写成 `starts_with('[') && ends_with(']')`，于是 `[gui] # 备注`
/// **不算节头**：解析时 `current_section` 还停在上一节，紧随其后的 `theme = dark`
/// 被记成 `database.theme`；回写时同一判据把那一行原样留在 `[database]` 的缓冲里，
/// 而 `[gui]` 整节没在文件里出现过 ⇒ 走"追加到文件末尾"，写下默认的 `theme = light`。
/// 净效果：用户的行被复制进错误的节、并被默认值盖掉（下次 parse 后者赢）。
/// Python 的 `configparser` 用 `re.match`，尾注是被接受的 —— 与本文件"同格式、
/// 兼容用户既有配置"的承诺相反。`]` 之后只允许空白或注释起始符（`#` / `;`），
/// 别的字符（例如 `[gui]x`）仍然不算节头。
fn section_header(line: &str) -> Option<&str> {
    let s = line.trim();
    if !s.starts_with('[') {
        return None;
    }
    let close = s.find(']')?;
    let tail = s[close + 1..].trim();
    if tail.is_empty() || tail.starts_with('#') || tail.starts_with(';') {
        Some(s[1..close].trim())
    } else {
        None
    }
}

/// 留着的话首行是 `\u{FEFF}[database]`，不是合法的 section 头 → 第一个 section 的
/// 键全被丢掉；而 `load()` 结尾无条件 `save()`，于是用户自己的 `[database]` 配置
/// 直接被默认值覆盖回写进文件 —— 静默丢配置，不只是这次读错。
pub(crate) fn parse_ini(text: &str) -> HashMap<String, HashMap<String, String>> {
    let mut out: HashMap<String, HashMap<String, String>> = HashMap::new();
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut current_section: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(section) = section_header(line) {
            current_section = Some(section.to_string());
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
            out.entry(section).or_default().insert(key, val);
        }
    }
    out
}

/// 逐行重写：**只替换自己认识的键行**，其余一律逐字保留。
///
/// 旧实现是「从内存快照整份重建」：用户手写的注释行、空行、节内顺序全部被抹掉，
/// 节头之前游离的键也会被丢。本项目到处是"你去 config.ini 里加一行"的提示语
/// （`[scheduler] allow_extra` 就是 scheduler 拒绝启动某程序时给的话），用户照着
/// 加的任何一行、写的任何一条注释，都不该被下一次保存抹掉。
///
/// 规则：
/// - 注释行 / 空行 / 解析不了的杂行 / 文件里有而内存里没有的键（含游离在节头前的）→ 原样保留；
/// - 内存里认识的键 → 原位替换成 `key = value`（值以内存为准，规则与旧 `merge_unknown_keys` 一致）；
/// - 废弃键（[`DEPRECATED_CONFIG`]）→ 整行丢掉：`load()` 刻意清理它们，回写等于复活；
/// - 快照里有、文件里没有的键 → 追加到所在**节尾**；整节都没有 → 追加到文件末尾
///   （有固定序的按固定序，其余按名字排 —— 旧重建用 HashMap 迭代序，每次保存都可能变）；
/// - 键全被清掉的节连节头一起丢（留个空节头只会让人以为那里还有什么可配）；
///   还留着注释/未知键的节保留节头 —— 不替用户做主删他写的东西。
fn serialize_preserving_structure(
    snapshot: &HashMap<String, HashMap<String, String>>,
    original: &str,
) -> String {
    // 开头 BOM 先剥掉（PowerShell `>`、记事本另存为都会写，见 parse_ini 的注释）：
    // 保留它等于每次回写都把 `\u{FEFF}` 带回文件开头。
    let original = original.strip_prefix('\u{feff}').unwrap_or(original);
    let mut out = String::new();
    // 已在文件里输出过的键：节尾追加"新增键"时靠它排除
    let mut seen_keys: HashSet<(String, String)> = HashSet::new();
    let mut seen_sections: HashSet<String> = HashSet::new();
    // 当前节的输出缓冲。节头去留要等整个节扫完才知道（键可能全被废弃清掉），
    // 所以先攒着，节结束（下一个节头/EOF）时统一结算。
    let mut cur: Option<(String, Vec<String>)> = None;

    for line in original.lines() {
        let trimmed = line.trim();
        if let Some(section) = section_header(trimmed) {
            if let Some((section, lines)) = cur.take() {
                flush_section(
                    &mut out,
                    &mut seen_keys,
                    &mut seen_sections,
                    snapshot,
                    section,
                    lines,
                );
            }
            cur = Some((section.to_string(), vec![line.to_string()]));
            continue;
        }
        let Some((section, lines)) = cur.as_mut() else {
            // 节头之前的行：旧实现整行丢掉。逐字保留是本次要修的一部分。
            out.push_str(line);
            out.push('\n');
            continue;
        };
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            lines.push(line.to_string());
            continue;
        }
        let Some(eq) = trimmed.find('=') else {
            lines.push(line.to_string());
            continue;
        };
        let key = trimmed[..eq].trim();
        if DEPRECATED_CONFIG
            .iter()
            .any(|(s, ks)| *s == section.as_str() && ks.contains(&key))
        {
            continue;
        }
        match snapshot.get(section.as_str()).and_then(|m| m.get(key)) {
            Some(v) => {
                lines.push(format!("{key} = {v}"));
                seen_keys.insert((section.clone(), key.to_string()));
            }
            // 文件里有、内存里没有（load 之后才手加进文件的键走这里）：原样保留
            None => lines.push(line.to_string()),
        }
    }
    if let Some((section, lines)) = cur.take() {
        flush_section(
            &mut out,
            &mut seen_keys,
            &mut seen_sections,
            snapshot,
            section,
            lines,
        );
    }

    // 快照里还有整节没出现过的：追加到文件末尾
    let order = [
        "database", "stats", "paths", "listener", "gui", "hotkey", "floating", "pomodoro", "rest",
    ];
    let mut remaining: Vec<&String> = snapshot
        .keys()
        .filter(|s| !seen_sections.contains(*s))
        .collect();
    remaining.sort_by(|a, b| {
        let pa = order.iter().position(|o| o == *a).unwrap_or(usize::MAX);
        let pb = order.iter().position(|o| o == *b).unwrap_or(usize::MAX);
        pa.cmp(&pb).then_with(|| a.cmp(b))
    });
    for section in remaining {
        let keys = &snapshot[section.as_str()];
        if keys.is_empty() {
            continue;
        }
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str(&format!("[{section}]\n"));
        let mut sorted: Vec<&String> = keys.keys().collect();
        sorted.sort();
        for key in sorted {
            out.push_str(&format!("{key} = {}\n", keys[key.as_str()]));
        }
    }
    out
}

/// 结算一个节：追加快照里这个节还没输出过的键，然后决定节头去留、写进 out。
fn flush_section(
    out: &mut String,
    seen_keys: &mut HashSet<(String, String)>,
    seen_sections: &mut HashSet<String>,
    snapshot: &HashMap<String, HashMap<String, String>>,
    section: String,
    mut lines: Vec<String>,
) {
    seen_sections.insert(section.clone());
    if let Some(keys) = snapshot.get(&section) {
        let mut new_keys: Vec<&String> = keys
            .keys()
            .filter(|k| !seen_keys.contains(&(section.clone(), (*k).clone())))
            .collect();
        new_keys.sort();
        for key in new_keys {
            lines.push(format!("{key} = {}", keys[key.as_str()]));
        }
    }
    // 节头之外还有内容（键/注释，空行不算）才保留整节
    let has_content = lines[1..].iter().any(|l| !l.trim().is_empty());
    if !has_content {
        return;
    }
    for line in lines {
        out.push_str(&line);
        out.push('\n');
    }
}

/// 整份从内存重建（旧 save 的行为）。只在原文件读不到时兜底：
/// 那种情况下逐行重写同样没有输入可用，注释/未知键本来就已无处可保。
fn serialize_rebuilt(snapshot: &HashMap<String, HashMap<String, String>>) -> String {
    let mut out = String::new();
    // 固定 section 顺序，与 Python 版一致，便于阅读与 diff。
    let order = [
        "database", "stats", "paths", "listener", "gui", "hotkey", "floating", "pomodoro", "rest",
    ];
    let mut sections: Vec<&String> = snapshot.keys().collect();
    sections.sort_by_key(|s| order.iter().position(|o| o == s).unwrap_or(usize::MAX));
    for section in sections {
        let mut keys: Vec<&String> = snapshot[section].keys().collect();
        if keys.is_empty() {
            // 一个键都没有就整节不写：清掉废弃键之后可能把一整节掏空
            // （`[tray]` 以前只剩 tooltip_interval 一个死键），留个空节头
            // 只是让人以为那里还有什么可配。
            continue;
        }
        out.push_str(&format!("[{section}]\n"));
        keys.sort();
        for key in keys {
            out.push_str(&format!("{} = {}\n", key, snapshot[section][key]));
        }
        out.push('\n');
    }
    out
}

/// 首次「逐行重写」前把原文件留一份 `.bak`（只留第一份，之后不覆盖）。
///
/// 这是对唯一一份用户配置的结构性改动：`atomic_write` 只保证"不写半截"，
/// 不保证"写出来的内容一定对"。留一份改动前的原貌，最坏情况照它手工恢复。
fn backup_original_once(path: &Path, original: &str) {
    let mut name = path
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    name.push(".bak");
    let bak = path.with_file_name(name);
    if bak.exists() {
        return;
    }
    if let Err(e) = std::fs::write(&bak, original) {
        tracing::warn!("配置备份（{}）写入失败: {e}", bak.display());
    }
}

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
                    for (section, keys) in parse_ini(&text) {
                        values.entry(section).or_default().extend(keys);
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
        // 结尾这次回写只是把"补齐的默认键"落到盘上，它失败**不能**推翻已经读出来、
        // 已经 reconcile 好的那一份。原来的 `cfg.save()?` 会让整个 `load()` 报错，
        // 而 `instance()` 的兜底臂是 `in_memory(path)` = `default_config()` ⇒
        // 本次运行全程跑默认值，接着下一笔成功的 save（退出前强制落盘、改数据目录、
        // 任意 `set()` 唤醒的去抖保存）就拿这份默认快照逐行重写**真实**的 config.ini：
        // `data_home` 被写空（下次启动数据根回到程序目录，真历史静默失联）、
        // `[app_stats] exclude` 那道隐私保险丝失效、`[plugins] disabled` 复原。
        // 读成功而写失败在 Windows 上是常态：另一个实例握着文件、记事本开着它、
        // 杀软首扫 —— 所以这条链随时走得通，不是假想形状。
        if let Err(rewrite) = cfg.save() {
            tracing::warn!(
                "配置已读出来，但补齐默认键的回写没成功（本次运行仍按读出来的值，不会拿默认值盖掉文件）: {rewrite:#}"
            );
        }
        Ok(cfg)
    }

    /// 保存当前配置到文件（缺失 section/key 已补默认值）。
    ///
    /// 仅在锁内做快照，序列化与写盘在锁外完成：
    /// 落盘期间的磁盘 IO 不会阻塞热路径（键鼠监听/统计线程）的配置读取。
    pub fn save(&self) -> anyhow::Result<()> {
        // 把「快照 → 序列化 → rename」整体串行化。
        //
        // 落盘刻意不能放在 `values` 锁里做（否则磁盘 IO 会阻塞键鼠监听/统计线程读配置），
        // 但一旦出了锁，两个并发 save 的 **rename 先后** 就和 **快照新旧** 再无关系：
        // 拿着旧快照的线程只要 IO 慢一点，就会把另一个线程刚写好的新配置整个盖回去，
        // 设置静默丢失。调用方确实有两个 —— 去抖的 config-saver 线程，和退出前
        // `RunEvent::Exit` 里主线程那次强制 save（改完设置立刻关窗口时正好撞上）。
        let _write_guard = SAVE_WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let snapshot: HashMap<String, HashMap<String, String>> = self
            .values
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        // 原文件读得到 → 逐行重写（保注释/空行/节序/未知键，见函数注释）；
        // 读不到（被占用/刚被删）→ 退回整份重建：那种情况下逐行重写同样没有
        // 输入可用，"文件里有而内存没有"的键本来就已无处可保，与旧行为一致。
        let out = match std::fs::read_to_string(&self.path) {
            Ok(original) => {
                backup_original_once(&self.path, &original);
                serialize_preserving_structure(&snapshot, &original)
            }
            Err(_) => serialize_rebuilt(&snapshot),
        };
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        atomic_write(&self.path, &out)?;
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
        // 保存线程死了（spawn 失败/通道断）send 会一直报错：热路径只喊一次，
        // 别让悬浮窗每动一下就刷一条。
        if saver_tx().send(()).is_err() {
            warn_saver_dead_once();
        }
        Ok(())
    }
}

/// 原子写入：先写同目录临时文件并 fsync，再 rename 替换目标
/// （Windows 上 rename 同样会替换已存在的目标）。
/// 直接 `fs::write` 截断重写，掉电/崩溃会留下空文件或半截 INI；
/// 原子替换保证任意时刻磁盘上的 config.ini 要么是旧的完整内容，要么是新的。
fn atomic_write(path: &Path, contents: &str) -> anyhow::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let mut name = path
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    // 每次调用唯一后缀：并发 save 各用各的临时文件，避免一个 rename
    // 把另一个的临时文件"偷走"后报"找不到文件"。
    name.push(format!(
        ".tmp-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = path.with_file_name(name);
    let write = || -> anyhow::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
        Ok(())
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(anyhow::Error::new(e).context("配置文件原子替换失败"))
        }
    }
}

/// 全局配置单例（与 Python 版全局 `config` 对应）。
///
/// 首次访问时加载，仅一次。
pub static INSTANCE: OnceLock<FocusFlowConfig> = OnceLock::new();

/// 配置落盘的全局串行锁：保证后完成的写一定基于不早于它的快照。
/// 进程内只有一个配置实例（`INSTANCE`），所以全局锁等价于"每个文件一把"。
static SAVE_WRITE_LOCK: Mutex<()> = Mutex::new(());

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
                    if let Err(e) = cfg.save() {
                        tracing::error!("配置落盘失败: {e:#}");
                    }
                }
            })
            // 全仓曾经唯一一处裸 `.ok()`：spawn 失败连日志都没有，此后所有
            // 设置改动静默不落盘。失败记下；set() 侧还有发送失败的第二道闸。
            .map_err(|e| {
                tracing::error!(
                    "配置保存线程启动失败（{e}）：本次运行的设置改动将不再落盘，重启程序可恢复"
                )
            })
            .ok();
        tx
    })
}

/// 配置保存线程死了只喊一次：`set` 每次都会 send，逐条 warn 会把日志刷爆
/// （同 `warn_bad_clock_once` 的热路径先例）。
fn warn_saver_dead_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        tracing::error!("配置保存线程不可用：本次运行的设置改动不会落盘（重启程序可恢复）");
    });
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
        std::fs::write(&path, "[gui]\ntheme = dark\nactive_refresh_interval = 5\n").unwrap();

        let cfg = FocusFlowConfig::load(&path).unwrap();
        // 覆盖值
        assert_eq!(cfg.get("gui", "theme"), "dark");
        assert_eq!(cfg.get_int("gui", "active_refresh_interval", 0), 5);
        // 默认值补齐
        assert!(cfg.get_bool("listener", "ignore_key_repeat", false));
        assert_eq!(cfg.get("hotkey", "toggle_window"), "ctrl+shift+f");
        assert_eq!(cfg.get_int("rest", "key_threshold", 0), 10000);
        // 「代码在读、但默认值里没有」的键必须也能在这里看到：它们在界面上是
        // 真开关，藏在 config.ini 里不可发现就是文档缺失（floating_metric 之前
        // 就是这种状态，UI 里那个口径切换只有会改文件的人才用得到）。
        assert_eq!(cfg.get("gui", "floating_metric"), "times");
        assert_eq!(cfg.get_int("gui", "default_period", 99), -1);
        // 插件沙箱上限与"停用插件要不要一起备份"也都要在文件里看得见
        assert_eq!(cfg.get_int("plugins", "memory_limit_mb", 0), 16);
        assert_eq!(cfg.get_int("plugins", "instruction_limit", 0), 10_000_000);
        assert!(!cfg.get_bool("database", "backup_disabled_plugins", true));
        assert!(cfg.get_bool("gui", "unload_hidden", false));
        assert_eq!(cfg.get_int("gui", "unload_hidden_delay", 0), 60);
        assert!(cfg.get_bool("device_stats", "enabled", false));

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
opacity = 0.5
             [gui]
theme = light
refresh_interval = 3
show_key_groups = false
font = hei
             [tray]
tooltip_interval = 5
             [pomodoro]
enabled = false
work_minutes = 45
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
        // 「写进文件却全仓没人读」的假开关同族：读不到，也不许被回填
        for (section, key) in [
            ("floating", "opacity"),
            ("gui", "refresh_interval"),
            ("gui", "show_key_groups"),
            ("gui", "font"),
            ("tray", "tooltip_interval"),
            ("pomodoro", "enabled"),
        ] {
            assert!(
                cfg.get(section, key).is_empty(),
                "{section}.{key} 已无读点，必须被清掉"
            );
        }
        // 同前缀的活键不能被顺手带走：full_refresh_interval 有读点，
        // work_minutes 是番茄钟真正生效的时长
        assert_eq!(cfg.get_int("gui", "full_refresh_interval", 0), 10);
        assert_eq!(cfg.get_int("pomodoro", "work_minutes", 0), 45);
        assert_eq!(cfg.get_int("gui", "active_refresh_interval", 0), 2);

        // 而且不能只清内存：load() 结尾无条件 save()，文件里也不该再留着它们
        let on_disk = std::fs::read_to_string(&path).unwrap();
        let dead: Vec<&str> = on_disk
            .lines()
            .map(str::trim)
            .filter(|l| {
                [
                    "opacity",
                    "refresh_interval",
                    "tooltip_interval",
                    "show_key_groups",
                    "font",
                    "today_count_cache_ttl",
                ]
                .iter()
                .any(|k| l.starts_with(k))
            })
            .collect();
        assert!(dead.is_empty(), "回写的文件里还留着死键: {dead:?}");
        assert!(
            on_disk.contains("work_minutes = 45"),
            "活键必须照旧留在文件里: {on_disk}"
        );
        assert!(
            !on_disk.contains("[tray]"),
            "清空之后的 [tray] 节不该被回写"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// BOM 不许吃掉第一个 section —— 吃掉之后还会被回写坐实成"配置没了"。
    ///
    /// PowerShell 5.1 的 `>` / `Out-File`、记事本的"另存为 UTF-8"都会写 BOM，
    /// 而 `[database]` 正好是本项目 config.ini 的第一个 section。
    #[test]
    fn utf8_bom_does_not_eat_the_first_section() {
        let _lock = crate::paths::test_app_dir_lock();
        let dir = crate::paths::test_app_dir("cfg_bom");
        let path = dir.path().join("config.ini");
        std::fs::write(
            &path,
            "\u{feff}[database]\nmax_backups = 99\n[gui]\ntheme = dark\n",
        )
        .unwrap();

        let cfg = FocusFlowConfig::load(&path).unwrap();
        assert_eq!(
            cfg.get_int("database", "max_backups", 0),
            99,
            "BOM 之后第一个 section 的键必须读得到"
        );
        assert_eq!(cfg.get("gui", "theme"), "dark");
        // load() 结尾会 save() 一次：读不到就会被默认值覆盖回写进文件，
        // 所以这里查的是**文件**而不是内存 —— 用户下次打开看到的正是它。
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("max_backups = 99"),
            "用户配置不该被默认值盖掉:\n{after}"
        );
        assert!(
            !after.starts_with('\u{feff}'),
            "回写不必再把 BOM 带回去:\n{after}"
        );
    }

    /// 用户照提示语手加的行，必须活得过程序自己的每一次保存。
    ///
    /// `save()` 写的是内存快照，而代码里到处是"去 config.ini 加一行"的提示
    /// （`[scheduler] allow_extra` 就是 scheduler 拒绝启动某程序时给的话）。
    #[test]
    fn hand_added_keys_survive_program_saves() {
        let _lock = crate::paths::test_app_dir_lock();
        let dir = crate::paths::test_app_dir("cfg_handedit");
        let path = dir.path().join("config.ini");
        std::fs::write(&path, "[stats]\ntoday_count_cache_ttl = 10\n").unwrap();

        let cfg = FocusFlowConfig::load(&path).unwrap();
        // 关键在"启动之后"：程序已经在跑了，用户这才照提示语往文件里加一行
        // （另开一个 CLI 进程写文件是同一形状）。load 时见过的键本来就在内存里，
        // 验不到回填这条路。
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("[scheduler]\nallow_extra = mytool.exe\n");
        std::fs::write(&path, &text).unwrap();

        // 触发一次整文件重写。刻意不调 `set()`：它把信号发给**全局**去抖保存线程，
        // 那个线程在 300ms 后才写 `instance()` 的路径 —— 而 instance 的 app_dir
        // 可能是别的用例已经删掉的临时目录，写它等于把目录建回来（实测每个全量
        // 跑完 %TEMP% 多一个 ff_archive_*/config.ini）。
        cfg.save().unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("allow_extra = mytool.exe"),
            "手加的白名单不该被保存抹掉:\n{after}"
        );
        assert!(
            after.contains("theme = light"),
            "内存里已有的默认键照旧要写进去（证明文件真被重写过）:\n{after}"
        );
        assert!(
            !after.contains("today_count_cache_ttl"),
            "废弃键不该靠回填复活:\n{after}"
        );
    }

    /// save() 必须逐行重写：注释、空行、节序、未知键逐字保留，认识的键只换值、
    /// 原位不动。旧实现（从快照整份重建）会把这条用例里的注释与节序全部抹掉 ——
    /// 他文件里暂时还没有注释行（所以一直没咬到人），但"你去 config.ini 加一行"
    /// 的提示语到处都是，注释与手写行迟早出现。
    #[test]
    fn save_preserves_comments_blank_lines_and_unknown_keys() {
        let _lock = crate::paths::test_app_dir_lock();
        let dir = crate::paths::test_app_dir("cfg_comments");
        let path = dir.path().join("config.ini");
        std::fs::write(
            &path,
            concat!(
                "# 顶部的用户注释\n",
                "[listener]\n",
                "; 分号注释：滚轮节奏自己调过\n",
                "scroll_burst_window = 0.8\n",
                "mouse_enabled = true\n",
                "\n",
                "[custom]\n",
                "my_note = hello\n",
            ),
        )
        .unwrap();

        let cfg = FocusFlowConfig::load(&path).unwrap();
        // 改一个内存值再保存。刻意不走 set()：它把信号发给全局去抖保存线程，
        // 那个线程写的是 instance() 的路径，不是这里的临时目录。
        cfg.values
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut("listener")
            .unwrap()
            .insert("scroll_burst_window".to_string(), "0.25".to_string());
        cfg.save().unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("# 顶部的用户注释") && after.contains("; 分号注释：滚轮节奏自己调过"),
            "注释必须逐字活着:\n{after}"
        );
        assert!(
            after.contains("scroll_burst_window = 0.25") && !after.contains("= 0.8"),
            "认识的键要换成内存的新值:\n{after}"
        );
        assert!(
            after.contains("my_note = hello"),
            "内存不认识的键（整个 [custom] 节）必须原样保留:\n{after}"
        );
        let listener = after.find("[listener]").expect("listener 节头还在");
        let custom = after.find("[custom]").expect("custom 节头还在");
        assert!(listener < custom, "文件里原有的节序不许被重排:\n{after}");
        assert!(
            after.contains("mouse_enabled = true"),
            "同节没动过的键不许被新键挤掉:\n{after}"
        );
        assert!(
            after.contains("ignore_function_keys"),
            "快照里有、文件里没有的键要追加到节尾（证明确实重写过）:\n{after}"
        );
    }

    /// 回归：`load()` 结尾那次"补齐默认键"的回写失败，原来会把**已经读好的**配置
    /// 整个推翻（`cfg.save()?` ⇒ `load` 报 `Err`），而 `instance()` 的兜底臂是
    /// `in_memory(path)` = `default_config()` ⇒ 本次运行跑默认值，下一笔成功的
    /// `save()`（退出前强制落盘、改数据目录、任意 `set` 的去抖保存）就拿默认快照
    /// 逐行重写真实的 `config.ini`：`data_home` 写空、`[app_stats] exclude`
    /// 那道隐私保险丝失效、`[plugins] disabled` 复原。
    ///
    /// 夹具用**只读属性**让落盘失败（正是"另一个实例开着 / 记事本开着 / 杀软首扫"
    /// 那一类写不进去的形态），并先断言这一步真的写不进去 —— 否则整条用例只是空跑。
    /// （先试的是"握着另一个读句柄"，实测走不通：Rust 的 `File::open` 连
    /// `FILE_SHARE_DELETE` 一起给，rename 照样成功。是前提断言把它否掉的。）
    #[cfg(windows)]
    #[test]
    fn a_failed_rewrite_at_load_time_does_not_degrade_to_defaults() {
        // 只读位用 `attrib` 设：`PermissionsExt::set_readonly` 在本工具链还没稳定
        // （E0658，issue #152956），而 `std::fs::set_permissions` 没有别的法子置它。
        // 仓库里已有起外部进程的测试先例（autostart 那组跑 powershell 造 .lnk）。
        let attrib_ro = |p: &std::path::Path, on: bool| {
            let flag = if on { "+R" } else { "-R" };
            let out = std::process::Command::new("cmd")
                .args(["/C", "attrib", flag, &p.to_string_lossy()])
                .output();
            out.map(|o| o.status.success()).unwrap_or(false)
        };
        // panic 路径也要把只读位撤掉，否则 TestAppDir::drop 删不动这个目录
        // （%TEMP% 泄漏这条账记过好几轮）
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
        let tmp = crate::paths::test_app_dir("cfg_write_fails");
        let path = tmp.path().join("config.ini");
        std::fs::write(
            &path,
            "[gui]\ntheme = dark\n\n[paths]\ndata_home = X:\\我的数据\\123\n\n\
             [app_stats]\nenabled = true\nexclude = secret.exe\n",
        )
        .unwrap();
        // 只读位是这台机器上唯一稳定的"写得动读得动、就是 rename 不进去"的形态；
        // 设不上就直接跳过（不是失败：CI 上没有 cmd/attrib 时不该骗人）
        if !attrib_ro(&path, true) {
            eprintln!("attrib +R 没生效，跳过（夹具做不出来）");
            return;
        }
        let _ro = RoGuard(&path, &attrib_ro);

        // 前提：只读文件上，落盘确实会失败
        let probe = FocusFlowConfig::in_memory(path.clone());
        assert!(
            probe.save().is_err(),
            "夹具没让写失败：这条用例照不出「读成功、写失败」那一支"
        );

        // 读成功 ⇒ 写失败不能让 load 报错，也不能把用户那份换成默认值
        let cfg = FocusFlowConfig::load(&path).expect("补齐键的回写失败不该让加载失败");
        assert_eq!(cfg.get("gui", "theme"), "dark", "读出来的值必须留着");
        assert_eq!(cfg.get("paths", "data_home"), "X:\\我的数据\\123");
        assert_eq!(cfg.get("app_stats", "exclude"), "secret.exe");

        // 只读位撤掉之后正常落盘：用户那三样一个字都不能少
        assert!(attrib_ro(&path, false), "撤掉只读位该成功");
        let _ro_off = RoGuard(&path, &attrib_ro);
        cfg.save().expect("解锁后应该写得动");
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("theme = dark"),
            "默认值不许盖掉用户配置:\n{after}"
        );
        assert!(
            after.contains(r"data_home = X:\我的数据\123"),
            "data_home 被写空就等于把数据根搬回程序目录:\n{after}"
        );
        assert!(
            after.contains("exclude = secret.exe"),
            "隐私 exclude 不许被抹掉:\n{after}"
        );
    }

    /// 回归（第八扫 A3）：节头带尾注（`[gui] # 界面`）时，两处判据原来都要求
    /// `ends_with(']')` ⇒ 那一行不算节头：紧随其后的 `theme = dark` 被记进**上一节**
    /// （`database.theme`），回写又把那一行原样留在上一节的缓冲里，再在文件末尾
    /// 另起一节写默认值 `theme = light` —— 用户的键被复制进错误的节并被盖掉，
    /// 下次 parse 后写的赢。Python 的 `configparser` 用 `re.match`，尾注是接受的，
    /// 而本文件的承诺是"同格式、兼容用户既有配置"。
    #[test]
    fn section_header_with_trailing_comment_stays_its_own_section() {
        let _lock = crate::paths::test_app_dir_lock();
        let tmp = crate::paths::test_app_dir("cfg_section_tail_note");
        let path = tmp.path().join("config.ini");
        std::fs::write(
            &path,
            "[database]\nmax_backups = 99\n[gui] # 界面\ntheme = dark\nunload_hidden = true\n",
        )
        .unwrap();

        let cfg = FocusFlowConfig::load(&path).expect("带尾注的节头该能读");
        assert_eq!(
            cfg.get("gui", "theme"),
            "dark",
            "theme 不该被记到 database 那一节去"
        );
        assert_eq!(
            cfg.get("database", "max_backups").parse::<i64>().unwrap(),
            99,
            "上一节自己的键要照旧"
        );

        // 落盘之后：那一行逐字保留、gui 节里还是 dark，且全文件不许出现第二份 theme
        cfg.save().expect("保存该成功");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("[gui] # 界面"),
            "尾注节头该行该逐字留着:\n{text}"
        );
        let gui_block = text
            .split("\n[")
            .find(|b| b.starts_with("gui]"))
            .unwrap_or_default();
        assert!(
            gui_block.contains("theme = dark"),
            "gui 节里必须还是 dark:\n{text}"
        );
        assert_eq!(
            text.matches("theme = ").count(),
            1,
            "不许再冒出一份默认值的 theme:\n{text}"
        );
    }
}
