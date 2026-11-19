-- Clipboard inspection is local; Rex transfers bytes to the selected terminal's
-- machine and pastes its resulting paths. No SSH or local-PTY guessing is needed.
local M={}
function M.paste(wezterm,window,pane)
  local function run(args)
    local ok,out,err=wezterm.run_child_process(args)
    assert(ok,err~='' and err or (args[1]..' failed'))
    return out
  end
  local wayland=os.getenv('WAYLAND_DISPLAY')
  local function read(kind)
    if wayland then return run{'wl-paste','--no-newline','--type',kind} end
    return run{'xclip','-selection','clipboard','-o','-t',kind}
  end
  local temporary
  local ok,err=pcall(function()
    local types=wayland and run{'wl-paste','--list-types'} or run{'xclip','-selection','clipboard','-o','-t','TARGETS'}
    local function has(mime)for line in types:gmatch('[^\r\n]+') do if line==mime then return true end end return false end
    for _,format in ipairs{{'image/png','.png'},{'image/jpeg','.jpg'},{'image/webp','.webp'},{'image/gif','.gif'},{'image/bmp','.bmp'},{'image/tiff','.tiff'}} do
      if has(format[1]) then
        temporary=run{'mktemp','/tmp/wezterm-image-XXXXXXXXXXXX'..format[2]}:gsub('%s+$','')
        -- Keep binary clipboard data outside Lua/JSON strings.
        local args=wayland and {'wl-paste','--no-newline','--type',format[1]} or {'xclip','-selection','clipboard','-o','-t',format[1]}
        local shell={'sh','-c','target=$1; shift; "$@" > "$target" && test -s "$target"','rex-clipboard',temporary}
        for _,v in ipairs(args) do shell[#shell+1]=v end
        run(shell)
        pane:rex_call('wezterm.upload',{paths={temporary}})
        os.remove(temporary);temporary=nil;return
      end
    end
    if has('text/uri-list') then
      local paths={}
      for uri in read('text/uri-list'):gmatch('[^\r\n]+') do
        if uri:sub(1,1)~='#' then
          local path=uri:match('^file://(/.*)$') or uri:match('^file://localhost(/.*)$')
          assert(path,'Clipboard URI is not a local file')
          paths[#paths+1]=path:gsub('%%(%x%x)',function(n)return string.char(tonumber(n,16))end)
        end
      end
      if #paths>0 then pane:rex_call('wezterm.upload',{paths=paths});return end
    end
    window:perform_action(wezterm.action.PasteFrom('Clipboard'),pane)
  end)
  if temporary then os.remove(temporary) end
  if not ok then wezterm.log_error('Rex clipboard: '..tostring(err));window:toast_notification('Paste failed',tostring(err),nil,5000) end
end
return M
