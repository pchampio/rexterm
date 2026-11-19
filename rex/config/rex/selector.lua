-- Real fzf, in a native floating terminal. Only opaque row indices cross back
-- to Lua; choice text is never evaluated as shell code.
local M={}
function M.open(wezterm,w,p,opts,done)
  local file=os.tmpname()
  local input=assert(io.open(file,'w'))
  if opts.capture then input:write(opts.capture)
  else for i,c in ipairs(opts.choices) do input:write(i,'\t',c.label:gsub('[\r\n\t]',' '),'\n') end end
  input:close()
  local output=file..'.out';local finished=file..'.done'
  local args={'fzf','--reverse','--bind=ctrl-j:down,ctrl-k:up','--header='..opts.title,'--expect='..(opts.expect or 'esc,ctrl-c')}
  if opts.menu then
    args[#args+1]='--disabled'
    local bindings={}
    for _,c in ipairs(opts.choices) do bindings[#bindings+1]=c.id..':accept' end
    args[#args+1]='--expect='..table.concat((function()local t={'esc','ctrl-c'};for _,c in ipairs(opts.choices)do t[#t+1]=c.id end;return t end)(),',')
  end
  if opts.query then args[#args+1]='--print-query' end
  if opts.capture then args[#args+1]='--multi';args[#args+1]='--tiebreak=index';args[#args+1]='--no-info'
  else args[#args+1]='--with-nth=2..';args[#args+1]='--delimiter=\t' end
  local command='umask 077; '..wezterm.shell_join_args(args)..' < "$1" > "$2"; printf done > "$3"'
  if opts.capture then
    command='umask 077; '..wezterm.shell_join_args{opts.extractor,'--dump'}..' < "$1" | tac | '..wezterm.shell_join_args(args)..' > "$2"; printf done > "$3"'
  end
  local popup=p:spawn_popup{domain={DomainName='local'},args={'sh','-c',command,'rex-fzf',file,output,finished},width=opts.width or .65,height=opts.height or .65,cwd=wezterm.home_dir}
  local function clean()for _,path in ipairs{file,output,finished}do os.remove(path)end end
  local function poll()
    local f=io.open(finished,'r')
    if f then
      f:close();local result=io.open(output,'r');local text=result and result:read('*a') or '';if result then result:close()end
      clean()
      local lines={};for line in text:gmatch('(.-)\n')do lines[#lines+1]=line end
      local query=opts.query and table.remove(lines,1) or nil
      local key=table.remove(lines,1)
      if key and key~='esc' and key~='ctrl-c' then
        -- The PTY may still exist for one frame. Hide it before invoking the
        -- continuation, so password/command input always targets its origin.
        pcall(function()popup:set_popup_visible(false)end)
        if opts.capture then done(table.concat(lines,' '),key)
        elseif opts.menu and key~='' then done(key,key)
        else local index=tonumber((lines[1] or ''):match('^(%d+)\t'));done(index and opts.choices[index].id or query,key) end
      end
      return
    end
    local ok,live=pcall(wezterm.mux.get_pane,popup:pane_id())
    if not ok or not live then clean();return end
    wezterm.time.call_after(.025,poll)
  end
  wezterm.time.call_after(.025,poll)
end
return M
