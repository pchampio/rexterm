//! The agent panel, drawn above the bottom right of the window: the coding
//! agent sessions of a tab (Claude Code, Codex), each with its status, and
//! under each one the agent shells that it ran (its tool commands, each in
//! its own Rex terminal). A card shows the command, a live preview of its
//! terminal, its state (running, or finished with its exit status) and two
//! buttons: Open (the terminal in a popup) and Stop for a running command.
//! A finished command cannot be stopped: its button archives it, and an
//! archived one can be removed (its terminal closes).
//!
//! Keyboard (prefix l opens the panel): j/k move between agents, l enters
//! the commands of an agent (or opens its only command), l opens the
//! selected command, h archives it, x stops it (removes an archived one),
//! H shows or hides archived commands, L shows only running commands,
//! Escape goes back, then closes; q closes it from any level.
use super::tmux_windows::{contains, rect, text_line};
use crate::utilsprites::RenderMetrics;
use config::keyassignment::AgentPanel as Args;
use mux::pane::PaneId;
use mux::Mux;
use serde_json::json;
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use wezterm_term::color::ColorAttribute;
use window::color::LinearRgba;
use window::{KeyCode, Modifiers, MouseEvent, MouseEventKind as WMEK, MousePress, WindowOps};

type RectF = euclid::Rect<f32, window::PixelUnit>;

/// Preview rows of each card.
const PREVIEW_ROWS: usize = 8;
/// The most cards in the panel.
const MAX_CARDS: usize = 5;

/// Archived commands (block ids). They stay archived while the GUI runs.
fn archived() -> &'static Mutex<HashSet<String>> {
    static ARCHIVED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    ARCHIVED.get_or_init(|| Mutex::new(HashSet::new()))
}

fn is_archived(block: &str) -> bool {
    archived().lock().unwrap().contains(block)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Hit {
    Agent(usize),
    Card(usize),
    Open(usize),
    Stop(usize),
}

/// An agent and the commands of it that the panel shows (indexes into the
/// entries). `agent` is None for commands of no listed agent.
struct Group {
    agent: Option<usize>,
    entries: Vec<usize>,
}

pub struct AgentPanel {
    args: Args,
    /// The Rex pane that asked for the panel: it carries the server calls.
    owner: PaneId,
    hover: Option<Hit>,
    hits: Vec<(RectF, Hit)>,
    area: RectF,
    /// The selected agent group.
    group: usize,
    /// The selected command of the group, once the user entered it (l).
    command: Option<usize>,
    show_archived: bool,
    running_only: bool,
}

fn running(entry: &config::keyassignment::AgentPanelEntry) -> bool {
    entry.state.is_empty() || entry.state == "running"
}

impl AgentPanel {
    pub fn new(args: Args, owner: PaneId) -> Self {
        Self {
            args,
            owner,
            hover: None,
            hits: vec![],
            area: rect(0., 0., 0., 0.),
            group: 0,
            command: None,
            show_archived: false,
            running_only: false,
        }
    }

    fn visible(&self, index: usize) -> bool {
        let entry = &self.args.entries[index];
        (!self.running_only || running(entry)) && (self.show_archived || !is_archived(&entry.block_id))
    }

    fn groups(&self) -> Vec<Group> {
        let mut groups = vec![];
        let mut taken = vec![false; self.args.entries.len()];
        for (a, agent) in self.args.agents.iter().enumerate() {
            let mut entries = vec![];
            for (i, entry) in self.args.entries.iter().enumerate() {
                if !agent.block_id.is_empty() && entry.parent == agent.block_id {
                    taken[i] = true;
                    if self.visible(i) {
                        entries.push(i);
                    }
                }
            }
            groups.push(Group { agent: Some(a), entries });
        }
        let rest: Vec<usize> = (0..self.args.entries.len()).filter(|i| !taken[*i] && self.visible(*i)).collect();
        if !rest.is_empty() {
            groups.push(Group { agent: None, entries: rest });
        }
        groups
    }

    /// Keep the selection inside the lists after a change.
    fn clamp(&mut self) {
        let groups = self.groups();
        self.group = self.group.min(groups.len().saturating_sub(1));
        if let Some(c) = self.command {
            let count = groups.get(self.group).map_or(0, |g| g.entries.len());
            self.command = if count == 0 { None } else { Some(c.min(count - 1)) };
        }
    }

    /// The entry that h and x act on: the selected command, or the only
    /// command of the selected agent.
    fn target(&self) -> Option<usize> {
        let groups = self.groups();
        let group = groups.get(self.group)?;
        match self.command {
            Some(c) => group.entries.get(c).copied(),
            None if group.entries.len() == 1 => Some(group.entries[0]),
            None => None,
        }
    }
}

impl crate::TermWindow {
    pub fn show_agent_panel(&mut self, args: &Args, owner: PaneId) {
        // An empty list still opens: it shows the agents of the tab.
        self.agent_panel = Some(AgentPanel::new(args.clone(), owner));
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    fn agent_call(&self, method: &'static str, block: String) {
        log::debug!("agent panel: {method} {block}");
        let Some(pane) = Mux::get().get_pane(self.agent_panel.as_ref().map_or(0, |p| p.owner)) else {
            return;
        };
        promise::spawn::spawn_into_main_thread(async move {
            let Some(rex) = pane.downcast_ref::<mux::rex::RexPane>() else { return; };
            if let Err(error) = rex.call(method, json!({"block_id": block})).await {
                log::error!("agent panel {method}: {error:#}");
            }
        })
        .detach();
    }

    /// Open the terminal of an entry in a popup, and close the panel.
    fn agent_panel_open(&mut self, index: usize) {
        let Some(block) = self.agent_panel.as_ref().map(|p| p.args.entries[index].block_id.clone()) else { return; };
        self.agent_call("wezterm.agent_shell_show", block);
        self.agent_panel = None;
    }

    /// The second button of a card: Stop a running command, Archive a
    /// finished one, Remove an archived one (its terminal closes).
    fn agent_panel_second(&mut self, index: usize) {
        let Some(panel) = self.agent_panel.as_mut() else { return; };
        let entry = panel.args.entries[index].clone();
        if running(&entry) || is_archived(&entry.block_id) {
            panel.args.entries.remove(index);
            panel.hover = None;
            archived().lock().unwrap().remove(&entry.block_id);
            self.agent_call("block.close", entry.block_id);
        } else {
            archived().lock().unwrap().insert(entry.block_id);
        }
        if let Some(panel) = self.agent_panel.as_mut() {
            panel.clamp();
        }
    }

    /// Focus the pane of an agent, and close the panel.
    fn agent_panel_focus(&mut self, agent: usize) {
        let pane = self
            .agent_panel
            .as_ref()
            .and_then(|p| p.args.agents[agent].pane_id)
            .and_then(|id| Mux::get().get_pane(id as PaneId));
        self.agent_panel = None;
        if let Some(pane) = pane {
            let mux = Mux::get();
            if let Some((_, _, tab)) = mux.resolve_pane_id(pane.pane_id()) {
                if let Some(tab) = mux.get_tab(tab) {
                    tab.set_active_pane(&pane);
                }
            }
        }
    }

    /// Keys while the panel is open. Returns true when the panel used the key.
    pub fn agent_panel_key(&mut self, key: &KeyCode, mods: Modifiers, context: &dyn WindowOps) -> bool {
        let Some(panel) = self.agent_panel.as_mut() else {
            return false;
        };
        if mods.intersects(Modifiers::CTRL | Modifiers::ALT | Modifiers::SUPER) {
            return false;
        }
        let shift = mods.contains(Modifiers::SHIFT);
        let key = match key {
            KeyCode::Char(c) if shift && c.is_ascii_lowercase() => KeyCode::Char(c.to_ascii_uppercase()),
            other => other.clone(),
        };
        let groups = panel.groups();
        match key {
            KeyCode::Char('j') | KeyCode::DownArrow => match panel.command {
                Some(c) => {
                    let count = groups.get(panel.group).map_or(0, |g| g.entries.len());
                    panel.command = Some((c + 1).min(count.saturating_sub(1)));
                }
                None => panel.group = (panel.group + 1).min(groups.len().saturating_sub(1)),
            },
            KeyCode::Char('k') | KeyCode::UpArrow => match panel.command {
                Some(c) => panel.command = Some(c.saturating_sub(1)),
                None => panel.group = panel.group.saturating_sub(1),
            },
            KeyCode::Char('l') | KeyCode::RightArrow | KeyCode::Char('\r') => {
                let Some(group) = groups.get(panel.group) else { return true; };
                match (panel.command, group.entries.len()) {
                    (Some(c), _) => {
                        if let Some(index) = group.entries.get(c).copied() {
                            self.agent_panel_open(index);
                        }
                    }
                    (None, 1) => self.agent_panel_open(group.entries[0]),
                    (None, 0) => {
                        if let Some(agent) = group.agent {
                            self.agent_panel_focus(agent);
                        }
                    }
                    (None, _) => panel.command = Some(0),
                }
            }
            KeyCode::Char('h') => {
                if let Some(index) = panel.target() {
                    let block = panel.args.entries[index].block_id.clone();
                    let mut set = archived().lock().unwrap();
                    if !set.remove(&block) {
                        set.insert(block);
                    }
                    drop(set);
                    panel.clamp();
                }
            }
            KeyCode::Char('x') => {
                if let Some(index) = panel.target() {
                    let entry = &panel.args.entries[index];
                    // A finished command cannot be stopped; an archived one
                    // can be removed.
                    if running(entry) || is_archived(&entry.block_id) {
                        self.agent_panel_second(index);
                    }
                }
            }
            KeyCode::Char('H') => {
                panel.show_archived = !panel.show_archived;
                panel.clamp();
            }
            KeyCode::Char('L') => {
                panel.running_only = !panel.running_only;
                panel.clamp();
            }
            // q hides the panel from any level.
            KeyCode::Char('q') => self.agent_panel = None,
            KeyCode::Char('\u{1b}') | KeyCode::LeftArrow => {
                if panel.command.is_some() {
                    panel.command = None;
                } else {
                    self.agent_panel = None;
                }
            }
            _ => return false,
        }
        context.invalidate();
        true
    }

    /// Returns true when the panel used the event.
    pub fn agent_panel_mouse(&mut self, event: &MouseEvent, context: &dyn WindowOps) -> bool {
        let Some(panel) = self.agent_panel.as_mut() else {
            return false;
        };
        let (x, y) = (event.coords.x as f32, event.coords.y as f32);
        let inside = contains(&panel.area, x, y);
        // Buttons and agents first; elsewhere in the panel, the nearest card.
        let hit = panel
            .hits
            .iter()
            .find(|(r, h)| !matches!(h, Hit::Card(_)) && contains(r, x, y))
            .map(|(_, h)| *h)
            .or_else(|| {
                panel
                    .hits
                    .iter()
                    .filter(|(_, h)| matches!(h, Hit::Card(_)))
                    .filter(|_| inside)
                    .min_by(|(a, _), (b, _)| {
                        let distance = |r: &RectF| if y < r.min_y() { r.min_y() - y } else if y > r.max_y() { y - r.max_y() } else { 0. };
                        distance(a).total_cmp(&distance(b))
                    })
                    .map(|(_, h)| *h)
            });
        match event.kind {
            WMEK::Move => {
                if hit != panel.hover {
                    panel.hover = hit;
                    context.invalidate();
                }
                // The panel stays open until a click elsewhere or Escape.
                inside
            }
            WMEK::Press(MousePress::Left) => {
                log::debug!("agent panel: press at {x},{y} hit {hit:?} inside {inside}");
                let Some(hit) = hit else {
                    if !inside {
                        // A click on the agent button again closes the panel
                        // (it must not open it again at once).
                        let bar_top = self.dimensions.pixel_height as f32 - self.render_metrics.cell_size.height as f32 * 1.5;
                        let on_bar = y >= bar_top && x >= panel.area.min_x();
                        self.agent_panel = None;
                        context.invalidate();
                        return on_bar;
                    }
                    return true;
                };
                match hit {
                    Hit::Agent(index) => self.agent_panel_focus(index),
                    Hit::Card(index) | Hit::Open(index) => self.agent_panel_open(index),
                    Hit::Stop(index) => self.agent_panel_second(index),
                }
                context.invalidate();
                true
            }
            WMEK::Press(_) | WMEK::Release(_) | WMEK::VertWheel(_) | WMEK::HorzWheel(_) => inside,
        }
    }

    pub fn paint_agent_panel(&mut self) -> anyhow::Result<()> {
        let Some(panel) = self.agent_panel.as_ref() else {
            return Ok(());
        };
        let args = panel.args.clone();
        let hover = panel.hover;
        let groups = panel.groups();
        let (selected_group, selected_command) = (panel.group, panel.command);
        // The command that l, x and h act on now: red in the panel.
        let target = panel.target();
        let archived_count = args.entries.iter().filter(|e| is_archived(&e.block_id)).count();
        let (show_archived, running_only) = (panel.show_archived, panel.running_only);
        let running_count = args.entries.iter().filter(|e| running(e)).count();
        let finished_count = args.entries.len() - running_count;
        // Previews are live: paint again while the panel is open.
        self.update_next_frame_time(Some(Instant::now() + Duration::from_millis(150)));

        // The rows. Agent level: each agent and its newest commands. Inside
        // an agent (after l): that agent and its commands around the
        // selected one.
        enum Row {
            Agent(usize),
            Card(usize),
            Empty(String),
        }
        let mut plan = vec![];
        match selected_command.filter(|_| selected_group < groups.len()) {
            Some(selected) => {
                let group = &groups[selected_group];
                plan.push(Row::Agent(selected_group));
                let first = selected.saturating_sub(MAX_CARDS - 1).min(group.entries.len().saturating_sub(MAX_CARDS));
                if first > 0 {
                    plan.push(Row::Empty(format!("{first} more above")));
                }
                for index in group.entries.iter().skip(first).take(MAX_CARDS) {
                    plan.push(Row::Card(*index));
                }
                let below = group.entries.len().saturating_sub(first + MAX_CARDS);
                if below > 0 {
                    plan.push(Row::Empty(format!("{below} more below")));
                }
            }
            None => {
                let mut cards = 0;
                for (g, group) in groups.iter().enumerate() {
                    plan.push(Row::Agent(g));
                    if group.entries.is_empty() {
                        plan.push(Row::Empty(if running_only { "No running command".into() } else { "No command yet".into() }));
                    }
                    let mut shown = 0;
                    for index in &group.entries {
                        if cards < MAX_CARDS {
                            plan.push(Row::Card(*index));
                            cards += 1;
                            shown += 1;
                        }
                    }
                    if shown < group.entries.len() {
                        plan.push(Row::Empty(format!("{} more: l to see them", group.entries.len() - shown)));
                    }
                }
                if groups.is_empty() {
                    plan.push(Row::Empty("No agent in this tab yet.".to_owned()));
                }
            }
        }
        if !args.note.is_empty() {
            plan.push(Row::Empty(args.note.clone()));
        }

        let cw = self.render_metrics.cell_size.width as f32;
        let ch = self.render_metrics.cell_size.height as f32;
        let window_w = self.dimensions.pixel_width as f32;
        let window_h = self.dimensions.pixel_height as f32;
        let bar = if self.show_tab_bar { self.tab_bar_pixel_height()? } else { 0. };
        let palette = self.palette().clone();
        let bg = palette.resolve_bg(ColorAttribute::Default).to_linear();
        let fg = palette.resolve_fg(ColorAttribute::Default).to_linear();
        let mix = |a: LinearRgba, b: LinearRgba, t: f32| {
            LinearRgba::with_components(a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t, a.2 + (b.2 - a.2) * t, 1.)
        };
        let accent = palette.colors.0[4].to_linear();
        let danger = palette.colors.0[1].to_linear();
        let good = palette.colors.0[2].to_linear();
        let warn = palette.colors.0[3].to_linear();
        let muted = mix(bg, fg, 0.55);
        let mark = self.config.tab_bar_active_underline.map(|c| c.to_linear()).unwrap_or(danger);

        let font_size = self.config.font_size * self.fonts.get_font_scale() * 0.8;
        let small = self.fonts.preview_font(font_size.max(1.))?;
        let small_metrics = RenderMetrics::with_font_metrics(&small.metrics());
        let scw = small_metrics.cell_size.width as f32;
        let sch = small_metrics.cell_size.height as f32;

        let pad = (cw * 0.75).round();
        let width = (window_w * 0.5).clamp((cw * 60.).min(window_w - 2. * cw), (cw * 96.).min(window_w - 2. * cw));
        let header = (ch * 1.6).round();
        let title_h = (ch * 1.4).round();
        let agent_h = (ch * 1.8).round();
        let empty_h = (ch * 1.3).round();
        let help_h = 0.;
        let available = window_h - bar - ch * 2.;
        // A card: its state and buttons, then its command. Only the selected
        // card (the one that l opens) shows the live preview, under the
        // command, which stays in place while the preview scrolls.
        let cmd_h = (ch * 1.4).round();
        let card_h = |rows: usize, open: bool| pad + header + cmd_h + if open { rows as f32 * sch + pad } else { 0. } + pad;
        let content = |rows: usize| -> f32 {
            plan.iter()
                .map(|row| match row {
                    Row::Agent(_) => agent_h,
                    Row::Card(i) => card_h(rows, target == Some(*i)) + pad,
                    Row::Empty(_) => empty_h,
                })
                .sum()
        };
        let mut rows = PREVIEW_ROWS;
        while rows > 2 && title_h + content(rows) + help_h + pad > available {
            rows -= 1;
        }
        let height = title_h + content(rows) + help_h + pad;
        let left = (window_w - width - cw).max(0.);
        let top = (window_h - bar - height - ch * 0.4).max(0.);
        let area = rect(left, top, width, height);

        let layer = self.render_state.as_ref().unwrap().layer_for_zindex(5)?;
        let mut layers = layer.quad_allocator();
        let radius = (ch * 0.45).round();
        // Shadow, border (2 pixels, clearly darker than the page), body.
        self.fill_rounded(rect(left + 2., top + 4., width + 1., height + 1.), radius + 2., mix(bg, fg, 0.22), 0, &mut layers)?;
        self.fill_rounded(rect(left - 2., top - 2., width + 4., height + 4.), radius + 2., mix(bg, fg, 0.5), 0, &mut layers)?;
        self.fill_rounded(area, radius, mix(bg, fg, 0.04), 0, &mut layers)?;
        // Top line: what this tab has (agents, running, finished, archived),
        // then the filters as pills (with their key), red when on.
        let agents = args.agents.len();
        let mut heading = if agents == 1 { "1 agent".to_owned() } else { format!("{agents} agents") };
        heading += &format!(" · {running_count} running · {finished_count} finished");
        if archived_count > 0 {
            heading += &format!(" · {archived_count} archived");
        }
        let pill_h = (ch * 1.15).round();
        let pill_y = top + (title_h - pill_h) / 2.;
        let mut px = left + width - pad;
        let mut pills_left = px;
        for (label, on) in [("H archived", show_archived), ("L running", running_only)] {
            let w = (label.chars().count() as f32 + 1.6) * cw;
            px -= w;
            let r = rect(px, pill_y, w, pill_h);
            self.fill_rounded(r, pill_h / 2., if on { mix(bg, mark, 0.2) } else { mix(bg, fg, 0.07) }, 1, &mut layers)?;
            let colour = if on { mark } else { muted };
            let line = text_line(label, ColorAttribute::TrueColorWithDefaultFallback(colour.to_srgb()), ColorAttribute::Default, on);
            self.render_line_at(&line, px + cw * 0.8, pill_y + (pill_h - ch) / 2., label.chars().count(), None, self.render_metrics, &palette, &mut layers)?;
            px -= cw * 0.6;
            pills_left = px;
        }
        let room = ((pills_left - left - pad) / cw).floor().max(0.) as usize;
        let heading = termwiz_funcs::truncate_right(&heading, room);
        let line = text_line(&heading, ColorAttribute::TrueColorWithDefaultFallback(mix(bg, fg, 0.65).to_srgb()), ColorAttribute::Default, true);
        self.render_line_at(&line, left + pad, top + (title_h - ch) / 2., heading.chars().count(), None, self.render_metrics, &palette, &mut layers)?;

        let mut hits = vec![];
        let mut y = top + title_h;
        for row in &plan {
            match row {
                Row::Empty(text) => {
                    let line = text_line(text, ColorAttribute::TrueColorWithDefaultFallback(muted.to_srgb()), ColorAttribute::Default, false);
                    self.render_line_at(&line, left + pad * 2.5, y + (empty_h - ch) / 2., text.chars().count(), None, self.render_metrics, &palette, &mut layers)?;
                    y += empty_h;
                }
                Row::Agent(g) => {
                    let group = &groups[*g];
                    let r = rect(left + pad, y + 2., width - 2. * pad, agent_h - 4.);
                    let selected = *g == selected_group && selected_command.is_none();
                    let on = group.agent.is_some_and(|a| hover == Some(Hit::Agent(a)));
                    if selected || on {
                        self.fill_rounded(r, radius, mix(bg, fg, if selected { 0.11 } else { 0.07 }), 0, &mut layers)?;
                    }
                    if selected {
                        self.fill_rounded(rect(r.min_x(), r.min_y() + 2., 4., r.height() - 4.), 2., mark, 1, &mut layers)?;
                    }
                    let Some(index) = group.agent else {
                        let text = "Other commands";
                        let line = text_line(text, ColorAttribute::Default, ColorAttribute::Default, true);
                        self.render_line_at(&line, r.min_x() + pad, y + (agent_h - ch) / 2., text.len(), None, self.render_metrics, &palette, &mut layers)?;
                        y += agent_h;
                        continue;
                    };
                    let agent = &args.agents[index];
                    hits.push((r, Hit::Agent(index)));
                    let icon = agent.icon.clone().unwrap_or_else(|| "shell".to_owned());
                    let tile_h = (ch * 1.15).round();
                    let used = self.paint_tiles(&mut layers, &[icon.as_str()], r.min_x() + pad * 0.5, y + (agent_h - tile_h) / 2., tile_h * 1.5, tile_h)?;
                    let mut x = r.min_x() + pad * 0.5 + used + cw * 0.6;
                    let text_y = y + (agent_h - ch) / 2.;
                    let name = if agent.name.is_empty() { "agent".to_owned() } else { agent.name.clone() };
                    let line = text_line(&name, ColorAttribute::Default, ColorAttribute::Default, true);
                    self.render_line_at(&line, x, text_y, name.chars().count(), None, self.render_metrics, &palette, &mut layers)?;
                    x += (name.chars().count() as f32 + 1.) * cw;
                    // The status: the tab bar's indicator and a word.
                    let (kind, word, colour) = match agent.status.as_str() {
                        "working" => (Some("working"), "working", accent),
                        "needs_input" => (Some("question"), "waits for you", warn),
                        _ => (None, "idle", muted),
                    };
                    if let Some(kind) = kind {
                        self.paint_status(&mut layers, kind, x, y + (agent_h - ch) / 2., cw * 2., ch)?;
                        x += cw * 2.5;
                    }
                    let line = text_line(word, ColorAttribute::TrueColorWithDefaultFallback(colour.to_srgb()), ColorAttribute::Default, kind.is_some());
                    self.render_line_at(&line, x, text_y, word.chars().count(), None, self.render_metrics, &palette, &mut layers)?;
                    x += (word.chars().count() as f32 + 2.) * cw;
                    let detail = if agent.detail.is_empty() { agent.title.clone() } else { agent.detail.clone() };
                    let room = ((r.max_x() - pad - x) / cw).floor().max(0.) as usize;
                    let detail = termwiz_funcs::truncate_right(&detail, room);
                    let line = text_line(&detail, ColorAttribute::TrueColorWithDefaultFallback(mix(bg, fg, 0.6).to_srgb()), ColorAttribute::Default, false);
                    self.render_line_at(&line, x, text_y, detail.chars().count(), None, self.render_metrics, &palette, &mut layers)?;
                    y += agent_h;
                }
                Row::Card(index) => {
                    let index = *index;
                    let entry = &args.entries[index];
                    let finished = !running(entry);
                    let archived_entry = is_archived(&entry.block_id);
                    let selected = target == Some(index);
                    // Cards of an agent sit a little to the right of it.
                    let indent = if args.agents.is_empty() { 0. } else { pad * 1.5 };
                    let card = rect(left + pad + indent, y, width - 2. * pad - indent, card_h(rows, selected));
                    let hovered = matches!(hover, Some(Hit::Card(i) | Hit::Open(i) | Hit::Stop(i)) if i == index);
                    let (edge, edge_w) = if selected { (mark, 2.) } else { (mix(bg, fg, 0.2), 1.) };
                    self.fill_rounded(rect(card.min_x() - edge_w, card.min_y() - edge_w, card.width() + 2. * edge_w, card.height() + 2. * edge_w), radius + edge_w, edge, 0, &mut layers)?;
                    self.fill_rounded(card, radius, mix(bg, fg, if hovered || selected { 0.11 } else { 0.07 }), 0, &mut layers)?;
                    hits.push((card, Hit::Card(index)));

                    let text_y = y + pad + (header - ch) / 2.;
                    let mut x = card.min_x() + pad;
                    // State badge: running, or finished with its exit status.
                    let (badge, colour) = if !finished {
                        ("● running".to_owned(), accent)
                    } else if entry.state == "exit 0" {
                        ("✓ finished".to_owned(), good)
                    } else {
                        (format!("✗ finished, {}", entry.state), danger)
                    };
                    let badge_w = (badge.chars().count() as f32 + 1.4) * cw;
                    let badge_h = (ch * 1.15).round();
                    let badge_rect = rect(x, y + pad + (header - badge_h) / 2., badge_w, badge_h);
                    self.fill_rounded(badge_rect, badge_h / 2., mix(bg, colour, 0.16), 1, &mut layers)?;
                    let line = text_line(&badge, ColorAttribute::TrueColorWithDefaultFallback(colour.to_srgb()), ColorAttribute::Default, true);
                    self.render_line_at(&line, x + cw * 0.7, badge_rect.min_y() + (badge_h - ch) / 2., badge.chars().count(), None, self.render_metrics, &palette, &mut layers)?;
                    x += badge_w + cw;
                    if archived_entry {
                        let line = text_line("archived", ColorAttribute::TrueColorWithDefaultFallback(muted.to_srgb()), ColorAttribute::Default, false);
                        self.render_line_at(&line, x, text_y, 8, None, self.render_metrics, &palette, &mut layers)?;
                        x += 9. * cw;
                    }
                    // Commands of no listed agent show which agent ran them.
                    if args.agents.iter().all(|a| a.block_id != entry.parent) {
                        let agent = if entry.agent.is_empty() { "agent".to_owned() } else { entry.agent.clone() };
                        let line = text_line(&agent, ColorAttribute::Default, ColorAttribute::Default, true);
                        self.render_line_at(&line, x, text_y, agent.chars().count(), None, self.render_metrics, &palette, &mut layers)?;
                        x += (agent.chars().count() as f32 + 1.) * cw;
                    }

                    let (second, second_colour) = if !finished {
                        ("Stop", danger)
                    } else if archived_entry {
                        ("Remove", danger)
                    } else {
                        ("Archive", muted)
                    };
                    let button = |label: &str| (label.chars().count() as f32 + 2.) * cw;
                    let second_w = button(second);
                    let open_w = button("Open");
                    let button_h = (ch * 1.25).round();
                    let button_y = y + pad + (header - button_h) / 2.;
                    let stop = rect(card.max_x() - pad - second_w, button_y, second_w, button_h);
                    let open = rect(stop.min_x() - cw * 0.6 - open_w, button_y, open_w, button_h);
                    for (r, label, colour, hit) in [(open, "Open", accent, Hit::Open(index)), (stop, second, second_colour, Hit::Stop(index))] {
                        // Under the pointer: a darker tone of the same button.
                        let on = hover == Some(hit);
                        self.fill_rounded(r, button_h / 2., mix(bg, colour, if on { 0.32 } else { 0.18 }), 1, &mut layers)?;
                        let line = text_line(label, ColorAttribute::TrueColorWithDefaultFallback(colour.to_srgb()), ColorAttribute::Default, true);
                        self.render_line_at(&line, r.min_x() + cw, r.min_y() + (button_h - ch) / 2., label.len(), None, self.render_metrics, &palette, &mut layers)?;
                        hits.push((r, hit));
                    }
                    let _ = x;
                    // The command, as the agent ran it: what matters when
                    // looking around. Bold, on the page colour.
                    let cmd = rect(card.min_x() + pad, y + pad + header, card.width() - 2. * pad, cmd_h);
                    self.fill_rounded(cmd, radius * 0.6, bg, 1, &mut layers)?;
                    let room = ((cmd.width() - cw) / cw).floor().max(1.) as usize;
                    let text = format!("$ {}", entry.title);
                    let text = if text.chars().count() > room { format!("{}…", termwiz_funcs::truncate_right(&text, room.saturating_sub(1))) } else { text };
                    let line = text_line(&text, ColorAttribute::TrueColorWithDefaultFallback(accent.to_srgb()), ColorAttribute::Default, true);
                    self.render_line_at(&line, cmd.min_x() + cw * 0.5, cmd.min_y() + (cmd_h - ch) / 2., text.chars().count(), None, self.render_metrics, &palette, &mut layers)?;

                    // Live preview of the selected card: the last rows of the
                    // terminal that hold text, under the command.
                    let pane = if selected { entry.pane_id.and_then(|id| Mux::get().get_pane(id as PaneId)) } else { None };
                    let preview = rect(card.min_x() + pad, cmd.max_y() + 1., card.width() - 2. * pad, rows as f32 * sch + pad - 1.);
                    if selected {
                        self.fill_rounded(preview, radius * 0.6, bg, 1, &mut layers)?;
                    }
                    if let Some(pane) = pane {
                        let dims = pane.get_dimensions();
                        let (_, lines) = pane.get_lines(dims.physical_top..dims.physical_top + dims.viewport_rows as isize);
                        let mut last = lines.len();
                        while last > 0 && lines[last - 1].as_str().trim().is_empty() {
                            last -= 1;
                        }
                        let first = last.saturating_sub(rows);
                        let cols = ((preview.width() - pad) / scw).floor().max(1.) as usize;
                        let pane_palette = pane.palette();
                        for (row, line) in lines[first..last].iter().enumerate() {
                            let mut line = line.clone();
                            line.resize(cols, 0);
                            self.render_line_at(&line, preview.min_x() + pad / 2., preview.min_y() + pad / 2. + row as f32 * sch, cols, Some(&small), small_metrics, &pane_palette, &mut layers)?;
                        }
                    }
                    y += card_h(rows, selected) + pad;
                }
            }
        }
        if let Some(panel) = self.agent_panel.as_mut() {
            panel.hits = hits;
            panel.area = area;
        }
        Ok(())
    }
}
