//! 开机自启模块（Windows）。
//!
//! 镜像 Python 版 `autostart.py`：使用"启动文件夹"快捷方式。
//! - 启用：在启动文件夹创建 FocusFlow.lnk（指向 exe，带 --hidden 参数）
//! - 禁用：删除该快捷方式
//! - 兼容清理：删除旧版注册表 Run 键与 StartupApproved 标记

use std::path::PathBuf;

use crate::paths;

const SHORTCUT_NAME: &str = "FocusFlow.lnk";
const RUN_REG_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const STARTUP_APPROVED_PATH: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

fn startup_dir() -> PathBuf {
    if let Ok(appdata) = std::env::var("APPDATA") {
        PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs\Startup")
    } else {
        PathBuf::from(std::env::var("USERPROFILE").unwrap_or_else(|_| ".".to_string()))
            .join(r"Microsoft\Windows\Start Menu\Programs\Startup")
    }
}

fn shortcut_path() -> PathBuf {
    startup_dir().join(SHORTCUT_NAME)
}

/// 当前 exe 路径（后续打包版用 current_exe）。
fn exe_path() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| paths::app_dir().join("focusflow-app.exe"))
}

/// PowerShell 单引号字符串字面量。
///
/// 内部的单引号必须翻倍：路径里一个 `'` 就足以提前结束字符串，把它后面的内容
/// 当命令执行（`C:\Users\D'Angelo\FocusFlow\FocusFlow.exe` 这种带撇号的用户名
/// 并不罕见 —— 那时不只是自启动静默失败，而是把整段 `-Command` 改了形状）。
fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// 后台静默运行 PowerShell 创建快捷方式。
fn create_shortcut() -> anyhow::Result<PathBuf> {
    let exe = exe_path();
    let workdir = exe
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let lnk = shortcut_path();
    std::fs::create_dir_all(lnk.parent().unwrap()).ok();
    let ps_cmd = format!(
        "$ws = New-Object -ComObject WScript.Shell; \
         $s = $ws.CreateShortcut({}); \
         $s.TargetPath = {}; \
         $s.Arguments = '--hidden'; \
         $s.WorkingDirectory = {}; \
         $s.Description = 'FocusFlow - 效率追踪器'; \
         $s.Save()",
        ps_quote(&lnk.to_string_lossy()),
        ps_quote(&exe.to_string_lossy()),
        ps_quote(&workdir),
    );
    let mut cmd = std::process::Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-WindowStyle",
        "Hidden",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        &ps_cmd,
    ]);
    // Windows 下隐藏控制台窗口（CREATE_NO_WINDOW）
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    let output = cmd.output()?;
    if output.status.success() {
        Ok(lnk)
    } else {
        anyhow::bail!(
            "PowerShell 创建快捷方式失败: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

/// 从 .lnk 读出它指向的目标程序路径。
///
/// 用与调度器同一个 MS-SHLLINK 解析器（见 `scheduler::parse_lnk_target`）。
/// 早先这里是在文件字节里 grep「盘符:\ … .exe」的 ASCII 明文，代价是非 ASCII 安装目录
/// （`D:\软件\FocusFlow`）的 LocalBasePath 是 GBK/UTF-16 字节，grep 什么都找不到 ——
/// 表现是自启动明明开着，设置页却显示"未启用"，而且本地文件里的路径含 NUL 分隔的
/// 多段文本，第一个"看起来存在"的候选也未必是链接目标。
fn shortcut_target() -> Option<String> {
    shortcut_target_at(&shortcut_path())
}

fn shortcut_target_at(lnk: &std::path::Path) -> Option<String> {
    let data = std::fs::read(lnk).ok()?;
    crate::scheduler::parse_lnk_target(&data)
}

fn shortcut_points_to_exe() -> bool {
    let target = shortcut_target();
    match target {
        Some(t) => {
            let norm = |p: &str| {
                std::fs::canonicalize(p)
                    .unwrap_or_else(|_| PathBuf::from(p))
                    .to_string_lossy()
                    .to_lowercase()
            };
            norm(&t) == norm(&exe_path().to_string_lossy())
        }
        None => false,
    }
}

/// 清理旧版注册表方案（Run 键 + StartupApproved 标记）。
fn remove_legacy_registry() {
    for path in [RUN_REG_PATH, STARTUP_APPROVED_PATH] {
        let _ = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
            .open_subkey_with_flags(path, winreg::enums::KEY_SET_VALUE)
            .and_then(|key| key.delete_value("FocusFlow"));
    }
}

/// 是否已启用开机自启。
pub fn is_autostart_enabled() -> bool {
    if shortcut_points_to_exe() {
        return true;
    }
    // 兼容旧版本：Run 键仍存在且指向当前 exe
    if let Ok(key) = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_REG_PATH, winreg::enums::KEY_READ)
    {
        if let Ok(val) = key.get_value::<String, _>("FocusFlow") {
            return val
                .to_lowercase()
                .contains(&exe_path().to_string_lossy().to_lowercase());
        }
    }
    false
}

/// 启用开机自启，返回 (成功, 消息)。
pub fn enable_autostart() -> (bool, String) {
    match create_shortcut() {
        Ok(lnk) => {
            remove_legacy_registry();
            tracing::info!("已启用开机自启: {}", lnk.display());
            (true, "已启用开机自启，开机后将自动后台运行".to_string())
        }
        Err(e) => {
            let msg = format!("启用开机自启失败: {e}");
            tracing::error!("{msg}");
            (false, msg)
        }
    }
}

/// 禁用开机自启，返回 (成功, 消息)。
pub fn disable_autostart() -> (bool, String) {
    let lnk = shortcut_path();
    let r1 = if lnk.exists() {
        std::fs::remove_file(&lnk).is_ok()
    } else {
        true
    };
    remove_legacy_registry();
    if r1 {
        tracing::info!("已取消开机自启");
        (true, "已取消开机自启".to_string())
    } else {
        (false, "删除启动快捷方式失败".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 撇号必须翻倍，否则它会提前闭合 PowerShell 的字符串字面量。
    #[test]
    fn ps_quote_doubles_inner_single_quotes() {
        assert_eq!(ps_quote(r"C:\a\b.exe"), "'C:\\a\\b.exe'");
        assert_eq!(
            ps_quote(r"C:\Users\D'Angelo\FocusFlow\FocusFlow.exe"),
            r"'C:\Users\D''Angelo\FocusFlow\FocusFlow.exe'"
        );
        // 翻倍之后整段里只剩首尾两个真正的引号定界符
        let q = ps_quote("a'; Write-Host pwned; 'b");
        assert_eq!(q.matches("''").count(), 2, "原文两个撇号各自翻倍: {q}");
        assert!(q.starts_with('\'') && q.ends_with('\'') && q.len() > 2);
        let inner = &q[1..q.len() - 1];
        assert!(
            !inner.starts_with('\'') && !inner.ends_with('\''),
            "定界符不该与内容粘连: {q}"
        );
    }

    /// 真实快捷方式必须能读出目标 —— 旧实现是在字节里 grep ASCII 明文，
    /// 非 ASCII 目录与 UTF-16 字段都会读空。
    #[test]
    fn shortcut_target_reads_real_shell_links() {
        let roots: Vec<PathBuf> = ["APPDATA", "ProgramData"]
            .iter()
            .filter_map(|k| {
                std::env::var_os(k)
                    .map(|v| PathBuf::from(v).join(r"Microsoft\Windows\Start Menu\Programs"))
            })
            .filter(|p| p.is_dir())
            .collect();
        let mut stack = roots;
        let mut checked = 0usize;
        let mut hits = 0usize;
        while let Some(dir) = stack.pop() {
            if checked >= 15 {
                break;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                let is_lnk = p
                    .extension()
                    .and_then(|x| x.to_str())
                    .map(|x| x.eq_ignore_ascii_case("lnk"))
                    .unwrap_or(false);
                if !is_lnk {
                    continue;
                }
                checked += 1;
                if let Some(t) = shortcut_target_at(&p) {
                    hits += 1;
                    assert!(
                        std::path::Path::new(&t).is_absolute(),
                        "解析出的目标必须是绝对路径: {t}"
                    );
                }
            }
        }
        if checked == 0 {
            return; // 这台机器上没有开始菜单
        }
        assert!(
            hits * 2 >= checked,
            "真实快捷方式大多应能读出目标：{checked} 个里只读到 {hits} 个"
        );
    }

    /// 不是 shell link 的文件（旧版注册表残留、半个文件）只能得到 None，不能 panic。
    #[test]
    fn shortcut_target_rejects_non_shell_links() {
        let _lock = crate::paths::test_app_dir_lock();
        let dir = crate::paths::test_app_dir("autostart_junk");
        let p = dir.path().join("junk.lnk");
        std::fs::write(&p, b"not a shell link at all").unwrap();
        assert_eq!(shortcut_target_at(&p), None);
        std::fs::write(&p, []).unwrap();
        assert_eq!(shortcut_target_at(&p), None);
        assert_eq!(shortcut_target_at(&dir.path().join("missing.lnk")), None);
    }
}
