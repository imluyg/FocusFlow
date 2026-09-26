//! 数据库写入器：内存聚合 + 周期落库。
//!
//! 按键事件不再逐条落库，而是在内存中按 (天, 小时) / (天, 按键) 聚合，
//! 每 10 秒（或 flush 信号）把增量 UPSERT 到聚合表。相比逐事件写入：
//! - 数据库体积约为原来的 1/170（主统计聚合表一年约 1MB 而非 180MB；
//!   设备维度表另计：那里每行是「天 × 设备 × 键名」，路径已字典化成整数 id）
//! - 写入频率固定，不受按键速度影响
//! - flush 信号：立即落库 + 等待完成（退出/备份用）

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use chrono::Datelike;
use rusqlite::Connection;

use crate::db::connection;
use crate::db::queries;
use crate::paths;

/// 当前日期键（YYYYMMDD），用于跨天判断。
fn current_day_key() -> u64 {
    let now = chrono::Local::now();
    now.year() as u64 * 10000 + now.month() as u64 * 100 + now.day() as u64
}

/// 对写线程的控制信号
enum Signal {
    /// 立即 flush；`done` 为 Some 时等待完成后通知
    Flush { done: Option<mpsc::Sender<()>> },
    /// 停止线程（退出前 flush 残留）
    Stop,
}

/// 连续活跃判定窗口（秒）：事件间隔 ≤ 该值视为同一段连续活跃。
///
/// 活跃时长（active）与前台应用时长（apps）**共用这一个门限**：
/// 两者都在 `record` 里按同一份事件间隔累加，因此天然可比，
/// 不会出现「同一个下午，一张卡说 3 小时、另一张卡说 1 小时」的口径分裂。
pub(crate) const ACTIVE_GAP_SECS: i64 = 60;

/// 设备登记信息（内存会话态：首个事件时登记，之后只读缓存）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DeviceMeta {
    /// 显示名（如 "HID-compliant mouse · 046D/C52B"）
    pub name: String,
    /// 设备类型：mouse / keyboard / hybrid（同一实例产生两类事件时）
    pub kind: String,
}

/// 内存中的聚合增量（未落库部分）。
/// Clone 用于恢复文件快照；恢复文件的序列化格式见 `AggDeltasFile`。
#[derive(Default, Clone)]
struct AggDeltas {
    /// (date_key) -> count
    daily: HashMap<i64, i64>,
    /// (date_key, hour) -> count
    hourly: HashMap<(i64, i64), i64>,
    /// (date_key) -> (key_name -> count)。
    /// 按天嵌套而非 (date_key, String) 元素键：record 热路径可用 &str
    /// 查内层 map（String: Borrow<str>），避免每按键分配 String。
    keys: HashMap<i64, HashMap<String, i64>>,
    /// (date_key) -> 当日累计活跃秒数
    active: HashMap<i64, i64>,
    /// ((date_key, 应用名)) -> 当日累计使用秒数（前台应用统计）
    apps: HashMap<(i64, String), i64>,
    /// ((date_key, 设备实例路径)) -> 当日累计输入次数（设备维度统计）。
    /// 独立口径（键盘按下 + 鼠标按键按下 + 滚轮），不进 daily/hourly/keys。
    /// 内存里按路径聚合（热路径只做字符串查表），落库时才换成 devices.id。
    devices: HashMap<(i64, String), i64>,
    /// ((date_key, 设备实例路径, 键名)) -> 当日累计次数（设备 × 键名明细）。
    /// 用于「设备详情」里的键名排行；同为独立口径，不参与主统计的 key_counts。
    device_keys: HashMap<(i64, String, String), i64>,
    /// 设备登记（device_key -> 名称/类型）。会话态，flush 不取走
    /// （名称缓存由采集线程在设备首个事件时写入一次，此后只读）。
    device_meta: HashMap<String, DeviceMeta>,
    /// 上一个事件的时间戳（连续活跃判定用；flush 取走增量时保留）
    last_ts: i64,
    /// 当前前台应用名（归属用；flush 取走增量时保留）。
    /// 采集线程写入，`record` 热路径读取 —— 事件发生时把「距上一事件的间隔」
    /// 记到当时的前台应用头上。None = 无可归属应用（启动初期 / exclude 命中 / 采集失败）。
    current_app: Option<String>,
}

impl AggDeltas {
    fn is_empty(&self) -> bool {
        self.daily.is_empty()
            && self.hourly.is_empty()
            && self.keys.is_empty()
            && self.active.is_empty()
            && self.apps.is_empty()
            && self.devices.is_empty()
            && self.device_keys.is_empty()
    }

    /// 取走待落库增量。`last_ts` / `current_app` / `device_meta` 保留在内存聚合中：
    /// 前者否则每次 flush 都会打断连续活跃判定（每 10 秒白丢一段时长），
    /// 后两者是会话态（当前前台应用、设备名称缓存），不属于待落库数据。
    /// `devices`（计数）会被取走落库。
    fn take_for_flush(&mut self) -> AggDeltas {
        AggDeltas {
            daily: std::mem::take(&mut self.daily),
            hourly: std::mem::take(&mut self.hourly),
            keys: std::mem::take(&mut self.keys),
            active: std::mem::take(&mut self.active),
            apps: std::mem::take(&mut self.apps),
            devices: std::mem::take(&mut self.devices),
            device_keys: std::mem::take(&mut self.device_keys),
            // 登记表快照随增量走（落库时写 devices 表），本体保留在内存
            device_meta: self.device_meta.clone(),
            last_ts: self.last_ts,
            current_app: None,
        }
    }

    /// 按年份库切分增量。
    ///
    /// 年度库是 `focusflow_<年>.db`，而增量里的 date_key 来自事件发生时刻。
    /// 跨年时（23:59 的事件在 00:00 后才 flush）如果统一写进「当前年份」的库，
    /// 旧年份的数据会落进新年度文件：按日期查询会漏读，
    /// 而年度归档会因目标库已存在同 date_key 主键冲突而整体回滚，导致归档永久失败。
    fn split_by_year(&self, year_of: impl Fn(i64) -> i32) -> HashMap<i32, AggDeltas> {
        let mut parts: HashMap<i32, AggDeltas> = HashMap::new();
        for (dk, n) in &self.daily {
            parts.entry(year_of(*dk)).or_default().daily.insert(*dk, *n);
        }
        for ((dk, h), n) in &self.hourly {
            parts
                .entry(year_of(*dk))
                .or_default()
                .hourly
                .insert((*dk, *h), *n);
        }
        for (dk, key_map) in &self.keys {
            parts
                .entry(year_of(*dk))
                .or_default()
                .keys
                .insert(*dk, key_map.clone());
        }
        for (dk, n) in &self.active {
            parts
                .entry(year_of(*dk))
                .or_default()
                .active
                .insert(*dk, *n);
        }
        for ((dk, app), n) in &self.apps {
            parts
                .entry(year_of(*dk))
                .or_default()
                .apps
                .insert((*dk, app.clone()), *n);
        }
        for ((dk, dev), n) in &self.devices {
            parts
                .entry(year_of(*dk))
                .or_default()
                .devices
                .insert((*dk, dev.clone()), *n);
        }
        for ((dk, dev, key), n) in &self.device_keys {
            parts
                .entry(year_of(*dk))
                .or_default()
                .device_keys
                .insert((*dk, dev.clone(), key.clone()), *n);
        }
        // 设备登记表随各分区带上（落库时写 devices 表）。
        // 必须显式拷贝：partition 由 or_default() 新建，device_meta 默认为空，
        // 漏掉这一步会出现「device_counts 写成功但 devices 表为空」的隐性数据缺失。
        for part in parts.values_mut() {
            part.device_meta = self.device_meta.clone();
        }
        parts
    }
}

struct WriterState {
    /// 未落库的聚合增量（record 累加，flush 时取走）
    agg: Mutex<AggDeltas>,
    /// 信号发送端
    sig_tx: mpsc::Sender<Signal>,
    /// 今日计数（内存缓存）
    today_count: AtomicU64,
    /// 今日活跃秒数（内存缓存）
    today_active: AtomicU64,
    /// 今日日期键（YYYYMMDD），用于跨天重置
    today_key: AtomicU64,
    /// 线程是否存活
    alive: AtomicBool,
    /// 成功落库次数（有实际写入才递增）：图表缓存用 "序号未变" 判定库内容没变，跳过重聚合
    flush_seq: AtomicU64,
    /// 启动回放后留下的兜底副本路径，等**首批增量真的落库**才删（见 `take_recovery`）。
    /// `None` = 这次启动没有回放任何东西。
    recovery_leftover: Mutex<Option<std::path::PathBuf>>,
}

/// 写入器句柄（Send + Sync，可跨线程持有）。
pub struct DbWriter {
    state: Arc<WriterState>,
}

impl DbWriter {
    /// 创建并启动写入线程。
    ///
    /// spawn 失败必须往上传（`Err` → `Database::init` → 启动失败）：这个线程是
    /// 数据持久化的**唯一出口**，它没起来时 `record()` 只在内存累加，一条都进不了
    /// 库、退出即丢 —— 与其让程序"看起来在跑其实什么都不存"，不如起不来就说清楚
    /// （原来 `.map_err(日志).ok()` 吞掉后连启动日志都照常打"已启动"）。
    pub fn start(flush_interval: Duration) -> anyhow::Result<Arc<Self>> {
        let (sig_tx, sig_rx) = mpsc::channel();
        // 今日计数初始值 = 聚合表中今日的记录数
        let today_base_count = {
            let path = paths::current_year_db_path();
            connection::open_ro(&path)
                .ok()
                .and_then(|conn| {
                    conn.query_row(
                        "SELECT COALESCE(SUM(count), 0) FROM daily_counts WHERE date_key = ?1",
                        [queries::day_key_of_date(chrono::Local::now().date_naive())],
                        |r| r.get::<_, i64>(0),
                    )
                    .ok()
                })
                .unwrap_or(0)
                .max(0) as u64
        };
        let today_base_active = {
            let path = paths::current_year_db_path();
            connection::open_ro(&path)
                .ok()
                .and_then(|conn| {
                    conn.query_row(
                        "SELECT COALESCE(seconds, 0) FROM daily_counts WHERE date_key = ?1",
                        [queries::day_key_of_date(chrono::Local::now().date_naive())],
                        |r| r.get::<_, i64>(0),
                    )
                    .ok()
                })
                .unwrap_or(0)
                .max(0) as u64
        };
        // 启动回放：上次进程异常终止时写入的未落库增量（若存在）并入内存聚合，
        // 随首次周期 flush 正常落库。读到就改名留一份副本，等首批增量真落库再删
        // （见 `take_recovery`）—— 原来是读到即删，等于把兜底证据只留到首次 flush。
        let (recovered, recovered_copy) = take_recovery();
        if let Some(ref r) = recovered {
            tracing::warn!(
                "发现未落库增量恢复文件，已回放: daily={} hourly={} keys={} apps={}",
                r.daily.len(),
                r.hourly.len(),
                r.keys.len(),
                r.apps.len()
            );
        }
        // 回放增量若属于今天，计入今日缓存基准，保证缓存 = 库 + 内存待落库
        let today_dk = queries::day_key_of_date(chrono::Local::now().date_naive());
        let recovered_today_count = recovered
            .as_ref()
            .and_then(|r| r.daily.get(&today_dk))
            .copied()
            .unwrap_or(0)
            .max(0) as u64;
        let recovered_today_active = recovered
            .as_ref()
            .and_then(|r| r.active.get(&today_dk))
            .copied()
            .unwrap_or(0)
            .max(0) as u64;
        let today_base_count = today_base_count + recovered_today_count;
        let today_base_active = today_base_active + recovered_today_active;
        // 设备登记预热：恢复文件不带 device_meta（会话态不透传），先从 devices 表把
        // 名字补回来，避免回放后首次 flush 只能拿 device_key 占位补登。
        let mut initial = recovered.unwrap_or_default();
        // B14-2：升级前崩溃留下的恢复批次里可能还挂着旧版完整实例路径键 ——
        // 先归一到硬件身份段（同一硬件的新旧计数在内存里就合并），后面的
        // preload_device_meta 与首次落库才同轨；正常运行这里没有旧键，零成本。
        initial = normalize_device_keys(&initial);
        preload_device_meta(&mut initial);
        let state = Arc::new(WriterState {
            agg: Mutex::new(initial),
            sig_tx,
            today_count: AtomicU64::new(today_base_count),
            today_active: AtomicU64::new(today_base_active),
            today_key: AtomicU64::new(current_day_key()),
            alive: AtomicBool::new(true),
            flush_seq: AtomicU64::new(0),
            recovery_leftover: Mutex::new(recovered_copy),
        });

        let writer = Arc::new(Self {
            state: Arc::clone(&state),
        });

        let state2 = Arc::clone(&state);
        thread::Builder::new()
            .name("db-writer".into())
            .spawn(move || writer_loop(state2, sig_rx, flush_interval))
            .map_err(|e| anyhow::anyhow!("启动 DB 写入线程失败: {e}"))?;

        tracing::info!(
            "DB 写入线程已启动 (聚合写入, interval={:?})",
            flush_interval
        );
        Ok(writer)
    }

    /// 记录一次按键：累加到内存聚合（非阻塞，永不阻塞监听热路径）。
    pub fn record(&self, key_name: &str, timestamp: i64) {
        let state = &*self.state;
        // 跨天检查：日期变化则重置今日计数/活跃时长（避免次日显示累计值）
        let day = current_day_key();
        if state.today_key.load(Ordering::Relaxed) != day {
            state.today_key.store(day, Ordering::Relaxed);
            state.today_count.store(0, Ordering::Relaxed);
            state.today_active.store(0, Ordering::Relaxed);
        }
        state.today_count.fetch_add(1, Ordering::Relaxed);

        let day_key = queries::day_key_of_ts(timestamp);
        let hour = queries::hour_of_ts(timestamp);
        let mut agg = state.agg.lock().unwrap_or_else(|e| e.into_inner());
        *agg.daily.entry(day_key).or_insert(0) += 1;
        *agg.hourly.entry((day_key, hour)).or_insert(0) += 1;
        // 不用 entry()：那会为每次按键分配一个立刻丢弃的 String；
        // 内层 map 用 &str 查（绝大多数命中）零分配，仅首次出现某键时才分配。
        let day_map = agg.keys.entry(day_key).or_default();
        if let Some(c) = day_map.get_mut(key_name) {
            *c += 1;
        } else {
            day_map.insert(key_name.to_string(), 1);
        }
        // 活跃时长：与上一事件间隔 ≤ ACTIVE_GAP_SECS 视为连续活跃
        let contrib = if agg.last_ts > 0
            && timestamp >= agg.last_ts
            && timestamp - agg.last_ts <= ACTIVE_GAP_SECS
        {
            timestamp - agg.last_ts
        } else {
            0
        };
        agg.last_ts = timestamp;
        if contrib > 0 {
            *agg.active.entry(day_key).or_insert(0) += contrib;
            state
                .today_active
                .fetch_add(contrib as u64, Ordering::Relaxed);
            // 同一段秒数同时归给「事件发生时所在的前台应用」：
            // 与 active 同源、同门限（ACTIVE_GAP_SECS），所以应用总时长恒 ≤ 今日活跃时长，
            // 差额只剩「当时没有已知前台应用」的时段（启动初期 / exclude 命中）。
            // 归给「本次事件时」的应用是安全的：切窗动作（Alt+Tab、点任务栏）本身
            // 也是一次键鼠事件，会把上一段静默期封口在前一个应用上，
            // 不会出现「在 A 看了半小时却记到 B 头上」。
            if let Some(app) = agg.current_app.clone() {
                *agg.apps.entry((day_key, app)).or_insert(0) += contrib;
            }
        }
    }

    /// 今日计数（内存缓存值）。
    pub fn today_count(&self) -> u64 {
        self.state.today_count.load(Ordering::Relaxed)
    }

    /// 今日活跃秒数（内存缓存值）。
    pub fn today_active_seconds(&self) -> u64 {
        self.state.today_active.load(Ordering::Relaxed)
    }

    /// 采集线程调用：更新「当前前台应用」（None = 无可归属应用）。
    ///
    /// 这里**不累计秒数** —— 秒数由 `record` 在键鼠事件发生时按事件间隔写入，
    /// 与应用活跃时长共用同一个门限，因此两个指标天然可比。
    pub(crate) fn set_current_app(&self, name: Option<&str>) {
        let mut agg = self.state.agg.lock().unwrap_or_else(|e| e.into_inner());
        match name {
            // 名字没变就不重建 String：每秒采样一次，避免无谓分配
            Some(n) if agg.current_app.as_deref() != Some(n) => {
                agg.current_app = Some(n.to_string());
            }
            Some(_) => {}
            None => agg.current_app = None,
        }
    }

    /// 设备维度统计：记录一次某设备的输入（独立口径，不进主统计）。
    ///
    /// 只累加 `devices` 聚合 —— **不碰 daily/hourly/keys/active**：
    /// 主统计由 rdev 低级钩子负责，本方法来自 Raw Input 侧信道，
    /// 两条链路的过滤规则不同（Raw Input 只收真实硬件输入、
    /// 不做长按去重/修饰键过滤），数字不追求与键鼠统计相等。
    ///
    /// 设备登记（名称/类型）在首个事件时写入 `device_meta` 一次，
    /// 之后同名调用零开销（名称未变不重建 String）。
    pub fn record_device(&self, device_key: &str, name: &str, kind: &str, timestamp: i64) {
        let mut agg = self.state.agg.lock().unwrap_or_else(|e| e.into_inner());
        match agg.device_meta.get_mut(device_key) {
            Some(meta) => {
                // 类型冲突（同一实例产生了鼠标+键盘两类事件）→ hybrid
                if meta.kind != kind && meta.kind != "hybrid" {
                    meta.kind = "hybrid".to_string();
                }
                if meta.name != name {
                    meta.name = name.to_string();
                }
            }
            None => {
                agg.device_meta.insert(
                    device_key.to_string(),
                    DeviceMeta {
                        name: name.to_string(),
                        kind: kind.to_string(),
                    },
                );
            }
        }
        let day_key = queries::day_key_of_ts(timestamp);
        *agg.devices
            .entry((day_key, device_key.to_string()))
            .or_insert(0) += 1;
    }

    /// 设备 × 键名明细：记录某设备的某个按键/滚轮一次（供设备详情排行）。
    ///
    /// 与 [`Self::record_device`] 同源同口径，只是多带一个键名维度；
    /// 调用方一次输入同时调用两者（次数 + 键名），键名明细不单独进 devices 计数。
    pub fn record_device_key(&self, device_key: &str, key_name: &str, timestamp: i64) {
        let mut agg = self.state.agg.lock().unwrap_or_else(|e| e.into_inner());
        let day_key = queries::day_key_of_ts(timestamp);
        *agg.device_keys
            .entry((day_key, device_key.to_string(), key_name.to_string()))
            .or_insert(0) += 1;
    }

    /// 是否有未落库的增量（重聚合前判断是否需要先 flush）。
    pub fn has_pending(&self) -> bool {
        !self
            .state
            .agg
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// 今日**尚未落库**的按键增量（内存聚合里今日那一份）。
    ///
    /// 统计线程用它把「近 n 天」那张卡片补成逐键动的：库里那份窗口和只到上次
    /// flush（默认 10 秒）为止，不加这个量，卡片就只能每 10 秒跳一次。
    /// 落库之后今日那项会被清空、库里同时长出同一批 → 加上它不会重复计数。
    pub fn today_pending_count(&self) -> i64 {
        let dk = queries::day_key_of_date(chrono::Local::now().date_naive());
        self.state
            .agg
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .daily
            .get(&dk)
            .copied()
            .unwrap_or(0)
            .max(0)
    }

    /// 重新统计「今日按键数 + 今日活跃时长」缓存（导入或外部写库之后调用）。
    ///
    /// 两个都得刷：落库是 `seconds = seconds + excluded.seconds` 的增量式，所以
    /// 导入进来的时长不会丢，但内存里那个基准还是旧的 —— 界面上的「今日活跃时长」
    /// 会一直少着一截，直到下次重启才补回来。以前这里只刷了次数。
    /// 先 flush 再读聚合表，两个数取的是同一份已落库的状态。
    ///
    /// flush 只等 3 秒，超时是常态而非例外（写线程正在重试退避、库被别的连接占着），
    /// 而那批增量随后才落库 —— 所以锚定值不能只取库值：`库里的今日聚合 + 内存里
    /// 尚未落库的今日增量` 才是今日真值。原来超时后照用库值，等于把那批增量从
    /// 「今日」里抹掉，而 `today_count` 之后只会往上加，一整天都补不回来
    /// （「总计」卡片和悬浮窗跟着偏小）。
    pub fn recompute_today_totals(&self) {
        let flushed = self.flush_confirmed(true);
        self.state
            .today_key
            .store(current_day_key(), Ordering::Relaxed);
        let today_dk = queries::day_key_of_date(chrono::Local::now().date_naive());
        // 持着 agg 锁去读库：写线程的 flush_pending 也要这把锁（它的 SQLite IO 在
        // 锁外做），所以"库里的值"和"内存里的待落库增量"取的是同一瞬间的状态，
        // 不会出现同一批既算进库值又算进增量。
        let agg = self.state.agg.lock().unwrap_or_else(|e| e.into_inner());
        let pending_count = agg.daily.get(&today_dk).copied().unwrap_or(0).max(0) as u64;
        let pending_active = agg.active.get(&today_dk).copied().unwrap_or(0).max(0) as u64;
        let (count, seconds) = connection::open_ro(&paths::current_year_db_path())
            .ok()
            .and_then(|conn| {
                conn.query_row(
                    "SELECT COALESCE(SUM(count), 0), COALESCE(SUM(seconds), 0) \
                     FROM daily_counts WHERE date_key = ?1",
                    [today_dk],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
                )
                .ok()
            })
            .unwrap_or((0, 0));
        // 锁到这一步才放：上面那句读库必须和"读内存增量"取同一瞬间的状态
        let base_count = self.state.today_count.load(Ordering::Relaxed);
        let base_active = self.state.today_active.load(Ordering::Relaxed);
        drop(agg);
        if !flushed {
            tracing::warn!("recompute_today_totals: flush 未确认完成，今日基准只追平不回退");
        }
        // flush 确认完成 → 库值 + 内存增量就是今日真值，照它重锚（含"库变小"的场景，
        // 例如 --reset 之后）。超时 → 那批增量可能已被写线程从 agg 里取走、正卡在重试
        // （既不在库里也不在 agg 里），而内存基准**是含它的** → 只能取 max，
        // 让今日只会追平、不会倒退。
        let landed_count = count.max(0) as u64 + pending_count;
        let landed_active = seconds.max(0) as u64 + pending_active;
        let (new_count, new_active) = if flushed {
            (landed_count, landed_active)
        } else {
            (landed_count.max(base_count), landed_active.max(base_active))
        };
        self.state.today_count.store(new_count, Ordering::Relaxed);
        self.state.today_active.store(new_active, Ordering::Relaxed);
    }

    /// 立即 flush：发信号让写线程落库。`wait=true` 时阻塞等待完成。
    pub fn flush(&self, wait: bool) {
        let _ = self.flush_confirmed(wait);
    }

    /// 同 [`Self::flush`]，但把"这批增量是否已确认落库"告诉调用方。
    /// `wait=false` 恒返回 false：没等过就不能声称落完了。
    fn flush_confirmed(&self, wait: bool) -> bool {
        if !wait {
            let _ = self.state.sig_tx.send(Signal::Flush { done: None });
            return false;
        }
        let (tx, rx) = mpsc::channel();
        if self
            .state
            .sig_tx
            .send(Signal::Flush { done: Some(tx) })
            .is_err()
        {
            return false;
        }
        if rx.recv_timeout(Duration::from_secs(3)).is_ok() {
            return true;
        }
        tracing::warn!("flush 等待超时（3 秒），写线程可能繁忙，增量仍在内存中");
        false
    }

    /// 停止写线程（退出前 flush 残留）。
    /// 超时仍未停止（磁盘忙/库被锁）时，把未落库增量写到恢复文件兜底，
    /// 下次启动回放——数据从内存移除，写线程即使随后恢复也不会再写一份（防重复计数）。
    pub fn stop(&self) {
        let _ = self.state.sig_tx.send(Signal::Stop);
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.state.alive.load(Ordering::Relaxed) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        if self.state.alive.load(Ordering::Relaxed) {
            snapshot_recovery(&self.state, true);
        }
    }

    /// 线程是否存活。
    pub fn is_alive(&self) -> bool {
        self.state.alive.load(Ordering::Relaxed)
    }

    /// `stop()` 之后继续等到线程真的退出（最多 10 秒）。
    ///
    /// `stop()` 只等 3 秒，超时就先写恢复文件再返回 —— 那时线程可能还在做
    /// 最后一次 flush，而它会照着自己记下的 `app_dir` 去 `open_rw`。生产里
    /// 目录不会被删，无所谓；测试里紧跟着就是 `TestAppDir` 的 Drop 删目录，
    /// 于是那个目录连带 `data/focusflow_YYYY.db` 被重新建出来
    /// （实测每个全量跑多 1~2 个 `%TEMP%` 目录）。
    ///
    /// 等的是 `alive`，而它只在末次 flush **之后**才置 false，所以等到就等于
    /// "再没有人会往这个目录写"。
    pub fn stop_and_wait(&self) {
        self.stop();
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.state.alive.load(Ordering::Relaxed) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        if self.state.alive.load(Ordering::Relaxed) {
            tracing::warn!("DB 写入线程 10 秒内没退出，后续落盘可能写到别的目录");
        }
    }

    /// 成功落库序号（有实际写入才递增）。
    pub fn flush_seq(&self) -> u64 {
        self.state.flush_seq.load(Ordering::Relaxed)
    }
}

/// 未落库增量恢复文件路径。
fn recovery_path() -> std::path::PathBuf {
    paths::data_dir().join("agg_recovery.json")
}

/// 回放之后给恢复文件留的副本名：等首批增量真的落库才删。
fn recovery_kept_path() -> std::path::PathBuf {
    paths::data_dir().join("agg_recovery.replayed.json")
}

/// 恢复文件的序列化格式：JSON 对象键必须是字符串，故整数键的 map 一律转成
/// 元组数组（AggDeltas 本身保持 HashMap 以保证热路径效率）。
#[derive(serde::Serialize, serde::Deserialize)]
struct AggDeltasFile {
    daily: Vec<(i64, i64)>,
    hourly: Vec<((i64, i64), i64)>,
    keys: Vec<(i64, Vec<(String, i64)>)>,
    active: Vec<(i64, i64)>,
    apps: Vec<((i64, String), i64)>,
    devices: Vec<((i64, String), i64)>,
    /// 设备 × 键名明细。旧版恢复文件没有该字段 → 用 default 兼容
    #[serde(default)]
    device_keys: Vec<((i64, String, String), i64)>,
    last_ts: i64,
}

impl From<&AggDeltas> for AggDeltasFile {
    fn from(a: &AggDeltas) -> Self {
        Self {
            daily: a.daily.iter().map(|(k, v)| (*k, *v)).collect(),
            hourly: a.hourly.iter().map(|(k, v)| (*k, *v)).collect(),
            keys: a
                .keys
                .iter()
                .map(|(dk, m)| (*dk, m.iter().map(|(k, v)| (k.clone(), *v)).collect()))
                .collect(),
            active: a.active.iter().map(|(k, v)| (*k, *v)).collect(),
            apps: a.apps.iter().map(|(k, v)| (k.clone(), *v)).collect(),
            devices: a.devices.iter().map(|(k, v)| (k.clone(), *v)).collect(),
            device_keys: a.device_keys.iter().map(|(k, v)| (k.clone(), *v)).collect(),
            last_ts: a.last_ts,
        }
    }
}

impl From<AggDeltasFile> for AggDeltas {
    fn from(f: AggDeltasFile) -> Self {
        Self {
            daily: f.daily.into_iter().collect(),
            hourly: f.hourly.into_iter().collect(),
            keys: f
                .keys
                .into_iter()
                .map(|(dk, m)| (dk, m.into_iter().collect()))
                .collect(),
            active: f.active.into_iter().collect(),
            apps: f.apps.into_iter().collect(),
            devices: f.devices.into_iter().collect(),
            device_keys: f.device_keys.into_iter().collect(),
            // 会话态不透传恢复文件：回放后由采集线程重新填充
            // （设备名称在下一个输入事件时重新登记）
            device_meta: HashMap::new(),
            last_ts: f.last_ts,
            current_app: None,
        }
    }
}

/// 把未落库增量快照写到恢复文件。
/// `take=true` 时同时从内存聚合移除：用于 stop 超时 / panic（进程即将终止），
/// 数据此后只存在于文件中，避免写线程随后恢复后再次落库造成重复计数。
///
/// 全程持有 agg 锁，且**先写盘成功、后从内存取走**：原先是"先取走再写盘"，
/// 写失败只留一行 `tracing::error!` 就 return —— 那批增量既不在库里、也不在
/// 文件里，等于在最需要兜底的时刻（线程已经不收增量了）把数据凭空抹掉。
/// 持锁跨写盘还顺带堵掉了取走与落库之间的竞态：写线程的 `flush_pending`
/// 同样要拿这把锁（它的 SQLite IO 在锁外做），所以在它看来这批增量从未消失过。
fn snapshot_recovery(state: &WriterState, take: bool) {
    // try_lock：panic 可能发生在持有 agg 锁的线程，此时放弃快照（进程即将终止）
    let Ok(mut agg) = state.agg.try_lock() else {
        return;
    };
    if agg.is_empty() {
        return;
    }
    let json = match serde_json::to_string(&AggDeltasFile::from(&*agg)) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!("恢复文件序列化失败: {e}");
            return;
        }
    };
    // 临时文件 + rename，避免写一半崩溃留下残缺 JSON
    let path = recovery_path();
    std::fs::create_dir_all(path.parent().unwrap_or(std::path::Path::new("."))).ok();
    let tmp = path.with_extension("json.tmp");
    let result = std::fs::write(&tmp, json).and_then(|_| std::fs::rename(&tmp, &path));
    match result {
        Ok(()) => {
            tracing::warn!(
                "未落库增量已写入恢复文件 {}（下次启动回放）",
                path.display()
            );
            if take {
                agg.take_for_flush();
            }
        }
        // 写失败就**不**取走：增量仍在内存里，至少不比修前更糟
        Err(e) => tracing::error!("恢复文件写入失败 {}: {e}", path.display()),
    }
}

/// 读取恢复文件（启动回放）。
///
/// 返回 `(回放进内存的增量, 要留到首批落库之后再删的副本路径)`。
/// 原来读到就 `remove_file`，而回放的数据要等**首次周期 flush**（默认 10 秒）才进库
/// —— 这 10 秒里再崩一次/断电，那批增量就二次丢失，而且盘上连痕迹都没有。
/// 现在改成改名留一份：改名成功就等于"不会被第二次回放"（下次启动只看
/// `agg_recovery.json`），删除则推迟到写线程确认首批增量真的落库之后。
/// 解析失败同样改名（不再重读重败），但不必等落库 —— 没有数据要保护。
fn take_recovery() -> (Option<AggDeltas>, Option<std::path::PathBuf>) {
    let path = recovery_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return (None, None);
    };
    let kept = recovery_kept_path();
    // Windows 上目标已存在时 rename 会失败（上次崩留下的副本还没到删除时机）→ 先清掉它。
    // 这里覆盖是安全的：那份副本的内容已经在这次的 agg_recovery.json 里被重新算过了
    // —— 上一轮回放进内存的增量如果没落库，进程就不会活着写新的恢复文件；落了库就已在库里。
    if std::fs::rename(&path, &kept).is_err() {
        let _ = std::fs::remove_file(&kept);
        if std::fs::rename(&path, &kept).is_err() {
            // 改名失败（副本被别的进程占着）就退回旧行为：删掉，
            // 不能让同一份文件每次启动都重放一遍——那是确定的重复计数。
            let _ = std::fs::remove_file(&path);
        }
    }
    match serde_json::from_str::<AggDeltasFile>(&text) {
        Ok(v) => (Some(v.into()), Some(kept)),
        Err(e) => {
            tracing::error!("恢复文件解析失败（已丢弃，残片在 {}）: {e}", kept.display());
            (None, None)
        }
    }
}

/// 首批增量已落库 → 可以扔掉回放副本了。幂等：没有副本时为空操作。
fn drop_recovery_leftover(state: &WriterState) {
    let Ok(mut slot) = state.recovery_leftover.lock() else {
        return;
    };
    let Some(path) = slot.take() else { return };
    match std::fs::remove_file(&path) {
        Ok(()) => tracing::debug!("回放副本已删除 {}", path.display()),
        // 删不掉不影响正确性：下次启动只读 agg_recovery.json，不会重放这个副本
        Err(e) => tracing::warn!("回放副本删除失败 {}: {e}", path.display()),
    }
}

// panic 兜底：panic hook（logger）在进程终止前调用 snapshot，
// 把未落库增量从内存移出并落盘，配合启动回放把异常终止的丢失窗口收到接近零。
static PANIC_WRITER: std::sync::Mutex<Option<std::sync::Arc<DbWriter>>> =
    std::sync::Mutex::new(None);

/// 注册全局 writer 引用（Database 创建后调用一次）。
pub fn register_panic_recovery(writer: std::sync::Arc<DbWriter>) {
    if let Ok(mut slot) = PANIC_WRITER.lock() {
        *slot = Some(writer);
    }
}

/// panic hook 调用：对未落库增量做兜底快照。幂等，未注册或无增量时为空操作。
pub fn panic_recovery_snapshot() {
    if let Ok(slot) = PANIC_WRITER.try_lock() {
        if let Some(w) = slot.as_ref() {
            snapshot_recovery(&w.state, true);
        }
    }
}

fn writer_loop(state: Arc<WriterState>, sig_rx: mpsc::Receiver<Signal>, flush_interval: Duration) {
    let mut last_flush = Instant::now();
    // 持久连接：跨年时重建，避免每批重开
    let mut conn: Option<Connection> = None;
    let mut conn_year: i32 = 0;

    loop {
        // 处理信号（100ms 超时，保证周期 flush 检查）
        let mut stop = false;
        loop {
            match sig_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Signal::Flush { done }) => {
                    flush_pending(&mut conn, &mut conn_year, &state);
                    if let Some(d) = done {
                        let _ = d.send(());
                    }
                    last_flush = Instant::now();
                }
                Ok(Signal::Stop) => {
                    stop = true;
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    stop = true;
                    break;
                }
            }
        }
        if stop {
            flush_pending(&mut conn, &mut conn_year, &state);
            state.alive.store(false, Ordering::Relaxed);
            tracing::info!("DB 写入线程已停止");
            return;
        }

        // 周期落库
        if last_flush.elapsed() >= flush_interval {
            flush_pending(&mut conn, &mut conn_year, &state);
            last_flush = Instant::now();
        }
    }
}

/// 确保连接指向 `year` 年份库（换年时重建）。
fn ensure_connection(conn: &mut Option<Connection>, conn_year: &mut i32, year: i32) {
    if *conn_year == year && conn.is_some() {
        return;
    }
    // 换年或首次：重建连接
    *conn = None;
    let path = paths::year_db_path(year);
    if let Ok(new_conn) = connection::open_rw(&path) {
        if connection::ensure_schema(&new_conn, year).is_ok() {
            *conn = Some(new_conn);
            *conn_year = year;
        }
    }
}

/// date_key（本地天数序号）落在哪一年。
fn year_of_day_key(dk: i64) -> i32 {
    queries::day_key_to_date(dk)
        .map(|d| d.year())
        .unwrap_or_else(paths::current_year)
}

/// 把内存增量落库（按年份库分批、每批单事务 UPSERT，失败重试，最终失败回填内存避免丢数据）。
fn flush_pending(conn: &mut Option<Connection>, conn_year: &mut i32, state: &WriterState) {
    let pending = {
        let mut agg = state.agg.lock().unwrap_or_else(|e| e.into_inner());
        if agg.is_empty() {
            return;
        }
        agg.take_for_flush()
    };

    // 增量按年份库切分后分别写入：跨年那一刻的按键必须落进它所属年份的库，
    // 否则归档会主键冲突、按日期查询会漏读（见 AggDeltas::split_by_year）。
    let mut parts = pending.split_by_year(year_of_day_key);
    // 只回填写失败的那一份，其他年份已成功落库的不重写
    let mut failed: Vec<AggDeltas> = Vec::new();
    let mut wrote_any = false;
    for (year, part) in parts.drain() {
        match flush_partition(conn, conn_year, year, &part) {
            Ok(()) => {
                wrote_any = true;
                state.flush_seq.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                tracing::error!("聚合落库最终失败（{year} 年）: {e}");
                failed.push(part);
            }
        }
    }
    if wrote_any {
        // 首批真的进库了 —— 启动回放留的副本到这里才完成它的使命（见 take_recovery）
        drop_recovery_leftover(state);
    }
    if failed.is_empty() {
        return;
    }
    // 回填内存，避免数据丢失（下次周期 flush 再试）
    let mut agg = state.agg.lock().unwrap_or_else(|e| e.into_inner());
    for part in failed {
        for (dk, n) in part.daily {
            *agg.daily.entry(dk).or_insert(0) += n;
        }
        for ((dk, h), n) in part.hourly {
            *agg.hourly.entry((dk, h)).or_insert(0) += n;
        }
        for (dk, key_map) in part.keys {
            let day_map = agg.keys.entry(dk).or_default();
            for (key, n) in key_map {
                *day_map.entry(key).or_insert(0) += n;
            }
        }
        for (dk, n) in part.active {
            *agg.active.entry(dk).or_insert(0) += n;
        }
        for ((dk, app), n) in part.apps {
            *agg.apps.entry((dk, app)).or_insert(0) += n;
        }
        for ((dk, dev), n) in part.devices {
            *agg.devices.entry((dk, dev)).or_insert(0) += n;
        }
        for ((dk, dev, key), n) in part.device_keys {
            *agg.device_keys.entry((dk, dev, key)).or_insert(0) += n;
        }
    }
}

/// 启动时把库里已登记的设备名/类型灌进内存会话态。
///
/// `device_meta` 原本只在设备产生首个输入事件时才建立，因此有两个空窗：
///  1. 恢复文件回放 —— `AggDeltasFile` 压根没有这个字段，回放后必然为空；
///  2. 程序刚启动到用户第一次动键鼠之间。
///
/// 空窗内若发生落库，补登只能拿 device_key 占位，界面会露出机器串。
///
/// 名字早就躺在 `devices` 表里，这里读回来当基线即可：
/// 采集线程随后解析出的真名仍会经 `record_device` 覆盖它（注册表是权威来源，
/// 设备换了型号或被改名时能自愈），所以预热只影响「还没等到事件」的那段时间。
///
/// 只读**当前年库**：写入侧面对的是正在输入的设备，它们的登记每次 flush 都会
/// UPSERT 到对应年库，当年库覆盖全部活跃设备，读历史年库没有额外收益。
fn preload_device_meta(agg: &mut AggDeltas) {
    let path = paths::current_year_db_path();
    if !connection::table_exists_readonly(&path, "devices") {
        return;
    }
    let Ok(conn) = connection::open_ro(&path) else {
        return;
    };
    let Ok(mut stmt) = conn.prepare("SELECT device_key, name, kind FROM devices") else {
        return;
    };
    let Ok(rows) = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    }) else {
        return;
    };
    let mut loaded = 0usize;
    for (key, name, kind) in rows.flatten() {
        // 占位登记不算名字：空名、或之前被补登成了 device_key 本身（写入/迁移/
        // 归档三条路径都会这么写）。把它们读回来等于把脏名字固化成「真名」。
        if name.trim().is_empty() || name == key {
            continue;
        }
        agg.device_meta
            .entry(key)
            .or_insert(DeviceMeta { name, kind });
        loaded += 1;
    }
    if loaded > 0 {
        tracing::debug!("设备登记预热：已从 devices 表载入 {loaded} 项");
    }
}

/// 把批次里的设备键统一归一到硬件身份段（B14-2）。
///
/// 只在启动回放时调用：升级前崩溃留下的恢复批次里可能还挂着旧版完整实例
/// 路径键，不归一的话 meta/补登按 A 键查、统计行按 B 键落，设备名会退化成
/// 回退命名、同一硬件的新旧计数也会分家。身份键原样返回 —— 没有旧键时
/// 每个映射只多做一次字符串判断。
fn normalize_device_keys(pending: &AggDeltas) -> AggDeltas {
    let stale = |k: &str| crate::device_alias::hardware_identity_key(k) != k;
    let needs = pending.device_meta.keys().any(|k| stale(k))
        || pending.devices.keys().any(|(_, d)| stale(d))
        || pending.device_keys.keys().any(|(_, d, _)| stale(d));
    if !needs {
        return pending.clone();
    }
    tracing::info!("恢复批次含旧版完整路径设备键，归一到硬件身份段");
    let ident = |k: &String| crate::device_alias::hardware_identity_key(k);
    let mut out = AggDeltas {
        daily: pending.daily.clone(),
        hourly: pending.hourly.clone(),
        keys: pending.keys.clone(),
        active: pending.active.clone(),
        apps: pending.apps.clone(),
        devices: HashMap::new(),
        device_keys: HashMap::new(),
        device_meta: pending
            .device_meta
            .iter()
            .map(|(k, v)| (ident(k), v.clone()))
            .collect(),
        // 会话态原样带过去（归一只动设备键）
        last_ts: pending.last_ts,
        current_app: pending.current_app.clone(),
    };
    for ((dk, dev), n) in &pending.devices {
        *out.devices.entry((*dk, ident(dev))).or_insert(0) += *n;
    }
    for ((dk, dev, key), n) in &pending.device_keys {
        *out.device_keys
            .entry((*dk, ident(dev), key.clone()))
            .or_insert(0) += *n;
    }
    out
}

/// 取设备在本库的整数 id：登记行不存在时补登一行（名称走回退命名、kind 记 unknown）。
/// 统计表字典化后只存 id，漏登记会让查询侧 JOIN 静默丢行、凭空少掉设备数据，
/// 所以这里宁可补一条占位登记，也不放弃计数。
fn device_id_in_db(
    upsert: &mut rusqlite::Statement<'_>,
    dev: &str,
    meta: Option<&DeviceMeta>,
) -> rusqlite::Result<i64> {
    // 没有登记信息时不能把裸设备路径写进 devices.name：查询侧只把「空名字」和
    // 「name == device_key」当作缺登记（`merge_device_rows`），其余原样展示，
    // 于是会把 `\\?\HID#VID_046D&PID_C52B&MI_00#8&...` 直接顶到设备排行上。
    // 正常路径还有 `preload_device_meta`（启动时读登记表）+ 设备首个事件的双重保障，
    // 走到这里的多半是恢复回放后仍未取到真名的时段。
    let fallback_name;
    let (name, kind) = match meta {
        Some(m) => (m.name.as_str(), m.kind.as_str()),
        None => {
            fallback_name = queries::fallback_device_name(dev);
            (fallback_name.as_str(), "unknown")
        }
    };
    // B14-2：升级前崩溃留下的恢复文件里可能还是旧版完整实例路径 ——
    // 先归一到身份段再登记，别让一次回放凭空多出一行旧路径键
    // （身份键经身份函数原样返回，正常路径无副作用）。
    let dev = crate::device_alias::hardware_identity_key(dev);
    upsert.query_row(rusqlite::params![dev, name, kind], |r| r.get(0))
}

/// 单个年份库的落库：单事务 UPSERT + 最多 3 次重试。失败时返回错误（调用方回填）。
fn flush_partition(
    conn: &mut Option<Connection>,
    conn_year: &mut i32,
    year: i32,
    pending: &AggDeltas,
) -> anyhow::Result<()> {
    let max_retries = 3;
    let mut last_err: Option<anyhow::Error> = None;

    for attempt in 0..max_retries {
        ensure_connection(conn, conn_year, year);

        let result = (|| -> anyhow::Result<()> {
            let c = conn.as_mut().ok_or_else(|| anyhow::anyhow!("无可用连接"))?;
            c.execute("BEGIN IMMEDIATE;", [])?;
            let apply = || -> anyhow::Result<()> {
                // 设备字典：先落登记并取回本库的整数 id（统计表只存 id）。
                // id 是库内自增、各年度库互不相干，所以每次落库都要按 device_key 取一次。
                let mut dev_ids: HashMap<&str, i64> = HashMap::new();
                {
                    let mut upsert = c.prepare(
                        "INSERT INTO devices (device_key, name, kind) VALUES (?1, ?2, ?3)
                         ON CONFLICT(device_key) DO UPDATE SET name = excluded.name, kind = excluded.kind
                         RETURNING id",
                    )?;
                    for dev in pending.device_meta.keys() {
                        let meta = pending.device_meta.get(dev);
                        let id = device_id_in_db(&mut upsert, dev, meta)?;
                        dev_ids.insert(dev.as_str(), id);
                    }
                    // 统计行出现、登记缺失的设备（历史库/恢复文件）兜底补登，
                    // 否则写入侧拿不到 id、查询侧 JOIN 也会丢行。
                    for dev in pending
                        .devices
                        .keys()
                        .map(|(_, d)| d)
                        .chain(pending.device_keys.keys().map(|(_, d, _)| d))
                    {
                        if dev_ids.contains_key(dev.as_str()) {
                            continue;
                        }
                        let id = device_id_in_db(&mut upsert, dev, pending.device_meta.get(dev))?;
                        dev_ids.insert(dev.as_str(), id);
                    }
                }
                let id_of = |dev: &String| -> anyhow::Result<i64> {
                    dev_ids
                        .get(dev.as_str())
                        .copied()
                        .ok_or_else(|| anyhow::anyhow!("设备 id 解析失败: {dev}"))
                };
                {
                    // 活跃时长已并入 daily_counts.seconds：一次 UPSERT 同时累加两列。
                    // 两个 map 的天集合未必一致（活跃统计上线晚于按键统计，
                    // 早期日期只有按键数没有时长），缺失的一侧写 0，
                    // `+ excluded.x` 的语义下不会影响另一侧。
                    let mut stmt = c.prepare(
                        "INSERT INTO daily_counts (date_key, count, seconds) VALUES (?1, ?2, ?3)
                         ON CONFLICT(date_key) DO UPDATE SET
                            count = count + excluded.count,
                            seconds = seconds + excluded.seconds",
                    )?;
                    let days: BTreeSet<i64> = pending
                        .daily
                        .keys()
                        .chain(pending.active.keys())
                        .copied()
                        .collect();
                    for dk in days {
                        let cnt = pending.daily.get(&dk).copied().unwrap_or(0);
                        let sec = pending.active.get(&dk).copied().unwrap_or(0);
                        stmt.execute(rusqlite::params![dk, cnt, sec])?;
                    }
                }
                {
                    let mut stmt = c.prepare(
                        "INSERT INTO hourly_counts (date_key, hour, count) VALUES (?1, ?2, ?3)
                         ON CONFLICT(date_key, hour) DO UPDATE SET count = count + excluded.count",
                    )?;
                    for ((dk, h), n) in &pending.hourly {
                        stmt.execute(rusqlite::params![dk, h, n])?;
                    }
                }
                {
                    let mut stmt = c.prepare(
                        "INSERT INTO key_counts (date_key, key_name, count) VALUES (?1, ?2, ?3)
                         ON CONFLICT(date_key, key_name) DO UPDATE SET count = count + excluded.count",
                    )?;
                    for (dk, key_map) in &pending.keys {
                        for (key, n) in key_map {
                            stmt.execute(rusqlite::params![dk, key, n])?;
                        }
                    }
                }
                {
                    let mut stmt = c.prepare(
                        "INSERT INTO app_usage (date_key, app_name, seconds) VALUES (?1, ?2, ?3)
                         ON CONFLICT(date_key, app_name) DO UPDATE SET seconds = seconds + excluded.seconds",
                    )?;
                    for ((dk, app), n) in &pending.apps {
                        stmt.execute(rusqlite::params![dk, app, n])?;
                    }
                }
                {
                    let mut stmt = c.prepare(
                        "INSERT INTO device_counts (date_key, device_id, count) VALUES (?1, ?2, ?3)
                         ON CONFLICT(date_key, device_id) DO UPDATE SET count = count + excluded.count",
                    )?;
                    for ((dk, dev), n) in &pending.devices {
                        stmt.execute(rusqlite::params![dk, id_of(dev)?, n])?;
                    }
                }
                {
                    let mut stmt = c.prepare(
                        "INSERT INTO device_key_counts (date_key, device_id, key_name, count) VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT(date_key, device_id, key_name) DO UPDATE SET count = count + excluded.count",
                    )?;
                    for ((dk, dev, key), n) in &pending.device_keys {
                        stmt.execute(rusqlite::params![dk, id_of(dev)?, key, n])?;
                    }
                }
                Ok(())
            };
            match apply() {
                Ok(()) => {
                    c.execute("COMMIT;", [])?;
                    Ok(())
                }
                Err(e) => {
                    let _ = c.execute("ROLLBACK;", []);
                    Err(e)
                }
            }
        })();

        match result {
            Ok(()) => {
                tracing::debug!(
                    "聚合落库成功（{year} 年）: daily={} hourly={} keys={}",
                    pending.daily.len(),
                    pending.hourly.len(),
                    pending.keys.len()
                );
                return Ok(());
            }
            Err(e) => {
                last_err = Some(e);
                // 连接可能损坏，重置以强制重建
                *conn = None;
                if attempt < max_retries - 1 {
                    tracing::warn!(
                        "聚合落库失败（{year} 年，第{}次）, 重试: {}",
                        attempt + 1,
                        last_err.as_ref().unwrap()
                    );
                    thread::sleep(Duration::from_millis(500 * (attempt as u64 + 1)));
                }
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("未知错误")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// `start` 现在返回 `Result`（写线程起不来 = 启动失败）：用例只关心行为，
    /// 统一 expect，失败本身就是测试环境坏了。
    fn start_writer(interval: Duration) -> Arc<DbWriter> {
        DbWriter::start(interval).expect("测试里 DB 写线程必须能启动")
    }

    /// 跨年落库：增量必须写进 date_key 所属年份的库文件。
    ///
    /// 回归：此前统一写「当前年份」的库，跨年夜 23:59 的按键会落进新年度的文件。
    /// 后果是按日期查询漏读、而年度归档因目标库已有同 date_key 主键冲突整体回滚。
    #[test]
    fn flush_writes_each_year_to_its_own_db() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("writer_crossyear");

        let w = start_writer(Duration::from_secs(3600));

        let now = chrono::Local::now();
        let last_year = now.year() - 1;
        let last_year_date = chrono::NaiveDate::from_ymd_opt(last_year, 12, 31).unwrap();
        let dk_last = queries::day_key_of_date(last_year_date);
        let dk_today = queries::day_key_of_date(now.date_naive());

        // 同一批增量里混入上一年的数据（跨年那一刻真实会出现的形态）
        {
            let mut agg = w.state.agg.lock().unwrap_or_else(|e| e.into_inner());
            agg.daily.insert(dk_last, 7);
            agg.daily.insert(dk_today, 3);
        }
        w.flush(true);

        assert_eq!(
            crate::db::queries::get_stats_by_date(last_year_date).0,
            7,
            "上一年的增量必须落在上一年份库里"
        );
        assert_eq!(
            crate::db::queries::get_stats_by_date(now.date_naive()).0,
            3,
            "今日增量必须落在当前年份库里"
        );

        w.stop_and_wait();
        crate::paths::set_app_dir(crate::paths::test_scratch_app_dir());
    }

    /// 跨天边界：today_key 落后于当前日期时，record 应重置今日计数
    /// （统计线程运行中跨 0 点，避免次日显示昨日累计值）。
    #[test]
    fn record_resets_today_count_on_day_change() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("writer_day");

        let w = start_writer(Duration::from_secs(3600));
        // 模拟"昨天"：today_key 是昨天的日期键、计数停留在昨日值
        w.state
            .today_key
            .store(current_day_key() - 1, Ordering::Relaxed);
        w.state.today_count.store(999, Ordering::Relaxed);

        w.record("A", queries::now_ts());

        assert_eq!(w.state.today_key.load(Ordering::Relaxed), current_day_key());
        assert_eq!(w.state.today_count.load(Ordering::Relaxed), 1);

        w.flush(true);
        w.stop_and_wait();
        crate::paths::set_app_dir(crate::paths::test_scratch_app_dir());
    }

    /// record 聚合到内存增量：daily/hourly/keys 正确累加。
    #[test]
    fn record_aggregates_deltas() {
        let _lock = crate::paths::test_app_dir_lock();
        // 必须有自己的 app_dir：这条用例只断内存聚合，但 stop 的最后一次 flush
        // 会把这 3 条事件照 `current_year_db_path()` 落库 —— 少了这一行，它落的
        // 就是**上一个用例已经删掉**的那个目录，于是那个目录连着 focusflow_YYYY.db
        // 被重新建出来（实测每个全量跑多一个 %TEMP% 残留）。
        let _tmp = crate::paths::test_app_dir("writer_agg");
        let w = start_writer(Duration::from_secs(3600));
        let ts = queries::now_ts();
        w.record("A", ts);
        w.record("A", ts);
        w.record("B", ts);
        let agg = w.state.agg.lock().unwrap_or_else(|e| e.into_inner());
        let dk = queries::day_key_of_ts(ts);
        assert_eq!(agg.daily.get(&dk), Some(&3));
        assert_eq!(agg.hourly.get(&(dk, queries::hour_of_ts(ts))), Some(&3));
        assert_eq!(agg.keys.get(&dk).and_then(|m| m.get("A")), Some(&2));
        drop(agg);
        w.stop_and_wait();
    }

    /// 活跃时长：与上一事件间隔 ≤ 60 秒累计连续活跃，超间隔或首事件不累计。
    #[test]
    fn record_tracks_active_seconds() {
        let _lock = crate::paths::test_app_dir_lock();
        // today_active 初始基准来自全局库的 active_seconds 表，
        // 必须用独立目录隔离，否则并行/残留数据会污染断言。
        let _tmp = crate::paths::test_app_dir("writer_active");
        let w = start_writer(Duration::from_secs(3600));
        let t0 = queries::now_ts();
        w.record("A", t0);
        w.record("A", t0 + 10); // 间隔 10s：活跃 +10
        w.record("A", t0 + 100); // 间隔 90s > 60s：不累计
        assert_eq!(w.state.today_active.load(Ordering::Relaxed), 10);
        w.record("A", t0 + 102); // 间隔 2s：活跃 +2
        assert_eq!(w.state.today_active.load(Ordering::Relaxed), 12);
        w.stop_and_wait();
    }

    /// 导入/外部写库之后，两个今日缓存都得跟着库走。
    ///
    /// 以前只刷按键数：落库是 `seconds = seconds + excluded.seconds` 的增量式，
    /// 所以导入进来的活跃时长不会丢，但内存里的基准还是旧的 —— 界面上
    /// 「今日活跃时长」会一直少着一截，直到下次重启才补回来。
    #[test]
    fn recompute_today_totals_refreshes_both_count_and_active_seconds() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("writer_recompute");
        let w = start_writer(Duration::from_secs(3600));
        let t0 = queries::now_ts();
        w.record("A", t0);
        w.record("A", t0 + 10); // 今日活跃 10 秒
        w.flush(true);

        let dk = queries::day_key_of_ts(t0);
        // 模拟"导入把今天的数抬高了"：直接改库
        {
            let conn = connection::open_rw(&paths::current_year_db_path()).unwrap();
            conn.execute(
                "UPDATE daily_counts SET count = count + 500, seconds = seconds + 4000 \
                 WHERE date_key = ?1",
                [dk],
            )
            .unwrap();
        }
        assert_eq!(w.today_count(), 2, "还没刷：内存仍是旧值（这条前提得成立）");
        assert_eq!(w.today_active_seconds(), 10);

        w.recompute_today_totals();
        assert_eq!(w.today_count(), 502, "按键数跟着库走");
        assert_eq!(w.today_active_seconds(), 4010, "活跃时长也得跟着库走");
        w.stop_and_wait();
    }

    /// 库被独占、flush 超时（3 秒）时重锚今日基准，绝不能把尚未落库的增量抹掉。
    ///
    /// 修前：只等 3 秒就照抄库值 → 今日按键数从那一刻起整天偏小，
    /// 「总计」卡片和悬浮窗跟着偏（`today_count` 之后只会往上加，补不回来）。
    #[test]
    fn recompute_today_totals_does_not_lose_deltas_that_have_not_landed() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("writer_recompute_locked");
        let w = start_writer(Duration::from_secs(3600));
        let t0 = queries::now_ts();
        for i in 0..5 {
            w.record("A", t0 + i);
        }
        assert_eq!(w.today_count(), 5, "前提：内存基准含这 5 次");

        // 另开一个连接把库独占住 → 写线程落不了库，flush(true) 必然超时
        let path = paths::current_year_db_path();
        {
            let blocker = connection::open_rw(&path).unwrap();
            blocker.execute_batch("BEGIN EXCLUSIVE;").unwrap();
            w.recompute_today_totals();
            assert!(
                w.today_count() >= 5,
                "未落库的 5 次必须还在今日里，实际 {}",
                w.today_count()
            );
            blocker.execute_batch("COMMIT;").ok();
        }
        w.stop_and_wait();
        assert!(w.today_count() >= 5, "落库之后今日计数更不能倒退");
    }

    /// 恢复文件写盘失败时，增量必须仍留在内存里。
    /// 修前是"先从内存取走、再写盘"，写失败只留一行 error 日志就 return
    /// → 那批数据既不在库里也不在文件里，凭空消失（而且正是在写线程已经
    /// 不收增量的时刻，没有任何补救途径）。
    #[test]
    fn snapshot_recovery_keeps_deltas_in_memory_when_the_write_fails() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("recovery_writefail");
        let w = start_writer(Duration::from_secs(3600));
        w.record("A", queries::now_ts());
        // 让写盘必失败：临时文件路径上放一个同名目录
        let tmp = recovery_path().with_extension("json.tmp");
        std::fs::create_dir_all(&tmp).unwrap();

        snapshot_recovery(&w.state, true);

        let still_there = w
            .state
            .agg
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .daily
            .values()
            .sum::<i64>();
        assert_eq!(
            still_there, 1,
            "写盘失败时不能把增量从内存取走（修前：已被 take 走，数据凭空消失）"
        );
        std::fs::remove_dir_all(&tmp).ok();
        w.stop_and_wait();
    }

    /// 启动回放留下的副本，要等首批增量真的落库才删。
    ///
    /// 修前是"读到即删"，而回放进内存的数据要等首次周期 flush（默认 10 秒）才进库
    /// → 这 10 秒内再崩一次就是二次丢失，且盘上连痕迹都没有。
    #[test]
    fn recovery_copy_survives_until_the_replayed_deltas_land() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("recovery_keep");
        // 造一份恢复文件：先起一个 writer，收到增量后按 stop 超时那条路做快照
        {
            let w = start_writer(Duration::from_secs(3600));
            w.record("A", queries::now_ts());
            w.record("A", queries::now_ts() + 1);
            snapshot_recovery(&w.state, true);
            assert!(recovery_path().exists(), "快照应写出恢复文件");
            w.stop_and_wait();
        }

        // 第二次"启动"：回放。原文件必须被改名带走（不能留在原地等着被重放第二次），
        // 副本先留着 —— 此时数据还没进库。
        let w = start_writer(Duration::from_millis(200));
        assert!(
            !recovery_path().exists(),
            "回放后原恢复文件不能再留在盘上（否则下次重放两次）"
        );
        assert!(
            recovery_kept_path().exists(),
            "回放要把恢复文件改名留成副本，读到就删等于只留 10 秒证据"
        );
        assert!(w.today_count() >= 2, "回放的增量要计入今日基准");

        // 等首批增量落库（interval=200ms）→ 副本此时才该消失
        for _ in 0..50 {
            if !recovery_kept_path().exists() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        assert!(
            !recovery_kept_path().exists(),
            "首批增量落库后回放副本必须被删掉，别永久留在 data/ 里"
        );
        w.stop_and_wait();
    }

    /// 今日尚未落库的按键增量：统计线程拿它补「近 N 天」那张卡片。
    ///
    /// 两条都得钉住，否则补法会双重计数：**未落库时它就是那批增量**，
    /// 而落库之后必须归零（同一批已经进了库里的窗口和）。
    #[test]
    fn today_pending_count_covers_only_unflushed_deltas() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("writer_pending");
        let w = start_writer(Duration::from_secs(3600));
        assert_eq!(w.today_pending_count(), 0, "开局没有未落库增量");

        let t0 = queries::now_ts();
        for i in 0..6 {
            w.record("A", t0 + i);
        }
        assert_eq!(w.today_pending_count(), 6, "这 6 次还没落库");

        w.flush(true);
        assert_eq!(
            w.today_pending_count(),
            0,
            "落库之后必须归零 —— 否则统计线程把同一批加两遍"
        );
        w.stop_and_wait();
    }

    /// 应用时长归属：间隔秒数在**事件发生时**归给当时的前台应用，
    /// 与活跃时长共用门限（≥/≤ 一致），无归属时只记活跃、不记应用。
    #[test]
    fn record_credits_gap_to_current_app() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("writer_credits");
        let w = start_writer(Duration::from_secs(3600));
        let t0 = queries::now_ts();
        let dk = queries::day_key_of_ts(t0);

        w.record("A", t0); // 首事件：无间隔可归
        w.set_current_app(Some("Obsidian.exe"));
        w.record("A", t0 + 10); // 间隔 10s → Obsidian
        w.set_current_app(Some("WorkBuddy.exe"));
        w.record("A", t0 + 30); // 间隔 20s → WorkBuddy
        w.record("A", t0 + 200); // 间隔 170s > 60s：既不活跃也不归属
        w.set_current_app(None);
        w.record("A", t0 + 205); // 间隔 5s：只有活跃，无应用归属

        let agg = w.state.agg.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(agg.active.get(&dk), Some(&35), "活跃 = 10 + 20 + 5");
        assert_eq!(agg.apps.get(&(dk, "Obsidian.exe".to_string())), Some(&10));
        assert_eq!(agg.apps.get(&(dk, "WorkBuddy.exe".to_string())), Some(&20));
        assert_eq!(agg.apps.len(), 2, "无归属的 5 秒不应落到任何应用上");
        drop(agg);
        w.stop_and_wait();
    }

    /// current_app 是会话态：flush 取走增量后必须保留，
    /// 否则每次落库（10 秒一次）后的第一个间隔都会丢掉归属。
    #[test]
    fn current_app_persists_across_flush() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("writer_curapp");
        let w = start_writer(Duration::from_secs(3600));
        let t0 = queries::now_ts();
        let dk = queries::day_key_of_ts(t0);

        w.set_current_app(Some("Obsidian.exe"));
        w.record("A", t0);
        w.record("A", t0 + 5);
        {
            let agg = w.state.agg.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(agg.apps.get(&(dk, "Obsidian.exe".to_string())), Some(&5));
        }
        w.flush(true);
        {
            let agg = w.state.agg.lock().unwrap_or_else(|e| e.into_inner());
            assert!(agg.apps.is_empty(), "flush 应取走应用增量");
            assert_eq!(
                agg.current_app.as_deref(),
                Some("Obsidian.exe"),
                "当前前台应用应跨 flush 保留"
            );
        }
        w.record("A", t0 + 10);
        {
            let agg = w.state.agg.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(
                agg.apps.get(&(dk, "Obsidian.exe".to_string())),
                Some(&5),
                "flush 后的事件仍应归属到同一应用"
            );
        }
        w.stop_and_wait();
    }

    /// 设备 × 键名明细：累加、落库，且不污染主统计键名表。
    #[test]
    fn record_device_key_persists_rows() {
        use chrono::Datelike;
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("writer_devkey");
        let w = start_writer(Duration::from_secs(3600));
        let ts = queries::now_ts();
        let dk = queries::day_key_of_ts(ts);
        let dev = "HID#VID_046D&PID_C52B";

        w.record_device(dev, "我的新鼠标 · 046D/C52B", "mouse", ts);
        w.record_device_key(dev, "鼠标左键", ts);
        w.record_device_key(dev, "鼠标左键", ts);
        w.record_device_key(dev, "滚轮上滑", ts);
        {
            let agg = w.state.agg.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(
                agg.device_keys
                    .get(&(dk, dev.to_string(), "鼠标左键".to_string())),
                Some(&2)
            );
            assert!(agg.keys.is_empty(), "设备键名不得进主统计键名表");
        }
        w.flush(true);
        let year = chrono::Local::now().date_naive().year();
        let conn = crate::db::connection::open_ro(&paths::year_db_path(year)).unwrap();
        let read = |key: &str| -> i64 {
            conn.query_row(
                "SELECT k.count FROM device_key_counts k \
                   JOIN devices d ON d.id = k.device_id \
                 WHERE k.date_key = ?1 AND d.device_key = ?2 AND k.key_name = ?3",
                rusqlite::params![dk, dev, key],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(read("鼠标左键"), 2);
        assert_eq!(read("滚轮上滑"), 1);
        drop(conn);
        w.stop_and_wait();
    }

    /// 设备维度：record_device 只累加 devices 聚合，不污染主统计；
    /// 类型冲突升级 hybrid；登记信息保留在会话态（flush 后仍在）。
    #[test]
    fn record_device_isolated_from_main_stats() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("writer_device");
        let w = start_writer(Duration::from_secs(3600));
        let ts = queries::now_ts();
        let dk = queries::day_key_of_ts(ts);

        w.record("A", ts);
        w.record_device(
            "HID#VID_046D&PID_C52B",
            "我的新鼠标 · 046D/C52B",
            "mouse",
            ts,
        );
        w.record_device(
            "HID#VID_046D&PID_C52B",
            "我的新鼠标 · 046D/C52B",
            "mouse",
            ts,
        );
        // 同一实例产生键盘事件 → hybrid
        w.record_device(
            "HID#VID_046D&PID_C52B",
            "我的新鼠标 · 046D/C52B",
            "keyboard",
            ts,
        );

        {
            let agg = w.state.agg.lock().unwrap_or_else(|e| e.into_inner());
            let dev = "HID#VID_046D&PID_C52B".to_string();
            assert_eq!(agg.devices.get(&(dk, dev.clone())), Some(&3));
            assert_eq!(
                agg.daily.get(&dk),
                Some(&1),
                "record_device 不得计入主统计 daily"
            );
            // 主统计只含显式 record 的那 1 次按键，设备事件不新增键名
            assert_eq!(
                agg.keys.get(&dk).and_then(|m| m.get("A")),
                Some(&1),
                "record_device 不得计入键名排行"
            );
            assert_eq!(agg.keys[&dk].len(), 1, "设备事件不得新增键名");
            assert_eq!(
                agg.device_meta.get(&dev).map(|m| m.kind.as_str()),
                Some("hybrid")
            );
        }
        w.flush(true);
        // flush 后登记信息仍在内存（会话态），计数已取走
        {
            let agg = w.state.agg.lock().unwrap_or_else(|e| e.into_inner());
            assert!(agg.devices.is_empty(), "flush 应取走设备计数");
            assert!(!agg.device_meta.is_empty(), "设备登记应跨 flush 保留");
        }
        w.stop_and_wait();
    }

    /// 设备维度落库：device_counts 累加 + devices 登记表写入。
    #[test]
    fn flush_writes_device_tables() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("writer_devflush");
        let w = start_writer(Duration::from_secs(3600));
        let ts = queries::now_ts();
        let dk = queries::day_key_of_ts(ts);
        let dev = "HID#VID_1234&PID_5678".to_string();

        w.record_device(&dev, "测试鼠标 · 1234/5678", "mouse", ts);
        w.record_device(&dev, "测试鼠标 · 1234/5678", "mouse", ts);
        w.flush(true);

        let conn = connection::open_ro(&paths::current_year_db_path()).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT c.count FROM device_counts c JOIN devices d ON d.id = c.device_id \
                 WHERE c.date_key = ?1 AND d.device_key = ?2",
                rusqlite::params![dk, dev],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
        let (id, name, kind): (i64, String, String) = conn
            .query_row(
                "SELECT id, name, kind FROM devices WHERE device_key = ?1",
                [&dev],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(name, "测试鼠标 · 1234/5678");
        assert_eq!(kind, "mouse");
        // 字典化：统计表存 id，且只登记一行（两次事件不重复登记）
        assert!(id > 0, "devices 必须有整数 id");
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM devices WHERE device_key = ?1",
                [&dev],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1);
        w.stop_and_wait();
    }

    /// 启动预热：库里已登记的设备名/类型应在首个输入事件之前就可用。
    ///
    /// 这消掉了「恢复回放 → 首次 flush」必然缺 device_meta 的空窗
    /// （`AggDeltasFile` 不带该字段），也让刚启动到用户第一次动键鼠之间的
    /// 那段时间无需依赖占位补登。占位行（name == device_key / 空名）必须被跳过，
    /// 否则等于把脏名字读回来固化成「真名」。
    #[test]
    fn start_preloads_device_meta_from_registry() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("writer_preload");

        let year = chrono::Local::now().year();
        {
            let conn = connection::open_rw(&paths::current_year_db_path()).unwrap();
            connection::ensure_schema(&conn, year).unwrap();
            let ins = |key: &str, name: &str, kind: &str| {
                conn.execute(
                    "INSERT OR REPLACE INTO devices (device_key, name, kind) VALUES (?1, ?2, ?3)",
                    rusqlite::params![key, name, kind],
                )
                .unwrap();
            };
            ins("HID#VID_24AE&PID_1464#a", "我的鼠标 · 24AE/1464", "mouse");
            ins(
                "HID#VID_07D7&PID_EFFF#b",
                "我的键盘 · 07D7/EFFF",
                "keyboard",
            );
            // 占位行：名字就是路径本身（历史补登的产物），不得被预热读回去
            ins(
                r"\\?\HID#VID_046D&PID_C52B&MI_00#8&2c5f&0&0000",
                r"\\?\HID#VID_046D&PID_C52B&MI_00#8&2c5f&0&0000",
                "unknown",
            );
            ins("HID#VID_1111&PID_2222#c", "", "mouse");
        }

        let w = start_writer(Duration::from_secs(3600));
        {
            let agg = w.state.agg.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(
                agg.device_meta
                    .get("HID#VID_24AE&PID_1464#a")
                    .map(|m| m.name.as_str()),
                Some("我的鼠标 · 24AE/1464"),
                "已登记设备应被预热"
            );
            assert_eq!(
                agg.device_meta
                    .get("HID#VID_07D7&PID_EFFF#b")
                    .map(|m| m.kind.as_str()),
                Some("keyboard"),
                "设备类型也应一并预热"
            );
            assert!(
                !agg.device_meta
                    .contains_key(r"\\?\HID#VID_046D&PID_C52B&MI_00#8&2c5f&0&0000"),
                "name == device_key 的占位行不得被当成真名读回"
            );
            assert!(
                !agg.device_meta.contains_key("HID#VID_1111&PID_2222#c"),
                "空名登记行不得被预热"
            );
            assert_eq!(agg.device_meta.len(), 2, "只有两条真实登记应进入会话态");
        }
        w.stop_and_wait();
    }

    /// 缺设备登记时的兜底命名：不能把裸路径写进 devices.name（2026-09-21）。
    ///
    /// 恢复文件回放会把 device_meta 置空（会话态不透传），首次 flush 走 meta=None 分支；
    /// 而查询侧只在 name 为空时才回退，所以这里必须写出人话，
    /// 否则设备排行会顶出一长串 `\\?\HID#VID_...`，直到该设备下一次真名 flush 为止。
    #[test]
    fn device_id_in_db_registers_readable_name_without_meta() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("writer_devfb");

        let year = chrono::Local::now().year();
        let conn = connection::open_rw(&paths::current_year_db_path()).unwrap();
        connection::ensure_schema(&conn, year).unwrap();
        const DEV: &str = r"\\?\HID#VID_046D&PID_C52B&MI_00#8&2c5f&0&0000#{378de44c}";
        const UPSERT: &str = "INSERT INTO devices (device_key, name, kind) VALUES (?1, ?2, ?3) \
             ON CONFLICT(device_key) DO UPDATE SET name = excluded.name, kind = excluded.kind \
             RETURNING id";

        let mut upsert = conn.prepare(UPSERT).unwrap();
        let id = device_id_in_db(&mut upsert, DEV, None).unwrap();
        drop(upsert);
        assert!(id > 0, "补登必须拿到整数 id");
        let name: String = conn
            .query_row("SELECT name FROM devices WHERE id = ?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "HID 设备 · 046D/C52B", "缺登记时应写回退命名");
        assert_ne!(name, DEV, "绝不能是裸设备实例路径");

        // 设备补齐后（真名随首个真实事件到达）UPSERT 应覆盖成真名且不新开登记行
        let real = DeviceMeta {
            name: "我的新鼠标 · 046D/C52B".to_string(),
            kind: "mouse".to_string(),
        };
        let mut upsert = conn.prepare(UPSERT).unwrap();
        let id2 = device_id_in_db(&mut upsert, DEV, Some(&real)).unwrap();
        drop(upsert);
        assert_eq!(id2, id, "补名必须命中同一登记行");
        let (name2, kind2): (String, String) = conn
            .query_row("SELECT name, kind FROM devices WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(name2, "我的新鼠标 · 046D/C52B");
        assert_eq!(kind2, "mouse");
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM devices", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1, "同一设备只登记一行");

        drop(conn);
    }

    /// 端到端：崩溃恢复回放后的首次落库应当用**真名**补登，而不是占位名。
    ///
    /// 恢复文件故意不带 device_meta，回放后落库只能靠 `preload_device_meta`
    /// 把库里的登记表读回来。缺这层时这里写进去的是 `[recovery]*` 前的回退名
    /// （"HID 设备 · 24AE/1464"），虽然不再是机器串，但仍丢失了用户看到的设备名。
    #[test]
    fn recovery_replay_writes_real_device_name() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("writer_devname");

        let ts = queries::now_ts();
        let dev = "HID#VID_24AE&PID_1464#e2e".to_string();
        let real_name = "端到端鼠标 · 24AE/1464";
        // B14-2：落库前 device_id_in_db 会把键归一到硬件身份段 ——
        // 登记行按身份键存，查询也得按身份键问
        let dev_in_db = crate::device_alias::hardware_identity_key(&dev);
        let name_in_db = || -> Option<String> {
            connection::open_ro(&paths::current_year_db_path())
                .ok()
                .and_then(|conn| {
                    conn.query_row(
                        "SELECT name FROM devices WHERE device_key=?1",
                        [&dev_in_db],
                        |r| r.get(0),
                    )
                    .ok()
                })
        };

        // 1. 正常记一次并落库 —— 登记行进入 devices 表（预热的数据来源）
        let w = start_writer(Duration::from_secs(3600));
        w.record_device(&dev, real_name, "mouse", ts);
        w.flush(true);
        assert_eq!(name_in_db().as_deref(), Some(real_name), "首次落库应是真名");

        // 2. 又有两次输入，进程被强杀：这 2 次只存在于恢复文件里
        w.record_device(&dev, real_name, "mouse", ts);
        w.record_device(&dev, real_name, "mouse", ts);
        snapshot_recovery(&w.state, true);
        w.stop_and_wait();

        // 3. 重启回放 + 落库：登记名字必须是真名，不能被回退命名顶掉
        let w2 = start_writer(Duration::from_secs(3600));
        w2.flush(true);
        let deadline = Instant::now() + Duration::from_secs(15);
        while w2.flush_seq() == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(
            name_in_db().as_deref(),
            Some(real_name),
            "恢复回放后的补登应保留原设备名，预热失效时会退化成回退名"
        );
        let cnt: i64 = connection::open_ro(&paths::current_year_db_path())
            .ok()
            .and_then(|conn| {
                conn.query_row(
                    "SELECT COALESCE(SUM(c.count),0) FROM device_counts c \
                       JOIN devices d ON d.id = c.device_id \
                      WHERE d.device_key=?1",
                    [&dev_in_db],
                    |r| r.get(0),
                )
                .ok()
            })
            .unwrap_or(0);
        assert_eq!(cnt, 3, "1 次直接落库 + 2 次回放，不得重复或丢计数");
        w2.stop_and_wait();
    }

    /// 恢复机制：设备计数随快照落盘并回放，恰好落库一次。
    #[test]
    fn recovery_replays_device_counts_once() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("writer_devrec");

        let w = start_writer(Duration::from_secs(3600));
        let ts = queries::now_ts();
        let dk = queries::day_key_of_ts(ts);
        let dev = "HID#VID_AAAA&PID_BBBB".to_string();
        w.record_device(&dev, "恢复测试键盘 · AAAA/BBBB", "keyboard", ts);
        w.record_device(&dev, "恢复测试键盘 · AAAA/BBBB", "keyboard", ts);
        snapshot_recovery(&w.state, true);
        w.stop_and_wait();

        let db_count = || -> i64 {
            connection::open_ro(&paths::current_year_db_path())
                .ok()
                .and_then(|conn| {
                    conn.query_row(
                        "SELECT COALESCE(SUM(c.count),0) FROM device_counts c \
                           JOIN devices d ON d.id = c.device_id \
                         WHERE c.date_key=?1 AND d.device_key=?2",
                        rusqlite::params![dk, dev],
                        |r| r.get(0),
                    )
                    .ok()
                })
                .unwrap_or(0)
        };
        let base = db_count();

        let w2 = start_writer(Duration::from_secs(3600));
        w2.flush(true);
        let deadline = Instant::now() + Duration::from_secs(15);
        while w2.flush_seq() == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(db_count(), base + 2, "设备计数应恰好回放一次");
        w2.stop_and_wait();
    }

    /// 恢复机制：快照落盘并从内存移除 → 正常 flush 不再落库 →
    /// 重启回放后恰好落库一次且文件被删除（不重复计数）。
    #[test]
    fn recovery_snapshot_replayed_once() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("recovery");

        let w = start_writer(Duration::from_secs(3600));
        let ts = queries::now_ts();
        w.record("A", ts);
        w.record("A", ts);
        snapshot_recovery(&w.state, true);
        assert!(recovery_path().exists(), "快照应写入恢复文件");
        w.stop_and_wait(); // 内存已空，stop 的最终 flush 不会再写库

        // 并行测试共享全局 app_dir，库中可能有其他测试的数据，
        // 因此全部用相对断言验证"回放数据恰好落库一次"。
        // 库文件可能尚未创建（本测试早于任何 flush 运行）：缺失视为 0。
        let dk = queries::day_key_of_ts(ts);
        let db_count = || -> i64 {
            connection::open_ro(&paths::current_year_db_path())
                .ok()
                .and_then(|conn| {
                    conn.query_row(
                        "SELECT COALESCE(SUM(count),0) FROM daily_counts WHERE date_key=?1",
                        [dk],
                        |r| r.get(0),
                    )
                    .ok()
                })
                .unwrap_or(0)
        };
        let base = db_count();

        let w2 = start_writer(Duration::from_secs(3600));
        assert!(w2.today_count() >= 2, "回放的今日计数应计入缓存基准");
        assert_eq!(w2.flush_seq(), 0);
        w2.flush(true);
        // flush(true) 只等 3 秒，并行测试短暂占用 DB 写锁时可能提前返回；
        // 数据由写线程异步落库，这里轮询等待（序号递增 = 落库成功）。
        let deadline = Instant::now() + Duration::from_secs(15);
        while w2.flush_seq() == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(w2.flush_seq(), 1, "成功落库后序号应递增");
        assert_eq!(db_count(), base + 2, "回放数据应恰好落库一次");
        assert!(!recovery_path().exists(), "恢复文件回放后应删除");
        w2.stop_and_wait();
    }
}
