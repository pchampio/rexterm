-- Keep the user's real appearance and integrations, and let tmux interpret
-- application-aware navigation in control-mode panes.
local wezterm = require 'wezterm'
local home = os.getenv('HOME')
package.path = home .. '/.config/wezterm/?.lua;' .. home .. '/.config/wezterm/?/init.lua;' .. package.path
local config = dofile(home .. '/.wezterm.lua')
-- Native CC panes no longer include tmux's old status-line text. Preserve
-- the user's Shift-click based_copymode workflow using the actual domain.
local tmux = package.loaded.tmux
if type(tmux) == 'table' and type(tmux.is_inside_tmux) == 'function' then
  local original_is_inside_tmux = tmux.is_inside_tmux
  tmux.is_inside_tmux = function(pane)
    return pane:get_domain_name() == 'tmux' or original_is_inside_tmux(pane)
  end
end
-- Click copy (based_copymode): Shift-click shows "CLICK COPY" in the pane
-- status row, then a plain drag selects. WezTerm itself copies on the mouse
-- release (snapping an edge that is one letter short of a word) and closes
-- the mode, so no toggle state can get out of step with the overlay.
local bc = package.loaded.based_copymode
if type(bc) == 'table' then
  local act = wezterm.action
  local function clear_later(window, pane)
    wezterm.time.call_after(0.25, function()
      window:perform_action(act.ClearSelection, pane)
    end)
  end
  bc.single_streak_click = wezterm.action_callback(function(window, pane)
    local text = window:get_selection_text_for_pane(pane)
    if text and text ~= '' then
      -- A Shift-drag selection made without click copy.
      window:perform_action(act.CopyTo('ClipboardAndPrimarySelection'), pane)
      clear_later(window, pane)
      return
    end
    window:perform_action(act.QuickSelectArgs({
      label = 'click-copy',
      patterns = { 'jksfldjjkfdsljflflsdkfjlsdjfdsfjlsdjflksdjklf' },
    }), pane)
  end)
  bc.double_streak_click = wezterm.action_callback(function(window, pane)
    window:perform_action(act.SelectTextAtMouseCursor('Word'), pane)
    window:perform_action(act.CopyTo('ClipboardAndPrimarySelection'), pane)
    clear_later(window, pane)
  end)
end
-- Every lab build draws the tmux window strip natively. (A version-name test
-- for 'continuous-tabs' once switched it off when the build was renamed.)
if wezterm.version:find('tmuxcc', 1, true) then
  config.tmux_continuous_window_tabs = true
end
-- tmux applies its own active/inactive styles within one ordinary terminal.
config.inactive_pane_hsb = {saturation=1, brightness=1}
for _, binding in ipairs(config.keys or {}) do
  if binding.mods == 'CTRL' and ({h=true,j=true,k=true,l=true})[binding.key] then
    local original = binding.action
    local key = binding.key
    binding.action = wezterm.action_callback(function(window, pane)
      if pane:get_domain_name() == 'tmux' then
        window:perform_action(wezterm.action.SendKey{key=key, mods='CTRL'}, pane)
      else
        window:perform_action(original, pane)
      end
    end)
  end
end
return config
