if vim.g.loaded_plan then return end
vim.g.loaded_plan = true

vim.api.nvim_create_user_command('Plan', function(opts)
  require('plan').open(opts.args ~= '' and opts.args or nil)
end, { nargs = '*', desc = 'Open a plan file (today, @~N, yesterday, @- for the most recent)' })

vim.api.nvim_create_user_command('PlanLog', function(opts)
  require('plan').log(opts.args)
end, { nargs = '+', desc = "Log a timestamped entry to today's inbox" })

vim.api.nvim_create_user_command('PlanList', function()
  require('plan').list()
end, { desc = 'List recent plan files in quickfix' })

vim.api.nvim_create_user_command('PlanSearch', function(opts)
  require('plan').search(opts.args)
end, { nargs = '+', desc = 'Search across plan files' })
