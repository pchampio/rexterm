# rexterm

rexterm is WezTerm (from the ykawa/wezterm fork, with floating panes) with a native
Rex domain. Rex sessions show as native WezTerm tabs,
splits and popups, locally and through an SSH shell, as a replacement for tmux.

This directory holds what the GUI build does not: the launcher, the Lua
configuration, and the host helpers.

| Path | Use |
| --- | --- |
| `bin/wezterm-rex` | The launcher. It opens an ordinary shell with the Rex profile. |
| `bin/tm` | The session chooser and attachment (`tm`, `tm NAME`). `rex-attach` is the same program. |
| `bin/rex-agent-status`, `bin/rex-agent-shell` | Claude Code and Codex hooks: agent status in the tab bar, agent commands in their own terminals. |
| `bin/imgcat`, `bin/wezterm-audio` | Kitty images and OSC 7377 audio. |
| `config/` | The WezTerm profile (`wezterm-rex.lua`) and the Rex workflow modules. `config/rex/server.lua` is the companion that the Rex server loads. |
| `scripts/package` | Makes `rexterm-linux-x86_64.tar.gz` from a build. |

## Build

The GitHub workflow `.github/workflows/rexterm.yml` builds Linux x86-64 on Ubuntu
24.04 (glibc 2.39 or later), and publishes the bundle as the `continuous` release.

To build by hand, you need the packages of `get-deps`, Rust 1.99, and Zig 0.17
(`ZIG=/path/to/zig`, for `ghostty-vt`):

```
cargo build --locked --release -p wezterm-gui -p wezterm
rex/scripts/package target/release dist
```

## Install

Extract the bundle, then link a Rex server binary as `runtime-rex/bin/rex` (or
put `rex` on PATH). Start `bin/wezterm-rex`, then run `tm` in its shell, or
`ssh HOST` and `tm` on a host with `tm`, `config/rex/server.lua` and `rex`.
