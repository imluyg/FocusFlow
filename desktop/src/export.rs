//! 导出报告（CSV / HTML），逻辑与 CLI 版一致。

use std::collections::HashMap;
use std::io::Write;

use anyhow::Context;
use chrono::Local;

use focusflow_core::format::{csv_field, fmt_thousands, html_escape};

/// 导出 CSV（带 BOM，Excel 打开中文不乱码）。
pub fn export_csv(
    path: &std::path::Path,
    total: i64,
    stats: &HashMap<String, i64>,
) -> anyhow::Result<()> {
    let mut sorted: Vec<(&String, &i64)> = stats.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(a.1));
    let now_str = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let mut out = String::new();
    out.push_str("# FocusFlow 键鼠活跃统计导出\n");
    out.push_str("# 统计周期 总计\n");
    out.push_str(&format!("# 总活跃次数 {total}\n"));
    out.push_str(&format!("# 导出时间 {now_str}\n\n"));
    out.push_str("排名,键鼠,次数,占比(%)\n");
    let mut rank = 0;
    for (key, count) in sorted {
        rank += 1;
        let percent = if total > 0 {
            format!("{:.2}", (*count as f64 / total as f64) * 100.0)
        } else {
            "0.00".to_string()
        };
        out.push_str(&format!("{rank},{},{count},{percent}\n", csv_field(key)));
    }
    let mut f =
        std::fs::File::create(path).with_context(|| format!("创建文件失败: {}", path.display()))?;
    f.write_all(b"\xef\xbb\xbf").context("写入 BOM 失败")?;
    f.write_all(out.as_bytes()).context("写入 CSV 内容失败")?;
    Ok(())
}

/// 导出 HTML 统计报告。
pub fn export_html(
    path: &std::path::Path,
    total: i64,
    stats: &HashMap<String, i64>,
) -> anyhow::Result<()> {
    let mut sorted: Vec<(&String, &i64)> = stats.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(a.1));
    let now_str = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let mut rows = String::new();
    let mut rank = 0;
    for (key, count) in sorted {
        rank += 1;
        let percent = if total > 0 {
            format!("{:.2}%", (*count as f64 / total as f64) * 100.0)
        } else {
            "0.00%".to_string()
        };
        let bar_width = if total > 0 {
            (*count as f64 / total as f64) * 100.0
        } else {
            0.0
        };
        rows.push_str(&format!(
            r#"<tr><td class="rank">{rank}</td><td class="key">{}</td><td class="count">{}</td><td class="percent"><div class="bar-container"><div class="bar" style="width:{bar_width:.1}%"></div><span>{percent}</span></div></td></tr>"#,
            html_escape(key),
            fmt_thousands(*count)
        ));
    }
    let html = format!(
        r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="UTF-8">
<title>FocusFlow 活跃统计报告</title>
<style>
    body {{ font-family: "Segoe UI", "Microsoft YaHei", sans-serif; margin: 40px; background: #f5f5f5; }}
    .container {{ max-width: 800px; margin: 0 auto; background: white; padding: 30px; border-radius: 8px; }}
    h1 {{ color: #0078d4; }}
    .meta {{ color: #666; margin-bottom: 20px; }}
    .total {{ font-size: 28px; font-weight: bold; color: #0078d4; }}
    table {{ width: 100%; border-collapse: collapse; margin-top: 20px; }}
    th {{ background: #0078d4; color: white; padding: 12px; }}
    td {{ padding: 10px; border-bottom: 1px solid #eee; }}
    .bar-container {{ position: relative; min-width: 200px; }}
    .bar {{ background: #0078d4; height: 20px; border-radius: 3px; opacity: 0.3; }}
    .bar-container span {{ position: absolute; left: 8px; top: 2px; }}
</style>
</head>
<body>
<div class="container">
    <h1>FocusFlow 活跃统计报告</h1>
    <div class="meta"><div>统计周期：总计</div><div>导出时间：{now_str}</div></div>
    <div class="total">总活跃次数：{}</div>
    <table>
        <thead><tr><th>排名</th><th>键鼠</th><th>次数</th><th>占比</th></tr></thead>
        <tbody>{rows}</tbody>
    </table>
</div>
</body>
</html>"#,
        fmt_thousands(total)
    );
    std::fs::write(path, html).with_context(|| format!("写入文件失败: {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn sample_stats() -> HashMap<String, i64> {
        let mut m = HashMap::new();
        m.insert("A".to_string(), 40);
        m.insert("B".to_string(), 10);
        m.insert("鼠标左键".to_string(), 30);
        m
    }

    #[test]
    fn export_csv_has_bom_header_and_rows() -> anyhow::Result<()> {
        let dir = std::env::temp_dir().join("ff_export_test");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("out.csv");
        let _ = std::fs::remove_file(&path);

        export_csv(&path, 80, &sample_stats()).expect("export_csv should succeed");
        let data = std::fs::read(&path)?;
        // 带 UTF-8 BOM
        assert_eq!(&data[0..3], b"\xef\xbb\xbf", "CSV should have BOM");
        let text = String::from_utf8_lossy(&data[3..]);
        assert!(
            text.contains("总活跃次数 80"),
            "should contain total: {text}"
        );
        assert!(
            text.contains("排名,键鼠,次数,占比(%)"),
            "should contain header"
        );
        // 排行按次数降序：A(40) 应排第一
        let a_pos = text.find("1,A,40").expect("rank1 A 40");
        assert!(a_pos > 0);
        assert!(text.contains("鼠标左键"), "should contain中文键名");
        std::fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    #[test]
    fn export_csv_escapes_special_fields() {
        let mut stats = HashMap::new();
        stats.insert("a,b".to_string(), 5);
        stats.insert("=cmd()".to_string(), 3);
        let dir = std::env::temp_dir().join("ff_export_test_esc");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("esc.csv");
        export_csv(&path, 8, &stats).expect("export_csv should succeed");
        let text = std::fs::read_to_string(&path).expect("read csv");
        assert!(text.contains("\"a,b\""), "逗号字段应加引号: {text}");
        assert!(text.contains("'=cmd()"), "公式注入应加前导引号: {text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn export_html_contains_stats() -> anyhow::Result<()> {
        let dir = std::env::temp_dir().join("ff_export_test2");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("out.html");
        let _ = std::fs::remove_file(&path);

        export_html(&path, 80, &sample_stats()).expect("export_html should succeed");
        let html = std::fs::read_to_string(&path)?;
        assert!(html.contains("总活跃次数：80"), "should contain total");
        assert!(html.contains("<html"), "should be html");
        assert!(html.contains("50.00%"), "A 占比 40/80 = 50.00%");
        assert!(html.contains("12.50%"), "B 占比 10/80 = 12.50%");
        std::fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    #[test]
    fn export_html_escapes_key_names() {
        let mut stats = HashMap::new();
        stats.insert("<b>x&y</b>".to_string(), 1);
        let dir = std::env::temp_dir().join("ff_export_test_esc2");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("esc.html");
        export_html(&path, 1, &stats).expect("export_html should succeed");
        let html = std::fs::read_to_string(&path).expect("read html");
        assert!(
            html.contains("&lt;b&gt;x&amp;y&lt;/b&gt;"),
            "HTML 键名应转义"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
