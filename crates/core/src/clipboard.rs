//! 剪贴板历史（文本 / 图片 / 文件）。
//!
//! 与截图那条路的边界：`capture` 负责把图写进剪贴板，这里负责**读回用户复制的内容**。
//!
//! 线程模型：监听线程只读剪贴板、做判定（图片在这里解码并落盘），然后把一件 `Captured`
//! 经通道交给 UI 线程；入库由 UI 线程做。附属库连接是线程本地池（`db::connection::aux_conn`），
//! 单写者省掉了退出刷新、`clear_aux_cache`、换数据目录时的跨线程协调。
//!
//! 隐私口径（写进代码而不是只写进文档）：
//! - 收 `CF_UNICODETEXT`、图片（`PNG` / `CF_DIB`）与 `CF_HDROP`；富文本不碰。
//! - 三种类型共用同一套隐私判定：三个隐私格式、排除进程、自写抑制。
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
///
/// 图片按落盘 PNG 的大小计入。默认给到 64 MB：一张 4K 截图的 PNG 常有 3–8 MB，
/// 8 MB 的总预算会让第二张图就把第一张裁掉，图片支持等于摆设。
pub const DEFAULT_MAX_TOTAL_BYTES: i64 = 64 * 1024 * 1024;

/// 单张图片的默认上限（PNG 字节）。超限整张跳过 —— 不缩放了再收：用户复制的是原图，
/// 存一份已经被压过的版本，粘回去就变糊了。
pub const DEFAULT_MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
/// 图片轴长/总像素上限。只防御"解出来要 4 GB"的坏数据，正常截图远低于这个。
pub const MAX_IMAGE_SIDE: u32 = 8192;
pub const MAX_IMAGE_PIXELS: u64 = 32_000_000;
/// 缩略图长边（像素）。列表里只画 32–48 px，留到 160 是为了高分屏下仍不发虚。
pub const THUMB_MAX_SIDE: u32 = 160;

/// 一条记录最多带多少个文件路径。再多就只是"路径列表"，收进历史也没有粘贴价值。
pub const MAX_FILES_PER_ITEM: usize = 20;
/// 单条路径的字符上限。Windows 长路径上限 32767，这里按"够用且不让一条记录吃掉整库"取。
pub const MAX_FILE_PATH_CHARS: usize = 1024;

/// 默认排除的进程（密码管理器）。格式标记（`CanIncludeInClipboardHistory=0` 那三项）
/// 是更可靠的一层，但并非所有管理器都设；这份名单是兜底的隐私保险丝。
pub const DEFAULT_EXCLUDE: &str = "1Password.exe,Bitwarden.exe,KeePass.exe,KeePassXC.exe,LastPass.exe,Dashlane.exe,Keeper.exe,Enpass.exe,RoboForm.exe,NordPass.exe";

/// 自己刚写进剪贴板时的序号。监听线程看到同一个序号就跳过，避免"程序自己复制"也进历史。
///
/// 写入方必须在**持有剪贴板锁时**、`CloseClipboard` 之前记录，顺序反了监听线程会抢在
/// 记录之前读到并入库（`GetClipboardSequenceNumber` 在锁内才是稳定的）。
static SELF_SEQ: AtomicU32 = AtomicU32::new(0);

/// 本次运行里的临时暂停（托盘「暂停 15 分钟」）。与 `[clipboard] enabled` 不同：
/// 这条不落盘，重启就恢复记录；0 = 没暂停。
static PAUSED_UNTIL_MS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

pub fn pause_for(seconds: i64) {
    if seconds > 0 {
        PAUSED_UNTIL_MS.store(
            now_ms().saturating_add(seconds.saturating_mul(1000)),
            Ordering::Release,
        );
    }
}

/// 暂停到什么时候（Unix ms）；0 = 没暂停（过期也当没暂停）。
pub fn pause_until_ms() -> i64 {
    let until = PAUSED_UNTIL_MS.load(Ordering::Acquire);
    if until > now_ms() {
        until
    } else {
        0
    }
}

pub fn is_paused() -> bool {
    pause_until_ms() > 0
}

/// 复制后是否自动向原窗口粘贴（v2 自动粘贴）。
///
/// 默认关：把键击注入别的程序是用户该显式点头的行为。关着的时候弹窗行为与 v1.2 完全一致。
pub fn auto_paste_enabled() -> bool {
    crate::config::instance().get_bool("clipboard", "auto_paste", false)
}

/// 这个窗口是不是本进程的。自动粘贴先要排掉"粘回自己"（比如从主窗打开弹窗）。
#[cfg(windows)]
pub fn window_belongs_to_us(hwnd: isize) -> bool {
    crate::app_stats::window_process_id(hwnd) == Some(std::process::id())
}

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
///
/// v1.2 加了图片/文件那几列：新库直接建全，旧库走 `ensure_item_columns` 的
/// `ALTER TABLE … ADD COLUMN`（每列各自判一次，半截迁移下次启动会补完）。
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
            byte_len   INTEGER NOT NULL,
            kind       TEXT    NOT NULL DEFAULT 'text',
            image_path TEXT    NOT NULL DEFAULT '',
            thumb_path TEXT    NOT NULL DEFAULT '',
            file_paths TEXT    NOT NULL DEFAULT '',
            width      INTEGER NOT NULL DEFAULT 0,
            height     INTEGER NOT NULL DEFAULT 0
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
    ensure_item_columns(&conn)?;
    sweep_orphan_images(&conn);
    Ok(())
}

/// v1.1 及更早的库只有前七列。补列是 `ALTER TABLE … ADD COLUMN`（自带提交），
/// 所以每列单独判、单独补：任何一列失败都带着原因返回，已经补上的列不会因为
/// 后面失败而被误判成"迁移没跑过"。
fn ensure_item_columns(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    use crate::db::connection::{column_presence, Presence};

    const EXTRA: [(&str, &str); 6] = [
        ("kind", "TEXT NOT NULL DEFAULT 'text'"),
        ("image_path", "TEXT NOT NULL DEFAULT ''"),
        ("thumb_path", "TEXT NOT NULL DEFAULT ''"),
        ("file_paths", "TEXT NOT NULL DEFAULT ''"),
        ("width", "INTEGER NOT NULL DEFAULT 0"),
        ("height", "INTEGER NOT NULL DEFAULT 0"),
    ];
    for (name, decl) in EXTRA {
        match column_presence(conn, "clipboard_items", name) {
            Presence::Yes => {}
            Presence::No => {
                conn.execute(
                    &format!("ALTER TABLE clipboard_items ADD COLUMN {name} {decl}"),
                    [],
                )
                .map_err(|e| anyhow::anyhow!("剪贴板迁移：补 {name} 列失败：{e}"))?;
            }
            Presence::Unreadable(why) => {
                // 读不出列清单就不能补列（补错是 duplicate column），但也不能假装这列在：
                // 带着原因失败，调用方会把这层能力关掉并说出来。
                anyhow::bail!("剪贴板迁移：读不出 clipboard_items 有没有 {name} 这一列（{why}）");
            }
        }
    }
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
    pub max_image_bytes: usize,
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
            max_image_bytes: DEFAULT_MAX_IMAGE_BYTES,
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
        max_image_bytes: cfg
            .get_int(
                "clipboard",
                "max_image_bytes",
                DEFAULT_MAX_IMAGE_BYTES as i64,
            )
            .clamp(1, 64 * 1024 * 1024) as usize,
        // 0 = 不设上限（与 `trim_plan` 的口径一致）。
        max_total_bytes: cfg
            .get_int(
                "clipboard",
                "max_total_mb",
                DEFAULT_MAX_TOTAL_BYTES / (1024 * 1024),
            )
            .clamp(0, 4096)
            .saturating_mul(1024 * 1024),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipKind {
    Text,
    Image,
    File,
}

impl ClipKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ClipKind::Text => "text",
            ClipKind::Image => "image",
            ClipKind::File => "file",
        }
    }

    /// 库里读到不认识的值时按文本处理：这是 v1 唯一的类型，也是最无害的退路
    /// （宁可把一个坏值当文本展示，也别让整行读不出来）。
    pub fn from_db(s: &str) -> ClipKind {
        match s {
            "image" => ClipKind::Image,
            "file" => ClipKind::File,
            _ => ClipKind::Text,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipItem {
    pub id: i64,
    pub content: String,
    pub source_app: String,
    pub copied_at: i64,
    pub pinned: bool,
    pub byte_len: i64,
    pub kind: ClipKind,
    /// 图片落盘的 PNG 绝对路径；非图片为空。
    pub image_path: Option<String>,
    /// 缩略图绝对路径；生成失败时为空（界面退回文字徽标）。
    pub thumb_path: Option<String>,
    /// 文件条目里的路径（按复制时的顺序）。
    pub file_paths: Vec<String>,
    /// 图片像素尺寸；非图片为 0。
    pub width: i32,
    pub height: i32,
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
    let image_path: String = r.get(7)?;
    let thumb_path: String = r.get(8)?;
    let files_json: String = r.get(9)?;
    Ok(ClipItem {
        id: r.get(0)?,
        content: r.get(1)?,
        source_app: r.get(2)?,
        copied_at: r.get(3)?,
        pinned: r.get::<_, i64>(4)? != 0,
        byte_len: r.get(5)?,
        kind: ClipKind::from_db(&r.get::<_, String>(6)?),
        image_path: (!image_path.is_empty()).then_some(image_path),
        thumb_path: (!thumb_path.is_empty()).then_some(thumb_path),
        file_paths: parse_json_paths(&files_json),
        width: r.get(10)?,
        height: r.get(11)?,
    })
}

const ITEM_COLS: &str = "id, content, source_app, copied_at, pinned, byte_len, kind, \
                         image_path, thumb_path, file_paths, width, height";

fn parse_json_paths(json: &str) -> Vec<String> {
    if json.trim().is_empty() {
        return Vec::new();
    }
    serde_json::from_str::<Vec<String>>(json).unwrap_or_default()
}

/// 一次入库需要的全部列。文本/图片/文件共用同一条"去重 → 置顶 → 记次数 → 裁剪"。
struct NewItem<'a> {
    kind: ClipKind,
    content: &'a str,
    hash: &'a str,
    source_app: &'a str,
    copied_at_ms: i64,
    byte_len: i64,
    image_path: &'a str,
    thumb_path: &'a str,
    file_paths: &'a str,
    width: i32,
    height: i32,
}

fn insert_or_touch(new: NewItem<'_>) -> Result<IngestOutcome, String> {
    let conn = open().map_err(|e| format!("打开剪贴板库失败：{e}"))?;
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("剪贴板库事务开不起来：{e}"))?;
    let existing: Option<i64> = tx
        .query_row(
            "SELECT id FROM clipboard_items WHERE hash=?1",
            [new.hash],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("查剪贴板去重失败：{e}"))?;
    let (id, dedup) = match existing {
        Some(id) => {
            // 只动时间和来源：固定状态是用户显式动作，不能被一次重新复制抹掉。
            tx.execute(
                "UPDATE clipboard_items SET copied_at=?1, source_app=?2 WHERE id=?3",
                rusqlite::params![new.copied_at_ms, new.source_app, id],
            )
            .map_err(|e| format!("更新剪贴板条目失败：{e}"))?;
            (id, true)
        }
        None => {
            tx.execute(
                "INSERT INTO clipboard_items(
                     content, hash, source_app, copied_at, pinned, byte_len,
                     kind, image_path, thumb_path, file_paths, width, height)
                 VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                rusqlite::params![
                    new.content,
                    new.hash,
                    new.source_app,
                    new.copied_at_ms,
                    new.byte_len,
                    new.kind.as_str(),
                    new.image_path,
                    new.thumb_path,
                    new.file_paths,
                    new.width,
                    new.height
                ],
            )
            .map_err(|e| format!("写入剪贴板条目失败：{e}"))?;
            (tx.last_insert_rowid(), false)
        }
    };
    let day = day_key(new.copied_at_ms);
    tx.execute(
        "INSERT INTO clipboard_daily(day, source_app, count) VALUES (?1, ?2, 1)
         ON CONFLICT(day, source_app) DO UPDATE SET count = count + 1",
        rusqlite::params![day, new.source_app],
    )
    .map_err(|e| format!("记剪贴板次数失败：{e}"))?;
    tx.commit()
        .map_err(|e| format!("提交剪贴板条目失败：{e}"))?;
    trim(&conn)?;
    Ok(IngestOutcome::Stored { id, dedup })
}

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
    insert_or_touch(NewItem {
        kind: ClipKind::Text,
        content: text,
        hash: &hash,
        source_app: &source_app,
        copied_at_ms,
        byte_len: text.len() as i64,
        image_path: "",
        thumb_path: "",
        file_paths: "",
        width: 0,
        height: 0,
    })
}

/// 图片入库。`stored` 已经由监听线程落盘（见 `store_image`），这里只写库。
pub fn ingest_image(
    stored: &StoredImage,
    source_app: &str,
    copied_at_ms: i64,
) -> Result<IngestOutcome, String> {
    let image_path = stored.png_path.to_string_lossy().to_string();
    let thumb_path = stored
        .thumb_path
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let content = format!("图片 {} × {}", stored.width, stored.height);
    let source_app = normalize_source_app(source_app);
    insert_or_touch(NewItem {
        kind: ClipKind::Image,
        content: &content,
        hash: &stored.hash,
        source_app: &source_app,
        copied_at_ms,
        byte_len: stored.png_bytes as i64,
        image_path: &image_path,
        thumb_path: &thumb_path,
        file_paths: "",
        width: stored.width as i32,
        height: stored.height as i32,
    })
}

/// 文件路径清单的判定：去掉空串与重复项后是否还够一条记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilesVerdict {
    Ok(Vec<String>),
    Empty,
    TooMany,
}

pub fn files_verdict(paths: &[String], max_files: usize) -> FilesVerdict {
    let mut out: Vec<String> = Vec::new();
    for p in paths {
        let p = p.trim();
        if p.is_empty() || p.chars().count() > MAX_FILE_PATH_CHARS {
            continue;
        }
        if !out.iter().any(|q| q.eq_ignore_ascii_case(p)) {
            out.push(p.to_string());
        }
    }
    if out.is_empty() {
        FilesVerdict::Empty
    } else if out.len() > max_files {
        FilesVerdict::TooMany
    } else {
        FilesVerdict::Ok(out)
    }
}

/// 行里显示的文件清单："3 个文件：a.png, b.txt, c.pdf"，超过三个只点名前三个。
pub fn files_label(paths: &[String]) -> String {
    let names: Vec<String> = paths
        .iter()
        .map(|p| {
            std::path::Path::new(p)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| p.clone())
        })
        .collect();
    let shown: Vec<String> = names.iter().take(3).cloned().collect();
    let tail = if names.len() > shown.len() {
        " …".to_string()
    } else {
        String::new()
    };
    format!("{} 个文件：{}{tail}", names.len(), shown.join(", "))
}

/// 文件入库。路径原样存 JSON，失效与否在显示/复制时现问磁盘。
pub fn ingest_files(
    paths: &[String],
    source_app: &str,
    copied_at_ms: i64,
) -> Result<IngestOutcome, String> {
    let FilesVerdict::Ok(paths) = files_verdict(paths, MAX_FILES_PER_ITEM) else {
        return Ok(IngestOutcome::Skipped(SkipReason::Empty));
    };
    let json = serde_json::to_string(&paths).map_err(|e| format!("文件路径序列化失败：{e}"))?;
    let content = files_label(&paths);
    let source_app = normalize_source_app(source_app);
    let hash = sha256_hex(json.as_bytes());
    insert_or_touch(NewItem {
        kind: ClipKind::File,
        content: &content,
        hash: &hash,
        source_app: &source_app,
        copied_at_ms,
        byte_len: json.len() as i64,
        image_path: "",
        thumb_path: "",
        file_paths: &json,
        width: 0,
        height: 0,
    })
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

/// 最近一条**文本**的内容（插件 `clipboard.read_text` 的唯一数据源）。
///
/// 只读文本：图片/文件在这一版不对外，所以"最新那条"如果是图片，就往更早找文本，
/// 而不是把图片的尺寸标签当内容交出去。超过 `max_bytes` 的不截断，带着原因回来 ——
/// 插件拿到半截文本比拿不到更糟（与收录口径同一条）。
pub fn latest_text(max_bytes: usize) -> Result<Option<String>, String> {
    let conn = open().map_err(|e| format!("打开剪贴板库失败：{e}"))?;
    let row: Option<String> = conn
        .query_row(
            "SELECT content FROM clipboard_items
             WHERE kind='text' ORDER BY copied_at DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("读最近一条文本失败：{e}"))?;
    match row {
        None => Ok(None),
        Some(text) if text.len() > max_bytes => Err(format!(
            "最近一条文本有 {} 字节，超过上限 {max_bytes} 字节，未交给插件",
            text.len()
        )),
        Some(text) => Ok(Some(text)),
    }
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

/// 批量固定/取消固定，一个事务里做完。交回真正改到的条数。
pub fn set_pinned_many(ids: &[i64], pinned: bool) -> Result<usize, String> {
    if ids.is_empty() {
        return Ok(0);
    }
    let conn = open().map_err(|e| format!("打开剪贴板库失败：{e}"))?;
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("剪贴板库事务开不起来：{e}"))?;
    let mut n = 0usize;
    for id in ids {
        n += tx
            .execute(
                "UPDATE clipboard_items SET pinned=?1 WHERE id=?2",
                rusqlite::params![if pinned { 1 } else { 0 }, id],
            )
            .map_err(|e| format!("更新剪贴板固定状态失败：{e}"))?;
    }
    tx.commit()
        .map_err(|e| format!("提交剪贴板固定状态失败：{e}"))?;
    Ok(n)
}

/// 批量删除，一个事务里做完；图片/缩略图随行一起清掉。
pub fn delete_many(ids: &[i64]) -> Result<usize, String> {
    if ids.is_empty() {
        return Ok(0);
    }
    let conn = open().map_err(|e| format!("打开剪贴板库失败：{e}"))?;
    delete_rows_cleaning_files(&conn, ids)
}

pub fn delete(id: i64) -> Result<bool, String> {
    let conn = open().map_err(|e| format!("打开剪贴板库失败：{e}"))?;
    Ok(delete_rows_cleaning_files(&conn, &[id])? > 0)
}

/// 清空历史，保留固定项。交回删了几条。
pub fn clear_unpinned() -> Result<usize, String> {
    let conn = open().map_err(|e| format!("打开剪贴板库失败：{e}"))?;
    let ids: Vec<i64> = {
        let mut stmt = conn
            .prepare("SELECT id FROM clipboard_items WHERE pinned=0")
            .map_err(|e| format!("清空剪贴板历史失败：{e}"))?;
        let rows = stmt
            .query_map([], |r| r.get(0))
            .map_err(|e| format!("清空剪贴板历史失败：{e}"))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| format!("清空剪贴板历史失败：{e}"))?
    };
    delete_rows_cleaning_files(&conn, &ids)
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
    let paths = collect_item_paths(conn, &doomed);
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("剪贴板裁剪事务开不起来：{e}"))?;
    for id in &doomed {
        tx.execute("DELETE FROM clipboard_items WHERE id=?1", [id])
            .map_err(|e| format!("裁剪剪贴板条目失败：{e}"))?;
    }
    // 次数台账比内容活得更久，但也留一条上界：一年。
    let cutoff_day = day_key(now_ms().saturating_sub(365 * 86_400_000));
    tx.execute("DELETE FROM clipboard_daily WHERE day < ?1", [cutoff_day])
        .map_err(|e| format!("裁剪剪贴板次数台账失败：{e}"))?;
    tx.commit()
        .map_err(|e| format!("提交剪贴板裁剪失败：{e}"))?;
    unlink_unreferenced(conn, &paths);
    Ok(())
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

pub fn rename_snippet(id: i64, name: &str) -> Result<bool, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("片段名不能为空".to_string());
    }
    let conn = open().map_err(|e| format!("打开剪贴板库失败：{e}"))?;
    let n = conn
        .execute(
            "UPDATE clipboard_snippets SET name=?1 WHERE id=?2",
            rusqlite::params![name, id],
        )
        .map_err(|e| format!("重命名片段失败：{e}"))?;
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

// ---------------------------------------------------------------------------
// 图片：解码、缩略图、落盘
// ---------------------------------------------------------------------------

pub fn images_dir() -> PathBuf {
    paths::data_dir().join("clipboard").join("images")
}

pub fn thumbs_dir() -> PathBuf {
    paths::data_dir().join("clipboard").join("thumbs")
}

/// 像素数据的 hash：宽高一起进哈希，免得"2×2 与 1×4 字节恰好相同"的两张图被合并。
pub fn image_hash(rgba: &[u8], w: u32, h: u32) -> String {
    let mut hsh = Sha256::new();
    hsh.update(w.to_le_bytes());
    hsh.update(h.to_le_bytes());
    hsh.update(rgba);
    format!("{:x}", hsh.finalize())
}

pub fn check_image_dims(w: u32, h: u32) -> Result<(), String> {
    if w == 0 || h == 0 {
        return Err(format!("图片尺寸不可用（{w}x{h}）"));
    }
    if w > MAX_IMAGE_SIDE || h > MAX_IMAGE_SIDE {
        return Err(format!("图片太大（{w}x{h}，单轴上限 {MAX_IMAGE_SIDE}）"));
    }
    if w as u64 * h as u64 > MAX_IMAGE_PIXELS {
        return Err(format!(
            "图片太大（{w}x{h}，总像素上限 {MAX_IMAGE_PIXELS}）"
        ));
    }
    Ok(())
}

/// `CF_DIB` → RGBA8。只接 `BI_RGB` 的 24/32 bpp（GDI 与绝大多数应用交的就是这两种）；
/// 其他压缩交给调用方跳过并记日志 —— 猜错格式的代价是存进来一张花屏。
///
/// `biHeight < 0` 是 top-down（屏幕截图常见），正数是 bottom-up（GDI 默认）。
/// 32 bpp 的 alpha 常常整片为 0（GDI 不填）：全 0 时按不透明处理，否则粘回去是透明的。
pub fn dib_to_rgba(dib: &[u8]) -> Result<(Vec<u8>, u32, u32), String> {
    if dib.len() < 40 {
        return Err(format!("DIB 太短（{} 字节）", dib.len()));
    }
    // 少数程序把带 BITMAPFILEHEADER 的整张 .bmp 塞进 CF_DIB：认出来就跳过 14 字节。
    let at = if dib.len() >= 54 && &dib[0..2] == b"BM" {
        14
    } else {
        0
    };
    if dib.len() < at + 40 {
        return Err("DIB 头被截断".to_string());
    }
    let rd_u32 = |off: usize| u32::from_le_bytes(dib[off..off + 4].try_into().unwrap());
    let rd_i32 = |off: usize| i32::from_le_bytes(dib[off..off + 4].try_into().unwrap());
    let header_size = rd_u32(at) as usize;
    if header_size < 40 || at + header_size > dib.len() {
        return Err(format!("DIB 头长度不可用（{header_size}）"));
    }
    let w = rd_i32(at + 4);
    let raw_h = rd_i32(at + 8);
    let planes = u16::from_le_bytes(dib[at + 12..at + 14].try_into().unwrap());
    let bpp = u16::from_le_bytes(dib[at + 14..at + 16].try_into().unwrap());
    let compression = rd_u32(at + 16);
    let clr_used = rd_u32(at + 32) as usize;
    if planes != 1 {
        return Err(format!("DIB 平面数异常（{planes}）"));
    }
    if w <= 0 || raw_h == 0 {
        return Err(format!("DIB 尺寸异常（{w}x{raw_h}）"));
    }
    let (w, h) = (w as u32, raw_h.unsigned_abs());
    check_image_dims(w, h)?;
    if bpp != 24 && bpp != 32 {
        return Err(format!("不支持的 DIB 位深（{bpp} bpp）"));
    }
    // BI_RGB(0) 是直接通道顺序；BI_BITFIELDS(3) 把红绿蓝掩码放在头后面
    //（40 字节的老头紧跟着 12 字节掩码；V4/V5 头则已经含在头里）。
    // .NET 的 `Clipboard.SetImage` 交的就是后者，不认它等于"从 WinForms 应用复制图片收不到"。
    let masks: Option<(u32, u32, u32)> = match compression {
        0 => None,
        3 => {
            if bpp != 32 {
                return Err(format!("BI_BITFIELDS 只支持 32 bpp，这份是 {bpp}"));
            }
            let m_off = if header_size >= 52 {
                at + 40
            } else {
                at + header_size
            };
            if dib.len() < m_off + 12 {
                return Err("DIB 通道掩码被截断".to_string());
            }
            let rm = rd_u32(m_off);
            let gm = rd_u32(m_off + 4);
            let bm = rd_u32(m_off + 8);
            if rm == 0 || gm == 0 || bm == 0 {
                return Err(format!("DIB 通道掩码不可用（{rm:#x}/{gm:#x}/{bm:#x}）"));
            }
            Some((rm, gm, bm))
        }
        other => return Err(format!("不支持的 DIB 压缩方式（{other}）")),
    };
    // 掩码在 40 字节老头里是"紧跟其后"的 12 字节，要算进像素起点。
    let mask_bytes = if masks.is_some() && header_size < 52 {
        12
    } else {
        0
    };
    let pixel_at = at + header_size + mask_bytes + clr_used * 4;
    let stride = (w as usize * (bpp as usize / 8)).div_ceil(4) * 4;
    let need = pixel_at + stride * h as usize;
    if need > dib.len() {
        return Err(format!(
            "DIB 数据不完整：需要 {need} 字节，只有 {}",
            dib.len()
        ));
    }
    let top_down = raw_h < 0;
    let mut rgba = vec![0u8; w as usize * h as usize * 4];
    let mut any_alpha = false;
    for y in 0..h as usize {
        let src_row = if top_down { y } else { h as usize - 1 - y };
        let row = &dib[pixel_at + src_row * stride..];
        let dst = &mut rgba[y * w as usize * 4..(y + 1) * w as usize * 4];
        for x in 0..w as usize {
            let d = &mut dst[x * 4..x * 4 + 4];
            match masks {
                Some((rm, gm, bm)) => {
                    let px = u32::from_le_bytes(
                        row[x * 4..x * 4 + 4]
                            .try_into()
                            .expect("32 bpp 每像素四字节"),
                    );
                    d[0] = channel8(px, rm);
                    d[1] = channel8(px, gm);
                    d[2] = channel8(px, bm);
                    d[3] = 255;
                }
                None => {
                    let s = &row[x * (bpp as usize / 8)..];
                    d[0] = s[2];
                    d[1] = s[1];
                    d[2] = s[0];
                    d[3] = if bpp == 32 { s[3] } else { 255 };
                    any_alpha |= d[3] != 0;
                }
            }
        }
    }
    if masks.is_none() && bpp == 32 && !any_alpha {
        for px in rgba.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
    }
    Ok((rgba, w, h))
}

/// 按掩码取一个通道并缩到 0..255。
fn channel8(px: u32, mask: u32) -> u8 {
    let shift = mask.trailing_zeros();
    let max = mask >> shift;
    let v = (px & mask) >> shift;
    ((v * 255 + max / 2) / max) as u8
}

/// 任意常见 PNG → RGBA8。剪贴板上别家应用交来的 PNG 可能是调色板/灰度/带 tRNS，
/// 而落盘统一是 RGBA8：这里用 `png` 的变换把差异抹平，落盘之后就只有一种形状。
pub fn decode_png_rgba_loose(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), String> {
    use png::Transformations;
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(Transformations::ALPHA | Transformations::STRIP_16);
    decoder.set_limits(png::Limits {
        bytes: 256 * 1024 * 1024,
    });
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("PNG 头读不出来：{e}"))?;
    let (w, h) = {
        let info = reader.info();
        (info.width, info.height)
    };
    check_image_dims(w, h)?;
    let mut buf = vec![0u8; w as usize * h as usize * 4];
    let out = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("PNG 解码失败（{w}x{h}）：{e}"))?;
    match out.color_type {
        png::ColorType::Rgba
        | png::ColorType::GrayscaleAlpha
        | png::ColorType::Rgb
        | png::ColorType::Grayscale => {}
        other => return Err(format!("PNG 色型不受支持：{other:?}")),
    }
    let px = w as usize * h as usize;
    let src = &buf[..out.buffer_size()];
    let rgba = match out.color_type {
        png::ColorType::Rgba => src.to_vec(),
        png::ColorType::Rgb => {
            let mut v = vec![0u8; px * 4];
            for i in 0..px {
                v[i * 4] = src[i * 3];
                v[i * 4 + 1] = src[i * 3 + 1];
                v[i * 4 + 2] = src[i * 3 + 2];
                v[i * 4 + 3] = 255;
            }
            v
        }
        png::ColorType::Grayscale => {
            let mut v = vec![0u8; px * 4];
            for i in 0..px {
                let g = src[i];
                v[i * 4] = g;
                v[i * 4 + 1] = g;
                v[i * 4 + 2] = g;
                v[i * 4 + 3] = 255;
            }
            v
        }
        png::ColorType::GrayscaleAlpha => {
            let mut v = vec![0u8; px * 4];
            for i in 0..px {
                let g = src[i * 2];
                v[i * 4] = g;
                v[i * 4 + 1] = g;
                v[i * 4 + 2] = g;
                v[i * 4 + 3] = src[i * 2 + 1];
            }
            v
        }
        _ => unreachable!("上面已经挡掉了"),
    };
    if rgba.len() != px * 4 {
        return Err(format!(
            "PNG 像素长度不符：{} 字节，按 {w}x{h} RGBA 应是 {}",
            rgba.len(),
            px * 4
        ));
    }
    Ok((rgba, w, h))
}

/// 长边压到 `max_side` 以内（区域平均）。图本身比上限小就原样返回。
pub fn make_thumb(
    rgba: &[u8],
    w: u32,
    h: u32,
    max_side: u32,
) -> Result<(Vec<u8>, u32, u32), String> {
    check_image_dims(w, h)?;
    let want = w as usize * h as usize * 4;
    if rgba.len() != want {
        return Err(format!(
            "缩略图输入长度不符：{} 字节，应为 {want}（{w}x{h}）",
            rgba.len()
        ));
    }
    let max_side = max_side.max(1);
    if w <= max_side && h <= max_side {
        return Ok((rgba.to_vec(), w, h));
    }
    let long = w.max(h) as f64;
    let scale = max_side as f64 / long;
    let dw = ((w as f64 * scale).round() as u32).max(1).min(max_side);
    let dh = ((h as f64 * scale).round() as u32).max(1).min(max_side);
    let mut out = vec![0u8; dw as usize * dh as usize * 4];
    for dy in 0..dh as usize {
        let y0 = dy * h as usize / dh as usize;
        let y1 = ((dy + 1) * h as usize / dh as usize).max(y0 + 1);
        for dx in 0..dw as usize {
            let x0 = dx * w as usize / dw as usize;
            let x1 = ((dx + 1) * w as usize / dw as usize).max(x0 + 1);
            let mut acc = [0u64; 4];
            let mut n = 0u64;
            for y in y0..y1.min(h as usize) {
                for x in x0..x1.min(w as usize) {
                    let i = (y * w as usize + x) * 4;
                    for c in 0..4 {
                        acc[c] += rgba[i + c] as u64;
                    }
                    n += 1;
                }
            }
            let n = n.max(1);
            let o = (dy * dw as usize + dx) * 4;
            for c in 0..4 {
                out[o + c] = (acc[c] / n) as u8;
            }
        }
    }
    Ok((out, dw, dh))
}

/// 一张已经落盘的剪贴板图片。
#[derive(Debug, Clone)]
pub struct StoredImage {
    pub hash: String,
    pub png_path: PathBuf,
    pub thumb_path: Option<PathBuf>,
    pub png_bytes: usize,
    pub width: u32,
    pub height: u32,
}

fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes).map_err(|e| format!("写 {} 失败：{e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("把临时文件换到 {} 失败：{e}", path.display())
    })
}

/// 图片落盘：`images/<hash>.png` + `thumbs/<hash>.png`（长边 `THUMB_MAX_SIDE`）。
///
/// 已经存在就复用（同一张图复制两次不该再写一遍盘）。缩略图失败不算整条失败：
/// 库里那行照样入库，界面退回文字徽标 —— 比"复制了图片却什么也没记下"好。
pub fn store_image(rgba: &[u8], w: u32, h: u32) -> Result<StoredImage, String> {
    check_image_dims(w, h)?;
    let want = w as usize * h as usize * 4;
    if rgba.len() != want {
        return Err(format!(
            "图片像素长度不符：{} 字节，应为 {want}（{w}x{h}）",
            rgba.len()
        ));
    }
    let cfg = capture_config();
    let png = crate::capture::encode_png(rgba, w, h)?;
    if png.len() > cfg.max_image_bytes {
        return Err(format!(
            "图片 PNG 有 {} 字节，超过单张上限 {} 字节，整条跳过",
            png.len(),
            cfg.max_image_bytes
        ));
    }
    let hash = image_hash(rgba, w, h);
    let dir = images_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("建图片目录失败：{e}"))?;
    let png_path = dir.join(format!("{hash}.png"));
    if !png_path.exists() {
        write_atomic(&png_path, &png)?;
    }
    let thumb_path = match make_thumb(rgba, w, h, THUMB_MAX_SIDE) {
        Ok((trgba, tw, th)) => match crate::capture::encode_png(&trgba, tw, th) {
            Ok(bytes) => {
                let tdir = thumbs_dir();
                let path = tdir.join(format!("{hash}.png"));
                let written = std::fs::create_dir_all(&tdir)
                    .map_err(|e| format!("建缩略图目录失败：{e}"))
                    .and_then(|_| {
                        if path.exists() {
                            Ok(())
                        } else {
                            write_atomic(&path, &bytes)
                        }
                    });
                match written {
                    Ok(()) => Some(path),
                    Err(e) => {
                        tracing::warn!("剪贴板缩略图落盘失败（这张图仍会入库）：{e}");
                        None
                    }
                }
            }
            Err(e) => {
                tracing::warn!("剪贴板缩略图编码失败（这张图仍会入库）：{e}");
                None
            }
        },
        Err(e) => {
            tracing::warn!("剪贴板缩略图生成失败（这张图仍会入库）：{e}");
            None
        }
    };
    Ok(StoredImage {
        hash,
        png_path,
        thumb_path,
        png_bytes: png.len(),
        width: w,
        height: h,
    })
}

/// 解析 `CF_HDROP` 的负载（`DROPFILES` 头 + 以 null 收尾的路径表）。
///
/// 宽字符与 ANSI 两种都认：`fWide=1` 是 Windows 资源管理器的常态，老程序仍可能交 ANSI。
/// 空路径就是列表结束（双 null 收尾），超出缓冲的路径直接截断，绝不让坏数据越过边界。
pub fn parse_hdrop(bytes: &[u8]) -> Vec<String> {
    if bytes.len() < 20 {
        return Vec::new();
    }
    let p_files = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
    let f_wide = i32::from_le_bytes(bytes[16..20].try_into().unwrap()) != 0;
    if p_files < 20 || p_files >= bytes.len() {
        return Vec::new();
    }
    let body = &bytes[p_files..];
    let mut out = Vec::new();
    if f_wide {
        let units: Vec<u16> = body
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        let mut start = 0usize;
        for (i, &u) in units.iter().enumerate() {
            if u != 0 {
                continue;
            }
            if i == start {
                break;
            }
            out.push(String::from_utf16_lossy(&units[start..i]));
            start = i + 1;
        }
    } else {
        for part in body.split(|&b| b == 0) {
            if part.is_empty() {
                break;
            }
            out.push(String::from_utf8_lossy(part).to_string());
        }
    }
    out
}

/// 文件条目里"还在的 / 已经没了的"。只问磁盘，不碰库。
pub fn split_alive_paths(paths: &[String]) -> (Vec<String>, Vec<String>) {
    let mut alive = Vec::new();
    let mut gone = Vec::new();
    for p in paths {
        if std::path::Path::new(p).exists() {
            alive.push(p.clone());
        } else {
            gone.push(p.clone());
        }
    }
    (alive, gone)
}

/// 把一条历史复制回剪贴板。弹窗与管理页共用，行为差异只在这里分。
///
/// 返回值是给人看的一句话（写了哪几种格式 / 为什么只剩路径文本）。
pub fn copy_item(item: &ClipItem) -> Result<String, String> {
    copy_parts(
        item.kind,
        &item.content,
        item.image_path.as_deref(),
        &item.file_paths,
    )
}

/// 与 `copy_item` 同一套行为，只是界面那边手里只有行摘要、没有整个 `ClipItem`。
pub fn copy_parts(
    kind: ClipKind,
    content: &str,
    image_path: Option<&str>,
    file_paths: &[String],
) -> Result<String, String> {
    match kind {
        ClipKind::Text => win::write_text(content).map(|_| "已复制文本".to_string()),
        ClipKind::Image => {
            let path = image_path.ok_or_else(|| "这条图片记录没有落盘路径".to_string())?;
            let png = std::fs::read(path).map_err(|e| format!("读图片文件失败：{e}"))?;
            let (rgba, w, h) = decode_png_rgba_loose(&png)?;
            let mut bgra = rgba;
            for px in bgra.as_chunks_mut::<4>().0 {
                px.swap(0, 2);
            }
            let formats = crate::capture::win::write_image_to_clipboard(
                &bgra,
                w,
                h,
                &png,
                Some(std::path::Path::new(path)),
            )?;
            Ok(format!("已复制图片 {} × {}（{}）", w, h, formats.join("+")))
        }
        ClipKind::File => {
            let (alive, gone) = split_alive_paths(file_paths);
            if alive.is_empty() {
                // 文件都不在了：写回路径文本仍比"什么也没发生"有用，但必须说清是文本。
                let text = file_paths.join("\r\n");
                win::write_text(&text)?;
                return Ok(format!(
                    "{} 个文件都已不存在，已把路径作为文本复制",
                    file_paths.len()
                ));
            }
            let n = win::write_files(&alive)?;
            if gone.is_empty() {
                Ok(format!("已复制 {n} 个文件"))
            } else {
                Ok(format!(
                    "已复制 {n} 个文件（{} 个路径已失效，跳过）",
                    gone.len()
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 图片/文件的磁盘清理
// ---------------------------------------------------------------------------

fn collect_item_paths(conn: &rusqlite::Connection, ids: &[i64]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if ids.is_empty() {
        return out;
    }
    let Ok(mut stmt) =
        conn.prepare("SELECT image_path, thumb_path FROM clipboard_items WHERE id=?1")
    else {
        return out;
    };
    for id in ids {
        if let Ok((img, thumb)) = stmt.query_row([id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        }) {
            for p in [img, thumb] {
                if !p.is_empty() {
                    out.push(PathBuf::from(p));
                }
            }
        }
    }
    out
}

/// 删掉"已经没有任何一行引用"的图片/缩略图。别的行还指着同一路径时留着（hash 去重下
/// 正常不会发生，但删错就是删掉用户还会再看到的内容）。
fn unlink_unreferenced(conn: &rusqlite::Connection, paths: &[PathBuf]) {
    for p in paths {
        let s = p.to_string_lossy().to_string();
        let refs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM clipboard_items WHERE image_path=?1 OR thumb_path=?1",
                [&s],
                |r| r.get(0),
            )
            .unwrap_or(1); // 查不动就当还有人用：宁可留下孤儿文件，也不删活的
        if refs == 0 {
            if let Err(e) = std::fs::remove_file(p) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!("删除剪贴板图片文件失败（{}）：{e}", p.display());
                }
            }
        }
    }
}

fn delete_rows_cleaning_files(conn: &rusqlite::Connection, ids: &[i64]) -> Result<usize, String> {
    if ids.is_empty() {
        return Ok(0);
    }
    let paths = collect_item_paths(conn, ids);
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("剪贴板库事务开不起来：{e}"))?;
    let mut n = 0usize;
    for id in ids {
        n += tx
            .execute("DELETE FROM clipboard_items WHERE id=?1", [id])
            .map_err(|e| format!("删除剪贴板条目失败：{e}"))?;
    }
    tx.commit()
        .map_err(|e| format!("提交剪贴板删除失败：{e}"))?;
    unlink_unreferenced(conn, &paths);
    Ok(n)
}

/// 启动时扫一遍：库还没引用的图片/缩略图删掉（"文件写了、库没写成"那一路的残局）。
fn sweep_orphan_images(conn: &rusqlite::Connection) {
    let mut referenced = std::collections::HashSet::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT image_path FROM clipboard_items WHERE image_path<>''
         UNION SELECT thumb_path FROM clipboard_items WHERE thumb_path<>''",
    ) {
        if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
            for p in rows.flatten() {
                referenced.insert(p);
            }
        }
    }
    for dir in [images_dir(), thumbs_dir()] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if referenced.contains(&path.to_string_lossy().to_string()) {
                continue;
            }
            // 十分钟内的文件可能是另一条链路刚写好、库还没来得及记的那一张。
            let fresh = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age < std::time::Duration::from_secs(600));
            if fresh {
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => tracing::info!("清理没被引用的剪贴板图片：{}", path.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!("清理剪贴板图片失败（{}）：{e}", path.display()),
            }
        }
    }
}

#[cfg(windows)]
mod win {
    use std::sync::mpsc;
    use std::sync::Mutex;
    use std::time::Duration;

    use windows::core::w;
    use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, DIB_RGB_COLORS, HBITMAP,
    };
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

    /// CF_UNICODETEXT（WinUser.h：13）、CF_DIB（8）、CF_HDROP（15）、CF_BITMAP（2）。
    /// 和 capture 里的用法一样，不为几个常数开 Ole 模块。
    const CF_UNICODETEXT: u32 = 13;
    const CF_DIB: u32 = 8;
    const CF_HDROP: u32 = 15;
    const CF_BITMAP: u32 = 2;
    /// 从剪贴板整块读出来的上限。4K/32bpp 的 DIB 约 33 MB；再往上就是"不该整块搬进内存"。
    const MAX_READ_BYTES: usize = 64 * 1024 * 1024;

    static WATCHER_TX: Mutex<Option<mpsc::Sender<Captured>>> = Mutex::new(None);

    #[derive(Debug, Clone)]
    pub enum Captured {
        Text {
            text: String,
            source_app: String,
            copied_at_ms: i64,
        },
        Image {
            stored: super::StoredImage,
            source_app: String,
            copied_at_ms: i64,
        },
        Files {
            paths: Vec<String>,
            source_app: String,
            copied_at_ms: i64,
        },
    }

    impl Captured {
        pub fn source_app(&self) -> &str {
            match self {
                Captured::Text { source_app, .. }
                | Captured::Image { source_app, .. }
                | Captured::Files { source_app, .. } => source_app,
            }
        }
    }

    /// 锁内从剪贴板抄出来的原始内容。PNG/DIB 在锁外才解码：解码一张 4K 图要几十毫秒，
    /// 拿着剪贴板锁做这件事会让别的程序读不到剪贴板。`CF_BITMAP` 是个 GDI 句柄，
    /// 只能在锁内 `GetDIBits` 出来，所以它交来的已经是 RGBA。
    enum RawClip {
        Text(String),
        Image(RawImage),
        Files(Vec<String>),
    }

    enum RawImage {
        Png(Vec<u8>),
        Dib(Vec<u8>),
        /// 已经从 `CF_BITMAP` 转好的 RGBA 像素与尺寸。
        Bitmap(Vec<u8>, u32, u32),
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
        if !cfg.enabled || super::is_paused() {
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
        let Some((raw, owner_hwnd)) = read else {
            return;
        };
        let source_app = if owner_hwnd.0.is_null() {
            String::new()
        } else {
            crate::app_stats::window_process_name(owner_hwnd.0 as isize).unwrap_or_default()
        };
        if !source_allowed(&source_app, &cfg.exclude) {
            return;
        }
        let sender = WATCHER_TX.lock().ok().and_then(|s| s.as_ref().cloned());
        let Some(tx) = sender else {
            return;
        };
        match raw {
            RawClip::Text(text) => {
                if super::is_self_text(&text) {
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
                let _ = tx.send(Captured::Text {
                    text,
                    source_app,
                    copied_at_ms: now_ms(),
                });
            }
            RawClip::Image(image) => {
                // 锁已经放开：解码与落盘都在这一步做，慢也不挡别的程序读剪贴板。
                let decoded = match image {
                    RawImage::Png(bytes) => super::decode_png_rgba_loose(&bytes),
                    RawImage::Dib(bytes) => super::dib_to_rgba(&bytes),
                    RawImage::Bitmap(rgba, w, h) => Ok((rgba, w, h)),
                };
                let (rgba, w, h) = match decoded {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::info!("剪贴板里有一张图片读不出来，跳过：{e}");
                        return;
                    }
                };
                let stored = match super::store_image(&rgba, w, h) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::info!("剪贴板图片没有入库：{e}");
                        return;
                    }
                };
                let _ = tx.send(Captured::Image {
                    stored,
                    source_app,
                    copied_at_ms: now_ms(),
                });
            }
            RawClip::Files(paths) => {
                let paths = match super::files_verdict(&paths, super::MAX_FILES_PER_ITEM) {
                    super::FilesVerdict::Ok(paths) => paths,
                    super::FilesVerdict::Empty => return,
                    super::FilesVerdict::TooMany => {
                        tracing::info!(
                            "剪贴板里有超过 {} 个文件，整条跳过",
                            super::MAX_FILES_PER_ITEM
                        );
                        return;
                    }
                };
                let _ = tx.send(Captured::Files {
                    paths,
                    source_app,
                    copied_at_ms: now_ms(),
                });
            }
        }
    }

    /// 锁已持有：读序号、隐私格式、所有权，以及**一份**内容（文本 > 图片 > 文件）。
    /// 读取顺序就是优先级：Excel 单元格这类同时带文本与 DIB 的按文本收，不会一条变两条。
    unsafe fn read_locked() -> Option<(RawClip, HWND)> {
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
        if IsClipboardFormatAvailable(CF_UNICODETEXT).is_ok() {
            if let Some(text) = read_unicode_text() {
                return Some((RawClip::Text(text), owner));
            }
            // 文本格式在、内容却读不出来：继续按图片/文件试，不因一次坏文本丢掉这次更新。
        }
        let png_fmt = unsafe { RegisterClipboardFormatW(w!("PNG")) };
        if png_fmt != 0 && IsClipboardFormatAvailable(png_fmt).is_ok() {
            if let Some(bytes) = read_global_bytes(png_fmt, MAX_READ_BYTES) {
                return Some((RawClip::Image(RawImage::Png(bytes)), owner));
            }
        }
        if IsClipboardFormatAvailable(CF_DIB).is_ok() {
            if let Some(bytes) = read_global_bytes(CF_DIB, MAX_READ_BYTES) {
                return Some((RawClip::Image(RawImage::Dib(bytes)), owner));
            }
        }
        // .NET 的 `Clipboard.SetImage` 只交 CF_BITMAP，Paint/浏览器那一路多半交 CF_DIB；
        // 两条都认，少一类"复制了图片却什么也没记下"。
        if IsClipboardFormatAvailable(CF_BITMAP).is_ok() {
            if let Ok((rgba, w, h)) = read_bitmap_rgba() {
                return Some((RawClip::Image(RawImage::Bitmap(rgba, w, h)), owner));
            }
        }
        if IsClipboardFormatAvailable(CF_HDROP).is_ok() {
            if let Some(bytes) = read_global_bytes(CF_HDROP, MAX_READ_BYTES) {
                let paths = super::parse_hdrop(&bytes);
                if !paths.is_empty() {
                    return Some((RawClip::Files(paths), owner));
                }
            }
        }
        None
    }

    /// 整块抄出某个格式的负载。超过上限就放弃：读进来再判会先分配一大块内存。
    unsafe fn read_global_bytes(fmt: u32, max_bytes: usize) -> Option<Vec<u8>> {
        let h = GetClipboardData(fmt).ok()?;
        let hg = HGLOBAL(h.0);
        let size = GlobalSize(hg);
        if size == 0 || size > max_bytes {
            return None;
        }
        let p = GlobalLock(hg);
        if p.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(p as *const u8, size).to_vec();
        let _ = GlobalUnlock(hg);
        Some(bytes)
    }

    /// 剪贴板里的 `CF_BITMAP`（GDI 句柄）→ RGBA。必须在剪贴板锁内调用：
    /// 那个句柄只在我们开着剪贴板的这段时间里有效。
    unsafe fn read_bitmap_rgba() -> Result<(Vec<u8>, u32, u32), String> {
        let h = GetClipboardData(CF_BITMAP).map_err(|e| format!("取 CF_BITMAP 失败：{e}"))?;
        let hbmp = HBITMAP(h.0);
        let mut bm = BITMAP::default();
        let got = GetObjectW(
            hbmp.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut _ as *mut core::ffi::c_void),
        );
        if got == 0 {
            return Err("读 CF_BITMAP 尺寸失败（GetObjectW）".to_string());
        }
        let (w, hgt) = (bm.bmWidth, bm.bmHeight);
        if w <= 0 || hgt <= 0 {
            return Err(format!("CF_BITMAP 尺寸异常（{w}x{hgt}）"));
        }
        super::check_image_dims(w as u32, hgt as u32)?;
        let mut buf = vec![0u8; w as usize * hgt as usize * 4];
        let dc = GetDC(None);
        if dc.is_invalid() {
            return Err("GetDC 失败：拿不到设备上下文".to_string());
        }
        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader = crate::capture::win::dib_header(w as u32, hgt as u32);
        let lines = GetDIBits(
            dc,
            hbmp,
            0,
            hgt as u32,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, dc);
        if lines != hgt {
            return Err(format!("GetDIBits 只取回 {lines} 行，应为 {hgt} 行"));
        }
        // BGRA → RGBA；GDI 位图通常不填 alpha，全零时按不透明处理。
        let mut any_alpha = false;
        for px in buf.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
            any_alpha |= px[3] != 0;
        }
        if !any_alpha {
            for px in buf.as_chunks_mut::<4>().0 {
                px[3] = 255;
            }
        }
        Ok((buf, w as u32, hgt as u32))
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

    /// 把一组文件路径写进剪贴板（`CF_HDROP`）。调用方负责先过滤掉已经不在的路径。
    pub fn write_files(paths: &[String]) -> Result<usize, String> {
        if paths.is_empty() {
            return Err("没有可写的文件路径".to_string());
        }
        let files: Vec<std::path::PathBuf> = paths.iter().map(std::path::PathBuf::from).collect();
        let payload = crate::capture::win::hdrop_payload(&files);
        let owner = super::super::capture::win::clip_owner_hwnd()?;
        let mut last = String::from("未尝试");
        for attempt in 1..=5u32 {
            match unsafe { OpenClipboard(Some(owner)) } {
                Ok(()) => {
                    let result = unsafe { write_hdrop_locked(&payload) };
                    if result.is_ok() {
                        // 与写文本同一条口径：序号在锁内、Close 之前记。
                        note_self_sequence(unsafe { GetClipboardSequenceNumber() });
                    }
                    if let Err(e) = unsafe { CloseClipboard() } {
                        tracing::warn!("关闭剪贴板失败（本次结果仍按 {result:?} 上报）：{e}");
                    }
                    return result.map(|_| paths.len());
                }
                Err(e) => last = format!("{e}"),
            }
            if attempt < 5 {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        Err(format!("剪贴板被其它程序占用，重试 5 次后放弃：{last}"))
    }

    unsafe fn write_hdrop_locked(payload: &[u8]) -> Result<(), String> {
        let hg = GlobalAlloc(GMEM_MOVEABLE, payload.len())
            .map_err(|e| format!("分配剪贴板内存失败：{e}"))?;
        let dst = GlobalLock(hg);
        if dst.is_null() {
            let _ = GlobalFree(Some(hg));
            return Err("GlobalLock 返回空指针".to_string());
        }
        std::ptr::copy_nonoverlapping(payload.as_ptr(), dst as *mut u8, payload.len());
        let _ = GlobalUnlock(hg);
        if let Err(e) = EmptyClipboard() {
            let _ = GlobalFree(Some(hg));
            return Err(format!("清空剪贴板失败：{e}"));
        }
        if let Err(e) = SetClipboardData(CF_HDROP, Some(HANDLE(hg.0))) {
            let _ = GlobalFree(Some(hg));
            return Err(format!("写入剪贴板失败：{e}"));
        }
        Ok(())
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

    #[test]
    fn pause_is_session_only_and_expires() {
        pause_for(3600);
        assert!(is_paused(), "暂停一小时之内应当算暂停");
        assert!(pause_until_ms() > now_ms());
        PAUSED_UNTIL_MS.store(0, Ordering::Release);
        assert!(!is_paused(), "清掉之后立刻恢复记录");
        assert_eq!(pause_until_ms(), 0);
    }

    // ---- v1.2：图片 / 文件 ----

    fn dib_header(w: i32, h: i32, bpp: u16, compression: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&40u32.to_le_bytes());
        out.extend_from_slice(&w.to_le_bytes());
        out.extend_from_slice(&h.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&bpp.to_le_bytes());
        out.extend_from_slice(&compression.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0i32.to_le_bytes());
        out.extend_from_slice(&0i32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out
    }

    #[test]
    fn dib_24bpp_bottom_up_flips_rows_and_swaps_channels() {
        let mut dib = dib_header(2, 2, 24, 0);
        // bottom-up：先存最下面一行（蓝、白），再存上面一行（红、绿），每行补 2 字节对齐。
        dib.extend_from_slice(&[0xFF, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0x00, 0x00]);
        dib.extend_from_slice(&[0x00, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0x00, 0x00]);
        let (rgba, w, h) = dib_to_rgba(&dib).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(
            rgba,
            vec![
                255, 0, 0, 255, // 红
                0, 255, 0, 255, // 绿
                0, 0, 255, 255, // 蓝
                255, 255, 255, 255, // 白
            ]
        );
    }

    #[test]
    fn dib_24bpp_top_down_keeps_rows_as_written() {
        let mut dib = dib_header(2, -2, 24, 0);
        dib.extend_from_slice(&[0x00, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0x00, 0x00]);
        dib.extend_from_slice(&[0xFF, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0x00, 0x00]);
        let (rgba, _, _) = dib_to_rgba(&dib).unwrap();
        assert_eq!(&rgba[0..8], &[255, 0, 0, 255, 0, 255, 0, 255]);
    }

    #[test]
    fn dib_32bpp_all_zero_alpha_is_treated_opaque() {
        let mut dib = dib_header(1, 1, 32, 0);
        dib.extend_from_slice(&[0x10, 0x20, 0x30, 0x00]);
        let (rgba, _, _) = dib_to_rgba(&dib).unwrap();
        assert_eq!(rgba, vec![0x30, 0x20, 0x10, 255]);
    }

    #[test]
    fn dib_bitfields_masks_map_channels() {
        let mut dib = dib_header(1, 1, 32, 3);
        dib.extend_from_slice(&0x00FF_0000u32.to_le_bytes()); // R
        dib.extend_from_slice(&0x0000_FF00u32.to_le_bytes()); // G
        dib.extend_from_slice(&0x0000_00FFu32.to_le_bytes()); // B
                                                              // 小端 32 位：低字节是 B。
        dib.extend_from_slice(&[0x10, 0x20, 0x30, 0x00]);
        let (rgba, _, _) = dib_to_rgba(&dib).unwrap();
        assert_eq!(rgba, vec![0x30, 0x20, 0x10, 255]);
    }

    #[test]
    fn dib_rejects_compressed_truncated_and_odd_depths() {
        let mut rle = dib_header(2, 2, 8, 1);
        rle.extend_from_slice(&[0, 0]);
        assert!(dib_to_rgba(&rle).is_err(), "RLE 不支持");

        let short = dib_header(4, 4, 24, 0);
        assert!(dib_to_rgba(&short).is_err(), "缺像素数据");

        let dib16 = dib_header(1, 1, 16, 0);
        assert!(dib_to_rgba(&dib16).is_err(), "16 bpp 不支持");
    }

    #[test]
    fn decode_png_loose_accepts_rgb_without_alpha() {
        let mut png = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png, 2, 1);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[255, 0, 0, 0, 255, 0]).unwrap();
        }
        let (rgba, w, h) = decode_png_rgba_loose(&png).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(rgba, vec![255, 0, 0, 255, 0, 255, 0, 255]);
    }

    #[test]
    fn thumb_is_bounded_and_averages_source_blocks() {
        // 4×2：左两列 100、右两列 200 ⇒ 缩到 2×1 后应是 100 / 200。
        let mut rgba = Vec::new();
        for _y in 0..2 {
            for x in 0..4 {
                let v = if x < 2 { 100 } else { 200 };
                rgba.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let (thumb, w, h) = make_thumb(&rgba, 4, 2, 2).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(thumb, vec![100, 100, 100, 255, 200, 200, 200, 255]);

        let (same, w, h) = make_thumb(&rgba, 4, 2, 160).unwrap();
        assert_eq!((w, h), (4, 2), "小图不放大");
        assert_eq!(same, rgba);
    }

    #[test]
    fn image_hash_depends_on_dimensions_and_pixels() {
        let a = image_hash(&[1, 2, 3, 4, 5, 6, 7, 8], 2, 1);
        let b = image_hash(&[1, 2, 3, 4, 5, 6, 7, 8], 1, 2);
        let c = image_hash(&[8, 7, 6, 5, 4, 3, 2, 1], 2, 1);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(a, image_hash(&[1, 2, 3, 4, 5, 6, 7, 8], 2, 1));
    }

    fn hdrop(paths: &[&str], wide: bool) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&20u32.to_le_bytes());
        out.extend_from_slice(&0i32.to_le_bytes());
        out.extend_from_slice(&0i32.to_le_bytes());
        out.extend_from_slice(&0i32.to_le_bytes());
        out.extend_from_slice(&(if wide { 1i32 } else { 0i32 }).to_le_bytes());
        for p in paths {
            if wide {
                for u in p.encode_utf16() {
                    out.extend_from_slice(&u.to_le_bytes());
                }
                out.extend_from_slice(&0u16.to_le_bytes());
            } else {
                out.extend_from_slice(p.as_bytes());
                out.push(0);
            }
        }
        if wide {
            out.extend_from_slice(&0u16.to_le_bytes());
        } else {
            out.push(0);
        }
        out
    }

    #[test]
    fn parse_hdrop_reads_wide_and_ansi_lists() {
        assert_eq!(
            parse_hdrop(&hdrop(&["C:\\a.txt", "D:\\b.txt"], true)),
            vec!["C:\\a.txt".to_string(), "D:\\b.txt".to_string()]
        );
        assert_eq!(
            parse_hdrop(&hdrop(&["C:\\a.txt"], false)),
            vec!["C:\\a.txt".to_string()]
        );
        assert!(parse_hdrop(&[]).is_empty());
        assert!(parse_hdrop(&[0u8; 8]).is_empty());
    }

    #[test]
    fn files_verdict_dedups_and_enforces_limits() {
        let got = files_verdict(
            &[
                "C:\\a.txt".to_string(),
                "c:\\A.TXT".to_string(),
                "  ".to_string(),
                "D:\\b.txt".to_string(),
            ],
            MAX_FILES_PER_ITEM,
        );
        assert_eq!(
            got,
            FilesVerdict::Ok(vec!["C:\\a.txt".to_string(), "D:\\b.txt".to_string()]),
            "空串与非 Windows 大小写不敏感的重复项都该去掉"
        );
        assert_eq!(files_verdict(&[], MAX_FILES_PER_ITEM), FilesVerdict::Empty);
        assert_eq!(
            files_verdict(&["a".into(), "b".into()], 1),
            FilesVerdict::TooMany
        );
    }

    #[test]
    fn files_label_names_three_and_elides_the_rest() {
        let two = files_label(&["C:\\a.png".into(), "D:\\b.txt".into()]);
        assert_eq!(two, "2 个文件：a.png, b.txt");
        let four = files_label(&[
            "C:\\a.png".into(),
            "D:\\b.txt".into(),
            "E:\\c.pdf".into(),
            "F:\\d.doc".into(),
        ]);
        assert_eq!(four, "4 个文件：a.png, b.txt, c.pdf …");
    }

    #[test]
    fn image_rows_store_thumb_and_delete_removes_files() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("clip_img");
        init_db().unwrap();

        let rgba = [
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];
        let stored = store_image(&rgba, 2, 2).unwrap();
        assert!(stored.png_path.exists(), "原图应落盘");
        let thumb = stored.thumb_path.clone().expect("缩略图应生成");
        assert!(thumb.exists());

        let IngestOutcome::Stored { id, dedup } =
            ingest_image(&stored, "paint.exe", now_ms()).unwrap()
        else {
            panic!("图片应当入库");
        };
        assert!(!dedup);
        let item = get(id).unwrap().unwrap();
        assert_eq!(item.kind, ClipKind::Image);
        assert_eq!((item.width, item.height), (2, 2));
        assert_eq!(item.content, "图片 2 × 2");
        assert_eq!(item.byte_len, stored.png_bytes as i64);

        // 同一张图再入库：只置顶不新增。
        let again = ingest_image(&stored, "paint.exe", now_ms() + 1).unwrap();
        assert!(matches!(again, IngestOutcome::Stored { dedup: true, .. }));

        delete(id).unwrap();
        assert!(!stored.png_path.exists(), "删行时原图应一起删掉");
        assert!(!thumb.exists(), "删行时缩略图应一起删掉");
    }

    #[test]
    fn files_row_shows_label_and_copy_round_trips_paths() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("clip_files");
        init_db().unwrap();
        let IngestOutcome::Stored { id, .. } = ingest_files(
            &["C:\\gone-a.txt".into(), "C:\\gone-b.txt".into()],
            "explorer.exe",
            now_ms(),
        )
        .unwrap() else {
            panic!("文件条目应当入库");
        };
        let item = get(id).unwrap().unwrap();
        assert_eq!(item.kind, ClipKind::File);
        assert_eq!(item.file_paths.len(), 2);
        assert_eq!(item.content, "2 个文件：gone-a.txt, gone-b.txt");
        // 两个路径都不存在：复制应退成文本（这里只验证退路被选中，不碰真实剪贴板）。
        let (alive, gone) = split_alive_paths(&item.file_paths);
        assert!(alive.is_empty());
        assert_eq!(gone.len(), 2);
    }

    #[test]
    fn trim_deletes_files_of_rows_it_drops() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("clip_trim_img");
        init_db().unwrap();
        crate::config::instance()
            .set("clipboard", "max_items", "1")
            .unwrap();

        let older = store_image(&[255, 0, 0, 255], 1, 1).unwrap();
        let IngestOutcome::Stored { id: old_id, .. } =
            ingest_image(&older, "paint.exe", now_ms() - 10_000).unwrap()
        else {
            panic!("第一张应当入库");
        };
        assert!(older.png_path.exists());

        // 第二条入库时 trim 只留一条：旧的那一行与它的文件都该消失。
        let newer = store_image(&[0, 255, 0, 255], 1, 1).unwrap();
        ingest_image(&newer, "paint.exe", now_ms()).unwrap();
        assert!(get(old_id).unwrap().is_none(), "超出条数的那一行应被裁掉");
        assert!(!older.png_path.exists(), "被裁掉那一行的原图应一起删掉");
        if let Some(thumb) = &older.thumb_path {
            assert!(!thumb.exists(), "被裁掉那一行的缩略图应一起删掉");
        }
        assert!(newer.png_path.exists(), "留下的那一行不该被误删");

        crate::config::instance()
            .set("clipboard", "max_items", "500")
            .unwrap();
    }

    #[test]
    fn migration_adds_v12_columns_to_old_table() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("clip_migrate");
        {
            let conn = open().unwrap();
            conn.execute_batch(
                "CREATE TABLE clipboard_items (
                    id         INTEGER PRIMARY KEY AUTOINCREMENT,
                    content    TEXT    NOT NULL,
                    hash       TEXT    NOT NULL,
                    source_app TEXT    NOT NULL DEFAULT '',
                    copied_at  INTEGER NOT NULL,
                    pinned     INTEGER NOT NULL DEFAULT 0,
                    byte_len   INTEGER NOT NULL
                );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO clipboard_items(content, hash, source_app, copied_at, pinned, byte_len)
                 VALUES ('旧文本', 'h1', '', 1, 0, 9)",
                [],
            )
            .unwrap();
        }
        init_db().unwrap();
        let item = get(1).unwrap().unwrap();
        assert_eq!(item.kind, ClipKind::Text);
        assert!(item.image_path.is_none());
        assert!(item.file_paths.is_empty());
        assert_eq!((item.width, item.height), (0, 0));
        // 再跑一遍：补列幂等，不该报 duplicate column。
        init_db().unwrap();
    }
}
