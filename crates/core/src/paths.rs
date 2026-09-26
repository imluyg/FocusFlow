//! 应用路径管理。
//!
//! 镜像 Python 版 `config.py` 的目录约定：
//! - 程序目录 = exe 所在目录（或开发模式下工作区目录）
//! - `data/` 数据目录（年度数据库）
//! - `logs/` 日志目录
//! - `backup/` 备份目录
//! - `plugins/` 插件目录
//!
//! 运行时数据与 exe 同级存放，保证"拷贝整个文件夹即可迁移数据"的既有产品形态。

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::Datelike;

/// 应用名称（目录/文件命名用，与 Python 版一致）
pub const APP_NAME: &str = "FocusFlow";
/// 应用显示名称
pub const APP_DISPLAY_NAME: &str = "FocusFlow - 效率追踪器";
/// 应用描述
pub const APP_DESCRIPTION: &str = "FocusFlow - 效率与专注力分析工具";
/// 版本号：从 Cargo 包版本自动生成（focusflow-core 使用 workspace 版本）。
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 进程级 app_dir 覆盖（测试/部署指定数据目录用）。
///
/// 优先级：`set_app_dir` 显式设置 > 环境变量 `FOCUSFLOW_APP_DIR` > 当前工作目录。
static APP_DIR_OVERRIDE: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

fn app_dir_override() -> &'static Mutex<Option<PathBuf>> {
    APP_DIR_OVERRIDE.get_or_init(|| Mutex::new(None))
}

/// 显式设置程序目录（测试隔离用；也可在打包版指向 exe 目录）。
pub fn set_app_dir(dir: impl Into<PathBuf>) {
    *app_dir_override().lock().unwrap_or_else(|e| e.into_inner()) = Some(dir.into());
}

/// 测试专用：切换全局 app_dir 的测试必须持有此锁跑完全程。
/// 并行测试共享进程级全局路径，不加锁会互相改写（DB/恢复文件写错位置、
/// 启动回放误删其他测试刚写入的恢复文件）。
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub fn test_app_dir_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// 测试专用的临时程序目录：**Drop 时删除目录**。
///
/// 为什么要有它：各测试原来自己 `temp_dir().join(format!("ff_x_{}_{}", pid, 纳秒))`
/// 并在函数末尾手写一行 `remove_dir_all` —— 名字每轮都新，收尾又只在断言全过时
/// 才执行，于是一次失败（或干脆忘了写）就在 %TEMP% 里永久留下一份 SQLite 库。
/// 实测本机 %TEMP% 已堆到 2666 个目录 / 1.4GB，其中 `ff_acc_test_*` 一个前缀 333 个。
///
/// 两点刻意保留：
/// - 名字继续带 pid+纳秒：同一进程内多个用例必须各用一套目录，否则互相看数据；
/// - **不加串行锁**：`app_dir` 的串行由各用例自己的 `test_app_dir_lock()` 负责，
///   这里再拿一次就是重入 std::Mutex（不可重入）→ 直接死锁。
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub struct TestAppDir {
    dir: PathBuf,
}

#[cfg(any(test, feature = "test-utils"))]
impl TestAppDir {
    pub fn path(&self) -> &Path {
        &self.dir
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl Drop for TestAppDir {
    fn drop(&mut self) {
        // 先把全局 app_dir 撤到哨兵，再放只读连接、删目录。
        // ① 用例留下的后台线程（写线程的收尾竞态、Database::init 带起的
        //    采样/备份这类不死线程、将来任何惰性解析 app_dir 的代码）在此刻
        //    之后解析到的是 target/ 的哨兵，而不是把刚删掉的目录原样建回来 ——
        //    实测泄漏目录里只有一份 config.ini 或一份年度库，正是这个形状。
        // ② Windows 上句柄没放完时 remove_dir_all 会静默失败：这些测试正是
        //    靠 with_ro_conn 的线程本地缓存反复读年度库的 —— 不先清缓存，
        //    删除就返回 Err，每个用例每跑一次留一个目录（历史上所有前缀一律
        //    +1/run，所以哨兵之后还要 clear_ro_cache）。
        set_app_dir(test_scratch_app_dir());
        crate::db::connection::clear_ro_cache();
        // 句柄释放有竞态（写线程先清 alive 标志、连接在其后析构）：remove
        // 撞上未释放的句柄会失败，重试几轮等它放干净；目录已经不在了就当成功。
        for _ in 0..20 {
            match std::fs::remove_dir_all(&self.dir) {
                Ok(()) => return,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
            }
        }
    }
}

/// 建一个隔离的临时程序目录并切过去；返回值守着期间目录可用，离开作用域自动删除
/// （含 panic / 断言失败路径）。
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub fn test_app_dir(tag: &str) -> TestAppDir {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("ff_{tag}_{}_{nanos}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("data")).expect("创建临时程序目录失败");
    set_app_dir(&dir);
    TestAppDir { dir }
}

/// 用例收尾时把全局 `app_dir` 指到这里：**哨兵，不是数据目录**。
///
/// `set_app_dir` 是进程级全局，而 `stop()` 放弃等待之后仍可能有线程惰性去解析它；
/// 把它指向一个与任何用例都无关的路径，孤儿写入就不会落进下一个用例刚建好的目录。
///
/// 原先直接用 `%TEMP%/ff_restore_nonexistent`：名字虽固定，但 `accounting`/`pomodoro`/
/// `scheduler` 打开附属库时会 `create_dir_all(data_dir())`，于是这个"本不存在"的哨兵
/// 每轮真被建出来（实测 +1/轮），还成了一个谁也不删、又持续吸收孤儿写入的常驻目录。
/// 放 `target/` 下就对了：跨运行复用同一个、一次 `cargo clean` 带走，`%TEMP%` 不再增长。
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub fn test_scratch_app_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/ff_restore_sentinel")
}

/// 程序目录。
///
/// 优先级：`set_app_dir` 显式设置 > 环境变量 `FOCUSFLOW_APP_DIR` > exe 所在目录（release）> 当前工作目录。
///
/// release 下默认取 exe 所在目录：数据/配置固定跟程序走（README 承诺"数据存放在程序目录"），
/// 且不受快捷方式"起始位置"错误导致的静默数据写错目录影响。
/// debug 下保留当前工作目录（cargo run 的 cwd 即工程目录，便于开发调试）。
pub fn app_dir() -> PathBuf {
    if let Some(dir) = app_dir_override()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        return dir;
    }
    if let Ok(dir) = std::env::var("FOCUSFLOW_APP_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    #[cfg(not(debug_assertions))]
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            return dir.to_path_buf();
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// `data/` 数据目录，不存在则创建。
pub fn data_dir() -> PathBuf {
    let dir = app_dir().join("data");
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// `logs/` 日志目录，不存在则创建。
pub fn log_dir() -> PathBuf {
    let dir = app_dir().join("logs");
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// `backup/` 备份目录，不存在则创建。
pub fn backup_dir() -> PathBuf {
    let dir = app_dir().join("backup");
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// `plugins/` 插件目录，不存在则创建。
pub fn plugins_dir() -> PathBuf {
    let dir = app_dir().join("plugins");
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// `config.ini` 配置文件路径。
pub fn config_path() -> PathBuf {
    app_dir().join("config.ini")
}

/// `window_state.ini` 窗口状态文件路径（易变状态独立存放，与 Python 版一致）。
pub fn window_state_path() -> PathBuf {
    app_dir().join("window_state.ini")
}

/// 指定年份的数据库文件路径：`data/focusflow_YYYY.db`。
pub fn year_db_path(year: i32) -> PathBuf {
    data_dir().join(format!("focusflow_{year}.db"))
}

/// 当前年份的数据库文件路径。
pub fn current_year_db_path() -> PathBuf {
    year_db_path(current_year())
}

/// 当前年份（本地时区）。
pub fn current_year() -> i32 {
    chrono::Local::now().year()
}

/// 判断路径是否为年度数据库文件（`focusflow_<4位年份>.db`）。
pub fn is_year_db_file(path: &Path) -> Option<i32> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_prefix("focusflow_")?.strip_suffix(".db")?;
    if stem.len() == 4 && stem.chars().all(|c| c.is_ascii_digit()) {
        stem.parse().ok()
    } else {
        None
    }
}
