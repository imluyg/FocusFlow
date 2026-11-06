//! 剪贴板历史（v1：纯文本）。
//!
//! 与截图那条路的边界：`capture` 负责把图写进剪贴板，这里负责**读回用户复制的内容**。
//!
//! 线程模型：监听线程只读剪贴板、做判定，然后把一件 `Captured` 经通道交给 UI 线程；
//! 入库由 UI 线程做。附属库连接是线程本地池（`db::connection::aux_conn`），单写者
//! 省掉了退出刷新、`clear_aux_cache`、换数据目录时的跨线程协调。
//!
//! 隐私口径（写进代码而不是只写进文档）：
//! - 只收 `CF_UNICODETEXT`，图片/文件/富文本这一版一律不碰。
//! - 超长文本**整条跳过**，不截断保存：用户点它粘贴却得到半截，比不收录更坏。
//! - `ExcludeClipboardContentFromMonitorProcessing`、`CanIncludeInClipboardHistory=0`、
//!   `CanUploadToCloudClipboard=0` 三个格式任一生效都不收录。
//! - 来源应用取剪贴板所有者窗口的进程名；取不到就留空，**不回退前台窗口**——
//!   后台复制/热键复制会把来源记到前台应用上，错数据比空数据更坏。
//! - 日志里只记长度与 hash 前缀，绝不记内容。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

use crate::paths;

/// 单条文本上限的默认值（字节）。超过整条跳过，不截断。
pub const DEFAULT_MAX_TEXT_BYTES: usize = 16 * 1024;
pub const DEFAULT_MAX_ITEMS: i64 = 500;
pub const DEFAULT_RETENTION_DAYS: i64 = 30;
/// 未固定条目的总字节上限。固定项不计入（用户显式保护的内容不该被别人挤掉）。
pub const DEFAULT_MAX_TOTAL_BYTES: i64 = 8 * 1024 * 1024;

/// 默认排除的进程（密码管理器）。格式标记（`CanIncludeInClipboardHistory=0` 那三项）
/// 是更可靠的一层，但并非所有管理器都设；这份名单是兜底的隐私保险丝。
pub const DEFAULT_EXCLUDE: &str = "1Password.exe,Bitwarden.exe,KeePass.exe,KeePassXC.exe,LastPass.exe,Dashlane.exe,Keeper.exe,Enpass.exe,RoboForm.exe,NordPass.exe";

/// 自己刚写进剪贴板时的序号。监听线程看到同一个序号就跳过，避免"程序自己复制"也进历史。
///
/// 写入方必须在**持有剪贴板锁时**、`CloseClipboard` 之前记录，顺序反了监听线程会抢在
/// 记录之前读到并入库（`GetClipboardSequenceNumber` 在锁内才是稳定的）。
static SELF_SEQ: AtomicU32 = AtomicU32::new(0);

pub fn note_self_sequence(seq: u32) {
    if seq != 0 {
        SELF_SEQ.store(seq, Ordering::Release);
    }
}

pub fn is_self_sequence(seq: u32) -> bool {
    seq != 0 && SELF_SEQ.load(Ordering::Acquire) == seq
}

/// 自己刚写进去的文本（哈希 + 时刻）。序号在系统剪贴板服务/历史面板插一脚时会跳，
/// 光靠序号会漏；内容哈希加一个短窗口兜住"同一段文本被服务重新发布"这一次。
/// 窗口只有 3 秒：用户在这之后主动复制同一段文本仍然会被收录。
static SELF_TEXT: std::sync::Mutex<Option<(String, std::time::Instant)>> =
    std::sync::Mutex::new(None);

pub fn note_self_text(text: &str) {
    let hash = sha256_hex(text.as_bytes());
    if let Ok(mut slot) = SELF_TEXT.lock() {
        *slot = Some((hash, std::time::Instant::now()));
    }
}

pub fn is_self_text(text: &str) -> bool {
    let Ok(mut slot) = SELF_TEXT.lock() else {
        return false;
    };
    let Some((hash, at)) = slot.as_ref() else {
        return false;
    };
    if at.elapsed() > std::time::Duration::from_secs(3) {
        *slot = None;
        return false;
    }
    *hash == sha256_hex(text.as_bytes())
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn db_path() -> PathBuf {
    paths::data_dir().join("focusflow_clipboard.db")
}

fn open() -> rusqlite::Result<crate::db::connection::AuxConn> {
    crate::db::connection::aux_conn(&db_path(), |conn| {
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL").ok();
        conn.busy_timeout(Duration::from_secs(15)).ok();
    })
}

/// 幂等建表。和另外几块附属库一样：没有版本号，只有 `CREATE TABLE IF NOT EXISTS`。
pub fn init_db() -> anyhow::Result<()> {
    let conn = open().map_err(|e| anyhow::anyhow!("打开剪贴板库失败：{e}"))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS clipboard_items (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            content    TEXT    NOT NULL,
            hash       TEXT    NOT NULL,
            source_app TEXT    NOT NULL DEFAULT '',
            copied_at  INTEGER NOT NULL,
            pinned     INTEGER NOT NULL DEFAULT 0,
            byte_len   INTEGER NOT NULL
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_clip_hash ON clipboard_items(hash);
        CREATE INDEX IF NOT EXISTS idx_clip_copied ON clipboard_items(pinned DESC, copied_at DESC);
        CREATE TABLE IF NOT EXISTS clipboard_daily (
            day        TEXT    NOT NULL,
            source_app TEXT    NOT NULL DEFAULT '',
            count      INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (day, source_app)
        );
        CREATE TABLE IF NOT EXISTS clipboard_snippets (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            name       TEXT    NOT NULL,
            content    TEXT    NOT NULL,
            created_at INTEGER NOT NULL,
            use_count  INTEGER NOT NULL DEFAULT 0
        );",
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 配置
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureConfig {
    pub enabled: bool,
    pub max_items: i64,
    pub retention_days: i64,
    pub max_text_bytes: usize,
    pub max_total_bytes: i64,
    pub exclude: Vec<String>,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_items: DEFAULT_MAX_ITEMS,
            retention_days: DEFAULT_RETENTION_DAYS,
            max_text_bytes: DEFAULT_MAX_TEXT_BYTES,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            exclude: Vec::new(),
        }
    }
}

pub fn capture_config() -> CaptureConfig {
    let cfg = crate::config::instance();
    let (exclude, widened) =
        crate::app_stats::parse_exclude(&cfg.get_or("clipboard", "exclude", DEFAULT_EXCLUDE));
    if !widened.is_empty() {
        // 配置里写了不带扩展名的项时，每次复制都会走这里；只提醒一次，别把日志刷满。
        static WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        if WARNED.set(()).is_ok() {
            tracing::warn!(
                "剪贴板排除清单里有没写扩展名的项（已按补 .exe 处理）：{}",
                widened.join(", ")
            );
        }
    }
    CaptureConfig {
        enabled: cfg.get_bool("clipboard", "enabled", true),
        max_items: cfg
            .get_int("clipboard", "max_items", DEFAULT_MAX_ITEMS)
            .max(0),
        retention_days: cfg
            .get_int("clipboard", "retention_days", DEFAULT_RETENTION_DAYS)
            .max(0),
        max_text_bytes: cfg
            .get_int("clipboard", "max_text_bytes", DEFAULT_MAX_TEXT_BYTES as i64)
            .clamp(1, 4 * 1024 * 1024) as usize,
        max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
        exclude,
    }
}

// ---------------------------------------------------------------------------
// 采集判定（纯函数，可脱离窗口与库单测）
// ---------------------------------------------------------------------------

/// 三个隐私格式的实际读数。`None` = 格式不存在；`Some(0)` = 应用明确说"别收"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PrivacyFlags {
    pub exclude_monitor: bool,
    pub history_flag: Option<u32>,
    pub cloud_flag: Option<u32>,
}

/// `true` = 可以收录。
pub fn privacy_verdict(flags: PrivacyFlags) -> bool {
    !(flags.exclude_monitor || flags.history_flag == Some(0) || flags.cloud_flag == Some(0))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextVerdict {
    Ok,
    Empty,
    TooLarge,
}

pub fn text_verdict(text: &str, max_bytes: usize) -> TextVerdict {
    if text.trim().is_empty() {
        return TextVerdict::Empty;
    }
    if text.len() > max_bytes {
        return TextVerdict::TooLarge;
    }
    TextVerdict::Ok
}

/// 来源应用是否不在排除清单里（大小写不敏感）。
pub fn source_allowed(source: &str, exclude: &[String]) -> bool {
    source.is_empty() || crate::app_stats::is_recordable(source, exclude)
}

/// 来源应用名归一：去首尾空白；空就是空，不编一个"未知"出来。
pub fn normalize_source_app(source: &str) -> String {
    source.trim().to_string()
}

pub fn day_key(ts_ms: i64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(ts_ms)
        .single()
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "1970-01-01".to_string())
}

// ---------------------------------------------------------------------------
// 数据模型
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipItem {
    pub id: i64,
    pub content: String,
    pub source_app: String,
    pub copied_at: i64,
    pub pinned: bool,
    pub byte_len: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    pub id: i64,
    pub name: String,
    pub content: String,
    pub created_at: i64,
    pub use_count: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClipStats {
    pub today: i64,
    pub window_days: i64,
    pub window_count: i64,
    pub total_copies: i64,
    pub total_items: i64,
    pub pinned_items: i64,
    /// (来源应用名, 次数)，按次数倒序；空名表示未知来源。
    pub per_app: Vec<(String, i64)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    Empty,
    TooLarge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestOutcome {
    Stored { id: i64, dedup: bool },
    Skipped(SkipReason),
}

fn row_to_item(r: &rusqlite::Row<'_>) -> rusqlite::Result<ClipItem> {
    Ok(ClipItem {
        id: r.get(0)?,
        content: r.get(1)?,
        source_app: r.get(2)?,
        copied_at: r.get(3)?,
        pinned: r.get::<_, i64>(4)? != 0,
        byte_len: r.get(5)?,
    })
}

const ITEM_COLS: &str = "id, content, source_app, copied_at, pinned, byte_len";

/// 入库：空/超长跳过，同 hash 只置顶不新增，次数照记，随后按配置裁剪。
pub fn ingest(text: &str, source_app: &str, copied_at_ms: i64) -> Result<IngestOutcome, String> {
    let cfg = capture_config();
    match text_verdict(text, cfg.max_text_bytes) {
        TextVerdict::Empty => return Ok(IngestOutcome::Skipped(SkipReason::Empty)),
        TextVerdict::TooLarge => return Ok(IngestOutcome::Skipped(SkipReason::TooLarge)),
        TextVerdict::Ok => {}
    }
    let source_app = normalize_source_app(source_app);
    let hash = sha256_hex(text.as_bytes());
    let byte_len = text.len() as i64;

    let conn = open().map_err(|e| format!("打开剪贴板库失败：{e}"))?;
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("剪贴板库事务开不起来：{e}"))?;
    let existing: Option<i64> = tx
        .query_row(
            "SELECT id FROM clipboard_items WHERE hash=?1",
            [&hash],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("查剪贴板去重失败：{e}"))?;
    let (id, dedup) = match existing {
        Some(id) => {
            // 只动时间和来源：固定状态是用户显式动作，不能被一次重新复制抹掉。
            tx.execute(
                "UPDATE clipboard_items SET copied_at=?1, source_app=?2 WHERE id=?3",
                rusqlite::params![copied_at_ms, source_app, id],
            )
            .map_err(|e| format!("更新剪贴板条目失败：{e}"))?;
            (id, true)
        }
        None => {
            tx.execute(
                "INSERT INTO clipboard_items(content, hash, source_app, copied_at, pinned, byte_len)
                 VALUES (?1, ?2, ?3, ?4, 0, ?5)",
                rusqlite::params![text, hash, source_app, copied_at_ms, byte_len],
            )
            .map_err(|e| format!("写入剪贴板条目失败：{e}"))?;
            (tx.last_insert_rowid(), false)
        }
    };
    let day = day_key(copied_at_ms);
    tx.execute(
        "INSERT INTO clipboard_daily(day, source_app, count) VALUES (?1, ?2, 1)
         ON CONFLICT(day, source_app) DO UPDATE SET count = count + 1",
        rusqlite::params![day, source_app],
    )
    .map_err(|e| format!("记剪贴板次数失败：{e}"))?;
    tx.commit()
        .map_err(|e| format!("提交剪贴板条目失败：{e}"))?;
    trim(&conn)?;
    Ok(IngestOutcome::Stored { id, dedup })
}

pub fn list_recent(limit: i64) -> Result<Vec<ClipItem>, String> {
    let conn = open().map_err(|e| format!("打开剪贴板库失败：{e}"))?;
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {ITEM_COLS} FROM clipboard_items ORDER BY pinned DESC, copied_at DESC LIMIT ?1"
        ))
        .map_err(|e| format!("查剪贴板历史失败：{e}"))?;
    let rows = stmt
        .query_map([limit.clamp(1, 500)], row_to_item)
        .map_err(|e| format!("查剪贴板历史失败：{e}"))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("读剪贴板历史失败：{e}"))
}

pub fn search(query: &str, limit: i64) -> Result<Vec<ClipItem>, String> {
    let q = query.trim();
    if q.is_empty() {
        return list_recent(limit);
    }
    let conn = open().map_err(|e| format!("打开剪贴板库失败：{e}"))?;
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {ITEM_COLS} FROM clipboard_items
             WHERE instr(lower(content), lower(?1)) > 0
             ORDER BY pinned DESC, copied_at DESC LIMIT ?2"
        ))
        .map_err(|e| format!("搜索剪贴板失败：{e}"))?;
    let rows = stmt
        .query_map(rusqlite::params![q, limit.clamp(1, 500)], row_to_item)
        .map_err(|e| format!("搜索剪贴板失败：{e}"))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("读剪贴板搜索结果失败：{e}"))
}

pub fn get(id: i64) -> Result<Option<ClipItem>, String> {
    let conn = open().map_err(|e| format!("打开剪贴板库失败：{e}"))?;
    conn.query_row(
        &format!("SELECT {ITEM_COLS} FROM clipboard_items WHERE id=?1"),
        [id],
        row_to_item,
    )
    .optional()
    .map_err(|e| format!("读剪贴板条目失败：{e}"))
}

/// 固定/取消固定；交回"这一条还在不在"。
pub fn set_pinned(id: i64, pinned: bool) -> Result<bool, String> {
    let conn = open().map_err(|e| format!("打开剪贴板库失败：{e}"))?;
    let n = conn
        .execute(
            "UPDATE clipboard_items SET pinned=?1 WHERE id=?2",
            rusqlite::params![if pinned { 1 } else { 0 }, id],
        )
        .map_err(|e| format!("更新剪贴板固定状态失败：{e}"))?;
    Ok(n > 0)
}

pub fn delete(id: i64) -> Result<bool, String> {
    let conn = open().map_err(|e| format!("打开剪贴板库失败：{e}"))?;
    let n = conn
        .execute("DELETE FROM clipboard_items WHERE id=?1", [id])
        .map_err(|e| format!("删除剪贴板条目失败：{e}"))?;
    Ok(n > 0)
}

/// 清空历史，保留固定项。交回删了几条。
pub fn clear_unpinned() -> Result<usize, String> {
    let conn = open().map_err(|e| format!("打开剪贴板库失败：{e}"))?;
    conn.execute("DELETE FROM clipboard_items WHERE pinned=0", [])
        .map_err(|e| format!("清空剪贴板历史失败：{e}"))
}

// ---------------------------------------------------------------------------
// 裁剪
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrimRow {
    pub id: i64,
    pub byte_len: i64,
    pub pinned: bool,
    pub copied_at: i64,
}

/// 按"新→旧"排好的行，算出该删哪些 id。固定项永不删，也不占未固定的条数/字节预算。
///
/// `max_items` / `retention_days` / `max_total_bytes` 为 0 都表示不设该项上限。
pub fn trim_plan(
    rows: &[TrimRow],
    retention_days: i64,
    max_items: i64,
    max_total_bytes: i64,
    now_ms: i64,
) -> Vec<i64> {
    let cutoff = if retention_days > 0 {
        Some(now_ms.saturating_sub(retention_days.saturating_mul(86_400_000)))
    } else {
        None
    };
    let mut doomed = Vec::new();
    let mut kept = 0i64;
    let mut kept_bytes = 0i64;
    for row in rows {
        if row.pinned {
            continue;
        }
        let too_old = cutoff.is_some_and(|c| row.copied_at < c);
        let too_many = max_items > 0 && kept >= max_items;
        let too_big = max_total_bytes > 0 && kept_bytes + row.byte_len.max(0) > max_total_bytes;
        if too_old || too_many || too_big {
            doomed.push(row.id);
        } else {
            kept += 1;
            kept_bytes += row.byte_len.max(0);
        }
    }
    doomed
}

fn trim(conn: &rusqlite::Connection) -> Result<(), String> {
    let cfg = capture_config();
    let rows: Vec<TrimRow> = {
        let mut stmt = conn
            .prepare("SELECT id, byte_len, pinned, copied_at FROM clipboard_items ORDER BY copied_at DESC")
            .map_err(|e| format!("读剪贴板裁剪清单失败：{e}"))?;
        let mapped = stmt
            .query_map([], |r| {
                Ok(TrimRow {
                    id: r.get(0)?,
                    byte_len: r.get(1)?,
                    pinned: r.get::<_, i64>(2)? != 0,
                    copied_at: r.get(3)?,
                })
            })
            .map_err(|e| format!("读剪贴板裁剪清单失败：{e}"))?;
        mapped
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| format!("读剪贴板裁剪清单失败：{e}"))?
    };
    let doomed = trim_plan(
        &rows,
        cfg.retention_days,
        cfg.max_items,
        cfg.max_total_bytes,
        now_ms(),
    );
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("剪贴板裁剪事务开不起来：{e}"))?;
    for id in doomed {
        tx.execute("DELETE FROM clipboard_items WHERE id=?1", [id])
            .map_err(|e| format!("裁剪剪贴板条目失败：{e}"))?;
    }
    // 次数台账比内容活得更久，但也留一条上界：一年。
    let cutoff_day = day_key(now_ms().saturating_sub(365 * 86_400_000));
    tx.execute("DELETE FROM clipboard_daily WHERE day < ?1", [cutoff_day])
        .map_err(|e| format!("裁剪剪贴板次数台账失败：{e}"))?;
    tx.commit().map_err(|e| format!("提交剪贴板裁剪失败：{e}"))
}

// ---------------------------------------------------------------------------
// 片段
// ---------------------------------------------------------------------------

pub fn snippets() -> Result<Vec<Snippet>, String> {
    let conn = open().map_err(|e| format!("打开剪贴板库失败：{e}"))?;
    let mut stmt = conn
        .prepare(
            "SELECT id, name, content, created_at, use_count
             FROM clipboard_snippets ORDER BY created_at DESC, id DESC",
        )
        .map_err(|e| format!("读片段失败：{e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Snippet {
                id: r.get(0)?,
                name: r.get(1)?,
                content: r.get(2)?,
                created_at: r.get(3)?,
                use_count: r.get(4)?,
            })
        })
        .map_err(|e| format!("读片段失败：{e}"))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("读片段失败：{e}"))
}

pub fn derive_snippet_name(content: &str) -> String {
    let line = content
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("片段");
    let mut out: String = line.chars().take(40).collect();
    if line.chars().count() > 40 {
        out.push('…');
    }
    if out.is_empty() {
        "片段".to_string()
    } else {
        out
    }
}

pub fn add_snippet(name: &str, content: &str) -> Result<i64, String> {
    let content = content.trim_end();
    if content.trim().is_empty() {
        return Err("片段内容为空".to_string());
    }
    let name = if name.trim().is_empty() {
        derive_snippet_name(content)
    } else {
        name.trim().to_string()
    };
    let conn = open().map_err(|e| format!("打开剪贴板库失败：{e}"))?;
    conn.execute(
        "INSERT INTO clipboard_snippets(name, content, created_at, use_count) VALUES (?1, ?2, ?3, 0)",
        rusqlite::params![name, content, now_ms()],
    )
    .map_err(|e| format!("写入片段失败：{e}"))?;
    Ok(conn.last_insert_rowid())
}

pub fn delete_snippet(id: i64) -> Result<bool, String> {
    let conn = open().map_err(|e| format!("打开剪贴板库失败：{e}"))?;
    let n = conn
        .execute("DELETE FROM clipboard_snippets WHERE id=?1", [id])
        .map_err(|e| format!("删除片段失败：{e}"))?;
    Ok(n > 0)
}

pub fn touch_snippet(id: i64) -> Result<(), String> {
    let conn = open().map_err(|e| format!("打开剪贴板库失败：{e}"))?;
    conn.execute(
        "UPDATE clipboard_snippets SET use_count = use_count + 1 WHERE id=?1",
        [id],
    )
    .map_err(|e| format!("记片段使用次数失败：{e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 统计
// ---------------------------------------------------------------------------

pub fn stats(window_days: i64) -> Result<ClipStats, String> {
    let window_days = window_days.clamp(1, 3650);
    let conn = open().map_err(|e| format!("打开剪贴板库失败：{e}"))?;
    let now = now_ms();
    let today = day_key(now);
    let since = day_key(now.saturating_sub(window_days.saturating_mul(86_400_000)));
    let today_count: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(count), 0) FROM clipboard_daily WHERE day=?1",
            [&today],
            |r| r.get(0),
        )
        .map_err(|e| format!("读剪贴板统计失败：{e}"))?;
    let window_count: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(count), 0) FROM clipboard_daily WHERE day>=?1",
            [&since],
            |r| r.get(0),
        )
        .map_err(|e| format!("读剪贴板统计失败：{e}"))?;
    let total_copies: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(count), 0) FROM clipboard_daily",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("读剪贴板统计失败：{e}"))?;
    let total_items: i64 = conn
        .query_row("SELECT COUNT(*) FROM clipboard_items", [], |r| r.get(0))
        .map_err(|e| format!("读剪贴板统计失败：{e}"))?;
    let pinned_items: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM clipboard_items WHERE pinned=1",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("读剪贴板统计失败：{e}"))?;
    let per_app = {
        let mut stmt = conn
            .prepare(
                "SELECT source_app, SUM(count) FROM clipboard_daily
                 WHERE day>=?1 GROUP BY source_app ORDER BY 2 DESC LIMIT 5",
            )
            .map_err(|e| format!("读剪贴板来源失败：{e}"))?;
        let rows = stmt
            .query_map([&since], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .map_err(|e| format!("读剪贴板来源失败：{e}"))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| format!("读剪贴板来源失败：{e}"))?
    };
    Ok(ClipStats {
        today: today_count,
        window_days,
        window_count,
        total_copies,
        total_items,
        pinned_items,
        per_app,
    })
}

// ---------------------------------------------------------------------------
// 文本加工
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transform {
    Trim,
    CollapseWhitespace,
    StripTracking,
    PrettyJson,
    Upper,
    Lower,
}

impl Transform {
    pub const ALL: [Transform; 6] = [
        Transform::Trim,
        Transform::CollapseWhitespace,
        Transform::StripTracking,
        Transform::PrettyJson,
        Transform::Upper,
        Transform::Lower,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Transform::Trim => "去首尾空白",
            Transform::CollapseWhitespace => "合并空白与空行",
            Transform::StripTracking => "URL 去追踪参数",
            Transform::PrettyJson => "JSON 美化",
            Transform::Upper => "转大写",
            Transform::Lower => "转小写",
        }
    }

    pub fn apply(self, text: &str) -> Result<String, String> {
        match self {
            Transform::Trim => Ok(text.trim().to_string()),
            Transform::CollapseWhitespace => Ok(collapse_whitespace(text)),
            Transform::StripTracking => Ok(strip_tracking(text)),
            Transform::PrettyJson => serde_json::from_str::<serde_json::Value>(text)
                .map_err(|e| format!("不是合法 JSON：{e}"))
                .and_then(|v| {
                    serde_json::to_string_pretty(&v).map_err(|e| format!("JSON 格式化失败：{e}"))
                }),
            Transform::Upper => Ok(text.to_uppercase()),
            Transform::Lower => Ok(text.to_lowercase()),
        }
    }
}

fn collapse_whitespace(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let joined = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if !joined.is_empty() {
            out.push(joined);
        }
    }
    out.join("\n")
}

fn is_tracking_key(key: &str) -> bool {
    key.starts_with("utm_")
        || matches!(
            key,
            "fbclid" | "gclid" | "yclid" | "msclkid" | "mc_eid" | "mc_cid" | "igshid"
        )
}

fn strip_tracking(text: &str) -> String {
    let (before_frag, frag) = match text.find('#') {
        Some(i) => (&text[..i], &text[i..]),
        None => (text, ""),
    };
    let Some(q) = before_frag.find('?') else {
        return text.to_string();
    };
    let base = &before_frag[..q];
    let query = &before_frag[q + 1..];
    let pairs: Vec<&str> = query.split('&').collect();
    let kept: Vec<&str> = pairs
        .iter()
        .copied()
        .filter(|pair| {
            let key = pair.split('=').next().unwrap_or("").to_ascii_lowercase();
            !is_tracking_key(&key)
        })
        .collect();
    if kept.len() == pairs.len() {
        return text.to_string();
    }
    if kept.is_empty() {
        format!("{base}{frag}")
    } else {
        format!("{base}?{}{frag}", kept.join("&"))
    }
}

// ---------------------------------------------------------------------------
// 写入剪贴板（供界面与弹窗共用）
// ---------------------------------------------------------------------------

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

#[cfg(windows)]
mod win {
    use std::sync::mpsc;
    use std::sync::Mutex;
    use std::time::Duration;

    use windows::core::w;
    use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::DataExchange::{
        AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
        GetClipboardOwner, GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
        RegisterClipboardFormatW, RemoveClipboardFormatListener, SetClipboardData,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
        PostMessageW, PostQuitMessage, RegisterClassW, TranslateMessage, HWND_MESSAGE, MSG,
        WM_CLIPBOARDUPDATE, WM_CLOSE, WM_DESTROY, WNDCLASSW,
    };

    use super::{
        capture_config, is_self_sequence, note_self_sequence, now_ms, privacy_verdict,
        source_allowed, text_verdict, PrivacyFlags, TextVerdict,
    };

    /// CF_UNICODETEXT（WinUser.h：13）。和 capture 里的 CF_DIB 一样，不为一个数开 Ole 模块。
    const CF_UNICODETEXT: u32 = 13;

    static WATCHER_TX: Mutex<Option<mpsc::Sender<Captured>>> = Mutex::new(None);

    #[derive(Debug, Clone)]
    pub struct Captured {
        pub text: String,
        pub source_app: String,
        pub copied_at_ms: i64,
        pub seq: u32,
    }

    pub struct Watcher {
        hwnd: isize,
        join: Option<std::thread::JoinHandle<()>>,
    }

    impl Watcher {
        pub fn shutdown(&mut self) {
            if self.hwnd != 0 {
                let hwnd = HWND(self.hwnd as *mut core::ffi::c_void);
                unsafe {
                    let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            }
            if let Some(j) = self.join.take() {
                let _ = j.join();
            }
            self.hwnd = 0;
        }
    }

    impl Drop for Watcher {
        fn drop(&mut self) {
            self.shutdown();
        }
    }

    /// 起监听线程。交回（句柄, 收件口）。
    pub fn start() -> Result<(Watcher, mpsc::Receiver<Captured>), String> {
        let (tx, rx) = mpsc::channel();
        {
            let mut slot = WATCHER_TX
                .lock()
                .map_err(|_| "剪贴板通道锁坏了".to_string())?;
            *slot = Some(tx);
        }
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let join = match std::thread::Builder::new()
            .name("focusflow-clipboard".into())
            .spawn(move || watcher_thread(ready_tx))
        {
            Ok(j) => j,
            Err(e) => {
                clear_sender();
                return Err(format!("剪贴板监听线程起不来：{e}"));
            }
        };
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(hwnd)) => Ok((
                Watcher {
                    hwnd,
                    join: Some(join),
                },
                rx,
            )),
            Ok(Err(e)) => {
                let _ = join.join();
                clear_sender();
                Err(e)
            }
            Err(_) => {
                clear_sender();
                Err("剪贴板监听线程 5 秒内没有报到".to_string())
            }
        }
    }

    fn clear_sender() {
        if let Ok(mut slot) = WATCHER_TX.lock() {
            *slot = None;
        }
    }

    fn watcher_thread(ready: mpsc::SyncSender<Result<isize, String>>) {
        let hwnd = match create_watcher_window() {
            Ok(h) => h,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        if unsafe { AddClipboardFormatListener(hwnd) }.is_err() {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            let _ = ready.send(Err(
                "注册剪贴板监听失败（AddClipboardFormatListener）".to_string()
            ));
            return;
        }
        let _ = ready.send(Ok(hwnd.0 as isize));
        let mut msg = MSG::default();
        while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        clear_sender();
    }

    fn create_watcher_window() -> Result<HWND, String> {
        static CLASS: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
        let registered = CLASS.get_or_init(|| {
            let hinstance =
                unsafe { GetModuleHandleW(None) }.map_err(|e| format!("取模块句柄失败：{e}"))?;
            let class = WNDCLASSW {
                lpfnWndProc: Some(watcher_proc),
                hInstance: hinstance.into(),
                lpszClassName: w!("FocusFlowClipboardWatcher"),
                ..Default::default()
            };
            if unsafe { RegisterClassW(&class) } == 0 {
                return Err("注册剪贴板监听窗口类失败".to_string());
            }
            Ok(())
        });
        registered.as_ref().map_err(|e| e.clone())?;
        let hinstance =
            unsafe { GetModuleHandleW(None) }.map_err(|e| format!("取模块句柄失败：{e}"))?;
        unsafe {
            CreateWindowExW(
                Default::default(),
                w!("FocusFlowClipboardWatcher"),
                w!(""),
                Default::default(),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(hinstance.into()),
                None,
            )
        }
        .map_err(|e| format!("创建剪贴板监听窗口失败：{e}"))
    }

    unsafe extern "system" fn watcher_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_CLIPBOARDUPDATE => {
                capture_once();
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = RemoveClipboardFormatListener(hwnd);
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    fn capture_once() {
        let cfg = capture_config();
        if !cfg.enabled {
            return;
        }
        let owner = super::super::capture::win::clip_owner_hwnd().ok();
        let mut opened = false;
        for attempt in 0..5u32 {
            if unsafe { OpenClipboard(owner) }.is_ok() {
                opened = true;
                break;
            }
            if attempt < 4 {
                std::thread::sleep(Duration::from_millis(30));
            }
        }
        if !opened {
            return;
        }

        let read = unsafe { read_locked() };
        let _ = unsafe { CloseClipboard() };
        let Some((text, owner_hwnd, seq)) = read else {
            return;
        };
        if super::is_self_text(&text) {
            return;
        }
        let source_app = if owner_hwnd.0.is_null() {
            String::new()
        } else {
            crate::app_stats::window_process_name(owner_hwnd.0 as isize).unwrap_or_default()
        };
        if !source_allowed(&source_app, &cfg.exclude) {
            return;
        }
        match text_verdict(&text, cfg.max_text_bytes) {
            TextVerdict::Ok => {}
            TextVerdict::Empty => return,
            TextVerdict::TooLarge => {
                tracing::info!(
                    "剪贴板有一条 {} 字节的文本，超过上限 {} 字节，整条跳过",
                    text.len(),
                    cfg.max_text_bytes
                );
                return;
            }
        }
        let sender = WATCHER_TX.lock().ok().and_then(|s| s.as_ref().cloned());
        if let Some(tx) = sender {
            let _ = tx.send(Captured {
                text,
                source_app,
                copied_at_ms: now_ms(),
                seq,
            });
        }
    }

    /// 锁已持有：读序号、隐私格式、所有权与文本。所有 Win32 调用都在这一小段里。
    unsafe fn read_locked() -> Option<(String, HWND, u32)> {
        let seq = GetClipboardSequenceNumber();
        if is_self_sequence(seq) {
            return None;
        }
        // 剪贴板所有者是我们自己的窗口 ⇒ 这一笔就是程序自己写的（截图复制/提示框复制/
        // 弹窗回车）。放在锁内问：所有者会在下一次 EmptyClipboard 时被别的程序换走。
        let owner = GetClipboardOwner().unwrap_or_default();
        if !owner.0.is_null()
            && crate::app_stats::window_process_id(owner.0 as isize) == Some(std::process::id())
        {
            return None;
        }
        let formats = privacy_formats();
        let flags = PrivacyFlags {
            exclude_monitor: IsClipboardFormatAvailable(formats.0).is_ok(),
            history_flag: read_dword_format(formats.1),
            cloud_flag: read_dword_format(formats.2),
        };
        if !privacy_verdict(flags) {
            return None;
        }
        if IsClipboardFormatAvailable(CF_UNICODETEXT).is_err() {
            return None;
        }
        let owner = GetClipboardOwner().unwrap_or_default();
        let text = read_unicode_text()?;
        Some((text, owner, seq))
    }

    fn privacy_formats() -> (u32, u32, u32) {
        static CACHE: std::sync::OnceLock<(u32, u32, u32)> = std::sync::OnceLock::new();
        *CACHE.get_or_init(|| unsafe {
            (
                RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing")),
                RegisterClipboardFormatW(w!("CanIncludeInClipboardHistory")),
                RegisterClipboardFormatW(w!("CanUploadToCloudClipboard")),
            )
        })
    }

    unsafe fn read_dword_format(fmt: u32) -> Option<u32> {
        if fmt == 0 || IsClipboardFormatAvailable(fmt).is_err() {
            return None;
        }
        let h = GetClipboardData(fmt).ok()?;
        let hg = HGLOBAL(h.0);
        if GlobalSize(hg) < 4 {
            return None;
        }
        let p = GlobalLock(hg);
        if p.is_null() {
            return None;
        }
        let v = *(p as *const u32);
        let _ = GlobalUnlock(hg);
        Some(v)
    }

    unsafe fn read_unicode_text() -> Option<String> {
        let h = GetClipboardData(CF_UNICODETEXT).ok()?;
        let hg = HGLOBAL(h.0);
        let bytes = GlobalSize(hg);
        if bytes < 2 {
            return None;
        }
        // 上限在拷贝之前就挡掉：先读进内存再判超限会让一条 1 MB 的复制多一次全量分配。
        let max_units = (super::capture_config().max_text_bytes).saturating_add(2);
        if bytes / 2 > max_units {
            return None;
        }
        let p = GlobalLock(hg);
        if p.is_null() {
            return None;
        }
        let units = std::slice::from_raw_parts(p as *const u16, bytes / 2);
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        let text = String::from_utf16_lossy(&units[..end]);
        let _ = GlobalUnlock(hg);
        Some(text)
    }

    /// 把文本写进剪贴板。返回后调用方可以关窗/还焦点，粘贴由用户自己按 Ctrl+V。
    pub fn write_text(text: &str) -> Result<(), String> {
        write_text_inner(text, true)
    }

    /// 测试用：读当前的纯文本（保存/恢复用户的剪贴板，别让用例把人家正在粘的东西弄丢）。
    #[cfg(any(test, feature = "test-utils"))]
    pub fn read_text_for_test() -> Option<String> {
        let owner = super::super::capture::win::clip_owner_hwnd().ok();
        if unsafe { OpenClipboard(owner) }.is_err() {
            return None;
        }
        let text = unsafe { read_unicode_text() };
        let _ = unsafe { CloseClipboard() };
        text
    }

    fn write_text_inner(text: &str, mark_self: bool) -> Result<(), String> {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = wide.len() * 2;
        let owner = super::super::capture::win::clip_owner_hwnd()?;
        let mut last = String::from("未尝试");
        for attempt in 1..=5u32 {
            match unsafe { OpenClipboard(Some(owner)) } {
                Ok(()) => {
                    let result = unsafe { write_text_locked(&wide, bytes) };
                    // 序号必须在锁内、关之前记：顺序反了监听线程会抢在记录之前入库。
                    let touched = !matches!(&result, Err(WriteError::Untouched(_)));
                    if mark_self && touched {
                        note_self_sequence(unsafe { GetClipboardSequenceNumber() });
                        super::note_self_text(text);
                    }
                    if let Err(e) = unsafe { CloseClipboard() } {
                        tracing::warn!("关闭剪贴板失败（本次结果仍按 {result:?} 上报）：{e}");
                    }
                    return match result {
                        Ok(()) => Ok(()),
                        Err(WriteError::Untouched(e)) | Err(WriteError::ClearedButFailed(e)) => {
                            Err(e)
                        }
                    };
                }
                Err(e) => last = format!("{e}"),
            }
            if attempt < 5 {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        Err(format!("剪贴板被其它程序占用，重试 5 次后放弃：{last}"))
    }

    #[derive(Debug)]
    enum WriteError {
        /// 还没动用户的内容（备料/打开失败）。
        Untouched(String),
        /// 已经清空、但新内容没写进去。
        ClearedButFailed(String),
    }

    unsafe fn write_text_locked(wide: &[u16], bytes: usize) -> Result<(), WriteError> {
        let hg = GlobalAlloc(GMEM_MOVEABLE, bytes)
            .map_err(|e| WriteError::Untouched(format!("分配剪贴板内存失败：{e}")))?;
        let dst = GlobalLock(hg);
        if dst.is_null() {
            let _ = GlobalFree(Some(hg));
            return Err(WriteError::Untouched("GlobalLock 返回空指针".to_string()));
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), dst as *mut u16, wide.len());
        let _ = GlobalUnlock(hg);
        if let Err(e) = EmptyClipboard() {
            let _ = GlobalFree(Some(hg));
            return Err(WriteError::Untouched(format!("清空剪贴板失败：{e}")));
        }
        if let Err(e) = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(hg.0))) {
            let _ = GlobalFree(Some(hg));
            return Err(WriteError::ClearedButFailed(format!("写入剪贴板失败：{e}")));
        }
        Ok(())
    }
}

#[cfg(windows)]
pub use win::{start, write_text, Captured, Watcher};

#[cfg(all(windows, any(test, feature = "test-utils")))]
pub use win::read_text_for_test;

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, byte_len: i64, pinned: bool, copied_at: i64) -> TrimRow {
        TrimRow {
            id,
            byte_len,
            pinned,
            copied_at,
        }
    }

    #[test]
    fn privacy_formats_with_zero_flag_block_capture() {
        assert!(privacy_verdict(PrivacyFlags::default()));
        assert!(!privacy_verdict(PrivacyFlags {
            exclude_monitor: true,
            ..Default::default()
        }));
        assert!(!privacy_verdict(PrivacyFlags {
            history_flag: Some(0),
            ..Default::default()
        }));
        assert!(privacy_verdict(PrivacyFlags {
            history_flag: Some(1),
            ..Default::default()
        }));
        assert!(!privacy_verdict(PrivacyFlags {
            cloud_flag: Some(0),
            ..Default::default()
        }));
    }

    #[test]
    fn text_verdict_rejects_blank_and_oversize() {
        assert_eq!(text_verdict("  \n\t ", 10), TextVerdict::Empty);
        assert_eq!(text_verdict("hello", 5), TextVerdict::Ok);
        assert_eq!(text_verdict("hello!", 5), TextVerdict::TooLarge);
    }

    #[test]
    fn trim_keeps_pinned_and_applies_three_budgets() {
        let now = 1_000_000_000_000i64;
        let day = 86_400_000i64;

        // 条数：五条未固定、三条配额 ⇒ 最旧两条删。
        let five: Vec<TrimRow> = (1..=5)
            .map(|i| row(i, 10, false, now - (i - 1) * day))
            .collect();
        assert_eq!(trim_plan(&five, 0, 3, 0, now), vec![4, 5]);

        // 天数：90 天前那条删；固定项即便 90 天前也留下。
        let ages = vec![
            row(1, 10, false, now),
            row(2, 10, false, now - 90 * day),
            row(3, 10, true, now - 90 * day),
        ];
        assert_eq!(trim_plan(&ages, 30, 0, 0, now), vec![2]);

        // 字节：25 字节预算、每条 10 ⇒ 只有最新两条（20）装得下，其余从旧到新删。
        assert_eq!(trim_plan(&five, 0, 0, 25, now), vec![3, 4, 5]);

        // 固定项不占未固定的条数/字节配额。
        let mixed = vec![
            row(1, 10, true, now),
            row(2, 10, true, now - day),
            row(3, 10, false, now - 2 * day),
            row(4, 10, false, now - 3 * day),
        ];
        assert_eq!(trim_plan(&mixed, 0, 1, 0, now), vec![4]);
        assert_eq!(trim_plan(&mixed, 0, 0, 10, now), vec![4]);
    }

    #[test]
    fn trim_zero_means_unlimited() {
        let now = 1_000_000_000_000i64;
        let rows = vec![row(1, 999_999, false, 0), row(2, 999_999, false, 0)];
        assert!(trim_plan(&rows, 0, 0, 0, now).is_empty());
    }

    #[test]
    fn collapse_whitespace_joins_lines_and_drops_blanks() {
        let got = Transform::CollapseWhitespace
            .apply("  a\t b \n\n c\n")
            .unwrap();
        assert_eq!(got, "a b\nc");
    }

    #[test]
    fn strip_tracking_keeps_other_params_and_fragment() {
        let got = Transform::StripTracking
            .apply("https://e.com/p?a=1&utm_source=x&fbclid=y&b=2#frag")
            .unwrap();
        assert_eq!(got, "https://e.com/p?a=1&b=2#frag");
        assert_eq!(
            Transform::StripTracking
                .apply("https://e.com/p?utm_source=x")
                .unwrap(),
            "https://e.com/p"
        );
        assert_eq!(
            Transform::StripTracking.apply("没有问号的文本").unwrap(),
            "没有问号的文本"
        );
        assert_eq!(
            Transform::StripTracking
                .apply("https://e.com/p?utm_source=x&UTM_Medium=y")
                .unwrap(),
            "https://e.com/p",
            "键名大小写不敏感"
        );
    }

    #[test]
    fn pretty_json_reports_invalid_input() {
        let got = Transform::PrettyJson.apply("{\"a\":1}").unwrap();
        assert!(got.contains("\n  \"a\": 1"));
        assert!(Transform::PrettyJson.apply("不是 json").is_err());
    }

    #[test]
    fn case_transforms_are_unicode_aware() {
        assert_eq!(Transform::Upper.apply("abc 中文").unwrap(), "ABC 中文");
        assert_eq!(Transform::Lower.apply("ABC 中文").unwrap(), "abc 中文");
    }

    #[test]
    fn snippet_name_is_first_non_empty_line_and_bounded() {
        assert_eq!(derive_snippet_name("\n  第一行  \n第二行"), "第一行");
        let long = "字".repeat(60);
        let name = derive_snippet_name(&long);
        assert_eq!(name.chars().count(), 41);
        assert!(name.ends_with('…'));
        assert_eq!(derive_snippet_name("   "), "片段");
    }

    #[test]
    fn self_sequence_matches_only_the_recorded_value() {
        note_self_sequence(0xDEAD_BEEF);
        assert!(is_self_sequence(0xDEAD_BEEF));
        assert!(!is_self_sequence(0xDEAD_BEE0));
        assert!(!is_self_sequence(0), "0 是未知，不当作自己");
    }

    #[test]
    fn day_key_has_calendar_shape() {
        let key = day_key(1_700_000_000_000);
        assert_eq!(key.len(), 10);
        assert_eq!(&key[4..5], "-");
        assert_eq!(&key[7..8], "-");
    }
}
