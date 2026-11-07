//! 剪贴板历史库的行为测试：去重、置顶、搜索、清空、片段与次数台账。
//!
//! 隔离目录与串行锁是硬要求：`app_dir` 是进程级全局，两个用例并行切目录会互相看到
//! 对方的库（症状是随机红、单跑永远绿）。

use focusflow_core::clipboard::{self, IngestOutcome};
use focusflow_core::paths;

fn setup(tag: &str) -> paths::TestAppDir {
    let dir = paths::test_app_dir(tag);
    clipboard::init_db().unwrap();
    dir
}

#[test]
fn init_is_idempotent_and_empty_db_reads_zero() {
    let _lock = paths::test_app_dir_lock();
    let _dir = setup("clip_empty");
    clipboard::init_db().unwrap();
    assert!(clipboard::list_recent(50).unwrap().is_empty());
    let s = clipboard::stats(7).unwrap();
    assert_eq!(
        (s.today, s.window_count, s.total_copies, s.total_items),
        (0, 0, 0, 0)
    );
    assert!(clipboard::db_path().starts_with(_dir.path()));
}

#[test]
fn same_hash_updates_time_and_source_but_keeps_pin() {
    let _lock = paths::test_app_dir_lock();
    let _dir = setup("clip_dedupe");
    let t = clipboard::now_ms();
    let first = clipboard::ingest("同一段文字", "notepad.exe", t).unwrap();
    let IngestOutcome::Stored { id, dedup } = first else {
        panic!("正常文本应当入库");
    };
    assert!(!dedup);
    assert!(clipboard::set_pinned(id, true).unwrap());

    let second = clipboard::ingest("同一段文字", "chrome.exe", t + 1000).unwrap();
    assert_eq!(second, IngestOutcome::Stored { id, dedup: true });

    let items = clipboard::list_recent(10).unwrap();
    assert_eq!(items.len(), 1, "同 hash 不新增第二条");
    assert_eq!(
        items[0].source_app, "chrome.exe",
        "来源要跟着最新一次复制走"
    );
    assert_eq!(items[0].copied_at, t + 1000, "时间要置顶");
    assert!(items[0].pinned, "重新复制不能抹掉用户显式固定");

    let s = clipboard::stats(7).unwrap();
    assert_eq!(s.total_copies, 2, "复制了几次就是几次，去重只影响条目");
    assert_eq!(s.total_items, 1);
}

#[test]
fn search_is_substring_and_ascii_case_insensitive() {
    let _lock = paths::test_app_dir_lock();
    let _dir = setup("clip_search");
    let t = clipboard::now_ms();
    clipboard::ingest("Hello World", "", t).unwrap();
    clipboard::ingest("你好，世界", "", t + 1).unwrap();
    clipboard::ingest("unrelated", "", t + 2).unwrap();

    assert_eq!(clipboard::search("hello", 10).unwrap().len(), 1);
    assert_eq!(clipboard::search("世界", 10).unwrap().len(), 1);
    assert_eq!(clipboard::search("zzz", 10).unwrap().len(), 0);
    assert_eq!(
        clipboard::search("  ", 10).unwrap().len(),
        3,
        "空白查询等同于列出最近"
    );
}

#[test]
fn search_words_are_and_across_fields() {
    let _lock = paths::test_app_dir_lock();
    let _dir = setup("clip_search_words");
    let t = clipboard::now_ms();
    clipboard::ingest("alpha beta gamma", "editor.exe", t).unwrap();
    clipboard::ingest("alpha only", "editor.exe", t + 1).unwrap();
    clipboard::ingest("beta only", "chrome.exe", t + 2).unwrap();
    clipboard::ingest_files(&["C:\\work\\report.txt".to_string()], "explorer.exe", t + 3).unwrap();

    // 旧实现要求 "alpha beta" 连续出现，这里只应命中第一条。
    let hits = clipboard::search("alpha beta", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].content.starts_with("alpha beta"));

    // 来源应用也参与匹配（旧实现只看 content）。
    let by_source = clipboard::search("chrome", 10).unwrap();
    assert_eq!(by_source.len(), 1);
    assert_eq!(by_source[0].content, "beta only");

    // 文件路径按 JSON 里的反斜杠匹配。
    let by_path = clipboard::search("C:\\work\\report", 10).unwrap();
    assert_eq!(by_path.len(), 1);
    assert_eq!(by_path[0].kind, clipboard::ClipKind::File);
}

#[test]
fn search_prefixes_filter_by_kind_and_source() {
    let _lock = paths::test_app_dir_lock();
    let _dir = setup("clip_search_prefix");
    let t = clipboard::now_ms();
    clipboard::ingest("plain text body", "editor.exe", t).unwrap();
    clipboard::ingest_files(&["D:\\a\\b.pdf".to_string()], "explorer.exe", t + 1).unwrap();

    assert_eq!(clipboard::search("text:", 10).unwrap().len(), 1);
    assert_eq!(clipboard::search("file:", 10).unwrap().len(), 1);
    assert_eq!(clipboard::search("img:", 10).unwrap().len(), 0);
    assert_eq!(clipboard::search("app:explorer", 10).unwrap().len(), 1);
    assert_eq!(clipboard::search("app:editor body", 10).unwrap().len(), 1);
    assert_eq!(
        clipboard::search("unknown:token", 10).unwrap().len(),
        0,
        "不认识的前缀当普通文本"
    );
}

#[test]
fn search_can_return_more_than_the_old_popup_cap() {
    let _lock = paths::test_app_dir_lock();
    let _dir = setup("clip_search_cap");
    let t = clipboard::now_ms();
    for i in 0..120 {
        clipboard::ingest(&format!("common item {i}"), "", t + i).unwrap();
    }
    assert_eq!(clipboard::search("common", 500).unwrap().len(), 120);
}

#[test]
fn clear_keeps_pinned_and_delete_is_by_id() {
    let _lock = paths::test_app_dir_lock();
    let _dir = setup("clip_clear");
    let t = clipboard::now_ms();
    let a = match clipboard::ingest("a", "", t).unwrap() {
        IngestOutcome::Stored { id, .. } => id,
        _ => panic!("a 应当入库"),
    };
    let b = match clipboard::ingest("b", "", t + 1).unwrap() {
        IngestOutcome::Stored { id, .. } => id,
        _ => panic!("b 应当入库"),
    };
    clipboard::set_pinned(a, true).unwrap();
    assert_eq!(clipboard::clear_unpinned().unwrap(), 1);
    let items = clipboard::list_recent(10).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id, a);

    assert!(clipboard::delete(a).unwrap());
    assert!(clipboard::list_recent(10).unwrap().is_empty());
    assert!(!clipboard::delete(b).unwrap(), "已经删掉/清掉的不再报成功");
}

#[test]
fn snippets_can_be_added_touched_and_deleted() {
    let _lock = paths::test_app_dir_lock();
    let _dir = setup("clip_snippets");
    let id = clipboard::add_snippet("", "第一行\n第二行").unwrap();
    let list = clipboard::snippets().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "第一行", "空名字用首行兜底");
    assert_eq!(list[0].use_count, 0);

    clipboard::touch_snippet(id).unwrap();
    assert_eq!(clipboard::snippets().unwrap()[0].use_count, 1);
    assert!(clipboard::delete_snippet(id).unwrap());
    assert!(clipboard::snippets().unwrap().is_empty());
    assert!(
        clipboard::add_snippet("x", "   ").is_err(),
        "空内容不该建片段"
    );
}

#[test]
fn stats_separate_apps_and_count_window() {
    let _lock = paths::test_app_dir_lock();
    let _dir = setup("clip_stats");
    let t = clipboard::now_ms();
    clipboard::ingest("one", "notepad.exe", t).unwrap();
    clipboard::ingest("two", "notepad.exe", t + 1).unwrap();
    clipboard::ingest("three", "chrome.exe", t + 2).unwrap();

    let s = clipboard::stats(7).unwrap();
    assert_eq!(s.total_copies, 3);
    assert_eq!(s.today, 3);
    assert_eq!(s.per_app[0], ("notepad.exe".to_string(), 2));
    assert_eq!(s.per_app[1], ("chrome.exe".to_string(), 1));
}

#[test]
fn stats_window_is_calendar_days_including_today() {
    let _lock = paths::test_app_dir_lock();
    let _dir = setup("clip_stats_window");
    let day = 86_400_000i64;
    let now = clipboard::now_ms();
    clipboard::ingest("today", "", now).unwrap();
    clipboard::ingest("six days ago", "", now - 6 * day).unwrap();
    clipboard::ingest("seven days ago", "", now - 7 * day).unwrap();

    let s = clipboard::stats(7).unwrap();
    assert_eq!(
        s.window_count, 2,
        "最近 7 天＝今天＋前 6 天；第 7 天前那条不该算进来"
    );
    assert_eq!(
        clipboard::stats(1).unwrap().window_count,
        1,
        "最近 1 天只算今天（旧口径会把昨天一起算进来）"
    );
}

#[test]
fn bulk_pin_and_delete_and_snippet_rename() {
    let _lock = paths::test_app_dir_lock();
    let _dir = setup("clip_bulk");
    let t = clipboard::now_ms();
    let a = match clipboard::ingest("bulk-a", "", t).unwrap() {
        IngestOutcome::Stored { id, .. } => id,
        _ => panic!("bulk-a 应当入库"),
    };
    let b = match clipboard::ingest("bulk-b", "", t + 1).unwrap() {
        IngestOutcome::Stored { id, .. } => id,
        _ => panic!("bulk-b 应当入库"),
    };

    assert_eq!(clipboard::set_pinned_many(&[a, b], true).unwrap(), 2);
    assert!(clipboard::list_recent(10).unwrap().iter().all(|i| i.pinned));

    let sid = clipboard::add_snippet("old-name", "snippet body").unwrap();
    assert!(clipboard::rename_snippet(sid, "new-name").unwrap());
    assert_eq!(clipboard::snippets().unwrap()[0].name, "new-name");
    assert!(
        clipboard::rename_snippet(sid, "   ").is_err(),
        "空白名不该写进去"
    );

    assert_eq!(clipboard::delete_many(&[a, b]).unwrap(), 2);
    assert!(clipboard::list_recent(10).unwrap().is_empty());
    assert_eq!(
        clipboard::delete_many(&[]).unwrap(),
        0,
        "空 id 列表不报错也不动库"
    );
}
