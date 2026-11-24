//! TCP forwards through a Rex server, like `ssh -L` and `ssh -R` and
//! `rex forward new`. Each forward is a tunnel block in a session:
//!
//! - Local (`-L`): a dial tunnel (`com.superlogical.tunnel.dial`) on the
//!   server, and a listener on this machine. Each accepted connection opens one
//!   stream on the block (ref `conn:<name>`), and the server connects it to
//!   the target.
//! - Remote (`-R`): a listen tunnel (`com.superlogical.tunnel.listen`) on the
//!   server. For each connection it accepts, the block sends a
//!   `connection_requested` event with a ref; this side connects to the
//!   target and opens the stream with that ref.
//!
//! Streams use `rex.data.bytes.v1`: an empty message is a half-close, a close
//! with an error resets the connection.
//!
//! A forward has its own control connection, attached to the session of the
//! block: the GUI's connection attaches one session at a time and switches,
//! and only the block's owner may open its streams. The forward stops when it
//! is dropped (and closes its block), or when its block is closed elsewhere.
use crate::{transport, transport::Client, upload, wire};
use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, frame::coding::CloseCode};

const CHUNK: usize = 64 * 1024;
const DIAL: &str = "com.superlogical.tunnel.dial";
const LISTEN: &str = "com.superlogical.tunnel.listen";

/// What a forward does: listen on `bind:port` on one side, connect each
/// connection to `host:host_port` from the other side.
#[derive(Clone, Debug)]
pub struct Spec {
    /// true: listen on the server, connect from this machine (`-R`).
    pub remote: bool,
    pub bind: String,
    /// 0 picks a free port.
    pub port: u16,
    pub host: String,
    pub host_port: u16,
    /// Local only: listen on a free port when `port` is taken.
    pub any_port: bool,
}

impl Spec {
    /// `[bind:]port:host:hostport`, or `port` alone (the same port on
    /// localhost), as `ssh -L`/`-R` take it. `*` binds every interface;
    /// an IPv6 address goes in brackets.
    pub fn parse(remote: bool, text: &str) -> Result<Self> {
        let mut parts = Vec::new();
        let mut current = String::new();
        let mut bracket = false;
        for c in text.trim().chars() {
            match c {
                '[' => bracket = true,
                ']' => bracket = false,
                ':' if !bracket => parts.push(std::mem::take(&mut current)),
                c => current.push(c),
            }
        }
        parts.push(current);
        let number = |s: &str| s.parse::<u16>().with_context(|| format!("{s:?} is not a port"));
        let (bind, port, host, host_port) = match parts.as_slice() {
            [port] => ("127.0.0.1".to_owned(), number(port)?, "localhost".to_owned(), number(port)?),
            [port, host, host_port] => ("127.0.0.1".to_owned(), number(port)?, host.clone(), number(host_port)?),
            [bind, port, host, host_port] => (
                if bind == "*" || bind.is_empty() { "0.0.0.0".to_owned() } else { bind.clone() },
                number(port)?, host.clone(), number(host_port)?,
            ),
            _ => bail!("expected [bind_address:]port:host:hostport, got {text:?}"),
        };
        if host.is_empty() || host_port == 0 {
            bail!("the target needs a host and a port other than 0");
        }
        Ok(Spec { remote, bind, port, host, host_port, any_port: false })
    }
}

/// A running forward. Dropping it closes its listener and its block.
pub struct Forward {
    /// Where it listens on this machine (local forwards).
    pub local: SocketAddr,
    /// Where it listens, as host:port (on the server for a remote forward).
    pub listen: String,
    /// Where it connects to, as host:port.
    pub target: String,
    pub remote: bool,
    pub block: String,
    alive: Arc<AtomicBool>,
    stop: flume::Sender<()>,
}

impl Forward {
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}

impl Drop for Forward {
    fn drop(&mut self) {
        self.stop.try_send(()).ok();
    }
}

/// Listen on 127.0.0.1:`preferred` (or a free port when it is taken) and
/// forward each connection to `host:port` from the server.
pub fn open(endpoint: String, session: String, host: String, port: u16, preferred: u16) -> Result<Forward> {
    start(endpoint, session, Spec {
        remote: false, bind: "127.0.0.1".into(), port: preferred, host, host_port: port, any_port: true,
    })
}

struct Opened {
    local: SocketAddr,
    listen: String,
    block: String,
}

/// Start a forward; it runs on its own thread until dropped.
pub fn start(endpoint: String, session: String, spec: Spec) -> Result<Forward> {
    let (ready_tx, ready_rx) = flume::bounded::<Result<Opened>>(1);
    let (stop, stopped) = flume::bounded::<()>(1);
    let alive = Arc::new(AtomicBool::new(true));
    let running = alive.clone();
    let target = format!("{}:{}", spec.host, spec.host_port);
    let remote = spec.remote;
    std::thread::Builder::new().name("rex-forward".into()).spawn(move || {
        let ready = ready_tx.clone();
        let result = (move || -> Result<()> {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            let local = tokio::task::LocalSet::new();
            local.block_on(&runtime, run(endpoint, session, spec, ready, stopped))
        })();
        running.store(false, Ordering::Relaxed);
        if let Err(error) = result {
            ready_tx.send(Err(error)).ok();
        }
    })?;
    let opened = ready_rx.recv().context("Rex forward thread ended")??;
    Ok(Forward { local: opened.local, listen: opened.listen, target, remote, block: opened.block, alive, stop })
}

async fn run(endpoint: String, session: String, spec: Spec, ready: flume::Sender<Result<Opened>>,
             stopped: flume::Receiver<()>) -> Result<()> {
    let target = format!("{}:{}", spec.host, spec.host_port);
    let listener = if spec.remote {
        None
    } else {
        Some(match tokio::net::TcpListener::bind((spec.bind.as_str(), spec.port)).await {
            Ok(listener) => listener,
            Err(_) if spec.any_port => tokio::net::TcpListener::bind((spec.bind.as_str(), 0)).await?,
            Err(error) => return Err(error).with_context(|| format!("listen on {}:{}", spec.bind, spec.port)),
        })
    };
    let local = match &listener {
        Some(listener) => listener.local_addr()?,
        None => SocketAddr::from(([0, 0, 0, 0], 0)),
    };
    let (client, events) = Client::connect(endpoint.clone()).await?;
    client.call("session.attach", json!({"session_id": session})).await?;
    let (flavor, options, label) = if spec.remote {
        (LISTEN, json!({"address": spec.bind, "port": spec.port, "client_target": target}),
         format!("forward server {}:{} → {target}", spec.bind, spec.port))
    } else {
        (DIAL, json!({"host": spec.host, "port": spec.host_port, "client_address": local.to_string()}),
         format!("forward {local} → {target}"))
    };
    let created = client.call("session.new_block", json!({
        "session_id": session, "flavor": flavor, "label": label, "options": options,
    })).await?;
    let block = created["block_id"].as_str().context("tunnel block ID")?.to_owned();
    let listen = if spec.remote {
        // The port the server picked, for port 0.
        let info = client.call("com.superlogical.tunnel.info",
            json!({"session_id": session, "block_id": block, "args": {}})).await;
        info.ok().and_then(|i| i["address"].as_str().map(str::to_owned))
            .unwrap_or_else(|| format!("{}:{}", spec.bind, spec.port))
    } else {
        local.to_string()
    };
    ready.send(Ok(Opened { local, listen, block: block.clone() })).ok();
    let mut closed_elsewhere = false;
    loop {
        let accepted = async {
            match &listener {
                Some(listener) => listener.accept().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = stopped.recv_async() => break,
            accepted = accepted => {
                // A failed connection is reset; the forward goes on.
                let Ok((tcp, _peer)) = accepted else { continue };
                let (client, endpoint, session, block) = (client.clone(), endpoint.clone(), session.clone(), block.clone());
                tokio::task::spawn_local(async move {
                    let reference = format!("conn:{}", upload::new_uuid()?);
                    relay(&client, &endpoint, &session, &block, &reference, tcp).await
                });
            }
            event = events.recv_async() => {
                let Ok(event) = event else { break };
                match event["type"].as_str() {
                    Some("reconnected") if event["resumed"] != true => {
                        // A new control connection: attach and own the block again.
                        client.call("session.attach", json!({"session_id": session})).await.ok();
                        client.call("session.own_block", json!({"session_id": session, "block_ids": [block], "mode": "take"})).await.ok();
                        continue;
                    }
                    Some("event") => {}
                    _ => continue,
                }
                let envelope = &event["payload"];
                if envelope["type"] != "session_event" { continue; }
                let inner = &envelope["payload"]["event"];
                if inner["payload"]["block_id"] != block.as_str() { continue; }
                match inner["type"].as_str() {
                    Some("block_closed") => { closed_elsewhere = true; break; }
                    Some("block_event") if inner["payload"]["name"] == "connection_requested" => {
                        let request = &inner["payload"]["payload"];
                        let me = client.id();
                        if !me.is_empty() && request["client"].as_str().is_some_and(|c| c != me) { continue; }
                        let Some(reference) = request["ref"].as_str().map(str::to_owned) else { continue };
                        let (client, endpoint, session, block) = (client.clone(), endpoint.clone(), session.clone(), block.clone());
                        let (host, port) = (spec.host.clone(), spec.host_port);
                        tokio::task::spawn_local(async move {
                            match tokio::net::TcpStream::connect((host.as_str(), port)).await {
                                Ok(tcp) => relay(&client, &endpoint, &session, &block, &reference, tcp).await,
                                // The server resets the connection it accepted.
                                Err(_) => refuse(&client, &endpoint, &session, &block, &reference).await,
                            }
                        });
                    }
                    _ => {}
                }
            }
        }
    }
    if !closed_elsewhere {
        client.call("block.close", json!({"session_id": session, "block_id": block})).await.ok();
    }
    Ok(())
}

async fn stream(client: &Client, endpoint: &str, session: &str, block: &str, reference: &str) -> Result<transport::Socket> {
    let ticket = client.call("data.connect", json!({
        "session_id": session, "block_id": block,
        "data_ref_id": reference, "subprotocol": upload::PROTOCOL,
    })).await?;
    let mut socket = transport::socket(endpoint, "/api/data", upload::PROTOCOL).await?;
    wire::handshake(&mut socket, ticket["ticket"].as_str().context("data ticket")?).await?;
    Ok(socket)
}

/// Open the stream of a connection this side cannot serve, and reset it.
async fn refuse(client: &Client, endpoint: &str, session: &str, block: &str, reference: &str) -> Result<()> {
    let mut socket = stream(client, endpoint, session, block, reference).await?;
    socket.send(Message::Close(Some(CloseFrame { code: CloseCode::Error, reason: "connection refused".into() }))).await.ok();
    Ok(())
}

/// One connection: its bytes go both ways over one stream of the block.
async fn relay(client: &Client, endpoint: &str, session: &str, block: &str, reference: &str,
               tcp: tokio::net::TcpStream) -> Result<()> {
    let socket = stream(client, endpoint, session, block, reference).await?;
    let (mut sink, mut stream) = socket.split();
    let (mut reader, mut writer) = tcp.into_split();
    let up = async move {
        let mut buffer = vec![0; CHUNK];
        loop {
            let n = reader.read(&mut buffer).await?;
            // An empty message: the local side closed its half.
            sink.send(Message::Binary(buffer[..n].to_vec().into())).await?;
            if n == 0 {
                return Ok::<_, anyhow::Error>(sink);
            }
        }
    };
    let down = async move {
        while let Some(message) = stream.next().await {
            match message? {
                Message::Binary(bytes) if bytes.is_empty() => writer.shutdown().await?,
                Message::Binary(bytes) => writer.write_all(&bytes).await?,
                Message::Close(_) => break,
                _ => {}
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    let mut up = std::pin::pin!(up);
    let mut down = std::pin::pin!(down);
    let mut sent_all = None;
    loop {
        tokio::select! {
            result = &mut up, if sent_all.is_none() => sent_all = Some(result?),
            // The server ends the stream: the connection is over.
            result = &mut down => return result,
        }
    }
}

/// A forward of a session as the server reports it (`rex forward ls`).
pub fn describe(block: &Value, info: &Value) -> Value {
    let dial = info["flavor"] == "dial";
    json!({
        "block_id": block["block_id"],
        "direction": if dial { "local" } else { "remote" },
        "listen": if dial { &info["client_address"] } else { &info["address"] },
        "connect": if dial { &info["target"] } else { &info["client_target"] },
        "owner": block["owner"],
        "state": if block["owner"].is_string() { "active" } else { "orphaned" },
        "connections": info["connections"],
        "label": block["label"],
    })
}

#[cfg(test)]
mod tests {
    use super::Spec;
    #[test]
    fn specs_as_ssh_takes_them() {
        let s = Spec::parse(false, "8080:localhost:3000").unwrap();
        assert_eq!((s.bind.as_str(), s.port, s.host.as_str(), s.host_port), ("127.0.0.1", 8080, "localhost", 3000));
        let s = Spec::parse(true, "*:0:web:80").unwrap();
        assert_eq!((s.bind.as_str(), s.port, s.host.as_str(), s.host_port, s.remote), ("0.0.0.0", 0, "web", 80, true));
        let s = Spec::parse(false, "5432").unwrap();
        assert_eq!((s.port, s.host.as_str(), s.host_port), (5432, "localhost", 5432));
        let s = Spec::parse(false, "[::1]:9000:[fe80::1]:22").unwrap();
        assert_eq!((s.bind.as_str(), s.host.as_str()), ("::1", "fe80::1"));
        assert!(Spec::parse(false, "x:y").is_err());
        assert!(Spec::parse(false, "1:host:0").is_err());
    }
}
