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

    // 迁移：旧库 categories 表没有 subs 列 → 补列
    let has_subs: bool = {
        let mut stmt =
            conn.prepare("SELECT sql FROM sqlite_master WHERE type='table' AND name='categories'")?;
        let sql: String = stmt.query_row([], |r| r.get(0)).unwrap_or_default();
        sql.contains("subs")
    };
    if !has_subs {
        conn.execute(
            "ALTER TABLE categories ADD COLUMN subs TEXT NOT NULL DEFAULT '[]'",
            [],
        )?;
        // 旧版子分类在独立 subcategories 表（name, category_id）：合并进 subs 列
        let has_sub_table: bool = {
            let mut stmt = conn.prepare(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='subcategories'",
            )?;
            stmt.query_row([], |r| r.get::<_, i64>(0)).unwrap_or(0) > 0
        };
        if has_sub_table {
            let merged: Vec<(String, String)> = {
                let mut stmt = conn.prepare(
                    "SELECT c.name AS cat, s.name AS sub
                     FROM subcategories s JOIN categories c ON s.category_id = c.id
                     ORDER BY c.id, s.sort_order, s.id",
                )?;
                let rows =
                    stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
                rows.flatten().collect()
            };
            let mut cat_subs: std::collections::HashMap<String, Vec<String>> =
                std::collections::HashMap::new();
            for (cat, sub) in merged {
                cat_subs.entry(cat).or_default().push(sub);
            }
            for (cat, subs) in cat_subs {
                conn.execute(
                    "UPDATE categories SET subs=?1 WHERE name=?2",
                    rusqlite::params![subs.join(","), cat],
                )?;
            }
            // 迁移完成后移除旧表（数据已并入）
            conn.execute("DROP TABLE IF EXISTS subcategories", [])?;
        }
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
    Ok(())
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
        Err(_) => return -1,
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
            purchase_date,
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
        Err(_) => return false,
    };
    conn.execute(
        "UPDATE expenses SET type=?1, item_name=?2, store=?3, purchase_date=?4,
         amount=?5, category=?6, subcategory=?7, note=?8 WHERE id=?9",
        rusqlite::params![
            e.rtype,
            e.item_name,
            e.store,
            e.purchase_date,
            e.amount,
            blank_to_none(e.category.as_deref()),
            blank_to_none(e.subcategory.as_deref()),
            e.note,
            id
        ],
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

/// 删除记账记录。
pub fn delete_expense(id: i64) -> bool {
    open()
        .and_then(|conn| conn.execute("DELETE FROM expenses WHERE id=?1", [id]))
        .map(|n| n > 0)
        .unwrap_or(false)
}

/// 获取全部记账记录（按 id 降序）。
pub fn get_all_expenses(limit: i64) -> Vec<Expense> {
    let conn = match open() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut stmt = match conn.prepare("SELECT * FROM expenses ORDER BY id DESC LIMIT ?1") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let result = stmt.query_map([limit], row_to_expense);
    match result {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

/// 按日期筛选。
pub fn get_expenses_by_date(date: &str) -> Vec<Expense> {
    let conn = match open() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut stmt =
        match conn.prepare("SELECT * FROM expenses WHERE purchase_date=?1 ORDER BY id DESC") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
    let result = stmt.query_map([date], row_to_expense);
    match result {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => Vec::new(),
    }
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
    let conn = match open() {
        Ok(c) => c,
        Err(_) => return -1,
    };
    let subs_json = subs.join(",");
    let r = conn.execute(
        "INSERT OR IGNORE INTO categories (name, type, subs) VALUES (?1, ?2, ?3)",
        rusqlite::params![name, ctype, subs_json],
    );
    match r {
        Ok(_) => {
            // 返回 id（存在则查询）
            conn.query_row("SELECT id FROM categories WHERE name=?1", [name], |r| {
                r.get(0)
            })
            .unwrap_or(-1)
        }
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
    let conn = match open() {
        Ok(c) => c,
        Err(_) => return false,
    };
    conn.execute(
        "UPDATE categories SET type=?1 WHERE name=?2",
        rusqlite::params![ctype, name],
    )
    .map(|n| n > 0)
    .unwrap_or(false)
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
    let subs_raw: String = conn
        .query_row(
            "SELECT subs FROM categories WHERE name=?1",
            [category],
            |r| r.get(0),
        )
        .unwrap_or_default();
    let mut subs: Vec<String> = parse_subs(&subs_raw);
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
    let subs_raw: String = conn
        .query_row(
            "SELECT subs FROM categories WHERE name=?1",
            [category],
            |r| r.get(0),
        )
        .unwrap_or_default();
    let mut subs: Vec<String> = parse_subs(&subs_raw);
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
    let subs_raw: String = conn
        .query_row(
            "SELECT subs FROM categories WHERE name=?1",
            [category],
            |r| r.get(0),
        )
        .unwrap_or_default();
    let all = parse_subs(&subs_raw);
    let subs: Vec<String> = all
        .iter()
        .filter(|s| s.as_str() != sub_name)
        .cloned()
        .collect();
    if subs.len() == all.len() {
        return (false, format!("子分类 [{sub_name}] 不存在"));
    }
    match conn.execute(
        "UPDATE categories SET subs=?1 WHERE name=?2",
        rusqlite::params![subs.join(","), category],
    ) {
        Ok(n) if n > 0 => (true, "删除成功".into()),
        Ok(_) => (false, format!("分类 [{category}] 不存在")),
        Err(e) => (false, format!("删除失败: {e}")),
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

/// 获取所有分类。
pub fn get_all_categories() -> Vec<Category> {
    let conn = match open() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut stmt = match conn.prepare("SELECT id, name, type, subs FROM categories ORDER BY id") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
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
    match result {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => Vec::new(),
    }
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
    conn.query_row("SELECT * FROM expenses WHERE id=?1", [id], row_to_expense)
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
        Err(_) => return (Vec::new(), 0),
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
    if let Some(f) = date_from.filter(|f| !f.is_empty()) {
        conds.push("purchase_date>=?".to_string());
        params.push(rusqlite::types::Value::Text(f.to_string()));
    }
    if let Some(t) = date_to.filter(|t| !t.is_empty()) {
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
            Err(_) => return (Vec::new(), 0),
        };
        let mut rows = match stmt.query(rusqlite::params_from_iter(params.iter())) {
            Ok(r) => r,
            Err(_) => return (Vec::new(), 0),
        };
        match rows.next() {
            Ok(Some(row)) => row.get(0).unwrap_or(0),
            _ => 0,
        }
    };

    // 按日期倒序（最新在前），同日期按 id 倒序。
    // 注意：占位符必须用位置 ?（不能用 ?1/?2 编号），否则带筛选时 LIMIT 会绑到筛选参数上
    let sql = format!(
        "SELECT * FROM expenses{where_sql} ORDER BY purchase_date DESC, id DESC LIMIT ? OFFSET ?"
    );
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return (Vec::new(), total),
    };
    let mut all_params: Vec<rusqlite::types::Value> = params;
    all_params.push(page_size.into());
    all_params.push(offset.into());
    let result = stmt.query_map(
        rusqlite::params_from_iter(all_params.iter()),
        row_to_expense,
    );
    let records = match result {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => Vec::new(),
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

/// 月度汇总明细：返回 (总支出, 总收入, 条数, [(分类, 净收入)])。
pub fn monthly_summary_detail(year_month: &str) -> (f64, f64, i64, Vec<(String, f64)>) {
    let conn = match open() {
        Ok(c) => c,
        Err(_) => return (0.0, 0.0, 0, Vec::new()),
    };
    let prefix = format!("{year_month}%");
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
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM expenses WHERE purchase_date LIKE ?1",
            [&prefix],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let mut cat_stats: Vec<(String, f64)> = Vec::new();
    // 净额在 SQL 里按分相减、只在最后除一次：分两次查出 inc/exp 再在 Rust 里
    // 相减，等于把浮点漂移又请回来一次
    if let Ok(mut stmt) = conn.prepare(&format!(
        "SELECT COALESCE(NULLIF(category, ''), '(未分类)') AS cat,
                ({inc} - {exp}) / 100.0 AS net
         FROM expenses WHERE purchase_date LIKE ?1 GROUP BY cat
         ORDER BY net DESC",
        inc = sum_cents("CASE WHEN type='收入' THEN amount ELSE 0 END"),
        exp = sum_cents("CASE WHEN type='支出' THEN amount ELSE 0 END"),
    )) {
        if let Ok(mut rows) = stmt.query([&prefix]) {
            while let Ok(Some(row)) = rows.next() {
                if let (Ok(cat), Ok(net)) = (row.get::<_, String>(0), row.get::<_, f64>(1)) {
                    cat_stats.push((cat, net));
                }
            }
        }
    }
    (expense, income, count, cat_stats)
}

/// 分类盈亏统计：返回 [(分类, 投入=支出合计, 赚取=收入合计, 条数)]。
pub fn category_profit_loss() -> Vec<(String, f64, f64, i64)> {
    let conn = match open() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    if let Ok(mut stmt) = conn.prepare(&format!(
        "SELECT COALESCE(NULLIF(category, ''), '(未分类)') AS cat,
                {inv} AS inv,
                {earn} AS earn,
                COUNT(*) AS cnt
         FROM expenses GROUP BY cat ORDER BY inv - earn",
        inv = sum_amount_sql("CASE WHEN type='支出' THEN amount ELSE 0 END"),
        earn = sum_amount_sql("CASE WHEN type='收入' THEN amount ELSE 0 END"),
    )) {
        if let Ok(mut rows) = stmt.query([]) {
            while let Ok(Some(row)) = rows.next() {
                if let (Ok(cat), Ok(inv), Ok(earn), Ok(cnt)) = (
                    row.get::<_, String>(0),
                    row.get::<_, f64>(1),
                    row.get::<_, f64>(2),
                    row.get::<_, i64>(3),
                ) {
                    out.push((cat, inv, earn, cnt));
                }
            }
        }
    }
    out
}

/// 指定分类下子分类盈亏：返回 [(子分类, 投入, 赚取, 条数)]。
pub fn subcategory_profit_loss(category: &str) -> Vec<(String, f64, f64, i64)> {
    let conn = match open() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    if let Ok(mut stmt) = conn.prepare(&format!(
        "SELECT COALESCE(NULLIF(subcategory, ''), '(未分细类)') AS sub,
                {inv} AS inv,
                {earn} AS earn,
                COUNT(*) AS cnt
         FROM expenses WHERE category=?1 GROUP BY sub ORDER BY inv - earn",
        inv = sum_amount_sql("CASE WHEN type='支出' THEN amount ELSE 0 END"),
        earn = sum_amount_sql("CASE WHEN type='收入' THEN amount ELSE 0 END"),
    )) {
        if let Ok(mut rows) = stmt.query([category]) {
            while let Ok(Some(row)) = rows.next() {
                if let (Ok(sub), Ok(inv), Ok(earn), Ok(cnt)) = (
                    row.get::<_, String>(0),
                    row.get::<_, f64>(1),
                    row.get::<_, f64>(2),
                    row.get::<_, i64>(3),
                ) {
                    out.push((sub, inv, earn, cnt));
                }
            }
        }
    }
    out
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
                    if let Ok(date) = chrono::NaiveDate::parse_from_str(&date_str, "%Y-%m-%d") {
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
    let prefix = format!("{year_month}%");
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
        let profit = category_profit_loss();
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
        let (_, _, _, detail) = monthly_summary_detail(&ym);
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
        let sub = subcategory_profit_loss("餐饮");
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
}
