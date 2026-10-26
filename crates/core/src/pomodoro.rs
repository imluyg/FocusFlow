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

/// 一次「停止（记为完成）」的结局。面板要照它说话，不能替用户猜。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stopped {
    /// 没在计时，或这一段一秒都没计到：什么都没落库
    Nothing,
    /// 落库成功（阶段类型与实际秒数）
    Recorded { rtype: String, seconds: i64 },
    /// 有这一段但写库没成（库被占用 / 打不开）：没落库，也不算完成
    SaveFailed { rtype: String, seconds: i64 },
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

/// 读会话时那一条列表里必须**按名字**问的列，顺序就是 `row.get(0..7)` 的顺序。
///
/// 原来两条查询都写 `SELECT *` 再按下标取：列的先后由**建表时的顺序**决定，
/// 而这个库可能是 Python 时代留下的那张（列名与顺序我们无法保证）。少一列或多一列时
/// 下标会整体错位 —— 最省事的观察是"把 key_count 放到最后一列"，于是 planned 与 actual
/// 被读成别的数字，面板上的时长/键数安静地变成别的字段；那一列类型对不上时更干脆：
/// 每一行都转换失败，`flatten()` 把它们全丢掉 ⇒ "今天没做过番茄"。
const SESSION_COLS: &str =
    "id, type, start_time, end_time, planned_seconds, actual_seconds, key_count, created_at";

/// 把一批行读成会话；单行读不出来时丢掉**那一行**并数下来，不牵连别的行。
///
/// 交回 `(sessions, 被丢掉几行, 第一个失败原因)`：调用方负责把后两样说出来 ——
/// 原先这里是一条 `rows.flatten().collect()`，一行读不动就少一行，而今日汇总与列表
/// 都看不出差别。
fn collect_sessions<R>(rows: R) -> (Vec<Session>, usize, Option<String>)
where
    R: Iterator<Item = rusqlite::Result<Session>>,
{
    let mut sessions = Vec::new();
    let mut dropped = 0usize;
    let mut first_err: Option<String> = None;
    for row in rows {
        match row {
            Ok(s) => sessions.push(s),
            Err(e) => {
                dropped += 1;
                if first_err.is_none() {
                    first_err = Some(e.to_string());
                }
            }
        }
    }
    (sessions, dropped, first_err)
}

/// 按日期查询会话。
pub fn get_sessions_by_date(date_str: &str, limit: i64) -> Vec<Session> {
    let conn = match open_local() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("番茄钟查询（{date_str}）失败：库打不开: {e}");
            return Vec::new();
        }
    };
    let mut stmt = match conn.prepare(&format!(
        "SELECT {SESSION_COLS} FROM pomodoro_sessions WHERE start_time >= ?1 AND start_time < ?2 ORDER BY id DESC LIMIT ?3",
    )) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("番茄钟查询（{date_str}）失败：语句准备不了: {e}");
            return Vec::new();
        }
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
        row_to_session,
    );
    match result {
        Ok(rows) => {
            let (sessions, dropped, why) = collect_sessions(rows);
            report_dropped(date_str, dropped, why.as_deref());
            sessions
        }
        Err(e) => {
            tracing::error!("番茄钟查询（{date_str}）失败：这一天的行一条都没读出来: {e}");
            Vec::new()
        }
    }
}

/// 把一行（按下标，顺序即 [`SESSION_COLS`]）读成会话。
fn row_to_session(r: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
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
}

/// 有几行没读出来时要点名说：不说就等于"那一天少几行"是正常结果。
fn report_dropped(date_str: &str, dropped: usize, first_err: Option<&str>) {
    if dropped > 0 {
        tracing::warn!(
            "番茄钟查询（{date_str}）有 {dropped} 行读不出来（第一个原因：{}），\
             它们不会出现在今日汇总与列表里",
            first_err.unwrap_or("(没带上原因)")
        );
    }
}

/// 查询最近会话。
pub fn get_recent_sessions(limit: i64) -> Vec<Session> {
    let conn = match open_local() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("番茄钟最近会话查询失败：库打不开: {e}");
            return Vec::new();
        }
    };
    let mut stmt = match conn.prepare(&format!(
        "SELECT {SESSION_COLS} FROM pomodoro_sessions ORDER BY id DESC LIMIT ?1",
    )) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("番茄钟最近会话查询失败：语句准备不了: {e}");
            return Vec::new();
        }
    };
    let result = stmt.query_map([limit], row_to_session);
    match result {
        Ok(rows) => {
            let (sessions, dropped, why) = collect_sessions(rows);
            report_dropped("最近", dropped, why.as_deref());
            sessions
        }
        Err(e) => {
            tracing::error!("番茄钟最近会话查询失败：一条都没读出来: {e}");
            Vec::new()
        }
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
    /// 计时线程是否在跑。spawn 失败时保持 false —— 没有这条线程，倒计时永远不会动，
    /// 所以 `start_work`/`start_break` 必须拒绝，而不是让界面显示"计时中"、数字却冻住。
    tick_alive: Arc<AtomicBool>,
    /// 串行化 spawn，防两个并发调用各起一条计时线程。
    spawn_lock: Mutex<()>,
}

/// 计时线程退出时清存活标志（正常退出或 panic 都要清），下次取用时才会重试 spawn。
struct TickAliveGuard(Arc<AtomicBool>);

impl Drop for TickAliveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
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
            tick_alive: Arc::new(AtomicBool::new(false)),
            spawn_lock: Mutex::new(()),
        });
        if let Err(e) = timer.ensure_tick_thread() {
            // 不把实例作废：每次取用（`pomodoro_timer`）和每次点开始都会重试 spawn。
            // 但开始阶段必须能失败，不能再出现"界面说已经开始、倒计时却冻住"。
            tracing::error!("启动番茄钟线程失败，开始/休息会被拒绝；下次调用会再试: {e}");
        }
        timer
    }

    /// 起后台计时线程（幂等）；失败往上传。
    pub(crate) fn ensure_tick_thread(&self) -> Result<(), String> {
        let _spawn = self.spawn_lock.lock().unwrap_or_else(|e| e.into_inner());
        if self.stop.load(Ordering::SeqCst) {
            return Err("番茄钟计时线程已关闭".to_string());
        }
        if self.tick_alive.load(Ordering::SeqCst) {
            return Ok(());
        }
        let stop = Arc::clone(&self.stop);
        let state = Arc::clone(&self.state);
        let alive = Arc::clone(&self.tick_alive);
        // 先置位再 spawn：新线程若立刻 panic，守卫会把标志清回去；spawn 失败则在这里清。
        self.tick_alive.store(true, Ordering::SeqCst);
        let spawned = std::thread::Builder::new()
            .name("pomodoro".into())
            .spawn(move || {
                let _alive = TickAliveGuard(alive);
                tick_loop(state, stop);
            });
        match spawned {
            Ok(_) => Ok(()),
            Err(e) => {
                self.tick_alive.store(false, Ordering::SeqCst);
                Err(format!("启动番茄钟线程失败: {e}"))
            }
        }
    }

    /// 开始工作/休息前的同一道闸：计时线程不在跑就拒绝，绝不把状态改成"计时中"。
    fn require_tick_thread(&self) -> Result<(), String> {
        if self.stop.load(Ordering::SeqCst) {
            return Err("番茄钟计时线程已关闭".to_string());
        }
        if !self.tick_alive.load(Ordering::SeqCst) {
            return Err("番茄钟计时线程没有在运行，倒计时无法开始；请重启程序后再试".to_string());
        }
        Ok(())
    }

    /// 测试用：复现 spawn 失败后的状态（实例在、计时线程不在）。
    #[cfg(test)]
    fn without_tick_thread() -> Arc<Self> {
        Arc::new(Self {
            state: Arc::new(Mutex::new(TimerState::default())),
            stop: Arc::new(AtomicBool::new(false)),
            tick_alive: Arc::new(AtomicBool::new(false)),
            spawn_lock: Mutex::new(()),
        })
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
    /// 而计时器只用 `TimerState::default()` 的 25/5/true。表现：他把 `work_minutes` 改成 45，
    /// 倒计时照旧 25:00，自动休息也关不掉。
    ///
    /// 现在这两个数的口径已经不一样了，别说成一回事：
    /// `set_durations` 有调用方了（内置插件面板上的「应用时长」按钮，
    /// `plugins/pomodoro_plugin.lua` 走宿主的 `pomodoro_set_durations`）；
    /// `set_auto_break` 到写下这一行时仍然**零调用方**，宿主也没给它注册对应 API
    /// ⇒ 自动休息只能靠 `[pomodoro] auto_break` 这一条道，面板上换不掉它。
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

    /// 供宿主 API `pomodoro_set_durations` 调用：与 [`PomodoroTimer::apply_config`] 必须是同一套约束。
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

    pub fn start_work(&self) -> Result<(), String> {
        self.require_tick_thread()?;
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
            return Ok(());
        }
        if let Some(session) = pending {
            persist_session(&self.state, &session);
        }
        tracing::info!("番茄钟开始工作");
        Ok(())
    }

    pub fn start_break(&self) -> Result<(), String> {
        self.require_tick_thread()?;
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
            return Ok(());
        }
        if let Some(session) = pending {
            persist_session(&self.state, &session);
        }
        tracing::info!("番茄钟开始休息");
        Ok(())
    }

    pub fn toggle_pause(&self) -> bool {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.state == STATE_IDLE {
            return false;
        }
        s.paused = !s.paused;
        s.paused
    }

    /// 跳过当前阶段：**这段作废**，与 [`PomodoroTimer::stop`] 唯一的差别就是不落库。
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

    /// 停止当前阶段：已经计到的时间照常落库，但**只有真的落成了才算这一段**。
    ///
    /// 返回的结局是替面板说的那句话：「这一段按完成落库，会算进今日完成」原先是无条件
    /// 讲的，而它在三种情况下不成立 —— 没在计时、这一段一秒都没计到（`take_current`
    /// 直接丢掉）、写库没成。休息那半段还会落库但不计入「今日完成」（`today_summary`
    /// 只数 work 行），所以类型也要一起交出去。
    pub fn stop(&self) -> Stopped {
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
        match pending {
            None => {
                tracing::info!("番茄钟已停止（这一段没有可落库的内容）");
                Stopped::Nothing
            }
            Some(session) => {
                let rtype = session.rtype.clone();
                let seconds = session.actual_seconds;
                let saved = persist_session(&self.state, &session);
                tracing::info!("番茄钟已停止");
                if saved {
                    Stopped::Recorded { rtype, seconds }
                } else {
                    Stopped::SaveFailed { rtype, seconds }
                }
            }
        }
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
            persist_session(&self.state, &session);
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
    Some(session)
}

/// 出锁后落盘 `take_current` 交出来的那一段，**真的落进库才计一个番茄**，
/// 并把结局交给调用方（面板要照它说话，见 [`PomodoroTimer::stop`]）。
///
/// 原来 `take_current` 在锁内就先 `work_finished += 1`，写库失败只留一句
/// `tracing::error!`：内存里的计数与库里的行数从此永久分叉 —— 卡片上"今日完成 4 个"
/// 而库里只有 3 行，重启后按库行数读数就无声缩水。改成两边同轨：库里有这一行才算数。
fn persist_session(state: &Mutex<TimerState>, session: &Session) -> bool {
    if let Err(e) = save_session(session) {
        tracing::error!("保存番茄钟记录失败: {e}（本次不计入已完成番茄数）");
        return false;
    }
    if session.rtype == STATE_WORK && session.actual_seconds >= 1 {
        state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .work_finished += 1;
    }
    true
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

/// 按墙钟推进本段计时，返回 `(新的 remaining, 新的 elapsed)`。
///
/// 负方向由 `tick_delta_seconds` 夹过（回拨不把进度吐回去），这里夹**正方向**：
/// 墙钟也会凭空向前跳（NTP 校正、手工改时间、休眠后 RTC 与系统时钟对齐）。
/// 不夹回本段的剩余值，一次 +1 小时的跳变就把 25 分钟的番茄记成 4800 秒 ——
/// `build_session` 无条件取 `elapsed`，于是 `work_finished` 加一、今日专注时长
/// 跟着虚高，而这一行已经落库，事后没法修正。合盖睡过整段的补齐语义保持不变：
/// 最多推进到本段结束，多出来的那截丢掉。
fn advance_stage_seconds(remaining: i64, elapsed: i64, delta: i64) -> (i64, i64) {
    let step = delta.clamp(0, remaining.max(0));
    (remaining - step, elapsed + step)
}

/// 推进一轮计时：锁内只做纯内存的计时与状态推进，返回**出锁后**要落盘的那一段。
///
/// `delta` 与 `now_wall` 由调用方传进来而不是在这里读时钟：想钉"墙钟跳变""锁中毒后
/// 还在不在走"这类场景，靠线程里真 sleep 是钉不出来的。
///
/// 一轮最多跨过**一个**阶段：`delta` 超出本段剩余值的部分整段丢弃（见
/// [`advance_stage_seconds`]）。合盖睡两小时醒来只补一次切换，中间那几段既不计完成
/// 也不落记录 —— 这是刻意保留的：继续往下跨就会把睡着的时间逐段结算成番茄与专注时长。
fn tick_once(
    state: &Mutex<TimerState>,
    delta: i64,
    now_wall: chrono::DateTime<Local>,
) -> Option<Session> {
    // 取锁一律 `unwrap_or_else(into_inner)`。这里原先是全文件唯一的例外
    // （`match state.lock() { Err(_) => continue }`）：std 的中毒是**永久**的，
    // 一旦有人在持锁时 panic，计时线程就从此每轮 continue —— 倒计时卡住、永不落库、
    // 零日志，而其余取锁处照常工作，界面上只剩"番茄钟不动了"这一个现象。
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    if s.state == STATE_IDLE || s.paused {
        return None;
    }
    let (remaining, elapsed) = advance_stage_seconds(s.remaining, s.elapsed, delta);
    s.remaining = remaining;
    s.elapsed = elapsed;
    if s.remaining > 0 {
        return None;
    }
    // 阶段完成。番茄计数不在这里加：落库成功才加（见 `persist_session`）。
    let session = build_session(&s);
    if s.state == STATE_WORK && s.auto_break {
        s.state = STATE_BREAK.to_string();
        s.planned = s.break_minutes * 60;
        s.remaining = s.planned;
        s.elapsed = 0;
        s.key_count = 0;
        s.start_time = now_wall.format("%Y-%m-%d %H:%M:%S").to_string();
    } else {
        s.state = STATE_IDLE.to_string();
        s.paused = false;
        s.remaining = 0;
        s.elapsed = 0;
        s.key_count = 0;
    }
    session
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
        // 落盘排在出锁之后：原先是持锁 INSERT，而 SQLite 的 busy_timeout 是 15 秒，
        // 一旦库被占住，主线程每次按键的 record_key（抢同一把锁）都会跟着卡住整个界面。
        if let Some(session) = tick_once(&state, delta, now_wall) {
            persist_session(&state, &session);
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
        t.start_work().expect("计时线程应可用");
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

    /// 计时线程没起来时，开始工作/休息必须报错，不能把状态改成"计时中"。
    ///
    /// `new()` 原先用 `.map_err(日志).ok()` 吞掉 spawn 失败，实例照样被缓存：
    /// 点开始后状态变成 work、倒计时却永远停在 25:00，而 `record_key` 照常累加；
    /// 用户停止时 `elapsed` 还是 0，`take_current` 直接丢掉这一段 —— 一场空，且零提示。
    #[test]
    fn starting_without_a_tick_thread_is_refused_not_faked() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("pomo_no_tick");
        init_db().ok();

        let t = PomodoroTimer::without_tick_thread();
        let err = t.start_work().expect_err("没有计时线程必须拒绝开始工作");
        assert!(
            err.contains("计时线程"),
            "错误要说清是计时线程的问题: {err}"
        );
        assert_eq!(t.get_state(), STATE_IDLE, "拒绝之后不许停在 work 状态");
        assert_eq!(
            t.get_state_info()["remaining"],
            0,
            "不许留下一个冻住的倒计时"
        );

        let err = t.start_break().expect_err("没有计时线程必须拒绝开始休息");
        assert!(err.contains("计时线程"));
        assert_eq!(t.get_state(), STATE_IDLE);

        // 反向腿：同一条闸不能把活着的计时器也拒掉（上面的用例都依赖它放行）。
        let live = PomodoroTimer::new();
        live.start_work().expect("线程活着时必须允许开始");
        assert_eq!(live.get_state(), STATE_WORK);
        live.shutdown();
    }

    /// 锁中毒之后计时必须继续按秒推进。
    ///
    /// `tick_once` 原先是全文件唯一写成 `match state.lock() { Err(_) => continue }` 的
    /// 取锁处，而 std 的中毒是**永久**的：一次持锁 panic 之后计时线程从此每轮空转 ——
    /// 倒计时卡住、永不落库、零日志，其余取锁处照常工作，界面上只剩"番茄钟不动了"。
    #[test]
    fn tick_advances_after_the_state_lock_is_poisoned() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("pomo_poison");
        init_db().ok();

        let state = Mutex::new(TimerState {
            state: STATE_WORK.to_string(),
            planned: 30 * 60,
            remaining: 30 * 60,
            ..Default::default()
        });
        // 夹具：持锁时 panic，把 Mutex 打成中毒（之后每次 lock() 都返回 Err）
        let panicked = std::panic::catch_unwind(|| {
            let _g = state.lock().unwrap();
            panic!("用例制造的中锁 panic");
        });
        assert!(
            panicked.is_err() && state.is_poisoned(),
            "夹具没造出中毒状态，后面的断言就没有意义"
        );

        for _ in 1..=3 {
            tick_once(&state, 1, Local::now());
        }
        let remaining = state.lock().unwrap_or_else(|e| e.into_inner()).remaining;
        assert_eq!(
            remaining,
            30 * 60 - 3,
            "锁中毒后倒计时仍要按秒推进（旧写法会永久停在 1800）"
        );
    }

    /// 落库失败的那一段不算「今日完成」。
    ///
    /// 原来 `take_current` 在锁内先 `work_finished += 1`，再由调用方写库，写库失败只留
    /// 一句 `tracing::error!`：内存计数与库里的行数从此永久分叉 —— 卡片上 4 个、库里
    /// 3 行，重启后按库行数读数就无声缩水。现在两边同轨：库里有这一行才算一个。
    #[test]
    fn save_failure_does_not_credit_a_pomodoro() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("pomo_savefail");
        init_db().ok();
        // 把会话表打掉：open_local 仍建得起库，INSERT 必失败 —— 这就是"写库失败"的形状
        {
            let conn = open_local().unwrap();
            conn.execute("DROP TABLE pomodoro_sessions", []).unwrap();
        }

        let t = PomodoroTimer::new();
        t.start_work().expect("计时线程应可用");
        // 真的推进 5 秒：零秒会话在 take_current 里就被拦掉，绕不开要验的这条路径
        tick_once(&t.state, 5, Local::now());
        t.stop();
        t.shutdown();

        assert_eq!(
            t.get_state_info()["work_finished"],
            0,
            "没落进库的一段不该算完成一个番茄"
        );
    }

    /// 反面对照：保证上一条不是"根本没走到落库"造成的假红 —— 表在、落库成功时，
    /// 同一条路径必须正好计出一个番茄、库里正好一行。
    #[test]
    fn saved_session_credits_one_pomodoro() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("pomo_saveok");
        init_db().ok();

        let t = PomodoroTimer::new();
        t.start_work().expect("计时线程应可用");
        tick_once(&t.state, 5, Local::now());
        t.stop();
        t.shutdown();

        assert_eq!(
            t.get_state_info()["work_finished"],
            1,
            "落库成功要计一个番茄"
        );
        assert_eq!(get_recent_sessions(10).len(), 1, "库里要留下那一行");
    }

    /// 读会话必须按**列名**问，不能拿 `SELECT *` 的下标去猜。
    ///
    /// 两条查询原先都写 `SELECT *` 再 `row.get(0..7)`：下标对的是**建表时的列序**，
    /// 而这个库可能是 Python 时代那张（那一版把 key_count 放在 actual_seconds 之前完全
    /// 说得通）。列序一不同，"计划时长/实际时长/按键数"三个数就整体错位 —— 面板照样
    /// 出数字，只是每个都是别的字段；如果那张表少一列，则是每一行都转换失败，
    /// `flatten()` 把它们全丢掉，看上去"今天没做过番茄"。
    #[test]
    fn sessions_are_read_by_name_not_by_column_position() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("pomo_legacy_order");
        let today = Local::now().format("%Y-%m-%d").to_string();
        {
            let conn = open_local().unwrap();
            // 旧形态：列名一样、顺序不同（key_count 夹在 planned 之前）
            conn.execute_batch(
                "CREATE TABLE pomodoro_sessions (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    type TEXT NOT NULL,
                    start_time TEXT NOT NULL,
                    end_time TEXT NOT NULL,
                    key_count INTEGER NOT NULL DEFAULT 0,
                    planned_seconds INTEGER NOT NULL,
                    actual_seconds INTEGER NOT NULL,
                    created_at TEXT NOT NULL
                 );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO pomodoro_sessions
                 (type, start_time, end_time, key_count, planned_seconds, actual_seconds, created_at)
                 VALUES ('work', ?1, ?2, 42, 1500, 900, ?2)",
                rusqlite::params![format!("{today} 09:00:00"), format!("{today} 09:25:00")],
            )
            .unwrap();
        }

        for got in [get_sessions_by_date(&today, 100), get_recent_sessions(10)] {
            let s = got
                .first()
                .expect("旧形态那张表的行必须读得出来，不能变成「今天没做过番茄」");
            let mut wrong: Vec<String> = Vec::new();
            if s.rtype != "work" {
                wrong.push(format!("type 读成 {:?}", s.rtype));
            }
            if s.planned_seconds != 1500 {
                wrong.push(format!("planned_seconds 读成 {}", s.planned_seconds));
            }
            if s.actual_seconds != 900 {
                wrong.push(format!("actual_seconds 读成 {}", s.actual_seconds));
            }
            if s.key_count != 42 {
                wrong.push(format!("key_count 读成 {}", s.key_count));
            }
            assert!(wrong.is_empty(), "按位置猜列，实得: {}", wrong.join("、"));
        }

        // 今日汇总吃的是同一条读路：错位的三个数会直接变成卡片上的键数与时长
        let (count, keys, secs) = today_summary();
        assert!(
            count == 1 && keys == 42 && secs == 900,
            "今日汇总要按字段本来的意思算，实得 count={count} keys={keys} secs={secs}"
        );
    }

    /// 接线守卫：两条读路都得走那份列清单，且不许再把行"折叠"掉。
    ///
    /// 上面那条用例钉的是读出来的值；它盯不到有人把 SQL 改回 `SELECT *`（只要列序
    /// 恰好对上，用例照样绿），也盯不到"少一行没人说"（少的那一行本来就是被 `flatten()`
    /// 静默吞掉的形状）。所以这两半各钉一条字面判据。
    #[test]
    fn the_session_reads_name_their_columns_and_count_their_losses() {
        let src = include_str!("pomodoro.rs");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod = src[..cut]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut wrong: Vec<String> = Vec::new();
        if prod.contains("SELECT * FROM pomodoro_sessions") {
            wrong.push("又有查询用 SELECT * 猜列的位置".to_string());
        }
        let n = prod.matches("SELECT {SESSION_COLS}").count();
        if n != 2 {
            wrong.push(format!("两条读路必须都用那份列清单，实得 {n} 处"));
        }
        if prod.contains("rows.flatten()") {
            wrong.push("行又被 flatten() 静默吞掉了（少几行没人知道）".to_string());
        }
        for (needle, what) in [
            (
                "report_dropped(date_str, dropped, why.as_deref())",
                "按日期那条读路",
            ),
            (
                "report_dropped(\"最近\", dropped, why.as_deref())",
                "最近会话那条读路",
            ),
        ] {
            if !prod.contains(needle) {
                wrong.push(format!(
                    "{what}没把「少读了几行」说出来（needle 对不上＝被改写了）"
                ));
            }
        }
        assert!(wrong.is_empty(), "读路守卫红:\n{}", wrong.join("\n"));
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
        timer.start_work().expect("计时线程应可用");
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
        t.start_work().expect("计时线程应可用");
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

    /// 墙钟**向前**凭空跳，不能给本段造出没走过的时间。
    ///
    /// 上面那条钉住了回拨方向，正方向只考虑了"合盖睡过去"这一种，可同一个 delta 也
    /// 来自 NTP 校正、手工改时间、休眠后 RTC 对齐。不夹回本段剩余值，`build_session`
    /// 会无条件把 `elapsed` 写进落库行（4800 秒的"25 分钟番茄"），`work_finished`
    /// 跟着 +1，今日专注时长一起虚高，而且那行已经落库、事后无法修正。
    #[test]
    fn forward_clock_jump_cannot_inflate_a_stage_beyond_its_remaining() {
        // 正常一秒 tick：本段内怎么推进都不夹
        assert_eq!(advance_stage_seconds(900, 600, 1), (899, 601));
        // 合盖 30 分钟睡过整段：补齐到本段结束为止（1500 秒的番茄就是 1500）
        assert_eq!(advance_stage_seconds(900, 600, 1800), (0, 1500));
        assert_eq!(advance_stage_seconds(900, 600, 900), (0, 1500), "恰好走完");
        // 已经走完的段一步都不能再推进（remaining=0 时 elapsed 不再动）
        assert_eq!(advance_stage_seconds(0, 1500, 1800), (0, 1500));
        // 兜住符号：负 delta 不该把进度吐回去、也不该让 elapsed 倒退
        assert_eq!(advance_stage_seconds(900, 600, -5), (900, 600));
        // 计划外的畸形 remaining（历史状态里可能是负数）不能推进度
        assert_eq!(advance_stage_seconds(-30, 1500, 5), (-30, 1500));
    }

    /// 「停止」交出的结局要能撑起面板那句话：空闲时什么都没有落库。
    /// 面板原先无条件说「这一段按完成落库，会算进今日完成」，开着面板直接点停止
    /// 就是当着用户的面说一句没发生过的账。
    #[test]
    fn stopping_an_idle_timer_records_nothing() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("pomo_stop_idle");
        init_db().ok();

        let t = PomodoroTimer::new();
        assert_eq!(t.stop(), Stopped::Nothing, "没在计时时按停止不该有落库的段");
        t.shutdown();
    }

    /// 正向对照：真落了库的一段工作必须交出类型与秒数 —— 只有这种结局下
    /// 「按完成落库、算进今日完成」那句才成立。
    #[test]
    fn a_landed_work_session_is_reported_as_work() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("pomo_stop_work");
        init_db().ok();

        let t = PomodoroTimer::new();
        t.start_work().expect("计时线程应可用");
        tick_once(&t.state, 5, Local::now());
        let out = t.stop();
        t.shutdown();

        assert!(
            matches!(out, Stopped::Recorded { ref rtype, seconds }
                     if rtype == STATE_WORK && seconds >= 1),
            "落了库的一段工作要交出类型与秒数，实际 {out:?}"
        );
        assert_eq!(
            today_summary().0,
            1,
            "夹具前提：这一段确实落进了库（面板那句「算进今日完成」才有依据）"
        );
    }

    /// 写库没成时不许说「按完成落库」：夹具把番茄库的路径做成同名目录
    /// （本仓既有的"这截写不进去"形状）。
    #[test]
    fn an_unwritable_session_is_reported_as_save_failed() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("pomo_stop_fail");
        init_db().ok();
        std::fs::remove_file(db_path()).ok();
        std::fs::create_dir_all(db_path()).expect("夹具：把番茄库路径做成目录");

        let t = PomodoroTimer::new();
        t.start_work().expect("计时线程应可用");
        tick_once(&t.state, 5, Local::now());
        let out = t.stop();
        t.shutdown();

        assert!(
            matches!(out, Stopped::SaveFailed { ref rtype, .. } if rtype == STATE_WORK),
            "写不进去要报成「没落成」，实际 {out:?}"
        );
        assert_eq!(today_summary().0, 0, "没落库就不该计入今日完成");
    }
}
