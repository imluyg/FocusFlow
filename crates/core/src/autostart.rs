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
         $s = $ws.CreateShortcut('{}'); \
         $s.TargetPath = '{}'; \
         $s.Arguments = '--hidden'; \
         $s.WorkingDirectory = '{}'; \
         $s.Description = 'FocusFlow - 效率追踪器'; \
         $s.Save()",
        lnk.to_string_lossy(),
        exe.to_string_lossy(),
        workdir
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

/// 从 .lnk 二进制内容提取目标路径（ASCII 明文绝对路径）。
fn shortcut_target() -> Option<String> {
    let lnk = shortcut_path();
    if !lnk.is_file() {
        return None;
    }
    let data = std::fs::read(&lnk).ok()?;
    // 匹配盘符开头的绝对路径直到 .exe
    let text = String::from_utf8_lossy(&data);
    regex_like_paths(&text)
        .into_iter()
        .find(|m| std::path::Path::new(m).exists())
}

/// 从 .lnk 内容中找 `盘符:\...\xxx.exe` 形式的路径。
fn regex_like_paths(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        // 找 `盘符:` 后跟 `\`
        if (b.is_ascii_alphabetic())
            && bytes.get(i + 1) == Some(&b':')
            && bytes.get(i + 2) == Some(&b'\\')
        {
            let mut j = i;
            while j < bytes.len() && bytes[j] != 0 && bytes[j] != b'\n' && bytes[j] != b'\r' {
                j += 1;
            }
            let candidate = &text[i..j];
            if candidate.contains(".exe") {
                out.push(candidate.to_string());
            }
        }
    }
    out
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
