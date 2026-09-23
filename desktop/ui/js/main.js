// FocusFlow 主界面入口：视图切换、事件推送分发、统一事件委托、启动初始化。

import { invoke, listen } from "./tauri.js";
import { $, toast } from "./utils.js";
import { appState, analyticsTab } from "./state.js";
import { renderPlugins, openPlugin, closePlugin, renderPluginDetail, togglePlugin, pluginBtn, pluginBtnSel, pluginField, pluginFieldStay, pluginSelectRow, pluginSelectAll, modalOpen, modalCancel, modalSubmit, modalAction } from "./plugins.js";
import { applyLive, applyCharts, renderRank, renderApps, renderDevices, deviceRename, deviceRenameSave, deviceRenameClear, deviceRenameCancel, openDeviceDetail, closeDeviceDetail, deviceDetailSetPeriod, renderAnalytics, renderTrendChart, renderSettings, doImport, doExport, doVacuum, doBackup, doWeeklyReport } from "./views.js";

export function switchView(view) {
  appState.currentView = view;
  document.querySelectorAll("#view-tabs .tab").forEach((b) => {
    const active = b.dataset.view === view;
    b.classList.toggle("active", active);
    b.setAttribute("aria-selected", String(active));
  });
  const ids = ["rank", "apps", "devices", "analytics", "plugins", "settings"];
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
    case "apps": return renderApps(appState.chartsData);
    case "devices": return renderDevices(appState.chartsData);
    case "analytics": return renderAnalytics(appState.chartsData);
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
    case "toggle-plugin": togglePlugin(d.file, d.target); break;
    case "plugin-btn": pluginBtn(d.name, d.id); break;
    case "plugin-btn-sel": pluginBtnSel(d.name, d.id, d.group || ""); break;
    case "plugin-select-row": pluginSelectRow(el.closest("tr"), d.group || "", d.rid, d.onselect || ""); break;
    case "plugin-select-all": pluginSelectAll(el, d.group || ""); break;
    case "modal-open": modalOpen(d.id); break;
    case "modal-cancel": modalCancel(d.name, d.cancel || "", d.modal); break;
    case "modal-submit": modalSubmit(d.name, d.id, d.modal); break;
    case "modal-action": modalAction(d.name, d.id, d.modal); break;
    // 设备排行：改名 / 保存 / 还原 / 取消 / 点行看详情
    case "device-rename": deviceRename(d.key); break;
    case "device-rename-save": deviceRenameSave(d.key); break;
    case "device-rename-clear": deviceRenameClear(d.key); break;
    case "device-rename-cancel": deviceRenameCancel(); break;
    case "device-detail":
      // 行内按钮/改名输入框上的点击不弹详情（改名时的输入也会冒泡到行）
      if (e.target.closest("input, button")) break;
      openDeviceDetail(d.key);
      break;
    case "device-detail-close": closeDeviceDetail(); break;
    case "device-period": deviceDetailSetPeriod(d.p); break;
    // 点遮罩空白处关闭详情（点对话框内部不关：目标不是遮罩层本身）
    case "device-detail-overlay":
      if (e.target === el) closeDeviceDetail();
      break;
    // 点击遮罩背景关闭：仅当点击目标就是遮罩层本身（与旧 if (event.target === this) 等价）
    case "modal-overlay-cancel":
      if (e.target === el) modalCancel(d.name, d.cancel || "", d.modal);
      break;
    case "do-import": doImport(); break;
    case "do-export": doExport(d.fmt); break;
    case "do-vacuum": doVacuum(); break;
    case "do-backup": doBackup(); break;
    case "do-weekly-report": doWeeklyReport(); break;
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
// Esc 隐藏主窗口到托盘（不退出程序，托盘图标仍可唤回）。
// 注册在 plugins.js 的 Esc 监听之后：弹窗关闭时会 preventDefault，这里据此跳过。
document.addEventListener("keydown", (e) => {
  if (e.key !== "Escape" || e.defaultPrevented) return;
  // 设备详情弹窗最优先关闭（与插件弹窗同规则：弹窗打开时 Esc 先关弹窗）
  const devModal = $("device-modal");
  if (devModal && devModal.style.display === "flex") {
    closeDeviceDetail();
    return;
  }
  const el = e.target;
  const tag = el && el.tagName ? el.tagName.toLowerCase() : "";
  // 输入中按 Esc 视为取消输入（先失焦），不隐藏窗口，避免编辑设置时误触
  if (tag === "input" || tag === "select" || tag === "textarea") {
    if (el.blur) el.blur();
    return;
  }
  invoke("hide_main").catch(() => {});
});

document.querySelectorAll("#trend-days .tab").forEach((b) => {
  b.addEventListener("click", () => {
    appState.trendDays = Number(b.dataset.days);
    document.querySelectorAll("#trend-days .tab").forEach((x) => {
      const active = x === b;
      x.classList.toggle("active", active);
      x.setAttribute("aria-selected", String(active));
    });
    if (appState.chartsData) renderTrendChart(appState.chartsData);
  });
});

// 活跃分析页内视图切换：改状态后重渲染（renderAnalytics 内部同步页签高亮与分块可见性）
document.querySelectorAll("#analytics-tabs .tab").forEach((b) => {
  b.addEventListener("click", () => {
    analyticsTab.value = b.dataset.at;
    if (appState.chartsData) renderAnalytics(appState.chartsData);
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
  // 玻璃模式：系统材质（Mica/Acrylic）生效时后端通知/查询开启，
  // 切换 body.glass 半透明令牌让模糊透出；材质不可用则保持不透明外观
  try {
    document.body.classList.toggle("glass", !!(await invoke("get_vibrancy")));
  } catch (e) {}
  try {
    await listen("vibrancy-on", () => document.body.classList.add("glass"));
  } catch (e) {}
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
    // 悬浮窗可见性在托盘菜单 / 「立即显示·隐藏」按钮 / 设置页勾选任一入口改变时，
    // 同步那一页的勾选。少了这一步，从托盘切了悬浮窗之后设置页的勾还是旧的
    // （现在配置会被这些入口一并写回，所以勾与现实不一致是看得见的矛盾）。
    await listen("floating-changed", (e) => {
      const cb = $("set-floating");
      if (cb) cb.checked = !!e.payload;
    });
    // 久坐提醒（[rest]，判定在 core 的 RestMonitor）：顶部浮层几秒后自动消失
    await listen("rest-reminder", (e) => {
      const n = e.payload || {};
      const w = Number(n.window_minutes) || 0;
      const c = Number(n.events_in_window) || 0;
      const s = Number(n.rest_seconds) || 0;
      toast(`久坐提醒：最近 ${w} 分钟键鼠 ${c} 次，起来活动 ${s} 秒`);
    });
  } catch (e) {
    console.error("事件订阅失败", e);
  }
  // 插件详情页自己驱动刷新：番茄钟的倒计时原先只跟着 stats-charts 走，
  // 而主窗口可见且空闲时后端把重量推送间隔设成了 u64::MAX —— 人一走开倒计时就冻住，
  // 只有再动键鼠才跳一格。renderPluginDetail 内部有代次守卫与内容比对，
  // 内容没变不会重建 DOM，1 秒一次的代价只是"打开某个插件面板时多一次 IPC"。
  setInterval(() => {
    if (appState.currentView === "plugins" && appState.openPluginName) {
      try {
        const r = renderPluginDetail();
        if (r && typeof r.catch === "function") r.catch(() => {});
      } catch (e) {}
    }
  }, 1000);
  try {
    applyLive(await invoke("get_live"));
    applyCharts(await invoke("get_charts"));
    renderCurrentView();
  } catch (e) {
    console.error("初始加载失败", e);
  }
})();
