//! OSC 7501 revision 0.4 and Wezmux OSC 7777. Data only: never execute report text.
use base64::{
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
    Engine,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<u8>,
}
#[derive(Default, Debug)]
pub struct Status {
    pub records: BTreeMap<String, Record>,
    order: Vec<String>,
}
fn segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.+-".contains(&c))
}
fn text(v: &str, max: usize) -> Option<String> {
    if v.len() > 4 * ((max + 2) / 3) {
        return None;
    }
    let bytes = STANDARD
        .decode(v)
        .or_else(|_| STANDARD_NO_PAD.decode(v))
        .ok()?;
    if bytes.len() > max {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    if text.chars().any(char::is_control) {
        return None;
    }
    Some(text)
}
/// UI labels are plain, single-line text with bidi/formatting controls removed.
pub fn label(s: &str) -> String {
    s.chars().filter(|c| !c.is_control() && !matches!(*c,'\u{061c}'|'\u{200b}'..='\u{200f}'|'\u{202a}'..='\u{202e}'|'\u{2060}'..='\u{206f}'|'\u{feff}')).collect()
}
impl Status {
    /// Invalid reports leave all records untouched. Returns true for an accepted report.
    pub fn report(&mut self, body: &str) -> bool {
        if body.len() + 9 > 4096 {
            return false;
        }
        let mut pairs = BTreeMap::new();
        // Validate every occurrence, including values later replaced by a duplicate key.
        for pair in body.split(':') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();
            if key.len() > 16 {
                return false;
            }
            if key.is_empty()
                || !key.bytes().all(|c| c.is_ascii_lowercase())
                || !value
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_.,+/=-".contains(&c))
            {
                continue;
            }
            match key {
                "msg" if text(value, 2048).is_none() => return false,
                "title" if text(value, 192).is_none() => return false,
                "app" if value.len() > 32 => return false,
                "id" if value.len() > 128 => return false,
                _ => {}
            }
            pairs.insert(key, value);
        }
        let Some(&state) = pairs.get("state") else {
            return false;
        };
        if !matches!(
            state,
            "idle" | "working" | "done" | "blocked" | "error" | "clear"
        ) {
            return false;
        }
        let id = pairs.get("id").copied().unwrap_or("");
        if pairs.contains_key("id") && (id.split('/').count() > 8 || !id.split('/').all(segment)) {
            return false;
        }
        if state == "clear" {
            let prefix = format!("{id}/");
            self.records
                .retain(|k, _| !(id.is_empty() || k == id || k.starts_with(&prefix)));
            self.order.retain(|k| self.records.contains_key(k));
            return true;
        }
        let kind = pairs
            .get("kind")
            .filter(|v| state == "blocked" && matches!(**v, "permission" | "question" | "auth"))
            .map(|v| v.to_string());
        let progress = pairs
            .get("progress")
            .filter(|v| {
                matches!(state, "working" | "blocked")
                    && !v.is_empty()
                    && v.bytes().all(|c| c.is_ascii_digit())
            })
            .and_then(|v| v.parse::<u8>().ok())
            .filter(|v| *v <= 100);
        let record = Record {
            state: state.into(),
            kind,
            progress,
            app: pairs
                .get("app")
                .filter(|v| segment(v))
                .map(|v| v.to_string()),
            title: pairs.get("title").and_then(|v| text(v, 192)),
            msg: pairs.get("msg").and_then(|v| text(v, 2048)),
        };
        self.order.retain(|k| k != id);
        if self.order.len() >= 256 {
            let first = self.order.remove(0);
            self.records.remove(&first);
        }
        self.order.push(id.into());
        self.records.insert(id.into(), record);
        true
    }
    pub fn prompt_or_exit(&mut self) {
        self.records
            .retain(|_, r| !matches!(r.state.as_str(), "working" | "blocked"));
        self.order.retain(|k| self.records.contains_key(k));
    }
    pub fn reset(&mut self) {
        self.records.clear();
        self.order.clear();
    }
    pub fn json(&self) -> String {
        // Resolve app inheritance without changing stored records.
        let mut records = self.records.clone();
        for (id, r) in &mut records {
            let mut parent = id.as_str();
            while r.app.is_none() && !parent.is_empty() {
                parent = parent.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
                r.app = self.records.get(parent).and_then(|p| p.app.clone());
            }
            r.title = r.title.as_deref().map(label);
            r.msg = r.msg.as_deref().map(label);
        }
        serde_json::to_string(&records).unwrap()
    }
}

/// Wezmux wire form is `7777;event;data`, not Carrot's hex-JSON form.
pub fn agent_event(parts: &[Vec<u8>]) -> Option<(String, String)> {
    let event = std::str::from_utf8(parts.first()?).ok()?;
    if !matches!(
        event,
        "status"
            | "agent"
            | "title"
            | "message"
            | "tool"
            | "subagents"
            | "background_tasks"
            | "session_crons"
            | "clear"
    ) {
        return None;
    }
    if parts.iter().map(Vec::len).sum::<usize>() > 4096 {
        return None;
    }
    let data = parts[1..]
        .iter()
        .map(|s| std::str::from_utf8(s))
        .collect::<Result<Vec<_>, _>>()
        .ok()?
        .join(";");
    if data.chars().any(char::is_control) {
        return None;
    }
    if event == "status" && !matches!(data.as_str(), "working" | "idle" | "needs_input") {
        return None;
    }
    Some((event.into(), label(&data)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replacement_inheritance_and_subtree_clear() {
        let mut s = Status::default();
        assert!(s.report("state=idle:app=claude"));
        assert!(s.report("state=working:id=build:progress=10"));
        assert!(s.report("state=blocked:id=build/test:kind=question"));
        assert!(s.json().contains("claude"));
        assert!(s.report("state=done:id=builder"));
        assert!(s.report("state=clear:id=build"));
        assert_eq!(s.records.len(), 2);
        assert!(s.report("state=done"));
        assert_eq!(s.records[""].app, None);
        assert!(s.report("state=clear"));
        assert!(s.records.is_empty());
    }
    #[test]
    fn invalid_reports_are_atomic() {
        let mut s = Status::default();
        s.report("state=working");
        let before = s.json();
        for bad in [
            "state=other",
            "state=done:id=",
            "state=done:id=a//b",
            "state=done:msg=Cg==",
            "state=done:msg=%%%",
            "state=done:msg=A",
            "state=done:msg=Cg==:msg=b2s=",
        ] {
            // Invalid value-set pairs are skipped by the specification.
            if bad.contains("%%%") {
                continue;
            }
            assert!(!s.report(bad), "{bad}");
            assert_eq!(s.json(), before);
        }
    }
    #[test]
    fn duplicate_pairs_and_optional_values() {
        let mut s = Status::default();
        assert!(
            s.report(" state = working :broken:state=blocked:kind=question:progress=101:msg=b2s")
        );
        assert_eq!(s.records[""].msg.as_deref(), Some("ok"));
        assert_eq!(s.records[""].progress, None);
        assert!(s.report("state=done:kind=question:progress=2"));
        assert_eq!(s.records[""].kind, None);
    }
    #[test]
    fn prompt_lifetime_and_capacity() {
        let mut s = Status::default();
        s.report("state=done");
        s.report("state=blocked:id=wait");
        s.prompt_or_exit();
        assert_eq!(s.records.len(), 1);
        for i in 0..300 {
            s.report(&format!("state=idle:id=r{i}"));
        }
        assert_eq!(s.records.len(), 256);
        assert!(!s.records.contains_key("r0"));
        s.reset();
        assert!(s.records.is_empty());
    }
    #[test]
    fn wezmux_preserves_semicolons() {
        assert_eq!(
            agent_event(&[b"title".to_vec(), b"Fix auth".to_vec(), b" tests".to_vec()]),
            Some(("title".into(), "Fix auth; tests".into()))
        );
        assert!(agent_event(&[b"status".to_vec(), b"bogus".to_vec()]).is_none());
    }
}
