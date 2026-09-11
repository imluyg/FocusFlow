-- FocusFlow 示例插件：统计速览
-- 演示宿主 API 调用与声明式 UI

PLUGIN_NAME = "统计速览"
PLUGIN_DESC = "展示当前周期统计的快速概览"
PLUGIN_VERSION = "1.0.0"
PLUGIN_AUTHOR = "FocusFlow"

function init()
    focusflow.log("统计速览插件已初始化")
end

function cleanup()
    focusflow.log("统计速览插件已清理")
end

function get_view()
    local total, keys = focusflow.stats(-1)  -- 今日统计
    local today = focusflow.today_count()

    -- 构建排行行
    local rows = {}
    local i = 1
    for k, v in pairs(keys) do
        rows[i] = {k, tostring(v)}
        i = i + 1
        if i > 10 then break end
    end

    return {
        title = "今日统计速览",
        widgets = {
            { type = "heading", text = "今日活跃" },
            { type = "keyvalue", key = "总次数", value = tostring(today) },
            { type = "keyvalue", key = "键鼠种类", value = tostring(#rows) },
            { type = "separator" },
            { type = "heading", text = "键鼠排行（Top 10）" },
            {
                type = "table",
                headers = { "键鼠", "次数" },
                rows = rows,
            },
            { type = "separator" },
            { type = "label", text = "数据来自 focusflow.stats API" },
        },
    }
end
