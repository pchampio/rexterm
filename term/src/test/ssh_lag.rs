//! A shell behind SSH: its redraw for a size arrives after the terminal has
//! already taken newer sizes. The prompt has the user's shape (precmd prints
//! OSC 133 A and an empty line; PS1 prints the path line before OSC 133 P).
//! The redraw bytes are the ones of the user's real zsh (user_zsh_trace.rs).
use super::*;

const PATH: &str = "/home/user/project/wezterm/bin @server";

fn size(cols: usize) -> TerminalSize {
    TerminalSize { rows: 20, cols, pixel_width: 0, pixel_height: 0, dpi: 0 }
}

fn prompt(input: &str) -> String {
    format!(
        "\x1b[0m\x1b[27m\x1b[24m\x1b[J\x1b[34m/home/user/project/wezterm/bin\x1b[39m \x1b[38;5;242m@server\x1b[39m\r\n\r\x1b]133;P;k=i\x07\x1b[32m$~\x1b[39m \x1b]133;B\x07\x1b]122;> \x07{input}"
    )
}

/// The rows that zsh counts above its cursor: the path line at `cols`, the
/// `$~ INPUT` line (one row here).
fn ups(cols: usize) -> usize {
    PATH.len().div_ceil(cols)
}

fn start(cols: usize, input: &str) -> TestTerm {
    let mut term = TestTerm::new(20, cols, 1000);
    term.print("\x1b]133;C;\x07output one\r\noutput two\r\n\x1b]133;D;0\x07\x1b]133;A;cl=m\x07\r\n");
    term.print(prompt(input));
    term
}

fn redraw(term: &mut TestTerm, zsh_cols: usize, input: &str) {
    let mut out = String::from("\r\r");
    for _ in 0..ups(zsh_cols) {
        out.push_str("\x1b[A");
    }
    out.push_str(&prompt(input));
    term.print(out);
}

fn check(term: &TestTerm, input: &str, label: &str) {
    let rows: Vec<String> = term
        .screen()
        .visible_lines()
        .iter()
        .map(|line| line.as_str().trim_end().to_owned())
        .collect();
    let text: String = rows.concat();
    let shown = || rows.iter().filter(|r| !r.is_empty()).cloned().collect::<Vec<_>>().join("\n");
    std::assert_eq!(text.matches("@server").count(), 1, "{label}: one prompt\n{}", shown());
    assert!(text.contains("output oneoutput two") || rows.iter().any(|r| r == "output one"), "{label}: output\n{}", shown());
    std::assert_eq!(rows.iter().filter(|r| r.starts_with("$~")).count(), 1, "{label}\n{}", shown());
    if !input.is_empty() {
        assert!(rows.iter().any(|r| r.starts_with("$~") && r.ends_with(input)), "{label}: input\n{}", shown());
    }
}

/// zsh handles each SIGWINCH `lag` resizes late, and its row count is the one
/// of its last draw.
fn run(widths: &[usize], lag: usize, input: &str) {
    let first = widths[0];
    let mut term = start(first, input);
    let mut drawn = first;
    for (index, cols) in widths.iter().enumerate().skip(1) {
        term.resize(size(*cols));
        if index >= lag {
            let zsh_now = widths[index - lag];
            redraw(&mut term, drawn, input);
            drawn = zsh_now;
        }
    }
    for index in widths.len().saturating_sub(lag).max(1)..widths.len() {
        redraw(&mut term, drawn, input);
        drawn = widths[index];
    }
    redraw(&mut term, drawn, input);
    check(&term, input, &format!("widths {widths:?} lag {lag} input {input:?}"));
}

#[test]
fn lagging_redraws_keep_one_prompt() {
    let patterns: &[&[usize]] = &[
        &[110, 55, 54, 56],
        &[110, 55, 110, 55, 110],
        &[80, 30, 22, 70, 80],
        &[90, 60, 45, 30, 20, 15, 30, 60, 90],
        &[40, 39, 38, 37, 36, 35, 40, 120],
        &[60, 20, 60, 20, 60],
    ];
    for widths in patterns {
        for lag in 0..=3 {
            for input in ["", "fzf"] {
                run(widths, lag, input);
            }
        }
    }
}
