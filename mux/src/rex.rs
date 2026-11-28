//! Native Rex domains: server-owned sessions, GUI-owned rendering and overlays.
use crate::{
    Mux, MuxNotification,
    domain::{Domain, DomainId, DomainState, SplitSource, alloc_domain_id},
    pane::*,
    renderable::*,
    tab::{RemotePaneGeometry, SplitDirection, SplitRequest, SplitSize, Tab, TabId},
    window::WindowId,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use parking_lot::{MappedMutexGuard, Mutex};
use portable_pty::CommandBuilder;
use rangeset::RangeSet;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    io::Write,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use termwiz::{
    surface::{CursorShape, CursorVisibility, Line, SequenceNo},
};
use wezterm_rex::{
    actor::{Actor, Event},
    transport::Client,
};
use wezterm_term::{
    Alert, KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind, TerminalSize,
    color::ColorPalette,
};

pub struct RexDomain {
    id: DomainId,
    config: config::RexDomain,
    state: Mutex<State>,
    applying: smol::lock::Mutex<()>,
    attachment: Mutex<Option<(PaneId, WindowId, TabId)>>,
}
#[derive(Default)]
struct State {
    client: Option<Client>,
    session: String,
    window: Option<WindowId>,
    tabs: HashMap<String, TabId>,
    panes: HashMap<String, Arc<RexPane>>,
    revision: Option<u64>,
    focused_pane: Option<PaneId>,
    synchronized_windows: HashSet<String>,
    generation: usize,
    view: Value,
    pending_resizes: HashMap<String, Vec<Value>>,
    /// Agent shells that the user opened: detached server blocks (the tool
    /// commands of a coding agent) shown in a popup of this GUI only. The
    /// value is the server window of the agent's own block.
    agent_popups: HashMap<String, String>,
    /// Popups that were opened and not laid out yet: they take the focus.
    agent_popups_new: HashSet<String>,
    /// The open agent popups of the other sessions, by session: they open
    /// again (with the focus) when you switch back to that session.
    agent_popups_parked: HashMap<String, HashMap<String, String>>,
    /// Agent shells with a live terminal for the agent panel (no layout).
    agent_watch: HashSet<String>,
    /// The command and exit status (None while it runs) of each agent shell,
    /// for the popup title.
    agent_info: HashMap<String, (String, Option<i64>)>,
    /// The server windows in tab order and the active one, as the GUI showed
    /// them after the last view: a closed active tab goes to its left neighbor.
    tab_order: Vec<String>,
    active_tab_window: Option<String>,
    /// The terminals of the sessions that you left last, by session (the
    /// most recent first in parked_order). Their data channels stay open, so
    /// a switch back shows them at once instead of opening each terminal again.
    parked_panes: HashMap<String, HashMap<String, Arc<RexPane>>>,
    parked_order: Vec<String>,
}
/// The port forwards of this GUI (endpoint|host:port), for the life of the
/// process: a second click on the same URL reuses its forward.
fn forwards() -> &'static Mutex<HashMap<String, wezterm_rex::forward::Forward>> {
    static FORWARDS: std::sync::OnceLock<Mutex<HashMap<String, wezterm_rex::forward::Forward>>> = std::sync::OnceLock::new();
    FORWARDS.get_or_init(|| Mutex::new(HashMap::new()))
}
/// How many left sessions keep their terminals.
const PARKED_SESSIONS: usize = 3;

/// The frame of an agent shell popup: the command, its state, and the keys.
fn agent_popup_frame(info: Option<&(String, Option<i64>)>) -> PopupFrame {
    let style = config::configuration().rex_popup_styles.get(AGENT_SHELL_POPUP).cloned();
    let border_style = style.map(|s| s.border_style).unwrap_or_default();
    // "#[" starts a style run in the title.
    let command = info.map(|(title, _)| title.replace("#[", "# [")).unwrap_or_default();
    // The state and the keys in the title; the command on its own line under
    // it, which stays when the terminal scrolls.
    let title = match info.and_then(|(_, code)| *code) {
        None => "#[align=centre,fg=colour0,bg=colour248] running  ·  Esc/q hides  ·  prefix x stops ".to_owned(),
        Some(0) => "#[align=centre,fg=colour0,bg=colour2] finished  ·  Esc/q hides ".to_owned(),
        Some(code) => format!("#[align=centre,fg=colour7,bg=colour1] finished, exit {code}  ·  Esc/q hides "),
    };
    let subtitle = if command.is_empty() { String::new() } else { format!("$ {command}") };
    PopupFrame { title, border_style, subtitle }
}

/// The user variables that pick a tab's program tile, by Rex block. A pane
/// made again for the same block (a session switch, a reattach) starts with
/// them, so its tab shows the right tile before the server answers.
const REMEMBERED_VARS: [&str; 2] = ["WEZTERM_REX_PROCESS", "WEZTERM_AT_PROMPT"];
fn remembered_vars() -> &'static Mutex<HashMap<String, HashMap<String, String>>> {
    static VARS: std::sync::OnceLock<Mutex<HashMap<String, HashMap<String, String>>>> = std::sync::OnceLock::new();
    VARS.get_or_init(|| Mutex::new(HashMap::new()))
}
/// A session view without a window: its last pane exited.
fn view_is_empty(view: &Value) -> bool {
    view["windows"].is_null() || view["windows"].as_array().is_some_and(|w| w.is_empty())
}
fn remember_var(endpoint: &str, block: &str, name: &str, value: &str) {
    if !REMEMBERED_VARS.contains(&name) { return; }
    let mut all = remembered_vars().lock();
    // A bound on the memory: drop it all now and then (it fills again).
    if all.len() > 4096 { all.clear(); }
    all.entry(format!("{endpoint}|{block}")).or_default().insert(name.to_owned(), value.to_owned());
}

/// The label of a detached block that bin/rex-agent-shell made:
/// `agent-shell|PARENT BLOCK|AGENT|COMMAND`.
const AGENT_SHELL_LABEL: &str = "agent-shell|";
const AGENT_SHELL_DONE: &str = "agent-shell=";
const AGENT_SHELL_POPUP: &str = "Agent shell";

// Keep the newest local split ratios while older server revisions arrive.
// Return None if a structural edit removed this split, or whether it is acked.
fn apply_pending_resize(node: &mut Value, resize: &Value) -> Option<bool> {
    fn contains(node: &Value, block: &Value) -> bool {
        node.get("block_id") == Some(block) || node.get("split").is_some_and(|s|
            contains(&s["before"], block) || contains(&s["after"], block))
    }
    let split = node.get_mut("split")?;
    if split["direction"] == resize["direction"]
        && contains(&split["before"], &resize["before_block_id"])
        && contains(&split["after"], &resize["after_block_id"]) {
        let acked = (split["ratio"].as_f64()? - resize["ratio"].as_f64()?).abs() < 0.0001;
        split["ratio"] = resize["ratio"].clone();
        return Some(acked);
    }
    if let Some(result) = apply_pending_resize(&mut split["before"], resize) { return Some(result); }
    apply_pending_resize(&mut split["after"], resize)
}

fn resize_failed(domain_id: DomainId, window_id: String, resize: Value, error: anyhow::Error) {
    log::error!("Rex layout resize: {error:#}");
    promise::spawn::spawn_into_main_thread(async move {
        let Some(domain) = Mux::get().get_domain(domain_id) else { return; };
        let Some(domain) = domain.downcast_ref::<RexDomain>() else { return; };
        {
            let mut state = domain.state.lock();
            if let Some(pending) = state.pending_resizes.get_mut(&window_id) {
                pending.retain(|r| r["before_block_id"] != resize["before_block_id"]
                    || r["after_block_id"] != resize["after_block_id"]
                    || r["direction"] != resize["direction"] || r["ratio"] != resize["ratio"]);
            }
            // Reconcile with the actual server layout even if its revision
            // did not change (e.g. the server rejected the resize).
            state.revision = None;
        }
        if let Err(error) = domain.refresh().await { log::debug!("Rex resize reconciliation: {error:#}"); }
    }).detach();
}
impl RexDomain {
    pub fn new(config: config::RexDomain) -> Self {
        Self {
            id: alloc_domain_id(),
            config,
            state: Mutex::new(State::default()),
            applying: smol::lock::Mutex::new(()),
            attachment: Mutex::new(None),
        }
    }
    pub(crate) fn bind_attachment(&self, pane: PaneId, window: WindowId, tab: TabId) {
        *self.attachment.lock()=Some((pane,window,tab));
    }
    fn shell(&self,command:Option<CommandBuilder>,cwd:Option<String>, size: Option<TerminalSize>)->Value {
        let mut layout=shell(command,cwd);
        let window = self.state.lock().window.or_else(|| self.attachment.lock().as_ref().map(|a| a.1));
        let size = size.or_else(|| window.and_then(|w| Mux::get().get_active_tab_for_window(w)).map(|t| t.get_size()))
            .unwrap_or_else(|| config::configuration().initial_size(96, None));
        layout["block"]["options"]["size"] = json!({"columns":size.cols.max(1),"rows":size.rows.max(1),
            "cell_width_px":(size.pixel_width/size.cols.max(1)).max(1),"cell_height_px":(size.pixel_height/size.rows.max(1)).max(1)});
        let palette: ColorPalette = config::configuration().resolved_palette.clone().into();
        layout["block"]["options"]["theme"] = json!({"foreground":palette.foreground.to_rgb_string(),
            "background":palette.background.to_rgb_string(),"cursor":palette.cursor_bg.to_rgb_string(),
            "palette":palette.colors.0.iter().enumerate().map(|(i,c)|format!("{i}={}",c.to_rgb_string())).collect::<Vec<_>>()});
        if self.config.remote_shell {
            if let Some(argv)=layout["block"]["options"]["command"].as_array_mut() {
                argv.insert(0,json!("PROMPT_PURE_SSH_CONNECTION=rex"));
                argv.insert(0,json!("/usr/bin/env"));
            }
        }
        layout
    }
    fn focus_local(&self, block:&str)->Result<()> {
        let pane=self.state.lock().panes.get(block).cloned().context("Rex focus pane")?;
        let mux=Mux::get();
        let (_,window,tab)=mux.resolve_pane_id(pane.id).context("Rex focus tab")?;
        if let Some(mut win)=mux.get_window_mut(window) {
            if let Some(index)=win.get_tab_idx_for_id(tab) {win.remember_and_set_active_tab_idx(index);}
        }
        if let Some(tab)=mux.get_tab(tab) {let pane:Arc<dyn Pane>=pane;tab.set_active_pane(&pane);}
        Ok(())
    }
    fn connection(&self) -> Result<(Client, String)> {
        let s = self.state.lock();
        Ok((
            s.client.clone().context("Rex domain is detached")?,
            s.session.clone(),
        ))
    }
    /// The agent shells of the session, from the server's block list. A
    /// popup whose command ended closes.
    async fn agent_shells(&self) -> Result<Value> {
        let (client, session) = self.connection()?;
        let reply = client.call("session.list_blocks", json!({"session_id":session})).await?;
        let mut shells = vec![];
        for block in reply["blocks"].as_array().into_iter().flatten() {
            let (Some(id), Some(label)) = (block["block_id"].as_str(), block["label"].as_str()) else { continue; };
            // Running: "agent-shell|PARENT|AGENT|TITLE". Finished (the
            // terminal stays a while): "agent-shell=CODE|PARENT|AGENT|TITLE".
            let (rest, code) = if let Some(rest) = label.strip_prefix(AGENT_SHELL_LABEL) {
                (rest, None)
            } else if let Some((code, rest)) = label.strip_prefix(AGENT_SHELL_DONE).and_then(|r| r.split_once('|')) {
                (rest, code.parse::<i64>().ok())
            } else {
                continue;
            };
            if block["placement"] != "detached" { continue; }
            let mut parts = rest.splitn(3, '|');
            let (parent, agent, title) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            let state = code.map_or("running".to_owned(), |code| format!("exit {code}"));
            {
                // An open popup shows the state in its title: update it when
                // the command ends.
                let mut st = self.state.lock();
                let info = (title.to_owned(), code);
                if st.agent_info.get(id) != Some(&info) {
                    st.agent_info.insert(id.to_owned(), info.clone());
                    if st.agent_popups.contains_key(id) {
                        if let Some(pane) = st.panes.get(id) {
                            *pane.popup.lock() = Some(agent_popup_frame(Some(&info)));
                            Mux::notify_from_any_thread(crate::MuxNotification::PaneOutput(pane.id));
                        }
                    }
                }
            }
            shells.push(json!({"block_id":id,"parent":parent,"agent":agent,"title":title,
                "state":state,"running":code.is_none(),
                "open":self.state.lock().agent_popups.contains_key(id)}));
        }
        let (ended, gone) = {
            let mut state = self.state.lock();
            let before = state.agent_popups.len();
            state.agent_popups.retain(|block, _| shells.iter().any(|s| s["block_id"] == block.as_str()));
            let gone: Vec<String> = state.agent_watch.iter()
                .filter(|block| !shells.iter().any(|s| s["block_id"] == block.as_str()))
                .cloned().collect();
            for block in &gone { state.agent_watch.remove(block); }
            (before != state.agent_popups.len(), gone)
        };
        // A watched shell whose command ended: its terminal goes away.
        for block in gone {
            let pane = {
                let mut state = self.state.lock();
                if state.agent_popups.contains_key(&block) { None } else { state.panes.remove(&block) }
            };
            if let Some(pane) = pane {
                pane.dead.store(true, Ordering::Relaxed);
                Mux::get().remove_pane(pane.id);
            }
        }
        if ended { self.relayout().await?; }
        Ok(json!(shells))
    }
    /// The agent shells, each with a live terminal (`pane_id`) for previews.
    async fn watch_agent_shells(&self) -> Result<Value> {
        let mut shells = self.agent_shells().await?;
        let (client, session) = self.connection()?;
        for shell in shells.as_array_mut().into_iter().flatten() {
            let Some(block) = shell["block_id"].as_str().map(str::to_owned) else { continue; };
            let existing = self.state.lock().panes.get(&block).cloned();
            let pane = match existing {
                Some(pane) => pane,
                None => {
                    let pane = RexPane::new(self.id, client.clone(), self.config.endpoint.clone(), session.clone(),
                        block.clone(), String::new(), false, false).await?;
                    pane.detached.store(true, Ordering::Relaxed);
                    let dynpane: Arc<dyn Pane> = pane.clone();
                    Mux::get().add_pane(&dynpane)?;
                    self.state.lock().panes.insert(block.clone(), pane.clone());
                    pane
                }
            };
            self.state.lock().agent_watch.insert(block);
            shell["pane_id"] = json!(pane.id);
        }
        Ok(shells)
    }
    /// Build the tabs again from the server view and the open agent popups.
    async fn relayout(&self) -> Result<()> {
        {
            let mut state = self.state.lock();
            state.revision = None;
            state.view = Value::Null;
        }
        self.refresh().await
    }
    /// Show an agent shell in a popup of the window that holds `window`.
    async fn show_agent_shell(&self, block: String, window: String) -> Result<()> {
        self.open_agent_popup(block, window).await?;
        self.relayout().await
    }
    /// Make the popup of an agent shell (the next layout shows it, with the
    /// focus).
    async fn open_agent_popup(&self, block: String, window: String) -> Result<()> {
        // A view applied meanwhile would drop the new pane: it is not a popup
        // yet when that view ends.
        let _applying = self.applying.lock().await;
        let (client, session) = self.connection()?;
        if !self.state.lock().panes.contains_key(&block) {
            let pane = RexPane::new(self.id, client.clone(), self.config.endpoint.clone(), session.clone(),
                block.clone(), window.clone(), false, false).await?;
            pane.detached.store(true, Ordering::Relaxed);
            let dynpane: Arc<dyn Pane> = pane.clone();
            Mux::get().add_pane(&dynpane)?;
            self.state.lock().panes.insert(block.clone(), pane);
        }
        if let Some(pane) = self.state.lock().panes.get(&block) {
            *pane.window.lock() = window.clone();
        }
        // Input (Ctrl-C, an answer to a question) needs the ownership. A
        // terminal opened for a preview did not take it yet.
        client.call("session.own_block", json!({"session_id":session,"block_ids":[block],"mode":"take"})).await?;
        {
            let mut state = self.state.lock();
            state.agent_popups.insert(block.clone(), window);
            state.agent_popups_new.insert(block);
        }
        Ok(())
    }
    /// Take the popup away. The command keeps running on the server.
    async fn hide_agent_shell(&self, block: &str) -> Result<()> {
        if self.state.lock().agent_popups.remove(block).is_none() { return Ok(()); }
        self.relayout().await
    }
    async fn refresh(&self) -> Result<()> {
        let (client, session) = self.connection()?;
        let reply = client
            .call("session.view", json!({"session_id":session}))
            .await?;
        let revision=reply["revision"].clone();
        match self.apply_view(reply).await {
            Ok(())=>Ok(()),
            Err(error)=>{
                let latest=client.call("session.view",json!({"session_id":session})).await?;
                if latest["revision"]!=revision {self.apply_view(latest).await}else{Err(error)}
            }
        }
    }
    async fn apply_view(&self, mut view: Value) -> Result<()> {
        let _applying = self.applying.lock().await;
        let started=std::time::Instant::now();
        log::debug!("apply Rex view revision={} active={}",view["revision"],view["active_window_id"]);
        let revision = view["revision"]
            .as_u64()
            .context("Rex view has no revision")?;
        {
            let s = self.state.lock();
            if s.revision.is_some_and(|r| r >= revision) {
                return Ok(());
            }
        }
        // Rex serializes the last closed window as null, not necessarily [].
        // The last pane exited: end the session, as tmux does, so no empty
        // session stays on the server (Rex keeps one), then leave.
        if view_is_empty(&view) {
            if let (Some(id),Ok((client,_)))=(view["session_id"].as_str(),self.connection()) {
                log::info!("Rex session {id} has no pane left: destroying it");
                if let Err(e)=client.call("session.destroy",json!({"session_id":id})).await {log::warn!("Rex destroy empty session: {e:#}")}
            }
            return self.detach();
        }
        {
            let mut state = self.state.lock();
            for win in view["windows"].as_array_mut().context("windows")? {
                let wid = win["window_id"].as_str().unwrap_or_default().to_owned();
                if let Some(pending) = state.pending_resizes.get_mut(&wid) {
                    pending.retain(|resize| {
                        win["layers"].as_array_mut().into_iter().flatten()
                            .filter(|layer| layer["kind"] == "tiled")
                            .find_map(|layer| apply_pending_resize(&mut layer["layout"], resize)) == Some(false)
                    });
                }
            }
            state.pending_resizes.retain(|_, pending| !pending.is_empty());
        }
        let (client, session) = self.connection()?;
        let mux = Mux::get();
        let window = self.state.lock().window.context("no GUI window")?;
        let previous_active=if self.config.independent {mux.get_window(window).and_then(|w|w.get_active_tab().map(|t|t.tab_id()))}else{None};
        // The tab order and the active tab before this view (the GUI may have
        // changed tabs since the last view).
        let (order_before, active_before) = {
            let mut state = self.state.lock();
            let by_tab: HashMap<TabId, String> = state.tabs.iter().map(|(w, t)| (*t, w.clone())).collect();
            if let Some(win) = mux.get_window(window) {
                let order: Vec<String> = win.iter_tabs().filter_map(|t| by_tab.get(&t.tab_id()).cloned()).collect();
                let active = win.get_active_tab().and_then(|t| by_tab.get(&t.tab_id()).cloned());
                if active.is_some() { state.tab_order = order; state.active_tab_window = active; }
            }
            (state.tab_order.clone(), state.active_tab_window.clone())
        };
        // The active tab closed: the tab on its left gets the focus (the
        // first tab when the first one closed), not the server's choice.
        let closed_fallback = active_before.as_ref().and_then(|active| {
            let present = |id: &String| view["windows"].as_array().into_iter().flatten().any(|w| w["window_id"] == id.as_str());
            if present(active) { return None; }
            // A block of the closed tab moved to the server's active tab (join
            // a pane into another tab): follow it.
            let blocks = |win: &Value| -> HashSet<String> {
                win["layers"].as_array().into_iter().flatten()
                    .flat_map(|l| l["blocks"].as_array().into_iter().flatten())
                    .filter_map(|b| b["block_id"].as_str().map(str::to_owned)).collect()
            };
            let old_view = self.state.lock().view.clone();
            let closed = old_view["windows"].as_array().into_iter().flatten()
                .find(|w| w["window_id"] == active.as_str()).map(|w| blocks(w)).unwrap_or_default();
            let server_active = view["windows"].as_array().into_iter().flatten()
                .find(|w| w["window_id"] == view["active_window_id"]).map(|w| blocks(w)).unwrap_or_default();
            if !closed.is_disjoint(&server_active) { return None; }
            let index = order_before.iter().position(|w| w == active)?;
            order_before[..index].iter().rev().chain(order_before[index + 1..].iter()).find(|w| present(w)).cloned()
        });
        // Open every new terminal before changing any visible tab. A block can
        // disappear while this snapshot is being fetched (e.g. the temporary
        // anchor used by break-pane). Failed preparation must leave the GUI's
        // previous layout and focus intact while refresh fetches a newer view.
        let mut missing = Vec::new();
        let mut preparing = HashSet::new();
        for win in view["windows"].as_array().context("windows")? {
            let wid=win["window_id"].as_str().context("window ID")?;
            for layer in win["layers"].as_array().context("layers")? {
                if layer["state"]=="minimized" {continue;}
                for block in layer["blocks"].as_array().into_iter().flatten() {
                    let block=block["block_id"].as_str().context("block ID")?;
                    if self.state.lock().panes.contains_key(block) || !preparing.insert(block.to_owned()) {continue;}
                    missing.push((block.to_owned(),wid.to_owned()));
                }
            }
        }
        // Ownership and data-channel setup are independent. Await ownership
        // before publishing/input, but do not add a network RTT before reads.
        let ownership=if !missing.is_empty() {
            Some(client.enqueue("session.own_block",json!({"session_id":session,"block_ids":missing.iter().map(|(b,_)|b).collect::<Vec<_>>(),"mode":"take"}))?)
        } else {None};
        let initial_attach=self.state.lock().revision.is_none();
        let active_window=view["active_window_id"].as_str().unwrap_or("");
        missing.sort_by_key(|(_,window)| window != active_window);
        let mut tasks=Vec::new();
        for (block,window) in missing {
            let wait_ready=initial_attach && window==active_window;
            let deferred=initial_attach && window!=active_window;
            let client=client.clone();let endpoint=self.config.endpoint.clone();let session=session.clone();let domain=self.id;
            tasks.push(promise::spawn::spawn(async move {
                let pane=RexPane::new(domain,client,endpoint,session,block.clone(),window,wait_ready,deferred).await?;
                Ok::<_,anyhow::Error>((block,pane))
            }));
        }
        let mut prepared=Vec::new();
        for task in tasks {prepared.push(task.await?);}
        if let Some(ownership)=ownership {ownership.await?;}
        for (block,pane) in prepared {
            let dynpane: Arc<dyn Pane> = pane.clone();mux.add_pane(&dynpane)?;
            self.state.lock().panes.insert(block,pane.clone());
            // Metadata must not add two network round trips before showing a
            // new tab. Events continue updating it after this initial fetch.
            let background=initial_attach && *pane.window.lock()!=active_window;
            promise::spawn::spawn(async move {
                if background {smol::Timer::after(std::time::Duration::from_millis(300)).await;}
                if pane.dead.load(Ordering::Relaxed) {return;}
                let (status,process)=smol::future::zip(pane.refresh_status(),pane.refresh_process()).await;
                if let Err(error)=status.and(process) {log::debug!("Rex initial metadata: {error:#}");}
            }).detach();
        }
        let mut live_tabs = HashSet::new();
        let mut live_panes = HashSet::new();
        // An open agent popup or a watched agent shell is not in the server
        // view: keep its pane.
        {
            let state = self.state.lock();
            live_panes.extend(state.agent_popups.keys().cloned());
            live_panes.extend(state.agent_watch.iter().cloned());
        }
        for win in view["windows"]
            .as_array()
            .context("Rex view has no windows")?
        {
            let wid = win["window_id"].as_str().context("window ID")?.to_owned();
            live_tabs.insert(wid.clone());
            let existing = self
                .state
                .lock()
                .tabs
                .get(&wid)
                .and_then(|id| mux.get_tab(*id));
            // A new tab does not change every existing tab's layout. Avoid
            // rebuilding unchanged pane trees and issuing resize/focus/paint
            // notifications for each one on every session revision.
            let unchanged=existing.is_some() && self.state.lock().view["windows"].as_array()
                .is_some_and(|windows|windows.iter().any(|old|old==win));
            if unchanged {
                for layer in win["layers"].as_array().into_iter().flatten() {
                    for block in layer["blocks"].as_array().into_iter().flatten() {
                        if let Some(id)=block["block_id"].as_str() {live_panes.insert(id.to_owned());}
                    }
                }
                continue;
            }
            let is_new = existing.is_none();
            let size = existing
                .as_ref()
                .map(|t| t.get_size())
                .or_else(|| {
                    mux.get_window(window)
                        .and_then(|w| w.get_active_tab().map(|t| t.get_size()))
                })
                .unwrap_or(TerminalSize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 0,
                    pixel_height: 0,
                    dpi: 96,
                });
            let tab = existing.unwrap_or_else(|| Arc::new(Tab::new(&size)));
            let mut geometry = vec![];
            for layer in win["layers"].as_array().context("layers")? {
                if let Some(blocks) = layer["blocks"].as_array() {
                    for b in blocks {
                        if let Some(id) = b["block_id"].as_str() {
                            live_panes.insert(id.to_owned());
                        }
                    }
                }
                if layer["state"] == "minimized" {
                    continue;
                }
                let floating = layer["kind"] != "tiled";
                let mut leaves = vec![];
                let bounds = if floating {
                    rect(&layer["bounds"], size.cols, size.rows)
                } else {
                    (0, 0, size.cols, size.rows)
                };
                layout(&layer["layout"], bounds, &mut leaves);
                for (block, left, top, cols, rows) in leaves {
                    live_panes.insert(block.clone());
                    let pane = self.state.lock().panes.get(&block).cloned().context("prepared Rex pane missing")?;
                    *pane.window.lock() = wid.clone();
                    *pane.popup.lock() = if floating {
                        let label = layer["blocks"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .find(|b| b["block_id"] == block)
                            .and_then(|b| b["label"].as_str())
                            .unwrap_or("");
                        log::debug!("Rex popup label={:?} styles={:?}",label,config::configuration().rex_popup_styles.keys().collect::<Vec<_>>());
                        config::configuration()
                            .rex_popup_styles
                            .get(label)
                            .map(|s| PopupFrame {
                                title: s.title.clone(),
                                border_style: s.border_style.clone(),
                                subtitle: String::new(),
                            })
                    } else {
                        None
                    };
                    geometry.push(RemotePaneGeometry {
                        pane,
                        left,
                        top,
                        cols,
                        rows,
                        floating,
                        active: win["focused_block_id"] == block,
                    });
                }
            }
            // Agent shells that the user opened in this window: local popups.
            let popups = self.state.lock().agent_popups.iter()
                .filter(|(_, parent)| **parent == wid).map(|(block, _)| block.clone()).collect::<Vec<_>>();
            for block in popups {
                let Some(pane) = self.state.lock().panes.get(&block).cloned() else { continue; };
                let (left, top, cols, rows) = rect(&json!([0.1, 0.1, 0.8, 0.8]), size.cols, size.rows);
                // The frame draws the command in the row above the terminal.
                let (top, rows) = (top + 1, rows.saturating_sub(1).max(1));
                let frame = agent_popup_frame(self.state.lock().agent_info.get(&block));
                *pane.popup.lock() = Some(frame);
                let focused = self.state.lock().agent_popups_new.remove(&block)
                    || tab.get_active_pane().is_some_and(|active| active.pane_id() == pane.id);
                if focused {
                    for other in &mut geometry { other.active = false; }
                }
                geometry.push(RemotePaneGeometry { pane, left, top, cols, rows, floating: true, active: focused });
            }
            if geometry.is_empty() {
                continue;
            }
            if is_new {
                tab.assign_pane(&geometry[0].pane);
                mux.add_tab_and_active_pane(&tab)?;
                mux.add_tab_to_window(&tab, window)?;
                self.state.lock().tabs.insert(wid.clone(), tab.tab_id());
            }
            let zoomed = win["zoomed_block_id"]
                .as_str()
                .and_then(|b| self.state.lock().panes.get(b).map(|p| p.id));
            if self.config.independent {
                if let Some(active)=tab.get_active_pane() {
                    if geometry.iter().any(|p|p.pane.pane_id()==active.pane_id()) {
                        for p in &mut geometry {p.active=p.pane.pane_id()==active.pane_id();}
                    }
                }
            }
            tab.sync_rex_layout(&geometry, zoomed.is_some())?;
            tab.set_title(win["label"].as_str().unwrap_or("Rex"));
        }
        let (removed_tabs, removed_panes) = {
            let mut s = self.state.lock();
            // An agent popup or preview opened while this view was applied
            // (after live_panes was made) keeps its pane.
            live_panes.extend(s.agent_popups.keys().cloned());
            live_panes.extend(s.agent_watch.iter().cloned());
            let tabs = s
                .tabs
                .iter()
                .filter(|(k, _)| !live_tabs.contains(*k))
                .map(|(_, v)| *v)
                .collect::<Vec<_>>();
            let panes = s
                .panes
                .iter()
                .filter(|(k, _)| !live_panes.contains(*k))
                .map(|(_, v)| v.clone())
                .collect::<Vec<_>>();
            s.tabs.retain(|k, _| live_tabs.contains(k));
            s.panes.retain(|k, _| live_panes.contains(k));
            s.revision = Some(revision);
            s.view = view.clone();
            (tabs, panes)
        };
        for p in removed_panes {
            p.dead.store(true, Ordering::Relaxed);
            mux.remove_pane(p.id);
        }
        for t in removed_tabs {
            mux.remove_tab(t);
        }
        // Reconcile server order without replacing panes, tabs, or their scrollback.
        if let Some(mut win) = mux.get_window_mut(window) {
            for (index, remote) in view["windows"].as_array().into_iter().flatten().enumerate() {
                let id = remote["window_id"]
                    .as_str()
                    .and_then(|id| self.state.lock().tabs.get(id).copied());
                if let Some(id) = id {
                    if let Some(old) = win.get_tab_idx_for_id(id) {
                        if old != index {
                            let tab = win.remove_tab_idx(old);
                            win.insert_tab_at_idx(index, &tab);
                        }
                    }
                }
            }
        }
        let fallback_tab = closed_fallback.as_ref().and_then(|id| self.state.lock().tabs.get(id).copied());
        if let (Some(id), Some(_)) = (&closed_fallback, fallback_tab) {
            if view["active_window_id"].as_str() != Some(id.as_str()) {
                // Tell the server, so that its next view keeps this tab.
                let block = view["windows"].as_array().into_iter().flatten()
                    .find(|w| w["window_id"] == id.as_str())
                    .and_then(|w| w["focused_block_id"].as_str()).map(str::to_owned);
                log::debug!("closed tab: focus the tab on its left {id} block {block:?}");
                if let Some(block) = block {
                    if let Err(error) = client.call("session.focus_block", json!({"session_id":session,"block_id":block})).await {
                        log::debug!("focus the tab on the left of a closed tab: {error:#}");
                    }
                }
            }
        }
        if let Some(active) = fallback_tab.or_else(|| previous_active.filter(|id|mux.get_tab(*id).is_some())).or_else(|| view["active_window_id"]
            .as_str()
            .and_then(|id| self.state.lock().tabs.get(id).copied()))
        {
            if let Some(mut win) = mux.get_window_mut(window) {
                let index = win.iter_tabs().position(|t| t.tab_id() == active);
                if let Some(index) = index {
                    win.remember_and_set_active_tab_idx(index);
                }
            }
        }
        self.state.lock().focused_pane = mux.get_window(window)
            .and_then(|w| w.get_active_tab().cloned())
            .and_then(|t| t.get_active_pane())
            .map(|p| p.pane_id());
        {
            let mut state = self.state.lock();
            let by_tab: HashMap<TabId, String> = state.tabs.iter().map(|(w, t)| (*t, w.clone())).collect();
            if let Some(win) = mux.get_window(window) {
                state.tab_order = win.iter_tabs().filter_map(|t| by_tab.get(&t.tab_id()).cloned()).collect();
                state.active_tab_window = win.get_active_tab().and_then(|t| by_tab.get(&t.tab_id()).cloned());
            }
        }
        log::debug!("committed Rex view {} focus {:?} preparation_ms={}",revision,self.state.lock().focused_pane,started.elapsed().as_millis());
        mux.notify(MuxNotification::WindowInvalidated(window));
        Ok(())
    }
    /// The sessions on the server, without empty ones. A session whose last
    /// pane exited while no client watched (or that an older client left)
    /// is destroyed here. Its own connection: calls on other sessions need
    /// session.attach_all, which would widen the domain connection's events.
    async fn live_sessions(&self) -> Result<Vec<Value>> {
        let (client, _events) = Client::connect(self.config.endpoint.clone()).await?;
        let listed = client.call("session.attach_all", json!({"event_scope":"session"})).await?;
        let sessions: Vec<Value> = listed["sessions"].as_array().cloned().unwrap_or_default();
        let views = sessions.iter()
            .map(|s| client.enqueue("session.view", json!({"session_id":s["session_id"]})))
            .collect::<Result<Vec<_>>>()?;
        let mut live = Vec::new();
        for (session, view) in sessions.into_iter().zip(views) {
            match view.await {
                Ok(view) if view_is_empty(&view) => {
                    log::info!("Rex session {} has no pane left: destroying it", session["session_id"]);
                    if let Err(e) = client.call("session.destroy", json!({"session_id":session["session_id"]})).await {
                        log::warn!("Rex destroy empty session: {e:#}");
                    }
                }
                // A session destroyed meanwhile is gone; keep the others.
                Ok(_) => live.push(session),
                Err(e) => log::debug!("Rex session view: {e:#}"),
            }
        }
        Ok(live)
    }
    async fn switch_session(&self, target: &str) -> Result<()> {
        let (client, previous) = self.connection()?;
        // The chooser already supplies a server ID. Do not repeat its list
        // request (and a whole SSH round trip) before attaching that session.
        let found = if target.strip_prefix("session:").is_some_and(|id|
            id.len() == 36 && id.bytes().enumerate().all(|(i, b)|
                if matches!(i, 8 | 13 | 18 | 23) { b == b'-' } else { b.is_ascii_hexdigit() })) {
            Some(target.to_owned())
        } else {
            // An empty session of that name ends here; a new one starts.
            let sessions = self.live_sessions().await?;
            sessions.iter()
                .find(|s| s["session_id"] == target || s["label"] == target)
                .and_then(|s| s["session_id"].as_str()).map(str::to_owned)
        };
        let session = match found {
            Some(s) => s,
            None => client
                .call(
                    "session.create",
                    json!({"label":target,"initial_windows":[{"layout":self.shell(None,self.config.initial_cwd.clone(),None)}]}),
                )
                .await?["session_id"]
                .as_str()
                .context("created session ID")?
                .to_owned(),
        };
        if session == previous {
            return Ok(());
        }
        let old_panes=self.state.lock().panes.values().cloned().collect::<Vec<_>>();
        for pane in &old_panes {pane.input.lock().suspended=true;}
        let attach = client.enqueue("session.attach", json!({"session_id":session}));
        // The kept terminals of the target session: claim them in the same
        // round trip as the attach (the requests keep their order).
        let kept: Vec<String> = self.state.lock().parked_panes.get(&session)
            .map(|panes| panes.keys().cloned().collect()).unwrap_or_default();
        let claim = match &attach {
            Ok(_) if !kept.is_empty() => client.enqueue("session.own_block",
                json!({"session_id":session,"block_ids":kept,"mode":"take"})).ok(),
            _ => None,
        };
        // Their data channels reconnect now: the requests go after the attach.
        let waking: Vec<Arc<RexPane>> = self.state.lock().parked_panes.get(&session)
            .map(|panes| panes.values().cloned().collect()).unwrap_or_default();
        if attach.is_ok() { for pane in &waking { pane.actor.park(false); } }
        let response = match async { attach?.await }.await {
            Ok(response)=>response,
            Err(error)=>{
                for pane in &old_panes {pane.input.lock().suspended=false;}
                for pane in &waking { pane.actor.park(true); }
                return Err(error);
            }
        };
        let (reopen, restored) = {
            let mut state = self.state.lock();
            // The open agent popups stay with their session: their panes go
            // with the session's other panes, and come back with it.
            let open = std::mem::take(&mut state.agent_popups);
            state.agent_popups_new.clear();
            // Panel previews of the old session go too (the panel asks again).
            let watched = std::mem::take(&mut state.agent_watch);
            // Keep the terminals of the old session (not its popups and
            // previews, which open again as before). Marked dead, they send
            // nothing (and no close) while the server sees another session.
            let leaving: HashMap<String, Arc<RexPane>> = state.panes.iter()
                .filter(|(block, _)| !open.contains_key(*block) && !watched.contains(*block))
                .map(|(block, pane)| (block.clone(), pane.clone())).collect();
            for (block, pane) in &leaving {
                pane.dead.store(true, Ordering::Relaxed);
                pane.actor.park(true);
                state.panes.remove(block);
            }
            state.parked_order.retain(|s| *s != previous && *s != session);
            if !leaving.is_empty() {
                state.parked_panes.insert(previous.clone(), leaving);
                state.parked_order.insert(0, previous.clone());
            }
            let keep = state.parked_order.len().min(PARKED_SESSIONS);
            for old in state.parked_order.split_off(keep) {
                state.parked_panes.remove(&old);
            }
            if !open.is_empty() { state.agent_popups_parked.insert(previous.clone(), open); }
            state.session = session.clone();
            state.revision = None;
            let restored = state.parked_panes.remove(&session).unwrap_or_default();
            for (block, pane) in &restored {
                pane.dead.store(false, Ordering::Relaxed);
                pane.input.lock().suspended = false;
                // It shows its last frame at once and reconnects now; input
                // goes over the control channel meanwhile.
                {let mut frame = pane.actor.frame.lock().unwrap(); frame.connected = true; frame.error = None;}
                pane.actor.park(false);
                state.panes.insert(block.clone(), pane.clone());
            }
            let reopen = state.agent_popups_parked.remove(&session).unwrap_or_default();
            log::debug!("Rex session switch {previous} -> {session}: parked {} popups, reopening {}, kept {} terminals, reused {}",
                state.agent_popups_parked.get(&previous).map_or(0, |p| p.len()), reopen.len(),
                state.parked_panes.get(&previous).map_or(0, |p| p.len()), restored.len());
            (reopen, restored.into_values().collect::<Vec<_>>())
        };
        let mux = Mux::get();
        for pane in &restored {
            let dynpane: Arc<dyn Pane> = pane.clone();
            mux.add_pane(&dynpane)?;
        }
        self.apply_view(response["view"].clone()).await?;
        // A kept terminal missed the events of its session while you were
        // away: ask for its state again (not before it shows).
        let claimed = match claim { Some(claim) => claim.await.is_ok(), None => true };
        for pane in restored {
            if pane.dead.load(Ordering::Relaxed) { continue; }
            let client = client.clone(); let session = session.clone();
            promise::spawn::spawn(async move {
                if !claimed {
                    if let Err(error) = client.call("session.own_block", json!({"session_id":session,"block_ids":[pane.block],"mode":"take"})).await {
                        log::debug!("Rex kept terminal: {error:#}");
                    }
                }
                let (status, process) = smol::future::zip(pane.refresh_status(), pane.refresh_process()).await;
                if let Err(error) = status.and(process) { log::debug!("Rex kept terminal metadata: {error:#}"); }
                let size = *pane.size.lock();
                if let Err(error) = pane.send_size(size) { log::debug!("Rex kept terminal size: {error:#}"); }
            }).detach();
        }
        if !reopen.is_empty() {
            for (block, window) in reopen {
                // The command may have ended and its terminal closed meanwhile.
                if let Err(error) = self.open_agent_popup(block, window).await {
                    log::debug!("Rex agent popup not opened again: {error:#}");
                }
            }
            self.relayout().await?;
        }
        // The new session shows already: the detach of the old one does not
        // hold the switch (it goes out now, so it stays before a later attach).
        let detach = client.enqueue("session.detach", json!({"session_id":previous}))?;
        promise::spawn::spawn(async move {
            if let Err(error) = detach.await { log::error!("Rex session detach: {error:#}"); }
        }).detach();
        Ok(())
    }
    pub fn move_floating_pane(&self, pane_id: PaneId, left: usize, top: usize) {
        let state=self.state.lock();
        let Some(pane)=state.panes.values().find(|p|p.id==pane_id) else{return};
        let window=pane.window.lock().clone();
        let Some(tab)=state.tabs.get(&window).and_then(|id|Mux::get().get_tab(*id)) else{return};
        let size=tab.get_size();
        let layer=state.view["windows"].as_array().into_iter().flatten().find(|w|w["window_id"]==window)
            .and_then(|w|w["layers"].as_array()).into_iter().flatten()
            .find(|l|l["kind"]!="tiled" && l["blocks"].as_array().is_some_and(|bs|bs.iter().any(|b|b["block_id"]==pane.block)));
        let Some(layer)=layer else{return};
        let mut bounds=layer["bounds"].clone();bounds[0]=json!(left as f64/size.cols.max(1) as f64);bounds[1]=json!(top as f64/size.rows.max(1) as f64);
        if let Some(client)=state.client.clone() {
            let payload=json!({"session_id":state.session,"layer_id":layer["layer_id"],"bounds":bounds});
            if let Ok(reply)=client.enqueue("session.resize_layer",payload) {promise::spawn::spawn(async move {if let Err(e)=reply.await {log::error!("Rex popup move: {e:#}")}}).detach();}
        }
    }
    pub fn persist_layout(&self, tab_id: TabId) {
        let mux = Mux::get();
        let Some(tab) = mux.get_tab(tab_id) else {
            return;
        };
        let panes = tab.iter_panes_ignoring_zoom();
        let mut state = self.state.lock();
        let Some(client) = state.client.clone() else {
            return;
        };
        let Some(window_id) = state
            .tabs
            .iter()
            .find_map(|(w, t)| (*t == tab_id).then_some(w.clone()))
        else {
            return;
        };
        let Some(win) = state.view["windows"]
            .as_array()
            .and_then(|ws| ws.iter().find(|w| w["window_id"] == window_id))
        else {
            return;
        };
        let session = state.session.clone();
        let mut sizes = HashMap::new();
        for p in panes {
            if let Some(rex) = p.pane.downcast_ref::<RexPane>() {
                sizes.insert(rex.block.clone(), (p.left, p.top, p.width, p.height));
            }
        }
        let mut calls = vec![];
        fn collect(
            node: &Value,
            sizes: &HashMap<String, (usize, usize, usize, usize)>,
            calls: &mut Vec<Value>,
        ) -> Option<(String, usize, usize, usize, usize)> {
            if let Some(b) = node["block_id"].as_str() {
                let &(x, y, w, h) = sizes.get(b)?;
                return Some((b.into(), x, y, w, h));
            }
            let s = &node["split"];
            let (a, ax, ay, aw, ah) = collect(&s["before"], sizes, calls)?;
            let (b, bx, by, bw, bh) = collect(&s["after"], sizes, calls)?;
            let horizontal = s["direction"] == "horizontal";
            let ratio = if horizontal {
                aw as f64 / (aw + bw).max(1) as f64
            } else {
                ah as f64 / (ah + bh).max(1) as f64
            };
            if (ratio - s["ratio"].as_f64().unwrap_or(0.5)).abs() > 0.0001 {
                calls.push(json!({"direction":if horizontal{"horizontal"}else{"vertical"},"before_block_id":a,"after_block_id":b,"ratio":ratio.clamp(0.001,0.999)}));
            }
            Some((
                a,
                ax.min(bx),
                ay.min(by),
                (ax + aw).max(bx + bw) - ax.min(bx),
                (ay + ah).max(by + bh) - ay.min(by),
            ))
        }
        if let Some(layers) = win["layers"].as_array() {
            for l in layers {
                if l["kind"] == "tiled" {
                    collect(&l["layout"], &sizes, &mut calls);
                }
            }
        }
        if !calls.is_empty() {
            let pending = state.pending_resizes.entry(window_id.clone()).or_default();
            for resize in &calls {
                pending.retain(|old| old["before_block_id"] != resize["before_block_id"]
                    || old["after_block_id"] != resize["after_block_id"] || old["direction"] != resize["direction"]);
                pending.push(resize.clone());
            }
        }
        drop(state);
        for mut payload in calls {
            payload["session_id"] = json!(session);
            let domain = self.id;
            let window_id = window_id.clone();
            match client.enqueue("session.resize_pane", payload.clone()) {
                Ok(reply) => { smol::spawn(async move {
                    if let Err(e) = reply.await { resize_failed(domain, window_id, payload, e); }
                }).detach(); }
                Err(error) => resize_failed(domain, window_id, payload, error),
            }
        }
    }
    async fn publish_created_pane(&self, client: Client, session: String, window: String, block: String) -> Result<Arc<RexPane>> {
        let pane=RexPane::new(self.id,client.clone(),self.config.endpoint.clone(),session.clone(),block.clone(),window,false,false).await?;
        *pane.input.lock().pending.lock()=Some(Vec::new());
        let claim=client.enqueue("session.own_block",json!({"session_id":session,"block_ids":[block],"mode":"take"}))?;
        let prepared=pane.clone();
        promise::spawn::spawn(async move {
            match claim.await {
                Ok(_)=>{
                    let input=prepared.input.lock();
                    let pending=input.pending.lock().take().unwrap_or_default();
                    for (method,payload) in pending {if let Err(error)=input.call(&method,payload){log::error!("Rex pending input: {error:#}");}}
                },
                Err(error)=>{
                    let input=prepared.input.lock();input.pending.lock().take();
                    let mut frame=input.frame.lock().unwrap();frame.connected=false;frame.error=Some(format!("ownership: {error:#}"));
                    Mux::notify_from_any_thread(MuxNotification::PaneOutput(prepared.id));
                }
            }
            let _=smol::future::zip(prepared.refresh_status(),prepared.refresh_process()).await;
        }).detach();
        let dynpane:Arc<dyn Pane>=pane.clone();Mux::get().add_pane(&dynpane)?;
        self.state.lock().panes.insert(block,pane.clone());
        Ok(pane)
    }
    async fn new_window(
        &self,
        size: TerminalSize,
        command: Option<CommandBuilder>,
        cwd: Option<String>,
    ) -> Result<Arc<Tab>> {
        // The create response contains the authoritative new IDs. Publish
        // immediately; the following view event reconciles the full layout.
        // Holding the layout lock prevents that event from creating duplicates.
        let _applying=self.applying.lock().await;
        let (client,session)=self.connection()?;
        let result=client.call("session.new_window",json!({"session_id":session,
            "layout":self.shell(command,cwd,Some(size)),"focus":!self.config.independent})).await?;
        let id=result["window_id"].as_str().context("new window ID")?.to_owned();
        let block=result["block_ids"][0].as_str().context("new block ID")?.to_owned();
        let pane=self.publish_created_pane(client,session,id.clone(),block).await?;
        pane.resize(size)?;
        let tab=Arc::new(Tab::new(&size));let dynpane:Arc<dyn Pane>=pane;
        tab.assign_pane(&dynpane);
        let mux=Mux::get();mux.add_tab_and_active_pane(&tab)?;
        let window={let mut state=self.state.lock();state.tabs.insert(id,tab.tab_id());
            state.revision=Some(result["revision"].as_u64().context("new window revision")?.saturating_sub(1));
            state.window.context("Rex GUI window")?};
        mux.add_tab_to_window(&tab,window)?;
        Ok(tab)
    }
}
fn shell(command: Option<CommandBuilder>, cwd: Option<String>) -> Value {
    let config=config::configuration();
    let mut argv=command.map(|cmd|cmd.get_argv().iter().map(|s|s.to_string_lossy().into_owned()).collect::<Vec<_>>())
        .unwrap_or_else(||config.rex_default_prog.clone());
    let mut options=json!({});
    if !argv.is_empty() {
        if !config.rex_environment.is_empty() {
            let mut env=vec!["/usr/bin/env".to_owned()];
            env.extend(config.rex_environment.iter().map(|(k,v)|format!("{k}={v}")));
            env.append(&mut argv);argv=env;
        }
        options["shell"]=json!("none");options["login"]=json!(false);options["command"]=json!(argv);
    }
    if let Some(cwd)=cwd {options["cwd"]=json!(cwd);}
    json!({"block":{"flavor":"com.superlogical.terminal.shell","options":options}})
}
fn rect(v: &Value, cols: usize, rows: usize) -> (usize, usize, usize, usize) {
    let n = |i| v[i].as_f64().unwrap_or(if i < 2 { 0.0 } else { 1.0 });
    (
        (n(0) * cols as f64).round() as usize,
        (n(1) * rows as f64).round() as usize,
        ((n(2) * cols as f64).round() as usize).max(1),
        ((n(3) * rows as f64).round() as usize).max(1),
    )
}
fn layout(
    node: &Value,
    (x, y, w, h): (usize, usize, usize, usize),
    out: &mut Vec<(String, usize, usize, usize, usize)>,
) {
    if let Some(b) = node["block_id"].as_str() {
        out.push((b.into(), x, y, w.max(1), h.max(1)));
        return;
    }
    let s = &node["split"];
    if s.is_null() {
        return;
    }
    let ratio = s["ratio"].as_f64().unwrap_or(0.5).clamp(0.01, 0.99);
    if s["direction"] == "horizontal" {
        let a = ((w.saturating_sub(1) as f64 * ratio).round() as usize)
            .clamp(1, w.saturating_sub(2).max(1));
        layout(&s["before"], (x, y, a, h), out);
        layout(&s["after"], (x + a + 1, y, w.saturating_sub(a + 1), h), out);
    } else {
        let a = ((h.saturating_sub(1) as f64 * ratio).round() as usize)
            .clamp(1, h.saturating_sub(2).max(1));
        layout(&s["before"], (x, y, w, a), out);
        layout(&s["after"], (x, y + a + 1, w, h.saturating_sub(a + 1)), out);
    }
}
#[async_trait(?Send)]
impl Domain for RexDomain {
    fn domain_id(&self) -> DomainId {
        self.id
    }
    fn domain_name(&self) -> &str {
        &self.config.name
    }
    fn detachable(&self) -> bool {
        true
    }
    fn state(&self) -> DomainState {
        if self.state.lock().client.is_some() {
            DomainState::Attached
        } else {
            DomainState::Detached
        }
    }
    async fn attach(&self, window: Option<WindowId>) -> Result<()> {
        if self.state() == DomainState::Attached {
            return Ok(());
        }
        let attachment=self.attachment.lock().as_ref().map(|a|a.0);
        let (client, events) = Client::connect(self.config.endpoint.clone()).await?;
        let list = client.call("session.list", json!({})).await?;
        let session = list["sessions"]
            .as_array()
            .and_then(|a| {
                a.iter().find(|v| {
                    v["session_id"] == self.config.session || v["label"] == self.config.session
                })
            })
            .and_then(|v| v["session_id"].as_str())
            .map(str::to_owned);
        let session=match session {Some(s)=>s,None=>client.call("session.create",json!({"label":self.config.session,"initial_windows":[{"layout":self.shell(None,self.config.initial_cwd.clone(),None)}]})).await?["session_id"].as_str().context("created session ID")?.to_owned()};
        let response = client
            .call_reply("session.attach", json!({"session_id":session}))
            .await?;
        let mut initial_view=response.payload["view"].clone();
        if initial_view["revision"].is_u64() && (initial_view["windows"].is_null()
            || initial_view["windows"].as_array().is_some_and(|w|w.is_empty())) {
            client.call("session.new_window",json!({"session_id":session,
                "layout":self.shell(None,self.config.initial_cwd.clone(),None),"focus":true})).await?;
            initial_view=client.call("session.view",json!({"session_id":session})).await?;
        }
        anyhow::ensure!(attachment.is_none() || self.attachment.lock().as_ref().map(|a|a.0)==attachment,"Rex attachment cancelled");
        let builder = if window.is_none() {
            Some(Mux::get().new_empty_window(None, None))
        } else {
            None
        };
        let window = window.unwrap_or_else(|| builder.as_ref().unwrap().window_id);
        let generation = {
            let mut s = self.state.lock();
            s.client = Some(client);
            s.session = session;
            s.window = Some(window);
            s.generation += 1;
            s.generation
        };
        self.apply_view(initial_view).await?;
        drop(builder);
        let id = self.id;
        let mut seq = response.seq;
        promise::spawn::spawn_into_main_thread(async move {
            while let Ok(event)=events.recv_async().await {
                let Some(domain)=Mux::get().get_domain(id) else{break};let Some(domain)=domain.downcast_ref::<RexDomain>()else{break};
                if domain.state.lock().generation!=generation{break}
                if event["type"]=="disconnected" {log::error!("Rex control disconnected: {}",event);for p in domain.state.lock().panes.values(){p.actor.frame.lock().unwrap().connected=false;}continue}
                if event["type"]=="reconnected" {
                    let result=async {
                        let (client,session)=domain.connection()?;
                        let reply=client.call_reply("session.attach",json!({"session_id":session})).await?;
                        seq=reply.seq;domain.state.lock().revision=None;
                        domain.apply_view(reply.payload["view"].clone()).await?;
                        let panes=domain.state.lock().panes.values().cloned().collect::<Vec<_>>();
                        // One ownership request for every pane, then each pane's
                        // metadata at the same time: no round trip per pane.
                        let blocks=panes.iter().map(|p|p.block.clone()).collect::<Vec<_>>();
                        if !blocks.is_empty() {
                            client.call("session.own_block",json!({"session_id":session,"block_ids":blocks,"mode":"take"})).await?;
                        }
                        let tasks=panes.into_iter().map(|p|promise::spawn::spawn(async move {
                            let (status,process)=smol::future::zip(p.refresh_status(),p.refresh_process()).await;
                            status?;process?;
                            p.send_size(*p.size.lock())?;
                            let mut frame=p.actor.frame.lock().unwrap();if frame.error.is_none(){frame.connected=true;}
                            Ok::<_,anyhow::Error>(())
                        })).collect::<Vec<_>>();
                        for task in tasks {
                            if let Err(e)=task.await {log::error!("Rex reattach pane: {e:#}")}
                        }
                        Ok::<_,anyhow::Error>(())
                    }.await;
                    if let Err(e)=result{log::error!("Rex reattach: {e:#}")}
                    continue
                }
                if event["payload"]["type"]=="client_perform_requested" {
                    let pane={let state=domain.state.lock();state.panes.values().find(|p|Some(p.id)==state.focused_pane).cloned().or_else(||state.panes.values().next().cloned())};
                    if let Some(pane)=pane {pane.publish_var("WEZTERM_REX_CLIENT_REQUEST",event["payload"]["payload"].to_string());}
                    continue;
                }
                if event["seq"].as_u64().unwrap_or(0)<=seq{continue}
                let envelope=&event["payload"];
                // An event too large to send (API revision 15 and later): fetch
                // what it would have changed, the layout and each pane's state.
                if envelope["type"]=="event_dropped" {
                    let ours=envelope["payload"]["session_id"].as_str().is_some_and(|id|id==domain.state.lock().session);
                    log::warn!("Rex dropped an event too large to send: {}",envelope["payload"]);
                    if ours {
                        if let Err(e)=domain.refresh().await{log::error!("Rex refresh after a dropped event: {e:#}")}
                        let panes=domain.state.lock().panes.values().cloned().collect::<Vec<_>>();
                        for pane in panes {
                            if let Err(e)=pane.refresh_status().await{log::debug!("Rex status after a dropped event: {e:#}")}
                        }
                    }
                    continue;
                }
                // kill-session here or `rex kill` elsewhere: leave like a detach.
                if envelope["type"]=="session_destroyed" && envelope["payload"]["session_id"].as_str().is_some_and(|id|id==domain.state.lock().session) {
                    if let Err(e)=domain.detach(){log::error!("Rex session destroyed: {e:#}")}
                    break;
                }
                if envelope["type"]=="session_event" && envelope["payload"]["session_id"].as_str().is_some_and(|id|id!=domain.state.lock().session){continue}
                let p=if envelope["type"]=="session_event"{&envelope["payload"]["event"]}else{envelope};
                if p["type"]=="block_event" {
                    let body=&p["payload"];
                    let pane=body["block_id"].as_str().and_then(|b|domain.state.lock().panes.get(b).cloned());
                    if let Some(pane)=pane {
                        match body["name"].as_str().unwrap_or("") {
                            "program_status_changed"|"program_status_removed"=>{if let Err(e)=pane.refresh_status().await{log::error!("Rex status: {e:#}")}},
                            "process_changed"=>{
                                let foreground=body["payload"]["foreground"].clone();
                                pane.publish_var("WEZTERM_REX_PROCESS",foreground.to_string());
                                pane.publish_var("WEZTERM_REX_INPUT_PROCESSES", "[]".into());
                                promise::spawn::spawn(async move {let _=pane.refresh_input_processes(foreground).await;}).detach();
                            },
                            "child_exited"=>pane.publish_var("WEZTERM_REX_EXIT",body["payload"].to_string()),
                            "bell"=>pane.notification(None,String::new()),
                            "desktop_notification"=>pane.notification(body["payload"]["title"].as_str().map(str::to_owned),body["payload"]["body"].as_str().unwrap_or("").into()),
                            _=>{}
                        }
                    }
                }
                if p["type"]=="session_view_changed" {if let Err(e)=domain.refresh().await{log::error!("Rex layout: {e:#}")}}
            }
        }).detach();
        Ok(())
    }
    fn detach(&self) -> Result<()> {
        if let Some((source,window,tab))=self.attachment.lock().take() {
            // Restore the shell before removing the last Rex tab so the window
            // never becomes empty during manual detach.
            let mux=Mux::get();
            let window_closed=mux.get_window(window).is_none();
            if let (Some(tab),Some(mut win))=(mux.get_tab(tab),mux.get_window_mut(window)) {
                if win.get_tab_idx_for_id(tab.tab_id()).is_none() {win.push_tab(&tab);}
                if let Some(index)=win.get_tab_idx_for_id(tab.tab_id()) {win.remember_and_set_active_tab_idx(index);}
            }
            crate::rex_bridge::release(source);
            // Closing the GUI removed its window before detaching domains.
            // Its hidden carrier tab must close too, otherwise an orphaned
            // local shell keeps the GUI process and SSH connection alive.
            if window_closed {mux.remove_tab(tab);}
        }
        let (tabs, panes) = {
            let mut s = self.state.lock();
            s.generation += 1;
            s.client = None;
            s.revision = None;
            s.parked_panes.clear();
            s.parked_order.clear();
            (std::mem::take(&mut s.tabs), std::mem::take(&mut s.panes))
        };
        for (_, p) in panes {
            p.dead.store(true, Ordering::Relaxed);
            Mux::get().remove_pane(p.id);
        }
        for (_, id) in tabs {
            Mux::get().remove_tab(id);
        }
        Ok(())
    }
    async fn spawn(
        &self,
        size: TerminalSize,
        command: Option<CommandBuilder>,
        cwd: Option<String>,
        window: WindowId,
    ) -> Result<Arc<Tab>> {
        let was_detached = self.state() == DomainState::Detached;
        self.attach(Some(window)).await?;
        if was_detached && command.is_none() && cwd.is_none() {
            let id={let state=self.state.lock();state.view["active_window_id"].as_str().and_then(|id|state.tabs.get(id)).copied()};
            if let Some(id) = id {
                if let Some(t) = Mux::get().get_tab(id) {
                    t.resize(size);
                    return Ok(t);
                }
            }
        }
        self.new_window(size, command, cwd).await
    }
    async fn spawn_pane(
        &self,
        _: TerminalSize,
        _: Option<CommandBuilder>,
        _: Option<String>,
    ) -> Result<Arc<dyn Pane>> {
        anyhow::bail!("Rex panes must be created in a server window")
    }
    async fn split_pane(
        &self,
        source: SplitSource,
        _tab: TabId,
        pane_id: PaneId,
        request: SplitRequest,
    ) -> Result<Arc<dyn Pane>> {
        let (client, session) = self.connection()?;
        let anchor = self
            .state
            .lock()
            .panes
            .values()
            .find(|p| p.id == pane_id)
            .cloned()
            .context("Rex split anchor")?;
        let (mut command, mut cwd) = match source {
            SplitSource::Spawn {
                command,
                command_dir,
            } => (command, command_dir),
            _ => anyhow::bail!("Rex move split requires session.move_block"),
        };
        // In a pane running ssh, OSC 7 names a directory on the remote host:
        // the new local shell starts in ssh's own directory (below) instead.
        // The GUI fills command_dir from OSC 7 too, so drop it as well.
        if command.is_none() && anchor.get_foreground_process_name(CachePolicy::AllowStale)
            .is_some_and(|name| std::path::Path::new(&name).file_name().is_some_and(|n| n == "ssh")) {
            cwd = None;
        } else if cwd.is_none() {
            cwd = anchor.get_current_working_dir(CachePolicy::AllowStale).map(|u| u.path().to_owned());
        }
        if cwd.is_none() {
            let process = anchor.call("com.superlogical.terminal.process", json!({})).await?;
            cwd = process["foreground"]["cwd"].as_str()
                .or_else(|| process["child"]["cwd"].as_str()).map(str::to_owned);
        }
        let ratio = match request.size {
            SplitSize::Percent(p) => p as f64 / 100.0,
            SplitSize::Cells(c) => {
                c as f64
                    / if request.direction == SplitDirection::Horizontal {
                        anchor.get_dimensions().cols
                    } else {
                        anchor.get_dimensions().viewport_rows
                    } as f64
            }
        };
        let dims = anchor.get_dimensions();
        let mut child_size = TerminalSize {cols:dims.cols,rows:dims.viewport_rows,pixel_width:dims.pixel_width,pixel_height:dims.pixel_height,dpi:dims.dpi};
        let split_ratio=(if request.target_is_second {1.0-ratio} else {ratio}).clamp(0.05,0.95);
        let available=if request.direction==SplitDirection::Horizontal {dims.cols} else {dims.viewport_rows};
        let before=((available.saturating_sub(1) as f64*split_ratio).round() as usize).clamp(1,available.saturating_sub(2).max(1));
        let child=if request.target_is_second {available.saturating_sub(before+1).max(1)} else {before};
        if request.direction==SplitDirection::Horizontal {child_size.cols=child;} else {child_size.rows=child;}
        child_size.pixel_width=child_size.cols*dims.pixel_width/dims.cols.max(1);
        child_size.pixel_height=child_size.rows*dims.pixel_height/dims.viewport_rows.max(1);
        let _applying=self.applying.lock().await;
        let reply=client.call("session.new_split",json!({"session_id":session,"anchor_id":anchor.block,"direction":if request.direction==SplitDirection::Horizontal{"horizontal"}else{"vertical"},"side":if request.target_is_second{"after"}else{"before"},"ratio":split_ratio,"layout":self.shell(command,cwd,Some(child_size)),"focus":true})).await?;
        let id=reply["block_ids"][0].as_str().context("split block ID")?.to_owned();
        let window=anchor.window.lock().clone();
        let pane=self.publish_created_pane(client,session,window,id).await?;
        let tab=Mux::get().get_tab(_tab).context("split tab")?;
        let index=tab.iter_panes_ignoring_zoom().iter().position(|p|p.pane.pane_id()==pane_id).context("split anchor")?;
        tab.set_zoomed(false);
        let active=tab.split_and_insert(index,request,pane.clone())?;tab.set_active_idx(active);
        self.state.lock().revision=Some(reply["revision"].as_u64().context("split revision")?.saturating_sub(1));
        Ok(pane)
    }
}

struct Input {
    pending: Mutex<Option<Vec<(String,Value)>>>,
    suspended: bool,
    client: Client,
    session: String,
    block: String,
    frame: Arc<std::sync::Mutex<wezterm_rex::actor::Frame>>,
    dead: Arc<AtomicBool>,
}
impl Input {
    fn call(&self, method: &str, payload: Value) -> Result<()> {
        // Resize/key-release notifications for the old tab can arrive while
        // session.attach has already switched the control stream.
        if self.suspended || (self.dead.load(Ordering::Relaxed) && method!="block.close") {return Ok(());}
        if let Some(pending)=self.pending.lock().as_mut() {
            anyhow::ensure!(pending.len()<4096,"Rex pending input queue full");
            pending.push((method.to_owned(),payload));return Ok(());
        }
        let dead=self.dead.clone();
        anyhow::ensure!(
            self.frame.lock().unwrap().connected,
            "Rex is disconnected; input was not sent"
        );
        let reply = self.client.enqueue(method, payload)?;
        let method = method.to_owned();
        smol::spawn(async move {
            if let Err(e) = reply.await {
                if dead.load(Ordering::Relaxed) {
                    // Detach/close can retire the pane before its pending resize
                    // reply arrives. Live connection failures still log below.
                    log::debug!("Rex {method}: pane retired before reply: {e:#}");
                    return;
                }
                if e.downcast_ref::<wezterm_rex::transport::RpcError>().is_some_and(|e|e.0["code"]=="not_found" && e.0["message"].as_str().is_some_and(|m|m.starts_with("block not found:"))) {
                    dead.store(true,Ordering::Relaxed);
                    log::debug!("Rex {method}: block closed before queued input arrived");
                } else {log::error!("Rex {method}: {e:#}")}

            }
        })
        .detach();
        Ok(())
    }
    fn event(&self, kind: &str, payload: Value) -> Result<()> {
        // A large paste goes as several paste events, in order on this
        // connection: one request over the server's message limit would be
        // refused, and the paste lost.
        if kind == "paste" {
            if let Some(text) = payload["text"].as_str() {
                for piece in wezterm_rex::transport::json_text_chunks(text, wezterm_rex::transport::INPUT_CHUNK_JSON_BYTES) {
                    self.call("block.event",json!({"session_id":self.session,"block_id":self.block,"event":{"type":kind,"payload":{"text":piece}}}))?;
                }
                return Ok(());
            }
        }
        self.call("block.event",json!({"session_id":self.session,"block_id":self.block,"event":{"type":kind,"payload":payload}}))
    }
}
impl Write for Input {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        let text = std::str::from_utf8(b)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        for piece in wezterm_rex::transport::json_text_chunks(text, wezterm_rex::transport::INPUT_CHUNK_JSON_BYTES) {
            self.call(
                "com.superlogical.terminal.write",
                json!({"session_id":self.session,"block_id":self.block,"args":{"data":piece}}),
            )
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub struct RexPane {
    id: PaneId,
    domain: DomainId,
    endpoint: String,
    block: String,
    window: Mutex<String>,
    actor: Actor,
    input: Mutex<Input>,
    size: Mutex<TerminalSize>,
    dead: Arc<AtomicBool>,
    buffer_key: String,
    vars: Arc<Mutex<HashMap<String, String>>>,
    popup: Mutex<Option<PopupFrame>>,
    terminal_config: Mutex<Option<Arc<dyn wezterm_term::TerminalConfiguration>>>,
    resize_send: Mutex<ResizeSend>,
    /// An agent shell: a server block that no server window holds.
    detached: AtomicBool,
}
/// A separator drag changes the size on every pointer step. The local
/// terminal follows each step. The server gets at most one size per
/// `RESIZE_SEND_GAP`: the shell redraws its prompt for each size it sees, and
/// a redraw that is made for an older size than the one the terminal has
/// leaves stale prompt parts on screen.
#[derive(Default)]
struct ResizeSend {
    last: Option<std::time::Instant>,
    pending: Option<TerminalSize>,
    timer: bool,
}
const RESIZE_SEND_GAP: std::time::Duration = std::time::Duration::from_millis(60);
impl RexPane {
    fn queue_size(&self, size: TerminalSize) -> Result<()> {
        let wait = {
            let mut state = self.resize_send.lock();
            let now = std::time::Instant::now();
            let elapsed = state.last.map(|last| now.duration_since(last));
            if !state.timer && elapsed.map_or(true, |e| e >= RESIZE_SEND_GAP) {
                state.last = Some(now);
                state.pending = None;
                None
            } else {
                state.pending = Some(size);
                if state.timer {
                    return Ok(());
                }
                state.timer = true;
                Some(RESIZE_SEND_GAP.saturating_sub(elapsed.unwrap_or_default()))
            }
        };
        let Some(wait) = wait else {
            return self.send_size(size);
        };
        let id = self.id;
        promise::spawn::spawn_into_main_thread(async move {
            smol::Timer::after(wait).await;
            let Some(pane) = Mux::get().get_pane(id) else { return; };
            let Some(pane) = pane.downcast_ref::<RexPane>() else { return; };
            let size = {
                let mut state = pane.resize_send.lock();
                state.timer = false;
                state.last = Some(std::time::Instant::now());
                state.pending.take()
            };
            if let Some(size) = size {
                if let Err(error) = pane.send_size(size) {
                    log::debug!("Rex delayed resize: {error:#}");
                }
            }
        })
        .detach();
        Ok(())
    }
    async fn new(
        domain: DomainId,
        client: Client,
        endpoint: String,
        session: String,
        block: String,
        window: String,
        wait_ready: bool,
        deferred: bool,
    ) -> Result<Arc<Self>> {
        let buffer_key = format!("{}:{}", endpoint, session);
        let id = alloc_pane_id();
        let mut known = remembered_vars().lock().get(&format!("{endpoint}|{block}")).cloned().unwrap_or_default();
        // The block ID lets the tab title code know a pane again.
        known.insert("WEZTERM_REX_BLOCK".to_owned(), block.clone());
        let vars = Arc::new(Mutex::new(known));
        let v = vars.clone();
        let (var_endpoint, var_block) = (endpoint.clone(), block.clone());
        let palette: ColorPalette = config::configuration().resolved_palette.clone().into();
        let theme=json!({"session_id":session,"block_id":block,"args":{
            "foreground":palette.foreground.to_rgb_string(),"background":palette.background.to_rgb_string(),"cursor":palette.cursor_bg.to_rgb_string(),
            "palette":palette.colors.0.iter().enumerate().map(|(i,c)|format!("{i}={}",c.to_rgb_string())).collect::<Vec<_>>()
        }});
        let theme_client=client.clone();
        let actor = Actor::start_with_config_deferred(
            endpoint.clone(),
            client.clone(),
            session.clone(),
            block.clone(),
            Arc::new(config::TermConfig::new()),
            move |event| {
                if matches!(&event,Event::Activated) {
                    if let Ok(reply)=theme_client.enqueue("com.superlogical.terminal.set_theme",theme.clone()) {
                        tokio::spawn(async move {if let Err(error)=reply.await {log::debug!("Rex initial theme: {error:#}");}});
                    }
                    return;
                }
                if let Event::Alert(alert)=event {
                    // Rex owns process-scoped OSC 7501 records, including
                    // removals on prompt/exit. A delayed output packet must
                    // not overwrite a newer control event with a local copy.
                    if matches!(&alert,Alert::SetUserVar{name,..} if name=="WEZTERM_PROGRAM_STATUS") {return;}
                    if let Alert::SetUserVar{name,value}=&alert {
                        v.lock().insert(name.clone(),value.clone());
                        remember_var(&var_endpoint,&var_block,name,value);
                    }
                    // Rex emits authoritative notification events as well.
                    if !matches!(alert,Alert::Bell|Alert::ToastNotification{..}) {
                        Mux::notify_from_any_thread(MuxNotification::Alert{pane_id:id,alert});
                    }
                }
                Mux::notify_from_any_thread(MuxNotification::PaneOutput(id));
            },
            wait_ready, deferred,
        )
        .await?;
        let dead=Arc::new(AtomicBool::new(false));
        let input = Input {
            pending: Mutex::new(None),
            suspended:false,
            dead:dead.clone(),
            client,
            session,
            block: block.clone(),
            frame: actor.frame.clone(),
        };
        Ok(Arc::new(Self {
            terminal_config: Mutex::new(None),
            id,
            domain,
            endpoint,
            block,
            window: Mutex::new(window),
            actor,
            input: Mutex::new(input),
            size: Mutex::new(TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
                dpi: 96,
            }),
            dead,
            buffer_key,
            vars,
            popup: Mutex::new(None),
            resize_send: Mutex::new(ResizeSend::default()),
            detached: AtomicBool::new(false),
        }))
    }
    fn publish_var(&self, name: &str, value: String) {
        remember_var(&self.endpoint, &self.block, name, &value);
        let changed = self.vars.lock().insert(name.into(), value.clone()).as_ref() != Some(&value);
        if changed {
            Mux::notify_from_any_thread(MuxNotification::Alert {
                pane_id: self.id,
                alert: Alert::SetUserVar {
                    name: name.into(),
                    value,
                },
            });
        }
    }
    async fn refresh_process(&self) -> Result<()> {
        let process=self.call("com.superlogical.terminal.process",json!({})).await?;
        let foreground=if process["foreground"].is_null(){&process["child"]}else{&process["foreground"]};
        self.publish_var("WEZTERM_REX_PROCESS",foreground.to_string());
        self.refresh_input_processes(foreground.clone()).await?;
        Ok(())
    }
    async fn refresh_input_processes(&self, foreground: Value) -> Result<()> {
        // Resolve shell substitutions when process metadata changes, never
        // block a navigation key on an SSH round trip.
        let names = if matches!(foreground["name"].as_str(), Some("zsh" | "bash" | "sh" | "dash" | "fish")) {
            let (client,session) = {let input=self.input.lock();(input.client.clone(),input.session.clone())};
            let reply=client.call("client.action",json!({"action":"wezterm_input_processes",
                "session_id":session,"block_id":self.block})).await?;
            if reply["result"]["pid"] != foreground["pid"] { return Ok(()); }
            reply["result"]["names"].clone()
        } else {json!([])};
        let current=self.vars.lock().get("WEZTERM_REX_PROCESS") == Some(&foreground.to_string());
        if current {
            self.publish_var("WEZTERM_REX_INPUT_PROCESSES",names.to_string());
        }
        Ok(())
    }
    async fn refresh_status(&self) -> Result<()> {
        let response = self
            .call("com.superlogical.terminal.program_status", json!({}))
            .await?;
        let records = response["records"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| {
                // The same shape as a local pane's records: app is the
                // inherited one, and msg and title are plain one-line labels
                // (the program's text can hold bidi overrides).
                let mut r = r.clone();
                if let Some(app) = r.get("effective_app").cloned() {
                    r["app"] = app;
                }
                for key in ["msg", "title"] {
                    if let Some(text) = r[key].as_str().map(wezterm_program_status::label) {
                        r[key] = Value::String(text);
                    }
                }
                (r["id"].as_str().unwrap_or("").to_owned(), r)
            })
            .collect::<serde_json::Map<_, _>>();
        self.publish_var("WEZTERM_PROGRAM_STATUS", serde_json::to_string(&records)?);
        Ok(())
    }
    fn notification(&self, title: Option<String>, body: String) {
        let next = self
            .vars
            .lock()
            .get("WEZTERM_REX_NOTIFICATION")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
            + 1;
        self.publish_var("WEZTERM_REX_NOTIFICATION", next.to_string());
        let alert = if body.is_empty() {
            Alert::Bell
        } else {
            Alert::ToastNotification {
                title,
                body,
                focus: true,
            }
        };
        Mux::notify_from_any_thread(MuxNotification::Alert {
            pane_id: self.id,
            alert,
        });
    }
    pub fn context(&self) -> Value {
        let i = self.input.lock();
        json!({"session_id":i.session,"block_id":self.block,"window_id":self.window.lock().clone(),"domain_id":self.domain,"agent_shell":self.detached.load(Ordering::Relaxed),"buffer_key":self.buffer_key,"client_id":i.client.id(),"remote_shell":Mux::get().get_domain(self.domain).and_then(|d|d.downcast_ref::<RexDomain>().map(|d|d.config.remote_shell)).unwrap_or(false)})
    }
    pub async fn call(&self, method: &str, mut payload: Value) -> Result<Value> {
        if method == "wezterm.synchronize" {
            let domain=Mux::get().get_domain(self.domain).context("Rex domain")?;
            let domain=domain.downcast_ref::<RexDomain>().context("Rex domain")?;
            let window=self.window.lock().clone();let mut state=domain.state.lock();
            let enabled=payload["enabled"].as_bool().unwrap_or(!state.synchronized_windows.contains(&window));
            if enabled {state.synchronized_windows.insert(window);}else{state.synchronized_windows.remove(&window);}
            return Ok(json!(enabled));
        }
        if method == "wezterm.agent_shells" || method == "wezterm.agent_shells_watch" || method == "wezterm.agent_shell_show" || method == "wezterm.agent_shell_hide" {
            let domain = Mux::get().get_domain(self.domain).context("Rex domain")?;
            let domain = domain.downcast_ref::<RexDomain>().context("Rex domain")?;
            return match method {
                "wezterm.agent_shells" => Box::pin(domain.agent_shells()).await,
                "wezterm.agent_shells_watch" => Box::pin(domain.watch_agent_shells()).await,
                "wezterm.agent_shell_show" => {
                    let block = payload["block_id"].as_str().context("block_id")?.to_owned();
                    let window = self.window.lock().clone();
                    Box::pin(domain.show_agent_shell(block, window)).await.map(|_| Value::Null)
                }
                _ => {
                    let block = payload["block_id"].as_str().unwrap_or(&self.block).to_owned();
                    Box::pin(domain.hide_agent_shell(&block)).await.map(|_| Value::Null)
                }
            };
        }
        if method == "wezterm.forward_open" {
            // A URL of the Rex host (http://localhost:8000/...): forward a
            // port of this machine to it (the same port when it is free) and
            // return the URL to open here.
            let mut url = url::Url::parse(payload["url"].as_str().context("url")?)?;
            let host = url.host_str().context("URL host")?.trim_matches(|c| c == '[' || c == ']').to_owned();
            let port = url.port_or_known_default().context("URL port")?;
            let target = if matches!(host.as_str(), "0.0.0.0" | "::") { "127.0.0.1".to_owned() } else { host.clone() };
            let session = self.input.lock().session.clone();
            let endpoint = self.endpoint.clone();
            let key = format!("{endpoint}|{target}:{port}");
            let existing = forwards().lock().get(&key).filter(|f| f.is_alive()).map(|f| f.local);
            let local = match existing {
                Some(local) => local,
                None => {
                    let (e, s, t) = (endpoint.clone(), session.clone(), target.clone());
                    let forward = smol::unblock(move || wezterm_rex::forward::open(e, s, t, port, port)).await?;
                    let local = forward.local;
                    forwards().lock().insert(key, forward);
                    local
                }
            };
            let local_host = if matches!(host.as_str(), "localhost" | "0.0.0.0" | "::") { "localhost" } else { "127.0.0.1" };
            url.set_host(Some(local_host))?;
            url.set_port(Some(local.port())).ok();
            return Ok(json!({"url": url.to_string(), "port": local.port(), "target": format!("{target}:{port}")}));
        }
        if method == "wezterm.forward_list" {
            // The session's forwards (tunnel blocks), whoever carries them;
            // `mine` marks the ones this GUI carries.
            let (client, session) = {
                let i = self.input.lock();
                (i.client.clone(), i.session.clone())
            };
            let blocks = client.call("session.list_blocks", json!({"session_id": session})).await?;
            let mine = forwards().lock().values().filter(|f| f.is_alive()).map(|f| f.block.clone()).collect::<HashSet<_>>();
            let mut list = Vec::new();
            for block in blocks["blocks"].as_array().into_iter().flatten() {
                if block["creator_name"] != "com.superlogical.tunnel" { continue; }
                let Some(id) = block["block_id"].as_str() else { continue };
                let info = client.call("com.superlogical.tunnel.info",
                    json!({"session_id": session, "block_id": id, "args": {}})).await.unwrap_or_default();
                let mut row = wezterm_rex::forward::describe(block, &info);
                row["mine"] = json!(mine.contains(id));
                list.push(row);
            }
            return Ok(json!({"forwards": list}));
        }
        if method == "wezterm.forward_new" {
            // {remote, spec}: a forward like ssh -L (remote false: listen
            // here, connect from the server) or ssh -R (remote true).
            let remote = payload["remote"].as_bool().unwrap_or(false);
            let spec = wezterm_rex::forward::Spec::parse(remote, payload["spec"].as_str().context("spec")?)?;
            let session = self.input.lock().session.clone();
            let endpoint = self.endpoint.clone();
            let e = endpoint.clone();
            let forward = smol::unblock(move || wezterm_rex::forward::start(e, session, spec)).await?;
            let row = json!({"block_id": forward.block, "listen": forward.listen, "connect": forward.target,
                "direction": if forward.remote { "remote" } else { "local" }});
            forwards().lock().insert(format!("{endpoint}|{}", forward.block), forward);
            return Ok(row);
        }
        if method == "wezterm.forward_close" {
            // A forward this GUI carries stops (and closes its block);
            // another one's block is closed, which stops its carrier.
            let block = payload["block_id"].as_str().context("block_id")?.to_owned();
            let carried = {
                let mut all = forwards().lock();
                let key = all.iter().find(|(_, f)| f.block == block).map(|(k, _)| k.clone());
                key.and_then(|k| all.remove(&k))
            };
            if let Some(forward) = carried {
                let alive = forward.is_alive();
                drop(forward);
                if alive { return Ok(json!({"closed": block})); }
            }
            let (client, session) = {
                let i = self.input.lock();
                (i.client.clone(), i.session.clone())
            };
            client.call("block.close", json!({"session_id": session, "block_id": block})).await?;
            return Ok(json!({"closed": block}));
        }
        if method == "wezterm.download" {
            // Files of the Rex host to this machine (/tmp unless dir is set).
            let (client, session) = {
                let i = self.input.lock();
                (i.client.clone(), i.session.clone())
            };
            let block = self.block.clone();
            let endpoint = self.endpoint.clone();
            let dir = std::path::PathBuf::from(payload["dir"].as_str().unwrap_or("/tmp"));
            let paths = payload["paths"].as_array().context("download paths")?.iter()
                .map(|v| v.as_str().map(str::to_owned).context("download path")).collect::<Result<Vec<_>>>()?;
            return smol::unblock(move || -> Result<Value> {
                tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
                    let mut written = Vec::new();
                    for path in paths {
                        let target = wezterm_rex::upload::download(&client, &endpoint, &session, &block, &path, &dir).await
                            .with_context(|| format!("download of {path}"))?;
                        written.push(target.to_string_lossy().into_owned());
                    }
                    Ok(json!({"paths": written}))
                })
            }).await;
        }
        if method == "wezterm.upload" {
            let (client, session) = {
                let i = self.input.lock();
                (i.client.clone(), i.session.clone())
            };
            let block = self.block.clone();
            let endpoint = self.endpoint.clone();
            let paths = payload["paths"]
                .as_array()
                .context("upload paths")?
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(std::path::PathBuf::from)
                        .context("upload path")
                })
                .collect::<Result<Vec<_>>>()?;
            return smol::unblock(move || -> Result<Value> {
                tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
                    let batch=wezterm_rex::upload::new_uuid()?;let count=paths.len();let mut transfers=tokio::task::JoinSet::new();
                    let files=paths.into_iter().map(wezterm_rex::upload::File::path).map(|f|f.size().map(|size|(f,size))).collect::<Result<Vec<_>>>()?;
                    for (index,(file,size)) in files.into_iter().enumerate() {
                        let reference=format!("upload:{}",wezterm_rex::upload::new_uuid()?);
                        let ticket=client.call("com.superlogical.terminal.upload_begin",json!({"session_id":session,"block_id":block,"args":{"ref":reference,"name":file.name,"size":size,"batch":batch,"index":index,"count":count}})).await?;
                        let ticket=ticket["ticket"].as_str().context("upload ticket")?.to_owned();let endpoint=endpoint.clone();
                        transfers.spawn(async move {wezterm_rex::upload::send(&endpoint,&ticket,&file,size).await});
                    }
                    while let Some(result)=transfers.join_next().await {result??;}
                    Ok(Value::Null)
                })
            }).await;
        }
        if method == "wezterm.switch_session" {
            let domain = Mux::get().get_domain(self.domain).context("Rex domain")?;
            Box::pin(
                domain
                    .downcast_ref::<RexDomain>()
                    .context("Rex domain")?
                    .switch_session(payload["session"].as_str().context("session")?),
            )
            .await?;
            return Ok(Value::Null);
        }
        if method == "wezterm.sessions" {
            let domain = Mux::get().get_domain(self.domain).context("Rex domain")?;
            let sessions = Box::pin(domain.downcast_ref::<RexDomain>().context("Rex domain")?.live_sessions()).await?;
            return Ok(json!({"sessions": sessions}));
        }
        if method == "wezterm.buffer_set" || method == "wezterm.buffer_get" {
            use sha2::{Digest, Sha256};
            let key = Sha256::digest(self.buffer_key.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            let directory = config::DATA_DIR.join("rex-buffers");
            let path = directory.join(key);
            let text = payload["text"].as_str().unwrap_or("").to_owned();
            let write = method == "wezterm.buffer_set";
            return smol::unblock(move || -> Result<Value> {
                if !write {
                    return match std::fs::read_to_string(path) {
                        Ok(s) => Ok(json!(s)),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Null),
                        Err(e) => Err(e.into()),
                    };
                }
                anyhow::ensure!(
                    text.len() <= 16 * 1024 * 1024,
                    "Rex copy buffer exceeds 16 MiB"
                );
                let mut builder = std::fs::DirBuilder::new();
                builder.recursive(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                builder.create(&directory)?;
                static NEXT_BUFFER_WRITE: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let temp = path.with_extension(format!(
                    "{}-{}-tmp",
                    std::process::id(),
                    NEXT_BUFFER_WRITE.fetch_add(1, Ordering::Relaxed)
                ));
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create(true).truncate(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
                }
                let mut file = options.open(&temp)?;
                file.write_all(text.as_bytes())?;
                file.sync_all()?;
                std::fs::rename(temp, path)?;
                Ok(Value::Null)
            })
            .await;
        }
        if method == "wezterm.overview" {
            Mux::get().notify(MuxNotification::Alert {
                pane_id: self.id,
                alert: Alert::SetUserVar {
                    name: crate::tmux_strip::OVERVIEW_USER_VAR.into(),
                    value: String::new(),
                },
            });
            return Ok(Value::Null);
        }
        let (client, session) = {
            let i = self.input.lock();
            (i.client.clone(), i.session.clone())
        };
        if payload.is_null() {
            payload = json!({});
        }
        if method.starts_with("session.")
            || method.starts_with("block.")
            || method.starts_with("com.superlogical.")
        {
            payload["session_id"] = json!(session);
        }
        if method.starts_with("com.superlogical.") {
            if payload.get("block_id").is_none() { payload["block_id"] = json!(self.block); }
            if payload.get("args").is_none() {
                payload["args"] = json!({});
            }
        }
        let domain=Mux::get().get_domain(self.domain).context("Rex domain")?;
        let rex=domain.downcast_ref::<RexDomain>().context("Rex domain")?;
        if rex.config.independent && method=="session.focus_block" {
            rex.focus_local(payload["block_id"].as_str().context("block ID")?)?;
            return Ok(json!({}));
        }
        let local_focus=rex.config.independent && matches!(method,"session.new_window"|"session.new_split"|"session.new_layer") && payload["focus"]==true;
        if local_focus {payload["focus"]=json!(false);}
        let reply=client.call(method,payload).await?;
        if local_focus {
            rex.refresh().await?;
            if let Some(block)=reply["block_ids"][0].as_str() {rex.focus_local(block)?;}
        }
        Ok(reply)
    }
    fn send_size(&self, size: TerminalSize) -> Result<()> {
        let input=self.input.lock();
        input.call("com.superlogical.terminal.resize",json!({"session_id":input.session,"block_id":input.block,"args":{"columns":size.cols,"rows":size.rows,"cell_width_px":(size.pixel_width/size.cols.max(1)).max(1),"cell_height_px":(size.pixel_height/size.rows.max(1)).max(1)}}))
    }
    fn send_input(&self, event: &str, payload: Value) -> Result<()> {
        let mut peers=Vec::new();
        if let Some(domain)=Mux::get().get_domain(self.domain) {
            if let Some(domain)=domain.downcast_ref::<RexDomain>() {
                let window=self.window.lock().clone();let state=domain.state.lock();
                if state.synchronized_windows.contains(&window) {
                    let tiled=state.view["windows"].as_array().into_iter().flatten()
                        .find(|w|w["window_id"]==window).map(|w|&w["layers"][0]["blocks"]);
                    if let Some(blocks)=tiled.and_then(Value::as_array) {
                        if blocks.iter().any(|b|b["block_id"]==self.block) {
                            for b in blocks {if let Some(p)=b["block_id"].as_str().and_then(|id|state.panes.get(id)) {
                                if p.id!=self.id {peers.push(p.clone());}
                            }}
                        }
                    }
                }
            }
        }
        self.input.lock().event(event,payload.clone())?;
        for pane in peers {pane.input.lock().event(event,payload.clone())?;}
        Ok(())
    }
    fn key(&self, key: KeyCode, mods: KeyModifiers, release: bool) -> Result<()> {
        // An agent shell popup only shows the command: it takes no input.
        // Escape or q hides it (the command goes on); prefix x stops it.
        if self.detached.load(Ordering::Relaxed) {
            // q does what Escape does, as in a pager.
            if !release && (matches!(key, KeyCode::Escape | KeyCode::Char('\u{1b}')) || (key == KeyCode::Char('q') && mods.is_empty())) {
                let (domain, block) = (self.domain, self.block.clone());
                promise::spawn::spawn_into_main_thread(async move {
                    let Some(domain) = Mux::get().get_domain(domain) else { return; };
                    let Some(domain) = domain.downcast_ref::<RexDomain>() else { return; };
                    if let Err(error) = domain.hide_agent_shell(&block).await {
                        log::error!("hide agent shell: {error:#}");
                    }
                })
                .detach();
            }
            return Ok(());
        }
        let (name, text) = key_name(key);
        let text = if release { String::new() } else { text };
        let unshifted = text
            .chars()
            .next()
            .map(|c| match c {
                '!' => '1',
                '@' => '2',
                '#' => '3',
                '$' => '4',
                '%' => '5',
                '^' => '6',
                '&' => '7',
                '*' => '8',
                '(' => '9',
                ')' => '0',
                '_' => '-',
                '+' => '=',
                '{' => '[',
                '}' => ']',
                '|' => '\\',
                ':' => ';',
                '"' => '\'',
                '<' => ',',
                '>' => '.',
                '?' => '/',
                '~' => '`',
                _ => c.to_ascii_lowercase(),
            } as u32)
            .unwrap_or(0);
        self.send_input("key",json!({"key":name,"mods":modifiers(mods),"text":text,"consumed_mods":if mods.contains(KeyModifiers::SHIFT) && !text.is_empty(){"shift"}else{""},"unshifted_codepoint":unshifted,"composing":false,"action":if release{"release"}else{"press"}}))
    }
}
fn modifiers(m: KeyModifiers) -> String {
    [
        (KeyModifiers::CTRL, "ctrl"),
        (KeyModifiers::SHIFT, "shift"),
        (KeyModifiers::ALT, "alt"),
        (KeyModifiers::SUPER, "super"),
    ]
    .iter()
    .filter_map(|(flag, s)| m.contains(*flag).then_some(*s))
    .collect::<Vec<_>>()
    .join("+")
}
fn key_name(key: KeyCode) -> (String, String) {
    let name = match key {
        KeyCode::Char(c) => {
            let lower = c.to_ascii_lowercase();
            let n = match lower {
                'a'..='z' => format!("key_{lower}"),
                '0'..='9' => format!("digit_{lower}"),
                '!' | '@' | '#' | '$' | '%' | '^' | '&' | '*' | '(' | ')' => format!(
                    "digit_{}",
                    match lower {
                        '!' => '1',
                        '@' => '2',
                        '#' => '3',
                        '$' => '4',
                        '%' => '5',
                        '^' => '6',
                        '&' => '7',
                        '*' => '8',
                        '(' => '9',
                        _ => '0',
                    }
                ),
                ' ' => "space".into(),
                '-' | '_' => "minus".into(),
                '=' | '+' => "equal".into(),
                '[' | '{' => "bracket_left".into(),
                ']' | '}' => "bracket_right".into(),
                '\\' | '|' => "backslash".into(),
                ';' | ':' => "semicolon".into(),
                '\'' | '"' => "quote".into(),
                ',' | '<' => "comma".into(),
                '.' | '>' => "period".into(),
                '/' | '?' => "slash".into(),
                '`' | '~' => "backquote".into(),
                _ => "unidentified".into(),
            };
            return (n, c.to_string());
        }
        KeyCode::Enter => "enter",
        KeyCode::Escape => "escape",
        KeyCode::Backspace => "backspace",
        KeyCode::Tab => "tab",
        KeyCode::Delete => "delete",
        KeyCode::Insert => "insert",
        KeyCode::LeftArrow => "arrow_left",
        KeyCode::RightArrow => "arrow_right",
        KeyCode::UpArrow => "arrow_up",
        KeyCode::DownArrow => "arrow_down",
        KeyCode::Home => "home",
        KeyCode::End => "end",
        KeyCode::PageUp => "page_up",
        KeyCode::PageDown => "page_down",
        KeyCode::Function(n) => return (format!("f{n}"), String::new()),
        _ => "unidentified",
    };
    (name.into(), String::new())
}
#[async_trait(?Send)]
impl Pane for RexPane {
    fn pane_id(&self) -> PaneId {
        self.id
    }
    fn popup_frame(&self) -> Option<PopupFrame> {
        self.popup.lock().clone()
    }
    fn domain_id(&self) -> DomainId {
        self.domain
    }
    fn get_current_seqno(&self) -> SequenceNo {
        self.actor.terminal.lock().unwrap().current_seqno()
    }
    fn get_cursor_position(&self) -> StableCursorPosition {
        terminal_get_cursor_position(&mut self.actor.terminal.lock().unwrap())
    }
    fn get_changed_since(&self, range: Range<isize>, seq: SequenceNo) -> RangeSet<isize> {
        terminal_get_dirty_lines(&mut self.actor.terminal.lock().unwrap(), range, seq)
    }
    fn get_lines(&self, range: Range<isize>) -> (isize, Vec<Line>) {
        terminal_get_lines(&mut self.actor.terminal.lock().unwrap(), range)
    }
    fn with_lines_mut(&self, range: Range<isize>, with: &mut dyn WithPaneLines) {
        self.actor.activate();
        terminal_with_lines_mut(&mut self.actor.terminal.lock().unwrap(), range, with)
    }
    fn for_each_logical_line_in_stable_range_mut(&self, range: Range<isize>, f: &mut dyn ForEachPaneLogicalLine) {
        terminal_for_each_logical_line_in_stable_range_mut(&mut self.actor.terminal.lock().unwrap(), range, f)
    }
    fn get_logical_lines(&self, range: Range<isize>) -> Vec<LogicalLine> {
        impl_get_logical_lines_via_get_lines(self, range)
    }
    fn get_dimensions(&self) -> RenderableDimensions {
        terminal_get_dimensions(&mut self.actor.terminal.lock().unwrap())
    }
    fn get_config(&self) -> Option<Arc<dyn wezterm_term::TerminalConfiguration>> {
        Some(self.actor.terminal.lock().unwrap().get_config())
    }
    fn get_progress(&self) -> wezterm_term::Progress {self.actor.terminal.lock().unwrap().get_progress()}
    fn has_unseen_output(&self) -> bool {self.actor.terminal.lock().unwrap().has_unseen_output()}
    fn focus_changed(&self, focused: bool) {self.actor.terminal.lock().unwrap().focus_changed(focused);}
    fn get_foreground_process_name(&self, _: CachePolicy) -> Option<String> {
        let value=self.vars.lock().get("WEZTERM_REX_PROCESS")?.clone();
        let process:Value=serde_json::from_str(&value).ok()?;
        process["path"].as_str().or_else(||process["name"].as_str()).map(str::to_owned)
    }
    fn get_title(&self) -> String {
        let error=self.actor.frame.lock().unwrap().error.clone();
        error.map(|e|format!("Rex disconnected: {e}"))
            .unwrap_or_else(||self.actor.terminal.lock().unwrap().get_title().to_owned())
    }
    fn send_paste(&self, text: &str) -> Result<()> {
        if self.detached.load(Ordering::Relaxed) { return Ok(()); }
        self.send_input("paste", json!({"text":text}))
    }
    fn reader(&self) -> Result<Option<Box<dyn std::io::Read + Send>>> {
        Ok(None)
    }
    fn writer(&self) -> MappedMutexGuard<'_, dyn Write> {
        parking_lot::MutexGuard::map(self.input.lock(), |i| i as &mut dyn Write)
    }
    fn resize(&self, size: TerminalSize) -> Result<()> {
        let mut old = self.size.lock();
        if *old == size {
            return Ok(());
        }
        self.actor.resize_local(size);
        self.queue_size(size)?;
        *old = size;
        Ok(())
    }
    fn key_down(&self, k: KeyCode, m: KeyModifiers) -> Result<()> {
        self.key(k, m, false)
    }
    fn key_up(&self, k: KeyCode, m: KeyModifiers) -> Result<()> {
        self.key(k, m, true)
    }
    fn mouse_event(&self, e: MouseEvent) -> Result<()> {
        let wheel = match e.button {
            MouseButton::WheelUp(n) => Some((0, -(n as i64))),
            MouseButton::WheelDown(n) => Some((0, n as i64)),
            MouseButton::WheelLeft(n) => Some((-(n as i64), 0)),
            MouseButton::WheelRight(n) => Some((n as i64, 0)),
            _ => None,
        };
        if let Some((dx, dy)) = wheel {
            return self
                .input
                .lock()
                .event("wheel", json!({"x":e.x,"y":e.y,"delta_x":dx,"delta_y":dy}));
        }
        let button = match e.button {
            MouseButton::Left => "left",
            MouseButton::Right => "right",
            MouseButton::Middle => "middle",
            _ => "none",
        };
        let mut payload=json!({"action":match e.kind{MouseEventKind::Press=>"press",MouseEventKind::Release=>"release",MouseEventKind::Move=>"motion"},"x":e.x,"y":e.y,"mods":modifiers(e.modifiers)});
        if button!="none" {payload["button"]=json!(button);}
        self.input.lock().event("mouse",payload)
    }
    fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
    }
    fn set_config(&self, config: Arc<dyn wezterm_term::TerminalConfiguration>) {
        self.actor.terminal.lock().unwrap().set_config(config.clone());
        *self.terminal_config.lock()=Some(config);
    }
    fn palette(&self) -> ColorPalette { self.actor.terminal.lock().unwrap().palette() }
    fn is_mouse_grabbed(&self) -> bool { self.actor.terminal.lock().unwrap().is_mouse_grabbed() }
    fn is_alt_screen_active(&self) -> bool { self.actor.terminal.lock().unwrap().is_alt_screen_active() }
    fn get_current_working_dir(&self, _: CachePolicy) -> Option<url::Url> {
        self.actor.terminal.lock().unwrap().get_current_dir().cloned()
    }
    fn copy_user_vars(&self) -> HashMap<String, String> {
        let mut vars=self.actor.terminal.lock().unwrap().user_vars().clone();
        vars.remove("WEZTERM_PROGRAM_STATUS");
        vars.extend(self.vars.lock().clone());vars
    }
    fn set_clipboard(&self, clipboard: &Arc<dyn wezterm_term::Clipboard>) {
        // The normal terminal receives OSC 52; the output observer below only
        // handles status events that aren't part of the native terminal model.
        self.actor.terminal.lock().unwrap().set_clipboard(clipboard);
    }
    fn set_download_handler(&self, handler: &Arc<dyn wezterm_term::DownloadHandler>) {
        self.actor.terminal.lock().unwrap().set_download_handler(handler);
    }
    async fn search(
        &self,
        pattern: Pattern,
        range: Range<isize>,
        limit: Option<u32>,
    ) -> Result<Vec<SearchResult>> {
        let actor = self.actor.clone();
        smol::unblock(move || -> Result<Vec<SearchResult>> {
            let expression = match &pattern {
                Pattern::Regex(s) => s.clone(),
                Pattern::CaseInSensitiveString(s) => format!("(?i){}", fancy_regex::escape(s)),
                Pattern::CaseSmartString(s) if !s.chars().any(char::is_uppercase) => {
                    format!("(?i){}", fancy_regex::escape(s))
                }
                _ => fancy_regex::escape(&pattern).into_owned(),
            };
            if expression.is_empty() {
                return Ok(vec![]);
            }
            let regex = fancy_regex::Regex::new(&expression)?;
            // Read complete logical lines from the native client snapshot.
            // Chunk boundaries may bisect a wrapped match; never search two
            // truncated halves or consult the remote server for terminal text.
            let mut logical = Vec::new();
            {
                let term=actor.terminal.lock().unwrap();
                term.screen().for_each_display_line_in_stable_range(range.clone(), |rows, lines| {
                    logical.push((rows.start,lines.iter().map(|l| (*l).clone()).collect::<Vec<_>>()));
                    true
                });
            }
            let mut results = vec![];
            for (first, rows) in logical {
                let text=rows.iter().map(|r| r.as_str()).collect::<String>();
                let matches=regex.find_iter(&text).collect::<std::result::Result<Vec<_>,_>>()?;
                if matches.is_empty() { continue; }
                // Only matching lines need a byte-to-terminal-cell map.
                let mut positions=Vec::with_capacity(text.len());
                for (i,row) in rows.iter().enumerate() {
                    for cell in row.visible_cells() {
                        positions.extend(std::iter::repeat_n((first+i as isize,cell.cell_index(),cell.width()),cell.str().len()));
                    }
                }
                for m in matches {
                    if m.start()==m.end() {continue;}
                    let (sy,sx,_)=positions[m.start()];
                    let (ey,ex,ew)=positions[m.end()-1];
                    if !range.contains(&sy) {continue;}
                    results.push(SearchResult {start_y:sy,start_x:sx,end_y:ey,end_x:ex+ew,match_id:results.len()});
                    if limit.is_some_and(|n|results.len()>=n as usize) {return Ok(results);}
                }
            }
            Ok(results)
        })
        .await
    }
    fn get_semantic_zones(&self) -> Result<Vec<wezterm_term::SemanticZone>> {
        self.actor.terminal.lock().unwrap().get_semantic_zones()
    }
    fn can_close_without_prompting(&self, _: CloseReason) -> bool {
        true
    }
    fn kill(&self) {
        if self.dead.swap(true, Ordering::Relaxed) {
            return;
        }
        let i = self.input.lock();
        let _ = i.call(
            "block.close",
            json!({"session_id":i.session,"block_id":i.block}),
        );
    }
    fn prepare_for_display(&self) {
        self.actor.activate();
    }
    fn advise_focus(&self) {
        self.actor.activate();
        let Some(domain) = Mux::get().get_domain(self.domain) else { return; };
        let Some(domain) = domain.downcast_ref::<RexDomain>() else { return; };
        // A layout update can temporarily show a fallback tab while awaiting a
        // new block. Its paint is not a user focus action. Track focus once per
        // domain, and commit the server's authoritative focus after applying it.
        if domain.config.independent || domain.applying.try_lock().is_none() { return; }
        let block = {
            let mut state = domain.state.lock();
            let active = state.window.and_then(|w| Mux::get().get_active_tab_for_window(w))
                .and_then(|t| t.get_active_pane()).map(|p| p.pane_id());
            if active != Some(self.id) || state.focused_pane == Some(self.id) { return; }
            // An agent shell is in no server window: focus the block that has
            // the focus in the popup's window, so that the server knows which
            // window is active (a session switch comes back to it).
            let block = if self.detached.load(Ordering::Relaxed) {
                let window = self.window.lock().clone();
                let found = state.view["windows"].as_array().into_iter().flatten()
                    .find(|w| w["window_id"] == window.as_str())
                    .and_then(|w| w["focused_block_id"].as_str()).map(str::to_owned);
                let Some(block) = found else { return; };
                block
            } else {
                self.block.clone()
            };
            log::debug!("Rex GUI focus {:?} -> {} block {}",state.focused_pane,self.id,block);
            state.focused_pane = Some(self.id);
            block
        };
        let i = self.input.lock();
        let _ = i.call(
            "session.focus_block",
            json!({"session_id":i.session,"block_id":block}),
        );
    }
}
