//! 内置看板页的状态那一半（路线图 3.2）。
//!
//! 数字与指标**都不在这里算** —— 那一份在 `focusflow_core::dashboard`，与「活跃分析」
//! 那几页共用同一批读数函数。这个模块只管三件事：现在选的是哪一档、上一次是什么时候
//! 问的库、以及页签上那三个范围胶囊的位置（画与命中读同一份）。
//!
//! 画在 `main.rs` 的 `draw_board_page` 里：那一带私有的只有渲染器 `P`、`rect`、
//! 圆角档位那些东西，把它们为了一个新模块全改成 `pub(crate)` 不值得 —— 排版留在
//! 排版的那一处，这里留的是**状态与几何**。

use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;

use focusflow_core::dashboard::DashBoard;
use focusflow_core::db::queries::DashRange;

/// 这一页的重查节奏（方案 3.2.3）：停留在看板页时每 30 秒重查一次。
///
/// 30 秒的理由不是"查询贵"（整页五趟查询实测 0.27 ms 量级），而是看的量级是
/// 本周/本月：一秒一次没有任何视觉收益，而每分钟一次又会与采集线程的攒批
/// （10 秒）错开成"有时含这一批、有时不含"。
pub const REFRESH_SECS: u64 = 30;

/// 三档范围的顺序就是胶囊从左到右的顺序。
pub const RANGES: [DashRange; 3] = [DashRange::Today, DashRange::Week, DashRange::Month];

/// 把 `D2D_RECT_F` 造出来（与 `main.rs` 那个私有 `rect` 同义，这里只给胶囊用）。
fn box_rect(x: f32, y: f32, w: f32, h: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}

/// 顶部那一排的宽度/间距，与 `main.rs` 的 `view_tabs()` 同一把尺子（80 宽、84 一节），
/// 胶囊排在八个页签右边。
const TAB_PITCH: f32 = 84.0;
const TAB_FIRST_X: f32 = 16.0;
const TAB_Y: f32 = crate::TAB_ROW_TOP;
const TAB_H: f32 = 26.0;
const RANGE_W: f32 = 68.0;

/// 范围胶囊：(哪一档, 矩形)。画、悬停、点击三处都从这一处取矩形，
/// 命中判定用全仓那一个 `hit`（这里不自建第二条边界规则 —— 那条规则的含与不含
/// 已经由 `hit` 定了，另写一遍就是"页面上看着是这格、点上去答的是那格"的起点）。
pub fn range_pills() -> [(DashRange, D2D_RECT_F); 3] {
    let mut out = [(
        DashRange::Today,
        box_rect(TAB_FIRST_X, TAB_Y, RANGE_W, TAB_H),
    ); 3];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = (
            RANGES[i],
            box_rect(
                TAB_FIRST_X + (8.0 + i as f32) * TAB_PITCH,
                TAB_Y,
                RANGE_W,
                TAB_H,
            ),
        );
    }
    out
}

/// 这一页要不要重查库。三个条件缺一条就不查（方案 3.2.3 那张表）。
///
/// 单独一个纯函数是因为"最小化/不可见不刷新"这一条只能靠它测得到：
/// 常驻着一页看不见的数据还在每秒问库，是这一族最容易写歪的那类安静浪费。
pub fn should_refresh(elapsed_secs: u64, on_board: bool, window_watching: bool) -> bool {
    on_board && window_watching && elapsed_secs >= REFRESH_SECS
}

/// 这一页的运行时状态。
pub struct Dash {
    pub board: DashBoard,
    pub range: DashRange,
    /// 上一次问库的时刻（`None` = 这一版还没问过，进页就问）
    pub last: Option<std::time::Instant>,
}

impl Default for Dash {
    fn default() -> Self {
        Self::new()
    }
}

impl Dash {
    /// 默认「本周」（方案 14 节第 3 项按默认执行）。
    pub fn new() -> Self {
        Self {
            board: DashBoard::default(),
            range: DashRange::Week,
            last: None,
        }
    }

    /// 距上次问库多久（没问过 ⇒ 视为已到点）。
    pub fn elapsed_secs(&self) -> u64 {
        match self.last {
            Some(at) => at.elapsed().as_secs(),
            None => u64::MAX,
        }
    }

    pub fn mark_loaded(&mut self) {
        self.last = Some(std::time::Instant::now());
    }

    /// 换一档：记住新的那一档，并把"上次问过"抹掉 —— 切范围要**立即**重查（3.2.3）。
    pub fn set_range(&mut self, range: DashRange) -> bool {
        if self.range == range {
            return false;
        }
        self.range = range;
        self.last = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 三个胶囊必须**排在八个页签右边**、互不重叠、且宽度落在页签那一行的同一条带上。
    ///
    /// 钉这三件事是因为这一族几何只被两处读（画 + 命中），写歪一格的症状是
    /// "点『本月』选上的是『本周』"或者胶囊压住最后一个页签，而两个都答不上"是谁吃的那一脚"。
    #[test]
    fn the_range_pills_sit_right_of_the_eight_tabs() {
        let pills = range_pills();
        let names: Vec<DashRange> = pills.iter().map(|(r, _)| *r).collect();
        assert_eq!(
            names,
            vec![DashRange::Today, DashRange::Week, DashRange::Month]
        );
        // 八个页签占 16 + i*84（i=0..7），最后一个的右边界
        let last_tab_right = TAB_FIRST_X + 7.0 * TAB_PITCH + 80.0;
        assert!(
            pills[0].1.left >= last_tab_right,
            "第一个胶囊压在最右边那个页签上: 页签到 {}，胶囊从 {}",
            last_tab_right,
            pills[0].1.left
        );
        for i in 1..3 {
            assert!(
                pills[i].1.left >= pills[i - 1].1.right,
                "胶囊自己互相重叠: {i}"
            );
        }
        for (_, r) in &pills {
            assert_eq!(r.top, TAB_Y, "胶囊必须与页签同一行");
            assert_eq!(r.bottom - r.top, TAB_H);
            assert!(
                r.right <= 1200.0 - 16.0,
                "窄窗口那一头再看，这里先保证常见宽度不越界: {}",
                r.right
            );
        }
    }

    /// 三块胶囊必须互不重叠，且相邻两块**即便按含右含下的取边也不共用像素**。
    ///
    /// 为什么专门钉这一条：全仓那一个 `hit` 是四边全含的（`x <= right`），两块矩形只要挨上
    /// 就会同时答"是我"，而点下去选中的是循环里先碰到的那一档 —— 症状是"点本月选上了本周"，
    /// 不报错、不留日志。胶囊之间有缝就不可能出这件事，这里把那道缝钉住。
    #[test]
    fn no_two_pills_can_answer_the_same_click() {
        let pills = range_pills();
        for (i, (range, r)) in pills.iter().enumerate() {
            assert_eq!(range, &RANGES[i], "胶囊的顺序必须跟 RANGES 一致");
            for (j, (_, other)) in pills.iter().enumerate() {
                if i == j {
                    continue;
                }
                assert!(
                    r.right < other.left || other.right < r.left,
                    "第 {i} 与第 {j} 块胶囊共用像素：含边界的 hit 会双答"
                );
            }
        }
        // 与页签也不能贴边：页签是 80 宽 / 84 一节，最后一张的右边界
        let last_tab_right = TAB_FIRST_X + 7.0 * TAB_PITCH + 80.0;
        assert!(
            pills[0].1.left > last_tab_right,
            "第一个胶囊与最右那个页签贴边了"
        );
    }

    /// 「不可见不刷新」与「30 秒」是同一句里的两个条件，少一个都会常驻浪费。
    #[test]
    fn the_page_asks_the_library_only_when_it_is_actually_being_read() {
        assert!(
            should_refresh(REFRESH_SECS, true, true),
            "到点、在这页、看得见 ⇒ 问"
        );
        assert!(should_refresh(REFRESH_SECS + 1, true, true));
        assert!(
            !should_refresh(REFRESH_SECS - 1, true, true),
            "没到点不许问：整页查询才 0.27 ms，但一秒一次没有任何视觉收益"
        );
        assert!(
            !should_refresh(10_000, false, true),
            "人不在这页上就不该为这一页问库"
        );
        assert!(
            !should_refresh(10_000, true, false),
            "窗口最小化/藏进托盘时问库，是看不见的那一屏在花钱"
        );
        assert!(
            !should_refresh(10_000, false, false),
            "两条都不成立更不该问"
        );
    }

    /// 换档要立刻把"上次问过"清掉，否则切到「今日」看到的还是上一次本周的数。
    #[test]
    fn changing_the_range_earns_an_immediate_requery() {
        let mut dash = Dash::new();
        assert_eq!(dash.range, DashRange::Week, "默认必须是本周");
        assert_eq!(dash.elapsed_secs(), u64::MAX, "还没问过 ⇒ 视为到点");
        dash.mark_loaded();
        assert!(dash.elapsed_secs() < REFRESH_SECS, "刚问过就该还没到点");
        assert!(!dash.set_range(DashRange::Week), "同一档不算换");
        assert!(dash.elapsed_secs() < REFRESH_SECS, "同档不该把时刻清掉");
        assert!(
            dash.set_range(DashRange::Month),
            "换档要答 true（调用方据此立刻重查）"
        );
        assert_eq!(dash.elapsed_secs(), u64::MAX, "换档之后必须当成没问过");
    }

    /// 接线的形状（读 `main.rs` 的源码）。
    ///
    /// 这一页的每条要求都发生在"按下鼠标 / 时钟到点"那一瞬，无头环境量不到像素；
    /// 但**有没有接到正确的出口上**是能在字节上问的：漏一条的症状分别是
    /// 新页签画成占位文案、进去之后看到的是旧数、以及看不见时还在问库。
    mod wiring {
        fn prod() -> String {
            let src = include_str!("main.rs");
            let cut = src.find("#[cfg(test)]").expect("测试模块的起点找不到了");
            src[..cut].to_string()
        }

        fn flat(s: &str) -> String {
            s.lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n")
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect()
        }

        /// 页签加了一个，就必须同时加在**三处**：标签表、坐标表、以及分发那一支。
        /// 少第三处的症状最难发现 —— 页签画出来了、也点得动，画面却是"这个页面还没做"。
        #[test]
        fn the_tabs_are_declared_drawn_and_dispatched() {
            let p = prod();
            let start = p
                .find("const VIEWS: [&str; 8]")
                .expect("页签标签表找不到了");
            let block = &p[start..start + 400];
            for label in ["\"看板\"", "\"剪贴板\""] {
                assert_eq!(
                    block.matches(label).count(),
                    1,
                    "页签标签应当只有 `VIEWS` 那一份"
                );
            }
            assert!(
                p.contains("const VIEWS: [&str; 8]"),
                "页签标签表不是 8 个 —— 新加的那一页根本没有入口"
            );
            assert!(
                p.contains("fn view_tabs() -> [D2D_RECT_F; 8]"),
                "坐标表不是 8 格：标签有 8 个而位置只有 7 个，索引会越界或被截掉"
            );
            let f = flat(&p);
            let board = f
                .find("V_BOARD=>draw_board_page(p,w,h,dash,vs)")
                .expect("看板页的分发不见了");
            let clip = f
                .find("V_CLIPBOARD=>draw_clipboard_page(p,w,h,clip,vs)")
                .expect("剪贴板页的分发不见了：它会落到 draw_placeholder 那句占位文案");
            let fallback = f
                .find("v=>draw_placeholder(p,w,h,v)")
                .expect("兜底那一支不在了（页签越界时画面会消失）");
            assert!(
                board < fallback && clip < fallback,
                "新页签必须排在 `v => draw_placeholder` 兜底**之前**，否则永远走不到"
            );
        }

        /// 页签顺序：总览在前、配置在最后。顺序一变，命中、快捷键与验收脚本
        /// 都会跟着错位，所以这里把八格的先后钉死。
        #[test]
        fn the_tab_order_starts_with_the_board_and_ends_with_settings() {
            let p = prod();
            let start = p
                .find("const VIEWS: [&str; 8]")
                .expect("页签标签表找不到了");
            let block = &p[start..start + 400];
            let want = [
                "\"看板\"",
                "\"键鼠排行\"",
                "\"应用排行\"",
                "\"设备排行\"",
                "\"活跃分析\"",
                "\"剪贴板\"",
                "\"插件管理\"",
                "\"设置\"",
            ];
            let mut at = 0;
            for label in want {
                let i = block
                    .find(label)
                    .unwrap_or_else(|| panic!("页签表里没有 {label}"));
                assert!(i >= at, "页签顺序错了：{label} 排到了前一个前面");
                at = i;
            }
        }

        /// 剪贴板页的数只能从 core 的 `clipboard` 要；桌面端不许自己写 SQL。
        #[test]
        fn the_clipboard_page_asks_core_and_reloads_on_entry() {
            let f = flat(&prod());
            assert!(
                f.contains("self.clip_page.reload("),
                "进剪贴板页没有重查：翻到的会是上一次离开时的旧列表"
            );
            assert!(
                f.contains("focusflow_core::clipboard::"),
                "剪贴板页没有走 core 的读取口"
            );
            assert_eq!(
                f.matches("SELECT ").count(),
                0,
                "桌面端生产段里出现了 SQL 语句：查库的口子应当只在 core"
            );
        }

        /// 这一页不许自己写 SQL：数字只能从 core 那一半要。
        ///
        /// 方案 3.2.4 那条"两个页面共用同一个查询函数"是口径的根 —— 一旦这一页
        /// 自己查一遍，同一天的数就会因为夹取/合并的差异而两边不一致。
        #[test]
        fn the_page_asks_core_for_every_number_and_writes_no_sql() {
            let f = flat(&prod());
            assert!(
                f.contains("focusflow_core::dashboard::collect(range,collect::pending_today())"),
                "这一页的数不再是从 core 那一半要的，或者把「今日未落库的增量」丢了"
            );
            assert_eq!(
                f.matches("SELECT ").count(),
                0,
                "桌面端生产段里出现了 SQL 语句：查库的口子应当只在 core"
            );
            let here = flat(include_str!("dashboard.rs"));
            assert_eq!(
                here.matches("SELECT ").count(),
                0,
                "看板页这个模块自己写了 SQL"
            );
        }

        /// 三个问库的时机（进页、换档、每 30 秒）都要接得上，且 30 这一档只有一个主人。
        #[test]
        fn all_three_moment_requery_and_the_thirty_seconds_has_one_owner() {
            let f = flat(&prod());
            // ① 切到这一页就问一次（写在页签那一圈里）
            assert!(
                f.contains("ifi==V_BOARD{self.reload_board();}"),
                "切到看板页没有立刻问库：第一次点进来看到的是旧数或全空"
            );
            // ② 换档问一次
            assert!(
                f.contains("ifself.dash.set_range(range){") && f.contains("self.reload_board();"),
                "换档那一条路不重查了"
            );
            // ③ 30 秒那一档走的是纯函数，不是一处硬写的秒数
            assert_eq!(
                f.matches("dashboard::should_refresh(").count(),
                1,
                "定时那一档不再问这个纯函数（或者被抄了两遍）"
            );
            assert!(
                !f.contains("as_secs()>=30") && !f.contains(">=30"),
                "UI 里又硬写了一遍 30 秒：那一份与 `REFRESH_SECS` 会漂"
            );
            // 「不可见就不问」这个条件必须真的进了那次调用
            assert!(
                f.contains("letwatching=matches!(watching_now(hwnd),Readers::Panel)"),
                "刷新没接窗口可见性：最小化了还在问库"
            );
        }

        /// 趋势必须用插件图表那一个画法（纵轴分母与空数据那句话只有一份）。
        #[test]
        fn the_trend_reuses_the_plugin_chart_drawing() {
            let f = flat(&prod());
            assert_eq!(
                f.matches("draw_plugin_chart(").count(),
                3,
                "函数本体 + 插件视图那一处 + 看板这一处；少了看板那处就是趋势另画了一遍"
            );
            assert!(
                f.contains("draw_board_page(p,w,h,dash,vs)")
                    && f.contains(
                        "draw_rank_card(p,rect(m,rank_top,col_w,rank_h),\"Top应用（时长）\""
                    ),
                "两列排行不再是这一族的画法"
            );
        }
    }
}
