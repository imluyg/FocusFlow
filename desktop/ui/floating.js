// 悬浮窗逻辑：事件订阅统计、手动拖动、位置持久化、双击打开主界面
import { invoke, listen } from "./js/tauri.js";
import { fmt, fmtDuration } from "./js/utils.js";

const appWindow = window.__TAURI__.window.getCurrentWindow();
const PhysicalPosition = window.__TAURI__.window.PhysicalPosition;

// ===== 手动拖动 =====
// 高频 mousemove 用 requestAnimationFrame 合并：每帧最多一次 setPosition IPC，
// 快速拖动下明显减少跨进程调用（视觉上无差别）。
//
// 单位约定（踩过的坑）：`outerPosition()` 返回**物理像素**，而 `e.screenX` 是
// **CSS/逻辑像素**。两者必须经 `scaleFactor` 换算后再相加，否则在 125%/150%
// 缩放下窗口位移只有鼠标位移的 1/scale（拖动明显落后于鼠标）。
let isDown = false;
let drag = null;
let rafPending = false;
let pendingPos = null;

document.addEventListener("mousedown", async (e) => {
  if (e.button !== 0) return;
  isDown = true;
  drag = null; // 清掉上一次拖动，await 期间不再用陈旧基准
  try {
    const pos = await appWindow.outerPosition();
    const scale = (await appWindow.scaleFactor()) || 1;
    if (!isDown) return; // await 期间已松开：丢弃本次基准
    drag = {
      startX: e.screenX,
      startY: e.screenY,
      winX: pos.x,
      winY: pos.y,
      scale,
    };
  } catch (err) {
    drag = null;
  }
});

function flushPosition() {
  rafPending = false;
  if (!pendingPos) return;
  const p = pendingPos;
  pendingPos = null;
  appWindow.setPosition(new PhysicalPosition(p.x, p.y)).catch(() => {});
}

document.addEventListener("mousemove", (e) => {
  if (!isDown) return;
  if (!drag) return;
  // 逻辑像素位移 → 物理像素位移
  const dx = e.screenX - drag.startX;
  const dy = e.screenY - drag.startY;
  // 阈值按逻辑像素判断，避免高缩放下拖动一开始就跳动
  if (Math.abs(dx) < 3 && Math.abs(dy) < 3) return;
  pendingPos = {
    x: Math.round(drag.winX + dx * drag.scale),
    y: Math.round(drag.winY + dy * drag.scale),
  };
  if (!rafPending) {
    rafPending = true;
    requestAnimationFrame(flushPosition);
  }
});

document.addEventListener("mouseup", () => {
  if (isDown) {
    isDown = false;
    flushPosition();
    persistPosition();
  }
});

// 窗口隐藏/失焦时复位拖动状态：隐藏期间收不到 mouseup，若残留 isDown，
// 重开后 mousemove 会用陈旧坐标把窗口搬走（跳位）
function resetDrag() {
  isDown = false;
  drag = null;
  pendingPos = null;
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
// 显示口径（config gui.floating_metric）："times"=按键次数（默认），
// "duration"=活跃时长，"both"=两者都显示
let metric = "times";

function apply(s) {
  const durationEl = document.getElementById("duration");
  if (metric === "duration") {
    document.getElementById("today").textContent = fmtDuration(s.active_seconds);
    durationEl.textContent = fmtDuration(s.active_seconds);
  } else {
    document.getElementById("today").textContent = fmt(s.today_count);
    durationEl.textContent = fmtDuration(s.active_seconds);
  }
  const durRow = document.getElementById("row-duration");
  if (durRow) durRow.style.display = metric === "both" ? "" : "none";
  document.getElementById("cpm").textContent = String(s.cpm || 0);
}

(async () => {
  try {
    const m = await invoke("get_config", { section: "gui", key: "floating_metric" });
    if (m === "duration" || m === "both") metric = m;
  } catch (e) {}
  try {
    const dark = (await invoke("get_config", { section: "gui", key: "theme" })) === "dark";
    document.body.classList.toggle("dark", dark);
  } catch (e) {}
  try {
    await listen("stats-live", (e) => apply(e.payload));
  } catch (e) {}
  // 主题跟随主窗口：设置页切暗色时广播 theme-changed，悬浮窗实时切换（原需重启）
  try {
    await listen("theme-changed", (e) => {
      document.body.classList.toggle("dark", !!e.payload);
    });
  } catch (e) {}
  // 暂停态反馈：降透明度 + "已暂停"角标（此前暂停时悬浮窗毫无变化）
  try {
    document.body.classList.toggle("paused", !!(await invoke("is_paused")));
  } catch (e) {}
  try {
    await listen("pause-changed", (e) => {
      document.body.classList.toggle("paused", !!e.payload);
    });
  } catch (e) {}
  try {
    apply(await invoke("get_live"));
  } catch (e) {}
})();
