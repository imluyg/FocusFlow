// 通用工具与常量

export const WD = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];

export function $(id) {
  return document.getElementById(id);
}

export function fmt(n) {
  return Number(n || 0).toLocaleString("zh-CN");
}

export function escapeHtml(s) {
  return String(s == null ? "" : s)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}
