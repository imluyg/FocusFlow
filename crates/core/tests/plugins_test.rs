//! 插件系统集成测试：加载/卸载/热重载/宿主 API。

#[cfg(test)]
mod tests {
    use focusflow_core::config::FocusFlowConfig;
    use focusflow_core::db;
    use focusflow_core::paths;
    use focusflow_core::plugins::manager::PluginManager;
    use focusflow_core::plugins::Widget;

    /// 串行锁（app_dir 全局状态），容忍 poison（测试失败后不阻塞其他）
    fn test_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        test_lock().lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn load_unload_plugin() {
        let _guard = guard();
        // 用 core crate 所在目录作为 app_dir（plugins/ 在其中）
        let dir = std::env::current_dir().unwrap();
        paths::set_app_dir(&dir);
        db::queries::invalidate_years_cache();

        let config: &'static FocusFlowConfig = Box::leak(Box::new(
            FocusFlowConfig::load(dir.join("config.ini")).unwrap(),
        ));
        let database = db::Database::init_readonly();
        let mut manager = PluginManager::new(config, database);

        // 发现插件
        let files = manager.discover();
        assert!(
            files.iter().any(|f| f.ends_with("stats_overview.lua")),
            "应发现 stats_overview.lua, got {files:?}"
        );

        // 加载
        let file = files
            .iter()
            .find(|f| f.ends_with("stats_overview.lua"))
            .unwrap();
        let name = manager.load_plugin(file).expect("加载插件失败");
        assert_eq!(name, "统计速览");

        // 检查元数据
        let info = manager.get_plugin(&name).expect("插件应存在");
        assert_eq!(info.version, "1.0.0");
        assert!(info.view.is_some(), "应有视图");

        // 视图内容
        let view = info.view.as_ref().unwrap();
        assert_eq!(view.title, "今日统计速览");
        assert!(!view.widgets.is_empty());
        // 应包含 table 控件
        let has_table = view
            .widgets
            .iter()
            .any(|w| matches!(w, focusflow_core::plugins::Widget::Table { .. }));
        assert!(has_table, "应包含表格控件");

        // 卸载
        assert!(manager.unload_plugin(&name));
        assert!(manager.get_plugin(&name).is_none());
    }

    /// 全量体检：`crates/core/plugins/` 下每个内置插件都必须能解析、init、
    /// 渲染出视图，并跑一次 `refresh` 动作走到宿主 API。
    ///
    /// 上面的用例只显式加载了 stats_overview.lua，其余内置插件的 Lua 从来没被
    /// 任何测试执行过 —— 而宿主与 Lua 互不编译，宿主 API 改了形状（例如
    /// `edge_update_today` 从返回 (ok, today, total) 改成只返回「是否已启动后台
    /// 刷新」+ 新增 `edge_refresh_state`）时，编译和 clippy 全绿，只有用户在 GUI
    /// 里点开那个插件才会发现它加载失败。这个测试把这类断裂变成 CI 可见。
    #[test]
    fn all_bundled_plugins_load_render_and_act() {
        let _guard = guard();
        let dir = std::env::current_dir().unwrap();
        paths::set_app_dir(&dir);
        db::queries::invalidate_years_cache();
        let config: &'static FocusFlowConfig = Box::leak(Box::new(
            FocusFlowConfig::load(dir.join("config.ini")).unwrap(),
        ));
        let database = db::Database::init_readonly();
        let mut manager = PluginManager::new(config, database);

        let files = manager.discover();
        assert!(
            files.len() >= 4,
            "内置插件目录应有至少 4 个插件，实际 {files:?}"
        );
        for f in &files {
            let fpath = f.display().to_string();
            let name = manager
                .load_plugin(f)
                .unwrap_or_else(|e| panic!("加载内置插件 {fpath} 失败: {e}"));
            let info = manager.get_plugin(&name).expect("插件应已注册");
            assert!(
                info.view.is_some(),
                "{name}（{fpath}）应渲染出视图 —— init/get_view 里报错会让插件页空白"
            );
            assert!(
                !info.view.as_ref().unwrap().title.is_empty(),
                "{name} 的视图标题不应为空"
            );
            // refresh 是各插件共用的动作 id：走一遍 on_action → 宿主 API 调用链
            let _ = manager.plugin_action(&name, "refresh");
            // 动作之后必须还能重新出图：on_action 改坏了模块级状态（例如把某个
            // 变量置成面板引用不到的形态）时，用户看到的就是插件页空白。
            // 刻意不触发 "add" —— 那会真的插入一条定时任务并被调度线程执行。
            manager
                .refresh_view(&name)
                .unwrap_or_else(|e| panic!("{name} 动作之后重新出图失败: {e}"));
            let after = manager.get_plugin(&name).expect("插件应仍在").view.clone();
            let widgets = after.expect("重新出图后应有视图");
            assert!(
                !widgets.widgets.is_empty(),
                "{name}（{fpath}）动作后的视图不应为空面板"
            );
            assert!(
                manager.unload_plugin(&name),
                "{name}（{fpath}）应能卸载（cleanup 里报错会残留后台线程）"
            );
        }
    }

    #[test]
    fn accounting_view_widgets() {
        // 新控件类型（select / modal_form / 分页按钮 disabled）解析回归测试
        let _guard = guard();
        let dir = std::env::current_dir().unwrap();
        paths::set_app_dir(&dir);
        db::queries::invalidate_years_cache();

        let config: &'static FocusFlowConfig = Box::leak(Box::new(
            FocusFlowConfig::load(dir.join("config.ini")).unwrap(),
        ));
        let database = db::Database::init_readonly();
        let mut manager = PluginManager::new(config, database);

        let files = manager.discover();
        let file = files
            .iter()
            .find(|f| f.ends_with("accounting_plugin.lua"))
            .expect("应发现 accounting_plugin.lua");
        let name = manager.load_plugin(file).expect("加载记账本插件失败");
        assert_eq!(name, "记账本");

        let view = manager
            .get_plugin(&name)
            .and_then(|p| p.view.clone())
            .expect("应有视图");
        use focusflow_core::plugins::Widget;
        let has_modal = view.widgets.iter().any(|w| {
            matches!(
                w,
                Widget::ModalForm {
                    submit,
                    fields,
                    ..
                } if submit == "add_record" && !fields.is_empty()
            )
        });
        assert!(has_modal, "记账本视图应包含新增记录的弹窗表单控件");
        let has_edit_modal = view
            .widgets
            .iter()
            .any(|w| matches!(w, Widget::ModalForm { id, .. } if id == "edit_modal"));
        assert!(has_edit_modal, "记账本视图应包含编辑记录弹窗");
        // 递归查找（筛选控件在 Row 容器内）
        fn find_widget(widgets: &[Widget], pred: impl Fn(&Widget) -> bool + Copy) -> bool {
            widgets.iter().any(|w| {
                if pred(w) {
                    return true;
                }
                if let Widget::Row { children } = w {
                    return find_widget(children, pred);
                }
                false
            })
        }
        let has_cat_filter = find_widget(&view.widgets, |w| {
            matches!(
                w,
                Widget::Select { field, refresh, .. } if field == "f_cat" && *refresh
            )
        });
        assert!(has_cat_filter, "记账本视图应包含分类筛选下拉（联动刷新）");
        let has_row = view
            .widgets
            .iter()
            .any(|w| matches!(w, Widget::Row { children } if !children.is_empty()));
        assert!(has_row, "记账本视图应包含横向行容器（操作栏/筛选栏）");
        let has_pager = view
            .widgets
            .iter()
            .any(|w| matches!(w, Widget::Pager { page, .. } if *page >= 1));
        assert!(has_pager, "记账本视图应包含分页条");
        let has_table_ids = view.widgets.iter().any(|w| {
            // ids 与 rows 平行（测试库为空时均为空，验证接线结构）
            matches!(w, Widget::Table { ids, rows, .. } if ids.len() == rows.len())
        });
        assert!(has_table_ids, "记账本视图表格应包含记录 id（行选中用）");
        let has_sel_button = find_widget(
            &view.widgets,
            |w| matches!(w, Widget::Button { id, sel, .. } if id == "edit_" && *sel),
        );
        assert!(has_sel_button, "记账本视图应包含选中行修改按钮（sel）");

        // 回归：set_field 后视图必须刷新（否则出现"点了没反应"）
        manager
            .plugin_set_field(&name, "f_kw", "测试关键词")
            .expect("set_field 应成功");
        let view2 = manager
            .get_plugin(&name)
            .and_then(|p| p.view.clone())
            .expect("应有视图");
        let has_kw = find_widget(&view2.widgets, |w| {
            matches!(
                w,
                Widget::TextInput { field, value, .. } if field == "f_kw" && value == "测试关键词"
            )
        });
        assert!(has_kw, "set_field 后视图应刷新（关键词输入框回填新值）");

        assert!(manager.unload_plugin(&name));
    }

    /// 回归：前端 sel 按钮的动作 id 是「按钮 id 直接拼接选中项」（无分隔符），
    /// 例如 `m_edit_cat_sel` .. `食品饮料`。Lua 侧模式曾写成带尾部下划线的
    /// `^m_edit_cat_sel_`，永远匹配不上 → 分类管理页 4 个按钮点了完全没反应。
    #[test]
    fn accounting_category_sel_buttons_are_wired() {
        let _guard = guard();
        let dir = std::env::current_dir().unwrap();
        paths::set_app_dir(&dir);
        db::queries::invalidate_years_cache();

        let config: &'static FocusFlowConfig = Box::leak(Box::new(
            FocusFlowConfig::load(dir.join("config.ini")).unwrap(),
        ));
        let database = db::Database::init_readonly();
        let mut manager = PluginManager::new(config, database);

        let files = manager.discover();
        let file = files
            .iter()
            .find(|f| f.ends_with("accounting_plugin.lua"))
            .expect("应发现 accounting_plugin.lua");
        let name = manager.load_plugin(file).expect("加载记账本插件失败");

        fn find_widget(widgets: &[Widget], pred: impl Fn(&Widget) -> bool + Copy) -> bool {
            widgets.iter().any(|w| {
                if pred(w) {
                    return true;
                }
                if let Widget::Row { children } = w {
                    return find_widget(children, pred);
                }
                false
            })
        }
        fn current_widgets(manager: &PluginManager, name: &str) -> Vec<Widget> {
            manager
                .get_plugin(name)
                .and_then(|p| p.view.as_ref())
                .map(|v| v.widgets.clone())
                .unwrap_or_default()
        }
        use focusflow_core::plugins::Widget;

        // 进入分类管理页
        manager
            .plugin_action(&name, "open_manage")
            .expect("打开分类管理应成功");
        assert!(
            !find_widget(&current_widgets(&manager, &name), |w| matches!(
                w,
                Widget::ModalForm { id, open, .. } if id == "m_edit_cat_modal" && *open
            )),
            "初始状态不应弹出修改分类弹窗"
        );

        // 前端实际发出的动作 id：按钮 id + 选中分类名（无分隔符）
        manager
            .plugin_action(&name, "m_edit_cat_sel食品饮料")
            .expect("点击「修改分类」应成功");
        assert!(
            find_widget(&current_widgets(&manager, &name), |w| matches!(
                w,
                Widget::ModalForm { id, open, .. } if id == "m_edit_cat_modal" && *open
            )),
            "点击「修改分类」必须弹出编辑弹窗（回归：模式串曾带尾部下划线导致永不命中）"
        );

        // 取消后关闭
        manager
            .plugin_action(&name, "m_cancel_edit_cat")
            .expect("取消编辑应成功");
        assert!(
            !find_widget(&current_widgets(&manager, &name), |w| matches!(
                w,
                Widget::ModalForm { id, open, .. } if id == "m_edit_cat_modal" && *open
            )),
            "取消后编辑弹窗应关闭"
        );

        // 修改子分类按钮同属一类契约（同一模式下划线问题）
        manager
            .plugin_action(&name, "m_edit_sub_sel餐饮")
            .expect("点击「修改子分类」应成功");
        assert!(
            find_widget(&current_widgets(&manager, &name), |w| matches!(
                w,
                Widget::ModalForm { id, open, .. } if id == "m_edit_sub_modal" && *open
            )),
            "点击「修改子分类」必须弹出编辑弹窗"
        );

        assert!(manager.unload_plugin(&name));
    }

    #[test]
    fn host_api_stats() {
        let _guard = guard();
        let dir = std::env::current_dir().unwrap();
        paths::set_app_dir(&dir);
        db::queries::invalidate_years_cache();

        // 用 Lua 直接调宿主 API 验证
        let lua = mlua::Lua::new();
        let config: &'static FocusFlowConfig = Box::leak(Box::new(
            FocusFlowConfig::load(dir.join("config.ini")).unwrap(),
        ));
        let database = db::Database::init_readonly();
        focusflow_core::plugins::host::register_host_api(&lua, config, database, "host_api_test")
            .unwrap();

        // today_count 应返回数字
        let today: i64 = lua
            .load("return focusflow.today_count()")
            .eval()
            .expect("today_count 调用失败");
        assert!(today >= 0);

        // stats(0) 返回 (total, **按次数降序**的 {name,count} 数组, 键鼠种类数)。
        //
        // 老契约是"名字->次数"的表 + 两个返回值：`get_stats` 内部是 HashMap，
        // SQL 的 ORDER BY cnt DESC 被抹平，插件再 `pairs()` 取前十 —— 于是
        // 「键鼠排行（Top 10）」是任意十个，而"种类"显示的是榜单长度（永远 10）。
        let (total, rows, distinct): (i64, mlua::Table, i64) = lua
            .load("local t, k, n = focusflow.stats(0) return t, k, n")
            .eval()
            .expect("stats 调用失败");
        assert!(total >= 0);
        let mut prev: Option<i64> = None;
        let mut n = 0i64;
        for row in rows.sequence_values::<mlua::Table>() {
            let row = row.expect("榜单每一项都该是表");
            let name: String = row.get("name").expect("每项要有 name");
            let count: i64 = row.get("count").expect("每项要有 count");
            assert!(!name.is_empty(), "键名不该为空");
            if let Some(p) = prev {
                assert!(
                    count <= p,
                    "榜单必须按次数从多到少: {name} {count} 排在 {p} 之后"
                );
            }
            prev = Some(count);
            n += 1;
        }
        assert_eq!(
            n, distinct,
            "数组长度就是真实种类数；旧实现把榜单长度当种类数（永远 10）"
        );

        // app_info 应返回版本
        let info: String = lua.load("return focusflow.app_info()").eval().unwrap();
        assert!(info.contains("FocusFlow"));
    }

    #[test]
    fn hot_reload_request() {
        focusflow_core::logger::init_logging();
        let _guard = guard();
        let dir = std::env::current_dir().unwrap();
        paths::set_app_dir(&dir);
        db::queries::invalidate_years_cache();

        let config: &'static FocusFlowConfig = Box::leak(Box::new(
            FocusFlowConfig::load(dir.join("config.ini")).unwrap(),
        ));
        let database = db::Database::init_readonly();
        let mut manager = PluginManager::new(config, database);

        // 加载
        let file = manager
            .discover()
            .into_iter()
            .find(|f| f.ends_with("stats_overview.lua"))
            .unwrap();
        manager.load_plugin(&file).unwrap();

        // 启用热重载（检测线程）
        manager.enable_hot_reload();

        // 等待检测线程完成首次基线扫描（2s 周期）
        std::thread::sleep(std::time::Duration::from_millis(2500));

        // 修改文件触发重载请求（追加注释改变 mtime）
        let path = manager.get_plugin("统计速览").unwrap().file_path.clone();
        let content = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("{content}\n-- hot reload test\n")).unwrap();

        // 等待检测线程扫描并 poll（最多 8 秒）
        let mut reloaded = Vec::new();
        for _ in 0..8 {
            std::thread::sleep(std::time::Duration::from_millis(1000));
            reloaded = manager.poll_reload_requests();
            if !reloaded.is_empty() {
                break;
            }
        }

        // 恢复内容（避免污染）
        std::fs::write(&path, content).unwrap();

        assert!(
            reloaded.contains(&"stats_overview".to_string())
                || reloaded.contains(&"统计速览".to_string()),
            "应收到重载请求, got {reloaded:?}"
        );
        assert!(manager.get_plugin("统计速览").is_some(), "重载后插件应存在");

        manager.disable_hot_reload();
    }

    /// 内置插件的"行内操作按钮"必须真的声明出来 —— 只在 `on_action` 里写分支不够。
    ///
    /// 定时任务插件原先把 `"toggle_3"` / `"del_3"` 当普通文本塞进单元格：表格没有
    /// `ids` + `actions` 时前端只渲染文本，于是那两列既点不动、又把内部动作 id
    /// 印在界面上，而 `on_action` 里那两段 `^toggle_` / `^del_` 分支永远到不了 ——
    /// 表现就是"定时任务在界面上既停不掉也删不掉"（只能改库或走 CLI）。
    #[test]
    fn scheduler_plugin_declares_row_actions_instead_of_action_text() {
        use focusflow_core::plugins::Widget;
        let _guard = guard();
        let dir = std::env::current_dir().unwrap();
        paths::set_app_dir(&dir);
        let config: &'static FocusFlowConfig = Box::leak(Box::new(
            FocusFlowConfig::load(dir.join("config.ini")).unwrap(),
        ));
        let database = db::Database::init_readonly();
        let mut manager = PluginManager::new(config, database);
        let file = manager
            .discover()
            .into_iter()
            .find(|f| f.ends_with("scheduler_plugin.lua"))
            .expect("应发现 scheduler_plugin.lua");
        let name = manager.load_plugin(&file).expect("加载插件失败");
        let view = manager
            .get_plugin(&name)
            .expect("插件应存在")
            .view
            .clone()
            .expect("应有视图");

        let found = view.widgets.iter().find_map(|w| match w {
            Widget::Table {
                headers,
                rows,
                ids,
                actions,
                ..
            } => Some((headers.clone(), rows.clone(), ids.clone(), actions.clone())),
            _ => None,
        });
        let (headers, rows, ids, actions) = found.expect("任务列表应是表格控件");
        assert!(
            !actions.is_empty(),
            "表格必须声明行内按钮，否则界面上就是两列死文本"
        );
        let prefixes: Vec<&str> = actions.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            prefixes,
            vec!["toggle_", "edit_", "del_"],
            "按钮前缀必须与 on_action 里的 ^toggle_ / ^edit_ / ^del_ 分支对得上"
        );
        assert_eq!(ids.len(), rows.len(), "ids 必须与 rows 一一对应");
        // 最后一列由按钮渲染，所以表头要比数据列多一个
        if let Some(first) = rows.first() {
            assert_eq!(
                headers.len(),
                first.len() + 1,
                "有 actions 时表头应比数据列多一列（按钮那一列）"
            );
        }
        for r in &rows {
            for c in r {
                assert!(
                    !c.starts_with("toggle_") && !c.starts_with("del_"),
                    "单元格里不该再印动作 id：{c}"
                );
            }
        }
    }

    /// 弹窗表单里的 `refresh` 必须一路活到前端 —— 分类 → 子分类联动靠它。
    ///
    /// 行内下拉控件（`Widget::Select`）一直带着这个标志，所以记账页顶部的
    /// "分类"筛选是好的；而弹窗表单的 `FormField` 从头到尾没有这个字段，
    /// Lua 里 `d_category` 写的 `refresh = true` 在 Rust 侧被丢掉，
    /// 前端就永远走 `plugin-field-stay`（只写状态、不重建）——
    /// 换分类后子分类下拉还是旧分类的选项，能存进去一个不属于该分类的子分类。
    #[test]
    fn accounting_modal_form_keeps_refresh_flag() {
        use focusflow_core::plugins::Widget;
        let _guard = guard();
        let dir = std::env::current_dir().unwrap();
        paths::set_app_dir(&dir);
        let config: &'static FocusFlowConfig = Box::leak(Box::new(
            FocusFlowConfig::load(dir.join("config.ini")).unwrap(),
        ));
        let database = db::Database::init_readonly();
        let mut manager = PluginManager::new(config, database);
        let file = manager
            .discover()
            .into_iter()
            .find(|f| f.ends_with("accounting_plugin.lua"))
            .expect("应发现 accounting_plugin.lua");
        let name = manager.load_plugin(&file).expect("加载插件失败");
        let view = manager
            .get_plugin(&name)
            .expect("插件应存在")
            .view
            .clone()
            .expect("应有视图");

        let mut saw_category_field = false;
        for w in &view.widgets {
            if let Widget::ModalForm { fields, .. } = w {
                for f in fields {
                    if f.field == "d_category" {
                        saw_category_field = true;
                        assert!(
                            f.refresh,
                            "分类下拉必须带 refresh：否则改了分类，子分类还是旧选项"
                        );
                    }
                }
            }
        }
        assert!(saw_category_field, "记账弹窗里应有 d_category 这一项");
    }

    /// 记账的「保存」被拒时，原因必须出现在视图里。
    ///
    /// 旧实现只 `focusflow.log("请填写名称和有效金额")` 然后 `return`，而前端
    /// `modalSubmit` 是**先关弹窗再发动作**（`ui/js/plugins.js`）—— 于是用户看到的是
    /// "弹窗关了、列表里没这条记录"，一个字都没有。与定时任务那边已修的
    /// "被拒时要说清原因"同一族，这是记账本里漏的半边。
    #[test]
    fn accounting_rejected_save_shows_a_visible_reason() {
        fn labels(manager: &PluginManager, name: &str) -> Vec<String> {
            fn walk(widgets: &[focusflow_core::plugins::Widget], out: &mut Vec<String>) {
                use focusflow_core::plugins::Widget;
                for w in widgets {
                    match w {
                        Widget::Label(t) => out.push(t.clone()),
                        Widget::Row { children } => walk(children, out),
                        _ => {}
                    }
                }
            }
            let mut out = Vec::new();
            if let Some(p) = manager.get_plugin(name) {
                if let Some(view) = &p.view {
                    walk(&view.widgets, &mut out);
                }
            }
            out
        }

        let _guard = guard();
        let dir = std::env::current_dir().unwrap();
        paths::set_app_dir(&dir);
        db::queries::invalidate_years_cache();
        let config: &'static FocusFlowConfig = Box::leak(Box::new(
            FocusFlowConfig::load(dir.join("config.ini")).unwrap(),
        ));
        let mut manager = PluginManager::new(config, db::Database::init_readonly());
        let file = manager
            .discover()
            .into_iter()
            .find(|f| f.ends_with("accounting_plugin.lua"))
            .expect("应发现 accounting_plugin.lua");
        let name = manager.load_plugin(&file).expect("加载记账本插件失败");

        assert!(
            !labels(&manager, &name).iter().any(|t| t.contains("没保存")),
            "刚加载不该有失败提示"
        );

        // 名称与金额都是空的草稿：必须被拒（**不写库**），并且把原因写在视图里
        manager
            .plugin_action(&name, "add_record")
            .expect("动作本身不该失败");
        let seen = labels(&manager, &name);
        assert!(
            seen.iter().any(|t| t.contains("没保存")),
            "被拒的原因必须出现在插件页上，实际标签: {seen:?}"
        );

        // 修改路径同理：没有编辑对象时静默 return 是对的，但校验失败必须有话
        manager
            .plugin_action(&name, "save_edit")
            .expect("无编辑对象时的 save_edit 不该报错");
    }

    // =====================================================================
    // 下面这组用例都跑在**独立临时目录**里：它们要真的写记账/番茄钟/定时任务的
    // 附属库，不能落在仓库目录（`plugins_test.rs` 上面那批只做加载+渲染，才敢用
    // `set_app_dir(当前目录)`）。
    // =====================================================================

    /// 把一个内置插件复制进隔离的临时程序目录并加载。
    fn bundled_in(tag: &str, file: &str) -> (paths::TestAppDir, PluginManager, String) {
        let tmp = paths::test_app_dir(tag);
        let src = std::env::current_dir()
            .expect("用例应从 crates/core 目录运行")
            .join("plugins")
            .join(file);
        let dst = tmp.path().join("plugins").join(file);
        std::fs::create_dir_all(dst.parent().unwrap()).expect("建临时 plugins 目录");
        std::fs::copy(&src, &dst).unwrap_or_else(|e| panic!("复制内置插件 {src:?} 失败: {e}"));
        db::queries::invalidate_years_cache();
        let config: &'static FocusFlowConfig = Box::leak(Box::new(
            FocusFlowConfig::load(tmp.path().join("config.ini")).expect("临时配置应能载入"),
        ));
        let mut manager = PluginManager::new(config, db::Database::init_readonly());
        let name = manager
            .load_plugin(&dst)
            .unwrap_or_else(|e| panic!("加载内置插件 {file} 失败: {e}"));
        (tmp, manager, name)
    }

    /// 递归展开控件（`Row` 容器与弹窗内嵌的那一层也要看到）。
    fn flatten(widgets: &[Widget]) -> Vec<Widget> {
        let mut out = Vec::new();
        for w in widgets {
            out.push(w.clone());
            match w {
                Widget::Row { children } => out.extend(flatten(children)),
                Widget::ModalForm { widgets: inner, .. } => out.extend(flatten(inner)),
                _ => {}
            }
        }
        out
    }

    fn view_of(manager: &PluginManager, name: &str) -> Vec<Widget> {
        flatten(
            &manager
                .get_plugin(name)
                .and_then(|p| p.view.as_ref())
                .expect("插件应有视图")
                .widgets,
        )
    }

    fn title_of(manager: &PluginManager, name: &str) -> String {
        manager
            .get_plugin(name)
            .and_then(|p| p.view.as_ref())
            .map(|v| v.title.clone())
            .unwrap_or_default()
    }

    fn labels_of(manager: &PluginManager, name: &str) -> Vec<String> {
        view_of(manager, name)
            .into_iter()
            .filter_map(|w| match w {
                Widget::Label(t) | Widget::TextArea(t) => Some(t),
                _ => None,
            })
            .collect()
    }

    /// 渲染时就处于打开态的弹窗（`open = true`）—— A0.4 说的那"自己弹出来"。
    fn open_modal_ids(manager: &PluginManager, name: &str) -> Vec<String> {
        view_of(manager, name)
            .into_iter()
            .filter_map(|w| match w {
                Widget::ModalForm { id, open: true, .. } => Some(id),
                _ => None,
            })
            .collect()
    }

    fn select_value(manager: &PluginManager, name: &str, field: &str) -> Option<String> {
        view_of(manager, name).into_iter().find_map(|w| match w {
            Widget::Select {
                field: f, value, ..
            } if f == field => Some(value),
            _ => None,
        })
    }

    fn textinput_value(manager: &PluginManager, name: &str, field: &str) -> Option<String> {
        view_of(manager, name).into_iter().find_map(|w| match w {
            Widget::TextInput {
                field: f, value, ..
            } if f == field => Some(value),
            _ => None,
        })
    }

    fn pager_of(manager: &PluginManager, name: &str) -> (i64, i64, i64) {
        view_of(manager, name)
            .into_iter()
            .find_map(|w| match w {
                Widget::Pager {
                    page, pages, total, ..
                } => Some((page, pages, total)),
                _ => None,
            })
            .expect("视图里应有分页条")
    }

    fn first_table_rows(manager: &PluginManager, name: &str) -> Vec<Vec<String>> {
        view_of(manager, name)
            .into_iter()
            .find_map(|w| match w {
                Widget::Table { rows, .. } => Some(rows),
                _ => None,
            })
            .expect("视图里应有表格")
    }

    fn keyvalue_of(manager: &PluginManager, name: &str, key: &str) -> Option<String> {
        view_of(manager, name).into_iter().find_map(|w| match w {
            Widget::KeyValue(k, v) if k == key => Some(v),
            _ => None,
        })
    }

    fn button_ids(manager: &PluginManager, name: &str) -> Vec<String> {
        view_of(manager, name)
            .into_iter()
            .filter_map(|w| match w {
                Widget::Button { id, .. } => Some(id),
                _ => None,
            })
            .collect()
    }

    fn modal_field(modal: &Widget, field: &str) -> Option<String> {
        match modal {
            Widget::ModalForm { fields, .. } => fields
                .iter()
                .find(|f| f.field == field)
                .map(|f| f.value.clone()),
            _ => None,
        }
    }

    fn modal_open(modal: &Widget) -> bool {
        matches!(modal, Widget::ModalForm { open: true, .. })
    }

    /// A0.4：面板关掉之后，Lua 里的一次性状态必须清零。
    ///
    /// `closePlugin` 原先只清 JS 侧的 `openModals`，插件的 Lua 状态活着 —— 重开记账
    /// 面板时 `edit_modal`/`profit_modal`/结果弹窗自己弹出来、人还落在分类管理子页；
    /// Edge 的 `hint`、定时任务的 `msg` 挂到下一次同类动作才走。
    /// 前端在关闭路径上发一个约定的 `__panel_closed` 动作（宿主没有"面板关闭"入口），
    /// 这条用例钉的就是插件收到它之后的行为。
    #[test]
    fn panel_transient_state_is_reset_when_the_panel_closes() {
        let _guard = guard();

        // ---- 记账：弹窗 + 分类管理子页 ----
        let (tmp, mut manager, name) = bundled_in("reset_acct", "accounting_plugin.lua");
        manager
            .plugin_action(&name, "open_manage")
            .expect("打开分类管理");
        manager
            .plugin_action(&name, "open_profit_picker")
            .expect("打开细分盈亏选择");
        manager
            .plugin_action(&name, "monthly_detail")
            .expect("月度汇总");
        let opened = open_modal_ids(&manager, &name);
        assert!(
            opened.contains(&"result_modal".to_string())
                && opened.contains(&"profit_modal".to_string()),
            "动作之后这些弹窗本就该是打开的（先证明状态确实立起来了）: {opened:?}"
        );
        assert_eq!(title_of(&manager, &name), "记账本 - 分类管理");

        manager
            .plugin_action(&name, "__panel_closed")
            .expect("关闭动作不该失败");
        assert!(
            open_modal_ids(&manager, &name).is_empty(),
            "面板关闭后不该再有任何弹窗是打开的： {:?}",
            open_modal_ids(&manager, &name)
        );
        assert_eq!(
            title_of(&manager, &name),
            "记账本",
            "关闭后重开必须落在记账主页，而不是分类管理子页"
        );
        assert!(manager.unload_plugin(&name));
        drop(tmp);

        // ---- Edge：hint 不该挂到下一次刷新为止 ----
        let (tmp, mut manager, name) = bundled_in("reset_edge", "edge_history_plugin.lua");
        manager
            .plugin_action(&name, "refresh")
            .expect("刷新动作不该失败");
        assert!(
            labels_of(&manager, &name)
                .iter()
                .any(|t| t.contains("后台")),
            "点了刷新数据必须有一句回话"
        );
        manager
            .plugin_action(&name, "__panel_closed")
            .expect("关闭动作不该失败");
        // 只要求 **Lua 侧那句 hint** 消失。"⏳ 正在后台读取…" 那一行是写线程真实在途时的
        // 状态标签（`95eb1f1` 的 claim_round 语义），关闭面板不该、也无力把它抹掉 ——
        // A0.4 修的是"提示挂到下次同类动作为止"，不是伪造进度。
        assert!(
            labels_of(&manager, &name)
                .iter()
                .all(|t| !t.contains("已启动后台读取") && !t.contains("稍后再试")),
            "关闭面板后那句自家的提示必须消失，实际: {:?}",
            labels_of(&manager, &name)
        );
        assert!(manager.unload_plugin(&name));
        drop(tmp);

        // ---- 定时任务：结果语同样不该残留 ----
        let (tmp, mut manager, name) = bundled_in("reset_sched", "scheduler_plugin.lua");
        manager
            .plugin_action(&name, "toggle_999999")
            .expect("点了不存在的任务也不该报错");
        assert!(
            labels_of(&manager, &name)
                .iter()
                .any(|t| t.contains("切换失败")),
            "失败原因要先出现在面板上（才有'残留'可言）"
        );
        manager
            .plugin_action(&name, "__panel_closed")
            .expect("关闭动作不该失败");
        assert!(
            labels_of(&manager, &name)
                .iter()
                .all(|t| !t.contains("切换失败")),
            "关闭面板后结果语必须清零，实际: {:?}",
            labels_of(&manager, &name)
        );
        assert!(manager.unload_plugin(&name), "卸载要回收调度线程");
        drop(tmp);
    }

    /// A0.1 的补测（`ca9111c` 的行为断言）：改名/删除分类后，悬空的筛选条件必须被
    /// 校正回来，而且**列表要重新有数据**。
    ///
    /// 那条提交只被"加载 + 渲染 + refresh"盖住 —— 语法错与 get_view 抛错能挡住，
    /// "校正"本身没被断言过。这里把 `reset_stale_filters()` 注掉，红的就是
    /// `pager.total`（筛选还停在老分类名上 → SQL 一条都匹配不到 → 共 0 条）。
    #[test]
    fn renamed_or_deleted_category_corrects_the_stale_filter() {
        use focusflow_core::accounting;

        let _guard = guard();
        let (tmp, mut manager, name) = bundled_in("stale_filter", "accounting_plugin.lua");
        accounting::init_db().expect("记账库应能建起来");
        assert!(accounting::add_category("测试分类", "both", &[]) > 0);
        let put = |date: &str, item: &str, cat: &str| {
            accounting::add_expense("支出", item, None, date, 3.0, Some(cat), None, None)
        };
        for (i, d) in ["2026-08-01", "2026-08-02", "2026-08-03"]
            .iter()
            .enumerate()
        {
            assert!(put(d, &format!("测试记录{}", i + 1), "测试分类") > 0);
        }
        for (i, d) in ["2026-08-04", "2026-08-05"].iter().enumerate() {
            assert!(put(d, &format!("饮料{}", i + 1), "食品饮料") > 0);
        }

        // 先把筛选立到"测试分类"上：只有 3 条
        manager
            .plugin_set_field(&name, "f_cat", "测试分类")
            .expect("设置分类筛选");
        assert_eq!(
            select_value(&manager, &name, "f_cat").as_deref(),
            Some("测试分类")
        );
        assert_eq!(
            pager_of(&manager, &name).2,
            3,
            "筛选立住时只看到该分类的 3 条"
        );

        // 通过面板改名 → 悬空
        manager
            .plugin_action(&name, "open_manage")
            .expect("进分类管理");
        manager
            .plugin_action(&name, "m_edit_cat_sel测试分类")
            .expect("选中要改名的分类");
        manager
            .plugin_set_field(&name, "m_name", "测试分类改名")
            .expect("填新名字");
        manager
            .plugin_action(&name, "m_save_edit_cat")
            .expect("保存改名");
        assert!(accounting::get_all_categories()
            .iter()
            .any(|c| c.name == "测试分类改名"));
        assert_eq!(
            select_value(&manager, &name, "f_cat").as_deref(),
            Some("全部"),
            "改完之后筛选里存的还是老名字 → 下拉显示'全部'而列表是空的，两边各说一套"
        );
        assert_eq!(
            pager_of(&manager, &name).2,
            5,
            "校正回来的这一帧必须看得到全部 5 条，不能是共 0 条"
        );

        // 删除分类同理（记录不会被删，所以总数仍是 5）
        manager
            .plugin_set_field(&name, "f_cat", "食品饮料")
            .expect("换一个分类筛");
        assert_eq!(pager_of(&manager, &name).2, 2);
        manager
            .plugin_action(&name, "m_del_cat_sel食品饮料")
            .expect("删除分类");
        assert_eq!(
            select_value(&manager, &name, "f_cat").as_deref(),
            Some("全部"),
            "被删掉的分类名不该留在筛选里"
        );
        assert_eq!(pager_of(&manager, &name).2, 5, "同理：共 0 条就是那条假象");
        assert!(manager.unload_plugin(&name));
        drop(tmp);
    }

    /// A0.7 之一：日期筛选是自由文本，`2026-9-1` 这种不补零的写法过去会**静默漏**。
    ///
    /// `purchase_date >= ?` 是字符串比较，`"2026-9-1" > "2026-09-05"`（字节序里
    /// `'9' > '0'`），于是"从 9 月 1 日"恰好把整个九月上旬筛掉而一句错都不报；
    /// 同一个面板里弹窗的日期控件却是补零的。现在两侧都规范成 `YYYY-MM-DD`，
    /// 认不出来的写法必须点名说"这一条没生效"，而不是继续装成筛过了。
    #[test]
    fn date_range_filter_is_normalised_and_bad_input_is_called_out() {
        use focusflow_core::accounting;

        let _guard = guard();
        let (tmp, mut manager, name) = bundled_in("date_filter", "accounting_plugin.lua");
        accounting::init_db().expect("记账库应能建起来");
        for d in ["2026-08-20", "2026-09-05", "2026-09-15", "2026-10-01"] {
            assert!(
                accounting::add_expense(
                    "支出",
                    &format!("记录{d}"),
                    None,
                    d,
                    1.0,
                    None,
                    None,
                    None
                ) > 0
            );
        }

        // 不补零的起止：过去 total=0，现在应筛出九月那 2 条
        manager
            .plugin_set_field(&name, "f_from", "2026-9-1")
            .expect("填起始日期");
        manager
            .plugin_set_field(&name, "f_to", "2026-9-30")
            .expect("填结束日期");
        assert_eq!(
            pager_of(&manager, &name).2,
            2,
            "2026-9-1 得能筛到 2026-09-05（字符串比较下少一个零就整个漏掉）"
        );
        assert_eq!(
            textinput_value(&manager, &name, "f_from").as_deref(),
            Some("2026-09-01"),
            "输入框该回填成与日期控件同一套口径（补零）"
        );
        assert!(
            labels_of(&manager, &name)
                .iter()
                .all(|t| !t.contains("没生效")),
            "认得出来的写法不该报错: {:?}",
            labels_of(&manager, &name)
        );

        // 认不出来：这条筛选不生效（宁可给全量），并且必须说清是哪一条
        manager
            .plugin_set_field(&name, "f_from", "上个月")
            .expect("填一个不是日期的值");
        let seen = labels_of(&manager, &name);
        assert!(
            seen.iter()
                .any(|t| t.contains("从") && t.contains("没生效")),
            "非法日期必须点名说'哪一条没生效'，实际: {seen:?}"
        );
        assert_eq!(
            pager_of(&manager, &name).2,
            3,
            "非法的起、合法的止：合法那条照旧生效，非法这条不筛"
        );

        // 清空 = 不筛（哨兵值不会被 norm 成 nil 之后误报）
        manager
            .plugin_set_field(&name, "f_from", "")
            .expect("清空起始日期");
        manager
            .plugin_set_field(&name, "f_to", "")
            .expect("清空结束日期");
        assert_eq!(pager_of(&manager, &name).2, 4, "两端都空 = 全量");
        assert!(
            labels_of(&manager, &name)
                .iter()
                .all(|t| !t.contains("没生效")),
            "留空不该报错"
        );
        assert!(manager.unload_plugin(&name));
        drop(tmp);
    }

    /// A0.7 之二：`get_view` 原先"先用未夹的 page 查询、再 clamp_page"，
    /// 于是外部改库/双开把记录删少之后会有一帧"表格空 + 第 3 / 3 页"。
    #[test]
    fn out_of_range_page_is_queried_again_after_clamping() {
        use focusflow_core::accounting;

        let _guard = guard();
        let (tmp, mut manager, name) = bundled_in("page_clamp", "accounting_plugin.lua");
        accounting::init_db().expect("记账库应能建起来");
        for i in 0..25 {
            assert!(
                accounting::add_expense(
                    "支出",
                    &format!("记录{i:02}"),
                    None,
                    "2026-07-01",
                    1.0,
                    None,
                    None,
                    None
                ) > 0
            );
        }
        // 翻到第 3 页（10 条/页 → 3 页）
        for _ in 0..2 {
            manager.plugin_action(&name, "page_next").expect("下一页");
        }
        assert_eq!(pager_of(&manager, &name), (3, 3, 25));
        assert_eq!(first_table_rows(&manager, &name).len(), 5);

        // 外部把库删到只剩 4 条（面板的 page 还是 3 → 必须重查而不是给空表）
        let (all, _) = accounting::get_expenses_page(1, 200, None, None, None, None, None);
        for e in all.iter().skip(4) {
            assert!(accounting::delete_expense(e.id));
        }
        manager.refresh_view(&name).expect("重新出图");
        let (page, pages, total) = pager_of(&manager, &name);
        assert_eq!((page, pages, total), (1, 1, 4), "夹回到唯一那一页");
        assert_eq!(
            first_table_rows(&manager, &name).len(),
            4,
            "同一帧就得给出这 4 条：夹过之后没重查的话，页码写着 1/1 而表格是空的"
        );
        assert!(manager.unload_plugin(&name));
        drop(tmp);
    }

    /// A0.3：番茄钟面板要有「跳过」，而且它和「停止」的落库结果必须不同。
    ///
    /// 宿主早就有作废语义的 `pomodoro_skip`（`pomodoro.rs` 的注释专门讲了这条区别，
    /// 且这是他**明确选定**的丢弃语义），但全仓 `.lua` 零调用 —— 面板只有「停止」，
    /// 而 `take_current` 只要 `actual > 0` 就落库并 `work_finished += 1`，
    /// 于是"工作到一半不想记了"只能按停止，今日汇总跟着谎报"完成 1 个"。
    #[test]
    fn pomodoro_skip_voids_the_session_while_stop_records_it() {
        use focusflow_core::pomodoro;

        let _guard = guard();
        let (tmp, mut manager, name) = bundled_in("pomo_skip", "pomodoro_plugin.lua");
        // 宿主的 `ensure_pomodoro_db` 是进程级闩（本二进制里别的用例已经把它落下了），
        // 这里显式在**本用例的临时目录**里建好 schema，落库才不会被静默丢掉。
        pomodoro::init_db().expect("番茄钟库应能建起来");
        let tick = || std::thread::sleep(std::time::Duration::from_millis(1500));

        assert!(
            button_ids(&manager, &name).iter().any(|b| b == "skip"),
            "面板必须有「跳过」按钮： {:?}",
            button_ids(&manager, &name)
        );

        // 跳过：不落库、不计入今日完成
        manager
            .plugin_action(&name, "start_work")
            .expect("开始工作");
        tick();
        manager.plugin_action(&name, "skip").expect("跳过");
        assert!(
            pomodoro::get_recent_sessions(10).is_empty(),
            "跳过过的这一段不该留下一行"
        );
        assert_eq!(pomodoro::today_summary().0, 0, "跳过不该被数成'完成 1 个'");
        assert!(
            labels_of(&manager, &name)
                .iter()
                .any(|t| t.contains("跳过") && t.contains("不落库")),
            "跳过之后要说清这段作废了"
        );

        // 停止：落库、计入今日完成（这条语义刻意保持不动）
        manager
            .plugin_action(&name, "start_work")
            .expect("再开一个");
        tick();
        manager.plugin_action(&name, "stop").expect("停止");
        let rows = pomodoro::get_recent_sessions(10);
        assert_eq!(
            rows.len(),
            1,
            "停止必须照旧把已计到的这一段落库（跳过那条不能顺手把它也关掉）"
        );
        assert_eq!(pomodoro::today_summary().0, 1);
        assert!(
            labels_of(&manager, &name)
                .iter()
                .any(|t| t.contains("已停止") && t.contains("今日完成")),
            "停止之后要说清这一段算完成"
        );

        // 暂停/继续以前零反馈（`toggle_pause` 的返回值被丢弃、hint 是死代码）
        manager.plugin_action(&name, "start_work").expect("开始");
        manager.plugin_action(&name, "toggle_pause").expect("暂停");
        assert!(
            labels_of(&manager, &name)
                .iter()
                .any(|t| t.contains("已暂停")),
            "暂停必须有回话"
        );
        manager.plugin_action(&name, "toggle_pause").expect("继续");
        assert!(
            labels_of(&manager, &name)
                .iter()
                .any(|t| t.contains("已继续")),
            "继续必须有回话"
        );
        manager
            .plugin_action(&name, "skip")
            .expect("收尾：作废这一段");
        manager
            .plugin_action(&name, "toggle_pause")
            .expect("空闲时点暂停");
        assert!(
            labels_of(&manager, &name)
                .iter()
                .any(|t| t.contains("没有在计时的番茄钟")),
            "空闲时点暂停要说'先开始工作'，而不是分不清的已暂停/已继续"
        );
        assert_eq!(
            pomodoro::get_recent_sessions(10).len(),
            1,
            "以上没有任何一段被记成完成"
        );
        assert!(manager.unload_plugin(&name));
        drop(tmp);
    }

    /// A0.5 之一：番茄钟面板看得到、也改得动时长（以前 `work_min`/`brk_min`
    /// 取出即弃，宿主的 `pomodoro_set_durations` 零调用方）。
    #[test]
    fn pomodoro_panel_shows_and_changes_the_durations() {
        let _guard = guard();
        let (tmp, mut manager, name) = bundled_in("pomo_dur", "pomodoro_plugin.lua");

        assert_eq!(
            keyvalue_of(&manager, &name, "工作时长").as_deref(),
            Some("25 分钟"),
            "默认时长要看得见（值取自宿主状态，不是面板自己写死的）"
        );
        assert_eq!(
            keyvalue_of(&manager, &name, "休息时长").as_deref(),
            Some("5 分钟")
        );

        manager
            .plugin_set_field(&name, "d_work", "40")
            .expect("填工作时长");
        manager
            .plugin_set_field(&name, "d_brk", "10")
            .expect("填休息时长");
        manager
            .plugin_action(&name, "apply_durations")
            .expect("应用时长");
        assert_eq!(
            keyvalue_of(&manager, &name, "工作时长").as_deref(),
            Some("40 分钟"),
            "点了应用就该真的写进计时器"
        );
        assert_eq!(
            keyvalue_of(&manager, &name, "休息时长").as_deref(),
            Some("10 分钟")
        );

        // 越界与非数字都要被拒并说清（宿主的 clamp 只兜住边界，界面得给原因）
        manager
            .plugin_set_field(&name, "d_work", "abc")
            .expect("填一个不是数字的时长");
        manager
            .plugin_action(&name, "apply_durations")
            .expect("应用非法时长不该让动作本身失败");
        assert!(
            labels_of(&manager, &name)
                .iter()
                .any(|t| t.contains("时长没改")),
            "非法时长必须有话，实际: {:?}",
            labels_of(&manager, &name)
        );
        assert_eq!(
            keyvalue_of(&manager, &name, "工作时长").as_deref(),
            Some("40 分钟"),
            "被拒的输入不许改动计时器"
        );

        // 还原成默认，别把这个进程级单例留给后面的用例
        manager
            .plugin_set_field(&name, "d_work", "25")
            .expect("还原");
        manager.plugin_set_field(&name, "d_brk", "5").expect("还原");
        manager
            .plugin_action(&name, "apply_durations")
            .expect("还原时长");
        assert!(manager.unload_plugin(&name));
        drop(tmp);
    }

    /// A0.5 之二：定时任务面板要真能**编辑**任务。
    ///
    /// `scheduler_plugin.lua` 的 `edit_id` 从未被使用、宿主的 `scheduler_update` 零
    /// 调用方 → GUI 里根本不存在"编辑任务"，只能删了重建（而新建被目标白名单卡着）。
    #[test]
    fn scheduler_panel_can_edit_a_task() {
        use focusflow_core::scheduler;

        let _guard = guard();
        let (tmp, mut manager, name) = bundled_in("sched_edit", "scheduler_plugin.lua");
        scheduler::init_db().expect("定时任务库应能建起来");
        // enabled=false：这条测试绝不能起记事本（连「添加示例任务」那个动作都不点）
        let id = scheduler::add_task(
            "改名前",
            "C:\\Windows\\notepad.exe",
            "",
            "daily",
            "09:00",
            false,
        )
        .expect("夹具任务应能入库");
        manager.refresh_view(&name).expect("重新出图");

        let actions: Vec<String> = view_of(&manager, &name)
            .into_iter()
            .find_map(|w| match w {
                Widget::Table { actions, .. } => {
                    Some(actions.into_iter().map(|(prefix, _)| prefix).collect())
                }
                _ => None,
            })
            .expect("任务表格应有行内按钮");
        assert!(
            actions.iter().any(|a| a == "edit_"),
            "行内必须有「编辑」： {:?}",
            actions
        );

        manager
            .plugin_action(&name, &format!("edit_{id}"))
            .expect("点编辑");
        let modal = view_of(&manager, &name)
            .into_iter()
            .find(|w| matches!(w, Widget::ModalForm { id, .. } if id == "edit_modal"))
            .expect("视图里应有编辑弹窗");
        assert!(modal_open(&modal), "点编辑必须把弹窗开起来");
        assert_eq!(
            modal_field(&modal, "ed_name").as_deref(),
            Some("改名前"),
            "弹窗要预填任务现值，不然等于让用户重打一遍"
        );
        assert_eq!(
            modal_field(&modal, "ed_target").as_deref(),
            Some("C:\\Windows\\notepad.exe")
        );
        assert_eq!(modal_field(&modal, "ed_enabled").as_deref(), Some("0"));

        manager
            .plugin_set_field(&name, "ed_name", "改名后")
            .expect("改名字");
        manager.plugin_action(&name, "save_edit").expect("保存编辑");
        let tasks = scheduler::get_all_tasks();
        let edited = tasks.iter().find(|t| t.id == id).expect("任务该还在");
        assert_eq!(edited.name, "改名后", "scheduler_update 必须真的被走到");
        assert!(!edited.enabled, "编辑不该顺手把任务启用（会起记事本）");
        assert!(
            labels_of(&manager, &name)
                .iter()
                .any(|t| t.contains("已更新任务")),
            "保存成功要有回话"
        );
        assert!(
            open_modal_ids(&manager, &name)
                .iter()
                .all(|m| m != "edit_modal"),
            "保存之后编辑弹窗要关闭"
        );

        // 被拒时把宿主的原话说出来，并且留着弹窗让人改
        manager
            .plugin_action(&name, &format!("edit_{id}"))
            .expect("再开一次编辑");
        manager
            .plugin_set_field(&name, "ed_time", " nonsense : : ")
            .expect("填一个非法调度");
        manager
            .plugin_action(&name, "save_edit")
            .expect("非法调度的保存不该让动作失败");
        let seen = labels_of(&manager, &name);
        assert!(
            seen.iter()
                .any(|t| t.contains("更新失败") && t.contains("调度")),
            "被宿主拒绝时要把原因搬上面板，实际: {seen:?}"
        );
        assert!(
            open_modal_ids(&manager, &name).contains(&"edit_modal".to_string()),
            "被拒时弹窗要留着（草稿还在里面）"
        );
        assert_eq!(
            scheduler::get_all_tasks()
                .iter()
                .find(|t| t.id == id)
                .expect("还在")
                .name,
            "改名后",
            "被拒的那次保存一个字都不该写进去"
        );

        manager
            .plugin_action(&name, "cancel_edit")
            .expect("取消编辑");
        assert!(
            open_modal_ids(&manager, &name)
                .iter()
                .all(|m| m != "edit_modal"),
            "取消之后弹窗要关"
        );
        manager
            .plugin_action(&name, "edit_999999")
            .expect("点一个不存在 id 的编辑");
        assert!(
            labels_of(&manager, &name)
                .iter()
                .any(|t| t.contains("编辑失败")),
            "列表里没有这一条时也要说清"
        );
        assert!(manager.unload_plugin(&name), "卸载要回收调度线程");
        drop(tmp);
    }
}
