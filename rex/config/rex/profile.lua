-- Load the ordinary WezTerm profile without writing or replacing any dotfile.
-- Plugins see their normal config_dir/config_file and module search paths.
local M={}
function M.load(wezterm)
  local home=wezterm.home_dir
  local xdg=os.getenv('XDG_CONFIG_HOME') or home..'/.config'
  local explicit=os.getenv('WEZTERM_REX_BASE_CONFIG')
  local candidates=explicit and {explicit} or {home..'/.wezterm.lua',xdg..'/wezterm/wezterm.lua'}
  for _,path in ipairs(candidates) do
    local file=io.open(path,'r')
    if file then
      file:close()
      local dir=path:match('^(.*)/') or '.'
      package.path=dir..'/?.lua;'..dir..'/?/init.lua;'..xdg..'/wezterm/?.lua;'..xdg..'/wezterm/?/init.lua;'..package.path
      wezterm.add_to_config_reload_watch_list(path)
      wezterm.config_dir=dir
      wezterm.config_file=path
      return dofile(path)
    end
  end
  if explicit then error('Cannot read WEZTERM_REX_BASE_CONFIG: '..explicit) end
  -- This is WezTerm's normal default appearance, not a replacement theme/font.
  return wezterm.config_builder()
end
return M
