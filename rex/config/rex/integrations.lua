-- Local helpers (voice, audio, vault unlock) stay on the GUI's machine even
-- when the session is remote. These are actual native floating panes in both
-- ordinary WezTerm tabs and attached Rex tabs; no split-pane emulation.
local M={}
function M.apply(config,wezterm)
  local toggle=package.loaded.toggle_terminal
  if type(toggle)~='table' or toggle.rex_popups_installed then return end
  toggle.rex_popups_installed=true
  local function read_states()
    local saved=wezterm.GLOBAL.rex_local_popups_json
    if type(saved)=='string' then
      local ok,value=pcall(wezterm.json_parse,saved);if ok then return value end
    end
    return {}
  end
  local function remember(key,id)
    local states=read_states();states[key]=id
    wezterm.GLOBAL.rex_local_popups_json=wezterm.json_encode(states)
  end
  local function get(key)
    local id=read_states()[key];if not id then return end
    local ok,p=pcall(wezterm.mux.get_pane,id)
    if ok and p then return p end
    remember(key,nil)
  end
  local function popup(pane,name,args)
    local key=tostring(pane:tab():tab_id())..':'..name
    local p=get(key)
    if p then p:set_popup_visible(not p:is_popup_visible());return p end
    args.domain={DomainName='local'}
    args.cwd=args.cwd or wezterm.home_dir
    args.width=args.width or .8
    p=pane:spawn_popup(args);remember(key,p:pane_id());return p
  end
  toggle.toggle_terminal=function(window,pane)
    return popup(pane,'shell',{height=toggle.opts.size.Cells or .4})
  end
  toggle.toggle_command=function(window,pane,name,spawn)
    return popup(pane,name,{args=spawn.args,cwd=spawn.cwd,height=spawn.size or .4})
  end
  toggle.send_command_to_tab=function(window,text)
    local pane=window:active_pane()
    if not pane then return end
    local p=get(tostring(pane:tab():tab_id())..':shell')
    if p then p:send_text(text..(text:sub(-1)=='\n' and '' or '\n')) end
  end
  local complete=package.loaded.auto_complete
  if type(complete)=='table' then
    -- The original callback waits synchronously for `rbw unlocked`; an input
    -- callback cannot wait for a password that this same GUI must deliver.
    local cfg=complete.config
    local pending={}
    local function fill(window,pane)
      local function insert(command)
        local success,password=complete.run_cmd(command)
        if success and password and password~='' then pane:send_text(password) end
      end
      local text=pane:get_logical_lines_as_text()
      for pattern,command in pairs(cfg.password_patterns or {}) do
        if text:find(pattern) then insert(command);return end
      end
      window:perform_action(wezterm.action.InputSelector{title='Choose Password',fuzzy=true,choices=cfg.fallback_passwords_prompt,
        action=wezterm.action_callback(function(_,_,command)if command then insert(command) end end)},pane)
    end
    local action=wezterm.action_callback(function(window,pane)
      local id=pane:pane_id()
      if pending[id] then return end
      if not cfg.is_locked() then fill(window,pane);return end
      local rbw=wezterm.home_dir..'/.local/bin/rbw'
      local f=io.open(rbw,'r');if f then f:close() else rbw='rbw' end
      local unlock=pane:spawn_popup{domain={DomainName='local'},args={'sh','-c','"$1" unlock; exit 0','rex-unlock',rbw},width=.8,height=5,cwd=wezterm.home_dir}
      pending[id]=true
      local function poll()
        local ok,target=pcall(wezterm.mux.get_pane,id)
        if not ok or not target then pending[id]=nil;return end
        local ok,live=pcall(wezterm.mux.get_pane,unlock:pane_id())
        if ok and live then wezterm.time.call_after(.1,poll);return end
        pending[id]=nil
        if not cfg.is_locked() then fill(window,pane) end
      end
      wezterm.time.call_after(.1,poll)
    end)
    -- Preserve the profile's patterns, helpers and fallback choices.
    for _,binding in ipairs(config.keys or {}) do
      if binding.key=='L' and (binding.mods=='CTRL|SHIFT' or binding.mods=='SHIFT|CTRL') then binding.action=action end
    end
  end
end
return M
