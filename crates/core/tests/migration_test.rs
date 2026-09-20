//! 旧数据导入测试：从旧目录迁移年度库（去重）+ 附属库。

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::OnceLock;

    use focusflow_core::db;
    use focusflow_core::migration;
    use focusflow_core::paths;

    fn test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        test_lock().lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn import_legacy_year_db_dedup() {
        let _g = guard();
        // 旧目录
        let old_dir = std::env::temp_dir().join(format!("ff_old_{}", std::process::id()));
        // 当前目录（目标）
        let new_dir = std::env::temp_dir().join(format!("ff_new_{}", std::process::id()));
        // pid 可能被操作系统复用：先清掉上次运行残留，避免旧库干扰去重与计数断言
        std::fs::remove_dir_all(&old_dir).ok();
        std::fs::remove_dir_all(&new_dir).ok();
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::create_dir_all(&new_dir).unwrap();
        paths::set_app_dir(&new_dir);
        db::queries::invalidate_years_cache();

        // 构造旧库（Python 版 schema）
        let old_db = old_dir.join("focusflow_2025.db");
        {
            let conn = rusqlite::Connection::open(&old_db).unwrap();
            conn.execute_batch(
                "CREATE TABLE key_log (id INTEGER PRIMARY KEY AUTOINCREMENT, key_name TEXT NOT NULL, timestamp INTEGER NOT NULL);
                 CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE INDEX idx_timestamp ON key_log(timestamp);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO key_log (key_name, timestamp) VALUES ('A', 100), ('B', 200), ('C', 300)",
                [],
            )
            .unwrap();
        }
        // 旧附属库
        let old_acc = old_dir.join("focusflow_accounting.db");
        {
            let conn = rusqlite::Connection::open(&old_acc).unwrap();
            conn.execute(
                "CREATE TABLE expenses (id INTEGER PRIMARY KEY, item_name TEXT)",
                [],
            )
            .unwrap();
        }

        // 执行导入
        let summary = migration::import_legacy_data(&old_dir);
        assert_eq!(summary.year_dbs, vec![2025], "应导入 2025 库");
        assert_eq!(summary.records_by_year, vec![(2025, 3)], "应导入 3 条");
        assert!(summary
            .copied_aux
            .contains(&"focusflow_accounting.db".to_string()));
        assert!(summary.errors.is_empty(), "errors: {:?}", summary.errors);

        // 验证目标库：聚合表有数据，暂存表已丢弃
        // （key_log 只在导入时按需创建，聚合完成后连表一起丢掉，不再常年占 4 页）
        let new_db = paths::year_db_path(2025);
        assert!(new_db.exists());
        let conn = rusqlite::Connection::open(&new_db).unwrap();
        let cnt: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(count), 0) FROM daily_counts",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cnt, 3);
        let staged: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='key_log'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(staged, 0, "聚合完成后应丢弃暂存表 key_log");

        // 二次导入应幂等（去重，不再增加）
        let summary2 = migration::import_legacy_data(&old_dir);
        assert_eq!(
            summary2.records_by_year,
            vec![(2025, 0)],
            "二次导入应为 0 新增"
        );
        let cnt2: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(count), 0) FROM daily_counts",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cnt2, 3, "去重后总数不变");

        // 用 CLI 查询验证
        let (total, _) = db::get_stats(None, Some(2025));
        assert_eq!(total, 3);

        std::fs::remove_dir_all(&old_dir).ok();
        std::fs::remove_dir_all(&new_dir).ok();
    }
}
