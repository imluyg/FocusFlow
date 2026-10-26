//! 给原型造一天的数据。
//!
//! **只写 `FOCUSFLOW_APP_DIR` 指到的目录**——跑之前把它指到 `desktop-native/probe`，
//! 绝不要指到 `dist/FocusFlow`。用的是 core 自己的写线程，口径与真实采集一致。

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

fn main() {
    let cfg = focusflow_core::config::instance();
    let paused = Arc::new(AtomicBool::new(false));
    let db = focusflow_core::db::Database::init(cfg, paused).expect("数据库初始化失败");

    let plan: [(&str, u32); 10] = [
        ("A", 240),
        ("空格", 180),
        ("E", 150),
        ("退格", 120),
        ("回车", 96),
        ("S", 80),
        ("左Ctrl", 60),
        ("左Shift", 55),
        ("，", 12),
        ("。", 9),
    ];
    let mut total = 0;
    for (key, n) in plan {
        for _ in 0..n {
            db.record_key(key, chrono::Local::now().timestamp());
            total += 1;
        }
    }
    db.flush(true);

    // 再多造 50 个键位：P2 的滚动容器要有东西可滚（一屏大约 20 行）。
    for i in 0..50 {
        let key = format!("K{i}");
        for _ in 0..(60 - i) {
            db.record_key(&key, chrono::Local::now().timestamp());
        }
    }
    db.flush(true);

    // 过去 25 天：让「7天 / 30天」周期和趋势图有东西可比。
    // 事件摊到 9~21 点，否则小时图上所有历史都挤在同一根柱子里。
    let now_hour = chrono::Timelike::hour(&chrono::Local::now()) as i64;
    for d in 1..=25i64 {
        let day_anchor = chrono::Local::now().timestamp() - d * 86_400;
        for (i, key) in ["A", "空格", "E", "退格", "回车"].iter().enumerate() {
            for h in 9..=21i64 {
                let ts = day_anchor - (now_hour - h) * 3600;
                let n = ((300 - i as i64 * 40 + (d % 7) * 10) / 12).max(1);
                for _ in 0..n {
                    db.record_key(key, ts);
                }
            }
        }
    }
    db.flush(true);

    // 设备维度：P1b 的行内改名要有设备行可点。
    if let Some(w) = db.writer() {
        let devices: [(&str, &str, &str, u32); 3] = [
            ("HID\\VID_046D&PID_C52B&MI_00", "罗技 G304 鼠标", "mouse", 320),
            ("HID\\VID_046D&PID_C31C&MI_00", "HID Keyboard Device", "keyboard", 260),
            ("HID\\VID_1532&PID_0266&MI_00", "雷蛇黑寡妇", "keyboard", 140),
        ];
        for (key, name, kind, n) in devices {
            for _ in 0..n {
                w.record_device(key, name, kind, chrono::Local::now().timestamp());
            }
        }
    }
    db.flush(true);

    println!(
        "已写入 {total} 条到 {}",
        focusflow_core::paths::data_dir().display()
    );
    db.shutdown(cfg);

    // 应用排行：writer 没有公开的"记应用"接口，直接写 app_usage 聚合表。
    // 只写 FOCUSFLOW_APP_DIR 指到的探针库，别指到 dist。
    let year = chrono::Datelike::year(&chrono::Local::now());
    let path = focusflow_core::paths::year_db_path(year);
    if let Ok(conn) = focusflow_core::db::connection::open_rw(&path) {
        let _ = focusflow_core::db::connection::ensure_schema(&conn, year);
        let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        let dk = (chrono::Local::now().date_naive() - epoch).num_days();
        let apps: [(&str, i64); 6] = [
            ("Code.exe", 5400),
            ("chrome.exe", 3600),
            ("WeChat.exe", 1800),
            ("QQ.exe", 900),
            ("explorer.exe", 600),
            ("WINWORD.EXE", 300),
        ];
        for (app, secs) in apps {
            let sql = format!(
                "INSERT INTO app_usage(date_key, app_name, seconds) VALUES ({dk}, '{app}', {secs}) \
                 ON CONFLICT(date_key, app_name) DO UPDATE SET seconds = excluded.seconds"
            );
            let _ = conn.execute(&sql, []);
        }
        println!("已写入应用时长到 {}", path.display());
    }
}
