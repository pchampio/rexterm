local wezterm=require 'wezterm'
local bundle_dir=wezterm.config_dir
local config
if os.getenv('REX_MINIMAL')=='1' then
  config=wezterm.config_builder()
  config.font=wezterm.font 'DejaVu Sans Mono'
  config.font_size=12
  config.color_scheme='Builtin Solarized Light'
  config.enable_wayland=false
  config.window_padding={left=0,right=0,top=0,bottom=0}
else
  -- Keep the user's existing fonts, voice, audio, password and image-paste hooks.
  config=dofile(bundle_dir..'/rex/profile.lua').load(wezterm)
end
config.check_for_updates=false
config.serialize_lua_key_actions=true
-- Start the ordinary local shell. Rex is attached explicitly from that shell.
-- An endpoint is only set by the optional --attach / --ssh launcher modes.
local endpoint=os.getenv('REX_ENDPOINT')
if endpoint and endpoint~='' then
  config.rex_domains={{name='rex',endpoint=endpoint,session=os.getenv('REX_SESSION') or 'main',remote_shell=os.getenv('REX_REMOTE_SHELL')=='1'}}
  config.default_domain='rex'
  config.default_prog=nil
else
  config.default_domain='local'
end
config.tmux_continuous_window_tabs=false
dofile(bundle_dir..'/rex/workflow.lua').apply(config,wezterm,bundle_dir..'/rex')
dofile(bundle_dir..'/rex/status.lua').apply(config,wezterm,bundle_dir..'/rex')
dofile(bundle_dir..'/rex/integrations.lua').apply(config,wezterm)
dofile(bundle_dir..'/rex/smart-splits.lua').apply(wezterm)
return config
