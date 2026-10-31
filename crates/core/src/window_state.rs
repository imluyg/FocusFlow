//! 主窗口几何的持久化（`window_state.ini`）。
//!
//! 为什么单独一个文件：这份状态"拖一次写一次"，而 `config.ini` 是用户手改的配置。
//! Python 版起窗口几何就放在 `window_state.ini`（见 [`crate::paths::window_state_path`]），
//! 这里沿用同一处，免得换一个版本就挪一次文件。
//!
//! 存的都是**逻辑像素**（DIP）加上"量这组坐标时那块屏的缩放"，与悬浮窗 `[floating] pos_x/pos_y`
//! 同一口径。还原时按存档里那个缩放换回物理像素 —— 不靠"建完窗再问 DPI"：多屏缩放不同时，
//! 逻辑点可能先落在另一块屏上，问到的就是另一块的缩放，窗口会跑到错误的屏、错误的尺寸。
//!
//! 文件整份由程序重写（不带注释、不保留额外键）：它不是给人编辑的配置。

use std::path::Path;

use anyhow::Context;

use crate::paths;

/// 尺寸下限：比这更小的存档当坏数据 —— 小到抓不住的矩形等于没有位置。
const MIN_W: f64 = 320.0;
const MIN_H: f64 = 240.0;
/// 尺寸上限：够到 8K 竖屏都绰绰有余；更大的数只可能是坏数据或被人手改坏了。
const MAX_W: f64 = 20000.0;
const MAX_H: f64 = 20000.0;
/// 坐标上限：多屏拼出来的虚拟桌面也在几千以内；再离谱就回默认位置。
const COORD_LIMIT: f64 = 100_000.0;

/// 主窗口上一次的矩形与最大化状态（逻辑像素 + 存盘时那块屏的缩放）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MainWindowState {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
    /// 记下这组逻辑坐标时窗口所在显示器的缩放倍数（96 DPI = 1.0）。
    ///
    /// 还原要按它换回物理像素。不能"建完窗再问 DPI"：多屏缩放不同时，逻辑坐标可能先落在
    /// 另一块屏上，问到的就是另一块的缩放，窗口会还原到错误的屏、错误的尺寸。
    pub scale: f64,
    pub maximized: bool,
}

impl MainWindowState {
    /// 没存过 / 存档坏了时用的矩形：与接这根线之前建窗那组写死值一致（(40, 40) 1200×860）。
    pub fn defaults() -> Self {
        Self {
            left: 40.0,
            top: 40.0,
            width: 1200.0,
            height: 860.0,
            scale: 1.0,
            maximized: false,
        }
    }

    /// 存档能不能直接用。坐标允许负值（副屏在主屏左边/上边），但尺寸、缩放与量级都要讲得通：
    /// 宁可回默认位置，也不要把窗口放到一个再也点不到的地方。
    pub fn is_sane(&self) -> bool {
        let all_finite = [self.left, self.top, self.width, self.height, self.scale]
            .iter()
            .all(|v| v.is_finite());
        all_finite
            && self.left.abs() <= COORD_LIMIT
            && self.top.abs() <= COORD_LIMIT
            && (MIN_W..=MAX_W).contains(&self.width)
            && (MIN_H..=MAX_H).contains(&self.height)
            && (0.5..=8.0).contains(&self.scale)
    }
}

/// 读程序目录下那份 `window_state.ini`。没有文件、读不动、内容不完整或数值不合理都返回
/// `None`（调用方回默认矩形），不报错 —— 它只是"上次的位置"，不是必须存在的数据。
pub fn load() -> Option<MainWindowState> {
    load_from(&paths::window_state_path())
}

/// 从指定路径读（测试用；生产侧走 [`load`]）。
pub fn load_from(path: &Path) -> Option<MainWindowState> {
    let text = std::fs::read_to_string(path).ok()?;
    parse(&text).filter(MainWindowState::is_sane)
}

/// 写回程序目录下那份 `window_state.ini`（原子替换，掉电不会留下半截文件）。
pub fn save(state: &MainWindowState) -> anyhow::Result<()> {
    save_to(&paths::window_state_path(), state)
}

/// 写到指定路径（测试用；生产侧走 [`save`]）。
pub fn save_to(path: &Path, state: &MainWindowState) -> anyhow::Result<()> {
    crate::config::atomic_write(path, &render(state))
        .with_context(|| format!("写窗口状态文件失败：{}", path.display()))
}

fn render(state: &MainWindowState) -> String {
    format!(
        "[main]\nleft = {}\ntop = {}\nwidth = {}\nheight = {}\nscale = {}\nmaximized = {}\n",
        state.left.round() as i64,
        state.top.round() as i64,
        state.width.round() as i64,
        state.height.round() as i64,
        state.scale,
        u8::from(state.maximized),
    )
}

/// 只认 `[main]` 段里的那几个键；缺一个尺寸或坐标的，整份当坏数据。
///
/// 解析宽容的是**书写形态**（BOM、CRLF、空行、注释、未知键、行内空白），不是数值本身：
/// 半份存档（比如只有 left/top）会落回默认矩形，而不是拼一个"左半边是旧的、右半边是默认"的窗口。
/// `scale` 缺省按 1.0（100%）算：手写存档没写它时，按物理像素原样还原。
fn parse(text: &str) -> Option<MainWindowState> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut section = "";
    let mut left = None;
    let mut top = None;
    let mut width = None;
    let mut height = None;
    let mut scale = 1.0;
    let mut maximized = false;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            section = name.trim();
            continue;
        }
        if !section.eq_ignore_ascii_case("main") {
            continue;
        }
        let Some(eq) = line.find('=') else {
            continue;
        };
        let key = line[..eq].trim().to_ascii_lowercase();
        let value = line[eq + 1..].trim();
        match key.as_str() {
            "left" => left = value.parse::<f64>().ok(),
            "top" => top = value.parse::<f64>().ok(),
            "width" => width = value.parse::<f64>().ok(),
            "height" => height = value.parse::<f64>().ok(),
            "scale" => scale = value.parse::<f64>().unwrap_or(1.0),
            "maximized" => {
                maximized = matches!(
                    value.to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            }
            _ => {}
        }
    }

    Some(MainWindowState {
        left: left?,
        top: top?,
        width: width?,
        height: height?,
        scale,
        maximized,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_rect_comes_back_unchanged() {
        let _lock = paths::test_app_dir_lock();
        let dir = paths::test_app_dir("win_state_roundtrip");
        let path = dir.path().join("window_state.ini");
        // 负坐标是合法存档：副屏挂在主屏左边时窗口的 left 就是负数。
        let st = MainWindowState {
            left: -1920.0,
            top: 137.0,
            width: 1100.0,
            height: 720.0,
            scale: 1.5,
            maximized: true,
        };
        save_to(&path, &st).unwrap();
        assert_eq!(load_from(&path), Some(st));
    }

    #[test]
    fn hand_written_noise_around_the_numbers_is_tolerated() {
        let _lock = paths::test_app_dir_lock();
        let dir = paths::test_app_dir("win_state_noise");
        let path = dir.path().join("window_state.ini");
        std::fs::write(
            &path,
            "\u{feff}; 这是程序自己写的文件\r\n\r\n[main]\r\nleft = 300\r\nunknown = whatever\r\n\
             top = 120\r\nwidth = 1000\r\nheight = 700\r\nmaximized = true\r\n",
        )
        .unwrap();
        assert_eq!(
            load_from(&path),
            Some(MainWindowState {
                left: 300.0,
                top: 120.0,
                width: 1000.0,
                height: 700.0,
                // 手写存档没写 scale：按 100% 原样还原，别把窗口缩成一块。
                scale: 1.0,
                maximized: true,
            })
        );
        // 别的节里的同名键不算数
        std::fs::write(&path, "[other]\nleft = 1\ntop = 2\nwidth = 3\nheight = 4\n").unwrap();
        assert_eq!(load_from(&path), None);
    }

    #[test]
    fn a_missing_or_half_written_file_falls_back_instead_of_guessing() {
        let _lock = paths::test_app_dir_lock();
        let dir = paths::test_app_dir("win_state_partial");
        let path = dir.path().join("window_state.ini");
        assert_eq!(load_from(&path), None, "没有文件时不该编一组坐标出来");
        std::fs::write(&path, "[main]\nleft = 300\ntop = 120\n").unwrap();
        assert_eq!(load_from(&path), None, "半份存档拼不出窗口，宁可回默认");
        std::fs::write(
            &path,
            "[main]\nleft = 300\ntop = 120\nwidth = abc\nheight = 700\n",
        )
        .unwrap();
        assert_eq!(load_from(&path), None, "不是数字的尺寸不能当 0 用");
    }

    #[test]
    fn insane_rects_are_rejected_so_the_window_cannot_land_out_of_reach() {
        let base = MainWindowState::defaults();
        assert!(base.is_sane());
        assert!(!MainWindowState {
            width: 10.0,
            ..base
        }
        .is_sane());
        assert!(!MainWindowState {
            height: 0.0,
            ..base
        }
        .is_sane());
        assert!(!MainWindowState {
            width: f64::NAN,
            ..base
        }
        .is_sane());
        assert!(!MainWindowState { scale: 0.0, ..base }.is_sane());
        assert!(!MainWindowState {
            scale: f64::NAN,
            ..base
        }
        .is_sane());
        assert!(!MainWindowState {
            left: 1.0e9,
            ..base
        }
        .is_sane());
    }
}
