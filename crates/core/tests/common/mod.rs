//! 集成测试共用的程序目录夹具（三个插件二进制都要用）。
//!
//! 存在的理由：这些用例要读随包的 `plugins/*.lua`，而宿主只按 `app_dir()/plugins/`
//! 去找插件 —— 旧写法于是直接 `set_app_dir(current_dir())`，可 cargo 给集成测试的 cwd
//! 就是包目录 `crates/core`，那等于把**仓库**声明成程序目录。后果不是"脏"这么简单：
//! 宿主会在里面建 `data/`、`backup/`、`logs/`、`config.ini`，实测留下过一份带着
//! 2026-09 数据的 `crates/core/data/focusflow_2026.db`（被 .gitignore 挡住，看不见），
//! 而它给下一条用例留了旧行 —— 表现成偶发的
//! `UNIQUE constraint failed: daily_counts.date_key`：随机红，且 `--test-threads=1`
//! 永远全绿。要读的只是那几个 `.lua` 字节，复制一份进临时目录即可，仓库字节全程不动。
//!
//! 换目录之后必须**自己把表建出来**（见下面那三次 `init_db`）：宿主那边的
//! `ensure_accounting_db` / `ensure_pomodoro_db` 用的是进程级 `OnceLock` 闩，一个进程
//! 只建一次，谁先跑就在谁的目录里建。本文件里已经有用例踩过这个形状
//! （`host_accounting_category_add_returns_a_reason` 的注释记着同一条）。

use focusflow_core::paths;

/// 建一个隔离的临时程序目录，把随包插件复制进去、把表建好，并把全局 `app_dir` 锚过去。
///
/// 返回值守着期间目录可用，离开作用域（含 panic 路径）自动删除并把 `app_dir` 恢复到
/// 哨兵 —— 那套收尾逻辑在 `paths::TestAppDir::drop` 里，不在这里重抄。
/// 调用方必须自己持有本文件的串行守卫：切换的是进程级全局目录。
pub fn isolated_app_dir(tag: &str) -> paths::TestAppDir {
    let app = paths::test_app_dir(&format!("plugins_{tag}"));
    let shipped = std::env::current_dir()
        .expect("集成测试的 cwd 是 crates/core")
        .join("plugins");
    let dst = app.path().join("plugins");
    std::fs::create_dir_all(&dst).expect("建临时 plugins 目录");
    let mut copied = 0usize;
    for entry in std::fs::read_dir(&shipped).unwrap_or_else(|e| {
        panic!("读随包 plugins 目录 {} 失败: {e}", shipped.display());
    }) {
        let path = entry.expect("读目录项失败").path();
        if path.extension().is_some_and(|e| e == "lua") {
            let name = path.file_name().expect("复制来的路径必须有文件名");
            std::fs::copy(&path, dst.join(name))
                .unwrap_or_else(|e| panic!("复制 {} 进临时目录失败: {e}", path.display()));
            copied += 1;
        }
    }
    // 连 `config.ini` 一起复制：这些插件用例过去读的正是包目录里那份（里面有
    // `[paths] data_home`、`[scheduler] allow_extra` 一类），只给默认值会让它们对着一份
    // 不同的配置跑。复制而不是留空 = 仓库字节不动、用例读到的内容与改前一致。
    let shipped_cfg = std::env::current_dir()
        .expect("集成测试的 cwd 是 crates/core")
        .join("config.ini");
    if shipped_cfg.is_file() {
        std::fs::copy(&shipped_cfg, app.path().join("config.ini"))
            .unwrap_or_else(|e| panic!("复制 {} 失败: {e}", shipped_cfg.display()));
    }
    // 夹具自己的哨兵：复制数少到不合理 = 随包目录变了或读法错了，
    // 那样下面所有断言都在对着一个空 plugins 目录跑，会比"没测到"更糟（它绿）。
    assert!(
        copied >= 5,
        "复制到的内置插件少到不合理（实数 {copied}）：随包 plugins 目录是不是变了？"
    );
    // 换了程序目录就等于换了一套库：年度库清单的缓存必须作废，否则会读到上一个
    // 目录列出来的年份（与 `bundled_in` 同一处理）。
    focusflow_core::db::queries::invalidate_years_cache();
    // 宿主只在进程内建一次表（`OnceLock` 闩），本夹具换了目录就得自己补上，
    // 否则这个目录里的库永远是空的 —— 表现成隔壁用例"分类清单为空 ⇒ 点了没反应"。
    focusflow_core::accounting::init_db().expect("临时目录里的记账库应能建起来");
    focusflow_core::pomodoro::init_db().expect("临时目录里的番茄钟库应能建起来");
    focusflow_core::scheduler::init_db().expect("临时目录里的定时任务库应能建起来");
    app
}
