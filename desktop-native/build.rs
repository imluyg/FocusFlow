//! 往 exe 的资源表里塞两样东西：**图标**（手写 .res）与**版本信息**（`rc.exe` 编 .rc）。
//!
//! 为什么都需要：图标在运行时先从镜像的 `RT_GROUP_ICON` 取（读不到才回退散文件
//! `assets/focusflow.ico`），而 **exe 文件本身**没有图标资源 —— 资源管理器、快捷方式、任务栏
//! 固定都只看 PE 里的 `RT_GROUP_ICON`，于是装出来的程序显示的是 Windows 的空白程序图标。
//! 版本信息同理：没有那条
//! `RT_VERSION` 记录，右键属性→详细信息 一片空白、按文件版本审计拿不到数（`installer.nsi` 的
//! `VIAddVersionKey` 只给 setup.exe 盖章，帮不上 exe 自己）。
//!
//! 为什么两条走法不一样：
//! - 图标：定这条路时这台机器**确实没装 Windows SDK**（找不到 `rc.exe`），而 `winres` /
//!   `embed-resource` 都要靠 `rc.exe`/`windres`，于是照规范手写一份 .res（每条资源 = 两个 DWORD
//!   + 头部 + 原始数据），直接喂给 `link.exe`。现在 SDK 装了（见下面 `find_rc_exe`），但这 40 行
//!     实测可用，没有为"统一走法"去重写它的理由。
//! - 版本信息：先按图标那条老路子手写过一份，字节布局跟**能读出来的**金标准逐字节对上了
//!   `GetFileVersionInfo` 仍返回空（差的是资源树的形状）⇒ 改让 `rc.exe` 生成。

use std::path::{Path, PathBuf};

/// .res 里每条资源的头部长度（数字型 type + 数字型 name）。
/// 规范里 HeaderSize **包含**最前面那两个 DWORD（DataSize / HeaderSize 自己）：
/// DataSize 4 + HeaderSize 4 + type 4 + name 4 + DataVersion 4 + MemoryFlags 2
/// + LanguageId 2 + Version 4 + Characteristics 4 = 32。
///   写 24（只算后半段）时 link.exe 报 LNK1136: invalid or corrupt file。
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
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

/// `rc.exe` 生成的 .res 都以一条"空记录"（DataSize=0、type=0、name=0、flags=0）开头。
/// 这不是装饰：链接器要求第一条记录的头部合法，**直接以 RT_ICON 开头会被判成
/// LNK1136 invalid or corrupt file**（实测：只加这一条 32 字节，其余字节完全不变）。
fn push_null_entry(out: &mut Vec<u8>) {
    push_resource(out, 0, 0, &[], 0);
}

// ---------------------------------------------------------------------------
// VS_VERSIONINFO（右键属性→详细信息 那几格）。
//
// 为什么曾经缺：版本资源本来由 Tauri 前端从 `tauri.conf.json` 生成（2026-09-23 那几场量到的
// `File=0.3.0 Prod=0.3.0` 就是这么来的）；前端删掉、换成原生 `cargo` bin 之后这一格掉了。
// 而 `installer.nsi` 的 `VIAddVersionKey` 只给 **setup.exe** 盖章、注册表 `DisplayVersion` 也正常，
// 于是"应用和功能"里看得见版本，只有 exe 自己的属性面板是空白。
//
// 为什么走 rc.exe 而不是继续手写：先按图标那条老路子手搓了一份 VS_VERSIONINFO 塞进同一份 .res，
// 连结构头、补白、52 字节 FIXEDFILEINFO 的字段序都跟 setup.exe 里那份**能读出来的**逐字节对上了，
// `GetFileVersionInfo` 仍然返回空 —— 差的是树的形状：正规布局是
// root → `StringFileInfo` → `040904b0` → 各条 VALUE，外面还要一个 `VarFileInfo`/`Translation`，
// 把手写那份的 StringTable 直接挂在根上不够。字节级排布靠人肉对齐没有意义，
// 而这台机器的 SDK 其实装着（注册表 `KitsRoot10` → `E:\develop\Windows Kits\10`，
// `bin\10.0.28000.0\x64\rc.exe` 实测在）—— 本文件开头那句"这台机器没装 Windows SDK"是过期话，
// 只对**图标**那条历史成立（当年确实没 rc.exe，手写 .res 才留下）。
// 找不到 rc.exe 就跳过这条资源并 cargo:warning 出声（图标那条照旧手写，不受影响）。
// ---------------------------------------------------------------------------

/// `0.5.0` / `0.5.0.3` → `[0,5,0,3]`：FILEVERSION 那四个数与文本串同一个出处。
fn version_parts(text: &str) -> [u32; 4] {
    let mut n = [0u32; 4];
    for (i, piece) in text.split('.').take(4).enumerate() {
        n[i] = piece.trim().parse().unwrap_or(0);
    }
    n
}

/// 交给 rc.exe 的 .rc 文本。不 `#include <winver.h>`：那几个宏（`VOS_NT_WINDOWS32` / `VFT_APP` /
/// flagsmask）写成字面量就行，省掉一条"rc 得能找到 SDK 头文件"的路径依赖。纯 ASCII。
///
/// ⚠ 但 `VS_VERSION_INFO` 必须自己 `#define` 成 1：它在头文件里本来是个宏，不定义就被 rc 当成一条
/// **字符串资源名**（实测 .res 里记的是 `type=16 name="VS_VERSION_INFO"`），而 `GetFileVersionInfo`
/// 找的是 `RT_VERSION` + **ID 1** —— 资源明明进了镜像，API 却回 `ERROR_NOT_FOUND`(1813)、
/// `GetFileVersionInfoSize` 返 0，右键属性仍然空白。
fn version_rc(version_text: &str, exe_name: &str, parts: [u32; 4]) -> String {
    let nums = parts.map(|p| p.to_string()).join(",");
    format!(
        "#define VS_VERSION_INFO 1\n VS_VERSION_INFO VERSIONINFO\n \
         FILEVERSION {nums}\n PRODUCTVERSION {nums}\n FILEFLAGSMASK 0x3fL\n FILEFLAGS 0x0L\n \
         FILEOS 0x40004L\n FILETYPE 0x1L\n FILESUBTYPE 0x0L\nBEGIN\n \
         BLOCK \"StringFileInfo\"\n BEGIN\n \
         BLOCK \"040904b0\"\n BEGIN\n \
         VALUE \"CompanyName\", \"FocusFlow\"\n \
         VALUE \"FileDescription\", \"FocusFlow\"\n \
         VALUE \"FileVersion\", \"{ver}\"\n \
         VALUE \"OriginalFilename\", \"{exe}\"\n \
         VALUE \"ProductName\", \"FocusFlow\"\n \
         VALUE \"ProductVersion\", \"{ver}\"\n \
         END\n END\n \
         BLOCK \"VarFileInfo\"\n BEGIN\n VALUE \"Translation\", 0x409, 1200\n END\nEND\n",
        nums = nums,
        ver = version_text,
        exe = exe_name
    )
}

/// 某个 SDK 根目录下版本号最高的 `rc.exe`（优先 x64，退回 x86；老 SDK 也支持 bin\rc.exe 直放）。
fn rc_under(sdk_root: &Path) -> Option<PathBuf> {
    let bin = sdk_root.join("bin");
    let mut vers: Vec<String> = std::fs::read_dir(&bin)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    vers.sort();
    vers.reverse();
    for v in vers {
        for arch in ["x64", "x86"] {
            let p = bin.join(&v).join(arch).join("rc.exe");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    let flat = bin.join("rc.exe");
    flat.is_file().then_some(flat)
}

/// 找 rc.exe：`RC`（显式指定）→ `WindowsSdkDir`（vcvars 里会有）→ 注册表 `KitsRoot10`
/// （这台机器指到 `E:\develop\Windows Kits\10`，Program Files 那条默认路径并不存在）→ 默认路径。
///
/// 注册表输出里路径是 ASCII，中文标签走的是代码页、不影响取值，所以按行找 `InstallationFolder`
/// 后面那条 `REG_SZ` 的最后一个字段即可。
fn find_rc_exe() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("RC") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(d) = std::env::var_os("WindowsSdkDir") {
        roots.push(PathBuf::from(d));
    }
    let hk = [
        r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Microsoft SDKs\Windows\v10.0",
        r"HKLM\SOFTWARE\Microsoft\Microsoft SDKs\Windows\v10.0",
    ];
    for key in hk {
        let out = std::process::Command::new("reg")
            .args(["query", key])
            .output()
            .ok()?;
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if line.contains("InstallationFolder") && line.contains("REG_SZ") {
                if let Some(v) = line.rsplit("REG_SZ").next() {
                    let t = v.trim();
                    if !t.is_empty() {
                        roots.push(PathBuf::from(t));
                    }
                }
            }
        }
    }
    for default in [
        r"C:\Program Files (x86)\Windows Kits\10",
        r"C:\Program Files\Windows Kits\10",
    ] {
        roots.push(PathBuf::from(default));
    }
    roots.iter().find_map(|r| rc_under(r))
}

/// 用 rc.exe 生成版本资源，并**并入同一份 .res**。
///
/// 为什么要并成一份：图标那条单独一份 .res 时链接器认（`RT_GROUP_ICON` 读得出来），
/// 而"两份 .res 各一条链接参数"这种给法，版本资源在 exe 里查不到 ——
/// `GetFileVersionInfoSizeW` 返回 0、`GetLastError` = 1813 `ERROR_NOT_FOUND`，
/// 尽管 rc 那份记录的字节确实躺在镜像里。合成一份交给链接器就绕开了这个合并问题。
///
/// 找不到 rc.exe 或它编译失败就返回空（图标那条不受影响），并 cargo:warning 出声。
fn embed_version(manifest_dir: &Path, version_text: &str) -> Vec<u8> {
    let Some(rc) = find_rc_exe() else {
        println!("cargo:warning=没找到 rc.exe（可设 RC 或 WindowsSdkDir），exe 将没有版本资源");
        return Vec::new();
    };
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let rc_path = out_dir.join("focusflow_version.rc");
    let res_path = out_dir.join("focusflow_version.res");
    let exe_name = format!("{}.exe", bin_name(manifest_dir));
    if let Err(e) = std::fs::write(
        &rc_path,
        version_rc(version_text, &exe_name, version_parts(version_text)),
    ) {
        println!("cargo:warning=写 {} 失败：{e}", rc_path.display());
        return Vec::new();
    }
    let ok = std::process::Command::new(&rc)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_path)
        .arg(&rc_path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        println!(
            "cargo:warning={} 编译 {} 失败，exe 将没有版本资源",
            rc.display(),
            rc_path.display()
        );
        return Vec::new();
    }
    let built = match std::fs::read(&res_path) {
        Ok(b) => b,
        Err(e) => {
            println!("cargo:warning=读 {} 失败：{e}", res_path.display());
            return Vec::new();
        }
    };
    // rc.exe 也照惯例以一条 32 字节的"空记录"开头 —— 我们那份已经有了，重复一条留给链接器判断没意义。
    if built.len() <= 32 {
        println!(
            "cargo:warning={} 短得不像一份 .res（{} 字节）",
            res_path.display(),
            built.len()
        );
        return Vec::new();
    }
    built[32..].to_vec()
}

/// `[[bin]] name` —— OriginalFilename 要照着它写，别在两个地方各抄一遍产物名。
fn bin_name(manifest_dir: &Path) -> String {
    let ok = std::fs::read_to_string(manifest_dir.join("Cargo.toml")).unwrap_or_default();
    let mut in_bin = false;
    for line in ok.lines() {
        let t = line.trim();
        if t == "[[bin]]" {
            in_bin = true;
            continue;
        }
        if in_bin {
            if t.starts_with('[') {
                break;
            }
            if let Some(rest) = t.strip_prefix("name") {
                if let Some(q) = rest.find('"') {
                    let after = &rest[q + 1..];
                    if let Some(end) = after.find('"') {
                        return after[..end].to_string();
                    }
                }
            }
        }
    }
    "FocusFlow".to_string()
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
    println!("cargo:rerun-if-changed=Cargo.toml");

    let mut res: Vec<u8> = Vec::new();
    push_null_entry(&mut res);
    // 版本资源跟图标互相独立：单独编一份 .res 交给链接器（找不到 rc.exe 就跳过并出声，
    // 图标那条手写路径不受影响）。版本号取 `CARGO_PKG_VERSION` —— `build_native.bat` 里的
    // VERSION 也是从 Cargo.toml 读的，两边同一个出处，装出来的 exe 才跟 setup.exe 对得上
    //（`accept-installer` 就盯着这件事）。
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    res.extend_from_slice(&embed_version(&manifest, &version));

    let entries = match read_ico(&ico) {
        Ok(e) if !e.is_empty() => e,
        other => {
            // 缺图标不该挡住编译：窗口图标本来就还有运行时那条路。
            // 但 .res 还是要写出去 —— 不然刚写进去的版本资源会跟着一起没了。
            println!(
                "cargo:warning=无法读取 {}（{:?}），exe 将没有图标资源",
                ico.display(),
                other.err()
            );
            write_res(&res);
            return;
        }
    };
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

    write_res(&res);
}

/// 落地 .res 并把链接参数递出去（`link.exe` 直接吃 .res）。
fn write_res(res: &[u8]) {
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("focusflow.res");
    if let Err(e) = std::fs::write(&out, res) {
        println!("cargo:warning=写 {} 失败：{e}", out.display());
        return;
    }
    println!("cargo:rustc-link-arg={}", out.display());
}
