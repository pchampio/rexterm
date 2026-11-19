-- GUI side of rex-splits.nvim. OSC travels over the existing terminal stream.
local M={}
function M.apply(wezterm)
  local directions={Left=true,Right=true,Up=true,Down=true}
  wezterm.on('user-var-changed',function(window,pane,name,value)
    if name~='WEZTERM_PANE_REQUEST' then return end
    local active=window:active_pane()
    if not active or active:pane_id()~=pane:pane_id() then return end
    local ok,request=pcall(wezterm.json_parse,value)
    if not ok or type(request)~='table' or request.version~=1 or not directions[request.direction] then return end
    if request.action=='move' then
      window:perform_action(wezterm.action.ActivatePaneDirection(request.direction),pane)
    elseif request.action=='resize' then
      local amount=tonumber(request.amount)
      if amount and amount>=1 and amount<=100 and amount%1==0 then
        window:perform_action(wezterm.action.AdjustPaneSize{request.direction,amount},pane)
      end
    end
  end)
end
return M
