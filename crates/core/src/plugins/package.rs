//! 插件包（方案 P2 第 16 步）：一个**确定性** zip，里面是 `plugin.lua` + `assets/`。
//!
//! 为什么不是"随便一个 zip"：内容哈希要能回答"这份包与上次授权的是不是同一份"。同一棵
//! 目录必须每次都打出**同一个字节序列**，否则重打一次包就会让每个用户看到"插件已更新"。
//! 所以：条目按名字排序、时间戳写死 1980-01-01 00:00、不带注释与额外字段。
//!
//! 为什么只做 STORED（method 0）：宿主不引 deflate 依赖，而我们的包是脚本 + 少量图片。
//! 别的实现打出来的压缩包会**明确拒绝**（"只认得 STORED"），不会读出一堆乱码 ——
//! 读错比读不了贵得多。
//!
//! 读的一侧要当不可信输入：条目名不许带 `..`、不许绝对路径、不许反斜杠，声明的大小与
//! 实际数据必须对得上，CRC 要验，entry 数与总字节数都有上限（zip 炸弹）。
//!
//! 包里的两样东西是契约（方案 3.3 / 第 16 步）：根上的 `plugin.lua`（必须有）与
//! `assets/` 下的附属文件（可选）。目录里多出来的东西一律拒绝 —— 让它们悄悄不进包，
//! 作者会以为它们进了。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// 包里那份脚本的名字（方案 3.3 的 `plugin.lua`）。
pub const SCRIPT_NAME: &str = "plugin.lua";
/// 附属文件的那一格。名字以它开头的条目才会被收进包。
pub const ASSET_PREFIX: &str = "assets/";
/// 一个包最多几个条目。
pub const MAX_ENTRIES: usize = 4096;
/// 单个条目最多多少字节（脚本 + 图片远低于这个数）。
pub const MAX_ENTRY_BYTES: usize = 8 * 1024 * 1024;
/// 整个包解出来最多多少字节。
pub const MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;

const LOCAL_SIG: u32 = 0x0403_4b50;
const CENTRAL_SIG: u32 = 0x0201_4b50;
const EOCD_SIG: u32 = 0x0605_4b50;
/// bit 11：文件名按 UTF-8 解释（Windows 资源管理器与 7-Zip 都认）。
const FLAG_UTF8: u16 = 0x0800;
/// 写死的 1980-01-01 00:00:00（DOS 时间）。确定性包不许带"打包那一刻"。
const DOS_TIME: u16 = 0x0000;
const DOS_DATE: u16 = 0x0021;
const VERSION_NEEDED: u16 = 20;
const EOCD_MIN: usize = 22;

/// 包里的一个文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// 包内路径，一律用 `/` 分隔（`plugin.lua` 或 `assets/…`）。
    pub name: String,
    pub data: Vec<u8>,
}

/// 这个路径看起来是不是一个插件包（`.zip`）。
pub fn is_package_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
}

/// 把一组条目打成确定性 zip。
///
/// 排序、时间戳、条目内容之外**没有一个字节**来自环境：同一组条目在任何机器上都是同一份。
pub fn pack(mut entries: Vec<Entry>) -> Result<Vec<u8>, String> {
    if entries.is_empty() {
        return Err("插件包里一个文件都没有".to_string());
    }
    if entries.len() > MAX_ENTRIES {
        return Err(format!(
            "插件包条目太多：{} 个，上限 {MAX_ENTRIES} 个",
            entries.len()
        ));
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let mut total = 0usize;
    for window in entries.windows(2) {
        if window[0].name == window[1].name {
            return Err(format!("插件包里有重名条目：{}", window[0].name));
        }
    }
    for entry in &entries {
        checked_name(&entry.name)?;
        if entry.data.len() > MAX_ENTRY_BYTES {
            return Err(format!(
                "条目 {} 太大：{} 字节，单个上限 {MAX_ENTRY_BYTES} 字节",
                entry.name,
                entry.data.len()
            ));
        }
        total = total.saturating_add(entry.data.len());
    }
    if total > MAX_TOTAL_BYTES {
        return Err(format!(
            "插件包太大：解出来共 {total} 字节，上限 {MAX_TOTAL_BYTES} 字节"
        ));
    }

    let mut out: Vec<u8> = Vec::new();
    let mut central: Vec<u8> = Vec::new();
    for entry in &entries {
        let offset = out.len() as u32;
        let crc = crc32(&entry.data);
        let size = entry.data.len() as u32;
        let name = entry.name.as_bytes();

        push_u32(&mut out, LOCAL_SIG);
        push_u16(&mut out, VERSION_NEEDED);
        push_u16(&mut out, FLAG_UTF8);
        push_u16(&mut out, 0); // STORED
        push_u16(&mut out, DOS_TIME);
        push_u16(&mut out, DOS_DATE);
        push_u32(&mut out, crc);
        push_u32(&mut out, size);
        push_u32(&mut out, size);
        push_u16(&mut out, name.len() as u16);
        push_u16(&mut out, 0); // extra
        out.extend_from_slice(name);
        out.extend_from_slice(&entry.data);

        push_u32(&mut central, CENTRAL_SIG);
        push_u16(&mut central, VERSION_NEEDED); // made by
        push_u16(&mut central, VERSION_NEEDED); // needed
        push_u16(&mut central, FLAG_UTF8);
        push_u16(&mut central, 0); // STORED
        push_u16(&mut central, DOS_TIME);
        push_u16(&mut central, DOS_DATE);
        push_u32(&mut central, crc);
        push_u32(&mut central, size);
        push_u32(&mut central, size);
        push_u16(&mut central, name.len() as u16);
        push_u16(&mut central, 0); // extra
        push_u16(&mut central, 0); // comment
        push_u16(&mut central, 0); // disk
        push_u16(&mut central, 0); // internal attrs
        push_u32(&mut central, 0); // external attrs
        push_u32(&mut central, offset);
        central.extend_from_slice(name);
    }
    let cd_offset = out.len() as u32;
    let cd_size = central.len() as u32;
    out.extend_from_slice(&central);
    push_u32(&mut out, EOCD_SIG);
    push_u16(&mut out, 0); // this disk
    push_u16(&mut out, 0); // cd disk
    push_u16(&mut out, entries.len() as u16);
    push_u16(&mut out, entries.len() as u16);
    push_u32(&mut out, cd_size);
    push_u32(&mut out, cd_offset);
    push_u16(&mut out, 0); // comment
    Ok(out)
}

/// 读一个包。任何一条不满足都交 Err —— 读的一侧当**不可信输入**。
pub fn read(bytes: &[u8]) -> Result<Vec<Entry>, String> {
    let eocd = find_eocd(bytes).ok_or_else(|| {
        "这不是一个 zip：找不到目录尾（EOCD）。如果是压缩过的包，本宿主只认得 STORED（不压缩）"
            .to_string()
    })?;
    let count = u16_at(bytes, eocd + 10).ok_or("目录尾被截断")?;
    let cd_size = u32_at(bytes, eocd + 12).ok_or("目录尾被截断")?;
    let cd_offset = u32_at(bytes, eocd + 16).ok_or("目录尾被截断")?;
    if count == u16::MAX || cd_size == u32::MAX || cd_offset == u32::MAX {
        return Err(
            "这个包用了 zip64，本宿主不认（条目/尺寸超出 4 GB 的包不属于插件）".to_string(),
        );
    }
    if count as usize > MAX_ENTRIES {
        return Err(format!("条目太多：{count} 个，上限 {MAX_ENTRIES} 个"));
    }
    let cd_end = (cd_offset as usize)
        .checked_add(cd_size as usize)
        .ok_or("目录偏移溢出")?;
    if cd_end > bytes.len() {
        return Err("目录区越过了文件末尾（包被截断？）".to_string());
    }

    let mut entries: Vec<Entry> = Vec::with_capacity(count as usize);
    let mut total = 0usize;
    let mut pos = cd_offset as usize;
    for index in 0..count {
        let where_ = format!("第 {} 个条目", index + 1);
        if u32_at(bytes, pos) != Some(CENTRAL_SIG) {
            return Err(format!("{where_}的目录项签名不对（目录区错位或包被改过）"));
        }
        let method = u16_at(bytes, pos + 10).ok_or("目录项被截断")?;
        let crc = u32_at(bytes, pos + 16).ok_or("目录项被截断")?;
        let compressed = u32_at(bytes, pos + 20).ok_or("目录项被截断")?;
        let uncompressed = u32_at(bytes, pos + 24).ok_or("目录项被截断")?;
        let name_len = u16_at(bytes, pos + 28).ok_or("目录项被截断")? as usize;
        let extra_len = u16_at(bytes, pos + 30).ok_or("目录项被截断")? as usize;
        let comment_len = u16_at(bytes, pos + 32).ok_or("目录项被截断")? as usize;
        let local_offset = u32_at(bytes, pos + 42).ok_or("目录项被截断")? as usize;
        let name_at = pos + 46;
        let name_bytes = bytes
            .get(name_at..name_at + name_len)
            .ok_or_else(|| format!("{where_}的名字越过了文件末尾"))?;
        let name = std::str::from_utf8(name_bytes)
            .map_err(|e| format!("{where_}的名字不是 UTF-8（{e}）：包坏了"))?
            .to_string();
        checked_name(&name)?;
        if method != 0 {
            return Err(format!(
                "条目 {name} 用的是压缩方法 {method}，本宿主只认得 STORED（0）—— \
                 请用 focusflow-plugin pack 打出来的包，或先解压再打包"
            ));
        }
        if compressed != uncompressed {
            return Err(format!("条目 {name} 的压缩/原始大小不一致，包坏了"));
        }
        if uncompressed as usize > MAX_ENTRY_BYTES {
            return Err(format!(
                "条目 {name} 声明的原始大小 {} 字节超过上限 {MAX_ENTRY_BYTES}",
                uncompressed
            ));
        }
        total = total.saturating_add(uncompressed as usize);
        if total > MAX_TOTAL_BYTES {
            return Err(format!(
                "包解出来太大（已超过 {MAX_TOTAL_BYTES} 字节）：拒绝，免得把宿主的内存吃掉"
            ));
        }

        // 本地头：偏移必须落在文件内，方法、名字与大小要与目录项说得一致。
        if u32_at(bytes, local_offset) != Some(LOCAL_SIG) {
            return Err(format!(
                "条目 {name} 的本地头签名不对（偏移错位或包被改过）"
            ));
        }
        let local_method = u16_at(bytes, local_offset + 8).ok_or("本地头被截断")?;
        let local_name_len = u16_at(bytes, local_offset + 26).ok_or("本地头被截断")? as usize;
        let local_extra_len = u16_at(bytes, local_offset + 28).ok_or("本地头被截断")? as usize;
        let local_name = bytes
            .get(local_offset + 30..local_offset + 30 + local_name_len)
            .ok_or("本地头被截断")?;
        if local_method != 0 || local_name != name_bytes {
            return Err(format!(
                "条目 {name} 的本地头与目录项对不上（包里两处各说一套）"
            ));
        }
        let data_at = local_offset + 30 + local_name_len + local_extra_len;
        let data = bytes
            .get(data_at..data_at + uncompressed as usize)
            .ok_or_else(|| format!("条目 {name} 的数据越过了文件末尾（包被截断）"))?;
        if crc32(data) != crc {
            return Err(format!("条目 {name} 的 CRC 对不上：包被改过或读坏了"));
        }
        entries.push(Entry {
            name,
            data: data.to_vec(),
        });

        pos = name_at + name_len + extra_len + comment_len;
    }
    Ok(entries)
}

/// 把一个目录打成条目表：根上必须有 `plugin.lua`，旁边只许有 `assets/`。
///
/// 多出来的东西一律拒绝而不是忽略：作者把 `readme.md` 放进插件目录、以为它也进了包，
/// 是这一族里最常见的"发布出去才发现少东西"。
pub fn pack_dir(dir: &Path) -> Result<Vec<Entry>, String> {
    if !dir.is_dir() {
        return Err(format!("{} 不是目录", dir.display()));
    }
    let script = dir.join(SCRIPT_NAME);
    let data = std::fs::read(&script).map_err(|e| {
        format!(
            "{} 读不出来：{e}（插件包根上必须有 {SCRIPT_NAME}）",
            script.display()
        )
    })?;
    std::str::from_utf8(&data).map_err(|e| {
        format!(
            "{} 不是 UTF-8 文本：{e}（Lua 脚本按文本读）",
            script.display()
        )
    })?;
    let mut entries = vec![Entry {
        name: SCRIPT_NAME.to_string(),
        data,
    }];

    let assets = dir.join("assets");
    if assets.exists() {
        if !assets.is_dir() {
            return Err(format!(
                "{} 存在但不是目录：附属文件要放在 assets/ 下面",
                assets.display()
            ));
        }
        walk_assets(&assets, ASSET_PREFIX.trim_end_matches('/'), &mut entries)?;
    }

    let mut accepted: Vec<String> = Vec::new();
    let listing = std::fs::read_dir(dir).map_err(|e| format!("{} 列不出来：{e}", dir.display()))?;
    for item in listing {
        let item = item.map_err(|e| format!("{} 列到一半出错：{e}", dir.display()))?;
        let name = item
            .file_name()
            .into_string()
            .map_err(|_| format!("{} 里有一个不是 UTF-8 的名字，打不进包", dir.display()))?;
        if name != SCRIPT_NAME && name != "assets" {
            accepted.push(name);
        }
    }
    if !accepted.is_empty() {
        accepted.sort();
        return Err(format!(
            "插件包里只收 {SCRIPT_NAME} 与 assets/，这一份里还有：{} —— \
             附属文件请挪进 assets/，其余文件不属于插件包",
            accepted.join("、")
        ));
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

fn walk_assets(dir: &Path, prefix: &str, out: &mut Vec<Entry>) -> Result<(), String> {
    let mut items: Vec<std::fs::DirEntry> = Vec::new();
    for item in std::fs::read_dir(dir).map_err(|e| format!("{} 列不出来：{e}", dir.display()))?
    {
        items.push(item.map_err(|e| format!("{} 列到一半出错：{e}", dir.display()))?);
    }
    items.sort_by_key(|i| i.file_name());
    for item in items {
        let path = item.path();
        let name = item
            .file_name()
            .into_string()
            .map_err(|_| format!("{} 里有一个不是 UTF-8 的名字，打不进包", dir.display()))?;
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| format!("{} 读不出来：{e}", path.display()))?;
        let inner = format!("{prefix}/{name}");
        if meta.file_type().is_symlink() {
            return Err(format!(
                "{} 是符号链接：包里不收链接（换一台机器就断了的条目不该进哈希）",
                path.display()
            ));
        }
        if meta.is_dir() {
            walk_assets(&path, &inner, out)?;
        } else if meta.is_file() {
            let data =
                std::fs::read(&path).map_err(|e| format!("{} 读不出来：{e}", path.display()))?;
            out.push(Entry { name: inner, data });
        } else {
            return Err(format!("{} 既不是文件也不是目录，打不进包", path.display()));
        }
    }
    Ok(())
}

/// 把条目写到目录里（当前没有生产调用方：安装是整包落盘，运行时不解压）。
///
/// 名字在写入前**再过一遍**同一把尺子：`read` 已经验过，但调用方也可能把手工拼的条目
/// 交进来（比如以后从别处导入），路径纪律不该只挂在一条入口上。
pub fn extract_to(entries: &[Entry], dir: &Path) -> Result<(), String> {
    for entry in entries {
        checked_name(&entry.name)?;
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("{} 建不出来：{e}", dir.display()))?;
    for entry in entries {
        let target = under(dir, &entry.name);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("{} 建不出来：{e}", parent.display()))?;
        }
        let mut file = std::fs::File::create(&target)
            .map_err(|e| format!("{} 写不出来：{e}", target.display()))?;
        file.write_all(&entry.data)
            .map_err(|e| format!("{} 写到一半失败：{e}", target.display()))?;
    }
    Ok(())
}

/// 条目名拼成落盘路径（只按 `/` 切；条目名已过 [`checked_name`]）。
fn under(dir: &Path, name: &str) -> PathBuf {
    let mut path = dir.to_path_buf();
    for segment in name.split('/') {
        path.push(segment);
    }
    path
}

/// 读一份**插件源文件**：单文件 `.lua` 或插件包 `.zip` 都走这一条。
///
/// 交回 `(脚本正文, 用来算 content_hash 的字节)`：
/// · 单文件 —— 文件原始字节既是脚本也是哈希对象（方案 3.3 那一行的原意）；
/// · 包 —— 正文取自包里那份 `plugin.lua`，哈希对象是**包字节**（确定性打包保证
///   同一棵树每次是同一串字节，所以重新打包不会被念成"插件已更新"）。
///
/// 两处调用（加载与元数据扫描）读同一份字节，页面上的名字才不会与跑起来的那一份分家。
pub fn read_script(path: &Path) -> Result<(String, Vec<u8>), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{} 读不出来：{e}", path.display()))?;
    if is_package_path(path) {
        let entries = read(&bytes)?;
        let text = script_source(&entries)?.to_string();
        return Ok((text, bytes));
    }
    let text = String::from_utf8(bytes.clone())
        .map_err(|e| format!("{} 不是 UTF-8 文本：{e}", path.display()))?;
    Ok((text, bytes))
}

/// 包里那份脚本（必须是 UTF-8）。
pub fn script_source(entries: &[Entry]) -> Result<&str, String> {
    let entry = entries
        .iter()
        .find(|e| e.name == SCRIPT_NAME)
        .ok_or_else(|| format!("包里没有 {SCRIPT_NAME}：它必须是包根上的那一份"))?;
    std::str::from_utf8(&entry.data).map_err(|e| format!("{SCRIPT_NAME} 不是 UTF-8 文本：{e}"))
}

/// 条目名的一条尺子：不许绝对、不许 `..`、不许反斜杠、不许空段、不许盘符。
fn checked_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("包里有空名字的条目".to_string());
    }
    if name.len() > 512 {
        return Err(format!("条目名太长（{} 字节）：{name}", name.len()));
    }
    // 冒号整串都挡：盘符不只长在开头。`assets/C:evil` 的第二段是一个 Windows
    // 盘符相对路径，逐段 `Path::push` 会用它替换掉整个目标路径。
    if name.starts_with('/') || name.contains('\\') || name.contains('\0') || name.contains(':') {
        return Err(format!(
            "条目名 {name:?} 不是包内相对路径（不许绝对路径、反斜杠、盘符或 NUL）"
        ));
    }
    if name.ends_with('/') {
        return Err(format!("条目名 {name:?} 是目录条目：本格式只收文件"));
    }
    for segment in name.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(format!(
                "条目名 {name:?} 里有空段、`.` 或 `..`：解包时会跑到目标目录外面"
            ));
        }
    }
    Ok(())
}

fn find_eocd(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < EOCD_MIN {
        return None;
    }
    let first = bytes.len().saturating_sub(EOCD_MIN + u16::MAX as usize);
    (first..=bytes.len() - EOCD_MIN).rev().find(|&at| {
        // 只认注释长度与文件末尾**对得上**的那一个：数据区里也可能出现同样的四个字节。
        u32_at(bytes, at) == Some(EOCD_SIG)
            && u16_at(bytes, at + 20).is_some_and(|len| at + EOCD_MIN + len as usize == bytes.len())
    })
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    let slice = bytes.get(at..at + 2)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 4)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// CRC-32（IEEE 802.3）：zip 每个条目都要它。
fn crc32(data: &[u8]) -> u32 {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (i, slot) in table.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *slot = c;
        }
        table
    });
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc = table[((crc ^ byte as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, text: &str) -> Entry {
        Entry {
            name: name.to_string(),
            data: text.as_bytes().to_vec(),
        }
    }

    fn sample() -> Vec<Entry> {
        vec![
            entry(SCRIPT_NAME, "PLUGIN_NAME = \"demo\"\n"),
            entry("assets/logo.txt", "logo"),
            entry("assets/icons/a.bin", "\u{1}\u{2}\u{3}"),
        ]
    }

    /// 确定性：同一组条目（顺序打乱也给）打出来必须是同一串字节。
    ///
    /// 内容哈希要回答"这份包与授权时那一份是不是同一份"—— 每次重打包都变一个字节，
    /// 用户就会每次启动都看到"插件已更新"。
    #[test]
    fn two_packs_of_the_same_entries_are_byte_identical() {
        let a = pack(sample()).unwrap();
        let mut shuffled = sample();
        shuffled.reverse();
        let b = pack(shuffled).unwrap();
        assert_eq!(a, b, "打包结果不许依赖调用方给的顺序");
        assert_eq!(a, pack(sample()).unwrap(), "同一组条目重打一次必须一样");
        // 时间戳是写死的 1980-01-01：文件里不许带"打包那一刻"
        assert_eq!(u16_at(&a, 10), Some(DOS_TIME));
        assert_eq!(u16_at(&a, 12), Some(DOS_DATE));
    }

    #[test]
    fn a_pack_round_trips_through_the_reader() {
        let packed = pack(sample()).unwrap();
        let mut back = read(&packed).unwrap();
        back.sort_by(|a, b| a.name.cmp(&b.name));
        let mut want = sample();
        want.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(back, want);
        assert_eq!(script_source(&back).unwrap(), "PLUGIN_NAME = \"demo\"\n");
    }

    /// 读的一侧当不可信输入：目录项里写 `../evil.lua`，解包就会跑到目标目录外面。
    #[test]
    fn the_reader_refuses_a_name_that_climbs_out_of_the_package() {
        for bad in ["../evil.lua", "/abs.lua", "a\\b.lua", "assets//x", "./x"] {
            let err = read(&raw_zip(bad, b"x", 0)).expect_err("这种名字必须被拒");
            assert!(
                err.contains("条目名") || err.contains("相对路径"),
                "拒绝的理由要说清是名字的问题，实得：{err}"
            );
        }
    }

    #[test]
    fn the_reader_refuses_compressed_entries_by_name() {
        let err = read(&raw_zip("plugin.lua", b"x", 8)).expect_err("deflate 不在支持范围");
        assert!(
            err.contains("STORED") && err.contains("8"),
            "要说清只认 STORED 以及那一项用的是什么方法：{err}"
        );
    }

    #[test]
    fn a_truncated_package_says_it_is_truncated() {
        let packed = pack(sample()).unwrap();
        let cut = &packed[..packed.len() / 2];
        let err = read(cut).expect_err("截断的包不能被当成空包");
        assert!(
            err.contains("zip") || err.contains("截断") || err.contains("越过"),
            "拒绝的理由要指向包本身：{err}"
        );
    }

    /// 声明的原始大小远超实际数据（zip 炸弹的常见形状）必须在分配内存之前拒绝。
    #[test]
    fn a_declared_size_over_the_cap_is_refused_before_reading_data() {
        let mut zip = raw_zip("plugin.lua", b"x", 0);
        // 目录项与本地头里的"原始大小"都改成上限之上
        let huge = (MAX_ENTRY_BYTES + 1) as u32;
        let eocd = find_eocd(&zip).unwrap();
        let cd_offset = u32_at(&zip, eocd + 16).unwrap() as usize;
        // 压缩大小与原始大小一起改：只改一个的话先撞上的是"两处各说一套"那条检查。
        for at in [cd_offset + 20, cd_offset + 24, 18, 22] {
            zip[at..at + 4].copy_from_slice(&huge.to_le_bytes());
        }
        let err = read(&zip).expect_err("超大声明必须被拒");
        assert!(err.contains("上限"), "要说清是超了哪条上限：{err}");
    }

    /// 目录打进来只收 `plugin.lua` + `assets/`；少脚本、多别的都要点名。
    #[test]
    fn pack_dir_names_what_is_missing_or_extra() {
        let root = temp_dir("pack_dir_rules");
        std::fs::write(root.join("readme.md"), "hi").unwrap();
        let err = pack_dir(&root).expect_err("根上没有 plugin.lua 不行");
        assert!(err.contains(SCRIPT_NAME), "要说清少的是哪一份：{err}");

        std::fs::write(root.join(SCRIPT_NAME), "PLUGIN_NAME = \"x\"\n").unwrap();
        let err = pack_dir(&root).expect_err("多出来的 readme.md 不许悄悄丢掉");
        assert!(
            err.contains("readme.md") && err.contains("assets/"),
            "要说清多的是什么、该放哪里：{err}"
        );

        std::fs::remove_file(root.join("readme.md")).unwrap();
        std::fs::create_dir_all(root.join("assets/icons")).unwrap();
        std::fs::write(root.join("assets/icons/a.txt"), "a").unwrap();
        let entries = pack_dir(&root).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["assets/icons/a.txt", SCRIPT_NAME]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 盘符藏在第二段也要挡：`checked_name` 原先只看整串的第 2 个字节，
    /// 而解包那一侧是逐段 `Path::push` —— Windows 上 `push("C:evil")` 会把整个目标
    /// 路径换成盘符相对路径，于是"解到目标目录里"这句话就不成立了。
    #[test]
    fn a_drive_letter_is_refused_wherever_it_sits() {
        for bad in ["C:evil.lua", "assets/C:evil", "assets/c:/x"] {
            let err = read(&raw_zip(bad, b"x", 0)).expect_err("带盘符的条目名必须被拒");
            assert!(
                err.contains("盘符") || err.contains("相对路径"),
                "拒绝的理由要说是名字的问题：{err}"
            );
        }
        let root = temp_dir("extract_drive");
        let err = extract_to(&[entry("assets/C:evil", "x")], &root)
            .expect_err("手工拼的条目也要过同一把尺子");
        assert!(
            err.contains("盘符") || err.contains("相对路径"),
            "写入那一侧也要挡住：{err}"
        );
        assert!(!root.join("assets").exists(), "被拒的条目不该留下半个目录");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn extract_writes_the_tree_and_nothing_outside_it() {
        let root = temp_dir("extract");
        extract_to(&sample(), &root).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join(SCRIPT_NAME)).unwrap(),
            "PLUGIN_NAME = \"demo\"\n"
        );
        assert_eq!(
            std::fs::read(root.join("assets/icons/a.bin")).unwrap(),
            vec![1, 2, 3]
        );
        let err = extract_to(&[entry("../escape.txt", "x")], &root.join("nested"))
            .expect_err("手工拼的条目也要过路径纪律");
        assert!(err.contains(".."), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ff_pkg_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 手搓一个最小 zip（一个条目），用来喂"读的一侧"那些坏形状。
    fn raw_zip(name: &str, data: &[u8], method: u16) -> Vec<u8> {
        let mut out = Vec::new();
        let crc = crc32(data);
        let size = data.len() as u32;
        push_u32(&mut out, LOCAL_SIG);
        push_u16(&mut out, VERSION_NEEDED);
        push_u16(&mut out, FLAG_UTF8);
        push_u16(&mut out, method);
        push_u16(&mut out, DOS_TIME);
        push_u16(&mut out, DOS_DATE);
        push_u32(&mut out, crc);
        push_u32(&mut out, size);
        push_u32(&mut out, size);
        push_u16(&mut out, name.len() as u16);
        push_u16(&mut out, 0);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        let cd_offset = out.len() as u32;
        let mut central = Vec::new();
        push_u32(&mut central, CENTRAL_SIG);
        push_u16(&mut central, VERSION_NEEDED);
        push_u16(&mut central, VERSION_NEEDED);
        push_u16(&mut central, FLAG_UTF8);
        push_u16(&mut central, method);
        push_u16(&mut central, DOS_TIME);
        push_u16(&mut central, DOS_DATE);
        push_u32(&mut central, crc);
        push_u32(&mut central, size);
        push_u32(&mut central, size);
        push_u16(&mut central, name.len() as u16);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u32(&mut central, 0);
        push_u32(&mut central, 0);
        central.extend_from_slice(name.as_bytes());
        let cd_size = central.len() as u32;
        out.extend_from_slice(&central);
        push_u32(&mut out, EOCD_SIG);
        push_u16(&mut out, 0);
        push_u16(&mut out, 0);
        push_u16(&mut out, 1);
        push_u16(&mut out, 1);
        push_u32(&mut out, cd_size);
        push_u32(&mut out, cd_offset);
        push_u16(&mut out, 0);
        out
    }
}
