//! 把 `assets/focusflow.ico` 嵌进 exe 的资源表。
//!
//! 为什么需要：窗口图标是运行时从 `assets/focusflow.ico` 加载的（`GetClassLongPtr(GCLP_HICON)`
//! 非 0），但 **exe 文件本身**没有图标资源 —— 资源管理器、快捷方式、任务栏固定都只看 PE 里的
//! `RT_GROUP_ICON`，于是装出来的程序显示的是 Windows 的空白程序图标。
//!
//! 为什么是手写 .res 而不是 `winres` / `embed-resource`：这台机器没装 Windows SDK
//! （找不到 `rc.exe`），而这两个 crate 都要靠 `rc.exe`/`windres` 编译 `.rc`。
//! .res 的格式很简单（每条资源 = 两个 DWORD + 头部 + 原始数据），照规范手写一份，
//! 再把 .res 直接喂给 MSVC 的链接器（`link.exe` 接受 .res 作为输入），零新增依赖。

use std::path::{Path, PathBuf};

/// .res 里每条资源的头部长度（数字型 type + 数字型 name）。
/// 规范里 HeaderSize **包含**最前面那两个 DWORD（DataSize / HeaderSize 自己）：
/// DataSize 4 + HeaderSize 4 + type 4 + name 4 + DataVersion 4 + MemoryFlags 2
/// + LanguageId 2 + Version 4 + Characteristics 4 = 32。
/// 写 24（只算后半段）时 link.exe 报 LNK1136: invalid or corrupt file。
const RES_HEADER_SIZE: u32 = 32;
const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;

fn push_resource(out: &mut Vec<u8>, ty: u16, id: u16, data: &[u8], memory_flags: u16) {
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&RES_HEADER_SIZE.to_le_bytes());
    // 头部：数字型 type（0xFFFF + id）、数字型 name（0xFFFF + id）
    out.extend_from_slice(&0xFFFFu16.to_le_bytes());
    out.extend_from_slice(&ty.to_le_bytes());
    out.extend_from_slice(&0xFFFFu16.to_le_bytes());
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // DataVersion
    out.extend_from_slice(&memory_flags.to_le_bytes());
    out.extend_from_slice(&0x0409u16.to_le_bytes()); // LanguageId: en-US（与 installer.nsi 一致）
    out.extend_from_slice(&0u32.to_le_bytes()); // Version
    out.extend_from_slice(&0u32.to_le_bytes()); // Characteristics
    out.extend_from_slice(data);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

/// `rc.exe` 生成的 .res 都以一条"空记录"（DataSize=0、type=0、name=0、flags=0）开头。
/// 这不是装饰：链接器要求第一条记录的头部合法，**直接以 RT_ICON 开头会被判成
/// LNK1136 invalid or corrupt file**（实测：只加这一条 32 字节，其余字节完全不变）。
fn push_null_entry(out: &mut Vec<u8>) {
    push_resource(out, 0, 0, &[], 0);
}

struct IcoEntry {
    width: u8,
    height: u8,
    color_count: u8,
    planes: u16,
    bit_count: u16,
    bytes: Vec<u8>,
}

/// 读 .ico 的 ICONDIR + ICONDIRENTRY，切出每张图的原始字节。
/// 0x00 的宽高表示 256（ICO 里一个 BYTE 放不下 256）。
fn read_ico(path: &Path) -> std::io::Result<Vec<IcoEntry>> {
    let b = std::fs::read(path)?;
    if b.len() < 6 || u16::from_le_bytes([b[0], b[1]]) != 0 || u16::from_le_bytes([b[2], b[3]]) != 1
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} 不是 .ico 文件", path.display()),
        ));
    }
    let count = u16::from_le_bytes([b[4], b[5]]) as usize;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let o = 6 + i * 16;
        if o + 16 > b.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "ICONDIRENTRY 越界",
            ));
        }
        let size = u32::from_le_bytes([b[o + 8], b[o + 9], b[o + 10], b[o + 11]]) as usize;
        let off = u32::from_le_bytes([b[o + 12], b[o + 13], b[o + 14], b[o + 15]]) as usize;
        if off + size > b.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "图标数据越界",
            ));
        }
        out.push(IcoEntry {
            width: b[o],
            height: b[o + 1],
            color_count: b[o + 2],
            planes: u16::from_le_bytes([b[o + 4], b[o + 5]]),
            bit_count: u16::from_le_bytes([b[o + 6], b[o + 7]]),
            bytes: b[off..off + size].to_vec(),
        });
    }
    Ok(out)
}

fn main() {
    // 只有 MSVC 的链接器吃 .res；别的目标（比如以后跑 clippy 用 gnu 目标）就跳过。
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let ico = manifest.join("assets").join("focusflow.ico");
    println!("cargo:rerun-if-changed={}", ico.display());
    let entries = match read_ico(&ico) {
        Ok(e) if !e.is_empty() => e,
        other => {
            // 缺图标不该挡住编译：窗口图标本来就还有运行时那条路。
            println!(
                "cargo:warning=无法读取 {}（{:?}），exe 将没有图标资源",
                ico.display(),
                other.err()
            );
            return;
        }
    };

    let mut res: Vec<u8> = Vec::new();
    push_null_entry(&mut res);
    // RT_ICON：每张图一条，id 从 1 开始。
    // MemoryFlags 照 rc.exe 的口径：图标 0x1010（MOVEABLE|DISCARDABLE）、
    // 图标组 0x1030（MOVEABLE|PURE|DISCARDABLE）。
    for (i, e) in entries.iter().enumerate() {
        push_resource(&mut res, RT_ICON, (i + 1) as u16, &e.bytes, 0x1010);
    }
    // RT_GROUP_ICON：GRPICONDIR(6) + 每条 14 字节，指回上面那些 RT_ICON 的 id。
    let mut group: Vec<u8> = Vec::new();
    group.extend_from_slice(&0u16.to_le_bytes()); // reserved
    group.extend_from_slice(&1u16.to_le_bytes()); // type = 1（图标）
    group.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for (i, e) in entries.iter().enumerate() {
        group.push(e.width);
        group.push(e.height);
        group.push(e.color_count);
        group.push(0); // reserved
        group.extend_from_slice(&e.planes.to_le_bytes());
        group.extend_from_slice(&e.bit_count.to_le_bytes());
        group.extend_from_slice(&(e.bytes.len() as u32).to_le_bytes());
        group.extend_from_slice(&((i + 1) as u16).to_le_bytes());
    }
    push_resource(&mut res, RT_GROUP_ICON, 1, &group, 0x1030);

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("focusflow.res");
    if let Err(e) = std::fs::write(&out, &res) {
        println!("cargo:warning=写 {} 失败：{e}", out.display());
        return;
    }
    println!("cargo:rustc-link-arg={}", out.display());
}
