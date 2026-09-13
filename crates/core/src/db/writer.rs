//! 数据库写入器：内存聚合 + 周期落库。
//!
//! 按键事件不再逐条落库，而是在内存中按 (天, 小时) / (天, 按键) 聚合，
//! 每 10 秒（或 flush 信号）把增量 UPSERT 到聚合表。相比逐事件写入：
//! - 数据库体积约为原来的 1/170（一年约 1MB 而非 180MB）
//! - 写入频率固定，不受按键速度影响
//! - flush 信号：立即落库 + 等待完成（退出/备份用）

use std::collections::HashMap;
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

/// 连续活跃判定：与上一事件间隔 ≤ 该秒数视为连续活跃（累计活跃时长）。
/// 连续活跃判定窗口（秒）：事件间隔 ≤ 该值视为同一段连续活跃。
/// 前台应用时长累计（app_stats）复用同一口径。
pub(crate) const ACTIVE_GAP_SECS: i64 = 60;

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
    /// 上一个事件的时间戳（连续活跃判定用；flush 取走增量时保留）
    last_ts: i64,
}

impl AggDeltas {
    fn is_empty(&self) -> bool {
        self.daily.is_empty()
            && self.hourly.is_empty()
            && self.keys.is_empty()
            && self.active.is_empty()
            && self.apps.is_empty()
    }

    /// 取走待落库增量。`last_ts` 保留在内存聚合中，
    /// 否则每次 flush 都会打断连续活跃判定（每 10 秒白丢一段时长）。
    fn take_for_flush(&mut self) -> AggDeltas {
        AggDeltas {
            daily: std::mem::take(&mut self.daily),
            hourly: std::mem::take(&mut self.hourly),
            keys: std::mem::take(&mut self.keys),
            active: std::mem::take(&mut self.active),
            apps: std::mem::take(&mut self.apps),
            last_ts: self.last_ts,
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
}

/// 写入器句柄（Send + Sync，可跨线程持有）。
pub struct DbWriter {
    state: Arc<WriterState>,
}

impl DbWriter {
    /// 创建并启动写入线程。
    pub fn start(flush_interval: Duration) -> Arc<Self> {
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
                        "SELECT COALESCE(seconds, 0) FROM active_seconds WHERE date_key = ?1",
                        [queries::day_key_of_date(chrono::Local::now().date_naive())],
                        |r| r.get::<_, i64>(0),
                    )
                    .ok()
                })
                .unwrap_or(0)
                .max(0) as u64
        };
        // 启动回放：上次进程异常终止时写入的未落库增量（若存在）并入内存聚合，
        // 随首次周期 flush 正常落库。读取后立即删除，保证只回放一次不重复计数。
        let recovered = take_recovery();
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
        let state = Arc::new(WriterState {
            agg: Mutex::new(recovered.unwrap_or_default()),
            sig_tx,
            today_count: AtomicU64::new(today_base_count),
            today_active: AtomicU64::new(today_base_active),
            today_key: AtomicU64::new(current_day_key()),
            alive: AtomicBool::new(true),
            flush_seq: AtomicU64::new(0),
        });

        let writer = Arc::new(Self {
            state: Arc::clone(&state),
        });

        let state2 = Arc::clone(&state);
        thread::Builder::new()
            .name("db-writer".into())
            .spawn(move || writer_loop(state2, sig_rx, flush_interval))
            .expect("启动 DB 写入线程失败");

        tracing::info!(
            "DB 写入线程已启动 (聚合写入, interval={:?})",
            flush_interval
        );
        writer
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

    /// 采集线程调用：累计前台应用使用秒数（进写线程内存聚合，随周期 flush 落库）。
    pub(crate) fn add_app_seconds(&self, date_key: i64, app_name: &str, seconds: i64) {
        let mut agg = self.state.agg.lock().unwrap_or_else(|e| e.into_inner());
        *agg.apps
            .entry((date_key, app_name.to_string()))
            .or_insert(0) += seconds;
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

    /// 最近一次键鼠事件的 Unix 时间戳（无事件为 0，flush 取走增量时保留）。
    /// 前台应用时长累计据此判定"活跃窗口"：挂机（超窗口无事件）时不累计。
    pub fn last_event_ts(&self) -> i64 {
        self.state
            .agg
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_ts
    }

    /// 重置今日计数缓存（外部清除数据后调用）。
    pub fn reset_today_count(&self) {
        self.state.today_count.store(0, Ordering::Relaxed);
    }

    /// 重新统计今日计数（导入/外部写入数据后调用，避免缓存与库不一致）。
    /// 先落库再读聚合表，保证计数准确。
    pub fn recompute_today_count(&self) {
        self.flush(true);
        self.state
            .today_key
            .store(current_day_key(), Ordering::Relaxed);
        let count = connection::open_ro(&paths::current_year_db_path())
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
            .max(0) as u64;
        self.state.today_count.store(count, Ordering::Relaxed);
    }

    /// 立即 flush：发信号让写线程落库。`wait=true` 时阻塞等待完成。
    pub fn flush(&self, wait: bool) {
        if wait {
            let (tx, rx) = mpsc::channel();
            let _ = self.state.sig_tx.send(Signal::Flush { done: Some(tx) });
            if rx.recv_timeout(Duration::from_secs(3)).is_err() {
                tracing::warn!("flush 等待超时（3 秒），写线程可能繁忙，增量仍在内存中");
            }
        } else {
            let _ = self.state.sig_tx.send(Signal::Flush { done: None });
        }
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

    /// 成功落库序号（有实际写入才递增）。
    pub fn flush_seq(&self) -> u64 {
        self.state.flush_seq.load(Ordering::Relaxed)
    }
}

/// 未落库增量恢复文件路径。
fn recovery_path() -> std::path::PathBuf {
    paths::data_dir().join("agg_recovery.json")
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
            last_ts: f.last_ts,
        }
    }
}

/// 把未落库增量快照写到恢复文件。
/// `take=true` 时同时从内存聚合移除：用于 stop 超时 / panic（进程即将终止），
/// 数据此后只存在于文件中，避免写线程随后恢复后再次落库造成重复计数。
fn snapshot_recovery(state: &WriterState, take: bool) {
    // try_lock：panic 可能发生在持有 agg 锁的线程，此时放弃快照（进程即将终止）
    let Ok(mut agg) = state.agg.try_lock() else {
        return;
    };
    if agg.is_empty() {
        return;
    }
    let pending = if take {
        agg.take_for_flush()
    } else {
        agg.clone()
    };
    drop(agg);
    let json = match serde_json::to_string(&AggDeltasFile::from(&pending)) {
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
        Ok(()) => tracing::warn!(
            "未落库增量已写入恢复文件 {}（下次启动回放）",
            path.display()
        ),
        Err(e) => tracing::error!("恢复文件写入失败 {}: {e}", path.display()),
    }
}

/// 读取并删除恢复文件（启动回放）。解析失败同样删除：残缺文件重试无意义。
fn take_recovery() -> Option<AggDeltas> {
    let path = recovery_path();
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    match serde_json::from_str::<AggDeltasFile>(&text) {
        Ok(v) => Some(v.into()),
        Err(e) => {
            tracing::error!("恢复文件解析失败（已丢弃）: {e}");
            None
        }
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
    for (year, part) in parts.drain() {
        match flush_partition(conn, conn_year, year, &part) {
            Ok(()) => {
                state.flush_seq.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                tracing::error!("聚合落库最终失败（{year} 年）: {e}");
                failed.push(part);
            }
        }
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
    }
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
                {
                    let mut stmt = c.prepare(
                        "INSERT INTO daily_counts (date_key, count) VALUES (?1, ?2)
                         ON CONFLICT(date_key) DO UPDATE SET count = count + excluded.count",
                    )?;
                    for (dk, n) in &pending.daily {
                        stmt.execute(rusqlite::params![dk, n])?;
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
                        "INSERT INTO active_seconds (date_key, seconds) VALUES (?1, ?2)
                         ON CONFLICT(date_key) DO UPDATE SET seconds = seconds + excluded.seconds",
                    )?;
                    for (dk, n) in &pending.active {
                        stmt.execute(rusqlite::params![dk, n])?;
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

    /// 跨年落库：增量必须写进 date_key 所属年份的库文件。
    ///
    /// 回归：此前统一写「当前年份」的库，跨年夜 23:59 的按键会落进新年度的文件。
    /// 后果是按日期查询漏读、而年度归档因目标库已有同 date_key 主键冲突整体回滚。
    #[test]
    fn flush_writes_each_year_to_its_own_db() {
        let _lock = crate::paths::test_app_dir_lock();
        let dir = std::env::temp_dir().join(format!("ff_writer_crossyear_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).ok();
        crate::paths::set_app_dir(&dir);

        let w = DbWriter::start(Duration::from_secs(3600));

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

        w.stop();
        crate::paths::set_app_dir(std::env::temp_dir().join("ff_restore_nonexistent"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 跨天边界：today_key 落后于当前日期时，record 应重置今日计数
    /// （统计线程运行中跨 0 点，避免次日显示昨日累计值）。
    #[test]
    fn record_resets_today_count_on_day_change() {
        let _lock = crate::paths::test_app_dir_lock();
        let dir = std::env::temp_dir().join(format!("ff_writer_day_{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        crate::paths::set_app_dir(&dir);

        let w = DbWriter::start(Duration::from_secs(3600));
        // 模拟"昨天"：today_key 是昨天的日期键、计数停留在昨日值
        w.state
            .today_key
            .store(current_day_key() - 1, Ordering::Relaxed);
        w.state.today_count.store(999, Ordering::Relaxed);

        w.record("A", queries::now_ts());

        assert_eq!(w.state.today_key.load(Ordering::Relaxed), current_day_key());
        assert_eq!(w.state.today_count.load(Ordering::Relaxed), 1);

        w.flush(true);
        w.stop();
        crate::paths::set_app_dir(std::env::temp_dir().join("ff_restore_nonexistent"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// record 聚合到内存增量：daily/hourly/keys 正确累加。
    #[test]
    fn record_aggregates_deltas() {
        let _lock = crate::paths::test_app_dir_lock();
        let w = DbWriter::start(Duration::from_secs(3600));
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
        w.stop();
    }

    /// 活跃时长：与上一事件间隔 ≤ 60 秒累计连续活跃，超间隔或首事件不累计。
    #[test]
    fn record_tracks_active_seconds() {
        let _lock = crate::paths::test_app_dir_lock();
        // today_active 初始基准来自全局库的 active_seconds 表，
        // 必须用独立目录隔离，否则并行/残留数据会污染断言。
        let dir = std::env::temp_dir().join(format!("ff_writer_active_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).ok();
        crate::paths::set_app_dir(&dir);
        let w = DbWriter::start(Duration::from_secs(3600));
        let t0 = queries::now_ts();
        w.record("A", t0);
        w.record("A", t0 + 10); // 间隔 10s：活跃 +10
        w.record("A", t0 + 100); // 间隔 90s > 60s：不累计
        assert_eq!(w.state.today_active.load(Ordering::Relaxed), 10);
        w.record("A", t0 + 102); // 间隔 2s：活跃 +2
        assert_eq!(w.state.today_active.load(Ordering::Relaxed), 12);
        w.stop();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// last_event_ts：record 后更新，flush 取走增量后保留（前台应用时长
    /// 的活跃窗口判定依赖该值在两次事件之间保持有效）。
    #[test]
    fn last_event_ts_persists_across_flush() {
        let _lock = crate::paths::test_app_dir_lock();
        let dir = std::env::temp_dir().join(format!("ff_writer_lastts_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).ok();
        crate::paths::set_app_dir(&dir);
        let w = DbWriter::start(Duration::from_secs(3600));
        assert_eq!(w.last_event_ts(), 0, "无事件时应为 0（永远不处于活跃窗口）");
        let ts = queries::now_ts();
        w.record("A", ts);
        assert_eq!(w.last_event_ts(), ts);
        w.flush(true);
        assert_eq!(w.last_event_ts(), ts, "flush 取走增量后 last_ts 应保留");
        w.stop();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 恢复机制：快照落盘并从内存移除 → 正常 flush 不再落库 →
    /// 重启回放后恰好落库一次且文件被删除（不重复计数）。
    #[test]
    fn recovery_snapshot_replayed_once() {
        let _lock = crate::paths::test_app_dir_lock();
        let dir = std::env::temp_dir().join(format!("ff_recovery_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        crate::paths::set_app_dir(&dir);

        let w = DbWriter::start(Duration::from_secs(3600));
        let ts = queries::now_ts();
        w.record("A", ts);
        w.record("A", ts);
        snapshot_recovery(&w.state, true);
        assert!(recovery_path().exists(), "快照应写入恢复文件");
        w.stop(); // 内存已空，stop 的最终 flush 不会再写库

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

        let w2 = DbWriter::start(Duration::from_secs(3600));
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
        w2.stop();
        std::fs::remove_dir_all(&dir).ok();
    }
}
