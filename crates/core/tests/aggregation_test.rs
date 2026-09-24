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
        let _app = paths::test_app_dir("agg");
        let dir = _app.path().to_path_buf();
        db::queries::invalidate_years_cache();

        let config = FocusFlowConfig::load(dir.join("config.ini")).unwrap();
        let now = chrono::Utc::now().timestamp();

        // 第一轮：写入 25 条后落库
        let database =
            db::Database::init(&config, focusflow_core::listener::new_pause_flag()).unwrap();
        let writer = database.writer().unwrap().clone();
        for i in 0..25u64 {
            writer.record(&format!("K{}", i % 5), now - (25 - i) as i64);
        }
        writer.flush(true);
        assert_eq!(writer.today_count(), 25, "flush 后今日计数应为 25");
        database.shutdown(&config);

        // 第二轮：模拟重启，今日计数应从聚合表恢复
        let database2 =
            db::Database::init(&config, focusflow_core::listener::new_pause_flag()).unwrap();
        let writer2 = database2.writer().unwrap().clone();
        assert_eq!(
            writer2.today_count(),
            25,
            "重启后今日计数应从 daily_counts 恢复（回归：COUNT(*) 误用为行数）"
        );
        database2.shutdown(&config);
    }

    #[test]
    fn daily_and_hourly_aggregate_correctly() {
        let _g = guard();
        let _app = paths::test_app_dir("agg2");
        let dir = _app.path().to_path_buf();
        db::queries::invalidate_years_cache();

        let config = FocusFlowConfig::load(dir.join("config.ini")).unwrap();
        let database =
            db::Database::init(&config, focusflow_core::listener::new_pause_flag()).unwrap();
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
        let database2 =
            db::Database::init(&config, focusflow_core::listener::new_pause_flag()).unwrap();
        let (t2, _) = db::get_stats_by_date(chrono::Local::now().date_naive());
        assert_eq!(t2, 18);
        database2.shutdown(&config);
    }

    #[test]
    fn alltime_summary_totals_all_days_and_picks_max_day() {
        let _g = guard();
        let _app = paths::test_app_dir("alltime");
        let dir = _app.path().to_path_buf();
        db::queries::invalidate_years_cache();

        let config = FocusFlowConfig::load(dir.join("config.ini")).unwrap();
        let database =
            db::Database::init(&config, focusflow_core::listener::new_pause_flag()).unwrap();
        let writer = database.writer().unwrap().clone();

        // 昨天 3 次 + 今天 7 次：总计 10，最高单日为今天 7
        let today_start = focusflow_core::db::queries::today_start_ts();
        let yesterday = today_start - 86_400 + 3600;
        for _ in 0..3 {
            writer.record("A", yesterday);
        }
        for _ in 0..7 {
            writer.record("A", today_start + 60);
        }
        writer.flush(true);
        // 跨天增量可能新建上一年度的库文件，年度列表缓存必须失效后才能被扫到
        db::queries::invalidate_years_cache();

        let today = chrono::Local::now()
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();
        let (total, max_day) = db::get_alltime_summary();
        assert_eq!(total, 10, "总计应为全部日期之和（回归：只统计今日）");
        assert_eq!(
            max_day,
            Some((today.clone(), 7)),
            "最高单日应取全历史最大值"
        );

        // 与既有口径一致：最高单日 = get_alltime_max_day，总计 = get_stats(None, None)
        assert_eq!(db::get_alltime_max_day(), Some((today, 7)));
        assert_eq!(db::get_stats(None, None).0, 10);

        database.shutdown(&config);
    }

    /// 清理接口的两道闸：非正天数必须整笔拒绝，以及「保留 N 天」按含今天
    /// 的自然日计。
    ///
    /// 原先 `cutoff = today - keep_days` 不做任何校验：填 0 或负数会让 cutoff
    /// 落到今天甚至未来，一条 `DELETE ... WHERE date_key < cutoff` 连今天一起
    /// 清空，而这是不可逆操作、只靠删前快照兜底。同时它比口径多留一天
    /// （keep 10 实际留 11 天），与 `get_daily_counts` 的「含今天共 N 天」不一致。
    #[test]
    fn cleanup_refuses_non_positive_and_keeps_including_today() {
        let _g = guard();
        let _app = paths::test_app_dir("cleanup");
        let dir = _app.path().to_path_buf();
        db::queries::invalidate_years_cache();

        let config = FocusFlowConfig::load(dir.join("config.ini")).unwrap();
        let database =
            db::Database::init(&config, focusflow_core::listener::new_pause_flag()).unwrap();
        let writer = database.writer().unwrap().clone();
        // 今天与 10 天前各一天（同一年度库内，日期由写入路径自己算）
        let today_noon = focusflow_core::db::queries::today_start_ts() + 43_200;
        for _ in 0..3 {
            writer.record("A", today_noon);
        }
        for _ in 0..4 {
            writer.record("A", today_noon - 10 * 86_400);
        }
        writer.flush(true);
        database.shutdown(&config);

        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let days_with_data = || -> usize {
            db::get_daily_counts(11, None)
                .into_iter()
                .filter(|(_, c)| *c > 0)
                .count()
        };
        assert_eq!(days_with_data(), 2, "前置：今天与 10 天前各一天");

        assert_eq!(
            db::maintenance::cleanup_old_data(0).deleted,
            0,
            "0 天必须被拒绝"
        );
        assert_eq!(
            db::maintenance::cleanup_old_data(-5).deleted,
            0,
            "负数必须被拒绝"
        );
        assert_eq!(days_with_data(), 2, "拒绝时不得删掉任何数据");

        let _ = db::maintenance::cleanup_old_data(11);
        assert_eq!(days_with_data(), 2, "保留 11 天（含今天）刚好覆盖 10 天前");

        assert!(
            db::maintenance::cleanup_old_data(10).deleted > 0,
            "保留 10 天应清掉 10 天前那天"
        );
        assert_eq!(days_with_data(), 1, "含今天共 10 天不含第 11 天");
        assert_eq!(
            db::get_daily_counts(11, None)
                .into_iter()
                .find(|(d, _)| *d == today)
                .map(|(_, c)| c),
            Some(3),
            "剩下的必须是今天那 3 次"
        );
    }
}
