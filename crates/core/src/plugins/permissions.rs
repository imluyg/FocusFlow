//! 权限：声明 → 授权 → 运行时强制（方案 3.7）。
//!
//! 三道判断只在**扩展插件**这一侧走（3.4 那张表里"内置专属"那一枝由位置免授权）：
//!
//! 1. manifest 没声明这个权限 ⇒ 该调用被拒，并计入这一格的违规数；
//! 2. 声明了但 `permission_hash` 没有对应的授权记录 ⇒ 同样被拒，页面上要说「需要授权」；
//! 3. 两道都过了才进业务代码。
//!
//! 刻意**不在加载时对比**声明与实际调用：`init()` 里通常一个 API 都不调，第一次 `get_view()`
//! 才调 —— 加载那一刻判"声明了但没用"会把所有插件都误报成过度授权（方案 3.7 那句话）。
//! "声明必用"归校验器 lint（路线图第 14 步），这一头只管"用必声明"。
//!
//! 两个哈希各管一件事，不许合成一个：授权绑 `permission_hash`（权限集合没变 = 插件能做的事
//! 没变），`content_hash` 只用来提示「上次授权之后代码变了」。绑全文件的话，作者每修一个
//! typo 就要让所有用户重新点一遍同意，实际结局是用户不看提示一路点过去。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// 已定型的权限清单（方案 3.7 那张表里 v2 冻结的那几项）。
///
/// `net:<host>` 属于 P2、`fs` 永不开放，所以都不在这里：清单外的声明会被折叠掉并由
/// [`unknown_permissions`] 说出去，而运行时要用的权限必须先是这一份里的一项。
pub const KNOWN_PERMISSIONS: &[&str] = &[
    "stats.read",
    "storage.read",
    "storage.write",
    "timer",
    "events.app",
    "notify",
];

/// 一个宿主 API（`store` 那一格还要看动词）需要哪一项权限。
///
/// `None` = 这一问不要权限：`now` / `log` / `app_info` / `config_get` 是元信息与自述那一类，
/// 拿它们做授权弹窗只会把「FocusFlow 现在几点」也变成要点同意的东西。
/// 反过来，**新加通用 API 时必须在这里登记**，漏登记的症状是"这个 API 谁都能调、
/// 页面上却写着已授权"，而那条红不了任何用例 —— 所以 `every_api_asks_for_a_permission_or_is_listed`
/// 钉了一份"不要权限"的白名单，加一个就要在这里加一个。
pub fn required_for(api: &str, verb: Option<&str>) -> Option<&'static str> {
    if let Some(verb) = verb {
        return match (api, verb) {
            ("store", "get") | ("store", "list") => Some("storage.read"),
            ("store", "set") | ("store", "delete") | ("store", "clear") | ("store", "flush") => {
                Some("storage.write")
            }
            _ => None,
        };
    }
    match api {
        "stats" | "today_count" | "daily_counts" | "available_years" | "query" => {
            Some("stats.read")
        }
        "store" => None,
        "every" | "after" | "cancel" | "on" | "off" => Some("timer"),
        // `on` 目前只有 `tick` / `day_change` 两个事件，都是**按时间**的，所以归 `timer`。
        // 等 3.6 那第三条 `app_change`（前台应用切换）接上来，它才需要 `events.app` ——
        // 到那时是按事件名分权限、不是按 API 名，这一格的映射要跟着拆（写在用例里盯着）。
        "notify" => Some("notify"),
        _ => None,
    }
}

/// 不要权限的那几个（`None` 的那一侧），给用例当白名单用。
pub const PERMISSION_FREE_APIS: &[&str] = &[
    "now",
    "log",
    "app_info",
    "config_get",
    "invalidate",
    "timers",
];

/// 声明里认不得的那几项：原样交回，由调用方出声（方案 3.7 只有这几项是权限）。
pub fn unknown_permissions(declared: &[String]) -> Vec<String> {
    declared
        .iter()
        .filter(|p| !KNOWN_PERMISSIONS.contains(&p.as_str()))
        .cloned()
        .collect()
}

/// 顶层需要装闸的那些通用 API 名字 —— 从 3.4 那份通用清单里现筛，不另抄一份。
///
/// 抄两份的下场是"清单加了新通用 API、闸没跟上"：那一格从此谁都能调，而授权弹窗里
/// 看不见它，正是本节要治的那一类。筛法与判定共用 [`required_for`] 同一个函数。
pub fn guarded_api_names() -> Vec<&'static str> {
    crate::plugins::host::EXTENSION_API_NAMES
        .iter()
        .copied()
        .filter(|n| *n != "store" && required_for(n, None).is_some())
        .collect()
}

/// `store` 那一格里需要装闸的动词。与 [`required_for`] 的分支同一份来源：
/// 认不得的动词（`required_for` 回 `None`）不进名单，也就不会被悄悄放行。
pub fn store_verb_names() -> Vec<&'static str> {
    ["get", "list", "set", "delete", "clear", "flush"]
        .into_iter()
        .filter(|v| required_for("store", Some(v)).is_some())
        .collect()
}

/// 授权与运行时判定都按这一份规范化：排序 + 去重。
///
/// `PLUGIN_PERMISSIONS = { "storage.write", "stats.read" }` 与 `{ "stats.read",
/// "storage.write" }` 必须是同一个授权 —— 否则作者换个顺序就要用户重新点一遍同意。
pub fn canonical(declared: &[String]) -> Vec<String> {
    let mut set: HashSet<&str> = HashSet::new();
    for p in declared.iter().map(|s| s.as_str()) {
        set.insert(p);
    }
    let mut out: Vec<String> = set.into_iter().map(|s| s.to_string()).collect();
    out.sort();
    out
}

/// 内部那把 sha256，写成 `sha256:<小写十六进制>`（方案 3.7 的 `grants.json` 就是这一串）。
fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest.iter() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// 权限集合的哈希 = 授权绑的那一个。
pub fn permission_hash(declared: &[String]) -> String {
    let joined = canonical(declared).join("\n");
    format!("sha256:{}", sha256_hex(joined.as_bytes()))
}

/// 插件文件字节的哈希 = 只用于「代码已变更」那句提示。
///
/// 不做换行/编码归一（方案 3.3）：同一份文件在任何机器上必须是同一个值，归一反而会把
/// 平台差异引进来 —— 而引进来的症状是"授权在另一台机器上失效"。
pub fn content_hash(bytes: &[u8]) -> String {
    format!("sha256:{}", sha256_hex(bytes))
}

/// `plugins/state/<stem>/grants.json` 的那一份记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Grants {
    /// 授权绑它：不匹配 ⇒ 要求重新授权
    pub permission_hash: String,
    /// 只用于提示「上次授权后代码已变更」
    pub content_hash: String,
    /// 用户当时点同意的那一份权限（与 `permission_hash` 同源，留着是为了"新增了哪几项"能说出口）
    pub granted: Vec<String>,
}

const GRANTS_FILE: &str = "grants.json";

/// 授权文件的位置。名字越界的 `stem` 交 `None`：与 store 同一把尺子
/// （[`paths::is_safe_state_stem`]），否则插件写一个带分隔符的名字就能碰到别家的授权格。
fn grants_file(stem: &str) -> Option<PathBuf> {
    if !crate::paths::is_safe_state_stem(stem) {
        return None;
    }
    Some(crate::paths::plugin_state_path(stem).join(GRANTS_FILE))
}

/// 读这一份授权记录。`None` = 没有记录（从没授权过，或目录还没建）。
///
/// 读不动与读出来不是合法 JSON 各说一句（与 store 那一族同一口径）：把"读坏了"折成
/// "没授权过"，用户看到的是一句假话，而他手上那份明明点过同意。
pub fn read_grants(stem: &str) -> GrantRead {
    let Some(path) = grants_file(stem) else {
        return GrantRead::UnsafeStem;
    };
    match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<Grants>(&bytes) {
            Ok(g) => GrantRead::Found(g),
            Err(e) => GrantRead::Corrupt(format!(
                "{} 读不出授权记录（{e}）：这一格按\"没授权过\"处理，要重新授权请先删掉它",
                path.display()
            )),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => GrantRead::Absent,
        Err(e) => GrantRead::Unreadable(format!("{} 读不出来: {e}", path.display())),
    }
}

/// 读授权记录的四种结局（"没这条记录"与"问不出"必须是两个答案，本仓为这件事修过三轮）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantRead {
    /// 没有记录 = 从没授权过
    Absent,
    /// 有记录
    Found(Grants),
    /// 文件在但读不出来（占用、权限）
    Unreadable(String),
    /// 文件读出来了但不是合法 JSON
    Corrupt(String),
    /// 插件名越界：连"该去哪一格找"都不成立，不许回退到上一格读别人的授权
    UnsafeStem,
}

impl GrantRead {
    /// 交回记录本身（`Found` 之外都是 `None`），原因留在原处可由调用方说出去。
    pub fn grants(&self) -> Option<&Grants> {
        match self {
            Self::Found(g) => Some(g),
            _ => None,
        }
    }
}

/// 当场落一份授权记录（用户点「同意」的那一刻）。
///
/// 与 store 的写盘不同，这里**不去抖**：授权是一次点击的结果，页面上说完「已授权」就得
/// 已经在盘上，否则重启回到未授权那一档而插件页从没说过假话的那句会变成假话。
/// 走同一族的原子替换（`tmp` → `rename`）。
pub fn write_grants(stem: &str, grants: &Grants) -> Result<(), String> {
    let Some(path) = grants_file(stem) else {
        return Err(format!(
            "插件名的这一格越界了（{stem}）：授权只认插件文件名，本次没有落盘"
        ));
    };
    let dir = path
        .parent()
        .ok_or_else(|| "授权记录的路径没有上一层，写不下去".to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| format!("建授权目录失败: {e}"))?;
    let bytes = serde_json::to_vec_pretty(grants).map_err(|e| format!("序列化失败: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes).map_err(|e| format!("写临时授权文件失败: {e}"))?;
    // Windows 上 rename 不覆盖已存在的目标，先摘掉旧的
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| format!("换掉旧授权记录失败: {e}"))?;
    }
    std::fs::rename(&tmp, &path).map_err(|e| format!("换掉授权记录失败: {e}"))
}

/// 撤销授权（用户点「停用授权」或权限变了）：文件删掉，回到「没授权过」。
pub fn revoke_grants(stem: &str) -> Result<(), String> {
    let Some(path) = grants_file(stem) else {
        return Err(format!("插件名的这一格越界了（{stem}）：没有授权记录可删"));
    };
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("删授权记录失败: {e}")),
    }
}

/// 一份插件在授权这一头的结局，插件页直接念它（方案 16 那张表的「授权失效」那一行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Authorization {
    /// 内置模块与旧目录回滚那一档：免授权（方案 3.7 最后一句）
    Exempt,
    /// v1 老插件：按今天的行为跑，不弹授权（方案 5 的迁移表）
    LegacyApi,
    /// 已授权，且代码没变
    Granted,
    /// 已授权，但只改过代码：授权保留，页面提示「插件已更新」
    ContentChanged,
    /// `permission_hash` 对不上：权限集合变了，要重新授权
    PermissionChanged { newly: Vec<String> },
    /// 从没授权过
    NotGranted,
    /// 声明里一个权限都没有 ⇒ 没有要授权的东西
    NothingToGrant,
    /// 授权文件问不出结局（读不动 / 读坏了）：不许折成「没授权过」
    Undetermined(String),
}

impl Authorization {
    /// 插件页那一格的话。
    pub fn note(&self) -> String {
        match self {
            Self::Exempt => "内置模块，免授权".to_string(),
            Self::LegacyApi => "按旧版接口运行，不弹授权".to_string(),
            Self::Granted => "已授权".to_string(),
            Self::ContentChanged => "已授权 · 插件已更新（权限没变）".to_string(),
            Self::PermissionChanged { newly } => format!(
                "权限变了，需要重新授权（新增了 {}）",
                if newly.is_empty() {
                    "无".to_string()
                } else {
                    newly.join("、")
                }
            ),
            Self::NotGranted => "需要授权".to_string(),
            Self::NothingToGrant => "没声明权限".to_string(),
            Self::Undetermined(why) => format!("授权状态问不出来：{why}"),
        }
    }

    /// 需不需要用户点一次「授权」：只有这两档给按钮。
    pub fn needs_action(&self) -> bool {
        matches!(self, Self::NotGranted | Self::PermissionChanged { .. })
    }
}

/// 这次加载要不要走权限那道闸。
///
/// v1（没写 `PLUGIN_API`）不拦：方案 5 那张迁移表定的是"老第三方按 v1 处理，行为不变"，
/// 而今天满盘的脚本都没写这一项 —— 一上线就拦，等于把所有存量插件一次停掉。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateMode {
    /// 一律放行（内置、旧目录回滚、v1 插件）
    Free,
    /// 未声明的调用被拒并计数
    Enforce { granted: bool },
}

/// 一个插件的运行时闸（闭包按 `Arc` 持着，与插件同生共死）。
#[derive(Debug)]
pub struct Gate {
    /// 这一份接口走不走闸：内置、旧目录回滚与 v1 插件一律放行（见 [`GateMode`]）。
    enforcing: bool,
    /// 用户点没点过同意。**可以在运行中翻**：插件页上点「授权」不该要求重新加载一次
    /// —— 重载会再跑一遍 `init()`，而那是有副作用的一次初始化。
    granted: std::sync::atomic::AtomicBool,
    declared: HashSet<String>,
    /// 每次被拒都要出声一次太吵，而「同一 API 连续 3 次被拒就停用这一格」是方案 9.1 那条；
    /// 这里记的是**每个 API 连着被拒了几次**与是否已停用。
    denied: std::sync::Mutex<HashMap<String, u32>>,
}

/// 同一个 API 连续被拒几次之后，这一格在本次会话里停用（方案 9.1 最后那句）。
const DENY_LIMIT: u32 = 3;

impl Gate {
    pub fn new(mode: GateMode, declared: &[String]) -> Self {
        let (enforcing, granted) = match mode {
            GateMode::Free => (false, true),
            GateMode::Enforce { granted } => (true, granted),
        };
        Gate {
            enforcing,
            granted: std::sync::atomic::AtomicBool::new(granted),
            declared: declared.iter().cloned().collect(),
            denied: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// 用户当场点完授权：这一份闸门立刻放行，不必重新加载插件。
    pub fn set_granted(&self, value: bool) {
        self.granted
            .store(value, std::sync::atomic::Ordering::SeqCst);
        if value {
            // 授权之前那几笔被拒是**同一件事**的账，翻完档就该清零：否则刚点完同意就撞上
            // "这一格本次会话停用"，屏上说的与手上做的又是两回事
            self.denied
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
        }
    }

    /// 放行与否。返回的是**给用户看的那一句**（`Err` 的一侧），不是错误码。
    ///
    /// 三道判断的顺序照方案 3.7 那张流程图：先问声明，再问授权。顺序反了的症状是给一个
    /// 压根没声明这个权限的插件弹「请授权」，而用户点了也没用 —— 声明那一头就没写。
    pub fn check(&self, api: &str, perm: &str) -> Result<(), String> {
        if !self.enforcing {
            return Ok(());
        }
        let key = api.to_string();
        let mut tally = self.denied.lock().unwrap_or_else(|e| e.into_inner());
        if tally.get(&key).copied().unwrap_or(0) >= DENY_LIMIT {
            return Err(format!(
                "DENIED: focusflow.{api} 已连续 {DENY_LIMIT} 次被拒，本次会话里这一格对该插件停用\
                 （重新启用插件可恢复）"
            ));
        }
        let granted = self.granted.load(std::sync::atomic::Ordering::SeqCst);
        let verdict = if !self.declared.contains(perm) {
            Err(format!(
                "DENIED: focusflow.{api} 需要权限 {perm}，而这份插件没在 PLUGIN_PERMISSIONS \
                 里声明它（要用它请先声明，不要就别调）"
            ))
        } else if !granted {
            Err(format!(
                "DENIED: focusflow.{api} 需要权限 {perm}，用户还没授权（插件页点「授权」）"
            ))
        } else {
            Ok(())
        };
        if verdict.is_err() {
            // 只给这一格 +1，另两处（读授权、查声明）不共用计数：
            // 一个插件在三处被拒是同一次配置问题，但刷日志的是"每拍都调"这一族，按格数才停得对。
            *tally.entry(key).or_insert(0) += 1;
        } else {
            // 成功一次就清零：偶发一次误调（比如插件按分支走错了 API）不该攒成永久停用
            tally.remove(&key);
        }
        drop(tally);
        verdict
    }

    /// 这一格当前被拒的次数（用例与插件详情页要看得见代价）。
    pub fn denial_count(&self, api: &str) -> u32 {
        self.denied
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(api)
            .copied()
            .unwrap_or(0)
    }

    pub fn mode(&self) -> GateMode {
        if !self.enforcing {
            return GateMode::Free;
        }
        GateMode::Enforce {
            granted: self.granted.load(std::sync::atomic::Ordering::SeqCst),
        }
    }
}

/// 把授权那份读法折成页面上的一格结论。
///
/// `declared` 是清单里那份（已排序去重的）声明；`read` 是盘上的授权记录结局。
/// 这一份折叠只有这里一处：管理器加载时、插件页列出未加载的条目时都要念同一句话。
pub fn authorize(
    mode_free_reason: Option<Authorization>,
    declared: &[String],
    current_permission_hash: &str,
    current_content_hash: &str,
    read: &GrantRead,
) -> Authorization {
    if let Some(exempt) = mode_free_reason {
        return exempt;
    }
    if declared.is_empty() {
        return Authorization::NothingToGrant;
    }
    match read {
        GrantRead::Absent => Authorization::NotGranted,
        GrantRead::Unreadable(why) | GrantRead::Corrupt(why) => {
            Authorization::Undetermined(why.clone())
        }
        GrantRead::UnsafeStem => Authorization::Undetermined(
            "插件名越界，问不到属于它的那一格授权（授权只按插件文件名那一格记）".to_string(),
        ),
        GrantRead::Found(g) => {
            if g.permission_hash != current_permission_hash {
                let before: HashSet<&str> = g.granted.iter().map(|s| s.as_str()).collect();
                let newly: Vec<String> = declared
                    .iter()
                    .filter(|p| !before.contains(p.as_str()))
                    .cloned()
                    .collect();
                return Authorization::PermissionChanged { newly };
            }
            if g.content_hash != current_content_hash {
                return Authorization::ContentChanged;
            }
            Authorization::Granted
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// 哈希先要对得上公开向量：这一串是整套授权的键，算错了症状是"所有人重新授权一次"。
    #[test]
    fn the_hash_matches_the_published_sha256_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // 56 字节那一档跨过 `0x7f` 长度边界（分组补位的另一条臂）。
        // 这三个期望值是**独立算出来的**（另一份实现同一串字节），不是抄被测函数的输出：
        // 抄输出的话，这一条永远绿，而哈希算错的症状是"所有人重新授权一次"。
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    /// 换个顺序、写重一项都不许改变授权（否则作者的写法决定用户要不要重点同意）。
    #[test]
    fn the_permission_hash_ignores_order_and_duplicates() {
        let a = permission_hash(&p(&["storage.write", "stats.read"]));
        let b = permission_hash(&p(&["stats.read", "storage.write"]));
        let c = permission_hash(&p(&["stats.read", "stats.read", "storage.write"]));
        assert_eq!(a, b, "权限顺序变了就换了授权");
        assert_eq!(a, c, "写重一项也换了授权");
        assert!(
            a.starts_with("sha256:"),
            "写法要与 grants.json 那一句一致: {a}"
        );
        // 少一项必须换（这一条挡住"哈希把权限集合吞成一个定长串"的写法）
        assert_ne!(a, permission_hash(&p(&["stats.read"])));
    }

    /// 内容哈希按字节算：一行换行符的差别就要看得见。
    #[test]
    fn the_content_hash_follows_the_file_bytes() {
        assert_eq!(content_hash(b"a"), content_hash(b"a"));
        assert_ne!(content_hash(b"a\n"), content_hash(b"a\r\n"));
    }

    /// 每一个通用 API 要么登记了权限，要么进那份"不要权限"的白名单，不许两边都不沾。
    ///
    /// 两边都不沾的症状正是这套方案要治的那一类：能调、页面上写着已授权、而授权弹窗里
    /// 看不见它（`stats` 若漏登记就是这一种）。
    #[test]
    fn every_api_asks_for_a_permission_or_is_listed() {
        let needs_permission = [
            "stats",
            "today_count",
            "daily_counts",
            "available_years",
            "query",
        ];
        for api in needs_permission {
            assert_eq!(
                required_for(api, None),
                Some("stats.read"),
                "{api} 该要 stats.read"
            );
        }
        for api in [
            "store",
            "now",
            "log",
            "app_info",
            "config_get",
            "invalidate",
        ] {
            // store 是表里的表：顶层不要权限，动词各自要
            if api == "store" {
                assert_eq!(required_for(api, None), None);
                for verb in ["get", "list"] {
                    assert_eq!(
                        required_for(api, Some(verb)),
                        Some("storage.read"),
                        "store.{verb} 该要 storage.read"
                    );
                }
                for verb in ["set", "delete", "clear", "flush"] {
                    assert_eq!(
                        required_for(api, Some(verb)),
                        Some("storage.write"),
                        "store.{verb} 该要 storage.write"
                    );
                }
                // 认不得的动词不许悄悄放行
                assert_eq!(required_for("store", Some("nope")), None);
            } else {
                assert!(
                    PERMISSION_FREE_APIS.contains(&api),
                    "{api} 既没登记权限也不在免权限清单里"
                );
            }
        }
        assert_eq!(required_for("every", None), Some("timer"));
        assert_eq!(required_for("on", None), Some("timer"));
        assert_eq!(required_for("off", None), Some("timer"));
        assert_eq!(required_for("notify", None), Some("notify"));
        // `events.app` 现在是"清单里有、还没有人登记要用"的那一项：`app_change` 那个事件
        // 没接（3.6 的第三条），接它的时候才按事件名分权限。留在这里是有意的，
        // 但这条断言保证没人误以为 `on` 已经在吃它。
        assert!(
            KNOWN_PERMISSIONS.contains(&"events.app"),
            "清单里要有 events.app，留给 app_change"
        );
        // 内置专属那一组由 3.4 的视图挡住，不在这里登记（登记了也到不了这一头）
        for api in [
            "pomodoro_state",
            "accounting_add",
            "scheduler_tasks",
            "edge_counts",
        ] {
            assert_eq!(required_for(api, None), None, "{api} 不该出现在扩展侧");
        }
        assert_eq!(unknown_permissions(&p(&["fs", "stats.read"])), p(&["fs"]));
    }

    /// 三道判断的矩阵，一次点清（方案 3.7 那张流程表的每条边）。
    #[test]
    fn undeclared_is_asked_before_ungranted_and_both_are_denied() {
        // 没声明 + 没授权 ⇒ 说的是"没声明"，不是让用户去点一个点了也没用的按钮
        let gate = Gate::new(GateMode::Enforce { granted: false }, &p(&["stats.read"]));
        let e = gate.check("stats", "storage.write").unwrap_err();
        assert!(e.contains("没在 PLUGIN_PERMISSIONS 里声明"), "{e}");
        assert!(!e.contains("插件页点"), "没声明的事不该说成没授权: {e}");
        // 声明了 + 没授权 ⇒ 说的是"没授权"
        let e2 = gate.check("stats", "stats.read").unwrap_err();
        assert!(e2.contains("还没授权") && e2.contains("stats.read"), "{e2}");
        // 授权之后放行
        let ok = Gate::new(GateMode::Enforce { granted: true }, &p(&["stats.read"]));
        ok.check("stats", "stats.read").expect("该放行");
        // 免授权那一档一律放行
        let free = Gate::new(GateMode::Free, &p(&[]));
        free.check("stats", "storage.write").expect("内置不该被拦");
    }

    /// 连续三次被拒 ⇒ 这一格本次会话停用，而**成功一次就清零**。
    ///
    /// 清零这一半是防误伤：插件按分支走错一次 API 不该攒到永久停用（那症状是"用着用着
    /// 某格突然没了"，而页面上没有任何一句说为什么）。
    #[test]
    fn three_denials_disable_that_one_api_and_a_success_resets_it() {
        let gate = Gate::new(GateMode::Enforce { granted: false }, &p(&["stats.read"]));
        for i in 1..=3 {
            let e = gate.check("stats", "stats.read").unwrap_err();
            assert!(!e.contains("停用"), "第 {i} 次还该说原因，别说停用");
        }
        assert_eq!(gate.denial_count("stats"), 3);
        let fourth = gate.check("stats", "stats.read").unwrap_err();
        assert!(fourth.contains("停用"), "第四次该停用这一格: {fourth}");
        // 停用这一格不影响另一格，也不影响放行
        let other = gate.check("today_count", "stats.read");
        assert!(other.is_err(), "别的格也要过闸");
        let ok = Gate::new(GateMode::Enforce { granted: true }, &p(&["stats.read"]));
        for _ in 0..3 {
            ok.check("stats", "stats.read").expect("放行不该计数");
        }
        assert_eq!(ok.denial_count("stats"), 0);
    }

    /// 授权那份折叠：五档结局各要对得上盘上的事实，"问不出"不许折成"没授权过"。
    #[test]
    fn the_five_outcomes_each_name_what_they_saw() {
        let declared = p(&["stats.read"]);
        let ph = permission_hash(&declared);
        let ch = content_hash(b"script");
        let granted = Grants {
            permission_hash: ph.clone(),
            content_hash: ch.clone(),
            granted: declared.clone(),
        };
        let none = |a: GrantRead| authorize(None, &declared, &ph, &ch, &a);
        assert_eq!(
            none(GrantRead::Absent),
            Authorization::NotGranted,
            "没记录就该说需要授权"
        );
        assert_eq!(
            none(GrantRead::Found(granted.clone())),
            Authorization::Granted
        );
        // 只改代码 ⇒ 授权保留 + 提示已更新（方案 3.3 那句取舍）
        assert_eq!(
            none(GrantRead::Found(Grants {
                content_hash: content_hash(b"edited"),
                ..granted.clone()
            })),
            Authorization::ContentChanged
        );
        // 权限变了 ⇒ 点名新增了哪一项
        let wider = p(&["stats.read", "storage.write"]);
        let got = authorize(
            None,
            &wider,
            &permission_hash(&wider),
            &ch,
            &GrantRead::Found(granted.clone()),
        );
        assert_eq!(
            got,
            Authorization::PermissionChanged {
                newly: p(&["storage.write"])
            },
            "新增的权限必须在句子里点出来"
        );
        // 问不出 ≠ 没有：这一档必须原样把原因交出去
        let why = GrantRead::Unreadable("文件被占用".to_string());
        assert_eq!(
            none(why.clone()),
            Authorization::Undetermined("文件被占用".to_string())
        );
        assert!(why.grants().is_none());
        // 免授权那一档不吃后面的判断
        assert_eq!(
            authorize(
                Some(Authorization::Exempt),
                &declared,
                &ph,
                &ch,
                &GrantRead::Absent
            ),
            Authorization::Exempt
        );
        // 什么都没声明 ⇒ 没东西可授
        assert_eq!(
            authorize(None, &[], &ph, &ch, &GrantRead::Absent),
            Authorization::NothingToGrant
        );
    }

    /// 越界的 `stem` 既不能读也不能写：`../grants` 那一条路径纪律（方案 3.5 的硬约束）。
    #[test]
    fn an_escaping_stem_never_touches_the_grants_slot() {
        for evil in ["../grants", "..\\x", "a/b", "", ".."] {
            assert!(grants_file(evil).is_none(), "{evil} 该被挡在路径之外");
            let err = write_grants(evil, &Grants::default()).unwrap_err();
            assert!(err.contains("越界"), "{evil} 的写入口要说清为什么: {err}");
            revoke_grants(evil).unwrap_err();
        }
        // 读的一侧同形：不许回退到上一格去读别人的授权
        assert_eq!(read_grants("../grants"), GrantRead::UnsafeStem);
        assert_eq!(read_grants(""), GrantRead::UnsafeStem);
    }

    /// 点一次同意必须当场读得回来，而且要落在**备份会扫到的那一格**下。
    ///
    /// 为什么核位置：授权写在 `plugins/state/<stem>/` 之外（比如错接到 `data/` 根上），
    /// 读写两侧照样自洽、用例照样绿，而备份与体检扫的是 `plugins_state_dir()` 那一棵 ——
    /// 症状是"换机器/重装之后所有人都要重新点一遍同意"，那正好是方案 3.8 要避免的那件事。
    #[test]
    fn a_grant_written_now_reads_back_from_the_slot_the_backup_scans() {
        let _lock = crate::paths::test_app_dir_lock();
        let _app = crate::paths::test_app_dir("grants_roundtrip");
        let stem = "demo_plugin";
        assert_eq!(
            read_grants(stem),
            GrantRead::Absent,
            "一处没写过的目录该读成「没授权过」，不是「问不出」"
        );
        let record = Grants {
            permission_hash: permission_hash(&p(&["stats.read"])),
            content_hash: content_hash(b"local x = 1"),
            granted: p(&["stats.read"]),
        };
        write_grants(stem, &record).expect("授权该落得下去");
        assert_eq!(read_grants(stem), GrantRead::Found(record.clone()));
        let in_backup_slot = crate::paths::plugins_state_dir()
            .join(stem)
            .join("grants.json");
        assert!(
            in_backup_slot.is_file(),
            "授权没落在备份会扫的那一棵子树里: {}",
            in_backup_slot.display()
        );
        // 改一次内容（只改代码那一档）要看得见，否则「插件已更新」那句永远说不出口
        let edited = Grants {
            content_hash: content_hash(b"local x = 2"),
            ..record.clone()
        };
        write_grants(stem, &edited).expect("换一份记录该成功");
        assert_eq!(read_grants(stem), GrantRead::Found(edited));
        revoke_grants(stem).expect("撤销该成功");
        assert_eq!(read_grants(stem), GrantRead::Absent);
        revoke_grants(stem).expect("再撤一次是空操作，不该报错");
    }

    /// 判定那一头不许沾运行时类型（方案 3.4 那条判据在权限这一族的照法）。
    ///
    /// 权限模型是"这个插件能不能做这件事"，与脚本用 Lua 还是别的东西写无关。哪天这些判定
    /// 开始接 `Table`/`Function`，换运行时就要连授权与哈希一起重写 —— 而那正是 3.4 立这条
    /// 判据要挡住的方向，所以钉在源文件形状上，而不是只写在注释里。
    #[test]
    fn the_ruler_does_not_name_a_runtime() {
        let src = include_str!("permissions.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        for banned in ["mlua", "Lua", "Table", "Function", "Variadic"] {
            assert!(
                !prod.contains(banned),
                "权限判定里出现了运行时类型「{banned}」：这一份必须运行时无关"
            );
        }
    }
}
