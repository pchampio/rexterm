//! The user's real zsh (instant-zsh, two-line prompt, OSC 133 hooks), recorded
//! in a pty with scripts/record-user-zsh.py: clear, ls, echo, then the resizes
//! of a split (110 -> 55 -> 54 -> 56) and a squeeze (30 -> 22 -> 70 columns).

pub const USER_ZSH_SPLIT: &[(usize, &str)] = &[
    (110, ""),
    (0, "                                                                                                              \r\x1b[0m\x1b[49m\x1b[39m\x1b[K\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\x1b[10A\x1b7\x1b[34m/home/user/project/wezterm/bin\x1b[0m\x1b[34m\x1b[39m\r\n\x1b[32m$~\x1b[39m \x1b]2;@server:/home/user/project/wezterm/bin\x07\x1b]1;..t/wezterm/bin\x07\x1b]133;A;cl=m;aid=1386406\x07\x1b]0;(server) /home/user/project/wezterm/bin\x07\r\n\x1b8\x1b(B\x1b[m\x1b[J\x1b[0m\x1b[27m\x1b[24m\x1b[J\x1b[34m/home/user/project/wezterm/bin\x1b[39m \x1b[38;5;242m\x1b[39m\x1b[38;5;242m@server\x1b[39m\r\n\r\x1b]133;P;k=i\x07\x1b[32m$~\x1b[39m \x1b]133;B\x07\x1b]122;> \x07\x1b[K\x1b[?1h\x1b=\x1b[?2004hc\x08clear\x1b[?1l\x1b>\x1b[?2004l\x08\x08\x08\x08\x08\x1b[32mc\x1b[32ml\x1b[32me\x1b[32ma\x1b[32mr\x1b[39m\r\r\n\x1b]2;clear\x07\x1b]1;clear\x07\x1b]133;C;\x07\x1b]0;(server) bin: clear\x07\x1b[H\x1b[2J\x1b[3J\x1b]2;@server:/home/user/project/wezterm/bin\x07\x1b]1;..t/wezterm/bin\x07\x1b]133;D;0;aid=1386406\x07\x1b]133;A;cl=m;aid=1386406\x07\x1b]0;(server) /home/user/project/wezterm/bin\x07\r\n\x1b[0m\x1b[27m\x1b[24m\x1b[J\x1b[34m/home/user/project/wezterm/bin\x1b[39m \x1b[38;5;242m\x1b[39m\x1b[38;5;242m@server\x1b[39m\r\n\r\x1b]133;P;k=i\x07\x1b[32m$~\x1b[39m \x1b]133;B\x07\x1b]122;> \x07\x1b[K\x1b[?1h\x1b=\x1b[?2004hl\x08ls\x1b[?1l\x1b>\x1b[?2004l\x08\x08\x1b[32ml\x1b[32ms\x1b[39m\r\r\n\x1b]2;ls --color=tty\x07\x1b]1;ls\x07\x1b]133;C;\x07\x1b]0;(server) bin: ls --color=tty\x07\x1b[0m\x1b[01;31mimgcat\x1b[0m           \x1b[01;31mrex-agent-status\x1b[0m  \x1b[01;31mtm\x1b[0m    \x1b[01;31mtmuxcc\x1b[0m       \x1b[01;31mwezterm-tmuxcc\x1b[0m\r\n\x1b[01;31mrex-agent-shell\x1b[0m  \x1b[01;31mrex-attach\x1b[0m        \x1b[01;31mtmcc\x1b[0m  \x1b[01;31mwezterm-rex\x1b[0m  \x1b[01;31mwezterm-tmuxcc-preview\x1b[0m\r\n\x1b]2;@server:/home/user/project/wezterm/bin\x07\x1b]1;..t/wezterm/bin\x07\x1b]133;D;0;aid=1386406\x07\x1b]133;A;cl=m;aid=1386406\x07\x1b]0;(server) /home/user/project/wezterm/bin\x07\r\n\x1b[0m\x1b[27m\x1b[24m\x1b[J\x1b[34m/home/user/project/wezterm/bin\x1b[39m \x1b[38;5;242m\x1b[39m\x1b[38;5;242m@server\x1b[39m\r\n\r\x1b]133;P;k=i\x07\x1b[32m$~\x1b[39m \x1b]133;B\x07\x1b]122;> \x07\x1b[K\x1b[?1h\x1b=\x1b[?2004he\x08echo alpha beta\x1b[?1l\x1b>\x1b[?2004l\x1b[15D\x1b[32me\x1b[32mc\x1b[32mh\x1b[32mo\x1b[39m\x1b[11C\r\r\n\x1b]2;echo alpha beta\x07\x1b]1;echo\x07\x1b]133;C;\x07\x1b]0;(server) bin: echo alpha beta\x07alpha beta\r\n\x1b]2;@server:/home/user/project/wezterm/bin\x07\x1b]1;..t/wezterm/bin\x07\x1b]133;D;0;aid=1386406\x07\x1b]133;A;cl=m;aid=1386406\x07\x1b]0;(server) /home/user/project/wezterm/bin\x07\r\n\x1b[0m\x1b[27m\x1b[24m\x1b[J\x1b[34m/home/user/project/wezterm/bin\x1b[39m \x1b[38;5;242m\x1b[39m\x1b[38;5;242m@server\x1b[39m\r\n\r\x1b]133;P;k=i\x07\x1b[32m$~\x1b[39m \x1b]133;B\x07\x1b]122;> \x07\x1b[K\x1b[?1h\x1b=\x1b[?2004h"),
    (55, ""),
    (0, "\r\r\x1b[A\x1b[0m\x1b[27m\x1b[24m\x1b[J\x1b[34m/home/user/project/wezterm/bin\x1b[39m \x1b[38;5;242m\x1b[39m\x1b[38;5;242m@server\x1b[39m\r\n\r\x1b]133;P;k=i\x07\x1b[32m$~\x1b[39m \x1b]133;B\x07\x1b]122;> \x07"),
    (54, ""),
    (0, "\r\r\x1b[A\x1b[0m\x1b[27m\x1b[24m\x1b[J\x1b[34m/home/user/project/wezterm/bin\x1b[39m \x1b[38;5;242m\x1b[39m\x1b[38;5;242m@server\x1b[39m\r\n\r\x1b]133;P;k=i\x07\x1b[32m$~\x1b[39m \x1b]133;B\x07\x1b]122;> \x07"),
    (56, ""),
    (0, "\r\r\x1b[A\x1b[0m\x1b[27m\x1b[24m\x1b[J\x1b[34m/home/user/project/wezterm/bin\x1b[39m \x1b[38;5;242m\x1b[39m\x1b[38;5;242m@server\x1b[39m\r\n\r\x1b]133;P;k=i\x07\x1b[32m$~\x1b[39m \x1b]133;B\x07\x1b]122;> \x07"),
    (30, ""),
    (0, "\r\r\x1b[A\x1b[0m\x1b[27m\x1b[24m\x1b[J\x1b[34m/home/user/project/wezterm/bin\x1b[39m \x1b[38;5;242m\x1b[39m\x1b[38;5;242m@server\x1b[39m\r\n\r\x1b]133;P;k=i\x07\x1b[32m$~\x1b[39m \x1b]133;B\x07\x1b]122;> \x07"),
    (22, ""),
    (0, "\r\r\x1b[A\x1b[A\x1b[0m\x1b[27m\x1b[24m\x1b[J\x1b[34m/home/user/project/wezterm/bin\x1b[39m \x1b[38;5;242m\x1b[39m\x1b[38;5;242m@server\x1b[39m\r\n\r\x1b]133;P;k=i\x07\x1b[32m$~\x1b[39m \x1b]133;B\x07\x1b]122;> \x07"),
    (70, ""),
    (0, "\r\r\x1b[A\x1b[A\x1b[0m\x1b[27m\x1b[24m\x1b[J\x1b[34m/home/user/project/wezterm/bin\x1b[39m \x1b[38;5;242m\x1b[39m\x1b[38;5;242m@server\x1b[39m\r\n\r\x1b]133;P;k=i\x07\x1b[32m$~\x1b[39m \x1b]133;B\x07\x1b]122;> \x07"),
];

/// The user's real zsh at widths around the length of its path line (38).
pub const USER_ZSH_EDGE: &[(usize, &str)] = &[
    (110, ""),
    (0, "                                                                                                              \r\x1b[0m\x1b[49m\x1b[39m\x1b[K\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\x1b[10A\x1b7\x1b[34m/home/user/project/wezterm/bin\x1b[0m\x1b[34m\x1b[39m\r\n\x1b[32m$~\x1b[39m \x1b]2;@server:/home/user/project/wezterm/bin\x07\x1b]1;..t/wezterm/bin\x07\x1b]133;A;cl=m;aid=1573708\x07\x1b]0;(server) /home/user/project/wezterm/bin\x07\r\n\x1b8\x1b(B\x1b[m\x1b[J\x1b[0m\x1b[27m\x1b[24m\x1b[J\x1b[34m/home/user/project/wezterm/bin\x1b[39m \x1b[38;5;242m\x1b[39m\x1b[38;5;242m@server\x1b[39m\r\n\r\x1b]133;P;k=i\x07\x1b[32m$~\x1b[39m \x1b]133;B\x07\x1b]122;> \x07\x1b[K\x1b[?1h\x1b=\x1b[?2004hc\x08clear\x1b[?1l\x1b>\x1b[?2004l\x08\x08\x08\x08\x08\x1b[32mc\x1b[32ml\x1b[32me\x1b[32ma\x1b[32mr\x1b[39m\r\r\n\x1b]2;clear\x07\x1b]1;clear\x07\x1b]133;C;\x07\x1b]0;(server) bin: clear\x07\x1b[H\x1b[2J\x1b[3J\x1b]2;@server:/home/user/project/wezterm/bin\x07\x1b]1;..t/wezterm/bin\x07\x1b]133;D;0;aid=1573708\x07\x1b]133;A;cl=m;aid=1573708\x07\x1b]0;(server) /home/user/project/wezterm/bin\x07\r\n\x1b[0m\x1b[27m\x1b[24m\x1b[J\x1b[34m/home/user/project/wezterm/bin\x1b[39m \x1b[38;5;242m\x1b[39m\x1b[38;5;242m@server\x1b[39m\r\n\r\x1b]133;P;k=i\x07\x1b[32m$~\x1b[39m \x1b]133;B\x07\x1b]122;> \x07\x1b[K\x1b[?1h\x1b=\x1b[?2004h"),
    (60, ""),
    (0, "\r\r\x1b[A\x1b[0m\x1b[27m\x1b[24m\x1b[J\x1b[34m/home/user/project/wezterm/bin\x1b[39m \x1b[38;5;242m\x1b[39m\x1b[38;5;242m@server\x1b[39m\r\n\r\x1b]133;P;k=i\x07\x1b[32m$~\x1b[39m \x1b]133;B\x07\x1b]122;> \x07"),
    (39, ""),
    (0, "\r\r\x1b[A\x1b[0m\x1b[27m\x1b[24m\x1b[J\x1b[34m/home/user/project/wezterm/bin\x1b[39m \x1b[38;5;242m\x1b[39m\x1b[38;5;242m@server\x1b[39m\r\n\r\x1b]133;P;k=i\x07\x1b[32m$~\x1b[39m \x1b]133;B\x07\x1b]122;> \x07"),
    (38, ""),
    (0, "\r\r\x1b[A\x1b[0m\x1b[27m\x1b[24m\x1b[J\x1b[34m/home/user/project/wezterm/bin\x1b[39m \x1b[38;5;242m\x1b[39m\x1b[38;5;242m@server\x1b[39m\r\n\r\x1b]133;P;k=i\x07\x1b[32m$~\x1b[39m \x1b]133;B\x07\x1b]122;> \x07"),
    (37, ""),
    (0, "\r\r\x1b[A\x1b[0m\x1b[27m\x1b[24m\x1b[J\x1b[34m/home/user/project/wezterm/bin\x1b[39m \x1b[38;5;242m\x1b[39m\x1b[38;5;242m@server\x1b[39m\r\n\r\x1b]133;P;k=i\x07\x1b[32m$~\x1b[39m \x1b]133;B\x07\x1b]122;> \x07"),
    (38, ""),
    (0, "\r\r\x1b[A\x1b[A\x1b[0m\x1b[27m\x1b[24m\x1b[J\x1b[34m/home/user/project/wezterm/bin\x1b[39m \x1b[38;5;242m\x1b[39m\x1b[38;5;242m@server\x1b[39m\r\n\r\x1b]133;P;k=i\x07\x1b[32m$~\x1b[39m \x1b]133;B\x07\x1b]122;> \x07"),
    (39, ""),
    (0, "\r\r\x1b[A\x1b[0m\x1b[27m\x1b[24m\x1b[J\x1b[34m/home/user/project/wezterm/bin\x1b[39m \x1b[38;5;242m\x1b[39m\x1b[38;5;242m@server\x1b[39m\r\n\r\x1b]133;P;k=i\x07\x1b[32m$~\x1b[39m \x1b]133;B\x07\x1b]122;> \x07"),
    (80, ""),
    (0, "\r\r\x1b[A\x1b[0m\x1b[27m\x1b[24m\x1b[J\x1b[34m/home/user/project/wezterm/bin\x1b[39m \x1b[38;5;242m\x1b[39m\x1b[38;5;242m@server\x1b[39m\r\n\r\x1b]133;P;k=i\x07\x1b[32m$~\x1b[39m \x1b]133;B\x07\x1b]122;> \x07"),
    (38, ""),
    (0, "\r\r\x1b[A\x1b[0m\x1b[27m\x1b[24m\x1b[J\x1b[34m/home/user/project/wezterm/bin\x1b[39m \x1b[38;5;242m\x1b[39m\x1b[38;5;242m@server\x1b[39m\r\n\r\x1b]133;P;k=i\x07\x1b[32m$~\x1b[39m \x1b]133;B\x07\x1b]122;> \x07"),
    (37, ""),
    (0, "\r\r\x1b[A\x1b[0m\x1b[27m\x1b[24m\x1b[J\x1b[34m/home/user/project/wezterm/bin\x1b[39m \x1b[38;5;242m\x1b[39m\x1b[38;5;242m@server\x1b[39m\r\n\r\x1b]133;P;k=i\x07\x1b[32m$~\x1b[39m \x1b]133;B\x07\x1b]122;> \x07"),
    (39, ""),
    (0, "\r\r\x1b[A\x1b[A\x1b[0m\x1b[27m\x1b[24m\x1b[J\x1b[34m/home/user/project/wezterm/bin\x1b[39m \x1b[38;5;242m\x1b[39m\x1b[38;5;242m@server\x1b[39m\r\n\r\x1b]133;P;k=i\x07\x1b[32m$~\x1b[39m \x1b]133;B\x07\x1b]122;> \x07"),
    (38, ""),
    (0, "\r\r\x1b[A\x1b[0m\x1b[27m\x1b[24m\x1b[J\x1b[34m/home/user/project/wezterm/bin\x1b[39m \x1b[38;5;242m\x1b[39m\x1b[38;5;242m@server\x1b[39m\r\n\r\x1b]133;P;k=i\x07\x1b[32m$~\x1b[39m \x1b]133;B\x07\x1b]122;> \x07"),
    (70, ""),
    (0, "\r\r\x1b[A\x1b[0m\x1b[27m\x1b[24m\x1b[J\x1b[34m/home/user/project/wezterm/bin\x1b[39m \x1b[38;5;242m\x1b[39m\x1b[38;5;242m@server\x1b[39m\r\n\r\x1b]133;P;k=i\x07\x1b[32m$~\x1b[39m \x1b]133;B\x07\x1b]122;> \x07"),
    (38, ""),
    (0, "\r\r\x1b[A\x1b[0m\x1b[27m\x1b[24m\x1b[J\x1b[34m/home/user/project/wezterm/bin\x1b[39m \x1b[38;5;242m\x1b[39m\x1b[38;5;242m@server\x1b[39m\r\n\r\x1b]133;P;k=i\x07\x1b[32m$~\x1b[39m \x1b]133;B\x07\x1b]122;> \x07"),
];
