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

/// 测试夹具的临时副本路径（进程级，靠 `test_app_dir_lock` 串行化）。
#[cfg(test)]
static TEMP_COPY_OVERRIDE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

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
    #[cfg(test)]
    if let Some(p) = TEMP_COPY_OVERRIDE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        return p;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "{TEMP_COPY_PREFIX}{}_{nanos}.db",
        std::process::id()
    ))
}

/// 删除副本及其 WAL/SHM 附属文件，回**删不动的那几份**（带系统原因）。
///
/// 副本里是明文浏览记录（URL、标题、时间）。正常路径每一轮兜底都走这里，而
/// `cleanup_stale_copies_once` 一个进程只跑一次、门槛又是一小时 ⇒ 这一份此刻删不动时
/// 没有任何机制会在本次运行里再扫它一遍，只能等下次启动 —— 原先三处 `let _ =`
/// 让这件事在日志里一个字都没有。上面 `SweepReport` 那段立的规矩（"删不动"与"没得删"
/// 必须分开说）当时只落了清扫那半边，正常路径这半边漏下了。
///
/// `NotFound` 不算删不动：那份本来就不在（复制没建成、或只建了主文件没有 sidecar），
/// 把它报成"有一份清不掉"是让假警。
fn remove_temp_copy(temp: &std::path::Path) -> Vec<String> {
    let mut stuck: Vec<String> = Vec::new();
    let mut discard = |path: &std::path::Path| {
        if let Err(e) = std::fs::remove_file(path) {
            if !crate::data_location::is_confirmed_absent(&e) {
                stuck.push(format!("{}: {e}", path.display()));
            }
        }
    };
    discard(temp);
    for suffix in ["-wal", "-shm"] {
        discard(&crate::data_location::with_suffix(temp, suffix));
    }
    stuck
}

/// 丢掉副本，并把"哪一份还留在临时目录里"说到日志上（清单原样交回，供用例数）。
///
/// 调用方是快照读取的三条出路（复制没成／查成功／查不动），每一条都不该因为
/// "这只是收尾"就把话咽下去：这一刻留在 `%TEMP%` 里的那份文件比什么都可读。
fn discard_temp_copy(temp: &std::path::Path) -> Vec<String> {
    let stuck = remove_temp_copy(temp);
    if !stuck.is_empty() {
        tracing::warn!(
            "有 {} 份 Edge 历史的明文副本删不掉（{}）—— 它会留在系统临时目录里，\
             直到下一次启动的清扫（约一小时后才会再试一次）",
            stuck.len(),
            stuck.join("；")
        );
    }
    stuck
}

/// 副本名里稳定的前缀，供启动时清扫识别。
const TEMP_COPY_PREFIX: &str = "focusflow_edge_";

/// 清扫 `%TEMP%` 里残留副本的年龄门槛。
///
/// 一份副本只在 `with_edge_snapshot` 兜底期间存在（毫秒到秒级），留够一个数量级
/// 的余量就不会碰到另一个实例正在用的文件；门槛再短就成了"两个实例互删"。
const STALE_TEMP_COPY_AGE: std::time::Duration = std::time::Duration::from_secs(3600);

/// 一次清扫的三种结局，分开交回来。
///
/// 原先这个函数只回一个 `usize`：「目录读不出来」「目录里没有超龄副本」
/// 「有超龄副本但一份都删不动」三件事共用同一个 0，而调用方只在 `> 0` 时说话 ⇒
/// 一份含明文浏览记录的副本可以永远躺在临时目录里，日志一个字都没有。
/// 这条清扫存在的理由恰恰是"那份 History 谁都能读"，所以删不动与没得删必须分开说。
#[derive(Debug, Default)]
struct SweepReport {
    /// 真删掉的份数
    removed: usize,
    /// 超龄却删不动的副本（带系统原因）：杀软实时扫描、另一个实例握着句柄
    stuck: Vec<String>,
    /// 目录本身读不出来的原因；`None` = 读到了
    unreadable: Option<String>,
}

/// 删除目录里老于 `max_age` 的 Edge 副本（含 `-wal`/`-shm`）。
///
/// 为什么需要这一条：副本里是**明文浏览记录**，正常路径每次都走 `discard_temp_copy`
/// （它把删不动的那几份说到日志上），
/// 但 release 是 `panic = "abort"`、被任务管理器直接杀也来不及删 —— 那份完整的
/// History 就永久躺在 `%TEMP%`，谁都能读。`dir` 参数是为测试留的：单测扫自己的
/// 临时目录，不去碰系统 `%TEMP%`。
fn sweep_stale_temp_copies_in(dir: &std::path::Path, max_age: std::time::Duration) -> SweepReport {
    let mut report = SweepReport::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            report.unreadable = Some(format!("{}: {e}", dir.display()));
            return report;
        }
    };
    let now = std::time::SystemTime::now();
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
        if age <= max_age {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => report.removed += 1,
            Err(e) => report.stuck.push(format!("{name}: {e}")),
        }
    }
    report
}

/// 旧版本曾把副本放在程序目录 `data/_edge_history_temp.db`，
/// 崩溃时会残留明文浏览记录；顺手扫 `%TEMP%` 里本模块留下的副本。
/// 一个进程只跑一次，所以放在每次快照读取的入口，而不是等到真要复制时才清。
fn cleanup_stale_copies_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let legacy = paths::data_dir().join("_edge_history_temp.db");
        let _ = std::fs::remove_file(&legacy);
        for suffix in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(crate::data_location::with_suffix(&legacy, suffix));
        }
        let report = sweep_stale_temp_copies_in(&std::env::temp_dir(), STALE_TEMP_COPY_AGE);
        // 三种结局各说一句：副本里是明文浏览记录，"清不动"和"没得清"必须分得开，
        // 否则这份隐私数据可以一直留着而日志上一个字都没有。
        if let Some(reason) = report.unreadable {
            tracing::warn!(
                "系统临时目录读不出来（{reason}）：上一次异常退出留下的 Edge 历史副本（明文浏览记录）\
                 可能还在那里，这次一份都没清动"
            );
        }
        if !report.stuck.is_empty() {
            tracing::warn!(
                "有 {} 份超龄的 Edge 历史副本删不掉（系统临时目录，内含明文浏览记录）：{} —— \
                 关掉握着这些文件的程序（杀软实时扫描、另一个 FocusFlow 实例）之后再试，或手动删除",
                report.stuck.len(),
                report.stuck.join("；")
            );
        }
        if report.removed > 0 {
            // 说清"为什么会有"，否则用户只看到一句"我删了你临时目录里的东西"
            tracing::warn!(
                "清理了 {} 份上一次异常退出残留的 Edge 历史副本（位于系统临时目录，含明文浏览记录）",
                report.removed
            );
        }
    });
}

/// 一次快照读取的三种结局。
#[derive(Debug, PartialEq, Eq)]
enum Snapshot<T> {
    /// 查到了
    Value(T),
    /// **没找到** Edge 的历史库（带路径）：没装 Edge、别的 Profile、便携版
    Absent(String),
    /// 找到了但这一轮读不出来（被独占、库过大不敢复制、副本三轮都失败）
    Unreadable(String),
}

/// 在一份 Edge History 快照上执行一批查询，交出**这一轮到底是哪一种结局**。
///
/// 原先回 `Option`：`None` 同时表示"没找到这个文件"（那条早退连一行日志都不写）与
/// "找到了但读不动"（各有一行 warn），调用方只能把两者并成一句
/// 「Edge 历史库被占用或不可读，请稍后重试」。那句话对"没装 Edge / 用的是 Profile 1"
/// 的用户是错的建议 —— 关浏览器、重启、再点刷新，一百次也不会好。
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
fn with_edge_snapshot<T>(f: impl Fn(&Connection) -> Option<T>) -> Snapshot<T> {
    #[cfg(test)]
    SNAPSHOT_ROUNDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    cleanup_stale_copies_once();
    let path = edge_history_path();
    // 「在不在」问成三态（判据同 `data_location::is_confirmed_absent`）：确认没有 ⇒
    // 没装/别的 Profile；stat 自己失败 ⇒ 不许冒充"没装"，走读不出来那一句。
    match std::fs::metadata(&path) {
        Ok(_) => {}
        Err(e) if crate::data_location::is_confirmed_absent(&e) => {
            tracing::warn!(
                "没找到 Edge 的历史库（{}）—— 没装 Edge、用的是别的 Profile（如 Profile 1）\
                 或便携版时就是这个形状；这不是「Edge 正开着把库占住了」",
                path.display()
            );
            return Snapshot::Absent(format!("{}", path.display()));
        }
        Err(e) => {
            let why = format!("说不出 Edge 历史库在不在（{}）: {e}", path.display());
            tracing::error!("{why}");
            return Snapshot::Unreadable(why);
        }
    }

    // 1) 只读直连：rusqlite 打开不一定失败（锁在首个查询才报 SQLITE_BUSY），
    //    必须实际验证查询可用才采用，否则 Edge 运行时直连会静默返回 0。
    if let Ok(conn) = Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        let _ = conn.busy_timeout(EDGE_BUSY_TIMEOUT);
        if let Some(v) = f(&conn) {
            return Snapshot::Value(v);
        }
    }

    // 2) 复制兜底：先检查大小，超大库跳过复制避免卡顿
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if size > EDGE_COPY_MAX_BYTES {
        tracing::warn!(
            "Edge 历史库过大（{}MB），跳过复制（只读直连被锁定）",
            size / (1024 * 1024)
        );
        return Snapshot::Unreadable(format!(
            "Edge 历史库过大（{}MB），这次不复制快照去读",
            size / (1024 * 1024)
        ));
    }
    let temp = temp_copy_path();
    for attempt in 0..3 {
        // 缺 -wal 的副本不拿去做查询：那份历史会比真实值小，而补档写进本地缓存后
        // 历史日不会被再挑出来重查（见 `try_snapshot_copy` 的注释）。
        if let Err(e) = try_snapshot_copy(&path, &temp) {
            discard_temp_copy(&temp);
            tracing::warn!(
                "Edge 历史库快照不完整（第{}次）：{e} —— 不拿缺 WAL 的副本去查",
                attempt + 1
            );
            std::thread::sleep(std::time::Duration::from_millis(150));
            continue;
        }
        if let Ok(conn) = Connection::open(&temp) {
            if let Some(v) = f(&conn) {
                drop(conn);
                discard_temp_copy(&temp);
                return Snapshot::Value(v);
            }
        }
        discard_temp_copy(&temp);
        tracing::debug!("Edge 历史库复制查询失败（第{}次），重试", attempt + 1);
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    tracing::warn!("Edge 历史库无法读取（直连被锁且复制失败）");
    Snapshot::Unreadable("直连被锁，且三轮复制快照都没读到".to_string())
}

/// 把 Edge 历史库连同 sidecar 复制成一份可查询的快照。
///
/// `-wal` 必须搬成功：Edge 运行时刚写入的记录还在 WAL 里，缺了它这份副本就是一份
/// **偏小**的历史。旧写法 `let _ = copy(…)` 吞掉失败后照查不误，补档那些历史日会把
/// 偏小的数写进本地缓存，而 `stale_days_before` 之后不再重查 —— 永久少算。
/// `-shm` 拿不到无所谓：SQLite 会照着 WAL 自己重建。
fn try_snapshot_copy(src: &std::path::Path, temp: &std::path::Path) -> Result<(), String> {
    std::fs::copy(src, temp).map_err(|e| {
        format!(
            "复制主库失败（{} → {}）: {e}",
            src.display(),
            temp.display()
        )
    })?;
    for suffix in ["-wal", "-shm"] {
        let from = crate::data_location::with_suffix(src, suffix);
        if !from.exists() {
            continue;
        }
        let to = crate::data_location::with_suffix(temp, suffix);
        match std::fs::copy(&from, &to) {
            Ok(_) => {}
            Err(e) if suffix == "-wal" => {
                return Err(format!(
                    "复制 -wal 失败（{} → {}）: {e}",
                    from.display(),
                    to.display()
                ));
            }
            Err(e) => tracing::debug!("跳过 Edge 快照的 -shm 副本: {e}"),
        }
    }
    Ok(())
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

/// 本地 Edge 缓存库**在不在**：`Ok(true)` 在、`Ok(false)` 确认没有、`Err` 这一问问不出来。
///
/// 这一问必须先有，因为 [`open_local`] 会把文件创建出来，读侧不能拿"库里有没有行"
/// 当"有没有刷过"。但 `Path::exists()` 只答得了三件事里的两件（真没有／读得动／
/// **这一次 stat 自己失败了**），第三种被它折成"没有" ⇒ 挂件上就只剩那个 "—"
/// （从没刷过），而真正的原因一个字都没留下。判据与 `get_edge_history_counts`
/// 共用这一份（原先各处自己 `match std::fs::metadata`）。
fn local_cache_present() -> Result<bool, String> {
    let path = edge_db_path();
    match std::fs::metadata(&path) {
        Ok(_) => Ok(true),
        Err(e) if crate::data_location::is_confirmed_absent(&e) => Ok(false),
        Err(e) => Err(format!(
            "说不出本地 Edge 缓存库在不在（{}）: {e}",
            path.display()
        )),
    }
}

/// 保存指定日期的计数到本地。失败原因交回调用方 —— 本轮刷新要据此改口，
/// 不许把"写不进去"吞成"刷新成功"（见 [`update_today_edge_history`]）。
pub fn save_edge_history_count(target_date: NaiveDate, count: i64) -> Result<(), String> {
    let day = target_date.format("%Y-%m-%d").to_string();
    let conn = open_local().map_err(|e| format!("打开本地缓存库失败: {e}"))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS edge_history (
            date TEXT PRIMARY KEY,
            count INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );",
    )
    .map_err(|e| format!("{day} 建表失败: {e}"))?;
    conn.execute(
        "INSERT OR REPLACE INTO edge_history (date, count, updated_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![day, count, Utc::now().timestamp()],
    )
    .map_err(|e| format!("{day} 写入失败: {e}"))?;
    Ok(())
}

/// 获取近 N 天 Edge 历史计数，**按天补零**：返回的要么正好是 `days` 项、按日期升序，
/// 要么是空表（这个窗口内一次都没刷新过）。
///
/// 窗口是 `[今天-days+1, 今天]`，两头都夹：晚于今天的行既不进表，也不算"刷过"
/// （见查询处那条注释 —— 它能从时钟超前的机器上随 `data/` 一起复制进来）。
///
/// 以前只回数据库里存在的那几行：面板标题写着「近 30 天」，表格里却常只躺两三行，
/// 而且没人说得清"这天没记录"和"这天没刷过"差在哪。补零之后"没有行"这个信号只剩
/// 一种含义（从没刷新过），插件据此把峰值也显示成 "—"，与今日/总数同一口径。
///
/// 从没刷新过时**刻意不补零**：`open_local()` 会把文件创建出来，所以不能靠"文件在不在"
/// 判，只能看窗口内到底有没有行 —— 补成一整列 0 就等于把"没刷过"伪装成"每天都是 0"。
/// 天数上限 366 只是防御性的：真正决定 `with_capacity` 的数值不能来自外部（release 是
/// `panic = "abort"`，`i64::MAX` 那种值会直接在分配处崩掉整个程序）。
///
/// `Err` 只表示**读不出可信内容**（本地缓存库打不开、查询失败），与"读到了、库里就是
/// 没有行"是两件事：旧写法把两种都回成空表，于是"刷新成功、数据为空"与"本地缓存根本
/// 读不动"在界面上长得一模一样（与上一场修掉的落盘假成功同族）。
pub fn get_edge_history_counts(days: i64) -> Result<Vec<(String, i64)>, String> {
    // 上面自己写着"不能靠文件在不在判"，所以这一问保留，只把它问成三态
    // （见 [`local_cache_present`]）：确认没有 ⇒ 空表；stat 自己失败 ⇒ `Err`，
    // 不许冒充"这个窗口一次都没刷过"。
    if !local_cache_present()? {
        return Ok(Vec::new());
    }
    let conn = open_local().map_err(|e| format!("打开本地缓存库失败: {e}"))?;
    // 「表还没建过」与「表在但这一窗没有行」都是真的没刷过，不是读不动 ——
    // 首次刷新之前 `open_local()` 刚创建出来的就是这份空库。
    if !history_table_exists(&conn)? {
        return Ok(Vec::new());
    }
    let days = days.clamp(1, 366);
    let today = Local::now().date_naive();
    let Some(start) = today.checked_sub_days(chrono::Days::new((days - 1) as u64)) else {
        return Ok(Vec::new());
    };
    let start_str = start.format("%Y-%m-%d").to_string();
    let today_str = today.format("%Y-%m-%d").to_string();
    // 上界必须有：库里可能存在**晚于今天**的行（在时钟超前的机器上刷新一轮后，把
    // `data/` 按搬家那条路口径复制过来；或本机对过表 —— 先往前调、刷了一轮、又调回来，
    // 与 `writer.rs` 为时钟回拨重锚今日计数防的是同一件事）。只夹下界时那行会挤进
    // `saved`，于是下面"窗口内没有行 = 从没刷过"这个哨兵被顶开，一列 0 的假历史就
    // 发出去了，而插件的峰值会显示 0 而不是 "—"。
    let mut stmt = conn
        .prepare(
            "SELECT date, count FROM edge_history WHERE date >= ?1 AND date <= ?2 ORDER BY date",
        )
        .map_err(|e| format!("准备趋势查询失败: {e}"))?;
    let saved: Vec<(String, i64)> = stmt
        .query_map(rusqlite::params![start_str, today_str], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })
        .map_err(|e| format!("查询趋势失败: {e}"))?
        .flatten()
        .collect();
    if saved.is_empty() {
        return Ok(Vec::new());
    }

    let mut by_date: std::collections::HashMap<String, i64> = saved.into_iter().collect();
    let mut out = Vec::with_capacity(days as usize);
    let mut day = start;
    while day <= today {
        let key = day.format("%Y-%m-%d").to_string();
        let count = by_date.remove(&key).unwrap_or(0);
        out.push((key, count));
        match day.succ_opt() {
            Some(next) => day = next,
            None => break,
        }
    }
    Ok(out)
}

/// 刷新状态（`RefreshSlot::state` 的取值）。
const ST_IDLE: u8 = 0;
const ST_RUNNING: u8 = 1;
const ST_OK: u8 = 2;
const ST_FAIL: u8 = 3;
/// 读到了 Edge 的数，但**没写进自己的本地缓存**。
///
/// 与 `ST_FAIL` 分开是因为给用户的那句话完全不同：读不到是"Edge 还开着、去关掉再试"，
/// 写不进是"FocusFlow 自己的 `data/` 目录不让写"—— 后者重试一百次也没用，
/// 得先让那个目录可写（同步盘、杀软、只读移动盘）。过去两者折成一个 `fail`，
/// 挂件照着后一句提示，用户就一直在关 Edge。
const ST_WRITE_FAIL: u8 = 4;
/// 压根**没找到** Edge 的历史库（不是它被占用）。
///
/// 与 `ST_FAIL` 分开的原因和上面那条一样：给用户的话完全不同。路径是写死的
/// `%LOCALAPPDATA%\Microsoft\Edge\User Data\Default\History`，所以"没装 Edge"、
/// "用的是 `Profile 1` 之类的其它 Profile"、"便携版把 User Data 放在别处"这三种
/// 常见形态都落在这里 —— 原先它们和"Edge 正开着、锁着库"共用一句
/// 「被占用或不可读…请稍后重试」，于是用户反复关 Edge、重启浏览器，重试一百次
/// 也不会好，而日志一个字都没有（那条早退既没 warn 也没 error）。
const ST_NO_SOURCE: u8 = 5;

/// 一次刷新失败在哪一步。
#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub enum RefreshFail {
    /// Edge 的历史库读不出来（被独占、快照副本也失败）
    Read,
    /// Edge 读到了，本地缓存写不进去
    Write,
    /// 没找到 Edge 的历史库（没装 Edge、别的 Profile、便携版）—— 重试不解决问题
    NoSource,
}

/// RUNNING 超过这个时长就认定为卡死。
///
/// 阈值刻意放宽到 120 秒：一轮正常刷新最坏也只是 300ms busy 等待 + 3 轮 ≤100MB
/// 整文件复制，慢盘上十几秒也走得完，这里留了一个数量级的余量。取短了会招来
/// 更糟的后果 —— 接管会与仍在正常跑的轮次并发，两个线程各复制一份 100MB。
const RUNNING_STALE: std::time::Duration = std::time::Duration::from_secs(120);

/// 刷新槽位：状态 + 本轮世代号 + 进入 RUNNING 的时刻。
///
/// 世代号是给「被接管的那一轮」准备的：卡死的线程哪天真的从 `fs::copy` 里回来，
/// 也不该由它把新一轮的状态改写成 ok/fail。
///
/// 计时用 `Instant` 而不是墙上时钟：本机对过表（NTP 校正、手动改系统时间、
/// 从休眠回来同步）会让一个**正在正常跑**的轮次被算成"跑了 2 分钟"，于是再起一轮
/// —— 那正是 120 秒阈值想防的"两个线程各复制一份 100MB"。反过来时钟后跳会让
/// `now - started` 变成负数（饱和成 0），按钮卡在「正在后台读取」几个小时点不动。
/// `Instant` 走的是单调计时器，不受时钟调整影响；本仓别处（`maintenance.rs`）也是这个口径。
#[derive(Default)]
struct RefreshSlot {
    state: u8,
    gen: u64,
    started_at: Option<std::time::Instant>,
}

static REFRESH_SLOT: std::sync::Mutex<RefreshSlot> = std::sync::Mutex::new(RefreshSlot {
    state: ST_IDLE,
    gen: 0,
    started_at: None,
});

fn lock_slot() -> std::sync::MutexGuard<'static, RefreshSlot> {
    REFRESH_SLOT.lock().unwrap_or_else(|e| e.into_inner())
}

/// 一轮刷新是否已经拖过了头。
fn is_wedged(slot: &RefreshSlot, now: std::time::Instant) -> bool {
    slot.state == ST_RUNNING
        && slot
            .started_at
            .is_some_and(|s| now.saturating_duration_since(s) >= RUNNING_STALE)
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
    let Some(gen) = claim_round(std::time::Instant::now()) else {
        return false;
    };
    let spawned = std::thread::Builder::new()
        .name("edge-refresh".into())
        .spawn(move || {
            let (res, _today, _total) = update_today_edge_history();
            finish_round(gen, res);
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
/// 拒绝的唯一理由是上一轮还在正常跑；它一旦超过 [`RUNNING_STALE`] 仍没收尾就
/// 按卡死处理并接管 —— 不然线程回不来时，用户点到重启为止都不会再有任何反应。
/// 世代号用来让被接管掉的那一轮之后再返回也改不动新一轮的状态。
fn claim_round(now: std::time::Instant) -> Option<u64> {
    let mut slot = lock_slot();
    let wedged = is_wedged(&slot, now);
    if slot.state == ST_RUNNING && !wedged {
        return None;
    }
    if wedged {
        tracing::warn!(
            "Edge 刷新线程超过 {} 秒未收尾，按卡死接管并启动新一轮",
            RUNNING_STALE.as_secs()
        );
    }
    slot.state = ST_RUNNING;
    slot.started_at = Some(now);
    slot.gen += 1;
    Some(slot.gen)
}

/// 收尾：只有「本轮仍是当前这一轮」时才允许写回结果。
fn finish_round(gen: u64, res: Result<(), RefreshFail>) {
    let mut slot = lock_slot();
    if slot.gen != gen {
        return;
    }
    slot.state = match res {
        Ok(()) => ST_OK,
        Err(RefreshFail::Read) => ST_FAIL,
        Err(RefreshFail::Write) => ST_WRITE_FAIL,
        Err(RefreshFail::NoSource) => ST_NO_SOURCE,
    };
    slot.started_at = None;
}

/// 线程根本没起来时的回滚：本轮没人在跑，别把状态留在 RUNNING 卡住用户。
fn release_round(gen: u64) {
    let mut slot = lock_slot();
    if slot.gen != gen {
        return;
    }
    slot.state = ST_IDLE;
    slot.started_at = None;
}

/// 一次后台刷新的状态：`idle` / `running` / `ok` / `fail` / `write_fail` / `no_source`。
///
/// 三种失败各说一句：读不到 Edge（`fail`，去关 Edge 有用）、写不进自己的缓存
/// （`write_fail`，要让 `data/` 可写）、压根没找到 Edge 的历史库（`no_source`，
/// 关浏览器与重试都不解决问题，要指的是 Profile 路径）。
/// 第三方脚本把三者都当"不是 ok"处理仍然正确，所以这些是**加值**、没有改旧值的含义。
//////
/// 读的时候顺手自愈：光靠 `spawn_update_today` 里的接管救不了刷新按钮 ——
/// 线程卡死后插件会一直显示「正在后台读取」，用户连再点一次的入口都没有。
pub fn refresh_state() -> &'static str {
    refresh_state_at(std::time::Instant::now())
}

/// [`refresh_state`] 的可注入时刻版：卡死判定要能测，就得能把"现在"当参数传进来。
fn refresh_state_at(now: std::time::Instant) -> &'static str {
    let mut slot = lock_slot();
    if is_wedged(&slot, now) {
        tracing::error!(
            "Edge 刷新线程已卡死（超过 {} 秒未收尾），状态已复位，可重新点刷新",
            RUNNING_STALE.as_secs()
        );
        slot.state = ST_FAIL;
        slot.started_at = None;
        // 推进世代号，让那个再也不会收尾的线程之后没有改写的余地
        slot.gen += 1;
    }
    match slot.state {
        ST_RUNNING => "running",
        ST_OK => "ok",
        ST_FAIL => "fail",
        ST_WRITE_FAIL => "write_fail",
        ST_NO_SOURCE => "no_source",
        _ => "idle",
    }
}

/// 补档往回看多少天（与趋势表的口径一致）。
const BACKFILL_DAYS: i64 = 30;

/// 更新今天并返回 (结果, 今日数, 总数)。
///
/// 三种失败要分得开（见 [`RefreshFail`]）：Edge 库被锁/副本失败是 `Err(Read)` 且两个数都是 0；
/// 压根**没找到** Edge 的历史库是 `Err(NoSource)`（没装、别的 Profile、便携版 —— 关浏览器
/// 与重试都不解决问题）；读到数但**没写进本地缓存**是 `Err(Write)`，数值照样交回
/// —— 把后两者也说成"读失败"会让用户去关 Edge，而他的 `data/` 目录或路径假设才是问题所在。
/// 调用方据此提示用户，避免把失败静默当成"0 条记录"。同一次快照顺手把近 30 天里
/// 不可信的历史计数重查一遍。
///
/// 调用方应当用 [`spawn_update_today`] 而不是直接调本函数（本函数会阻塞调用线程）。
pub fn update_today_edge_history() -> (Result<(), RefreshFail>, i64, i64) {
    let today = Local::now().date_naive();
    let Some((chrome_start, chrome_end)) = chrome_day_range(today) else {
        // 换算不出当天的时间区间 = 这一轮根本没去读 ⇒ 归到读失败那一类（不是写不进）
        return (Err(RefreshFail::Read), 0, 0);
    };
    // 要补哪些天必须在开快照**之前**定好：这样今日数、总数和整批补档共用同一轮
    // 快照，被锁时也只兜底复制一份副本，且所有数字取自同一时点。
    let stale = stale_days_before(today, BACKFILL_DAYS);
    let pending = stale.len();
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
        let filled = counts_on_snapshot(conn, &stale).unwrap_or_default();
        Some((day, total, filled))
    });
    match batch {
        Snapshot::Value((today_count, total, filled)) => {
            let persisted = save_edge_history_count(today, today_count)
                .and_then(|_| save_edge_history_meta("total", total));
            let mut backfill_failed = 0usize;
            if !filled.is_empty() {
                for (day, count) in &filled {
                    if let Err(e) = save_edge_history_count(*day, *count) {
                        backfill_failed += 1;
                        tracing::debug!("Edge 补档 {day} 未落库: {e}");
                    }
                }
                if backfill_failed == 0 {
                    tracing::info!(
                        "Edge 历史已重查 {}/{} 天（缺行的日子 + 当天就写下、还没盖完整的日子）",
                        filled.len(),
                        pending
                    );
                } else {
                    // 补档失败不翻本轮状态：那些是**历史行**，下一轮 `stale_days_before`
                    // 仍会把它们挑出来重查（写的是 `INSERT OR REPLACE`，无双计风险）。
                    tracing::warn!(
                        "Edge 补档有 {}/{} 天没写进本地缓存，下一轮重试",
                        backfill_failed,
                        filled.len()
                    );
                }
            }
            match persisted {
                Ok(()) => (Ok(()), today_count, total),
                Err(e) => {
                    // 面板读的是本地库（`get_edge_history_saved_*`）：今日数没落库时
                    // 它显示 "—"，而状态位还报 ok —— "刷新成功、数据为空"于是与
                    // "真的没数据"无法区分。数值仍交回调用方（本轮确实读到了）。
                    tracing::error!(
                        "Edge 计数没写进本地缓存（今日 {today_count}／总数 {total} 只在内存里）: {e}"
                    );
                    (Err(RefreshFail::Write), today_count, total)
                }
            }
        }
        // 「没找到 Edge 的历史库」与「找到了但读不动」分开：前者叫用户去关 Edge 是错的建议，
        // 具体路径已经由 `with_edge_snapshot` 写进日志（面板这一头只有状态字符串，
        // 带不动路径 —— 挂件那句要指的是 Profile 路径这件事）。
        Snapshot::Absent(_) => (Err(RefreshFail::NoSource), 0, 0),
        Snapshot::Unreadable(_) => (Err(RefreshFail::Read), 0, 0),
    }
}

/// 近 `days` 天里本地缓存**还不可信**的日子（不含 `today`，那条由刷新主路写）。
///
/// 不可信有两种，旧实现只认第一种：
/// ① 缓存里压根没有这一行；
/// ② 有行，但那一行是在**该日结束之前**写的 —— 于是它只覆盖到当天最后一次刷新为止，
///    当天剩下的几个小时永久丢失（早上刷一次，那一天就少算一整天里余下的部分）。
///
/// 为什么第②种能靠重查救回来：Edge 的 `urls.last_visit_time` 是"这条 URL 最后一次
/// 访问的时刻"，第二天再按同一天的区间查，仍停在那一天的 URL 都还在区间内，整天就
/// 捞齐了。代价是那之后又访问过的 URL 会从这一天挪走 —— 但"挪走"只在 URL 被再次
/// 访问时发生，比"整个下午加晚上都没有"小一个量级。
///
/// 判据刻意做成"只在还没盖完时重查"：重查过一次之后 `updated_at` 就落在该日之后了，
/// 每个历史日因此**恰好**被重查一次（下一次刷新时），不会天天回头改写更老的日子。
fn stale_days_before(today: NaiveDate, days: i64) -> Vec<NaiveDate> {
    let start = today - chrono::Days::new((days - 1).max(0) as u64);
    let saved = match saved_rows_since(start) {
        Ok(rows) => rows,
        Err(e) => {
            // 读不出判据 ≠ 判据说"这些天都没有行"。旧写法 `unwrap_or_default()` 把两件事
            // 并成一件，于是整窗 29 天全被判成陈旧：每轮刷新都为这 29 天重查 Edge、并在
            // 库里被别的连接占住时走 ≤100MB×3 的整文件复制兜底 —— 白烧一整轮 IO，
            // 而且永远不收敛（因为下一次照样读不动）。跳过本轮补档，下一轮再挑。
            tracing::warn!("Edge 补档判据读不出来，本轮跳过补档（下一轮重试）: {e}");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    let mut day = start;
    while day < today {
        let complete = match saved.get(&day.format("%Y-%m-%d").to_string()) {
            None => false,
            Some(&updated_at) => day_end(updated_at, day),
        };
        if !complete {
            out.push(day);
        }
        day = day + chrono::Days::new(1);
    }
    out
}

/// 那一天结束之前是否已经不再改写了（`updated_at` 不早于该日本地零点之后的界）。
/// 界算不出来（日期越界）就当已完整，宁可少重查也不要每天都重查。
fn day_end(updated_at: i64, day: NaiveDate) -> bool {
    let Some(next) = day.succ_opt() else {
        return true;
    };
    match local_midnight(next) {
        Some(end) => updated_at >= end.timestamp(),
        None => true,
    }
}

/// `edge_history` 表在不在。`Err` = 这份库根本读不动（不是数据库、被别的连接占着、
/// 模式损坏），`Ok(false)` 才是"从没建过表"。
///
/// 拆出来是因为两条读路都把"表还没建"当成判据：首刷之前 `open_local()` 刚创建的
/// 空库就是这种状态，直接查 `edge_history` 会报 `no such table`，那不该混进"读不动"。
fn table_exists(conn: &Connection, name: &str) -> Result<bool, String> {
    let mut stmt = conn
        .prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1 LIMIT 1")
        .map_err(|e| format!("检查 {name} 表失败: {e}"))?;
    let found = stmt
        .query_map([name], |r| r.get::<_, i64>(0))
        .map_err(|e| format!("读取表清单失败: {e}"))?
        .flatten()
        .next()
        .is_some();
    Ok(found)
}

fn history_table_exists(conn: &Connection) -> Result<bool, String> {
    table_exists(conn, "edge_history")
}

/// 本地缓存库里 `start` 之后（含）已有记录的日子 → 该行的写入时刻。
///
/// `Err` = 这份判据**读不出来**；调用方不许把它当成"读到了、库里没有行"（见
/// [`stale_days_before`]）。
fn saved_rows_since(start: NaiveDate) -> Result<std::collections::HashMap<String, i64>, String> {
    let conn = open_local().map_err(|e| format!("打开本地缓存库失败: {e}"))?;
    if !history_table_exists(&conn)? {
        return Ok(std::collections::HashMap::new());
    }
    let mut stmt = conn
        .prepare("SELECT date, updated_at FROM edge_history WHERE date >= ?1")
        .map_err(|e| format!("准备补档判据查询失败: {e}"))?;
    let rows = stmt
        .query_map([start.format("%Y-%m-%d").to_string()], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })
        .map_err(|e| format!("查询补档判据失败: {e}"))?
        .flatten()
        .collect();
    Ok(rows)
}

/// 保存上次刷新的数值（meta 表），插件重启后恢复显示，避免出现误导性的 "—" / 0。
/// 失败原因同样交回调用方。
fn save_edge_history_meta(key: &str, value: i64) -> Result<(), String> {
    let conn = open_local().map_err(|e| format!("打开本地缓存库失败: {e}"))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )
    .map_err(|e| format!("{key} 建表失败: {e}"))?;
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
        rusqlite::params![key, value.to_string()],
    )
    .map_err(|e| format!("{key} 写入失败: {e}"))?;
    Ok(())
}

/// 读取本地缓存库中某一天已保存的计数：`Ok(Some(n))` 读到了、`Ok(None)` **确认**那天
/// 没刷过、`Err` 这一问问不出来（带原因）。
///
/// 原先三种结局全折成 `None`：`edge_db_path().exists()` 把"这一次 stat 失败了"读成
/// "没有"，`open_local().ok()?` 把"缓存库打不开"读成"没刷过"，`query_row(..).ok()` 把
/// "这一行查不动"也读成"没刷过"。挂件那侧 `today and tostring(today) or "—"` ⇒
/// 用户看到的是"这个功能你还没用过"，而真实原因是它的缓存库此刻读不动。
/// 「还没建过 `edge_history` 这张表」仍归 `Ok(None)`（还没建 ≠ 坏了），与
/// `accounting::CategoryType`、本文件 `Snapshot` 同一把尺子。
fn saved_count_on(target_date: NaiveDate) -> Result<Option<i64>, String> {
    use rusqlite::OptionalExtension;
    if !local_cache_present()? {
        return Ok(None);
    }
    let conn = open_local().map_err(|e| format!("打开本地 Edge 缓存库失败: {e}"))?;
    if !history_table_exists(&conn)? {
        return Ok(None);
    }
    let day = target_date.format("%Y-%m-%d").to_string();
    conn.query_row(
        "SELECT count FROM edge_history WHERE date = ?1",
        [&day],
        |r| r.get::<_, i64>(0),
    )
    .optional()
    .map_err(|e| format!("{day} 那一天的 Edge 计数读不出来: {e}"))
}

/// 读取今天的 Edge 记录数（本地缓存）。
///
/// 按日期查 `edge_history` 表，而不是读 meta 里那个"上次刷新时当作今天"的值：
/// meta 不会自己跨零点更新，应用通宵开着的话，第二天早上就会把昨天的数当成
/// "今日记录数"显示给用户。
///
/// 结局三态见 `saved_count_on`；`Err` 时调用方不许把它印成"从没刷过"。
pub fn get_edge_history_saved_today() -> Result<Option<i64>, String> {
    saved_count_on(Local::now().date_naive())
}

/// 读取上次保存的总记录数（本地缓存），结局三态同 `saved_count_on`。
///
/// 这条原先没有"在不在"那一问，直接 `open_local()` —— 而 `open_local()` 会建目录、
/// `Connection::open` 会把库文件创建出来。挂件每渲染一次就调它一次（`get_view`），
/// 于是**从没用过 Edge 功能的机器上也会躺着一个空缓存库**：与"插件停用了但数据目录里
/// 却躺着它的空库"同族，只是这次是渲染路径干的。
pub fn get_edge_history_saved_total() -> Result<Option<i64>, String> {
    use rusqlite::OptionalExtension;
    if !local_cache_present()? {
        return Ok(None);
    }
    let conn = open_local().map_err(|e| format!("打开本地 Edge 缓存库失败: {e}"))?;
    // 库里还没有 meta 这张表 = 一次都没刷过，不是读不动
    if !table_exists(&conn, "meta")? {
        return Ok(None);
    }
    let raw = conn
        .query_row("SELECT value FROM meta WHERE key='total'", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()
        .map_err(|e| format!("上次保存的 Edge 总数读不出来: {e}"))?;
    let Some(raw) = raw else { return Ok(None) };
    raw.parse::<i64>()
        .map(Some)
        .map_err(|e| format!("本地缓存里 total 那格写的是「{raw}」，不是数字: {e}"))
}

/// 本地趋势（近 N 天），供插件展示。
pub fn trend_counts(days: i64) -> Result<Vec<(String, i64)>, String> {
    get_edge_history_counts(days)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用例侧读缓存计数：`Err` 一律算用例失败，不许悄悄当"没有"。
    ///
    /// 这一场起「问不出来」是第三种独立结局，既有那些"该读到某个数"的用例如果退回
    /// 折成 `None` 的写法，就必须红在这里而不是绿着往下走。
    fn saved(day: NaiveDate) -> Option<i64> {
        saved_count_on(day).expect("这一问本该答得出来")
    }

    /// 把"Edge 的库"指向夹具文件，析构时还原（panic 时也不会留给下一个用例）。
    struct EdgeFixture;
    impl Drop for EdgeFixture {
        fn drop(&mut self) {
            *EDGE_PATH_OVERRIDE.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    /// 临时副本路径的夹具：析构时还原（panic 时也不会留给下一个用例）。
    struct TempCopyFixture;
    impl Drop for TempCopyFixture {
        fn drop(&mut self) {
            *TEMP_COPY_OVERRIDE.lock().unwrap_or_else(|e| e.into_inner()) = None;
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

    /// 快照的 sidecar：`-wal` 搬不成必须报错，`-shm` 拿不到无所谓。
    #[test]
    fn snapshot_copy_requires_the_wal_but_not_the_shm() {
        let _lock = crate::paths::test_app_dir_lock();
        let app = crate::paths::test_app_dir("edge_copy_sidecar");
        let src = app.path().join("History");
        std::fs::write(&src, b"main").unwrap();
        std::fs::write(crate::data_location::with_suffix(&src, "-wal"), b"wal").unwrap();

        let temp = app.path().join("snap_ok.db");
        try_snapshot_copy(&src, &temp).expect("主库与 WAL 都在时应复制成功");
        assert!(temp.exists());
        assert!(
            crate::data_location::with_suffix(&temp, "-wal").exists(),
            "WAL 必须一起搬走"
        );
        remove_temp_copy(&temp);
        assert!(
            !temp.exists() && !crate::data_location::with_suffix(&temp, "-wal").exists(),
            "清理必须把 sidecar 一起带走"
        );

        // 目标 -wal 被占成目录 ⇒ 这份副本不完整，调用方不许拿去查
        let temp = app.path().join("snap_bad.db");
        let bad_wal = crate::data_location::with_suffix(&temp, "-wal");
        std::fs::create_dir_all(&bad_wal).unwrap();
        let err = try_snapshot_copy(&src, &temp).expect_err("-wal 复制失败不能当成功");
        assert!(err.contains("-wal"), "要说是哪一步败了: {err}");
        remove_temp_copy(&temp);
        let _ = std::fs::remove_dir_all(&bad_wal);

        // -shm 只是索引，SQLite 会照 WAL 重建 ⇒ 失败不影响整份快照
        let src2 = app.path().join("History2");
        std::fs::write(&src2, b"main").unwrap();
        std::fs::create_dir_all(crate::data_location::with_suffix(&src2, "-shm")).unwrap();
        let temp2 = app.path().join("snap_shm.db");
        try_snapshot_copy(&src2, &temp2).expect("-shm 失败不该毁掉整份快照");
        assert!(temp2.exists());
        remove_temp_copy(&temp2);
    }

    /// 本地缓存那一问也必须是三态：「说不出在不在」不许被读成"从没刷过"。
    ///
    /// 这个函数自己的注释就写着"不能靠文件在不在判"，而它第一句用的正是 `exists()`。
    /// 界面后果：峰值显示 "—"、趋势图空白，读起来与"这个功能上线后一次都没刷过"一模一样，
    /// 而真实原因是那次 stat 失败了 —— 插件因此不会去刷新，用户也等不到自愈。
    #[test]
    fn the_edge_cache_read_does_not_guess_from_file_existence() {
        let _lock = crate::paths::test_app_dir_lock();
        // 对照组：缓存库真的还不存在 ⇒ 空表（不是错误），这条不许被矫枉过正改掉
        let app = crate::paths::test_app_dir("edge_cache_absent_ok");
        let got = get_edge_history_counts(30).expect("没有缓存库是真答案，不是读不出来");
        assert!(got.is_empty(), "全新那一份就该回空表，实得 {got:?}");
        assert!(
            !edge_db_path().exists(),
            "夹具前提：这一路不该把缓存库创建出来: {}",
            edge_db_path().display()
        );
        drop(app);

        let src = include_str!("edge_history.rs");
        // ⚠ 本文件的**生产代码里就有 `#[cfg(test)]`**（测试夹具那几处），
        // 所以不能按第一次出现切：那会把整段生产代码切成第 20 行之前那一小截。
        let prod = &src[..src.find("\nmod tests {").expect("测试模块的起点找不到了")];
        // 先摘注释行：这些说明文字里本来就写着 `exists()` 与"文件在不在"
        let body = |head: &str| -> String {
            let at = prod.find(head).expect("这个函数找不到了（判据要跟着改）");
            let raw = &prod[at..at + prod[at..].find("\n}").expect("函数体切不出来")];
            raw.lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        // 三条读路径共用一份尺子，谁都不许再自己 `match std::fs::metadata`
        for head in [
            "pub fn get_edge_history_counts(",
            "fn saved_count_on(",
            "pub fn get_edge_history_saved_total(",
        ] {
            let b = body(head);
            assert!(
                !b.contains(".exists()"),
                "又用 exists() 猜在不在 —— 第三种结局会被读成「从没刷过」: {head}"
            );
            assert!(
                b.contains("local_cache_present()?"),
                "这一问必须走那把共用的三态尺子: {head}"
            );
        }
        assert_eq!(
            prod.matches("local_cache_present()?").count(),
            3,
            "这一问在读侧该恰好三处"
        );
        let ruler = body("fn local_cache_present(");
        assert!(
            ruler.contains("is_confirmed_absent(&e)"),
            "「确认没有」的判据不许在文件里各写一遍"
        );
        assert!(
            ruler.contains("说不出本地 Edge 缓存库在不在"),
            "说不出那一支要交 Err，让面板说得出为什么"
        );
        // 顺序也是判据的一半：`open_local()` 会建目录、`Connection::open` 会建库文件
        let total_body = body("pub fn get_edge_history_saved_total(");
        assert!(
            total_body.find("local_cache_present()?").unwrap()
                < total_body.find("open_local()").unwrap(),
            "先开库再问在不在 ⇒ 挂件每渲染一次就凭空建出一个空缓存库"
        );
    }

    /// 明文副本删不掉时必须点名，而且"本来就没有"不许被报成"有一份清不掉"。
    ///
    /// 这条链自己的注释（`SweepReport` 那段）立的规矩是"删不动与没得删必须分开说"，
    /// 当时只落了**启动清扫**那半边；正常路径每轮兜底都走这里，原先是三处 `let _ =`。
    /// 而 `cleanup_stale_copies_once` 一个进程只跑一次、门槛又是一小时 ⇒ 此刻删不动的
    /// 这份明文浏览记录，在本次运行里没有任何机制会再碰它 —— 日志还一个字没有。
    #[test]
    fn an_undeletable_plaintext_copy_is_named_not_swallowed() {
        let _lock = crate::paths::test_app_dir_lock();
        let app = crate::paths::test_app_dir("edge_temp_stuck");
        let temp = app.path().join("snap_stuck.db");
        let side = crate::data_location::with_suffix(&temp, "-wal");
        std::fs::write(&temp, b"plaintext browsing history").unwrap();
        std::fs::write(&side, b"wal").unwrap();

        // 正向腿：返回值不是摆设 —— 两份都删得动时清单是空的，且 sidecar 一起带走
        let stuck = remove_temp_copy(&temp);
        assert!(stuck.is_empty(), "两份都该删动，实得 {stuck:?}");
        assert!(
            !temp.exists() && !side.exists(),
            "清理必须把 sidecar 一起带走"
        );

        // 文件已经不在了 ⇒ 那是"没得删"，不许报成"有一份清不掉"（假警）
        let stuck = remove_temp_copy(&temp);
        assert!(stuck.is_empty(), "NotFound 不是删不动: {stuck:?}");

        // 真删不动：只放行读的句柄握着它（std 默认连 FILE_SHARE_DELETE 也共享 ⇒
        // 必须显式 share_mode(1)，那才是杀软实时扫描/另一个实例握着它的形态）
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::write(&temp, b"plaintext browsing history").unwrap();
            let blocker = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(1)
                .open(&temp)
                .expect("夹具：只放行读的句柄要能开起来");
            let (stuck, logs) = crate::logger::capture_logs(|| discard_temp_copy(&temp));
            assert_eq!(stuck.len(), 1, "主文件删不动要点名一份，实得 {stuck:?}");
            assert!(
                stuck[0].contains(&temp.display().to_string()),
                "要说清是哪一份留在临时目录里: {stuck:?}"
            );
            assert!(
                logs.iter()
                    .any(|l| l.contains("删不掉") && l.contains("明文")),
                "收尾这一步也要出声，否则这份隐私留到下次启动都没人知道: {logs:?}"
            );
            drop(blocker);
        }
    }

    /// 缺 WAL 的副本不许拿去查：补档历史日会把偏小的数永久写进本地缓存。
    ///
    /// 夹具：第一次查询返回 `None` 强制走复制兜底；源侧放一个 `-wal`，目标 `-wal`
    /// 用目录占住让复制必失败。旧写法吞掉失败后照查主库（2 行）⇒ 返回 `Some(2)`；
    /// 现在必须返回 `None`（这一轮没拿到可信快照，下一轮再试）。
    #[test]
    fn a_snapshot_without_its_wal_is_not_used() {
        let _lock = crate::paths::test_app_dir_lock();
        let app = crate::paths::test_app_dir("edge_no_wal_snapshot");
        let _fx = write_edge_fixture("no_wal", &[1, 2]);
        let src = edge_history_path();
        std::fs::write(crate::data_location::with_suffix(&src, "-wal"), b"tail").unwrap();

        let temp = app.path().join("snap.db");
        let bad_wal = crate::data_location::with_suffix(&temp, "-wal");
        std::fs::create_dir_all(&bad_wal).unwrap();
        *TEMP_COPY_OVERRIDE.lock().unwrap_or_else(|e| e.into_inner()) = Some(temp.clone());
        let _copy = TempCopyFixture;

        let first = std::cell::Cell::new(true);
        let got = with_edge_snapshot(|conn| {
            if first.replace(false) {
                return None;
            }
            conn.query_row("SELECT COUNT(*) FROM urls", [], |r| r.get::<_, i64>(0))
                .ok()
        });
        assert!(
            matches!(got, Snapshot::Unreadable(_)),
            "缺 WAL 的副本被拿去查了；补档历史日会把数永久写小。实得 {got:?}"
        );
        let _ = std::fs::remove_dir_all(&bad_wal);
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
        save_edge_history_count(two, 7).unwrap();
        let rows = [
            chrome_start_of(today),
            chrome_start_of(today) + 500_000,
            chrome_start_of(y) + 3_000_000,
            chrome_start_of(two) + 3_000_000,
        ];
        let _fx = write_edge_fixture("backfill", &rows);

        SNAPSHOT_ROUNDS.store(0, std::sync::atomic::Ordering::Relaxed);
        let (res, today_count, total) = update_today_edge_history();
        let rounds = SNAPSHOT_ROUNDS.load(std::sync::atomic::Ordering::Relaxed);

        assert!(res.is_ok(), "夹具可读，刷新不该失败");
        assert_eq!((today_count, total), (2, 4));
        assert_eq!(rounds, 1, "今日数+总数+29 天补档必须共用一轮快照");
        // 同步补齐：函数返回时就该已经落库，不是"回头某个时刻出现"
        assert_eq!(saved(y), Some(1), "缺失日应在本次调用内补齐");
        assert_eq!(saved(today), Some(2));
        assert_eq!(
            saved(two),
            Some(7),
            "这一行是刚刚写下的（`updated_at` 已在该日之后）→ 已盖完，不该重查"
        );
        // 再刷新一次：没有缺失日了就完全不必再动补档查询
        assert!(update_today_edge_history().0.is_ok());
        assert_eq!(
            SNAPSHOT_ROUNDS.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "补齐过后第二次刷新仍是一轮快照"
        );
    }

    /// 读到了数、一条也没落库时，本轮刷新不许报成功。
    ///
    /// 旧形状：ok 位只由**读侧**那一趟查询决定，`save_edge_history_count` /
    /// `save_edge_history_meta` 里的失败全被吞（`Err(_) => return`、`let _ =`）。
    /// 于是本地缓存库打不开时（被第二个实例锁住、同步盘占用、`data_dir` 不可写）
    /// `refresh_state()` 仍是 `ok`，插件却按 `get_edge_history_saved_*` 读到 None、
    /// 把「今日记录数 / 总记录数」画成 "—" —— "刷新成功、数据为空"与"真的没数据"
    /// 在界面上长成同一个样子（B15 同族）。
    /// 注回旧写法（ok 位不看落盘结果）时，前两条断言里红在 `assert!(!ok)`。
    /// 第三条是另一场改的口径：那个夹具把缓存库的位置占成了目录，读侧现在要交回
    /// 「问不出来＋原因」，不再冒充"今天没刷过"。
    #[test]
    fn a_refresh_that_cannot_persist_does_not_report_ok() {
        let _lock = crate::paths::test_app_dir_lock();
        let _app = crate::paths::test_app_dir("edge_unwritable");
        // 用目录占住本地缓存库的位置：`Connection::open` 必失败，一条也写不进去
        std::fs::create_dir_all(edge_db_path()).unwrap();
        let today = Local::now().date_naive();
        let _fx = write_edge_fixture("unwritable", &[chrome_start_of(today)]);

        let (res, today_count, total) = update_today_edge_history();
        assert_eq!(
            (today_count, total),
            (1, 1),
            "Edge 库可读，数值该照原样交回调用方（别降级成 0）"
        );
        assert_eq!(
            res,
            Err(RefreshFail::Write),
            "Edge 读到了、一条也没落库：本轮该算「写不进」，不是「读不到」——\
             挂件照后一句提示就会让用户去关 Edge"
        );
        assert!(
            get_edge_history_saved_today().is_err(),
            "缓存库的位置被目录占住 ⇒ 这一问问不出来，不许折成「今天没刷过」"
        );

        // 界面读的是状态位：那一轮既不该留在 ok，也不该被说成"Edge 读不到"
        reset_slot();
        let gen = claim_round(std::time::Instant::now()).expect("空槽位该能占用");
        finish_round(gen, res);
        assert_eq!(
            refresh_state(),
            "write_fail",
            "没落库的那一轮不该把界面留在 ok，也不该混进「Edge 被占用」"
        );
    }

    /// 直接按指定 `updated_at` 写一行本地缓存。
    /// `save_edge_history_count` 只会写"此刻"，而这几条用例要模拟的正是
    /// "这一行是当天还没过完时写下的"，只能绕过它自己插。
    fn seed_saved_row(day: NaiveDate, count: i64, updated_at: i64) {
        let conn = open_local().expect("本地缓存库应能打开");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS edge_history (
                date TEXT PRIMARY KEY,
                count INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO edge_history (date, count, updated_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![day.format("%Y-%m-%d").to_string(), count, updated_at],
        )
        .unwrap();
    }

    /// 昨天那一行是"昨天还没过完"时留下的 → 它只盖到当天最后一次刷新，
    /// 剩下的几个小时永久丢失。旧判据只问"有没有行"，于是那一天**永远**不再重查：
    /// 早上刷一次，那一天的趋势值就永远停在半天。
    ///
    /// 两个方向都要钉住：
    /// ① 还没盖完的日子要重查（`y`：2 → 3，深夜那条被捞回来）；
    /// ② 已经盖完的日子不许重查（`d2`：存着 42、真实只有 1，重查会把它打回 1）。
    /// 注回旧判据（行存在即完整）会红在 ①。
    #[test]
    fn a_day_saved_before_it_ended_is_requeried_once() {
        let _lock = crate::paths::test_app_dir_lock();
        let _app = crate::paths::test_app_dir("edge_stale_day");
        let today = Local::now().date_naive();
        let y = today - chrono::Days::new(1);
        let d2 = today - chrono::Days::new(2);
        let hour = 3_600 * 1_000_000; // Chrome 时间戳是微秒
        let _fx = write_edge_fixture(
            "staleday",
            &[
                chrome_start_of(y) + 8 * hour, // 上午两条：旧行已经算进去了
                chrome_start_of(y) + 9 * hour,
                chrome_start_of(y) + 23 * hour, // 深夜这条：旧行没算到
                chrome_start_of(d2) + 12 * hour,
                chrome_start_of(today) + hour,
            ],
        );
        let y_end = local_midnight(y.succ_opt().unwrap()).unwrap().timestamp();
        // 昨天：当天 23:00 写的 → 该日还没盖完
        seed_saved_row(y, 2, y_end - 3_600);
        // 前天：昨天凌晨写的 → 已经盖完（真实值只有 1，重查就会把 42 打回去）
        seed_saved_row(d2, 42, y_end + 3_600);

        let (res, today_count, total) = update_today_edge_history();
        assert!(res.is_ok(), "夹具可读，刷新不该失败");
        assert_eq!((today_count, total), (1, 5));
        assert_eq!(saved(y), Some(3), "深夜那条必须被捞回来");
        assert_eq!(saved(d2), Some(42), "已盖完的日子不该被重查改写");

        // 重查过一次就算盖完了：下一轮不该再回头动它
        assert!(update_today_edge_history().0.is_ok());
        assert_eq!(saved(y), Some(3), "每个历史日只该被重查一次");
        assert_eq!(saved(d2), Some(42));
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
        assert_eq!(removed.removed, 2, "只该删掉那两份超龄的 Edge 副本");
        assert!(
            removed.stuck.is_empty() && removed.unreadable.is_none(),
            "这条走的是最顺的那一支，另外两种结局不该同时出现: {removed:?}"
        );
        assert!(
            !stale_db.exists() && !stale_wal.exists(),
            "超龄副本要真删掉"
        );
        assert!(fresh.exists(), "几秒大的副本可能正被另一个实例用着");
        assert!(other.exists(), "前缀不匹配的文件不该被动");
    }

    /// 「目录读不出来」「里面没得清」「有一份清不动」三件事不许共用一个 0。
    ///
    /// 这条清扫管的是异常退出留在临时目录的**明文浏览记录**：原先只回一个 `usize`
    /// 且调用方只在 `> 0` 时说话，于是"读不动/删不动"这两种真结局与"本来就没有"
    /// 同形，那份隐私数据可以一直留着而日志一个字都没有。
    #[test]
    fn the_sweep_tells_an_unreadable_dir_apart_from_an_undeletable_copy() {
        let _lock = crate::paths::test_app_dir_lock();
        let app = crate::paths::test_app_dir("edge_sweep_kinds");

        // ① 目录读不出来：把这个位置做成**普通文件**（read_dir 对文件必然报错）
        let blocked = app.path().join("blocked");
        std::fs::write(&blocked, b"x").unwrap();
        let r = sweep_stale_temp_copies_in(&blocked, STALE_TEMP_COPY_AGE);
        assert_eq!(r.removed, 0, "读不出来时不该声称清掉了什么");
        assert!(
            r.unreadable.is_some(),
            "「目录读不出来」必须单独交出去，不许折成「里面没有超龄副本」: {r:?}"
        );
        assert!(
            r.stuck.is_empty(),
            "连目录都没读到，谈不上某一份删不动: {r:?}"
        );

        // ② 超龄却删不动：用一个只放行读的句柄握着它。std 默认连 FILE_SHARE_DELETE
        //    也共享了 ⇒ 必须显式 share_mode(1)，那才是杀软实时扫描/另一个实例握着它的形态。
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let dir = app.path().join("sweep2");
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join("focusflow_edge_55555_666666.db");
            std::fs::write(&p, b"plaintext browsing history").unwrap();
            std::fs::OpenOptions::new()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(7200))
                .unwrap();
            let blocker = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(1)
                .open(&p)
                .expect("夹具：只放行读的句柄要能开起来");
            let r = sweep_stale_temp_copies_in(&dir, STALE_TEMP_COPY_AGE);
            assert_eq!(r.removed, 0, "夹具前提：这一份删不动");
            assert_eq!(
                r.stuck.len(),
                1,
                "「有一份清不动」要点名到那一份，不能静默地报 0: {r:?}"
            );
            assert!(
                r.unreadable.is_none(),
                "目录是读得到的，两种原因不许混着说: {r:?}"
            );
            drop(blocker);
        }
    }

    /// 把槽位置成「有一轮在 `start` 这一时刻进入 RUNNING」，返回那一轮的世代号。
    /// 每次改槽位都推进世代号，这样上一轮遗留的后台线程（`plugins_test` 真的会
    /// 点一次 refresh）也就没法反过来改写测试中的状态。
    ///
    /// 时刻一律"从此刻往前起算、再往后加"，不做 `Instant::now() - 3 分钟`：
    /// `Instant` 是单调时钟（Windows 上是开机以来的计数），刚开机的机器上往回减会下溢。
    fn fake_running_round(start: std::time::Instant) -> u64 {
        let mut slot = lock_slot();
        slot.state = ST_RUNNING;
        slot.started_at = Some(start);
        slot.gen += 1;
        slot.gen
    }

    fn reset_slot() {
        let mut slot = lock_slot();
        slot.state = ST_IDLE;
        slot.started_at = None;
        slot.gen += 1;
    }

    /// 卡死判定的时长只能由**两个单调时刻相减**得出，墙上时钟不参与。
    ///
    /// 旧实现取 `Utc::now().timestamp_millis()`：对表/休眠回来同步时钟时，前跳会把一
    /// 个**正在正常跑**的轮次算成"已经跑了 2 分钟"而去接管 → 两个线程各复制一份
    /// 100MB；后跳则让 `now - started` 饱和成 0，按钮卡在「正在后台读取」几小时。
    /// 这条把四个边界钉死：差 1 毫秒不算、恰好到阈值算、`now` 早于本轮起点（回拨在
    /// 旧表示里造出的形状）不算也不下溢、没有起点不算。
    /// 想退回墙上时钟 —— `is_wedged` 收的是 `Instant`，改回去就编译不过去。
    #[test]
    fn wedge_verdict_is_pure_monotonic_elapsed() {
        let _lock = crate::paths::test_app_dir_lock();
        let base = std::time::Instant::now();
        let slot = RefreshSlot {
            state: ST_RUNNING,
            gen: 7,
            started_at: Some(base),
        };
        assert!(
            !is_wedged(
                &slot,
                base + RUNNING_STALE - std::time::Duration::from_millis(1)
            ),
            "差 1 毫秒到阈值仍算正常在跑"
        );
        assert!(is_wedged(&slot, base + RUNNING_STALE), "恰好到阈值才算卡死");
        assert!(!is_wedged(&slot, base), "刚起头就查，不该算卡死");
        let rolled_back = RefreshSlot {
            state: ST_RUNNING,
            gen: 7,
            // 本轮起点"在未来"：正是时钟后跳时旧实现看到的形状
            started_at: Some(base + std::time::Duration::from_secs(3600)),
        };
        assert!(
            !is_wedged(&rolled_back, base),
            "拿到的 now 比本轮起点还早时该饱和成 0，既不下溢也不误判成卡死"
        );
        assert!(
            !is_wedged(
                &RefreshSlot {
                    state: ST_RUNNING,
                    gen: 7,
                    started_at: None,
                },
                base + RUNNING_STALE * 2
            ),
            "没有起点就没有'拖过头'的依据"
        );
    }

    /// 线程卡死（例如回不来地卡在 `fs::copy`）后，刷新按钮不该死到重启为止。
    #[test]
    fn wedged_refresh_self_heals_so_the_button_stays_usable() {
        let _lock = crate::paths::test_app_dir_lock();
        let base = std::time::Instant::now();
        fake_running_round(base);
        assert_eq!(
            refresh_state_at(base + RUNNING_STALE + std::time::Duration::from_secs(60)),
            "fail",
            "越过阈值的 running 必须自愈，否则面板永远显示「正在后台读取」"
        );
        assert!(
            claim_round(base + RUNNING_STALE + std::time::Duration::from_secs(61)).is_some(),
            "自愈之后下一次点击要真能启动新一轮"
        );
        reset_slot();
    }

    /// 阈值内是"正常在跑"，不能拒绝第二次点击之外的接管，也不能放行第二轮并发。
    #[test]
    fn live_round_is_not_stolen_but_a_wedged_one_is() {
        let _lock = crate::paths::test_app_dir_lock();
        let base = std::time::Instant::now();
        fake_running_round(base);
        assert_eq!(
            refresh_state_at(base + std::time::Duration::from_secs(1)),
            "running",
            "才跑 1 秒不该被判成卡死"
        );
        assert!(
            claim_round(base + std::time::Duration::from_secs(2)).is_none(),
            "上一轮还在正常跑时再起一轮 = 两个线程各复制 100MB"
        );

        fake_running_round(base);
        assert!(
            claim_round(base + RUNNING_STALE + std::time::Duration::from_millis(1)).is_some(),
            "超过阈值就该接管，而不是到重启前都点不动"
        );
        reset_slot();
    }

    /// 被接管的那一轮之后就算真的回来，也不许改写新一轮的状态。
    #[test]
    fn superseded_round_cannot_write_back_its_result() {
        let _lock = crate::paths::test_app_dir_lock();
        let base = std::time::Instant::now();
        let zombie = fake_running_round(base);
        let live = claim_round(base + RUNNING_STALE + std::time::Duration::from_millis(1))
            .expect("接管应成功");
        assert_ne!(zombie, live);
        finish_round(zombie, Ok(()));
        assert_eq!(
            refresh_state_at(base + RUNNING_STALE + std::time::Duration::from_secs(1)),
            "running",
            "僵尸线程回来不该把新一轮标成已完成"
        );
        finish_round(live, Ok(()));
        assert_eq!(refresh_state_at(base + RUNNING_STALE * 2), "ok");
        reset_slot();
    }

    /// 两种失败在状态通道上必须是两个值。
    ///
    /// 挂件那句提示完全按这个串分：折成一个 `fail` 时，"Edge 读到了但本程序的
    /// `data/` 写不进"会被说成"Edge 被占用，请稍后重试" —— 用户于是反复去关 Edge，
    /// 而真正要处理的是目录权限/同步盘占位。Rust 侧日志本来就分得开，
    /// 丢在 API 边界上等于没分。
    #[test]
    fn the_two_edge_failures_are_told_apart_in_the_state_channel() {
        let _lock = crate::paths::test_app_dir_lock();
        let base = std::time::Instant::now();

        let gen = claim_round(base).expect("这一轮该被接下");
        finish_round(gen, Err(RefreshFail::Read));
        assert_eq!(
            refresh_state_at(base),
            "fail",
            "Edge 读不到才是那句「请稍后重试」"
        );
        reset_slot();

        let gen = claim_round(base).expect("这一轮该被接下");
        finish_round(gen, Err(RefreshFail::Write));
        assert_eq!(
            refresh_state_at(base),
            "write_fail",
            "读到了但没落库得是另一个值，否则挂件给的建议是错的"
        );
        reset_slot();
    }

    /// 跨语言契约：内置挂件对每个要提示的状态都得有句话。
    ///
    /// 少一条不报错，症状只是"面板上什么都不说" —— 那正是这一族缺陷的形状。
    /// 顺带钉住 `write_fail` 那句话不许叫用户去关 Edge。
    #[test]
    fn the_bundled_widget_has_a_line_for_each_failing_state() {
        let lua = include_str!("../plugins/edge_history_plugin.lua");
        for state in ["running", "fail", "write_fail", "no_source"] {
            let needle = format!("state == \"{state}\"");
            assert!(
                lua.contains(&needle),
                "挂件没有处理 `edge_refresh_state` 交出的 {state}：{needle}"
            );
        }
        let at = lua
            .find("state == \"write_fail\"")
            .expect("上一轮已经确认有了");
        // 只取这一支（到下一个 `elseif` 之前）：下一支 `fail` 的提示里正好有
        // "Edge 后台进程" 那句，整段去比会把两支混成一支
        let rest = &lua[at..];
        let chunk = &rest[..rest.find("elseif").unwrap_or(rest.len())];
        assert!(
            !chunk.contains("Edge 后台进程"),
            "写不进自己缓存那一支不该叫用户去关 Edge：{chunk}"
        );
        assert!(
            chunk.contains("写不进去"),
            "要把问题指到本程序的统计目录上：{chunk}"
        );

        // no_source 那一支不许复用 `fail` 的话术：路径是写死的 `User Data\Default`，
        // 没装 Edge / 别的 Profile / 便携版都会走到这里，而"关掉 Edge 再试"对它们没用。
        let ns_at = lua
            .find("state == \"no_source\"")
            .expect("挂件没有 no_source 这一支");
        let ns_rest = &lua[ns_at..];
        let ns_chunk = &ns_rest[..ns_rest.find("end").expect("这一支没有收尾")];
        assert!(
            !ns_chunk.contains("请稍后重试") && !ns_chunk.contains("Edge 后台进程"),
            "没找到库那一支不该叫用户去关 Edge、再试一次: {ns_chunk}"
        );
        assert!(
            ns_chunk.contains("Profile"),
            "要说清这是路径／Profile 的事: {ns_chunk}"
        );
    }

    /// 「没找到 Edge 的历史库」与「找到了但读不动」必须是两种结局。
    ///
    /// 原先 `with_edge_snapshot` 回 `Option`：那条 `if !path.exists() { return None }`
    /// 连一行日志都不写，而调用方把两种 None 并成一句「被占用或不可读…请稍后重试」。
    /// 于是没装 Edge、用的是 `Profile 1`、或便携版把数据放在别处的机器上，
    /// 用户被反复支去关浏览器，日志里却始终一个字没有。
    #[test]
    fn a_missing_edge_history_is_not_reported_as_a_locked_one() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("edge_no_source");

        // —— 确认没有 ⇒ Absent，且要点名路径、出声 ——
        let missing = paths::data_dir().join("this_edge_history_is_not_there.db");
        *EDGE_PATH_OVERRIDE.lock().unwrap_or_else(|e| e.into_inner()) = Some(missing.clone());
        let _fx = EdgeFixture;
        let (outcome, logs) = crate::logger::capture_logs(|| {
            with_edge_snapshot(|conn| {
                conn.query_row("SELECT COUNT(*) FROM urls", [], |r| r.get::<_, i64>(0))
                    .ok()
            })
        });
        let Snapshot::Absent(shown) = outcome else {
            panic!("文件本来不存在必须归 Absent，实得 {outcome:?}");
        };
        assert!(
            shown.contains("this_edge_history_is_not_there"),
            "要说清去找哪个路径没找到: {shown}"
        );
        assert!(
            logs.iter()
                .any(|l| l.contains("没找到") && !l.contains("占用")),
            "这一条要出声，而且不许顺口说成被占用: {logs:?}"
        );

        // 面板拿到的状态也得分开：挂件那句"去关 Edge"来自 fail
        let (res, today, total) = update_today_edge_history();
        assert_eq!(
            res,
            Err(RefreshFail::NoSource),
            "没找到库不是 Read 失败，挂件那句建议对它是错的"
        );
        assert_eq!((today, total), (0, 0), "什么都没读到时就该交回 0");
        // 状态串是挂件唯一能分开的东西：`update_today_edge_history` 自己不动槽位
        // （那是线程收尾时 `finish_round` 的事），所以这一条要按同样的顺序走一遍。
        let gen = fake_running_round(std::time::Instant::now());
        finish_round(gen, res);
        assert_eq!(
            refresh_state(),
            "no_source",
            "没找到库要和\"被占用\"分成两个状态串，否则挂件那句还是同一句话"
        );

        // 对照腿：Edge 库真在、能读 ⇒ 仍走 Value（别把这条修成见谁都报 no_source）
        let _fx2 = write_edge_fixture("present", &[chrome_start_of(Local::now().date_naive())]);
        let got = with_edge_snapshot(|conn| {
            conn.query_row("SELECT COUNT(*) FROM urls", [], |r| r.get::<_, i64>(0))
                .ok()
        });
        assert!(
            matches!(got, Snapshot::Value(1)),
            "读得到时必须照常交回数值，实得 {got:?}"
        );
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

        assert_eq!(
            get_edge_history_saved_today().unwrap(),
            None,
            "从未刷新过不该有值"
        );
        assert!(
            !edge_db_path().exists(),
            "只是问一句「今天刷过没有」，不该把缓存库创建出来: {}",
            edge_db_path().display()
        );
        assert_eq!(
            get_edge_history_saved_total().unwrap(),
            None,
            "总数同一口径：确认没刷过是 `Ok(None)`，不是「问不出来」"
        );
        assert!(
            !edge_db_path().exists(),
            "总数那条原先连「在不在」都不问就 `open_local()` —— 挂件每渲染一次就凭空\
             建出一个空缓存库: {}",
            edge_db_path().display()
        );
        save_edge_history_count(yesterday, 111).unwrap();
        assert_eq!(
            get_edge_history_saved_today().unwrap(),
            None,
            "昨天的计数不该被当成今天"
        );
        save_edge_history_count(today, 42).unwrap();
        assert_eq!(get_edge_history_saved_today().unwrap(), Some(42));
        save_edge_history_meta("total", 900).unwrap();
        assert_eq!(get_edge_history_saved_total().unwrap(), Some(900));
    }

    /// 「缓存库此刻读不出来」与「这个功能还没刷过」是两种结局。
    ///
    /// 旧形状把三种失败全折成同一个 `None`：`edge_db_path().exists()`（stat 自己失败读成
    /// "没有"）、`open_local().ok()?`（库打不开读成"没刷过"）、`query_row(..).ok()`
    /// （这一行查不动读成"没刷过"）。宿主再 `unwrap_or(0)`，挂件那句
    /// `today and tostring(today) or "—"` ⇒ "你的缓存库读不动"被说成"你从没用过这个功能"。
    /// 前者的下一步是让统计目录可写，后者的下一步是点「刷新数据」。
    /// 「还没建这张表」仍归 `Ok(None)`（还没建 ≠ 坏了），与 `CategoryType` 同一把尺子。
    #[test]
    fn the_saved_number_channels_answer_in_three_states() {
        let _lock = crate::paths::test_app_dir_lock();
        let _app = crate::paths::test_app_dir("edge_saved_states");
        let today = Local::now().date_naive();
        // 把缓存库的位置占成目录：`metadata` 答"有"，`Connection::open` 必失败
        std::fs::create_dir_all(edge_db_path()).unwrap();

        let e = get_edge_history_saved_today().expect_err("位置被占住时不许回 Ok(None)");
        assert!(
            e.contains("打开本地 Edge 缓存库失败"),
            "要说清是哪一步问不出来: {e}"
        );
        assert!(
            get_edge_history_saved_total().is_err(),
            "总数那一条同理：打不开就是打不开"
        );

        // 撤掉占位 ⇒ 确认没有这个库，必须是 `Ok(None)`（别矫枉过正成见谁都报错）
        std::fs::remove_dir(edge_db_path()).unwrap();
        assert_eq!(
            get_edge_history_saved_today().unwrap(),
            None,
            "真没刷过时不许说「读不出来」"
        );
        assert_eq!(get_edge_history_saved_total().unwrap(), None);

        // 库建出来了但还没有 `edge_history` 这张表 = 还没刷过，不是读不动
        Connection::open(edge_db_path()).unwrap();
        assert_eq!(
            get_edge_history_saved_today().unwrap(),
            None,
            "还没这张表 ⇒ 确认没刷过"
        );
        // 有 `edge_history`、还没有 meta 那一格：总数也该是"没刷过"
        save_edge_history_count(today, 7).unwrap();
        assert_eq!(saved(today), Some(7));
        assert_eq!(
            get_edge_history_saved_total().unwrap(),
            None,
            "还没有 meta 这张表 = 总数没存过，不该报「读不出来」"
        );
    }

    /// 缓存里 total 那格写的不是数字时要点名，不许再折成"没刷过"。
    ///
    /// 旧形状是 `.ok().and_then(|v| v.parse().ok())`：库里明明有一格、内容是坏的，
    /// 也被读成"没这一格" ⇒ 挂件画 "—"。而这与"从没刷过"下一步相反 ——
    /// 那种形状要清掉/重建本地缓存，点刷新只是白等。
    #[test]
    fn a_broken_total_cell_is_named_not_quietly_absent() {
        let _lock = crate::paths::test_app_dir_lock();
        let _app = crate::paths::test_app_dir("edge_total_garbage");
        let today = Local::now().date_naive();
        save_edge_history_count(today, 1).unwrap();
        {
            // 脏值不走生产入口（生产只会写数字）：形状是上一版程序留下的或手工改过的库
            let conn = Connection::open(edge_db_path()).unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
            )
            .unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('total', 'abc')",
                [],
            )
            .unwrap();
        }
        let e = get_edge_history_saved_total().expect_err("坏的那一格不能被读成「没刷过」");
        assert!(
            e.contains("不是数字") && e.contains("abc"),
            "要点名是哪一格写坏了: {e}"
        );
        // 对照腿：同一格写回数字 ⇒ 正常读出（别修成见谁都报错）
        save_edge_history_meta("total", 5).unwrap();
        assert_eq!(get_edge_history_saved_total().unwrap(), Some(5));
    }

    /// 「近 30 天」必须真的给满 30 天：库里没有的日子补 0。
    ///
    /// 旧实现把 SQLite 里恰好存在的那几行原样交给面板 —— 标题写「近 30 天」、表格里
    /// 只躺两三行，而且"这天没记录"和"这天没刷过"在界面上长得一模一样。
    /// 另一头也不能编造：一次都没刷新过时必须是**空表**，否则插件会把"没刷过"
    /// 显示成一整列 0（那正是这一场要连带修掉的"峰值恒 0"）。
    #[test]
    fn trend_zero_fills_missing_days_but_never_invents_history() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("edge_zero_fill");
        let today = Local::now().date_naive();

        assert!(
            get_edge_history_counts(30).unwrap().is_empty(),
            "一次都没刷新过时不该编出一张 30 天的表"
        );

        // 窗口之外的另一头：晚于今天的行。写法用的是产品自己的入口 —— 真实成因就是
        // "刷新发生在那一刻的『今天』"（本机时钟先往前调过、随后调回来；或把 `data/`
        // 从时钟超前的机器上复制过来），测试改不了系统时钟，只能把那种行按同一条路径插进去。
        // 只夹下界时它会把"窗口内没行 = 从没刷过"这个哨兵顶开，面板就从 "—"
        // 变成一整列 0、峰值显示 0。
        save_edge_history_count(today + chrono::Days::new(5), 33).unwrap();
        assert_eq!(
            get_edge_history_counts(30),
            Ok(Vec::new()),
            "库里只有晚于今天的那一行时，仍然必须判成「这个窗口没刷过」"
        );

        save_edge_history_count(today, 7).unwrap();
        save_edge_history_count(today - chrono::Days::new(3), 4).unwrap();
        // 窗口之外的老日子：存在库里，但不该挤进这张 30 天的表
        save_edge_history_count(today - chrono::Days::new(400), 999).unwrap();

        let counts = get_edge_history_counts(30).unwrap();
        assert_eq!(counts.len(), 30, "标题写近 30 天就得给满 30 行");
        assert!(
            !counts.iter().any(|(_, c)| *c == 999),
            "400 天前那行不该被算进近 30 天"
        );
        assert!(
            !counts.iter().any(|(_, c)| *c == 33),
            "晚于今天那行不该挤进这张表，也不该改变任何一天的值"
        );
        let fmt = |d: NaiveDate| d.format("%Y-%m-%d").to_string();
        assert_eq!(counts[0].0, fmt(today - chrono::Days::new(29)));
        assert_eq!(counts[29], (fmt(today), 7), "今天那行用存进去的值");
        assert_eq!(
            counts[26],
            (fmt(today - chrono::Days::new(3)), 4),
            "3 天前那行用存进去的值"
        );
        assert_eq!(
            counts.iter().filter(|(_, c)| *c == 0).count(),
            28,
            "缺的日子一律补 0"
        );
        for w in counts.windows(2) {
            let prev = NaiveDate::parse_from_str(&w[0].0, "%Y-%m-%d").expect("日期格式");
            let next = NaiveDate::parse_from_str(&w[1].0, "%Y-%m-%d").expect("日期格式");
            assert_eq!(
                next,
                prev.succ_opt().unwrap(),
                "必须逐日连续且升序：{:?}",
                w.iter().map(|e| e.0.as_str()).collect::<Vec<_>>()
            );
        }
    }

    /// A④ 之一（§十五 第 17 条）：本地缓存读不动与"一次都没刷过"必须是两种结果。
    ///
    /// 旧写法两处 `return Vec::new()`（打开失败、查询失败）与"库里没有行"都回同一张
    /// 空表，于是面板上的 "—" 有两种含义，与上一场修掉的"落盘失败仍报刷新成功"同族。
    #[test]
    fn unreadable_cache_is_not_reported_as_never_refreshed() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("edge_trend_readfail");

        // 读得到、库里就是没有行 —— 这才是"从没刷过"
        assert_eq!(
            get_edge_history_counts(30),
            Ok(Vec::new()),
            "一次都没刷新过应当是 Ok(空表)"
        );

        // 读不动：把本地缓存库的路径占成目录，open 必失败
        std::fs::create_dir_all(edge_db_path()).expect("占位目录应能建出来");
        let problem = get_edge_history_counts(30).expect_err("读不出内容不该被说成「没刷过」");
        assert!(
            problem.contains("打开本地缓存库失败"),
            "错误要说清是哪一步读不动: {problem}"
        );
    }

    /// A④ 之二（§十五 第 18 条）：补档判据读不动时跳过本轮，而不是把整窗判成陈旧。
    #[test]
    fn unreadable_backfill_probe_skips_the_round_instead_of_refilling_every_day() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("edge_stale_probe");
        let today = Local::now().date_naive();

        // 对照腿（改动前后都必须绿）：读得到但没有行时，29 个历史日全该补档
        assert_eq!(
            stale_days_before(today, 30).len(),
            29,
            "从没刷过时，窗口里每个历史日都该被挑出来补档"
        );

        // 读不动：把本地缓存库的路径占成目录，open 必失败。先删掉对照腿里
        // `open_local()` 已经创建出来的那个空库文件，否则同名占位建不出来。
        std::fs::remove_file(edge_db_path()).ok();
        std::fs::create_dir_all(edge_db_path()).expect("占位目录应能建出来");
        assert!(
            stale_days_before(today, 30).is_empty(),
            "判据读不出来时本轮不该补档 —— 那要留给下一轮"
        );
    }
}
