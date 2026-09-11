-- FocusFlow 插件：番茄工作法
-- 工作/休息定时器，自动记录每个番茄钟的按键数据
-- 依赖宿主 API：focusflow.pomodoro_*

PLUGIN_NAME = "番茄工作法"
PLUGIN_DESC = "番茄钟定时器，自动记录每个番茄钟的按键数据"
PLUGIN_VERSION = "1.0.0"
PLUGIN_AUTHOR = "FocusFlow"

-- 内部状态：当前显示的操作提示
local hint = "点击「开始工作」开始一个番茄钟"

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
    focusflow.log("番茄工作法插件已清理")
end

-- 供宿主调用：按键联动（每个有效按键触发）
function record_key(_key)
    focusflow.pomodoro_record_key(_key)
end

-- 供宿主调用：按钮动作
function on_action(id)
    if id == "start_work" then
        focusflow.pomodoro_start_work()
    elseif id == "start_break" then
        focusflow.pomodoro_start_break()
    elseif id == "toggle_pause" then
        focusflow.pomodoro_toggle_pause()
    elseif id == "stop" then
        focusflow.pomodoro_stop()
    end
end

function _build_widgets()
    local state = focusflow.pomodoro_state()
    local code = state["state"] or 0
    local remaining = state["remaining"] or 0
    local key_count = state["key_count"] or 0
    local work_finished = state["work_finished"] or 0
    local paused = (state["paused"] or 0) == 1
    local work_min = state["work_minutes"] or 25
    local brk_min = state["break_minutes"] or 5
    local summary_count, summary_keys = focusflow.pomodoro_summary()

    local st = _state_text(code)
    if paused then st = st .. "（已暂停）" end

    local widgets = {
        { type = "heading", text = "计时器" },
        { type = "keyvalue", key = "状态", value = st },
        { type = "keyvalue", key = "倒计时", value = _fmt(remaining) },
        { type = "keyvalue", key = "本阶段键鼠", value = tostring(key_count) },
        { type = "keyvalue", key = "今日完成", value = tostring(work_finished) .. " 个" },
        { type = "keyvalue", key = "今日键鼠", value = tostring(summary_keys) },
        { type = "separator" },
        { type = "button", id = "start_work", text = "开始工作" },
        { type = "button", id = "start_break", text = "开始休息" },
        { type = "button", id = "toggle_pause", text = "暂停/继续" },
        { type = "button", id = "stop", text = "停止" },
        { type = "separator" },
        { type = "heading", text = "历史记录（最近 20 条）" },
    }

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
