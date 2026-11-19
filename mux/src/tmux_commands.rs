use crate::domain::{DomainId, WriterWrapper};
use crate::activity::Activity;
use crate::localpane::LocalPane;
use crate::pane::{alloc_pane_id, PaneId};
use crate::tab::{SplitDirection, SplitRequest, SplitSize, Tab, TabId};
use crate::tmux::{AttachState, PaneOutput, TmuxDomain, TmuxDomainState, TmuxRemotePane, TmuxTab};
use crate::tmux_pty::{TmuxChild, TmuxPty};
use crate::{Mux, MuxNotification, Pane};
use anyhow::{anyhow, Context};
use parking_lot::{Condvar, Mutex};
use portable_pty::{MasterPty, PtySize};
use std::collections::HashSet;
use std::fmt::{Debug, Write};
use std::io::Write as _;
use std::sync::Arc;
use termwiz::escape::csi::{Cursor, CSI};
use termwiz::escape::{Action, OneBased};
use termwiz::tmux_cc::*;
use wezterm_term::TerminalSize;

pub(crate) trait TmuxCommand: Send + Debug {
    // These commands only use shared mux state and transport IO; they must
    // not wait behind GUI rendering to advance the control protocol.
    fn immediate(&self) -> bool { self.is_client_input() }
    fn inline_result(&self) -> bool { false }
    /// A compound read produces one flag-1 guard per tmux command.
    fn response_count(&self) -> usize { 1 }
    /// Only screen reads may replace an older queued read of the same surface.
    fn snapshot_key(&self) -> Option<(&'static str, TmuxPaneId)> { None }
    fn text_keys(&self) -> Option<(TmuxPaneId, &[String])> { None }
    fn append_text_keys(&mut self, _input: &dyn TmuxCommand) -> bool { false }
    /// A queued pointer motion that no later input follows can take the
    /// newer position: only the latest drag position matters to tmux.
    fn mouse_motion(&self) -> Option<(usize, usize, u32)> { None }
    fn merge_mouse_motion(&mut self, _input: &dyn TmuxCommand) -> bool { false }
    /// Drag pointer above (-1), below (1) or inside (0) its pane.
    fn mouse_edge(&self) -> Option<i8> { None }

    fn is_client_input(&self) -> bool {
        false
    }
    fn get_command(&self, domain_id: DomainId) -> String;
    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()>;
}

#[derive(Debug, Clone)]
pub(crate) struct PaneItem {
    session_id: TmuxSessionId,
    window_id: TmuxWindowId,
    pane_id: TmuxPaneId,
    _pane_index: u64,
    cursor_x: u64,
    cursor_y: u64,
    pane_width: u64,
    pane_height: u64,
    pane_left: u64,
    pane_top: u64,
    pane_active: bool,
    pane_floating: bool,
    pane_border: bool,
    window_zoomed: bool,
    popup_frame: Option<crate::pane::PopupFrame>,
}

#[derive(Debug)]
struct WindowItem {
    session_id: TmuxSessionId,
    window_id: TmuxWindowId,
    window_width: u64,
    window_height: u64,
    window_active: bool,
    window_name: String,
    layout: Vec<WindowLayout>,
    layout_csum: String,
    history_limit: isize,
}

impl TmuxDomainState {
    /// check if a PaneItem received from a session snapshot has been attached
    pub fn check_pane_attached(&self, window_id: TmuxWindowId, pane_id: TmuxPaneId) -> bool {
        let gui_tabs = self.gui_tabs.lock();
        let Some(local_tab) = gui_tabs.get(&window_id) else {
            return false;
        };

        return local_tab.panes.get(&pane_id).is_some();
    }

    pub fn check_window_attached(&self, window_id: TmuxWindowId) -> bool {
        let gui_tabs = self.gui_tabs.lock();
        return gui_tabs.get(&window_id).is_some();
    }

    /// after we create a tab for a remote pane, save its ID into the
    /// TmuxPane-TmuxPane tree, so we can ref it later.
    fn add_attached_pane(
        &self,
        window_id: TmuxWindowId,
        pane_id: TmuxPaneId,
    ) -> anyhow::Result<()> {
        let mut gui_tabs = self.gui_tabs.lock();

        let panes = match gui_tabs.get_mut(&window_id) {
            Some(tab) => &mut tab.panes,
            None => anyhow::bail!("The window {window_id} is not attached"),
        };

        match panes.get(&pane_id) {
            Some(_) => {
                anyhow::bail!("Tmux pane already attached");
            }
            None => {
                panes.insert(pane_id);
                return Ok(());
            }
        }
    }

    fn add_attached_window(&self, target: &WindowItem, tab_id: &TabId) -> anyhow::Result<()> {
        let mut gui_tabs = self.gui_tabs.lock();
        if !gui_tabs.contains_key(&target.window_id) {
            gui_tabs.insert(
                target.window_id,
                TmuxTab {
                    tab_id: *tab_id,
                    tmux_window_id: target.window_id,
                    layout_csum: target.layout_csum.clone(),
                    panes: HashSet::new(),
                },
            );
        }

        Ok(())
    }

    fn remove_detached_pane(
        &self,
        window_id: TmuxWindowId,
        new_set: &HashSet<TmuxPaneId>,
    ) -> anyhow::Result<()> {
        let mut gui_tabs = self.gui_tabs.lock();

        let (tab_id, panes) = match gui_tabs.get_mut(&window_id) {
            Some(tab) => (tab.tab_id, &mut tab.panes),
            None => anyhow::bail!("The window {window_id} is not attached"),
        };

        let to_remove: Vec<_> = panes.difference(new_set).cloned().collect();

        let mux = Mux::get();
        for p in to_remove {
            // A destination snapshot may already have adopted this pane.
            // Removing it from the old tab must not kill the live terminal.
            let moved = self.remote_panes.lock().get(&p)
                .map(|pane| pane.lock().window_id != window_id).unwrap_or(false);
            if moved {
                panes.remove(&p);
                continue;
            }
            let frame_id = u64::MAX / 2 - p;
            if let Some(frame) = self.remote_panes.lock().remove(&frame_id) {
                let frame = frame.lock();
                *frame.active_lock.0.lock() = true;
                frame.active_lock.1.notify_all();
                mux.remove_pane(frame.local_pane_id);
            }
            let removed = self.remote_panes.lock().remove(&p);
            if let Some(pane) = removed {
                let pane = pane.lock();
                let local_pane_id = pane.local_pane_id;
                if let Some(tab) = mux.get_tab(tab_id) {
                    tab.remove_remote_frame(local_pane_id);
                    if let Some((index, _)) = tab.get_floating_pane_by_pane_id(local_pane_id) {
                        let _ = tab.remove_floating_pane(index);
                    }
                }
                let (lock, ready) = &*pane.active_lock;
                *lock.lock() = true;
                ready.notify_all();
                mux.remove_pane(local_pane_id);
            }
            self.backlog.lock().remove(&p);
            panes.remove(&p);
        }

        if panes.is_empty() {
            mux.remove_tab(tab_id);
            gui_tabs.remove(&window_id);
        }

        Ok(())
    }

    fn remove_remote_pane(&self, id: TmuxPaneId) {
        let children: Vec<_> = self
            .remote_panes
            .lock()
            .iter()
            .filter_map(|(child, p)| (p.lock().key_target == Some(id)).then_some(*child))
            .collect();
        for child in children {
            self.remove_remote_pane(child);
        }
        let removed = self.remote_panes.lock().remove(&id);
        if let Some(p) = removed {
            let p = p.lock();
            let mux = Mux::get();
            let tab_id = self.gui_tabs.lock().get(&p.window_id).map(|t| t.tab_id);
            if let Some(tab) = tab_id.and_then(|id| mux.get_tab(id)) {
                tab.remove_remote_frame(p.local_pane_id);
                if let Some((index, _)) = tab.get_floating_pane_by_pane_id(p.local_pane_id) {
                    let _ = tab.remove_floating_pane(index);
                }
            }
            *p.active_lock.0.lock() = true;
            p.active_lock.1.notify_all();
            mux.remove_pane(p.local_pane_id);
        }
        self.backlog.lock().remove(&id);
    }

    pub fn remove_detached_window(&self, window_id: TmuxWindowId) -> anyhow::Result<()> {
        let ids: Vec<_> = self
            .remote_panes
            .lock()
            .iter()
            .filter_map(|(id, p)| (p.lock().window_id == window_id).then_some(*id))
            .collect();
        for overlay in [&self.ui_overlay, &self.ui_prompt] {
            let mut overlay = overlay.lock();
            if overlay.map(|id| ids.contains(&id)).unwrap_or(false) {
                *overlay = None;
            }
        }
        for id in ids {
            self.remove_remote_pane(id);
        }
        let tab = self.gui_tabs.lock().remove(&window_id);
        if let Some(tab) = tab {
            let mux = Mux::get();
            if let Some(native) = mux.get_tab(tab.tab_id) {
                // The old tab may still reference panes adopted by a surviving
                // window. Detach those references without killing terminals.
                let moved: Vec<_> = self.remote_panes.lock().values()
                    .filter_map(|p| { let p = p.lock(); (p.window_id != window_id).then_some(p.local_pane_id) })
                    .collect();
                for id in moved {
                    if native.iter_all_panes().iter().any(|p| p.pane_id() == id) { native.remove_pane(id); }
                }
            }
            mux.remove_tab(tab.tab_id);
        }
        self.active_remote_panes.lock().remove(&window_id);
        self.window_tab_regions.lock().remove(&window_id);
        Ok(())
    }

    fn set_pane_cursor_position(&self, pane: &Arc<dyn Pane>, x: usize, y: usize) {
        pane.perform_actions(vec![Action::CSI(CSI::Cursor(
            Cursor::CharacterAndLinePosition {
                col: OneBased::from_zero_based(x as u32),
                line: OneBased::from_zero_based(y as u32),
            },
        ))]);
    }

    fn create_pane(&self, pane: &PaneItem) -> anyhow::Result<Arc<dyn Pane>> {
        // A pane can move between windows (join/break/swap, including thumbs).
        // Keep its native terminal/history and update ownership instead of
        // replacing the map entry while its previous tab still references it.
        if let Some(remote) = self.remote_panes.lock().get(&pane.pane_id) {
            let mut remote = remote.lock();
            if let Some(local) = Mux::get().get_pane(remote.local_pane_id) {
                remote.window_id = pane.window_id;
                remote.session_id = pane.session_id;
                return Ok(local);
            }
        }
        let local_pane_id = alloc_pane_id();
        let active_lock = Arc::new((Mutex::new(false), Condvar::new()));
        let (output_read, output_write) = filedescriptor::socketpair()?;
        let ref_pane = Arc::new(Mutex::new(TmuxRemotePane {
            local_pane_id,
            output_write: PaneOutput::new(output_write),
            active_lock: active_lock.clone(),
            session_id: pane.session_id,
            window_id: pane.window_id,
            pane_id: pane.pane_id,
            key_target: None,
            mode_active: false,
            mode_cursor_visible: true,
            mode_name: String::new(),
            mode_snapshot: None,
                mode_selection_snapshot: None,
            mode_render_cursor: None,
            mode_was_alt: false,
            mode_screen_pending: false,
            audio_carry: Vec::new(),
            mode_backlog: Vec::new(),
            mode_resized: false,
            cursor_x: pane.cursor_x,
            cursor_y: pane.cursor_y,
            pane_width: pane.pane_width,
            pane_height: pane.pane_height,
            pane_left: pane.pane_left,
            pane_top: pane.pane_top,
        }));

        {
            // Output that arrived before this pane existed includes the
            // application's terminal modes and saved cursor (e.g. a shell's
            // instant prompt). Replay it under the map lock: later output is
            // written by the transport reader only after this insertion.
            let mut pane_map = self.remote_panes.lock();
            if let Some(early) = self.backlog.lock().remove(&pane.pane_id) {
                let _ = ref_pane.lock().output_write.write_all(&early);
            }
            pane_map.insert(pane.pane_id, ref_pane.clone());
        }

        let pane_pty = TmuxPty {
            domain_id: self.domain_id,
            reader: output_read,
            cmd_queue: self.cmd_queue.clone(),
            master_pane: ref_pane,
        };

        let writer = WriterWrapper::new(pane_pty.take_writer()?);

        let size = TerminalSize {
            rows: pane.pane_height as usize,
            cols: pane.pane_width as usize,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        };

        let child = TmuxChild {
            active_lock: active_lock.clone(),
        };

        let mut terminal = wezterm_term::Terminal::new(
            size,
            std::sync::Arc::new(config::TermConfig::new()),
            "WezTerm",
            config::wezterm_version(),
            Box::new(writer.clone()),
        );
        terminal.set_mirrored_output(true);

        let local = LocalPane::new(
            local_pane_id,
            terminal,
            Box::new(child),
            Box::new(pane_pty),
            Box::new(writer),
            self.domain_id,
            "tmux pane".to_string(),
        );
        if self
            .support_commands
            .lock()
            .get("send-keys")
            .map(|v| v.contains("-m "))
            .unwrap_or(false)
        {
            local.mark_tmux_client();
        }
        Ok(Arc::new(local))
    }

    pub fn split_pane(
        &self,
        tab_id: TabId,
        pane_id: PaneId,
        remote_id: TmuxPaneId,
        split_request: SplitRequest,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let mux = Mux::get();
        let tab = match mux.get_tab(tab_id) {
            Some(t) => t,
            None => anyhow::bail!("Invalid tab id {}", tab_id),
        };

        let pane_index = match tab
            .iter_panes_ignoring_zoom()
            .iter()
            .find(|p| p.pane.pane_id() == pane_id)
        {
            Some(p) => p.index,
            None => anyhow::bail!("invalid pane id {}", pane_id),
        };

        let split_size = match tab.compute_split_size(pane_index, split_request) {
            Some(s) => s,
            None => anyhow::bail!("invalid pane index {}", pane_index),
        };

        let window_id = match self.gui_tabs.lock().iter().find(|t| t.1.tab_id == tab_id) {
            Some((_, tab)) => tab.tmux_window_id,
            None => anyhow::bail!("No tab {}", tab_id),
        };

        let p = PaneItem {
            session_id: 0,
            window_id: window_id,
            pane_id: remote_id,
            _pane_index: 0,
            cursor_x: 0,
            cursor_y: 0,
            pane_width: split_size.second.cols as u64,
            pane_height: split_size.second.rows as u64,
            pane_left: 0,
            pane_top: 0,
            pane_active: false,
            pane_floating: false,
            pane_border: false,
            window_zoomed: false,
            popup_frame: None,
        };

        let pane = self.create_pane(&p).context("failed to create pane")?;
        tab.split_and_insert(pane_index, split_request, Arc::clone(&pane))?;

        self.add_attached_pane(window_id, remote_id)?;

        let _ = mux.add_pane(&pane);

        return Ok(pane);
    }

    pub(crate) fn update_ui_overlay(
        &self,
        parent: TmuxPaneId,
        geometry: Option<[usize; 6]>,
        text: &[u8],
    ) -> anyhow::Result<()> {
        self.update_ui_surface(parent, false, geometry, text)
    }

    pub(crate) fn update_prompt_surface(
        &self,
        parent: TmuxPaneId,
        cursor: Option<usize>,
        text: &[u8],
    ) -> anyhow::Result<()> {
        let remote_window = self
            .remote_panes
            .lock()
            .get(&parent)
            .map(|p| p.lock().window_id);
        let tab = remote_window
            .and_then(|w| self.gui_tabs.lock().get(&w).map(|t| t.tab_id))
            .and_then(|id| Mux::get().get_tab(id));
        let geometry = cursor.and_then(|x| {
            tab.map(|t| {
                let size = t.get_size();
                [0, size.rows.saturating_sub(1), size.cols, 1, x, 0]
            })
        });
        self.update_ui_surface(parent, true, geometry, text)
    }

    fn update_ui_surface(
        &self,
        parent: TmuxPaneId,
        prompt: bool,
        geometry: Option<[usize; 6]>,
        text: &[u8],
    ) -> anyhow::Result<()> {
        let mux = Mux::get();
        let parent_data = self.remote_panes.lock().get(&parent).map(|p| {
            let p = p.lock();
            (p.window_id, p.session_id)
        });
        let Some((window_id, session_id)) = parent_data else {
            return Ok(());
        };
        let tab_id = self.gui_tabs.lock().get(&window_id).map(|t| t.tab_id);
        let Some(tab) = tab_id.and_then(|id| mux.get_tab(id)) else {
            return Ok(());
        };
        let mut overlay = if prompt {
            self.ui_prompt.lock()
        } else {
            self.ui_overlay.lock()
        };
        let Some([left, top, width, height, cx, cy]) = geometry else {
            if let Some(id) = overlay.take() {
                self.remove_remote_pane(id);
                self.refresh_remote_windows();
                Self::schedule_send_next_command(self.domain_id);
            }
            return Ok(());
        };
        let previous_window = overlay.and_then(|id| {
            self.remote_panes
                .lock()
                .get(&id)
                .map(|p| p.lock().window_id)
        });
        if previous_window.is_some() && previous_window != Some(window_id) {
            if let Some(id) = overlay.take() {
                self.remove_remote_pane(id);
            }
        }
        let id = match *overlay {
            Some(id) => id,
            None => {
                // Virtual screen belongs to tmux, but has no tmux PTY/pane ID.
                let id = u64::MAX - window_id * 2 - u64::from(prompt);
                let item = PaneItem {
                    session_id,
                    window_id,
                    pane_id: id,
                    _pane_index: 0,
                    cursor_x: cx as u64,
                    cursor_y: cy as u64,
                    pane_width: width as u64,
                    pane_height: height as u64,
                    pane_left: left as u64,
                    pane_top: top as u64,
                    pane_active: true,
                    pane_floating: true,
                    pane_border: false,
                    window_zoomed: false,
                    popup_frame: None,
                };
                let local = self.create_pane(&item)?;
                if let Some(local) = local.downcast_ref::<LocalPane>() {
                    local.mark_tmux_surface();
                }
                if let Some(p) = self.remote_panes.lock().get(&id) {
                    p.lock().key_target = Some(parent);
                }
                mux.add_pane(&local)?;
                tab.set_remote_floating_geometry(local.pane_id(), left, top, width, height);
                tab.append_floating_pane(local);
                *overlay = Some(id);
                id
            }
        };
        let local_id = self
            .remote_panes
            .lock()
            .get(&id)
            .map(|p| p.lock().local_pane_id);
        if let Some(local) = local_id.and_then(|id| mux.get_pane(id)) {
            let old = local.get_dimensions();
            if old.cols != width || old.viewport_rows != height {
                let size = tab.get_size();
                local.resize(TerminalSize {
                    cols: width,
                    rows: height,
                    pixel_width: width * (size.pixel_width / size.cols.max(1)),
                    pixel_height: height * (size.pixel_height / size.rows.max(1)),
                    dpi: size.dpi,
                })?;
            }
        }
        let panes = self.remote_panes.lock();
        if let Some(p) = panes.get(&id) {
            let mut p = p.lock();
            tab.set_remote_floating_geometry(p.local_pane_id, left, top, width, height);
            p.pane_left = left as u64;
            p.pane_top = top as u64;
            p.pane_width = width as u64;
            p.pane_height = height as u64;
            p.key_target = Some(parent);
            p.mode_cursor_visible = cx < width && cy < height;
            let mut frame = b"\x0f\x1b[0m\x1b)0\x1b[H\x1b[2J".to_vec();
            frame.extend_from_slice(text);
            if cx < width && cy < height {
                write!(frame, "\x1b[?25h\x1b[{};{}H", cy + 1, cx + 1)?;
            } else { frame.extend_from_slice(b"\x1b[?25l"); }
            if let Some(local) = mux.get_pane(p.local_pane_id) {
                local.downcast_ref::<LocalPane>().unwrap().set_tmux_surface(&frame);
            }
        }
        Ok(())
    }

    pub(crate) fn update_float_frame(
        &self,
        parent: TmuxPaneId,
        text: &[u8],
        chrome: bool,
        tabs: Option<&(String, Vec<(usize, usize, TmuxWindowId)>)>,
    ) -> anyhow::Result<()> {
        let mux = Mux::get();
        let data = self.remote_panes.lock().get(&parent).map(|p| {
            let p = p.lock();
            (
                p.local_pane_id,
                p.window_id,
                p.session_id,
                p.pane_left as usize,
                p.pane_top as usize,
                p.pane_width as usize,
                p.pane_height as usize,
            )
        });
        let Some((parent_local, window_id, session_id, x, y, cols, rows)) = data else {
            return Ok(());
        };
        let tab = self
            .gui_tabs
            .lock()
            .get(&window_id)
            .and_then(|t| mux.get_tab(t.tab_id));
        let Some(tab) = tab else {
            return Ok(());
        };
        let size = tab.get_size();
        let left = if chrome { 0 } else { x.saturating_sub(1) };
        let top = if chrome { 0 } else { y.saturating_sub(1) };
        let width = if chrome {
            size.cols
        } else {
            cols + 1 + usize::from(x != 0)
        };
        let height = if chrome {
            size.rows
        } else {
            rows + 1 + usize::from(y != 0)
        };
        let id = if chrome {
            u64::MAX / 4 - window_id
        } else {
            u64::MAX / 2 - parent
        };
        let existing = self
            .remote_panes
            .lock()
            .get(&id)
            .map(|p| p.lock().local_pane_id);
        let local = if let Some(local) = existing.and_then(|id| mux.get_pane(id)) {
            local
        } else {
            let local = self.create_pane(&PaneItem {
                session_id,
                window_id,
                pane_id: id,
                _pane_index: 0,
                cursor_x: 0,
                cursor_y: 0,
                pane_width: width as u64,
                pane_height: height as u64,
                pane_left: left as u64,
                pane_top: top as u64,
                pane_active: false,
                pane_floating: true,
                pane_border: false,
                window_zoomed: false,
                popup_frame: None,
            })?;
            if chrome {
                local
                    .downcast_ref::<LocalPane>()
                    .unwrap()
                    .mark_tmux_chrome();
            } else {
                local
                    .downcast_ref::<LocalPane>()
                    .unwrap()
                    .mark_tmux_surface();
            }
            self.remote_panes.lock().get(&id).unwrap().lock().key_target = Some(parent);
            mux.add_pane(&local)?;
            local
        };
        let size = tab.get_size();
        let pixel_width = width * size.pixel_width / size.cols.max(1);
        let pixel_height = height * size.pixel_height / size.rows.max(1);
        let old = local.get_dimensions();
        if old.cols != width || old.viewport_rows != height {
            local.resize(TerminalSize {
                cols: width,
                rows: height,
                pixel_width,
                pixel_height,
                dpi: size.dpi,
            })?;
        }
        let window_tabs = if chrome
            && config::configuration().tmux_continuous_window_tabs
            && self.ui_prompt.lock().is_none()
            && !self.remote_panes.lock().values().any(|p| {
                let p = p.lock();
                p.window_id == window_id && p.mode_active
            }) {
            tabs.filter(|(text, _)| !text.is_empty())
        } else {
            None
        };
        if let Some(p) = self.remote_panes.lock().get(&id) {
            let mut p = p.lock();
            p.pane_left = left as u64;
            p.pane_top = top as u64;
            p.pane_width = width as u64;
            p.pane_height = height as u64;
        }
        let mut frame = b"\x0f\x1b[0m\x1b)0\x1b[H\x1b[2J\x1b[?25l".to_vec();
        frame.extend_from_slice(text);
        if let Some((tabs, regions)) = window_tabs {
            // Cover pane-local statuses/junctions with one opaque bottom row.
            write!(frame, "\x1b[{};1H{}{}", height, tabs, "─".repeat(width))?;
            self.window_tab_regions.lock().insert(window_id, regions.clone());
        }
        local.downcast_ref::<LocalPane>().unwrap().set_tmux_surface(&frame);
        let positioned = crate::tab::PositionedPane {
            index: 0,
            is_active: false,
            is_zoomed: false,
            is_floating: true,
            left,
            top,
            width,
            height,
            pixel_width,
            pixel_height,
            pane: local,
        };
        if chrome {
            tab.set_remote_chrome(positioned);
        } else {
            tab.set_remote_frame(parent_local, positioned);
        }
        if chrome && *self.active_remote_window.lock() == Some(window_id) {
            self.present_remote_window(window_id);
        }
        Ok(())
    }

    fn sync_pane_state(&self, panes: &[PaneItem]) -> anyhow::Result<()> {
        let Some(current_session) = *self.tmux_session.lock() else {
            return Ok(());
        };
        let mux = Mux::get();

        for pane in panes.iter() {
            if pane.session_id != current_session {
                continue;
            }
            if !self.check_pane_attached(pane.window_id, pane.pane_id) {
                let tab_id = self.gui_tabs.lock().get(&pane.window_id).map(|t| t.tab_id);
                let Some(tab) = tab_id.and_then(|id| mux.get_tab(id)) else {
                    continue;
                };
                let had_native = self.remote_panes.lock().get(&pane.pane_id)
                    .and_then(|p| mux.get_pane(p.lock().local_pane_id)).is_some();
                let local = self.create_pane(pane)?;
                mux.add_pane(&local)?;
                if pane.pane_floating {
                    tab.append_floating_pane(local.clone());
                }
                self.add_attached_pane(pane.window_id, pane.pane_id)?;
                if !had_native {
                    self.cmd_queue.lock().push_back(Box::new(CapturePane {
                        pane_id: pane.pane_id,
                        history_limit: 100000,
                    }));
                }
            }

            // We now have the cursor information, fix the cursor position
            let pane_map = self.remote_panes.lock();
            let local_pane = match pane_map.get(&pane.pane_id) {
                Some(p) => {
                    let local_pane_id = p.lock().local_pane_id;
                    mux.get_pane(local_pane_id)
                }
                None => None,
            };

            if let Some(local_pane) = local_pane {
                if pane.pane_border
                    && self
                        .support_commands
                        .lock()
                        .get("capture-pane")
                        .map(|v| v.contains('U'))
                        .unwrap_or(false)
                {
                    if let Some(local) = local_pane.downcast_ref::<LocalPane>() {
                        local.ensure_tmux_status();
                    }
                }
                if let Some(remote) = pane_map.get(&pane.pane_id) {
                    let mut remote = remote.lock();
                    remote.pane_width = pane.pane_width;
                    remote.pane_height = pane.pane_height;
                    remote.cursor_x = pane.cursor_x;
                    remote.cursor_y = pane.cursor_y;
                    remote.pane_left = pane.pane_left;
                    remote.pane_top = pane.pane_top;
                }
                let tab_size = self
                    .gui_tabs
                    .lock()
                    .get(&pane.window_id)
                    .and_then(|t| mux.get_tab(t.tab_id))
                    .map(|t| t.get_size());
                let old = local_pane.get_dimensions();
                let cols = pane.pane_width as usize;
                let rows = pane.pane_height as usize + usize::from(pane.pane_border);
                let (cell_width, cell_height, dpi) = tab_size
                    .map(|s| {
                        (
                            s.pixel_width / s.cols.max(1),
                            s.pixel_height / s.rows.max(1),
                            s.dpi,
                        )
                    })
                    .unwrap_or((0, 0, old.dpi));
                if old.cols != cols
                    || old.viewport_rows != rows
                    || (old.pixel_width == 0 && cell_width != 0)
                {
                    local_pane.resize(TerminalSize {
                        cols,
                        rows,
                        pixel_width: cols * cell_width,
                        pixel_height: rows * cell_height,
                        dpi,
                    })?;
                    if let Some(remote) = pane_map.get(&pane.pane_id) {
                        let mut remote = remote.lock();
                        if remote.mode_active {
                            remote.mode_resized = true;
                        }
                    }
                    self.cmd_queue.lock().push_back(Box::new(CapturePane {
                        pane_id: pane.pane_id,
                        history_limit: 0,
                    }));
                }
                if pane.pane_floating {
                    if let Some(local) = local_pane.downcast_ref::<LocalPane>() {
                        local.mark_tmux_overlay();
                        local.set_tmux_popup_frame(pane.popup_frame.clone());
                    }
                    let tab_id = self.gui_tabs.lock().get(&pane.window_id).map(|t| t.tab_id);
                    if let Some(tab) = tab_id.and_then(|id| mux.get_tab(id)) {
                        tab.set_remote_floating_geometry(
                            local_pane.pane_id(),
                            pane.pane_left as usize,
                            pane.pane_top as usize,
                            pane.pane_width as usize,
                            pane.pane_height as usize,
                        );
                    }
                }
                if pane.pane_active {
                    self.active_remote_panes
                        .lock()
                        .insert(pane.window_id, pane.pane_id);
                }
            }

            log::info!("new pane synced, id: {}", pane.pane_id);
        }

        if let Some(first) = panes.first() {
            let tab_id = self.gui_tabs.lock().get(&first.window_id).map(|t| t.tab_id);
            if let Some(tab) = tab_id.and_then(|id| mux.get_tab(id)) {
                let pane_map = self.remote_panes.lock();
                let geometry: Vec<_> = panes
                    .iter()
                    .filter_map(|p| {
                        let local_id = pane_map.get(&p.pane_id)?.lock().local_pane_id;
                        Some(crate::tab::RemotePaneGeometry {
                            pane: mux.get_pane(local_id)?,
                            left: p.pane_left as usize,
                            top: p.pane_top as usize,
                            cols: p.pane_width as usize,
                            rows: p.pane_height as usize + usize::from(p.pane_border),
                            floating: p.pane_floating,
                            active: p.pane_active,
                        })
                    })
                    .collect();
                drop(pane_map);
                tab.sync_remote_layout(&geometry, first.window_zoomed)?;
                // Server pane focus remains on the underlying application
                // while its client prompt/menu/popup receives the keys.
                let ui = (*self.ui_prompt.lock()).or(*self.ui_overlay.lock());
                if let Some(local_id) = ui.and_then(|id| {
                    self.remote_panes
                        .lock()
                        .get(&id)
                        .map(|p| p.lock().local_pane_id)
                }) {
                    if let Some((index, _)) = tab.get_floating_pane_by_pane_id(local_id) {
                        tab.set_active_floating_pane(index);
                        tab.set_floating_pane_visibility(true);
                    }
                }
            }
        }

        Ok(())
    }

    fn sync_window_state(&self, session: TmuxSessionId, windows: &[WindowItem], panes: &[PaneItem], new_window: bool) -> anyhow::Result<()> {
        // Pane adoption can transiently empty a native tab. Keep the parent
        // window alive until the complete reconciliation has finished.
        let _reconcile_activity = Activity::new();
        let Some(current_session) = *self.tmux_session.lock() else {
            return Ok(());
        };
        // Restore plugins can switch sessions several times while a window
        // list is in flight. An old session's reply must never retire the
        // current session's native tabs.
        if current_session != session { return Ok(()); }
        let mux = Mux::get();

        let existing_native: HashSet<_> = self.remote_panes.lock().iter()
            .filter_map(|(id, p)| mux.get_pane(p.lock().local_pane_id).map(|_| *id)).collect();

        // Adopt all real panes (including floats) from the same server read
        // before retiring anything. Window layouts alone omit floating panes.
        for pane in panes.iter().filter(|p| p.session_id == current_session) {
            if let Some(remote) = self.remote_panes.lock().get(&pane.pane_id) {
                remote.lock().window_id = pane.window_id;
            }
        }

        self.create_gui_window();
        let mut gui_window = self.gui_window.lock();
        let gui_window_id = match gui_window.as_mut() {
            Some(x) => x,
            None => {
                anyhow::bail!("No tmux gui created");
            }
        };

        let mut history_captures = vec![];
        for window in windows.iter() {
            if window.session_id != current_session {
                continue;
            }

            if self.check_window_attached(window.window_id) {
                continue;
            }

            let size = TerminalSize {
                rows: window.window_height as usize,
                cols: window.window_width as usize,
                pixel_width: 0,
                pixel_height: 0,
                dpi: 0,
            };

            let tab = Arc::new(Tab::new(&size));
            tab.set_title(&format!("{}", &window.window_name));
            mux.add_tab_no_panes(&tab);

            let _ = self.add_attached_window(window, &tab.tab_id())?;

            let mut split_stack;
            let mut split_direction;

            let mut split_pane_index = 1;
            for l in &window.layout {
                match l {
                    WindowLayout::SinglePane(x) => {
                        let p = PaneItem {
                            session_id: window.session_id,
                            window_id: window.window_id,
                            _pane_index: 0,
                            cursor_x: 0,
                            cursor_y: 0,
                            pane_active: false,
                            pane_floating: false,
                            pane_border: false,
                            window_zoomed: false,
                            pane_id: x.pane_id,
                            pane_width: x.pane_width,
                            pane_height: x.pane_height,
                            pane_left: x.pane_left,
                            pane_top: x.pane_top,
                            popup_frame: None,
                        };
                        let local_pane = self.create_pane(&p).context("failed to create pane")?;
                        tab.assign_pane(&local_pane);
                        self.add_attached_pane(p.window_id, p.pane_id)?;
                        let _ = mux.add_pane(&local_pane);
                        break;
                    }

                    WindowLayout::SplitHorizontal(x) => {
                        split_direction = SplitDirection::Horizontal;
                        split_stack = x;
                    }

                    WindowLayout::SplitVertical(x) => {
                        split_direction = SplitDirection::Vertical;
                        split_stack = x;
                    }
                }

                for x in split_stack {
                    let p = PaneItem {
                        session_id: window.session_id,
                        window_id: window.window_id,
                        _pane_index: 0,
                        cursor_x: 0,
                        cursor_y: 0,
                        pane_active: false,
                        pane_floating: false,
                        pane_border: false,
                        window_zoomed: false,
                        pane_id: x.pane_id,
                        pane_width: x.pane_width,
                        pane_height: x.pane_height,
                        pane_left: x.pane_left,
                        pane_top: x.pane_top,
                        popup_frame: None,
                    };
                    let local_pane;
                    if !self.check_pane_attached(p.window_id, p.pane_id) {
                        local_pane = self.create_pane(&p).context("failed to create pane")?;
                        self.add_attached_pane(p.window_id, p.pane_id)?;
                        let _ = mux.add_pane(&local_pane);
                        if let None = tab.get_active_pane() {
                            tab.assign_pane(&local_pane);
                            split_pane_index = tab.get_active_idx();
                            continue;
                        }

                        split_pane_index = tab.split_and_insert(
                            split_pane_index,
                            SplitRequest {
                                direction: split_direction,
                                target_is_second: false,
                                top_level: false,
                                size: SplitSize::Cells(
                                    if split_direction == SplitDirection::Horizontal {
                                        p.pane_width as usize
                                    } else {
                                        p.pane_height as usize
                                    },
                                ),
                            },
                            local_pane.clone(),
                        )? + 1;
                    } else {
                        let pane_map = self.remote_panes.lock();
                        let local_pane_id = match pane_map.get(&p.pane_id) {
                            Some(x) => x.lock().local_pane_id,
                            None => anyhow::bail!("cannot find the local pane for {}", p.pane_id),
                        };

                        split_pane_index = match tab
                            .iter_panes_ignoring_zoom()
                            .iter()
                            .find(|x| x.pane.pane_id() == local_pane_id)
                        {
                            Some(x) => x.index,
                            None => {
                                log::info!("invalid pane id {local_pane_id}");
                                continue;
                            }
                        };
                        continue;
                    }
                }
            }

            mux.add_tab_to_window(&tab, **gui_window_id)?;
            gui_window_id.notify();

            let gui_tabs = self.gui_tabs.lock();
            let local_tab = match gui_tabs.get(&window.window_id) {
                Some(x) => x,
                None => {
                    log::info!(
                        "cannot find the local tab for tmux window {}",
                        window.window_id
                    );
                    continue;
                }
            };

            // For new window, we wait for natural output instead of capturing
            if !new_window {
                for p in local_tab.panes.iter().filter(|p| !existing_native.contains(p)) {
                    history_captures.push((window.window_active, *p, window.history_limit));
                }
            }

        }

        // Install every destination layout before pruning any source window.
        // Deferring this to separate list-panes replies lets remove_tab kill a
        // moved terminal still referenced by its old native tab.
        for window in windows {
            let window_panes: Vec<_> = panes.iter().filter(|p| p.window_id == window.window_id).cloned().collect();
            self.sync_pane_state(&window_panes)?;
        }
        for window in windows {
            let pane_set = panes.iter().filter(|p| p.window_id == window.window_id).map(|p| p.pane_id).collect();
            self.remove_detached_pane(window.window_id, &pane_set)?;
        }

        let local_window = **gui_window_id;
        drop(gui_window);
        if *self.tmux_session.lock() != Some(current_session) { return Ok(()); }
        if !new_window {
            let old_windows: Vec<_> = self
                .gui_tabs
                .lock()
                .keys()
                .copied()
                .filter(|id| !windows.iter().any(|w| w.window_id == *id))
                .collect();
            for id in old_windows {
                self.remove_detached_window(id)?;
            }
        }
        if let Some(transport) = *self.transport_tab.lock() {
            if let Some(mut window) = mux.get_window_mut(local_window) {
                window.remove_tab_id(transport);
            }
        }
        if let Some(window) = windows.iter().find(|w| w.window_active) {
            self.focus_remote_window(window.window_id);
        }
        if *self.attach_state.lock() == AttachState::Init {
            self.cmd_queue.lock().push_back(Box::new(AttachDone));
        }
        // A full history can take seconds to transfer and parse. Show every
        // visible screen first (active window first), then load histories;
        // each history capture replaces its pane's screen and scrollback.
        // Input and UI reads are queued ahead of these captures.
        history_captures.sort_by_key(|(active, _, _)| !*active);
        {
            let mut queue = self.cmd_queue.lock();
            for (_, pane_id, _) in history_captures.iter() {
                queue.push_back(Box::new(CapturePane { pane_id: *pane_id, history_limit: 0 }));
            }
            for (_, pane_id, history_limit) in history_captures.iter() {
                if *history_limit != 0 {
                    queue.push_back(Box::new(CapturePane {
                        pane_id: *pane_id,
                        history_limit: *history_limit,
                    }));
                }
            }
        }

        TmuxDomainState::schedule_send_next_command(self.domain_id);

        Ok(())
    }

    pub fn subscribe_notification(&self) {
        let mux = Mux::get();
        let domain_id = self.domain_id;
        mux.subscribe(move |n| {
            promise::spawn::spawn_into_main_thread(async move {
                let mux = Mux::get();
                let domain = match mux.get_domain(domain_id) {
                    Some(d) => d,
                    None => return,
                };
                let tmux_domain = match domain.downcast_ref::<TmuxDomain>() {
                    Some(t) => t,
                    None => return,
                };

                if *tmux_domain.inner.attach_state.lock() == AttachState::Init {
                    return;
                }

                match n {
                    MuxNotification::PaneFocused(pane_id) => {
                        // client-ui routes user input through the server. Local
                        // surface teardown is not a request to select a pane.
                        if tmux_domain
                            .inner
                            .support_commands
                            .lock()
                            .get("send-keys")
                            .map(|s| s.contains("-m"))
                            .unwrap_or(false)
                        {
                            return;
                        }
                        let local_pane_id = pane_id;
                        let tmux_pane_id =
                            match tmux_domain.inner.remote_panes.lock().iter().find(|(_, p)| {
                                let p = p.lock();
                                p.local_pane_id == pane_id && p.key_target.is_none()
                            }) {
                                Some((_, p)) => {
                                    let p = p.lock();
                                    Some((p.window_id, p.pane_id))
                                }
                                None => None,
                            };

                        if let Some((window_id, pane_id)) = tmux_pane_id {
                            let tab = tmux_domain
                                .inner
                                .gui_tabs
                                .lock()
                                .get(&window_id)
                                .and_then(|t| mux.get_tab(t.tab_id));
                            let still_active = tab
                                .and_then(|t| t.get_active_pane())
                                .map(|p| p.pane_id() == local_pane_id)
                                .unwrap_or(false);
                            if !still_active {
                                return;
                            }
                            let mut active = tmux_domain.inner.active_remote_panes.lock();
                            if active.get(&window_id) == Some(&pane_id) {
                                return;
                            }
                            active.insert(window_id, pane_id);
                            drop(active);
                            tmux_domain
                                .inner
                                .cmd_queue
                                .lock()
                                .push_back(Box::new(SelectPane { pane_id: pane_id }));
                            TmuxDomainState::schedule_send_next_command(domain_id);
                        }
                    }
                    MuxNotification::WindowInvalidated(window_id) => {
                        if tmux_domain
                            .inner
                            .support_commands
                            .lock()
                            .get("send-keys")
                            .map(|s| s.contains("-m"))
                            .unwrap_or(false)
                        {
                            return;
                        }
                        if let Some(window) = mux.get_window(window_id) {
                            let Some(tab) = window.get_active_tab() else {
                                return;
                            };
                            let tmux_window_id = match tmux_domain
                                .inner
                                .gui_tabs
                                .lock()
                                .iter()
                                .find(|(_, t)| t.tab_id == tab.tab_id())
                            {
                                Some((_, t)) => Some(t.tmux_window_id),
                                None => None,
                            };
                            if let Some(window_id) = tmux_window_id {
                                let mut active = tmux_domain.inner.active_remote_window.lock();
                                if *active == Some(window_id) {
                                    return;
                                }
                                *active = Some(window_id);
                                drop(active);
                                tmux_domain.inner.cmd_queue.lock().push_back(Box::new(
                                    SelectWindow {
                                        window_id: window_id,
                                    },
                                ));
                                TmuxDomainState::schedule_send_next_command(domain_id);
                            }
                        }
                    }
                    _ => {}
                }
            })
            .detach();
            true
        });
    }
}

fn parse_sigil_number(text: &str) -> anyhow::Result<u64> {
    let num = text
        .get(1..)
        .ok_or_else(|| anyhow!("wrong prefixed id"))?
        .parse()?;

    Ok(num)
}

fn parse_pane_items(output: &str) -> anyhow::Result<Vec<PaneItem>> {
        let mut items = vec![];
        for line in output.split('\n') {
            if line.is_empty() {
                continue;
            }
            let mut fields = line.split(' ');
            // These ids all have various sigils such as `$`, `%`, `@`,
            // so skip those prior to parsing them
            let session_id =
                parse_sigil_number(fields.next().ok_or_else(|| anyhow!("missing session_id"))?)?;
            let window_id =
                parse_sigil_number(fields.next().ok_or_else(|| anyhow!("missing window_id"))?)?;
            let pane_id =
                parse_sigil_number(fields.next().ok_or_else(|| anyhow!("missing pane_id"))?)?;
            let _pane_index = fields
                .next()
                .ok_or_else(|| anyhow!("missing pane_index"))?
                .parse()?;
            let cursor_x = fields
                .next()
                .ok_or_else(|| anyhow!("missing cursor_x"))?
                .parse()?;
            let cursor_y = fields
                .next()
                .ok_or_else(|| anyhow!("missing cursor_y"))?
                .parse()?;
            let pane_width = fields
                .next()
                .ok_or_else(|| anyhow!("missing pane_width"))?
                .parse()?;
            let pane_height = fields
                .next()
                .ok_or_else(|| anyhow!("missing pane_height"))?
                .parse()?;
            let pane_left = fields
                .next()
                .ok_or_else(|| anyhow!("missing pane_left"))?
                .parse()?;
            let pane_top = fields
                .next()
                .ok_or_else(|| anyhow!("missing pane_top"))?
                .parse()?;
            let pane_active = fields
                .next()
                .ok_or_else(|| anyhow!("missing pane_active"))?
                .parse::<usize>()?;

            let pane_active = pane_active == 1;
            let pane_floating = fields.next().unwrap_or("0") == "1";
            let pane_border = fields.next().unwrap_or("off") == "bottom" && !pane_floating;
            let window_zoomed = fields.next().unwrap_or("0") == "1";
            // A popup whose frame the GUI draws (companion tmux, -G): border
            // style, then the title as the rest of the line (it has spaces).
            let popup = fields.next() == Some("1");
            let border_style = fields.next().unwrap_or("").to_string();
            let title = fields.collect::<Vec<_>>().join(" ");
            let popup_frame = (popup && pane_floating).then(|| crate::pane::PopupFrame {
                border_style,
                title,
                subtitle: String::new(),
            });


            items.push(PaneItem {
                session_id,
                window_id,
                pane_id,
                _pane_index,
                cursor_x,
                cursor_y,
                pane_width,
                pane_height,
                pane_left,
                pane_top,
                pane_active,
                pane_floating,
                pane_border,
                window_zoomed,
                popup_frame,
            });
        }

        Ok(items)
}

#[derive(Debug)]
pub(crate) struct ListAllWindows {
    pub session_id: TmuxSessionId,
    pub window_id: Option<TmuxWindowId>,
}

impl TmuxCommand for ListAllWindows {
    fn snapshot_key(&self) -> Option<(&'static str, TmuxPaneId)> {
        self.window_id.is_none().then_some(("windows", self.session_id))
    }
    fn response_count(&self) -> usize { 3 }
    fn get_command(&self, _domain_id: DomainId) -> String {
        format!(
            "list-windows -F \
                '#{{session_id}} #{{window_id}} \
                #{{window_width}} #{{window_height}} \
                #{{window_active}} \
                #{{window_layout}} \
                #{{history_limit}} #{{window_name}}' -t ${0} ; list-panes -s -t ${0} -F 'P #{{session_id}} #{{window_id}} #{{pane_id}} #{{pane_index}} #{{cursor_x}} #{{cursor_y}} #{{pane_width}} #{{pane_height}} #{{pane_left}} #{{pane_top}} #{{pane_active}} #{{pane_floating_flag}} #{{pane-border-status}} #{{window_zoomed_flag}} #{{?#{{@tmcc-popup}},1,0}} #{{pane-active-border-style}} #{{pane_title}}' ; {1}\n",
            self.session_id,
            crate::tmux_strip::strip_list_command(self.session_id)
        )
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("list-window in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        self.apply(domain_id, &result.output, false)
    }
}

impl ListAllWindows {
    /// Panes whose status row must be read before their new geometry is
    /// shown: a moved pane would otherwise display its old status (e.g. the
    /// window tabs of a former bottom pane in the middle of the window).
    fn panes_needing_status(tmux: &TmuxDomainState, panes: &[PaneItem]) -> Vec<TmuxPaneId> {
        if !tmux.support_commands.lock().get("capture-pane").map(|v| v.contains('U')).unwrap_or(false) {
            return vec![];
        }
        let Some(session) = *tmux.tmux_session.lock() else { return vec![]; };
        let remote = tmux.remote_panes.lock();
        panes.iter()
            .filter(|p| p.session_id == session && p.pane_border && !p.pane_floating)
            .filter(|p| remote.get(&p.pane_id).map(|r| {
                let r = r.lock();
                (r.pane_left, r.pane_top, r.pane_width, r.pane_height)
                    != (p.pane_left, p.pane_top, p.pane_width, p.pane_height)
            }).unwrap_or(true))
            .map(|p| p.pane_id)
            .collect()
    }

    fn apply(&self, domain_id: DomainId, output: &str, statuses_read: bool) -> anyhow::Result<()> {
        let pane_text: String = output.lines().filter_map(|l| l.strip_prefix("P ")).map(|l| format!("{l}\n")).collect();
        let panes = parse_pane_items(&pane_text)?;
        let mut items = vec![];

        let strip = crate::tmux_strip::parse_strip_windows(output);
        for line in output.split('\n') {
            if line.is_empty() || line.starts_with("P ") || line.starts_with("S ") {
                continue;
            }
            let mut fields = line.splitn(8, ' ');
            let session_id =
                parse_sigil_number(fields.next().ok_or_else(|| anyhow!("missing session_id"))?)?;
            let window_id =
                parse_sigil_number(fields.next().ok_or_else(|| anyhow!("missing window_id"))?)?;
            let window_width = fields
                .next()
                .ok_or_else(|| anyhow!("missing window_width"))?
                .parse()?;
            let window_height = fields
                .next()
                .ok_or_else(|| anyhow!("missing window_height"))?
                .parse()?;
            let window_active = fields
                .next()
                .ok_or_else(|| anyhow!("missing window_active"))?
                .parse::<usize>()?;

            let window_layout = fields
                .next()
                .ok_or_else(|| anyhow!("missing window_layout"))?;

            let history_limit = fields
                .next()
                .ok_or_else(|| anyhow!("missing history_limit"))?
                .parse::<isize>()?;

            let window_name = fields
                .next()
                .ok_or_else(|| anyhow!("missing window_name"))?;

            let window_active = window_active == 1;

            if let Some(x) = self.window_id {
                if x != window_id {
                    continue;
                }
            }

            let layout_csum = window_layout
                .get(0..4)
                .ok_or_else(|| anyhow!("missing window_layout"))?;
            let window_layout = window_layout
                .get(5..)
                .ok_or_else(|| anyhow!("missing window_layout"))?;

            let layout = parse_layout(window_layout)?;

            items.push(WindowItem {
                session_id,
                window_id,
                window_width,
                window_height,
                window_active,
                window_name: window_name.to_string(),
                layout,
                layout_csum: layout_csum.to_string(),
                history_limit,
            });
        }

        log::debug!("layout in domain_id {}: {:#?}", domain_id, items);
        let mux = Mux::get();
        if let Some(domain) = mux.get_domain(domain_id) {
            if let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() {
                let new_window = if let Some(_x) = self.window_id {
                    true
                } else {
                    false
                };
                if !statuses_read {
                    let needed = Self::panes_needing_status(&tmux_domain.inner, &panes);
                    if !needed.is_empty() {
                        let active = items.iter().find(|w| w.window_active).map(|w| w.window_id);
                        let chrome = config::configuration().tmux_continuous_window_tabs
                            .then(|| panes.iter().find(|p| Some(p.window_id) == active && !p.pane_floating))
                            .flatten()
                            .map(|p| p.pane_id);
                        tmux_domain.inner.enqueue_client_refresh(Box::new(LayoutWithStatuses {
                            listing: ListAllWindows { session_id: self.session_id, window_id: self.window_id },
                            output: output.to_string(),
                            panes: needed,
                            chrome,
                        }));
                        return Ok(());
                    }
                }
                // The strip changes in the same step as the layout and tabs.
                if !strip.is_empty() {
                    tmux_domain.inner.set_strip_windows(strip);
                }
                return tmux_domain.inner.sync_window_state(self.session_id, &items, &panes, new_window);
            }
        }
        anyhow::bail!("Tmux domain lost");
    }
}

/// Applies a window listing together with the status rows of panes whose
/// geometry changed, in one main-thread step. No frame shows the new layout
/// with stale status rows.
#[derive(Debug)]
pub(crate) struct LayoutWithStatuses {
    listing: ListAllWindows,
    output: String,
    panes: Vec<TmuxPaneId>,
    chrome: Option<TmuxPaneId>,
}

impl TmuxCommand for LayoutWithStatuses {
    fn response_count(&self) -> usize { self.panes.len() * 2 + if self.chrome.is_some() { 3 } else { 0 } }
    fn get_command(&self, _domain_id: DomainId) -> String {
        let mut reads: Vec<String> = self.chrome.iter().map(|p| format!(
            "display-message -p -t %{p} 'WEZTERM-CHROME' ; {}", crate::tmux_keys::chrome_reads(*p)
        )).collect();
        reads.extend(self.panes.iter().map(|p| format!(
            "display-message -p -t %{p} 'WEZTERM-STATUS {p}' ; capture-pane -p -e -C -U -V -t %{p}"
        )));
        format!("{}\n", reads.join(" ; "))
    }
    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        self.listing.apply(domain_id, &self.output, true)?;
        let mux = Mux::get();
        let Some(domain) = mux.get_domain(domain_id) else { return Ok(()); };
        let Some(tmux) = domain.downcast_ref::<TmuxDomain>() else { return Ok(()); };
        if result.error {
            // A pane closed meanwhile; read the remaining statuses normally.
            for pane in &self.panes {
                tmux.inner.enqueue_client_refresh(Box::new(crate::tmux_keys::InspectMode { pane: *pane }));
            }
            return Ok(());
        }
        let mut current: Option<(TmuxPaneId, Vec<&str>)> = None;
        let mut statuses = vec![];
        let mut chrome: Option<Vec<&str>> = None;
        for line in result.output.split('\n') {
            if line == "WEZTERM-CHROME" {
                chrome = Some(vec![]);
            } else if let Some(id) = line.strip_prefix("WEZTERM-STATUS ").and_then(|v| v.trim().parse().ok()) {
                if let (Some(pane), Some(lines)) = (self.chrome, chrome.take()) {
                    if let Some((tabs, body)) = lines.split_first() {
                        let tabs = crate::tmux_keys::decode_window_tabs(tabs);
                        let text = termwiz::tmux_cc::unvis(&body.join("\n"))?;
                        let text = text.strip_suffix('\n').unwrap_or(&text).replace('\n', "\r\n");
                        tmux.inner.update_float_frame(pane, text.as_bytes(), true, Some(&tabs))?;
                    }
                }
                statuses.extend(current.take());
                current = Some((id, vec![]));
            } else if let Some((_, lines)) = current.as_mut() {
                lines.push(line);
            } else if let Some(lines) = chrome.as_mut() {
                lines.push(line);
            }
        }
        statuses.extend(current.take());
        for (pane, lines) in statuses {
            let Some((header, body)) = lines.split_first() else { continue; };
            // A client prompt replaces every status capture; leave statuses
            // to the prompt surface in that case.
            if header.split_whitespace().nth(1) == Some("1") { continue; }
            let text = termwiz::tmux_cc::unvis(&body.join("\n"))?;
            let local = tmux.inner.remote_panes.lock().get(&pane)
                .and_then(|p| mux.get_pane(p.lock().local_pane_id));
            if let Some(local) = local {
                if local.has_server_frame() { continue; }
                if let Some(local) = local.downcast_ref::<LocalPane>() {
                    local.set_tmux_status(text.trim_end_matches('\n').as_bytes());
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct Resize {
    pub pane_id: TmuxPaneId,
    pub size: PtySize,
}

impl TmuxCommand for Resize {
    fn get_command(&self, domain_id: DomainId) -> String {
        let mux = Mux::get();
        let domain = match mux.get_domain(domain_id) {
            Some(d) => d,
            None => return "".to_string(),
        };
        let tmux_domain = match domain.downcast_ref::<TmuxDomain>() {
            Some(t) => t,
            None => return "".to_string(),
        };

        // Not in stable state for now, don't do resizing, otherwise it will cause tmux output
        // unexpected content.
        if *tmux_domain.inner.attach_state.lock() == AttachState::Init {
            return "".to_string();
        }

        let pane_map = tmux_domain.inner.remote_panes.lock();
        {
            let mut pane = match pane_map.get(&self.pane_id) {
                Some(x) => x.lock(),
                None => return "".to_string(),
            };
            if pane.key_target.is_some() {
                return String::new();
            }

            if pane.pane_width == self.size.cols as u64 && pane.pane_height == self.size.rows as u64
            {
                return "".to_string();
            } else {
                pane.pane_width = self.size.cols as u64;
                pane.pane_height = self.size.rows as u64;
            }
        }

        let tmux_window_id = match pane_map.get(&self.pane_id) {
            Some(x) => x.lock().window_id,
            None => return "".to_string(),
        };

        let gui_tabs = tmux_domain.inner.gui_tabs.lock();
        let local_tab = match gui_tabs.get(&tmux_window_id) {
            Some(t) => t,
            None => return "".to_string(),
        };

        let size = match mux.get_tab(local_tab.tab_id) {
            Some(x) => x.get_size(),
            None => return "".to_string(),
        };

        let support_commands = tmux_domain.inner.support_commands.lock();

        if let Some(x) = support_commands.get("refresh-client") {
            if x.contains("-C XxY") || x.contains("-C XxY[") {
                format!(
                    "refresh-client -C {}x{},{},{}\n",
                    size.cols,
                    size.rows,
                    size.pixel_width / size.cols.max(1),
                    size.pixel_height / size.rows.max(1)
                )
            } else {
                format!("refresh-client -C {},{}\n", size.cols, size.rows)
            }
        } else {
            String::new()
        }
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("resize-pane in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct CapturePane {
    pub(crate) pane_id: TmuxPaneId,
    pub(crate) history_limit: isize,
}

impl TmuxCommand for CapturePane {
    fn response_count(&self) -> usize { 2 }
    fn get_command(&self, _domain_id: DomainId) -> String {
        // Read the cursor in the same command list: a cursor from an older
        // list-panes reply can be stale by the time the screen is captured.
        // -J joins wrapped lines: the native terminal rewraps them at the same
        // width and keeps them one logical line (URL quick select, search).
        format!(
            "display-message -p -t %{0} '#{{cursor_x}} #{{cursor_y}}' ; capture-pane -p -t %{0} -e -C -J -S {1}\n",
            self.pane_id,
            self.history_limit * -1
        )
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("capture-pane in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        let mux = Mux::get();
        let domain = match mux.get_domain(domain_id) {
            Some(d) => d,
            None => anyhow::bail!("Tmux domain lost"),
        };
        let tmux_domain = match domain.downcast_ref::<TmuxDomain>() {
            Some(t) => t,
            None => anyhow::bail!("Tmux domain lost"),
        };

        let (cursor, output) = result.output.split_once('\n').unwrap_or(("", &result.output));
        let mut cursor = cursor.split_whitespace().map(|v| v.parse::<u64>().ok());
        let cursor_x = cursor.next().flatten();
        let cursor_y = cursor.next().flatten();
        let unescaped = termwiz::tmux_cc::unvis(output).context("unescape pane content")?;
        // capturep contents returned from guarded lines which always contain a tailing '\n'
        let unescaped = &unescaped[0..unescaped.len().saturating_sub(1)].replace("\n", "\r\n");

        let pane_map = tmux_domain.inner.remote_panes.lock();
        if let Some(pane) = pane_map.get(&self.pane_id) {
            let mut pane = pane.lock();
            if pane.mode_active && self.history_limit == 0 {
                return Ok(());
            }
            if let (Some(x), Some(y)) = (cursor_x, cursor_y) {
                pane.cursor_x = x;
                pane.cursor_y = y;
            }
            // Replace the screen (and, for a history capture, the scrollback)
            // with tmux's exact state, then place the cursor. Never use
            // DECSC/DECRC here: they would overwrite the application's own
            // saved cursor, and a later DECRC from the application (such as
            // an instant prompt being replaced) then lands on the wrong row.
            // Mode 9728 keeps passthrough images (imgcat), which tmux cannot
            // capture, on rows that stay blank.
            let mut frame = b"\x1b[?2026h\x1b[?9728h\x0f\x1b[0m\x1b[H\x1b[2J".to_vec();
            if self.history_limit != 0 {
                frame.extend_from_slice(b"\x1b[3J");
            }
            frame.extend_from_slice(unescaped.as_bytes());
            write!(frame, "\x1b[{};{}H\x1b[?9728l\x1b[?2026l", pane.cursor_y + 1, pane.cursor_x + 1)?;
            if pane.mode_active {
                // Applied after the mode screen is dismissed.
                pane.mode_backlog.extend_from_slice(&frame);
            } else {
                pane.output_write.write_all(&frame)?;
            }
        }
        drop(pane_map);

        if tmux_domain
            .inner
            .support_commands
            .lock()
            .get("capture-pane")
            .map(|v| v.contains('G'))
            .unwrap_or(false)
        {
            tmux_domain.inner.cmd_queue.lock().push_back(Box::new(
                crate::tmux_keys::CaptureImages { pane: self.pane_id },
            ));
            TmuxDomainState::schedule_send_next_command(domain_id);
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct SendKeys {
    pub keys: Vec<u8>,
    pub pane: TmuxPaneId,
}
impl TmuxCommand for SendKeys {
    fn get_command(&self, _domain_id: DomainId) -> String {
        let mut s = String::new();
        for &byte in self.keys.iter() {
            write!(&mut s, "0x{:X} ", byte).expect("unable to write key");
        }
        format!("send-keys -H -t %{} {}\n", self.pane, s)
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("send-key in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct NewWindow;
impl TmuxCommand for NewWindow {
    fn get_command(&self, _domain_id: DomainId) -> String {
        "new-window\n".to_owned()
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("new-window in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct ListCommands;
impl TmuxCommand for ListCommands {
    fn get_command(&self, _domain_id: DomainId) -> String {
        "list-commands\n".to_owned()
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("list-command in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        let mux = Mux::get();
        let domain = match mux.get_domain(domain_id) {
            Some(d) => d,
            None => anyhow::bail!("Tmux domain lost"),
        };
        let tmux_domain = match domain.downcast_ref::<TmuxDomain>() {
            Some(t) => t,
            None => anyhow::bail!("Tmux domain lost"),
        };

        let mut support_commands = tmux_domain.inner.support_commands.lock();

        for line in result.output.split('\n') {
            if line.is_empty() {
                continue;
            }
            let v: Vec<&str> = line.split(' ').collect();
            support_commands.insert(v[0].to_string(), line.to_string());
        }

        let mut cmd_queue = tmux_domain.inner.cmd_queue.as_ref().lock();
        if support_commands
            .get("capture-pane")
            .map(|s| s.contains('U'))
            .unwrap_or(false)
        {
            cmd_queue.push_back(Box::new(crate::tmux_keys::EnableClientUi));
        }
        if let Some(session) = *tmux_domain.inner.tmux_session.lock() {
            cmd_queue.push_back(Box::new(ListAllWindows {
                session_id: session,
                window_id: None,
            }));
            TmuxDomainState::schedule_send_next_command(domain_id);
        }

        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct SplitPane {
    pub pane_id: TmuxPaneId,
    pub direction: SplitDirection,
}

impl TmuxCommand for SplitPane {
    fn get_command(&self, _domain_id: DomainId) -> String {
        if self.direction == SplitDirection::Horizontal {
            format!("split-window -h -t %{}\n", self.pane_id)
        } else {
            format!("split-window -v -t %{}\n", self.pane_id)
        }
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("split-window in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct SelectWindow {
    pub window_id: TmuxWindowId,
}

impl TmuxCommand for SelectWindow {
    fn get_command(&self, _domain_id: DomainId) -> String {
        format!("select-window -t @{}\n", self.window_id)
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("select-window in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct SelectPane {
    pub pane_id: TmuxPaneId,
}

impl TmuxCommand for SelectPane {
    fn get_command(&self, domain_id: DomainId) -> String {
        if let Some(domain) = Mux::get().get_domain(domain_id) {
            if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                if !tmux.inner.remote_panes.lock().contains_key(&self.pane_id) {
                    return String::new();
                }
            }
        }
        format!("select-pane -t %{}\n", self.pane_id)
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("select-pane in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        Ok(())
    }
}

// This is a dummy command which indicates the attaching is done, it prevents the tmux output
// the unexpected and unnecessary content when syncing with back end in attaching stage.
#[derive(Debug)]
pub(crate) struct AttachDone;
impl TmuxCommand for AttachDone {
    fn get_command(&self, _domain_id: DomainId) -> String {
        // The command doesn't matter, just give a legal simple command to let process_result called.
        "list-session\n".to_string()
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("list-session in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        let mux = Mux::get();
        let domain = match mux.get_domain(domain_id) {
            Some(d) => d,
            None => anyhow::bail!("Tmux domain lost"),
        };
        let tmux_domain = match domain.downcast_ref::<TmuxDomain>() {
            Some(t) => t,
            None => anyhow::bail!("Tmux domain lost"),
        };

        // Do nothing, just change the state.
        *tmux_domain.inner.attach_state.lock() = AttachState::Done;
        Ok(())
    }
}

/// Places a floating pane after the GUI dragged it (companion tmux:
/// resize-pane -X/-Y set the content's left and top cell).
#[derive(Debug)]
pub(crate) struct MoveFloatingPane {
    pub pane_id: TmuxPaneId,
    pub left: usize,
    pub top: usize,
}

impl TmuxCommand for MoveFloatingPane {
    fn get_command(&self, _domain_id: DomainId) -> String {
        format!("resize-pane -t %{} -X {} -Y {}\n", self.pane_id, self.left, self.top)
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            log::error!("resize-pane -X/-Y in domain={domain_id} failed: {result:#?}");
        }
        Ok(())
    }
}

impl TmuxDomain {
    /// Called by the GUI when a floating pane drag ends.
    pub fn move_floating_pane(&self, local_pane_id: PaneId, left: usize, top: usize) {
        let remote = self
            .inner
            .remote_panes
            .lock()
            .values()
            .find(|p| p.lock().local_pane_id == local_pane_id)
            .map(|p| p.lock().pane_id);
        if let Some(pane_id) = remote {
            self.inner
                .cmd_queue
                .lock()
                .push_back(Box::new(MoveFloatingPane { pane_id, left, top }));
            TmuxDomainState::schedule_send_next_command(self.inner.domain_id);
        }
    }
}
