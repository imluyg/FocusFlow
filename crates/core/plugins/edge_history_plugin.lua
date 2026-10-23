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

-- 缓存里的数值怎么画：读到就画数字；确认没刷过画 "—"；这一问根本答不出来要画
-- "读不出来"。前两者可以并成一格（都是"这里没有数"），后一种不行 ——
-- "从没刷过"的下一步是点「刷新数据」，"读不出来"的下一步是让统计目录可读，
-- 而画成 "—" 时用户会一直点前者。
local function saved_display(value, why)
    if value ~= nil then
        return tostring(value)
    end
    if why ~= nil then
        return "读不出来"
    end
    return "—"
end

function get_view()
    -- 数值只取本地缓存库（上次后台刷新保存的结果）：渲染路径不碰 Edge 库，
    -- 所以打开面板永远不卡。
    local today, today_why = focusflow.edge_saved_today()
    local total, total_why = focusflow.edge_saved_total()
    local today_display = saved_display(today, today_why)
    local total_display = saved_display(total, total_why)
    local state = focusflow.edge_refresh_state()

    -- 30 天趋势（本地缓存库，快）；日期从新到旧排列（近 → 远）
    local counts, counts_why = focusflow.edge_counts(30)
    local max_count = 0
    local rows = {}
    for i = #counts, 1, -1 do
        local c = counts[i]
        if c["count"] > max_count then max_count = c["count"] end
        rows[#rows + 1] = { c["date"], tostring(c["count"]) }
    end
    -- 峰值与今日/总数同一口径：一次都没刷新过时显示 "—"，读不出来时显示"读不出来"。
    -- 以前这里恒显示 0，而 0 在标题写着「近 30 天」的表里是句真话还是假话分不清。
    local peak_display
    if #counts > 0 then
        peak_display = tostring(max_count)
    elseif counts_why ~= nil then
        peak_display = "读不出来"
    else
        peak_display = "—"
    end

    local widgets = {
        { type = "heading", text = "概览" },
        { type = "keyvalue", key = "今日记录数", value = today_display },
        { type = "keyvalue", key = "总记录数", value = total_display },
        { type = "keyvalue", key = "近30天峰值", value = peak_display },
        { type = "button", id = "refresh", text = "刷新数据" },
    }
    if state == "running" then
        widgets[#widgets + 1] = { type = "label", text = "⏳ 正在后台读取 Edge 历史（Edge 运行时会先复制一份快照）…" }
    elseif state == "write_fail" then
        -- 与 fail 分开：这一步 Edge 已经读到了，卡住的是 FocusFlow 自己的缓存库。
        -- 让用户去关 Edge 是错的建议——重试一百次也不会好，要先让 data/ 目录可写。
        widgets[#widgets + 1] = { type = "label", text = "⚠ Edge 读到了，但本程序的统计目录写不进去（同步盘占位、只读移动盘或被杀软锁定），请让该目录可写后再刷新" }
    elseif state == "fail" then
        widgets[#widgets + 1] = { type = "label", text = "⚠ 读取失败：Edge 历史库被占用或不可读（Edge 后台进程可能仍在运行），请稍后重试" }
    elseif state == "no_source" then
        -- 与 fail 分开：这一条不是"Edge 正开着"。程序读的是
        -- %LOCALAPPDATA%\Microsoft\Edge\User Data\Default\History，
        -- 没装 Edge、用的是 Profile 1 之类的其它 Profile、或便携版把数据放别处，都会走到这里。
        -- 叫用户去关浏览器、再点刷新，一百次也不会好。
        widgets[#widgets + 1] = { type = "label", text = "⚠ 没找到 Edge 的历史库（不在默认的 User Data\\Default 里）。没装 Edge、用的是别的 Profile（如 Profile 1）或便携版时就是这样 —— 这跟 Edge 开不开着没关系，具体路径写在日志里" }
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
