use super::TestTermConfig;
use crate::{Terminal, TerminalSize};
use std::io::{self, Write};
use std::sync::{mpsc, Arc};
use std::time::Duration;
use wezterm_escape_parser::csi::{Device, CSI};
use wezterm_escape_parser::Action;

struct Recorder(mpsc::Sender<Vec<u8>>);
impl Write for Recorder {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.send(bytes.to_vec()).unwrap();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn mirrored_output_does_not_answer_queries_but_user_input_survives() {
    let (tx, rx) = mpsc::channel();
    let mut terminal = Terminal::new(
        TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 640,
            pixel_height: 384,
            dpi: 96,
        },
        Arc::new(TestTermConfig { scrollback: 100 }),
        "WezTerm",
        "test",
        Box::new(Recorder(tx)),
    );
    terminal.set_mirrored_output(true);
    terminal.advance_bytes(b"\x1b[c\x1b[6n");
    terminal.perform_actions(vec![Action::CSI(CSI::Device(
        Box::new(Device::RequestSecondaryDeviceAttributes),
    ))]);
    terminal
        .key_down(
            crate::input::KeyCode::Char('x'),
            crate::input::KeyModifiers::NONE,
        )
        .unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), b"x");
    // A regular terminal must still answer queries.
    terminal.set_mirrored_output(false);
    terminal.advance_bytes(b"\x1b[c");
    let answer = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(answer.starts_with(b"\x1b[?65;"), "{answer:?}");
    assert!(rx.try_recv().is_err());
}
