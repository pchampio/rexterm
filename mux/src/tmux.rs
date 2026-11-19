use crate::activity::Activity;
use crate::domain::{alloc_domain_id, Domain, DomainId, DomainState, SplitSource};
use crate::pane::{Pane, PaneId};
use crate::tab::{SplitRequest, Tab, TabId};
use crate::tmux_commands::{
    ListAllWindows, ListCommands, NewWindow, SplitPane, TmuxCommand,
};
use crate::window::WindowId;
use crate::{Mux, MuxWindowBuilder};
use async_trait::async_trait;
use filedescriptor::FileDescriptor;
use parking_lot::{Condvar, Mutex};
use portable_pty::CommandBuilder;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::sync::Arc;
use termwiz::tmux_cc::*;
use wezterm_term::TerminalSize;

#[derive(PartialEq, Eq, Debug, Copy, Clone)]
pub enum AttachState {
    Init,
    Done,
}

#[derive(PartialEq, Eq, Debug, Copy, Clone)]
enum State {
    WaitForInitialGuard,
    Idle,
    WaitingForResponse,
    Exit,
}

#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct TmuxRemotePane {
    // members for local
    pub local_pane_id: PaneId,
    pub output_write: PaneOutput,
    pub active_lock: Arc<(Mutex<bool>, Condvar)>,
    // members sync with remote
    pub session_id: TmuxSessionId,
    pub window_id: TmuxWindowId,
    pub pane_id: TmuxPaneId,
    pub key_target: Option<TmuxPaneId>,
    pub mode_active: bool,
    pub mode_cursor_visible: bool,
    pub mode_name: String,
    pub mode_snapshot: Option<String>,
    pub mode_selection_snapshot: Option<String>,
    pub mode_render_cursor: Option<(usize, usize, bool, Option<isize>)>,
    pub mode_was_alt: bool,
    /// Copy/tree mode began but its first screen has not arrived yet. The
    /// switch to the mode screen (private mode 9727) is sent in the same
    /// synchronized frame as that screen; sent alone, it showed a blank
    /// screen for one tmux round trip (a flash on entering copy mode).
    pub mode_screen_pending: bool,
    /// An OSC 7377 sequence cut by the end of a %output line while the
    /// pane is in a mode (see split_audio_sequences).
    pub audio_carry: Vec<u8>,
    pub mode_backlog: Vec<u8>,
    /// The native pane was resized while a mode screen was displayed; its
    /// primary screen must be recaptured when the mode ends.
    pub mode_resized: bool,
    pub cursor_x: u64,
    pub cursor_y: u64,
    pub pane_width: u64,
    pub pane_height: u64,
    pub pane_left: u64,
    pub pane_top: u64,
}

pub(crate) type RefTmuxRemotePane = Arc<Mutex<TmuxRemotePane>>;

/// Mirrored pane output is written by a dedicated thread. A history capture
/// can exceed the socket buffer; a blocking write on the GUI or transport
/// thread stalled rendering and input until the pane's parser drained it.
/// A single channel per pane preserves the order of all writes.
#[derive(Debug)]
pub(crate) struct PaneOutput {
    tx: std::sync::mpsc::Sender<Vec<u8>>,
}

impl PaneOutput {
    pub(crate) fn new(mut fd: FileDescriptor) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let _ = std::thread::Builder::new()
            .name("tmux-pane-output".into())
            .spawn(move || {
                while let Ok(mut data) = rx.recv() {
                    while data.len() < 1 << 20 {
                        match rx.try_recv() {
                            Ok(more) => data.extend_from_slice(&more),
                            Err(_) => break,
                        }
                    }
                    if fd.write_all(&data).is_err() {
                        break;
                    }
                }
            });
        Self { tx }
    }
}

impl Write for PaneOutput {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if !buf.is_empty() {
            self.tx.send(buf.to_vec()).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "tmux pane output closed")
            })?;
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Output received before a native pane exists. It is replayed when the pane
/// is created, so that its terminal modes and saved cursor match tmux.
pub(crate) const PANE_BACKLOG_LIMIT: usize = 512 * 1024;

/// As a remote TmuxTab, keeping the TmuxPanes ID
/// within the remote tab.
#[allow(dead_code)]
pub(crate) struct TmuxTab {
    pub tab_id: TabId, // local tab ID
    pub tmux_window_id: TmuxWindowId,
    pub layout_csum: String,
    pub panes: HashSet<TmuxPaneId>, // tmux panes within tmux window
}

pub(crate) type TmuxCmdQueue = VecDeque<Box<dyn TmuxCommand>>;
pub(crate) struct TmuxDomainState {
    pub pane_id: PaneId,     // ID of the original pane
    pub domain_id: DomainId, // ID of TmuxDomain
    state: Mutex<State>,
    pub cmd_queue: Arc<Mutex<TmuxCmdQueue>>,
    response_buffer: Mutex<Vec<Guarded>>,
    pub gui_window: Mutex<Option<MuxWindowBuilder>>,
    pub gui_tabs: Mutex<HashMap<TmuxWindowId, TmuxTab>>,
    pub remote_panes: Mutex<HashMap<TmuxPaneId, RefTmuxRemotePane>>,
    pub tmux_session: Mutex<Option<TmuxSessionId>>,
    pub active_remote_window: Mutex<Option<TmuxWindowId>>,
    pub active_remote_panes: Mutex<HashMap<TmuxWindowId, TmuxPaneId>>,
    pub ui_overlay: Mutex<Option<TmuxPaneId>>,
    pub ui_prompt: Mutex<Option<TmuxPaneId>>,
    pub transport_tab: Mutex<Option<TabId>>,
    pub client_title: Mutex<Option<String>>,
    pub window_tab_regions: Mutex<HashMap<TmuxWindowId, Vec<(usize, usize, TmuxWindowId)>>>,
    /// Window list for the GUI-drawn tab strip (see tmux_strip.rs).
    pub strip_windows: Mutex<Vec<crate::tmux_strip::StripWindow>>,
    pub strip_last_key: Mutex<Option<String>>,
    pub strip_pending_window: Mutex<Option<TmuxWindowId>>,
    /// tmux status message: (text, message-style, shown until).
    pub client_message: Mutex<Option<(String, String, std::time::Instant)>>,
    /// Last strip drawn without a switch in flight: (window, when, line, regions).
    pub strip_last: Mutex<Option<(TmuxWindowId, std::time::Instant, (usize, Vec<(usize, usize)>), Vec<(usize, wezterm_term::Line)>, Vec<crate::tmux_strip::StripRegion>)>>,
    pub client_size: Mutex<Option<(usize, usize, usize, usize)>>,
    pub support_commands: Mutex<HashMap<String, String>>,
    pub attach_state: Mutex<AttachState>,
    pending_splits: Mutex<VecDeque<promise::Promise<TmuxPaneId>>>,
    pub backlog: Mutex<HashMap<TmuxPaneId, Vec<u8>>>,
    /// The pane (id, left, top, width, height) in which a left-button drag
    /// started. Motion is clamped into it, so tmux's own edge auto-scroll
    /// keeps working when the pointer leaves the pane or the window.
    pub mouse_drag: Mutex<Option<(TmuxPaneId, usize, usize, usize, usize)>>,
}

pub struct TmuxDomain {
    pub(crate) inner: Arc<TmuxDomainState>,
}

fn coalesce_snapshot(
    queue: &mut VecDeque<Box<dyn TmuxCommand>>,
    command: &dyn TmuxCommand,
    in_flight: usize,
) {
    let Some(key) = command.snapshot_key() else { return; };
    let mut index = 0;
    queue.retain(|queued| {
        let keep = index < in_flight || queued.snapshot_key() != Some(key);
        index += 1;
        keep
    });
}

impl TmuxDomainState {
    pub(crate) fn refresh_remote_windows(&self) {
        let session = *self.tmux_session.lock();
        if let Some(session_id) = session {
            self.enqueue_background_snapshot(Box::new(ListAllWindows { session_id, window_id: None }));
        }
    }

    pub(crate) fn enqueue_background_snapshot(&self, command: Box<dyn TmuxCommand>) {
        let mut queue = self.cmd_queue.lock();
        let in_flight = usize::from(*self.state.lock() == State::WaitingForResponse);
        coalesce_snapshot(&mut queue, &*command, in_flight);
        queue.push_back(command);
        drop(queue);
        Self::schedule_send_next_command(self.domain_id);
    }

    pub(crate) fn enqueue_client_refresh(&self, command: Box<dyn TmuxCommand>) {
        let mut queue = self.cmd_queue.lock();
        // Refresh between inputs, rather than waiting until a key-repeat or
        // wheel burst drains. The outstanding response still owns slot zero.
        let index = usize::from(*self.state.lock() == State::WaitingForResponse).min(queue.len());
        coalesce_snapshot(&mut queue, &*command, index);
        queue.insert(index, command);
        drop(queue);
        self.send_next_command(true);
        Self::schedule_send_next_command(self.domain_id);
    }

    pub(crate) fn enqueue_client_input(&self, command: Box<dyn TmuxCommand>) {
        let mut queue = self.cmd_queue.lock();
        // The front entry owns an outstanding response and must not move.
        // Preserve input order while avoiding a backlog of UI snapshots.
        let in_flight = usize::from(*self.state.lock() == State::WaitingForResponse);
        let after_input = queue.iter().rposition(|c| c.is_client_input())
            .map(|i| i + 1).unwrap_or(0);
        let index = in_flight.max(after_input).min(queue.len());
        // Only printable input into an already displayed popup can merge.
        // Never mutate the outstanding command or cross another input type.
        let merged = index > in_flight
            && (queue[index - 1].append_text_keys(&*command)
                || queue[index - 1].merge_mouse_motion(&*command));
        if !merged { queue.insert(index, command); }
        drop(queue);
        self.send_next_command(true);
        Self::schedule_send_next_command(self.domain_id);
    }

    pub(crate) fn has_exited(&self) -> bool {
        *self.state.lock() == State::Exit
    }
    pub fn advance(&self, events: Box<Vec<Event>>) {
        for event in events.iter() {
            let state = *self.state.lock();
            log::debug!("tmux: {:?} in state {:?}", event, state);
            match event {
                // Tmux generic events
                Event::Guarded(response) => match state {
                    State::WaitForInitialGuard => {
                        *self.state.lock() = State::Idle;
                    }
                    State::WaitingForResponse => {
                        // Server-side bindings emit independent flag-0 guards.
                        // Only flag-1 guards acknowledge this command queue.
                        let mut cmd_queue = self.cmd_queue.as_ref().lock();
                        let count = cmd_queue.front().map(|cmd| cmd.response_count()).unwrap_or(1);
                        let Some(resp) = collect_response(&mut self.response_buffer.lock(), response, count) else {
                            continue;
                        };
                        if let Some(cmd) = cmd_queue.pop_front() {
                            drop(cmd_queue);
                            let domain_id = self.domain_id;
                            *self.state.lock() = State::Idle;
                            let inline = cmd.inline_result();
                            let process = move || {
                                if let Err(err) = cmd.process_result(domain_id, &resp) {
                                    log::error!("Tmux processing command result error: {}", err);
                                }
                            };
                            if inline { process(); } else {
                                promise::spawn::spawn_into_main_thread(async move { process(); }).detach();
                            }
                        }
                    }
                    State::Idle => {}
                    State::Exit => {}
                },

                // Tmux specific events
                Event::ConfigError { error } => {
                    // tmux config file error, not our fault, just log it and go
                    log::warn!("tmux configuration error: {error}");
                }
                Event::Exit { reason: _ } => {
                    // Child exits and the DCS-end handler can prune the
                    // window before the main-thread transport restoration.
                    let restore_activity = Activity::new();
                    *self.state.lock() = State::Exit;
                    let mut pane_map = self.remote_panes.lock();
                    for (_, v) in pane_map.iter_mut() {
                        let remote_pane = v.lock();
                        let (lock, condvar) = &*remote_pane.active_lock;
                        let mut released = lock.lock();
                        *released = true;
                        condvar.notify_all();
                    }
                    let mut cmd_queue = self.cmd_queue.as_ref().lock();
                    cmd_queue.clear();

                    // Force to quit the tmux mode
                    let pane_id = self.pane_id;
                    let transport = *self.transport_tab.lock();
                    let window = self.gui_window.lock().as_ref().map(|w| **w);
                    let native_tabs: Vec<_> = self.gui_tabs.lock().values().map(|tab| tab.tab_id).collect();
                    promise::spawn::spawn_into_main_thread_with_low_priority(async move {
                        let _restore_activity = restore_activity;
                        let mux = Mux::get();
                        if let (Some(tab), Some(window)) =
                            (transport.and_then(|id| mux.get_tab(id)), window)
                        {
                            if mux.get_window(window).is_some() {
                                let _ = mux.add_tab_to_window(&tab, window);
                                if let Some(mut gui) = mux.get_window_mut(window) {
                                    if let Some(index) = gui.get_tab_idx_for_id(tab.tab_id()) {
                                        gui.set_active_tab_idx_without_saving(index);
                                    }
                                }
                                // Hidden local popups keep a tab alive after
                                // its tmux mirrors exit. Retire those tabs and
                                // their local jobs, then show the transport.
                                for id in native_tabs {
                                    if id != tab.tab_id() { mux.remove_tab(id); }
                                }
                                mux.notify(crate::MuxNotification::WindowInvalidated(window));
                            } else {
                                // Release the hidden transport when its native
                                // window closes, leaving server jobs alive.
                                mux.remove_tab(tab.tab_id());
                                return;
                            }
                        }
                        if let Some(x) = mux.get_pane(pane_id) {
                            let _ = write!(x.writer(), "\n\n");
                        }
                    })
                    .detach();

                    return;
                }
                Event::LayoutChange { window, .. } => {
                    self.refresh_remote_windows();
                    self.refresh_window_strip();
                    self.prefetch_window_chrome(*window);
                }
                Event::Output { pane, text } => {
                    let pane_map = self.remote_panes.lock();
                    if let Some(ref_pane) = pane_map.get(pane) {
                        let mut tmux_pane = ref_pane.lock();
                        if tmux_pane.mode_active {
                            // Sound must not wait for copy mode to end: complete
                            // OSC 7377 sequences go to the terminal at once (they
                            // do not touch the screen); the rest waits.
                            // tmux splits one sequence over several %output
                            // lines: an unfinished one waits in audio_carry.
                            let mut text_with_carry = std::mem::take(&mut tmux_pane.audio_carry);
                            text_with_carry.extend_from_slice(text);
                            let (audio, mut rest) = split_audio_sequences(&text_with_carry);
                            if let Some(start) = unfinished_audio_sequence(&rest) {
                                tmux_pane.audio_carry = rest.split_off(start);
                            }
                            if !audio.is_empty() {
                                let _ = tmux_pane.output_write.write_all(&audio);
                            }
                            tmux_pane.mode_backlog.extend_from_slice(&rest);
                            continue;
                        }
                        if let Err(err) = tmux_pane.output_write.write_all(text) {
                            log::error!("Failed to write tmux data to output: {:#}", err);
                        }
                    } else {
                        // the output may come early then pane is ready, in this case we
                        // backlog it
                        let mut backlog = self.backlog.lock();
                        let entry = backlog.entry(*pane).or_default();
                        // A truncated replay is still followed by an exact
                        // capture; only modes/saved cursor may be missing.
                        if entry.len() + text.len() <= PANE_BACKLOG_LIMIT {
                            entry.extend_from_slice(text);
                        }
                        log::debug!("Tmux pane {} haven't been attached", pane);
                    }
                }
                Event::SessionChanged { session, name: _ } => {
                    let previous = self.tmux_session.lock().replace(*session);
                    if previous == Some(*session) {
                        continue;
                    }
                    self.refresh_window_strip();
                    if previous.is_none() {
                        self.cmd_queue.lock().push_back(Box::new(ListCommands));
                        self.subscribe_notification();
                    } else {
                        self.cmd_queue.lock().push_back(Box::new(ListAllWindows {
                            session_id: *session,
                            window_id: None,
                        }));
                    }
                    log::info!("tmux session changed:{}", session);
                }
                Event::WindowAdd { .. } => {
                    self.refresh_remote_windows();
                    self.refresh_window_strip();
                }
                Event::WindowClose { .. } | Event::UnlinkedWindowClose { .. } => {
                    // A closed temporary window may have just returned its
                    // pane elsewhere. Both close notifications must reconcile
                    // ownership before retirement; an unlinked close can
                    // arrive before the pane-swap snapshot is installed.
                    self.refresh_remote_windows();
                    self.refresh_window_strip();
                }
                Event::SessionWindowChanged { session, window } => {
                    if *self.tmux_session.lock() == Some(*session) {
                        self.refresh_window_strip();
                        let domain_id = self.domain_id;
                        let window = *window;
                        promise::spawn::spawn_into_main_thread(async move {
                            if let Some(domain) = Mux::get().get_domain(domain_id) {
                                if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                                    tmux.inner.focus_remote_window(window);
                                }
                            }
                        })
                        .detach();
                    }
                }
                Event::WindowPaneChanged { window, pane } => {
                    // The tmux 2.7 WindowPaneChanged event comes early than WindowAdd, we need to
                    // skip it
                    if !self.check_window_attached(*window) {
                        continue;
                    }

                    // Split pane
                    if !self.check_pane_attached(*window, *pane) {
                        let mut pending_splits = self.pending_splits.lock();
                        if let Some(mut promise) = pending_splits.pop_front() {
                            promise.ok(*pane);
                        }
                    }
                    self.active_remote_panes.lock().insert(*window, *pane);
                    self.refresh_remote_windows();
                    log::info!("tmux window pane changed: {}:{}", window, pane);
                }
                // Paste buffers stay inside tmux (nvim's tmux clipboard
                // provider, copy-mode copies with -C). Only what tmux itself
                // would send to a terminal as OSC 52 reaches the desktop.
                Event::SetClipboard { data } => {
                    if let Some(p) = self
                        .remote_panes
                        .lock()
                        .values()
                        .find(|p| p.lock().key_target.is_none())
                    {
                        if let Err(err) =
                            write!(p.lock().output_write, "\x1b]52;c;{}\x07", data)
                        {
                            log::error!("tmux clipboard: {:#}", err);
                        }
                    }
                }
                // The user's prefix-a menu: display-message tmcc:expose.
                Event::Message { message } if message.trim() == "tmcc:expose" => {
                    self.request_window_overview();
                }
                Event::Message { message } => {
                    self.show_client_message(message);
                }
                Event::PaneModeChanged { pane } => {
                    self.cmd_queue
                        .lock()
                        .push_back(Box::new(crate::tmux_keys::InspectMode { pane: *pane }));
                }
                Event::WindowRenamed { window, name } => {
                    self.refresh_window_strip();
                    let gui_tabs = self.gui_tabs.lock();
                    if let Some(x) = gui_tabs.get(&window) {
                        let mux = Mux::get();
                        if let Some(tab) = mux.get_tab(x.tab_id) {
                            tab.set_title(&format!("{}", name));
                        }
                    }
                }
                _ => {}
            }
        }

        // send pending commands to tmux
        self.send_next_command(true);
        let cmd_queue = self.cmd_queue.as_ref().lock();
        if *self.state.lock() == State::Idle && !cmd_queue.is_empty() {
            TmuxDomainState::schedule_send_next_command(self.domain_id);
        }
    }

    /// send next command at the front of cmd_queue.
    /// General commands run on the main thread; input/copy reads may advance
    /// on the transport reader. The queue lock serializes both send paths.
    fn send_next_command(&self, immediate_only: bool) {
        let mut cmd_queue = self.cmd_queue.as_ref().lock();
        if *self.state.lock() != State::Idle {
            return;
        }
        while let Some(first) = cmd_queue.front() {
            if immediate_only && !first.immediate() { break; }
            let cmd = first.get_command(self.domain_id);
            if cmd.is_empty() {
                cmd_queue.pop_front();
                continue;
            }
            self.response_buffer.lock().clear();
            log::debug!("sending cmd {:?}", cmd);
            let mux = Mux::get();
            if let Some(pane) = mux.get_pane(self.pane_id) {
                let mut writer = pane.writer();
                // tmux -CC enables ICRNL on its tty. Use the terminal's
                // carriage-return framing so tssh's transfer filter can also
                // recognize and forward control commands during a transfer.
                // A newline is accepted by tmux, but buffered indefinitely by
                // that filter, leaving our pending RPC without a response.
                let _ = write!(writer, "{}\r", cmd.strip_suffix('\n').unwrap_or(&cmd));
            }
            *self.state.lock() = State::WaitingForResponse;
            break;
        }
    }

    /// schedule a `send_next_command` into main thread
    pub fn schedule_send_next_command(domain_id: usize) {
        promise::spawn::spawn_into_main_thread(async move {
            let mux = Mux::get();
            if let Some(domain) = mux.get_domain(domain_id) {
                if let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() {
                    tmux_domain.send_next_command();
                }
            }
        })
        .detach();
    }

    /// create a standalone window for tmux tabs
    pub fn create_gui_window(&self) {
        if self.gui_window.lock().is_none() {
            let mux = Mux::get();
            let window_builder =
                if let Some((_domain, window_id, tab)) = mux.resolve_pane_id(self.pane_id) {
                    *self.transport_tab.lock() = Some(tab);
                    MuxWindowBuilder {
                        window_id,
                        activity: Some(Activity::new()),
                        notified: false,
                    }
                } else {
                    mux.new_empty_window(
                        None, /* TODO: pass session here */
                        None, /* position */
                    )
                };

            log::info!("Tmux create window id {}", window_builder.window_id);
            {
                let mut window_id = self.gui_window.lock();
                *window_id = Some(window_builder); // keep the builder so it won't be purged
            }
        };
    }

    /// create a tmux window
    pub fn create_tmux_window(&self) {
        let mut cmd_queue = self.cmd_queue.as_ref().lock();
        cmd_queue.push_back(Box::new(NewWindow));
        TmuxDomainState::schedule_send_next_command(self.domain_id);
    }

    /// split the tmux pane
    pub fn split_tmux_pane(
        &self,
        _tab: TabId,
        pane_id: PaneId,
        split_request: SplitRequest,
    ) -> anyhow::Result<()> {
        let tmux_pane_id = self
            .remote_panes
            .lock()
            .iter()
            .find(|(_, ref_pane)| ref_pane.lock().local_pane_id == pane_id)
            .map(|p| p.1.lock().pane_id);

        if let Some(id) = tmux_pane_id {
            let mut cmd_queue = self.cmd_queue.as_ref().lock();
            cmd_queue.push_back(Box::new(SplitPane {
                pane_id: id,
                direction: split_request.direction,
            }));
            TmuxDomainState::schedule_send_next_command(self.domain_id);
            return Ok(());
        } else {
            anyhow::bail!("Could not find the tmux pane peer for local pane: {pane_id}");
        }
    }
}

impl TmuxDomain {
    pub fn transport_pane_id(&self) -> PaneId {
        self.inner.pane_id
    }

    pub fn client_title(&self) -> Option<String> {
        self.inner.client_title.lock().clone()
    }
    pub fn new(pane_id: PaneId) -> Self {
        let domain_id = alloc_domain_id();
        let cmd_queue = VecDeque::new();
        let inner = Arc::new(TmuxDomainState {
            domain_id,
            pane_id,
            // parser,
            state: Mutex::new(State::WaitForInitialGuard),
            cmd_queue: Arc::new(Mutex::new(cmd_queue)),
            response_buffer: Mutex::new(Vec::new()),
            gui_window: Mutex::new(None),
            gui_tabs: Mutex::new(HashMap::default()),
            remote_panes: Mutex::new(HashMap::default()),
            tmux_session: Mutex::new(None),
            active_remote_window: Mutex::new(None),
            active_remote_panes: Mutex::new(HashMap::new()),
            ui_overlay: Mutex::new(None),
            ui_prompt: Mutex::new(None),
            transport_tab: Mutex::new(None),
            client_title: Mutex::new(None),
            window_tab_regions: Mutex::new(HashMap::new()),
            strip_windows: Mutex::new(Vec::new()),
            strip_last_key: Mutex::new(None),
            strip_pending_window: Mutex::new(None),
            client_message: Mutex::new(None),
            strip_last: Mutex::new(None),
            client_size: Mutex::new(None),
            support_commands: Mutex::new(HashMap::default()),
            attach_state: Mutex::new(AttachState::Init),
            pending_splits: Mutex::new(VecDeque::default()),
            backlog: Mutex::new(HashMap::default()),
            mouse_drag: Mutex::new(None),
        });

        Self { inner }
    }

    fn send_next_command(&self) {
        self.inner.send_next_command(false);
    }
}

#[async_trait(?Send)]
impl Domain for TmuxDomain {
    async fn spawn(
        &self,
        _size: TerminalSize,
        _command: Option<CommandBuilder>,
        _command_dir: Option<String>,
        _window: WindowId,
    ) -> anyhow::Result<Arc<Tab>> {
        self.inner.create_tmux_window();
        // This is intention, we would not return a Tab, since we don't have now!
        // We use create_tmux_window to create back end tmux window, then the
        // Tmux WindowAdd event will triage us to do the rest things.
        anyhow::bail!("Intention: we use tmux command to do so");
    }

    async fn split_pane(
        &self,
        _source: SplitSource,
        tab: TabId,
        pane_id: PaneId,
        split_request: SplitRequest,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let mut promise = promise::Promise::new();
        if let Some(future) = promise.get_future() {
            {
                let mut pending_splits = self.inner.pending_splits.lock();
                let _ = self.inner.split_tmux_pane(tab, pane_id, split_request)?;
                pending_splits.push_back(promise);
            }

            if let Ok(id) = future.await {
                let pane = self.inner.split_pane(tab, pane_id, id, split_request);
                return pane;
            }
        }

        anyhow::bail!("Split_pane failed");
    }

    async fn spawn_pane(
        &self,
        _size: TerminalSize,
        _command: Option<CommandBuilder>,
        _command_dir: Option<String>,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        anyhow::bail!("Spawn_pane not yet implemented for TmuxDomain");
    }

    fn domain_id(&self) -> DomainId {
        self.inner.domain_id
    }

    fn domain_name(&self) -> &str {
        "tmux"
    }

    async fn attach(&self, _window_id: Option<crate::WindowId>) -> anyhow::Result<()> {
        Ok(())
    }

    fn detachable(&self) -> bool {
        true
    }

    fn detach(&self) -> anyhow::Result<()> {
        if !self.inner.has_exited() {
            self.inner
                .enqueue_client_input(Box::new(crate::tmux_keys::DetachClient));
        }
        Ok(())
    }

    fn state(&self) -> DomainState {
        if self.inner.has_exited() {
            DomainState::Detached
        } else {
            DomainState::Attached
        }
    }
}

// A tmux command list returns one guard per command; bindings can interleave
// their own flag-0 guards. Keep the queue slot until the entire list completes.
fn collect_response(buffer: &mut Vec<Guarded>, response: &Guarded, count: usize) -> Option<Guarded> {
    if response.flags & 1 == 0 { return None; }
    buffer.push(response.clone());
    if !response.error && buffer.len() < count { return None; }
    let mut combined = response.clone();
    combined.output = buffer.iter().map(|r| r.output.as_str()).collect();
    combined.error = buffer.iter().any(|r| r.error);
    buffer.clear();
    Some(combined)
}

#[cfg(test)]
mod response_tests {
    use super::*;
    #[derive(Debug)]
    struct SnapshotTest(&'static str, Option<(&'static str, TmuxPaneId)>);
    impl TmuxCommand for SnapshotTest {
        fn snapshot_key(&self) -> Option<(&'static str, TmuxPaneId)> { self.1 }
        fn get_command(&self, _: DomainId) -> String { self.0.to_owned() }
        fn process_result(&self, _: DomainId, _: &Guarded) -> anyhow::Result<()> { Ok(()) }
    }
    #[test]
    fn snapshot_coalescing_preserves_outstanding_response_inputs_and_other_surfaces() {
        let mut queue: VecDeque<Box<dyn TmuxCommand>> = vec![
            SnapshotTest("outstanding", Some(("frame", 1))),
            SnapshotTest("key-one", None),
            SnapshotTest("obsolete", Some(("frame", 1))),
            SnapshotTest("other-pane", Some(("frame", 2))),
            SnapshotTest("prompt", Some(("prompt", 1))),
            SnapshotTest("key-two", None),
        ].into_iter().map(|c| Box::new(c) as Box<dyn TmuxCommand>).collect();
        coalesce_snapshot(&mut queue, &SnapshotTest("new", Some(("frame", 1))), 1);
        assert_eq!(queue.iter().map(|c| c.get_command(0)).collect::<Vec<_>>(),
            ["outstanding", "key-one", "other-pane", "prompt", "key-two"]);
    }
    #[test]
    fn idle_snapshot_coalescing_replaces_front_but_never_coalesces_input() {
        let mut queue: VecDeque<Box<dyn TmuxCommand>> = vec![
            SnapshotTest("old-overlay", Some(("overlay", 0))),
            SnapshotTest("key", None),
        ].into_iter().map(|c| Box::new(c) as Box<dyn TmuxCommand>).collect();
        coalesce_snapshot(&mut queue, &SnapshotTest("new-key", None), 0);
        assert_eq!(queue.len(), 2);
        coalesce_snapshot(&mut queue, &SnapshotTest("new-overlay", Some(("overlay", 0))), 0);
        assert_eq!(queue.front().unwrap().get_command(0), "key");
        assert_eq!(queue.len(), 1);
    }
    fn guard(text: &str, flags: i64, error: bool) -> Guarded {
        Guarded { error, timestamp: 0, number: 1, flags, output: text.to_owned() }
    }
    #[test]
    fn compound_capture_ignores_binding_guards_and_keeps_order() {
        let mut buffer = vec![];
        assert!(collect_response(&mut buffer, &guard("42\n", 1, false), 2).is_none());
        assert!(collect_response(&mut buffer, &guard("binding\n", 0, false), 2).is_none());
        let r = collect_response(&mut buffer, &guard("@ui 0 1 2\nbody\n", 1, false), 2).unwrap();
        assert_eq!(r.output, "42\n@ui 0 1 2\nbody\n");
        assert!(buffer.is_empty());
        assert!(!r.error);
    }
    #[test]
    fn compound_error_releases_slot_and_does_not_leak_into_next_command() {
        let mut buffer = vec![];
        let r = collect_response(&mut buffer, &guard("no pane\n", 1, true), 2).unwrap();
        assert!(r.error);
        assert!(buffer.is_empty());
        assert_eq!(collect_response(&mut buffer, &guard("next\n", 1, false), 1).unwrap().output, "next\n");
    }
}

/// Start of an OSC 7377 sequence that `rest` (output after
/// split_audio_sequences) ends with, still unterminated.
pub(crate) fn unfinished_audio_sequence(rest: &[u8]) -> Option<usize> {
    let starts = [&b"\x1bPtmux;\x1b\x1b]7377;"[..], &b"\x1b]7377;"[..]];
    let start = starts
        .iter()
        .filter_map(|needle| rest.windows(needle.len()).rposition(|w| w == *needle))
        .min()?;
    // Bound the carry: a stray marker must not hold output back for long.
    (rest.len() - start < 1 << 20).then_some(start)
}

/// Splits complete OSC 7377 audio sequences (plain, or wrapped in tmux
/// passthrough) out of pane output. Anything else, including a sequence cut
/// by the end of this chunk, stays in `rest` in its original order.
pub(crate) fn split_audio_sequences(text: &[u8]) -> (Vec<u8>, Vec<u8>) {
    const PLAIN: &[u8] = b"\x1b]7377;";
    const WRAPPED: &[u8] = b"\x1bPtmux;\x1b\x1b]7377;";
    let find = |hay: &[u8], needle: &[u8], from: usize| {
        hay.get(from..)?
            .windows(needle.len())
            .position(|w| w == needle)
            .map(|i| i + from)
    };
    let mut audio = vec![];
    let mut rest = vec![];
    let mut i = 0;
    while i < text.len() {
        let plain = find(text, PLAIN, i);
        let wrapped = find(text, WRAPPED, i);
        let (start, wrap) = match (plain, wrapped) {
            (Some(p), Some(w)) if w <= p => (w, true),
            (Some(p), _) => (p, false),
            (None, Some(w)) => (w, true),
            (None, None) => break,
        };
        let terminator: &[u8] = if wrap { b"\x1b\x1b\\\x1b\\" } else { b"\x1b\\" };
        let Some(end) = find(text, terminator, start + 2) else {
            break;
        };
        let end = end + terminator.len();
        rest.extend_from_slice(&text[i..start]);
        audio.extend_from_slice(&text[start..end]);
        i = end;
    }
    rest.extend_from_slice(&text[i..]);
    (audio, rest)
}

#[cfg(test)]
mod audio_split_tests {
    use super::split_audio_sequences;

    #[test]
    fn audio_leaves_copy_mode_backlog() {
        let wrapped = b"\x1bPtmux;\x1b\x1b]7377;a=d;i=1;AAAA\x1b\x1b\\\x1b\\";
        let mut text = b"ab".to_vec();
        text.extend_from_slice(wrapped);
        text.extend_from_slice(b"cd\x1b]7377;a=e;i=1\x1b\\ef\x1b]7377;a=d;i=1;cut");
        let (audio, rest) = split_audio_sequences(&text);
        let mut want = wrapped.to_vec();
        want.extend_from_slice(b"\x1b]7377;a=e;i=1\x1b\\");
        assert_eq!(audio, want);
        assert_eq!(rest, b"abcdef\x1b]7377;a=d;i=1;cut".to_vec());
        assert_eq!(super::unfinished_audio_sequence(&rest), Some(6));
        assert_eq!(super::unfinished_audio_sequence(b"plain"), None);
    }
}
