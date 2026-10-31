-- Priority 10 puts this ahead of catch-all patterns such as snacks.nvim bigfile's `.*` (priority 0),
-- which would otherwise turn every IFC file above 1.5 MB into filetype `bigfile`.
local function step_content(_, bufnr)
  if vim.api.nvim_buf_get_lines(bufnr, 0, 1, false)[1]:match('^%s*ISO%-10303%-21%s*;') then
    return 'ifc'
  end
end
vim.filetype.add({
  pattern = {
    ['.*%.[iI][fF][cC]'] = { 'ifc', { priority = 10 } },
    ['.*%.[sS][tT][pP]'] = { step_content, { priority = 10 } },
    ['.*%.[sS][tT][eE][pP]'] = { step_content, { priority = 10 } },
    ['.*%.p21'] = { step_content, { priority = 10 } },
  },
})
