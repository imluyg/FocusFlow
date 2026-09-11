//! 聚合写入/读取回归测试。

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::OnceLock;

    use focusflow_core::config::FocusFlowConfig;
    use focusflow_core::db;
    use focusflow_core::paths;

    fn test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        test_lock().lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn today_count_persists_across_restart() {
        let _g = guard();
        let dir = std::env::temp_dir().join(format!("ff_agg_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        paths::set_app_dir(&dir);
        db::queries::invalidate_years_cache();

        let config = FocusFlowConfig::load(dir.join("config.ini")).unwrap();
        let now = chrono::Utc::now().timestamp();

        // 第一轮：写入 25 条后落库
        let database = db::Database::init(&config).unwrap();
        let writer = database.writer().unwrap().clone();
        for i in 0..25u64 {
            writer.record(&format!("K{}", i % 5), now - (25 - i) as i64);
        }
        writer.flush(true);
        assert_eq!(writer.today_count(), 25, "flush 后今日计数应为 25");
        database.shutdown(&config);

        // 第二轮：模拟重启，今日计数应从聚合表恢复
        let database2 = db::Database::init(&config).unwrap();
        let writer2 = database2.writer().unwrap().clone();
        assert_eq!(
            writer2.today_count(),
            25,
            "重启后今日计数应从 daily_counts 恢复（回归：COUNT(*) 误用为行数）"
        );
        database2.shutdown(&config);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn daily_and_hourly_aggregate_correctly() {
        let _g = guard();
        let dir = std::env::temp_dir().join(format!("ff_agg2_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        paths::set_app_dir(&dir);
        db::queries::invalidate_years_cache();

        let config = FocusFlowConfig::load(dir.join("config.ini")).unwrap();
        let database = db::Database::init(&config).unwrap();
        let writer = database.writer().unwrap().clone();

        let today_start = focusflow_core::db::queries::today_start_ts();
        // 今天 00:30：10 次 A + 5 次 B；今天 02:00：3 次 A（保证同一天、不同小时）
        let t1 = today_start + 1800;
        let t2 = today_start + 7200;
        for _ in 0..10 {
            writer.record("A", t1);
        }
        for _ in 0..5 {
            writer.record("B", t1);
        }
        for _ in 0..3 {
            writer.record("A", t2);
        }
        writer.flush(true);

        let (total, stats) = db::get_stats(None, None);
        assert_eq!(total, 18, "总计数应为 18");
        assert_eq!(stats.get("A"), Some(&13));
        assert_eq!(stats.get("B"), Some(&5));

        let (today_total, today_stats) = db::get_stats_by_date(chrono::Local::now().date_naive());
        assert_eq!(today_total, 18, "今日总数应为 18（回归：SUM 而非 COUNT）");
        assert_eq!(today_stats.get("A"), Some(&13));

        let hourly = db::queries::get_hourly_stats(None);
        assert_eq!(hourly[0], 15, "00 点小时应为 15 次");
        assert_eq!(hourly[2], 3, "02 点小时应为 3 次");

        // 重启后今日总数依旧正确
        database.shutdown(&config);
        let database2 = db::Database::init(&config).unwrap();
        let (t2, _) = db::get_stats_by_date(chrono::Local::now().date_naive());
        assert_eq!(t2, 18);
        database2.shutdown(&config);

        std::fs::remove_dir_all(&dir).ok();
    }
}
