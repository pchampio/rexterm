// Handshake adapted from Rex Rune (MIT); see THIRD-PARTY.txt.
use crate::transport::Socket;
use anyhow::{Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

pub const PROTOCOL: &str = "rex.data.terminal.v1";

#[derive(Debug)]
pub struct HandshakeRejected(pub u8);
impl std::fmt::Display for HandshakeRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Rex rejected data handshake (reason {})", self.0)
    }
}
impl std::error::Error for HandshakeRejected {}

#[derive(Clone)]
pub struct Welcome {
    pub token: [u8; 16],
    pub nonce: u64,
    pub retention_ms: u32,
}

pub async fn handshake(socket: &mut Socket, ticket: &str) -> Result<Welcome> {
    let ticket = URL_SAFE_NO_PAD.decode(ticket)?;
    ensure!(ticket.len() == 16, "invalid data ticket length");
    let mut message = vec![1];
    message.extend(ticket);
    exchange(socket, message).await
}

pub async fn resume(socket: &mut Socket, token: &[u8; 16], offset: u64) -> Result<Welcome> {
    let mut message = vec![2];
    message.extend(token);
    message.extend(offset.to_le_bytes());
    exchange(socket, message).await
}

async fn exchange(socket: &mut Socket, message: Vec<u8>) -> Result<Welcome> {
    socket.send(Message::Binary(message.into())).await?;
    let bytes =
        tokio::time::timeout(std::time::Duration::from_secs(8), next_binary(socket)).await??;
    if bytes.first() == Some(&0x82) {
        return Err(HandshakeRejected(*bytes.get(1).unwrap_or(&0)).into());
    }
    ensure!(
        bytes.len() == 32 && bytes[0] == 0x81,
        "invalid data WELCOME"
    );
    let nonce = u64::from_le_bytes(bytes[24..32].try_into()?);
    ensure!(nonce != 0, "invalid channel nonce");
    Ok(Welcome {
        token: bytes[1..17].try_into()?,
        nonce,
        retention_ms: u32::from_le_bytes(bytes[20..24].try_into()?),
    })
}

#[derive(Debug)]
pub struct DataClosed {
    pub code: Option<u16>,
    pub reason: String,
}
impl std::fmt::Display for DataClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "data connection closed ({:?}): {}",
            self.code, self.reason
        )
    }
}
impl std::error::Error for DataClosed {}

pub async fn next_binary(socket: &mut Socket) -> Result<Vec<u8>> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Binary(bytes))) => return Ok(bytes.to_vec()),
            Some(Ok(Message::Close(frame))) => {
                return Err(DataClosed {
                    code: frame.as_ref().map(|f| f.code.into()),
                    reason: frame.map(|f| f.reason.to_string()).unwrap_or_default(),
                }
                .into());
            }
            Some(Ok(_)) => socket.flush().await?,
            Some(Err(error)) => return Err(error.into()),
            None => bail!("data connection ended"),
        }
    }
}

use serde_json::Value;
#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Output(Vec<u8>),
    Metadata(Value),
}

/// Retains partial escape prefixes across frames and reconnects. Only a Rex
/// APC with this channel's nonce is interpreted as transport metadata.
pub struct ApcParser {
    prefix: Vec<u8>,
    pending: Vec<u8>,
}
impl ApcParser {
    pub fn new(nonce: u64) -> Self {
        Self {
            prefix: format!("\x1b_rex1;{nonce};").into_bytes(),
            pending: Vec::new(),
        }
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Part>> {
        let mut parts = Vec::new();
        let mut output = Vec::new();
        for &byte in bytes {
            self.pending.push(byte);
            loop {
                if self.pending.starts_with(&self.prefix) {
                    ensure!(self.pending.len() <= 1024 * 1024, "Rex APC exceeds 1 MiB");
                    if self.pending.ends_with(b"\x1b\\") {
                        if !output.is_empty() {
                            parts.push(Part::Output(std::mem::take(&mut output)));
                        }
                        let json = &self.pending[self.prefix.len()..self.pending.len() - 2];
                        parts.push(Part::Metadata(serde_json::from_slice(json)?));
                        self.pending.clear();
                    }
                    break;
                }
                if self.prefix.starts_with(&self.pending) {
                    break;
                }
                output.push(self.pending.remove(0));
            }
        }
        if !output.is_empty() {
            parts.push(Part::Output(output));
        }
        Ok(parts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_at_every_boundary_and_foreign_apc() {
        let input =
            b"abc\x1b_rex1;42;{\"type\":\"resize\",\"cols\":80}\x1b\\def\x1b_rex1;99;foreign\x1b\\";
        for cut in 0..=input.len() {
            let mut p = ApcParser::new(42);
            let mut parts = p.feed(&input[..cut]).unwrap();
            parts.extend(p.feed(&input[cut..]).unwrap());
            let mut text = Vec::new();
            let mut metadata = Vec::new();
            for part in parts {
                match part {
                    Part::Output(b) => text.extend(b),
                    Part::Metadata(v) => metadata.push(v),
                }
            }
            assert_eq!(text, b"abcdef\x1b_rex1;99;foreign\x1b\\");
            assert_eq!(
                metadata,
                vec![serde_json::json!({"type":"resize","cols":80})]
            );
        }
    }
    #[test]
    fn bounded_and_invalid_metadata_fails() {
        let mut p = ApcParser::new(42);
        assert!(p.feed(b"\x1b_rex1;42;not-json\x1b\\").is_err());
        let mut p = ApcParser::new(42);
        p.feed(b"\x1b_rex1;42;").unwrap();
        assert!(p.feed(&vec![b'x'; 1024 * 1024]).is_err());
    }
}
