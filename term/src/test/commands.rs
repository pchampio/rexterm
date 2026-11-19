//! Command ranges must be the same commands at every pane width.
use super::*;
use crate::commands::{find_command, CommandRange, CommandTarget};

fn size(cols: usize, rows: usize) -> TerminalSize {
    TerminalSize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
        dpi: 0,
    }
}

const PROMPT: &str = "\x1b]133;A;cl=m\x07\x1b]133;P;k=i\x07~/lab/project @host\r\n$~ \x1b]133;B\x07";

/// One prompt, a typed command and its output, as the user's zsh sends them.
fn run(term: &mut TestTerm, command: &str, output: &str) {
    term.print(PROMPT);
    term.print(command);
    term.print("\r\n\x1b]133;C;\x07");
    term.print(output);
    term.print("\x1b]133;D;0\x07");
}

fn session(cols: usize) -> TestTerm {
    let mut term = TestTerm::new(40, cols, 1000);
    run(&mut term, "echo one", "one\r\n");
    run(&mut term, "true", "");
    run(
        &mut term,
        "for name in alpha beta gamma delta; do printf '%s\\n' \"$name\"; done",
        "alpha\r\nbeta\r\ngamma\r\ndelta\r\n",
    );
    // A continuation prompt starts a second input zone.
    term.print(PROMPT);
    term.print("echo first \\\r\n\x1b]133;P;k=s\x07> \x1b]133;B\x07second line");
    term.print("\r\n\x1b]133;C;\x07first second line\r\n\x1b]133;D;0\x07");
    run(&mut term, "  ls   ", "file\r\n");
    term.print(PROMPT);
    term
}

fn text(term: &mut TestTerm, range: CommandRange) -> String {
    let screen = term.screen();
    let mut out = String::new();
    for y in range.start_y..=range.end_y {
        let phys = screen.stable_row_to_phys(y).unwrap();
        let line = screen.lines_in_phys_range(phys..phys + 1).remove(0);
        let from = if y == range.start_y { range.start_x } else { 0 };
        let to = if y == range.end_y { range.end_x } else { usize::MAX };
        let mut row = String::new();
        for cell in line.visible_cells() {
            let x = cell.cell_index();
            if x >= from && x <= to && cell.attrs().semantic_type() != SemanticType::Prompt {
                row.push_str(cell.str());
            }
        }
        // A wrapped row keeps its trailing space: the text continues.
        if line.last_cell_was_wrapped() && y != range.end_y {
            out.push_str(&row);
        } else {
            out.push_str(row.trim_end());
            if y != range.end_y {
                out.push('\n');
            }
        }
    }
    out
}

fn find(term: &mut TestTerm, y: StableRowIndex, target: CommandTarget, marker: Option<&str>) -> Option<CommandRange> {
    let zones = term.get_semantic_zones().unwrap();
    let lines: Vec<(StableRowIndex, Line)> = {
        let screen = term.screen();
        let total = screen.scrollback_rows();
        screen
            .lines_in_phys_range(0..total)
            .into_iter()
            .enumerate()
            .map(|(i, line)| (screen.phys_to_stable_row_index(i), line))
            .collect()
    };
    find_command(&zones, y, target, marker, &mut |row| {
        lines.iter().find(|(y, _)| *y == row).map(|(_, line)| line.clone())
    })
}

/// Walk up from the prompt, then down again, as Ctrl-k and Ctrl-j do.
fn walk(term: &mut TestTerm) -> (Vec<String>, Vec<String>) {
    let cursor = term.cursor_pos().y;
    let mut y = term.screen().visible_row_to_stable_row(cursor);
    let mut up = vec![];
    while let Some(range) = find(term, y, CommandTarget::Previous, None) {
        // Without a command above, the search stays on the current command.
        if range.end_y >= y {
            break;
        }
        up.push(text(term, range));
        // Copy mode leaves its cursor on the last cell of the selection.
        y = range.end_y;
    }
    let mut down = vec![];
    while let Some(range) = find(term, y, CommandTarget::Next, None) {
        if range.start_y <= y {
            break;
        }
        down.push(text(term, range));
        y = range.end_y;
    }
    (up, down)
}

const COMMANDS: [&str; 5] = [
    "ls",
    "echo first \\\nsecond line",
    "for name in alpha beta gamma delta; do printf '%s\\n' \"$name\"; done",
    "true",
    "echo one",
];

#[test]
fn the_same_commands_at_every_width() {
    for cols in [200, 80, 40, 23, 12] {
        let mut term = session(cols);
        let (up, down) = walk(&mut term);
        std::assert_eq!(up, COMMANDS, "walk up at {cols} columns");
        let mut expected: Vec<&str> = COMMANDS.iter().rev().skip(1).copied().collect();
        expected.truncate(down.len());
        std::assert_eq!(down, expected, "walk down at {cols} columns");
        std::assert_eq!(down.len(), COMMANDS.len() - 1, "walk down at {cols} columns");
    }
}

#[test]
fn the_same_commands_after_the_window_shrinks() {
    let mut term = session(120);
    for cols in [90, 61, 37, 20, 14, 33, 150] {
        term.resize(size(cols, 40));
        let (up, _) = walk(&mut term);
        std::assert_eq!(up, COMMANDS, "after resize to {cols} columns");
    }
}

#[test]
fn current_command_and_prompt_marker() {
    let mut term = session(80);
    let zones = term.get_semantic_zones().unwrap();
    let ls = zones
        .iter()
        .filter(|zone| zone.semantic_type == SemanticType::Input)
        .nth(5)
        .unwrap()
        .start_y;
    let range = find(&mut term, ls, CommandTarget::Current, None).unwrap();
    std::assert_eq!(text(&mut term, range), "ls");
    // An output row selects the command above it.
    let range = find(&mut term, ls + 1, CommandTarget::Current, None).unwrap();
    std::assert_eq!(text(&mut term, range), "ls");
    // A shell without OSC 133 marks: the text after the prompt marker.
    let mut plain = TestTerm::new(5, 40, 100);
    plain.print("~/x @remote\r\n$~   make test  \r\nok\r\n");
    let y = plain.screen().visible_row_to_stable_row(1);
    let range = find(&mut plain, y, CommandTarget::Current, Some("$~")).unwrap();
    std::assert_eq!(text(&mut plain, range), "make test");
    std::assert_eq!(find(&mut plain, y, CommandTarget::Current, None), None);
}
