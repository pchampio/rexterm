use super::*;

#[test]
fn status_sequences_fragmented_and_reset() {
    let mut t = TestTerm::new(4, 40, 100);
    let seq=b"\x1b]7501;state=working:app=codex\x1b\\\x1b]7501;state=blocked:id=build:kind=question:msg=b2s=\x07";
    for byte in seq {
        t.print([*byte]);
    }
    let state = &t.term.user_vars()["WEZTERM_PROGRAM_STATUS"];
    assert!(state.contains("blocked"));
    assert!(state.contains("codex"));
    assert!(state.contains("ok"));
    t.print(b"\x1b]7501;state=done:id=result\x07\x1b]133;A\x07");
    let state = &t.term.user_vars()["WEZTERM_PROGRAM_STATUS"];
    assert!(!state.contains("working"));
    assert!(!state.contains("blocked"));
    assert!(state.contains("done"));
    t.print(b"\x1b]7777;title;Fix auth; tests\x07");
    std::assert_eq!(t.term.user_vars()["WEZTERM_AGENT_TITLE"], "Fix auth; tests");
    t.print(b"\x1b]7777;status;needs_input\x07");
    std::assert_eq!(t.term.user_vars()["WEZTERM_AGENT_STATUS"], "needs_input");
    t.print(b"\x1b]7777;clear\x07\x1bc");
    std::assert_eq!(t.term.user_vars()["WEZTERM_PROGRAM_STATUS"], "{}");
    std::assert_eq!(t.term.user_vars()["WEZTERM_AGENT_TITLE"], "");
}

#[test]
fn status_query_is_answered_once_and_suppressed_for_mirrors() {
    use std::io::{self, Write};
    struct Recorder(Arc<Mutex<Vec<u8>>>);
    impl Write for Recorder {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let output = Arc::new(Mutex::new(Vec::new()));
    let mut t = Terminal::new(
        TerminalSize {
            rows: 4,
            cols: 40,
            ..Default::default()
        },
        Arc::new(TestTermConfig { scrollback: 100 }),
        "WezTerm",
        "test",
        Box::new(Recorder(output.clone())),
    );
    t.advance_bytes(b"\x1b]7501;?\x07");
    // Terminal writes are threaded; wait for this specific response, bounded.
    for _ in 0..100 {
        if !output.lock().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    std::assert_eq!(
        &*output.lock().unwrap(),
        b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x1b\\"
    );
    output.lock().unwrap().clear();
    t.set_mirrored_output(true);
    t.advance_bytes(b"\x1b]7501;?\x07");
    t.key_down(crate::KeyCode::Char('x'), crate::KeyModifiers::NONE)
        .unwrap();
    for _ in 0..100 {
        if !output.lock().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    std::assert_eq!(&*output.lock().unwrap(), b"x");
}

#[test]
fn shell_prompt_forgets_agent_details() {
    let mut t = TestTerm::new(4, 40, 100);
    t.print(b"\x1b]7777;agent;claude\x07\x1b]7777;tool;Bash: cargo test\x07");
    t.print(b"\x1b]7501;state=working:id=claude:app=claude-code\x1b\\");
    std::assert_eq!(t.term.user_vars()["WEZTERM_AGENT_AGENT"], "claude");
    // The agent crashed: the shell prompt comes back.
    t.print(b"\x1b]133;A\x07");
    std::assert_eq!(t.term.user_vars()["WEZTERM_AGENT_AGENT"], "");
    std::assert_eq!(t.term.user_vars()["WEZTERM_AGENT_TOOL"], "");
    std::assert_eq!(t.term.user_vars()["WEZTERM_PROGRAM_STATUS"], "{}");
    // Variables that were never set are not published as empty ones.
    assert!(!t.term.user_vars().contains_key("WEZTERM_AGENT_SUBAGENTS"));
}
