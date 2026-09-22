//! 集成测试：写入 → flush → 查询 全链路。
//!
//! 每个测试使用独立的临时 `app_dir` 隔离数据。由于 `set_app_dir` 是
//! 进程级全局状态，测试通过一个静态互斥锁串行执行。

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};

    use chrono::Datelike;

    use focusflow_core::config::FocusFlowConfig;
    use focusflow_core::db;
    use focusflow_core::paths;

    /// 进程级测试串行锁：`set_app_dir` 是全局状态，测试必须互斥执行。
    fn test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// 独立测试环境：持有全局锁，设置独立 app_dir。
    struct TestEnv {
        _guard: std::sync::MutexGuard<'static, ()>,
        dir: PathBuf,
    }

    impl TestEnv {
        fn new(name: &str) -> Self {
            // 不用 unwrap()：前一个用例失败时会把锁弄毒，unwrap 让后续用例全部
            // 以一样的 PoisonError 崩掉，真正的失败就被盖住了（本仓库其他测试
            // 也统一用 into_inner 口径）。
            let guard = test_lock().lock().unwrap_or_else(|e| e.into_inner());
            let dir = std::env::temp_dir().join(format!("ff_rs_{name}_{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            paths::set_app_dir(&dir);
            Self { _guard: guard, dir }
        }

        fn config(&self) -> FocusFlowConfig {
            FocusFlowConfig::load(self.dir.join("config.ini")).unwrap()
        }
    }

    /// 关闭数据库，并**等到写线程真的退出**才返回。
    ///
    /// `DbWriter::stop()` 只等 3 秒，超时就放弃（并把内存里的增量写进恢复文件）。
    /// 被放弃的那个线程仍在继续排空，而它落库用的年度库路径是在 flush 那一刻从
    /// 全局 `app_dir` 现取的 —— 此时下一个用例已经 `set_app_dir` 到自己的目录，
    /// 于是上一轮的残留事件会写进**别人的库**（实测：`batch_flush_idempotent`
    /// 的 1 变成 2，且只在负载较高的 `cargo test --workspace` 全量跑时偶发）。
    /// 用例在释放串行锁之前必须等到线程退出，残留事件才会落在自己的目录里。
    fn shutdown_and_wait(db: &db::Database, config: &FocusFlowConfig) {
        let writer = db.writer().cloned();
        db.shutdown(config);
        let Some(w) = writer else { return };
        for _ in 0..200 {
            if !w.is_alive() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("写线程 10 秒后仍未退出：它随后会在别的用例目录里落库");
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            // 先放掉本线程的只读连接：Windows 下目录里还有打开的句柄时
            // remove_dir_all 会静默失败，于是每跑一次留一个目录。
            focusflow_core::db::connection::clear_ro_cache();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// 写入用的基准时间：今天本地 00:00 起算的若干秒。
    ///
    /// 不用 `Utc::now().timestamp()`：跨年那一瞬间 `now - 100` 会落到上一年，
    /// 而写线程现在按 date_key 的年份落库（这是正确行为，见 writer 的跨年测试），
    /// 那样本测试就变成看运行时刻的偶然结果。
    fn today_base_ts(offset_secs: i64) -> i64 {
        focusflow_core::db::queries::today_start_ts() + offset_secs
    }

    #[test]
    fn write_then_query_roundtrip() {
        let env = TestEnv::new("roundtrip");
        let config = env.config();
        let db = db::Database::init(&config).expect("初始化数据库失败");

        for i in 0..100 {
            db.record_key(&format!("键{}", i % 5), today_base_ts(i));
        }
        db.flush(true);

        let (total, stats) = db::get_stats(None, None);
        assert!(total >= 100, "总数应 >= 100, got {total}");
        assert_eq!(stats.len(), 5);
        for (_, count) in stats {
            assert_eq!(count, 20, "每种键各 20 次");
        }

        shutdown_and_wait(&db, &config);
    }

    #[test]
    fn batch_flush_idempotent() {
        let env = TestEnv::new("flush");
        let config = env.config();
        let db = db::Database::init(&config).expect("初始化数据库失败");

        db.record_key("A", today_base_ts(10));
        db.flush(true);
        db.flush(true); // 二次 flush 应无副作用

        let (total, _) = db::get_stats(None, None);
        assert_eq!(total, 1);

        shutdown_and_wait(&db, &config);
    }

    #[test]
    fn writer_high_volume() {
        let env = TestEnv::new("highvol");
        let config = env.config();
        let db = db::Database::init(&config).expect("初始化数据库失败");
        let writer = db.writer().expect("写入器未启动").clone();

        // 高压写入：验证不 panic、写线程持续工作、事件部分落库
        for i in 0..8000 {
            writer.record(&format!("X{}", i % 50), today_base_ts(i));
        }
        writer.flush(true);

        let (total, stats) = db::get_stats(None, None);
        // 写线程与测试并发排空，正常应大部分落库；断言下限保证有数据写入
        assert!(total >= 1000, "应写入大量数据, got {total}");
        assert_eq!(stats.len(), 50, "50 种键都出现");

        shutdown_and_wait(&db, &config);
    }

    #[test]
    fn daily_and_date_stats() {
        let env = TestEnv::new("daily");
        let config = env.config();
        let db = db::Database::init(&config).expect("初始化数据库失败");

        // 今天 + 昨天各写几条。
        // 用 Date 逐日回退再取正午，而不是 `today_start_ts() - 86400`：
        // 那样在 1 月 1 日会把"昨天"落到上一年，而写线程现在按 date_key 的年份
        // 落库（正确行为），断言就会随运行日期漂移。
        let today_date = chrono::Local::now().date_naive();
        let yesterday_date = today_date - chrono::Days::new(1);
        // 极端情况（1 月 1 日）：昨天的日期落在上一年，换用同年的最后两天，
        // 保证"两天数据都在同一年份库"这一测试前提成立。
        let (d1, d2) = if yesterday_date.year() == today_date.year() {
            (yesterday_date, today_date)
        } else {
            let dec31 = chrono::NaiveDate::from_ymd_opt(today_date.year() - 1, 12, 31).unwrap();
            let dec30 = dec31 - chrono::Days::new(1);
            (dec30, dec31)
        };
        let noon = |d: chrono::NaiveDate| {
            d.and_hms_opt(12, 0, 0)
                .unwrap()
                .and_local_timezone(chrono::Local)
                .single()
                .unwrap()
                .timestamp()
        };
        for i in 0..10 {
            db.record_key("A", noon(d2) + i);
            db.record_key("B", noon(d1) + i);
        }
        db.flush(true);

        let daily = db::get_daily_counts(7, None);
        assert!(!daily.is_empty());
        assert_eq!(daily.len(), 7, "应返回 7 天");
        // 回归：daily 计数必须真实反映数据（曾因"天序号被当作秒"导致全为 0）
        // 返回的是"近 7 天"升序列表：[5天前, 4天前, ..., 昨天, 今天]
        assert_eq!(daily[5].1, 10, "前一天应为 10 条, got {:?}", daily[5]);
        assert_eq!(daily[6].1, 10, "后一天应为 10 条, got {:?}", daily[6]);

        let hour = focusflow_core::db::queries::get_hourly_stats(None);
        assert_eq!(hour.len(), 24);
        assert!(hour.iter().sum::<i64>() >= 10, "今日小时分布应有数据");

        let wd = focusflow_core::db::queries::get_weekday_stats(7);
        assert!(!wd.is_empty(), "至少 1 天有数据");
        assert!(
            wd.values().sum::<i64>() >= 20,
            "星期分布总数应 >= 20, got {:?}",
            wd
        );

        shutdown_and_wait(&db, &config);
    }
}
