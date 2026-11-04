//! 插件包的安装 / 卸载 / 更新（方案 P2 第 17 步）。
//!
//! 安装的产物就是**一份 `.zip`**（不是解压出来的目录）：磁盘上一格插件仍然是一个文件，
//! stem 仍是文件名，加载时才去包里取 `plugin.lua`（见 `package::read_script`）。
//! 这样热重载、停用列表、占用统计这些按"文件"写的既有逻辑一个字都不用改。
//!
//! 三条纪律：
//! · 装之前先**读一遍包**（读手在 `package`）：一个连 `plugin.lua` 都没有的 zip 不是插件，
//!   不该在盘上留下半个；
//! · 与内置模块同名的包**装不进来**：内置的位置就是身份，覆盖它等于让随包脚本被用户改掉；
//! · 卸载**默认保留** `plugins/state/<stem>/`：停用/卸载是"别再跑它"，不是"把它的设置删掉"。
//!   要删得显式传 `purge_state`（界面上是二次确认那一档）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths;
use crate::plugins::package;

/// `plugins/state/<stem>/meta.json` 的文件名（方案 3.5 那一格：安装来源、校验和）。
pub const META_FILE: &str = "meta.json";
/// 这份记录的格式版本（认不得的不猜）。
pub const META_VERSION: u32 = 1;

/// 安装记录：这一份包**从哪来、装进来的是哪一串字节**。
///
/// 它是「来源提示 + 校验和」那一半（P2 第 19 步）的落点：用户问"这个插件是谁给的"、
/// 或者怀疑"装进来的和我手里这份是不是同一份"时，详情页读的就是这一份。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageMeta {
    pub version: u32,
    pub stem: String,
    /// 安装时给的那条路径（能规范化就规范化；失败就原样记 —— 记一个失败比记一个假的绝对路径强）。
    pub source: String,
    /// 与授权同一套 `sha256:<hex>`：装进来的是哪一串字节，这一句只说这一个事实。
    pub content_hash: String,
    pub entries: usize,
    pub bytes: usize,
    /// 装进来的时刻（本地时间，给人看的）。
    pub installed_at: String,
}

fn meta_path(stem: &str) -> PathBuf {
    paths::plugin_state_path(stem).join(META_FILE)
}

/// 给人看的来源路径：去掉 `canonicalize` 带出来的 `\\?\` 前缀。
///
/// 那一串是 Windows 的扩展长度写法，合法但没人认得；`\\?\UNC\srv\share` 还原成
/// `\\srv\share`。只影响**显示**，它不参与任何判定（校验和是另一格）。
fn display_source(path: &Path) -> String {
    let text = path.display().to_string();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
}

/// 读一份安装记录。`Ok(None)` = 没有这份记录（手工放进目录的单文件插件就没有）。
pub fn meta_for(stem: &str) -> Result<Option<PackageMeta>, String> {
    let path = meta_path(stem);
    match std::fs::read(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{} 读不出来：{e}", path.display())),
        Ok(bytes) => {
            let meta: PackageMeta = serde_json::from_slice(&bytes)
                .map_err(|e| format!("{} 不是合法的安装记录：{e}", path.display()))?;
            if meta.version != META_VERSION {
                return Err(format!(
                    "{} 的格式版本是 {}，本宿主只认 {META_VERSION}（不猜旧格式）",
                    path.display(),
                    meta.version
                ));
            }
            Ok(Some(meta))
        }
    }
}

/// 详情页那一行「来源」：三档各说各的 —— 有记录 / 手放进目录的 / 记录读不出来。
///
/// 把格式化收在这里（而不是页面里）有两个理由：页面不在 GUI 线程之外读文件；
/// 而"没有记录"与"读不出来"必须是两句不同的话（本仓那一族缺陷的固定形状）。
pub fn source_line(stem: &str) -> String {
    source_line_of(&meta_for(stem))
}

/// [`source_line`] 的纯函数那一半：页面读一次记录，顺手把"装没装过"与那句话一起拿到，
/// 不必为同一份文件读两遍。
pub fn source_line_of(meta: &Result<Option<PackageMeta>, String>) -> String {
    match meta {
        Ok(Some(meta)) => {
            let hash = meta
                .content_hash
                .strip_prefix("sha256:")
                .unwrap_or(&meta.content_hash);
            let short: String = hash.chars().take(16).collect();
            format!(
                "来源：{} · sha256 {short}… · 装于 {}",
                meta.source,
                short_time(&meta.installed_at)
            )
        }
        Ok(None) => "来源：手放进插件目录（没有安装记录）".to_string(),
        Err(e) => format!("来源：安装记录读不出来（{e}）"),
    }
}

/// 这一格是不是**通过安装进来的**包（有安装记录 ⇒ 才有包文件可卸）。
pub fn is_installed(meta: &Result<Option<PackageMeta>, String>) -> bool {
    matches!(meta, Ok(Some(_)))
}

/// 记录里存的是完整的 RFC3339（带纳秒与时区）；页面上只留到分钟。
///
/// 认不出来的串原样交回：把"这一栏念不出来"折成空，会让那一行读起来像"没装过"。
fn short_time(rfc: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(rfc)
        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|_| rfc.to_string())
}

fn write_meta(meta: &PackageMeta) -> Result<(), String> {
    let path = meta_path(&meta.stem);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("{} 建不出来：{e}", parent.display()))?;
    }
    let text =
        serde_json::to_string_pretty(meta).map_err(|e| format!("安装记录序列化失败：{e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("{} 写不进去：{e}", tmp.display()))?;
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| format!("旧的 {} 删不掉：{e}", path.display()))?;
    }
    std::fs::rename(&tmp, &path).map_err(|e| format!("{} 换名失败：{e}", path.display()))
}

/// 装完之后的结论（页面/命令行要说的话都从这里取，不各自重算）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub stem: String,
    /// 落盘位置（`<data>/plugins/<stem>.zip`）。
    pub path: PathBuf,
    /// 这一份是覆盖安装（更新）还是第一次装。
    pub replaced: bool,
    /// 包里几个文件、解出来多少字节（页面那一格"占用"）。
    pub entries: usize,
    pub bytes: usize,
    /// 校验器说了几条"问题"（不阻塞安装，但要让人看见）。
    pub problems: usize,
}

/// 卸载之后那一格数据的下场。**三档分开**：`None` 一种写法会把"从来没存过"念成
/// "刚刚删掉了"，而那两句对用户意味着完全不同的事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateAfter {
    /// 它从来没写过状态文件（没什么可留的）。
    NeverExisted,
    /// 还在原处（默认那一档）。
    Kept(PathBuf),
    /// 这一次显式删掉了。
    Purged,
}

/// 卸载的结论：包删没删、数据去了哪一档。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uninstalled {
    pub stem: String,
    pub removed_package: PathBuf,
    pub state: StateAfter,
}

/// 安装一份 `.zip` 插件包。
pub fn install(package_path: &Path) -> Result<Installed, String> {
    if !package::is_package_path(package_path) {
        return Err(format!(
            "{} 不是 .zip 插件包：请先用 focusflow-plugin pack 打一份",
            package_path.display()
        ));
    }
    let bytes = std::fs::read(package_path)
        .map_err(|e| format!("{} 读不出来：{e}", package_path.display()))?;
    let entries = package::read(&bytes)?;
    let text = package::script_source(&entries)?.to_string();

    let stem = crate::plugins::manager::PluginManager::stem_of(package_path);
    if !paths::is_safe_state_stem(&stem) {
        return Err(format!(
            "包名 {stem:?} 不能当插件标识（不带路径分隔与 ..）：请换个文件名再打一次"
        ));
    }
    let builtin_has = ["lua", "zip"].iter().any(|ext| {
        paths::plugins_builtin_dir()
            .join(format!("{stem}.{ext}"))
            .exists()
    });
    if builtin_has {
        return Err(format!(
            "「{stem}」是随包内置模块的名字：内置那一份以程序目录为准，装一份同名的会互相盖住。\
             要跑自己那份请改个文件名再打包"
        ));
    }
    let user_dir = paths::plugins_user_dir();
    let single = user_dir.join(format!("{stem}.lua"));
    if single.exists() {
        return Err(format!(
            "{} 已经有一份同名的单文件插件：先把那一份移走（或换个包名）再装，\
             否则两份谁在跑取决于扫描顺序",
            single.display()
        ));
    }
    let target = user_dir.join(format!("{stem}.zip"));
    let replaced = target.exists();

    // 先写临时文件再换名：写到一半失败时盘上不会留下半份包（加载一侧会把半份当成坏包，
    // 而用户看到的是"我明明装好了"）。Windows 的 rename 不覆盖已有文件，先删再换。
    let tmp = user_dir.join(format!("{stem}.zip.tmp"));
    std::fs::write(&tmp, &bytes).map_err(|e| format!("{} 写不进去：{e}", tmp.display()))?;
    // 安装记录先落，再换名：后者失败时把记录清掉，不留"记录说有、盘上没有"的格子。
    let meta = PackageMeta {
        version: META_VERSION,
        stem: stem.clone(),
        source: display_source(
            &package_path
                .canonicalize()
                .unwrap_or_else(|_| package_path.to_path_buf()),
        ),
        content_hash: crate::plugins::permissions::content_hash(&bytes),
        entries: entries.len(),
        bytes: bytes.len(),
        installed_at: chrono::Local::now().to_rfc3339(),
    };
    if let Err(e) = write_meta(&meta) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if replaced {
        std::fs::remove_file(&target)
            .map_err(|e| format!("旧的 {} 删不掉：{e}", target.display()))?;
    }
    if let Err(e) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(meta_path(&stem));
        return Err(format!("{} 换名失败：{e}", target.display()));
    }

    // 校验器的结论只是"提示"（方案 3.7：漏报比误报代价低），装还是要装 ——
    // 但条数要交回调用方，别让作者以为这一份是干净的。
    let report =
        crate::plugins::validate::validate(&stem, crate::plugins::PluginSource::User, &text);
    let problems = report
        .findings
        .iter()
        .filter(|f| f.level == crate::plugins::validate::Level::Error)
        .count();
    Ok(Installed {
        stem,
        path: target,
        replaced,
        entries: entries.len(),
        bytes: bytes.len(),
        problems,
    })
}

/// 卸载一份**装进来的**包（`<data>/plugins/<stem>.zip`）。
///
/// 单文件插件不归这里管：那是用户自己放进去的文件，删它该由用户自己动手
/// （`uninstall` 只收拾"通过安装进来"的那一类，免得一个手滑把作者的工作副本删了）。
pub fn uninstall(stem: &str, purge_state: bool) -> Result<Uninstalled, String> {
    if !paths::is_safe_state_stem(stem) {
        return Err(format!("{stem:?} 不能当插件标识"));
    }
    let target = paths::plugins_user_dir().join(format!("{stem}.zip"));
    if !target.is_file() {
        return Err(format!(
            "{} 不在了：它不是通过安装进来的包（单文件插件请自己从插件目录移走）",
            target.display()
        ));
    }
    std::fs::remove_file(&target).map_err(|e| {
        format!(
            "{} 删不掉：{e}（程序正在用它？关掉面板再试）",
            target.display()
        )
    })?;

    let state = paths::plugin_state_path(stem);
    let state_after = if state.exists() {
        if purge_state {
            std::fs::remove_dir_all(&state)
                .map_err(|e| format!("{} 删不掉：{e}", state.display()))?;
            StateAfter::Purged
        } else {
            StateAfter::Kept(state)
        }
    } else {
        StateAfter::NeverExisted
    };
    Ok(Uninstalled {
        stem: stem.to_string(),
        removed_package: target,
        state: state_after,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::manager::PluginManager;
    use crate::plugins::package::Entry;

    /// 程序目录是进程全局的：本套用例逐个跑，用的是**全仓同一把锁**
    /// （`paths::test_app_dir_lock`）—— 另起一把私锁只挡得住自己，挡不住隔壁换目录。
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        paths::test_app_dir_lock()
    }

    fn write_package(dir: &Path, stem: &str, script: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let bytes = package::pack(vec![
            Entry {
                name: package::SCRIPT_NAME.to_string(),
                data: script.as_bytes().to_vec(),
            },
            Entry {
                name: "assets/logo.txt".to_string(),
                data: b"logo".to_vec(),
            },
        ])
        .unwrap();
        let path = dir.join(format!("{stem}.zip"));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn manager() -> PluginManager {
        let config: &'static crate::config::FocusFlowConfig = Box::leak(Box::new(
            crate::config::FocusFlowConfig::load(paths::data_dir().join("config.ini"))
                .expect("临时配置应能载入"),
        ));
        PluginManager::new(config, crate::db::Database::init_readonly())
    }

    /// 装进来的包必须出现在**发现清单**里，而且身份是用户插件。
    ///
    /// "文件写下去了"不算装好：清单扫不到它，用户打开插件页只会看到少一条。
    #[test]
    fn an_installed_package_shows_up_in_the_discovery_list() {
        let _lock = guard();
        let app = paths::test_app_dir("install_discover");
        let src = app.path().join("incoming");
        let zip = write_package(
            &src,
            "installed_demo",
            "PLUGIN_NAME = \"装进来的\"\nfunction get_view() return { widgets = {} } end\n",
        );

        let report = install(&zip).expect("一份合法包该装得进去");
        assert_eq!(report.stem, "installed_demo");
        assert!(!report.replaced);
        assert_eq!(report.entries, 2);
        assert!(report.path.is_file(), "包要落在数据目录那一处");

        let mut pm = manager();
        let discovered = pm.discover().expect("插件目录该读得动");
        let found = discovered
            .iter()
            .find(|d| d.path == report.path)
            .expect("装好的包必须在清单里");
        assert_eq!(found.source, crate::plugins::PluginSource::User);

        // 清单里有它还不够：加载那一侧要真能从包里取出脚本、装进 VM。
        let name = pm
            .load_plugin(&report.path, crate::plugins::PluginSource::User)
            .expect("包里的脚本要装得起来");
        assert_eq!(name, "装进来的");
        assert!(pm.get_plugin(&name).is_some(), "装上之后实例表里要有它");
    }

    /// 安装记录要说清**从哪来**与**装进来的是哪一串字节**（P2 第 19 步的来源提示 + 校验和）。
    ///
    /// 这一格是详情页的读者；没有它，"这个插件是谁给的""我手里这份和装进去的是不是同一份"
    /// 就只能靠用户自己记。单文件插件没有这份记录 —— 那是 `Ok(None)`，不是错误。
    #[test]
    fn the_install_record_names_the_source_and_the_package_hash() {
        let _lock = guard();
        let app = paths::test_app_dir("install_meta");
        let src = app.path().join("incoming");
        let zip = write_package(
            &src,
            "recorded",
            "PLUGIN_NAME = \"有记录的\"\nfunction get_view() return { widgets = {} } end\n",
        );
        let report = install(&zip).unwrap();

        let meta = meta_for("recorded")
            .expect("安装记录要读得动")
            .expect("装进来的包必须有一份安装记录");
        assert_eq!(meta.stem, "recorded");
        assert!(
            meta.source.ends_with("recorded.zip"),
            "来源要指向那一份包：{}",
            meta.source
        );
        assert!(
            !meta.source.starts_with(r"\\?\"),
            "显示用的来源不许带 Windows 的扩展长度前缀：{}",
            meta.source
        );
        assert_eq!(
            meta.content_hash,
            crate::plugins::permissions::content_hash(&std::fs::read(&zip).unwrap()),
            "校验和与授权那一头是同一把尺子（都是包字节）"
        );
        assert_eq!(meta.entries, 2);
        assert_eq!(meta.bytes, report.bytes);
        assert!(!meta.installed_at.is_empty(), "装进来的时刻要留着");

        let line = source_line("recorded");
        assert!(
            line.contains("recorded.zip") && line.contains("sha256 ") && line.contains("装于 "),
            "详情页那一句要念出来源、校验和与时刻：{line}"
        );
        let stamp = line.split("装于 ").nth(1).unwrap_or_default();
        assert_eq!(
            stamp.chars().count(),
            16,
            "时刻只留到分钟（YYYY-MM-DD HH:MM），别把纳秒与时区糊到页面上：{line}"
        );

        // 手工放进目录的单文件插件没有这份记录：三态里的"没有"，不是错误。
        std::fs::create_dir_all(paths::plugins_user_dir()).unwrap();
        std::fs::write(
            paths::plugins_user_dir().join("hand_written.lua"),
            "PLUGIN_NAME = \"手放的\"\n",
        )
        .unwrap();
        assert_eq!(meta_for("hand_written").unwrap(), None);
        assert!(
            source_line("hand_written").contains("没有安装记录"),
            "没有记录要说成没有记录，不许说成坏了：{}",
            source_line("hand_written")
        );

        // 记录被改坏：那一句要说"读不出来"，不许折成"没有记录"。
        let meta_file = meta_path("recorded");
        std::fs::write(&meta_file, "{ not json").unwrap();
        assert!(
            source_line("recorded").contains("读不出来"),
            "坏记录要念成读不出来：{}",
            source_line("recorded")
        );
        let _ = meta_file;
    }

    /// 第二次装同一份 = 更新：要明确说"覆盖了"，而不是静默换掉一份。
    #[test]
    fn installing_again_replaces_and_says_so() {
        let _lock = guard();
        let app = paths::test_app_dir("install_replace");
        let src = app.path().join("incoming");
        let first = write_package(
            &src,
            "updatable",
            "PLUGIN_NAME = \"v1\"\nfunction get_view() return { widgets = {} } end\n",
        );
        let one = install(&first).unwrap();
        assert!(!one.replaced);

        let second = write_package(
            &src,
            "updatable",
            "PLUGIN_NAME = \"v2\"\nfunction get_view() return { widgets = {} } end\n",
        );
        let two = install(&second).unwrap();
        assert!(two.replaced, "第二次装要说成覆盖");
        assert_eq!(two.path, one.path);
        let (text, _) = package::read_script(&two.path).expect("包里的脚本读得动");
        assert!(
            text.contains("PLUGIN_NAME = \"v2\""),
            "盘上那一份必须是新的，实得：{text}"
        );
    }

    /// 卸载默认保留数据；显式清除才删。
    ///
    /// 这两档差的是用户重新启用之后设置还在不在 —— 不能靠一个布尔默认值猜。
    #[test]
    fn uninstall_keeps_state_unless_purged() {
        let _lock = guard();
        let app = paths::test_app_dir("install_uninstall");
        let src = app.path().join("incoming");
        let zip = write_package(
            &src,
            "leaver",
            "PLUGIN_NAME = \"要走的\"\nfunction get_view() return { widgets = {} } end\n",
        );
        let installed = install(&zip).unwrap();
        drop(installed);

        let state = paths::plugin_state_path("leaver");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("state.json"), "{\"k\":1}").unwrap();

        let kept = uninstall("leaver", false).unwrap();
        assert!(!kept.removed_package.exists(), "包要删掉");
        assert_eq!(kept.state, StateAfter::Kept(state.clone()));
        assert!(state.join("state.json").is_file(), "默认保留设置");

        let zip = write_package(
            &src,
            "leaver",
            "PLUGIN_NAME = \"要走的\"\nfunction get_view() return { widgets = {} } end\n",
        );
        install(&zip).unwrap();
        let purged = uninstall("leaver", true).unwrap();
        assert_eq!(purged.state, StateAfter::Purged);
        assert!(!state.exists(), "显式清除之后那一格才该没了");

        // 从来没存过东西的那一格：既不是"留着"，也不是"刚删掉"。装包会留下一份安装记录，
        // 所以要先把它删掉，才轮得到"这一格根本不存在"这条腿。
        let zip = write_package(
            &src,
            "leaver",
            "PLUGIN_NAME = \"要走的\"\nfunction get_view() return { widgets = {} } end\n",
        );
        install(&zip).unwrap();
        std::fs::remove_dir_all(&state).unwrap();
        assert_eq!(
            uninstall("leaver", false).unwrap().state,
            StateAfter::NeverExisted,
            "没写过状态文件就说没写过，不许念成「删掉了」"
        );
    }

    /// 没有 `plugin.lua` 的包不是插件：拒绝，并且盘上不许留下任何东西。
    #[test]
    fn a_zip_without_a_script_is_refused_and_writes_nothing() {
        let _lock = guard();
        let app = paths::test_app_dir("install_bad");
        let src = app.path().join("incoming");
        std::fs::create_dir_all(&src).unwrap();
        let zip = src.join("not_a_plugin.zip");
        let bytes = package::pack(vec![Entry {
            name: "assets/logo.txt".to_string(),
            data: b"logo".to_vec(),
        }])
        .unwrap();
        std::fs::write(&zip, bytes).unwrap();

        let err = install(&zip).expect_err("没有脚本的包必须被拒");
        assert!(err.contains(package::SCRIPT_NAME), "{err}");
        assert!(
            !paths::plugins_user_dir().join("not_a_plugin.zip").exists(),
            "拒绝的时候不许留下半个包"
        );
    }

    /// 已有同名单文件时不许装包：两份谁在跑取决于扫描顺序。
    #[test]
    fn a_same_stem_single_file_blocks_the_install() {
        let _lock = guard();
        let app = paths::test_app_dir("install_clash");
        let user = paths::plugins_user_dir();
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(user.join("clash.lua"), "PLUGIN_NAME = \"hand\"\n").unwrap();
        let src = app.path().join("incoming");
        let zip = write_package(
            &src,
            "clash",
            "PLUGIN_NAME = \"packed\"\nfunction get_view() return { widgets = {} } end\n",
        );
        let err = install(&zip).expect_err("同名单文件挡着，不许两边各有一份");
        assert!(err.contains("clash.lua"), "要说清挡路的是哪一份：{err}");
        assert!(!user.join("clash.zip").exists());
    }
}
