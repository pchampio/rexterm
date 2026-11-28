-- Native one-row tab status. Independent of session transport.
local M = {}
function M.message(wezterm,window,text)
  local all=wezterm.GLOBAL.rex_messages or {}
  all[tostring(window:window_id())]={text=text,expires=os.time()+3}
  wezterm.GLOBAL.rex_messages=all
  if window.set_status_message then window:set_status_message(wezterm.format{{Background={Color='#eee8d5'}},{Foreground={Color='#586e75'}},{Text=' '..text..' '}}) end
end
local rank = { idle = 0, done = 1, working = 2, error = 3, blocked = 4 }
local names = { idle = '', working = 'Working', done = 'Done', error = 'Error', blocked = 'Needs input' }
local kinds = { permission = 'Approval', question = 'Question', auth = 'Sign-in' }

-- The state of the agent in a pane: 'working', 'needs_input', 'idle' or ''
-- (no agent), and its message. bin/rex-agent-status reports it with OSC 7501,
-- in the record that has the agent's name as its id; another tool can still
-- report it with OSC 7777 (status;...).
local function agent_status(wezterm, vars)
  local name = vars.WEZTERM_AGENT_AGENT or ''
  if name ~= '' then
    local ok, records = pcall(wezterm.json_parse, vars.WEZTERM_PROGRAM_STATUS or '{}')
    local r = ok and type(records) == 'table' and records[name]
    if type(r) == 'table' then
      return ({ working = 'working', blocked = 'needs_input' })[r.state] or 'idle', r.msg or ''
    end
  end
  return vars.WEZTERM_AGENT_STATUS or '', vars.WEZTERM_AGENT_MESSAGE or ''
end
M.agent_status = agent_status

local function summarize(wezterm, vars)
  local result = { state = 'idle', text = '', message = '' }
  local ok, records = pcall(wezterm.json_parse, vars.WEZTERM_PROGRAM_STATUS or '{}')
  if ok and type(records) == 'table' then
    for _, r in pairs(records) do
      if type(r) == 'table' and (rank[r.state] or -1) > rank[result.state] then
        result = { state = r.state, text = kinds[r.kind] or names[r.state], message = r.msg or r.title or '', kind = r.kind, app = r.effective_app or r.app }
        if type(r.progress) == 'number' then result.text = result.text .. ' ' .. tostring(r.progress) .. '%' end
      end
    end
  end
  local agent = ({working='working', idle='idle', needs_input='blocked'})[vars.WEZTERM_AGENT_STATUS]
  if agent and rank[agent] > rank[result.state] then
    result = { state = agent, text = names[agent], message = vars.WEZTERM_AGENT_MESSAGE or '' }
  end
  return result
end
M.summarize = summarize
-- The tab bar indicator (drawn by the GUI, see tab_icons.rs) for a summary.
local indicators = { working = 'working', done = 'done', error = 'error' }
local blocked_kinds = { question = 'question', permission = 'approval', auth = 'signin' }
local function indicator(s)
  if s.state == 'blocked' then return blocked_kinds[s.kind] or 'question' end
  return indicators[s.state]
end
M.indicator = indicator

function M.apply(config, wezterm, module_dir)
  -- Program tiles before the tab names (see icons.lua). `module_dir` is the
  -- bundle's rex directory; without it the bar shows names only.
  local icons
  if module_dir then
    local dir=module_dir..'/program-icons'
    local ok,resolver=pcall(function()return dofile(module_dir..'/icons.lua').load(wezterm,dir)end)
    if ok and resolver then icons=resolver;config.tab_bar_icon_dir=dir end
  end
  -- The simple native bar occupies exactly one terminal row at the bottom.
  config.enable_tab_bar = true
  config.use_fancy_tab_bar = false
  config.tab_bar_at_bottom = true
  config.hide_tab_bar_if_only_one_tab = false
  config.show_new_tab_button_in_tab_bar = false
  config.status_update_interval = 100
  config.copy_mode_pane_status = true
  config.colors=config.colors or {}
  config.colors.split='#bcbcbc'
  local scheme=(config.color_schemes or {})[config.color_scheme or '']
  if not scheme and wezterm.color and wezterm.color.get_builtin_schemes then scheme=wezterm.color.get_builtin_schemes()[config.color_scheme or ''] end
  local background=config.colors.background or (scheme and scheme.background) or '#fbf3db'
  config.tab_bar_active_underline='#af0000'
  config.colors.tab_bar={background=background,active_tab={bg_color='#d7d7d7',fg_color='#5f5f5f'},
    inactive_tab={bg_color='#d7d7d7',fg_color='#5f5f00'},inactive_tab_hover={bg_color='#e4e4e4',fg_color='#5f5f00'}}
  local key = 'wezterm_rex_status'
  wezterm.GLOBAL[key] = wezterm.GLOBAL[key] or {}
  local function read() return wezterm.GLOBAL[key] or {} end
  local function panes(tab)
    if not tab then return {} end
    local ok,list=pcall(function()return tab:panes()end)
    return ok and list or {}
  end
  local function tabs(window)
    local ok,list=pcall(function()return window:tabs()end)
    return ok and list or {}
  end
  local function active_tab(window)
    -- An update may resume after the GUI's mux window has been closed.
    local ok,tab=pcall(function()return window:active_tab()end)
    return ok and tab or nil
  end
  local function visible(window, pane)
    if not window:is_focused() then return false end
    local tab = active_tab(window)
    for _, p in ipairs(panes(tab)) do
      if p:pane_id() == pane:pane_id() then return true end
    end
    return false
  end
  wezterm.on('user-var-changed', function(window, pane, name)
    if name ~= 'WEZTERM_PROGRAM_STATUS' and name ~= 'WEZTERM_REX_NOTIFICATION' and not name:match('^WEZTERM_AGENT_') then return end
    local all = read()
    local id = tostring(pane:pane_id())
    local prev = all[id] or { state = 'idle', unread = 0 }
    local ok,vars=pcall(function()return pane:get_user_vars()end)
    if not ok then return end
    local now = summarize(wezterm, vars)
    now.unread = prev.unread or 0
    -- An agent that finishes while you look elsewhere shows a check until
    -- you come back to its tab (update-status clears it).
    local agent_idle = now.state == 'idle' and agent_status(wezterm, vars) == 'idle'
    if agent_idle and not visible(window, pane) and (prev.agent_done or prev.state == 'working' or prev.state == 'blocked') then
      now.state, now.text, now.agent_done = 'done', 'Done', true
    end
    -- A "done" report (Codex sends OSC 7501 state=done at the end of a turn)
    -- shows a check only until the tab is seen: once seen, the same report
    -- stays quiet (update-status records it as seen).
    now.seen_program = prev.seen_program
    if now.state == 'done' and not now.agent_done then
      if visible(window, pane) or prev.seen_program == vars.WEZTERM_PROGRAM_STATUS then
        now.state, now.text, now.seen_program = 'idle', '', vars.WEZTERM_PROGRAM_STATUS
      end
    end
    if visible(window, pane) then
      now.unread = 0
    elseif name == 'WEZTERM_REX_NOTIFICATION' or (now.state ~= prev.state and (now.state == 'blocked' or now.state == 'error' or now.state == 'done')) then
      now.unread = math.min(999, now.unread + 1)
    end
    all[id] = now
    wezterm.GLOBAL[key] = all
  end)
  -- Agent shells: bin/rex-agent-shell runs each tool command of a coding
  -- agent in a detached block of the Rex server. The block's label names the
  -- agent's own block, so a tab knows the shells of its agents. The wrapper
  -- sets the user variable REX_AGENT_SHELL when a shell starts or ends.
  local act=wezterm.action
  local shells_key='rex_agent_shells'
  local function rex_pane(window,pane)
    local ok,c=pcall(function()return pane:rex_context()end)
    if ok and c then return pane,c end
    for _,peer in ipairs(panes(active_tab(window))) do
      local valid,remote=pcall(function()return peer:rex_context()end)
      if valid and remote then return peer,remote end
    end
  end
  local function refresh_shells(pane,c)
    local ok,list=pcall(function()return pane:rex_call('wezterm.agent_shells')end)
    if not ok or type(list)~='table' then return end
    local all=wezterm.GLOBAL[shells_key] or {}
    -- wezterm.GLOBAL keeps an empty table as an object, not as a list:
    -- a session without shells has no entry.
    all[c.session_id]=#list>0 and {time=os.time(),shells=list} or nil
    wezterm.GLOBAL[shells_key]=all
  end
  -- The agent shells of the panes of the window's active tab.
  local function tab_shells(window)
    local store,found=wezterm.GLOBAL[shells_key] or {},{}
    for _,p in ipairs(panes(active_tab(window))) do
      local ok,c=pcall(function()return p:rex_context()end)
      local entry=ok and c and store[c.session_id]
      for _,shell in ipairs(entry and entry.shells or {}) do
        if shell.parent==c.block_id then found[#found+1]=shell end
      end
    end
    return found
  end
  wezterm.on('user-var-changed',function(window,pane,name)
    if name~='REX_AGENT_SHELL' then return end
    local target,c=rex_pane(window,pane)
    if target then refresh_shells(target,c) end
  end)
  -- The rows of the menu call these fixed actions: a callback made for each
  -- menu would stay registered for ever.
  -- The agent panel (Rust, agent_panel.rs): a card for each agent shell of
  -- this tab, with a live preview of its terminal and Open / Stop buttons.
  -- The server opens a live terminal for each shell for the previews.
  local function agent_tile(agent)
    if not icons then return nil end
    local slug=icons.slug(agent)
    -- An agent without a logo of its own gets the AI tile (sparkles).
    return (slug:match('^other%-') or slug=='shell') and (icons.slug('agent')) or slug
  end
  -- The agent sessions of the window's active tab: the panes whose agent
  -- reports its status (bin/rex-agent-status, OSC 7777).
  local function tab_agents(window)
    local found={}
    for _,p in ipairs(panes(active_tab(window))) do
      local valid,vars=pcall(function()return p:get_user_vars()end)
      local status,message=agent_status(wezterm,valid and vars or {})
      if valid and (status~='' or (vars.WEZTERM_AGENT_AGENT or '')~='') then
        local name=vars.WEZTERM_AGENT_AGENT
        if not name or name=='' then name=(vars.WEZTERM_AGENT_TITLE or ''):match('^(%S+)') or 'agent' end
        local ok,c=pcall(function()return p:rex_context()end)
        found[#found+1]={name=name,status=status~='' and status or 'idle',title=vars.WEZTERM_AGENT_TITLE or '',
          detail=(status=='needs_input' and message) or vars.WEZTERM_AGENT_TOOL or '',
          block_id=ok and c and c.block_id or '',pane_id=p:pane_id(),icon=agent_tile(name)}
      end
    end
    return found
  end
  local function shell_menu(window,pane)
    local agents=tab_agents(window)
    local target,c=rex_pane(window,pane)
    -- Without Rex there is no agent panel: the button only shows the agents.
    if not target then return end
    local ok,list=pcall(function()return target:rex_call('wezterm.agent_shells_watch')end)
    if not ok or type(list)~='table' then list={} end
    local parents={}
    for _,p in ipairs(panes(active_tab(window))) do
      local valid,context=pcall(function()return p:rex_context()end)
      if valid and context then parents[context.block_id]=true end
    end
    local entries={}
    for _,shell in ipairs(list) do
      if parents[shell.parent] then
        entries[#entries+1]={block_id=shell.block_id,agent=shell.agent or '',title=shell.title or '',
          state=shell.state or 'running',pane_id=shell.pane_id,icon=agent_tile(shell.agent),parent=shell.parent or ''}
      end
    end
    -- Running commands first, then the newest finished ones (block ids grow
    -- with time). An empty list still opens the panel: it shows the agents.
    table.sort(entries,function(a,b)
      local ra,rb=a.state=='running',b.state=='running'
      if ra~=rb then return ra end
      return a.block_id>b.block_id
    end)
    window:perform_action(act.AgentPanel{entries=entries,agents=agents},target)
  end
  -- A click on the agent button opens the panel (another click closes it);
  -- prefix l too (workflow.lua).
  wezterm.on('right-status-click',shell_menu)
  wezterm.on('rex-agent-panel',shell_menu)
  wezterm.on('update-status', function(window, pane)
    local messages=wezterm.GLOBAL.rex_messages or {}
    local message=messages[tostring(window:window_id())]
    if window.set_status_message then window:set_status_message(message and message.expires>os.time() and wezterm.format{{Background={Color='#eee8d5'}},{Foreground={Color='#586e75'}},{Text=' '..message.text..' '}} or '') end
    local all = read()
    local live = {}
    for _, win in ipairs(wezterm.mux.all_windows()) do
      for _, tab in ipairs(tabs(win)) do
        for _, p in ipairs(panes(tab)) do live[tostring(p:pane_id())] = true end
      end
    end
    for id in pairs(all) do if not live[id] then all[id] = nil end end
    if window:is_focused() then
      local tab = active_tab(window)
      for _, p in ipairs(panes(tab)) do
        local s = all[tostring(p:pane_id())]
        if s then
          s.unread = 0
          if s.state == 'done' then
            local ok, pv = pcall(function() return p:get_user_vars() end)
            s.state, s.text, s.agent_done = 'idle', '', nil
            s.seen_program = ok and pv.WEZTERM_PROGRAM_STATUS or nil
          end
        end
      end
    end
    wezterm.GLOBAL[key] = all
    local ok,vars = pcall(function()return pane:get_user_vars()end)
    if not ok then return end
    -- Agents show as logos (the agent button), never their tool, title or
    -- message as text.
    local text = ''
    local width = math.max(0, math.min(48, math.floor(window:get_dimensions().pixel_width / 24)))
    local ok,c=pcall(function()return pane:rex_context()end)
    -- A local helper popup still belongs to its Rex tab.
    local attached=ok and c~=nil
    if not attached then
      for _,peer in ipairs(panes(active_tab(window))) do
        local valid,remote=pcall(function()return peer:rex_context()end)
        if valid and remote then attached=true;break end
      end
    end
    if window.set_tab_bar_rule then window:set_tab_bar_rule(attached) end
    local sync=ok and c and (wezterm.GLOBAL.rex_sync or {})[c.window_id]
    if window.set_left_status then window:set_left_status(window:active_key_table()=='rex_passthrough' and ' ☞ Bound to remote ' or '   ') end
    local indicators={}
    if sync then indicators[#indicators+1]='⇔ SYNC' end
    local tab=active_tab(window)
    local zoom_ok,zoomed=pcall(function()return tab and tab:is_zoomed()end)
    if zoom_ok and zoomed then indicators[#indicators+1]='↗ ZOOM' end
    local right=wezterm.truncate_right(text,width)
    if #indicators>0 then right=right..'  '..table.concat(indicators,'  ') end
    -- A list that still has shells can be stale (an agent was killed).
    local store=wezterm.GLOBAL[shells_key] or {}
    local entry=ok and c and store[c.session_id]
    -- The wrapper tells the GUI through the agent's terminal; a Codex command
    -- has no terminal. While an agent of this tab works, ask the server.
    local working=false
    for _,peer in ipairs(panes(active_tab(window))) do
      local valid,peer_vars=pcall(function()return peer:get_user_vars()end)
      local status=valid and agent_status(wezterm,peer_vars)
      if status=='working' or status=='needs_input' then working=true;break end
    end
    local last=(wezterm.GLOBAL.rex_agent_poll or {})[ok and c and c.session_id or '']
    if ok and c and (entry or working) and os.time()-(entry and entry.time or last or 0)>=(entry and 2 or 1) then
      local polls=wezterm.GLOBAL.rex_agent_poll or {};polls[c.session_id]=os.time();wezterm.GLOBAL.rex_agent_poll=polls
      refresh_shells(pane,c)
    end
    local shells=tab_shells(window)
    local items={{Background={Color=background}},{Foreground={Color='#586e75'}},{Text=right~='' and right..' ' or ''}}
    -- The agent button at the right end: one tile for each agent of this tab
    -- (Claude, Codex), then the number of running commands. It shows as soon
    -- as an agent runs in the tab. A click opens the agent panel.
    local stack,seen,running={},{},0
    local function add(agent)
      local tile=agent and agent~='' and (agent_tile(agent) or 'shell') or nil
      if tile and not seen[tile] and #stack<3 then seen[tile]=true;stack[#stack+1]=tile end
    end
    for _,peer in ipairs(panes(active_tab(window))) do
      local valid,peer_vars=pcall(function()return peer:get_user_vars()end)
      if valid and (agent_status(wezterm,peer_vars)~='' or (peer_vars.WEZTERM_AGENT_AGENT or '')~='') then
        local name=peer_vars.WEZTERM_AGENT_AGENT
        if not name or name=='' then name=(peer_vars.WEZTERM_AGENT_TITLE or ''):match('^(%S+)') or 'agent' end
        add(name)
      elseif valid and summarize(wezterm,peer_vars).state~='idle' then
        -- A program that reports its status (Codex: OSC 7501) is an agent
        -- too: the logo of its app, else of its process (the AI tile when
        -- it has none).
        local okp,process=pcall(wezterm.json_parse,peer_vars.WEZTERM_REX_PROCESS or 'null')
        local name=summarize(wezterm,peer_vars).app or (okp and type(process)=='table' and (process.name or process.path))
        if not name or name=='' or tostring(name):match('^%d[%d%.]*$') then
          local okf,fg=pcall(function()return peer:get_foreground_process_name()end)
          name=okf and fg or nil
        end
        add((name and name~='') and name or 'agent')
      end
    end
    for _,shell in ipairs(shells) do
      add(shell.agent)
      if shell.running~=false and (shell.state or 'running')=='running' then running=running+1 end
    end
    -- Outside Rex too: the agents of the tab show (no agent panel there).
    if #stack>0 then
      items[#items+1]={Text=icons and icons.cells(stack,icons.width(#stack),true) or ' agents '}
      if running>0 then items[#items+1]={Foreground={Color='#586e75'}};items[#items+1]={Text=' '..tostring(running)..' '} end
    end
    window:set_right_status((right~='' or #stack>0) and wezterm.format(items) or '')
  end)
  -- The name of a process. Claude Code runs a binary named after its
  -- version (~/.local/share/claude/versions/2.1.326): use the program's
  -- directory name then.
  local function program_name(process)
    if type(process)~='table' then return nil end
    local name=process.name
    if type(name)=='string' and name:match('^%d[%d%.]*$') and type(process.path)=='string' then
      local owner=process.path:match('([^/]+)/versions/[^/]+$') or process.path:match('([^/]+)/[^/]+$')
      if owner then return owner end
    end
    return name or process.script
  end
  local title_cache={}
  local title_started={}
  local shell_title=(os.getenv("SHELL") or "/bin/zsh"):match("([^/]+)$")
  -- A pane's age is kept by its Rex block: after a session switch or a
  -- reattach the new pane of a known block is not "new" again (its tile
  -- comes from the program it runs, not the shell tile of a starting pane).
  local function age_key(pane)
    local block=(pane.user_vars or {}).WEZTERM_REX_BLOCK
    return (block and block~='') and block or pane.pane_id
  end
  wezterm.on('format-tab-title-compact', function(tab, _, _, _, max_width)
    local all, unread, best = read(), 0, {state='idle', text=''}
    local now=os.time()
    -- One status drawing for each pane that has one (each agent of the tab),
    -- in the order of the panes.
    local marks={}
    for _, pane in ipairs(tab.panes or {}) do
      -- Age every pane, including inactive splits. A tab can be hours old
      -- when its newly focused shell starts running initialization helpers.
      title_started[age_key(pane)]=title_started[age_key(pane)] or now
      local s = all[tostring(pane.pane_id)] or summarize(wezterm, pane.user_vars or {})
      unread = unread + (s.unread or 0)
      if rank[s.state] > rank[best.state] then best = s end
      local m=indicator(s)
      if m and #marks<4 then marks[#marks+1]=m end
    end
    local title = tab.tab_title
    if title == '' or title:match('^Window %x+$') then
      local pane=tab.active_pane or {};local vars=pane.user_vars or {}
      local ok,process=pcall(wezterm.json_parse,vars.WEZTERM_REX_PROCESS or '{}')
      local process_title=ok and program_name(process)
      -- At its prompt the shell is the program, whatever helper a prompt
      -- hook runs (git, pgrep). OSC 133 marks give WEZTERM_AT_PROMPT.
      if vars.WEZTERM_AT_PROMPT=='1' then process_title=shell_title end
      title=process_title or pane.title or shell_title
      local id=pane.pane_id and age_key(pane) or ('tab:'..tostring(tab.tab_id or tab.tab_index))
      title_started[id]=title_started[id] or now
      -- Shell initialization briefly exposes helpers as the foreground job.
      -- Keep the initial label stable; explicit tab names always win.
      if now-title_started[id]<2 or title=='' or
          (not process_title and title:lower()=='wezterm') then title=shell_title end
    end
    -- The status is a small drawing, not a word: dots that move while an
    -- agent works, a "?" bubble while it waits for an answer, and so on.
    local mark=marks[1]
    -- The unread count only shows when no drawing says it already.
    local suffix = (unread > 0 and not mark) and (' [' .. tostring(unread) .. ']') or ''
    local parts={}
    for i,m in ipairs(marks) do parts[i]='\27]7778;status;'..m..';2\7' end
    local mark_text=table.concat(parts,' ')
    local progress=best.text:match('%d+%%$')
    if progress then suffix=suffix..' '..progress end
    -- The program tiles of the tab stand where the tab number was: the
    -- focused pane's tile in front, two other panes' tiles behind it (as
    -- gorex does). Without tiles the number stays.
    local prefix=' '..tostring(tab.tab_index+1)..' '
    local prefix_cells=wezterm.column_width(prefix)
    if icons then
      local slugs={}
      local function add(pane)
        local vars=pane.user_vars or {}
        local ok,process=pcall(wezterm.json_parse,vars.WEZTERM_REX_PROCESS or 'null')
        local slug
        if now-(title_started[age_key(pane)] or now)<2 or vars.WEZTERM_AT_PROMPT=='1' then slug='shell'
        elseif ok and type(process)=='table' then
          slug=icons.slug(program_name(process) or process.path)
          -- A script (node, python) shows its own tile when it has one.
          if slug:match('^other%-') and type(process.script)=='string' then
            local own=icons.slug(process.script:match('([^/]+)$'))
            if not own:match('^other%-') then slug=own end
          end
        else slug=icons.slug(pane.foreground_process_name) end
        slugs[#slugs+1]=slug
      end
      local active=tab.active_pane
      if active then add(active) end
      for _,pane in ipairs(tab.panes or {}) do
        if #slugs>=3 then break end
        if not active or pane.pane_id~=active.pane_id then add(pane) end
      end
      if #slugs==0 then slugs={'shell'} end
      prefix_cells=icons.width(#slugs)
      prefix=icons.cells(slugs,prefix_cells)
    end
    local budget = math.max(0, max_width - prefix_cells - wezterm.column_width(suffix .. '   ') - 3*#marks)
    local cache_key=(tab.is_active and 'active' or 'inactive')..'\0'..prefix..title..suffix..mark_text..'\0'..max_width
    local cached=title_cache[tab.tab_id or tab.tab_index]
    if cached and cached.key==cache_key then return cached.text end
    local formatted=wezterm.format{{Background={Color=background}},{Text=' '},
      -- The tiles sit on the same grey as the title; the active tab has a red
      -- line along its bottom (tab_bar_active_underline). Without tiles, the
      -- number keeps the red (active) or grey box.
      {Background={Color=icons and '#d7d7d7' or (tab.is_active and '#af0000' or '#5f5f5f')}},
      {Foreground={Color=tab.is_active and '#c0c0c0' or '#808080'}},{Text=prefix},
      {Background={Color='#d7d7d7'}},{Foreground={Color=tab.is_active and '#5f5f5f' or '#5f5f00'}},
      {Text=' '..wezterm.truncate_right(title,math.max(0,budget))..suffix..' '},
      {Text=mark and (mark_text..' ') or ''}}
    title_cache[tab.tab_id or tab.tab_index]={key=cache_key,text=formatted}
    return formatted

  end)
end
return M
