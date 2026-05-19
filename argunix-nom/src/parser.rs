//! Incremental parser for nix's `internal-json` log format.
//!
//! nix emits one `@nix {…}` JSON object per line on stderr. argunix
//! receives that stderr in arbitrary byte chunks (16 KB frames from a
//! pool builder), so the parser buffers an incomplete trailing line
//! across [`NomParser::feed`] calls and only ever parses whole lines.

use std::collections::HashMap;

use serde_json::Value;

use crate::event::{ActivityKind, NomEvent};

// nix `internal-json` activity type codes — see nix `logging.hh`
// (`enum ActivityType`). Only the ones argunix surfaces are named.
const ACT_COPY_PATH: u64 = 100;
const ACT_FILE_TRANSFER: u64 = 101;
const ACT_BUILDS: u64 = 104;
const ACT_BUILD: u64 = 105;
const ACT_SUBSTITUTE: u64 = 108;

// nix `internal-json` result type codes (`enum ResultType`).
const RES_BUILD_LOG_LINE: u64 = 101;
const RES_PROGRESS: u64 = 105;
const RES_POST_BUILD_LOG_LINE: u64 = 107;

/// A streaming parser for nix `internal-json`. Feed it raw stderr
/// chunks; get [`NomEvent`]s back. It tolerates JSON split across
/// chunk boundaries and degrades anything unrecognised to
/// [`NomEvent::Raw`] — it never errors and never panics.
#[derive(Default)]
pub struct NomParser {
    /// Bytes of an incomplete trailing line, carried across `feed`s.
    pending: Vec<u8>,
    /// Live activity labels by id, for attributing build-log lines
    /// back to the derivation that produced them.
    activities: HashMap<u64, String>,
    /// Id of nix's aggregate `actBuilds` activity, once seen — its
    /// `resProgress` results become [`NomEvent::Progress`].
    builds_activity: Option<u64>,
}

impl NomParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one raw stderr chunk. Returns the events from every
    /// *complete* line in it; an incomplete trailing line is buffered
    /// for the next call.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<NomEvent> {
        let mut out = Vec::new();
        self.pending.extend_from_slice(chunk);
        while let Some(nl) = self.pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=nl).collect();
            self.parse_line(&line[..line.len() - 1], &mut out);
        }
        out
    }

    /// Flush at end of stream — emit any buffered partial line (a
    /// final line nix wrote without a trailing newline).
    pub fn finish(&mut self) -> Vec<NomEvent> {
        let mut out = Vec::new();
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.parse_line(&line, &mut out);
        }
        out
    }

    fn parse_line(&mut self, line: &[u8], out: &mut Vec<NomEvent>) {
        if line.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(line);
        // Anything that is not a `@nix {…}` line — plain build output
        // from a pre-flag builder, an archived `--read-log` dump, an
        // argunix-injected notice — passes straight through.
        let Some(json) = text.strip_prefix("@nix ") else {
            out.push(NomEvent::Raw {
                text: text.into_owned(),
            });
            return;
        };
        let Ok(v) = serde_json::from_str::<Value>(json) else {
            out.push(NomEvent::Raw {
                text: text.into_owned(),
            });
            return;
        };
        match v.get("action").and_then(Value::as_str) {
            Some("start") => self.on_start(&v, out),
            Some("stop") => self.on_stop(&v, out),
            Some("result") => self.on_result(&v, out),
            Some("msg") => {
                if let Some(msg) = v.get("msg").and_then(Value::as_str) {
                    let level = v.get("level").and_then(Value::as_u64).unwrap_or(3) as u8;
                    out.push(NomEvent::Message {
                        level,
                        text: msg.to_string(),
                    });
                }
            }
            // Unknown / future action — ignored, never fatal.
            _ => {}
        }
    }

    fn on_start(&mut self, v: &Value, out: &mut Vec<NomEvent>) {
        let id = v.get("id").and_then(Value::as_u64).unwrap_or(0);
        let typ = v.get("type").and_then(Value::as_u64).unwrap_or(0);
        // nix's aggregate builds counter — not an activity we show,
        // but its progress results drive the live footer.
        if typ == ACT_BUILDS {
            self.builds_activity = Some(id);
            return;
        }
        let act = match typ {
            ACT_BUILD => ActivityKind::Build,
            ACT_SUBSTITUTE => ActivityKind::Substitute,
            ACT_FILE_TRANSFER => ActivityKind::Download,
            ACT_COPY_PATH => ActivityKind::CopyPath,
            // Evaluation, query-path-info, realise, … — internal noise.
            _ => return,
        };
        let parent = v.get("parent").and_then(Value::as_u64).unwrap_or(0);
        let first_field = v
            .get("fields")
            .and_then(Value::as_array)
            .and_then(|f| f.first())
            .and_then(Value::as_str)
            .unwrap_or("");
        let label = match act {
            // `actBuild`/`actSubstitute`/`actCopyPath` field 0 is a
            // store path; `actFileTransfer` field 0 is a URL.
            ActivityKind::Download => first_field.to_string(),
            _ => short_name(first_field),
        };
        self.activities.insert(id, label.clone());
        out.push(NomEvent::ActStart {
            id,
            parent,
            act,
            label,
        });
    }

    fn on_stop(&mut self, v: &Value, out: &mut Vec<NomEvent>) {
        let id = v.get("id").and_then(Value::as_u64).unwrap_or(0);
        if self.builds_activity == Some(id) {
            self.builds_activity = None;
        }
        if self.activities.remove(&id).is_some() {
            out.push(NomEvent::ActStop { id });
        }
    }

    fn on_result(&mut self, v: &Value, out: &mut Vec<NomEvent>) {
        let id = v.get("id").and_then(Value::as_u64).unwrap_or(0);
        let typ = v.get("type").and_then(Value::as_u64).unwrap_or(0);
        let fields = v.get("fields").and_then(Value::as_array);
        match typ {
            RES_BUILD_LOG_LINE | RES_POST_BUILD_LOG_LINE => {
                let text = fields
                    .and_then(|f| f.first())
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let label = self
                    .activities
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| "build".to_string());
                out.push(NomEvent::Line {
                    activity: id,
                    label,
                    text: text.to_string(),
                });
            }
            // resProgress carries `[done, expected, running, failed]`;
            // only the aggregate builds counter's is worth surfacing.
            RES_PROGRESS if self.builds_activity == Some(id) => {
                let n = |i: usize| {
                    fields
                        .and_then(|f| f.get(i))
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                };
                out.push(NomEvent::Progress {
                    done: n(0),
                    expected: n(1),
                    running: n(2),
                    failed: n(3),
                });
            }
            _ => {}
        }
    }
}

/// Reduce a `/nix/store/<hash>-<name>` path (with an optional `.drv`
/// suffix) to just the human `<name>`. Returns the last path segment
/// unchanged if it does not look like a store path.
fn short_name(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    let base = base.strip_suffix(".drv").unwrap_or(base);
    // A store basename is `<32-char nix-base32 hash>-<name>`.
    match base.split_once('-') {
        Some((hash, name)) if hash.len() == 32 && !name.is_empty() => name.to_string(),
        _ => base.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real `nix build --log-format internal-json` capture: the
    /// flake eval, substituter queries, and one `runCommand` build
    /// that echoes five lines.
    const FIXTURE: &str = include_str!("../tests/fixtures/build.jsonl");

    fn all_events(input: &[u8]) -> Vec<NomEvent> {
        let mut p = NomParser::new();
        let mut ev = p.feed(input);
        ev.extend(p.finish());
        ev
    }

    #[test]
    fn short_name_strips_store_hash_and_drv() {
        assert_eq!(
            short_name("/nix/store/6mggkdmgi32wq9dwfkh6cj2iw7988b8d-nom-fixture.drv"),
            "nom-fixture",
        );
        assert_eq!(
            short_name("/nix/store/6mggkdmgi32wq9dwfkh6cj2iw7988b8d-hello-2.12.1"),
            "hello-2.12.1",
        );
        // Not a store path — last segment, unchanged.
        assert_eq!(short_name("just-a-name"), "just-a-name");
    }

    #[test]
    fn attributes_build_log_lines_to_their_derivation() {
        let events = all_events(FIXTURE.as_bytes());
        let lines: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                NomEvent::Line { label, text, .. } if label == "nom-fixture" => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            lines,
            [
                "building the nom fixture derivation",
                "a second line of output",
                "step 1 of 3",
                "step 2 of 3",
                "step 3 of 3",
            ],
        );
        // The build activity is announced and finished.
        assert!(events.iter().any(|e| matches!(
            e,
            NomEvent::ActStart { act: ActivityKind::Build, label, .. } if label == "nom-fixture"
        )));
        assert!(events.iter().any(|e| matches!(e, NomEvent::ActStop { .. })));
    }

    #[test]
    fn surfaces_aggregate_progress() {
        let events = all_events(FIXTURE.as_bytes());
        assert!(
            events
                .iter()
                .any(|e| matches!(e, NomEvent::Progress { expected, .. } if *expected >= 1)),
            "expected at least one Progress event from the actBuilds counter",
        );
    }

    #[test]
    fn non_json_lines_pass_through_as_raw() {
        let events = all_events(b"plain build output\nnot @nix prefixed\n");
        assert_eq!(
            events,
            [
                NomEvent::Raw {
                    text: "plain build output".into()
                },
                NomEvent::Raw {
                    text: "not @nix prefixed".into()
                },
            ],
        );
    }

    #[test]
    fn malformed_nix_json_degrades_to_raw() {
        let events = all_events(b"@nix {not valid json\n");
        assert!(matches!(events.as_slice(), [NomEvent::Raw { .. }]));
    }

    #[test]
    fn a_final_line_without_a_newline_is_flushed() {
        let mut p = NomParser::new();
        assert!(p.feed(b"trailing line no newline").is_empty());
        assert_eq!(
            p.finish(),
            [NomEvent::Raw {
                text: "trailing line no newline".into()
            }],
        );
    }

    #[test]
    fn split_at_every_byte_offset_yields_identical_events() {
        // The load-bearing partial-line property: feeding the stream
        // in two pieces split anywhere must produce the exact same
        // events as feeding it whole.
        let bytes = FIXTURE.as_bytes();
        let whole = all_events(bytes);
        for split in 0..=bytes.len() {
            let mut p = NomParser::new();
            let mut got = p.feed(&bytes[..split]);
            got.extend(p.feed(&bytes[split..]));
            got.extend(p.finish());
            assert_eq!(got, whole, "mismatch when split at byte {split}");
        }
    }
}
