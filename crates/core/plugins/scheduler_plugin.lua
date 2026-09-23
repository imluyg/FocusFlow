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

-- 目标程序、参数与调度都由宿主校验，被拒时 scheduler_add 直接给出原因。
-- 不再先调 scheduler_check_target / scheduler_validate 预检一遍：那两道预检和
-- 入库之间文件可能被改（TOCTOU），而 add 的返回值本来就是同一条判定链。
function add_sample_task()
    local target = "C:\\Windows\\notepad.exe"
    local new_id, why = focusflow.scheduler_add("新任务", target, "", "daily", "09:00", true)
    if new_id > 0 then
        focusflow.log("已添加任务 #" .. tostring(new_id))
        return "已添加任务 #" .. tostring(new_id) .. "（每日 09:00 启动记事本）"
    end
    focusflow.log("示例任务被拒绝：" .. why)
    return "示例任务被拒绝：" .. why
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
