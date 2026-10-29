-- FocusFlow 插件：记账本（对标 Python 版）
-- 收支记录增删改查、分类/子分类筛选、日期范围、关键词搜索、分页、
-- 月度汇总（含分类明细）、分类盈亏、细分盈亏、距今多久
-- 依赖宿主 API：focusflow.accounting_*

PLUGIN_NAME = "记账本"
PLUGIN_DESC = "收支记录管理：增删改查、分类/子分类筛选、日期范围、关键词、分页、月度汇总、盈亏统计"
PLUGIN_VERSION = "1.2.0"
PLUGIN_AUTHOR = "FocusFlow"

-- 新增/编辑草稿（弹窗字段，set_field 写入）
local draft = { type = "支出", item = "", store = "", amount = "", category = "", subcategory = "", date = "", note = "" }
local editing_id = nil -- 非 nil 时编辑弹窗打开

-- 筛选条件
local f_cat = "全部"
local f_sub = "全部"
local f_kw = ""
local f_from = ""
local f_to = ""

-- 分页
local page = 1
local PAGE_SIZE = 10

-- 统计结果与细分盈亏选择
local result_text = ""
-- 上一次动作的可见结果（成功与**被拒**都要有话）。
-- 只 `focusflow.log` 等于没说：弹窗是先关后发动作的（`ui/js/plugins.js` 的
-- `modalSubmit`），于是校验失败/写库失败的表现是"弹窗关了、列表里没这条"，
-- 一个字都没有 —— 与定时任务那边已修的"被拒时必须说清原因"同一族。
local list_msg = ""
local picker_open = false
local profit_cat = ""

-- 分类管理面板状态
local manage_open = false
local m_cat = ""
local m_name = "" -- 添加/重命名分类名
local m_type = "both"
-- 「修改分类」弹窗里这一轮没能读出这个分类的类型。true 时下拉里显示的是默认值，
-- 保存必须只改名（第三个参数传空串 = 保持原类型），不许把那个我们不知道的值写回去。
local m_type_unknown = false
local m_sub_name = "" -- 添加/重命名子分类名
local m_edit_old = "" -- 编辑中的原分类名
local m_edit_open = false -- 分类编辑小弹窗
local m_sub_edit_old = "" -- 编辑中的原子分类名
local m_sub_edit_open = false -- 子分类编辑小弹窗

function init()
    focusflow.log("记账本插件已初始化")
end

function cleanup()
    focusflow.log("记账本插件已清理")
end

-- 「面板已关闭」由前端在关闭路径上发来（`ui/js/plugins.js` 的 closePlugin）。
-- 面板关掉并不清空 Lua 状态：以前重开记账面板时 edit_modal / profit_modal /
-- 统计结果弹窗会自己弹出来，人也直接落在分类管理子页上。
-- 刻意**保留**筛选条件与页码（那是用户选的"看哪一批"，不是一次性状态，
-- 要清有「重置」按钮）。
local PANEL_CLOSED = "__panel_closed"

local function reset_transient_state()
    editing_id = nil
    draft.type = "支出"; draft.item = ""; draft.store = ""; draft.amount = ""
    draft.category = ""; draft.subcategory = ""; draft.date = ""; draft.note = ""
    result_text = ""
    list_msg = ""
    picker_open = false
    profit_cat = ""
    manage_open = false
    m_cat = ""
    m_name = ""
    m_sub_name = ""
    m_edit_old = ""
    m_edit_open = false
    -- 面板关了再开，"这一轮没读出类型"不许还挂着：挂着的话下一次保存会静默不改类型
    m_type_unknown = false
    m_sub_edit_old = ""
    m_sub_edit_open = false
end

function _today()
    return os.date("%Y-%m-%d")
end

-- 某一天是否存在（闰年 2 月 29 日这种"数字都对、日子不存在"的写法要挡掉）
local function days_in_month(y, m)
    local lens = { 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31 }
    if m == 2 and y % 4 == 0 and (y % 100 ~= 0 or y % 400 == 0) then return 29 end
    return lens[m]
end

-- 日期筛选的规范化：`2026-9-1` / `2026/09/01` / `2026.9.1` / `20260901` 一律收成
-- 补零的 `YYYY-MM-DD`，认不出来返回 nil。
--
-- 为什么两边（面板与宿主 `accounting.normalize_date_filter`）都要做：`purchase_date`
-- 存的是补零形态，而日期筛选是**字符串比较** —— 少一个零就静默漏：
-- `"2026-9-1" > "2026-09-05"`（字节序里 `'9' > '0'`），"从 2026-9-1 起"恰好把
-- 整个九月上旬筛掉，一个错都不报。同一个弹窗里的日期控件却是补零的，
-- 同一列两套口径。面板这一侧还要负责把话说明白（宿主只会"不筛"，没有反馈通道）。
function norm_date(s)
    if type(s) ~= "string" then return nil end
    local t = s:match("^%s*(.-)%s*$")
    if t == "" then return nil end
    local y, m, d = t:match("^(%d+)%D+(%d+)%D+(%d+)$")
    if not y and t:match("^%d%d%d%d%d%d%d%d$") then
        y, m, d = t:sub(1, 4), t:sub(5, 6), t:sub(7, 8)
    end
    if not y then return nil end
    y, m, d = tonumber(y), tonumber(m), tonumber(d)
    if y < 1970 or y > 9999 or m < 1 or m > 12 or d < 1 or d > days_in_month(y, m) then
        return nil
    end
    return string.format("%04d-%02d-%02d", y, m, d)
end

-- 分类清单，连同「这次有没有读全」（空串 = 每一行都读出来了）。
-- 第二个值不能省：下面 `reset_stale_filters` 拿这份清单判「用户筛的分类还在不在」，
-- 于是"库读不动 / 某一行读不出来"会被念成"这个分类被删了"，用户的筛选条件跟草稿
-- 就被一起清零。清单本身照样交出去（下拉里能显示几条显示几条），只是不许据残缺的它下结论。
local function cats()
    local list, why = focusflow.accounting_categories()
    return list or {}, why or ""
end

local function subs(cat)
    if cat == nil or cat == "" or cat == "全部" then return {} end
    return focusflow.accounting_subcategories(cat)
end

-- 查询当前页，返回 (records, total)。日期在这里再规范一次：认不出来的写法
-- 不当筛选条件用（面板上另有一行说明是哪一条没认出来）。
local function query_page()
    return focusflow.accounting_query(
        page, PAGE_SIZE,
        f_cat == "全部" and "" or f_cat,
        f_sub == "全部" and "" or f_sub,
        f_kw, norm_date(f_from) or "", norm_date(f_to) or ""
    )
end

local function clamp_page(total)
    local pages = math.max(1, math.ceil(total / PAGE_SIZE))
    if page > pages then page = pages end
    if page < 1 then page = 1 end
    return pages
end

local function fmt2(v)
    return string.format("%.2f", v or 0)
end

local function fmt_net(v)
    if v == nil then v = 0 end
    if v > 0 then return string.format("+%.2f", v) end
    return string.format("%.2f", v)
end

-- 分类/子分类被改名或删除之后，筛选条件里存的还是老名字。
--
-- 后果有两句、而且互相加重：
-- ① SQL 按老名字筛 → 列表"共 0 条"，用户以为**记录被删了**；
-- ② `<select>` 拿到一个不在 options 里的 value 时，浏览器会退回显示第一项
--    （也就是"全部"，见 `ui/js/plugins.js` 渲染 select 的地方）—— 于是下拉说"全部"、
--    列表却是空的，两边各说一套，比单纯显示 0 条更难看懂。
-- 弹窗草稿里的分类同样会悬空：留着它，下一次"保存"就会把记录写进一个**已经不存在的分类**。
local function reset_stale_filters()
    local all, why = cats()
    if why ~= "" then
        -- 清单没读全 ⇒ 一个字都不改。这里要清的是用户手选的筛选条件与弹窗草稿：
        -- 一次瞬时读不动就把它们清零，比留着那个"也许已经不存在的名字"更难解释，
        -- 而且下一次读全了还会照常校正。界面上另有一行说明这次没读全。
        return
    end
    local valid = {}
    local sub_of = {}
    for _, c in ipairs(all) do
        valid[c] = true
        local set = {}
        for _, x in ipairs(subs(c)) do
            set[x] = true
        end
        sub_of[c] = set
    end
    if f_cat ~= "全部" and not valid[f_cat] then
        f_cat = "全部"
        f_sub = "全部"
        page = 1
    elseif f_sub ~= "全部" and not (sub_of[f_cat] and sub_of[f_cat][f_sub]) then
        f_sub = "全部"
        page = 1
    end
    if profit_cat ~= "" and not valid[profit_cat] then
        profit_cat = ""
    end
    if draft.category ~= "" and not valid[draft.category] then
        draft.category = ""
        draft.subcategory = ""
    end
    if m_cat ~= "" and not valid[m_cat] then
        m_cat = ""
    end
end

-- 汇总类报告的「这份数能不能当结论」头一句。三个读数口（月度汇总、分类盈亏、细分盈亏）
-- 都从这里出声：一次读不动不许被印成一份格式完整的零报告 —— 那等于对用户说
-- 「这个月没花钱」「这些分类一条记录都没有」，而宿主根本没读到。
-- 交回 `true` = 这次没读全，调用方据此别再往下说「暂无数据」那两句结论。
local function gap_head(lines, gap)
    if gap == nil or gap == "" then
        return false
    end
    lines[#lines + 1] = "  这份数没读全：" .. tostring(gap)
    return true
end

function on_action(id)
    if id == PANEL_CLOSED then
        reset_transient_state()
    elseif id == "page_prev" then
        if page > 1 then page = page - 1 end
    elseif id == "page_next" then
        local _, total = query_page()
        local pages = math.max(1, math.ceil(total / PAGE_SIZE))
        if page < pages then page = page + 1 end
    elseif id == "do_query" then
        page = 1
    elseif id == "reset_query" then
        f_cat = "全部"; f_sub = "全部"; f_kw = ""; f_from = ""; f_to = ""
        page = 1
    elseif id == "add_record" then
        local amount = tonumber(draft.amount) or 0
        if draft.item == "" or amount <= 0 then
            list_msg = "没保存：请填写名称，金额要大于 0（小数写 12.50 这种，别写 1,234）"
            focusflow.log("请填写名称和有效金额")
            return
        end
        local backdated = false
        if draft.date == "" then
            draft.date = _today()
        else
            -- 「排在后面页」只在它比今天**早**时成立：列表是 purchase_date 降序，
            -- 比今天晚的日期反而排在第 1 页最前。原先写 `~=`，于是记一笔明天的账
            -- 也被当成补记 —— 提示让人去翻后面的页，而它就在眼前这页的第一行。
            -- 比较前先 norm_date 收成补零形态（与宿主写入时同一把尺子），否则
            -- `2026-9-1` 这种没补零的串按字节序会被判成「不早于今天」。
            local nd = norm_date(draft.date)
            backdated = nd ~= nil and nd < _today()
        end
        local new_id, why = focusflow.accounting_add(
            draft.type, draft.item, draft.store, draft.date, amount,
            draft.category, draft.subcategory, draft.note
        )
        if new_id > 0 then
            list_msg = "已保存 #" .. tostring(new_id) .. "：" .. draft.date .. " " .. draft.item
            focusflow.log("已添加记录 #" .. tostring(new_id))
            local saved_date = draft.date
            draft.item = ""; draft.store = ""; draft.amount = ""
            draft.category = ""; draft.subcategory = ""; draft.date = ""; draft.note = ""
            -- 列表是 `ORDER BY purchase_date DESC, id DESC`（accounting.rs 的
            -- get_expenses_page），**不是 id 降序**：补记一笔上个月的账时它压根不在
            -- 首页，而旧代码无条件跳第 1 页 —— 用户看到"保存过了却没这条"，
            -- 于是再点一次保存，造出重复记录。只有当天的记录才保证落在首页。
            if not backdated then
                page = 1
            else
                list_msg = list_msg .. "（记在 " .. saved_date .. "，按日期排在后面页，用日期筛选能找到）"
            end
        else
            -- 宿主的 `accounting_add` 现在把「哪条规则拒的」一起交回来（打不开库／
            -- 类型不在允许值／日期不可识别／写入本身失败是四件事）。原先四个败法在
            -- 这里都只显示同一句「库正被占用或打不开」，而当天最常见的一档其实是日期
            -- 档位：用户照着提示去关占用他的库，改完再点一次还是同一句。
            local reason = why ~= "" and why or "记账库写入失败（库正被占用或打不开）"
            list_msg = "没保存：" .. reason .. "，没有生成记录"
            focusflow.log("记账没保存：" .. reason)
        end
    elseif id == "save_edit" then
        if editing_id == nil then return end
        local amount = tonumber(draft.amount) or 0
        if draft.item == "" or amount <= 0 then
            -- 刻意不清 editing_id：弹窗带着原草稿重新开出来，改完就能再保存
            list_msg = "没保存：请填写名称，金额要大于 0（小数写 12.50 这种，别写 1,234）"
            focusflow.log("请填写名称和有效金额")
            return
        end
        local ok = focusflow.accounting_update(
            editing_id, draft.type, draft.item, draft.store,
            draft.date == "" and _today() or draft.date, amount,
            draft.category, draft.subcategory, draft.note
        )
        if ok then
            local updated = editing_id
            list_msg = "已更新记录 #" .. tostring(updated)
            focusflow.log("已更新记录 #" .. tostring(updated))
            editing_id = nil
            -- 清空草稿，避免下次"添加记录"弹窗预填旧数据
            draft.item = ""; draft.store = ""; draft.amount = ""
            draft.category = ""; draft.subcategory = ""; draft.date = ""; draft.note = ""
            -- 改日期（或改成当前筛选不再命中的分类/名称）会把这条移出**当前这一页**：
            -- 列表按 `purchase_date DESC, id DESC` 排。只回一句「已更新」而列表里没有它，
            -- 用户多半以为没存上、再改一遍 —— 与 add_record 那条同族（那条早修过：
            -- 当天记录跳回第 1 页，补记的记录说明排在后面页）。
            -- 判据是"再查一次当前页里还有没有它"，而不是在 Lua 里重抄一遍 SQL 的筛选条件。
            local still_here = false
            for _, r in ipairs(query_page()) do
                if tonumber(r["id"]) == updated then
                    still_here = true
                    break
                end
            end
            if not still_here then
                page = 1
                list_msg = list_msg
                    .. "（按当前筛选或日期排序，它已不在这一页，已回到第 1 页；找不到就用日期或关键词筛选）"
            end
        else
            list_msg = "没更新 #" .. tostring(editing_id) .. "：写入失败（库被占用或记录已被删除）"
        end
    elseif id:match("^edit_") then
        local rid = tonumber(id:sub(6))
        if not rid then return end
        local rec = focusflow.accounting_get(rid)
        if rec then
            draft.type = rec["type"]
            draft.item = rec["item"]
            draft.store = rec["store"]
            draft.amount = tostring(rec["amount"])
            draft.category = rec["category"]
            draft.subcategory = rec["subcategory"]
            draft.date = rec["date"]
            draft.note = rec["note"]
            editing_id = rid
        end
    elseif id:match("^del_") then
        -- 支持多选：id 形如 del_42,43
        local list = id:sub(5)
        local deleted, failed = 0, 0
        for rid in list:gmatch("([^,]+)") do
            local n = tonumber(rid)
            if not n then
                failed = failed + 1
            elseif focusflow.accounting_delete(n) then
                deleted = deleted + 1
            else
                failed = failed + 1
            end
        end
        if deleted > 0 then focusflow.log("已删除 " .. deleted .. " 条记录") end
        -- 每次删除都要写一句结论（本文件的规矩：add/save 成功失败都写）。
        -- `list_msg` 只在 `reset_transient_state` 里清，只写失败那一支的话，
        -- 下一次成功删除之后上一句「没删掉」还挂在页上。
        -- 宿主把「记录已不存在」与「库打不开」都回 false，分不开，所以两种一起点名。
        if failed == 0 then
            list_msg = "已删除 " .. deleted .. " 条记录"
        else
            local head = ""
            if deleted > 0 then head = "删掉了 " .. deleted .. " 条，" end
            list_msg = head .. "这 " .. failed ..
                " 条没删掉（记录可能已被别处删掉，或记账库正被占用），刷新后还在的就是没删掉的"
            focusflow.log("删除有失败：成功 " .. deleted .. " 条，失败 " .. failed .. " 条")
        end
        local _, total = query_page()
        clamp_page(total)
    elseif id:match("^days_") then
        -- 支持多选：id 形如 days_42,43
        local list = id:sub(6)
        local lines = {}
        local misses = {}
        local asked = 0
        for rid in list:gmatch("([^,]+)") do
            local n = tonumber(rid)
            if n then
                asked = asked + 1
                local rec = focusflow.accounting_get(n)
                -- 第二个值是「这一条为什么没答上」：原先宿主只交答案，没命中就一行都不拼，
                -- 而下面那句 `result_text ~= ""` 才开弹窗 ⇒ "没填日期"、"库被别处占着"、
                -- "这条已被删掉" 三种结局在用户眼里都是"这个按钮按了没反应"。
                local data, why = focusflow.accounting_days_ago({ n })
                if data and data[1] then
                    local d = data[1]
                    local name = (rec and rec["item"]) or ("#" .. tostring(n))
                    if d["years"] > 0 then
                        lines[#lines + 1] = "《" .. name .. "》 距今 " .. d["years"] .. " 年 " .. d["days"] .. " 天"
                    else
                        lines[#lines + 1] = "《" .. name .. "》 距今 " .. d["days"] .. " 天"
                    end
                end
                -- 宿主交回的原因一律贴出来：就算这一条答上了，它同时说的话也不能被丢掉
                for _, w in ipairs(why or {}) do
                    misses[#misses + 1] = w
                end
            else
                misses[#misses + 1] = "选中的编号「" .. tostring(rid) .. "」不是数字，没去查"
            end
        end
        if asked == 0 and #misses == 0 then
            misses[#misses + 1] = "没有选中任何记录：先在列表里点一行，再按「距今多久」"
        end
        if #misses > 0 then
            lines[#lines + 1] = ""
            lines[#lines + 1] = "这几条没算出来："
            for _, w in ipairs(misses) do
                lines[#lines + 1] = "  " .. w
            end
        end
        result_text = table.concat(lines, "\n")
    elseif id == "monthly_detail" then
        local ym = os.date("%Y-%m")
        local expense, income, count, stats, gap = focusflow.accounting_monthly_detail(ym)
        local lines = { "【" .. ym .. " 月度汇总】" }
        local partial = gap_head(lines, gap)
        lines[#lines + 1] = "  总支出：" .. fmt2(expense)
        lines[#lines + 1] = "  总收入：" .. fmt2(income)
        lines[#lines + 1] = "  净额：" .. fmt_net(income - expense)
        lines[#lines + 1] = "  记录数：" .. tostring(count)
        lines[#lines + 1] = ""
        if stats and #stats > 0 then
            lines[#lines + 1] = "分类明细（净额，收入为正/支出为负）："
            for _, s in ipairs(stats) do
                lines[#lines + 1] = "  " .. s["category"] .. ": " .. fmt_net(s["net"])
            end
        elseif partial then
            -- 没读全的那一次不许说「本月暂无」：库里可能正躺着这个月的明细
            lines[#lines + 1] = "分类明细这一次没列出来（见上面那句原因），这不算「本月没有明细」"
        else
            lines[#lines + 1] = "（本月暂无分类明细）"
        end
        result_text = table.concat(lines, "\n")
    elseif id == "cat_profit" then
        local data, gap = focusflow.accounting_category_profit()
        data = data or {}
        local lines = { "【分类盈亏统计】" }
        local partial = gap_head(lines, gap)
        if #data == 0 and partial then
            -- 一条都没读到 ⇒ 既不给「总投入 0.00」也不写「暂无数据」：那两句都是结论，
            -- 而这一次宿主压根没读到（库被占住 / 文件读不动）。
            lines[#lines + 1] = "  这一次一条都没读到：这里不给合计，也不说「暂无数据」"
        else
            local total_inv, total_earn = 0, 0
            for _, d in ipairs(data) do
                total_inv = total_inv + d["invested"]
                total_earn = total_earn + d["earned"]
            end
            lines[#lines + 1] = ""
            lines[#lines + 1] = "  总投入：" .. fmt2(total_inv) .. "  总赚取：" .. fmt2(total_earn) .. "  净额：" .. fmt_net(total_earn - total_inv)
            lines[#lines + 1] = ""
            if #data > 0 then
                for _, d in ipairs(data) do
                    local net = d["earned"] - d["invested"]
                    lines[#lines + 1] = string.format(
                        "  %-12s 投入 %10s  赚取 %10s  净额 %10s  记录 %d",
                        d["category"], fmt2(d["invested"]), fmt2(d["earned"]), fmt_net(net), d["count"]
                    )
                end
            else
                lines[#lines + 1] = "（暂无数据）"
            end
        end
        result_text = table.concat(lines, "\n")
    elseif id == "cancel_edit" then
        -- 取消编辑：清空编辑状态（弹窗由前端关闭）
        editing_id = nil
    elseif id == "cancel_profit" then
        -- 取消分类选择
        picker_open = false
    elseif id == "open_profit_picker" then
        picker_open = true
    elseif id == "subcat_profit" then
        picker_open = false
        local cat = profit_cat
        if cat == "" then
            result_text = "请先在弹窗中选择分类"
            return
        end
        local data, gap = focusflow.accounting_subcategory_profit(cat)
        data = data or {}
        local lines = { "【" .. cat .. " - 细分盈亏】" }
        local partial = gap_head(lines, gap)
        if #data == 0 and partial then
            lines[#lines + 1] = "  这一次一条都没读到：这里不给合计，也不说「暂无数据」"
        else
            local total_inv, total_earn = 0, 0
            for _, d in ipairs(data) do
                total_inv = total_inv + d["invested"]
                total_earn = total_earn + d["earned"]
            end
            lines[#lines + 1] = ""
            lines[#lines + 1] = "  总投入：" .. fmt2(total_inv) .. "  总赚取：" .. fmt2(total_earn) .. "  净额：" .. fmt_net(total_earn - total_inv)
            lines[#lines + 1] = ""
            if #data > 0 then
                for _, d in ipairs(data) do
                    local net = d["earned"] - d["invested"]
                    lines[#lines + 1] = string.format(
                        "  %-14s 投入 %10s  赚取 %10s  净额 %10s  记录 %d",
                        d["subcategory"], fmt2(d["invested"]), fmt2(d["earned"]), fmt_net(net), d["count"]
                    )
                end
            else
                lines[#lines + 1] = "（暂无数据）"
            end
        end
        result_text = table.concat(lines, "\n")
    elseif id == "open_manage" then
        manage_open = true
        local all = cats()
        if m_cat == "" and #all > 0 then m_cat = all[1] end
    elseif id == "close_manage" then
        manage_open = false
    elseif id == "m_add_cat" then
        -- 宿主那边现在也会拒（add_category 已 trim + 拒空），但 `accounting_category_add`
        -- 只回一个 id、没有原因串，所以下面那句 `msg` 恒为 nil —— 界面只会显示
        -- "添加分类失败：" 四个字。空名/纯空格在这里就拦下，才能说清是哪一种。
        m_name = (m_name or ""):match("^%s*(.-)%s*$")
        if m_name == "" then
            result_text = "分类名不能是空的（也别只打空格）"
            return
        end
        local ok, msg = focusflow.accounting_category_add(m_name, m_type)
        result_text = (ok and ok > 0 and "分类 [" .. m_name .. "] 已添加") or ("添加分类失败：" .. tostring(msg or ""))
        if ok and ok > 0 then m_cat = m_name; m_name = "" end
    -- 前端契约：sel 按钮的动作 id = 按钮 id 直接拼接选中项（无分隔符），
    -- 例如 "m_edit_cat_sel" .. "食品饮料"。因此模式不能带尾部下划线，
    -- 且 sub(N) 的起点就是按钮 id 长度的 +1。
    elseif id:match("^m_edit_cat_sel") then
        -- 顶部"修改分类"（基于选中的分类行）
        local name = id:sub(15)
        if name == "" then return end
        m_edit_old = name
        m_name = name
        local t, why = focusflow.accounting_category_type(name)
        -- 第二个值是"这一轮到底答没答上"（宿主返回 (类型, 原因)）。旧写法只取第一个值：
        -- 读不出来时它是空串，`if t ~= ""` 不成立 ⇒ 下拉留着**上一个分类**的类型显示，
        -- 而用户按保存就把那个旧值写进了这个分类 —— 一句错都没报。
        m_type_unknown = why ~= nil and why ~= ""
        if m_type_unknown then
            m_type = "both"
            result_text = "分类 [" .. name
                .. "] 的类型这一轮读不出来：" .. why
                .. "；下拉里是默认值，保存只改名、不动类型"
        elseif t ~= "" then
            m_type = t
        end
        m_edit_open = true
    elseif id:match("^m_del_cat_sel") then
        local name = id:sub(14)
        if name == "" then return end
        local ok, msg = focusflow.accounting_category_delete(name)
        result_text = (ok and "分类 [" .. name .. "] 已删除") or ("删除失败：" .. tostring(msg))
        if ok then
            m_cat = ""
            local all = cats()
            if #all > 0 then m_cat = all[1] end
        end
        reset_stale_filters()
    elseif id:match("^m_edit_sub_sel") then
        local name = id:sub(15)
        if name == "" or m_cat == "" then return end
        m_sub_edit_old = name
        m_sub_name = name
        m_sub_edit_open = true
    elseif id:match("^m_del_sub_sel") then
        local name = id:sub(14)
        if name == "" or m_cat == "" then
            result_text = "请先在左侧分类列表点击选中一个分类"
        else
            local ok, msg = focusflow.accounting_subcategory_delete(m_cat, name)
            result_text = (ok and "子分类 [" .. name .. "] 已删除") or ("删除失败：" .. tostring(msg))
            if ok then m_sub_name = "" end
            reset_stale_filters()
        end
    elseif id == "m_save_edit_cat" then
        if m_edit_old == nil or m_edit_old == "" then return end
        -- 与「添加分类」那条同一个前置：先 trim、空名就地拦下。宿主 rename 也是 trim
        -- 之后才落库，而这里原先把 `m_cat` 设成**没 trim** 的那个串 —— 选中的分类于是
        -- 指向一条不存在的记录：右侧子分类静默变空，接着「添加子分类」报「分类不存在」。
        m_name = (m_name or ""):match("^%s*(.-)%s*$")
        if m_name == "" then
            result_text = "分类名不能是空的（也别只打空格）"
            return
        end
        -- 类型这一轮没读出来 ⇒ 第三个参数传空串，宿主那侧对空串的解释是"保持原类型"，
        -- 于是改名照常做，而那个我们并不知道的旧值不会被写进这个分类。
        local ctype = m_type_unknown and "" or m_type
        local ok, msg = focusflow.accounting_category_rename(m_edit_old, m_name, ctype)
        result_text = (ok and ("分类已更新为 [" .. m_name .. "]"
            .. (m_type_unknown and "（类型没动：这一轮没读出来）" or "")))
            or ("更新失败：" .. tostring(msg))
        m_edit_open = false; m_edit_old = ""; m_type_unknown = false
        if ok then m_cat = m_name; m_name = "" end
        reset_stale_filters()
    elseif id == "m_cancel_edit_cat" then
        m_edit_open = false; m_edit_old = ""; m_type_unknown = false
    elseif id == "m_add_sub" then
        if m_cat == "" then
            result_text = "请先在左侧分类列表点击选中一个分类"
        else
            local ok, msg = focusflow.accounting_subcategory_add(m_cat, m_sub_name)
            result_text = (ok and "子分类 [" .. m_sub_name .. "] 已添加到 [" .. m_cat .. "]") or ("添加失败：" .. tostring(msg))
            if ok then m_sub_name = "" end
        end
    elseif id == "m_save_edit_sub" then
        if m_cat == "" or m_sub_edit_old == nil or m_sub_edit_old == "" then return end
        local ok, msg = focusflow.accounting_subcategory_rename(m_cat, m_sub_edit_old, m_sub_name)
        result_text = (ok and "子分类已更新为 [" .. m_sub_name .. "]") or ("更新失败：" .. tostring(msg))
        m_sub_edit_open = false; m_sub_edit_old = ""
        if ok then m_sub_name = "" end
        reset_stale_filters()
    elseif id == "m_cancel_edit_sub" then
        m_sub_edit_open = false; m_sub_edit_old = ""
    elseif id == "clear_result" then
        result_text = ""
    end
end

-- 文本输入更新（宿主调用）
function set_field(field, value)
    if field == "d_type" then draft.type = value
    elseif field == "d_item" then draft.item = value
    elseif field == "d_store" then draft.store = value
    elseif field == "d_amount" then draft.amount = value
    elseif field == "d_category" then draft.category = value; draft.subcategory = ""
    elseif field == "d_subcategory" then draft.subcategory = value
    elseif field == "d_date" then draft.date = value
    elseif field == "d_note" then draft.note = value
    elseif field == "f_cat" then f_cat = value; f_sub = "全部"; page = 1
    elseif field == "f_sub" then f_sub = value; page = 1
    elseif field == "f_kw" then f_kw = value; page = 1
    elseif field == "f_from" or field == "f_to" then
        -- 认得出来就收成补零形态（与弹窗的 `kind="date"` 同一套口径）；
        -- 认不出来留着原样给用户改，视图上另有一行说明它没生效。
        local n = norm_date(value)
        if field == "f_from" then f_from = n or value
        else f_to = n or value end
        page = 1
    elseif field == "profit_cat" then profit_cat = value
    elseif field == "m_cat" then m_cat = value
    elseif field == "m_name" then m_name = value
    elseif field == "m_type" then
        m_type = value
        -- 用户在这个下拉里挑过一次，这一格就不再是"我们不知道的值"了：
        -- 保存要按他挑的写（不设这条，读不出来那一轮就永远改不了类型）
        m_type_unknown = false
    elseif field == "m_sub_name" then m_sub_name = value
    end
end

-- 构建下拉选项表：{ {value=.., label=..}, ... }（前面加一个"全部/无"项）
local function cat_opts_with(extra_label, extra_value)
    local opts = { { value = extra_value, label = extra_label } }
    for _, c in ipairs(cats()) do
        opts[#opts + 1] = { value = c, label = c }
    end
    return opts
end

local function sub_opts_with(extra_label, extra_value, cat)
    local opts = { { value = extra_value, label = extra_label } }
    for _, s in ipairs(subs(cat)) do
        opts[#opts + 1] = { value = s, label = s }
    end
    return opts
end

function get_view()
    -- 先按当前页查、再判断这一页还在不在：以前是"用**没夹过**的 page 查一次、
    -- 然后才 clamp_page"，所以外部改库/双开把记录删少之后，会有一帧
    -- "表格空着 + 第 3 / 3 页"，要等下一次 2 秒推送才自愈。
    -- 夹过之后重查一次（只在真的越界时发生，正常翻页一次查询就够）。
    local asked = page
    local records, total = query_page()
    local pages = clamp_page(total)
    if page ~= asked then
        records, total = query_page()
        pages = clamp_page(total)
    end

    local rows = {}
    local ids = {}
    for _, r in ipairs(records or {}) do
        rows[#rows + 1] = {
            r["date"], r["type"], r["item"], r["store"],
            string.format("%.2f", r["amount"]),
            r["category"], r["subcategory"], r["note"],
        }
        ids[#ids + 1] = tostring(r["id"])
    end

    -- 新增/编辑弹窗字段（共用草稿）
    local form_fields = {
        { kind = "select", field = "d_type", label = "类型", value = draft.type,
          options = { { value = "支出", label = "支出" }, { value = "收入", label = "收入" } } },
        { kind = "text", field = "d_item", label = "名称", value = draft.item },
        { kind = "text", field = "d_store", label = "渠道", value = draft.store },
        { kind = "text", field = "d_amount", label = "金额", value = draft.amount },
        { kind = "select", field = "d_category", label = "分类", value = draft.category,
          refresh = true, options = cat_opts_with("（无）", "") },
        { kind = "select", field = "d_subcategory", label = "子分类", value = draft.subcategory,
          options = sub_opts_with("（无）", "", draft.category) },
        { kind = "date", field = "d_date", label = "日期", value = draft.date },
        { kind = "text", field = "d_note", label = "备注", value = draft.note },
    }

    local widgets = {}
    local function add(w) widgets[#widgets + 1] = w end

    -- 操作栏：记录操作（基于选中行，可多选）+ 统计分析
    add({
        type = "row",
        children = {
            { type = "button", id = "open_add", text = "＋ 添加记录", modal = "add_modal" },
            { type = "button", id = "edit_", text = "修改", sel = true },
            { type = "button", id = "del_", text = "删除", sel = true },
            { type = "button", id = "days_", text = "距今多久", sel = true },
            { type = "button", id = "monthly_detail", text = "月度汇总" },
            { type = "button", id = "cat_profit", text = "分类盈亏" },
            { type = "button", id = "open_profit_picker", text = "细分盈亏" },
            { type = "button", id = "open_manage", text = "分类管理" },
        },
    })

    -- 新增记录弹窗
    add({
        type = "modal_form",
        id = "add_modal",
        title = "新增记录",
        submit = "add_record",
        submit_text = "保存",
        fields = form_fields,
    })

    -- 编辑记录弹窗（编辑动作后自动打开；取消会重置编辑状态）
    add({
        type = "modal_form",
        id = "edit_modal",
        title = "修改记录",
        submit = "save_edit",
        submit_text = "保存",
        cancel = "cancel_edit",
        open = editing_id ~= nil,
        fields = form_fields,
    })

    -- 细分盈亏：分类选择弹窗（取消会关闭选择状态）
    add({
        type = "modal_form",
        id = "profit_modal",
        title = "选择分类（细分盈亏）",
        submit = "subcat_profit",
        submit_text = "确定",
        cancel = "cancel_profit",
        open = picker_open,
        fields = {
            {
                kind = "select", field = "profit_cat", label = "分类",
                value = profit_cat,
                options = cat_opts_with("（请选择）", ""),
            },
        },
    })

    add({ type = "separator" })

    -- 筛选栏（两行：分类/子分类 + 关键词/日期/查询/重置）
    local _, cat_gap = cats()
    add({
        type = "row",
        children = {
            { type = "select", field = "f_cat", label = "分类", value = f_cat, refresh = true,
              options = cat_opts_with("全部", "全部") },
            { type = "select", field = "f_sub", label = "子分类", value = f_sub, refresh = true,
              options = sub_opts_with("全部", "全部", f_cat) },
        },
    })
    -- 下拉里没有的名字不等于分类被删了：这一头没读全时必须说出来，否则用户看到
    -- 一个变短的下拉，以为自己的分类没了（本文件另一处 `reset_stale_filters` 也照这条走）。
    if cat_gap ~= "" then
        add({ type = "label", text = "分类清单这次没读全：" .. cat_gap
            .. "。下拉里缺的名字不代表已被删除，筛选条件也按原样保留" })
    end
    add({
        type = "row",
        children = {
            { type = "textinput", field = "f_kw", label = "关键词", value = f_kw },
            { type = "textinput", field = "f_from", label = "从", value = f_from },
            { type = "textinput", field = "f_to", label = "到", value = f_to },
            { type = "button", id = "do_query", text = "查询" },
            { type = "button", id = "reset_query", text = "重置" },
        },
    })
    -- 日期是自由文本框：认不出来的写法必须点名说"这一条没生效"，
    -- 否则用户以为筛过了，看到的其实是全量（宿主那一头只会静默不筛）。
    for _, pair in ipairs({ { "从", f_from }, { "到", f_to } }) do
        if pair[2] ~= "" and not norm_date(pair[2]) then
            add({ type = "label", text = "日期筛选「" .. pair[1] .. "」没生效："
                .. "「" .. pair[2] .. "」不是能认的日期，写成 2026-09-01（年-月-日，个位数不用补零）或留空" })
        end
    end

    add({ type = "separator" })

    -- 记录列表（点击行选中，配合顶部 修改/删除/距今多久 按钮）
    add({ type = "heading", text = "收支记录（点击行选中，按日期倒序）" })
    if list_msg ~= "" then
        add({ type = "label", text = list_msg })
    end
    add({
        type = "table",
        headers = { "日期", "类型", "名称", "渠道", "金额", "分类", "子分类", "备注" },
        rows = rows,
        ids = ids,
    })
    add({
        type = "pager",
        page = page,
        pages = pages,
        total = total,
        prev = "page_prev",
        next = "page_next",
    })

    -- 统计结果弹窗（月度汇总/盈亏/距今多久）
    add({
        type = "modal_form",
        id = "result_modal",
        title = "统计结果",
        content = result_text,
        submit = "clear_result",
        submit_text = "关闭",
        cancel = "clear_result",
        open = result_text ~= "",
    })

    -- ===== 分类管理子页面（对标主流记账 App：左分类列表 + 右子分类列表）=====
    if manage_open then
        local all_cats = cats()
        local all_subs = subs(m_cat)

        -- 分类表格（点击行选中，单选高亮）
        local cat_rows = {}
        local cat_ids = {}
        for _, c in ipairs(all_cats) do
            local t, why = focusflow.accounting_category_type(c)
            local tlabel
            if why ~= nil and why ~= "" then
                -- 读不出来时不许印成「双向」：那一格与"库里真没设类型"长得一模一样，
                -- 而用户会照着这个假读数去判断这个分类的钱算收入还是支出
                tlabel = "读不出来"
            else
                tlabel = t == "income" and "收入" or (t == "expense" and "支出" or "双向")
            end
            cat_rows[#cat_rows + 1] = { c, tlabel }
            cat_ids[#cat_ids + 1] = c
        end
        -- 子分类表格（属于选中分类）
        local sub_rows = {}
        local sub_ids = {}
        for _, s in ipairs(all_subs) do
            sub_rows[#sub_rows + 1] = { s }
            sub_ids[#sub_ids + 1] = s
        end

        -- 返回栏
        add({ type = "heading", text = "分类管理（点击左侧分类，右侧显示其子分类）" })
        add({
            type = "row",
            children = {
                { type = "button", id = "close_manage", text = "← 返回记账" },
                { type = "button", id = "m_add_cat", text = "＋ 添加分类", modal = "m_cat_modal" },
                { type = "button", id = "m_edit_cat_sel", text = "修改分类", sel = true, group = "mcat" },
                { type = "button", id = "m_del_cat_sel", text = "删除分类", sel = true, group = "mcat" },
                { type = "button", id = "m_add_sub", text = "＋ 添加子分类", modal = "m_sub_modal" },
                { type = "button", id = "m_edit_sub_sel", text = "修改子分类", sel = true, group = "msub" },
                { type = "button", id = "m_del_sub_sel", text = "删除子分类", sel = true, group = "msub" },
            },
        })

        add({
            type = "row",
            children = {
                { type = "heading", text = "分类列表" },
                { type = "heading", text = "子分类列表" },
            },
        })
        -- 双列表用两个表格并排（放在同一 row 中无法并排，用键值行+表格代替）
        add({
            type = "table",
            headers = { "分类", "类型" },
            rows = cat_rows,
            ids = cat_ids,
            group = "mcat",
            onselect = "m_cat",
        })
        add({
            type = "table",
            headers = { "子分类" },
            rows = sub_rows,
            ids = sub_ids,
            group = "msub",
        })
        if #all_subs == 0 and m_cat ~= "" then
            add({ type = "label", text = "（分类 [" .. m_cat .. "] 暂无子分类，点上方 ＋ 添加子分类）" })
        end

        -- 添加分类弹窗
        add({
            type = "modal_form",
            id = "m_cat_modal",
            title = "添加分类",
            submit = "m_add_cat",
            submit_text = "添加",
            fields = {
                { kind = "text", field = "m_name", label = "分类名", value = m_name },
                { kind = "select", field = "m_type", label = "类型", value = m_type,
                  options = {
                      { value = "expense", label = "支出" },
                      { value = "income", label = "收入" },
                      { value = "both", label = "双向" },
                  } },
            },
        })

        -- 添加子分类弹窗
        add({
            type = "modal_form",
            id = "m_sub_modal",
            title = "添加子分类",
            submit = "m_add_sub",
            submit_text = "添加",
            fields = {
                { kind = "text", field = "m_sub_name", label = "子分类名", value = m_sub_name },
            },
        })

        -- 分类编辑小弹窗（选中后点"修改分类"）
        if m_edit_open then
            add({
                type = "modal_form",
                id = "m_edit_cat_modal",
                title = "修改分类",
                submit = "m_save_edit_cat",
                submit_text = "保存",
                cancel = "m_cancel_edit_cat",
                open = true,
                fields = {
                    { kind = "text", field = "m_name", label = "分类名", value = m_name },
                    { kind = "select", field = "m_type", label = "类型", value = m_type,
                      options = {
                          { value = "expense", label = "支出" },
                          { value = "income", label = "收入" },
                          { value = "both", label = "双向" },
                      } },
                },
            })
        end

        -- 子分类编辑小弹窗（选中后点"修改子分类"）
        if m_sub_edit_open then
            add({
                type = "modal_form",
                id = "m_edit_sub_modal",
                title = "修改子分类",
                submit = "m_save_edit_sub",
                submit_text = "保存",
                cancel = "m_cancel_edit_sub",
                open = true,
                fields = {
                    { kind = "text", field = "m_sub_name", label = "子分类名", value = m_sub_name },
                },
            })
        end

        return { title = "记账本 - 分类管理", widgets = widgets }
    end

    return { title = "记账本", widgets = widgets }
end
