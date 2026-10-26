//! 原生悬浮窗的**可测部分**：数字格式、版面、逐像素合成。
//!
//! 为什么把这个模块单独拆出来（`native.rs` 里全是 Win32 调用，门禁开不了窗口）：
//! 卡片上那几个字与它们的透明度是这条路径唯一的用户可见产物，而它最容易被写成
//! "只在真机上看得出来对不对"。逐像素合成、格式串、版面坐标都在这里，
//! 每条都能在 `cargo test` 里钉住；`native.rs` 只负责把它们搬到屏幕上。
//!
//! 与 `desktop/ui/floating.html` + `floating.js` 的对应关系：
//! - 卡片底色 `rgba(255,255,255,.72)`、边框 `rgba(224,233,248,.95)`（深色另套），圆角 8px，
//!   内边距 4/8px，行高 1.35 × 13px，标签列宽 26px；
//! - `活跃 <次数>` / `时长 <时长>`（仅 metric=both）/ `速度 <cpm>/分`；
//! - 暂停：整张卡片 `opacity: .55` + 右上角「已暂停」角标（`--warning` 色）；
//! - 久坐提醒：贴在顶部的一条 `--warning` 色横带，`shown_seconds` 后消失。
//! - **不含** backdrop blur：那是系统材质（Mica/Acrylic），分层窗口拿不到。
//!   这是原生化唯一确定的观感差别，写在台账里，不假装等价。

/// 一像素 = 非预乘 BGRA 里的 rgb + alpha（合成阶段用抽象名，不落字节序）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
}

/// 卡片配色。数值逐条抄自 `floating.html` 的内联样式与 `ui/tokens.css`，
/// 有一条用例 [`crate::floating::tests`] 回头去读那两个文件比对，防止改一边忘一边。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub card: Color,
    pub border: Color,
    pub fg: Color,
    pub muted: Color,
    pub accent: Color,
    pub warning: Color,
}

pub const LIGHT: Palette = Palette {
    card: Color::rgba(255, 255, 255, 184), // rgba(255,255,255,.72)
    border: Color::rgba(224, 233, 248, 242), // rgba(224,233,248,.95)
    fg: Color::rgba(26, 26, 46, 255),      // --fg #1A1A2E
    muted: Color::rgba(74, 74, 106, 255),  // --muted #4A4A6A
    accent: Color::rgba(77, 140, 247, 255), // --accent #4D8CF7
    warning: Color::rgba(245, 158, 11, 255), // --warning #F59E0B
};

pub const DARK: Palette = Palette {
    card: Color::rgba(30, 35, 49, 184),      // rgba(30,35,49,.72)
    border: Color::rgba(43, 50, 70, 242),    // rgba(43,50,70,.95)
    fg: Color::rgba(232, 234, 238, 255),     // --fg #E8EAEE
    muted: Color::rgba(138, 146, 166, 255),  // --muted #8A92A6
    accent: Color::rgba(91, 156, 247, 255),  // --accent #5B9CF7
    warning: Color::rgba(251, 191, 36, 255), // --warning #FBBF24
};

pub fn palette_for(theme: &str) -> Palette {
    if theme == "dark" {
        DARK
    } else {
        LIGHT
    }
}

/// 悬浮窗显示口径（`[gui] floating_metric`）。与前端 `floating.js:112` 那一组字面量一致。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Metric {
    Times,
    Duration,
    Both,
}

pub fn metric_from(value: &str) -> Metric {
    match value {
        "duration" => Metric::Duration,
        "both" => Metric::Both,
        // 前端把任何别的值都当 "times"（`floating.js:131` 只在 duration/both 时改），保持一致。
        _ => Metric::Times,
    }
}

/// `utils.js::fmt` —— `Number(n||0).toLocaleString("zh-CN")`，即按千位加逗号、负数保留。
///
/// 只覆盖这条路径实际会喂进去的量（计数非负、`i64`）；`0` 与"读不到"都渲染成 `0`，
/// 与 `Number(n || 0)` 的 `0→0`、`null→0` 同一口径。
pub fn fmt_count(n: i64) -> String {
    let sign = if n < 0 { "-" } else { "" };
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    out.push_str(sign);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `utils.js::fmtDuration` 逐字对齐：`<60 → N秒`；`h>0 → N小时M分`；否则 `N分钟`。
/// 负数与小数按前端那样先夹到 `max(0, floor(x))`。
pub fn fmt_duration(sec: i64) -> String {
    let sec = sec.max(0);
    if sec < 60 {
        return format!("{sec}秒");
    }
    let h = sec / 3600;
    let m = (sec % 3600) / 60;
    if h > 0 {
        format!("{h}小时{m}分")
    } else {
        format!("{m}分钟")
    }
}

/// 一条要画在卡片上的行：标签 + 值。`role` 决定值用哪个色（`--fg` 还是 `--accent`）。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Row {
    pub label: &'static str,
    pub value: String,
    pub accent: bool,
    /// `速度` 那行值后面还跟着 `/分` 两个小字号字符，只有这一行有。
    pub suffix: Option<&'static str>,
}

/// 卡片当前要说什么。由调用方（`native.rs` 的每一拍）从 parked 的 LiveStats 拼出来。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PillContent {
    pub rows: Vec<Row>,
    pub paused: bool,
    /// 久坐提醒那条横带的文字；`None` = 没有提醒或已过期（不许念旧账）。
    pub notice: Option<String>,
}

pub fn build_content(
    metric: Metric,
    today_count: i64,
    cpm: i64,
    active_seconds: i64,
    paused: bool,
    notice: Option<String>,
) -> PillContent {
    let duration = fmt_duration(active_seconds);
    let mut rows = Vec::with_capacity(3);
    match metric {
        Metric::Times => rows.push(Row {
            label: "活跃",
            value: fmt_count(today_count),
            accent: false,
            suffix: None,
        }),
        Metric::Duration => rows.push(Row {
            label: "活跃",
            value: duration.clone(),
            accent: false,
            suffix: None,
        }),
        Metric::Both => {
            rows.push(Row {
                label: "活跃",
                value: fmt_count(today_count),
                accent: false,
                suffix: None,
            });
            rows.push(Row {
                label: "时长",
                value: duration,
                accent: false,
                suffix: None,
            });
        }
    }
    rows.push(Row {
        label: "速度",
        value: fmt_count(cpm),
        accent: true,
        suffix: Some("/分"),
    });
    PillContent {
        rows,
        paused,
        notice,
    }
}

/// 通知横带的原文案（前端 `floating.js:155` 同一句，两处尺子会同桌吃饭）。
pub fn notice_text(window_minutes: i64, rest_seconds: i64) -> String {
    format!("久坐提醒：已连用 {window_minutes} 分钟，起来活动 {rest_seconds} 秒")
}

// ===== 版面 =====

/// CSS 侧的固定量（逻辑像素）。`line-height: 1.35` × `font-size: 13px` ≈ 17.55 → 取 18，
/// 与浏览器实际占位差不到 1px；卡片 `padding: 4px 8px`、圆角 8、边框 1、标签列 26、列间距 2。
pub const PAD_X: i32 = 8;
pub const PAD_Y: i32 = 4;
pub const ROW_H: i32 = 18;
pub const LABEL_W: i32 = 26;
pub const GAP: i32 = 2;
pub const RADIUS: i32 = 8;
pub const BORDER: i32 = 1;
pub const FONT_PX: i32 = 13;
/// 暂停角标与通知带的字号（CSS 里 9px / 11px）。
pub const NOTICE_FONT_PX: i32 = 11;
pub const BADGE_FONT_PX: i32 = 9;

/// 内容需要的最小高度（逻辑像素）：通知横带在时多占一行，行数随行数变。
///
/// 为什么不能像 WebView2 那样"内容溢出去好了"：分层窗口没有滚动条，画不下就是**看不见**，
/// 而 `metric=both` 或提醒出现时行数会变。所以尺寸由内容决定，再与配置值取大。
pub fn content_height(rows: usize, has_notice: bool) -> i32 {
    let body = rows as i32 * ROW_H + 2 * PAD_Y + 2 * BORDER;
    let notice = if has_notice { ROW_H } else { 0 };
    body + notice
}

/// 卡片有几行。`metric` 决定上面那一到两行，「速度」那一行永远在。
///
/// 单独一个函数是为了让"窗口该多高"这件事不必先把内容拼一遍才知道行数 ——
/// WebView2 那条路的尺寸在 Rust 侧，而它拿得到配置、拿不到页面已经画完的版面。
pub fn row_count(metric: Metric) -> usize {
    match metric {
        Metric::Both => 3,
        Metric::Times | Metric::Duration => 2,
    }
}

/// 悬浮窗该有多高（逻辑像素）：配置值是**下限**，内容多一行就要多一行。
///
/// 这一条过去只有原生那一侧在做（`repaint` 里 `max(配置高, content_height)`）。
/// WebView2 那一侧照配置值写死 46，于是 `metric=both` 的第三行与久坐提醒横带出现时，
/// 多出来的那一行被窗口自己的下边缘裁掉 —— 而那正是原生这条要避免的症状
/// （`ui/floating.html` 的 `overflow:hidden` 与分层窗口一样，画不下就是看不见）。
/// 两边都问这里，尺寸这一件事才只有一把尺子。
///
/// `rows` 由调用方给：原生那一侧拿的是已经拼好的内容（`content.rows.len()`），
/// WebView2 那一侧只拿得到配置，于是走 [`row_count`]。两条都汇到 [`content_height`]。
pub fn pill_height(cfg_height: f64, rows: usize, has_notice: bool) -> i32 {
    let floor = cfg_height.max(24.0).round() as i32;
    floor.max(content_height(rows, has_notice))
}

/// 卡片长高之后 top 该落在哪（同一单位内的纯几何，i32/f64 由各条路径自己换算）。
///
/// 长出来的那一行是往**下**长的：贴在屏幕底边放好的卡片（`pos_y = 1031` 在 1080 高的屏上
/// 就是按底边摆的）每来一次提醒就被下边缘切掉最后一行，而用户只会看见"那一行没了"。
/// 所以下边缘不够就把整张往上让，横带消失后再回到锚点。
///
/// ⚠ `anchor` 必须是"用户放下的那一个 top"，不是当前窗口矩形里的 top：按矩形算就是
/// 让上去之后再也没人把它放回来 —— 每来一次提醒往上爬一截，那就是"悬浮窗自己跑位"。
/// 屏幕比卡片还矮时钉在 `mon_top`（把切掉的部分留给下边缘，但至少上半截全看得见）。
pub fn fit_top(anchor: f64, height: f64, mon_top: f64, mon_bottom: f64) -> f64 {
    anchor.min(mon_bottom - height).max(mon_top)
}

/// 一块矩形（左开右开，像素坐标，y 自上而下）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// 一行的三段矩形：标签、值、后缀。坐标已按 `scale` 放大成物理像素。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RowLayout {
    pub label: Rect,
    pub value: Rect,
    pub suffix: Option<Rect>,
}

/// 逻辑像素 → 物理像素。**逐值四舍五入**，不许把 scale 先取整：
/// 125%/150% 缩放下 `scale as i32` 恒等于 1，整张卡片会停在 100% 的大小、字也糊。
pub fn px(v: i32, scale: f64) -> i32 {
    (v as f64 * scale.max(0.5)).round() as i32
}

pub fn layout_rows(w: i32, scale: f64, count: usize, top_pad_rows: usize) -> Vec<RowLayout> {
    let pad_x = px(PAD_X, scale);
    let pad_y = px(PAD_Y, scale);
    let row_h = px(ROW_H, scale);
    let label_w = px(LABEL_W, scale);
    let gap = px(GAP, scale);
    let start_y = pad_y + (top_pad_rows as i32 * row_h);
    (0..count)
        .map(|i| {
            let y = start_y + i as i32 * row_h;
            let value_x = pad_x + label_w + gap;
            let rest = (w - value_x - pad_x).max(0);
            RowLayout {
                label: Rect {
                    x: pad_x,
                    y,
                    w: label_w,
                    h: row_h,
                },
                value: Rect {
                    x: value_x,
                    y,
                    w: rest,
                    h: row_h,
                },
                suffix: None,
            }
        })
        .collect()
}

/// 把 `速度 123/分` 那一行的后缀矩形从值矩形的右边让出来。
///
/// 为什么单独一步：`/分` 是 9px 小字，宽度与 13px 数字不同，只有这一行需要让位；
/// 全交给 `layout_rows` 会让每个调用点都懂"哪一行有后缀"，那是把渲染知识漏进版面函数。
pub fn reserve_suffix(value: Rect, scale: f64, suffix: &str) -> (Rect, Rect) {
    // 粗略按 0.9em × 字符数估算，末尾再让 1px。宁可让窄一点：值那一侧是 DrawText 的
    // 裁剪矩形，宽了会和后缀叠字，窄了只是末尾少一位数字。
    let est = (suffix.chars().count() as f64 * BADGE_FONT_PX as f64 * 0.9).ceil() as i32 + 1;
    let w = px(est, scale).min(value.w.max(1));
    let cut = (value.w - w).max(0);
    (
        Rect {
            x: value.x,
            y: value.y,
            w: cut,
            h: value.h,
        },
        Rect {
            x: value.x + cut,
            y: value.y,
            w: value.w - cut,
            h: value.h,
        },
    )
}

/// 通知带矩形（占满卡片宽度，落在顶部一行）。
pub fn notice_rect(w: i32, scale: f64) -> Rect {
    Rect {
        x: 0,
        y: 0,
        w,
        h: px(ROW_H, scale),
    }
}

/// 暂停角标矩形（右上角）。
pub fn badge_rect(w: i32, scale: f64) -> Rect {
    Rect {
        x: w - px(44, scale),
        y: px(2, scale),
        w: px(42, scale),
        h: px(BADGE_FONT_PX + 2, scale),
    }
}

// ===== 逐像素合成 =====
//
// 全程用**预乘**格式（`[r, g, b, a]`，rgb 已乘过 a），因为 `UpdateLayeredWindow` 配
// `AC_SRC_ALPHA` 要的就是预乘。合成公式是标准 source-over：
//   a' = a_s + a_d·(1−a_s)，rgb' = rgb_s + rgb_d·(1−a_s)
// 8 位定点下用 255 作分母，误差 ≤1/255，肉眼不可见，且能写成纯函数被测。

/// 圆角矩形在某像素处的覆盖率（0 或 `alpha`）。四角 1bit 锯齿：
/// 卡片只有 90×46，1px 圆角阶跃在缩放下看不出来，换来的是这条能单元测试。
///
/// 角区用的是圆心 `(r−1, r−1)`、半径 `r−1` 的**离散**四分之一圆：
/// 像素 `(0,0)` 在 r=8 时距离 √98 > 7 ⇒ 透明，`(3,3)` 距离 √32 ≤ 7 ⇒ 实心底。
/// 判据要钉的就是"角外透明、正中实心"这两头，不是弧线第几像素落在哪。
pub fn card_alpha_at(x: i32, y: i32, w: i32, h: i32, radius: i32, alpha: u8) -> u8 {
    if x < 0 || y < 0 || x >= w || y >= h {
        return 0;
    }
    let r = radius.clamp(0, (w / 2).min(h / 2));
    if r == 0 {
        return alpha;
    }
    // 离"最近的角心"超出了多少（负数 = 根本不在那一条角带上）
    let dx = if x < r {
        r - 1 - x
    } else if x >= w - r {
        x - (w - r)
    } else {
        -1
    };
    let dy = if y < r {
        r - 1 - y
    } else if y >= h - r {
        y - (h - r)
    } else {
        -1
    };
    if dx < 0 && dy < 0 {
        return alpha; // 直边带里，圆角管不着
    }
    let (ddx, ddy) = (dx.max(0) as i64, dy.max(0) as i64);
    let rr = (r - 1) as i64;
    if ddx * ddx + ddy * ddy <= rr * rr {
        alpha
    } else {
        0
    }
}

/// 把一层纯色（非预乘）按覆盖率打进预乘缓冲。`idx` 是像素下标。
pub fn put_premultiplied(buf: &mut [u8], idx: usize, color: Color, coverage: u8) {
    let a = coverage as u16;
    buf[idx * 4] = ((color.b as u16 * a) / 255) as u8;
    buf[idx * 4 + 1] = ((color.g as u16 * a) / 255) as u8;
    buf[idx * 4 + 2] = ((color.r as u16 * a) / 255) as u8;
    buf[idx * 4 + 3] = a as u8;
}

/// 目标像素已预乘；把源（预乘）叠上去，结果仍预乘。
pub fn over(dst: &mut [u8], di: usize, sr: u16, sg: u16, sb: u16, sa: u16) {
    let da = dst[di * 4 + 3] as u16;
    let dr = dst[di * 4] as u16;
    let dg = dst[di * 4 + 1] as u16;
    let db = dst[di * 4 + 2] as u16;
    let inv = 255 - sa;
    dst[di * 4] = ((sb * 255 + db * inv) / 255).min(255) as u8; // b
    dst[di * 4 + 1] = ((sg * 255 + dg * inv) / 255).min(255) as u8; // g
    dst[di * 4 + 2] = ((sr * 255 + dr * inv) / 255).min(255) as u8; // r
    dst[di * 4 + 3] = (sa + (da * inv) / 255).min(255) as u8; // a
}

/// 把 GDI 画出来的"白字黑底"覆盖度层叠进缓冲。
///
/// 为什么绕这一圈：GDI 往 32bpp DIB 里画字时会把 alpha 字节写成 0，
/// 于是"直接在卡片缓冲上 DrawText"得到的是一堆透明像素。改成
/// 单独一层：白色文字画在黑底上，R 通道就是覆盖度，再按色相乘叠上去 ——
/// 顺带让抗锯齿只能用灰度（`ANTIALIASED_QUALITY`），ClearType 的子像素彩色在这里会变成彩边。
pub fn overlay_text_coverage(buf: &mut [u8], text: &[u8], fg: Color) {
    for i in 0..(buf.len() / 4) {
        let cov = text[i * 4 + 2] as u16; // GDI 写的白色 → R 通道即覆盖度
        if cov == 0 {
            continue;
        }
        let sr = (fg.r as u16 * cov) / 255;
        let sg = (fg.g as u16 * cov) / 255;
        let sb = (fg.b as u16 * cov) / 255;
        over(buf, i, sr, sg, sb, cov);
    }
}

/// 整层乘一个不透明度（CSS `opacity` 的语义：预乘缓冲里 rgb 与 a 同比例缩）。
pub fn scale_opacity(buf: &mut [u8], factor_num: u16, factor_den: u16) {
    if factor_num >= factor_den {
        return;
    }
    for px in buf.as_chunks_mut::<4>().0 {
        px[0] = (px[0] as u16 * factor_num / factor_den) as u8;
        px[1] = (px[1] as u16 * factor_num / factor_den) as u8;
        px[2] = (px[2] as u16 * factor_num / factor_den) as u8;
        px[3] = (px[3] as u16 * factor_num / factor_den) as u8;
    }
}

/// 暂停态那个 `.55`。写成常量而不是散在调用点，是为了让判据能钉住它。
pub const PAUSED_NUM: u16 = 55;
pub const PAUSED_DEN: u16 = 100;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_group_by_three_like_the_frontend() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1000), "1,000");
        assert_eq!(fmt_count(1234567), "1,234,567");
        assert_eq!(fmt_count(-1500), "-1,500");
    }

    /// `utils.js::fmtDuration` 的四条边界，两处必须同一条尺子。
    #[test]
    fn duration_matches_fmtduration_at_every_boundary() {
        assert_eq!(fmt_duration(0), "0秒");
        assert_eq!(
            fmt_duration(-5),
            "0秒",
            "负数先夹到 0，与 Math.max(0, …) 一致"
        );
        assert_eq!(fmt_duration(59), "59秒");
        assert_eq!(fmt_duration(60), "1分钟");
        assert_eq!(fmt_duration(119), "1分钟");
        assert_eq!(fmt_duration(3599), "59分钟");
        assert_eq!(fmt_duration(3600), "1小时0分");
        assert_eq!(fmt_duration(3661), "1小时1分");
    }

    #[test]
    fn unknown_metric_falls_back_to_times_like_the_page() {
        assert_eq!(metric_from("times"), Metric::Times);
        assert_eq!(metric_from("duration"), Metric::Duration);
        assert_eq!(metric_from("both"), Metric::Both);
        assert_eq!(metric_from(""), Metric::Times);
        assert_eq!(
            metric_from("耗时"),
            Metric::Times,
            "前端只认两个字面量，其余一律 times"
        );
    }

    /// `metric=duration` 时「活跃」那一格换成长度，但**不会**多出第三行；
    /// 只有 both 才三行。行数直接决定卡片高度，画错是"卡片变高/数字被裁"。
    #[test]
    fn metric_decides_which_rows_exist_and_speed_is_always_last() {
        let t = build_content(Metric::Times, 1234, 45, 3700, false, None);
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.rows[0].value, "1,234");
        assert_eq!(t.rows[1].label, "速度");
        assert!(t.rows[1].accent);
        assert_eq!(t.rows[1].suffix, Some("/分"));

        let d = build_content(Metric::Duration, 1234, 45, 3700, false, None);
        assert_eq!(d.rows.len(), 2);
        assert_eq!(d.rows[0].value, "1小时1分");

        let b = build_content(Metric::Both, 1234, 45, 3700, false, None);
        assert_eq!(b.rows.len(), 3);
        assert_eq!(b.rows[1].label, "时长");
        assert_eq!(b.rows[2].label, "速度");
    }

    /// 卡片默认 90×46 —— 两行版面的高度必须**正好**等于配置默认值，
    /// 否则原生化第一天就把用户的悬浮窗撑大或缩一小截。
    #[test]
    fn two_row_layout_is_exactly_the_shipped_pill_height() {
        assert_eq!(content_height(2, false), 46);
        assert_eq!(content_height(3, false), 64, "metric=both 多一行");
        assert_eq!(content_height(2, true), 64, "久坐横带多占一行");
    }

    /// 窗口高度这一件事只有一处算法，两条渲染器都问它：配置值是**下限**，
    /// 内容多一行就多一行。写死配置值的那一半（WebView2）就是把第三行与提醒横带裁掉。
    #[test]
    fn the_window_height_is_a_floor_that_content_can_raise() {
        assert_eq!(row_count(Metric::Times), 2);
        assert_eq!(row_count(Metric::Duration), 2, "换口径不该多出一行");
        assert_eq!(row_count(Metric::Both), 3);

        assert_eq!(pill_height(46.0, 2, false), 46, "默认配置正好容下两行");
        assert_eq!(pill_height(46.0, 3, false), 64, "both 的第三行");
        assert_eq!(pill_height(46.0, 2, true), 64, "提醒横带的那一行");
        assert_eq!(pill_height(46.0, 3, true), 82, "两件事同时发生");
        assert_eq!(
            pill_height(200.0, 2, false),
            200,
            "人把卡片调大时就照他的，不许被内容顶回去"
        );
        assert_eq!(
            pill_height(f64::NAN, 2, false),
            46,
            "配置读不出来也要有个能看见的高度（max(24) 之后取内容值）"
        );
    }

    /// 长高之后卡片该待在哪：往下够就不动，不够就整张往上让，收回来时回到**锚点**。
    ///
    /// 第三条腿是这条规则的全部难点：让上去之后再按当前矩形算，卡片就每来一次提醒
    /// 往上爬一截 —— 那是"悬浮窗自己跑位"。单位由调用方决定（原生交物理像素、
    /// WebView2 交逻辑像素），所以这里只用抽象数。
    #[test]
    fn a_taller_card_moves_up_only_when_the_bottom_edge_is_in_the_way() {
        let (top, bottom) = (0.0, 1080.0);
        assert_eq!(
            fit_top(60.0, 64.0, top, bottom),
            60.0,
            "下方有空间就一个字都不动"
        );
        assert_eq!(
            fit_top(1031.0, 46.0, top, bottom),
            1031.0,
            "贴着底边放好的位置是用户放的"
        );
        assert_eq!(
            fit_top(1031.0, 64.0, top, bottom),
            1016.0,
            "1031+64=1095 越过 1080 ⇒ 往上让 15"
        );
        assert_eq!(
            fit_top(1031.0, 46.0, top, bottom),
            1031.0,
            "提醒收掉必须回到锚点，不是停在让过的那一个矩形"
        );
        assert_eq!(
            fit_top(30.0, 64.0, 0.0, 48.0),
            0.0,
            "屏幕只有 48 高而卡片要 64：钉在屏幕顶，不许把卡片推到屏幕之上"
        );
        // 上面那块屏（负原点）按它自己的边界算：它的下边缘就是 y = 0。
        assert_eq!(
            fit_top(-1070.0, 64.0, -1080.0, 0.0),
            -1070.0,
            "-1070+64=-1006 还在那块屏里，不动"
        );
        assert_eq!(
            fit_top(-20.0, 64.0, -1080.0, 0.0),
            -64.0,
            "越过那块屏自己的下边缘 0 ⇒ 让到 0-64，而不是让到主屏的 1080"
        );
    }

    /// 提醒条在页面里占的那一行高，必须与这里加的那一行高是同一个数。
    ///
    /// 两边的尺子一旦分家（CSS 改成 20 而 ROW_H 还是 18），Rust 加的高度就不够，
    /// 症状照旧是"最后一行被裁"，而这次连日志都没有 —— 所以回头读那两个文件对拍。
    #[test]
    fn the_notice_row_in_the_page_is_exactly_one_row_tall() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let js = std::fs::read_to_string(root.join("ui/floating.js"))
            .expect("读不到 ui/floating.js：判据的路径或文件名变了");
        let want = format!("height:{ROW_H}px;line-height:{ROW_H}px");
        assert!(
            js.contains(&want),
            "提醒条那一行的行高与 ROW_H({ROW_H}) 对不上，实得片段：{}",
            js.split("cssText")
                .nth(1)
                .unwrap_or("(页面里没有 cssText)")
                .chars()
                .take(220)
                .collect::<String>()
        );
        assert!(
            !js.contains("position:fixed;left:0;right:0;top:0"),
            "提醒条又变回压在数据上的覆盖层了：卡片是 overflow:hidden，盖住就是看不见"
        );
    }

    #[test]
    fn rows_stack_from_the_top_padding_and_notice_pushes_them_down() {
        let r = layout_rows(90, 1.0, 2, 0);
        assert_eq!(
            r[0].label,
            Rect {
                x: 8,
                y: 4,
                w: 26,
                h: 18
            }
        );
        assert_eq!(r[1].label.y, 22);
        assert_eq!(r[0].value.x, 36);
        let with_notice = layout_rows(90, 1.0, 2, 1);
        assert_eq!(with_notice[0].label.y, 22, "横带把两行整体压下去一行");

        let scaled = layout_rows(135, 1.5, 2, 0);
        assert_eq!(scaled[0].label.x, 12, "8 × 1.5");
        assert_eq!(scaled[0].label.h, 27, "18 × 1.5");
    }

    /// 后缀让位之后：两块矩形仍落在原值矩形里，且值那一侧不会被压成 0 宽。
    #[test]
    fn suffix_reserve_stays_inside_the_value_rect() {
        let v = Rect {
            x: 36,
            y: 4,
            w: 46,
            h: 18,
        };
        let (head, tail) = reserve_suffix(v, 1.0, "/分");
        assert_eq!(head.x, v.x);
        assert_eq!(head.w + tail.w, v.w, "让出来的宽度必须正好是后缀那一条");
        assert_eq!(tail.x, head.x + head.w);
        assert!(head.w > 0 && tail.w > 0);

        let narrow = Rect {
            x: 36,
            y: 4,
            w: 3,
            h: 18,
        };
        let (head, tail) = reserve_suffix(narrow, 1.0, "/分");
        assert!(head.w >= 0 && tail.w > 0, "窄到放不下也不许算出负宽度");
    }

    #[test]
    fn corners_are_transparent_and_the_middle_is_solid() {
        // 圆心在 (7,7)、半径 7 的离散四分之一圆：角上第 2 个像素还在圆外，第 4 个已经在圆内
        let a = |x, y| card_alpha_at(x, y, 90, 46, 8, 184);
        assert_eq!(a(0, 0), 0, "左上角在圆外");
        assert_eq!(a(1, 1), 0, "贴着角的那几个像素也要真透明，否则圆角是块白边");
        assert_eq!(a(45, 23), 184, "正中");
        assert_eq!(a(3, 3), 184, "半径 7 之内");
        assert_eq!(a(8, 8), 184, "角心圆内");
        assert_eq!(a(89, 45), 0, "右下角同样");
        assert_eq!(a(45, 0), 184, "上边正中不受圆角影响");
        assert_eq!(a(-1, 5), 0, "窗口外");
        assert_eq!(a(90, 5), 0);
        assert_eq!(card_alpha_at(0, 0, 90, 46, 0, 184), 184, "半径 0 = 直角");
    }

    #[test]
    fn premultiply_is_identity_at_full_and_zero_at_clear() {
        let mut buf = vec![0u8; 8];
        put_premultiplied(&mut buf, 0, Color::rgba(255, 255, 255, 255), 255);
        assert_eq!(&buf[0..4], &[255, 255, 255, 255], "BGR 顺序：a=255 时原色");
        put_premultiplied(&mut buf, 1, Color::rgba(255, 255, 255, 255), 0);
        assert_eq!(&buf[4..8], &[0, 0, 0, 0]);

        let mut half = vec![0u8; 4];
        put_premultiplied(&mut half, 0, Color::rgba(200, 100, 50, 128), 128);
        assert_eq!(half[3], 128);
        assert_eq!(half[2], 100, "r=200 预乘 128/255 ⇒ 100");
        assert_eq!(half[1], 50, "g=100 ⇒ 50");
        assert_eq!(half[0], 25, "b=50 ⇒ 25");
    }

    /// 一条完整的合成链：透明底 → 0.72 的白卡 → 全覆盖的黑字。
    /// 结论必须是"字那一个像素完全不透明、卡片像素 184"，这正是 CSS 的分层结果。
    #[test]
    fn text_lands_opaque_on_a_translucent_card() {
        let mut buf = vec![0u8; 8];
        let card = LIGHT.card;
        put_premultiplied(&mut buf, 0, card, card.a);
        put_premultiplied(&mut buf, 1, card, card.a);
        let mut mask = vec![0u8; 8];
        // 两个像素的缓冲：第二个像素起点是 4
        let second = 4;
        mask[second + 2] = 255; // R 通道：第二个像素是满覆盖的字
        overlay_text_coverage(&mut buf, &mask, LIGHT.fg);
        assert_eq!(buf[3], 184, "没字的地方仍是卡片的 .72");
        assert_eq!(buf[second + 3], 255, "满覆盖的字完全不透明");
        assert_eq!(buf[second + 2], 26, "字色 r=26 预乘 255 不变");
    }

    #[test]
    fn partial_text_coverage_blends_instead_of_punching_a_hole() {
        let mut buf = vec![0u8; 4];
        put_premultiplied(&mut buf, 0, LIGHT.card, 184);
        let mut mask = vec![0u8; 4];
        mask[2] = 128;
        overlay_text_coverage(&mut buf, &mask, LIGHT.fg);
        // 半覆盖的字：alpha 落在 184 与 255 之间，而不是直接跳到 255
        assert!(buf[3] > 184 && buf[3] < 255, "实得 alpha={}", buf[3]);
    }

    #[test]
    fn paused_scales_all_four_channels_together() {
        let mut buf = vec![100u8, 120, 140, 200];
        scale_opacity(&mut buf, PAUSED_NUM, PAUSED_DEN);
        assert_eq!(buf, vec![55, 66, 77, 110]);

        let mut full = vec![100u8, 120, 140, 200];
        scale_opacity(&mut full, 100, 100);
        assert_eq!(
            full,
            vec![100, 120, 140, 200],
            "不透明度 1 时一个字节都不许动"
        );
    }

    #[test]
    fn theme_picks_the_matching_palette() {
        assert_eq!(palette_for("dark"), DARK);
        assert_eq!(palette_for("light"), LIGHT);
        assert_eq!(palette_for(""), LIGHT, "读不到配置时按亮色，与前端默认一致");
    }

    // ===== 配色不许和 CSS 分家 =====

    fn block_of<'a>(text: &'a str, opener: &str) -> Option<&'a str> {
        let start = text.find(opener)? + opener.len();
        Some(&text[start..text[start..].find('}')? + start])
    }

    /// CSS 里的 `#RRGGBB`（十六进制色一律不透明，所以第四个分量恒为 255）。
    /// 找不到就 panic 让判据红，而不是拿默认值蒙过去。
    fn css_hex(text: &str, key: &str) -> (u8, u8, u8, u8) {
        let at = text
            .find(key)
            .unwrap_or_else(|| panic!("CSS 里找不到 {key}，这条判据的写法过期了"));
        let rest = &text[at..];
        let hash = rest.find('#').expect("键后面没有十六进制色");
        let hex: Vec<u8> = rest.as_bytes()[hash + 1..hash + 7]
            .iter()
            .map(|c| (*c as char).to_digit(16).unwrap_or(0) as u8)
            .collect();
        (
            hex[0] << 4 | hex[1],
            hex[2] << 4 | hex[3],
            hex[4] << 4 | hex[5],
            255,
        )
    }

    /// CSS 里的 `rgba(r, g, b, a)` → 8 位 alpha。
    fn css_rgba(text: &str, key: &str) -> (u8, u8, u8, u8) {
        let at = text
            .find(key)
            .unwrap_or_else(|| panic!("CSS 里找不到 {key}，这条判据的写法过期了"));
        let rest = &text[at..];
        let open = rest.find("rgba(").expect("键后面没有 rgba(") + 5;
        let close = rest[open..].find(')').expect("rgba( 没有收尾");
        let nums: Vec<f64> = rest[open..open + close]
            .split(',')
            .map(|s| s.trim().parse::<f64>().expect("rgba 里不是数字"))
            .collect();
        assert_eq!(nums.len(), 4, "rgba 必须四个分量");
        (
            nums[0] as u8,
            nums[1] as u8,
            nums[2] as u8,
            (nums[3] * 255.0).round() as u8,
        )
    }

    fn same(c: Color, rgb: (u8, u8, u8, u8), what: &str) {
        // ±1：CSS 的 0.72 × 255 = 183.6，浏览器与这里的 round() 可以差一个刻度。
        let near = |a: u8, b: u8| (a as i16 - b as i16).abs() <= 1;
        assert!(
            near(c.r, rgb.0) && near(c.g, rgb.1) && near(c.b, rgb.2) && near(c.a, rgb.3),
            "{what}：Rust 侧 {:?} 与 CSS {:?} 对不上（改一边必须改另一边）",
            c,
            rgb
        );
    }

    /// 悬浮窗那六个颜色在仓库里有两处写法：CSS（唯一权威）与这里的 `LIGHT`/`DARK`。
    /// 本仓已经吃过多次"契约常量多处写"的账，所以这条判据直接回头读那两个文件比数，
    /// 而不是抄一份注释说"记得同步"。
    #[test]
    fn the_native_palette_matches_the_css_it_is_replacing() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let tokens = std::fs::read_to_string(root.join("ui/tokens.css"))
            .expect("读不到 ui/tokens.css：判据的路径或文件名变了");
        let html = std::fs::read_to_string(root.join("ui/floating.html"))
            .expect("读不到 ui/floating.html：判据的路径或文件名变了");

        let light = block_of(&tokens, ":root").expect(":root 块找不到了");
        let dark = block_of(&tokens, "body.dark").expect("body.dark 块找不到了");
        same(
            LIGHT.fg,
            css_hex(light, "--fg"),
            "亮色 --fg（活跃/时长的字色）",
        );
        same(
            LIGHT.muted,
            css_hex(light, "--muted"),
            "亮色 --muted（标签字色）",
        );
        same(
            LIGHT.accent,
            css_hex(light, "--accent"),
            "亮色 --accent（速度字色）",
        );
        same(
            LIGHT.warning,
            css_hex(light, "--warning"),
            "亮色 --warning（暂停角标）",
        );
        same(DARK.fg, css_hex(dark, "--fg"), "深色 --fg");
        same(DARK.muted, css_hex(dark, "--muted"), "深色 --muted");
        same(DARK.accent, css_hex(dark, "--accent"), "深色 --accent");
        same(DARK.warning, css_hex(dark, "--warning"), "深色 --warning");

        // 卡片底与边框是悬浮窗自己的样式（不在 tokens 里）
        same(
            LIGHT.card,
            css_rgba(&html, "background: rgba(255, 255, 255, 0.72)"),
            "亮色卡片底",
        );
        same(
            DARK.card,
            css_rgba(&html, "background: rgba(30, 35, 49, 0.72)"),
            "深色卡片底",
        );
        same(
            LIGHT.border,
            css_rgba(&html, "--border: rgba(224,233,248,0.95)"),
            "亮色卡片边框",
        );
        same(
            DARK.border,
            css_rgba(&html, "--border: rgba(43,50,70,0.95)"),
            "深色卡片边框",
        );
    }

    /// 卡片外观的三个尺寸也在 CSS 里，抄错一处就是"看上去不一样"。
    #[test]
    fn the_geometry_constants_match_the_css_rules() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let html =
            std::fs::read_to_string(root.join("ui/floating.html")).expect("读不到悬浮窗页面");
        assert!(
            html.contains("padding: 4px 8px"),
            "内边距与 PAD_X/PAD_Y 脱钩"
        );
        assert!(html.contains("border-radius: 8px"), "圆角与 RADIUS 脱钩");
        assert!(html.contains("border: 1px solid"), "边框宽与 BORDER 脱钩");
        assert!(
            html.contains("font-size: 13px") && html.contains("width: 26px"),
            "字号或标签列宽与 FONT_PX/LABEL_W 脱钩"
        );
    }
}
