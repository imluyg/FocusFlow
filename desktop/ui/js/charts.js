// 手绘 SVG 折线图 / 柱状图（无外部依赖）
// 尺寸：渲染时实测容器宽度（不再用固定 viewBox 等比缩放，窗口变窄文字不再跟着缩），
// ResizeObserver 监听容器变化自动重绘；悬停显示数据点 tooltip。

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
  return { accent: v("--accent"), muted: v("--muted"), grid: v("--grid-line"), fg: v("--fg") };
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
    // 重建前先收起 tooltip：container.innerHTML = "" 会连同 svg 一起丢掉
    // mouseleave 监听，悬停中遇重绘会留下永久残留的提示框（主窗口活跃时每 2 秒推送一次）。
    hideTip(tip);
    const W = Math.max(320, Math.floor(container.clientWidth));
    const H = Math.max(220, Math.floor(container.clientHeight) || 260);
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
    // 重建前先收起 tooltip：container.innerHTML = "" 会连同 svg 一起丢掉
    // mouseleave 监听，悬停中遇重绘会留下永久残留的提示框（主窗口活跃时每 2 秒推送一次）。
    hideTip(tip);
    const W = Math.max(320, Math.floor(container.clientWidth));
    const H = Math.max(220, Math.floor(container.clientHeight) || 260);
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
