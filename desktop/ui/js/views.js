// 统计视图（键鼠排行[排行/分组]、应用排行、设备排行、活跃分析）与设置页、数据操作。

import { invoke, emit } from "./tauri.js";
import { $, fmt, fmtDuration, escapeHtml, WD } from "./utils.js";
import { appState, rankFilter, rankTab, analyticsTab } from "./state.js";
import { lineChart, barChart } from "./charts.js";

// ===== 统计快照 =====
// 最高单日卡片：今日周期显示"历史最高"作对比目标，其余周期显示窗口内最高单日。
// 日期一律显示完整 (YYYY-MM-DD)，跨年无歧义。
function applyMax(s) {
  $("st-max-label").textContent = s.period === -1 ? "历史最高" : "最高单日";
  $("st-max").textContent = fmt(s.max_day);
  $("st-max-date").textContent = s.max_day_date ? "(" + s.max_day_date + ")" : "";
}

// 总计卡片：今日周期下"今日次数"已由"今日活跃"卡片显示，
// 这里改显全历史总计，避免两张卡片是同一个数字；其余周期仍显示所选周期总数。
// 取值来源：charts 推送带 total，live 推送带 period_total / alltime_total
// （后端在同一把锁里快照 period + 两个总数，切周期时不会标签与数值错配）。
function applyTotal(s) {
  const alltime = s.period === -1 || s.period === 0;
  $("st-total-label").textContent = alltime ? "总计" : "周期总数(" + s.period + "天)";
  const value = s.period === -1 ? s.alltime_total : s.period_total != null ? s.period_total : s.total;
  if (typeof value === "number") $("st-total").textContent = fmt(value);
}

// 轻量数据：今日/速度/周期（高频推送）
export function applyLive(s) {
  $("st-today").textContent = fmt(s.today_count);
  $("st-active").textContent = "时长 " + fmtDuration(s.active_seconds);
  $("st-cpm").textContent = fmt(s.cpm) + " 次/分";

  $("st-avg-label").textContent = s.period === -1 ? "日均(今日)" : s.period === 0 ? "日均(近30天)" : "日均(" + s.period + "天)";
  applyTotal(s);

  document.querySelectorAll("#period-tabs .tab").forEach((b) => {
    const active = Number(b.dataset.period) === s.period;
    b.classList.toggle("active", active);
    b.setAttribute("aria-selected", String(active));
  });

  applyMax(s);
}

// 重量数据：图表/排行（低频推送，变化才更新）
export function applyCharts(s) {
  appState.chartsData = s;
  applyTotal(s);
  $("st-avg").textContent = fmt(s.avg);
  applyMax(s);
  // 设置页不依赖图表数据：不随推送重建，避免整页 innerHTML 重建清空正在输入的内容
  if (appState.currentView === "settings") return;
  // 插件详情页与统计视图的重渲染分发在 main.js 的 stats-charts 监听器中
}

// ===== 前台应用排行 =====
// 复用按键排行的渲染模式：s.apps 为 [应用名, 累计秒数]（后端已降序截断）。
// 数据指纹：活跃时每 2s 推送一次 stats-charts，内容未变就跳过整表重建，
// 避免滚动位置/悬停状态丢失与视觉抖动。
function skipIfUnchanged(box, fp) {
  if (box.dataset.fp === fp) return true;
  box.dataset.fp = fp;
  return false;
}

// 周期文案：与应用排行/键鼠排行共用，让周期状态在内容区可见
function periodText(p) {
  if (p === -1) return "今日";
  if (p === 0) return "全部历史";
  return "近" + p + "天";
}

export function renderApps(s) {
  const box = $("view-apps");
  const head = (extra) =>
    `<div class="rank-summary"><span>统计周期：<b>${periodText(s.period)}</b></span>${extra}</div>`;
  // 指纹含周期：切周期后即使数据内容恰巧相同也要重建（标签随周期变化）
  const fp = JSON.stringify([s.period, s.app_total, s.apps]);
  if (skipIfUnchanged(box, fp)) return;
  if (!s.apps || s.apps.length === 0) {
    box.innerHTML = head("") + '<div class="empty">暂无数据</div>';
    return;
  }
  // 占比分母用后端返回的周期总时长（未截断）；旧版后端无该字段时回退为列表求和
  const total = Number(s.app_total) || s.apps.reduce((acc, [, sec]) => acc + sec, 0);
  const covered = s.apps.reduce((acc, [, sec]) => acc + sec, 0);
  // Top N 未覆盖全部时长时给出覆盖度，避免把"占 Top100"误读成"占全部"
  const cover = total && covered < total
    ? `<span>Top${s.apps.length} 覆盖：<b>${((covered / total) * 100).toFixed(1)}%</b></span>`
    : "";
  const summary = head(
    `<span>应用总时长：<b>${fmtDuration(total)}</b></span>` +
      `<span>覆盖应用：<b>${fmt(s.apps.length)}</b> 个</span>${cover}`
  );
  const rows = s.apps
    .map(
      ([app, sec], i) =>
        `<tr><td>${i + 1}</td><td class="key">${escapeHtml(app)}</td><td class="num">${fmtDuration(sec)}</td><td>${total ? ((sec / total) * 100).toFixed(2) : "0.00"}%</td></tr>`
    )
    .join("");
  box.innerHTML = summary + `<table class="grid"><thead><tr>
    <th class="col-rank">排名</th><th class="col-key">应用</th><th class="col-count">使用时长</th><th class="col-percent">占比</th>
    </tr></thead><tbody>${rows}</tbody></table>`;
}

// ===== 设备排行 =====
// 数据来自 Raw Input 侧信道（独立口径：键盘按下 + 鼠标左右中键按下 + 滚轮事件），
// 与「键鼠排行」的过滤规则不同（不做长按去重/修饰键过滤、不含合成输入），
// 数字不追求相等 —— 设备维度回答的是"哪个设备在用、占比多少"。
// 设备名可改：自动解析出的名字（HID 鼠标 · 24AE/1464）不直观，用户可设别名，
// 存 data/device_aliases.json（后端 alias 优先，自动名作为副标题保留）。
const deviceFilter = { value: "all" };
// 改名编辑态：正在改的设备 key + 草稿值（跨推送保留，避免每 2 秒重渲染吃掉输入）
let deviceEditing = null;
let deviceDraft = "";
let deviceLastCharts = null;

const kindLabel = (k) => ({ mouse: "鼠标", keyboard: "键盘", hybrid: "键鼠" }[k] || "未知");

export function renderDevices(s) {
  const box = $("view-devices");
  deviceLastCharts = s;
  // 指纹含周期、筛选与编辑态：切周期/切筛选/进改名时重建，纯数据推送则跳过
  const fp = JSON.stringify([s.period, s.device_total, s.devices, deviceFilter.value, deviceEditing]);
  if (skipIfUnchanged(box, fp)) return;
  const summary = (extra) =>
    `<div class="rank-summary"><span>统计周期：<b>${periodText(s.period)}</b></span>${extra}</div>`;
  const list = Array.isArray(s.devices) ? s.devices : [];
  if (list.length === 0) {
    box.innerHTML =
      summary("") +
      '<div class="empty">暂无设备数据<br><span style="font-size:12px;color:var(--muted);">设备统计随程序运行开始积累，更换键鼠后各自独立计数</span></div>';
    return;
  }
  // 占比分母用后端返回的周期总次数（未截断）；旧版后端无该字段时回退为列表求和
  const total = Number(s.device_total) || list.reduce((acc, d) => acc + d.count, 0);
  const covered = list.reduce((acc, d) => acc + d.count, 0);
  const cover =
    total && covered < total
      ? `<span>Top${list.length} 覆盖：<b>${((covered / total) * 100).toFixed(1)}%</b></span>`
      : "";
  const head = summary(
    `<span>输入总次数：<b>${fmt(total)}</b></span>` +
      `<span>设备数：<b>${list.length}</b> 个</span>${cover}`
  );
  const filterTabs = (data) => `<div class="tabs" id="device-filter" role="tablist" aria-label="设备筛选" style="margin-bottom:10px;">
      <span style="color:var(--muted);font-size:13px;">筛选</span>
      <button class="tab ${deviceFilter.value === "all" ? "active" : ""}" data-df="all" role="tab" aria-selected="${deviceFilter.value === "all"}">全部</button>
      <button class="tab ${deviceFilter.value === "mouse" ? "active" : ""}" data-df="mouse" role="tab" aria-selected="${deviceFilter.value === "mouse"}">鼠标</button>
      <button class="tab ${deviceFilter.value === "keyboard" ? "active" : ""}" data-df="keyboard" role="tab" aria-selected="${deviceFilter.value === "keyboard"}">键盘</button>
    </div><div id="device-result">${data}</div>`;
  const src =
    deviceFilter.value === "all"
      ? list
      : list.filter((d) => {
          const k = d.kind || "unknown";
          // hybrid（同一实例同时上报键鼠事件）在两类筛选里都显示
          return deviceFilter.value === "mouse"
            ? k === "mouse" || k === "hybrid"
            : k === "keyboard" || k === "hybrid";
        });
  const rows = src
    .map((d, i) => {
      const editing = deviceEditing === d.key;
      // 名称 + （有别名时）原名副标题：都用 title 挂全文，截断后悬停可看全部
      const nameCell = editing
        ? `<input type="text" id="device-alias-input" value="${escapeHtml(deviceDraft)}" placeholder="${escapeHtml(d.auto_name)}" maxlength="24">`
        : `<div title="${escapeHtml(d.name)}">${escapeHtml(d.name)}</div>` +
          (d.has_alias
            ? `<div class="dev-auto" title="${escapeHtml(d.auto_name)}">原名 ${escapeHtml(d.auto_name)}</div>`
            : "");
      const actions = editing
        ? `<button class="tab dev-act" data-act="device-rename-save" data-key="${escapeHtml(d.key)}">保存</button>` +
          `<button class="tab dev-act" data-act="device-rename-cancel">取消</button>`
        : `<button class="tab dev-act" data-act="device-rename" data-key="${escapeHtml(d.key)}">改名</button>` +
          (d.has_alias
            ? `<button class="tab dev-act" data-act="device-rename-clear" data-key="${escapeHtml(d.key)}">还原</button>`
            : "");
      return `<tr data-act="device-detail" data-key="${escapeHtml(d.key)}"><td>${i + 1}</td><td class="key">${nameCell}</td><td class="col-kind">${kindLabel(d.kind)}</td><td class="num">${fmt(d.count)}</td><td>${total ? ((d.count / total) * 100).toFixed(2) : "0.00"}%</td><td class="col-act">${actions}</td></tr>`;
    })
    .join("");
  const table = src.length
    ? `<table class="grid grid-devices"><thead><tr>
    <th class="col-rank">排名</th><th class="col-key">设备</th><th class="col-kind">类型</th><th class="col-count">次数</th><th class="col-percent">占比</th><th class="col-act">操作</th>
    </tr></thead><tbody>${rows}</tbody></table>`
    : '<div class="empty">该类型暂无设备</div>';
  const note =
    '<div style="margin-top:8px;font-size:12px;color:var(--muted);">口径说明：设备排行与键鼠排行口径不同，这里按物理动作计次——滚轮每格、按键每次（含长按重复）都算，且只认真实硬件（脚本/宏的模拟输入不计）；键鼠排行则做了滚动合并与长按去重。因此两者数字一般不会相同，设备侧略高属正常。<br>设备名可点「改名」自定义，同型号设备换 USB 口后仍生效。</div>';
  box.innerHTML = head + filterTabs(table) + note;
  bindDeviceFilter(s);
  bindAliasInput();
}

function bindDeviceFilter(s) {
  document.querySelectorAll("#device-filter .tab").forEach((b) => {
    b.addEventListener("click", () => {
      deviceFilter.value = b.dataset.df;
      renderDevices(s);
    });
  });
}

// 编辑态草稿：每次输入同步到 deviceDraft，重渲染（数据推送）后输入不丢；回车即保存
function bindAliasInput() {
  const input = $("device-alias-input");
  if (!input) return;
  input.addEventListener("input", () => {
    deviceDraft = input.value;
  });
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") deviceRenameSave(deviceEditing);
    if (e.key === "Escape") deviceRenameCancel();
  });
  input.focus();
}

/// 进入改名编辑态（草稿预填已有别名，便于修改）
export function deviceRename(key) {
  // 从弹窗点「改名」时先关弹窗：编辑框在表格行里，被弹窗盖住就看不见了
  closeDeviceDetail();
  const list = (deviceLastCharts && deviceLastCharts.devices) || [];
  const dev = list.find((d) => d.key === key);
  deviceEditing = key;
  deviceDraft = dev && dev.has_alias ? dev.name : "";
  renderDevices(deviceLastCharts);
}

/// 保存别名（空值等同还原为自动名）
export function deviceRenameSave(key) {
  const input = $("device-alias-input");
  const alias = input ? input.value : deviceDraft;
  exitDeviceEditing();
  invoke("set_device_alias", { key, alias }).catch((e) => console.warn("设备改名失败", e));
}

/// 清除别名，回到自动名
export function deviceRenameClear(key) {
  exitDeviceEditing();
  invoke("set_device_alias", { key, alias: "" }).catch((e) => console.warn("设备还原失败", e));
}

/// 取消编辑
export function deviceRenameCancel() {
  exitDeviceEditing();
}

function exitDeviceEditing() {
  deviceEditing = null;
  deviceDraft = "";
  renderDevices(deviceLastCharts);
}

// ===== 设备详情弹窗 =====
// 点设备行打开：展示该设备各周期次数、周期内排名/占比、活跃情况与键名明细。
// 数据按需向后端查询（get_device_detail），不参与高频统计推送。
// 弹窗内可单独切换统计周期（与主界面周期互不影响）。
const PERIODS = [
  [-1, "今日"],
  [7, "7天"],
  [15, "15天"],
  [30, "30天"],
  [365, "1年"],
  [0, "总计"],
];
let deviceDetailKey = null;
let deviceDetailPeriod = 0;

export async function openDeviceDetail(key) {
  const overlay = $("device-modal");
  const body = $("device-modal-body");
  if (!overlay || !body) return;
  // 改名编辑态下不弹详情（输入框点击会冒泡到行；这里再兜一层）
  if (deviceEditing) return;
  deviceDetailKey = key;
  // 默认跟随主界面当前周期，之后可在弹窗内单独切换
  deviceDetailPeriod = appState.chartsData ? Number(appState.chartsData.period) : 0;
  overlay.style.display = "flex";
  await loadDeviceDetail();
}

/// 弹窗内切换周期：只重查详情，不动主界面
export async function deviceDetailSetPeriod(p) {
  deviceDetailPeriod = Number(p);
  await loadDeviceDetail();
}

async function loadDeviceDetail() {
  const body = $("device-modal-body");
  if (!body || !deviceDetailKey) return;
  body.innerHTML = '<div class="empty">加载中…</div>';
  let d;
  try {
    d = await invoke("get_device_detail", { key: deviceDetailKey, period: deviceDetailPeriod });
  } catch (e) {
    body.innerHTML = `<div class="empty">读取失败：${escapeHtml(String(e))}</div>`;
    return;
  }
  body.innerHTML = deviceDetailHtml(d);
}

export function closeDeviceDetail() {
  const overlay = $("device-modal");
  if (overlay) overlay.style.display = "none";
  deviceDetailKey = null;
}

function deviceDetailHtml(d) {
  const kind = kindLabel(d.kind);
  const total = Number(d.period_total) || 0;
  const pct = total ? ((d.period_count / total) * 100).toFixed(2) + "%" : "—";
  const rankText =
    d.rank > 0
      ? `第 <b>${d.rank}</b> / ${d.device_count}`
      : "<b>本周期无输入</b>";
  const kindText =
    d.kind_rank > 0 ? `第 <b>${d.kind_rank}</b> / ${d.kind_count}` : "—";
  const avg = Number(d.avg_per_active_day || 0);

  const cell = (k, v) =>
    `<div class="dev-detail-cell"><div class="k">${k}</div><div class="v">${v}</div></div>`;

  // 弹窗内周期切换：与主界面周期按钮同一套取值（-1 今日 / 0 总计 / n 天）
  const periodTabs = `<div class="tabs" id="device-period-tabs" role="tablist" aria-label="明细周期">
      <span style="color:var(--muted);font-size:13px;">明细周期</span>
      ${PERIODS.map(
        ([p, label]) =>
          `<button class="tab ${p === d.period ? "active" : ""}" data-act="device-period" data-p="${p}" role="tab" aria-selected="${p === d.period}">${label}</button>`
      ).join("")}
    </div>`;

  // 键名明细：与「键鼠排行」同样的表格（排名 / 键名 / 次数 / 占比）
  const keys = Array.isArray(d.keys) ? d.keys : [];
  const keyTotal = Number(d.key_total) || 0;
  const TOP = 20;
  const shown = keys.slice(0, TOP);
  const rows = shown
    .map(
      ([name, c], i) =>
        `<tr><td>${i + 1}</td><td class="key">${escapeHtml(name)}</td><td class="num">${fmt(c)}</td><td>${keyTotal ? ((c / keyTotal) * 100).toFixed(2) : "0.00"}%</td></tr>`
    )
    .join("");
  const keySection = d.has_key_detail
    ? `<div class="dev-detail-line" style="margin-top:12px;">键鼠明细（${periodText(d.period)}）</div>
       <table class="grid"><thead><tr>
         <th class="col-rank">排名</th><th class="col-key">键名</th><th class="col-count">次数</th><th class="col-percent">占比</th>
       </tr></thead><tbody>${rows}</tbody></table>
       <div class="dev-detail-line" style="font-size:12px;margin-top:6px;">
         共 <b>${keys.length}</b> 个键位，按次数降序${keys.length > TOP ? `（显示前 ${TOP}）` : ""}。
       </div>`
    : `<div class="empty" style="margin-top:12px;">该设备暂无键名明细<br>
         <span style="font-size:12px;color:var(--muted);">键名明细从该功能上线后开始积累，此前的历史数据无法回溯</span></div>`;

  return `
    <div class="dev-detail-head">
      <span class="dev-detail-name">${escapeHtml(d.name)}</span>
      <span class="dev-detail-sub">${kind}</span>
      ${d.has_alias ? `<span class="dev-detail-sub">原名 ${escapeHtml(d.auto_name)}</span>` : ""}
    </div>
    <div class="dev-detail-sub" title="${escapeHtml(d.key)}">设备标识 ${escapeHtml(d.key)}</div>

    ${periodTabs}

    <div class="dev-detail-grid">
      ${cell("今日", fmt(d.today))}
      ${cell("近 7 天", fmt(d.week))}
      ${cell("近 30 天", fmt(d.month))}
      ${cell("总计", fmt(d.all))}
    </div>

    <div class="dev-detail-line">
      ${periodText(d.period)}：<b>${fmt(d.period_count)}</b> 次 · 占比 <b>${pct}</b>
      · 设备排名 ${rankText} · 同类（${kind}）${kindText}
    </div>
    <div class="dev-detail-line">
      活跃 <b>${d.active_days}</b> 天 · 活跃日均 <b>${fmt(Math.round(avg))}</b> 次
      ${d.first_date ? ` · 首次 <b>${escapeHtml(d.first_date)}</b>` : ""}
      ${d.last_date ? ` · 最近 <b>${escapeHtml(d.last_date)}</b>` : ""}
    </div>

    ${keySection}

    <div class="dev-detail-line" style="font-size:12px;margin-top:10px;">
      设备维度为独立口径，按物理动作计次：滚轮每格、按键每次（含长按重复）都算，只统计真实硬件输入
      （脚本/宏的模拟输入不计）；键鼠排行做了滚动合并与长按去重，两者数字一般不会相同，设备侧略高属正常。
    </div>
    <div class="setting-row" style="margin-top:10px;">
      <button class="tab active" data-act="device-rename" data-key="${escapeHtml(d.key)}">改名</button>
      ${d.has_alias ? `<button class="tab" data-act="device-rename-clear" data-key="${escapeHtml(d.key)}">还原</button>` : ""}
      <button class="tab" data-act="device-detail-close">关闭</button>
    </div>`;
}

// ===== 键鼠排行 =====
function rankIsMouse(k) {
  return k.startsWith("鼠标") || k.startsWith("滚轮");
}

export function renderRank(s) {
  const box = $("view-rank");
  // 指纹含周期、页内视图（排行/分组）与筛选状态：切换时仍会重建，纯数据推送则跳过
  const fp = JSON.stringify([s.period, s.rank, s.group, s.mouse_total, s.keyboard_total, s.total, rankFilter.value, rankTab.value]);
  if (skipIfUnchanged(box, fp)) return;
  const summary = (hasData) =>
    `<div class="rank-summary"><span>统计周期：<b>${periodText(s.period)}</b></span>${
      hasData
        ? `<span>鼠标总次数：<b>${fmt(s.mouse_total)}</b></span>
        <span>键盘总次数：<b>${fmt(s.keyboard_total)}</b></span>`
        : ""
    }</div>`;
  // 页内视图切换：分组统计与排行同源同周期（同吃 keys 数据），收进同一页
  const viewTabs = `<div class="tabs" id="rank-tabs" role="tablist" aria-label="视图" style="margin-bottom:10px;">
      <span style="color:var(--muted);font-size:13px;">视图</span>
      <button class="tab ${rankTab.value === "rank" ? "active" : ""}" data-rt="rank" role="tab" aria-selected="${rankTab.value === "rank"}">排行</button>
      <button class="tab ${rankTab.value === "group" ? "active" : ""}" data-rt="group" role="tab" aria-selected="${rankTab.value === "group"}">分组</button>
    </div>`;
  const filterTabs = (data) => `<div class="tabs" id="rank-filter" role="tablist" aria-label="排行筛选" style="margin-bottom:10px;">
      <span style="color:var(--muted);font-size:13px;">筛选</span>
      <button class="tab ${rankFilter.value === "all" ? "active" : ""}" data-rf="all" role="tab" aria-selected="${rankFilter.value === "all"}">全部</button>
      <button class="tab ${rankFilter.value === "mouse" ? "active" : ""}" data-rf="mouse" role="tab" aria-selected="${rankFilter.value === "mouse"}">鼠标</button>
      <button class="tab ${rankFilter.value === "keyboard" ? "active" : ""}" data-rf="keyboard" role="tab" aria-selected="${rankFilter.value === "keyboard"}">键盘</button>
    </div><div id="rank-result">${data}</div>`;
  const empty = '<div class="empty">暂无数据</div>';
  let body;
  if (rankTab.value === "group") {
    const rows = (s.group || [])
      .map(
        ([k, c]) =>
          `<tr><td class="key">${escapeHtml(k)}</td><td class="num">${fmt(c)}</td><td>${s.total ? ((c / s.total) * 100).toFixed(2) : "0.00"}%</td></tr>`
      )
      .join("");
    body = viewTabs + (rows
      ? `<table class="grid"><thead><tr><th>分组</th><th>次数</th><th>占比</th></tr></thead><tbody>${rows}</tbody></table>`
      : empty);
  } else if (!s.rank || s.rank.length === 0) {
    body = viewTabs + filterTabs(empty);
  } else {
    const src = rankFilter.value === "all"
      ? s.rank
      : s.rank.filter(([k]) => (rankFilter.value === "mouse") === rankIsMouse(k));
    const rows = src
      .map(
        ([k, c], i) =>
          `<tr><td>${i + 1}</td><td class="key">${escapeHtml(k)}</td><td class="num">${fmt(c)}</td><td>${s.total ? ((c / s.total) * 100).toFixed(2) : "0.00"}%</td></tr>`
      )
      .join("");
    const table = src.length
      ? `<table class="grid"><thead><tr>
    <th class="col-rank">排名</th><th class="col-key">键鼠</th><th class="col-count">次数</th><th class="col-percent">占比</th>
    </tr></thead><tbody>${rows}</tbody></table>`
      : empty;
    body = viewTabs + filterTabs(table);
  }
  const hasData = rankTab.value === "group"
    ? !!(s.group && s.group.length)
    : !!(s.rank && s.rank.length);
  box.innerHTML = summary(hasData) + body;
  bindRankTabs(s);
  bindRankFilter(s);
}

function bindRankTabs(s) {
  document.querySelectorAll("#rank-tabs .tab").forEach((b) => {
    b.addEventListener("click", () => {
      rankTab.value = b.dataset.rt;
      renderRank(s);
    });
  });
}

function bindRankFilter(s) {
  document.querySelectorAll("#rank-filter .tab").forEach((b) => {
    b.addEventListener("click", () => {
      rankFilter.value = b.dataset.rf;
      renderRank(s);
    });
  });
}

// ===== 活跃分析 =====
// 趋势 / 小时 / 星期三张图同属「活跃时长」的时间切面，但数据窗口各自固定
// （趋势 7/30 可切、小时固定今日、星期固定近30天），收进一页做页内切换；
// 每张图标题自带窗口说明，不再与顶部的统计周期选择器混淆。

// 趋势图单独导出：切「近7天/近30天」时只重绘这一张
export function renderTrendChart(s) {
  const data = (appState.trendDays === 30 ? s.trend30 : s.trend) || [];
  const mapped = data.map(([date, value]) => ({ date, value }));
  lineChart($("trend-chart"), `每日活跃趋势（近${appState.trendDays}天）`, mapped);
}

function renderHourlyChart(s) {
  const hourly = s.hourly || [];
  barChart($("hourly-chart"), "今日每小时活跃", hourly, hourly.map((_, h) => h + "时"));
}

function renderWeekdayChart(s) {
  const weekday = s.weekday || [];
  barChart($("weekday-chart"), "近30天星期活跃", weekday.map(([, v]) => v), WD);
}

// 只渲染当前页内视图；分块可见性在这里同步（切页/推送重绘都走这里，状态保持一致）
export function renderAnalytics(s) {
  document.querySelectorAll("#analytics-tabs .tab").forEach((b) => {
    const active = b.dataset.at === analyticsTab.value;
    b.classList.toggle("active", active);
    b.setAttribute("aria-selected", String(active));
  });
  $("ana-trend").style.display = analyticsTab.value === "trend" ? "" : "none";
  $("ana-hourly").style.display = analyticsTab.value === "hourly" ? "" : "none";
  $("ana-weekday").style.display = analyticsTab.value === "weekday" ? "" : "none";
  if (analyticsTab.value === "trend") return renderTrendChart(s);
  if (analyticsTab.value === "hourly") return renderHourlyChart(s);
  renderWeekdayChart(s);
}

// ===== 设置 =====
export async function renderSettings() {
  const box = $("view-settings");
  const s = await invoke("get_settings");
  const dark = s.theme === "dark";
  const paused = s.paused;
  const hotkeyEnabled = s.hotkey_enabled;
  const hotkeyStr = s.hotkey_str;
  const hotkeyError = s.hotkey_error || "";
  const floatingEnabled = s.floating_enabled;
  // 备份开关：退出时备份 / 运行中定时备份（0 小时 = 关闭，与 config.ini 同语义）
  const backupOnExit = s.backup_on_exit !== false;
  const backupHours = Number(s.backup_online_hours ?? 24) || 0;
  const backupOnline = backupHours > 0;
  const hourOptions = [6, 12, 24, 48, 168];
  if (backupOnline && !hourOptions.includes(backupHours)) hourOptions.push(backupHours);
  hourOptions.sort((a, b) => a - b);
  const hourLabel = (h) => (h === 168 ? "每 7 天" : `每 ${h} 小时`);
  const hourSelect = hourOptions
    .map((h) => `<option value="${h}" ${h === backupHours ? "selected" : ""}>${hourLabel(h)}</option>`)
    .join("");

  box.innerHTML = `
    <div class="section-title">常规</div>
    <div class="setting-row"><span class="lbl">暗色模式</span><input type="checkbox" id="set-dark" ${dark ? "checked" : ""}></div>
    <div class="setting-row"><span class="lbl">暂停记录</span><input type="checkbox" id="set-paused" ${paused ? "checked" : ""}></div>

    <div class="section-title">全局热键</div>
    <div class="setting-row"><span class="lbl">启用热键</span><input type="checkbox" id="set-hotkey-enabled" ${hotkeyEnabled ? "checked" : ""}></div>
    <div class="setting-row"><span class="lbl">热键组合</span><input type="text" id="set-hotkey-str" value="${escapeHtml(hotkeyStr)}"></div>
    ${
      hotkeyEnabled && hotkeyError
        ? `<div style="font-size:12px;color:var(--danger);margin:4px 0;">热键注册失败：${escapeHtml(
            hotkeyError
          )}（换一个组合键，或让开占用者）</div>`
        : ""
    }

    <div class="section-title">悬浮窗</div>
    <div class="setting-row"><span class="lbl">显示悬浮窗</span><input type="checkbox" id="set-floating" ${floatingEnabled ? "checked" : ""}></div>
    <div class="setting-row"><button class="btn ghost" data-act="show-floating">立即显示</button>
      <button class="btn ghost" data-act="hide-floating">立即隐藏</button></div>

    <div class="section-title">数据备份</div>
    <div class="setting-row"><span class="lbl">退出时自动备份</span><input type="checkbox" id="set-backup-exit" ${backupOnExit ? "checked" : ""}></div>
    <div class="setting-row">
      <span class="lbl">运行中定时备份</span>
      <input type="checkbox" id="set-backup-online" ${backupOnline ? "checked" : ""}>
      <select id="set-backup-interval" ${backupOnline ? "" : "disabled"}>${hourSelect}</select>
    </div>
    <div style="color:var(--muted);font-size:12px;margin-top:4px;">
      备份为单文件快照（不受插件开关影响，属核心功能），每类库各保留最近若干份（数量见 config.ini 的 max_backups）；
      清空/清理数据、跨年归档前的自动快照始终保留，用于兜底恢复。
    </div>

    <div class="section-title">数据操作</div>
    <div class="setting-row">
      <button class="btn ghost" data-act="do-import">导入旧数据</button>
      <button class="btn ghost" data-act="do-export" data-fmt="csv">导出 CSV</button>
      <button class="btn ghost" data-act="do-export" data-fmt="html">导出 HTML</button>
      <button class="btn" data-act="do-vacuum">压缩数据库</button>
      <button class="btn ghost" data-act="do-backup">立即备份</button>
    </div>
    <div id="set-msg" style="color:var(--success);margin-top:8px;"></div>
    <div id="set-maint" style="color:var(--muted);font-size:13px;margin-top:8px;"></div>
  `;

  $("set-dark").addEventListener("change", async (e) => {
    const dark = e.target.checked;
    await invoke("set_config", { section: "gui", key: "theme", value: dark ? "dark" : "light" });
    document.body.classList.toggle("dark", dark);
    // 图表配色取自 CSS 变量：切换主题后立即重绘当前统计视图。
    // 不能只重绘 trend——空闲时后端可能长时间不推送 stats-charts，
    // 停在小时/星期分布页会一直保持旧配色。
    const chartViews = { rank: renderRank, apps: renderApps, devices: renderDevices, analytics: renderAnalytics };
    const rerender = chartViews[appState.currentView];
    if (appState.chartsData && rerender) rerender(appState.chartsData);
    // 悬浮窗只在启动时读一次主题，这里广播让它实时跟随
    emit("theme-changed", dark).catch(() => {});
  });
  $("set-paused").addEventListener("change", async (e) => {
    // 同步到实际暂停状态
    const target = e.target.checked;
    if ((await invoke("is_paused")) !== target) await invoke("toggle_pause");
  });
  $("set-hotkey-enabled").addEventListener("change", async (e) => {
    await invoke("set_config", { section: "hotkey", key: "enabled", value: e.target.checked ? "true" : "false" });
    // 重读一遍设置：配置写入成功但注册失败时 set_config 仍返回 Ok（不能拿它
    // 报错，否则前端以为开关没写上），注册结果只能由 hotkey_error 反映
    await renderSettings();
  });
  $("set-hotkey-str").addEventListener("change", async (e) => {
    await invoke("set_config", { section: "hotkey", key: "toggle_window", value: e.target.value });
    await renderSettings();
  });
  $("set-floating").addEventListener("change", async (e) => {
    await invoke("set_config", { section: "floating", key: "enabled", value: e.target.checked ? "true" : "false" });
  });
  // 退出时备份：退出路径每次都重读配置，改完立即生效
  $("set-backup-exit").addEventListener("change", async (e) => {
    await invoke("set_config", {
      section: "database",
      key: "backup_on_exit",
      value: e.target.checked ? "true" : "false",
    });
  });
  // 定时备份：勾选写入所选小时数，取消写 0（= 关闭）。定时线程每分钟重读配置，
  // 无需重启；关闭期间不累积计时，重新打开后从零开始计。
  $("set-backup-online").addEventListener("change", async (e) => {
    const on = e.target.checked;
    const sel = $("set-backup-interval");
    sel.disabled = !on;
    const hours = on ? Number(sel.value) || 24 : 0;
    await invoke("set_config", {
      section: "database",
      key: "online_backup_interval_hours",
      value: String(hours),
    });
  });
  $("set-backup-interval").addEventListener("change", async (e) => {
    if (!$("set-backup-online").checked) return;
    await invoke("set_config", {
      section: "database",
      key: "online_backup_interval_hours",
      value: e.target.value,
    });
  });
  renderMaintInfo();
}

export async function doVacuum() {
  try {
    await invoke("vacuum_db");
    $("set-msg").textContent = "压缩完成";
    $("set-msg").style.color = "var(--success)";
  } catch (e) {
    $("set-msg").textContent = "压缩失败: " + e;
    $("set-msg").style.color = "var(--danger)";
  }
}

export async function doImport() {
  try {
    const msg = await invoke("import_legacy");
    $("set-msg").textContent = msg || "导入完成";
    $("set-msg").style.color = "var(--success)";
    applyLive(await invoke("get_live"));
    applyCharts(await invoke("get_charts"));
  } catch (e) {
    $("set-msg").textContent = "导入失败: " + e;
    $("set-msg").style.color = "var(--danger)";
  }
}

export async function doExport(fmtName) {
  try {
    const path = await invoke("export_report", { fmt: fmtName });
    $("set-msg").textContent = path && path !== "已取消" ? "已导出: " + path : "已取消";
    $("set-msg").style.color = "var(--success)";
  } catch (e) {
    $("set-msg").textContent = "导出失败: " + e;
    $("set-msg").style.color = "var(--danger)";
  }
}

export async function doBackup() {
  try {
    const path = await invoke("do_backup");
    $("set-msg").textContent = "备份完成: " + path;
    $("set-msg").style.color = "var(--success)";
    await renderMaintInfo();
  } catch (e) {
    $("set-msg").textContent = "备份失败: " + e;
    $("set-msg").style.color = "var(--danger)";
  }
}

async function renderMaintInfo() {
  try {
    const info = await invoke("get_maintenance_info");
    const lines = [
      "上次压缩: " + (info.last_vacuum || "尚未压缩"),
      "备份数量: " + info.backup_count + (info.latest_backup ? "，最新: " + info.latest_backup : ""),
    ];
    let html = lines.map((l) => `<div>${escapeHtml(l)}</div>`).join("");
    // 异常体检提示：备份时发现数据体量异常（骤降/暴涨），当次已冻结轮转
    if (Array.isArray(info.suspect_notes) && info.suspect_notes.length > 0) {
      html += info.suspect_notes
        .map((n) => `<div style="color:var(--danger)">备份异常：${escapeHtml(n)}</div>`)
        .join("");
    }
    $("set-maint").innerHTML = html;
  } catch (e) {}
}
