//! 启动自检：把「子系统起没起来」的结论带出初始化路径。
//!
//! 起线程失败原来是 `.map_err(日志).ok()` 吞掉（B14）：DB 写线程起不来 =
//! 一条都不存、设备侧信道起不来 = 设备排行消失，而日志埋在 `logs/` 里，
//! 不主动翻就永远不知道。这里提供一个极薄的结果类型，让各 starter 把
//! 结论**返回**给组合根（`Database::init` / `AppState::init`），由它汇总成
//! 启动报告：写日志、经 `get_startup_report` 给前端 toast、托盘 tooltip
//! 带 ⚠ 计数。全绿静默，不出事不多话。
//!
//! 刻意做成实例数据（`Database::startup_checks`）而不是进程级静态：
//! 测试进程里几十个用例各初始化一次 `Database`，静态会互相污染。

/// 一条自检结论：`step` 是子系统名，`ok=false` 时 `detail` 是人能看懂的原因。
#[derive(Debug, Clone, serde::Serialize)]
pub struct CheckResult {
    pub step: String,
    pub ok: bool,
    pub detail: String,
}

impl CheckResult {
    pub fn ok(step: &str, detail: impl Into<String>) -> Self {
        Self {
            step: step.to_string(),
            ok: true,
            detail: detail.into(),
        }
    }

    pub fn fail(step: &str, detail: impl Into<String>) -> Self {
        Self {
            step: step.to_string(),
            ok: false,
            detail: detail.into(),
        }
    }
}
