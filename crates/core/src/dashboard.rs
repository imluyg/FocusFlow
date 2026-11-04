//! 内置看板页的读数（路线图 3.2）。
//!
//! 这一页的底线是**它不写 SQL**：六个指标全部出自 `db::queries` 与 `stats` 既有的读数，
//! 与「活跃分析」那几页走同一批函数。方案里那句"同一天的数字在两个页面不一样，
//! 用户只会信小的那个"就是这条约束的理由，而 `metrics_match_documented_semantics`
//! 钉的是它。
//!
//! 分成两半：[`summarize`] 是纯的（给一堆读数就算，跨年、周一、月初那些边界都能在
//! 用例里造出来，不用等日历走到那天），[`collect`] 只管去问库。

use crate::db::queries::{self, DashRange};
use crate::format::{fmt_compact, fmt_compact_duration};
use crate::stats;

/// 排行每一列取前几名（方案 3.2 的表定的是各取前 8）。
pub const DASH_RANK_TOP: usize = 8;

/// 纯那一半的输入：已经读回来的数，不再碰库。
///
/// `trend` 必须是**含今天、长度等于这一档天数**的每日按键序列（`daily_counts_totals`
/// 交回的就是这个形状，缺的天按 0 补）。长度不是今天现算的：范围与点数对不上，
/// 页面上那句「本周」就会盖在一段不是本周的数据上。
pub struct DashInput<'a> {
    pub range: DashRange,
    /// 「YYYY-MM-DD」，与 `trend` 里同一把日期串
    pub today: &'a str,
    pub trend: Vec<(String, i64)>,
    /// 窗口内 `daily_counts.seconds` 合计（**含**没采到前台应用的那段，见口径第 3 条）
    pub active_seconds: i64,
    /// 窗口内应用时长（全量，这里排序与截断）
    pub apps: Vec<(String, i64)>,
    /// 窗口内按键次数（全量，同上）
    pub keys: Vec<(String, i64)>,
    /// 认得的设备总数（快照，与范围无关）
    pub devices: i64,
    /// 从今天往前连续 `count > 0` 的天数（今天为 0 时从昨天起算，周末不跳）
    pub streak_days: i64,
    /// 这一轮没读全的地方。非空 ⇒ 上面所有数都是**下界**，页面要把它说出去
    pub unreadable: Vec<String>,
}

/// 一页要画的全部内容。字段都是"已经念成人话"的那一份，页面只负责排版。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DashBoard {
    /// KPI 行：(标题, 值)，顺序就是页面上的顺序
    pub kpi: Vec<(String, String)>,
    /// 趋势：横轴日期串 + 每日按键数（与 [`DashInput::trend`] 同长同序）
    pub trend: Vec<(String, i64)>,
    pub top_apps: Vec<(String, i64)>,
    pub top_keys: Vec<(String, i64)>,
    /// 「最常用」那一格的写法：`应用名 · 3.2h`
    pub top_app_line: String,
    /// 一句话（方案 3.2 的最后一行）
    pub sentence: String,
    pub gaps: Vec<String>,
}

/// 六个指标 + 一句话，全部对着方案 3.2.1 那张表算。
///
/// 排序一律走 [`crate::format::cmp_rank_desc`]：这些榜的输入来自 `HashMap`，
/// 少那半截"同分按名字"就会让**谁进前 8** 每进程一变（本仓为这件事修过两轮）。
pub fn summarize(input: DashInput<'_>) -> DashBoard {
    let keys_total: i64 = input.trend.iter().map(|(_, v)| *v).sum();
    let mut apps = input.apps;
    apps.sort_by(|a, b| crate::format::cmp_rank_desc(&a.0, &a.1, &b.0, &b.1));
    let mut keys = input.keys;
    keys.sort_by(|a, b| crate::format::cmp_rank_desc(&a.0, &a.1, &b.0, &b.1));
    apps.truncate(DASH_RANK_TOP);
    keys.truncate(DASH_RANK_TOP);

    // `apps.top` 取的是"时长降序第一"，榜空就没有这一格（口径第 3 条：无归属时段
    // 不进 `apps.top`，它进的是 `active.hours`）
    let top_app_line = match apps.first() {
        Some((name, secs)) => format!("{name} · {}", fmt_compact_duration(*secs)),
        None => format!("{}（暂无）", "最常用应用"),
    };

    let sentence = match apps.first() {
        Some((name, _)) => format!(
            "{}共按键 {} 次，活跃 {}，最常用 {}",
            input.range.label(),
            fmt_compact(keys_total),
            fmt_compact_duration(input.active_seconds),
            name
        ),
        None => format!(
            "{}共按键 {} 次，活跃 {}，这段时间没有归到某个应用上的活动",
            input.range.label(),
            fmt_compact(keys_total),
            fmt_compact_duration(input.active_seconds)
        ),
    };

    DashBoard {
        kpi: vec![
            ("按键总数".to_string(), fmt_compact(keys_total)),
            (
                "活跃时长".to_string(),
                fmt_compact_duration(input.active_seconds),
            ),
            ("连续天数".to_string(), format!("{} 天", input.streak_days)),
            ("设备数".to_string(), input.devices.to_string()),
        ],
        trend: input.trend,
        top_apps: apps,
        top_keys: keys,
        top_app_line,
        sentence,
        gaps: input.unreadable,
    }
}

/// 连续天数：与打卡条**同一个函数**，只是把谓词钉在 `count > 0`。
///
/// 为什么不再写一个：`stats::goal_status` 里那圈 while 已经处理了"今天还没有数据就从
/// 昨天起算""缺日不算断""周末不跳"这三条边界（第 2 条已确认过），另起一份就是给
/// 同一件事第二把尺子 —— 而这两处念的是同一个词（「连续打卡 N 天」与「连续天数」），
/// 对不上就会被当成 bug 报回来。传 `goal = 1` 是因为它的谓词是 `count >= max(goal,1)`，
/// 钉 1 正好等价于 `count > 0`。
fn streak_days(rows: &[(String, i64)], today: &str, today_pending: i64) -> i64 {
    stats::goal_status(1, rows, today, today_pending).streak
}

/// 去库里问这一档范围要看的全部读数。
///
/// `today_pending` 是"今天还没落库的那一截增量"（桌面端 `collect::pending_today()`）。
/// 为什么这一层要知道它：不加上，「今日」那一档的 KPI 与主页顶部那颗卡片就会差一批
/// （卡片读的是"库值 + 未落库增量"），而差出来的是**同一屏上的两个今日数**。
/// 加上之后这里与卡片同形，代价也是同一份：落库那一瞬可能重复计一次，
/// 那是本仓既有读数一起承担的窗口，不是这一页新引入的。
pub fn collect(range: DashRange, today_pending: i64) -> DashBoard {
    let today_date = chrono::Local::now().date_naive();
    let days = range.days_including_today(today_date);
    let today = today_date.format("%Y-%m-%d").to_string();

    let daily_pages = queries::daily_counts_windows(days, crate::stats::GOAL_LOOKBACK_DAYS);
    let (daily, goal_window) = daily_pages;
    let seconds = queries::daily_seconds_totals(days, None);
    let active_seconds = seconds.series.iter().map(|(_, v)| *v).sum::<i64>();
    let apps = queries::app_stats_totals(Some(days), None);
    let keys = queries::stats_totals(Some(days), None);
    let devices = queries::device_registry_totals();

    // 今天的增量并进最后一格：`trend` 的最后一项就是今天（窗口含今天，见上面那句），
    // 并进它而不是并成一个新项 —— 多出的一天会让点数比这一档天数多一个。
    let mut trend = daily.series;
    if today_pending != 0 {
        if let Some(last) = trend.last_mut() {
            last.1 = last.1.saturating_add(today_pending);
        }
    }
    // 连续天数读**宽窗口**（`GOAL_LOOKBACK_DAYS`）：3.2.1 那张表写明它「不受范围影响
    // （快照）」。拿页签这一档的 1/7/30 天序列去数，连续纪录会被窗口截断 —— 今天还没
    // 敲键盘时更糟：`goal_status` 从昨天往前数，而昨天不在窗口里，屏上直接念「0 天」。
    let streak = streak_days(&goal_window.series, &today, today_pending);

    let mut gaps: Vec<String> = Vec::new();
    for part in [
        daily.unreadable,
        seconds.unreadable,
        apps.unreadable,
        keys.unreadable,
        devices.unreadable,
    ] {
        gaps.extend(part);
    }
    // 连续天数那一段（370 天）坏掉的年份不在上面几份里：它的窗口比页签宽，缺读只会
    // 让连续纪录变短。不说出来，KPI 会把一个偏小的数念成完整答案。
    for why in goal_window.unreadable {
        // 同一年如果页签那一份已经点过名，这里不再重复一遍。
        if !gaps.iter().any(|g| why.contains(g.as_str())) {
            gaps.push(format!("连续天数可能偏小：{why}"));
        }
    }

    summarize(DashInput {
        range,
        today: &today,
        trend,
        active_seconds,
        apps: apps.rows.into_iter().collect(),
        keys: keys.rows.into_iter().collect(),
        devices: devices.devices,
        streak_days: streak,
        unreadable: gaps,
    })
}

/// 页面切换用的下一档（三档循环，默认「本周」）。
pub fn next_range(current: DashRange) -> DashRange {
    match current {
        DashRange::Today => DashRange::Week,
        DashRange::Week => DashRange::Month,
        DashRange::Month => DashRange::Today,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;

    fn monday_of_recent_week() -> chrono::NaiveDate {
        // 2026-11-02。夹具前提在下面单独钉一次，不靠日历自己走到那天。
        chrono::NaiveDate::from_ymd_opt(2026, 11, 2).unwrap()
    }

    /// 六个指标对着**手算得出来的**数据集各核一遍（方案验收表里那条
    /// `metrics_match_documented_semantics`）。
    ///
    /// 判据不许拿被测函数自己算期望值：这里每个期望数都是按 3.2.1 那张表写死的字面量，
    /// 所以"改口径"必须同时改这里，而"算错"会直接红。
    #[test]
    fn the_six_metrics_match_the_documented_semantics() {
        let input = DashInput {
            range: DashRange::Week,
            today: "2026-11-04",
            // 2026-11-04 是周三（夹具前提在下面那条用例里钉着）⇒ 本周含今天 3 天
            trend: vec![
                ("2026-11-02".to_string(), 1_000),
                ("2026-11-03".to_string(), 2_000),
                ("2026-11-04".to_string(), 9_200),
            ],
            // 3 天 × 1200 秒 = 3600 秒 = 1.0h
            active_seconds: 3_600,
            apps: vec![
                ("编辑器".to_string(), 7_200),
                ("浏览器".to_string(), 3_600),
                ("终端".to_string(), 3_600),
            ],
            keys: vec![("Space".to_string(), 12_345), ("A".to_string(), 7)],
            devices: 3,
            streak_days: 3,
            unreadable: vec![],
        };
        let board = summarize(input);

        // 1) keys.total = 范围内 count 合计 = 1000+2000+9200 = 12200，
        //    按统一写法 ≥10000 收成「1.2万」（口径第 4 条），不是 12200 也不是 1.22万
        assert_eq!(board.kpi[0], ("按键总数".to_string(), "1.2万".to_string()));
        // 2) active.hours = seconds 合计 ÷ 3600，一位小数
        assert_eq!(board.kpi[1], ("活跃时长".to_string(), "1.0h".to_string()));
        // 3) streak.days 直接进 KPI（怎么算出来是另一条用例）
        assert_eq!(board.kpi[2], ("连续天数".to_string(), "3 天".to_string()));
        // 4) devices.count = 快照，与范围无关
        assert_eq!(board.kpi[3], ("设备数".to_string(), "3".to_string()));
        // 5) keys.top：榜上第一是 Space（次数降序），次数写「1.2万」
        assert_eq!(board.top_keys[0].0, "Space");
        assert_eq!(crate::format::fmt_compact(board.top_keys[0].1), "1.2万");
        // 6) apps.top：时长降序第一，格式「应用名 · 3.2h」
        assert_eq!(board.top_app_line, "编辑器 · 2.0h");
        assert_eq!(
            board.sentence,
            "本周共按键 1.2万 次，活跃 1.0h，最常用 编辑器"
        );
        assert!(board.gaps.is_empty());
    }

    /// KPI 那一行的**顺序与条数**就是页面排版读的：多一条少一条、换个头一页就变了。
    #[test]
    fn the_kpi_row_is_four_cells_in_a_fixed_order() {
        let board = summarize(DashInput {
            range: DashRange::Today,
            today: "2026-11-04",
            trend: vec![("2026-11-04".to_string(), 42)],
            active_seconds: 45,
            apps: vec![],
            keys: vec![],
            devices: 0,
            streak_days: 1,
            unreadable: vec![],
        });
        let names: Vec<&str> = board.kpi.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["按键总数", "活跃时长", "连续天数", "设备数"]);
        // 今日档只有 45 秒：不足一分钟不许念 0m（同一屏上按键 42 次而时长 0m 就是自相矛盾）
        assert_eq!(board.kpi[1].1, "<1m");
        assert_eq!(board.kpi[0].1, "42");
        // 榜空的时候那句话要还在，并说清"没归到应用上"，而不是编一个最常用
        assert!(
            board.sentence.contains("这段时间没有归到某个应用上的活动"),
            "{}",
            board.sentence
        );
        assert!(!board.sentence.contains("最常用 编辑器"));
    }

    /// 三档范围换算成"含今天的 N 天"：周一起点、月初是 1 天、跨年那一周也算得出来。
    ///
    /// 这一条同时是 `dashboard_ranges_are_consistent` 的前半截（点数），后半截在趋势那一行：
    /// 范围说「本周」而点数是整月，就是标题盖在别人的数据上。
    #[test]
    fn the_dashboard_ranges_count_days_from_the_calendar() {
        let monday = monday_of_recent_week();
        assert_eq!(
            monday.weekday().num_days_from_monday(),
            0,
            "夹具前提没成立：2026-11-02 不是周一，那下面每一条都在测别的东西"
        );
        let d = |m: u32, day: u32| chrono::NaiveDate::from_ymd_opt(2026, m, day).unwrap();

        assert_eq!(DashRange::Today.days_including_today(d(11, 4)), 1);
        // 一整周逐日必须是 1..7（起点周一，不是周日，也不是"近 7 天"）
        let week: Vec<i64> = (0..7)
            .map(|i| DashRange::Week.days_including_today(monday + chrono::Duration::days(i)))
            .collect();
        assert_eq!(week, vec![1, 2, 3, 4, 5, 6, 7], "{week:?}");
        // 本月按日号：1 号那天「本月」就是 1 天，绝不是 30
        assert_eq!(DashRange::Month.days_including_today(d(11, 1)), 1);
        assert_eq!(DashRange::Month.days_including_today(d(11, 30)), 30);
        assert_eq!(DashRange::Month.days_including_today(d(12, 31)), 31);
        // 跨年那一周（2027-01-01 是周五）⇒ 5 天，其中 4 天在上一年
        let ny = chrono::NaiveDate::from_ymd_opt(2027, 1, 1).unwrap();
        assert_eq!(
            DashRange::Week.days_including_today(ny),
            5,
            "跨年那一周算成 0 或负数，页面上就是一条空白趋势"
        );
        assert_eq!(DashRange::Month.days_including_today(ny), 1);
        // 任何一天、任何一档都落在 1..=31（这一条挡住"周/月算成 0 天"那一族）
        for offset in 0..400 {
            let day = monday + chrono::Duration::days(offset);
            for range in [DashRange::Today, DashRange::Week, DashRange::Month] {
                let n = range.days_including_today(day);
                assert!(
                    (1..=31).contains(&n),
                    "{range:?} 在 {day} 算出 {n} 天，窗口长度越出这一档的日历边界了"
                );
            }
        }
    }

    /// 趋势的点数必须等于这一档换算出来的天数，且点数与范围标签同源于一个函数。
    #[test]
    fn the_trend_carries_exactly_the_days_of_its_range() {
        let monday = monday_of_recent_week();
        let wed = monday + chrono::Duration::days(2);
        assert_eq!(
            wed.weekday().num_days_from_monday(),
            2,
            "夹具前提：这一天得真的是周三"
        );
        // 期望值按日历手算（周三 = 一二三 = 3 天），**不拿被测函数自己算**——
        // 用它算期望值的话，范围换算错成几天这条判据就永远跟着一起变，红不了。
        let days = 3i64;
        assert_eq!(
            DashRange::Week.days_including_today(wed),
            days,
            "换算与手算的那一天数分家了"
        );
        let trend: Vec<(String, i64)> = (0..days)
            .map(|i| {
                let day = wed - chrono::Duration::days(days - 1 - i);
                (day.format("%Y-%m-%d").to_string(), 10 * (i + 1))
            })
            .collect();
        let board = summarize(DashInput {
            range: DashRange::Week,
            today: &wed.format("%Y-%m-%d").to_string(),
            trend: trend.clone(),
            active_seconds: 60,
            apps: vec![],
            keys: vec![],
            devices: 1,
            streak_days: days,
            unreadable: vec![],
        });
        assert_eq!(board.trend.len() as i64, days, "点数与范围分家了");
        assert_eq!(
            board.trend.last().unwrap().1,
            10 * days,
            "最后一格必须是今天（KPI 与一句话都按它算）"
        );
        assert_eq!(
            board.trend.first().unwrap().0,
            (wed - chrono::Duration::days(days - 1))
                .format("%Y-%m-%d")
                .to_string(),
            "起点必须是本周一"
        );
        assert_eq!(board.kpi[0].1, "60", "3 天合计 10+20+30");
    }

    /// 榜的截断与排序走那一把唯一的比较器：同分时必须按名字定序，否则**谁进前 8** 每进程一变。
    #[test]
    fn the_two_rank_lists_take_eight_with_a_deterministic_tie_break() {
        // 20 个应用，前 12 个同分：截前 8 完全取决于同分怎么排
        let apps: Vec<(String, i64)> = (0..20)
            .map(|i| (format!("app{i:02}"), if i < 12 { 100 } else { 1 }))
            .collect();
        let board = summarize(DashInput {
            range: DashRange::Week,
            today: "2026-11-04",
            trend: vec![("2026-11-04".to_string(), 1)],
            active_seconds: 0,
            apps,
            keys: vec![("K".to_string(), 5)],
            devices: 0,
            streak_days: 0,
            unreadable: vec![],
        });
        assert_eq!(
            board.top_apps.len(),
            DASH_RANK_TOP,
            "榜要截到前 {DASH_RANK_TOP}"
        );
        let names: Vec<&str> = board.top_apps.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec!["app00", "app01", "app02", "app03", "app04", "app05", "app06", "app07"],
            "同分那一堆里进榜的是谁，必须由那把唯一的比较器决定，不能看 HashMap 的哈希序"
        );
    }

    /// 「读不出来」不能在这一页被折成 0：每个数都是下界，得把地方说出来。
    ///
    /// 这条防的是"看板上写着本周 0 次而明细页有几十万"：范围与域名在这里都不进句子，
    /// 只有 `gaps` 传得过去，页面漏了这一句就复现了那个症状。
    #[test]
    fn a_partial_read_travels_to_the_page_instead_of_becoming_zero() {
        let board = summarize(DashInput {
            range: DashRange::Month,
            today: "2026-11-04",
            trend: vec![("2026-11-04".to_string(), 0)],
            active_seconds: 0,
            apps: vec![],
            keys: vec![],
            devices: 0,
            streak_days: 0,
            unreadable: vec!["2025 年库每日趋势读不出来: 库打不开".to_string()],
        });
        assert_eq!(board.gaps.len(), 1, "缺读原样交给页面: {:?}", board.gaps);
        // 期望值按被测函数的输出手抄过一次是危险的：这里只核"没被吞掉"这一件事
        assert!(
            board.gaps[0].contains("2025"),
            "点名的年份不能在路上丢掉: {}",
            board.gaps[0]
        );
    }

    /// 连续天数复用打卡条那一个函数：`goal` 钉 1 就等价于「`count > 0`」，
    /// 而「今天还没数据就从昨天起算」「缺日不断」「周末不跳」都不用在这里重写。
    #[test]
    fn the_streak_is_the_goal_strip_ruler_with_the_predicate_pinned_to_one() {
        // 2026-11-04（周三）今天为 0 ⇒ 从昨天往前连续数
        let rows: Vec<(String, i64)> = vec![
            ("2026-11-01".to_string(), 5),
            ("2026-11-02".to_string(), 5),
            ("2026-11-03".to_string(), 5),
            ("2026-11-04".to_string(), 0),
        ];
        assert_eq!(
            streak_days(&rows, "2026-11-04", 0),
            3,
            "今天为 0 时从昨天起算，那三天连着"
        );
        // 今天有一点 ⇒ 今天算进去
        let with_today: Vec<(String, i64)> = vec![
            ("2026-11-02".to_string(), 1),
            ("2026-11-03".to_string(), 1),
            ("2026-11-04".to_string(), 1),
        ];
        assert_eq!(streak_days(&with_today, "2026-11-04", 0), 3);
        // 一次大目标（20000）不该把这一格判成 0：看板的谓词是"有没有活动"
        let big_goal: Vec<(String, i64)> = vec![("2026-11-04".to_string(), 500)];
        assert_eq!(
            streak_days(&big_goal, "2026-11-04", 0),
            1,
            "500 次在打卡条那把尺子上不算达标，在这一格必须算一天"
        );
        // 周末不跳：周六周日连着数进来
        let sat = chrono::NaiveDate::from_ymd_opt(2026, 11, 7).unwrap();
        let sun = chrono::NaiveDate::from_ymd_opt(2026, 11, 8).unwrap();
        assert_eq!(
            sat.weekday().num_days_from_monday(),
            5,
            "夹具前提：这天得是周六"
        );
        let weekend = vec![
            (sat.format("%Y-%m-%d").to_string(), 1),
            (sun.format("%Y-%m-%d").to_string(), 1),
        ];
        assert_eq!(
            streak_days(&weekend, &sun.format("%Y-%m-%d").to_string(), 0),
            2,
            "「连续自然日、周末不跳」是已确认过的口径"
        );
    }

    /// 3.2.1 那张表把 `streak.days` 标成「不受范围影响（快照）」：选「今日」时，
    /// 连续 12 天的纪录也必须照旧念 12，而不是被一格的窗口截成 1。
    ///
    /// 旧写法拿页签那一档的序列去数（今日 1 格 / 本周 7 格 / 本月 30 格），连续纪录
    /// 于是随页签变短；今天还没敲键盘时更糟 —— 从昨天起算的那一段根本不在窗口里，
    /// 屏上会念「连续 0 天」。夹具按日期各自写进它那一年，跨年也不靠日历碰运气。
    #[test]
    fn the_streak_is_not_clipped_by_the_selected_range() {
        let _lock = crate::paths::test_app_dir_lock();
        let _tmp = crate::paths::test_app_dir("dash_streak_wide");
        let today = chrono::Local::now().date_naive();
        for i in 0..12 {
            let d = today - chrono::Duration::days(i);
            let year = d.year();
            let conn = crate::db::connection::open_rw(&crate::paths::year_db_path(year))
                .expect("打开年度库");
            crate::db::connection::ensure_schema(&conn, year).expect("建表");
            conn.execute(
                "INSERT INTO daily_counts (date_key, count) VALUES (?1, 1)",
                [crate::db::queries::day_key_of_date(d)],
            )
            .expect("写一条每日计数");
        }
        crate::db::queries::invalidate_years_cache();
        crate::db::connection::clear_ro_cache();

        let board = collect(DashRange::Today, 0);
        assert_eq!(
            board.kpi[2],
            ("连续天数".to_string(), "12 天".to_string()),
            "今日档把连续 12 天截成了窗口长度：{:?}",
            board.kpi
        );
    }

    /// 三档循环取下一档：默认必须是「本周」（方案 14 节第 3 项按默认执行）。
    #[test]
    fn the_range_cycles_today_week_month() {
        assert_eq!(next_range(DashRange::Today), DashRange::Week);
        assert_eq!(next_range(DashRange::Week), DashRange::Month);
        assert_eq!(next_range(DashRange::Month), DashRange::Today);
        assert_eq!(DashRange::Week.label(), "本周");
    }
}
