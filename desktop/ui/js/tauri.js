// Tauri API 封装（withGlobalTauri 注入的全局对象）
export const invoke = window.__TAURI__.core.invoke;
export const listen = window.__TAURI__.event.listen;
