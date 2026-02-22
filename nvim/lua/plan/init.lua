local M = {}

M.config = {
  bin = 'plan',
  open_cmd = 'edit', -- 'edit', 'split', 'vsplit', 'tabedit'
}

local ns = vim.api.nvim_create_namespace('plan')
local buf_zone_map = {} -- buf -> { [line_number] = true } for lines inside language zones

local hl_rules = {
  { pattern = '^%d+, %a+ %d+ %- %a+$', group = 'Title' },         -- header
  { pattern = '^~+inbox~+$',            group = 'Special' },       -- inbox marker
  { pattern = '^~+$',                    group = 'Special' },       -- tilde line
  { pattern = '^%* ',                   group = 'Identifier', sigil = 2 },
  { pattern = '^\\ ',                   group = 'DiagnosticWarn', sigil = 2 },
  { pattern = '^%+ ',                   group = 'DiagnosticOk', sigil = 2 },
  { pattern = '^%- ',                   group = 'DiagnosticError', sigil = 2 },
}

local lang_aliases = {
  md = 'markdown',
  ts = 'typescript',
  js = 'javascript',
  py = 'python',
  rs = 'rust',
  sh = 'bash',
  yml = 'yaml',
}

local function resolve_lang(name)
  return lang_aliases[name]
    or vim.treesitter.language.get_lang(name)
    or name
end

--- Scan lines for `--- :lang` / `---` zone boundaries.
--- Returns in_zone (set of 1-indexed line numbers inside a language zone)
--- and zones (list of { lang, start, stop } with 1-indexed content lines).
local function detect_zones(lines)
  local in_zone = {}
  local zones = {}
  local i = 1
  while i <= #lines do
    local lang = lines[i]:match('^%-%-%-%s*:(%w+)')
    if lang then
      local zone_start = i + 1
      local j = i + 1
      while j <= #lines and not lines[j]:match('^%-%-%-') do
        in_zone[j] = true
        j = j + 1
      end
      if zone_start <= j - 1 then
        zones[#zones + 1] = { lang = resolve_lang(lang), start = zone_start, stop = j - 1 }
      end
      i = j -- advance to closing --- (will be processed next iteration as bare ---)
    else
      i = i + 1
    end
  end
  return in_zone, zones
end

local function highlight_buf(buf)
  vim.api.nvim_buf_clear_namespace(buf, ns, 0, -1)
  local lines = vim.api.nvim_buf_get_lines(buf, 0, -1, false)
  local in_zone, zones = detect_zones(lines)

  -- Cache zone map for keybinding guards
  buf_zone_map[buf] = in_zone

  -- Highlight --- boundary lines (with or without :lang)
  for i, line in ipairs(lines) do
    if line:match('^%-%-%-') then
      vim.api.nvim_buf_set_extmark(buf, ns, i - 1, 0, {
        end_col = #line,
        hl_group = 'Comment',
      })
    end
  end

  -- Tree-sitter highlighting for language zones
  for _, zone in ipairs(zones) do
    local code_lines = {}
    for k = zone.start, zone.stop do
      code_lines[#code_lines + 1] = lines[k]
    end
    local code = table.concat(code_lines, '\n')
    local ok, parser = pcall(vim.treesitter.get_string_parser, code, zone.lang)
    if ok then
      parser:parse(true)
      -- Apply highlights from each parser in the tree (root + injected children).
      -- Child parsers (e.g. lua inside markdown fenced blocks) get higher priority
      -- so their token highlights override the parent's block-level styling.
      parser:for_each_tree(function(tree, ltree)
        local lang_name = ltree:lang()
        local is_root = (lang_name == zone.lang)
        local priority = is_root and 100 or 125
        local query = vim.treesitter.query.get(lang_name, 'highlights')
        if query then
          for id, node in query:iter_captures(tree:root(), code) do
            local sr, sc, er, ec = node:range()
            vim.api.nvim_buf_set_extmark(buf, ns, zone.start - 1 + sr, sc, {
              end_row = zone.start - 1 + er,
              end_col = ec,
              hl_group = '@' .. query.captures[id] .. '.' .. lang_name,
              priority = priority,
            })
          end
        end
      end)
    end
  end

  -- Plan-specific highlighting (only outside language zones)
  for i, line in ipairs(lines) do
    if not in_zone[i] and not line:match('^%-%-%-') then
      for _, rule in ipairs(hl_rules) do
        if line:match(rule.pattern) then
          vim.api.nvim_buf_set_extmark(buf, ns, i - 1, 0, {
            end_col = rule.sigil or #line,
            hl_group = rule.group,
          })
          break
        end
      end
      -- Italic dim timestamp suffix on terminal state lines
      local ts_start = line:match('^[+%-] .* ()%(%d%d%d%d%-%d%d%-%d%d%)$')
      if ts_start then
        vim.api.nvim_buf_set_extmark(buf, ns, i - 1, ts_start - 1, {
          end_col = #line,
          hl_group = 'planTimestamp',
        })
      end
    end
  end
end

function M.setup(opts)
  M.config = vim.tbl_deep_extend('force', M.config, opts or {})

  vim.filetype.add({ extension = { plan = 'plan' } })
  vim.api.nvim_set_hl(0, 'planTimestamp', { link = 'Comment', italic = true, default = true })

  vim.api.nvim_create_autocmd('FileType', {
    pattern = 'plan',
    callback = function(ev)
      highlight_buf(ev.buf)

      vim.api.nvim_create_autocmd({ 'TextChanged', 'TextChangedI' }, {
        buffer = ev.buf,
        callback = function() highlight_buf(ev.buf) end,
      })

      vim.keymap.set('n', '<CR>', function() require('plan').cycle() end, { buffer = true })
      vim.keymap.set('n', '<BS>', function() require('plan').cancel() end, { buffer = true })
    end,
  })
end

--- Resolve the plan directory from config (mirrors plan's config resolution order).
local function get_plan_dir()
  local dir = vim.env.PLAN_DIR
  if dir and dir ~= '' then
    return vim.fn.expand(dir)
  end
  local xdg = vim.env.XDG_CONFIG_HOME
  local base = (xdg and xdg ~= '') and xdg or vim.fn.expand('~/.config')
  local ok, lines = pcall(vim.fn.readfile, base .. '/plan/config')
  if ok then
    for _, line in ipairs(lines) do
      local val = line:match('^%s*dir%s*=%s*(.+)%s*$')
      if val then return vim.fn.expand(val) end
    end
  end
  return nil
end

--- Run the plan binary synchronously. Returns stdout, stderr, exit code.
local function run(args)
  local cmd = { M.config.bin }
  for _, a in ipairs(args) do
    cmd[#cmd + 1] = a
  end
  local result = vim.system(cmd, { text = true }):wait()
  return result.stdout or '', result.stderr or '', result.code
end

local timestamp_pat = ' %(%d%d%d%d%-%d%d%-%d%d%)$'

local function today()
  return os.date('%Y-%m-%d')
end

--- Check if the cursor is inside a language zone.
local function in_language_zone()
  local buf = vim.api.nvim_get_current_buf()
  local zones = buf_zone_map[buf]
  if not zones then return false end
  local row = vim.api.nvim_win_get_cursor(0)[1] -- 1-indexed
  return zones[row] == true
end

--- Cycle task state: \ → + (done), or reopen +/- → \.
--- Appends (YYYY-MM-DD) on transition to +, strips it on reopen.
function M.cycle()
  if in_language_zone() then return end
  local line = vim.api.nvim_get_current_line()
  local sigil = line:match('^([\\+%-]) ')
  if not sigil then return end

  local new
  if sigil == '\\' then
    new = '+' .. line:sub(2) .. ' (' .. today() .. ')'
  elseif sigil == '+' or sigil == '-' then
    new = '\\' .. line:sub(2):gsub(timestamp_pat, '')
  end

  if new then vim.api.nvim_set_current_line(new) end
end

--- Cancel a task (any state → -), or reopen if already cancelled (- → \).
--- Appends (YYYY-MM-DD) on cancel, strips it on reopen.
function M.cancel()
  if in_language_zone() then return end
  local line = vim.api.nvim_get_current_line()
  local sigil = line:match('^([\\*+%-]) ')
  if not sigil then return end

  local new
  if sigil == '-' then
    new = '\\' .. line:sub(2):gsub(timestamp_pat, '')
  else
    local stripped = line:sub(2):gsub(timestamp_pat, '')
    new = '-' .. stripped .. ' (' .. today() .. ')'
  end

  if new then vim.api.nvim_set_current_line(new) end
end

--- Open a plan file in a buffer. Creates the file via the plan binary if needed.
--- @param date string|nil  Date argument (e.g. "@~1", "yesterday", "last") or nil for today.
function M.open(date)
  local args = { '--path' }
  if date == 'last' then
    args[#args + 1] = '--last'
  elseif date and date ~= '' then
    args[#args + 1] = date
  end
  local stdout, stderr, code = run(args)
  if code ~= 0 then
    vim.notify('plan: ' .. vim.trim(stderr), vim.log.levels.ERROR)
    return
  end
  local path = vim.trim(stdout)
  if path == '' then return end
  vim.cmd(M.config.open_cmd .. ' ' .. vim.fn.fnameescape(path))
end

--- Log a bullet item into the inbox.
--- @param text string  The text to log (will be prefixed with "* ").
--- @param date string|nil  Optional date argument.
function M.log(text, date)
  local args = { 'log', text }
  if date and date ~= '' then args[#args + 1] = date end
  local _, stderr, code = run(args)
  if code ~= 0 then
    vim.notify('plan: ' .. vim.trim(stderr), vim.log.levels.ERROR)
  end
end

--- Jot raw text into the inbox.
--- @param text string  The text to jot (inserted as-is).
--- @param date string|nil  Optional date argument.
function M.jot(text, date)
  local args = { 'jot', text }
  if date and date ~= '' then args[#args + 1] = date end
  local _, stderr, code = run(args)
  if code ~= 0 then
    vim.notify('plan: ' .. vim.trim(stderr), vim.log.levels.ERROR)
  end
end

--- List recent plan files in the quickfix list.
function M.list()
  local plan_dir = get_plan_dir()
  if not plan_dir then
    vim.notify('plan: could not resolve plan directory', vim.log.levels.ERROR)
    return
  end
  local stdout, stderr, code = run({ 'ls' })
  if code ~= 0 then
    vim.notify('plan: ' .. vim.trim(stderr), vim.log.levels.ERROR)
    return
  end
  local items = {}
  for line in stdout:gmatch('[^\n]+') do
    local date_str = line:match('^(%d%d%d%d%-%d%d%-%d%d)')
    if date_str then
      items[#items + 1] = {
        filename = plan_dir .. '/' .. date_str .. '.plan',
        lnum = 1,
        text = vim.trim(line),
      }
    end
  end
  vim.fn.setqflist(items)
  vim.cmd('copen')
end

--- Search across plan files and populate the quickfix list.
--- @param query string  The search query.
function M.search(query)
  local plan_dir = get_plan_dir()
  if not plan_dir then
    vim.notify('plan: could not resolve plan directory', vim.log.levels.ERROR)
    return
  end
  local stdout, stderr, code = run({ 'search', query })
  if code ~= 0 then
    vim.notify('plan: ' .. vim.trim(stderr), vim.log.levels.ERROR)
    return
  end
  local items = {}
  for line in stdout:gmatch('[^\n]+') do
    local file, lnum, text = line:match('^(.-):(%-?%d+):%s*(.*)$')
    if file then
      items[#items + 1] = {
        filename = plan_dir .. '/' .. file,
        lnum = tonumber(lnum),
        text = text,
      }
    end
  end
  if #items == 0 then
    vim.notify('No results for: ' .. query, vim.log.levels.INFO)
    return
  end
  vim.fn.setqflist(items)
  vim.cmd('copen')
end

return M
