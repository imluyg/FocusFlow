// 统计视图（排行/分组/趋势/小时/星期）与设置页、数据操作。

import { invoke } from "./tauri.js";
import { $, fmt, fmtDuration, escapeHtml, WD } from "./utils.js";
import { appState, rankFilter } from "./state.js";
import { lineChart, barChart } from "./charts.js";

// ===== 统计快照 =====
// 最高单日卡片：今日周期显示"历史最高"作对比目标，其余周期显示窗口内最高单日。
// 日期一律显示完整 (YYYY-MM-DD)，跨年无歧义。
function applyMax(s) {
  $("st-max-label").textContent = s.period === -1 ? "历史最高" : "最高单日";
  $("st-max").textContent = fmt(s.max_day);
  $("st-max-date").textContent = s.max_day_date ? "(" + s.max_day_date + ")" : "";
}

// 轻量数据：今日/速度/周期（高频推送）
export function applyLive(s) {
  $("st-today").textContent = fmt(s.today_count);
  $("st-active").textContent = fmtDuration(s.active_seconds);
  $("st-cpm").textContent = fmt(s.cpm) + " 次/分";

  const periodLabel = s.period === -1 ? "今日" : s.period === 0 ? "总计" : s.period + "天";
  $("st-total-label").textContent = "周期总数(" + periodLabel + ")";
  $("st-avg-label").textContent = s.period === -1 ? "日均(今日)" : s.period === 0 ? "日均(近30天)" : "日均(" + s.period + "天)";

  document.querySelectorAll("#period-tabs .tab").forEach((b) => {
    b.classList.toggle("active", Number(b.dataset.period) === s.period);
  });

  applyMax(s);
}

// 重量数据：图表/排行（低频推送，变化才更新）
export function applyCharts(s) {
  appState.chartsData = s;
  $("st-total").textContent = fmt(s.total);
  $("st-avg").textContent = fmt(s.avg);
  applyMax(s);
  // 设置页不依赖图表数据：不随推送重建，避免整页 innerHTML 重建清空正在输入的内容
  if (appState.currentView === "settings") return;
  // 插件详情页与统计视图的重渲染分发在 main.js 的 stats-charts 监听器中
}

// ===== 前台应用排行 =====
// 复用按键排行的渲染模式：s.apps 为 [应用名, 累计秒数]（后端已降序截断）。
export function renderApps(s) {
  const box = $("view-apps");
  const empty = '<div class="empty">暂无数据</div>';
  if (!s.apps || s.apps.length === 0) {
    box.innerHTML = empty;
    return;
  }
  const total = s.apps.reduce((acc, [, sec]) => acc + sec, 0);
  const rows = s.apps
    .map(
      ([app, sec], i) =>
        `<tr><td>${i + 1}</td><td class="key">${escapeHtml(app)}</td><td class="num">${fmtDuration(sec)}</td><td>${total ? ((sec / total) * 100).toFixed(2) : "0.00"}%</td></tr>`
    )
    .join("");
  box.innerHTML = `<table class="grid"><thead><tr>
    <th class="col-rank">排名</th><th class="col-key">应用</th><th class="col-count">使用时长</th><th class="col-percent">占比</th>
    </tr></thead><tbody>${rows}</tbody></table>`;
}

// ===== 键鼠排行 =====
function rankIsMouse(k) {
  return k.startsWith("鼠标") || k.startsWith("滚轮");
}

export function renderRank(s) {
  const box = $("view-rank");
  const summary = (hasData) => hasData
    ? `<div class="rank-summary">
        <span>鼠标总次数：<b>${fmt(s.mouse_total)}</b></span>
        <span>键盘总次数：<b>${fmt(s.keyboard_total)}</b></span>
      </div>`
    : "";
  const filterTabs = (data) => `<div class="tabs" id="rank-filter" style="margin-bottom:10px;">
      <span style="color:var(--muted);font-size:13px;">筛选</span>
      <button class="tab ${rankFilter.value === "all" ? "active" : ""}" data-rf="all">全部</button>
      <button class="tab ${rankFilter.value === "mouse" ? "active" : ""}" data-rf="mouse">鼠标</button>
      <button class="tab ${rankFilter.value === "keyboard" ? "active" : ""}" data-rf="keyboard">键盘</button>
    </div><div id="rank-result">${data}</div>`;
  const empty = '<div class="empty">暂无数据</div>';
  if (!s.rank || s.rank.length === 0) {
    box.innerHTML = summary(false) + filterTabs(empty);
    bindRankFilter(s);
    return;
  }
  const total = s.total || 0;
  const src = rankFilter.value === "all"
    ? s.rank
    : s.rank.filter(([k]) => (rankFilter.value === "mouse") === rankIsMouse(k));
  const rows = src
    .map(
      ([k, c], i) =>
        `<tr><td>${i + 1}</td><td class="key">${escapeHtml(k)}</td><td class="num">${fmt(c)}</td><td>${total ? ((c / total) * 100).toFixed(2) : "0.00"}%</td></tr>`
    )
    .join("");
  const table = src.length
    ? `<table class="grid"><thead><tr>
    <th class="col-rank">排名</th><th class="col-key">键鼠</th><th class="col-count">次数</th><th class="col-percent">占比</th>
    </tr></thead><tbody>${rows}</tbody></table>`
    : empty;
  box.innerHTML = summary(true) + filterTabs(table);
  bindRankFilter(s);
}

function bindRankFilter(s) {
  document.querySelectorAll("#rank-filter .tab").forEach((b) => {
    b.addEventListener("click", () => {
      rankFilter.value = b.dataset.rf;
      renderRank(s);
    });
  });
}

export function renderGroup(s) {
  const box = $("view-group");
  if (!s.group || s.group.length === 0) {
    box.innerHTML = '<div class="empty">暂无数据</div>';
    return;
  }
  const total = s.total || 0;
  const rows = s.group
    .map(
      ([k, c]) =>
        `<tr><td class="key">${escapeHtml(k)}</td><td class="num">${fmt(c)}</td><td>${total ? ((c / total) * 100).toFixed(2) : "0.00"}%</td></tr>`
    )
    .join("");
  box.innerHTML = `<table class="grid"><thead><tr>
    <th>分组</th><th>次数</th><th>占比</th>
    </tr></thead><tbody>${rows}</tbody></table>`;
}

// ===== 趋势图 =====
export function renderTrend(s) {
  const data = (appState.trendDays === 30 ? s.trend30 : s.trend) || [];
  const mapped = data.map(([date, value]) => ({ date, value }));
  lineChart($("trend-chart"), "每日活跃趋势", mapped);
}

// ===== 小时 / 星期 =====
export function renderHourly(s) {
  const hourly = s.hourly || [];
  const labels = hourly.map((_, h) => h + "时");
  barChart($("hourly-chart"), "今日每小时活跃", hourly, labels);
}

export function renderWeekday(s) {
  const weekday = s.weekday || [];
  const values = weekday.map(([, v]) => v);
  barChart($("weekday-chart"), "近30天星期活跃", values, WD);
}

// ===== 设置 =====
export async function renderSettings() {
  const box = $("view-settings");
  const s = await invoke("get_settings");
  const dark = s.theme === "dark";
  const paused = s.paused;
  const hotkeyEnabled = s.hotkey_enabled;
  const hotkeyStr = s.hotkey_str;
  const floatingEnabled = s.floating_enabled;

  box.innerHTML = `
    <div class="section-title">常规</div>
    <div class="setting-row"><span class="lbl">暗色模式</span><input type="checkbox" id="set-dark" ${dark ? "checked" : ""}></div>
    <div class="setting-row"><span class="lbl">暂停记录</span><input type="checkbox" id="set-paused" ${paused ? "checked" : ""}></div>

    <div class="section-title">全局热键</div>
    <div class="setting-row"><span class="lbl">启用热键</span><input type="checkbox" id="set-hotkey-enabled" ${hotkeyEnabled ? "checked" : ""}></div>
    <div class="setting-row"><span class="lbl">热键组合</span><input type="text" id="set-hotkey-str" value="${escapeHtml(hotkeyStr)}"></div>

    <div class="section-title">悬浮窗</div>
    <div class="setting-row"><span class="lbl">显示悬浮窗</span><input type="checkbox" id="set-floating" ${floatingEnabled ? "checked" : ""}></div>
    <div class="setting-row"><button class="btn ghost" data-act="show-floating">立即显示</button>
      <button class="btn ghost" data-act="hide-floating">立即隐藏</button></div>

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
    await invoke("set_config", { section: "gui", key: "theme", value: e.target.checked ? "dark" : "light" });
    document.body.classList.toggle("dark", e.target.checked);
    // 图表配色取自 CSS 变量：切换主题后立即用当前数据重绘，不等下次推送
    if (appState.chartsData && appState.currentView === "trend") renderTrend(appState.chartsData);
  });
  $("set-paused").addEventListener("change", async (e) => {
    // 同步到实际暂停状态
    const target = e.target.checked;
    if ((await invoke("is_paused")) !== target) await invoke("toggle_pause");
  });
  $("set-hotkey-enabled").addEventListener("change", async (e) => {
    await invoke("set_config", { section: "hotkey", key: "enabled", value: e.target.checked ? "true" : "false" });
  });
  $("set-hotkey-str").addEventListener("change", async (e) => {
    await invoke("set_config", { section: "hotkey", key: "toggle_window", value: e.target.value });
  });
  $("set-floating").addEventListener("change", async (e) => {
    await invoke("set_config", { section: "floating", key: "enabled", value: e.target.checked ? "true" : "false" });
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
      "退出时自动备份（轮转保留最近若干份），上次压缩: " + (info.last_vacuum || "尚未压缩"),
      "备份数量: " + info.backup_count + (info.latest_backup ? "，最新: " + info.latest_backup : ""),
    ];
    $("set-maint").innerHTML = lines.map((l) => `<div>${escapeHtml(l)}</div>`).join("");
  } catch (e) {}
}
