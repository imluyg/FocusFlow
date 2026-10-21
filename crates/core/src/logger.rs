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

/// 文件日志那一层**没**建起来的原因；`None` = 建起来了（或还没试过）。
///
/// 立这一格的理由：`init_logging` 在装好 subscriber 之后就不会再装第二次，所以
/// 文件层一旦没建成，**本进程一辈子都没有文件日志** —— 而 `logs/` 是本仓每个故障
/// 的默认下一步（"去日志里查原因"）。原先这件事只往 stderr 印一句，而桌面端是
/// windows 子系统、没有控制台，等于完全无声：用户按提示去翻 `logs/`，翻到的是空目录，
/// 谁也不知道少了一层。现在把它存下来，由启动报告带出来（`is_initialized()` 说的
/// 是"subscriber 装过"，不代表文件日志可用 —— 这两件事别再混）。
static FILE_LOG_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// 记录一次文件层的结局：成功清掉旧原因，失败留下原因。
fn record_file_log_outcome(outcome: Result<(), String>) {
    let mut slot = FILE_LOG_ERROR.lock().unwrap_or_else(|e| e.into_inner());
    // 成功 ⇒ 清掉旧原因；失败 ⇒ 留下它（`Option::err` 正好是这个形状）
    *slot = outcome.err();
}

/// 文件日志这一层此刻是否可用；不可用时带回**为什么**（供启动报告/自检显示）。
pub fn file_log_failure() -> Option<String> {
    FILE_LOG_ERROR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

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
        // 上面那条 error 走的是 non_blocking 通道，而 release 是 `panic = "abort"`：
        // 紧随其后进程就没了，那条"唯一的死因"多半还留在缓冲区里 —— 于是"双击没反应"
        // 连一行日志都不剩（`shutdown` 的注释记过这件事：落盘靠显式 drop，存全局不管）。
        // 所以这里在 abort 之前显式 flush 一次。
        //
        // 只在真正会 abort 的构建里做：debug/测试是 unwind，一个用例 panic 之后
        // 进程还要继续跑完其余用例，而 guard 一 drop 之后整个进程的日志就静默死了
        // （`shutdown_flushes_and_is_repeatable` 那类读日志文件的用例会因此偶发红）。
        #[cfg(not(test))]
        flush_before_abort();
    }));
}

/// panic 路径上的日志 flush。
///
/// 与上面的调用点同样只在会 abort 的构建里存在（测试构建里没人调它）。
///
/// **不能直接复用 [`shutdown`]**：那个函数拿的是阻塞锁，而 panic 完全可能发生在
/// 正持有 `LOG_GUARD` 的线程里（日志自己出错、或 appender 写盘时炸），在 hook 里
/// 一阻塞就把"崩溃至少留下一行"换成"进程挂死在那里"。拿不到锁就放弃 ——
/// 少一行日志比整个程序卡死好。
#[cfg(not(test))]
fn flush_before_abort() {
    if let Ok(mut guard) = LOG_GUARD.try_lock() {
        let taken = guard.take();
        drop(taken); // `WorkerGuard` 的 Drop 会把队列里的尾部写完
    }
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

    // 文件 appender：按天滚动，保留最近 4 个文件（避免单文件无限增长）。
    //
    // 建不出来时不能 panic：目录取自 `paths::log_dir()` → `app_dir`，而 `app_dir` 可以被
    // `FOCUSFLOW_APP_DIR` 指到任何地方（不可写的目录、已被删掉的路径、或一个同名普通文件
    // 都会走到这条 Err 分支）。更要紧的是 `init_logging()` 在命令行参数解析**之前**就被调用，
    // 而 release 是 `panic = "abort"` —— 进程连一行用法提示都印不出来，用户看到的是
    // "双击没反应、也没有任何报错"。现在退化成只走控制台层，程序照常能用。
    let file_layer = match tracing_appender::rolling::Builder::new()
        .max_log_files(4)
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .build(crate::paths::log_dir())
    {
        Ok(file_appender) => {
            // 队列槽是**创建时一次性分配**的：crossbeam 的 `bounded(N)` 会预分配整块
            // `Box<[Slot<Msg>]>`，每槽约 40 字节（Slot 未做 cache padding，最大变体是
            // 24 字节的 `Vec<u8>`）。默认 128_000 行 ⇒ **常驻约 5 MB**，与"有没有写满"
            // 无关 —— 相对整个 Rust 进程 12 MB 的私有提交，这一项就占了约四成。
            //
            // 16384 行（约 0.65 MB）在本程序的 `info` 级日志下等于永远填不满。
            // 唯一代价：`RUST_LOG=debug`（listener 每事件一行）时才可能触到上限，
            // 而 lossy 模式丢的是**最新**那条 —— 包括 panic hook 想留下的那一行。
            // 这是本条改动已知且有限的代价，用远大于最小可用值的余量把它压到最小。
            let (file_writer, guard) =
                tracing_appender::non_blocking::NonBlockingBuilder::default()
                    .buffered_lines_limit(16_384)
                    .lossy(true)
                    .finish(file_appender);
            // 收好 guard：它一被 drop，worker 线程就停转、后续日志静默丢弃
            *LOG_GUARD.lock().unwrap_or_else(|e| e.into_inner()) = Some(guard);
            record_file_log_outcome(Ok(()));
            Some(
                fmt::layer()
                    .with_writer(file_writer)
                    .with_ansi(false)
                    .with_target(true)
                    .with_thread_names(true)
                    .with_thread_ids(true),
            )
        }
        Err(e) => {
            // 同一句话两个去处：stderr 给 CLI（它的结论分流本来就靠这条），
            // 而桌面端没有控制台 ⇒ 只能靠 `file_log_failure()` 让启动报告说得出这件事
            // （见 desktop 的 `AppState::init`）。
            let why = format!(
                "日志文件建不出来（目录 {}）：本次运行只有控制台输出、不落盘文件。原因: {e}",
                crate::paths::log_dir().display()
            );
            record_file_log_outcome(Err(why.clone()));
            eprintln!("{why}");
            None
        }
    };

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // 控制台只同步写 **stderr**，不再套一层 non_blocking：
    // 那样要多管一个 guard，而它原先在同一条语句里就被丢弃了，
    // worker 随即停转 —— 控制台层实际上是死的。ERROR 量小，同步写无碍。
    //
    // 为什么必须是 stderr：CLI 把自己的结论严格分流（成功那句走 stdout，未完成/失败走
    // stderr，因为这条命令挂在计划任务上，运维收的就是 stdout）。这一层只放 ERROR 级
    // 消息 —— 它落在 stdout 上，等于把「执行前快照只兜住一份」这类失败掺进那份
    // 看着像成功的产物里，正是本仓为 `--backup` / `--cleanup` 反复修过的那一族。
    // 桌面端是 windows 子系统、根本没有控制台，改动对它无感。
    let console_layer = fmt::layer()
        .with_writer(std::io::stderr)
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

/// 抓**当前线程**产生的 tracing 消息（`with_default` 是线程本地的作用域）：
/// 并行的其它用例既不会被这里收走，也不会串进来。
///
/// 供"某条分支必须出声"这类断言用 —— 那种缺陷唯一的症状就是一行日志，
/// 没有这个夹具就只能眼验（先例：`device_alias::migration_keeps_the_first_alias_and_warns_about_the_dropped_one`）。
#[cfg(test)]
pub(crate) fn capture_logs<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
    use tracing::Subscriber;
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

    #[derive(Clone, Default)]
    struct Sink(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    struct MsgVisitor<'a>(&'a mut String);

    impl tracing::field::Visit for MsgVisitor<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                *self.0 = format!("{value:?}");
            }
        }
    }

    impl<S: Subscriber> Layer<S> for Sink {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            let mut msg = String::new();
            event.record(&mut MsgVisitor(&mut msg));
            self.0.lock().unwrap_or_else(|e| e.into_inner()).push(msg);
        }
    }

    let sink = Sink::default();
    let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(sink.clone()));
    let out = tracing::dispatcher::with_default(&dispatch, f);
    let msgs = sink.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
    (out, msgs)
}

#[cfg(test)]
mod tests {
    /// 控制台那一层只许写 stderr。
    ///
    /// 为什么钉在这一处：CLI 的 stdout 是**给计划任务收的那份结论**，所以它自己严格
    /// 分流（成功那句走 stdout、未完成与失败走 stderr）。这一层放的是 ERROR 级消息，
    /// 落在 stdout 上就等于把「执行前快照只兜住一份」「某年库没压缩成」这类失败掺进
    /// 那份看着像成功的产物里 —— `--backup` / `--cleanup` / `--vacuum` 为同一族修过三轮，
    /// 漏的这一半在日志层。桌面端是 windows 子系统、没有控制台，所以改动对它无感。
    ///
    /// 这条只能钉形状：全局 subscriber 装好就撤不下来，同一进程里没法再拿第二个
    /// subscriber 去观察"ERROR 到底写到哪个流"。
    #[test]
    fn the_console_layer_writes_to_stderr() {
        let src = include_str!("logger.rs");
        let at = src
            .find("let console_layer")
            .expect("控制台层不见了（删掉它之前请先想想到底的 ERROR 往哪儿走）");
        let rest = &src[at..];
        let body = &rest[..rest.find(';').map(|n| n + 1).unwrap_or(rest.len())];
        assert!(
            body.contains("with_writer(std::io::stderr)"),
            "控制台层不再写 stderr，失败消息会混进 CLI 给计划任务的那份 stdout：{body}"
        );
        assert!(
            !body.contains("stdout"),
            "同一层里不能同时引用 stdout：{body}"
        );
    }

    /// `shutdown` 的契约：释放 guard、把尾部日志写完、可重复调用。
    ///
    /// 注：这条只能算契约/冒烟测试 —— non_blocking 的 worker 平时也会很快把行
    /// 写下去，所以「不调 shutdown 就丢尾部」在这里断言不出确定性（那是进程
    /// 终止那一刻才成立的事实）。真正被严格锁住的是 shutdown 本身存在且有效：
    /// 它一旦被改回「guard 塞进 OnceLock 再也不动」，尾部就随进程一起没了。
    #[test]
    fn shutdown_flushes_and_is_repeatable() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("logflush");
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
    }

    /// 文件层那一层的结局必须存得住、也清得掉。
    ///
    /// `init_logging` 只有第一次会真正装 subscriber ⇒ "这次没建成"是一整轮的既成事实，
    /// 所以这件事得有个能被别人读走的地方，而不是只往 stderr 印一句（桌面端没有控制台）。
    /// 这里钉的是那对读写本身；appender 到底建没建得起来不是用例能重复演的
    /// （全局 subscriber 装好就撤不下来），那一半由下面的形状守卫守着两个臂。
    #[test]
    fn the_file_layer_outcome_is_readable_and_a_success_clears_it() {
        let _lock = crate::paths::test_app_dir_lock();
        // 串行前提：同一条锁也是别的用例调 `init_logging` 时用的那把，
        // 否则并发的一次初始化会把这里刚存的擦掉（表现为偶发红）。
        super::record_file_log_outcome(Err("目录 X 建不出来".to_string()));
        assert_eq!(
            super::file_log_failure().as_deref(),
            Some("目录 X 建不出来"),
            "没建起来的这一层要把原因留给人读"
        );
        super::record_file_log_outcome(Ok(()));
        assert_eq!(
            super::file_log_failure(),
            None,
            "建成之后不许还留着上一次的失败（启动报告会说假话）"
        );
    }

    /// 形状守卫：`init_logging` 的两个臂各记录一次结局，且 CLI 那半句仍然出声。
    ///
    /// 上面那条用例跑的是抽出来的那对读写，它盯不到真正的构造点 —— 而这场缺陷的落点
    /// 就在那两个臂里（原先失败只 `eprintln!`，桌面端没人能看见）。
    #[test]
    fn both_file_layer_arms_record_their_outcome() {
        let src = include_str!("logger.rs");
        let at = src
            .find("pub fn init_logging()")
            .expect("init_logging 不见了");
        let body = &src[at..at + src[at..].find("\n}\n").expect("init_logging 的结尾")];
        // 摘掉注释行再比：判据的字样也出现在注释里时，一条被注释掉的调用会替
        // 真正的那一条"过关"（本仓为同一件事红过两次）。
        let body = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut wrong: Vec<String> = Vec::new();
        for needle in [
            "record_file_log_outcome(Ok(()))",
            "record_file_log_outcome(Err(why.clone()))",
            "eprintln!(\"{why}\")",
        ] {
            if !body.contains(needle) {
                wrong.push(needle.to_string());
            }
        }
        if body.matches("record_file_log_outcome(").count() != 2 {
            wrong.push("两个臂应各记录一次结局".to_string());
        }
        assert!(
            wrong.is_empty(),
            "文件日志层的结局没被记下来（缺: {}）—— 切片没盖住函数体也会红在这里",
            wrong.join("、")
        );
    }
}
