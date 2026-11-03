//! 附属库那份复用连接池（方案 3.9.1 的 A 档）。
//!
//! 这一族判据要钉的是**一对矛盾**：复用要真发生（否则 17 ms 那笔开销一分没省），
//! 而句柄必须能在改名与删源之前放干净（否则 Windows 上"更改数据文件夹"与备份轮转
//! 会变成半途而废的两份历史）。只测前一半会留一条很贵的坑，只测后一半等于没上这个池。

use focusflow_core::db::connection;

/// 同一线程内问同一份文件三次，只该真的开一次库。
///
/// 观察量是"开库次数"而不是耗时：耗时断言在共享机器上必然抖（本仓的耗时探针一律只报数、
/// 不判绿）。而"三次问 = 一次开库"正是 A 档要买的那件事本身。
#[test]
fn three_questions_about_one_file_open_it_once() {
    let _lock = focusflow_core::paths::test_app_dir_lock();
    let app = focusflow_core::paths::test_app_dir("aux_pool_reuse");
    connection::clear_aux_cache();
    let path = app.path().join("data").join("aux_probe.db");
    std::fs::create_dir_all(path.parent().expect("路径该有上一层")).expect("建数据目录");
    let before = connection::aux_opens_for_test();
    for _ in 0..3 {
        let conn = connection::aux_conn(&path, |_| {}).expect("取连接该成功");
        // 真的用它一次：空连接被 SQLite 优化掉的话，下面的计数就没有意义
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        assert!(
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS probe (k INTEGER PRIMARY KEY, v INTEGER);"
            )
            .is_ok(),
            "这条连接得真能用"
        );
        drop(conn);
    }
    let opened = connection::aux_opens_for_test() - before;
    assert_eq!(opened, 1, "三次取用该只开一次库，实开 {opened} 次");
    assert_eq!(
        connection::aux_pool_len_for_test(),
        1,
        "用完要放回池里（这一格就是那条还握着文件的句柄）"
    );
}

/// `clear_aux_cache()` 必须真的把句柄放掉：放下之后那份文件要能被改名。
///
/// 这一条是这套取舍的另一半，也是它唯一可测的那一半 —— 备份轮转与"更改数据文件夹"
/// 都靠这一步。第二次取用还得再开一次库（代次失效），否则清完池却拿着旧句柄继续用。
#[test]
fn clearing_the_pool_gives_the_file_back() {
    let _lock = focusflow_core::paths::test_app_dir_lock();
    let app = focusflow_core::paths::test_app_dir("aux_pool_clear");
    let dir = app.path().join("data");
    std::fs::create_dir_all(&dir).expect("建数据目录");
    connection::clear_aux_cache();
    let path = dir.join("rename_me.db");
    {
        let conn = connection::aux_conn(&path, |_| {}).expect("取连接该成功");
        conn.execute_batch("CREATE TABLE probe (k INTEGER);")
            .expect("建一张表");
    }
    assert_eq!(
        connection::aux_pool_len_for_test(),
        1,
        "夹具前提：句柄在池里"
    );
    connection::clear_aux_cache();
    assert_eq!(
        connection::aux_pool_len_for_test(),
        0,
        "清过池本线程就该把句柄放下"
    );
    std::fs::rename(&path, dir.join("renamed.db")).expect("清完池之后这份文件必须能改名");
    let before = connection::aux_opens_for_test();
    let conn = connection::aux_conn(&dir.join("renamed.db"), |_| {}).expect("改名后还能再取");
    conn.execute_batch("SELECT count(*) FROM sqlite_master;")
        .expect("这条新连接该能用");
    assert_eq!(
        connection::aux_opens_for_test() - before,
        1,
        "清过池之后下一次取用必须重开，不许复用已作废的句柄"
    );
}

/// 别的线程清过池，本线程下一次取用要自愈（与只读池同一条代次机制）。
///
/// 不测这一半的话，"备份线程清完池、GUI 线程继续拿着旧句柄"就是那条最贵的漏：
/// 它不影响正确性读数，只让改名与删除一直失败，而界面上一个字都不说。
#[test]
fn another_threads_clear_is_honoured_on_next_use() {
    let _lock = focusflow_core::paths::test_app_dir_lock();
    let app = focusflow_core::paths::test_app_dir("aux_pool_gen");
    let dir = app.path().join("data");
    std::fs::create_dir_all(&dir).expect("建数据目录");
    connection::clear_aux_cache();
    let path = dir.join("cross_thread.db");
    {
        let conn = connection::aux_conn(&path, |_| {}).expect("取连接");
        conn.execute_batch("CREATE TABLE probe (k INTEGER);")
            .expect("建表");
    }
    let before = connection::aux_opens_for_test();
    // 从另一根线程失效（本线程的池不会被动到，只能靠代次在下一次取用时作废）
    std::thread::spawn(|| {
        connection::clear_aux_cache();
    })
    .join()
    .expect("清池那根线程该正常结束");
    let conn = connection::aux_conn(&path, |_| {}).expect("再取一次");
    conn.execute_batch("SELECT 1;").expect("新连接该能用");
    assert_eq!(
        connection::aux_opens_for_test() - before,
        1,
        "别的线程清过池之后，本线程必须重开（复用陈旧句柄就是改名一直失败的那一族）"
    );
}

/// 四本附属库都从池里取连接，不许有人留着自己 `Connection::open` 的老路子。
///
/// 判据钉的是**调用形态**（`aux_conn(&路径` 那一句），不是"某个函数还在"：
/// 留着函数名而偷偷换回直开的写法，屏上什么都正常，只是那 17 ms 一分没省。
#[test]
fn all_four_aux_databases_borrow_from_the_pool() {
    // 路径相对 `crates/core`（`CARGO_MANIFEST_DIR` 就是它），不是仓库根
    for (file, opener) in [
        ("src/accounting.rs", "fn open()"),
        ("src/pomodoro.rs", "fn open_local()"),
        ("src/scheduler.rs", "fn open()"),
        ("src/edge_history.rs", "fn open_local()"),
    ] {
        let src =
            std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(file))
                .unwrap_or_else(|e| panic!("读 {file} 失败: {e}"));
        let at = src
            .find(opener)
            .unwrap_or_else(|| panic!("{file} 里找不到 {opener}（改名了要跟着改这条判据）"));
        let body = &src[at..at + src[at..].find("\n}").expect("函数体没有结束？") + 2];
        let flat: String = body.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            flat.contains("connection::aux_conn(&"),
            "{file} 的 {opener} 不再走池了：那一趟开库的开销就回来了"
        );
        assert!(
            !flat.contains("Connection::open("),
            "{file} 的 {opener} 里还留着直开的写法：绕过池就是每次都重开一次库"
        );
    }
}
