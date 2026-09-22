-- FocusFlow 插件：定时任务
-- 每日定时 / 一次性 / 间隔执行三种调度
-- 依赖宿主 API：focusflow.scheduler_*

PLUGIN_NAME = "定时任务"
PLUGIN_DESC = "定时启动指定程序（每日/一次性/间隔执行）"
PLUGIN_VERSION = "1.0.0"
PLUGIN_AUTHOR = "FocusFlow"

local edit_id = -1
local msg = nil

function init()
    focusflow.log("定时任务插件已初始化")
end

function cleanup()
    -- 回收宿主的调度线程：不叫这句，插件停用后定时任务仍会照常触发
    focusflow.scheduler_shutdown()
    focusflow.log("定时任务插件已清理")
end

function on_action(id)
    if id == "refresh" then
        msg = nil
    elseif id == "add" then
        msg = add_sample_task()
    elseif id:match("^toggle_") then
        local tid = tonumber(id:sub(8))
        local tasks = focusflow.scheduler_tasks()
        for _, t in ipairs(tasks) do
            if t["id"] == tid then
                focusflow.scheduler_toggle(tid, not t["enabled"])
                msg = "任务 #" .. tostring(tid) .. " 已" .. (_status(not t["enabled"]))
                break
            end
        end
    elseif id:match("^del_") then
        local tid = tonumber(id:sub(5))
        if focusflow.scheduler_delete(tid) then
            msg = "已删除任务 #" .. tostring(tid)
        else
            msg = "删除失败：任务 #" .. tostring(tid) .. " 不存在"
        end
    end
end

-- 目标程序与参数由宿主白名单校验（canonicalize 后必须位于系统安装目录，
-- 且在 [scheduler] 白名单内）。被拒时必须把原因显示出来：否则点「添加」
-- 像是没反应，用户只会反复点。
function add_sample_task()
    local target = "C:\\Windows\\notepad.exe"
    local ok_sched, sched_msg = focusflow.scheduler_validate("daily", "09:00")
    if not ok_sched then
        return "调度配置无效：" .. sched_msg
    end
    local ok_target, target_msg = focusflow.scheduler_check_target(target, "")
    if not ok_target then
        focusflow.log("示例任务被拒绝：" .. target_msg)
        return "示例任务被拒绝：" .. target_msg
    end
    local new_id = focusflow.scheduler_add("新任务", target, "", "daily", "09:00", true)
    if new_id > 0 then
        focusflow.log("已添加任务 #" .. tostring(new_id))
        return "已添加任务 #" .. tostring(new_id) .. "（每日 09:00 启动记事本）"
    end
    return "添加失败：目标被拒绝或调度库写入失败（详见日志）"
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

    local widgets = {
        { type = "heading", text = "任务列表" },
        { type = "button", id = "refresh", text = "刷新" },
        { type = "button", id = "add", text = "添加示例任务" },
        { type = "separator" },
        {
            type = "table",
            headers = { "ID", "名称", "调度", "状态", "启用/禁用", "删除" },
            rows = rows,
        },
    }
    -- 上一次操作的结果：被白名单拒绝、删除成功与否都要在面板上说出来，
    -- 只写日志的话用户看到的就是「点了没反应」。
    if msg then
        widgets[#widgets + 1] = { type = "separator" }
        widgets[#widgets + 1] = { type = "label", text = msg }
    end
    widgets[#widgets + 1] = { type = "separator" }
    widgets[#widgets + 1] = { type = "label", text = "调度类型：daily=每日 / once=一次性 / interval=窗口间隔" }
    widgets[#widgets + 1] = { type = "label", text = "示例任务固定启动记事本。要定时启动别的程序：目标必须是 .exe、位于系统安装目录（或写进 config.ini 的 [scheduler] allow_extra），且参数不能是开关/URL 形态。" }

    return {
        title = "定时任务",
        widgets = widgets,
    }
end
