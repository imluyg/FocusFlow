//! 实时统计：CPM（每分钟操作数）计算。
//!
//! 镜像 Python 版 `stats.py`：
//! - 滑动时间窗口 + deque 上限安全阀
//! - 写入/查询分离，查询时惰性清理过期数据
//! - 结果缓存（TTL 500ms）
//! - 线程安全

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::config::FocusFlowConfig;

/// CPM 内部状态（时间戳队列 + 结果缓存合并为单锁，减少热路径锁争用）。
struct CpmState {
    /// 时间戳队列（单调时钟 Instant）
    timestamps: VecDeque<Instant>,
    /// 缓存结果
    cached_count: i64,
    cached_at: Instant,
}

/// CPM 计算器。
pub struct CpmCalculator {
    /// 窗口（秒）
    window: f64,
    state: Mutex<CpmState>,
}

impl CpmCalculator {
    pub fn new(window: f64) -> Self {
        Self {
            window: window.max(1.0),
            state: Mutex::new(CpmState {
                timestamps: VecDeque::with_capacity(4096),
                cached_count: 0,
                cached_at: Instant::now() - std::time::Duration::from_secs(10),
            }),
        }
    }

    /// 记录一次操作时间戳。
    pub fn record(&self) {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.timestamps.push_back(now);
        // 硬上限安全阀（对应 Python maxlen=100000）
        if state.timestamps.len() > 100_000 {
            state.timestamps.pop_front();
        }
        // 顺带清理窗口外旧数据
        let cutoff = now - std::time::Duration::from_secs_f64(self.window);
        while let Some(&front) = state.timestamps.front() {
            if front < cutoff {
                state.timestamps.pop_front();
            } else {
                break;
            }
        }
        // 写入时缓存失效
        state.cached_count = 0;
        state.cached_at = now - std::time::Duration::from_secs(10);
    }

    /// 获取当前 CPM（窗口内事件数）。
    pub fn get_cpm(&self) -> i64 {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.cached_at.elapsed() < std::time::Duration::from_millis(500) {
            return state.cached_count;
        }
        let cutoff = now - std::time::Duration::from_secs_f64(self.window);
        while let Some(&front) = state.timestamps.front() {
            if front < cutoff {
                state.timestamps.pop_front();
            } else {
                break;
            }
        }
        let count = state.timestamps.len() as i64;
        state.cached_count = count;
        state.cached_at = now;
        count
    }

    /// 重置（清除当前时间戳）。
    pub fn reset(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.timestamps.clear();
        state.cached_count = 0;
        state.cached_at = Instant::now() - std::time::Duration::from_secs(10);
    }
}

/// 全局 CPM 单例。
static CPM: std::sync::OnceLock<Arc<CpmCalculator>> = std::sync::OnceLock::new();

/// 获取全局 CPM 计算器。
pub fn cpm(config: &'static FocusFlowConfig) -> Arc<CpmCalculator> {
    Arc::clone(CPM.get_or_init(|| {
        let window = config.get_float("stats", "cpm_window", 60.0);
        Arc::new(CpmCalculator::new(window))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpm_window() {
        let calc = CpmCalculator::new(60.0);
        for _ in 0..10 {
            calc.record();
        }
        assert_eq!(calc.get_cpm(), 10);
        calc.reset();
        assert_eq!(calc.get_cpm(), 0);
    }
}
