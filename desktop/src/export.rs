//! 导出报告（CSV / HTML）。
//!
//! 与 CLI 共用的是**转义与排名比较**那两把尺子（都在 `focusflow_core::format`），
//! 不是"两份实现互为镜像"——列结构、周期标签与 CLI 那份本来就不一样。
//! 读侧的可读性闸两边都要问，判据在 `db::queries::check_years_readable`。

use std::io::Write;

use anyhow::Context;
use chrono::Local;

use focusflow_core::format::{cmp_rank_desc, csv_field, fmt_duration, fmt_thousands, html_escape};

/// 导出用的窗口。与界面顶部那六个周期一档一档对得上。
#[derive(Debug, Clone, PartialEq)]
pub enum ReportWindow {
    /// 某一天（「今日」周期）
    Today(chrono::NaiveDate),
    /// 近 N 天
    Days(i64),
    /// 「总计」：排行榜与总数看全部历史，而按日/按小时/按星期那三段仍只看近 30 天
    All,
}

/// 顶部周期的数字 → 导出窗口。**非法值拒绝，不夹**。
///
/// 夹一下也会"能导出"，但标题与内容就对不上了 —— 命令行里那句「标题写 -5 天、拿到 1 天
/// 数据」记的就是这一族。周期合法性的唯一判据是 `queries::is_valid_period`，这里不另立。
pub fn report_window(period: i64) -> Result<ReportWindow, String> {
    if !focusflow_core::db::queries::is_valid_period(period) {
        return Err(format!(
            "无法导出不存在的统计周期（收到 {period}）：-1 = 今日、0 = 总计、正整数 = 近 N 天"
        ));
    }
    Ok(match period {
        -1 => ReportWindow::Today(Local::now().date_naive()),
        0 => ReportWindow::All,
        n => ReportWindow::Days(n),
    })
}

/// 排行榜与总数那几段挂的名字。
pub(crate) fn period_label(w: &ReportWindow) -> String {
    match w {
        ReportWindow::Today(_) => "今日".to_string(),
        ReportWindow::Days(n) => format!("近{n}天"),
        ReportWindow::All => "总计".to_string(),
    }
}

/// 按日/按小时/按星期那三段挂的名字：`总计` 这一档它们只数近 30 天。
///
/// 这两个标签必须分开写。合成一个"总计"就会让趋势那一段看着像全部历史，
/// 而它其实只有 30 天 —— 天数与"日均"那一格用的是同一把尺子（`chart_window_days`），
/// 这里只是把它说出来，不再另算一次。
pub(crate) fn slice_label(w: &ReportWindow) -> String {
    match w {
        ReportWindow::Today(_) => "今日".to_string(),
        ReportWindow::Days(n) => format!("近{n}天"),
        ReportWindow::All => "近30天".to_string(),
    }
}

/// 三段"时间切面"实际查多少天 —— 尺子在 `state::chart_window_days`，这里只调用。
fn slice_days(w: &ReportWindow) -> i64 {
    match w {
        ReportWindow::Today(_) => -1,
        ReportWindow::Days(n) => *n,
        ReportWindow::All => 0,
    }
}

/// 一份报告的全部数据：查一次，三种格式共用。
///
/// 三个 writer 不再各自查库：过去 `export_csv` 与 `export_html` 都只接 `(total, stats)`，
/// 于是"导出的东西比页面上看到的少一大半"，而且两份各自查一次库时，中间落了一次盘
/// 就会给出两个不同的总数。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReportData {
    pub period_label: String,
    pub slice_label: String,
    pub generated_at: String,
    pub total: i64,
    pub keys: Vec<(String, i64)>,
    pub daily: Vec<(String, i64)>,
    pub hourly: Vec<i64>,
    /// (0=周一 .. 6=周日, 次数) —— 固定 7 格，与界面那把尺子同一个约定
    pub weekday: Vec<(i64, i64)>,
    pub app_total: i64,
    pub apps: Vec<(String, i64)>,
    pub device_total: i64,
    pub devices: Vec<(String, i64)>,
}

/// 0=周一 .. 6=周日，与 `queries::aggregate_weekday` 用的 `num_days_from_monday` 同一约定。
const WD_CN: [&str; 7] = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];
fn weekday_name(d: i64) -> &'static str {
    // 越界按周日处理：这里是显示标签，不许因为它把整份导出炸成 panic
    WD_CN[d.clamp(0, 6) as usize]
}

/// 按次数降序排一份榜单（比较器只许用 `cmp_rank_desc`，`export.rs` 末尾那条断言钉着）。
fn ranked(mut items: Vec<(String, i64)>) -> Vec<(String, i64)> {
    items.sort_by(|a, b| cmp_rank_desc(&a.0, &a.1, &b.0, &b.1));
    items
}

/// 查一次库，凑齐一份报告。全部走现成的读函数，不在这里重写任何口径。
pub fn collect_report(w: &ReportWindow) -> ReportData {
    use focusflow_core::db::queries as q;
    let days = slice_days(w);
    let (total, key_map) = match w {
        ReportWindow::Today(d) => q::get_stats_by_date(*d),
        other => q::get_stats(
            if other == &ReportWindow::All {
                None
            } else {
                Some(days)
            },
            None,
        ),
    };
    let apps_src = match w {
        ReportWindow::Today(d) => q::get_app_stats_by_date(*d),
        ReportWindow::All => q::get_app_stats(None, None),
        ReportWindow::Days(n) => q::get_app_stats(Some(*n), None),
    };
    let devices_src = match w {
        ReportWindow::Today(d) => q::get_device_stats_by_date(*d),
        ReportWindow::All => q::get_device_stats(None, None),
        ReportWindow::Days(n) => q::get_device_stats(Some(*n), None),
    };
    let (app_total, app_map) = apps_src;
    let (device_total, device_list) = devices_src;
    // 时间切面那三段：天数交给 `chart_window_days` 那一把尺子（「总计」= 近 30 天）
    // 天数一律问 core 那把尺子（界面、导出、命令行三处同一个答案）
    let days_window = q::chart_window_days(days);
    let daily = q::get_daily_counts(days_window, None);
    let hourly = q::get_hourly_totals(days_window, None);
    let agg_wd = q::aggregate_weekday(&daily);
    let weekday: Vec<(i64, i64)> = (0..7i64)
        .map(|d| (d, *agg_wd.get(&d).unwrap_or(&0)))
        .collect();
    ReportData {
        period_label: period_label(w),
        slice_label: slice_label(w),
        generated_at: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        total,
        keys: ranked(key_map.into_iter().collect()),
        daily,
        hourly,
        weekday,
        app_total,
        apps: ranked(app_map.into_iter().collect()),
        device_total,
        devices: ranked(device_list.into_iter().map(|d| (d.name, d.count)).collect()),
    }
}

/// 占比：分母是 0 就说 0，不写出"除以零"的那种漂亮数字。
fn share(count: i64, total: i64) -> f64 {
    if total > 0 {
        (count as f64 / total as f64) * 100.0
    } else {
        0.0
    }
}

/// 导出 CSV（带 BOM，Excel 打开中文不乱码）。
pub fn export_csv(path: &std::path::Path, d: &ReportData) -> anyhow::Result<()> {
    let mut out = String::new();
    out.push_str("# FocusFlow 活跃统计导出\n");
    out.push_str(&format!("# 统计周期 {}\n", d.period_label));
    out.push_str(&format!("# 总活跃次数 {}\n", d.total));
    out.push_str(&format!(
        "# 时间切面窗口 {}（按日 / 按小时 / 按星期）\n",
        d.slice_label
    ));
    out.push_str(&format!("# 导出时间 {}\n", d.generated_at));
    out.push_str("\n# 键鼠排行\n排名,键鼠,次数,占比(%)\n");
    for (i, (key, count)) in d.keys.iter().enumerate() {
        out.push_str(&format!(
            "{},{},{},{:.2}\n",
            i + 1,
            csv_field(key),
            count,
            share(*count, d.total)
        ));
    }
    out.push_str("\n# 每日趋势\n日期,次数\n");
    for (day, count) in &d.daily {
        out.push_str(&format!("{day},{count}\n"));
    }
    out.push_str("\n# 每小时分布\n小时,次数\n");
    for (h, count) in d.hourly.iter().enumerate() {
        out.push_str(&format!("{h},{count}\n"));
    }
    out.push_str("\n# 星期分布\n星期,次数\n");
    for (wd, count) in &d.weekday {
        out.push_str(&format!("{},{count}\n", weekday_name(*wd)));
    }
    out.push_str("\n# 应用使用时长（秒）\n排名,应用,秒数,占比(%)\n");
    for (i, (app, secs)) in d.apps.iter().enumerate() {
        out.push_str(&format!(
            "{},{},{},{:.2}\n",
            i + 1,
            csv_field(app),
            secs,
            share(*secs, d.app_total)
        ));
    }
    out.push_str("\n# 设备输入次数\n排名,设备,次数,占比(%)\n");
    for (i, (dev, count)) in d.devices.iter().enumerate() {
        out.push_str(&format!(
            "{},{},{},{:.2}\n",
            i + 1,
            csv_field(dev),
            count,
            share(*count, d.device_total)
        ));
    }
    let mut f =
        std::fs::File::create(path).with_context(|| format!("创建文件失败: {}", path.display()))?;
    f.write_all(b"\xef\xbb\xbf").context("写入 BOM 失败")?;
    f.write_all(out.as_bytes()).context("写入 CSV 内容失败")?;
    Ok(())
}

/// HTML 里一段小标题 + 一张表。六个板块共用这一份，省得每加一列就漏一处转义。
fn html_table(title: &str, head: &[&str], rows: &[Vec<String>]) -> String {
    let mut s = format!("<h2>{}</h2><table><thead><tr>", html_escape(title));
    for col in head {
        if !col.is_empty() {
            s.push_str(&format!("<th>{}</th>", html_escape(col)));
        }
    }
    s.push_str("</tr></thead><tbody>");
    for r in rows {
        s.push_str("<tr>");
        for cell in r {
            s.push_str(&format!("<td>{cell}</td>"));
        }
        s.push_str("</tr>");
    }
    s.push_str("</tbody></table>");
    s
}

/// 带条形图的占比单元格（与旧版那份 HTML 的视觉保持一致）。
fn bar_cell(count: i64, total: i64) -> String {
    let pct = share(count, total);
    format!(
        r#"<div class="bar-container"><div class="bar" style="width:{pct:.1}%"></div><span>{pct:.2}%</span></div>"#
    )
}

/// 导出 HTML 统计报告。
pub fn export_html(path: &std::path::Path, d: &ReportData) -> anyhow::Result<()> {
    let rank_rows: Vec<Vec<String>> = d
        .keys
        .iter()
        .enumerate()
        .map(|(i, (k, c))| {
            vec![
                (i + 1).to_string(),
                html_escape(k),
                fmt_thousands(*c),
                bar_cell(*c, d.total),
            ]
        })
        .collect();
    let daily_rows: Vec<Vec<String>> = d
        .daily
        .iter()
        .map(|(day, c)| vec![html_escape(day), fmt_thousands(*c)])
        .collect();
    let hourly_rows: Vec<Vec<String>> = d
        .hourly
        .iter()
        .enumerate()
        .map(|(h, c)| vec![format!("{h}时"), fmt_thousands(*c), bar_cell(*c, d.total)])
        .collect();
    let weekday_rows: Vec<Vec<String>> = d
        .weekday
        .iter()
        .map(|(w, c)| vec![weekday_name(*w).to_string(), fmt_thousands(*c)])
        .collect();
    let app_rows: Vec<Vec<String>> = d
        .apps
        .iter()
        .enumerate()
        .map(|(i, (a, s))| {
            vec![
                (i + 1).to_string(),
                html_escape(a),
                format!("{} 秒", fmt_thousands(*s)),
                bar_cell(*s, d.app_total),
            ]
        })
        .collect();
    let device_rows: Vec<Vec<String>> = d
        .devices
        .iter()
        .enumerate()
        .map(|(i, (n, c))| {
            vec![
                (i + 1).to_string(),
                html_escape(n),
                fmt_thousands(*c),
                bar_cell(*c, d.device_total),
            ]
        })
        .collect();
    let body = [
        html_table("键鼠排行", &["排名", "键鼠", "次数", "占比"], &rank_rows),
        html_table(
            &format!("每日趋势（{}）", d.slice_label),
            &["日期", "次数"],
            &daily_rows,
        ),
        html_table(
            &format!("每小时分布（{}）", d.slice_label),
            &["小时", "次数", "占比"],
            &hourly_rows,
        ),
        html_table(
            &format!("星期分布（{}）", d.slice_label),
            &["星期", "次数"],
            &weekday_rows,
        ),
        html_table("应用使用时长", &["排名", "应用", "时长", "占比"], &app_rows),
        html_table(
            "设备输入次数",
            &["排名", "设备", "次数", "占比"],
            &device_rows,
        ),
    ]
    .join("\n");
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
    h2 {{ color: #0078d4; margin-top: 32px; font-size: 16px; }}
    .meta {{ color: #666; margin-bottom: 20px; }}
    .total {{ font-size: 28px; font-weight: bold; color: #0078d4; }}
    table {{ width: 100%; border-collapse: collapse; margin-top: 12px; }}
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
    <div class="meta"><div>统计周期：{period}（时间切面按 {slice} 计）</div><div>导出时间：{at}</div></div>
    <div class="total">总活跃次数：{total}</div>
    {body}
</div>
</body>
</html>"#,
        period = html_escape(&d.period_label),
        slice = html_escape(&d.slice_label),
        at = html_escape(&d.generated_at),
        total = fmt_thousands(d.total),
        body = body,
    );
    std::fs::write(path, html).with_context(|| format!("写入文件失败: {}", path.display()))?;
    Ok(())
}

/// 导出 JSON：给人写的脚本读的那一份。
///
/// 字段名、周期标签与上面两个格式出自**同一个** `ReportData`（三份文件不会互相打脸）。
pub fn export_json(path: &std::path::Path, d: &ReportData) -> anyhow::Result<()> {
    let f =
        std::fs::File::create(path).with_context(|| format!("创建文件失败: {}", path.display()))?;
    serde_json::to_writer_pretty(f, d).context("写入 JSON 失败")?;
    Ok(())
}

/// Markdown 表格单元格：键名里可能带 `|` 或换行，会直接破坏表格结构。
fn md_cell(s: &str) -> String {
    // 反斜杠要先转义：单元格里以 `\` 结尾会把紧跟的 `|` 吃掉，整行列错位。
    // 顺序不能反 —— 先转 `|` 再转 `\` 会把刚加的那个 `\` 又转一遍。
    s.replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace(['\n', '\r'], " ")
}

/// 百分比（分母为 0 时给 0.0%，不做除零）。
fn pct(part: i64, total: i64) -> String {
    if total > 0 {
        format!("{:.1}%", part as f64 / total as f64 * 100.0)
    } else {
        "0.0%".to_string()
    }
}

/// 周报文件所在目录。
pub fn report_dir() -> std::path::PathBuf {
    focusflow_core::paths::data_dir().join("reports")
}

/// 周报里「环比」那一段的文案。
///
/// 上一周基数很小的时候，百分比在数学上没错、在信息量上是噪音：刚开始记录的第二个
/// 星期，上一周可能只有十几次点击，本机第一份周报就印着 `环比 +35678.6%`。
/// 超过 ±1000% 就改报绝对差值 —— 读者要的是"多了多少"，不是一个位数都数不清的百分号。
fn week_delta(total: i64, prev_total: i64) -> String {
    if prev_total <= 0 {
        return String::new();
    }
    let diff = total - prev_total;
    let pct = diff as f64 / prev_total as f64 * 100.0;
    if pct.abs() >= 1000.0 {
        format!(
            "，环比 {}{} 次（上一周基数太小，百分比没有意义）",
            if diff >= 0 { "+" } else { "-" },
            fmt_thousands(diff.abs())
        )
    } else {
        format!("，环比 {:+.1}%", pct)
    }
}

/// 生成并写出「上一个完整周」（周一~周日）的周报，返回文件路径。
pub fn write_weekly_report() -> anyhow::Result<Option<std::path::PathBuf>> {
    let (from, to) = focusflow_core::stats::last_finished_week(chrono::Local::now().date_naive());
    write_weekly_report_for(from, to)
}

/// 周报用的目标次数：必须与设置页、`stats::goal_status` 同一把尺子。
///
/// 单独成函数是为了能被喂一份临时配置去测（`config::instance()` 是进程内单例，
/// 用例改不动它）。周报里"达标 X 天"和逐日的 ✅ 是自己数出来的，而"连续 N 天"走
/// `goal_status` —— 两处必须过同一个 `effective_goal`，否则 `daily_keys = 0` 时
/// 一份文档里两把尺子（见 `stats::effective_goal`）。
fn weekly_goal(cfg: &focusflow_core::config::FocusFlowConfig) -> i64 {
    focusflow_core::stats::effective_goal(cfg.get_int("goal", "daily_keys", 20000))
}

/// 写出指定「周一~周日」区间的周报。
///
/// 环比、连续打卡都改用同一批**按日**查询取数，而不是拿「相对今天的 21 天」去套：
/// 那样只有"上一个整周"这一种取值碰巧落在窗口内，任何历史周（跨年、手动补生成）
/// 都会静默读到 0 —— 报告照样生成，只是数字全空，最难发现。按日查询自己按日期
/// 选年度库，所以跨年周（一月的第一个周报正好压在年界上）会把两个库都算进来。
pub fn write_weekly_report_for(
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
) -> anyhow::Result<Option<std::path::PathBuf>> {
    use chrono::Datelike;
    use focusflow_core::db::queries as q;

    // 读侧的闸必须先过：`get_stats_by_date` 把「年度库打不开」折成 `(0, 空表)`，
    // `get_daily_counts` 折成缺行 —— 移动盘还没插、或某一年的库被同步盘换成占位文件时，
    // 这条路的结局是 `Ok(None)`，于是设置页说「上一个整周没有任何记录，未生成文件」，
    // 自动那条只留一行 debug。两句都把"读不出来"讲成了"那一周没记录"，而它差着一整年的数。
    // 判据与 CLI 七条读命令、桌面导出共用 `check_years_readable`（本文件开头写明的口径）。
    q::check_years_readable().map_err(|e| anyhow::anyhow!("生成周报被拒绝：{e}"))?;

    let goal = weekly_goal(focusflow_core::config::instance());

    let mut keys: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut apps: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut hourly = [0i64; 24];
    let mut rows: Vec<(chrono::NaiveDate, i64)> = Vec::new();
    // 上一周 + 本周共 14 天：上一周只用来做环比；连续/最长另按
    // `GOAL_LOOKBACK_DAYS` 取序列（见下面 `goal_state`，与设置页同一把尺子）。
    let mut keyboard = 0i64;
    let mut mouse = 0i64;
    let mut prev_total = 0i64;

    let mut day = from - chrono::Duration::days(7);
    while day <= to {
        let (total, day_keys) = q::get_stats_by_date(day);
        if day >= from {
            rows.push((day, total));
            for (k, c) in &day_keys {
                *keys.entry(k.clone()).or_insert(0) += *c;
                if focusflow_core::format::MOUSE_GROUPS
                    .contains(&focusflow_core::format::classify_key(k))
                {
                    mouse += *c;
                } else {
                    keyboard += *c;
                }
            }
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
        } else {
            prev_total += total;
        }
        day += chrono::Duration::days(1);
    }

    let total: i64 = rows.iter().map(|(_, c)| c).sum();
    let best = rows.iter().max_by_key(|(_, c)| *c).copied();
    let worst = rows.iter().min_by_key(|(_, c)| *c).copied();
    let met_days = rows.iter().filter(|(_, c)| *c >= goal).count();
    // 连续/最长必须与设置页同一把尺子。`stats::GOAL_LOOKBACK_DAYS` 的注释写着
    // 「调用方取按日序列时必须用同一个值，否则 best 会在数据边界上被截断，
    // 而两处各自写死数字迟早对不上」—— 周报原来喂的是上面那份 **14 天**序列，
    // 于是同一个人连打 40 天：设置页显示 40，周报显示 14，而周报的文案还写着
    // 「截至本周日连续」，读起来像整段纪录。两处的数字必须能对上。
    // 锚点仍是本周日（`to`）不是今天，pending 恒传 0：周报是「已落库快照」文档（§七·4）。
    // 序列的**尾界**是今天（`db/queries.rs:469-477`：`end_dk = 今天`、起点 `今天-(n-1)`），
    // 而判定的锚在 `to`（`stats.rs:342` 从 `to` 往回数 `GOAL_LOOKBACK_DAYS-1` 天）。
    // 只要 370 天就等于把序列左端少要了 `今天 - to`（1..7）天：连打 363 天以上的人，
    // 周报的连续/最长会比设置页少这么多天 —— 正是 `GOAL_LOOKBACK_DAYS` 注释禁止的
    // 「两处两把尺子」。上面那条修的是同族的另一头（周报喂 14 天序列），那次播的纪录
    // 只有 41 天，正好跨不过窗口左端，所以这个缺口没被现有用例照出来。
    let anchor_gap = (Local::now().date_naive() - to).num_days().max(0);
    let goal_rows =
        q::get_daily_counts(focusflow_core::stats::GOAL_LOOKBACK_DAYS + anchor_gap, None);
    let goal_state =
        focusflow_core::stats::goal_status(goal, &goal_rows, &to.format("%Y-%m-%d").to_string(), 0);
    let app_total: i64 = apps.values().sum();
    let hour_max = *hourly.iter().max().unwrap_or(&0);

    // 整周（连同一周对比用的上一周）一条记录都没有 → 不生成文件。
    // 这个函数在每次启动时都会被自动触发一次：新装的机器上写一份全 0 的周报
    // 只是往 data/reports/ 里堆噪音，还让人以为程序在空转。
    if total == 0 && prev_total == 0 {
        return Ok(None);
    }

    let week_cn = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];
    let mut out = String::new();
    out.push_str(&format!(
        "# FocusFlow 周报（{} ~ {}）\n\n",
        from.format("%Y-%m-%d"),
        to.format("%Y-%m-%d")
    ));
    out.push_str(&format!(
        "- 生成时间：{}\n",
        Local::now().format("%Y-%m-%d %H:%M:%S")
    ));
    let delta = week_delta(total, prev_total);
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
        "- 每日目标 {}：达标 {}/7 天；截至本周日连续 {} 天（近一年最长 {} 天）\n\n",
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
    top.sort_by(|a, b| cmp_rank_desc(a.0, a.1, b.0, b.1));
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
        at.sort_by(|a, b| cmp_rank_desc(a.0, a.1, b.0, b.1));
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
    Ok(Some(path))
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

    /// 空周不产出文件：自动触发点在统计线程的第一轮循环里，也就是**每次启动**
    /// 都会走一次。新装的机器那一周没有任何记录，写一份全 0 的周报只是噪音。
    #[test]
    fn weekly_report_skips_week_without_data() -> anyhow::Result<()> {
        use chrono::NaiveDate;
        use focusflow_core::paths;

        let _serial = crate::app_dir_lock();
        let _app = paths::test_app_dir("weekly_empty");
        focusflow_core::db::queries::invalidate_years_cache();

        let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).unwrap();
        let r = write_weekly_report_for(d(2020, 3, 2), d(2020, 3, 8))?;
        assert!(r.is_none(), "没有任何记录时不该生成文件");
        assert!(!report_dir().exists(), "连 reports/ 目录都不该被创建出来");
        Ok(())
    }

    /// 「那一年的库读不出来」不能说成「那一周没有记录」，也不能出一份偏小的数。
    ///
    /// 这条道原先不问可读性：按日查询把打不开折成 0，于是 `total == 0 && prev_total == 0`
    /// 成立 → 回 `Ok(None)` → 设置页那句「上一个整周没有任何记录，未生成文件」，
    /// 自动那条一行 debug。坏掉的那年如果在报告窗口之外（连续天数要往回看一年多），
    /// 结局更糟：文件照写，数字偏小，界面报绿色成功。
    #[test]
    fn a_week_that_cannot_be_read_is_not_reported_as_empty() -> anyhow::Result<()> {
        use chrono::{Datelike, NaiveDate};
        use focusflow_core::db::connection::{ensure_schema, open_rw};
        use focusflow_core::db::queries;
        use focusflow_core::paths;

        let _serial = crate::app_dir_lock();
        let _app = paths::test_app_dir("weekly_unreadable");
        queries::invalidate_years_cache();
        focusflow_core::db::connection::clear_ro_cache();

        let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).unwrap();
        let (from, to) = (d(2020, 3, 2), d(2020, 3, 8));
        let epoch = d(1970, 1, 1);
        let dk = |day: NaiveDate| day.signed_duration_since(epoch).num_days();

        // 报告那一年的库：建好，并且**真有数据**（这样"读不出来"与"没有记录"才可能被分开）
        {
            let c = open_rw(&paths::year_db_path(from.year()))?;
            ensure_schema(&c, from.year())?;
            c.execute_batch(&format!(
                "INSERT INTO daily_counts (date_key, count, seconds) VALUES ({}, 30000, 3600);",
                dk(from)
            ))?;
        }
        // 另一年（周报的连续天数序列要往回看一年多）被换成一份不是 SQLite 的文件：
        // 这正是同步盘占位、移动盘没插那一类
        std::fs::write(paths::year_db_path(2019), b"not a sqlite file at all")?;
        queries::invalidate_years_cache();
        focusflow_core::db::connection::clear_ro_cache();

        let e = write_weekly_report_for(from, to).expect_err("有一年的库读不出来时不该回 Ok(None)");
        let msg = e.to_string();
        assert!(
            msg.contains("打不开") && msg.contains("2019"),
            "要说清是哪一年读不出来，实得: {msg}"
        );
        assert!(
            !msg.contains("没有任何记录"),
            "不能把读不出来讲成没有记录，实得: {msg}"
        );
        assert!(!report_dir().exists(), "被拒绝的这一次不该留下任何文件");

        // 反向腿：把那份坏文件清掉，同一周就必须正常生成 —— 这条判据不是恒真
        std::fs::remove_file(paths::year_db_path(2019))?;
        queries::invalidate_years_cache();
        focusflow_core::db::connection::clear_ro_cache();
        let r = write_weekly_report_for(from, to)?;
        assert!(r.is_some(), "坏文件拿走后同一周就该生成文件，实得 {r:?}");
        Ok(())
    }

    #[test]
    fn report_helpers_escape_and_format() {
        // 键名里的竖线会切断 Markdown 表格列，换行会撕掉整行
        assert_eq!(md_cell("a|b"), "a\\|b");
        assert_eq!(md_cell("a\nb"), "a b");
        assert_eq!(md_cell("鼠标左键"), "鼠标左键");
        // 反斜杠也要转义：以 `\` 结尾的单元格会把紧跟的 `|` 当转义吃掉，整行列错位。
        // 顺序不能反 —— 先转 `|` 再转 `\` 会把刚加的那个反斜杠又转一遍。
        assert_eq!(md_cell("x\\"), "x\\\\");
        assert_eq!(md_cell("a\\|b"), "a\\\\\\|b");
        assert_eq!(pct(1, 4), "25.0%");
        assert_eq!(pct(1, 0), "0.0%", "分母为 0 不能除零");
        assert_eq!(fmt_duration(3780), "1小时3分");
        assert_eq!(fmt_duration(90), "1分");
        assert_eq!(fmt_duration(9), "9秒");
    }

    /// 端到端：造一个有数据的年度库，跑一次真实周报生成。
    ///
    /// 为什么值得测：这份报告由统计线程在**启动后第一轮**自动触发，而 release
    /// profile 是 `panic = "abort"` —— 生成路径里任何一次 panic 都不是"报告没了"，
    /// 是整个进程在开机时消失。格式化（除零、下标、空表）与 Markdown 转义都必须
    /// 在真数据上跑一遍，而不是只看它编译得过。
    #[test]
    fn weekly_report_renders_seeded_week() -> anyhow::Result<()> {
        use chrono::{Datelike, NaiveDate};
        use focusflow_core::db::connection::open_rw;
        use focusflow_core::db::queries;
        use focusflow_core::paths;

        let _serial = crate::app_dir_lock();
        let _app = paths::test_app_dir("weekly");
        queries::invalidate_years_cache();

        let today = Local::now().date_naive();
        let (from, to) = focusflow_core::stats::last_finished_week(today);
        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        let dk = |d: NaiveDate| d.signed_duration_since(epoch).num_days();

        // 连打纪录必须跨过两周才测得出"周报用的尺子"：多播 27 天，让真实连续是 41 天，
        // 而 14 天序列最多只能算出 14。
        // 按天所属年份各写各的库（一天一个连接，测试里不必把 Connection 类型写出来）：
        // 一月里跑这次用例时，多播的那几周属于上一个年份库，全塞进 `to` 那一年的库
        // 就写错了地方，连续天数也就测不出效果。
        const EXTRA_DAYS: i64 = 27;
        let seed_day = |day: NaiveDate| -> anyhow::Result<()> {
            let y = day.year();
            let c = open_rw(&paths::year_db_path(y))?;
            focusflow_core::db::connection::ensure_schema(&c, y)?;
            let k = dk(day);
            // 最后一天的键名带竖线：它必须被 md_cell 转义，否则表格列会被切断
            let key = if day == to { "a|b" } else { "鼠标左键" };
            c.execute_batch(&format!(
                "INSERT INTO daily_counts (date_key, count, seconds) VALUES ({k}, 30000, 3600);
                 INSERT OR REPLACE INTO key_counts (date_key, key_name, count) VALUES ({k}, '{key}', 20000);
                 INSERT OR REPLACE INTO key_counts (date_key, key_name, count) VALUES ({k}, 'A', 10000);
                 INSERT OR REPLACE INTO app_usage (date_key, app_name, seconds) VALUES ({k}, 'code.exe', 5400);
                 INSERT OR REPLACE INTO hourly_counts (date_key, hour, count) VALUES ({k}, 14, 9000);"
            ))?;
            Ok(())
        };
        let mut day = from - chrono::Duration::days(7 + EXTRA_DAYS);
        while day <= to {
            seed_day(day)?;
            day += chrono::Duration::days(1);
        }
        let seeded = EXTRA_DAYS + 14;
        queries::invalidate_years_cache();

        let path = write_weekly_report()?.expect("有数据时应生成文件");
        let md = std::fs::read_to_string(&path)?;

        assert!(path.starts_with(paths::data_dir().join("reports")));
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            format!(
                "周报-{}-{}.md",
                from.format("%Y-%m-%d"),
                to.format("%Y-%m-%d")
            ),
            "文件名必须带上它覆盖的那个整周"
        );
        assert!(md.contains(&format!(
            "{} ~ {}",
            from.format("%Y-%m-%d"),
            to.format("%Y-%m-%d")
        )));
        // 7 天 × 30000
        assert!(md.contains("210,000"), "本周总数应为 21 万：\n{md}");
        // 上一周同量 → 环比 0.0%
        assert!(md.contains("环比 +0.0%"), "同量对比应为 +0.0%：\n{md}");
        assert!(md.contains("每日") && md.contains("时段分布"));
        assert!(md.contains("| 14:00 |"), "14 时应有 7×9000 的聚合");
        // 键名转义：竖线必须成 \|，否则它会自成一个新的列分隔
        assert!(md.contains(r#"a\|b"#), "键名里的竖线必须转义：\n{md}");
        // 连续天数必须与设置页同一把尺子：播了 `seeded` 个连续达标日，周报就得报出
        // 这个数；喂 14 天序列的旧写法最多只能算出 14，这条会红。
        assert!(
            md.contains(&format!("连续 {seeded} 天")),
            "周报的连续天数被 14 天序列截断了（应报 {seeded}）：\n{md}"
        );
        assert!(md.contains("鼠标左键"));
        assert!(md.contains("code.exe"));
        // 重跑一次：同一周必须落在同一个文件（幂等，不产生第二份）
        let again = write_weekly_report()?.expect("重跑应仍指向同一文件");
        assert_eq!(again, path);
        // 目录由 _app 的 Drop 删除：别让下一个用例继续沿着这份年度库列表查下去
        queries::invalidate_years_cache();
        Ok(())
    }

    /// 连续纪录跨过 `GOAL_LOOKBACK_DAYS` 时，周报与设置页必须是同一把尺子。
    ///
    /// `get_daily_counts` 的窗口尾界是**今天**，而 `goal_status` 从 `to`（周末那天）往回数
    /// `GOAL_LOOKBACK_DAYS - 1` 天 —— 只要 370 天就等于把序列左端少要 `今天 - to` 天，
    /// 连打 363 天以上的人周报会少报这么多天。上一场修的是同族的另一头（周报喂 14 天
    /// 序列），那条用例播 41 天，纪录跨不过窗口左端 ⇒ 照不出这个缺口。
    ///
    /// **刻意不走 `last_finished_week`**：今天正好是周日时那一周的"周末"就是今天，
    /// 缺口 0 天，旧写法照样过 —— 用例得日历无关，所以直接把界钉在 `今天 - 3`。
    /// 时间只会往前走，实际缺口只会 ≥ 3，不会把这条翻成假绿。
    #[test]
    fn weekly_report_lookback_is_not_short_by_the_days_since_the_week_end() -> anyhow::Result<()> {
        use chrono::{Datelike, NaiveDate};
        use focusflow_core::db::connection::{ensure_schema, open_rw};
        use focusflow_core::db::queries;
        use focusflow_core::paths;

        let _serial = crate::app_dir_lock();
        let _app = paths::test_app_dir("weekly_lookback");
        queries::invalidate_years_cache();

        let today = Local::now().date_naive();
        let to = today - chrono::Duration::days(3);
        let from = to - chrono::Duration::days(6);
        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        let dk = |d: NaiveDate| d.signed_duration_since(epoch).num_days();
        let lookback = focusflow_core::stats::GOAL_LOOKBACK_DAYS;

        // 逐年一条批量语句：逐日开连接会把这条用例拖到秒级以上
        let mut by_year: std::collections::HashMap<i32, String> = std::collections::HashMap::new();
        let mut day = to - chrono::Duration::days(lookback - 1);
        while day <= to {
            let k = dk(day);
            by_year.entry(day.year()).or_default().push_str(&format!(
                "INSERT OR REPLACE INTO daily_counts (date_key, count, seconds) \
                 VALUES ({k}, 30000, 3600); \
                 INSERT OR REPLACE INTO key_counts (date_key, key_name, count) \
                 VALUES ({k}, '鼠标左键', 30000);"
            ));
            day += chrono::Duration::days(1);
        }
        for (year, sql) in &by_year {
            let conn = open_rw(&paths::year_db_path(*year))?;
            ensure_schema(&conn, *year)?;
            conn.execute_batch(sql)?;
        }
        queries::invalidate_years_cache();

        let path = write_weekly_report_for(from, to)?.expect("有数据时应生成文件");
        let md = std::fs::read_to_string(&path)?;
        assert!(
            md.contains(&format!("连续 {lookback} 天")),
            "周报的连续天数被少要了 `今天 - to` 天（旧写法报 {}）：\n{md}",
            lookback - 3
        );
        assert!(
            md.contains(&format!("近一年最长 {lookback} 天）")),
            "近一年最长同样被窗口左端截短：\n{md}"
        );
        // 总量照旧只算那 7 天（7 × 30000），别让补长的窗口漏进别的口径里
        assert!(md.contains("210,000"), "本周总量应仍是 21 万：\n{md}");
        queries::invalidate_years_cache();
        Ok(())
    }

    /// 上一周基数极小时，环比必须改报绝对差值，而不是印一个几万次方的百分号。
    #[test]
    fn week_delta_avoids_meaningless_percentages() {
        // 刚开始记录的第二个星期：上一周只有 14 次（本机第一份周报的真实形态）
        let s = week_delta(5009, 14);
        assert!(s.contains("4,995 次"), "小基数应报绝对差值：{s}");
        assert!(!s.contains('%'), "不该再出现无意义的百分比：{s}");
        // 正常量级照旧走百分比
        assert_eq!(week_delta(210000, 210000), "，环比 +0.0%");
        assert_eq!(week_delta(100, 200), "，环比 -50.0%");
        // 下降最多到 -100%，不会被误判成"基数太小"
        assert_eq!(week_delta(0, 30000), "，环比 -100.0%");
        // 上一周为 0：整段不写（既不是除零 panic，也不是 inf%）
        assert_eq!(week_delta(500, 0), "");
    }

    /// 跨年周：一月的第一个周报正好压在年度库边界上，两个库都必须算进来。
    ///
    /// 少查一个库的表现是"报告照样生成、数字偏小"，属于最难被发现的一类错误
    /// （不 panic、不报错、文件也在），所以把总量直接钉死。
    #[test]
    fn weekly_report_spans_year_boundary() -> anyhow::Result<()> {
        use chrono::NaiveDate;
        use focusflow_core::db::connection::{ensure_schema, open_rw};
        use focusflow_core::db::queries;
        use focusflow_core::paths;

        let _serial = crate::app_dir_lock();
        let _app = paths::test_app_dir("weekly_xyear");
        queries::invalidate_years_cache();

        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        let seed = |year: i32, dates: &[NaiveDate], per_day: i64| -> anyhow::Result<()> {
            let conn = open_rw(&paths::year_db_path(year))?;
            ensure_schema(&conn, year)?;
            conn.execute_batch("BEGIN IMMEDIATE;")?;
            for d in dates {
                let k = d.signed_duration_since(epoch).num_days();
                conn.execute_batch(&format!(
                    "INSERT INTO daily_counts (date_key, count, seconds) VALUES ({k}, {per_day}, 600);
                     INSERT OR REPLACE INTO key_counts (date_key, key_name, count) VALUES ({k}, 'A', {per_day});"
                ))?;
            }
            conn.execute_batch("COMMIT;")?;
            Ok(())
        };
        // 2025-12-29 是周一，2026-01-04 是周日：整周横跨两个年度库
        let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).unwrap();
        seed(
            2025,
            &[d(2025, 12, 29), d(2025, 12, 30), d(2025, 12, 31)],
            10000,
        )?;
        seed(
            2026,
            &[d(2026, 1, 1), d(2026, 1, 2), d(2026, 1, 3), d(2026, 1, 4)],
            20000,
        )?;
        queries::invalidate_years_cache();

        let path = write_weekly_report_for(d(2025, 12, 29), d(2026, 1, 4))
            .expect("跨年周不应报错")
            .expect("跨年周应生成");
        let md = std::fs::read_to_string(&path)?;
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            "周报-2025-12-29-2026-01-04.md"
        );
        // 3×10000 + 4×20000：只查一个年度库会得到 30,000 或 80,000
        assert!(
            md.contains("110,000"),
            "跨年周必须把两个年度库都算进来：\n{md}"
        );
        assert!(md.contains("2025-12-29") && md.contains("2026-01-04"));
        // 每日表的星期标签按真实星期几走（12-29 周一、01-04 周日）
        assert!(md.contains("| 2025-12-29 | 周一 |"), "标签错位：\n{md}");
        assert!(md.contains("| 2026-01-04 | 周日 |"), "标签错位：\n{md}");
        // 目录随 _app 的 Drop 回收，年份缓存留给下一个用例重新扫
        queries::invalidate_years_cache();
        Ok(())
    }

    /// 不查库、直接喂一份 `ReportData` 给三个 writer 用。
    ///
    /// 三个格式必须出自同一份数据，所以测试也造同一份数据 —— 让测试去查库，
    /// 测的就变成"读函数对不对"，而那一层 core 自己有用例。
    fn report_from(total: i64, stats: &HashMap<String, i64>) -> ReportData {
        ReportData {
            period_label: "总计".to_string(),
            slice_label: "近30天".to_string(),
            generated_at: "2026-10-24 10:00:00".to_string(),
            total,
            keys: ranked(stats.iter().map(|(k, v)| (k.clone(), *v)).collect()),
            daily: vec![("2026-10-20".to_string(), 40)],
            hourly: vec![0i64; 24],
            weekday: (0..7i64).map(|d| (d, 0i64)).collect(),
            app_total: 0,
            apps: vec![],
            device_total: 0,
            devices: vec![],
        }
    }

    #[test]
    fn export_csv_has_bom_header_and_rows() -> anyhow::Result<()> {
        let dir = std::env::temp_dir().join("ff_export_test");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("out.csv");
        let _ = std::fs::remove_file(&path);

        export_csv(&path, &report_from(80, &sample_stats())).expect("export_csv should succeed");
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
        export_csv(&path, &report_from(8, &stats)).expect("export_csv should succeed");
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

        export_html(&path, &report_from(80, &sample_stats())).expect("export_html should succeed");
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
        export_html(&path, &report_from(1, &stats)).expect("export_html should succeed");
        let html = std::fs::read_to_string(&path).expect("read html");
        assert!(
            html.contains("&lt;b&gt;x&amp;y&lt;/b&gt;"),
            "HTML 键名应转义"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 周期 → 窗口：非法值**拒绝**而不是夹一下。
    ///
    /// 夹一下也"能导出"，但标题与内容就对不上了 —— 命令行里那句「标题写 -5 天、拿到 1 天
    /// 数据」记的就是这一族。合法值那几条同时钉住两个标签：`总计` 的排行榜看全部历史，
    /// 而按日/按小时那几段只数近 30 天，两个名字必须分开写。
    #[test]
    fn report_window_refuses_a_period_that_does_not_exist() {
        assert_eq!(
            report_window(-1).unwrap(),
            ReportWindow::Today(Local::now().date_naive())
        );
        assert_eq!(report_window(7).unwrap(), ReportWindow::Days(7));
        assert_eq!(report_window(0), Ok(ReportWindow::All));
        for bad in [-2, -100, 40_000] {
            let why = report_window(bad).unwrap_err();
            assert!(
                why.contains("无法导出不存在的统计周期") && why.contains(&bad.to_string()),
                "要说清收到的是什么，实得: {why}"
            );
        }
        let all = ReportWindow::All;
        assert_eq!(period_label(&all), "总计", "排行榜那几段的名字");
        assert_eq!(
            slice_label(&all),
            "近30天",
            "时间切面不许挂着「总计」报 30 天的数"
        );
        assert_eq!(
            slice_days(&all),
            0,
            "天数交给 chart_window_days，不在这里夹"
        );
        assert_eq!(
            slice_days(&ReportWindow::Today(Local::now().date_naive())),
            -1
        );
    }

    /// JSON 是给人写的脚本读的那一份：板块齐不齐、标签在不在，都得当场能验。
    #[test]
    fn export_json_carries_every_section_and_both_labels() -> anyhow::Result<()> {
        let dir = std::env::temp_dir().join("ff_export_json");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("out.json");
        let mut stats = HashMap::new();
        stats.insert("A".to_string(), 40);
        let d = ReportData {
            hourly: (0..24).map(|h| h as i64).collect(),
            app_total: 90,
            apps: vec![("notepad.exe".to_string(), 90)],
            device_total: 5,
            devices: vec![("新鼠标".to_string(), 5)],
            ..report_from(80, &stats)
        };
        export_json(&path, &d).expect("export_json should succeed");
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)
            .expect("写出来的必须是合法 JSON");
        assert_eq!(v["period_label"], "总计");
        assert_eq!(v["slice_label"], "近30天");
        assert_eq!(v["total"], 80);
        assert_eq!(v["keys"].as_array().unwrap().len(), 1);
        assert_eq!(
            v["hourly"].as_array().unwrap().len(),
            24,
            "小时必须固定 24 格"
        );
        assert_eq!(
            v["weekday"].as_array().unwrap().len(),
            7,
            "星期必须固定 7 格"
        );
        assert!(
            !v["daily"].as_array().unwrap().is_empty(),
            "每日趋势那一段不见了"
        );
        assert!(v["apps"].as_array().unwrap().len() == 1, "应用一段不见了");
        assert!(
            v["devices"].as_array().unwrap().len() == 1,
            "设备一段不见了"
        );
        assert!(
            v["generated_at"].is_string(),
            "导出时间要在，两份文件才比得出谁新"
        );
        std::fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    /// 写一份临时 config.ini 并加载。`config::instance()` 是进程内单例，用例改不动它，
    /// 所以把"读配置"这一步单独成函数（[`weekly_goal`]）才好喂配置进去测。
    /// 放 `target/` 下按 tag 固定命名：`%TEMP%` 里的随机名每跑一次漏一个目录。
    fn cfg(tag: &str, body: &str) -> focusflow_core::config::FocusFlowConfig {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../target")
            .join(format!("ff_export_{tag}.ini"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("{body}\n")).unwrap();
        focusflow_core::config::FocusFlowConfig::load(&path).unwrap()
    }

    /// 回归（账 37）：`[goal] daily_keys = 0` 时，周报的"达标 X 天"与逐日的 ✅ 是按
    /// **原始配置值**数的（0 ⇒ 每一天都满分），而同一份文件里的"连续 N 天"走
    /// `stats::goal_status`，那边有 `goal.max(1)`；设置页那边又自己写了一遍 `.max(1)`。
    /// 三处三把尺子，现在都过 `stats::effective_goal`。
    #[test]
    fn weekly_goal_shares_the_settings_page_caliper() {
        assert_eq!(
            weekly_goal(&cfg("goal_zero", "[goal]\ndaily_keys = 0")),
            1,
            "目标 0 次不能被当成「每天都达标」"
        );
        assert_eq!(
            weekly_goal(&cfg("goal_absent", "[gui]\ntheme = dark")),
            20000,
            "没配就用默认值"
        );
        assert_eq!(
            weekly_goal(&cfg("goal_ok", "[goal]\ndaily_keys = 500")),
            500,
            "正常配置值一字不动"
        );
    }

    /// 导出侧的排名必须**与输入顺序无关** —— 这是 `HashMap` 那个随机种子的正面判据。
    ///
    /// 四条榜单（CSV / HTML / 周报键鼠 Top 10 / 周报应用 Top 5）原先都只写
    /// `sort_by(|a, b| b.1.cmp(a.1))`，而屏幕上同一条榜早就补了同分破平
    /// （`state.rs::sort_rank_desc`，注释里记着他真实库 113 个键、第 99~102 名并列
    /// 被 100 那条线切开的现场）。导出这四处在同一个形状上漏掉：紧跟在排序后面的是
    /// `take(10)` / `take(5)`，所以随机性不止于"顺序换了"，而是**谁进榜**每进程都换；
    /// 周报每次启动自动重生成一份 ⇒ 同一周两次导出的 Top 10 可以不同，
    /// 而且与用户在屏幕上看到的那份不一致。
    #[test]
    fn export_rank_membership_does_not_depend_on_input_order() {
        // 一组并列 10 次、恰好跨过 take(10) 这条线的键：只有按名字定序才谈得上稳定
        const TIED_NAMES: [&str; 14] = [
            "k00", "k01", "k02", "k03", "k04", "k05", "k06", "k07", "k08", "k09", "k10", "k11",
            "k12", "k13",
        ];
        let mut items: Vec<(&str, i64)> = vec![("zzz-big", 90)];
        for name in TIED_NAMES {
            items.push((name, 10));
        }

        let rank10 = |src: &mut [(&str, i64)]| -> Vec<String> {
            src.sort_by(|a, b| cmp_rank_desc(a.0, &a.1, b.0, &b.1));
            src.iter().take(10).map(|(k, _)| k.to_string()).collect()
        };

        // 两份"HashMap 会给出的"不同输入序：结果必须逐位相同
        let mut a = items.clone();
        let first = rank10(&mut a);
        items.reverse();
        let mut b = items.clone();
        let second = rank10(&mut b);
        assert_eq!(
            first, second,
            "同一份数据换个输入序就换榜 ⇒ take(10) 是随机的"
        );

        assert_eq!(first[0], "zzz-big", "次数降序这一半不能丢");
        assert_eq!(
            &first[1..],
            &["k00", "k01", "k02", "k03", "k04", "k05", "k06", "k07", "k08"],
            "并列那组里上榜的必须是名字最小的 9 个，且这个集合与输入序无关"
        );
        assert!(
            !first.iter().any(|k| k == "k13"),
            "名字最大的那个并列项不该挤进榜：{first:?}"
        );
    }

    /// 导出与周报的每一张榜单都必须都走那**一个**比较器 —— 屏幕上修过的那条规则不许只在屏幕上有。
    ///
    /// 与本文件 `weekly_goal_shares_the_settings_page_caliper` 同一族的教训：一把尺子
    /// 写在几处，就会有一处被漏掉，而这里的漏法是"每次导出可能给不同的 Top 10"，
    /// 不报错、不崩溃，只有人对过两份文件时才会发现。
    ///
    /// 三处而不是六处：CSV 与 HTML 的键鼠/应用/设备三张榜现在都从同一个 `ranked()`
    /// 出来（改之前是两份文件各写一遍 `.sort_by(`，那是六处）。
    #[test]
    fn every_export_rank_goes_through_the_one_comparator() {
        let src = include_str!("export.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let sorts: Vec<usize> = prod.match_indices(".sort_by(").map(|(i, _)| i).collect();
        assert_eq!(
            sorts.len(),
            3,
            "导出的排名排序应当正好三处（共用的 `ranked` / 周报 Top 10 / 周报应用 Top 5），\
             现在数到 {} 处 —— 加了新榜单就要一并进这条断言（并优先接进 `ranked`，别再手抄一遍）",
            sorts.len()
        );
        for (n, at) in sorts.iter().enumerate() {
            let line = &prod[*at..prod[*at..].find('\n').unwrap_or(prod.len() - *at) + *at];
            assert!(
                line.contains("cmp_rank_desc"),
                "第 {} 处排序没走共用比较器：{line}\n\
                 只排次数 = 同分键谁进榜随 HashMap 种子变",
                n + 1
            );
        }
    }
}
