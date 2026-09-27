//! 数据目录的迁移：把 `data/` + `backup/` 整体搬到用户另选的目录。
//!
//! 程序目录与数据目录的分工见 [`crate::paths::data_home`]。把数据挪出去这件事，产品上
//! 刻意**不做运行中热切换** —— 写线程持长连接、只读连接是线程本地按路径缓存的，
//! 备份/采样/统计这些常驻线程又随时惰性重解析路径；一旦中途改指向，同一次运行里就会有
//! 一半线程写旧目录、一半写新目录，数据静默分裂，比不切换更糟。
//!
//! 所以搬运分成两步、跨一次重启：点按钮的那个进程只写配置（新 `data_home` +
//! `data_migrate_from` 标记）然后重启；**新进程**在开库之前按标记把旧目录搬过来
//! （见 [`run_pending_migration`]）。这样旧进程是把自己那套写完整、备份完整才死的，
//! 搬走的就是完整结果，不会留下一个"看着还在被用、其实早就停了"的旧目录。
//!
//! 删源只发生在复制成功**且逐文件核对通过**之后；任何一步没成，源侧原封不动、标记留着
//! 下次重试。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::paths;

/// 复制结果汇总。
#[derive(Debug, Default)]
pub struct CopySummary {
    /// 复制过去的文件数
    pub copied: usize,
    /// 实际复制的字节数
    pub bytes: u64,
    /// 两边同大小、无需再搬而跳过的文件
    pub skipped_existing: Vec<String>,
    /// 目标里原有、已被改名留档的文件（供 UI 告诉用户去哪儿找）
    pub backed_up: Vec<String>,
    /// 出错项（不中断整体：一条错误说明一项）
    pub errors: Vec<String>,
}

impl CopySummary {
    /// 有没有出错（调用方据此决定该不该改写配置并重启）。
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }
}

/// 当前生效的数据根目录（`data/`、`backup/` 挂在它下面）。
pub fn current_data_home() -> PathBuf {
    paths::data_home()
}

/// 改数据目录之前的体检：目标是不是同一个目录、有没有选进正在被搬的子树里、
/// 以及**现在**建不建得出来。
///
/// 这一步必须存在，是因为"选了不合法的目录"原本要等到重启后新进程搬运时才暴露，
/// 而那时界面早没了 —— 用户看到的就只是"点了没反应、数据还在原地"（实测就是这样：
/// 他选了 `程序目录\123`，被判定误杀，错误只躺在日志里）。提前判一次，
/// 报错就能当场显示在设置页上。
pub fn validate_new_data_home(dst: &Path) -> Result<(), String> {
    let src = paths::data_home();
    guard_destination(&src, dst)?;
    std::fs::create_dir_all(dst)
        .map_err(|e| format!("目标目录建不出来（{}）: {e}", dst.display()))?;
    // 目标里除了 data / backup 之外还有别的东西就拒绝：往装着旧版 FocusFlow 的
    // 目录里搬是正常的，但那里已经有无关文件时，搬运结果和用户的预期不一致，
    // 而且事后很难分清哪份是谁的。
    let mut strangers: Vec<String> = Vec::new();
    if let Ok(it) = std::fs::read_dir(dst) {
        for entry in it.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name != "data" && name != "backup" {
                strangers.push(name);
            }
        }
    }
    if !strangers.is_empty() {
        strangers.sort();
        strangers.truncate(6);
        return Err(format!(
            "目标目录里已经有别的内容（{}），请换一个空的或新建的文件夹",
            strangers.join(", ")
        ));
    }
    Ok(())
}

/// 把 `src_root` 下的 `data/` 与 `backup/` 复制到 `dst_root` 的对应子目录。
///
/// 目标已存在的同名文件：同大小就跳过（重复点一次"更改"不该把刚复制好的库再搬
/// 一遍，白白搬一年数据）；大小不同说明目标那份是**别的东西**（用户选了个装着
/// 旧版 FocusFlow 的目录是很常见的），先把它改名留档再写，绝不静默覆盖 ——
/// 源目录本来就一字不动地留着，但目标里原有的那份没有第二份副本。
pub fn copy_data_tree(src_root: &Path, dst_root: &Path) -> CopySummary {
    let mut summary = CopySummary::default();

    // 先挡住"复制会出事"的指向，再动手：
    if let Err(e) = guard_destination(src_root, dst_root) {
        summary.errors.push(e);
        return summary;
    }

    for sub in ["data", "backup"] {
        let src = src_root.join(sub);
        if !src.is_dir() {
            continue;
        }
        let dst = dst_root.join(sub);
        // 兜底，且**每个子树只判一次**：只有目标子树落进正在被复制的那棵源子树里
        // 才会自我嵌套（`dst` 本身在 `src/data` 下面，于是 `src/data` 被复制进
        // `src/data/…/data`，遍历一边读一边往里写，套娃直到撑满磁盘 —— 上一版
        // 就是这么产出 3630 个文件才被测试逮住的）。
        //
        // 刻意**不判**"目标是否落在 src_root 里面"：`data_home` 未设时 src_root 就是
        // 程序目录，而"在程序目录里新建一个子文件夹当数据目录"是完全正当的用法。
        // 上一版的宽判定把用户实测选的 `FocusFlow\123` 当成套娃拒了，而且对
        // `data`、`backup` 各报一遍，目录一个字都没换成。
        if dst.starts_with(&src) {
            summary.errors.push(format!(
                "目标落在当前 {} 子树内部（{}），已拒绝以免自我嵌套；请选一个外面的文件夹",
                sub,
                src.display()
            ));
            continue;
        }
        let mut stack = vec![(src.clone(), dst.clone())];
        while let Some((s, d)) = stack.pop() {
            let entries = match std::fs::read_dir(&s) {
                Ok(it) => it,
                Err(e) => {
                    summary
                        .errors
                        .push(format!("{} 读取失败: {e}", s.display()));
                    continue;
                }
            };
            if let Err(e) = std::fs::create_dir_all(&d) {
                summary
                    .errors
                    .push(format!("{} 创建失败: {e}", d.display()));
                continue;
            }
            for entry in entries.flatten() {
                let from = entry.path();
                let to = d.join(entry.file_name());
                let ft = match entry.file_type() {
                    Ok(ft) => ft,
                    Err(e) => {
                        summary
                            .errors
                            .push(format!("{} 元数据读取失败: {e}", from.display()));
                        continue;
                    }
                };
                if ft.is_dir() {
                    stack.push((from, to));
                    continue;
                }
                if !ft.is_file() {
                    // 符号链接、管道之类：SQLite 数据目录里不该有，跳过了事，
                    // 追进去反而可能把复制引到目录外面去。
                    continue;
                }
                match copy_file(&from, &to, &mut summary) {
                    Ok(()) => {}
                    Err(e) => summary
                        .errors
                        .push(format!("{} 复制失败: {e}", from.display())),
                }
            }
        }
    }

    summary
}

/// 拒绝会让这次复制失去意义的目标：同一个目录、或目标在源的 `data`/`backup` 里面。
///
/// 后者是真会发生的误操作：用户在目录选择器里顺着路径点进了当前的 `data/`，
/// 于是"把 data 复制到 data 里面"，复制出来的是一份套一份的自嵌套树。
///
/// 两侧一律用 [`stable_abs`] 规范化后再比 —— 不能一半 `canonicalize` 一半词法拼接：
/// Windows 的 `canonicalize` 会给出 `\\?\C:\...` 这种 verbatim 前缀，而用户手选的
/// 路径拼出来是 `C:\...`，`starts_with` 于是永远为假，拦截整个形同不存在。
fn guard_destination(src_root: &Path, dst_root: &Path) -> Result<(), String> {
    let src = stable_abs(src_root)
        .ok_or_else(|| format!("源目录读不出来（{}）: 路径无效", src_root.display()))?;
    let dst = stable_abs(dst_root).unwrap_or_else(|| dst_root.to_path_buf());

    if src == dst {
        return Err(format!(
            "目标就是当前的数据目录（{}），无需更改",
            src.display()
        ));
    }
    for sub in ["data", "backup"] {
        let inside = src.join(sub);
        if dst.starts_with(&inside) {
            return Err(format!(
                "目标在当前数据目录内部（{}），请选一个外面的文件夹",
                inside.display()
            ));
        }
    }
    Ok(())
}

/// 绝对化 + 词法折叠 `.`/`..`，**不碰文件系统**。
///
/// 刻意不用 `std::fs::canonicalize`：① 目标目录常常还不存在（新建目录是正常用法），
/// canonicalize 那种情况直接失败；② 它在 Windows 上返回 `\\?\` verbatim 形式，
/// 与用户手选路径的拼法不同形，前缀比较会静默判假（就是上面那段注释说的漏拦截）。
/// 代价是看不清符号链接/junction 的真身 —— 目录选择器给的是真路径，这个代价可以接受。
fn stable_abs(p: &Path) -> Option<PathBuf> {
    match std::path::absolute(p) {
        Ok(abs) => Some(abs),
        // 空串之类的输入 absolute 会 Err；退化成词法折叠，实在也没内容可比就算了。
        Err(_) => (!p.as_os_str().is_empty()).then(|| p.to_path_buf()),
    }
}

/// 单文件复制：同大小且同内容才跳过；否则先把目标改名留档，再整份写过来。
fn copy_file(from: &Path, to: &Path, summary: &mut CopySummary) -> anyhow::Result<()> {
    if to.exists() {
        if same_file(from, to) {
            summary
                .skipped_existing
                .push(format!("{}（源与目标是同一个文件）", from.display()));
            return Ok(());
        }
        // 只比大小不比 mtime：复制会把 mtime 带过去，重复点一次"更改"之后两边
        // 再核一遍内容：SQLite 文件按页分配，两份**内容不同**的库撞出同一个字节数是
        // 常事（没写过几行的库尤其如此）。只比大小的话这种巧合会被判成"已经搬过了"
        // 而跳过，随后删源那一步照样通过 —— 用户的历史就此没了，新目录里躺着的是
        // 别人的那份。
        let same_len = std::fs::metadata(from)
            .and_then(|a| std::fs::metadata(to).map(|b| (a, b)))
            .map(|(a, b)| a.len() == b.len())
            .unwrap_or(false);
        if same_len && contents_equal(from, to) {
            summary.skipped_existing.push(from.display().to_string());
            return Ok(());
        }
        match keep_aside(to) {
            Ok(Some(kept)) => summary.backed_up.push(kept.display().to_string()),
            Ok(None) => {}
            Err(e) => {
                // 留不住就不覆盖：目标那份是唯一副本，宁可这次不搬、报错误让用户处理，
                // 也不能拿它赌一把（同 migration::import_legacy_data 的取舍）。
                summary
                    .errors
                    .push(format!("{} 覆盖前留档失败，已跳过: {e}", to.display()));
                return Ok(());
            }
        }
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let len = std::fs::metadata(from).map(|m| m.len()).unwrap_or(0);
    std::fs::copy(from, to)?;
    summary.copied += 1;
    summary.bytes += len;
    Ok(())
}

/// 把目标上原有的文件改名成 `<原名>.pre-switch-<时间戳>`；它不存在时是 No-op。
///
/// 用 rename 而不是 copy：省一次全量读写，也不会出现"复制到一半失败、原文件与
/// 副本都不完整"的中间态（同 `migration::backup_before_overwrite`）。
fn keep_aside(dst: &Path) -> anyhow::Result<Option<PathBuf>> {
    if !dst.exists() {
        return Ok(None);
    }
    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S%3f").to_string();
    let mut name = dst
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    name.push(format!(".pre-switch-{timestamp}"));
    let kept = dst.with_file_name(name);
    std::fs::rename(dst, &kept)?;
    // WAL 附属文件必须跟着主库走（同 `migration::backup_before_overwrite`）：只搬主库，
    // 目标目录里那份**别人的** `-wal` 就留在原位配上了新主库，SQLite 打开时照那份 wal
    // 恢复 —— 而这次搬运随后还要删源，唯一一份可能就是那个错配的组合。
    // 附属文件本就不存在是常态（多数库早就 checkpoint 了），所以只在存在时搬；
    // 搬不动就当失败：宁可这次不覆盖，也不能留下主库与 wal 成套但来源不同的两份。
    for suffix in ["-wal", "-shm"] {
        let sidecar = with_suffix(dst, suffix);
        if sidecar.exists() {
            std::fs::rename(&sidecar, with_suffix(&kept, suffix))?;
        }
    }
    Ok(Some(kept))
}

/// 在文件名的**末尾**接一段后缀（`focusflow_2026.db` → `focusflow_2026.db-wal`）。
///
/// 走 `OsString` 而不是 `format!("{}", path.display())`：后者对非 UTF-8 文件名是 lossy
/// 转换，拼出来的路径根本不存在 —— 本仓库的复制链在别处也刻意保 `OsString` 原名。
/// `migration::backup_before_overwrite` 共用这一份（那条链原先正是用 lossy 拼接的）。
pub(crate) fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    name.push(suffix);
    path.with_file_name(name)
}

/// 两个路径是不是同一个目录？`PathBuf` 的相等是按字节比的，Windows 上同一个目录
/// 至少有下列写法：`C:\data`、`C:\data\`、`C:/data`、`c:\DATA`。
///
/// 字节不等时才 `canonicalize`（它解析大小写与 `..` 段，并给两边同一个 `\\?\` 前缀）。
/// 任一 `canonicalize` 失败就回 `false`：读不出来的那条由调用方自己的失败路径去报错，
/// 这里不顺手把"读不出来"判成"同一个目录"（那会让一次 U 盘没挂上被说成"不用搬"）。
fn same_directory(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// 搬运结果（供日志与启动报告措辞用）。
#[derive(Debug)]
pub struct MigrationReport {
    /// 旧数据根
    pub from: PathBuf,
    /// 新数据根
    pub to: PathBuf,
    /// 复制汇总
    pub summary: CopySummary,
    /// 没删干净的源侧条目数（删除失败不影响"数据已在新目录"这一事实，只说明要手工清）
    pub leftover_in_source: usize,
}

/// 留给启动报告的一条说明：只有失败才填（全绿不该打扰用户，这是 B15 的既有口径）。
static STARTUP_NOTICE: Mutex<Option<crate::startup::CheckResult>> = Mutex::new(None);

/// 取走启动报告要并入的那条说明（取后即空，保证只报一次）。
pub fn take_startup_notice() -> Option<crate::startup::CheckResult> {
    STARTUP_NOTICE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
}

/// 启动早期调用：配置里留着 `[paths] data_migrate_from` 就把旧数据根的 `data/`+`backup/`
/// 搬进新数据根，逐文件核对无误后删掉源侧，再清掉标记。没有标记时零成本返回。
///
/// 为什么搬运动作在**新进程**而不是点「更改数据文件夹」的那个进程：本进程从解析出数据
/// 目录那一刻起就一直按它写（写线程、备份/采样/统计线程都是常驻的），当场把目录搬走
/// 之后到进程真退出之间还会往旧目录落增量 —— 新目录里没有那一份，表现就是"搬完还差一截"，
/// 也正是他说的"两份数据差得越来越远"。让旧进程把自己那套写完整、备份完整再死，
/// 新进程搬走的就是完整结果。
///
/// 每一步都可重放：复制/校验没过 → 源侧一个字不动、标记留着（下次启动重试），并把本进程
/// 的数据根**钉回旧目录**继续记录（失败不能让按键白打）；复制成功但删源失败 → 数据已在
/// 新目录、标记已清，只留一条"旧目录请手工清理"的说明。
pub fn run_pending_migration() -> Option<MigrationReport> {
    let cfg = crate::config::instance();
    let from_raw = cfg.get("paths", "data_migrate_from").trim().to_string();
    if from_raw.is_empty() {
        return None;
    }
    let from = PathBuf::from(&from_raw);
    let to = paths::data_home();

    if from == to {
        tracing::warn!(
            "data_migrate_from 与 data_home 是同一个目录（{}），没有可搬的东西，清掉标记",
            from.display()
        );
        clear_marker(cfg);
        return None;
    }

    match migrate_data_tree(&from, &to) {
        Ok(report) => {
            clear_marker(cfg);
            tracing::info!(
                "数据目录迁移完成：{} → {}（{} 个文件 / {} 字节，旧目录{}）",
                report.from.display(),
                report.to.display(),
                report.summary.copied,
                report.summary.bytes,
                if report.leftover_in_source > 0 {
                    format!("还剩下 {} 项删不掉，可手工清理", report.leftover_in_source)
                } else {
                    "已清空".to_string()
                }
            );
            Some(report)
        }
        Err(problem) => {
            give_up(&from, &to, problem);
            None
        }
    }
}

/// 搬家本身：复制 → 逐文件核对 → 删源。任何一步没过就返回 Err，**源侧一个字不动**
/// （目标里可能已经躺着一半，这没关系：标记留着，下次启动重放，同大小的文件会被跳过）。
///
/// 刻意与 [`run_pending_migration`] 分开：那边要读/清 config 的进程级单例，而它一个
/// 进程只能初始化一次、路径启动即钉死，单测里换不了目录；不依赖配置的部分拆出来才按得住。
/// 成功后由调用方清标记。
pub fn migrate_data_tree(from: &Path, to: &Path) -> Result<MigrationReport, String> {
    // 源侧根目录必须读得出来。旧写法让"旧根里既没有 data/ 也没有 backup/"一路
    // continue 过去：copy_data_tree 零错误、verify_copy 零核对项、remove_source_trees
    // 零可删 → 返回 Ok(copied: 0) → 调用方清掉 data_migrate_from 标记。可"旧数据根
    // 此刻读不出来"（U 盘/网络盘还没挂上、`data_home` 回退成了程序目录、config.ini
    // 里那行是相对路径而 CWD 变了）与"旧根确实没有历史"在旧实现里长得一模一样，
    // 前者被清掉标记就等于从"这次没搬成"升级成"永远不再搬"：全部历史留在旧目录，
    // 新目录从今天起开始攒第二份，两边越差越远。
    if !from.is_dir() {
        return Err(format!(
            "旧数据目录读不出来（{}）：不存在、不是目录，或那个盘还没挂上；标记保留，下次启动重试",
            from.display()
        ));
    }
    // 读得出来、但两棵子树都没有：这确实是"没有可搬的历史"，让它成功（否则刚装完
    // 就改目录的用户会被这个标记钉在旧目录上，永远搬不完），只留一条 warn 备查。
    if !from.join("data").is_dir() && !from.join("backup").is_dir() {
        tracing::warn!(
            "旧数据目录 {} 里没有 data/ 也没有 backup/，本次按「没有可搬的历史」完成",
            from.display()
        );
    }
    // 同一个目录的两种写法必须在这里再拦一次。`run_pending_migration` 那道
    // `from == to` 是 `PathBuf` 的字节比较，而 Windows 上 `C:\data`、`C:\data\`、
    // `c:/DATA` 指的是同一个目录：闸过了以后下面的 `copy_data_tree` 是自己抄自己、
    // `verify_copy` 天然全对，最后 `remove_source_trees(from)` 删掉的正是**活目录**。
    if same_directory(from, to) {
        tracing::warn!(
            "新旧数据目录其实是同一个（{}），没有可搬的东西；源侧一个文件都不动",
            from.display()
        );
        return Ok(MigrationReport {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
            summary: CopySummary::default(),
            leftover_in_source: 0,
        });
    }
    let summary = copy_data_tree(from, to);
    if summary.has_errors() {
        return Err(join_errors(&summary));
    }
    // 只数 copied 不够：`backup` 在旧目录本来就不存在时 copied 天然是 0，
    // 而这里要的是"源侧每一个文件都在新目录里有一份同大小的对应物"。
    verify_copy(from, to)?;
    let leftover_in_source = remove_source_trees(from);
    Ok(MigrationReport {
        from: from.to_path_buf(),
        to: to.to_path_buf(),
        summary,
        leftover_in_source,
    })
}

/// 搬不成：把本次运行的数据根钉回旧目录，并给启动报告一条失败说明。
///
/// 这里刻意**不碰配置**：`data_migrate_from` 保持原样就是"下次启动重试"的全部内容，
/// 而 `data_home` 也已经写好了 —— 失败只是本次退回旧目录继续记录，不代表放弃新目录。
fn give_up(from: &Path, to: &Path, problem: String) {
    tracing::error!(
        "数据目录迁移失败（{} → {}）：{problem}；本次继续用旧目录，标记已保留，下次启动会重试",
        from.display(),
        to.display()
    );
    // 先钉住再报：数据必须有个能写的地方，而新目录此刻可能是半空的 —— 按它记录等于
    // 把新键鼠写进一个不完整的历史里。
    paths::force_data_home(from);
    *STARTUP_NOTICE.lock().unwrap_or_else(|e| e.into_inner()) =
        Some(crate::startup::CheckResult::fail(
            "数据目录迁移",
            format!(
                "没能搬到 {}（{}）；本次仍记在旧目录 {}，标记留着下次重试。原因：{problem}",
                to.display(),
                from.display(),
                from.display()
            ),
        ));
}

/// 清掉待搬运标记并立刻落盘（去抖那一路来不及：进程可能马上就退了）。
fn clear_marker(cfg: &crate::config::FocusFlowConfig) {
    if let Err(e) = cfg.set("paths", "data_migrate_from", "") {
        tracing::error!("清除 data_migrate_from 失败: {e}");
        return;
    }
    if let Err(e) = cfg.save() {
        tracing::error!("清除迁移标记后落盘失败: {e}");
    }
}

fn join_errors(summary: &CopySummary) -> String {
    summary.errors.join("；")
}

/// 源侧每一个文件都得在新目录里有同大小的一份，否则不许删源。
fn verify_copy(src_root: &Path, dst_root: &Path) -> Result<(), String> {
    for sub in ["data", "backup"] {
        let base = src_root.join(sub);
        if !base.is_dir() {
            continue;
        }
        let mut stack = vec![base.clone()];
        while let Some(dir) = stack.pop() {
            let entries = std::fs::read_dir(&dir)
                .map_err(|e| format!("{} 读取失败，无法核对: {e}", dir.display()))?;
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else { continue };
                let from = entry.path();
                if ft.is_dir() {
                    stack.push(from);
                    continue;
                }
                if !ft.is_file() {
                    continue;
                }
                // 相对路径必须相对**这棵子树的根**（`src_root/data`）来算，再挂到
                // `dst_root/data` 上。相对 `src_root` 算出来的相对段本身已经含 `data/`，
                // 拼下去就是 `新目录/data/data/xxx.db` —— 核对永远找不到文件，
                // 于是永远不敢删源、永远退回旧目录、下次启动原地重来（上一版就是这么
                // 被单测当场抓住的）。
                let rel = from.strip_prefix(&base).unwrap_or(Path::new(""));
                let to = dst_root.join(sub).join(rel);
                let want = std::fs::metadata(&from)
                    .map(|m| m.len())
                    .unwrap_or(u64::MAX);
                match std::fs::metadata(&to) {
                    Ok(m) if m.len() == want => {
                        // 核对必须是内容级的：这道闸是"能不能删旧目录"的唯一依据，
                        // 而复制那一步对同大小的文件是允许跳过的 —— 两边都用大小
                        // 当判据，等于同一条弱判据把两道关，跳过的错误就没人兜底了。
                        if !contents_equal(&from, &to) {
                            return Err(format!(
                                "{} 在新目录里大小相同但内容不同（{}），不敢删源",
                                from.display(),
                                to.display()
                            ));
                        }
                    }
                    Ok(m) => {
                        return Err(format!(
                            "{} 在新目录里大小对不上（{} vs {}），不敢删源",
                            from.display(),
                            m.len(),
                            want
                        ))
                    }
                    Err(e) => {
                        return Err(format!(
                            "{} 在新目录里不存在（{}）: {e}，不敢删源",
                            from.display(),
                            to.display()
                        ))
                    }
                }
            }
        }
    }
    Ok(())
}

/// 删掉旧数据根的 `data/`、`backup/` 两棵子树，返回没删掉的顶层条目数。
///
/// 只删这两棵，绝不整个删掉 `from`：默认情形下 `from` 就是程序目录，那里还放着
/// config.ini、日志与插件，它们跟着程序走、不归这次搬运管。
fn remove_source_trees(from: &Path) -> usize {
    let mut leftover = 0;
    for sub in ["data", "backup"] {
        let dir = from.join(sub);
        if !dir.exists() {
            continue;
        }
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            tracing::warn!("旧目录 {} 没能删除: {e}", dir.display());
            leftover += std::fs::read_dir(&dir).map(|it| it.count()).unwrap_or(1);
        }
    }
    leftover
}

/// 两个路径是否指向同一个文件（照 `migration::same_file` 的口径：认不出来就当不是）。
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// 分块比对用的缓冲区大小。刻意小：这条链跑在主线程启动路径上，
/// 两个栈上缓冲加起来只有 64KB。
const CONTENT_CHUNK: usize = 32 * 1024;

/// 读到缓冲满或 EOF，返回实际读到的字节数（EOF 时可能少于一整块）。
fn fill(r: &mut impl std::io::Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        match r.read(&mut buf[done..]) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(done)
}

/// 两个文件内容是否逐字节相同；任一读不开都当"不同"（宁可多搬一次，不许误判成相同）。
///
/// 这是"跳过"与"删源"两处判据的最后一道闸，所以比对而不是哈希：哈希要在两份文件
/// 都读完之后才能比，而内容在第 1 个块就不一样的常见情形下，比对可以立刻收工。
fn contents_equal(a: &Path, b: &Path) -> bool {
    let Ok(mut fa) = std::fs::File::open(a) else {
        return false;
    };
    let Ok(mut fb) = std::fs::File::open(b) else {
        return false;
    };
    let mut ba = [0u8; CONTENT_CHUNK];
    let mut bb = [0u8; CONTENT_CHUNK];
    loop {
        let ra = fill(&mut fa, &mut ba);
        let rb = fill(&mut fb, &mut bb);
        match (ra, rb) {
            (Ok(0), Ok(0)) => return true,
            (Ok(x), Ok(y)) if x == y && ba[..x] == bb[..y] => {}
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths;

    fn write_file(p: &Path, content: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    /// 写一个 `[paths] data_home` 并让 paths 重新解析一次。
    ///
    /// 生产上没有"重新解析"这一步（换目录靠重启，见模块注释），唯一的入口就是
    /// `set_app_dir` 顺带作废缓存 —— 测试借它触发，正好也把"切 app_dir 必然连带
    /// 重解析数据目录"这条不变量按住。
    fn configure_data_home(app: &Path, value: &str) {
        std::fs::write(
            app.join("config.ini"),
            format!("[database]\nflush_interval = 10\n\n[paths]\ndata_home = {value}\n"),
        )
        .unwrap();
        paths::set_app_dir(app);
    }

    #[test]
    fn data_home_defaults_to_app_dir_and_program_files_stay_put() {
        let _lock = paths::test_app_dir_lock();
        let app = paths::test_app_dir("dh_default");
        // 没有 config.ini、或有文件而没这个键时，数据目录 = 程序目录。
        // 便携包"拷走整个文件夹即迁移"和整套靠 set_app_dir 做的测试隔离都靠这一点。
        assert_eq!(paths::data_home(), app.path().to_path_buf());
        configure_data_home(app.path(), "");
        assert_eq!(paths::data_home(), app.path().to_path_buf());

        let target = paths::test_app_dir("dh_target");
        paths::set_app_dir(app.path());
        configure_data_home(app.path(), &target.path().to_string_lossy());

        assert_eq!(paths::data_home(), target.path().to_path_buf());
        assert!(paths::data_dir().starts_with(target.path()));
        assert!(paths::backup_dir().starts_with(target.path()));
        // 分工的另一半：程序本体相关文件一律不跟着搬走
        assert_eq!(paths::config_path(), app.path().join("config.ini"));
        assert_eq!(
            paths::window_state_path(),
            app.path().join("window_state.ini")
        );
        assert!(paths::log_dir().starts_with(app.path()));
        assert!(paths::plugins_dir().starts_with(app.path()));
    }

    #[test]
    fn relative_data_home_is_expanded_against_app_dir() {
        let _lock = paths::test_app_dir_lock();
        let app = paths::test_app_dir("dh_rel");
        configure_data_home(app.path(), "MyData");
        let expect = app.path().join("MyData");
        assert_eq!(paths::data_home(), expect);
        // 相对形式也该有个明确基准，且用的时候才建目录（不是填了个不存在的目录就坏）
        assert!(expect.is_dir());
        assert!(paths::data_dir().starts_with(&expect));
    }

    #[test]
    fn unusable_data_home_falls_back_to_app_dir() {
        let _lock = paths::test_app_dir_lock();
        let app = paths::test_app_dir("dh_fallback");
        // 指到一个已存在的**普通文件**上：create_dir_all 必然失败（移动盘没插、
        // OneDrive 占位、权限不足都是这一类），此时必须回落而不是硬失败。
        let blocker = app.path().join("not_a_dir");
        write_file(&blocker, "x");
        configure_data_home(app.path(), &blocker.to_string_lossy());
        assert_eq!(paths::data_home(), app.path().to_path_buf());
        // 回落后仍能正常写数据
        assert!(paths::data_dir().starts_with(app.path()));
        assert!(paths::data_dir().is_dir());
    }

    #[test]
    fn copy_data_tree_moves_data_and_backup_but_not_program_files() {
        let _lock = paths::test_app_dir_lock();
        let src = paths::test_app_dir("cp_src");
        let dst = paths::test_app_dir("cp_dst");
        paths::set_app_dir(src.path());

        write_file(&src.path().join("data/focusflow_2026.db"), "year-db-bytes");
        write_file(&src.path().join("data/reports/week.md"), "report");
        write_file(&src.path().join("backup/focusflow_2026.db"), "backup-bytes");
        // 留在程序目录的两样：日志与插件，不该被这次复制带过去
        write_file(&src.path().join("logs/2026-09-27"), "log");
        write_file(&src.path().join("plugins/a.lua"), "lua");

        let s = copy_data_tree(src.path(), dst.path());
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        assert_eq!(s.copied, 3);
        assert!(s.bytes > 0);
        assert_eq!(
            std::fs::read_to_string(dst.path().join("data/reports/week.md")).unwrap(),
            "report"
        );
        assert_eq!(
            std::fs::read_to_string(dst.path().join("backup/focusflow_2026.db")).unwrap(),
            "backup-bytes"
        );
        assert!(!dst.path().join("logs/2026-09-27").exists());
        assert!(!dst.path().join("plugins/a.lua").exists());
        // "拷贝而非移动"的命门：源目录一字不动，它是这次操作的备份
        assert!(src.path().join("data/focusflow_2026.db").exists());
        assert!(src.path().join("backup/focusflow_2026.db").exists());
    }

    #[test]
    fn copy_data_tree_refuses_same_or_nested_destination() {
        let _lock = paths::test_app_dir_lock();
        let src = paths::test_app_dir("gd_src");
        let db = src.path().join("data/focusflow_2026.db");
        write_file(&db, "x");

        // 挑到自己当前的数据根目录上
        let s = copy_data_tree(src.path(), src.path());
        assert_eq!(s.copied, 0);
        assert_eq!(s.errors.len(), 1, "一次拒绝只该有一条话: {:?}", s.errors);
        // 顺着路径点进当前 data/ 里面：照做会复制出一份套一层的自嵌套树
        let nested = src.path().join("data").join("sub");
        let s2 = copy_data_tree(src.path(), &nested);
        assert_eq!(s2.copied, 0);
        assert_eq!(
            s2.errors.len(),
            1,
            "`data` 与 `backup` 各报一遍同样的句子，读起来像出了两次故障"
        );
        // 同一件事换成带 `..` 的写法也必须拦住：这条路径上的目录都还不存在，
        // 靠的是 stable_abs 的词法折叠（canonicalize 那种情况根本用不上）。
        let via_dotdot = src.path().join("data").join("x").join("..").join("y");
        let s3 = copy_data_tree(src.path(), &via_dotdot);
        assert_eq!(s3.copied, 0);
        assert_eq!(s3.errors.len(), 1);
        // 拒绝必须发生在动手之前：源文件在原地，一个新目录也没建出来
        assert!(db.exists());
        assert!(!nested.join("data").exists());
        assert!(!src.path().join("data/y").exists());
    }

    /// 用户实测的那个手势：`data_home` 没设时程序目录**就是**数据根目录，而他选的
    /// `程序目录\123` 是一个正当的新数据根 —— 既不是同一个目录，也不在 `data/`、
    /// `backup/` 任何一棵子树里面。上一版的兜底判定的条件是"落在 src_root 里"，
    /// 于是把它当套娃拒了；前端紧接着又用 `renderSettings()` 把那句错误抹掉，
    /// 用户看到的就是"点了没反应、数据还在原地"。两个环节一起按住。
    #[test]
    fn sibling_folder_inside_program_dir_is_a_valid_target() {
        let _lock = paths::test_app_dir_lock();
        let app = paths::test_app_dir("sib_app");
        write_file(&app.path().join("data/focusflow_2026.db"), "year-db");
        write_file(&app.path().join("backup/focusflow_2026.db"), "bk");
        // 前提就是没配 data_home：数据根 == 程序目录，此时兄弟目录的判定才最容易误伤
        assert_eq!(paths::data_home(), app.path().to_path_buf());

        let dst = app.path().join("123");
        let s = copy_data_tree(&paths::data_home(), &dst);
        assert!(
            !s.has_errors(),
            "程序目录里的兄弟目录必须能当选: {:?}",
            s.errors
        );
        assert_eq!(s.copied, 2);
        assert_eq!(
            std::fs::read_to_string(dst.join("data/focusflow_2026.db")).unwrap(),
            "year-db"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("backup/focusflow_2026.db")).unwrap(),
            "bk"
        );
        // copy_data_tree 本身只复制；删源是 migrate_data_tree 的事，两层的职责不同
        assert!(app.path().join("data/focusflow_2026.db").exists());
    }

    /// 回归（账 31）：`run_pending_migration` 那道 `from == to` 是 `PathBuf` 的字节比较，
    /// 而 Windows 上 `C:\data` 与 `C:\data\` 是同一个目录的两种写法 ⇒ 过了闸以后
    /// `migrate_data_tree` 一路走到 `remove_source_trees(from)`，删掉的正是**活目录**。
    #[test]
    fn same_directory_in_two_spellings_deletes_nothing() {
        let _lock = paths::test_app_dir_lock();
        let dir = paths::test_app_dir("mv_same");
        write_file(&dir.path().join("data/focusflow_2026.db"), "history");
        write_file(&dir.path().join("backup/keep.db"), "bk");
        let spelled = format!(
            "{}{}",
            dir.path().to_string_lossy(),
            std::path::MAIN_SEPARATOR
        );
        let rep = migrate_data_tree(dir.path(), std::path::Path::new(&spelled))
            .expect("同一个目录应按「没有可搬的东西」成功");
        assert_eq!(rep.summary.copied, 0, "自己搬自己不该复制任何文件");
        assert!(
            dir.path().join("data/focusflow_2026.db").is_file(),
            "源侧就是目标侧，主库一个字都不能少"
        );
        assert!(dir.path().join("backup/keep.db").is_file(), "backup 侧同理");
    }

    #[test]
    fn migrate_data_tree_moves_both_trees_and_empties_source() {
        let _lock = paths::test_app_dir_lock();
        let from = paths::test_app_dir("mv_from");
        let to = paths::test_app_dir("mv_to");
        paths::set_app_dir(from.path());
        write_file(&from.path().join("data/focusflow_2026.db"), "year-db-bytes");
        write_file(&from.path().join("data/reports/week.md"), "report");
        write_file(&from.path().join("backup/focusflow_2026.db"), "bk");
        // 程序本体不该跟着搬走的两样
        write_file(&from.path().join("logs/2026-09-27"), "log");
        write_file(&from.path().join("config.ini"), "[paths]\ndata_home = x\n");

        let rep = migrate_data_tree(from.path(), to.path()).expect("该搬成功");
        assert_eq!(rep.summary.copied, 3);
        assert_eq!(rep.leftover_in_source, 0);
        assert_eq!(
            std::fs::read_to_string(to.path().join("data/reports/week.md")).unwrap(),
            "report"
        );
        // 源侧两棵子树都没了 —— 这就是"转移"而不是"复制"的地方
        assert!(!from.path().join("data").exists());
        assert!(!from.path().join("backup").exists());
        // 但日志、配置、插件一样不少：搬运只管 data 与 backup
        assert!(from.path().join("logs/2026-09-27").is_file());
        assert!(from.path().join("config.ini").is_file());
    }

    #[test]
    fn migrate_data_tree_keeps_source_when_copy_cannot_run() {
        let _lock = paths::test_app_dir_lock();
        let from = paths::test_app_dir("mf_from");
        let to = paths::test_app_dir("mf_to");
        paths::set_app_dir(from.path());
        write_file(&from.path().join("data/focusflow_2026.db"), "year-db-bytes");
        // 目标里的 `data` 是个**普通文件**：建目录必失败 → 复制出错 → 绝不允许删源
        let dst = to.path().join("dst");
        write_file(&dst.join("data"), "not-a-dir");

        let e = migrate_data_tree(from.path(), &dst).expect_err("该失败");
        assert!(
            e.contains("创建失败") || e.contains("复制失败"),
            "错误文案该说清为什么没搬成: {e}"
        );
        assert!(
            from.path().join("data/focusflow_2026.db").exists(),
            "没核对通过时源必须原样留着"
        );
    }

    #[test]
    fn migrate_data_tree_refuses_a_source_root_that_cannot_be_read() {
        let _lock = paths::test_app_dir_lock();
        let from = paths::test_app_dir("mr_from");
        let to = paths::test_app_dir("mr_to");
        paths::set_app_dir(from.path());

        // 旧根读不出来（U 盘/网络盘没挂上、data_home 回退、相对路径撞上别的 CWD 都是
        // 这一类）：旧实现里 copy_data_tree 零错误、verify_copy 零核对项、删源零可删，
        // 于是返回 Ok(copied: 0) → 调用方把 data_migrate_from 标记清掉 ⇒
        // "这次没搬成"升级成"永远不再搬"，历史留在旧目录、新目录开始攒第二份。
        let not_mounted = from.path().join("not-mounted");
        let e = migrate_data_tree(&not_mounted, to.path()).expect_err("源根读不出来时必须失败");
        assert!(
            e.contains("读不出来"),
            "文案要说清是「没搬成」而不是「搬完了」: {e}"
        );
        // 目标侧必须是干净的：`test_app_dir` 自己会建出空的 `data/`，所以判"有没有
        // 落下半截东西"要看条目数，不能判目录在不在。
        let landed = std::fs::read_dir(to.path().join("data"))
            .map(|it| it.flatten().count())
            .unwrap_or(0);
        assert_eq!(landed, 0, "失败不该在新目录里留下半截东西");

        // 反向腿：旧根读得出来、里面确实没有历史（刚装完就改数据目录）必须成功，
        // 否则这个用户会被标记钉在旧目录上永远搬不完
        let fresh = from.path().join("fresh");
        std::fs::create_dir_all(&fresh).unwrap();
        let rep = migrate_data_tree(&fresh, to.path()).expect("空的旧根不该卡住迁移");
        assert_eq!(rep.summary.copied, 0);
    }

    #[test]
    fn keeping_aside_a_conflicting_target_takes_its_wal_sidecars_too() {
        let _lock = paths::test_app_dir_lock();
        let src = paths::test_app_dir("ksw_src");
        let dst = paths::test_app_dir("ksw_dst");
        paths::set_app_dir(src.path());
        write_file(
            &src.path().join("data/focusflow_2026.db"),
            "fresh-from-source",
        );
        // 目标目录里那份是**别人**的一套库，还带着自己没 checkpoint 完的 WAL
        write_file(
            &dst.path().join("data/focusflow_2026.db"),
            "older-other-data",
        );
        write_file(
            &dst.path().join("data/focusflow_2026.db-wal"),
            "someone-elses-wal",
        );
        write_file(
            &dst.path().join("data/focusflow_2026.db-shm"),
            "someone-elses-shm",
        );

        let s = copy_data_tree(src.path(), dst.path());
        assert!(s.errors.is_empty(), "{:?}", s.errors);

        let dir = dst.path().join("data");
        let archived = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".pre-switch-"))
            .count();
        assert_eq!(
            archived,
            3,
            "主库与它的 -wal/-shm 必须成套留档，不该只搬主库: {:?}",
            std::fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect::<Vec<_>>()
        );
        // 原位一份都不该剩：新主库 + 别人的旧 wal = SQLite 照错配的 wal 恢复
        assert!(
            !dir.join("focusflow_2026.db-wal").exists(),
            "孤儿 -wal 不该留在原位"
        );
        assert!(
            !dir.join("focusflow_2026.db-shm").exists(),
            "孤儿 -shm 不该留在原位"
        );
    }

    #[test]
    fn validate_new_data_home_rejects_bad_choices_up_front() {
        let _lock = paths::test_app_dir_lock();
        let app = paths::test_app_dir("va_app");
        write_file(&app.path().join("data/focusflow_2026.db"), "x");

        // 就是当前数据根
        assert!(validate_new_data_home(&paths::data_home()).is_err());
        // 选进了正在被搬的子树里
        assert!(validate_new_data_home(&app.path().join("data/sub")).is_err());
        // 目标建不出来（路径中间是个普通文件）
        let blocker = app.path().join("not_a_dir");
        write_file(&blocker, "x");
        assert!(validate_new_data_home(&blocker.join("inner")).is_err());
        // 目标里已经有无关内容
        let messy = app.path().join("messy");
        write_file(&messy.join("notes.txt"), "hi");
        assert!(validate_new_data_home(&messy).is_err());

        // 干净的兄弟目录 = 用户实测的那个手势，必须放行
        assert!(validate_new_data_home(&app.path().join("123")).is_ok());
        // `data`/`backup` 是搬运自己的产物，不该被算作"无关内容"（重放第二次也要能过）
        let next = app.path().join("next");
        write_file(&next.join("data/keep.db"), "k");
        assert!(validate_new_data_home(&next).is_ok());
    }

    #[test]
    fn copy_keeps_aside_conflicting_target_then_skips_same_size() {
        let _lock = paths::test_app_dir_lock();
        let src = paths::test_app_dir("ks_src");
        let dst = paths::test_app_dir("ks_dst");
        paths::set_app_dir(src.path());
        write_file(
            &src.path().join("data/focusflow_2026.db"),
            "fresh-from-source",
        );
        // 目标目录里已经有一份别的数据库（选了个装过旧版 FocusFlow 的目录）
        write_file(
            &dst.path().join("data/focusflow_2026.db"),
            "older-other-data",
        );

        let s = copy_data_tree(src.path(), dst.path());
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        assert_eq!(s.copied, 1);
        assert_eq!(
            s.backed_up.len(),
            1,
            "目标原有那份必须留档: {:?}",
            s.backed_up
        );
        assert_eq!(
            std::fs::read_to_string(dst.path().join("data/focusflow_2026.db")).unwrap(),
            "fresh-from-source"
        );
        let kept = Path::new(&s.backed_up[0]);
        assert_eq!(std::fs::read_to_string(kept).unwrap(), "older-other-data");

        // 再点一次"更改"：同大小的文件不该再搬一遍（否则每次都整份重写一年库），
        // 也不该因此又留一份档
        let again = copy_data_tree(src.path(), dst.path());
        assert_eq!(again.copied, 0);
        assert!(again.backed_up.is_empty());
        assert!(again
            .skipped_existing
            .iter()
            .any(|p| p.contains("focusflow_2026.db")));
    }

    /// 目标上那份"大小刚好相同、内容不同"的库，绝不能被当成上一轮已经搬好的自己人。
    ///
    /// 年度库按页分配，两份内容不同的库撞出同一个字节数是常事（没写过几行的库更是
    /// 天生同尺寸）。上一版只比大小：这种巧合会被跳过，而删源前的核对同样只比大小，
    /// 于是两道关用同一条弱判据互相背书 —— 结果是旧目录被删、新目录里留着别人的那份。
    #[test]
    fn same_size_different_content_is_not_mistaken_for_an_already_copied_file() {
        let _lock = paths::test_app_dir_lock();
        let src = paths::test_app_dir("sc_src");
        let dst = paths::test_app_dir("sc_dst");
        paths::set_app_dir(src.path());
        write_file(&src.path().join("data/focusflow_2026.db"), "AAAA");
        write_file(&dst.path().join("data/focusflow_2026.db"), "BBBB");

        let s = copy_data_tree(src.path(), dst.path());
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        assert_eq!(s.copied, 1, "同大小不同内容就该搬过去，不是跳过");
        assert_eq!(s.backed_up.len(), 1, "目标原有那份是唯一副本，必须留档");
        assert_eq!(
            std::fs::read_to_string(dst.path().join("data/focusflow_2026.db")).unwrap(),
            "AAAA"
        );
        assert_eq!(
            std::fs::read_to_string(Path::new(&s.backed_up[0])).unwrap(),
            "BBBB"
        );

        // 放行到删源这一步：新目录里躺着的确实是自己那份，删旧目录才安全
        let rep = migrate_data_tree(src.path(), dst.path()).expect("该搬成功");
        assert_eq!(rep.leftover_in_source, 0);
        assert!(!src.path().join("data").exists());
        assert_eq!(
            std::fs::read_to_string(dst.path().join("data/focusflow_2026.db")).unwrap(),
            "AAAA"
        );
    }

    /// 删源前的最后一道闸：核对要到内容级，不是"看着差不多大"。
    #[test]
    fn verify_copy_demands_identical_content_not_just_equal_size() {
        let _lock = paths::test_app_dir_lock();
        let from = paths::test_app_dir("vc_from");
        let to = paths::test_app_dir("vc_to");
        paths::set_app_dir(from.path());
        write_file(&from.path().join("data/focusflow_2026.db"), "1234");
        write_file(&to.path().join("data/focusflow_2026.db"), "abcd");
        let e = verify_copy(from.path(), to.path()).expect_err("同大小不同内容不许放行");
        assert!(e.contains("内容"), "文案该说清为什么不敢删源: {e}");

        // 大小不同同样不许（既有行为的回归位）
        write_file(&to.path().join("data/focusflow_2026.db"), "abc");
        assert!(verify_copy(from.path(), to.path()).is_err());

        // 逐字节相同才放行
        write_file(&to.path().join("data/focusflow_2026.db"), "1234");
        assert!(verify_copy(from.path(), to.path()).is_ok());
    }
}
