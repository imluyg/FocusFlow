// 贴图窗口：显示一次截图提交产出的那张 PNG，能拖、能滚轮缩放、能关。
//
// 四条口径（与 `pin.rs` 模块头同一套，别在这里另立）：
// 1. 图就是那次提交产出的**同一份** png（已经落盘、已经进过剪贴板）。这里不编辑、不保存，
//    所以窗口里不存在"没存下来的东西"，退出时也没什么可挽留的。
// 2. 100% 那一档必须**逐像素等于图**：CSS 尺寸 = 物理尺寸 × 缩放 / devicePixelRatio，
//    而不是"铺满视口"。WebView2 建控制器时会把窗口强制放宽到至少 120px（见 `state.rs`），
//    铺满就等于横向拉糊一张本来没失真的图 —— 而那正是"看着正常但不对"的一类。
//    窗口比图宽时多出来的是底色边。
// 3. 拖拽照悬浮窗那一套：`outerPosition()` 是**物理像素**而 `e.screenX` 是 **CSS 像素**，
//    必须乘 scale 再相加，否则 150% 屏上窗口只跟半程；高频 mousemove 用 rAF 合并成每帧一次 IPC。
// 4. 页面脚本挂了会怎样：窗口只剩灰底，但 **Alt+F4 与设置页的「关掉所有贴图」都不依赖这个页面**
//    （`pin.rs` 不拦 CloseRequested）。所以这里不再给"脚本异常"另开一条到 Rust 的通路 ——
//    那需要一个只为失败存在的新命令；先把原因画在窗口里，让人看得见。
import { invoke } from "./js/tauri.js";

const img = document.getElementById("img");
const badge = document.getElementById("badge");
const dead = document.getElementById("dead");
const closeBtn = document.getElementById("x");

const appWindow = window.__TAURI__.window.getCurrentWindow();
const PhysicalPosition = window.__TAURI__.window.PhysicalPosition;
const dpr = window.devicePixelRatio || 1;

/** 图像的物理尺寸（= 落盘那张 PNG 的像素尺寸），取到图之后才有值。 */
let imgSize = null;
/** 当前缩放倍数，只走 Rust 那张档位表（`pin.rs` 的 `ZOOM_STEPS`）。 */
let scale = 1.0;

/** 图该画多大（CSS 像素）。100% 时就是「物理尺寸 / dpr」，一个像素都不多不少。 */
export function cssSize(width, height, scale, dpr) {
  return { w: (width * scale) / dpr, h: (height * scale) / dpr };
}

/** 滚轮方向 → `pin_resize` 的 dir。往上推是放大，与看图习惯一致。 */
export function wheelDir(deltaY) {
  return deltaY < 0 ? 1 : deltaY > 0 ? -1 : 0;
}

function layout() {
  if (!imgSize) return;
  const s = cssSize(imgSize.w, imgSize.h, scale, dpr);
  img.style.width = s.w + "px";
  img.style.height = s.h + "px";
}

let badgeTimer = null;
function showBadge(text) {
  badge.textContent = text;
  badge.style.display = "block";
  if (badgeTimer) clearTimeout(badgeTimer);
  badgeTimer = setTimeout(() => {
    badge.style.display = "none";
  }, 1200);
}

function giveUp(reason) {
  dead.textContent = "这张贴图取不到图：" + reason + "（Esc 或 Alt+F4 关掉这个窗口）";
  document.body.classList.add("dead");
}

// ---------- 启动：只拉一次 ----------
// id 不从 URL 或参数来：Rust 侧按**调用方窗口的 label** 认人（`pin_take` 里写的），
// 这样别的窗口冒充不了。页面这边连自己的 id 都不需要知道。
async function boot() {
  let payload;
  try {
    payload = await invoke("pin_take");
  } catch (e) {
    giveUp(String(e));
    return;
  }
  imgSize = { w: payload.width, h: payload.height };
  layout();
  img.src = "data:image/png;base64," + payload.png_base64;
  img.addEventListener("error", () => giveUp("PNG 解码失败"));
  // 窗口与图的尺寸对不对**不在这里量**：Rust 侧 `apply_pin_size` 已经量过并出声
  // （比图小=会被切一块走 warn，比图大=WebView2 那 120px 地板、只多一条底色边走 debug）。
  // 页面再量一次要的是 `core:window:allow-inner-size`，而这条权限我没现量过 ——
  // 拿一条没验过的权限当守卫的前提，等于写一个静默不跑的假守卫。
}

// ---------- 关闭 ----------
async function closePin() {
  try {
    await invoke("pin_close");
  } catch (e) {
    // 关不掉（Rust 那边退成了隐藏）：至少把这句话说出来，别让人以为点坏了。
    giveUp(String(e));
  }
}
closeBtn.addEventListener("click", closePin);
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    e.preventDefault();
    closePin();
  }
});

// ---------- 拖拽 ----------
// 拖动状态在窗口隐藏/失焦时必须复位：隐藏期间收不到 mouseup，留着 isDown 的话
// 下一次鼠标一动就把窗口搬到一个陈旧基准上（悬浮窗那条注释写的就是这个坑）。
let isDown = false;
let drag = null;
let rafPending = false;
let pendingPos = null;

function resetDrag() {
  isDown = false;
  drag = null;
  pendingPos = null;
}

document.addEventListener("mousedown", async (e) => {
  if (e.button !== 0) return;
  isDown = true;
  drag = null;
  try {
    const pos = await appWindow.outerPosition();
    const factor = (await appWindow.scaleFactor()) || 1;
    if (!isDown) return; // await 期间已经松手：这次基准作废，别用它
    drag = { startX: e.screenX, startY: e.screenY, winX: pos.x, winY: pos.y, scale: factor };
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
  if (!isDown || !drag) return;
  const dx = e.screenX - drag.startX;
  const dy = e.screenY - drag.startY;
  // 阈值按逻辑像素判：高缩放下不至于刚动一下就跳
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
  }
});
window.addEventListener("blur", resetDrag);
document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "hidden") resetDrag();
});

// ---------- 缩放 ----------
// 尺寸由 Rust 算（档位表在那边），这里只报方向。窗口改完尺寸要重排一次图的 CSS 尺寸。
document.addEventListener(
  "wheel",
  async (e) => {
    e.preventDefault();
    if (!imgSize) return;
    const dir = wheelDir(e.deltaY);
    if (dir === 0) return;
    try {
      const [applied] = await invoke("pin_resize", { scale, dir });
      scale = applied;
      layout();
      showBadge(Math.round(scale * 100) + "%");
    } catch (err) {
      showBadge("缩放失败：" + err);
    }
  },
  { passive: false }
);

// ---------- 别把贴图做成浏览器 ----------
// 右键菜单（含"图片另存为/重新加载"）与 Ctrl+滚轮的页面缩放都不该出现在贴图上；
// 图本身也不许被原生拖走（我们自己的 mousedown 才是拖窗口）。
document.addEventListener("contextmenu", (e) => e.preventDefault());
document.addEventListener("dragstart", (e) => e.preventDefault());
window.addEventListener("error", (e) => giveUp(e.message || "页面脚本异常"));
window.addEventListener("unhandledrejection", (e) =>
  giveUp(String((e.reason && e.reason.message) || e.reason))
);

boot();
