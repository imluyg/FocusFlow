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

/// 本次进程里「配置的数据目录用不了、已经回落到程序目录」这句话（`None` = 没回落）。
///
/// 它跟着 [`DATA_HOME`] 一起作废：那件事实就是那一次解析留下的，换了 app_dir 或改口
/// 钉死数据目录之后，旧的那句就不再描述本进程正在用的目录了。
static DATA_HOME_FALLBACK: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn app_dir_override() -> &'static Mutex<Option<PathBuf>> {
    APP_DIR_OVERRIDE.get_or_init(|| Mutex::new(None))
}

fn data_home_cache() -> &'static Mutex<Option<PathBuf>> {
    DATA_HOME.get_or_init(|| Mutex::new(None))
}

fn data_home_fallback_slot() -> &'static Mutex<Option<String>> {
    DATA_HOME_FALLBACK.get_or_init(|| Mutex::new(None))
}

/// 显式设置程序目录（测试隔离用；也可在打包版指向 exe 目录）。
pub fn set_app_dir(dir: impl Into<PathBuf>) {
    *app_dir_override().lock().unwrap_or_else(|e| e.into_inner()) = Some(dir.into());
    // 数据目录是从「当前 app_dir 下的 config.ini」抠出来的一个键，换了 app_dir 就等于
    // 换了一份配置，缓存必须一起作废：否则 `test_app_dir` 切目录之后，`data_dir()` 仍然
    // 指着上一个用例已经删掉的临时目录 —— 孤儿写入落进下一个用例刚建好的目录，正是本文件
    // 到处设哨兵、`TestAppDir::drop` 反复清理要防的那个形状。
    *data_home_cache().lock().unwrap_or_else(|e| e.into_inner()) = None;
    // 那句"回落到程序目录"是上一次解析留下的，换了 app_dir 就不该再被界面念出来。
    *data_home_fallback_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
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
///
/// cfg 必须盖住 `any(test, feature = "test-utils")`：唯一的调用方 `TestAppDir::drop`
/// 就带这个 cfg，只写 `windows` 会让普通 lib 构建报 `dead_code`。
#[cfg(all(windows, any(test, feature = "test-utils")))]
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

#[cfg(all(not(windows), any(test, feature = "test-utils")))]
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

/// 把 `path` 那一格做成**自指目录 junction**：`std::fs::metadata` 与 `File::open` 在它上面
/// 都报 os error 1921（`FilesystemLoop`，本机现量）—— 既不是"在"也不是 `NotFound`，
/// 于是"这一次问不出"那一臂第一次有了真实系统调用下的形状（符号链接那条路要特权，
/// 本机实测 os error 1314 拒建，所以走 `mklink /J`，它不要管理员）。
///
/// 返回 `false` = 这台机器建不出这个夹具。调用方必须**出声跳过**，不许静默当绿：
/// 那样那些臂会重新变回没人守。收尾用 `std::fs::remove_dir` 摘掉链接本身
/// （`RemoveDirectoryW` 删的是重解析点，不跟着删进目标；`TestAppDir::drop` 也认得这个形状）。
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub fn test_self_loop_junction(path: &Path) -> bool {
    #[cfg(windows)]
    {
        // `mklink` 是 cmd 内置，只吃反斜杠路径
        let link = path.to_string_lossy().replace('/', "\\");
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J", &link, &link])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
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
/// debug（非测试）下保留当前工作目录（cargo run 的 cwd 即工程目录，便于开发调试）。
/// **测试构建例外**：忘了锚定时不许写到仓库里去，见下面那段注释与 `test_scratch_app_dir`。
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
    // 测试构建里**不许**回落到当前工作目录：cargo 的 cwd 就是包目录（`crates/core`），
    // 于是一条忘了锚定的用例、或常驻线程的一次惰性解析，会把库与配置静默写进仓库 ——
    // 实测留下过 `crates/core/data/focusflow_2026.db`（.gitignore 挡住看不见），
    // 而它的下一次命中表现成隔壁用例 `UNIQUE constraint failed: daily_counts.date_key`：
    // 随机红，且 `--test-threads=1` 永远全绿。指到 `target/` 下那个哨兵：
    // 一次 `cargo clean` 带走，也不会谎称程序目录在仓库里。
    #[cfg(any(test, feature = "test-utils"))]
    return test_scratch_app_dir();
    #[cfg(not(any(test, feature = "test-utils")))]
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
    // 钉死之后生效的就是这一枚目录，"回落到程序目录"那句不再描述本进程。
    *data_home_fallback_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
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
            let note = format!(
                "配置的数据目录不可用（{}）: {e}；本次回落到程序目录 {}",
                candidate.display(),
                app.display()
            );
            tracing::error!("{note}");
            // 光有日志那一行，用户看到的是"我的历史没了"而不知道库换到了哪。
            // 这句同时交给启动时那条状态行（`desktop-native` 的 boot 状态）显示。
            *data_home_fallback_slot()
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(note);
            app
        }
    }
}

/// 本次进程有没有发生「配置的数据目录不可用、回落到程序目录」；有就说清换到了哪、为什么。
///
/// 与 [`data_home`] 同一个口径：一次解析、一次运行只认一个数据目录，所以这条事实也是
/// 进程级的，并且跟着那枚缓存一起作废（`None` = 当前生效的目录不是回落来的）。
pub fn data_home_fallback_notice() -> Option<String> {
    data_home_fallback_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// 建目录；失败时把路径与原因说出来。
///
/// 这几个构造函数把路径交出去之后，调用方（设置页、写库、截图、备份）会把它当成
/// "位置就在这里"。原来一律 `create_dir_all(...).ok()`：目录建不出来时界面照旧
/// 显示那个位置，日志里也没有任何线索，直到写入失败才在别处冒出来。
/// 返回原路径，行为不变。
fn ensure_dir(dir: PathBuf) -> PathBuf {
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(
            "目录建不出来（{}）：{e} —— 设置页仍会显示这个位置，写入这里的文件会失败",
            dir.display()
        );
    }
    dir
}

/// `data/` 数据目录，不存在则创建。
pub fn data_dir() -> PathBuf {
    ensure_dir(data_home().join("data"))
}

/// `logs/` 日志目录，不存在则创建。
///
/// 跟着**程序目录**走而不是数据目录：`init_logging()` 在配置能否读上来之前就得装好
/// （今天两个入口都这样：CLI 一进来先调，原生端在 `desktop-native/src/collect.rs`
/// 起采集时调），让它依赖配置文件等于把日志系统架在配置能否读上来的赌注上。
pub fn log_dir() -> PathBuf {
    ensure_dir(app_dir().join("logs"))
}

/// `data/screenshots/` 截图目录，不存在则创建。
///
/// 位置不是随便挑的：搬家逻辑（`data_location.rs`）在四处硬编码只走 `data` 与 `backup`
/// 两棵子树 —— 复制 `:98`、逐字节核对 `:502`、删源 `:572`，而 `:67` 会把目标目录根下
/// 任何其它条目当成"陌生人"拒绝切换。所以截图放在 `data_home()/data/` **里面**，
/// 用户改数据文件夹时才会被现有递归 walk 一起带走；放在 `data_home` 根下则要么被落下、
/// 要么直接把"更改数据文件夹"这个功能挡住。
pub fn screenshots_dir() -> PathBuf {
    ensure_dir(data_home().join("data").join("screenshots"))
}

/// `backup/` 备份目录，不存在则创建。
pub fn backup_dir() -> PathBuf {
    ensure_dir(data_home().join("backup"))
}

/// `plugins/` 程序目录那一侧的根。**取插件清单别用它**，用 [`plugins_builtin_dir`]
/// 与 [`plugins_user_dir`] 这两处。
///
/// 它还活着的理由只有一个：旧版本把内置与用户插件都平铺在这里，升级时这一批是
/// 迁移的**源**（`manager.rs` 的 `migrate_legacy_plugins`），并且
/// `[plugins] legacy_layout = true` 时它仍然是唯一被扫的那一处（回滚）。
///
/// 刻意**不建目录**：这两条路都不需要它存在（读不到就是"旧目录里没有插件"），而安装版
/// 落在 Program Files 时这里写不进去 —— 这些函数会被每秒的插件清单扫描调到，
/// 建不出来的那句 warn 会变成刷不完的日志。旧版那种"扫之前先建"留在
/// `manager.rs` 的回滚分支里，只有真要用它的人才建它。
pub fn plugins_dir() -> PathBuf {
    app_dir().join("plugins")
}

/// `plugins/builtin/`：随包发布的内置模块。程序的一部分，重装即可恢复，因此**只读、不进备份**。
///
/// 与 [`plugins_user_dir`] 分开是整套目录分离的地基：安装版落在 Program Files 时这一处
/// 用户写不进去（也不该写），而备份扫程序目录会把随包脚本当成用户数据一起轮转。
/// 插件的身份（内置还是用户装的）由**它出自哪一处目录**决定，不由脚本自己写的常量决定。
pub fn plugins_builtin_dir() -> PathBuf {
    app_dir().join("plugins").join("builtin")
}

/// 数据目录那一侧的插件根 `data/plugins/`：用户自己装的脚本插件，可写、跟着数据目录迁移与备份走。
///
/// 放在 `data_home()/data/` **里面**而不是 `data_home` 根下，跟 [`screenshots_dir`] 同一个理由：
/// 搬家逻辑（`data_location.rs`）只走 `data` 与 `backup` 两棵子树，而目标目录根下出现别的
/// 条目会被当成"陌生人"拒绝切换。
///
/// 它的子目录 `state/` 是插件运行时数据（[`plugin_state_dir`]），与插件包分居两处，
/// 所以算内容哈希时天然扫不到运行时文件。
pub fn plugins_user_dir() -> PathBuf {
    ensure_dir(data_dir().join("plugins"))
}

/// `stem` 能不能当一个状态目录名 —— 这条规则**只有这一份**。
///
/// 插件状态这一族有两个读者：[`plugin_state_dir`]（要不要落盘）与
/// `plugins::plugin_store`（要不要接受这次读写）。两处各写一遍的话，
///  store 那关放宽一寸，`../grants` 就又回到"能碰别人的授权文件"。
pub fn is_safe_state_stem(stem: &str) -> bool {
    !stem.is_empty() && !stem.contains('/') && !stem.contains('\\') && stem != ".."
}

/// 某个插件状态目录的**位置**，不建目录 —— 读一侧用它。
///
/// 把一次只读查询变成一次 `mkdir` 是副作用：开着插件页就会给每个插件（包括从来不存东西的）
/// 在数据目录里造出一格空目录。真要落盘的那一侧用 [`plugin_state_dir`]，它才建。
/// 路径公式只有这一处，越界与否由 [`is_safe_state_stem`] 判。
pub fn plugin_state_path(stem: &str) -> PathBuf {
    plugins_state_dir().join(stem)
}

/// 某个插件的私有运行时数据目录 `data/plugins/state/<stem>/`。
///
/// `stem` 必须是**插件文件名**（来自目录扫描，不是插件自己起的展示名，也不是 store 的键）：
/// 停用列表、授权、状态文件全按文件名走，插件改名不影响这些数据。
///
/// 这里对 `stem` 做分隔符检查，是因为它是这条路径上唯一可能被外部字符串喂到的一格：
/// store 落地之后，插件给的键、config.ini 里被人手改过的 `disabled` 项都会往这儿流。
/// 一个 `../grants` 就能把"插件私有"变成"碰别人的授权文件"。越界的名字不落盘、只出声。
pub fn plugin_state_dir(stem: &str) -> PathBuf {
    let safe = is_safe_state_stem(stem);
    if !safe {
        tracing::error!(
            "插件状态目录的名字不合法（{stem}）：只认插件文件名，不许带路径分隔或 .. —— \
             本次不落盘，别去 {} 底下找它",
            plugins_state_dir().display()
        );
        return plugins_state_dir();
    }
    ensure_dir(plugin_state_path(stem))
}

/// `config.ini` 配置文件路径。
pub fn config_path() -> PathBuf {
    app_dir().join("config.ini")
}

/// `window_state.ini` 窗口状态文件路径（易变状态独立存放，与 Python 版一致）。
pub fn window_state_path() -> PathBuf {
    app_dir().join("window_state.ini")
}

/// 这一轮**真正该被扫的**插件目录，顺序即身份顺序（内置在前，同名的以随包那份为准）。
///
/// 只有这一处说清"扫哪几处"：管理器取清单用它，插件页那一行「目录」也用它。
/// 抄两份的后果是"清单来自 A、屏上写着 B" —— 用户照屏上那个目录放插件，放下去不加载。
pub fn plugin_scan_dirs(legacy_layout: bool) -> Vec<PathBuf> {
    if legacy_layout {
        vec![plugins_dir()]
    } else {
        vec![plugins_builtin_dir(), plugins_user_dir()]
    }
}

/// 所有插件运行时数据的根 `data/plugins/state/`（**不建目录**）。
///
/// 备份与体检扫这一棵子树用它（方案 3.8：范围只在数据目录）。刻意不调 `ensure_dir`：
/// 一次备份扫描不该在数据目录里凭空造出一格 `state/`，而程序目录那一侧
/// （`app_dir/plugins/builtin/`）**不在这里** —— 那是程序的一部分，重装即可恢复，没有备份价值。
pub fn plugins_state_dir() -> PathBuf {
    data_dir().join("plugins").join("state")
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

    /// 目录建不出来时不能只 `.ok()`：设置页拿到的路径看起来是好的，写入却会失败。
    ///
    /// 夹具把 `data/` 换成同名普通文件（只读卷、同步盘占位、同名文件都是这个形状）。
    /// 成功路径也必须安静 —— 这些函数在热路径上被反复调用。
    #[test]
    fn a_failed_directory_creation_is_reported_with_its_path() {
        let _lock = test_app_dir_lock();
        let app = test_app_dir("dir_fail");
        set_app_dir(app.path());
        let blocked = data_dir();
        assert!(blocked.is_dir(), "正常时该建好");
        std::fs::remove_dir(&blocked).unwrap();
        std::fs::write(&blocked, b"not a directory").unwrap();

        let (got, logs) = crate::logger::capture_logs(data_dir);
        assert_eq!(got, blocked, "失败时仍把路径交回去，行为不变");
        let shown = logs.join("\n");
        assert!(shown.contains("目录建不出来"), "建不成必须出声: {shown}");
        assert!(
            shown.contains(&blocked.display().to_string()),
            "要点名哪个目录: {shown}"
        );
        assert!(shown.contains("写入"), "要说出后果: {shown}");

        std::fs::remove_file(&blocked).unwrap();
        let (made, logs) = crate::logger::capture_logs(data_dir);
        assert_eq!(made, blocked);
        assert!(made.is_dir());
        assert!(
            logs.iter().all(|l| !l.contains("目录建不出来")),
            "成功不该报警: {logs:?}"
        );
    }

    /// `[paths] data_home` 用不了 ⇒ 本次回落到程序目录继续记账。这件事原先只有
    /// `tracing::error!` 一行，而它的用户侧症状是"我的历史没了"：真库在原地不被使用，
    /// 安装目录里另起一套空库，读数全报 0 而退出码 0。这里钉的是**屏上拿得到那句**
    /// （`desktop-native` 的启动状态行读 [`data_home_fallback_notice`]）。
    #[test]
    fn the_data_home_fallback_leaves_a_notice_the_screen_can_read() {
        let _lock = test_app_dir_lock();
        let app = test_app_dir("home_fallback");
        set_app_dir(app.path());
        // 夹具取"被同名普通文件占住"这一形（移动盘没插、OneDrive 占位是同一支）
        let blocked = app.path().join("elsewhere_home");
        std::fs::write(&blocked, b"not a directory").unwrap();
        std::fs::write(
            app.path().join("config.ini"),
            format!("[paths]\ndata_home = {}\n", blocked.display()),
        )
        .unwrap();
        assert!(
            data_home_fallback_notice().is_none(),
            "还没解析过，不该挂着上一轮的句子"
        );

        let home = data_home();
        assert_eq!(home, app.path(), "该回落到程序目录");
        let note = data_home_fallback_notice().expect("回落这件事要留下一句能上屏的话");
        assert!(
            note.contains(&blocked.display().to_string()),
            "要点名是哪个目录用不了: {note}"
        );
        assert!(note.contains("回落到程序目录"), "要说清现在记在哪: {note}");

        // 那句描述的是"这一次解析"的结果：换了 app_dir 就等于换了一份配置，
        // 留着它会让下一个进程状态读到一句不属于它的话。
        set_app_dir(app.path().join("second_home"));
        assert!(
            data_home_fallback_notice().is_none(),
            "换目录之后旧的那句必须作废"
        );
    }

    /// 目录修好之后，下一次解析要真的用回配置里那一枚，而不是停在回落后的程序目录。
    /// 那句降级也跟着换目录一起作废（`set_app_dir` 那一步），所以屏上不会念一句过期的话。
    #[test]
    fn a_working_data_home_clears_the_fallback_notice() {
        let _lock = test_app_dir_lock();
        let app = test_app_dir("home_recovered");
        set_app_dir(app.path());
        let blocked = app.path().join("recoverable_home");
        std::fs::write(&blocked, b"not a directory").unwrap();
        let cfg = app.path().join("config.ini");
        std::fs::write(
            &cfg,
            format!("[paths]\ndata_home = {}\n", blocked.display()),
        )
        .unwrap();
        assert_eq!(data_home(), app.path());
        assert!(data_home_fallback_notice().is_some(), "先让它回落一次");

        std::fs::remove_file(&blocked).unwrap();
        set_app_dir(app.path());
        assert_eq!(data_home(), blocked, "现在该用配置里那一枚");
        assert!(
            data_home_fallback_notice().is_none(),
            "重新解析过一轮，那句回落该跟着作废"
        );
    }

    /// 迁移失败时 core 会把数据根**钉回**旧目录（[`force_data_home`]）。钉死之后生效的
    /// 就是那一枚，之前那句"回落到程序目录"不再描述本进程 —— 屏上念一句过期的降级，
    /// 比念不出降级更容易把人引到错的目录上去找库。
    #[test]
    fn pinning_the_data_home_retires_the_fallback_notice() {
        let _lock = test_app_dir_lock();
        let app = test_app_dir("home_pinned");
        set_app_dir(app.path());
        let blocked = app.path().join("pinnable_home");
        std::fs::write(&blocked, b"not a directory").unwrap();
        std::fs::write(
            app.path().join("config.ini"),
            format!("[paths]\ndata_home = {}\n", blocked.display()),
        )
        .unwrap();
        assert_eq!(data_home(), app.path(), "先让它回落到程序目录");
        assert!(
            data_home_fallback_notice().is_some(),
            "回落得先留下一句，这条才有东西可作废"
        );

        let pinned = app.path().join("old_home");
        force_data_home(&pinned);
        assert_eq!(data_home(), pinned, "钉死的那一枚要生效");
        assert!(
            data_home_fallback_notice().is_none(),
            "钉死之后不许再念那句回落"
        );
    }

    /// 两处插件目录的归属：内置跟程序目录，用户装的跟数据目录，运行时数据在数据目录的 `state/` 下。
    ///
    /// 这不是审美问题，是三条后果：
    /// · 用户插件落在程序目录 ⇒ 安装版写 Program Files 要管理员权限、升级会清掉它、备份扫不到；
    /// · 内置模块落在数据目录 ⇒ 重装程序不会恢复它，而它是程序的一部分；
    /// · 用户插件必须在 `data_home()/data/` **里面** ⇒ 搬家逻辑只走 `data` 与 `backup` 两棵子树，
    ///   放在 `data_home` 根下要么被落在旧目录，要么直接把「更改数据文件夹」挡死
    ///   （与 [`screenshots_dir`] 同一条理由，那条已有用例钉着）。
    /// 换数据目录时内置不跟着走：那一处仍是程序目录下面的 `plugins/builtin/`。
    #[test]
    fn builtin_plugins_follow_the_app_dir_while_user_plugins_follow_the_data_dir() {
        let _lock = test_app_dir_lock();
        let app = test_app_dir("plugin_dirs");
        set_app_dir(app.path());
        assert_eq!(
            plugins_builtin_dir(),
            app.path().join("plugins").join("builtin")
        );
        assert_eq!(
            plugins_user_dir(),
            app.path().join("data").join("plugins"),
            "用户插件必须在 data/ 里面，否则「更改数据文件夹」扫不到它"
        );
        assert_eq!(plugins_dir(), app.path().join("plugins"));
        // 「扫哪几处」只在这一处说：管理器取清单、插件页那一行「目录」都读它
        assert_eq!(
            plugin_scan_dirs(false),
            vec![plugins_builtin_dir(), plugins_user_dir()],
            "新布局扫两处，且内置在前（同名的以随包那份为准）"
        );
        assert_eq!(
            plugin_scan_dirs(true),
            vec![plugins_dir()],
            "回滚开关打开时只扫旧目录、忽略数据目录"
        );
        assert!(
            !plugins_dir().exists(),
            "旧目录不再被顺手建出来：它只在回滚分支里被创建（见 manager.rs 的 discover_with_source）"
        );
        assert!(
            !plugins_builtin_dir().exists(),
            "内置目录也不在建目录的手上：安装版落在 Program Files 时这里写不进去，\
             而插件清单每秒扫一次，建不出的那句 warn 会变成刷不完的日志"
        );

        // 数据目录换走之后，内置留在程序目录、用户插件跟着搬
        let elsewhere = app.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(
            app.path().join("config.ini"),
            format!("[paths]\ndata_home = {}\n", elsewhere.display()),
        )
        .unwrap();
        set_app_dir(app.path());
        assert_eq!(
            plugins_builtin_dir(),
            app.path().join("plugins").join("builtin"),
            "内置是程序的一部分，不跟着数据目录搬"
        );
        assert_eq!(
            plugins_user_dir(),
            elsewhere.join("data").join("plugins"),
            "用户插件是用户数据，跟着 data_home 走"
        );
        assert!(
            plugins_user_dir().is_dir(),
            "取用时该已经把目录建好（用户往里放 .lua 才有地方）"
        );
    }

    /// 插件的私有数据目录：一处一个子目录，名字不合法时不许跑到别处去。
    ///
    /// `state/` 与插件包（`plugins/`）分居两处是刻意的：内容哈希只扫包，运行时文件不会被
    /// 算进去，卸载插件也不会顺手删掉数据。
    /// 名字那一格是这条路径上唯一可能被外部字符串喂到的地方（store 的键、config.ini 里
    /// 手改过的 `disabled` 项都会往这儿流），一个 `../` 就能从"自己的目录"走到"别人的授权"。
    #[test]
    fn the_plugin_state_dir_is_one_per_stem_and_cannot_walk_out() {
        let _lock = test_app_dir_lock();
        let app = test_app_dir("state_dir");
        set_app_dir(app.path());
        let root = plugins_state_dir();

        let mine = plugin_state_dir("mytool");
        assert_eq!(mine, root.join("mytool"));
        assert!(mine.is_dir(), "取用时该已经把目录建好");
        // 读的一侧不 mkdir：`plugin_state_path` 只算位置。少了这一分，插件页每刷一次就给
        // 每个从不存东西的插件造出一格空目录（在真程序里现量到 5 个空目录、0 个文件）。
        let quiet = plugin_state_path("quiet");
        assert_eq!(quiet, root.join("quiet"), "两处算的必须是同一格");
        assert!(!quiet.exists(), "只是算了一下位置，目录不该被建出来");
        assert_eq!(plugin_state_dir("quiet"), quiet, "落盘那一侧才建");
        assert!(quiet.is_dir(), "落盘那一侧确实把目录建出来了");
        assert_eq!(
            plugin_state_dir("mytool"),
            mine,
            "同一个名字每次算出来是同一处"
        );
        assert_ne!(plugin_state_dir("other"), mine, "两个插件不共享一个目录");

        // 越界的名字不落盘、只出声，交回去的路径就是 state/ 那一格本身（不是某个插件的目录）
        for evil in ["../grants", "..\\x", "a/b", "..", ""] {
            let (got, logs) = crate::logger::capture_logs(|| plugin_state_dir(evil));
            assert_eq!(
                got, root,
                "越界的名字 {evil} 不该给出一个像样插件目录的路径"
            );
            assert!(
                logs.iter().any(|l| l.contains("不合法") && l.contains(evil)),
                "越界的名字要点名是哪个、并出声（静默给一个能用的路径等于让它下次真写进去）: {logs:?}"
            );
        }
    }
}
