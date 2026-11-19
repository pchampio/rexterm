//! OSC 7377: audio streams played on the host that runs WezTerm.
//!
//! A program in the terminal (typically over SSH, also inside tmux with
//! passthrough) sends raw PCM inline; each stream feeds one player process
//! (paplay, pw-cat or aplay, or the `audio_player` setting) through its stdin.
//!
//! ```text
//! ESC ] 7377 ; a=o ; i=ID ; r=RATE ; c=CHANNELS ; f=s16le ST   open
//! ESC ] 7377 ; a=d ; i=ID ; BASE64-PCM ST                       data
//! ESC ] 7377 ; a=e ; i=ID ST                                    end (play the rest)
//! ESC ] 7377 ; a=x ; i=ID ST                                    stop now
//! ```
//!
//! Writing to a player never blocks the terminal: a stream that falls more
//! than about two seconds behind drops data.
use std::collections::HashMap;
use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};

pub const OSC_CODE: &[u8] = b"7377";
/// Chunks queued for a player; at 20 ms per chunk this is ~2.5 s.
const QUEUE_CHUNKS: usize = 128;
/// Upper bound for one decoded chunk (1 s of 8 channel 96 kHz s16).
const MAX_CHUNK: usize = 96_000 * 8 * 2;

struct Stream {
    child: Child,
    tx: Option<SyncSender<Vec<u8>>>,
    dropped: usize,
}

#[derive(Default)]
pub struct AudioStreams {
    streams: HashMap<u32, Stream>,
}

impl std::fmt::Debug for AudioStreams {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("AudioStreams")
            .field("streams", &self.streams.keys().collect::<Vec<_>>())
            .finish()
    }
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

fn player_command(configured: &[String], rate: u32, channels: u32) -> Option<Vec<String>> {
    let template: Vec<String> = if !configured.is_empty() {
        configured.to_vec()
    } else if which("paplay") {
        "paplay --raw --format={format} --rate={rate} --channels={channels}"
            .split(' ')
            .map(String::from)
            .collect()
    } else if which("pw-cat") {
        "pw-cat --playback --raw --format s16 --rate {rate} --channels {channels} -"
            .split(' ')
            .map(String::from)
            .collect()
    } else if which("aplay") {
        "aplay -q -t raw -f S16_LE -r {rate} -c {channels} -"
            .split(' ')
            .map(String::from)
            .collect()
    } else {
        return None;
    };
    Some(
        template
            .into_iter()
            .map(|arg| {
                arg.replace("{rate}", &rate.to_string())
                    .replace("{channels}", &channels.to_string())
                    .replace("{format}", "s16le")
            })
            .collect(),
    )
}

fn decode_base64(data: &[u8]) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()
        .filter(|d| d.len() <= MAX_CHUNK)
}

impl AudioStreams {
    /// Handles one OSC 7377 (`params` excludes the code itself).
    pub fn handle(&mut self, player: &[String], params: &[Vec<u8>]) {
        let mut fields = HashMap::new();
        let mut payload: Option<&[u8]> = None;
        for p in params {
            match p.iter().position(|&b| b == b'=') {
                Some(eq) if eq <= 2 => {
                    fields.insert(&p[..eq], &p[eq + 1..]);
                }
                _ => payload = Some(p),
            }
        }
        let text = |key: &[u8]| {
            fields
                .get(key)
                .and_then(|v| std::str::from_utf8(v).ok())
                .map(str::to_string)
        };
        let number = |key: &[u8]| text(key).and_then(|v| v.parse::<u32>().ok());
        let Some(id) = number(b"i") else { return };
        match text(b"a").as_deref() {
            Some("o") => {
                self.stop(id);
                let rate = number(b"r").unwrap_or(48000).clamp(8000, 192000);
                let channels = number(b"c").unwrap_or(2).clamp(1, 8);
                if text(b"f").map_or(false, |f| f != "s16le") {
                    log::warn!("OSC 7377: only s16le audio is supported");
                    return;
                }
                if self.streams.len() >= 8 {
                    log::warn!("OSC 7377: too many audio streams");
                    return;
                }
                let Some(argv) = player_command(player, rate, channels) else {
                    log::warn!("OSC 7377: no audio player (paplay, pw-cat or aplay) found");
                    return;
                };
                match Command::new(&argv[0])
                    .args(&argv[1..])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                {
                    Ok(mut child) => {
                        let mut stdin = child.stdin.take().unwrap();
                        let (tx, rx) = sync_channel::<Vec<u8>>(QUEUE_CHUNKS);
                        let _ = std::thread::Builder::new()
                            .name(format!("osc7377-audio-{id}"))
                            .spawn(move || {
                                while let Ok(chunk) = rx.recv() {
                                    if stdin.write_all(&chunk).is_err() {
                                        break;
                                    }
                                }
                                // Dropping stdin lets the player finish.
                            });
                        self.streams.insert(
                            id,
                            Stream {
                                child,
                                tx: Some(tx),
                                dropped: 0,
                            },
                        );
                    }
                    Err(err) => log::warn!("OSC 7377: cannot start {:?}: {err:#}", argv),
                }
            }
            Some("d") => {
                let data = payload.and_then(decode_base64);
                let (Some(data), Some(stream)) = (data, self.streams.get_mut(&id)) else {
                    return;
                };
                if let Some(tx) = &stream.tx {
                    match tx.try_send(data) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) => {
                            stream.dropped += 1;
                            if stream.dropped == 1 {
                                log::warn!("OSC 7377: audio stream {id} is behind; dropping data");
                            }
                        }
                        Err(TrySendError::Disconnected(_)) => {
                            self.stop(id);
                        }
                    }
                }
            }
            Some("e") => {
                if let Some(mut stream) = self.streams.remove(&id) {
                    // Closing the queue ends the writer, then the player's
                    // stdin: it plays what it has and exits.
                    stream.tx.take();
                    std::thread::spawn(move || {
                        let _ = stream.child.wait();
                    });
                }
            }
            Some("x") => self.stop(id),
            _ => {}
        }
    }

    fn stop(&mut self, id: u32) {
        if let Some(mut stream) = self.streams.remove(&id) {
            stream.tx.take();
            let _ = stream.child.kill();
            std::thread::spawn(move || {
                let _ = stream.child.wait();
            });
        }
    }

    pub fn stop_all(&mut self) {
        let ids: Vec<u32> = self.streams.keys().copied().collect();
        for id in ids {
            self.stop(id);
        }
    }
}

impl Drop for AudioStreams {
    fn drop(&mut self) {
        self.stop_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn osc(s: &str) -> Vec<Vec<u8>> {
        s.split(';').map(|p| p.as_bytes().to_vec()).collect()
    }

    #[test]
    fn stream_reaches_player() {
        let dir = std::env::temp_dir().join(format!("osc7377-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.raw");
        let player = vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("cat > {}", out.display()),
        ];
        let mut audio = AudioStreams::default();
        audio.handle(&player, &osc("a=o;i=3;r=48000;c=2;f=s16le"));
        audio.handle(&player, &osc("a=d;i=3;AAECAwQFBgc="));
        audio.handle(&player, &osc("a=d;i=4;AAECAwQFBgc="));
        audio.handle(&player, &osc("a=e;i=3"));
        for _ in 0..50 {
            if std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0) == 8 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(std::fs::read(&out).unwrap(), (0u8..8).collect::<Vec<_>>());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn player_placeholders() {
        let argv = player_command(&["p".into(), "{rate}/{channels}/{format}".into()], 44100, 1);
        assert_eq!(argv.unwrap()[1], "44100/1/s16le");
    }
}
