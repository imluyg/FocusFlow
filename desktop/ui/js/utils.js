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

// 轻提示（替代阻塞式 alert）：顶部浮层 3 秒自动消失
export function toast(msg) {
  let el = document.getElementById("toast");
  if (!el) {
    el = document.createElement("div");
    el.id = "toast";
    document.body.appendChild(el);
  }
  el.textContent = msg;
  el.classList.add("show");
  clearTimeout(toast._timer);
  toast._timer = setTimeout(() => el.classList.remove("show"), 3000);
}
