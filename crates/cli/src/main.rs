//! FocusFlow 命令行接口。
//!
//! 镜像 Python 版 `cli.py`：
//!   focusflow-cli --stats 7          # 最近 7 天统计
//!   focusflow-cli --stats today      # 今日统计
//!   focusflow-cli --stats all        # 总计
//!   focusflow-cli --stats-year 2025  # 指定年度
//!   focusflow-cli --export csv|html  # 导出（当前目录 focusflow_export.csv/html）
//!   focusflow-cli --reset            # 清空所有记录
//!   focusflow-cli --vacuum           # 压缩数据库
//!   focusflow-cli --cleanup 30       # 清理 30 天前数据
//!   focusflow-cli --list-years       # 列出有数据的年份

use std::process::ExitCode;

use chrono::Local;
use focusflow_core::db;
use focusflow_core::logger;

use focusflow_core::format::{csv_field, fmt_thousands, html_escape};

fn main() -> ExitCode {
    logger::init_logging();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = run(&args);
    ExitCode::from(code as u8)
}

/// 命令用法（`--help` / `-h` / 无参数时输出）。
const USAGE: &str = "\
用法: focusflow-cli <命令> [参数]

  --stats <天数|today|all>   统计最近 N 天 / 今日 / 总计
  --stats-year <年份>        指定年度的统计
  --devices <天数|today|all> 设备维度统计（各键鼠输入次数与占比）
  --backup                   立即备份全部数据库到 backup/（含轮转与残留清理）
  --device-keys [周期]       列出设备标识（device_key）与当前展示名，便于手改别名
  --rename-device <匹配> <别名>
                             给设备取别名（匹配 device_key 或展示名子串；别名留空则还原）
  --list-years               列出有数据的年份
  --export <csv|html>        导出报表到当前目录
  --vacuum                   压缩数据库
  --cleanup <天数>           清理 N 天前的数据
  --reset                    清空所有统计记录（需输入 yes 确认）
  -h, --help                 显示本帮助
";

fn run(args: &[String]) -> i32 {
    if args.is_empty() {
        eprint!("{USAGE}");
        return 1;
    }

    match args[0].as_str() {
        "-h" | "--help" => {
            print!("{USAGE}");
            return 0;
        }
        _ => {}
    }

    // 读取配置（读当前目录 config.ini，不存在则生成默认）
    let _ = focusflow_core::config::instance();

    match args[0].as_str() {
        "--stats" => {
            if args.len() < 2 {
                eprintln!("用法: --stats <天数|today|all>");
                return 1;
            }
            let db = db::Database::init_readonly();
            print_stats(&db, &args[1])
        }
        "--stats-year" => {
            if args.len() < 2 {
                eprintln!("用法: --stats-year <年份>");
                return 1;
            }
            match args[1].parse::<i32>() {
                Ok(year) => {
                    let db = db::Database::init_readonly();
                    print_year_stats(&db, year)
                }
                Err(_) => {
                    eprintln!("无效的年份: {}", args[1]);
                    1
                }
            }
        }
        "--list-years" => {
            let db = db::Database::init_readonly();
            print_list_years(&db)
        }
        "--devices" => {
            if args.len() < 2 {
                eprintln!("用法: --devices <天数|today|all>");
                return 1;
            }
            let db = db::Database::init_readonly();
            print_devices(&db, &args[1])
        }
        "--device-keys" => {
            let db = db::Database::init_readonly();
            print_device_keys(&db)
        }
        "--rename-device" => {
            if args.len() < 3 {
                eprintln!("用法: --rename-device <匹配文本> <别名>");
                eprintln!(
                    "  匹配文本为 device_key 或展示名的子串；别名留空字符串 \"\" 表示还原为自动名"
                );
                return 1;
            }
            rename_device(&args[1], &args[2])
        }
        "--export" => {
            if args.len() < 2 {
                eprintln!("用法: --export <csv|html>");
                return 1;
            }
            let db = db::Database::init_readonly();
            export(&db, &args[1])
        }
        "--reset" => {
            let db = db::Database::init_readonly();
            reset(&db)
        }
        "--vacuum" => {
            db::maintenance::vacuum_all();
            0
        }
        "--backup" => {
            let _ = db::Database::init_readonly();
            let max_backups = focusflow_core::config::instance()
                .get_int("database", "max_backups", 5)
                .max(1);
            match db::maintenance::backup_database(max_backups) {
                Some(path) => {
                    println!("备份完成: {}", path.display());
                    println!(
                        "  目录: {}（已停用插件的数据会跳过）",
                        focusflow_core::paths::backup_dir().display()
                    );
                    0
                }
                None => {
                    eprintln!("备份失败或无可备份的数据库");
                    1
                }
            }
        }
        "--cleanup" => {
            if args.len() < 2 {
                eprintln!("用法: --cleanup <保留天数>");
                return 1;
            }
            match args[1].parse::<i64>() {
                Ok(days) => {
                    let deleted = db::maintenance::cleanup_old_data(days);
                    println!("已删除 {days} 天前的记录 {deleted} 条");
                    0
                }
                Err(_) => {
                    eprintln!("天数必须为整数");
                    1
                }
            }
        }
        "--import-legacy" => {
            if args.len() < 2 {
                eprintln!("用法: --import-legacy <旧数据目录>");
                eprintln!("  从旧版（Python 原版或旧目录）导入全部数据到当前数据目录");
                return 1;
            }
            import_legacy(&args[1])
        }
        other => {
            eprintln!("未知参数: {other}");
            1
        }
    }
}

/// 执行旧数据导入并打印汇总。
fn import_legacy(src_dir: &str) -> i32 {
    let src = std::path::Path::new(src_dir);
    if !src.is_dir() {
        eprintln!("旧数据目录不存在或不是目录: {src_dir}");
        return 1;
    }
    let summary = focusflow_core::migration::import_legacy_data(src);
    println!("\n=== 旧数据导入完成 ===");
    if summary.year_dbs.is_empty() && summary.copied_aux.is_empty() {
        println!("未发现可导入的数据");
    }
    for (year, count) in &summary.records_by_year {
        println!("  {year} 年度键鼠: {count} 条记录");
    }
    if !summary.copied_aux.is_empty() {
        println!("  附属数据: {}", summary.copied_aux.join(", "));
    }
    // 覆盖前的留档路径：选错导入目录时用户靠它找回原数据
    if !summary.backed_up_aux.is_empty() {
        println!("  原数据已留档（覆盖前）：");
        for kept in &summary.backed_up_aux {
            println!("    {kept}");
        }
    }
    if !summary.skipped.is_empty() {
        println!("  跳过: {}", summary.skipped.join(", "));
    }
    if !summary.errors.is_empty() {
        println!("  错误:");
        for e in &summary.errors {
            println!("    {e}");
        }
    }
    println!();
    if summary.errors.is_empty() {
        0
    } else {
        1
    }
}

fn print_stats(_db: &db::Database, period: &str) -> i32 {
    let (total, stats, label) = match period.to_lowercase().as_str() {
        "today" => {
            let d = Local::now().date_naive();
            let (t, s) = db::get_stats_by_date(d);
            (t, s, "今日".to_string())
        }
        "all" => {
            let (t, s) = db::get_stats(None, None);
            (t, s, "总计".to_string())
        }
        _ => match period.parse::<i64>() {
            Ok(days) => {
                let (t, s) = db::get_stats(Some(days), None);
                (t, s, format!("最近 {days} 天"))
            }
            Err(_) => {
                eprintln!("无效的参数: {period}（应为数字、today 或 all）");
                return 1;
            }
        },
    };

    println!("\n{}", "=".repeat(50));
    println!("  FocusFlow 活跃统计 - {label}");
    println!("{}", "=".repeat(50));
    println!("  总活跃次数: {}", fmt_thousands(total));
    println!("{}", "-".repeat(50));
    println!("  {:<6}{:<12}{:<12}{:<10}", "排名", "键鼠", "次数", "占比");
    println!("  {}", "-".repeat(40));
    let mut rank = 0;
    let mut sorted: Vec<(&String, &i64)> = stats.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(a.1));
    for (key, count) in sorted {
        rank += 1;
        let percent = if total > 0 {
            format!("{:.1}%", (*count as f64 / total as f64) * 100.0)
        } else {
            "0%".to_string()
        };
        println!(
            "  {rank:<6}{key:<12}{:<12}{percent:<10}",
            fmt_thousands(*count)
        );
        if rank >= 20 {
            println!("  ... 共 {} 种键鼠", stats.len());
            break;
        }
    }
    println!("{}\n", "=".repeat(50));
    0
}

/// 设备维度统计：各键鼠设备的输入次数与占比（独立口径，见 device_stats.rs）。
fn print_devices(_db: &db::Database, period: &str) -> i32 {
    let (total, devices, label) = match period.to_lowercase().as_str() {
        "today" => {
            let (t, s) = db::get_device_stats_by_date(Local::now().date_naive());
            (t, s, "今日".to_string())
        }
        "all" => {
            let (t, s) = db::get_device_stats(None, None);
            (t, s, "总计".to_string())
        }
        _ => match period.parse::<i64>() {
            Ok(days) => {
                let (t, s) = db::get_device_stats(Some(days), None);
                (t, s, format!("最近 {days} 天"))
            }
            Err(_) => {
                eprintln!("无效的参数: {period}（应为数字、today 或 all）");
                return 1;
            }
        },
    };

    println!("\n{}", "=".repeat(60));
    println!("  FocusFlow 设备统计 - {label}");
    println!("{}", "=".repeat(60));
    println!(
        "  输入总次数: {}（口径：键盘按下 + 鼠标按键 + 滚轮）",
        fmt_thousands(total)
    );
    println!("{}", "-".repeat(60));
    if devices.is_empty() {
        println!("  暂无设备数据（功能上线后开始积累）");
        println!("{}\n", "=".repeat(60));
        return 0;
    }
    println!("  {:<6}{:<44}{:<8}{:<8}", "排名", "设备", "类型", "占比");
    println!("  {}", "-".repeat(56));
    for (rank, dev) in devices.iter().enumerate() {
        let percent = if total > 0 {
            format!("{:.1}%", (dev.count as f64 / total as f64) * 100.0)
        } else {
            "0%".to_string()
        };
        let kind = match dev.kind.as_str() {
            "mouse" => "鼠标",
            "keyboard" => "键盘",
            "hybrid" => "键鼠",
            _ => "未知",
        };
        println!("  {:<6}{:<44}{:<8}{:<8}", rank + 1, dev.name, kind, percent);
    }
    println!("{}\n", "=".repeat(60));
    0
}

/// 列出设备标识与展示名：便于手改 device_aliases.json 或配合 --rename-device。
fn print_device_keys(_db: &db::Database) -> i32 {
    let (total, devices) = db::get_device_stats(None, None);
    if devices.is_empty() {
        println!("暂无设备数据（设备统计从功能上线后开始积累）");
        return 0;
    }
    println!(
        "\n共 {} 个设备，输入总次数 {}\n",
        devices.len(),
        fmt_thousands(total)
    );
    for d in &devices {
        println!("  展示名: {}", d.name);
        println!("  自动名: {}", d.auto_name);
        println!("  device_key: {}\n", d.key);
    }
    0
}

/// 给设备取别名：匹配 device_key、展示名或自动名的子串（区分大小写不敏感）。
/// 别名给空字符串表示还原为自动名。
fn rename_device(pattern: &str, alias: &str) -> i32 {
    let (_, devices) = db::get_device_stats(None, None);
    let needle = pattern.to_lowercase();
    let matched: Vec<&db::DeviceStat> = devices
        .iter()
        .filter(|d| {
            d.key.to_lowercase().contains(&needle)
                || d.name.to_lowercase().contains(&needle)
                || d.auto_name.to_lowercase().contains(&needle)
        })
        .collect();

    let target = match matched.len() {
        0 => {
            eprintln!("未找到匹配的设备: {pattern}");
            eprintln!("  用 --device-keys 查看全部设备标识");
            return 1;
        }
        1 => matched[0],
        n => {
            eprintln!("匹配到 {n} 个设备，请换更精确的匹配文本（或直接用 device_key）:");
            for d in matched {
                eprintln!("  {:<40} <- {}", d.name, d.key);
            }
            return 1;
        }
    };

    let saved = focusflow_core::device_alias::clamp_alias(alias);
    match focusflow_core::device_alias::set(&target.key, &saved) {
        Ok(_) => {
            if saved.is_empty() {
                println!("已还原为自动名: {}", target.auto_name);
            } else {
                println!("已设别名: {} -> {}", target.auto_name, saved);
            }
            0
        }
        Err(e) => {
            eprintln!("写入别名失败: {e}");
            1
        }
    }
}

fn print_year_stats(_db: &db::Database, year: i32) -> i32 {
    let (total, stats) = db::get_stats(None, Some(year));
    println!("\n{}", "=".repeat(50));
    println!("  FocusFlow 活跃统计 - {year} 年度");
    println!("{}", "=".repeat(50));
    println!("  总活跃次数: {}", fmt_thousands(total));
    println!("{}", "-".repeat(50));
    let mut rank = 0;
    let mut sorted: Vec<(&String, &i64)> = stats.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(a.1));
    for (key, count) in sorted {
        rank += 1;
        println!("  {rank:<6}{key:<12}{}", fmt_thousands(*count));
        if rank >= 20 {
            break;
        }
    }
    println!("{}\n", "=".repeat(50));
    0
}

fn print_list_years(_db: &db::Database) -> i32 {
    let years = db::available_years();
    if years.is_empty() {
        println!("暂无数据");
        return 0;
    }
    println!("\n有数据的年份：");
    for y in years {
        println!("  {y}");
    }
    println!();
    0
}

fn export(_db: &db::Database, fmt: &str) -> i32 {
    let filepath = std::path::PathBuf::from(format!(
        "focusflow_export.{}",
        if fmt == "csv" { "csv" } else { "html" }
    ));
    let (total, stats) = db::get_stats(None, None);
    let ok = match fmt {
        "csv" => export_csv(&filepath, total, &stats),
        "html" => export_html(&filepath, total, &stats),
        other => {
            eprintln!("不支持的格式: {other}（可选 csv 或 html）");
            return 1;
        }
    };
    if ok {
        println!("已导出到: {}", filepath.display());
        0
    } else {
        println!("导出失败");
        1
    }
}

fn export_csv(
    path: &std::path::Path,
    total: i64,
    stats: &std::collections::HashMap<String, i64>,
) -> bool {
    use std::io::Write;
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
        // 键名可能来自导入的旧库（含逗号/引号/公式前缀），必须转义，否则 CSV 串列
        out.push_str(&format!("{rank},{},{count},{percent}\n", csv_field(key)));
    }
    std::fs::File::create(path)
        .and_then(|mut f| {
            // 带 BOM，Excel 打开中文不乱码
            f.write_all(b"\xef\xbb\xbf")?;
            f.write_all(out.as_bytes())
        })
        .is_ok()
}

fn export_html(
    path: &std::path::Path,
    total: i64,
    stats: &std::collections::HashMap<String, i64>,
) -> bool {
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
        // 键名未转义会污染报告 HTML（`<`/`&` 被当标记解析）
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
    std::fs::write(path, html).is_ok()
}

fn reset(_db: &db::Database) -> i32 {
    println!("警告：将清空所有记录！输入 yes 确认: ");
    use std::io::BufRead;
    let mut line = String::new();
    let stdin = std::io::stdin();
    if stdin.lock().read_line(&mut line).is_err() {
        return 1;
    }
    if line.trim().to_lowercase() != "yes" {
        println!("已取消");
        return 0;
    }
    let total = db::maintenance::reset_all_data();
    println!("所有统计记录已清空 ({} 行)", fmt_thousands(total));
    0
}

#[cfg(test)]
mod tests {
    use super::{export_csv, export_html};
    use std::collections::HashMap;

    /// 回归：CLI 导出曾直接拼接键名 —— 带逗号的键名会让 CSV 串列、
    /// 带 `<`/`&` 的键名会污染 HTML 报告。两者必须与 GUI 导出同样转义。
    #[test]
    fn cli_exports_escape_key_names() {
        let mut stats: HashMap<String, i64> = HashMap::new();
        stats.insert("a,b".to_string(), 5);
        stats.insert("=cmd()".to_string(), 3);
        stats.insert("<b>x&y</b>".to_string(), 2);

        let dir = std::env::temp_dir().join(format!("ff_cli_export_{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let csv = dir.join("out.csv");
        let html = dir.join("out.html");
        let _ = std::fs::remove_file(&csv);
        let _ = std::fs::remove_file(&html);

        assert!(export_csv(&csv, 10, &stats), "CSV 导出应成功");
        assert!(export_html(&html, 10, &stats), "HTML 导出应成功");

        let csv_text = std::fs::read_to_string(&csv).expect("读 CSV");
        assert!(csv_text.contains("\"a,b\""), "逗号字段应加引号: {csv_text}");
        assert!(
            csv_text.contains("'=cmd()"),
            "公式注入应加前导引号: {csv_text}"
        );

        let html_text = std::fs::read_to_string(&html).expect("读 HTML");
        assert!(
            html_text.contains("&lt;b&gt;x&amp;y&lt;/b&gt;"),
            "HTML 键名应转义，不得出现原始尖括号: {html_text}"
        );
        assert!(
            !html_text.contains("<b>x&y</b>"),
            "转义后不应残留未转义形式"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
