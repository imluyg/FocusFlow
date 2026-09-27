//! 性能探针（默认 ignore，不进常规门禁）：
//!
//! ```text
//! cargo test -p focusflow-core --test perf_probe -- --ignored --nocapture
//! ```
//!
//! 在隔离 app_dir 里种出**他机器量级**的两份年度库（各 365 天：key_counts 150 键/天、
//! device_key_counts 5 设备 × 40 键/天、hourly 24/天），然后逐项量：
//! - 启动 DB 侧成本（`Database::init` 在这些数据上的全序列）
//! - 打字热路径（`record_key` 吞吐）+ 一次 flush 的落库成本
//! - 查询面：设备详情（点一次的真实成本）/ 排行 / 导出全史 / 目标窗口 / 小时分布
//! - 退出成本（`shutdown` = flush + stop + backup_on_exit 的备份与校验）
//! - 备份单项（backup_database 热身后的稳态）
//! - B14-2 迁移扫描（迁完之后每次启动的剩余成本）与配置整读整写
//!
//! 数字出来后对照两条人感线：100 ms（可感知）与 16 ms（一帧）。

use std::time::Instant;

use chrono::NaiveDate;

use focusflow_core::config::FocusFlowConfig;
use focusflow_core::db::{self, connection, maintenance, queries, Database};
use focusflow_core::paths;

const YEARS: [i32; 2] = [2025, 2026];
const KEYS_PER_DAY: usize = 150;
const DEVICES: usize = 5;
const DEVKEYS_PER_DEVICE_PER_DAY: usize = 40;

fn epoch_day(y: i32, m: u32, d: u32) -> i64 {
    NaiveDate::from_ymd_opt(y, m, d)
        .unwrap()
        .signed_duration_since(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap())
        .num_days()
}

fn key_names() -> Vec<String> {
    // 150 个稳定键名（同一天内重复使用，跨天也复用 —— 真实键盘就是这个形状）
    (0..KEYS_PER_DAY).map(|i| format!("Key_{:03}", i)).collect()
}

fn seed_year(year: i32) -> anyhow::Result<()> {
    let path = paths::year_db_path(year);
    let conn = connection::open_rw(&path)?;
    connection::ensure_schema(&conn, year)?;
    let keys = key_names();
    let device_keys: Vec<String> = (0..DEVICES)
        .map(|i| {
            [
                "VID_046D&PID_C52B&MI_00",
                "VID_046D&PID_C52B&MI_01",
                "VID_24AE&PID_1464&MI_00",
                "VID_1532&PID_0094&MI_00",
                "VID_1B1C&PID_1B2D",
            ][i]
                .to_string()
        })
        .collect();
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    // 设备登记（B14-2 身份键形状）
    for (i, dk) in device_keys.iter().enumerate() {
        conn.execute(
            "INSERT OR IGNORE INTO devices (id, device_key, name, kind) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                (i + 1) as i64,
                dk,
                format!("设备 {}", i + 1),
                if i % 2 == 0 { "keyboard" } else { "mouse" }
            ],
        )?;
    }
    let mut stmt_daily =
        conn.prepare("INSERT INTO daily_counts (date_key, count, seconds) VALUES (?1, ?2, ?3)")?;
    let mut stmt_hourly =
        conn.prepare("INSERT INTO hourly_counts (date_key, hour, count) VALUES (?1, ?2, ?3)")?;
    let mut stmt_key =
        conn.prepare("INSERT INTO key_counts (date_key, key_name, count) VALUES (?1, ?2, ?3)")?;
    let mut stmt_app =
        conn.prepare("INSERT INTO app_usage (date_key, app_name, seconds) VALUES (?1, ?2, ?3)")?;
    let mut stmt_dev =
        conn.prepare("INSERT INTO device_counts (date_key, device_id, count) VALUES (?1, ?2, ?3)")?;
    let mut stmt_devkey = conn.prepare("INSERT INTO device_key_counts (date_key, device_id, key_name, count) VALUES (?1, ?2, ?3, ?4)")?;
    for day in 0..365i64 {
        let dk = epoch_day(year, 1, 1) + day;
        let total = 18_000 + (day % 40) * 100;
        stmt_daily.execute(rusqlite::params![dk, total, 20_000 + (day % 30) * 60])?;
        for h in 0..24i64 {
            stmt_hourly.execute(rusqlite::params![dk, h, total / 30])?;
        }
        for (i, k) in keys.iter().enumerate() {
            stmt_key.execute(rusqlite::params![dk, k, 50 + (day + i as i64) % 300])?;
        }
        for a in 0..8 {
            stmt_app.execute(rusqlite::params![dk, format!("app{}.exe", a), 1_800 + a])?;
        }
        for dev in 0..DEVICES {
            stmt_dev.execute(rusqlite::params![
                dk,
                (dev + 1) as i64,
                total / DEVICES as i64
            ])?;
            for i in 0..DEVKEYS_PER_DEVICE_PER_DAY {
                let k = &keys[(day as usize + dev * 11 + i) % KEYS_PER_DAY];
                stmt_devkey.execute(rusqlite::params![
                    dk,
                    (dev + 1) as i64,
                    k,
                    3 + (i as i64 % 7)
                ])?;
            }
        }
    }
    conn.execute_batch("COMMIT;")?;
    Ok(())
}

fn bench<T>(name: &str, f: impl FnOnce() -> T) -> (T, f64) {
    let t = Instant::now();
    let out = f();
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    println!("  {:<46} {:>9.1} ms", name, ms);
    (out, ms)
}

fn bench_avg(name: &str, runs: usize, mut f: impl FnMut()) {
    let mut total = 0.0;
    let mut min = f64::MAX;
    for _ in 0..runs {
        let t = Instant::now();
        f();
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        total += ms;
        min = min.min(ms);
    }
    println!(
        "  {:<46} {:>9.1} ms（{} 次平均，min {:.1}）",
        name,
        total / runs as f64,
        runs,
        min
    );
}

#[test]
#[ignore = "perf 探针：cargo test -p focusflow-core --test perf_probe -- --ignored --nocapture"]
fn perf_probe_full_pass() {
    let _lock = paths::test_app_dir_lock();
    let _dir = paths::test_app_dir("perf_probe");
    std::fs::write(
        _dir.path().join("config.ini"),
        "[device_stats]\nenabled = false\n\
         [app_stats]\nenabled = false\n\
         [database]\nonline_backup_interval_hours = 0\n\
         backup_on_exit = false\nflush_interval = 31536000\nmax_backups = 5\n",
    )
    .unwrap();

    println!("== 种数据（不计入指标） ==");
    let t = Instant::now();
    for y in YEARS {
        seed_year(y).expect("种库失败");
    }
    queries::invalidate_years_cache();
    println!(
        "  两份年度库（各 365 天）种完，用时 {:.1}s",
        t.elapsed().as_secs_f64()
    );
    for y in YEARS {
        let meta = std::fs::metadata(paths::year_db_path(y)).unwrap();
        println!(
            "  focusflow_{y}.db = {:.1} MB",
            meta.len() as f64 / 1048576.0
        );
    }

    println!("\n== 启动 DB 侧（Database::init 全序列：建表/迁移/自愈/归档/扫描/起写线程） ==");
    let cfg = FocusFlowConfig::load(_dir.path().join("config.ini")).expect("配置加载");
    let paused = focusflow_core::listener::new_pause_flag();
    let (db, _init_ms) = bench("Database::init（真实启动路径）", || {
        Database::init(&cfg, paused).expect("init")
    });

    println!("\n== 打字热路径 ==");
    let keys = key_names();
    let now = chrono::Utc::now().timestamp();
    let n = 50_000;
    let t = Instant::now();
    for i in 0..n {
        db.record_key(&keys[i % KEYS_PER_DAY], now);
    }
    let per_event_us = t.elapsed().as_secs_f64() * 1e6 / n as f64;
    println!(
        "  record_key × {n}：每事件 {per_event_us:.2} µs（{:.0} 万事件/秒）",
        1e6 / per_event_us / 1e4
    );
    let _ = bench(
        "flush(true)（把这批增量落库，超时上限 3s）",
        || {
            db.flush(true);
        },
    );

    println!("\n== 查询面（先各跑一次热身连接池，不计入） ==");
    let dev_key = "VID_046D&PID_C52B&MI_00";
    let _ = db::get_stats(Some(30), None);
    let _ = db::get_app_stats(Some(30), None);
    let _ = db::get_device_stats(Some(30), None);
    let _ = db::get_daily_counts(370, None);
    let _ = db::get_hourly_stats(None);
    let _ = db::get_device_detail(dev_key, 0);
    let _ = db::get_device_detail(dev_key, 30);

    bench_avg("设备详情 period=0（全部）", 3, || {
        let _ = db::get_device_detail(dev_key, 0);
    });
    bench_avg("设备详情 period=30", 3, || {
        let _ = db::get_device_detail(dev_key, 30);
    });
    bench_avg("设备详情 period=-1（今日）", 3, || {
        let _ = db::get_device_detail(dev_key, -1);
    });
    bench_avg("设备排行 get_device_stats(None)", 3, || {
        let _ = db::get_device_stats(None, None);
    });
    bench_avg("导出全史 get_stats(None)", 3, || {
        let _ = db::get_stats(None, None);
    });
    bench_avg("图表 get_stats(30)", 3, || {
        let _ = db::get_stats(Some(30), None);
    });
    bench_avg("全史应用时长 get_app_stats(None)", 3, || {
        let _ = db::get_app_stats(None, None);
    });
    bench_avg("目标窗口 get_daily_counts(370)", 3, || {
        let _ = db::get_daily_counts(370, None);
    });
    bench_avg("get_alltime_summary（总计+最高单日）", 3, || {
        let _ = db::get_alltime_summary();
    });
    bench_avg("get_hourly_stats(None)", 3, || {
        let _ = db::get_hourly_stats(None);
    });

    println!("\n== 退出成本（backup_on_exit 那条路） ==");
    bench(
        "backup_database(5) 第一次（复制+校验+轮转）",
        || maintenance::backup_database(5),
    );
    bench("backup_database(5) 第二次（稳态）", || {
        maintenance::backup_database(5)
    });
    // shutdown 读的是传入 config 的 backup_on_exit：换一份把开关打开的配置
    let mut exit_ini = std::fs::read_to_string(_dir.path().join("config.ini")).unwrap();
    exit_ini = exit_ini.replace("backup_on_exit = false", "backup_on_exit = true");
    std::fs::write(_dir.path().join("config_exit.ini"), exit_ini).unwrap();
    let exit_cfg = FocusFlowConfig::load(_dir.path().join("config_exit.ini")).expect("配置加载");
    bench(
        "Database::shutdown（flush+stop+退出备份，= 真实退出）",
        || {
            db.shutdown(&exit_cfg);
        },
    );

    println!("\n== 启动杂项 ==");
    bench(
        "B14-2 迁移扫描（迁完后的每次启动成本）",
        || {
            maintenance::migrate_all_year_device_keys();
        },
    );
    bench("FocusFlowConfig::load（含整份 save 重写）", || {
        FocusFlowConfig::load(_dir.path().join("config.ini")).expect("配置加载");
    });
    println!("\n== 人感参照：>100ms 可感知；>16ms 掉一帧；热路径 >300µs 才值得看 ==");
    drop(db);
    queries::invalidate_years_cache();
}
