//! 番茄工作法模块。
//!
//! 镜像 Python 版 `pomodoro.py`：
//! - 工作/休息定时器（后台线程计时）
//! - 每个番茄钟自动记录按键数据（与统计联动）
//! - 历史记录持久化到 `data/focusflow_pomodoro.db`
//! - 暂停/继续/停止/跳过

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Local;
use rusqlite::Connection;

use crate::paths;

pub const STATE_IDLE: &str = "idle";
pub const STATE_WORK: &str = "work";
pub const STATE_BREAK: &str = "break";

/// 番茄钟数据库路径。
pub fn db_path() -> std::path::PathBuf {
    paths::data_dir().join("focusflow_pomodoro.db")
}

/// 打开本地库：WAL + busy_timeout，避免并发短锁导致读写直接失败。
fn open_local() -> rusqlite::Result<Connection> {
    std::fs::create_dir_all(paths::data_dir()).ok();
    let conn = Connection::open(db_path())?;
    conn.pragma_update(None, "journal_mode", "WAL").ok();
    conn.pragma_update(None, "synchronous", "NORMAL").ok();
    conn.busy_timeout(std::time::Duration::from_secs(15)).ok();
    Ok(conn)
}

/// 初始化番茄钟数据库。
pub fn init_db() -> anyhow::Result<()> {
    std::fs::create_dir_all(paths::data_dir()).ok();
    let conn = open_local()?;
    conn.pragma_update(None, "journal_mode", "WAL").ok();
    conn.pragma_update(None, "synchronous", "NORMAL").ok();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS pomodoro_sessions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            type TEXT NOT NULL,
            start_time TEXT NOT NULL,
            end_time TEXT NOT NULL,
            planned_seconds INTEGER NOT NULL,
            actual_seconds INTEGER NOT NULL,
            key_count INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_pomo_type ON pomodoro_sessions(type);
        CREATE INDEX IF NOT EXISTS idx_pomo_created ON pomodoro_sessions(created_at);",
    )?;
    Ok(())
}

/// 会话记录。
#[derive(Debug, Clone)]
pub struct Session {
    pub id: i64,
    pub rtype: String,
    pub start_time: String,
    pub end_time: String,
    pub planned_seconds: i64,
    pub actual_seconds: i64,
    pub key_count: i64,
    pub created_at: String,
}

/// 保存一条会话记录。
pub fn save_session(s: &Session) -> anyhow::Result<()> {
    let conn = open_local()?;
    conn.execute(
        "INSERT INTO pomodoro_sessions
         (type, start_time, end_time, planned_seconds, actual_seconds, key_count, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            s.rtype,
            s.start_time,
            s.end_time,
            s.planned_seconds,
            s.actual_seconds,
            s.key_count,
            s.created_at
        ],
    )?;
    Ok(())
}

/// 按日期查询会话。
pub fn get_sessions_by_date(date_str: &str, limit: i64) -> Vec<Session> {
    let conn = match open_local() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut stmt = match conn.prepare(
        "SELECT * FROM pomodoro_sessions WHERE start_time >= ?1 AND start_time < ?2 ORDER BY id DESC LIMIT ?3",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    // 上界取「次日 00:00:00」的开区间。原先写成 `start_time < '<date> 23:59:59'`，
    // 把一天最后那一秒（23:59:59 整）开始的会话整个排除在外。
    let end_bound = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.checked_add_days(chrono::Days::new(1)))
        .map(|d| format!("{} 00:00:00", d.format("%Y-%m-%d")))
        .unwrap_or_else(|| format!("{date_str} 23:59:59"));
    let result = stmt.query_map(
        rusqlite::params![format!("{date_str} 00:00:00"), end_bound, limit],
        |r| {
            Ok(Session {
                id: r.get(0)?,
                rtype: r.get(1)?,
                start_time: r.get(2)?,
                end_time: r.get(3)?,
                planned_seconds: r.get(4)?,
                actual_seconds: r.get(5)?,
                key_count: r.get(6)?,
                created_at: r.get(7)?,
            })
        },
    );
    match result {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

/// 查询最近会话。
pub fn get_recent_sessions(limit: i64) -> Vec<Session> {
    let conn = match open_local() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut stmt = match conn.prepare("SELECT * FROM pomodoro_sessions ORDER BY id DESC LIMIT ?1") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let result = stmt.query_map([limit], |r| {
        Ok(Session {
            id: r.get(0)?,
            rtype: r.get(1)?,
            start_time: r.get(2)?,
            end_time: r.get(3)?,
            planned_seconds: r.get(4)?,
            actual_seconds: r.get(5)?,
            key_count: r.get(6)?,
            created_at: r.get(7)?,
        })
    });
    match result {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

/// 今日番茄钟汇总。
pub fn today_summary() -> (i64, i64, i64) {
    let today = Local::now().format("%Y-%m-%d").to_string();
    let sessions = get_sessions_by_date(&today, 1000);
    let work: Vec<&Session> = sessions.iter().filter(|s| s.rtype == "work").collect();
    let count = work.len() as i64;
    let total_keys = work.iter().map(|s| s.key_count).sum();
    let total_secs = work.iter().map(|s| s.actual_seconds).sum();
    (count, total_keys, total_secs)
}

/// 番茄钟定时器（后台线程驱动）。
pub struct PomodoroTimer {
    /// 内部状态（Mutex 保护）
    state: Arc<Mutex<TimerState>>,
    /// 停止事件
    stop: Arc<AtomicBool>,
}

struct TimerState {
    state: String,
    paused: bool,
    remaining: i64,
    planned: i64,
    elapsed: i64,
    key_count: i64,
    work_minutes: i64,
    break_minutes: i64,
    auto_break: bool,
    work_finished: i64,
    /// 当前阶段开始时间（保存记录用）
    start_time: String,
}

impl Default for TimerState {
    fn default() -> Self {
        Self {
            state: STATE_IDLE.to_string(),
            paused: false,
            remaining: 0,
            planned: 0,
            elapsed: 0,
            key_count: 0,
            work_minutes: 25,
            break_minutes: 5,
            auto_break: true,
            work_finished: 0,
            start_time: String::new(),
        }
    }
}

impl PomodoroTimer {
    pub fn new() -> Arc<Self> {
        let timer = Arc::new(Self {
            state: Arc::new(Mutex::new(TimerState::default())),
            stop: Arc::new(AtomicBool::new(false)),
        });
        // 启动后台计时线程
        let stop = Arc::clone(&timer.stop);
        let state = Arc::clone(&timer.state);
        std::thread::Builder::new()
            .name("pomodoro".into())
            .spawn(move || tick_loop(state, stop))
            .expect("启动番茄钟线程失败");
        timer
    }

    pub fn get_state_info(&self) -> std::collections::HashMap<String, i64> {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut m = std::collections::HashMap::new();
        m.insert("state".into(), state_code(&s.state));
        m.insert("paused".into(), s.paused as i64);
        m.insert("remaining".into(), s.remaining);
        m.insert("planned".into(), s.planned);
        m.insert("key_count".into(), s.key_count);
        m.insert("work_finished".into(), s.work_finished);
        m.insert("work_minutes".into(), s.work_minutes);
        m.insert("break_minutes".into(), s.break_minutes);
        m.insert("auto_break".into(), s.auto_break as i64);
        m
    }

    pub fn get_state(&self) -> String {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state
            .clone()
    }

    pub fn set_durations(&self, work_minutes: i64, break_minutes: i64) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.work_minutes = work_minutes.max(1);
        s.break_minutes = break_minutes.max(1);
    }

    pub fn set_auto_break(&self, enabled: bool) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .auto_break = enabled;
    }

    pub fn start_work(&self) {
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if s.state == STATE_WORK {
                return;
            }
            save_current(&mut s);
            s.state = STATE_WORK.to_string();
            s.paused = false;
            s.planned = s.work_minutes * 60;
            s.remaining = s.planned;
            s.elapsed = 0;
            s.key_count = 0;
            s.start_time = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        }
        tracing::info!("番茄钟开始工作");
    }

    pub fn start_break(&self) {
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if s.state == STATE_BREAK {
                return;
            }
            save_current(&mut s);
            s.state = STATE_BREAK.to_string();
            s.paused = false;
            s.planned = s.break_minutes * 60;
            s.remaining = s.planned;
            s.elapsed = 0;
            s.key_count = 0;
            s.start_time = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        }
        tracing::info!("番茄钟开始休息");
    }

    pub fn toggle_pause(&self) -> bool {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.state == STATE_IDLE {
            return false;
        }
        s.paused = !s.paused;
        s.paused
    }

    /// 跳过当前阶段：**这段作废**，与 [`stop`] 唯一的差别就是不落库。
    ///
    /// 工作到一半按下来，已过的时间和本阶段键鼠数都不写进会话表，因此既不占
    /// 今日番茄数也不进键鼠统计。刻意保留这个语义（而不是让「跳过」记成一个
    /// 完成）：没做完的一段被数成"做完了 1 个"，比丢掉一段更难事后分辨。
    /// 会打一条日志，免得在日志里也是无声消失。
    pub fn skip(&self) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.state != STATE_IDLE && s.elapsed > 0 {
            tracing::info!(
                "跳过 {} 阶段：已计时 {} 秒、键鼠 {} 次，按作废处理（未落库）",
                s.state,
                s.elapsed,
                s.key_count
            );
        }
        s.state = STATE_IDLE.to_string();
        s.paused = false;
        s.remaining = 0;
        s.elapsed = 0;
        s.key_count = 0;
    }

    pub fn stop(&self) {
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            save_current(&mut s);
            s.state = STATE_IDLE.to_string();
            s.paused = false;
            s.remaining = 0;
            s.elapsed = 0;
            s.key_count = 0;
        }
        tracing::info!("番茄钟已停止");
    }

    /// 按键回调：仅在工作中计数。
    pub fn record_key(&self, _key_name: &str) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.state == STATE_WORK && !s.paused {
            s.key_count += 1;
        }
    }

    pub fn shutdown(&self) {
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            save_current(&mut s);
            s.state = STATE_IDLE.to_string();
        }
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn state_code(s: &str) -> i64 {
    match s {
        STATE_WORK => 1,
        STATE_BREAK => 2,
        _ => 0,
    }
}

/// 由当前状态构造待落盘的阶段记录：纯内存、不做 I/O。
/// `None` 表示该状态无需保存（空闲或尚未计时）。
fn build_session(s: &TimerState) -> Option<Session> {
    if s.state == STATE_IDLE || s.planned <= 0 {
        return None;
    }
    let now = Local::now();
    let end_time = now.format("%Y-%m-%d %H:%M:%S").to_string();
    let actual = if s.elapsed > 0 {
        s.elapsed
    } else {
        // 刻意不 .max(1)：一秒都还没走过就是 0 秒。垫成 1 会让下面的
        // `actual_seconds >= 1` 判定通过，于是"点了开始立刻点停止"也算完成一个番茄。
        (s.planned - s.remaining).max(0)
    };
    let start_time = if s.start_time.is_empty() {
        now.format("%Y-%m-%d %H:%M:%S").to_string()
    } else {
        s.start_time.clone()
    };
    Some(Session {
        id: 0,
        rtype: s.state.clone(),
        start_time,
        end_time,
        planned_seconds: s.planned,
        actual_seconds: actual,
        key_count: s.key_count,
        created_at: now.format("%Y-%m-%d %H:%M:%S").to_string(),
    })
}

/// 保存当前阶段记录（须持有锁）。
///
/// 调用方都在主线程（stop/shutdown 由宿主 API 触发），本来就要等这一次
/// I/O，不会额外冻住别人；计时线程的阶段完成走的是另一条出锁再写的路径。
fn save_current(s: &mut TimerState) {
    let Some(session) = build_session(s) else {
        return;
    };
    // 零秒会话不入库也不计数：`today_summary` 是按 work 行数算番茄数的，
    // 留下一行 actual_seconds=0 的记录同样会被数成一个番茄。
    if session.actual_seconds <= 0 {
        return;
    }
    let counted = session.actual_seconds >= 1;
    if let Err(e) = save_session(&session) {
        tracing::error!("保存番茄钟记录失败: {e}");
    }
    if s.state == STATE_WORK && counted {
        s.work_finished += 1;
    }
}

/// 后台计时循环（每秒 tick）。
fn tick_loop(state: Arc<Mutex<TimerState>>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(1000));
        // 锁内只做纯内存的计时与状态推进；阶段完成时要写的记录攒到出锁后落盘。
        // 原先是持锁 INSERT：SQLite 的 busy_timeout 是 15 秒，一旦库被占住，
        // 主线程每次按键的 record_key（抢同一把锁）都会跟着卡住整个界面。
        let session = {
            let mut s = match state.lock() {
                Ok(g) => g,
                Err(_) => continue,
            };
            if s.state == STATE_IDLE || s.paused {
                continue;
            }
            s.remaining -= 1;
            s.elapsed += 1;
            if s.remaining > 0 {
                continue;
            }
            // 阶段完成
            let session = build_session(&s);
            if s.state == STATE_WORK && session.as_ref().is_some_and(|x| x.actual_seconds >= 1) {
                s.work_finished += 1;
            }
            if s.state == STATE_WORK && s.auto_break {
                s.state = STATE_BREAK.to_string();
                s.planned = s.break_minutes * 60;
                s.remaining = s.planned;
                s.elapsed = 0;
                s.key_count = 0;
                s.start_time = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
            } else {
                s.state = STATE_IDLE.to_string();
                s.paused = false;
                s.remaining = 0;
                s.elapsed = 0;
                s.key_count = 0;
            }
            session
        };
        if let Some(session) = session {
            if let Err(e) = save_session(&session) {
                tracing::error!("保存番茄钟记录失败: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 开始还不满一秒就停：不该留下一条"完成了一个番茄"的记录。
    ///
    /// `build_session` 原先把 0 秒垫成 `(planned - remaining).max(1)`，于是
    /// actual_seconds=1 → `save_current` 里 `>= 1` 的判定通过 → `work_finished` +1，
    /// `today_summary` 也把它算成一个番茄：手一抖点了开始又点停止，今天就多一个番茄。
    #[test]
    fn stopping_before_the_first_tick_records_nothing() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("pomo_zero");
        init_db().ok();

        let t = PomodoroTimer::new();
        t.start_work();
        t.stop();
        t.shutdown();

        assert_eq!(
            today_summary(),
            (0, 0, 0),
            "不到一秒的番茄不该被记成今天完成了一个"
        );
        assert!(get_recent_sessions(10).is_empty(), "不该落进历史记录");
        assert_eq!(t.get_state_info()["work_finished"], 0, "不该算完成一个番茄");
    }

    /// 一天最后一秒里开始的会话必须查得到。
    ///
    /// 上界原先写成 `start_time < '<date> 23:59:59'`，把 23:59:59 这一秒整个排除在外。
    #[test]
    fn last_second_of_the_day_is_included() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("pomo_edge");
        init_db().ok();
        let today = Local::now().format("%Y-%m-%d").to_string();
        save_session(&Session {
            id: 0,
            rtype: "work".into(),
            start_time: format!("{today} 23:59:59"),
            end_time: format!("{today} 23:59:59"),
            planned_seconds: 1500,
            actual_seconds: 1,
            key_count: 3,
            created_at: format!("{today} 23:59:59"),
        })
        .unwrap();

        let got = get_sessions_by_date(&today, 100);
        assert_eq!(got.len(), 1, "23:59:59 开始的会话被查询边界吃掉了");
    }
}
