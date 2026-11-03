//! 脚本插件清单的校验器（方案 3.7 的"声明必用"那一半 + 路线图第 14 步）。
//!
//! 它回答的是插件作者的问题：「我这份 .lua 有没有写错」。运行时那一头（[`permissions`]）
//! 只管"这一次调用能不能放行"，而它出声的地方是日志与页面 —— 作者手里没有正在跑的程序，
//! 他需要的是一句带着行号的话。
//!
//! 三条纪律：
//! · **解析复用加载侧那一份**（`manager::declared_fields`，它是加载侧 `scan_meta_source`
//!   现在唯一的那台取词机器）。校验器自己再抄一遍
//!   "`KEY = 值` 怎么算顶层赋值"的话，同一份文件两侧会读出两个结果，作者照着校验器的话
//!   改完，加载侧照旧折成缺省 —— 那是本仓修过好几轮的那种"两处各抄一份"。
//! · **只提示，不阻塞加载**：API 名可能由字符串拼出来（`focusflow["sto" .. "re"]`），静态扫
//!   必然有漏报。方案那张风险表写的判据是"漏报比误报代价低"，所以这里所有话都落在文字上：
//!   不改运行时行为，也不作为拒绝加载的依据。
//! · **每条话都带行号**：说"这一项会被折掉"而不说在哪一行，作者只能从头通读一遍文件。

use super::host::{is_builtin_only_api, EXTENSION_API_NAMES};
use super::manager::{
    declared_fields, field_line, strip_lua_non_code, DeclaredFields, MAX_SUPPORTED_PLUGIN_API,
};
use super::{permissions, PluginLifecycle, PluginManifest, PluginSource};
use std::path::Path;

/// 一条话的分量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// 这一份跑起来会与作者以为的不一样（值被折掉、格子被摘掉、调用被拒绝）。
    Error,
    /// 跑得起来，但有一句多余的话或一个可以收紧的地方。
    Note,
}

/// 一条话。
#[derive(Debug, Clone)]
pub struct Finding {
    pub level: Level,
    /// 原文件里的行号（1 起）；`None` = 这一条不属于某一个字段，而是整份文件的形状。
    pub line: Option<usize>,
    pub message: String,
}

/// 一份文件的校验结果。
#[derive(Debug, Clone)]
pub struct Report {
    /// 加载侧真正会用的那一份清单（折叠之后的，与插件页看到的同一张脸）。
    pub manifest: PluginManifest,
    pub findings: Vec<Finding>,
}

impl Report {
    /// 有没有"跑起来与作者以为的不一样"的那一类。CLI 拿它决定退出码。
    pub fn has_errors(&self) -> bool {
        self.findings.iter().any(|f| f.level == Level::Error)
    }

    /// 给人看的逐行输出（`--validate-plugin` 打的就是这个）。
    pub fn render(&self) -> String {
        if self.findings.is_empty() {
            return format!("{}：这份清单没有要说的。", self.manifest.name);
        }
        self.findings
            .iter()
            .map(|f| {
                let head = match f.level {
                    Level::Error => "问题",
                    Level::Note => "提示",
                };
                match f.line {
                    Some(n) => format!("第 {n} 行 {head}：{}", f.message),
                    None => format!("{head}：{}", f.message),
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// 代码里点到的一格，连同它在哪一行。
struct Use {
    api: String,
    /// `store` 那一格下面还分动词（`set` / `get`），其余的格子都是整格。
    verb: Option<String>,
    line: usize,
}

/// 一份脚本的全部校验。`text` 是文件原文，`source` 是它**此刻**所在的位置（见 [`source_of`]）。
pub fn validate(stem: &str, source: PluginSource, text: &str) -> Report {
    let raw = declared_fields(text, stem, &format!("校验 {stem}"));
    let manifest = super::manager::build_manifest(DeclaredFields {
        // 折叠那一步自己会出声（进日志），这里只换一个说法：校验器念的是这一份文件的名字
        where_: format!("校验 {stem}"),
        ..raw.clone()
    });
    let code = strip_lua_non_code(text);
    let used = used_apis(&code);

    let mut sink = Sink {
        findings: Vec::new(),
        code: &code,
    };
    check_field_shapes(&mut sink, &raw, source);
    check_permissions(&mut sink, &manifest, source, &used);
    check_api_names(&mut sink, &used, source);

    let mut findings = sink.finish();
    findings.sort_by_key(|f| (f.line.unwrap_or(usize::MAX), f.message.clone()));
    Report { manifest, findings }
}

/// 出声的那一口：两条路都收进同一份清单，行号从哪儿来由调用方说清。
struct Sink<'a> {
    findings: Vec<Finding>,
    /// 抹掉注释之后的那一份代码，行号与原文件一一对应（`strip_lua_non_code` 保住换行）。
    code: &'a str,
}

impl Sink<'_> {
    /// 声明字段那一类：行号按字段名找，用的是加载侧同一批谓词（[`field_line`]）。
    fn field(&mut self, level: Level, key: &'static str, message: String) {
        self.findings.push(Finding {
            level,
            line: field_line(self.code, key),
            message,
        });
    }

    /// 代码那一类：行号就是命中处那一行。
    fn at(&mut self, level: Level, line: usize, message: String) {
        self.findings.push(Finding {
            level,
            line: Some(line),
            message,
        });
    }

    fn finish(self) -> Vec<Finding> {
        self.findings
    }
}

/// 按**位置**定身份（方案 3.1）：内置位免授权、扩展位走三道判断，同一份文件在两处要说的话
/// 不一样。认不出它在哪一格就按最严的那一支判（用户插件）—— 校验器宁可多说一句。
pub fn source_of(path: &Path) -> PluginSource {
    let dirs = crate::paths::plugin_scan_dirs(false);
    let Some(parent) = path.parent() else {
        return PluginSource::User;
    };
    if dirs.first().is_some_and(|d| d == parent) {
        PluginSource::Builtin
    } else {
        PluginSource::User
    }
}

/// 读一份文件并校验。读不出来的那句话交回调用方（CLI 要按"问不出"退出，而不是按"没问题"）。
pub fn validate_path(path: &Path) -> Result<Report, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("{} 读不出来: {e}", path.display()))?;
    let stem = crate::plugins::manager::PluginManager::stem_of(path);
    Ok(validate(&stem, source_of(path), &text))
}

/// 代码里出现过的 `focusflow.<格>`（含 `focusflow.store.<动词>`），按调用点带行号。
///
/// 形状与运行时闸门吃的参数是同一对（[`permissions::required_for`] 收 `(api, verb)`）：
/// 两边不同形的话，"用了没声明"这一条就只能对上一半。
fn used_apis(code: &str) -> Vec<Use> {
    let mut out: Vec<Use> = Vec::new();
    let b = code.as_bytes();
    let needle = b"focusflow.";
    let mut i = 0;
    while i + needle.len() <= b.len() {
        if &b[i..i + needle.len()] != needle {
            i += 1;
            continue;
        }
        let mut j = i + needle.len();
        let name = ident_at(code, &mut j);
        if name.is_empty() {
            // 点后面跟的不是标识符（换行、`[`、拼出来的字符串）—— 这一处扫不到，认漏报
            i += needle.len();
            continue;
        }
        let mut verb = None;
        if name == "store" && b.get(j) == Some(&b'.') {
            let mut k = j + 1;
            let v = ident_at(code, &mut k);
            if !v.is_empty() {
                verb = Some(v);
                j = k;
            }
        }
        // 行号按命中之前那一段数出来（`strip_lua_non_code` 保住换行 ⇒ 与原文件同一行）。
        // `lines()` 会把这一段末尾那半行也算一行，所以这里不能再 +1。
        let line = code
            .get(..i)
            .map(|head| head.lines().count())
            .unwrap_or(1)
            .max(1);
        if !out.iter().any(|u| u.api == name && u.verb == verb) {
            out.push(Use {
                api: name,
                verb,
                line,
            });
        }
        i = j;
    }
    out
}

/// 从 `*i` 起读一个 Lua 标识符（`[A-Za-z0-9_]`），把游标推进到它之后。
fn ident_at(code: &str, i: &mut usize) -> String {
    let b = code.as_bytes();
    let mut j = *i;
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
        j += 1;
    }
    let s = code.get(*i..j).unwrap_or_default().to_string();
    *i = j;
    s
}

/// 字段本身写坏了的那些：值会被折回缺省的，都是"作者写了却像没写"。
fn check_field_shapes(sink: &mut Sink<'_>, raw: &DeclaredFields, source: PluginSource) {
    if let Some(n) = raw.api {
        if n < 1 {
            sink.field(
                Level::Error,
                "PLUGIN_API",
                format!("= {n} 不是一个正整数，加载时按缺省 1 走 —— 这一份想要的能力版本没生效"),
            );
        } else if n as u32 > MAX_SUPPORTED_PLUGIN_API {
            sink.field(
                Level::Error,
                "PLUGIN_API",
                format!(
                    "= {n} 比本宿主认得的最高版本还高（这一版到 {}）—— 加载会在执行脚本之前就被拒绝，\
                     插件页那一行写「需要更新版本」；这一份要配更高版本的程序",
                    MAX_SUPPORTED_PLUGIN_API
                ),
            );
        }
    }
    if let Some(n) = raw.refresh_ms {
        if n < 0 {
            sink.field(
                Level::Error,
                "PLUGIN_REFRESH_MS",
                format!("= {n} 不是一个非负整数，加载时按缺省 0（只在动作后刷新）走"),
            );
        } else if n > 0 && n < 1000 {
            sink.field(
                Level::Note,
                "PLUGIN_REFRESH_MS",
                format!(
                    "= {n} 比界面那一拍的节奏还快（面板每 1000 ms 重画一次），再快也只是白算一次"
                ),
            );
        }
    }
    if let Some(word) = raw.lifecycle.as_deref() {
        match PluginLifecycle::parse(word) {
            None => sink.field(
                Level::Error,
                "PLUGIN_LIFECYCLE",
                format!(
                    "= \"{word}\" 认不出来（只认 panel / always），按缺省 {} 走",
                    PluginLifecycle::default().keyword()
                ),
            ),
            // 位置与档位打架的那一种：内置模块被强制常驻（3.14），写 panel 也不生效
            Some(PluginLifecycle::Panel) if source == PluginSource::Builtin => sink.field(
                Level::Note,
                "PLUGIN_LIFECYCLE",
                "内置模块一律常驻，这一句写了也不生效（它随程序一起发布，没有「面板关了就走」那一档）"
                    .to_string(),
            ),
            Some(_) => {}
        }
    }
}

/// 权限那两条双向校验里落在静态的一边：用必声明（提前一行说）、声明必用（lint）。
fn check_permissions(
    sink: &mut Sink<'_>,
    manifest: &PluginManifest,
    source: PluginSource,
    used: &[Use],
) {
    for unknown in permissions::unknown_permissions(&manifest.permissions) {
        sink.field(
            Level::Error,
            "PLUGIN_PERMISSIONS",
            format!(
                "里有 \"{unknown}\" —— 宿主没有这一项权限。已知的只有 {}；\
                 清单外的声明会被折掉，运行时也就不会为它装闸",
                permissions::KNOWN_PERMISSIONS.join(" / ")
            ),
        );
    }

    // 这一份用到了哪些权限，连同第一次用到它的那一行（与运行时同一把尺子：required_for）
    let mut used_perms: Vec<(&'static str, usize)> = Vec::new();
    for u in used {
        if let Some(p) = permissions::required_for(&u.api, u.verb.as_deref()) {
            if !used_perms.iter().any(|(q, _)| *q == p) {
                used_perms.push((p, u.line));
            }
        }
    }

    if source == PluginSource::User {
        for (p, line) in &used_perms {
            if !manifest.permissions.iter().any(|d| d == p) {
                sink.at(
                    Level::Error,
                    *line,
                    format!(
                        "用到了一格需要权限 \"{p}\" 的能力，而 PLUGIN_PERMISSIONS 里没声明它 —— \
                         运行时这一问会被拒绝（DENIED），页面上也会一直写着「需要授权」"
                    ),
                );
            }
        }
    }

    // 声明必用：方案里那句 declared_but_unused（验收第 16 条）
    for p in &manifest.permissions {
        // 清单外的那一项上面已经按"问题"报过一次，再叠一句"没用到"只是噪音：
        // 它连是不是权限都还没定，谈不上多不多要
        if !permissions::KNOWN_PERMISSIONS.contains(&p.as_str()) {
            continue;
        }
        if used_perms.iter().any(|(q, _)| q == p) {
            continue;
        }
        sink.field(
            Level::Note,
            "PLUGIN_PERMISSIONS",
            format!(
                "声明了 \"{p}\" 而整份代码里没有一处用到它 —— 这是多要的那一份权限：\
                 用户照这一份清单点的「同意」比插件实际能做的宽"
            ),
        );
    }

    // 没升到 2 的那一份：今天不装闸，所以"声明了权限"这件事本身是空话
    if source == PluginSource::User && manifest.api <= 1 && !used_perms.is_empty() {
        let (p, line) = used_perms[0];
        sink.at(
            Level::Note,
            line,
            format!(
                "这一格要权限 \"{p}\"，而 PLUGIN_API 没写到 2（现在按 {} 走）—— 这一档的插件不装\
                 权限闸，声明与页面上那颗「授权」都不生效（方案 3.7 那三道判断要 PLUGIN_API = 2）",
                manifest.api
            ),
        );
    }
}

/// 代码里点了宿主没有的那一格、或扩展位拿不到的那两格。
fn check_api_names(sink: &mut Sink<'_>, used: &[Use], source: PluginSource) {
    for u in used {
        if let Some(v) = &u.verb {
            if u.api == "store" && !permissions::store_verb_names().contains(&v.as_str()) {
                sink.at(
                    Level::Error,
                    u.line,
                    format!(
                        "`focusflow.store.{v}` 没有这个动词（认得的只有 {}）",
                        permissions::store_verb_names().join(" / ")
                    ),
                );
            }
            continue;
        }
        if EXTENSION_API_NAMES.contains(&u.api.as_str()) {
            continue;
        }
        if is_builtin_only_api(&u.api) {
            if source != PluginSource::User {
                continue;
            }
            sink.at(
                Level::Error,
                u.line,
                format!(
                    "用了 `{}` —— 这一格是内置模块专属（记账 / 番茄 / 定时任务 / Edge 那些随包面板\
                     的内部接口），装在数据目录里的插件拿不到它：加载时会从表里摘掉，调用得到 nil",
                    u.api
                ),
            );
            continue;
        }
        sink.at(
            Level::Error,
            u.line,
            format!("用了 `focusflow.{}` —— 宿主没有这一格，调用会是 nil", u.api),
        );
    }
}
