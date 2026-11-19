//! Shell-initiated attachment over the existing PTY (local or SSH).
use crate::{Mux, Domain, pane::PaneId, rex::RexDomain};
use anyhow::{Result,Context};
use serde_json::Value;
use std::{collections::HashMap,sync::{Arc,Mutex,LazyLock}};
static SOURCES: LazyLock<Mutex<HashMap<PaneId,(String,Option<usize>)>>> = LazyLock::new(||Mutex::new(HashMap::new()));

pub fn handle(pane_id:PaneId, bytes:Vec<u8>) {
    let result=(||->Result<()> {
        anyhow::ensure!(bytes.len()<128*1024,"Rex bridge packet exceeds limit");
        let message:Value=serde_json::from_slice(&bytes)?;
        let token=message["token"].as_str().context("Rex bridge token")?.to_owned();
        if message["type"]=="hello" {
            anyhow::ensure!(message["version"]==1,"unsupported Rex attachment version");
            anyhow::ensure!(token.len()==32 && token.bytes().all(|b|b.is_ascii_hexdigit()),"invalid Rex attachment token");
            let session=message["session"].as_str().unwrap_or("main").to_owned();
            let request=message.clone();
            anyhow::ensure!(!session.is_empty() && session.len()<=256,"invalid Rex session name");
            {
                let mut sources=SOURCES.lock().unwrap();
                anyhow::ensure!(!sources.contains_key(&pane_id),"shell already attaching to Rex");
                sources.insert(pane_id,(token.clone(),None));
            }
            promise::spawn::spawn_into_main_thread(async move {
                promise::spawn::spawn(async move {
                    if let Err(error)=attach(pane_id,token,session,request).await {
                        release(pane_id);
                        log::error!("Rex shell attachment: {error:#}");
                        crate::localpane::emit_output_for_pane(pane_id,&format!("\r\nRex attachment failed: {error:#}\r\n"));
                    }
                }).detach();
            }).detach();
        } else {
            let Some(binding)=SOURCES.lock().unwrap().get(&pane_id).cloned() else {return Ok(());};
            if binding.0!=token {return Ok(());}

            if message["type"]=="bye" {
                if let Some(id)=binding.1 {
                    promise::spawn::spawn_into_main_thread(async move {
                        if let Some(domain)=Mux::get().get_domain(id) {domain.detach().ok();}
                    }).detach();
                } else {SOURCES.lock().unwrap().remove(&pane_id);}
            } else {
                wezterm_rex::bridge::receive(&token,message)?;
            }
        }
        Ok(())
    })();
    if let Err(e)=result {log::warn!("Rex bridge: {e:#}");}
}
async fn attach(pane_id:PaneId,token:String,session:String,request:Value)->Result<()> {
    let mux=Mux::get();
    anyhow::ensure!(SOURCES.lock().unwrap().get(&pane_id).is_some_and(|(t,d)|t==&token && d.is_none()),"Rex attachment cancelled");
    let pane=mux.get_pane(pane_id).context("attachment shell exited")?;
    let (_,window,tab)=mux.resolve_pane_id(pane_id).context("attachment shell has no window")?;
    let source=pane.clone();let outgoing_token=token.clone();
    // A local helper may use the Unix socket directly. Trust this optimization
    // only when the foreground OS process is that exact helper PID; a remote
    // SSH peer cannot request connections to arbitrary local sockets.
    let local=pane.get_foreground_process_info(crate::pane::CachePolicy::FetchImmediate)
        .is_some_and(|p|Some(u64::from(p.pid))==request["pid"].as_u64()
            && p.executable.file_name().is_some_and(|n|n.to_string_lossy().starts_with("python"))
            && p.argv.get(1).is_some_and(|arg|std::path::Path::new(arg).file_name().is_some_and(|n|n=="rex-attach")));
    let endpoint=if local {
        request["endpoint"].as_str().filter(|e|e.starts_with("unix:///"))
            .map(str::to_owned).unwrap_or_else(||format!("bridge://{token}"))
    } else {format!("bridge://{token}")};

    wezterm_rex::bridge::register(token.clone(),Arc::new(move |mut message| {
        message["token"]=Value::String(outgoing_token.clone());
        let mut bytes=serde_json::to_vec(&message)?;bytes.push(b'\n');
        source.writer().write_all(&bytes)?;Ok(())
    }))?;
    let domain=Arc::new(RexDomain::new(config::RexDomain {
        name:format!("rex-shell-{pane_id}"),endpoint,session,
        initial_cwd:request["cwd"].as_str().map(str::to_owned),
        remote_shell:request["remote"].as_bool().unwrap_or(false),
        independent:request["independent"].as_bool().unwrap_or(false),
    }));
    let id=domain.domain_id();
    SOURCES.lock().unwrap().insert(pane_id,(token.clone(),Some(id)));
    domain.bind_attachment(pane_id,window,tab);
    let dynamic:Arc<dyn Domain>=domain.clone();mux.add_domain(&dynamic);
    // Acknowledge before waiting for the server so a slow attachment cannot
    // make the shell helper time out and resume interpreting transport bytes.
    let mut reply=serde_json::to_vec(&serde_json::json!({"type":"attached","token":token}))?;
    reply.push(b'\n');pane.writer().write_all(&reply)?;
    if let Err(e)=domain.attach(Some(window)).await {
        domain.detach().ok();return Err(e);
    }
    if domain.state()!=crate::domain::DomainState::Attached {return Ok(());}
    // Keep the connection shell alive in the mux, but replace its single-pane
    // tab with the server's native tabs until the user detaches.
    if mux.get_tab(tab).is_some_and(|t|t.iter_panes().len()==1) {
        if let Some(mut win)=mux.get_window_mut(window) {win.remove_tab_id(tab);}
    }
    Ok(())
}
pub fn release(pane_id:PaneId) {
    if let Some((token,_))=SOURCES.lock().unwrap().remove(&pane_id) {wezterm_rex::bridge::close(&token);}
}
