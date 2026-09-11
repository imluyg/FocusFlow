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
local picker_open = false
local profit_cat = ""

-- 分类管理面板状态
local manage_open = false
local m_cat = ""
local m_name = "" -- 添加/重命名分类名
local m_type = "both"
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

function _today()
    return os.date("%Y-%m-%d")
end

local function cats()
    return focusflow.accounting_categories()
end

local function subs(cat)
    if cat == nil or cat == "" or cat == "全部" then return {} end
    return focusflow.accounting_subcategories(cat)
end

-- 查询当前页，返回 (records, total)
local function query_page()
    return focusflow.accounting_query(
        page, PAGE_SIZE,
        f_cat == "全部" and "" or f_cat,
        f_sub == "全部" and "" or f_sub,
        f_kw, f_from, f_to
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

function on_action(id)
    if id == "page_prev" then
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
            focusflow.log("请填写名称和有效金额")
            return
        end
        if draft.date == "" then draft.date = _today() end
        local new_id = focusflow.accounting_add(
            draft.type, draft.item, draft.store, draft.date, amount,
            draft.category, draft.subcategory, draft.note
        )
        if new_id > 0 then
            focusflow.log("已添加记录 #" .. tostring(new_id))
            draft.item = ""; draft.store = ""; draft.amount = ""
            draft.category = ""; draft.subcategory = ""; draft.date = ""; draft.note = ""
            page = 1 -- 新记录在首页（id 降序）
        end
    elseif id == "save_edit" then
        if editing_id == nil then return end
        local amount = tonumber(draft.amount) or 0
        if draft.item == "" or amount <= 0 then
            focusflow.log("请填写名称和有效金额")
            return
        end
        local ok = focusflow.accounting_update(
            editing_id, draft.type, draft.item, draft.store,
            draft.date == "" and _today() or draft.date, amount,
            draft.category, draft.subcategory, draft.note
        )
        if ok then
            focusflow.log("已更新记录 #" .. tostring(editing_id))
            editing_id = nil
            -- 清空草稿，避免下次"添加记录"弹窗预填旧数据
            draft.item = ""; draft.store = ""; draft.amount = ""
            draft.category = ""; draft.subcategory = ""; draft.date = ""; draft.note = ""
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
        local deleted = 0
        for rid in list:gmatch("([^,]+)") do
            local n = tonumber(rid)
            if n and focusflow.accounting_delete(n) then deleted = deleted + 1 end
        end
        if deleted > 0 then focusflow.log("已删除 " .. deleted .. " 条记录") end
        local _, total = query_page()
        clamp_page(total)
    elseif id:match("^days_") then
        -- 支持多选：id 形如 days_42,43
        local list = id:sub(6)
        local lines = {}
        for rid in list:gmatch("([^,]+)") do
            local n = tonumber(rid)
            if n then
                local rec = focusflow.accounting_get(n)
                local data = focusflow.accounting_days_ago({ n })
                if data and data[1] then
                    local d = data[1]
                    local name = (rec and rec["item"]) or ("#" .. tostring(n))
                    if d["years"] > 0 then
                        lines[#lines + 1] = "《" .. name .. "》 距今 " .. d["years"] .. " 年 " .. d["days"] .. " 天"
                    else
                        lines[#lines + 1] = "《" .. name .. "》 距今 " .. d["days"] .. " 天"
                    end
                end
            end
        end
        result_text = table.concat(lines, "\n")
    elseif id == "monthly_detail" then
        local ym = os.date("%Y-%m")
        local expense, income, count, stats = focusflow.accounting_monthly_detail(ym)
        local lines = {
            "【" .. ym .. " 月度汇总】",
            "  总支出：" .. fmt2(expense),
            "  总收入：" .. fmt2(income),
            "  净额：" .. fmt_net(income - expense),
            "  记录数：" .. tostring(count),
            "",
        }
        if stats and #stats > 0 then
            lines[#lines + 1] = "分类明细（净额，收入为正/支出为负）："
            for _, s in ipairs(stats) do
                lines[#lines + 1] = "  " .. s["category"] .. ": " .. fmt_net(s["net"])
            end
        else
            lines[#lines + 1] = "（本月暂无分类明细）"
        end
        result_text = table.concat(lines, "\n")
    elseif id == "cat_profit" then
        local data = focusflow.accounting_category_profit()
        local lines = { "【分类盈亏统计】", "" }
        local total_inv, total_earn = 0, 0
        for _, d in ipairs(data or {}) do
            total_inv = total_inv + d["invested"]
            total_earn = total_earn + d["earned"]
        end
        lines[#lines + 1] = "  总投入：" .. fmt2(total_inv) .. "  总赚取：" .. fmt2(total_earn) .. "  净额：" .. fmt_net(total_earn - total_inv)
        lines[#lines + 1] = ""
        if data and #data > 0 then
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
        local data = focusflow.accounting_subcategory_profit(cat)
        local lines = { "【" .. cat .. " - 细分盈亏】", "" }
        local total_inv, total_earn = 0, 0
        for _, d in ipairs(data or {}) do
            total_inv = total_inv + d["invested"]
            total_earn = total_earn + d["earned"]
        end
        lines[#lines + 1] = "  总投入：" .. fmt2(total_inv) .. "  总赚取：" .. fmt2(total_earn) .. "  净额：" .. fmt_net(total_earn - total_inv)
        lines[#lines + 1] = ""
        if data and #data > 0 then
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
        result_text = table.concat(lines, "\n")
    elseif id == "open_manage" then
        manage_open = true
        local all = cats()
        if m_cat == "" and #all > 0 then m_cat = all[1] end
    elseif id == "close_manage" then
        manage_open = false
    elseif id == "m_add_cat" then
        local ok, msg = focusflow.accounting_category_add(m_name, m_type)
        result_text = (ok and ok > 0 and "分类 [" .. m_name .. "] 已添加") or ("添加分类失败：" .. tostring(msg or ""))
        if ok and ok > 0 then m_cat = m_name; m_name = "" end
    elseif id:match("^m_edit_cat_sel_") then
        -- 顶部"修改分类"（基于选中的分类行）
        local name = id:sub(15)
        if name == "" then return end
        m_edit_old = name
        m_name = name
        local t = focusflow.accounting_category_type(name)
        if t ~= "" then m_type = t end
        m_edit_open = true
    elseif id:match("^m_del_cat_sel_") then
        local name = id:sub(14)
        if name == "" then return end
        local ok, msg = focusflow.accounting_category_delete(name)
        result_text = (ok and "分类 [" .. name .. "] 已删除") or ("删除失败：" .. tostring(msg))
        if ok then
            m_cat = ""
            local all = cats()
            if #all > 0 then m_cat = all[1] end
        end
    elseif id:match("^m_edit_sub_sel_") then
        local name = id:sub(15)
        if name == "" or m_cat == "" then return end
        m_sub_edit_old = name
        m_sub_name = name
        m_sub_edit_open = true
    elseif id:match("^m_del_sub_sel_") then
        local name = id:sub(14)
        if name == "" or m_cat == "" then
            result_text = "请先在左侧分类列表点击选中一个分类"
        else
            local ok, msg = focusflow.accounting_subcategory_delete(m_cat, name)
            result_text = (ok and "子分类 [" .. name .. "] 已删除") or ("删除失败：" .. tostring(msg))
            if ok then m_sub_name = "" end
        end
    elseif id == "m_save_edit_cat" then
        if m_edit_old == nil or m_edit_old == "" then return end
        local ok, msg = focusflow.accounting_category_rename(m_edit_old, m_name, m_type)
        result_text = (ok and "分类已更新为 [" .. m_name .. "]") or ("更新失败：" .. tostring(msg))
        m_edit_open = false; m_edit_old = ""
        if ok then m_cat = m_name; m_name = "" end
    elseif id == "m_cancel_edit_cat" then
        m_edit_open = false; m_edit_old = ""
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
    elseif field == "f_from" then f_from = value; page = 1
    elseif field == "f_to" then f_to = value; page = 1
    elseif field == "profit_cat" then profit_cat = value
    elseif field == "m_cat" then m_cat = value
    elseif field == "m_name" then m_name = value
    elseif field == "m_type" then m_type = value
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
    local records, total = query_page()
    local pages = clamp_page(total)

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
    add({
        type = "row",
        children = {
            { type = "select", field = "f_cat", label = "分类", value = f_cat, refresh = true,
              options = cat_opts_with("全部", "全部") },
            { type = "select", field = "f_sub", label = "子分类", value = f_sub, refresh = true,
              options = sub_opts_with("全部", "全部", f_cat) },
        },
    })
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

    add({ type = "separator" })

    -- 记录列表（点击行选中，配合顶部 修改/删除/距今多久 按钮）
    add({ type = "heading", text = "收支记录（点击行选中，按日期倒序）" })
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
            local t = focusflow.accounting_category_type(c)
            local tlabel = t == "income" and "收入" or (t == "expense" and "支出" or "双向")
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
