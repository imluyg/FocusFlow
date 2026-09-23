//! Edge 浏览器历史记录模块。
//!
//! 镜像 Python 版 `edge_history.py`：
//! - 直读 Edge History SQLite（%LOCALAPPDATA%\Microsoft\Edge\User Data\Default\History）
//! - Chrome 时间戳（1601-01-01 起微秒）转换
//! - 只读连接优先，被锁时复制 WAL 兜底
//! - 本地存储每日计数（focusflow_edge_history.db）供趋势图

use std::path::PathBuf;

use chrono::{Local, NaiveDate, TimeZone, Utc};
use rusqlite::Connection;

use crate::paths;

/// Edge History 数据库路径。
pub fn edge_history_path() -> PathBuf {
    // 单测把"Edge 的库"指向自己造的夹具：真机上的 History 既不确定（他随时在浏览），
    // 也不该被测试读走一份副本。
    #[cfg(test)]
    if let Some(p) = EDGE_PATH_OVERRIDE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        return p;
    }
    if let Ok(local_app) = std::env::var("LOCALAPPDATA") {
        PathBuf::from(local_app).join(r"Microsoft\Edge\User Data\Default\History")
    } else {
        PathBuf::from(r"C:\Users\Default\AppData\Local\Microsoft\Edge\User Data\Default\History")
    }
}

/// 测试夹具路径（进程级，靠 `test_app_dir_lock` 串行化）。
#[cfg(test)]
static EDGE_PATH_OVERRIDE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// 本进程进过 `with_edge_snapshot` 几次 —— 补档那条路要求"一批查询只走一轮"，
/// 光看代码结构断言不住，只能数。
#[cfg(test)]
static SNAPSHOT_ROUNDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Chrome 时间戳（1601-01-01 起微秒）转 datetime。
#[allow(dead_code)]
fn chrome_to_datetime(chrome_time: i64) -> chrono::DateTime<Utc> {
    let epoch = Utc.with_ymd_and_hms(1601, 1, 1, 0, 0, 0).unwrap();
    epoch + chrono::Duration::microseconds(chrome_time)
}

/// datetime 转 Chrome 时间戳。
fn datetime_to_chrome(dt: &chrono::DateTime<Local>) -> i64 {
    let epoch = Utc.with_ymd_and_hms(1601, 1, 1, 0, 0, 0).unwrap();
    (dt.with_timezone(&Utc) - epoch)
        .num_microseconds()
        .unwrap_or(0)
}

/// 打开 Edge History（只读优先，锁定则复制）。
/// 复制前大小保护阈值：Edge 历史库超过该大小（字节）时不再复制
/// （复制几百 MB 会卡顿，直接返回错误提示）。
const EDGE_COPY_MAX_BYTES: u64 = 100 * 1024 * 1024; // 100MB

/// 直连 busy 等待上限：Edge 运行时会频繁短事务持排他锁，
/// 等待过短会让查询在锁窗口内失败并静默返回 0；过长则批量查询时逐个干等。
/// 被锁时 300ms 后自动转复制兜底（毫秒级），整体感知最快。
const EDGE_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);

/// 复制兜底用的临时副本路径：放系统临时目录 + 随机文件名。
/// 副本包含 Edge 完整浏览记录（URL/标题/时间），不落在程序目录，
/// 且随机名 + 短生命周期把崩溃残留的风险窗口压到最低。
fn temp_copy_path() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "{TEMP_COPY_PREFIX}{}_{nanos}.db",
        std::process::id()
    ))
}

/// 删除副本及其 WAL/SHM 附属文件。
fn remove_temp_copy(temp: &std::path::Path) {
    let _ = std::fs::remove_file(temp);
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{}", temp.display(), suffix));
    }
}

/// 副本名里稳定的前缀，供启动时清扫识别。
const TEMP_COPY_PREFIX: &str = "focusflow_edge_";

/// 清扫 `%TEMP%` 里残留副本的年龄门槛。
///
/// 一份副本只在 `with_edge_snapshot` 兜底期间存在（毫秒到秒级），留够一个数量级
/// 的余量就不会碰到另一个实例正在用的文件；门槛再短就成了"两个实例互删"。
const STALE_TEMP_COPY_AGE: std::time::Duration = std::time::Duration::from_secs(3600);

/// 删除目录里老于 `max_age` 的 Edge 副本（含 `-wal`/`-shm`），返回删掉的个数。
///
/// 为什么需要这一条：副本里是**明文浏览记录**，正常路径每次都 `remove_temp_copy`，
/// 但 release 是 `panic = "abort"`、被任务管理器直接杀也来不及删 —— 那份完整的
/// History 就永久躺在 `%TEMP%`，谁都能读。`dir` 参数是为测试留的：单测扫自己的
/// 临时目录，不去碰系统 `%TEMP%`。
fn sweep_stale_temp_copies_in(dir: &std::path::Path, max_age: std::time::Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let now = std::time::SystemTime::now();
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(TEMP_COPY_PREFIX) || !name.contains(".db") {
            continue;
        }
        // mtime 读不出来就留着：宁可少删，也不要把一个状态不明的文件删掉
        let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        // 时钟回拨/未来 mtime → 当作 0 龄，不删
        let age = now
            .duration_since(mtime)
            .unwrap_or(std::time::Duration::ZERO);
        if age > max_age && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// 旧版本曾把副本放在程序目录 `data/_edge_history_temp.db`，
/// 崩溃时会残留明文浏览记录；顺手扫 `%TEMP%` 里本模块留下的副本。
/// 一个进程只跑一次，所以放在每次快照读取的入口，而不是等到真要复制时才清。
fn cleanup_stale_copies_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let legacy = paths::data_dir().join("_edge_history_temp.db");
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", legacy.display(), suffix));
        }
        let removed = sweep_stale_temp_copies_in(&std::env::temp_dir(), STALE_TEMP_COPY_AGE);
        if removed > 0 {
            // 说清"为什么会有"，否则用户只看到一句"我删了你临时目录里的东西"
            tracing::warn!(
                "清理了 {removed} 份上一次异常退出残留的 Edge 历史副本（位于系统临时目录，含明文浏览记录）"
            );
        }
    });
}

/// 在一份 Edge History 快照上执行一批查询，返回 Option（None 表示读取失败/被锁）。
///
/// 策略：
/// 1) 只读直连（busy_timeout 300ms）：Edge 未运行或锁间隙时最快。
/// 2) 查询失败（被锁）→ 复制主文件兜底重试；复制是整文件快照，
///    可能撞上 Edge 写事务产生撕裂快照，用少量重试覆盖。
///    主文件在回滚日志模式下含全部已提交数据；WAL 模式下附带复制 -wal/-shm。
///
/// 之所以是「一批查询」而不是「每个查询各走一遍」：刷新一次要同时取今日数与
/// 总数，若各自兜底，被锁时最坏要走两轮 300ms busy 等 + 两轮各 3 次 ≤100MB 复制。
/// 共用快照后只剩一轮，且两个数取自同一时点，不会出现「总数比昨天小」。
fn with_edge_snapshot<T>(f: impl Fn(&Connection) -> Option<T>) -> Option<T> {
    #[cfg(test)]
    SNAPSHOT_ROUNDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    cleanup_stale_copies_once();
    let path = edge_history_path();
    if !path.exists() {
        return None;
    }

    // 1) 只读直连：rusqlite 打开不一定失败（锁在首个查询才报 SQLITE_BUSY），
    //    必须实际验证查询可用才采用，否则 Edge 运行时直连会静默返回 0。
    if let Ok(conn) = Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        let _ = conn.busy_timeout(EDGE_BUSY_TIMEOUT);
        if let Some(v) = f(&conn) {
            return Some(v);
        }
    }

    // 2) 复制兜底：先检查大小，超大库跳过复制避免卡顿
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if size > EDGE_COPY_MAX_BYTES {
        tracing::warn!(
            "Edge 历史库过大（{}MB），跳过复制（只读直连被锁定）",
            size / (1024 * 1024)
        );
        return None;
    }
    let temp = temp_copy_path();
    for attempt in 0..3 {
        if std::fs::copy(&path, &temp).is_ok() {
            for suffix in ["-wal", "-shm"] {
                let src = format!("{}{}", path.display(), suffix);
                if std::path::Path::new(&src).exists() {
                    let _ = std::fs::copy(&src, format!("{}{}", temp.display(), suffix));
                }
            }
            if let Ok(conn) = Connection::open(&temp) {
                if let Some(v) = f(&conn) {
                    drop(conn);
                    remove_temp_copy(&temp);
                    return Some(v);
                }
            }
        }
        remove_temp_copy(&temp);
        tracing::debug!("Edge 历史库复制查询失败（第{}次），重试", attempt + 1);
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    tracing::warn!("Edge 历史库无法读取（直连被锁且复制失败）");
    None
}

/// 在**一份**快照上逐日取计数（`days` 通常是一批缺失的历史日期）。
///
/// 返回 `None` 表示这条快照根本不可用（被锁 / 没有 `urls` 表），调用方
/// （`with_edge_snapshot`）据此走复制兜底；已经开始有结果后中途失败则返回
/// 已取到的那部分 —— 半批也比全弃强，剩下的日期下一轮还会被挑出来。
///
/// 替代了原来的 `query_edge_history_count(day)`：那个形状让每个缺失日各走一遍
/// 快照（各一次 300ms busy 等待 + 最多 3 份 ≤100MB 整文件复制）。
fn counts_on_snapshot(conn: &Connection, days: &[NaiveDate]) -> Option<Vec<(NaiveDate, i64)>> {
    let mut out = Vec::with_capacity(days.len());
    for day in days {
        let Some((chrome_start, chrome_end)) = chrome_day_range(*day) else {
            continue;
        };
        match conn.query_row(
            "SELECT COUNT(*) FROM urls WHERE last_visit_time >= ?1 AND last_visit_time < ?2",
            rusqlite::params![chrome_start, chrome_end],
            |r| r.get::<_, i64>(0),
        ) {
            Ok(count) => out.push((*day, count)),
            Err(e) => {
                if out.is_empty() {
                    tracing::debug!("Edge 历史批量补档查询失败，转复制兜底: {e}");
                    return None;
                }
                tracing::debug!(
                    "Edge 历史批量补档中途失败，先收下已取到的 {} 天: {e}",
                    out.len()
                );
                break;
            }
        }
    }
    Some(out)
}

/// 某天 [00:00, 次日 00:00) 对应的 Chrome 微秒区间。
///
/// 右端点取**次日的本地零点**，不是"本日零点 + 24 小时"：有夏令时的日子真实长度是
/// 23 或 25 小时，固定加 24 小时会让相邻两天的区间互相重叠（同一条记录计进两天）或
/// 留出空洞（某天最后一小时谁都不算）。
fn chrome_day_range(target_date: NaiveDate) -> Option<(i64, i64)> {
    let day_start = local_midnight(target_date)?;
    let day_end = local_midnight(target_date.succ_opt()?)?;
    Some((datetime_to_chrome(&day_start), datetime_to_chrome(&day_end)))
}

/// 某日本地零点。DST 跳变的空档里该时刻不存在，回退取该日最早可用映射。
fn local_midnight(day: NaiveDate) -> Option<chrono::DateTime<Local>> {
    let naive = day.and_hms_opt(0, 0, 0)?;
    match Local.from_local_datetime(&naive).single() {
        Some(dt) => Some(dt),
        None => Local.from_local_datetime(&naive).earliest(),
    }
}

pub fn edge_db_path() -> PathBuf {
    paths::data_dir().join("focusflow_edge_history.db")
}

/// 打开本地 Edge 计数库：busy_timeout 防并发短锁导致读写直接失败。
fn open_local() -> rusqlite::Result<Connection> {
    std::fs::create_dir_all(paths::data_dir()).ok();
    let conn = Connection::open(edge_db_path())?;
    conn.busy_timeout(std::time::Duration::from_secs(15)).ok();
    Ok(conn)
}

/// 保存指定日期的计数到本地。
pub fn save_edge_history_count(target_date: NaiveDate, count: i64) {
    let conn = match open_local() {
        Ok(c) => c,
        Err(_) => return,
    };
    let _ = conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS edge_history (
            date TEXT PRIMARY KEY,
            count INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );",
    );
    let _ = conn.execute(
        "INSERT OR REPLACE INTO edge_history (date, count, updated_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![
            target_date.format("%Y-%m-%d").to_string(),
            count,
            Utc::now().timestamp()
        ],
    );
}

/// 获取近 N 天 Edge 历史计数。
pub fn get_edge_history_counts(days: i64) -> Vec<(String, i64)> {
    let path = edge_db_path();
    if !path.exists() {
        return Vec::new();
    }
    let conn = match open_local() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let start = (Local::now().date_naive() - chrono::Days::new((days - 1).max(0) as u64))
        .format("%Y-%m-%d")
        .to_string();
    let result = conn
        .prepare("SELECT date, count FROM edge_history WHERE date >= ?1 ORDER BY date")
        .and_then(|mut stmt| {
            stmt.query_map([&start], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .map(|it| it.flatten().collect())
        });
    result.unwrap_or_default()
}

/// 刷新状态（`RefreshSlot::state` 的取值）。
const ST_IDLE: u8 = 0;
const ST_RUNNING: u8 = 1;
const ST_OK: u8 = 2;
const ST_FAIL: u8 = 3;

/// RUNNING 超过这个时长就认定为卡死。
///
/// 阈值刻意放宽到 120 秒：一轮正常刷新最坏也只是 300ms busy 等待 + 3 轮 ≤100MB
/// 整文件复制，慢盘上十几秒也走得完，这里留了一个数量级的余量。取短了会招来
/// 更糟的后果 —— 接管会与仍在正常跑的轮次并发，两个线程各复制一份 100MB。
const RUNNING_STALE_MS: i64 = 120_000;

/// 刷新槽位：状态 + 本轮世代号 + 进入 RUNNING 的时刻。
///
/// 世代号是给「被接管的那一轮」准备的：卡死的线程哪天真的从 `fs::copy` 里回来，
/// 也不该由它把新一轮的状态改写成 ok/fail。
#[derive(Default)]
struct RefreshSlot {
    state: u8,
    gen: u64,
    started_at_ms: i64,
}

static REFRESH_SLOT: std::sync::Mutex<RefreshSlot> = std::sync::Mutex::new(RefreshSlot {
    state: ST_IDLE,
    gen: 0,
    started_at_ms: 0,
});

fn lock_slot() -> std::sync::MutexGuard<'static, RefreshSlot> {
    REFRESH_SLOT.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// 一轮刷新是否已经拖过了头。
fn is_wedged(slot: &RefreshSlot, now: i64) -> bool {
    slot.state == ST_RUNNING
        && slot.started_at_ms > 0
        && now.saturating_sub(slot.started_at_ms) >= RUNNING_STALE_MS
}

/// 非阻塞启动一次「今日 + 总数」刷新，返回是否真的启动了任务。
///
/// 为什么必须异步：宿主 Lua 插件跑在**主线程**（Lua 状态机不是 `Send`，只能在
/// 主线程操作，见 `desktop/src/plugins.rs`），而一次同步刷新最坏要等 300ms busy
/// 超时、再走三轮 ≤100MB 的整文件复制 —— 用户点「刷新数据」会把整个界面冻住。
///
/// 已有一轮在跑时不再排队（重复点击不该放大复制开销），直接返回 `false`；
/// 数值写进本地缓存库，插件下次渲染用 `get_edge_history_saved_*` 取。
pub fn spawn_update_today() -> bool {
    let Some(gen) = claim_round(now_ms()) else {
        return false;
    };
    let spawned = std::thread::Builder::new()
        .name("edge-refresh".into())
        .spawn(move || {
            let ok = update_today_edge_history().0;
            finish_round(gen, ok);
        });
    match spawned {
        Ok(_) => true,
        Err(e) => {
            tracing::error!("启动 Edge 历史刷新线程失败: {e}");
            release_round(gen);
            false
        }
    }
}

/// 尝试占用「本轮刷新」这个槽位：成功返回本轮世代号，`None` 表示应当拒绝。
///
/// 拒绝的唯一理由是上一轮还在正常跑；它一旦超过 [`RUNNING_STALE_MS`] 仍没收尾就
/// 按卡死处理并接管 —— 不然线程回不来时，用户点到重启为止都不会再有任何反应。
/// 世代号用来让被接管掉的那一轮之后再返回也改不动新一轮的状态。
fn claim_round(now: i64) -> Option<u64> {
    let mut slot = lock_slot();
    let wedged = is_wedged(&slot, now);
    if slot.state == ST_RUNNING && !wedged {
        return None;
    }
    if wedged {
        tracing::warn!(
            "Edge 刷新线程超过 {} 秒未收尾，按卡死接管并启动新一轮",
            RUNNING_STALE_MS / 1000
        );
    }
    slot.state = ST_RUNNING;
    slot.started_at_ms = now;
    slot.gen += 1;
    Some(slot.gen)
}

/// 收尾：只有「本轮仍是当前这一轮」时才允许写回结果。
fn finish_round(gen: u64, ok: bool) {
    let mut slot = lock_slot();
    if slot.gen != gen {
        return;
    }
    slot.state = if ok { ST_OK } else { ST_FAIL };
    slot.started_at_ms = 0;
}

/// 线程根本没起来时的回滚：本轮没人在跑，别把状态留在 RUNNING 卡住用户。
fn release_round(gen: u64) {
    let mut slot = lock_slot();
    if slot.gen != gen {
        return;
    }
    slot.state = ST_IDLE;
    slot.started_at_ms = 0;
}

/// 一次后台刷新的状态：`idle` / `running` / `ok` / `fail`。
///
/// 读的时候顺手自愈：光靠 `spawn_update_today` 里的接管救不了刷新按钮 ——
/// 线程卡死后插件会一直显示「正在后台读取」，用户连再点一次的入口都没有。
pub fn refresh_state() -> &'static str {
    let now = now_ms();
    let mut slot = lock_slot();
    if is_wedged(&slot, now) {
        tracing::error!(
            "Edge 刷新线程已卡死（超过 {} 秒未收尾），状态已复位，可重新点刷新",
            RUNNING_STALE_MS / 1000
        );
        slot.state = ST_FAIL;
        slot.started_at_ms = 0;
        // 推进世代号，让那个再也不会收尾的线程之后没有改写的余地
        slot.gen += 1;
    }
    match slot.state {
        ST_RUNNING => "running",
        ST_OK => "ok",
        ST_FAIL => "fail",
        _ => "idle",
    }
}

/// 补档往回看多少天（与趋势表的口径一致）。
const BACKFILL_DAYS: i64 = 30;

/// 更新今天并返回 (是否成功, 今日数, 总数)。
/// 任一步失败（Edge 库被锁/不可读）返回 (false, 0, 0)，调用方据此提示用户，
/// 避免把失败静默当成"0 条记录"。同一次快照顺手补齐近 30 天缺失的历史计数。
///
/// 调用方应当用 [`spawn_update_today`] 而不是直接调本函数（本函数会阻塞调用线程）。
pub fn update_today_edge_history() -> (bool, i64, i64) {
    let today = Local::now().date_naive();
    let Some((chrome_start, chrome_end)) = chrome_day_range(today) else {
        return (false, 0, 0);
    };
    // 要补哪些天必须在开快照**之前**定好：这样今日数、总数和整批补档共用同一轮
    // 快照，被锁时也只兜底复制一份副本，且所有数字取自同一时点。
    let missing = missing_days_before(today, BACKFILL_DAYS);
    let pending = missing.len();
    let batch = with_edge_snapshot(move |conn| {
        let day = conn
            .query_row(
                "SELECT COUNT(*) FROM urls WHERE last_visit_time >= ?1 AND last_visit_time < ?2",
                rusqlite::params![chrome_start, chrome_end],
                |r| r.get::<_, i64>(0),
            )
            .ok()?;
        let total = conn
            .query_row("SELECT COUNT(*) FROM urls", [], |r| r.get::<_, i64>(0))
            .ok()?;
        // 补档一条查不动时不该把已经到手的今日数/总数一起丢掉：退化成"这轮不补"
        let filled = counts_on_snapshot(conn, &missing).unwrap_or_default();
        Some((day, total, filled))
    });
    match batch {
        Some((today_count, total, filled)) => {
            save_edge_history_count(today, today_count);
            save_edge_history_meta("total", total);
            if !filled.is_empty() {
                for (day, count) in &filled {
                    save_edge_history_count(*day, *count);
                }
                tracing::info!(
                    "Edge 历史已补齐 {} 天缺失记录（{} 天待补，与今日数同一份快照）",
                    filled.len(),
                    pending
                );
            }
            (true, today_count, total)
        }
        _ => (false, 0, 0),
    }
}

/// 近 `days` 天里本地缓存库还没有记录的日子（不含 `today`，那条由刷新主路写）。
fn missing_days_before(today: NaiveDate, days: i64) -> Vec<NaiveDate> {
    let start = today - chrono::Days::new((days - 1).max(0) as u64);
    let existing = saved_dates_since(start);
    let mut out = Vec::new();
    let mut day = start;
    while day < today {
        if !existing.contains(&day.format("%Y-%m-%d").to_string()) {
            out.push(day);
        }
        day = day + chrono::Days::new(1);
    }
    out
}

/// 本地缓存库里 `start` 之后（含）已有记录的日子集合。
fn saved_dates_since(start: NaiveDate) -> std::collections::HashSet<String> {
    open_local()
        .ok()
        .and_then(|conn| {
            conn.prepare("SELECT date FROM edge_history WHERE date >= ?1")
                .ok()
                .and_then(|mut stmt| {
                    stmt.query_map([start.format("%Y-%m-%d").to_string()], |r| {
                        r.get::<_, String>(0)
                    })
                    .ok()
                    .map(|it| it.flatten().collect())
                })
        })
        .unwrap_or_default()
}

/// 保存上次刷新的数值（meta 表），插件重启后恢复显示，避免出现误导性的 "—" / 0。
fn save_edge_history_meta(key: &str, value: i64) {
    let conn = match open_local() {
        Ok(c) => c,
        Err(_) => return,
    };
    let _ = conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    );
    let _ = conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
        rusqlite::params![key, value.to_string()],
    );
}

/// 读取本地缓存库中某一天已保存的计数（没刷新过那一天返回 None）。
fn saved_count_on(target_date: NaiveDate) -> Option<i64> {
    if !edge_db_path().exists() {
        return None;
    }
    let conn = open_local().ok()?;
    conn.query_row(
        "SELECT count FROM edge_history WHERE date = ?1",
        [target_date.format("%Y-%m-%d").to_string()],
        |r| r.get::<_, i64>(0),
    )
    .ok()
}

/// 读取今天的 Edge 记录数（本地缓存，今天没刷新过返回 None）。
///
/// 按日期查 `edge_history` 表，而不是读 meta 里那个"上次刷新时当作今天"的值：
/// meta 不会自己跨零点更新，应用通宵开着的话，第二天早上就会把昨天的数当成
/// "今日记录数"显示给用户。
pub fn get_edge_history_saved_today() -> Option<i64> {
    saved_count_on(Local::now().date_naive())
}

/// 读取上次保存的总记录数（本地缓存，未刷新过返回 None）。
pub fn get_edge_history_saved_total() -> Option<i64> {
    let conn = open_local().ok()?;
    conn.query_row("SELECT value FROM meta WHERE key='total'", [], |r| {
        r.get::<_, String>(0)
    })
    .ok()
    .and_then(|v| v.parse().ok())
}

/// 本地趋势（近 N 天），供插件展示。
pub fn trend_counts(days: i64) -> Vec<(String, i64)> {
    get_edge_history_counts(days)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把"Edge 的库"指向夹具文件，析构时还原（panic 时也不会留给下一个用例）。
    struct EdgeFixture;
    impl Drop for EdgeFixture {
        fn drop(&mut self) {
            *EDGE_PATH_OVERRIDE.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    /// 造一份 Edge 形状的 History 库（列名对齐 Chrome 的 `urls` 表）。
    fn write_edge_fixture(tag: &str, last_visit_times: &[i64]) -> EdgeFixture {
        let path = paths::data_dir().join(format!("{tag}_edge_history.db"));
        std::fs::create_dir_all(path.parent().unwrap()).ok();
        let conn = Connection::open(&path).expect("夹具库应能创建");
        conn.execute_batch(
            "CREATE TABLE urls (
                id INTEGER PRIMARY KEY,
                url TEXT NOT NULL,
                title TEXT,
                visit_count INTEGER,
                last_visit_time INTEGER NOT NULL
            );",
        )
        .unwrap();
        for t in last_visit_times {
            conn.execute(
                "INSERT INTO urls (url, title, visit_count, last_visit_time) VALUES (?1,'t',1,?2)",
                rusqlite::params![format!("https://example.com/{t}"), t],
            )
            .unwrap();
        }
        drop(conn);
        *EDGE_PATH_OVERRIDE.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
        EdgeFixture
    }

    /// 某天 `[00:00, 次日 00:00)` 起点的 Chrome 微秒值。
    fn chrome_start_of(day: NaiveDate) -> i64 {
        chrome_day_range(day).unwrap().0
    }

    /// Chrome 纪元换算的独立校验：不借用模块自己的 `chrome_day_range`，直接对
    /// 一个手算的常量。夹具是拿同一个函数造的，只有这条能证明纪元没歪。
    ///
    /// 1601-01-01→1970-01-01 是 134774 天 = 11_644_473_600 秒；
    /// 2020-01-01T00:00:00Z 的 Unix 秒 1_577_836_800 → 微秒 13_222_310_400_000_000。
    #[test]
    fn chrome_epoch_matches_a_hand_computed_instant() {
        let dt = Utc
            .timestamp_opt(1_577_836_800, 0)
            .single()
            .expect("合法 Unix 秒")
            .with_timezone(&Local);
        assert_eq!(datetime_to_chrome(&dt), 13_222_310_400_000_000);
    }

    /// 一天的窗口上界是**次日零点**而非"本日 + 24 小时"：零点整那条属于下一天。
    #[test]
    fn snapshot_counts_use_half_open_local_days() {
        let _lock = crate::paths::test_app_dir_lock();
        let _app = crate::paths::test_app_dir("edge_half_open");
        let today = Local::now().date_naive();
        let y = today - chrono::Days::new(1);
        let hour = 3_600 * 1_000_000;
        let rows = [
            chrome_start_of(y),            // y 的零点：属于 y
            chrome_start_of(y) + hour,     // y 的上午：属于 y
            chrome_start_of(today),        // 今天的零点：不该算进 y
            chrome_start_of(today) + hour, // 今天
        ];
        let _fx = write_edge_fixture("halfopen", &rows);
        let conn = Connection::open(edge_history_path()).unwrap();
        let got = counts_on_snapshot(&conn, &[y, today]).unwrap();
        assert_eq!(got, vec![(y, 2), (today, 2)], "零点整那条必须落到下一天");
    }

    /// 相邻两天的区间必须严丝合缝地拼起来：既不重叠也不留空洞。
    ///
    /// 注：这条不变量在 UTC+8（本机）拿新旧实现都验不出差别 —— 旧实现是
    /// "本日零点 + 24 小时"，只有真实日长不等于 24 小时的那天（有夏令时的地区）
    /// 才会错开。写成断言是为了让它在有 DST 的机器上真的把关，而不是装成已复现。
    #[test]
    fn day_windows_tile_exactly() {
        let today = Local::now().date_naive();
        for offset in 0..400 {
            let d = today - chrono::Days::new(offset);
            let Some(next) = d.succ_opt() else { continue };
            let (_, end) = chrome_day_range(d).unwrap();
            let (next_start, _) = chrome_day_range(next).unwrap();
            assert_eq!(end, next_start, "{d} 的右端点应等于次日的左端点");
            let (start, _) = chrome_day_range(d).unwrap();
            assert!(start < end, "{d} 的窗口不该为空或反向");
        }
    }

    /// **一轮刷新只走一份快照**：今日数、总数、以及 29 天补档全部在同一次
    /// `with_edge_snapshot` 里查完，且在函数返回时就已写进本地库。
    ///
    /// 旧实现是 `spawn` 一个游离线程、对每个缺失日各调一次 `query_edge_history_count`，
    /// 也就是最坏 30 轮"300ms busy 等待 + 3 份 ≤100MB 整文件复制"，还不受
    /// `REFRESH_SLOT` 跟踪。把这条注回旧形状，本用例的 `== 1` 立刻变 `== 2`。
    #[test]
    fn one_refresh_backfills_every_missing_day_in_a_single_snapshot() {
        let _lock = crate::paths::test_app_dir_lock();
        let _app = crate::paths::test_app_dir("edge_backfill");
        let today = Local::now().date_naive();
        let y = today - chrono::Days::new(1);
        let two = today - chrono::Days::new(2);
        // 本地库只有 `two` 这一天 → 补档还剩 29 天（今天由主路写）
        save_edge_history_count(two, 7);
        let rows = [
            chrome_start_of(today),
            chrome_start_of(today) + 500_000,
            chrome_start_of(y) + 3_000_000,
            chrome_start_of(two) + 3_000_000,
        ];
        let _fx = write_edge_fixture("backfill", &rows);

        SNAPSHOT_ROUNDS.store(0, std::sync::atomic::Ordering::Relaxed);
        let (ok, today_count, total) = update_today_edge_history();
        let rounds = SNAPSHOT_ROUNDS.load(std::sync::atomic::Ordering::Relaxed);

        assert!(ok, "夹具可读，刷新不该失败");
        assert_eq!((today_count, total), (2, 4));
        assert_eq!(rounds, 1, "今日数+总数+29 天补档必须共用一轮快照");
        // 同步补齐：函数返回时就该已经落库，不是"回头某个时刻出现"
        assert_eq!(saved_count_on(y), Some(1), "缺失日应在本次调用内补齐");
        assert_eq!(saved_count_on(today), Some(2));
        assert_eq!(saved_count_on(two), Some(7), "已有的一天不该被重查覆盖");
        // 再刷新一次：没有缺失日了就完全不必再动补档查询
        assert!(update_today_edge_history().0);
        assert_eq!(
            SNAPSHOT_ROUNDS.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "补齐过后第二次刷新仍是一轮快照"
        );
    }

    /// 崩溃/被杀留在 `%TEMP%` 的副本是**明文浏览记录**，必须能被扫掉；
    /// 但正在使用的副本只有几秒大，绝不能被误删。
    #[test]
    fn stale_temp_copies_are_swept_but_recent_ones_survive() {
        let _lock = crate::paths::test_app_dir_lock();
        let app = crate::paths::test_app_dir("edge_sweep");
        // 扫自己的临时子目录：不碰系统 %TEMP%，也不会留东西
        let dir = app.path().join("sweep");
        std::fs::create_dir_all(&dir).unwrap();
        let mk = |name: &str, age: std::time::Duration| {
            let p = dir.join(name);
            std::fs::write(&p, b"plaintext browsing history").unwrap();
            let old = std::time::SystemTime::now() - age;
            std::fs::OpenOptions::new()
                .write(true)
                .open(&p)
                .expect("为了改 mtime 需要写权限")
                .set_modified(old)
                .expect("set_modified 应可用（Rust 1.75+）");
            p
        };
        let stale_db = mk(
            "focusflow_edge_11111_222222.db",
            std::time::Duration::from_secs(7200),
        );
        let stale_wal = mk(
            "focusflow_edge_11111_222222.db-wal",
            std::time::Duration::from_secs(7200),
        );
        let fresh = mk(
            "focusflow_edge_99999_000000.db",
            std::time::Duration::from_secs(5),
        );
        // 同名不同前缀的文件不该被牵连
        let other = mk("focusflow_other_1.db", std::time::Duration::from_secs(7200));

        let removed = sweep_stale_temp_copies_in(&dir, STALE_TEMP_COPY_AGE);
        assert_eq!(removed, 2, "只该删掉那两份超龄的 Edge 副本");
        assert!(
            !stale_db.exists() && !stale_wal.exists(),
            "超龄副本要真删掉"
        );
        assert!(fresh.exists(), "几秒大的副本可能正被另一个实例用着");
        assert!(other.exists(), "前缀不匹配的文件不该被动");
    }

    /// 把槽位置成「有一轮在跑，且已经跑了 `ms_elapsed` 毫秒」，返回那一轮的世代号。
    /// 每次改槽位都推进世代号，这样上一轮遗留的后台线程（`plugins_test` 真的会
    /// 点一次 refresh）也就没法反过来改写测试中的状态。
    fn fake_running_round(ms_elapsed: i64) -> u64 {
        let mut slot = lock_slot();
        slot.state = ST_RUNNING;
        slot.started_at_ms = now_ms() - ms_elapsed;
        slot.gen += 1;
        slot.gen
    }

    fn reset_slot() {
        let mut slot = lock_slot();
        slot.state = ST_IDLE;
        slot.started_at_ms = 0;
        slot.gen += 1;
    }

    /// 线程卡死（例如回不来地卡在 `fs::copy`）后，刷新按钮不该死到重启为止。
    #[test]
    fn wedged_refresh_self_heals_so_the_button_stays_usable() {
        let _lock = crate::paths::test_app_dir_lock();
        fake_running_round(RUNNING_STALE_MS + 60_000);
        assert_eq!(
            refresh_state(),
            "fail",
            "越过阈值的 running 必须自愈，否则面板永远显示「正在后台读取」"
        );
        assert!(
            claim_round(now_ms()).is_some(),
            "自愈之后下一次点击要真能启动新一轮"
        );
        reset_slot();
    }

    /// 阈值内是"正常在跑"，不能拒绝第二次点击之外的接管，也不能放行第二轮并发。
    #[test]
    fn live_round_is_not_stolen_but_a_wedged_one_is() {
        let _lock = crate::paths::test_app_dir_lock();
        fake_running_round(1_000);
        assert_eq!(refresh_state(), "running", "才跑 1 秒不该被判成卡死");
        assert!(
            claim_round(now_ms()).is_none(),
            "上一轮还在正常跑时再起一轮 = 两个线程各复制 100MB"
        );

        fake_running_round(RUNNING_STALE_MS + 1);
        assert!(
            claim_round(now_ms()).is_some(),
            "超过阈值就该接管，而不是到重启前都点不动"
        );
        reset_slot();
    }

    /// 被接管的那一轮之后就算真的回来，也不许改写新一轮的状态。
    #[test]
    fn superseded_round_cannot_write_back_its_result() {
        let _lock = crate::paths::test_app_dir_lock();
        let zombie = fake_running_round(RUNNING_STALE_MS + 1);
        let live = claim_round(now_ms()).expect("接管应成功");
        assert_ne!(zombie, live);
        finish_round(zombie, true);
        assert_eq!(
            refresh_state(),
            "running",
            "僵尸线程回来不该把新一轮标成已完成"
        );
        finish_round(live, true);
        assert_eq!(refresh_state(), "ok");
        reset_slot();
    }

    /// 「今日记录数」按日期查表。
    ///
    /// 老实现读 meta 里那个"上次刷新时算作今天"的值：应用通宵开着时它不会跨零点
    /// 更新，第二天早上就会把昨天的数当成今天显示。
    #[test]
    fn saved_today_is_date_keyed() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("edge_saved");
        let today = Local::now().date_naive();
        let yesterday = today - chrono::Days::new(1);

        assert_eq!(get_edge_history_saved_today(), None, "从未刷新过不该有值");
        save_edge_history_count(yesterday, 111);
        assert_eq!(
            get_edge_history_saved_today(),
            None,
            "昨天的计数不该被当成今天"
        );
        save_edge_history_count(today, 42);
        assert_eq!(get_edge_history_saved_today(), Some(42));
        save_edge_history_meta("total", 900);
        assert_eq!(get_edge_history_saved_total(), Some(900));
    }
}
