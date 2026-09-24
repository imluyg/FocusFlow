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

        // 启动写入线程
        let flush_interval =
            Duration::from_secs(config.get_int("database", "flush_interval", 10).max(1) as u64);
        let writer = Some(DbWriter::start(flush_interval));
        // panic hook 兜底：进程异常终止前把未落库增量写入恢复文件
        writer::register_panic_recovery(Arc::clone(writer.as_ref().unwrap()));

        // 运行中定时在线备份：进程被强杀不再丢失自上次备份后的全部数据。
        // 线程常驻、每分钟重读配置（online_backup_interval_hours，0 = 关闭），
        // 这样设置页里开关备份不必重启。
        maintenance::start_periodic_backup();

        // 前台应用识别（写入 current_app，时长归属由写线程按键鼠事件完成；
        // Windows；[app_stats] enabled=false 或 exclude 可关停）
        crate::app_stats::start_sampler(Arc::clone(writer.as_ref().unwrap()));

        // 设备维度统计（Raw Input 侧信道，独立口径按设备归属计数；
        // Windows；[device_stats] enabled=false 可关停）
        // 共享暂停位一并交给它：设备计数必须和主链路一起停，否则暂停后「设备排行」还在涨
        crate::device_stats::start_device_stats(Arc::clone(writer.as_ref().unwrap()), paused);

        Ok(Arc::new(Self { writer }))
    }

    /// 只读初始化（CLI 统计用，不启动写入线程）。
    pub fn init_readonly() -> Arc<Self> {
        // 旧格式库也先聚合迁移（CLI 直接读用户数据目录）
        maintenance::migrate_v2();
        invalidate_years_cache();
        Arc::new(Self { writer: None })
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
