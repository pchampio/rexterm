//! Window tabs of a tmux -CC session, drawn by the GUI in the bottom row.
//!
//! tmux only reports the window list (one coalesced list-windows after
//! window, rename, layout and focus notifications); the strip itself, its
//! separators and its click regions are built here, so a tab switch or a tab
//! move repaints at once instead of waiting for a tmux-rendered border row.
use crate::domain::DomainId;
use crate::tab::TabId;
use crate::tmux::{TmuxDomain, TmuxDomainState};
use crate::tmux_commands::TmuxCommand;
use crate::Mux;
use termwiz::tmux_cc::*;
use wezterm_term::color::ColorAttribute;
use wezterm_term::{CellAttributes, Line};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripWindow {
    pub id: TmuxWindowId,
    pub index: u64,
    pub name: String,
    pub active: bool,
    pub zoomed: bool,
    pub synchronized: bool,
    /// pane-border-status is bottom: the strip has a row to live in.
    pub bottom_border: bool,
    /// The session's prefix key, in tmux key-name form (C-Space).
    pub prefix: String,
    /// display-time (ms) and message-style, for tmux messages in the strip row.
    pub display_time: u64,
    pub message_style: String,
    pub session: String,
}

/// User var that asks the GUI for the window overview (Exposé).
pub const OVERVIEW_USER_VAR: &str = "WEZTERM_TMUX_OVERVIEW";

/// A click region of the strip: columns [start, end) select `window`.
pub type StripRegion = (usize, usize, TmuxWindowId);

#[derive(Debug)]
pub(crate) struct ListStripWindows {
    pub session: TmuxSessionId,
}

impl TmuxCommand for ListStripWindows {
    fn snapshot_key(&self) -> Option<(&'static str, TmuxPaneId)> {
        Some(("strip", self.session))
    }
    fn get_command(&self, _: DomainId) -> String {
        format!("{}\n", strip_list_command(self.session))
    }
    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            return Ok(());
        }
        let mux = Mux::get();
        let Some(domain) = mux.get_domain(domain_id) else {
            return Ok(());
        };
        let Some(tmux) = domain.downcast_ref::<TmuxDomain>() else {
            return Ok(());
        };
        tmux.inner.set_strip_windows(parse_strip_windows(&result.output));
        Ok(())
    }
}

/// Window list for the strip; every line starts with "S ". The same command
/// runs with each full window listing (ListAllWindows), so the strip changes
/// in the same step as the layout and the tab.
pub(crate) fn strip_list_command(session: TmuxSessionId) -> String {
    format!(
        "list-windows -t ${} -F 'S #{{window_id}}|#{{window_index}}|#{{window_active}}|#{{window_zoomed_flag}}|#{{pane_synchronized}}|#{{pane-border-status}}|#{{prefix}}|#{{display-time}}|#{{message-style}}|#{{session_name}}|#{{window_name}}'",
        session
    )
}

pub(crate) fn parse_strip_windows(output: &str) -> Vec<StripWindow> {
    output
        .lines()
        .filter_map(|line| line.strip_prefix("S "))
        .filter_map(|line| {
            // The name is last: it may contain the separator.
            let mut f = line.splitn(11, '|');
            let id = f.next()?.strip_prefix('@')?.parse().ok()?;
            let index = f.next()?.parse().ok()?;
            let active = f.next()? == "1";
            let zoomed = f.next()? == "1";
            let synchronized = f.next()? == "1";
            let bottom_border = f.next()? == "bottom";
            let prefix = f.next()?.to_string();
            let display_time = f.next()?.parse().unwrap_or(750);
            let message_style = f.next()?.to_string();
            let session = f.next()?.to_string();
            let name = f.next().unwrap_or("").to_string();
            Some(StripWindow {
                id,
                index,
                name,
                active,
                zoomed,
                synchronized,
                bottom_border,
                prefix,
                display_time,
                message_style,
                session,
            })
        })
        .collect()
}

fn attrs(fg: ColorAttribute, bg: ColorAttribute) -> CellAttributes {
    let mut a = CellAttributes::default();
    a.set_foreground(fg);
    a.set_background(bg);
    a
}

/// The strip row: `──` then the tabs one after another, then `─` up to the
/// edge. Where a vertical pane separator reaches the row and no tab covers
/// it, the separator ends with `│` (no `┴` junction).
pub fn strip_line(
    windows: &[StripWindow],
    cols: usize,
    separators: &[usize],
) -> (Line, Vec<StripRegion>) {
    let line_attrs = attrs(ColorAttribute::PaletteIndex(250), ColorAttribute::Default);
    let mut line = Line::with_width(0, 0);
    let mut regions = vec![];
    line.append_line(Line::from_text("──", &line_attrs, 0, None), 0);
    for w in windows {
        let start = line.len();
        let (index_attrs, name_attrs) = if w.active {
            (
                attrs(ColorAttribute::PaletteIndex(7), ColorAttribute::PaletteIndex(124)),
                attrs(ColorAttribute::PaletteIndex(59), ColorAttribute::PaletteIndex(188)),
            )
        } else {
            (
                attrs(ColorAttribute::PaletteIndex(8), ColorAttribute::PaletteIndex(59)),
                attrs(ColorAttribute::PaletteIndex(58), ColorAttribute::PaletteIndex(188)),
            )
        };
        let name: String = w.name.chars().filter(|c| !c.is_control()).collect();
        let mut label = format!(" {} ", name);
        if w.zoomed {
            label.push_str("🔍 ");
        }
        if w.synchronized {
            label.push_str("⇔ ");
        }
        line.append_line(Line::from_text(" ", &line_attrs, 0, None), 0);
        line.append_line(Line::from_text(&format!(" {} ", w.index), &index_attrs, 0, None), 0);
        line.append_line(Line::from_text(&label, &name_attrs, 0, None), 0);
        regions.push((start, line.len(), w.id));
    }
    let used = line.len();
    if used < cols {
        line.append_line(Line::from_text(&"─".repeat(cols - used), &line_attrs, 0, None), 0);
    }
    line.resize(cols, 0);
    // The line runs on; the GUI ends the separators on it (see
    // paint_tmux_separators), so they neither cross the row nor break it.
    let _ = separators;
    (line, regions)
}

impl TmuxDomainState {
    pub(crate) fn set_strip_windows(&self, windows: Vec<StripWindow>) {
        let changed = {
            let mut list = self.strip_windows.lock();
            let changed = *list != windows;
            *list = windows;
            changed
        };
        if changed {
            self.invalidate_gui();
        }
        // A new window is shown once the strip lists it (same listing as its
        // panes): never a frame with the window but without its tab.
        let pending = *self.strip_pending_window.lock();
        if let Some(window) = pending {
            if self.strip_windows.lock().iter().any(|w| w.id == window) {
                self.strip_pending_window.lock().take();
                if *self.active_remote_window.lock() == Some(window) {
                    self.present_remote_window(window);
                }
            }
        }
    }

    /// True when the strip does not list `window` yet: the switch waits for
    /// the next listing.
    pub(crate) fn strip_defer_window(&self, window: TmuxWindowId) -> bool {
        let known = self.strip_windows.lock().iter().any(|w| w.id == window);
        if known || self.strip_windows.lock().is_empty() {
            return false;
        }
        *self.strip_pending_window.lock() = Some(window);
        self.refresh_remote_windows();
        true
    }

    /// The shown window changed: mark it active at once, in the same frame as
    /// the tab (the listing that follows confirms it).
    pub(crate) fn strip_window_shown(&self, window: TmuxWindowId) {
        let mut list = self.strip_windows.lock();
        for w in list.iter_mut() {
            w.active = w.id == window;
        }
    }

    /// Keys after the prefix run bindings that can change the strip without
    /// a notification (synchronize-panes, for example).
    pub(crate) fn strip_key_sent(&self, key: &str) {
        let prefix = self.strip_windows.lock().first().map(|w| w.prefix.clone());
        let mut last = self.strip_last_key.lock();
        if prefix.is_some() && last.as_deref() == prefix.as_deref() {
            self.refresh_window_strip();
        }
        *last = Some(key.to_string());
    }

    pub(crate) fn refresh_window_strip(&self) {
        if let Some(session) = *self.tmux_session.lock() {
            self.enqueue_background_snapshot(Box::new(ListStripWindows { session }));
        }
    }

    /// Asks the GUI to open the window overview (Exposé). The request rides
    /// on a user-var alert of the shown pane: GUI windows already route
    /// those, and this name is reserved for the mirror.
    pub(crate) fn request_window_overview(&self) {
        let window = *self.active_remote_window.lock();
        let pane = window
            .and_then(|w| self.active_remote_panes.lock().get(&w).copied())
            .and_then(|p| self.remote_panes.lock().get(&p).map(|p| p.lock().local_pane_id));
        if let Some(pane_id) = pane {
            Mux::get().notify(crate::MuxNotification::Alert {
                pane_id,
                alert: wezterm_term::Alert::SetUserVar {
                    name: OVERVIEW_USER_VAR.to_string(),
                    value: String::new(),
                },
            });
        }
    }

    /// A tmux status message (display-message from a binding): shown in the
    /// strip row for display-time, in message-style colours.
    pub(crate) fn show_client_message(&self, text: &str) {
        let (time_ms, style) = self
            .strip_windows
            .lock()
            .first()
            .map(|w| (w.display_time, w.message_style.clone()))
            .unwrap_or((750, String::new()));
        let until = std::time::Instant::now() + std::time::Duration::from_millis(time_ms.max(100));
        *self.client_message.lock() = Some((text.to_string(), style, until));
        self.invalidate_gui();
        let domain_id = self.domain_id;
        std::thread::spawn(move || {
            std::thread::sleep(until.saturating_duration_since(std::time::Instant::now()));
            promise::spawn::spawn_into_main_thread(async move {
                if let Some(domain) = Mux::get().get_domain(domain_id) {
                    if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                        tmux.inner.invalidate_gui();
                    }
                }
            })
            .detach();
        });
    }

    fn message_line(&self, cols: usize) -> Option<Line> {
        let (text, style, until) = self.client_message.lock().clone()?;
        if std::time::Instant::now() >= until {
            return None;
        }
        let colour = |key: &str| {
            style
                .split(',')
                .find_map(|item| item.trim().strip_prefix(key))
                .and_then(|v| v.trim_start_matches("colour").trim_start_matches("color").parse::<u8>().ok())
                .map(ColorAttribute::PaletteIndex)
        };
        let message_attrs = attrs(
            colour("fg=").unwrap_or(ColorAttribute::PaletteIndex(8)),
            colour("bg=").unwrap_or(ColorAttribute::PaletteIndex(7)),
        );
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        let mut line = Line::from_text(&text, &message_attrs, 0, None);
        line.resize(cols, 0);
        // The message uses the whole row, like tmux's message line.
        for cell in line.cells_mut().iter_mut().skip(text.chars().count()) {
            cell.attrs_mut().set_background(message_attrs.background());
        }
        Some(line)
    }

    pub(crate) fn invalidate_gui(&self) {
        if let Some(window) = self.gui_window.lock().as_ref().map(|w| **w) {
            Mux::get().notify(crate::MuxNotification::WindowInvalidated(window));
        }
    }

    /// Strip for the tmux window shown in `tab_id`, as (first column, line)
    /// segments around the `hidden` column spans (panes in a mode, whose
    /// status lives in tmux's border row). None when tmux's own bottom
    /// border row must stay visible (prompt, no room).
    pub(crate) fn window_strip(
        &self,
        tab_id: TabId,
        cols: usize,
        hidden: &[(usize, usize)],
    ) -> Option<Vec<(usize, Line)>> {
        if !config::configuration().tmux_continuous_window_tabs {
            return None;
        }
        let window = self
            .gui_tabs
            .lock()
            .iter()
            .find(|(_, t)| t.tab_id == tab_id)
            .map(|(id, _)| *id)?;
        if self.ui_prompt.lock().is_some() {
            return None;
        }
        if let Some(line) = self.message_line(cols) {
            return Some(vec![(0, line)]);
        }
        let mut windows = self.strip_windows.lock().clone();
        if !windows.iter().find(|w| w.id == window)?.bottom_border {
            return None;
        }
        // tmux already lists another window as current (a new window, a
        // switch in flight): keep the strip drawn with the shown tab until
        // the tab follows, so labels and panes change in the same frame.
        let switching = windows.iter().any(|w| w.active && w.id != window);
        {
            let last = self.strip_last.lock();
            if let Some((shown, since, key, segments, regions)) = last.as_ref() {
                if switching
                    && *shown == window
                    && key.0 == cols
                    && key.1 == hidden
                    && since.elapsed() < std::time::Duration::from_millis(500)
                {
                    self.window_tab_regions.lock().insert(window, regions.clone());
                    return Some(segments.clone());
                }
            }
        }
        // The highlighted tab is the one shown in this frame, never ahead of
        // or behind the panes drawn with it.
        for w in windows.iter_mut() {
            w.active = w.id == window;
        }
        let Some((segments, regions)) = strip_segments(&windows, cols, hidden) else {
            self.window_tab_regions.lock().remove(&window);
            return None;
        };
        self.window_tab_regions.lock().insert(window, regions.clone());
        if !switching {
            *self.strip_last.lock() = Some((
                window,
                std::time::Instant::now(),
                (cols, hidden.to_vec()),
                segments.clone(),
                regions,
            ));
        }
        Some(segments)
    }

    /// Local panes of `window` that are in a mode (copy mode): their status
    /// is in tmux's border row, which the strip leaves visible.
    fn mode_panes(&self, window: TmuxWindowId) -> Vec<crate::pane::PaneId> {
        self.remote_panes
            .lock()
            .values()
            .filter_map(|p| {
                let p = p.lock();
                (p.window_id == window && p.key_target.is_none() && p.mode_active).then_some(p.local_pane_id)
            })
            .collect()
    }
}

/// The strip around `hidden` column spans: the tabs go in the first free
/// part that holds them all (else the widest), the other free parts get the
/// plain line. Click regions are in row columns.
pub fn strip_segments(
    windows: &[StripWindow],
    cols: usize,
    hidden: &[(usize, usize)],
) -> Option<(Vec<(usize, Line)>, Vec<StripRegion>)> {
    let mut spans = hidden.to_vec();
    spans.sort();
    let mut free = vec![];
    let mut start = 0;
    for (left, right) in spans {
        let left = left.min(cols);
        if left > start {
            free.push((start, left));
        }
        start = start.max(right.min(cols));
    }
    if start < cols {
        free.push((start, cols));
    }
    // The first part that holds every tab, else the widest one.
    let needed = strip_line(windows, cols, &[]).1.last().map_or(3, |r| r.1 + 1);
    let main = free
        .iter()
        .copied()
        .find(|(l, r)| r - l >= needed)
        .or_else(|| free.iter().copied().max_by_key(|(l, r)| r - l))?;
    if main.1 - main.0 < 3 {
        return None;
    }
    let line_attrs = attrs(ColorAttribute::PaletteIndex(250), ColorAttribute::Default);
    let mut segments = vec![];
    let mut regions = vec![];
    for (l, r) in free {
        if (l, r) == main {
            let (line, tabs) = strip_line(windows, r - l, &[]);
            regions = tabs
                .into_iter()
                .map(|(a, b, id)| (a + l, (b + l).min(r), id))
                .filter(|(a, b, _)| a < b)
                .collect();
            segments.push((l, line));
        } else {
            let mut line = Line::from_text(&"─".repeat(r - l), &line_attrs, 0, None);
            line.resize(r - l, 0);
            segments.push((l, line));
        }
    }
    Some((segments, regions))
}

impl TmuxDomain {
    /// The GUI paints these (first column, line) segments over the strip
    /// row of `tab_id`, leaving the `hidden` column spans to tmux.
    pub fn window_strip(&self, tab_id: TabId, cols: usize, hidden: &[(usize, usize)]) -> Option<Vec<(usize, Line)>> {
        self.inner.window_strip(tab_id, cols, hidden)
    }

    /// Local panes of the window in `tab_id` that are in copy mode.
    pub fn mode_panes(&self, tab_id: TabId) -> Vec<crate::pane::PaneId> {
        let window = self
            .inner
            .gui_tabs
            .lock()
            .iter()
            .find(|(_, t)| t.tab_id == tab_id)
            .map(|(id, _)| *id);
        window.map(|w| self.inner.mode_panes(w)).unwrap_or_default()
    }

    /// Window list for previews and the window overview.
    pub fn strip_windows(&self) -> Vec<(StripWindow, Option<TabId>)> {
        let tabs = self.inner.gui_tabs.lock();
        self.inner
            .strip_windows
            .lock()
            .iter()
            .map(|w| (w.clone(), tabs.get(&w.id).map(|t| t.tab_id)))
            .collect()
    }

    /// Strip region (start, end) of each window, for hover previews.
    pub fn strip_regions(&self, tab_id: TabId) -> Vec<StripRegion> {
        let window = self
            .inner
            .gui_tabs
            .lock()
            .iter()
            .find(|(_, t)| t.tab_id == tab_id)
            .map(|(id, _)| *id);
        window
            .and_then(|w| self.inner.window_tab_regions.lock().get(&w).cloned())
            .unwrap_or_default()
    }

    /// Tiled panes of the window shown in `tab_id`: (local pane, left, top,
    /// width, height) in tmux cells; the border row is below the height.
    pub fn tiled_panes(&self, tab_id: TabId) -> Vec<(crate::pane::PaneId, usize, usize, usize, usize)> {
        let window = self
            .inner
            .gui_tabs
            .lock()
            .iter()
            .find(|(_, t)| t.tab_id == tab_id)
            .map(|(id, _)| *id);
        let Some(window) = window else {
            return vec![];
        };
        let tab = Mux::get().get_tab(tab_id);
        let is_floating = |id: crate::pane::PaneId| {
            tab.as_ref().map_or(false, |t| t.get_floating_pane_by_pane_id(id).is_some())
        };
        self.inner
            .remote_panes
            .lock()
            .values()
            .filter_map(|p| {
                let p = p.lock();
                (p.window_id == window && p.key_target.is_none() && !is_floating(p.local_pane_id))
                    .then_some((
                        p.local_pane_id,
                        p.pane_left as usize,
                        p.pane_top as usize,
                        p.pane_width as usize,
                        p.pane_height as usize,
                    ))
            })
            .collect()
    }

    /// Sets a pane's width or height (the GUI drags a separator locally and
    /// sends this a few times per second and on release).
    pub fn resize_pane_to(&self, local_pane_id: crate::pane::PaneId, width: Option<usize>, height: Option<usize>) {
        let remote = self
            .inner
            .remote_panes
            .lock()
            .values()
            .find(|p| p.lock().local_pane_id == local_pane_id)
            .map(|p| p.lock().pane_id);
        if let Some(pane_id) = remote {
            self.inner
                .enqueue_client_input(Box::new(ResizePaneTo { pane_id, width, height }));
        }
    }

    /// Recaptures the tiled panes of the window in `tab_id` (after a resize
    /// that the GUI first applied locally). Queued after the resize itself.
    pub fn recapture_tab(&self, tab_id: TabId) {
        let panes: Vec<TmuxPaneId> = {
            let ids: std::collections::HashSet<crate::pane::PaneId> =
                self.tiled_panes(tab_id).into_iter().map(|p| p.0).collect();
            self.inner
                .remote_panes
                .lock()
                .values()
                .filter_map(|p| {
                    let p = p.lock();
                    ids.contains(&p.local_pane_id).then_some(p.pane_id)
                })
                .collect()
        };
        for pane_id in panes {
            self.inner.enqueue_client_input(Box::new(crate::tmux_commands::CapturePane {
                pane_id,
                history_limit: 0,
            }));
        }
    }

    pub fn select_window(&self, window_id: TmuxWindowId) {
        self.inner
            .enqueue_client_input(Box::new(crate::tmux_commands::SelectWindow { window_id }));
    }
}

/// A plain tmux command whose result is not needed.
#[derive(Debug)]
pub(crate) struct TmuxRaw(pub String);

impl TmuxCommand for TmuxRaw {
    fn is_client_input(&self) -> bool {
        true
    }
    fn get_command(&self, _: DomainId) -> String {
        format!("{}\n", self.0)
    }
    fn process_result(&self, _: DomainId, _: &Guarded) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct ResizePaneTo {
    pane_id: TmuxPaneId,
    width: Option<usize>,
    height: Option<usize>,
}

impl TmuxCommand for ResizePaneTo {
    fn is_client_input(&self) -> bool {
        true
    }
    fn get_command(&self, _: DomainId) -> String {
        let mut cmd = format!("resize-pane -t %{}", self.pane_id);
        if let Some(w) = self.width {
            cmd.push_str(&format!(" -x {}", w.max(1)));
        }
        if let Some(h) = self.height {
            cmd.push_str(&format!(" -y {}", h.max(1)));
        }
        cmd.push('\n');
        cmd
    }
    fn process_result(&self, _: DomainId, _: &Guarded) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(id: u64, index: u64, name: &str, active: bool) -> StripWindow {
        StripWindow {
            id,
            index,
            name: name.into(),
            active,
            zoomed: false,
            synchronized: false,
            bottom_border: true,
            prefix: "C-Space".into(),
            display_time: 750,
            message_style: String::new(),
            session: "main".into(),
        }
    }

    #[test]
    fn tabs_follow_each_other_and_separators_end_with_bar() {
        let (line, regions) = strip_line(&[w(1, 1, "zsh", true), w(2, 2, "second", false)], 40, &[3, 30]);
        let want = format!("──  1  zsh   2  second {}", "─".repeat(17));
        assert_eq!(line.as_str(), want);
        assert_eq!(regions, vec![(2, 11, 1), (11, 23, 2)]);
        assert_eq!(line.len(), 40);
    }

    #[test]
    fn strip_goes_around_a_copy_mode_pane() {
        let windows = [w(1, 1, "zsh", true), w(2, 2, "second", false)];
        // Copy-mode pane on the right: the tabs stay on the left part.
        let (segments, regions) = strip_segments(&windows, 60, &[(41, 60)]).unwrap();
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].0, 0);
        assert_eq!(segments[0].1.len(), 41);
        assert_eq!(regions, vec![(2, 11, 1), (11, 23, 2)]);
        // Copy-mode pane on the left: the tabs move after it.
        let (segments, regions) = strip_segments(&windows, 60, &[(0, 20)]).unwrap();
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].0, 20);
        assert_eq!(regions[0], (22, 31, 1));
        // Copy-mode pane in the middle: tabs left, plain line right.
        let (segments, _) = strip_segments(&windows, 60, &[(25, 40)]).unwrap();
        assert_eq!(segments.iter().map(|(l, s)| (*l, s.len())).collect::<Vec<_>>(), vec![(0, 25), (40, 20)]);
        assert!(segments[1].1.as_str().chars().all(|c| c == '─'));
        // A narrow pane before the copy-mode pane: the tabs go after it.
        let (segments, regions) = strip_segments(&windows, 60, &[(10, 25)]).unwrap();
        assert_eq!(segments.iter().map(|(l, s)| (*l, s.len())).collect::<Vec<_>>(), vec![(0, 10), (25, 35)]);
        assert!(segments[0].1.as_str().chars().all(|c| c == '─'));
        assert_eq!(regions[0], (27, 36, 1));
        // A zoomed copy-mode pane covers the row: tmux's row stays.
        assert!(strip_segments(&windows, 60, &[(0, 60)]).is_none());
    }

    #[test]
    fn parses_window_list() {
        let list = parse_strip_windows("S @3|1|1|0|0|bottom|C-Space|750|fg=colour8,bg=colour7|dev|my name|x\nP other\nS @7|2|0|1|1|off|C-b|4000||ops|x\n");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "my name|x");
        assert!(list[1].zoomed && list[1].synchronized && !list[1].bottom_border);
    }
}
