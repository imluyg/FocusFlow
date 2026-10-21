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
        // 「0 条」单独说不完整：它必须带上"为什么是 0"，否则与"这一年本来没数据"
        // 逐字节同形，而重跑导入正是用户查"刚才到底导进去没有"的动作。
        assert!(
            summary2.skipped.iter().any(|s| s.contains("2025")),
            "源文件未变那一臂要把这一年写进 skipped，实得 {:?}",
            summary2.skipped
        );
        let cnt2: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(count), 0) FROM daily_counts",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cnt2, 3, "去重后总数不变");

        // 第三臂：改掉源文件的修改时间（大小不变）⇒ 第一道闸放行，这一轮是靠**内容指纹**
        // 才跳过的。那一臂原先只留一句 info 日志，报出来的还是一个裸 0。
        {
            let when =
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_699_991_000);
            std::fs::OpenOptions::new()
                .write(true)
                .open(&old_db)
                .unwrap()
                .set_modified(when)
                .unwrap();
        }
        let summary3 = migration::import_legacy_data(&old_dir);
        assert_eq!(
            summary3.records_by_year,
            vec![(2025, 0)],
            "明细内容逐行没变就不该再聚一遍"
        );
        assert!(
            summary3.skipped.iter().any(|s| s.contains("2025")),
            "内容指纹那一臂也要把这一年写进 skipped，实得 {:?}",
            summary3.skipped
        );
        let cnt3: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(count), 0) FROM daily_counts",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cnt3, 3, "第三轮之后总数仍不变");

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

    /// 回归：合并分支（目标年库已存在）必须把源库的**按天活跃时长**一起搬过来。
    ///
    /// 原来这条路只搬 `key_log` 明细，而明细里没有时长信息 —— 旧库的时长按天存在
    /// 独立表 `active_seconds`（旧版）或 `daily_counts.seconds`（并入后），一步都没读。
    /// 表现：换电脑把旧库并进同年库后按键数上去了，「活跃时长」却全是 0，
    /// 而旧文件还在，谁也不会想到是没搬。
    #[test]
    fn import_into_existing_year_db_carries_over_active_seconds() {
        let _g = guard();
        let _old = paths::test_app_dir("secs_old");
        let old_dir = _old.path().to_path_buf();
        let _new = paths::test_app_dir("secs_new");
        db::queries::invalidate_years_cache();

        // 三条明细同一时刻 → 同一天同一小时，不受时区与日界影响
        let ts: i64 = 1_700_000_000;
        let off = chrono::Local::now().offset().local_minus_utc() as i64;
        let dk = (ts + off) / 86_400;

        // 旧版库形态：明细 + 独立 active_seconds（daily_counts 里还没有 seconds 列）
        {
            let conn = rusqlite::Connection::open(old_dir.join("focusflow_2025.db")).unwrap();
            conn.execute_batch(
                "CREATE TABLE key_log (id INTEGER PRIMARY KEY AUTOINCREMENT, key_name TEXT NOT NULL, timestamp INTEGER NOT NULL);
                 CREATE TABLE active_seconds (date_key INTEGER PRIMARY KEY, seconds INTEGER NOT NULL);
                 CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO key_log (key_name, timestamp) VALUES ('A', ?1), ('B', ?1), ('C', ?1)",
                [ts],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO active_seconds VALUES (?1, 500), (?2, 900)",
                rusqlite::params![dk, dk + 1],
            )
            .unwrap();
        }

        // 目标年库已存在：dk 有一天（时长比旧库小，该被抬高）、
        // dk+2 是新版本自己采集的一天（旧库没有，绝不能被 0 覆盖）
        {
            let dst = paths::year_db_path(2025);
            let conn = rusqlite::Connection::open(&dst).unwrap();
            db::connection::ensure_schema(&conn, 2025).unwrap();
            conn.execute(
                "INSERT INTO daily_counts (date_key, count, seconds) VALUES (?1, 5, 42), (?2, 7, 8080)",
                rusqlite::params![dk, dk + 2],
            )
            .unwrap();
        }

        let summary = migration::import_legacy_data(&old_dir);
        assert!(summary.errors.is_empty(), "errors: {:?}", summary.errors);

        let conn = rusqlite::Connection::open(paths::year_db_path(2025)).unwrap();
        let seconds_of = |key: i64| -> i64 {
            conn.query_row(
                "SELECT seconds FROM daily_counts WHERE date_key = ?1",
                [key],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        };
        assert_eq!(
            seconds_of(dk),
            500,
            "同一天取两侧较大值，不是相加也不是覆盖"
        );
        assert_eq!(seconds_of(dk + 1), 900, "旧库独有的那一天必须补进来");
        assert_eq!(seconds_of(dk + 2), 8080, "旧库没有的天不得被清零");
        assert_eq!(
            conn.query_row(
                "SELECT count FROM daily_counts WHERE date_key = ?1",
                [dk],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            8,
            "明细照旧累加（原有 5 + 导入 3）"
        );

        // 二次导入必须幂等：时长不得翻倍
        let summary2 = migration::import_legacy_data(&old_dir);
        assert!(summary2.errors.is_empty(), "errors: {:?}", summary2.errors);
        let conn = rusqlite::Connection::open(paths::year_db_path(2025)).unwrap();
        assert_eq!(
            conn.query_row("SELECT SUM(seconds) FROM daily_counts", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            500 + 900 + 8080,
            "重复导入不得再增加活跃时长"
        );
    }

    /// 回归：源库**只剩聚合表**（没有 key_log）时，活跃时长仍要搬过来。
    ///
    /// 暂存明细聚完就丢，所以这种旧库很常见。原来那段 `has_src_table` 提前 return
    /// 在它之前，等于把这类源库整段跳过 —— 时长必须先并，再谈明细。
    #[test]
    fn import_from_aggregate_only_legacy_db_still_carries_seconds() {
        let _g = guard();
        let _old = paths::test_app_dir("agg_old");
        let old_dir = _old.path().to_path_buf();
        let _new = paths::test_app_dir("agg_new");
        db::queries::invalidate_years_cache();

        let dk: i64 = 20_500;
        {
            let conn = rusqlite::Connection::open(old_dir.join("focusflow_2025.db")).unwrap();
            conn.execute_batch(
                "CREATE TABLE daily_counts (date_key INTEGER PRIMARY KEY, count INTEGER NOT NULL, seconds INTEGER NOT NULL);
                 CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO daily_counts VALUES (?1, 99, 500), (?2, 30, 900)",
                rusqlite::params![dk, dk + 1],
            )
            .unwrap();
        }
        {
            let dst = paths::year_db_path(2025);
            let conn = rusqlite::Connection::open(&dst).unwrap();
            db::connection::ensure_schema(&conn, 2025).unwrap();
            conn.execute(
                "INSERT INTO daily_counts (date_key, count, seconds) VALUES (?1, 5, 42)",
                [dk],
            )
            .unwrap();
        }

        let summary = migration::import_legacy_data(&old_dir);
        assert!(summary.errors.is_empty(), "errors: {:?}", summary.errors);

        // 时长并进来了、按键数没有 —— 报告必须说清这件事。旧写法只回一个裸的 `Ok(0)`，
        // 调用方照样把这年记进 `year_dbs`，界面上就成了「2025 年度键鼠: 0 条」，
        // 与"这一年本来没数据"完全同形；而源库里那 99+30 次按键就是被丢下的那一份。
        // （"只剩聚合表的旧库"是本仓年度库的常态：明细聚完就丢表。）
        assert_eq!(
            summary.records_by_year,
            vec![(2025, 0)],
            "症状先钉住：报的就是这个 0"
        );
        assert!(
            summary
                .skipped
                .iter()
                .any(|s| s.contains("2025") && s.contains("只剩聚合表")),
            "被丢下的那一年必须出现在跳过清单里: {:?}",
            summary.skipped
        );

        let conn = rusqlite::Connection::open(paths::year_db_path(2025)).unwrap();
        assert_eq!(
            conn.query_row("SELECT SUM(seconds) FROM daily_counts", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            500 + 900,
            "无明细的旧库也要把时长并进来了"
        );
        assert_eq!(
            conn.query_row(
                "SELECT count FROM daily_counts WHERE date_key = ?1",
                [dk],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            5,
            "聚合计数不做累加（无法幂等去重），原有按键数必须保持原样"
        );
    }

    /// 目标年度库的导入标记**读不出来**时，那一年必须报错跳过，不能当成"没导过"再累加一遍。
    ///
    /// 两道去重闸（源文件大小+mtime、key_log 内容指纹）认的都是"标记里写着的值"，
    /// 而旧写法是 `read_import_marker(..).unwrap_or(None)` —— 一次占用、或库被同步盘
    /// 换成占位文件，就被折成"没有标记"，于是同一份旧明细被再聚一遍、那一年整体翻倍，
    /// 界面上只会显示「导入完成」。整库复制那一支早就改成"读不动就真报错"了
    /// （见 `record_import_marker` 的注释），这是剩下的读侧一半。
    ///
    /// 夹具：目标位置放一个**同名目录** ⇒ 那个"库"打不开，标记自然读不出来。
    #[test]
    fn an_unreadable_import_marker_aborts_that_year() {
        let _g = guard();
        let _old = paths::test_app_dir("mkold");
        let old_dir = _old.path().to_path_buf();
        let _new = paths::test_app_dir("mknew");
        db::queries::invalidate_years_cache();

        let old_db = old_dir.join("focusflow_2025.db");
        {
            let conn = rusqlite::Connection::open(&old_db).unwrap();
            conn.execute_batch(
                "CREATE TABLE key_log (id INTEGER PRIMARY KEY AUTOINCREMENT, key_name TEXT NOT NULL, timestamp INTEGER NOT NULL);
                 CREATE INDEX idx_timestamp ON key_log(timestamp);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO key_log (key_name, timestamp) VALUES ('A', 100), ('B', 200)",
                [],
            )
            .unwrap();
        }

        // 目标库位置换成同名目录：合并分支里第一次碰它就是读标记
        std::fs::create_dir_all(paths::year_db_path(2025)).unwrap();

        let summary = migration::import_legacy_data(&old_dir);
        assert!(
            !summary.year_dbs.contains(&2025),
            "读不出标记的一年不能被记成已导入: {:?}",
            summary.year_dbs
        );
        assert!(
            summary
                .errors
                .iter()
                .any(|e| e.contains("2025") && e.contains("标记读不出来")),
            "必须把原因说出去，而不是安静地什么都不做: {:?}",
            summary.errors
        );
    }

    /// 附属库的 `-wal` 也要一起搬：那里面是还没 checkpoint 进主库的记录。
    ///
    /// 年度库那一支早就为此改成"搬不动就回滚报错"，附属库这一支以前只
    /// `std::fs::copy(&src, &dst)` 那一个文件 —— 旧目录里那段尾巴永久留在原地，
    /// 而界面报的是「导入完成」（记账/日程/浏览历史都算）。
    #[test]
    fn aux_import_carries_the_wal_tail() {
        let _g = guard();
        let _old = paths::test_app_dir("auxwal_old");
        let old_dir = _old.path().to_path_buf();
        let _new = paths::test_app_dir("auxwal_new");
        db::queries::invalidate_years_cache();

        let src_aux = old_dir.join("focusflow_accounting.db");
        std::fs::write(&src_aux, b"main-db-bytes").unwrap();
        std::fs::write(
            format!("{}-wal", src_aux.display()),
            b"the-uncheckpointed-tail",
        )
        .unwrap();

        let summary = migration::import_legacy_data(&old_dir);
        assert!(
            summary
                .copied_aux
                .iter()
                .any(|s| s.contains("focusflow_accounting.db")),
            "附属库应报已复制: {:?}",
            summary.copied_aux
        );
        let dst_wal = paths::data_dir().join("focusflow_accounting.db-wal");
        assert_eq!(
            std::fs::read(&dst_wal).ok(),
            Some(b"the-uncheckpointed-tail".to_vec()),
            "WAL 尾巴必须跟到目标目录，否则那一段永远留在旧目录"
        );
    }

    /// `-wal` 搬不过去时：不许报"已导入"，而且要把留档那份回滚回正名。
    ///
    /// 回滚这件事不是讲究：`backup_before_overwrite` 已经把旧库连 sidecar 一起 rename 走了，
    /// 不回滚的话目录里躺着的是一份"主库是新的、WAL 缺失"的残缺库，
    /// 下一次导入会把它当成"目标库已存在"去合并 —— 读的就是这份残缺。
    /// 回滚成功后也不许再报"给你留了一份备份"：那份文件已经回到正名上了。
    #[test]
    fn aux_import_rolls_back_when_the_wal_cannot_be_copied() {
        let _g = guard();
        let _old = paths::test_app_dir("auxrb_old");
        let old_dir = _old.path().to_path_buf();
        let _new = paths::test_app_dir("auxrb_new");
        db::queries::invalidate_years_cache();

        // 让那趟 sidecar 复制必然失败：源目录里的 `-wal` 是个同名**目录**
        // （同步盘或清理脚本留下的畸形物就是这个形态），`std::fs::copy` 会直接拒绝。
        let src_aux = old_dir.join("focusflow_accounting.db");
        std::fs::write(&src_aux, b"new-from-old-dir").unwrap();
        std::fs::create_dir_all(format!("{}-wal", src_aux.display())).unwrap();

        // 目标位置先放一份现有库：它会被留档，然后必须被回滚回正名
        let dst_aux = paths::data_dir().join("focusflow_accounting.db");
        std::fs::write(&dst_aux, b"existing-current-db").unwrap();

        let summary = migration::import_legacy_data(&old_dir);
        assert!(
            summary.copied_aux.is_empty(),
            "sidecar 搬不过去就不能报已导入: {:?}",
            summary.copied_aux
        );
        assert!(
            summary.errors.iter().any(|e| e.contains("WAL")),
            "要说出是哪一步没成: {:?}",
            summary.errors
        );
        assert_eq!(
            std::fs::read(&dst_aux).ok(),
            Some(b"existing-current-db".to_vec()),
            "目标库必须还是导入前那一份（已回滚）"
        );
        assert!(
            summary.backed_up_aux.is_empty(),
            "留档已经搬回正名，报告里不该再留一条指向不存在文件的备份: {:?}",
            summary.backed_up_aux
        );
    }

    /// 清空之后再导入同一份旧目录，数据必须真的回来。
    ///
    /// `reset_all_data` 只删六张统计表，`meta` 里的 `imported_src_*`（源库大小/mtime +
    /// 明细指纹）留着 —— 而它记的是「这份源库已经导过」。清空后用导入把旧数据找回来
    /// 是最自然的恢复路径；旧实现会被指纹直接跳过、报 0 条，数据永远回不来。
    #[test]
    fn a_reset_clears_import_fingerprints_so_the_same_source_can_be_reimported() {
        let _g = guard();
        let _old = paths::test_app_dir("reset_reimport_old");
        let old_dir = _old.path().to_path_buf();
        let _new = paths::test_app_dir("reset_reimport_new");
        db::queries::invalidate_years_cache();
        // `reset_all_data` 的快照会读配置（max_backups）；在临时目录里初始化一份，
        // 别让它去碰上一个用例留下的全局单例。
        let _ = focusflow_core::config::instance();

        let old_db = old_dir.join("focusflow_2026.db");
        {
            let conn = rusqlite::Connection::open(&old_db).unwrap();
            conn.execute_batch(
                "CREATE TABLE key_log (
                    key_name TEXT NOT NULL,
                    timestamp INTEGER NOT NULL,
                    PRIMARY KEY (key_name, timestamp)
                 ) WITHOUT ROWID;
                 CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO key_log (key_name, timestamp) VALUES ('A', 100), ('B', 200), ('C', 300)",
                [],
            )
            .unwrap();
        }

        let first = migration::import_legacy_data(&old_dir);
        assert_eq!(
            first.records_by_year,
            vec![(2026, 3)],
            "首次导入应有 3 条: {first:?}"
        );
        assert!(first.errors.is_empty(), "首次导入报错: {:?}", first.errors);

        let report = db::maintenance::reset_all_data();
        assert!(
            report.failed_years.is_empty(),
            "清空失败: {:?}",
            report.failed_years
        );

        let target = paths::year_db_path(2026);
        let query_i64 = |sql: &str| -> i64 {
            let conn = rusqlite::Connection::open(&target).unwrap();
            conn.query_row(sql, [], |r| r.get(0)).unwrap()
        };
        assert_eq!(
            query_i64("SELECT COALESCE(SUM(count),0) FROM daily_counts"),
            0,
            "前置条件：清空后统计应为 0"
        );
        assert_eq!(
            query_i64("SELECT COUNT(*) FROM meta WHERE key GLOB 'imported_src_*'"),
            0,
            "清空必须同时清掉导入指纹，否则重导会被它跳过"
        );

        // 同一份源库、同一个目录再导一次：数据必须原样回来
        let again = migration::import_legacy_data(&old_dir);
        assert!(again.errors.is_empty(), "重导报错: {:?}", again.errors);
        assert_eq!(
            again.records_by_year,
            vec![(2026, 3)],
            "清空后重导同一份旧目录不能被指纹跳过（实得 {:?}）",
            again.records_by_year
        );
        assert_eq!(
            query_i64("SELECT COALESCE(SUM(count),0) FROM daily_counts"),
            3,
            "重导之后统计要真的回来"
        );
    }
}
