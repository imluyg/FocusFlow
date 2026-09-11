-- FocusFlow 插件：定时任务
-- 每日定时 / 一次性 / 间隔执行三种调度
-- 依赖宿主 API：focusflow.scheduler_*

PLUGIN_NAME = "定时任务"
PLUGIN_DESC = "定时启动指定程序（每日/一次性/间隔执行）"
PLUGIN_VERSION = "1.0.0"
PLUGIN_AUTHOR = "FocusFlow"

local edit_id = -1

function init()
    focusflow.log("定时任务插件已初始化")
end

function cleanup()
    focusflow.log("定时任务插件已清理")
end

function on_action(id)
    if id == "refresh" then
        -- 刷新即可
    elseif id == "add" then
        local ok, msg = focusflow.scheduler_validate("daily", "09:00")
        if ok then
            local new_id = focusflow.scheduler_add(
                "新任务",
                "C:\\Windows\\notepad.exe",
                "",
                "daily",
                "09:00",
                true
            )
            if new_id > 0 then
                focusflow.log("已添加任务 #" .. tostring(new_id))
            end
        end
    elseif id:match("^toggle_") then
        local tid = tonumber(id:sub(8))
        local tasks = focusflow.scheduler_tasks()
        for _, t in ipairs(tasks) do
            if t["id"] == tid then
                focusflow.scheduler_toggle(tid, not t["enabled"])
                break
            end
        end
    elseif id:match("^del_") then
        local tid = tonumber(id:sub(5))
        focusflow.scheduler_delete(tid)
    end
end

function _status(enabled)
    if enabled then return "启用" end
    return "禁用"
end

function get_view()
    local tasks = focusflow.scheduler_tasks()
    local rows = {}
    for _, t in ipairs(tasks) do
        rows[#rows + 1] = {
            tostring(t["id"]),
            t["name"],
            t["desc"],
            _status(t["enabled"]),
            "toggle_" .. tostring(t["id"]),
            "del_" .. tostring(t["id"]),
        }
    end

    return {
        title = "定时任务",
        widgets = {
            { type = "heading", text = "任务列表" },
            { type = "button", id = "refresh", text = "刷新" },
            { type = "button", id = "add", text = "添加示例任务" },
            { type = "separator" },
            {
                type = "table",
                headers = { "ID", "名称", "调度", "状态", "启用/禁用", "删除" },
                rows = rows,
            },
            { type = "separator" },
            { type = "label", text = "调度类型：daily=每日 / once=一次性 / interval=窗口间隔" },
        },
    }
end
