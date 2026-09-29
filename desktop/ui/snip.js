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
// 5. **标注模式（`snip_annotate` 那条热键）只改"松手之后去哪"**：`annotate` 为真时，
//    吸附选中与拖框松手都只确定选区、进标注态，只有 Enter 或点「完成」才提交；
//    `annotate` 为假时上面第 2、4 条一个字节都不变。两种手势共用同一份底图会话，
//    所以区分点只有 payload 里那一个布尔 —— 别在这里再猜"用户是不是想标注"。
// 6. 标注层的位图尺寸就是 Rust 裁出来的那块**物理像素**，画笔坐标与选区用同一条
//    `round(v*dpr)` 规则换算（见 `cssRectToPhysical` 上面那段说明）。Rust 那边尺寸不
//    严格相等就拒收、绝不缩放，所以这里的镜像算错一格，提交就会响而不是悄悄裁偏。
// 7. 文字与马赛克**共用同一条回传管道**（都画在那一层 canvas 上、都走 toDataURL →
//    Rust 解码 → 合成），所以 Rust 侧不为它们加任何概念：文字用 `fillText`，字号是
//    物理像素；马赛克从冻结的底图取子块「缩小再放大」，缩小那趟要平滑（要的就是平均
//    掉的糊），放大那趟关平滑（不然糊成一片灰、等于白打码）。
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

// ---------- 标注态（annotate 模式才有意义） ----------

const annotCanvas = document.getElementById("annot");
const toolsEl = document.getElementById("tools");
const txtEl = document.getElementById("txt");
const actx = annotCanvas.getContext("2d");
/** 马赛克的两步缩放要一块临时画布，借一条而不是每笔新建一块。 */
const scratch = document.createElement("canvas");
const sctx = scratch.getContext("2d");
/** 与 `snip.html` 里 body 那串同一个字体：文字笔不许再引第二种字体。 */
const FONT_STACK = 'system-ui, "Segoe UI", "Microsoft YaHei", sans-serif';
/** 马赛克一格多大（物理像素）。固定值：同一块区域每次打码结果一样，撤销重绘才不会漂。 */
const MOSAIC_CELL = 10;

/** 这一张是不是 `snip_annotate` 进来的（跟会话走，由 payload 给）。 */
let annotate = false;
/** 已经选好区、正在画。为真时拖框与吸附那两条手势全部让路。 */
let annotating = false;
/** 正在用输入框打一段字：这时的 Enter/Esc 归输入框，不能被"提交/放弃"那套抢走。 */
let typing = false;
let pendingAt = null;
/** 底图解码完了没有 —— 马赛克要从那张真像素上取子块，没解码完画出来是空的。 */
let imgDecoded = false;
/** 已完成的笔。撤销 = pop 一支然后整层重绘（不做真 undo 栈）。 */
let ops = [];
/** 正在拖的那一支（还没进 ops）。 */
let live = null;
/** 选区的物理矩形：标注层画布的原点，也是"页面所见 = 落盘像素"的那个原点。 */
let selPhys = null;
let tool = "rect";
let penColor = "#ff2d2d";
let penWidth = 3;
/** 字号（物理像素），三档：小/中/大。 */
let textSize = 30;

/** CSS 选区 → 屏内相对物理矩形。与 Rust 的 `capture::css_rect_to_physical` 同一条规则：
 *  先归一化反向拖框、一律 round、再钳到屏内（先角点后宽高，反过来会各吃掉一像素）。
 *  这份镜像是唯一一处不得不在 JS 里重算的地方，所以导出给夹具：那组分数/越界矩形会
 *  同时喂给两边逐条对拍（Rust 侧收货条件是严格相等，不一致就是提交被拒而不是悄悄裁偏）。 */
export function cssRectToPhysical(r, dpr, monitor) {
  const left = r.w < 0 ? r.x + r.w : r.x;
  const top = r.h < 0 ? r.y + r.h : r.y;
  const w = Math.abs(r.w);
  const h = Math.abs(r.h);
  if (!(w >= 1) || !(h >= 1)) return null;
  const px = (v) => Math.round(v * dpr);
  const clamp = (v, lo, hi) => Math.min(Math.max(v, lo), hi);
  const x = clamp(px(left), 0, monitor.width - 1);
  const y = clamp(px(top), 0, monitor.height - 1);
  return {
    x,
    y,
    w: clamp(px(w), 1, monitor.width - x),
    h: clamp(px(h), 1, monitor.height - y),
  };
}

/** 客户端 CSS 坐标 → 画布里的物理格（画布原点就是选区的物理原点）。 */
function toCanvasPx(p) {
  return { x: Math.round(p.x * dpr) - selPhys.x, y: Math.round(p.y * dpr) - selPhys.y };
}

function newOp(p) {
  const q = toCanvasPx(p);
  return tool === "pen"
    ? { t: "pen", color: penColor, w: penWidth, pts: [q] }
    : { t: tool, color: penColor, w: penWidth, a: q, b: q };
}

function updateOp(op, p) {
  const q = toCanvasPx(p);
  if (op.t === "pen") op.pts.push(q);
  else op.b = q;
}

function drawOp(op) {
  // 这两支不描边，先分流出去：它们要的是"从底图取像素"和"排字"，与描线的状态无关
  if (op.t === "mosaic") {
    drawMosaic(op);
    return;
  }
  if (op.t === "text") {
    drawText(op);
    return;
  }
  actx.strokeStyle = op.color;
  actx.fillStyle = op.color;
  actx.lineWidth = op.w;
  actx.lineCap = "round";
  actx.lineJoin = "round";
  if (op.t === "rect") {
    const x = Math.min(op.a.x, op.b.x);
    const y = Math.min(op.a.y, op.b.y);
    actx.strokeRect(x, y, Math.abs(op.b.x - op.a.x), Math.abs(op.b.y - op.a.y));
    return;
  }
  if (op.t === "arrow") {
    actx.beginPath();
    actx.moveTo(op.a.x, op.a.y);
    actx.lineTo(op.b.x, op.b.y);
    actx.stroke();
    const ang = Math.atan2(op.b.y - op.a.y, op.b.x - op.a.x);
    const head = Math.max(10, op.w * 4);
    actx.beginPath();
    actx.moveTo(op.b.x, op.b.y);
    actx.lineTo(op.b.x - head * Math.cos(ang - Math.PI / 7), op.b.y - head * Math.sin(ang - Math.PI / 7));
    actx.lineTo(op.b.x - head * Math.cos(ang + Math.PI / 7), op.b.y - head * Math.sin(ang + Math.PI / 7));
    actx.closePath();
    actx.fill();
    return;
  }
  actx.beginPath();
  actx.moveTo(op.pts[0].x, op.pts[0].y);
  for (const p of op.pts) actx.lineTo(p.x, p.y);
  actx.stroke();
}

/** 把选区内的某一格区域按物理像素映射回底图的 intrinsic 像素。
 *  正常路径上比值恒等于 1（底图就是那块屏的原生像素），带上它是因为哪天底图被别处
 *  缩放过，也要让马赛克盖在该盖的位置上，而不是悄悄错开一格。 */
function baseOf(p) {
  const k = img.naturalWidth && monitor.width ? img.naturalWidth / monitor.width : 1;
  return { x: (selPhys.x + p.x) * k, y: (selPhys.y + p.y) * k, k };
}

function drawMosaic(op) {
  const x = Math.round(Math.min(op.a.x, op.b.x));
  const y = Math.round(Math.min(op.a.y, op.b.y));
  const w = Math.round(Math.abs(op.b.x - op.a.x));
  const h = Math.round(Math.abs(op.b.y - op.a.y));
  if (w < 2 || h < 2) return;
  const src = baseOf({ x, y });
  const sw = Math.max(1, Math.ceil(w / MOSAIC_CELL));
  const sh = Math.max(1, Math.ceil(h / MOSAIC_CELL));
  scratch.width = sw;
  scratch.height = sh;
  // 缩小这一趟**要**平滑：把一格平均成一个颜色才叫糊
  sctx.imageSmoothingEnabled = true;
  sctx.clearRect(0, 0, sw, sh);
  sctx.drawImage(img, src.x, src.y, w * src.k, h * src.k, 0, 0, sw, sh);
  actx.save();
  // 放大这一趟**关**平滑：开着就会糊成一片均匀的灰，等于白打码
  actx.imageSmoothingEnabled = false;
  actx.drawImage(scratch, 0, 0, sw, sh, x, y, w, h);
  actx.restore();
}

function drawText(op) {
  actx.save();
  actx.fillStyle = op.color;
  actx.textBaseline = "top";
  actx.font = `${op.size}px ${FONT_STACK}`;
  actx.fillText(op.text, op.at.x, op.at.y);
  actx.restore();
}

/** 整层重绘：笔只有 ops 这一个真相，撤销与改线宽都走同一条路。 */
function redraw() {
  actx.clearRect(0, 0, annotCanvas.width, annotCanvas.height);
  for (const op of ops) drawOp(op);
  if (live) drawOp(live);
}

function undoOp() {
  ops.pop();
  live = null;
  redraw();
}

/** 文字笔的起点：把输入框摆在那一格上，它自己就是预览。
 *  字号按 dpr 除掉换成 CSS 像素，屏幕上看着多大、落到物理像素上就多大。 */
function startTyping(p) {
  if (typing) finishTyping(true);
  pendingAt = toCanvasPx(p);
  typing = true;
  document.body.classList.add("typing");
  txtEl.value = "";
  txtEl.style.color = penColor;
  txtEl.style.fontFamily = FONT_STACK;
  txtEl.style.fontSize = textSize / dpr + "px";
  txtEl.style.left = (selPhys.x + pendingAt.x) / dpr + "px";
  txtEl.style.top = (selPhys.y + pendingAt.y) / dpr + "px";
  txtEl.focus();
}

/** 收一段字。keep=false 是"打字中按 Esc"——只丢这段字，不取消整张截图。 */
function finishTyping(keep) {
  if (!typing) return;
  typing = false;
  document.body.classList.remove("typing");
  const s = txtEl.value.trim();
  txtEl.value = "";
  if (keep && s && pendingAt) {
    ops.push({ t: "text", text: s, at: pendingAt, color: penColor, size: textSize });
    redraw();
  }
  pendingAt = null;
}

// 打字期间这三个键归输入框：全局那套 Enter=提交 / Esc=放弃 / Backspace=撤销
// 会把"正在打的字"当成手势吃掉，所以在这里截住，不让它冒到 document。
txtEl.addEventListener("keydown", (e) => {
  e.stopPropagation();
  if (e.key === "Enter") {
    e.preventDefault();
    finishTyping(true);
  } else if (e.key === "Escape") {
    e.preventDefault();
    finishTyping(false);
  }
});
txtEl.addEventListener("blur", () => finishTyping(true));
txtEl.addEventListener("mousedown", (e) => e.stopPropagation());

/** 工具条摆在选区下沿；放不下就翻到上沿（选区贴屏幕底部是常态）。 */
function placeTools() {
  const r = annotCanvas.getBoundingClientRect();
  toolsEl.style.left = Math.max(4, r.left) + "px";
  const below = r.bottom + 6;
  const h = toolsEl.offsetHeight;
  const top = below + h > window.innerHeight ? Math.max(4, r.top - h - 6) : below;
  toolsEl.style.top = top + "px";
}

function enterAnnotate() {
  selPhys = cssRectToPhysical(box, dpr, monitor);
  if (!selPhys) {
    giveUp("选区不足 1 像素，什么都没框住");
    return;
  }
  annotCanvas.width = selPhys.w;
  annotCanvas.height = selPhys.h;
  annotCanvas.style.left = selPhys.x / dpr + "px";
  annotCanvas.style.top = selPhys.y / dpr + "px";
  annotCanvas.style.width = selPhys.w / dpr + "px";
  annotCanvas.style.height = selPhys.h / dpr + "px";
  ops = [];
  live = null;
  annotating = true;
  document.body.classList.add("annotating");
  redraw();
  placeTools();
  syncTools();
  setHint("画一笔 · Enter 或点「完成」提交 · Backspace 撤销 · Esc 放弃");
}

/** 退出并清空标注态。复用覆盖层时必须走这里：上一张的笔漏给下一张就是画错图。 */
function exitAnnotate() {
  finishTyping(false);
  annotating = false;
  live = null;
  ops = [];
  selPhys = null;
  document.body.classList.remove("annotating");
  actx.clearRect(0, 0, annotCanvas.width, annotCanvas.height);
  annotCanvas.width = 0;
  annotCanvas.height = 0;
}

/** 标注层 PNG 的 base64；一笔都没画就交 null（走原路，与加这个字段之前逐字节一致）。 */
function layerBase64() {
  if (!annotating || ops.length === 0) return null;
  const url = annotCanvas.toDataURL("image/png");
  return url.slice(url.indexOf(",") + 1);
}

function syncTools() {
  const on = (el, want) => el.classList.toggle("on", want);
  for (const b of toolsEl.querySelectorAll("button[data-tool]")) on(b, b.dataset.tool === tool);
  for (const b of toolsEl.querySelectorAll("button[data-color]")) on(b, b.dataset.color === penColor);
  for (const b of toolsEl.querySelectorAll("button[data-w]")) on(b, Number(b.dataset.w) === penWidth);
  for (const b of toolsEl.querySelectorAll("button[data-size]")) on(b, Number(b.dataset.size) === textSize);
}

// 点工具条不能同时在画布上落一笔：工具条在选区外面时不会，贴边时就可能重叠。
toolsEl.addEventListener("mousedown", (e) => e.stopPropagation());
toolsEl.addEventListener("mouseup", (e) => e.stopPropagation());
toolsEl.addEventListener("click", (e) => {
  const b = e.target.closest("button");
  if (!b) return;
  if (b.dataset.tool) tool = b.dataset.tool;
  else if (b.dataset.color) penColor = b.dataset.color;
  else if (b.dataset.w) penWidth = Number(b.dataset.w);
  else if (b.dataset.size) textSize = Number(b.dataset.size);
  else if (b.id === "t-undo") undoOp();
  else if (b.id === "t-ok") doCommit();
  else if (b.id === "t-cancel") giveUp("标注态点了取消");
  syncTools();
});

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
      // 标注层（null = 没画东西）。Rust 侧解码后严格核对尺寸，不等就报错而不是缩放。
      layer_png_base64: layerBase64(),
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
  if (annotating) {
    if (tool === "text") {
      startTyping(point(e));
      return;
    }
    if (tool === "mosaic" && !imgDecoded) {
      // 底图没解码就取不到像素，画出来会是一块透明的"假打码"——出声比画错好
      setHint("底图还没解码完，稍等一下再打码", true);
      return;
    }
    live = newOp(point(e));
    redraw();
    return;
  }
  anchor = point(e);
  document.body.classList.add("dragging");
  showSnap(null);
});

document.addEventListener("mousemove", (e) => {
  if (submitted || !started) return;
  if (annotating) {
    if (!live) return;
    updateOp(live, point(e));
    redraw();
    return;
  }
  if (anchor) {
    showBox(anchor, point(e));
    return;
  }
  showSnap(snapAt(point(e)));
});

document.addEventListener("mouseup", (e) => {
  if (annotating) {
    if (!live) return;
    // 抬手时位置没动也算一笔（点一个箭头/一个十字是正常用法）
    ops.push(live);
    live = null;
    redraw();
    return;
  }
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
    // 标注模式下到这里就停：吸附选中只是"确定选区"，提交要等 Enter 或点「完成」
    if (annotate) enterAnnotate();
    else doCommit();
    return;
  }
  showBox(a, end);
  if (annotate) enterAnnotate();
  else doCommit();
});

document.addEventListener("contextmenu", (e) => {
  e.preventDefault();
  // 标注态下右键**不**取消：画了半天的笔不该被一次误触清掉，
  // 放弃这条路有 Esc 和工具条上的「取消」两条明路（直出模式下原语义不变）。
  if (annotating) return;
  giveUp("");
});

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    e.preventDefault();
    giveUp(annotating ? "标注态按 Esc 放弃" : "");
    return;
  }
  if (!annotating) return;
  if (e.key === "Enter") {
    e.preventDefault();
    doCommit();
  } else if (e.key === "Backspace") {
    // 撤销上一笔。不 preventDefault 的话某些 WebView 配置会把它当成"后退"
    e.preventDefault();
    undoOp();
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
    // 上一张的标注必须整体清掉：复用覆盖层时留着的笔会画到下一张图上
    exitAnnotate();
    sel.style.display = "none";
    sizeEl.style.display = "none";
    showSnap(null);
    document.body.classList.remove("has-sel", "dragging", "has-snap");
    img.style.visibility = "hidden";
    // 新的底图还没解码：马赛克这条笔要先等它（见 drawMosaic）
    imgDecoded = false;

    let payload;
    try {
      payload = await invoke("snip_take");
    } catch (err) {
      giveUp("取不到截图底图：" + err);
      return;
    }
    epoch = payload.epoch;
    monitor = payload.monitor;
    annotate = payload.annotate === true;
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
    setHint(
      annotate
        ? "标注模式：拖框或单击选中窗口，然后画标注 · Enter 提交 · Esc 放弃"
        : "拖框自由选区 · 单击选中整个窗口 · Esc / 右键取消"
    );
  } finally {
    pulling = false;
  }
}

(async () => {
  img.addEventListener("error", () => giveUp("底图解码失败，已取消"));
  img.addEventListener("load", () => {
    imgDecoded = true;
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
