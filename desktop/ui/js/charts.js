// 手绘 SVG 折线图 / 柱状图（无外部依赖）
// 尺寸：渲染时实测容器宽度（不再用固定 viewBox 等比缩放，窗口变窄文字不再跟着缩），
// ResizeObserver 监听容器变化自动重绘；悬停显示数据点 tooltip。

import { escapeHtml } from "./utils.js";

const NS = "http://www.w3.org/2000/svg";

function el(tag, attrs, text) {
  const node = document.createElementNS(NS, tag);
  for (const [k, v] of Object.entries(attrs || {})) node.setAttribute(k, v);
  if (text != null) node.textContent = text;
  return node;
}

function fmtThousands(n) {
  return Number(n).toLocaleString("zh-CN");
}

// 读取当前主题的图表配色（每次渲染读取一次，主题切换后的下一次推送即生效）
function chartColors() {
  const css = getComputedStyle(document.documentElement);
  const v = (name) => css.getPropertyValue(name).trim();
  return { accent: v("--accent"), muted: v("--muted"), grid: v("--grid-line"), fg: v("--fg"), card: v("--card") };
}

// ===== 尺寸自适应 + tooltip 基础设施 =====
const resizeObserver =
  typeof ResizeObserver !== "undefined"
    ? new ResizeObserver((entries) => {
        for (const entry of entries) {
          const rerender = entry.target._chartRerender;
          if (rerender) requestAnimationFrame(rerender);
        }
      })
    : null;

// 容器准备：定位上下文 + tooltip 元素 + 尺寸变化重绘钩子
function prepareContainer(container) {
  if (getComputedStyle(container).position === "static") {
    container.style.position = "relative";
  }
  let tip = container.querySelector(".chart-tip");
  if (!tip) {
    tip = document.createElement("div");
    tip.className = "chart-tip";
    container.appendChild(tip);
  }
  if (resizeObserver && !container._chartRerender) {
    resizeObserver.observe(container);
  }
  return tip;
}

function showTip(tip, container, clientX, clientY, html) {
  tip.innerHTML = html;
  tip.style.display = "block";
  const rect = container.getBoundingClientRect();
  const tw = tip.offsetWidth;
  // 优先跟随鼠标并居中，靠边时夹回容器内
  let x = clientX - rect.left - tw / 2;
  x = Math.max(4, Math.min(x, rect.width - tw - 4));
  let y = clientY - rect.top - tip.offsetHeight - 10;
  if (y < 4) y = clientY - rect.top + 14;
  tip.style.left = `${x}px`;
  tip.style.top = `${y}px`;
}

function hideTip(tip) {
  tip.style.display = "none";
}

// 同一份数据 + 同一尺寸 + 同一主题就不重画：整棵 SVG 重建会丢掉悬停状态
// （参考线、放大点、tooltip 全没了），而主窗口活跃时每 ~2 秒就有一次推送。
// 主题进指纹是必须的 —— 设置页切主题后是「用同样的数据再调一次渲染」，
// 只看数据的守卫会把这次重绘一起跳过。ResizeObserver 触发的重绘靠尺寸变化
// 自然通过校验。
function chartKey(W, H, title, payloadSig) {
  return `${W}x${H}|${title}|${document.body.className}|${payloadSig}`;
}

// 坐标轴 + 网格 + 标题骨架（折线/柱状共用）。W/H 为实测像素，1:1 绘制。
function chartSkeleton(W, H, padL, padR, padT, padB, title, max, colors) {
  const svg = el("svg", { viewBox: `0 0 ${W} ${H}`, width: W, height: H });
  svg.appendChild(el("text", { x: W / 2, y: 16, "text-anchor": "middle", "font-size": 15, fill: colors.fg }, title));
  const plotW = W - padL - padR, plotH = H - padT - padB;
  for (let i = 0; i <= 4; i++) {
    const y = padT + plotH - (plotH * i) / 4;
    svg.appendChild(el("line", { x1: padL, y1: y, x2: W - padR, y2: y, stroke: colors.grid, "stroke-width": 1 }));
    svg.appendChild(el("text", { x: padL - 6, y: y + 4, "text-anchor": "end", "font-size": 10, fill: colors.muted }, String(Math.round((max * i) / 4))));
  }
  return { svg, plotW, plotH };
}

// 折线图：data = [{date, value}]
export function lineChart(container, title, data) {
  const tip = prepareContainer(container);

  const render = () => {
    container._chartRerender = render;
    const W = Math.max(320, Math.floor(container.clientWidth));
    const H = Math.max(220, Math.floor(container.clientHeight) || 260);
    const key = chartKey(W, H, title, data.map((d) => d.date + ":" + d.value).join(","));
    // 还要确认图形确实挂在容器里：指纹断言的是「上一次画的结果还在」，
    // 而容器可能被别处的 innerHTML 赋值整体清掉，那时跳过就会留下空白
    if (container._chartKey === key && container.querySelector("svg")) return;
    container._chartKey = key;
    // 重建前先收起 tooltip：container.innerHTML = "" 会连同 svg 一起丢掉
    // mouseleave 监听，悬停中遇重绘会留下永久残留的提示框（主窗口活跃时每 2 秒推送一次）。
    hideTip(tip);
    container.innerHTML = "";
    container.appendChild(tip);
    const padL = 50, padR = 16, padT = 30, padB = 30;
    const colors = chartColors();
    const max = Math.max(1, ...data.map((d) => d.value));
    const { svg, plotW, plotH } = chartSkeleton(W, H, padL, padR, padT, padB, title, max, colors);

    const n = data.length;
    if (n < 2) {
      svg.appendChild(el("text", { x: W / 2, y: H / 2, "text-anchor": "middle", "font-size": 14, fill: colors.muted }, "暂无数据"));
      container.appendChild(svg);
      return;
    }

    const stepX = plotW / (n - 1);
    const pts = data.map((d, i) => {
      const x = padL + stepX * i;
      const y = padT + plotH - (plotH * d.value) / max;
      return { x, y };
    });

    const fillPts = `${padL},${padT + plotH} ${pts.map((p) => `${p.x},${p.y}`).join(" ")} ${padL + plotW},${padT + plotH}`;
    svg.appendChild(el("polygon", { points: fillPts, fill: colors.accent, "fill-opacity": 0.25 }));
    svg.appendChild(el("polyline", { points: pts.map((p) => `${p.x},${p.y}`).join(" "), fill: "none", stroke: colors.accent, "stroke-width": 2.5, "stroke-linejoin": "round" }));
    for (const p of pts) svg.appendChild(el("circle", { cx: p.x, cy: p.y, r: 3.5, fill: colors.accent }));

    const labelStep = Math.max(1, Math.floor(n / 7));
    data.forEach((d, i) => {
      if (i % labelStep === 0 || i === n - 1) {
        const x = padL + stepX * i;
        const short = d.date.length >= 10 ? d.date.slice(5) : d.date;
        svg.appendChild(el("text", { x, y: padT + plotH + 16, "text-anchor": "middle", "font-size": 10, fill: colors.muted }, short));
      }
    });

    // 悬停：竖直参考线 + 放大点 + tooltip（跟随最近数据点）
    const guide = el("line", { x1: 0, y1: padT, x2: 0, y2: padT + plotH, stroke: colors.grid, "stroke-width": 1, visibility: "hidden" });
    const dot = el("circle", { cx: 0, cy: 0, r: 5.5, fill: colors.accent, stroke: colors.fg, "stroke-width": 1.5, visibility: "hidden" });
    svg.appendChild(guide);
    svg.appendChild(dot);
    svg.addEventListener("mousemove", (e) => {
      const rect = svg.getBoundingClientRect();
      const x = e.clientX - rect.left;
      const i = Math.max(0, Math.min(n - 1, Math.round((x - padL) / stepX)));
      const p = pts[i];
      guide.setAttribute("x1", p.x);
      guide.setAttribute("x2", p.x);
      guide.setAttribute("visibility", "visible");
      dot.setAttribute("cx", p.x);
      dot.setAttribute("cy", p.y);
      dot.setAttribute("visibility", "visible");
      showTip(tip, container, e.clientX, e.clientY, `<b>${fmtThousands(data[i].value)}</b> 次<div class="chart-tip-sub">${data[i].date}</div>`);
    });
    svg.addEventListener("mouseleave", () => {
      guide.setAttribute("visibility", "hidden");
      dot.setAttribute("visibility", "hidden");
      hideTip(tip);
    });

    container.appendChild(svg);
  };
  render();
}

// 柱状图：values = [v0, v1, ...]，labels 可选
export function barChart(container, title, values, labels) {
  const tip = prepareContainer(container);

  const render = () => {
    container._chartRerender = render;
    const W = Math.max(320, Math.floor(container.clientWidth));
    const H = Math.max(220, Math.floor(container.clientHeight) || 260);
    const sig = values.join(",") + "|" + (labels ? labels.join("\u0001") : "");
    const key = chartKey(W, H, title, sig);
    // 还要确认图形确实挂在容器里：指纹断言的是「上一次画的结果还在」，
    // 而容器可能被别处的 innerHTML 赋值整体清掉，那时跳过就会留下空白
    if (container._chartKey === key && container.querySelector("svg")) return;
    container._chartKey = key;
    // 重建前先收起 tooltip：container.innerHTML = "" 会连同 svg 一起丢掉
    // mouseleave 监听，悬停中遇重绘会留下永久残留的提示框（主窗口活跃时每 2 秒推送一次）。
    hideTip(tip);
    container.innerHTML = "";
    container.appendChild(tip);
    const padL = 50, padR = 16, padT = 30, padB = 30;
    const colors = chartColors();
    const max = Math.max(1, ...values);
    const { svg, plotW, plotH } = chartSkeleton(W, H, padL, padR, padT, padB, title, max, colors);

    const count = values.length;
    const slot = count ? plotW / count : plotW;
    const barW = slot * 0.6;
    const gap = slot * 0.4;

    values.forEach((v, i) => {
      const x = padL + i * slot + gap / 2;
      const barH = (plotH * v) / max;
      const y = padT + plotH - barH;
      const rect = el("rect", { x, y, width: barW, height: barH, rx: 2, fill: colors.accent, "fill-opacity": 0.85 });
      // 不可见的全高命中区：悬停整列都能出 tooltip（细柱更好指）
      const hit = el("rect", { x: padL + i * slot, y: padT, width: slot, height: plotH, fill: "transparent" });
      const label = labels && labels.length === count ? labels[i] : `#${i + 1}`;
      hit.addEventListener("mousemove", (e) => {
        rect.setAttribute("fill-opacity", "1");
        showTip(tip, container, e.clientX, e.clientY, `<b>${fmtThousands(v)}</b> 次<div class="chart-tip-sub">${label}</div>`);
      });
      hit.addEventListener("mouseleave", () => {
        rect.setAttribute("fill-opacity", "0.85");
        hideTip(tip);
      });
      svg.appendChild(rect);
      svg.appendChild(hit);
      if (v > 0) {
        svg.appendChild(el("text", { x: x + barW / 2, y: y - 5, "text-anchor": "middle", "font-size": 9, fill: colors.fg }, fmtThousands(v)));
      }
    });

    if (labels && labels.length === count) {
      const labelStep = Math.max(1, Math.floor(count / 12));
      labels.forEach((lab, i) => {
        if (i % labelStep === 0) {
          const x = padL + i * slot + slot / 2;
          svg.appendChild(el("text", { x, y: padT + plotH + 16, "text-anchor": "middle", "font-size": 10, fill: colors.muted }, lab));
        }
      });
    }

    container.appendChild(svg);
  };
  render();
}

// ===== 键盘热力图（物理键位） =====
// rank = [[键名, 次数], ...]，键名与 crates/core/src/format.rs classify_key 的命名一致；
// 版面上没摆的键（输入法标点/CapsLock…）统一进底部「其它按键」条，不丢数据。
// 颜色一律走 chartColors() 取令牌实际值：SVG 表现属性里的 var() 不生效，
// 主题切换后的重绘由 chartKey 指纹里的 body.className 保证。

// [键名, x, 宽]，单位 u = 一个标准键宽；主区按 ANSI 15u 排布
const KH_MAIN_ROWS = [
  { y: 0, keys: [["Esc", 0], ["F1", 1.5], ["F2", 2.5], ["F3", 3.5], ["F4", 4.5], ["F5", 6], ["F6", 7], ["F7", 8], ["F8", 9], ["F9", 10.5], ["F10", 11.5], ["F11", 12.5], ["F12", 13.5]] },
  { y: 1.2, keys: [["`", 0], ["1", 1], ["2", 2], ["3", 3], ["4", 4], ["5", 5], ["6", 6], ["7", 7], ["8", 8], ["9", 9], ["0", 10], ["-", 11], ["=", 12], ["退格", 13, 2]] },
  { y: 2.4, keys: [["Tab", 0, 1.5], ["Q", 1.5], ["W", 2.5], ["E", 3.5], ["R", 4.5], ["T", 5.5], ["Y", 6.5], ["U", 7.5], ["I", 8.5], ["O", 9.5], ["P", 10.5], ["[", 11.5], ["]", 12.5], ["\\", 13.5, 1.5]] },
  { y: 3.6, keys: [["A", 1.75], ["S", 2.75], ["D", 3.75], ["F", 4.75], ["G", 5.75], ["H", 6.75], ["J", 7.75], ["K", 8.75], ["L", 9.75], [";", 10.75], ["'", 11.75], ["回车", 12.75, 2.25]] },
  { y: 4.8, keys: [["左Shift", 0, 2.25], ["Z", 2.25], ["X", 3.25], ["C", 4.25], ["V", 5.25], ["B", 6.25], ["N", 7.25], ["M", 8.25], [",", 9.25], [".", 10.25], ["/", 11.25], ["右Shift", 12.25, 2.75]] },
  { y: 6, keys: [["左Ctrl", 0, 1.25], ["左Win", 1.25, 1.25], ["左Alt", 2.5, 1.25], ["空格", 3.75, 6.25], ["右Alt", 10, 1.25], ["右Win", 11.25, 1.25], ["右Ctrl", 13.75, 1.25]] },
];

// [键名, x, y, 宽, 高, 显示名]
const KH_NAV = [
  ["Insert", 16, 1.2, 1, 1, "Ins"], ["Home", 17.1, 1.2], ["PageUp", 18.2, 1.2, 1, 1, "PgUp"],
  ["Delete", 16, 2.4, 1, 1, "Del"], ["End", 17.1, 2.4], ["PageDown", 18.2, 2.4, 1, 1, "PgDn"],
  ["↑", 17.1, 3.9], ["←", 16, 5.1], ["↓", 17.1, 5.1], ["→", 18.2, 5.1],
];

// 鼠标簇刻意做小一号，读起来不像是键盘上的键
const KH_MOUSE = [
  ["滚轮上滑", 21.38, 1.2, 0.9, 0.85, "滚↑"],
  ["鼠标左键", 20.3, 2.1, 0.9, 1.9, "左键"], ["鼠标中键", 21.38, 2.1, 0.9, 0.85, "中键"], ["鼠标右键", 22.46, 2.1, 0.9, 1.9, "右键"],
  ["滚轮下滑", 21.38, 3.0, 0.9, 0.85, "滚↓"],
];

function khMainKeys(row) {
  return row.keys.map(([name, x, w]) => ({ name, x, y: row.y, w: w || 1, h: 1, label: name }));
}

function khCluster(defs) {
  return defs.map(([name, x, y, w, h, label]) => ({ name, x, y, w: w || 1, h: h || 1, label: label || name }));
}

const KH_BOARD = KH_MAIN_ROWS.flatMap(khMainKeys).concat(khCluster(KH_NAV), khCluster(KH_MOUSE));
const KH_PLACED = new Set(KH_BOARD.map((k) => k.name));
const KH_W = 23.4;
const KH_H = 7.15;

function khClamp(v, lo, hi) {
  return Math.max(lo, Math.min(hi, v));
}

// 次数：负数/非数字按 0 处理，重复键名合并
function khCounts(rank) {
  const counts = new Map();
  if (!Array.isArray(rank)) return counts;
  for (const item of rank) {
    const name = item && item[0];
    const c = Math.max(0, Number(item && item[1]) || 0);
    if (typeof name !== "string" || !name || c <= 0) continue;
    counts.set(name, (counts.get(name) || 0) + c);
  }
  return counts;
}

function khHeat(count, maxCount) {
  if (!(count > 0) || !(maxCount > 0)) return 0;
  return 0.06 + 0.8 * Math.min(1, count / maxCount);
}

// 非 ASCII（汉字/箭头/全角标点）按整宽估，ASCII 按 0.6 宽
function khTextUnits(s) {
  let units = 0;
  for (const ch of s) units += ch.charCodeAt(0) < 128 ? 0.6 : 1;
  return units;
}

function khFitFont(label, w, h) {
  return khClamp(Math.min((w - 4) / Math.max(1, khTextUnits(label)), h * 0.55), 6.5, 13);
}

function khTip(tip, container, e, name, count, total) {
  const pct = total > 0 ? (count / total) * 100 : 0;
  const p = pct >= 10 ? pct.toFixed(0) : pct.toFixed(1);
  showTip(tip, container, e.clientX, e.clientY, `<b>${fmtThousands(count)}</b> 次<div class="chart-tip-sub">${escapeHtml(name)} · ${p}%</div>`);
}

function khKeyNode(k, u, colors, counts, maxCount, total, tip, container) {
  const count = counts.get(k.name) || 0;
  const x = k.x * u, y = k.y * u, w = k.w * u - 2, h = k.h * u - 2;
  const g = el("g", { class: "keyheat-key" });
  g.appendChild(el("rect", { class: "keyheat-base", x, y, width: w, height: h, rx: 3, fill: colors.card, stroke: colors.grid, "stroke-width": 1 }));
  const heat = khHeat(count, maxCount);
  if (heat > 0) g.appendChild(el("rect", { x, y, width: w, height: h, rx: 3, fill: colors.accent, "fill-opacity": heat.toFixed(3) }));
  g.appendChild(el("text", {
    class: "keyheat-label", x: x + w / 2, y: y + h / 2, "text-anchor": "middle", "dominant-baseline": "central",
    "font-size": khFitFont(k.label, w, h), fill: count > 0 ? colors.fg : colors.muted,
  }, k.label));
  g.addEventListener("mousemove", (e) => khTip(tip, container, e, k.name, count, total));
  g.addEventListener("mouseleave", () => hideTip(tip));
  return g;
}

// 其它按键：宽度靠字数估算（容器可能处于 display:none，量不到真实字宽），末尾留冗余
function khChipLayout(items, W) {
  const fs = 11, gap = 6, rowH = 24;
  const placed = [];
  let x = 2, y = 0, rows = 0;
  for (const [name, count] of items) {
    const chars = Array.from(name);
    const short = chars.length > 10 ? chars.slice(0, 10).join("") + "…" : name;
    const w = Math.round(khTextUnits(short + " " + fmtThousands(count)) * fs) + 18;
    if (x > 2 && x + w > W - 2) {
      x = 2;
      y += rowH;
      rows++;
    }
    placed.push({ name, count, label: short, x, y, w });
    x += w + gap;
  }
  return { items: placed, h: placed.length ? (rows + 1) * rowH + 20 : 0 };
}

function khChip(c, colors, maxCount, total, tip, container) {
  const heat = khHeat(c.count, maxCount);
  const g = el("g", { class: "keyheat-key" });
  const attrs = { x: c.x, y: c.y, width: c.w, height: 20, rx: 6 };
  g.appendChild(el("rect", Object.assign({ class: "keyheat-base", fill: colors.card, stroke: colors.grid, "stroke-width": 1 }, attrs)));
  if (heat > 0) g.appendChild(el("rect", Object.assign({ fill: colors.accent, "fill-opacity": heat.toFixed(3) }, attrs)));
  g.appendChild(el("text", {
    class: "keyheat-label", x: c.x + c.w / 2, y: c.y + 10, "text-anchor": "middle", "dominant-baseline": "central",
    "font-size": 11, fill: colors.fg,
  }, c.label + " " + fmtThousands(c.count)));
  g.addEventListener("mousemove", (e) => khTip(tip, container, e, c.name, c.count, total));
  g.addEventListener("mouseleave", () => hideTip(tip));
  return g;
}

export function renderKeyHeat(container, rank) {
  const tip = prepareContainer(container);

  const render = () => {
    container._chartRerender = render;
    const counts = khCounts(rank);
    let total = 0, maxCount = 0;
    for (const v of counts.values()) {
      total += v;
      if (v > maxCount) maxCount = v;
    }
    const W = Math.max(320, Math.floor(container.clientWidth));
    const AH = Math.max(0, Math.floor(container.clientHeight));
    const leftover = Array.from(counts).filter(([name]) => !KH_PLACED.has(name));
    const strip = khChipLayout(leftover.slice(0, 20), W);
    const hintH = total > 0 ? 0 : 26;
    // 键宽同时受容器宽高约束：容器高度是写死的内联样式，板子超出去就会压到下方内容
    const byWidth = (W - 10) / KH_W;
    const byHeight = AH > 80 ? (AH - strip.h - hintH - 8) / KH_H : Infinity;
    const u = khClamp(Math.min(byWidth, byHeight), 11, 40);
    const boardH = KH_H * u;
    const H = Math.ceil(boardH + strip.h + hintH + 4);
    const sig = Array.isArray(rank) ? rank.map((it) => it[0] + "\u0002" + it[1]).join("\u0001") : "";
    const key = chartKey(W, H, "keyheat", sig);
    if (container._chartKey === key && container.querySelector("svg")) return;
    container._chartKey = key;
    hideTip(tip);
    container.innerHTML = "";
    container.appendChild(tip);
    const colors = chartColors();

    const svg = el("svg", { class: "keyheat-svg", viewBox: `0 0 ${W} ${H}`, width: W, height: H });
    const board = el("g", { transform: `translate(${Math.max(0, Math.round((W - KH_W * u) / 2))},0)` });
    for (const k of KH_BOARD) board.appendChild(khKeyNode(k, u, colors, counts, maxCount, total, tip, container));
    const cap = khClamp(u * 0.34, 9, 12);
    board.appendChild(el("text", { class: "keyheat-label", x: 17.6 * u, y: 7.05 * u, "text-anchor": "middle", "font-size": cap, fill: colors.muted }, "导航键"));
    board.appendChild(el("text", { class: "keyheat-label", x: 21.83 * u, y: 4.4 * u, "text-anchor": "middle", "font-size": cap, fill: colors.muted }, "鼠标"));
    svg.appendChild(board);

    if (strip.items.length) {
      const head = leftover.length > strip.items.length
        ? `其它按键（显示 ${strip.items.length} / ${leftover.length}）`
        : `其它按键 ${strip.items.length} 项`;
      svg.appendChild(el("text", { class: "keyheat-label", x: 2, y: boardH + 13, "font-size": 11, fill: colors.muted }, head));
      const chips = el("g", { transform: `translate(0,${Math.round(boardH + 18)})` });
      for (const c of strip.items) chips.appendChild(khChip(c, colors, maxCount, total, tip, container));
      svg.appendChild(chips);
    }
    if (total === 0) {
      svg.appendChild(el("text", { x: W / 2, y: boardH + 20, "text-anchor": "middle", "font-size": 13, fill: colors.muted }, "当前周期没有键盘数据"));
    }

    container.appendChild(svg);
  };
  render();
}
