-- FocusFlow 插件：Edge 浏览器历史记录
-- 今日/总记录数 + 30 天趋势
-- 依赖宿主 API：focusflow.edge_*

PLUGIN_NAME = "Edge历史记录"
PLUGIN_DESC = "查看 Edge 浏览器历史记录数量及 30 天趋势"
PLUGIN_VERSION = "1.0.0"
PLUGIN_AUTHOR = "FocusFlow"

local cached_today = -1
local cached_total = -1
local refresh_error = nil

function init()
    -- 恢复上次刷新的数值（存于本地缓存库），重启后不再显示 "—"
    cached_today = focusflow.edge_saved_today() or -1
    cached_total = focusflow.edge_saved_total() or -1
    focusflow.log("Edge历史记录插件已初始化")
end

function cleanup()
    focusflow.log("Edge历史记录插件已清理")
end

function on_action(id)
    if id == "refresh" then
        refresh_error = nil
        local ok, today, total = focusflow.edge_update_today()
        if ok then
            cached_today = today
            cached_total = total
            focusflow.log("已更新 Edge 历史：今日 " .. tostring(today) .. "，总计 " .. tostring(total))
        else
            refresh_error = "读取失败：Edge 历史库被占用或不可读（Edge 后台进程可能仍在运行），请稍后重试"
            focusflow.log(refresh_error)
        end
    end
end

function get_view()
    -- 延迟查询：首次打开不自动查 Edge 库（可能很大/被锁定），
    -- 用户点击"刷新数据"才执行，避免加载卡顿
    local today_display = "—"
    local total_display = "—"
    if cached_today >= 0 then
        today_display = tostring(cached_today)
        total_display = tostring(cached_total)
    end

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
    if refresh_error then
        widgets[#widgets + 1] = { type = "label", text = "⚠ " .. refresh_error }
    end
    widgets[#widgets + 1] = { type = "separator" }
    widgets[#widgets + 1] = { type = "heading", text = "近 30 天趋势" }
    widgets[#widgets + 1] = { type = "table", headers = { "日期", "记录数" }, rows = rows }
    widgets[#widgets + 1] = { type = "separator" }
    widgets[#widgets + 1] = { type = "label", text = "点击「刷新数据」从 Edge 浏览器读取最新记录（可能较慢）" }

    return {
        title = "Edge 历史记录",
        widgets = widgets,
    }
end
