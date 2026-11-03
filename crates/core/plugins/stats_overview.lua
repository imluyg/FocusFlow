-- FocusFlow 示例插件：统计速览
-- 演示宿主 API 调用与声明式 UI

PLUGIN_NAME = "统计速览"
PLUGIN_DESC = "展示当前周期统计的快速概览"
PLUGIN_VERSION = "1.0.0"
PLUGIN_AUTHOR = "FocusFlow"

-- 自刷节奏：这一页读的是内存里的计数，几乎不花钱（每次不到 1 ms），1 秒一次保持原样。
PLUGIN_REFRESH_MS = 1000

function init()
    focusflow.log("统计速览插件已初始化")
end

function cleanup()
    focusflow.log("统计速览插件已清理")
end

function get_view()
    local total, keys, distinct, gap = focusflow.stats(-1)  -- 今日统计
    local today = focusflow.today_count()

    -- 构建排行行（宿主已按次数降序，这里只截前十）
    local rows = {}
    local i = 1
    for _, row in ipairs(keys or {}) do
        rows[i] = { row.name, tostring(row.count) }
        i = i + 1
        if i > 10 then break end
    end

    local widgets = {
        { type = "heading", text = "今日活跃" },
        { type = "keyvalue", key = "总次数", value = tostring(today) },
        { type = "keyvalue", key = "键鼠种类", value = tostring(distinct or #rows) },
        { type = "separator" },
        { type = "heading", text = "键鼠排行（Top 10）" },
        {
            type = "table",
            headers = { "键鼠", "次数" },
            rows = rows,
        },
        { type = "separator" },
        { type = "label", text = "数据来自 focusflow.stats API" },
    }

    -- 第四个返回值是"这次哪一年没答上来"。不接住它，移动盘没醒、`-wal` 被杀软握着
    -- 那一轮，这里念出来的今日数就长得像一份完整答案（宿主从前也只交回三个值）。
    if gap ~= nil and gap ~= "" then
        widgets[#widgets + 1] = {
            type = "label",
            text = "这次没读全：" .. gap .. "；上面的数字是下界，不是没有数据",
        }
    end

    return {
        title = "今日统计速览",
        widgets = widgets,
    }
end
