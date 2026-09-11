// 悬浮窗逻辑：事件订阅统计、手动拖动、位置持久化、双击打开主界面
import { invoke, listen } from "./js/tauri.js";
import { fmt } from "./js/utils.js";

const appWindow = window.__TAURI__.window.getCurrentWindow();
const PhysicalPosition = window.__TAURI__.window.PhysicalPosition;

// ===== 手动拖动 =====
let isDown = false;
let drag = null;

document.addEventListener("mousedown", async (e) => {
  if (e.button !== 0) return;
  isDown = true;
  try {
    const pos = await appWindow.outerPosition();
    drag = { startX: e.screenX, startY: e.screenY, winX: pos.x, winY: pos.y };
  } catch (err) {
    drag = null;
  }
});

document.addEventListener("mousemove", (e) => {
  if (!isDown) return;
  if (!drag) return;
  const dx = e.screenX - drag.startX;
  const dy = e.screenY - drag.startY;
  if (Math.abs(dx) < 3 && Math.abs(dy) < 3) return;
  appWindow
    .setPosition(new PhysicalPosition(drag.winX + dx, drag.winY + dy))
    .catch(() => {});
});

document.addEventListener("mouseup", () => {
  if (isDown) {
    isDown = false;
    persistPosition();
  }
});

// 窗口隐藏/失焦时复位拖动状态：隐藏期间收不到 mouseup，若残留 isDown，
// 重开后 mousemove 会用陈旧坐标把窗口搬走（跳位）
function resetDrag() {
  isDown = false;
  drag = null;
}
document.addEventListener("visibilitychange", () => {
  if (document.hidden) resetDrag();
});
window.addEventListener("blur", () => resetDrag());

// 双击打开主界面
document.addEventListener("dblclick", () => {
  invoke("show_main");
});

// 禁用 WebView2 默认右键菜单（另存为等）
document.addEventListener("contextmenu", (e) => e.preventDefault());

// ===== 位置持久化（仅拖动结束时写入，避免定时轮询）=====
let lastSaved = { x: null, y: null };
async function persistPosition() {
  try {
    const pos = await appWindow.outerPosition();
    const scale = (await appWindow.scaleFactor()) || 1;
    let px = Math.round(pos.x / scale);
    let py = Math.round(pos.y / scale);
    if (lastSaved.x === px && lastSaved.y === py) return;
    lastSaved = { x: px, y: py };
    await invoke("set_config", { section: "floating", key: "pos_x", value: String(px) });
    await invoke("set_config", { section: "floating", key: "pos_y", value: String(py) });
  } catch (e) {}
}

// ===== 数据 =====
function apply(s) {
  document.getElementById("today").textContent = fmt(s.today_count);
  document.getElementById("cpm").textContent = String(s.cpm || 0);
}

(async () => {
  try {
    const dark = (await invoke("get_config", { section: "gui", key: "theme" })) === "dark";
    document.body.classList.toggle("dark", dark);
  } catch (e) {}
  try {
    await listen("stats-live", (e) => apply(e.payload));
  } catch (e) {}
  try {
    apply(await invoke("get_live"));
  } catch (e) {}
})();
