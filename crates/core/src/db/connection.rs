//! SQLite 连接管理。
//!
//! - WAL + synchronous=NORMAL + busy_timeout + cache_size + WAL 上限
//! - 聚合存储：`daily_counts` / `hourly_counts` / `key_counts` 三张按天聚合表
//!   （每条按键不再落明细行，体积约为原来的 1/170）
//! - 设备维度：`devices` 是整数 id 字典表，`device_counts` / `device_key_counts`
//!   只存 `device_id` —— 设备实例路径有 84~151 字符，逐行存它会让设备明细表
//!   年涨 4~7MB（是其余全部统计表的 4~7 倍），字典化后降到 ~0.5MB/年。
//!   旧库的 `device_key` TEXT 形态由 [`migrate_device_tables`] 自动转换。
//! - `key_log` 是**导入旧版库时的暂存表**，运行期不写：不再由 `ensure_schema`
//!   无条件创建（每年库白占 4 页），只在导入路径用 [`ensure_staging_table`] 建。
//! - `meta` 表记录元信息
//! - 提供读写连接与只读连接两种打开方式

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

// 线程本地只读连接缓存：同一线程内按文件路径复用，避免每次查询重开连接。
// 只读连接不参与写锁，WAL 模式下可安全并发；文件被替换/移动后通过
// [`clear_ro_cache`] 失效（归档/导入/压缩时调用）。
thread_local! {
    static RO_POOL: RefCell<HashMap<PathBuf, (Connection, std::time::Instant)>> =
        RefCell::new(HashMap::new());
    /// 本线程上次清池时对应的全局代次；与 `RO_GEN` 不一致就说明别的线程做过失效
    static RO_GEN_SEEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// 只读连接池的**全局**代次。
///
/// 池子是 thread_local 的，而失效动作（归档/导入/压缩/按日期删数据）发生在别的线程上
/// —— init 主线程、或 async 命令的 tokio worker。原先的 `clear_ro_cache()` 只清得掉
/// 调用者自己那一份，于是存活几个月的统计线程会继续拿着指向"已被换掉的同名文件"的句柄
/// 读旧内容；更绕的是 `available_years()` 走的是非池化的 `open_ro`，年份照样被列出来，
/// 表现就是"列出了这一年、查它却是 0"。改成惰性比对代次：每个线程下一次用到时自愈。
static RO_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 测试用：池子里真正**新打开**过多少次连接（用来判断"有没有复用陈旧句柄"）。
#[cfg(test)]
static RO_OPENS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// 连接缓存上限：超过则按 LRU 淘汰最久未用的条目，防止多年份库长期运行后无界增长。
//
// 上限必须容得下「一个用户的全部年度库」，否则每遍历一轮都要重开被挤掉的连接。
// 实测（bench_test::aggregation_cost_vs_year_db_count，每轮 = 6 个聚合查询）：
// 7 个年度库时上限 4 → 16.9ms，上限 12 → 10.0ms（−40%）；1/4 库场景不变。
// 代价是每条缓存连接约 1MB 页缓存（open_ro 里 cache_size=-1024）+ 一个文件句柄，
// 且只在真正查过这么多年库的线程上增长。
const RO_POOL_MAX: usize = 12;

/// 使用缓存中的只读连接执行 `f`。连接不存在或打开失败时返回 `None`。
pub fn with_ro_conn<T>(path: &Path, f: impl FnOnce(&Connection) -> T) -> Option<T> {
    RO_POOL.with(|pool| {
        let gen = RO_GEN.load(std::sync::atomic::Ordering::Acquire);
        let mut pool = pool.borrow_mut();
        if RO_GEN_SEEN.get() != gen {
            // 别的线程调用过 clear_ro_cache()：本线程的陈旧句柄全部作废
            pool.clear();
            RO_GEN_SEEN.set(gen);
        }
        if !pool.contains_key(path) {
            // LRU 淘汰最久未用条目（此前整体 clear 会造成缓存抖动）
            if pool.len() >= RO_POOL_MAX {
                let oldest = pool
                    .iter()
                    .min_by_key(|(_, (_, last_used))| *last_used)
                    .map(|(k, _)| k.clone());
                if let Some(oldest) = oldest {
                    pool.remove(&oldest);
                }
            }
            match open_ro(path) {
                Ok(conn) => {
                    #[cfg(test)]
                    RO_OPENS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    pool.insert(path.to_path_buf(), (conn, std::time::Instant::now()));
                }
                Err(e) => {
                    // 不静默：调用方普遍用 unwrap_or(0) 兜底，若不记日志，
                    // "库损坏/被锁/不可读"会表现为"今日 0 次、排行榜为空"，
                    // 用户与排查者都无法区分"没有数据"和"读不出来"。
                    tracing::error!("只读连接打开失败 {}: {e}", path.display());
                    return None;
                }
            }
        }
        match pool.get_mut(path) {
            Some((conn, last_used)) => {
                *last_used = std::time::Instant::now();
                Some(f(conn))
            }
            None => None,
        }
    })
}

/// 清空只读连接缓存（归档/导入/压缩/删除数据后调用，避免持有失效句柄）。
pub fn clear_ro_cache() {
    // 先推进全局代次（别的线程下次取连接时自愈），再清掉本线程这一份
    RO_GEN.fetch_add(1, std::sync::atomic::Ordering::Release);
    RO_POOL.with(|pool| pool.borrow_mut().clear());
    RO_GEN_SEEN.with(|g| g.set(RO_GEN.load(std::sync::atomic::Ordering::Acquire)));
}

/// 打开一个可写连接并应用标准 PRAGMA。
pub fn open_rw(path: &Path) -> anyhow::Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let conn = Connection::open(path)?;
    apply_rw_pragmas(&conn)?;
    Ok(conn)
}

/// 打开一个只读连接（并发读安全，不产生 WAL 副作用）。
pub fn open_ro(path: &Path) -> anyhow::Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(15))?;
    // 只读连接用较小的页缓存（默认约 2MB，聚合表查询无需大缓存）
    conn.pragma_update(None, "cache_size", -1024)?; // 1MB
    Ok(conn)
}

/// 应用与 Python 版一致的写入端 PRAGMA。
fn apply_rw_pragmas(conn: &Connection) -> anyhow::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_secs(15))?;
    conn.pragma_update(None, "cache_size", -8000)?; // 8MB 缓存
                                                    // WAL 高水位：默认 wal_autocheckpoint=1000 页（4MB）且不设 journal_size_limit 时，
                                                    // WAL 一旦涨到 4MB 就永远停在那里（帧早已 checkpoint 回主库，纯粹是文件不缩，
                                                    // 实测主库 132KB / WAL 4.14MB）。收紧阈值 + 设上限后稳定在 1MB 内。
    conn.pragma_update(None, "wal_autocheckpoint", 256)?; // 256 页 = 1MB
    conn.pragma_update(None, "journal_size_limit", 1048576)?; // checkpoint 后收缩到 1MB
    Ok(())
}

/// 表是否存在。
pub(crate) fn table_exists(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |_| Ok(()),
    )
    .is_ok()
}

/// 只读判断某个库里有没有某张表（库/表不存在都返回 false）。用完即关。
pub fn table_exists_readonly(path: &Path, table: &str) -> bool {
    match open_ro(path) {
        Ok(conn) => table_exists(&conn, table),
        Err(_) => false,
    }
}

/// 表里是否有指定列（旧形态判定用）。
pub(crate) fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    // 表名来自本模块内的字面量，无注入面；用 format 而非绑定参数是为了兼容
    // pragma_table_info 的参数绑定差异。
    conn.prepare(&format!(
        "SELECT 1 FROM pragma_table_info('{table}') WHERE name=?1"
    ))
    .and_then(|mut stmt| stmt.exists([column]))
    .unwrap_or(false)
}

/// 按设备实例路径取整数 id（统计表字典化后只存 id）。
pub fn device_id_of(conn: &Connection, device_key: &str) -> Option<i64> {
    conn.query_row(
        "SELECT id FROM devices WHERE device_key=?1",
        [device_key],
        |r| r.get(0),
    )
    .ok()
}

/// 确保「导入旧版库」用的暂存表 `key_log` 存在（幂等）。
///
/// 只在导入路径调用：运行期不写该表，无条件创建会让每个年度库白占
/// 表 + 索引 + `sqlite_sequence` 共 4 页（实测占库体积 12%，0 行）。
/// 兼容旧形态（rowid + AUTOINCREMENT + 两个单列索引）：用唯一索引兜住去重语义，
/// 导入侧才能安全地改用 `INSERT OR IGNORE`。
pub fn ensure_staging_table(conn: &Connection) -> anyhow::Result<()> {
    if !table_exists(conn, "key_log") {
        conn.execute_batch(
            "CREATE TABLE key_log (
                key_name TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                PRIMARY KEY (key_name, timestamp)
            ) WITHOUT ROWID;",
        )?;
    }
    conn.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_key_log_dedup ON key_log(key_name, timestamp)",
        [],
    )?;
    Ok(())
}

/// 确保指定连接的年度库 schema 存在（幂等）。
pub fn ensure_schema(conn: &Connection, year: i32) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS daily_counts (
            date_key INTEGER PRIMARY KEY,
            count INTEGER NOT NULL,
            seconds INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS hourly_counts (
            date_key INTEGER NOT NULL,
            hour INTEGER NOT NULL,
            count INTEGER NOT NULL,
            PRIMARY KEY (date_key, hour)
        ) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS key_counts (
            date_key INTEGER NOT NULL,
            key_name TEXT NOT NULL,
            count INTEGER NOT NULL,
            PRIMARY KEY (date_key, key_name)
        ) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS app_usage (
            date_key INTEGER NOT NULL,
            app_name TEXT NOT NULL,
            seconds INTEGER NOT NULL,
            PRIMARY KEY (date_key, app_name)
        ) WITHOUT ROWID;
        -- 设备维度统计：date_key + 设备整数 id 按天聚合
        -- （id 指向 devices 字典表，采集侧给的是 Raw Input 设备实例路径）
        CREATE TABLE IF NOT EXISTS device_counts (
            date_key INTEGER NOT NULL,
            device_id INTEGER NOT NULL,
            count INTEGER NOT NULL,
            PRIMARY KEY (date_key, device_id)
        ) WITHOUT ROWID;
        -- 设备 × 键名明细：供「设备详情」的键名排行（独立口径，不与 key_counts 混用）
        CREATE TABLE IF NOT EXISTS device_key_counts (
            date_key INTEGER NOT NULL,
            device_id INTEGER NOT NULL,
            key_name TEXT NOT NULL,
            count INTEGER NOT NULL,
            PRIMARY KEY (date_key, device_id, key_name)
        ) WITHOUT ROWID;
        -- 设备字典表：首个事件时登记一次，之后只读缓存。
        -- id 是库内自增（各年度库互不相干），device_key 是查询入口。
        CREATE TABLE IF NOT EXISTS devices (
            id INTEGER PRIMARY KEY,
            device_key TEXT NOT NULL UNIQUE,
            name TEXT NOT NULL,
            kind TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        ) WITHOUT ROWID;",
    )?;
    // 顺序有讲究：先把旧的 device_key 文本形态迁成整数 id（旧表没有 device_id 列，
    // 索引建在它上面会直接报 "no such column"），再统一补索引。
    migrate_device_tables(conn)?;
    merge_active_seconds_into_daily(conn)?;
    // 再把「完整实例路径」归组键迁成「硬件身份段」（B14-2）：必须在索引批次之前，
    // 它会整表重建两张统计表（索引随 DROP 消失，下面的 CREATE IF NOT EXISTS 会补回）。
    migrate_device_identity_keys(conn)?;
    // 复合主键前缀是 date_key，按设备单列过滤（设备详情）只能全表扫：
    // 这两条索引把「按设备取序列 / 取键名明细」变成索引区间扫描。
    //
    // daily_counts **不要**给 count 建索引：「历史最高一天」排的是
    // (count DESC, date_key ASC) 两个键，单列 count 索引免不掉排序，
    // EXPLAIN 实测是 SCAN 覆盖索引而非 SEARCH，有无索引差 0.025ms；
    // 而它一年才 365 行，全表扫比维护索引更划算（2026-09-21 实测，原索引已移除）。
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_device_counts_dev
            ON device_counts(device_id, date_key);
         CREATE INDEX IF NOT EXISTS idx_device_key_counts_dev
            ON device_key_counts(device_id, date_key);",
    )?;
    // 兼容清理：旧库遗留的 count DESC 索引（上面已论证无用），存在就删。
    conn.execute_batch("DROP INDEX IF EXISTS idx_daily_counts_count")?;
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('year', ?1)",
        [year.to_string()],
    )?;
    Ok(())
}

/// 设备归组键重做（B14-2）：完整实例路径 → 硬件身份段（见
/// `device_alias::hardware_identity_key`），同身份的历史计数合并求和。
///
/// 幂等：触发条件是 devices 里还有含 `#` 的键（完整路径形态）；迁过的库键已
/// 无 `#`，一次 EXISTS 就跳过。身份键经身份函数原样返回，重复执行是空操作。
/// 整段单事务，失败整体回滚 —— 骨架照 `migrate_device_tables` 的先例。
///
/// 硬取舍（见身份函数注释）：同型号 + 同接口的多台设备在此颗粒度必然并成一台，
/// 历史计数随之合并 —— 这是迁移的一部分，不是 bug。
fn migrate_device_identity_keys(conn: &Connection) -> anyhow::Result<()> {
    let needs: i64 = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM devices WHERE device_key LIKE '%#%')",
        [],
        |r| r.get(0),
    )?;
    if needs == 0 {
        return Ok(());
    }
    tracing::info!("设备归组键迁移：完整实例路径 → 硬件身份段（同身份合并计数）");
    conn.execute("BEGIN IMMEDIATE;", [])?;
    let migrate = (|| -> anyhow::Result<()> {
        // 旧登记行按 id 升序读：同一身份多行时，先登记的（换口前的老路径）
        // 名字/类型优先 —— 它更接近用户第一次见到这台设备时的样子
        let mut stmt =
            conn.prepare("SELECT id, device_key, name, kind FROM devices ORDER BY id")?;
        let mut old_rows: Vec<(i64, String, String, String)> = Vec::new();
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            old_rows.push(row?);
        }
        drop(stmt);

        // 旧 id → 新 id；同一身份的旧行全部指到同一新行
        let mut new_id_of: HashMap<i64, i64> = HashMap::new();
        // (身份键, 名字, 类型)，按新 id 序
        let mut merged_meta: Vec<(String, String, String)> = Vec::new();
        let mut identity_new_id: HashMap<String, i64> = HashMap::new();
        for (id, key, name, kind) in &old_rows {
            let identity = crate::device_alias::hardware_identity_key(key);
            let new_id = match identity_new_id.get(&identity) {
                Some(nid) => *nid,
                None => {
                    let nid = merged_meta.len() as i64 + 1;
                    identity_new_id.insert(identity.clone(), nid);
                    merged_meta.push((identity, name.clone(), kind.clone()));
                    nid
                }
            };
            new_id_of.insert(*id, new_id);
        }
        // 孤儿 device_id（统计行有、登记行没有——迁移/归档历史遗留）：
        // 补占位登记行，键沿用查询侧 `device-id:N` 的既有约定（无 #，重复迁移稳定），
        // 不补的话下面的映射会静默丢行
        let mut orphan_ids: std::collections::BTreeSet<i64> = std::collections::BTreeSet::new();
        for table in ["device_counts", "device_key_counts"] {
            let mut st = conn.prepare(&format!("SELECT DISTINCT device_id FROM {table}"))?;
            let rows = st.query_map([], |r| r.get::<_, i64>(0))?;
            for r in rows {
                let id = r?;
                if !new_id_of.contains_key(&id) {
                    orphan_ids.insert(id);
                }
            }
        }
        for oid in &orphan_ids {
            let key = format!("{}{oid}", crate::db::queries::ARCHIVED_DEVICE_KEY_PREFIX);
            let nid = merged_meta.len() as i64 + 1;
            identity_new_id.insert(key.clone(), nid);
            merged_meta.push((key.clone(), key, "unknown".to_string()));
            new_id_of.insert(*oid, nid);
        }

        // devices 按新键重建（新 id 连续下发）
        conn.execute_batch(
            "DROP TABLE IF EXISTS devices_new;
             CREATE TABLE devices_new (
                id INTEGER PRIMARY KEY,
                device_key TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL,
                kind TEXT NOT NULL
             );",
        )?;
        {
            let mut ins = conn.prepare(
                "INSERT INTO devices_new (id, device_key, name, kind) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (i, (identity, name, kind)) in merged_meta.iter().enumerate() {
                ins.execute(rusqlite::params![i as i64 + 1, identity, name, kind])?;
            }
        }
        rebuild_device_counts_by_identity(conn, "device_counts", &new_id_of, false)?;
        rebuild_device_counts_by_identity(conn, "device_key_counts", &new_id_of, true)?;
        // 旧登记表整体换掉
        if table_exists(conn, "devices") {
            conn.execute("DROP TABLE devices", [])?;
        }
        conn.execute("ALTER TABLE devices_new RENAME TO devices", [])?;
        Ok(())
    })();

    match migrate {
        Ok(()) => {
            conn.execute("COMMIT;", [])?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute("ROLLBACK;", []);
            Err(e)
        }
    }
}

/// 按新归组 id 重建一张设备统计表：旧行读进内存、映射 + 求和、写进新表、换名。
/// （表都很小：device_counts 一年 365×设备数 行，明细表多一个键名维度。）
fn rebuild_device_counts_by_identity(
    conn: &Connection,
    table: &str,
    new_id_of: &HashMap<i64, i64>,
    with_key_name: bool,
) -> anyhow::Result<()> {
    let sql = if with_key_name {
        format!("SELECT date_key, device_id, key_name, count FROM {table}")
    } else {
        format!("SELECT date_key, device_id, NULL, count FROM {table}")
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    let mut agg: std::collections::BTreeMap<(i64, i64, Option<String>), i64> = Default::default();
    for row in rows {
        let (date_key, old_id, key_name, count) = row?;
        let new_id = *new_id_of
            .get(&old_id)
            .ok_or_else(|| anyhow::anyhow!("{table} 引用了登记表里没有的 device_id {old_id}"))?;
        *agg.entry((date_key, new_id, key_name)).or_insert(0) += count;
    }
    drop(stmt);
    conn.execute_batch(&format!(
        "DROP TABLE IF EXISTS {table}_new;
         CREATE TABLE {table}_new (
            {schema}
            , PRIMARY KEY ({pk})
         ) WITHOUT ROWID;",
        schema = if with_key_name {
            "date_key INTEGER NOT NULL, device_id INTEGER NOT NULL, key_name TEXT NOT NULL, count INTEGER NOT NULL"
        } else {
            "date_key INTEGER NOT NULL, device_id INTEGER NOT NULL, count INTEGER NOT NULL"
        },
        pk = if with_key_name {
            "date_key, device_id, key_name"
        } else {
            "date_key, device_id"
        },
    ))?;
    let ins_sql = if with_key_name {
        format!("INSERT INTO {table}_new (date_key, device_id, key_name, count) VALUES (?1, ?2, ?3, ?4)")
    } else {
        format!("INSERT INTO {table}_new (date_key, device_id, count) VALUES (?1, ?2, ?3)")
    };
    {
        let mut ins = conn.prepare(&ins_sql)?;
        for ((date_key, new_id, key_name), count) in &agg {
            if with_key_name {
                ins.execute(rusqlite::params![
                    date_key,
                    new_id,
                    key_name.as_deref().unwrap_or(""),
                    count
                ])?;
            } else {
                ins.execute(rusqlite::params![date_key, new_id, count])?;
            }
        }
    }
    conn.execute(&format!("DROP TABLE {table}"), [])?;
    conn.execute(&format!("ALTER TABLE {table}_new RENAME TO {table}"), [])?;
    Ok(())
}

/// 把历史独立表 `active_seconds` 并入 `daily_counts.seconds`（幂等）。
///
/// 两张表都是「date_key 主键、一天一行」，且**所有查询都是 `WHERE date_key=?` 点查**，
/// 拆开只是历史原因 —— 合并后少一棵 B-tree、少一条 UPSERT，
/// 「今日按键次数 + 今日活跃时长」也能一次读出来。
///
/// 新建的库在 `ensure_schema` 里直接带上 seconds 列，只有**老库**才走到这里：
/// 补列 → 把旧表的秒数 UPDATE 过来 → 删旧表。整段在单事务里完成，失败即回滚，
/// 下次打开会重试；已完成时只做一次 `column_exists` 检测（一次 pragma 查询）。
fn merge_active_seconds_into_daily(conn: &Connection) -> anyhow::Result<()> {
    let already_merged = column_exists(conn, "daily_counts", "seconds");
    if already_merged {
        // 上次迁移中途失败时会剩下孤岛旧表，这里补删；正常路径是 no-op。
        if table_exists(conn, "active_seconds") {
            tracing::info!("清理残留的 active_seconds 表（已完成合并）");
            conn.execute_batch("DROP TABLE active_seconds")?;
        }
        return Ok(());
    }

    tracing::info!("活跃时长迁移：active_seconds → daily_counts.seconds");
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let result = (|| -> anyhow::Result<()> {
        conn.execute_batch(
            "ALTER TABLE daily_counts ADD COLUMN seconds INTEGER NOT NULL DEFAULT 0",
        )?;
        if table_exists(conn, "active_seconds") {
            // 两步走，**不允许丢任何一天的时长**：
            //   1) 先把「有活跃记录但 daily_counts 没有这一天」的行补进去（count=0）。
            //      正常采集下不会出现（时长由键鼠事件产生，同一事件也记 daily），
            //      但历史库的口径随版本变过，迁移逻辑宁可信其有。
            //   2) 再 UPDATE 其余各行 —— 反过来「有按键没时长」的早期日期保持默认 0，
            //      因为活跃统计本来就上线得更晚。
            conn.execute_batch(
                "INSERT OR IGNORE INTO daily_counts (date_key, count, seconds)
                   SELECT a.date_key, 0, a.seconds FROM active_seconds a;
                 UPDATE daily_counts
                    SET seconds = (SELECT a.seconds FROM active_seconds a
                                    WHERE a.date_key = daily_counts.date_key)
                  WHERE EXISTS (SELECT 1 FROM active_seconds a
                                 WHERE a.date_key = daily_counts.date_key);
                 DROP TABLE active_seconds;",
            )?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT;")?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK;");
            Err(e)
        }
    }
}

/// 把旧的「统计表直接存 device_key 文本」形态迁移成整数 id 形态（幂等）。
///
/// 旧形态每行都要带 84~151 字符的设备实例路径（实测 ≈200~300B/行），
/// 设备明细表一年涨 4~7MB。迁移只在首次遇到旧形态时执行，之后检测零成本
/// （一次 `pragma_table_info`）。整段在单事务里完成，失败即整体回滚。
fn migrate_device_tables(conn: &Connection) -> anyhow::Result<()> {
    let legacy_counts = column_exists(conn, "device_counts", "device_key");
    let legacy_detail = column_exists(conn, "device_key_counts", "device_key");
    let legacy_devices = table_exists(conn, "devices") && !column_exists(conn, "devices", "id");
    if !legacy_counts && !legacy_detail && !legacy_devices {
        return Ok(());
    }

    tracing::info!("设备统计表迁移：device_key 文本 → 整数 id 字典");
    conn.execute("BEGIN IMMEDIATE;", [])?;
    let migrate = (|| -> anyhow::Result<()> {
        conn.execute_batch(
            "DROP TABLE IF EXISTS devices_new;
             CREATE TABLE devices_new (
                id INTEGER PRIMARY KEY,
                device_key TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL,
                kind TEXT NOT NULL
             );",
        )?;
        if table_exists(conn, "devices") {
            conn.execute(
                "INSERT OR IGNORE INTO devices_new (device_key, name, kind)
                 SELECT device_key, name, kind FROM devices",
                [],
            )?;
        }
        // 统计行出现、登记表缺失的设备（归档早于登记表落地的历史库）补一行，
        // 否则下面的 JOIN 会静默丢行。
        for table in ["device_counts", "device_key_counts"] {
            if column_exists(conn, table, "device_key") {
                conn.execute(
                    &format!(
                        "INSERT OR IGNORE INTO devices_new (device_key, name, kind)
                         SELECT DISTINCT device_key, device_key, 'unknown' FROM {table}"
                    ),
                    [],
                )?;
            }
        }
        // 重建两张统计表：行数、数值不变，只把 device_key 换成 id
        for (table, pk, cols) in [
            (
                "device_counts",
                "date_key, device_id",
                "date_key, device_id, count",
            ),
            (
                "device_key_counts",
                "date_key, device_id, key_name",
                "date_key, device_id, key_name, count",
            ),
        ] {
            if !column_exists(conn, table, "device_key") {
                continue;
            }
            conn.execute_batch(&format!(
                "DROP TABLE IF EXISTS {table}_new;
                 CREATE TABLE {table}_new (
                    {cols_typed}
                    , PRIMARY KEY ({pk})
                 ) WITHOUT ROWID;",
                cols_typed = match table {
                    "device_counts" => {
                        "date_key INTEGER NOT NULL, device_id INTEGER NOT NULL, count INTEGER NOT NULL"
                    }
                    _ => "date_key INTEGER NOT NULL, device_id INTEGER NOT NULL, key_name TEXT NOT NULL, count INTEGER NOT NULL",
                }
            ))?;
            conn.execute(
                &format!(
                    "INSERT INTO {table}_new ({cols})
                     SELECT c.date_key, d.id{c_extra}, c.count FROM {table} c
                       JOIN devices_new d ON d.device_key = c.device_key",
                    c_extra = if table == "device_counts" {
                        ""
                    } else {
                        ", c.key_name"
                    }
                ),
                [],
            )?;
            conn.execute(&format!("DROP TABLE {table}"), [])?;
            conn.execute(&format!("ALTER TABLE {table}_new RENAME TO {table}"), [])?;
        }
        // 旧 devices 表整体换掉（WITHOUT ROWID + 文本主键 → 整数 id + 唯一键）
        if table_exists(conn, "devices") {
            conn.execute("DROP TABLE devices", [])?;
        }
        conn.execute("ALTER TABLE devices_new RENAME TO devices", [])?;
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_device_counts_dev
                ON device_counts(device_id, date_key);
             CREATE INDEX IF NOT EXISTS idx_device_key_counts_dev
                ON device_key_counts(device_id, date_key);",
        )?;
        Ok(())
    })();

    match migrate {
        Ok(()) => {
            conn.execute("COMMIT;", [])?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute("ROLLBACK;", []);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;

    /// 旧形态（统计表直接存 device_key 文本）→ 整数 id 字典的自动迁移。
    ///
    /// 旧形态每行都要重复存 84~151 字符的设备实例路径（实测 200~300B/行，
    /// 设备明细表一年涨 4~7MB），迁移必须保住行数、计数与设备归属。
    #[test]
    fn migrates_legacy_device_key_tables_to_ids() {
        let dir = std::env::temp_dir().join("ff_rs_db_migrate_devices");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("focusflow_2026.db");
        std::fs::remove_file(&path).ok();

        let conn = open_rw(&path).unwrap();
        // 旧形态 schema（设备字典化之前）
        conn.execute_batch(
            "CREATE TABLE devices (device_key TEXT PRIMARY KEY, name TEXT NOT NULL, kind TEXT NOT NULL) WITHOUT ROWID;
             CREATE TABLE device_counts (
                date_key INTEGER NOT NULL, device_key TEXT NOT NULL, count INTEGER NOT NULL,
                PRIMARY KEY (date_key, device_key)) WITHOUT ROWID;
             CREATE TABLE device_key_counts (
                date_key INTEGER NOT NULL, device_key TEXT NOT NULL, key_name TEXT NOT NULL,
                count INTEGER NOT NULL, PRIMARY KEY (date_key, device_key, key_name)) WITHOUT ROWID;",
        )
        .unwrap();
        let kb = "HID#VID_046D&PID_C52B&MI_00#7&1f126e19&0&0000";
        conn.execute(
            "INSERT INTO devices (device_key, name, kind) VALUES (?1, 'HID 键盘 · 046D/C52B', 'keyboard')",
            [kb],
        )
        .unwrap();
        conn.execute("INSERT INTO device_counts VALUES (20716, ?1, 12)", [kb])
            .unwrap();
        conn.execute(
            "INSERT INTO device_key_counts VALUES (20716, ?1, '空格', 7)",
            [kb],
        )
        .unwrap();
        // 孤儿：只有统计行、登记表里没有（归档早于设备登记表落地的历史库）
        conn.execute(
            "INSERT INTO device_counts VALUES (20715, 'HID#ORPHAN', 3)",
            [],
        )
        .unwrap();

        ensure_schema(&conn, 2026).unwrap();

        assert!(column_exists(&conn, "devices", "id"), "devices 应有整数 id");
        assert!(!column_exists(&conn, "device_counts", "device_key"));
        assert!(
            !table_exists(&conn, "key_log"),
            "运行期不再无条件创建暂存表"
        );
        // B14-2：3 段形态的完整路径键在 ensure_schema 里被一并迁成硬件身份段
        let identity = "VID_046D&PID_C52B&MI_00";
        let id = device_id_of(&conn, identity).expect("登记行应保留");
        let cnt: i64 = conn
            .query_row(
                "SELECT count FROM device_counts WHERE date_key=20716 AND device_id=?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cnt, 12, "计数必须原样保留");
        let (key, n): (String, i64) = conn
            .query_row(
                "SELECT key_name, count FROM device_key_counts WHERE date_key=20716 AND device_id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((key.as_str(), n), ("空格", 7));
        let name: String = conn
            .query_row(
                "SELECT name FROM devices WHERE device_key=?1",
                [identity],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(name, "HID 键盘 · 046D/C52B", "登记名不能丢");
        // 形状认不准的键（这里只有两段）不并也不改：HID#ORPHAN 原样保留
        let orphans: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM devices WHERE device_key = 'HID#ORPHAN'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orphans, 1, "孤儿统计行必须补占位登记，不能静默丢数");
        let total: i64 = conn
            .query_row("SELECT SUM(count) FROM device_counts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 15);

        // 幂等：重复调用不得重复登记、重复加数
        ensure_schema(&conn, 2026).unwrap();
        let dev_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM devices", [], |r| r.get(0))
            .unwrap();
        assert_eq!(dev_rows, 2);
        let total2: i64 = conn
            .query_row("SELECT SUM(count) FROM device_counts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total2, 15);

        drop(conn);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// B14-2：完整实例路径键 → 硬件身份段，同一硬件（只差拓扑实例号）的历史计数
    /// 必须并成一台。这是「换 USB 口就新起一行、计数从零开始」的根治，
    /// 代价（同型号 + 同接口必然并组）见 `device_alias::hardware_identity_key`。
    #[test]
    fn migrates_full_path_keys_to_identity_and_merges_same_hardware() {
        let dir = std::env::temp_dir().join("ff_rs_db_migrate_identity");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("focusflow_2026.db");
        std::fs::remove_file(&path).ok();

        let conn = open_rw(&path).unwrap();
        ensure_schema(&conn, 2026).unwrap();
        // 同一只鼠标在两个 USB 口登记过（只差第三段拓扑实例号），另有一台键盘
        let mouse_a = "HID#VID_046D&PID_C52B&MI_00#7&1f126e19&0&0000";
        let mouse_b = "HID#VID_046D&PID_C52B&MI_00#8&2c5f77d4&0&0001";
        let kb = "VID_1B1C&PID_1B2D"; // 已是身份形态：迁移必须原样保留
        let ins_dev = |key: &str, name: &str, kind: &str| {
            conn.execute(
                "INSERT INTO devices (device_key, name, kind) VALUES (?1, ?2, ?3)",
                rusqlite::params![key, name, kind],
            )
            .unwrap();
        };
        let ins_cnt = |key: &str, dk: i64, n: i64| {
            let id = device_id_of(&conn, key).unwrap();
            conn.execute(
                "INSERT INTO device_counts (date_key, device_id, count) VALUES (?1, ?2, ?3)",
                rusqlite::params![dk, id, n],
            )
            .unwrap();
        };
        ins_dev(mouse_a, "HID-compliant mouse · 046D/C52B", "mouse");
        ins_dev(mouse_b, "HID-compliant mouse · 046D/C52B", "mouse");
        ins_dev(kb, "我的键盘 · 1B1C/1B2D", "keyboard");
        ins_cnt(mouse_a, 20710, 30);
        ins_cnt(mouse_b, 20711, 12);
        ins_cnt(kb, 20711, 50);

        ensure_schema(&conn, 2026).unwrap();

        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM devices", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 2, "同一硬件的两条路径必须并成一行");
        let identity = "VID_046D&PID_C52B&MI_00";
        let mouse_id = device_id_of(&conn, identity).expect("身份键应在登记表里");
        let (name, kind): (String, String) = conn
            .query_row(
                "SELECT name, kind FROM devices WHERE device_key=?1",
                [identity],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            name, "HID-compliant mouse · 046D/C52B",
            "先登记的（id 小的）名字保留"
        );
        assert_eq!(kind, "mouse");
        let mouse_total: i64 = conn
            .query_row(
                "SELECT SUM(count) FROM device_counts WHERE device_id=?1",
                [mouse_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mouse_total, 42, "两个口的计数必须求和，不能丢");
        // 已是身份形态的设备不受影响
        let kb_total: i64 = conn
            .query_row(
                "SELECT SUM(count) FROM device_counts WHERE device_id=?1",
                [device_id_of(&conn, kb).unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kb_total, 50);
        let grand: i64 = conn
            .query_row("SELECT SUM(count) FROM device_counts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(grand, 92, "总数守恒");

        // 幂等：再跑一遍不得再并、不得再变
        ensure_schema(&conn, 2026).unwrap();
        let rows2: i64 = conn
            .query_row("SELECT COUNT(*) FROM devices", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows2, 2);
        let grand2: i64 = conn
            .query_row("SELECT SUM(count) FROM device_counts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(grand2, 92);

        drop(conn);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn schema_roundtrip() {
        let dir = std::env::temp_dir().join("ff_rs_db_test");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("focusflow_2026.db");
        std::fs::remove_file(&path).ok();

        let conn = open_rw(&path).unwrap();
        ensure_schema(&conn, 2026).unwrap();
        conn.execute(
            "INSERT INTO daily_counts (date_key, count) VALUES (20771, 2)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO hourly_counts (date_key, hour, count) VALUES (20771, 10, 2)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO key_counts (date_key, key_name, count) VALUES (20771, 'A', 2)",
            [],
        )
        .unwrap();

        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM daily_counts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 1);

        // 新库直接就是字典化形态，且不建运行期用不到的 key_log
        assert!(column_exists(&conn, "devices", "id"));
        assert!(column_exists(&conn, "device_counts", "device_id"));
        assert!(column_exists(&conn, "device_key_counts", "device_id"));
        assert!(
            !table_exists(&conn, "key_log"),
            "key_log 只在导入时按需创建"
        );
        for idx in ["idx_device_counts_dev", "idx_device_key_counts_dev"] {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
                    [idx],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "{idx} 必须存在（设备详情按 device_key 过滤要靠它）");
        }

        drop(conn);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// WAL 年度库缺 `-shm`（崩溃残留的那一类）时，只读路径必须还能读出内容。
    ///
    /// 不修的表现是"那一年静默读成空"：`open_ro` 打不开 → `with_ro_conn` 返回
    /// `None` → 调用方普遍 `unwrap_or(0)`，界面上就是整年排行榜空掉。
    #[test]
    fn ro_conn_reads_a_wal_db_that_lost_its_shm_sidecar() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("conn_wal_shm");

        // ① 正常建一个 WAL 库并写三行
        let src = crate::paths::year_db_path(2031);
        {
            let conn = open_rw(&src).unwrap();
            assert_eq!(
                conn.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
                    .unwrap()
                    .to_ascii_uppercase(),
                "WAL",
                "夹具必须是 WAL 库，否则这条用例什么都验不到"
            );
            conn.execute_batch("CREATE TABLE t (x INTEGER);").unwrap();
            for i in 0..3 {
                conn.execute("INSERT INTO t VALUES (?1)", [i]).unwrap();
            }
        }
        // ② 只取主库文件。正常关闭会把 -wal/-shm 合并回主库并删掉 sidecar，
        //    而库头的 journal 形态仍是 WAL —— 这正是"崩溃后只剩主文件"的形状。
        let orphan = src.parent().unwrap().join("orphan_2031.db");
        std::fs::copy(&src, &orphan).unwrap();
        assert!(!orphan.with_extension("db-shm").exists());

        // ③ 只读路径要读出 3 行，而不是 None（上层会把它当成 0）
        let got = with_ro_conn(&orphan, |c| {
            c.query_row("SELECT COUNT(*) FROM t", [], |r| r.get::<_, i64>(0))
                .unwrap()
        });
        assert_eq!(got, Some(3), "只读打开无 -shm 的 WAL 库不该静默读成空");
        // 兜底句柄仍须只读：读路径不能顺手写数据
        let can_write = with_ro_conn(&orphan, |c| c.execute("INSERT INTO t VALUES (9)", []));
        assert_eq!(
            can_write.map(|r| r.is_ok()),
            Some(false),
            "兜底之后仍然不许写"
        );
    }

    /// 更硬的一种形态：主库 + **还在的 -wal**，但 -shm 没了。
    ///
    /// 这才是"崩溃/整目录搬运后只剩 sidecar"的形状：未 checkpoint 的已提交帧还在
    /// -wal 里，只读连接要先把 WAL 恢复出来才能读，而恢复需要写 -shm。
    /// 这条用例先回答"SQLite 到底会不会失败"，再决定要不要在 `open_ro` 里兜底 ——
    /// 上一节那种"只有主库文件"的形态实测根本不失败（见另一个用例）。
    #[test]
    fn ro_conn_reads_a_wal_db_with_orphaned_wal_sidecar() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("conn_wal_orphan");

        let src = crate::paths::year_db_path(2032);
        let orphan = src.parent().unwrap().join("orphan_2032.db");
        let conn = open_rw(&src).unwrap();
        conn.execute_batch("CREATE TABLE t (x INTEGER);").unwrap();
        for i in 0..3 {
            conn.execute("INSERT INTO t VALUES (?1)", [i]).unwrap();
        }
        // 连接还开着的时候拷走主库 + -wal：关掉连接会把 -wal 合并回主库并删掉，
        // 那样就变回"只有主库文件"的温和形态，验不到想验的这一种。
        assert!(
            Path::new(&format!("{}-wal", src.display())).exists(),
            "夹具前提：WAL 里得有未合并的帧"
        );
        std::fs::copy(&src, &orphan).unwrap();
        std::fs::copy(
            format!("{}-wal", src.display()),
            format!("{}-wal", orphan.display()),
        )
        .unwrap();
        drop(conn);
        assert!(
            !Path::new(&format!("{}-shm", orphan.display())).exists(),
            "夹具前提：-shm 必须不存在"
        );

        let got = with_ro_conn(&orphan, |c| {
            c.query_row("SELECT COUNT(*) FROM t", [], |r| r.get::<_, i64>(0))
                .unwrap_or(-1)
        });
        assert_eq!(
            got,
            Some(3),
            "带着孤儿 -wal 的库不该被只读路径读成空（None 在上层就是 0）"
        );
    }

    /// 老库的 active_seconds 必须被并入 daily_counts.seconds，数据一天都不能丢。
    ///
    /// 这条用例专门走到「迁移分支」——其余测试都是用新 ensure_schema 直接建库，
    /// 压根不会执行 merge_active_seconds_into_daily，所以这里要手工搭老形态。
    #[test]
    fn ensure_schema_merges_legacy_active_seconds() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("conn_merge");

        let year = chrono::Local::now().date_naive().year();
        let path = crate::paths::year_db_path(year);
        {
            // 手工搭「2026-09-21 之前」的老形态：daily_counts 只有两列，时长另表存
            let conn = open_rw(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE daily_counts (
                    date_key INTEGER PRIMARY KEY,
                    count INTEGER NOT NULL
                 );
                 CREATE TABLE active_seconds (
                    date_key INTEGER PRIMARY KEY,
                    seconds INTEGER NOT NULL
                 );",
            )
            .unwrap();
            // A：既有按键也有时长（rusqlite 的 execute 只跑单条语句，多条要用 batch）
            conn.execute_batch(
                "INSERT INTO daily_counts VALUES (1000, 42);
                 INSERT INTO active_seconds VALUES (1000, 3600);
                 INSERT INTO daily_counts VALUES (1001, 7);
                 INSERT INTO active_seconds VALUES (1002, 900);",
            )
            .unwrap();
        }

        let conn = open_rw(&path).unwrap();
        ensure_schema(&conn, year).unwrap();

        let ones = |dk: i64| -> (i64, i64) {
            conn.query_row(
                "SELECT count, seconds FROM daily_counts WHERE date_key=?1",
                [dk],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(ones(1000), (42, 3600), "A 天：按键与时长都应保留");
        assert_eq!(ones(1001), (7, 0), "B 天：没有时长应补 0，按键数不受影响");
        assert_eq!(ones(1002), (0, 900), "C 天：只有时长的行不得丢失");
        assert!(
            !table_exists(&conn, "active_seconds"),
            "旧表必须删掉，否则后续写入会写进没人读的表"
        );
        drop(conn);

        // 幂等：再跑一次不能报错，也不能把 seconds 重新清零
        let conn = open_rw(&path).unwrap();
        ensure_schema(&conn, year).unwrap();
        assert_eq!(ones_reopen(&conn, 1000), (42, 3600), "重复迁移必须幂等");
        drop(conn);
    }

    fn ones_reopen(conn: &rusqlite::Connection, dk: i64) -> (i64, i64) {
        conn.query_row(
            "SELECT count, seconds FROM daily_counts WHERE date_key=?1",
            [dk],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }
}

#[cfg(test)]
mod ro_pool_thread_tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    fn make_db_at(path: &std::path::Path, value: i64) {
        let conn = Connection::open(path).expect("建库失败");
        conn.execute_batch("CREATE TABLE IF NOT EXISTS t (v INTEGER);")
            .expect("建表失败");
        conn.execute("INSERT INTO t (v) VALUES (?1)", [value])
            .expect("插入失败");
        drop(conn);
    }

    fn read_t(conn: &Connection) -> i64 {
        conn.query_row("SELECT v FROM t ORDER BY v LIMIT 1", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap_or(-999)
    }

    /// 别的线程调用 `clear_ro_cache()` 之后，本线程的连接池必须真的作废。
    ///
    /// 池子是 thread_local，而失效动作（归档/导入/压缩/按日期删数据）跑在 init 主线程
    /// 或 async 命令的 tokio worker 上：旧实现只清得掉调用者自己那一份，于是存活几个月
    /// 的统计线程会一直复用指向"已被换掉的同名文件"的旧句柄。
    /// 断的是"重开了连接"而不是"读到了新内容"—— 后者要在 Windows 上换掉一个正被
    /// 打开着的文件（改名/删除都受共享句柄阻挡），换成计次既确定又不依赖文件系统脾气。
    #[test]
    fn clear_ro_cache_invalidates_other_threads_too() {
        let _lock = crate::paths::test_app_dir_lock();
        let dir = crate::paths::test_app_dir("ro_gen");
        let path = dir.path().join("gen_year.db");
        make_db_at(&path, 5);

        let (started_tx, started_rx) = channel::<(u64, i64)>();
        let (go_tx, go_rx) = channel::<()>();
        let worker_path = path.clone();
        let worker = std::thread::spawn(move || {
            let before = RO_OPENS.load(Ordering::Relaxed);
            let first = with_ro_conn(&worker_path, read_t).expect("首次读取失败");
            let after_first = RO_OPENS.load(Ordering::Relaxed);
            let _ = started_tx.send((after_first - before, first));
            let released = go_rx.recv_timeout(Duration::from_secs(10)).is_ok();
            let again_before = RO_OPENS.load(Ordering::Relaxed);
            let second = with_ro_conn(&worker_path, read_t).unwrap_or(-1);
            let after_second = RO_OPENS.load(Ordering::Relaxed);
            (released, second, after_second - again_before)
        });

        let (opens_first, first) = started_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("工作线程没能完成首次读取");
        assert_eq!(opens_first, 1, "前置条件：首次使用应当新开一条连接");
        assert_eq!(first, 5, "前置条件：读到的是建库时写入的值");

        clear_ro_cache(); // 从**另一个线程**失效
        let _ = go_tx.send(());

        let (released, second, opens_second) = worker.join().expect("工作线程 panic");
        assert!(released, "工作线程等待失效信号超时");
        assert_eq!(second, 5, "内容没变，值应当一致");
        assert_eq!(
            opens_second, 1,
            "另一个线程 clear_ro_cache() 之后，本线程必须重开连接（旧实现这里复用陈旧句柄 → 0）"
        );
    }
}
