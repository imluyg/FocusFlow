-- FocusFlow 插件：Edge 浏览器历史记录
-- 今日/总记录数 + 30 天趋势
-- 依赖宿主 API：focusflow.edge_*

PLUGIN_NAME = "Edge历史记录"
PLUGIN_DESC = "查看 Edge 浏览器历史记录数量及 30 天趋势"
PLUGIN_VERSION = "1.0.0"
PLUGIN_AUTHOR = "FocusFlow"

-- 上一次动作的结果（nil = 不显示）
local hint = nil

-- 「面板已关闭」由前端在关闭路径上发来（`ui/js/plugins.js` 的 closePlugin）。
-- Lua 状态在面板关掉后仍然活着：不清的话这句提示会一直挂到下一次点「刷新数据」。
local PANEL_CLOSED = "__panel_closed"

function init()
    focusflow.log("Edge历史记录插件已初始化")
end

function cleanup()
    focusflow.log("Edge历史记录插件已清理")
end

function on_action(id)
    if id == PANEL_CLOSED then
        hint = nil
    elseif id == "refresh" then
        -- 只启动后台刷新：Edge 库读取可能要几百毫秒到数秒（被锁时还要复制整份
        -- 快照），而插件跑在主线程上，同步等会把整个界面冻住。
        if focusflow.edge_update_today() then
            -- 别说"重新打开本面板才看到最新值"：`get_view` 每次渲染都会重跑，
            -- 前端每 2 秒推送一次数据，后台读完的那一帧自己就上屏了。
            hint = "已启动后台读取，读完后这个面板的数值会自己更新（不用重开面板）"
        else
            hint = "上一次读取还没结束，稍后再试"
        end
        focusflow.log(hint)
    end
end

function get_view()
    -- 数值只取本地缓存库（上次后台刷新保存的结果）：渲染路径不碰 Edge 库，
    -- 所以打开面板永远不卡。从未刷新过时显示 "—"。
    local today = focusflow.edge_saved_today()
    local total = focusflow.edge_saved_total()
    local today_display = today and tostring(today) or "—"
    local total_display = total and tostring(total) or "—"
    local state = focusflow.edge_refresh_state()

    -- 30 天趋势（本地缓存库，快）；日期从新到旧排列（近 → 远）
    local counts = focusflow.edge_counts(30)
    local max_count = 0
    local rows = {}
    for i = #counts, 1, -1 do
        local c = counts[i]
        if c["count"] > max_count then max_count = c["count"] end
        rows[#rows + 1] = { c["date"], tostring(c["count"]) }
    end
    -- 峰值与今日/总数同一口径：一次都没刷新过时显示 "—"。
    -- 以前这里恒显示 0，而 0 在标题写着「近 30 天」的表里是句真话还是假话分不清。
    local peak_display = (#counts > 0) and tostring(max_count) or "—"

    local widgets = {
        { type = "heading", text = "概览" },
        { type = "keyvalue", key = "今日记录数", value = today_display },
        { type = "keyvalue", key = "总记录数", value = total_display },
        { type = "keyvalue", key = "近30天峰值", value = peak_display },
        { type = "button", id = "refresh", text = "刷新数据" },
    }
    if state == "running" then
        widgets[#widgets + 1] = { type = "label", text = "⏳ 正在后台读取 Edge 历史（Edge 运行时会先复制一份快照）…" }
    elseif state == "fail" then
        widgets[#widgets + 1] = { type = "label", text = "⚠ 读取失败：Edge 历史库被占用或不可读（Edge 后台进程可能仍在运行），请稍后重试" }
    end
    if hint then
        widgets[#widgets + 1] = { type = "label", text = hint }
    end
    widgets[#widgets + 1] = { type = "separator" }
    widgets[#widgets + 1] = { type = "heading", text = "近 30 天趋势" }
    widgets[#widgets + 1] = { type = "table", headers = { "日期", "记录数" }, rows = rows }
    widgets[#widgets + 1] = { type = "separator" }
    widgets[#widgets + 1] = { type = "label", text = "点击「刷新数据」在后台从 Edge 浏览器读取最新记录，不阻塞界面" }

    return {
        title = "Edge 历史记录",
        widgets = widgets,
    }
end
