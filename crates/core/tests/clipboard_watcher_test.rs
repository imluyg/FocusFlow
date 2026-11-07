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
        panic!("程序自己写进剪贴板的文本不该被收录，却收到 {c:?}");
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
        if let Ok(clipboard::Captured::Text {
            text, source_app, ..
        }) = rx.recv_timeout(Duration::from_millis(200))
        {
            if text.contains("external-marker-42") {
                got = Some((text, source_app));
                break;
            }
        }
    }
    let (text, _source) = got.expect("外部复制应在 5 秒内交件");
    assert!(text.contains("external-marker-42"));

    watcher.shutdown();
    if let Some(text) = original {
        let _ = clipboard::write_text(&text);
    }
}

/// 外部进程往剪贴板里放文件（CF_HDROP）与图片（CF_DIB），监听线程应各交来一件，
/// 并且图片真的落了盘、库里那一行的类型也对。
///
/// 为什么走 PowerShell：自写抑制会过滤掉本进程的写入，只有**别的进程**设的剪贴板
/// 才是"用户复制"的形状。`Set-Clipboard -Path` 交 CF_HDROP；`Clipboard.SetImage`
/// 交 CF_DIB（不是 PNG 格式），正好覆盖我们自己解析 DIB 的那条路。
#[test]
fn watcher_captures_external_files_and_images() {
    let _lock = paths::test_app_dir_lock();
    let dir = paths::test_app_dir("clip_watch_img");
    clipboard::init_db().unwrap();

    let original = clipboard::read_text_for_test();
    let (mut watcher, rx) = clipboard::start().expect("监听线程应起来");
    std::thread::sleep(Duration::from_millis(250));
    while rx.try_recv().is_ok() {}

    fn powershell(script: &str) -> std::process::Output {
        std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-STA", "-Command", script])
            .output()
            .expect("powershell.exe 应能起来")
    }

    // ---- 文件 ----
    // 剪贴板会被两边抢（监听线程每次变化都会短暂占用）：夹具必须重试并在失败时出声。
    // 不重试时 .NET 的异常是非终止的，脚本退出码仍是 0 —— 于是"没放上去"会被读成"监控没收到"。
    let file = dir.path().join("剪贴板夹具.txt");
    std::fs::write(&file, b"fixture").unwrap();
    let script = format!(
        "$ErrorActionPreference='Stop'; $ok=$false; \
         for ($i=0; $i -lt 20; $i++) {{ try {{ Set-Clipboard -Path '{}'; $ok=$true; break }} \
         catch {{ Start-Sleep -Milliseconds 150 }} }}; \
         if (-not $ok) {{ Write-Output 'SETCLIP_FAILED'; exit 3 }}",
        file.display()
    );
    let out = powershell(&script);
    assert!(out.status.success(), "Set-Clipboard -Path 失败: {out:?}");

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut files = None;
    while Instant::now() < deadline {
        if let Ok(clipboard::Captured::Files { paths, .. }) =
            rx.recv_timeout(Duration::from_millis(200))
        {
            files = Some(paths);
            break;
        }
    }
    let files = files.expect("外部复制文件应在 15 秒内交件");
    assert!(
        files.iter().any(|p| p.ends_with("剪贴板夹具.txt")),
        "交来的路径里应有那个文件：{files:?}"
    );
    let outcome = clipboard::ingest_files(&files, "explorer.exe", clipboard::now_ms()).unwrap();
    let clipboard::IngestOutcome::Stored { id, .. } = outcome else {
        panic!("文件条目应当入库");
    };
    let item = clipboard::get(id).unwrap().unwrap();
    assert_eq!(item.kind, clipboard::ClipKind::File);
    assert!(item.content.contains("剪贴板夹具.txt"));

    // ---- 图片 ----
    let png_path = dir.path().join("夹具.png");
    let rgba: Vec<u8> = (0..4 * 4)
        .flat_map(|i| {
            if i % 2 == 0 {
                [255u8, 0, 0, 255]
            } else {
                [0u8, 0, 255, 255]
            }
        })
        .collect();
    let png = focusflow_core::capture::encode_png(&rgba, 4, 4).unwrap();
    std::fs::write(&png_path, &png).unwrap();
    let script = format!(
        "$ErrorActionPreference='Stop'; Add-Type -AssemblyName System.Windows.Forms,System.Drawing; \
         $img = [System.Drawing.Image]::FromFile('{}'); $ok=$false; \
         for ($i=0; $i -lt 20; $i++) {{ try {{ [System.Windows.Forms.Clipboard]::SetImage($img); $ok=$true; break }} \
         catch {{ Start-Sleep -Milliseconds 150 }} }}; \
         $img.Dispose(); \
         if (-not $ok) {{ Write-Output 'SETIMAGE_FAILED'; exit 3 }}; \
         Start-Sleep -Seconds 6",
        png_path.display()
    );
    let out = powershell(&script);
    assert!(
        out.status.success(),
        "SetImage 失败（夹具必须真的放上剪贴板，否则失败会被误读成监控漏收）: {out:?}"
    );

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut image = None;
    while Instant::now() < deadline {
        if let Ok(clipboard::Captured::Image { stored, .. }) =
            rx.recv_timeout(Duration::from_millis(200))
        {
            image = Some(stored);
            break;
        }
    }
    let stored = image.expect("外部复制图片应在 15 秒内交件");
    assert_eq!((stored.width, stored.height), (4, 4), "尺寸应原样读回");
    assert!(stored.png_path.exists(), "原图应落盘");
    assert!(
        stored.thumb_path.as_ref().is_some_and(|p| p.exists()),
        "缩略图应生成"
    );
    let clipboard::IngestOutcome::Stored { id, .. } =
        clipboard::ingest_image(&stored, "paint.exe", clipboard::now_ms()).unwrap()
    else {
        panic!("图片条目应当入库");
    };
    let item = clipboard::get(id).unwrap().unwrap();
    assert_eq!(item.kind, clipboard::ClipKind::Image);
    assert_eq!((item.width, item.height), (4, 4));
    assert!(item
        .image_path
        .as_deref()
        .is_some_and(|p| std::path::Path::new(p).exists()));

    watcher.shutdown();
    if let Some(text) = original {
        let _ = clipboard::write_text(&text);
    }
}
