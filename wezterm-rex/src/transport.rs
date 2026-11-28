use anyhow::{Context, Result, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};

/// The newest Rex API revision this client speaks. Revisions 15 to 19 add
/// methods and events (ports_changed, event_dropped, download_begin, prompts)
/// and change no shape that this client reads. From 17 on, the server asks
/// every app to show rex.ui prompts: the control loop declines them.
pub const API_REVISION: u64 = 19;

#[derive(Debug)]
pub struct RpcError(pub Value);
impl std::fmt::Display for RpcError {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {write!(f,"Rex: {}",self.0)}
}
impl std::error::Error for RpcError {}

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
pub type Socket = WebSocketStream<MaybeTlsStream<Box<dyn Io>>>;

pub async fn socket(endpoint: &str, path: &str, protocol: &str) -> Result<Socket> {
    // Work both as a standalone crate and inside a GUI that may have already
    // selected its TLS provider through another dependency.
    let _ = rustls::crypto::ring::default_provider().install_default();
    tokio::time::timeout(Duration::from_secs(10), async {
        let (mut url, stream): (_, Box<dyn Io>) =
            if let Some(token) = endpoint.strip_prefix("bridge://") {
                (url::Url::parse("ws://localhost")?, Box::new(crate::bridge::connect(token).await?))
            } else if let Some(path) = endpoint.strip_prefix("unix://") {
                (
                    url::Url::parse("ws://localhost")?,
                    Box::new(tokio::net::UnixStream::connect(path).await?),
                )
            } else {
                let mut url = url::Url::parse(endpoint)?;
                let scheme = match url.scheme() {
                    "https" | "wss" => "wss",
                    "http" | "ws" => "ws",
                    _ => bail!("Rex endpoint must be unix://, https:// or http://"),
                };
                url.set_scheme(scheme)
                    .map_err(|_| anyhow::anyhow!("invalid endpoint"))?;
                let stream = tokio::net::TcpStream::connect((
                    url.host_str().context("missing host")?,
                    url.port_or_known_default().context("missing port")?,
                ))
                .await?;
                stream.set_nodelay(true)?;
                (url, Box::new(stream))
            };
        url.set_path(path);
        url.set_query(None);
        let mut request = url.as_str().into_client_request()?;
        request
            .headers_mut()
            .insert("Sec-WebSocket-Protocol", protocol.parse()?);
        let (socket, response) = tokio_tungstenite::client_async_tls(request, stream).await?;
        ensure!(
            response
                .headers()
                .get("Sec-WebSocket-Protocol")
                .and_then(|v| v.to_str().ok())
                == Some(protocol),
            "Rex subprotocol not negotiated"
        );
        Ok(socket)
    })
    .await
    .context("Rex connection timed out")?
}

#[derive(Debug, Clone)]
pub struct Reply {
    pub payload: Value,
    pub seq: u64,
}
struct Request {
    method: String,
    payload: Value,
    reply: flume::Sender<Result<Reply>>,
}
struct Pending {
    reply: flume::Sender<Result<Reply>>,
    deadline: Instant,
}
#[derive(Clone)]
pub struct Client {
    requests: flume::Sender<Request>,
    identity: std::sync::Arc<std::sync::Mutex<String>>,
    connected: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl Client {
    /// Starts one I/O thread. All GUI requests await channels, never blocking its event loop.
    pub async fn connect(endpoint: String) -> Result<(Self, flume::Receiver<Value>)> {
        let (tx, rx) = flume::bounded::<Request>(256);
        let (events_tx, events_rx) = flume::bounded(4096);
        let (ready_tx, ready_rx) = flume::bounded(1);
        let connected = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let online = connected.clone();
        let identity = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let id = identity.clone();
        std::thread::Builder::new().name("rex-control".into()).spawn(move || {
            let result=(|| -> Result<()> {
                tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
                    let mut token:Option<String>=None;let mut first=true;let mut next_id=0u64;
                    loop {
                        let connection=hello(&endpoint,token.as_deref()).await;
                        let (mut ws,welcome)=match connection {
                            Ok(c)=>c,
                            Err(e) if first=>return Err(e),
                            Err(e)=>{
                                // Expired client tokens require a fresh client. The domain will
                                // attach its session and reconcile its view before accepting input.
                                if e.to_string()=="Rex rejected hello" {token=None;}
                                if rx.is_disconnected() || events_tx.is_disconnected(){return Ok(())}
                                tokio::time::sleep(Duration::from_millis(500)).await;continue
                            }
                        };
                        *id.lock().unwrap()=welcome["client"]["id"].as_str().unwrap_or("").to_owned();
                        token=welcome["client"]["reconnect_token"].as_str().map(str::to_owned);
                        let max_message=welcome["limits"]["max_message_bytes"].as_u64().context("missing Rex message limit")? as usize;
                        online.store(true,std::sync::atomic::Ordering::Release);
                        if first {ready_tx.send(Ok(())).ok();first=false;}
                        else {events_tx.try_send(json!({"type":"reconnected","resumed":welcome["client"]["resumed"]})).context("Rex event consumer unavailable")?;}
                        let mut pending:HashMap<String,Pending>=HashMap::new();
                        let mut expiry=tokio::time::interval(Duration::from_millis(100));
                        let served:Result<()> = async {
                            loop {tokio::select! {
                                _=expiry.tick()=>{
                                    pending.retain(|_,p|{
                                        if Instant::now()>=p.deadline {p.reply.send(Err(anyhow::anyhow!("Rex request timed out; outcome unknown, not retried"))).ok();false}
                                        else {!p.reply.is_disconnected()}
                                    });
                                },
                                request=rx.recv_async(), if pending.len()<256=>{
                                    let Ok(request)=request else{return Ok(())};
                                    next_id+=1;let id=next_id.to_string();
                                    let text=json!({"type":"request","id":id,"method":request.method,"payload":request.payload}).to_string();
                                    if text.len()>max_message {request.reply.send(Err(anyhow::anyhow!("Rex request exceeds negotiated message limit"))).ok();continue}
                                    pending.insert(id,Pending{reply:request.reply,deadline:Instant::now()+Duration::from_secs(10)});
                                    tokio::time::timeout(Duration::from_secs(10),ws.send(Message::text(text))).await??;
                                },
                                message=ws.next()=>match message.context("Rex disconnected")?? {
                                    Message::Text(text)=>{
                                        let value:Value=serde_json::from_str(&text)?;
                                        if value["type"]=="response" {
                                            if let Some(p)=value["id"].as_str().and_then(|id|pending.remove(id)) {
                                                let result=if value.get("error").is_some_and(|e|!e.is_null()){Err(anyhow::Error::new(RpcError(value["error"].clone())))}
                                                else {Ok(Reply{payload:value["payload"].clone(),seq:value["seq"].as_u64().context("Rex reply missing sequence")?})};
                                                p.reply.send(result).ok();
                                            }
                                        } else if value["type"]=="event" {
                                            // A prompt (rex.ui) or a call on another server: this
                                            // client shows no prompts and makes no such calls. Say
                                            // so at once, so the action does not wait for its
                                            // timeout, and the other apps keep the prompt.
                                            let kind=value["payload"]["type"].as_str().unwrap_or("");
                                            if let ("client_prompt_requested"|"client_call_requested",Some(call))=(kind,value["payload"]["payload"]["call_id"].as_str()) {
                                                next_id+=1;
                                                let reply=json!({"type":"request","id":next_id.to_string(),"method":"client.reply","payload":{"call_id":call,
                                                    "error":{"code":"not_supported","message":"WezTerm Rex does not show Rex prompts or call other servers"}}});
                                                tokio::time::timeout(Duration::from_secs(10),ws.send(Message::text(reply.to_string()))).await??;
                                                continue;
                                            }
                                            events_tx.try_send(value).context("Rex event consumer stalled or closed")?;
                                        }
                                    },
                                    Message::Close(_)=>bail!("Rex disconnected"),
                                    _=>ws.flush().await?,
                                }
                            }}
                        }.await;
                        online.store(false,std::sync::atomic::Ordering::Release);
                        for (_,p) in pending {p.reply.send(Err(anyhow::anyhow!("Rex disconnected; request outcome unknown, not retried"))).ok();}
                        // Nothing queued before a disconnect is replayed after it.
                        for r in rx.try_iter(){r.reply.send(Err(anyhow::anyhow!("Rex disconnected before request was sent"))).ok();}
                        if served.is_ok() || rx.is_disconnected() || events_tx.is_disconnected(){return Ok(())}
                        events_tx.try_send(json!({"type":"disconnected","message":served.unwrap_err().to_string()})).context("Rex event consumer unavailable")?;
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                })
            })();
            online.store(false,std::sync::atomic::Ordering::Release);
            if let Err(error)=result {ready_tx.send(Err(anyhow::anyhow!("{error:#}"))).ok();events_tx.try_send(json!({"type":"disconnected","message":format!("{error:#}")})).ok();}
        })?;
        ready_rx.recv_async().await??;
        Ok((
            Self {
                requests: tx,
                identity,
                connected,
            },
            events_rx,
        ))
    }
    pub fn id(&self) -> String { self.identity.lock().unwrap().clone() }
    pub fn is_connected(&self) -> bool {
        self.connected.load(std::sync::atomic::Ordering::Acquire)
    }
    /// Enqueue synchronously to preserve key/mouse order; await acknowledgement elsewhere.
    pub fn enqueue(
        &self,
        method: &str,
        payload: Value,
    ) -> Result<impl std::future::Future<Output = Result<Value>> + Send + 'static> {
        ensure!(
            self.is_connected(),
            "Rex control is disconnected; request not sent"
        );
        let (reply, rx) = flume::bounded(1);
        self.requests
            .try_send(Request {
                method: method.into(),
                payload,
                reply,
            })
            .map_err(|e| anyhow::anyhow!("Rex input queue unavailable: {e}"))?;
        Ok(async move {
            Ok(rx
                .recv_async()
                .await
                .context("Rex connection closed before acknowledgement")??
                .payload)
        })
    }
    pub async fn call(&self, method: &str, payload: Value) -> Result<Value> {
        Ok(self.call_reply(method, payload).await?.payload)
    }
    /// Bootstrap consumers must retain seq and discard older queued events.
    pub async fn call_reply(&self, method: &str, payload: Value) -> Result<Reply> {
        ensure!(
            self.is_connected(),
            "Rex control is disconnected; request not sent"
        );
        let (reply, rx) = flume::bounded(1);
        self.requests
            .send_async(Request {
                method: method.into(),
                payload,
                reply,
            })
            .await
            .context("Rex connection closed")?;
        rx.recv_async()
            .await
            .context("Rex connection closed before reply")?
    }
}

async fn hello(endpoint: &str, token: Option<&str>) -> Result<(Socket, Value)> {
    let mut ws = socket(endpoint, "/api/control", "rex.control.v1").await?;
    let mut hello = json!({"type":"hello","api_revision":API_REVISION,"api_min_revision":10,"client":{"kind":"app","platform":"linux","name":"WezTerm Rex"}});
    if let Some(token) = token {
        hello["reconnect_token"] = json!(token);
    }
    ws.send(Message::text(hello.to_string())).await?;
    let welcome = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await?
        .context("no welcome")??;
    let welcome: Value = serde_json::from_str(welcome.to_text()?)?;
    ensure!(welcome["type"] == "welcome", "Rex rejected hello");
    ensure!(
        welcome["protocol"] == "rex.control.v1",
        "unexpected Rex control protocol"
    );
    let revision = welcome["api_revision"]
        .as_u64()
        .context("missing negotiated API revision")?;
    ensure!(
        (10..=API_REVISION).contains(&revision),
        "unsupported Rex API revision {revision}"
    );
    Ok((ws, welcome))
}

/// The JSON-encoded size that one piece of input text may have. Every Rex
/// server accepts messages of at least 64 KiB (`limits.max_message_bytes`
/// is never lower), and the request around the text needs a few hundred
/// bytes, so pieces of this size always fit, whatever the server's limit.
pub const INPUT_CHUNK_JSON_BYTES: usize = 56 * 1024;

/// The bytes that `c` takes inside a JSON string, as serde_json writes it.
fn json_char_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// `text` cut into pieces that each take at most `budget` bytes once
/// JSON-encoded. A paste of newlines doubles in size when encoded, and a
/// request over the server's limit is refused, so the size is counted
/// after encoding. Pieces end on character boundaries.
pub fn json_text_chunks(text: &str, budget: usize) -> Vec<&str> {
    let mut pieces = Vec::new();
    let (mut start, mut size) = (0, 0);
    for (index, c) in text.char_indices() {
        let len = json_char_len(c);
        if size + len > budget && index > start {
            pieces.push(&text[start..index]);
            (start, size) = (index, 0);
        }
        size += len;
    }
    if start < text.len() || pieces.is_empty() {
        pieces.push(&text[start..]);
    }
    pieces
}

#[cfg(test)]
mod chunk_tests {
    use super::*;

    #[test]
    fn short_text_is_one_piece() {
        assert_eq!(json_text_chunks("ls -la\n", 100), vec!["ls -la\n"]);
        assert_eq!(json_text_chunks("", 100), vec![""]);
    }

    #[test]
    fn pieces_fit_once_encoded_and_join_back() {
        let text = "a\"b\\c\n".repeat(40_000) + &"é€😀\u{1}\t".repeat(20_000);
        let pieces = json_text_chunks(&text, INPUT_CHUNK_JSON_BYTES);
        assert!(pieces.len() > 1);
        assert_eq!(pieces.concat(), text);
        for piece in pieces {
            let encoded = serde_json::to_string(piece).unwrap();
            // The two quotes are the request's, not the budget's.
            assert!(encoded.len() - 2 <= INPUT_CHUNK_JSON_BYTES, "{}", encoded.len());
        }
    }

    #[test]
    fn newlines_count_twice() {
        let text = "\n".repeat(10);
        let pieces = json_text_chunks(&text, 8);
        assert_eq!(pieces, vec!["\n\n\n\n", "\n\n\n\n", "\n\n"]);
    }
}
