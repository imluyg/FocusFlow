// 统计视图（键鼠排行[排行/分组]、应用排行、设备排行、活跃分析）与设置页、数据操作。

import { invoke, emit } from "./tauri.js";
import { $, fmt, fmtDuration, escapeHtml, WD, toast } from "./utils.js";
import { appState, rankFilter, rankTab, analyticsTab } from "./state.js";
import { lineChart, barChart, renderKeyHeat } from "./charts.js";

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
  // 打卡条的「今日」跟着 live 推送走（目标本身 60 秒才取一次）
  liveToday = s.today_count;
  paintGoalStrip();
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

/// 设备是否属于当前筛选。hybrid（同一实例同时上报键鼠事件）在两类筛选里都显示。
function deviceMatchesFilter(d) {
  if (deviceFilter.value === "all") return true;
  const k = d.kind || "unknown";
  return deviceFilter.value === "mouse"
    ? k === "mouse" || k === "hybrid"
    : k === "keyboard" || k === "hybrid";
}

export function renderDevices(s) {
  const box = $("view-devices");
  deviceLastCharts = s;
  // 正在改名时不重建：指纹里含每台设备的计数，而**每按一个键计数就变** ——
  // 重建会把输入框换成新节点，bindAliasInput() 又把焦点抢过去，于是光标跳回开头、
  // 中文输入法正在合成的那几个字被吞掉（草稿文本本身不丢，丢的是手上的编辑）。
  // 列表数字停在推开前的样子无所谓，退出编辑态后的下一次推送会照常用指纹重画。
  const ae = document.activeElement;
  if (deviceEditing && ae && ae.id === "device-alias-input") return;
  // 改名对象被筛掉、或换了周期之后它压根不在列表里时，输入框会随该行一起消失，
  // 但 deviceEditing 还留着 —— 而 openDeviceDetail 开头那句 `if (deviceEditing) return`
  // 会让**所有**设备行点了都没反应，且界面上再没有任何东西能清掉这个状态。
  // 必须在算指纹之前清掉，否则这次推送会画出一份"编辑态已经不存在了"的旧指纹。
  if (deviceEditing) {
    const stillThere = (Array.isArray(s.devices) ? s.devices : []).some(
      (d) => d.key === deviceEditing && deviceMatchesFilter(d)
    );
    if (!stillThere) {
      deviceEditing = null;
      deviceDraft = "";
    }
  }
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
      : list.filter((d) => deviceMatchesFilter(d));
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
  // 刻意不在这里 focus：本函数每 2 秒的推送都会跑到（只要该行还在列表里），
  // 于是用户点到别处去（切筛选、看说明、点某一行）之后焦点会被硬抢回来，
  // 值也是刚重建出来的新节点 → 光标跳回开头、输入法正在合成的字被吞。
  // 进入编辑态时的那一次聚焦由 deviceRename() 负责。
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
  // 进入编辑态的这一次才聚焦（bindAliasInput 里不再 focus，见那里的注释）。
  // 光标放到末尾：预填的是已有别名，从头开始会让他先按一遍 End。
  const input = $("device-alias-input");
  if (input) {
    input.focus();
    const n = input.value.length;
    try {
      input.setSelectionRange(n, n);
    } catch (_) {
      /* 某些控件类型不支持选区，忽略 */
    }
  }
}

/// 保存别名（空值等同还原为自动名）
export function deviceRenameSave(key) {
  const input = $("device-alias-input");
  const alias = input ? input.value : deviceDraft;
  exitDeviceEditing();
  invoke("set_device_alias", { key, alias })
    .catch((e) => console.warn("设备改名失败", e))
    .finally(refreshDeviceDetailIfOpen);
}

/// 清除别名，回到自动名
export function deviceRenameClear(key) {
  exitDeviceEditing();
  invoke("set_device_alias", { key, alias: "" })
    .catch((e) => console.warn("设备还原失败", e))
    .finally(refreshDeviceDetailIfOpen);
}

/// 详情弹窗里也有「改名/还原」，而列表那侧保存完之后没人回头刷新弹窗 ——
/// 弹窗于是继续显示「原名 X」和一个已经无效的「还原」按钮。
function refreshDeviceDetailIfOpen() {
  if (deviceDetailKey) loadDeviceDetail();
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
// 弹窗内"最后一次请求"的序号，用来让先发的慢请求不再落画（见 `loadDeviceDetail`）。
let deviceDetailSeq = 0;

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
  // 只让"最后一次请求"的回应落画（同 plugins.js 的 pluginDetailSeq）。get_device_detail
  // 是跨年四趟扫描、跑在后台线程上，几百毫秒到几秒都可能：点了「1年」再点「今日」，
  // 慢的那次后到就把快的那次盖掉 —— 弹窗里是上一个周期、甚至上一台设备的数字，
  // 而高亮的按钮是用户最后点的那个。closeDeviceDetail 之后重开同理。
  const seq = ++deviceDetailSeq;
  const key = deviceDetailKey;
  body.innerHTML = '<div class="empty">加载中…</div>';
  let d;
  try {
    d = await invoke("get_device_detail", { key, period: deviceDetailPeriod });
  } catch (e) {
    if (seq !== deviceDetailSeq) return;
    body.innerHTML = `<div class="empty">读取失败：${escapeHtml(String(e))}</div>`;
    return;
  }
  if (seq !== deviceDetailSeq || key !== deviceDetailKey) return;
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
  // rank 为 0 有两种完全不同的原因：这周期真没输入，或它排在被截断的 top-100 之外
  // （后端榜单只带前 100 名回来）。有 period_count 却说"无输入"是把话讲反了。
  const rankText =
    d.rank > 0
      ? `第 <b>${d.rank}</b> / ${d.device_count}`
      : Number(d.period_count) > 0
        ? "未进前 100（榜单只回传前 100 名）"
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
    ? keys.length
      ? `<div class="dev-detail-line" style="margin-top:12px;">键鼠明细（${periodText(d.period)}）</div>
       <table class="grid"><thead><tr>
         <th class="col-rank">排名</th><th class="col-key">键名</th><th class="col-count">次数</th><th class="col-percent">占比</th>
       </tr></thead><tbody>${rows}</tbody></table>
       <div class="dev-detail-line" style="font-size:12px;margin-top:6px;">
         共 <b>${keys.length}</b> 个键位，按次数降序${keys.length > TOP ? `（显示前 ${TOP}）` : ""}。
       </div>`
      : `<div class="empty" style="margin-top:12px;">本周期该设备没有键名明细<br>
         <span style="font-size:12px;color:var(--muted);">它其它时间是记过的，把周期换长一点就能看到</span></div>`
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

// 每日目标与连续打卡条。get_goal_status 要扫近 370 天的按日序列，
// 不能跟着 2 秒一次的图表推送跑，这里自带 60 秒节流。
let goalFetchedAt = 0;
// 设置页当前生效的每日目标：非法输入时用它回写控件，避免显示值与后端值不一致
let lastGoalKeys = 20000;
// 上次从后端取到的目标信息（含 streak/best/近 7 天），以及 live 推送里的今日数。
// get_goal_status 要扫近 370 天按日序列，不能跟着 2 秒一次的图表推送跑 —— 但
// 「今日 x / 目标 y」这一项如果只靠 60 秒节流，就会和顶部每 500ms 更新的
// 「今日活跃」卡片长时间不一致（卡片 25,300、打卡条还写着 24,900）。
// 折中：进度与今日数用 live 推送就地补丁，streak/最长天数按 60 秒节流刷新。
let goalInfo = null;
let liveToday = null;

function goalToday() {
  return liveToday == null ? goalInfo.today : liveToday;
}

function paintGoalStrip() {
  const box = $("goal-strip");
  if (!box || !goalInfo) return;
  const today = Math.max(0, goalToday());
  const pctv = Math.max(0, Math.min(100, Math.round((today / goalInfo.goal) * 100)));
  const met = today >= goalInfo.goal;
  const txt = box.querySelector("#goal-today");
  const bar = box.querySelector("#goal-bar");
  const dot = box.querySelector("#goal-dot-today");
  if (!txt || !bar) return;
  txt.textContent = `今日 ${fmt(today)} / ${fmt(goalInfo.goal)}`;
  bar.style.width = pctv + "%";
  bar.style.background = met ? "var(--success)" : "var(--accent)";
  if (dot) dot.style.background = met ? "var(--success)" : "var(--grid-line)";
}

async function refreshGoalStrip(force) {
  const box = $("goal-strip");
  if (!box) return;
  const now = Date.now();
  if (!force && now - goalFetchedAt < 60000) return;
  goalFetchedAt = now;
  let g;
  try {
    g = await invoke("get_goal_status");
  } catch (e) {
    return;
  }
  goalInfo = g;
  const today = Math.max(0, goalToday());
  const pctv = Math.max(0, Math.min(100, Math.round((today / g.goal) * 100)));
  const met = today >= g.goal;
  const dots = (g.days || [])
    .map(
      (d, i, arr) =>
        `<span ${i === arr.length - 1 ? 'id="goal-dot-today" ' : ""}title="${escapeHtml(d.date)}：${fmt(d.count)}" style="width:14px;height:14px;` +
        `border-radius:3px;flex:none;background:${d.met ? "var(--success)" : "var(--grid-line)"};"></span>`
    )
    .join("");
  box.innerHTML = `
    <div style="display:flex;align-items:center;gap:10px;flex-wrap:wrap;margin-bottom:12px;">
      <span id="goal-today" style="font-weight:600;white-space:nowrap;">今日 ${fmt(today)} / ${fmt(g.goal)}</span>
      <div style="flex:1;min-width:120px;height:8px;border-radius:4px;background:var(--grid-line);overflow:hidden;">
        <div id="goal-bar" style="height:100%;width:${pctv}%;background:${met ? "var(--success)" : "var(--accent)"};"></div>
      </div>
      <span style="color:var(--muted);font-size:13px;white-space:nowrap;">
        连续打卡 ${g.streak} 天 · 近一年最长 ${g.best} 天
      </span>
      <span style="display:flex;gap:3px;">${dots}</span>
    </div>`;
}

// 结果必须写进**当下还挂在 DOM 上**的 #set-msg。
///
/// 这些动作都在后台跑几十秒（周报要跨年扫库；改数据目录要等一个非模态的原生选择器，
/// 期间标签页照样能点），而 await 之前拿到的那个节点可能早就被 renderSettings 换成
/// 新的一份了 —— 写进脱离 DOM 的旧节点等于什么都没写，用户看到的就是"点了没反应、
/// 提示永远停在'正在生成…'"。doVacuum/doImport 一直是 await 之后再查一次节点，
/// 这里跟它们对齐。
function setMsg(text, color) {
  const el = $("set-msg");
  if (!el) return;
  if (color) el.style.color = color;
  el.textContent = text;
}

export async function doWeeklyReport() {
  setMsg("正在生成上周周报…", "var(--muted)");
  try {
    const p = await invoke("get_weekly_report");
    setMsg(p ? "周报已生成：" + p : "上一个整周没有任何记录，未生成文件", "var(--success)");
  } catch (e) {
    setMsg("周报生成失败：" + e, "var(--danger)");
  }
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
  $("ana-heat").style.display = analyticsTab.value === "heat" ? "" : "none";
  refreshGoalStrip();
  if (analyticsTab.value === "trend") return renderTrendChart(s);
  if (analyticsTab.value === "hourly") return renderHourlyChart(s);
  if (analyticsTab.value === "heat") return renderKeyHeat($("key-heat"), s.rank || []);
  renderWeekdayChart(s);
}

// ===== 设置 =====
// 侧栏版：左栏分类 + 右面板「一行一项」（名称与说明占左列，控件贴右边缘）。
// 加一项设置只改 SETTINGS_CATS：分类导航、搜索过滤、控件外观与写回都从这张表派生。
//
// 行字段：
//   name 项标题 / desc 说明小字（可以是函数，读快照拼文案）/ descWide 说明占满整行
//   t    "switch" | "text" | "btns" | "raw"（raw 自己给控件字符串，用于开关+下拉这类组合）
//   v    取值（读 get_settings 快照）/ on change 处理器 / wide 占满整行的附加块
//   id   控件 id：set-paused、set-floating 这两个名字不能改，main.js 的
//        pause-changed / floating-changed 订阅按 id 找节点同步勾选
//
// 这张表留在 views.js 而不另开 settings.js：暗色切换要顺手重绘 renderRank /
// renderApps / renderDevices / renderAnalytics，还要动 lastGoalKeys 与
// refreshGoalStrip，全在本文件里；拆出去会形成 views ↔ settings 的循环 import。

const settingsUI = { cat: "general", q: "", data: null };

const BACKUP_HOURS = [6, 12, 24, 48, 168];
const backupHourLabel = (h) => (h === 168 ? "每 7 天" : `每 ${h} 小时`);
// 现实值不在预设里（手改过 config.ini）时补进选项，否则下拉会把它显示成第一项
function backupHourSelect(cur) {
  const opts = BACKUP_HOURS.includes(cur) ? BACKUP_HOURS.slice() : BACKUP_HOURS.concat(cur);
  opts.sort((a, b) => a - b);
  return opts
    .map((h) => `<option value="${h}"${h === cur ? " selected" : ""}>${backupHourLabel(h)}</option>`)
    .join("");
}

async function onDarkChange(e) {
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
}

async function onPauseChange(e) {
  // 同步到实际暂停状态
  const target = e.target.checked;
  if ((await invoke("is_paused")) !== target) await invoke("toggle_pause");
}

async function onHotkeyEnabledChange(e) {
  await invoke("set_config", { section: "hotkey", key: "enabled", value: e.target.checked ? "true" : "false" });
  // 重读一遍设置：配置写入成功但注册失败时 set_config 仍返回 Ok（不能拿它
  // 报错，否则前端以为开关没写上），注册结果只能由 hotkey_error 反映
  await renderSettings();
}

async function onHotkeyStrChange(e) {
  await invoke("set_config", { section: "hotkey", key: "toggle_window", value: e.target.value });
  await renderSettings();
}

// 截图热键：与上面同属 [hotkey] 段、共用 enabled 总开关，但各存各的组合与失败原因
async function onSnipHotkeyStrChange(e) {
  await invoke("set_config", { section: "hotkey", key: "snip", value: e.target.value });
  await renderSettings();
}

// 标注版截图热键：同一段、同一开关，各存各的
async function onSnipAnnotateHotkeyStrChange(e) {
  await invoke("set_config", { section: "hotkey", key: "snip_annotate", value: e.target.value });
  await renderSettings();
}

export async function doSnip() {
  setMsg("正在冻结屏幕…", "var(--muted)");
  try {
    setMsg(await invoke("do_snip"), "var(--success)");
  } catch (e) {
    setMsg("截图未能开始：" + e, "var(--danger)");
  }
}

export async function doSnipAnnotate() {
  setMsg("正在冻结屏幕…", "var(--muted)");
  try {
    setMsg(await invoke("do_snip_annotate"), "var(--success)");
  } catch (e) {
    setMsg("标注截图未能开始：" + e, "var(--danger)");
  }
}

async function onFloatingChange(e) {
  await invoke("set_config", { section: "floating", key: "enabled", value: e.target.checked ? "true" : "false" });
}

// 每日目标：夹到 [1, 5000000]。写回输入框，避免显示与实际生效值不一致
async function onGoalChange(e) {
  // 空串/非正整数一律保持原值，不写库：Number("") 是 0 而不是 NaN，
  // 早先的夹取会把「清空输入框顺手一离开」变成「每日目标 = 1」，等于关掉打卡。
  const raw = e.target.value.trim();
  const n = Math.floor(Number(raw));
  if (raw === "" || !Number.isFinite(n) || n < 1) {
    e.target.value = String(lastGoalKeys);
    const box = $("set-msg");
    if (box) {
      box.style.color = "var(--danger)";
      box.textContent = "每日目标必须是正整数，已保持原来的 " + fmt(lastGoalKeys) + " 次";
    }
    return;
  }
  const v = Math.min(5000000, n);
  lastGoalKeys = v;
  e.target.value = String(v);
  await invoke("set_config", { section: "goal", key: "daily_keys", value: String(v) });
  refreshGoalStrip(true);
}

// 退出时备份：退出路径每次都重读配置，改完立即生效
async function onBackupExitChange(e) {
  await invoke("set_config", {
    section: "database",
    key: "backup_on_exit",
    value: e.target.checked ? "true" : "false",
  });
}

// 定时备份：勾选写入所选小时数，取消写 0（= 关闭）。定时线程每分钟重读配置，
// 无需重启；关闭期间不累积计时，重新打开后从零开始计。
async function onBackupOnlineChange(e) {
  const on = e.target.checked;
  const sel = $("set-backup-interval");
  sel.disabled = !on;
  const hours = on ? Number(sel.value) || 24 : 0;
  await invoke("set_config", {
    section: "database",
    key: "online_backup_interval_hours",
    value: String(hours),
  });
}

async function onBackupIntervalChange(e) {
  if (!$("set-backup-online").checked) return;
  await invoke("set_config", {
    section: "database",
    key: "online_backup_interval_hours",
    value: e.target.value,
  });
}

const SETTINGS_CATS = [
  {
    id: "general",
    cap: "应用",
    label: "常规",
    icon: "◐",
    cards: [
      {
        title: "外观与记录",
        rows: [
          {
            id: "set-dark", t: "switch", name: "暗色模式",
            desc: "本页、图表与悬浮窗立即跟随，不必重启。",
            v: (s) => s.theme === "dark", on: onDarkChange,
          },
          {
            id: "set-paused", t: "switch", name: "暂停记录",
            desc: "暂停期间不再统计键鼠。托盘与悬浮窗也能切，状态会同步到这里。",
            v: (s) => !!s.paused, on: onPauseChange,
          },
        ],
      },
    ],
  },
  {
    id: "hotkey",
    cap: "应用",
    label: "热键",
    icon: "⌨",
    cards: [
      {
        title: "呼出主窗口",
        rows: [
          {
            id: "set-hotkey-enabled", t: "switch", name: "启用热键",
            desc: "按下组合键显示或隐藏主窗口。",
            v: (s) => !!s.hotkey_enabled, on: onHotkeyEnabledChange,
          },
          {
            id: "set-hotkey-str", t: "text", name: "热键组合",
            v: (s) => s.hotkey_str || "", on: onHotkeyStrChange,
            wide: (s) =>
              s.hotkey_enabled && s.hotkey_error
                ? `<div class="ff-wide ff-alert">热键注册失败：${escapeHtml(s.hotkey_error)}（换一个组合键，或让开占用者）</div>`
                : "",
            desc: "形如 Ctrl+Alt+F。改动后立即重新注册：成功不提示，失败会红字写在上面。",
          },
        ],
      },
      {
        title: "区域截图",
        rows: [
          {
            id: "set-snip-hotkey-str", t: "text", name: "截图热键",
            v: (s) => s.snip_hotkey_str || "", on: onSnipHotkeyStrChange,
            wide: (s) =>
              s.hotkey_enabled && s.snip_hotkey_error
                ? `<div class="ff-wide ff-alert">截图热键注册失败：${escapeHtml(s.snip_hotkey_error)}（换一个组合键，或让开占用者）</div>`
                : "",
            desc: "形如 Shift+F1。共用上面那个「启用热键」开关：关掉开关后两条热键都不占用。"
              + "这是全局抢占——启用后别的程序（如 Excel/Word 的 Shift+F1）就拿不到这个组合了。",
          },
          {
            id: "set-snip-annotate-hotkey-str", t: "text", name: "标注截图热键",
            v: (s) => s.snip_annotate_hotkey_str || "", on: onSnipAnnotateHotkeyStrChange,
            wide: (s) =>
              s.hotkey_enabled && s.snip_annotate_hotkey_error
                ? `<div class="ff-wide ff-alert">标注截图热键注册失败：${escapeHtml(s.snip_annotate_hotkey_error)}（换一个组合键，或让开占用者）</div>`
                : "",
            desc: "形如 Shift+F2。按它截图时松手不立刻提交，而是进标注态：画完之后 Enter 或点「完成」才存盘，"
              + "Backspace 撤销上一笔，Esc 放弃（什么都不留下）。上面那条「截图热键」的手势不受影响，"
              + "两条填成同一个组合时两条都不会注册。",
          },
          {
            t: "btns", name: "立即截图",
            btns: [{ act: "do-snip", label: "框选一块" }, { act: "do-snip-annotate", label: "框选并标注" }],
            desc: (s) =>
              "屏幕会先被冻结，拖框选区后存成 PNG 并复制到剪贴板；单击或按 Esc 取消，什么都不留下。"
              + `文件写在 ${escapeHtml(s.screenshots_dir || "data/screenshots/")}，程序不会自动删，也不上传任何东西。`,
            descWide: true,
          },
        ],
      },
    ],
  },
  {
    id: "floating",
    cap: "应用",
    label: "悬浮窗",
    icon: "▣",
    cards: [
      {
        title: "桌面小窗",
        rows: [
          {
            id: "set-floating", t: "switch", name: "显示悬浮窗",
            desc: "关掉后本次运行不再显示。",
            v: (s) => !!s.floating_enabled, on: onFloatingChange,
          },
          {
            t: "btns", name: "临时显示 / 隐藏",
            desc: "只影响当前这一次，不改上面那个开关。",
            btns: [
              { act: "show-floating", label: "立即显示" },
              { act: "hide-floating", label: "立即隐藏" },
            ],
          },
        ],
      },
    ],
  },
  {
    id: "goal",
    cap: "应用",
    label: "目标与周报",
    icon: "◎",
    cards: [
      {
        title: "每日目标",
        rows: [
          {
            // 值不加千分位：onGoalChange 用 Number() 解析，"20,000" 会变 NaN 被当成非法输入
            id: "set-goal", t: "text", cls: "ff-num", name: "每日目标次数",
            v: (s) => String(Number(s.goal_daily_keys ?? 20000) || 20000), on: onGoalChange,
            desc: "达标即算打卡：按整日总活跃次数判定，连续天数显示在「活跃分析」页顶部（今天没达标不清零昨天的纪录）。",
          },
        ],
      },
      {
        title: "周报",
        rows: [
          {
            t: "btns", name: "上周周报",
            btns: [{ act: "do-weekly-report", label: "立即生成" }],
            desc: "应用会在启动后与每周一自动把上一个完整周（周一~周日）汇总成 Markdown 周报，写到 data/reports/ 下；同一周重复生成只会重写同一份文件。",
          },
        ],
      },
    ],
  },
  {
    id: "data",
    cap: "数据",
    label: "目录与备份",
    icon: "🗀",
    cards: [
      {
        title: "数据目录",
        rows: [
          {
            t: "btns", name: "当前位置",
            btns: [{ act: "do-change-data-dir", label: "更改数据文件夹…" }],
            // 后端报的是**当前真正生效**的那个目录（配的目录建不出来时会回落程序目录）
            wide: (s) => `<div class="ff-wide ff-path" id="set-data-home">${escapeHtml(s.data_home || "")}</div>`,
            desc: (s) =>
              (s.data_home_shared_with_app !== false
                ? "数据目前与程序放在一起，拷走整个文件夹即迁移。"
                : "数据已从程序目录挪出。") +
              " 更改会把 data 与 backup <b>整体搬</b>到你选的文件夹：程序先重启，重启时复制并逐文件核对，确认无误才删旧目录；核对没过就保留原样、下次启动重试。日志、插件与 config.ini 仍留在程序目录。搬运期间请勿输入。",
            descWide: true,
          },
        ],
      },
      {
        title: "备份",
        rows: [
          {
            id: "set-backup-exit", t: "switch", name: "退出时自动备份",
            desc: "每次正常退出前给各类库各存一份快照。",
            v: (s) => s.backup_on_exit !== false, on: onBackupExitChange,
          },
          {
            // 0 小时 = 关闭，与 config.ini 同语义
            t: "raw", name: "运行中定时备份",
            ctl: (s) => {
              const h = Number(s.backup_online_hours ?? 24) || 0;
              return (
                `<input type="checkbox" class="sw" id="set-backup-online"${h > 0 ? " checked" : ""}>` +
                `<select id="set-backup-interval"${h > 0 ? "" : " disabled"}>${backupHourSelect(h)}</select>`
              );
            },
            bind: () => {
              $("set-backup-online").addEventListener("change", onBackupOnlineChange);
              $("set-backup-interval").addEventListener("change", onBackupIntervalChange);
            },
            desc: "备份为单文件快照（不受插件开关影响，属核心功能），每类库与设备别名表各保留最近若干份（数量见 config.ini 的 max_backups，别名只在真的改过名时留新的一份）；清空/清理数据、跨年归档前的自动快照始终保留，用于兜底恢复。",
          },
          {
            t: "btns", name: "立即备份",
            btns: [{ act: "do-backup", label: "现在备份一次", primary: true }],
            desc: "马上对当前各类库各存一份快照。",
          },
        ],
      },
    ],
  },
  {
    id: "ops",
    cap: "数据",
    label: "导入与导出",
    icon: "↧",
    cards: [
      {
        title: "导出与整理",
        rows: [
          {
            t: "btns", name: "导出",
            btns: [
              { act: "do-export", label: "导出 CSV", fmt: "csv" },
              { act: "do-export", label: "导出 HTML", fmt: "html" },
            ],
            desc: "导出全部历史数据，与顶部统计周期无关。",
          },
          {
            t: "btns", name: "导入旧数据",
            btns: [{ act: "do-import", label: "选择目录导入" }],
            desc: "把旧版 FocusFlow 目录里的历史数据并进来。",
          },
          {
            t: "btns", name: "压缩数据库",
            btns: [{ act: "do-vacuum", label: "压缩数据库" }],
            desc: "重建库文件、回收空间；数据量大时耗时较久。",
          },
        ],
      },
    ],
  },
];

function descTextOf(r, s) {
  const d = typeof r.desc === "function" ? r.desc(s) : r.desc || "";
  return d || "";
}

function ffRowMatches(r, q) {
  if (!q) return true;
  const s = settingsUI.data || {};
  return (r.name + " " + descTextOf(r, s)).toLowerCase().includes(q);
}

function ffCatMatches(c, q) {
  return (c.label + " " + c.cap).toLowerCase().includes(q);
}

function ffRowHtml(r, s) {
  let ctl = "";
  if (r.t === "switch") {
    ctl = `<input type="checkbox" class="sw" id="${r.id}"${r.v(s) ? " checked" : ""}>`;
  } else if (r.t === "text") {
    ctl = `<input type="text" class="${r.cls || "ff-in"}" id="${r.id}" value="${escapeHtml(r.v(s))}">`;
  } else if (r.t === "btns") {
    ctl = r.btns
      .map(
        (b) =>
          `<button class="btn${b.primary ? "" : " ghost"}" data-act="${b.act}"` +
          `${b.fmt ? ` data-fmt="${b.fmt}"` : ""}>${escapeHtml(b.label)}</button>`
      )
      .join("");
  } else if (r.t === "raw") {
    ctl = r.ctl(s);
  }
  const desc = descTextOf(r, s);
  const wide = r.wide ? r.wide(s) : "";
  return (
    `<div class="ff-item"><span class="ff-name">${escapeHtml(r.name)}</span>` +
    `<span class="ff-ctl">${ctl}</span>${wide}` +
    (desc ? `<span class="ff-desc ${r.descWide ? "span" : "row2"}">${desc}</span>` : "") +
    `</div>`
  );
}

function ffCardHtml(card, s, q) {
  const rows = card.rows.filter((r) => ffRowMatches(r, q));
  if (!rows.length) return "";
  return `<div class="ff-h2">${escapeHtml(card.title)}</div><div class="ff-card">${rows.map((r) => ffRowHtml(r, s)).join("")}</div>`;
}

// 有查询词时把命中的分类摊平成一张长列表；没有就只显示当前分类
function ffVisibleCats(q) {
  if (!q) return SETTINGS_CATS.filter((c) => c.id === settingsUI.cat);
  return SETTINGS_CATS.filter(
    (c) => ffCatMatches(c, q) || c.cards.some((cd) => cd.rows.some((r) => ffRowMatches(r, q)))
  );
}

function ffBindRows(cats) {
  cats.forEach((c) =>
    c.cards.forEach((cd) =>
      cd.rows.forEach((r) => {
        if (r.bind) {
          r.bind();
        } else if (r.on && r.id) {
          const el = $(r.id);
          if (el) el.addEventListener("change", r.on);
        }
      })
    )
  );
}

function paintSettingsBody() {
  const s = settingsUI.data;
  const body = $("ff-body");
  if (!s || !body) return;
  const q = settingsUI.q.trim().toLowerCase();
  const cats = ffVisibleCats(q);
  const html = cats.map((c) => c.cards.map((cd) => ffCardHtml(cd, s, q)).join("")).join("");
  body.innerHTML = html || '<div class="empty">没有匹配的设置</div>';
  ffBindRows(cats);
}

// 只改可见性与高亮，不重建左栏：重建会把搜索框里的焦点与输入一起丢掉
function paintNavFilter() {
  const q = settingsUI.q.trim().toLowerCase();
  document.querySelectorAll(".ff-nav button[data-cat]").forEach((b) => {
    const c = SETTINGS_CATS.find((x) => x.id === b.dataset.cat);
    const hit =
      !q || ffCatMatches(c, q) || c.cards.some((cd) => cd.rows.some((r) => ffRowMatches(r, q)));
    b.style.display = hit ? "" : "none";
    b.classList.toggle("on", !q && c.id === settingsUI.cat);
  });
  document.querySelectorAll(".ff-nav .ff-cap").forEach((el) => {
    el.style.display = q ? "none" : "";
  });
}

function ffNavHtml() {
  let out = "";
  let cap = null;
  for (const c of SETTINGS_CATS) {
    if (c.cap !== cap) {
      cap = c.cap;
      out += `<div class="ff-cap">${escapeHtml(cap)}</div>`;
    }
    out += `<button data-cat="${c.id}"${c.id === settingsUI.cat ? ' class="on"' : ""}><span class="i">${c.icon}</span>${escapeHtml(c.label)}</button>`;
  }
  return out;
}

function paintSettingsShell() {
  const box = $("view-settings");
  // 重建会连带清空 #set-msg：先记下再放回。压缩/导入/改数据目录这些动作要等几十秒
  // 才回话，中途一次重渲染（比如顺手改了热键）不能把它抹掉。
  const prev = $("set-msg");
  const keep = prev ? { text: prev.textContent, color: prev.style.color } : null;
  box.innerHTML = `
    <div class="ff-shell">
      <nav class="ff-nav">
        <input type="text" class="ff-search" id="ff-search" placeholder="搜索设置…" value="${escapeHtml(settingsUI.q)}">
        ${ffNavHtml()}
      </nav>
      <div class="ff-body" id="ff-body"></div>
    </div>
    <div class="ff-foot">
      <div id="set-maint"></div>
      <div id="set-msg"></div>
    </div>`;
  if (keep) {
    const el = $("set-msg");
    el.textContent = keep.text;
    if (keep.color) el.style.color = keep.color;
  }
  $("ff-search").addEventListener("input", (e) => {
    settingsUI.q = e.target.value;
    paintSettingsBody();
    paintNavFilter();
  });
  box.querySelectorAll(".ff-nav button[data-cat]").forEach((b) =>
    b.addEventListener("click", () => {
      settingsUI.cat = b.dataset.cat;
      settingsUI.q = "";
      $("ff-search").value = "";
      paintSettingsBody();
      paintNavFilter();
    })
  );
  paintSettingsBody();
  paintNavFilter();
}

export async function renderSettings() {
  const s = await invoke("get_settings");
  settingsUI.data = s;
  lastGoalKeys = Number(s.goal_daily_keys ?? 20000) || 20000;
  if (!SETTINGS_CATS.some((c) => c.id === settingsUI.cat)) settingsUI.cat = SETTINGS_CATS[0].id;
  paintSettingsShell();
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

export async function doChangeDataDir() {
  // 不弹原生 confirm：本项目的确认要么走页面内的消息位，要么走 openModals 那套，
  // 而这里的"确认"本来就是目录选择器本身 —— 选完目录等于表态，且这一步只复制不删除，
  // 随时可回退。真正需要用户知道的是"要重启"，所以写在按钮旁边的说明里。
  setMsg(
    "请在弹出的窗口里选择新的数据文件夹；选定后程序会重启，并在重启时把 data 与 backup 整体搬过去…",
    "var(--muted)",
  );
  let text;
  try {
    text = await invoke("change_data_dir");
  } catch (e) {
    // 失败的话必须留在页面上。原先这里跟着调了 renderSettings()，而那会整段重建设置页
    // 的 innerHTML —— 刚写进 #set-msg 的错误被它一起抹掉，用户看到的就是"点了没反应、
    // 数据还在原地"，而那句话其实好好地躺在日志里。现在只刷路径那一行，消息位不动。
    setMsg("更改失败: " + e, "var(--danger)");
    toast("更改数据目录失败：" + e);
    refreshDataHome();
    return;
  }
  if (!text || text === "已取消") {
    setMsg("已取消，数据目录没有改动", "var(--muted)");
    refreshDataHome();
    return;
  }
  setMsg(text, "var(--success)");
  // 重启在即，页面马上就没用了：把同一句话再 toast 一遍，免得用户正看着别处
  toast(text);
  // 已安排重启：不再刷设置页，避免在退出路径上多发一轮 IPC。
}

// 只刷"数据目录当前位置"那一行，不重建整页（重建会连带清空 #set-msg）。
async function refreshDataHome() {
  try {
    const s = await invoke("get_settings");
    const el = $("set-data-home");
    if (el) el.textContent = s.data_home || "";
  } catch (_) {
    // 拿不到就留着上一次的显示：这条路径本身就是收尾用的，不值得为它再抛一次
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
    // 备份目录读不出来时不能报"备份数量 0" —— 那和"确实一次没备份过"长得一模一样
    const backupError = info.backup_error ? String(info.backup_error) : "";
    const backupLine = backupError
      ? "备份数量: 未知 —— " + backupError
      : "备份数量: " +
        info.backup_count +
        (info.latest_backup ? "，最新: " + info.latest_backup : "");
    // 压缩时间同理：查不到不能报成"尚未压缩"（当年库被同步盘换成占位文件就是这个形态）
    const vacuumError = info.last_vacuum_error ? String(info.last_vacuum_error) : "";
    const vacuumLine = vacuumError
      ? "上次压缩: 查不到 —— " + vacuumError
      : "上次压缩: " + (info.last_vacuum || "尚未压缩");
    let html =
      `<div${vacuumError ? ` style="color:var(--danger)"` : ""}>${escapeHtml(vacuumLine)}</div>` +
      `<div${backupError ? ` style="color:var(--danger)"` : ""}>${escapeHtml(backupLine)}</div>`;
    // 异常体检提示：备份时发现数据体量异常（骤降/暴涨），当次已冻结轮转
    if (Array.isArray(info.suspect_notes) && info.suspect_notes.length > 0) {
      html += info.suspect_notes
        .map((n) => `<div style="color:var(--danger)">备份异常：${escapeHtml(n)}</div>`)
        .join("");
    }
    $("set-maint").innerHTML = html;
  } catch (e) {}
}
