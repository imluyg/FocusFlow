//! 数据库模块。
//!
//! 镜像 Python 版 `database.py`：年度归档 SQLite 存储 + 单写线程 + 查询 API。

pub mod connection;
pub mod maintenance;
pub mod queries;
pub mod writer;

use std::sync::Arc;
use std::time::Duration;

use chrono::Datelike;

pub use queries::{
    available_years, get_alltime_max_day, get_alltime_summary, get_app_stats,
    get_app_stats_by_date, get_daily_counts, get_device_detail, get_device_stats,
    get_device_stats_by_date, get_hourly_stats, get_stats, get_stats_by_date, get_today_count,
    get_weekday_stats, invalidate_years_cache, DeviceDetail, DeviceStat,
};
pub use writer::DbWriter;

use crate::config::FocusFlowConfig;

/// 数据库门面：初始化 schema、归档检查、启动写入线程。
pub struct Database {
    /// 写入器（可空：CLI 只读模式不启动）
    writer: Option<Arc<DbWriter>>,
    /// 启动自检（B15）：init 路径上各子系统「起没起来」的结论。致命的失败
    /// 直接 `?` 让启动失败；不致命的（备份/采样/设备侧信道）收在这里，
    /// 由组合根汇总成启动报告（日志 / get_startup_report / 托盘 ⚠）。全绿静默。
    startup_checks: std::sync::Mutex<Vec<crate::startup::CheckResult>>,
}

impl Database {
    /// 初始化：建表、旧数据聚合迁移、归档检查、启动写入线程。
    ///
    /// `paused` 是采集层的共享暂停位（`listener::PauseFlag`），由组合根创建、
    /// 与 `InputListener` 同一份：本函数会把它转交给设备侧信道线程，
    /// 这样「暂停」能同时闸住 rdev 主链路与设备计数（不能各自拿一份拷贝）。
    pub fn init(
        config: &FocusFlowConfig,
        paused: crate::listener::PauseFlag,
    ) -> anyhow::Result<Arc<Self>> {
        let year = chrono::Local::now().year();
        let path = crate::paths::year_db_path(year);
        let conn = crate::db::connection::open_rw(&path)?;
        crate::db::connection::ensure_schema(&conn, year)?;
        // 兼容清理：若存在旧版独立 mouse_stats 表，安全删除
        let _ = conn.execute("DROP TABLE IF EXISTS mouse_stats", []);
        drop(conn);
        tracing::info!("数据库初始化完成: {} (年份={year})", path.display());

        // 旧版逐条数据 → 聚合表（先迁移，归档检查才能基于聚合表）
        maintenance::migrate_v2();

        // 聚合表一致性自愈（修正历史 delete_key_today 不同步扣减留下的偏差）
        maintenance::heal_daily_consistency();

        // 年度归档检查
        let yearly_archive = config.get_bool("database", "yearly_archive", true);
        maintenance::check_yearly_archive(yearly_archive);

        invalidate_years_cache();

        // 启动自愈：清掉旧版备份在 backup/ 里遗留的 -wal/-shm 垃圾
        // （备份已切回 rollback journal 模式、不再产生 sidecar；非空 WAL 会保留）
        let swept = maintenance::sweep_stale_sidecars();
        if swept > 0 {
            tracing::info!("启动清理：backup/ 中 {swept} 个遗留残留文件已删除");
        }

        // 设备归组键迁移（B14-2）：当前年度库上面的 ensure_schema 已迁，
        // 这里补上历史年度库 —— 否则跨年视图里同一台设备一库一key、两行显示。
        // 幂等；个别库占用时跳过（查询侧按身份段归组兜底），下次启动再试。
        maintenance::migrate_all_year_device_keys();
        // 别名文件的精确键跟着换轨（B14-2 配套）：换口后别名仍然命中
        crate::device_alias::migrate_exact_keys_to_identity();

        // 启动写入线程：起不来 = 一条都存不了，直接让启动失败（见 DbWriter::start）
        let flush_interval =
            Duration::from_secs(config.get_int("database", "flush_interval", 10).max(1) as u64);
        let writer = Some(DbWriter::start(flush_interval)?);
        // panic hook 兜底：进程异常终止前把未落库增量写入恢复文件
        writer::register_panic_recovery(Arc::clone(writer.as_ref().unwrap()));

        // 下面三个子系统起不来都不值得让整个程序死掉，但必须**留痕**：
        // 结论收进启动自检，由组合根汇总显性化（托盘/toast/日志）。
        let startup_checks = vec![
            // 运行中定时在线备份：进程被强杀不再丢失自上次备份后的全部数据。
            // 线程常驻、每分钟重读配置（online_backup_interval_hours，0 = 关闭），
            // 这样设置页里开关备份不必重启。
            maintenance::start_periodic_backup(),
            // 前台应用识别（写入 current_app，时长归属由写线程按键鼠事件完成；
            // Windows；[app_stats] enabled=false 或 exclude 可关停）
            crate::app_stats::start_sampler(Arc::clone(writer.as_ref().unwrap())),
            // 设备维度统计（Raw Input 侧信道，独立口径按设备归属计数；
            // Windows；[device_stats] enabled=false 可关停）
            // 共享暂停位一并交给它：设备计数必须和主链路一起停，否则暂停后「设备排行」还在涨
            crate::device_stats::start_device_stats(Arc::clone(writer.as_ref().unwrap()), paused),
        ];

        Ok(Arc::new(Self {
            writer,
            startup_checks: std::sync::Mutex::new(startup_checks),
        }))
    }

    /// 只读初始化（CLI 统计用，不启动写入线程）。
    pub fn init_readonly() -> Arc<Self> {
        // 旧格式库也先聚合迁移（CLI 直接读用户数据目录）
        maintenance::migrate_v2();
        invalidate_years_cache();
        Arc::new(Self {
            writer: None,
            startup_checks: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// 取走启动自检结果（组合根组装启动报告用；取后清空，只汇总一次）。
    pub fn take_startup_checks(&self) -> Vec<crate::startup::CheckResult> {
        std::mem::take(
            &mut *self
                .startup_checks
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        )
    }

    /// 获取写入器。
    pub fn writer(&self) -> Option<&Arc<DbWriter>> {
        self.writer.as_ref()
    }

    /// 记录一次按键。
    pub fn record_key(&self, key_name: &str, timestamp: i64) {
        if let Some(w) = &self.writer {
            w.record(key_name, timestamp);
        }
    }

    /// 立即 flush。
    pub fn flush(&self, wait: bool) {
        if let Some(w) = &self.writer {
            w.flush(wait);
        }
    }

    /// 优雅关闭：flush + 停止写线程 + 备份。
    ///
    /// 备份必须排在 `stop()` 之后：`stop()` 里处理线程还要做最后一次 flush_pending，
    /// 而 `flush(true)` 最多只等 3 秒、超时也只 warn（跨年那天新建年度库 + ensure_schema
    /// 真会超）。原先备份夹在两次 flush 中间，一旦第一次超时，那份备份就永久少了最后
    /// 几毫秒才落盘的增量 —— 而它正是次日恢复时要用的那一份。
    pub fn shutdown(&self, config: &FocusFlowConfig) {
        if let Some(w) = &self.writer {
            w.flush(true);
            w.stop();
            if config.get_bool("database", "backup_on_exit", true) {
                let max_backups = config.get_int("database", "max_backups", 5).max(1);
                maintenance::backup_database(max_backups);
            }
        }
        tracing::info!("数据库已关闭");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// B15：init 路径上各子系统的「起没起来」必须收进 startup_checks，
    /// 不能再吞回各自函数里（原来 `.map_err(日志).ok()` 之后外面什么都看不见，
    /// 备份/设备统计悄悄消失只有翻日志才知道）。取走即清空：报告只汇总一次。
    #[test]
    fn database_init_collects_startup_checks() {
        let _lock = crate::paths::test_app_dir_lock();
        let dir = crate::paths::test_app_dir("db_init_checks");
        // 危险子系统全部关掉（设备统计会起 Raw Input 循环、在线备份会写盘），
        // flush_interval 给到一年：用例结束后不得有任何后台线程再碰这个临时目录
        // （§八·2 那类泄漏）。值直接预置进文件，**不走 set()**：set 会把信号发给
        // 全局去抖保存线程，那个线程存的是全局 INSTANCE —— 而它可能已被某个
        // 不死线程（采样/备份）在**别的用例的目录**里惰性初始化过，300ms 后一次
        // 落盘就把那个已删的目录原样建回来（实测每轮 +1 个只剩 config.ini 的
        // ff_archive_*，就是这么来的）。
        std::fs::write(
            dir.path().join("config.ini"),
            "[device_stats]\nenabled = false\n\
             [database]\nonline_backup_interval_hours = 0\n\
             backup_on_exit = false\nflush_interval = 31536000\n",
        )
        .unwrap();
        let cfg = crate::config::FocusFlowConfig::load(dir.path().join("config.ini")).unwrap();

        let paused = crate::listener::new_pause_flag();
        let db = Database::init(&cfg, paused).expect("init 应成功");
        let checks = db.take_startup_checks();
        let steps: Vec<&str> = checks.iter().map(|c| c.step.as_str()).collect();
        for step in ["定时备份线程", "前台应用采样线程", "设备统计线程"] {
            assert!(
                steps.contains(&step),
                "自检结论里必须有「{step}」: {steps:?}"
            );
        }
        assert!(
            checks.iter().all(|c| c.ok),
            "这些线程在测试环境里应该都起来了（设备统计是显式未启用，也算 ok）: {checks:?}"
        );
        // 取走即清空
        assert!(
            db.take_startup_checks().is_empty(),
            "报告只汇总一次，第二次取应为空"
        );
        // 停掉写线程：否则它按 flush_interval 醒来，可能把已删掉的临时目录建回来
        db.shutdown(&cfg);
    }
}
