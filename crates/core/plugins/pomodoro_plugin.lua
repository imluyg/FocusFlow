-- FocusFlow 插件：番茄工作法
-- 工作/休息定时器，自动记录每个番茄钟的按键数据
-- 依赖宿主 API：focusflow.pomodoro_*

PLUGIN_NAME = "番茄工作法"
PLUGIN_DESC = "番茄钟定时器，自动记录每个番茄钟的按键数据"
PLUGIN_VERSION = "1.0.0"
PLUGIN_AUTHOR = "FocusFlow"

-- 内部状态：当前显示的操作提示（空串 = 不显示）。
-- 以前这句是死代码：从来没人读写它，于是暂停/继续（连 `toggle_pause` 的返回值都
-- 被丢弃）与停止全部**零反馈** —— 用户点了按钮只看到倒计时还在走，不知道按没按上。
local hint = "点击「开始工作」开始一个番茄钟"
-- 时长草稿（分钟）：两个输入框的当前值，点「应用时长」才写进计时器
local d_work = ""
local d_brk = ""

-- 「面板已关闭」由前端在关闭路径上发来（`ui/js/plugins.js` 的 closePlugin）。
-- Lua 状态在面板关掉后仍然活着，不清就会把上一次的提示挂到重开之后。
local PANEL_CLOSED = "__panel_closed"

function _fmt(seconds)
    seconds = math.max(0, seconds)
    local m = math.floor(seconds / 60)
    local s = seconds % 60
    return string.format("%02d:%02d", m, s)
end

function _state_text(code)
    if code == 1 then return "工作中" end
    if code == 2 then return "休息中" end
    return "空闲"
end

function init()
    focusflow.log("番茄工作法插件已初始化")
end

function cleanup()
    focusflow.pomodoro_stop()
    -- stop 只结束当前番茄会话，计时线程还在跑；shutdown 才回收线程
    focusflow.pomodoro_shutdown()
    focusflow.log("番茄工作法插件已清理")
end

-- 供宿主调用：按键联动（每个有效按键触发）
function record_key(_key)
    focusflow.pomodoro_record_key(_key)
end

-- 时长输入是否可用。`tonumber` 收得下 "inf"/"nan"/"1e300" 这类值，宿主的
-- `set_durations` 只在换成 i64 **之后**才夹 1..=180，所以这一道由面板自己把关
-- （`v == v` 挡 NaN，上下界挡 ±inf 与 1e300）。
function _minutes_ok(v)
    return type(v) == "number" and v == v and v >= 1 and v <= 180
end

-- 供宿主调用：按钮动作
function on_action(id)
    if id == PANEL_CLOSED then
        hint = ""
        d_work = ""
        d_brk = ""
    elseif id == "start_work" then
        -- 核心语义：已在工作阶段时 `start_work` 什么都不做（不重新倒数），
        -- 所以提示要说的是"现在在工作"而不是"已重新开始"。
        local was = (focusflow.pomodoro_state()["state"] or 0) == 1
        focusflow.pomodoro_start_work()
        hint = was and "已经在工作阶段里了：要继续按「暂停」，要作废按「跳过」"
            or "已开始一个工作阶段"
    elseif id == "start_break" then
        local was = (focusflow.pomodoro_state()["state"] or 0) == 2
        focusflow.pomodoro_start_break()
        hint = was and "已经在休息阶段里了" or "已开始休息阶段"
    elseif id == "toggle_pause" then
        -- 宿主给的是"切换之后是否处于暂停"，空闲时它恒为 false，只看返回值分不清
        -- 「已继续」与「根本没在计时」，所以再读一次状态码。
        local now_paused = focusflow.pomodoro_toggle_pause()
        local state = focusflow.pomodoro_state()
        if (state["state"] or 0) == 0 then
            hint = "没有在计时的番茄钟：先点「开始工作」，暂停才有意义"
        elseif now_paused then
            hint = "已暂停：倒计时与键鼠计数都停住，再点一次继续"
        else
            hint = "已继续计时"
        end
    elseif id == "stop" then
        -- 停止 = 结束这一段，但**已经计到的时间照常落库并计入今日番茄数**
        -- （核心 `take_current` 只要 actual > 0 就写一行）。要作废请用「跳过」。
        focusflow.pomodoro_stop()
        hint = "已停止：这一段按完成落库，会算进「今日完成」"
    elseif id == "skip" then
        -- 宿主的 pomodoro_skip 早就有"这段作废"的语义（不落库、不计入今日番茄数），
        -- 只是面板一直缺这个按钮：工作到一半只能按停止，于是今日汇总谎报"完成 1 个"。
        focusflow.pomodoro_skip()
        hint = "已跳过：这一段作废，不落库、也不计入「今日完成」"
    elseif id == "apply_durations" then
        local w = tonumber(d_work)
        local b = tonumber(d_brk)
        if not _minutes_ok(w) or not _minutes_ok(b) then
            hint = "时长没改：工作/休息两个框都要填 1..180 之间的分钟数"
        else
            focusflow.pomodoro_set_durations(math.floor(w), math.floor(b))
            local state = focusflow.pomodoro_state()
            hint = "时长已设为 工作 " .. tostring(state["work_minutes"])
                .. " 分钟 / 休息 " .. tostring(state["break_minutes"])
                .. " 分钟：从下一个阶段开始倒数"
            d_work = ""
            d_brk = ""
        end
    end
end

-- 供宿主调用：输入框回写
function set_field(field, value)
    if field == "d_work" then
        d_work = value
    elseif field == "d_brk" then
        d_brk = value
    end
end

function _build_widgets()
    local state = focusflow.pomodoro_state()
    local code = state["state"] or 0
    local remaining = state["remaining"] or 0
    local key_count = state["key_count"] or 0
    local paused = (state["paused"] or 0) == 1
    local work_min = state["work_minutes"] or 25
    local brk_min = state["break_minutes"] or 5
    -- 今日完成取库里的当日汇总，不取计时器里的 work_finished：后者是进程内的
    -- 累加值，重启后归零、跨零点又继续往上加，标成「今日」两头都会说谎。
    local summary_count, summary_keys = focusflow.pomodoro_summary()

    local st = _state_text(code)
    if paused then st = st .. "（已暂停）" end

    local widgets = {
        { type = "heading", text = "计时器" },
        { type = "keyvalue", key = "状态", value = st },
        { type = "keyvalue", key = "倒计时", value = _fmt(remaining) },
        { type = "keyvalue", key = "本阶段键鼠", value = tostring(key_count) },
        { type = "keyvalue", key = "今日完成", value = tostring(summary_count) .. " 个" },
        { type = "keyvalue", key = "今日键鼠", value = tostring(summary_keys) },
        { type = "separator" },
        { type = "button", id = "start_work", text = "开始工作" },
        { type = "button", id = "start_break", text = "开始休息" },
        { type = "button", id = "toggle_pause", text = "暂停/继续" },
        { type = "button", id = "stop", text = "停止（记为完成）" },
        { type = "button", id = "skip", text = "跳过（作废这一段）" },
        { type = "label", text = "「停止」把已计到的时间落库并计入今日完成；「跳过」是作废这一段，既不落库也不计数。" },
    }

    -- 上一次动作的结果。面板打开时给一句引导语，之后每点一次按钮都被改写一次；
    -- 关闭面板时清零（见 on_action 的 PANEL_CLOSED 分支）。
    if hint and hint ~= "" then
        widgets[#widgets + 1] = { type = "label", text = hint }
    end

    widgets[#widgets + 1] = { type = "separator" }

    -- 时长：以前 work_min/brk_min 取出即弃，面板既看不到也改不了
    -- （宿主的 pomodoro_set_durations 零调用方）。
    widgets[#widgets + 1] = { type = "keyvalue", key = "工作时长", value = tostring(work_min) .. " 分钟" }
    widgets[#widgets + 1] = { type = "keyvalue", key = "休息时长", value = tostring(brk_min) .. " 分钟" }
    widgets[#widgets + 1] = {
        type = "row",
        children = {
            { type = "textinput", field = "d_work", label = "新工作分钟", value = d_work },
            { type = "textinput", field = "d_brk", label = "新休息分钟", value = d_brk },
            { type = "button", id = "apply_durations", text = "应用时长" },
        },
    }
    widgets[#widgets + 1] = { type = "label", text = "时长填 1..180 的分钟数，改完从下一个阶段开始倒数（config.ini 的 [pomodoro] 两键是启动时的默认值）。" }
    widgets[#widgets + 1] = { type = "separator" }
    widgets[#widgets + 1] = { type = "heading", text = "历史记录（最近 20 条）" }

    -- 历史表格
    local sessions = focusflow.pomodoro_sessions(20)
    local rows = {}
    for _, s in ipairs(sessions) do
        local tname = "工作"
        if s["type"] == "break" then tname = "休息" end
        rows[#rows + 1] = { tname, s["start"], tostring(s["actual"]) .. "s", tostring(s["keys"]) }
    end
    widgets[#widgets + 1] = {
        type = "table",
        headers = { "类型", "开始时间", "时长", "键鼠数" },
        rows = rows,
    }

    return widgets
end

function get_view()
    return {
        title = "番茄工作法",
        widgets = _build_widgets(),
    }
end
