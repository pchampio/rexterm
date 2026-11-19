use anyhow::{Context, Result, ensure};
use ghostty_vt::{Terminal, snapshot};
/// Framing is independent of WebSocket boundaries. No partial restore reaches a pane.
#[derive(Default)]
pub struct SnapshotBuffer {
    bytes: Vec<u8>,
    scan: usize,
}
impl SnapshotBuffer {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Option<(Terminal, Vec<u8>)>> {
        const LIMIT: usize = 256 * 1024 * 1024;
        ensure!(
            self.bytes.len().saturating_add(bytes.len()) <= LIMIT,
            "Rex snapshot exceeds 256 MiB"
        );
        self.bytes.extend_from_slice(bytes);
        if self.scan == 0 {
            if self.bytes.len() < 10 {
                return Ok(None);
            }
            ensure!(
                &self.bytes[..10] == b"GHOSTSNP\x01\x00",
                "unsupported Rex snapshot envelope"
            );
            self.scan = 10;
        }
        while self.bytes.len() >= self.scan + 10 {
            let header = &self.bytes[self.scan..self.scan + 10];
            let tag = u16::from_le_bytes(header[..2].try_into()?);
            let len = u32::from_le_bytes(header[2..6].try_into()?) as usize;
            ensure!(
                len <= LIMIT - self.scan - 10,
                "snapshot record exceeds limit"
            );
            let end = self.scan + 10 + len;
            if end > self.bytes.len() {
                return Ok(None);
            }
            self.scan = end;
            if tag == 6 {
                let mut decoder = snapshot::Decoder::from_slice(&self.bytes[..end]);
                decoder.set_retain_continuation(true)?;
                let term = decoder.decode()
                    .context("Rex snapshot decoding")?;
                let tail = self.bytes[end..].to_vec();
                self.bytes.clear();
                self.scan = 0;
                return Ok(Some((term, tail)));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghostty_vt::{
        TerminalOptions,
        fmt::{Format, Formatter, FormatterOptions},
    };

    fn text(t: &Terminal) -> String {
        String::from_utf8(
            Formatter::new(t, FormatterOptions::new().with_format(Format::Plain))
                .unwrap()
                .format_alloc()
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }
    #[test]
    fn snapshot_retains_screens_saved_cursor_and_parser_continuation() {
        let mut source = Terminal::new(TerminalOptions {
            cols: 40,
            rows: 5,
            ..Default::default()
        })
        .unwrap();
        source.set_continuation_max_bytes(4096).unwrap();
        source.vt_write(b"PRIMARY\x1b[3;4H\x1b7\x1b[?1049hALT\x1b]2;par");
        let bytes = snapshot::encode(&source).unwrap();
        let mut buffer = SnapshotBuffer::default();
        let mut restored = None;
        for chunk in bytes.chunks(13) {
            if let Some((t, tail)) = buffer.push(chunk).unwrap() {
                assert!(tail.is_empty());
                restored = Some(t);
            }
        }
        let mut restored = restored.unwrap();
        let tail = b"tial title\x1b\\\x1b[?1049l\x1b8Z";
        source.vt_write(tail);
        restored.vt_write(tail);
        assert_eq!(text(&restored), text(&source));
        assert_eq!(restored.cursor_x().unwrap(), source.cursor_x().unwrap());
        assert_eq!(restored.cursor_y().unwrap(), source.cursor_y().unwrap());
        assert_eq!(restored.title().unwrap(), "partial title");
        assert!(text(&restored).contains("PRIMARY"));
    }
    #[test]
    fn invalid_envelope_and_record_lengths_rejected() {
        assert!(SnapshotBuffer::default().push(b"not-a-snapshot").is_err());
        let mut bytes = b"GHOSTSNP\x01\x00".to_vec();
        bytes.extend_from_slice(&[1, 0, 255, 255, 255, 255, 0, 0, 0, 0]);
        assert!(SnapshotBuffer::default().push(&bytes).is_err());
    }
    #[test]
    fn corrupt_snapshot_is_never_published_and_live_tail_is_separate() {
        let t = Terminal::new(TerminalOptions {
            cols: 40,
            rows: 5,
            ..Default::default()
        })
        .unwrap();
        let mut bytes = snapshot::encode(&t).unwrap().to_vec();
        let mut with_tail = bytes.clone();
        with_tail.extend_from_slice(b"LIVE");
        let (_, tail) = SnapshotBuffer::default().push(&with_tail).unwrap().unwrap();
        assert_eq!(tail, b"LIVE");
        bytes[20] ^= 1;
        assert!(SnapshotBuffer::default().push(&bytes).is_err());
    }
}
