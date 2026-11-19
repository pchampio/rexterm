//! Replays of the user's real zsh (see user_zsh_trace.rs). Its PS1 prints the
//! path line before the OSC 133 P mark, and precmd prints an empty line
//! between OSC 133 A and the prompt.
use super::user_zsh_trace::*;
use super::*;
use crate::commands::{find_command, CommandTarget};

fn size(cols: usize) -> TerminalSize {
    TerminalSize { rows: 30, cols, pixel_width: 0, pixel_height: 0, dpi: 0 }
}

fn replay(trace: &[(usize, &str)], until_size: usize) -> TestTerm {
    let mut term = TestTerm::new(30, 110, 1000);
    let mut sizes = 0;
    for (cols, bytes) in trace {
        if *cols > 0 {
            sizes += 1;
            if sizes > until_size {
                break;
            }
            term.resize(size(*cols));
        } else {
            term.print(bytes);
        }
    }
    term
}

fn rows(term: &TestTerm) -> Vec<String> {
    term.screen()
        .visible_lines()
        .iter()
        .map(|line| line.as_str().trim_end().to_owned())
        .filter(|line| !line.is_empty())
        .collect()
}

#[test]
fn split_resizes_keep_one_prompt() {
    let term = replay(USER_ZSH_SPLIT, usize::MAX);
    let all = rows(&term);
    let prompts = all.iter().filter(|row| row.starts_with("/home/user/project/wezterm/bin")).count();
    // Three prompts: before ls, before echo, and the current one.
    std::assert_eq!(prompts, 3, "{}", all.join("\n"));
    std::assert_eq!(all.last().map(String::as_str), Some("$~"), "{}", all.join("\n"));
}

/// Walk the commands from the bottom, as Ctrl-k does in copy mode.
fn commands(term: &mut TestTerm) -> Vec<String> {
    let zones = term.get_semantic_zones().unwrap();
    let screen = term.screen();
    let total = screen.scrollback_rows();
    let lines: Vec<(StableRowIndex, Line)> = screen
        .lines_in_phys_range(0..total)
        .into_iter()
        .enumerate()
        .map(|(i, line)| (screen.phys_to_stable_row_index(i), line))
        .collect();
    let mut y = screen.visible_row_to_stable_row(term.cursor_pos().y);
    let mut found = vec![];
    while let Some(range) = find_command(&zones, y, CommandTarget::Previous, Some("$~"), &mut |row| {
        lines.iter().find(|(y, _)| *y == row).map(|(_, line)| line.clone())
    }) {
        if range.end_y >= y {
            break;
        }
        let mut text = String::new();
        for row in range.start_y..=range.end_y {
            let line = &lines.iter().find(|(y, _)| *y == row).unwrap().1;
            let from = if row == range.start_y { range.start_x } else { 0 };
            let to = if row == range.end_y { range.end_x } else { usize::MAX };
            for cell in line.visible_cells() {
                if cell.cell_index() >= from && cell.cell_index() <= to {
                    text.push_str(cell.str());
                }
            }
            text.push('|');
        }
        found.push(text);
        y = range.end_y;
    }
    found
}

#[test]
fn command_jump_selects_only_the_typed_command() {
    // Before the resizes, and after each one: the redrawn path line is
    // prompt text, never a command.
    for sizes in 1..=7 {
        let mut term = replay(USER_ZSH_SPLIT, sizes);
        std::assert_eq!(
            commands(&mut term),
            vec!["echo alpha beta|", "ls|"],
            "after {sizes} sizes:\n{}",
            rows(&term).join("\n")
        );
    }
}

#[test]
fn every_resize_keeps_one_prompt() {
    for sizes in 1..=7 {
        let term = replay(USER_ZSH_SPLIT, sizes);
        let text: String = rows(&term).concat();
        std::assert_eq!(text.matches("@server").count(), 3, "after {sizes} sizes:\n{}", rows(&term).join("\n"));
        assert!(text.contains("alpha beta"), "the output stays:\n{}", rows(&term).join("\n"));
    }
}

/// Replay with the shell's output for each size delayed until `lag` more
/// resizes reached the terminal, as over SSH.
fn replay_lagged(trace: &[(usize, &str)], lag: usize) -> TestTerm {
    let mut term = TestTerm::new(30, 110, 1000);
    let mut waiting: std::collections::VecDeque<&str> = Default::default();
    let mut seen_size = false;
    for (cols, bytes) in trace {
        if *cols > 0 {
            term.resize(size(*cols));
            if seen_size {
                waiting.push_back("");
                while waiting.len() > lag {
                    let out = waiting.pop_front().unwrap();
                    term.print(out);
                }
            }
            seen_size = true;
        } else if let Some(last) = waiting.back_mut() {
            // Output after a resize waits with that resize.
            let joined: &'static str = Box::leak(format!("{last}{bytes}").into_boxed_str());
            *last = joined;
        } else {
            term.print(bytes);
        }
    }
    while let Some(out) = waiting.pop_front() {
        term.print(out);
    }
    term
}

#[test]
fn path_line_as_wide_as_the_pane_keeps_one_prompt() {
    for lag in 0..=3 {
        let term = replay_lagged(USER_ZSH_EDGE, lag);
        let all = rows(&term);
        std::assert_eq!(all.concat().matches("@server").count(), 1, "lag {lag}:\n{}", all.join("\n"));
        std::assert_eq!(all.last().map(String::as_str), Some("$~"), "lag {lag}:\n{}", all.join("\n"));
    }
    for lag in 1..=3 {
        let term = replay_lagged(USER_ZSH_SPLIT, lag);
        let all = rows(&term);
        std::assert_eq!(all.concat().matches("@server").count(), 3, "split lag {lag}:\n{}", all.join("\n"));
    }
}

#[test]
fn a_restored_prompt_is_corrected_on_the_first_resize() {
    // Rex attach: the screen comes from a snapshot. The cells keep their
    // semantic types, but the terminal never saw the prompt's marks.
    let mut prefix = vec![];
    let mut sizes = 0;
    for event in USER_ZSH_SPLIT {
        if event.0 > 0 {
            sizes += 1;
            if sizes == 2 {
                break;
            }
        }
        prefix.push(*event);
    }
    let mut term = replay(&prefix, usize::MAX);
    term.forget_prompt_state();
    // The resizes of the trace, but narrower: the path line wraps.
    let mut zsh_rows = 1;
    for cols in [30usize, 22, 70] {
        term.resize(size(cols));
        let mut redraw = String::from("\r\r");
        for _ in 0..zsh_rows {
            redraw.push_str("\x1b[A");
        }
        redraw.push_str("\x1b[0m\x1b[27m\x1b[24m\x1b[J\x1b[34m/home/user/project/wezterm/bin\x1b[39m \x1b[38;5;242m@server\x1b[39m\r\n\r\x1b]133;P;k=i\x07\x1b[32m$~\x1b[39m \x1b]133;B\x07");
        term.print(redraw);
        zsh_rows = "/home/user/project/wezterm/bin @server".len().div_ceil(cols);
        let all = rows(&term);
        std::assert_eq!(all.concat().matches("@server").count(), 3, "at {cols}:\n{}", all.join("\n"));
    }
}

#[test]
fn command_blocks_end_before_the_next_prompt() {
    use crate::commands::command_blocks;
    let mut term = replay(USER_ZSH_SPLIT, 1);
    let zones = term.get_semantic_zones().unwrap();
    let screen = term.screen();
    let total = screen.scrollback_rows();
    let lines: Vec<(StableRowIndex, Line)> = screen
        .lines_in_phys_range(0..total)
        .into_iter()
        .enumerate()
        .map(|(i, line)| (screen.phys_to_stable_row_index(i), line))
        .collect();
    let text = |y: StableRowIndex| lines.iter().find(|(row, _)| *row == y).map(|(_, l)| l.as_str().trim_end().to_owned()).unwrap_or_default();
    let last = lines.last().unwrap().0;
    let blocks = command_blocks(&zones, last, &mut |row| lines.iter().find(|(y, _)| *y == row).map(|(_, l)| l.clone()));
    let shown: Vec<(String, String, String)> = blocks
        .iter()
        .map(|(range, end)| (text(range.start_y), text(*end), text(*end + 1)))
        .collect();
    // Each block ends on the empty row that precmd prints, right above the
    // next path line.
    for (start, end, next) in &shown {
        assert!(start.starts_with("$~ "), "{shown:?}");
        std::assert_eq!(end.as_str(), "", "{shown:?}");
        assert!(next.starts_with("/home/user/project/wezterm/bin"), "{shown:?}");
    }
    std::assert_eq!(shown.len(), 2, "{shown:?}");
}

#[test]
fn at_prompt_follows_the_shell_marks() {
    let mut term = TestTerm::new(10, 80, 100);
    let at = |term: &TestTerm| term.user_vars().get("WEZTERM_AT_PROMPT").cloned().unwrap_or_default();
    term.print("\x1b]133;A;cl=m\x07\r\n/path @host\r\n\r\x1b]133;P;k=i\x07$~ \x1b]133;B\x07ls");
    std::assert_eq!(at(&term), "1");
    term.print("\r\n\x1b]133;C;\x07file\r\n");
    std::assert_eq!(at(&term), "");
    term.print("\x1b]133;D;0\x07\x1b]133;A;cl=m\x07");
    std::assert_eq!(at(&term), "1");
}
