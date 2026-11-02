-- Headless end-to-end tests for the plan Neovim plugin, against a real binary.
--
--   PLAN_BIN=target/debug/plan nvim --headless --clean -l nvim/test/plugin_test.lua
--
-- The binary must be built with `--features test-clock` so PLAN_MOCK_TIME is
-- honoured. `cargo test --features test-clock --test nvim` does all of this.

local root = vim.fn.fnamemodify(debug.getinfo(1, 'S').source:sub(2), ':p:h:h')
vim.opt.rtp:prepend(root)
vim.cmd('filetype on')

local bin = assert(vim.env.PLAN_BIN, 'set PLAN_BIN to a plan binary built with --features test-clock')
local tmp = vim.fn.tempname()
local dir = tmp .. '/plans'
vim.fn.mkdir(dir, 'p')
vim.env.HOME = tmp
vim.env.XDG_CONFIG_HOME = tmp .. '/config'
vim.env.PLAN_DIR = dir
vim.env.PLAN_MOCK_TIME = '2026-02-19T10:15'
local today, yesterday = '2026-02-19', '2026-02-18'

vim.cmd.runtime('plugin/plan.lua')
local plan = require('plan')
plan.setup({ bin = bin })

-- Capture notifications instead of printing them.
local notes = {}
vim.notify = function(msg, level)
  notes[#notes + 1] = { msg = msg, level = level }
end

local failures, passed = {}, 0
local function test(name, fn)
  notes = {}
  local ok, err = pcall(fn)
  if ok then
    passed = passed + 1
    io.stdout:write('ok   ' .. name .. '\n')
  else
    failures[#failures + 1] = name
    io.stdout:write('FAIL ' .. name .. '\n     ' .. tostring(err) .. '\n')
  end
end

local function eq(actual, expected, what)
  if not vim.deep_equal(actual, expected) then
    error(string.format('%s: expected %s, got %s', what or 'value', vim.inspect(expected), vim.inspect(actual)), 2)
  end
end

local function file_lines(date)
  return vim.fn.readfile(dir .. '/' .. date .. '.plan')
end

local function buf_name()
  return vim.fn.fnamemodify(vim.api.nvim_buf_get_name(0), ':t')
end

local function qf_file(item)
  return vim.fn.fnamemodify(vim.fn.bufname(item.bufnr), ':t')
end

--- The lines strictly between the inbox fences.
local function inbox(lines)
  local out, inside = {}, false
  for _, l in ipairs(lines) do
    if l:match('^~+inbox~+$') then
      inside = true
    elseif inside and l:match('^~+$') then
      break
    elseif inside then
      out[#out + 1] = l
    end
  end
  return out
end

test(':PlanLog logs a timestamped entry into today', function()
  vim.cmd('PlanLog ship the thing')
  vim.cmd('PlanLog -leading dash')
  eq(inbox(file_lines(today)), { '10:15 ship the thing', '10:15 -leading dash' }, "today's inbox")
  eq(notes, {}, 'notifications')
end)

test('log() with a selector logs into that plan', function()
  vim.fn.writefile({ 'notes for yesterday' }, dir .. '/' .. yesterday .. '.plan')
  plan.log('late entry', 'yesterday')
  eq(inbox(file_lines(yesterday)), { '10:15 late entry' }, "yesterday's inbox")
  eq(notes, {}, 'notifications')
end)

test(':PlanLog for a missing past date reports the error', function()
  plan.log('nowhere to go', '@~5')
  eq(#notes, 1, 'notifications')
  assert(notes[1].msg:match('No plan file for that date'), vim.inspect(notes))
end)

test(':Plan opens today with the plan filetype', function()
  vim.cmd('Plan')
  eq(buf_name(), today .. '.plan', 'buffer')
  eq(vim.bo.filetype, 'plan', 'filetype')
  eq(inbox(vim.api.nvim_buf_get_lines(0, 0, -1, false)), { '10:15 ship the thing', '10:15 -leading dash' }, 'buffer inbox')
end)

test('highlights the header, fences, and entry times', function()
  local ns = vim.api.nvim_get_namespaces().plan
  local by_row = {}
  for _, m in ipairs(vim.api.nvim_buf_get_extmarks(0, ns, 0, -1, { details = true })) do
    by_row[m[2]] = by_row[m[2]] or { group = m[4].hl_group, end_col = m[4].end_col }
  end
  -- rows: 0 header, 1 inbox open, 2-3 entries, 4 inbox close
  eq(by_row[0].group, 'Title', 'header')
  eq(by_row[1].group, 'Special', 'inbox open')
  eq(by_row[2], { group = 'planTime', end_col = 5 }, 'entry time')
  eq(by_row[4].group, 'Special', 'inbox close')
end)

test(':Plan accepts every selector form', function()
  for _, sel in ipairs({ 'yesterday', '@~1', '1 day ago' }) do
    vim.cmd('Plan ' .. sel)
    eq(buf_name(), yesterday .. '.plan', ':Plan ' .. sel)
  end
  for _, sel in ipairs({ 'last', '@-', '@', 'today' }) do
    vim.cmd('Plan ' .. sel)
    eq(buf_name(), today .. '.plan', ':Plan ' .. sel)
  end
  eq(notes, {}, 'notifications')
end)

test(':Plan for a missing past date reports the error and stays put', function()
  vim.cmd('Plan')
  vim.cmd('Plan @~5')
  eq(buf_name(), today .. '.plan', 'buffer')
  eq(#notes, 1, 'notifications')
  assert(notes[1].msg:match('No plan file for that date'), vim.inspect(notes))
end)

test(':PlanList fills the quickfix list, newest first', function()
  vim.cmd('PlanList')
  local qf = vim.fn.getqflist()
  eq(#qf, 2, 'items')
  eq(qf_file(qf[1]), today .. '.plan', 'first file')
  eq(qf_file(qf[2]), yesterday .. '.plan', 'second file')
  assert(qf[1].text:match('^2026%-02%-19  Thu'), qf[1].text)
  vim.cmd('cclose')
end)

test(':PlanSearch fills the quickfix list with line numbers', function()
  vim.cmd('PlanSearch ship')
  local qf = vim.fn.getqflist()
  eq(#qf, 1, 'items')
  eq(qf_file(qf[1]), today .. '.plan', 'file')
  eq(qf[1].lnum, 3, 'line')
  eq(qf[1].text, '10:15 ship the thing', 'text')
  vim.cmd('cclose')
end)

test(':PlanSearch with no matches notifies', function()
  vim.cmd('PlanSearch zzz-nothing-matches')
  eq(#notes, 1, 'notifications')
  assert(notes[1].msg:match('No results'), vim.inspect(notes))
end)

test('<CR> cycles a task, <BS> cancels it', function()
  vim.cmd('Plan')
  local cr = vim.fn.maparg('<CR>', 'n', false, true)
  local bs = vim.fn.maparg('<BS>', 'n', false, true)
  assert(cr.buffer == 1 and cr.callback, '<CR> is not mapped in the plan buffer')
  assert(bs.buffer == 1 and bs.callback, '<BS> is not mapped in the plan buffer')
  local stamp = ' (' .. os.date('%Y-%m-%d') .. ')'
  vim.api.nvim_buf_set_lines(0, 2, 2, false, { '\\ open task', '* bullet' })
  vim.api.nvim_win_set_cursor(0, { 3, 0 })
  cr.callback()
  eq(vim.api.nvim_get_current_line(), '+ open task' .. stamp, 'done')
  cr.callback()
  eq(vim.api.nvim_get_current_line(), '\\ open task', 'reopened')
  vim.api.nvim_win_set_cursor(0, { 4, 0 })
  bs.callback()
  eq(vim.api.nvim_get_current_line(), '- bullet' .. stamp, 'cancelled')
  bs.callback()
  eq(vim.api.nvim_get_current_line(), '\\ bullet', 'reopened')
  vim.cmd('bwipeout!')
end)

test('inbox entries are a plain log: the keymaps leave them alone', function()
  vim.cmd('Plan')
  local entry = '10:15 ship the thing'
  vim.api.nvim_win_set_cursor(0, { 3, 0 })
  eq(vim.api.nvim_get_current_line(), entry, 'cursor line')
  vim.fn.maparg('<CR>', 'n', false, true).callback()
  vim.fn.maparg('<BS>', 'n', false, true).callback()
  eq(vim.api.nvim_get_current_line(), entry, 'entry after <CR> and <BS>')
  eq(vim.bo.modified, false, 'buffer modified')
  vim.cmd('bwipeout!')
end)

vim.fn.delete(tmp, 'rf')
io.stdout:write(string.format('\n%d passed, %d failed\n', passed, #failures))
os.exit(#failures == 0 and 0 or 1)
