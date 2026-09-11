//! 展示辅助：键鼠分类与数字格式化（desktop/desktop 导出/CLI 共用）。

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
}
