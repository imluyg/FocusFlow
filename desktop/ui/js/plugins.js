// 插件生态：插件列表、详情页、Widget 渲染、表格选中、弹窗、动作与字段回写。

import { invoke } from "./tauri.js";
import { $, escapeHtml, toast } from "./utils.js";
import { appState, openModals } from "./state.js";

// ===== 插件最近使用记录（localStorage 持久化，用于列表排序）=====
const LAST_USED_KEY = "ff_plugin_last_used";

// 插件详情视图数据指纹（renderPluginDetail 增量刷新判断依据）
let lastViewFingerprint = "";

function loadLastUsed() {
  try {
    return JSON.parse(localStorage.getItem(LAST_USED_KEY)) || {};
  } catch (e) {
    return {};
  }
}

function markPluginUsed(name) {
  try {
    const m = loadLastUsed();
    m[name] = Date.now();
    localStorage.setItem(LAST_USED_KEY, JSON.stringify(m));
  } catch (e) {
    // 存储不可用（如隐私模式）：仅影响排序，不影响功能
  }
}

export async function renderPlugins() {
  if (appState.openPluginName) {
    renderPluginDetail();
    return;
  }
  try {
    const plugins = await invoke("get_plugins");
    const box = $("view-plugins");
    if (!plugins || plugins.length === 0) {
      box.innerHTML = '<div class="empty">暂无插件</div>';
      return;
    }
    // 最近使用的排前面；都没用过的按名称排序保证列表稳定
    const lastUsed = loadLastUsed();
    plugins.sort(
      (a, b) =>
        (lastUsed[b.name] || 0) - (lastUsed[a.name] || 0) ||
        a.name.localeCompare(b.name, "zh-Hans-CN")
    );
    box.innerHTML = plugins
      .map((p) => {
        const file = escapeHtml(p.file || "");
        // 停用态：整行灰化，且不提供"打开"（已卸载，打开必然失败）
        const toggle = `<button class="btn ghost" data-act="toggle-plugin" data-file="${file}" data-target="${p.enabled ? 0 : 1}">${p.enabled ? "停用" : "启用"}</button>`;
        const open = p.enabled
          ? `<button class="btn ghost" data-act="open-plugin" data-name="${escapeHtml(p.name)}">打开</button>`
          : "";
        const err = p.enabled && p.error
          ? `<div class="desc" style="color:var(--danger);">加载失败：${escapeHtml(p.error)}</div>`
          : "";
        return `<div class="plugin-row${p.enabled ? "" : " disabled"}">
            <div class="info">
              <div class="name">${escapeHtml(p.name)}${p.enabled ? "" : ' <span style="color:var(--muted);font-weight:400;">（已停用）</span>'}</div>
              <div class="desc">${escapeHtml(p.desc || "")}</div>${err}
            </div>
            <div style="display:flex;align-items:center;gap:10px;">
              <span style="font-size:13px;color:var(--muted);">v${escapeHtml(p.version || "")} · ${escapeHtml(p.author || "")}</span>
              ${toggle}${open}
            </div>
          </div>`;
      })
      .join("");
  } catch (e) {
    $("view-plugins").innerHTML = '<div class="empty">插件加载失败</div>';
  }
}

// 启用/停用插件：停用会卸载实例并调用 cleanup，之后不再加载、不接收键事件。
// 开关作用在文件名上（插件改名不影响配置）。
export async function togglePlugin(file, enabled) {
  const want = enabled === true || enabled === "1";
  try {
    await invoke("set_plugin_enabled", { file, enabled: want });
    toast(want ? "插件已启用" : "插件已停用");
  } catch (e) {
    toast("操作失败: " + e);
  }
  await renderPlugins();
}

export function openPlugin(name) {
  markPluginUsed(name);
  appState.openPluginName = name;
  lastViewFingerprint = ""; // 切换插件：指纹失效，强制重建
  renderPluginDetail();
}

export function closePlugin() {
  appState.openPluginName = null;
  lastViewFingerprint = "";
  openModals.clear(); // 离开插件页：清空弹窗打开状态，避免重开时旧弹窗自动弹出
  appState.pluginSelIds = { __main: new Set() };
  renderPlugins();
}

export async function renderPluginDetail(force) {
  const seq = ++appState.pluginDetailSeq;
  const box = $("view-plugins");
  const name = appState.openPluginName;
  let view;
  try {
    view = await invoke("get_plugin_view", { name });
  } catch (e) {
    if (seq !== appState.pluginDetailSeq) return;
    lastViewFingerprint = "";
    box.innerHTML = `<button class="btn ghost" data-act="close-plugin">返回</button><div class="empty">加载失败: ${escapeHtml(e)}</div>`;
    return;
  }
  // 已有更新的请求在途：丢弃本次陈旧响应（防乱序覆盖）
  if (seq !== appState.pluginDetailSeq) return;
  if (!view) {
    lastViewFingerprint = "";
    box.innerHTML = `<button class="btn ghost" data-act="close-plugin">返回</button><div class="empty">插件未提供视图</div>`;
    return;
  }
  let html = `<div style="margin-bottom:8px;"><button class="btn ghost" data-act="close-plugin">← 返回插件列表</button>
    <span style="margin-left:10px;font-weight:700;font-size:16px;">${escapeHtml(view.title || name)}</span></div><hr>`;
  for (const w of view.widgets) {
    html += renderWidget(w);
  }
  // 数据指纹比对：view 数据未变化则不重建 DOM，保留正在输入的内容与焦点
  //（每 2s 图表推送也会触发刷新）。指纹取后端 view 的序列化串，
  // 不受行选中高亮等运行时 class 修改与浏览器 innerHTML 序列化差异影响。
  const prevSel = { ...appState.pluginSelIds };
  const fingerprint = JSON.stringify(view);
  if (fingerprint === lastViewFingerprint) {
    restoreSelClasses(box, prevSel);
    return;
  }
  // 正在输入（焦点在框内输入控件）：跳过本次重建，失焦后下次刷新再更新。
  // 注意：指纹必须在**真正重建之后**才写回。此前在这里就写入，导致被跳过的
  // 这一次更新永久丢失（下次推送内容相同 → 指纹命中 → 一直走 skip 分支），
  // 插件详情页会停留在旧数据上直到后端数据再次变化。
  // force=true（按钮动作/联动刷新）时强制重建，否则下拉联动会失效。
  if (!force) {
    const ae = document.activeElement;
    if (ae && box.contains(ae) && (ae.tagName === "INPUT" || ae.tagName === "SELECT" || ae.tagName === "TEXTAREA")) {
      return;
    }
  }
  // force 重建会把整个子树换掉，正在输入的那个框也跟着变成新节点 → 焦点掉到
  // <body>。记账的关键词框就是这里栽的：`change` 事件在按 Enter 时才触发，
  // 于是"输入关键词 → Enter"= 写回插件状态 + 强制重建，文本还在但焦点没了，
  // 后面的按键全打在空气里。按 data-field 认出同一个控件，重建后把焦点和
  // 光标位置放回去。
  const ae2 = document.activeElement;
  const keepField =
    force && ae2 && box.contains(ae2) && ae2.dataset && ae2.dataset.field
      ? ae2.dataset.field
      : null;
  const keepFocus = keepField
    ? {
        field: keepField,
        // 同一个 `data-field` 在视图里**可能有好几份**：记账的 `form_fields` 被
        // add_modal 与 edit_modal 共用（accounting_plugin.lua:434/446），于是
        // `d_category` 有两组控件。以前只按名字取第一个 → 用户在「修改记录」弹窗里
        // 换分类时，焦点被放回那个**隐藏的「新增」弹窗**的框上（等于没恢复，
        // 后续按键还是打在空气里）。所以记下"焦点元素在同名字段里排第几"，
        // 重建后放回同一个序号。
        index: [
          ...box.querySelectorAll(`[data-field="${cssEscape(keepField)}"]`),
        ].indexOf(ae2),
        start: typeof ae2.selectionStart === "number" ? ae2.selectionStart : null,
      }
    : null;
  box.innerHTML = html;
  lastViewFingerprint = fingerprint;
  restoreSelClasses(box, prevSel); // 重建后恢复选中状态（高亮 + 集合）
  if (keepFocus) {
    const same = [
      ...box.querySelectorAll(`[data-field="${cssEscape(keepFocus.field)}"]`),
    ];
    const next = same[keepFocus.index] || same[0];
    if (next) {
      next.focus();
      if (keepFocus.start !== null && typeof next.setSelectionRange === "function") {
        try {
          next.setSelectionRange(keepFocus.start, keepFocus.start);
        } catch (_) {
          /* select 之类没有选区，忽略 */
        }
      }
    }
  }
}

/// 选择器里的字符串转义（CSS.escape 在旧 WebView2 上可能没有）
function cssEscape(s) {
  return window.CSS && CSS.escape ? CSS.escape(String(s)) : String(s).replace(/["\\]/g, "\\$&");
}

// 重建后恢复表格选中行：按保存的分组集合重新加高亮 class 并同步选中集合
// 注意：无分组表格（记账记录）的行 data-group 为空串，而集合键是 "__main"
function restoreSelClasses(box, groups) {
  const next = {};
  for (const g of Object.keys(groups || {})) {
    const set = new Set();
    const attrGroup = g === "__main" ? "" : g;
    (groups[g] || []).forEach((id) => {
      // 按 dataset 原始值匹配，避免把插件提供的 id 拼进选择器（引号会抛异常）
      const tr = [...box.querySelectorAll("tr[data-rid]")].find(
        (t) => t.dataset.rid === String(id) && (t.dataset.group || "") === attrGroup
      );
      if (tr) {
        tr.classList.add("sel");
        set.add(id);
        const cb = tr.querySelector("input.plugin-check");
        if (cb) cb.checked = true;
      }
    });
    next[g] = set;
  }
  appState.pluginSelIds = next;
  Object.keys(next).forEach((g) => updateCheckAll(g === "__main" ? "" : g));
}

export function renderWidget(w) {
  switch (w.kind) {
    case "label":
      return `<p>${escapeHtml(w.text)}</p>`;
    case "heading":
      return `<h3 style="color:var(--accent);margin:12px 0 4px;">${escapeHtml(w.text)}</h3>`;
    case "keyvalue":
      return `<div class="setting-row"><span class="lbl">${escapeHtml(w.key)}</span><span style="font-weight:600;">${escapeHtml(w.value)}</span></div>`;
    case "table": {
      const hasActions = (w.ids && w.ids.length && w.actions && w.actions.length);
      const selectable = (w.ids && w.ids.length);
      const grpAttr = escapeHtml(w.group || "");
      // 复选框列：仅无分组表格（多选场景，如记账记录）显示；分组表格（单选，如分类管理）不显示
      const showCheck = selectable && !w.group;
      const thead = (w.headers || []).map((h) => `<th>${escapeHtml(h)}</th>`).join("");
      const tbody = (w.rows || []).map((r, ri) => {
        const cells = r.map((c) => `<td>${escapeHtml(c)}</td>`).join("");
        let actionTd = "";
        if (hasActions) {
          const id = w.ids[ri];
          // 动作按钮带 data-act：事件委托按"最近祖先"分发，内层按钮优先于行的选中处理，
          // 天然等效于旧内联 onclick 的 stopPropagation
          const btns = w.actions.map((a) => `<button class="btn ghost mini" data-act="plugin-btn" data-name="${escapeHtml(appState.openPluginName)}" data-id="${escapeHtml(String(a.prefix) + String(id))}">${escapeHtml(a.text)}</button>`).join("");
          actionTd = `<td class="row-actions">${btns}</td>`;
        }
        // 可选中行：首列复选框 + 点击行切换选中（配合顶部 sel 按钮做修改/删除/距今）。
        // 高亮只由 pluginSelectRow 运行时添加 class，不写进模板；
        // 重建判断已改为数据指纹比对，不受运行时 class 影响。
        // 参数一律放 data-* 属性（escapeHtml 即安全），由事件委托分发，不拼接内联 JS。
        let rowAttrs = "";
        let cbTd = "";
        if (selectable) {
          const rawId = w.ids[ri];
          const selAttrs = ` data-act="plugin-select-row" data-group="${escapeHtml(w.group || "")}" data-rid="${escapeHtml(String(rawId))}" data-onselect="${escapeHtml(w.onselect || "")}"`;
          rowAttrs = ` data-rid="${escapeHtml(String(rawId))}" data-group="${grpAttr}"${selAttrs}`;
          if (showCheck) {
            cbTd = `<td class="col-check"><input type="checkbox" class="plugin-check"${selAttrs}></td>`;
          }
        }
        return `<tr${rowAttrs}>${cbTd}${cells}${actionTd}</tr>`;
      }).join("");
      const headCheck = showCheck
        ? `<th class="col-check"><input type="checkbox" class="plugin-check-all" data-act="plugin-select-all" data-group="${grpAttr}"></th>`
        : "";
      return `<table class="grid" style="margin:8px 0;"><thead><tr>${headCheck}${thead}${hasActions ? "<th>操作</th>" : ""}</tr></thead><tbody>${tbody}</tbody></table>`;
    }
    case "button": {
      const attrs = w.modal
        ? `data-act="modal-open" data-id="${escapeHtml(w.modal)}"`
        : w.sel
          ? `data-act="plugin-btn-sel" data-name="${escapeHtml(appState.openPluginName)}" data-id="${escapeHtml(w.id)}" data-group="${escapeHtml(w.group || "")}"`
          : `data-act="plugin-btn" data-name="${escapeHtml(appState.openPluginName)}" data-id="${escapeHtml(w.id)}"`;
      return `<div style="margin:6px 0;"><button class="btn" ${w.disabled ? "disabled" : ""} ${attrs}>${escapeHtml(w.text)}</button></div>`;
    }
    case "separator":
      return `<hr>`;
    case "textarea":
      return `<pre style="background:var(--accent-soft);padding:8px;border-radius:8px;white-space:pre-wrap;font-family:inherit;">${escapeHtml(w.text)}</pre>`;
    case "textinput":
      return `<div class="setting-row"><span class="lbl">${escapeHtml(w.label || w.text || "")}</span><input type="text" value="${escapeHtml(w.value || "")}" data-act-change="plugin-field" data-name="${escapeHtml(appState.openPluginName)}" data-field="${escapeHtml(w.field)}"></div>`;
    case "select":
      return `<div class="setting-row"><span class="lbl">${escapeHtml(w.label || w.text || "")}</span><select data-act-change="${w.refresh ? "plugin-field" : "plugin-field-stay"}" data-name="${escapeHtml(appState.openPluginName)}" data-field="${escapeHtml(w.field)}">${(w.options || []).map((o) => `<option value="${escapeHtml(o.value)}" ${String(o.value) === String(w.value) ? "selected" : ""}>${escapeHtml(o.label)}</option>`).join("")}</select></div>`;
    case "modal_form": {
      // 弹窗表单：按钮（可选）打开模态框；字段变更写入插件状态（不重建页面），提交触发插件动作
      const mid = w.id || "pf-modal-" + (w.field || Math.random().toString(36).slice(2, 8));
      const midAttr = escapeHtml(mid);
      const fields = (w.fields || []).map((f) => pluginFieldHtml(f)).join("");
      const innerWidgets = (w.children || []).map((c) => renderWidget(c)).join("");
      const bodyHtml = (w.content ? `<pre class="modal-content">${escapeHtml(w.content)}</pre>` : "")
        + fields
        + innerWidgets
        + ((w.actions && w.actions.length) ? `<div class="widget-row modal-actions">${w.actions.map((a) => `<button class="btn" data-act="modal-action" data-name="${escapeHtml(appState.openPluginName)}" data-id="${escapeHtml(a.prefix)}" data-modal="${midAttr}">${escapeHtml(a.text)}</button>`).join("")}</div>` : "");
      const isOpen = !!(w.open || openModals.has(mid));
      return `<div class="plugin-modal">${w.text ? `<div style="margin:6px 0;"><button class="btn" data-act="modal-open" data-id="${midAttr}">${escapeHtml(w.text)}</button></div>` : ""}
        <div class="modal-overlay" id="${midAttr}" style="display:${isOpen ? "flex" : "none"};" data-act="modal-overlay-cancel" data-name="${escapeHtml(appState.openPluginName)}" data-cancel="${escapeHtml(w.cancel || "")}" data-modal="${midAttr}">
          <div class="modal-dialog" role="dialog" aria-modal="true" aria-label="${escapeHtml(w.title || w.text || "对话框")}">
            <div class="modal-head"><span>${escapeHtml(w.title || w.text || "新增")}</span><button class="modal-close" data-act="modal-cancel" data-name="${escapeHtml(appState.openPluginName)}" data-cancel="${escapeHtml(w.cancel || "")}" data-modal="${midAttr}" aria-label="关闭">✕</button></div>
            <div class="modal-body">${bodyHtml}</div>
            <div class="modal-foot">
              <button class="btn ghost" data-act="modal-cancel" data-name="${escapeHtml(appState.openPluginName)}" data-cancel="${escapeHtml(w.cancel || "")}" data-modal="${midAttr}">取消</button>
              <button class="btn" data-act="modal-submit" data-name="${escapeHtml(appState.openPluginName)}" data-id="${escapeHtml(w.submit)}" data-modal="${midAttr}">${escapeHtml(w.submit_text || "确定")}</button>
            </div>
          </div>
        </div></div>`;
    }
    case "row":
      return `<div class="widget-row">${(w.children || []).map((c) => renderWidget(c)).join("")}</div>`;
    case "pager": {
      const page = Number(w.page || 1), pages = Number(w.pages || 1), total = Number(w.total || 0);
      return `<div class="pager"><button class="btn ghost" ${page <= 1 ? "disabled" : ""} data-act="plugin-btn" data-name="${escapeHtml(appState.openPluginName)}" data-id="${escapeHtml(w.prev)}">上一页</button>
        <span>第 ${page} / ${pages} 页 · 共 ${total} 条</span>
        <button class="btn ghost" ${page >= pages ? "disabled" : ""} data-act="plugin-btn" data-name="${escapeHtml(appState.openPluginName)}" data-id="${escapeHtml(w.next)}">下一页</button></div>`;
    }
    default:
      return "";
  }
}

// 弹窗表单字段渲染（text / select / date）
// Esc 关闭最上层的可见弹窗（与关闭按钮行为一致）
document.addEventListener("keydown", (e) => {
  if (e.key !== "Escape") return;
  // 必须用 getComputedStyle：设备详情弹窗靠 CSS 隐藏、没有内联 style，
  // 判 o.style.display !== "none" 会把它误判为可见（空串 ≠ "none"）。
  // 取末位是因为各弹窗 z-index 相同，DOM 靠后的才盖在上面。
  const overlay = [...document.querySelectorAll(".modal-overlay")]
    .filter((o) => getComputedStyle(o).display !== "none")
    .at(-1);
  const btn = overlay?.querySelector(".modal-close");
  // 本弹窗没有 .modal-close 就不认领：不 preventDefault，
  // 把 Esc 让给 main.js（它负责关设备弹窗、否则收起主窗口到托盘）。
  if (!btn) return;
  btn.click();
  // 标记已消费：main.js 的 Esc（隐藏主窗口）据此跳过，避免关弹窗时把窗口一起藏了
  e.preventDefault();
});
function pluginFieldHtml(f) {
  const label = `<span class="lbl">${escapeHtml(f.label || f.text || "")}</span>`;
  const act = f.refresh ? "plugin-field" : "plugin-field-stay";
  const changeAttrs = `data-act-change="${act}" data-name="${escapeHtml(appState.openPluginName)}" data-field="${escapeHtml(f.field)}"`;
  if (f.kind === "select") {
    return `<div class="setting-row">${label}<select ${changeAttrs}>${(f.options || []).map((o) => `<option value="${escapeHtml(o.value)}" ${String(o.value) === String(f.value) ? "selected" : ""}>${escapeHtml(o.label)}</option>`).join("")}</select></div>`;
  }
  if (f.kind === "date") {
    return `<div class="setting-row">${label}<input type="date" value="${escapeHtml(f.value || "")}" ${changeAttrs}></div>`;
  }
  return `<div class="setting-row">${label}<input type="text" value="${escapeHtml(f.value || "")}" ${changeAttrs}></div>`;
}

// ===== 插件表格选中（按分组独立管理；无分组时沿用全局多选）=====
function selSet(group) {
  const g = group || "__main";
  if (!appState.pluginSelIds[g]) appState.pluginSelIds[g] = new Set();
  return appState.pluginSelIds[g];
}

export function pluginSelectRow(tr, group, id, onselect) {
  const key = String(id);
  const set = selSet(group);
  const syncCb = (sel) => {
    const cb = tr.querySelector("input.plugin-check");
    if (cb) cb.checked = sel;
  };
  if (group) {
    // 有分组：单选（同组内互斥）
    if (set.has(key)) {
      set.delete(key);
      tr.classList.remove("sel");
      syncCb(false);
    } else {
      // 同组互斥：按 dataset 原始值匹配（插件提供的 group 可能含引号，不能拼进选择器）
      set.clear();
      set.add(key);
      document.querySelectorAll("tr[data-group]").forEach((t) => {
        if (t.dataset.group !== group) return;
        t.classList.remove("sel");
        const c = t.querySelector("input.plugin-check");
        if (c) c.checked = false;
      });
      tr.classList.add("sel");
      syncCb(true);
      // 联动：把选中项写入插件字段并刷新（如分类→刷新子分类列表）
      if (onselect) {
        pluginField(appState.openPluginName, onselect, key);
      }
    }
  } else {
    // 无分组：多选切换（记账记录列表）
    if (set.has(key)) {
      set.delete(key);
      tr.classList.remove("sel");
      syncCb(false);
    } else {
      set.add(key);
      tr.classList.add("sel");
      syncCb(true);
    }
  }
  updateCheckAll(group);
}

// 表头全选/取消全选（仅当前页可见行）
export function pluginSelectAll(cb, group) {
  const g = group || "";
  const rows = [...document.querySelectorAll("tr[data-group]")].filter((t) => (t.dataset.group || "") === g);
  const set = selSet(group);
  set.clear();
  rows.forEach((tr) => {
    if (cb.checked) {
      set.add(tr.dataset.rid);
      tr.classList.add("sel");
      const c = tr.querySelector("input.plugin-check");
      if (c) c.checked = true;
    } else {
      tr.classList.remove("sel");
      const c = tr.querySelector("input.plugin-check");
      if (c) c.checked = false;
    }
  });
}

// 同步表头全选复选框状态（全选时勾选）
function updateCheckAll(group) {
  const g = group || "";
  const cb = [...document.querySelectorAll(".plugin-check-all")].find((c) => (c.dataset.group || "") === g);
  if (!cb) return;
  const rows = [...document.querySelectorAll("tr[data-group]")].filter((t) => (t.dataset.group || "") === g);
  const all = rows.length > 0 && rows.every((tr) => tr.classList.contains("sel"));
  cb.checked = all;
}

// sel 按钮：动作 id 拼接选中 id 列表（如 "del_" + "42,43" → del_42,43）；
// 修改仅支持单条
export async function pluginBtnSel(name, id, group) {
  const set = selSet(group);
  if (set.size === 0) {
    toast("请先在列表中点击选中一项");
    return;
  }
  if (id === "edit_" && set.size > 1) {
    toast("修改仅支持选中一条记录");
    return;
  }
  await pluginBtn(name, id + [...set].join(","));
}

// ===== 插件弹窗 =====
export function modalOpen(id) {
  openModals.add(id);
  const m = document.getElementById(id);
  if (m) m.style.display = "flex";
}
function modalClose(id) {
  openModals.delete(id);
  const m = document.getElementById(id);
  if (m) m.style.display = "none";
}
// 表单字段变更：写入插件状态但不重建页面（保持弹窗与输入焦点不被打断）
export async function pluginFieldStay(name, field, value) {
  try {
    await invoke("plugin_set_field", { name, field, value });
  } catch (e) {
    console.error("插件输入失败", e);
  }
}
// 弹窗提交：关闭弹窗 → 触发插件动作 → 刷新视图
export async function modalSubmit(name, id, modalId) {
  modalClose(modalId);
  await pluginBtn(name, id);
}
// 弹窗内自定义按钮（如分类管理操作）：触发插件动作但弹窗保持打开
export async function modalAction(name, id, modalId) {
  await pluginBtn(name, id);
  const m = document.getElementById(modalId);
  if (m) m.style.display = "flex";
}
// 弹窗取消：关闭弹窗；若插件提供了 cancel 动作（如重置编辑状态），一并触发
export async function modalCancel(name, cancelId, modalId) {
  modalClose(modalId);
  if (cancelId) await pluginBtn(name, cancelId);
}

export async function pluginBtn(name, id) {
  try {
    await invoke("plugin_action", { name, id });
    await renderPluginDetail(true);
  } catch (e) {
    toast("插件动作失败: " + e);
  }
}

export async function pluginField(name, field, value) {
  try {
    await invoke("plugin_set_field", { name, field, value });
    await renderPluginDetail(true);
  } catch (e) {
    console.error("插件输入失败", e);
  }
}
