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

use std::sync::{Arc, Mutex, OnceLock};

use mlua::{Lua, Table};

use crate::accounting;
use crate::config::FocusFlowConfig;
use crate::db;
use crate::edge_history;
use crate::pomodoro::{self, PomodoroTimer};
use crate::scheduler;

// ---- 番茄钟 / 调度器的进程级单例 ----
//
// 用 `Mutex<Option<Arc<_>>>` 而不是 `OnceLock`：停用插件时要能拆掉后台线程，
// 而 OnceLock 占用后永远换不进去，用户重新启用插件时线程不会再起来。
static POMODORO: Mutex<Option<Arc<PomodoroTimer>>> = Mutex::new(None);
static SCHEDULER: Mutex<Option<Arc<scheduler::Scheduler>>> = Mutex::new(None);
static POMODORO_DB: OnceLock<()> = OnceLock::new();

fn pomodoro_timer() -> Arc<PomodoroTimer> {
    let mut slot = POMODORO.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(t) = slot.as_ref() {
        return Arc::clone(t);
    }
    ensure_pomodoro_db();
    let t = PomodoroTimer::new();
    *slot = Some(Arc::clone(&t));
    t
}

/// 只保证库建好，不启动计时线程：读历史/汇总的接口用。
fn ensure_pomodoro_db() {
    POMODORO_DB.get_or_init(|| {
        let _ = pomodoro::init_db();
    });
}

/// 回收番茄钟：落盘进行中的阶段并结束计时线程（重新启用插件会再起）。
///
/// 由番茄钟插件在自己的 `cleanup()` 里调用 —— 谁用资源谁负责释放，
/// 核心层不必知道哪个插件在用。
pub fn shutdown_pomodoro() {
    let timer = POMODORO.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(t) = timer {
        t.shutdown();
    }
}

fn ensure_scheduler() {
    let mut slot = SCHEDULER.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_none() {
        *slot = Some(scheduler::Scheduler::start());
    }
}

/// 回收调度线程：停用日程插件后定时任务不再触发（重新启用会再起）。
pub fn stop_scheduler() {
    let s = SCHEDULER.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(s) = s {
        s.stop();
    }
}

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
    // 共享番茄钟实例（进程级单例，见模块顶部），**惰性初始化**：
    // 只有插件真正调用番茄钟 API 时才建库、才起计时线程。早前这里是
    // 「注册即 init_db + spawn」，于是启用任意插件（哪怕与番茄钟无关）
    // 都会把 focusflow_pomodoro.db 建出来，番茄钟插件的停用开关形同虚设。

    let pomo_state_fn = lua.create_function(|lua, ()| {
        let info = pomodoro_timer().get_state_info();
        let t = lua.create_table()?;
        for (k, v) in &info {
            t.set(k.as_str(), *v)?;
        }
        Ok(t)
    })?;
    host.set("pomodoro_state", pomo_state_fn)?;

    host.set(
        "pomodoro_start_work",
        lua.create_function(|_, ()| {
            pomodoro_timer().start_work();
            Ok(())
        })?,
    )?;

    host.set(
        "pomodoro_start_break",
        lua.create_function(|_, ()| {
            pomodoro_timer().start_break();
            Ok(())
        })?,
    )?;

    host.set(
        "pomodoro_toggle_pause",
        lua.create_function(|_, ()| Ok(pomodoro_timer().toggle_pause()))?,
    )?;

    host.set(
        "pomodoro_stop",
        lua.create_function(|_, ()| {
            pomodoro_timer().stop();
            Ok(())
        })?,
    )?;

    // 与 pomodoro_stop 的区别：stop 只结束当前番茄会话、线程继续跑；
    // 这个是插件停用/卸载时在 cleanup 里回收计时线程用。
    host.set(
        "pomodoro_shutdown",
        lua.create_function(|_, ()| {
            shutdown_pomodoro();
            Ok(())
        })?,
    )?;

    host.set(
        "pomodoro_skip",
        lua.create_function(|_, ()| {
            pomodoro_timer().skip();
            Ok(())
        })?,
    )?;

    host.set(
        "pomodoro_set_durations",
        lua.create_function(|_, (work, brk): (i64, i64)| {
            pomodoro_timer().set_durations(work, brk);
            Ok(())
        })?,
    )?;

    // 番茄钟历史
    let sessions_fn = lua.create_function(|lua, limit: i64| {
        ensure_pomodoro_db();
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
        ensure_pomodoro_db();
        let (count, keys, secs) = pomodoro::today_summary();
        Ok((count, keys, secs))
    })?;
    host.set("pomodoro_summary", summary_fn)?;

    // 番茄钟按键联动（监听器回调调用）
    host.set(
        "pomodoro_record_key",
        lua.create_function(|_, key: String| {
            pomodoro_timer().record_key(&key);
            Ok(())
        })?,
    )?;

    // ---- 定时任务 API ----
    // 调度器（进程级单例，见模块顶部），**惰性初始化**：
    // `Scheduler::start` 会 spawn 一个常驻线程并建库，早前在注册时就执行，
    // 于是日程插件被禁用时线程照跑、focusflow_scheduler.db 照样生成。
    // 现在改成首次调用任一日程 API 时才启动，停用插件时经 scheduler_shutdown 回收。
    //
    // 注意连「列出任务」也要先启动：程序重启后 UI 往往只是渲染任务列表，
    // 若读任务不启动调度器，恢复上来的定时任务就永远不会被执行。

    let tasks_fn = lua.create_function(|lua, ()| {
        ensure_scheduler();
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

    // 添加：返回 (id, 失败原因)。只回 -1 的话，第三方插件拿到的就是"没成功也没解释"，
    // 而目标被白名单拒 / 调度格式无效 / 库写不进去是三件完全不同的事。
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
            ensure_scheduler();
            match scheduler::add_task(&name, &target, &args, &stype, &stime, enabled) {
                Ok(id) => Ok((id, String::new())),
                Err(e) => Ok((-1i64, e.to_string())),
            }
        },
    )?;
    host.set("scheduler_add", add_fn)?;

    // 预检目标 + 参数：返回 (是否可用, 被拒原因)。
    // 让界面能把「为什么点添加没反应」说清楚，而不是只写进日志。
    let check_fn = lua.create_function(|_, (target, args): (String, String)| {
        Ok(match scheduler::check_target(&target, &args) {
            Ok(()) => (true, String::new()),
            Err(e) => (false, e),
        })
    })?;
    host.set("scheduler_check_target", check_fn)?;

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
            ensure_scheduler();
            let r = scheduler::update_task(
                id,
                Some(&name),
                Some(&target),
                Some(&args),
                Some(&stype),
                Some(&stime),
                Some(enabled),
            );
            match r {
                Ok(()) => Ok((true, String::new())),
                Err(e) => Ok((false, e.to_string())),
            }
        },
    )?;
    host.set("scheduler_update", update_fn)?;

    let delete_fn = lua.create_function(|_, id: i64| {
        ensure_scheduler();
        Ok(scheduler::delete_task(id))
    })?;
    host.set("scheduler_delete", delete_fn)?;

    let toggle_fn = lua.create_function(|_, (id, enabled): (i64, bool)| {
        ensure_scheduler();
        scheduler::toggle_task(id, enabled);
        Ok(())
    })?;
    host.set("scheduler_toggle", toggle_fn)?;

    // 纯格式校验：不需要库、也不需要调度线程，保持惰性
    let validate_fn = lua.create_function(|_, (stype, stime): (String, String)| {
        let (ok, msg) = scheduler::validate_schedule(&stype, &stime);
        Ok((ok, msg))
    })?;
    host.set("scheduler_validate", validate_fn)?;

    // 插件停用/卸载时在 cleanup 里回收调度线程，否则定时任务会在插件
    // 显示为「已停用」的状态下继续触发。重新启用插件时首次调用任一日程
    // API 会经 ensure_scheduler 再把线程起回来。
    host.set(
        "scheduler_shutdown",
        lua.create_function(|_, ()| {
            stop_scheduler();
            Ok(())
        })?,
    )?;

    // ---- 记账本 API ----
    // 与番茄钟/日程同理：记账库也改成惰性建库，禁用记账插件时不再碰它的文件。
    static ACCOUNTING_INIT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    fn ensure_accounting_db() {
        ACCOUNTING_INIT.get_or_init(|| {
            let _ = accounting::init_db();
        });
    }

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
            ensure_accounting_db();
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
            ensure_accounting_db();
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
        ensure_accounting_db();
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
        ensure_accounting_db();
        Ok(accounting::add_category(&name, &ctype, &[]))
    })?;
    host.set("accounting_category_add", acc_cat_add)?;

    // 重命名分类（同步历史记录）：(ok, msg)；第三个参数修改类型，空串表示保持原类型
    let acc_cat_rename =
        lua.create_function(|_, (old_name, new_name, ctype): (String, String, String)| {
            ensure_accounting_db();
            let ctype = if ctype.is_empty() {
                None
            } else {
                Some(ctype.as_str())
            };
            Ok(accounting::update_category(&old_name, &new_name, ctype))
        })?;
    host.set("accounting_category_rename", acc_cat_rename)?;

    // 删除分类：(ok, msg)
    let acc_cat_del = lua.create_function(|_, name: String| {
        ensure_accounting_db();
        Ok(accounting::delete_category(&name))
    })?;
    host.set("accounting_category_delete", acc_cat_del)?;

    // 查询分类类型（expense/income/both），无则空串。
    //
    // 注意：这里曾同时注册过一个「修改分类类型」的同名写接口，被本块覆盖成
    // 不可达死代码 —— 而 Lua 插件按注释把它当读接口用（返回值是类型字符串，
    // 解包成 `(ok, msg)` 会误判成功）。修改类型现由
    // `accounting_category_rename(old, new, ctype)` 的第三个参数承担。
    //
    // ensure 不可省：category_type 内部走 accounting::open()，而它是
    // Connection::open —— 库文件不存在时会顺手建出一个没有任何表的空库。
    let acc_cat_type = lua.create_function(|_, name: String| {
        ensure_accounting_db();
        Ok(accounting::category_type(&name).unwrap_or_default())
    })?;
    host.set("accounting_category_type", acc_cat_type)?;

    // 添加子分类：(ok, msg)
    let acc_sub_add = lua.create_function(|_, (cat, sub): (String, String)| {
        ensure_accounting_db();
        Ok(accounting::add_subcategory(&cat, &sub))
    })?;
    host.set("accounting_subcategory_add", acc_sub_add)?;

    // 重命名子分类：(ok, msg)
    let acc_sub_rename =
        lua.create_function(|_, (cat, old_sub, new_sub): (String, String, String)| {
            ensure_accounting_db();
            Ok(accounting::update_subcategory(&cat, &old_sub, &new_sub))
        })?;
    host.set("accounting_subcategory_rename", acc_sub_rename)?;

    // 删除子分类：(ok, msg)
    let acc_sub_del = lua.create_function(|_, (cat, sub): (String, String)| {
        ensure_accounting_db();
        Ok(accounting::delete_subcategory(&cat, &sub))
    })?;
    host.set("accounting_subcategory_delete", acc_sub_del)?;

    // 子分类列表
    let acc_subs = lua.create_function(|lua, cat: String| {
        ensure_accounting_db();
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
        ensure_accounting_db();
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
            ensure_accounting_db();
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
        ensure_accounting_db();
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
        ensure_accounting_db();
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
        ensure_accounting_db();
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
        ensure_accounting_db();
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
        ensure_accounting_db();
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

    let acc_delete = lua.create_function(|_, id: i64| {
        ensure_accounting_db();
        Ok(accounting::delete_expense(id))
    })?;
    host.set("accounting_delete", acc_delete)?;

    let acc_summary = lua.create_function(|_, ym: String| {
        ensure_accounting_db();
        let (expense, income) = accounting::monthly_summary(&ym);
        Ok((expense, income))
    })?;
    host.set("accounting_summary", acc_summary)?;

    // ---- Edge 历史 API ----
    // 插件跑在主线程（Lua 状态机非 Send），而 Edge 库一次同步读取最坏要等
    // 300ms busy 超时 + 三轮 ≤100MB 整文件复制，会把界面整个冻住。
    // 因此 edge_update_today 只**启动**后台刷新并返回是否启动成功；
    // 结果落在本地缓存库，渲染时用 edge_saved_today / edge_saved_total 取。
    let edge_update = lua.create_function(|_, ()| Ok(edge_history::spawn_update_today()))?;
    host.set("edge_update_today", edge_update)?;

    // 后台刷新状态："idle" | "running" | "ok" | "fail"
    let edge_state = lua.create_function(|_, ()| Ok(edge_history::refresh_state()))?;
    host.set("edge_refresh_state", edge_state)?;

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

    // 今日数 / 总数也取自本地缓存库：这两个名字是早先的直接读 Edge 版本，第三方
    // 脚本在用，所以保留签名（永远返回整数、没刷新过就是 0），但绝不碰 Edge 文件。
    // 否则插件一调它们，就把 ea7b2d3 刚修掉的主线程卡顿（300ms busy + 三轮
    // ≤100MB 复制）原样请回来。
    let edge_today =
        lua.create_function(|_, ()| Ok(edge_history::get_edge_history_saved_today().unwrap_or(0)))?;
    host.set("edge_today_count", edge_today)?;

    let edge_total =
        lua.create_function(|_, ()| Ok(edge_history::get_edge_history_saved_total().unwrap_or(0)))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 注册宿主 API 必须**无副作用**：三个附属库都要等到首次调用才建。
    ///
    /// 早前 register_host_api 在注册时就跑 `init_db`（日程还会 spawn 常驻线程），
    /// 于是只要启用任意一个插件，番茄钟/日程/记账的库就全被建出来 ——
    /// 表现是：插件明明停用了，数据目录里却躺着它的空库。
    #[test]
    fn registering_host_api_creates_no_aux_databases() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("host_lazy");

        let lua = Lua::new();
        let database = db::Database::init_readonly();
        register_host_api(&lua, crate::config::instance(), database).expect("注册宿主 API");

        for (name, path) in [
            ("pomodoro", crate::pomodoro::db_path()),
            ("scheduler", crate::scheduler::db_path()),
            ("accounting", crate::accounting::db_path()),
        ] {
            assert!(
                !path.exists(),
                "注册 API 不应创建 {name} 库: {}",
                path.display()
            );
        }

        // 调用番茄钟 API 之后，只应有番茄钟库被建出来
        lua.load("return focusflow.pomodoro_state()")
            .eval::<mlua::Table>()
            .expect("调用番茄钟 API");
        assert!(
            crate::pomodoro::db_path().exists(),
            "调用番茄钟 API 后应建库"
        );
        assert!(
            !crate::scheduler::db_path().exists(),
            "日程库必须等日程 API 被调用才建"
        );
        assert!(
            !crate::accounting::db_path().exists(),
            "记账库必须等记账 API 被调用才建"
        );
    }
}
