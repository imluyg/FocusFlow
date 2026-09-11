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
    let result = stmt.query_map(
        rusqlite::params![
            format!("{date_str} 00:00:00"),
            format!("{date_str} 23:59:59"),
            limit
        ],
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

    pub fn skip(&self) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
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

/// 保存当前阶段记录（须持有锁）。
fn save_current(s: &mut TimerState) {
    if s.state == STATE_IDLE || s.planned <= 0 {
        return;
    }
    let now = Local::now();
    let end_time = now.format("%Y-%m-%d %H:%M:%S").to_string();
    let actual = if s.elapsed > 0 {
        s.elapsed
    } else {
        (s.planned - s.remaining).max(1)
    };
    let start_time = if s.start_time.is_empty() {
        now.format("%Y-%m-%d %H:%M:%S").to_string()
    } else {
        s.start_time.clone()
    };
    let session = Session {
        id: 0,
        rtype: s.state.clone(),
        start_time: start_time.clone(),
        end_time: end_time.clone(),
        planned_seconds: s.planned,
        actual_seconds: actual,
        key_count: s.key_count,
        created_at: now.format("%Y-%m-%d %H:%M:%S").to_string(),
    };
    if let Err(e) = save_session(&session) {
        tracing::error!("保存番茄钟记录失败: {e}");
    }
    if s.state == STATE_WORK && actual >= 1 {
        s.work_finished += 1;
    }
}

/// 后台计时循环（每秒 tick）。
fn tick_loop(state: Arc<Mutex<TimerState>>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(1000));
        let mut s = match state.lock() {
            Ok(g) => g,
            Err(_) => continue,
        };
        if s.state == STATE_IDLE || s.paused {
            continue;
        }
        s.remaining -= 1;
        s.elapsed += 1;
        if s.remaining <= 0 {
            // 阶段完成
            save_current(&mut s);
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
        }
    }
}
