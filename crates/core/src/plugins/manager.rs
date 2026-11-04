//! 插件管理器：扫描、加载、卸载、热重载。
//!
//! 设计约束：mlua 的 `Lua` 不是 Send（含 Rc），因此所有 Lua 操作必须在
//! 同一线程（GUI 主线程）执行。`PluginManager` 不跨线程共享，直接在
//! GUI 线程持有。热重载检测线程只扫描文件并发送"重载请求"到 channel，
//! 由 GUI 线程调用 `poll_reload_requests()` 实际执行重载。
//!
//! ⚠ 这半条链曾经**没人在生产里跑**：调用方原是旧桌面端的那条热重载，它随 WebView 一起
//! 删了，原生端到今天才接上（`desktop-native/src/main.rs`：管理器建起来时 `enable_hot_reload`，
//! 100ms 那一拍 `poll_reload_requests`）。接上之前的症状是改 `.lua` 要重启才生效、
//! 删 `.lua` 也要重启才停。
//!
//! 一条边界要说清：热重载只管**已经在跑的**那些。新扔进目录的 `.lua` 不在实例表里，
//! `reload_plugin` 找不到键就返回 false（见它的注释），所以它要等下一次进插件页或重启才加载 ——
//! 「自动把目录里每个陌生脚本装进 VM」不是这一条链的承诺。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::SystemTime;

use mlua::{HookTriggers, Lua, LuaOptions, StdLib, VmState};

use crate::config::FocusFlowConfig;
use crate::db;
use crate::paths;
use crate::plugins::host;
use crate::plugins::package;
use crate::plugins::permissions;
use crate::plugins::plugin_events;
use crate::plugins::plugin_health;
use crate::plugins::plugin_refresh;
use crate::plugins::plugin_timers;
use crate::plugins::resources;
use crate::plugins::PluginView;

/// 按键增量的取件口：写线程（或任何不能碰 Lua 的线程）只管往里并，GUI 线程一拍取一次。
///
/// 为什么要有这么一格：`Lua` 不是 Send，插件的 VM 只能在 GUI 线程上跑（模块顶部那条约束）。
/// 攒批那一头要"出锁再进 Lua"，唯一诚实的做法就是它只交出批，进 VM 的那一跳留在取件侧。
/// 尺寸上界是"这段时间里出现过几种键名"（同名相加），不是按键次数 —— 它不会随打字速度长。
static KEY_BATCH_QUEUE: std::sync::Mutex<Vec<(String, i64)>> = std::sync::Mutex::new(Vec::new());

/// 把一批按键增量并进取件口（同名相加，非正数的增量不算）。
pub fn queue_key_batch(pairs: Vec<(String, i64)>) {
    let mut q = KEY_BATCH_QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    for (name, n) in pairs {
        if n <= 0 {
            continue;
        }
        match q.iter_mut().find(|(k, _)| *k == name) {
            Some((_, have)) => *have += n,
            None => q.push((name, n)),
        }
    }
}

/// 取件：一次拿走全部。调用方（GUI 线程）负责在没人收的时候放回去。
pub fn take_key_batches() -> Vec<(String, i64)> {
    let mut q = KEY_BATCH_QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    std::mem::take(&mut *q)
}

/// 加载时读到的插件元数据：清单 + 宿主要知道的两个钩子存在性。
struct PluginMeta {
    manifest: PluginManifest,
    has_init: bool,
    has_view: bool,
}

/// 插件信息。
/// 「坏」那一档的窗口：最近 50 次里 3 次失败 ⇒ 停掉**那一条路线**，不是整个插件。
///
/// 为什么按路线而不是整个插件：视图、按键批次、定时/事件三条路各有各的受害者。
/// 按键钩子每次都抛错时，把整个插件卸掉会连面板一起没收；只停按键那一路，
/// 用户还能看视图、点按钮，页面上也写着是哪一路停了。`on_action` / `set_field`
/// 是用户点出来的一次调用，失败当场返回给点它的那个人，不设窗口。
const FAIL_WINDOW: usize = 50;
const FAIL_TRIPS: usize = 3;

/// 一条路线的失败窗。`record` 在第 3 次失败落进来的那一刻交回 true（只交回一次）。
#[derive(Default)]
struct RouteBreaker {
    outcomes: std::collections::VecDeque<bool>,
    stopped: Option<String>,
}

impl RouteBreaker {
    fn record(&mut self, failed: bool) -> bool {
        if self.stopped.is_some() {
            return false;
        }
        while self.outcomes.len() >= FAIL_WINDOW {
            self.outcomes.pop_front();
        }
        self.outcomes.push_back(failed);
        if self.outcomes.iter().filter(|failed| **failed).count() >= FAIL_TRIPS {
            // 先占位，调用方紧接着用 `stop_with` 填那句原因；中间没有可观察的窗口
            // （同一线程、同一次调用里）。
            self.stopped = Some(String::new());
            return true;
        }
        false
    }

    fn stop_with(&mut self, why: String) {
        self.stopped = Some(why);
    }

    fn stopped(&self) -> Option<&str> {
        self.stopped.as_deref()
    }
}

pub struct PluginInfo {
    /// 脚本自己声明的那份清单（加载时经 `read_meta` 折叠）
    pub manifest: PluginManifest,
    pub file_path: PathBuf,
    pub file_mtime: Option<SystemTime>,
    pub loaded: bool,
    pub error: Option<String>,
    /// 这一条出自哪一处目录 = 它的身份，决定它看见哪一组宿主 API（3.4）。
    ///
    /// 也是热重载的必需项：重载走的是"按已加载的那份文件重读一遍"，手上只有展示名/文件名，
    /// 若不在这里留住来源，重载一次就把内置专属 API 还给用户插件 —— 屏上什么都正常，
    /// 而边界只在下一次重载之前有效。
    pub source: PluginSource,
    /// 授权这一头的结论（插件页念这一句，方案 3.7「未授权不是拒绝加载，而是照常加载」）
    pub authorization: permissions::Authorization,
    /// 运行时那道闸（免授权的那一档也有一份，只是恒放行）：
    /// 用户点「授权」要能**当场翻开**这一份，而不是要求重新加载一次插件。
    pub gate: Arc<permissions::Gate>,
    /// 插件 Lua 环境（仅 GUI 线程访问）
    pub lua: Option<Lua>,
    /// 插件声明的视图（get_view() 结果缓存）
    pub view: Option<PluginView>,
    /// 三条路线各自的失败窗（3.9「坏」那一档，按路线分开）。
    view_route: RouteBreaker,
    key_route: RouteBreaker,
    event_route: RouteBreaker,
}

/// 插件出自哪一处目录，即它的**身份**。
///
/// 身份由位置决定，不由脚本自己写的常量决定（见 `docs/plugin-system-rework-plan.md` 3.1）：
/// 允许脚本写 `PLUGIN_KIND = "builtin"` 来免授权，权限模型第一天就失效。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginSource {
    /// `app_dir/plugins/builtin/`：随包发布的内置模块。
    Builtin,
    /// `data_dir/plugins/`：用户自己装的脚本插件。
    User,
    /// `[plugins] legacy_layout = true` 那条回滚分支里唯一被扫的旧目录（平铺的 `app_dir/plugins/`）。
    /// 这个变体只服务回滚，开关删掉时一起删。
    Legacy,
}

impl PluginSource {
    /// 给日志用的一行名字（启动那一行要说清两处各拿到几个）。
    fn label(self) -> &'static str {
        match self {
            Self::Builtin => "内置",
            Self::User => "用户",
            Self::Legacy => "旧目录",
        }
    }
}

/// 一处目录里扫到的一个插件文件（[`PluginManager::discover`] 的返回项）。
#[derive(Debug)]
pub struct Discovered {
    pub path: PathBuf,
    pub source: PluginSource,
}

/// 插件活多久（方案 3.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PluginLifecycle {
    /// 缺省：只在面板被打开时用到它。
    #[default]
    Panel,
    /// 常驻：面板关了也要继续收宿主投递（内置模块全是这一档 —— 番茄钟靠按键批次累计键数，
    /// 随面板卸载就断统计）。
    Always,
}

impl PluginLifecycle {
    /// 脚本里写的那个词。
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Panel => "panel",
            Self::Always => "always",
        }
    }

    /// 插件页那一格的话。
    pub fn label(self) -> &'static str {
        match self {
            Self::Panel => "面板",
            Self::Always => "常驻",
        }
    }

    /// 只认 `panel` / `always` 这两个词，拼错的折回缺省档并由调用方出声。
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw {
            "panel" => Some(Self::Panel),
            "always" => Some(Self::Always),
            _ => None,
        }
    }
}

/// 清单里那几个字段的缺省值（方案 3.3：v1 老插件什么都不写也是这套）。
const DEFAULT_PLUGIN_API: u32 = 1;

/// 本宿主认得上去的最高能力版本（方案 3.10：宿主支持 `1..=2`）。
///
/// 两个读者念同一个数：加载侧 [`PluginManager::load_plugin`] 那一道门（比它高就在执行脚本之前拒绝，
/// 并把这句话留在插件页那一行）与校验器（[`super::validate`]，让作者不用跑到运行时才知道）。
/// 升版本时必须当场回答这两处各自要不要跟着动 —— 只动一处的症状是"校验器放行、加载拒绝"。
pub const MAX_SUPPORTED_PLUGIN_API: u32 = 2;

/// 同时加载的常驻插件上限（方案 3.11 那张表：128 MB 总账 ÷ 16 MB 单插件上限 = 8）。
///
/// 为什么是"按启用顺序取前 8 个、其余标待加载"而不是静默丢掉：常驻插件是一份 VM 加一条
/// 可能存在的后台线程，装十个就是十份。页面上要看得见是谁被挤掉、为什么，
/// 以及要给得出路（先停用别的常驻插件）。
pub const MAX_ALWAYS_PLUGINS: usize = 8;

/// 这一条实际按哪一档活着。
///
/// 内置模块**由位置强制**常驻，用户装的认自己写的 `PLUGIN_LIFECYCLE`。两条都不看脚本里
/// 写的 `PLUGIN_KIND` —— 身份由位置说一次（3.1），这里只把那份身份翻译成存活档位。
/// 强制的那一半的理由：随包模块都靠宿主持续投递（番茄钟累计每个番茄的键数、定时任务到点
/// 触发），让脚本自己写 `panel` 就等于让一次升级把统计打断。
pub fn lifecycle_of(source: PluginSource, manifest: &PluginManifest) -> PluginLifecycle {
    if source == PluginSource::Builtin {
        PluginLifecycle::Always
    } else {
        manifest.lifecycle
    }
}

/// 一个脚本插件在文件顶层声明的清单（方案 3.3 的那 9 个字段）。
///
/// **两处生产、一份折叠**：`scan_meta_source`（纯文本，不执行代码）与 `read_meta`
/// （执行顶层代码后读全局）各自取到原始声明，折叠规则只在 `build_manifest` 一份。
/// 插件页那一行的身份来自前者，加载后的那一份来自后者；两边口径不同就是"同一个插件画出
/// 两张脸"—— 本文件对 `PLUGIN_NAME` 早就为这件事钉过判据（见 `scan_meta` 的说明），
/// 这 5 个新字段同理，所以合进同一个结构，让"两处等值"能整体断言。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginManifest {
    /// 展示名（`PLUGIN_NAME`，缺省 = 文件名）
    pub name: String,
    /// 简介（`PLUGIN_DESC`，缺省空）
    pub desc: String,
    /// 版本（`PLUGIN_VERSION`，缺省 `1.0`）
    pub version: String,
    /// 作者（`PLUGIN_AUTHOR`，缺省空）
    pub author: String,
    /// 能力版本（`PLUGIN_API`，缺省 1）
    pub api: u32,
    /// 声明的能力名（`PLUGIN_PERMISSIONS`，已排序去重；将来授权哈希绑这一份）
    pub permissions: Vec<String>,
    /// 面板自刷间隔（`PLUGIN_REFRESH_MS`，缺省 0 = 只在动作后刷新）
    pub refresh_ms: u64,
    /// 存活档位（`PLUGIN_LIFECYCLE`，缺省 [`PluginLifecycle::Panel`]）
    pub lifecycle: PluginLifecycle,
    /// 要求的最低程序版本（`PLUGIN_MIN_APP`，缺省空 = 不限）
    pub min_app: String,
}

impl Default for PluginManifest {
    fn default() -> Self {
        PluginManifest {
            name: String::new(),
            desc: String::new(),
            version: "1.0".to_string(),
            author: String::new(),
            api: DEFAULT_PLUGIN_API,
            permissions: Vec::new(),
            refresh_ms: 0,
            lifecycle: PluginLifecycle::default(),
            min_app: String::new(),
        }
    }
}

/// 两侧各自的"原始声明读数"：取法不同（文本扫描 / Lua 全局），折叠前必须长得一样。
///
/// `None` = 脚本没声明这一项（折成缺省值），不是"声明了但读坏了"。
///
/// 这份原始读数对校验器（[`super::validate`]）也是输入：它要说的正是"写了但会被折掉"
/// 那一类，折叠之后就问不出来了。
#[derive(Debug, Clone)]
pub(crate) struct DeclaredFields {
    /// 展示名已经带上各自的缺省（文件名），因为两处缺省来源相同、不必再折一次
    pub(crate) name: String,
    pub(crate) desc: Option<String>,
    pub(crate) version: Option<String>,
    pub(crate) author: Option<String>,
    pub(crate) api: Option<i64>,
    pub(crate) permissions: Option<Vec<String>>,
    pub(crate) refresh_ms: Option<i64>,
    pub(crate) lifecycle: Option<String>,
    pub(crate) min_app: Option<String>,
    /// 出声时自报家门的那一句（「扫描 x.lua」/「加载 x.lua」），一条规则两处报
    pub(crate) where_: String,
}

/// 文本侧的原始声明读数：`scan_meta_source` 与校验器（[`super::validate`]）共用这一份取法。
///
/// 立它的原因是"复用同一套解析器"这句要求本身：校验器若自己再抄一遍"`KEY = 值` 怎么算
/// 顶层赋值"，两侧就会对同一份文件给出不同的读数 —— 作者按校验器的话改完，加载侧照旧折。
/// 抹注释、只认第 0 列、`local` 不算全局这些规矩全在 [`strip_lua_non_code`] 与
/// [`top_level_string_assign`] / [`top_level_int_assign`] / [`top_level_table_strings`] 里，
/// 这里只负责把它们按九个字段各调一次。
pub(crate) fn declared_fields(text: &str, stem: &str, where_: &str) -> DeclaredFields {
    let code = strip_lua_non_code(text);
    let field = |key: &str| -> Option<String> {
        code.lines()
            .find_map(|line| top_level_string_assign(line, key))
    };
    let number = |key: &str| -> Option<i64> {
        code.lines()
            .find_map(|line| top_level_int_assign(line, key))
    };
    DeclaredFields {
        name: field("PLUGIN_NAME").unwrap_or_else(|| stem.to_string()),
        desc: field("PLUGIN_DESC"),
        version: field("PLUGIN_VERSION"),
        author: field("PLUGIN_AUTHOR"),
        api: number("PLUGIN_API"),
        permissions: top_level_table_strings(&code, "PLUGIN_PERMISSIONS"),
        refresh_ms: number("PLUGIN_REFRESH_MS"),
        lifecycle: field("PLUGIN_LIFECYCLE"),
        min_app: field("PLUGIN_MIN_APP"),
        where_: where_.to_string(),
    }
}

/// 把原始声明折成一份清单：缺省值与非法值的折叠**只有这一份**。
///
/// 非法值（`PLUGIN_API = 0`、`PLUGIN_REFRESH_MS = -5`、`PLUGIN_LIFECYCLE = "Always"`）
/// 一律折回缺省档并出声一次：静默折叠的症状是"作者写了却像没写"，而这一族缺陷
/// 在本仓已经修过两次（把"读不出来"印成"没有"）。
pub(crate) fn build_manifest(raw: DeclaredFields) -> PluginManifest {
    let where_ = raw.where_.as_str();
    let api = match raw.api {
        Some(n) if (1..=u32::MAX as i64).contains(&n) => n as u32,
        Some(n) => {
            tracing::warn!(
                "{where_} 的 PLUGIN_API = {n} 不是一个正整数，按缺省 {DEFAULT_PLUGIN_API} 走"
            );
            DEFAULT_PLUGIN_API
        }
        None => DEFAULT_PLUGIN_API,
    };
    let refresh_ms = match raw.refresh_ms {
        // 非负就行：`u64::MAX as i64` 是 -1，拿它当上界会把每一个合法值都折回缺省
        // （第一版就是这么红的 —— 写夹具的人看不出错，只有断言看得出）。
        Some(n) if n >= 0 => n as u64,
        Some(n) => {
            tracing::warn!("{where_} 的 PLUGIN_REFRESH_MS = {n} 不是一个非负整数，按缺省 0（只在动作后刷新）走");
            0
        }
        None => 0,
    };
    let lifecycle = match raw.lifecycle.as_deref() {
        None => PluginLifecycle::default(),
        Some(word) => PluginLifecycle::parse(word).unwrap_or_else(|| {
            tracing::warn!(
                "{where_} 的 PLUGIN_LIFECYCLE = \"{word}\" 认不出来（只认 panel / always），\
                 按缺省 {} 走",
                PluginLifecycle::default().keyword()
            );
            PluginLifecycle::default()
        }),
    };
    let mut permissions = raw.permissions.unwrap_or_default();
    permissions.sort();
    permissions.dedup();
    PluginManifest {
        name: raw.name,
        desc: raw.desc.unwrap_or_default(),
        version: raw.version.unwrap_or_else(|| "1.0".to_string()),
        author: raw.author.unwrap_or_default(),
        api,
        permissions,
        refresh_ms,
        lifecycle,
        min_app: raw.min_app.unwrap_or_default().trim().to_string(),
    }
}

/// 目录中发现的插件（含已停用的），供插件管理页展示。
#[derive(Debug)]
pub struct DiscoveredPlugin {
    /// 清单：展示名、简介、版本、作者 + 方案 3.3 新加的 5 个声明字段。
    ///
    /// 已加载的取 `read_meta` 那一份，未加载的取 `scan_meta_source` 那一份 ——
    /// 两条路的折叠共用 `build_manifest`，所以同一份文件在两处只会有一张脸
    /// （`manifest_scan_and_load_agree_on_one_manifest` 钉这条）。
    pub manifest: PluginManifest,
    /// 文件名（不含扩展名）：启用状态的持久化标识，插件改名不影响配置
    pub file: String,
    pub enabled: bool,
    /// 当前是否已加载进内存
    pub loaded: bool,
    /// 加载错误信息（启用但加载失败时展示）
    pub error: Option<String>,
    /// 出自哪一处目录 = 它是内置模块还是用户装的插件。
    ///
    /// 今天的读者有两个：启动那一行日志（`load_all`）与插件页的两栏分组；
    /// [`Self::lifecycle`] 也按它决定内置要不要强制常驻。
    pub source: PluginSource,
    /// 授权这一头的结论（页面念这一句，并按它决定给不给「授权」那颗按钮）。
    ///
    /// 已加载的那一条取加载时算好的那一份，未加载的按同一份规则现读一次文件算 ——
    /// 两处必须同一句话，否则同一份文件在页面上会有两张脸（本仓为 `PLUGIN_NAME`
    /// 那件事钉过同一条判据）。
    pub authorization: permissions::Authorization,
    /// 这一格的面板是不是被降频了（方案 3.9：慢熔断之后页面要标「该插件较慢」）。
    ///
    /// 只有打开过面板、且攒够样本的那一份才可能是真；没加载的一律 `false`
    /// —— 「这一页从来没跑过」与「它跑得慢」是两种话，不许折成同一句。
    pub slow: bool,
    /// 这一格**为什么没活着**（`panel` 那一档在等面板打开、常驻额度已满时排在本轮之外）。
    ///
    /// 方案 16 那条底线：所有失败都要在插件页留下原因，不许出现"未加载而什么都没说"。
    /// 这一格与 `error` 分开的理由也一样：`error` 说的是"试过了、坏了"，而这里是"故意没试"。
    pub note: Option<String>,
}

impl DiscoveredPlugin {
    /// 这一条实际该按哪一档活着：内置模块**由位置强制**常驻，用户装的认自己写的。
    ///
    /// 为什么内置不许自己说不：随包的模块都靠宿主持续投递（番茄钟累计每个番茄的键数、
    /// 定时任务到点触发），将来「面板关掉就卸载」真落地时，让脚本自己写 `panel`
    /// 就等于让一次升级把统计打断。反过来，强制也不看脚本里写了什么 `PLUGIN_KIND`
    /// —— 身份由位置说一次（3.1），这里只是把那份身份翻译成存活档位。
    pub fn lifecycle(&self) -> PluginLifecycle {
        lifecycle_of(self.source, &self.manifest)
    }
}

/// 随包内置模块的文件名白名单。
///
/// 迁移旧平铺目录时用它把「随包的内置模块」与「用户自己装的第三方脚本」分开：白名单里的
/// 名字由 `plugins/builtin/` 那一份提供（随包发布、升级覆盖），旧目录里的同名残留**不搬进
/// 数据目录** —— 搬过去就变成两条同 stem 的清单条目，而"用户改过的内置脚本从此以用户插件
/// 的身份活着"也与「升级覆盖内置脚本」这条既有行为相反。
const BUILTIN_PLUGIN_STEMS: &[&str] = &[
    "accounting_plugin",
    "pomodoro_plugin",
    "scheduler_plugin",
    "edge_history_plugin",
    "stats_overview",
];

/// 迁移标记的文件名，落在数据目录那一侧（`data/plugins/.migrated`）。
const MIGRATION_MARKER: &str = ".migrated";

/// 「同一件事本进程只说一次」的登记处。
///
/// 为什么需要它：`discover()` 不是只在启动跑一次，插件页开着时每秒重扫一遍。迁移这件事
/// 里的冲突、失败、旧目录残留都是**盘上的稳定状态**，每秒说一次就把日志刷满，而一次说清
/// 才是有用的那条。按完整路径记，用例之间各用各的临时目录，互不吃对方的登记。
static NOTICED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::OnceLock::new();

/// 这个键第一次出现时返回 `true`（该出声），之后返回 `false`（说过就别再刷）。
fn notice_once(key: &str) -> bool {
    let slot = NOTICED.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    slot.lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key.to_string())
}

/// 旧平铺目录搬进数据目录的一轮结果（`migrate_legacy_plugins` 交回，由调用方决定怎么说）。
#[derive(Debug, Default, PartialEq, Eq)]
struct MigrationReport {
    /// 这一轮复制成功的文件名（不带 `.lua`）
    copied: Vec<String>,
    /// 数据目录已有同名文件 ⇒ 不动它，以数据目录那份为准
    conflicts: Vec<String>,
    /// 白名单里的内置名字：由 `plugins/builtin/` 提供，不搬
    builtin_kept: Vec<String>,
    /// 复制失败的文件名 + 原因
    failed: Vec<(String, String)>,
    /// 旧目录读不出来的原因（`None` = 读得动，含"目录本来就不存在"）
    unreadable: Option<String>,
}

/// 每个插件 VM 的内存上限（MB）。**唯一出处**：装载 VM 的那一处与插件页顶上那一行都读这里。
///
/// 上限要夹：`memory_limit_mb` 来自可以手改的 config.ini，天文数字会在
/// `* 1024 * 1024` 处溢出成 0（release 不做溢出检查），于是 `set_memory_limit(0)`
/// —— 每个插件立刻 MemoryError；0 或负数同样等于别的意思。4096 MB 之外没有合理用途。
/// 页面若自己 `parse` 原始值，就会出现"屏上说 999999 MB、VM 实际 4096"那种两套账。
pub fn memory_limit_mb(config: &FocusFlowConfig) -> usize {
    config
        .get_int("plugins", "memory_limit_mb", 16)
        .clamp(1, 4096) as usize
}

/// 插件管理器（GUI 线程专用，不跨线程共享）。
pub struct PluginManager {
    config: &'static FocusFlowConfig,
    db: Arc<db::Database>,
    plugins: HashMap<String, PluginInfo>,
    /// 热重载停止标志
    stop_event: Arc<AtomicBool>,
    /// 热重载检测线程句柄
    hot_reload_thread: Option<std::thread::JoinHandle<()>>,
    /// 加载失败过的插件（按文件名记下最后一次错误）：
    /// 失败的文件不在 `plugins` 里，不另记一笔的话插件页就只能显示"没加载、也没原因"。
    load_errors: HashMap<String, String>,
    /// 重载请求接收端（GUI 线程 poll）
    reload_rx: mpsc::Receiver<String>,
    /// 重载请求发送端（检测线程用）
    reload_tx: mpsc::Sender<String>,
    /// 每个**打开过面板**的插件一份刷新账本（键 = 展示名，与 `plugins` 同键）。
    ///
    /// 为什么按展示名而不是文件名：面板那侧唯一拿得到的就是名字（`open_plugin_view(name)`），
    /// 而这一格账本只为"这一页现在该不该重算"服务。
    plans: HashMap<String, plugin_refresh::Cadence>,
    /// 被分流出去、这一轮**故意没加载**的插件：文件名 → 那句原因。
    ///
    /// 两个来源：`panel` 那一档等面板打开、常驻额度满了的排在本轮之外。
    /// 这一格必须有，否则插件页上"已启用、未加载"与"加载失败"之外会多出第三种安静 ——
    /// 用户只能猜。它不参与判重，只给页面念一句话。
    pending: HashMap<String, String>,
}

impl Drop for PluginManager {
    fn drop(&mut self) {
        // 停止热重载线程，避免退出后仍扫描
        self.stop_event.store(true, Ordering::SeqCst);
        if let Some(handle) = self.hot_reload_thread.take() {
            let _ = handle.join();
        }
        // 卸载所有插件（调用 cleanup，释放 Lua 环境）
        let names: Vec<String> = self.plugins.keys().cloned().collect();
        for name in names {
            self.unload_plugin(&name);
        }
    }
}

/// `[plugins] instruction_limit` 没写或写成 0 时用的默认预算（条 Lua 指令 / 每次调用）。
const DEFAULT_INSTRUCTION_LIMIT: i64 = 10_000_000;

/// 上面那个默认值允许被配置抬到多高；再高就是笔误，按笔误处理（见
/// [`instruction_budget_from`]）。
const MAX_INSTRUCTION_LIMIT: i64 = 100_000_000;

/// `instruction_limit` 的取值口径：**笔误不许等于"没有上限"**。
///
/// 旧写法把配置值一律夹到 `u32::MAX` 封顶（0 到最大之间全收），于是多敲一个 0（
/// `instruction_limit = 999999999999`）被夹成 42.9 亿 —— 而 `every_nth_instruction` 的语义是"跑满这么多条
/// Lua 指令才查一次钩子"，一次 UI 调用里根本到不了第二次检查，
/// `while true do end` 照旧把主线程冻死；隔壁 `apply_lua_limits` 里那句
/// 「防 `while true do end` 冻结主线程」的注释就此变成一句空话。
/// 同一把尺子在 `memory_limit_mb` 那一处是**回落到默认值**（超出按笔误），
/// 这一处却往上夹到最大 —— 两处不同、而且错的偏偏是危险的那一边。
fn instruction_budget_from(cfg_value: i64) -> u32 {
    match cfg_value {
        v if v <= 0 => DEFAULT_INSTRUCTION_LIMIT as u32,
        v if v > MAX_INSTRUCTION_LIMIT => DEFAULT_INSTRUCTION_LIMIT as u32,
        v => v as u32,
    }
}

/// 控件表允许的最大嵌套层数（见 `parse_widget`）。
const MAX_WIDGET_DEPTH: u32 = 32;

/// 「这一轮没列出这个插件文件名」的两种原因要分开说。
///
/// 走到这里时目录已经**读全了**（`discover()` 读不出目录会直接交 Err，`set_enabled`
/// 在它那一步就返回，不会落到这句话），所以"列表里没有"只剩盘上真没有这个名字、
/// 或这一瞬间目录被换成另一份两种可能。带路径分隔或 `..` 的名字**不走存在性检查**：
/// 那是把这里变成"文件存在预言机"，而这个 crate 的插件表本就是外部数据
/// （沙箱那段的注释防的就是这类通道）。
///
/// `dirs` 是这一轮真正被扫的那几处（内置 + 数据目录，回滚开关打开时只有旧目录一处）：
/// 只问其中一处的话，另一处明明有这个名字的文件会被说成「插件文件不存在」。
fn missing_plugin_reason(dirs: &[PathBuf], stem: &str) -> String {
    let simple =
        !stem.is_empty() && !stem.contains('/') && !stem.contains('\\') && !stem.contains("..");
    if simple && dirs.iter().any(|d| d.join(format!("{stem}.lua")).is_file()) {
        return format!("插件目录这一轮没扫到 {stem}（目录被占用或正在整份替换），请再试一次");
    }
    format!("插件文件不存在: {stem}")
}

/// 把旧版平铺在 `app_dir/plugins/` 的用户插件搬进数据目录（runbook 12.2 的前三条）。
///
/// 四条纪律，一条都不能少：
/// · **只复制、不移动**：旧目录的原件全留。它是回滚的退路 —— `[plugins] legacy_layout = true`
///   只认旧目录，搬走等于把那条开关变成"请重新下载一遍插件"。
/// · **同名不覆盖**：数据目录那一份赢。用户已经在新目录放过同名文件，那才是他要跑的。
/// · **复制失败只记 error、不阻塞**：权限不足、文件被占用、磁盘满都只让那一个文件留在原地。
///   把"搬不动"变成"起不来"是不可逆操作里最糟的一种错法。
/// · 白名单里的内置名字不搬（见 [`BUILTIN_PLUGIN_STEMS`]），由 `plugins/builtin/` 那份提供。
///
/// 两个目录都是参数（不读进程全局）：用例按路径钉每一条纪律，别的用例换过一次 `app_dir`
/// 就不该影响这里。`NotFound` 算"旧目录里什么都没有"，与 [`list_plugin_files`] 同一个口径。
fn migrate_legacy_plugins(legacy_dir: &Path, user_dir: &Path) -> MigrationReport {
    let mut report = MigrationReport::default();
    let files = match list_plugin_files(legacy_dir) {
        Ok(files) => files,
        Err(e) => {
            report.unreadable = Some(e);
            return report;
        }
    };
    for path in files {
        let stem = PluginManager::stem_of(&path);
        if BUILTIN_PLUGIN_STEMS.contains(&stem.as_str()) {
            report.builtin_kept.push(stem);
            continue;
        }
        let dst = user_dir.join(path.file_name().unwrap_or_default());
        if dst.exists() {
            report.conflicts.push(stem);
            continue;
        }
        // 目标目录还没建（全新安装、或上一轮没走到这里）：建不出来的原因交给下面的 copy 报，
        // 不在这里单独分一支 —— 那一支的结局同样是"没搬动，只记 error"。
        match std::fs::copy(&path, &dst) {
            Ok(_) => report.copied.push(stem),
            Err(e) => report.failed.push((stem, e.to_string())),
        }
    }
    report
}

/// 跑一轮迁移并把结局说干净（`discover()` 的新布局分支调用；它自己不报错也不让谁报错）。
///
/// 标记（12.2 第 4 步）写在数据目录那一侧，语义是「已经搬过一轮」：搬成功之后不再重搬。
/// 这里刻意**照旧每轮读一次旧目录**，因为今天装插件的唯一办法就是往目录里扔文件 ——
/// 若按标记整个跳过，扔进旧目录的新插件会永远不被加载而一声不吭（静默失效）。
/// 跳过的是复制，报出来的是"旧目录里有一份数据目录没有的名字"，用户照着搬或删标记都能修。
///
/// 只复制一次：`[plugins] legacy_layout = true` 那条回滚分支根本不走这里。
fn run_legacy_migration(legacy_dir: &Path, user_dir: &Path) {
    let migrated = user_dir.join(MIGRATION_MARKER);
    let already = migrated.is_file();
    let report = migrate_legacy_plugins(legacy_dir, user_dir);

    if let Some(e) = &report.unreadable {
        // 旧目录读不出来不影响新布局：内置在程序目录、用户在数据目录，两处都不靠它。
        tracing::error!("{e}；旧目录这一轮搬不动也不影响加载（回滚开关才读它）");
        return;
    }
    for (stem, e) in &report.failed {
        tracing::error!(
            "旧插件没搬进数据目录（{stem}）：{e} —— 原件留在 {}，本次继续用数据目录那份清单，\
             不阻塞启动",
            legacy_dir.display()
        );
    }
    for stem in &report.conflicts {
        if notice_once(&format!("conflict:{}", stem)) {
            tracing::warn!(
                "旧目录与数据目录各有一份 {stem}：不动数据目录那份（以它为准），\
                 {} 里的同名原件保留不覆盖",
                legacy_dir.display()
            );
        }
    }
    for stem in &report.builtin_kept {
        if notice_once(&format!("builtin:{}", stem)) {
            tracing::info!(
                "{stem} 是内置模块，由 {} 那一份提供，不搬进数据目录",
                legacy_dir.display()
            );
        }
    }
    if !report.copied.is_empty() && !already {
        // 标记只在"这一轮真的搬了东西"之后写：一次都没搬成（全新安装、旧目录本来就空）
        // 留着它没有意义，下一次启动该照样看见用户新扔进来的文件。
        if let Err(e) = std::fs::write(&migrated, b"legacy plugins copied\n") {
            tracing::warn!(
                "迁移标记写不进去（{}）：{e} —— 下次启动会重搬一轮，同名不覆盖，无害",
                migrated.display()
            );
        }
        tracing::info!(
            "旧目录的 {} 个用户插件已复制到数据目录（原件没动）：{}",
            report.copied.len(),
            report.copied.join(", ")
        );
    }
    if already {
        // 搬过一轮之后，旧目录里再出现"数据目录没有的"名字 = 用户往一个不再被扫的目录里
        // 扔了文件。不说出来的话，症状就是"我放了插件但插件页没有"。
        let user_stems: Vec<String> = list_plugin_files(user_dir)
            .unwrap_or_default()
            .iter()
            .map(|p| PluginManager::stem_of(p))
            .collect();
        for stem in list_plugin_files(legacy_dir)
            .unwrap_or_default()
            .iter()
            .map(|p| PluginManager::stem_of(p))
            .filter(|s| !BUILTIN_PLUGIN_STEMS.contains(&s.as_str()))
            .filter(|s| !user_stems.iter().any(|u| u == s))
        {
            if notice_once(&format!("stale:{stem}")) {
                tracing::warn!(
                    "{stem}.lua 只在旧目录 {} 里，而迁移标记已在（不再重搬）：\
                     把它复制到 {}，或删掉 {} 让下次启动重搬一轮",
                    legacy_dir.display(),
                    user_dir.display(),
                    migrated.display()
                );
            }
        }
    }
}

/// 路径上最深的那一格存在、但它不是目录 ⇒ 这个"不存在"的目录是被**文件堵住**的。
///
/// `read_dir` 对两种情况都报 `NotFound`：父级整条不存在（全新安装，真的一个插件都没有），
/// 以及父级里有一格是普通文件（`app_dir/plugins` 被建成同名文件，它底下的 `builtin/`
/// 就取不到了）。后者的症状是"5 个插件全在盘上，插件页却写着暂无插件" ——
/// 把它折成空清单正是这套护栏当初要修的那件事。
fn blocked_by_a_file(dir: &Path) -> bool {
    dir.ancestors()
        .take_while(|p| p.parent().is_some())
        .find(|p| p.try_exists().unwrap_or(false))
        .map(|p| !p.is_dir())
        .unwrap_or(false)
}

/// 读一个插件目录，回 `(排序后的 .lua 清单)`，或者**这个目录读不出来的原因**。
///
/// 三种结局要分开：
/// · 读得动 ⇒ `Ok(清单)`，哪怕清单是空的（那就是"目录里真没有插件"）；
/// · `NotFound` 且路径上没有被文件堵住 ⇒ `Ok(空)`：整个目录不存在就是"一个插件都没有"，
///   全新安装是这个形状，把它报成错误会让第一次打开插件页的人看见一句红字"读不出来"；
/// · 其余错误码，或 `NotFound` 但被一格普通文件堵住 ⇒ `Err(带上路径与原因)`：
///   `plugins` 被建成同名文件、整份放在断线的网盘上、移动盘休眠、杀软首扫握着句柄、
///   构建脚本正在整份替换目录 —— 这些时候盘上**有**插件，
///   把它们折成空清单就是谎报"暂无插件"。
///
/// 单独收一个 `dir` 参数（而不是挂在 `&self` 上）是为了让三种结局都能被用例直接按路径钉住：
/// `plugins_dir()` 走的是进程全局的 app_dir，别的用例换过一次就会漂。
/// 插件目录里的源文件：单文件 `.lua` 与安装进来的 `.zip` 包都算（P2 第 16 步）。
///
/// 包里那份 `plugin.lua` 不在这一层展开：磁盘上的一格插件**就是一个文件**，
/// stem 仍然是文件名 —— 加载时才去包里取脚本（见 `package::read_script`）。
fn list_plugin_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e == "lua" || e.eq_ignore_ascii_case("zip"))
            })
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !blocked_by_a_file(dir) => {
            return Ok(Vec::new())
        }
        Err(e) => {
            return Err(format!(
                "插件目录读不出来（{}）：{e} —— 这不是「没有插件」",
                dir.display()
            ))
        }
    };
    files.sort();
    Ok(files)
}

/// 这一处目录里 `stem` 的源文件：单文件 `.lua` 排在包 `.zip` 之前。
///
/// 两个读者（热重载的"文件还在不在"、安装那一侧的同名检查）读同一把尺子：
/// 两处各判一遍的话，症状是包删掉了而热重载以为它还在、或者反过来。
fn plugin_file_for(dir: &Path, stem: &str) -> Option<PathBuf> {
    for ext in ["lua", "zip"] {
        let candidate = dir.join(format!("{stem}.{ext}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

impl PluginManager {
    pub fn new(config: &'static FocusFlowConfig, db: Arc<db::Database>) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            config,
            db,
            plugins: HashMap::new(),
            load_errors: HashMap::new(),
            stop_event: Arc::new(AtomicBool::new(false)),
            hot_reload_thread: None,
            reload_rx: rx,
            reload_tx: tx,
            plans: HashMap::new(),
            pending: HashMap::new(),
        }
    }

    /// 创建沙箱化的 Lua 状态：
    /// - 剔除 `io` 库（任意文件读写）；
    /// - 移除 `os` 中可触达系统的高危函数（保留 date/time/clock 供插件使用）；
    /// - 禁用 `package.loadlib`/`cpath`（防加载任意 DLL）；
    /// - 摘掉 base 库的 `dofile`/`loadfile`（base 由 mlua 无条件打开，白名单挡不住）；
    /// - 不放行 `coroutine` 库（见下方 hook 说明）。
    ///
    /// 配合 `apply_lua_limits` 的内存/指令数限制构成完整沙箱。
    fn create_sandboxed_lua() -> mlua::Result<Lua> {
        // 显式白名单，不用 ALL_SAFE：后者含 io 库，且未来 mlua 加入新库时不会默默放行。
        //
        // 这里刻意没有 coroutine：Lua 的 debug hook 是 per-thread 的，
        // `set_hook` 只作用在主状态上，`coroutine.create` 出的线程不继承。
        // 于是 `coroutine.resume(coroutine.create(function() while true do end end))`
        // 能完整绕开指令数上限，而内存配额也拦不住不分配内存的死循环 ——
        // 所有 Lua 都跑在 Tauri 主线程上，这等价于永久冻死整个界面。
        // 与其去给新线程补 hook，不如关掉这个唯一的搬移入口（随附插件无一使用）。
        let libs = StdLib::TABLE
            | StdLib::STRING
            | StdLib::UTF8
            | StdLib::MATH
            | StdLib::PACKAGE
            | StdLib::OS;
        let lua = Lua::new_with(libs, LuaOptions::default())?;
        let globals = lua.globals();
        // base 库由 mlua 无条件打开（白名单里没有 BASE 也挡不住），所以按名字
        // 逐个摘掉能碰到磁盘的入口：随附插件没有一个用得到，留着只是多一条
        // "加载并执行任意路径下的 .lua" 的通道（跨插件代码混用、探测文件是否存在）。
        for name in ["dofile", "loadfile"] {
            globals.set(name, mlua::Value::Nil)?;
        }
        if let Ok(os) = globals.get::<mlua::Table>("os") {
            for name in [
                "execute",
                "exit",
                "getenv",
                "remove",
                "rename",
                "setlocale",
                "tmpname",
            ] {
                os.set(name, mlua::Value::Nil)?;
            }
        }
        if let Ok(pkg) = globals.get::<mlua::Table>("package") {
            pkg.set("loadlib", mlua::Value::Nil)?;
            pkg.set("cpath", "")?;
        }
        Ok(lua)
    }

    /// 为 Lua 状态施加资源限制（防 `while true do end` 冻结主线程）：
    /// - 内存上限：超限触发 `Error::MemoryError`；
    /// - 指令数 hook：每 N 条指令检查一次，超限直接中断执行。
    ///
    /// 配置项：config.ini [plugins] memory_limit_mb（默认 16）/ instruction_limit（默认 1000 万）。
    /// Lua 状态创建后调用一次即可覆盖该状态后续所有执行路径。
    fn apply_lua_limits(&self, lua: &Lua) {
        let mem_mb = memory_limit_mb(self.config);
        if let Err(e) = lua.set_memory_limit(mem_mb * 1024 * 1024) {
            tracing::warn!("Lua 内存限制设置失败: {e}");
        }
        Self::arm_instruction_budget(lua, self.config);
    }

    /// 装（或**重新**装）一次指令钩子：把这一轮的预算重置为 N 条指令。
    ///
    /// 为什么要"每次进入插件代码之前重装"：Lua 的 count 计数器挂在 `lua_State` 上，
    /// 只有 `lua_sethook` 会把它复位，而回调是**无条件 Err** —— 所以"创建状态时装一次"
    /// 的真实语义是「这个状态**累计**跑满 N 条指令，就把当时那次调用掐死」，
    /// 而不是注释所写的「每次调用有 N 条预算」。
    ///
    /// 后果是可复现的：插件详情页每 2 秒渲染一次 `get_view()`，插件页挂着不动，
    /// 攒到 N 的那一刻一个完全正常的插件会被 `mark_plugin_error` 判成
    /// "疑似死循环"而停用 —— 界面停在旧数据、插件页一行红字，不重启（或停用再启用）
    /// 不自愈。本仓有用例把这条钉住：
    /// `crates/core/tests/plugin_lifecycle_test.rs::instruction_budget_is_per_call_not_per_state`
    /// （默认 1000 万条 + 记账那种规模的 get_view，几个小时到几天内必中一次）。
    ///
    /// 重装之后死循环仍然在**同一个调用**里被掐断（预算没变、只是不再跨调用累计）。
    /// `instruction_limit = 0`（有人当"不限制"写）以前被 `.clamp(1)` 变成 1 →
    /// 每条指令都超限、所有插件在第一条就中断，这里一并退回默认值。
    fn arm_instruction_budget(lua: &Lua, cfg: &FocusFlowConfig) {
        let instr = instruction_budget_from(cfg.get_int(
            "plugins",
            "instruction_limit",
            DEFAULT_INSTRUCTION_LIMIT,
        ));
        lua.set_hook(
            HookTriggers::new().every_nth_instruction(instr),
            |_lua, _dbg| -> mlua::Result<VmState> {
                Err(mlua::Error::RuntimeError(
                    "插件执行超出指令数上限（疑似死循环），已中断".into(),
                ))
            },
        );
    }

    /// 将插件标记为错误并释放其 Lua 环境（保留条目供插件列表展示错误信息）。
    /// 超限后 Lua 状态不可信，跳过 cleanup 直接丢弃。
    fn mark_plugin_error(&mut self, name: &str, msg: String) {
        // owner = 插件文件名（见 `host::claim_scheduler`），要在置空 lua 之前取出来。
        // 必须在这里归还调度线程：Lua 侧唯一的归还入口是 `scheduler_shutdown()`，
        // 而它跟着 `info.lua = None` 一起消失了；`unload_plugin` 也救不回来
        //（它只在还有 lua 实例时才跑 cleanup）。不还不掉的后果不是"占点内存"：
        // 插件页显示这个面板已停用，`Scheduler` 线程却为整个进程续命，
        // 它登记的定时任务照旧到点 `CreateProcess` 启动程序 ——
        // `scheduler_plugin.lua` 的注释早就把这条语义写死了：
        // 「不叫这句，插件停用后定时任务仍会照常触发」。
        let owner = self
            .plugins
            .get(name)
            .map(|info| Self::stem_of(&info.file_path))
            .unwrap_or_default();
        // 停用那一刻的两件事都要留档：为什么（进错误历史）与第几次（计数），并当场落盘 ——
        // 用户重启之后还能在页面上查到"上次它是被什么打停的"。
        self.note_hook_error(name, &msg);
        if !owner.is_empty() {
            let board = plugin_health::board_for(&owner);
            board.lock().unwrap_or_else(|e| e.into_inner()).note_stop();
            if let Err(e) = plugin_health::flush(&owner) {
                tracing::warn!("插件 {owner} 停用时的台账落不下去: {e}");
            }
        }
        if let Some(info) = self.plugins.get_mut(name) {
            info.error = Some(msg.clone());
            info.lua = None;
            info.view = None;
            info.loaded = false;
        }
        if !owner.is_empty() {
            // 出错停用 = 这份实例已经不在了，它占着的每一份共享资源都按名册叫一遍归还
            // （方案 3.11）。从前这里是三行各叫各的（调度线程、番茄钟、定时器），
            // 而第四份资源加进来时早晚会有人只补两处。
            // 番茄钟那句要点名"进行中那一段已落库"：它按主人归还，不该把用户正在跑的
            // 番茄钟被别的插件超限打断这件事说成一条普通的回收。
            for label in resources::release_all(&owner) {
                let tail = if label == resources::POMODORO {
                    "（进行中那一段已落库）"
                } else {
                    ""
                };
                tracing::warn!("{label}随插件 {name} 一起回收{tail}");
            }
        }
        tracing::error!("插件已停用: {name}: {msg}");
    }

    /// `[plugins] legacy_layout`：回滚开关。`true` = 只扫旧的平铺目录 `app_dir/plugins/`，
    /// 忽略数据目录，行为退回目录分离之前那一版（runbook 12.4）。保留一个版本，之后连分支一起删。
    fn legacy_layout(&self) -> bool {
        self.config.get_bool("plugins", "legacy_layout", false)
    }

    /// 这一轮真正被扫的插件目录：内置在程序目录、用户在数据目录；回滚开关打开时只剩旧目录一处。
    ///
    /// 顺序就是身份的顺序：内置在前。同名的两条（用户把内置脚本复制了一份放进数据目录）
    /// 以程序目录那份为准 —— 内置模块随包发布、升级覆盖，这是它唯一的权威来源。
    ///
    /// 热重载线程与"文件还在不在"的检查都从这里取目录，所以它必须和 [`Self::discover`]
    /// 同一把尺子：两处各说一套，就会出现"清单在 A、监视在 B"的错位。
    fn plugin_dirs(&self) -> Vec<PathBuf> {
        paths::plugin_scan_dirs(self.legacy_layout())
    }

    /// 文件名（不含扩展名）：启用/停用配置的键。
    /// 用文件名而非 PLUGIN_NAME——后者插件作者可随时改，改了配置就失配。
    pub fn stem_of(path: &Path) -> String {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("plugin")
            .to_string()
    }

    /// 已停用的插件文件名列表（config.ini `[plugins] disabled`，逗号分隔）。
    fn disabled_list(&self) -> Vec<String> {
        self.config
            .get_or("plugins", "disabled", "")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// 该插件文件是否被停用。
    pub fn is_disabled(&self, stem: &str) -> bool {
        self.disabled_list().iter().any(|s| s.as_str() == stem)
    }

    /// 启用/停用插件（按文件名）。
    ///
    /// 停用会卸载已加载实例并调用其 cleanup；启用会立即加载。
    ///
    /// 返回值只表示**这次操作本身有没有做成**。`config.set` 只把改动排进 300ms 去抖的
    /// 持久化队列、永远返回 Ok，所以"写配置失败"那个分支是死代码 —— 它原先返回的是
    /// "操作后的启用状态"，于是**停用成功**时给出 false，调用方一句 `if !ok` 把成功
    /// 报成了「写入插件启用状态失败」（0a85386）。现在只有"启用但加载失败"才给 Err，
    /// 附带插件自己报的原因：那是真失败，而前端一直把 Ok 当成功弹「插件已启用」。
    pub fn set_enabled(&mut self, stem: &str, enabled: bool) -> Result<(), String> {
        // `stem` 会原样拼进 config.ini 的 `[plugins] disabled`（逗号分隔，写盘不做转义），
        // 而它是 IPC 传上来的字符串：名字里带一个 `,` 就能顺手停用别的插件，带换行的
        // 能往配置里注入任意 INI 行。只接受插件目录里真实存在的文件名。
        if stem.contains(',') || stem.contains('\n') || stem.contains('\r') {
            // 光"存在于磁盘"挡不住带逗号的文件名 —— 它自己就能把列表撑成两项，
            // 于是下一次启动会把别的插件一起"点亮"。这种文件直接不认。
            tracing::error!("插件名不能含逗号/换行（会破坏 [plugins] disabled 列表）: {stem}");
            return Err(format!("插件名不合法（含逗号或换行）: {stem}"));
        }
        // 扫一次，两次判断共用同一份清单：原先这里扫一遍、启用时再扫一遍，
        // 目录正在被整份替换时两半能各读到不同的东西（前一半说"在"、后一半找不到那个文件，
        // 于是配置写了、插件却没加载，页面还报成功）。
        // 拿的是带来源的那一份（`discover`）而不是只带路径的 `discover_paths`：
        // 「启用一个用户插件」必须按用户视图加载，把来源在这一站丢掉就等于从这条入口
        // 给回内置专属 API —— 而这条正是插件页上那颗「启用」。
        let files = self.discover()?;
        if !files.iter().any(|f| Self::stem_of(&f.path) == stem) {
            // 以前这里是 `return Ok(())`：命令层把 Ok 当成功，前端弹「插件已启用」，
            // 而配置一个字没改 —— 对着一个不存在的插件报成功。
            let reason = missing_plugin_reason(&self.plugin_dirs(), stem);
            tracing::warn!("启用插件被拒（{stem}）: {reason}");
            return Err(reason);
        }
        let mut list = self.disabled_list();
        if enabled {
            list.retain(|s| s.as_str() != stem);
        } else if !list.iter().any(|s| s.as_str() == stem) {
            list.push(stem.to_string());
        }
        // 这一笔要问得出结局：`config::set` 恒回 `Ok(())`（真写盘的是另一头的去抖线程），
        // 那个 `if let Err` 从来没有活过，而调用方（`desktop-native` 的 `toggle_plugin`）
        // 把 `Ok` 念成「插件 X 已启用/停用」—— 屏上说完，下一次启动全都回到文件里那一档。
        // 启用/停用是用户点一下的手势，当场落盘问一次才说得上"记住了"。
        if let Err(e) = self.config.set_now("plugins", "disabled", &list.join(",")) {
            let action = if enabled { "启用" } else { "停用" };
            tracing::error!("写入插件{action}状态失败 ({stem}): {e}");
            return Err(format!(
                "插件「{stem}」的{action}状态没写进 config.ini（{e}）：\
                 这一次改动只到本次运行，重启会回到文件里那一档"
            ));
        }
        if enabled {
            // 只有当前未加载时才加载，避免重复初始化
            let already = self
                .plugins
                .values()
                .any(|p| Self::stem_of(&p.file_path) == stem);
            if !already {
                if let Some(found) = files.iter().find(|f| Self::stem_of(&f.path) == stem) {
                    if let Err(e) = self.try_load(&found.path, found.source) {
                        tracing::warn!("启用插件失败 ({stem}): {e}");
                        return Err(format!("插件已记录为启用，但加载失败：{e}"));
                    }
                }
            }
        } else {
            // 先取出名字再卸载：避免在 if-let 条件里持有不可变借用、体内又要 &mut self
            let name = self
                .plugins
                .values()
                .find(|p| Self::stem_of(&p.file_path) == stem)
                .map(|p| p.manifest.name.clone());
            if let Some(n) = name {
                self.unload_plugin(&n);
            }
        }
        Ok(())
    }

    /// 列出目录中所有插件（含已停用的），停用的不执行其代码。
    ///
    /// 已加载的插件直接取内存中的信息，避免重复执行其顶层代码；
    /// 未加载的（停用或加载失败）才读取元数据。
    ///
    /// 目录读不出来时回 `Err(原因)`：那一页的空列表原本既可能是"真没插件"也可能是
    /// "整个目录读不动"，而前端只有一种画法（「暂无插件」）。现在错误由命令层交给页面，
    /// 页面上两种结局长得不一样。
    pub fn list_discovered(&self) -> Result<Vec<DiscoveredPlugin>, String> {
        let disabled = self.disabled_list();
        Ok(self
            .discover()?
            .into_iter()
            .map(|found| {
                let path = found.path;
                let source = found.source;
                let stem = Self::stem_of(&path);
                let enabled = !disabled.iter().any(|s| s.as_str() == stem);
                if let Some(info) = self
                    .plugins
                    .values()
                    .find(|p| Self::stem_of(&p.file_path) == stem)
                {
                    // 两条失败要分开说，而且**都得说**：
                    //   · `info.error` —— 这个实例上一次跑坏了（视图/初始化炸了）；
                    //   · `load_errors[stem]` —— 最近一次（热）重载失败，旧那份还在跑。
                    // 过去这里只读前者，于是"存进一个语法错的版本"这条最常见的手势在
                    // 插件页上是一片安静：热重载的返回值只进日志、`plugins-reloaded`
                    // 照发、`try_load` 的文档（见它自己那段注释）承诺的"插件页多一条
                    // 加载失败原因"根本兑不了现 —— 而用户看到的现象是"我改了文件没生效"。
                    let mut reasons: Vec<String> = Vec::new();
                    if let Some(e) = &info.error {
                        reasons.push(format!("上次运行出错：{e}"));
                    }
                    if let Some(e) = self.load_errors.get(&stem) {
                        reasons.push(format!("最近一次重载失败：{e}（界面跑的还是改之前那份）"));
                    }
                    return DiscoveredPlugin {
                        manifest: info.manifest.clone(),
                        file: stem,
                        enabled,
                        loaded: info.loaded,
                        source,
                        authorization: info.authorization.clone(),
                        slow: self.view_is_slow(&info.manifest.name),
                        note: None,
                        error: if reasons.is_empty() {
                            None
                        } else {
                            Some(reasons.join("；"))
                        },
                    };
                }
                match Self::scan_meta(&path) {
                    Ok(manifest) => {
                        // 未加载的那一条也要念得出这一句，且与加载那一条同一个规则：
                        // 插件页上「需要授权」与「已授权」的区别就是用户点不点那一颗按钮，
                        // 两处算得不一样会先看到一句假话、再看到一次被拒。
                        let authorization = Self::authorization_at(source, &manifest, &path);
                        let note = Self::load_note(
                            &self.pending,
                            &stem,
                            source,
                            &manifest,
                            enabled,
                            false,
                        );
                        DiscoveredPlugin {
                            manifest,
                            file: stem.clone(),
                            enabled,
                            loaded: false,
                            source,
                            authorization,
                            slow: false,
                            note,
                            // 没加载、但加载失败过：原因得显示出来，不能只剩"未加载"
                            error: self.load_errors.get(&stem).cloned(),
                        }
                    }
                    Err(e) => DiscoveredPlugin {
                        // 读不出来时名字仍回退成文件名，但那几个字段一律留空、不当"1.0"报出去：
                        // 插件页这一行说的是"这份文件我没能读全"，不是"它自述版本是 1.0"。
                        manifest: PluginManifest {
                            name: stem.clone(),
                            desc: String::new(),
                            version: String::new(),
                            author: String::new(),
                            ..PluginManifest::default()
                        },
                        file: stem,
                        enabled,
                        loaded: false,
                        source,
                        // 清单问不出，授权就也问不出：这一格不许折成"没授权过"，
                        // 否则用户会去点一颗本来就不该出现的「授权」按钮。
                        authorization: if source == PluginSource::User {
                            permissions::Authorization::Undetermined(format!("清单读不出来（{e}）"))
                        } else {
                            permissions::Authorization::Exempt
                        },
                        slow: false,
                        note: None,
                        error: Some(e),
                    },
                }
            })
            .collect())
    }

    /// 扫两处插件目录并**带上来源**：内置在 `app_dir/plugins/builtin/`、用户在 `data_dir/plugins/`。
    ///
    /// 为什么要分两处（3.1）：内置模块是程序的一部分（重装即恢复、不进备份、也不该由用户改），
    /// 用户自己装的插件是用户数据（要能写、要跟着"更改数据文件夹"一起搬、要进备份）。
    /// 混在程序目录里则三条都反着来：安装版写 Program Files 要管理员权限、升级会清掉用户插件、
    /// 备份扫程序目录。
    ///
    /// 身份**由位置决定**，不看脚本里写了什么常量 —— 否则一个脚本自述 `PLUGIN_KIND = "builtin"`
    /// 就能绕过授权。这里交出的 `source` 就是那份身份，插件页两栏分组（P0 第 8 步）用它。
    ///
    /// 旧目录 `app_dir/plugins/*.lua` 在这条路上不是清单来源，只是**迁移的源**：
    /// 先按 12.2 把用户插件复制进数据目录（只复制、同名不覆盖、失败只记 error），
    /// 再由数据目录进清单。`[plugins] legacy_layout = true` 是唯一读旧目录清单的分支。
    ///
    /// **目录读不出来时交回原因，不交回空清单**：空清单从此只有一种含义 —— 这些目录读全了、
    /// 里面确实没有 .lua。以前是 `.unwrap_or_default()`：`plugins` 被建成同名文件、整份放在
    /// 断线的网盘上、或替换目录的那一刻，插件页显示「暂无插件」而 5 个插件都还在盘上，
    /// 日志里一个字都没有；点启用还会得到一句假原因「插件文件不存在」。
    /// 半份清单比没清单更难解释，所以任一目录读不出来就整体交 Err。
    pub fn discover(&self) -> Result<Vec<Discovered>, String> {
        // 扫哪几处只在 `paths::plugin_scan_dirs` 说一次：插件页那一行「目录」读的是同一份。
        // 这里再列一遍目录的话，两处会各说一套 —— 变异探针把内置目录从那把尺子里摘掉，
        // 行为用例照样绿，而屏上告诉用户的目录已经和清单来的不是同一处（那就是"照屏上放的
        // 地方放下去不加载"）。身份按位置算，见 [`Self::source_at`]。
        let legacy = self.legacy_layout();
        let dirs = paths::plugin_scan_dirs(legacy);
        if legacy {
            // 只有真要用旧目录的人才建它（新布局不建程序目录那一格：安装版写不进去，
            // 而这些函数会被每秒的清单扫描调到，建不出的 warn 会变成日志噪声）。
            if let Some(dir) = dirs.first() {
                std::fs::create_dir_all(dir).ok();
            }
        } else {
            run_legacy_migration(&paths::plugins_dir(), &paths::plugins_user_dir());
        }
        let mut errors: Vec<String> = Vec::new();
        let mut out: Vec<Discovered> = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        for (index, dir) in dirs.iter().enumerate() {
            let source = Self::source_at(index, legacy);
            let files = match list_plugin_files(dir) {
                Ok(files) => files,
                Err(e) => {
                    errors.push(e);
                    Vec::new()
                }
            };
            for path in files {
                let stem = Self::stem_of(&path);
                if seen.contains(&stem) {
                    // 同名只认先那一支（内置在前）。不报出来的话，用户看到的是"我放了一份但
                    // 插件页没多一条"，而这一句在日志里本来该有原因。
                    if notice_once(&format!("shadowed:{}:{}", source.label(), path.display())) {
                        tracing::warn!(
                            "{stem} 在同名的前一处目录里已经有了，这一份（{}，{}插件）不加载：\
                             内置模块以随包那一份为准，想跑自己改的那份请换个文件名",
                            path.display(),
                            source.label()
                        );
                    }
                    continue;
                }
                seen.push(stem);
                out.push(Discovered { path, source });
            }
        }
        if !errors.is_empty() {
            return Err(errors.join("；"));
        }
        Ok(out)
    }

    /// 位置决定身份（3.1）：回滚分支里只有一处旧目录；新布局是那两处的顺序就是身份的顺序，
    /// 那份顺序写在 `paths::plugin_scan_dirs` 里，这里只按位次翻译。
    fn source_at(index: usize, legacy_layout: bool) -> PluginSource {
        if legacy_layout {
            return PluginSource::Legacy;
        }
        if index == 0 {
            PluginSource::Builtin
        } else {
            PluginSource::User
        }
    }

    /// [`Self::discover`] 的"只要路径"投影 —— 不需要身份的调用方用这一条。
    ///
    /// 刻意不另扫一遍目录：两处各扫会出现"清单与来源对不上"，而这份清单的每一个消费者
    /// 要么只要路径、要么两者都要。
    pub fn discover_paths(&self) -> Result<Vec<PathBuf>, String> {
        Ok(self.discover()?.into_iter().map(|d| d.path).collect())
    }

    /// 只做文本扫描读元数据，**不执行插件代码**：交出完整清单 [`PluginManifest`]。
    ///
    /// `list_discovered` 原先回退到 `read_meta`，而 `read_meta` 是 `exec()` 整段脚本再取
    /// 全局变量 —— 于是「已停用」的插件每开一次插件管理页、每来一次 `plugins-reloaded`
    /// 都要在主线程上把顶层代码跑一遍（只有指令数上限兜着），与 `mod.rs` 里
    /// 「停用的不执行其代码」这句承诺正好相反，也白跑了加载失败那批的顶层代码。
    /// 这 9 个字段本来就是给人看的字面量（字符串 / 整数 / 字符串数组），扫一行足够；
    /// 真正要执行顶层代码的加载路径照旧用 `read_meta`。**新增声明字段不许在这条路上开口子
    /// 去 exec**：方案 3.3 要的正是「已停用的插件也不执行脚本代码」。
    ///
    /// **判据必须与 `read_meta` 的实际口径一致**（扫出来的名字 = 加载后的名字），
    /// 否则插件管理页会拿一个假名字当键用：那页的「打开」按钮传的就是列表里的
    /// `p.manifest.name`（原生端同一条路：`desktop-native/src/main.rs` 拿这一行的名字点
    /// `open_plugin_view`），名字对不上
    /// 就是点一次报一次「插件未提供视图」。所以要挡掉两类假命中：
    /// `--[[ ]]` 块注释里被注释掉的旧值、函数体里缩进的同名局部赋值。
    /// 规则写在 [`top_level_string_assign`] 与 [`strip_lua_non_code`] 里：
    /// 先把注释和长字符串整段抹成空格，再只认**从第 0 列起**正好是
    /// `KEY = "字面量"` 的行（`local KEY = …` 也不算 —— 那根本不会成为全局变量）。
    fn scan_meta(path: &Path) -> Result<PluginManifest, String> {
        // 单文件与包走同一条读手：页面上读到的名字必须与真正加载的那一份是同一个人写的。
        let (text, _) = package::read_script(path)?;
        Ok(Self::scan_meta_source(&text, path))
    }

    /// `scan_meta` 的全部判定（读文件之外的那部分，单独拿出来才测得动）。
    fn scan_meta_source(text: &str, path: &Path) -> PluginManifest {
        let stem = Self::stem_of(path);
        build_manifest(declared_fields(text, &stem, &format!("扫描 {stem}")))
    }

    /// 从 Lua 脚本读取元数据（不执行 init）。
    fn read_meta(&self, path: &Path) -> Result<PluginMeta, String> {
        let lua = Self::create_sandboxed_lua().map_err(|e| e.to_string())?;
        self.apply_lua_limits(&lua);
        // 只注册空的 focusflow 占位表，避免扫描阶段触发宿主单例副作用；
        // 真正的宿主 API 在 load_plugin 时才注册。
        {
            let placeholder = lua.create_table().map_err(|e| e.to_string())?;
            lua.globals()
                .set("focusflow", placeholder)
                .map_err(|e| e.to_string())?;
        }
        let (script, _) = package::read_script(path)?;
        lua.load(&script)
            .set_name(
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("plugin"),
            )
            .exec()
            .map_err(|e| format!("Lua 执行失败: {e}"))?;

        let globals = lua.globals();
        // 只认 Lua 字符串：`PLUGIN_MIN_APP = 123` 在加载侧会被 `lua_tostring` 折成 "123"，
        // 而扫描侧根本读不出它（值不是引号串）—— 两侧就会交出两份清单（第一条用例就是这么红的）。
        // 折成"没声明"才是两边的共同答案。
        let gstr = |key: &str| -> Option<String> {
            match globals.get::<mlua::Value>(key).ok()? {
                mlua::Value::String(s) => Some(s.to_string_lossy()),
                _ => None,
            }
        };
        // 整数只认 Lua 的整数（`PLUGIN_API = 2`）：写成 `2.0` 或 `"2"` 都不算声明。
        // 这不是苛刻，是为了和扫描侧的 `top_level_int_assign`（只认裸十进制）折回同一个缺省值
        // —— 一侧认、另一侧不认，插件页就会显示一个值而跑起来是另一个值。
        let gint = |key: &str| -> Option<i64> { globals.get::<i64>(key).ok() };
        let gperms = || -> Option<Vec<String>> {
            let t = globals.get::<mlua::Table>("PLUGIN_PERMISSIONS").ok()?;
            // 只取数组部分的字符串项：`{ "a", b }` 里的 `b` 是个变量，扫描侧同样取不到它。
            let mut out = Vec::new();
            for item in t.sequence_values::<mlua::String>().flatten() {
                out.push(item.to_string_lossy());
            }
            Some(out)
        };
        let name = gstr("PLUGIN_NAME").unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("plugin")
                .to_string()
        });
        let has_fn = |key: &str| -> bool { globals.get::<mlua::Function>(key).is_ok() };

        Ok(PluginMeta {
            manifest: build_manifest(DeclaredFields {
                name,
                desc: gstr("PLUGIN_DESC"),
                version: gstr("PLUGIN_VERSION"),
                author: gstr("PLUGIN_AUTHOR"),
                api: gint("PLUGIN_API"),
                permissions: gperms(),
                refresh_ms: gint("PLUGIN_REFRESH_MS"),
                lifecycle: gstr("PLUGIN_LIFECYCLE"),
                min_app: gstr("PLUGIN_MIN_APP"),
                where_: format!(
                    "加载 {}",
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("plugin")
                ),
            }),
            has_init: has_fn("init"),
            has_view: has_fn("get_view"),
        })
    }

    /// 这一次加载在授权这一头的结论，以及要不要走那道闸。
    ///
    /// 三档免拦，其余一律拦：
    /// · 内置与 `[plugins] legacy_layout` 那一档 —— 位置就是身份，内置模块免授权（3.7）；
    /// · `PLUGIN_API <= 1` 的老脚本 —— 方案 5 那张迁移表定的是"行为不变，不弹授权"，
    ///   而今天满盘的脚本都没写这一项，一上线就拦等于把所有存量插件一次停掉；
    /// · 声明为空也不是"放行"：它的意思是"这份插件不该用任何要权限的能力"，
    ///   所以照样拦，只是页面上没有东西可授（[`permissions::Authorization::NothingToGrant`]）。
    ///
    /// `granted` 只从"哈希对得上"那两档来（`Granted` 与 `ContentChanged`）：
    /// 权限集合没变、只改过代码 ⇒ 授权保留（3.3 那条取舍），而这一份**不要求重新加载**。
    fn authorization_for(
        source: PluginSource,
        api: u32,
        declared: &[String],
        stem: &str,
        bytes: &[u8],
    ) -> (permissions::Authorization, permissions::GateMode) {
        use permissions::Authorization as A;
        if source != PluginSource::User {
            return (A::Exempt, permissions::GateMode::Free);
        }
        if api <= 1 {
            return (A::LegacyApi, permissions::GateMode::Free);
        }
        let ph = permissions::permission_hash(declared);
        let ch = permissions::content_hash(bytes);
        let read = permissions::read_grants(stem);
        let authorization = permissions::authorize(None, declared, &ph, &ch, &read);
        let granted = matches!(authorization, A::Granted | A::ContentChanged);
        (authorization, permissions::GateMode::Enforce { granted })
    }

    /// 用户当场点「同意」：落一份 `grants.json`，并把这一份插件的闸翻开。
    ///
    /// **没加载的插件也能授权**（2026-11-04 的 GUI 屏证逮到的死锁）：API 2 的插件若在
    /// `init()` 里就调权限 API（脚手架正是这样），未授权会让 init 抛 `DENIED`、加载失败；
    /// 而「授权」按钮原先要求"已加载" —— 于是"要先授权才能加载、要先加载才能授权"。
    /// 未加载时按清单找到这一份文件，用**纯文本扫描**（不执行脚本）取声明：授权绑的是
    /// 权限集合与文件字节，这两样扫描都拿得到，而扫描不会因为没授权而失败。
    ///
    /// 已加载时仍用实例里那一份清单：那是这一份 VM 真正在读的东西。
    ///
    /// 交回的是页面上那一句话（授权之后插件页要当场改口，而不是等下一次重扫）。
    pub fn grant_plugin(&mut self, stem: &str) -> Result<String, String> {
        let loaded = self
            .plugins
            .values()
            .find(|p| Self::stem_of(&p.file_path) == stem)
            .map(|p| {
                (
                    p.manifest.permissions.clone(),
                    p.file_path.clone(),
                    Some(Arc::clone(&p.gate)),
                )
            });
        let (declared, path, gate) = match loaded {
            Some(found) => found,
            None => {
                let path = self
                    .discover()?
                    .into_iter()
                    .find(|d| Self::stem_of(&d.path) == stem)
                    .map(|d| d.path)
                    .ok_or_else(|| {
                        format!("清单里没有「{stem}」：插件目录这一轮没扫到它，授权无处可绑")
                    })?;
                let manifest = Self::scan_meta(&path)?;
                (manifest.permissions, path, None)
            }
        };
        // 与加载时同一把读手：包算包字节、单文件算文件字节 —— 两处各读一套的话，
        // 授权落下去的那份哈希与插件实际跑的那一份不是同一个对象。
        let (_, bytes) = package::read_script(&path).map_err(|e| {
            format!("读不出 {stem} 这一份文件（{e}）：授权绑的是它的字节，读不出就不落盘")
        })?;
        if declared.is_empty() {
            return Err(format!(
                "「{stem}」没在 PLUGIN_PERMISSIONS 里声明任何权限：没有要授权的东西"
            ));
        }
        let record = permissions::Grants {
            permission_hash: permissions::permission_hash(&declared),
            content_hash: permissions::content_hash(&bytes),
            granted: declared.clone(),
        };
        permissions::write_grants(stem, &record)
            .map_err(|e| format!("「{stem}」的授权没落盘（{e}）：屏上不说已授权"))?;
        // 没加载的那一档没有闸可翻：下一次加载会按刚落的 grants.json 重建它。
        if let Some(gate) = gate {
            gate.set_granted(true);
        }
        Ok("已授权".to_string())
    }

    /// 撤销授权：删记录 + 把闸合上。与「停用插件」是两件事（停用不碰授权与数据）。
    pub fn revoke_plugin(&mut self, stem: &str) -> Result<String, String> {
        permissions::revoke_grants(stem)
            .map_err(|e| format!("「{stem}」的授权没撤销（{e}）：屏上不说已撤销"))?;
        if let Some(p) = self
            .plugins
            .values()
            .find(|p| Self::stem_of(&p.file_path) == stem)
        {
            p.gate.set_granted(false);
        }
        Ok(permissions::Authorization::NotGranted.note())
    }

    /// 没加载的那一条也念同一句话：规则共用 [`Self::authorization_for`]，差别只是这里现读一次文件。
    ///
    /// 为什么值得为未加载的那条也算：停用的插件页面上要写清它要哪些权限、授没授过，
    /// 用户重新启用时才知道自己点的是什么；两处各算一遍就会分家（本仓为 `PLUGIN_NAME`
    /// 那件事钉过同一条判据）。
    fn authorization_at(
        source: PluginSource,
        manifest: &PluginManifest,
        path: &Path,
    ) -> permissions::Authorization {
        use permissions::Authorization as A;
        if source != PluginSource::User {
            return A::Exempt;
        }
        if manifest.api <= 1 {
            return A::LegacyApi;
        }
        match std::fs::read(path) {
            Ok(bytes) => {
                Self::authorization_for(
                    source,
                    manifest.api,
                    &manifest.permissions,
                    &Self::stem_of(path),
                    &bytes,
                )
                .0
            }
            Err(e) => A::Undetermined(format!("这一份文件读不出来（{e}）")),
        }
    }

    /// 加载单个插件文件。
    ///
    /// `source` 是这一条**出自哪一处目录**，由 [`Self::discover`] 给出、由 [`Self::load_all`]
    /// 一路带到这里：它决定插件看见哪一组宿主 API（3.4）。身份不读脚本自己写的常量，
    /// 所以这里必须由调用方说，不能在函数里猜。
    pub fn load_plugin(&mut self, path: &Path, source: PluginSource) -> Result<String, String> {
        // 方案 3.10 那一档：能力版本比宿主还高的这一份，在**执行脚本之前**就拒绝。
        // 取这一眼靠的是纯文本扫描（`scan_meta`，与插件页那一栏同一把尺子），不碰 Lua ——
        // 症状是作者写 `PLUGIN_API = 3` 指望那些新格子，而今天每一格拿到的都是 nil；
        // 与其让它在运行时零散失败，不如在加载这一步就说清"这一份要配更高版本的程序"。
        let scanned = Self::scan_meta(path)?;
        if scanned.api > MAX_SUPPORTED_PLUGIN_API {
            return Err(format!(
                "{} 需要更新版本的 FocusFlow：它声明要 API {}，而这一版只到 API {}",
                scanned.name, scanned.api, MAX_SUPPORTED_PLUGIN_API
            ));
        }
        let meta = self.read_meta(path)?;
        // 展示名（PLUGIN_NAME）是插件表的键。两个文件声明同一个名字时，原先是
        // 后来者静默覆盖前者：被顶掉的那个既不再接收键事件，也再没有 unload
        // 入口（cleanup 永不执行），而它的 .lua 还在盘上。拿复制模板改插件的人
        // 十有八九是忘了改 PLUGIN_NAME，所以这里明确报错、保留已加载的那个。
        //
        // **判据是源文件，不是展示名**：热重载现在"先加载新版本、成功才退休旧条目"
        // （见 `reload_plugin`），而加载那一下旧条目还在表里挂着 —— 按展示名判重的话，
        // 每个插件的自我重载都会被自己判成撞名，于是"坏不了的插件反而永远重载不动"
        // （上一场就是卡在这一步停手的）。同一个源文件的重复加载不算撞名：
        // 下面 insert 会顶掉它自己那一条，改了名的旧名字另外摘掉。
        let stem = Self::stem_of(path);
        let manifest = &meta.manifest;
        if let Some(clash) = self
            .plugins
            .values()
            .find(|p| p.manifest.name == manifest.name && !is_same_source(&p.file_path, path))
        {
            return Err(format!(
                "PLUGIN_NAME「{}」与已加载插件（文件 {}）重名，请改一个再启用",
                manifest.name,
                Self::stem_of(&clash.file_path)
            ));
        }
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        // 先读文件字节：`content_hash` 吃的就是这一份（方案 3.3 那两个哈希的范围写死了
        // "文件原始字节"，不做换行归一），而下面执行 Lua 用的也是它。
        // 单文件与包走同一条读手：正文用来执行，`hash_bytes` 交给授权那一头算内容哈希
        // （包算的是包字节，单文件算的是文件字节）。
        let (script, hash_bytes) = package::read_script(path)?;

        let lua = Self::create_sandboxed_lua().map_err(|e| e.to_string())?;
        self.apply_lua_limits(&lua);
        // 宿主按文件名记账调度线程的使用者，所以这里传 stem 而不是展示名：
        // 两个插件的 PLUGIN_NAME 相同是常见的手误，展示名当键会串味。
        let api = host::register_host_api(&lua, self.config, Arc::clone(&self.db), &stem)
            .map_err(|e| format!("宿主 API 注册失败: {e}"))?;
        // 用户装的脚本只拿通用能力：内置专属那一组（`pomodoro_*` / `accounting_*` /
        // `scheduler_*` / `edge_*`）从这张表里摘掉，插件取到的是 `nil`（3.4、用例 14）。
        // 摘不摘由**位置**决定，脚本写什么 `PLUGIN_KIND` 都不算 —— 允许自述就等于
        // 谁都能把自己报成内置，权限模型第一天失效（3.1）。
        if source == PluginSource::User {
            let hidden =
                host::hide_builtin_only_api(&api).map_err(|e| format!("宿主 API 收口失败: {e}"))?;
            tracing::debug!(
                "插件 {stem} 按用户插件加载：{} 组内置专属 API 对它不可见",
                hidden.len()
            );
        }
        // 通用那一组再叠一道权限闸（方案 3.7）：未声明/未授权的调用抛 `DENIED:` 那句话，
        // 而**照常加载** —— 拒绝加载的结局是插件页只剩一行错误，用户不知道它要什么权限。
        // 定时与事件先装（`every` / `on` 那几个名字要在收口之后、权限闸之前进表，
        // 闸才会把它们一起包进去）。
        // 每次（重）加载都从**空表**开始：回调函数住在上一份 Lua 里，那份已经跟着旧条目没了，
        // 留着旧句柄就是让排程去查一个再也不存在的回调。
        // 与调度线程那一份 owner 记账不同 —— 那条刻意不在重载时销（见 `reload_plugin`），
        // 因为线程还在为别的插件跑；这里销的是"这个实例自己的待办清单"，重载语义就是换一份。
        // 顺序不能反：先销旧格、再装入口，装的那一步取到的是这一份新表。
        plugin_timers::forget(&stem);
        // 每一次（重）加载都从空表开始，台账同理：**先把上一份账落盘，再摘掉这一格**
        // （`plugin_health::forget` 里就是这一序）。落盘的必要性在这条路上最直白 ——
        // 热重载换的是字节，不该顺手把"这一格慢过几次、上次为什么停用"清零。
        // 摘掉之后下一次 `board_for` 从盘上重开一格，读回来的正是刚写下去的那一份。
        plugin_health::forget(&stem);
        host::install_timer_api(&lua, &api, &stem).map_err(|e| format!("定时器入口装不上: {e}"))?;
        let declared = meta.manifest.permissions.clone();
        let unknown = permissions::unknown_permissions(&declared);
        if !unknown.is_empty() {
            // 认不得的声明不许"当成没声明"就过去：作者写了 `PLUGIN_PERMISSIONS = {"stor.write"}`
            // 少一个 s，症状是每次调用都被拒而屏上只说"没声明"，看不出是拼错。
            tracing::warn!(
                "插件 {stem} 声明了认不得的权限（{}）：这些项按不生效处理，能用的权限只有 {}",
                unknown.join("、"),
                permissions::KNOWN_PERMISSIONS.join("、")
            );
        }
        let (authorization, mode) =
            Self::authorization_for(source, meta.manifest.api, &declared, &stem, &hash_bytes);
        let gate = Arc::new(permissions::Gate::new(mode, &declared));
        if let permissions::GateMode::Enforce { .. } = gate.mode() {
            let wrapped = host::install_permission_guard(
                &lua,
                &api,
                Arc::clone(&gate),
                &permissions::guarded_api_names(),
                &permissions::store_verb_names(),
            )
            .map_err(|e| format!("权限闸装不上: {e}"))?;
            if wrapped.is_empty() {
                // 一格都没装上 = 闸是空的，而页面上会写着"已授权"这种让人信的话
                tracing::error!(
                    "插件 {stem} 的权限闸没装上任何一格（声明 {declared:?}）：\
                     这一份插件本次按无闸运行，请报告"
                );
            }
        }
        lua.load(&script)
            .set_name(&meta.manifest.name)
            .exec()
            .map_err(|e| format!("Lua 执行失败: {e}"))?;

        if meta.has_init {
            let init: mlua::Function = lua.globals().get("init").map_err(|e| e.to_string())?;
            let _: () = init.call(()).map_err(|e| format!("init() 失败: {e}"))?;
        } else if let Err(e) = Self::take_hook(&lua, "init") {
            // has_init 来自 read_meta 里**另一个** Lua 实例，它把「没写」和「写了但不是
            // 函数」捏成同一个 false —— 于是 init = "setup" 的插件加载"成功"、初始化一次
            // 没跑、一个字都不留。前者是正常形态（Ok(None)），只有后者该出声。
            tracing::warn!("插件 {stem} 的 {e}：初始化没有执行");
        }

        // 3.14 那条警告：标了 `always` 却没有任何后台入口的用户插件，多半是抄模板抄来的 ——
        // 它会常驻着什么都不收，白占一份 VM 与一格常驻额度。
        // **只出声，不擅自改档位**：把它折成 `panel` 会让页面上的「常驻」标签与脚本里写的
        // 那两个字对不上，那是另一种"读不出来被念成没有"。后台入口有两种：`record_key`
        // 这个钩子，以及 `init()` 真排下去的定时器/事件（后者要跑完 init 才知道）。
        if source == PluginSource::User && meta.manifest.lifecycle == PluginLifecycle::Always {
            let hooked = lua
                .globals()
                .get::<mlua::Value>("record_key")
                .map(|v| matches!(v, mlua::Value::Function(_)))
                .unwrap_or(false)
                || !plugin_timers::board_for(&stem)
                    .lock()
                    .map(|b| b.is_empty())
                    .unwrap_or(true);
            if !hooked {
                tracing::warn!(
                    "插件 {stem} 声明 PLUGIN_LIFECYCLE = \"always\"，但它既没有 record_key \
                     也没排任何定时器：这一格会常驻着什么都不收（多半是抄模板抄来的）"
                );
            }
        }
        let view = if meta.has_view {
            match Self::read_view(&lua) {
                Ok(v) => Some(v),
                Err(e) => {
                    // 原来这里是 `.ok()`：源码里声明了 get_view、但首次调用就抛错的插件，
                    // 结局是 `loaded = true` + 没有视图 + 一个字都不留 —— 插件页看着健康，
                    // 卡片却永远不出现。同一个失败晚些时候在 `refresh_view` 会返回 `Err`
                    // 摊给用户（超限那一类还会 `mark_plugin_error`），两边不一致。
                    // 仍然只留日志、不改 `loaded`：init() 已经跑过，标成"未加载"会让
                    // 用户以为插件根本没起来（并可能去点"启用"再跑一遍 init）。
                    if is_limit_error(&e) {
                        tracing::error!(
                            "插件 {stem} 的 get_view() 超出指令预算（{e}）：本次取不到视图，\
                             界面上不会出卡片，界面刷新时会再报一次并停用该插件"
                        );
                    } else {
                        tracing::error!(
                            "插件 {stem} 声明了 get_view() 但首次取视图就失败（{e}）：\
                             本次没有视图，界面上不会出卡片"
                        );
                    }
                    None
                }
            }
        } else {
            if let Err(e) = Self::take_hook(&lua, "get_view") {
                tracing::error!("插件 {stem} 的 {e}：本次没有视图，界面上不会出卡片");
            }
            None
        };

        let info = PluginInfo {
            manifest: meta.manifest,
            file_path: path.to_path_buf(),
            file_mtime: mtime,
            loaded: true,
            error: None,
            source,
            authorization,
            gate,
            lua: Some(lua),
            view,
            view_route: RouteBreaker::default(),
            key_route: RouteBreaker::default(),
            event_route: RouteBreaker::default(),
        };

        // 新实例装好了，才让同源的旧版本退休。同名的那条由 insert 顶掉；
        // 但这一版把 PLUGIN_NAME 改了名的话顶不掉，会留下"同一个文件、两份实例"的
        // 僵尸 —— 旧那条既不再收键事件、又没有 unload 入口（cleanup 永不执行），
        // 记在 stem 上的调度线程使用者也就再没人回收。这里刻意**不**跑 cleanup、
        // 也不动 owner 记账：重载语义见 `reload_plugin`。
        let renamed = self
            .plugins
            .iter()
            .find(|(name, p)| {
                name.as_str() != info.manifest.name.as_str() && is_same_source(&p.file_path, path)
            })
            .map(|(name, _)| name.clone());
        if let Some(old) = renamed {
            tracing::info!("插件改名，旧条目退休: {old} -> {}", info.manifest.name);
            self.plugins.remove(&old);
        }

        self.plugins.insert(info.manifest.name.clone(), info);
        tracing::info!("插件加载成功: {}", self.plugins.len());
        // 返回刚加载的插件名
        let name = self
            .plugins
            .values()
            .find(|p| p.file_path == path)
            .map(|p| p.manifest.name.clone())
            .ok_or_else(|| "加载后未找到插件".to_string())?;
        Ok(name)
    }

    /// `load_plugin` 的记账版：成功就清掉这个文件之前的失败原因，失败就留档，
    /// 好让插件页显示"为什么没加载"而不是只有一行未加载。
    fn try_load(&mut self, path: &Path, source: PluginSource) -> Result<String, String> {
        let stem = Self::stem_of(path);
        match self.load_plugin(path, source) {
            Ok(name) => {
                self.load_errors.remove(&stem);
                Ok(name)
            }
            Err(e) => {
                // 失败的那次尝试可能已经在调度线程上把这个文件名登记成使用者了 ——
                // `init()` 跑到一半才报错最常见（`focusflow.scheduler_tasks()` 是第一个
                // 会 claim 的宿主 API，插件往往先列任务再在下一行炸）。表里已经没有这个
                // 源文件的条目时，那份登记就成了无主孤儿：唯一的释放入口是插件 Lua 里的
                // `scheduler_shutdown()`，而那个状态已经跟着失败的回滚一起没了 →
                // 本进程的调度线程再也回收不掉（见 `reload_plugin` 的说明）。
                // **有活着的旧实例时一律不销**：那份账属于旧实例，重载失败要"什么都没
                // 发生"，销它等于把定时任务的线程从还在用它的插件脚下拆走。
                if !self
                    .plugins
                    .values()
                    .any(|p| is_same_source(&p.file_path, path))
                {
                    resources::release_all(&stem);
                }
                self.load_errors.insert(stem, e.clone());
                Err(e)
            }
        }
    }

    /// 跑一遍到期的定时器与事件回调（GUI 那一拍调用，方案 3.6 的实现要点）。
    ///
    /// 与按键攒批同一个模型：排程表是进程级的（后台线程也能投请求），但进 Lua 的这一跳
    /// 必须留在调用它的那一根线程上 —— `Lua` 不是 `Send`。回调函数从插件自己那一格
    /// `_ff_timers[句柄]` 取，所以插件被卸载或被停用之后这里必然取不到东西（那张表跟着
    /// VM 一起没了），而排程那一格也在 `unload_plugin` / `mark_plugin_error` 里销掉了。
    ///
    /// 交回「插件名 → 这一拍跑了几格」是一份**可观察量**：只说"函数跑过了"盯不住
    /// 验收 4 那句「停用后回调次数不再增长」—— 那条要有数得出来才能比。
    pub fn pump_timers(&mut self) -> Vec<(String, usize)> {
        let cfg = self.config;
        let (minute_key, day_key) = plugin_timers::wall_buckets(&chrono::Local::now());
        let now = std::time::Instant::now();
        let mut ran: Vec<(String, usize)> = Vec::new();
        // 先把名字取出来：跑回调会要 `&mut self`（超限那条会停用插件）
        let loaded: Vec<(String, String)> = self
            .plugins
            .values()
            .filter(|p| p.lua.is_some())
            .map(|p| (p.manifest.name.clone(), Self::stem_of(&p.file_path)))
            .collect();
        // app_change 的载荷从件口取**一次**就够：它是"此刻前台是谁"这一个事实，
        // 每个插件的每一格订阅者拿同一份，而"这一格见过谁"记在各自的排程表上。
        let app = plugin_events::take_app_change();
        for (name, stem) in loaded {
            // 事件/定时这一路已经停过：不再投（其余两路照常）。
            if self
                .plugins
                .get(&name)
                .map(|p| p.event_route.stopped().is_some())
                .unwrap_or(false)
            {
                continue;
            }
            let board = plugin_timers::board_for(&stem);
            let due = {
                let mut guard = board.lock().unwrap_or_else(|e| e.into_inner());
                guard.collect_due(now, minute_key, day_key)
            };
            let event_due: Vec<u64> = match app.as_deref() {
                Some(a) => {
                    let mut guard = board.lock().unwrap_or_else(|e| e.into_inner());
                    guard.collect_app_change(a)
                }
                None => Vec::new(),
            };
            if due.is_empty() && event_due.is_empty() {
                continue;
            }
            // 墙钟那一轮先跑、事件那一轮后跑：顺序固定，插件挂多格时"这一拍先跑谁"可预期
            let mut queue: Vec<(u64, Option<String>, &'static str)> = Vec::new();
            queue.extend(due.into_iter().map(|h| (h, None, "定时器")));
            queue.extend(event_due.into_iter().map(|h| (h, app.clone(), "事件")));
            let mut fired = 0usize;
            for (handle, payload, which) in queue {
                let outcome = {
                    let Some(info) = self.plugins.get(&name) else {
                        break;
                    };
                    let Some(lua) = info.lua.as_ref() else {
                        break;
                    };
                    Self::fire_hook(lua, cfg, &stem, handle, payload.as_deref())
                };
                match outcome {
                    Ok(()) => {
                        fired += 1;
                        if let Some(info) = self.plugins.get_mut(&name) {
                            info.event_route.record(false);
                        }
                    }
                    Err(e) => {
                        let msg = format!("{}回调（句柄 {handle}）失败: {e}", which);
                        tracing::error!("插件 {name} {msg}");
                        if is_limit_error(&e) {
                            // 死循环的那一类交给既有的熔断：停用、销表、插件页留原因
                            self.mark_plugin_error(&name, msg);
                            break;
                        }
                        // 与视图那一路同一条：进错误历史，停了就不再记。
                        self.note_hook_error(&name, &msg);
                        let tripped = self
                            .plugins
                            .get_mut(&name)
                            .map(|info| info.event_route.record(true))
                            .unwrap_or(false);
                        if tripped {
                            let stop = format!(
                                "定时/事件回调已停：最近 {FAIL_WINDOW} 次里 {FAIL_TRIPS} 次失败（最后一次：{msg}）——修好后停用再启用"
                            );
                            if let Some(info) = self.plugins.get_mut(&name) {
                                info.event_route.stop_with(stop.clone());
                                if info.error.is_none() {
                                    info.error = Some(stop.clone());
                                }
                            }
                            tracing::warn!("插件 {name} {stop}");
                            // 这一拍剩下的句柄也不再跑：路已经停了，再跑只是重复同一句错。
                            break;
                        }
                    }
                }
            }
            if fired > 0 {
                ran.push((name, fired));
            }
        }
        ran
    }

    /// 从插件那一格 Lua 里取出回调跑一次。
    ///
    /// 载荷那一格只有事件那一路用得上（app_change 要把应用名字交回给插件），定时器传
    /// None = 一个参数都不带 —— 这两条不许折成一条：给定时器回调多塞一个参数，插件里
    /// 写 function() ... end 的那些人看不见任何异常，但 select 数参数那类写法会开始数出东西来。
    ///
    /// 指令预算在**每一次调用之前**重新上：它是"这一格回调最多烧多少"的闸，共用一次
    /// 上过的预算会让第二格跑完也无人拦。
    fn fire_hook(
        lua: &Lua,
        cfg: &'static FocusFlowConfig,
        stem: &str,
        handle: u64,
        payload: Option<&str>,
    ) -> Result<(), mlua::Error> {
        let cb: Option<mlua::Function> = lua
            .globals()
            .get::<mlua::Table>("_ff_timers")
            .ok()
            .and_then(|t| t.get(handle).ok());
        match cb {
            Some(f) => {
                Self::arm_instruction_budget(lua, cfg);
                plugin_timers::note_fire_thread(stem);
                let started = std::time::Instant::now();
                let outcome = match payload {
                    Some(name) => f.call::<()>(name.to_string()),
                    None => f.call::<()>(()),
                };
                // 排程表与事件回调走的是同一次投递（`pump_timers` 那一拍），所以这一档
                // 一起算进「事件回调」：预算表那一行说的是"回调"，不是某个 Lua 名字。
                let board = plugin_health::board_for(stem);
                board
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .record(plugin_health::HookKind::Event, started.elapsed());
                outcome
            }
            None => Err(mlua::Error::RuntimeError(format!(
                "句柄 {handle} 在 _ff_timers 里没有对应的函数"
            ))),
        }
    }

    /// 打开着的那一页现在该不该重算（方案 3.9 的节奏，判断全在 `plugin_refresh`）。
    ///
    /// 每一拍先取一次 `invalidate` 的件口：投递方是插件自己（宿主闭包），它碰不到账本。
    /// 声明变了（作者改了 `PLUGIN_REFRESH_MS` 又存盘）换一份新账本 —— 旧样本压着新声明，
    /// 症状是"改成了 0 还在每秒刷"。
    pub fn view_due(&mut self, name: &str, now: std::time::Instant) -> bool {
        for stem in plugin_refresh::take_dirty() {
            if let Some(info) = self
                .plugins
                .values()
                .find(|p| Self::stem_of(&p.file_path) == stem)
            {
                let display = info.manifest.name.clone();
                let declared = info.manifest.refresh_ms;
                self.plans
                    .entry(display)
                    .or_insert_with(|| plugin_refresh::Cadence::new(declared))
                    .invalidate();
            }
        }
        let declared = match self.plugins.get(name) {
            Some(info) => info.manifest.refresh_ms,
            None => return false,
        };
        let plan = self
            .plans
            .entry(name.to_string())
            .or_insert_with(|| plugin_refresh::Cadence::new(declared));
        if !plan.declared_is(declared) {
            *plan = plugin_refresh::Cadence::new(declared);
            plan.invalidate();
        }
        plan.due(now)
    }

    /// 真的重算完之后交回耗时：这一格是慢熔断的样本来源。
    ///
    /// 只有跑完才记，跑失败也要记（一次失败的 `get_view()` 通常不比成功快）；
    /// 时间戳也在这一推进，所以调用顺序必须是"问 `view_due` → 跑 → 记"，
    /// 反过来（先记再跑）就出现了"刷失败但下一拍不再试"那一族缺陷。
    ///
    /// 从"正常"翻进"慢"的那一次还要进台账：插件页与详情页念的「降频 N 次」只有
    /// `note_throttle` 一个写手，而它原先只在台账自己的用例里被叫过 —— 生产里那一格
    /// 永远是 0，页面上一句"该插件较慢"与"降频 0 次"同屏，读起来就是两条账。
    pub fn note_view_refreshed(
        &mut self,
        name: &str,
        now: std::time::Instant,
        elapsed: std::time::Duration,
    ) {
        let declared = self
            .plugins
            .get(name)
            .map(|p| p.manifest.refresh_ms)
            .unwrap_or(plugin_refresh::DEFAULT_REFRESH_MS);
        let plan = self
            .plans
            .entry(name.to_string())
            .or_insert_with(|| plugin_refresh::Cadence::new(declared));
        let was_slow = plan.is_slow();
        plan.record(now, elapsed);
        if was_slow || !plan.is_slow() {
            return;
        }
        let Some(stem) = self.plugins.get(name).map(|p| Self::stem_of(&p.file_path)) else {
            return;
        };
        plugin_health::board_for(&stem)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .note_throttle();
    }

    /// 这一页现在被降频了吗（插件页那格要说「该插件较慢」，方案 3.9 那一列）。
    pub fn view_is_slow(&self, name: &str) -> bool {
        self.plans.get(name).map(|p| p.is_slow()).unwrap_or(false)
    }

    /// 这一页实际按多少毫秒自刷（诊断与用例的观察量）。
    pub fn view_interval_ms(&self, name: &str) -> u64 {
        self.plans
            .get(name)
            .map(|p| p.interval_ms())
            .unwrap_or(plugin_refresh::DEFAULT_REFRESH_MS)
    }

    /// 把一次钩子的耗时记进台账（预算表那四类的落点都走这一处 —— 尺子只有一份）。
    ///
    /// 四类各有各的预算，比的是 `plugin_health` 那一侧按档取的数；这里只回答
    /// "哪一类、跑了多久"。超预算那句走 debug：按键那一路每批都可能超，warn 会把日志刷满。
    fn note_hook_time(
        &self,
        name: &str,
        kind: plugin_health::HookKind,
        elapsed: std::time::Duration,
    ) {
        let Some(stem) = self.plugins.get(name).map(|i| Self::stem_of(&i.file_path)) else {
            return;
        };
        let board = plugin_health::board_for(&stem);
        let over = {
            let mut guard = board.lock().unwrap_or_else(|e| e.into_inner());
            guard.record(kind, elapsed)
        };
        if over {
            tracing::debug!(
                "插件 {name} 的{}用了 {:.1} ms，超过本档预算",
                kind.label(),
                elapsed.as_micros() as f64 / 1000.0
            );
        }
    }

    /// 把一条失败原因记进错误历史。**墙钟时间戳由这一侧给** ——
    /// 台账那一侧不读钟，判据才能拿固定的时刻串测"重启后还在不在"。
    fn note_hook_error(&self, name: &str, msg: &str) {
        let Some(stem) = self.plugins.get(name).map(|i| Self::stem_of(&i.file_path)) else {
            return;
        };
        if stem.is_empty() {
            return;
        }
        let at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let board = plugin_health::board_for(&stem);
        board
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .note_error(&at, msg);
    }

    /// 从插件 Lua 环境读取 get_view() 返回值（声明式 UI 表）。
    fn read_view(lua: &Lua) -> mlua::Result<PluginView> {
        let get_view: mlua::Function = lua.globals().get("get_view")?;
        let view_val: mlua::Value = get_view.call(())?;
        let mut view = PluginView::default();
        if let mlua::Value::Table(t) = view_val {
            if let Ok(title) = t.get::<String>("title") {
                view.title = title;
            }
            if let Ok(widgets) = t.get::<mlua::Table>("widgets") {
                for (i, (_, w)) in widgets
                    .pairs::<mlua::Value, mlua::Table>()
                    .flatten()
                    .enumerate()
                {
                    match parse_widget(&w, 0, &format!("w{i}")) {
                        Ok(widget) => view.widgets.push(widget),
                        // 认不出来的控件以前是**静默丢掉**的：`type` 打错一个字母、
                        // headers 里混进一个数字，界面上就是少一块，而插件行不报红、
                        // 日志里一个字都没有 —— 这一类"看不见"正是上面那些契约问题
                        // 能藏很久的原因。仍然跳过（不能让一个坏控件毁掉整页），
                        // 但要留下为什么。
                        Err(e) => tracing::warn!("跳过无法解析的控件: {e}"),
                    }
                }
            }
        }
        Ok(view)
    }

    /// 卸载插件。
    pub fn unload_plugin(&mut self, name: &str) -> bool {
        let cfg = self.config;
        if let Some(mut info) = self.plugins.remove(name) {
            let stem = Self::stem_of(&info.file_path);
            // 卸载 = 这一页不存在了，账本一起丢：留着旧样本的话，重新启用后第一次刷新
            // 就会被上一次留下的"慢"记录降频，而那时插件是全新的实例。
            self.plans.remove(name);
            // 丢掉的是**降频那一份**（上面那行 `plans`），而诊断那一份要留下：耗时读数、
            // 超预算次数与错误历史是给人回看的东西，它与"下一拍该不该降频"不是一个用途。
            // 所以这里落盘后摘格子，下一次问账从文件里读回来。
            //
            // 文件已经没了（用户卸包装时选了「删除设置」、或自己删了插件文件）就**只摘不落**：
            // 那一笔落盘会把刚被清掉的 `plugins/state/<stem>/` 又建回来。
            if info.file_path.exists() {
                plugin_health::forget(&stem);
            } else {
                plugin_health::discard(&stem);
            }
            // 卸载 = 用户不要它了（停用/删除/重启前的一次回收），旧的加载失败原因
            // 不该继续挂在插件页上。
            self.load_errors.remove(&stem);
            if let Some(lua) = &mut info.lua {
                match Self::take_hook(lua, "cleanup") {
                    Ok(Some(cleanup)) => {
                        Self::arm_instruction_budget(lua, cfg);
                        let _: mlua::Result<()> = cleanup.call(());
                    }
                    // 这条卸载路径上归还调度线程的动作**只有** Lua 侧那句
                    // `scheduler_shutdown()`：Rust 侧两处 `release_scheduler` 分别在
                    // `mark_plugin_error` 与加载失败回滚里，都到不了这里。这一句没跑
                    // 就是线程不还、定时任务照旧触发，所以"写了名字但不是函数"必须出声。
                    Err(e) => tracing::warn!("插件 {name} 的 {e}：卸载时没跑 cleanup"),
                    Ok(None) => {}
                }
            }
            info.loaded = false;
            info.lua = None;
            // 兜底归还：这条路径上原本**只有** Lua 侧 `scheduler_shutdown()` 一个出口，
            // 而 `cleanup` 是插件自愿写的（没写是合法形态），于是"用了共享资源却没写
            // cleanup"的整类插件停用之后登记永留 —— 线程为它续命、它登记的定时任务照旧
            // 到点启动程序，而那一刻 Lua 状态已经跟着上面那行没了，本进程再没有第二条归还路。
            // 现在按名册一次叫完（调度线程、番茄钟、定时器都在册）：卸载 = 这个实例占着的
            // 每一份资源都归还，**不依赖插件写 cleanup**（方案 3.6 与 3.11 那两句）。
            // 只摘自己那个 stem：`users` 是集合，插件自己已经归还过也不会有第二次效果。
            let gave = resources::release_all(&stem);
            if !gave.is_empty() {
                let names: Vec<&str> = gave.clone();
                tracing::info!("插件 {name} 卸载时归还：{}", names.join("、"));
            }
            tracing::info!("插件已卸载: {name}");
            true
        } else {
            false
        }
    }

    /// 按文件名（stem）卸载插件，供热重载处理「插件文件被删除」用。
    /// 插件表以展示名（PLUGIN_NAME）为键，所以要先反查。返回是否真卸载了一个。
    pub fn unload_by_stem(&mut self, stem: &str) -> bool {
        let name = self
            .plugins
            .values()
            .find(|p| Self::stem_of(&p.file_path) == stem)
            .map(|p| p.manifest.name.clone());
        match name {
            Some(n) => self.unload_plugin(&n),
            None => false,
        }
    }

    /// 重新加载插件（按文件名或插件名匹配）。
    ///
    /// **先加载新版本，成功之后才让旧条目退休**（退休那一步在 `load_plugin` 里，
    /// 因为改名时新旧展示名不同、光靠 insert 顶不掉）。旧实现是先 `plugins.remove(n)`
    /// 再 `try_load`，那是"把两步中可能失败的那步放在后面"：编辑器/网盘留下一个
    /// 半截的、语法错误的文件时（这类工具最爱这么写），旧实例已经离开表、而它的
    /// `cleanup()` 按设计没跑，新加载又失败 → 插件从 map 里消失、正打开的详情页
    /// 变成「插件未提供视图」，而它记在 `claim_scheduler` 上的 owner 成了无主孤儿
    /// （`host.rs` 里那份 owner 表只有 Lua 侧 `scheduler_shutdown()` 一个释放入口，
    /// 而那个 Lua 状态已经跟着旧条目没了）→ 本进程再也回收不掉，
    /// 除了重启没有别的办法把定时任务线程还回来。
    /// 现在失败路径**一个字都不动**旧实例：重载失败 = 什么都没发生（插件页多一条
    /// 加载失败原因，见 `try_load`），下一次存盘成功再换上来。
    pub fn reload_plugin(&mut self, key: &str) -> bool {
        // 先按文件名（stem）、再按展示名。它原先生产里唯一的调用方是旧桌面端的热重载
        // （`reload_plugin_by_key`，发的是"改了的那个文件名"；那一侧随 WebView 删了，
        // 原生端接上的现在这一侧走的是 `poll_reload_requests` ⇒ 到这里的路径又有了生产调用方），
        // 而表是以展示名为键的 ⇒ 旧顺序先查展示名会在重名时把**另一个**插件顶上来：
        // 文件 `beta.lua` 变更 → key="beta"，若 `alpha.lua` 的 PLUGIN_NAME 正好叫「beta」，
        // 命中的是 alpha：没改过的被重载、改过的继续跑旧代码，而日志写着"插件已热重载: beta"，
        // 用户看到的就是"存盘没生效"。两种键都存在且指向不同文件时留一条 warn。
        let by_stem = self
            .plugins
            .values()
            .find(|p| Self::stem_of(&p.file_path) == key)
            .map(|p| (p.file_path.clone(), p.source));
        let by_display = self
            .plugins
            .get(key)
            .map(|p| (p.file_path.clone(), p.source));
        if let (Some((s, _)), Some((n, _))) = (&by_stem, &by_display) {
            if s != n {
                tracing::warn!(
                    "重载键 {key} 同时是文件 {} 的名字与另一个插件（文件 {}）的展示名：按文件那个重载",
                    s.display(),
                    n.display()
                );
            }
        }
        let found = by_stem.or(by_display);
        match found {
            Some((p, source)) => {
                // 重载刻意**不**跑 cleanup()：这条路径是「存了个文件」触发的
                // （mtime 变化、构建脚本换目录、编辑器先截断再写入），而番茄插件的
                // cleanup 会 `pomodoro_stop()` —— 那段 `save_current()` 只要实际计时
                // ≥1 秒就给 `work_finished += 1`。于是按一次 Ctrl+S 就把用户跑到一半的
                // 番茄钟判成「完成一个」，还是静默的。调度线程同理，拆了要靠重新渲染
                // 定时任务面板才起得回来。
                // 所以这里只把条目换掉：旧 Lua 状态随旧条目一起释放，新的那份经宿主
                // API 复用同一批进程级单例（沙箱里没有 io/coroutine，插件本身持不住
                // 别的资源），owner 记账按 stem 走、两份实例同一个 stem 也就一次登记。
                // 真正的「停用/删文件」仍然走 unload_plugin → cleanup。
                //
                // 来源**跟着已加载的那一条走**（不是重新问一遍 discover）：重载的触发点是 mtime，
                // 那一刻目录可能正读不动，而"读不到清单"不该把沙箱边界一起放掉 ——
                // 用户插件重载一次就拿回 `accounting_*` 是这条路径上唯一可能的越界方式。
                self.try_load(&p, source).is_ok()
            }
            None => false,
        }
    }

    /// 加载所有未停用的插件（停用的跳过，其代码完全不执行）。
    ///
    /// 目录读不出来时必须出声并直接收手：这里过去吃的是一个被折成空的 `discover()`，
    /// 于是"5 个插件一个都没加载"与"本来就没装插件"在启动路径上是同一段安静 ——
    /// 插件页要等用户点开才发现全是空的，而那一刻目录可能早已能读了。
    /// 扫一遍插件目录，把没加载过的都试一遍。
    ///
    /// 返回值是「这一轮清单到底拿到没有」：`false` = `discover()` 失败，一个插件都没试。
    /// 调用方那个「已经加载过一遍」的闩只许在 `true` 时抬起 —— 一次读不动（杀软首扫、
    /// 网盘同步、构建脚本换目录）不该把整个会话的加载机会一起关掉：闩抬了之后
    /// 再进插件页也只是重扫清单，屏上永远是「已启用，尚未加载」，而那一刻目录早读得出来了。
    pub fn load_all(&mut self) -> bool {
        let disabled = self.disabled_list();
        let found = match self.discover() {
            Ok(found) => found,
            Err(e) => {
                tracing::error!("{e}：本次一个插件也不加载");
                return false;
            }
        };
        // 这一行是 `source` 今天的读者：清单上有几处来源、各几个，出事时（内置目录没打上包、
        // 数据目录被换走）先看这句比翻目录快。插件页两栏分组（P0 第 8 步）是它的界面版。
        let builtin = found
            .iter()
            .filter(|f| f.source == PluginSource::Builtin)
            .count();
        let user = found
            .iter()
            .filter(|f| f.source == PluginSource::User)
            .count();
        tracing::info!("插件清单：内置 {builtin} 个 · 用户 {user} 个");
        for Discovered { path, source } in found {
            let stem = Self::stem_of(&path);
            if disabled.iter().any(|s| s.as_str() == stem) {
                continue;
            }
            // 按文件名判重：插件展示名（PLUGIN_NAME）与文件名通常不同，
            // 用名字判重会导致每次扫描都重复加载。
            if self
                .plugins
                .values()
                .any(|p| Self::stem_of(&p.file_path) == stem)
            {
                continue;
            }
            // 生命周期先分流（方案 3.14）：`panel` 那一档**不在启动时加载**，
            // 面板打开才装（`ensure_loaded`），关掉面板就卸（`release_panel_plugins`）。
            // 这里读的是文本扫描的那份清单，不 exec 脚本：与插件页同一条路（3.3 那条判据），
            // 而且"没打开过的面板不该在启动时就把它的库建出来、线程拉起来"正是这一档的目的。
            let declared = match Self::scan_meta(&path) {
                Ok(m) => m,
                Err(e) => {
                    // 读不出清单就不分流：按 `always` 那一路交给 `try_load` 去报真正的原因
                    // （清单读不动多半是文件本身坏了，那条错误比"跳过"有用）。
                    tracing::warn!("插件 {stem} 的声明读不出来（{e}）：这一格按常驻那一档尝试加载");
                    self.pending.remove(&stem);
                    let _ = self.try_load(&path, source);
                    continue;
                }
            };
            if lifecycle_of(source, &declared) == PluginLifecycle::Panel {
                self.pending
                    .insert(stem.clone(), "面板打开时才加载".to_string());
                continue;
            }
            // 常驻额度（3.11）：装十个常驻插件就是十份 VM，所以按启用顺序取前 N 个，
            // 其余标「待加载」而不是静丢掉 —— 页面上看得见是谁被挤掉了、为什么。
            if self.always_count() >= MAX_ALWAYS_PLUGINS {
                self.pending.insert(
                    stem.clone(),
                    format!(
                        "常驻额度已满（{} 个），待加载：先在插件页停用别的常驻插件",
                        MAX_ALWAYS_PLUGINS
                    ),
                );
                tracing::warn!(
                    "插件 {stem} 是常驻的，但常驻额度已满（{MAX_ALWAYS_PLUGINS} 个）：本次不加载"
                );
                continue;
            }
            self.pending.remove(&stem);
            let _ = self.try_load(&path, source);
        }
        true
    }

    /// 现在活着几份常驻插件（额度那一格用）。
    fn always_count(&self) -> usize {
        self.plugins
            .values()
            .filter(|p| lifecycle_of(p.source, &p.manifest) == PluginLifecycle::Always)
            .count()
    }

    /// 这一格"故意没加载"的那句话。`None` = 没什么要交代的。
    ///
    /// 顺序是刻意的：额度那句排在前面（那是本轮真把它挡在外面的原因），
    /// 其次才是 `panel` 那一档的"等面板打开"。已加载的什么都不说 ——
    /// 对着一张活着的卡片说"没加载"是这一族缺陷最常见的形状。
    fn load_note(
        pending: &HashMap<String, String>,
        stem: &str,
        source: PluginSource,
        manifest: &PluginManifest,
        enabled: bool,
        loaded: bool,
    ) -> Option<String> {
        if loaded {
            return None;
        }
        if let Some(why) = pending.get(stem) {
            return Some(why.clone());
        }
        if enabled && lifecycle_of(source, manifest) == PluginLifecycle::Panel {
            return Some("面板打开时才加载".to_string());
        }
        None
    }

    /// 面板打开时确保这一格活着（方案 3.14 那张表的"面板打开 → 加载"那一行）。
    ///
    /// 为什么它**不受常驻额度限制**：额度是给"没人碰它也要一直活着"的那批准备的预算，
    /// 而用户刚刚点了这一格。挤掉一个已经在跑的常驻插件去腾位置，换来的是
    /// "我打开的面板说我待加载"——那是比超预算更糟的答案。
    ///
    /// 名字两处都认（展示名与文件名）：面板与插件页传的是清单里那一行的名字，
    /// 而清单里两个都可能出现（作者改了 `PLUGIN_NAME` 之后两者就不同）。
    pub fn ensure_loaded(&mut self, name: &str) -> Result<(), String> {
        let alive = self
            .plugins
            .values()
            .any(|p| p.loaded && (p.manifest.name == name || Self::stem_of(&p.file_path) == name));
        if alive {
            return Ok(());
        }
        let mut target: Option<(PathBuf, PluginSource, String)> = None;
        for found in self.discover()? {
            let stem = Self::stem_of(&found.path);
            let display = Self::scan_meta(&found.path)
                .map(|m| m.name)
                .unwrap_or_else(|_| stem.clone());
            if stem == name || display == name {
                target = Some((found.path, found.source, display));
                break;
            }
        }
        let (path, source, display) = match target {
            Some(t) => t,
            None => {
                return Err(format!(
                    "插件「{name}」不在插件目录里：先确认文件还在（列出的两处是 {:?}）",
                    self.plugin_dirs()
                ));
            }
        };
        self.pending.remove(&Self::stem_of(&path));
        self.try_load(&path, source)?;
        if !self
            .plugins
            .get(&display)
            .map(|p| p.loaded)
            .unwrap_or(false)
        {
            return Err(format!("插件「{name}」加载后没进清单（展示名 {display}）"));
        }
        Ok(())
    }

    /// 关掉某一页：只卸它自己那一格，而且只卸 `panel` 那一档（方案 3.14）。
    ///
    /// 为什么不写成"把所有 panel 档都卸掉"：切换面板时上一页关掉与下一页打开是同一个手势，
    /// 一次叫完会把刚打开的那一页一起卸掉（症状是"点开第二页要等它重新装一次"）。
    /// `always` 的那一档不动 —— 番茄钟的按键统计与日程的到点触发都靠常驻投递，
    /// 随面板卸载就会**断统计**（那正是 3.3 给 `PLUGIN_LIFECYCLE` 写的理由）。
    ///
    /// 交回有没有真的卸掉：常驻那一格点了关闭是什么都没发生，这句要能说出口。
    pub fn release_panel_of(&mut self, name: &str) -> bool {
        let panel = self
            .plugins
            .get(name)
            .map(|p| p.loaded && lifecycle_of(p.source, &p.manifest) == PluginLifecycle::Panel)
            .unwrap_or(false);
        if !panel {
            return false;
        }
        self.unload_plugin(name)
    }

    /// 获取插件列表。
    pub fn get_all_plugins(&self) -> Vec<&PluginInfo> {
        self.plugins.values().collect()
    }

    /// 启动自检用（B15）：加载失败的插件清单（展示名/文件名 + 最后一次错误）。
    ///
    /// 「启用但加载失败」有两个去向：进了 `plugins` 但被标成未加载（`try_load`
    /// 失败的），以及连 parse 都没过、只留在 `load_errors` 的 —— 两处都要收，
    /// 否则自检报告只能看见一半。已加载与已停用的不在此列。
    pub fn load_failures(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        for p in self.plugins.values() {
            if !p.loaded {
                if let Some(e) = &p.error {
                    out.push((p.manifest.name.clone(), e.clone()));
                }
            }
        }
        for (file, e) in &self.load_errors {
            out.push((file.clone(), e.clone()));
        }
        out
    }

    /// 获取单个插件。
    pub fn get_plugin(&self, name: &str) -> Option<&PluginInfo> {
        self.plugins.get(name)
    }

    /// 启用热重载：启动检测线程（只扫描文件，不执行 Lua）。
    pub fn enable_hot_reload(&mut self) {
        if self.hot_reload_thread.is_some() {
            return;
        }
        self.stop_event.store(false, Ordering::SeqCst);
        let stop = Arc::clone(&self.stop_event);
        let tx = self.reload_tx.clone();
        let dirs = self.plugin_dirs();
        let handle = std::thread::Builder::new()
            .name("plugin-hot-reload".into())
            .spawn(move || hot_reload_loop(dirs, tx, stop))
            .map_err(|e| tracing::error!("启动热重载线程失败: {e}"))
            .ok();
        // spawn 失败时 .ok() 已经是 None，字段本身就该留 None
        self.hot_reload_thread = handle;
        tracing::info!("插件热重载已启用");
    }

    /// 禁用热重载。
    pub fn disable_hot_reload(&mut self) {
        self.stop_event.store(true, Ordering::SeqCst);
        if let Some(handle) = self.hot_reload_thread.take() {
            let _ = handle.join();
        }
        tracing::info!("插件热重载已禁用");
    }

    /// GUI 线程轮询：处理热重载请求。返回本次实际换上来的插件名列表
    /// （文件已被删除的那些，卸载成功后也算 —— 调用方要刷新列表的就是这批）。
    ///
    /// 删除这条在 core 这边原先是**检测到也没人处理**的：`reload_plugin` 按名字找不
    /// 到实例就返回 false，于是"删掉 .lua"要等下次重启才生效，而删除表达的恰恰是
    /// 「别再跑它了」（它的 cleanup 也就一直不执行）。旧桌面端早就按这个口径做了
    /// （那条 `reload_plugin_by_key` 随 WebView 一起删了；原生端到今天还没接这条线）。
    pub fn poll_reload_requests(&mut self) -> Vec<String> {
        let mut reloaded = Vec::new();
        let dirs = self.plugin_dirs();
        while let Ok(name) = self.reload_rx.try_recv() {
            // "文件还在不在"要问这一轮真被扫的那几处：只问内置目录会把用户插件的删除
            // 当成"还在"，只问数据目录则反过来把内置的删除吞掉。
            if !dirs.iter().any(|d| plugin_file_for(d, &name).is_some()) {
                if self.unload_by_stem(&name) {
                    tracing::info!("插件文件已删除，已卸载: {name}");
                    reloaded.push(name);
                }
                continue;
            }
            if self.reload_plugin(&name) {
                reloaded.push(name);
            }
        }
        reloaded
    }

    /// 调用插件函数（GUI 线程）。
    /// 返回是否成功（函数存在且执行无错）。
    pub fn call_plugin_fn<R>(
        &mut self,
        name: &str,
        fn_name: &str,
        args: mlua::MultiValue,
    ) -> Result<R, String>
    where
        R: mlua::FromLua + mlua::IntoLua,
    {
        let info = self
            .plugins
            .get(name)
            .ok_or_else(|| format!("插件不存在: {name}"))?;
        let lua = info
            .lua
            .as_ref()
            .ok_or_else(|| format!("插件未加载: {name}"))?;
        let f: mlua::Function = lua
            .globals()
            .get(fn_name)
            .map_err(|e| format!("获取 {fn_name} 失败: {e}"))?;
        let ret: R = match f.call(args) {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("调用 {fn_name} 失败: {e}");
                if is_limit_error(&e) {
                    self.mark_plugin_error(name, msg.clone());
                }
                return Err(msg);
            }
        };
        Ok(ret)
    }

    /// 取一个全局钩子，把「没写」「写成了函数」「写了名字但不是函数」分开交回。
    ///
    /// `on_action = "handle_click"`（把函数名当字符串赋给钩子，或 `on_action = handler`
    /// 写在 `local handler` 之前）是这一族最常见的作者笔误，而
    /// `globals().get::<mlua::Function>(key)` 对 nil 和对一张表报的是同一个错，
    /// 于是每一处都把它念成「插件未定义 on_action」—— 作者照那句话去找一个他明明写了的东西。
    /// `record_key` 那一处已经分开（见 `plugin_key_event`），这条补齐同一族剩下的入口。
    ///
    /// `Ok(None)` = 真没写，是多数插件的正常形态，调用方不许为它出声。
    fn take_hook(lua: &Lua, key: &str) -> Result<Option<mlua::Function>, String> {
        let value = match lua.globals().get::<mlua::Value>(key) {
            Ok(v) => v,
            Err(e) => return Err(format!("读取 {key} 失败: {e}")),
        };
        match value {
            mlua::Value::Function(f) => Ok(Some(f)),
            mlua::Value::Nil => Ok(None),
            _ => Err(format!(
                "插件的 {key} 有值但不是函数（钩子要写函数本体，别写函数名的字符串）"
            )),
        }
    }

    /// 重新执行 get_view() 并更新视图缓存。
    /// 动作/输入回写会改变插件状态，前端拿到的视图必须是新鲜的，
    /// 否则会出现"点了没反应"（如分页不切换）。
    pub fn refresh_view(&mut self, name: &str) -> Result<(), String> {
        let cfg = self.config;
        // 这一条路已经停过：不再进 Lua，把那句原因原样交回（面板上看得见）。
        if let Some(why) = self
            .plugins
            .get(name)
            .and_then(|i| i.view_route.stopped())
            .map(str::to_string)
        {
            return Err(why);
        }
        let info = self
            .plugins
            .get(name)
            .ok_or_else(|| format!("插件不存在: {name}"))?;
        let lua = info
            .lua
            .as_ref()
            .ok_or_else(|| format!("插件未加载: {name}"))?;
        // 无 get_view 的插件没有视图，跳过；写了名字却不是函数的那一种要报错，
        // 不能捏成「这插件没有面板」—— 界面上只会少一张卡片，而原因在他自己写的文件里。
        match Self::take_hook(lua, "get_view") {
            Ok(Some(_)) => {}
            Ok(None) => return Ok(()),
            Err(e) => {
                let stop = self.note_view_failure(name, &e);
                return Err(stop.unwrap_or(e));
            }
        }
        Self::arm_instruction_budget(lua, cfg);
        let view_started = std::time::Instant::now();
        let view = Self::read_view(lua);
        // 失败的那一次也要进台账：它确实跑过，耗时也真花掉了。只记成功那一刻的话，
        // 详情页的「视图重建」会把一个一直失败的插件念成「没跑过」（与"这一类没接线"
        // 是同一句话），p95 也只统计跑成功的那几次 —— 看上去比实际快。
        self.note_hook_time(name, plugin_health::HookKind::View, view_started.elapsed());
        let view = match view {
            Ok(v) => v,
            Err(e) => {
                let msg = format!("get_view() 失败: {e}");
                if is_limit_error(&e) {
                    self.mark_plugin_error(name, msg.clone());
                    return Err(msg);
                }
                // 跨过阈值的那一次就把"这一路停了"交回去：调用方（面板/页面）
                // 看到的应该是最新的结论，而不是刚才那条还会被重试的错误。
                return Err(self.note_view_failure(name, &msg).unwrap_or(msg));
            }
        };
        if let Some(p) = self.plugins.get_mut(name) {
            p.view_route.record(false);
            p.view = Some(view);
        }
        Ok(())
    }

    /// 视图那一路的一次失败：攒到窗口阈值就停刷新，**不卸插件**（按键/动作还能用）。
    /// 跨过阈值的那一次交回那句"这一路停了"，其余时候交回 `None`。
    fn note_view_failure(&mut self, name: &str, last: &str) -> Option<String> {
        // 进详情页的错误历史：路一停就不再记了（最多三条），不会把历史刷满。
        self.note_hook_error(name, last);
        let info = self.plugins.get_mut(name)?;
        if !info.view_route.record(true) {
            return None;
        }
        let stop = format!(
            "视图刷新已停：最近 {FAIL_WINDOW} 次里 {FAIL_TRIPS} 次失败（最后一次：{last}）——修好后停用再启用"
        );
        info.view_route.stop_with(stop.clone());
        if info.error.is_none() {
            info.error = Some(stop.clone());
        }
        tracing::warn!("插件 {name} {stop}");
        Some(stop)
    }

    /// 调用插件按钮动作（动作后刷新视图缓存）。
    ///
    /// 返回值只表示**动作**成不成功：`on_action` 已经跑完（任务删了、记录写了）之后
    /// 才失败的 `get_view()`，不能报成"动作失败"—— 那正是 `set_enabled` 修过的同一类
    /// 谎报（插件页每点一次「停用」弹一条失败，可插件确实已经卸载）。刷新失败留一条
    /// warn，视图沿用上一份，下一次 `get_plugin_view` 会再试并把错误摊开。
    pub fn plugin_action(&mut self, name: &str, action_id: &str) -> Result<(), String> {
        let cfg = self.config;
        let info = self
            .plugins
            .get(name)
            .ok_or_else(|| format!("插件不存在: {name}"))?;
        let lua = info
            .lua
            .as_ref()
            .ok_or_else(|| format!("插件未加载: {name}"))?;
        // 「没写」与「写了但不是函数」分开：后一种过去也念成"未定义"。
        match Self::take_hook(lua, "on_action")? {
            Some(on_action) => {
                Self::arm_instruction_budget(lua, cfg);
                let started = std::time::Instant::now();
                let outcome = on_action.call::<()>(action_id);
                self.note_hook_time(name, plugin_health::HookKind::Action, started.elapsed());
                if let Err(e) = outcome {
                    let msg = format!("on_action 失败: {e}");
                    if is_limit_error(&e) {
                        self.mark_plugin_error(name, msg.clone());
                    }
                    return Err(msg);
                }
                if let Err(e) = self.refresh_view(name) {
                    tracing::warn!("插件 {name} 动作已执行，但视图刷新失败: {e}");
                }
                Ok(())
            }
            None => Err("插件未定义 on_action".to_string()),
        }
    }

    /// 向插件投递**一批**按键：`key` 是键名，`count` 是这一批里它的增量。
    ///
    /// 为什么按批而不是逐键：mlua 的 `Lua` 不是 Send，插件的 VM 只能由 GUI 线程碰（模块顶部
    /// 那条设计约束），所以写线程那一侧只把增量并进取件口，真正进 Lua 的是取件的那一跳。
    /// 调用次数因此与"这 10 秒里出现过几种键"成正比，而不是按键次数 —— 逐键进 VM 等于把插件
    /// 钩子死循环这一族事故请回按键热路径（它炸过一次启动）。
    ///
    /// 这条是唯一一个**调用方看不见失败**的 Lua 入口：`on_action` 与 `set_field` 把原因
    /// `return Err` 给点的那个人，而这条的返回值是 `()`。
    /// 所以非限额的报错过去整包丢掉 —— 插件的 `record_key` 一抛错，按键数从这一次起
    /// 再也不涨，插件页却仍写着已加载、没有一行原因。
    pub fn plugin_key_event(&mut self, name: &str, key: &str, count: i64) {
        let cfg = self.config;
        let info = match self.plugins.get(name) {
            Some(i) => i,
            None => return,
        };
        let lua = match &info.lua {
            Some(l) => l,
            None => return,
        };
        // 按键这一路已经停过：不再进 Lua（插件的其余功能照常）。
        if info.key_route.stopped().is_some() {
            return;
        }
        // 「没定义这个钩子」是正常形态（多数插件不收按键），不许因此出声；
        // 但"有一个同名的东西、偏偏不是函数"是作者写错了，过去跟前者共用同一个静默出口。
        let hook = match lua.globals().get::<mlua::Value>("record_key") {
            Ok(mlua::Value::Function(f)) => f,
            Ok(mlua::Value::Nil) => return,
            Ok(_other) => {
                // 有个同名的东西、偏偏不是函数（写成字符串或表是作者最常见的笔误）
                self.note_key_hook_failure(name, "record_key 有值但不是函数".to_string());
                return;
            }
            Err(e) => {
                self.note_key_hook_failure(name, format!("读取 record_key 失败: {e}"));
                return;
            }
        };
        Self::arm_instruction_budget(lua, cfg);
        let key_started = std::time::Instant::now();
        // 两个参数：键名 + 这一批的增量。只声明一个形参的旧插件照旧能收（多给的参数被丢掉）。
        let outcome = hook.call::<()>((key, count.max(1)));
        self.note_hook_time(
            name,
            plugin_health::HookKind::KeyEvent,
            key_started.elapsed(),
        );
        if let Err(e) = outcome {
            if is_limit_error(&e) {
                // 跑飞了：这一类必须停用整个插件（原有口径，不许被下面那条弱化）
                self.mark_plugin_error(name, format!("record_key 失败: {e}"));
            } else {
                self.note_key_hook_failure(name, format!("record_key 抛错，按键数不再累计: {e}"));
            }
        } else if let Some(info) = self.plugins.get_mut(name) {
            info.key_route.record(false);
        }
    }

    /// 已加载（拿得出 VM）的插件名。攒批分发先问这一份，别每个键名都撞一遍全表。
    pub fn loaded_plugin_names(&self) -> Vec<String> {
        self.plugins
            .iter()
            .filter(|(_, info)| info.lua.is_some())
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// 按键钩子出问题时"挂个原因"，但**不停用插件**、也**不每次按键都喊**。
    ///
    /// 为什么不复用 `mark_plugin_error`：那一条是给"这 Lua 环境已经不可信"（跑满指令上限）
    /// 用的，会卸实例、归还调度线程所有权；作者在一次按键的表达式里写错一个 nil，
    /// 不该没收整个面板。为什么不每轮 warn：这条是每个按键调一次，日志会被刷满，
    /// 而那一句真正要说的只有"从这一次起不再累计"。
    ///
    /// `error` 已有值时不覆盖：那是别处挂上的原因（例如热重载失败），这一行本来就红的。
    /// 代价是按键停摆这件事在那一行上被前一个原因挡住 —— 日志里那一句仍然只记一次，
    /// 而两种失败本来就要分开看。
    fn note_key_hook_failure(&mut self, name: &str, msg: String) {
        // 原因也要进历史：这一类不停用插件，页面上那行只留最后一次，而"从这一次起按键数
        // 不再涨"发生了几次、分别是因为什么，是排障要往回看的东西。
        self.note_hook_error(name, &msg);
        let tripped = self
            .plugins
            .get_mut(name)
            .map(|info| info.key_route.record(true))
            .unwrap_or(false);
        let Some(info) = self.plugins.get_mut(name) else {
            return;
        };
        if tripped {
            // 到阈值了：只停**按键这一路**。整个插件不卸 —— 视图、动作、定时器都还在，
            // 用户不会因为一次按键表达式写错就丢掉整块面板。
            let stop = format!(
                "按键采集已停：record_key 最近 {FAIL_WINDOW} 次里 {FAIL_TRIPS} 次失败（最后一次：{msg}）——修好后停用再启用"
            );
            info.key_route.stop_with(stop.clone());
            info.error = Some(stop.clone());
            tracing::warn!("插件 {name} {stop}");
            return;
        }
        if info.error.is_some() {
            return;
        }
        info.error = Some(msg.clone());
        tracing::warn!("插件 {name} 收按键失败：{msg}");
    }

    /// 调用插件 set_field(field, value)（输入框回传，随后刷新视图缓存）。
    pub fn plugin_set_field(&mut self, name: &str, field: &str, value: &str) -> Result<(), String> {
        let cfg = self.config;
        let info = self
            .plugins
            .get(name)
            .ok_or_else(|| format!("插件不存在: {name}"))?;
        let lua = info
            .lua
            .as_ref()
            .ok_or_else(|| format!("插件未加载: {name}"))?;
        // 与 on_action 同一族：写了名字、但写的不是函数。
        match Self::take_hook(lua, "set_field")? {
            Some(set_field) => {
                Self::arm_instruction_budget(lua, cfg);
                let started = std::time::Instant::now();
                let outcome = set_field.call::<()>((field, value));
                self.note_hook_time(name, plugin_health::HookKind::Action, started.elapsed());
                if let Err(e) = outcome {
                    let msg = format!("set_field 失败: {e}");
                    if is_limit_error(&e) {
                        self.mark_plugin_error(name, msg.clone());
                    }
                    return Err(msg);
                }
                if let Err(e) = self.refresh_view(name) {
                    tracing::warn!("插件 {name} 输入已写入，但视图刷新失败: {e}");
                }
                Ok(())
            }
            None => Err("插件未定义 set_field".to_string()),
        }
    }
}

/// 判断两个路径指向的是不是**同一个源文件**。
///
/// 撞名闸改成按源文件判重（见 `load_plugin`）之后，这一步的准头直接决定"重载"和
/// "撞名"分不分得开：判成同一个文件 → 放行（那是自我重载，旧条目随后退休）；
/// 判成两个文件 → 拒。所以先按原样比，再 `canonicalize` 比一次（同一个文件可能以
/// 相对/绝对、大小写不同的写法传进来），都拿不到就保守地当成**两个**文件 ——
/// 宁可多拒一次撞名，也不能把"两个插件抢一个名字"放过去。
fn is_same_source(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// 判断 Lua 错误是否由资源限制触发（指令数超限 / 内存超限）。
/// 此类错误说明插件已不可信，应停用插件而非继续复用其 Lua 环境。
fn is_limit_error(e: &mlua::Error) -> bool {
    matches!(e, mlua::Error::MemoryError(_)) || e.to_string().contains("指令数上限")
}

/// 热重载检测循环：扫描插件目录 mtime，变更时发送重载请求。
/// 不执行 Lua（Lua 非 Send，只能在 GUI 线程）。
///
/// 三条保护是照旧桌面端那份热重载（`start_hot_reload`，已随 WebView 删掉）补的 ——
/// 那三条都是那边先踩过、写过注释才定下来的：
/// 1. 目录整体没读出来时**既不比对、也不清基线**（见 [`scan_plugin_mtimes`]）；
/// 2. **文件消失**也要发请求（见 [`reload_requests`]），否则删掉的插件要重启才停；
/// 3. 0 字节的 `.lua` 本轮不算变更（编辑器"先截断再写入"的一瞬间，见
///    [`scan_plugin_mtimes`]）—— 空的 .lua 会被 Lua 当成空块、一句错都不报，
///    插件就此变成没有函数的空壳。
///
/// 热重载检测线程：每 2 秒把这一轮被扫的**每一处**目录各扫一遍，变更名发到通道里。
///
/// 基线与"是否已配平"必须**按目录各存一份**：首轮只建基线不发事件，否则每次启动都把全部
/// 插件当新文件重载一遍。共用一份 primed 的话，第二处目录会在第一处配平后被当成
/// "全都是新的"，一开程序就把所有插件重载一次。
fn hot_reload_loop(dirs: Vec<PathBuf>, tx: mpsc::Sender<String>, stop: Arc<AtomicBool>) {
    let mut state: Vec<(PathBuf, HashMap<String, Option<SystemTime>>, bool)> = dirs
        .into_iter()
        .map(|dir| (dir, HashMap::new(), false))
        .collect();
    while !stop.load(Ordering::SeqCst) {
        for (dir, last_mtime, primed) in state.iter_mut() {
            let (names, unreadable) = hot_reload_round(dir, last_mtime, primed);
            for name in names {
                tracing::info!("热重载检测到变更: {name}");
                let _ = tx.send(name);
            }
            // 读不出来这件事每 2 秒会重来一次，而它是盘上的稳定状态：照实每轮说一次，
            // 一天能刷出四万条，把真正要看的那几条埋掉。所以走全仓那一把「同一件事只说一次」
            // 的尺子，并且**按目录各记一格** —— 用常数当键，内置那一格读不动就会把数据目录
            // 那一格的公告一起吃掉（症状：只有一处目录坏了，日志里却永远只提另一处）。
            if let Some(why) = unreadable {
                if notice_once(&format!("watch-unreadable:{}", dir.display())) {
                    tracing::error!(
                        "热重载扫描读不到插件目录（{}），本轮不判定任何变更 —— 这不是「没有插件」: {why}",
                        dir.display()
                    );
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(2000));
    }
}

/// 一轮扫描的全部判定（线程里除了 sleep 就只剩这一句，判定本身全在这里 —— 这样
/// 上面那三条保护才测得动，不用去起真线程、也不用等 2 秒一周期的 tick）。
/// 交回「这一轮该发出去的重载请求」与「这一轮目录读不出来的原因」（`None` = 读得动），
/// 出声与否留给调用方定（线程要按目录去重，判定函数不该替它决定）。
/// `last` / `primed` 是跨轮状态。
fn hot_reload_round(
    dir: &Path,
    last: &mut HashMap<String, Option<SystemTime>>,
    primed: &mut bool,
) -> (Vec<String>, Option<String>) {
    // 读不出来：基线与 `primed` 一个字都不动，下一轮再说（也不发任何"文件消失"）。
    let seen = match scan_plugin_mtimes(dir, last) {
        Ok(seen) => seen,
        Err(why) => return (Vec::new(), Some(why)),
    };
    let requests = reload_requests(last, &seen, *primed);
    *last = seen;
    *primed = true;
    (requests, None)
}

/// 扫一次插件目录，得到「文件名 → mtime」快照；**`Err` 表示目录没读出来**
/// （`plugins` 被建成同名文件、整份放在断线的网盘上、构建脚本正在替换目录……）。
/// 这条区分必须传出去：把"没读到"当"目录空了"参与比对，每个插件都会被判成
/// 「文件已删除」而全部卸载；把基线清空，则下一轮成功读取会把所有文件当成新变更，
/// 白重载一整轮。
///
/// 这里不出声：一轮扫描每秒被调到两次，去重是线程那一层的事（见 [`hot_reload_loop`]），
/// 判定函数只负责把原因原样交回去。
///
/// `last` 只用来给 0 字节的文件沿用上一轮指纹（本轮既不算新增也不算消失）。
fn scan_plugin_mtimes(
    dir: &Path,
    last: &HashMap<String, Option<SystemTime>>,
) -> Result<HashMap<String, Option<SystemTime>>, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => return Err(e.to_string()),
    };
    let mut seen: HashMap<String, Option<SystemTime>> = HashMap::new();
    for entry in entries.flatten() {
        let p = entry.path();
        if !p
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e == "lua" || e.eq_ignore_ascii_case("zip"))
        {
            continue;
        }
        let Some(name) = p.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&p) else {
            continue;
        };
        if meta.len() == 0 {
            // 0 字节 = 还没写完的那一瞬间：沿用上一轮指纹（没有就不记），
            // 否则删除检测会把它误判成"文件消失"，而插件页会拿到一个空壳。
            if let Some(prev) = last.get(name) {
                seen.insert(name.to_string(), *prev);
            }
            continue;
        }
        seen.insert(name.to_string(), meta.modified().ok());
    }
    Ok(seen)
}

/// 两轮快照之间该发哪些重载请求：mtime 变了、出现了新文件、**文件消失了**。
/// 只遍历 `seen` 会漏掉最后一种 —— 而删掉文件表达的意思恰恰是「别再跑它了」。
/// `primed == false`（首轮）什么都不发。返回排序后的稳定顺序，便于日志与测试对账。
fn reload_requests(
    last: &HashMap<String, Option<SystemTime>>,
    seen: &HashMap<String, Option<SystemTime>>,
    primed: bool,
) -> Vec<String> {
    if !primed {
        return Vec::new();
    }
    let mut out: Vec<String> = seen
        .iter()
        .filter(|(name, mtime)| last.get(*name) != Some(mtime))
        .map(|(name, _)| name.clone())
        .collect();
    for name in last.keys() {
        if !seen.contains_key(name) {
            out.push(name.clone());
        }
    }
    out.sort();
    out
}

/// 把 Lua 源码里**不是代码**的部分抹掉：`--` 行注释、`--[[ ]]` / `[==[ ]==]` 块注释、
/// `[[ ]]` 长字符串。目标是让"这一行的第一个代码记号"这件事可以用**列号**回答：
/// - 整段正好从行首开始 → 直接删掉（调用处连它后面的空白一起吞掉），于是
///   `--[[ 说明 ]] PLUGIN_NAME = "值"` 这种"注释打头、后面跟着真声明"的一行仍算顶层；
/// - 否则等长换成空格（保住后面那些字节的列号）；
/// - 换行一律原样保留：行号不能错位，而块注释/长字符串**内部**那些顶在第 0 列的
///   假赋值整段都在被抹掉的区间里，这正是这一轮要挡掉的东西。
///
/// 为什么不写成"正经的 Lua 词法分析器"：这里只需要回答"这一行是不是顶层的
/// `KEY = "字面量"`"，而扫错名字的代价已经写在 `PluginManager::scan_meta` 上了。
/// 引号内的 `--` 不当注释（`PLUGIN_DESC = "a -- b"` 整值被抹掉就是假阴性）；
/// 单行字符串到行尾还没闭合时后半段原样保留（半截文件本来就语法错，别再把手上
/// 唯一一处真名字也吃掉）。所有切片走 `.get()`：读的是外部文件，release 是 abort。
pub(crate) fn strip_lua_non_code(src: &str) -> String {
    let b = src.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    // 刚删掉一段行首的注释/长字符串：连它后面到下一个代码记号之间的空白一起吞
    let mut swallow = false;
    while i < b.len() {
        match b[i] {
            b'-' if b.get(i + 1) == Some(&b'-') => {
                // `--[[ … ]]` 抹到配平的 `]==]`（没有结尾就是文件被写坏，抹到文件尾）；
                // `-- …` 抹到行尾（换行本身留着）。
                let end = match long_bracket_open(b, i + 2) {
                    Some((level, after)) => {
                        long_bracket_close_from(b, after, level).unwrap_or(b.len())
                    }
                    None => newline_from(b, i).unwrap_or(b.len()),
                };
                swallow = blank_non_code(&mut out, &b[i..end]);
                i = end;
            }
            b'[' if long_bracket_open(b, i).is_some() => {
                // 长字符串里的 `PLUGIN_NAME = "…"` 是数据，不是声明
                let (level, after) = long_bracket_open(b, i).expect("上面刚判定过是长括号");
                let end = long_bracket_close_from(b, after, level).unwrap_or(b.len());
                swallow = blank_non_code(&mut out, &b[i..end]);
                i = end;
            }
            q @ (b'"' | b'\'') => {
                // 字符串照原样保留（值正是我们要取的东西），整段"吃掉"不再往里看
                swallow = false;
                out.push(q);
                let mut j = i + 1;
                while let Some(&x) = b.get(j) {
                    if x == b'\\' {
                        out.push(x);
                        if let Some(&y) = b.get(j + 1) {
                            out.push(y);
                        }
                        j += 2;
                        continue;
                    }
                    j += 1;
                    out.push(x);
                    if x == b'\n' || x == q {
                        break; // 闭合，或这一行根本没闭合
                    }
                }
                i = j;
            }
            b' ' | b'\t' if swallow => {
                i += 1;
            }
            c => {
                swallow = false;
                out.push(c);
                i += 1;
            }
        }
    }
    // 正常一定解得开（只写 ASCII 空白 + 原文的合法片段）；解不开也不 panic，退化成 lossy。
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// 把一段"不是代码"的字节追加进 `out`（见 [`strip_lua_non_code`] 的两种形状）。
/// 返回 `true` = 这一段是从行首整段删掉的，后面的空白该一起吞掉。
fn blank_non_code(out: &mut Vec<u8>, chunk: &[u8]) -> bool {
    if out.last().is_none_or(|&c| c == b'\n') {
        out.extend(chunk.iter().copied().filter(|&c| c == b'\n'));
        true
    } else {
        out.extend(chunk.iter().map(|&c| if c == b'\n' { b'\n' } else { b' ' }));
        false
    }
}

/// `i` 处是否是长括号起始 `[[` / `[=[` / `[==[`…：返回 (等号个数, 起始标记之后的下标)。
fn long_bracket_open(b: &[u8], i: usize) -> Option<(usize, usize)> {
    if b.get(i) != Some(&b'[') {
        return None;
    }
    let mut j = i + 1;
    let mut level = 0usize;
    while b.get(j) == Some(&b'=') {
        level += 1;
        j += 1;
    }
    if b.get(j) == Some(&b'[') {
        Some((level, j + 1))
    } else {
        None
    }
}

/// 从 `i` 起找配平的长括号结束标记 `]` + `=`×level + `]`，返回其之后的下标。
fn long_bracket_close_from(b: &[u8], mut i: usize, level: usize) -> Option<usize> {
    loop {
        i = b.get(i..)?.iter().position(|&c| c == b']')? + i;
        let mut j = i + 1;
        let mut n = 0usize;
        while b.get(j) == Some(&b'=') {
            n += 1;
            j += 1;
        }
        if n == level && b.get(j) == Some(&b']') {
            return Some(j + 1);
        }
        i += 1;
    }
}

/// `i` 起第一个换行的下标（`None` = 到文件尾）。
fn newline_from(b: &[u8], i: usize) -> Option<usize> {
    Some(b.get(i..)?.iter().position(|&c| c == b'\n')? + i)
}

/// 从一行**已抹掉注释**的 Lua 代码里取顶层元数据赋值 `KEY = "值"`（取不到 `None`）。
///
/// 刻意不做转义、不接受 `[[长字符串]]` 当值、也不接受 `KEY = 变量` 这类计算值：
/// 元数据这几个字段就是给人看的名字/简介/版本，扫不到时调用方回退成文件名，
/// 换来的是"列表刷新绝不执行插件代码"。
///
/// 两条判据都是为了"别把假名字当真的"（旧实现都不挡）：
/// - **不 trim 行首空白**：带缩进说明它写在某个块里，函数体里的
///   `PLUGIN_NAME = "…"` 只是个局部变量，加载后 `read_meta` 从 globals 里取不到它；
/// - **不接受 `local KEY = …`**：`local` 的同样不会成为全局变量。
pub(crate) fn top_level_string_assign(line: &str, key: &str) -> Option<String> {
    let after_key = line.strip_prefix(key)?.trim_start();
    let value = after_key.strip_prefix('=')?.trim_start();
    let quote = *value.as_bytes().first()?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let inner = value.get(1..)?;
    let end = inner.find(quote as char)?;
    Some(inner.get(..end)?.to_string())
}

/// 从一行**已抹掉注释**的 Lua 代码里取顶层整数赋值 `KEY = 123`（取不到 `None`）。
///
/// 与 [`top_level_string_assign`] 同一套纪律（不 trim 行首 ⇒ 挡掉函数体里的缩进赋值，
/// `strip_prefix(key)` 挡掉 `local KEY = …`），并且**只认裸的十进制整数**（可带一个负号）：
/// `PLUGIN_API = "2"` / `= 2.0` / `= 2 + 0` 一律算没声明。理由写在那两条缺省折叠里 ——
/// 加载侧读的是 Lua 整数，两侧认得动的项目必须完全一样，否则同一份文件两处折成两个值。
///
/// 负号**要认**：`PLUGIN_REFRESH_MS = -5` 是"写了但写坏了"，扫出来才谈得上出声一句；
/// 当成没声明的话，症状恰恰是作者最难受的那种（改了文件，屏上毫无反应）。
/// 折叠规则本身（负数 ⇒ 缺省 0）在 [`build_manifest`]，两侧共用。
pub(crate) fn top_level_int_assign(line: &str, key: &str) -> Option<i64> {
    let after_key = line.strip_prefix(key)?.trim_start();
    let value = after_key.strip_prefix('=')?.trim_start();
    let digits = value.strip_prefix('-').unwrap_or(value);
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    value.parse::<i64>().ok()
}

/// 一行里出现的引号串内容（`{ "a", 'b' }` → `["a", "b"]`）。
///
/// 注释与长字符串早就被 `strip_lua_non_code` 处理过了，这里只看引号；
/// 反斜杠按 Lua 的转义吃掉下一个字符（`{ "a\"b" }` 不至于在 `\"` 中间断开）。
fn quoted_strings_in(seg: &str) -> Vec<String> {
    let b = seg.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let q = b[i];
        if q != b'"' && q != b'\'' {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        let mut piece = Vec::new();
        loop {
            match b.get(j) {
                None => break, // 这行没闭合：这段不要（半截文件本来就不是声明）
                Some(&b'\\') => {
                    if let Some(&c) = b.get(j + 1) {
                        piece.push(c);
                    }
                    j += 2;
                }
                Some(&c) if c == q => break, // 闭合
                Some(&c) => {
                    piece.push(c);
                    j += 1;
                }
            }
        }
        if j < b.len() && b[j] == q {
            if let Ok(s) = String::from_utf8(piece) {
                out.push(s);
            }
            i = j + 1;
            continue;
        }
        // 没找到闭合引号：整段放弃，别把行尾的半个串当成声明
        return out;
    }
    out
}

/// 从**已抹掉注释**的源码里取顶层表赋值 `KEY = { "a", "b" }` 的字符串项（取不到 `None`）。
///
/// 表可以跨行（作者们真会把它写成多行），但起始那一行仍受顶层纪律约束：从第 0 列起正好是
/// `KEY = {`，`local` 与缩进都不算。只在这一处开始往后读，读到配平的 `}` 为止；
/// 读不到 `}`（文件被写坏）当"没声明"，交缺省值 —— 加载侧遇到同样的半截表也拿不到东西。
/// 这一行是不是顶层的 `KEY = {`：是就交出花括号之后剩下的那段。
pub(crate) fn table_opened_at(line: &str, key: &str) -> Option<String> {
    let after_key = line.strip_prefix(key)?.trim_start();
    let value = after_key.strip_prefix('=')?.trim_start();
    value.strip_prefix('{').map(|s| s.to_string())
}

/// 某个声明字段落在**原文件**的第几行（1 起）。
///
/// 传进来的必须是 [`strip_lua_non_code`] 之后的那一份：它把注释与长字符串整段抹成空格，
/// 但**逐字保住换行**（`blank_non_code` 只把非代码段里的 `\n` 原样吐回去），所以行号与
/// 原文件一一对应。判据用上面那三个顶层赋值谓词，与加载侧同一份规则 —— 校验器报的行号
/// 若与加载侧认的赋值不是同一行，作者就会照着错的那一行改。
pub(crate) fn field_line(code: &str, key: &str) -> Option<usize> {
    code.lines().enumerate().find_map(|(i, line)| {
        (top_level_string_assign(line, key).is_some()
            || top_level_int_assign(line, key).is_some()
            || table_opened_at(line, key).is_some())
        .then_some(i + 1)
    })
}

pub(crate) fn top_level_table_strings(code: &str, key: &str) -> Option<Vec<String>> {
    let lines: Vec<&str> = code.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let Some(mut buf) = table_opened_at(lines[i], key) else {
            i += 1;
            continue;
        };
        let mut items = Vec::new();
        loop {
            match buf.find('}') {
                Some(close) => {
                    items.extend(quoted_strings_in(&buf[..close]));
                    return Some(items);
                }
                None => {
                    items.extend(quoted_strings_in(&buf));
                    i += 1;
                    let Some(next) = lines.get(i) else {
                        return None; // 表没闭合 = 文件被写坏，按"没声明"折回缺省
                    };
                    buf = next.to_string();
                }
            }
        }
    }
    None
}

/// 解析 Lua 表的 options 数组为 (value, label) 列表。
fn parse_options(t: &mlua::Table) -> mlua::Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    if let Ok(opts) = t.get::<mlua::Table>("options") {
        for (_, o) in opts.pairs::<mlua::Value, mlua::Table>().flatten() {
            out.push((
                o.get::<String>("value").unwrap_or_default(),
                o.get::<String>("label").unwrap_or_default(),
            ));
        }
    }
    Ok(out)
}

/// 内嵌控件解析失败：跳过它，但要说清为什么（与顶层那条 `跳过无法解析的控件` 同一口径）。
///
/// 顶层三年前踩过一次"静默丢掉"，内嵌那两处（`row.children` 与 `modal_form.widgets`）
/// 当时没跟着改：插件作者把上一行的 row 塞进新 row、或子控件的 `type` 打错一个字母，
/// 界面上就是少一块，而插件行不报红、日志里一个字都没有。
fn skip_nested_widget(where_: &str, e: mlua::Error) {
    tracing::warn!("跳过无法解析的{where_}内嵌控件: {e}");
}

/// 弹窗缺 `id` 时按位置派生一个**稳定**的 id。
///
/// 前端把"哪个弹窗是开着的"记在 `openModals` 这把集合里，键就是元素的 DOM id；
/// `id` 为空时它退到 `"pf-modal-" + Math.random()` —— 于是每 2 秒一次的插件数据推送
/// 都会重画出一个新 id，正在填的弹窗下一次推送就自己关掉了（用户看到的是"填了一半没了"）。
/// 派生值只依赖控件在列表里的位置，同一份视图每轮渲染都一样的 ⇒ 稳定。
fn modal_id(raw: mlua::Result<String>, auto_id: &str) -> String {
    let raw = raw.unwrap_or_default();
    if raw.trim().is_empty() {
        format!("pf-modal-{auto_id}")
    } else {
        raw
    }
}

/// 解析 Lua 表为声明式控件。
///
/// `auto_id` 是这一格在列表里的位置串（顶层 `w0`、弹窗内嵌 `w0m1`、行内 `w0r2`），
/// 只给"没写 id 的弹窗"派生稳定 id 用（见 [`modal_id`]）。
fn parse_widget(
    w: &mlua::Table,
    depth: u32,
    auto_id: &str,
) -> mlua::Result<crate::plugins::Widget> {
    // 深度闸：`modal_form.widgets` 与 `row.children` 是递归下降解析的，而插件表是
    // **外部数据**。没有上限时一句 `local w={type="row",children={}}; w.children[1]=w`
    // 就成了自引用 → 递归无界 → 打穿线程栈。栈溢出是 SIGSEGV：不走 panic hook
    // （那场给 hook 加 flush 也救不了这条），release 又是 abort + 无控制台，
    // 用户看到的是"双击没反应/开着开着就没了"，连一行死因都没有。
    // 无环但要撞上限也只需要几千层，插件作者手滑（把上一行的 row 塞进新 row）即可命中。
    if depth > MAX_WIDGET_DEPTH {
        return Err(mlua::Error::RuntimeError(format!(
            "控件嵌套超过 {MAX_WIDGET_DEPTH} 层，已停止解析该控件（疑似自引用的控件表）"
        )));
    }
    let wtype: String = w.get("type")?;
    use crate::plugins::{FormField, Widget, MAX_CHART_POINTS};
    match wtype.as_str() {
        "label" => Ok(Widget::Label(w.get("text").unwrap_or_default())),
        "heading" => Ok(Widget::Heading(w.get("text").unwrap_or_default())),
        "keyvalue" => Ok(Widget::KeyValue(
            w.get("key").unwrap_or_default(),
            w.get("value").unwrap_or_default(),
        )),
        "separator" => Ok(Widget::Separator),
        "textarea" => Ok(Widget::TextArea(w.get("text").unwrap_or_default())),
        "textinput" => Ok(Widget::TextInput {
            field: w.get("field").unwrap_or_default(),
            label: w
                .get("label")
                .or_else(|_| w.get("text"))
                .unwrap_or_default(),
            value: w.get("value").unwrap_or_default(),
        }),
        "button" => Ok(Widget::Button {
            id: w.get("id").unwrap_or_default(),
            text: w.get("text").unwrap_or_default(),
            disabled: w.get("disabled").unwrap_or(false),
            modal: w.get::<Option<String>>("modal").unwrap_or(None),
            sel: w.get("sel").unwrap_or(false),
            group: w.get("group").unwrap_or_default(),
        }),
        "select" => Ok(Widget::Select {
            field: w.get("field").unwrap_or_default(),
            label: w
                .get("label")
                .or_else(|_| w.get("text"))
                .unwrap_or_default(),
            value: w.get("value").unwrap_or_default(),
            options: parse_options(w)?,
            refresh: w.get("refresh").unwrap_or(false),
        }),
        "modal_form" => {
            let mut fields = Vec::new();
            if let Ok(fields_val) = w.get::<mlua::Table>("fields") {
                for (_, f) in fields_val.pairs::<mlua::Value, mlua::Table>().flatten() {
                    fields.push(FormField {
                        kind: f.get("kind").unwrap_or_else(|_| "text".into()),
                        field: f.get("field").unwrap_or_default(),
                        label: f.get("label").unwrap_or_default(),
                        value: f.get("value").unwrap_or_default(),
                        options: parse_options(&f)?,
                        refresh: f.get("refresh").unwrap_or(false),
                    });
                }
            }
            // 弹窗内自定义操作按钮（(动作 id, 文字)）
            let mut buttons = Vec::new();
            if let Ok(btns_val) = w.get::<mlua::Table>("buttons") {
                for (_, b) in btns_val.pairs::<mlua::Value, mlua::Table>().flatten() {
                    buttons.push((
                        b.get::<String>("id").unwrap_or_default(),
                        b.get::<String>("text").unwrap_or_default(),
                    ));
                }
            }
            // 弹窗主体内嵌控件（表格等）
            let mut widgets = Vec::new();
            if let Ok(ws_val) = w.get::<mlua::Table>("widgets") {
                for (i, (_, c)) in ws_val
                    .pairs::<mlua::Value, mlua::Table>()
                    .flatten()
                    .enumerate()
                {
                    match parse_widget(&c, depth + 1, &format!("{auto_id}m{i}")) {
                        Ok(widget) => widgets.push(widget),
                        Err(e) => skip_nested_widget("弹窗", e),
                    }
                }
            }
            Ok(Widget::ModalForm {
                id: modal_id(w.get("id"), auto_id),
                title: w.get("title").unwrap_or_default(),
                submit: w.get("submit").unwrap_or_default(),
                submit_text: w.get("submit_text").unwrap_or_default(),
                cancel: w.get("cancel").unwrap_or_default(),
                content: w.get("content").unwrap_or_default(),
                open: w.get("open").unwrap_or(false),
                fields,
                buttons,
                widgets,
            })
        }
        "row" => {
            let mut children = Vec::new();
            if let Ok(children_val) = w.get::<mlua::Table>("children") {
                for (i, (_, c)) in children_val
                    .pairs::<mlua::Value, mlua::Table>()
                    .flatten()
                    .enumerate()
                {
                    match parse_widget(&c, depth + 1, &format!("{auto_id}r{i}")) {
                        Ok(widget) => children.push(widget),
                        Err(e) => skip_nested_widget("行", e),
                    }
                }
            }
            Ok(Widget::Row { children })
        }
        "pager" => Ok(Widget::Pager {
            page: w.get("page").unwrap_or(1),
            pages: w.get("pages").unwrap_or(1),
            total: w.get("total").unwrap_or(0),
            prev_id: w.get("prev").unwrap_or_default(),
            next_id: w.get("next").unwrap_or_default(),
        }),
        "table" => {
            let headers: Vec<String> = w.get("headers").unwrap_or_default();
            let mut rows = Vec::new();
            if let Ok(rows_val) = w.get::<mlua::Table>("rows") {
                for (_, row) in rows_val.pairs::<mlua::Value, mlua::Table>().flatten() {
                    let mut cols = Vec::new();
                    for (_, v) in row.pairs::<mlua::Value, mlua::Value>().flatten() {
                        if let Ok(s) = v.to_string() {
                            cols.push(s);
                        }
                    }
                    rows.push(cols);
                }
            }
            let ids: Vec<String> = w
                .get::<Vec<mlua::Value>>("ids")
                .unwrap_or_default()
                .iter()
                .map(|v| match v {
                    mlua::Value::Integer(n) => n.to_string(),
                    mlua::Value::String(s) => s.to_string_lossy().to_string(),
                    mlua::Value::Number(f) => (*f as i64).to_string(),
                    _ => v.to_string().unwrap_or_default(),
                })
                .collect();
            let mut actions = Vec::new();
            if let Ok(actions_val) = w.get::<mlua::Table>("actions") {
                for (_, a) in actions_val.pairs::<mlua::Value, mlua::Table>().flatten() {
                    actions.push((
                        a.get::<String>("prefix").unwrap_or_default(),
                        a.get::<String>("text").unwrap_or_default(),
                    ));
                }
            }
            Ok(Widget::Table {
                headers,
                rows,
                ids,
                actions,
                group: w.get("group").unwrap_or_default(),
                onselect: w.get("onselect").unwrap_or_default(),
            })
        }
        "chart" => {
            let mut labels = lua_text_list(w, "labels");
            let (mut points, dropped) = lua_number_list(w, "points");
            if dropped > 0 && notice_once(&format!("chart-drop:{auto_id}:{dropped}")) {
                tracing::warn!(
                    "图表控件（{auto_id}）里有 {dropped} 个值不是数，已跳过、其余照常画 —— \
                     points 要给一串数字（{MAX_CHART_POINTS} 个以内）"
                );
            }
            // 上限挂在解析这一头，且夹的是**开头**：趋势图被夹掉的一定是最早那几个，
            // 留最近这段才是读者要看的那一段（夹尾部等于把最新数据藏起来）。
            if points.len() > MAX_CHART_POINTS {
                let over = points.len() - MAX_CHART_POINTS;
                points.drain(..over);
                labels.drain(..over.min(labels.len()));
                if notice_once(&format!("chart-cap:{auto_id}:{}", over + MAX_CHART_POINTS)) {
                    tracing::warn!(
                        "图表控件（{auto_id}）交了 {} 个点，只画最近 {MAX_CHART_POINTS} 个\
                         （夹掉最早的 {over} 个）：横轴范围请由插件自己收窄，别指望宿主画得下",
                        over + MAX_CHART_POINTS
                    );
                }
            }
            // 点数与刻度数对齐：谁的尾巴对谁的尾巴（夹过头部之后长度可能差几格）
            if labels.len() > points.len() {
                labels.drain(..labels.len() - points.len());
            }
            Ok(Widget::Chart {
                kind: w.get("kind").unwrap_or_default(),
                labels,
                points,
            })
        }
        _ => Ok(Widget::Label(format!("[未知控件: {wtype}]"))),
    }
}

/// 把 Lua 那一侧「一串文本」读成 `Vec<String>`：逐个元素自己转，**不按整表转**。
///
/// 整表 `w.get::<Vec<String>>("labels")` 在作者把横轴写成 `{1, 2, 3}`（日序号，很常见的写法）
/// 时整串转换失败，而这一族全都是 `.unwrap_or_default()` 的口径 —— 于是一格都不剩，
/// 症状是图画出来了、横轴空白，面板一句话不说。逐个转把"这一项不是文本"只算在它自己头上：
/// 数字变成自己的十进制文本，其它项照常。
fn lua_text_list(w: &mlua::Table, key: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(list) = w.get::<mlua::Table>(key) else {
        return out;
    };
    for (_, v) in list.pairs::<mlua::Value, mlua::Value>().flatten() {
        out.push(match v {
            mlua::Value::String(s) => s.to_string_lossy().to_string(),
            mlua::Value::Integer(n) => n.to_string(),
            mlua::Value::Number(f) => f.to_string(),
            other => other.to_string().unwrap_or_default(),
        });
    }
    out
}

/// 把 Lua 那一侧「一串数值」读成 `Vec<i64>`，并交回**丢掉了几个不是数的元素**。
///
/// 与 [`lua_text_list`] 同一理由：整表转 `Vec<i64>` 会因为其中一个 `"12 万"`（作者从
/// 展示文本里抄来的）而一个都不剩，画出来是一张「暂无数据」的空图，而盘上明明有值。
/// 布尔/nil/表 这些是真的当不成数，丢掉并交回个数，由调用方出声。
fn lua_number_list(w: &mlua::Table, key: &str) -> (Vec<i64>, usize) {
    let mut out = Vec::new();
    let mut dropped = 0usize;
    let Ok(list) = w.get::<mlua::Table>(key) else {
        return (out, dropped);
    };
    for (_, v) in list.pairs::<mlua::Value, mlua::Value>().flatten() {
        let n = match v {
            mlua::Value::Integer(i) => Some(i),
            // 小数按既有那把尺子截断（表格读数把 Number 也 `as i64`），不另发明四舍五入
            mlua::Value::Number(f) => Some(f as i64),
            mlua::Value::String(s) => s.to_string_lossy().trim().parse::<i64>().ok(),
            _ => None,
        };
        match n {
            Some(v) => out.push(v),
            None => dropped += 1,
        }
    }
    (out, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 往 `plugins/` 里放一个插件文件（路径按 `discover()` 的口径来）。
    fn write_lua(dir: &Path, file: &str, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(format!("{file}.lua"));
        std::fs::write(&p, body).unwrap();
        p
    }

    /// 「要先授权才能加载、要先加载才能授权」那条死锁（2026-11-04 的 GUI 屏证逮到的）。
    ///
    /// API 2 的插件若在 `init()` 里就调权限 API（脚手架正是这样），未授权会让 init 抛
    /// `DENIED`、加载失败；而「授权」原先要求"已加载" —— 用户永远点不动那颗按钮。
    /// 这一条钉两半：未加载时授权要成功；授权之后同一份文件要装得起来。
    #[test]
    fn a_plugin_that_fails_init_for_want_of_a_grant_can_still_be_granted() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("grant_before_load");
        let script = "PLUGIN_NAME = \"needs_grant\"\nPLUGIN_API = 2\n\
                      PLUGIN_PERMISSIONS = { \"storage.read\" }\n\
                      function init() local v = focusflow.store.get(\"k\") end\n\
                      function get_view() return { widgets = {} } end\n";
        let path = write_lua(&paths::plugins_user_dir(), "needs_grant", script);
        let mut pm = manager_in(tmp.path());

        let err = pm
            .load_plugin(&path, PluginSource::User)
            .expect_err("未授权时 init 会撞 DENIED，加载应当失败");
        assert!(
            err.contains("DENIED") || err.contains("storage.read"),
            "失败原因要指向权限：{err}"
        );
        assert!(pm.get_plugin("needs_grant").is_none(), "它不该在实例表里");

        let said = pm
            .grant_plugin("needs_grant")
            .expect("没加载的那一档也必须能授权（否则那颗按钮永远点不动）");
        assert!(said.contains("已授权"), "{said}");

        pm.load_plugin(&path, PluginSource::User)
            .expect("授权之后同一份文件要装得起来");
    }

    /// 一个用到调度线程、并按 `host.rs` 的说明在 cleanup 里归还的插件。
    fn scheduler_user(name: &str) -> String {
        format!(
            r###"
PLUGIN_NAME = "{name}"
function init() focusflow.scheduler_tasks() end
function cleanup() focusflow.scheduler_shutdown() end
function get_view() return {{ title = "{name}", widgets = {{ {{type="label", text="ok"}} }} }} end
"###
        )
    }

    /// 一个有视图、别的什么都不干的插件。
    fn view_only(name: &str) -> String {
        format!(
            "PLUGIN_NAME = \"{name}\"\nfunction get_view() return {{ title = \"{name}\", \
             widgets = {{ {{type=\"label\", text=\"v\"}} }} }} end\n"
        )
    }

    /// 每个用例一套**自己的** config：`config::instance()` 是另一个进程级全局，
    /// 一旦被哪个用例初始化，后面任何一次 `config.set` 都会把盘写到它那个已经删掉的
    /// 临时目录里（`%TEMP%` 就是这么堆起来的）。这里只 `load`，且用例一律不碰 `set`。
    fn manager_in(tmp: &Path) -> PluginManager {
        db::queries::invalidate_years_cache();
        let config: &'static FocusFlowConfig = Box::leak(Box::new(
            FocusFlowConfig::load(tmp.join("config.ini")).expect("临时配置应能载入"),
        ));
        manager_in_with(config, tmp)
    }

    /// 同上，但配置由调用方给（回滚开关那条用例要在载入之后再改口一次）。
    fn manager_in_with(config: &'static FocusFlowConfig, _tmp: &Path) -> PluginManager {
        PluginManager::new(config, db::Database::init_readonly())
    }

    /// 展示名固定、只换 `get_view` 标题的插件：条目在表里的键不变，
    /// 断言就不必去管 `load_plugin` 的改名退休逻辑。
    fn view_with_name(display: &str, title: &str) -> String {
        format!(
            "PLUGIN_NAME = \"{display}\"\nfunction get_view() return {{ title = \"{title}\", \
             widgets = {{ {{type=\"label\", text=\"v\"}} }} }} end\n"
        )
    }

    /// 「插件已启用/停用」这句话要有盘上的事实兜着，而不是只看着内存里改了。
    ///
    /// `config::set` 恒回 `Ok(())`（真落盘的是另一头的去抖线程），所以从前 `set_enabled`
    /// 里那句 `if let Err(e) = self.config.set(...)` 是**死路**，调用方 `toggle_plugin`
    /// 把 `Ok` 一律念成「插件 X 已停用」：屏上说完，重启之后那条又活了。
    /// 现在当场落盘并问一次结局，落不成就交回 Err，并把"只到本次运行"说清楚。
    ///
    /// 两条腿各钉一边：干净那条钉**成功必须真的写进文件**（只断"不报错"盯不住
    /// "回 Ok 而盘上没动"那一半）；障碍那条钉失败必须回 Err 且点名是哪一样。
    #[test]
    fn a_plugin_toggle_that_cannot_persist_says_so_instead_of_claiming_success() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("plugin_toggle_persist");
        let plugins = tmp.path().join("plugins");
        let alpha = write_lua(&plugins, "alpha", &view_only("甲"));
        let mut pm = manager_in(tmp.path());
        pm.load_plugin(&alpha, PluginSource::Builtin)
            .expect("alpha 该能加载");

        let path = tmp.path().join("config.ini");
        pm.set_enabled("alpha", false)
            .expect("正常情况下停用该成功");
        let on_disk = std::fs::read_to_string(&path).expect("配置文件该在");
        assert!(
            on_disk.contains("disabled") && on_disk.contains("alpha"),
            "屏上说了已停用，文件里却没有它: {on_disk}"
        );

        // 夹具：把 `config.ini` 的位置换成**同名目录** ⇒ 那次 rename 必然失败
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir_all(&path).unwrap();
        let err = pm
            .set_enabled("alpha", true)
            .expect_err("写不进文件就不许回 Ok（回 Ok 屏上就是「已启用」）");
        assert!(
            err.contains("alpha") && err.contains("没写进 config.ini") && err.contains("重启"),
            "失败要说清是哪一样、为什么、以及只到本次运行，实得: {err}"
        );
        std::fs::remove_dir_all(&path).unwrap();
    }

    /// 热重载的键是**文件名**，所以先按文件匹配、再按展示名。
    ///
    /// 回归：`plugins` 以展示名为键，旧顺序先 `get(key)`。文件 `beta.lua` 变了 ⇒
    /// key="beta"，而 `alpha.lua` 的 `PLUGIN_NAME` 正好叫「beta」⇒ 命中的是 alpha：
    /// **没改的那个被重载、改过的那个继续跑旧代码**，日志还写着"插件已热重载: beta"
    /// —— 用户看到的就是"存盘没生效"。
    #[test]
    fn a_reload_key_prefers_the_changed_file_over_a_same_named_display_name() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("reload_key_prefers_stem");
        let plugins = tmp.path().join("plugins");
        let alpha = write_lua(&plugins, "alpha", &view_with_name("beta", "alpha-旧"));
        let beta = write_lua(&plugins, "beta", &view_with_name("真正的乙", "beta-旧"));
        let mut pm = manager_in(tmp.path());
        pm.load_plugin(&alpha, PluginSource::Builtin)
            .expect("alpha 该能加载");
        pm.load_plugin(&beta, PluginSource::Builtin)
            .expect("beta 该能加载");

        // 只有文件 beta.lua 变了（展示名不变，只换标题）
        write_lua(&plugins, "beta", &view_with_name("真正的乙", "beta-新"));
        assert!(
            pm.reload_plugin("beta"),
            "键 beta 既是 beta.lua 的文件名、又是 alpha.lua 的展示名，总得命中一个"
        );

        let reloaded = pm.get_plugin("真正的乙").expect("beta.lua 的条目还在");
        assert_eq!(
            reloaded.view.as_ref().map(|v| v.title.as_str()),
            Some("beta-新"),
            "改了的那个文件必须换上新代码（旧顺序在这里仍然跑 beta-旧）"
        );
        let untouched = pm.get_plugin("beta").expect("alpha.lua 的条目还在");
        assert_eq!(
            untouched.view.as_ref().map(|v| v.title.as_str()),
            Some("alpha-旧"),
            "没改过的 alpha 不该因为展示名撞上就被顶替"
        );
        assert_eq!(untouched.file_path, alpha, "键 beta 指向的仍是 alpha.lua");
    }

    // ---------- 第八节 A7：热重载失败不许把还能用的插件弄丢 ----------

    /// 把一个还能用的插件文件写成半截语法错误（编辑器自动保存、网盘同步到一半都会这样）：
    /// 重载必须"什么都没发生"，而不是旧实例已被摘除、新版本又装不回来。
    ///
    /// 旧实现是 `plugins.remove(n)` 之后才 `try_load`，于是那一刻：插件从列表里消失、
    /// 正打开的详情页变成「插件未提供视图」，而它记在 `claim_scheduler` 上的 owner
    /// 成了无主孤儿 —— 唯一的释放入口是插件 Lua 里的 `scheduler_shutdown()`，
    /// 而那份 Lua 状态已经跟着条目一起没了 → 本进程的调度线程再也还不回来。
    #[test]
    fn a_failed_reload_leaves_the_old_plugin_exactly_where_it_was() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("reload_keeps_old");
        let plugins = tmp.path().join("plugins");
        let p = write_lua(&plugins, "keep_old", &scheduler_user("重载别丢我"));
        let mut pm = manager_in(tmp.path());
        pm.load_plugin(&p, PluginSource::Builtin)
            .expect("好版本该能加载");
        assert_eq!(
            host::scheduler_state_for_test(),
            Some(1),
            "init() 用过调度线程就该把这个文件名登记成使用者"
        );

        // 半截的语法错误
        write_lua(
            &plugins,
            "keep_old",
            "PLUGIN_NAME = \"重载别丢我\"\nfunction init() 这行不是 Lua ((( \n",
        );
        assert!(
            !pm.reload_plugin("keep_old"),
            "坏版本不能让重载报成功（否则调用方以为换上了新的）"
        );

        // 旧实例还在表里，而且照常能渲染（详情页不该变成「插件未提供视图」）
        let info = pm
            .get_plugin("重载别丢我")
            .expect("旧实例必须还挂着：摘掉它的那一步只能在新版本装好之后发生");
        assert!(info.loaded, "旧实例不该被顺手标成未加载");
        assert_eq!(info.file_path, p);
        pm.refresh_view("重载别丢我")
            .expect("旧实例的 get_view 该照常能跑");
        assert!(pm.get_plugin("重载别丢我").unwrap().view.is_some());
        // owner 记账一个字都不许动 —— 这份账现在是旧实例的，不是孤儿的
        assert_eq!(
            host::scheduler_state_for_test(),
            Some(1),
            "加载失败时把旧实例的 owner 销掉，等于把定时任务的线程拆了"
        );

        // 反向腿：旧实例活着 ⇒ 停用就能回收。旧实现走到这里条目已经没了，
        // `unload_plugin` 返回 false、owner 永远留在表上（= 上面那句"再也回收不掉"）。
        assert!(pm.unload_plugin("重载别丢我"), "卸载应当成功");
        assert_eq!(
            host::scheduler_state_for_test(),
            None,
            "最后一个用户走了就该回收"
        );
    }

    /// 撞名闸按**源文件**判重之后，两个方向都要钉住：
    /// - 改了名的重载要真能换上来，并且把旧名字那条一起带走（不然同一个文件两份实例，
    ///   旧那份再没有 unload 入口、cleanup 永不执行）；
    /// - 撞上**别的文件**占用的名字仍然要拒，而且被拒的那次不许动旧实例。
    #[test]
    fn a_renamed_reload_retires_the_old_entry_and_still_respects_the_gate() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("reload_rename");
        let plugins = tmp.path().join("plugins");
        let p = write_lua(&plugins, "ren", &view_only("旧名"));
        let mut pm = manager_in(tmp.path());
        pm.load_plugin(&p, PluginSource::Builtin).expect("加载失败");

        write_lua(&plugins, "ren", &view_only("新名"));
        assert!(pm.reload_plugin("ren"), "改名重载应当成功");
        assert!(pm.get_plugin("新名").is_some(), "新名字该进表");
        assert!(
            pm.get_plugin("旧名").is_none(),
            "insert 顶不掉改了名的旧条目，必须显式退休，否则留一份僵尸"
        );
        assert_eq!(
            pm.get_all_plugins().len(),
            1,
            "同一个文件不该同时有两份实例"
        );

        let other = write_lua(&plugins, "neighbour", &view_only("邻居"));
        pm.load_plugin(&other, PluginSource::Builtin)
            .expect("加载邻居失败");
        write_lua(&plugins, "ren", &view_only("邻居"));
        assert!(!pm.reload_plugin("ren"), "抢了别人的名字要被拒");
        assert!(
            pm.get_plugin("新名").is_some(),
            "被拒的那次重载不许把旧实例一起带走"
        );
        assert_eq!(pm.get_all_plugins().len(), 2);
    }

    /// `init()` 跑一半才失败（首次启用/启动补载这条路上没有"旧实例"可留）：
    /// 那次尝试在调度线程上登记的 owner 也要跟着回滚，不然同样留下一个本进程
    /// 再也回收不掉的孤儿 —— 而 `release_scheduler` 只有 Lua 侧一个调用点。
    #[test]
    fn a_failed_first_load_does_not_leave_an_orphan_scheduler_owner() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("failed_load_owner");
        let plugins = tmp.path().join("plugins");
        write_lua(
            &plugins,
            "orphan",
            "PLUGIN_NAME = \"跑一半就坏\"\nfunction init() focusflow.scheduler_tasks() \
             error(\"下一行才坏\") end\n",
        );
        let mut pm = manager_in(tmp.path());
        pm.load_all();
        assert!(
            pm.get_plugin("跑一半就坏").is_none(),
            "加载失败的插件不该在表里"
        );
        assert_eq!(
            host::scheduler_state_for_test(),
            None,
            "这次尝试登记下的 owner 必须销掉：表里没有它的插件，就再没人能归还"
        );
    }

    /// 回归（第九扫 A1）：**超限停用**把调度线程的 owner 变成永久孤儿。
    ///
    /// `mark_plugin_error` 原来只置 `info.lua = None` 就完事 —— 而 Lua 侧唯一的归还
    /// 入口（`scheduler_shutdown()`）跟着那个状态一起消失，`unload_plugin` 也只在还有
    /// lua 实例时才跑 cleanup。后果不是占点内存：插件页显示这个面板"已停用"，
    /// `Scheduler` 线程却为整个进程续命，它登记的定时任务照旧到点启动程序
    /// （`scheduler_plugin.lua` 的注释自己就写着"不叫这句，插件停用后定时任务仍会照常触发"）。
    ///
    /// 夹具：`init` 里先 claim（`scheduler_tasks`），`get_view` 里跑一条有界大循环，
    /// 临时 `config.ini` 把 `instruction_limit` 压到 3000 ⇒ 渲染必然超限。
    #[test]
    fn an_over_limit_disable_does_not_orphan_the_scheduler_owner() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("limit_owner");
        std::fs::write(
            tmp.path().join("config.ini"),
            "[plugins]\ninstruction_limit = 3000\n",
        )
        .unwrap();
        write_lua(
            // 夹具落在**内置那一格**：它要用 `scheduler_tasks`，而那一组从 3.4 起只对内置模块开放。
            // 落在旧的平铺格会被当成用户插件迁进数据目录，于是 `init()` 第一句就取到 nil ——
            // 红的是这条前提，而不是被测的归还逻辑。
            &tmp.path().join("plugins").join("builtin"),
            "hog",
            r###"
PLUGIN_NAME = "超限甲"
function init() focusflow.scheduler_tasks() end
function cleanup() focusflow.scheduler_shutdown() end
function get_view()
  local n = 0
  for i = 1, 200000 do n = n + i end
  return { title = "超限甲", widgets = { {type="label", text=tostring(n)} } }
end
"###,
        );
        let mut pm = manager_in(tmp.path());
        pm.load_all();
        // 调度线程槽是**进程全局**的，同二进制里并行跑的用例也可能各占一个 owner
        // ⇒ 前提只看"至少登记了一个"，归还之后才断精确的 None（那才是要钉的东西）。
        assert!(
            host::scheduler_state_for_test()
                .map(|n| n >= 1)
                .unwrap_or(false),
            "前提：init 里那句 scheduler_tasks 该把 owner 登记上"
        );

        let r = pm.refresh_view("超限甲");
        assert!(r.is_err(), "超限那次渲染该报错，实得 {r:?}");
        assert!(
            pm.get_plugin("超限甲")
                .map(|p| !p.loaded && p.error.is_some())
                .unwrap_or(false),
            "插件必须被停用（表里留着错误信息）"
        );
        assert_eq!(
            host::scheduler_state_for_test(),
            None,
            "停用必须同时归还 owner：Lua 状态一没，就再没有别的归还入口了"
        );

        // 反向腿：正常停用（没超限的那条路）仍然要能回收
        pm.unload_plugin("超限甲");
        assert_eq!(
            host::scheduler_state_for_test(),
            None,
            "对一个已停用条目再卸载不该 panic，也不该把 owner 留下"
        );
    }

    /// 停用一条**从没打算归还**的插件，owner 也得从调度线程表里摘掉。
    ///
    /// `unload_plugin` 里归还动作一个都没有：它只跑 Lua 的 `cleanup()`，而 `cleanup` 是
    /// 插件自愿写的（`Ok(None)`＝没写，是合法形态，`host.rs` 的注释还专门叮嘱不许为此出声）。
    /// 于是"用日程 API 但没写 cleanup"这一整类插件停用之后，owner 永远留在
    /// `SCHEDULER.users` 里：线程为它续命、它登记的定时任务照旧到点启动程序，而插件页
    /// 写着已停用 —— Lua 侧那个归还入口跟着 `info.lua = None` 一起消失了，本进程再没有
    /// 第二条路。这与**超限停用**那条是同一类（`mark_plugin_error` 已经补过它那一份），
    /// 漏的是正常停用这一条。
    ///
    /// 反向腿钉住"兜底"两字的边界：只摘自己那个 stem，别家的用户不许被一起带走
    /// （`host.rs` 顶部写的就是无条件 `scheduler_shutdown` 会把内置定时任务压掉的旧事故），
    /// 而已经自己归还过一次的插件，第二次归还必须是空操作（`users` 是集合）。
    #[test]
    fn unloading_a_plugin_that_never_releases_still_gives_the_scheduler_back() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("unload_owner");
        // 两份夹具都要用 `scheduler_tasks` ⇒ 落在内置那一格（用户格子从 3.4 起看不见那一组）
        let plugins = tmp.path().join("plugins").join("builtin");
        write_lua(
            &plugins,
            "quiet",
            "PLUGIN_NAME = \"不归还\"\nfunction init() focusflow.scheduler_tasks() end\n",
        );
        write_lua(
            &plugins,
            "polite",
            "PLUGIN_NAME = \"自己归还\"\nfunction init() focusflow.scheduler_tasks() end\n\
             function cleanup() focusflow.scheduler_shutdown() end\n",
        );
        let mut pm = manager_in(tmp.path());
        pm.load_all();

        assert!(
            host::scheduler_has_owner_for_test("quiet"),
            "前提：init 里那句 scheduler_tasks 该把 quiet 登记进去"
        );
        assert!(
            host::scheduler_has_owner_for_test("polite"),
            "前提：同一条路上 polite 也登记了"
        );

        // 先卸会自己归还的那个：它的第二次归还（兜底那行）必须是空操作，
        // 且不许顺手把邻居的 owner 带走。
        assert!(pm.unload_plugin("自己归还"), "卸载应当成功");
        assert!(
            !host::scheduler_has_owner_for_test("polite"),
            "自己归还过的插件，卸载后 owner 该没了"
        );
        assert!(
            host::scheduler_has_owner_for_test("quiet"),
            "兜底归还只摘自己那个 stem：邻居的用户数不许被动到（动了就是旧事故的镜像）"
        );

        // 正题：没写 cleanup 的那一条，卸载必须替它把 owner 摘掉。
        assert!(pm.unload_plugin("不归还"), "卸载应当成功");
        assert!(
            !host::scheduler_has_owner_for_test("quiet"),
            "插件都不在表里了，owner 却还占着调度线程 ⇒ 定时任务照旧触发，且再无归还入口"
        );
    }

    // ---------- 第八节 A8：元数据扫描只认真正的顶层赋值 ----------

    /// `--[[ ]]` 里被注释掉的旧值、函数体里缩进的同名赋值、`local PLUGIN_NAME`、
    /// 顶层长字符串里的那一行，都不许赢过真声明。
    ///
    /// 扫错的代价写在 `scan_meta` 上：插件管理页的「打开」按钮传的就是这里扫出来的
    /// 名字（原生端同样是这一份名字，见 `desktop-native/src/main.rs` 的 `open_plugin_view`），
    /// 名字对不上就是点一次
    /// 报一次「插件未提供视图」；旧实现"取第一条匹配行"，只要注释那行带缩进就先命中。
    #[test]
    fn scan_meta_only_takes_top_level_assignments() {
        let path = Path::new("whatever.lua");
        let name_of = |src: &str| PluginManager::scan_meta_source(src, path).name;

        // 块注释里的假名正好顶在第 0 列 —— 旧实现就是它赢
        assert_eq!(
            name_of("--[[\nPLUGIN_NAME = \"注释掉的旧名\"\n--]]\nPLUGIN_NAME = \"真名\"\n",),
            "真名",
            "块注释里的行不能算声明"
        );
        // 同行块的 `--[[ … ]]`、以及 `--[==[ … ]==]` 这种带等号的形状
        assert_eq!(
            name_of("--[[ PLUGIN_NAME = \"同行的假名\" ]] PLUGIN_NAME = \"真名2\"\n"),
            "真名2"
        );
        assert_eq!(
            name_of("--[==[\nPLUGIN_NAME = \"等号块里的\"\n]==]\nPLUGIN_NAME = \"真名3\"\n"),
            "真名3"
        );
        // 函数体里缩进的同名赋值（那是个局部变量，加载后 globals 里根本没有它）
        assert_eq!(
            name_of(
                "function init()\n  PLUGIN_NAME = \"函数体里的\"\nend\nPLUGIN_NAME = \"顶层的\"\n",
            ),
            "顶层的",
            "带缩进的说明它写在某个块里"
        );
        // `local` 的同样不会成为全局变量
        assert_eq!(
            name_of("local PLUGIN_NAME = \"局部的\"\nPLUGIN_NAME = \"全局的\"\n"),
            "全局的"
        );
        // 顶层长字符串里的那一行是数据
        assert_eq!(
            name_of("local hint = [[\nPLUGIN_NAME = \"串里的\"\n]]\nPLUGIN_NAME = \"真名4\"\n"),
            "真名4"
        );
        // 行注释（旧实现靠"行首不是 --"侥幸挡住，现在由同一套规则挡住）
        assert_eq!(
            name_of("-- PLUGIN_NAME = \"行注释里的\"\nPLUGIN_NAME = \"真名5\"\n"),
            "真名5"
        );
    }

    /// 正常形状照旧扫得出来（含"简介里带 `--`"这种不能被注释规则吃掉的形状），
    /// 什么都没有时回退成文件名 —— 这两条不是新行为，是防上面那套规则改过头的网。
    #[test]
    fn scan_meta_still_reads_normal_fields() {
        let path = Path::new("whatever.lua");
        let scanned = PluginManager::scan_meta_source(
            "PLUGIN_NAME = '单引号也行'\nPLUGIN_DESC = \"含 -- 破折号的简介\"\n\
             PLUGIN_VERSION = \"1.2.3\"\nPLUGIN_AUTHOR = \"某人\"\n",
            path,
        );
        assert_eq!(scanned.name, "单引号也行");
        assert_eq!(scanned.desc, "含 -- 破折号的简介", "引号里的 -- 不是注释");
        assert_eq!(scanned.version, "1.2.3");
        assert_eq!(scanned.author, "某人");

        let scanned = PluginManager::scan_meta_source("function init() end\n", path);
        assert_eq!(scanned.name, "whatever", "扫不到就回退成文件名");
        assert_eq!(scanned.desc, "");
        assert_eq!(scanned.version, "1.0");
        assert_eq!(scanned.author, "");
    }

    /// 新加的 5 个声明字段扫得出来，而且**不靠执行代码**（方案 3.3）。
    ///
    /// 这条不是"能解析就行"的样板用例：`PLUGIN_PERMISSIONS = { … }` 与两个整数声明
    /// 都不是原来那把尺子（只认 `KEY = "字符串"`）覆盖的形状 —— 少一项的症状是插件页
    /// 那一格永远空着，而作者明明写了。数组还要检查**排序去重**：将来授权哈希绑的
    /// 就是这一份，两处顺序不同会让同一个权限集合算出两个哈希。
    #[test]
    fn scan_reads_the_five_new_manifest_fields() {
        let path = Path::new("whatever.lua");
        let scanned = PluginManager::scan_meta_source(
            "PLUGIN_NAME = \"声明齐全\"\n\
             PLUGIN_API = 2\n\
             PLUGIN_PERMISSIONS = { \"stats.read\", \"storage.write\", \"stats.read\" }\n\
             PLUGIN_REFRESH_MS = 1500\n\
             PLUGIN_LIFECYCLE = \"always\"\n\
             PLUGIN_MIN_APP = \"1.6.0\"\n\
             function init() error(\"扫这一轮绝不该跑到这里\") end\n",
            path,
        );
        assert_eq!(scanned.api, 2, "PLUGIN_API 扫不出来");
        assert_eq!(
            scanned.permissions,
            vec!["stats.read".to_string(), "storage.write".to_string()],
            "权限清单要排序 + 去重（授权哈希将来就绑这一份）"
        );
        assert_eq!(scanned.refresh_ms, 1500, "PLUGIN_REFRESH_MS 扫不出来");
        assert_eq!(
            scanned.lifecycle,
            PluginLifecycle::Always,
            "PLUGIN_LIFECYCLE = \"always\" 认不出来"
        );
        assert_eq!(scanned.min_app, "1.6.0", "PLUGIN_MIN_APP 扫不出来");
        // 名字仍照旧（这一句是防"新规则把老的 4 项吃掉了"）
        assert_eq!(scanned.name, "声明齐全");
    }

    /// 老插件什么都不写 ⇒ 五项全按缺省（v1 的 5 个随包脚本一字不改也要有这份清单）。
    #[test]
    fn absent_manifest_fields_fold_to_the_documented_defaults() {
        let path = Path::new("old.lua");
        let scanned = PluginManager::scan_meta_source("PLUGIN_NAME = \"老插件\"\n", path);
        assert_eq!(scanned.api, 1, "PLUGIN_API 的缺省是 1");
        assert!(scanned.permissions.is_empty(), "没声明就是零项");
        assert_eq!(scanned.refresh_ms, 0, "缺省只在动作后刷新");
        assert_eq!(
            scanned.lifecycle,
            PluginLifecycle::Panel,
            "PLUGIN_LIFECYCLE 的缺省是 panel"
        );
        assert_eq!(scanned.min_app, "", "缺省不限版本");
    }

    /// 扫描侧与加载侧必须交出**同一份清单**（含非法值的折叠）。
    ///
    /// 这是这一族缺陷的共用尺子：插件页那一行读扫描侧（不执行代码），加载之后的运行态
    /// 读加载侧。两边折得不一样，症状是"屏上写着常驻、跑起来是面板"这种最难查的一半 ——
    /// 所以三条臂都放在一起比，而不是各比各的：齐全那条钉五个字段都接得上，
    /// 非法那条钉折叠规则同源，缺省那条钉"没写"这个状态两侧一致。
    #[test]
    fn manifest_scan_and_load_agree_on_one_manifest() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("manifest_two_sides");
        let pm = manager_in(tmp.path());
        let cases = [
            (
                "齐全",
                "PLUGIN_NAME = \"齐全\"\nPLUGIN_API = 2\n\
                 PLUGIN_PERMISSIONS = {\n  \"timer\",\n  \"stats.read\",\n}\n\
                 PLUGIN_REFRESH_MS = 30000\nPLUGIN_LIFECYCLE = \"always\"\n\
                 PLUGIN_MIN_APP = \"1.6.0\"\n",
            ),
            (
                "非法",
                "PLUGIN_NAME = \"非法\"\nPLUGIN_API = 0\nPLUGIN_API_OVERRIDE = 7\n\
                 PLUGIN_REFRESH_MS = -5\nPLUGIN_LIFECYCLE = \"Always\"\n\
                 PLUGIN_MIN_APP = 123\n",
            ),
            ("缺省", "PLUGIN_NAME = \"缺省\"\nfunction init() end\n"),
        ];
        for (tag, src) in cases {
            let file = write_lua(&tmp.path().join("plugins").join("builtin"), tag, src);
            let scanned = PluginManager::scan_meta(&file).expect("扫描侧读得动");
            let loaded = pm
                .read_meta(&file)
                .unwrap_or_else(|e| panic!("加载侧（{tag}）跑不了：{e}"))
                .manifest;
            assert_eq!(
                scanned, loaded,
                "「{tag}」这一份两处口径不一致：{scanned:?} vs {loaded:?}"
            );
        }
    }

    /// 非法声明要出声一次，不许静默折回缺省。
    ///
    /// 折叠本身由上面那条盯等值；这条盯的是"作者能不能知道自己写错了"。
    /// `PLUGIN_LIFECYCLE = "Always"` 那一档差一个大写字母，静默折叠的症状是
    /// 「我明明写了常驻，插件页却写着面板」。
    #[test]
    fn an_illegal_declaration_says_so_instead_of_quietly_defaulting() {
        let logs = crate::logger::capture_logs(|| {
            PluginManager::scan_meta_source(
                "PLUGIN_LIFECYCLE = \"Always\"\nPLUGIN_API = 0\nPLUGIN_REFRESH_MS = -5\n",
                Path::new("noisy.lua"),
            );
        })
        .1;
        for want in ["PLUGIN_LIFECYCLE", "PLUGIN_API", "PLUGIN_REFRESH_MS"] {
            assert!(
                logs.iter().any(|l| l.contains(want)),
                "{want} 写坏了却没有一句说明（日志只有 {logs:?}）"
            );
        }
    }

    /// 权限表的两条边界：跨行要读得动，注释与函数体里的不算声明。
    ///
    /// 反面形状各钉一次：块注释里那份"以前用的权限清单"顶班 ⇒ 插件页显示成早已作废的
    /// 一组；函数体里的缩进表根本是局部变量，加载后 globals 里没有它 ⇒ 两侧不一致。
    #[test]
    fn the_permissions_table_is_only_read_at_top_level() {
        let path = Path::new("x.lua");
        // 跨行的那一行紧跟着 `}` ⇒ 仍要读全
        let multi = PluginManager::scan_meta_source(
            "PLUGIN_PERMISSIONS = {\n  \"timer\",\n  \"notify\",\n}\n",
            path,
        );
        assert_eq!(
            multi.permissions,
            vec!["notify".to_string(), "timer".to_string()],
            "跨行写的权限表要读得动（作者真会这么写）"
        );
        // 块注释里的旧清单不顶班
        let commented = PluginManager::scan_meta_source(
            "--[[\nPLUGIN_PERMISSIONS = { \"早就作废的那一组\" }\n--]]\n\
             PLUGIN_PERMISSIONS = { \"stats.read\" }\n",
            path,
        );
        assert_eq!(
            commented.permissions,
            vec!["stats.read".to_string()],
            "注释里那份是历史，不是声明"
        );
        // 函数体里缩进的那一份是局部变量，加载后 globals 里没有它
        let nested = PluginManager::scan_meta_source(
            "function init()\n  PLUGIN_PERMISSIONS = { \"函数体里的\" }\nend\n",
            path,
        );
        assert!(
            nested.permissions.is_empty(),
            "缩进的那一份不算声明: {:?}",
            nested.permissions
        );
        // 表没闭合（文件被写坏）⇒ 按"没声明"折回缺省，不许把剩下的整份文件当权限读
        let unclosed = PluginManager::scan_meta_source(
            "PLUGIN_PERMISSIONS = { \"timer\",\nfunction init() end\n",
            path,
        );
        assert!(
            unclosed.permissions.is_empty(),
            "没闭合的表不该读出东西: {:?}",
            unclosed.permissions
        );
    }

    /// 内置那份**由位置强制**常驻，用户装的认自己写的（方案 3.3）。
    ///
    /// 这条钉的是"谁说了算"：如果哪天有人改成读脚本里的 `PLUGIN_KIND`，
    /// 一个用户插件自述 builtin 就能免掉一切按位置算的口径（授权、备份都在这条上）。
    #[test]
    fn a_bundled_plugin_is_resident_even_without_declaring_it() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("lifecycle_by_position");
        let builtin = tmp.path().join("plugins").join("builtin");
        let user = tmp.path().join("data").join("plugins");
        write_lua(
            &builtin,
            "quiet_builtin",
            "PLUGIN_NAME = \"没声明的内置\"\n",
        );
        write_lua(&user, "quiet_user", "PLUGIN_NAME = \"没声明的用户插件\"\n");
        write_lua(
            &user,
            "loud_user",
            "PLUGIN_NAME = \"自己说要常驻\"\nPLUGIN_LIFECYCLE = \"always\"\n",
        );
        write_lua(
            &user,
            "fake_builtin",
            "PLUGIN_NAME = \"自称内置的\"\nPLUGIN_KIND = \"builtin\"\n",
        );

        let pm = manager_in(tmp.path());
        let by_file = |file: &str| -> DiscoveredPlugin {
            pm.list_discovered()
                .expect("两处目录都该读得动")
                .into_iter()
                .find(|p| p.file == file)
                .unwrap_or_else(|| panic!("清单里没有 {file}"))
        };
        assert_eq!(
            by_file("quiet_builtin").lifecycle(),
            PluginLifecycle::Always,
            "内置模块没自己声明也该常驻"
        );
        assert_eq!(
            by_file("quiet_user").lifecycle(),
            PluginLifecycle::Panel,
            "用户插件的缺省档是面板"
        );
        assert_eq!(
            by_file("loud_user").lifecycle(),
            PluginLifecycle::Always,
            "用户插件自己声明的 always 要认"
        );
        let fake = by_file("fake_builtin");
        assert_eq!(
            (fake.source, fake.lifecycle()),
            (PluginSource::User, PluginLifecycle::Panel),
            "脚本自述 PLUGIN_KIND 不改变身份：身份只由位置决定"
        );
    }

    // ---------- 第八节 A11：core 自己那份热重载扫描的三条保护 ----------

    /// 新文件要报、删文件也要报（旧实现只遍历本轮看到的，消失的那个永远没人提），
    /// 而 0 字节的 `.lua` 本轮不能算"有新版本"—— 空的 .lua 会被 Lua 当成空块、
    /// 一句错都不报，插件就此变成没有函数的空壳。
    #[test]
    fn hot_reload_round_reports_new_and_deleted_files_but_not_empty_ones() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("hr_new_del");
        let dir = tmp.path().join("plugins");
        write_lua(&dir, "a", "PLUGIN_NAME = \"甲\"\n");
        let mut last = HashMap::new();
        let mut primed = false;

        assert!(
            hot_reload_round(&dir, &mut last, &mut primed).0.is_empty(),
            "首轮只建基线，否则每次启动都把全部插件重载一遍"
        );
        assert!(primed);

        // 半截文件（编辑器"先截断再写入"的那一瞬间 / 网盘还没同步上来）
        write_lua(&dir, "b", "");
        assert!(
            hot_reload_round(&dir, &mut last, &mut primed).0.is_empty(),
            "0 字节的 b 不该被当成新版本，也不该被记进基线"
        );
        assert!(!last.contains_key("b"), "空壳不该有指纹");

        // 内容写上了 → 这才算一个新文件
        write_lua(&dir, "b", "PLUGIN_NAME = \"乙\"\n");
        assert_eq!(hot_reload_round(&dir, &mut last, &mut primed).0, ["b"]);

        // 已知插件被截成 0 字节：本轮不发请求，指纹沿用上一轮
        let known = last["a"];
        std::thread::sleep(std::time::Duration::from_millis(60));
        write_lua(&dir, "a", "");
        assert!(
            hot_reload_round(&dir, &mut last, &mut primed).0.is_empty(),
            "0 字节 = 还没写完，不能判成变更（旧实现在这里发请求，把空壳装上去）"
        );
        assert_eq!(last["a"], known, "删除检测不能把截断误判成文件消失");

        // 真删掉：必须报（旧实现漏这一整条）
        std::fs::remove_file(dir.join("a.lua")).unwrap();
        assert_eq!(hot_reload_round(&dir, &mut last, &mut primed).0, ["a"]);
        assert!(!last.contains_key("a"), "基线要跟着收缩");
    }

    /// 目录整体没读出来（`plugins` 被建成同名文件、整份放在断线的网盘上、构建脚本
    /// 正在替换目录）时既不判定任何变更，**也不许把基线清空** —— 清空的话下一轮
    /// 成功读取会把所有文件都当成新变更，白重载一整轮。
    #[test]
    fn hot_reload_round_keeps_the_baseline_when_the_dir_cannot_be_listed() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("hr_unreadable");
        let dir = tmp.path().join("plugins");
        write_lua(&dir, "a", "PLUGIN_NAME = \"甲\"\n");
        write_lua(&dir, "b", "PLUGIN_NAME = \"乙\"\n");
        let not_a_dir = write_lua(tmp.path(), "not_a_dir", "这是个文件，不是目录\n");
        let mut last = HashMap::new();
        let mut primed = false;
        let (first, why) = hot_reload_round(&dir, &mut last, &mut primed);
        assert!(first.is_empty());
        assert!(why.is_none(), "读得动的那一轮不该交回原因");
        let baseline = last.clone();

        assert!(
            scan_plugin_mtimes(&not_a_dir, &last).is_err(),
            "读不出来要当成「不知道」，不是「目录空了」"
        );
        let (reqs, why) = hot_reload_round(&not_a_dir, &mut last, &mut primed);
        assert!(
            reqs.is_empty(),
            "读不出来的那一轮不该判成\"所有插件都被删了\""
        );
        // 原因要交回调用方：线程按目录去重之后只说一次，判定函数自己不出声
        // （每 2 秒真说一次，一天四万条，把要看的那几条埋掉）。
        let why = why.expect("读不出来的原因要一路交回去，不是就地吞掉");
        assert!(
            !why.is_empty(),
            "交了错却没带上原因＝日志里只能说「读不出来」"
        );
        assert_eq!(last, baseline, "基线一个字都不许动");
        assert!(
            hot_reload_round(&dir, &mut last, &mut primed).0.is_empty(),
            "下一轮成功读取不能把每个文件都当成新变更（旧实现在这里全员重载）"
        );
    }

    /// 「只说一次」这件事必须**按目录各记一格**：用常数当键，内置那一格读不动就会把
    /// 数据目录那一格的公告一起吃掉 —— 症状是只有一处坏了，日志里永远只提另一处。
    /// 这条只能钉调用形态（一次线程要跑 2 秒一轮，用例里等不到）。
    #[test]
    fn the_unreadable_notice_is_keyed_per_directory() {
        let src = include_str!("manager.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let loop_body = &prod[prod
            .find("fn hot_reload_loop(")
            .expect("热重载线程的本体找不到了")
            ..prod.find("fn hot_reload_round(").expect("一轮扫描找不到了")];
        assert!(
            loop_body.contains("notice_once(&format!(\"watch-unreadable:{}\", dir.display()))"),
            "读不出来的公告不再按目录去重了（改成了常数键、或干脆不去重每轮都说）"
        );
        // 判定函数那一侧不许自己出声，否则去重这一层管不到它
        let scan_body = &prod[prod
            .find("fn scan_plugin_mtimes(")
            .expect("扫描函数找不到了")
            ..prod
                .find("/// 两轮快照之间该发哪些重载请求")
                .expect("下一段找不到")];
        assert!(
            !scan_body.contains("tracing::error!("),
            "一轮扫描自己出声了：去重在线程那一层，它拦不住每 2 秒一条"
        );
    }

    /// `poll_reload_requests` 这条路上，"文件已经不在盘上"要真的把插件卸掉。
    /// 扫描线程那边现在会发删除事件了，而 core 这头的处理原先是空的：
    /// `reload_plugin` 按名字找不到实例就返回 false → 删掉的插件要重启才停得下来，
    /// 而删除表达的意思恰恰是「别再跑它了」（桌面侧 `reload_plugin_by_key` 早就做了）。
    #[test]
    fn poll_unloads_a_plugin_whose_file_was_deleted() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("hr_poll_delete");
        let plugins = tmp.path().join("plugins");
        let p = write_lua(&plugins, "gone", &scheduler_user("会被删掉的"));
        let mut pm = manager_in(tmp.path());
        pm.load_plugin(&p, PluginSource::Builtin).expect("加载失败");
        assert_eq!(host::scheduler_state_for_test(), Some(1));

        std::fs::remove_file(&p).unwrap();
        // 不经线程：直接往检测线程那条 channel 里塞一条请求
        pm.reload_tx.send("gone".to_string()).unwrap();
        assert_eq!(pm.poll_reload_requests(), ["gone"], "删除该被当成一次卸载");
        assert!(
            pm.get_plugin("会被删掉的").is_none(),
            "文件没了却还在跑 = cleanup 永不执行"
        );
        assert_eq!(
            host::scheduler_state_for_test(),
            None,
            "卸载走了 cleanup，owner 才还得回来"
        );
    }

    /// 死循环必须被指令数上限打断，且沙箱里不能留着"换个线程躲开 hook"的出口。
    ///
    /// 实测（去掉本修复前）：同样规模的主线程循环被 hook 中断，而包进
    /// `coroutine.create` 就一路跑完不报错 —— Lua 的 hook 是 per-thread 的。
    /// 用有界循环而不是 `while true`：绕过成立时测试会自己跑完，不会挂住 CI。
    #[test]
    fn instruction_limit_cannot_be_escaped_by_coroutines() {
        let lua = PluginManager::create_sandboxed_lua().unwrap();
        lua.set_hook(
            HookTriggers::new().every_nth_instruction(1_000),
            |_lua, _dbg| -> mlua::Result<VmState> {
                Err(mlua::Error::RuntimeError("超出指令数上限".into()))
            },
        );

        let err = lua
            .load("local n = 0 for i = 1, 200000 do n = n + i end")
            .exec()
            .expect_err("主线程死循环必须被 hook 中断");
        assert!(err.to_string().contains("指令数上限"), "{err}");

        assert!(
            lua.globals()
                .get::<mlua::Value>("coroutine")
                .unwrap()
                .is_nil(),
            "协程是唯一能把指令计数搬走的入口，必须整体不可用"
        );
        let escaped = lua
            .load(
                "coroutine.resume(coroutine.create(function() \
                 local n = 0 for i = 1, 200000 do n = n + i end end))",
            )
            .exec();
        assert!(
            escaped.is_err(),
            "协程入口被移除后这段要立刻报错，而不是安静跑完"
        );
    }

    /// 沙箱必须让 io 库不可用、os 高危函数与 package.loadlib 被移除，
    /// 同时保留 os.date 等插件在用的安全函数。
    #[test]
    fn sandboxed_lua_blocks_dangerous_stdlib() {
        let lua = PluginManager::create_sandboxed_lua().unwrap();
        // io 库整体不可用
        assert!(lua.globals().get::<mlua::Value>("io").unwrap().is_nil());
        // os 高危函数已移除
        for name in ["execute", "exit", "getenv", "remove", "rename", "tmpname"] {
            let v: mlua::Value = lua.load(format!("return os.{name}")).eval().unwrap();
            assert!(v.is_nil(), "os.{name} 应为 nil");
        }
        // package.loadlib 已禁用
        let loadlib: mlua::Value = lua.load("return package.loadlib").eval().unwrap();
        assert!(loadlib.is_nil());
        // base 库 mlua 会无条件打开（白名单里没有 BASE 也挡不住），能落盘的两个
        // 入口只能按名字摘掉；协程库则整体不在白名单内（hook 是 per-thread 的）
        for name in ["dofile", "loadfile", "coroutine"] {
            let v: mlua::Value = lua.globals().get(name).unwrap();
            assert!(v.is_nil(), "{name} 应为 nil");
        }
        // 插件在用的安全函数仍在
        for expr in [
            "return os.date",
            "return os.time",
            "return os.clock",
            "return string.format",
            "return math.floor",
        ] {
            lua.load(expr).exec().unwrap();
        }
        // 运行时兜底：io.open 即便被伪造调用也应报"未定义"
        let err = lua.load("return io.open('C:/x.txt','w')").exec();
        let Err(err) = err else {
            panic!("io 不可用时必须报错");
        };
        assert!(err.to_string().contains("io"));
    }

    /// 内嵌控件的两个出口都必须走 `skip_nested_widget`，不许退回静默的 `if let Ok(..)`。
    ///
    /// 为什么按源码形状钉而不是跑一遍：这里要防的恰恰是"有人在某个内嵌分支上把
    /// `match` 改回 `if let`" —— 那种改动的症状是界面上少一块控件、日志里一个字都没有，
    /// 谁都不会报错，也就没有一条行为用例能盯住它（顶层那条 `跳过无法解析的控件` 当年
    /// 就是这么补上的，内嵌两处漏了半边）。
    #[test]
    fn nested_widget_failures_are_never_silent() {
        let src = include_str!("manager.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        assert_eq!(
            prod.match_indices("skip_nested_widget(").count(),
            3,
            "应当是「定义一处 + row.children 与 modal_form.widgets 各一处」，多一处少一处都要查"
        );
        assert!(
            !prod.contains("if let Ok(widget) = parse_widget("),
            "内嵌控件又退回静默丢弃了 —— 少一块控件会重新变成看不见的事"
        );
        // 顶层那条同样要在：三个出口共用一把尺子，不能只修内嵌
        assert!(
            prod.contains("跳过无法解析的控件"),
            "顶层的失败留痕被摘掉了"
        );
    }

    /// 存进一个坏版本之后，插件页必须看得见原因。
    ///
    /// 症状是"我改了文件但没生效"：热重载失败只进日志、`plugins-reloaded` 照发、
    /// 列表那一行既不报红也不说为什么 —— 因为 `list_discovered` 在"插件还活着"那一支
    /// 只看 `info.error`，从不看 `load_errors`。
    #[test]
    fn a_failed_hot_reload_shows_its_reason_on_the_plugin_page() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("reload_error_visible");
        let plugins = tmp.path().join("plugins");
        let file = write_lua(&plugins, "w", &view_only("视图插件"));
        let mut pm = manager_in(tmp.path());
        pm.load_plugin(&file, PluginSource::Builtin)
            .expect("第一版该装上");

        // 用户按一次 Ctrl+S：这次写进去的是半截表达式（语法错，Lua 编译就拒）
        std::fs::write(
            &file,
            "PLUGIN_NAME = \"视图插件\"\nfunction get_view() return { ",
        )
        .unwrap();
        assert!(!pm.reload_plugin("w"), "坏版本必须被判定成重载失败");

        let row = pm
            .list_discovered()
            .expect("这一轮的插件目录该读得动")
            .into_iter()
            .find(|p| p.file == "w")
            .expect("列表里得有这个文件那一行");
        let reason = row
            .error
            .clone()
            .expect("重载失败要在插件页上看得见，不能只剩一片安静");
        assert!(
            reason.contains("重载失败"),
            "要说清这是最近一次重载的结论：{reason}"
        );
        assert!(
            reason.contains("改之前"),
            "还得说清现在跑的是哪一份，否则用户以为新代码生效了：{reason}"
        );
        // 旧实例仍在服务（这不是"插件没了"）：`try_load` 刻意不销活着的旧实例，
        // 那条口径也在这里一起钉住 —— 哪天改成回滚成未加载，这条会红着提醒
        assert!(row.loaded, "重载失败不该把还在跑的旧实例判成未加载");

        // 改好之后再重载一次：那条原因必须消失，不能挂在行上不走
        std::fs::write(&file, view_with_name("视图插件", "视图插件-新")).unwrap();
        assert!(pm.reload_plugin("w"), "修好的版本该重载成功");
        let row = pm
            .list_discovered()
            .expect("这一轮的插件目录该读得动")
            .into_iter()
            .find(|p| p.file == "w")
            .expect("列表里得有这个文件那一行");
        assert!(
            row.error.is_none(),
            "成功那次要把失败原因清掉，否则红字会永久挂着：{:?}",
            row.error
        );
        assert!(row.loaded, "重载成功后跑的就是新那份");
    }

    /// 插件目录"这一轮读不动"与"一个插件都没有"必须是两种结局。
    ///
    /// 症状是界面上一句「暂无插件」：`discover()` 过去把 `read_dir` 的错误折成空清单，
    /// 而插件页对两种结局的画法是同一个。插件明明在盘上时这句话会把用户推向
    /// 重命名、重传插件，甚至以为自己的插件被删了。
    /// 四种读数缺一不可：读得动的清单／空目录／目录本来不存在（首启）／这一轮读不动（带原因）。
    #[test]
    fn an_unreadable_plugin_dir_is_not_reported_as_having_no_plugins() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("unreadable_plugin_dir");

        // —— 尺子本身：按路径钉住三种结局，先不碰进程全局 ——
        let ok_dir = tmp.path().join("scan_ok");
        std::fs::create_dir_all(&ok_dir).unwrap();
        std::fs::write(ok_dir.join("beta.lua"), "PLUGIN_NAME = \"乙\"\n").unwrap();
        std::fs::write(ok_dir.join("alpha.lua"), "PLUGIN_NAME = \"甲\"\n").unwrap();
        std::fs::write(ok_dir.join("readme.txt"), "这不是插件\n").unwrap();
        // 装进来的包与单文件在这里**一样是一个条目**：清单只认文件名，包什么时候拆
        // 是加载那一侧的事（`package::read_script`）。
        std::fs::write(ok_dir.join("gamma.zip"), b"packed").unwrap();
        let got = list_plugin_files(&ok_dir).expect("这个目录该读得动");
        assert_eq!(
            got.iter()
                .map(|p| p.file_stem().unwrap().to_str().unwrap().to_string())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta", "gamma"],
            "收 .lua 与 .zip 两种源文件、按文件名排序（列表稳定是页面上的顺序）: {got:?}"
        );

        let empty_dir = tmp.path().join("scan_empty");
        std::fs::create_dir_all(&empty_dir).unwrap();
        assert!(
            list_plugin_files(&empty_dir)
                .expect("空目录是读得动的")
                .is_empty(),
            "读全了、里面确实没有 ⇒ 空清单"
        );
        let absent = tmp.path().join("scan_never_created");
        assert!(
            list_plugin_files(&absent)
                .expect("目录不存在＝一个插件都没有，不是读不出来")
                .is_empty(),
            "全新安装一个目录都没建，此刻该报\"没有插件\"，不许报读不出来"
        );

        let not_a_dir = tmp.path().join("scan_is_a_file");
        std::fs::write(&not_a_dir, "这是一个文件，不是目录\n").unwrap();
        let err = list_plugin_files(&not_a_dir)
            .expect_err("把一个普通文件当插件目录读，必须交回原因而不是空清单");
        assert!(
            err.contains("读不出来") && err.contains("没有插件"),
            "要说清\"读不出来\"并且不许被读成\"没有插件\": {err}"
        );
        assert!(
            err.contains(&not_a_dir.display().to_string()),
            "得说出是哪个目录读不动，否则用户只能猜: {err}"
        );

        // —— 消费侧：命令层拿到的必须是同一个 Err ——
        // `plugins` 占成同名文件：`create_dir_all` 建不成，`read_dir` 也读不动。
        let plugins = tmp.path().join("plugins");
        std::fs::write(&plugins, "占位：这是一个文件\n").unwrap();
        let mut pm = manager_in(tmp.path());
        let err = pm
            .discover()
            .expect_err("插件目录是个文件时，取数这一头必须拿到原因");
        assert!(err.contains("读不出来"), "discover() 把结局弄丢了: {err}");
        let Err(err) = pm.list_discovered() else {
            panic!(
                "插件目录读不动时，列表这一头必须交 Err，而不是回一份空清单让页面画「暂无插件」"
            );
        };
        assert!(err.contains("读不出来"), "原因要原样交给命令层: {err}");

        // 点「启用」时那句假话（「插件文件不存在」）也不许再出现：文件在不在，
        // 这一轮根本没读出来，用户照着那句去重传插件是白跑一趟。
        let err = pm
            .set_enabled("w", true)
            .expect_err("目录读不动时启用必须失败");
        assert!(
            !err.contains("不存在"),
            "目录整体读不动时那句\"插件文件不存在\"是假的: {err}"
        );
        assert!(
            err.contains("读不出来"),
            "启用被拒的原因要落在目录上: {err}"
        );

        // —— 对照组：不许把矫枉过正当成修好 ——
        std::fs::remove_file(&plugins).unwrap();
        std::fs::create_dir_all(&plugins).unwrap();
        assert!(
            pm.list_discovered()
                .unwrap_or_else(|e| panic!("空的插件目录该读得动，实得 Err: {e}"))
                .is_empty(),
            "真没有插件时仍回空清单，页面那句「暂无插件」才说得起"
        );
    }

    /// 启动那一次加载，目录读不动必须出声。
    ///
    /// `load_all` 吃的是启动路径上唯一一份清单：它静默回手时，"5 个插件一个都没加载"
    /// 与"本来就没装插件"是同一个样子，而用户要等自己点开插件页才发现全是空的 ——
    /// 那一刻目录可能早已能读了，现场也就没了。
    #[test]
    fn load_all_announces_an_unreadable_plugin_directory() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("load_all_unreadable");
        std::fs::write(tmp.path().join("plugins"), "占位：这是一个文件\n").unwrap();
        let mut pm = manager_in(tmp.path());
        let (_, logs) = crate::logger::capture_logs(|| pm.load_all());
        assert!(
            pm.get_all_plugins().is_empty(),
            "读不动时一个插件也不该被加载（半份清单比没清单更难解释）"
        );
        assert!(
            logs.iter()
                .any(|l| l.contains("读不出来") && l.contains("不加载")),
            "启动那一次的静默等于永远没人知道: {logs:?}"
        );

        // 对照组：目录读得动（哪怕空）时不许喊
        let plugins = tmp.path().join("plugins");
        std::fs::remove_file(&plugins).unwrap();
        std::fs::create_dir_all(&plugins).unwrap();
        let (_, logs) = crate::logger::capture_logs(|| pm.load_all());
        assert!(
            !logs.iter().any(|l| l.contains("不加载")),
            "读得动就别喊，否则每启动一次都假警: {logs:?}"
        );
    }

    /// 同一份现场量第二样东西：`load_all` 的**返回值**。桌面端那个「已经加载过一遍」的闩
    /// 现在只由它决定 —— 读不动 ⇒ `false`（闩不抬，下次进页再试一次）。
    ///
    /// 与上一条分开写：那条盯的是"有没有出声"，这条盯的是"调用方能不能知道该不该重试"。
    #[test]
    fn load_all_reports_whether_the_directory_was_readable() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("load_all_latch");
        let plugins = tmp.path().join("plugins");
        std::fs::write(&plugins, "占位：这是一个文件\n").unwrap();
        let mut pm = manager_in(tmp.path());
        assert!(
            !pm.load_all(),
            "清单没拿到就是「一个都没试」，闩不许抬，否则整个会话不再加载"
        );
        assert!(
            pm.get_all_plugins().is_empty(),
            "读不动时不该有插件被记成已加载"
        );

        // 对照组：目录读得动（哪怕里面一个插件都没有）⇒ 这一轮是问得出答案的，闩该抬。
        // 「确认没有插件」与「这一刻问不出」必须是两个值，否则每次进页都要重扫一遍目录。
        std::fs::remove_file(&plugins).unwrap();
        std::fs::create_dir_all(&plugins).unwrap();
        assert!(pm.load_all(), "空目录是「问得出、答案是零个」，不是问不出");
    }

    /// 闩必须接在那一句返回值上 —— 这条盯的是桌面端的**调用形态**，不是函数名。
    ///
    /// 撤掉修复的写法是 `pm.load_all(); self.plugins_loaded = true;`：它照样调用
    /// `load_all`，所以"这个名字还在不在"那种判据对它无效（本仓那条教训）。
    #[test]
    fn the_native_latch_is_driven_by_the_load_result() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../desktop-native/src/main.rs");
        let src = std::fs::read_to_string(&path).expect("桌面端源码读不出来（路径变了？）");
        // 生产段：第一个 `#[cfg(test)]` 之前
        let prod = &src[..src
            .find("#[cfg(test)]")
            .expect("桌面端测试模块的起点找不到了")];
        let flat: String = prod.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            flat.contains("self.plugins_loaded=pm.load_all()"),
            "那个闩不再认这一轮的加载结果了"
        );
        assert!(
            !flat.contains("pm.load_all();self.plugins_loaded=true"),
            "读不动时不许无条件抬闩"
        );
    }

    /// 这道闸的形状：取数的三个消费点，一个也不许偷偷把 Err 折回"空"。
    ///
    /// 上面两条行为用例盯的是现在的写法；这里防的是"下一次有人把 `?` 换成
    /// `unwrap_or_default()`" —— 那种改动的症状恰恰是界面重新变成一片安静的「暂无插件」，
    /// 而改的那一瞬间之外没有任何东西会响。判序按归一化后的调用形态数，
    /// 光数函数名会被 `discover()` 的注释与另一种写法顶班。
    ///
    /// 目录分离之后 `discover()` 交出带来源的清单（路线图 11 节第 2 步：返回项带来源），
    /// 三个消费点如今**都要**那一份来源：列表要它分组、启动加载与「启用」要它决定
    /// 插件看见哪一组宿主 API（3.4）。`discover_paths()` 那条抹平来源的薄壳只剩测试在用。
    /// 两条都必须把"读不出来"交出去，合计仍是三个消费点。
    #[test]
    fn no_caller_folds_an_unreadable_plugin_dir_back_into_empty() {
        let src = include_str!("manager.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let flat: String = prod
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        // `discover_paths()` 只是"把来源抹平"的薄壳，它自己那一句委托不算消费点。
        // 先按位置把它摘掉，剩下的才是要判的三个消费点 —— 不摘的话它与列表那一处的
        // 归一化形状一模一样（都是 `Ok(self.discover()?)`），数出来的 2 说不清是谁。
        let shell_head = "pubfndiscover_paths(&self)->Result<Vec<PathBuf>,String>{";
        let shell_at = flat
            .find(shell_head)
            .expect("`discover_paths()` 的函数头形状变了（这条判序要跟着改）");
        let shell =
            &flat[shell_at..shell_at + flat[shell_at..].find('}').expect("函数体没有结束？") + 1];
        assert!(
            shell.contains("self.discover()"),
            "`discover_paths()` 不再经带来源那一头取数 = 它自己重新扫了一遍目录，\
             两处各扫会出现\"清单与来源对不上\""
        );
        let consumers = flat.replacen(shell, "", 1);
        assert!(
            !consumers.contains(shell_head),
            "薄壳没摘干净（生产段里出现了第二个同形状的 `discover_paths()`？）"
        );

        assert_eq!(
            consumers.matches("self.discover_paths()?").count(),
            0,
            "「启用」这一处从 3.4 起也要来源（它决定插件看见哪一组宿主 API），\
             生产段不再有只要路径的消费点。新加一处若真只要路径，先在这里说清它为什么不需要身份"
        );
        assert_eq!(
            consumers.matches("self.discover()?").count(),
            4,
            "带来源的四个消费点（列表、启用、面板打开时的 ensure_loaded、未加载时的授权）\
             都必须把 Err 原样交出去（`?`），而不是就地折成空 —— 折成空的那一档，症状是\
             \"目录读不动\"被念成\"没有这一份插件\"，授权就会报一个假原因"
        );
        assert_eq!(
            consumers.matches("matchself.discover()").count(),
            1,
            "启动加载那一处必须自己接住 Err 并出声"
        );
        // 兜底：五个消费点之外不许再有人取数（第六处一定是新加的，先问它吃哪种结局）
        let total = consumers.matches("self.discover()").count()
            + consumers.matches("self.discover_paths()").count();
        assert_eq!(
            total, 5,
            "取数的消费点是：列表、启用、启动加载、面板打开时的 ensure_loaded、\
             未加载时的授权（这一处用 `?` 把读不出来的原因原样交给调用方）。\
             第六处要先说清它吃的是哪种结局"
        );
        for (at, call) in consumers.match_indices("self.discover()") {
            let after = &consumers[at + call.len()..];
            let taken_by_question = after.starts_with('?');
            let taken_by_match = consumers[..at].ends_with("match");
            assert!(
                taken_by_question || taken_by_match,
                "第 {} 个字符处这一处 `discover()` 既没把 Err 交出去（`?`），也没自己接住并出声\
                 （match）—— 它接下来一定会把\"读不出来\"折成空清单",
                at
            );
        }
        for (at, call) in consumers.match_indices("self.discover_paths()") {
            let after = &consumers[at + call.len()..];
            let taken_by_question = after.starts_with('?');
            assert!(
                taken_by_question,
                "第 {} 个字符处这一处 `discover_paths()` 没把 Err 交出去（`?`）",
                at
            );
        }
        // 来源在清单这一头不能丢：`discover()` 交出的必须是带来源的那一项，
        // 退化成 `Vec<PathBuf>` 就等于把身份那件事又抹平（第 8 步的两栏分组会拿不到依据）。
        assert!(
            flat.contains("pubfndiscover(&self)->Result<Vec<Discovered>,String>{"),
            "`discover()` 的返回项不再带来源了"
        );
    }

    /// 目录分离这两步的接线形状：清单来自两处、旧目录只当迁移的源、开关是那条唯一的分支。
    ///
    /// 行为用例已经各自钉住了自己那一块，这里防的是**换法**：把内置目录改成又去读平铺目录、
    /// 或者把迁移偷偷变成移动，行为用例可能照样绿（夹具刚好都在），而产品语义已经换了。
    #[test]
    fn the_two_dirs_are_the_only_things_the_new_layout_reads() {
        let src = include_str!("manager.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let flat: String = prod
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        // 清单只从"那一处唯一的尺子"取目录，扫描本身对每处目录只写一遍。
        // 这两条是变异探针逼出来的：先前这里数的是 `list_plugin_files(&builtin_dir)`，
        // 也就是**清单自己又列了一遍目录** —— 把内置目录从 `paths::plugin_scan_dirs` 摘掉，
        // 行为用例照样绿，而插件页那一行「目录」已经和清单来的不是同一处。
        assert_eq!(
            flat.matches("paths::plugin_scan_dirs(self.legacy_layout())")
                .count(),
            1,
            "`plugin_dirs()`（热重载与存在性检查那几处用）不再委托那一把尺子"
        );
        assert!(
            flat.contains("letdirs=paths::plugin_scan_dirs(legacy)"),
            "`discover()` 不再从那一把尺子取目录 = 界面与清单会各说一套"
        );
        assert_eq!(
            flat.matches("list_plugin_files(dir)").count(),
            1,
            "每处目录一次扫描；数到第二处说明清单又列了一遍目录"
        );
        // 身份按位次翻译，不在清单里硬编码目录名（那样一改目录顺序就把身份贴错）。
        assert!(
            flat.contains("letsource=Self::source_at(index,legacy)"),
            "`source` 不再由位次决定 = 位置决定身份这条被换了"
        );
        // 身份不看脚本自己写什么：生产段里不许出现读 PLUGIN_KIND 的代码。
        // 判据要吃**去掉注释**的那一份 —— 上面这几段注释本身就写着这个词，
        // 拿原文数会把"注释里提到"数成"代码里在读"（假红），而摘掉判据又等于没判。
        assert_eq!(
            flat.matches("PLUGIN_KIND").count(),
            0,
            "身份由位置决定；一旦开始读脚本自述的 PLUGIN_KIND，权限模型第一天就失效"
        );
        // 内置目录不许在这份文件里被单独列进清单（只在 `paths` 那一处出现）
        assert_eq!(
            flat.matches("plugins_builtin_dir()").count(),
            0,
            "清单在这份文件里又点了一次内置目录 = 与界面那一行「目录」各说一套"
        );
        assert_eq!(
            flat.matches("get_bool(\"plugins\",\"legacy_layout\",false)")
                .count(),
            1,
            "开关只有一处读点，默认值必须是 false（新布局）"
        );
        // 迁移只复制：生产段里不许出现 remove/move 那两条（12.2 第一条纪律）
        let migrate = &flat[flat
            .find("fnmigrate_legacy_plugins")
            .expect("迁移函数不在了")..];
        let migrate = &migrate[..migrate
            .find("fnrun_legacy_migration")
            .expect("迁移函数与它的日志包装之间该是相邻的")];
        for forbidden in ["std::fs::remove", "std::fs::rename"] {
            assert!(
                !migrate.contains(forbidden),
                "迁移里出现 {forbidden} = 复制变成了移动，回滚开关会读不到东西"
            );
        }
        assert!(
            migrate.contains("BUILTIN_PLUGIN_STEMS.contains(&stem.as_str())"),
            "白名单那道判断不在迁移里 = 内置脚本会被搬进数据目录"
        );
    }

    /// 「插件文件不存在」不能当成"这一轮没列出它"的默认解释。
    ///
    /// 目录整体读不出来的那条路已经由 `discover()` 直接交 Err（见
    /// `an_unreadable_plugin_dir_is_not_reported_as_having_no_plugins`），剩下的"目录读全了、
    /// 清单里却没有这个名字"仍然有两种可能：文件在盘上但这一轮没扫到（目录正在被整份替换），
    /// 或真没有这个名字。后者才配得上「不存在」—— 把前者说成不存在，用户会去重命名、重传插件。
    ///
    /// 目录分离之后这个名字可能在**两处**里的任何一处，所以存在性检查必须问全部被扫的目录：
    /// 只问内置那一处的话，用户装的那份会被说成「插件文件不存在」。
    #[test]
    fn an_unreadable_directory_is_not_reported_as_a_missing_file() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("missing_reason");
        let dir = tmp.path().join("plugins");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("w.lua"), "PLUGIN_NAME = \"w\"\n").unwrap();
        // 第二处：只在数据目录里有的名字，也必须被认成"在盘上"
        let user = tmp.path().join("data").join("plugins");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(user.join("u.lua"), "PLUGIN_NAME = \"u\"\n").unwrap();
        let both = vec![dir.clone(), user.clone()];

        let present = missing_plugin_reason(&both, "w");
        assert!(
            present.contains("这一轮没扫到") && present.contains("再试一次"),
            "文件在盘上就该说\"读不到/没扫到\"，不是\"不存在\": {present}"
        );
        let present_in_second_dir = missing_plugin_reason(&both, "u");
        assert!(
            present_in_second_dir.contains("这一轮没扫到"),
            "名字在第二处目录里，那句\"不存在\"就是假的: {present_in_second_dir}"
        );
        let absent = missing_plugin_reason(&both, "nope");
        assert!(
            absent.contains("插件文件不存在") && absent.contains("nope"),
            "真没有这个名字才配得上那句\"不存在\": {absent}"
        );
        // 带路径分隔或 `..` 的名字不许触发存在性检查：那会把这里变成文件存在预言机
        for evil in ["../plugins/w", "..\\plugins\\w", "a/b", ""] {
            let r = missing_plugin_reason(&both, evil);
            assert!(
                !r.contains("这一轮没扫到"),
                "越界/畸形名不该给出探测结果: {evil} → {r}"
            );
        }

        // 接线：`set_enabled` 走的必须就是这个函数（判据写在源码里，行为用例碰不得配置）
        let src = include_str!("manager.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        assert_eq!(
            prod.match_indices("missing_plugin_reason(").count(),
            2,
            "应当是「定义一处 + set_enabled 用一处」，多一处少一处都要查"
        );
        // 而且问的是"这一轮被扫的那几处"，不是硬编码的某一个目录
        let flat: String = prod
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        assert!(
            flat.contains("missing_plugin_reason(&self.plugin_dirs()"),
            "存在性检查必须跟着 plugin_dirs() 那把尺子走，否则内置/数据两处会各说一套"
        );
    }

    /// 两处目录的身份由**位置**决定，清单一并交出来源；同名只认随包那一份。
    ///
    /// 这条盯三件事，都是目录分离之后才会出现的形状：
    /// ① 内置与用户各自从自己的目录进清单，来源标签不能贴反（贴反＝授权与备份口径全错）；
    /// ② 数据目录里放一份与内置同名的文件，不会变成两条清单条目，也不会顶掉内置那份；
    /// ③ 顶掉的那一刻要在日志里留下原因 —— 否则用户看到的是"我放了但插件页没多一条"。
    #[test]
    fn the_two_plugin_dirs_keep_their_identities_and_same_name_collides_once() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("two_dirs");
        let builtin = tmp.path().join("plugins").join("builtin");
        let user = tmp.path().join("data").join("plugins");
        write_lua(&builtin, "shared", "PLUGIN_NAME = \"随包那份\"\n");
        write_lua(&builtin, "only_builtin", "PLUGIN_NAME = \"只内置\"\n");
        write_lua(&user, "only_user", "PLUGIN_NAME = \"只用户\"\n");
        // 同名的第二份：内容刻意不同，读出来的名字能证明是哪一份进了清单
        write_lua(&user, "shared", "PLUGIN_NAME = \"用户改过的那份\"\n");

        let pm = manager_in(tmp.path());
        // 第一轮的日志里必须有那句"顶掉"：`notice_once` 是按本进程记的，第二轮只该安静。
        let (found, first_logs) = crate::logger::capture_logs(|| pm.discover());
        let found = found.expect("两处目录都读得动，该交出完整清单");
        let by_stem = |stem: &str| -> Vec<&Discovered> {
            found
                .iter()
                .filter(|f| PluginManager::stem_of(&f.path) == stem)
                .collect()
        };
        assert_eq!(
            by_stem("shared").len(),
            1,
            "同名的两份只该进清单一条（内置在前），实测 {found:?}"
        );
        assert_eq!(
            by_stem("shared")[0].source,
            PluginSource::Builtin,
            "内置模块以随包那一份为准"
        );
        assert_eq!(
            by_stem("only_user")[0].source,
            PluginSource::User,
            "数据目录那份的身份是用户插件，来源不许贴反"
        );
        assert_eq!(by_stem("only_builtin")[0].source, PluginSource::Builtin);
        assert_eq!(
            found.len(),
            3,
            "两处各一份 + 同名一条 ⇒ 清单共 3 条，实测 {found:?}"
        );

        // 名字读出来的是随包那份 ⇒ 顶掉的那一份在日志里留了原因
        let (listed, second_logs) = crate::logger::capture_logs(|| pm.list_discovered());
        let rows = listed.expect("清单读得动");
        let shared = rows
            .iter()
            .find(|r| r.file == "shared")
            .expect("清单里有 shared");
        assert_eq!(
            shared.manifest.name, "随包那份",
            "插件页那一行是内置那份的名字，用户复制的那份没有顶掉它"
        );
        assert_eq!(shared.source, PluginSource::Builtin);
        assert!(
            rows.iter()
                .any(|r| r.file == "only_user" && r.source == PluginSource::User),
            "列表这一头也要带上来源: {rows:?}"
        );
        let shown = first_logs.join("\n");
        assert!(
            first_logs
                .iter()
                .any(|l| l.contains("shared") && l.contains("不加载")),
            "同名被内置顶掉这件事要出声，否则症状是\"放了插件但插件页没多一条\": {shown}"
        );
        // 那句只说一次：插件页开着时 `discover()` 每秒一轮，同一件事刷满日志就等于没说。
        // 这条腿的观察量是"第二轮确实看得见重跑"（同一份盘、同一组目录），不是猜的。
        assert!(
            !second_logs.iter().any(|l| l.contains("不加载")),
            "同一件事第二轮不许再刷一遍: {}",
            second_logs.join("\n")
        );
    }

    /// 旧平铺目录里的用户插件是**复制**进数据目录的，同名不覆盖，内置白名单不搬。
    ///
    /// 三条各对一个真实后果：
    /// · 原件不删 —— `[plugins] legacy_layout = true` 那条回滚只认旧目录，搬走就把回滚
    ///   变成"请用户重新下载插件"；
    /// · 同名不动 —— 用户已经在新目录放过一份，说明那才是他要跑的（12.2 第 2 步：以数据目录为准）；
    /// · 白名单不搬 —— 内置脚本搬进数据目录会变成两条同 stem 条目，而且"用户改过的内置脚本
    ///   从此以用户插件身份活着"与「升级覆盖内置脚本」这条既有行为相反。
    #[test]
    fn legacy_plugins_are_copied_into_the_data_dir_without_overwriting() {
        // 用 `test_app_dir` 那一套临时目录（Drop 收尾，**断言失败也收**）：手写
        // `remove_dir_all` 只在全绿时执行，一次红就在 %TEMP% 里永久留一份；
        // 门禁的 TEMP_LEAK 那一道量的就是这个。
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("legacy_copy");
        let legacy = tmp.path().join("plugins");
        let user = tmp.path().join("data").join("plugins");
        write_lua(&legacy, "mytool", "PLUGIN_NAME = \"旧目录那份\"\n");
        write_lua(&legacy, "pomodoro_plugin", "PLUGIN_NAME = \"随包的内置\"\n");
        write_lua(&user, "keepme", "PLUGIN_NAME = \"新目录已有\"\n");

        let report = migrate_legacy_plugins(&legacy, &user);
        assert_eq!(report.copied, vec!["mytool".to_string()], "只搬用户那份");
        assert_eq!(
            report.builtin_kept,
            vec!["pomodoro_plugin".to_string()],
            "白名单里的名字由 plugins/builtin/ 提供，不搬"
        );
        // 复制不移动：原件必须还在旧目录
        assert!(
            legacy.join("mytool.lua").is_file(),
            "旧目录的原件被删了 —— 回滚开关就没东西可读了"
        );
        assert!(user.join("mytool.lua").is_file(), "数据目录该多出这一份");
        assert!(
            !user.join("pomodoro_plugin.lua").exists(),
            "内置脚本被搬进了数据目录：插件页会出现两条同名"
        );

        // 同名不覆盖：把数据目录那份改成另一个内容，再跑一轮，它一个字都不许变
        std::fs::write(user.join("mytool.lua"), "PLUGIN_NAME = \"用户自己那份\"\n").unwrap();
        let again = migrate_legacy_plugins(&legacy, &user);
        assert_eq!(
            again.conflicts,
            vec!["mytool".to_string()],
            "这一份该记成\"没覆盖\"，不是\"复制成功\""
        );
        assert!(again.copied.is_empty(), "重跑一轮不能又搬一次");
        assert_eq!(
            std::fs::read_to_string(user.join("mytool.lua")).unwrap(),
            "PLUGIN_NAME = \"用户自己那份\"\n",
            "数据目录那份被旧目录顶掉了 —— 12.2 说的是以数据目录为准"
        );

        // 旧目录本来不存在 = 全新安装，不是"读不出来"
        let fresh = migrate_legacy_plugins(
            &tmp.path().join("no_such_plugins_dir"),
            &tmp.path().join("no_such_user_dir"),
        );
        assert!(
            fresh.unreadable.is_none(),
            "目录不存在就是没有旧插件，不许报读不出来: {:?}",
            fresh.unreadable
        );
    }

    /// 迁移搬不动时只记 error，启动照旧：清单不因此变空，也不交 Err。
    ///
    /// 这是整套方案里唯一不可逆那一步的反面：如果"复制失败"变成"起不来"，用户双击没反应
    /// 而日志里只有一句权限错误 —— 那比留在旧目录不能用了更糟。
    /// 夹具取"数据目录那一格被同名普通文件占住"（复制必败，而 `run_legacy_migration`
    /// 拿的是已经算好的两处目录，不看这一格）。
    #[test]
    fn a_migration_copy_failure_is_logged_and_does_not_block_startup() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("migration_fail");
        let legacy = tmp.path().join("plugins");
        write_lua(&legacy, "mytool", "PLUGIN_NAME = \"旧目录那份\"\n");

        // 尺子本身：目标目录建不出来（父级是个普通文件）⇒ 进的是 failed，不是 copied/conflicts
        let blocked_user = tmp.path().join("no_such_parent").join("plugins");
        std::fs::write(tmp.path().join("no_such_parent"), "这不是目录").unwrap();
        let report = migrate_legacy_plugins(&legacy, &blocked_user);
        assert!(
            report.copied.is_empty() && report.conflicts.is_empty(),
            "一份都没搬成，不该有 copied/conflicts: {report:?}"
        );
        assert_eq!(
            report.failed.iter().map(|(s, _)| s).collect::<Vec<_>>(),
            vec!["mytool"],
            "复制失败要落在 failed 里，并带上文件名"
        );
        assert!(!report.failed[0].1.is_empty(), "还要带上原因");

        // 消费侧：这一轮失败要说 error、要点名文件，并且不许念"已复制"
        let (_, logs) = crate::logger::capture_logs(|| {
            run_legacy_migration(&legacy, &blocked_user);
        });
        let shown = logs.join("\n");
        assert!(
            logs.iter()
                .any(|l| l.contains("mytool") && l.contains("不阻塞启动")),
            "失败要点名是哪个文件、并说不阻塞这件事: {shown}"
        );
        assert!(
            !shown.contains("已复制到数据目录"),
            "一份都没搬成，不该念\"已复制\": {shown}"
        );
        assert!(
            !blocked_user.join(MIGRATION_MARKER).is_file(),
            "什么都没搬成却写了标记 = 下次启动不再重试，那份插件就永远留在旧目录"
        );

        // 旧目录整个读不出来（被建成同名文件）也只出声，并说不影响新布局的加载
        let legacy_file = tmp.path().join("broken.lua");
        std::fs::write(&legacy_file, "占位：这是一个文件").unwrap();
        let (_, logs) = crate::logger::capture_logs(|| {
            run_legacy_migration(&legacy_file, &tmp.path().join("some_user_dir"));
        });
        let shown = logs.join("\n");
        assert!(
            shown.contains("旧目录这一轮搬不动也不影响加载"),
            "旧目录读不出来要出声，但要说不影响加载: {shown}"
        );
    }

    /// `[plugins] legacy_layout = true`：只扫旧目录，忽略新布局，也不做迁移。
    ///
    /// 这条开关存在的意义是"线上新布局出问题时用户能自己退回去"，所以退回的必须是
    /// **完整旧行为**：旧目录那份清单、不碰数据目录、来源只能是旧目录。
    /// 开关打开时还往数据目录复制，回滚就不再是"退回上一版"而是"两份都在跑"。
    #[test]
    fn the_rollback_switch_scans_only_the_legacy_dir() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("legacy_switch");
        let legacy = tmp.path().join("plugins");
        write_lua(&legacy, "oldstyle", "PLUGIN_NAME = \"旧目录那份\"\n");
        write_lua(
            &tmp.path().join("plugins").join("builtin"),
            "newstyle",
            "PLUGIN_NAME = \"内置那份\"\n",
        );
        write_lua(
            &tmp.path().join("data").join("plugins"),
            "userstyle",
            "PLUGIN_NAME = \"用户那份\"\n",
        );

        // 开关默认关：新布局两处
        let off = FocusFlowConfig::load(tmp.path().join("config.ini")).unwrap();
        let off = Box::leak(Box::new(off));
        let pm = manager_in_with(off, tmp.path());
        assert!(!pm.legacy_layout(), "默认值就该是新布局");
        let found = pm.discover().expect("两处都读得动");
        let got: Vec<(String, PluginSource)> = found
            .iter()
            .map(|f| (PluginManager::stem_of(&f.path), f.source))
            .collect();
        assert!(
            got.contains(&("newstyle".to_string(), PluginSource::Builtin)),
            "内置那份要从 plugins/builtin/ 进清单，来源标内置: {got:?}"
        );
        assert!(
            got.contains(&("userstyle".to_string(), PluginSource::User)),
            "数据目录那份的来源必须是用户插件: {got:?}"
        );
        assert!(
            got.contains(&("oldstyle".to_string(), PluginSource::User)),
            "旧目录那份要先复制进数据目录、再以用户插件的身份进清单 —— 迁移不接进清单，\
             升级后就等于把用户已有的插件弄丢了: {got:?}"
        );
        assert!(
            tmp.path().join("plugins").join("oldstyle.lua").is_file(),
            "复制不是移动：旧目录那份原件必须还在（回滚开关只认它）"
        );

        // 开关打开：只认旧目录，且数据目录一个字都不被写
        std::fs::write(
            tmp.path().join("config.ini"),
            "[plugins]\nlegacy_layout = true\n",
        )
        .unwrap();
        let on = Box::leak(Box::new(
            FocusFlowConfig::load(tmp.path().join("config.ini")).unwrap(),
        ));
        let pm = manager_in_with(on, tmp.path());
        assert!(pm.legacy_layout(), "配置里那句 true 要读得出来");
        let found = pm.discover().expect("旧目录读得动");
        assert_eq!(
            found
                .iter()
                .map(|f| (PluginManager::stem_of(&f.path), f.source))
                .collect::<Vec<_>>(),
            vec![("oldstyle".to_string(), PluginSource::Legacy)],
            "回滚分支里清单只有旧目录那一份，来源也必须说清是旧目录"
        );
    }

    /// 指令上限的配置值：笔误的大数按默认处理，不许当成"没有上限"。
    ///
    /// 这条防的是把"防死循环"这道保险用一次配置就关掉：`every_nth_instruction` 的
    /// 粒度是"跑满 N 条才查一次钩子"，N 被夹到 42.9 亿时一次 UI 调用永远查不到第二次，
    /// `while true do end` 照旧冻住主线程 —— 症状与没有这道保险完全一样，
    /// 而插件页上不会有任何异常。
    #[test]
    fn a_typo_in_instruction_limit_does_not_disable_the_guard() {
        let default = DEFAULT_INSTRUCTION_LIMIT as u32;
        assert_eq!(instruction_budget_from(0), default, "没写/写 0 用默认");
        assert_eq!(
            instruction_budget_from(-1),
            default,
            "负数不能变成 0 次检查"
        );
        assert_eq!(
            instruction_budget_from(5_000_000),
            5_000_000,
            "合理值原样采纳"
        );
        assert_eq!(
            instruction_budget_from(MAX_INSTRUCTION_LIMIT),
            MAX_INSTRUCTION_LIMIT as u32,
            "上边界本身要收（夹的是越界，不是边界）"
        );
        assert_eq!(
            instruction_budget_from(MAX_INSTRUCTION_LIMIT + 1),
            default,
            "越界按笔误回落默认，不能往上夹成 u32::MAX"
        );
        assert_eq!(instruction_budget_from(999_999_999_999), default);
        assert_eq!(
            instruction_budget_from(i64::MAX),
            default,
            "天文数字也不能翻上去"
        );

        // 接线：旧的那句 clamp 不许回来。needle 用拼接 —— 上面那个函数的**注释里
        // 就写着这串原文**，直接当字面量会先匹配到说明文字，把判据变成自己盯自己。
        let src = include_str!("manager.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let old_clamp = format!("clamp(0, {} as i64)", "u32::MAX");
        assert!(
            !prod.contains(old_clamp.as_str()),
            "指令上限又回到「越界就抬到最大」的夹法了"
        );
        assert_eq!(
            prod.match_indices("instruction_budget_from(").count(),
            2,
            "应当是「定义一处 + arm_instruction_budget 用一处」"
        );
    }

    /// 没写 `id` 的弹窗要拿到一个**稳定**的 id。
    ///
    /// 前端 `plugins.js` 那圈 `modal_form` 的兜底是 `"pf-modal-" + Math.random()`：
    /// 空 id 每渲染一次就换一个 DOM id，而"哪个弹窗开着"记在 `openModals` 这把集合里、
    /// 键就是那个 id ⇒ 插件页每 2 秒一次的数据推送会把用户**正在填的弹窗自己关掉**
    /// （症状是"填了一半突然没了"）。派生只依赖控件在列表里的位置，所以同一份视图
    /// 每轮渲染都给出同一个值。
    #[test]
    fn a_modal_without_id_gets_a_stable_one() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("modal_auto_id");
        let plugins = tmp.path().join("plugins");
        let src = r###"
PLUGIN_NAME = "弹窗插件"
function get_view()
  return { title = "t", widgets = {
    { type = "modal_form", title = "新增", submit = "add",
      fields = { { kind = "text", field = "f", label = "L" } } },
    { type = "modal_form", id = "mine", title = "编辑", submit = "edit" },
  } }
end
"###;
        let file = write_lua(&plugins, "modal", src);
        let mut pm = manager_in(tmp.path());
        pm.load_plugin(&file, PluginSource::Builtin)
            .expect("这份视图该装得起来");

        let ids = |pm: &PluginManager| -> Vec<String> {
            let info = pm.get_plugin("弹窗插件").expect("插件应在表里");
            let view = info.view.clone().expect("加载时就该有视图");
            view.widgets
                .iter()
                .map(|w| match w {
                    crate::plugins::Widget::ModalForm { id, .. } => id.clone(),
                    other => panic!("第二个控件不是弹窗：{other:?}"),
                })
                .collect()
        };

        let first = ids(&pm);
        assert_eq!(first.len(), 2, "夹具前提：两个弹窗控件");
        assert!(
            first[0].starts_with("pf-modal-"),
            "缺 id 的弹窗必须拿到带前缀的派生 id，绝不能留空（空 id 会让前端退到 Math.random）：{first:?}"
        );
        assert_eq!(first[1], "mine", "插件自己写了 id 就要用他的，一个字不改");
        assert_ne!(
            first[0], first[1],
            "派生值要按位置唯一，两个弹窗不能抢同一个 DOM id"
        );

        // 再渲染一次：这条才是那场"弹窗自己关掉"的真正判据
        assert!(
            pm.reload_plugin("modal"),
            "内容没变的重载该成功（这一步会重跑 get_view 并重新派生 id）"
        );
        assert_eq!(
            ids(&pm),
            first,
            "id 每轮渲染都变 ⇒ 推送一来，正在填的弹窗就没了"
        );
    }

    /// 图表控件的解析口径：点数上限夹头部、横轴与数值尾对齐、非法元素只丢自己那一格。
    ///
    /// 四条腿各对一个"面板看起来正常但数据是错的"形状：
    /// ① 超上限夹**尾部**会把最新几天藏起来 —— 趋势图留的必须是最近这一段；
    /// ② 横轴写成数字（`labels = {1,2,3}`，作者拿日序号填的很常见）时，整表转
    ///    `Vec<String>` 会一句都不剩 ⇒ 图有、轴空白、还不报错；
    /// ③ `points` 里混进一个 `"12 万"`（从展示文本抄来的）时，整表转 `Vec<i64>` 同样清零，
    ///    画出来是一张「暂无数据」的空图，而盘上明明有值 —— 必须只丢那一格并出声；
    /// ④ `kind` 写错（`pie`）不许把整个控件变成 `[未知控件]` 或空白：按折线画照样看得出趋势。
    /// 空 `points` 是合法输入（今天还没数据），交回空序列，怎么画由渲染那一头负责（那边
    /// 有自己的一条：画基线并写「暂无数据」，不许留白）。
    #[test]
    fn a_chart_keeps_the_recent_points_and_pairs_its_axis() {
        use crate::plugins::{Widget, MAX_CHART_POINTS};
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("chart_parse");
        let plugins = tmp.path().join("plugins");

        let many_points = (1..=100i64)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let many_labels = (1..=100i32)
            .map(|n| format!("\"d{n}\""))
            .collect::<Vec<_>>()
            .join(",");
        let fixture = |name: &str, body: &str| {
            format!(
                "PLUGIN_NAME = \"{name}\"\nfunction get_view() return {{ title = \"t\", \
                 widgets = {{ {body} }} }} end\n"
            )
        };
        let cases: Vec<(&str, String)> = vec![
            (
                "基本形状",
                fixture(
                    "基本形状",
                    r#"{ type = "chart", kind = "bar", labels = {"甲", "乙"}, points = {1, 2} }"#,
                ),
            ),
            (
                "夹掉的是头部",
                fixture(
                    "夹掉的是头部",
                    &format!("{{ type = \"chart\", labels = {{{many_labels}}}, points = {{{many_points}}}}}"),
                ),
            ),
            (
                "横轴填的是数字",
                fixture(
                    "横轴填的是数字",
                    r#"{ type = "chart", labels = {1, 2, 3}, points = {7, 8, 9} }"#,
                ),
            ),
            (
                "数值里混进一个词",
                fixture(
                    "数值里混进一个词",
                    r#"{ type = "chart", labels = {"a", "b", "c"}, points = {5, "12 万", 7} }"#,
                ),
            ),
            (
                "kind 写了个画不了的",
                fixture(
                    "kind 写了个画不了的",
                    r#"{ type = "chart", kind = "pie", points = {3} }"#,
                ),
            ),
            (
                "今天还没有数据",
                fixture("今天还没有数据", r#"{ type = "chart", points = {} }"#),
            ),
        ];

        let mut pm = manager_in(tmp.path());
        let mut parsed: Vec<(String, String, Vec<String>, Vec<i64>)> = Vec::new();
        for (stem, src) in &cases {
            let file = write_lua(&plugins, stem, src);
            let name = pm
                .load_plugin(&file, PluginSource::Builtin)
                .unwrap_or_else(|e| panic!("这份视图该装得起来（{stem}）: {e}"));
            let info = pm.get_plugin(&name).expect("插件应在表里");
            let view = info.view.clone().expect("加载时就该有视图");
            let widget = view
                .widgets
                .first()
                .unwrap_or_else(|| panic!("{stem} 一个控件都没有"));
            match widget {
                Widget::Chart {
                    kind,
                    labels,
                    points,
                } => parsed.push((
                    stem.to_string(),
                    kind.clone(),
                    labels.clone(),
                    points.clone(),
                )),
                other => panic!(
                    "{stem} 的 chart 没解析成 Chart 控件，退化成 {other:?} —— \
                     落到「未知控件」那条臂上时编译与既有判据都不会响"
                ),
            }
        }
        let find = |stem: &str| -> &(String, String, Vec<String>, Vec<i64>) {
            parsed
                .iter()
                .find(|p| p.0 == stem)
                .unwrap_or_else(|| panic!("夹具里少了 {stem}"))
        };

        let (_, kind, labels, points) = find("基本形状");
        assert_eq!(kind, "bar", "kind 要原样交出去");
        assert_eq!(labels, &vec!["甲".to_string(), "乙".to_string()]);
        assert_eq!(points, &vec![1, 2]);

        // ① 一百个点 ⇒ 留最后 90 个（11..=100），横轴跟着一起对齐（d11..=d100）
        let (head_dropped, tail): (Vec<i64>, Vec<i64>) =
            ((1..=10i64).collect(), (11..=100i64).collect());
        let want_labels: Vec<String> = (11..=100i32).map(|n| format!("d{n}")).collect();
        let got = find("夹掉的是头部");
        assert_eq!(got.2, want_labels, "横轴必须跟着夹掉同样的头部");
        assert!(
            !got.3.starts_with(&head_dropped),
            "头部那 10 个点还留着 ⇒ 夹的位置不对: {:?}",
            &got.3[..10.min(got.3.len())]
        );
        assert_eq!(got.3, tail, "留的必须是最近 90 天");
        assert_eq!(
            got.3.len(),
            MAX_CHART_POINTS,
            "上限挂在一个地方，别这里 90 那里 89"
        );

        // ② 横轴给数字 ⇒ 变成自己的十进制文本，不是一个都不剩
        assert_eq!(
            find("横轴填的是数字").2,
            vec!["1".to_string(), "2".to_string(), "3".to_string()],
            "整表转 Vec<String> 在这条输入上会清零 ⇒ 图有轴空白"
        );

        // ③ 混进一个词 ⇒ 只丢那一格（5 与 7 都要留着）
        assert_eq!(
            find("数值里混进一个词").3,
            vec![5, 7],
            "一格坏不能把整条序列清零"
        );

        // ④ kind 不认识也照样收下（按折线画是渲染那一头的兜底，控件本身不许消失）
        assert_eq!(find("kind 写了个画不了的").1, "pie");
        assert_eq!(find("kind 写了个画不了的").3, vec![3]);

        // 空数据是合法输入：交回空序列，「暂无数据」由渲染画
        assert!(
            find("今天还没有数据").3.is_empty(),
            "空 points 不该变成错误或缺失控件"
        );
    }

    /// 图表的两处出声要点名到"是哪个控件"，并且同一件事本进程只说一次。
    ///
    /// 插件面板开着时 `get_view()` 每秒跑一轮：夹了 10 个点如果每秒都 warn 一句，
    /// 日志就被同一件事刷满，而真正要看的那条线索（哪个控件、夹了多少）反而找不到。
    #[test]
    fn an_over_long_chart_says_it_once_and_names_the_widget() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("chart_cap_notice");
        let plugins = tmp.path().join("plugins");
        let many = (1..=200i64)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let src = format!(
            "PLUGIN_NAME = \"长图\"\nfunction get_view() return {{ title = \"t\", widgets = {{ \
             {{ type = \"chart\", points = {{{many}}} }} }} }} end\n"
        );
        let file = write_lua(&plugins, "longchart", &src);
        let mut pm = manager_in(tmp.path());
        let (loaded, logs) =
            crate::logger::capture_logs(|| pm.load_plugin(&file, PluginSource::Builtin));
        loaded.expect("这份视图该装得起来");
        let shown = logs.join("\n");
        assert!(
            logs.iter()
                .any(|l| l.contains("只画最近") && l.contains("200") && l.contains("110")),
            "夹了头部要点名交了多少、夹掉几个: {shown}"
        );

        // 第二轮（同一个进程、同一个控件）不许再说一遍
        let (_, second) = crate::logger::capture_logs(|| {
            pm.reload_plugin("longchart");
        });
        assert!(
            !second.iter().any(|l| l.contains("只画最近")),
            "同一件事第二轮不许再刷一遍: {}",
            second.join("\n")
        );
    }

    /// 钩子写了名字、偏偏不是函数：三处入口都要说「不是函数」，不许说「没定义」。
    ///
    /// `on_action = "handle_click"`（把函数名写成了字符串）与 `get_view = { ... }`
    /// 都是作者笔误，而旧写法一律走 `get::<mlua::Function>` 的失败分支，把两种完全不同的
    /// 事捏成一句「插件未定义 on_action」：作者照着这句话去查「我哪里没写」，
    /// 可他明明写了。`record_key` 那一条已经分开（见 `plugin_key_event` 的
    /// 「有值但不是函数」），这里是同一族剩下的三处。
    ///
    /// 反向腿一并钉死：真的没写钩子时那句「未定义」必须还在说「未定义」，
    /// 没定义 get_view 的插件继续算正常（多数插件没有面板），否则这条闸只是换了个瞎法。
    #[test]
    fn a_hook_written_as_a_non_function_is_not_reported_as_missing() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("hook_not_a_function");
        let file = write_lua(
            &tmp.path().join("plugins"),
            "mistyped",
            r###"
PLUGIN_NAME = "写错钩子"
on_action = "handle_click"
set_field = 42
get_view = { title = "这是一张表" }
"###,
        );
        let mut pm = manager_in(tmp.path());
        let name = pm
            .load_plugin(&file, PluginSource::Builtin)
            .expect("钩子写错类型不该挡住加载");

        let checks: [(Result<(), String>, &str); 3] = [
            (pm.plugin_action(&name, "start"), "on_action"),
            (pm.plugin_set_field(&name, "d_work", "9"), "set_field"),
            (pm.refresh_view(&name), "get_view"),
        ];
        for (got, hook) in checks {
            let Err(err) = got else {
                panic!("{hook} 不是函数，这条入口必须报错");
            };
            assert!(
                err.contains(hook) && err.contains("不是函数"),
                "{hook} 要指名是哪个钩子、错在哪一类，实得：{err}"
            );
            assert!(
                !err.contains("未定义"),
                "{hook} 明明写了名字，不许报成没定义，实得：{err}"
            );
        }

        // 反向腿：真没写钩子的插件，话必须说回「未定义」。
        let plain = write_lua(
            &tmp.path().join("plugins"),
            "plain",
            "PLUGIN_NAME = \"没写钩子\"\nfunction get_view() \
             return { title = \"v\", widgets = { {type=\"label\", text=\"v\"} } } end\n",
        );
        let name2 = pm
            .load_plugin(&plain, PluginSource::Builtin)
            .expect("无钩子插件应装得上");
        let err = pm
            .plugin_action(&name2, "start")
            .expect_err("没写 on_action 要报错");
        assert!(
            err.contains("未定义"),
            "没写的场合要说「未定义」，实得：{err}"
        );
        assert!(
            pm.refresh_view(&name2).is_ok(),
            "没定义 get_view 是正常形态，不许变成失败"
        );
    }

    /// `init` 与 `cleanup` 被写成别的东西时必须出声，不能安静地不跑。
    ///
    /// 这两处连报错的出口都没有：加载那步吃 `has_fn("init")`（不是函数就当没写），
    /// 卸载那步是 `if let Ok(cleanup) = ...`。于是 `cleanup = "shutdown"` 的插件
    /// 停用之后一个字的痕迹都不留，而这条卸载路径上归还调度线程的动作**只有** cleanup 里那句
    /// （见 `host.rs` 的 owner 表）—— 没跑就是不还，本进程再也回收不掉。
    ///
    /// 反向腿：真没写 init/cleanup 的插件不许被这两条误伤（那是绝大多数插件）。
    #[test]
    fn init_and_cleanup_written_as_a_non_function_say_so() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("hook_init_cleanup");
        let bad = write_lua(
            &tmp.path().join("plugins"),
            "badinit",
            r###"
PLUGIN_NAME = "钩子写错"
init = "setup"
cleanup = "shutdown"
"###,
        );
        let mut pm = manager_in(tmp.path());
        let (name, logs) = crate::logger::capture_logs(|| {
            pm.load_plugin(&bad, PluginSource::Builtin)
                .expect("该装得上")
        });
        assert!(
            logs.iter()
                .any(|l| l.contains("init") && l.contains("不是函数")),
            "init 不是函数却没跑，要有一条话指着它，实得日志：{logs:?}"
        );
        let (_, logs) = crate::logger::capture_logs(|| {
            pm.unload_plugin(&name);
        });
        assert!(
            logs.iter()
                .any(|l| l.contains("cleanup") && l.contains("不是函数")),
            "cleanup 没跑等于没归还调度线程，必须出声，实得日志：{logs:?}"
        );

        // 反向腿：两个钩子都没写的插件，这两条一句话都不该说。
        let none = write_lua(
            &tmp.path().join("plugins"),
            "nohooks",
            "PLUGIN_NAME = \"什么都没有\"\n",
        );
        let (_, logs) = crate::logger::capture_logs(|| {
            let n = pm
                .load_plugin(&none, PluginSource::Builtin)
                .expect("该装得上");
            pm.unload_plugin(&n);
        });
        assert!(
            !logs.iter().any(|l| l.contains("不是函数")),
            "没写钩子是正常形态，不许喊：{logs:?}"
        );
    }

    /// 攒批交来的两条增量，进 VM 只该有两次，而且每次都带着这一批的总数。
    ///
    /// 这条盯的是"面板那一栏不再是 0"的实质：钩子真的收到 `(键名, 增量)` —— 不是逐键一次
    /// （那等于把每键进 VM 的代价请回按键热路径），也不是只收到键名（那数会少报成"出现过几种键"）。
    #[test]
    fn a_batch_of_two_keys_reaches_the_hook_twice_with_its_counts() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("key_batch_counts");
        let file = write_lua(
            &tmp.path().join("plugins"),
            "kb_batch",
            r###"
PLUGIN_NAME = "按键攒批"
function record_key(key, count)
  focusflow.log("kb-line key=" .. tostring(key) .. " count=" .. tostring(count))
end
"###,
        );
        let mut pm = manager_in(tmp.path());
        pm.load_plugin(&file, PluginSource::Builtin)
            .expect("夹具该加载得动");
        let names = pm.loaded_plugin_names();
        assert!(
            names.iter().any(|n| n == "按键攒批"),
            "已加载名单里没有它: {names:?}"
        );
        // 攒批那一头交过来的形状：两个键各按了若干次。这里直接摆出来，不绕件口 ——
        // 件口是进程全局的，同一个二进制里别的用例一落库就往里投，拿它当被测对象会互相污染
        // （完整跑里红过一次：捞到别人留下的 ("A", 8)）。件口自己的相加与取空由
        // `db::writer` 那侧的用例钉。
        let batches = vec![("A".to_string(), 10i64), ("B".to_string(), 7i64)];
        let name = "按键攒批".to_string();
        let (_, logs) = crate::logger::capture_logs(|| {
            for (k, c) in &batches {
                pm.plugin_key_event(&name, k, *c);
            }
        });
        let hits: Vec<&String> = logs.iter().filter(|l| l.contains("kb-line key=")).collect();
        assert_eq!(
            hits.len(),
            2,
            "两个键该只有两次钩子（不是 17 次），实得 {hits:?}"
        );
        assert!(
            hits.iter()
                .any(|l| l.contains("key=A") && l.contains("count=10")),
            "A 这一批的增量没带过去: {hits:?}"
        );
        assert!(
            hits.iter()
                .any(|l| l.contains("key=B") && l.contains("count=7")),
            "B 这一批的增量没带过去: {hits:?}"
        );
        assert!(
            !logs
                .iter()
                .any(|l| l.contains("收按键失败") || l.contains("record_key 抛错")),
            "钩子跑通了却还在出声: {logs:?}"
        );
    }

    /// 插件的 `record_key` 抛错时，按键停摆必须看得见。
    ///
    /// 同一族里 `on_action` 与 `set_field` 都把原因 `return Err` 给点按钮的那个人，
    /// 唯独按键投递的返回值是 `()`、调用方是键盘分发线程 —— 于是非限额的报错过去整包丢掉：
    /// 番茄钟插件的按键数从第一次抛错起再也不涨，而插件页仍写着已加载、一行原因都没有
    /// （用户看到的只是"这个面板不数我的按键了"）。
    #[test]
    fn a_key_hook_that_throws_leaves_a_reason_on_the_plugin_page() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("key_hook_throws");
        let file = write_lua(
            &tmp.path().join("plugins"),
            "boom",
            r###"
PLUGIN_NAME = "按键会炸"
function record_key(k)
  local t = nil
  return t.x
end
"###,
        );
        let mut pm = manager_in(tmp.path());
        let name = pm
            .load_plugin(&file, PluginSource::Builtin)
            .expect("插件本身该装得上");

        let (_, logs) = crate::logger::capture_logs(|| pm.plugin_key_event(&name, "A", 1));
        let info = pm.get_plugin(&name).expect("插件应还在表里");
        assert!(info.loaded, "一次表达式写错不该没收整个面板");
        let reason = info
            .error
            .clone()
            .expect("按键停摆要在插件页上看得见，不能只剩一片安静");
        assert!(
            reason.contains("record_key"),
            "要说清是哪个钩子坏了: {reason}"
        );
        assert!(reason.contains("按键数"), "还得说清现在的症状: {reason}");
        assert!(
            logs.iter().any(|l| l.contains("收按键失败")),
            "第一次要出声，否则这一刻只有用户在猜: {logs:?}"
        );

        // 观察量：这条是每个按键调一次，已经挂过原因就不许再刷日志
        let (_, logs) = crate::logger::capture_logs(|| {
            for i in 0..50 {
                pm.plugin_key_event(&name, &format!("K{i}"), 1);
            }
        });
        assert_eq!(
            logs.iter().filter(|l| l.contains("收按键失败")).count(),
            0,
            "同一件事不该每按一键喊一次: {logs:?}"
        );
        assert!(
            pm.get_plugin(&name).expect("插件应还在表里").loaded,
            "五十次抛错之后仍然不该停用"
        );
    }

    /// 跑满指令上限那一类**仍然**停用整个插件。
    ///
    /// 新加的那一支只挂原因、不停用；这条腿防的是"把这条弱化口径一路带下去"，
    /// 因为限额命中的语义是"这个 Lua 环境已经不可信"（`is_limit_error` 的注释写着）。
    #[test]
    fn an_over_limit_key_hook_is_still_disabled_not_just_annotated() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("key_hook_limit");
        std::fs::write(
            tmp.path().join("config.ini"),
            "[plugins]\ninstruction_limit = 3000\n",
        )
        .unwrap();
        let file = write_lua(
            &tmp.path().join("plugins"),
            "hog",
            r###"
PLUGIN_NAME = "按键跑飞"
function record_key(k)
  local n = 0
  for i = 1, 200000 do n = n + i end
  return n
end
"###,
        );
        let mut pm = manager_in(tmp.path());
        let name = pm
            .load_plugin(&file, PluginSource::Builtin)
            .expect("插件本身该装得上");

        let (_, logs) = crate::logger::capture_logs(|| pm.plugin_key_event(&name, "A", 1));
        let info = pm.get_plugin(&name).expect("插件应还在表里");
        assert!(
            !info.loaded,
            "限额命中必须停用，不能只挂个原因继续复用这个 Lua 环境"
        );
        let reason = info.error.clone().expect("停用的原因也要写出来");
        assert!(reason.contains("record_key 失败"), "{reason}");
        assert!(
            logs.iter().any(|l| l.contains("插件已停用")),
            "限额那一支走的是停用，日志要这么说: {logs:?}"
        );
        assert!(
            !logs.iter().any(|l| l.contains("收按键失败")),
            "限额那一支不该被新加的弱口径接走: {logs:?}"
        );
    }

    /// 三条按键失败之后只停**按键这一路**：插件不卸、视图照出，且不再进 Lua。
    #[test]
    fn three_key_failures_stop_only_the_key_route() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("key_route_trip");
        let file = write_lua(
            &tmp.path().join("plugins"),
            "keytrip",
            r###"
PLUGIN_NAME = "按键连坏"
count = 0
function record_key(k)
  count = count + 1
  local t = nil
  return t.x
end
function get_view()
  return { widgets = { { type = "label", text = tostring(count) } } }
end
"###,
        );
        let mut pm = manager_in(tmp.path());
        let name = pm
            .load_plugin(&file, PluginSource::Builtin)
            .expect("插件该装得上");
        for i in 0..3 {
            pm.plugin_key_event(&name, "A", 1);
            let _ = i;
        }
        let info = pm.get_plugin(&name).expect("插件还在");
        assert!(info.loaded, "按键那一路出错不该没收整个插件");
        let reason = info.error.clone().expect("停了要有原因");
        assert!(
            reason.contains("按键采集已停") && reason.contains("record_key"),
            "{reason}"
        );

        // 再按十次也不再进 Lua：视图里的计数停在 3。
        for _ in 0..10 {
            pm.plugin_key_event(&name, "A", 1);
        }
        pm.refresh_view(&name).expect("视图这一路还必须能跑");
        let text = match &pm
            .get_plugin(&name)
            .expect("插件还在")
            .view
            .as_ref()
            .expect("视图该在")
            .widgets[0]
        {
            crate::plugins::Widget::Label(text) => text.clone(),
            other => panic!("夹具应当是 label，实得 {other:?}"),
        };
        assert_eq!(text, "3", "停了之后不许再进 record_key");
    }

    /// 视图连续失败三次 → 停这一路；之后不再进 Lua，插件本身仍在。
    #[test]
    fn three_view_failures_stop_only_the_view_route() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("view_route_trip");
        let file = write_lua(
            &tmp.path().join("plugins"),
            "viewtrip",
            r###"
PLUGIN_NAME = "视图连坏"
function get_view()
  local t = nil
  return t.x
end
"###,
        );
        let mut pm = manager_in(tmp.path());
        let name = pm
            .load_plugin(&file, PluginSource::Builtin)
            .expect("插件该装得上");
        let mut last = String::new();
        for _ in 0..3 {
            last = pm.refresh_view(&name).expect_err("三次都该失败");
        }
        assert!(last.contains("视图刷新已停"), "{last}");
        // 失败的三次也是真跑过的三次：那一档的"共跑过多少次"要是 0，详情页就会把
        // 一个一直在失败的插件念成「没跑过」——与"这一类没接线"是同一句话。
        let calls = plugin_health::board_for("viewtrip")
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .samples(plugin_health::HookKind::View)
            .map(|s| s.calls())
            .unwrap_or(0);
        assert_eq!(calls, 3, "失败的重建没进耗时台账");
        let (_, logs) = crate::logger::capture_logs(|| {
            for _ in 0..5 {
                let _ = pm.refresh_view(&name);
            }
        });
        assert!(
            !logs.iter().any(|l| l.contains("get_view")),
            "停了之后不该再进 Lua: {logs:?}"
        );
        assert!(
            pm.get_plugin(&name).expect("插件还在").loaded,
            "停一路不等于停整个插件"
        );
    }

    /// 上限只有一处出处，而且是夹过的：页面与 VM 不许各说一套。
    ///
    /// 原先页面拿原始字符串自己 parse（不夹），于是 `memory_limit_mb = 999999` 时
    /// 屏上说 999999 MB、VM 实际 4096 MB；写成 0 时屏上说 0、VM 是 1。
    #[test]
    fn the_memory_cap_is_clamped_in_one_place() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("mem_cap");
        let loaded = |text: &str| {
            std::fs::write(tmp.path().join("config.ini"), text).unwrap();
            FocusFlowConfig::load(tmp.path().join("config.ini")).unwrap()
        };
        assert_eq!(
            memory_limit_mb(&loaded("[plugins]\nmemory_limit_mb = 999999\n")),
            4096,
            "天文数字要按笔误夹住"
        );
        assert_eq!(
            memory_limit_mb(&loaded("[plugins]\nmemory_limit_mb = 0\n")),
            1,
            "0 会让每个插件立刻 MemoryError"
        );
        assert_eq!(
            memory_limit_mb(&loaded("[plugins]\nmemory_limit_mb = 很多\n")),
            16,
            "读不出来按默认值"
        );
    }

    /// 插件文件已经没了（卸载时选了「删除设置」、或用户自己删了文件）⇒ 卸载那条路
    /// 只摘格子、不落盘：`forget` 那一笔会把刚被清掉的 `plugins/state/<stem>/` 建回来。
    #[test]
    fn unloading_a_deleted_plugin_does_not_recreate_its_state_directory() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("unload_gone_state");
        let file = write_lua(
            &tmp.path().join("plugins"),
            "gonep",
            "PLUGIN_NAME = \"没了\"\nfunction get_view() return { widgets = {} } end\n",
        );
        let mut pm = manager_in(tmp.path());
        let name = pm
            .load_plugin(&file, PluginSource::Builtin)
            .expect("插件该装得上");
        pm.refresh_view(&name).expect("视图该刷得动");
        let stem = "gonep";
        assert!(
            !paths::plugin_state_path(stem).exists(),
            "夹具前提：这一格还没写过 state"
        );
        std::fs::remove_file(&file).expect("删掉插件文件");
        assert!(pm.unload_by_stem(stem), "文件没了也该能按 stem 卸");
        assert!(
            !paths::plugin_state_path(stem).exists(),
            "卸载把刚清掉的 state 目录又建回来了"
        );
    }

    /// 定时/事件回调连续失败三次 → 停这一路，后续拍不再投。
    #[test]
    fn three_event_failures_stop_only_the_event_route() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("event_route_trip");
        let file = write_lua(
            &tmp.path().join("plugins"),
            "evtrip",
            r###"
PLUGIN_NAME = "定时连坏"
function init()
  for i = 1, 3 do
    focusflow.after(1, function()
      local t = nil
      return t.x
    end)
  end
end
function get_view()
  return { widgets = { { type = "label", text = "ok" } } }
end
"###,
        );
        let mut pm = manager_in(tmp.path());
        let name = pm
            .load_plugin(&file, PluginSource::Builtin)
            .expect("插件该装得上");
        // 定时器最短 1 秒：等过这一秒，三格在同一拍里一起到。
        std::thread::sleep(std::time::Duration::from_millis(1_100));
        pm.pump_timers();
        let reason = pm
            .get_plugin(&name)
            .and_then(|i| i.error.clone())
            .expect("停了要留原因");
        assert!(reason.contains("定时/事件回调已停"), "{reason}");
        let again = pm.pump_timers();
        assert!(again.is_empty(), "停了之后不该再投: {again:?}");
        pm.refresh_view(&name).expect("视图这一路还必须能跑");
    }

    /// 「这个插件不收按键」是正常形态，不许因此出声、也不许挂原因。
    ///
    /// 反向腿：新加的三态里 `nil`（没定义这个钩子）必须仍然安静 —— 多数插件都没有
    /// `record_key`，一次按键给每个插件各喊一句红字的话，这条链就再没人看了。
    #[test]
    fn a_plugin_without_a_key_hook_stays_quiet() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("key_hook_absent");
        let file = write_lua(&tmp.path().join("plugins"), "quiet", &view_only("不收按键"));
        let mut pm = manager_in(tmp.path());
        let name = pm
            .load_plugin(&file, PluginSource::Builtin)
            .expect("插件该装得上");

        let (_, logs) = crate::logger::capture_logs(|| pm.plugin_key_event(&name, "A", 1));
        let info = pm.get_plugin(&name).expect("插件应还在表里");
        assert!(
            info.error.is_none(),
            "没有这个钩子不是失败: {:?}",
            info.error
        );
        assert!(info.loaded);
        assert!(
            !logs.iter().any(|l| l.contains("收按键失败")),
            "没定义钩子不该出声: {logs:?}"
        );
    }

    /// 同名存在但**不是函数**：过去与"没定义"共用同一个静默出口。
    ///
    /// `record_key = "..."` 这类笔误的症状与抛错那条一模一样（按键数永远不涨、
    /// 页面说健康），所以它得有自己的那句原因 —— 而这条又不能被"没定义"那条腿吞掉。
    #[test]
    fn a_key_hook_that_is_not_a_function_says_so() {
        let _lock = paths::test_app_dir_lock();
        let tmp = paths::test_app_dir("key_hook_not_function");
        let file = write_lua(
            &tmp.path().join("plugins"),
            "str",
            "PLUGIN_NAME = \"钩子写成字符串\"\nrecord_key = \"我把它写成字符串了\"\n",
        );
        let mut pm = manager_in(tmp.path());
        let name = pm
            .load_plugin(&file, PluginSource::Builtin)
            .expect("插件该装得上");

        let (_, logs) = crate::logger::capture_logs(|| pm.plugin_key_event(&name, "A", 1));
        let info = pm.get_plugin(&name).expect("插件应还在表里");
        let reason = info.error.clone().expect("非函数的钩子要有原因");
        assert!(reason.contains("不是函数"), "{reason}");
        assert!(info.loaded, "写错类型也只挂原因，不停用");
        assert!(
            logs.iter().any(|l| l.contains("收按键失败")),
            "这一支同样要出声: {logs:?}"
        );

        // 形状腿：四个出口各有归属，谁被摘掉都要红
        let src = include_str!("manager.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let flat: String = prod
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        assert_eq!(
            flat.matches("self.note_key_hook_failure(").count(),
            3,
            "非函数、取不出、非限额抛错三条都要挂原因，多一条少一条都要查"
        );
        // 判据里不写引号：这段要在**去掉全部空白**的源码上比，带引号的片段一旦被转义写错
        // 就是语法错，而它的形状本身（停用走 `mark_plugin_error`）已经够用。
        assert_eq!(
            flat.matches("self.mark_plugin_error(name,format!(").count(),
            1,
            "限额那一支必须仍然走停用，多一处停用要说清是谁"
        );
        assert!(
            flat.contains("record_key失败:"),
            "停用的那句原因还是要点名 record_key"
        );
    }

    /// 回收路径只有那一个名字（方案 3.11 的名册）。
    ///
    /// 三条路各查一遍：出错停用、正常卸载、加载失败回滚 —— 从前这三处各写着
    /// `host::release_scheduler` 与 `shutdown_pomodoro_if_owned_by`（第三处还漏了番茄钟），
    /// 加第四份共享资源时就要三处各补一行，早晚有一处漏。现在都收成 `release_all`，
    /// 而"名册里每份都被叫到"由 `resources` 自己的用例钉。
    #[test]
    fn every_recycle_path_releases_the_whole_roster() {
        let src = include_str!("manager.rs");
        let call = ["resources::release_all", "(&o"].concat();
        let call_stem = ["resources::release_all", "(&s"].concat();
        for (site, needle) in [
            ("fn mark_plugin_error", call.as_str()),
            ("fn unload_plugin", call_stem.as_str()),
            ("fn try_load", call_stem.as_str()),
        ] {
            let at = src.find(site).unwrap_or_else(|| panic!("{site} 不见了"));
            let rest = &src[at..];
            let cut = ["\n    fn ", "\n    pub fn "]
                .iter()
                .filter_map(|p| rest[2..].find(p).map(|i| i + 2))
                .min()
                .unwrap_or(rest.len());
            let body: String = rest[..cut]
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("")
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            assert!(
                body.contains(needle),
                "{site} 这条回收路径没有按名册一次叫完：那份资源会为已停用的插件继续跑"
            );
        }
        // 旧的"各叫各的"写法不许在某一条路上偷偷回来
        for site in ["fn mark_plugin_error", "fn unload_plugin"] {
            let at = src.find(site).expect("回收路径的函数头不见了");
            let rest = &src[at..];
            let cut = ["\n    fn ", "\n    pub fn "]
                .iter()
                .filter_map(|p| rest[2..].find(p).map(|i| i + 2))
                .min()
                .unwrap_or(rest.len());
            let body: String = rest[..cut]
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("")
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            assert!(
                !body.contains("host::release_scheduler(&"),
                "{site} 又自己去还调度线程了：这一份该由名册叫"
            );
        }
    }
}
