//! Multiplex Rex sockets over an explicitly started shell attachment.
//! This carries opaque WebSocket bytes, not a terminal screen or text UI.
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::{Arc,Mutex,LazyLock,atomic::{AtomicU64,AtomicBool,Ordering}}};
use tokio::io::{AsyncReadExt,AsyncWriteExt};

type SendFrame = Arc<dyn Fn(Value)->Result<()>+Send+Sync>;
struct Bridge {
    send: SendFrame,
    output: Mutex<()>,
    closed: AtomicBool,
    next: AtomicU64,
    channels: Mutex<HashMap<u64,flume::Sender<Option<Vec<u8>>>>>,
}
impl Bridge {
    fn send_frame(&self,message:Value)->Result<()> {
        let _output=self.output.lock().unwrap();
        if self.closed.load(Ordering::Acquire) {return Ok(());}
        (self.send)(message)
    }
}
static BRIDGES: LazyLock<Mutex<HashMap<String,Arc<Bridge>>>> = LazyLock::new(||Mutex::new(HashMap::new()));

pub fn register(token: String, send: SendFrame) -> Result<()> {
    ensure!(token.len()==32 && token.bytes().all(|b|b.is_ascii_hexdigit()),"invalid Rex bridge token");
    let mut all=BRIDGES.lock().unwrap();
    ensure!(!all.contains_key(&token),"Rex bridge already exists");
    all.insert(token,Arc::new(Bridge{send,output:Mutex::new(()),closed:AtomicBool::new(false),next:AtomicU64::new(1),channels:Mutex::new(HashMap::new())}));
    Ok(())
}
pub fn receive(token:&str, message:Value) -> Result<()> {
    let bridge=BRIDGES.lock().unwrap().get(token).cloned().context("detached Rex bridge")?;
    let id=message["id"].as_u64().context("Rex bridge channel")?;
    let channel=bridge.channels.lock().unwrap().get(&id).cloned();
    if let Some(channel)=channel {
        let data=match message["type"].as_str() {
            Some("data")=> {
                let encoded=message["data"].as_str().context("Rex bridge bytes")?;
                ensure!(encoded.len()<=65536,"Rex bridge frame too large");
                Some(STANDARD.decode(encoded)?)
            },
            Some("closed")=>None,
            Some("opened")=>return Ok(()),
            _=>anyhow::bail!("invalid Rex bridge message"),
        };
        match channel.try_send(data) {
            Ok(()) | Err(flume::TrySendError::Disconnected(_)) => {},
            Err(flume::TrySendError::Full(_)) => anyhow::bail!("Rex bridge receiver stalled: queue full"),
        }
    }
    Ok(())
}
pub fn close(token:&str) {
    if let Some(bridge)=BRIDGES.lock().unwrap().remove(token) {
        // Serialize the final packet with every channel writer. No delayed
        // close/data packet may become shell input after raw mode ends.
        let _output=bridge.output.lock().unwrap();
        bridge.closed.store(true,Ordering::Release);
        (bridge.send)(json!({"type":"detach"})).ok();
        bridge.channels.lock().unwrap().clear();
    }
}
pub async fn connect(token:&str)->Result<tokio::io::DuplexStream> {
    let bridge=BRIDGES.lock().unwrap().get(token).cloned().context("Rex attachment is no longer active")?;
    let id=bridge.next.fetch_add(1,Ordering::Relaxed);
    let (tx,rx)=flume::bounded(128);
    {
        let mut channels=bridge.channels.lock().unwrap();
        ensure!(channels.len()<256,"too many Rex bridge sockets");
        channels.insert(id,tx);
    }
    if let Err(e)=bridge.send_frame(json!({"type":"open","id":id})) {
        bridge.channels.lock().unwrap().remove(&id);return Err(e);
    }
    let (local,remote)=tokio::io::duplex(256*1024);
    tokio::spawn(async move {
        let (mut reader,mut writer)=tokio::io::split(remote);
        let sender=bridge.clone();
        let outgoing=async {
            let mut bytes=vec![0;12*1024];
            loop {
                let n=reader.read(&mut bytes).await?;
                if n==0 {return Ok::<_,anyhow::Error>(());}
                sender.send_frame(json!({"type":"data","id":id,"data":STANDARD.encode(&bytes[..n])}))?;
            }
        };
        let incoming=async {
            while let Ok(Some(bytes))=rx.recv_async().await {writer.write_all(&bytes).await?;}
            Ok::<_,anyhow::Error>(())
        };
        tokio::select!{_=outgoing=>{},_=incoming=>{}}
        bridge.channels.lock().unwrap().remove(&id);
        bridge.send_frame(json!({"type":"close","id":id})).ok();
    });
    Ok(local)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn closed_receiver_is_normal_but_backpressure_is_an_error() {
        let token="b948ce02f43346eebeea000000000002";
        register(token.into(),Arc::new(|_|Ok(()))).unwrap();
        let bridge=BRIDGES.lock().unwrap().get(token).unwrap().clone();
        let (tx,rx)=flume::bounded(1);
        bridge.channels.lock().unwrap().insert(1,tx);
        let frame=json!({"type":"data","id":1,"data":STANDARD.encode(b"x")});
        receive(token,frame.clone()).unwrap();
        assert!(receive(token,frame.clone()).unwrap_err().to_string().contains("queue full"));
        drop(rx);
        receive(token,frame).unwrap();
        close(token);
    }
    #[test]
    fn detach_is_final_even_when_channel_tasks_are_pending() {
        let rt=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let token="b948ce02f43346eebeea000000000001";
            let frames=Arc::new(Mutex::new(Vec::new()));let output=frames.clone();
            register(token.into(),Arc::new(move |message|{output.lock().unwrap().push(message);Ok(())})).unwrap();
            let mut socket=connect(token).await.unwrap();
            socket.write_all(b"pending shell input").await.unwrap();
            receive(token,json!({"type":"data","id":1,"data":STANDARD.encode(b"remote output")})).unwrap();
            let mut read=[0;13];socket.read_exact(&mut read).await.unwrap();assert_eq!(&read,b"remote output");
            close(token);drop(socket);
            tokio::task::yield_now().await;
            let frames=frames.lock().unwrap();
            assert_eq!(frames.last().unwrap()["type"],"detach");
            assert_eq!(frames.iter().filter(|m|m["type"]=="detach").count(),1);
            assert!(connect(token).await.is_err());
        });
    }
}
