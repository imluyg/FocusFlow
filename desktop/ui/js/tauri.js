// Tauri API 封装（withGlobalTauri 注入的全局对象）
export const invoke = window.__TAURI__.core.invoke;
export const listen = window.__TAURI__.event.listen;
// 全局广播（所有窗口都能收到）：用于主窗口 → 悬浮窗的主题/状态同步
export const emit = window.__TAURI__.event.emit;
