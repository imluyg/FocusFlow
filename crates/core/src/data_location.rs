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
/// 校验一次"更改数据目录"的目标是否可用。`src` 是**当前**数据根，由调用方交进来。
///
/// `src` 以前是这里自己现读 `paths::data_home()`：那是第二个读点。当年的调用方（旧桌面端
/// 那条 `change_data_dir`）前一行刚用 `current_data_home()` 算出要搬走的
/// 源目录，转天校验时读的却是另一个时刻解析出来的值 —— 两次解析中间只要有人
/// `force_data_home`/换过 app_dir，校验的就是一个根本不会被搬运的目录。现在两边
/// 用的是同一个串，一把尺子。
///
/// 这一步必须存在，是因为"选了不合法的目录"原本要等到重启后新进程搬运时才暴露，
/// 而那时界面早没了 —— 用户看到的就只是"点了没反应、数据还在原地"（实测就是这样：
/// 他选了 `程序目录\123`，被判定误杀，错误只躺在日志里）。提前判一次，
/// 报错就能当场显示在设置页上。
pub fn validate_new_data_home(src: &Path, dst: &Path) -> Result<(), String> {
    guard_destination(src, dst)?;
    std::fs::create_dir_all(dst)
        .map_err(|e| format!("目标目录建不出来（{}）: {e}", dst.display()))?;
    // 目标里除了 data / backup 之外还有别的东西就拒绝：往装着旧版 FocusFlow 的
    // 目录里搬是正常的，但那里已经有无关文件时，搬运结果和用户的预期不一致，
    // 而且事后很难分清哪份是谁的。
    // 单个条目读不动也一并算"看不清这个目录"：那正是会漏掉一个陌生名字的地方。
    let listing = std::fs::read_dir(dst).and_then(|it| {
        it.map(|entry| entry.map(|e| e.file_name().to_string_lossy().to_string()))
            .collect::<Result<Vec<String>, std::io::Error>>()
    });
    strangers_verdict(listing, dst)
}

/// 「目标目录里有没有无关内容」这条校验的结论。抽成纯函数是为了钉住**读不出来**那一支：
///
/// 原先写的是 `if let Ok(it) = std::fs::read_dir(dst)` —— 读失败时段落直接落空，
/// 名单是空的 ⇒ 校验"通过"。可这条校验存在的唯一理由就是"搬进去之前先看清那里有什么"，
/// 看不清的时候放行，等于在最该停手的形态下把一整年数据搬进一个读不动的目录，
/// 而界面只会显示"已经安排好了"。刚 `create_dir_all` 成功、下一步就列不出来，
/// 是受限 ACL、被杀软握着、或那个位置被同步盘换成了别的东西的真实形态 ⇒ 保守做法是停下来问。
fn strangers_verdict(
    listing: Result<Vec<String>, std::io::Error>,
    dst: &Path,
) -> Result<(), String> {
    let names = match listing {
        Err(e) => {
            return Err(format!(
                "目标目录（{}）建好了却读不出里面有什么: {e} —— 请换一个能正常打开的文件夹再试",
                dst.display()
            ))
        }
        Ok(names) => names,
    };
    let mut strangers: Vec<String> = names
        .into_iter()
        .filter(|name| name != "data" && name != "backup")
        .collect();
    if strangers.is_empty() {
        return Ok(());
    }
    strangers.sort();
    strangers.truncate(6);
    Err(format!(
        "目标目录里已经有别的内容（{}），请换一个空的或新建的文件夹",
        strangers.join(", ")
    ))
}

/// 把 `src_root` 下的 `data/` 与 `backup/` 复制到 `dst_root` 的对应子目录。
///
/// 目标已存在的同名文件：同大小就跳过（重复点一次"更改"不该把刚复制好的库再搬
/// 一遍，白白搬一年数据）；大小不同说明目标那份是**别的东西**（用户选了个装着
/// 旧版 FocusFlow 的目录是很常见的），先把它改名留档再写，绝不静默覆盖 ——
/// 源目录本来就一字不动地留着，但目标里原有的那份没有第二份副本。
/// `data/` 与 `backup/` 这两棵「在不在、是不是目录」的三态。
///
/// 立它的原因：两处过去都写 `!dir.is_dir() ⇒ continue`，而 `Path::is_dir()` 对**任何**一次
/// 失败都回 `false` —— 包括那个自指/成环的目录 junction（本机现量 os error 1921）、
/// 移动盘休眠、同步盘正握着。于是"这一棵问不出"与"旧目录本来没有这一棵"在两条链上
/// 都长成同一个"跳过"：复制那一步交回 `copied: 0` 且零错误，核对那一步压根没往那棵里看，
/// `remove_source_trees` 就把用户的全部历史删掉，回执念的却是「迁移完成」。
/// 只有 `NotFound` 才算确认没有 —— 与 `is_confirmed_absent`、`queries::year_db_presence`
/// 同一把尺子。
enum Subtree {
    /// 是目录，往里走
    Dir,
    /// 确认这一棵没有（或那儿是个普通文件，本来不该当子树搬）
    Absent,
    /// 这一刻问不出，原因带回去
    Unknown(String),
}

fn subtree_of(dir: &Path) -> Subtree {
    match std::fs::metadata(dir) {
        Ok(m) if m.is_dir() => Subtree::Dir,
        Ok(_) => Subtree::Absent,
        Err(e) if is_confirmed_absent(&e) => Subtree::Absent,
        Err(e) => Subtree::Unknown(format!("{} 问不出是不是目录: {e}", dir.display())),
    }
}

pub fn copy_data_tree(src_root: &Path, dst_root: &Path) -> CopySummary {
    let mut summary = CopySummary::default();

    // 先挡住"复制会出事"的指向，再动手：
    if let Err(e) = guard_destination(src_root, dst_root) {
        summary.errors.push(e);
        return summary;
    }

    for sub in ["data", "backup"] {
        let src = src_root.join(sub);
        match subtree_of(&src) {
            Subtree::Absent => continue,
            Subtree::Unknown(why) => {
                // 说得出"这一棵没搬"，调用方那句回执就不会把它念成搬完了
                summary
                    .errors
                    .push(format!("{why}；这一棵（{sub}）一个文件都没复制"));
                continue;
            }
            Subtree::Dir => {}
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
            for entry in entries {
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        // 枚举到一半读不出来（网络盘/USB 断了会话、同步盘正握着目录）：
                        // 这一层**后面的条目一个都没被复制**。旧写法是 `entries.flatten()`，
                        // 把这条 Err 与"这里没有更多文件"折成同一个结局，而核对那一步同样
                        // 是 flatten ⇒ 两边都没看见这批文件，`verify_copy` 照样交回
                        // "每个源文件都有 counterpart"，删源那道闸就这么开着过去了。
                        // 记进 errors 之后调用方既不清标记也不删源，下次启动原地重来。
                        summary.errors.push(format!(
                            "{} 枚举到一半读不出来，这一层后面的条目没被复制: {e}",
                            s.display()
                        ));
                        continue;
                    }
                };
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
    use crate::db::connection::Presence;
    // 目标那一格要答三件事，`to.exists()` 只答得了两件：确认没有、读得动，以及**这一刻问不出**
    // （那份正被别的程序握着、杀软首扫、移动盘刚休眠）。第三件被读成「没有」时，下面整段保护
    // （同文件判据、内容比对、`keep_aside` 留档）一起被跳过，直奔最后那句 `fs::copy`
    // **截断覆盖**目标上那份唯一副本 —— 而换完目录随后还要删源，好数据两头都没了。
    // `keep_aside` 自己认得"读不出就不覆盖"（它上面的注释写的就是这条），却被这一问绕了过去：
    // 尺子要用在同一层上，而不是只在里面那一层。
    let target = probe_open(to);
    if let Presence::Unreadable(reason) = &target {
        summary.errors.push(format!(
            "说不出 {} 在不在（{reason}）—— 这一次不覆盖它：先让那个名字读得出来再改目录",
            to.display()
        ));
        return Ok(());
    }
    if matches!(target, Presence::Yes) {
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

/// 把目标上原有的文件改名成 `<原名>.pre-switch-<时间戳>`；**确认**不存在时才是 No-op。
///
/// 用 rename 而不是 copy：省一次全量读写，也不会出现"复制到一半失败、原文件与
/// 副本都不完整"的中间态。
///
/// 附属文件**先**搬、主库**后**搬，搬不动就整次失败（这条顺序与
/// `migration::backup_before_overwrite` 现在一致 —— 那边原先也是先搬主库、再 `let _ =`
/// 吞掉附属的失败，被现量之后改了过来，这里当时漏下了）。反过来的话失败那一刻目录里躺着的是
/// 「主库已改名带走、它那份 `-wal` 还在原位」的半套状态，而调用方只看到一句错误文案，
/// 不知道目录被动过；下一次重试时目标名已经空了 ⇒ 直接用源副本**盖上那份没搬走的旧
/// `-wal`**，SQLite 打开它时照这份错配的 WAL 恢复，而搬运随后还要删源 —— 唯一一份好数据
/// 就在那个被删的源目录里。
///
/// 「读不出它在不在」也不许当「不在」：这一步之后就是截断覆盖（`fs::copy`），
/// 说不准时宁可这次不搬。
fn keep_aside(dst: &Path) -> anyhow::Result<Option<PathBuf>> {
    match dst.metadata() {
        Ok(_) => {}
        Err(e) if is_confirmed_absent(&e) => return Ok(None),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "读不出 {} 在不在（{e}）：说不准就不覆盖它",
                dst.display()
            ))
        }
    }
    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S%3f").to_string();
    let mut name = dst
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    name.push(format!(".pre-switch-{timestamp}"));
    let kept = dst.with_file_name(name);
    // WAL 附属文件必须跟着主库走：只搬主库，目标目录里那份 `-wal` 就留在原位配上了新主库。
    // 附属文件本就不存在是常态（多数库早就 checkpoint 了），所以只在存在时搬；
    // 搬不动就当失败：宁可这次不覆盖，也不能留下主库与 wal 成套但来源不同的两份。
    for suffix in ["-wal", "-shm"] {
        let sidecar = with_suffix(dst, suffix);
        match sidecar.metadata() {
            Ok(_) => {
                std::fs::rename(&sidecar, with_suffix(&kept, suffix))
                    .map_err(|e| anyhow::anyhow!("{} 没能一起留档: {e}", sidecar.display()))?;
            }
            Err(e) if is_confirmed_absent(&e) => {}
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "读不出 {} 在不在（{e}）：说不准就不动它旁边那份主库",
                    sidecar.display()
                ))
            }
        }
    }
    std::fs::rename(dst, &kept)?;
    Ok(Some(kept))
}

/// `metadata()` 报回来的错里，只有「确实没有这个东西」才算 No-op。
///
/// `Path::exists()` / `is_dir()` 把「查不动」（移动盘刚唤醒、杀软首扫、同步盘占位、
/// 只读句柄挡着）折成「没有」，而这一步之后是**截断覆盖**。口径与本仓库 `Presence`
/// 那一条一致（旧桌面端那个 `FileState` 是同一形状，那份随前端一起删了）：三种结局分开。
///
/// `pub(crate)` 之后本 crate 里问"在不在"的地方都共用这一句判据
/// （`db::queries::classify_stat`、`edge_history::remove_temp_copy` 与
/// `edge_history::get_edge_history_counts`）—— 这条规则原先在三个文件里各写了一遍。
pub(crate) fn is_confirmed_absent(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::NotFound
}

/// 「这个东西在不在」——**真开一次只读句柄**来问，交回三种结局。
///
/// 用在"问完这一步就要覆盖或搬走那个文件"的地方。`Path::exists()` 与 `metadata()` 问的
/// 都是"能不能取到属性"，而那一刻要防的是「文件在、却问不出」（别的程序正握着句柄、杀软
/// 首扫、移动盘刚唤醒）：`exists()` 把它读成"没有"，调用方就当场覆盖那份还在被用的库，
/// 连留档都不留。开一次句柄分得清三件事 —— 开得上＝在，`NotFound`＝确认没有，其余＝说不出。
/// 口径与 [`is_confirmed_absent`] 一致：只有 `NotFound` 算"确认没有"。
///
/// `pub(crate)`：导入链（`migration::backup_before_overwrite`、`import_year_db`）与备份
/// 侧（`db::maintenance` 的清扫、体量探针）原先各写一遍"在不在"，现在共用这一把尺子。
///
/// 开不上时先看一眼是不是**目录占着这个名字**（导入时被同步盘挪过、或被换成同名目录的现场）：
/// 那是"有东西挡着"的确证而不是"问不出"，按 `Yes` 交回 —— 让调用方走到它自己那一步的
/// "这个库打不开"分支去说清细节，别在这一问就被折成"说不出在不在"。
pub(crate) fn probe_open(path: &Path) -> crate::db::connection::Presence {
    use crate::db::connection::Presence;
    match std::fs::File::open(path) {
        Ok(_) => Presence::Yes,
        Err(e) if is_confirmed_absent(&e) => Presence::No,
        Err(_) if path.is_dir() => Presence::Yes,
        Err(e) => Presence::Unreadable(format!("{}: {e}", path.display())),
    }
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

/// 配置里那两行 + 本进程实际生效的数据根 → 这次启动该做什么。**纯判断，不做任何副作用**。
///
/// 抽出来是因为 `run_pending_migration` 读的是 `config::instance()`，而那个单例一个进程
/// 只能初始化一次、路径启动即钉死（见 `migrate_data_tree` 的注释），单测里换不了目录；
/// 判断本身却是最容易判错、也最需要反例的一环。
#[derive(Debug)]
enum MigrationPlan {
    /// 标记与生效的数据根确实是同一个目录，而配置里的 `data_home` 也就是这个目录 ——
    /// 那是"先写标记、再写新目录"中途落盘失败留下的半写状态（`desktop` 侧
    /// `change_data_dir` 的注释里写明了这个顺序是故意的）：没有可搬的东西，清掉标记即可。
    ClearMarker,
    /// 配置里的新数据目录**没有生效**（`paths::resolve_data_home` 在目标建不出来时会
    /// 回落到程序目录：USB/网络盘开机还没就绪、同名普通文件、权限不足都是这一类）。
    /// 此刻指向的不是用户选的那个地方，而这次搬运是不可逆的（删源 + 清标记）⇒ 什么都不做，
    /// 标记留着下次重试。
    RetryLater {
        configured: PathBuf,
        problem: String,
    },
    /// 正常搬运。
    Migrate { from: PathBuf, to: PathBuf },
}

/// 把配置里的 `data_home` 原文按 `paths::resolve_data_home` 的同一条规则展开
/// （绝对路径原样、相对路径以程序目录为基准），好让两边比的是同一种东西。
fn expand_data_home(configured: &str, app_dir: &Path) -> PathBuf {
    let p = Path::new(configured);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        app_dir.join(p)
    }
}

fn plan_migration(
    from_raw: &str,
    configured_raw: &str,
    app_dir: &Path,
    resolved: &Path,
) -> MigrationPlan {
    let from = PathBuf::from(from_raw);
    // 这道闸必须在 `from == resolved` 之前：目标没生效时 `resolved` 就是程序目录，
    // 而旧目录在很多情形里也正是程序目录 —— 两者撞成同一个，下面的半写分支就会
    // 抢在"目标没到"这一支前面命中，把标记清掉。清掉等于把"这次没搬成"变成
    // "永远不再搬"（与 `migrate_data_tree` 为"旧根读不出来"补的那道闸是同一族，
    // 那处防的是源侧，这里防的是目标侧）。
    let configured_raw = configured_raw.trim();
    if !configured_raw.is_empty() {
        let configured = expand_data_home(configured_raw, app_dir);
        if !same_directory(&configured, resolved) {
            return MigrationPlan::RetryLater {
                problem: format!(
                    "配置里的新数据目录（{}）这次没能生效，本进程实际指向 {}；\
                     多半是那个盘还没就绪或建不出来。搬运是不可逆的（核对通过就删源），\
                     不能做在没到达的目标上 —— 标记保留，下次启动重试",
                    configured.display(),
                    resolved.display()
                ),
                configured,
            };
        }
    }
    if from == resolved {
        return MigrationPlan::ClearMarker;
    }
    MigrationPlan::Migrate {
        from,
        to: resolved.to_path_buf(),
    }
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
    let configured_raw = cfg.get("paths", "data_home").trim().to_string();
    let resolved = paths::data_home();
    let from = PathBuf::from(&from_raw);
    match plan_migration(&from_raw, &configured_raw, &paths::app_dir(), &resolved) {
        MigrationPlan::RetryLater {
            configured,
            problem,
        } => {
            // 与"搬不成"同一套处置：钉回旧目录继续记录、标记留着、启动报告出声。
            give_up(&from, &configured, problem);
            None
        }
        MigrationPlan::ClearMarker => {
            tracing::warn!(
                "data_migrate_from 与 data_home 是同一个目录（{}），没有可搬的东西，清掉标记",
                from.display()
            );
            clear_marker(cfg);
            None
        }
        MigrationPlan::Migrate { from, to } => match migrate_data_tree(&from, &to) {
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
        },
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
        match subtree_of(&base) {
            // 确认没有这一棵：没什么要核对的
            Subtree::Absent => continue,
            // 问不出：这一棵里的文件一个都没核对过，删源这道闸不许开
            Subtree::Unknown(why) => {
                return Err(format!("{why}：这一棵（{sub}）没能核对，不敢删源"));
            }
            Subtree::Dir => {}
        }
        let mut stack = vec![base.clone()];
        while let Some(dir) = stack.pop() {
            let entries = std::fs::read_dir(&dir)
                .map_err(|e| format!("{} 读取失败，无法核对: {e}", dir.display()))?;
            for entry in entries {
                let entry = match entry {
                    Ok(e) => e,
                    // 这一问的关键不对称：核对阶段"看不见某一条"绝不等于"那条不存在"。
                    // 旧写法 `entries.flatten()` 加 `let Ok(ft) = … else { continue }`
                    // 把两种"问不出"都当成"这一条不用核对"，于是复制阶段没搬走的文件
                    // 在核对阶段也一起消失，`Ok(())` 交出去、旧目录被删 —— 与本模块头
                    // 那句"删源只发生在复制成功且逐文件核对通过之后"正好相反。
                    Err(e) => {
                        return Err(format!(
                            "{} 枚举到一半读不出来（{e}）：这一层没能核对完，不敢删源",
                            dir.display()
                        ));
                    }
                };
                let from = entry.path();
                let ft = match entry.file_type() {
                    Ok(ft) => ft,
                    Err(e) => {
                        return Err(format!(
                            "{} 的类型问不出（{e}）：这一条没能核对，不敢删源",
                            from.display()
                        ));
                    }
                };
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

    /// `-wal` 搬不动时，主库必须原样留在原地：留档要么整套搬走，要么一步都不动。
    ///
    /// 旧写法先 `rename` 主库、再搬附属。附属失败那一刻，目录里躺着的是「主库已被改名带走、
    /// 它那份 `-wal` 还在原位」的半套状态，而调用方只看到一句错误文案，不知道目录被动过。
    /// 下一次重试时目标名已经空了 ⇒ 源的副本被直接盖在那份旧 `-wal` 上，SQLite 按错配的
    /// WAL 恢复；而这次搬运随后还要删源 —— 唯一一份好数据就在被删的那个目录里。
    #[test]
    fn a_sidecar_that_will_not_move_leaves_the_main_db_where_it_was() {
        let _lock = paths::test_app_dir_lock();
        let app = paths::test_app_dir("keep_aside_order");
        let dst = app.path().join("focusflow_2099.db");
        std::fs::write(&dst, "主库原本的字节").unwrap();
        // 把 `-wal` 做成一个"里面有打开句柄"的目录：Windows 上搬这种名字必失败，
        // 而随机时间戳的留档名没法提前占住
        let wal = with_suffix(&dst, "-wal");
        std::fs::create_dir_all(&wal).unwrap();
        let _hold = std::fs::File::create(wal.join("holding.bin")).unwrap();

        let err =
            keep_aside(&dst).expect_err("附属搬不动时整次留档必须失败，不许只记一句就当搬过了");
        let shown = err.to_string();
        assert!(shown.contains("-wal"), "要点名是哪一个搬不动: {shown}");
        assert!(
            dst.exists(),
            "主库不许在半套状态里被搬走: {}",
            dst.display()
        );
        let leftovers: Vec<String> = std::fs::read_dir(app.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".pre-switch-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "失败那一次不该留下半个留档（那意味着主库已被改名）: {leftovers:?}"
        );
    }

    /// 确认没有这个东西时才是 No-op —— 而「说不准」不是「没有」。
    #[test]
    fn only_a_confirmed_missing_target_is_a_no_op() {
        let missing = paths::data_dir().join("focusflow_2098.db");
        let _ = std::fs::remove_file(&missing);
        assert!(
            keep_aside(&missing).unwrap().is_none(),
            "确认不在 ⇒ 什么都不做"
        );
        // 两个错误码各管一头：2/3 是"确实没有"，5 是"查不动"，后者不许折成前者
        for code in [2, 3] {
            let e = std::io::Error::from_raw_os_error(code);
            assert!(is_confirmed_absent(&e), "code {code} 该算「确实没有」: {e}");
        }
        for code in [5, 32] {
            let e = std::io::Error::from_raw_os_error(code);
            assert!(
                !is_confirmed_absent(&e),
                "code {code}（受限或被握着）说不准，不能当「没有」就去覆盖: {e}"
            );
        }
    }

    /// 接线腿：这一步现在靠 `metadata()` 的三态，而不是把「查不动」折成「没有」的
    /// `exists()`；附属文件也必须在主库之前搬。这两件事都是字节顺序上的约束，
    /// 行为用例只挡住其中一半（附属搬不动那条），所以这里把形状钉住。
    #[test]
    fn keep_aside_reads_metadata_and_moves_sidecars_first() {
        let src = include_str!("data_location.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let at = prod.find("fn keep_aside(").expect("keep_aside 找不到了");
        let rest = &prod[at..];
        let end = rest.find("\n}\n").expect("函数切不出来");
        let code: String = rest[..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut bad: Vec<String> = Vec::new();
        if code.contains(".exists()") {
            bad.push(
                "又用 `exists()` 判断在不在：查不动会被折成「没有」，随后就是截断覆盖".to_string(),
            );
        }
        let sidecars_first = match (
            code.find("for suffix in [\"-wal\", \"-shm\"]"),
            code.find("std::fs::rename(dst, &kept)"),
        ) {
            (Some(side), Some(main)) => side < main,
            _ => false,
        };
        if !sidecars_first {
            bad.push("主库必须在附属文件之后搬：先搬主库就会留下半套状态".to_string());
        }
        // 外层那一问也要钉住：`keep_aside` 里面的三态挡不住"外面先判成没有、整段保护被绕过"
        let cf_rest = &prod[prod.find("fn copy_file(").expect("copy_file 找不到了")..];
        let cf_code: String = cf_rest[..cf_rest.find("\n}\n").expect("copy_file 函数体切不出来")]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        if cf_code.contains(".exists()") {
            bad.push("覆盖前那一问又用 exists()：保护段会被它整段绕过".to_string());
        }
        if !cf_code.contains("probe_open(to)") {
            bad.push("保护段必须走那把三态尺子".to_string());
        }
        assert!(bad.is_empty(), "{bad:#?}");
    }

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
        // 截图目录：它和 reports 一样是"用户产物"，必须跟着 data/ 一起走。
        // 这条断言盯的是搬家只认 `data`/`backup` 两棵子树这个口径 —— 一旦有人把
        // `screenshots_dir()` 挪到 data_home 根下，这里会从 4 变回 3 并且读不到文件。
        write_file(&src.path().join("data/screenshots/snip_a.png"), "png-bytes");
        write_file(&src.path().join("backup/focusflow_2026.db"), "backup-bytes");
        // 留在程序目录的两样：日志与插件，不该被这次复制带过去
        write_file(&src.path().join("logs/2026-09-27"), "log");
        write_file(&src.path().join("plugins/a.lua"), "lua");

        let s = copy_data_tree(src.path(), dst.path());
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        assert_eq!(s.copied, 4);
        assert!(s.bytes > 0);
        assert_eq!(
            std::fs::read_to_string(dst.path().join("data/reports/week.md")).unwrap(),
            "report"
        );
        assert_eq!(
            std::fs::read_to_string(dst.path().join("data/screenshots/snip_a.png")).unwrap(),
            "png-bytes",
            "截图没被带走：说明 screenshots 不在 data/ 下面，或者搬家漏了新目录"
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

    /// 目标那份**问不出在不在**时，`copy_file` 不许往下覆盖 —— 保护段不许被这一问绕过。
    ///
    /// 旧写法 `if to.exists() { …保护… }` 在属性问不出的那一刻连 `keep_aside` 都不进，直接落到
    /// 最后一句 `fs::copy` 把目标截断。`keep_aside` 上面那句"说不准就不覆盖"是同一件事，
    /// 只是原先写在了里面那一层、外面这一问先把它短路掉了。
    #[cfg(windows)]
    #[test]
    fn an_unanswerable_target_is_not_overwritten_when_switching_dirs() {
        use std::os::windows::fs::OpenOptionsExt;
        let _lock = paths::test_app_dir_lock();
        let src = paths::test_app_dir("cow_src");
        let dst = paths::test_app_dir("cow_dst");
        let from = src.path().join("focusflow_2026.db");
        let to = dst.path().join("focusflow_2026.db");
        write_file(&from, "new-copy");
        write_file(&to, "the-only-copy");
        let _holder = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&to)
            .expect("占位句柄应能打开");
        assert!(
            std::fs::File::open(&to).is_err(),
            "夹具没成立：这个句柄没挡住读，测不到「问不出」那一支"
        );

        let mut summary = CopySummary::default();
        copy_file(&from, &to, &mut summary).expect("说不准的那一格是停手，不是崩");
        assert!(
            summary.errors.iter().any(|e| e.contains("说不出")),
            "要说清是哪一问没答上，而不是「留档失败」那种猜出来的话: {:?}",
            summary.errors
        );
        assert_eq!(summary.copied, 0, "问不出就不许算搬过来了");

        drop(_holder);
        assert_eq!(
            std::fs::read_to_string(&to).expect("目标必须还在"),
            "the-only-copy",
            "被拒绝的那一步绝不能截断目标"
        );

        // 对照腿：读得动之后同一次复制要照常留档再覆盖（防修法做成"永远不搬"）
        let mut again = CopySummary::default();
        copy_file(&from, &to, &mut again).expect("读得动时照常搬");
        assert_eq!(again.copied, 1, "撤掉占用后要真的搬过去: {again:?}");
        assert!(
            again.backed_up.len() == 1 && again.backed_up[0].contains(".pre-switch-"),
            "覆盖前那份留档不许丢: {:?}",
            again.backed_up
        );
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

    /// 「更改数据目录」的提前校验必须照**调用方给的那个源**判，而不是自己再解析一次全局。
    ///
    /// 原先函数内部现读 `paths::data_home()`：那是第二个读点 —— 调用方前一行刚用
    /// `current_data_home()` 算出要搬走的目录，校验时读的却可能是另一个时刻解析出来的值。
    /// 在测试里这一点尤其要命：并行的别的用例会改写进程级 app_dir，于是
    /// 「嵌套目标该被拒」这几条断言偶发红（本仓多次把它记成 env flake，
    /// 现量：单跑本模块 6/6 绿、全量 4 次里红 2 次，红点是
    /// `validate_new_data_home(&app/data/sub).is_err()`）。src 由调用方交进来之后，
    /// 这条用例只依赖自己的临时目录。
    #[test]
    fn validate_new_data_home_rejects_bad_choices_up_front() {
        let _lock = paths::test_app_dir_lock();
        let app = paths::test_app_dir("va_app");
        let src = app.path().to_path_buf();
        write_file(&src.join("data/focusflow_2026.db"), "x");

        // 就是当前数据根
        assert!(validate_new_data_home(&src, &src).is_err());
        // 选进了正在被搬的子树里
        assert!(validate_new_data_home(&src, &src.join("data/sub")).is_err());
        // 目标建不出来（路径中间是个普通文件）
        let blocker = src.join("not_a_dir");
        write_file(&blocker, "x");
        assert!(validate_new_data_home(&src, &blocker.join("inner")).is_err());
        // 目标里已经有无关内容
        let messy = src.join("messy");
        write_file(&messy.join("notes.txt"), "hi");
        assert!(validate_new_data_home(&src, &messy).is_err());

        // 干净的兄弟目录 = 用户实测的那个手势，必须放行
        assert!(validate_new_data_home(&src, &src.join("123")).is_ok());
        // `data`/`backup` 是搬运自己的产物，不该被算作"无关内容"（重放第二次也要能过）
        let next = src.join("next");
        write_file(&next.join("data/keep.db"), "k");
        assert!(validate_new_data_home(&src, &next).is_ok());

        // src 真的被用上了：换一个源，同一个目标就不再是"套在自己里面"
        let elsewhere = src.join("elsewhere");
        write_file(&elsewhere.join("data/x.db"), "y");
        assert!(
            validate_new_data_home(&elsewhere, &src.join("data/sub")).is_ok(),
            "校验必须用调用方交进来的 src：源换成别处，这个目标就没有套进自己"
        );
    }

    /// 「目标目录列不出来」不许折成"里面是空的"，那等于放行一次看不清内容的搬家。
    ///
    /// 这条只能走纯函数：`create_dir_all` 刚成功、紧接着 `read_dir` 就失败的形态
    /// （受限 ACL、杀软握着目录、位置被同步盘换成别的东西）在测试里造不出来 ——
    /// 把那个位置做成同名**文件**会先落进前一条「目标目录建不出来」那一支。
    #[test]
    fn an_unreadable_destination_is_not_reported_as_empty() {
        let dst = std::path::Path::new("E:/wherever");
        let msg = strangers_verdict(Err(std::io::Error::from_raw_os_error(5)), dst)
            .expect_err("列不出目标里有什么，不能当成「里面没有别的内容」放行");
        assert!(msg.contains("读不出"), "拒绝的理由要说清是读不出来: {msg}");
        assert!(msg.contains("wherever"), "得说清是哪个目录读不出来: {msg}");

        // 同一个判据另外三支也要继续成立，免得这条修复把正常路径一起挡掉
        assert!(
            strangers_verdict(Ok(vec![]), dst).is_ok(),
            "空目录本来就该放行"
        );
        assert!(
            strangers_verdict(Ok(vec!["data".to_string(), "backup".to_string()]), dst).is_ok(),
            "data/backup 是搬运自己的产物，不该算无关内容"
        );
        let many: Vec<String> = (0..9).map(|i| format!("z{i:0>2}")).collect();
        let msg = strangers_verdict(Ok(many), dst).expect_err("有陌生内容必须拒");
        assert!(
            !msg.contains("z06"),
            "名单要照原来的规矩截到六个，别把整页名字塞进提示: {msg}"
        );
        assert!(
            msg.contains("z00") && msg.contains("z05"),
            "截断后留前六个: {msg}"
        );
    }

    /// 接线腿：校验必须把 `read_dir` 的**结果**交给判据，不许再把失败折成空名单。
    ///
    /// 上面那条测的是判据本身；调用点退回 `if let Ok(..)` 时判据照样全绿，
    /// 而"看不清也放行"这个症状只会在调用点上发生。
    #[test]
    fn the_destination_check_feeds_the_real_listing_to_the_verdict() {
        let src = include_str!("data_location.rs");
        let at = src
            .find("pub fn validate_new_data_home(")
            .expect("校验函数找不到了");
        let end = src[at..].find("\n}\n").expect("函数切不出来");
        let body = &src[at..at + end + 3];
        assert!(
            body.contains("strangers_verdict("),
            "判据没接上真实的列目录结果"
        );
        for fold in ["if let Ok(", ".unwrap_or_default(", ".unwrap_or(", ".ok()"] {
            assert!(
                !body.contains(fold),
                "列目录的失败被折成「名单为空」（{fold}）= 校验在看不清目标时静默放行"
            );
        }
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

#[cfg(test)]
mod migration_plan_tests {
    use super::*;

    /// 把配置里的 `[paths] data_home` 原文展开成"应当生效的那个目录"。
    fn configured_of(app: &Path, raw: &str) -> PathBuf {
        expand_data_home(raw, app)
    }

    /// 目标目录这次没生效（回落到程序目录）时，绝不能清掉迁移标记。
    ///
    /// 旧实现只看 `data_migrate_from == 实际生效的 data_home`：而 `resolve_data_home`
    /// 在建不出目标时（USB/网络盘开机还没就绪是最常见的一种，而"改数据目录"的人多半
    /// 正是为了把数据放到那种盘上）回落到程序目录，旧目录在此刻**也**就是程序目录，
    /// 于是这一对撞成相同 → 清标记 → "这次没搬成"变成"永远不再搬"：全部历史留在旧目录，
    /// 盘挂上之后新目录从今天起开始攒第二份，两边越差越远。
    #[test]
    fn a_target_that_never_took_effect_keeps_the_marker() {
        let _lock = paths::test_app_dir_lock();
        let app = paths::test_app_dir("plan_fallback");
        // 夹具：目标建不出来（路径中间是个普通文件）⇒ 本进程回落到程序目录
        let blocker = app.path().join("not_a_dir");
        std::fs::write(&blocker, "x").unwrap();
        let wanted = configured_of(app.path(), &blocker.join("FocusData").to_string_lossy());
        let resolved = app.path().to_path_buf();

        // 前提：这正是旧代码会误判成"半写状态"的那一对 —— 旧目录与生效目录是同一个
        let from = resolved.clone();
        assert_eq!(
            from, resolved,
            "夹具的前提就是旧代码那一对相等，否则这条照不出回归"
        );
        let plan = plan_migration(
            &from.to_string_lossy(),
            &wanted.to_string_lossy(),
            app.path(),
            &resolved,
        );
        match &plan {
            MigrationPlan::RetryLater {
                configured,
                problem,
            } => {
                assert_eq!(
                    configured, &wanted,
                    "话要说清是**哪个**目标没到，不是回落后那个"
                );
                assert!(
                    problem.contains("没能生效"),
                    "文案该说清为什么不动手: {problem}"
                );
                assert!(
                    problem.contains("标记保留"),
                    "要说清下次启动会重试: {problem}"
                );
            }
            _ => panic!("目标没生效时必须留着标记重试，不该清标记、也不该删源: {plan:?}"),
        }
    }

    /// 反向腿：三种"目标确实生效"的写法都不许被误判成没生效。
    ///
    /// 这一半才是这条修改真正的风险 —— 误判会让用户每次启动都搬不成、还永远看不到原因。
    #[test]
    fn a_target_that_did_take_effect_is_never_called_a_fallback() {
        let _lock = paths::test_app_dir_lock();
        let app = paths::test_app_dir("plan_ok");
        let target = paths::test_app_dir("plan_ok_target");
        let from = app.path().to_path_buf();

        // ① 绝对路径、盘也真的建出来了：正常搬运
        let raw = target.path().to_string_lossy().to_string();
        let plan = plan_migration(
            &from.to_string_lossy(),
            &raw,
            app.path(),
            &configured_of(app.path(), &raw),
        );
        assert!(
            matches!(plan, MigrationPlan::Migrate { .. }),
            "目标生效时该搬运: {plan:?}"
        );

        // ② 同一个目录多写一个尾随分隔符（用户在配置里手打路径的常态）
        let spelled = format!("{raw}{}", std::path::MAIN_SEPARATOR);
        let plan = plan_migration(
            &from.to_string_lossy(),
            &spelled,
            app.path(),
            &configured_of(app.path(), &raw),
        );
        assert!(
            !matches!(plan, MigrationPlan::RetryLater { .. }),
            "同一个目录的另一种写法不该被判成没生效: {plan:?}"
        );

        // ③ 相对写法：按程序目录展开后与生效目录一致
        let rel_home = app.path().join("MyData");
        std::fs::create_dir_all(&rel_home).unwrap();
        let plan = plan_migration(&from.to_string_lossy(), "MyData", app.path(), &rel_home);
        assert!(
            !matches!(plan, MigrationPlan::RetryLater { .. }),
            "相对形式的 data_home 不该被判成没生效: {plan:?}"
        );
    }

    /// 半写状态（写了 `data_migrate_from`、`data_home` 还是旧目录）必须仍走"清掉标记"，
    /// 这是 `change_data_dir` 刻意先写标记再写新目录时所依赖的那一支。
    #[test]
    fn the_half_written_state_still_clears_the_marker() {
        let _lock = paths::test_app_dir_lock();
        let app = paths::test_app_dir("plan_half");
        let plan = plan_migration(
            &app.path().to_string_lossy(),
            &app.path().to_string_lossy(),
            app.path(),
            app.path(),
        );
        assert!(
            matches!(plan, MigrationPlan::ClearMarker),
            "data_home 与标记都指旧目录 = 半写状态，该清标记: {plan:?}"
        );

        // 配置里根本没写 data_home（便携包的默认布局）时也照旧走原来的两条分支
        let plan2 = plan_migration(&app.path().to_string_lossy(), "", app.path(), app.path());
        assert!(matches!(plan2, MigrationPlan::ClearMarker), "{plan2:?}");
    }
}

#[cfg(test)]
mod delete_gate_tests {
    use super::*;

    /// 本 mod 自己的写文件小助手（`write_file` 住在 `tests` mod 里，跨 mod 够不着）。
    fn fs_write(p: &Path, content: &str) {
        std::fs::write(p, content).unwrap();
    }
    /// 「这一棵问不出」与「这一棵本来没有」不许共用一个跳过。
    ///
    /// 症状是**数据没了还念成功**：复制那一步 `!src.is_dir() ⇒ continue`、核对那一步同一个写法，
    /// 于是那整棵（用户的全部历史都在 `data/` 里）一个文件都没搬、一个文件都没核对，
    /// `Ok(())` 交出去，`remove_source_trees` 把旧目录删掉，回执念的是「迁移完成」。
    /// 触发形状本机造得出来：自指 junction 让 `metadata` 报 os error 1921（`FilesystemLoop`），
    /// 而 `Path::is_dir()` 对任何一次失败都回 `false` —— 与本仓 `year_db_presence`、
    /// `is_confirmed_absent` 立过两次的规矩是同一件事。
    #[test]
    fn an_unanswerable_subtree_never_opens_the_delete_gate() {
        let _lock = paths::test_app_dir_lock();
        let app = paths::test_app_dir("dl_unanswerable");

        // 控制带①：一切都答得出来的搬运，两道闸都该放行（否则下面的断言是恒真）
        let ok_src = app.path().join("ok_src");
        let ok_dst = app.path().join("ok_dst");
        std::fs::create_dir_all(ok_src.join("data")).unwrap();
        fs_write(&ok_src.join("data").join("focusflow_2026.db"), "一行历史");
        let s = copy_data_tree(&ok_src, &ok_dst);
        assert!(
            s.errors.is_empty(),
            "夹具前提：正常那棵不该报错，实得 {:?}",
            s.errors
        );
        assert_eq!(s.copied, 1, "夹具前提：真的要搬到一个文件");
        verify_copy(&ok_src, &ok_dst).expect("复制过且核对得上的那棵该放行");

        // 观察带：`data` 这一棵换成一个读不出真身的自指 junction
        let bad_src = app.path().join("bad_src");
        let bad_dst = app.path().join("bad_dst");
        std::fs::create_dir_all(&bad_src).unwrap();
        if !paths::test_self_loop_junction(&bad_src.join("data")) {
            eprintln!("这台机器建不出自指 junction ⇒ 这一条没测过，只是跳过了");
            return;
        }
        let s2 = copy_data_tree(&bad_src, &bad_dst);
        let text2 = s2.errors.join("\n");
        assert!(
            !s2.errors.is_empty(),
            "问不出「这一棵在不在」必须报错，不许与「本来没有」共用一个跳过"
        );
        assert!(
            text2.contains("问不出") && text2.contains("一个文件都没复制"),
            "错误要点名是哪一棵、这次搬了几个，实得: {text2}"
        );
        assert_eq!(s2.copied, 0, "夹具前提：这一棵一个都没搬成");
        let why =
            verify_copy(&bad_src, &bad_dst).expect_err("没核对过这一棵就不许说「敢删源」，实得 Ok");
        assert!(
            why.to_string().contains("不敢删源"),
            "要说清是「没能核对所以不许删源」，实得: {why}"
        );
    }
}
