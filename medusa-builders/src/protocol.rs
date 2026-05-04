//! Wire protocol for the medusa-builder control channel.
//!
//! Newline-delimited JSON in both directions, exactly five message
//! types. See `design/builders.md` for the rationale.

use medusa_domain::BuilderName;
use serde::{Deserialize, Serialize};

/// Shared envelope for every control-channel message. `tag` selects the
/// variant; serde flattens the variant fields onto the same object.
///
/// `Eq` is intentionally not derived — `Heartbeat::load` is `Option<f64>`
/// which precludes it. Round-trip tests use `==` (PartialEq).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMessage {
    /// Builder → medusa. Sent immediately after auth completes; carries
    /// the builder's self-described identity and capabilities.
    Hello {
        name: BuilderName,
        systems: Vec<String>,
        #[serde(default)]
        features: Vec<String>,
        #[serde(default = "default_max_jobs")]
        max_jobs: u32,
        #[serde(default)]
        nix_version: String,
    },
    /// Medusa → builder. Sent in reply to Hello. `builder_id` is the
    /// sqlite row id, surfaced for the agent's log line so operators
    /// can correlate `medusactl builders` output with the agent's logs.
    Welcome { builder_id: String },
    /// Builder → medusa. Periodic liveness signal. The optional `load`
    /// is a free-form floating point (e.g. 1-min loadavg); medusa
    /// records it on the row for `medusactl builders`.
    Heartbeat {
        ts: i64,
        #[serde(default)]
        load: Option<f64>,
    },
    /// Builder → medusa. Sent on graceful agent stop (SIGTERM).
    /// `drain` is reserved for future graceful-drain semantics; v1
    /// always announces with `drain=false` and exits immediately.
    Shutdown {
        #[serde(default)]
        reason: String,
        #[serde(default)]
        drain: bool,
    },
    /// Medusa → builder. Sent on revoke or duplicate-name takeover.
    /// The agent should close cleanly and stop reconnecting until the
    /// operator intervenes.
    Kick {
        #[serde(default)]
        reason: String,
    },
}

fn default_max_jobs() -> u32 {
    1
}

impl ControlMessage {
    /// Serialize to a single JSON line (with trailing newline). Caller
    /// writes the bytes to the SSH channel.
    pub fn encode_line(&self) -> Vec<u8> {
        let mut buf =
            serde_json::to_vec(self).expect("ControlMessage always serialises (no f64::NaN)");
        buf.push(b'\n');
        buf
    }
}

/// Stateful line-extractor for the control channel. Russh delivers
/// `data()` callbacks with arbitrary chunk boundaries, so we accumulate
/// bytes and emit one parsed message per complete `\n`-terminated line.
#[derive(Default)]
pub struct LineFramer {
    buf: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("control channel line is not valid UTF-8")]
    NotUtf8,
    #[error("control channel JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("control channel line is too long ({0} bytes; cap {1})")]
    LineTooLong(usize, usize),
}

const MAX_LINE_BYTES: usize = 64 * 1024;

impl LineFramer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `chunk` and drain any complete lines. Each yielded result
    /// is either a parsed message or a protocol error for that line; the
    /// framer recovers from per-line errors so a malformed line doesn't
    /// poison subsequent ones.
    pub fn extend(&mut self, chunk: &[u8]) -> Vec<Result<ControlMessage, ProtocolError>> {
        if self.buf.len() + chunk.len() > MAX_LINE_BYTES * 4 {
            // Defensive cap so a misbehaving / malicious client can't
            // grow the buffer without bound. The 4× allows for some
            // slack while still tripping fast on garbage.
            self.buf.clear();
            return vec![Err(ProtocolError::LineTooLong(chunk.len(), MAX_LINE_BYTES))];
        }
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            // Strip the trailing '\n'.
            let line = &line[..line.len() - 1];
            if line.is_empty() {
                continue;
            }
            if line.len() > MAX_LINE_BYTES {
                out.push(Err(ProtocolError::LineTooLong(line.len(), MAX_LINE_BYTES)));
                continue;
            }
            match std::str::from_utf8(line) {
                Err(_) => out.push(Err(ProtocolError::NotUtf8)),
                Ok(s) => match serde_json::from_str::<ControlMessage>(s) {
                    Ok(m) => out.push(Ok(m)),
                    Err(e) => out.push(Err(ProtocolError::Json(e))),
                },
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(s: &str) -> Vec<u8> {
        let mut b = s.as_bytes().to_vec();
        b.push(b'\n');
        b
    }

    #[test]
    fn hello_round_trip() {
        let msg = ControlMessage::Hello {
            name: BuilderName::new("bobs-mini").unwrap(),
            systems: vec!["aarch64-darwin".into(), "aarch64-linux".into()],
            features: vec!["big-parallel".into()],
            max_jobs: 2,
            nix_version: "2.18.1".into(),
        };
        let bytes = msg.encode_line();
        assert!(bytes.ends_with(b"\n"));
        let s = std::str::from_utf8(&bytes).unwrap().trim_end();
        let back: ControlMessage = serde_json::from_str(s).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn welcome_round_trip() {
        let m = ControlMessage::Welcome {
            builder_id: "42".into(),
        };
        let bytes = m.encode_line();
        let s = std::str::from_utf8(&bytes).unwrap().trim_end();
        let back: ControlMessage = serde_json::from_str(s).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn framer_handles_split_lines() {
        let mut f = LineFramer::new();
        let mut got = Vec::new();
        got.extend(f.extend(br#"{"type":"heart"#));
        got.extend(f.extend(br#"beat","ts":42}"#));
        // No newline yet → no message emitted.
        assert!(got.is_empty());
        got.extend(f.extend(b"\n"));
        assert_eq!(got.len(), 1);
        let m = got.into_iter().next().unwrap().unwrap();
        match m {
            ControlMessage::Heartbeat { ts, load } => {
                assert_eq!(ts, 42);
                assert!(load.is_none());
            }
            other => panic!("expected Heartbeat, got {other:?}"),
        }
    }

    #[test]
    fn framer_handles_multiple_lines_at_once() {
        let mut f = LineFramer::new();
        let blob = [
            line(r#"{"type":"heartbeat","ts":1}"#),
            line(r#"{"type":"heartbeat","ts":2}"#),
        ]
        .concat();
        let got: Vec<_> = f.extend(&blob);
        assert_eq!(got.len(), 2);
        for r in got {
            assert!(matches!(r.unwrap(), ControlMessage::Heartbeat { .. }));
        }
    }

    #[test]
    fn framer_recovers_from_garbage_lines() {
        // A bad line shouldn't poison subsequent good ones — operators
        // should be able to identify the offender and the agent
        // shouldn't lose subsequent valid heartbeats over one typo.
        let mut f = LineFramer::new();
        let blob = [
            line("not even json"),
            line(r#"{"type":"unknown_variant"}"#),
            line(r#"{"type":"heartbeat","ts":99}"#),
        ]
        .concat();
        let got: Vec<_> = f.extend(&blob);
        assert_eq!(got.len(), 3);
        assert!(got[0].is_err());
        assert!(got[1].is_err());
        assert!(matches!(
            got[2].as_ref().unwrap(),
            ControlMessage::Heartbeat { .. }
        ));
    }

    #[test]
    fn framer_skips_blank_lines() {
        let mut f = LineFramer::new();
        let blob = [b"\n\n".to_vec(), line(r#"{"type":"heartbeat","ts":1}"#)].concat();
        let got: Vec<_> = f.extend(&blob);
        assert_eq!(got.len(), 1);
        assert!(matches!(
            got[0].as_ref().unwrap(),
            ControlMessage::Heartbeat { .. }
        ));
    }
}
