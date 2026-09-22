//! 实时统计：CPM（每分钟操作数）计算。
//!
//! 镜像 Python 版 `stats.py`：
//! - 滑动时间窗口 + deque 上限安全阀
//! - 写入/查询分离，查询时惰性清理过期数据
//! - 结果缓存（TTL 500ms）
//! - 线程安全

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::FocusFlowConfig;

/// CPM 内部状态（时间戳队列 + 结果缓存合并为单锁，减少热路径锁争用）。
struct CpmState {
    /// 时间戳队列（单调时钟 Instant）
    timestamps: VecDeque<Instant>,
    /// 缓存结果
    cached_count: i64,
    /// 算出 `cached_count` 的时刻；`None` = 缓存无效，下次查询重算。
    ///
    /// 用 `Option` 而不是「把时刻伪造成 now - 10s」来表示过期：`Instant` 减法在
    /// 开机时间短于该时长时下溢 panic，而 release 的 panic=abort 会让开机自启
    /// 撞上「登录后台刚起来、一敲键盘整个程序就没了」。
    /// 同一个坑在 app_stats.rs / db/queries.rs 都留过告诫。
    cached_at: Option<Instant>,
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
                cached_at: None,
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
        // 顺带清理窗口外旧数据。判定写成 `now - front > window` 而不是
        // `front < now - window`：后者要算出那个过去时刻，开机不足窗口长度时
        // Instant 减法会下溢 panic（release 下 panic=abort，每次按键都走这里）。
        let window = Duration::from_secs_f64(self.window);
        while let Some(&front) = state.timestamps.front() {
            if now.duration_since(front) > window {
                state.timestamps.pop_front();
            } else {
                break;
            }
        }
        // 写入使缓存失效
        state.cached_at = None;
    }

    /// 获取当前 CPM（窗口内事件数）。
    pub fn get_cpm(&self) -> i64 {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = state.cached_at {
            if at.elapsed() < Duration::from_millis(500) {
                return state.cached_count;
            }
        }
        let window = Duration::from_secs_f64(self.window);
        while let Some(&front) = state.timestamps.front() {
            if now.duration_since(front) > window {
                state.timestamps.pop_front();
            } else {
                break;
            }
        }
        let count = state.timestamps.len() as i64;
        state.cached_count = count;
        state.cached_at = Some(now);
        count
    }

    /// 重置（清除当前时间戳）。
    pub fn reset(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.timestamps.clear();
        state.cached_count = 0;
        state.cached_at = None;
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

    /// 回归：窗口长度超过机器开机时长时不得 panic。
    ///
    /// 原先 record()/get_cpm() 用 `Instant::now() - Duration::from_secs_f64(window)`
    /// 算窗口起点，而默认窗口 60 秒 —— 开机自启后一分钟内第一次敲键就下溢 panic，
    /// release 的 panic=abort 让它表现为「刚开机、一动键盘程序就消失」。
    /// 这里把窗口取成必然大于任何开机时长的秒数，与机器实际开了多久无关：
    /// 改成 `now - front > window` 判定后只是永远不淘汰时间戳，不再 panic。
    #[test]
    fn cpm_window_longer_than_uptime_does_not_panic() {
        let calc = CpmCalculator::new(1e12);
        for _ in 0..5 {
            calc.record();
        }
        assert_eq!(calc.get_cpm(), 5, "窗口远超开机时长：没有一条记录算过期");
        calc.reset();
        assert_eq!(calc.get_cpm(), 0);
    }
}
