//! Wire protocol for the medusa-builder control channel.
//!
//! Newline-delimited JSON in both directions. The original five
//! message types (`Hello`/`Welcome`/`Heartbeat`/`Shutdown`/`Kick`)
//! cover connection lifecycle. M14b adds five build-dispatch types
//! (`Build`/`BuildStarted`/`BuildLogChunk`/`BuildFinished`/`Abort`)
//! so the daemon can drive `nix-store --realise` *on the agent host*
//! over this channel, matching hydra's "subprocess on the builder"
//! pattern (`hydra-builder/src/state.rs:460` →
//! `crates/nix-utils/src/realise.rs:83`). Drv closure / output
//! transport is intentionally *not* part of this protocol — it goes
//! over a separate side channel (TBD; see `design/plan.md` M14b).
//! See `design/builders.md` for the rationale.

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
    /// Medusa → builder (M14b). Dispatch a single derivation to be
    /// realised on the builder host. The daemon is responsible for
    /// having ensured the drv (and its transitive input closure) is
    /// already present in the builder's nix store before sending this
    /// — closure transport is not part of the control protocol.
    ///
    /// `build_id` is operator-allocated by the daemon (we use the
    /// `JobId` row id). It correlates the subsequent
    /// `BuildStarted`/`BuildLogChunk`/`BuildFinished` messages and
    /// any later `Abort`.
    Build {
        build_id: i64,
        drv_path: String,
        /// Optional gcroot path on the *builder*. `None` means no
        /// `--add-root`. Surfaced over the wire because the agent is
        /// the only side that can write into its own filesystem.
        #[serde(default)]
        gc_root: Option<String>,
        /// Wall-clock cap, seconds. Mirrors `nix-store --timeout`.
        timeout_secs: u64,
        /// Soft cap on raw stderr bytes the agent buffers and sends
        /// as `BuildLogChunk`. Frames past this point are dropped
        /// (the agent records the truncation in the final
        /// `BuildFinished`). Mirrors `LogCaptureLimit` on the daemon.
        max_log_bytes: u64,
    },
    /// Builder → medusa. Acknowledges that the agent received a
    /// `Build` and successfully spawned `nix-store --realise`. Sent
    /// once per build, before the first `BuildLogChunk`. `pid` is
    /// informational (operators correlating agent logs).
    BuildStarted {
        build_id: i64,
        #[serde(default)]
        pid: Option<u32>,
    },
    /// Builder → medusa. A chunk of raw stderr bytes from
    /// `nix-store --realise`, base64-encoded so the JSON line stays
    /// printable. Multiple chunks per build; ordered. Bytes are
    /// concatenated daemon-side and written into the build log
    /// (zstd-compressed, capped) by the existing `LogCaptureLimit`
    /// machinery. Stops at `max_log_bytes`.
    BuildLogChunk { build_id: i64, bytes_b64: String },
    /// Builder → medusa. Terminal status of one dispatched build.
    /// `output_paths` are the stdout lines from `nix-store --realise`
    /// (i.e. the realised store paths), in the order printed.
    BuildFinished {
        build_id: i64,
        status: BuildOutcomeStatus,
        #[serde(default)]
        exit_code: Option<i32>,
        #[serde(default)]
        output_paths: Vec<String>,
        /// True if `BuildLogChunk` framing stopped at `max_log_bytes`
        /// before the subprocess finished writing stderr.
        #[serde(default)]
        log_truncated: bool,
    },
    /// Medusa → builder. Cancel a running build. The agent SIGKILLs
    /// the `nix-store --realise` child (via `kill_on_drop` plus an
    /// explicit `start_kill`), drains any remaining stderr into a
    /// final `BuildLogChunk`, and emits a `BuildFinished` with
    /// `status: Killed`.
    Abort { build_id: i64 },
}

/// Terminal disposition reported in `BuildFinished`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildOutcomeStatus {
    Success,
    Failure,
    /// Wall-clock timeout from the `Build`'s `timeout_secs`. Distinct
    /// from `Failure` so the daemon can surface a different failure
    /// reason and decide whether to retry.
    Timeout,
    /// Subprocess was killed (operator cancel via `Abort`, daemon
    /// disconnect, or builder shutdown).
    Killed,
    /// Agent failed to spawn the subprocess at all (binary missing,
    /// permission denied). Daemon should log and treat as fatal —
    /// retrying on the same builder won't help.
    SpawnFailed,
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
    fn build_round_trip() {
        let m = ControlMessage::Build {
            build_id: 7,
            drv_path: "/nix/store/abc-foo.drv".into(),
            gc_root: Some("/nix/var/nix/gcroots/per-user/medusa/1/2/3".into()),
            timeout_secs: 7200,
            max_log_bytes: 16 * 1024 * 1024,
        };
        let s = std::str::from_utf8(&m.encode_line())
            .unwrap()
            .trim_end()
            .to_string();
        let back: ControlMessage = serde_json::from_str(&s).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn build_started_round_trip_with_optional_pid() {
        let with_pid = ControlMessage::BuildStarted {
            build_id: 7,
            pid: Some(12345),
        };
        let without_pid = ControlMessage::BuildStarted {
            build_id: 7,
            pid: None,
        };
        for m in [with_pid, without_pid] {
            let s = std::str::from_utf8(&m.encode_line())
                .unwrap()
                .trim_end()
                .to_string();
            let back: ControlMessage = serde_json::from_str(&s).unwrap();
            assert_eq!(back, m);
        }
    }

    #[test]
    fn build_log_chunk_round_trip_preserves_bytes() {
        use base64::Engine;
        let raw: Vec<u8> = (0u8..=255).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
        let m = ControlMessage::BuildLogChunk {
            build_id: 7,
            bytes_b64: b64.clone(),
        };
        let s = std::str::from_utf8(&m.encode_line())
            .unwrap()
            .trim_end()
            .to_string();
        let back: ControlMessage = serde_json::from_str(&s).unwrap();
        let ControlMessage::BuildLogChunk { bytes_b64, .. } = back else {
            panic!("variant lost in round trip");
        };
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&bytes_b64)
            .expect("agent-encoded base64 must decode daemon-side");
        assert_eq!(decoded, raw, "every byte must survive the round trip");
    }

    #[test]
    fn build_finished_round_trip_each_status() {
        for status in [
            BuildOutcomeStatus::Success,
            BuildOutcomeStatus::Failure,
            BuildOutcomeStatus::Timeout,
            BuildOutcomeStatus::Killed,
            BuildOutcomeStatus::SpawnFailed,
        ] {
            let m = ControlMessage::BuildFinished {
                build_id: 7,
                status,
                exit_code: Some(0),
                output_paths: vec!["/nix/store/zzz-foo".into(), "/nix/store/yyy-foo-dev".into()],
                log_truncated: false,
            };
            let s = std::str::from_utf8(&m.encode_line())
                .unwrap()
                .trim_end()
                .to_string();
            let back: ControlMessage = serde_json::from_str(&s).unwrap();
            assert_eq!(back, m, "status {status:?} did not round-trip");
        }
    }

    #[test]
    fn abort_round_trip() {
        let m = ControlMessage::Abort { build_id: 7 };
        let s = std::str::from_utf8(&m.encode_line())
            .unwrap()
            .trim_end()
            .to_string();
        let back: ControlMessage = serde_json::from_str(&s).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn framer_recovers_unknown_build_status_without_dropping_subsequent() {
        // Forward-compatibility: a future `BuildFinished` variant
        // adding a new `status` value must surface as a parse error
        // for that line only, not corrupt the framer.
        let mut f = LineFramer::new();
        let blob = [
            line(r#"{"type":"build_finished","build_id":1,"status":"new_future_status"}"#),
            line(r#"{"type":"abort","build_id":1}"#),
        ]
        .concat();
        let got: Vec<_> = f.extend(&blob);
        assert_eq!(got.len(), 2);
        assert!(got[0].is_err(), "unknown status must produce a parse error");
        assert!(matches!(
            got[1].as_ref().unwrap(),
            ControlMessage::Abort { build_id: 1 }
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
