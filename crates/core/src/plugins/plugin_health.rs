//! 每类钩子的耗时台账、慢预算计数与错误历史（方案 3.12 那张预算表 + 3.9 那句"记录写进 health.json"）。
//!
//! 这一格要治的两件事，都是屏上看得见的：
//! · **只测 `get_view` 会漏掉最要命的一类。** `record_key` 每 10 秒被投递一批，它慢会拖住
//!   攒批的那条路径；`on_action` 慢一下是用户点下去那一下的卡顿。四类各有一份预算，
//!   各自的窗口自己数（3.12 那张表把四个数写死了：2 ms / 5 ms / 0.2 ms / 2 ms）。
//! · **账本重启清零。** 今天"降了几次频、为什么被停用"只活在内存里，用户重启就查不到，
//!   而这三样正是详情页与排障要念的那几句。所以台账能折成一份 `health.json` 落盘、
//!   再在加载时读回来。
//!
//! 三条分工纪律：
//! · 时间由调用方注入（`Duration` 与一个给人看的时刻串），本模块**不读墙钟** ——
//!   否则判据只能靠真等，而"等了多久"在测试机上是不定的；
//! · 窗口那份数复用 [`crate::plugins::plugin_refresh::SAMPLE_WINDOW`]，不在这里另抄一个 20；
//! · 每一格的物理路径只由 stem 决定，消息正文一律截断 —— 这一份文件不该长成第二个 `state.json`。

use crate::paths;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// 预算：四类钩子各一个数，单位微秒（0.2 ms 那一档用毫秒会写成 0）。
///
/// 这四个数是"插件慢不慢"的唯一尺子（方案第 15 节自己这么说的），改动必须是有意的一次。
pub const BUDGET_VIEW_US: u64 = 2_000;
pub const BUDGET_ACTION_US: u64 = 5_000;
pub const BUDGET_KEY_US: u64 = 200;
pub const BUDGET_EVENT_US: u64 = 2_000;

/// 错误历史留几条。窗口与慢熔断那个 20 是同一族，但语义不同：那一份数的是样本，
/// 这一份数的是"用户往回能看到几条红字"。
pub const ERROR_HISTORY: usize = 20;

/// 一条错误消息最多留多少字符（截断处补 `…`）。
pub const ERROR_MSG_MAX_CHARS: usize = 200;

/// 落盘那份文档的格式版本（读的时候只认现有的一档，认不得的不猜）。
pub const HEALTH_DOC_VERSION: u32 = 1;

const HEALTH_FILE: &str = "health.json";

/// 页面那一行的汇总最多多少个字符。
///
/// 这一页的字体全是 `NO_WRAP`，而 `DrawText` 不按矩形裁切 —— 一句没有上界的动态文本会一路
/// 画到窗口外（本仓为同一件事改过"两条长路径拼进一句"那一处）。行上那格的宽度是个固定
/// 表达式，所以"反正会被夹掉"不算保护：夹在哪一处由读的人决定，不如写的人自己停在界上。
pub const SUMMARY_MAX_CHARS: usize = 88;

/// 四类钩子。方案 3.12 那张表里的四行，一行一档。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookKind {
    /// 面板重建那一次 `get_view()`
    View,
    /// `on_action(id)` 与 `set_field(k, v)` 共用一档（表里写在同一行）
    Action,
    /// `record_key(k, n)`，按**批**计时而不是按单键
    KeyEvent,
    /// `on("tick")` / `on("day_change")` / `on("app_change")` 的回调
    Event,
}

impl HookKind {
    /// 这一档的预算（微秒）。
    pub fn budget_us(self) -> u64 {
        match self {
            HookKind::View => BUDGET_VIEW_US,
            HookKind::Action => BUDGET_ACTION_US,
            HookKind::KeyEvent => BUDGET_KEY_US,
            HookKind::Event => BUDGET_EVENT_US,
        }
    }

    /// 落盘与命令行用的那个稳定名（不是给人看的那一句）。
    pub fn keyword(self) -> &'static str {
        match self {
            HookKind::View => "view",
            HookKind::Action => "action",
            HookKind::KeyEvent => "key_event",
            HookKind::Event => "event",
        }
    }

    /// 给人看的那一句（页面上与工具输出用）。
    pub fn label(self) -> &'static str {
        match self {
            HookKind::View => "视图重建",
            HookKind::Action => "动作与字段",
            HookKind::KeyEvent => "按键批次",
            HookKind::Event => "事件回调",
        }
    }

    /// 四类都在这一份里 —— 详情页与落盘都按这一份走，不各抄一份清单。
    pub const ALL: [HookKind; 4] = [
        HookKind::View,
        HookKind::Action,
        HookKind::KeyEvent,
        HookKind::Event,
    ];

    fn from_keyword(word: &str) -> Option<Self> {
        HookKind::ALL.into_iter().find(|k| k.keyword() == word)
    }
}

/// 一类钩子的样本窗口。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Samples {
    /// 最近 N 次的耗时（微秒），新的在尾巴上
    #[serde(default)]
    recent: VecDeque<u64>,
    /// 窗口里超预算的那几次
    #[serde(default)]
    over_budget: usize,
    /// 一共跑过多少次（不被窗口挤掉的那部分账）
    #[serde(default)]
    calls: u64,
}

impl Samples {
    /// 记一次耗时，回一句"这一次超了本档的预算没有"。
    pub fn record(&mut self, elapsed: Duration, budget_us: u64) -> bool {
        let micros = micros_of(elapsed);
        self.calls += 1;
        if self.recent.len() >= crate::plugins::plugin_refresh::SAMPLE_WINDOW {
            // 挤掉最旧那一条时，它的"超预算"账也要一起挤掉：窗口滚掉了，标记就该滚掉
            // （慢熔断那一档自己会解除，理由同一条用例里写着）。
            if let Some(old) = self.recent.pop_front() {
                if old > budget_us {
                    self.over_budget = self.over_budget.saturating_sub(1);
                }
            }
        }
        self.recent.push_back(micros);
        let over = micros > budget_us;
        if over {
            self.over_budget += 1;
        }
        over
    }

    pub fn last_us(&self) -> Option<u64> {
        self.recent.back().copied()
    }

    /// 窗口里的 p95（样本不足一档时给出的是"最多 95% 位置"那一个，方案第 15 节
    /// 本来就把这几个数标成待标定 —— 这里不假装它是统计学意义上的 p95）。
    pub fn p95_us(&self) -> Option<u64> {
        if self.recent.is_empty() {
            return None;
        }
        let mut sorted: Vec<u64> = self.recent.iter().copied().collect();
        sorted.sort_unstable();
        let idx = ((sorted.len() as f64 - 1.0) * 0.95).round() as usize;
        sorted.get(idx.min(sorted.len() - 1)).copied()
    }

    pub fn over_budget(&self) -> usize {
        self.over_budget
    }

    pub fn calls(&self) -> u64 {
        self.calls
    }

    pub fn len(&self) -> usize {
        self.recent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.recent.is_empty()
    }
}

/// 一条错误/停用记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorNote {
    /// 给人看的时刻串（由调用方给，本模块不读墙钟）
    pub at: String,
    /// 原因（写入前按 [`ERROR_MSG_MAX_CHARS`] 截断）
    pub msg: String,
}

/// 一个插件的台账。
#[derive(Debug, Default, Clone)]
pub struct Health {
    per_kind: HashMap<HookKind, Samples>,
    errors: VecDeque<ErrorNote>,
    /// 被降频的那几次（每次从"正常"翻到"慢"算一次）
    throttle_flips: u32,
    /// 被停用的那几次（`mark_plugin_error` 那一路）
    stops: u32,
}

impl Health {
    /// 记一次某类钩子的耗时，回"这一次超预算没有"。
    pub fn record(&mut self, kind: HookKind, elapsed: Duration) -> bool {
        self.per_kind
            .entry(kind)
            .or_default()
            .record(elapsed, kind.budget_us())
    }

    /// 记一条错误/停用原因（新的在尾巴上，超过 [`ERROR_HISTORY`] 挤掉最旧那一条）。
    pub fn note_error(&mut self, at: &str, msg: &str) {
        if self.errors.len() >= ERROR_HISTORY {
            self.errors.pop_front();
        }
        self.errors.push_back(ErrorNote {
            at: at.to_string(),
            msg: truncate_chars(msg, ERROR_MSG_MAX_CHARS),
        });
    }

    /// 记一次"被停用"（与那条错误各记各的：停用是动作，原因是话）。
    pub fn note_stop(&mut self) {
        self.stops += 1;
    }

    pub fn note_throttle(&mut self) {
        self.throttle_flips += 1;
    }

    pub fn samples(&self, kind: HookKind) -> Option<&Samples> {
        self.per_kind.get(&kind)
    }

    pub fn errors(&self) -> Vec<&ErrorNote> {
        self.errors.iter().collect()
    }

    pub fn throttle_flips(&self) -> u32 {
        self.throttle_flips
    }

    pub fn stops(&self) -> u32 {
        self.stops
    }

    /// 折成落盘那一份。四类**一律写出来**（没跑过的那一档写 0 次而不是省掉那一格）：
    /// 读的一侧要能区分"这一类没跑过"与"这一类没登记"。
    pub fn to_doc(&self) -> HealthDoc {
        let mut hooks = Vec::new();
        for kind in HookKind::ALL {
            let s = self.per_kind.get(&kind).cloned().unwrap_or_default();
            hooks.push(HookDoc {
                kind: kind.keyword().to_string(),
                calls: s.calls,
                last_us: s.last_us().unwrap_or(0),
                p95_us: s.p95_us().unwrap_or(0),
                over_budget: s.over_budget() as u64,
            });
        }
        HealthDoc {
            version: HEALTH_DOC_VERSION,
            hooks,
            errors: self.errors.iter().cloned().collect(),
            throttle_flips: self.throttle_flips,
            stops: self.stops,
        }
    }

    /// 从落盘那一份读回来（重启后账不清零就靠这一步）。
    pub fn from_doc(doc: &HealthDoc) -> Health {
        let mut health = Health {
            throttle_flips: doc.throttle_flips,
            stops: doc.stops,
            ..Default::default()
        };
        for h in &doc.hooks {
            if let Some(kind) = HookKind::from_keyword(&h.kind) {
                health.per_kind.insert(
                    kind,
                    Samples {
                        // 窗口里的逐条耗时不落盘（落的是 last/p95 两个读数），
                        // 读回来时窗口从 p95 那一个数重建：够页面说话，也够下一拍的窗口判断继续滚。
                        recent: if h.calls > 0 {
                            VecDeque::from([h.last_us, h.p95_us])
                        } else {
                            VecDeque::new()
                        },
                        over_budget: h.over_budget as usize,
                        calls: h.calls,
                    },
                );
            }
        }
        health.errors = doc.errors.iter().cloned().collect();
        health
    }
}

/// 落盘的一份文档。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthDoc {
    pub version: u32,
    pub hooks: Vec<HookDoc>,
    pub errors: Vec<ErrorNote>,
    pub throttle_flips: u32,
    pub stops: u32,
}

/// 一类钩子的落盘读数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookDoc {
    pub kind: String,
    pub calls: u64,
    pub last_us: u64,
    pub p95_us: u64,
    pub over_budget: u64,
}

impl Default for HealthDoc {
    fn default() -> Self {
        Self {
            version: HEALTH_DOC_VERSION,
            hooks: Vec::new(),
            errors: Vec::new(),
            throttle_flips: 0,
            stops: 0,
        }
    }
}

impl HealthDoc {
    pub fn hook(&self, kind: HookKind) -> Option<&HookDoc> {
        self.hooks.iter().find(|h| h.kind == kind.keyword())
    }

    /// 这一份账有没有可说的：任何一类跑过、被降过频、被停用过，或留着错误历史。
    ///
    /// 「有没有东西可说」这条判断只许有这一份出处 —— 页面那一格与 `summary_for` 各写一个
    /// 空判据，迟早一边把"只有停用记录"当成有话、另一边当成没话。
    pub fn has_anything(&self) -> bool {
        self.hooks.iter().any(|h| h.calls > 0)
            || self.throttle_flips > 0
            || self.stops > 0
            || !self.errors.is_empty()
    }
}

/// 插件页那一行要念的那一句；**没可说的时回 `None`**。
///
/// 页面上不该为了一句"这一格还没有记录"给每个插件多占一格 —— 那正是"没有"与
/// "还没轮到它跑"混成同一句的那类噪声（本仓在打卡条与周报上修过同源的两处）。
/// 有内容时交回 [`summary_line`] 那一份，句子只有一处写法。
pub fn summary_for(stem: &str) -> Option<String> {
    let doc = board_for(stem)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .to_doc();
    if doc.has_anything() {
        Some(summary_line(&doc))
    } else {
        None
    }
}

/// 页面与工具念的那一句。四类都没跑过、也没错误时给的是"没有记录"，
/// 不是空串——空串在屏上与"这一格没接上"分不开。
pub fn summary_line(doc: &HealthDoc) -> String {
    if !doc.has_anything() {
        return "这一格还没有记录".to_string();
    }
    // 先"要动它的那半句"，再读数：一句被夹在界上时，留下的应该是让人知道下一步做什么的那部分。
    // 原因正文不在这一行里 —— 行上只报**几条**，具体那几句话属于详情页（那里列错误历史）。
    let mut head: Vec<String> = Vec::new();
    if !doc.errors.is_empty() {
        head.push(format!("失败记录 {} 条", doc.errors.len()));
    }
    if doc.stops > 0 {
        head.push(format!("停用 {} 次", doc.stops));
    }
    if doc.throttle_flips > 0 {
        head.push(format!("降频 {} 次", doc.throttle_flips));
    }
    let mut body: Vec<String> = Vec::new();
    for kind in HookKind::ALL {
        let Some(h) = doc.hook(kind) else { continue };
        if h.calls == 0 {
            continue;
        }
        if h.over_budget > 0 {
            body.push(format!(
                "{}超预算 {} 次（p95 {:.1} ms）",
                kind.label(),
                h.over_budget,
                h.p95_us as f64 / 1000.0
            ));
        } else {
            body.push(format!(
                "{} p95 {:.1} ms",
                kind.label(),
                h.p95_us as f64 / 1000.0
            ));
        }
    }
    let all: Vec<&String> = head.iter().chain(body.iter()).collect();
    let joined = all
        .into_iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(" · ");
    let chars: Vec<char> = joined.chars().collect();
    if chars.len() <= SUMMARY_MAX_CHARS {
        return joined;
    }
    // 自己停在界上，而不是让 DrawText 夹出去：夹出去的那一半是谁都猜不到的
    let mut clipped: String = chars.iter().take(SUMMARY_MAX_CHARS - 1).collect();
    clipped.push('…');
    clipped
}

/// 时长转微秒：亚微秒的那一段折成 0（预算最小的那一档也有 200 µs，不会把"刚好"折成"超"）。
fn micros_of(elapsed: Duration) -> u64 {
    elapsed.as_micros().min(u64::MAX as u128) as u64
}

/// 按**字符**截断（不是按字节 —— 中文原因按字节切会切在字符中间，那是 panic 的写法）。
fn truncate_chars(msg: &str, max: usize) -> String {
    let chars: Vec<char> = msg.chars().collect();
    if chars.len() <= max {
        return msg.to_string();
    }
    let mut out: String = chars.iter().take(max).collect();
    out.push('…');
    out
}

type Registry = HashMap<String, Arc<Mutex<Health>>>;

static BOARDS: OnceLock<Mutex<Registry>> = OnceLock::new();

fn registry() -> &'static Mutex<Registry> {
    BOARDS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 这一份插件的台账（没有就建一格；**新建那一格会先读盘**）。
///
/// 键是文件名 stem，与定时器、存储、授权同一族设计：展示名会撞、句柄会换。
///
/// 读盘只发生在"本进程第一次碰这一格"那一次：账要能跨过重启还在（这一格存在的理由），
/// 又不能每问一次都读一次文件。所以本进程里已经建起来的，绝不重新从盘上盖一遍 ——
/// 盖一次的后果是这一跑新记的那几条没了。
pub fn board_for(stem: &str) -> Arc<Mutex<Health>> {
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(found) = reg.get(stem) {
        return found.clone();
    }
    let seeded = match read(stem) {
        Ok(Some(doc)) => Health::from_doc(&doc),
        // 读不出来（坏文件、版本认不得）不许把插件的台账整体当成"没有记录"，
        // 也不许让采集这一路失败：话留在日志，本进程从空的一格开始记。
        Ok(None) => Health::default(),
        Err(e) => {
            tracing::warn!("插件 {stem} 的 health.json 读不回来，这一跑从空台账开始记：{e}");
            Health::default()
        }
    };
    let cell = Arc::new(Mutex::new(seeded));
    reg.insert(stem.to_string(), cell.clone());
    cell
}

/// 把这一格的账落到盘上（不摘格子）。停用、卸载、面板关闭那三条路都要先叫这一句。
pub fn flush(stem: &str) -> Result<(), String> {
    let board = {
        let reg = registry().lock().unwrap_or_else(|e| e.into_inner());
        reg.get(stem).cloned()
    };
    let Some(board) = board else {
        return Ok(()); // 这一格没被碰过 = 没有要落的东西
    };
    let doc = board.lock().unwrap_or_else(|e| e.into_inner()).to_doc();
    write(stem, &doc)
}

/// 落盘并摘掉这一格（卸载、停用那两条路）。
///
/// 先落再摘：摘了之后这一跑的账就再也写不出去了，而"重启后还在"靠的正是这一笔。
/// 落失败要说一句（不是Err：账没了不该让卸载失败）。
pub fn forget(stem: &str) -> bool {
    if let Err(e) = flush(stem) {
        tracing::warn!("插件 {stem} 的台账落不下去: {e}");
    }
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(stem)
        .is_some()
}

/// 落盘的那份文件在哪：`plugins/state/<stem>/health.json`（与 grants/state 同一格）。
fn health_file(stem: &str) -> PathBuf {
    paths::plugin_state_path(stem).join(HEALTH_FILE)
}

/// 写一份台账。原子替换（`tmp` → `rename`），与 `plugin_store` 同一条写法：
/// 半截的 health.json 比没有更糟 —— 读的一侧会把"上次崩在这儿"读成别的话。
pub fn write(stem: &str, doc: &HealthDoc) -> Result<(), String> {
    let path = health_file(stem);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("{} 建不出来: {e}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(doc).map_err(|e| format!("台账排不出 JSON: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json.as_bytes())
        .map_err(|e| format!("{} 写不出去: {e}", tmp.display()))?;
    if path.exists() {
        std::fs::remove_file(&path).ok();
    }
    std::fs::rename(&tmp, &path)
        .map_err(|e| format!("{} 换不到 {}：{e}", tmp.display(), path.display()))
}

/// 读回一份台账。读不出来分两档说：没这份文件（`Ok(None)`，新插件的正常样子）与
/// 有文件但读不出（`Err`，那一格不能被当成"没有记录"）。
pub fn read(stem: &str) -> Result<Option<HealthDoc>, String> {
    let path = health_file(stem);
    match std::fs::read(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{} 读不出来: {e}", path.display())),
        Ok(bytes) => {
            let doc: HealthDoc = serde_json::from_slice(&bytes)
                .map_err(|e| format!("{} 不是合法的台账: {e}", path.display()))?;
            if doc.version != HEALTH_DOC_VERSION {
                return Err(format!(
                    "{} 的格式版本是 {}，本宿主只认 {HEALTH_DOC_VERSION}（不猜旧格式）",
                    path.display(),
                    doc.version
                ));
            }
            Ok(Some(doc))
        }
    }
}

use std::path::PathBuf;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::plugin_refresh::SAMPLE_WINDOW;

    fn us(n: u64) -> Duration {
        Duration::from_micros(n)
    }

    /// 四类钩子、四个预算，都要与方案那张表一字不差；漏一档的症状是"那一类永远不慢"。
    #[test]
    fn the_four_hook_kinds_each_carry_the_documented_budget() {
        assert_eq!(HookKind::ALL.len(), 4);
        assert_eq!(HookKind::View.budget_us(), 2_000);
        assert_eq!(HookKind::Action.budget_us(), 5_000);
        assert_eq!(HookKind::KeyEvent.budget_us(), 200);
        assert_eq!(HookKind::Event.budget_us(), 2_000);
        // 关键字要能往返（落盘写的是它，读回来认的也是它）
        for kind in HookKind::ALL {
            assert_eq!(HookKind::from_keyword(kind.keyword()), Some(kind));
        }
        assert_eq!(HookKind::from_keyword("teleport"), None);
    }

    /// 预算是**按档**比的，不是全表共用那一个 5 ms：同一次 3 ms 在视图那一档是超、
    /// 在动作那一档是不超。这条是"只测 get_view 会漏掉最要命的一类"的另一半。
    #[test]
    fn a_three_millisecond_call_is_slow_for_the_view_but_not_for_the_action() {
        let mut health = Health::default();
        assert!(
            health.record(HookKind::View, us(3_000)),
            "3 ms 比视图那 2 ms 的预算大，必须算超"
        );
        assert!(
            !health.record(HookKind::Action, us(3_000)),
            "3 ms 在动作那一档（5 ms）里不该算超"
        );
        assert!(
            health.record(HookKind::KeyEvent, us(3_000)),
            "3 ms 对按键批次（0.2 ms）是 15 倍，那一档最凶"
        );
        assert_eq!(
            health.samples(HookKind::View).map(|s| s.over_budget()),
            Some(1)
        );
        assert_eq!(
            health.samples(HookKind::Action).map(|s| s.over_budget()),
            Some(0),
            "共用一个阈值的话，这一格也会数成 1"
        );
    }

    /// 窗口滚掉最旧那一条时，它欠下的"超预算"账要一起滚掉（慢会自动解除那条的同一理由）。
    #[test]
    fn the_over_budget_count_leaves_with_the_window() {
        let mut samples = Samples::default();
        // 先塞满一窗全是超预算的
        for _ in 0..SAMPLE_WINDOW {
            samples.record(us(9_000), 2_000);
        }
        assert_eq!(samples.over_budget(), SAMPLE_WINDOW);
        // 再挤进同样多条都很快：旧的必须被顶出去
        for _ in 0..SAMPLE_WINDOW {
            samples.record(us(100), 2_000);
        }
        assert_eq!(samples.len(), SAMPLE_WINDOW, "窗口本身不该越长越大");
        assert_eq!(
            samples.over_budget(),
            0,
            "样本早滚掉了，超预算那一格还压着账 —— 这一档就永远解不除了"
        );
        assert_eq!(
            samples.calls(),
            (SAMPLE_WINDOW * 2) as u64,
            "总次数不该被窗口挤掉"
        );
    }

    /// p95 取的是排好序后 95% 那一个位置，不是最大值也不是平均值。
    #[test]
    fn the_p95_reads_the_ninety_fifth_percentile_of_the_window() {
        let mut samples = Samples::default();
        for i in 1..=20_u64 {
            samples.record(us(i * 100), 2_000);
        }
        // 100..2000 µs 二十条：95% 位置（(20-1)*0.95 = 18.05 → 18）是第 19 条 = 1900
        assert_eq!(samples.p95_us(), Some(1_900));
        assert_eq!(samples.last_us(), Some(2_000), "最近一次该是最后进去那一条");
    }

    /// 错误历史有界、留最新的、中文原因不被切在半截。
    #[test]
    fn the_error_history_keeps_the_newest_and_cuts_on_char_bounds() {
        let mut health = Health::default();
        for i in 0..(ERROR_HISTORY + 5) {
            health.note_error("07:00", &format!("第 {i} 条原因"));
        }
        let errs = health.errors();
        assert_eq!(errs.len(), ERROR_HISTORY, "历史该封顶");
        assert!(
            errs.last().unwrap().msg.contains("第 24 条"),
            "留的必须最新那几条"
        );
        assert!(
            !errs.first().unwrap().msg.contains("第 0 条"),
            "最旧的该被挤掉"
        );

        let long = "错".repeat(ERROR_MSG_MAX_CHARS + 50);
        let mut h2 = Health::default();
        h2.note_error("07:00", &long);
        let kept = h2.errors()[0].msg.chars().count();
        assert_eq!(
            kept,
            ERROR_MSG_MAX_CHARS + 1,
            "截断要停在字符上并留一个省略号（按字节切会在多字节中间断掉）"
        );
    }

    /// 落盘再读回来：计数、原因、四类读数都不该清零 —— 这条就是"重启清零"那个缺陷的验收。
    #[test]
    fn the_ledger_survives_a_restart_through_the_file() {
        // 程序目录是进程全局的：本仓的每一份用例都先拿这把锁，否则隔壁那条把
        // app_dir 换走了，这一条写进去的文件就"存在但看不见"（并行跑时红的是这一条）。
        let _lock = crate::paths::test_app_dir_lock();
        let dir = crate::paths::test_app_dir("health_roundtrip");
        let stem = "health_case";
        let mut health = Health::default();
        health.record(HookKind::View, us(3_000));
        health.record(HookKind::KeyEvent, us(400));
        health.note_error("07:31", "视图跑不出来：脚本第 3 行没有那个格子");
        health.note_throttle();
        health.note_stop();

        let doc = health.to_doc();
        // 四类一律写出来，没跑过的那一档也要在场
        assert_eq!(doc.hooks.len(), HookKind::ALL.len());
        assert_eq!(doc.hook(HookKind::Action).map(|h| h.calls), Some(0));
        assert_eq!(doc.hook(HookKind::View).map(|h| h.over_budget), Some(1));

        write(stem, &doc).expect("台账该落得下去");
        assert!(health_file(stem).exists(), "说了写成功却没文件");
        let back = read(stem)
            .expect("读回来这一步不该失败")
            .expect("刚落盘不该读成没有");
        assert_eq!(back, doc, "落盘再读回来必须与写出去的那一份逐字相等");
        let restored = Health::from_doc(&back);
        assert_eq!(restored.throttle_flips(), 1);
        assert_eq!(restored.stops(), 1);
        assert_eq!(restored.errors().len(), 1, "错误历史不该随重启清零");
        assert_eq!(
            restored.samples(HookKind::View).map(|s| s.over_budget()),
            Some(1)
        );
        // 台账那一格不能碰到授权文件（同一个目录里躺着 grants.json）
        assert!(
            !crate::paths::plugin_state_path(stem)
                .join("grants.json")
                .exists(),
            "health 那一格写到了别人的文件上"
        );
        drop(dir);
    }

    /// 「没这份文件」与「有文件但读不出」是两种结局，不许都折成"没有记录"。
    #[test]
    fn absent_is_not_the_same_as_unreadable() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("health_absent");
        let stem = "health_missing";
        assert!(
            read(stem).expect("没文件不该报错").is_none(),
            "新插件本来就没有这一格，那不是一个错"
        );
        let path = health_file("health_broken");
        std::fs::create_dir_all(path.parent().unwrap()).expect("状态目录");
        std::fs::write(&path, "不是 JSON").expect("写坏夹具");
        let err = read("health_broken").expect_err("坏文件要说成坏文件");
        assert!(err.contains("不是合法"), "读不出的那句要念出文件：{err}");
        // 版本号不认识的那一份也不猜
        std::fs::write(
            &path,
            r#"{"version":99,"hooks":[],"errors":[],"throttle_flips":0,"stops":0}"#,
        )
        .expect("写版本夹具");
        let err = read("health_broken").expect_err("更高的一档格式不该当本档读");
        assert!(err.contains("格式版本"), "认不得的版本要念出版本号：{err}");
    }

    /// 页面那一行只在有话可说时才要这一句：`summary_for` 对空账回 `None`。
    ///
    /// `None` 与空串不是一回事 —— 页面拿 `None` 是"这一格别画"，拿空串会画出一个孤零零的分隔点。
    /// 两档（跑过一次 / 只留了一条错误历史）都要算"有话"，因为它们走的是同一份判据。
    #[test]
    fn summary_for_is_quiet_only_when_there_is_nothing_to_say() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("health_quiet");
        assert_eq!(
            summary_for("quiet_case"),
            None,
            "全新的格子什么都没跑过，不该有话"
        );
        let board = board_for("quiet_case");
        board
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record(HookKind::View, Duration::from_micros(3_000));
        let said = summary_for("quiet_case").expect("跑过一次就该有话");
        assert!(
            said.contains("视图重建") && said.contains("超预算 1 次"),
            "{said}"
        );

        assert_eq!(summary_for("error_only"), None);
        board_for("error_only")
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .note_error("07:00", "只留了一条原因");
        assert!(
            summary_for("error_only")
                .expect("有历史就该有话")
                .contains("失败记录 1 条"),
            "只有错误历史的那一种被当成\"没话可说\" ⇒ 判据与页面各写了一份空判据"
        );
    }

    /// 汇总那句有上界，而且被夹时留下的是"要动它的那半句"。
    ///
    /// 这一页全是 `NO_WRAP`，动态文本没有上界就会画到窗口外（本仓改过同源的两处）。
    /// 段的先后按"下一步要不要动它"排：失败条数与停用/降频在前，各档 p95 读数在后。
    #[test]
    fn the_summary_line_is_bounded_and_keeps_the_actionable_half() {
        let mut health = Health::default();
        for kind in HookKind::ALL {
            // 每一档都超预算，四段读数拼起来一定越过界
            health.record(kind, Duration::from_micros(kind.budget_us() + 10));
        }
        health.note_throttle();
        health.note_stop();
        health.note_error("2026-11-04 07:31:09", "一条很长的原因");
        let line = summary_line(&health.to_doc());
        let chars = line.chars().count();
        assert!(
            chars <= SUMMARY_MAX_CHARS,
            "汇总那句没有上界（{chars} 个字符 > {SUMMARY_MAX_CHARS}）：{line}"
        );
        assert!(
            chars == SUMMARY_MAX_CHARS && line.ends_with('…'),
            "内容多到越界时该停在界上并留省略号，而不是让 DrawText 夹：{line}"
        );
        assert!(
            line.starts_with("失败记录 1 条"),
            "被夹的时候留下的该是\"要动它的那半句\"：{line}"
        );
        assert!(
            !line.contains("一条很长的原因"),
            "原因正文不进这一行（它属于详情页的错误历史那一栏）：{line}"
        );
    }

    /// 页面念的那一句：空台账要说"还没有记录"，而不是空串（空串在屏上与"没接上"分不开）。
    #[test]
    fn the_summary_line_says_something_for_every_state() {
        let mut health = Health::default();
        assert_eq!(summary_line(&health.to_doc()), "这一格还没有记录");
        health.record(HookKind::KeyEvent, us(900));
        health.note_throttle();
        health.note_error("08:00", "按键批次超限");
        let line = summary_line(&health.to_doc());
        assert!(
            line.contains("按键批次") && line.contains("p95") && line.contains("降频 1 次"),
            "四类读数与降频次数都要在句子里：{line}"
        );
        assert!(
            line.contains("按键批次") && line.contains("超预算 1 次"),
            "超预算那一格要说出口：{line}"
        );
        assert!(
            line.contains("失败记录 1 条"),
            "有过几条失败要报出来（原因正文在详情页那一栏）：{line}"
        );
    }
}
