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
        let files = manager.discover().expect("内置插件目录该读得动");
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

        let files = manager.discover().expect("内置插件目录该读得动");
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
        // 上面那圈里 Edge 插件被点过 `refresh`，见 `wait_edge_refresh_settled`。
        wait_edge_refresh_settled();
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

        let files = manager.discover().expect("内置插件目录该读得动");
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

        let files = manager.discover().expect("内置插件目录该读得动");
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

    /// 临时根目录：panic 也要回收。
    ///
    /// `/tmp/ff_*` 是门禁基线盯的那一项，泄漏一个就报不出 `TEMP_FLAT`；
    /// 而 `app_dir` 是进程级的，用例炸在半路不把仓库目录还回去，后面的用例
    /// 就接手一个已经删掉的目录（同文件里那些用例都自己 `set_app_dir`，
    /// 但顺序与并行不由我们挑）。
    struct TempRoot(std::path::PathBuf);

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
            if let Ok(dir) = std::env::current_dir() {
                paths::set_app_dir(&dir);
            }
            db::queries::invalidate_years_cache();
        }
    }

    #[test]
    fn hot_reload_request() {
        focusflow_core::logger::init_logging();
        let _guard = guard();
        // 这一条以前把 `app_dir` 指到仓库目录（`current_dir`），然后**直接改仓库里那份
        // `plugins/stats_overview.lua`** 来动 mtime，用例末尾再写回去。两处坏：
        // ① 进程被杀、或 `fs::write` / 中途任何一句 panic，那句"写回去"就轮不到 ——
        //    签入的插件源文件于是带着 `-- hot reload test` 留在工作树里（实测脏过一次，
        //    是并行杀进程那一轮留下的），而那是会随包发给用户的字节；
        // ② 它和其它并行用例抢同一块地盘（同一份源文件既被读又被写）。
        // 现在整份搬进临时目录，改的是自己的副本，仓库字节全程不动。
        let root = std::env::temp_dir().join(format!("ff_hotreload_{}", std::process::id()));
        std::fs::create_dir_all(root.join("plugins")).unwrap();
        let shipped = std::env::current_dir()
            .unwrap()
            .join("plugins/stats_overview.lua");
        let copy = root.join("plugins/stats_overview.lua");
        std::fs::copy(&shipped, &copy).unwrap();
        let _root = TempRoot(root.clone());

        paths::set_app_dir(&root);
        db::queries::invalidate_years_cache();

        let config: &'static FocusFlowConfig = Box::leak(Box::new(
            FocusFlowConfig::load(root.join("config.ini")).unwrap(),
        ));
        let database = db::Database::init_readonly();
        let mut manager = PluginManager::new(config, database);

        // 加载
        let file = manager
            .discover()
            .expect("内置插件目录该读得动")
            .into_iter()
            .find(|f| f.ends_with("stats_overview.lua"))
            .unwrap();
        manager.load_plugin(&file).unwrap();

        // 启用热重载（检测线程）
        manager.enable_hot_reload();

        // 等待检测线程完成首次基线扫描（2s 周期）
        std::thread::sleep(std::time::Duration::from_millis(2500));

        // 修改文件触发重载请求（追加注释改变 mtime）—— 改的是本次自己复制出来的那份
        let path = manager.get_plugin("统计速览").unwrap().file_path.clone();
        assert_eq!(
            path, copy,
            "被改的必须是临时副本，不能是仓库里那份签入的文件"
        );
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
        // 仓库里那份一个字节都不许动 —— 这条断言是这次改动真正的守卫：
        // 上面若又去写 `current_dir()/plugins/...`，这里会立刻读出差异并红。
        assert_eq!(
            std::fs::read_to_string(&shipped).unwrap(),
            std::fs::read_to_string(&copy).unwrap(),
            "仓库里签入的插件源文件被用例改了字节"
        );

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
            .expect("内置插件目录该读得动")
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
        // 「上次执行」必须是看得见的一列：面板那句"改了时刻会连同「上次执行」一起重置"
        // 与宿主那句"连续三次没能启动，本时段内不再重试"都假设用户看得见它，而这一列
        // 以前从来没被渲染过 —— 任务到底跑没跑、是不是被退避按住，只能靠翻日志知道。
        assert!(
            headers.iter().any(|h| h.contains("上次执行")),
            "任务列表要有「上次执行」这一列，实得表头: {headers:?}"
        );
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
            .expect("内置插件目录该读得动")
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
            .expect("内置插件目录该读得动")
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

    /// 编辑把一条记录改到另一页之后，列表要跟着回到能看见它的那一页。
    ///
    /// 列表按 `purchase_date DESC, id DESC` 排，而编辑弹窗能改日期：把第 2 页的
    /// 一条旧记录改到今天，它会跳到第 1 页顶部。旧实现只回一句「已更新记录 #N」
    /// 而页码不动 —— 用户看到的是「更新成功 + 列表里没有这条」，多半以为没存上，
    /// 与 `add_record` 当年那条「保存过了却没这条」同一族（那条已修：当天记录跳回
    /// 第 1 页、补记的记录明说排在后面页）。
    #[test]
    fn editing_a_record_onto_another_page_moves_the_view_with_it() {
        use focusflow_core::plugins::Widget;

        fn widgets(manager: &PluginManager, name: &str) -> Vec<Widget> {
            manager
                .get_plugin(name)
                .and_then(|p| p.view.as_ref())
                .map(|v| v.widgets.clone())
                .unwrap_or_default()
        }
        fn list_has(manager: &PluginManager, name: &str, needle: &str) -> bool {
            widgets(manager, name).iter().any(|w| match w {
                Widget::Table { rows, .. } => {
                    rows.iter().any(|r| r.iter().any(|c| c.contains(needle)))
                }
                _ => false,
            })
        }
        fn pager_page(manager: &PluginManager, name: &str) -> i64 {
            widgets(manager, name)
                .iter()
                .find_map(|w| match w {
                    Widget::Pager { page, .. } => Some(*page),
                    _ => None,
                })
                .expect("记录列表应有分页条")
        }

        let _guard = guard();
        let (_tmp, mut manager, name) = bundled_in("acct_edit_page", "accounting_plugin.lua");
        // 宿主那侧的 ensure_accounting_db 是"本进程只跑一次"的形状，全量并行时
        // 闩可能已被别的用例用掉 ⇒ 本目录里没有表。显式建库是用例的前置。
        focusflow_core::accounting::init_db().expect("记账库应能建起来");
        let today = chrono::Local::now()
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();

        // 10 条今天的（第 1 页整页）+ 1 条 2020 年的（第 2 页；id 更大 ⇒ 改成今天后排最前）
        for i in 0..10 {
            let id = focusflow_core::accounting::add_expense(
                "支出",
                &format!("今日{i}"),
                None,
                &today,
                10.0,
                None,
                None,
                None,
            );
            assert!(id > 0, "夹具：今天的记录应能写入");
        }
        let old_id = focusflow_core::accounting::add_expense(
            "支出",
            "陈年旧账",
            None,
            "2020-01-01",
            5.0,
            None,
            None,
            None,
        );
        assert!(old_id > 0, "夹具：旧记录应能写入");

        manager
            .plugin_action(&name, "page_next")
            .expect("翻页动作不该失败");
        assert_eq!(pager_page(&manager, &name), 2, "前置条件：应已翻到第 2 页");
        assert!(
            list_has(&manager, &name, "陈年旧账"),
            "前置条件：第 2 页应能看到这条旧记录"
        );

        manager
            .plugin_action(&name, &format!("edit_{old_id}"))
            .expect("打开编辑弹窗不该失败");
        manager
            .plugin_set_field(&name, "d_date", &today)
            .expect("写回日期不该失败");
        manager
            .plugin_action(&name, "save_edit")
            .expect("保存不该失败");

        assert_eq!(
            pager_page(&manager, &name),
            1,
            "改到今天后它已不在第 2 页，页码必须回到第 1 页 —— 否则「更新成功」与列表里没有它同时发生"
        );
        assert!(
            list_has(&manager, &name, "陈年旧账"),
            "第 1 页必须能看到刚改过的这条（它 id 更大，按 id 倒序排最前）"
        );
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

    /// 回归：宿主 `accounting_category_add` 必须回 `(id, 原因串)` 两个值。
    ///
    /// 面板本来就读两个（`local ok, msg = focusflow.accounting_category_add(...)`），
    /// 而宿主原来只回一个 id ⇒ `msg` 恒为 nil，于是：
    ///
    /// - 重名添加：`INSERT OR IGNORE` + 回读 id 拿到**已存在那一条**的 id（> 0）
    ///   ⇒ 面板印「分类 [x] 已添加」，而这次传的类型/子分类一个字都没落库；
    /// - 其他失败：面板只能说"添加分类失败：" 四个字。
    ///
    /// 这一族在 Rust 侧（`accounting::add_category`）已有用例，但宿主这两条腿
    /// 只有真过一遍 Lua 才测得到 —— 就是这条。
    #[test]
    fn host_accounting_category_add_returns_a_reason() {
        let _guard = guard();
        let tmp = paths::test_app_dir("host_cat_reason");
        db::queries::invalidate_years_cache();
        // 宿主那侧的 `ensure_accounting_db()` 是"本进程只跑一次"的形状：单跑这条时
        // 它会在本用例的临时目录里建表，而全量并行跑时**前一条用例已经把它用掉了**，
        // 于是本用例的目录里没有 `categories` ⇒ "no such table: categories"。
        // 记账建库本来就是用例的前置（同文件 `renamed_or_deleted_category...` 也这么写）。
        focusflow_core::accounting::init_db().expect("记账库应能建起来");
        let config: &'static FocusFlowConfig = Box::leak(Box::new(
            FocusFlowConfig::load(tmp.path().join("config.ini")).expect("临时配置应能载入"),
        ));
        let lua = mlua::Lua::new();
        focusflow_core::plugins::host::register_host_api(
            &lua,
            config,
            db::Database::init_readonly(),
            "host_cat_reason",
        )
        .expect("宿主 API 应能注册");

        let call = |name: &str, ctype: &str| -> (i64, String) {
            lua.load(format!(
                "local ok, msg = focusflow.accounting_category_add('{name}', '{ctype}') \
                 return ok, msg or '<nil>'"
            ))
            .eval()
            .expect("accounting_category_add 该回两个值")
        };

        // 第一次正常添加：id > 0，原因串为空
        let (id, msg) = call("宠物", "expense");
        assert!(id > 0, "首次添加该回真实 id，实得 {id} / {msg}");
        assert_eq!(msg, "", "成功时原因串该是空的");

        // 重名：必须报"已存在"，不能回一个已存在记录的 id 让面板印「已添加」
        let (dup_id, dup_msg) = call("宠物", "income");
        assert_eq!(dup_id, -1, "重名不能回已存在那一条的 id，实得 {dup_id}");
        assert!(
            dup_msg.contains("已经存在"),
            "要说清是重名（传的 ctype 没落库），实得: {dup_msg}"
        );
        // 那次"添加"不能把已有分类的类型改掉
        assert_eq!(
            focusflow_core::accounting::category_type("宠物").as_deref(),
            Some("expense"),
            "重名那次传的 income 不该落库"
        );

        // 空名：也要有话可说（原来 msg 恒为 nil ⇒ 界面只剩四个字）
        let (empty_id, empty_msg) = call("   ", "both");
        assert_eq!(empty_id, -1, "空分类名不该被建出来");
        assert!(
            !empty_msg.is_empty() && empty_msg != "<nil>",
            "失败必须带原因，实得: {empty_msg:?}"
        );
    }

    /// 等 Edge 那一轮后台刷新真的收尾，再让用例继续往下走。
    ///
    /// `edge-refresh` 线程是 detach 的，而它落盘用的 `data_dir()` 取的是**当下**的
    /// 全局 `app_dir`：不等它，它会写进下一条用例刚建好的临时目录（实测在
    /// `ff_reset_sched_*` 里长出 `focusflow_edge_history.db`），于是那个目录在
    /// `TestAppDir::drop` 时删不掉（句柄还在），或被删掉之后又重建 ——
    /// 每跑一轮就往 `%TEMP%` 留一个目录。
    fn wait_edge_refresh_settled() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while focusflow_core::edge_history::refresh_state() == "running"
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_ne!(
            focusflow_core::edge_history::refresh_state(),
            "running",
            "后台刷新线程没在 15 秒内收尾，它之后会把数据写进下一个用例的临时目录"
        );
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

    /// 统计结果弹窗（月度汇总 / 分类盈亏 / 距今多久）正文里那份报告。
    ///
    /// 它挂在 `Widget::ModalForm.content` 上，`labels_of` 看不见 —— 想断"面板说了什么"
    /// 就得从这里读。
    fn report_of(manager: &PluginManager, name: &str) -> String {
        view_of(manager, name)
            .into_iter()
            .find_map(|w| match w {
                Widget::ModalForm { id, content, .. } if id == "result_modal" => Some(content),
                _ => None,
            })
            .expect("视图里应有统计结果弹窗")
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
        wait_edge_refresh_settled();
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
            .0
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

    /// 分类清单**没读全**时，面板既不许据此清掉用户的筛选条件，也要把"这次没读全"说出来。
    ///
    /// 上一条用例守的是"读全了要校正"，这一条守它的另一半：读不全时什么都不许改。
    /// 少了这一半，一次瞬时读不动（库被别的连接握着、某一行是坏值）就足以让用户看到
    /// 「我的分类和筛选条件都没了」—— 而库里一条都没少。
    #[test]
    fn an_incomplete_category_list_neither_resets_the_filter_nor_stays_silent() {
        use focusflow_core::accounting;

        let _guard = guard();
        let (tmp, mut manager, name) = bundled_in("stale_filter_gap", "accounting_plugin.lua");
        accounting::init_db().expect("记账库应能建起来");
        assert!(accounting::add_category("测试分类", "both", &[]) > 0);
        // 「食品饮料」是 `init_db` 的预置分类：改名要落在**读得出来**的那个分类上，用它
        assert!(accounting::get_all_categories()
            .0
            .iter()
            .any(|c| c.name == "食品饮料"));
        manager
            .plugin_set_field(&name, "f_cat", "测试分类")
            .expect("设置分类筛选");
        assert_eq!(
            select_value(&manager, &name, "f_cat").as_deref(),
            Some("测试分类"),
            "夹具：筛选立在测试分类上"
        );

        // 把「测试分类」这一行换成读不出来的值（`subs` 是动态类型列）：清单从此少一条，
        // 而"少这一条"的原因是被挡住，不是被删除。
        let bake = |subs: rusqlite::types::Value| {
            let conn =
                rusqlite::Connection::open(accounting::db_path()).expect("夹具应能打开记账库");
            let n = conn
                .execute(
                    "UPDATE categories SET subs=?1 WHERE name='测试分类'",
                    [subs],
                )
                .expect("夹具：改 subs 这一列");
            assert_eq!(n, 1, "夹具：那一行还在库里");
        };
        bake(rusqlite::types::Value::Blob("充值".as_bytes().to_vec()));

        // 走一次会调 `reset_stale_filters` 的动作：改的是读得出来的那个分类
        manager
            .plugin_action(&name, "open_manage")
            .expect("进分类管理");
        manager
            .plugin_action(&name, "m_edit_cat_sel食品饮料")
            .expect("选中要改名的分类");
        manager
            .plugin_set_field(&name, "m_name", "食品饮料改名")
            .expect("填新名字");
        manager
            .plugin_action(&name, "m_save_edit_cat")
            .expect("保存改名");

        assert_eq!(
            select_value(&manager, &name, "f_cat").as_deref(),
            Some("测试分类"),
            "清单没读全时不许改动用户的筛选条件：库里那一行还在，只是这次读不出来"
        );
        assert!(
            labels_of(&manager, &name)
                .iter()
                .any(|t| t.contains("分类清单这次没读全")),
            "下拉里少了名字这件事要说给用户，不能让它看着像被删了: {:?}",
            labels_of(&manager, &name)
        );

        // 读全之后照常校正：这一腿证明上面那条不是"永远不动"。
        // 删掉的正是在筛的那个分类，读得出来了，名字就该从筛选里清掉。
        bake(rusqlite::types::Value::Text(String::new()));
        manager
            .plugin_action(&name, "m_del_cat_sel测试分类")
            .expect("删除分类");
        assert_eq!(
            select_value(&manager, &name, "f_cat").as_deref(),
            Some("全部"),
            "读全之后，被删掉的分类名照常从筛选里清掉"
        );
        assert!(manager.unload_plugin(&name));
        drop(tmp);
    }

    /// 读不动的账不许被面板印成一份「零报告」。
    ///
    /// 症状现量：库里当月明明有记录，`open()` 那一轮读不动时结果弹窗写的是
    /// 「总支出 0.00 / 记录数 0 /（本月暂无分类明细）」和「总投入 0.00 总赚取 0.00（暂无数据）」
    /// —— 两句都是关于用户账本的结论。宿主现在多交一个「没读全的原因」，
    /// 面板只在它为空时才说那两句，否则说明原因（用例的最后两条腿分别钉这两边）。
    #[test]
    fn an_unreadable_ledger_is_not_reported_as_a_zero_month() {
        use focusflow_core::accounting;

        let _guard = guard();
        let (tmp, mut manager, name) = bundled_in("summary_gap", "accounting_plugin.lua");
        accounting::init_db().expect("记账库应能建起来");
        assert!(accounting::add_category("测试餐饮", "expense", &[]) > 0);
        assert!(
            accounting::add_expense(
                "支出",
                "午饭",
                None,
                "2026-08-01",
                12.5,
                Some("测试餐饮"),
                None,
                None
            ) > 0,
            "夹具：记一笔"
        );

        // 读得出来的那一轮：合计照说，且不许多一句"没读全"（这一腿挡住"永远不说数"）
        manager
            .plugin_action(&name, "cat_profit")
            .expect("分类盈亏");
        let ok = report_of(&manager, &name);
        assert!(ok.contains("测试餐饮"), "正常报告该列出分类:\n{ok}");
        assert!(!ok.contains("没读全"), "读全了不许报任何保留:\n{ok}");

        // 把库换成一份读不出来的字节（半截文件 / 被同步盘换成的占位内容）
        std::fs::write(accounting::db_path(), "这不是一个 SQLite 文件").expect("写坏文件");
        manager
            .plugin_action(&name, "cat_profit")
            .expect("读不出来不该让动作本身失败");
        let bad = report_of(&manager, &name);
        assert!(bad.contains("这份数没读全"), "要说清这次没读到:\n{bad}");
        assert!(
            !bad.contains("（暂无数据）"),
            "读不动不许说成没有数据:\n{bad}"
        );
        assert!(!bad.contains("总投入："), "不许给一份零报告:\n{bad}");

        manager
            .plugin_action(&name, "monthly_detail")
            .expect("月度汇总");
        let monthly = report_of(&manager, &name);
        assert!(monthly.contains("这份数没读全"), "{monthly}");
        assert!(
            !monthly.contains("（本月暂无分类明细）"),
            "读不动时不许断定本月没有明细:\n{monthly}"
        );

        assert!(manager.unload_plugin(&name));
        drop(tmp);
    }

    /// 「距今多久」按下去一定要出声音：算不出来的那几条要说是哪一条、为什么。
    ///
    /// 面板原先只在拼出了行时才开结果弹窗，于是「这条没填日期」「这条已被别处删掉」
    /// 「库正被别处占着」三种结局都是同一张"按钮坏了"的脸。宿主现在把没答上的原因一起
    /// 交回来，面板无论有没有命中都开弹窗说话 —— 所以这一条既钉文案，也钉弹窗开没开。
    #[test]
    fn the_days_ago_button_answers_even_when_it_cannot_compute() {
        use focusflow_core::accounting;

        let _guard = guard();
        let (tmp, mut manager, name) = bundled_in("days_ago_voice", "accounting_plugin.lua");
        accounting::init_db().expect("记账库应能建起来");
        assert!(accounting::add_category("餐饮", "expense", &[]) > 0);
        let good = accounting::add_expense(
            "支出",
            "午饭",
            None,
            "2026-01-05",
            1.0,
            Some("餐饮"),
            None,
            None,
        );
        assert!(good > 0, "夹具：这一笔要写进去");

        // 正常那一条：报告照常给"距今"，也不许多一句没算出来
        manager
            .plugin_action(&name, &format!("days_{good}"))
            .expect("距今多久");
        let ok = report_of(&manager, &name);
        assert!(
            ok.contains("距今") && ok.contains("午饭"),
            "正常报告:\n{ok}"
        );
        assert!(!ok.contains("没算出来"), "{ok}");

        // 把这条的日期改成空白：算不出来时要点名是哪一条、为什么，而且弹窗必须开
        {
            let conn = rusqlite::Connection::open(accounting::db_path()).expect("打开记账库");
            conn.execute("UPDATE expenses SET purchase_date='' WHERE id=?1", [good])
                .unwrap();
        }
        manager
            .plugin_action(&name, &format!("days_{good}"))
            .expect("算不出来也不该让动作失败");
        let bad = report_of(&manager, &name);
        assert!(
            bad.contains("没填日期") && bad.contains(&format!("id={good}")),
            "{bad}"
        );
        assert!(
            open_modal_ids(&manager, &name).contains(&"result_modal".to_string()),
            "没答上时弹窗必须开：旧行为是一行都不拼、弹窗也不开，按钮看着就是坏了"
        );

        // 库里没有的那个 id：同样要说话
        manager
            .plugin_action(&name, "days_999999")
            .expect("查无此条也不该没反应");
        let gone = report_of(&manager, &name);
        assert!(gone.contains("库里没有"), "查无此条也要说一句:\n{gone}");

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

    /// 记账面板的三个读数：分页条上的页码、表格里有没有某条、以及所有会说给人听的话。
    mod acct_reading {
        use focusflow_core::plugins::{PluginManager, Widget};

        pub fn widgets(manager: &PluginManager, name: &str) -> Vec<Widget> {
            manager
                .get_plugin(name)
                .and_then(|p| p.view.as_ref())
                .map(|v| v.widgets.clone())
                .unwrap_or_default()
        }

        pub fn list_has(manager: &PluginManager, name: &str, needle: &str) -> bool {
            widgets(manager, name).iter().any(|w| match w {
                Widget::Table { rows, .. } => {
                    rows.iter().any(|r| r.iter().any(|c| c.contains(needle)))
                }
                _ => false,
            })
        }

        pub fn pager_page(manager: &PluginManager, name: &str) -> i64 {
            widgets(manager, name)
                .iter()
                .find_map(|w| match w {
                    Widget::Pager { page, .. } => Some(*page),
                    _ => None,
                })
                .expect("记录列表应有分页条")
        }

        /// 标签、表格单元与弹窗正文拼成一句，供"这话到底有没有说"的断言用。
        pub fn sayings(manager: &PluginManager, name: &str) -> String {
            fn walk(ws: &[Widget], out: &mut Vec<String>) {
                for w in ws {
                    match w {
                        Widget::Label(t) => out.push(t.clone()),
                        Widget::Row { children } => walk(children, out),
                        Widget::Table { rows, .. } => {
                            for r in rows {
                                for c in r {
                                    out.push(c.clone());
                                }
                            }
                        }
                        Widget::ModalForm {
                            content,
                            widgets: inner,
                            ..
                        } => {
                            if !content.is_empty() {
                                out.push(content.clone());
                            }
                            walk(inner, out);
                        }
                        _ => {}
                    }
                }
            }
            let mut out = Vec::new();
            walk(&widgets(manager, name), &mut out);
            out.join(" | ")
        }
    }

    /// 记一笔**明天**的账：列表按 purchase_date 降序，它排在全表最前。
    /// 旧判据写的是「日期不是今天就算补记」，于是未来日期也被当成过去的账：
    /// 页码不动（人还停在第 2 页），提示还要人「用日期筛选去后面的页」——
    /// 而它就在眼前这一页的第一行，照着提示翻反而找不到。
    #[test]
    fn a_future_dated_record_lands_on_the_first_page() {
        use acct_reading::{list_has, pager_page, sayings};

        let _guard = guard();
        let (_tmp, mut manager, name) = bundled_in("future_add", "accounting_plugin.lua");
        focusflow_core::accounting::init_db().expect("记账库应能建起来");
        let today = chrono::Local::now().date_naive();
        let today_s = today.format("%Y-%m-%d").to_string();
        let tomorrow = (today + chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();

        // 11 条才够出第 2 页（PAGE_SIZE=10），否则 `page_next` 会被 clamp 回第 1 页
        for i in 0..11 {
            let id = focusflow_core::accounting::add_expense(
                "支出",
                &format!("今日{i}"),
                None,
                &today_s,
                10.0,
                None,
                None,
                None,
            );
            assert!(id > 0, "夹具：今天的记录应能写入");
        }
        manager
            .plugin_action(&name, "page_next")
            .expect("翻页动作不该失败");
        assert_eq!(pager_page(&manager, &name), 2, "前置条件：应已翻到第 2 页");

        manager
            .plugin_set_field(&name, "d_item", "预定的账")
            .expect("填名称不该失败");
        manager
            .plugin_set_field(&name, "d_amount", "12")
            .expect("填金额不该失败");
        manager
            .plugin_set_field(&name, "d_date", &tomorrow)
            .expect("填明天不该失败");
        manager
            .plugin_action(&name, "add_record")
            .expect("保存不该失败");

        assert_eq!(
            pager_page(&manager, &name),
            1,
            "明天的记录排在全表最前，页码必须回到第 1 页"
        );
        assert!(list_has(&manager, &name, "预定的账"), "第 1 页就该看见它");
        let said = sayings(&manager, &name);
        assert!(
            !said.contains("排在后面页"),
            "提示不许把人支去后面的页: {said}"
        );

        // 反向腿：真正的补记（过去的日期）仍然不跳页，并且说明它在后面的页
        manager
            .plugin_set_field(&name, "d_item", "补记的旧账")
            .expect("填名称不该失败");
        manager
            .plugin_set_field(&name, "d_amount", "7")
            .expect("填金额不该失败");
        manager
            .plugin_set_field(&name, "d_date", "2020-01-01")
            .expect("填过去的日期不该失败");
        manager
            .plugin_action(&name, "add_record")
            .expect("保存不该失败");
        let said2 = sayings(&manager, &name);
        assert!(
            said2.contains("排在后面页"),
            "补记要说清它在后面的页: {said2}"
        );
        assert!(
            !list_has(&manager, &name, "补记的旧账"),
            "第 1 页看不到这条（它是 2020 年的）"
        );
    }

    /// 删除没删成要在面板上讲出来。
    /// 旧实现只写 `focusflow.log`，界面一个字不写，而那几行照样挂在列表里 ——
    /// 用户看不出「点了没生效」，只会再点一次删除。
    #[test]
    fn a_delete_that_changed_nothing_says_so() {
        use acct_reading::{list_has, sayings};

        let _guard = guard();
        let (_tmp, mut manager, name) = bundled_in("del_silent", "accounting_plugin.lua");
        focusflow_core::accounting::init_db().expect("记账库应能建起来");
        let today = chrono::Local::now()
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();
        let id = focusflow_core::accounting::add_expense(
            "支出",
            "要删掉的这条",
            None,
            &today,
            3.0,
            None,
            None,
            None,
        );
        assert!(id > 0, "夹具：记录应能写入");

        manager
            .plugin_action(&name, "del_999999")
            .expect("动作本身不该失败（失败是数据层的结论）");
        let said = sayings(&manager, &name);
        assert!(
            said.contains("没删掉"),
            "一条都没删掉时不许一片安静: {said}"
        );

        // 反向腿：真删掉了就报「已删除 N 条」，不许把成功也说成失败
        manager
            .plugin_action(&name, &format!("del_{id}"))
            .expect("删除动作不该失败");
        assert!(
            !list_has(&manager, &name, "要删掉的这条"),
            "删掉的这条要从列表里消失"
        );
        let said2 = sayings(&manager, &name);
        assert!(
            !said2.contains("没删掉"),
            "全部删成就不要把成功也说成失败: {said2}"
        );
    }

    /// 两条「分类改名」的入口必须做同一件前置：先 trim、空名就地拦下。
    /// 「添加分类」早就这么改了，重命名那半边漏下 —— 宿主是 trim 之后才落库，
    /// 而面板把「选中的分类」设成没 trim 的那个串，于是它指向一条不存在的记录：
    /// `reset_stale_filters` 把 m_cat 清成空，右侧子分类静默变空，
    /// 接着「添加子分类」报「请先在左侧分类列表点击选中一个分类」。
    #[test]
    fn both_category_rename_paths_trim_the_name() {
        let src = include_str!("../plugins/accounting_plugin.lua");
        let at = src
            .find("m_save_edit_cat")
            .expect("重命名分类那条动作找不到了");
        let body = &src[at..at + 900];
        assert!(
            body.contains("m_name = (m_name or \"\"):match(\"^%s*(.-)%s*$\")"),
            "重命名没有像添加那样先 trim（选中的分类会指向一条不存在的记录）"
        );
        assert!(
            body.contains("分类名不能是空的"),
            "trim 之后还要就地拦下空名，否则空分类被写进库"
        );
    }

    /// 空闲时按「停止」，面板不许说「这一段按完成落库、会算进今日完成」。
    /// 宿主原先只回 `()`，面板无从分辨三种结局（没在计时 / 一秒都没计到 / 写库没成），
    /// 于是开着面板直接点停止也当着用户的面说一句没发生过的账。
    #[test]
    fn stopping_an_idle_pomodoro_does_not_claim_a_landed_session() {
        use focusflow_core::plugins::Widget;

        fn sayings(manager: &PluginManager, name: &str) -> String {
            fn walk(ws: &[Widget], out: &mut Vec<String>) {
                for w in ws {
                    match w {
                        Widget::Label(t) => out.push(t.clone()),
                        Widget::Row { children } => walk(children, out),
                        Widget::ModalForm {
                            content,
                            widgets: inner,
                            ..
                        } => {
                            if !content.is_empty() {
                                out.push(content.clone());
                            }
                            walk(inner, out);
                        }
                        _ => {}
                    }
                }
            }
            let ws = manager
                .get_plugin(name)
                .and_then(|p| p.view.as_ref())
                .map(|v| v.widgets.clone())
                .unwrap_or_default();
            let mut out = Vec::new();
            walk(&ws, &mut out);
            out.join(" | ")
        }

        let _guard = guard();
        let (_tmp, mut manager, name) = bundled_in("pomo_stop_hint", "pomodoro_plugin.lua");
        focusflow_core::pomodoro::init_db().ok();

        manager
            .plugin_action(&name, "stop")
            .expect("停止动作不该失败");
        let said = sayings(&manager, &name);
        assert!(
            said.contains("没有在计时的番茄钟"),
            "空闲停止要说清什么都没结束: {said}"
        );
        assert!(
            !said.contains("会算进「今日完成」"),
            "空闲时不许承诺计入今日完成: {said}"
        );

        // 反向腿：刚开始就停（一秒都没计到）同样什么都没落
        manager
            .plugin_action(&name, "start_work")
            .expect("开始工作不该失败");
        manager
            .plugin_action(&name, "stop")
            .expect("停止动作不该失败");
        let said2 = sayings(&manager, &name);
        assert!(
            said2.contains("一秒都没计到"),
            "刚开始就停要说清这一段没计到时间: {said2}"
        );
        assert!(
            !said2.contains("会算进「今日完成」"),
            "什么都没落库时不许说算进今日完成: {said2}"
        );
    }
}
