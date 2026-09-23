//! 在**他真实生产库的副本**上跑的一致性用例。
//!
//! 数据来自 `dist/FocusFlow/data/`（真实使用数据，gitignored）。手工 fixture
//! 抓不到只有真数据才会出现的形状：一百多个键名、跨几个月的一天多行、未
//! checkpoint 的 WAL、42 天「有按键没时长」、从旧版 Python 沿用的 `<90>` 类
//! 旧键名。所以这里**只读原目录、拷一份到临时 app_dir 再测**，绝不往原目录写。
//!
//! 与同目录其它用例的分工：`migration_test.rs` 管形状（构造出来的旧库），
//! 这个文件管口径（同一数字经两条独立路径算出来必须相等）——
//! 上一轮那个「周报 5,009 vs 上周 14」就是只有真数据才能暴露的那类问题。
//!
//! 这台机器上没有那份数据时整组跳过（打印原因），不影响门禁。

use std::path::{Path, PathBuf};

use chrono::{Duration, NaiveDate};
use focusflow_core::db::connection;
use focusflow_core::db::queries;
use focusflow_core::migration;
use focusflow_core::paths;

/// date_key ↔ 日期（与 core 内 `day_key_of_date` 同口径：自 1970-01-01 的天数）。
/// 那两个函数是 `pub(crate)`，集成测试链接的是不带 `cfg(test)` 的库，这里自己算。
fn date_of_day_key(dk: i64) -> NaiveDate {
    NaiveDate::from_ymd_opt(1970, 1, 1)
        .expect("date")
        .checked_add_signed(Duration::days(dk))
        .expect("date_key 越界")
}

fn day_key_of(date: NaiveDate) -> i64 {
    date.signed_duration_since(NaiveDate::from_ymd_opt(1970, 1, 1).expect("date"))
        .num_days()
}

/// 真实数据目录（相对 core crate）。
fn real_data_dir() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../dist/FocusFlow/data")
        .canonicalize()
        .ok()?;
    p.is_dir().then_some(p)
}

/// 只拷年度库主文件与 `-wal`（不拷 `-shm`：SQLite 照 WAL 重建 wal-index，
/// 而一份与副本对不上的索引反而添乱）。跳过附属库。
fn copy_year_dbs(src: &Path, dst: &Path) -> usize {
    let mut n = 0;
    let Ok(entries) = std::fs::read_dir(src) else {
        return 0;
    };
    for entry in entries.flatten() {
        let from = entry.path();
        let Some(name) = from.file_name().map(|s| s.to_string_lossy().to_string()) else {
            continue;
        };
        // 年份判定用 core 自己的口径（`is_year_db_file`），附属库天然排除；
        // `-wal` 那份先把后缀换成 `.db` 再问一次
        let is_year = if name.ends_with(".db-wal") {
            paths::is_year_db_file(&from.with_extension("db")).is_some()
        } else {
            paths::is_year_db_file(&from).is_some()
        };
        if is_year && std::fs::copy(&from, dst.join(&name)).is_ok() {
            n += 1;
        }
    }
    n
}

/// 在真实库副本上跑一段：持住进程级 app_dir 锁 → 拷贝 → 跑 `f` → `TestAppDir`
/// 的 Drop 清 RO 连接池并删目录。源数据不存在（或拷贝失败）时返回 None。
fn on_real_data_copy<T>(tag: &str, f: impl FnOnce() -> T) -> Option<T> {
    let _lock = paths::test_app_dir_lock();
    let src = real_data_dir()?;
    let _dir = paths::test_app_dir(tag);
    if copy_year_dbs(&src, &paths::data_dir()) == 0 {
        eprintln!("跳过 {tag}：{} 里没有年度库", src.display());
        return None;
    }
    queries::invalidate_years_cache();
    Some(f())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实库里逐年跑「只查自身」的不变量：聚合表之间必须自洽。
    ///
    /// 这些不是凭空要求的：归档、重聚合、`delete_key_today`、导入都各自只动
    /// 三张表里的一张，历史上哪一步漏了哪张，界面上就是"日视图与总视图各说各话"。
    #[test]
    fn real_data_aggregates_are_self_consistent() {
        let outcome = on_real_data_copy("realdata_agg", || {
            let mut checked = 0usize;
            for year in queries::available_years() {
                let conn = connection::open_ro(&paths::year_db_path(year)).expect("打开副本");
                // 拷贝撕裂（应用恰好在写）与真损坏分不开，宁可响亮地失败
                let ok: String = conn
                    .query_row("PRAGMA quick_check", [], |r| r.get(0))
                    .unwrap();
                assert_eq!(ok, "ok", "{year} 年库副本自检不过");

                let one =
                    |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap() };
                let stale_days = |sql: &str| -> Vec<i64> {
                    let mut stmt = conn.prepare(sql).unwrap();
                    stmt.query_map([], |r| r.get::<_, i64>(0))
                        .unwrap()
                        .collect::<Result<_, _>>()
                        .unwrap()
                };

                // 1) 按天总数 = 键名维度之和 = 小时维度之和
                assert!(
                    stale_days(
                        "SELECT d.date_key FROM daily_counts d
                         LEFT JOIN (SELECT date_key, SUM(count) c FROM key_counts GROUP BY date_key) k
                           ON k.date_key = d.date_key
                         WHERE d.count != COALESCE(k.c, 0)"
                    )
                    .is_empty(),
                    "{year} 年 daily_counts 与 key_counts 对不同一天"
                );
                assert!(
                    stale_days(
                        "SELECT d.date_key FROM daily_counts d
                         LEFT JOIN (SELECT date_key, SUM(count) c FROM hourly_counts GROUP BY date_key) h
                           ON h.date_key = d.date_key
                         WHERE d.count != COALESCE(h.c, 0)"
                    )
                    .is_empty(),
                    "{year} 年 daily_counts 与 hourly_counts 对不同一天"
                );
                // 2) 明细不许指向没有的天（否则「总计」与按天池子对不上）
                assert_eq!(
                    one(
                        "SELECT COUNT(*) FROM key_counts k
                         WHERE NOT EXISTS (SELECT 1 FROM daily_counts d WHERE d.date_key = k.date_key)"
                    ),
                    0,
                    "{year} 年 key_counts 存在没有 daily_counts 行的天"
                );
                assert_eq!(
                    one(
                        "SELECT COUNT(*) FROM hourly_counts h
                         WHERE NOT EXISTS (SELECT 1 FROM daily_counts d WHERE d.date_key = h.date_key)"
                    ),
                    0,
                    "{year} 年 hourly_counts 存在没有 daily_counts 行的天"
                );
                // 3) date_key 必须落在本文件所属年份内（跨年数据要靠归档分流）
                let y0 = day_key_of(NaiveDate::from_ymd_opt(year, 1, 1).expect("date"));
                let y1 = day_key_of(NaiveDate::from_ymd_opt(year + 1, 1, 1).expect("date"));
                assert_eq!(
                    one(&format!(
                        "SELECT COUNT(*) FROM daily_counts WHERE date_key < {y0} OR date_key >= {y1}"
                    )),
                    0,
                    "{year} 年库里躺着别的年份的天（归档没跑成？）"
                );
                // 4) 取值域：计数为正、时长不超过一天、小时在 0..=23
                assert_eq!(
                    one("SELECT COUNT(*) FROM daily_counts WHERE count <= 0 OR seconds < 0 OR seconds > 86400"),
                    0,
                    "{year} 年存在非正计数或越界时长"
                );
                assert_eq!(
                    one("SELECT COUNT(*) FROM hourly_counts WHERE hour < 0 OR hour > 23 OR count <= 0"),
                    0,
                    "{year} 年小时分布越界"
                );
                // 5) 设备字典完整性（统计表存 id，字典缺行就是查询看不见的幽灵数据）
                assert_eq!(
                    one("SELECT COUNT(*) FROM device_counts c
                         WHERE NOT EXISTS (SELECT 1 FROM devices d WHERE d.id = c.device_id)"),
                    0,
                    "{year} 年 device_counts 指向不存在的设备 id"
                );
                assert_eq!(
                    one("SELECT COUNT(*) FROM devices d WHERE TRIM(d.device_key) = ''"),
                    0,
                    "{year} 年设备字典有空键"
                );
                // 6) 前台应用同一天的总秒数不能超过一天
                assert!(
                    stale_days("SELECT date_key FROM app_usage GROUP BY date_key HAVING SUM(seconds) > 86400")
                        .is_empty(),
                    "{year} 年 app_usage 单天总秒数超过一天"
                );
                assert!(
                    stale_days(
                        "SELECT date_key FROM key_counts WHERE key_name IS NULL OR TRIM(key_name) = ''"
                    )
                    .is_empty(),
                    "{year} 年存在空键名"
                );
                checked += 1;
            }
            checked
        });
        match outcome {
            Some(0) => panic!("年度库都拷过来了却一个年份都没列出来，available_years 有问题"),
            Some(n) => println!("真实数据一致性：检查了 {n} 个年度库"),
            None => println!("真实数据一致性：本机无数据，跳过"),
        }
    }

    /// 同一个数字经两条独立路径算出来必须相等：core 查询函数 vs 直接 SQL。
    ///
    /// 界面上每个卡片走的是不同的（有时是缓存的）那条路，上一轮「总计」冻结
    /// 一整天、榜单顺序被打乱那类问题都属于这一族。
    #[test]
    fn real_data_core_queries_agree_with_sql() {
        let outcome = on_real_data_copy("realdata_api", || {
            let mut checked = 0usize;
            for year in queries::available_years() {
                let conn = connection::open_ro(&paths::year_db_path(year)).expect("打开副本");
                let sum = |sql: &str| -> i64 {
                    conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap_or(0)
                };

                // 年度总按键数：get_stats(None, Some(year)) 走的是整表 SUM
                let (total, ranks) = queries::get_stats(None, Some(year));
                assert_eq!(
                    total,
                    sum("SELECT COALESCE(SUM(count),0) FROM daily_counts"),
                    "{year} 年：查询函数的总数与 daily_counts 不等"
                );
                assert!(ranks.values().all(|v| *v > 0), "{year} 年榜单里有非正计数");

                // 全历史汇总（跨库遍历）不得小于单年
                let (all_total, max_day) = queries::get_alltime_summary();
                assert!(
                    all_total >= total,
                    "总计({all_total})比单年({total})还少，跨库遍历漏了年份"
                );
                if let Some((day, cnt)) = max_day {
                    assert!(cnt > 0, "「最高单日」报出非正计数：{day} = {cnt}");
                }

                // 按日查询、小时分布与聚合表必须给同一天同一个数
                let dk: i64 = conn
                    .query_row("SELECT MAX(date_key) FROM daily_counts", [], |r| r.get(0))
                    .unwrap();
                let day_count = sum(&format!(
                    "SELECT count FROM daily_counts WHERE date_key = {dk}"
                ));
                let day = date_of_day_key(dk);
                let (by_date, _) = queries::get_stats_by_date(day);
                assert_eq!(
                    by_date, day_count,
                    "{day} 这一天：get_stats_by_date 与聚合表不等"
                );
                let hourly: i64 = queries::get_hourly_stats(Some(day)).iter().sum();
                assert_eq!(hourly, day_count, "{day} 小时分布加起来不是当天总数");

                // 前台应用与设备维度
                let (app_secs, _) = queries::get_app_stats(None, Some(year));
                assert_eq!(
                    app_secs,
                    sum("SELECT COALESCE(SUM(seconds),0) FROM app_usage"),
                    "{year} 年前台应用时长与 app_usage 不等"
                );
                let (dev_total, dev_list) = queries::get_device_stats(None, Some(year));
                assert_eq!(
                    dev_total,
                    sum("SELECT COALESCE(SUM(count),0) FROM device_counts"),
                    "{year} 年设备总次数与 device_counts 不等"
                );
                assert!(
                    dev_list.iter().all(|d| d.count > 0),
                    "{year} 年设备列表里有零次数的行"
                );
                checked += 1;
            }
            checked
        });
        match outcome {
            Some(0) => panic!("年度库都拷过来了却一个年份都没列出来"),
            Some(n) => println!("真实数据口径互校：{n} 个年度库"),
            None => println!("真实数据口径互校：本机无数据，跳过"),
        }
    }

    /// 拿真实库当导入源跑一遍合并分支：已有的更大值不许被旧库顶掉，重复导入不许翻倍。
    ///
    /// 源就是目标自己那份拷贝（值完全相同），所以只有"取 MAX + 幂等"的实现能过：
    /// 写成相加会翻倍；写成覆盖则第二步（先把目标某天抬高）会红。
    #[test]
    fn real_data_import_merge_is_idempotent_and_never_lowers() {
        let outcome = on_real_data_copy("realdata_import", || {
            let year = *queries::available_years()
                .first()
                .expect("副本里至少有一个年度库");
            let dst = paths::year_db_path(year);

            // 抬高目标库某一天的时长：合并分支若写成"覆盖"，这一步会立刻被顶回
            let raised = {
                let conn = rusqlite::Connection::open(&dst).unwrap();
                let (dk, secs): (i64, i64) = conn
                    .query_row(
                        "SELECT date_key, seconds FROM daily_counts WHERE seconds > 0 LIMIT 1",
                        [],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .unwrap();
                conn.execute(
                    "UPDATE daily_counts SET seconds = seconds + 100000 WHERE date_key = ?1",
                    [dk],
                )
                .unwrap();
                (dk, secs + 100000)
            };

            // 导入源：同一份真实数据的另一份拷贝，放在另一个目录里
            let src = real_data_dir().expect("上面已经拷过一份");
            let old_dir = paths::data_dir().join("_import_src");
            std::fs::create_dir_all(&old_dir).unwrap();
            assert!(copy_year_dbs(&src, &old_dir) > 0, "源目录一个库都没拷到");

            let before = {
                let conn = rusqlite::Connection::open(&dst).unwrap();
                (
                    conn.query_row(
                        "SELECT COALESCE(SUM(seconds),0) FROM daily_counts",
                        [],
                        |r| r.get::<_, i64>(0),
                    )
                    .unwrap(),
                    conn.query_row("SELECT COALESCE(SUM(count),0) FROM daily_counts", [], |r| {
                        r.get::<_, i64>(0)
                    })
                    .unwrap(),
                )
            };

            let summary = migration::import_legacy_data(&old_dir);
            assert!(
                summary.errors.is_empty(),
                "导入真实数据报错：{:?}",
                summary.errors
            );

            let conn = rusqlite::Connection::open(&dst).unwrap();
            let after_seconds: i64 = conn
                .query_row(
                    "SELECT COALESCE(SUM(seconds),0) FROM daily_counts",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let after_count: i64 = conn
                .query_row("SELECT COALESCE(SUM(count),0) FROM daily_counts", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(
                after_seconds, before.0,
                "自导入改变了活跃时长总量（该幂等）"
            );
            assert_eq!(after_count, before.1, "自导入改变了按键总量（该幂等）");
            let kept: i64 = conn
                .query_row(
                    "SELECT seconds FROM daily_counts WHERE date_key = ?1",
                    [raised.0],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(kept, raised.1, "目标库自己更大的时长被旧库顶掉了");
            drop(conn);

            // 再来一次，仍然不动
            let again = migration::import_legacy_data(&old_dir);
            assert!(again.errors.is_empty(), "二次导入报错：{:?}", again.errors);
            let conn = rusqlite::Connection::open(&dst).unwrap();
            let third: i64 = conn
                .query_row(
                    "SELECT COALESCE(SUM(seconds),0) FROM daily_counts",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(third, before.0, "重复导入把时长叠上去了");
            year
        });
        match outcome {
            Some(year) => println!("真实数据导入回归：{year} 年库"),
            None => println!("真实数据导入回归：本机无数据，跳过"),
        }
    }
}
