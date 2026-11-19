//! Client keys and paste stay symbolic until tmux interprets them.
//! Application protocol replies continue using the raw PTY writer.
use crate::pane::PaneId;
use crate::tmux::{TmuxDomain, TmuxDomainState};
use crate::tmux_commands::TmuxCommand;
use crate::{DomainId, Mux};
use std::convert::TryFrom;
use std::io::Write;
use termwiz::tmux_cc::{Guarded, TmuxPaneId, TmuxWindowId};
use wezterm_term::{KeyCode, KeyModifiers};

fn key_name(key: KeyCode, mods: KeyModifiers) -> Option<String> {
    if mods.contains(KeyModifiers::SUPER) {
        return None;
    }
    let key = key.normalize_shift_to_upper_case(mods);
    let mut shift = mods.contains(KeyModifiers::SHIFT);
    let name = match key {
        KeyCode::Char(' ') => "Space".into(),
        KeyCode::Char('\r') | KeyCode::Char('\n') | KeyCode::Enter => "Enter".into(),
        KeyCode::Char('\t') | KeyCode::Tab => "Tab".into(),
        KeyCode::Char('\x1b') | KeyCode::Escape => "Escape".into(),
        KeyCode::Char(c) if !c.is_control() => {
            shift = false;
            c.to_string()
        }
        KeyCode::Backspace => "BSpace".into(),
        KeyCode::Delete => "DC".into(),
        KeyCode::Insert => "IC".into(),
        KeyCode::LeftArrow => "Left".into(),
        KeyCode::RightArrow => "Right".into(),
        KeyCode::UpArrow => "Up".into(),
        KeyCode::DownArrow => "Down".into(),
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        KeyCode::PageUp => "PPage".into(),
        KeyCode::PageDown => "NPage".into(),
        KeyCode::Function(n) => format!("F{n}"),
        _ => return None,
    };
    Some(format!(
        "{}{}{}{}",
        if mods.contains(KeyModifiers::CTRL) {
            "C-"
        } else {
            ""
        },
        if mods.contains(KeyModifiers::ALT) {
            "M-"
        } else {
            ""
        },
        if shift { "S-" } else { "" },
        name
    ))
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

impl TmuxDomainState {
    fn refresh_client_pane(&self, pane: TmuxPaneId, scroll: bool) {
        let capture = self.remote_panes.lock().get(&pane).and_then(|p| {
            let p = p.lock();
            (scroll && p.mode_active && p.mode_name == "copy-mode").then_some(CaptureMode {
                pane, prompt: false, cursor_x: p.cursor_x as usize, cursor_y: p.cursor_y as usize,
            })
        });
        if let Some(capture) = capture {
            self.enqueue_client_refresh(Box::new(capture));
        } else {
            self.enqueue_client_refresh(Box::new(InspectMode { pane }));
            if !scroll && config::configuration().tmux_continuous_window_tabs {
                self.enqueue_background_snapshot(Box::new(CaptureFrame { pane, chrome: true }));
            }
        }
    }

    pub fn send_client_key(
        &self,
        local: PaneId,
        key: KeyCode,
        mods: KeyModifiers,
    ) -> anyhow::Result<bool> {
        // send-keys -K is negotiated rather than assumed on older tmux servers.
        if !self
            .support_commands
            .lock()
            .get("send-keys")
            .map(|s| s.contains('K'))
            .unwrap_or(false)
        {
            return Ok(false);
        }
        let Some(key) = key_name(key, mods) else {
            return Ok(false);
        };
        let overlay = *self.ui_overlay.lock();
        let remote = self.remote_panes.lock().values().find_map(|p| {
            let p = p.lock();
            (p.local_pane_id == local).then_some((p.key_target.unwrap_or(p.pane_id),
                overlay == Some(p.pane_id) && p.mode_cursor_visible))
        });
        let Some((pane, popup_text)) = remote else {
            return Ok(false);
        };
        // A popup shown as a native floating pane (new-pane -K) receives every
        // key directly, without bindings, so its printable keys merge too.
        let popup_text = popup_text
            || Mux::get()
                .get_pane(local)
                .map_or(false, |p| p.popup_frame().is_some());
        // A menu has no visible cursor. Keep all menu/prefix/mode keys as
        // separate dispatches: their bindings can asynchronously change the
        // next key's meaning. A visible popup receives ordinary text bytes.
        self.strip_key_sent(&key);
        if popup_text && (key.chars().count() == 1 || key == "Space") {
            self.enqueue_client_input(Box::new(ClientPaste { pane, keys: vec![key], coalescible: true }));
        } else {
            self.enqueue_client_input(Box::new(ClientKey { pane, key }));
        }
        Ok(true)
    }

    pub(crate) fn send_client_paste(&self, local: PaneId, text: &str) -> anyhow::Result<bool> {
        if !self
            .support_commands
            .lock()
            .get("send-keys")
            .map(|s| s.contains("-m"))
            .unwrap_or(false)
        {
            return Ok(false);
        }
        let remote = self.remote_panes.lock().values().find_map(|p| {
            let p = p.lock();
            (p.local_pane_id == local).then_some(p.key_target.unwrap_or(p.pane_id))
        });
        let Some(pane) = remote else {
            return Ok(false);
        };
        // tmux enables bracketed paste on its outer terminal regardless of
        // the application's mode. Preserve literal LF and filter embedded
        // paste delimiters, just as Terminal::send_paste does in that mode.
        let normalized = text.replace("\x1b[200~", "").replace("\x1b[201~", "");
        let mut keys = vec!["PasteStart".to_owned()];
        keys.extend(normalized.chars().map(|c| {
            if c.is_control() {
                format!("0x{:x}", c as u32)
            } else {
                key_name(KeyCode::Char(c), KeyModifiers::NONE).unwrap()
            }
        }));
        keys.push("PasteEnd".to_owned());
        for chunk in keys.chunks(1024) {
            self.enqueue_client_input(Box::new(ClientPaste {
                pane,
                keys: chunk.to_vec(),
                coalescible: false,
            }));
        }
        Ok(true)
    }

    pub fn focus_remote_window(&self, remote: TmuxWindowId) {
        *self.active_remote_window.lock() = Some(remote);
        // Prepare the target chrome before publishing the tab. Otherwise a
        // new tab exposes an empty surface, and an old tab exposes stale labels
        // until the next periodic capture. Input still follows the server.
        if config::configuration().tmux_continuous_window_tabs
            && self.support_commands.lock().get("capture-pane")
                .map(|v| v.contains('U')).unwrap_or(false)
        {
            let (pane, tab_id) = self.gui_tabs.lock().get(&remote)
                .map(|tab| (tab.panes.iter().next().copied(), Some(tab.tab_id)))
                .unwrap_or((None, None));
            if let Some(pane) = pane {
                self.enqueue_client_refresh(Box::new(CaptureFrame { pane, chrome: true }));
            }
            // Separators and the tab strip are drawn by the GUI from the
            // layout, so the switch shows at once; the capture only serves
            // the frames of tmux floating panes.
            let _ = tab_id;
            if self.strip_defer_window(remote) {
                return;
            }
        }
        self.present_remote_window(remote);
    }

    /// Captures the border chrome of a window that is not shown, so that a
    /// later switch to it shows its own borders at once.
    pub(crate) fn prefetch_window_chrome(&self, window: TmuxWindowId) {
        if !config::configuration().tmux_continuous_window_tabs
            || *self.active_remote_window.lock() == Some(window)
        {
            return;
        }
        let pane = self.gui_tabs.lock().get(&window).and_then(|tab| tab.panes.iter().next().copied());
        if let Some(pane) = pane {
            self.enqueue_background_snapshot(Box::new(CaptureFrame { pane, chrome: true }));
        }
    }

    pub(crate) fn present_remote_window(&self, remote: TmuxWindowId) {
        self.strip_window_shown(remote);
        let mux = Mux::get();
        let tab_id = self.gui_tabs.lock().get(&remote).map(|t| t.tab_id);
        if let Some(tab_id) = tab_id {
            if let Some(window_id) = mux.window_containing_tab(tab_id) {
                if let Some(mut window) = mux.get_window_mut(window_id) {
                    let idx = window.get_tab_idx_for_id(tab_id);
                    if let Some(idx) = idx {
                        if window.get_active_tab_idx() == idx { return; }
                        window.set_active_tab_idx_without_saving(idx);
                    }
                }
                mux.notify(crate::MuxNotification::WindowInvalidated(window_id));
            }
        }
    }
}

#[derive(Debug)]
struct ClientKey {
    pane: TmuxPaneId,
    key: String,
}

#[derive(Debug)]
pub(crate) struct DetachClient;
impl TmuxCommand for DetachClient {
    fn is_client_input(&self) -> bool {
        true
    }
    fn get_command(&self, _: DomainId) -> String {
        "detach-client\n".to_owned()
    }
    fn process_result(&self, _: DomainId, result: &Guarded) -> anyhow::Result<()> {
        anyhow::ensure!(!result.error, "tmux detach failed: {}", result.output);
        Ok(())
    }
}
impl TmuxCommand for ClientKey {
    fn inline_result(&self) -> bool { true }
    fn is_client_input(&self) -> bool {
        true
    }
    fn get_command(&self, _: DomainId) -> String {
        // The control connection identifies the client. Its current pane may
        // already differ from the last GUI snapshot (especially after closing
        // a pane), so do not validate a stale -t before dispatching client input.
        format!("send-keys -K {}\n", quote(&self.key))
    }
    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        anyhow::ensure!(!result.error, "tmux client key failed: {}", result.output);
        if let Some(domain) = Mux::get().get_domain(domain_id) {
            if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                let scroll = matches!(self.key.as_str(), "M-k" | "M-j");
                // Each key still gets its own ordered server dispatch. When
                // more input is waiting, avoid interleaving several UI reads
                // per character: over SSH that made typing fall behind.
                // The last key refreshes the current surface; scroll gestures
                // always refresh between inputs to preserve animation.
                let pending_input = tmux.inner.cmd_queue.lock().iter().any(|c| c.is_client_input());
                if !pending_input || scroll {
                    tmux.inner.refresh_client_pane(self.pane, scroll);
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct InspectMode {
    pub(crate) pane: TmuxPaneId,
}
impl TmuxCommand for InspectMode {
    fn snapshot_key(&self) -> Option<(&'static str, TmuxPaneId)> {
        Some(("inspect", self.pane))
    }
    fn get_command(&self, _: DomainId) -> String {
        format!(
            "display-message -p -t %{} '#{{pane_in_mode}} #{{client_prompt_active}} #{{pane-border-status}} #{{?#{{==:#{{pane_mode}},copy-mode}},#{{copy_cursor_x}},#{{cursor_x}}}} #{{?#{{==:#{{pane_mode}},copy-mode}},#{{copy_cursor_y}},#{{cursor_y}}}} #{{client_overlay_geometry}} #{{client_prompt_cursor}} #{{pane_floating_flag}} #{{alternate_on}} #{{cursor_flag}} #{{pane_mode}}'\n",
            self.pane
        )
    }
    fn process_result(&self, id: DomainId, r: &Guarded) -> anyhow::Result<()> {
        if r.error {
            return Ok(());
        } // Pane may have been killed by the binding.
        let mux = Mux::get();
        let Some(domain) = mux.get_domain(id) else {
            return Ok(());
        };
        let Some(tmux) = domain.downcast_ref::<TmuxDomain>() else {
            return Ok(());
        };
        let mut fields = r.output.split_whitespace();
        let in_mode = fields.next() == Some("1");
        let prompt = fields.next() == Some("1");
        let border = fields.next() == Some("bottom");
        let cursor_x = fields
            .next()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        let cursor_y = fields
            .next()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        if let Some(geometry) = fields.next().filter(|v| *v != "0") {
            let values: Vec<usize> = geometry.split(',').filter_map(|v| v.parse().ok()).collect();
            if let Ok(geometry) = <[usize; 6]>::try_from(values) {
                tmux.inner
                    .enqueue_client_refresh(Box::new(CaptureOverlay {
                        pane: self.pane,
                        geometry,
                    }));
                TmuxDomainState::schedule_send_next_command(id);
            }
        } else {
            tmux.inner.update_ui_overlay(self.pane, None, &[])?;
        }
        let panes = tmux.inner.remote_panes.lock();
        let Some(pane) = panes.get(&self.pane) else {
            return Ok(());
        };
        let mut pane = pane.lock();
        let prompt_cursor = fields.next().and_then(|v| v.parse::<usize>().ok());
        let floating = fields.next() == Some("1");
        let alternate = fields.next() == Some("1");
        let cursor_visible = fields.next() != Some("0");
        pane.mode_cursor_visible = cursor_visible;
        pane.mode_name = fields.next().unwrap_or("").to_owned();
        if floating {
            tmux.inner
                .enqueue_background_snapshot(Box::new(CaptureFrame {
                    pane: self.pane,
                    chrome: false,
                }));
            TmuxDomainState::schedule_send_next_command(id);
        }
        if (border && !floating) || prompt {
            let capture = Box::new(CapturePrompt {
                pane: self.pane,
                prompt,
                cursor: prompt_cursor.unwrap_or(0),
            });
            // Status rows go ahead of long queued reads (history captures
            // after attaching), but never ahead of mode screens: copy-mode
            // scroll animation needs each new screen as soon as possible.
            if in_mode {
                tmux.inner.enqueue_background_snapshot(capture);
            } else {
                tmux.inner.enqueue_client_refresh(capture);
            }
            TmuxDomainState::schedule_send_next_command(id);
        }
        if in_mode {
            if !pane.mode_active {
                pane.mode_snapshot = None;
                pane.mode_render_cursor = None;
                pane.mode_was_alt = alternate;
                pane.mode_resized = false;
                // Private mode 9727 shows the mode screen without DECSC: the
                // application's saved cursor must survive copy mode. It is
                // sent with the first mode screen (CaptureMode), atomically.
                pane.mode_screen_pending = true;
                pane.mode_active = true;
            }
            tmux.inner.enqueue_client_refresh(Box::new(CaptureMode {
                pane: self.pane,
                prompt: false,
                cursor_x,
                cursor_y,
            }));
            TmuxDomainState::schedule_send_next_command(id);
        } else if pane.mode_active {
            pane.output_write.write_all(b"\x1b]1337;SetUserVar=WEZTERM_TMUX_COPY_SCROLL=\x07")?;
            // A mode that ended before its first screen never replaced the
            // application screen.
            if !std::mem::take(&mut pane.mode_screen_pending) {
                pane.output_write.write_all(b"\x1b[?9727l")?;
            }
            pane.output_write.write_all(if cursor_visible { b"\x1b[?25h" } else { b"\x1b[?25l" })?;
            pane.mode_active = false;
            let mut backlog = std::mem::take(&mut pane.mode_backlog);
            backlog.append(&mut pane.audio_carry);
            pane.output_write.write_all(&backlog)?;
            pane.cursor_x = cursor_x as u64;
            pane.cursor_y = cursor_y as u64;
            // The primary screen was preserved under the mode screen and the
            // output received meanwhile was replayed in order, so it already
            // matches tmux. Recapturing it would erase inline images (imgcat).
            // An alternate-screen application was drawn over, and a resize
            // reflowed tmux independently: those cases are recaptured.
            if pane.mode_was_alt || pane.mode_resized {
                tmux.inner
                    .cmd_queue
                    .lock()
                    .push_back(Box::new(crate::tmux_commands::CapturePane {
                        pane_id: self.pane,
                        history_limit: 0,
                    }));
                TmuxDomainState::schedule_send_next_command(id);
            }
        } else if let Some(local) = mux.get_pane(pane.local_pane_id) {
            if local.is_alt_screen_active() != alternate {
                let screen: &[u8] = if alternate {
                    b"\x1b[?1049h"
                } else {
                    b"\x1b[?1049l"
                };
                pane.output_write.write_all(screen)?;
                pane.cursor_x = cursor_x as u64;
                pane.cursor_y = cursor_y as u64;
                tmux.inner.cmd_queue.lock().push_back(Box::new(
                    crate::tmux_commands::CapturePane {
                        pane_id: self.pane,
                        history_limit: 0,
                    },
                ));
                TmuxDomainState::schedule_send_next_command(id);
            }
        }
        Ok(())
    }
}

// ncurses extends DEC graphics with arrows and a solid block. tmux emits
// their ACS bytes in captures but UTF-8 glyphs to an ordinary UTF-8 client.
pub(crate) fn tmux_acs_extensions(text: &str) -> String {
    use termwiz::escape::{Action, ControlCode};
    use std::fmt::Write;
    let mut parser = termwiz::escape::parser::Parser::new();
    let mut shifted = false;
    let mut result = String::new();
    parser.parse(text.as_bytes(), |action| {
        match &action {
            Action::Control(ControlCode::ShiftOut) => shifted = true,
            Action::Control(ControlCode::ShiftIn) => shifted = false,
            Action::Print(c) if shifted => {
                result.push(match c { '+' => '→', ',' => '←', '-' => '↑', '.' => '↓', '0' => '▮', other => *other });
                return;
            }
            _ => {}
        }
        write!(result, "{}", action).unwrap();
    });
    result
}

fn mode_selection_record<'a>(header: &str, body: &'a str) -> (&'a str, &'a str) {
    if header.split_whitespace().nth(4) != Some("selection-layers") { return ("", body); }
    if let Some(rest) = body.strip_prefix("@selection ") {
        rest.split_once('\n').unwrap_or((rest, ""))
    } else { ("", body) }
}

#[derive(Debug)]
struct CaptureMode {
    pane: TmuxPaneId,
    prompt: bool,
    cursor_x: usize,
    cursor_y: usize,
}
impl TmuxCommand for CaptureMode {
    fn immediate(&self) -> bool { true }
    fn inline_result(&self) -> bool { true }
    fn response_count(&self) -> usize { 2 }
    fn get_command(&self, _: DomainId) -> String {
        format!("display-message -p -t %{} '#{{?pane_in_mode,#{{scroll_position}},}}' ; capture-pane -p -e -C -N -M -V -Y -t %{}\n", self.pane, self.pane)
    }
    fn process_result(&self, id: DomainId, r: &Guarded) -> anyhow::Result<()> {
        if r.error {
            return Ok(());
        }
        let mux = Mux::get();
        let Some(domain) = mux.get_domain(id) else {
            return Ok(());
        };
        let Some(tmux) = domain.downcast_ref::<TmuxDomain>() else {
            return Ok(());
        };
        let (scroll, capture) = r.output.split_once('\n').unwrap_or(("", &r.output));
        let (header, body) = capture.split_once('\n').unwrap_or(("", capture));
        let values: Vec<_> = header.split_whitespace().collect();
        let (selection, body) = mode_selection_record(header, body);
        let cursor_x = values
            .get(2)
            .and_then(|v| v.parse().ok())
            .unwrap_or(self.cursor_x);
        let cursor_y = values
            .get(3)
            .and_then(|v| v.parse().ok())
            .unwrap_or(self.cursor_y);
        let text = tmux_acs_extensions(&termwiz::tmux_cc::unvis(body)?);
        let text = text
            .strip_suffix('\n')
            .unwrap_or(&text)
            .replace('\n', "\r\n");
        if let Some(pane) = tmux.inner.remote_panes.lock().get(&self.pane) {
            let mut pane = pane.lock();
            if pane.mode_active {
                if pane.mode_name == "copy-mode" && scroll.trim().is_empty() {
                    // A custom scroll binding may have left copy mode. Its
                    // mode-change notification will restore the application.
                    return Ok(());
                }
                // Tree modes keep x=0 on the selected row with the cursor
                // hidden; their internal search/filter prompt moves x past
                // its label. cursor_flag itself describes the application,
                // not this mode screen. Clock mode also hides the cursor.
                let visible = match pane.mode_name.as_str() {
                    "tree-mode" | "buffer-mode" | "client-mode" => cursor_x > 0,
                    "clock-mode" => false,
                    "copy-mode" | "view-mode" => true,
                    _ => pane.mode_cursor_visible,
                };
                let cursor = (cursor_x, cursor_y, visible, scroll.trim().parse().ok());
                let enter = std::mem::take(&mut pane.mode_screen_pending);
                let unchanged = !enter && pane.mode_snapshot.as_ref() == Some(&text);
                if unchanged && pane.mode_render_cursor == Some(cursor) && pane.mode_selection_snapshot.as_deref() == Some(selection) {
                    return Ok(());
                }
                // Avoid repainting an unchanged mode at the polling rate.
                // Otherwise idle full-screen redraws delay the next input and
                // hide intermediate steps of short scroll animations.
                let body = if unchanged { String::new() } else {
                    format!("\x0f\x1b)0\x1b[0m\x1b[H\x1b[2J{}", text)
                };
                // Carry the position through the same parser stream as the
                // cells: GUI animation must never observe metadata ahead of
                // the corresponding screen snapshot.
                use base64::Engine;
                let scroll = if pane.mode_name == "copy-mode" { scroll.trim() } else { "" };
                let marker = base64::engine::general_purpose::STANDARD.encode(scroll);
                let selection_marker = base64::engine::general_purpose::STANDARD.encode(selection);
                let frame = format!(
                    "\x1b[?2026h{}{}\x1b[{};{}H{}\x1b]1337;SetUserVar=WEZTERM_TMUX_COPY_SCROLL={}\x07\x1b]1337;SetUserVar=WEZTERM_TMUX_COPY_SELECTION={}\x07\x1b[?2026l",
                    if enter { "\x1b[?9727h" } else { "" }, body, cursor_y + 1, cursor_x + 1,
                    if visible { "\x1b[?25h" } else { "\x1b[?25l" }, marker, selection_marker
                );
                pane.output_write.write_all(frame.as_bytes())?;
                pane.mode_selection_snapshot = Some(selection.to_string());
                pane.mode_snapshot = Some(text);
                pane.mode_render_cursor = Some(cursor);
            }
        }
        if self.prompt {
            tmux.inner
                .cmd_queue
                .lock()
                .push_back(Box::new(CapturePrompt {
                    pane: self.pane,
                    prompt: true,
                    cursor: 0,
                }));
            TmuxDomainState::schedule_send_next_command(id);
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct EnableClientUi;
impl TmuxCommand for EnableClientUi {
    fn get_command(&self, _: DomainId) -> String {
        "refresh-client -f client-ui\n".into()
    }
    fn process_result(&self, id: DomainId, r: &Guarded) -> anyhow::Result<()> {
        anyhow::ensure!(!r.error, "cannot enable tmux client UI: {}", r.output);
        if let Some(domain) = Mux::get().get_domain(id) {
            if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                if let Some(pane) = Mux::get().get_pane(tmux.inner.pane_id) {
                    let bg = pane.palette().background;
                    let key = if 0.2126 * bg.0 + 0.7152 * bg.1 + 0.0722 * bg.2 > 0.5 {
                        "ReportLightTheme"
                    } else {
                        "ReportDarkTheme"
                    };
                    tmux.inner
                        .cmd_queue
                        .lock()
                        .push_back(Box::new(ClientTheme { key }));
                    TmuxDomainState::schedule_send_next_command(id);
                }
            }
        }
        promise::spawn::spawn_into_main_thread(async move {
            let mut tick = 0;
            loop {
                smol::Timer::after(std::time::Duration::from_millis(16)).await;
                let mux = Mux::get();
                let Some(domain) = mux.get_domain(id) else {
                    break;
                };
                let Some(tmux) = domain.downcast_ref::<TmuxDomain>() else {
                    break;
                };
                if tmux.inner.has_exited() {
                    break;
                }
                let window_id = tmux.inner.gui_window.lock().as_ref().map(|w| **w);
                let active_tab = window_id
                    .and_then(|w| mux.get_window(w))
                    .and_then(|w| w.get_active_tab().map(|t| t.tab_id()));
                let panes: Vec<_> = tmux
                    .inner
                    .gui_tabs
                    .lock()
                    .values()
                    .filter(|t| Some(t.tab_id) == active_tab)
                    .flat_map(|t| t.panes.iter().copied())
                    .collect();
                // Copy-mode animations can advance between 100 ms UI polls.
                // Refresh their real server screen at display cadence without
                // queuing stale snapshots or repeatedly querying static chrome.
                let mode_panes: Vec<_> = {
                    let remote = tmux.inner.remote_panes.lock();
                    panes.iter().filter_map(|id| {
                        let pane = remote.get(id)?.lock();
                        pane.mode_active.then_some((*id, pane.cursor_x, pane.cursor_y))
                    }).collect()
                };
                let mut queue = tmux.inner.cmd_queue.lock();
                tick += 1;
                if tick % 15 != 0 {
                    if tick % 3 == 0 && queue.is_empty() {
                        for (pane, x, y) in mode_panes {
                            queue.push_back(Box::new(CaptureMode {
                                pane, prompt: false, cursor_x: x as usize, cursor_y: y as usize,
                            }));
                        }
                    }
                    drop(queue);
                    TmuxDomainState::schedule_send_next_command(id);
                    continue;
                }
                if queue.len() > 4 {
                    // Long reads (history captures after attaching) are
                    // queued. Keep borders, status rows and modes current by
                    // reading them ahead of those captures, but never ahead
                    // of queued keystrokes or paste chunks.
                    if queue.iter().any(|c| c.is_client_input()) {
                        continue;
                    }
                    drop(queue);
                    if let Some(pane) = panes.first().copied() {
                        tmux.inner.enqueue_client_refresh(Box::new(CaptureFrame { pane, chrome: true }));
                    }
                    for pane in panes {
                        tmux.inner.enqueue_client_refresh(Box::new(InspectMode { pane }));
                    }
                    continue;
                }
                if let Some(pane) = panes.first().copied() {
                    queue.push_back(Box::new(CaptureFrame { pane, chrome: true }));
                }
                for pane in panes {
                    queue.push_back(Box::new(InspectMode { pane }));
                }
                if tick % 60 == 0 {
                    queue.push_back(Box::new(CaptureTitle));
                }
                drop(queue);
                TmuxDomainState::schedule_send_next_command(id);
            }
        })
        .detach();
        Ok(())
    }
}

#[derive(Debug)]
struct CapturePrompt {
    pane: TmuxPaneId,
    prompt: bool,
    cursor: usize,
}
impl TmuxCommand for CapturePrompt {
    fn snapshot_key(&self) -> Option<(&'static str, TmuxPaneId)> {
        Some(("prompt", self.pane))
    }
    fn get_command(&self, _: DomainId) -> String {
        format!("capture-pane -p -e -C -U -V -t %{}\n", self.pane)
    }
    fn process_result(&self, id: DomainId, r: &Guarded) -> anyhow::Result<()> {
        if r.error {
            return Ok(());
        }
        let mux = Mux::get();
        let Some(domain) = mux.get_domain(id) else {
            return Ok(());
        };
        let Some(tmux) = domain.downcast_ref::<TmuxDomain>() else {
            return Ok(());
        };
        let (header, body) = r.output.split_once('\n').unwrap_or(("", &r.output));
        let values: Vec<_> = header.split_whitespace().collect();
        let prompt = values.get(1).map(|v| *v == "1").unwrap_or(self.prompt);
        let cursor = values
            .get(2)
            .and_then(|v| v.parse().ok())
            .unwrap_or(self.cursor);
        let text = termwiz::tmux_cc::unvis(body)?;
        if prompt {
            return tmux.inner.update_prompt_surface(
                self.pane,
                Some(cursor),
                text.trim_end_matches('\n').as_bytes(),
            );
        }
        tmux.inner.update_prompt_surface(self.pane, None, &[])?;
        if let Some(pane) = tmux.inner.remote_panes.lock().get(&self.pane) {
            let pane = pane.lock();
            if let Some(local) = mux.get_pane(pane.local_pane_id) {
                // A prompt snapshot may finish after dismissal. Floating
                // panes already have server-rendered frames; adding a tiled
                // status row here would shrink and reflow their contents.
                if local.has_server_frame() {
                    return Ok(());
                }
                if let Some(local) = local.downcast_ref::<crate::localpane::LocalPane>() {
                    local.set_tmux_status(text.trim_end_matches('\n').as_bytes());
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct CaptureOverlay {
    pane: TmuxPaneId,
    geometry: [usize; 6],
}
impl TmuxCommand for CaptureOverlay {
    fn snapshot_key(&self) -> Option<(&'static str, TmuxPaneId)> {
        // Menus/popups are client-wide, even when inspected through a float.
        Some(("overlay", 0))
    }
    fn get_command(&self, _: DomainId) -> String {
        format!("capture-pane -p -e -C -O -t %{}\n", self.pane)
    }
    fn process_result(&self, id: DomainId, r: &Guarded) -> anyhow::Result<()> {
        if r.error {
            return Ok(());
        }
        let mux = Mux::get();
        let Some(domain) = mux.get_domain(id) else {
            return Ok(());
        };
        let Some(tmux) = domain.downcast_ref::<TmuxDomain>() else {
            return Ok(());
        };
        let text = termwiz::tmux_cc::unvis(&r.output)?;
        let text = text.trim_end_matches('\n').replace('\n', "\r\n");
        tmux.inner
            .update_ui_overlay(self.pane, Some(self.geometry), text.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ordinary_selection_like_text_is_not_metadata() {
        assert_eq!(mode_selection_record("@ui 0 1 2", "@selection hello\nnext"), ("", "@selection hello\nnext"));
    }
    #[test]
    fn selection_record_requires_the_capture_header_marker() {
        assert_eq!(mode_selection_record("@ui 0 1 2 selection-layers", "@selection 1 2|plain|styled\ntext"), ("1 2|plain|styled", "text"));
        assert_eq!(mode_selection_record("@ui 0 1 2 selection-layers", "@selection \n@selection ordinary text"), ("", "@selection ordinary text"));
    }
    #[test]
    fn tmux_acs_extensions_preserve_styles_and_plain_ascii() {
        assert_eq!(tmux_acs_extensions("+,-.0\x0e+,-.0\x1b[30m+\x0f+"), "+,-.0\x0e→←↑↓▮\x1b[30m→\x0f+");
    }

    #[test]
    fn continuous_tabs_allow_styles_but_not_terminal_controls() {
        assert_eq!(
            window_tabs_ansi(r"\033[38;5;188m café"),
            "\x1b[38;5;188m café"
        );
        assert_eq!(window_tabs_ansi("a\x1b[6n\nb\x07"), "a[6nb");
        assert_eq!(window_tabs_ansi(r"\033[2J"), r"\033[2J");
        let capture = CaptureFrame { pane: 7, chrome: true };
        assert_eq!(capture.response_count(), 2);
        assert_eq!(capture.get_command(0),
            "display-message -p -t %7 '' ; capture-pane -p -e -C -D -t %7\n");
        assert_eq!(CaptureFrame { pane: 7, chrome: false }.response_count(), 1);
    }

    #[test]
    fn continuous_tab_hits_follow_visible_unicode_columns() {
        let (text, regions) = decode_window_tabs(r"──\037@12\037\033[31m 1 é界 \037@99\037 2 zsh \037-\037─");
        assert_eq!(text, "──\x1b[31m 1 é界  2 zsh ─");
        assert_eq!(regions, vec![(2, 9, 12), (9, 16, 99)]);
    }

    #[test]
    fn symbolic_keys_preserve_server_bindings() {
        assert_eq!(
            key_name(KeyCode::Char(' '), KeyModifiers::CTRL).as_deref(),
            Some("C-Space")
        );
        assert_eq!(
            key_name(KeyCode::Char('n'), KeyModifiers::ALT).as_deref(),
            Some("M-n")
        );
        assert_eq!(
            key_name(KeyCode::Char('v'), KeyModifiers::SHIFT).as_deref(),
            Some("V")
        );
        assert_eq!(
            key_name(KeyCode::LeftArrow, KeyModifiers::CTRL).as_deref(),
            Some("C-Left")
        );
        assert_eq!(
            key_name(KeyCode::Function(12), KeyModifiers::NONE).as_deref(),
            Some("F12")
        );
        assert_eq!(key_name(KeyCode::Char('c'), KeyModifiers::SUPER), None);
    }
    #[test]
    fn commands_quote_punctuation_and_never_embed_newlines() {
        for c in ['\'', ';', '$', '\\', '#', 'é'] {
            let k = key_name(KeyCode::Char(c), KeyModifiers::NONE).unwrap();
            let cmd = ClientKey { pane: 7, key: k }.get_command(0);
            assert_eq!(cmd.matches('\n').count(), 1);
            assert!(cmd.starts_with("send-keys -K '"));
        }
        assert_eq!(
            key_name(KeyCode::Char('\n'), KeyModifiers::NONE).as_deref(),
            Some("Enter")
        );
    }

}

#[cfg(test)]
mod popup_input_tests {
    use super::*;
    fn text(pane: TmuxPaneId, key: &str) -> ClientPaste {
        ClientPaste { pane, keys: vec![key.into()], coalescible: true }
    }
    #[test]
    fn merge_preserves_punctuation_and_input_boundaries() {
        let mut batch = text(7, "a");
        for key in ["Space", "'", ";", "é"] { assert!(batch.append_text_keys(&text(7, key))); }
        assert_eq!(batch.get_command(0), concat!(r#"send-keys -K 'a' 'Space' ''\''' ';' 'é'"#, "\n"));
        assert!(!batch.append_text_keys(&text(8, "b")));
        assert!(!batch.append_text_keys(&ClientKey { pane: 7, key: "Enter".into() }));
        assert!(!batch.append_text_keys(&ClientPaste { pane: 7, keys: vec!["PasteStart".into()], coalescible: false }));
    }
    #[test]
    fn merge_is_bounded() {
        let mut batch = text(7, "a");
        for _ in 1..64 { assert!(batch.append_text_keys(&text(7, "b"))); }
        assert!(!batch.append_text_keys(&text(7, "c")));
        assert_eq!(batch.keys.len(), 64);
    }
}

// Client geometry also governs prompt width and popup placement in tmux.
pub(crate) fn resize_client(id: DomainId, cols: usize, rows: usize, cw: usize, ch: usize) {
    if let Some(domain) = Mux::get().get_domain(id) {
        if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
            let mut previous = tmux.inner.client_size.lock();
            if *previous == Some((cols, rows, cw, ch)) {
                return;
            }
            *previous = Some((cols, rows, cw, ch));
            drop(previous);
            tmux.inner
                .cmd_queue
                .lock()
                .push_back(Box::new(ClientSize { cols, rows, cw, ch }));
            TmuxDomainState::schedule_send_next_command(id);
        }
    }
}
#[derive(Debug)]
struct ClientSize {
    cols: usize,
    rows: usize,
    cw: usize,
    ch: usize,
}
impl TmuxCommand for ClientSize {
    fn get_command(&self, _: DomainId) -> String {
        format!(
            "refresh-client -C {}x{},{},{}\n",
            self.cols, self.rows, self.cw, self.ch
        )
    }
    fn process_result(&self, _: DomainId, r: &Guarded) -> anyhow::Result<()> {
        anyhow::ensure!(!r.error, "client resize failed: {}", r.output);
        Ok(())
    }
}

impl TmuxDomainState {
    pub(crate) fn send_client_mouse(
        &self,
        local: PaneId,
        mut event: wezterm_term::MouseEvent,
    ) -> anyhow::Result<bool> {
        let pane = self.remote_panes.lock().values().find_map(|p| {
            let p = p.lock();
            (p.local_pane_id == local).then_some((p.pane_left, p.pane_top))
        });
        let Some((left, top)) = pane else {
            return Ok(false);
        };
        event.x += left as usize;
        event.y += top as i64;
        self.send_window_mouse(event)
    }

    fn send_window_mouse(&self, mut event: wezterm_term::MouseEvent) -> anyhow::Result<bool> {
        use wezterm_term::{MouseButton as B, MouseEventKind as K};
        // The full-width strip crosses tmux pane boundaries, so its hit
        // regions belong to the native surface rather than pane-border cells.
        if event.modifiers.is_empty() && self.ui_overlay.lock().is_none()
            && self.ui_prompt.lock().is_none()
        {
            let remote = *self.active_remote_window.lock();
            let tab = remote.and_then(|w| self.gui_tabs.lock().get(&w).map(|t| t.tab_id))
                .and_then(|t| Mux::get().get_tab(t));
            let strip_row = tab.as_ref().map(|t| {
                // tmux's border row under its window (the GUI draws the strip
                // there), above the bottom when tmux's window is shorter.
                let last = t.get_size().rows.saturating_sub(1);
                let bottom = remote.and_then(|w| {
                    self.remote_panes.lock().values().filter_map(|p| {
                        let p = p.lock();
                        (p.window_id == w && p.key_target.is_none()
                            && t.get_floating_pane_by_pane_id(p.local_pane_id).is_none())
                            .then_some((p.pane_top + p.pane_height) as usize)
                    }).max()
                });
                bottom.map_or(last, |b| b.min(last))
            });
            if strip_row.map(|row| event.y == row as i64).unwrap_or(false) {
                let target = remote.and_then(|w| self.window_tab_regions.lock().get(&w)
                    .and_then(|regions| regions.iter().find(|(left, right, _)| event.x >= *left && event.x < *right)
                        .map(|(_, _, id)| *id)));
                if let Some(window_id) = target {
                    // A middle click closes the tab's window.
                    if event.kind == K::Press && event.button == B::Middle {
                        self.enqueue_client_input(Box::new(crate::tmux_strip::TmuxRaw(format!("kill-window -t @{}", window_id))));
                        return Ok(true);
                    }
                    if event.kind == K::Release && event.button == B::Middle { return Ok(true); }
                    if event.kind == K::Press && event.button == B::Left {
                        self.enqueue_client_input(Box::new(crate::tmux_commands::SelectWindow { window_id }));
                        return Ok(true);
                    }
                    if event.kind == K::Release && event.button == B::Left { return Ok(true); }
                }
            }
        }
        // Where the drag pointer is before clamping: tmux auto-scrolls only
        // while it is past the pane's top or bottom edge.
        let mut edge = None;
        // A left drag stays in the pane where it started, as in a native
        // selection: past an edge it is clamped onto the edge row, where
        // tmux's copy-mode drag timer scrolls. Without this, motion outside
        // the pane is ignored by tmux and auto-scroll stops.
        match (event.kind, event.button) {
            (K::Press, B::Left) => {
                let window = *self.active_remote_window.lock();
                let active = window.and_then(|w| self.active_remote_panes.lock().get(&w).copied());
                let (x, y) = (event.x, event.y.max(0) as usize);
                // Only real panes: virtual panes (status rows, popups) share
                // the window and can cover the same cells.
                let hits: Vec<_> = self.remote_panes.lock().values().filter_map(|p| {
                    let p = p.lock();
                    let (l, t, w, h) = (p.pane_left as usize, p.pane_top as usize,
                        p.pane_width as usize, p.pane_height as usize);
                    (Some(p.window_id) == window && p.key_target.is_none() && w > 0 && h > 0
                        && x >= l && x < l + w && y >= t && y < t + h)
                        .then_some((p.pane_id, l, t, w, h))
                }).collect();
                let hit = hits.iter().find(|h| Some(h.0) == active).or(hits.first()).copied();
                log::debug!("mouse drag pane {:?} of {:?}", hit, hits);
                *self.mouse_drag.lock() = hit;
            }
            (K::Move, B::Left) => {
                if let Some((_, l, t, w, h)) = *self.mouse_drag.lock() {
                    edge = Some(Self::drag_edge(event.y, t, h));
                    event.x = event.x.clamp(l, l + w - 1);
                    event.y = event.y.clamp(t as i64, (t + h - 1) as i64);
                }
            }
            // A release past the window edge must reach the drag pane too:
            // tmux ignores events outside the window and would keep its
            // edge auto-scroll running after the button is up.
            (K::Release, _) => {
                if let Some((_, l, t, w, h)) = self.mouse_drag.lock().take() {
                    edge = Some(Self::drag_edge(event.y, t, h));
                    event.x = event.x.clamp(l, l + w - 1);
                    event.y = event.y.clamp(t as i64, (t + h - 1) as i64);
                }
            }
            _ => {}
        }
        let (mut button, repeat) = match event.button {
            B::Left => (0, 1),
            B::Middle => (1, 1),
            B::Right => (2, 1),
            B::None => (3, 1),
            B::WheelUp(_) => (64, 1),
            B::WheelDown(_) => (65, 1),
            B::WheelLeft(_) => (66, 1),
            B::WheelRight(_) => (67, 1),
        };
        if event.kind == K::Release {
            button = 3;
        }
        if event.kind == K::Move {
            button |= 32;
        }
        button |= Self::mouse_modifier_bits(&event);
        let x = event.x;
        let y = event.y.max(0) as usize;
        for _ in 0..repeat {
            self.enqueue_client_input(Box::new(ClientMouse { x, y, button, edge }));
        }
        Ok(true)
    }
}
impl TmuxDomainState {
    /// Rows past the drag pane (top row t, h rows): negative above, positive
    /// below, 0 inside. tmux scrolls faster the further past.
    fn drag_edge(y: i64, t: usize, h: usize) -> i8 {
        let bottom = (t + h).saturating_sub(1) as i64;
        let rows = if y < t as i64 {
            y - t as i64
        } else if y > bottom {
            y - bottom
        } else {
            0
        };
        rows.clamp(-100, 100) as i8
    }

    fn mouse_modifier_bits(event: &wezterm_term::MouseEvent) -> u32 {
        let mut bits = 0;
        if event.modifiers.contains(KeyModifiers::SHIFT) {
            bits |= 4;
        }
        if event.modifiers.contains(KeyModifiers::ALT) {
            bits |= 8;
        }
        if event.modifiers.contains(KeyModifiers::CTRL) {
            bits |= 16;
        }
        bits
    }

}

#[derive(Debug)]
struct ClientMouse {
    x: usize,
    y: usize,
    button: u32,
    edge: Option<i8>,
}
impl TmuxCommand for ClientMouse {
    fn inline_result(&self) -> bool { true }
    fn mouse_motion(&self) -> Option<(usize, usize, u32)> {
        (self.button & 32 != 0).then_some((self.x, self.y, self.button))
    }
    fn merge_mouse_motion(&mut self, input: &dyn TmuxCommand) -> bool {
        match (self.mouse_motion(), input.mouse_motion()) {
            (Some((_, _, mine)), Some((x, y, button))) if mine == button => {
                self.x = x;
                self.y = y;
                self.edge = input.mouse_edge();
                true
            }
            _ => false,
        }
    }
    fn mouse_edge(&self) -> Option<i8> {
        self.edge
    }
    fn is_client_input(&self) -> bool {
        true
    }
    fn get_command(&self, _: DomainId) -> String {
        match self.edge {
            Some(edge) => format!("send-keys -K -m {},{},{},{}\n", self.x, self.y, self.button, edge),
            None => format!("send-keys -K -m {},{},{}\n", self.x, self.y, self.button),
        }
    }
    fn process_result(&self, id: DomainId, r: &Guarded) -> anyhow::Result<()> {
        anyhow::ensure!(!r.error, "mouse input failed: {}", r.output);
        if let Some(domain) = Mux::get().get_domain(id) {
            if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                let window = *tmux.inner.active_remote_window.lock();
                let pane = window.and_then(|w| tmux.inner.active_remote_panes.lock().get(&w).copied());
                if let Some(pane) = pane {
                    // Wheel and drag motion change only the copy screen: refresh it
                    // alone. Chrome and status follow the release.
                    tmux.inner.refresh_client_pane(
                        pane,
                        matches!(self.button, 64 | 65) || self.button & 32 != 0,
                    );
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct ClientTheme {
    key: &'static str,
}
impl TmuxCommand for ClientTheme {
    fn get_command(&self, _: DomainId) -> String {
        format!("send-keys -K {}\n", self.key)
    }
    fn process_result(&self, _: DomainId, r: &Guarded) -> anyhow::Result<()> {
        anyhow::ensure!(!r.error, "theme report failed: {}", r.output);
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct CaptureImages {
    pub pane: TmuxPaneId,
}
impl TmuxCommand for CaptureImages {
    fn response_count(&self) -> usize { 2 }
    fn get_command(&self, _: DomainId) -> String {
        format!("display-message -p -t %{0} '#{{cursor_x}} #{{cursor_y}}' ; capture-pane -G -t %{0}\n", self.pane)
    }
    fn process_result(&self, id: DomainId, r: &Guarded) -> anyhow::Result<()> {
        if r.error {
            return Ok(());
        }
        use base64::Engine;
        let mux = Mux::get();
        let Some(domain) = mux.get_domain(id) else {
            return Ok(());
        };
        let Some(tmux) = domain.downcast_ref::<TmuxDomain>() else {
            return Ok(());
        };
        let panes = tmux.inner.remote_panes.lock();
        let Some(pane) = panes.get(&self.pane) else {
            return Ok(());
        };
        let mut pane = pane.lock();
        let (cursor, images) = r.output.split_once('\n').unwrap_or(("", ""));
        let mut cursor = cursor.split_whitespace().map(|v| v.parse::<usize>().ok());
        let (Some(Some(cx)), Some(Some(cy))) = (cursor.next(), cursor.next()) else {
            return Ok(());
        };
        let mut frame = Vec::new();
        for line in images.lines() {
            let mut f = line.split_whitespace();
            let (Some(x), Some(y), Some(encoded)) = (f.next(), f.next(), f.next()) else {
                continue;
            };
            let x: usize = x.parse()?;
            let y: usize = y.parse()?;
            let image = base64::engine::general_purpose::STANDARD.decode(encoded)?;
            write!(frame, "\x1b[{};{}H", y + 1, x + 1)?;
            frame.extend_from_slice(&image);
        }
        if frame.is_empty() {
            return Ok(());
        }
        // Restore the exact cursor without DECSC/DECRC (application state).
        write!(frame, "\x1b[{};{}H", cy + 1, cx + 1)?;
        if pane.mode_active {
            pane.mode_backlog.extend_from_slice(&frame);
        } else {
            pane.output_write.write_all(&frame)?;
        }
        Ok(())
    }
}

#[derive(Debug)]
struct CaptureTitle;
impl TmuxCommand for CaptureTitle {
    fn get_command(&self, _: DomainId) -> String {
        "display-message -p '#{T:set-titles-string}'\n".into()
    }
    fn process_result(&self, id: DomainId, r: &Guarded) -> anyhow::Result<()> {
        if r.error {
            return Ok(());
        }
        let mux = Mux::get();
        if let Some(domain) = mux.get_domain(id) {
            if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                let title = r.output.trim_end_matches('\n').to_owned();
                let mut old = tmux.inner.client_title.lock();
                if old.as_ref() != Some(&title) {
                    *old = Some(title);
                    drop(old);
                    if let Some(window) = tmux.inner.gui_window.lock().as_ref().map(|w| **w) {
                        mux.notify(crate::MuxNotification::WindowInvalidated(window));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Border chrome of a window. The first (empty) line once carried tmux-rendered
/// window tab labels; the GUI now draws the tab strip itself (tmux_strip.rs).
pub(crate) fn chrome_reads(pane: TmuxPaneId) -> String {
    format!("display-message -p -t %{pane} '' ; capture-pane -p -e -C -D -t %{pane}")
}

#[derive(Debug)]
pub(crate) struct CaptureFrame {
    pane: TmuxPaneId,
    chrome: bool,
}
impl TmuxCommand for CaptureFrame {
    fn response_count(&self) -> usize { if self.chrome { 2 } else { 1 } }
    fn snapshot_key(&self) -> Option<(&'static str, TmuxPaneId)> {
        Some((if self.chrome { "chrome" } else { "frame" }, self.pane))
    }
    fn get_command(&self, _: DomainId) -> String {
        if self.chrome {
            // One command list, one completed snapshot: do not combine a new
            // border capture with labels from an earlier poll/window list.
            format!("{}\n", chrome_reads(self.pane))
        } else {
            format!("capture-pane -p -e -C -W -t %{}\n", self.pane)
        }
    }
    fn process_result(&self, id: DomainId, r: &Guarded) -> anyhow::Result<()> {
        if r.error {
            return Ok(());
        }
        if let Some(domain) = Mux::get().get_domain(id) {
            if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                let (tabs, body) = if self.chrome {
                    let (tabs, body) = r.output.split_once('\n').unwrap_or(("", &r.output));
                    (Some(decode_window_tabs(tabs)), body)
                } else { (None, r.output.as_str()) };
                let text = termwiz::tmux_cc::unvis(body)?;
                let text = text
                    .strip_suffix('\n')
                    .unwrap_or(&text)
                    .replace('\n', "\r\n");
                tmux.inner
                    .update_float_frame(self.pane, text.as_bytes(), self.chrome, tabs.as_ref())?;
            }
        }
        Ok(())
    }
}

impl TmuxDomain {
    pub fn send_window_mouse(&self, event: wezterm_term::MouseEvent) -> anyhow::Result<bool> {
        if !self
            .inner
            .support_commands
            .lock()
            .get("send-keys")
            .map(|s| s.contains("-m"))
            .unwrap_or(false)
        {
            return Ok(false);
        }
        self.inner.send_window_mouse(event)
    }

    pub(crate) fn is_virtual_pane(&self, local: PaneId) -> bool {
        self.inner.remote_panes.lock().values().any(|p| {
            let p = p.lock();
            p.local_pane_id == local && p.key_target.is_some()
        })
    }

    pub(crate) fn cursor_is_active(&self, local: PaneId) -> bool {
        let remote = self.inner.remote_panes.lock().values().find_map(|p| {
            let p = p.lock();
            (p.local_pane_id == local).then_some((p.pane_id, p.window_id))
        });
        let Some((pane, window)) = remote else {
            return false;
        };
        if let Some(overlay) = *self.inner.ui_overlay.lock() {
            return pane == overlay;
        }
        if let Some(prompt) = *self.inner.ui_prompt.lock() {
            return pane == prompt;
        }
        self.inner.active_remote_panes.lock().get(&window) == Some(&pane)
    }
}

#[derive(Debug)]
struct ClientPaste {
    pane: TmuxPaneId,
    keys: Vec<String>,
    coalescible: bool,
}
impl TmuxCommand for ClientPaste {
    fn inline_result(&self) -> bool { self.coalescible }
    fn text_keys(&self) -> Option<(TmuxPaneId, &[String])> {
        self.coalescible.then_some((self.pane, self.keys.as_slice()))
    }
    fn append_text_keys(&mut self, input: &dyn TmuxCommand) -> bool {
        let Some((pane, keys)) = input.text_keys() else { return false; };
        if !self.coalescible || self.pane != pane || self.keys.len() + keys.len() > 64 {
            return false;
        }
        self.keys.extend_from_slice(keys);
        true
    }
    fn is_client_input(&self) -> bool {
        true
    }
    fn get_command(&self, _: DomainId) -> String {
        format!(
            "send-keys -K {}\n",
            self.keys
                .iter()
                .map(|s| quote(s))
                .collect::<Vec<_>>()
                .join(" ")
        )
    }
    fn process_result(&self, id: DomainId, r: &Guarded) -> anyhow::Result<()> {
        anyhow::ensure!(!r.error, "tmux paste failed: {}", r.output);
        if let Some(domain) = Mux::get().get_domain(id) {
            if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                let mut queue = tmux.inner.cmd_queue.lock();
                if !queue.iter().any(|c| c.is_client_input()) {
                    queue.push_back(Box::new(InspectMode { pane: self.pane }));
                }
                drop(queue);
                TmuxDomainState::schedule_send_next_command(id);
            }
        }
        Ok(())
    }
}

/// Only SGR is accepted from the configured tab format; names cannot inject
/// cursor motion, terminal queries, or control-mode protocol records.
fn window_tabs_ansi(text: &str) -> String { decode_window_tabs(text).0 }

pub(crate) fn decode_window_tabs(text: &str) -> (String, Vec<(usize, usize, TmuxWindowId)>) {
    let mut result = String::new();
    let mut plain = String::new();
    let mut regions = Vec::new();
    let mut active = None;
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(suffix) = rest.strip_prefix("\\037") {
            if let Some(end) = suffix.find("\\037") {
                let token = &suffix[..end];
                let window = token.strip_prefix('@').and_then(|v| v.parse::<TmuxWindowId>().ok());
                if window.is_some() || token == "-" {
                    let col = termwiz::cell::unicode_column_width(&plain, None);
                    if let Some((start, id)) = active.take() { regions.push((start, col, id)); }
                    active = window.map(|id| (col, id));
                    rest = &suffix[end + 4..];
                    continue;
                }
            }
        }
        if let Some(suffix) = rest.strip_prefix("\\033[") {
            if let Some(end) = suffix.find('m') {
                if suffix[..end].chars().all(|c| c.is_ascii_digit() || c == ';') {
                    result.push_str("\x1b[");
                    result.push_str(&suffix[..=end]);
                    rest = &suffix[end + 1..];
                    continue;
                }
            }
        }
        let c = rest.chars().next().unwrap();
        if !c.is_control() { result.push(c); plain.push(c); }
        rest = &rest[c.len_utf8()..];
    }
    if let Some((start, id)) = active { regions.push((start, termwiz::cell::unicode_column_width(&plain, None), id)); }
    (result, regions)
}
