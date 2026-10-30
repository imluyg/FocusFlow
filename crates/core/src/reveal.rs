//! 截图的"找到它"入口：托盘一项直达最近那张，省掉"专门去某个文件夹里挑"。
//!
//! 这一份原本长在 Tauri 前端的 `desktop/src/reveal.rs`，换原生时随前端一起被删了，
//! 而"截图之后我还要专门去某个文件夹去找"是他当场提过的诉求 ⇒ 搬回 core：
//! 裁决（哪张最新、参数怎么拼）与副作用（起 Explorer）分开，前者全是纯函数能被用例钉住。
//!
//! 没有前端参与：只暴露一个函数给调用方（原生托盘/设置页），不再新开任何 IPC 面。

use std::path::{Path, PathBuf};

/// 目录里最近的一张 PNG：按 mtime 取最新，平票按文件名字典序取大的那个
/// （截图名是 `snip_<时间戳>_<宽>x<高>`，同一秒里后写的那个名字更大）。
///
/// 只认 `.png`（大小写都收）且必须是普通文件：子目录、`.tmp` 残骸都不算，
/// 否则托盘那点一下会"选中一个打不开的目录"。
///
/// 两种"没拿到图"要分开交出去：`Ok(None)` ＝ 目录读得出来、里面确实没有 PNG；
/// `Err` ＝ 这个位置此刻读不出来（目录还不存在、被同步盘换成普通文件、移动盘休眠）。
/// 把后者折成 `None`，调用方那句就成了「截图目录是空的，已打开 …」——
/// 同一族（备份目录、`vacuum_all`）本仓已经修过两轮。
pub fn scan_png(dir: &Path) -> Result<Option<PathBuf>, String> {
    let mut best: Option<(std::time::SystemTime, String, PathBuf)> = None;
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
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
    Ok(best.map(|(_, _, p)| p))
}

/// 组装 `explorer.exe` 的参数：有文件就"打开所在目录并选中它"，没有就只开目录。
///
/// 交回来的**就是 Explorer 将要看到的整串**，所以调用方必须 `raw_arg`，不许 `arg`
/// （见 `reveal_last_screenshot`）—— std 会把里面的引号再转义一遍，那就回到下面这条事故。
/// 原样传参不带来注入口子：Windows 的文件名里根本不允许出现 `"`，
/// 所以这一串里除了我们自己加的那对引号不会有别的引号。
///
/// 两个会把失败**藏起来**的写法在这里一次做对：
/// - 必须是反斜杠绝对路径。`E:/a/b.png` 这种正斜杠形式 Explorer 不认，
///   它会静默退化成"打开文档文件夹"，既不报错也不弹提示。
/// - 带空格时引号只许包住**路径本身**、留在 `/select,` 后面。本机 Win11 实测三种写法：
///   `"/select,C:\ff reveal probe\...\snip_probe.png"`（整串被引起来，= `Command::arg` 的产物）
///   ⇒ 开出来的是「文档」；`/select,"C:\ff reveal probe\...\snip_probe.png"` ⇒ 开到位；
///   同一路径换成不带空格的目录则两种写法都对 —— 所以这一条只在带空格的路径上露出来，
///   而 `data_home` 是允许手写的。
pub fn explorer_args(dir: &Path, target: Option<&Path>) -> Vec<String> {
    // 两条分支都要过这把尺子。目录那一支不是次要路径：截图目录为空时走的就是它，
    // 而 `data_home` 是允许手写的（README 里就写着可以把 `[paths] data_home` 改成
    // `E:/…`）—— 只修文件那一支的话，"空目录 + 正斜杠配置"仍然是打开「文档」。
    let shown = |p: &Path| p.display().to_string().replace('/', "\\");
    match target {
        Some(file) => vec![format!("/select,\"{}\"", shown(file))],
        None => vec![format!("\"{}\"", shown(dir))],
    }
}

/// 把拼好的那几串**原样**交给 Explorer。
fn spawn_explorer(args: &[String]) -> Result<(), String> {
    let mut cmd = std::process::Command::new("explorer");
    for a in args {
        // 原样交出去：`arg()` 会再把整串里的引号转义成 `\"`，而 Explorer 自己解析命令行、
        // 不认那种写法 —— 那正是"路径带空格就打开文档"的那条事故（见 `explorer_args` 的实测）。
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.raw_arg(a);
        }
        #[cfg(not(windows))]
        cmd.arg(a);
    }
    // Explorer 成功了也常返回非零（甚至 1），所以只看"能不能起进程"，
    // 不等它、也不拿退出码当判据 —— 拿退出码判会把每次成功都记成失败。
    let _child = cmd
        .spawn()
        .map_err(|e| format!("启动 explorer 失败：{e}（目标 {}）", args.join(" ")))?;
    Ok(())
}

/// 在资源管理器里打开一个目录 —— 设置页「打开数据目录」与"最近截图"没有文件那一支共用。
///
/// 斜杠必须换：`data_home` 是允许手写成正斜杠形式的，而本机实测
/// `explorer C:/…/ff-slash-probe`（以及给它整串加引号的那一版）开出来的是「文档」，
/// 只有反斜杠那一版开到了目标目录。不报错、不提示，屏上只多一个**看错地方**的窗口。
pub fn open_dir(dir: &Path) -> Result<(), String> {
    spawn_explorer(&explorer_args(dir, None))
}

/// 直达最近一张截图。**返回值是给人看的落点描述**，调用方必须写进日志：
/// 托盘点下去"没反应"是本功能唯一故障形态，不吭声就等于把失败吞掉。
pub fn reveal_last_screenshot() -> Result<String, String> {
    let dir = crate::paths::screenshots_dir();
    // 读不出来时不往下开资源管理器：那一支会指着同一个目录说「截图目录是空的，已打开 …」，
    // 而这个目录既没被读过、也可能根本开不出来（休眠的移动盘、同名普通文件）。
    let target = scan_png(&dir)
        .map_err(|why| format!("截图目录读不出来（{why}），本次没有打开资源管理器"))?;
    let args = explorer_args(&dir, target.as_deref());
    spawn_explorer(&args)?;
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
    fn scan_png_picks_newest_and_ignores_junk() {
        let _lock = crate::paths::test_app_dir_lock();
        let app = crate::paths::test_app_dir("reveal_latest");
        let dir = app.path().join("shots");
        std::fs::create_dir_all(&dir).unwrap();

        touch(&dir, "snip_20260929_030000_800x600.png", 1_000, "old");
        let newest = touch(&dir, "snip_20260929_031000_800x600.png", 2_000, "new");
        // 干扰项：更晚时间但非 PNG、同名目录、以及一个没有后缀的文件
        touch(&dir, "notes.txt", 3_000, "not an image");
        std::fs::create_dir_all(dir.join("snip_20260929_099999_1x1.png")).unwrap();
        touch(&dir, "no_extension", 4_000, "x");

        assert_eq!(
            scan_png(&dir).unwrap().as_deref(),
            Some(newest.as_path()),
            "该拿那张最新的 PNG，目录名与 txt 都不许抢走"
        );
    }

    #[test]
    fn latest_png_tie_breaks_on_name() {
        let _lock = crate::paths::test_app_dir_lock();
        let app = crate::paths::test_app_dir("reveal_tie");
        let dir = app.path().join("shots");
        std::fs::create_dir_all(&dir).unwrap();

        // 同一秒连按两次：后一张靠 unique_name 的 _2 后缀区分，时间戳完全相同
        touch(&dir, "snip_20260929_031000_800x600.png", 5_000, "a");
        let second = touch(&dir, "snip_20260929_031000_800x600_2.png", 5_000, "b");

        assert_eq!(
            scan_png(&dir).unwrap().as_deref(),
            Some(second.as_path()),
            "平票要取名字大的那张（=同秒里后写的那张）"
        );
    }

    /// `Ok(None)`＝读得出来、里面没有 PNG；`Err`＝这个位置此刻读不出来。
    #[test]
    fn scan_png_tells_empty_apart_from_unreadable() {
        let _lock = crate::paths::test_app_dir_lock();
        let app = crate::paths::test_app_dir("reveal_empty");

        let missing = app.path().join("nope");
        let why = scan_png(&missing).expect_err("目录不存在不是「空的」，要说读不出来");
        assert!(
            why.contains(&missing.display().to_string()),
            "原因里要带上是哪个目录: {why}"
        );

        std::fs::create_dir_all(&missing).unwrap();
        assert_eq!(
            scan_png(&missing).unwrap(),
            None,
            "读得出的空目录才是「里面没有 PNG」"
        );

        // 同名普通文件（同步盘占位的形状）：也走 Err，不能被当成"没有截图"
        let blocker = app.path().join("as_file");
        std::fs::write(&blocker, b"not a directory").unwrap();
        assert!(
            scan_png(&blocker).is_err(),
            "位置是个普通文件 ⇒ 读不出来，不是空目录"
        );
    }

    /// 带空格的路径：引号必须包住**路径本身**、留在 `/select,` 后面。
    ///
    /// 本机 Win11 实测（三种写法各开一次资源管理器，读新窗口的标题）：整串被引起来那种
    /// ——也就是 `Command::arg` 遇到空格时的产物——开出来的是「文档」；引号在逗号后面那种
    /// 才开到 `screenshots`。不带空格的目录两种都对，所以这条只在路径带空格时露出来。
    #[test]
    fn the_select_argument_quotes_the_path_after_the_comma() {
        let spaced = Path::new(r"E:\my data\screenshots\snip a.png");
        assert_eq!(
            explorer_args(Path::new(r"E:\data\screenshots"), Some(spaced)),
            vec![r#"/select,"E:\my data\screenshots\snip a.png""#.to_string()],
            "形状变了就等于回到\"打开文档\"那条事故"
        );
        // 正斜杠那一路也要过同一把尺子（`data_home` 允许手写成正斜杠）
        assert_eq!(
            explorer_args(
                Path::new(r"E:\data\screenshots"),
                Some(Path::new("E:/my data/screenshots/snip a.png"))
            ),
            vec![r#"/select,"E:\my data\screenshots\snip a.png""#.to_string()]
        );
    }

    /// 接线腿：拼好的整串必须**原样**交给 Explorer。
    /// 用回 `arg()` 的话 std 会把里面的引号转义成 `\"`，上面那条实测过的正确形状当场失效，
    /// 而单元用例盯不到（它只看得见字符串，看不见最终那条命令行）。
    #[test]
    fn the_command_line_is_passed_verbatim_to_explorer() {
        let src = include_str!("reveal.rs").replace("\r\n", "\n");
        let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
        let prod = &src[..cut];
        let body = &prod[prod
            .find("fn spawn_explorer(")
            .expect("spawn_explorer 不见了：两条路又各自起一次 Explorer 了")..];
        assert!(
            body.contains("cmd.raw_arg(a)"),
            "整串不再原样交出去了：std 会把里面的引号二次转义"
        );
        // 起 Explorer 的那一处只许有一个：两条路（选中文件 / 只开目录）共用同一把斜杠尺子。
        assert_eq!(
            prod.matches("Command::new(\"explorer\")").count(),
            1,
            "又出现第二处自己起 Explorer 的地方了"
        );
    }

    #[test]
    fn explorer_args_without_target_just_opens_the_dir() {
        let dir = Path::new(r"E:\data\screenshots");
        assert_eq!(
            explorer_args(dir, None),
            vec![r#""E:\data\screenshots""#.to_string()]
        );
        // 目录那一支也要换斜杠：截图目录为空时走的就是这一支，而 `data_home` 允许手写
        // 成正斜杠形式（README 里就这么写着）。
        let slashed = Path::new("E:/data/screenshots");
        assert_eq!(
            explorer_args(slashed, None),
            vec![r#""E:\data\screenshots""#.to_string()],
            "正斜杠的目录也要换成反斜杠"
        );
        // 混合形式也不能漏（`E:/a\b` 这种真会出现：一半来自配置、一半来自 join）
        assert_eq!(
            explorer_args(Path::new("E:/data\\shots"), None),
            vec![r#""E:\data\shots""#.to_string()]
        );
    }
}
