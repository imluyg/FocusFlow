//! 悬浮窗的**纯计算**部分：数字格式、版面、逐像素合成、屏幕夹取。
//!
//! 为什么单独拆一个模块：`floating.rs` 全是 Win32 调用，没有真窗口什么也验不了；
//! 而卡片上那几个字、它们的透明度、落点该夹到哪，全是能在 `cargo test` 里钉住的纯函数。
//!
//! 这一份是从 P7 删掉的 `desktop/src/floating/render.rs` 搬过来的，删掉原生端当时没有的三样：
//! **久坐提醒横带**（没有采集进程，谈不上连用多久）、**暂停角标**（同样，没地方暂停）、
//! 以及「速度 /分」那一行 —— 原生端当时没有监听，速度无处可读。2026-10-27 采集接上之后，
//! 卡片恢复成旧 WebView 版的「活跃 / 时长（可选）/ 速度」三格；目标百分比回到主窗口。
//!
//! 颜色与字体口径也回到旧版：`tokens.css` 的 `--font: "Segoe UI", "Microsoft YaHei"`，
//! 标签与值同为 13px、行高 18、卡片 48（任务栏高度），只有层级靠颜色和右侧对齐。
//!
//! 颜色仍是深色/浅色两套：`[gui] theme` 说哪套就用哪套，这样 §8 第 3 项（浅色主题）
//! 落地时不用再回来改这里。

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

/// 卡片配色。数值逐条抄自旧版的 `ui/floating.html` 内联样式与 `ui/tokens.css`
/// （那两个文件随 `desktop/` 一起删了，这里现在是唯一一份）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub card: Color,
    pub border: Color,
    pub fg: Color,
    pub muted: Color,
    pub accent: Color,
}

pub const LIGHT: Palette = Palette {
    card: Color::rgba(255, 255, 255, 232), // rgba(255,255,255,.91)
    border: Color::rgba(224, 233, 248, 242), // rgba(224,233,248,.95)
    fg: Color::rgba(26, 26, 46, 255),      // --fg #1A1A2E
    muted: Color::rgba(74, 74, 106, 255),  // --muted #4A4A6A
    accent: Color::rgba(77, 140, 247, 255), // --accent #4D8CF7
};

pub const DARK: Palette = Palette {
    card: Color::rgba(30, 35, 49, 232),      // rgba(30,35,49,.91)
    border: Color::rgba(43, 50, 70, 242),    // rgba(43,50,70,.95)
    fg: Color::rgba(232, 234, 238, 255),     // --fg #E8EAEE
    muted: Color::rgba(138, 146, 166, 255),  // --muted #8A92A6
    accent: Color::rgba(91, 156, 247, 255),  // --accent #5B9CF7
};

pub fn palette_for(theme: &str) -> Palette {
    if theme == "dark" {
        DARK
    } else {
        LIGHT
    }
}

/// 悬浮窗显示口径（`[gui] floating_metric`）。
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
        // 任何别的值都当 times（旧前端 `floating.js` 就是只认这两个字面量）。
        _ => Metric::Times,
    }
}

/// `utils.js::fmt` 的口径：按千位加逗号、负数保留。
pub fn fmt_count(n: i64) -> String {
    let sign = if n < 0 { "-" } else { "" };
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    out.push_str(sign);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// 时长写法与主界面 `duration_text` 同一份口径：`3h 30m` / `12m` / `45s`。
///
/// 旧版悬浮窗用的是 `3小时30分`（跟页面上的 `fmtDuration` 对齐）。原生卡片只有 104px 宽，
/// 那一串连标签一起是 7 个全角宽的字，实测会被省略号切掉尾巴；紧凑写法与「应用排行」
/// 那一列又是同一个形状，读起来不打架。
pub fn fmt_duration(sec: i64) -> String {
    let s = sec.max(0);
    if s >= 3600 {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

/// 一条要画在卡片上的行：标签 + 值。`accent` 决定值用强调色还是正文色；
/// `suffix` 是跟在值后面的小字单位（速度那一行是 `/分`），旧 WebView 里它比数字小一号。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Row {
    pub label: &'static str,
    pub value: String,
    pub accent: bool,
    pub suffix: &'static str,
}

/// 卡片当前要说什么。每一拍由 `floating.rs` 从**已发布的那三格**拼出来。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PillContent {
    pub rows: Vec<Row>,
}

/// 拼出卡片内容。
///
/// 与旧 WebView 版同一口径（`floating.html`）：**活跃 / 时长（可选） / 速度**。
/// 采集没开时速度画「—」、暂停时画「暂停」，这一格永远在 —— 行数变了卡片会跳高。
/// 旧版在原生端刚接上时没有采集进程，才用「目标」顶了这一格；现在 `collect::cpm()`
/// 有了，目标回到主窗口，卡片恢复"打字速度"这件事。
pub fn build_content(
    metric: Metric,
    today_count: i64,
    active_seconds: i64,
    collector_on: bool,
    paused: bool,
    cpm: i64,
) -> PillContent {
    let duration = fmt_duration(active_seconds);
    let mut rows = Vec::with_capacity(3);
    match metric {
        Metric::Times => rows.push(Row {
            label: "活跃",
            value: fmt_count(today_count),
            accent: false,
            suffix: "",
        }),
        Metric::Duration => rows.push(Row {
            label: "活跃",
            value: duration,
            accent: false,
            suffix: "",
        }),
        Metric::Both => {
            rows.push(Row {
                label: "活跃",
                value: fmt_count(today_count),
                accent: false,
                suffix: "",
            });
            rows.push(Row {
                label: "时长",
                value: duration,
                accent: false,
                suffix: "",
            });
        }
    }
    let (speed, accent, suffix) = if !collector_on {
        ("—".to_string(), false, "")
    } else if paused {
        ("暂停".to_string(), false, "")
    } else {
        (cpm.max(0).to_string(), true, "/分")
    };
    rows.push(Row {
        label: "速度",
        value: speed,
        accent,
        suffix,
    });
    PillContent { rows }
}

// ===== 版面 =====

// 2026-10-27 定稿：照旧 WebView 的 `floating.html` —— 8px 边距、标签列 26px、2px 间距、
// 13px 字号、行高 18；标签与值**紧挨着**（值左对齐），不再把值推到右边。
// 高度仍由任务栏兜底（100% 下 48），所以上下各留 5px。
pub const PAD_X: i32 = 8;
pub const PAD_Y: i32 = 5;
pub const ROW_H: i32 = 18;
pub const LABEL_W: i32 = 26;
pub const GAP: i32 = 2;
pub const RADIUS: i32 = 8;
pub const BORDER: i32 = 1;
/// 标签字号（Microsoft YaHei UI）：与旧 WebView 的 13px 一致。
pub const LABEL_PX: i32 = 13;
/// 值字号（Segoe UI）：旧版 `.val` / `.cpm` 都是 13px，不做加粗。
pub const VALUE_PX: i32 = 13;
/// 卡片再窄就放不下「标签 + 五位数」。旧 WebView 默认 90，这里给它当下限。
pub const MIN_W: i32 = 90;

/// 内容需要的最小高度（逻辑像素）。
///
/// 为什么高度由内容决定：分层窗口没有滚动条，画不下就是**看不见**，
/// 而 `metric=both` 时行数会变。所以尺寸 = max(配置值, 内容需要值)。
pub fn content_height(rows: usize) -> i32 {
    rows as i32 * ROW_H + 2 * PAD_Y + 2 * BORDER
}

/// 卡片有几行（`metric` 决定上面那一到两行，目标那一行永远在）。
pub fn row_count(metric: Metric) -> usize {
    match metric {
        Metric::Both => 3,
        Metric::Times | Metric::Duration => 2,
    }
}

/// 悬浮窗该有多高（逻辑像素）：配置值是**下限**，内容多一行就要多一行。
pub fn pill_height(cfg_height: f64, rows: usize) -> i32 {
    let floor = if cfg_height.is_nan() {
        48
    } else {
        cfg_height.max(24.0).round() as i32
    };
    floor.max(content_height(rows))
}

/// 卡片长高之后 top 该落在哪（同一单位内的纯几何）。
///
/// 长出来的那一行是往**下**长的：贴着屏幕底边放好的卡片每多一行就被切掉最后一行，
/// 所以下边缘不够就把整张往上让，行数回落时再回到锚点。
///
/// ⚠ `anchor` 必须是"用户放下的那一个 top"，不是当前窗口矩形里的 top —— 按矩形算
/// 就是让上去之后再也回不来，每变一次往上爬一截，那就是"悬浮窗自己跑位"。
pub fn fit_top(anchor: f64, height: f64, mon_top: f64, mon_bottom: f64) -> f64 {
    anchor.min(mon_bottom - height).max(mon_top)
}

/// 一块矩形（像素坐标，y 自上而下）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// 一行的两段矩形：标签、值。坐标已按 `scale` 放大成物理像素。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RowLayout {
    pub label: Rect,
    pub value: Rect,
}

/// 逻辑像素 → 物理像素。**逐值四舍五入**，不许把 scale 先取整：
/// 125%/150% 缩放下 `scale as i32` 恒等于 1，整张卡片会停在 100% 的大小、字也糊。
pub fn px(v: i32, scale: f64) -> i32 {
    (v as f64 * scale.max(0.5)).round() as i32
}

pub fn layout_rows(w: i32, scale: f64, count: usize) -> Vec<RowLayout> {
    let pad_x = px(PAD_X, scale);
    let pad_y = px(PAD_Y, scale);
    let row_h = px(ROW_H, scale);
    let label_w = px(LABEL_W, scale);
    let gap = px(GAP, scale);
    (0..count)
        .map(|i| {
            let y = pad_y + i as i32 * row_h;
            let value_x = pad_x + label_w + gap;
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
                    w: (w - value_x - pad_x).max(0),
                    h: row_h,
                },
            }
        })
        .collect()
}

// ===== 屏幕夹取 =====

/// 悬浮窗"至少还得露出这么多像素"才算看得见；低于这个才认为它跑丢了。
const FLOATING_MIN_VISIBLE: f64 = 20.0;

/// 一块屏幕的位置与尺寸 + 悬浮卡片尺寸（合成一个参数）。
pub struct Bounds {
    pub left: f64,
    pub top: f64,
    pub screen_w: f64,
    pub screen_h: f64,
    pub win_w: f64,
    pub win_h: f64,
}

/// 把存过的悬浮窗坐标夹回屏幕，但**只在卡片基本看不见时才动它**。
///
/// 要防的是那一件事：换显示器/拔副屏之后坐标落在屏幕之外，窗口照样显示
/// —— 表现是"设置里开着悬浮窗却看不见"，而卡片看不见就拖不到。
///
/// 判据不能用**工作区**：悬浮窗是 top-most 工具窗，压在任务栏上照样看得见；
/// 拿工作区夹会把人故意放在屏幕底边的位置每次启动往上拽一截（旧版实测过）。
pub fn clamp_into_screen(x: f64, y: f64, b: &Bounds) -> (f64, f64) {
    let min_x = b.left - b.win_w + FLOATING_MIN_VISIBLE;
    let max_x = b.left + b.screen_w - FLOATING_MIN_VISIBLE;
    let min_y = b.top - b.win_h + FLOATING_MIN_VISIBLE;
    let max_y = b.top + b.screen_h - FLOATING_MIN_VISIBLE;
    // max 可能小于 min（屏幕比卡片还小）→ 夹成单点而不是 panic
    (
        x.clamp(min_x, max_x.max(min_x)),
        y.clamp(min_y, max_y.max(min_y)),
    )
}

// ===== 逐像素合成 =====
//
// 全程用**预乘**格式（`[b, g, r, a]`，rgb 已乘过 a），因为 `UpdateLayeredWindow` 配
// `AC_SRC_ALPHA` 要的就是预乘。合成公式是标准 source-over：
//   a' = a_s + a_d·(1−a_s)，rgb' = rgb_s + rgb_d·(1−a_s)
// 8 位定点下用 255 作分母，误差 ≤1/255，肉眼不可见，且能写成纯函数被测。

/// 圆角矩形在某个像素上的覆盖率（4×4 超采样，255 = 完全在内部）。
///
/// 旧实现按"像素中心在不在圆内"给 0/alpha 两档，四个角是 1px 台阶；分层窗口没有系统圆角，
/// 那圈台阶在浅色壁纸上很显眼。超采样只影响边缘那一圈像素，内部仍是 255。
pub fn rounded_rect_coverage(x: i32, y: i32, w: i32, h: i32, radius: i32) -> u8 {
    if w <= 0 || h <= 0 || x < 0 || y < 0 || x >= w || y >= h {
        return 0;
    }
    let r = radius.clamp(0, (w / 2).min(h / 2)) as f64;
    if r == 0.0 {
        return 255;
    }
    let fw = w as f64;
    let fh = h as f64;
    let mut hits = 0u32;
    for sy in 0..4 {
        for sx in 0..4 {
            let px = x as f64 + (sx as f64 + 0.5) / 4.0;
            let py = y as f64 + (sy as f64 + 0.5) / 4.0;
            let cx = px.clamp(r, fw - r);
            let cy = py.clamp(r, fh - r);
            let dx = px - cx;
            let dy = py - cy;
            if dx * dx + dy * dy <= r * r {
                hits += 1;
            }
        }
    }
    (hits * 255 / 16) as u8
}

/// 卡片（圆角矩形）在像素上的 alpha：覆盖率 × 卡片不透明度。
/// 产品路径直接用 [`rounded_rect_coverage`] 分别算内层/边框，这里只留给版面测试。
#[cfg(test)]
pub fn card_alpha_at(x: i32, y: i32, w: i32, h: i32, radius: i32, alpha: u8) -> u8 {
    let cov = rounded_rect_coverage(x, y, w, h, radius) as u16;
    (cov * alpha as u16 / 255) as u8
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
/// 于是"直接在卡片缓冲上 DrawText"得到的是一堆透明像素。改成单独一层：
/// 白色文字画在黑底上，R 通道就是覆盖度，再按色相乘叠上去 ——
/// 顺带让抗锯齿只能用灰度（`ANTIALIASED_QUALITY`），ClearType 的子像素在这里会变成彩边。
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_group_by_three() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1000), "1,000");
        assert_eq!(fmt_count(1234567), "1,234,567");
        assert_eq!(fmt_count(-1500), "-1,500");
    }

    #[test]
    fn duration_matches_the_compact_style_of_the_main_window() {
        assert_eq!(fmt_duration(0), "0s");
        assert_eq!(fmt_duration(-5), "0s", "负数先夹到 0");
        assert_eq!(fmt_duration(59), "59s");
        assert_eq!(fmt_duration(60), "1m");
        assert_eq!(fmt_duration(119), "1m");
        assert_eq!(fmt_duration(3599), "59m");
        assert_eq!(fmt_duration(3600), "1h 0m");
        assert_eq!(fmt_duration(3661), "1h 1m");
        assert_eq!(fmt_duration(12600), "3h 30m");
    }

    #[test]
    fn unknown_metric_falls_back_to_times() {
        assert_eq!(metric_from("times"), Metric::Times);
        assert_eq!(metric_from("duration"), Metric::Duration);
        assert_eq!(metric_from("both"), Metric::Both);
        assert_eq!(metric_from(""), Metric::Times);
        assert_eq!(metric_from("耗时"), Metric::Times);
    }

    /// `metric=duration` 时「活跃」那一格换成长度，但**不会**多出第三行；
    /// 只有 both 才三行。最后一格恒为「速度」。
    #[test]
    fn metric_decides_which_rows_exist_and_speed_is_always_last() {
        let t = build_content(Metric::Times, 1234, 3700, true, false, 45);
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.rows[0].value, "1,234");
        assert_eq!(t.rows[1].label, "速度");
        assert_eq!(t.rows[1].value, "45");
        assert_eq!(t.rows[1].suffix, "/分");
        assert!(t.rows[1].accent);

        let d = build_content(Metric::Duration, 1234, 3700, true, false, 45);
        assert_eq!(d.rows.len(), 2);
        assert_eq!(d.rows[0].value, "1h 1m");
        assert_eq!(d.rows[1].label, "速度");

        let b = build_content(Metric::Both, 1234, 3700, true, false, 45);
        assert_eq!(b.rows.len(), 3);
        assert_eq!(b.rows[1].label, "时长");
        assert_eq!(b.rows[2].label, "速度");
    }

    /// 采集没开 / 暂停时速度那一格仍在，只是写「—」/「暂停」并且不再用强调色。
    #[test]
    fn a_silent_collector_still_occupies_the_speed_row() {
        let off = build_content(Metric::Times, 10, 0, false, false, 0);
        assert_eq!(off.rows.len(), 2);
        assert_eq!(off.rows[1].value, "—");
        assert_eq!(off.rows[1].suffix, "");
        assert!(!off.rows[1].accent);

        let paused = build_content(Metric::Times, 10, 0, true, true, 0);
        assert_eq!(paused.rows[1].value, "暂停");
        assert_eq!(paused.rows[1].suffix, "");
        assert!(!paused.rows[1].accent);
    }

    /// 卡片默认 132×48（两行）—— 高度正好是任务栏高度。
    #[test]
    fn two_row_layout_is_exactly_the_shipped_pill_height() {
        assert_eq!(content_height(2), 48);
        assert_eq!(content_height(3), 66, "metric=both 多一行");
    }

    #[test]
    fn the_window_height_is_a_floor_that_content_can_raise() {
        assert_eq!(row_count(Metric::Times), 2);
        assert_eq!(row_count(Metric::Duration), 2, "换口径不该多出一行");
        assert_eq!(row_count(Metric::Both), 3);

        assert_eq!(pill_height(46.0, 2), 48, "旧的 46 低于内容需要值");
        assert_eq!(pill_height(48.0, 2), 48, "默认配置正好容下两行");
        assert_eq!(pill_height(48.0, 3), 66, "both 的第三行");
        assert_eq!(pill_height(200.0, 2), 200, "人把卡片调大时就照他的");
        assert_eq!(pill_height(f64::NAN, 2), 48, "配置读不出来也要有能看见的高度");
    }

    /// 长高之后卡片该待在哪：往下够就不动，不够就整张往上让，收回来时回到**锚点**。
    #[test]
    fn a_taller_card_moves_up_only_when_the_bottom_edge_is_in_the_way() {
        let (top, bottom) = (0.0, 1080.0);
        assert_eq!(fit_top(60.0, 64.0, top, bottom), 60.0, "下方有空间就不动");
        assert_eq!(fit_top(1031.0, 46.0, top, bottom), 1031.0, "贴底边放的是用户放的");
        assert_eq!(fit_top(1031.0, 64.0, top, bottom), 1016.0, "越过底边 ⇒ 往上让 15");
        assert_eq!(
            fit_top(1031.0, 46.0, top, bottom),
            1031.0,
            "收掉那一行必须回到锚点，不是停在让过的矩形上"
        );
        assert_eq!(fit_top(30.0, 64.0, 0.0, 48.0), 0.0, "屏幕比卡片还矮就钉在顶上");
    }

    #[test]
    fn rows_stack_from_the_top_padding() {
        let r = layout_rows(104, 1.0, 2);
        assert_eq!(
            r[0].label,
            Rect {
                x: 8,
                y: 5,
                w: 26,
                h: 18
            }
        );
        assert_eq!(r[1].label.y, 23);
        assert_eq!(r[0].value.x, 36, "8 + 26 + 2");
        assert_eq!(r[0].value.w, 60, "104 - 36 - 8");

        let scaled = layout_rows(156, 1.5, 2);
        assert_eq!(scaled[0].label.x, 12, "8 × 1.5");
        assert_eq!(scaled[0].label.h, 27, "18 × 1.5");
    }

    #[test]
    fn corners_are_transparent_and_the_middle_is_solid() {
        let a = |x, y| card_alpha_at(x, y, 104, 48, 8, 184);
        assert_eq!(a(0, 0), 0, "左上角在圆外");
        assert_eq!(a(1, 1), 0, "贴着角的像素也要真透明");
        assert_eq!(a(52, 24), 184, "正中");
        assert_eq!(a(4, 4), 184, "半径之内");
        assert_eq!(a(103, 47), 0, "右下角同样");
        assert_eq!(a(52, 0), 184, "上边正中不受圆角影响");
        assert_eq!(a(-1, 5), 0, "窗口外");
        assert_eq!(card_alpha_at(0, 0, 104, 48, 0, 184), 184, "半径 0 = 直角");
    }

    /// 角上的过渡像素必须是**部分** alpha：0/满两档就是旧版的 1px 台阶。
    #[test]
    fn rounded_corners_have_partial_coverage_instead_of_a_hard_step() {
        let partial = (0..12)
            .flat_map(|y| (0..12).map(move |x| (x, y)))
            .map(|(x, y)| rounded_rect_coverage(x, y, 104, 48, 8))
            .filter(|c| *c > 0 && *c < 255)
            .count();
        assert!(partial >= 4, "角上至少要有一圈部分覆盖的像素，实得 {partial}");
        assert_eq!(rounded_rect_coverage(52, 24, 104, 48, 8), 255);
        assert_eq!(rounded_rect_coverage(0, 0, 104, 48, 8), 0);
    }

    #[test]
    fn premultiply_is_identity_at_full_and_zero_at_clear() {
        let mut buf = vec![0u8; 8];
        put_premultiplied(&mut buf, 0, Color::rgba(255, 255, 255, 255), 255);
        assert_eq!(&buf[0..4], &[255, 255, 255, 255], "BGRA：a=255 时原色");
        put_premultiplied(&mut buf, 1, Color::rgba(255, 255, 255, 255), 0);
        assert_eq!(&buf[4..8], &[0, 0, 0, 0]);

        let mut half = vec![0u8; 4];
        put_premultiplied(&mut half, 0, Color::rgba(200, 100, 50, 128), 128);
        assert_eq!(half[3], 128);
        assert_eq!(half[2], 100, "r=200 × 128/255 ⇒ 100");
        assert_eq!(half[1], 50, "g=100 ⇒ 50");
        assert_eq!(half[0], 25, "b=50 ⇒ 25");
    }

    /// 一条完整的合成链：透明底 → 0.72 的白卡 → 全覆盖的字。
    #[test]
    fn text_lands_opaque_on_a_translucent_card() {
        let mut buf = vec![0u8; 8];
        put_premultiplied(&mut buf, 0, LIGHT.card, LIGHT.card.a);
        put_premultiplied(&mut buf, 1, LIGHT.card, LIGHT.card.a);
        let mut mask = vec![0u8; 8];
        mask[6] = 255; // 第二个像素的 R 通道：满覆盖
        overlay_text_coverage(&mut buf, &mask, LIGHT.fg);
        assert_eq!(buf[3], 232, "没字的地方仍是卡片的 .91");
        assert_eq!(buf[7], 255, "满覆盖的字完全不透明");
        assert_eq!(buf[6], 26, "字色 r=26 预乘 255 不变");
    }

    #[test]
    fn partial_text_coverage_blends_instead_of_punching_a_hole() {
        let mut buf = vec![0u8; 4];
        put_premultiplied(&mut buf, 0, LIGHT.card, 184);
        let mut mask = vec![0u8; 4];
        mask[2] = 128;
        overlay_text_coverage(&mut buf, &mask, LIGHT.fg);
        assert!(buf[3] > 184 && buf[3] < 255, "实得 alpha={}", buf[3]);
    }

    #[test]
    fn theme_picks_the_matching_palette() {
        assert_eq!(palette_for("dark"), DARK);
        assert_eq!(palette_for("light"), LIGHT);
        assert_eq!(palette_for(""), LIGHT, "读不到配置时按亮色");
    }

    // ===== 落点 =====

    fn screen(x: i32, y: i32, w: u32, h: u32) -> Bounds {
        Bounds {
            left: x as f64,
            top: y as f64,
            screen_w: w as f64,
            screen_h: h as f64,
            win_w: 90.0,
            win_h: 46.0,
        }
    }

    /// 坐标落在已不存在的副屏那边：卡片得被拉回露出 20 像素的地方，而不是整张消失。
    #[test]
    fn a_saved_point_beyond_the_only_screen_is_pulled_back() {
        let b = screen(0, 0, 1920, 1080);
        assert_eq!(
            clamp_into_screen(4000.0, 300.0, &b),
            (1900.0, 300.0),
            "右边界是 1920-20；只有一个轴越界时不许动另一个轴"
        );
        assert_eq!(
            clamp_into_screen(-2500.0, 300.0, &b),
            (-70.0, 300.0),
            "左边界是 0-90+20：露出 20 像素就算看得见"
        );
    }

    /// 反向腿：人自己放好的位置一个字都不许动 —— 尤其是贴在屏幕底边那一种。
    #[test]
    fn a_saved_point_that_is_already_visible_is_left_alone() {
        let b = screen(0, 0, 1920, 1080);
        assert_eq!(
            clamp_into_screen(82.0, 1031.0, &b),
            (82.0, 1031.0),
            "贴着底边的位置是用户放的"
        );
        // 左边那块屏的原点是负数，那是正确结果，不是"跑丢了"。
        assert_eq!(
            clamp_into_screen(-1900.0, 100.0, &screen(-1920, 0, 1920, 1080)),
            (-1900.0, 100.0)
        );
    }
}
