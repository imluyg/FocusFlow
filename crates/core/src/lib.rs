//! FocusFlow 核心库：配置、日志、路径管理。
//!
//! 对应 Python 版 `config.py`、`logger.py` 的功能，后续数据库/统计/监听
//! 等纯逻辑模块也将放在此 crate 中（无 GUI 依赖，可被 CLI 与 GUI 复用）。

pub mod accounting;
pub mod app_stats;
pub mod autostart;
pub mod config;
pub mod db;
pub mod edge_history;
pub mod format;
pub mod listener;
pub mod logger;
pub mod migration;
pub mod paths;
pub mod plugins;
pub mod pomodoro;
pub mod scheduler;
pub mod stats;

pub use config::FocusFlowConfig;
pub use logger::init_logging;
