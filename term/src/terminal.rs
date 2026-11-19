use super::*;
use crate::terminalstate::performer::Performer;
use std::sync::Arc;
use wezterm_escape_parser::parser::Parser;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub enum ClipboardSelection {
    Clipboard,
    PrimarySelection,
}

pub trait Clipboard: Send + Sync {
    fn set_contents(
        &self,
        selection: ClipboardSelection,
        data: Option<String>,
    ) -> anyhow::Result<()>;
}

impl Clipboard for Box<dyn Clipboard> {
    fn set_contents(
        &self,
        selection: ClipboardSelection,
        data: Option<String>,
    ) -> anyhow::Result<()> {
        self.as_ref().set_contents(selection, data)
    }
}

pub trait DeviceControlHandler: Send + Sync {
    fn handle_device_control(&mut self, _control: wezterm_escape_parser::DeviceControlMode);
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub enum Progress {
    #[default]
    None,
    Percentage(u8),
    Error(u8),
    Indeterminate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub enum Alert {
    Bell,
    ToastNotification {
        /// The title text for the notification.
        title: Option<String>,
        /// The message body
        body: String,
        /// Whether clicking on the notification should focus the
        /// window/tab/pane that generated it
        focus: bool,
    },
    CurrentWorkingDirectoryChanged,
    IconTitleChanged(Option<String>),
    WindowTitleChanged(String),
    TabTitleChanged(Option<String>),
    /// When the color palette has been updated
    PaletteChanged,
    /// A UserVar has changed value
    SetUserVar {
        name: String,
        value: String,
    },
    /// When something bumps the seqno in the terminal model and
    /// the terminal is not focused
    OutputSinceFocusLost,
    /// A change to the progress bar state
    Progress(Progress),
}

pub trait AlertHandler: Send + Sync {
    fn alert(&mut self, alert: Alert);
}

pub trait DownloadHandler: Send + Sync {
    fn save_to_downloads(&self, name: Option<String>, data: Vec<u8>);
}

/// Represents an instance of a terminal emulator.
pub struct Terminal {
    /// The terminal model/state
    state: TerminalState,
    /// Baseline terminal escape sequence parser
    parser: Parser,
    mirrored_output: bool,
    mirrored_actions: Vec<wezterm_escape_parser::Action>,
    mirrored_hold: Option<std::time::Instant>,
    mirrored_held_bytes: usize,
}

impl Deref for Terminal {
    type Target = TerminalState;

    fn deref(&self) -> &TerminalState {
        &self.state
    }
}

impl DerefMut for Terminal {
    fn deref_mut(&mut self) -> &mut TerminalState {
        &mut self.state
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, FromDynamic, ToDynamic)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub struct TerminalSize {
    pub rows: usize,
    pub cols: usize,
    pub pixel_width: usize,
    pub pixel_height: usize,
    pub dpi: u32,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        }
    }
}

impl Terminal {
    /// Construct a new Terminal.
    /// `physical_rows` and `physical_cols` describe the dimensions
    /// of the visible portion of the terminal display in terms of
    /// the number of text cells.
    ///
    /// `pixel_width` and `pixel_height` describe the dimensions of
    /// that same visible area but in pixels.
    ///
    /// `term_program` and `term_version` are required to identify
    /// the host terminal program; they are used to respond to the
    /// terminal identification sequence `\033[>q`.
    ///
    /// `writer` is anything that implements `std::io::Write`; it
    /// is used to send input to the connected program; both keyboard
    /// and mouse input is encoded and written to that stream, as
    /// are answerback responses to a number of escape sequences.
    pub fn new(
        size: TerminalSize,
        config: Arc<dyn TerminalConfiguration + Send + Sync>,
        term_program: &str,
        term_version: &str,
        // writing to the writer sends data to input of the pty
        writer: Box<dyn std::io::Write + Send>,
    ) -> Terminal {
        Terminal {
            state: TerminalState::new(size, config, term_program, term_version, writer),
            parser: Parser::new(),
            mirrored_output: false,
            mirrored_actions: vec![],
            mirrored_hold: None,
            mirrored_held_bytes: 0,
        }
    }

    /// A multiplexer already answers application queries. Its display mirror
    /// must render those queries without injecting a second answer into stdin.
    /// Explicit keyboard, mouse and paste input remains enabled.
    pub fn set_mirrored_output(&mut self, mirrored: bool) {
        self.mirrored_output = mirrored;
        self.state.image_files_allowed = !mirrored;
    }

    /// Start an authoritative remote snapshot without carrying an incomplete
    /// escape sequence from the previous stream into the restored state.
    /// Keep the normal terminal configuration, clipboard and alert handlers.
    pub fn reset_for_remote_snapshot(&mut self) {
        self.state.cancel_resize_preview();
        self.parser = Parser::new();
        self.state.reset_remote_image_state();
        self.mirrored_actions.clear();self.mirrored_hold=None;self.mirrored_held_bytes=0;
        self.advance_bytes(b"\x1bc");
    }

    /// Feed the terminal parser a slice of bytes from the output
    /// of the associated program.
    /// The slice is not required to be a complete sequence of escape
    /// characters; it is valid to feed in chunks of data as they arrive.
    /// The output is parsed and applied to the terminal model.
    pub fn advance_bytes<B: AsRef<[u8]>>(&mut self, bytes: B) {
        if self.mirrored_output {
            self.advance_mirrored_bytes(bytes.as_ref());
            return;
        }
        self.state.increment_seqno();
        {
            let bytes = bytes.as_ref();

            let mut performer = Performer::new(&mut self.state);

            self.parser.parse(bytes, |action| performer.perform(action));
        }
        self.trigger_unseen_output_notif();
    }

    pub fn has_pending_synchronized_output(&self) -> bool { self.mirrored_hold.is_some() }

    pub fn flush_remote_output_if_due(&mut self) -> bool {
        if self.mirrored_hold.is_some_and(|since|since.elapsed()>=std::time::Duration::from_secs(1)) {
            self.mirrored_hold=None;self.flush_mirrored_actions();return true;
        }
        false
    }

    fn flush_mirrored_actions(&mut self) {
        self.mirrored_held_bytes=0;
        if !self.mirrored_actions.is_empty() {
            let actions=std::mem::take(&mut self.mirrored_actions);
            self.perform_actions(actions);
        }
    }

    fn advance_mirrored_bytes(&mut self, bytes: &[u8]) {
        use wezterm_escape_parser::{Action,csi::{CSI,Mode,DecPrivateMode,DecPrivateModeCode,Device}};
        let mut parsed=vec![];
        self.parser.parse(bytes,|action|action.append_to(&mut parsed));
        for action in parsed {
            match &action {
                Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::SynchronizedOutput))))=>{
                    if self.mirrored_hold.is_none() {self.flush_mirrored_actions();}
                    self.mirrored_hold=Some(std::time::Instant::now());
                },
                Action::CSI(CSI::Mode(Mode::ResetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::SynchronizedOutput))))=>{
                    self.mirrored_hold=None;
                },
                Action::CSI(CSI::Device(device)) if matches!(**device,Device::SoftReset)=>{self.mirrored_hold=None;},
                _=>{},
            }
            action.append_to(&mut self.mirrored_actions);
        }
        self.mirrored_held_bytes=self.mirrored_held_bytes.saturating_add(bytes.len());
        if self.mirrored_held_bytes>16*1024*1024 {self.mirrored_hold=None;}
        if self.mirrored_hold.is_none() {self.flush_mirrored_actions();}
        else {self.flush_remote_output_if_due();}
    }

    pub fn perform_actions(&mut self, actions: Vec<wezterm_escape_parser::Action>) {
        if self.mirrored_output {
            self.state.suppress_output_responses(true);
        }
        self.state.increment_seqno();
        {
            let mut performer = Performer::new(&mut self.state);
            for action in actions {
                performer.perform(action);
            }
        }
        if self.mirrored_output {
            self.state.suppress_output_responses(false);
        }
        self.trigger_unseen_output_notif();
    }
}
