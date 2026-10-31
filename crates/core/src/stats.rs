//! 打卡与周报的纯计算：每日目标达成、连续天数、最长纪录、"刚结束的那一周"锚点。
//!
//! 这里**不再有** CPM 滑窗与久坐判定：那两件是旧前端时代的形状，原生重写之后
//! 生产端一个读者都没有（CPM 每事件写一次、只有本模块用例读它；久坐那条横带在
//! 原生卡片上没有实现，`desktop-native/src/floating.rs` 的头注释自己写着"没有了"），
//! 2026-10-31 按"删掉每事件都写、没人读的环"这一条一并摘掉。屏幕上「次/分」的
//! 出处不受影响：那是 `desktop-native/src/collect.rs` 那份 60 秒滑窗。

/// 打卡判定的回看窗口（天）。调用方取按日序列时必须用同一个值，否则
/// `best` 会在数据边界上被截断，而两处各自写死数字迟早对不上。
pub const GOAL_LOOKBACK_DAYS: i64 = 370;

/// 打卡判定的**实际**目标次数：0 次不是"没有目标"，而是"每天都达标"——
/// 那样"连续 N 天"就退化成"库里有几天"，所以按 1 兜住。
///
/// 所有喂给 [`goal_status`] 的调用方、以及任何自己数"达标天数"的面（周报）都必须
/// 过这个函数。原来只有 `goal_status` 内部 `goal.max(1)`，设置页那边自己又写了个
/// `.max(1)`，而周报拿的是**原始配置值**去数达标天数 ⇒ 同一个人把 `daily_keys` 设成
/// 0 时，一份周报里"达标 X 天"按 0 算（每周都满分）、"连续 N 天"按 1 算 ——
/// 三处三把尺子，正是上面那条常量注释禁止的形状。
pub fn effective_goal(goal: i64) -> i64 {
    goal.max(1)
}

/// 每日目标与连续打卡的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct GoalStatus {
    pub goal: i64,
    /// 今日次数（库值 + 调用方并入的未落库增量，见 [`goal_status`] 的 `today_pending`）
    pub today: i64,
    /// 今日是否已达标
    pub today_met: bool,
    /// 连续达标天数。今天还没达标时，从昨天往回数——否则每天零点一到
    /// 连续记录就会清零，用户会看到「连了 20 天突然变 0」。
    pub streak: i64,
    /// 回看窗口内的最长连续纪录
    pub best: i64,
    /// 最近 7 天（旧→新）：(日期, 次数, 是否达标)
    pub days: Vec<(String, i64, bool)>,
}

/// 由「按日计数」序列算出每日目标达成与连续打卡。
///
/// `rows` 来自 `db::get_daily_counts`（升序、只含库里有的日期）；缺口日期按 0 处理。
/// `today_pending` 是今天**尚未落库**的增量，并进今天那一行之后再做全部判定 ——
/// 今日数、达标、连续、最长、近 7 天读的是同一张 map，不会出现「进度条已过线、
/// 连续打卡还少一天」这种同一页两把尺子。顶部卡片的「今日」由 live 推送实时
/// 修正（库值 + 内存增量），本函数不并上它的话，设置页这半边就停在上次落库为止。
/// 纯函数，便于覆盖跨年、缺口、今天未达标这些真实会踩到的边界。
pub fn goal_status(
    goal: i64,
    rows: &[(String, i64)],
    today: &str,
    today_pending: i64,
) -> GoalStatus {
    use chrono::{Local, NaiveDate};
    let mut map: std::collections::HashMap<&str, i64> =
        rows.iter().map(|(d, c)| (d.as_str(), *c)).collect();
    // 负数按 0 兜住：没有"未落库"可扣，也不能让今天的数变小。
    if today_pending > 0 {
        *map.entry(today).or_insert(0) += today_pending;
    }
    let goal = effective_goal(goal);
    let count_of = |d: NaiveDate| -> i64 {
        map.get(d.format("%Y-%m-%d").to_string().as_str())
            .copied()
            .unwrap_or(0)
    };
    let today_d =
        NaiveDate::parse_from_str(today, "%Y-%m-%d").unwrap_or_else(|_| Local::now().date_naive());

    let today_count = count_of(today_d);
    let today_met = today_count >= goal;

    // 连续天数：今天达标则 +1，再从昨天往回数。今天还没达标不清零——
    // 否则每天零点一到，昨天的纪录就凭空消失，用户会看到「连了 20 天突然变 0」。
    let mut streak = if today_met { 1 } else { 0 };
    let mut day = today_d - chrono::Duration::days(1);
    while count_of(day) >= goal {
        streak += 1;
        day -= chrono::Duration::days(1);
    }

    // 最长纪录：与 streak 一样在**日期轴**上数，不能直接遍历 rows。零活动的那天
    // 压根没有 daily_counts 行，按行遍历等于把缺失的间隔当成连续 ——
    // 9/1 和 9/3 各达标一次会被数成「连续 2 天」，而 streak 用日期轴算出来的是 1，
    // 两个口径互相打架（设置页显示的"最长"因此可能比真实值大）。
    let mut best = 0i64;
    let mut run = 0i64;
    let mut d = today_d - chrono::Duration::days(GOAL_LOOKBACK_DAYS - 1);
    while d <= today_d {
        if count_of(d) >= goal {
            run += 1;
            best = best.max(run);
        } else {
            run = 0;
        }
        d += chrono::Duration::days(1);
    }

    let days = (0..7i64)
        .rev()
        .map(|i| {
            let d = today_d - chrono::Duration::days(i);
            let key = d.format("%Y-%m-%d").to_string();
            let c = count_of(d);
            (key, c, c >= goal)
        })
        .collect();

    GoalStatus {
        goal,
        today: today_count,
        today_met,
        streak,
        best,
        days,
    }
}

/// 「刚结束的那一周」= 上一个完整的周一~周日。
///
/// 周报只能在整周结束后生成，所以锚点是本周一：不管今天周二还是周日，
/// 算出来的都是同一个区间，进程重启也不会多生成一份别的周。
pub fn last_finished_week(today: chrono::NaiveDate) -> (chrono::NaiveDate, chrono::NaiveDate) {
    use chrono::{Datelike, Duration, Weekday};
    let since_monday = match today.weekday() {
        Weekday::Mon => 0,
        Weekday::Tue => 1,
        Weekday::Wed => 2,
        Weekday::Thu => 3,
        Weekday::Fri => 4,
        Weekday::Sat => 5,
        Weekday::Sun => 6,
    };
    let this_monday = today - Duration::days(since_monday);
    (
        this_monday - Duration::days(7),
        this_monday - Duration::days(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(v: &[(&str, i64)]) -> Vec<(String, i64)> {
        v.iter().map(|(d, c)| (d.to_string(), *c)).collect()
    }

    /// 今天还没达标时，已有的连续纪录不得被清零。
    #[test]
    fn streak_survives_unfinished_today() {
        let s = goal_status(
            20000,
            &rows(&[
                ("2026-09-19", 30000),
                ("2026-09-20", 25000),
                ("2026-09-21", 500),
            ]),
            "2026-09-21",
            0,
        );
        assert!(!s.today_met);
        assert_eq!(s.streak, 2, "零点不该把昨天的纪录抹掉");
        assert_eq!(s.best, 2);
    }

    /// 达标日必须连续：中间断一天就重新计数，但最长纪录仍记住断点之前。
    #[test]
    fn streak_breaks_on_gap_and_best_keeps_longest_run() {
        let s = goal_status(
            20000,
            &rows(&[
                ("2026-09-15", 40000),
                ("2026-09-16", 40000),
                ("2026-09-17", 40000),
                ("2026-09-18", 1),
                ("2026-09-19", 1),
                ("2026-09-20", 40000),
                ("2026-09-21", 40000),
            ]),
            "2026-09-21",
            0,
        );
        assert!(s.today_met);
        assert_eq!(s.streak, 2, "断了两天之后不该接上更早的三连");
        assert_eq!(s.best, 3);
        assert_eq!(s.days.len(), 7);
        assert_eq!(s.days[6], ("2026-09-21".to_string(), 40000, true));
        assert_eq!(s.days[4].0, "2026-09-19");
        assert!(!s.days[4].2);
    }

    /// 空库 / 没有当日记录：全 0，不 panic、不除零；非法目标值按 1 处理。
    #[test]
    fn goal_status_handles_empty_history() {
        let s = goal_status(20000, &[], "2026-09-21", 0);
        assert_eq!((s.today, s.streak, s.best, s.today_met), (0, 0, 0, false));
        assert_eq!(s.days.len(), 7);
        assert!(s.days.iter().all(|(_, c, met)| *c == 0 && !*met));
        assert_eq!(
            goal_status(0, &rows(&[("2026-09-21", 1)]), "2026-09-21", 0).streak,
            1,
            "目标 0 会让任何非零天数都永不达标，按 1 兜住"
        );
    }

    /// 缺口日期必须按 0 算：零活动的那天没有 daily_counts 行，如果按"行"数连续，
    /// 9/1 与 9/3 两次达标会被报成「最长 2 天」—— 而 streak 走的是日期轴，
    /// 两个口径会自相矛盾。
    #[test]
    fn goal_best_counts_calendar_days_not_rows() {
        let goal = 20000;
        let s = goal_status(
            goal,
            &rows(&[
                ("2026-08-30", 25000),
                // 8/31 整天没有活动 → 库里没有这一行
                ("2026-09-01", 25000),
                ("2026-09-02", 25000),
                ("2026-09-03", 25000),
            ]),
            "2026-09-03",
            0,
        );
        assert_eq!(s.streak, 3, "9/1..9/3 是真连续");
        assert_eq!(s.best, 3, "缺口把 8/30 与 9/1 隔开，最长只能是 3");
    }

    /// 目标打卡要把今天未落库的增量并进**整份**判定（§七·2 日均同族的最后一处）。
    ///
    /// 顶部卡片与打卡条的「今日 x / 目标 y」已被 live 推送实时补丁（库值 + 内存
    /// 增量），本函数若仍用纯库值，用户刚跨线的那个 flush 周期里会看到
    /// 「今日 20,500 / 20,000」进度条已绿，而「连续打卡 N 天」还少算今天 ——
    /// 同一页两把尺子；最长纪录也可能因此比连续小 1（streak > best 的矛盾）。
    /// 注回旧行为（忽略 `today_pending` 参数）这条必红。
    #[test]
    fn goal_status_counts_today_pending_in_streak_best_and_days() {
        // 库里今天 19,000 + 未落库 1,500，目标 20,000：刚跨线
        let s = goal_status(
            20000,
            &rows(&[("2026-09-25", 20000), ("2026-09-26", 19000)]),
            "2026-09-26",
            1500,
        );
        assert_eq!(s.today, 20500, "今日 = 落库 + 未落库");
        assert!(s.today_met);
        assert_eq!(s.streak, 2, "今天跨线后连续要立刻算上今天");
        assert_eq!(s.best, 2, "最长与连续同口径，不能出现连续 > 最长");
        assert_eq!(
            s.days[6],
            ("2026-09-26".to_string(), 20500, true),
            "近 7 天的今天那行也用同一个数"
        );

        // 库里还没有今天的行（纯函数直接调用、或序列不含今天）：增量独立成行
        let s2 = goal_status(20000, &rows(&[("2026-09-25", 20000)]), "2026-09-26", 20000);
        assert!(s2.today_met);
        assert_eq!(s2.streak, 2);

        // 未落库为 0（周报那条路）：与不含增量的口径一致；负数按 0 兜住
        let base = rows(&[("2026-09-25", 20000), ("2026-09-26", 19000)]);
        let a = goal_status(20000, &base, "2026-09-26", 0);
        let b = goal_status(20000, &base, "2026-09-26", -5);
        assert_eq!(a, b, "负数不能让今天的数变小");
        assert_eq!((a.today, a.today_met, a.streak), (19000, false, 1));
    }

    /// 周报锚点：永远是「上一个完整周」，且同一周内天天算出同一个区间。
    #[test]
    fn last_finished_week_is_the_previous_full_week() {
        use chrono::NaiveDate as D;
        let d = |y, m, day| D::from_ymd_opt(y, m, day).unwrap();
        // 2026-09-22 是周二 → 上周一 09-14 ~ 上周日 09-20
        assert_eq!(
            last_finished_week(d(2026, 9, 22)),
            (d(2026, 9, 14), d(2026, 9, 20))
        );
        // 周一当天：本周一 09-21，上一个整周仍是 09-14~09-20（与周二同区间 → 一周只生成一次）
        assert_eq!(
            last_finished_week(d(2026, 9, 21)),
            (d(2026, 9, 14), d(2026, 9, 20))
        );
        // 周日当天不能把自己这一周算进去
        assert_eq!(
            last_finished_week(d(2026, 9, 20)),
            (d(2026, 9, 7), d(2026, 9, 13))
        );
        // 跨年
        assert_eq!(
            last_finished_week(d(2026, 1, 1)),
            (d(2025, 12, 22), d(2025, 12, 28))
        );
    }

    /// 目标 0 次不是"没目标"，而是"每天都达标"⇒ 必须按 1 兜（三个面共用这一把尺子）。
    #[test]
    fn effective_goal_floors_zero_but_keeps_the_configured_value() {
        assert_eq!(effective_goal(0), 1, "0 会让每一天都算达标");
        assert_eq!(effective_goal(-5), 1, "负数同理，不能被当成「没目标」");
        assert_eq!(effective_goal(20000), 20000, "正常值一字不动");
    }
}
