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
            .map_err(|e| tracing::error!("启动番茄钟线程失败: {e}"))
            .ok();
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

    /// 用 `[pomodoro]` 三键覆盖时长与自动休息。
    ///
    /// 这三个键以前是**摆设**：`config.rs` 把它们写进每个人的 config.ini（镜像 Python 版），
    /// 而计时器只用 `TimerState::default()` 的 25/5/true，`set_durations` 与
    /// `set_auto_break` 全仓零调用方（宿主注册了 `pomodoro_set_durations`，
    /// 但没有任何内置 `.lua` 调它）。表现：他把 `work_minutes` 改成 45，
    /// 倒计时照旧 25:00，自动休息也关不掉。
    ///
    /// 只在计时器创建时读一次：改完配置要停用/再启用番茄钟插件（或重启）才生效。
    pub fn apply_config(&self, config: &crate::config::FocusFlowConfig) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.work_minutes = config
            .get_int("pomodoro", "work_minutes", s.work_minutes)
            .clamp(1, 180);
        s.break_minutes = config
            .get_int("pomodoro", "break_minutes", s.break_minutes)
            .clamp(1, 180);
        s.auto_break = config.get_bool("pomodoro", "auto_break", s.auto_break);
    }

    /// 供宿主 API `pomodoro_set_durations` 调用：与 [`apply_config`] 必须是同一套约束。
    ///
    /// 夹取范围与配置那条路一致（1..=180）。以前这里只有 `.max(1)`，
    /// 于是"config.ini 多写一位数不该变成 1666 小时的倒计时"那条理由
    /// 只对配置文件成立，从程序这条路照样能进去：release 不开 overflow-checks，
    /// `work_minutes * 60` 会绕成负的或荒唐的 `planned`，倒计时显示成
    /// "26358279…"或者一秒"完成"且什么都不落库，而 stop 之后每次 start_work 都重演。
    pub fn set_durations(&self, work_minutes: i64, break_minutes: i64) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.work_minutes = work_minutes.clamp(1, 180);
        s.break_minutes = break_minutes.clamp(1, 180);
    }

    pub fn set_auto_break(&self, enabled: bool) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .auto_break = enabled;
    }

    pub fn start_work(&self) {
        let (pending, started) = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if s.state == STATE_WORK {
                (None, false)
            } else {
                let p = take_current(&mut s);
                s.state = STATE_WORK.to_string();
                s.paused = false;
                s.planned = s.work_minutes * 60;
                s.remaining = s.planned;
                s.elapsed = 0;
                s.key_count = 0;
                s.start_time = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
                (p, true)
            }
        };
        if !started {
            return;
        }
        if let Some(session) = pending {
            persist_session(&session);
        }
        tracing::info!("番茄钟开始工作");
    }

    pub fn start_break(&self) {
        let (pending, started) = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if s.state == STATE_BREAK {
                (None, false)
            } else {
                let p = take_current(&mut s);
                s.state = STATE_BREAK.to_string();
                s.paused = false;
                s.planned = s.break_minutes * 60;
                s.remaining = s.planned;
                s.elapsed = 0;
                s.key_count = 0;
                s.start_time = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
                (p, true)
            }
        };
        if !started {
            return;
        }
        if let Some(session) = pending {
            persist_session(&session);
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
        let pending = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let p = take_current(&mut s);
            s.state = STATE_IDLE.to_string();
            s.paused = false;
            s.remaining = 0;
            s.elapsed = 0;
            s.key_count = 0;
            p
        };
        if let Some(session) = pending {
            persist_session(&session);
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
        let pending = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let p = take_current(&mut s);
            s.state = STATE_IDLE.to_string();
            p
        };
        self.stop.store(true, Ordering::SeqCst);
        if let Some(session) = pending {
            persist_session(&session);
        }
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

/// 在锁内取出待落盘的那一段并复位计数；**I/O 留给调用方在出锁之后做**。
/// `None` = 无需落库（空闲、还没计时、或计划时长无效）。
///
/// 为什么不在这里写库：`save_session` 走 `open_local()`，那是 `busy_timeout=15s` 的
/// SQLite 写操作。而 `record_key()` 每个按键都要抢同一把 `state` 锁（rdev 钩子线程
/// 同步调用），锁里等库 = 整个界面跟着卡住，Windows 还会在 LowLevelHooksTimeout 之后
/// 干脆不再回调钩子 —— 表现是"统计悄悄不涨了"。计时线程的阶段完成早就是这么改的
/// （见 `tick_loop`），这四条路径当时漏下了。
fn take_current(s: &mut TimerState) -> Option<Session> {
    let session = build_session(s)?;
    // 零秒会话不入库也不计数：`today_summary` 是按 work 行数算番茄数的，
    // 留下一行 actual_seconds=0 的记录同样会被数成一个番茄。
    if session.actual_seconds <= 0 {
        return None;
    }
    if s.state == STATE_WORK {
        s.work_finished += 1;
    }
    Some(session)
}

/// 出锁后落盘 `take_current` 交出来的那一段。
fn persist_session(session: &Session) {
    if let Err(e) = save_session(session) {
        tracing::error!("保存番茄钟记录失败: {e}");
    }
}

/// 一次 tick 之间过了多少**墙钟秒**。
///
/// 计时原来是每轮 `remaining -= 1`：一台笔记本合上盖睡 30 分钟，循环根本不跑，
/// 醒来后还要从"还剩 20 分钟"继续倒数 20 分钟 —— 休息晚半小时才开始；
/// 而落库那行的 `end_time` 用的是 `Local::now()`，于是同一条记录自相矛盾
/// （"时长 1500 秒"却跨了 45 分钟墙钟），今日活跃时长也少算同一截。
/// 改成按墙钟推进：睡眠期间真实过去的时间一次补齐。
/// 时钟被往回拨时取 0（不能把已经走过的进度吐回去）。
fn tick_delta_seconds(prev: &chrono::DateTime<Local>, now: &chrono::DateTime<Local>) -> i64 {
    now.signed_duration_since(*prev).num_seconds().max(0)
}

/// 后台计时循环（每秒 tick）。
fn tick_loop(state: Arc<Mutex<TimerState>>, stop: Arc<AtomicBool>) {
    let mut last_tick = Local::now();
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(1000));
        let now_wall = Local::now();
        // 每轮都推进锚点：空闲/暂停的这些轮也必须重置，否则 resume 之后
        // 第一次 tick 会把"暂停期间过去的时间"整个算进这一段
        let delta = tick_delta_seconds(&last_tick, &now_wall);
        last_tick = now_wall;
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
            s.remaining -= delta;
            s.elapsed += delta;
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
    /// actual_seconds=1 → 落库计数那一步的 `>= 1` 判定通过 → `work_finished` +1，
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

#[cfg(test)]
mod busy_lock_tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// 库被占住时，`stop()` 等 SQLite 没错，但不该把 `state` 锁一起拖着等。
    ///
    /// `record_key()` 每个按键都要抢这把锁（rdev 钩子线程同步调用），而原先
    /// start_work / start_break / stop / shutdown 四条路径在锁里做 I/O，
    /// `open_local()` 的 busy_timeout 是 15 秒：库一被占住（WAL checkpoint、备份、
    /// 杀软握着 -wal），整个界面跟着卡住，Windows 过了 LowLevelHooksTimeout 还会
    /// 干脆不再回调钩子 —— 表现是"统计悄悄不涨了"。`tick_loop` 早就改成出锁再写了，
    /// 这四条是漏下的那几条。
    #[test]
    fn stop_does_not_hold_the_state_lock_while_the_db_is_busy() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("pomo_busy_lock");
        init_db().expect("建库失败");
        let timer = PomodoroTimer::new();
        timer.start_work();
        // 计时要真的走过一秒，stop() 才有东西要落库（零秒段刻意不写）
        std::thread::sleep(Duration::from_millis(1200));

        // 另一个连接占住写锁：WAL 下一个 IMMEDIATE 事务就足以挡住 INSERT
        let hold = open_local().expect("占位连接失败");
        hold.execute_batch("BEGIN IMMEDIATE").expect("占锁失败");

        let t = Arc::clone(&timer);
        let stopper = std::thread::spawn(move || t.stop());
        std::thread::sleep(Duration::from_millis(80));
        let mut lock_free = false;
        for _ in 0..25 {
            if timer.state.try_lock().is_ok() {
                lock_free = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        // 放掉写锁，让 stop() 里那次写落完
        let _ = hold.execute_batch("ROLLBACK");
        drop(hold);
        let waited = Instant::now();
        stopper.join().expect("stop 线程 panicked");
        timer.shutdown();
        assert!(
            lock_free,
            "库被占住期间 state 锁必须已经放开（等到 {:?} 才脱身）",
            waited.elapsed()
        );
        // 反向腿：这一段确实落了库，否则上面的"不卡"只是因为什么都没写
        assert_eq!(get_recent_sessions(10).len(), 1);
    }

    /// `[pomodoro]` 三键必须真的落到计时器上。
    ///
    /// 以前是摆设：`config.rs` 把 `work_minutes/break_minutes/auto_break` 写进每个人的
    /// config.ini，计时器却只用 `TimerState::default()`，而 `set_durations` /
    /// `set_auto_break` 全仓零调用方 —— 改成 45 分钟仍然倒数 25:00。
    #[test]
    fn config_durations_reach_the_timer() {
        let _lock = crate::paths::test_app_dir_lock();
        let dir = crate::paths::test_app_dir("pomodoro_cfg");
        let path = dir.path().join("config.ini");
        std::fs::write(
            &path,
            "[pomodoro]\nwork_minutes = 45\nbreak_minutes = 10\nauto_break = false\n",
        )
        .unwrap();
        let cfg = crate::config::FocusFlowConfig::load(&path).unwrap();
        let t = PomodoroTimer::new();
        t.apply_config(&cfg);
        let s = t.get_state_info();
        assert_eq!(s["work_minutes"], 45, "手改的工作时长必须生效");
        assert_eq!(s["break_minutes"], 10);
        assert_eq!(s["auto_break"], 0, "自动休息必须能关掉");

        // 越界要夹住：config.ini 是手改的，多写一位数不该变成 1666 小时的倒计时
        std::fs::write(&path, "[pomodoro]\nwork_minutes = 99999\n").unwrap();
        let cfg2 = crate::config::FocusFlowConfig::load(&path).unwrap();
        t.apply_config(&cfg2);
        assert_eq!(t.get_state_info()["work_minutes"], 180, "越界必须被夹住");

        // 反向腿：键没了要回到默认，而不是停在上一次的值
        std::fs::write(&path, "[pomodoro]\n").unwrap();
        let cfg3 = crate::config::FocusFlowConfig::load(&path).unwrap();
        t.apply_config(&cfg3);
        assert_eq!(t.get_state_info()["work_minutes"], 25);
    }

    /// `pomodoro_set_durations`（宿主 API，程序这条路）必须和配置那条路一样夹住。
    ///
    /// 以前只有 `.max(1)`：一个天文数字进来后 `work_minutes * 60` 在 release
    /// （不开 overflow-checks）里绕成负的/荒唐的 `planned`，倒计时就成了
    /// "26358279…"或一秒"完成"，而且 stop 后每次 start_work 都重演。
    #[test]
    fn set_durations_clamps_like_apply_config() {
        let _lock = crate::paths::test_app_dir_lock();
        // start_work 会碰番茄钟自己的库，必须隔离到临时目录
        let _dir = crate::paths::test_app_dir("pomodoro_clamp");
        let t = PomodoroTimer::new();
        t.set_durations(i64::from(i32::MAX) * 1_000, 5);
        let s = t.get_state_info();
        assert_eq!(s["work_minutes"], 180, "程序入口也要夹 1..=180");
        t.start_work();
        let s = t.get_state_info();
        assert_eq!(s["planned"], 180 * 60, "planned 不能绕成负数或荒唐值");
        assert!(s["remaining"] > 0, "倒计时不该一上来就是 0 或负数");

        // 反向腿：0 与负数也不能把计时器变成"立刻完成"
        t.set_durations(0, -30);
        let s = t.get_state_info();
        assert_eq!(s["work_minutes"], 1);
        assert_eq!(s["break_minutes"], 1);
    }

    /// 计时必须按**墙钟**推进，不是"每轮 tick 减一秒"。
    ///
    /// 老实现合上盖睡 30 分钟回来，还要从"剩 20 分钟"再倒数 20 分钟，
    /// 而落库的 end_time 用的是真实时刻 —— 同一行记录自相矛盾。
    #[test]
    fn tick_advances_by_wall_clock_not_by_iterations() {
        use chrono::TimeZone;
        let a = Local
            .with_ymd_and_hms(2026, 9, 23, 10, 0, 0)
            .single()
            .expect("合法本地时刻");
        let b = Local
            .with_ymd_and_hms(2026, 9, 23, 10, 30, 0)
            .single()
            .expect("合法本地时刻");
        assert_eq!(tick_delta_seconds(&a, &b), 1800, "睡 30 分钟要一次补齐");
        assert_eq!(tick_delta_seconds(&b, &a), 0, "时钟往回拨不能把进度吐回去");
        assert_eq!(tick_delta_seconds(&a, &a), 0);
    }
}
