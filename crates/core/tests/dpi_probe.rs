//! DPI 感知探针测的是**自己这个进程**，不是别人的窗口。
//!
//! 为什么单独开一个测试二进制：`SetProcessDpiAwarenessContext` 是每进程一次、且一旦设过
//! 就改不动的东西。放进 `src/capture.rs` 的单元测试里，就会随用例执行顺序去影响同一进程
//! 里那些真的抓屏的用例（这仓里"每进程一次"的形状害过并行全量红、单跑绿）。单独一个二进制
//! 里，这个进程只属于这条断言。
//!
//! 这条用例的存在理由：上一版探针写成 `GetWindowDpiAwarenessContext(GetDesktopWindow())`，
//! 问的是桌面窗口所在线程 → 一个已经是 Per-Monitor V2 的进程被报成"未启用"，而上层把
//! 这句话当硬失败用，结果六次截图六次被挡死。改成问当前线程之后，用"设之前/设之后"
//! 把探针真的按了一遍 —— 不然只是把一处没验证的判断换成另一处。

#![cfg(windows)]

use focusflow_core::capture::win;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

#[test]
fn dpi_probe_tracks_our_own_process_awareness() {
    // 这个测试二进制里没有 tao、也没有清单里的 dpiAware 设置，起点对不是 PMv2
    let before = win::dpi_awareness_text();
    assert_ne!(
        before, "per-monitor-v2",
        "起点就已经是 PMv2，说明夹具与假设不符（这个二进制没人设过感知）"
    );

    // 设成 PMv2：与 tao 在事件循环创建时做的是同一件事
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
        .expect("SetProcessDpiAwarenessContext 失败：本进程已经设过感知，或已被窗口锁定");

    // 探针必须立刻跟着变 —— 不回 PMv2 就是它没在问自己
    let after = win::dpi_awareness_text();
    assert_eq!(
        after, "per-monitor-v2",
        "设成 Per-Monitor V2 之后探针仍回 {after:?}：探针问错了对象"
    );
    assert!(win::is_per_monitor_v2());
}
