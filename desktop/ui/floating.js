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

// ===== 卡片量自己的排版，把高度交给 Rust =====
// 为什么不让 Rust 自己算完：WebView2 这一侧的版面是浏览器定的。现量 `metric=both` 时
// 「时长」那个值在 90 宽里折成两行，真实需要 94 而 Rust 那把"一行 18 像素"的尺子只给 82，
// 最后那一行照样被窗口下边缘裁掉。报的是**卡片内容的高度** —— 卡片自己是 height:auto，
// 与窗口多高无关，所以"改完尺寸再量一次"不会来回弹。
let lastReportedH = 0;
async function syncCardHeight() {
  const card = document.querySelector(".card");
  if (!card) return 0;
  const h = Math.ceil(card.getBoundingClientRect().height);
  if (h === lastReportedH) return h;
  lastReportedH = h;
  try {
    return await invoke("report_floating_height", { height: h });
  } catch (e) {
    // IPC 没成本次就没长高（与这条存在之前的样子一样）。把去重的那格清掉，
    // 下一次版面变化还会再报 —— 留着它反而永远不会再试。
    lastReportedH = 0;
    return 0;
  }
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
  // 久坐提醒（[rest]，判定在 core 的 RestMonitor）：悬浮窗没有主界面那条 toast，
  // 自己画一条会自动消失的横条 —— 悬浮窗本来就是"不看主窗口也想看数"的入口。
  try {
    await listen("rest-reminder", (e) => {
      const n = e.payload || {};
      let el = document.getElementById("rest-notice");
      if (!el) {
        el = document.createElement("div");
        el.id = "rest-notice";
        el.setAttribute("role", "status");
        // 版面里的一行，不是覆盖层。过去这条是 position:fixed;top:0，直接压在「活跃」那一行上
        // （真浏览器现量：与那一行重叠 17.55px，也就是整个 20 秒里主数字被盖住）。卡片本身是
        // overflow:hidden 且铺满窗口，既压不住也挤不下去 ⇒ 它只能占自己的一行，
        // 由 Rust 那边按同一份公式把窗口加长（floating::webview_pill_size）。
        // ⚠ height 这一个数必须与 desktop/src/floating/render.rs 的 ROW_H 一致，
        //   有一条判据回头读这两个文件对拍；改这里不改那边，多出来的那一行就还是被裁。
        el.style.cssText =
          "flex:none;height:18px;line-height:18px;font-size:11px;margin:0 -8px;" +
          "text-align:center;overflow:hidden;white-space:nowrap;" +
          "background:rgba(220,120,30,.92);color:#fff;border-radius:6px;";
        const card = document.querySelector(".card");
        card.insertBefore(el, card.firstChild);
      }
      el.textContent = `久坐提醒：已连用 ${Number(n.window_minutes) || 0} 分钟，起来活动 ${Number(n.rest_seconds) || 0} 秒`;
      el.style.display = "";
      // 横带占的是版面里的一行 ⇒ 卡片高了 18 像素，立刻报一次让窗口跟上
      syncCardHeight();
      clearTimeout(el._hide);
      // 存活时长由 core 那一格说一遍（`RestNotice.shown_seconds` = 建议时长，带可读下限）：
      // 原先这里写死 20 秒，于是 `[rest] rest_seconds = 60` 时条上念着"活动 60 秒"、
      // 20 秒就消失 —— 同一句话两处尺子。兜底的 20 只给缺这一格的老载荷用。
      const shown = Number(n.shown_seconds) > 0 ? Number(n.shown_seconds) : 20;
      el._hide = setTimeout(() => {
        el.style.display = "none";
        // 那一行还回去了 ⇒ 也要报一次，否则窗口停在长高后的尺寸空一截
        syncCardHeight();
      }, shown * 1000);
    });
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
  // 版面定下来之后报一次（`metric=both` 的第三行与折行在这一刻才量得准）
  await syncCardHeight();
})();
