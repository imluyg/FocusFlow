// 手绘 SVG 折线图 / 柱状图（无外部依赖）

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

// 坐标轴 + 网格 + 标题骨架（折线/柱状共用）
function chartSkeleton(W, H, padL, padR, padT, padB, title, max, colors) {
  const svg = el("svg", { viewBox: `0 0 ${W} ${H}`, width: "100%", height: "100%" });
  svg.appendChild(el("text", { x: W / 2, y: 16, "text-anchor": "middle", "font-size": 15, fill: colors.fg }, title));
  const plotW = W - padL - padR, plotH = H - padT - padB;
  for (let i = 0; i <= 4; i++) {
    const y = padT + plotH - (plotH * i) / 4;
    svg.appendChild(el("line", { x1: padL, y1: y, x2: W - padR, y2: y, stroke: colors.grid, "stroke-width": 1 }));
    svg.appendChild(el("text", { x: padL - 6, y: y + 4, "text-anchor": "end", "font-size": 10, fill: colors.muted }, String(Math.round((max * i) / 4))));
  }
  return { svg, plotW, plotH };
}

// 折线图：data = [{date, value}]，width/height 为 viewBox 逻辑尺寸
export function lineChart(container, title, data) {
  container.innerHTML = "";
  const W = 900, H = 260;
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

  container.appendChild(svg);
}

// 柱状图：values = [v0, v1, ...]，labels 可选
export function barChart(container, title, values, labels) {
  container.innerHTML = "";
  const W = 900, H = 260;
  const padL = 50, padR = 16, padT = 30, padB = 30;
  const colors = chartColors();
  const max = Math.max(1, ...values);
  const { svg, plotW, plotH } = chartSkeleton(W, H, padL, padR, padT, padB, title, max, colors);

  const count = values.length;
  const slot = plotW / count;
  const barW = slot * 0.6;
  const gap = slot * 0.4;

  values.forEach((v, i) => {
    const x = padL + i * slot + gap / 2;
    const barH = (plotH * v) / max;
    const y = padT + plotH - barH;
    svg.appendChild(el("rect", { x, y, width: barW, height: barH, rx: 2, fill: colors.accent, "fill-opacity": 0.85 }));
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
}
