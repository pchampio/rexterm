# format-tab-title-compact

This build provides a lightweight fallback for `format-tab-title`:

```lua
wezterm.on('format-tab-title-compact', function(tab, tabs, panes, hover, max_width)
  return tab.tab_title
end)
```

Arguments and return values match `format-tab-title`, except that the full
effective configuration is omitted. Capture the settings your formatter needs
in its Lua closure. This avoids converting every key binding, domain and color
scheme into Lua for every tab during each tab-bar update.

The existing `format-tab-title` event keeps its original arguments and takes
precedence. The compact event runs only when that event returns `nil` or has no
handler. If both return `nil`, the built-in title is used.
