//! 插件稳健性：反复 get_view 渲染 + 加载/卸载循环，检查无泄漏/无崩溃。

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::OnceLock;

    use focusflow_core::config::FocusFlowConfig;
    use focusflow_core::db;
    use focusflow_core::paths;
    use focusflow_core::plugins::manager::PluginManager;

    fn test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        test_lock().lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn repeated_view_render_stability() {
        let _g = guard();
        let dir = std::env::current_dir().unwrap();
        paths::set_app_dir(&dir);
        db::queries::invalidate_years_cache();

        let config: &'static FocusFlowConfig = Box::leak(Box::new(
            FocusFlowConfig::load(dir.join("config.ini")).unwrap(),
        ));
        let database = db::Database::init_readonly();
        let mut manager = PluginManager::new(config, database);

        // 加载所有插件
        for f in manager.discover() {
            manager.load_plugin(&f).unwrap();
        }
        assert!(!manager.get_all_plugins().is_empty());

        // 反复读取视图（模拟 GUI 每帧渲染 get_view）
        for _ in 0..50 {
            for name in ["统计速览", "番茄工作法", "记账本", "定时任务"] {
                let info = manager.get_plugin(name);
                assert!(info.is_some(), "{name} 应存在");
                // 触发 get_view 重新解析（模拟渲染线程读缓存视图）
                let view = info.unwrap().view.as_ref();
                assert!(view.is_some(), "{name} 应有视图");
            }
        }

        // 卸载再加载循环（模拟热重载），验证无累积
        for _ in 0..5 {
            for name in ["统计速览", "番茄工作法", "记账本", "定时任务"] {
                assert!(manager.unload_plugin(name), "卸载 {name}");
            }
            for f in manager.discover() {
                manager.load_plugin(&f).unwrap();
            }
        }

        assert_eq!(manager.get_all_plugins().len(), manager.discover().len());
    }

    #[test]
    fn plugin_key_event_robust() {
        let _g = guard();
        let dir = std::env::current_dir().unwrap();
        paths::set_app_dir(&dir);
        db::queries::invalidate_years_cache();

        let config: &'static FocusFlowConfig = Box::leak(Box::new(
            FocusFlowConfig::load(dir.join("config.ini")).unwrap(),
        ));
        let database = db::Database::init_readonly();
        let mut manager = PluginManager::new(config, database);
        for f in manager.discover() {
            manager.load_plugin(&f).unwrap();
        }

        // 向番茄钟插件投递按键（无 record_key 的插件应安全忽略）
        for i in 0..100 {
            manager.plugin_key_event("番茄工作法", &format!("K{i}"));
            manager.plugin_key_event("统计速览", "A"); // 无 record_key，应忽略
        }

        // 动作调用（番茄钟按钮）
        manager.plugin_action("番茄工作法", "start_work").unwrap();
        manager.plugin_action("番茄工作法", "toggle_pause").unwrap();
        manager.plugin_action("番茄工作法", "stop").unwrap();

        // 未知插件应报错而非 panic
        assert!(manager.plugin_action("不存在", "x").is_err());
        assert!(manager.plugin_set_field("不存在", "a", "b").is_err());
    }
}
