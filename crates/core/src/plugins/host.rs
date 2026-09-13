//! 插件宿主 API：将核心功能注册为 Lua 全局表 `focusflow`。
//!
//! 插件通过 `focusflow.*` 访问：
//! - `focusflow.stats(period)` -> total, keys 表
//! - `focusflow.today_count()` -> 今日计数
//! - `focusflow.now()` -> Unix 秒
//! - `focusflow.log(msg)` -> 日志
//! - `focusflow.daily_counts(days)` -> 每日统计
//! - `focusflow.available_years()` -> 年份列表
//! - `focusflow.config_get("section.key")` -> 配置值

use std::sync::Arc;

use mlua::{Lua, Table};

use crate::accounting;
use crate::config::FocusFlowConfig;
use crate::db;
use crate::edge_history;
use crate::pomodoro::{self, PomodoroTimer};
use crate::scheduler;

/// 注册宿主 API 到 Lua 全局表 `focusflow`，返回该表。
pub fn register_host_api(
    lua: &Lua,
    config: &'static FocusFlowConfig,
    database: Arc<db::Database>,
) -> mlua::Result<Table> {
    let host = lua.create_table()?;

    // 统计查询：period = -1 今日, 0 总计, N 天数。返回 (total, keys表)
    let stats_fn = lua.create_function(move |lua, period: i64| {
        let (total, key_stats) = match period {
            -1 => db::get_stats_by_date(chrono::Local::now().date_naive()),
            0 => db::get_stats(None, None),
            n => db::get_stats(Some(n), None),
        };
        let keys = lua.create_table()?;
        for (k, v) in &key_stats {
            keys.set(k.as_str(), *v)?;
        }
        Ok((total, keys))
    })?;
    host.set("stats", stats_fn)?;

    // 今日计数
    let db_today = Arc::clone(&database);
    let today_fn = lua.create_function(move |_, ()| {
        Ok(db::get_today_count(db_today.writer().map(|w| w.as_ref())))
    })?;
    host.set("today_count", today_fn)?;

    // 当前 Unix 秒
    let now_fn = lua.create_function(|_, ()| Ok(chrono::Utc::now().timestamp()))?;
    host.set("now", now_fn)?;

    // 日志
    let log_fn = lua.create_function(|_, msg: String| {
        tracing::info!("[plugin] {msg}");
        Ok(())
    })?;
    host.set("log", log_fn)?;

    // 每日统计（趋势）
    let daily_fn = lua.create_function(|lua, days: i64| {
        let daily = db::get_daily_counts(days.max(1), None);
        let t = lua.create_table()?;
        for (i, (date, count)) in daily.iter().enumerate() {
            let row = lua.create_table()?;
            row.set("date", date.as_str())?;
            row.set("count", *count)?;
            t.set(i + 1, row)?;
        }
        Ok(t)
    })?;
    host.set("daily_counts", daily_fn)?;

    // 可用年份
    let years_fn = lua.create_function(|_, ()| Ok(db::available_years()))?;
    host.set("available_years", years_fn)?;

    // 配置读取：key = "section.key"
    let cfg_fn = lua.create_function(move |_, key: String| {
        let (section, key) = key.split_once('.').unwrap_or(("", &key));
        let v = config.get(section, key);
        Ok(v)
    })?;
    host.set("config_get", cfg_fn)?;

    // 调试：返回环境信息
    let info_fn = lua.create_function(|_, ()| {
        Ok(format!(
            "FocusFlow {} · {}",
            crate::paths::APP_VERSION,
            crate::paths::app_dir().display()
        ))
    })?;
    host.set("app_info", info_fn)?;

    // ---- 番茄钟 API ----
    // 共享番茄钟实例（进程级单例）
    static POMODORO: std::sync::OnceLock<Arc<PomodoroTimer>> = std::sync::OnceLock::new();
    let pomo = Arc::clone(POMODORO.get_or_init(|| {
        let _ = pomodoro::init_db();
        PomodoroTimer::new()
    }));

    let pomo_state = Arc::clone(&pomo);
    let pomo_state_fn = lua.create_function(move |lua, ()| {
        let info = pomo_state.get_state_info();
        let t = lua.create_table()?;
        for (k, v) in &info {
            t.set(k.as_str(), *v)?;
        }
        Ok(t)
    })?;
    host.set("pomodoro_state", pomo_state_fn)?;

    let pomo_start_work = Arc::clone(&pomo);
    host.set(
        "pomodoro_start_work",
        lua.create_function(move |_, ()| {
            pomo_start_work.start_work();
            Ok(())
        })?,
    )?;

    let pomo_start_break = Arc::clone(&pomo);
    host.set(
        "pomodoro_start_break",
        lua.create_function(move |_, ()| {
            pomo_start_break.start_break();
            Ok(())
        })?,
    )?;

    let pomo_toggle = Arc::clone(&pomo);
    host.set(
        "pomodoro_toggle_pause",
        lua.create_function(move |_, ()| Ok(pomo_toggle.toggle_pause()))?,
    )?;

    let pomo_stop = Arc::clone(&pomo);
    host.set(
        "pomodoro_stop",
        lua.create_function(move |_, ()| {
            pomo_stop.stop();
            Ok(())
        })?,
    )?;

    let pomo_skip = Arc::clone(&pomo);
    host.set(
        "pomodoro_skip",
        lua.create_function(move |_, ()| {
            pomo_skip.skip();
            Ok(())
        })?,
    )?;

    let pomo_durations = Arc::clone(&pomo);
    host.set(
        "pomodoro_set_durations",
        lua.create_function(move |_, (work, brk): (i64, i64)| {
            pomo_durations.set_durations(work, brk);
            Ok(())
        })?,
    )?;

    // 番茄钟历史
    let sessions_fn = lua.create_function(|lua, limit: i64| {
        let sessions = pomodoro::get_recent_sessions(limit.clamp(1, 100));
        let t = lua.create_table()?;
        for (i, s) in sessions.iter().enumerate() {
            let row = lua.create_table()?;
            row.set("id", s.id)?;
            row.set("type", s.rtype.as_str())?;
            row.set("start", s.start_time.as_str())?;
            row.set("end", s.end_time.as_str())?;
            row.set("actual", s.actual_seconds)?;
            row.set("keys", s.key_count)?;
            t.set(i + 1, row)?;
        }
        Ok(t)
    })?;
    host.set("pomodoro_sessions", sessions_fn)?;

    let summary_fn = lua.create_function(|_, ()| {
        let (count, keys, secs) = pomodoro::today_summary();
        Ok((count, keys, secs))
    })?;
    host.set("pomodoro_summary", summary_fn)?;

    // 番茄钟按键联动（监听器回调调用）
    let pomo_record = Arc::clone(&pomo);
    host.set(
        "pomodoro_record_key",
        lua.create_function(move |_, key: String| {
            pomo_record.record_key(&key);
            Ok(())
        })?,
    )?;

    // ---- 定时任务 API ----
    // 启动调度器（进程级单例）
    static SCHEDULER: std::sync::OnceLock<Arc<scheduler::Scheduler>> = std::sync::OnceLock::new();
    let _sched = SCHEDULER.get_or_init(scheduler::Scheduler::start);

    let tasks_fn = lua.create_function(|lua, ()| {
        let tasks = scheduler::get_all_tasks();
        let t = lua.create_table()?;
        for (i, task) in tasks.iter().enumerate() {
            let row = lua.create_table()?;
            row.set("id", task.id)?;
            row.set("name", task.name.as_str())?;
            row.set("target", task.target_path.as_str())?;
            row.set("args", task.args.as_str())?;
            row.set("type", task.schedule_type.as_str())?;
            row.set("time", task.schedule_time.as_str())?;
            row.set("enabled", task.enabled)?;
            row.set("last_run", task.last_run.clone().unwrap_or_default())?;
            row.set(
                "desc",
                scheduler::describe_schedule(&task.schedule_type, &task.schedule_time),
            )?;
            t.set(i + 1, row)?;
        }
        Ok(t)
    })?;
    host.set("scheduler_tasks", tasks_fn)?;

    let add_fn = lua.create_function(
        |_,
         (name, target, args, stype, stime, enabled): (
            String,
            String,
            String,
            String,
            String,
            bool,
        )| {
            Ok(scheduler::add_task(
                &name, &target, &args, &stype, &stime, enabled,
            ))
        },
    )?;
    host.set("scheduler_add", add_fn)?;

    let update_fn = lua.create_function(
        |_,
         (id, name, target, args, stype, stime, enabled): (
            i64,
            String,
            String,
            String,
            String,
            String,
            bool,
        )| {
            let r = scheduler::update_task(
                id,
                Some(&name),
                Some(&target),
                Some(&args),
                Some(&stype),
                Some(&stime),
                Some(enabled),
            );
            Ok(r)
        },
    )?;
    host.set("scheduler_update", update_fn)?;

    let delete_fn = lua.create_function(|_, id: i64| Ok(scheduler::delete_task(id)))?;
    host.set("scheduler_delete", delete_fn)?;

    let toggle_fn = lua.create_function(|_, (id, enabled): (i64, bool)| {
        scheduler::toggle_task(id, enabled);
        Ok(())
    })?;
    host.set("scheduler_toggle", toggle_fn)?;

    let validate_fn = lua.create_function(|_, (stype, stime): (String, String)| {
        let (ok, msg) = scheduler::validate_schedule(&stype, &stime);
        Ok((ok, msg))
    })?;
    host.set("scheduler_validate", validate_fn)?;

    // ---- 记账本 API ----
    let _ = accounting::init_db();

    let acc_add = lua.create_function(
        |_,
         (rtype, item, store, date, amount, cat, sub, note): (
            String,
            String,
            String,
            String,
            f64,
            String,
            String,
            String,
        )| {
            Ok(accounting::add_expense(
                &rtype,
                &item,
                if store.is_empty() {
                    None
                } else {
                    Some(store.as_str())
                },
                &date,
                amount,
                if cat.is_empty() {
                    None
                } else {
                    Some(cat.as_str())
                },
                if sub.is_empty() {
                    None
                } else {
                    Some(sub.as_str())
                },
                if note.is_empty() {
                    None
                } else {
                    Some(note.as_str())
                },
            ))
        },
    )?;
    host.set("accounting_add", acc_add)?;

    // 分页 + 筛选查询：返回 (records, total)
    let acc_query = lua.create_function(
        |lua,
         (page, page_size, cat, sub, kw, date_from, date_to): (
            i64,
            i64,
            String,
            String,
            String,
            String,
            String,
        )| {
            let (records, total) = accounting::get_expenses_page(
                page,
                page_size,
                if cat.is_empty() {
                    None
                } else {
                    Some(cat.as_str())
                },
                if sub.is_empty() {
                    None
                } else {
                    Some(sub.as_str())
                },
                if kw.is_empty() {
                    None
                } else {
                    Some(kw.as_str())
                },
                if date_from.is_empty() {
                    None
                } else {
                    Some(date_from.as_str())
                },
                if date_to.is_empty() {
                    None
                } else {
                    Some(date_to.as_str())
                },
            );
            let t = lua.create_table()?;
            for (i, e) in records.iter().enumerate() {
                let row = lua.create_table()?;
                row.set("id", e.id)?;
                row.set("type", e.rtype.as_str())?;
                row.set("item", e.item_name.as_str())?;
                row.set("store", e.store.clone().unwrap_or_default())?;
                row.set("date", e.purchase_date.as_str())?;
                row.set("amount", e.amount)?;
                row.set("category", e.category.clone().unwrap_or_default())?;
                row.set("subcategory", e.subcategory.clone().unwrap_or_default())?;
                row.set("note", e.note.clone().unwrap_or_default())?;
                t.set(i + 1, row)?;
            }
            Ok((t, total))
        },
    )?;
    host.set("accounting_query", acc_query)?;

    // 分类列表（名字数组）
    let acc_cats = lua.create_function(|lua, ()| {
        let cats = accounting::get_all_categories();
        let t = lua.create_table()?;
        for (i, c) in cats.iter().enumerate() {
            t.set(i + 1, c.name.as_str())?;
        }
        Ok(t)
    })?;
    host.set("accounting_categories", acc_cats)?;

    // 分类管理：添加分类（返回 id，失败 -1）
    let acc_cat_add = lua.create_function(|_, (name, ctype): (String, String)| {
        Ok(accounting::add_category(&name, &ctype, &[]))
    })?;
    host.set("accounting_category_add", acc_cat_add)?;

    // 重命名分类（同步历史记录）：(ok, msg)；第三个参数修改类型，空串表示保持原类型
    let acc_cat_rename =
        lua.create_function(|_, (old_name, new_name, ctype): (String, String, String)| {
            let ctype = if ctype.is_empty() {
                None
            } else {
                Some(ctype.as_str())
            };
            Ok(accounting::update_category(&old_name, &new_name, ctype))
        })?;
    host.set("accounting_category_rename", acc_cat_rename)?;

    // 删除分类：(ok, msg)
    let acc_cat_del =
        lua.create_function(|_, name: String| Ok(accounting::delete_category(&name)))?;
    host.set("accounting_category_delete", acc_cat_del)?;

    // 查询分类类型（expense/income/both），无则空串。
    //
    // 注意：这里曾同时注册过一个「修改分类类型」的同名写接口，被本块覆盖成
    // 不可达死代码 —— 而 Lua 插件按注释把它当读接口用（返回值是类型字符串，
    // 解包成 `(ok, msg)` 会误判成功）。修改类型现由
    // `accounting_category_rename(old, new, ctype)` 的第三个参数承担。
    let acc_cat_type = lua.create_function(|_, name: String| {
        Ok(accounting::category_type(&name).unwrap_or_default())
    })?;
    host.set("accounting_category_type", acc_cat_type)?;

    // 添加子分类：(ok, msg)
    let acc_sub_add = lua.create_function(|_, (cat, sub): (String, String)| {
        Ok(accounting::add_subcategory(&cat, &sub))
    })?;
    host.set("accounting_subcategory_add", acc_sub_add)?;

    // 重命名子分类：(ok, msg)
    let acc_sub_rename =
        lua.create_function(|_, (cat, old_sub, new_sub): (String, String, String)| {
            Ok(accounting::update_subcategory(&cat, &old_sub, &new_sub))
        })?;
    host.set("accounting_subcategory_rename", acc_sub_rename)?;

    // 删除子分类：(ok, msg)
    let acc_sub_del = lua.create_function(|_, (cat, sub): (String, String)| {
        Ok(accounting::delete_subcategory(&cat, &sub))
    })?;
    host.set("accounting_subcategory_delete", acc_sub_del)?;

    // 子分类列表
    let acc_subs = lua.create_function(|lua, cat: String| {
        let subs = accounting::get_subcategories(&cat);
        let t = lua.create_table()?;
        for (i, s) in subs.iter().enumerate() {
            t.set(i + 1, s.as_str())?;
        }
        Ok(t)
    })?;
    host.set("accounting_subcategories", acc_subs)?;

    // 按 id 查询
    let acc_get = lua.create_function(|lua, id: i64| {
        let Some(e) = accounting::get_expense_by_id(id) else {
            return Ok(mlua::Value::Nil);
        };
        let row = lua.create_table()?;
        row.set("id", e.id)?;
        row.set("type", e.rtype.as_str())?;
        row.set("item", e.item_name.as_str())?;
        row.set("store", e.store.clone().unwrap_or_default())?;
        row.set("date", e.purchase_date.as_str())?;
        row.set("amount", e.amount)?;
        row.set("category", e.category.clone().unwrap_or_default())?;
        row.set("subcategory", e.subcategory.clone().unwrap_or_default())?;
        row.set("note", e.note.clone().unwrap_or_default())?;
        Ok(mlua::Value::Table(row))
    })?;
    host.set("accounting_get", acc_get)?;

    // 更新记录（含渠道/子分类/备注）
    let acc_update = lua.create_function(
        |_,
         (id, rtype, item, store, date, amount, cat, sub, note): (
            i64,
            String,
            String,
            String,
            String,
            f64,
            String,
            String,
            String,
        )| {
            let e = accounting::Expense {
                id,
                rtype,
                item_name: item,
                store: if store.is_empty() { None } else { Some(store) },
                purchase_date: date,
                amount,
                category: if cat.is_empty() { None } else { Some(cat) },
                subcategory: if sub.is_empty() { None } else { Some(sub) },
                delivery_date: None,
                record_time: String::new(),
                note: if note.is_empty() { None } else { Some(note) },
            };
            Ok(accounting::update_expense(id, &e))
        },
    )?;
    host.set("accounting_update", acc_update)?;

    // 月度汇总（含分类明细）：返回 (支出, 收入, 条数, [{category, net}])
    let acc_monthly = lua.create_function(|lua, ym: String| {
        let (expense, income, count, cat_stats) = accounting::monthly_summary_detail(&ym);
        let t = lua.create_table()?;
        for (i, (cat, net)) in cat_stats.iter().enumerate() {
            let row = lua.create_table()?;
            row.set("category", cat.as_str())?;
            row.set("net", *net)?;
            t.set(i + 1, row)?;
        }
        Ok((expense, income, count, t))
    })?;
    host.set("accounting_monthly_detail", acc_monthly)?;

    // 分类盈亏：返回 [{category, invested, earned, count}]
    let acc_cat_profit = lua.create_function(|lua, ()| {
        let data = accounting::category_profit_loss();
        let t = lua.create_table()?;
        for (i, (cat, inv, earn, cnt)) in data.iter().enumerate() {
            let row = lua.create_table()?;
            row.set("category", cat.as_str())?;
            row.set("invested", *inv)?;
            row.set("earned", *earn)?;
            row.set("count", *cnt)?;
            t.set(i + 1, row)?;
        }
        Ok(t)
    })?;
    host.set("accounting_category_profit", acc_cat_profit)?;

    // 细分盈亏：返回 [{subcategory, invested, earned, count}]
    let acc_sub_profit = lua.create_function(|lua, cat: String| {
        let data = accounting::subcategory_profit_loss(&cat);
        let t = lua.create_table()?;
        for (i, (sub, inv, earn, cnt)) in data.iter().enumerate() {
            let row = lua.create_table()?;
            row.set("subcategory", sub.as_str())?;
            row.set("invested", *inv)?;
            row.set("earned", *earn)?;
            row.set("count", *cnt)?;
            t.set(i + 1, row)?;
        }
        Ok(t)
    })?;
    host.set("accounting_subcategory_profit", acc_sub_profit)?;

    // 距今多久：入参 id 数组，返回 [{id, years, days}]
    let acc_days_ago = lua.create_function(|lua, ids: Vec<i64>| {
        let data = accounting::days_ago(&ids);
        let t = lua.create_table()?;
        for (i, (id, years, days)) in data.iter().enumerate() {
            let row = lua.create_table()?;
            row.set("id", *id)?;
            row.set("years", *years)?;
            row.set("days", *days)?;
            t.set(i + 1, row)?;
        }
        Ok(t)
    })?;
    host.set("accounting_days_ago", acc_days_ago)?;

    let acc_list = lua.create_function(|lua, limit: i64| {
        // 上限放宽到 10000：记账本分页/查询需要全量记录（Lua 侧过滤 + 分页）
        let expenses = accounting::get_all_expenses(limit.clamp(1, 10000));
        let t = lua.create_table()?;
        for (i, e) in expenses.iter().enumerate() {
            let row = lua.create_table()?;
            row.set("id", e.id)?;
            row.set("type", e.rtype.as_str())?;
            row.set("item", e.item_name.as_str())?;
            row.set("date", e.purchase_date.as_str())?;
            row.set("amount", e.amount)?;
            row.set("category", e.category.clone().unwrap_or_default())?;
            row.set("subcategory", e.subcategory.clone().unwrap_or_default())?;
            t.set(i + 1, row)?;
        }
        Ok(t)
    })?;
    host.set("accounting_list", acc_list)?;

    let acc_delete = lua.create_function(|_, id: i64| Ok(accounting::delete_expense(id)))?;
    host.set("accounting_delete", acc_delete)?;

    let acc_summary = lua.create_function(|_, ym: String| {
        let (expense, income) = accounting::monthly_summary(&ym);
        Ok((expense, income))
    })?;
    host.set("accounting_summary", acc_summary)?;

    // ---- Edge 历史 API ----
    // 返回 (是否成功, 今日数, 总数)；失败时 ok=false，插件据此提示而非显示 0
    let edge_update = lua.create_function(|_, ()| {
        let (ok, today, total) = edge_history::update_today_edge_history();
        Ok((ok, today, total))
    })?;
    host.set("edge_update_today", edge_update)?;

    let edge_counts = lua.create_function(|lua, days: i64| {
        let data = edge_history::get_edge_history_counts(days.clamp(1, 90));
        let t = lua.create_table()?;
        for (i, (date, count)) in data.iter().enumerate() {
            let row = lua.create_table()?;
            row.set("date", date.as_str())?;
            row.set("count", *count)?;
            t.set(i + 1, row)?;
        }
        Ok(t)
    })?;
    host.set("edge_counts", edge_counts)?;

    let edge_today = lua.create_function(|_, ()| {
        let today = chrono::Local::now().date_naive();
        Ok(edge_history::query_edge_history_count(today).unwrap_or(0))
    })?;
    host.set("edge_today_count", edge_today)?;

    let edge_total =
        lua.create_function(|_, ()| Ok(edge_history::query_edge_total_count().unwrap_or(0)))?;
    host.set("edge_total_count", edge_total)?;

    // 本地缓存（上次刷新保存的值），插件重启后恢复显示
    let edge_saved_today =
        lua.create_function(|_, ()| Ok(edge_history::get_edge_history_saved_today()))?;
    host.set("edge_saved_today", edge_saved_today)?;

    let edge_saved_total =
        lua.create_function(|_, ()| Ok(edge_history::get_edge_history_saved_total()))?;
    host.set("edge_saved_total", edge_saved_total)?;

    // 注册为全局 `focusflow`
    lua.globals().set("focusflow", host.clone())?;
    Ok(host)
}
