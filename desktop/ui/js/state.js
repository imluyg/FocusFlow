// 跨模块共享的可变前端状态。
// ES module 的导出绑定是只读的，可变量统一挂在这个对象上读写。

export const appState = {
  /// 当前可见视图（rank/group/trend/hourly/weekday/plugins/settings）
  currentView: "rank",
  /// 趋势图天数（7/30）
  trendDays: 7,
  /// 最近一次 charts 推送快照
  chartsData: null,
  /// 正在打开的插件名（null=插件列表页）
  openPluginName: null,
  /// 插件详情页渲染请求代次：丢弃乱序返回的陈旧响应
  pluginDetailSeq: 0,
  /// 插件表格选中集合：{ group: Set<id> }，group 为空串的条目合并到 "__main"
  pluginSelIds: { __main: new Set() },
};

/// 已打开的弹窗 id 集合：页面重建（如联动刷新）后弹窗保持打开
export const openModals = new Set();

/// 键鼠排行筛选（"all" | "mouse" | "keyboard"）
export const rankFilter = { value: "all" };
