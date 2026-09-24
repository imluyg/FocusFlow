//! 记账模块行为测试：分页/筛选/盈亏统计/日期计算（隔离临时目录）。

use focusflow_core::accounting;
use focusflow_core::paths;

/// 串行锁（app_dir 是进程级全局，测试必须串行执行）。
fn guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// 一个隔离的记账库；返回值 Drop 时删目录（断言 panic 也会删）。
///
/// 原来这里手拼 pid+纳秒的目录名、又在函数末尾才 `remove_dir_all`：目录名每轮都新，
/// 收尾只在全绿时执行 —— 于是一次失败就在 %TEMP% 永久留一份 SQLite 库（本机曾堆到
/// 333 个 `ff_acc_test_*`）。清理改由 RAII 负责。
fn setup() -> paths::TestAppDir {
    let dir = paths::test_app_dir("acc_test");
    accounting::init_db().unwrap();
    dir
}

/// 分类名：空与纯空格必须被拒，合法名字必须去掉首尾空格。
///
/// 回归：`categories` 只有 NOT NULL + UNIQUE，以前 `add_category("")` 会真建出一行，
/// 而它的 id 就是 `""` —— 记账面板把 `""` 当"未选中"的哨兵（删除/修改按钮开头都是
/// `if name == "" then return`），于是**这个分类删不掉、改不了、挂不上子分类**。
/// 带空格的版本更隐蔽：记录侧存分类时走 `blank_to_none`（会 trim），
/// 于是" 餐饮"这个分类永远筛不到任何记录，列表看着像空的。
#[test]
fn category_names_are_trimmed_and_blank_ones_rejected() {
    let _g = guard();
    let _d = setup();
    let names = || -> Vec<String> {
        accounting::get_all_categories()
            .iter()
            .map(|c| c.name.clone())
            .collect()
    };

    assert_eq!(
        accounting::add_category("", "both", &[]),
        -1,
        "空名不该建出来"
    );
    assert_eq!(
        accounting::add_category("   ", "both", &[]),
        -1,
        "只有空格也不算名字"
    );
    assert!(
        !names().iter().any(|n| n.trim().is_empty()),
        "被拒之后不该留下任何空分类: {:?}",
        names()
    );

    assert!(accounting::add_category(" 餐饮 ", "both", &[]) > 0);
    assert!(
        names().iter().any(|n| n == "餐饮"),
        "存进去的该是去掉首尾空格的名字: {:?}",
        names()
    );

    // 改名同理：改成纯空格必须被拒，并给出原因
    let (ok, msg) = accounting::update_category("餐饮", "   ", None);
    assert!(!ok, "改成纯空格竟然成功了");
    assert!(msg.contains("不能为空"), "原因要说清，实际: {msg}");
    // 改成带空格的合法名字 → 落地成 trimmed 值（否则匹配不上记录）
    let renamed = accounting::update_category("餐饮", " 饮料 ", None).0;
    assert!(renamed, "改成带空格的合法名字该成功");
    assert!(names().iter().any(|n| n == "饮料"), "{:?}", names());
    assert!(
        !names().iter().any(|n| n != n.trim()),
        "不该留下带空格的分类名: {:?}",
        names()
    );
}

fn add(date: &str, rtype: &str, item: &str, amount: f64, cat: &str, sub: &str) -> i64 {
    accounting::add_expense(
        rtype,
        item,
        None,
        date,
        amount,
        Some(cat),
        if sub.is_empty() { None } else { Some(sub) },
        None,
    )
}

#[test]
fn page_filter_and_escaping() {
    let _g = guard(); // 全程持有：app_dir 是进程级全局
    let _dir = setup();
    // 6 条记录：4 支出 + 2 收入，跨日期
    add("2026-08-01", "支出", "早餐", 8.0, "餐饮", "早餐类");
    add("2026-08-02", "支出", "午餐", 20.0, "餐饮", "午餐类");
    add("2026-08-03", "支出", "地铁", 4.0, "交通", "");
    add("2026-08-04", "收入", "工资", 5000.0, "收入", "");
    add("2026-08-05", "支出", "网费100%", 100.0, "通讯", "");
    add("2026-08-06", "收入", "奖金", 800.0, "收入", "");

    // 分页：每页 2 条 → 3 页，第 1 页最新在前
    let (page1, total) = accounting::get_expenses_page(1, 2, None, None, None, None, None);
    assert_eq!(total, 6);
    assert_eq!(page1.len(), 2);
    assert_eq!(page1[0].purchase_date, "2026-08-06");
    assert_eq!(page1[1].purchase_date, "2026-08-05");

    // 第 3 页
    let (page3, _) = accounting::get_expenses_page(3, 2, None, None, None, None, None);
    assert_eq!(page3.len(), 2);
    assert_eq!(page3[1].purchase_date, "2026-08-01");

    // 超尾页 → 空列表但 total 正确
    let (page9, total9) = accounting::get_expenses_page(9, 2, None, None, None, None, None);
    assert!(page9.is_empty());
    assert_eq!(total9, 6);

    // 分类筛选
    let (only_cat, total_cat) =
        accounting::get_expenses_page(1, 10, Some("餐饮"), None, None, None, None);
    assert_eq!(total_cat, 2);
    assert!(only_cat
        .iter()
        .all(|e| e.category.as_deref() == Some("餐饮")));

    // 子分类筛选
    let (only_sub, total_sub) =
        accounting::get_expenses_page(1, 10, None, Some("午餐类"), None, None, None);
    assert_eq!(total_sub, 1);
    assert_eq!(only_sub[0].item_name, "午餐");

    // 日期范围
    let (_in_range, total_range) = accounting::get_expenses_page(
        1,
        10,
        None,
        None,
        None,
        Some("2026-08-03"),
        Some("2026-08-04"),
    );
    assert_eq!(total_range, 2);

    // 关键词：% 和 _ 按字面匹配（不当作通配符）
    let (kw_pct, total_pct) =
        accounting::get_expenses_page(1, 10, None, None, Some("100%"), None, None);
    assert_eq!(total_pct, 1, "百分号应字面匹配");
    assert_eq!(kw_pct[0].item_name, "网费100%");
    let (_kw_under, total_under) =
        accounting::get_expenses_page(1, 10, None, None, Some("_"), None, None);
    assert_eq!(total_under, 0, "下划线不应作为通配符");

    // 关键词匹配分类
    let (_kw_cat, total_kw) =
        accounting::get_expenses_page(1, 10, None, None, Some("餐饮"), None, None);
    assert_eq!(total_kw, 2);
}

#[test]
fn legacy_db_migration() {
    // 旧版（Python 版）结构：categories 无 subs 列 + 独立 subcategories 表
    let _g = guard();
    let dir = paths::test_app_dir("acc_mig");
    let db = dir.path().join("data").join("focusflow_accounting.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE categories (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                type TEXT NOT NULL DEFAULT 'both',
                sort_order INTEGER DEFAULT 0,
                created_at TEXT NOT NULL
            );
            CREATE TABLE subcategories (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                category_id INTEGER NOT NULL,
                sort_order INTEGER DEFAULT 0,
                created_at TEXT NOT NULL
            );
            INSERT INTO categories (name, type, created_at) VALUES ('游戏','both','2026-01-01');
            INSERT INTO categories (name, type, created_at) VALUES ('工资收入','income','2026-01-01');
            INSERT INTO subcategories (name, category_id, created_at) VALUES ('梦幻西游',1,'2026-01-01');
            INSERT INTO subcategories (name, category_id, created_at) VALUES ('充值',1,'2026-01-01');
            INSERT INTO subcategories (name, category_id, created_at) VALUES ('月薪',2,'2026-01-01');",
        )
        .unwrap();
    }

    // init_db 应迁移：补 subs 列、合并旧子分类、不重复预置
    accounting::init_db().unwrap();
    let cats = accounting::get_all_categories();
    assert_eq!(cats.len(), 2, "迁移后分类数不变");
    let game = cats.iter().find(|c| c.name == "游戏").unwrap();
    assert_eq!(game.subs, vec!["梦幻西游", "充值"]);
    let income = cats.iter().find(|c| c.name == "工资收入").unwrap();
    assert_eq!(income.subs, vec!["月薪"]);

    // 旧 subcategories 表应被删除
    let conn = rusqlite::Connection::open(&db).unwrap();
    let cnt: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='subcategories'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cnt, 0, "旧 subcategories 表应被移除");
}

#[test]
fn category_management_crud() {
    // 分类管理 API：添加/重命名/删除分类、子分类增删改 + 历史记录同步
    let _g = guard();
    let _dir = setup();
    let id1 = add("2026-08-01", "支出", "点卡", 100.0, "游戏", "梦幻西游");
    assert!(id1 > 0);

    // 预置分类已存在：找"游戏"
    let cats = accounting::get_all_categories();
    let game = cats.iter().find(|c| c.name == "游戏").unwrap();
    assert!(game.subs.contains(&"梦幻西游".to_string()));

    // 添加新分类
    let new_id = accounting::add_category("宠物", "expense", &["猫粮".into()]);
    assert!(new_id > 0);
    assert!(
        accounting::get_all_categories()
            .iter()
            .any(|c| c.name == "宠物"),
        "新分类应存在"
    );

    // 重命名分类 → 历史记录同步
    let (ok, _) = accounting::update_category("宠物", "宠物用品", None);
    assert!(ok);
    assert!(
        accounting::get_all_categories()
            .iter()
            .any(|c| c.name == "宠物用品"),
        "重命名后新名应存在"
    );

    // 重名检查
    let (dup_ok, _) = accounting::update_category("宠物用品", "游戏", None);
    assert!(!dup_ok, "重名应失败");

    // 添加子分类（去重）
    let (ok, _) = accounting::add_subcategory("游戏", "新道具");
    assert!(ok);
    let (dup_ok, _) = accounting::add_subcategory("游戏", "新道具");
    assert!(!dup_ok, "重复子分类应失败");

    // 重命名子分类 → 历史记录同步
    let (ok, _) = accounting::update_subcategory("游戏", "梦幻西游", "梦幻西游2");
    assert!(ok);
    let after = accounting::get_all_categories();
    let game2 = after.iter().find(|c| c.name == "游戏").unwrap();
    assert!(game2.subs.contains(&"梦幻西游2".to_string()));
    let rec = accounting::get_expense_by_id(id1).unwrap();
    assert_eq!(
        rec.subcategory.as_deref(),
        Some("梦幻西游2"),
        "历史记录子分类应同步"
    );

    // 删除子分类 / 删除分类
    let (ok, _) = accounting::delete_subcategory("游戏", "新道具");
    assert!(ok);
    let (ok, _) = accounting::delete_category("宠物用品");
    assert!(ok);
    assert!(
        !accounting::get_all_categories()
            .iter()
            .any(|c| c.name == "宠物用品"),
        "删除后分类不应存在"
    );
    // 删除分类不影响历史记录
    let rec2 = accounting::get_expense_by_id(id1).unwrap();
    assert_eq!(rec2.category.as_deref(), Some("游戏"));
}

#[test]
fn profit_and_days_ago() {
    let _g = guard(); // 全程持有：app_dir 是进程级全局
    let _dir = setup();
    add("2026-08-01", "支出", "早餐", 10.0, "餐饮", "早餐类");
    add("2026-08-02", "支出", "午餐", 30.0, "餐饮", "午餐类");
    add("2026-08-03", "收入", "外快", 100.0, "餐饮", "早餐类");
    add("2026-08-04", "支出", "地铁", 5.0, "交通", "");

    // 分类盈亏：餐饮 投入40 赚取100 记录3；交通 投入5 记录1
    let profits = accounting::category_profit_loss();
    let din: Vec<_> = profits.iter().filter(|(c, _, _, _)| c == "餐饮").collect();
    assert_eq!(din.len(), 1);
    let (_, inv, earn, cnt) = din[0];
    assert_eq!(*inv, 40.0);
    assert_eq!(*earn, 100.0);
    assert_eq!(*cnt, 3);

    // 细分盈亏：餐饮下 早餐类 投入10 赚取100
    let subs = accounting::subcategory_profit_loss("餐饮");
    let brk: Vec<_> = subs.iter().filter(|(s, _, _, _)| s == "早餐类").collect();
    assert_eq!(brk.len(), 1);
    assert_eq!(brk[0].1, 10.0);
    assert_eq!(brk[0].2, 100.0);

    // 月度汇总明细（当月 8 月）
    let (expense, income, count, stats) = accounting::monthly_summary_detail("2026-08");
    assert_eq!(expense, 45.0);
    assert_eq!(income, 100.0);
    assert_eq!(count, 4);
    assert_eq!(stats.len(), 2);

    // 距今多久：今天的记录 0 天；一年前的约 365 天
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let id_today = add(&today, "支出", "今天买的", 1.0, "餐饮", "");
    let last_year = chrono::Local::now()
        .date_naive()
        .checked_sub_months(chrono::Months::new(12))
        .unwrap()
        .format("%Y-%m-%d")
        .to_string();
    let id_year = add(&last_year, "支出", "去年买的", 2.0, "餐饮", "");
    let days = accounting::days_ago(&[id_today, id_year]);
    let d_today = days.iter().find(|(id, _, _)| *id == id_today).unwrap();
    assert_eq!(d_today.1 + d_today.2, 0, "今天的记录应为 0 天");
    let d_year = days.iter().find(|(id, _, _)| *id == id_year).unwrap();
    assert!(
        (d_year.1 == 0 && d_year.2 >= 360) || (d_year.1 == 1 && d_year.2 <= 5),
        "一年前应为约 1 年 0-5 天，实际 {} 年 {} 天",
        d_year.1,
        d_year.2
    );
}

/// 回归：金额汇总按「分」精确相加，不再累积浮点误差。
///
/// `amount` 是 REAL，逐条相加走的是二进制浮点：实测十笔 0.1 加起来是
/// 0.9999999999999999，0.1 + 0.2 是 0.30000000000000004 —— 界面上就表现为
/// 「明细逐条加起来比合计多一分/少一分」，而用户会以为是算错了账。
/// 修法是聚合时先化成整数分相加、最后除回 100（30/100 与 0.3 是同一个 double）。
#[test]
fn monthly_totals_sum_in_cents_not_floats() {
    let _g = guard();
    let _dir = setup();

    for _ in 0..10 {
        add("2026-03-05", "支出", "一毛", 0.1, "吃饭", "");
    }
    let (expense, _) = accounting::monthly_summary("2026-03");
    assert_eq!(expense, 1.0, "十笔 0.1 必须正好 1.00");

    add("2026-04-01", "支出", "a", 0.1, "吃饭", "");
    add("2026-04-02", "支出", "b", 0.2, "吃饭", "");
    let (apr_expense, _) = accounting::monthly_summary("2026-04");
    assert_eq!(
        apr_expense, 0.3,
        "0.1 + 0.2 按分相加应得 0.30 而非 0.3000…004"
    );

    // 分类净额：收 0.3 对支 0.1+0.2，必须恰好抵消成 0.00
    add("2026-04-03", "收入", "c", 0.3, "吃饭", "");
    let (_, _, _, cats) = accounting::monthly_summary_detail("2026-04");
    let net = cats
        .iter()
        .find(|(c, _)| c == "吃饭")
        .map(|(_, n)| *n)
        .unwrap_or(f64::NAN);
    assert_eq!(net, 0.0, "净额须在分维度相减，不能剩浮点残差：{cats:?}");
}
