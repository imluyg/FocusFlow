// 截图覆盖层：拉取冻结的整屏底图 → 拖框选区 → 提交（存盘 + 剪贴板）或取消。
//
// 三条口径说明（都在这里，别在调用方各写一遍）：
// 1. 坐标一律用**相对视口的 CSS 像素**（clientX/clientY）。窗口被 Rust 侧摆成恰好等于
//    那块屏，所以 CSS 原点就是那块屏的原点，副屏的负坐标由 Rust 侧的缓冲区原点消化；
//    换算成物理像素由 Rust 做（`capture::css_rect_to_physical`），这里只乘一次 dpr 是为了
//    **显示**尺寸标签，不参与提交值。
// 2. 提交只做一次：`submitted` 置上之后所有输入忽略。连按两次鼠标会产出两张图，
//    而这个手势的定义就是"一次框选一张"。
// 3. 缩放倍数在这里先和 Rust 认定的值对账，对不上就不提交 —— 裁偏的图看着完全正常，
//    是最难被发现的一种坏。
// 4. **单击 = 选中光标下那个窗口**（Rust 给的 z-order 里第一个包住光标的），
//    拖框 = 自由矩形，松手即完成这点不变。原来"单击 = 取消"仍然成立，只是变成
//    "没有候选时才取消" —— 取消还有 Esc 与右键两条路，都不受影响。
//    清单是冻结那一刻定下来的，所以覆盖层自己不会在里面（它那时还没显示，
//    而且 Rust 侧按进程号排掉了自己进程的窗口）。
import { invoke, listen } from "./js/tauri.js";

const img = document.getElementById("shot");
const sel = document.getElementById("sel");
const snapEl = document.getElementById("snap");
const sizeEl = document.getElementById("size");
const hint = document.getElementById("hint");

// 按下到抬起的位移小于它就当作"单击"（沿用原来那个"没框住任何东西"的判据，
// 只是现在单击不再是取消，而是选中光标下那个窗口 —— 没有候选时才仍是取消）。
const CLICK_SLACK = 2;

const dpr = window.devicePixelRatio || 1;
let epoch = 0;
let monitor = null;
/** 可吸附窗口，已换算成 CSS 像素、相对视口；顺序是 z-order 顶→底（Rust 侧 EnumWindows 给的）。 */
let wins = [];
let submitted = false;
let pulling = false;
let started = false;
let anchor = null;
let box = null;

function setHint(text, isError) {
  hint.textContent = text;
  hint.classList.toggle("error", !!isError);
}

/** 放弃这次截图：不落文件、不动剪贴板，Rust 侧负责关窗并恢复悬浮窗。
 *  reason 会进日志——页面上的失败若在 Rust 侧不可见，这里就只剩一条 8 秒超时可查。 */
function giveUp(reason) {
  if (submitted) return;
  submitted = true;
  if (reason) setHint(reason, true);
  invoke("snip_cancel", { reason: reason || "" }).catch(() => {});
}

// 脚本抛异常一律报给 Rust 再取消。不加这一条，模块里任何一处抛错都是"黑屏盖 8 秒然后自己关掉"，
// 日志里连一个字都没有（这两轮就是靠它才定位到页面侧的问题）。
window.addEventListener("error", (e) => giveUp("页面脚本异常：" + (e.message || "未知")));
window.addEventListener("unhandledrejection", (e) =>
  giveUp("页面异步失败：" + String((e.reason && e.reason.message) || e.reason))
);

/** 画选区并同步尺寸标签。参数是 CSS 像素、相对视口。 */
function applyBox(x, y, w, h) {
  box = { x, y, w, h };
  sel.style.left = x + "px";
  sel.style.top = y + "px";
  sel.style.width = w + "px";
  sel.style.height = h + "px";
  sel.style.display = "block";
  document.body.classList.add("has-sel");
  // 标签给人看的是最终文件的像素尺寸，所以这里才乘 dpr
  sizeEl.textContent = `${Math.round(w * dpr)} × ${Math.round(h * dpr)}`;
  sizeEl.style.display = "block";
  sizeEl.style.left = x + "px";
  sizeEl.style.top = Math.max(0, y - 22) + "px";
}

function showBox(a, b) {
  applyBox(
    Math.min(a.x, b.x),
    Math.min(a.y, b.y),
    Math.abs(b.x - a.x),
    Math.abs(b.y - a.y)
  );
}

function point(e) {
  return { x: e.clientX, y: e.clientY };
}

/** 光标下最上面那个可吸附窗口。`wins` 已经是 z-order 顶→底，所以 find 就是"看到的那个"。 */
function snapAt(p) {
  return (
    wins.find((w) => p.x >= w.x && p.x < w.x + w.w && p.y >= w.y && p.y < w.y + w.h) || null
  );
}

/** 高亮吸附候选。轮廓亮着 = 这一下点下去会选中整个窗口；不亮 = 这一下是取消。 */
function showSnap(r) {
  // 揭开压暗也挂在这个开关上：不揭开的话"要点的是哪一块"看不清
  document.body.classList.toggle("has-snap", !!r);
  if (!r) {
    snapEl.style.display = "none";
    return;
  }
  snapEl.style.left = r.x + "px";
  snapEl.style.top = r.y + "px";
  snapEl.style.width = r.w + "px";
  snapEl.style.height = r.h + "px";
  snapEl.style.display = "block";
}

/** 提交唯一入口：`submitted` 这道闸只在这里落下（一次手势一张图）。 */
function doCommit() {
  if (!box) return;
  submitted = true;
  sel.style.cursor = "progress";
  invoke("snip_commit", {
    sel: {
      x: box.x,
      y: box.y,
      w: box.w,
      h: box.h,
      dpr,
      epoch,
      ...viewportReport(),
    },
  }).catch((err) => {
    // 提交失败要留在覆盖层上：底图还在 Rust 侧的会话里，重新框一次就能再交。
    submitted = false;
    setHint("截图失败：" + err + "（重新框选或按 Esc）", true);
  });
}

document.addEventListener("mousedown", (e) => {
  if (submitted || !started) return;
  if (e.button === 2) return; // 右键交给 contextmenu 处理成"取消"
  if (e.button !== 0) return;
  anchor = point(e);
  document.body.classList.add("dragging");
  showSnap(null);
});

document.addEventListener("mousemove", (e) => {
  if (submitted || !started) return;
  if (anchor) {
    showBox(anchor, point(e));
    return;
  }
  showSnap(snapAt(point(e)));
});

document.addEventListener("mouseup", (e) => {
  if (!anchor || submitted) return;
  const a = anchor;
  const end = point(e);
  anchor = null;
  document.body.classList.remove("dragging");
  if (Math.abs(end.x - a.x) < CLICK_SLACK || Math.abs(end.y - a.y) < CLICK_SLACK) {
    // 单击：光标下有候选就选中整个窗口，没有才仍是"取消"（原来的行为）
    const r = snapAt(end);
    showSnap(null);
    if (!r) {
      giveUp("");
      return;
    }
    applyBox(r.x, r.y, r.w, r.h);
    doCommit();
    return;
  }
  showBox(a, end);
  doCommit();
});

document.addEventListener("contextmenu", (e) => {
  e.preventDefault();
  giveUp("");
});

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    e.preventDefault();
    giveUp("");
  }
});

// 底图尺寸与"物理像素 / dpr"的差只**记录**，不再否决截图。
// 理由：选区换算走的是 dpr，跟这块布局宽度无关；上一版把它当硬失败用，
// 1.3% 的窗口取整差就把整条功能判死（守卫连着坑过两次，这次学乖）。
// 数值随提交一起回报，Rust 侧差得离谱（>2%）会 WARN 一行，看得见但不拦你。
function viewportReport() {
  return {
    viewport_w: document.documentElement.clientWidth,
    viewport_h: document.documentElement.clientHeight,
  };
}

// 拉一次底图。两条路都会走到这里：
// - 首次创建：窗口刚建好，页面跑启动脚本时会话已经在 Rust 的槽里，直接拉得到；
// - 之后每次：页面早就加载完了，由 Rust 广播 `snip-ready` 叫它重新拉。
// `pulling` 是防重入：底图取走一次就没了，第二次拉必然失败，不能把那次失败当真。
async function pull() {
  if (pulling) return;
  pulling = true;
  try {
    // 先放开上一张留下的闸门，再谈别的：`submitted` 是"这次手势只提交一张"的闸，
    // 而复用窗口时新会话开始前它还留着 true —— 挡在守卫里的话页面就再也不取图了
    // （夹具测出来的正是这条：第二次截图只剩 8 秒看门狗超时）。
    submitted = false;
    anchor = null;
    box = null;
    sel.style.display = "none";
    sizeEl.style.display = "none";
    showSnap(null);
    document.body.classList.remove("has-sel", "dragging", "has-snap");
    img.style.visibility = "hidden";

    let payload;
    try {
      payload = await invoke("snip_take");
    } catch (err) {
      giveUp("取不到截图底图：" + err);
      return;
    }
    epoch = payload.epoch;
    monitor = payload.monitor;
    // Rust 给的是物理像素 + 这块屏的绝对坐标；换成页面一直在用的"CSS 像素、相对视口"。
    // 副屏的负原点与 dpr≠1 都在这一步一起消化，后面的命中判定才是纯 CSS 坐标。
    wins = payload.windows.map((w) => ({
      x: (w.x - monitor.x) / dpr,
      y: (w.y - monitor.y) / dpr,
      w: w.width / dpr,
      h: w.height / dpr,
    }));
    if (Math.abs(dpr - payload.dpr) > 0.01) {
      giveUp(`缩放倍数对不上（页面 ${dpr}，程序 ${payload.dpr}），已取消`);
      return;
    }
    img.src = "data:image/png;base64," + payload.png_base64;
    started = true;
    setHint("拖框自由选区 · 单击选中整个窗口 · Esc / 右键取消");
  } finally {
    pulling = false;
  }
}

(async () => {
  img.addEventListener("error", () => giveUp("底图解码失败，已取消"));
  img.addEventListener("load", () => {
    img.style.visibility = "visible";
  });
  try {
    await listen("snip-ready", () => {
      pull();
    });
  } catch (err) {
    setHint("监听截图通知失败：" + err, true);
  }
  await pull();
})();
