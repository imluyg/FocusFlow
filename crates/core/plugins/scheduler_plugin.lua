-- FocusFlow 插件：定时任务
-- 每日定时 / 一次性 / 间隔执行三种调度
-- 依赖宿主 API：focusflow.scheduler_*

PLUGIN_NAME = "定时任务"
PLUGIN_DESC = "定时启动指定程序（每日/一次性/间隔执行）"
PLUGIN_VERSION = "1.0.0"
PLUGIN_AUTHOR = "FocusFlow"

-- 正在编辑的任务 id（-1 = 编辑弹窗关闭）。以前它被声明却从不使用，
-- `scheduler_update` 也零调用 → **GUI 里根本不存在"编辑任务"**，只能删了重建。
local edit_id = -1
-- 编辑弹窗的草稿（由 set_field 写入）
local ed_name = ""
local ed_target = ""
local ed_args = ""
local ed_type = "daily"
local ed_time = ""
local ed_enabled = "1"
local msg = nil

-- 「面板已关闭」由前端在关闭路径上发来（`ui/js/plugins.js` 的 closePlugin）：
-- Lua 状态在面板关掉后仍然活着，不清就会把上一次的结果挂到重开之后。
local PANEL_CLOSED = "__panel_closed"

function init()
    focusflow.log("定时任务插件已初始化")
end

function cleanup()
    -- 回收宿主的调度线程：不叫这句，插件停用后定时任务仍会照常触发
    focusflow.scheduler_shutdown()
    focusflow.log("定时任务插件已清理")
end

-- 把一条任务装进编辑草稿（宿主给的字段名见 host.rs 的 scheduler_tasks）
local function load_draft(t)
    ed_name = t["name"] or ""
    ed_target = t["target"] or ""
    ed_args = t["args"] or ""
    ed_type = t["type"] or "daily"
    ed_time = t["time"] or ""
    ed_enabled = t["enabled"] and "1" or "0"
end

function on_action(id)
    if id == PANEL_CLOSED then
        msg = nil
        edit_id = -1
    elseif id == "refresh" then
        msg = nil
    elseif id == "add" then
        msg = add_sample_task()
    elseif id:match("^toggle_") then
        local tid = tonumber(id:sub(8))
        local tasks = focusflow.scheduler_tasks()
        local target = nil
        for _, t in ipairs(tasks) do
            if t["id"] == tid then
                target = t
                break
            end
        end
        if not target then
            msg = "切换失败：任务 #" .. tostring(tid) .. " 不在列表里（先点刷新）"
        else
            -- 宿主给的是 (是否改成, 原因)：库被写事务占住时不能说成"已启用"
            local ok, why = focusflow.scheduler_toggle(tid, not target["enabled"])
            if ok then
                msg = "任务 #" .. tostring(tid) .. " 已" .. (_status(not target["enabled"]))
            else
                msg = "切换失败：" .. ((why and #why > 0) and why or ("任务 #" .. tostring(tid) .. " 没改成"))
            end
        end
    elseif id:match("^edit_") then
        -- 行内「编辑」：动作 id = "edit_" .. 任务 id
        local tid = tonumber(id:sub(6))
        local target = nil
        if tid then
            for _, t in ipairs(focusflow.scheduler_tasks()) do
                if t["id"] == tid then
                    target = t
                    break
                end
            end
        end
        if not target then
            msg = "编辑失败：任务 #" .. tostring(tid) .. " 不在列表里（先点刷新）"
        else
            load_draft(target)
            edit_id = tid
            msg = nil
        end
    elseif id == "cancel_edit" then
        -- 取消：只关弹窗，草稿留着下次编辑会重新装填
        edit_id = -1
    elseif id == "save_edit" then
        if edit_id < 0 then return end
        -- 宿主给的是 (是否改成, 原因)，与「启用/禁用」「删除」同一形状：
        -- 目标不在白名单、调度格式不对、库写不进去是三件不同的事，得说清是哪一件。
        local ok, why = focusflow.scheduler_update(
            edit_id, ed_name, ed_target, ed_args, ed_type, ed_time, ed_enabled == "1"
        )
        if ok then
            msg = "已更新任务 #" .. tostring(edit_id) .. "：" .. ed_name .. "（"
                .. _status(ed_enabled == "1") .. "）"
            -- 刻意不关弹窗以外的状态：草稿即下一次的真值
            edit_id = -1
        else
            -- 被拒时**不关弹窗**：草稿还在，改完能立刻再保存
            msg = "更新失败：" .. ((why and #why > 0) and why or ("任务 #" .. tostring(edit_id) .. " 没改成"))
        end
    elseif id:match("^del_") then
        local tid = tonumber(id:sub(5))
        -- 宿主给的是 (是否删到, 原因)：库读不出来时不能说成"任务不存在"
        local ok, why = focusflow.scheduler_delete(tid)
        if ok then
            msg = "已删除任务 #" .. tostring(tid)
            if tid == edit_id then edit_id = -1 end
        else
            msg = "删除失败：" .. ((why and #why > 0) and why or ("任务 #" .. tostring(tid) .. " 不存在"))
        end
    end
end

-- 供宿主调用：编辑弹窗里的输入框回写
function set_field(field, value)
    if field == "ed_name" then ed_name = value
    elseif field == "ed_target" then ed_target = value
    elseif field == "ed_args" then ed_args = value
    elseif field == "ed_type" then ed_type = value
    elseif field == "ed_time" then ed_time = value
    elseif field == "ed_enabled" then ed_enabled = value
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
    local ids = {}
    for _, t in ipairs(tasks) do
        rows[#rows + 1] = {
            tostring(t["id"]),
            t["name"],
            t["desc"],
            _status(t["enabled"]),
        }
        -- 行必须带 ids + actions 才会渲染成按钮。以前是把 "toggle_3"/"del_3"
        -- 当普通文本塞进单元格，于是这两列既点不动、又把内部动作 id 直接印在界面上，
        -- 而 on_action 里那两段 ^toggle_ / ^del_ 分支压根到不了 ——
        -- 定时任务在界面上既停不掉也删不掉（只能改库或用 CLI）。
        ids[#ids + 1] = tostring(t["id"])
    end

    local widgets = {
        { type = "heading", text = "任务列表" },
        { type = "button", id = "refresh", text = "刷新" },
        { type = "button", id = "add", text = "添加示例任务" },
        { type = "separator" },
        {
            type = "table",
            headers = { "ID", "名称", "调度", "状态", "操作" },
            rows = rows,
            ids = ids,
            actions = {
                { prefix = "toggle_", text = "启用/禁用" },
                { prefix = "edit_", text = "编辑" },
                { prefix = "del_", text = "删除" },
            },
        },
        -- 编辑任务：以前宿主的 scheduler_update 零调用方、`edit_id` 也从没用过，
        -- 于是 GUI 里只有"删了再建"——而新建那一侧被目标白名单卡着，等于改不了。
        {
            type = "modal_form",
            id = "edit_modal",
            title = edit_id >= 0 and ("编辑任务 #" .. tostring(edit_id)) or "编辑任务",
            submit = "save_edit",
            submit_text = "保存",
            cancel = "cancel_edit",
            open = edit_id >= 0,
            fields = {
                { kind = "text", field = "ed_name", label = "名称", value = ed_name },
                { kind = "text", field = "ed_target", label = "目标程序", value = ed_target },
                { kind = "text", field = "ed_args", label = "参数", value = ed_args },
                { kind = "select", field = "ed_type", label = "调度类型", value = ed_type,
                  options = {
                      { value = "daily", label = "每日" },
                      { value = "once", label = "一次性" },
                      { value = "interval", label = "间隔执行" },
                  } },
                { kind = "text", field = "ed_time", label = "调度时刻", value = ed_time },
                { kind = "select", field = "ed_enabled", label = "状态", value = ed_enabled,
                  options = {
                      { value = "1", label = "启用" },
                      { value = "0", label = "禁用" },
                  } },
            },
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
    widgets[#widgets + 1] = { type = "label", text = "调度时刻：daily 写 HH:MM；once 写 YYYY-MM-DD HH:MM；interval 写 HH:MM-HH:MM|分钟数。改了时刻会连同「上次执行」一起重置。" }
    widgets[#widgets + 1] = { type = "label", text = "示例任务固定启动记事本。要定时启动别的程序：目标必须是 .exe、位于系统安装目录（或写进 config.ini 的 [scheduler] allow_extra），且参数不能是开关/URL 形态。" }

    return {
        title = "定时任务",
        widgets = widgets,
    }
end
