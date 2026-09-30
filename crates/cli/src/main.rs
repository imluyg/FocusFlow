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

use focusflow_core::format::{cmp_rank_desc, csv_field, fmt_thousands, html_escape};

fn main() -> ExitCode {
    logger::init_logging();
    // 有待搬运的数据目录就先把家搬了。CLI 与 GUI 读同一套库：只写配置不搬的话，
    // 命令行会对着一个空的新目录统计出 0（就是"列出了这一年、查它却是 0"那一族），
    // 所以这个入口也得自愈，不能只靠 GUI 启动时搬。
    focusflow_core::data_location::run_pending_migration();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = run(&args);
    // 返回前把非阻塞日志缓冲里的内容写完：main 一返回进程就终止，static 里的
    // WorkerGuard 不会自动析构，最后那几条日志（往往正是出错时最想要的）会丢
    logger::shutdown();
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
  --cleanup <天数>           保留 N 天（含今天），删除更早的数据
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
            match parse_stats_year(&args[1]) {
                Err(msg) => {
                    eprintln!("{msg}");
                    1
                }
                Ok(year) => {
                    let db = db::Database::init_readonly();
                    print_year_stats(&db, year)
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
            // 破坏性/改写型命令一律先把**目标目录**说出来：`FOCUSFLOW_APP_DIR` 是
            // app_dir 的第一优先级，测试里临时设过之后再忘，就会把真实数据目录当成
            // 草稿目录处理掉（旧代码从头到尾不提它在动哪套库）。
            println!(
                "压缩全部年度库（数据目录 {}）",
                focusflow_core::paths::data_dir().display()
            );
            let report = db::maintenance::vacuum_all();
            if report.incomplete() {
                // 旧行为是 `vacuum_all(); 0`：每个库的成败都被丢掉，永远退 0，
                // 挂在计划任务上就是"每天准时什么都不做"。
                eprintln!("压缩未完成：{}（原因见日志）", report.why_incomplete());
                1
            } else {
                println!("压缩完成");
                0
            }
        }
        "--backup" => {
            let _ = db::Database::init_readonly();
            let max_backups = focusflow_core::config::instance()
                .get_int("database", "max_backups", 5)
                .max(1);
            match db::maintenance::backup_database(max_backups) {
                db::maintenance::BackupOutcome::Done {
                    first,
                    count,
                    failed,
                } => {
                    println!("备份完成: {}（共 {count} 份）", first.display());
                    println!(
                        "  目录: {}（已停用插件的数据会跳过）",
                        focusflow_core::paths::backup_dir().display()
                    );
                    if failed.is_empty() {
                        0
                    } else {
                        // 部分成功必须退非 0：这条是给计划任务用的，而"5 个库里只备份出
                        // 1 个"在退出码上一直读成成功 —— `Done` 原先不带 `failed`，
                        // 调用方根本没有信息可以区分。
                        eprintln!(
                            "备份不完整: 这些库没能产出通过校验的备份: {failed:?}\
                             （已经备份到的 {count} 份仍然有效）"
                        );
                        1
                    }
                }
                // 没东西可备份不是失败：挂计划任务时退 1 会让用户以为天天在坏，
                // 而真失败的那一次也混在同一句话里看不出来
                db::maintenance::BackupOutcome::NothingToDo => {
                    println!("没有需要备份的数据库（新建/刚重置过，或历史库都与上次一致）");
                    0
                }
                db::maintenance::BackupOutcome::Failed { reason } => {
                    eprintln!("备份失败: {reason}");
                    1
                }
            }
        }
        "--cleanup" => {
            if args.len() < 2 {
                eprintln!("用法: --cleanup <保留天数>");
                return 1;
            }
            match parse_keep_days(&args[1]) {
                Err(msg) => {
                    eprintln!("{msg}，已取消");
                    1
                }
                Ok(days) => {
                    println!(
                        "清理 {days} 天前的记录（数据目录 {}）",
                        focusflow_core::paths::data_dir().display()
                    );
                    let report = db::maintenance::cleanup_old_data(days);
                    let (line, code) = cleanup_outcome(days, &report);
                    // 成功那句话只属于成功：走 stdout；未完成走 stderr（这条是给计划任务用的）
                    if code == 0 {
                        println!("{line}");
                    } else {
                        eprintln!("{line}");
                    }
                    code
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

/// `--stats` / `--devices` 的周期。
enum Period {
    Today,
    All,
    Days(i64),
}

/// 解析周期参数，语义与 UI 的 `set_period` 完全一致：`-1` = 今日、`0` = 总计、
/// `1..=MAX_QUERY_DAYS` = 最近 N 天，`today` / `all` 是字面量别名。
///
/// 之前这里只 `parse::<i64>()` 成功就用，于是 `-5`、`99999999999` 都能通过，而查询层
/// 会把天数静默钳进 `1..=MAX_QUERY_DAYS` —— 标题写着"最近 -5 天"、拿到的却是 1 天的
/// 数据。诊断工具给出一套对不上的口径，比直接报错更糟。
fn parse_period(arg: &str) -> Result<Period, String> {
    let lower = arg.to_lowercase();
    let n = match lower.as_str() {
        "today" => return Ok(Period::Today),
        "all" => return Ok(Period::All),
        other => other
            .parse::<i64>()
            .map_err(|_| format!("无效的参数: {arg}（应为数字、today 或 all）"))?,
    };
    if !db::queries::is_valid_period(n) {
        return Err(format!(
            "无效的天数: {n}（-1 = 今日，0 = 总计，1..={} = 最近 N 天）",
            db::queries::MAX_QUERY_DAYS
        ));
    }
    Ok(match n {
        -1 => Period::Today,
        0 => Period::All,
        d => Period::Days(d),
    })
}

/// 年度库列不出来、或其中某一年打不开时**必须说话**。
///
/// `available_years()` 与 `get_stats(None, None)` 都把"读不出来"折成空列表 / 偏小的
/// 总数，于是这些诊断命令会打印一个自信而错误的数字、还退回退出码 0。
/// 破坏性命令（`--reset` / `--vacuum` / `--cleanup`）上一批已经改成会报原因的版本，
/// 这里补的是留下来的那半边 —— 一个分不清"没有数据"与"读不到数据"的诊断工具，
/// 是这仓库目前产出最高的一类 bug。
fn check_years_readable() -> Result<Vec<i32>, String> {
    let years = db::queries::try_available_years().map_err(|e| format!("年度库列不出来: {e}"))?;
    let bad: Vec<i32> = years
        .iter()
        .copied()
        .filter(|y| db::connection::open_ro(&focusflow_core::paths::year_db_path(*y)).is_err())
        .collect();
    if !bad.is_empty() {
        return Err(format!(
            "这些年度库打不开，它们的计数不会出现在结果里: {bad:?}"
        ));
    }
    Ok(years)
}

fn print_stats(_db: &db::Database, period: &str) -> i32 {
    if let Err(e) = check_years_readable() {
        eprintln!("{e}");
        return 1;
    }
    let (total, stats, label) = match parse_period(period) {
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
        Ok(Period::Today) => {
            let d = Local::now().date_naive();
            let (t, s) = db::get_stats_by_date(d);
            (t, s, "今日".to_string())
        }
        Ok(Period::All) => {
            let (t, s) = db::get_stats(None, None);
            (t, s, "总计".to_string())
        }
        Ok(Period::Days(days)) => {
            let (t, s) = db::get_stats(Some(days), None);
            (t, s, format!("最近 {days} 天"))
        }
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
    sorted.sort_by(|a, b| cmp_rank_desc(a.0, a.1, b.0, b.1));
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
    if let Err(e) = check_years_readable() {
        eprintln!("{e}");
        return 1;
    }
    let (total, devices, label) = match parse_period(period) {
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
        Ok(Period::Today) => {
            let (t, s) = db::get_device_stats_by_date(Local::now().date_naive());
            (t, s, "今日".to_string())
        }
        Ok(Period::All) => {
            let (t, s) = db::get_device_stats(None, None);
            (t, s, "总计".to_string())
        }
        Ok(Period::Days(days)) => {
            let (t, s) = db::get_device_stats(Some(days), None);
            (t, s, format!("最近 {days} 天"))
        }
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
    if let Err(e) = check_years_readable() {
        eprintln!("{e}");
        return 1;
    }
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
    // 这条是"读一遍统计再写别名"：读不全时给出的匹配结果本身就是残缺的，
    // 而用户会以为设备名改错了（不是没读到）。所以同样要先过这道闸。
    if let Err(e) = check_years_readable() {
        eprintln!("{e}");
        return 1;
    }
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

/// `--stats-year` 先要回答"这一年能不能查"。
///
/// 三种"读不到"以前印成同一句「总活跃次数: 0」并退 0：清单列不出来、那一年的库
/// 打不开、那一年真的没有库。前两种是**故障**（挂在计划任务上退 0 就是"每天准时
/// 什么都不做却报告成功"），只有第三种才配得上一个 0。
#[derive(Debug)]
enum YearLookup {
    /// 能查（不代表有数据 —— 那一年本身可能就是空的）
    Queryable,
    /// 这一年没有可查的年度库；带着现有年份清单
    NoLibrary(Vec<i32>),
    /// 读不出可靠清单
    Unreadable(String),
}

/// 纯函数版判定，好让三种结局能被单测钉住（`check_years_readable` 要碰真库）。
fn year_lookup(year: i32, readable: Result<Vec<i32>, String>) -> YearLookup {
    match readable {
        Err(e) => YearLookup::Unreadable(e),
        Ok(known) if known.contains(&year) => YearLookup::Queryable,
        Ok(known) => YearLookup::NoLibrary(known),
    }
}

fn print_year_stats(_db: &db::Database, year: i32) -> i32 {
    match year_lookup(year, check_years_readable()) {
        YearLookup::Unreadable(e) => {
            eprintln!("{e}");
            return 1;
        }
        YearLookup::NoLibrary(known) => {
            println!("  {year} 年没有可查的年度库（现有年份: {known:?}）");
            return 0;
        }
        YearLookup::Queryable => {}
    }
    let (total, stats) = db::get_stats(None, Some(year));
    println!("\n{}", "=".repeat(50));
    println!("  FocusFlow 活跃统计 - {year} 年度");
    println!("{}", "=".repeat(50));
    println!("  总活跃次数: {}", fmt_thousands(total));
    println!("{}", "-".repeat(50));
    let mut rank = 0;
    let mut sorted: Vec<(&String, &i64)> = stats.iter().collect();
    sorted.sort_by(|a, b| cmp_rank_desc(a.0, a.1, b.0, b.1));
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
    let years = match check_years_readable() {
        Ok(y) => y,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
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
    if let Err(e) = check_years_readable() {
        eprintln!("导出的会是一份偏小的数据：{e}");
        return 1;
    }
    let (total, stats) = db::get_stats(None, None);
    // 导出文件名是相对路径 ⇒ 成败都要把**绝对位置**说出来：写到当前目录这件事，
    // 对挂着计划任务的人来说从来不是显然的（当前目录可能是 System32）。
    let target = match std::env::current_dir() {
        Ok(dir) => dir.join(&filepath),
        Err(_) => filepath.clone(),
    };
    let outcome = match fmt {
        "csv" => export_csv(&filepath, total, &stats),
        "html" => export_html(&filepath, total, &stats),
        other => {
            eprintln!("不支持的格式: {other}（可选 csv 或 html）");
            return 1;
        }
    };
    match outcome {
        Ok(()) => {
            println!("已导出到: {}", target.display());
            0
        }
        // 失败的话要说在 stderr，并带上 OS 给的原因和写不出去的那个位置：
        // 旧写法 `.is_ok()` 把错误丢了，`println!("导出失败")` 连目录都不给，
        // 而"当前目录没权限"与"盘满了"用户完全无从区分。
        Err(e) => {
            eprintln!("导出失败（目标 {}）: {e}", target.display());
            1
        }
    }
}

fn export_csv(
    path: &std::path::Path,
    total: i64,
    stats: &std::collections::HashMap<String, i64>,
) -> Result<(), String> {
    use std::io::Write;
    let mut sorted: Vec<(&String, &i64)> = stats.iter().collect();
    sorted.sort_by(|a, b| cmp_rank_desc(a.0, a.1, b.0, b.1));
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
    let written = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(path)?;
        // 带 BOM，Excel 打开中文不乱码
        f.write_all(b"\xef\xbb\xbf")?;
        f.write_all(out.as_bytes())
    })();
    written.map_err(|e| format!("写 {} 失败: {e}", path.display()))
}

fn export_html(
    path: &std::path::Path,
    total: i64,
    stats: &std::collections::HashMap<String, i64>,
) -> Result<(), String> {
    let mut sorted: Vec<(&String, &i64)> = stats.iter().collect();
    sorted.sort_by(|a, b| cmp_rank_desc(a.0, a.1, b.0, b.1));
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
    std::fs::write(path, html).map_err(|e| format!("写 {} 失败: {e}", path.display()))
}

/// `--cleanup <保留天数>` 的取值口径：必须落在 1..=3660。
///
/// 与 `parse_period` 同一个理由：core 对非法天数只会返回 0，直接打「已删除 0 条」
/// 看起来就像成功，用户不会发现自己把天数打成了 0 或负数（那会连今天一起删）。
/// 上限是给打错字兜底的：旧代码接受任意 `>= 1`，于是 `--cleanup 3000` 与
/// `--cleanup 999999` 都合法，而它们的真实含义是"把历史全删了"；十年以上的保留期
/// 没有合理用途，超出就当笔误处理。
fn parse_keep_days(raw: &str) -> Result<i64, String> {
    let days = raw
        .parse::<i64>()
        .map_err(|_| format!("保留天数必须是整数（收到 {raw:?}）"))?;
    if days < 1 {
        return Err(format!(
            "保留天数必须 >= 1（收到 {days}）：0 或负数会连今天一起删掉"
        ));
    }
    if days > 3660 {
        return Err(format!(
            "保留天数 {days} 超出可理解的范围（上限 3660 天）：这个值等价于清空全部历史，\
             真要清空请用 --reset"
        ));
    }
    Ok(days)
}

/// `--reset` 在向人要确认**之前**必须先看清要清哪些库（三种结局；纯函数好钉住）。
#[derive(Debug)]
enum ResetPlan {
    /// 列不出来，或列出来的年度库里有打不开的：不能让人对着一份看不见的清单按 yes。
    ///
    /// 这里刻意是"整套操作都不做"而不是"清能清的那几个"：确认这一步的意义就在于
    /// 用户知道自己批准的是什么，一份缺了年份的清单已经不是他批准的那件事了。
    Blocked(String),
    /// 一套可清的年度库都没有 ⇒ 没有破坏性可做，连"输入 yes"都不该讨
    /// （`reset_all_data` 清的就是这批库：逐年 DELETE 聚合表与 `devices`）。
    NothingToClear,
    /// 确认提示里逐条列出的年份，与 `reset_all_data` 将要处理的清单同一来源。
    Confirm(Vec<i32>),
}

fn reset_plan(readable: Result<Vec<i32>, String>) -> ResetPlan {
    match readable {
        Err(e) => ResetPlan::Blocked(e),
        Ok(years) if years.is_empty() => ResetPlan::NothingToClear,
        Ok(years) => ResetPlan::Confirm(years),
    }
}

fn reset(_db: &db::Database) -> i32 {
    // 确认提示必须说清楚要清的是哪一套库、哪些年份：这是唯一一个有确认的破坏性命令，
    // 而它原先只写"清空所有记录"—— 用户无从发现 `FOCUSFLOW_APP_DIR` 还指着真实数据目录。
    //
    // 清单走 `check_years_readable`，与 `reset_all_data` 里 `years_or_record` 同一把尺子。
    // 原先这里用 `available_years()`：它把"目录读不出来"和"每个年度库都打不开"都折成
    // `[]` ⇒ 提示里印一个空的 `[]` 还照旧讨一次 yes，用户在看不见要清什么的情况下
    // 确认了破坏性操作，而真正的失败要到下一步才听得见（那时执行前快照都已经做过了）。
    let years = match reset_plan(check_years_readable()) {
        ResetPlan::Blocked(e) => {
            eprintln!("无法确认要清空哪些库，已取消（没有改动任何数据）: {e}");
            return 1;
        }
        ResetPlan::NothingToClear => {
            println!(
                "没有需要清空的年度库（数据目录 {} 里没有带聚合数据的 focusflow_年份.db）",
                focusflow_core::paths::data_dir().display()
            );
            return 0;
        }
        ResetPlan::Confirm(years) => years,
    };
    println!(
        "警告：将清空数据目录 {} 下这些年份的全部统计记录：{years:?}\n输入 yes 确认: ",
        focusflow_core::paths::data_dir().display()
    );
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
    let report = db::maintenance::reset_all_data();
    if report.incomplete() {
        eprintln!(
            "只清掉了一部分：{}（这些年份已回滚，行还在）。\
             先关掉正在运行的 FocusFlow 再重试。",
            report.why_incomplete()
        );
        return 1;
    }
    println!("所有统计记录已清空 ({} 行)", fmt_thousands(report.deleted));
    0
}

/// `--stats-year <年份>` 的取值口径：1000..=9999（四位数）。
///
/// 旧代码只 `parse::<i32>()`，于是 `--stats-year 0`、`-5`、`99999` 都能进去跑一圈，
/// 而给出来的却是与"那一年真的没数据"一模一样的那句「总活跃次数: 0」+ 退 0。
/// 下界取 1000 而不是 1970：年份就是年度库文件名的一部分（`focusflow_2026.db`），
/// `paths::is_year_db_file` 只认四位十进制，再小的数字连库都拼不出来；而 1970 **之前**
/// 的记录现在有自己的年份库（core 的 `day_key_to_date` 已能解负 `day_key`，归档那侧
/// 会把它们搬进 `focusflow_1969.db` 这类真实年份），所以 `--stats-year 1969` 是个合法
/// 问题，不该被当成笔误拒掉。
fn parse_stats_year(raw: &str) -> Result<i32, String> {
    let year = raw
        .trim()
        .parse::<i32>()
        .map_err(|_| format!("无效的年份: {raw}（需要 1000..=9999 的四位数字）"))?;
    if !(1000..=9999).contains(&year) {
        return Err(format!(
            "年份 {year} 不在可查范围（1000..=9999）内：年度库的文件名是 focusflow_YYYY.db，\
             四位数之外的年份没有对应的库"
        ));
    }
    Ok(year)
}

/// `--cleanup` 的结论：**先**判有没有失败，再决定说哪句话。
///
/// 旧写法把「已删除 N 条」排在 `incomplete()` 之前，于是一轮"每个库都没打开"的清理
/// 也会先往 stdout 打出「已删除 30 天前的记录 0 条」—— 看着就是成功，而这条命令是
/// 挂在计划任务上的（第 57 轮 `c4aee24` 已经定过口径：结论必须带上失败项）。
/// 返回（要打的那句, 退出码）。
fn cleanup_outcome(days: i64, report: &db::maintenance::MaintenanceReport) -> (String, i32) {
    if report.incomplete() {
        (
            format!(
                "清理未完成：{}（已回滚的年份未删的行还在；原因见日志）",
                report.why_incomplete()
            ),
            1,
        )
    } else {
        (
            format!(
                "已删除 {days} 天前的记录 {} 条",
                fmt_thousands(report.deleted)
            ),
            0,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{cleanup_outcome, export_csv, export_html, year_lookup, YearLookup};
    use super::{parse_keep_days, parse_period, parse_stats_year, Period};
    use super::{reset_plan, ResetPlan};
    use focusflow_core::db::maintenance::MaintenanceReport;

    /// `--cleanup` 的天数口径：0 / 负数 / 非整数 / 大得离谱都要在入口挡住。
    ///
    /// 旧代码只挡 `< 1`，于是 `--cleanup 3000` 与 `--cleanup 999999` 一路放行，
    /// 而它们实际等价于"把历史全删了"。core 那边对非法值只返回 0，直接打
    /// 「已删除 0 条」看起来就是成功。
    #[test]
    fn keep_days_argument_is_bounded() {
        assert_eq!(parse_keep_days("30"), Ok(30));
        assert_eq!(parse_keep_days("1"), Ok(1));
        assert_eq!(parse_keep_days("3660"), Ok(3660), "上限本身是合法保留期");
        for bad in [
            "0",
            "-5",
            "abc",
            "",
            " ",
            "30.5",
            "3661",
            "999999",
            "99999999999",
        ] {
            assert!(parse_keep_days(bad).is_err(), "{bad} 应当被拒绝");
        }
        // 文案要能分清是哪一种：会连今天一起删，与"笔误"是两件事
        assert!(parse_keep_days("0").unwrap_err().contains(">= 1"));
        assert!(parse_keep_days("999999").unwrap_err().contains("--reset"));
    }

    /// 周期参数必须与 UI 的 `set_period` 同一口径，越界值要报错而不是被静默钳制。
    #[test]
    fn period_argument_matches_the_ui_semantics() {
        assert!(matches!(parse_period("today"), Ok(Period::Today)));
        assert!(matches!(parse_period("ALL"), Ok(Period::All)));
        assert!(matches!(parse_period("7"), Ok(Period::Days(7))));
        // -1 / 0 在 UI 里就是今日 / 总计，之前会被印成"最近 -1 天""最近 0 天"
        assert!(matches!(parse_period("-1"), Ok(Period::Today)));
        assert!(matches!(parse_period("0"), Ok(Period::All)));
        // 越界与非法：以前 -5 会被钳成 1 天、99999999999 被钳成上限，
        // 表格照出、标题却印着原始数字
        for bad in ["-5", "99999999999", "abc", "", " "].iter() {
            assert!(parse_period(bad).is_err(), "{bad} 应当被拒绝");
        }
    }
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

        export_csv(&csv, 10, &stats).expect("CSV 导出应成功");
        export_html(&html, 10, &stats).expect("HTML 导出应成功");

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

    /// CLI 的每一个排名都必须走那**一个**比较器（`focusflow_core::format::cmp_rank_desc`）。
    ///
    /// 为什么单独给 CLI 钉一条，而不是靠桌面侧那条同名的用例：同一把尺子在三个地方各写
    /// 一遍，已经因此栽过两回 —— 第一次是导出转义（CLI 那份漏了转义，见 `format.rs` 模块头），
    /// 第二次是同分破平（屏幕上先修了，桌面导出与 CLI 这八处全漏）。漏法不报错也不崩溃：
    /// 榜的输入是 `HashMap`，每进程随机序，而排序后面紧跟着 `truncate` / `take` /
    /// `if rank >= N { break }` ⇒ **谁进榜**每次跑 CLI 都可能不一样。
    /// 桌面那条用例管不到这个 crate，所以覆盖面要在这儿自己盯住。
    #[test]
    fn every_cli_rank_goes_through_the_one_comparator() {
        let src = include_str!("main.rs");
        let prod = &src[..src.find("mod tests").expect("测试模块的起点找不到了")];
        let sorts: Vec<usize> = prod.match_indices(".sort_by(").map(|(i, _)| i).collect();
        assert_eq!(
            sorts.len(),
            4,
            "CLI 的排名应当正好四处（两个统计面板 + CSV / HTML 导出），现在数到 {} 处 ——\
             加了新的榜就要一并进这条断言",
            sorts.len()
        );
        for (n, at) in sorts.iter().enumerate() {
            let line = &prod[*at..prod[*at..].find('\n').unwrap_or(prod.len() - *at) + *at];
            assert!(
                line.contains("cmp_rank_desc"),
                "第 {} 处排名没走共用比较器：{line}\n\
                 只排次数 = 同分键谁进榜随 HashMap 种子变",
                n + 1
            );
        }
    }
    /// 年份取值要夹住。旧代码只要 `parse::<i32>()` 成功就用，于是 `--stats-year 0`、
    /// `-5`、`99999` 一路跑完，给出的却是与"那一年真的没数据"同一句话。
    #[test]
    fn stats_year_argument_is_bounded() {
        assert_eq!(parse_stats_year("2025"), Ok(2025));
        assert_eq!(
            parse_stats_year(" 2026 "),
            Ok(2026),
            "首尾空格是命令行常见写法"
        );
        assert_eq!(parse_stats_year("1000"), Ok(1000), "边界本身合法");
        assert_eq!(parse_stats_year("9999"), Ok(9999), "边界本身合法");
        // 1970 之前是**合法问题**：那一年的记录有自己的年份库（core 那边已经能解负
        // day_key 并按真实年份归档），把它当笔误拒掉会把数据锁死在查不到的地方。
        assert_eq!(
            parse_stats_year("1969"),
            Ok(1969),
            "历元之前的年份照样要能问"
        );
        for bad in ["0", "-5", "999", "10000", "999999", "abc", "", " ", "20.5"] {
            assert!(parse_stats_year(bad).is_err(), "{bad} 应当被拒绝");
        }
        // 文案要能分清"不是数字"与"数字超出可查范围"
        assert!(parse_stats_year("abc").unwrap_err().contains("无效的年份"));
        assert!(
            parse_stats_year("999")
                .unwrap_err()
                .contains("不在可查范围"),
            "实得: {}",
            parse_stats_year("999").unwrap_err()
        );
    }

    /// 导出失败必须说出**哪个文件**与 OS 给的原因。
    ///
    /// 旧写法是 `.is_ok()`：错误当场丢掉，调用方只能印一句"导出失败"，还在 stdout，
    /// 也不说写到哪儿去了 —— 而"当前目录没权限"与"盘满了"是完全不同的两件事，
    /// 计划任务里前者天天坏、后者某天突然坏，用户看到的都是同样四个字。
    #[test]
    fn a_failed_export_names_the_file_and_the_os_error() {
        let mut stats: HashMap<String, i64> = HashMap::new();
        stats.insert("键盘".to_string(), 7);
        let dir = std::env::temp_dir().join(format!("ff_cli_fail_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("造临时目录");
        // 夹具：把一个目录摆在要写的位置上，File::create 与 fs::write 都会失败
        let blocked = dir.join("out.csv");
        std::fs::create_dir_all(&blocked).expect("把 csv 目标做成一个目录");
        let e = export_csv(&blocked, 7, &stats).expect_err("目标是个目录时必须失败");
        assert!(
            e.contains(&blocked.display().to_string()),
            "要说清楚写不出去的是哪个文件，实得: {e}",
        );
        assert!(e.len() > 20, "还得带上 OS 给的原因，实得: {e}");
        let blocked_html = dir.join("out.html");
        std::fs::create_dir_all(&blocked_html).expect("把 html 目标做成一个目录");
        let e2 = export_html(&blocked_html, 7, &stats).expect_err("同上");
        assert!(e2.contains("out.html"), "实得: {e2}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 「已删除 N 条」只能在**没有失败项**的时候说。
    ///
    /// 旧写法把成功句排在 `report.incomplete()` 之前 ⇒ 一轮"每个库都没打开"的清理
    /// 也会先往 stdout 打出「已删除 30 天前的记录 0 条」，挂在计划任务上就是成功。
    #[test]
    fn cleanup_says_success_only_when_nothing_failed() {
        let ok = MaintenanceReport {
            deleted: 12,
            failed_years: vec![],
            dir_error: None,
        };
        let (line, code) = cleanup_outcome(30, &ok);
        assert_eq!(code, 0);
        assert!(
            line.contains("已删除") && line.contains("12"),
            "实得: {line}"
        );

        // 部分失败：这一句绝不能出现
        let partial = MaintenanceReport {
            deleted: 0,
            failed_years: vec![2025],
            dir_error: None,
        };
        let (line, code) = cleanup_outcome(30, &partial);
        assert_eq!(code, 1);
        assert!(
            !line.contains("已删除"),
            "没做成的那一轮不能报删除数，实得: {line}"
        );
        assert!(
            line.contains("未完成") && line.contains("2025"),
            "实得: {line}"
        );

        // 连目录都列不出来：一套库都没碰
        let nodir = MaintenanceReport {
            deleted: 0,
            failed_years: vec![],
            dir_error: Some("读取数据目录失败".to_string()),
        };
        let (line, code) = cleanup_outcome(30, &nodir);
        assert_eq!(code, 1, "目录读不出来不是「没东西可删」");
        assert!(!line.contains("已删除"), "实得: {line}");
    }

    /// 七条读命令都必须先过 `check_years_readable`。
    ///
    /// 这道闸原先只罩着其中三条，剩下四条把"读不到"折成 0 / 空表照样印出来并退 0。
    /// 与本文件那条排名守卫同一个理由：**这个 crate 的覆盖面要在这儿自己盯住**，
    /// 桌面侧的用例管不到它，而"加了一条读命令却忘了出声"是这条线上反复出现的漏法。
    #[test]
    fn every_cli_read_command_checks_the_years_are_readable() {
        let src = include_str!("main.rs");
        let prod = &src[..src.find("mod tests").expect("测试模块的起点找不到了")];
        let readers = [
            "print_stats",
            "print_year_stats",
            "print_list_years",
            "print_devices",
            "print_device_keys",
            "rename_device",
            "export",
        ];
        for name in readers {
            let at = prod
                .find(&format!("fn {name}("))
                .unwrap_or_else(|| panic!("读命令的函数不见了: {name}"));
            let rest = &prod[at..];
            let end = rest[1..].find("\nfn ").map(|n| n + 1).unwrap_or(rest.len());
            let body = &rest[..end];
            assert!(
                body.contains("check_years_readable"),
                "{name} 没有先问年度库读不读得动 —— 它会对着读不到的库印一个自信的数字并退 0",
            );
        }
    }
    /// `--stats-year` 的三种"读不到"必须是三句不同的话。
    ///
    /// 旧写法把它们全印成同一句「总活跃次数: 0」并退 0 —— 一个分不清"没有数据"与
    /// "读不到数据"的诊断命令，正是这仓库产出最高的一类 bug。
    #[test]
    fn a_year_that_cannot_be_read_is_not_reported_as_an_empty_year() {
        assert!(matches!(
            year_lookup(2026, Ok(vec![2026, 2025])),
            YearLookup::Queryable
        ));
        match year_lookup(2024, Ok(vec![2026, 2025])) {
            YearLookup::NoLibrary(known) => {
                assert_eq!(known, vec![2026, 2025], "要把可选的年份一起说出来");
            }
            other => panic!("没有库要报 NoLibrary，实得 {other:?}"),
        }
        let bad = "这些年度库打不开，它们的计数不会出现在结果里: [2025]";
        match year_lookup(2025, Err(bad.to_string())) {
            YearLookup::Unreadable(e) => assert_eq!(e, bad),
            other => panic!("读不动要报 Unreadable（它不是\"没有数据\"），实得 {other:?}"),
        }
    }
    /// `--reset` 不能对着一份看不见的清单讨一次 yes。
    ///
    /// 旧写法用 `available_years()`（它把「读不出来」折成 `[]`），于是提示里印 `[]`、
    /// 用户照样确认，失败要到 `reset_all_data` 那边才说出口 —— 那时执行前快照都做过了。
    /// 三种结局必须是三句话：能清（逐条列年份）、没得清（直接收工、不讨确认）、
    /// 看不清（拒绝并说明原因，且一个字节都不动）。
    #[test]
    fn reset_never_asks_confirmation_on_a_list_it_cannot_read() {
        match reset_plan(Ok(vec![2026, 2025])) {
            ResetPlan::Confirm(years) => {
                assert_eq!(years, vec![2026, 2025], "确认提示要拿到将要清的年份清单");
            }
            other => panic!("读得动就该 Confirm，实得 {other:?}"),
        }
        assert!(
            matches!(reset_plan(Ok(Vec::new())), ResetPlan::NothingToClear),
            "一套库都没有时不该讨一次 yes，更不该做执行前快照"
        );
        let bad = "这些年度库打不开，它们的计数不会出现在结果里: [2025]";
        match reset_plan(Err(bad.to_string())) {
            ResetPlan::Blocked(e) => assert_eq!(e, bad, "拒绝的原因要原样递给用户"),
            other => panic!("读不出清单必须 Blocked，不能退化成空白确认: {other:?}"),
        }
    }
}
