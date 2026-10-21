//! 记账模块。
//!
//! 镜像 Python 版 `accounting.py`：
//! - 收入/支出记录 CRUD
//! - 分类/子分类
//! - 按日期/类型/分类筛选
//! - 持久化到 `data/focusflow_accounting.db`

use rusqlite::Connection;

use crate::paths;

/// 记账记录。
#[derive(Debug, Clone)]
pub struct Expense {
    pub id: i64,
    pub rtype: String,
    pub item_name: String,
    pub store: Option<String>,
    pub purchase_date: String,
    pub amount: f64,
    pub category: Option<String>,
    pub subcategory: Option<String>,
    pub delivery_date: Option<String>,
    pub record_time: String,
    pub note: Option<String>,
}

/// 分类。
#[derive(Debug, Clone)]
pub struct Category {
    pub id: i64,
    pub name: String,
    pub ctype: String,
    pub subs: Vec<String>,
}

pub fn db_path() -> std::path::PathBuf {
    paths::data_dir().join("focusflow_accounting.db")
}

fn open() -> rusqlite::Result<Connection> {
    std::fs::create_dir_all(paths::data_dir()).ok();
    let conn = Connection::open(db_path())?;
    conn.pragma_update(None, "journal_mode", "WAL").ok();
    conn.pragma_update(None, "synchronous", "NORMAL").ok();
    conn.pragma_update(None, "foreign_keys", "ON").ok();
    conn.busy_timeout(std::time::Duration::from_secs(15)).ok();
    Ok(conn)
}

/// 空白值按"没有分类"处理：落库存 NULL。
///
/// 插件侧的下拉框用 `""` 表示"（无）/（请选择）"，直接原样写库的话：
/// - 分类盈亏/月度分类明细里出现一个**名字为空**的行（`(未分类)` 只兜住了 NULL），
///   插件把它拼成 `"  : 12.00"` 这样一行看不出是谁的数；
/// - 与旧版 Python 导入的 NULL 分成两组，同一个"没分类"被拆成两行、各自的数都不对。
///
/// 顺带 trim：`"餐饮"` 与 `" 餐饮"` 在 GROUP BY 里也是两个分类。
fn blank_to_none(raw: Option<&str>) -> Option<String> {
    let trimmed = raw.map(str::trim).filter(|s| !s.is_empty())?;
    Some(trimmed.to_string())
}

/// 预置分类（首次初始化用，对标 Python 版 DEFAULT_CATEGORIES）。
/// 名称 -> (类型, 子分类列表)
const DEFAULT_CATEGORIES: &[(&str, &str, &[&str])] = &[
    (
        "食品饮料",
        "expense",
        &[
            "早餐", "午餐", "晚餐", "零食", "饮料", "水果", "外卖", "其他",
        ],
    ),
    (
        "日用百货",
        "expense",
        &["清洁用品", "纸品", "厨房用品", "卫浴用品", "其他"],
    ),
    (
        "数码电子",
        "expense",
        &["电脑配件", "手机配件", "耳机", "充电器", "存储设备", "其他"],
    ),
    (
        "服饰鞋包",
        "expense",
        &["上衣", "裤子", "鞋子", "包", "配饰", "其他"],
    ),
    (
        "家居家电",
        "expense",
        &["家具", "小家电", "灯具", "装饰", "其他"],
    ),
    ("图书文具", "expense", &["书籍", "文具", "办公用品", "其他"]),
    (
        "交通出行",
        "expense",
        &["公交", "地铁", "打车", "加油", "停车", "其他"],
    ),
    (
        "医疗健康",
        "expense",
        &["药品", "保健品", "医疗器械", "其他"],
    ),
    ("娱乐休闲", "expense", &["电影", "音乐", "运动", "其他"]),
    (
        "游戏",
        "both",
        &["梦幻西游", "充值", "道具", "账号", "装备", "其他"],
    ),
    ("工资收入", "income", &["月薪", "奖金", "兼职", "其他"]),
    ("其他收入", "income", &["退款", "红包", "投资收益", "其他"]),
    ("其他", "both", &["其他"]),
];

/// 初始化表结构（幂等），分类表为空时预置默认分类。
/// 兼容旧版（Python 版）结构：旧库 categories 表无 subs 列、子分类在
/// 独立的 subcategories 表中，此处自动迁移合并。
pub fn init_db() -> anyhow::Result<()> {
    let conn = open()?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS expenses (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            type TEXT NOT NULL DEFAULT '支出',
            item_name TEXT NOT NULL,
            store TEXT,
            purchase_date TEXT NOT NULL,
            amount REAL NOT NULL,
            category TEXT,
            subcategory TEXT,
            delivery_date TEXT,
            record_time TEXT NOT NULL,
            note TEXT
        );
        CREATE TABLE IF NOT EXISTS categories (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE,
            type TEXT NOT NULL DEFAULT 'both',
            subs TEXT NOT NULL DEFAULT '[]'
        );
        -- 排序/按日期筛选走索引（幂等，旧库启动时自动补建）
        CREATE INDEX IF NOT EXISTS idx_expenses_purchase_date ON expenses(purchase_date);",
    )?;

    // 迁移第一步：这一列有没有 —— **只补列**，而且读不出来时不乱写。
    // `ALTER TABLE … ADD COLUMN` 自带提交，所以补完之后"旧表要不要合并"就不能再参考
    // 这一步做过没有：旧写法把合并整段套在 `if !has_subs` 里，一旦 ALTER 成功而后面
    // 任何一步失败（合并查询报错、UPDATE 写不动、DROP 被别的连接握着），
    // 下一次进来 `has_subs` 已为真 ⇒ 整段合并**从此不再执行**：
    // Python 时代的子分类永远进不了下拉，而旧表还躺在库里，日志只剩一行报错。
    //
    // 判据原来是自己从 `sqlite_master.sql` 里数子串（`sql.contains("subs")`），
    // 且那次读包着 `unwrap_or_default()` ⇒ 读不出来＝"这一列不存在"：于是拿着一个
    // 本来就有这列的库去 `ADD COLUMN`，撞出一条意义不明的 duplicate column 报错，
    // 然后**整段跳过下面的合并**。现在与别的迁移共用同一把尺子（`column_presence`）。
    match crate::db::connection::column_presence(&conn, "categories", "subs") {
        crate::db::connection::Presence::Yes => {}
        crate::db::connection::Presence::No => {
            if let Err(e) = conn.execute(
                "ALTER TABLE categories ADD COLUMN subs TEXT NOT NULL DEFAULT '[]'",
                [],
            ) {
                tracing::error!(
                    "记账迁移：subs 列补不上（{e}），旧子分类这一轮没法合并，下次启动再试"
                );
                return Ok(());
            }
        }
        crate::db::connection::Presence::Unreadable(why) => {
            // 这一轮既不补列（补错就是 duplicate column）也不判"没得合并"：
            // 真缺列的话下面那步会带着原因报错、旧表原样留着，下次启动重来。
            tracing::warn!(
                "记账迁移：读不出 categories 有没有 subs 这一列（{why}）—— 这一轮不补列，\
                 合并照旧尝试，缺列会带着原因报错而不是静默跳过"
            );
        }
    }
    // 第二步：旧表**还在**就合并（与第一步是不是刚跑过无关 ⇒ 半截迁移会被重试）。
    // 这一问原先也是折好的布尔值（`COUNT(*)` 那次读包着 `unwrap_or(0)`）：读不动就
    // 等于"旧表没了"⇒ 整段合并被跳过，而 `init_db` 照样回 Ok ⇒ 记账的闩落下，
    // 这一整次运行都不会再并一次，日志一个字没有。
    match crate::db::connection::table_presence(&conn, "subcategories") {
        crate::db::connection::Presence::Yes => match merge_legacy_subcategories(&conn) {
            Ok(orphans) => {
                if orphans > 0 {
                    tracing::warn!(
                        "记账迁移：旧 subcategories 表里有 {orphans} 行的 category_id 找不到对应分类，\
                         它们没有归属、这一轮之后不再保留旧表（其余子分类已并入 subs）"
                    );
                }
                // 只有合并真的走完才收掉旧表；收不掉不算失败：下一次还会重并一次，
                // 而 `UPDATE categories SET subs=` 是按整串覆盖写的，不会越写越多。
                if let Err(e) = conn.execute("DROP TABLE IF EXISTS subcategories", []) {
                    tracing::warn!("旧 subcategories 表没能删掉（{e}）：下一次启动会再并一次");
                }
            }
            Err(e) => tracing::error!(
                "记账迁移：旧子分类这一轮没合并成（{e}）；旧表原样留着，下次启动再试，\
                 这一列先按没有子分类处理"
            ),
        },
        crate::db::connection::Presence::No => {}
        crate::db::connection::Presence::Unreadable(why) => tracing::warn!(
            "记账迁移：读不出旧的 subcategories 表还在不在（{why}）：这一轮不合并，\
             Python 时代的子分类暂时进不了下拉，下次启动会再问一次"
        ),
    }

    // 分类表为空时预置默认分类
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM categories", [], |r| r.get(0))
        .unwrap_or(0);
    if count == 0 {
        for (name, ctype, subs) in DEFAULT_CATEGORIES {
            conn.execute(
                "INSERT OR IGNORE INTO categories (name, type, subs) VALUES (?1, ?2, ?3)",
                rusqlite::params![name, ctype, subs.join(",")],
            )?;
        }
    }
    // 把历史行里没补零的 purchase_date 补齐成这一列的尺子（见函数注释）。
    //
    // 这一步**不许把整次初始化判成失败**：schema 与默认分类在上面已经做完并落库，
    // 而记账 API 每次调用都惰性走这条（`host.rs` 的闩只在成功时才落下）⇒
    // 一次写不进去（盘满、库被别的连接长期握着）就会让闩永不落下、
    // 每一次 `accounting_add` 都回「记账库没打开或写入失败」，
    // 而那个库其实开得好好的、数据也完整 —— 少的只是"某些旧行能被月度合计查到"。
    let stuck = heal_purchase_date_format(&conn);
    if !stuck.is_empty() {
        tracing::warn!(
            "purchase_date 格式有 {} 行没补齐（记账照常可用，这些行在月度合计与日期区间里仍查不到）：{}",
            stuck.len(),
            stuck.join("；")
        );
    }
    Ok(())
}

/// 把 Python 时代那张 `subcategories` 表并进 `categories.subs`。
///
/// 交回"孤儿行数"（`category_id` 找不到对应分类的那些）。旧写法是 INNER JOIN 配
/// `rows.flatten()`：孤儿与任何一行读不出来的都被静默丢掉，而那句迁移照打不误。
/// 读不动/写不动一律交回 Err —— 调用方**保留旧表**，下一次启动再并一次，
/// 而不是像旧写法那样 `?` 抛出后靠 `has_subs` 已经把整段跳过（=永久放弃）。
///
/// `sort_order` 按本仓夹具里的旧表结构是存在的；真遇到没有它的库，带它的 ORDER BY
/// 会让 prepare 直接失败。子分类的先后是用户当年排出来的，不是小事，
/// 所以先按 `sort_order` 排，排不动才退回按 id 排。
fn merge_legacy_subcategories(conn: &Connection) -> Result<usize, String> {
    let read = |sql: &str| -> rusqlite::Result<Vec<(String, String)>> {
        let mut stmt = conn.prepare(sql)?;
        let mut out = Vec::new();
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    };
    let merged = match read(
        "SELECT c.name, s.name FROM subcategories s
         JOIN categories c ON s.category_id = c.id
         ORDER BY c.id, s.sort_order, s.id",
    ) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                "旧 subcategories 表按 sort_order 排不出来（{e}）：改按 id 顺序合并，\
                 子分类的先后可能与你当年排的不一样"
            );
            read(
                "SELECT c.name, s.name FROM subcategories s
                 JOIN categories c ON s.category_id = c.id
                 ORDER BY c.id, s.id",
            )
            .map_err(|e2| format!("读旧子分类失败: {e2}"))?
        }
    };
    let orphans: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM subcategories s
             WHERE NOT EXISTS (SELECT 1 FROM categories c WHERE c.id = s.category_id)",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    // 用 Vec 而不是 HashMap：并进来的顺序跟着查询结果走，`UPDATE` 的先后也就不再
    // 每进程随机（HashMap 的迭代序是随机的，同一份旧表两次迁移会写出不同的 UPDATE 序）
    let mut cat_subs: Vec<(String, Vec<String>)> = Vec::new();
    for (cat, sub) in merged {
        match cat_subs.iter_mut().find(|(n, _)| *n == cat) {
            Some((_, list)) => list.push(sub),
            None => cat_subs.push((cat, vec![sub])),
        }
    }
    for (cat, subs) in &cat_subs {
        conn.execute(
            "UPDATE categories SET subs=?1 WHERE name=?2",
            rusqlite::params![subs.join(","), cat],
        )
        .map_err(|e| format!("分类 [{cat}] 的 subs 写不进去: {e}"))?;
    }
    Ok(orphans as usize)
}

/// 一次性把库里**格式不对**的 `purchase_date` 写成这一列声明的尺子（补零形态）。
///
/// 为什么要在 `init_db` 里做：这一列的所有读侧都是**字符串**比较（`LIKE 'YYYY-MM%'`、
/// `>=`/`<=`、`ORDER BY`），少一个零的那一行在月度合计与日期区间里永久隐身，却照常
/// 进分类盈亏（那条 SQL 不看日期）—— 同一个数两本账。写入侧从 2026-09-27 起才规范化，
/// 所以旧库、以及"整份导入 Python 时代的账本"里那种行是真实存在过的。
///
/// 认不出来的值（含空串）**原样留着**：这是补齐格式，不是替用户猜一个日期。
///
/// 交回"没补成的原因"清单（空 = 全部补齐，或本来就没有要补的行）。原先它回 `Result`
/// 并由 `init_db` 用 `?` 往外抛 ⇒ 一条 UPDATE 写不动就整次初始化失败（后果见调用点那段）；
/// 而那句"补齐了 N 行"报的是**打算**改写的行数，不是写成功的那些。
fn heal_purchase_date_format(conn: &Connection) -> Vec<String> {
    let pending = (|| -> rusqlite::Result<Vec<(i64, String)>> {
        let mut stmt = conn.prepare("SELECT id, purchase_date FROM expenses")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (id, raw) = row?;
            if let Some(norm) = normalize_date_filter(&raw) {
                if norm != raw {
                    out.push((id, norm));
                }
            }
        }
        Ok(out)
    })();
    let pending = match pending {
        Ok(rows) => rows,
        Err(e) => return vec![format!("读 expenses 失败: {e}")],
    };
    let mut fixed = 0usize;
    let mut stuck: Vec<String> = Vec::new();
    for (id, norm) in &pending {
        if let Err(e) = conn.execute(
            "UPDATE expenses SET purchase_date=?1 WHERE id=?2",
            rusqlite::params![norm, id],
        ) {
            stuck.push(format!("第 {id} 行没写成: {e}"));
            continue;
        }
        fixed += 1;
    }
    if fixed > 0 {
        tracing::warn!(
            "补齐了 {fixed} 行 purchase_date 的格式（它们此前在月度合计与日期区间里查不到）"
        );
    }
    stuck
}

fn row_to_expense(r: &rusqlite::Row<'_>) -> rusqlite::Result<Expense> {
    Ok(Expense {
        id: r.get(0)?,
        rtype: r.get(1)?,
        item_name: r.get(2)?,
        store: r.get(3)?,
        purchase_date: r.get(4)?,
        amount: r.get(5)?,
        category: r.get(6)?,
        subcategory: r.get(7)?,
        delivery_date: r.get(8)?,
        record_time: r.get(9)?,
        note: r.get(10)?,
    })
}

/// 读侧点名的列，**顺序必须与 [`row_to_expense`] 的位置一一对应**。
///
/// 原先四处都写 `SELECT *`，而 `row_to_expense` 按 0..10 的位置取列 ⇒ 列序由库决定。
/// 旧版（Python 时代）整份导入的账本里 `expenses` 用的可以是同一批列名、不同的列序
/// （`init_db` 只补 `categories` 那一侧，`expenses` 是 `CREATE TABLE IF NOT EXISTS`，
/// 老表原样留着）：那种库里"店铺"会被读成"金额"，类型对不上的整行被静默丢掉，
/// 类型碰巧能读的则变成一条数字与文字互换的记录。
/// 点名之后列序不再参与；写侧本来就点名这十一列（少哪一列 `add_expense` 会直接失败），
/// 所以这个改法不引入新的失败形态。
const EXPENSE_COLS: &str = "id, type, item_name, store, purchase_date, amount, \
                            category, subcategory, delivery_date, record_time, note";

/// 把读出来的行收起来，顺手数出"整行读不出来"的那些。
///
/// 原先三处都是 `rows.flatten().collect()`：某一行转换失败就凭空消失。
/// 分页那一头的条数取的是 `COUNT(*)`，于是界面写着「共 37 条」却只列得出 30 条，
/// 日志一个字都没有 —— 少了列序这个错因之后这条路仍可能触发（外部工具往 `amount`
/// 里写过文字就是），所以要说出来。
fn collect_expenses<I>(rows: I, which: &str) -> Vec<Expense>
where
    I: Iterator<Item = rusqlite::Result<Expense>>,
{
    let mut out = Vec::new();
    let mut dropped = 0usize;
    for row in rows {
        match row {
            Ok(e) => out.push(e),
            Err(e) => {
                dropped += 1;
                tracing::debug!("记账有一行读不出来: {e}");
            }
        }
    }
    if dropped > 0 {
        tracing::error!(
            "记账读取（{which}）有 {dropped} 行整行读不出来：它们不会出现在列表里，\
             却仍被算进那一头的条数"
        );
    }
    out
}

/// 记账允许的类型值。
///
/// 合计 SQL 用的是**字面量**比较（`type='支出'` / `type='收入'`，见
/// `monthly_summary_detail`），所以一个拼错的值不会报错，只会在所有月度合计里
/// 静默算 0 —— 记录在列表里看得见、钱却怎么也对不上。挡在写入口比在读侧猜意图便宜。
const EXPENSE_TYPES: [&str; 2] = ["支出", "收入"];

/// 分类的「适用方向」合法值 —— **不是** [`EXPENSE_TYPES`] 那一组。
///
/// 两串中文是**流水**的收支（写进 `expenses.type`，被合计 SQL 按字面量比较）；
/// 这三个英文串是**分类**的取值，来源是面板的三个选项
/// （`accounting_plugin.lua` 的 expense/income/both）与预置分类 `DEFAULT_CATEGORIES`。
/// 原先这一列收任意字符串：打错的值既不会报错，也不会生效 —— 它只在插件的标签
/// 兜底里被显示成「双向」（`t == "income" and 收入 or (t == "expense" and 支出 or 双向")`），
/// 于是用户以为改了类型，实际那行永远是"双向"。
const CATEGORY_TYPES: [&str; 3] = ["expense", "income", "both"];

/// 添加记账记录，返回 id。
#[allow(clippy::too_many_arguments)]
pub fn add_expense(
    rtype: &str,
    item_name: &str,
    store: Option<&str>,
    purchase_date: &str,
    amount: f64,
    category: Option<&str>,
    subcategory: Option<&str>,
    note: Option<&str>,
) -> i64 {
    let conn = match open() {
        Ok(c) => c,
        // 面板那一头对 -1 说的是「记账库没打开或写入失败」，而宿主对"为什么不写"
        // 原先一个字都不留（与 `add_category` 那处同形）
        Err(e) => {
            tracing::error!("添加记账记录「{item_name}」失败：记账库打不开: {e}");
            return -1;
        }
    };
    let rtype = rtype.trim();
    if !EXPENSE_TYPES.contains(&rtype) {
        tracing::warn!("记账类型「{rtype}」不在允许值里（支出/收入），这条没写进去");
        return -1;
    }
    // 日期尺子与读侧同一把（见 `normalize_date_filter`）。**空串也拒**：原先空值
    // 原样落库（`purchase_date TEXT NOT NULL` 容得下 `""`），因为**面板**自己会在
    // 提交前填今天 —— 那是面板的私有知识，不是宿主的契约。宿主收下空值就等于允许
    // 任何调用方写出一行"对月度合计与日期区间隐身、却照常进分类盈亏"的账。
    // 要"今天"就自己算好传进来。
    let date = match normalize_date_filter(purchase_date) {
        Some(d) => d,
        None => {
            tracing::warn!(
                "记账日期「{purchase_date}」不是可识别的日期（空值也算不可识别），这条没写进去"
            );
            return -1;
        }
    };
    let record_time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let r = conn.execute(
        "INSERT INTO expenses
         (type, item_name, store, purchase_date, amount, category, subcategory, record_time, note)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            rtype,
            item_name,
            store,
            date,
            amount,
            blank_to_none(category),
            blank_to_none(subcategory),
            record_time,
            note
        ],
    );
    match r {
        Ok(_) => conn.last_insert_rowid(),
        Err(e) => {
            tracing::error!("添加记账失败: {e}");
            -1
        }
    }
}

/// 更新记账记录。
pub fn update_expense(id: i64, e: &Expense) -> bool {
    let conn = match open() {
        Ok(c) => c,
        // 原先是 `Err(_) => return false`，一行日志都不留：面板显示"更新失败"，
        // 而到底是那句 UPDATE 没写成、还是库压根没打开，事后无从查证
        Err(e) => {
            tracing::error!("更新记账记录 #{id} 失败：记账库打不开: {e}");
            return false;
        }
    };
    let rtype = e.rtype.trim();
    if !EXPENSE_TYPES.contains(&rtype) {
        tracing::warn!("记账类型「{rtype}」不在允许值里（支出/收入），#{id} 没改");
        return false;
    }
    // 老库里一条日期没补零的记录，用户改个名字顺手就被修成补零形态（而不是被拒）；
    // 认不出来或空值才报错（空值被拒的理由见 `add_expense`）。
    let date = match normalize_date_filter(&e.purchase_date) {
        Some(d) => d,
        None => {
            tracing::warn!(
                "记账日期「{}」不是可识别的日期，#{id} 没改",
                e.purchase_date
            );
            return false;
        }
    };
    // 三种结局分开：这一条本来就不在（别的窗口删过 / 传进来的 id 是旧的）与
    // "这次写没写成"是两件事。旧写法 `.map(|n| n > 0).unwrap_or(false)` 把它们
    // 与"库打不开"折成同一个 false，而且这一条链上一行日志都没有。
    match conn.execute(
        "UPDATE expenses SET type=?1, item_name=?2, store=?3, purchase_date=?4,
         amount=?5, category=?6, subcategory=?7, note=?8 WHERE id=?9",
        rusqlite::params![
            rtype,
            e.item_name,
            e.store,
            date,
            e.amount,
            blank_to_none(e.category.as_deref()),
            blank_to_none(e.subcategory.as_deref()),
            e.note,
            id
        ],
    ) {
        Ok(n) if n > 0 => true,
        Ok(_) => {
            tracing::warn!(
                "更新记账记录 #{id}：这一条已经不在了（大概另一个窗口刚删掉），没有任何改动"
            );
            false
        }
        Err(err) => {
            tracing::error!("更新记账记录 #{id} 没写成: {err}");
            false
        }
    }
}

/// 删除记账记录。
///
/// 对外的 bool 口径不变（宿主与面板都读它），但把"这件事没得到答案"
/// 与"这一条本来就不在"分开放进 `Result`：旧写法是一条
/// `open().and_then(execute).map(n| n>0).unwrap_or(false)`，库打不开、语句失败、
/// 行不存在三种结局全是同一个 false，而整条链上一行日志都没有 ——
/// 面板那句「有 N 条没删成」因此既查不到原因，也没法告诉用户
/// 该刷新一下还是该去看磁盘。
pub fn delete_expense(id: i64) -> bool {
    match try_delete_expense(id) {
        Ok(deleted) => deleted,
        Err(e) => {
            tracing::error!("删除记账记录 #{id} 没做成: {e}");
            false
        }
    }
}

/// `Ok(true)` 删掉了这一条；`Ok(false)` 执行成功但那一行本来就不在；
/// `Err(原因)` 是**没机会知道结果**（库打不开、语句失败）。
fn try_delete_expense(id: i64) -> Result<bool, String> {
    let conn = open().map_err(|e| format!("记账库打不开: {e}"))?;
    conn.execute("DELETE FROM expenses WHERE id=?1", [id])
        .map(|n| n > 0)
        .map_err(|e| format!("DELETE 失败: {e}"))
}

/// 获取全部记账记录（按 id 降序）。
///
/// 读不出来与"一条记录都没有"必须分开说话：这里的调用方（`host.rs` 的 `accounting_list`）
/// 把返回的数组直接交给面板，空数组会被渲染成「（暂无数据）」——
/// 库被别的连接握着、页坏了、临时目录读不动都会走成那句话，而日志里一个字都没有，
/// 用户看到的和"我把记录删光了"完全一样。
pub fn get_all_expenses(limit: i64) -> Vec<Expense> {
    let conn = match open() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("读记账记录列表失败：记账库打不开: {e}");
            return Vec::new();
        }
    };
    let mut stmt = match conn.prepare(&format!(
        "SELECT {EXPENSE_COLS} FROM expenses ORDER BY id DESC LIMIT ?1"
    )) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("读记账记录列表失败：准备查询就没过（{e}）");
            return Vec::new();
        }
    };
    let result = stmt.query_map([limit], row_to_expense);
    match result {
        Ok(rows) => collect_expenses(rows, "全部记录"),
        Err(e) => {
            tracing::error!("读记账记录列表失败：查询开始就失败（{e}）");
            Vec::new()
        }
    }
}

/// 按日期筛选。
pub fn get_expenses_by_date(date: &str) -> Vec<Expense> {
    let conn = match open() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut stmt = match conn.prepare(&format!(
        "SELECT {EXPENSE_COLS} FROM expenses WHERE purchase_date=?1 ORDER BY id DESC"
    )) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let result = stmt.query_map([date], row_to_expense);
    match result {
        Ok(rows) => collect_expenses(rows, "按日期筛选"),
        Err(_) => Vec::new(),
    }
}

/// 分类类型的写入侧闸：trim 后必须落在 [`CATEGORY_TYPES`] 里，否则 None。
fn normalize_category_type(raw: &str) -> Option<String> {
    let t = raw.trim();
    CATEGORY_TYPES.contains(&t).then(|| t.to_string())
}

/// 添加分类。
pub fn add_category(name: &str, ctype: &str, subs: &[String]) -> i64 {
    // trim + 拒空。空名以前真能建出来（`categories` 只有 NOT NULL + UNIQUE），
    // 而它的行 id 就是 `""` —— 记账面板里 `""` 同时是"未选中"的哨兵
    // （`m_del_cat_sel` / `m_edit_cat_sel` 开头都是 `if name == "" then return`），
    // 于是**这个分类建得出来却永远删不掉、改不了、挂不上子分类**。
    // 带首尾空格的名字同样危险：记录侧写分类时走 `blank_to_none`（本文件里会 trim），
    // 存成"餐饮"而分类表里是" 餐饮"，按它筛选恒为 0 条。
    let name = name.trim();
    if name.is_empty() {
        tracing::warn!("分类名为空，已拒绝创建（空名会成为一个在界面上删不掉的分类）");
        return -1;
    }
    // 类型也走白名单（见 `CATEGORY_TYPES`）：原先任意字符串都能落库，而它唯一的
    // 症状是"这个分类在界面上永远显示双向"—— 插件的标签兜底臂把不认识的值都念成双向。
    let ctype = match normalize_category_type(ctype) {
        Some(t) => t,
        None => {
            tracing::warn!(
                "分类类型「{ctype}」不在允许值里（expense/income/both），分类 [{name}] 没建"
            );
            return -1;
        }
    };
    let conn = match open() {
        Ok(c) => c,
        // 原先这里是 `Err(_) => return -1`，一行日志都不留 —— 而面板那一头
        // （`host.rs` 的 `accounting_category_add`）对 -1 说的是
        // 「记账库没打开或写入失败（详见日志）」，那句话把人指向日志里一条根本不存在的记录。
        Err(e) => {
            tracing::error!("添加分类 [{name}] 失败：记账库打不开: {e}");
            return -1;
        }
    };
    // 查重用**同一个连接**，并把"读不出来"与"没有这个分类"分开。
    // 原先调的是 `category_type(name)`，它自己又 `open().ok()?` 一次：库被别的连接握着时
    // 它回 `None`，也就是"没有这个分类" ⇒ 于是往下走 INSERT OR IGNORE，被 UNIQUE 挡下
    // （一条都没插），最后再按名字回读拿到**已存在那一条**的 id（> 0）⇒
    // 面板报「分类 [x] 已添加」，而这次传的类型与子分类一个字都没落库。
    let exists = conn.query_row(
        "SELECT 1 FROM categories WHERE name=?1",
        [name],
        |_r| -> rusqlite::Result<()> { Ok(()) },
    );
    match exists {
        Ok(()) => {
            tracing::warn!(
                "分类 [{name}] 已经存在：这次传的类型「{ctype}」与子分类都没有落库（要改请用「修改分类」）"
            );
            return -1;
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => {}
        Err(e) => {
            tracing::error!(
                "添加分类 [{name}] 失败：查重这一步读不出来（{e}），不按「没有这个分类」继续往下写"
            );
            return -1;
        }
    }
    let subs_json = subs.join(",");
    // 不再用 `INSERT OR IGNORE` + 事后按名字回读猜 id：
    // `execute` 的返回值说的就是"这一条是不是我插进去的"，`last_insert_rowid()` 免掉
    // 第二次查询 —— 那次回读原先 `unwrap_or(-1)`，读不动时会把**已经建好的分类**报成失败。
    match conn.execute(
        "INSERT INTO categories (name, type, subs) VALUES (?1, ?2, ?3)",
        rusqlite::params![name, ctype, subs_json],
    ) {
        Ok(_) => conn.last_insert_rowid(),
        Err(e) => {
            tracing::error!("添加分类失败: {e}");
            -1
        }
    }
}

/// 重命名分类（同步更新已记账记录的分类名），返回 (成功, 错误信息)。
pub fn update_category(old_name: &str, new_name: &str, ctype: Option<&str>) -> (bool, String) {
    let conn = match open() {
        Ok(c) => c,
        Err(e) => return (false, format!("打开数据库失败: {e}")),
    };
    if old_name == new_name && ctype.is_none() {
        return (false, "没有需要修改的内容".into());
    }
    // 先 trim 再判空：`"   ".is_empty()` 是 false，所以一个纯空格的分类名以前能改成功，
    // 而它此后再也匹配不上任何记录（见 `add_category` 里那段）。
    let new_name = new_name.trim();
    if new_name.is_empty() {
        return (false, "分类名不能为空".into());
    }
    // 重名检查
    let dup: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM categories WHERE name=?1 AND name!=?2",
            rusqlite::params![new_name, old_name],
            |r| r.get(0),
        )
        .unwrap_or(0)
        > 0;
    if dup {
        return (false, format!("分类 [{new_name}] 已存在"));
    }
    // 类型传了就校验（同 `add_category`）：原先打错的值会照样落库，而它的症状只是
    // 界面上那个分类永远显示「双向」。
    let ctype = match ctype {
        Some(t) => match normalize_category_type(t) {
            Some(norm) => Some(norm),
            None => {
                return (
                    false,
                    format!(
                        "分类类型「{t}」不在允许值里（expense/income/both），[{old_name}] 没改"
                    ),
                )
            }
        },
        None => None,
    };
    // 两条 UPDATE 必须同事务：分类表改了名而 expenses 没跟上时，
    // 按分类筛选会一条都查不出来，而界面已经提示"更新成功"。
    // 显式 BEGIN IMMEDIATE：立刻拿写锁，失败就明确报错，绝不部分更新。
    if let Err(e) = conn.execute("BEGIN IMMEDIATE;", []) {
        return (false, format!("更新失败: {e}"));
    }
    let apply = (|| -> rusqlite::Result<usize> {
        let n = match ctype {
            Some(t) => conn.execute(
                "UPDATE categories SET name=?1, type=?2 WHERE name=?3",
                rusqlite::params![new_name, t, old_name],
            )?,
            None => conn.execute(
                "UPDATE categories SET name=?1 WHERE name=?2",
                rusqlite::params![new_name, old_name],
            )?,
        };
        if n > 0 {
            // 分类表已改名，历史记录必须同步，否则按分类筛选查不到任何记录
            conn.execute(
                "UPDATE expenses SET category=?1 WHERE category=?2",
                rusqlite::params![new_name, old_name],
            )?;
        }
        Ok(n)
    })();
    match apply {
        Ok(0) => {
            let _ = conn.execute("ROLLBACK;", []);
            (false, format!("分类 [{old_name}] 不存在"))
        }
        Ok(_) => match conn.execute("COMMIT;", []) {
            Ok(_) => (true, "更新成功".into()),
            Err(e) => {
                let _ = conn.execute("ROLLBACK;", []);
                (false, format!("更新失败: {e}"))
            }
        },
        Err(e) => {
            let _ = conn.execute("ROLLBACK;", []);
            (false, format!("更新失败: {e}"))
        }
    }
}

/// 删除分类（不影响已记账记录，其 category 字段保留原字符串），返回 (成功, 错误信息)。
pub fn delete_category(name: &str) -> (bool, String) {
    let conn = match open() {
        Ok(c) => c,
        Err(e) => return (false, format!("打开数据库失败: {e}")),
    };
    match conn.execute("DELETE FROM categories WHERE name=?1", [name]) {
        Ok(n) if n > 0 => (true, "删除成功".into()),
        Ok(_) => (false, format!("分类 [{name}] 不存在")),
        Err(e) => (false, format!("删除失败: {e}")),
    }
}

/// 修改分类类型（expense/income/both）。
pub fn update_category_type(name: &str, ctype: &str) -> bool {
    let Some(norm) = normalize_category_type(ctype) else {
        tracing::warn!(
            "分类类型「{ctype}」不在允许值里（expense/income/both），分类 [{name}] 没改"
        );
        return false;
    };
    let conn = match open() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("改分类 [{name}] 的类型失败：记账库打不开: {e}");
            return false;
        }
    };
    match conn.execute(
        "UPDATE categories SET type=?1 WHERE name=?2",
        rusqlite::params![norm, name],
    ) {
        Ok(n) if n > 0 => true,
        // 「这个分类不在库里」与「那句 UPDATE 没写成」以前都是同一个 false，
        // 而面板对两者说的都是"改类型失败" —— 前者刷新一下就好，后者要去查盘
        Ok(_) => {
            tracing::warn!("改分类 [{name}] 的类型：库里没有这个分类，没有任何改动");
            false
        }
        Err(e) => {
            tracing::error!("改分类 [{name}] 的类型没写成: {e}");
            false
        }
    }
}

/// 一个分类现有的子分类清单，三种结局分开交回来。
///
/// 三个调用点接下来写的都是 `UPDATE categories SET subs=<整列>` —— **覆盖**，不是追加。
/// 所以"读不出来"绝不能被折成"里面是空的"：`add_subcategory` 正是那条路 ——
/// 一次读失败（值类型不对、页面坏了、库被握着）之后它照样往下写，
/// 结果是把用户已有的整列子分类换成刚加的那一个，还回一句「添加成功」。
enum SubsRead {
    Present(Vec<String>),
    MissingCategory,
    Unreadable(String),
}

fn read_subs(conn: &Connection, category: &str) -> SubsRead {
    match conn.query_row(
        "SELECT subs FROM categories WHERE name=?1",
        [category],
        |r| r.get::<_, String>(0),
    ) {
        Ok(raw) => SubsRead::Present(parse_subs(&raw)),
        Err(rusqlite::Error::QueryReturnedNoRows) => SubsRead::MissingCategory,
        Err(e) => SubsRead::Unreadable(format!(
            "读分类 [{category}] 的子分类清单失败: {e} —— 这一列要被整列覆盖，读不动时不能接着写"
        )),
    }
}

/// 为分类添加子分类，返回 (成功, 错误信息)。
pub fn add_subcategory(category: &str, sub_name: &str) -> (bool, String) {
    let sub_name = sub_name.trim();
    if sub_name.is_empty() {
        return (false, "子分类名不能为空".into());
    }
    if sub_name.contains(',') {
        return (
            false,
            "子分类名里不能带逗号：逗号是 subs 列的内部分隔符，\"a,b\" 会被存成两个子分类".into(),
        );
    }
    let conn = match open() {
        Ok(c) => c,
        Err(e) => return (false, format!("打开数据库失败: {e}")),
    };
    let subs_raw: Vec<String> = match read_subs(&conn, category) {
        SubsRead::Present(v) => v,
        SubsRead::MissingCategory => return (false, format!("分类 [{category}] 不存在")),
        SubsRead::Unreadable(e) => return (false, e),
    };
    let mut subs = subs_raw;
    if subs.iter().any(|s| s == sub_name) {
        return (
            false,
            format!("子分类 [{sub_name}] 在 [{category}] 下已存在"),
        );
    }
    subs.push(sub_name.to_string());
    match conn.execute(
        "UPDATE categories SET subs=?1 WHERE name=?2",
        rusqlite::params![subs.join(","), category],
    ) {
        Ok(n) if n > 0 => (true, "添加成功".into()),
        Ok(_) => (false, format!("分类 [{category}] 不存在")),
        Err(e) => (false, format!("添加失败: {e}")),
    }
}

/// 重命名子分类（同步更新已记账记录），返回 (成功, 错误信息)。
pub fn update_subcategory(category: &str, old_sub: &str, new_sub: &str) -> (bool, String) {
    let new_sub = new_sub.trim();
    if new_sub.is_empty() {
        return (false, "子分类名不能为空".into());
    }
    if old_sub == new_sub {
        return (false, "没有需要修改的内容".into());
    }
    let conn = match open() {
        Ok(c) => c,
        Err(e) => return (false, format!("打开数据库失败: {e}")),
    };
    let mut subs: Vec<String> = match read_subs(&conn, category) {
        SubsRead::Present(v) => v,
        SubsRead::MissingCategory => return (false, format!("分类 [{category}] 不存在")),
        SubsRead::Unreadable(e) => return (false, e),
    };
    if new_sub.contains(',') {
        return (
            false,
            "子分类名里不能带逗号：逗号是 subs 列的内部分隔符".into(),
        );
    }
    if subs.iter().any(|s| s == new_sub) {
        return (
            false,
            format!("子分类 [{new_sub}] 在 [{category}] 下已存在"),
        );
    }
    let idx = subs.iter().position(|s| s == old_sub);
    match idx {
        Some(i) => {
            subs[i] = new_sub.to_string();
            // 与 update_category 同理：分类表的 subs 与 expenses.subcategory
            // 必须一起改，否则按子分类筛选查不到记录却提示"更新成功"。
            if let Err(e) = conn.execute("BEGIN IMMEDIATE;", []) {
                return (false, format!("更新失败: {e}"));
            }
            let apply = (|| -> rusqlite::Result<usize> {
                let n = conn.execute(
                    "UPDATE categories SET subs=?1 WHERE name=?2",
                    rusqlite::params![subs.join(","), category],
                )?;
                if n > 0 {
                    conn.execute(
                        "UPDATE expenses SET subcategory=?1 WHERE category=?2 AND subcategory=?3",
                        rusqlite::params![new_sub, category, old_sub],
                    )?;
                }
                Ok(n)
            })();
            match apply {
                Ok(0) => {
                    let _ = conn.execute("ROLLBACK;", []);
                    (false, format!("分类 [{category}] 不存在"))
                }
                Ok(_) => match conn.execute("COMMIT;", []) {
                    Ok(_) => (true, "更新成功".into()),
                    Err(e) => {
                        let _ = conn.execute("ROLLBACK;", []);
                        (false, format!("更新失败: {e}"))
                    }
                },
                Err(e) => {
                    let _ = conn.execute("ROLLBACK;", []);
                    (false, format!("更新失败: {e}"))
                }
            }
        }
        None => (false, format!("子分类 [{old_sub}] 不存在")),
    }
}

/// 删除子分类，返回 (成功, 错误信息)。
pub fn delete_subcategory(category: &str, sub_name: &str) -> (bool, String) {
    let conn = match open() {
        Ok(c) => c,
        Err(e) => return (false, format!("打开数据库失败: {e}")),
    };
    let all: Vec<String> = match read_subs(&conn, category) {
        SubsRead::Present(v) => v,
        SubsRead::MissingCategory => return (false, format!("分类 [{category}] 不存在")),
        SubsRead::Unreadable(e) => return (false, e),
    };
    let subs: Vec<String> = all
        .iter()
        .filter(|s| s.as_str() != sub_name)
        .cloned()
        .collect();
    if subs.len() == all.len() {
        return (false, format!("子分类 [{sub_name}] 不存在"));
    }
    // 两条 UPDATE 必须同事务，且必须**一起**做：`get_subcategories` 会把"记录里出现过
    // 的子分类"并回列表（见它那段注释），所以只改 categories.subs 的话，删掉的子分类
    // 立刻又出现在下拉里 —— 插件页刚说完"子分类 […] 已删除"，细分盈亏里它也还在。
    // 与 update_subcategory / update_category 同一口径。
    if let Err(e) = conn.execute("BEGIN IMMEDIATE;", []) {
        return (false, format!("删除失败: {e}"));
    }
    let apply = (|| -> rusqlite::Result<usize> {
        let n = conn.execute(
            "UPDATE categories SET subs=?1 WHERE name=?2",
            rusqlite::params![subs.join(","), category],
        )?;
        if n > 0 {
            conn.execute(
                "UPDATE expenses SET subcategory=NULL WHERE category=?1 AND subcategory=?2",
                rusqlite::params![category, sub_name],
            )?;
        }
        Ok(n)
    })();
    match apply {
        Ok(0) => {
            let _ = conn.execute("ROLLBACK;", []);
            (false, format!("分类 [{category}] 不存在"))
        }
        Ok(_) => match conn.execute("COMMIT;", []) {
            Ok(_) => (true, "删除成功".into()),
            Err(e) => {
                let _ = conn.execute("ROLLBACK;", []);
                (false, format!("删除失败: {e}"))
            }
        },
        Err(e) => {
            let _ = conn.execute("ROLLBACK;", []);
            (false, format!("删除失败: {e}"))
        }
    }
}

/// 查询分类类型（expense/income/both），不存在返回 None。
pub fn category_type(name: &str) -> Option<String> {
    let conn = open().ok()?;
    conn.query_row("SELECT type FROM categories WHERE name=?1", [name], |r| {
        r.get(0)
    })
    .ok()
}

/// 获取所有分类，并说清这份清单**能不能当完整答案用**：`(清单, 没读全的原因)`，
/// 原因空串 = 每一行都读出来了。
///
/// 第二个值不是装饰：界面调用方拿这份清单去判「用户筛的那个分类还在不在」
/// （`accounting_plugin.lua` 的 `reset_stale_filters`）。原先三个失败臂一律
/// `return Vec::new()` 且一条日志不写，「这一行读不动 / 库被别的连接握着 / 库打不开」
/// 与「用户把分类删光了」是同一个答案 —— 于是面板替用户把筛选条件与弹窗草稿里的分类
/// 一起清零，看起来就像用户自己删掉的。口径与本文件里的 `collect_expenses` 一致：
/// 读不出来的那些必须点名，且不许被当成「没有」。
pub fn get_all_categories() -> (Vec<Category>, String) {
    let conn = match open() {
        Ok(c) => c,
        Err(e) => {
            let why = format!("记账库打不开: {e}");
            tracing::error!(
                "读取分类清单失败：{why}（分类不是被删了，是这一次读不出来，界面不会据此改掉筛选条件）"
            );
            return (Vec::new(), why);
        }
    };
    let mut stmt = match conn.prepare("SELECT id, name, type, subs FROM categories ORDER BY id") {
        Ok(s) => s,
        Err(e) => {
            let why = format!("分类表读不出来（语句准备不了）: {e}");
            tracing::error!("读取分类清单失败：{why}");
            return (Vec::new(), why);
        }
    };
    let result = stmt.query_map([], |r| {
        let subs_raw: String = r.get(3)?;
        let subs: Vec<String> = parse_subs(&subs_raw);
        Ok(Category {
            id: r.get(0)?,
            name: r.get(1)?,
            ctype: r.get(2)?,
            subs,
        })
    });
    let rows = match result {
        Ok(rows) => rows,
        Err(e) => {
            let why = format!("分类一条都没读出来: {e}");
            tracing::error!("读取分类清单失败：{why}");
            return (Vec::new(), why);
        }
    };

    let mut cats: Vec<Category> = Vec::new();
    let mut dropped = 0usize;
    let mut first_err: Option<String> = None;
    for row in rows {
        match row {
            Ok(c) => cats.push(c),
            Err(e) => {
                dropped += 1;
                if first_err.is_none() {
                    first_err = Some(e.to_string());
                }
            }
        }
    }
    if dropped > 0 {
        let why = format!(
            "有 {dropped} 行分类整行读不出来（第一个原因：{}）",
            first_err.unwrap_or_else(|| "(没带上原因)".to_string())
        );
        tracing::error!(
            "读取分类清单：{why}：它们不会出现在列表里，\
             所以这份清单不能用来断定某个分类已经没了"
        );
        return (cats, why);
    }
    (cats, String::new())
}

/// `categories.subs` 的解析口径（逗号分隔），全模块共用。
///
/// 刻意把字面量 `[]` 也当作"没有子分类"：建表语句给这列的默认值是 `'[]'`，
/// `ALTER TABLE ... ADD COLUMN subs TEXT NOT NULL DEFAULT '[]'` 补列时同样如此，
/// 于是**从旧版库迁移过来、原本一条子分类都没有**的分类，列里躺着的就是 `[]`。
/// 只按 `split(',')` 过滤空串的话，界面上会凭空多出一个名叫 `[]` 的子分类（还能被
/// 选中写进记录），而给它"添加子分类"会把 `[],新名字` 真的写回库里。
fn parse_subs(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && *s != "[]")
        .map(str::to_string)
        .collect()
}

/// 按 id 获取记录。
pub fn get_expense_by_id(id: i64) -> Option<Expense> {
    let conn = open().ok()?;
    conn.query_row(
        &format!("SELECT {EXPENSE_COLS} FROM expenses WHERE id=?1"),
        [id],
        row_to_expense,
    )
    .ok()
}

/// 分类下的子分类列表（分类表中声明的 + 记录中出现过的，去重合并）。
pub fn get_subcategories(category: &str) -> Vec<String> {
    let conn = match open() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<String> = Vec::new();
    // 分类表中声明的子分类
    if let Ok(mut stmt) = conn.prepare("SELECT subs FROM categories WHERE name=?1") {
        if let Ok(mut rows) = stmt.query([category]) {
            if let Ok(Some(row)) = rows.next() {
                let subs_raw: String = row.get(0).unwrap_or_default();
                for s in parse_subs(&subs_raw) {
                    if !out.contains(&s) {
                        out.push(s);
                    }
                }
            }
        }
    }
    // 记录中出现过的子分类
    if let Ok(mut stmt) = conn.prepare(
        "SELECT DISTINCT subcategory FROM expenses WHERE category=?1 AND subcategory IS NOT NULL AND subcategory != '' ORDER BY subcategory",
    ) {
        if let Ok(mut rows) = stmt.query([category]) {
            while let Ok(Some(row)) = rows.next() {
                if let Ok(s) = row.get::<_, String>(0) {
                    if !out.contains(&s) {
                        out.push(s);
                    }
                }
            }
        }
    }
    out
}

/// 把"人手写出来的日期"规范成 `YYYY-MM-DD`：`2026-9-1`、`2026/09/01`、`2026.9.1`、
/// `20260901` 都收到同一个值；根本不是日期就返回 `None`。
///
/// 为什么要有它：`expenses.purchase_date` 存的是补零形态，而日期筛选是**字符串比较**
/// （`purchase_date >= ?`）。少一个零就漏：`"2026-9-1" > "2026-09-05"` 在字节序里成立
/// （`'9' > '0'`），于是"从 2026-9-1 起"恰好把整个九月上旬筛掉，而且一句错都不报。
/// 同一个弹窗里的日期控件给的却是补零形态（`kind="date"`）—— 同一个列两套口径。
pub fn normalize_date_filter(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let parts: Vec<&str> = s.split(['-', '/', '.']).collect();
    let (ys, ms, ds) = match parts.len() {
        3 => (parts[0], parts[1], parts[2]),
        // 紧凑写法 20260901：八位数字
        1 if parts[0].len() == 8 && parts[0].bytes().all(|b| b.is_ascii_digit()) => {
            let p = parts[0];
            (p.get(..4)?, p.get(4..6)?, p.get(6..8)?)
        }
        _ => return None,
    };
    let num = |seg: &str| -> Option<i64> {
        let t = seg.trim();
        if t.is_empty() || t.len() > 8 || !t.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        t.parse::<i64>().ok()
    };
    let (y, m, d) = (num(ys)?, num(ms)?, num(ds)?);
    if !(1970..=9999).contains(&y) {
        return None;
    }
    // from_ymd_opt 顺带管掉 2 月 30 日、13 月这类"数字都对但日子不存在"的输入
    chrono::NaiveDate::from_ymd_opt(y as i32, m as u32, d as u32)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
}

/// 分页 + 筛选查询：返回 (records, total)。
/// category/subcategory/keyword/date_from/date_to 为空时不过滤。
#[allow(clippy::too_many_arguments)]
pub fn get_expenses_page(
    page: i64,
    page_size: i64,
    category: Option<&str>,
    subcategory: Option<&str>,
    keyword: Option<&str>,
    date_from: Option<&str>,
    date_to: Option<&str>,
) -> (Vec<Expense>, i64) {
    let conn = match open() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("读记账分页失败：记账库打不开: {e}");
            return (Vec::new(), 0);
        }
    };
    let mut conds: Vec<String> = Vec::new();
    let mut params: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(c) = category.filter(|c| !c.is_empty()) {
        conds.push("category=?".to_string());
        params.push(rusqlite::types::Value::Text(c.to_string()));
    }
    if let Some(s) = subcategory.filter(|s| !s.is_empty()) {
        conds.push("subcategory=?".to_string());
        params.push(rusqlite::types::Value::Text(s.to_string()));
    }
    if let Some(k) = keyword.filter(|k| !k.is_empty()) {
        // 转义 LIKE 通配符，用户输入按字面匹配
        let escaped = k
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        conds.push(
            "(item_name LIKE ? ESCAPE '\\' OR category LIKE ? ESCAPE '\\'
             OR subcategory LIKE ? ESCAPE '\\' OR store LIKE ? ESCAPE '\\'
             OR note LIKE ? ESCAPE '\\')"
                .to_string(),
        );
        let like = format!("%{}%", escaped);
        for _ in 0..5 {
            params.push(rusqlite::types::Value::Text(like.clone()));
        }
    }
    // 日期两端先规范成库里那套补零形态，再进字符串比较（见 `normalize_date_filter`）。
    // 规范不出来的（"上个月"）按"这一条筛选不生效"处理：与其悄悄少筛一半，
    // 不如把全部列出来 —— 面板那一头会在筛选栏下面把"哪一条没认出来"说清楚。
    let from_norm = date_from
        .filter(|f| !f.is_empty())
        .and_then(normalize_date_filter);
    if let Some(f) = from_norm.as_deref() {
        conds.push("purchase_date>=?".to_string());
        params.push(rusqlite::types::Value::Text(f.to_string()));
    }
    let to_norm = date_to
        .filter(|t| !t.is_empty())
        .and_then(normalize_date_filter);
    if let Some(t) = to_norm.as_deref() {
        conds.push("purchase_date<=?".to_string());
        params.push(rusqlite::types::Value::Text(t.to_string()));
    }
    let where_sql = if conds.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conds.join(" AND "))
    };
    let page = page.clamp(1, 1_000_000);
    let page_size = page_size.clamp(1, 200);
    let offset = (page - 1) * page_size;

    let total: i64 = {
        let sql = format!("SELECT COUNT(*) FROM expenses{where_sql}");
        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("读记账分页的条数失败（{e}）");
                return (Vec::new(), 0);
            }
        };
        let mut rows = match stmt.query(rusqlite::params_from_iter(params.iter())) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("读记账分页的条数失败：查询没跑起来（{e}）");
                return (Vec::new(), 0);
            }
        };
        match rows.next() {
            Ok(Some(row)) => row.get(0).unwrap_or(0),
            Ok(None) => 0,
            Err(e) => {
                tracing::error!("读记账分页的条数失败：结果读不出来（{e}）");
                0
            }
        }
    };

    // 按日期倒序（最新在前），同日期按 id 倒序。
    // 注意：占位符必须用位置 ?（不能用 ?1/?2 编号），否则带筛选时 LIMIT 会绑到筛选参数上
    let sql = format!(
        "SELECT {EXPENSE_COLS} FROM expenses{where_sql}
         ORDER BY purchase_date DESC, id DESC LIMIT ? OFFSET ?"
    );
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            // 条数是刚刚才查证过的真数，不许在这里被改成 0：面板会因此显示
            // 「共 0 条」，而用户的记录一条都没少 —— 那是比"列不出来"更坏的一句假话。
            // 这一支的矛盾（有 N 条却列不出行）由日志说清，不由返回值撒谎来遮。
            tracing::error!(
                "读记账分页的明细失败（{e}）：条数 {total} 仍然有效，但这一页没有行可读"
            );
            return (Vec::new(), total);
        }
    };
    let mut all_params: Vec<rusqlite::types::Value> = params;
    all_params.push(page_size.into());
    all_params.push(offset.into());
    let result = stmt.query_map(
        rusqlite::params_from_iter(all_params.iter()),
        row_to_expense,
    );
    let records = match result {
        Ok(rows) => collect_expenses(rows, "分页列表"),
        Err(e) => {
            tracing::error!("读记账分页的明细失败：查询没跑起来（{e}）：条数 {total} 仍然有效");
            Vec::new()
        }
    };
    (records, total)
}

/// 月度汇总（含分类明细）：返回 (总支出, 总收入, 条数, [(分类, 净额=收入-支出)]，按净额降序)。
/// 金额按「分」求和的 SQL 片段（整数相加，精确）。
///
/// `amount` 是 REAL：逐条相加会累积二进制浮点误差（`0.1 + 0.2 =
/// 0.30000000000000004`），于是分类/月份汇总偶尔比明细逐条加起来多一分、
/// 少一分。整数加法没有这个问题，而 `ROUND(x * 100)` 对按两位小数录入的金额
/// 稳定收敛（REAL 里的 12.34 是 12.3400000000000002，×100 后 ROUND 回 1234）。
///
/// 刻意不改 schema、不改 Lua 侧的 f64 口径，只在聚合这一步归一：单条金额读出来
/// 仍是原样的 REAL，只有「合计」变成先化分、整数相加、最后除回 100。
fn sum_cents(inner: &str) -> String {
    format!("COALESCE(SUM(CAST(ROUND(({inner}) * 100) AS INTEGER)), 0)")
}

/// 上者的金额（元）形式：分维度求和后做一次除法，避免两端各自除完再相减又引入漂移
fn sum_amount_sql(inner: &str) -> String {
    format!("({}) / 100.0", sum_cents(inner))
}

/// 把用户或脚本给的月份写法归一成 `YYYY-MM`；认不出来返回 `None`。
///
/// 为什么要有它：月度汇总是拿 `purchase_date LIKE '<ym>%'` 做**字符串前缀**匹配，
/// 前缀本身直接决定统计范围，而 `ym` 是原样开放给第三方脚本的 `String`
/// （`plugins/host.rs` 的 `accounting_monthly*`）。不守这个口有三类静默错法：
/// - `"2026-1"`（人手写一月）：前缀命中 2026-10/11/12 三个月，而**真正的一月一条不算**；
/// - `""`：前缀成 `%` ⇒ 全时段总额被当成一个月报出去（Lua 表头还照原样印那个空 ym）；
/// - 含 `%` / `_`：LIKE 的通配符，等于让调用方自己拼窗口。
///   归一之后进 `LIKE` 的串只剩数字与 `-`，通配符没有入口。
pub fn normalize_month(raw: &str) -> Option<String> {
    let s = raw.trim();
    let parts: Vec<&str> = s.split(['-', '/', '.']).collect();
    let (ys, ms) = match parts[..] {
        [y, m] => (y, m),
        // 紧凑写法 202601
        [one] if one.len() == 6 && one.bytes().all(|b| b.is_ascii_digit()) => {
            (one.get(..4)?, one.get(4..)?)
        }
        _ => return None,
    };
    let num = |seg: &str| -> Option<i64> {
        if seg.is_empty() || seg.len() > 4 || !seg.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        seg.parse().ok()
    };
    let (y, m) = (num(ys)?, num(ms)?);
    if !(1970..=9999).contains(&y) || !(1..=12).contains(&m) {
        return None;
    }
    Some(format!("{y:04}-{m:02}"))
}

/// 汇总类读数的「这份数能不能当完整答案」：空串 = 每一个数都读出来了。
///
/// 三个读数口（月度汇总、分类盈亏、细分盈亏）共用这一个通道，因为面板拿到零与空列表时
/// 说的是同一句话 —— 「这个月没花钱」「（暂无数据）」，而那一次可能什么都没读到。
/// 口径与上一轮的 `get_all_categories`、本文件的 `collect_expenses` 一致：读不出来的
/// 那一部分必须点名，不许折进一份格式完整的报告里。
#[derive(Default)]
struct ReadGaps(Vec<String>);

impl ReadGaps {
    fn push(&mut self, why: impl Into<String>) {
        self.0.push(why.into());
    }

    /// 收口：读全了交空串；没读全就留一条 error 并交出那句话，让调用方据此**不下结论**。
    fn finish(self, what: &str) -> String {
        if self.0.is_empty() {
            return String::new();
        }
        let gap = self.0.join("；");
        tracing::error!(
            "记账读数（{what}）没读全：{gap} —— 零与空列表这一次不是结论，界面不许把它们念成结论"
        );
        gap
    }
}

/// 月度汇总明细：`(总支出, 总收入, 条数, [(分类, 净收入)], 没读全的原因)`。
///
/// 最后一个值是空串才许面板说「这个月就这样」。原先 `open()` 失败折成
/// `(0.0, 0.0, 0, [])`、三个合计各写一处 `unwrap_or`、分类明细那一圈
/// `if let Ok(..)` 没有 else 且行级失败整行丢掉 ⇒ 一次瞬时读不动就印出
/// 「总支出 0.00 / 记录数 0 / （本月暂无分类明细）」，而隔壁列表同一帧正写着「共 37 条」。
pub fn monthly_summary_detail(year_month: &str) -> (f64, f64, i64, Vec<(String, f64)>, String) {
    let mut gaps = ReadGaps::default();
    let conn = match open() {
        Ok(c) => c,
        Err(e) => {
            gaps.push(format!("记账库打不开: {e}"));
            return (0.0, 0.0, 0, Vec::new(), gaps.finish("月度汇总"));
        }
    };
    let ym = match normalize_month(year_month) {
        Some(v) => v,
        None => {
            gaps.push(format!("月份写法「{year_month}」认不出来（需要 YYYY-MM）"));
            return (0.0, 0.0, 0, Vec::new(), gaps.finish("月度汇总"));
        }
    };
    let prefix = format!("{ym}%");
    let expense: f64 = match conn.query_row(
        &format!(
            "SELECT {} FROM expenses WHERE type='支出' AND purchase_date LIKE ?1",
            sum_amount_sql("amount")
        ),
        [&prefix],
        |r| r.get::<_, f64>(0),
    ) {
        Ok(v) => v,
        Err(e) => {
            gaps.push(format!("支出合计读不出来: {e}"));
            0.0
        }
    };
    let income: f64 = match conn.query_row(
        &format!(
            "SELECT {} FROM expenses WHERE type='收入' AND purchase_date LIKE ?1",
            sum_amount_sql("amount")
        ),
        [&prefix],
        |r| r.get::<_, f64>(0),
    ) {
        Ok(v) => v,
        Err(e) => {
            gaps.push(format!("收入合计读不出来: {e}"));
            0.0
        }
    };
    let count: i64 = match conn.query_row(
        "SELECT COUNT(*) FROM expenses WHERE purchase_date LIKE ?1",
        [&prefix],
        |r| r.get::<_, i64>(0),
    ) {
        Ok(v) => v,
        Err(e) => {
            gaps.push(format!("条数读不出来: {e}"));
            0
        }
    };
    let mut cat_stats: Vec<(String, f64)> = Vec::new();
    // 净额在 SQL 里按分相减、只在最后除一次：分两次查出 inc/exp 再在 Rust 里
    // 相减，等于把浮点漂移又请回来一次
    let cat_sql = format!(
        "SELECT COALESCE(NULLIF(category, ''), '(未分类)') AS cat,
                ({inc} - {exp}) / 100.0 AS net
         FROM expenses WHERE purchase_date LIKE ?1 GROUP BY cat
         ORDER BY net DESC",
        inc = sum_cents("CASE WHEN type='收入' THEN amount ELSE 0 END"),
        exp = sum_cents("CASE WHEN type='支出' THEN amount ELSE 0 END"),
    );
    match conn.prepare(&cat_sql) {
        Ok(mut stmt) => match stmt.query([&prefix]) {
            Ok(mut rows) => loop {
                match rows.next() {
                    Ok(Some(row)) => match (row.get::<_, String>(0), row.get::<_, f64>(1)) {
                        (Ok(cat), Ok(net)) => cat_stats.push((cat, net)),
                        (Ok(_), Err(e)) | (Err(e), _) => {
                            gaps.push(format!("分类明细里有一行读不出来: {e}"))
                        }
                    },
                    Ok(None) => break,
                    Err(e) => {
                        gaps.push(format!("分类明细读到一半断了: {e}"));
                        break;
                    }
                }
            },
            Err(e) => gaps.push(format!("分类明细查不动: {e}")),
        },
        Err(e) => gaps.push(format!("分类明细的语句准备不了: {e}")),
    }
    (expense, income, count, cat_stats, gaps.finish("月度汇总"))
}

/// 分类盈亏统计：`([(分类, 投入=支出合计, 赚取=收入合计, 条数)], 没读全的原因)`。
///
/// 原因空串 = 这一份可以当完整答案。原先库打不开、语句准备不了、遍历中断、
/// 单行读不出来四种结局全折成一个空 `Vec`，面板接着印「总投入：0.00 总赚取：0.00」
/// 加一句「（暂无数据）」—— 一份读不出来的账被写成一份确定的零报告。
pub fn category_profit_loss() -> (Vec<(String, f64, f64, i64)>, String) {
    let mut gaps = ReadGaps::default();
    let conn = match open() {
        Ok(c) => c,
        Err(e) => {
            gaps.push(format!("记账库打不开: {e}"));
            return (Vec::new(), gaps.finish("分类盈亏"));
        }
    };
    let mut out = Vec::new();
    let sql = format!(
        "SELECT COALESCE(NULLIF(category, ''), '(未分类)') AS cat,
                {inv} AS inv,
                {earn} AS earn,
                COUNT(*) AS cnt
         FROM expenses GROUP BY cat ORDER BY inv - earn",
        inv = sum_amount_sql("CASE WHEN type='支出' THEN amount ELSE 0 END"),
        earn = sum_amount_sql("CASE WHEN type='收入' THEN amount ELSE 0 END"),
    );
    read_profit_rows(&conn, &sql, "分类盈亏", None, &mut out, &mut gaps);
    (out, gaps.finish("分类盈亏"))
}

/// 指定分类下子分类盈亏：`([(子分类, 投入, 赚取, 条数)], 没读全的原因)`。
pub fn subcategory_profit_loss(category: &str) -> (Vec<(String, f64, f64, i64)>, String) {
    let mut gaps = ReadGaps::default();
    let conn = match open() {
        Ok(c) => c,
        Err(e) => {
            gaps.push(format!("记账库打不开: {e}"));
            return (Vec::new(), gaps.finish("细分盈亏"));
        }
    };
    let mut out = Vec::new();
    let sql = format!(
        "SELECT COALESCE(NULLIF(subcategory, ''), '(未分细类)') AS sub,
                {inv} AS inv,
                {earn} AS earn,
                COUNT(*) AS cnt
         FROM expenses WHERE category=?1 GROUP BY sub ORDER BY inv - earn",
        inv = sum_amount_sql("CASE WHEN type='支出' THEN amount ELSE 0 END"),
        earn = sum_amount_sql("CASE WHEN type='收入' THEN amount ELSE 0 END"),
    );
    read_profit_rows(&conn, &sql, "细分盈亏", Some(category), &mut out, &mut gaps);
    (out, gaps.finish("细分盈亏"))
}

/// 两张盈亏表共用那一条读法（只有名字列与要不要 `WHERE category=?1` 不同）。
/// 每一处失败都记进 `gaps`：语句准备不了、查不动、读到一半断了、单行是坏值 ——
/// 四种结局原先在外头都是同一句「（暂无数据）」。
fn read_profit_rows(
    conn: &rusqlite::Connection,
    sql: &str,
    what: &str,
    category: Option<&str>,
    out: &mut Vec<(String, f64, f64, i64)>,
    gaps: &mut ReadGaps,
) {
    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(e) => {
            gaps.push(format!("{what}的语句准备不了: {e}"));
            return;
        }
    };
    let fetched = match category {
        Some(cat) => stmt.query([cat]),
        None => stmt.query([]),
    };
    let mut rows = match fetched {
        Ok(r) => r,
        Err(e) => {
            gaps.push(format!("{what}查不动: {e}"));
            return;
        }
    };
    loop {
        match rows.next() {
            Ok(Some(row)) => {
                match (
                    row.get::<_, String>(0),
                    row.get::<_, f64>(1),
                    row.get::<_, f64>(2),
                    row.get::<_, i64>(3),
                ) {
                    (Ok(name), Ok(inv), Ok(earn), Ok(cnt)) => out.push((name, inv, earn, cnt)),
                    _ => gaps.push(format!("{what}里有一行读不出来")),
                }
            }
            Ok(None) => break,
            Err(e) => {
                gaps.push(format!("{what}读到一半断了: {e}"));
                break;
            }
        }
    }
}

/// 两个日期之间的「整年 + 余下天数」，按日历周年数。
///
/// 不能拿 `总天数 / 365` 当整年：闰年混进来之后余数会漂移，差一天满三年的
/// 记录会被算成"已经 3 年"（2021-03-16 → 2024-03-15 恰好是 1095 天，
/// 除下来是 (3, 0)）。界面上这就是一句读起来怪、又没人会去核对的话。
pub fn years_and_days(from: chrono::NaiveDate, today: chrono::NaiveDate) -> (i64, i64) {
    if from >= today {
        return (0, 0);
    }
    let mut years: u32 = 0;
    while from
        .checked_add_months(chrono::Months::new((years + 1) * 12))
        .is_some_and(|d| d <= today)
    {
        years += 1;
    }
    let anniversary = from
        .checked_add_months(chrono::Months::new(years * 12))
        .unwrap_or(from);
    (years as i64, (today - anniversary).num_days().max(0))
}

/// 距今多久：返回 [(id, 年, 天)]（天为去掉整年后的余数）。
pub fn days_ago(ids: &[i64]) -> Vec<(i64, i64, i64)> {
    let conn = match open() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let today = chrono::Local::now().date_naive();
    let mut out = Vec::new();
    let mut stmt = match conn.prepare("SELECT id, purchase_date FROM expenses WHERE id=?1") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    for id in ids {
        if let Ok(mut rows) = stmt.query([id]) {
            if let Ok(Some(row)) = rows.next() {
                if let (Ok(rid), Ok(date_str)) = (row.get::<_, i64>(0), row.get::<_, String>(1)) {
                    // 尺子用本列声明的那把（`normalize_date_filter`）。原先这里用严格
                    // `%Y-%m-%d`：实测它**能**解 `2026-9-1`（chrono 的数字位宽是弹性的），
                    // 却只认破折号分隔 —— `2026/9/1`、`2026.9.1`、`20260901` 这些本列
                    // 承认的写法在它眼里等于"没有日期"，那一行就在"距今多久"里隐身。
                    let parsed = normalize_date_filter(&date_str)
                        .and_then(|norm| chrono::NaiveDate::parse_from_str(&norm, "%Y-%m-%d").ok());
                    if let Some(date) = parsed {
                        let (years, rest) = years_and_days(date, today);
                        out.push((rid, years, rest));
                    }
                }
            }
        }
    }
    out
}

/// 月度汇总：返回 (总支出, 总收入)。
pub fn monthly_summary(year_month: &str) -> (f64, f64) {
    let conn = match open() {
        Ok(c) => c,
        Err(_) => return (0.0, 0.0),
    };
    let ym = match normalize_month(year_month) {
        Some(v) => v,
        None => {
            tracing::error!(
                "月度汇总的月份参数认不出来（{year_month:?}），本次按 0 返回；需要的是 YYYY-MM"
            );
            return (0.0, 0.0);
        }
    };
    let prefix = format!("{ym}%");
    let expense: f64 = conn
        .query_row(
            &format!(
                "SELECT {} FROM expenses WHERE type='支出' AND purchase_date LIKE ?1",
                sum_amount_sql("amount")
            ),
            [&prefix],
            |r| r.get(0),
        )
        .unwrap_or(0.0);
    let income: f64 = conn
        .query_row(
            &format!(
                "SELECT {} FROM expenses WHERE type='收入' AND purchase_date LIKE ?1",
                sum_amount_sql("amount")
            ),
            [&prefix],
            |r| r.get(0),
        )
        .unwrap_or(0.0);
    (expense, income)
}

#[cfg(test)]
mod tests {
    /// 宿主侧这道日期规范化得有自己的网。
    ///
    /// 面板与宿主两侧都做（同规则、双保险），于是面板级用例**证明不了宿主这半边**：
    /// 注掉宿主那侧、面板照样补零，端到端用例仍然绿 —— 本场做注入验证时就是这么发现的。
    #[test]
    fn normalize_date_filter_pads_and_rejects_junk() {
        let norm = |s: &str| super::normalize_date_filter(s);
        assert_eq!(
            norm("2026-9-1").as_deref(),
            Some("2026-09-01"),
            "不补零的要收成库里的形态"
        );
        assert_eq!(norm("2026/9/1").as_deref(), Some("2026-09-01"));
        assert_eq!(
            norm("20260901").as_deref(),
            Some("2026-09-01"),
            "紧凑八位也认"
        );
        assert_eq!(
            norm("  2026-09-05  ").as_deref(),
            Some("2026-09-05"),
            "首尾空白不算错"
        );
        assert_eq!(norm(""), None, "空 = 不参与筛选");
        assert_eq!(norm("2026-13-1"), None, "没有 13 月，不能静默当成没筛");
        assert_eq!(norm("2026-2-30"), None, "数字都对但日子不存在");
        assert_eq!(norm("昨天"), None, "认不出来必须报出来");
    }
    use super::*;

    /// 「几年几天」按日历周年，不按 365 天一块切。
    ///
    /// 老实现是 `days/365, days%365`：2021-03-16 → 2024-03-15 正好 1095 天，
    /// 除完报「3 年」，而那天还差一天才满三年。跨闰年时误差必然出现，
    /// 因为一年从来不是 365 天。
    #[test]
    fn years_and_days_counts_calendar_anniversaries_not_365_day_blocks() {
        let d = |y, m, day| chrono::NaiveDate::from_ymd_opt(y, m, day).unwrap();
        assert_eq!(
            years_and_days(d(2021, 3, 16), d(2024, 3, 15)),
            (2, 365),
            "差一天满三年：不能报成 3 年"
        );
        assert_eq!(
            years_and_days(d(2021, 3, 16), d(2024, 3, 16)),
            (3, 0),
            "整三年那天才是 3 年"
        );
        assert_eq!(years_and_days(d(2020, 1, 1), d(2020, 1, 31)), (0, 30));
        assert_eq!(years_and_days(d(2026, 1, 1), d(2026, 1, 1)), (0, 0));
        assert_eq!(
            years_and_days(d(2026, 5, 1), d(2026, 1, 1)),
            (0, 0),
            "未来日期（手填错了购买日期）不该给出负数"
        );
        // 2 月 29 日出生的记录：周年被夹到 2/28，不该因此少算一整年
        assert_eq!(years_and_days(d(2024, 2, 29), d(2025, 2, 28)), (1, 0));
    }

    /// 从旧版库迁移过来的分类，`subs` 列里躺的是建表默认值 `'[]'` 而不是空串
    /// （`ALTER TABLE categories ADD COLUMN subs TEXT NOT NULL DEFAULT '[]'`：只有
    /// 旧 `subcategories` 表里有行的那批会被 UPDATE 覆盖，一条子分类都没有的那批
    /// 就留着字面量 `[]`）。各读取点原先只 `split(',')` 去空串，于是界面上凭空多出
    /// 一个名叫 `[]` 的子分类，还能被选中写进记录。
    #[test]
    fn migrated_empty_subs_is_not_a_subcategory() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("acc_subs");
        init_db().expect("建库失败");
        {
            let conn = open().expect("打开库失败");
            conn.execute(
                "INSERT INTO categories (name,type,subs) VALUES ('迁移来的','both','[]')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO categories (name,type,subs) VALUES ('新建的','both','')",
                [],
            )
            .unwrap();
        }

        let migrated = get_all_categories()
            .0
            .into_iter()
            .find(|c| c.name == "迁移来的")
            .expect("应读到该分类");
        assert!(
            migrated.subs.is_empty(),
            "默认值 [] 不该变成一个可选中的子分类: {:?}",
            migrated.subs
        );
        assert_eq!(get_subcategories("迁移来的"), Vec::<String>::new());
        // 给它加子分类时，也不能把 [] 一并写回库里
        let (ok, msg) = add_subcategory("迁移来的", "零食");
        assert!(ok, "{msg}");
        assert_eq!(get_subcategories("迁移来的"), vec!["零食".to_string()]);
        // 逗号是 subs 列的内部分隔符：带逗号的名字会被存成**两个**子分类，
        // 之后按其中一个名字去删会把另一个也带走。只能堵在入口。
        let (ok, msg) = add_subcategory("迁移来的", "零食,饮料");
        assert!(!ok, "带逗号的名字必须被拒");
        assert!(msg.contains("逗号"), "{msg}");
        assert_eq!(
            get_subcategories("迁移来的"),
            vec!["零食".to_string()],
            "被拒的名字不该落库"
        );

        // 反向腿：真实子分类必须照旧读得到，否则上面几条只是"什么都没读到"
        {
            let conn = open().expect("打开库失败");
            conn.execute(
                "UPDATE categories SET subs='饮料,零食' WHERE name='新建的'",
                [],
            )
            .unwrap();
        }
        assert_eq!(
            get_subcategories("新建的"),
            vec!["饮料".to_string(), "零食".to_string()]
        );
    }

    /// 「没有分类」在库里可以有两种写法，聚合必须把它们当成同一件事。
    ///
    /// 插件的下拉框用空串表示"（无）/（请选择）"，旧版 Python 导入的记录用的是 NULL。
    /// 旧实现按 `category` 原样 GROUP BY、只在 NULL 上兜 `(未分类)`，于是分类盈亏
    /// 里出现一个**名字为空**的行（插件把它拼成 `"  : 12.00"`），而同一件"没分类"
    /// 被拆成两行、两边的数都不对。
    #[test]
    fn empty_and_null_category_are_the_same_bucket() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("acc_blank_cat");
        init_db().expect("建库失败");
        let ym = chrono::Local::now().format("%Y-%m").to_string();
        let day = chrono::Local::now().format("%Y-%m-%d").to_string();

        // ① 写入侧：空串/纯空白的分类与子分类不该落成一个空名字
        let a = add_expense(
            "支出",
            "无分类甲",
            None,
            &day,
            10.0,
            Some(""),
            Some("  "),
            None,
        );
        let b = add_expense(
            "支出",
            "无分类乙",
            None,
            &day,
            5.0,
            Some("  "),
            Some(""),
            None,
        );
        assert!(a > 0 && b > 0, "写入应成功: {a} {b}");
        let rec = get_expense_by_id(a).expect("记录应能读回");
        assert_eq!(rec.category, None, "空白分类该落成 NULL");
        assert_eq!(rec.subcategory, None, "空白子分类该落成 NULL");
        // 反向腿：非空白的照原样留下（首尾空格被 trim，否则 GROUP BY 里又是两个分类）
        let c = add_expense(
            "支出",
            "有分类",
            None,
            &day,
            2.0,
            Some(" 餐饮 "),
            Some(" 零食 "),
            None,
        );
        assert_eq!(
            get_expense_by_id(c).unwrap().category.as_deref(),
            Some("餐饮"),
            "有效分类不该被动"
        );

        // ② 库里再补两条旧形态 —— 写入侧管不到历史数据：NULL 一类、真空串一类
        {
            let conn = open().unwrap();
            conn.execute(
                "INSERT INTO expenses (type,item_name,purchase_date,amount,category,subcategory,record_time)
                 VALUES ('收入','旧版导入',?1,25.0,NULL,NULL,?2)",
                rusqlite::params![day, day],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO expenses (type,item_name,purchase_date,amount,category,subcategory,record_time)
                 VALUES ('支出','修复前写入',?1,7.0,'','',?2)",
                rusqlite::params![day, day],
            )
            .unwrap();
        }

        // ③ 读取侧：NULL 与空串并成一行 (未分类)，且不许出现空名字的行
        let (profit, profit_gap) = category_profit_loss();
        assert!(profit_gap.is_empty(), "读全了不该带原因: {profit_gap}");
        let names: Vec<&str> = profit.iter().map(|(n, ..)| n.as_str()).collect();
        assert!(
            !names.iter().any(|n| n.is_empty()),
            "不该出现名字为空的分类行: {names:?}"
        );
        let blank: Vec<&(String, f64, f64, i64)> =
            profit.iter().filter(|(n, ..)| n == "(未分类)").collect();
        assert_eq!(blank.len(), 1, "NULL 与空串必须并成一行: {names:?}");
        let row = blank[0];
        assert_eq!(
            (row.1, row.2, row.3),
            (22.0, 25.0, 4),
            "(未分类) 的四笔应并成一行（10+5+7 支出、25 收入）: {row:?}"
        );
        assert!(names.contains(&"餐饮"), "非空白分类照常成行: {names:?}");

        // ④ 月度分类明细同一口径
        let (_, _, _, detail, detail_gap) = monthly_summary_detail(&ym);
        assert!(detail_gap.is_empty(), "读全了不该带原因: {detail_gap}");
        assert_eq!(
            detail.iter().filter(|(n, _)| n == "(未分类)").count(),
            1,
            "月度明细不该把同一件事拆成两行: {detail:?}"
        );
        assert!(
            detail.iter().all(|(n, _)| !n.is_empty()),
            "月度明细里不该有空名字: {detail:?}"
        );

        // ⑤ 子分类：同一分类下 '' 与 NULL 不能各占一行 "(未分细类)"
        add_expense(
            "支出",
            "细类空",
            None,
            &day,
            1.0,
            Some("餐饮"),
            Some(""),
            None,
        );
        {
            let conn = open().unwrap();
            conn.execute(
                "INSERT INTO expenses (type,item_name,purchase_date,amount,category,subcategory,record_time)
                 VALUES ('支出','细类NULL',?1,2.0,'餐饮',NULL,?2)",
                rusqlite::params![day, day],
            )
            .unwrap();
        }
        let (sub, sub_gap) = subcategory_profit_loss("餐饮");
        assert!(sub_gap.is_empty(), "读全了不该带原因: {sub_gap}");
        let sub_names: Vec<&str> = sub.iter().map(|(n, ..)| n.as_str()).collect();
        assert_eq!(
            sub_names.iter().filter(|n| **n == "(未分细类)").count(),
            1,
            "不该出现两行 (未分细类): {sub_names:?}"
        );
        let merged = sub.iter().find(|(n, ..)| n == "(未分细类)").unwrap();
        assert_eq!(
            (merged.1, merged.3),
            (3.0, 2),
            "两笔空白子分类该并成一行: {merged:?}"
        );
        assert!(
            sub_names.contains(&"零食"),
            "有名字的子分类照常成行: {sub_names:?}"
        );
    }

    /// 回归（第 23 轮）：`delete_subcategory` 曾经只改 `categories.subs`，不像
    /// `update_subcategory` / `update_category` 那样同步清 `expenses.subcategory`。
    /// 而 `get_subcategories` 刻意会把"记录里出现过的子分类"并回列表 ⇒ 删了立刻又长回来，
    /// 面板刚说完"子分类 […] 已删除"，细分盈亏里它也还在。
    #[test]
    fn deleted_subcategory_goes_away_from_records_too() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("acc_del_sub");
        init_db().expect("建库失败");
        // 分类名刻意避开 `init_db` 预置的那批（"游戏" 就是预置分类，且自带六个子分类）
        assert!(add_category("虚拟道具", "both", &["点卡".to_string()]) > 0);
        let hit = add_expense(
            "支出",
            "月卡",
            None,
            "2026-09-05",
            68.0,
            Some("虚拟道具"),
            Some("点卡"),
            None,
        );
        assert!(hit > 0, "记账没写进去，后面的断言都没有意义");
        let (ok, msg) = delete_subcategory("虚拟道具", "点卡");
        assert!(ok, "删除子分类该成功，实得: {msg}");
        // ① 列表面：删掉的不能因为"记录里出现过"又长回来
        let subs = get_subcategories("虚拟道具");
        assert!(subs.is_empty(), "删掉的子分类不能还在列表里: {subs:?}");
        // ② 记录面：那条记录的 subcategory 必须一起清掉
        let sub: Option<String> = {
            let conn = open().unwrap();
            conn.query_row("SELECT subcategory FROM expenses WHERE id=?1", [hit], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert!(
            sub.clone().unwrap_or_default().is_empty(),
            "记录里的子分类要一起清掉，实得 {sub:?}"
        );
        // ③ 反向腿：UPDATE 必须带 category 条件，别的分类不能被牵连
        assert!(add_category("食品", "both", &["零食".to_string(), "饮料".to_string()]) > 0);
        let other = add_expense(
            "支出",
            "薯片",
            None,
            "2026-09-06",
            8.0,
            Some("食品"),
            Some("零食"),
            None,
        );
        assert!(delete_subcategory("食品", "饮料").0);
        assert_eq!(
            get_subcategories("食品"),
            vec!["零食".to_string()],
            "同分类下别的子分类要留着"
        );
        let kept: Option<String> = {
            let conn = open().unwrap();
            conn.query_row(
                "SELECT subcategory FROM expenses WHERE id=?1",
                [other],
                |r| r.get::<_, Option<String>>(0),
            )
            .unwrap()
        };
        assert_eq!(kept.as_deref(), Some("零食"), "没删的子分类不能被顺手清空");
    }

    /// 月份写法必须归一（`normalize_month`），否则月度汇总是**前缀**匹配出的另一个窗口。
    #[test]
    fn normalize_month_pads_and_rejects() {
        let m = |s: &str| super::normalize_month(s);
        assert_eq!(m("2026-1").as_deref(), Some("2026-01"), "人手写的一月");
        assert_eq!(m("2026-09").as_deref(), Some("2026-09"), "已经补零的不变");
        assert_eq!(m("2026/9").as_deref(), Some("2026-09"));
        assert_eq!(m("202609").as_deref(), Some("2026-09"), "紧凑六位也认");
        assert_eq!(m(" 2026-12 ").as_deref(), Some("2026-12"), "首尾空白不算错");
        assert_eq!(m(""), None, "空串会变成前缀 % = 全时段，必须拒");
        assert_eq!(m("2026-%"), None, "LIKE 通配符不能进前缀");
        assert_eq!(m("2026-13"), None, "没有 13 月");
        assert_eq!(m("2026-09-27"), None, "这不是一个月");
        assert_eq!(m("昨天"), None, "认不出来必须报出来");
    }

    /// 回归（第 23 轮）：`year_month` 以前原样拼成 `LIKE 'x%'`。
    /// `"2026-1"` ⇒ 命中 2026-10/11/12 三个月、真正的一月一条不算；
    /// `""` ⇒ 前缀 `%` ⇒ 全时段总额被当成一个月报出去（Lua 表头还照原样印那个空 ym）。
    #[test]
    fn monthly_summary_only_counts_the_month_it_was_asked_about() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("acc_ym");
        init_db().expect("建库失败");
        for (d, amt) in [
            ("2026-01-15", 10.0),
            ("2026-10-05", 20.0),
            ("2026-11-05", 40.0),
        ] {
            assert!(
                add_expense("支出", "x", None, d, amt, None, None, None) > 0,
                "{d} 没写进去"
            );
        }
        assert_eq!(monthly_summary("2026-10").0, 20.0, "对照腿：整月照旧");
        assert_eq!(monthly_summary("2026-11").0, 40.0);
        assert_eq!(
            monthly_summary("2026-1").0,
            10.0,
            "「2026-1」必须归一成 2026-01：既不能吞下十月/十一月，也不能丢掉一月"
        );
        assert_eq!(
            monthly_summary("").0,
            0.0,
            "空月份不能把全时段总额当成一个月"
        );
        assert_eq!(monthly_summary("2026-%").0, 0.0, "通配符不能当月份");
        assert_eq!(
            monthly_summary_detail("2026-1").2,
            1,
            "明细面的条数必须与总额同一把尺子"
        );
    }

    /// 回归（第 23 轮）：写入侧以前既不规范化 `purchase_date`，也不校验 `type`。
    /// 一条 `"2026-9-1"` 在列表里永远可见、却不在**任何**月度合计里（读侧全是字符串
    /// 前缀/区间比较）；`type="expense"` 在支出与收入两侧的字面量比较下都算 0，
    /// 对用户的表现就是"记录在、钱对不上"，一句错都不报。
    #[test]
    fn write_side_pads_dates_and_rejects_unknown_type() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("acc_write");
        init_db().expect("建库失败");
        let id = add_expense("支出", "早餐", None, "2026-9-1", 12.0, None, None, None);
        assert!(id > 0, "合法日期（只是没补零）不能被拒");
        let stored: String = {
            let conn = open().unwrap();
            conn.query_row(
                "SELECT purchase_date FROM expenses WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(stored, "2026-09-01", "写入侧要把日期补成库里的形态");
        assert_eq!(
            monthly_summary("2026-09").0,
            12.0,
            "补零之后这笔必须在九月合计里"
        );

        // 类型白名单：非 支出/收入 一律拒写，且不能留下半条记录
        assert_eq!(
            add_expense("expense", "笔误", None, "2026-09-02", 5.0, None, None, None),
            -1,
            "type 只能是 支出/收入"
        );
        assert_eq!(
            add_expense("", "空类型", None, "2026-09-02", 5.0, None, None, None),
            -1
        );
        // 认不出来的日期拒，而不是存进去变成合计里的幽灵
        assert_eq!(
            add_expense("支出", "坏日期", None, "昨天", 5.0, None, None, None),
            -1
        );
        let rows: i64 = {
            let conn = open().unwrap();
            conn.query_row("SELECT COUNT(*) FROM expenses", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(rows, 1, "被拒的三条一条都不能留在库里，实得 {rows}");
        // 反向腿（第 63 轮改口）：空日期现在也**拒**。原先放行是因为面板提交前
        // 自己会填今天 —— 那是面板的私有知识；宿主收下空值就等于允许任何调用方
        // 写出一行"对月度合计与日期区间隐身、却照常进分类盈亏"的账。
        assert_eq!(
            add_expense("收入", "没日期", None, "", 5.0, None, None, None),
            -1,
            "空日期不是一个合法日期"
        );
        assert!(add_expense("支出", "斜杠日期", None, "2026/9/3", 3.0, None, None, None) > 0);
        assert_eq!(monthly_summary("2026-09").0, 15.0, "斜杠写法也要落进九月");
    }

    /// 分类的 `ctype` 原先收任意字符串：既不报错也不生效，唯一的症状是面板上那个
    /// 分类永远显示「双向」（插件的标签兜底臂把不认识的值念成双向）。
    /// 合法域是 `expense/income/both` —— **不是** [`EXPENSE_TYPES`] 那两串中文，
    /// 那是流水的收支；拿它当分类白名单会把面板的三个选项与预置分类一起拒掉。
    #[test]
    fn category_type_is_whitelisted_at_every_write_door() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("acc_ctype");
        init_db().expect("建库失败");

        // 先钉合法域本身：预置分类必须全部过闸，否则这道闸会把默认值自己拒掉
        for (name, ctype, _) in DEFAULT_CATEGORIES {
            assert!(
                CATEGORY_TYPES.contains(ctype),
                "预置分类 [{name}] 的类型「{ctype}」不在白名单里"
            );
        }

        assert!(add_category("食品", "expense", &[]) > 0);
        assert_eq!(
            add_category("交通", "支出", &[]),
            -1,
            "流水那两串中文不是分类类型"
        );
        assert_eq!(
            add_category("交通", "typo", &[]),
            -1,
            "打错的值不能默默落库"
        );
        assert!(
            get_all_categories().0.iter().all(|c| c.name != "交通"),
            "被拒的分类一条都不能留在库里"
        );
        assert!(add_category("交通", "both", &[]) > 0, "换个合法值照常建");

        // 三个写入口都要挡，且被拒的那次不能改到原值
        assert!(!update_category_type("食品", "支出"));
        assert_eq!(
            category_type("食品").as_deref(),
            Some("expense"),
            "被拒的那次不该已经把库改了"
        );
        assert!(update_category_type("食品", "income"));
        assert_eq!(category_type("食品").as_deref(), Some("income"));
        let (ok, msg) = update_category("交通", "交通", Some("双向"));
        assert!(!ok, "中文名不是合法分类类型: {msg}");
        assert_eq!(
            category_type("交通").as_deref(),
            Some("both"),
            "被拒的那次不该已经把库改了"
        );
        // 正向腿：带空格但合法的取值照常收（与分类名同口径的 trim）
        let (ok, msg) = update_category("交通", "交通", Some(" expense "));
        assert!(ok, "trim 之后合法就该收: {msg}");
        assert_eq!(category_type("交通").as_deref(), Some("expense"));
    }

    /// 同一列只该有一把日期尺子。
    ///
    /// 回归两半：
    /// 1. `days_ago` 原先用 `%Y-%m-%d` 解析 `purchase_date`，而这一列声明的尺子是
    ///    [`normalize_date_filter`]。实测前者**能**解没补零的 `2026-9-1`（chrono 数字
    ///    位宽弹性），却只认破折号分隔 ⇒ `2026/9/1`、`20260901` 这类行在"距今多久"里隐身。
    /// 2. 更要紧的是这一列**所有**读侧都是字符串比较（`LIKE 'YYYY-MM%'`、`>=`/`<=`、
    ///    `ORDER BY`）⇒ 没补零的行在月度合计与日期区间里隐身，却照常进分类盈亏
    ///    （那条 SQL 不看日期）—— 同一个数两本账。修法：读侧换成同一把尺子，
    ///    并由 `init_db` 把历史行补齐成补零形态。
    #[test]
    fn one_date_ruler_for_the_column_days_ago_and_the_backfill() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("acc_ruler");
        init_db().expect("建库失败");

        // 绕过写入侧的规范化，造出旧库里那种两种形态（写入侧从 2026-09-27 起才规范化）
        let (unpadded, slashed) = {
            let conn = open().unwrap();
            conn.execute(
                "INSERT INTO expenses (type, item_name, purchase_date, amount, record_time)
                 VALUES ('支出', '没补零', '2026-9-1', 12.0, '2026-09-01 08:00:00')",
                [],
            )
            .expect("造一条没补零的历史行");
            let a = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO expenses (type, item_name, purchase_date, amount, record_time)
                 VALUES ('收入', '斜杠写法', '2026/9/2', 8.0, '2026-09-02 08:00:00')",
                [],
            )
            .expect("造一条斜杠写法的历史行");
            (a, conn.last_insert_rowid())
        };
        let stored = |id: i64| -> String {
            let conn = open().unwrap();
            conn.query_row(
                "SELECT purchase_date FROM expenses WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            stored(unpadded),
            "2026-9-1",
            "前提：这一行真是没补零的旧形态"
        );
        assert_eq!(stored(slashed), "2026/9/2", "前提：这一行真是斜杠写法");

        // 第一腿（读侧）：补齐之前这两行就该在「距今多久」里看得见
        let seen: Vec<i64> = days_ago(&[unpadded, slashed])
            .into_iter()
            .map(|(id, _, _)| id)
            .collect();
        assert_eq!(
            seen,
            vec![unpadded, slashed],
            "没补零与斜杠写法都不该在「距今多久」里隐身"
        );

        // 第二腿（症状）：这两行此刻对月度合计完全隐身（分类盈亏却照常算它们）
        assert_eq!(
            monthly_summary("2026-09").0,
            0.0,
            "前提：补齐之前这两笔不在九月合计里 —— 这就是「两本账」少掉的那一本"
        );

        // 第三腿（补齐）：再走一次 init_db 就该写成这一列的尺子
        init_db().expect("init_db 要幂等");
        assert_eq!(stored(unpadded), "2026-09-01");
        assert_eq!(stored(slashed), "2026-09-02");
        assert_eq!(
            monthly_summary("2026-09").0,
            12.0,
            "补齐之后没补零那笔要落进九月合计（斜杠那笔是收入，不进支出侧）"
        );
        assert_eq!(monthly_summary("2026-09").1, 8.0, "收入侧同理");

        // 认不出来的值原样留着：补齐格式不是清洗数据，不替用户猜一个日期
        {
            let conn = open().unwrap();
            conn.execute(
                "INSERT INTO expenses (type, item_name, purchase_date, amount, record_time)
                 VALUES ('支出', '垃圾行', '昨天', 9.0, '2026-09-01 08:00:00')",
                [],
            )
            .expect("造一条认不出的行");
        }
        init_db().expect("第三次 init_db 仍要幂等");
        let junk: String = {
            let conn = open().unwrap();
            conn.query_row(
                "SELECT purchase_date FROM expenses WHERE item_name='垃圾行'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(junk, "昨天", "认不出的值不能被改掉");
    }

    /// 补格式那一步写不进去时，不许把整次初始化判成失败。
    ///
    /// 记账 API 每次调用都惰性走 `init_db`，而 `host.rs` 那个闩只在成功时落下：
    /// 一条 UPDATE 写不动（盘满、库被别的连接长期握着）就让闩永不落下，
    /// 之后每一次 `accounting_add` 都回「记账库没打开或写入失败」——
    /// 而那个库其实开得好好的、数据也完整，少的只是"某些旧行能被月度合计查到"。
    #[test]
    fn a_stuck_date_backfill_does_not_fail_the_whole_init() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("acc_backfill_stuck");
        init_db().expect("建库失败");
        {
            let conn = open().unwrap();
            conn.execute(
                "INSERT INTO expenses (type, item_name, purchase_date, amount, record_time)
                 VALUES ('支出', '没补零', '2026-9-1', 12.0, '2026-09-01 08:00:00')",
                [],
            )
            .unwrap();
            // 读还是好的，只有 UPDATE 被挡 —— 这是"写不动"最稳定的无头形态
            conn.execute_batch(
                "CREATE TRIGGER block_expense_update BEFORE UPDATE ON expenses
                 BEGIN SELECT RAISE(ABORT, '夹具：写被挡'); END;",
            )
            .unwrap();
        }

        init_db().expect("补齐写不动不能让整次初始化失败");

        let still = {
            let conn = open().unwrap();
            conn.query_row(
                "SELECT purchase_date FROM expenses WHERE item_name='没补零'",
                [],
                |r| r.get::<_, String>(0),
            )
            .unwrap()
        };
        assert_eq!(
            still, "2026-9-1",
            "没写成的那一行要原样留着，不能被清空、也不能替用户猜一个日期"
        );
    }

    /// 读侧点名要哪些列 —— 列序不该决定"哪一个值是金额"。
    ///
    /// 旧版（Python 时代）整份导入的账本里 `expenses` 可以是同一批列名、不同列序，
    /// 而 `init_db` 对 `expenses` 只做 `CREATE TABLE IF NOT EXISTS`（老表原样留着）。
    /// `SELECT *` + 按位置读的那种库上会出两种错：类型对不上的整行被
    /// `rows.flatten()` 静默丢掉（分页那头条数取 COUNT(*) ⇒「共 N 条」与实际列出的
    /// 行数不一致，零日志），类型碰巧能读的则变成一条数字与文字互换的记录。
    #[test]
    fn expenses_are_read_by_column_name_not_by_position() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("acc_col_order");
        {
            let conn = open().unwrap();
            conn.execute_batch(
                "CREATE TABLE expenses (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    item_name TEXT NOT NULL,
                    store TEXT,
                    type TEXT NOT NULL DEFAULT '支出',
                    purchase_date TEXT NOT NULL,
                    category TEXT,
                    subcategory TEXT,
                    delivery_date TEXT,
                    amount REAL NOT NULL,
                    note TEXT,
                    record_time TEXT NOT NULL
                 );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO expenses (item_name, store, type, purchase_date, amount, record_time)
                 VALUES ('书', '旧书店', '支出', '2026-09-05', 12.5, '2026-09-05 10:00:00')",
                [],
            )
            .unwrap();
        }

        let (rows, total) = get_expenses_page(1, 20, None, None, None, None, None);
        assert_eq!(total, 1, "COUNT(*) 看得见这一行");
        assert_eq!(
            rows.len(),
            1,
            "列表里也得有这一行 —— 旧写法按位置读，这一行会整行消失而条数照旧"
        );
        assert_eq!(rows[0].item_name, "书");
        assert_eq!(
            rows[0].amount, 12.5,
            "金额要来自 amount 那一列，不是按位置撞上的某一列"
        );
        assert_eq!(rows[0].purchase_date, "2026-09-05");
        assert_eq!(rows[0].rtype, "支出");

        // 同一个尺子也要管住另外三条读路（一处修好、三处照旧是本仓最常见的错）
        let all = get_all_expenses(50);
        assert_eq!(all.len(), 1, "「全部记录」那条路同样不许按位置读");
        assert_eq!(all[0].amount, 12.5);
        let by_date = get_expenses_by_date("2026-09-05");
        assert_eq!(by_date.len(), 1, "「按日期筛选」那条路同理");
        assert_eq!(by_date[0].store.as_deref(), Some("旧书店"));
        let one = get_expense_by_id(all[0].id);
        assert!(one.is_some(), "「按 id 取一条」同理");
        assert_eq!(one.map(|e| e.amount), Some(12.5));
    }

    /// 建一张"上一轮只补完 subs 列、旧表还没并掉"的库（`ALTER ADD COLUMN` 自带提交，
    /// 这就是合并中途失败后留下的形状）。
    fn build_half_done_legacy_db() {
        let conn = open().unwrap();
        conn.execute_batch(
            "CREATE TABLE categories (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                type TEXT NOT NULL DEFAULT 'both',
                subs TEXT NOT NULL DEFAULT '[]'
             );
             CREATE TABLE subcategories (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                category_id INTEGER NOT NULL,
                sort_order INTEGER DEFAULT 0
             );
             INSERT INTO categories (name, type) VALUES ('游戏','both');
             INSERT INTO subcategories (name, category_id, sort_order) VALUES ('梦幻西游',1,0);
             INSERT INTO subcategories (name, category_id, sort_order) VALUES ('充值',1,1);",
        )
        .unwrap();
    }

    fn legacy_table_still_there() -> bool {
        let conn = open().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='subcategories'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            > 0
    }

    /// 旧子分类的合并要盯住"旧表还在不在"，不能盯"这一列是不是刚补的"。
    ///
    /// 旧写法把合并整段套在 `if !has_subs {…}` 里：`ALTER TABLE … ADD COLUMN` 自带提交，
    /// 于是补完列之后任何一步失败（合并查询、UPDATE 写不动、DROP 被握着），
    /// 下一次进来 `has_subs` 已经是真 ⇒ 整段合并**从此不再执行**。
    /// 表现是 Python 时代的子分类永远进不了下拉，而旧表还躺在库里，日志只有一行报错。
    #[test]
    fn a_half_done_legacy_migration_is_retried_instead_of_abandoned() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("acc_mig_retry");
        build_half_done_legacy_db();

        init_db().expect("半截迁移不能让初始化失败");

        let game = get_all_categories()
            .0
            .into_iter()
            .find(|c| c.name == "游戏")
            .expect("分类还在");
        assert_eq!(
            game.subs,
            vec!["梦幻西游".to_string(), "充值".to_string()],
            "上一轮没并完的旧子分类这一轮要补上（并按当年的 sort_order 排）"
        );
        assert!(
            !legacy_table_still_there(),
            "并成功之后旧表要收掉，否则每次启动都重并一次"
        );
    }

    /// 「有没有 subs 这一列」要按**列名**问，不能在建表 SQL 的文本里数子串。
    ///
    /// 旧判据是 `sqlite_master.sql` 里 `contains("subs")`。一份没有这一列、但那句话里带着
    /// "subs" 字样的旧库就被判成"已经有了"⇒ 不补列；之后每一次
    /// `INSERT INTO categories (name, type, subs)` 与 `UPDATE categories SET subs=`
    /// 都撞 "no such column: subs" —— 分类与子分类整个用不了，而 `init_db` 回的是成功、
    /// 日志零字。旧库那一列当年叫什么我们无法保证（`subsidy`、`subscribe` 这类都会命中），
    /// 所以这不是假想输入。
    #[test]
    fn a_schema_text_lookalike_still_gets_the_subs_column() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("acc_subs_ruler");
        {
            let conn = open().unwrap();
            conn.execute_batch(
                "CREATE TABLE categories (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT NOT NULL UNIQUE,
                    type TEXT NOT NULL DEFAULT 'both',
                    subsidy TEXT NOT NULL DEFAULT ''
                 );
                 INSERT INTO categories (name, type) VALUES ('游戏', 'both');",
            )
            .unwrap();
        }

        init_db().expect("补列这一步不该让初始化失败");

        // 列真的被补上了（数子串那把尺子在这一刻还以为已经有了）
        assert!(
            matches!(
                crate::db::connection::column_presence(&open().unwrap(), "categories", "subs"),
                crate::db::connection::Presence::Yes
            ),
            "subs 这一列必须补上：旧库里那格叫 subsidy，不算它"
        );
        // 分类这条路要真的能用（旧写法在这里撞 no such column: subs）
        let (ok, why) = add_subcategory("游戏", "梦幻西游");
        assert!(ok, "写子分类必须成功，实得: {why}");
        assert_eq!(get_subcategories("游戏"), vec!["梦幻西游".to_string()]);
    }

    /// 合并没做成时：旧表必须留着等下一次，而 `init_db` 不许因此失败。
    ///
    /// 旧写法一条 `?` 抛出去 ⇒ 记账整个模块起不来（`host.rs` 那个闩只在成功时落下），
    /// 而重试时又因为 `has_subs` 已为真而不再合并 —— 一次写不进去就把数据永久锁在旧表里。
    #[test]
    fn a_failed_legacy_merge_keeps_the_old_table_and_the_module_usable() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("acc_mig_stuck");
        build_half_done_legacy_db();
        {
            let conn = open().unwrap();
            conn.execute_batch(
                "CREATE TRIGGER block_cat_update BEFORE UPDATE ON categories
                 BEGIN SELECT RAISE(ABORT, '夹具：写被挡'); END;",
            )
            .unwrap();
        }

        init_db().expect("合并写不进去不能让整次初始化失败");
        assert!(
            legacy_table_still_there(),
            "没并成就不能把旧表删掉 —— 它是那批子分类唯一的副本"
        );

        // 障碍撤掉之后，下一次启动真的能把它并完（这条才是"重试"而不是"少失败一次"）
        {
            let conn = open().unwrap();
            conn.execute("DROP TRIGGER block_cat_update", []).unwrap();
        }
        init_db().expect("障碍移开后要能继续");
        let game = get_all_categories()
            .0
            .into_iter()
            .find(|c| c.name == "游戏")
            .expect("分类还在");
        assert_eq!(
            game.subs.len(),
            2,
            "重试那一次要把两份旧子分类都并进来：{:?}",
            game.subs
        );
        assert!(!legacy_table_still_there(), "并完了就该收掉旧表");
    }

    /// `add_category` 交回的 id 必须是"这一次插进去的那一条"的 id，传来的子分类要落库。
    #[test]
    fn add_category_returns_the_id_of_the_row_it_inserted() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("acc_cat_id");
        init_db().expect("建库失败");
        let id = add_category("交通", "expense", &["地铁".to_string(), "打车".to_string()]);
        assert!(id > 0, "正常添加要回一个正 id");
        let stored: i64 = {
            let conn = open().unwrap();
            conn.query_row("SELECT id FROM categories WHERE name='交通'", [], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert_eq!(id, stored, "交回的 id 就是这一条的 id");
        let got = get_all_categories()
            .0
            .into_iter()
            .find(|c| c.name == "交通")
            .expect("分类在库里");
        assert_eq!(
            got.subs,
            vec!["地铁".to_string(), "打车".to_string()],
            "这次传的子分类要真的落库"
        );
    }

    /// 接线腿：查重与插入必须共用一个连接，"插没插进去"要由这一次插入的结果回答。
    ///
    /// 三条形状各对一个真实后果：另开一次连接的查重在库被握着时回 `None`
    /// （＝"没有这个分类"），于是 `INSERT OR IGNORE` 被 UNIQUE 挡下、再按名字回读
    /// 拿到已存在那条的 id ⇒ 面板说「已添加」而什么都没写；打开库失败被折成 -1
    /// 时面板那句「详见日志」指向一条不存在的记录。
    #[test]
    fn add_category_uses_one_connection_and_the_insert_answer() {
        let src = include_str!("accounting.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let at = prod
            .find("pub fn add_category(")
            .expect("添加分类函数找不到了");
        let rest = &prod[at..];
        let end = rest.find("\n}\n").expect("函数切不出来");
        // 注释里写着被替换掉的旧调用名，比原文会被自己的说明文字喂绿
        let code: String = rest[..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        // 四条腿攒齐了一次断：头一条先 panic 就把后面三条的敏感度藏起来了
        let mut bad: Vec<String> = Vec::new();
        if code.contains("category_type(name)") {
            bad.push(
                "查重又开了第二个连接：读不动时会被当成「没有这个分类」继续往下写".to_string(),
            );
        }
        if code.contains("Err(_) => return -1") {
            bad.push(
                "打开库失败被静默折成 -1：面板那句「详见日志」会指向一条不存在的记录".to_string(),
            );
        }
        if code.contains("INSERT OR IGNORE") {
            bad.push("又用 OR IGNORE 把冲突吞成成功".to_string());
        }
        if !code.contains("last_insert_rowid()") {
            bad.push("id 改为按这一次插入的结果给，不再事后回读猜".to_string());
        }
        assert!(bad.is_empty(), "{bad:?}");
    }

    /// 删除一条记录有三种结局，不许折成一个 bool 还不留日志。
    ///
    /// 旧写法是一条 `open().and_then(execute).map(n| n>0).unwrap_or(false)`：
    /// 库打不开、DELETE 被挡、那一行本来就不在，全都是同一个 false，而整条链上
    /// 一行日志都没有 —— 面板那句「有 N 条没删成」既查不到原因，也没法告诉用户
    /// 该刷新一下（那条其实已经没了）还是该去看磁盘（那条还站着）。
    #[test]
    fn a_delete_tells_gone_apart_from_refused() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("acc_delete_kinds");
        init_db().expect("建库失败");
        let id = add_expense("支出", "一笔", None, "2026-09-05", 3.0, None, None, None);
        assert!(id > 0, "夹具：先建一条");
        assert!(delete_expense(id), "删得掉时要回 true");
        assert!(
            matches!(try_delete_expense(id), Ok(false)),
            "已经不在的那一条是「执行成功但没删到」，不是错误"
        );

        {
            let conn = open().unwrap();
            conn.execute_batch(
                "CREATE TRIGGER block_expense_delete BEFORE DELETE ON expenses
                 BEGIN SELECT RAISE(ABORT, '夹具：删不动'); END;",
            )
            .unwrap();
        }
        let id2 = add_expense("支出", "第二笔", None, "2026-09-06", 4.0, None, None, None);
        match try_delete_expense(id2) {
            Err(e) => assert!(e.contains("DELETE 失败"), "要说清是哪一步没做成: {e}"),
            Ok(v) => panic!("删不动不许被说成「这一条本来就不在」: Ok({v})"),
        }
        assert!(
            !delete_expense(id2),
            "对外口径仍是 false（宿主的 bool 契约不变）"
        );
        let left: i64 = {
            let conn = open().unwrap();
            conn.query_row("SELECT COUNT(*) FROM expenses WHERE id=?1", [id2], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert_eq!(
            left, 1,
            "写被挡下时那一行必须还在 —— 这正是它必须与「已经没了」分开的原因"
        );
    }

    /// 接线腿：写入口不许把"库打不开"静默折成哨兵值。
    ///
    /// 面板与宿主对 -1/false 说的话都指向"写入失败"，可旧代码在这一支一个字都不留，
    /// 于是那句提示成了查不到的死路（同 `add_category` 那处，一起钉住这一类）。
    #[test]
    fn write_doors_say_why_the_database_could_not_be_opened() {
        let src = include_str!("accounting.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let code: String = prod
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for fold in ["Err(_) => return false", "Err(_) => return -1"] {
            assert!(
                !code.contains(fold),
                "又有写入口把「库打不开」静默折成 {fold}：面板的提示会指向一条不存在的日志"
            );
        }
    }

    /// 分类清单没读全时要交出**原因**并留下日志，不许看着像"用户把分类删光了"。
    ///
    /// 这一头少一个值就等于界面多说一句假话：`accounting_plugin.lua` 的
    /// `reset_stale_filters` 拿这份清单判「用户筛的那个分类还在不在」。原先三个失败臂
    /// （库打不开 / 语句准备不了 / 某一行整行读不出来）全部折成 `Vec::new()` 且零日志，
    /// 于是面板会把用户的筛选条件与弹窗草稿清零，看起来像用户自己删的。
    #[test]
    fn an_incomplete_category_list_says_so_instead_of_looking_empty() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("acc_cats_partial");
        init_db().expect("建库失败");
        assert!(add_category("收藏", "both", &["充值".to_string()]) > 0);
        assert!(add_category("交通", "expense", &[]) > 0);

        // 读全的那一轮：原因必须是空串 —— 界面只在这一轮才允许校正筛选条件
        let ((cats, gap), logs) = crate::logger::capture_logs(get_all_categories);
        assert!(gap.is_empty(), "读全了不该带原因: {gap}");
        assert!(
            cats.iter().any(|c| c.name == "收藏") && cats.iter().any(|c| c.name == "交通"),
            "夹具应读到这两个分类: {cats:?}"
        );
        assert!(
            !logs.iter().any(|l| l.contains("读取分类清单")),
            "读全了不该出声: {logs:?}"
        );

        // 第一档：某一行整行读不出来（subs 换成非文本值）。
        // 读得出来的那一行照常交出，但这份清单从此不可当完整答案用。
        {
            let conn = open().unwrap();
            conn.execute(
                "UPDATE categories SET subs=?1 WHERE name='收藏'",
                [rusqlite::types::Value::Blob("充值".as_bytes().to_vec())],
            )
            .unwrap();
        }
        let ((cats, gap), logs) = crate::logger::capture_logs(get_all_categories);
        assert!(
            cats.iter().any(|c| c.name == "交通"),
            "读得出来的那一行仍要交出: {cats:?}"
        );
        assert!(
            !cats.iter().any(|c| c.name == "收藏"),
            "读不出来那一行不该凭空出现: {cats:?}"
        );
        assert!(gap.contains("1 行"), "要说清少了几行: {gap}");
        assert!(
            logs.iter()
                .any(|l| l.contains("读取分类清单") && l.contains("行分类整行读不出来")),
            "读不全必须留下日志: {logs:?}"
        );

        // 第二档：整个文件不是数据库（移动盘上的半截文件、被同步盘换成的占位内容）。
        // 这一档连一行都没有，原因照样要交出，且不许与"一个分类都没有"同形。
        std::fs::write(db_path(), "这不是一个 SQLite 文件").unwrap();
        let ((cats, gap), logs) = crate::logger::capture_logs(get_all_categories);
        assert!(cats.is_empty(), "读不动时不该凭空给出分类: {cats:?}");
        assert!(!gap.is_empty(), "库读不动时原因不能是空串");
        assert!(
            logs.iter().any(|l| l.contains("读取分类清单失败")),
            "库读不动必须留下日志: {logs:?}"
        );
    }

    /// 三个汇总读数口都不许把「这次读不动」折成一份格式完整的零报告。
    ///
    /// 症状现量：`open()` 失败时月度汇总交回 `0.0 / 0.0 / 0 / []`，面板接着印
    /// 「总支出 0.00 / 记录数 0 /（本月暂无分类明细）」；分类盈亏与细分盈亏同理，
    /// 交回空 `Vec` 后写「总投入 0.00 总赚取 0.00」加一句「（暂无数据）」。
    /// 那两句都是关于用户账本的**结论**，而这一次什么都没读到 —— 隔壁列表同一帧还写着「共 N 条」。
    #[test]
    fn a_summary_that_could_not_be_read_says_so_instead_of_printing_zero() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("acc_summary_gap");
        init_db().expect("建库失败");
        assert!(add_category("餐饮", "expense", &[]) > 0);
        assert!(
            add_expense(
                "支出",
                "午饭",
                None,
                "2026-08-01",
                12.5,
                Some("餐饮"),
                None,
                None
            ) > 0,
            "夹具：当月记一笔"
        );

        // 第一档：读全。三个口的原因都必须是空串，数照常给。
        let (exp, inc, cnt, cats, gap) = monthly_summary_detail("2026-08");
        assert!(gap.is_empty(), "读全了不该带原因: {gap}");
        assert_eq!((exp, inc, cnt), (12.5, 0.0, 1), "{exp} {inc} {cnt}");
        assert_eq!(cats.len(), 1, "夹具: {cats:?}");
        let (rows, gap2) = category_profit_loss();
        assert!(gap2.is_empty() && rows.len() == 1, "{gap2} {rows:?}");
        let (srows, gap3) = subcategory_profit_loss("餐饮");
        assert!(gap3.is_empty(), "读全了不该带原因: {gap3}");
        assert_eq!(
            srows.iter().map(|(n, ..)| n.as_str()).collect::<Vec<_>>(),
            vec!["(未分细类)"],
            "没填子分类的那一笔该并进 (未分细类): {srows:?}"
        );

        // 第二档：某一行的名字列是坏值（`category` 是动态类型列，塞 BLOB 就让它读不出来）。
        // 读得出的合计仍要交出，但这份清单是残缺的 —— 不许被念成「这个分类没了」。
        {
            let conn = open().unwrap();
            conn.execute(
                "UPDATE expenses SET category=?1",
                [rusqlite::types::Value::Blob(vec![1u8, 2, 3])],
            )
            .unwrap();
        }
        let ((exp2, _, _, cats2, gap4), logs) =
            crate::logger::capture_logs(|| monthly_summary_detail("2026-08"));
        assert!(
            gap4.contains("有一行读不出来"),
            "分类明细少一行要说出来: {gap4}"
        );
        assert!(
            cats2.is_empty(),
            "读不出名字的那行不该凭空编一个名字: {cats2:?}"
        );
        assert_eq!(exp2, 12.5, "合计那一读没坏，就照常交出: {exp2}");
        assert!(
            logs.iter()
                .any(|l| l.contains("记账读数（月度汇总）没读全") && l.contains("不是结论")),
            "没读全必须留日志: {logs:?}"
        );
        let (rows2, gap5) = category_profit_loss();
        assert!(gap5.contains("有一行读不出来"), "{gap5}");
        assert!(rows2.is_empty(), "{rows2:?}");

        // 第三档：整个文件不是数据库（半截文件 / 被同步盘换成的占位内容）。
        let _ = std::fs::write(db_path(), "这不是一个 SQLite 文件");
        let (_, _, _, _, gap6) = monthly_summary_detail("2026-08");
        assert!(
            gap6.contains("打不开") || gap6.contains("读不出来"),
            "{gap6}"
        );
        let (_, gap7) = category_profit_loss();
        assert!(!gap7.is_empty(), "库读不动时分类盈亏的原因不能是空串");
        let (_, gap8) = subcategory_profit_loss("餐饮");
        assert!(!gap8.is_empty(), "库读不动时细分盈亏的原因不能是空串");
    }

    /// 读不出来时，不许把整列子分类覆盖成"只剩刚加的那一个"。
    ///
    /// 这三个调用点接下来写的都是 `UPDATE categories SET subs=<整列>` —— 覆盖而不是追加。
    /// 旧写法把 `SELECT subs` 包成 `.unwrap_or_default()`，于是"读不出来"与"这个分类没有
    /// 子分类"是同一个答案：`add_subcategory` 拿到空清单、加上一个、整列写回去、回一句
    /// 「添加成功」—— 用户当年排好的子分类全没了。另外两个（改名/删除）因为后面还有一道
    /// "清单里找不到"的检查，坏的是一个错消息而不是丢数据，这次一并分开。
    #[test]
    fn an_unreadable_subs_column_is_not_treated_as_an_empty_one() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("acc_subs_unreadable");
        init_db().expect("建库失败");
        assert!(
            add_category("收藏", "both", &["充值".to_string(), "道具".to_string()],) > 0,
            "夹具：先建一个带子分类的分类"
        );
        // `subs` 这一列是动态类型的：换成非文本值就能让"读出来"这一步稳定失败，
        // 这就是页面坏了 / 库被握着那一类形态的最小可复现版本
        {
            let conn = open().unwrap();
            conn.execute(
                "UPDATE categories SET subs=?1 WHERE name='收藏'",
                [rusqlite::types::Value::Blob(
                    "充值,道具".as_bytes().to_vec(),
                )],
            )
            .unwrap();
        }

        let (ok, msg) = add_subcategory("收藏", "新手村");
        assert!(!ok, "读不出来时不能报成功：{msg}");
        assert!(msg.contains("失败"), "要说清是这一步读不出来: {msg}");
        let raw: Vec<u8> = {
            let conn = open().unwrap();
            conn.query_row("SELECT subs FROM categories WHERE name='收藏'", [], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert_eq!(
            raw,
            "充值,道具".as_bytes().to_vec(),
            "读失败之后整列被覆盖 = 用户当年的子分类没了"
        );

        // 「分类不存在」与「读不出来」是两件事，得各说各的
        let (ok2, msg2) = add_subcategory("没有这个分类", "随便");
        assert!(
            !ok2 && msg2.contains("不存在"),
            "缺分类不许被说成读不动，反之也不行: {msg2}"
        );
    }

    /// 「条数是真的、明细读不出来」时，返回值不许把条数一起归零。
    ///
    /// 面板按 `total` 写「共 N 条」、还拿它算页码；把 N 变成 0 等于对用户说
    /// "你一条记录都没有"，而记录全在库里 —— 那是比"这一页暂时列不出行"更坏的一句假话。
    /// 这一支的矛盾交给日志说清楚。
    #[test]
    fn a_page_whose_rows_cannot_be_read_keeps_the_count_it_verified() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("acc_page_no_column");
        {
            let conn = open().unwrap();
            // 一张少了 `delivery_date` 的旧表：COUNT 照样能跑，点名 11 列的明细查询准备不过
            conn.execute_batch(
                "CREATE TABLE expenses (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    type TEXT NOT NULL DEFAULT '支出',
                    item_name TEXT NOT NULL,
                    store TEXT,
                    purchase_date TEXT NOT NULL,
                    amount REAL NOT NULL,
                    category TEXT,
                    subcategory TEXT,
                    record_time TEXT NOT NULL,
                    note TEXT
                 );
                 INSERT INTO expenses (item_name, purchase_date, amount, record_time)
                 VALUES ('一笔','2026-09-05',3.0,'2026-09-05 10:00:00');",
            )
            .unwrap();
        }
        let (records, total) = get_expenses_page(1, 20, None, None, None, None, None);
        assert!(
            records.is_empty(),
            "这张表读不出明细（少一列），不该硬凑出行来"
        );
        assert_eq!(
            total, 1,
            "COUNT 查证过的条数不许被归零：那等于对用户说「你一条记录都没有」"
        );
    }

    /// 接线腿：两条读路不许再把错误整个吞掉换成"没有记录"。
    ///
    /// 空列表在面板上渲染成「（暂无数据）」，与"我把记录删光了"长得一样；
    /// 库被握着 / 页坏了 / 临时目录读不动都会走成那句话，旧代码一个字都不留。
    #[test]
    fn the_read_paths_say_why_the_records_could_not_be_read() {
        let src = include_str!("accounting.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        for head in ["pub fn get_all_expenses(", "pub fn get_expenses_page("] {
            let at = prod.find(head).unwrap_or_else(|| panic!("{head} 不见了"));
            let rest = &prod[at..];
            let end = rest.find("\n}\n").expect("函数切不出来");
            let code: String = rest[..end]
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                !code.contains("Err(_)"),
                "{head} 里还有把错误整个吞掉的臂：读不出来与「没有记录」必须是两件事"
            );
        }
    }

    /// 回归（第 23 轮）：`add_category` 撞重名时 `INSERT OR IGNORE` 整条忽略，紧接着
    /// 回读 id 却拿到**已存在那一条**的 id（> 0）⇒ 面板报"分类 [x] 已添加"，而这次传的
    /// `ctype` / `subs` 一个字都没落库。与 `update_category` 的"已存在"同口径。
    #[test]
    fn add_category_does_not_claim_a_duplicate_was_added() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("acc_dup_cat");
        init_db().expect("建库失败");
        assert!(add_category("食品", "expense", &[]) > 0);
        assert_eq!(
            add_category("食品", "income", &["早餐".to_string()]),
            -1,
            "重名不能回一个已存在记录的 id"
        );
        // 原来的那条不能被这次"添加"改掉
        assert_eq!(category_type("食品").as_deref(), Some("expense"));
        assert!(
            get_subcategories("食品").is_empty(),
            "重名那次传的 subs 不该落库"
        );
        // 反向腿：换个名字照常建
        assert!(add_category("交通", "expense", &[]) > 0);
    }
}
