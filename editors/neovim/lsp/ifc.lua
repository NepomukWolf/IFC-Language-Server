---@type vim.lsp.Config
return {
  cmd = { 'ifc-language-server' },
  filetypes = { 'ifc' },
  root_markers = { '.git' },
  workspace_required = false,
}
