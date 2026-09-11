// FocusFlow 主界面入口：视图切换、事件推送分发、统一事件委托、启动初始化。

import { invoke, listen } from "./tauri.js";
import { $ } from "./utils.js";
import { appState } from "./state.js";
import { renderPlugins, openPlugin, closePlugin, renderPluginDetail, pluginBtn, pluginBtnSel, pluginField, pluginFieldStay, pluginSelectRow, pluginSelectAll, modalOpen, modalCancel, modalSubmit, modalAction } from "./plugins.js";
import { applyLive, applyCharts, renderRank, renderGroup, renderTrend, renderHourly, renderWeekday, renderSettings, doImport, doExport, doVacuum, doBackup } from "./views.js";

export function switchView(view) {
  appState.currentView = view;
  document.querySelectorAll("#view-tabs .tab").forEach((b) => b.classList.toggle("active", b.dataset.view === view));
  const ids = ["rank", "group", "trend", "hourly", "weekday", "plugins", "settings"];
  ids.forEach((id) => {
    const el = $("view-" + id);
    if (el) el.style.display = id === view ? "" : "none";
  });
  // 插件管理页打开/关闭时切换热重载监听（打开才扫描，平时零后台开销）
  invoke("plugins_watch", { watch: view === "plugins" }).catch(() => {});
  renderCurrentView();
}

// 只渲染当前可见视图，避免每 500ms 全量重绘隐藏视图
function renderCurrentView() {
  if (appState.currentView === "plugins") return renderPlugins();
  if (appState.currentView === "settings") return renderSettings();
  if (!appState.chartsData) return;
  switch (appState.currentView) {
    case "rank": return renderRank(appState.chartsData);
    case "group": return renderGroup(appState.chartsData);
    case "trend": return renderTrend(appState.chartsData);
    case "hourly": return renderHourly(appState.chartsData);
    case "weekday": return renderWeekday(appState.chartsData);
  }
}

// ===== 事件委托 =====
// CSP 的 script-src 不含 'unsafe-inline'（Tauri 会给注入脚本追加 nonce，
// 按 CSP 规范同一指令出现 nonce 时 'unsafe-inline' 被忽略，内联 onclick 会被拦截），
// 因此所有动态 UI 的交互改为 data-act/data-act-change 属性 + 统一委托分发。
// 参数放在 data-* 属性里只需 escapeHtml，彻底避免内联 JS 字符串拼接的转义问题。
// 委托按 closest 自内向外匹配：行内按钮/复选框优先于 <tr> 的行选中，等效旧 stopPropagation。
document.addEventListener("click", (e) => {
  const el = e.target.closest("[data-act]");
  if (!el) return;
  const d = el.dataset;
  switch (d.act) {
    case "open-plugin": openPlugin(d.name); break;
    case "close-plugin": closePlugin(); break;
    case "plugin-btn": pluginBtn(d.name, d.id); break;
    case "plugin-btn-sel": pluginBtnSel(d.name, d.id, d.group || ""); break;
    case "plugin-select-row": pluginSelectRow(el.closest("tr"), d.group || "", d.rid, d.onselect || ""); break;
    case "plugin-select-all": pluginSelectAll(el, d.group || ""); break;
    case "modal-open": modalOpen(d.id); break;
    case "modal-cancel": modalCancel(d.name, d.cancel || "", d.modal); break;
    case "modal-submit": modalSubmit(d.name, d.id, d.modal); break;
    case "modal-action": modalAction(d.name, d.id, d.modal); break;
    // 点击遮罩背景关闭：仅当点击目标就是遮罩层本身（与旧 if (event.target === this) 等价）
    case "modal-overlay-cancel":
      if (e.target === el) modalCancel(d.name, d.cancel || "", d.modal);
      break;
    case "do-import": doImport(); break;
    case "do-export": doExport(d.fmt); break;
    case "do-vacuum": doVacuum(); break;
    case "do-backup": doBackup(); break;
    case "show-floating": invoke("show_floating").catch(() => {}); break;
    case "hide-floating": invoke("hide_floating").catch(() => {}); break;
  }
});
// 输入控件变更（原 onchange 内联）：取控件自身当前值回写插件状态
document.addEventListener("change", (e) => {
  const el = e.target.closest("[data-act-change]");
  if (!el) return;
  const d = el.dataset;
  if (d.actChange === "plugin-field") {
    pluginField(d.name, d.field, el.value);
  } else if (d.actChange === "plugin-field-stay") {
    pluginFieldStay(d.name, d.field, el.value);
  }
});

// ===== 静态 tab 绑定 =====
document.querySelectorAll("#period-tabs .tab").forEach((b) => {
  b.addEventListener("click", () => {
    const p = Number(b.dataset.period);
    invoke("set_period", { period: p });
    // 记住选择，重启后默认显示同一周期
    invoke("set_config", { section: "gui", key: "default_period", value: String(p) });
  });
});
document.querySelectorAll("#view-tabs .tab").forEach((b) => {
  b.addEventListener("click", () => switchView(b.dataset.view));
});
document.querySelectorAll("#trend-days .tab").forEach((b) => {
  b.addEventListener("click", () => {
    appState.trendDays = Number(b.dataset.days);
    document.querySelectorAll("#trend-days .tab").forEach((x) => x.classList.toggle("active", x === b));
    if (appState.chartsData) renderTrend(appState.chartsData);
  });
});

// 初始化主题 + 订阅事件 + 初始加载
(async () => {
  try {
    const dark = (await invoke("get_config", { section: "gui", key: "theme" })) === "dark";
    document.body.classList.toggle("dark", dark);
  } catch (e) {
    console.error("读取主题失败", e);
  }
  // 版本号从后端单一来源读取（Cargo 包版本），失败时用默认占位
  try {
    const v = await invoke("get_version");
    $("version").textContent = v ? "FocusFlow v" + v : "";
  } catch (e) {
    $("version").textContent = "";
  }
  try {
    await listen("stats-live", (e) => applyLive(e.payload));
    await listen("stats-charts", (e) => {
      applyCharts(e.payload);
      // 与 applyCharts 原有分发一致：
      // - 设置页不重建（避免整页 innerHTML 重建清空正在输入的内容）
      // - 插件详情页需要周期性刷新（如番茄钟倒计时），renderPluginDetail 内部做了
      //   代次防抖 + 内容比对，内容未变或输入中不会重建 DOM
      // - 其余统计视图随推送重渲染（周期切换/定时重聚合后排行、趋势等立即更新）
      if (appState.currentView === "settings") return;
      if (appState.currentView === "plugins") {
        if (appState.openPluginName) renderPluginDetail();
        return;
      }
      renderCurrentView();
    });
    // 插件热重载完成 → 若停在插件管理页则刷新列表
    await listen("plugins-reloaded", () => {
      if (appState.currentView === "plugins") renderPlugins();
    });
    // 暂停状态在托盘/命令任意入口改变时即时同步设置页勾选
    await listen("pause-changed", (e) => {
      const cb = $("set-paused");
      if (cb) cb.checked = !!e.payload;
    });
  } catch (e) {
    console.error("事件订阅失败", e);
  }
  try {
    applyLive(await invoke("get_live"));
    applyCharts(await invoke("get_charts"));
    renderCurrentView();
  } catch (e) {
    console.error("初始加载失败", e);
  }
})();
