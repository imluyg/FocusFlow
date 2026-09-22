//! 日志模块。
//!
//! 镜像 Python 版 `logger.py`：
//! - 文件日志写入 `logs/`，**按天轮转、最多保留 4 个文件**（不是按大小轮转，
//!   避免单文件无限增长）。轮转由 tracing-appender 负责，文件名即日期
//!   （实测 `logs/2026-09-21`），所以不要去拼一个 `focusflow.log` 路径
//! - 控制台输出 ERROR 及以上（Windows 下通常无控制台，仅开发时可见）
//! - 全局 panic hook 记录未捕获错误

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter, Layer};

/// 非阻塞日志 worker 的 guard。
///
/// 存全局是为了别让它在 `init_logging` 返回时就随语句结束被 drop。但要注意
/// **static 在进程退出时不会运行析构函数**，所以「存全局」本身并不刷盘 ——
/// 尾部日志（往往恰好是关闭过程那几条最该看的话）要靠 [`shutdown`] 显式 drop
/// 才会写完。原先的注释写着「程序退出时保证日志落盘」，那是错的。
static LOG_GUARD: Mutex<Option<WorkerGuard>> = Mutex::new(None);

/// 是否已初始化。tracing 的 `init()` 内部是 `try_init().expect(...)`，
/// 第二次调用直接 panic（release 下 panic=abort），所以幂等必须自己挡。
static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// 停掉日志 worker 并写完缓冲区中的日志。
///
/// 只会在「本函数返回后进程即终止」的出口上用到（见 desktop 的
/// `RunEvent::Exit`）；在那里调用之前，最后若干条日志还留在非阻塞通道里，
/// 进程一退就没了。可重复调用。
pub fn shutdown() {
    let guard = LOG_GUARD.lock().unwrap_or_else(|e| e.into_inner()).take();
    drop(guard);
}

/// 安装全局 panic hook：未捕获的 panic 记录为 critical 日志，
/// 并在进程终止前把未落库增量写入恢复文件（下次启动回放，见 db::writer）。
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .unwrap_or("unknown panic");
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "?".to_string());
        tracing::error!(target: "panic", "未捕获的 panic: {payload} @ {location}");
        crate::db::writer::panic_recovery_snapshot();
    }));
}

/// 初始化日志系统（文件轮转 + 控制台）。
///
/// 与 Python 版 `logger._build_logger()` 对应。幂等：重复调用只生效一次
/// （tracing 的 `init()` 二次调用会 panic，见 `INITIALIZED`）。
/// `WorkerGuard` 由全局持有，落盘要靠 [`shutdown`] 显式释放。
pub fn init_logging() {
    if INITIALIZED.swap(true, Ordering::SeqCst) {
        return;
    }

    // 日志目录
    std::fs::create_dir_all(crate::paths::log_dir()).ok();

    // 文件 appender：按天滚动，保留最近 4 个文件（避免单文件无限增长）
    let file_appender = tracing_appender::rolling::Builder::new()
        .max_log_files(4)
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .build(crate::paths::log_dir())
        .expect("创建日志 appender 失败");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
    // 收好 guard：它一被 drop，worker 线程就停转、后续日志静默丢弃
    *LOG_GUARD.lock().unwrap_or_else(|e| e.into_inner()) = Some(guard);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let file_layer = fmt::layer()
        .with_writer(file_writer)
        .with_ansi(false)
        .with_target(true)
        .with_thread_names(true)
        .with_thread_ids(true);
    // 控制台直接同步写 stdout，不再套一层 non_blocking：
    // 那样要多管一个 guard，而它原先在同一条语句里就被丢弃了，
    // worker 随即停转 —— 控制台层实际上是死的。ERROR 量小，同步写无碍。
    let console_layer = fmt::layer()
        .with_writer(std::io::stdout)
        .with_ansi(true)
        .with_target(false)
        .with_filter(tracing_subscriber::filter::LevelFilter::ERROR);

    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(console_layer)
        .init();

    install_panic_hook();
    // 记录目录而不是文件名：按天轮转后名字由 tracing-appender 决定（logs/<日期>）
    tracing::info!("日志系统已初始化: {}", crate::paths::log_dir().display());
}

/// 判断日志系统是否已初始化（`shutdown` 之后仍为 true：它表示「装过 subscriber」，
/// 而 tracing 的全局 subscriber 撤不下来）。
pub fn is_initialized() -> bool {
    INITIALIZED.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    /// `shutdown` 的契约：释放 guard、把尾部日志写完、可重复调用。
    ///
    /// 注：这条只能算契约/冒烟测试 —— non_blocking 的 worker 平时也会很快把行
    /// 写下去，所以「不调 shutdown 就丢尾部」在这里断言不出确定性（那是进程
    /// 终止那一刻才成立的事实）。真正被严格锁住的是 shutdown 本身存在且有效：
    /// 它一旦被改回「guard 塞进 OnceLock 再也不动」，尾部就随进程一起没了。
    #[test]
    fn shutdown_flushes_and_is_repeatable() {
        let _lock = crate::paths::test_app_dir_lock();
        let dir = std::env::temp_dir().join(format!("ff_logflush_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        crate::paths::set_app_dir(&dir);
        super::init_logging();
        // init_logging 自称幂等：tracing 的 init() 二次调用会 panic，这里验一道
        super::init_logging();

        let marker = format!("flush-probe-{}", std::process::id());
        tracing::info!("{marker}");
        super::shutdown();
        super::shutdown();

        // 按天轮转的文件名由 tracing-appender 决定（实测 logs/<日期>），只能扫目录
        let logs = crate::paths::log_dir();
        let written = std::fs::read_dir(&logs)
            .map(|entries| {
                entries.flatten().any(|e| {
                    std::fs::read_to_string(e.path())
                        .map(|s| s.contains(&marker))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        assert!(
            written,
            "shutdown 之后尾部日志应已落盘到 {}",
            logs.display()
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
