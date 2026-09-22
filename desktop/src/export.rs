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

/// Markdown 表格单元格：键名里可能带 `|` 或换行，会直接破坏表格结构。
fn md_cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\n', '\r'], " ")
}

/// 百分比（分母为 0 时给 0.0%，不做除零）。
fn pct(part: i64, total: i64) -> String {
    if total > 0 {
        format!("{:.1}%", part as f64 / total as f64 * 100.0)
    } else {
        "0.0%".to_string()
    }
}

/// 秒 → 「1小时23分」/「45分」/「30秒」。
fn fmt_duration(secs: i64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    if h > 0 {
        format!("{h}小时{m}分")
    } else if m > 0 {
        format!("{m}分")
    } else {
        format!("{secs}秒")
    }
}

/// 周报文件所在目录。
pub fn report_dir() -> std::path::PathBuf {
    focusflow_core::paths::data_dir().join("reports")
}

/// 生成并写出「上一个完整周」（周一~周日）的周报，返回文件路径。
///
/// 数据来源全是既有的按日查询：键名排行、应用时长、小时分布各自逐日取回再累加
/// （7 天 = 7 次查询）。一周只跑一次、且跑在后台线程里，所以不值得为它新增一条
/// 日期区间查询路径；按日序列（含上一周，用于环比）只需一次 get_daily_counts。
pub fn write_weekly_report() -> anyhow::Result<std::path::PathBuf> {
    use chrono::{Datelike, Local as ChLocal, NaiveDate};
    use focusflow_core::db::queries as q;

    let today = ChLocal::now().date_naive();
    let (from, to) = focusflow_core::stats::last_finished_week(today);
    let goal = focusflow_core::config::instance().get_int("goal", "daily_keys", 20000);

    // 近 21 天：够覆盖本周 + 上一周（环比用），一次查完
    let daily = q::get_daily_counts(21, None);
    let day_count = |d: NaiveDate| -> i64 {
        let key = d.format("%Y-%m-%d").to_string();
        daily
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, c)| *c)
            .unwrap_or(0)
    };

    let mut keys: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut apps: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut hourly = [0i64; 24];
    let mut rows: Vec<(NaiveDate, i64)> = Vec::new();
    let mut keyboard = 0i64;
    let mut mouse = 0i64;

    let mut day = from;
    while day <= to {
        let (total, day_keys) = q::get_stats_by_date(day);
        for (k, c) in &day_keys {
            *keys.entry(k.clone()).or_insert(0) += *c;
            if matches!(focusflow_core::format::classify_key(k), "滚轮" | "鼠标点击") {
                mouse += *c;
            } else {
                keyboard += *c;
            }
        }
        rows.push((day, total));
        let (_, day_apps) = q::get_app_stats_by_date(day);
        for (a, s) in day_apps {
            *apps.entry(a).or_insert(0) += s;
        }
        for (i, v) in q::get_hourly_stats(Some(day))
            .into_iter()
            .take(24)
            .enumerate()
        {
            hourly[i] += v;
        }
        day += chrono::Duration::days(1);
    }

    let total: i64 = rows.iter().map(|(_, c)| c).sum();
    let prev_total: i64 = (0..7)
        .map(|i| day_count(from - chrono::Duration::days(i + 1)))
        .sum();
    let best = rows.iter().max_by_key(|(_, c)| *c).copied();
    let worst = rows.iter().min_by_key(|(_, c)| *c).copied();
    let met_days = rows.iter().filter(|(_, c)| *c >= goal).count();
    let goal_state =
        focusflow_core::stats::goal_status(goal, &daily, &to.format("%Y-%m-%d").to_string());
    let app_total: i64 = apps.values().sum();
    let hour_max = *hourly.iter().max().unwrap_or(&0);

    let week_cn = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];
    let mut out = String::new();
    out.push_str(&format!(
        "# FocusFlow 周报（{} ~ {}）\n\n",
        from.format("%Y-%m-%d"),
        to.format("%Y-%m-%d")
    ));
    out.push_str(&format!(
        "- 生成时间：{}\n",
        ChLocal::now().format("%Y-%m-%d %H:%M:%S")
    ));
    let delta = if prev_total > 0 {
        format!(
            "，环比 {:+.1}%",
            (total - prev_total) as f64 / prev_total as f64 * 100.0
        )
    } else {
        String::new()
    };
    out.push_str(&format!(
        "- 总活跃次数：**{}**（上一周 {}{}）\n",
        fmt_thousands(total),
        fmt_thousands(prev_total),
        delta
    ));
    out.push_str(&format!(
        "- 键盘 {}（{}）/ 鼠标与滚轮 {}（{}）\n",
        fmt_thousands(keyboard),
        pct(keyboard, total),
        fmt_thousands(mouse),
        pct(mouse, total)
    ));
    if let (Some(b), Some(w)) = (best, worst) {
        out.push_str(&format!(
            "- 日均 {}　最高 {}（{}）　最低 {}（{}）\n",
            fmt_thousands(total / 7),
            b.0.format("%m-%d"),
            fmt_thousands(b.1),
            w.0.format("%m-%d"),
            fmt_thousands(w.1)
        ));
    }
    out.push_str(&format!(
        "- 每日目标 {}：达标 {}/7 天；截至本周日连续 {} 天（近 21 天最长 {} 天）\n\n",
        fmt_thousands(goal),
        met_days,
        goal_state.streak,
        goal_state.best
    ));

    out.push_str("## 每日\n\n| 日期 | 星期 | 次数 | 达标 |\n|---|---|---:|:--:|\n");
    for (d, c) in &rows {
        out.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            d.format("%Y-%m-%d"),
            week_cn[d.weekday().num_days_from_monday() as usize],
            fmt_thousands(*c),
            if *c >= goal { "✅" } else { "—" }
        ));
    }

    out.push_str("\n## 键鼠 Top 10\n\n| 键鼠 | 次数 | 占比 |\n|---|---:|---:|\n");
    let mut top: Vec<(&String, &i64)> = keys.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1));
    for (k, c) in top.iter().take(10) {
        out.push_str(&format!(
            "| {} | {} | {} |\n",
            md_cell(k),
            fmt_thousands(**c),
            pct(**c, total)
        ));
    }

    if !apps.is_empty() {
        out.push_str("\n## 前台应用时长 Top 5\n\n| 应用 | 时长 | 占比 |\n|---|---:|---:|\n");
        let mut at: Vec<(&String, &i64)> = apps.iter().collect();
        at.sort_by(|a, b| b.1.cmp(a.1));
        for (a, s) in at.iter().take(5) {
            out.push_str(&format!(
                "| {} | {} | {} |\n",
                md_cell(a),
                fmt_duration(**s),
                pct(**s, app_total)
            ));
        }
    }

    out.push_str("\n## 时段分布\n\n| 时段 | 次数 | 强度 |\n|---|---:|---|\n");
    for (h, c) in hourly.iter().enumerate() {
        let bars = if hour_max > 0 {
            (*c as f64 / hour_max as f64 * 20.0).round() as usize
        } else {
            0
        };
        out.push_str(&format!(
            "| {:02}:00 | {} | {} |\n",
            h,
            fmt_thousands(*c),
            "█".repeat(bars)
        ));
    }

    let dir = report_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("创建目录失败: {}", dir.display()))?;
    let path = dir.join(format!(
        "周报-{}-{}.md",
        from.format("%Y-%m-%d"),
        to.format("%Y-%m-%d")
    ));
    std::fs::write(&path, out).with_context(|| format!("写入失败: {}", path.display()))?;
    Ok(path)
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
    fn report_helpers_escape_and_format() {
        // 键名里的竖线会切断 Markdown 表格列，换行会撕掉整行
        assert_eq!(md_cell("a|b"), "a\\|b");
        assert_eq!(md_cell("a\nb"), "a b");
        assert_eq!(md_cell("鼠标左键"), "鼠标左键");
        assert_eq!(pct(1, 4), "25.0%");
        assert_eq!(pct(1, 0), "0.0%", "分母为 0 不能除零");
        assert_eq!(fmt_duration(3780), "1小时3分");
        assert_eq!(fmt_duration(90), "1分");
        assert_eq!(fmt_duration(9), "9秒");
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
