//! 定时任务模块。
//!
//! 镜像 Python 版 `scheduler.py`：
//! - 三种调度：daily（每日 HH:MM）/ once（一次性）/ interval（窗口内每 N 分钟）
//! - 后台检查线程（30 秒轮询），到点执行目标程序
//! - 启用/禁用/删除/编辑
//! - 持久化到 `data/focusflow_scheduler.db`

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{Local, NaiveDateTime, Timelike};
use rusqlite::Connection;

use crate::paths;

/// 定时任务。
#[derive(Debug, Clone)]
pub struct ScheduledTask {
    pub id: i64,
    pub name: String,
    pub target_path: String,
    pub args: String,
    pub schedule_type: String,
    pub schedule_time: String,
    pub enabled: bool,
    pub last_run: Option<String>,
    pub created_at: String,
}

fn db_path() -> std::path::PathBuf {
    paths::data_dir().join("focusflow_scheduler.db")
}

fn open() -> rusqlite::Result<Connection> {
    std::fs::create_dir_all(paths::data_dir()).ok();
    let conn = Connection::open(db_path())?;
    conn.pragma_update(None, "journal_mode", "WAL").ok();
    conn.pragma_update(None, "synchronous", "NORMAL").ok();
    Ok(conn)
}

/// 初始化表结构（幂等）。
pub fn init_db() -> anyhow::Result<()> {
    let conn = open()?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS scheduled_tasks (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            target_path TEXT NOT NULL,
            args TEXT,
            schedule_type TEXT NOT NULL DEFAULT 'daily',
            schedule_time TEXT NOT NULL,
            enabled INTEGER NOT NULL DEFAULT 1,
            last_run TEXT,
            created_at TEXT NOT NULL
        );",
    )?;
    Ok(())
}

/// 允许被定时启动的文件扩展名（可执行类型）。
const EXECUTABLE_EXTENSIONS: [&str; 4] = ["exe", "bat", "cmd", "lnk"];

/// 校验任务目标路径：必须是绝对路径、指向已存在的可执行类型文件。
/// 定时任务是持久化的进程启动通道，入口（插件 API / 未来 UI）统一在此拦截。
fn validate_task_target(target_path: &str) -> anyhow::Result<()> {
    let t = target_path.trim();
    if t.is_empty() {
        anyhow::bail!("目标程序路径不能为空");
    }
    let p = std::path::Path::new(t);
    if !p.is_absolute() {
        anyhow::bail!("目标程序必须是绝对路径: {t}");
    }
    let ext_ok = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| EXECUTABLE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false);
    if !ext_ok {
        anyhow::bail!(
            "目标程序仅支持 {} 文件",
            EXECUTABLE_EXTENSIONS
                .iter()
                .map(|e| format!(".{e}"))
                .collect::<Vec<_>>()
                .join(" / ")
        );
    }
    if !p.is_file() {
        anyhow::bail!("目标程序不存在: {t}");
    }
    Ok(())
}

/// 添加定时任务。
pub fn add_task(
    name: &str,
    target_path: &str,
    args: &str,
    schedule_type: &str,
    schedule_time: &str,
    enabled: bool,
) -> i64 {
    if let Err(e) = validate_task_target(target_path) {
        tracing::warn!("添加定时任务被拒绝（{name}）: {e}");
        return -1;
    }
    let created = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    match open().and_then(|conn| {
        conn.execute(
            "INSERT INTO scheduled_tasks
             (name, target_path, args, schedule_type, schedule_time, enabled, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                name,
                target_path,
                args,
                schedule_type,
                schedule_time,
                if enabled { 1 } else { 0 },
                created
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }) {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("添加定时任务失败: {e}");
            -1
        }
    }
}

/// 更新任务（None 字段保持原值）。
pub fn update_task(
    id: i64,
    name: Option<&str>,
    target_path: Option<&str>,
    args: Option<&str>,
    schedule_type: Option<&str>,
    schedule_time: Option<&str>,
    enabled: Option<bool>,
) -> bool {
    // 修改目标路径时同样校验（不修改 target 字段则跳过，避免目标被删后无法编辑其他字段）
    if let Some(t) = target_path {
        if let Err(e) = validate_task_target(t) {
            tracing::warn!("更新定时任务被拒绝（id={id}）: {e}");
            return false;
        }
    }
    // 读取当前值
    let conn = match open() {
        Ok(c) => c,
        Err(_) => return false,
    };
    let Ok(existing) = conn.query_row(
        "SELECT name, target_path, args, schedule_type, schedule_time, enabled FROM scheduled_tasks WHERE id=?1",
        [id],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
            ))
        },
    ) else {
        return false;
    };
    let new_name = name.unwrap_or(&existing.0).to_string();
    let new_target = target_path.unwrap_or(&existing.1).to_string();
    let new_args = args.unwrap_or(&existing.2).to_string();
    let new_type = schedule_type.unwrap_or(&existing.3).to_string();
    let new_time = schedule_time.unwrap_or(&existing.4).to_string();
    let new_enabled = enabled.unwrap_or(existing.5 != 0);
    let last_run: Option<String> = if schedule_time.is_some() {
        None // 修改时间时重置 last_run
    } else {
        conn.query_row(
            "SELECT last_run FROM scheduled_tasks WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .ok()
        .flatten()
    };

    let r = conn.execute(
        "UPDATE scheduled_tasks SET name=?1, target_path=?2, args=?3, schedule_type=?4, schedule_time=?5, enabled=?6, last_run=?7 WHERE id=?8",
        rusqlite::params![
            new_name, new_target, new_args, new_type, new_time,
            if new_enabled { 1 } else { 0 }, last_run, id
        ],
    );
    r.map(|n| n > 0).unwrap_or(false)
}

/// 删除任务。
pub fn delete_task(id: i64) -> bool {
    open()
        .and_then(|conn| conn.execute("DELETE FROM scheduled_tasks WHERE id=?1", [id]))
        .map(|n| n > 0)
        .unwrap_or(false)
}

/// 启用/禁用任务。
pub fn toggle_task(id: i64, enabled: bool) {
    let _ = open().and_then(|conn| {
        conn.execute(
            "UPDATE scheduled_tasks SET enabled=?1 WHERE id=?2",
            rusqlite::params![if enabled { 1 } else { 0 }, id],
        )
    });
}

/// 获取所有任务。
pub fn get_all_tasks() -> Vec<ScheduledTask> {
    let conn = match open() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut stmt = match conn.prepare("SELECT * FROM scheduled_tasks ORDER BY id") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let result = stmt.query_map([], |r| {
        Ok(ScheduledTask {
            id: r.get(0)?,
            name: r.get(1)?,
            target_path: r.get(2)?,
            args: r.get(3)?,
            schedule_type: r.get(4)?,
            schedule_time: r.get(5)?,
            enabled: r.get::<_, i64>(6)? != 0,
            last_run: r.get(7)?,
            created_at: r.get(8)?,
        })
    });
    match result {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

/// 解析 interval 格式 'HH:MM-HH:MM|N'，返回 (start_min, end_min, interval)。
fn parse_interval(s: &str) -> Option<(i64, i64, i64)> {
    let (time_part, n_part) = s.split_once('|')?;
    let (start_str, end_str) = time_part.split_once('-')?;
    let parse_hhmm = |t: &str| -> Option<i64> {
        let (h, m) = t.split_once(':')?;
        let h: i64 = h.parse().ok()?;
        let m: i64 = m.parse().ok()?;
        if (0..=23).contains(&h) && (0..=59).contains(&m) {
            Some(h * 60 + m)
        } else {
            None
        }
    };
    let start = parse_hhmm(start_str)?;
    let end = parse_hhmm(end_str)?;
    let interval: i64 = n_part.trim().parse().ok()?;
    if interval <= 0 || end < start {
        return None;
    }
    Some((start, end, interval))
}

/// 判断任务是否应执行（镜像 `_should_run`）。
fn should_run(t: &ScheduledTask, now: &chrono::DateTime<Local>) -> bool {
    if !t.enabled {
        return false;
    }
    let now_min = now.hour() as i64 * 60 + now.minute() as i64;
    let last_run = t.last_run.as_deref();

    match t.schedule_type.as_str() {
        "daily" => {
            // 格式 HH:MM
            let (h, m) = match t.schedule_time.split_once(':') {
                Some((h, m)) => (h.parse::<u32>().unwrap_or(0), m.parse::<u32>().unwrap_or(0)),
                None => return false,
            };
            let target_min = h as i64 * 60 + m as i64;
            if now_min < target_min {
                return false;
            }
            match last_run {
                Some(lr) => {
                    // 今天已执行过则不重复
                    if let Ok(lr_dt) = NaiveDateTime::parse_from_str(lr, "%Y-%m-%d %H:%M:%S") {
                        if lr_dt.date() == now.date_naive() {
                            return false;
                        }
                    }
                    true
                }
                None => true,
            }
        }
        "once" => {
            // 格式 YYYY-MM-DD HH:MM
            let target = match NaiveDateTime::parse_from_str(&t.schedule_time, "%Y-%m-%d %H:%M") {
                Ok(dt) => dt,
                Err(_) => return false,
            };
            if now.naive_local() < target {
                return false;
            }
            last_run.is_none()
        }
        "interval" => {
            let (start_min, end_min, interval) = match parse_interval(&t.schedule_time) {
                Some(v) => v,
                None => return false,
            };
            if now_min < start_min || now_min > end_min {
                return false;
            }
            match last_run {
                None => now_min >= start_min,
                Some(lr) => {
                    if let Ok(lr_dt) = NaiveDateTime::parse_from_str(lr, "%Y-%m-%d %H:%M:%S") {
                        if lr_dt.date() < now.date_naive() {
                            return now_min >= start_min;
                        }
                        // 同一天：检查间隔
                        let elapsed = now.naive_local().signed_duration_since(lr_dt).num_minutes();
                        elapsed >= interval
                    } else {
                        true
                    }
                }
            }
        }
        _ => false,
    }
}

/// 执行任务（启动目标程序，DETACHED_PROCESS）。
fn execute_task(t: &ScheduledTask) {
    if t.target_path.is_empty() {
        return;
    }
    let mut cmd = std::process::Command::new(&t.target_path);
    if !t.args.is_empty() {
        for arg in t.args.split_whitespace() {
            cmd.arg(arg);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x00000008); // DETACHED_PROCESS
    }
    match cmd.spawn() {
        Ok(_) => {
            tracing::info!("定时任务已执行: {} -> {}", t.name, t.target_path);
            let now_str = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
            let _ = open().and_then(|conn| {
                conn.execute(
                    "UPDATE scheduled_tasks SET last_run=?1 WHERE id=?2",
                    rusqlite::params![now_str, t.id],
                )
            });
        }
        Err(e) => {
            tracing::error!("定时任务执行失败: {} -> {}: {e}", t.name, t.target_path);
        }
    }
}

/// 后台检查循环。
fn check_loop(stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_secs(30));
        let now = Local::now();
        let tasks = get_all_tasks();
        for t in &tasks {
            if should_run(t, &now) {
                execute_task(t);
            }
        }
    }
}

/// 后台调度线程句柄。
pub struct Scheduler {
    stop: Arc<AtomicBool>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Scheduler {
    pub fn start() -> Arc<Self> {
        let _ = init_db();
        let s = Arc::new(Self {
            stop: Arc::new(AtomicBool::new(false)),
            handle: Mutex::new(None),
        });
        let stop = Arc::clone(&s.stop);
        let handle = std::thread::Builder::new()
            .name("scheduler".into())
            .spawn(move || check_loop(stop))
            .expect("启动调度线程失败");
        *s.handle.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        tracing::info!("定时任务调度线程已启动");
        s
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = h.join();
        }
        tracing::info!("定时任务调度线程已停止");
    }
}

/// 调度描述（用于显示）。
pub fn describe_schedule(schedule_type: &str, schedule_time: &str) -> String {
    match schedule_type {
        "daily" => format!("每日 {schedule_time}"),
        "once" => format!("一次性 {schedule_time}"),
        "interval" => match parse_interval(schedule_time) {
            Some((s, e, n)) => {
                format!(
                    "每 {n} 分钟 ({:02}:{:02} ~ {:02}:{:02})",
                    s / 60,
                    s % 60,
                    e / 60,
                    e % 60
                )
            }
            None => format!("间隔执行（格式错误：{schedule_time}）"),
        },
        _ => format!("{schedule_type} {schedule_time}"),
    }
}

/// 校验调度配置。返回 (ok, error_msg)。
pub fn validate_schedule(schedule_type: &str, schedule_time: &str) -> (bool, String) {
    let t = schedule_time.trim();
    if t.is_empty() {
        return (false, "执行时间不能为空".into());
    }
    match schedule_type {
        "daily" => {
            let parts: Vec<&str> = t.split(':').collect();
            if parts.len() != 2 {
                return (false, "每日定时格式应为 HH:MM".into());
            }
            match (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                (Ok(h), Ok(m)) if h <= 23 && m <= 59 => (true, String::new()),
                _ => (false, "时间超出范围".into()),
            }
        }
        "once" => {
            if NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M").is_ok() {
                (true, String::new())
            } else {
                (false, "一次性格式应为 YYYY-MM-DD HH:MM".into())
            }
        }
        "interval" => {
            if parse_interval(t).is_some() {
                (true, String::new())
            } else {
                (false, "间隔执行格式应为 HH:MM-HH:MM|分钟数".into())
            }
        }
        _ => (false, format!("未知调度类型: {schedule_type}")),
    }
}
