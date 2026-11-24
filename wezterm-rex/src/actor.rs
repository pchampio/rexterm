//! Rex transports terminal output; WezTerm owns live terminal interpretation.
//! libghostty is used only by restore to decode the server snapshot on attach.
use crate::{snapshot::SnapshotBuffer, transport::{self, Client}, wire};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{ops::Range, sync::{Arc, Mutex}, time::Duration};
use termwiz::surface::Line;
use wezterm_term::{Terminal, TerminalConfiguration, TerminalSize};

#[derive(Clone, Debug, Default)]
pub struct Frame {
    pub seq: usize,
    pub base: isize,
    pub top: isize,
    pub total: usize,
    pub cols: usize,
    pub rows: usize,
    pub connected: bool,
    pub error: Option<String>,
}
pub enum Event { Activated, Alert(wezterm_term::Alert), Metadata(Value), Updated }
struct AlertRelay(Arc<dyn Fn(Event)+Send+Sync>);
impl wezterm_term::AlertHandler for AlertRelay {
    fn alert(&mut self, alert:wezterm_term::Alert) {(self.0)(Event::Alert(alert));}
}
#[derive(Clone)]
pub struct Actor {
    pub frame: Arc<Mutex<Frame>>,
    pub terminal: Arc<Mutex<Terminal>>,
    desired_size: Arc<Mutex<Option<TerminalSize>>>,
    _lifetime: flume::Sender<()>,
    activate: flume::Sender<()>,
    /// A terminal of a session that the GUI left: the server closed its data
    /// channel, and it does not try again until `park(false)` wakes it.
    parked: Arc<std::sync::atomic::AtomicBool>,
    wake: Arc<tokio::sync::Notify>,
}
#[derive(Debug)]
struct DefaultConfig;
impl TerminalConfiguration for DefaultConfig {
    fn color_palette(&self) -> wezterm_term::color::ColorPalette { Default::default() }
    fn scrollback_size(&self) -> usize { 100_000 }
    fn enable_kitty_graphics(&self) -> bool { true }
}
impl Actor {
    /// Resize the native renderer immediately; older server acknowledgements
    /// must not pull the visible pane back during a continuous drag.
    pub fn resize_local(&self, size: TerminalSize) {
        let mut desired = self.desired_size.lock().unwrap();
        *desired = Some(size);
        let mut term = self.terminal.lock().unwrap();
        term.preview_resize(size);
        let screen = term.screen();
        let mut frame = self.frame.lock().unwrap();
        frame.seq = term.current_seqno();
        frame.base = screen.phys_to_stable_row_index(0);
        frame.top = screen.visible_row_to_stable_row(0);
        frame.total = screen.scrollback_rows();
        frame.cols = screen.physical_cols;
        frame.rows = screen.physical_rows;
    }

    pub async fn start(endpoint: String, client: Client, session: String, block: String,
                       notify: impl Fn(Event) + Send + Sync + 'static) -> Result<Self> {
        Self::start_with_config(endpoint, client, session, block, Arc::new(DefaultConfig), notify).await
    }
    pub async fn start_with_config(endpoint: String, client: Client, session: String, block: String,
                       config: Arc<dyn TerminalConfiguration>,
                       notify: impl Fn(Event) + Send + Sync + 'static) -> Result<Self> {
        Self::start_with_config_ready(endpoint,client,session,block,config,notify,true).await
    }
    /// Newly created server blocks can be displayed immediately while their
    /// data channel catches up. Reattachments still await the full snapshot.
    pub async fn start_with_config_ready(endpoint: String, client: Client, session: String, block: String,
                       config: Arc<dyn TerminalConfiguration>,
                       notify: impl Fn(Event) + Send + Sync + 'static, wait_ready: bool) -> Result<Self> {
        Self::start_with_config_deferred(endpoint,client,session,block,config,notify,wait_ready,false).await
    }
    pub fn activate(&self) {self.activate.try_send(()).ok();}
    /// Park (no reconnect attempts) or wake: a woken terminal reconnects at once.
    pub fn park(&self, parked: bool) {
        self.parked.store(parked, std::sync::atomic::Ordering::Relaxed);
        if !parked {self.wake.notify_one();}
    }
    /// Inactive tabs do not need a snapshot or data socket until displayed.
    pub async fn start_with_config_deferred(endpoint: String, client: Client, session: String, block: String,
                       config: Arc<dyn TerminalConfiguration>,
                       notify: impl Fn(Event) + Send + Sync + 'static, wait_ready: bool, deferred: bool) -> Result<Self> {
        let notify:Arc<dyn Fn(Event)+Send+Sync>=Arc::new(notify);
        let frame = Arc::new(Mutex::new(Frame {connected:!wait_ready,..Frame::default()}));
        let mut native = Terminal::new(TerminalSize::default(), config, "WezTerm", "rex", Box::new(std::io::sink()));
        native.set_mirrored_output(true);
        native.set_notification_handler(Box::new(AlertRelay(notify.clone())));
        let terminal = Arc::new(Mutex::new(native));
        let shared = frame.clone(); let term = terminal.clone();
        let desired_size = Arc::new(Mutex::new(None));
        let resize_target = desired_size.clone();
        let (lifetime, closed) = flume::bounded::<()>(1);
        let (ready_tx, ready_rx) = flume::bounded(1);
        let (activate, activated) = flume::bounded(1);
        if !deferred || wait_ready {activate.send(()).ok();}
        let parked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let wake = Arc::new(tokio::sync::Notify::new());
        let (parked_loop, wake_loop) = (parked.clone(), wake.clone());
        std::thread::Builder::new().name("rex-terminal".into()).spawn(move || {
            let result = (|| -> Result<()> {
                tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
                    tokio::select! { _=closed.recv_async()=>return Ok(()), _=activated.recv_async()=>{} }
                    notify(Event::Activated);
                    // The screen shows at READY, before the history arrives.
                    let mut shown=||{
                        if let Some(size)=*resize_target.lock().unwrap() {term.lock().unwrap().preview_resize(size);}
                        publish(&term, &shared); notify(Event::Updated);
                        ready_tx.try_send(Ok(())).ok();
                    };
                    let (mut ws, mut welcome, mut offset, mut parser) = fresh(&endpoint, &client, &session, &block, &term, &*notify, &mut shown).await?;
                    shown();
                    let mut hold_timer=tokio::time::interval(Duration::from_millis(50));
                    loop {
                        tokio::select! {
                            _=hold_timer.tick(), if term.lock().unwrap().has_pending_synchronized_output() => {
                                let size=*resize_target.lock().unwrap();
                                let changed={let mut t=term.lock().unwrap();t.cancel_resize_preview();
                                    let changed=t.flush_remote_output_if_due();
                                    if let Some(size)=size {t.preview_resize(size);} changed};
                                if changed {publish(&term,&shared);notify(Event::Updated);}
                            },
                            _=closed.recv_async() => return Ok(()),
                            result=wire::next_binary(&mut ws) => match result {
                                Ok(bytes) => {
                                    offset += bytes.len() as u64;
                                    feed(&term, &mut parser, &bytes, &*notify, Some(&resize_target))?;
                                    publish(&term, &shared); notify(Event::Updated);
                                },
                                Err(error) => {
                                    { let mut f=shared.lock().unwrap(); f.connected=false; f.error=Some(error.to_string()); }
                                    notify(Event::Updated);
                                    let resume_token=welcome.token; let resume_nonce=welcome.nonce; let resume_offset=offset;
                                    let reconnect = async {
                                        let mut attempt=0u32;
                                        loop {
                                            if parked_loop.load(std::sync::atomic::Ordering::Relaxed) {
                                                wake_loop.notified().await;
                                            } else {
                                                // A wake (the session shows again) tries at once.
                                                tokio::select! {
                                                    _=tokio::time::sleep(Duration::from_millis(100 << attempt.min(4)))=>{},
                                                    _=wake_loop.notified()=>{},
                                                }
                                            }
                                            attempt=attempt.saturating_add(1);
                                            if let Ok(mut socket)=transport::socket(&endpoint,"/api/data",wire::PROTOCOL).await {
                                                if let Ok(w)=wire::resume(&mut socket,&resume_token,resume_offset).await {
                                                    anyhow::ensure!(w.nonce==resume_nonce,"resume nonce changed");
                                                    return Ok::<_,anyhow::Error>((socket,w,None));
                                                }
                                            }
                                            if client.is_connected() {
                                                let mut shown=||{publish(&term,&shared);notify(Event::Updated);};
                                                if let Ok((socket,w,received,p))=fresh(&endpoint,&client,&session,&block,&term,&*notify,&mut shown).await {
                                                    return Ok((socket,w,Some((received,p))));
                                                }
                                            }
                                        }
                                    };
                                    let (socket,w,fresh)=tokio::select! {
                                        _=closed.recv_async()=>return Ok(()),
                                        result=reconnect=>result?,
                                    };
                                    ws=socket; welcome=w;
                                    if let Some((received,p))=fresh {
                                        offset=received;parser=p;
                                        if let Some(size)=*resize_target.lock().unwrap() {term.lock().unwrap().preview_resize(size);}
                                    }
                                    publish(&term,&shared); notify(Event::Updated);
                                }
                            }
                        }
                    }
                })
            })();
            if let Err(error)=result {
                let message=format!("{error:#}"); ready_tx.send(Err(anyhow::anyhow!(message.clone()))).ok();
                {let mut f=shared.lock().unwrap();f.connected=false;f.error=Some(message);}
                notify(Event::Updated);
            }
        })?;
        if wait_ready {ready_rx.recv_async().await??;}
        Ok(Self { frame, terminal, desired_size, _lifetime: lifetime, activate, parked, wake })
    }
    pub fn lines(&self, range: Range<isize>) -> (isize, Vec<Line>) {
        self.activate();
        let t=self.terminal.lock().unwrap();
        let screen=t.screen();let range=screen.stable_range(&range);
        (screen.phys_to_stable_row_index(range.start),screen.lines_in_phys_range(range))
    }
}
fn publish(term: &Mutex<Terminal>, shared: &Mutex<Frame>) {
    let t=term.lock().unwrap();let s=t.screen();
    *shared.lock().unwrap()=Frame {
        seq:t.current_seqno(), base:s.phys_to_stable_row_index(0),top:s.visible_row_to_stable_row(0),
        total:s.scrollback_rows(),cols:s.physical_cols,rows:s.physical_rows,connected:true,error:None,
    };
}
fn feed(term: &Mutex<Terminal>, parser: &mut wire::ApcParser, bytes: &[u8], notify: &dyn Fn(Event), desired: Option<&Mutex<Option<TerminalSize>>>) -> Result<()> {
    // Hold both locks across parsing and projection so painting never sees a
    // transient old geometry, and resize_local cannot race a pending redraw.
    let mut target=desired.map(|target|target.lock().unwrap());
    let mut t=term.lock().unwrap();
    t.cancel_resize_preview();
    for part in parser.feed(bytes)? {
        match part {
            wire::Part::Output(bytes) => {
                t.advance_bytes(&bytes);

            },
            wire::Part::Metadata(value) => {
                {
                    if value["type"]=="resize" {
                        let mut size=t.get_size();
                        size.cols=value["cols"].as_u64().context("cols")? as usize;
                        size.rows=value["rows"].as_u64().context("rows")? as usize;
                        size.pixel_width=size.cols*value["cell_width_px"].as_u64().unwrap_or(0) as usize;
                        size.pixel_height=size.rows*value["cell_height_px"].as_u64().unwrap_or(0) as usize;
                        t.resize(size);
                        if target.as_ref().and_then(|target|**target).is_some_and(|expected|expected.cols==size.cols && expected.rows==size.rows) {
                            if let Some(target)=target.as_mut() {**target=None;}
                        }
                    }
                    if value["type"]=="theme" {
                        let rgb=|v:&Value|v.as_str().and_then(|s|s.parse::<termwiz::color::SrgbaTuple>().ok());
                        let palette=t.palette_mut();
                        if let Some(c)=rgb(&value["foreground"]) {palette.foreground=c;}
                        if let Some(c)=rgb(&value["background"]) {palette.background=c;}
                        if let Some(c)=rgb(&value["cursor"]) {palette.cursor_bg=c;palette.cursor_border=c;}
                        for entry in value["palette"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                            if let Some((i,c))=entry.split_once('=') {
                                if let (Ok(i),Ok(c))=(i.parse::<u8>(),c.parse::<termwiz::color::SrgbaTuple>()) {palette.colors.0[i as usize]=c;}
                            }
                        }
                        t.make_all_lines_dirty();
                    }
                }
                notify(Event::Metadata(value));
            }
        }
    }
    if let Some(size)=target.as_ref().and_then(|target|**target) {t.preview_resize(size);}
    Ok(())
}
async fn fresh(endpoint:&str, client:&Client, session:&str, block:&str, terminal:&Mutex<Terminal>, notify:&dyn Fn(Event),
    shown:&mut dyn FnMut())
    -> Result<(transport::Socket, wire::Welcome, u64, wire::ApcParser)> {
    let (ticket,mut ws)=tokio::try_join!(
        client.call("data.connect",json!({"session_id":session,"block_id":block,"data_ref_id":"terminal","subprotocol":wire::PROTOCOL})),
        transport::socket(endpoint,"/api/data",wire::PROTOCOL)
    )?;
    let welcome=wire::handshake(&mut ws,ticket["ticket"].as_str().context("missing data ticket")?).await?;
    let mut offset=0;let mut decoder=SnapshotBuffer::default();let mut parser=wire::ApcParser::new(welcome.nonce);
    let mut shown_early=false;
    loop {
        let bytes=tokio::time::timeout(Duration::from_secs(15),wire::next_binary(&mut ws)).await??;
        offset+=bytes.len() as u64;
        let complete=decoder.push(&bytes)?;
        let early=decoder.take_ready().is_some_and(|screen|{
            // A screen that cannot be shown early still comes with the rest.
            crate::restore::restore(screen,&mut terminal.lock().unwrap()).is_ok()
        });
        if early {shown_early=true;shown();}
        if let Some((snapshot,tail))=complete {
            if shown_early {
                let (config,size)={let t=terminal.lock().unwrap();(t.get_config(),t.get_size())};
                let history=crate::restore::history(snapshot,config,size)?;
                crate::restore::merge_history(history,&mut terminal.lock().unwrap())?;
            } else {
                crate::restore::restore(snapshot,&mut terminal.lock().unwrap())?;
            }
            feed(terminal,&mut parser,&tail,notify,None)?;
            return Ok((ws,welcome,offset,parser));
        }
    }
}

#[cfg(test)]
mod resize_tests {
    use super::*;
    fn terminal() -> Terminal {
        Terminal::new(TerminalSize{cols:40,rows:8,pixel_width:320,pixel_height:128,dpi:96},
            Arc::new(DefaultConfig),"WezTerm","test",Box::new(std::io::sink()))
    }
    fn text(t:&Terminal)->Vec<String> {
        t.screen().lines_in_phys_range(0..t.screen().scrollback_rows()).iter().map(|l|l.as_str().into_owned()).collect()
    }
    #[test]
    fn local_resize_does_not_reinterpret_inflight_shell_redraw() {
        let term=Mutex::new(terminal());let mut expected=terminal();let mut parser=wire::ApcParser::new(7);
        let initial=b"/home/user1 @server\r\n$~ a long command being edited";
        expected.advance_bytes(initial);term.lock().unwrap().advance_bytes(initial);
        let size=TerminalSize{cols:11,rows:15,pixel_width:88,pixel_height:240,dpi:96};
        let desired=Mutex::new(Some(size));term.lock().unwrap().preview_resize(size);
        // This redraw was produced before the remote PTY received SIGWINCH.
        let old=b"\r\x1b[2K$~ another long command";
        expected.advance_bytes(old);feed(&term,&mut parser,old,&|_|{},Some(&desired)).unwrap();
        let mut projected=term.lock().unwrap();projected.cancel_resize_preview();
        assert_eq!(text(&projected),text(&expected));drop(projected);
        let event=format!("\x1b_rex1;7;{}\x1b\\",json!({"type":"resize","cols":11,"rows":15,"cell_width_px":8,"cell_height_px":16}));
        expected.resize(size);expected.advance_bytes(b"\r\x1b[2K$~ final");
        feed(&term,&mut parser,format!("{event}\r\x1b[2K$~ final").as_bytes(),&|_|{},Some(&desired)).unwrap();
        assert_eq!(text(&term.lock().unwrap()),text(&expected));
    }
    #[test]
    fn resize_previews_are_reversible_with_wrapped_history_and_saved_cursor() {
        let mut t=terminal();t.advance_bytes(b"one two three four five six seven eight nine ten\r\n\x1b7$~ ");
        let baseline=text(&t);let cursor=t.cursor_pos();
        for cols in [7,17,5,39,60,8] {
            t.preview_resize(TerminalSize{cols,rows:3,pixel_width:cols*8,pixel_height:48,dpi:96});
        }
        t.cancel_resize_preview();assert_eq!(text(&t),baseline);assert_eq!(t.cursor_pos().x,cursor.x);assert_eq!(t.cursor_pos().y,cursor.y);
        t.advance_bytes(b"\x1b8RESTORED");assert!(text(&t).join("\n").contains("RESTORED"));
    }
}
