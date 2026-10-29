//! 性能探针（默认 ignore，不进常规门禁）：
//!
//! ```text
//! cargo test -p focusflow-core --test perf_probe -- --ignored --nocapture
//! ```
//!
//! 在隔离 app_dir 里种出**他机器量级**的两份年度库（各 365 天：key_counts 150 键/天、
//! device_key_counts 5 设备 × 40 键/天、hourly 24/天），然后逐项量：
//! - 启动 DB 侧成本（`Database::init` 在这些数据上的全序列）
//! - 打字热路径：**两条数** —— 只 `record_key` 那一臂，与"主链路 + CPM + 设备两条"的合起来
//!   （台账里那句 2.3µs 历来指前者，它低估）+ 一次 flush 的落库成本
//! - 查询面：设备详情（点一次的真实成本）/ 排行 / 导出全史 / 目标窗口 / 小时分布
//! - **一拍 `load()` 的查询面**：耗时 + 语句条数（条数由 `connection::stmt_meter` 挂在
//!   `open_ro` 上现量，不再手算），并逐臂列出「这一臂自己几条」—— 第 1 步那几刀的
//!   优先级只由这张表定。复刻清单与生产 `data::load()` 的一致性由
//!   `tick_replica_matches_the_production_load_body` 钉
//! - 退出成本（`shutdown` = flush + stop + backup_on_exit 的备份与校验）
//! - 备份单项（backup_database 热身后的稳态）
//! - B14-2 迁移扫描（迁完之后每次启动的剩余成本）与配置整读整写
//! - 一拍里**不查库**的那两项（默认表整份重建、开机自启探测）与 `migrate_v2` 在同一进程里
//!   重复跑的那一趟 —— 摘不摘、加不加闸，都按这两行的数定，不凭"看着像浪费"
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

/// 亚毫秒那批用的读数档：`{:.1} ms` 会把 60 µs 印成 `0.0 ms`，那是"量不出"不是"没有"。
/// `black_box` 挡的是编译器把整次调用直接省掉。
fn bench_avg_us<T>(name: &str, runs: usize, mut f: impl FnMut() -> T) {
    let mut total = 0.0;
    let mut min = f64::MAX;
    for _ in 0..runs {
        let t = Instant::now();
        std::hint::black_box(f());
        let us = t.elapsed().as_secs_f64() * 1e6;
        total += us;
        min = min.min(us);
    }
    println!(
        "  {:<46} {:>9.1} µs（{} 次平均，min {:.1}）",
        name,
        total / runs as f64,
        runs,
        min
    );
}

/// 生产 `desktop-native/src/data.rs::window_days` 的镜像（周期 → 查询窗口天数）。
///
/// 为什么允许这里出现第二份：那把尺子住在另一个 crate 且是私有的，而本探针在 core 侧。
/// 抄来的东西必须钉住 —— 见 `days_for_period_is_still_the_desktop_native_ruler`，
/// 它比的是两份**规范化之后的函数体**，不是"看着一样"。
fn days_for_period(period: i64) -> Option<i64> {
    match period {
        0 => None,
        -1 => Some(1),
        n if n > 0 => Some(n),
        _ => Some(7),
    }
}

/// 一拍里的一臂：`(打印用的名字, 可以反复跑的调用)`。
type TickArm = (&'static str, Box<dyn FnMut()>);

/// 快照线程一拍里 `load()` 真正调到的**每一臂**，顺序与生产一字对齐。
///
/// 三个读者：①整拍耗时与条数（[`tick_replica`] 挨个跑一遍）；②逐臂语句数（复位后单跑）；
/// ③本文件末尾那条源文本判据的比对对象。之所以要成表而不是内联在 `bench_avg` 的闭包里：
/// 上一版就是内联手抄的，抄的那份漂开四臂（还在调 `get_weekday_stats`、缺
/// `get_app_stats(Some(1))`、用 `get_daily_counts(30)` 顶替 `daily_counts_totals(chart_days)`），
/// 于是「今日 7.1ms／总计 50.8ms」和照它定的刷新档位全站在过期副本上。
///
/// `collector_on = false` 那一条臂对应生产里「采集没开时 `today_total()` 回 `None`，
/// 才去查 `get_today_count`」（`data.rs:506-509`）；开着时那一臂整个不跑。
fn tick_arms(period: i64, collector_on: bool) -> Vec<TickArm> {
    let days = days_for_period(period);
    let chart_days = queries::chart_window_days(period);
    let goal_days = focusflow_core::stats::GOAL_LOOKBACK_DAYS;
    let mut arms: Vec<TickArm> = Vec::new();
    arms.push((
        "stats_totals",
        Box::new(move || {
            let _ = queries::stats_totals(days, None);
        }),
    ));
    arms.push((
        "get_alltime_max_day",
        Box::new(|| {
            let _ = queries::get_alltime_max_day();
        }),
    ));
    arms.push((
        "daily_counts_windows(图+打卡条)",
        Box::new(move || {
            let _ = queries::daily_counts_windows(chart_days, goal_days);
        }),
    ));
    arms.push((
        "hourly_window_totals",
        Box::new(move || {
            let _ = queries::hourly_window_totals(chart_days, None);
        }),
    ));
    arms.push((
        "device_totals",
        Box::new(move || {
            let _ = queries::device_totals(days, None);
        }),
    ));
    arms.push((
        "app_stats_totals",
        Box::new(move || {
            let _ = queries::app_stats_totals(days, None);
        }),
    ));
    if !collector_on {
        arms.push((
            "get_today_count",
            Box::new(|| {
                let _ = queries::get_today_count(None);
            }),
        ));
    }
    // 生产在 `days == Some(1)` 时不再单独扫 `app_usage`（复用排行那一趟的 total，
    // `data.rs` 的「今日前台时长」那一段）⇒ 复刻跟着分叉。不复刻这一支的话，
    // 今日档那一眼数的是"生产根本不会跑的一拍"，条数会虚高一臂。
    if days != Some(1) {
        arms.push((
            "get_app_stats(Some(1))",
            Box::new(|| {
                let _ = queries::get_app_stats(Some(1), None);
            }),
        ));
    }
    arms
}

/// 整拍跑一遍（顺序 = 生产 `load()` 的顺序）。
fn tick_replica(period: i64, collector_on: bool) {
    for (_name, mut arm) in tick_arms(period, collector_on) {
        arm();
    }
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
    // ⚠ 上面那个数只是**一条臂**。它从前被引用成"打字热路径的全部成本"，而一次真实按下
    // 在 core 侧还要跑：CPM 那把滑窗（`stats.rs:64`，一次 `Mutex` + 一次 `Duration` 换算）
    // 与设备侧信道的两条累加（`writer.rs:529`／`:560`，各一次 `agg` 锁 + 每事件 1~2 次
    // `to_string()`；`device_stats.rs:583` 缓存命中那一路还要再 clone 一个 `(String, String)`）。
    // 台账里那句「record_key 2.3µs/事件」因此是**低估**，引用它之前要说清它没覆盖什么。
    let writer = db.writer().expect("探针的写线程没起来").clone();
    let cpm = focusflow_core::stats::CpmCalculator::new(60.0);
    let devs = [
        ("VID_046D&PID_C52B&MI_00", "键盘 1", "keyboard"),
        ("VID_1532&PID_0094&MI_00", "鼠标 1", "mouse"),
    ];
    let t = Instant::now();
    for i in 0..n {
        let k = &keys[i % KEYS_PER_DAY];
        db.record_key(k, now);
        cpm.record();
        let (dk, name, kind) = devs[i % devs.len()];
        writer.record_device(dk, name, kind, now);
        writer.record_device_key(dk, k, now);
    }
    let full_us = t.elapsed().as_secs_f64() * 1e6 / n as f64;
    println!(
        "  一次按下（主链路 + CPM + 设备两条）× {n}：每事件 {full_us:.2} µs ⇒ record_key 那一臂占 {:.0}%",
        per_event_us * 100.0 / full_us
    );
    // 这把尺子仍然**没覆盖**的部分，写在这里免得被当成全链路上界：
    // rdev 回调本身、`is_new_press` 的长按去重（`listener.rs:487`，另一次 `pressed` 锁）、
    // `key_callbacks` 的每事件 `Vec::clone()`（`listener.rs:555`）—— 那三处住在
    // `InputListener` 的私有方法里，集成测试进不去，要量得在 core 内部开一扇 `pub(crate)` 的口子。
    println!("  （未覆盖：rdev 回调本体、is_new_press 去重、回调表 clone —— 见本文件注释）");
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
    bench_avg("get_alltime_max_day（最高单日）", 3, || {
        let _ = db::get_alltime_max_day();
    });
    bench_avg("get_hourly_stats(None)", 3, || {
        let _ = db::get_hourly_stats(None);
    });

    // 一拍里那些**不查库**的活：要不要把它们从 `load()` 摘出去，看的是这两行的数 ——
    // 用 µs 档是因为 `{:.1} ms` 会把它们全印成 `0.0 ms`，那既不是"零成本"也定不了级。
    // 两条都是纯读（默认表只装不写盘，自启探测只看快捷方式与 HKCU Run），
    // 不改这台机器的任何状态。
    println!("\n== 一拍里不查库的那两项（每拍都跑） ==");
    bench_avg_us(
        "config::default_config()（整份默认表重建）",
        20,
        focusflow_core::config::default_config,
    );
    bench_avg_us(
        "autostart::is_autostart_enabled()（快捷方式 + 注册表）",
        20,
        focusflow_core::autostart::is_autostart_enabled,
    );

    // "星期分布"这一档现在是**再扫一遍每日计数**（`get_weekday_stats` = `get_daily_counts`
    // + `aggregate_weekday`），而同一拍上面已经把同一窗口的每日序列查过了。
    // 这两行的差就是"复用已查得的那份"能省下的 —— 量出来再决定省不省，不猜。
    println!("\n== 星期分布那一趟（复用能省多少） ==");
    bench_avg("get_daily_counts(30)（每日序列本体）", 5, || {
        let _ = queries::get_daily_counts(30, None);
    });
    bench_avg(
        "get_weekday_stats(30)（再扫一遍 + 聚合）",
        5,
        || {
            let _ = queries::get_weekday_stats(30);
        },
    );
    {
        let daily = queries::get_daily_counts(30, None);
        bench_avg_us(
            "aggregate_weekday(已查得的 30 天)（纯聚合）",
            20,
            || queries::aggregate_weekday(&daily),
        );
    }

    // 快照线程那一拍**真正跑的东西**（`desktop-native/src/data.rs::load` 的查询面）。
    // 这一条决定两件事："可见时能不能从 2 秒收到 1 秒甚至更快"，以及
    // "看不见时停不停得有意义" —— 两个都要看数，不猜。
    // ⚠ 条数不再是手算：`connection::stmt_meter` 挂在 `open_ro` 上现量。它只数**只读
    // 连接上跑完的语句**，写线程每批那几组 UPSERT 不在这一格里（边界见那模块的注释）。
    println!("\n== 一拍 load() 的查询面（复刻体 = tick_arms，条数由 trace 现量） ==");
    for (label, period) in [("今日", -1i64), ("总计", 0i64)] {
        // 先热一轮：建连接池、立年份名单 —— 真实运行时的稳态是"热的"，
        // 冷启动那一轮会把 PRAGMA 数进第一批里，那不是他每拍付的账。
        tick_replica(period, true);
        connection::stmt_meter::reset();
        let runs = 5usize;
        let t = Instant::now();
        for _ in 0..runs {
            tick_replica(period, true);
        }
        let ms = t.elapsed().as_secs_f64() * 1000.0 / runs as f64;
        // ⚠ 计数器是 `runs` 轮的**累计**，而这一行念的是"每拍"。第一版就是在这里没除，
        // 把 5 轮的总数打成「条／拍」（今日档印成 125 条，真值 25）—— 暴露它的正是
        // 下面那句"逐臂相加"恰好等于它的 1/5。读数带单位就得当场核分母。
        let (presence, pragma, data) = connection::stmt_meter::read();
        let (p1, p2, p3) = (
            presence / runs as u64,
            pragma / runs as u64,
            data / runs as u64,
        );
        println!(
            "  {label}档：每拍 {ms:>6.1} ms｜{} 条／拍 = 存在性 {p1} + PRAGMA {p2} + 数据 {p3}（{runs} 轮累计 {} 条）",
            p1 + p2 + p3,
            presence + pragma + data
        );
        // 交叉核对：逐臂单跑相加，应当等于整拍跑一轮的总数。对不上就是有一臂的成本
        // 跟着前一臂留下的连接池/年份缓存状态走 —— 那本身也得看见，不许 silently 差着。
        let mut arm_total = 0u64;
        for (_name, mut arm) in tick_arms(period, true) {
            connection::stmt_meter::reset();
            arm();
            let (a, b, c) = connection::stmt_meter::read();
            arm_total += a + b + c;
        }
        println!(
            "    交叉核对：逐臂相加 {arm_total} 条 vs 整拍 {} 条 ⇒ {}",
            p1 + p2 + p3,
            if arm_total == p1 + p2 + p3 {
                "对上"
            } else {
                "没对上（有一臂跟着前一臂的池/缓存状态走，或有一臂只在整拍里跑）"
            }
        );
    }

    // 逐臂占比：第 1 步那几刀谁先动、能省几条，只看这张表。
    // 每臂复位后单跑，所以数的是「这一臂自己几条」；两档各自相加应对上上面那一拍的总数
    // —— 对不上就是有一臂的成本依赖前一臂留下的连接池或年份缓存状态，那本身也是要看见的事。
    println!("\n== 每一臂自己几条语句 ==");
    for (label, period) in [("今日", -1i64), ("总计", 0i64)] {
        println!("  -- {label}档 --");
        for (name, mut arm) in tick_arms(period, true) {
            connection::stmt_meter::reset();
            arm();
            let (p, pr, d) = connection::stmt_meter::read();
            println!(
                "    {:<28} {} 条（存在性 {p}／PRAGMA {pr}／数据 {d}）",
                name,
                p + pr + d
            );
        }
        // 生产那一拍里还有两条**不查库**的臂（`chart_window_days` 是纯算术、
        // `aggregate_weekday` 复用的是上面已经查得的每日序列）。把它们也当场数一遍：
        // 这两条要是哪天开始碰库，下面这两行是第一个看见的地方 ——
        // 源文本判据里的允许清单只挡"清单漂开"，挡不住"纯的变不纯"。
        let daily = queries::daily_counts_totals(queries::chart_window_days(period), None).series;
        connection::stmt_meter::reset();
        std::hint::black_box(queries::chart_window_days(period));
        std::hint::black_box(queries::aggregate_weekday(&daily));
        let (p, pr, d) = connection::stmt_meter::read();
        println!(
            "    {:<28} {} 条（存在性 {p}／PRAGMA {pr}／数据 {d}）",
            "chart_window_days+aggregate_weekday",
            p + pr + d
        );
    }

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
    // 同一个进程里 `migrate_v2` 现在跑两趟（`Database::init` 与 `init_readonly` 各一次）。
    // 要算"加进程级闸能省多少"，量的是**第二趟**：第一趟是迁移本体，省不掉。
    bench(
        "migrate_v2 第二次（= 同进程里重复的那一趟）",
        maintenance::migrate_v2,
    );
    bench(
        "B14-2 迁移扫描（迁完后的每次启动成本）",
        || {
            maintenance::migrate_all_year_device_keys();
        },
    );
    bench("FocusFlowConfig::load（含整份 save 重写）", || {
        FocusFlowConfig::load(_dir.path().join("config.ini")).expect("配置加载");
    });

    // 启动那 500 多毫秒到底花在哪：把 `Database::init` 里逐步骤**再走一遍**（都幂等），
    // 减出来的和与上面那条整序列的差，就是"这一段还没被解释"的余量。
    // ⚠ 一条口径：本探针的库是**直接种进聚合表**的，`migrate_v2` 第一趟在这儿没活干
    // ⇒ 这一组数只覆盖"已经是新格式、每次启动重扫"的那种启动，不含旧库首启那一次。
    println!("\n== 启动分解（init 里逐步骤再走一遍，幂等） ==");
    for y in YEARS {
        bench(
            &format!("ensure_schema({y})（建表/索引那趟）"),
            || {
                let conn = connection::open_rw(&paths::year_db_path(y)).expect("开库");
                connection::ensure_schema(&conn, y).expect("建表")
            },
        );
    }
    bench(
        "heal_daily_consistency（每日计数与键数对账自愈）",
        maintenance::heal_daily_consistency,
    );
    bench("check_yearly_archive(true)（归档检查）", || {
        maintenance::check_yearly_archive(true)
    });
    bench("sweep_stale_sidecars（backup/ 残留扫描）", || {
        std::hint::black_box(maintenance::sweep_stale_sidecars())
    });
    bench("device_alias::migrate_exact_keys_to_identity", || {
        focusflow_core::device_alias::migrate_exact_keys_to_identity()
    });

    // ⚠ 上面那条 `Database::init` 是**这一份数据的第一次**：探针的库是直接种进聚合表的，
    // 所以那一趟里 B14-2 归组键转换与一致性自愈都要**真改数据**（各自第二趟只扫不改，
    // 分别在"启动杂项"和"启动分解"里）。他每次开机付的是**第二次**那一趟 —— 下面这条
    // 才是启动体感的尺子，也是"某一步要不要挪出关键路径"唯一的裁决依据。
    println!("\n== 稳态启动（同一份数据再走一遍 init = 他每次开机付的那一趟） ==");
    let paused2 = focusflow_core::listener::new_pause_flag();
    let (db2, _) = bench("Database::init 第二次（稳态启动）", || {
        Database::init(&cfg, paused2).ok()
    });
    if let Some(d) = db2.as_ref() {
        d.shutdown(&cfg);
    }
    println!("\n== 人感参照：>100ms 可感知；>16ms 掉一帧；热路径 >300µs 才值得看 ==");
    drop(db);
    queries::invalidate_years_cache();
}

/// 去掉行注释，只留代码。两道源文本判据共用这一句：注释里的字不是"这一拍调了它"、
/// 也不是"这把尺子的取值规则"，而两处各写一遍"怎么认注释"迟早会分叉成一个剥、一个不剥。
///
/// ⚠ 已知够不着的一格：它按**行**在第一个 `//` 处切断，所以字符串字面量里出现的 `//`
/// （URL 那类）会被连着后半行一起削掉。这两道判据扫的是 `load()` 与那把尺子的函数体，
/// 里面没有这种串；真往里塞一条带 `//` 的字符串时，判据会红在"清单对不上"上，不会假绿。
fn strip_line_comments(src: &str) -> String {
    src.lines()
        .map(|line| match line.find("//") {
            Some(i) => &line[..i],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 取一段 Rust 源码里出现过的 `queries::` 名字（按出现顺序，先剥行注释）。
///
/// 为什么必须先剥注释：生产 `load()` 里有一句注释写着 `queries::PANEL_READ_DOMAINS`
/// （`data.rs:514`），那不是"这一拍调了它"。把注释里的字当成调用点，这道闸就会在
/// 谁改注释措辞时莫名红一次，或在相反方向上把一条真臂当噪音放过。
/// 切函数体的形状沿用本仓既有用法：从签名起、到第一个**顶格**的 `}` 止
/// （嵌套块都带缩进，所以 `\n}` 切到的正是函数那一格）。
fn queries_called_in(src: &str, head: &str) -> Vec<String> {
    let at = src
        .find(head)
        .unwrap_or_else(|| panic!("{head} 不见了 —— 签名改了，判据的锚要跟着改"));
    let after = &src[at..];
    let end = after
        .find("\n}")
        .unwrap_or_else(|| panic!("{head} 的函数体切不出来"));
    let code = strip_line_comments(&after[..end]);
    let mut out = Vec::new();
    let mut rest = code.as_str();
    while let Some(i) = rest.find("queries::") {
        rest = &rest[i + "queries::".len()..];
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() {
            out.push(name);
        }
    }
    out
}

/// 复刻清单必须与生产 `data::load()` 的查询臂清单**逐字同序**（第 0 步那把防漂的闸）。
///
/// 立它的理由不是"格式洁癖"：`desktop-native/src/main.rs` 里那两行档位
/// （短档 1 秒、长档 ×3）是照「今日 7.1ms／总计 50.8ms」定的，而那两条数来自本探针。
/// 上一版探针的复刻清单漂开了四臂，量具不会自己喊 —— 它只会让后面每一刀都按错账排优先级。
///
/// ⚠ 已知边界（写清楚，别让它看起来比实际更严）：这条比的是**函数名清单**，
/// 不比参数。把 `Some(1)` 改成 `None` 这一类"同一臂换窗口"的漂它抓不到，
/// 那是 `== 每一臂自己几条语句 ==` 那张表的活（窗口一变条数就变）。
#[test]
fn tick_replica_matches_the_production_load_body() {
    /// 允许缺席的两条：`chart_window_days` 是纯算术，`aggregate_weekday` 吃的是上面
    /// 已经查得的每日序列，都不该在复刻里再查一遍库。它们在探针里被单独数过
    /// （见「每一臂自己几条语句」末尾那一行），所以"清单允许它们缺席"不等于"纯的永远纯"。
    const PURE_ARMS: [&str; 2] = ["chart_window_days", "aggregate_weekday"];

    let prod = include_str!("../../../desktop-native/src/data.rs");
    let mut prod_arms = queries_called_in(prod, "pub fn load(period: i64) -> Snapshot");
    for pure in PURE_ARMS {
        let at = prod_arms.iter().position(|a| a == pure).unwrap_or_else(|| {
            panic!(
                "允许清单里的 {pure} 在生产 load() 里不见了：要么它改了名，要么这一拍不再调它。\
                 两种都得重看这份清单，别顺手把它从允许清单里删掉就算完"
            )
        });
        prod_arms.remove(at);
    }
    let mut replica = queries_called_in(
        include_str!("perf_probe.rs"),
        "fn tick_arms(period: i64, collector_on: bool)",
    );
    // 对称剥离：复刻体自己也要算一次 `chart_days`（那是它取每日序列的窗口），
    // 所以这两条同样从复刻侧摘掉 —— 只从生产侧摘会让这道闸一上来就红在
    // "两份都做对了同一件事" 上（第一次跑实测就是这个红点）。
    for pure in PURE_ARMS {
        if let Some(at) = replica.iter().position(|a| a == pure) {
            replica.remove(at);
        }
    }
    assert_eq!(
        prod_arms, replica,
        "探针复刻清单与生产 data::load() 的查询臂清单不一致（顺序也算）。生产侧读到的: \
         {prod_arms:?}；复刻侧读到的: {replica:?}"
    );
}

/// `days_for_period` 不许漂开生产那把尺子。
///
/// 这是"抄一份就得钉一份"的那条规矩：尺子本体 `window_days` 住在 `desktop-native` 且私有，
/// 探针在 core 侧只能镜像它。镜像漂了的形态和当年复刻清单漂开一模一样 —— 数还在动，
/// 数的已经不是那一拍了。这里比的是 `match period` 之后规范化（去全部空白）的那一截。
#[test]
fn days_for_period_is_still_the_desktop_native_ruler() {
    let body_after_match = |src: &str, head: &str| -> String {
        let at = src
            .find(head)
            .unwrap_or_else(|| panic!("{head} 不见了 —— 签名改了，判据的锚要跟着改"));
        let after = &src[at..];
        let end = after
            .find("\n}")
            .unwrap_or_else(|| panic!("{head} 的函数体切不出来"));
        // 注释与空白都不进比对：那把尺子要钉的是**取值规则**（哪档给几天、哪档给 `None`）。
        // 不剥注释的话，谁在生产 `window_days` 头上补一句说明就会红在这里 ——
        // 红在错的东西上，比不红更糟（下一轮就有人把它当成噪音绕过去了）。
        let code = strip_line_comments(&after[..end]);
        let m = code
            .find("match period")
            .unwrap_or_else(|| panic!("{head} 里没有 match period，它不再是那把尺子的形状"));
        code[m..].chars().filter(|c| !c.is_whitespace()).collect()
    };

    let prod = include_str!("../../../desktop-native/src/data.rs");
    let mine = body_after_match(prod, "fn window_days(period: i64)");
    let replica = body_after_match(
        include_str!("perf_probe.rs"),
        "fn days_for_period(period: i64)",
    );
    assert_eq!(
        mine, replica,
        "生产 window_days 与探针镜像已分叉（生产: {mine}；探针: {replica}）。\
         改的是哪一边都要说得出理由：只改一边就是\"同一件事两份答案\"那个老形状"
    );
}
