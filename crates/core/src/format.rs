//! 展示辅助：键鼠分类、数字格式化与导出转义（desktop/desktop 导出/CLI 共用）。
//!
//! 导出转义必须只有一份实现：CLI 与 GUI 曾各自维护一套，CLI 那份漏了转义，
//! 键名里带逗号会让 CSV 串列、带 `<`/`&` 会污染 HTML 报告。

/// 键鼠名 → 分组名（分组统计页的 8 个固定分组）。
pub fn classify_key(key_name: &str) -> &'static str {
    if key_name.starts_with("滚轮") {
        return "滚轮";
    }
    if key_name.starts_with("鼠标") {
        return "鼠标点击";
    }
    if matches!(
        key_name,
        "Shift"
            | "左Shift"
            | "右Shift"
            | "Ctrl"
            | "左Ctrl"
            | "右Ctrl"
            | "Alt"
            | "左Alt"
            | "右Alt"
            | "Win"
            | "左Win"
            | "右Win"
    ) {
        return "修饰键";
    }
    if key_name.starts_with('F')
        && key_name.len() > 1
        && key_name[1..].chars().all(|c| c.is_ascii_digit())
    {
        return "功能键";
    }
    if key_name.len() == 1 && key_name.chars().next().unwrap().is_ascii_digit() {
        return "数字键";
    }
    if key_name.len() == 1 && key_name.chars().next().unwrap().is_ascii_alphabetic() {
        return "字母键";
    }
    if matches!(
        key_name,
        "空格"
            | "回车"
            | "退格"
            | "Tab"
            | "Esc"
            | "Delete"
            | "Insert"
            | "Home"
            | "End"
            | "PageUp"
            | "PageDown"
            | "↑"
            | "↓"
            | "←"
            | "→"
    ) {
        return "编辑键";
    }
    "其他"
}

/// 分组统计页的固定分组顺序（classify_key 的全部可能输出）。
pub const KEY_GROUPS: [&str; 8] = [
    "字母键",
    "数字键",
    "功能键",
    "修饰键",
    "编辑键",
    "鼠标点击",
    "滚轮",
    "其他",
];

/// 千分位格式化（如 1234567 → "1,234,567"，支持负数）。
pub fn fmt_thousands(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    let len = s.len();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    if n < 0 {
        format!("-{out}")
    } else {
        out
    }
}

/// CSV 字段转义：含逗号/引号/换行时加引号并将内部引号双写；
/// `=`/`+`/`-`/`@`/Tab/CR 开头加前导单引号，防止 Excel 公式注入。
///
/// 键名来自旧版导入数据与恢复文件，不是可信内部常量，必须转义后再落盘。
///
/// `-` 与 `\t` 也在 OWASP 列出的公式前缀里。对本项目 `-` 尤其不是理论风险：
/// 减号键本身就叫 `-`，`-=~` 这类组合键名会以 `-` 开头，Excel 会按公式求值
/// （如 `-2+3` → 计算结果，`=cmd()|certutil` 之类可执行外部命令）。
pub fn csv_field(s: &str) -> String {
    let escaped = if matches!(s.chars().next(), Some('=' | '+' | '-' | '@' | '\t' | '\r')) {
        format!("'{s}")
    } else {
        s.to_string()
    };
    if escaped.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", escaped.replace('"', "\"\""))
    } else {
        escaped
    }
}

/// HTML 文本/属性转义（含单引号：属性值可能用单引号包裹）。
pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_formatter() {
        assert_eq!(fmt_thousands(0), "0");
        assert_eq!(fmt_thousands(1000), "1,000");
        assert_eq!(fmt_thousands(1234567), "1,234,567");
        assert_eq!(fmt_thousands(-500), "-500");
    }

    #[test]
    fn classify_categories() {
        assert_eq!(classify_key("滚轮下滑"), "滚轮");
        assert_eq!(classify_key("鼠标左键"), "鼠标点击");
        assert_eq!(classify_key("F12"), "功能键");
        assert_eq!(classify_key("左Ctrl"), "修饰键");
        assert_eq!(classify_key("A"), "字母键");
        assert_eq!(classify_key("7"), "数字键");
        assert_eq!(classify_key("空格"), "编辑键");
        assert_eq!(classify_key("未知键"), "其他");
    }

    /// 回归：CLI 与 GUI 共用同一份转义（CLI 曾漏转义 → CSV 串列 / HTML 注入）。
    #[test]
    fn csv_field_escapes_and_blocks_formula_injection() {
        assert_eq!(csv_field("A"), "A");
        assert_eq!(csv_field("鼠标左键"), "鼠标左键");
        // 逗号/引号/换行 → 加引号；内部引号双写
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field("l1\nl2"), "\"l1\nl2\"");
        // 公式注入前缀
        assert_eq!(csv_field("=cmd()"), "'=cmd()");
        assert_eq!(csv_field("+1"), "'+1");
        assert_eq!(csv_field("@x"), "'@x");
        // `-` 与 Tab 也在 OWASP 的公式前缀里，且对本项目不是理论风险：
        // 减号键的键名就是 "-"，组合键名会是 "-=~-_+" 这类形态。
        assert_eq!(csv_field("-"), "'-");
        assert_eq!(csv_field("-=~-_+"), "'-=~-_+");
        assert_eq!(csv_field("-2+3"), "'-2+3");
        assert_eq!(csv_field("\tA"), "'\tA");
        // 注入 + 逗号同时命中：先加前导引号，再因逗号整体加引号
        assert_eq!(csv_field("=a,b"), "\"'=a,b\"");
    }

    #[test]
    fn html_escape_covers_text_and_attribute_contexts() {
        assert_eq!(html_escape("A"), "A");
        assert_eq!(
            html_escape("<b>x&y</b>"),
            "&lt;b&gt;x&amp;y&lt;/b&gt;",
            "标签与 & 必须转义"
        );
        assert_eq!(
            html_escape("\"onmouseover='x'"),
            "&quot;onmouseover=&#39;x&#39;"
        );
        assert!(
            !html_escape("'").contains('\''),
            "单引号也要转义，否则属性值可被闭合"
        );
    }
}
