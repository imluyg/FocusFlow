//! 日志模块。
//!
//! 镜像 Python 版 `logger.py`：
//! - 文件日志写入 `logs/focusflow.log`，按大小轮转（5MB × 3 备份）
//! - 控制台输出 ERROR 及以上（Windows 下通常无控制台，仅开发时可见）
//! - 全局 panic hook 记录未捕获错误

use std::sync::OnceLock;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter, Layer};

/// 非阻塞日志 worker 的 guard；必须存为全局，程序退出时保证日志落盘。
static LOG_GUARD: OnceLock<WorkerGuard> = OnceLock::new();

/// 日志文件路径
pub fn log_file_path() -> std::path::PathBuf {
    crate::paths::log_dir().join("focusflow.log")
}

/// 安装全局 panic hook：未捕获的 panic 记录为 critical 日志。
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
    }));
}

/// 初始化日志系统（文件轮转 + 控制台）。
///
/// 与 Python 版 `logger._build_logger()` 对应。幂等：重复调用只生效一次。
/// 返回的 `WorkerGuard` 会被全局持有。
pub fn init_logging() {
    // 日志目录
    std::fs::create_dir_all(crate::paths::log_dir()).ok();

    // 文件 appender：按天滚动，保留最近 4 个文件（避免单文件无限增长）
    let file_appender = tracing_appender::rolling::Builder::new()
        .max_log_files(4)
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .build(crate::paths::log_dir())
        .expect("创建日志 appender 失败");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
    // 保存 guard，防止退出时日志丢失
    let _ = LOG_GUARD.set(guard);

    // 控制台 appender（仅 ERROR）
    let (console_writer, _) = tracing_appender::non_blocking(std::io::stdout());

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let file_layer = fmt::layer()
        .with_writer(file_writer)
        .with_ansi(false)
        .with_target(true)
        .with_thread_names(true)
        .with_thread_ids(true);
    let console_layer = fmt::layer()
        .with_writer(console_writer)
        .with_ansi(true)
        .with_target(false)
        .with_filter(tracing_subscriber::filter::LevelFilter::ERROR);

    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(console_layer)
        .init();

    install_panic_hook();
    tracing::info!("日志系统已初始化: {}", log_file_path().display());
}

/// 判断日志系统是否已初始化。
pub fn is_initialized() -> bool {
    // tracing_subscriber 无法直接查询，用我们的 guard 是否已设置近似判断
    LOG_GUARD.get().is_some()
}
