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
        // 旧目录（导入源）+ 当前目录（目标）。test_app_dir 会把全局 app_dir 切到自己
        // 身上，所以后建的那个才是目标目录，顺序不能反。
        let _old = paths::test_app_dir("old");
        let old_dir = _old.path().to_path_buf();
        let _new = paths::test_app_dir("new");
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
    }

    /// 回归：附属库是整文件覆盖，导入前必须把**现有**库留档。
    ///
    /// 导入目录由用户在对话框里自选，选错目录（或新版本里已经记了几个月账）
    /// 时，当前数据不能就这么被旧库替换掉且无处找回。
    #[test]
    fn import_backs_up_existing_aux_db_before_overwrite() {
        let _g = guard();
        // 标签必须与同文件其它用例区分开：两个用例都各自切 app_dir，
        // 共用目录名会互相把对方的 seed 数据删掉（曾表现为"目标库突然不存在"）
        let _old = paths::test_app_dir("auxbak_old");
        let old_dir = _old.path().to_path_buf();
        let _new = paths::test_app_dir("auxbak_new");
        let new_dir = _new.path().to_path_buf();
        db::queries::invalidate_years_cache();

        let seed = |path: &std::path::Path, item: &str| {
            let conn = rusqlite::Connection::open(path).unwrap();
            conn.execute(
                "CREATE TABLE expenses (id INTEGER PRIMARY KEY, item_name TEXT)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO expenses (id, item_name) VALUES (1, ?1)",
                [item],
            )
            .unwrap();
        };
        let item_of = |path: &std::path::Path| -> Option<String> {
            let conn = rusqlite::Connection::open(path).ok()?;
            conn.query_row("SELECT item_name FROM expenses WHERE id = 1", [], |r| {
                r.get(0)
            })
            .ok()
        };

        let aux = "focusflow_accounting.db";
        seed(&old_dir.join(aux), "来自旧目录");
        // 注意：程序目录是 app_dir，数据落在 app_dir/data（paths::data_dir）
        let data_dir = new_dir.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let dst = data_dir.join(aux);
        seed(&dst, "当前新数据");

        let summary = migration::import_legacy_data(&old_dir);

        assert!(
            summary.copied_aux.contains(&aux.to_string()),
            "附属库仍应被导入覆盖"
        );
        assert_eq!(
            summary.backed_up_aux.len(),
            1,
            "覆盖前必须留档一份: {:?}",
            summary.backed_up_aux
        );
        assert!(summary.errors.is_empty(), "errors: {:?}", summary.errors);

        // 被覆盖的旧内容留在 <原名>.import-backup-<时间戳> 里
        let kept = std::path::PathBuf::from(&summary.backed_up_aux[0]);
        assert!(kept.exists(), "留档文件应存在: {}", kept.display());
        assert_eq!(
            item_of(&kept).as_deref(),
            Some("当前新数据"),
            "留档必须是**被覆盖掉的那份**当前数据"
        );
        // 目标库被导入内容替换
        assert_eq!(
            item_of(&dst).as_deref(),
            Some("来自旧目录"),
            "导入后目标库应为旧目录内容"
        );
    }

    /// 回归：目标年库**已有当天聚合行**时，导入必须累加而不是插入。
    ///
    /// 三条 `INSERT ... SELECT ... GROUP BY` 原本没有 ON CONFLICT，而新版一直
    /// 在跑的年库早已有当天的 daily_counts 行 —— 一撞主键整段迁移回滚：旧明细
    /// 聚不上、暂存表也清不掉，且 `Database::init` 每次启动都重跑一遍重复失败。
    /// 更糟的是导入返回值取自暂存表的插入条数（那一步是成功的），于是界面显示
    /// 「导入 N 条」而统计数据毫无变化。
    #[test]
    fn import_into_year_db_with_existing_rows_accumulates() {
        let _g = guard();
        let _old = paths::test_app_dir("merge_old");
        let old_dir = _old.path().to_path_buf();
        let _new = paths::test_app_dir("merge_new");
        db::queries::invalidate_years_cache();

        // 三条明细共用一个时间戳：同日同小时，不受时区与小时边界影响
        let ts: i64 = 1_700_000_000;
        let off = chrono::Local::now().offset().local_minus_utc() as i64;
        let dk = (ts + off) / 86_400;
        let hour = ((ts + off) / 3_600) % 24;

        // 旧版库：含一个需修正的 Ctrl+A（应并入已存在的 A）
        {
            let conn = rusqlite::Connection::open(old_dir.join("focusflow_2025.db")).unwrap();
            conn.execute_batch(
                "CREATE TABLE key_log (id INTEGER PRIMARY KEY AUTOINCREMENT, key_name TEXT NOT NULL, timestamp INTEGER NOT NULL);
                 CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO key_log (key_name, timestamp) VALUES ('A', ?1), ('B', ?1), ('Ctrl+A', ?1)",
                [ts],
            )
            .unwrap();
        }

        // 目标年库：新版已在跑，当天已有聚合行（daily 还带着已算好的 seconds）
        {
            let dst = paths::year_db_path(2025);
            let conn = rusqlite::Connection::open(&dst).unwrap();
            db::connection::ensure_schema(&conn, 2025).unwrap();
            conn.execute(
                "INSERT INTO daily_counts (date_key, count, seconds) VALUES (?1, 5, 42)",
                [dk],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO hourly_counts (date_key, hour, count) VALUES (?1, ?2, 5)",
                rusqlite::params![dk, hour],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO key_counts (date_key, key_name, count) VALUES (?1, 'A', 5)",
                [dk],
            )
            .unwrap();
        }

        let summary = migration::import_legacy_data(&old_dir);
        assert!(summary.errors.is_empty(), "errors: {:?}", summary.errors);

        let conn = rusqlite::Connection::open(paths::year_db_path(2025)).unwrap();
        let sum_of =
            |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap() };
        assert_eq!(
            sum_of("SELECT SUM(count) FROM daily_counts"),
            8,
            "原有 5 + 导入 3"
        );
        assert_eq!(
            sum_of("SELECT SUM(count) FROM hourly_counts"),
            8,
            "小时维度同样累加"
        );
        assert_eq!(
            sum_of("SELECT SUM(count) FROM key_counts"),
            8,
            "键名维度同样累加"
        );
        assert_eq!(
            sum_of("SELECT count FROM key_counts WHERE key_name = 'A'"),
            7,
            "Ctrl+A 的 1 次应并入已存在的 A（5 + 1 + 1）"
        );
        assert_eq!(
            sum_of("SELECT COUNT(*) FROM key_counts WHERE key_name = 'Ctrl+A'"),
            0,
            "并入后不应再留 Ctrl+A 行"
        );
        assert_eq!(
            conn.query_row(
                "SELECT seconds FROM daily_counts WHERE date_key = ?1",
                [dk],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            42,
            "旧版明细没有活跃时长，冲突分支绝不能把它清零"
        );
        assert_eq!(
            sum_of("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='key_log'"),
            0,
            "聚合成功后应丢弃暂存表"
        );

        // 再跑一次迁移不得翻倍（明细与聚合同事务，不存在重放路径）
        db::maintenance::migrate_v2();
        let conn = rusqlite::Connection::open(paths::year_db_path(2025)).unwrap();
        assert_eq!(
            conn.query_row("SELECT SUM(count) FROM daily_counts", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            8,
            "重复迁移应保持幂等"
        );
    }
}
