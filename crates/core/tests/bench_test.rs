//! 写入性能与稳健性基准测试。
//!
//! 1. 吞吐：批量写入（单事务）在"队列可容纳"前提下的速度
//! 2. 有界保护：极端突发时丢弃而非内存失控

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::OnceLock;
    use std::time::Instant;

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
    fn write_throughput_benchmark() {
        let _g = guard();
        let _app = paths::test_app_dir("bench");
        let dir = _app.path().to_path_buf();
        db::queries::invalidate_years_cache();

        let config = FocusFlowConfig::load(dir.join("config.ini")).unwrap();
        let database =
            db::Database::init(&config, focusflow_core::listener::new_pause_flag()).unwrap();
        let writer = database.writer().unwrap().clone();

        // 持续高频注入 5 万条（模拟游戏/高速输入，约 5 万/秒），
        // 验证写线程在远高于人类输入下的消化能力
        let count = 50_000u64;
        let now = chrono::Utc::now().timestamp();
        let start = Instant::now();
        for i in 0..count {
            writer.record(&format!("K{}", i % 26), now - (count as i64 - i as i64));
            // 每 50 条 sleep 1ms → 约 5 万条/秒注入速率
            if i % 50 == 49 {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        writer.flush(true);
        let elapsed = start.elapsed();

        let (total, _) = db::get_stats(None, None);
        let _per_sec = count as f64 / elapsed.as_secs_f64();
        println!(
            "注入速率 ~5万/秒，写入 {count} 条耗时 {:.3}s，落库 {total}（丢失 {}）",
            elapsed.as_secs_f64(),
            count - total as u64
        );
        // 5 万条/秒（比人类快 1000 倍）下应基本全部落库
        assert!(
            total >= count as i64 * 97 / 100,
            "应写入绝大部分, got {total}"
        );

        database.shutdown(&config);
    }

    #[test]
    fn burst_aggregation_no_loss() {
        let _g = guard();
        let _app = paths::test_app_dir("bench2");
        let dir = _app.path().to_path_buf();
        db::queries::invalidate_years_cache();

        let config = FocusFlowConfig::load(dir.join("config.ini")).unwrap();
        let database =
            db::Database::init(&config, focusflow_core::listener::new_pause_flag()).unwrap();
        let writer = database.writer().unwrap().clone();

        // 微秒级突发注入 10 万条：内存聚合不丢数据、不 OOM、不 panic
        let now = chrono::Utc::now().timestamp();
        let start = Instant::now();
        for i in 0..100_000u64 {
            writer.record(&format!("X{}", i % 10), now);
        }
        let inject_time = start.elapsed();

        // 等写线程落库
        writer.flush(true);
        let (total, _) = db::get_stats(None, None);
        println!(
            "突发 10 万条注入耗时 {:.1}ms，聚合落库 {total}（丢失 {}）",
            inject_time.as_millis(),
            100_000 - total
        );
        // 聚合设计下所有事件都应计入（仅按键维度聚合，不丢量）
        assert_eq!(total, 100_000, "聚合不应丢失事件");

        database.shutdown(&config);
    }

    #[test]
    fn large_db_query_performance() {
        let _g = guard();
        let _app = paths::test_app_dir("bench3");
        let dir = _app.path().to_path_buf();
        db::queries::invalidate_years_cache();

        let config = FocusFlowConfig::load(dir.join("config.ini")).unwrap();
        let database =
            db::Database::init(&config, focusflow_core::listener::new_pause_flag()).unwrap();
        let writer = database.writer().unwrap().clone();

        // 预置 20 万条数据
        let now = chrono::Utc::now().timestamp();
        for i in 0..200_000u64 {
            writer.record(&format!("K{}", i % 40), now - (200_000 - i) as i64);
            if i % 100 == 99 {
                std::thread::sleep(std::time::Duration::from_micros(100));
            }
        }
        writer.flush(true);

        // 测量各查询耗时
        let t = Instant::now();
        let (total, stats) = db::get_stats(None, None);
        let stats_t = t.elapsed();
        println!(
            "20万条 get_stats: {:.1}ms (total={total}, keys={})",
            stats_t.as_secs_f64() * 1000.0,
            stats.len()
        );

        let t = Instant::now();
        let daily = db::get_daily_counts(30, None);
        let daily_t = t.elapsed();
        println!(
            "20万条 get_daily_counts(30): {:.1}ms ({}天)",
            daily_t.as_secs_f64() * 1000.0,
            daily.len()
        );

        let t = Instant::now();
        let hourly = db::queries::get_hourly_stats(None);
        let hourly_t = t.elapsed();
        println!(
            "20万条 get_hourly_stats: {:.1}ms ({}h)",
            hourly_t.as_secs_f64() * 1000.0,
            hourly.len()
        );

        // 查询应都在毫秒级（worker 每 2 秒跑一次，UI 无感）
        assert!(
            stats_t.as_secs_f64() < 1.0,
            "get_stats 过慢: {:.2}s",
            stats_t.as_secs_f64()
        );
        assert!(daily_t.as_secs_f64() < 1.0, "get_daily_counts 过慢");
        assert!(hourly_t.as_secs_f64() < 1.0, "get_hourly_stats 过慢");

        database.shutdown(&config);
    }

    /// 度量「每轮 UI 刷新的聚合序列」随年度库个数的变化。
    ///
    /// 统计线程每轮会跑 6-8 次「遍历全部年度库」的查询（stats/app/device/daily/
    /// hourly/alltime），而只读连接池上限是 4 且按 LRU 淘汰 —— 年库一旦超过 4 个，
    /// 每趟都会重开被挤掉的连接。这个基准用来判断值不值得为此合并扫描趟数，
    /// 而不是凭猜重构五个查询函数。只打印，不做紧断言（避免 CI 抖动）。
    #[test]
    fn aggregation_cost_vs_year_db_count() {
        let _g = guard();
        use chrono::Datelike;
        use std::time::Instant;

        /// 造 n 个年度库；返回的守卫 Drop 时删掉这次的临时目录。
        ///
        /// 原来这里手写 `remove_dir_all + create_dir_all` 复用同一个目录名，收尾
        /// 那行根本没有 —— 每跑一次留一份。现在每轮 seed 各用一套新目录，用完回收。
        fn seed(years: &[i32]) -> paths::TestAppDir {
            let app = paths::test_app_dir("bench_years");
            db::queries::invalidate_years_cache();
            let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
            for &y in years {
                let conn = rusqlite::Connection::open(paths::year_db_path(y)).unwrap();
                db::connection::ensure_schema(&conn, y).unwrap();
                // 造数整体一个事务：autocommit 下每年 2 万条 = 2 万次 WAL fsync，
                // 基准本身会跑到分钟级，量不到查询时间
                conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
                let base = chrono::NaiveDate::from_ymd_opt(y, 1, 1)
                    .unwrap()
                    .signed_duration_since(epoch)
                    .num_days();
                for d in 0..365 {
                    let dk = base + d;
                    conn.execute(
                        "INSERT OR REPLACE INTO daily_counts (date_key, count, seconds) VALUES (?1, ?2, ?3)",
                        rusqlite::params![dk, 5000 + d, 3600_i64],
                    )
                    .unwrap();
                    for h in 0..24 {
                        conn.execute(
                            "INSERT OR REPLACE INTO hourly_counts (date_key, hour, count) VALUES (?1, ?2, ?3)",
                            rusqlite::params![dk, h, 200],
                        )
                        .unwrap();
                    }
                    for k in 0..30 {
                        conn.execute(
                            "INSERT OR REPLACE INTO key_counts (date_key, key_name, count) VALUES (?1, ?2, ?3)",
                            rusqlite::params![dk, format!("K{k}"), 100],
                        )
                        .unwrap();
                    }
                    conn.execute(
                        "INSERT OR REPLACE INTO app_usage (date_key, app_name, seconds) VALUES (?1, 'app.exe', 600)",
                        [dk],
                    )
                    .unwrap();
                    conn.execute(
                        "INSERT OR REPLACE INTO device_counts (date_key, device_id, count) VALUES (?1, 1, 900)",
                        [dk],
                    )
                    .unwrap();
                }
                conn.execute(
                    "INSERT OR REPLACE INTO devices (id, device_key, name, kind) VALUES (1, 'k1', 'dev', 'keyboard')",
                    [],
                )
                .unwrap();
                conn.execute_batch("COMMIT;").unwrap();
            }
            db::queries::invalidate_years_cache();
            app
        }

        let mut report = String::new();
        for n in [1usize, 4, 7] {
            let now_year = chrono::Local::now().date_naive().year();
            let years: Vec<i32> = ((now_year - n as i32 + 1)..=now_year).collect();
            let _app = seed(&years);
            let t = Instant::now();
            for _ in 0..10 {
                let _ = db::get_stats(Some(30), None);
                let _ = db::get_daily_counts(30, None);
                let _ = db::queries::get_hourly_stats(None);
                let _ = db::get_app_stats(Some(30), None);
                let _ = db::get_device_stats(Some(30), None);
                let _ = db::get_alltime_summary();
            }
            let per = t.elapsed().as_secs_f64() * 1000.0 / 10.0;
            report.push_str(&format!("  {n} 个年度库：每轮聚合 {per:.2}ms\n"));
        }
        println!("聚合序列耗时随年度库个数变化：\n{report}");
    }
    /// 空壳年度库值多少钱：3 个有数据的库 + 4 个被删空的壳。
    ///
    /// `available_years()` 按文件名取年份时，那 4 个壳每轮都要各开一次连接、
    /// 把整套聚合查询再跑一遍（一行数据也没有）。改成按「有没有聚合行」过滤后，
    /// 它们不该再进入扫描。只打印，不做时序断言（CI 抖动）；对照数字来自
    /// 临时注释掉 retain 过滤后的同机重跑。
    #[test]
    fn empty_year_shells_cost_scan_passes() {
        let _g = guard();
        use chrono::Datelike;
        use std::time::Instant;

        let _app = paths::test_app_dir("bench_shells");
        db::queries::invalidate_years_cache();
        let now_year = chrono::Local::now().date_naive().year();
        let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        // 3 个真实年库 + 4 个只有表结构的壳（排在真实库之前，模拟往年遗留）
        for off in 0..7i32 {
            let y = now_year - off;
            let conn = rusqlite::Connection::open(paths::year_db_path(y)).unwrap();
            db::connection::ensure_schema(&conn, y).unwrap();
            if off >= 4 {
                continue; // 空壳：只有表结构
            }
            let base = chrono::NaiveDate::from_ymd_opt(y, 1, 1)
                .unwrap()
                .signed_duration_since(epoch)
                .num_days();
            conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
            for d in 0..365 {
                let dk = base + d;
                conn.execute(
                    "INSERT OR REPLACE INTO daily_counts (date_key, count, seconds) VALUES (?1, ?2, ?3)",
                    rusqlite::params![dk, 5000 + d, 3600_i64],
                )
                .unwrap();
                for h in 0..24 {
                    conn.execute(
                        "INSERT OR REPLACE INTO hourly_counts (date_key, hour, count) VALUES (?1, ?2, ?3)",
                        rusqlite::params![dk, h, 200],
                    )
                    .unwrap();
                }
                for k in 0..30 {
                    conn.execute(
                        "INSERT OR REPLACE INTO key_counts (date_key, key_name, count) VALUES (?1, ?2, ?3)",
                        rusqlite::params![dk, format!("K{k}"), 100],
                    )
                    .unwrap();
                }
                conn.execute(
                    "INSERT OR REPLACE INTO app_usage (date_key, app_name, seconds) VALUES (?1, 'app.exe', 600)",
                    [dk],
                )
                .unwrap();
                conn.execute(
                    "INSERT OR REPLACE INTO device_counts (date_key, device_id, count) VALUES (?1, 1, 900)",
                    [dk],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT OR REPLACE INTO devices (id, device_key, name, kind) VALUES (1, 'k1', 'dev', 'keyboard')",
                [],
            )
            .unwrap();
            conn.execute_batch("COMMIT;").unwrap();
        }
        db::queries::invalidate_years_cache();
        let scanned = db::queries::available_years();
        let t = Instant::now();
        for _ in 0..10 {
            let _ = db::get_stats(Some(30), None);
            let _ = db::get_daily_counts(30, None);
            let _ = db::queries::get_hourly_stats(None);
            let _ = db::get_app_stats(Some(30), None);
            let _ = db::get_device_stats(Some(30), None);
            let _ = db::get_alltime_summary();
        }
        let per = t.elapsed().as_secs_f64() * 1000.0 / 10.0;
        // 7 个文件里只有 off 0..=3 这 4 个年库有数据，另外 3 个是空壳
        println!(
            "7 个年度库文件（{} 个空壳）：available_years 返回 {} 个年份 {}，每轮聚合 {per:.2}ms",
            7 - 4,
            scanned.len(),
            if scanned.len() == 4 {
                "（壳已滤掉）"
            } else {
                "（壳没滤掉！）"
            }
        );
        assert_eq!(
            scanned.len(),
            4,
            "空壳不该进入年份列表 —— 这条断言让基准测试本身也能守住过滤行为"
        );
    }
}

// force rebuild
