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

/// `KEY_GROUPS` 里属于"鼠标"的那两个分组名。
///
/// 桌面端的"鼠标 vs 键盘"拆分（`state.rs` 的卡片与图表）和周报导出的行筛选都靠它，
/// 原来三处各写一遍 `"鼠标点击"`/`"滚轮"` 字面量 ⇒ `classify_key` 一改产出名，
/// 鼠标那一路就静默拿到 0（键盘＝总数），数字看着完全正常、没有任何用例报错。
pub const MOUSE_GROUPS: [&str; 2] = ["鼠标点击", "滚轮"];

/// 排名比较器：**次数降序，同次数按名字升序**。全仓唯一一份。
///
/// 为什么必须在 core 而不是桌面侧：用它的有四个地方 —— 屏幕上的键鼠榜与应用榜
/// （`desktop/src/state.rs`）、桌面导出的 CSV / HTML / 周报 Top 10 / 应用 Top 5
/// （`desktop/src/export.rs`）、以及 **CLI 自己的四个排名与两份导出器**
/// （`crates/cli/src/main.rs`）。放在 desktop 里 CLI 就够不着，而"同一把尺子抄两个
/// crate 各写一遍"正是本函数要防的事 —— 上面模块头记的就是同一族的上一次事故
/// （CLI 与 GUI 各维护一套导出转义，CLI 那套漏了转义）。
///
/// 少了"同分按名字"这一半会怎样：这些榜的输入全部来自 `HashMap`，`iter().collect()`
/// 出来的序每进程都不一样（SipHash 随机种子），而 `sort_by` 是稳定排序 —— 它只是把
/// 那份随机原样留在结果里。紧跟在排序后面的是 `truncate` / `take(n)` /
/// `if rank >= N { break }`，于是随机性不止"同分的先后换了"，而是**谁进榜**都换。
/// 屏幕上那次（他真实库 113 个键、第 99~102 名并列 2 次正好被 100 那条线切开）已经
/// 因此修过一遍，导出与 CLI 是漏掉的另一半。
pub fn cmp_rank_desc(
    name_a: &str,
    count_a: &i64,
    name_b: &str,
    count_b: &i64,
) -> std::cmp::Ordering {
    count_b
        .cmp(count_a)
        .then_with(|| name_a.as_bytes().cmp(name_b.as_bytes()))
}

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

    /// 排名比较器：次数降序 + **同分按名字升序**，而且结果不许依赖输入顺序。
    ///
    /// 这条要钉的是这样一类事故：榜的输入是 `HashMap`，排序只比次数，而排序后面就跟着
    /// 截断 —— 屏幕上（`state.rs`，他真实库第 99~102 名并列被 100 那条线切开）、
    /// 桌面导出四处、CLI 四处，同一个形状一共栽过三回。所以断言写成
    /// "把输入倒过来喂，取出的榜必须逐位相同"，而不是"看起来有序"。
    #[test]
    fn rank_comparator_is_total_and_independent_of_input_order() {
        const TIED: [&str; 12] = [
            "k00", "k01", "k02", "k03", "k04", "k05", "k06", "k07", "k08", "k09", "k10", "k11",
        ];
        let build = |reverse: bool| -> Vec<(String, i64)> {
            let mut v: Vec<(String, i64)> = vec![("zzz-big".to_string(), 90)];
            for n in TIED {
                v.push((n.to_string(), 10));
            }
            if reverse {
                v.reverse();
            }
            v
        };
        let rank8 = |v: &mut Vec<(String, i64)>| -> Vec<String> {
            v.sort_by(|a, b| cmp_rank_desc(&a.0, &a.1, &b.0, &b.1));
            v.iter().take(8).map(|(k, _)| k.clone()).collect()
        };

        let mut fwd = build(false);
        let mut rev = build(true);
        let (a, b) = (rank8(&mut fwd), rank8(&mut rev));
        assert_eq!(a, b, "同一份数据换个输入序就换榜 ⇒ 截断出来的是随机的");
        assert_eq!(a[0], "zzz-big", "次数降序这一半不能丢");
        assert_eq!(
            &a[1..],
            &["k00", "k01", "k02", "k03", "k04", "k05", "k06"],
            "并列那组里进榜的必须是名字最小的 7 个"
        );
        // 反向断言：把 `.then_with` 那半截掉，这条就会红（同分先后的随序留进结果里）
        assert!(
            !a.iter().any(|k| k == "k11"),
            "名字最大的那个并列项不该挤进榜：{a:?}"
        );
    }

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

    /// 分组名只许有一处真相：`classify_key` 的产出、`KEY_GROUPS` 的清单、
    /// `MOUSE_GROUPS` 的子集必须互相对得上。
    ///
    /// 回归：桌面端"鼠标 vs 键盘"（`state.rs` 的卡片/图表）与周报导出的行筛选
    /// （`export.rs`）原来各写一遍 `"鼠标点击"`/`"滚轮"` 字面量 ⇒ `classify_key` 一改
    /// 产出名，鼠标那一路就静默拿到 0（键盘＝总数，看着完全正常，没有用例会红）。
    #[test]
    fn group_names_stay_one_source_for_the_mouse_keyboard_split() {
        for g in MOUSE_GROUPS {
            assert!(
                KEY_GROUPS.contains(&g),
                "鼠标分组 {g:?} 不在 KEY_GROUPS 里 ⇒ 桌面端按 KEY_GROUPS 渲染时它会消失"
            );
        }
        for raw in [
            "a",
            "7",
            "F12",
            "左Shift",
            "↑",
            "鼠标左键",
            "滚轮下",
            "奇怪名字",
        ] {
            let g = classify_key(raw);
            assert!(KEY_GROUPS.contains(&g), "{raw:?} → {g:?} 落不进 KEY_GROUPS");
            assert_eq!(
                MOUSE_GROUPS.contains(&g),
                raw.starts_with("鼠标") || raw.starts_with("滚轮"),
                "{raw:?} 的鼠标/键盘归属与桌面算法不一致 ⇒ 鼠标拿 0 就等于键盘＝总数"
            );
        }
    }
}
