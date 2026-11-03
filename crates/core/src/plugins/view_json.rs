//! 插件视图的 JSON 出口（`focusflow-plugin run` 的那张嘴，方案 P2 第 18 步）。
//!
//! 为什么这份序列化住在 core 而不是工具里：`Widget` 的定义在这儿，写在这儿才能用
//! **穷尽 `match`** 把"新加一档控件忘了跟着改"变成编译错误。放在工具里那份只能靠人记得，
//! 而本仓为"新变体漏登记"吃过好几次 —— 病状不是崩，是"别的状态坏了"。
//!
//! 印的是**宿主读出来的那一份**，不是插件源文件里那段 Lua 的回声：解析时已经折过的东西
//! （图表夹到 90 点、`type` 写错整档被摘、嵌套超深被停）在这里就是折过的样子。
//! 这正是作者要在动手之前看见的那件事。
//!
//! 一处判定都不重算：口径在 [`super::manager`] 的解析里，这里只搬运。

use super::{FormField, PluginView, Widget};
use serde_json::{Map, Value};

/// 整张视图，印成给人（和 `jq`）看的那一份。
///
/// 出口只给字符串：工具的活是"把这一张贴到 stdout"，让 `serde_json` 渗进工具那一侧
/// 就等于给它多一个能自己排版的机会 —— 两处各排一份，`run` 的输出与判据核的就不是一对。
///
/// 手搭的这棵树里没有会变序列化成失败的键（无 f64、无循环引用、无非法字符），
/// 所以排版失败属于"这不可能发生"那一类：真发生了就是结构被改坏了，要让出声。
pub fn view_to_json_string(view: &PluginView) -> String {
    serde_json::to_string_pretty(&view_to_json(view))
        .expect("视图的 Value 树排不出 JSON —— 序列化侧被改成了会失败的那一种")
}

/// 整张视图（标题 + 控件树）。
pub(crate) fn view_to_json(view: &PluginView) -> Value {
    let mut map = Map::new();
    map.insert("title".to_string(), Value::String(view.title.clone()));
    map.insert(
        "widgets".to_string(),
        Value::Array(view.widgets.iter().map(widget_to_json).collect()),
    );
    Value::Object(map)
}

/// 单个控件。`type` 用解析侧那一套名字（`label` / `keyvalue` / `modal_form` / …），
/// 这样工具的 JSON 与插件作者写的 Lua 是同一本词汇表；字段名同样照解析侧。
pub fn widget_to_json(widget: &Widget) -> Value {
    let mut map = Map::new();
    match widget {
        Widget::Label(text) => {
            map.insert("type".into(), Value::String("label".into()));
            map.insert("text".into(), Value::String(text.clone()));
        }
        Widget::Heading(text) => {
            map.insert("type".into(), Value::String("heading".into()));
            map.insert("text".into(), Value::String(text.clone()));
        }
        Widget::KeyValue(key, value) => {
            map.insert("type".into(), Value::String("keyvalue".into()));
            map.insert("key".into(), Value::String(key.clone()));
            map.insert("value".into(), Value::String(value.clone()));
        }
        Widget::Separator => {
            map.insert("type".into(), Value::String("separator".into()));
        }
        Widget::TextArea(text) => {
            map.insert("type".into(), Value::String("textarea".into()));
            map.insert("text".into(), Value::String(text.clone()));
        }
        Widget::TextInput {
            field,
            label,
            value,
        } => {
            map.insert("type".into(), Value::String("textinput".into()));
            map.insert("field".into(), Value::String(field.clone()));
            map.insert("label".into(), Value::String(label.clone()));
            map.insert("value".into(), Value::String(value.clone()));
        }
        Widget::Button {
            id,
            text,
            disabled,
            modal,
            sel,
            group,
        } => {
            map.insert("type".into(), Value::String("button".into()));
            map.insert("id".into(), Value::String(id.clone()));
            map.insert("text".into(), Value::String(text.clone()));
            map.insert("disabled".into(), Value::Bool(*disabled));
            map.insert(
                "modal".into(),
                match modal {
                    Some(m) => Value::String(m.clone()),
                    None => Value::Null,
                },
            );
            map.insert("sel".into(), Value::Bool(*sel));
            map.insert("group".into(), Value::String(group.clone()));
        }
        Widget::Select {
            field,
            label,
            value,
            options,
            refresh,
        } => {
            map.insert("type".into(), Value::String("select".into()));
            map.insert("field".into(), Value::String(field.clone()));
            map.insert("label".into(), Value::String(label.clone()));
            map.insert("value".into(), Value::String(value.clone()));
            map.insert("options".into(), options_to_json(options));
            map.insert("refresh".into(), Value::Bool(*refresh));
        }
        Widget::ModalForm {
            id,
            title,
            submit,
            submit_text,
            cancel,
            content,
            open,
            fields,
            buttons,
            widgets,
        } => {
            map.insert("type".into(), Value::String("modal_form".into()));
            map.insert("id".into(), Value::String(id.clone()));
            map.insert("title".into(), Value::String(title.clone()));
            map.insert("submit".into(), Value::String(submit.clone()));
            map.insert("submit_text".into(), Value::String(submit_text.clone()));
            map.insert("cancel".into(), Value::String(cancel.clone()));
            map.insert("content".into(), Value::String(content.clone()));
            map.insert("open".into(), Value::Bool(*open));
            map.insert(
                "fields".into(),
                Value::Array(fields.iter().map(field_to_json).collect()),
            );
            map.insert(
                "buttons".into(),
                Value::Array(
                    buttons
                        .iter()
                        .map(|(action, text)| {
                            let mut b = Map::new();
                            b.insert("id".into(), Value::String(action.clone()));
                            b.insert("text".into(), Value::String(text.clone()));
                            Value::Object(b)
                        })
                        .collect(),
                ),
            );
            map.insert(
                "widgets".into(),
                Value::Array(widgets.iter().map(widget_to_json).collect()),
            );
        }
        Widget::Row { children } => {
            map.insert("type".into(), Value::String("row".into()));
            map.insert(
                "children".into(),
                Value::Array(children.iter().map(widget_to_json).collect()),
            );
        }
        Widget::Pager {
            page,
            pages,
            total,
            prev_id,
            next_id,
        } => {
            map.insert("type".into(), Value::String("pager".into()));
            map.insert("page".into(), Value::from(*page));
            map.insert("pages".into(), Value::from(*pages));
            map.insert("total".into(), Value::from(*total));
            // 字段名照解析侧：插件写的是 `prev` / `next`，工具印回同名才对得上
            map.insert("prev".into(), Value::String(prev_id.clone()));
            map.insert("next".into(), Value::String(next_id.clone()));
        }
        Widget::Table {
            headers,
            rows,
            ids,
            actions,
            group,
            onselect,
        } => {
            map.insert("type".into(), Value::String("table".into()));
            map.insert(
                "headers".into(),
                Value::Array(headers.iter().map(|h| Value::String(h.clone())).collect()),
            );
            map.insert(
                "rows".into(),
                Value::Array(
                    rows.iter()
                        .map(|r| Value::Array(r.iter().map(|c| Value::String(c.clone())).collect()))
                        .collect(),
                ),
            );
            map.insert(
                "ids".into(),
                Value::Array(ids.iter().map(|i| Value::String(i.clone())).collect()),
            );
            map.insert(
                "actions".into(),
                Value::Array(
                    actions
                        .iter()
                        .map(|(prefix, text)| {
                            let mut a = Map::new();
                            a.insert("prefix".into(), Value::String(prefix.clone()));
                            a.insert("text".into(), Value::String(text.clone()));
                            Value::Object(a)
                        })
                        .collect(),
                ),
            );
            map.insert("group".into(), Value::String(group.clone()));
            map.insert("onselect".into(), Value::String(onselect.clone()));
        }
        Widget::Chart {
            kind,
            labels,
            points,
        } => {
            map.insert("type".into(), Value::String("chart".into()));
            map.insert("kind".into(), Value::String(kind.clone()));
            map.insert(
                "labels".into(),
                Value::Array(labels.iter().map(|l| Value::String(l.clone())).collect()),
            );
            map.insert(
                "points".into(),
                Value::Array(points.iter().map(|p| Value::from(*p)).collect()),
            );
        }
    }
    Value::Object(map)
}

/// 弹窗里的一个字段。
fn field_to_json(field: &FormField) -> Value {
    let mut map = Map::new();
    map.insert("kind".into(), Value::String(field.kind.clone()));
    map.insert("field".into(), Value::String(field.field.clone()));
    map.insert("label".into(), Value::String(field.label.clone()));
    map.insert("value".into(), Value::String(field.value.clone()));
    map.insert("refresh".into(), Value::Bool(field.refresh));
    map.insert("options".into(), options_to_json(&field.options));
    Value::Object(map)
}

/// `(value, label)` 选项对。印成对象数组而不是二元组：工具的输出是给人读的，
/// 而 `["A","A 的显示名"]` 那种位置依赖的写法正是"看着像反了"的来源。
fn options_to_json(options: &[(String, String)]) -> Value {
    Value::Array(
        options
            .iter()
            .map(|(value, label)| {
                let mut o = Map::new();
                o.insert("value".into(), Value::String(value.clone()));
                o.insert("label".into(), Value::String(label.clone()));
                Value::Object(o)
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::MAX_CHART_POINTS;

    /// 解析侧认的那些 `type` 名（`manager.rs` 的 `parse_widget` 那一张表）。
    ///
    /// 这份清单是**故意手抄**的：它唯一的用途是把"序列化器印出一个解析器不认的名字"
    /// 变成一条红的判据。两边同源的话这条判据就永远绿着，而病状是工具的 JSON
    /// 在插件页与工具里是两种词汇。
    const PARSER_TYPE_NAMES: &[&str] = &[
        "label",
        "heading",
        "keyvalue",
        "separator",
        "textarea",
        "textinput",
        "button",
        "select",
        "modal_form",
        "row",
        "pager",
        "table",
        "chart",
    ];

    fn all_variants() -> Vec<Widget> {
        vec![
            Widget::Label("l".into()),
            Widget::Heading("h".into()),
            Widget::KeyValue("k".into(), "v".into()),
            Widget::Separator,
            Widget::TextArea("t".into()),
            Widget::TextInput {
                field: "f".into(),
                label: "L".into(),
                value: "v".into(),
            },
            Widget::Button {
                id: "b".into(),
                text: "按钮".into(),
                disabled: true,
                modal: Some("m1".into()),
                sel: false,
                group: String::new(),
            },
            Widget::Select {
                field: "cat".into(),
                label: "分类".into(),
                value: "a".into(),
                options: vec![("a".into(), "A".into()), ("b".into(), "B".into())],
                refresh: true,
            },
            Widget::ModalForm {
                id: "m1".into(),
                title: "编辑".into(),
                submit: "save".into(),
                submit_text: "保存".into(),
                cancel: "cancel".into(),
                content: String::new(),
                open: true,
                fields: vec![FormField {
                    kind: "select".into(),
                    field: "sub".into(),
                    label: "子分类".into(),
                    value: "x".into(),
                    options: vec![("x".into(), "X".into())],
                    refresh: true,
                }],
                buttons: vec![("extra".into(), "多做一件事".into())],
                widgets: vec![Widget::Separator],
            },
            Widget::Row {
                children: vec![Widget::Label("在里面".into())],
            },
            Widget::Pager {
                page: 2,
                pages: 5,
                total: 41,
                prev_id: "prev_page".into(),
                next_id: "next_page".into(),
            },
            Widget::Table {
                headers: vec!["名".into(), "数".into()],
                rows: vec![vec!["甲".into(), "7".into()]],
                ids: vec!["42".into()],
                actions: vec![("edit_".into(), "编辑".into())],
                group: "g".into(),
                onselect: "sel".into(),
            },
            Widget::Chart {
                kind: "bar".into(),
                labels: vec!["1".into(), "2".into()],
                points: vec![3, 4],
            },
        ]
    }

    /// 十三档控件每一档都要被印出来，且 `type` 名必须是解析侧认的那一个。
    ///
    /// 这条是"新变体漏登记"的那把尺子：穷尽 `match` 只保证编译得过，
    /// 保证不了新那档印出来的名字宿主认得。
    #[test]
    fn every_widget_variant_prints_a_type_the_parser_accepts() {
        let variants = all_variants();
        assert_eq!(
            variants.len(),
            PARSER_TYPE_NAMES.len(),
            "夹具的档数与解析侧认的 `type` 名数不一样 —— 有一侧漏了一项"
        );
        let mut seen: Vec<String> = Vec::new();
        for widget in &variants {
            let json = widget_to_json(widget);
            let got = json
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("{json:?} 没有 type 那一句"))
                .to_string();
            assert!(
                PARSER_TYPE_NAMES.contains(&got.as_str()),
                "序列化器印出 {got} —— 解析侧不认这个名字，工具的 JSON 与插件页是两套词汇"
            );
            seen.push(got);
        }
        for name in PARSER_TYPE_NAMES {
            assert!(
                seen.iter().any(|s| s == name),
                "{name} 那一档没被夹具覆盖：新加控件时这条该红，而不是静悄悄少一档"
            );
        }
    }

    /// 嵌套要跟着走：`row` 里的子控件、弹窗里的控件与字段，都得在 JSON 里看得见。
    ///
    /// 少一层的话症状是"工具说视图是空的，页面上却画得出来"。
    #[test]
    fn nested_children_survive_into_the_json() {
        let variants = all_variants();
        let row = widget_to_json(&variants[9]);
        assert_eq!(
            row.get("children")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0),
            1,
            "row 里的子控件没了"
        );
        let modal = widget_to_json(&variants[8]);
        assert_eq!(
            modal
                .get("widgets")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0),
            1,
            "弹窗内嵌控件没了"
        );
        let field = modal
            .get("fields")
            .and_then(Value::as_array)
            .and_then(|a| a.first().cloned())
            .expect("弹窗字段没了");
        assert_eq!(
            field.get("options").and_then(Value::as_array).map(Vec::len),
            Some(1),
            "字段里的选项没了 —— 下拉在 JSON 里变成空的就是这一处"
        );
        assert_eq!(field.get("refresh").and_then(Value::as_bool), Some(true));
    }

    /// 分页那档的字段名照解析侧写回 `prev` / `next`，不是 Rust 结构体里的 `prev_id`。
    ///
    /// 这条专门盯"结构体改了名、工具的词汇跟着改了"那种漂移：作者照着工具的 JSON
    /// 回写插件，宿主读不出来。
    #[test]
    fn the_pager_names_follow_the_parser_not_the_struct() {
        let variants = all_variants();
        let pager = widget_to_json(&variants[10]);
        assert_eq!(
            pager.get("prev").and_then(Value::as_str),
            Some("prev_page"),
            "分页的 prev 没按解析侧那一个名字印"
        );
        assert_eq!(pager.get("next").and_then(Value::as_str), Some("next_page"));
        assert_eq!(pager.get("page").and_then(Value::as_i64), Some(2));
        assert_eq!(pager.get("total").and_then(Value::as_i64), Some(41));
        assert!(
            pager.get("prev_id").is_none(),
            "把 Rust 字段名当成了对外的键名"
        );
    }

    /// 图表的点数与标签一起出来，且**夹过之后**的长度就是页面上画的那个长度。
    #[test]
    fn the_chart_prints_what_was_parsed_not_what_was_written() {
        let widget = Widget::Chart {
            kind: "line".into(),
            labels: (0..MAX_CHART_POINTS).map(|i| i.to_string()).collect(),
            points: (0..MAX_CHART_POINTS).map(|i| i as i64).collect(),
        };
        let json = widget_to_json(&widget);
        assert_eq!(
            json.get("points").and_then(Value::as_array).map(Vec::len),
            Some(MAX_CHART_POINTS),
            "上限那一档没印满 —— 少印就等于换了一把尺子"
        );
        assert_eq!(
            json.get("labels").and_then(Value::as_array).map(Vec::len),
            Some(MAX_CHART_POINTS)
        );
        assert_eq!(json.get("kind").and_then(Value::as_str), Some("line"));
    }

    /// 整张视图：标题 + widgets；这是 `run` 交出去的那个顶层形状。
    #[test]
    fn the_view_carries_title_and_widgets() {
        let view = PluginView {
            title: "我的插件".into(),
            widgets: vec![Widget::Label("一行".into())],
        };
        let json = view_to_json(&view);
        assert_eq!(json.get("title").and_then(Value::as_str), Some("我的插件"));
        assert_eq!(
            json.get("widgets").and_then(Value::as_array).map(Vec::len),
            Some(1)
        );
    }
}
