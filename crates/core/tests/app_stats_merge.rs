//! 回归测试：应用排行跨天合并。
//!
//! 曾有 bug：`app_usage` 主键是 (date_key, app_name)，同一应用跨多天有多行，
//! 查询侧裸选行 collect 进 HashMap 时同名覆盖（只剩最后一天），榜上时长被吃掉一大截
//! （多天周期下 Top N 覆盖度远低于 100%，今日单天无重复键所以看不出异常）。
//! 修复为 SQL `GROUP BY app_name` 聚合，本测试锁定该行为。

use chrono::{Datelike, Local};
use focusflow_core::db::{self, connection};
use focusflow_core::paths;
use rusqlite::Connection;

/// 本地日期 → date_key（纪元天数，与 `day_key_of_ts` 同口径）。
fn day_key_of(date: chrono::NaiveDate) -> i64 {
    (date - chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()).num_days()
}

#[test]
fn app_stats_merges_across_days() {
    let dir = std::env::temp_dir().join(format!("ff_app_merge_{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).ok();
    paths::set_app_dir(&dir);

    let today = Local::now().date_naive();
    let yesterday = today - chrono::Days::new(1);
    let (dk_today, dk_yesterday) = (day_key_of(today), day_key_of(yesterday));

    let conn: Connection = connection::open_rw(&paths::year_db_path(today.year())).unwrap();
    connection::ensure_schema(&conn, today.year()).unwrap();
    // 同一应用两天各一行 + 另一应用单行
    for (dk, sec) in [(dk_yesterday, 100), (dk_today, 50)] {
        conn.execute(
            "INSERT INTO app_usage (date_key, app_name, seconds) VALUES (?1, 'A', ?2)",
            [dk, sec],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO app_usage (date_key, app_name, seconds) VALUES (?1, 'B', 10)",
        [dk_yesterday],
    )
    .unwrap();
    drop(conn);

    // 全部历史：total = 两行 A + 一行 B，A 必须是跨天累加的 150 而不是 50
    let (total, map) = db::get_app_stats(None, None);
    assert_eq!(total, 160, "总秒数应为全部行之和");
    assert_eq!(map.len(), 2);
    assert_eq!(map["A"], 150, "同名应用跨天必须累加（回归：曾只剩最后一天）");
    assert_eq!(map["B"], 10);

    // 按日期：只含该日行（B 只在昨天）
    let (t1, m1) = db::get_app_stats_by_date(today);
    assert_eq!(t1, 50, "今日只有 A 的 50 秒");
    assert_eq!(m1["A"], 50);

    // 7 天窗口：两行都在窗口内
    let (t7, m7) = db::get_app_stats(Some(7), None);
    assert_eq!(t7, 160);
    assert_eq!(m7["A"], 150);

    std::fs::remove_dir_all(&dir).ok();
}
