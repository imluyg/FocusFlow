//! 旧数据导入：从旧版数据目录迁移到当前数据目录。
//!
//! 场景：
//! - 从 Python 版 FocusFlow 切换到 Rust 版（schema 兼容，直接迁移）
//! - 换电脑/换目录后迁移旧数据
//!
//! 设计：
//! - 年度键鼠库 `focusflow_YYYY.db`：先写入暂存表 key_log（按 timestamp 去重，幂等），
//!   再通过聚合迁移落进 daily/hourly/key 三张聚合表并压缩文件；
//!   按天的**活跃时长**单独并（明细里没有时长信息，旧库存在 `active_seconds` 表
//!   或 `daily_counts.seconds` 里，同一天取两侧较大值 —— 幂等且不覆盖新值）
//! - 重复导入靠源文件指纹（大小+mtime）跳过；指纹变了但 key_log **内容**没变
//!   （复制/云盘摸过 mtime 的同一份文件）靠内容指纹（行数+最大时间戳）跳过 ——
//!   聚合是累加式，把同一批明细再聚一遍就是整体翻倍
//! - 附属库（accounting/pomodoro/scheduler/edge_history）：整体复制覆盖，
//!   **覆盖前先把现有库改名留档**（见 `backup_before_overwrite`）——
//!   导入目录由用户自己选，选错目录不能让当前数据凭空消失
//! - 若目标库不存在则整体复制文件（最快路径）
//! - 输出导入汇总

use std::path::Path;

use rusqlite::Connection;

use crate::db::connection;
use crate::paths;

/// 导入结果汇总。
#[derive(Debug, Default)]
pub struct ImportSummary {
    /// 导入的年度库
    pub year_dbs: Vec<i32>,
    /// 各年度导入的键鼠记录数
    pub records_by_year: Vec<(i32, i64)>,
    /// 复制的附属库
    pub copied_aux: Vec<String>,
    /// 被导入覆盖、已改名留档的附属库（旧文件路径，供 UI 提示用户）
    pub backed_up_aux: Vec<String>,
    /// 跳过的文件（无数据/已存在）
    pub skipped: Vec<String>,
    /// 错误
    pub errors: Vec<String>,
}

/// 附属库文件名列表（直接复制）。
const AUX_DBS: &[&str] = &[
    "focusflow_accounting.db",
    "focusflow_pomodoro.db",
    "focusflow_scheduler.db",
    "focusflow_edge_history.db",
];

/// 从旧数据目录导入全部数据到当前数据目录。
pub fn import_legacy_data(src_dir: &Path) -> ImportSummary {
    use crate::db::connection::Presence;
    let mut summary = ImportSummary::default();

    // 1) 年度键鼠库
    let src_year_dbs = list_year_dbs(src_dir);
    for year in src_year_dbs {
        match import_year_db(src_dir, year, &mut summary) {
            Ok(imported) => {
                summary.year_dbs.push(year);
                summary.records_by_year.push((year, imported));
            }
            Err(e) => summary.errors.push(format!("{year} 年度库导入失败: {e}")),
        }
    }

    // 2) 附属库：整体复制（表独立，无法按行合并）
    for aux in AUX_DBS {
        let src = src_dir.join(aux);
        // 这一问也走那把三态尺子：`!src.exists()` 在"旧目录里那份正被握着、属性问不出"时同样
        // 回 false ⇒ 整块附属库被当成「旧目录里没有」跳过，而这一轮报的还是"导入完成"——
        // 用户随后清掉旧目录，那份记账/日程/浏览历史就没了。只有 `NotFound` 才算确认没有。
        match crate::data_location::probe_open(&src) {
            Presence::No => continue,
            Presence::Unreadable(reason) => {
                summary.errors.push(format!(
                    "{aux} 问不出旧目录里有没有（{reason}）—— 本次跳过这一份"
                ));
                continue;
            }
            Presence::Yes => {}
        }
        let dst = paths::data_dir().join(aux);
        // 自己导自己必须先看住：用户在目录选择器里挑到自己当前的 data/ 上是很常见的
        // 误操作。下面那步留档用的是 rename —— 它会把"源文件"一起搬走，紧接着的
        // copy 就找不到源了，结果是附属库从正名上消失、应用下次打开建一个空库，
        // 表现就是"我的账记/日程数据没了"（文件其实还在，只是躺在 .import-backup-* 里）。
        if same_file(&src, &dst) {
            summary
                .skipped
                .push(format!("{aux}（源与目标是同一个文件，无需导入）"));
            continue;
        }
        // 现有库先留档再覆盖：导入目录是用户手选的，选错目录（或新版本里
        // 已记了几个月账）时当前数据不能就这么没了。
        let kept = match backup_before_overwrite(&dst) {
            Ok(k) => {
                if let Some(p) = &k {
                    summary.backed_up_aux.push(p.display().to_string());
                }
                k
            }
            Err(e) => {
                // 留档失败就不覆盖：宁可这次不导入，也不能拿用户数据冒险
                summary
                    .errors
                    .push(format!("{aux} 覆盖前留档失败，已跳过: {e}"));
                continue;
            }
        };
        match std::fs::copy(&src, &dst) {
            Ok(_) => {}
            Err(e) => {
                summary.errors.push(format!("{aux} 复制失败: {e}"));
                restore_after_failed_copy(&mut summary, &dst, &kept);
                continue;
            }
        }
        // `-wal` 里是**还没 checkpoint 进主库的记录**（§十四 在真库上量到 `-wal` 比
        // 主库还大）。年度库那一支早就为此改成"搬不动就回滚、报错、不报已导入"，
        // 附属库这一支以前只 `std::fs::copy(&src, &dst)` 那一个文件 ⇒ 旧目录里那段
        // 尾巴永久留在原地，而界面报的是「导入完成」（记账/日程/浏览历史都算）。
        let mut sidecar_fail: Option<String> = None;
        for suffix in ["-wal", "-shm"] {
            let from = crate::data_location::with_suffix(&src, suffix);
            let to = crate::data_location::with_suffix(&dst, suffix);
            // 这里也不许用 `!from.exists()`：源 `-wal` 正被别的程序握着时它回 false ⇒ 那段还没
            // checkpoint 的记录被静默留在旧目录，而这一支照常 push `copied_aux`（界面说"已复制"）
            match crate::data_location::probe_open(&from) {
                Presence::No => continue,
                // 共享内存索引，SQLite 会照着 WAL 自己重建：拿不到、问不出都算了
                Presence::Unreadable(reason) if suffix == "-shm" => {
                    tracing::debug!(
                        "说不出 {aux} 的 -shm 在不在（{reason}）：它会照着 WAL 重建，跳过"
                    );
                    continue;
                }
                Presence::Unreadable(reason) => {
                    sidecar_fail = Some(format!("问不出 {} 在不在（{reason}）", from.display()));
                    break;
                }
                Presence::Yes => {
                    if let Err(e) = std::fs::copy(&from, &to) {
                        if suffix == "-shm" {
                            tracing::debug!("跳过 {aux} 的 -shm 副本: {e}");
                            continue;
                        }
                        sidecar_fail = Some(format!("{} → {}: {e}", from.display(), to.display()));
                        break;
                    }
                }
            }
        }
        if let Some(detail) = sidecar_fail {
            summary.errors.push(format!(
                "{aux} 的 WAL 没搬过来（{detail}）—— 那里面可能还有未落盘记录"
            ));
            restore_after_failed_copy(&mut summary, &dst, &kept);
            continue;
        }
        summary.copied_aux.push(aux.to_string());
    }

    summary
}

/// 两个路径是否指向同一个已存在的文件（用于看住"自己导自己"）。
///
/// 认不出来时返回 false：宁可照常走导入流程，也不要在读不了元数据的时候
/// 悄悄跳过一份真正该导入的数据。
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// 覆盖前的留档：把现有文件改名成 `<原名>.import-backup-<时间戳>`。
///
/// 用 rename 而不是 copy —— 既省一次全量拷贝，也保证不会出现"复制到一半
/// 失败、原文件和新文件都不完整"的中间态。返回被留下的路径（原先不存在时为 None）。
fn backup_before_overwrite(dst: &Path) -> anyhow::Result<Option<std::path::PathBuf>> {
    use crate::db::connection::Presence;
    // 覆盖前这一问必须分得清三件事。旧写法 `!dst.exists()` 把"这一刻问不出"（现有库正被
    // 别的程序握着、杀软首扫、移动盘刚休眠）读成「没有要留档的东西」，于是那份**还在被用**
    // 的附属库被直接覆盖、一份留档都没留 —— 与这个函数存在的理由（"宁可这次不导入，也不能
    // 拿用户数据冒险"）正好相反，而且事后无从找回。
    match crate::data_location::probe_open(dst) {
        Presence::No => return Ok(None),
        Presence::Unreadable(reason) => {
            return Err(anyhow::anyhow!(
                "说不出这份现有库在不在（{reason}）—— 先让那个名字读得出来，本次不覆盖"
            ))
        }
        Presence::Yes => {}
    }
    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S%3f").to_string();
    let mut name = dst
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("路径没有文件名: {}", dst.display()))?
        .to_os_string();
    name.push(format!(".import-backup-{timestamp}"));
    let kept = dst.with_file_name(name);
    // 附属文件**先**搬、主库**后**搬，搬不动就整次失败。
    // ① 顺序：旧写法先把主库 rename 走，再 `let _ =` 吞掉 sidecar 的失败 —— 那一刻目录里
    //    躺着的是"主库已改名带走、别人那份 -wal 还在原位"的半套状态，而调用方只看到
    //    一条错误文案，不知道目录被动过了。
    // ② 不吞：`-wal` 里正是还没 checkpoint 进主库的那段新数据（§十四 在真库上量到
    //    `-wal` 比主库还大），留在原位等于让下次打开按错配的 WAL 去恢复。
    // ③ 路径用 OsString 追加，不用 `format!("{}", path.display())`：后者对非 UTF-8
    //    文件名是 lossy 转换，拼出来的路径根本不存在 ⇒ 配合 `let _ =` 连失败都不会报。
    for suffix in ["-wal", "-shm"] {
        let from = crate::data_location::with_suffix(dst, suffix);
        match crate::data_location::probe_open(&from) {
            Presence::No => {}
            // 问不出就一步都不搬：主库的 rename 紧跟在后面，把它搬走而这一格留在原地，
            // 目录里就剩下"主库已改名带走、那段 -wal 还在原位"的半套状态
            Presence::Unreadable(reason) => {
                return Err(anyhow::anyhow!(
                    "{} 在不在问不出（{reason}）—— 整次留档不做，免得主库搬走后这段尾巴留在原位",
                    from.display()
                ))
            }
            Presence::Yes => {
                std::fs::rename(&from, crate::data_location::with_suffix(&kept, suffix))
                    .map_err(|e| anyhow::anyhow!("{} 没能一起留档: {e}", from.display()))?;
            }
        }
    }
    std::fs::rename(dst, &kept)?;
    tracing::info!(
        "导入前已留档现有库: {} -> {}",
        dst.display(),
        kept.display()
    );
    Ok(Some(kept))
}

/// 列出数据目录下的年度库（focusflow_YYYY.db）。
fn list_year_dbs(dir: &Path) -> Vec<i32> {
    let mut years = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Some(y) = paths::is_year_db_file(&entry.path()) {
                years.push(y);
            }
        }
    }
    years.sort_unstable();
    years
}

/// 回滚刚整体复制过来的半套年度库，返回删不掉的残留（路径 + 原因）。
fn rollback_fresh_copy(dst: &Path) -> Vec<String> {
    let mut leftover = Vec::new();
    let mut remove = |path: &Path| {
        if let Err(e) = std::fs::remove_file(path) {
            // 删完再回头 `exists()` 问一遍是第二个能失败的调用：那一刻"问不出"会被读成
            // "没有"，于是这份残缺副本从此没人点名，而下一次导入照样把它当目标库。
            // `NotFound`＝确实没有（别人删掉了），其余＝必须出声。
            if !crate::data_location::is_confirmed_absent(&e) {
                leftover.push(format!("{}（{e}）", path.display()));
            }
        }
    };
    remove(dst);
    for suffix in ["-wal", "-shm"] {
        remove(&crate::data_location::with_suffix(dst, suffix));
    }
    leftover
}

/// 回滚结局对应的"接下来怎么办"：清理干净才允许说"直接重试"。
fn retry_hint(leftover: &[String]) -> String {
    if leftover.is_empty() {
        "这次复制的半份副本已清掉，处理后可以直接重试".to_string()
    } else {
        format!(
            "但自动清理没成功（{}）。这些残留不清掉，下次导入可能仍走不成、或被当成\
             「目标库已存在」改走合并分支（读的正是缺这段尾巴的副本）—— 请先手动删掉再重试",
            leftover.join("；")
        )
    }
}

/// 导入单个年度库：先写暂存表（按 timestamp 去重），再聚合落库。
/// 返回导入的记录数。
/// 导入某一份年度库，返回本次落进暂存表的明细行数。
///
/// **每一条 `Ok(0)` 都必须自己往 `summary.skipped` 里写一条**：调用方照样把这一年
/// 记进 `year_dbs`，只按数字说话时「2025 年度键鼠: 0 条」与"这一年本来没数据"逐字节
/// 同形，而重跑导入正是用户查"刚才到底导进去没有"的动作。现存三处 `Ok(0)`
/// （只剩聚合表 / 源文件未变 / 明细内容指纹未变）都守这条。
fn import_year_db(src_dir: &Path, year: i32, summary: &mut ImportSummary) -> anyhow::Result<i64> {
    use crate::db::connection::Presence;
    let src_path = src_dir.join(format!("focusflow_{year}.db"));
    let dst_path = paths::year_db_path(year);

    // 目标库不存在 → 整体复制（最快）。这一问原先用 `!dst_path.exists()`：那一刻问不出
    // （目标库正被别的程序握着、移动盘刚休眠）会被读成「目标库还没有」⇒ 紧跟着的
    // `fs::copy` 把那一整年的**现有库整份覆盖**，而这一支不像附属库那一路会先留档 ——
    // 覆盖掉的那一年再也找不回来。说不出就两条路都不走：既不整库覆盖，也不当成空库去合并。
    let dst_there = match crate::data_location::probe_open(&dst_path) {
        Presence::Yes => true,
        Presence::No => false,
        Presence::Unreadable(reason) => {
            return Err(anyhow::anyhow!(
                "说不出 {year} 年目标库在不在（{reason}）：既不能整库覆盖，也不能当成空库走合并 —— 本年度跳过"
            ));
        }
    };
    if !dst_there {
        std::fs::copy(&src_path, &dst_path)?;
        // 复制 WAL/SHM（若有未 checkpoint 数据）。这一问原先用的是 `from.exists()`：源 `-wal`
        // 正被别的程序握着（同步盘在传、杀软首扫、移动盘休眠）时它也回 false ⇒ 那段尾巴被静默
        // 跳过，然后照常写 `imported_src_*` 标记、把整年报成"已导入" ⇒ 之后重导被大小+mtime 与
        // 内容指纹一路跳过，那一年最新的一段**永久拿不回来**。问不出与搬不动是同一种结局，
        // 所以两条都汇成同一个 abort：回滚刚复制的主库、报错、不写标记。
        let mut wal_missing: Option<String> = None;
        for suffix in ["-wal", "-shm"] {
            // 不能用 `format!("{}{}", path.display(), suffix)`：display() 对非 UTF-8 的
            // 目录名是 lossy 转换，拼出来的路径根本不存在 ⇒ WAL 被静默跳过（它正是还没
            // 进主库的那一段），整年照报"已导入"并写标记。
            let from = crate::data_location::with_suffix(&src_path, suffix);
            let to = crate::data_location::with_suffix(&dst_path, suffix);
            match crate::data_location::probe_open(&from) {
                Presence::No => continue,
                // `-shm` 是共享内存索引，SQLite 会照着 WAL 自己重建：拿不到、问不出都算了
                Presence::Unreadable(reason) if suffix == "-shm" => {
                    tracing::debug!(
                        "说不出 {year} 年库的 -shm 在不在（{reason}）：它会照着 WAL 重建，跳过"
                    );
                    continue;
                }
                Presence::Unreadable(reason) => {
                    wal_missing = Some(format!("问不出 {} 在不在（{reason}）", from.display()));
                    break;
                }
                Presence::Yes => {
                    if let Err(e) = std::fs::copy(&from, &to) {
                        if suffix == "-shm" {
                            tracing::debug!("跳过 {year} 年库的 -shm 副本: {e}");
                            continue;
                        }
                        // `-wal` 不一样：那里面是**还没 checkpoint 进主库的记录**。吞掉它
                        // （原来是 `let _ =`）会继续往下走、把整年总量报成"已导入"，
                        // 并写下 `imported_src_*` 标记 —— 之后重导会被大小+mtime 与内容
                        // 指纹比对直接跳过（见下面的 same_size / key_log 指纹），
                        // 那一年最新的一段就永久拿不回来了。
                        // 回滚本身失败时不能还说"请重试"：半份库留在原地，重试就换了分支。
                        wal_missing = Some(format!("{} → {}: {e}", from.display(), to.display()));
                        break;
                    }
                }
            }
        }
        if let Some(detail) = wal_missing {
            // 回滚是为了让重试仍走这条整体复制分支；留着半份库会让下次导入改走合并分支，
            // 而它读的是已复制过来的主库（正缺那段尾巴）。
            let leftover = rollback_fresh_copy(&dst_path);
            return Err(anyhow::anyhow!(
                "{year} 年库的 WAL 没搬过来（{detail}）—— \
                 那里面可能还有未落盘记录，本次导入未完成。{}",
                retry_hint(&leftover)
            ));
        }
        // 用 Rusqlite 打开确认可用 + 建聚合表
        let conn = connection::open_rw(&dst_path)?;
        connection::ensure_schema(&conn, year)?;
        // 指纹从这份**刚复制过来的**库里读：它与源文件逐字节相同、连接已经开着，
        // 不需要再开第三次源文件（`ensure_schema` 不建 key_log，所以这里读到的
        // 有无明细与源库一致）。
        let kl = key_log_fingerprint(&conn)?;
        drop(conn);
        // **标记写在聚合之前**，与下面合并分支同一条理由（那里的注释是完整版）：
        // `migrate_v2` 是累加式且没有去重键，"这份导过了"只记在 `meta` 的
        // `imported_src_*` 里。旧顺序在两步之间失败时留下「已聚合、未标记」，
        // 用户点重试 ⇒ 读不到标记 ⇒ 那一年再累加一遍。整库复制这一支以前正好漏在
        // 外面（"整库都搬过来了，标记写不上去有什么关系"），而它一样要跑 `migrate_v2`。
        record_import_marker(&dst_path, &src_path, kl)?;
        crate::db::maintenance::migrate_v2();
        // 统计导入条数（聚合表总量）
        let conn = connection::open_ro(&dst_path)?;
        let count: i64 = conn.query_row(
            "SELECT COALESCE(SUM(count), 0) FROM daily_counts",
            [],
            |r| r.get(0),
        )?;
        return Ok(count);
    }

    // 目标库已存在：先把导入标记读一次，而且**只读一次**（下面两处都用这份）。
    //
    // 读不动 ≠ 没有标记。两道去重闸（源文件大小+mtime、key_log 内容指纹）认的都是
    // "标记里写着的值"，把 Err 折成 `None` 等于当场把两道闸卸掉 —— 而 `migrate_v2`
    // 是累加式且没有去重键 ⇒ 同一份旧明细被再聚一遍，那一年整体翻倍，界面上一个字
    // 都没有。上面整库复制那一支的注释（"标记写在聚合之前"）修的是同一个形状的写侧，
    // `record_import_marker` 的注释修的是它自己的读侧，这是剩下的这一半。
    let marker = match read_import_marker(&dst_path) {
        Ok(m) => m,
        Err(e) => {
            return Err(anyhow::anyhow!(
                "{year} 年目标库的导入标记读不出来（{e}）：既不能证明「源文件没变过」，\
                 也不能证明「明细已经导过」，而累加式导入会把这一年翻倍 —— 本年度跳过。\
                 先让那个库能正常打开（被同步盘换成占位文件、被另一个进程独占都是这个形态）"
            ));
        }
    };
    // 源文件未变化（大小+修改时间一致）→ 已导入过，跳过
    if let Ok(meta) = std::fs::metadata(&src_path) {
        let same_size = marker.as_ref().map(|m| m.size) == Some(meta.len());
        let same_mtime = match (marker.as_ref().and_then(|m| m.mtime), meta.modified().ok()) {
            (Some(a), Some(b)) => {
                let a_s = a
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let b_s = b
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                a_s == b_s
            }
            _ => false,
        };
        if same_size && same_mtime {
            // 这条 `Ok(0)` 也得往 `summary.skipped` 里写一条（本函数开头的契约）：
            // 调用方照样把这一年记进 `year_dbs`，只按数字说话时「2025 年度键鼠: 0 条」
            // 与"这一年本来没数据"逐字节同形 —— 而重跑导入正是用户查"刚才到底导进去没有"
            // 的动作。`tracing::info!` 不算答复：日志埋在 logs/ 里，不主动翻就永远不知道。
            let note = format!(
                "{year} 年源文件与上次导入时一模一样（大小与修改时间都没变）：整库跳过，本次没有新数据"
            );
            tracing::info!("{note}");
            summary.skipped.push(note);
            return Ok(0);
        }
    }

    // 去重合并（写入暂存表）
    let src_conn = Connection::open(&src_path)?;
    let dst_conn = connection::open_rw(&dst_path)?;
    connection::ensure_schema(&dst_conn, year)?;

    // 活跃时长必须在检查 key_log 之前先并：这一步与明细无关，而源库有可能
    // 早就只剩聚合表（暂存明细聚合完就被丢弃），那种库走不到下面那段。
    merge_active_seconds_from_src(&src_conn, &dst_conn)?;

    // 源库 key_log 的内容指纹：没有这张表就是"只剩聚合表"，没有明细可导。
    let Some(kl_now) = key_log_fingerprint(&src_conn)? else {
        // **这条分支是本仓年度库的常态**：明细聚完就丢表，所以"上一个版本记的那一年"
        // 天生就是只剩聚合表。此时按键数/按日/按小时/按键明细整段不并（口径早已定成
        // "不累加"，见 migration_test 里那条"聚合计数不做累加（无法幂等去重）"），
        // 只有上面那段活跃时长并进来了。旧写法回一个裸的 `Ok(0)`，调用方照样把这一年
        // 记进 `year_dbs` ⇒ 报告上是一行「2025 年度键鼠: 0 条」，与"这一年本来没数据"
        // 完全同形，而那一份按键数其实被丢下了（同族：`backup_database` 的 Done 曾经
        // 不带 failed）。口径不改，改的是**不许谎报**。
        let note = format!(
            "{year} 年源库只剩聚合表：活跃时长已并，按键数与按键明细未并（目标库已有同一年，累加会翻倍）"
        );
        tracing::warn!("{note}");
        summary.skipped.push(note);
        return Ok(0);
    };

    // 大小/mtime 变了、key_log 内容却逐行没变 —— 同一份旧库被资源管理器复制、
    // 云盘同步或备份还原摸过 mtime 都是这个形状 —— 一行新增明细都没有。
    // 暂存表在上轮聚合后已清空，对它没有任何记忆：照旧往下走，整套旧明细会
    // 被**再聚合一遍**，而 migrate_v2 是累加式（`count = count + excluded`），
    // 那一年就整体翻倍，界面上没有任何提示。
    // 用开头读过的那一份标记（不再第三次开库：读不动的结局上面已经处理过了）
    if let Some((c0, t0)) = marker.as_ref().and_then(|m| m.key_log) {
        if (c0, t0) == kl_now {
            // 同一条契约的第二处：第一道闸（大小+mtime）已经放行、靠内容指纹才跳过，
            // 报出来的还是一个裸 `0`。
            let note = format!(
                "{year} 年源库的明细内容与上次导入逐行相同（只有文件时间戳被改动过）：跳过，不重复聚合"
            );
            tracing::info!("{note}");
            summary.skipped.push(note);
            return Ok(0);
        }
    }
    // 已知局限（判定不修，留档）：源库**真的追加过**新明细时，这里仍会整份重聚合，
    // 与上次导入的重叠段翻倍。要修就得按 `timestamp > 上次最大值` 过滤或保留暂存表
    // 当去重记忆 —— 前者会把"另一份更早的旧目录里独有的历史"挡在外面（多源回填
    // 是导入对话框明说的用法），后者要放弃"聚合后连表丢掉"的压缩设计。两个代价
    // 都比它想救的场景（旧版在两次导入之间还在写数据）大，交给指纹守卫 + 本条留档。
    // 注意 `imported` 计数与 `records_by_year` 报的是**本次暂存命中数**，重聚合
    // 不区分新旧 —— 界面上"导入 N 条"偏大是这条局限的可见症状。

    // 暂存表按需创建：确认源库确实有明细才建，避免在目标库留下空表 + 唯一索引
    // （运行期不写它，无条件建表会让每个年度库白占 2~4 页）
    connection::ensure_staging_table(&dst_conn)?;

    // 源库全部明细一次读出（旧库是逐条按键，量级几十万行，读完即关连接）
    let rows: Vec<(String, i64)> = {
        let mut stmt = src_conn.prepare("SELECT key_name, timestamp FROM key_log")?;
        let list = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        list
    };

    // 幂等：唯一索引 (key_name, timestamp) 去重，重复行直接忽略。
    // 旧写法是逐行 `INSERT ... WHERE NOT EXISTS(...)`：一次索引查找变两次，
    // 且两个单列索引无法一次性定位。
    dst_conn.execute("BEGIN IMMEDIATE;", [])?;
    let mut imported: i64 = 0;
    {
        let mut insert = dst_conn
            .prepare("INSERT OR IGNORE INTO key_log (key_name, timestamp) VALUES (?1, ?2)")?;
        for (key, ts) in &rows {
            if insert.execute(rusqlite::params![key, ts])? > 0 {
                imported += 1;
            }
        }
    }
    dst_conn.execute("COMMIT;", [])?;
    drop(dst_conn);

    // **先写标记，再聚合**（原来反过来）。理由不是排版：聚合是**累加式且没有去重键**
    // （`count = count + excluded.count`，见 `migrate_v2`），而"这次导过了"这件事只有
    // `meta` 里那几个键记得。所以两步之间任何一次失败（磁盘满、库被另一个连接占住、
    // 进程被杀）在旧顺序下留下的是「已聚合、未标记」——用户看到"导入失败"去重试，
    // 重试读不到标记 ⇒ 指纹比对失效 ⇒ **同一份明细再聚合第二遍，那一年整体翻倍**，
    // 而界面没有任何提示。
    // 反过来「已标记、未聚合」是可自愈的：明细还在目标库的 `key_log` 里，
    // 而 `Database::init` 每次启动都会跑一遍 `migrate_v2`（见 db/mod.rs），
    // 下一次启动就把它聚合掉。两种半截状态里只有一个会毁数据，所以把另一个留下。
    record_import_marker(&dst_path, &src_path, Some(kl_now))?;
    // 聚合落库并压缩（幂等：key_log 迁移后清空）
    crate::db::maintenance::migrate_v2();

    Ok(imported)
}

/// 读一份「date_key → 活跃秒数」（`sql` 只接受本模块内的字面量）。
fn read_day_seconds(conn: &Connection, sql: &str) -> anyhow::Result<Vec<(i64, i64)>> {
    let mut stmt = conn.prepare(sql)?;
    let list = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<Result<_, _>>()?;
    Ok(list)
}

/// 「旧库那段每天活跃时长存在哪儿」的四种结局。
#[derive(Debug, PartialEq)]
enum SecondsSource {
    /// 旧版的独立表
    ActiveTable,
    /// 2026-09-21 之后并进的 `daily_counts.seconds`
    DailyColumn,
    /// 两问都答得上来，两处都确实没有 ⇒ 真的没东西可搬
    Neither,
    /// 有一问没答上来（坏页、schema 读不出、被别的连接挡着）⇒ 不许当"没有"
    CannotTell(String),
}

/// 把上面那两问汇成一件**不经系统调用就能钉死**的判断。
///
/// 顺序照旧：有 `active_seconds` 就用它，否则看 `daily_counts` 有没有 `seconds` 那一列。
/// 新增的只有第四格：任何一问读不出都不许折成「没有」—— 折成没有就等于把这一年说成
/// "旧版本来没记时长"，而调用方随后还要写导入标记，下次重导被大小+mtime 与内容指纹一路跳过。
fn pick_active_seconds_source(
    active: connection::Presence,
    daily_column: connection::Presence,
) -> SecondsSource {
    use crate::db::connection::Presence;
    if let Presence::Unreadable(reason) = &active {
        return SecondsSource::CannotTell(format!(
            "读不出旧库里有没有 active_seconds 这张表: {reason}"
        ));
    }
    if active == Presence::Yes {
        return SecondsSource::ActiveTable;
    }
    match daily_column {
        Presence::Yes => SecondsSource::DailyColumn,
        Presence::No => SecondsSource::Neither,
        Presence::Unreadable(reason) => SecondsSource::CannotTell(format!(
            "读不出旧库 daily_counts 里有没有 seconds 那一列: {reason}"
        )),
    }
}

/// 把源库的「每天活跃秒数」并进目标库，返回目标库净增的秒数。
///
/// 合并分支（目标年库已存在）原先只搬 `key_log` 明细，而**明细里没有时长信息**：
/// 活跃时长按天存在独立表 `active_seconds`（旧版）或 `daily_counts.seconds`
/// （2026-09-21 并入后），那条路上没有任何一步读它们。于是"换电脑/换目录"把
/// 旧库并进已有同年库时，按键数上去了、活跃时长却整段留在旧文件里。
///
/// 取 `MAX()` 而不是相加：重复导入不能把一天算成两天；而目标库某天是新版本
/// 自己采集的，它的时长就是真值，不能被旧库那天的 0 覆盖（旧版早期没记时长）。
fn merge_active_seconds_from_src(src: &Connection, dst: &Connection) -> anyhow::Result<i64> {
    // 两问原先走 `table_exists` / `column_exists` —— 那两者就是把"这次读不出来"折成 `false`
    // 的判据（`table_exists` 字面是 `matches!(table_presence(..), Yes)`）。坏页、被截断的
    // schema、别的连接正挡着查询的那一轮，全走到"没东西可搬 ⇒ `Ok(0)`"那句。
    let chosen = pick_active_seconds_source(
        connection::table_presence(src, "active_seconds"),
        connection::column_presence(src, "daily_counts", "seconds"),
    );
    let sql = match &chosen {
        SecondsSource::ActiveTable => "SELECT date_key, seconds FROM active_seconds",
        SecondsSource::DailyColumn => "SELECT date_key, seconds FROM daily_counts",
        SecondsSource::Neither => return Ok(0),
        SecondsSource::CannotTell(reason) => {
            return Err(anyhow::anyhow!(
                "{reason}：这一年先跳过，不写导入标记，旧库里那段时长下次重导还能补"
            ));
        }
    };
    let rows: Vec<(i64, i64)> = read_day_seconds(src, sql)?;
    if rows.iter().all(|(_, s)| *s <= 0) {
        return Ok(0);
    }

    let before: i64 = dst.query_row(
        "SELECT COALESCE(SUM(seconds), 0) FROM daily_counts",
        [],
        |r| r.get(0),
    )?;
    for (date_key, seconds) in rows.iter().filter(|(_, s)| *s > 0) {
        // 补进来的那天可能一个按键记录都没有（时长口径与按键口径分别上线过）：
        // count 给 0，与 `merge_active_seconds_into_daily` 的既有口径一致。
        dst.execute(
            "INSERT INTO daily_counts (date_key, count, seconds) VALUES (?1, 0, ?2)
             ON CONFLICT(date_key)
             DO UPDATE SET seconds = MAX(daily_counts.seconds, excluded.seconds)",
            rusqlite::params![date_key, seconds],
        )?;
    }
    let after: i64 = dst.query_row(
        "SELECT COALESCE(SUM(seconds), 0) FROM daily_counts",
        [],
        |r| r.get(0),
    )?;
    let gained = after - before;
    if gained > 0 {
        tracing::info!("从旧库并入活跃时长 {gained} 秒");
    }
    Ok(gained)
}

/// 源库 key_log 的内容指纹：(行数, 最大时间戳)。**确认**没有这张表才返回 None。
///
/// 行数 + 最大时间戳足以分辨"这份旧库的明细变没变"：key_log 是按键发生时
/// 逐条追加的，内容没变时两者恒定，摸一摸 mtime 不会动它们。反推不成立
/// （两份不同内容可能撞上同一对数）不碍事 —— 误判的代价只是少跳过一次
/// 幂等的导入，而**不是**漏导或翻倍。
///
/// 「查不动」不能折成「没有这张表」：那条路上调用方说的是"源库只剩聚合表、按键明细本来就没有"，
/// 而真实情况是这一问没答上（坏页、schema 被截断、别的连接正挡着查询）—— 那一年整段按键数
/// 会被安静地丢下。交回 Err 让本年度跳过、不写标记，下次重导还能补。
fn key_log_fingerprint(conn: &Connection) -> anyhow::Result<Option<(i64, i64)>> {
    let has = conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='key_log'",
        [],
        |_| Ok(()),
    );
    match has {
        Ok(()) => {}
        // 查询跑通了却没有那一行 = 确认没有这张表（明细聚完就丢表，是本仓年度库的常态）
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "读不出旧库里有没有 key_log 这张表: {e} —— 这不是「没有明细」"
            ))
        }
    }
    let row = conn.query_row(
        "SELECT COUNT(*), COALESCE(MAX(timestamp), 0) FROM key_log",
        [],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
    )?;
    Ok(Some(row))
}

/// 记录源文件指纹（大小 + 修改时间 + key_log 内容）到目标库 meta，用于重复导入检测。
///
/// `kl` 由调用方给：合并分支手上已经有 `kl_now`，整库复制那一支的目标库就是源文件的
/// 逐字节副本、连接也已经开着 —— 两处都不该再去开第三次。旧写法在这里
/// `Connection::open(src_path).ok().and_then(...)` 另起一次读，失败与"源库没有明细表"
/// 折成同一种结局（两个键都不写），而读取侧"缺任一个键都按没有指纹处理" ⇒
/// 一次占用/同步盘抖动就把 `reimporting_unchanged_key_log_after_mtime_touch_
/// does_not_double_count` 那道守卫整个卸掉，那一年会被再聚一遍而界面无提示。
/// 现在读不动会真报错，不会静默降级成"没有指纹"。
fn record_import_marker(
    dst_path: &Path,
    src_path: &Path,
    kl: Option<(i64, i64)>,
) -> anyhow::Result<()> {
    let meta = std::fs::metadata(src_path)?;
    let conn = connection::open_rw(dst_path)?;
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('imported_src_size', ?1)",
        [meta.len().to_string()],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('imported_src_mtime', ?1)",
        [meta
            .modified()
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs().to_string())
            .unwrap_or_default()],
    )?;
    // key_log 内容指纹：内容没变、只有 mtime 变的再导入要靠它跳过（见
    // import_year_db 的内容比对）。源库没有 key_log 时不写 —— 读取侧两个键
    // 缺任一个都按"没有指纹"处理，与旧版本留下的标记兼容。
    if let Some(kl) = kl {
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('imported_src_kl_count', ?1)",
            [kl.0.to_string()],
        )?;
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('imported_src_kl_maxts', ?1)",
            [kl.1.to_string()],
        )?;
    }
    Ok(())
}

/// 覆盖式复制失败之后的回滚：删掉刚复制过来的那半套，把留档那份原样搬回正名。
///
/// 为什么必须回滚：`backup_before_overwrite` 已经把旧库连 sidecar 一起 rename 走了，
/// 失败那一刻目录里躺着的是"新库缺一段 WAL"这种中间态，而**下一次导入会把它当成
/// "目标库已存在"走合并分支**，读的就是这份残缺的新库。回滚之后状态与导入前一致，
/// 那句"请重试"才真的成立。回滚成功时把 `backed_up_aux` 里那条撤掉 —— 那份文件已经
/// 回到正名上，再报"给你留了一份备份"就是让人去删一个不存在的文件。
///
/// 顺序是这份报告能不能说实话的关键：**先把主库 rename 回去，再搬 sidecar**。
/// 旧顺序反过来（先搬 `-wal`/`-shm`，最后才 rename 主库），于是主库那一步失败时
/// 留档已经只剩一个光杆主库 —— 而那句错误提示写的是「导入前那份**完整**留在 …，
/// 请手工改回原名」，照着做等于把主库与它的 WAL 拆开：正是 `backup_before_overwrite`
/// 的注释禁止的那个状态（"留在原位等于让下次打开按错配的 WAL 去恢复"）。
fn restore_after_failed_copy(
    summary: &mut ImportSummary,
    dst: &Path,
    kept: &Option<std::path::PathBuf>,
) {
    // 半套新库删不掉也要说出来：它留在原地，下一次导入就会当"目标库已存在"去读它。
    // 判断用这一次删除自己的回音，不再回头 `exists()` 问（问不出＝会被读成"没有"⇒ 沉默）。
    let mut leftover: Vec<String> = Vec::new();
    if let Err(e) = std::fs::remove_file(dst) {
        if !crate::data_location::is_confirmed_absent(&e) {
            leftover.push(format!("删不掉残缺的 {}: {e}", dst.display()));
        }
    }
    for suffix in ["-wal", "-shm"] {
        let half = crate::data_location::with_suffix(dst, suffix);
        if let Err(e) = std::fs::remove_file(&half) {
            if !crate::data_location::is_confirmed_absent(&e) {
                leftover.push(format!("删不掉残缺的 {}: {e}", half.display()));
            }
        }
    }
    let Some(kept_path) = kept else {
        summary.errors.extend(leftover);
        return;
    };
    // 主库先回位：这一步成了，留档那一侧才允许被动
    let main_back = match std::fs::rename(kept_path, dst) {
        Ok(()) => true,
        Err(e) => {
            summary.errors.push(format!(
                "回滚失败（{e}）：导入前那一份还留在 {}，本次未导入 —— 它旁边的 -wal/-shm \
                 没有动过，仍然在一起；先让这两个名字都能用（残缺的那份如果删不掉，下面会\
                 单独说），再手工把它改回原名",
                kept_path.display()
            ));
            false
        }
    };
    if main_back {
        let kept_text = kept_path.display().to_string();
        if let Some(i) = summary.backed_up_aux.iter().position(|p| *p == kept_text) {
            summary.backed_up_aux.remove(i);
        }
        summary.errors.push(format!(
            "已回滚到导入前的那份 {}，本次未导入",
            dst.display()
        ));
        // sidecar 跟着回位；搬不动的那一个必须点名 —— 只搬回主库等于交回一个缺 WAL 尾巴的库
        for suffix in ["-wal", "-shm"] {
            let from = crate::data_location::with_suffix(kept_path, suffix);
            let to = crate::data_location::with_suffix(dst, suffix);
            // 这一问由改名自己回答，不再回头 `exists()` 猜。旧写法 `if !from.exists() { continue }`
            // 把"这一刻问不出"读成"没有"⇒ 主库回了位、那段未 checkpoint 的尾巴却留在留档名
            // 旁边，交回的正是上面那句注释禁止的状态。`NotFound` 才是"确实没有要搬的"。
            match std::fs::rename(&from, &to) {
                Ok(()) => {}
                Err(e) if crate::data_location::is_confirmed_absent(&e) => {}
                Err(e) => leftover.push(format!(
                    "{} 搬不回 {} 旁边（{e}）：那一段未 checkpoint 的记录还留在原地，\
                     手工处理时要把它一起挪",
                    from.display(),
                    dst.display(),
                )),
            }
        }
    }
    summary.errors.extend(leftover);
}

/// 导入标记：源文件指纹 + 上次导入时源库 key_log 的内容指纹。
struct ImportMarker {
    size: u64,
    mtime: Option<std::time::SystemTime>,
    /// (行数, 最大时间戳)；旧版本留下的标记没有这两个键 → None。
    key_log: Option<(i64, i64)>,
}

/// 读取导入标记。
fn read_import_marker(dst_path: &Path) -> anyhow::Result<Option<ImportMarker>> {
    let conn = connection::open_ro(dst_path)?;
    let size: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'imported_src_size'",
            [],
            |r| r.get(0),
        )
        .ok();
    let mtime: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'imported_src_mtime'",
            [],
            |r| r.get(0),
        )
        .ok();
    let kl_count: Option<i64> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'imported_src_kl_count'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|v| v.parse().ok());
    let kl_maxts: Option<i64> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'imported_src_kl_maxts'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|v| v.parse().ok());
    let Some(sz) = size else {
        return Ok(None);
    };
    Ok(Some(ImportMarker {
        size: sz.parse::<u64>().unwrap_or(0),
        mtime: mtime
            .and_then(|mt| mt.parse::<u64>().ok())
            .map(|secs| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs)),
        key_log: kl_count.zip(kl_maxts),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 现有库**问不出在不在**时，覆盖前留档那一步必须停手，不许当成"没有要留档的东西"。
    ///
    /// 旧写法 `if !dst.exists() { return Ok(None); }`：那份库正被别的程序握着（杀软首扫、
    /// 同步盘占位、移动盘休眠）时这一问照样回 false ⇒ 调用方拿着"不用留档"直接往下走
    /// `fs::copy` ⇒ 当前那份记账/日程库被整份覆盖，一条退路都没留。
    #[cfg(windows)]
    #[test]
    fn an_unanswerable_existing_aux_db_is_not_treated_as_absent() {
        use std::os::windows::fs::OpenOptionsExt;
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("import_aux_guard");
        let dst = crate::paths::data_dir().join("accounting.db");
        std::fs::write(&dst, "我是要紧的数据".as_bytes()).expect("写现有库");
        let _holder = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&dst)
            .expect("占位句柄应能打开");
        assert!(
            std::fs::File::open(&dst).is_err(),
            "夹具没成立：这个句柄没挡住读，测不到「问不出」那一支"
        );
        let err = backup_before_overwrite(&dst).expect_err("问不出时不许交回「不用留档」");
        assert!(
            err.to_string().contains("说不出这份现有库在不在"),
            "要说清是哪一问没答上: {err}"
        );
        // 撤掉占用 ⇒ 库还在原位、内容一字没动，这一次才照常留档
        drop(_holder);
        assert_eq!(
            std::fs::read(&dst).expect("现有库必须还在"),
            "我是要紧的数据".as_bytes(),
            "被拒绝的那一步绝不能碰原文件"
        );
        assert!(
            backup_before_overwrite(&dst)
                .expect("读得动时照常留档")
                .is_some(),
            "有现有库就该留下一份"
        );
    }

    /// 目标年度库问不出 ⇒ 既不整库覆盖、也不当成空库走合并（这一支原先一份留档都不做）。
    #[cfg(windows)]
    #[test]
    fn an_unanswerable_target_year_db_is_not_copied_over() {
        use std::os::windows::fs::OpenOptionsExt;
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("import_year_guard");
        let year = 2025;
        let src_dir = crate::paths::app_dir().join("import_src");
        std::fs::create_dir_all(&src_dir).expect("建源目录");
        std::fs::write(
            src_dir.join(format!("focusflow_{year}.db")),
            "别人的库".as_bytes(),
        )
        .expect("写源库");
        let dst = paths::year_db_path(year);
        std::fs::write(&dst, "我这一年还在用".as_bytes()).expect("写目标年度库");
        let _holder = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&dst)
            .expect("占位句柄应能打开");

        let mut summary = ImportSummary::default();
        let err = import_year_db(&src_dir, year, &mut summary)
            .expect_err("问不出时本年度必须跳过，不能整库覆盖");
        assert!(
            err.to_string().contains("说不出 2025 年目标库在不在"),
            "要说清是哪一问没答上: {err}"
        );
        assert!(
            summary.errors.is_empty(),
            "报错仍由调用方记账，这里不重复塞"
        );

        drop(_holder);
        assert_eq!(
            std::fs::read(&dst).expect("现有年度库必须原样留着"),
            "我这一年还在用".as_bytes(),
            "这一支不留档，覆盖一次就再也找不回来"
        );
    }

    /// 导入把新的一年搬进来之后，**同一拍**就要能看见那一年 —— 不许等 30 秒 TTL 自己到。
    ///
    /// 这条**不是缺陷登记，是接线腿**：负责兑现它的不是导入本身，而是
    /// `import_year_db` 两条分支（整库复制那一支与合并那一支）末尾都调的
    /// `maintenance::migrate_v2()` —— 失效写在它的尾部（`maintenance.rs` 里那句
    /// `queries::invalidate_years_cache()`）。我把这条腿立起来的直接原因很实在：
    /// 我先按"导入链路一条失效都没调"下了缺陷判断，只在 `migration.rs` 里 grep 了一次；
    /// 结果这条判据在**摘掉我自己加的那句失效**之后照样绿 —— 是间接链早就把它办了。
    /// 所以这条钉的是"谁真的在负责"：哪天有人把 `migrate_v2` 从导入里摘出去、或把那句
    /// 失效挪走，这里立刻红，而不是留下"回执说了、屏上没改"那半分钟的假账。
    ///
    /// 前置条件是**造**出来的、不是碰巧：先用一次 `available_years()` 把"只有今年"那份
    /// 名单灌进缓存（用例全程在 30 秒内跑完），再导入，再问一次。少了第一步，TTL 可能
    /// 早就过期、这条腿不修也会绿。
    #[test]
    fn an_imported_year_is_visible_without_waiting_for_the_ttl() {
        use chrono::Datelike;
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("import_year_visible");
        let this_year = chrono::Local::now().year();
        let prev = this_year - 1;
        // 旧目录里那一份"上一年"的库：照 schema 建出来并放进行数据，
        // 否则 `available_years()` 会把它当空壳滤掉，判据就红不到 TTL 这一层上。
        let src_dir = crate::paths::app_dir().join("import_src");
        std::fs::create_dir_all(&src_dir).expect("建源目录");
        let src_db = src_dir.join(format!("focusflow_{prev}.db"));
        {
            let conn = crate::db::connection::open_rw(&src_db).expect("造旧库");
            crate::db::connection::ensure_schema(&conn, prev).expect("建旧库结构");
            conn.execute(
                "INSERT INTO daily_counts (date_key, count, seconds) VALUES (?1, 40, 60)",
                [crate::db::queries::day_key_of_date(
                    chrono::NaiveDate::from_ymd_opt(prev, 6, 1).unwrap(),
                )],
            )
            .expect("放一行数据");
        }
        // 当前年也要有一份带数据的库，好让"预热"那一步读出来的是【只含今年】的名单
        {
            let conn = crate::db::connection::open_rw(&crate::paths::year_db_path(this_year))
                .expect("建今年");
            crate::db::connection::ensure_schema(&conn, this_year).expect("建今年结构");
            conn.execute(
                "INSERT INTO daily_counts (date_key, count, seconds) VALUES (?1, 7, 10)",
                [crate::db::queries::day_key_of_date(
                    chrono::Local::now().date_naive(),
                )],
            )
            .expect("今年放一行");
        }
        assert!(
            !crate::paths::year_db_path(prev).exists(),
            "夹具前提：上一年在导入前根本不存在（走的是整库 `fs::copy` 那一支）"
        );
        // 预热：把"只有今年"这份名单灌进 30 秒缓存
        crate::db::queries::invalidate_years_cache();
        let before = crate::db::queries::available_years();
        assert_eq!(
            before,
            vec![this_year],
            "夹具前提：预热读到的必须是【只有今年】，否则这条判据测不到失效"
        );

        let summary = import_legacy_data(&src_dir);
        assert!(
            summary.year_dbs.contains(&prev),
            "导入回执说有上一年，实得 {:?}（错误：{:?}）",
            summary.year_dbs,
            summary.errors
        );

        let after = crate::db::queries::available_years();
        assert!(
            after.contains(&prev),
            "新的一年要在**同一次运行里**立刻可见（现在还在 TTL 里 ⇒ 屏上继续只念 {before:?}），实得 {after:?}"
        );
    }

    /// 源 `-wal` **问不出在不在**时不许静默跳过：年度库这一支随后要写导入标记，
    /// 缺的那段尾巴就永久拿不回来了（与"搬不动"那一支汇成同一个出口）。
    #[cfg(windows)]
    #[test]
    fn an_unanswerable_source_wal_stops_the_year_import() {
        use std::os::windows::fs::OpenOptionsExt;
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("import_src_wal");
        let year = 2025;
        let src_dir = _dir.path().join("src_year");
        std::fs::create_dir_all(&src_dir).expect("建源目录");
        let src = src_dir.join(format!("focusflow_{year}.db"));
        std::fs::write(&src, "main").expect("写源主库");
        let src_wal = crate::data_location::with_suffix(&src, "-wal");
        std::fs::write(&src_wal, "tail").expect("写源 WAL");
        let _holder = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&src_wal)
            .expect("占位句柄应能打开");
        assert!(
            std::fs::File::open(&src_wal).is_err(),
            "夹具没成立：这个句柄没挡住读，测不到「问不出」那一支"
        );

        let mut summary = ImportSummary::default();
        let err = import_year_db(&src_dir, year, &mut summary)
            .expect_err("源 WAL 问不出时必须停下，不许照报已导入");
        let text = err.to_string();
        assert!(
            text.contains("WAL") && text.contains("问不出"),
            "要说清缺的是那段尾巴、以及是哪一问没答上: {text}"
        );
        // 半份目标库要回滚掉：留着它，下次导入改走合并分支、读的正是缺尾巴的那份
        assert!(
            !crate::paths::year_db_path(year).exists(),
            "整库复制的半成品必须回滚，否则下次换分支读半份"
        );

        drop(_holder);
        assert_eq!(
            std::fs::read(&src_wal).expect("源 WAL 要原样留在旧目录"),
            "tail".as_bytes(),
            "这一步不该碰源文件"
        );
    }

    /// 附属库那一支同理：问不出要进 `errors`，不许只 push `copied_aux`（界面那句"已复制"）。
    #[cfg(windows)]
    #[test]
    fn an_unanswerable_source_wal_is_not_reported_as_copied_for_aux_dbs() {
        use std::os::windows::fs::OpenOptionsExt;
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("import_aux_src_wal");
        let src_dir = _dir.path().join("src_aux");
        std::fs::create_dir_all(&src_dir).expect("建源目录");
        let src = src_dir.join("focusflow_accounting.db");
        std::fs::write(&src, "main").expect("写源附属库");
        let src_wal = crate::data_location::with_suffix(&src, "-wal");
        std::fs::write(&src_wal, "tail").expect("写源 WAL");
        let _holder = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&src_wal)
            .expect("占位句柄应能打开");
        assert!(
            std::fs::File::open(&src_wal).is_err(),
            "夹具没成立：这个句柄没挡住读，测不到「问不出」那一支"
        );

        let summary = import_legacy_data(&src_dir);
        assert!(
            summary
                .errors
                .iter()
                .any(|e| e.contains("WAL") && e.contains("问不出")),
            "要说清是哪一段没搬来、哪一问没答上: {:?}",
            summary.errors
        );
        assert!(
            !summary.copied_aux.iter().any(|a| a.contains("accounting")),
            "问不出的那一支不许算成已复制: {:?}",
            summary.copied_aux
        );
    }

    /// 旧目录里那份**主库**问不出时要点头名，不许当成「旧目录里没有这一份」而照报完成。
    ///
    /// 用户随后清掉旧目录，那份番茄钟/日程数据就没了 —— 而 `Path::exists()` 在属性问不出的
    /// 那一刻回的就是 false。对照腿钉住"撤掉占用就照常导入"，防修法做成"握着就永远导不了"。
    #[cfg(windows)]
    #[test]
    fn an_unanswerable_aux_source_is_said_out_loud_not_skipped_silently() {
        use std::os::windows::fs::OpenOptionsExt;
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("import_aux_src_main");
        let src_dir = _dir.path().join("src_main");
        std::fs::create_dir_all(&src_dir).expect("建源目录");
        let src = src_dir.join("focusflow_pomodoro.db");
        std::fs::write(&src, "main").expect("写源库");
        let _holder = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&src)
            .expect("占位句柄应能打开");
        assert!(
            std::fs::File::open(&src).is_err(),
            "夹具没成立：这个句柄没挡住读，测不到「问不出」那一支"
        );

        let summary = import_legacy_data(&src_dir);
        assert!(
            summary
                .errors
                .iter()
                .any(|e| e.contains("pomodoro") && e.contains("问不出")),
            "被握着的那一份要点头名: {:?}",
            summary.errors
        );
        assert!(
            !summary.copied_aux.iter().any(|a| a.contains("pomodoro")),
            "没搬成就不许算已复制: {:?}",
            summary.copied_aux
        );

        drop(_holder);
        let again = import_legacy_data(&src_dir);
        assert!(
            again.copied_aux.iter().any(|a| a.contains("pomodoro")),
            "撤掉占用后这一份要照常导入，实得 {:?}",
            again
        );
    }

    /// 接线判据：导入链那几问都走同一把尺子，且"删完/搬完再问一遍在不在"这种写法不许回来。
    ///
    /// 后半句是本轮补的：原先只禁 `"if from.exists()"` 这一个串，而回滚那一问写的是
    /// `if !from.exists()` —— 多了两个字符就抓不到，于是"第四处 sidecar 那一问"从来没上
    /// 尺子，判据却以为整条链都上了（`probe_open(&from)` 钉在 3，现场也正好是 3）。
    #[test]
    fn the_import_chain_asks_its_source_files_with_one_ruler() {
        let text = include_str!("migration.rs");
        let prod = &text[..text.find("#[cfg(test)]").expect("测试模块的起点找不到了")];
        let code = prod
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            code.matches("probe_open(&from)").count(),
            3,
            "三处 sidecar 那一问（留档、附属库复制、年度库复制）都要走三态尺子"
        );
        for shape in [
            "probe_open(&src)",
            "probe_open(dst)",
            "probe_open(&dst_path)",
        ] {
            assert!(code.contains(shape), "那一问要走共用的尺子，少了 {shape}");
        }
        // "残留删没删掉"与"sidecar 搬没搬走"这四问由**那一次 syscall 自己的错误**回答，
        // 口径与上面同一把尺子：只有 `NotFound` 算"确实没有"。
        assert_eq!(
            code.matches("is_confirmed_absent(&e)").count(),
            4,
            "四问（整体复制回滚 1 处、覆盖回滚主库 1 处 + sidecar 2 处、搬回留档 1 处里的\
             删除与改名）都该按错误分类，实得 {}",
            code.matches("is_confirmed_absent(&e)").count()
        );
        for forbidden in [
            "if from.exists()",
            "if !from.exists()",
            "if !src.exists()",
            "if !dst.exists()",
            "if dst.exists()",
            "if half.exists()",
            "if path.exists()",
        ] {
            assert!(
                !code.contains(forbidden),
                "那一问不许退回 exists() 猜：{forbidden}"
            );
        }
    }

    /// 旧库**读不出来**时不许说成"这库里没记过活跃时长"。
    ///
    /// `table_exists` / `column_exists` 都把"这次查不动"折成 `false`，于是坏页、schema 被截断、
    /// 别的连接挡着查询那几轮，全走到"没东西可搬 ⇒ `Ok(0)`"，而调用方随后写标记、报告说的是
    /// "活跃时长已并" —— 那一年在旧文件里的那段时长就这么掉了。
    #[test]
    fn an_unreadable_source_is_not_reported_as_having_no_active_seconds() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("merge_active_unreadable");
        let dst = Connection::open(_dir.path().join("dst.db")).expect("开目标库");
        dst.execute_batch(
            "CREATE TABLE daily_counts(date_key INTEGER PRIMARY KEY, count INTEGER, seconds INTEGER);",
        )
        .expect("建目标表");

        // ① 坏页（头部不是 sqlite 那十六个字节）⇒ 要说"读不出"，不许说"没记过"
        let junk = _dir.path().join("src_junk.db");
        std::fs::write(&junk, "这看着就不是一个 sqlite 库").expect("写坏库");
        let src = Connection::open(&junk).expect("坏库开连接本身该成功，失败在第一次查询");
        let err = merge_active_seconds_from_src(&src, &dst)
            .expect_err("读不出时必须交回错误，不能当成「没有时长」");
        assert!(
            err.to_string()
                .contains("读不出旧库里有没有 active_seconds"),
            "要说清是哪一问没答上: {err}"
        );

        // ② 对照腿：问得出、里面确实没有 ⇒ 仍是 Ok(0)（最早的旧版根本没记时长，该安静）
        let src2 = Connection::open(_dir.path().join("src_none.db")).expect("开一个干净的空库");
        src2.execute_batch("CREATE TABLE t(x);")
            .expect("建一张无关表");
        assert_eq!(
            merge_active_seconds_from_src(&src2, &dst).expect("问得出就没有就是没有"),
            0,
            "确认没有时长与读不出是两件事"
        );
        let total: i64 = dst
            .query_row(
                "SELECT COALESCE(SUM(seconds),0) FROM daily_counts",
                [],
                |r| r.get(0),
            )
            .expect("读目标库");
        assert_eq!(total, 0, "被拒绝的那一步不该写目标库");
    }

    /// 上面那把换算表不经系统调用就能钉死 —— 四格各有断言，顺序与"读不出优先"都要按住。
    #[test]
    fn the_active_seconds_lookup_table_has_four_answers_not_two() {
        use crate::db::connection::Presence as P;
        let why = |s: SecondsSource| match s {
            SecondsSource::CannotTell(reason) => reason,
            other => panic!("这一格该交「读不出」，实得 {other:?}"),
        };
        // ① 有独立表：第二问什么都不能改结论（顺序照旧）
        assert_eq!(
            pick_active_seconds_source(P::Yes, P::No),
            SecondsSource::ActiveTable
        );
        assert_eq!(
            pick_active_seconds_source(P::Yes, P::Unreadable("x".into())),
            SecondsSource::ActiveTable,
            "第一问答成「有表」时不该被第二问拖走"
        );
        // ② 没有独立表、有那一列
        assert_eq!(
            pick_active_seconds_source(P::No, P::Yes),
            SecondsSource::DailyColumn
        );
        // ③ 两问都答得上来、两处都确实没有 ⇒ 才是"没东西可搬"
        assert_eq!(
            pick_active_seconds_source(P::No, P::No),
            SecondsSource::Neither
        );
        // ④ 任何一问读不出 ⇒ CannotTell，且点名是哪一问
        assert!(why(pick_active_seconds_source(
            P::Unreadable("坏页".into()),
            P::No
        ))
        .contains("active_seconds"));
        assert!(why(pick_active_seconds_source(
            P::No,
            P::Unreadable("被挡".into())
        ))
        .contains("seconds 那一列"));
        // 第一问读不出时，第二问答什么都不能把它洗成"有/没有"
        assert!(why(pick_active_seconds_source(
            P::Unreadable("坏页".into()),
            P::Yes
        ))
        .contains("active_seconds"));
    }

    /// key_log 那一问同样分得开「确认没有这张表」与「这次读不出」。
    ///
    /// 旧的 `has.is_err() ⇒ Ok(None)` 把两者并成一件，于是坏库会被上游报成「源库只剩聚合表：
    /// 按键明细本来就没有」—— 那一年整段按键数被安静地丢下，而话说得像查过了。
    #[test]
    fn key_log_fingerprint_separates_no_table_from_cannot_read() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("kl_probe");
        // ① 问得出、确实没有这张表 ⇒ Ok(None)（本仓年度库的常态，那句"只剩聚合表"是对的）
        let conn = Connection::open(_dir.path().join("clean.db")).expect("开干净库");
        conn.execute_batch(
            "CREATE TABLE daily_counts(date_key INTEGER PRIMARY KEY, count INTEGER);",
        )
        .expect("建聚合表");
        assert_eq!(key_log_fingerprint(&conn).expect("问得出"), None);
        drop(conn);

        // ② 读不出 ⇒ 交回错误，不许折成「没有明细」
        let junk = _dir.path().join("junk.db");
        std::fs::write(&junk, "junk bytes").expect("写坏库");
        let src = Connection::open(&junk).expect("坏库开连接本身成功");
        let err = key_log_fingerprint(&src).expect_err("读不出时不许折成 None");
        assert!(
            err.to_string().contains("读不出旧库里有没有 key_log"),
            "要说清是哪一问没答上: {err}"
        );
    }

    /// 端到端：坏源库年度那一条要报错、**不许写导入标记**（写了以后重导被指纹一路跳过）。
    #[test]
    fn a_corrupt_source_year_db_is_skipped_without_writing_the_import_marker() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("merge_corrupt_src");
        let year = 2025;
        let src_dir = _dir.path().join("src_dir");
        std::fs::create_dir_all(&src_dir).expect("建源目录");
        std::fs::write(src_dir.join(format!("focusflow_{year}.db")), "坏掉的旧库")
            .expect("写坏源库");
        {
            let dst = paths::year_db_path(year);
            let conn = connection::open_rw(&dst).expect("建目标年度库");
            connection::ensure_schema(&conn, year).expect("建表");
            drop(conn);
        }

        let mut summary = ImportSummary::default();
        let err = import_year_db(&src_dir, year, &mut summary)
            .expect_err("读不出的那一年必须报错，不能报成「只剩聚合表」");
        assert!(
            err.to_string().contains("读不出"),
            "要说清是哪一问没答上: {err}"
        );
        assert!(
            summary.skipped.is_empty(),
            "不许把读不出说成「本来就没明细」: {:?}",
            summary.skipped
        );
        assert!(
            read_import_marker(&paths::year_db_path(year))
                .expect("目标库读得动")
                .is_none(),
            "标记没写才谈得上下次重导还能补"
        );
    }

    /// 把**当前**数据目录当成导入源（目录选择器里挑到自己 data/ 上很常见）：
    /// 附属库必须原地不动。
    ///
    /// 修之前留档那步 rename 会把"源文件"一起搬走，紧接着的 copy 找不到源而失败 ——
    /// 附属库从正名上消失，应用下次打开时建一个空库，表现就是"我的账记数据没了"。
    #[test]
    fn importing_the_current_data_dir_keeps_aux_dbs_in_place() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("selfimport");
        let data = crate::paths::data_dir();
        let aux = data.join("focusflow_accounting.db");
        std::fs::write(&aux, b"not a real db, but it must survive").unwrap();

        let summary = import_legacy_data(&data);

        assert!(
            aux.is_file(),
            "自导入把附属库从正名上弄丢了：{:?}",
            summary.errors
        );
        assert!(
            !summary.errors.iter().any(|e| e.contains("复制失败")),
            "不该出现源文件已被搬走导致的复制失败：{:?}",
            summary.errors
        );
        assert!(
            summary.skipped.iter().any(|s| s.contains("同一个文件")),
            "应说明为什么跳过：{:?}",
            summary.skipped
        );
        let leftovers: Vec<_> = std::fs::read_dir(&data)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".import-backup-"))
            .collect();
        assert!(leftovers.is_empty(), "不该留下留档文件：{leftovers:?}");
    }

    /// 年度库整体复制时 `-wal` 复制失败**不许被吞掉**。
    ///
    /// 旧写法是 `let _ = std::fs::copy(..)`：主库复制成功、边文件失败（杀软/索引器占住、
    /// 磁盘满、同步盘把它换成占位文件），而 `-wal` 里正是**还没 checkpoint 进主库**的那段
    /// 新记录。吞掉之后流程照走 —— 报"导入 N 条"、并写下 `imported_src_*` 标记，
    /// 而重导会被大小+mtime / 内容指纹比对跳过，那一年最新的一段就永久拿不回来了。
    #[test]
    fn failed_wal_copy_is_not_reported_as_imported() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("mig_wal_fail");
        let data = crate::paths::data_dir();
        let name = "focusflow_2031.db".to_string();

        // 源：一个真年度库 + 一个带尾巴的 -wal
        let src_dir = _dir.path().join("old");
        std::fs::create_dir_all(&src_dir).unwrap();
        let src = src_dir.join(&name);
        {
            let conn = Connection::open(&src).unwrap();
            conn.execute_batch(
                "CREATE TABLE daily_counts (
                    date_key INTEGER PRIMARY KEY,
                    count INTEGER NOT NULL,
                    seconds INTEGER NOT NULL DEFAULT 0
                 );
                 INSERT INTO daily_counts (date_key, count, seconds) VALUES (20000, 7, 7);",
            )
            .unwrap();
        }
        std::fs::write(format!("{}-wal", src.display()), b"un-checkpointed tail").unwrap();

        // 目标位置的 -wal 先占成一个目录：往它上面 copy 必失败（不需要真杀软）
        std::fs::create_dir_all(format!("{}-wal", data.join(&name).display())).unwrap();

        let summary = import_legacy_data(&src_dir);

        assert!(
            summary.errors.iter().any(|e| e.contains("WAL")),
            "WAL 复制失败必须报给用户，而不是被吞掉：{:?}",
            summary.errors
        );
        assert!(
            summary.year_dbs.is_empty() && summary.records_by_year.is_empty(),
            "没导成就不该报「导入 N 条」：{:?}",
            summary.records_by_year
        );
        assert!(
            !data.join(&name).exists(),
            "半份主库必须回滚：留着它，下次导入会改走合并分支，\
             而它读的正是缺了那段尾巴的副本"
        );
        let shown = summary.errors.join("\n");
        assert!(
            shown.contains("自动清理没成功"),
            "占位的 -wal 是目录、删不掉，必须按「清理失败」报而不是说可以重试: {shown}"
        );
        assert!(
            shown.contains("手动删掉"),
            "删不掉的残留要告诉用户动手清: {shown}"
        );
        assert!(
            !shown.contains("可以直接重试"),
            "残留还在时不许说可以重试: {shown}"
        );
    }

    /// 回滚真清干净时（失败原因不占着目标文件名）要明确说可以重试。
    ///
    /// 源侧的 `-wal` 是目录 ⇒ copy 读源就失败，目标侧没有留下任何删不掉的障碍。
    #[test]
    fn a_clean_rollback_tells_the_user_to_retry() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("mig_rollback_clean");
        let data = crate::paths::data_dir();
        let name = "focusflow_2034.db".to_string();

        let src_dir = _dir.path().join("old");
        std::fs::create_dir_all(&src_dir).unwrap();
        let src = src_dir.join(&name);
        {
            let conn = Connection::open(&src).unwrap();
            conn.execute_batch(
                "CREATE TABLE daily_counts (
                    date_key INTEGER PRIMARY KEY,
                    count INTEGER NOT NULL,
                    seconds INTEGER NOT NULL DEFAULT 0
                 );
                 INSERT INTO daily_counts (date_key, count, seconds) VALUES (20000, 1, 1);",
            )
            .unwrap();
        }
        std::fs::create_dir_all(crate::data_location::with_suffix(&src, "-wal")).unwrap();

        let summary = import_legacy_data(&src_dir);
        let shown = summary.errors.join("\n");
        assert!(!data.join(&name).exists(), "半份主库必须回滚: {shown}");
        assert!(
            shown.contains("可以直接重试"),
            "清干净后要明确说可以重试: {shown}"
        );
        assert!(
            !shown.contains("手动删掉"),
            "没有残留，不该让人去删不存在的文件: {shown}"
        );
    }

    /// 回滚自己失败时不许还说"重试"：半份库留在原地，重试会改走合并分支。
    ///
    /// 删文件失败在真实环境里不可控（杀软握着句柄、只读属性），这里用目录占住
    /// 目标名让 `remove_file` 必失败 —— 测的是"删不掉要进残留清单、话要跟着改"，
    /// 不是那个 IO 错误本身。
    #[test]
    fn a_failed_rollback_is_not_reported_as_retryable() {
        let _lock = crate::paths::test_app_dir_lock();
        let app = crate::paths::test_app_dir("mig_rollback_fail");

        let dst = app.path().join("focusflow_2032.db");
        std::fs::create_dir_all(&dst).unwrap();
        let leftover = rollback_fresh_copy(&dst);
        assert!(
            !leftover.is_empty(),
            "删不掉的主库必须进残留清单，不许 `let _ =` 吞掉"
        );
        assert!(
            leftover[0].contains("focusflow_2032.db"),
            "残留要点名让人能删: {leftover:?}"
        );
        let hint = retry_hint(&leftover);
        assert!(
            !hint.contains("可以直接重试"),
            "残留还在时不许说可以重试: {hint}"
        );
        assert!(
            hint.contains("手动删掉"),
            "要说清怎么恢复，而不是只说失败了: {hint}"
        );

        // sidecar 也要按真后缀拼，且同样收进残留清单
        let dst2 = app.path().join("focusflow_2033.db");
        let side = crate::data_location::with_suffix(&dst2, "-wal");
        std::fs::create_dir_all(&side).unwrap();
        let leftover = rollback_fresh_copy(&dst2);
        assert!(
            leftover.iter().any(|s| s.contains("focusflow_2033.db-wal")),
            "sidecar 删不掉也要点名: {leftover:?}"
        );

        let clean = retry_hint(&[]);
        assert!(clean.contains("可以直接重试"));
        assert!(!clean.contains("手动"));
    }

    /// **重试不得把那一年累加第二遍**：聚合是 `count = count + excluded` 且没有去重键，
    /// 而"这份已经导过了"只记在目标库 `meta` 的 `imported_src_*` 里。
    /// 旧顺序（先聚合、后写标记）在两步之间失败时留下的是「已聚合、未标记」——
    /// 用户看到"导入失败"点重试，重试读不到标记 ⇒ 同一份明细再聚合一遍 ⇒ 那一年翻倍。
    #[test]
    fn retry_after_a_failed_marker_does_not_double_count_the_year() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("mig_retry_double");
        let data = crate::paths::data_dir();
        std::fs::create_dir_all(&data).unwrap();
        let name = "focusflow_2026.db".to_string();

        // 源库：完整结构 + 5 条明细
        let src_dir = _dir.path().join("old");
        std::fs::create_dir_all(&src_dir).unwrap();
        {
            let conn = Connection::open(src_dir.join(&name)).unwrap();
            connection::ensure_schema(&conn, 2026).unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS key_log (key_name TEXT NOT NULL, timestamp INTEGER NOT NULL);
                 INSERT INTO key_log (key_name, timestamp) VALUES
                     ('A', 1770000000), ('B', 1770000001), ('A', 1770000002),
                     ('C', 1770000003), ('A', 1770000004);",
            )
            .unwrap();
        }

        // 目标库先存在（走合并分支），并让**只有标记那几个键**写不下去：
        // 失败点精确落在 record_import_marker 上（现场形状 = 磁盘满 / 库被别的连接占住）
        {
            let conn = connection::open_rw(&data.join(&name)).unwrap();
            connection::ensure_schema(&conn, 2026).unwrap();
            conn.execute_batch(
                "CREATE TRIGGER block_marker BEFORE INSERT ON meta
                     WHEN NEW.key LIKE 'imported_src_%'
                 BEGIN SELECT RAISE(ABORT, 'blocked by test'); END;",
            )
            .unwrap();
        }
        let total_of = || -> i64 {
            let conn = connection::open_ro(&data.join(&name)).unwrap();
            let v = conn
                .query_row(
                    "SELECT COALESCE(SUM(count), 0) FROM daily_counts",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0);
            drop(conn);
            v
        };

        let first = import_legacy_data(&src_dir);
        assert!(
            first.errors.iter().any(|e| e.contains("导入失败")),
            "标记写不下去必须报错，不能当成功：{:?}",
            first.errors
        );
        let after_first = total_of();
        assert_eq!(after_first, 0, "失败的那一次不该已经把聚合数落进库");

        let second = import_legacy_data(&src_dir);
        assert!(
            second.errors.iter().any(|e| e.contains("导入失败")),
            "重试同样报错：{:?}",
            second.errors
        );
        assert_eq!(
            total_of(),
            after_first,
            "重试把那一年又累加了一遍（{after_first} → {}）",
            total_of()
        );
    }

    /// 复制分支的同一对顺序也得钉住：它正好在"整库都搬过来了，标记写不写得上去有什么
    /// 关系"这个直觉盲区里，而它一样要跑 `migrate_v2` 的累加式聚合。
    ///
    /// 触发形状 = 源库自带一个只挡 `imported_src_*` 的触发器（现场对应 meta 所在页写不
    /// 进去：磁盘满、库被另一个连接占住）：复制成功、`ensure_schema` 成功、聚合成功，
    /// 唯独标记写不下去。旧顺序此时已把那一年累加进 `daily_counts`，报错后用户重试 ⇒
    /// 再累加一遍。注回旧顺序（标记写回聚合之后）红在"失败的那一次不该已经落了聚合数"。
    #[test]
    fn copy_branch_marker_failure_does_not_double_count_either() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("mig_copy_double");
        let data = crate::paths::data_dir();
        std::fs::create_dir_all(&data).unwrap();
        let name = "focusflow_2027.db".to_string();

        // 源库：完整结构 + 5 条明细 + 触发器（整库复制会把它一起带过去）
        let src_dir = _dir.path().join("old");
        std::fs::create_dir_all(&src_dir).unwrap();
        {
            let conn = Connection::open(src_dir.join(&name)).unwrap();
            connection::ensure_schema(&conn, 2027).unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS key_log (key_name TEXT NOT NULL, timestamp INTEGER NOT NULL);
                 INSERT INTO key_log (key_name, timestamp) VALUES
                     ('A', 1770000000), ('B', 1770000001), ('A', 1770000002),
                     ('C', 1770000003), ('A', 1770000004);
                 CREATE TRIGGER block_marker BEFORE INSERT ON meta
                     WHEN NEW.key LIKE 'imported_src_%'
                 BEGIN SELECT RAISE(ABORT, 'blocked by test'); END;",
            )
            .unwrap();
        }
        assert!(
            !data.join(&name).exists(),
            "目标库不存在才会走整体复制这一支"
        );

        let total_of = || -> i64 {
            let conn = connection::open_ro(&data.join(&name)).unwrap();
            let v = conn
                .query_row(
                    "SELECT COALESCE(SUM(count), 0) FROM daily_counts",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0);
            drop(conn);
            v
        };

        let first = import_legacy_data(&src_dir);
        assert!(
            first.errors.iter().any(|e| e.contains("导入失败")),
            "标记写不下去必须报错，不能当成功：{:?}",
            first.errors
        );
        let after_first = total_of();
        assert_eq!(after_first, 0, "复制分支失败的那一次不该已经落了聚合数");

        let second = import_legacy_data(&src_dir);
        assert!(
            second.errors.iter().any(|e| e.contains("导入失败")),
            "重试同样报错：{:?}",
            second.errors
        );
        assert_eq!(
            total_of(),
            after_first,
            "重试把那一年又累加了一遍（{after_first} → {}）",
            total_of()
        );
    }

    /// 真正的跨目录导入仍须照旧覆盖并留档（上一条的守卫不能顺手把正常路径也挡掉）。
    #[test]
    fn importing_from_another_dir_still_overwrites_and_keeps_backup() {
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("crossimport");
        let data = crate::paths::data_dir();
        let aux = data.join("focusflow_accounting.db");
        std::fs::write(&aux, b"current").unwrap();

        let src_dir = _dir.path().join("old");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("focusflow_accounting.db"), b"legacy").unwrap();

        let summary = import_legacy_data(&src_dir);

        assert!(
            summary.copied_aux.iter().any(|a| a.contains("accounting")),
            "跨目录导入应照常复制：{:?}",
            summary
        );
        assert_eq!(std::fs::read_to_string(&aux).unwrap(), "legacy");
        let kept: Vec<_> = std::fs::read_dir(&data)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".import-backup-"))
            .collect();
        assert_eq!(kept.len(), 1, "被覆盖的现有库必须留档一份");
    }

    /// 附属文件搬不动时，整次留档必须在**动主库之前**失败。
    ///
    /// 旧写法是「先 rename 主库 → `let _ =` 吞掉 `-wal` 的失败」，于是目录里留下
    /// "主库已改名带走、别人那份 `-wal` 还在原位"这半套状态；而 `-wal` 里正是还没
    /// checkpoint 进主库的那段新数据（§十四 在真库上量到 `-wal` 比主库还大），
    /// 下次打开就是按错配的 WAL 去恢复。
    #[cfg(windows)]
    #[test]
    fn a_sidecar_that_cannot_be_archived_aborts_before_the_main_file_moves() {
        use std::os::windows::fs::OpenOptionsExt;
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("mig_sidecar_hold");
        let data = crate::paths::data_dir();
        let main = data.join("focusflow_accounting.db");
        std::fs::write(&main, b"current").unwrap();
        let wal = crate::data_location::with_suffix(&main, "-wal");
        std::fs::write(&wal, b"someone-else-tail").unwrap();

        // 独占占住 -wal（同步盘/杀软那一类，不需要真杀软）：rename 它必失败
        let hold = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&wal)
            .expect("占位句柄应能打开");
        let e = backup_before_overwrite(&main).expect_err("-wal 搬不动时整次留档必须失败");
        assert!(
            e.to_string().contains("-wal"),
            "错误要说清是哪个附属文件搬不动: {e}"
        );
        assert_eq!(
            std::fs::read_to_string(&main).unwrap(),
            "current",
            "失败的那一次绝不能已经把主库改名带走"
        );
        assert!(wal.exists(), "搬不动的 -wal 必须还在原位");
        assert!(
            !data
                .read_dir()
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().contains(".import-backup-")),
            "半套归档也不该留下"
        );
        drop(hold);
    }

    /// 同一份旧库、内容一行没变、只是 mtime 变了（资源管理器复制一份、云盘
    /// 同步、备份还原都是这个形状）→ 再导入一次**不得**把那一年翻倍。
    ///
    /// 大小+mtime 的指纹守卫挡不住这个形状（mtime 变了就照常往下走），而暂存表
    /// 在上轮聚合后已经连表丢掉、对导过什么毫无记忆，聚合又是累加式 —— 没有
    /// 内容指纹这一刀，整套旧明细会被再聚一遍，统计翻倍且无任何提示。
    /// 注掉内容比对（注回旧行为）这条必红。
    #[test]
    fn reimporting_unchanged_key_log_after_mtime_touch_does_not_double_count() {
        use std::time::Duration;
        let _lock = crate::paths::test_app_dir_lock();
        let _dir = crate::paths::test_app_dir("reimport");
        let year = 2024i32;

        // 源库：旧版形状（只有 key_log 明细），100 条落进同一天
        let src_dir = _dir.path().join("old");
        std::fs::create_dir_all(&src_dir).unwrap();
        let src = src_dir.join(format!("focusflow_{year}.db"));
        {
            let c = Connection::open(&src).unwrap();
            c.execute_batch(
                "CREATE TABLE key_log (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    key_name TEXT NOT NULL,
                    timestamp INTEGER NOT NULL
                );",
            )
            .unwrap();
            for i in 0..100 {
                c.execute(
                    "INSERT INTO key_log (key_name, timestamp) VALUES (?1, ?2)",
                    rusqlite::params![format!("k{}", i % 10), 1_700_000_000i64 + i],
                )
                .unwrap();
            }
        }
        // 目标库：已存在的空年度库 → 走合并路径（而不是整文件复制）
        let dst = paths::year_db_path(year);
        {
            let c = connection::open_rw(&dst).unwrap();
            connection::ensure_schema(&c, year).unwrap();
        }
        let daily_total = || -> i64 {
            connection::open_ro(&dst)
                .unwrap()
                .query_row(
                    "SELECT COALESCE(SUM(count), 0) FROM daily_counts",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        };

        let first = import_legacy_data(&src_dir);
        assert_eq!(
            first.records_by_year,
            vec![(year, 100)],
            "首次应导入全部 100 条明细"
        );
        assert_eq!(daily_total(), 100);

        // 摸 mtime（往回拨一小时，避开"刚创建"的边界）：大小不变、时间戳变
        let older = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&src)
            .unwrap()
            .set_modified(older)
            .unwrap();

        let again = import_legacy_data(&src_dir);
        assert_eq!(
            again.records_by_year,
            vec![(year, 0)],
            "内容没变就没有任何新明细：{:?}",
            again
        );
        assert_eq!(daily_total(), 100, "重导同内容不得把那一年翻倍");
    }

    /// 回滚那一步：留档的主库与它的 WAL 必须**一起**回位，且每一步的成败都要说实话。
    ///
    /// 旧顺序先 rename `-wal`/`-shm`、最后才搬主库；主库那一步失败时留档已经只剩光杆
    /// 主库，而提示写的是「导入前那份**完整**留在 …，请手工改回原名」—— 照着做就是把
    /// 主库与它的 WAL 拆开（`backup_before_overwrite` 的注释正禁止这个状态：缺了配套
    /// WAL 的主库会让下一次打开按错配的尾巴去恢复）。
    fn rollback_fixture(tag: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("ff_mig_rollback_{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("临时目录该建得出来");
        let dst = dir.join("focusflow_aux.db");
        let kept = dir.join("focusflow_aux.db.kept-20260101");
        std::fs::write(&dst, "半套的新库".as_bytes()).unwrap();
        std::fs::write(
            crate::data_location::with_suffix(&dst, "-wal"),
            "半套的 wal".as_bytes(),
        )
        .unwrap();
        std::fs::write(&kept, "导入前那一份".as_bytes()).unwrap();
        std::fs::write(
            crate::data_location::with_suffix(&kept, "-wal"),
            "没 checkpoint 的记录".as_bytes(),
        )
        .unwrap();
        (dir, dst, kept)
    }

    #[test]
    fn a_finished_rollback_puts_the_main_db_and_its_wal_back_together() {
        let (dir, dst, kept) = rollback_fixture("ok");
        let mut summary = ImportSummary::default();
        summary.backed_up_aux.push(kept.display().to_string());

        restore_after_failed_copy(&mut summary, &dst, &Some(kept.clone()));

        assert_eq!(
            std::fs::read(&dst).unwrap(),
            "导入前那一份".as_bytes().to_vec(),
            "主库要回到正名上"
        );
        assert_eq!(
            std::fs::read(crate::data_location::with_suffix(&dst, "-wal")).unwrap(),
            "没 checkpoint 的记录".as_bytes().to_vec(),
            "配套 WAL 必须跟着回位，否则下次打开按错配的尾巴恢复"
        );
        assert!(!kept.exists() && !crate::data_location::with_suffix(&kept, "-wal").exists());
        assert!(
            summary.backed_up_aux.is_empty(),
            "已经回到正名的那份不该再报成\"给你留了备份\""
        );
        let text = summary.errors.join("\n");
        assert!(text.contains("已回滚"), "实得: {text}");
        assert!(!text.contains("搬不回"), "全搬回来了不该再有抱怨: {text}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 主库搬不回去时：留档那一侧**一个字都不许动**，提示也不许说"完整"。
    ///
    /// 这一臂是顺序的探针：旧顺序在这条路上已经先把留档的 `-wal` 搬走了，于是
    /// "手工改回原名"会拿到一份没有 WAL 的主库。
    #[test]
    fn a_failed_rollback_leaves_the_archive_untouched_and_says_so() {
        let (dir, dst, kept) = rollback_fixture("blocked");
        // 让主库那一步 rename 必失败：正名上先放一个**非空目录**挡着
        // （空目录会被当成"搬进去"，这一步就成了成功 —— 夹具要挡的是失败那条路）
        std::fs::remove_file(&dst).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(dst.join("挡住"), "非空").unwrap();
        let kept_wal = crate::data_location::with_suffix(&kept, "-wal");

        let mut summary = ImportSummary::default();
        restore_after_failed_copy(&mut summary, &dst, &Some(kept.clone()));

        assert!(
            kept.exists() && kept_wal.exists(),
            "主库没能回位时，留档的主库与它的 WAL 都该还在原地（旧顺序会先把 WAL 搬走）；\
             实得 kept={} wal={} errors={:?}",
            kept.exists(),
            kept_wal.exists(),
            summary.errors
        );
        let text = summary.errors.join("\n");
        assert!(text.contains("回滚失败"), "实得: {text}");
        assert!(
            !text.contains("完整"),
            "没能证明它完整就不许说「完整」，实得: {text}"
        );
        // 挡路的目录也算"残缺没删掉"：下一次导入会把它当成目标库
        assert!(
            text.contains("删不掉"),
            "正名上删不掉的东西必须点名，实得: {text}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
