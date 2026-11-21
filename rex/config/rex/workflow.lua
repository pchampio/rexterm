-- The GUI owns shortcuts and overlays; Rex owns sessions, PTYs and layouts.
local M = {}
function M.apply(config, wezterm, module_dir)
  module_dir=module_dir or wezterm.config_dir..'/rex'
  local act = wezterm.action
  local selector=dofile(module_dir..'/selector.lua')
  local status=dofile(module_dir..'/status.lua')
  wezterm.GLOBAL.rex_client_declarations={}
  local function callback(f) return wezterm.action_callback(f) end
  local function context(p) return p:rex_context() end
  local function call(p, method, args) return p:rex_call(method, args or {}) end
  local function perform(w,p,a) w:perform_action(a,p) end
  local function cwd(p)
    local url=p:get_current_working_dir()
    if url and url.file_path then local path=url.file_path;return path=='/' and path or path:gsub('/+$','') end
    local ok,process=pcall(call,p,'com.superlogical.terminal.process')
    if ok then return (process.foreground or process.child or {}).cwd end
  end
  local prefix={}
  local function bind(key,mods,action)
    if mods:find('LEADER',1,true) then
      local rest=mods:gsub('LEADER|?','');if rest=='' then rest='NONE' end
      prefix[#prefix+1]={key=key,mods=rest,action=action}
      if ('<>?&_'):find(key,1,true) and not rest:find('SHIFT',1,true) then
        prefix[#prefix+1]={key=key,mods=rest=='NONE' and 'SHIFT' or rest..'|SHIFT',action=action}
      end
      return
    end
    local original
    local aliases={Left='LeftArrow',Right='RightArrow',Up='UpArrow',Down='DownArrow'}
    for i=#(config.keys or {}),1,-1 do
      local b=config.keys[i]
      if (aliases[b.key] or b.key)==(aliases[key] or key) and b.mods==mods then original=b.action;table.remove(config.keys,i) end
    end
    local fallback=original or act.SendKey{key=key,mods=mods}
    if key=='n' and mods=='ALT' and not original then fallback=act.SpawnTab('CurrentPaneDomain') end
    local common=(mods=='ALT' and (key=='n' or key=='h' or key=='j' or key=='k' or key=='l' or key=='H' or key=='L')) or (mods=='CTRL' and (key=='a' or key=='h' or key=='j' or key=='k' or key=='l' or key:match('Arrow$')))
    local guarded=common and action or callback(function(w,p)
      perform(w,p,context(p) and action or fallback)
    end)
    table.insert(config.keys,{key=key,mods=mods,action=guarded})
    if ('<>?&_'):find(key,1,true) and not mods:find('SHIFT',1,true) then
      table.insert(config.keys,{key=key,mods=mods..'|SHIFT',action=guarded})
    end
  end
  config.keys=config.keys or {}
  -- Share the prefix across local and Rex panes so tabs and splits use the
  -- same keys before and after attachment.
  for i=#config.keys,1,-1 do
    local b=config.keys[i]
    if b.key=='Space' and b.mods=='CTRL' then table.remove(config.keys,i) end
  end
  config.keys[#config.keys+1]={key='Space',mods='CTRL',action=act.ActivateKeyTable{name='rex_prefix',one_shot=true,timeout_milliseconds=1500}}
  -- The Rex host resolves its own shell; use the normal interactive startup
  -- files once, and advertise the same terminal capabilities as WezTerm.
  config.rex_default_prog={'/bin/sh','-c','exec "${SHELL:-/bin/sh}" -l -i'}
  config.rex_environment={WEZTERM_REX='1',TERM='xterm-256color',COLORTERM='truecolor',TERM_PROGRAM='WezTerm'}
  config.copy_mode_selection_background='#d9d3bc'
  -- Rows that hold a command, while Ctrl-j/k (or Shift-J/K) walks the commands:
  -- Solarized base3 hue (45 degrees), a little lighter than the page (#fbf3db).
  -- Change this one value to taste.
  config.copy_mode_command_background='#fffbef'
  config.colors=config.colors or {}
  config.colors.copy_mode_active_highlight_bg={Color='#b58900'}
  config.colors.copy_mode_active_highlight_fg={Color='#002b36'}
  config.colors.copy_mode_inactive_highlight_bg={Color='#eee8d5'}
  config.colors.copy_mode_inactive_highlight_fg={Color='#586e75'}
  config.scrollback_lines=100000
  config.enable_kitty_keyboard=true
  config.enable_audio_escape=true
  config.rex_popup_styles={
    ['Scratch pad']={border_style='fg=colour001',title='#[align=centre,fg=colour0,bg=colour248] Scratch pad '},
    ['Python pad']={border_style='fg=colour001',title='#[align=centre,fg=colour0,bg=colour248] Python pad '},
    htop={border_style='fg=default',title='#[align=centre,fg=colour0,bg=colour2] htop '},
    nvtop={border_style='fg=default',title='#[align=centre,fg=colour0,bg=colour2] nvtop '},
    -- A tool command of a coding agent, opened from the agent panel. Only the
    -- border is used: the GUI writes the title (state, keys, command).
    ['Agent shell']={border_style='fg=colour004',title=''},
  }
  config.inactive_pane_hsb={saturation=1,brightness=1}
  local function rpc(method,args)
    return callback(function(w,p)
      local c=context(p);if not c then return end
      local payload=type(args)=='function' and args(c,p) or args or {}
      call(p,method,payload)
    end)
  end
  local function server_action(p,name,args)
    local c=context(p)
    return call(p,'client.action',{action=name,session_id=c.session_id,block_id=c.block_id,args=args}).result
  end
  local function buffer_set(p,text)
    wezterm.GLOBAL.rex_hint_buffer_active=false
    local token=server_action(p,'wezterm_buffer_begin').token
    local offset=1
    repeat
      local last=math.min(#text,offset+8191)
      while last<#text and text:byte(last+1)>=128 and text:byte(last+1)<192 do last=last-1 end
      server_action(p,'wezterm_buffer_append',{token=token,text=text:sub(offset,last),finish=last==#text})
      offset=last+1
    until offset>#text
  end
  local function buffer_get(p)
    local pieces,offset={},0
    while true do
      local part=server_action(p,'wezterm_buffer_read',{offset=offset})
      pieces[#pieces+1]=part.text;offset=offset+#part.text
      if part.eof then return table.concat(pieces) end
      assert(#part.text>0 and offset<=16*1024*1024,'Invalid session buffer')
    end
  end
  local flashes={}
  local function copy(desktop,leave)
    return callback(function(w,p)
      local text=w:get_selection_text_for_pane(p)
      if not text or text=='' then return end
      local c=context(p)
      if c and not desktop then
        local buffers=wezterm.GLOBAL.rex_buffers or {}
        buffers[c.buffer_key]=text;wezterm.GLOBAL.rex_buffers=buffers
        buffer_set(p,text)
      end
      if not c and not desktop then wezterm.GLOBAL.rex_local_buffer=text end
      if desktop then perform(w,p,act.CopyTo('ClipboardAndPrimarySelection')) end
      local id=w:window_id();local token={};flashes[id]=token
      perform(w,p,act.SetSelectionStyle{bold=true,underline=true,duration_milliseconds=250,
        foreground=desktop and '#366ad3' or '#d33682',
        background=(config.colors or {}).background or ((config.color_schemes or {})[config.color_scheme] or wezterm.color.get_builtin_schemes()[config.color_scheme or 'Builtin Solarized Light'] or {}).background or '#fbf3db'})
      wezterm.time.call_after(.25,function()
        if flashes[id]~=token then return end
        flashes[id]=nil
        -- The pane active now: copy mode may have closed in the meantime.
        local ok,now=pcall(function()return w:active_pane()end)
        perform(w,ok and now or p,leave and act.Multiple{act.CopyMode('Close'),act.ClearSelection,act.ScrollToBottom} or act.CopyMode('ClearSelectionMode'))
      end)
    end)
  end
  local function paste(w,p)
    local c=context(p);local buffers=wezterm.GLOBAL.rex_buffers or {}
    if wezterm.GLOBAL.rex_hint_buffer_active then p:paste(wezterm.GLOBAL.rex_local_buffer or '')
    elseif c then local text=buffer_get(p);if text then p:paste(text) end
    else p:paste(wezterm.GLOBAL.rex_local_buffer or '') end
  end
  local function popup(title,command,width,height)
    return callback(function(w,p)
      local c=context(p);if not c then p:spawn_popup{args=command,width=width,height=height,cwd=cwd(p)};return end
      local view=call(p,'session.view')
      for _,win in ipairs(view.windows or {}) do
        if win.window_id==c.window_id then
          for _,layer in ipairs(win.layers or {}) do
            for _,block in ipairs(layer.blocks or {}) do
              if block.label==title then
                call(p,'session.set_layer_state',{layer_id=layer.layer_id,state=layer.state=='minimized' and 'normal' or 'minimized'})
                if layer.state=='minimized' then call(p,'session.focus_block',{block_id=block.block_id}) end
                return
              end
            end
          end
        end
      end
      local options={cwd=cwd(p)}
      if command then options.command=command;options.shell='none';options.login=false end
      if title=='Python pad' or title=='nvtop' then
        local ok,result=pcall(call,p,'client.action',{action='wezterm_environment',session_id=c.session_id,block_id=c.block_id})
        local env=ok and result.result
        if env and env.prefix then options.command={env.prefix..'/bin/'..(title=='Python pad' and 'python3' or 'nvtop')} end
      end
      call(p,'session.new_layer',{window_id=c.window_id,bounds={(1-width)/2,(1-height)/2,width,height},focus=true,
        layout={block={flavor='com.superlogical.terminal.shell',label=title,options=options}}})
    end)
  end
  local scratch=popup('Scratch pad',nil,.65,.65)
  local python=popup('Python pad',{'python3'},.65,.65)
  local htop=popup('htop',{'htop','--sort-key=PERCENT_CPU'},.7,.5)
  local nvtop=popup('nvtop',{'nvtop'},.5,.5)
  local function hide_or_detach(w,p)
    local c=context(p)
    -- An agent shell: take the popup away, the command keeps running.
    if c and c.agent_shell then call(p,'wezterm.agent_shell_hide',{block_id=c.block_id});return end
    if not c then
      if p:is_popup_visible() then p:set_popup_visible(false) end
      return
    end
    local view=call(p,'session.view')
    for _,win in ipairs(view.windows or {}) do
      for _,layer in ipairs(win.layers or {}) do
        if layer.kind~='tiled' then
          for _,b in ipairs(layer.blocks or {}) do
            if b.block_id==c.block_id then call(p,'session.set_layer_state',{layer_id=layer.layer_id,state='minimized'});return end
          end
        end
      end
    end
    perform(w,p,act.DetachDomain('CurrentPaneDomain'))
  end
  local function switch_window(w,p)
    local choices={}
    for i,tab in ipairs(w:mux_window():tabs()) do choices[#choices+1]={id=tostring(i-1),label=i..'  '..tab:get_title()} end
    selector.open(wezterm,w,p,{title='jump-to-window',choices=choices,width=.3,height=.3},function(id)
      if id then perform(w,p,act.ActivateTab(tonumber(id))) end
    end)
  end
  local function rename(method,field,title)
    return callback(function(w,p)
      local c=context(p)
      perform(w,p,act.PromptInputLine{description=title,action=callback(function(_,pane,text)
        if text then local args={label=text};if field then args[field]=c[field] end;call(pane,method,args) end
      end)})
    end)
  end
  local extract=callback(function(w,p)
    selector.open(wezterm,w,p,{title='Find in pane — Enter: paste / Tab: buffer',capture=p:get_logical_lines_as_text(2000),
      extractor=module_dir..'/../../runtime-rex/bin/rex-extract',expect='tab,esc,ctrl-c',width=.9,height=.9},function(text,key)
      if text and text~='' then
        if context(p) then buffer_set(p,text) else wezterm.GLOBAL.rex_local_buffer=text end
        if key~='tab' then p:paste(text) end
      end
    end)
  end)
  local path_select=act.Multiple{
    act.Search{Regex=[[(?:[~./]|[A-Za-z0-9_-]+/)[^\s:]+(?::[0-9]+(?::[0-9]+)?)?|[A-Za-z0-9_.-]+:[0-9]+(?::[0-9]+)?]]},
    act.CopyMode('AcceptPattern'),
  }

  local function shell_layout(command,dir,label,remote,size)
    local argv={'/usr/bin/env'}
    if remote then argv[#argv+1]='PROMPT_PURE_SSH_CONNECTION=rex' end
    for key,value in pairs(config.rex_environment) do argv[#argv+1]=key..'='..value end
    for _,value in ipairs(command or config.rex_default_prog) do argv[#argv+1]=value end
    local options={cwd=dir,command=argv,shell='none',login=false,size=size}
    return {block={flavor='com.superlogical.terminal.shell',label=label,options=options}}
  end
  local function new_window(w,p)
    perform(w,p,act.SpawnTab('CurrentPaneDomain'))
  end
  local function split(direction)
    -- The current native domain owns creation, CWD inheritance and SSH context.
    return direction=='horizontal' and act.SplitHorizontal{domain='CurrentPaneDomain'}
      or act.SplitVertical{domain='CurrentPaneDomain'}
  end
  local function rotate(delta)
    return callback(function(_,p)
      local c=context(p);local view=call(p,'session.view')
      for _,win in ipairs(view.windows) do
        if win.window_id==c.window_id then
          local blocks=win.layers[1].blocks
          for i,b in ipairs(blocks) do
            if b.block_id==c.block_id and #blocks>1 then
              call(p,'session.swap_blocks',{block_id=c.block_id,other_block_id=blocks[(i-1+delta)%#blocks+1].block_id});return
            end
          end
        end
      end
    end)
  end
  local function move_window(delta)
    return callback(function(w,p)
      local c=context(p)
      if not c then perform(w,p,act.MoveTabRelative(delta));return end
      local view=call(p,'session.view')
      for i,win in ipairs(view.windows) do
        if win.window_id==c.window_id then
          local target=i+delta
          if target<1 or target>#view.windows then return end
          local before=view.windows[delta<0 and target or target+1]
          call(p,'session.move_window',{window_id=c.window_id,before_window_id=before and before.window_id});return
        end
      end
    end)
  end
  local break_pane=callback(function(_,p)
    local c=context(p)
    -- Rex creates windows with new blocks. A temporary shell is the move anchor;
    -- the original block, process, and scrollback survive the move unchanged.
    local created=call(p,'session.new_window',{layout=shell_layout(nil,cwd(p)),focus=false})
    local anchor=assert(created.block_ids[1])
    local ok,err=pcall(call,p,'session.move_block',{block_id=c.block_id,anchor_block_id=anchor,direction='horizontal',side='before',ratio=.5})
    if not ok then call(p,'session.close_window',{window_id=created.window_id});error(err) end
    call(p,'block.close',{block_id=anchor})
    call(p,'session.focus_block',{block_id=c.block_id})
  end)
  local join_pane=callback(function(w,p)
    local c=context(p);local view=call(p,'session.view');local choices={}
    for _,win in ipairs(view.windows) do
      if win.window_id~=c.window_id then
        for _,layer in ipairs(win.layers) do
          for _,block in ipairs(layer.blocks) do choices[#choices+1]={id=block.block_id,label=(win.label or 'Window')..' / '..(block.label or block.block_id)} end
        end
      end
    end
    perform(w,p,act.InputSelector{title='Join pane',fuzzy=true,choices=choices,action=callback(function(_,pane,id)
      if id then call(pane,'session.move_block',{block_id=id,anchor_block_id=c.block_id,direction='horizontal',side='after',ratio=.5});call(pane,'session.focus_block',{block_id=id}) end
    end)})
  end)
  local switch_session=callback(function(w,p)
    if not context(p) then status.message(wezterm,w,'Run tm or rex-attach in this shell to attach a session');return end
    local choices={}
    for _,s in ipairs(call(p,'session.list').sessions) do
      if s.session_id~=context(p).session_id and not (s.label or ''):find('ppt') and not (s.label or ''):find('scc') then choices[#choices+1]={id=s.session_id,label=s.label or s.session_id} end
    end
    selector.open(wezterm,w,p,{title='Switch session / type a new name',choices=choices,query=true,width=.3,height=.3},function(id)
      if id and id~='' then call(p,'wezterm.switch_session',{session=id}) end
    end)
  end)
  local capture=callback(function(w,p)
    perform(w,p,act.PromptInputLine{description='Save history on the Rex server',initial_value='/tmp/rex.history',action=callback(function(_,pane,path)
      if not path or path=='' then return end
      local c=context(pane);call(pane,'client.action',{action='wezterm_capture',session_id=c.session_id,block_id=c.block_id,args={path=path}})
    end)})
  end)
  local last_output=callback(function(_,p)
    local zones=p:get_semantic_zones('Output');local text=''
    for i=#zones,1,-1 do
      text=p:get_text_from_semantic_zone(zones[i]);if text:match('%S') then break end
    end
    if text=='' then return end
    call(p,'session.new_window',{window_label='last-command-output',focus=true,
      layout=shell_layout({'sh','-c','exec ${EDITOR:-nvim} - <<REX_LAST_OUTPUT\n$1\nREX_LAST_OUTPUT\n','rex-last-output',text},cwd(p))})
  end)
  -- Rust finds the command from the shell's OSC 133 input cells (wrapped and
  -- continuation rows included) and shades the commands on screen. The marker
  -- is for rows without OSC 133 marks: the command is the text after it.
  -- select=false moves to the start of the command without selecting it:
  -- Shift-Y selects the command at the cursor.
  local function command_jump(direction,enter,select)
    local jump=act.CopyMode{SelectCommand={direction=direction,prompt_marker='$~',select=select~=false}}
    return enter and act.Multiple{act.ActivateCopyMode,jump} or jump
  end
  local select_command=command_jump(0,true)
  -- Open FILE, FILE:LINE or FILE:LINE:COLUMN (or a file:// URL) in the
  -- editor on the Rex host: in the nvim of the window when it has one, else
  -- in a new split (current: in this pane).
  local function open_path(w,p,text,current)
      local file,line,column=text:match('^(.-):(%d+):(%d+)$')
      if not file then file,line=text:match('^(.-):(%d+)$') end
      file=file or text
      if file:match('^file://') then file=file:gsub('^file://localhost/','/'):gsub('^file:///','/'):gsub('%%(%x%x)',function(v)return string.char(tonumber(v,16))end) end
      if file:sub(1,2)=='~/' then
        local home=server_action(p,'wezterm_environment').home
        if home then file=home..file:sub(2) end
      end
      if file:sub(1,1)~='/' and file:sub(1,1)~='~' then file=(cwd(p) or '.')..'/'..file end
      local args={'sh','-c','exec ${EDITOR:-nvim} "$@"','rex-editor'}
      if line then args[#args+1]='+call cursor('..line..','..(column or '1')..')' end;args[#args+1]=file
      if current then
        perform(w,p,act.CopyMode('Close'));perform(w,p,act.SendKey{key='q',mods='CTRL'})
        p:paste(wezterm.shell_join_args(args));perform(w,p,act.SendKey{key='Enter'})
        return
      end
      local c=context(p);local view=call(p,'session.view')
      for _,win in ipairs(view.windows) do if win.window_id==c.window_id then
        for _,block in ipairs(win.layers[1].blocks) do
          local process=call(p,'com.superlogical.terminal.process',{block_id=block.block_id})
          local name=(process.foreground or process.child or {}).name
          if name=='nvim' or name=='vim' then
            local command=":execute 'vsplit ' . fnameescape(expand("..wezterm.json_encode(file).."))"
            if line then command=command..' | call cursor('..line..','..(column or '1')..')' end
            call(p,'com.superlogical.terminal.write',{block_id=block.block_id,args={data='\27'..command..'\r'}})
            call(p,'session.focus_block',{block_id=block.block_id});return
          end
        end
      end end
      call(p,'session.new_split',{anchor_id=c.block_id,direction='horizontal',side='after',ratio=.5,
        layout=shell_layout(args,cwd(p),'Editor'),focus=true})
  end
  local function open_selection(current)
    return callback(function(w,p)
      local text=w:get_selection_text_for_pane(p);if not text or text=='' then return end
      open_path(w,p,text,current)
    end)
  end
  -- Links of a Rex pane to a file (Codex: file_opener "vscode" makes
  -- vscode://file/PATH:LINE; file:// URLs) are files of the Rex host: they
  -- open in its editor, not on this desktop. Other links open as usual.
  local function file_link(uri)
    local path=uri:match('^vscode%-insiders://file(/.*)$') or uri:match('^vscode://file(/.*)$')
      or uri:match('^cursor://file(/.*)$') or uri:match('^windsurf://file(/.*)$')
    if path then return (path:gsub('%%(%x%x)',function(v)return string.char(tonumber(v,16))end)) end
    if uri:match('^file://') then
      local file,line=uri:match('^(.-)#L(%d+)')
      return file and file..':'..line or uri
    end
  end
  wezterm.on('open-uri',function(w,p,uri)
    local path=file_link(uri)
    if not path or not context(p) then return end
    local ok,error=pcall(open_path,w,p,path,false)
    if not ok then status.message(wezterm,w,'Cannot open '..path..': '..tostring(error)) end
    return false
  end)
  local readline=callback(function(w,p)
    local entries={{'t','ALT','Transpose words'}, {},{'u','CTRL','Cut to start of line'},{'d','ALT','Cut next word'},
      {'w','CTRL','Cut previous word'},{'y','CTRL','Yank kill ring'},{'y','ALT','Rotate kill ring'}, {},{'.','ALT','Previous argument'}}
    local items={}
    for _,e in ipairs(entries) do items[#items+1]={label=e[3] or '',key=e[1],mods=e[2] or 'NONE',action=e[1] and act.SendKey{key=e[1],mods=e[2]}} end
    perform(w,p,act.PopupMenu{title='Readline Bindings',items=items})
  end)
  local command_prompt=callback(function(w,p)
    perform(w,p,act.PromptInputLine{description='Rex command',bottom=true,prompt=':',completion={'rename-session ','rename-window ','new-window','select-window ','detach-client','kill-pane','kill-window','kill-tab','kill-session','kill-server'},action=callback(function(win,pane,text)
      if not text then return end
      local command,arg=text:match('^(%S+)%s*(.-)%s*$')
      -- Without a name, rename-* and select-window ask, like their prefix keys.
      if (command=='renamew' or command=='rename-window') and arg=='' then perform(win,pane,rename('session.set_window_label','window_id','Rename window'))
      elseif (command=='rename' or command=='rename-session') and arg=='' then perform(win,pane,rename('session.set_label',nil,'Rename session'))
      elseif command=='select-window' and arg=='' then switch_window(win,pane)
      elseif command=='renamew' or command=='rename-window' then
        local c=context(pane);if c then call(pane,'session.set_window_label',{window_id=c.window_id,label=arg}) else win:active_tab():set_title(arg) end
      elseif command=='rename' or command=='rename-session' then call(pane,'session.set_label',{label=arg})
      elseif command=='new-window' then new_window(win,pane)
      elseif command=='select-window' then perform(win,pane,act.ActivateTab((tonumber(arg) or 1)-1))
      elseif command=='detach' or command=='detach-client' then hide_or_detach(win,pane)
      -- tmux names. kill-window and kill-tab close the Rex window (a GUI tab).
      elseif command=='kill-pane' or command=='killp' then perform(win,pane,act.CloseCurrentPane{confirm=false})
      elseif command=='kill-window' or command=='killw' or command=='kill-tab' then
        local c=context(pane)
        if c then call(pane,'session.close_window',{window_id=c.window_id}) else perform(win,pane,act.CloseCurrentTab{confirm=false}) end
      elseif command=='kill-session' then
        local c=context(pane);if not c then error('kill-session needs a Rex pane') end
        call(pane,'session.destroy',{session_id=c.session_id})
      elseif command=='kill-server' then
        local c=context(pane);if not c then error('kill-server needs a Rex pane') end
        -- Every session goes, so the next start does not restore them.
        server_action(pane,'wezterm_kill_server');perform(win,pane,act.DetachDomain('CurrentPaneDomain'))
      else error('Unknown Rex workflow command: '..tostring(command)) end
    end)})
  end)

  local actions={
    p=python,g=path_select,h=htop,n=nvtop,M=rpc("wezterm.overview"),L=callback(switch_window),d=callback(hide_or_detach),
    f=extract,F=act.QuickSelectArgs{action=callback(function(w,p)
      local text=w:get_selection_text_for_pane(p)
      if text and text~='' then
        wezterm.GLOBAL.rex_local_buffer=text
        wezterm.GLOBAL.rex_hint_buffer_active=true
      end
    end)},r=rename('session.set_label',nil,'Rename session'),
    ['_']=break_pane,j=join_pane,t=switch_session,P=capture,B=last_output,k=act.CloseCurrentPane{confirm=false},K=rpc('session.close_window',function(c)return{window_id=c.window_id}end),q=act.Nop,
    b=command_jump(-1,true),
    ['<']=rotate(-1),['>']=rotate(1),
    m=callback(function(w,p)local c=context(p) or {pane_id=p:pane_id()};c.pane_id=p:pane_id();local mark=wezterm.GLOBAL.rex_mark;if mark and mark.pane_id==c.pane_id then wezterm.GLOBAL.rex_mark=nil;w:set_marked_pane(nil) else wezterm.GLOBAL.rex_mark=c;w:set_marked_pane(c.pane_id) end end),
    s=callback(function(_,p)local c=context(p);local mark=wezterm.GLOBAL.rex_mark;if mark and mark.session_id==c.session_id then call(p,'session.swap_blocks',{block_id=c.block_id,other_block_id=mark.block_id}) end end),
  }
  actions.c=callback(function(_,p)
    local c=context(p);local env=call(p,'client.action',{action='wezterm_environment',session_id=c.session_id,block_id=c.block_id}).result
    assert(env and env.root,'No env.sh found for this pane')
    p:paste(' __cd '..wezterm.shell_join_args{env.root}..'; source ./env.sh; __cd -')
    call(p,'block.event',{block_id=c.block_id,event={type='key',payload={key='enter',mods='',action='press'}}})
  end)
  actions.S=callback(function(_,p)
    local c=context(p);local mark=wezterm.GLOBAL.rex_mark
    if not mark or mark.session_id~=c.session_id or mark.window_id==c.window_id then return end
    local windows=call(p,'session.view').windows;local a,b
    for i,win in ipairs(windows) do if win.window_id==c.window_id then a=i elseif win.window_id==mark.window_id then b=i end end
    if not a or not b then return end
    windows[a],windows[b]=windows[b],windows[a]
    local before
    for i=#windows,1,-1 do call(p,'session.move_window',{window_id=windows[i].window_id,before_window_id=before});before=windows[i].window_id end
  end)
  local menu
  do
    local entries={{'p','Python'},{'c','env activate'}, {},
      {'n','Nvtop'},{'h','Htop'}, {},
      {'M','Windows mosaic'},{'t','Switch client'},{'L','Switch window'},{'_','Break pane'},{'j','Join Pane'},{'r','Rename session'},{'d','Detach client'}, {},
      {'f','Find in pane extrakto/fzf'},{'F','Find in pane thumbs/hints'},{'g','Find path:line in pane'},{'b','Select previous command'},{'B','Open last output in nvim'},{'P','Capture pane'}, {},
      {'<','Swap Pane Up'},{'>','Swap Pane Down'},{'m','Mark'},{'s','Swap Marked pane'},{'S','Swap Marked window'}, {},
      {'k','Kill Pane'},{'K','Kill Window'}, {},{'q','Close menu'}}
    menu=callback(function(w,p)
      local c=context(p);local mark=wezterm.GLOBAL.rex_mark;local items={}
      -- A row without an action is drawn dim and cannot be chosen. Dim every
      -- row that can do nothing in this pane.
      local ok,tab=pcall(function()return w:active_tab()end)
      local panes=ok and tab and #tab:panes() or 1
      local ok_tabs,tabs=pcall(function()return #w:mux_window():tabs()end)
      tabs=ok_tabs and tabs or 1
      local marked=c and mark and mark.session_id==c.session_id
      local unavailable={c=not c,M=not c,r=not c,m=not c,t=not c,B=not c,P=not c,
        ['_']=not c or panes<2,['<']=not c or panes<2,['>']=not c or panes<2,j=not c or tabs<2,
        s=not marked or mark.block_id==c.block_id,S=not marked or mark.window_id==c.window_id,
        d=not c and not p:is_popup_visible()}
      for _,e in ipairs(entries) do
        local key,label=e[1],e[2] or ''
        local action=key and actions[key]
        if key=='m' and c and mark and mark.block_id==c.block_id then label='Unmark' end
        if key and unavailable[key] then action=nil end
        if key=='K' and not c then action=act.CloseCurrentTab{confirm=false} end
        items[#items+1]={label=label,key=key,action=action}
      end
      local title=wezterm.hostname and wezterm.hostname():match('^[^.]+') or 'Commands'
      perform(w,p,act.PopupMenu{title=title,items=items})
    end)
  end
  local function foreground(p)
    if not context(p) then return p:get_foreground_process_name() or '' end
    local vars=p:get_user_vars()
    if vars.WEZTERM_REX_PROCESS then
      local ok,process=pcall(wezterm.json_parse,vars.WEZTERM_REX_PROCESS)
      if ok and type(process)=='table' then return process.script or process.name or process.path or '' end
    end
    local ok,process=pcall(call,p,'com.superlogical.terminal.process')
    if not ok then return '' end
    local child=process.foreground or process.child or {}
    return child.script or child.name or child.path or ''
  end

  local applications={nvim=true,neovim=true,vim=true,view=true,diff=true,fzf=true,atuin=true,proxget=true}
  -- Shell commands that open a picker (zjump runs fzf). The shell reports
  -- its running command line as WEZTERM_PROG (zsh preexec), also over SSH.
  local pickers={j=true,zjump=true,zi=true,sk=true,['fzf-tmux']=true,fzf=true}
  local function running_picker(vars)
    if vars.WEZTERM_AT_PROMPT=='1' then return false end
    local first=(vars.WEZTERM_PROG or ''):match('^%s*(%S+)')
    first=first and first:match('([^/]+)$')
    return first~=nil and (pickers[first] or applications[first])==true
  end
  local shells={sh=true,bash=true,zsh=true,fish=true,dash=true}
  local function has_interactive_child(process)
    if not process then return false end
    if applications[(process.name or ''):match('([^/]+)$')] then return true end
    for _,child in pairs(process.children or {}) do
      if has_interactive_child(child) then return true end
    end
    return false
  end
  -- action_callback assigns numeric event IDs in registration order. Keep that
  -- order stable across WezTerm's independent configuration/Lua contexts.
  for _,binding in ipairs{{'h','Left'},{'j','Down'},{'k','Up'},{'l','Right'}} do
    local key,direction=table.unpack(binding)
    bind(key,'CTRL',callback(function(w,p)
      if p:is_popup_visible() then perform(w,p,act.SendKey{key=key,mods='CTRL'});return end
      local name=foreground(p):match('([^/]+)$') or ''
      local picker=shells[name] and not context(p) and p.get_foreground_process_info and has_interactive_child(p:get_foreground_process_info())
      if shells[name] and context(p) then
        local ok,names=pcall(wezterm.json_parse,p:get_user_vars().WEZTERM_REX_INPUT_PROCESSES or '[]')
        if ok and type(names)=='table' then
          for _,child in ipairs(names) do if applications[child] then picker=true;break end end
        end
      end
      -- WEZTERM_PICKER: an fzf that a zsh key binding started (Ctrl-R), see
      -- the fzf function of the zsh configuration.
      local vars=p:get_user_vars()
      if picker or (vars.WEZTERM_PICKER or '')~='' or running_picker(vars) or (vars.NVIM_MODE or '')~='' or (name~='' and applications[name]) then perform(w,p,act.SendKey{key=key,mods='CTRL'})
      else perform(w,p,act.ActivatePaneDirection(direction)) end
    end))
  end
  for key,delta in pairs{h=-1,l=1,H=-1,L=1} do bind(key,'ALT',act.ActivateTabRelative(delta)) end
  bind('n','ALT',act.SpawnTab('CurrentPaneDomain'))
  for key,direction in pairs{LeftArrow='Left',RightArrow='Right',UpArrow='Up',DownArrow='Down'} do
    bind(key,'CTRL',act.AdjustPaneSize{direction,(direction=='Left' or direction=='Right') and 10 or 5})
  end
  bind('g','ALT',scratch);bind('p','ALT',python)
  bind('j','ALT',callback(function(w,p)
    local name=foreground(p);if name:find('opencode') or name:find('nvim') or name:find('fzf') or name:find('claude') or (p:get_user_vars().WEZTERM_PICKER or '')~='' then perform(w,p,act.SendKey{key='j',mods='ALT'})
    else perform(w,p,act.Multiple{act.ActivateCopyMode,act.CopyMode{ScrollByLine=2}}) end
  end))
  bind('k','ALT',callback(function(w,p)
    local name=foreground(p);if name:find('opencode') or name:find('nvim') or name:find('fzf') or name:find('claude') or (p:get_user_vars().WEZTERM_PICKER or '')~='' then perform(w,p,act.SendKey{key='k',mods='ALT'})
    else perform(w,p,act.Multiple{act.ActivateCopyMode,act.CopyMode{ScrollByLine=-2}}) end
  end))
  bind('b','CTRL',callback(function(w,p)perform(w,p,foreground(p):find('opencode') and act.SendKey{key='LeftArrow',mods='ALT'} or act.SendKey{key='b',mods='CTRL'})end))
  bind('w','ALT',callback(function(w,p)perform(w,p,foreground(p):find('opencode') and act.Multiple{act.SendKey{key='RightArrow',mods='ALT'},act.SendKey{key='LeftArrow',mods='ALT'},act.SendKey{key='w',mods='CTRL'}} or act.SendKey{key='w',mods='ALT'})end))
  bind('V','CTRL|SHIFT',callback(function(w,p)dofile(module_dir..'/clipboard.lua').paste(wezterm,w,p)end))
  bind('c','LEADER',act.SpawnTab('CurrentPaneDomain'))
  bind('s','LEADER',split('horizontal'))
  bind('i','LEADER',split('vertical'))
  bind('v','LEADER',act.ActivateCopyMode)
  bind('k','LEADER',act.Multiple{act.ActivateCopyMode,act.CopyMode('MoveUp')})
  -- prefix K goes to the previous command (as tmux did); Shift-Y selects it.
  bind('K','LEADER',command_jump(-1,true,false))
  bind('Space','LEADER',callback(function(w,p)
    local c=context(p);if c then call(p,'session.zoom_block',{block_id=c.block_id}) else perform(w,p,act.TogglePaneZoomState) end
  end))
  bind('a','CTRL',act.SendKey{key='a',mods='CTRL'});bind('a','LEADER',menu);bind('a','LEADER|CTRL',menu);bind('L','LEADER',callback(switch_window))
  bind('d','LEADER',callback(hide_or_detach))
  bind('m','LEADER',callback(function(_,p)
    local enabled=call(p,'wezterm.synchronize');local all=wezterm.GLOBAL.rex_sync or {};all[context(p).window_id]=enabled;wezterm.GLOBAL.rex_sync=all
  end))
  bind('D','LEADER',callback(function(_,p)
    local c=context(p);local clients=call(p,'client.list',{session_id=c.session_id}).clients or {}
    for _,client in ipairs(clients) do
      if client.client_id~=c.client_id and (client.info or {}).name=='WezTerm Rex' then
        call(p,'client.perform',{client_id=client.client_id,session_id=c.session_id,action='wezterm.detach'})
      end
    end
  end))
  wezterm.on('update-status',function(_,p)
    local ok,c=pcall(context,p);if not ok or not c then return end
    local declared=wezterm.GLOBAL.rex_client_declarations or {}
    if not declared[c.client_id] then
      declared[c.client_id]=true;wezterm.GLOBAL.rex_client_declarations=declared
      local ok=pcall(function()
        call(p,'client.keymap',{actions={{name='wezterm.detach',title='Detach this GUI',kind='client'}}})
      end)
      if not ok then declared[c.client_id]=nil;wezterm.GLOBAL.rex_client_declarations=declared end
    end
  end)
  wezterm.on('user-var-changed',function(w,p,name,value)
    if name~='WEZTERM_REX_CLIENT_REQUEST' then return end
    local request=wezterm.json_parse(value);local c=context(p)
    if request.action=='wezterm.detach' and (not request.session_id or request.session_id==c.session_id) then
      call(p,'client.reply',{call_id=request.call_id,result={}})
      perform(w,p,act.DetachDomain('CurrentPaneDomain'))
    else call(p,'client.reply',{call_id=request.call_id,error={code='not_supported',message='Unknown action or session'}}) end
  end)

  bind(',','LEADER',rename('session.set_window_label','window_id','Rename window'))
  bind('t','LEADER|CTRL',callback(function(w,p) status.message(wezterm,w,'Configuration reloaded');perform(w,p,act.ReloadConfiguration) end))
  for _,binding in ipairs{{'s','wezterm_save'},{'r','wezterm_restore'}} do
    local key,name=table.unpack(binding)
    bind(key,'LEADER|CTRL',callback(function(w,p)
      local c=context(p);call(p,'client.action',{action=name,session_id=c.session_id,block_id=c.block_id})
      status.message(wezterm,w,name=='wezterm_save' and 'Session saved' or 'Session restored')
    end))
  end
  bind('/','LEADER',act.Multiple{act.ActivateCopyMode,act.CopyMode('SearchBackward')})
  bind('?','LEADER',act.Multiple{act.ActivateCopyMode,act.CopyMode('SearchAgain')})
  bind('F','LEADER',actions.F);bind('F','LEADER|SHIFT',actions.F)
  bind('f','LEADER',extract);bind('f','CTRL',callback(function(w,p)
    local name=foreground(p):match('([^/]+)$') or '';local vars=p:get_user_vars()
    if applications[name] and (vars.nvimmode=='n' or vars.NVIM_MODE=='n') then perform(w,p,act.SendKey{key='f',mods='CTRL'}) else perform(w,p,extract) end
  end))
  bind('/','ALT',readline);bind(';','LEADER',command_prompt)
  bind('j','LEADER|CTRL',command_jump(-1,true,false));bind('k','LEADER|CTRL',command_jump(-1,true,false))
  bind('B','LEADER',last_output);bind('P','LEADER',capture);bind('Y','LEADER',select_command);bind('g','LEADER',path_select);bind('_','LEADER',break_pane)
  bind(']','LEADER',callback(paste))
  bind('.','LEADER',callback(function(w,p)perform(w,p,act.CopyTextTo{text=cwd(p) or '',destination='ClipboardAndPrimarySelection'})end))
  -- prefix x: in an agent shell popup, stop the command (no question; a
  -- finished command cannot be stopped). Elsewhere, close the pane.
  bind('x','LEADER',callback(function(w,p)
    local c=context(p)
    if c and c.agent_shell then
      local ok,list=pcall(call,p,'wezterm.agent_shells')
      for _,shell in ipairs(ok and type(list)=='table' and list or {}) do
        if shell.block_id==c.block_id then
          if shell.running~=false then call(p,'block.close',{block_id=c.block_id}) end
          return
        end
      end
      return
    end
    perform(w,p,act.CloseCurrentPane{confirm=true})
  end))
  -- prefix l: the agent panel (status.lua), to use with the keyboard
  -- (prefix L: the window switcher).
  bind('l','LEADER',act.EmitEvent('rex-agent-panel'))
  bind('&','LEADER',rpc('session.close_window',function(c)return{window_id=c.window_id}end))
  bind('<','LEADER',move_window(-1));bind('>','LEADER',move_window(1))
  bind('<','LEADER|CTRL',move_window(-1));bind('>','LEADER|CTRL',move_window(1))
  for i=1,9 do bind(tostring(i),'LEADER',act.ActivateTab(i-1)) end
  local tables=config.key_tables or {}
  -- Native selectors must receive navigation before the shell/pane bindings.
  tables.selector_mode=tables.selector_mode or {
    {key='j',mods='CTRL',action=act.SendKey{key='DownArrow',mods='NONE'}},
    {key='k',mods='CTRL',action=act.SendKey{key='UpArrow',mods='NONE'}},
    {key='n',mods='CTRL',action=act.SendKey{key='DownArrow',mods='NONE'}},
    {key='p',mods='CTRL',action=act.SendKey{key='UpArrow',mods='NONE'}},
  }
  prefix[#prefix+1]={key="Escape",mods="NONE",action=act.PopKeyTable}
  tables.rex_prefix=prefix
  local copy_table=wezterm.gui.default_key_tables().copy_mode
  local function cb(key,mods,action)
    for i=#copy_table,1,-1 do if copy_table[i].key==key and copy_table[i].mods==mods then table.remove(copy_table,i)end end
    copy_table[#copy_table+1]={key=key,mods=mods,action=action}
  end
  cb('c','NONE',copy(false,false));cb('C','SHIFT',copy(true,false));cb('y','NONE',copy(false,true))
  cb('v','NONE',act.CopyMode{SetSelectionMode='Cell'});cb('V','SHIFT',act.CopyMode{SetSelectionMode='Line'})
  cb('v','CTRL',act.CopyMode{SetSelectionMode='Block'});cb('Space','NONE',act.CopyMode{SetSelectionMode='Cell'})
  cb('q','NONE',act.Multiple{act.CopyMode('Close'),act.ClearSelection,act.ScrollToBottom});cb('Escape','NONE',act.Multiple{act.CopyMode('Close'),act.ClearSelection,act.ScrollToBottom})
  cb('j','CTRL',command_jump(1,false,false));cb('k','CTRL',command_jump(-1,false,false))
  -- Shift-J/K do what Ctrl-j/k do (move to a command or, in visual mode, extend by command).
  cb('K','SHIFT',command_jump(-1,false,false));cb('J','SHIFT',command_jump(1,false,false))
  cb('Y','SHIFT',callback(function(w,p) local text=w:get_selection_text_for_pane(p);perform(w,p,text and text~='' and copy(true,true) or select_command) end));cb('o','NONE',open_selection(false));cb('O','SHIFT',open_selection(true))
  cb('s','NONE',callback(function(w,p)local text=w:get_selection_text_for_pane(p);if text and text~='' then wezterm.open_with('https://www.google.com/search?q='..text:gsub('[^%w%-_%.~]',function(c)return string.format('%%%02X',c:byte())end)) end end))
  cb('H','SHIFT',act.CopyMode('MoveToStartOfLine'));cb('L','SHIFT',act.CopyMode('MoveToEndOfLineContent'))
  cb('j','ALT',act.CopyMode{ScrollByLine=2});cb('k','ALT',act.CopyMode{ScrollByLine=-2})
  cb('/','NONE',act.CopyMode('SearchBackward'));cb('?','SHIFT',act.CopyMode('SearchForward'));cb('?','NONE',act.CopyMode('SearchForward'))
  cb('n','NONE',act.CopyMode('SearchAgain'));cb('N','SHIFT',act.CopyMode('SearchReverse'))
  -- f/F and t/T as vim-sneak in nvim: f jumps to one character, t to two
  -- characters with labels on the other matches of the screen, both over
  -- several lines; the matches show, and f/t again go to the next one.
  -- : (and ;) repeats the last jump, , goes back.
  cb('t','NONE',act.CopyMode{Sneak={forward=true}})
  cb('T','SHIFT',act.CopyMode{Sneak={forward=false}});cb('T','NONE',act.CopyMode{Sneak={forward=false}})
  cb(':','SHIFT',act.CopyMode('JumpAgain'));cb(':','NONE',act.CopyMode('JumpAgain'))
  -- Ctrl-e/Ctrl-y: scroll one line, the cursor stays on its text (as in vim).
  cb('e','CTRL',act.CopyMode{ScrollViewport=1});cb('y','CTRL',act.CopyMode{ScrollViewport=-1})
  -- w b e and W B E as in vim (WORDs are runs of non-blank characters),
  -- over several lines; in visual mode they extend the selection.
  for _,m in ipairs{'w','b','e'} do
    cb(m,'NONE',act.CopyMode{VimWord={motion=m,big=false}})
    cb(m:upper(),'SHIFT',act.CopyMode{VimWord={motion=m,big=true}});cb(m:upper(),'NONE',act.CopyMode{VimWord={motion=m,big=true}})
  end
  tables.search_mode=wezterm.gui.default_key_tables().search_mode
  for _,key in ipairs{'a','e','w','b','f','h','k'} do
    tables.search_mode[#tables.search_mode+1]={key=key,mods='CTRL',action=act.SendKey{key=key,mods='CTRL'}}
  end
  -- Up/Down: the previous searches (newest first), as in a shell.
  for i=#tables.search_mode,1,-1 do
    local k=tables.search_mode[i]
    if (k.key=='UpArrow' or k.key=='DownArrow') and (k.mods or 'NONE')=='NONE' then table.remove(tables.search_mode,i) end
  end
  tables.search_mode[#tables.search_mode+1]={key='UpArrow',mods='NONE',action=act.CopyMode{SearchHistory=-1}}
  tables.search_mode[#tables.search_mode+1]={key='DownArrow',mods='NONE',action=act.CopyMode{SearchHistory=1}}
  tables.search_mode[#tables.search_mode+1]={key='Enter',mods='NONE',action=act.CopyMode('AcceptPattern')}
  tables.search_mode[#tables.search_mode+1]={key='Escape',mods='NONE',action=act.CopyMode('AcceptPattern')}
  tables.copy_mode=copy_table
  tables.rex_passthrough={{key='F12',mods='NONE',action=act.PopKeyTable}}
  config.key_tables=tables
  bind('F12','NONE',act.Multiple{act.CopyMode('Close'),act.ClearSelection,act.ActivateKeyTable{name='rex_passthrough',one_shot=false,until_unknown=false,replace_current=true,passthrough=true}})
  config.bypass_mouse_reporting_modifiers='SHIFT'
  config.quick_select_remove_styling=true
  config.mouse_bindings=config.mouse_bindings or {}
  local function mouse(event,mods,action)
    local function same(a,b)
      if type(a)~=type(b) then return false end
      if type(a)~='table' then return a==b end
      for k,v in pairs(a) do if not same(v,b[k]) then return false end end
      for k in pairs(b) do if a[k]==nil then return false end end
      return true
    end
    for i=#config.mouse_bindings,1,-1 do
      local binding=config.mouse_bindings[i]
      if (binding.mods or 'NONE')==mods and same(binding.event,event) then
        table.remove(config.mouse_bindings,i)
      end
    end
    table.insert(config.mouse_bindings,{event=event,mods=mods,action=action})
  end
  for _,wheel in ipairs{{'WheelUp',-3},{'WheelDown',3}} do
    mouse({Down={streak=1,button={[wheel[1]]=1}}},'NONE',callback(function(w,p)
      if not w:copy_mode_state(p) and p:is_alt_screen_active() then
        for _=1,3 do perform(w,p,act.SendKey{key=wheel[2]<0 and 'UpArrow' or 'DownArrow'}) end
      elseif wheel[2]<0 or w:copy_mode_state(p) then
        perform(w,p,act.Multiple{act.ActivateCopyMode,act.CopyMode{ScrollByLine=wheel[2]}})
      end
    end),act.ScrollByLine(wheel[2]))
  end
  -- A plain drag becomes native copy mode, preserving its exact anchor. The
  -- GUI's existing drag/autoscroll engine supplies the selection coordinates.
  local click_copy=act.QuickSelectArgs{label='click-copy',patterns={'jksfldjjkfdsljflflsdkfjlsdjfdsfjlsdjflksdjklf'}}
  local drag_copy=callback(function(w,p)
    local range=w:selection_state(p)
    if not range then
      -- A program that takes the mouse (htop, nvim) gets plain clicks; WezTerm
      -- drops Shift from a Shift-click before it looks for a binding, so a
      -- Shift-click without a drag lands here: start click copy.
      local ok,grabbed=pcall(function()return p:is_mouse_grabbed()end)
      if ok and grabbed then perform(w,p,click_copy) end
      return
    end
    perform(w,p,act.ActivateCopyMode)
    perform(w,p,act.Multiple{act.CopyMode('ClearSelectionMode'),act.CopyMode{MoveToCoordinates=range.start},
      act.CopyMode{SetSelectionMode=range.block and 'Block' or 'Cell'},act.CopyMode{MoveToCoordinates=range['end']}})
  end)
  mouse({Up={streak=1,button='Left'}},'NONE',drag_copy,act.CompleteSelectionOrOpenLinkAtMouseCursor('PrimarySelection'))
  mouse({Down={streak=1,button='Left'}},'SHIFT',act.SelectTextAtMouseCursor('Cell'),act.SelectTextAtMouseCursor('Cell'))
  mouse({Drag={streak=1,button='Left'}},'SHIFT',act.ExtendSelectionToMouseCursor('Cell'),act.ExtendSelectionToMouseCursor('Cell'))
  mouse({Up={streak=1,button='Left'}},'SHIFT',callback(function(w,p)
    local text=w:get_selection_text_for_pane(p)
    if text and text~='' then perform(w,p,act.CopyTo('ClipboardAndPrimarySelection'))
    else perform(w,p,click_copy) end
  end),act.CompleteSelection('PrimarySelection'))
  -- Ctrl-click opens the link under the mouse (a URL of the text or a
  -- hyperlink), also in a program that takes the mouse (Codex): the press
  -- does not reach that program.
  for i=#config.mouse_bindings,1,-1 do
    local b=config.mouse_bindings[i];local button=b.event and (b.event.Up or b.event.Down)
    if b.mods=='CTRL' and button and button.button=='Left' and button.streak==1 then table.remove(config.mouse_bindings,i) end
  end
  for _,reporting in ipairs{false,true} do
    table.insert(config.mouse_bindings,{event={Down={streak=1,button='Left'}},mods='CTRL',mouse_reporting=reporting,action=act.Nop})
    table.insert(config.mouse_bindings,{event={Up={streak=1,button='Left'}},mods='CTRL',mouse_reporting=reporting,action=act.OpenLinkAtMouseCursor})
  end
  -- Ctrl-Shift-x: the URLs of the screen get labels, and so do the
  -- hyperlinks (Codex shows a name and keeps the URL in the link). A label
  -- opens its URL (a file link of a Rex pane opens in the editor of the host).
  for i=#config.keys,1,-1 do
    local b=config.keys[i]
    if (b.key=='x' or b.key=='X') and (b.mods=='CTRL|SHIFT' or b.mods=='SHIFT|CTRL') then table.remove(config.keys,i) end
  end
  table.insert(config.keys,{key='x',mods='CTRL|SHIFT',action=act.QuickSelectArgs{label='open url',hyperlinks=true,
    patterns={[=[https?://[^\s<>"'`()\[\]{}\\]*[^\s<>"'`()\[\]{}\\.,;:!?]]=]},
    action=callback(function(w,p)
      local url=(w:get_selection_text_for_pane(p) or ''):gsub('[.,;:!?]+$','')
      if url~='' then wezterm.open_with(url) end
    end)}})
end
return M
