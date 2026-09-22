-- FocusFlow 插件：Edge 浏览器历史记录
-- 今日/总记录数 + 30 天趋势
-- 依赖宿主 API：focusflow.edge_*

PLUGIN_NAME = "Edge历史记录"
PLUGIN_DESC = "查看 Edge 浏览器历史记录数量及 30 天趋势"
PLUGIN_VERSION = "1.0.0"
PLUGIN_AUTHOR = "FocusFlow"

local hint = nil

function init()
    focusflow.log("Edge历史记录插件已初始化")
end

function cleanup()
    focusflow.log("Edge历史记录插件已清理")
end

function on_action(id)
    if id == "refresh" then
        -- 只启动后台刷新：Edge 库读取可能要几百毫秒到数秒（被锁时还要复制整份
        -- 快照），而插件跑在主线程上，同步等会把整个界面冻住。
        if focusflow.edge_update_today() then
            hint = "已启动后台读取，完成后重新打开本面板即可看到最新数值"
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

    local widgets = {
        { type = "heading", text = "概览" },
        { type = "keyvalue", key = "今日记录数", value = today_display },
        { type = "keyvalue", key = "总记录数", value = total_display },
        { type = "keyvalue", key = "近30天峰值", value = tostring(max_count) },
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
