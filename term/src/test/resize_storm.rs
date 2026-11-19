//! Replay real resize storms (see resize_storm_traces.rs) and make sure that
//! the screen ends with each prompt once and the command output in place.
use super::resize_storm_traces::*;
use super::*;

fn size(cols: usize) -> TerminalSize {
    TerminalSize {
        rows: 30,
        cols,
        pixel_width: 0,
        pixel_height: 0,
        dpi: 0,
    }
}

/// The screen text after the trace, as one string without row breaks.
fn replay(trace: &[(usize, &str)]) -> (String, Vec<String>) {
    let mut term = TestTerm::new(30, 60, 1000);
    for (cols, bytes) in trace {
        if *cols > 0 {
            term.resize(size(*cols));
        } else {
            term.print(bytes);
        }
    }
    let rows: Vec<String> = term
        .screen()
        .visible_lines()
        .iter()
        .map(|line| line.as_str().trim_end().to_owned())
        .filter(|line| !line.is_empty())
        .collect();
    (rows.concat(), rows)
}

fn check(name: &str, trace: &[(usize, &str)], prompt: &str) {
    let (text, rows) = replay(trace);
    std::assert_eq!(
        text.matches(prompt).count(),
        2,
        "{name}: each prompt once\n{}",
        rows.join("\n")
    );
    let output = "guest-selection-fixture-00-test.pyguest-selection-fixture-01-test.py\
                  guest-selection-fixture-02-test.pyLAST_OUTPUT_ROW";
    assert!(text.contains(output), "{name}: the command output is in place\n{}", rows.join("\n"));
    // Nothing but the two prompts, the command and its output.
    let rest = text
        .replace(prompt, "")
        .replace(output, "")
        .replace("ls | head -n 3; echo LAST_OUTPUT_ROW", "")
        .replace("$~", "");
    assert!(rest.trim().is_empty(), "{name}: stray text {rest:?}\n{}", rows.join("\n"));
}

#[test]
fn bash_fast_storm() {
    check("bash, 4 ms steps", BASH_FAST, "lab@wezterm-tmuxcc:/tmp/lab/wezterm_tmuxcc-untested/scripts$");
}

#[test]
fn bash_slow_storm() {
    check("bash, 30 ms steps", BASH_SLOW, "lab@wezterm-tmuxcc:/tmp/lab/wezterm_tmuxcc-untested/scripts$");
}

#[test]
fn zsh_fast_storm() {
    check("zsh, 4 ms steps", ZSH_FAST, "/tmp/lab/wezterm_tmuxcc-untested/scripts");
}

#[test]
fn zsh_slow_storm() {
    check("zsh, 30 ms steps", ZSH_SLOW, "/tmp/lab/wezterm_tmuxcc-untested/scripts");
}


#[test]
fn bash_squeezed_to_one_column() {
    check("bash, down to 1 column", BASH_NARROW, "lab@wezterm-tmuxcc:/tmp/lab/wezterm_tmuxcc-untested/scripts$");
}

#[test]
fn zsh_squeezed_to_one_column() {
    check("zsh, down to 1 column", ZSH_NARROW, "/tmp/lab/wezterm_tmuxcc-untested/scripts");
}

