//! 剪贴板插件 API（`PLUGIN_API = 3`）：读、写、事件三件事各一格判据。
//!
//! 这一族盯的是"权限与版本两头都接上了"：
//! · 读拿到的是**真正入库的那条文本**，不是空串；
//! · 没声明 `clipboard.write` 时写被拒（DENIED 里点名 `clipboard.write`）；
//! · 声明了写之后，超长文本被拒在 16 KB 那一格，而不是写进剪贴板；
//! · `on("clipboard")` 的载荷是每一段投进去的文本，两条就收到两条；
//! · `PLUGIN_API = 2` 的插件取到的是 `nil` —— v1 存量插件走免闸，这一格不能靠闸兜底。

#![cfg(feature = "test-utils")]

use std::sync::Mutex;

use focusflow_core::clipboard;
use focusflow_core::config::FocusFlowConfig;
use focusflow_core::db;
use focusflow_core::paths;
use focusflow_core::plugins::manager::PluginManager;
use focusflow_core::plugins::{permissions, plugin_events, plugin_store, PluginSource};

fn guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn clear_event_slot() {
    plugin_events::reset_clipboard_events();
}

fn write_plugin(dir: &std::path::Path, stem: &str, body: &str) -> std::path::PathBuf {
    let dir = dir.join("plugins");
    std::fs::create_dir_all(&dir).expect("建临时插件目录");
    let file = dir.join(format!("{stem}.lua"));
    std::fs::write(&file, body).expect("写夹具");
    file
}

fn manager_in(app: &paths::TestAppDir) -> PluginManager {
    let config: &'static FocusFlowConfig = Box::leak(Box::new(
        FocusFlowConfig::load(app.path().join("config.ini")).expect("读配置失败"),
    ));
    PluginManager::new(config, db::Database::init_readonly())
}

fn cell(stem: &str, key: &str) -> Option<String> {
    match plugin_store::get(stem, key) {
        Ok(Some(v)) => v.as_str().map(|s| s.to_string()),
        Ok(None) => None,
        Err(e) => panic!("{stem} 的 store 读不出来: {e}"),
    }
}

/// 读 + 拒写 + 订阅事件。声明的权限故意只有 `clipboard.read`。
fn reader_source() -> &'static str {
    r#"
PLUGIN_NAME = "clip_reader"
PLUGIN_API = 3
PLUGIN_PERMISSIONS = { "clipboard.read", "storage.read", "storage.write" }
function init()
  local text = focusflow.clipboard.read_text()
  focusflow.store.set("read", text or "<nil>")
  local ok, err = pcall(function() focusflow.clipboard.write_text("x") end)
  focusflow.store.set("denied", tostring(ok) .. "|" .. tostring(err))
  focusflow.on("clipboard", function(t)
    focusflow.store.set("event_last", t)
    local n = tonumber(focusflow.store.get("event_n") or "0") + 1
    focusflow.store.set("event_n", tostring(n))
  end)
  focusflow.store.flush()
end
function get_view()
  return { title = "t", widgets = { { type = "label", text = "x" } } }
end
"#
}

/// 声明了读写：超长文本必须被 16 KB 那一格拒掉（不许碰剪贴板）。
fn writer_source() -> &'static str {
    r#"
PLUGIN_NAME = "clip_writer"
PLUGIN_API = 3
PLUGIN_PERMISSIONS = { "clipboard.read", "clipboard.write", "storage.read", "storage.write" }
function init()
  local ok, res, why = pcall(function() return focusflow.clipboard.write_text(string.rep("x", 20000)) end)
  focusflow.store.set("limit", tostring(ok) .. "|" .. tostring(res) .. "|" .. tostring(why))
  focusflow.store.flush()
end
function get_view()
  return { title = "t", widgets = { { type = "label", text = "x" } } }
end
"#
}

fn grant(stem: &str, src: &str, declared: &[&str]) {
    let declared: Vec<String> = declared.iter().map(|s| s.to_string()).collect();
    permissions::write_grants(
        stem,
        &permissions::Grants {
            permission_hash: permissions::permission_hash(&declared),
            content_hash: permissions::content_hash(src.as_bytes()),
            granted: declared,
        },
    )
    .expect("夹具授权该落得下去");
}

#[test]
fn read_returns_the_ingested_text_and_write_needs_its_own_permission() {
    let _g = guard();
    clear_event_slot();
    let app = paths::test_app_dir("plugin_clip_read");
    clipboard::init_db().unwrap();
    clipboard::ingest("插件该读到这一行", "test.exe", clipboard::now_ms()).unwrap();

    let src = reader_source();
    let file = write_plugin(app.path(), "clip_reader", src);
    grant(
        "clip_reader",
        src,
        &["clipboard.read", "storage.read", "storage.write"],
    );
    let mut pm = manager_in(&app);
    let name = pm
        .load_plugin(&file, PluginSource::User)
        .unwrap_or_else(|e| panic!("读剪贴板的夹具该加载得起来: {e}"));
    assert_eq!(name, "clip_reader");

    assert_eq!(
        cell("clip_reader", "read").as_deref(),
        Some("插件该读到这一行"),
        "read_text 拿到的必须是真正入库的那条文本"
    );
    let denied = cell("clip_reader", "denied").expect("拒写的结局要落盘");
    assert!(
        denied.starts_with("false|") && denied.contains("clipboard.write"),
        "没声明写权限时 write_text 必须抛 DENIED 并点名 clipboard.write: {denied}"
    );

    // 事件：投两段，回调各收一次；载荷是投进去的原文。
    assert!(plugin_events::post_clipboard_text("第一段"));
    assert!(plugin_events::post_clipboard_text("第二段"));
    let ran = pm.pump_timers();
    assert!(
        ran.iter().any(|(n, _)| n == "clip_reader"),
        "这一拍该跑到那个插件: {ran:?}"
    );
    assert_eq!(
        cell("clip_reader", "event_n").as_deref(),
        Some("2"),
        "投了两段就该收到两次（不是合并成一条）"
    );
    assert_eq!(
        cell("clip_reader", "event_last").as_deref(),
        Some("第二段"),
        "最后一格拿到的必须最后投的那一段"
    );
    clear_event_slot();
}

#[test]
fn oversized_write_is_refused_before_it_touches_the_clipboard() {
    let _g = guard();
    let app = paths::test_app_dir("plugin_clip_write");
    clipboard::init_db().unwrap();

    let src = writer_source();
    let file = write_plugin(app.path(), "clip_writer", src);
    grant(
        "clip_writer",
        src,
        &[
            "clipboard.read",
            "clipboard.write",
            "storage.read",
            "storage.write",
        ],
    );
    let mut pm = manager_in(&app);
    pm.load_plugin(&file, PluginSource::User)
        .unwrap_or_else(|e| panic!("写剪贴板的夹具该加载得起来: {e}"));

    let limit = cell("clip_writer", "limit").expect("上限那一格的结局要落盘");
    assert!(
        limit.starts_with("true|false|") && limit.contains("超过上限"),
        "声明了写权限之后，20000 字节必须被 16 KB 那一格拒掉: {limit}"
    );
}

#[test]
fn a_v2_plugin_cannot_see_the_clipboard_api() {
    let _g = guard();
    let app = paths::test_app_dir("plugin_clip_hidden");
    clipboard::init_db().unwrap();
    let src = "PLUGIN_NAME = \"clip_hidden\"\n\
               PLUGIN_API = 2\n\
               assert(focusflow.clipboard == nil, \"API 2 的插件不该看见剪贴板 API\")\n\
               function get_view() return { title = \"t\", widgets = {{ type = \"label\", text = \"x\" }} } end\n";
    let file = write_plugin(app.path(), "clip_hidden", src);
    let mut pm = manager_in(&app);
    pm.load_plugin(&file, PluginSource::User)
        .unwrap_or_else(|e| panic!("API 2 的插件应当照常加载（只是看不见剪贴板）: {e}"));
}
