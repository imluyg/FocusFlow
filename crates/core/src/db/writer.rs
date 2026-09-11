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
const ACTIVE_GAP_SECS: i64 = 60;

/// 内存中的聚合增量（未落库部分）。
#[derive(Default)]
struct AggDeltas {
    /// (date_key) -> count
    daily: HashMap<i64, i64>,
    /// (date_key, hour) -> count
    hourly: HashMap<(i64, i64), i64>,
    /// (date_key, key_name) -> count
    keys: HashMap<(i64, String), i64>,
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
        let state = Arc::new(WriterState {
            agg: Mutex::new(AggDeltas::default()),
            sig_tx,
            today_count: AtomicU64::new(today_base_count),
            today_active: AtomicU64::new(today_base_active),
            today_key: AtomicU64::new(current_day_key()),
            alive: AtomicBool::new(true),
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
        *agg.keys.entry((day_key, key_name.to_string())).or_insert(0) += 1;
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
            let _ = rx.recv_timeout(Duration::from_secs(3));
        } else {
            let _ = self.state.sig_tx.send(Signal::Flush { done: None });
        }
    }

    /// 停止写线程（退出前 flush 残留）。
    pub fn stop(&self) {
        let _ = self.state.sig_tx.send(Signal::Stop);
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.state.alive.load(Ordering::Relaxed) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// 线程是否存活。
    pub fn is_alive(&self) -> bool {
        self.state.alive.load(Ordering::Relaxed)
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

/// 确保连接指向当前年份库（跨年时重建）。
fn ensure_connection(conn: &mut Option<Connection>, conn_year: &mut i32) {
    let now_year = paths::current_year();
    if *conn_year == now_year && conn.is_some() {
        return;
    }
    // 跨年或首次：重建连接
    *conn = None;
    let path = paths::year_db_path(now_year);
    if let Ok(new_conn) = connection::open_rw(&path) {
        if connection::ensure_schema(&new_conn, now_year).is_ok() {
            *conn = Some(new_conn);
            *conn_year = now_year;
        }
    }
}

/// 把内存增量落库（单事务 UPSERT，失败重试，最终失败回填内存避免丢数据）。
fn flush_pending(conn: &mut Option<Connection>, conn_year: &mut i32, state: &WriterState) {
    let pending = {
        let mut agg = state.agg.lock().unwrap_or_else(|e| e.into_inner());
        if agg.is_empty() {
            return;
        }
        agg.take_for_flush()
    };

    let max_retries = 3;
    let mut last_err: Option<anyhow::Error> = None;

    for attempt in 0..max_retries {
        ensure_connection(conn, conn_year);

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
                    for ((dk, key), n) in &pending.keys {
                        stmt.execute(rusqlite::params![dk, key, n])?;
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
                    "聚合落库成功: daily={} hourly={} keys={}",
                    pending.daily.len(),
                    pending.hourly.len(),
                    pending.keys.len()
                );
                return;
            }
            Err(e) => {
                last_err = Some(e);
                // 连接可能损坏，重置以强制重建
                *conn = None;
                if attempt < max_retries - 1 {
                    tracing::warn!(
                        "聚合落库失败 (第{}次), 重试: {}",
                        attempt + 1,
                        last_err.as_ref().unwrap()
                    );
                    thread::sleep(Duration::from_millis(500 * (attempt as u64 + 1)));
                }
            }
        }
    }

    tracing::error!(
        "聚合落库最终失败 (已重试{}次): {}",
        max_retries,
        last_err.as_ref().map(|e| e.to_string()).unwrap_or_default()
    );
    // 回填内存，避免数据丢失（下次周期 flush 再试）
    let mut agg = state.agg.lock().unwrap_or_else(|e| e.into_inner());
    for (dk, n) in pending.daily {
        *agg.daily.entry(dk).or_insert(0) += n;
    }
    for ((dk, h), n) in pending.hourly {
        *agg.hourly.entry((dk, h)).or_insert(0) += n;
    }
    for ((dk, key), n) in pending.keys {
        *agg.keys.entry((dk, key)).or_insert(0) += n;
    }
    for (dk, n) in pending.active {
        *agg.active.entry(dk).or_insert(0) += n;
    }
    for ((dk, app), n) in pending.apps {
        *agg.apps.entry((dk, app)).or_insert(0) += n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// 跨天边界：today_key 落后于当前日期时，record 应重置今日计数
    /// （统计线程运行中跨 0 点，避免次日显示昨日累计值）。
    #[test]
    fn record_resets_today_count_on_day_change() {
        let dir = std::env::temp_dir().join(format!("ff_writer_day_{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        crate::paths::set_app_dir(&dir);

        let w = DbWriter::start(Duration::from_secs(3600));
        // 模拟"昨天"：today_key 是昨天的日期键、计数停留在昨日值
        w.state.today_key.store(current_day_key() - 1, Ordering::Relaxed);
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
        let w = DbWriter::start(Duration::from_secs(3600));
        let ts = queries::now_ts();
        w.record("A", ts);
        w.record("A", ts);
        w.record("B", ts);
        let agg = w.state.agg.lock().unwrap_or_else(|e| e.into_inner());
        let dk = queries::day_key_of_ts(ts);
        assert_eq!(agg.daily.get(&dk), Some(&3));
        assert_eq!(
            agg.hourly.get(&(dk, queries::hour_of_ts(ts))),
            Some(&3)
        );
        assert_eq!(agg.keys.get(&(dk, "A".to_string())), Some(&2));
        drop(agg);
        w.stop();
    }

    /// 活跃时长：与上一事件间隔 ≤ 60 秒累计连续活跃，超间隔或首事件不累计。
    #[test]
    fn record_tracks_active_seconds() {
        let w = DbWriter::start(Duration::from_secs(3600));
        let t0 = queries::now_ts();
        w.record("A", t0);
        w.record("A", t0 + 10); // 间隔 10s：活跃 +10
        w.record("A", t0 + 100); // 间隔 90s > 60s：不累计
        assert_eq!(w.state.today_active.load(Ordering::Relaxed), 10);
        w.record("A", t0 + 102); // 间隔 2s：活跃 +2
        assert_eq!(w.state.today_active.load(Ordering::Relaxed), 12);
        w.stop();
    }
}
