//! 旧数据导入：从旧版数据目录迁移到当前数据目录。
//!
//! 场景：
//! - 从 Python 版 FocusFlow 切换到 Rust 版（schema 兼容，直接迁移）
//! - 换电脑/换目录后迁移旧数据
//!
//! 设计：
//! - 年度键鼠库 `focusflow_YYYY.db`：先写入暂存表 key_log（按 timestamp 去重，幂等），
//!   再通过聚合迁移落进 daily/hourly/key 三张聚合表并压缩文件
//! - 附属库（accounting/pomodoro/scheduler/edge_history）：直接复制（表独立）
//! - 若目标库不存在则整体复制文件（最快路径）
//! - 输出导入汇总

use std::path::Path;

use rusqlite::Connection;

use crate::db::connection;
use crate::paths;

/// 导入结果汇总。
#[derive(Debug, Default)]
pub struct ImportSummary {
    /// 导入的年度库
    pub year_dbs: Vec<i32>,
    /// 各年度导入的键鼠记录数
    pub records_by_year: Vec<(i32, i64)>,
    /// 复制的附属库
    pub copied_aux: Vec<String>,
    /// 跳过的文件（无数据/已存在）
    pub skipped: Vec<String>,
    /// 错误
    pub errors: Vec<String>,
}

/// 附属库文件名列表（直接复制）。
const AUX_DBS: &[&str] = &[
    "focusflow_accounting.db",
    "focusflow_pomodoro.db",
    "focusflow_scheduler.db",
    "focusflow_edge_history.db",
];

/// 从旧数据目录导入全部数据到当前数据目录。
pub fn import_legacy_data(src_dir: &Path) -> ImportSummary {
    let mut summary = ImportSummary::default();

    // 1) 年度键鼠库
    let src_year_dbs = list_year_dbs(src_dir);
    for year in src_year_dbs {
        match import_year_db(src_dir, year) {
            Ok(imported) => {
                summary.year_dbs.push(year);
                summary.records_by_year.push((year, imported));
            }
            Err(e) => summary.errors.push(format!("{year} 年度库导入失败: {e}")),
        }
    }

    // 2) 附属库直接复制
    for aux in AUX_DBS {
        let src = src_dir.join(aux);
        if !src.exists() {
            continue;
        }
        let dst = paths::data_dir().join(aux);
        match std::fs::copy(&src, &dst) {
            Ok(_) => summary.copied_aux.push(aux.to_string()),
            Err(e) => summary.errors.push(format!("{aux} 复制失败: {e}")),
        }
    }

    summary
}

/// 列出数据目录下的年度库（focusflow_YYYY.db）。
fn list_year_dbs(dir: &Path) -> Vec<i32> {
    let mut years = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Some(y) = paths::is_year_db_file(&entry.path()) {
                years.push(y);
            }
        }
    }
    years.sort_unstable();
    years
}

/// 导入单个年度库：先写暂存表（按 timestamp 去重），再聚合落库。
/// 返回导入的记录数。
fn import_year_db(src_dir: &Path, year: i32) -> anyhow::Result<i64> {
    let src_path = src_dir.join(format!("focusflow_{year}.db"));
    let dst_path = paths::year_db_path(year);

    // 目标库不存在 → 整体复制（最快）
    if !dst_path.exists() {
        std::fs::copy(&src_path, &dst_path)?;
        // 复制 WAL/SHM（若有未 checkpoint 数据）
        for suffix in ["-wal", "-shm"] {
            let s = format!("{}{}", src_path.display(), suffix);
            if Path::new(&s).exists() {
                let _ = std::fs::copy(&s, format!("{}{}", dst_path.display(), suffix));
            }
        }
        // 用 Rusqlite 打开确认可用 + 建聚合表 + 迁移旧格式数据
        let conn = connection::open_rw(&dst_path)?;
        connection::ensure_schema(&conn, year)?;
        drop(conn);
        crate::db::maintenance::migrate_v2();
        // 统计导入条数（聚合表总量）
        let conn = connection::open_ro(&dst_path)?;
        let count: i64 = conn.query_row(
            "SELECT COALESCE(SUM(count), 0) FROM daily_counts",
            [],
            |r| r.get(0),
        )?;
        record_import_marker(&dst_path, &src_path)?;
        return Ok(count);
    }

    // 目标库已存在：源文件未变化（大小+修改时间一致）→ 已导入过，跳过
    if let Ok(meta) = std::fs::metadata(&src_path) {
        let marker = read_import_marker(&dst_path).unwrap_or(None);
        let same_size = marker.map(|(s, _)| s) == Some(meta.len());
        let same_mtime = match (marker.and_then(|(_, m)| m), meta.modified().ok()) {
            (Some(a), Some(b)) => {
                let a_s = a
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let b_s = b
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                a_s == b_s
            }
            _ => false,
        };
        if same_size && same_mtime {
            return Ok(0);
        }
    }

    // 去重合并（写入暂存表）
    let src_conn = Connection::open(&src_path)?;
    let dst_conn = connection::open_rw(&dst_path)?;
    connection::ensure_schema(&dst_conn, year)?;

    // 确认源库有 key_log 表
    let has_src_table: bool = src_conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='key_log'",
            [],
            |_| Ok(()),
        )
        .is_ok();
    if !has_src_table {
        return Ok(0);
    }

    // 幂等：仅导入目标库中不存在的 (key_name, timestamp)
    let mut stmt = src_conn.prepare("SELECT key_name, timestamp FROM key_log")?;
    let rows: Vec<(String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;

    dst_conn.execute("BEGIN IMMEDIATE;", [])?;
    let mut imported: i64 = 0;
    {
        let mut insert = dst_conn.prepare(
            "INSERT INTO key_log (key_name, timestamp)
             SELECT ?1, ?2
             WHERE NOT EXISTS (SELECT 1 FROM key_log WHERE key_name=?1 AND timestamp=?2)",
        )?;
        for (key, ts) in &rows {
            if insert.execute(rusqlite::params![key, ts])? > 0 {
                imported += 1;
            }
        }
    }
    dst_conn.execute("COMMIT;", [])?;
    drop(dst_conn);

    // 聚合落库并压缩（幂等：key_log 迁移后清空）
    crate::db::maintenance::migrate_v2();
    record_import_marker(&dst_path, &src_path)?;

    Ok(imported)
}

/// 记录源文件指纹（大小 + 修改时间）到目标库 meta，用于重复导入检测。
fn record_import_marker(dst_path: &Path, src_path: &Path) -> anyhow::Result<()> {
    let meta = std::fs::metadata(src_path)?;
    let conn = connection::open_rw(dst_path)?;
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('imported_src_size', ?1)",
        [meta.len().to_string()],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('imported_src_mtime', ?1)",
        [meta
            .modified()
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs().to_string())
            .unwrap_or_default()],
    )?;
    Ok(())
}

/// 读取导入指纹：(大小, 修改时间)。
fn read_import_marker(
    dst_path: &Path,
) -> anyhow::Result<Option<(u64, Option<std::time::SystemTime>)>> {
    let conn = connection::open_ro(dst_path)?;
    let size: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'imported_src_size'",
            [],
            |r| r.get(0),
        )
        .ok();
    let mtime: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'imported_src_mtime'",
            [],
            |r| r.get(0),
        )
        .ok();
    match (size, mtime) {
        (Some(sz), Some(mt)) => {
            let size = sz.parse::<u64>().unwrap_or(0);
            let mtime = mt
                .parse::<u64>()
                .ok()
                .map(|secs| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs));
            Ok(Some((size, mtime)))
        }
        _ => Ok(None),
    }
}
