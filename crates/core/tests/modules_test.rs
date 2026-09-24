//! scheduler 与 pomodoro 模块测试。

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::OnceLock;

    use focusflow_core::accounting;
    use focusflow_core::edge_history;
    use focusflow_core::paths;
    use focusflow_core::pomodoro;
    use focusflow_core::scheduler;

    fn test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        test_lock().lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn scheduler_crud_and_validation() {
        let _g = guard();
        let dir = std::env::temp_dir().join(format!("ff_sched_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        paths::set_app_dir(&dir);

        // 初始化
        scheduler::init_db().unwrap();

        // 校验
        assert!(scheduler::validate_schedule("daily", "09:30").0);
        assert!(!scheduler::validate_schedule("daily", "25:00").0);
        assert!(scheduler::validate_schedule("once", "2026-12-01 10:00").0);
        assert!(!scheduler::validate_schedule("once", "bad").0);
        assert!(scheduler::validate_schedule("interval", "07:00-23:00|60").0);
        assert!(!scheduler::validate_schedule("interval", "bad").0);

        // 增删改查
        let id = scheduler::add_task(
            "测试",
            "C:\\Windows\\notepad.exe",
            "",
            "daily",
            "09:00",
            true,
        )
        .unwrap();
        assert!(id > 0);
        let tasks = scheduler::get_all_tasks();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "测试");

        assert!(
            scheduler::update_task(id, Some("改名"), None, None, None, None, Some(false)).is_ok()
        );
        let tasks = scheduler::get_all_tasks();
        assert_eq!(tasks[0].name, "改名");
        assert!(!tasks[0].enabled);

        assert!(
            scheduler::toggle_task(id, true).unwrap(),
            "存在的任务改成启用该回 true"
        );
        assert!(scheduler::get_all_tasks()[0].enabled);

        assert!(scheduler::delete_task(id).unwrap());
        assert!(scheduler::get_all_tasks().is_empty());

        // 调度描述
        assert_eq!(scheduler::describe_schedule("daily", "09:00"), "每日 09:00");
        assert_eq!(
            scheduler::describe_schedule("once", "2026-12-01 10:00"),
            "一次性 2026-12-01 10:00"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pomodoro_timer_flow() {
        let _g = guard();
        let dir = std::env::temp_dir().join(format!("ff_pomo_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        paths::set_app_dir(&dir);
        pomodoro::init_db().unwrap();

        let timer = pomodoro::PomodoroTimer::new();
        assert_eq!(timer.get_state(), pomodoro::STATE_IDLE);

        // 开始工作
        timer.start_work();
        assert_eq!(timer.get_state(), pomodoro::STATE_WORK);
        let info = timer.get_state_info();
        assert_eq!(info["state"], 1);
        assert!(info["planned"] >= 60);

        // 按键计数
        timer.record_key("A");
        timer.record_key("B");
        assert_eq!(timer.get_state_info()["key_count"], 2);

        // 暂停/继续
        assert!(timer.toggle_pause());
        timer.record_key("C"); // 暂停时不计数
        assert_eq!(timer.get_state_info()["key_count"], 2);
        assert!(!timer.toggle_pause());

        // 停之前先等满一个 tick（1 秒）：零秒的会话现在不再入库，
        // 而这个用例要验的是"停止会保存记录"，必须真有一段时长才行。
        std::thread::sleep(std::time::Duration::from_millis(1200));
        timer.stop();
        assert_eq!(timer.get_state(), pomodoro::STATE_IDLE);

        // 历史记录
        let sessions = pomodoro::get_recent_sessions(10);
        assert!(!sessions.is_empty(), "停止时应保存一条记录");
        assert_eq!(sessions[0].rtype, "work");
        assert!(
            sessions[0].actual_seconds >= 1,
            "记录里应带上真实跑过的秒数：{}",
            sessions[0].actual_seconds
        );
        assert_eq!(sessions[0].key_count, 2, "按键数应随会话一起入库");

        timer.shutdown();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn accounting_crud_and_summary() {
        let _g = guard();
        let dir = std::env::temp_dir().join(format!("ff_acc_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        paths::set_app_dir(&dir);
        accounting::init_db().unwrap();

        // 添加支出与收入
        let id1 = accounting::add_expense(
            "支出",
            "午餐",
            Some("食堂"),
            "2026-08-01",
            25.0,
            Some("食品"),
            Some("午餐"),
            None,
        );
        let id2 = accounting::add_expense(
            "收入",
            "工资",
            Some("公司"),
            "2026-08-01",
            5000.0,
            Some("工资收入"),
            None,
            None,
        );
        assert!(id1 > 0 && id2 > 0);

        // 查询
        let all = accounting::get_all_expenses(10);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].item_name, "工资"); // id DESC
        assert_eq!(all[0].amount, 5000.0);

        // 按日期
        let day = accounting::get_expenses_by_date("2026-08-01");
        assert_eq!(day.len(), 2);

        // 月度汇总
        let (expense, income) = accounting::monthly_summary("2026-08");
        assert_eq!(expense, 25.0);
        assert_eq!(income, 5000.0);

        // 分类：init_db 预置默认分类（13 个），再添加自定义分类
        accounting::add_category("食品", "expense", &["早餐".into(), "午餐".into()]);
        let cats = accounting::get_all_categories();
        assert_eq!(cats.len(), 14);
        assert!(cats.iter().any(|c| c.name == "食品" && c.subs.len() == 2));
        assert!(cats.iter().any(|c| c.name == "食品饮料"));

        // 删除
        assert!(accounting::delete_expense(id1));
        assert_eq!(accounting::get_all_expenses(10).len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn edge_history_storage() {
        let _g = guard();
        let dir = std::env::temp_dir().join(format!("ff_edge_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        paths::set_app_dir(&dir);

        // 保存计数到本地趋势库
        let today = chrono::Local::now().date_naive();
        let prev = today - chrono::Days::new(1);
        edge_history::save_edge_history_count(today, 100);
        edge_history::save_edge_history_count(prev, 50);

        // 读取趋势
        let counts = edge_history::get_edge_history_counts(30);
        assert!(!counts.is_empty(), "应有趋势数据");
        // 应包含今天和昨天
        let today_str = today.format("%Y-%m-%d").to_string();
        let prev_str = prev.format("%Y-%m-%d").to_string();
        let today_found = counts
            .iter()
            .find(|(d, _)| *d == today_str)
            .map(|(_, c)| *c);
        let prev_found = counts.iter().find(|(d, _)| *d == prev_str).map(|(_, c)| *c);
        assert_eq!(today_found, Some(100));
        assert_eq!(prev_found, Some(50));

        std::fs::remove_dir_all(&dir).ok();
    }
}
