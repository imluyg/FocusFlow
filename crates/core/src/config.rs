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
        &[
            ("enabled", "false"),
            ("toggle_window", "ctrl+shift+f"),
            // 区域截图（desktop/src/snip.rs）。默认值刻意避开三类撞车：
            // Win+Shift+S 被系统截图工具占着、裸 PrintScreen 被系统的"使用 Print
            // Screen 按钮"无障碍钩子抢、Ctrl+Alt+A 是微信的默认截图键。
            // `enabled` 是总开关，两条绑定共用：关掉开关时不占用任何全局快捷键。
            ("snip", "shift+f1"),
            // 标注版截图：框完松手不立刻提交，而是进标注态（Enter 才提交、Esc 放弃）。
            // 默认键沿用上面那三类避让；不复用 shift+f1 而另起一键，是为了让"松手即提交"
            // 那条老手势一个字节都不必改（叠修饰做分岔既容易撞车又不可发现）。
            ("snip_annotate", "shift+f2"),
        ],
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

/// `config.ini` 的节序，**全仓只有这一份**。
///
/// 原先两个序列化器各抄一份同样的数组（逐行重写里"整节没出现过的新节"那段追加、
/// 与整份重建那条）。抄两遍的病不是难看：新增一节时只改一处，同一份内存快照就会
/// 因"原文件读不读得到"给出两种节序 —— 而走到整份重建那一条的时机恰恰是文件被
/// 别的句柄握着（杀软首扫、OneDrive 占位），用户看到的就是"config.ini 的节顺序自己变了"。
///
/// 前九个与 Python 版一致（便于阅读与 diff），末尾三个是后来加的节。**清单必须盖住
/// `default_config()` 的每一节**：清单外的节在整份重建那条路上过去连名字兜底都没有，
/// 直接按 `HashMap` 迭代序排 ⇒ 实测同一份默认配置，重建路给
/// `app_stats, plugins, device_stats`、逐行重写路给 `app_stats, device_stats, plugins`，
/// 每跑一次进程都可能换个样子。现在两边都是"清单位置 → 同名兜底"。
const SECTION_ORDER: [&str; 12] = [
    "database",
    "stats",
    "paths",
    "listener",
    "gui",
    "hotkey",
    "floating",
    "pomodoro",
    "rest",
    "app_stats",
    "device_stats",
    "plugins",
];

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
    let order = SECTION_ORDER;
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
    let order = SECTION_ORDER;
    let mut sections: Vec<&String> = snapshot.keys().collect();
    // 与逐行重写那条**同一个比较器**：先按清单位置，位置相同（清单外的节）再按名字。
    // 过去这里只 `sort_by_key(位置)`，清单外的几节就退回 `HashMap` 的迭代序 ——
    // 那个序每进程随机（SipHash 种子），于是"整份重建"写出的 config.ini 节序会自己变。
    sections.sort_by(|a, b| {
        let pa = order.iter().position(|o| o == *a).unwrap_or(usize::MAX);
        let pb = order.iter().position(|o| o == *b).unwrap_or(usize::MAX);
        pa.cmp(&pb).then_with(|| a.cmp(b))
    });
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
    /// 本次运行里被 `set()` 改过、还没成功落盘的键。
    /// 它唯一的作用是区分"盘上与内存不一样的值是谁改的"：
    /// 在这个集合里 ⇒ 本进程改的（内存赢，否则设置静默丢失）；
    /// 不在 ⇒ 文件在运行期被外部改过（记事本/`notepad config.ini`/同步盘），
    /// 那种改动必须被采纳，绝不能被下一次 save 悄悄盖掉。
    pending_writes: Mutex<HashSet<(String, String)>>,
    /// 最近一次落盘失败的原因；`None` = 上一次落盘成功。
    ///
    /// 为什么要记在实例上：`set_config` 那条命令**永远返回 Ok** —— 真正写盘的是 300ms
    /// 之后的去抖线程，命令返回时盘上还没动过。所以"设置写好了"这句话在界面上从来
    /// 不是证据，而失败以前只有 `tracing::error!` 一行：用户改完一切看着都对，重启后
    /// 全部回到旧值。读侧那一族"读不出来必须说话"已经修过好几处，这是剩下的写侧一半。
    last_save_error: Mutex<Option<String>>,
}

/// `load()` 结尾那次回写失败时，本次运行手里到底拿的是哪一份值。
///
/// 立它的原因：那句 warn 原先对三种状态只有一句话——「配置已读出来…本次运行仍按读出来
/// 的值」，而其中两种恰恰**没有**读出来（文件读失败 ⇒ 内存里是默认值；文件根本不存在 ⇒
/// 也是默认值）。同一渠道会被界面当作"保存出过错"显示（`last_save_error` / 设置页），
/// 于是它把"正在跑默认值"讲成"你的配置还在用"，正好说反。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoadedFrom {
    /// 读到了文件里那一份
    ReadFromFile,
    /// 还没有配置文件
    NoFileYet,
    /// 文件在，但读失败（备份成没成另说）——内存里是默认值
    ReadFailed,
}

/// 回写失败那句话：三种状态三种说法。
fn rewrite_failure_note(from: LoadedFrom, err: &str) -> String {
    match from {
        LoadedFrom::ReadFromFile => format!(
            "配置已读出来，但补齐默认键的回写没成功（本次运行仍按读出来的值，不会拿默认值盖掉文件）: {err}"
        ),
        LoadedFrom::NoFileYet => format!(
            "还没有配置文件，而第一次落盘没成功（本次运行按默认值；下次启动文件不在时会再试一次）: {err}"
        ),
        LoadedFrom::ReadFailed => format!(
            "配置文件没读出来，本次运行**按默认值**；补齐默认键的回写也没成功，那份默认值还没落到盘上: {err}"
        ),
    }
}

/// 从"文件在不在、读没读成"推出本次拿的是哪一份值。
fn loaded_from_after_read(file_exists: bool, read_ok: bool) -> LoadedFrom {
    if !file_exists {
        LoadedFrom::NoFileYet
    } else if read_ok {
        LoadedFrom::ReadFromFile
    } else {
        LoadedFrom::ReadFailed
    }
}

impl FocusFlowConfig {
    /// 从指定路径加载配置；`path` 不存在时生成默认配置并保存。
    pub fn load(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = path.into();
        let defaults = default_config();
        let mut values = defaults.clone();

        let file_exists = path.exists();
        let mut read_ok = false;
        if file_exists {
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    read_ok = true;
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
            pending_writes: Mutex::new(HashSet::new()),
            last_save_error: Mutex::new(None),
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
        //
        // 那半截"拿默认快照盖真实文件"的后果现在由 `adopt_external_edits` 兜住：
        // `in_memory` 的 `pending_writes` 是空的 ⇒ 它第一次 save 会把文件里读得到的键
        // 全部采纳，于是写回去的就是用户那一份。但**别把它当许可**：采纳只覆盖文件里
        // 已有的键，默认值新增的键照样写进去，而本进程仍然一路跑默认值（`instance()`
        // 已经缓存了这份兜底）—— 所以要 warn，不要静默。
        if let Err(rewrite) = cfg.save() {
            tracing::warn!(
                "{}",
                rewrite_failure_note(
                    loaded_from_after_read(file_exists, read_ok),
                    &format!("{rewrite:#}")
                )
            );
        }
        Ok(cfg)
    }

    /// 保存当前配置到文件（缺失 section/key 已补默认值）。
    ///
    /// 这一层只多干一件事：把**这次到底落没落成**记在实例上，让界面看得见
    /// （见 `last_save_error` 字段）。真正的写在 `save_inner` 里。
    pub fn save(&self) -> anyhow::Result<()> {
        match self.save_inner() {
            Ok(()) => {
                *self
                    .last_save_error
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = None;
                Ok(())
            }
            Err(e) => {
                // 原样递出去，别在这里把错误链换掉；记下的是给人看的那一句
                *self
                    .last_save_error
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(format!("{e:#}"));
                Err(e)
            }
        }
    }

    /// 最近一次落盘失败的原因（`None` = 上一次落盘成功 / 从没失败）。
    pub fn last_save_error(&self) -> Option<String> {
        self.last_save_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 由 save 之外的路径记一句"设置不会落盘"（保存线程死了：那时根本不会再有
    /// 一次 save，光靠 `save()` 里的记录等于什么都没有）。
    pub(crate) fn record_save_error(&self, why: &str) {
        *self
            .last_save_error
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(why.to_string());
    }

    /// 仅在锁内做快照，序列化与写盘在锁外完成：
    /// 落盘期间的磁盘 IO 不会阻塞热路径（键鼠监听/统计线程）的配置读取。
    fn save_inner(&self) -> anyhow::Result<()> {
        // 把「快照 → 序列化 → rename」整体串行化。
        //
        // 落盘刻意不能放在 `values` 锁里做（否则磁盘 IO 会阻塞键鼠监听/统计线程读配置），
        // 但一旦出了锁，两个并发 save 的 **rename 先后** 就和 **快照新旧** 再无关系：
        // 拿着旧快照的线程只要 IO 慢一点，就会把另一个线程刚写好的新配置整个盖回去，
        // 设置静默丢失。调用方确实有两个 —— 去抖的 config-saver 线程，和退出前
        // `RunEvent::Exit` 里主线程那次强制 save（改完设置立刻关窗口时正好撞上）。
        let _write_guard = SAVE_WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut snapshot: HashMap<String, HashMap<String, String>> = self
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
                let adopted = self.adopt_external_edits(&mut snapshot, &original);
                if !adopted.is_empty() {
                    tracing::info!(
                        "config.ini 在本次运行期间被外部改过，已采纳这些项（现在起按新值读；只在启动时读一次的项仍要重启）: {}",
                        adopted.join(", ")
                    );
                }
                serialize_preserving_structure(&snapshot, &original)
            }
            Err(_) => serialize_rebuilt(&snapshot),
        };
        if let Some(parent) = self.path.parent() {
            // 父目录已经不在了就**不写**，也不去把它凭空建回来。
            //
            // 这里原先是 `create_dir_all(parent).ok()`：一次迟到的去抖保存（改完设置 300ms
            // 后才落盘、退出前那次强制 save 之外的另一条路）会把一个刚被删掉的目录复活，
            // 里面只躺一份新写的 `config.ini`。产品上这就是"换目录留档 / 拔掉移动盘之后
            // 手工清理过"那些路径上凭空多出来的空壳目录，而那次 `atomic_write` 还报成功 ——
            // 没有任何一句话说明它把目录建回来了。测试侧同样的形状会骗过收尾：目录被
            // `TestAppDir` 删掉之后保存线程才落地，于是每跑一次全套留一个目录。
            //
            // 不建了之后 `atomic_write` 自然失败：待写标记保住（`?` 在摘除之前返回）、
            // `last_save_error` 会被设置页当"保存出过错"显示出来，比静默复活一个空目录诚实。
            if !parent.is_dir() {
                return Err(anyhow::anyhow!(
                    "配置目录不存在，本次没写（也不会替你把它建出来）: {}",
                    parent.display()
                ));
            }
        }
        // 落盘成功 ⇒ 把**这次真的写出去的**那些"本进程改过的键"从待写集合里摘掉，此后这些
        // 键再出现差异就都算外部改动（`the_pending_set_clears_after_a_successful_save`
        // 钉的就是这一半）。判据必须把写出的值与此刻内存里的值对一遍而不只是看名单：
        // 快照取完之后才落进来的 `set()` 即便混进了名单，它那一版也没进这份文件
        // （`a_set_that_landed_mid_save_keeps_its_pending_mark` 钉这一半）。
        // 写失败时 `?` 先返回，摘除整步不执行，待写的值继续保住。
        atomic_write(&self.path, &out)?;
        self.settle_pending_after_write(&snapshot);
        Ok(())
    }

    /// 落盘成功后收敛待写清单：只摘掉"写出去的值就是现在内存里这一版"的键。
    fn settle_pending_after_write(&self, written: &HashMap<String, HashMap<String, String>>) {
        let values = self.values.lock().unwrap_or_else(|e| e.into_inner());
        let mut pending = self
            .pending_writes
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        pending.retain(|(section, key)| {
            let current = values.get(section).and_then(|m| m.get(key));
            let written = written.get(section).and_then(|m| m.get(key));
            current != written
        });
    }

    /// 把文件里「不是本进程改的、却与内存不同」的值采纳进内存与快照，返回项名清单。
    ///
    /// 这一步防的是**静默销毁用户手写的那一行**：`[app_stats] exclude`（隐私保险丝）、
    /// `[rest] *`、`[stats] cpm_window`、`[device_stats] enabled` 这些键都没有界面入口，
    /// 手改 config.ini 是唯一的写法，而程序常驻后台 ⇒ 用户永远是在"程序运行中"改的。
    /// 旧写法里 `serialize_preserving_structure` 对"文件与内存都有这个键"一律写内存值
    /// （那是启动时读进来的快照），于是退出前那次强制 save 把他刚改的那一行盖回旧值，
    /// 一行日志都没有 —— `app_stats.rs` 与 `stats.rs` 注释里"改文件立刻生效"那句
    /// 因此从来没成立过。
    ///
    /// 只认"本进程没写过"的键：UI 改键走 `set()`，它进 `pending_writes` 直到成功落盘，
    /// 那期间文件上的旧值必须让位给内存里的新值（否则就是上一场修掉的"设置静默丢失"）。
    /// 废弃键不采纳（`load()` 刻意清掉它们，采纳等于复活），未知键照旧原样保留。
    fn adopt_external_edits(
        &self,
        snapshot: &mut HashMap<String, HashMap<String, String>>,
        original: &str,
    ) -> Vec<String> {
        let disk = parse_ini(original);
        let pending = self
            .pending_writes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut changes: Vec<((String, String), String)> = Vec::new();
        for (section, keys) in &disk {
            for (key, value) in keys {
                if DEPRECATED_CONFIG
                    .iter()
                    .any(|(s, ks)| *s == section.as_str() && ks.contains(&key.as_str()))
                {
                    continue;
                }
                if pending.contains(&(section.clone(), key.clone())) {
                    continue;
                }
                let same = snapshot
                    .get(section)
                    .and_then(|m| m.get(key))
                    .is_some_and(|v| v == value);
                if same {
                    continue;
                }
                snapshot
                    .entry(section.clone())
                    .or_default()
                    .insert(key.clone(), value.clone());
                changes.push(((section.clone(), key.clone()), value.clone()));
            }
        }
        // 内存也要跟上：热路径读的是 `values`，只改快照会让"每次判定现读"的那批键
        // 这一轮还是旧值，而写进文件的却是新值 —— 两份真相。
        //
        // 落地前必须再核一次待写集合：上面那次 clone 之后、这把锁之前落进来的 `set()`
        // 内存里已经是新值，盘上读到的那份才是旧值。既不能拿旧值盖内存，也不能把旧值
        // 写进这次的文件（否则退出前那次强制 save 会把用户刚改的一项留在盘外，
        // 而已经没有下一笔去抖保存了）。
        self.apply_adopted_changes(snapshot, changes)
    }

    /// 采纳的第二步：把 `changes` 落到内存与写盘快照，返回真正采纳的项名。
    fn apply_adopted_changes(
        &self,
        snapshot: &mut HashMap<String, HashMap<String, String>>,
        mut changes: Vec<((String, String), String)>,
    ) -> Vec<String> {
        let mut values = self.values.lock().unwrap_or_else(|e| e.into_inner());
        let pending_now = self
            .pending_writes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        changes.retain(|((section, key), value)| {
            if pending_now.contains(&(section.clone(), key.clone())) {
                if let Some(current) = values.get(section).and_then(|m| m.get(key)) {
                    snapshot
                        .entry(section.clone())
                        .or_default()
                        .insert(key.clone(), current.clone());
                }
                return false;
            }
            values
                .entry(section.clone())
                .or_default()
                .insert(key.clone(), value.clone());
            true
        });
        let mut adopted: Vec<String> = changes
            .iter()
            .map(|((section, key), _)| format!("{section}.{key}"))
            .collect();
        // 顺序要稳定：`disk` 是 HashMap，不排序的话同一批改动每次列出的次序都不同
        adopted.sort();
        adopted
    }

    /// 仅内存的配置实例（加载/落盘失败时的兜底，保证应用可用，只损失持久化）。
    ///
    /// `pending_writes` 空 = 本进程什么都没改 ⇒ `save()` 会把盘上读得到的键全部采纳，
    /// 于是这次兜底不会拿 `default_config()` 去盖真实文件（`load()` 尾部那段注释防的
    /// 正是这个形状：`data_home` 被写空、`[app_stats] exclude` 保险丝失效）。
    fn in_memory(path: PathBuf) -> Self {
        Self {
            values: Mutex::new(default_config()),
            path,
            pending_writes: Mutex::new(HashSet::new()),
            last_save_error: Mutex::new(None),
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
        self.set_local(section, key, value);
        // 去抖持久化：合并 300ms 窗口内的多次写入，避免高频调用（如悬浮窗位置）频繁整文件重写。
        // 保存线程死了（spawn 失败/通道断）send 会一直报错：热路径只喊一次，
        // 别让悬浮窗每动一下就刷一条。
        if saver_tx().send(()).is_err() {
            warn_saver_dead_once();
        }
        Ok(())
    }

    /// `set()` 的前半步：只改内存 + 记上"这一处是本进程改的"，不叫醒全局去抖保存线程。
    ///
    /// 单测走这条：那条线程写的是 `instance()` 的路径，而测试用的是自己的临时目录 ——
    /// 叫醒它等于让另一个对象往本次测试的 `config.ini` 里写它那份快照（切全局
    /// app_dir 的测试还会把它引到别人的目录去）。生产侧只有 `set()` 调它。
    fn set_local(&self, section: &str, key: &str, value: &str) {
        // 值和新值标记在同一临界区里落：中间被 save 看见会出现"值已变、标记还没到"
        // 的窗口，那次 save 写完旧值后按"两边一致"摘掉标记 —— 下一笔就把文件里的
        // 旧值当外部改动采纳回来。锁序与 save 侧一致：values → pending。
        let mut values = self.values.lock().unwrap_or_else(|e| e.into_inner());
        let mut pending = self
            .pending_writes
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        values
            .entry(section.to_string())
            .or_default()
            .insert(key.to_string(), value.to_string());
        pending.insert((section.to_string(), key.to_string()));
    }
}

/// 原子写入：先写同目录临时文件并 fsync，再 rename 替换目标
/// （Windows 上 rename 同样会替换已存在的目标）。
/// 直接 `fs::write` 截断重写，掉电/崩溃会留下空文件或半截 INI；
/// 原子替换保证任意时刻磁盘上的 config.ini 要么是旧的完整内容，要么是新的。
///
/// 对 crate 内开放：`device_aliases.json` 是同一类"用户手工录入、丢了找不回来"的
/// 小文件，两条写路径共用这一份实现，别再各写一套半成品（那边原来用的
/// 固定名 `.json.tmp` + 无 fsync，正是本函数注释里要避免的形态）。
pub(crate) fn atomic_write(path: &Path, contents: &str) -> anyhow::Result<()> {
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
        let why = "配置保存线程不可用：本次运行的设置改动不会落盘（重启程序可恢复）";
        tracing::error!("{why}");
        // 这条路径上不会再有一次 save ⇒ `save()` 里的记录永远看不到它。
        // 不记这一笔，界面就继续显示"设置已改好"，而真实情况是这次运行的一切都不落盘。
        if let Some(cfg) = INSTANCE.get() {
            cfg.record_save_error(why);
        }
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

    /// 落盘失败要留下那句能给人看的原因，落成之后要把它清掉。
    ///
    /// 为什么值得单独钉：`set_config` 那条命令**永远返回 Ok** —— 真正写盘的是 300ms
    /// 之后的去抖线程，命令返回时盘上还没动过。所以"设置写好了"在界面上从来不是证据，
    /// 这条记录是设置页唯一拿得到的东西。以前失败只有 `tracing::error!` 一行，
    /// 用户改完一切看着都对、重启后全部回到旧值。
    ///
    /// 夹具是本仓那一族的常用造法：把目标位置换成**同名目录**，rename 必然失败。
    #[test]
    fn a_failed_save_leaves_the_reason_the_settings_page_shows() {
        let _lock = crate::paths::test_app_dir_lock();
        let app = crate::paths::test_app_dir("cfg_save_err");
        let path = app.path().join("config.ini");
        std::fs::write(&path, "[gui]\ntheme = dark\n").unwrap();
        let cfg = FocusFlowConfig::load(&path).unwrap();
        assert_eq!(cfg.last_save_error(), None, "还没试过盘，不该有话");

        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir_all(&path).unwrap();
        let err = cfg
            .save()
            .expect_err("config.ini 的位置上是一个目录，落盘应当失败");
        let shown = cfg.last_save_error().expect("失败必须留下给人看的那一句");
        assert!(
            !shown.trim().is_empty(),
            "留了个空字符串等于什么都没留（OS 原因: {err}）"
        );

        // 障碍清掉之后落成功了 ⇒ 那句话必须跟着消失，否则红字会一辈子挂在设置页上
        std::fs::remove_dir_all(&path).unwrap();
        cfg.save().expect("目录挪走之后应当写得出去");
        assert_eq!(
            cfg.last_save_error(),
            None,
            "落成了还留着失败原因＝界面永远红着"
        );
    }

    /// 迟到的保存不许把已经删掉的目录凭空建回来。
    ///
    /// 写点原先是 `create_dir_all(parent).ok()`，于是"目录被删了之后才落地的那一次保存"
    /// 会把它复活成一个只装 `config.ini` 的空壳，而 `atomic_write` 照样报成功 ——
    /// 产品上这正是"换目录留档 / 拔掉移动盘后手工清理过"那些路径上凭空多出来的目录，
    /// 测试上则是每跑一次全套留一个 `%TEMP%` 目录（本仓的泄漏判据要求跑完回到跑前状态）。
    #[test]
    fn a_late_save_does_not_resurrect_a_deleted_directory() {
        let _lock = crate::paths::test_app_dir_lock();
        let app = crate::paths::test_app_dir("cfg_no_resurrect");
        let path = app.path().join("config.ini");
        std::fs::write(&path, "[gui]\ntheme = dark\n").unwrap();
        let cfg = FocusFlowConfig::load(&path).unwrap();

        // 正向腿：目录还在 ⇒ 照常落地，且成功后不许留着失败原因
        cfg.set_local("gui", "theme", "light");
        cfg.save().expect("目录还在时这一次就该写成");
        assert!(
            cfg.last_save_error().is_none(),
            "落成了还留着原因＝界面永远红着"
        );

        // 目录没了 ⇒ 这次不能报成功，也不能把目录变回来
        std::fs::remove_dir_all(app.path()).expect("先把目录删掉");
        let err = cfg.save().expect_err("父目录已经不在了，这一次不能算写成");
        assert!(
            err.to_string().contains("配置目录不存在"),
            "要说是哪个目录不在了: {err}"
        );
        assert!(
            !app.path().exists(),
            "一次失败的保存不该把删掉的目录建回来: {}",
            app.path().display()
        );
        assert!(
            cfg.last_save_error().is_some(),
            "失败要留下设置页能看见的那一句"
        );
    }

    /// 保存线程死了这条路径也必须留痕：那条路上**不会再有一次 save**，
    /// 只靠 `save()` 里的记录等于什么都没记。
    #[test]
    fn the_dead_saver_path_records_its_own_reason() {
        let _lock = crate::paths::test_app_dir_lock();
        let app = crate::paths::test_app_dir("cfg_saver_dead");
        let path = app.path().join("config.ini");
        std::fs::write(&path, "[gui]\ntheme = dark\n").unwrap();
        let cfg = FocusFlowConfig::load(&path).unwrap();
        cfg.record_save_error("配置保存线程不可用：本次运行的设置改动不会落盘（重启程序可恢复）");
        let shown = cfg.last_save_error().expect("这一条路径也要有话");
        assert!(
            shown.contains("不会落盘"),
            "要说清后果，不只是「出错了」: {shown}"
        );
    }

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

    /// 程序运行期间用户手改 config.ini 的那一行，绝不能被下一次落盘盖掉。
    ///
    /// `[app_stats] exclude`（隐私保险丝）、`[rest] *`、`[stats] cpm_window`、
    /// `[device_stats] enabled` 都没有界面入口，手改文件是唯一写法，而程序是常驻后台的
    /// ⇒ 他永远是在"运行中"改的。旧写法对"文件与内存都有这个键"一律写内存值（启动时
    /// 那份快照），退出前的强制 save 就把那一行还原了，一行日志都没有 ——
    /// `app_stats.rs`/`stats.rs` 注释里"改文件立刻生效"因此从来没成立过。
    #[test]
    fn an_external_edit_survives_the_next_save() {
        let _lock = crate::paths::test_app_dir_lock();
        let app = crate::paths::test_app_dir("cfg_ext_edit");
        let path = app.path().join("config.ini");
        std::fs::write(&path, "[app_stats]\nexclude = psafe.exe\n").unwrap();
        let cfg = FocusFlowConfig::load(&path).unwrap();
        assert_eq!(cfg.get("app_stats", "exclude"), "psafe.exe");

        // 程序还在跑，用户用记事本又加了一个密码管理器
        std::fs::write(&path, "[app_stats]\nexclude = psafe.exe, keepass.exe\n").unwrap();
        cfg.save().unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("exclude = psafe.exe, keepass.exe"),
            "运行期手改的那一行被启动时的值盖掉了: {text}"
        );
        // 内存也必须跟上：热路径读的是 `values`，只改文件的话"每次判定现读"的那批键
        // 这一轮还是旧值（而 `load_config` 那边的注释承诺的是立刻生效）。
        assert_eq!(
            cfg.get("app_stats", "exclude"),
            "psafe.exe, keepass.exe",
            "采纳只落到文件、没落到内存 ⇒ 两份真相"
        );
    }

    /// 反面对照（这一半才是这条修改真正的风险）：UI 刚改、还没落盘的键必须继续赢。
    ///
    /// 上一场修过"两个并发 save 让设置静默丢失"，如果这里让文件上的旧值赢，
    /// 那个形状就回来了 —— 所以判据是"差异是谁改的"，不是"谁在文件上"。
    #[test]
    fn a_pending_ui_write_beats_a_stale_disk_value() {
        let _lock = crate::paths::test_app_dir_lock();
        let app = crate::paths::test_app_dir("cfg_ext_race");
        let path = app.path().join("config.ini");
        std::fs::write(&path, "[gui]\ntheme = dark\n").unwrap();
        let cfg = FocusFlowConfig::load(&path).unwrap();

        // 用户在设置页把主题改成 light（去抖还没落盘），同一时刻文件又被外部改成别的值
        cfg.set_local("gui", "theme", "light");
        std::fs::write(&path, "[gui]\ntheme = solarized\n").unwrap();
        cfg.save().unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("theme = light"),
            "本进程刚做的设置被文件里的旧值顶掉了: {text}"
        );
        assert_eq!(cfg.get("gui", "theme"), "light");
    }

    /// 落盘成功之后"待写"集合要清空：否则那个键一辈子不再接受外部改动
    /// （UI 改过一次主题之后，用户就再也改不动这一行了）。
    #[test]
    fn the_pending_set_clears_after_a_successful_save() {
        let _lock = crate::paths::test_app_dir_lock();
        let app = crate::paths::test_app_dir("cfg_ext_after_save");
        let path = app.path().join("config.ini");
        std::fs::write(&path, "[gui]\ntheme = dark\n").unwrap();
        let cfg = FocusFlowConfig::load(&path).unwrap();

        cfg.set_local("gui", "theme", "light");
        cfg.save().unwrap();
        assert_eq!(cfg.get("gui", "theme"), "light");

        // 这次改动已经在文件上了 ⇒ 之后的差异只能来自外部
        std::fs::write(&path, "[gui]\ntheme = solarized\n").unwrap();
        cfg.save().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("theme = solarized"),
            "已落盘的键此后再被外部改动应当采纳，而不是永久停在 UI 那次值: {text}"
        );
        assert_eq!(cfg.get("gui", "theme"), "solarized");
    }

    /// 快照取完之后才落进来的 `set()`：那次 save 写出去的是旧值，标记必须留下。
    ///
    /// 真实竞态要卡在两个 clone 之间，单测里做不到；这里把"写出去的那份快照"
    /// 直接交给收尾函数，代表 save 在 `set()` 之前取到的值。收尾若按名单一律摘掉，
    /// 下一笔 save 会把文件里的 dark 当成外部改动采纳回来，用户在设置页选的 light
    /// 静默回退 —— 重启后又变回去。
    #[test]
    fn a_set_that_landed_mid_save_keeps_its_pending_mark() {
        let _lock = crate::paths::test_app_dir_lock();
        let app = crate::paths::test_app_dir("cfg_mid_save_pending");
        let path = app.path().join("config.ini");
        std::fs::write(&path, "[gui]\ntheme = dark\n").unwrap();
        let cfg = FocusFlowConfig::load(&path).unwrap();

        cfg.set_local("gui", "theme", "light");
        let mut written: HashMap<String, HashMap<String, String>> = HashMap::new();
        written.insert(
            "gui".to_string(),
            HashMap::from([("theme".to_string(), "dark".to_string())]),
        );
        cfg.settle_pending_after_write(&written);

        cfg.save().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("theme = light"),
            "中途落下那一笔的待写标记被摘掉了，下一笔保存把盘上的旧值当外部改动采纳回来: {text}"
        );
        assert_eq!(cfg.get("gui", "theme"), "light");
    }

    /// 采纳在落地前必须再看一眼待写集合：`set()` 若落在"读盘后的 pending 快照"
    /// 与"写内存"之间，盘上的旧值既不能盖掉用户刚写的新值，也不能被写进这次文件
    /// （退出前那次强制 save 之后没有下一笔去抖保存替它补上）。
    #[test]
    fn an_adoption_cannot_overwrite_a_value_set_after_its_first_look() {
        let _lock = crate::paths::test_app_dir_lock();
        let app = crate::paths::test_app_dir("cfg_adopt_apply");
        let path = app.path().join("config.ini");
        std::fs::write(&path, "[gui]\ntheme = dark\n").unwrap();
        let cfg = FocusFlowConfig::load(&path).unwrap();

        cfg.set_local("gui", "theme", "light");
        let mut snapshot: HashMap<String, HashMap<String, String>> = HashMap::new();
        snapshot.insert(
            "gui".to_string(),
            HashMap::from([("theme".to_string(), "solarized".to_string())]),
        );
        let adopted = cfg.apply_adopted_changes(
            &mut snapshot,
            vec![(
                ("gui".to_string(), "theme".to_string()),
                "solarized".to_string(),
            )],
        );
        assert_eq!(
            cfg.get("gui", "theme"),
            "light",
            "盘上的旧值盖掉了刚设置的新值"
        );
        assert_eq!(
            snapshot
                .get("gui")
                .and_then(|m| m.get("theme"))
                .map(String::as_str),
            Some("light"),
            "这次文件仍会写出旧值，退出前那次强制 save 之后就没有下一笔了"
        );
        assert!(
            adopted.is_empty(),
            "这一项没有采纳，日志里不许列它: {adopted:?}"
        );
    }

    /// 竞态本身没法在用例里确定性复现（要 `set()` 正好落进 save 快照与写盘之间），
    /// 所以钉语句顺序：新值和新值标记必须**在同一段值锁持有期内**写入。
    /// 旧形态先放掉值锁再单独记标记，save 若在两者之间取快照，写出去的是旧值、
    /// 收尾却按标记当它已落盘 —— 下一笔就把文件里的旧值采纳回来（本文件另外两条
    /// 用例分别钉住了收尾与采纳那一半，这条钉的是窗口本身不回来）。
    #[test]
    fn a_local_set_keeps_the_value_lock_through_the_pending_mark() {
        let src = include_str!("config.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let at = prod.find("fn set_local(").expect("set_local 不见了");
        let rest = &prod[at..];
        let body = &rest[..rest.find("\n    }\n").expect("函数体切不出来")];
        let code: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let values_at = code.find("self.values.lock()").expect("这一处没拿值锁");
        let pending_at = code
            .find("self.pending_writes.lock()")
            .expect("这一处没拿待写锁");
        assert!(
            values_at < pending_at,
            "锁序必须是 values → pending（save 侧同向）"
        );
        let between = &code[values_at..pending_at];
        assert!(
            !between.contains('}'),
            "值锁在记下待写标记之前就放了：save 会在两者之间看到新值却没有标记"
        );
        assert!(
            !between.contains(".clone()"),
            "值锁只 clone 了一份就放掉：标记落下之前 save 仍能取到不一致的两半"
        );
        assert!(
            !between.contains("drop("),
            "值锁在记下待写标记之前就被显式放掉了：save 仍能取到不一致的两半"
        );
        assert!(
            code[pending_at..].contains("pending.insert"),
            "待写标记没有写进集合"
        );
    }

    /// 废弃键不采纳：`load()` 刻意把它们清掉，采纳等于让它们复活。
    #[test]
    fn a_deprecated_key_on_disk_is_not_adopted() {
        let _lock = crate::paths::test_app_dir_lock();
        let app = crate::paths::test_app_dir("cfg_ext_deprecated");
        let path = app.path().join("config.ini");
        std::fs::write(&path, "[stats]\ntoday_count_cache_ttl = 999\n").unwrap();
        let cfg = FocusFlowConfig::load(&path).unwrap();
        cfg.save().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("today_count_cache_ttl"),
            "废弃键被采纳回内存又写回文件了: {text}"
        );
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
        // 改一个值再保存。走 `set_local`（= `set()` 的前半步）：直接写 `values` 的话
        // 这次改动没进 `pending_writes`，`save()` 会把它当成"文件被外部改了"而把内存
        // 采纳回旧值 —— 那正是 `an_external_edit_survives_the_next_save` 要的行为。
        // 刻意不走完整 `set()`：它把信号发给全局去抖保存线程，
        // 那个线程写的是 instance() 的路径，不是这里的临时目录。
        cfg.set_local("listener", "scroll_burst_window", "0.25");
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

    /// 写失败的那一次不能顺手把"本进程改过"的身份也丢掉。
    ///
    /// ⚠ 这条**不是**"回退旧写法就红"的区分性用例：旧写法（成功后整表 `clear()`）在
    /// `atomic_write` 失败时靠 `?` 提前返回，同样不会摘除，所以这一支两版都绿。
    /// 它钉的是那条不变量本身 —— 将来谁把摘除挪到写盘**之前**、或改成失败也清，
    /// 这条就会红。上面代码注释里那半条真正的差异（`set()` 正好夹在"取快照"与
    /// "摘除"之间）需要 interleaving，headless 造不出来 ⇒ 登记为逻辑核过、无守卫视角。
    #[test]
    fn a_failed_save_keeps_the_pending_marks() {
        // 只读位是这台机器上唯一稳定的"读得动、就是 rename 不进去"的形态；
        // 设不上就直接跳过（CI 上没有 cmd/attrib 时不该骗人）
        let attrib_ro = |p: &std::path::Path, on: bool| {
            let flag = if on { "+R" } else { "-R" };
            std::process::Command::new("cmd")
                .args(["/C", "attrib", flag, &p.to_string_lossy()])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        // panic 路径也要把只读位撤掉，否则 TestAppDir::drop 删不动这个目录（%TEMP% 泄漏）
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
        let tmp = crate::paths::test_app_dir("cfg_failed_save_pending");
        let path = tmp.path().join("config.ini");
        std::fs::write(&path, "[gui]\ntheme = dark\n").unwrap();
        let cfg = FocusFlowConfig::load(&path).unwrap();
        assert_eq!(cfg.get("gui", "theme"), "dark");

        if !attrib_ro(&path, true) {
            eprintln!("attrib +R 没生效，跳过（夹具做不出来）");
            return;
        }
        let _ro = RoGuard(&path, &attrib_ro);

        // ① UI 改了主题，而这一次落盘被只读位挡住
        cfg.set_local("gui", "theme", "light");
        assert!(
            cfg.save().is_err(),
            "夹具没让写失败：这条用例照不出「写失败还要保住待写身份」那一支"
        );

        // ② 解锁之后文件被外部改成另一个值：那次没落盘的 UI 写入必须还赢一次
        assert!(attrib_ro(&path, false), "撤掉只读位该成功");
        std::fs::write(&path, "[gui]\ntheme = solarized\n").unwrap();
        cfg.save().expect("解锁后应该写得动");
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("theme = light"),
            "写失败把\"本进程改过\"的身份一起摘掉了 ⇒ UI 那次设置两头落空:\n{after}"
        );
        assert_eq!(cfg.get("gui", "theme"), "light");
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

    /// 金标准：`crates/core/tests/default_config_keys.txt` 记着「新装程序第一次落盘会写
    /// 哪些节/键」。这条用例保证那份文件跟着**程序的序列化器**走，改默认值时它会红。
    ///
    /// 为什么要这份金标准：发布用的 zip 模板（`.scratch/make_zip.sh` 的 `CFG_SRC`）每版都可能
    /// 是"上一版跑出来的那一份"，本版新增的默认键不在里面 —— v0.4.3 就缺 `[hotkey] snip`，
    /// 新装用户拿到的 config.ini 少一行，而用例、编译、门禁全都不会红（模板是 `.scratch/`
    /// 里的发布工件，不在仓库里）。`make_zip.sh` 拿这份文件对拍，本用例保证文件没跟丢代码。
    ///
    /// 只钉键集合、不钉值：默认值常年调（`scroll_burst_window` 就从 0.8 调到 0.25），
    /// 钉住值会让每次调参都得重新生成一份文件。
    ///
    /// 更新方式：把失败消息里印出的「应有清单」原样写回那份文件（注释头保留）。
    #[test]
    fn default_config_keys_match_golden() {
        let defaults = default_config();
        let rendered = serialize_rebuilt(&defaults);

        // 从渲染文本里抠出 节/键。serialize_rebuilt 不写注释，所以这里不必处理 # 行。
        let mut rendered_keys: Vec<String> = Vec::new();
        let mut section = String::new();
        for line in rendered.lines() {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            if let Some(rest) = t.strip_prefix('[') {
                section = rest
                    .split(']')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_string();
            } else if let Some(eq) = t.find('=') {
                let key = t[..eq].trim();
                if !key.is_empty() {
                    rendered_keys.push(format!("{section}/{key}"));
                }
            }
        }
        rendered_keys.sort();

        // 第一条腿：default_config() 里每一个键都必须真的出现在渲染里。
        // 光跟金标准比看不出来 —— serialize_rebuilt 会把**整节为空**的节跳过（金标准跟着
        // 一起少），而"这一节配了默认值却没落盘"正是新装用户少一行的形态。
        let mut declared: Vec<String> = Vec::new();
        for (section, keys) in &defaults {
            for key in keys.keys() {
                declared.push(format!("{section}/{key}"));
            }
        }
        declared.sort();
        assert_eq!(
            declared, rendered_keys,
            "default_config() 有键没被写进渲染（多半是那一节一个键都没有，被整节跳过了）"
        );

        // 第二条腿：渲染结果与仓库里那份金标准一致 ⇒ 金标准没跟丢代码。
        let golden: Vec<String> = include_str!("../tests/default_config_keys.txt")
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        let mut golden_sorted = golden.clone();
        golden_sorted.sort();
        if golden_sorted != rendered_keys {
            let want = rendered_keys.join("\n");
            let got = golden_sorted.join("\n");
            panic!(
                "金标准与程序的渲染不一致，请更新 crates/core/tests/default_config_keys.txt\n\
                 应有清单（{} 条）:\n{want}\n文件里现有（{} 条）:\n{got}",
                rendered_keys.len(),
                golden_sorted.len()
            );
        }
    }

    /// 两条落盘路必须用**同一份**节序，而 `default_config()` 的每一节都要在那份清单里。
    ///
    /// 这条盯的是"数组抄两份"的回潮：`serialize_preserving_structure`（原文件读得到，
    /// 逐行重写）与 `serialize_rebuilt`（读不到，整份重建）以前各写一遍同样的数组。
    /// 只改一处时症状是"config.ini 的节顺序自己变了"—— 而走到整份重建那一条的时机
    /// 恰恰是文件被杀软/同步盘握着句柄的时候，普通人根本对不上是哪一次保存改的。
    /// 两个方向的判据都要：节序两边相等（`HashMap` 迭代序每进程随机，所以这条不是在
    /// 比"插进去的顺序"），以及新增一节时不会悄悄掉到清单外。
    #[test]
    fn both_serializers_use_the_same_single_section_order() {
        let sections_of = |text: &str| -> Vec<String> {
            text.lines()
                .filter_map(|line| {
                    let t = line.trim();
                    t.strip_prefix('[')
                        .and_then(|rest| rest.strip_suffix(']'))
                        .map(|name| name.to_string())
                })
                .collect()
        };
        let defaults = default_config();
        let rebuilt = sections_of(&serialize_rebuilt(&defaults));
        let appended = sections_of(&serialize_preserving_structure(&defaults, ""));
        assert!(
            rebuilt.len() >= 2,
            "夹具前提：没抠出节头，上面两条比较是空转"
        );
        assert_eq!(
            rebuilt, appended,
            "同一份内存快照在两条落盘路上给出两种节序（清单抄回两份了）"
        );

        let mut missing: Vec<String> = defaults
            .keys()
            .filter(|s| !SECTION_ORDER.contains(&s.as_str()))
            .cloned()
            .collect();
        missing.sort();
        assert!(
            missing.is_empty(),
            "这些节在 default_config() 里但没进 SECTION_ORDER ⇒ 会掉到清单外: {missing:?}"
        );

        // 清单外的节（将来新增、或用户手写的节）也必须有个定数：按名字。
        let mut odd: HashMap<String, HashMap<String, String>> = HashMap::new();
        for name in ["zzeta", "aalpha", "database", "mmiddle"] {
            let mut kv = HashMap::new();
            kv.insert("k".to_string(), "1".to_string());
            odd.insert(name.to_string(), kv);
        }
        assert_eq!(
            sections_of(&serialize_rebuilt(&odd)),
            vec!["database", "aalpha", "mmiddle", "zzeta"],
            "整份重建那条对清单外的节要按名字排，不能退回 HashMap 的每进程随机序"
        );
        assert_eq!(
            sections_of(&serialize_rebuilt(&odd)),
            sections_of(&serialize_preserving_structure(&odd, "")),
            "同一条规则在两条路上得给同一个答案"
        );
    }

    /// 回写失败那句话只能说**当前状态成立的事**。
    ///
    /// 旧写法对"读成功／没有文件／读失败"三种状态共用一句「配置已读出来…本次运行仍按
    /// 读出来的值」—— 后两种恰恰跑的是默认值，而这句会被界面当作"保存出过错"显示出去，
    /// 于是把"正在跑默认值"讲成"你的配置还在用"。
    #[test]
    fn the_rewrite_note_only_claims_what_the_current_state_supports() {
        let mut wrong: Vec<String> = Vec::new();
        let read_ok = rewrite_failure_note(LoadedFrom::ReadFromFile, "夹具原因");
        if !(read_ok.contains("配置已读出来") && read_ok.contains("夹具原因")) {
            wrong.push(format!("读成功那一支要说\"按读出来的值\"，实得 {read_ok}"));
        }
        for (from, what) in [
            (LoadedFrom::ReadFailed, "文件读不出来"),
            (LoadedFrom::NoFileYet, "还没有配置文件"),
        ] {
            let note = rewrite_failure_note(from, "夹具原因");
            if note.contains("配置已读出来") || note.contains("仍按读出来的值") {
                wrong.push(format!("{what}那一支不许声称读出来了: {note}"));
            }
            if !note.contains("默认值") {
                wrong.push(format!("{what}那一支要说清本次跑的是默认值: {note}"));
            }
            if !note.contains("夹具原因") {
                wrong.push(format!("{what}那一支要带上失败原因: {note}"));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    /// 三种状态各由"文件在不在、读没读成"决定，别的组合不许存在。
    #[test]
    fn the_state_comes_from_the_file_and_the_read() {
        assert_eq!(
            loaded_from_after_read(false, true),
            LoadedFrom::NoFileYet,
            "文件不在就是没得读，read_ok 是什么都不算"
        );
        assert_eq!(loaded_from_after_read(true, true), LoadedFrom::ReadFromFile);
        assert_eq!(loaded_from_after_read(true, false), LoadedFrom::ReadFailed);
    }

    /// 接线守卫：那句 warn 必须**经过**上面两个函数，读成功与否也必须真的被记下来。
    ///
    /// 纯函数写得再对，调用点退回一句写死的话就白搭 —— 而 `load()` 要弹不了的东西
    /// 太多（真实文件 + 让它失败 + 再让回写也失败），只能钉形状。
    #[test]
    fn load_routes_the_note_through_the_actual_read_outcome() {
        let src = include_str!("config.rs");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod = src[..cut]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut wrong: Vec<String> = Vec::new();
        for (needle, what) in [
            (
                "loaded_from_after_read(file_exists, read_ok)",
                "那句 warn 没按真实的读取结局选说法",
            ),
            ("read_ok = true;", "读成功那一臂没把结局记下来"),
            (
                "let file_exists = path.exists();",
                "没文件那一支的来源被绕开了",
            ),
        ] {
            if !prod.contains(needle) {
                wrong.push(format!("{what}（少了「{needle}」）"));
            }
        }
        let n = prod.matches("&format!(\"{rewrite:#}\")").count();
        if n != 1 {
            wrong.push(format!("失败原因该恰好交给那句话一次，实得 {n} 次"));
        }
        assert!(
            wrong.is_empty(),
            "配置回写的接线守卫红:\n{}",
            wrong.join("\n")
        );
    }
}
