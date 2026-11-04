//! `focusflow-plugin` —— 脚本插件作者的工具链（方案 3.3 的清单 + 路线图 P2 第 18 步）。
//!
//! 与 `focusflow-cli` 分家：那一份读用户的账本、命令都是"动数据"的；这一份只做**脚本文件**
//! 上的静态事，一个字节都不碰正在使用的数据目录。
//!
//! 三条纪律，都是本仓修过好几轮的那一类：
//! · **判定不抄第二把**。清单折叠、权限表、两个哈希全在 `focusflow_core::plugins`，这里只
//!   选退出码与排版。抄一把的下场是"工具说这份没问题、插件页说要授权"。
//! · **四种结局各有出口**：没问题 / 有问题 / 根本没问到（文件读不出来）/ 命令写错，
//!   退出码与流向（stdout 还是 stderr）都不许折叠。"没问到"念成"没问题"是本仓最高产的缺陷族。
//! · **`new` 生成的那份要能被真加载器装起来**，不是只被这里的尺子说通过 ——
//!   脚手架自己写坏了，每个照着它写的插件都跟着坏。

use focusflow_core::config;
use focusflow_core::plugins::{self, permissions, PluginSource};
use focusflow_core::{db, paths};
use std::path::{Path, PathBuf};

/// `run` 不带 `--data` 时用的那份草稿程序目录。
///
/// 放在临时目录而不是当前数据目录，是因为这一条命令要**按声明落一份真授权**：
/// 那玩意儿落在用户正在用的 `plugins/state/<stem>/grants.json` 里，等于工具替用户点了「同意」。
const SCRATCH_DIR_NAME: &str = "focusflow-plugin-run";

/// `new <目录>` 时落的那个文件名。stem 决定 `plugins/state/<stem>/` 那一格，
/// 所以默认名要像一个标识符：小写、下划线、不带空格。
const DEFAULT_SCAFFOLD_FILE: &str = "my_plugin.lua";

const USAGE: &str = "\
focusflow-plugin —— 脚本插件作者的工具链

  new <目录|路径.lua>   生成一份脚本插件脚手架（不覆盖已有文件）
  validate <路径.lua|包.zip>   检查清单：字段会不会被折掉、声明的权限与代码真用到对不对得上
  hash <路径.lua|包.zip>       打印这份插件的两个哈希（授权绑 permission_hash 那一个）
  pack <目录> [-o <文件.zip>]  把 plugin.lua + assets/ 打成确定性 zip（缺省落在目录旁边同名 .zip）
  run <路径.lua>          用一拍事件夹具把插件跑一遍，把宿主读出来的视图打成 JSON
      --event <view|timer|app_change|action>   跑哪一拍（缺省 view = 只 init + get_view）
      --app <应用名>                            app_change 那一拍投出去的就是这个名字
      --action <动作 id>                        action 那一拍点的按钮
      --data <程序目录>                          用哪一份程序目录（缺省 = 临时目录，不碰正在用的那份）

退出码：0 = 这件事成了；1 = 有问题、读不出来或命令写错。
validate / hash 的话一律走 stdout，只有「根本没问到」走 stderr；
run 反过来 —— JSON 单独占 stdout（要能直接管道给 jq），其余话全走 stderr。
";

/// 每条命令最多接受几个位置参数（不含命令本身）。
///
/// 与 `focusflow-cli` 同一套路：多出来的参数绝不能静悄悄丢掉，那症状是"以为两件事都做了"。
fn max_args(cmd: &str) -> Option<usize> {
    match cmd {
        "new" | "validate" | "hash" => Some(1),
        // `pack <目录> [-o <文件.zip>]`：目录 + 选项 + 值。
        "pack" => Some(3),
        _ => None,
    }
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::ExitCode::from(run(&args) as u8)
}

fn run(args: &[String]) -> i32 {
    if args.is_empty() {
        eprint!("{USAGE}");
        return 1;
    }
    match args[0].as_str() {
        "-h" | "--help" => {
            print!("{USAGE}");
            return 0;
        }
        _ => {}
    }

    if let Some(max) = max_args(args[0].as_str()) {
        if args.len() - 1 > max {
            eprintln!(
                "{} 只接受 {max} 个参数，多出来的不会执行：{}",
                args[0],
                args[1 + max..].join(" ")
            );
            return 1;
        }
    }

    match args[0].as_str() {
        "validate" => match args.get(1).map(String::as_str) {
            // 这一条是作者手敲的，不是挂在计划任务上的，所以逐行结论走 stdout；
            // 只有"根本没问到"（文件读不出来）走 stderr。
            Some(target) => match validate_target(Path::new(target)) {
                Err(why) => {
                    eprintln!("{why}");
                    1
                }
                Ok(report) => {
                    println!("{}", report.render());
                    if report.has_errors() {
                        1
                    } else {
                        0
                    }
                }
            },
            None => {
                eprintln!("用法: focusflow-plugin validate <插件的 .lua 路径>");
                1
            }
        },
        "hash" => match args.get(1).map(String::as_str) {
            Some(target) => match hash_target(Path::new(target)) {
                Err(why) => {
                    eprintln!("{why}");
                    1
                }
                Ok(text) => {
                    println!("{text}");
                    0
                }
            },
            None => {
                eprintln!("用法: focusflow-plugin hash <插件的 .lua 路径>");
                1
            }
        },
        "new" => scaffold(args.get(1).map(String::as_str)),
        "pack" => match parse_pack(&args[1..]) {
            Err(why) => {
                eprintln!("{why}");
                1
            }
            Ok(req) => match pack_plugin(&req) {
                Err(why) => {
                    eprintln!("{why}");
                    1
                }
                Ok(out) => {
                    println!("已打包：{}", out.display());
                    println!(
                        "复核同一把尺子：focusflow-plugin validate \"{}\" / hash \"{}\"",
                        out.display(),
                        out.display()
                    );
                    0
                }
            },
        },
        "run" => match parse_run(&args[1..]) {
            Err(why) => {
                eprintln!("{why}");
                1
            }
            Ok(req) => match run_fixture(&req) {
                Err(why) => {
                    eprintln!("{why}");
                    1
                }
                Ok(json) => {
                    println!("{json}");
                    0
                }
            },
        },
        other => {
            eprintln!("未知命令：{other}");
            eprint!("{USAGE}");
            1
        }
    }
}

/// 那份插件的两个哈希，排成给人看的两行。
///
/// 拆成纯函数（返回文本而不是自己打印）是为了让判据能引用**具体那一行**：
/// 打印出去的字符串在测试里拿不到，而"键名对了但数是抄的"正是这种量具照不出来的。
///
/// 权限取自**加载侧折叠之后的那一份**（`validate_path` 交回的清单），不是脚本里那段文本 ——
/// 作者把形状写坏了（`PLUGIN_PERMISSIONS = "stats.read"`），哈希就该算在宿主真的会用的那份上，
/// 否则工具念的数与插件页那道闸认的数不是一对。
fn hash_lines(file: &Path) -> Result<String, String> {
    let report = plugins::validate::validate_path(file)?;
    let bytes =
        std::fs::read(file).map_err(|e| format!("{} 的字节读不出来: {e}", file.display()))?;
    Ok(hash_text(&report, &bytes))
}

/// 两条入口（单文件 / 包）共用的排版。
///
/// 两个哈希**只在这里**算：一处算权限、一处算字节的话，工具念的数与授权那道闸认的数
/// 迟早不是一对。
fn hash_text(report: &plugins::validate::Report, bytes: &[u8]) -> String {
    let mut out = format!(
        "permission_hash = {}\ncontent_hash    = {}",
        plugins::permissions::permission_hash(&report.manifest.permissions),
        plugins::permissions::content_hash(bytes),
    );
    // 清单里宿主认不得的那几项**仍留在声明里**（折叠只排序去重），但运行时不会为它装闸 ——
    // 于是那一格谁都调得动，而授权页上写着"已授权包含它"。这一句必须说出来，
    // 否则作者以为声明错了会被拦。
    for unknown in plugins::permissions::unknown_permissions(&report.manifest.permissions) {
        out.push_str(&format!(
            "\n提示：清单里的 {unknown} 宿主不认得 —— 它仍算在 permission_hash 里，\
             但运行时不会为它装闸，所以那一格谁都能调"
        ));
    }
    out
}

/// 包的哈希：清单来自包里那份 `plugin.lua`，`content_hash` 算在**包字节**上。
///
/// 为什么不单独算脚本字节：授权绑定的是"用户安装的这一份包"，而确定性打包保证同一棵
/// 目录每次打出来是同一串字节 —— 重新打包不会平白让用户看到"插件已更新"。
fn hash_package(file: &Path) -> Result<String, String> {
    let bytes =
        std::fs::read(file).map_err(|e| format!("{} 的字节读不出来: {e}", file.display()))?;
    let entries = plugins::package::read(&bytes)?;
    let text = plugins::package::script_source(&entries)?;
    let stem = plugins::manager::PluginManager::stem_of(file);
    let report = plugins::validate::validate(&stem, plugins::validate::source_of(file), text);
    Ok(hash_text(&report, &bytes))
}

fn hash_target(path: &Path) -> Result<String, String> {
    if plugins::package::is_package_path(path) {
        hash_package(path)
    } else {
        hash_lines(path)
    }
}

/// 校验的入口：`.lua` 走原路，`.zip` 先取出包里那份 `plugin.lua` 再走**同一个** `validate`。
fn validate_target(path: &Path) -> Result<plugins::validate::Report, String> {
    if plugins::package::is_package_path(path) {
        let bytes = std::fs::read(path).map_err(|e| format!("{} 读不出来: {e}", path.display()))?;
        let entries = plugins::package::read(&bytes)?;
        let text = plugins::package::script_source(&entries)?;
        let stem = plugins::manager::PluginManager::stem_of(path);
        Ok(plugins::validate::validate(
            &stem,
            plugins::validate::source_of(path),
            text,
        ))
    } else {
        plugins::validate::validate_path(path)
    }
}

/// `pack` 那一问要的东西。
struct PackReq {
    dir: PathBuf,
    out: Option<PathBuf>,
}

fn parse_pack(args: &[String]) -> Result<PackReq, String> {
    let mut dir: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        // 与 `parse_run` 同一个写法：取值要推进游标，闭包会与 `args` 的借用来回打架。
        let flag = args[i].clone();
        let mut value = |what: &str| -> Result<String, String> {
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| format!("{what} 后面要跟一个值（这一问根本没问到）"))
        };
        match flag.as_str() {
            "-o" => out = Some(PathBuf::from(value("-o")?)),
            other if other.starts_with('-') => {
                return Err(format!("认不出选项 {other}（-h 看用法）"))
            }
            other => {
                if dir.is_some() {
                    return Err(format!(
                        "pack 只接一个目录，多出来的那一个不会执行：{other}"
                    ));
                }
                dir = Some(PathBuf::from(other));
            }
        }
        i += 1;
    }
    let dir = dir.ok_or("用法: focusflow-plugin pack <目录> [-o <文件.zip>]")?;
    Ok(PackReq { dir, out })
}

fn pack_plugin(req: &PackReq) -> Result<PathBuf, String> {
    let entries = plugins::package::pack_dir(&req.dir)?;
    let bytes = plugins::package::pack(entries)?;
    let out = req
        .out
        .clone()
        .unwrap_or_else(|| default_package_path(&req.dir));
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("{} 建不出来: {e}", parent.display()))?;
        }
    }
    std::fs::write(&out, bytes).map_err(|e| format!("{} 写不出去: {e}", out.display()))?;
    Ok(out)
}

/// 不给 `-o` 时落在插件目录**旁边**：`foo/` ⇒ `foo.zip`。
fn default_package_path(dir: &Path) -> PathBuf {
    let text = dir.to_string_lossy();
    PathBuf::from(format!("{}.zip", text.trim_end_matches(['/', '\\'])))
}

/// 脚手架落在哪：给目录就是 `<目录>/<默认名>`，给文件名就照那个名。
fn scaffold(path: Option<&str>) -> i32 {
    let Some(target) = path else {
        eprintln!("用法: focusflow-plugin new <目录|{DEFAULT_SCAFFOLD_FILE}>");
        return 1;
    };
    let given = PathBuf::from(target);
    let file = if given.is_dir() {
        given.join(DEFAULT_SCAFFOLD_FILE)
    } else {
        given
    };
    if file.exists() {
        // 覆盖 = 把作者已经写过的东西吃掉。这条一律拒绝，恢复办法只有换一个名字。
        eprintln!("{} 已经存在，不覆盖", file.display());
        return 1;
    }
    let Some(parent) = file.parent() else {
        eprintln!("{} 认不出要写在哪个目录", file.display());
        return 1;
    };
    if let Err(e) = std::fs::create_dir_all(parent) {
        eprintln!("{} 建不出来: {e}", parent.display());
        return 1;
    }
    let stem = plugins::manager::PluginManager::stem_of(&file);
    if let Err(e) = std::fs::write(&file, scaffold_text(&stem)) {
        eprintln!("{} 写不出去: {e}", file.display());
        return 1;
    }
    println!("已生成脚手架：{}", file.display());
    println!("身份由位置决定（方案 3.1）：放进数据目录的 plugins/ 里 = 扩展插件（走授权），");
    println!("放进程序目录的 plugins/builtin/ 里 = 内置模块（免授权）。");
    0
}

/// 脚手架正文。`stem` 是文件名（不含后缀），同时写进 `PLUGIN_NAME` 让页面上认得出自己。
///
/// 这份文本每一格都是当契约写的，因为三条已有的尺子都要过：
/// · 校验器不许说一句"问题"（含「声明必用」，所以声明的三个权限都出现在活代码里）；
/// · 真加载器要装得起来，且**闸是开着的**（授权按声明落，未声明的调用会被拒 ——
///   于是"声明少了"会在这一条上红，而不是等作者去界面上猜）；
/// · `PLUGIN_API = 2` 是走授权与通用 API 视图那一档的门槛（见方案 3.10）。
fn scaffold_text(stem: &str) -> String {
    format!(
        r#"-- {stem} —— focusflow-plugin new 生成的脚手架
-- 清单里每一格都会被加载侧折成缺省值：能省的是"不写"，不能省的是"写错"。
-- 改完跑两遍：focusflow-plugin validate 这一份 → hash → 放进 plugins/ 重启。

PLUGIN_NAME = "{stem}"
PLUGIN_DESC = "改这一句：插件页上那行说明"
PLUGIN_VERSION = "0.1.0"
PLUGIN_API = 2
PLUGIN_PERMISSIONS = {{ "stats.read", "storage.read", "storage.write" }}
PLUGIN_REFRESH_MS = 0
PLUGIN_LIFECYCLE = "panel"

function init()
  -- 用 storage.write：把"第一次起来"记进通用存储，重启还读得到。
  if focusflow.store.get("booted_at") == nil then
    focusflow.store.set("booted_at", tostring(focusflow.now()))
    focusflow.store.flush()
  end
  focusflow.log("脚手架起来了")
end

function get_view()
  local booted = focusflow.store.get("booted_at")
  local recent = focusflow.stats(7)
  return {{
    title = "{stem}",
    widgets = {{
      {{ type = "heading", text = "脚手架" }},
      {{ type = "keyvalue", key = "首次启动", value = tostring(booted) }},
      {{ type = "keyvalue", key = "近 7 天", value = tostring(recent) }},
      {{ type = "label", text = "改 plugins/{stem}.lua 里的 get_view()，关掉再打开这一页就能看到变化。" }},
    }},
  }}
end
"#
    )
}

/// 一次夹具跑的全部输入（解析与执行分家：判据要能只问"这串参数读成了什么"）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct RunReq {
    file: PathBuf,
    data: Option<PathBuf>,
    event: Fixture,
    app: String,
    action: String,
}

/// 跑哪一拍。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fixture {
    /// 只走加载（`init()`）与取视图
    View,
    /// 走一拍 `pump_timers`：到点的定时器与墙钟那两个事件（`tick` / `day_change`）**在同一拍一起到**
    Timer,
    /// 先投一个前台应用名，再走同一拍
    AppChange,
    /// 点一个按钮动作，然后照常取视图
    Action,
}

const EVENT_HINT: &str = "view | timer | app_change | action";

/// 读 `run` 的那串参数。
///
/// 少给一个"这条命令问不出答案"的参数就要退非 0：`app_change` 不给名字、`action` 不给 id，
/// 跑起来都是"插件什么都没干"，而那与"夹具跑了、确实什么都没干"是两件事。
fn parse_run(args: &[String]) -> Result<RunReq, String> {
    let mut req = RunReq {
        file: PathBuf::new(),
        data: None,
        event: Fixture::View,
        app: String::new(),
        action: String::new(),
    };
    let mut i = 0;
    while i < args.len() {
        // 取值的那一口要能推进游标，所以写成闭包时把 `i` 的所有权问题留在原地：
        // 这里手动推进，不用闭包（闭包要同时改 `i` 与读 `args`，两处借用会打架）。
        let flag = args[i].as_str().to_string();
        let mut value = |what: &str| -> Result<String, String> {
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| format!("{what} 后面要跟一个值（这一问根本没问到）"))
        };
        match flag.as_str() {
            "--data" => req.data = Some(PathBuf::from(value("--data")?)),
            "--event" => {
                let v = value("--event")?;
                req.event = match v.as_str() {
                    "view" => Fixture::View,
                    "timer" => Fixture::Timer,
                    "app_change" => Fixture::AppChange,
                    "action" => Fixture::Action,
                    other => {
                        return Err(format!(
                            "--event 认不出 {other}（只有 {EVENT_HINT}：名字按 Lua 里 `on(...)` 写的那一套，不是 Rust 侧的钩子名）"
                        ))
                    }
                };
            }
            "--app" => req.app = value("--app")?,
            "--action" => req.action = value("--action")?,
            other if other.starts_with('-') => {
                return Err(format!("认不出选项 {other}（-h 看用法）"))
            }
            other => {
                if !req.file.as_os_str().is_empty() {
                    return Err(format!(
                        "run 只接一份插件路径，多出来的那一个不会执行：{other}"
                    ));
                }
                req.file = PathBuf::from(other);
            }
        }
        i += 1;
    }
    if req.file.as_os_str().is_empty() {
        return Err("用法: focusflow-plugin run <插件的 .lua 路径> [--event ...]".to_string());
    }
    if req.event == Fixture::AppChange && req.app.is_empty() {
        return Err("app_change 这一拍要给 --app <应用名>：夹具投出去的就是那一个名字".to_string());
    }
    if req.event == Fixture::Action && req.action.is_empty() {
        return Err("action 这一拍要给 --action <动作 id>：点的是控件上那一个 id".to_string());
    }
    Ok(req)
}

/// 两个路径是不是同一份文件（Windows 上 `copy(x, x)` 会直接失败）。
///
/// 规范化失败就当作"不是"：目标那一份还不存在时正该走复制，而源那一份读不出来
/// 在更早的 `validate_path` 就报过了。
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// 把插件真跑一遍，返回宿主读出来的那一份视图 JSON。
///
/// 三件事决定了这条命令的价值，都写在实现里而不是文档里：
/// · **闸是开着的**：授权走产品那一条 `write_grants`、按声明落，落点默认在临时程序目录；
///   未声明的调用会被当场拒掉，`get_view` 就红在这里 —— 与插件页上看到的是同一件事；
/// · **身份按位置定**：这份文件会被拷进 `<数据目录>/plugins/` 再以用户插件加载，
///   所以作者拿到的 API 视图与真装上时一致（拷进内置目录那条路不是这条命令走的）；
/// · **一拍就是一拍**：`timer` 与 `app_change` 都靠 `pump_timers` 那一拍投递，墙钟那两个
///   事件在同一拍一起到 —— 工具不假装能单独选中其中一个。
fn run_fixture(req: &RunReq) -> Result<String, String> {
    let report = plugins::validate::validate_path(&req.file)
        .map_err(|e| format!("{e} —— 清单都读不出来，夹具没开始跑"))?;
    let stem = plugins::manager::PluginManager::stem_of(&req.file);

    let dir = match &req.data {
        Some(d) => d.clone(),
        None => std::env::temp_dir().join(SCRATCH_DIR_NAME),
    };
    // 顺序要紧：配置那份 OnceLock 是从"当前 app_dir 下的 config.ini"初始化出来的，
    // 先 instance() 再 set_app_dir() 会让这一跑读的还是上一个目录。
    paths::set_app_dir(&dir);
    eprintln!("夹具：程序目录 {}", dir.display());

    let installed = paths::plugins_user_dir().join(format!("{stem}.lua"));
    let package = paths::plugins_user_dir().join(format!("{stem}.zip"));
    if package.exists() {
        // 同一格出现两份同 stem 的插件时，"哪一份被加载"取决于文件名顺序（`.lua` 排在
        // `.zip` 前面）—— 而这一跑是按手上这份 `.lua` 跑出来的结论。悄悄写下去等于
        // 让工具顺手改掉产品下次会加载的那一份。
        return Err(format!(
            "{} 里已经有一份同名的插件包 {}：再放一份单文件，谁被加载就取决于文件名顺序，\
             这一跑测的也不一定是那一份 —— 换一个程序目录，或先把包挪走",
            dir.display(),
            package.display()
        ));
    }
    // 作者那份本来就在这一格里（手工放进数据目录的插件是产品支持的形态）：
    // `copy(x, x)` 在 Windows 上直接是一个错，而"跑我盘上这一份"本该跑得动。
    if !same_file(&req.file, &installed) {
        std::fs::copy(&req.file, &installed)
            .map_err(|e| format!("{} 装不进 {}: {e}", req.file.display(), installed.display()))?;
    }

    let declared = report.manifest.permissions.clone();
    if !declared.is_empty() {
        let grants = permissions::Grants {
            permission_hash: permissions::permission_hash(&declared),
            content_hash: permissions::content_hash(
                std::fs::read(&installed)
                    .map_err(|e| format!("{} 的字节读不出来: {e}", installed.display()))?
                    .as_slice(),
            ),
            granted: declared.clone(),
        };
        permissions::write_grants(&stem, &grants)
            .map_err(|e| format!("按声明落授权失败了: {e}"))?;
        eprintln!(
            "夹具：已按声明（{}）落了一份授权，位置在 {} 那一格的 grants.json；运行时那道闸照常生效",
            declared.join(" / "),
            dir.display()
        );
    }

    let config: &'static config::FocusFlowConfig = config::instance();
    let mut pm = plugins::manager::PluginManager::new(config, db::Database::init_readonly());
    let name = pm
        .load_plugin(&installed, PluginSource::User)
        .map_err(|e| format!("{stem} 装不起来：{e}"))?;

    match req.event {
        Fixture::View => {}
        Fixture::Timer => {
            let ran = pm.pump_timers();
            eprintln!("夹具：这一拍跑了 {} 格回调", ran.len());
        }
        Fixture::AppChange => {
            // 件口是进程全局的：先清一次，否则上一次跑剩的名字会替这一份"答"出来。
            plugins::plugin_events::reset_app_change();
            let posted = plugins::plugin_events::post_app_change(&req.app);
            let ran = pm.pump_timers();
            eprintln!(
                "夹具：投了 \"{}\"（{}），这一拍跑了 {} 格回调",
                req.app,
                if posted {
                    "算是新的一份"
                } else {
                    "与件口里那一个同名，没再投"
                },
                ran.len()
            );
        }
        Fixture::Action => {
            pm.plugin_action(&name, &req.action)
                .map_err(|e| format!("动作 {} 没跑成：{e}", req.action))?;
            eprintln!("夹具：点了动作 {}", req.action);
        }
    }

    pm.refresh_view(&name)
        .map_err(|e| format!("{stem} 的 get_view 跑不出来：{e}"))?;
    let view = pm
        .get_plugin(&name)
        .and_then(|p| p.view.clone())
        .ok_or_else(|| format!("{stem} 加载起来了却没有视图（get_view 没返回那一份表？）"))?;
    Ok(plugins::view_json::view_to_json_string(&view))
}

#[cfg(test)]
mod tests {
    use super::*;
    use focusflow_core::{config::FocusFlowConfig, db, paths, plugins::permissions};

    /// 程序目录是进程全局的，本套用例逐个跑（同一把锁），并且每个都换一份临时目录。
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 一条命令的工作台：临时程序目录 + 其数据目录下的 plugins/。
    /// 返回（程序目录守卫, 那份插件该放的路径）。
    fn bench(tag: &str, stem: &str) -> (paths::TestAppDir, PathBuf) {
        let app = paths::test_app_dir(tag);
        let dir = paths::plugins_user_dir();
        std::fs::create_dir_all(&dir).expect("临时插件目录");
        (app, dir.join(format!("{stem}.lua")))
    }

    /// 往夹具路径写一份脚本正文。
    fn write(file: &Path, text: &str) {
        std::fs::write(file, text).expect("写夹具");
    }

    /// `validate` / `hash` 都只走 core 那一把尺子：分派臂不许自己抄清单或哈希。
    ///
    /// 抄第二把的下场与本仓其它那几处一样：工具说"这份没问题"、插件页说"要授权"。
    #[test]
    fn the_tools_delegate_both_calipers_to_core() {
        let src = include_str!("main.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        fn body_of<'a>(prod: &'a str, head: &str) -> &'a str {
            let at = prod
                .find(head)
                .unwrap_or_else(|| panic!("{head} 的实现不见了"));
            let rest = &prod[at..];
            &rest[..rest.find("\nfn ").map(|n| n + 1).unwrap_or(rest.len())]
        }
        assert!(
            body_of(prod, "fn hash_lines(file: &Path)")
                .contains("plugins::validate::validate_path(file)"),
            "hash 没走 core 那把折叠尺子（它要算在宿主真会用的那份清单上）"
        );
        assert!(
            body_of(prod, "fn hash_package(file: &Path)").contains("plugins::package::read(")
                && body_of(prod, "fn hash_package(file: &Path)")
                    .contains("plugins::package::script_source("),
            "包的清单必须从包里那份 plugin.lua 读出来（读手是 core 那一把），不许另写一套"
        );
        assert!(
            body_of(prod, "fn hash_text(").contains("plugins::permissions::permission_hash(")
                && body_of(prod, "fn hash_text(").contains("plugins::permissions::content_hash("),
            "哈希该由 permissions.rs 出，工具自己算一份就会与授权那道闸对不上"
        );
        assert_eq!(
            prod.matches("hash_text(").count(),
            3,
            "两条入口（单文件 / 包）各调一次 + 定义一处：多一处就是有人又抄了一份排版"
        );
        assert!(
            !prod.contains("PLUGIN_PERMISSIONS = { \"") && !prod.contains("Sha256"),
            "工具自己抄了一把清单尺子或哈希实现（判定只该有一个出处）"
        );
    }

    /// 四种结局各有各的出口：没问题退 0、有问题退 1、根本没问到也退 1、少参数那句也退 1。
    #[test]
    fn validate_exit_codes_name_which_outcome_happened() {
        let _g = guard();
        let (app, good) = bench("validate_exit", "good");
        let _ = &app;
        write(
            &good,
            "PLUGIN_API = 2\nPLUGIN_PERMISSIONS = { \"stats.read\" }\nfunction get_view()\n  local s = focusflow.stats(7)\n  return { title = \"t\", widgets = { { type = \"label\", text = tostring(s) } } }\nend\n",
        );
        assert_eq!(
            run(&["validate".into(), good.display().to_string()]),
            0,
            "声明与用法对得上的一份，不该有退出码"
        );
        let bad = good.with_file_name("bad.lua");
        write(
            &bad,
            "PLUGIN_API = 2\nfunction init()\n  focusflow.teleport(1)\nend\n",
        );
        assert_eq!(
            run(&["validate".into(), bad.display().to_string()]),
            1,
            "宿主没有的那一格要说成问题"
        );
        let missing = good.with_file_name("nope.lua");
        assert_eq!(
            run(&["validate".into(), missing.display().to_string()]),
            1,
            "文件读不出来与「这份没问题」是两种结局"
        );
        assert_eq!(run(&["validate".into()]), 1, "没给路径这一问根本没发生");
    }

    /// `hash_lines` 的两把尺子要**分别**跟着各自的东西动：权限换个顺序仍是同一个
    /// permission_hash（授权不该因作者排版而失效），字节动一个字符 content_hash 必须变。
    ///
    /// 这是行为腿。只核"打印了两行 sha256"的那把尺子，摘掉 canonical 照样绿。
    #[test]
    fn the_permission_hash_follows_the_set_while_content_follows_the_bytes() {
        let _g = guard();
        let (app, a) = bench("hash_pair", "a");
        let _ = &app;
        write(
            &a,
            "PLUGIN_API = 2\nPLUGIN_PERMISSIONS = { \"storage.write\", \"stats.read\" }\nfunction get_view()\n  focusflow.store.set(\"k\", \"v\")\n  local s = focusflow.stats(7)\n  return { title = \"t\", widgets = {} }\nend\n",
        );
        let b = a.with_file_name("b.lua");
        write(
            &b,
            "PLUGIN_API = 2\nPLUGIN_PERMISSIONS = { \"stats.read\", \"storage.write\" }\nfunction get_view()\n  local s = focusflow.stats(7)\n  focusflow.store.set(\"k\", \"v\")\n  return { title = \"t\", widgets = {} }\nend\n",
        );

        let text_a = hash_lines(&a).expect("夹具 a 该读得动");
        let text_b = hash_lines(&b).expect("夹具 b 该读得动");
        let perm = |t: &str| {
            t.lines()
                .find(|l| l.starts_with("permission_hash = "))
                .and_then(|l| l.split_once("= "))
                .map(|(_, v)| v.trim().to_string())
                .unwrap_or_default()
        };
        let content = |t: &str| {
            t.lines()
                .find(|l| l.starts_with("content_hash"))
                .and_then(|l| l.split_once("= "))
                .map(|(_, v)| v.trim().to_string())
                .unwrap_or_default()
        };
        assert!(
            perm(&text_a).starts_with("sha256:") && content(&text_a).starts_with("sha256:"),
            "两行里少了一个 sha256 值：{}",
            text_a
        );
        assert_eq!(
            perm(&text_a),
            perm(&text_b),
            "同一组权限换个顺序就要重新点一次同意 —— canonical 那一步没走到打印的数上"
        );
        assert_ne!(
            content(&text_a),
            content(&text_b),
            "字节不同而 content_hash 相同 ⇒ 那句「代码已变更」永远说不出口"
        );
        // 打印的那两个数必须就是 core 算出的那两个（不是键名对了、数是抄的）。
        assert_eq!(
            perm(&text_a),
            permissions::permission_hash(&["stats.read".to_string(), "storage.write".to_string()]),
            "打印的 permission_hash 与 core 那把尺子不是一对"
        );
        assert_eq!(
            content(&text_a),
            permissions::content_hash(&std::fs::read(&a).expect("夹具字节")),
            "打印的 content_hash 与 core 那把尺子不是一对"
        );
    }

    /// 声明里写了宿主不认得的那一项时，工具要说一句，而打印的数仍要与加载侧算的一致。
    ///
    /// 两个数一对不上，作者拿去核对授权就永远核不过 —— 而认不得的那一项**不会被从清单里删掉**
    /// （折叠只排序去重），它只是不会在运行时装闸。
    #[test]
    fn an_unknown_permission_is_named_without_moving_the_hash() {
        let _g = guard();
        let (app, file) = bench("hash_unknown", "u");
        let _ = &app;
        write(
            &file,
            "PLUGIN_API = 2\nPLUGIN_PERMISSIONS = { \"teleport\" }\nfunction get_view()\n  return { title = \"t\", widgets = {} }\nend\n",
        );
        let text = hash_lines(&file).expect("这份读得动");
        assert!(
            text.contains("teleport") && text.contains("不认得"),
            "清单外那一项没被点名：{}",
            text
        );
        assert_eq!(
            perm_of(&text),
            permissions::permission_hash(&["teleport".to_string()]),
            "工具打印的数与加载侧那道闸算的数不是一对（声明不该被工具再折一遍）"
        );
    }

    fn perm_of(text: &str) -> String {
        text.lines()
            .find(|l| l.starts_with("permission_hash = "))
            .and_then(|l| l.split_once("= "))
            .map(|(_, v)| v.trim().to_string())
            .unwrap_or_default()
    }

    /// `new` 生成的那份脚手架要能被**真加载器**装起来、闸是开着的、视图不是空的。
    ///
    /// 这条腿是这个命令的全部意义：脚手架只在工具自己嘴里"没问题"的话，每个照着它写的插件
    /// 都会撞上"页面显示要授权 / 加载失败"，而作者以为是自己的工作坏了。
    #[test]
    fn the_scaffold_loads_in_the_real_manager_and_validates_clean() {
        let _g = guard();
        let (app, file) = bench("scaffold_load", "demo_plugin");
        assert_eq!(run(&["new".into(), file.display().to_string()]), 0);
        assert!(file.exists(), "说了已生成却没落盘");

        let report = plugins::validate::validate_path(&file).expect("脚手架该读得动");
        assert!(
            !report.has_errors(),
            "脚手架自己被校验器判出问题：\n{}",
            report.render()
        );
        assert_eq!(report.manifest.api, 2, "脚手架没走 v2 那一档");
        assert_eq!(
            report.manifest.permissions,
            permissions::canonical(&[
                "stats.read".into(),
                "storage.read".into(),
                "storage.write".into()
            ]),
            "脚手架声明的那三项与折叠后的对不上"
        );

        // 真加载那一步：按**声明**落一份真 grants.json（闸照常生效），
        // 装起来、跑 get_view、核 widgets 不是空的。
        let declared = report.manifest.permissions.clone();
        permissions::write_grants(
            "demo_plugin",
            &permissions::Grants {
                permission_hash: permissions::permission_hash(&declared),
                content_hash: permissions::content_hash(
                    std::fs::read(&file).expect("脚手架字节").as_slice(),
                ),
                granted: declared,
            },
        )
        .expect("按声明落授权该成功");

        let config: &'static FocusFlowConfig = Box::leak(Box::new(
            FocusFlowConfig::load(app.path().join("config.ini")).expect("读配置失败"),
        ));
        let mut pm = plugins::manager::PluginManager::new(config, db::Database::init_readonly());
        let name = pm
            .load_plugin(&file, plugins::PluginSource::User)
            .unwrap_or_else(|e| panic!("脚手架装不起来: {e}"));
        pm.refresh_view(&name)
            .unwrap_or_else(|e| panic!("脚手架的 get_view 跑不出来: {e}"));
        let view = pm
            .get_plugin(&name)
            .and_then(|p| p.view.clone())
            .expect("装起来了却没有视图");
        assert_eq!(view.title, "demo_plugin", "视图标题没接上清单那一份");
        assert!(
            view.widgets.len() >= 3,
            "脚手架的视图只有 {} 个控件 —— 页面上会是一片白",
            view.widgets.len()
        );
        // 首次起来那一句要真落进通用存储（storage.write 那条腿走的是真文件）。
        assert!(
            plugins::plugin_store::get("demo_plugin", "booted_at").is_ok(),
            "init() 里那次 store.set 没落下去 —— 脚手架展示的写入路径是坏的"
        );
    }

    /// 已存在的文件不许被覆盖：这条拒绝要有真读者（字节一个都没变）。
    #[test]
    fn new_refuses_to_overwrite_and_says_so() {
        let _g = guard();
        let (app, file) = bench("overwrite", "taken");
        let _ = &app;
        write(&file, "-- 作者写的一半东西\n");
        assert_eq!(
            run(&["new".into(), file.display().to_string()]),
            1,
            "覆盖同名文件这件事必须退非 0"
        );
        assert_eq!(
            std::fs::read_to_string(&file).expect("读回"),
            "-- 作者写的一半东西\n",
            "说了不覆盖却改了字节"
        );
        assert_eq!(run(&["new".into()]), 1, "没给路径这一问根本没发生");
    }

    /// `run` 的参数解析：问不出答案的那几种写法各有出口。
    ///
    /// `--event app_change` 不给名字、`--event action` 不给 id 若照样跑，屏上会是一句
    /// "插件什么都没干"，而那与"夹具跑了、确实什么都没干"是两件事。
    #[test]
    fn run_option_parsing_names_what_it_cannot_answer() {
        let p = |args: &[&str]| -> Vec<String> { args.iter().map(|s| s.to_string()).collect() };
        assert!(parse_run(&p(&[])).is_err(), "没给路径这一问根本没发生");
        assert!(
            parse_run(&p(&["a.lua", "b.lua"])).is_err(),
            "第二份路径不该被当没看见"
        );
        assert!(
            parse_run(&p(&["a.lua", "--nope"])).is_err(),
            "认不出的选项要说成认不出"
        );
        assert!(
            parse_run(&p(&["a.lua", "--data"])).is_err(),
            "--data 后面那个值没给"
        );
        let bad_event = parse_run(&p(&["a.lua", "--event", "weekly"])).unwrap_err();
        assert!(
            bad_event.contains("view") && bad_event.contains("app_change"),
            "拒绝那一问要把认得出的那几个名字说出来：{bad_event}"
        );
        assert!(
            parse_run(&p(&["a.lua", "--event", "app_change"])).is_err(),
            "没给 --app"
        );
        assert!(
            parse_run(&p(&["a.lua", "--event", "action"])).is_err(),
            "没给 --action"
        );
        assert!(parse_run(&p(&[
            "a.lua",
            "--event",
            "app_change",
            "--app",
            "记事本.exe"
        ]))
        .is_ok());
        let plain = parse_run(&p(&["a.lua"])).expect("只给路径该读得动");
        assert_eq!(
            plain.event,
            Fixture::View,
            "缺省那一拍是「只 init + 取视图」"
        );
        assert!(plain.data.is_none(), "缺省不该指向某个真目录");
    }

    /// 夹具那一拍要真的动了手：`--event timer` 之后视图里读的数比 `--event view` 之后大。
    ///
    /// 两条腿一起写才算数 —— 只测"跑了 timer 那一拍然后屏上有东西"，摘掉 pump 也一样绿
    /// （init 里那次写就把数落下了）。这里缺省那一拍核的是"没跑回调时它是 0"。
    #[test]
    fn the_timer_beat_actually_runs_the_callback_the_view_beat_does_not() {
        let _g = guard();
        let lua = "\
PLUGIN_NAME = \"beat\"
PLUGIN_API = 2
PLUGIN_PERMISSIONS = { \"timer\", \"storage.read\", \"storage.write\" }
function init()
  if focusflow.store.get(\"n\") == nil then
    focusflow.store.set(\"n\", \"0\")
    focusflow.store.flush()
  end
  focusflow.on(\"tick\", function()
    local cur = tonumber(focusflow.store.get(\"n\")) or 0
    focusflow.store.set(\"n\", tostring(cur + 1))
    focusflow.store.flush()
  end)
end
function get_view()
  return { title = \"beat\", widgets = { { type = \"label\", text = \"n=\" .. tostring(focusflow.store.get(\"n\")) } } }
end
";
        let beat = |event: &str| -> String {
            // 每一拍都换一份新的草稿目录：状态留在上一份里就分不出是谁写的。
            // 作者那份源码要放在 plugins/ 之外 —— `run` 会把手上这份拷进数据目录的
            // plugins/ 里再加载，同源同路径那种写法在 Windows 上直接是一个错。
            let app = focusflow_core::paths::test_app_dir("run_beat");
            let author = app.path().join("author");
            std::fs::create_dir_all(&author).expect("作者目录");
            let file = author.join("beat.lua");
            std::fs::write(&file, lua).expect("写夹具");
            let mut args = vec![
                file.display().to_string(),
                "--data".to_string(),
                app.path().display().to_string(),
            ];
            if !event.is_empty() {
                args.push("--event".to_string());
                args.push(event.to_string());
            }
            let req = parse_run(&args).expect("参数读得动");
            run_fixture(&req).expect("夹具该跑得动")
        };
        let view_only = beat("");
        assert!(
            view_only.contains("n=0"),
            "只取视图那一拍不该跑回调（init 只把计数落成 0）：{view_only}"
        );
        let pumped = beat("timer");
        assert!(
            pumped.contains("n=1"),
            "跑了那一拍之后计数该是 1（回调真被执行）：{pumped}"
        );
    }

    /// 印出来的是**宿主读出来的那一份**：作者写 95 个点，解析侧夹到上限，工具就该印 90 个。
    ///
    /// 这一条是 `run` 存在的理由之一：作者以为画了 95 天，页面上只有最近 90 天。
    #[test]
    fn the_json_prints_the_clipped_chart_not_the_written_one() {
        let _g = guard();
        let app = focusflow_core::paths::test_app_dir("run_chart");
        let author = app.path().join("author");
        std::fs::create_dir_all(&author).expect("作者目录");
        let file = author.join("clip.lua");
        std::fs::write(
            &file,
            "\
PLUGIN_NAME = \"clip\"
PLUGIN_API = 2
function get_view()
  local labels = {}
  local points = {}
  for i = 0, 94 do
    labels[i + 1] = \"L\" .. i
    points[i + 1] = i
  end
  return { title = \"clip\", widgets = { { type = \"chart\", kind = \"line\", labels = labels, points = points } } }
end
",
        )
        .expect("写夹具");
        let req = parse_run(&[
            file.display().to_string(),
            "--data".to_string(),
            app.path().display().to_string(),
        ])
        .expect("参数读得动");
        let json = run_fixture(&req).expect("夹具该跑得动");
        let points_at = json.find("\"points\"").expect("JSON 里没有 points");
        // 切片坐标要落在同一个起点上：`json[at..].find(..)` 给的是**从那一位之后**的偏移，
        // 直接拿去当 `json` 的结尾用就是越界（这条先是把自己切反了，不是产品的问题）。
        let tail = &json[points_at..];
        let block = &tail[..tail.find(']').expect("points 数组没闭合") + 1];
        let printed = block
            .lines()
            .filter(|l| l.contains(|c: char| c.is_ascii_digit()))
            .count();
        assert_eq!(
            printed,
            focusflow_core::plugins::MAX_CHART_POINTS,
            "印出来的点数不是解析侧夹过的那一份"
        );
        let numbers: Vec<i64> = block
            .lines()
            .filter_map(|l| l.trim().trim_end_matches(',').parse::<i64>().ok())
            .collect();
        assert_eq!(
            numbers.first().copied(),
            Some(5),
            "夹的是开头那五个点，印出来的第一个数该是 5；不是就说明夹了尾部（最新那段被藏起来了）"
        );
        assert_eq!(
            numbers.last().copied(),
            Some(94),
            "最新那一个点（94）必须还在：作者写 95 个点，页面上画的是最近 90 个"
        );
        assert!(
            json.contains("\"title\": \"clip\"") && json.contains("\"type\": \"chart\""),
            "顶层形状不对：{json}"
        );
    }

    /// 视图的词汇表只有一份出处：JSON 必须由 core 那把序列化尺子出，工具不自己排版。
    #[test]
    fn the_json_comes_from_core_not_from_a_second_printer() {
        let src = include_str!("main.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let at = prod
            .find("fn run_fixture(req: &RunReq)")
            .expect("run_fixture 的实现不见了");
        let rest = &prod[at..];
        let body = &rest[..rest.find("\n#[cfg(test)]").unwrap_or(rest.len())];
        assert!(
            body.contains("plugins::view_json::view_to_json_string("),
            "视图没走 core 那把序列化尺子（新加一档控件时工具会静悄悄少印一档）"
        );
        assert!(
            body.contains("permissions::write_grants(")
                && body.contains("load_plugin(&installed, PluginSource::User)"),
            "夹具要么没落真授权、要么没按用户插件那一格的身份加载 —— 两种都是在关掉闸跑这一条"
        );
        assert!(
            !prod.contains("serde_json"),
            "工具自己排了一份 JSON（两处排版会分家，而 jq 读的是工具这一份）"
        );
    }

    /// 作者那份本来就在数据目录的 `plugins/` 里（手工放插件是产品支持的形态）：
    /// `run --data` 该跑得动，而不是把 `copy(x, x)` 那个 Windows 错误甩给他。
    #[test]
    fn a_plugin_already_in_the_target_plugins_dir_still_runs() {
        let _g = guard();
        let app = paths::test_app_dir("run_in_place");
        let dir = paths::plugins_user_dir();
        std::fs::create_dir_all(&dir).expect("临时插件目录");
        let in_place = dir.join("inplace.lua");
        write(
            &in_place,
            "PLUGIN_NAME = \"inplace\"\nfunction get_view() return { widgets = {{ type = \"label\", text = \"原地\" }} } end\n",
        );
        let req = parse_run(&[
            in_place.display().to_string(),
            "--data".to_string(),
            app.path().display().to_string(),
        ])
        .expect("参数读得动");
        let json = run_fixture(&req).expect("就在目标目录里的那一份该跑得动");
        assert!(json.contains("原地"), "{json}");
    }

    /// 同一格已经有一份同名的包：再放一份单文件，谁被加载取决于文件名顺序，
    /// 而这一跑还会悄悄把那份包顶掉。拒绝，并且一个字节都不写。
    #[test]
    fn run_refuses_to_shadow_a_same_stem_package() {
        let _g = guard();
        let (app, file) = author_file("run_shadow", "dup");
        write(
            &file,
            "PLUGIN_NAME = \"dup\"\nfunction get_view() return { widgets = {} } end\n",
        );
        let dir = paths::plugins_user_dir();
        std::fs::create_dir_all(&dir).expect("临时插件目录");
        std::fs::write(dir.join("dup.zip"), b"not really a zip").expect("放一份同名的包");
        let req = parse_run(&[
            file.display().to_string(),
            "--data".to_string(),
            app.path().display().to_string(),
        ])
        .expect("参数读得动");
        let err = run_fixture(&req).expect_err("同名的包挡着，这一跑不许动它");
        assert!(err.contains("同名") && err.contains("dup.zip"), "{err}");
        assert!(
            !dir.join("dup.lua").exists(),
            "被拒的那一跑还是写了一份单文件"
        );
    }

    /// 把"作者那份"写到 plugins/ 之外，返回（程序目录, 那份文件的路径）。
    ///
    /// 不能直接放在数据目录的 `plugins/` 里：`run` 会把手上这份拷进那一格再加载，
    /// 同源同路径这种写法在 Windows 上就是一个错（这一场踩过一次）。
    fn author_file(tag: &str, stem: &str) -> (paths::TestAppDir, PathBuf) {
        let app = paths::test_app_dir(tag);
        let author = app.path().join("author");
        std::fs::create_dir_all(&author).expect("作者目录");
        let file = author.join(format!("{stem}.lua"));
        (app, file)
    }

    /// 夹具那两拍要各自有各自的一次跑，缺省那一拍当对照臂：
    /// 摘掉投件或摘掉点动作，红点必须出现在"载荷没到手"那一句上，而不是永远绿。
    fn run_in(app: &paths::TestAppDir, file: &Path, extra: &[&str]) -> String {
        let mut args: Vec<String> = vec![
            file.display().to_string(),
            "--data".to_string(),
            app.path().display().to_string(),
        ];
        args.extend(extra.iter().map(|s| s.to_string()));
        let req = parse_run(&args).expect("参数读得动");
        run_fixture(&req).expect("夹具该跑得动")
    }

    /// `--event app_change --app 名字` 那一拍：投出去的名字要落到插件自己写的那一格里。
    ///
    /// 对照臂是缺省那一拍（什么都没投）：只有两臂都在，"屏上出现了那个名字"才不是插件
    /// 自己凭空写的、也不是上一拍剩在盘上的。
    #[test]
    fn the_app_change_beat_delivers_the_posted_name_into_the_plugin() {
        let _g = guard();
        let (app, file) = author_file("run_ac", "ac");
        std::fs::write(
            &file,
            "\
PLUGIN_NAME = \"ac\"
PLUGIN_API = 2
PLUGIN_PERMISSIONS = { \"events.app\", \"storage.read\", \"storage.write\" }
function init()
  focusflow.on(\"app_change\", function(name)
    focusflow.store.set(\"who\", name)
    focusflow.store.flush()
  end)
end
function get_view()
  return { title = \"ac\", widgets = { { type = \"label\", text = \"who=\" .. tostring(focusflow.store.get(\"who\")) } } }
end
",
        )
        .expect("写夹具");
        let untouched = run_in(&app, &file, &[]);
        assert!(
            untouched.contains("who=nil"),
            "什么都没投的那一拍不该已经有名字：{untouched}"
        );

        let (app2, file2) = author_file("run_ac2", "ac");
        std::fs::write(&file2, std::fs::read_to_string(&file).expect("读夹具")).expect("写第二臂");
        let posted = run_in(
            &app2,
            &file2,
            &["--event", "app_change", "--app", "记事本.exe"],
        );
        assert!(
            posted.contains("who=记事本.exe"),
            "投出去的那个名字没到插件手里：{posted}"
        );
    }

    /// `--event action --action id` 那一拍：点下去的 id 要真的进到 `on_action`。
    #[test]
    fn the_action_beat_clicks_the_button_the_plugin_actually_handles() {
        let _g = guard();
        let lua = "\
PLUGIN_NAME = \"act\"
PLUGIN_API = 2
PLUGIN_PERMISSIONS = { \"storage.read\", \"storage.write\" }
function on_action(id)
  focusflow.store.set(\"last\", id)
  focusflow.store.flush()
end
function get_view()
  return { title = \"act\", widgets = { { type = \"label\", text = \"last=\" .. tostring(focusflow.store.get(\"last\")) } } }
end
";
        let (app, file) = author_file("run_act", "act");
        std::fs::write(&file, lua).expect("写夹具");
        let untouched = run_in(&app, &file, &[]);
        assert!(
            untouched.contains("last=nil"),
            "没点动作那一拍不该有值：{untouched}"
        );

        let (app2, file2) = author_file("run_act2", "act");
        std::fs::write(&file2, lua).expect("写第二臂");
        let clicked = run_in(&app2, &file2, &["--event", "action", "--action", "保存"]);
        assert!(
            clicked.contains("last=保存"),
            "点下去的那个 id 没进 on_action：{clicked}"
        );
    }

    /// 随包那几份脚本要走一遍**新壳**：`validate` 与 `hash` 都不许对它们说有问题。
    ///
    /// 上一场那条真数据腿走的是 core 的尺子（`plugins::validate`）；这一场把壳从命令行搬到
    /// `focusflow-plugin`，判定一行没动，但"壳换了以后随包脚本还是干净的"这件事**没人核过**。
    /// 这一条就是那一次核。身份按位置定，所以先把它们拷进内置那一格再问 ——
    /// 拿用户位问它们，`pomodoro_*` / `accounting_*` 那些格子本来就该被念成问题。
    #[test]
    fn the_shipped_scripts_stay_clean_through_the_new_shell() {
        let _g = guard();
        let src = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../core/plugins"));
        let app = paths::test_app_dir("tool_shipped");
        let builtin = paths::plugins_builtin_dir();
        std::fs::create_dir_all(&builtin).expect("内置目录");

        let mut seen = 0usize;
        for entry in std::fs::read_dir(src).expect("读随包脚本目录") {
            let from = entry.expect("条目").path();
            if from.extension().and_then(|e| e.to_str()) != Some("lua") {
                continue;
            }
            let stem = from
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let dst = builtin.join(from.file_name().unwrap());
            std::fs::copy(&from, &dst).unwrap_or_else(|e| panic!("{stem} 拷不进内置位: {e}"));
            seen += 1;
            assert_eq!(
                run(&["validate".into(), dst.display().to_string()]),
                0,
                "随包的 {stem} 在新壳里被判出问题 —— 换壳该一动不动，动的是壳不是尺子"
            );
            assert_eq!(
                run(&["hash".into(), dst.display().to_string()]),
                0,
                "随包的 {stem} 连哈希都算不出，工具对它等于没用"
            );
        }
        assert!(
            seen >= 5,
            "随包脚本只数到 {seen} 份 —— 这一条腿的夹具是这几份文件本身，数少了就是扫空"
        );
        drop(app);
    }

    /// 多出来的参数不能被静悄悄丢掉：`hash a.lua b.lua` 只算 a 而退 0，
    /// 作者以为两份都核过了。
    #[test]
    fn extra_arguments_are_rejected_before_anything_runs() {
        let _g = guard();
        let (app, a) = bench("extra_args", "e");
        let _ = &app;
        write(&a, "PLUGIN_API = 1\n");
        assert_eq!(
            run(&[
                "hash".into(),
                a.display().to_string(),
                a.with_file_name("b.lua").display().to_string()
            ]),
            1,
            "多出来的那个路径必须被拒绝，而不是当没看见"
        );
        assert_eq!(run(&["nope".into()]), 1, "未知命令不该退 0");
        assert_eq!(
            run(&["run".into(), "x.lua".into()]),
            1,
            "没接的命令要说成没接"
        );
    }

    /// `pack` 的产出必须能被工具自己另外两条命令读回去：打包不是"写出一个文件就算完"
    /// （方案第 16 步与第 18 步的 `pack` 一起落，就是为了不出现"写了没人读"）。
    ///
    /// 三条一起核：①同一棵目录打两次是同一串字节；②包上的 `permission_hash` 等于那份
    /// `.lua` 的（清单来自包里那份脚本）；③`content_hash` 算在包字节上。
    #[test]
    fn a_packed_plugin_is_readable_by_validate_and_hash() {
        let _g = guard();
        let (app, lua) = bench("pack_roundtrip", "demo");
        let _ = &app;
        write(
            &lua,
            "PLUGIN_API = 2\nPLUGIN_PERMISSIONS = { \"stats.read\" }\nfunction get_view()\n  local s = focusflow.stats(7)\n  return { title = \"t\", widgets = {} }\nend\n",
        );
        let src = lua.with_file_name("demo_pkg");
        std::fs::create_dir_all(src.join("assets/icons")).unwrap();
        std::fs::write(src.join("plugin.lua"), std::fs::read(&lua).unwrap()).unwrap();
        std::fs::write(src.join("assets/icons/a.txt"), "a").unwrap();

        // 缺省落点：目录旁边同名 .zip
        assert_eq!(
            run(&["pack".into(), src.display().to_string()]),
            0,
            "打一份插件包不该失败"
        );
        let first = src.with_extension("zip");
        let bytes_first = std::fs::read(&first).expect("缺省落点那份包");
        // 再打一次到另一个路径：同一棵树必须给出同一串字节
        let again = src.with_file_name("demo_pkg_again.zip");
        assert_eq!(
            run(&[
                "pack".into(),
                src.display().to_string(),
                "-o".into(),
                again.display().to_string()
            ]),
            0
        );
        assert_eq!(
            std::fs::read(&again).unwrap(),
            bytes_first,
            "同一棵目录打两次不是同一串字节：内容哈希会跟着抖，用户每次都被提示「插件已更新」"
        );

        assert_eq!(
            run(&["validate".into(), first.display().to_string()]),
            0,
            "包里那份 plugin.lua 与单文件是同一把尺子，它不该被判出问题"
        );
        assert_eq!(
            run(&["hash".into(), first.display().to_string()]),
            0,
            "包也要算得出两个哈希"
        );
        let line = |text: &str, key: &str| {
            text.lines()
                .find(|l| l.starts_with(key))
                .unwrap_or_default()
                .to_string()
        };
        let plain = hash_lines(&lua).unwrap();
        let packed = hash_target(&first).unwrap();
        assert_eq!(
            line(&plain, "permission_hash"),
            line(&packed, "permission_hash"),
            "权限集合没变，包与单文件的 permission_hash 必须相同"
        );
        assert_eq!(
            line(&packed, "content_hash"),
            format!(
                "content_hash    = {}",
                plugins::permissions::content_hash(&bytes_first)
            ),
            "包的 content_hash 算在包字节上（重打包=同一串字节，才敢用它提示更新）"
        );
        let _ = std::fs::remove_dir_all(&src);
    }

    /// 不是插件目录就不许打包，而且拒绝时不许留下半个包。
    #[test]
    fn pack_refuses_a_directory_that_is_not_a_plugin() {
        let _g = guard();
        let (app, lua) = bench("pack_bad", "x");
        let _ = &app;
        let dir = lua.with_file_name("not_a_plugin");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("readme.md"), "hi").unwrap();
        assert_eq!(
            run(&["pack".into(), dir.display().to_string()]),
            1,
            "根上没有 plugin.lua 的目录不是插件"
        );
        assert!(
            !dir.with_extension("zip").exists(),
            "拒绝的时候不许留下半个包（作者会以为打成了）"
        );
        assert_eq!(
            run(&["pack".into(), dir.display().to_string(), "-o".into()]),
            1,
            "-o 后面没给值 = 这一问根本没问到"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
