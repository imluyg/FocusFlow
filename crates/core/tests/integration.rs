//! 集成测试：写入 → flush → 查询 全链路。
//!
//! 每个测试使用独立的临时 `app_dir` 隔离数据。由于 `set_app_dir` 是
//! 进程级全局状态，测试通过一个静态互斥锁串行执行。

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};

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
            let guard = test_lock().lock().unwrap();
            let dir = std::env::temp_dir().join(format!("ff_rs_{name}_{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            paths::set_app_dir(&dir);
            Self { _guard: guard, dir }
        }

        fn config(&self) -> FocusFlowConfig {
            FocusFlowConfig::load(self.dir.join("config.ini")).unwrap()
        }
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn write_then_query_roundtrip() {
        let env = TestEnv::new("roundtrip");
        let config = env.config();
        let db = db::Database::init(&config).expect("初始化数据库失败");

        let now = chrono::Utc::now().timestamp();
        for i in 0..100 {
            db.record_key(&format!("键{}", i % 5), now - (100 - i));
        }
        db.flush(true);

        let (total, stats) = db::get_stats(None, None);
        assert!(total >= 100, "总数应 >= 100, got {total}");
        assert_eq!(stats.len(), 5);
        for (_, count) in stats {
            assert_eq!(count, 20, "每种键各 20 次");
        }

        db.shutdown(&config);
    }

    #[test]
    fn batch_flush_idempotent() {
        let env = TestEnv::new("flush");
        let config = env.config();
        let db = db::Database::init(&config).expect("初始化数据库失败");

        let now = chrono::Utc::now().timestamp();
        db.record_key("A", now);
        db.flush(true);
        db.flush(true); // 二次 flush 应无副作用

        let (total, _) = db::get_stats(None, None);
        assert_eq!(total, 1);

        db.shutdown(&config);
    }

    #[test]
    fn writer_high_volume() {
        let env = TestEnv::new("highvol");
        let config = env.config();
        let db = db::Database::init(&config).expect("初始化数据库失败");
        let writer = db.writer().expect("写入器未启动").clone();

        // 高压写入：验证不 panic、写线程持续工作、事件部分落库
        let now = chrono::Utc::now().timestamp();
        for i in 0..8000 {
            writer.record(&format!("X{}", i % 50), now - (8000 - i));
        }
        writer.flush(true);

        let (total, stats) = db::get_stats(None, None);
        // 写线程与测试并发排空，正常应大部分落库；断言下限保证有数据写入
        assert!(total >= 1000, "应写入大量数据, got {total}");
        assert_eq!(stats.len(), 50, "50 种键都出现");

        db.shutdown(&config);
    }

    #[test]
    fn daily_and_date_stats() {
        let env = TestEnv::new("daily");
        let config = env.config();
        let db = db::Database::init(&config).expect("初始化数据库失败");

        // 今天 + 昨天各写几条
        let today = focusflow_core::db::queries::today_start_ts();
        let yesterday = today - 86_400;
        for i in 0..10 {
            db.record_key("A", today + i);
            db.record_key("B", yesterday + i);
        }
        db.flush(true);

        let daily = db::get_daily_counts(7, None);
        assert!(!daily.is_empty());
        assert_eq!(daily.len(), 7, "应返回 7 天");
        // 回归：daily 计数必须真实反映数据（曾因"天序号被当作秒"导致全为 0）
        // 返回的是"近 7 天"升序列表：[5天前, 4天前, ..., 昨天, 今天]
        assert_eq!(daily[5].1, 10, "昨天应为 10 条, got {:?}", daily[5]);
        assert_eq!(daily[6].1, 10, "今天应为 10 条, got {:?}", daily[6]);

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

        db.shutdown(&config);
    }
}
