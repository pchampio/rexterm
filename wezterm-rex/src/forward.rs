//! A port of this machine forwarded to a target that the Rex server reaches,
//! like `ssh -L` and `rex forward open`: a dial tunnel block
//! (`com.superlogical.tunnel.dial`) on the server, and a listener here. Each
//! accepted connection opens one `rex.data.bytes.v1` stream on the block (ref
//! `conn:<name>`); an empty message is a half-close, a close with an error
//! resets the connection.
//!
//! The forward has its own control connection, attached to the session of the
//! block: the GUI's connection attaches one session at a time and switches,
//! and only the block's owner may open its streams.
use crate::{transport, transport::Client, upload, wire};
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;

const CHUNK: usize = 64 * 1024;

/// A running forward. Dropping it closes the listener and the block.
pub struct Forward {
    /// Where it listens on this machine.
    pub local: SocketAddr,
    /// host:port, as the server connects to it.
    pub target: String,
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
    let (ready_tx, ready_rx) = flume::bounded::<Result<SocketAddr>>(1);
    let (stop, stopped) = flume::bounded::<()>(1);
    let alive = Arc::new(AtomicBool::new(true));
    let running = alive.clone();
    let target = format!("{host}:{port}");
    std::thread::Builder::new().name("rex-forward".into()).spawn(move || {
        let ready = ready_tx.clone();
        let result = (move || -> Result<()> {
            let ready_tx = ready;
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            let local = tokio::task::LocalSet::new();
            local.block_on(&runtime, async move {
                let listener = match tokio::net::TcpListener::bind(("127.0.0.1", preferred)).await {
                    Ok(listener) => listener,
                    Err(_) => tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?,
                };
                let address = listener.local_addr()?;
                let (client, events) = Client::connect(endpoint.clone()).await?;
                // Unread events would stop the control connection.
                tokio::task::spawn_local(async move { while events.recv_async().await.is_ok() {} });
                client.call("session.attach", json!({"session_id": session})).await?;
                let created = client.call("session.new_block", json!({
                    "session_id": session,
                    "flavor": "com.superlogical.tunnel.dial",
                    "label": format!("forward {address} → {host}:{port}"),
                    "options": {"host": host, "port": port, "client_address": address.to_string()},
                })).await?;
                let block = created["block_id"].as_str().context("tunnel block ID")?.to_owned();
                ready_tx.send(Ok(address)).ok();
                loop {
                    tokio::select! {
                        _ = stopped.recv_async() => break,
                        accepted = listener.accept() => {
                            let Ok((tcp, peer)) = accepted else { continue };
                            let (client, endpoint, session, block) = (client.clone(), endpoint.clone(), session.clone(), block.clone());
                            // A failed connection is reset; the forward goes on.
                            let _ = peer;
                            tokio::task::spawn_local(async move {
                                relay(&client, &endpoint, &session, &block, tcp).await.ok();
                            });
                        }
                    }
                }
                client.call("block.close", json!({"session_id": session, "block_id": block})).await.ok();
                Ok(())
            })
        })();
        running.store(false, Ordering::Relaxed);
        if let Err(error) = result {
            ready_tx.send(Err(error)).ok();
        }
    })?;
    let local = ready_rx.recv().context("Rex forward thread ended")??;
    Ok(Forward { local, target, alive, stop })
}

/// One connection: its bytes go both ways over one stream of the block.
async fn relay(client: &Client, endpoint: &str, session: &str, block: &str, tcp: tokio::net::TcpStream) -> Result<()> {
    let name = upload::new_uuid()?;
    let ticket = client.call("data.connect", json!({
        "session_id": session, "block_id": block,
        "data_ref_id": format!("conn:{name}"), "subprotocol": upload::PROTOCOL,
    })).await?;
    let mut socket = transport::socket(endpoint, "/api/data", upload::PROTOCOL).await?;
    wire::handshake(&mut socket, ticket["ticket"].as_str().context("data ticket")?).await?;
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
