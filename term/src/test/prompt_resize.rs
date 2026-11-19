//! A shell redraws its prompt after a resize, from the row where it believes
//! the prompt starts. zsh and readline count the rows of the prompt as they
//! were at their last draw: they do not expect the terminal to rewrap, and
//! during a fast resize the count can be for an older size than the terminal
//! has now. The models below send the bytes that the real shells send
//! (captured from zsh 5.9 and bash 5.2 in a pty).
use super::*;

const PATH: &str = "~/lab/project-untestedxxxxxxx/scripts @server";

fn size(cols: usize, rows: usize) -> TerminalSize {
    TerminalSize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
        dpi: 0,
    }
}

fn prompt() -> String {
    // zle turns bracketed paste on while it reads a line.
    format!("\x1b]133;P;k=i\x07{PATH}\r\n$~ \x1b]133;B\x07\x1b[?2004h")
}

/// zsh with the user's two-line prompt and OSC 133 hooks.
struct Zsh {
    /// The width that zsh used for its last draw.
    cols: usize,
}

impl Zsh {
    /// The rows that zsh counts for the prompt at `cols`.
    fn prompt_height(cols: usize) -> usize {
        PATH.len().div_ceil(cols) + 1
    }
    fn start(cols: usize) -> (TestTerm, Self) {
        let mut term = TestTerm::new(12, cols, 100);
        term.print("first output\r\nlast output line\r\n");
        term.print("\x1b]133;A;cl=m\x07");
        term.print(prompt());
        (term, Self { cols })
    }
    /// zsh after SIGWINCH for the width `cols`: go up by the prompt rows of
    /// the last draw, clear to the end of the display, draw the prompt.
    fn redraw(&mut self, term: &mut TestTerm, cols: usize) {
        let mut out = String::from("\r\r");
        for _ in 1..Self::prompt_height(self.cols) {
            out.push_str("\x1b[A");
        }
        out.push_str("\x1b[0m\x1b[27m\x1b[24m\x1b[J");
        out.push_str(&prompt());
        term.print(out);
        self.cols = cols;
    }
}

fn rows(term: &TestTerm) -> Vec<String> {
    term.screen()
        .visible_lines()
        .iter()
        .map(|line| line.as_str().trim_end().to_owned())
        .filter(|line| !line.is_empty())
        .collect()
}

/// The rows of `lines` on a screen that is `cols` wide, as `rows` gives them.
fn wrapped(lines: &[&str], cols: usize) -> Vec<String> {
    let mut rows = vec![];
    for line in lines {
        let cells: Vec<char> = line.chars().collect();
        for chunk in cells.chunks(cols) {
            rows.push(chunk.iter().collect::<String>().trim_end().to_owned());
        }
    }
    rows
}

fn expected(cols: usize) -> Vec<String> {
    wrapped(&["first output", "last output line", PATH, "$~"], cols)
}

#[test]
fn redraw_after_each_resize_keeps_one_prompt() {
    let (mut term, mut zsh) = Zsh::start(60);
    for cols in [31, 20, 50, 13, 44, 60, 27] {
        term.resize(size(cols, 12));
        zsh.redraw(&mut term, cols);
        std::assert_eq!(rows(&term), expected(cols), "at {cols} columns");
    }
}

#[test]
fn redraws_for_older_sizes_keep_one_prompt_and_the_output() {
    let (mut term, mut zsh) = Zsh::start(70);
    // zsh is always one size behind the terminal, in both directions.
    let mut behind = 70;
    for cols in [66, 51, 40, 31, 24, 19, 26, 37, 58, 80, 33] {
        term.resize(size(cols, 12));
        zsh.redraw(&mut term, behind);
        behind = cols;
        std::assert_eq!(rows(&term), expected(cols), "at {cols} columns");
    }
    zsh.redraw(&mut term, behind);
    std::assert_eq!(rows(&term), expected(behind));
}

#[test]
fn two_resizes_before_the_first_redraw() {
    let (mut term, mut zsh) = Zsh::start(60);
    term.resize(size(50, 12));
    term.resize(size(21, 12));
    zsh.redraw(&mut term, 50);
    std::assert_eq!(rows(&term), expected(21));
    // The second redraw has no resize before it.
    zsh.redraw(&mut term, 21);
    std::assert_eq!(rows(&term), expected(21));
    term.resize(size(35, 12));
    term.resize(size(60, 12));
    zsh.redraw(&mut term, 35);
    zsh.redraw(&mut term, 60);
    std::assert_eq!(rows(&term), expected(60));
}

#[test]
fn an_erase_below_the_prompt_is_not_a_redraw() {
    let (mut term, _zsh) = Zsh::start(60);
    term.resize(size(50, 12));
    // A completion list: the shell clears the rows under its input line.
    term.print("\r\n\x1b[Jone  two  three\x1b[A\r\x1b[3C");
    let mut want = expected(50);
    want.push("one  two  three".to_owned());
    std::assert_eq!(rows(&term), want);
}

#[test]
fn a_preview_does_not_change_the_real_prompt_state() {
    let (mut term, _zsh) = Zsh::start(60);
    term.preview_resize(size(31, 12));
    term.cancel_resize_preview();
    // Without a real resize, a prompt drawn on a lower row is a new prompt.
    term.print("\r\n\r\n\x1b[J");
    term.print(prompt());
    let mut want = expected(60);
    want.extend(expected(60).split_off(2));
    std::assert_eq!(rows(&term), want);
}

/// bash (readline) has no OSC 133 marks here. Its prompt and input are one
/// logical line, and after SIGWINCH it redraws that line from the row where
/// the line started before the resize: it does not expect a rewrap.
mod readline {
    use super::*;

    const LINE: &str = "lab@wezterm-tmuxcc:/tmp/lab/wezterm_tmuxcc-untested/scripts$ ls -la";

    struct Bash {
        /// The width that readline used for its last draw.
        cols: usize,
    }

    impl Bash {
        fn rows(cols: usize) -> usize {
            LINE.len().div_ceil(cols)
        }
        fn start(term: &mut TestTerm, cols: usize) -> Self {
            term.print("first output\r\nlast output line\r\n\x1b[?2004h");
            term.print(LINE);
            Self { cols }
        }
        /// The bytes of bash 5.2 (readline 8.2) after SIGWINCH for the width
        /// `cols`, as captured from a real bash: clear the bottom row, clear
        /// each row above it that the old layout had, then draw the line.
        fn redraw(&mut self, term: &mut TestTerm, cols: usize) {
            let mut out = String::from("\r\x1b[K");
            for _ in 1..Self::rows(self.cols) {
                out.push_str("\r\x1b[A\x1b[K");
            }
            out.push('\r');
            out.push_str(LINE);
            term.print(out);
            self.cols = cols;
        }
    }

    fn expected(cols: usize) -> Vec<String> {
        wrapped(&["first output", "last output line", LINE], cols)
    }

    #[test]
    fn redraw_after_each_resize_keeps_one_prompt() {
        let mut term = TestTerm::new(14, 90, 100);
        let mut bash = Bash::start(&mut term, 90);
        for cols in [47, 33, 21, 38, 29, 64, 90, 25] {
            term.resize(size(cols, 14));
            bash.redraw(&mut term, cols);
            std::assert_eq!(rows(&term), expected(cols), "at {cols} columns");
        }
    }

    #[test]
    fn redraws_for_older_sizes_keep_one_prompt_and_the_output() {
        let mut term = TestTerm::new(14, 90, 100);
        let mut bash = Bash::start(&mut term, 90);
        // bash is always one size behind the terminal, in both directions.
        let sizes = [70, 52, 41, 30, 22, 27, 36, 58, 90, 33];
        let mut behind = 90;
        for cols in sizes {
            term.resize(size(cols, 14));
            bash.redraw(&mut term, behind);
            behind = cols;
            let all = rows(&term);
            std::assert_eq!(&all[..2], &expected(cols)[..2], "output above the prompt at {cols} columns");
            let text: String = all[2..].concat();
            std::assert_eq!(text.matches("lab@wezterm-tmuxcc:").count(), 1, "one prompt at {cols} columns: {all:?}");
        }
        bash.redraw(&mut term, behind);
        std::assert_eq!(rows(&term), expected(behind));
    }

    #[test]
    fn output_in_progress_still_rewraps() {
        // No line editor: bracketed paste is off, the partial line rewraps.
        let mut term = TestTerm::new(8, 40, 100);
        term.print("0123456789012345678901234567890123456789ABCDEFGHIJ");
        term.resize(size(25, 8));
        std::assert_eq!(
            rows(&term),
            vec!["0123456789012345678901234", "567890123456789ABCDEFGHIJ"]
        );
    }
}

#[test]
fn output_after_a_resize_does_not_overwrite_the_last_line() {
    // A program is between two lines of output: the cursor is on the first
    // cell of an empty row.
    let mut term = TestTerm::new(8, 60, 100);
    term.print("line one\r\nline two\r\n");
    term.resize(size(40, 8));
    term.print("line three\r\n");
    term.resize(size(70, 8));
    term.print("line four\r\n");
    std::assert_eq!(rows(&term), vec!["line one", "line two", "line three", "line four"]);
}
