//! 截图的"找到它"入口：托盘一项直达最近那张，省掉"专门去某个文件夹里挑"。
//!
//! 刻意只做 Rust 侧：不加 `#[tauri::command]`、不动 `capabilities/default.json` ——
//! 覆盖层那回就是因为 label 不在任何 capability 里，第一个 `invoke` 就失败。
//! 这里没有前端参与，所以那条面根本不铺开。

use std::path::{Path, PathBuf};

/// 目录里最近的一张 PNG：按 mtime 取最新，平票按文件名字典序取大的那个
/// （截图名是 `snip_<时间戳>_<宽>x<高>`，同一秒里后写的那个名字更大）。
///
/// 只认 `.png`（大小写都收）且必须是普通文件：子目录、`.tmp` 残骸都不算，
/// 否则托盘那点一下会"选中一个打不开的目录"。
pub fn latest_png(dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, String, PathBuf)> = None;
    let entries = match std::fs::read_dir(dir) {
        Ok(it) => it,
        Err(_) => return None,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let is_png = path
            .extension()
            .is_some_and(|e| e.to_string_lossy().eq_ignore_ascii_case("png"));
        if !is_png {
            continue;
        }
        let mtime = match path.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            // 读不到时间就退回"按名字比"，别整张丢掉：退回的值取 Unix epoch，
            // 保证它只会输给真的有时间的文件。
            Err(_) => std::time::UNIX_EPOCH,
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let better = match &best {
            None => true,
            Some((b_time, b_name, _)) => (mtime, name.as_str()) > (*b_time, b_name.as_str()),
        };
        if better {
            best = Some((mtime, name, path));
        }
    }
    best.map(|(_, _, p)| p)
}

/// 组装 `explorer.exe` 的参数：有文件就"打开所在目录并选中它"，没有就只开目录。
///
/// 两个会把失败**藏起来**的写法在这里一次做对：
/// - 必须是反斜杠绝对路径。`E:/a/b.png` 这种正斜杠形式 Explorer 不认，
///   它会静默退化成"打开文档文件夹"，既不报错也不弹提示。
/// - 引号只许包在整串外面（`"/select,E:\a b\c.png"`），不许只包住路径
///   （`/select,"E:\a b\c.png"` 走 CreateProcess 时那对引号会留在参数里）。
///   交给 `Command` 正常传参即可：std 在遇到空格时正是把**整个 token** 引起来。
pub fn explorer_args(dir: &Path, target: Option<&Path>) -> Vec<String> {
    match target {
        Some(file) => {
            let p = file.display().to_string().replace('/', "\\");
            vec![format!("/select,{p}")]
        }
        None => vec![dir.display().to_string()],
    }
}

/// 直达最近一张截图。**返回值是给人看的落点描述**，调用方必须写进日志：
/// 托盘点下去"没反应"是本功能唯一故障形态，不吭声就等于把失败吞掉。
pub fn reveal_last_screenshot() -> Result<String, String> {
    let dir = focusflow_core::paths::screenshots_dir();
    let target = latest_png(&dir);
    let args = explorer_args(&dir, target.as_deref());

    let mut cmd = std::process::Command::new("explorer");
    for a in &args {
        cmd.arg(a);
    }
    // Explorer 成功了也常返回非零（甚至 1），所以只看"能不能起进程"，
    // 不等它、也不拿退出码当判据 —— 拿退出码判会把每次成功都记成失败。
    let _child = cmd
        .spawn()
        .map_err(|e| format!("启动 explorer 失败：{e}（目标 {}）", args.join(" ")))?;
    Ok(match target {
        Some(file) => format!("已在资源管理器里选中 {}", file.display()),
        None => format!("截图目录是空的，已打开 {}", dir.display()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, name: &str, mtime_secs: u64, content: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime_secs);
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(t))
            .unwrap();
        path
    }

    #[test]
    fn latest_png_picks_newest_and_ignores_junk() {
        let _lock = crate::app_dir_lock();
        let app = focusflow_core::paths::test_app_dir("reveal_latest");
        let dir = app.path().join("shots");
        std::fs::create_dir_all(&dir).unwrap();

        touch(&dir, "snip_20260929_030000_800x600.png", 1_000, "old");
        let newest = touch(&dir, "snip_20260929_031000_800x600.png", 2_000, "new");
        // 干扰项：更晚时间但非 PNG、同名目录、以及一个没有后缀的文件
        touch(&dir, "notes.txt", 3_000, "not an image");
        std::fs::create_dir_all(dir.join("snip_20260929_099999_1x1.png")).unwrap();
        touch(&dir, "no_extension", 4_000, "x");

        assert_eq!(
            latest_png(&dir).as_deref(),
            Some(newest.as_path()),
            "该拿那张最新的 PNG，目录名与 txt 都不许抢走"
        );
    }

    #[test]
    fn latest_png_tie_breaks_on_name() {
        let _lock = crate::app_dir_lock();
        let app = focusflow_core::paths::test_app_dir("reveal_tie");
        let dir = app.path().join("shots");
        std::fs::create_dir_all(&dir).unwrap();

        // 同一秒连按两次：后一张靠 unique_name 的 _2 后缀区分，时间戳完全相同
        touch(&dir, "snip_20260929_031000_800x600.png", 5_000, "a");
        let second = touch(&dir, "snip_20260929_031000_800x600_2.png", 5_000, "b");

        assert_eq!(
            latest_png(&dir).as_deref(),
            Some(second.as_path()),
            "平票要取名字大的那张（=同秒里后写的那张）"
        );
    }

    #[test]
    fn latest_png_on_missing_or_empty_dir_is_none() {
        let _lock = crate::app_dir_lock();
        let app = focusflow_core::paths::test_app_dir("reveal_empty");
        assert_eq!(
            latest_png(&app.path().join("nope")),
            None,
            "目录不存在不该 panic"
        );
        std::fs::create_dir_all(app.path().join("nope")).unwrap();
        assert_eq!(
            latest_png(&app.path().join("nope")),
            None,
            "空目录应回 None"
        );
    }

    #[test]
    fn explorer_args_use_backslashes_and_never_quote_the_path_only() {
        let dir = Path::new(r"E:\data\screenshots");
        let file = Path::new(r"E:/data/screenshots/snip_a.png");
        assert_eq!(
            explorer_args(dir, Some(file)),
            vec![r"/select,E:\data\screenshots\snip_a.png".to_string()],
            "正斜杠必须换成反斜杠：给 Explorer 正斜杠它会静默打开「文档」"
        );
        // 带空格的路径：我们只交出一个 token，引号由 std 在转义时包在整串外面
        let spaced = Path::new(r"E:\my data\screenshots\snip a.png");
        let args = explorer_args(dir, Some(spaced));
        assert_eq!(args.len(), 1);
        assert!(args[0].starts_with("/select,"), "参数形状不该变");
        assert!(
            !args[0].contains('"'),
            "自己不许塞引号（塞在逗号后面是错的写法）"
        );
    }

    #[test]
    fn explorer_args_without_target_just_opens_the_dir() {
        let dir = Path::new(r"E:\data\screenshots");
        assert_eq!(
            explorer_args(dir, None),
            vec![r"E:\data\screenshots".to_string()]
        );
    }
}
