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
//!
//! 程序目录（`app_dir`）与数据目录（`data_home`）是两件事，见各自函数注释：
//! 前者放程序本体（exe、config.ini、window_state.ini、plugins/、logs/），后者放
//! 用户数据（`data/`、`backup/`）。不设 `[paths] data_home` 时两者同一个目录，
//! 上面那句"拷走整个文件夹即迁移"依然成立。

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

/// 进程级数据目录解析结果缓存（详见 [`data_home`]：一次运行只认一个数据目录）。
static DATA_HOME: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

fn app_dir_override() -> &'static Mutex<Option<PathBuf>> {
    APP_DIR_OVERRIDE.get_or_init(|| Mutex::new(None))
}

fn data_home_cache() -> &'static Mutex<Option<PathBuf>> {
    DATA_HOME.get_or_init(|| Mutex::new(None))
}

/// 显式设置程序目录（测试隔离用；也可在打包版指向 exe 目录）。
pub fn set_app_dir(dir: impl Into<PathBuf>) {
    *app_dir_override().lock().unwrap_or_else(|e| e.into_inner()) = Some(dir.into());
    // 数据目录是从「当前 app_dir 下的 config.ini」抠出来的一个键，换了 app_dir 就等于
    // 换了一份配置，缓存必须一起作废：否则 `test_app_dir` 切目录之后，`data_dir()` 仍然
    // 指着上一个用例已经删掉的临时目录 —— 孤儿写入落进下一个用例刚建好的目录，正是本文件
    // 到处设哨兵、`TestAppDir::drop` 反复清理要防的那个形状。
    *data_home_cache().lock().unwrap_or_else(|e| e.into_inner()) = None;
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
        // 前 10 轮用普通路径，之后换成 `\\?\` 前缀再试 10 轮 —— 普通路径删不动
        // 超过 MAX_PATH 的树（%TEMP% 里那棵常驻的 `ff_gd_src_*` 就是：某版
        // `copy_data_tree` 还没闸自嵌套时建出来的 200 多层 `data/sub/data/sub/…`，
        // 5215 个条目，`rm -rf` 与 `remove_dir_all` 都吃不下，从此谁都删不掉它）。
        let mut last_err: Option<std::io::Error> = None;
        for attempt in 0..20 {
            let r = if attempt < 10 {
                std::fs::remove_dir_all(&self.dir)
            } else {
                remove_dir_all_long(&self.dir)
            };
            match r {
                Ok(()) => return,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
                Err(e) => {
                    last_err = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        }
        // 静默放弃过一次就被记成"账 33：那 1 项常驻"，查了两轮都没查出是谁留的。
        // 删不掉要说出来，哪怕说的只是"我自己也删不动"。
        if let Some(e) = last_err {
            tracing::warn!("测试临时目录没删掉，留在 {}: {e}", self.dir.display());
        }
    }
}

/// 与 `remove_dir_all` 同义，但先给路径挂上 `\\?\` 前缀 —— Windows 上绕开
/// MAX_PATH（260）唯一的办法。前缀路径要求绝对、反斜杠、不做规范化，
/// 所以这里先 `absolute` 再把 `/` 换掉。
#[cfg(windows)]
fn remove_dir_all_long(dir: &Path) -> std::io::Result<()> {
    let abs = std::path::absolute(dir)?;
    let s = abs.display().to_string().replace('/', "\\");
    let prefixed = if s.starts_with("\\\\?\\") {
        s
    } else {
        format!("\\\\?\\{s}")
    };
    std::fs::remove_dir_all(Path::new(&prefixed))
}

#[cfg(not(windows))]
fn remove_dir_all_long(dir: &Path) -> std::io::Result<()> {
    std::fs::remove_dir_all(dir)
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

/// 数据根目录：`data/` 与 `backup/` 挂在哪里，可由 `config.ini` 的
/// `[paths] data_home` 指到程序目录之外（不设 = 程序目录，与历史行为一致）。
///
/// 刻意**不读 `config::instance()`**，而是把 config.ini 当纯文本抠这一个键：
/// - 路径解析早于配置单例就绪（`config::load()` 自己结尾就要落盘，建库/备份都要用它），
///   拿单例取键会在其 `OnceLock` 的初始化闭包里再次进入它；
/// - 单例的 `path` 启动即钉死，本来就支撑不了"运行中换目录"的语义。
///
/// 也刻意**只在进程内解析一次**：换数据目录的流程是"拷贝 → 写配置 → 重启"，从写配置
/// 到进程真的退出之间，写线程与备份/采样这些常驻线程还在按老目录落库。若每次即时解析，
/// 那几秒里会有一半线程写旧目录、一半写新目录 —— 数据静默分裂比不切换更糟。
pub fn data_home() -> PathBuf {
    if let Some(dir) = data_home_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        return dir;
    }
    let dir = resolve_data_home();
    *data_home_cache().lock().unwrap_or_else(|e| e.into_inner()) = Some(dir.clone());
    dir
}

/// 把本进程的数据根**钉死**到指定目录（当前只有"迁移失败退回旧目录"这一条路用它）。
///
/// 与 [`set_app_dir`] 的区别：那个换的是程序目录（配置、日志、插件的所在），这个只换
/// 数据落在哪。刻意保留"一次运行只认一个数据目录"的不变量：这里直接写进缓存，
/// 而不是让后续解析再去看配置文件，否则常驻线程会在运行中改口、新旧目录一起被写。
pub fn force_data_home(dir: impl Into<PathBuf>) {
    *data_home_cache().lock().unwrap_or_else(|e| e.into_inner()) = Some(dir.into());
}

/// 解析一次 [`data_home`]：读程序目录下的 config.ini，取 `[paths] data_home`。
fn resolve_data_home() -> PathBuf {
    let app = app_dir();
    let configured = std::fs::read_to_string(app.join("config.ini"))
        .ok()
        .and_then(|text| {
            crate::config::parse_ini(&text)
                .get("paths")
                .and_then(|s| s.get("data_home"))
                .cloned()
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let Some(configured) = configured else {
        return app;
    };
    // 相对路径按程序目录展开：便携包里写 `data_home = D:\FocusData` 是绝对路径，
    // 而写 `..\Data` 这种相对形式的人也该有个明确的基准，不能取决于起始位置
    // （app_dir 的注释里就是为了防这个才在 release 下取 exe 目录的）。
    let p = Path::new(&configured);
    let candidate = if p.is_absolute() {
        p.to_path_buf()
    } else {
        app.join(p)
    };
    match std::fs::create_dir_all(&candidate) {
        Ok(()) => candidate,
        Err(e) => {
            // 配置指向的目录用不了（移动盘没插、OneDrive 占位、权限不足、同名普通文件）
            // 时不硬失败，回落程序目录继续跑：按键统计必须有个能写的地方，而"双击没反应、
            // 也没有任何报错"是 logger.rs 里已经避过一次的坑。回落的事实记进日志。
            tracing::error!(
                "配置的数据目录不可用（{}）: {e}；本次回落到程序目录 {}",
                candidate.display(),
                app.display()
            );
            app
        }
    }
}

/// `data/` 数据目录，不存在则创建。
pub fn data_dir() -> PathBuf {
    let dir = data_home().join("data");
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// `logs/` 日志目录，不存在则创建。
///
/// 跟着**程序目录**走而不是数据目录：`init_logging()` 在任何命令行参数解析之前就要跑
/// （见 desktop/src/lib.rs），让它依赖配置文件等于把日志系统架在配置能否读上来的赌注上。
pub fn log_dir() -> PathBuf {
    let dir = app_dir().join("logs");
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// `data/screenshots/` 截图目录，不存在则创建。
///
/// 位置不是随便挑的：搬家逻辑（`data_location.rs`）在四处硬编码只走 `data` 与 `backup`
/// 两棵子树 —— 复制 `:98`、逐字节核对 `:502`、删源 `:572`，而 `:67` 会把目标目录根下
/// 任何其它条目当成"陌生人"拒绝切换。所以截图放在 `data_home()/data/` **里面**，
/// 用户改数据文件夹时才会被现有递归 walk 一起带走；放在 `data_home` 根下则要么被落下、
/// 要么直接把"更改数据文件夹"这个功能挡住。
pub fn screenshots_dir() -> PathBuf {
    let dir = data_home().join("data").join("screenshots");
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// `backup/` 备份目录，不存在则创建。
pub fn backup_dir() -> PathBuf {
    let dir = data_home().join("backup");
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 截图目录必须落在 `data_home()/data/screenshots`。
    ///
    /// 这不是审美问题：搬家逻辑（`data_location.rs`）有四处硬编码只走 `data` 与 `backup`
    /// 两棵子树（复制 `:98`、逐字节核对 `:502`、删源 `:572`），而目标目录根下出现任何
    /// 别的条目都会被 `validate_new_data_home:67` 当成"陌生人"拒绝切换。
    /// 放到 `data_home` 根下的后果是二选一：截图被落在旧目录，或"更改数据文件夹"不能用了。
    #[test]
    fn screenshots_dir_is_inside_data_so_migration_carries_it() {
        let _lock = test_app_dir_lock();
        let app = test_app_dir("shot_dir");
        set_app_dir(app.path());
        let dir = screenshots_dir();
        assert_eq!(dir, app.path().join("data").join("screenshots"));
        assert!(
            dir.starts_with(data_home().join("data")),
            "screenshots 不在 data/ 下面：{dir:?}"
        );
        assert!(dir.is_dir(), "取用时该已经把目录建好");
    }
}
