//! 剪贴板监听线程的端到端测试：外部写入能收到，程序自己写的不收录。
//!
//! 这条用例会短暂改动系统剪贴板；开始前保存原文本，结束时用"自己写"的通道还回去
//! （自身写入带序号，监听线程会跳过，不会把恢复动作也记成一次复制）。

use std::time::{Duration, Instant};

use focusflow_core::clipboard;
use focusflow_core::paths;

#[test]
fn watcher_captures_external_text_and_skips_own_writes() {
    let _lock = paths::test_app_dir_lock();
    let _dir = paths::test_app_dir("clip_watch");
    clipboard::init_db().unwrap();

    let original = clipboard::read_text_for_test();
    let (mut watcher, rx) = clipboard::start().expect("监听线程应起来");

    // 监听器刚注册时可能补来一条"当前剪贴板"的通知；先把它排空，别把旧内容算成我们的。
    std::thread::sleep(Duration::from_millis(250));
    while rx.try_recv().is_ok() {}

    clipboard::write_text("focusflow-self-marker").unwrap();
    std::thread::sleep(Duration::from_millis(400));
    if let Ok(c) = rx.try_recv() {
        panic!(
            "程序自己写进剪贴板的文本不该被收录，却收到 {:?}（来源 {:?}，序号 {}）",
            c.text, c.source_app, c.seq
        );
    }

    // 真正的外部写入：另起一个进程（clip.exe）设剪贴板。所有者不是我们自己的窗口，
    // 正好绕开"自身写入"那三道闸。
    let out = std::process::Command::new("cmd")
        .arg("/C")
        .arg("echo external-marker-42| clip")
        .output()
        .expect("clip.exe 应能起来");
    assert!(out.status.success(), "clip.exe 退出码不为 0: {out:?}");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut got = None;
    while Instant::now() < deadline {
        if let Ok(c) = rx.recv_timeout(Duration::from_millis(200)) {
            if c.text.contains("external-marker-42") {
                got = Some(c);
                break;
            }
        }
    }
    let got = got.expect("外部复制应在 5 秒内交件");
    assert!(got.text.contains("external-marker-42"));

    watcher.shutdown();
    if let Some(text) = original {
        let _ = clipboard::write_text(&text);
    }
}
