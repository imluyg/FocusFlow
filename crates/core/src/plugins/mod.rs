//! 插件系统（Lua 脚本）。
//!
//! 镜像 Python 版 `plugins.py` 的插件协议，但用 Lua 脚本替代 Python：
//! - 插件是 `plugins/*.lua` 文件
//! - 声明元数据：PLUGIN_NAME / PLUGIN_DESC / PLUGIN_VERSION / PLUGIN_AUTHOR
//! - 可选函数：init() / cleanup()
//! - UI：get_view() 返回声明式 UI 描述表（宿主 Rust 渲染为 egui）
//!
//! 宿主向 Lua 环境注册 API 表 `focusflow`，插件通过它访问核心功能
//! （键鼠统计、番茄钟、定时任务、记账等）。
//!
//! 一条容易踩的约束：**顶层只能声明元数据常量与函数，不要调用 `focusflow.*`**。
//! 宿主 API 只在插件真正加载时注册，而列出插件页读元数据（PLUGIN_NAME 等）用的是
//! 另一个只带空 `focusflow` 占位表的环境 —— 那里调用会报 "attempt to call a nil
//! value"，插件直接加载失败。这是有意的：读名字这一步必须没有副作用，否则扫一次
//! 插件列表就会把调度线程拉起、把已停用插件的附属库建出来。初始化请放进 init()。

pub mod host;
pub mod manager;

pub use manager::{Discovered, DiscoveredPlugin, PluginInfo, PluginManager, PluginSource};

pub use self::Widget as WidgetT;

/// 插件 UI 描述：声明式视图，宿主渲染。
#[derive(Debug, Clone, Default)]
pub struct PluginView {
    /// 标题
    pub title: String,
    /// 控件列表
    pub widgets: Vec<Widget>,
}
/// 弹窗表单字段（modal_form 控件内使用）。
#[derive(Debug, Clone, Default)]
pub struct FormField {
    /// text | select | date
    pub kind: String,
    /// 回传给插件 set_field(field, value)
    pub field: String,
    pub label: String,
    /// 初始值（插件状态回填）
    pub value: String,
    /// select 的选项 (value, label)
    pub options: Vec<(String, String)>,
    /// 这一项改动后要重建视图（分类 → 子分类那种联动）。
    /// 与 `Widget::Select` 上的同名字段是同一个契约 —— 前端 `pluginFieldHtml`
    /// 只看这个布尔值决定发 `plugin-field` 还是 `plugin-field-stay`，
    /// 少了它弹窗里的下拉就是"改了没反应"，而插件那边一切正常。
    pub refresh: bool,
}

/// 单个图表最多画多少个点（路线图 3.2.2：三个月按天 = 90）。
///
/// 上限挂在**解析**这一头而不是渲染那一头：渲染器有好几个（插件面板、内置看板页），
/// 各夹一次就会出现"同一个视图在不同页画的点数不一样"。超出的部分按**尾部保留**——
/// 趋势图被夹掉的一定是最早那几个点，留最近这段才是读者要看的那一段。
pub const MAX_CHART_POINTS: usize = 90;

/// 声明式控件。
#[derive(Debug, Clone)]
pub enum Widget {
    /// 文本标签
    Label(String),
    /// 分组标题
    Heading(String),
    /// 只读文本行（键值对）
    KeyValue(String, String),
    /// 表格（表头 + 行）
    Table {
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
        /// 与 rows 平行的记录 id（字符串形式，如数字 id 或分类名；存在且 actions 非空时前端追加"操作"列）
        ids: Vec<String>,
        /// 行内操作按钮：(动作 id 前缀, 按钮文字)，如 ("edit_", "编辑")
        actions: Vec<(String, String)>,
        /// 行选中的分组名（同一表格内单选；不同分组独立选中），空串表示无分组
        group: String,
        /// 行选中时写入的插件字段名（联动刷新，如选中分类→刷新子分类列表）
        onselect: String,
    },
    /// 按钮
    Button {
        id: String,
        text: String,
        /// true 时前端渲染为禁用态
        disabled: bool,
        /// 非空时点击打开该 id 的弹窗（modal_form），而不是触发插件动作
        modal: Option<String>,
        /// true 时点击动作 id 会拼接表格选中的记录 id（如 id="edit_" → edit_42）
        sel: bool,
        /// sel 按钮对应的表格分组名（从该分组的选中行取 id）
        group: String,
    },
    /// 分隔线
    Separator,
    /// 滚动文本区
    TextArea(String),
    /// 文本输入框（field 用于回传给插件 set_field(field, value)）
    TextInput {
        field: String,
        label: String,
        /// 当前值（插件状态回填，避免重建后输入丢失）
        value: String,
    },
    /// 下拉选择（field 用于回传给插件 set_field）
    Select {
        field: String,
        label: String,
        value: String,
        /// (value, label) 选项
        options: Vec<(String, String)>,
        /// true 时变更后触发整页刷新（联动其他控件），false 仅写入状态
        refresh: bool,
    },
    /// 弹窗表单：按钮打开模态框，字段变更写入 set_field，提交触发 on_action(submit)
    ModalForm {
        /// 弹窗元素 id（前端 modalOpen/modalClose/button.modal 使用）
        id: String,
        title: String,
        /// 提交按钮对应的插件动作 id
        submit: String,
        submit_text: String,
        /// 取消按钮（✕/取消）触发的插件动作 id（用于重置编辑等持久状态），可为空
        cancel: String,
        /// 只读文本内容（如统计结果），非空时渲染在弹窗主体（与 fields 二选一）
        content: String,
        /// true 时渲染即打开（编辑/选择场景）
        open: bool,
        fields: Vec<FormField>,
        /// 弹窗内自定义操作按钮：(动作 id, 文字)，点击触发动作且弹窗保持打开
        buttons: Vec<(String, String)>,
        /// 弹窗主体内嵌控件（表格等，渲染在 content/fields 之后）
        widgets: Vec<Widget>,
    },
    /// 横向行容器（子控件按行排列）
    Row { children: Vec<Widget> },
    /// 分页条：「上一页」 第 x/y 页 · 共 N 条 「下一页」
    Pager {
        page: i64,
        pages: i64,
        total: i64,
        prev_id: String,
        next_id: String,
    },
    /// 图表：折线或柱状。本期只画线与柱，**不做交互**（悬停、缩放、点选都不做，
    /// 所以渲染器不给它任何点击分支：它在点击循环里必须是惰性的）。
    ///
    /// 有了它，"看趋势"这件事才不用退化成表格（路线图 3.2.2）；也是内置看板页的
    /// 趋势那一块唯一的画法 —— 内置页与插件走同一个控件、同一把纵轴刻度尺子，
    /// 否则同一天的数字在两个页面会画出不一样高。
    Chart {
        /// `line` | `bar`；写成别的（含漏写）一律按折线画，不弹错也不空着 ——
        /// 控件类型是插件作者手写的字符串，画错一档的代价应该是"照样看得出趋势"。
        kind: String,
        /// 横轴刻度文本，与 `points` 尾对齐（多出来的是前面的旧点被夹掉时一起掉的）
        labels: Vec<String>,
        /// 数值序列。空序列是合法输入（今天还没数据），渲染要画基线并写「暂无数据」，
        /// 不许留一块空白。
        points: Vec<i64>,
    },
}
