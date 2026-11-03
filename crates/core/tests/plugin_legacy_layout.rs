//! 回滚开关那一档的端到端判据（方案 3.10 的 `[plugins] legacy_layout`，验收第 20 条）。
//!
//! 这一条补的是本轮核账时发现的半边覆盖：`paths::plugin_scan_dirs` 自己那两条断言只说了
//! "开关打开时返回哪几个目录"，而**开关真的打开过一遍加载路径**这件事没人走过 ——
//! 那个形状的症状是"runbook 让用户退回旧行为，退回去却什么都扫不到"，而尺子那头的测试
//! 全绿。两条臂各盯一处：开关开 ⇒ 只认旧的平铺目录、那份的身份是"回滚分支"；
//! 开关关（出厂默认）⇒ 平铺目录根本不在扫描之列，别留一份"看起来还在用旧目录"的假象。

#![cfg(feature = "test-utils")]

use std::path::Path;
use std::sync::Mutex;

use focusflow_core::config::FocusFlowConfig;
use focusflow_core::db;
use focusflow_core::paths;
use focusflow_core::plugins::manager::{Discovered, PluginManager};
use focusflow_core::plugins::PluginSource;

static LOCK: Mutex<()> = Mutex::new(());

fn guard() -> std::sync::MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn write_lua(path: &Path, name: &str) {
    std::fs::create_dir_all(path.parent().expect("父目录")).expect("建目录");
    std::fs::write(
        path,
        format!(
            "PLUGIN_NAME = \"{name}\"\nfunction get_view()\n  return {{ title = \"{name}\", widgets = {{}} }}\nend\n"
        ),
    )
    .expect("写夹具");
}

fn stem_of(path: &Path) -> String {
    path.file_stem().unwrap().to_string_lossy().to_string()
}

/// 把开关写进配置文件：解析器**以最后一个同名 section 为准**（本仓在那条上踩过一次），
/// 所以要追加一份 `[plugins]` 而不是就地改第一处出现的那一格。
fn config_with(app: &Path, legacy: &str) -> &'static FocusFlowConfig {
    let cfg = app.join("config.ini");
    let mut lines: Vec<String> = std::fs::read_to_string(&cfg)
        .map(|s| s.lines().map(|l| l.to_string()).collect())
        .unwrap_or_default();
    lines.push(String::new());
    lines.push("[plugins]".to_string());
    lines.push(format!("legacy_layout = {legacy}"));
    std::fs::write(&cfg, lines.join("\n")).expect("写配置");
    Box::leak(Box::new(FocusFlowConfig::load(cfg).expect("读配置失败")))
}

fn stems(found: &[Discovered]) -> Vec<String> {
    found.iter().map(|d| stem_of(&d.path)).collect()
}

/// 开关打开：只扫旧的平铺目录，而且那份的身份是"回滚分支"（不是内置、也不是用户插件）。
#[test]
fn the_rollback_switch_scans_only_the_old_flat_dir() {
    let _g = guard();
    let app = paths::test_app_dir("legacy_on");
    write_lua(
        &app.path().join("plugins").join("old_flat.lua"),
        "旧目录那一份",
    );
    write_lua(
        &paths::plugins_user_dir().join("new_data.lua"),
        "数据目录那一份",
    );

    let pm = PluginManager::new(
        config_with(app.path(), "true"),
        db::Database::init_readonly(),
    );
    let found = pm.discover().expect("回滚那一档的清单要读得出来");
    assert_eq!(
        stems(&found),
        vec!["old_flat".to_string()],
        "开关打开时只该看见旧的平铺目录（多看见一份 = 退回去以后同名两份互相顶）"
    );
    assert_eq!(
        found[0].source,
        PluginSource::Legacy,
        "那份的身份必须是回滚分支：它决定看见哪一组宿主 API 与要不要授权（3.4 / 3.7）"
    );
}

/// 开关关着（出厂默认）：平铺目录**不被扫描**，那份脚本只能以"复制进数据目录"的身份出现。
///
/// 三件事一起断才说清这一档：① 清单里那两条都出自数据目录（身份是用户插件，走授权那一套）；
/// ② 平铺目录里的原件**还在**（迁移是复制不是移动，留着就是为了哪天还能退回 `legacy_layout`）；
/// ③ 数据目录里确实有了那一份的副本。少了 ② 就看不出"退路被迁移吃掉"这一族缺陷，
/// 少了 ① 就看不出复制回来的那份不该被当成内置。
#[test]
fn the_flat_dir_is_not_scanned_when_the_switch_is_off() {
    let _g = guard();
    let app = paths::test_app_dir("legacy_off");
    let flat = app.path().join("plugins").join("old_flat.lua");
    write_lua(&flat, "旧目录那一份");
    write_lua(
        &paths::plugins_user_dir().join("new_data.lua"),
        "数据目录那一份",
    );

    let pm = PluginManager::new(
        config_with(app.path(), "false"),
        db::Database::init_readonly(),
    );
    let found = pm.discover().expect("清单要读得出来");
    let mut stems = stems(&found);
    stems.sort();
    assert_eq!(
        stems,
        vec!["new_data".to_string(), "old_flat".to_string()],
        "两份都该以数据目录那一份的身份出现（平铺目录本身不在扫描之列）"
    );
    assert!(
        found.iter().all(|d| d.source == PluginSource::User),
        "复制回来的那份不是内置模块：它要走授权那一套，实得 {:?}",
        found
            .iter()
            .map(|d| (stem_of(&d.path), format!("{:?}", d.source)))
            .collect::<Vec<_>>()
    );
    assert!(
        flat.exists(),
        "平铺目录里的原件被搬走了 —— 回滚的退路就是这一份"
    );
    assert!(
        paths::plugins_user_dir().join("old_flat.lua").exists(),
        "旧目录那份没被复制进数据目录：新布局里它永远不生效，而屏上没有任何一句说明"
    );
}
