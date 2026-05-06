//! Side-channel framing (M14b).
//!
//! A side channel is a fresh russh session channel opened per
//! closure transfer (one in each direction per build). The channel
//! starts with a single newline-terminated JSON header describing
//! what the payload is, followed by raw bytes until channel-close.
//! See `design/plan.md` M14b for the broader picture.
//!
//! Why a header instead of inferring from byte content: nix-serve's
//! own protocol starts with magic bytes, and `nix-store --export`
//! output starts with magic bytes too. A JSON header makes the
//! channel kind explicit before any binary payload starts, so the
//! agent's dispatcher can branch unambiguously on the first line
//! without parsing nix's wire format.
//!
//! This module owns the **codec** (encoding, decoding, the IO
//! helpers to read/write a header from/to an `AsyncRead` /
//! `AsyncWrite`) and the agent-side **dispatcher**
//! ([`dispatch_inbound`]), which composes the codec with
//! [`crate::closure_xfer::import_closure`] /
//! [`crate::closure_xfer::export_closure`] to handle a complete
//! side-channel transfer. The russh wiring — adapting a
//! `russh::Channel` into `AsyncRead`+`AsyncWrite` — lives in a
//! separate adapter so this module stays unit-testable through
//! `tokio::io::duplex`.

use crate::closure_xfer::{
    ClosureXferError, ClosureXferOutcome, export_closure, import_closure, query_requisites,
};
use crate::protocol::BuildOutcomeStatus;
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Header that prefaces every M14b side channel. Always exactly one
/// `\n`-terminated JSON line, written before any binary payload. The
/// `paths` field carries the store paths the payload represents,
/// which the receiver uses for logging and post-import validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SideChannelHeader {
    pub kind: SideChannelKind,
    /// Correlates with the `Build` control message that triggered
    /// this transfer. Receivers log it for trace correlation.
    pub build_id: i64,
    /// Store paths the payload represents.
    ///
    /// - `ClosurePush`: the drv plus its transitive input closure.
    ///   The agent runs `nix-store --import` and the resulting paths
    ///   should match this list (validation is best-effort).
    /// - `ClosurePull`: the output paths of a finished build.
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideChannelKind {
    /// Daemon → agent. Payload is a `nix-store --export` byte
    /// stream of the drv closure; agent pipes it into
    /// `nix-store --import`.
    ClosurePush,
    /// Agent → daemon. Payload is a `nix-store --export` byte
    /// stream of the build output paths; daemon pipes it into
    /// `nix-store --import`.
    ClosurePull,
}

/// Trailer the agent writes back on a `ClosurePush` channel after its
/// `nix-store --import` finishes. Without this, a daemon-side IO
/// error mid-write (e.g. `BrokenPipe` because the agent's import
/// exited early) would have no diagnostic context — the operator
/// would need to grep the agent's journal by timestamp to find the
/// real cause. With this, the daemon reads the trailer after writing
/// and folds the agent's stderr into the build log.
///
/// Single newline-terminated JSON line, written immediately after
/// `nix-store --import` returns and before the agent's user closure
/// drops the channel writer. Best-effort on both sides — if the
/// channel is already torn down by the time the agent tries to write,
/// the daemon will simply not read it; the daemon's read is bounded
/// by a timeout so a non-cooperating agent can't hang the dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClosurePushReply {
    pub ok: bool,
    pub bytes_received: u64,
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Captured stderr from the agent's `nix-store --import`. UTF-8
    /// lossy on the wire; binary noise is rare in nix output.
    #[serde(default)]
    pub stderr: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SideChannelError {
    #[error("reading side-channel header: {0}")]
    Io(#[from] std::io::Error),
    #[error("side-channel header is not valid UTF-8")]
    NotUtf8,
    #[error("side-channel header JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("side-channel header exceeded max length ({actual} > {cap}); refusing to read further")]
    HeaderTooLong { actual: usize, cap: usize },
    #[error("side-channel reader closed before header newline arrived")]
    UnexpectedEof,
}

/// Cap on the side-channel header size. The JSON header carries the
/// full `paths` list of the closure being transferred — for fat
/// closures (thousands of input drvs, e.g. a moderately complex Rust
/// build) that easily exceeds 100 KiB. The original 64 KiB cap was
/// way too tight in production: a single Rust crate with ~1000
/// transitive deps already serialises to ~80 KiB and trips it.
///
/// 16 MiB is *defense against a hostile / runaway peer* (preventing
/// unbounded memory growth on a peer that opens a channel and never
/// terminates the line). It is comfortably above any realistic drv
/// closure: at ~85 bytes per path, that's ~190K paths — well past
/// the size of the entire nixpkgs closure.
pub const MAX_HEADER_BYTES: usize = 16 * 1024 * 1024;

impl SideChannelHeader {
    /// Encode as a single JSON line with trailing `\n`. Always
    /// succeeds — no `f64` fields.
    pub fn encode_line(&self) -> Vec<u8> {
        let mut buf = serde_json::to_vec(self).expect("SideChannelHeader always serialises");
        buf.push(b'\n');
        buf
    }
}

/// Read the header line from `reader`, byte-by-byte until a `\n`
/// arrives. Caps at [`MAX_HEADER_BYTES`] to defend against a peer
/// that opens a channel and never sends a newline. After this
/// returns, `reader` is positioned at the first byte of the binary
/// payload.
pub async fn read_header<R>(reader: &mut R) -> Result<SideChannelHeader, SideChannelError>
where
    R: AsyncRead + Unpin,
{
    let mut buf = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    loop {
        let n = reader.read(&mut byte).await?;
        if n == 0 {
            return Err(SideChannelError::UnexpectedEof);
        }
        if byte[0] == b'\n' {
            break;
        }
        if buf.len() >= MAX_HEADER_BYTES {
            return Err(SideChannelError::HeaderTooLong {
                actual: buf.len() + 1,
                cap: MAX_HEADER_BYTES,
            });
        }
        buf.push(byte[0]);
    }
    let s = std::str::from_utf8(&buf).map_err(|_| SideChannelError::NotUtf8)?;
    let h: SideChannelHeader = serde_json::from_str(s)?;
    Ok(h)
}

/// Write the encoded header to `writer` and flush. The caller is
/// responsible for streaming payload bytes after this returns.
pub async fn write_header<W>(
    writer: &mut W,
    header: &SideChannelHeader,
) -> Result<(), SideChannelError>
where
    W: AsyncWrite + Unpin,
{
    let line = header.encode_line();
    writer.write_all(&line).await?;
    writer.flush().await?;
    Ok(())
}

/// Outcome of a side-channel dispatch — either a closure was
/// imported (push direction) or exported (pull direction). Both
/// arms carry the underlying [`ClosureXferOutcome`] so callers can
/// log byte counts and forward stderr to the daemon.
#[derive(Debug)]
pub enum DispatchOutcome {
    /// Daemon → agent direction completed. The agent imported the
    /// drv closure into its local nix store.
    ClosurePushed {
        build_id: i64,
        paths: Vec<String>,
        import: ClosureXferOutcome,
    },
    /// Agent → daemon direction completed. The agent exported the
    /// requested paths onto the channel; the daemon will read them
    /// on the other end.
    ClosurePulled {
        build_id: i64,
        paths: Vec<String>,
        export: ClosureXferOutcome,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("reading side-channel header: {0}")]
    Header(#[from] SideChannelError),
    #[error("running nix-store subprocess for transfer: {0}")]
    Xfer(#[from] ClosureXferError),
}

/// Agent-side responder: read a side-channel header from `reader`,
/// branch on `kind`, and either pipe `reader`'s remaining bytes
/// into `nix-store --import` (`ClosurePush`) or run
/// `nix-store --export <paths>` and stream its stdout onto `writer`
/// (`ClosurePull`). The header is parsed first, so a malformed
/// header surfaces as a typed error before any subprocess spawns.
///
/// `nix_store_bin` is taken explicitly so unit tests can inject a
/// fake binary without mutating `PATH` (same convention as
/// `closure_xfer`).
pub async fn dispatch_inbound<R, W>(
    nix_store_bin: &Path,
    reader: &mut R,
    writer: &mut W,
) -> Result<DispatchOutcome, DispatchError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let header = read_header(reader).await?;
    match header.kind {
        SideChannelKind::ClosurePush => {
            let import = import_closure(nix_store_bin, reader).await?;
            // Reply with a JSON line so the daemon side has diagnostic
            // context for any IO error during its own write phase
            // (BrokenPipe etc. — without this, the daemon log just says
            // "broken pipe" and the operator has to grep agent logs by
            // timestamp). Best-effort: if the channel is already torn
            // down, the write silently no-ops.
            let reply = ClosurePushReply {
                ok: import.status == BuildOutcomeStatus::Success,
                bytes_received: import.bytes_transferred,
                exit_code: import.exit_code,
                stderr: String::from_utf8_lossy(&import.stderr).into_owned(),
            };
            if let Ok(mut line) = serde_json::to_vec(&reply) {
                line.push(b'\n');
                let _ = writer.write_all(&line).await;
                let _ = writer.flush().await;
            }
            Ok(DispatchOutcome::ClosurePushed {
                build_id: header.build_id,
                paths: header.paths,
                import,
            })
        }
        SideChannelKind::ClosurePull => {
            // Expand the requested output paths to their full
            // runtime closure before exporting. `nix-store --export
            // <paths>` ships only the listed paths — it does NOT
            // include their references. Without this expansion, any
            // build that picks up a runtime dependency via
            // substitution during the build (e.g. an OVMF / glibc /
            // busybox path the build pulled from cache.nixos.org)
            // would arrive on the daemon side as an unimportable
            // orphan: `nix-store --import` rejects it with
            // `error: path '/nix/store/...' is not valid` because
            // the runtime dep isn't in the daemon's local store.
            let closure = query_requisites(nix_store_bin, &header.paths).await?;
            let export = export_closure(nix_store_bin, &closure, writer).await?;
            Ok(DispatchOutcome::ClosurePulled {
                build_id: header.build_id,
                // Surface the original output paths in the outcome,
                // not the expanded closure — the daemon-side log
                // wants "we built these outputs", not "and here are
                // 200 transitive deps we shipped".
                paths: header.paths,
                export,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[test]
    fn header_round_trip_via_json() {
        let h = SideChannelHeader {
            kind: SideChannelKind::ClosurePush,
            build_id: 42,
            paths: vec!["/nix/store/aaa-foo.drv".into(), "/nix/store/bbb-dep".into()],
        };
        let line = h.encode_line();
        assert!(line.ends_with(b"\n"), "header line must terminate with \\n");
        let s = std::str::from_utf8(&line[..line.len() - 1]).unwrap();
        let back: SideChannelHeader = serde_json::from_str(s).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn header_kind_is_snake_case_on_wire() {
        // Forward-compat: pin the on-wire spelling so a future agent
        // and daemon can interop across versions.
        let h = SideChannelHeader {
            kind: SideChannelKind::ClosurePush,
            build_id: 1,
            paths: vec![],
        };
        let line = h.encode_line();
        let s = std::str::from_utf8(&line).unwrap();
        assert!(
            s.contains("\"kind\":\"closure_push\""),
            "unexpected kind on wire: {s}",
        );
    }

    #[tokio::test]
    async fn read_header_parses_line_then_leaves_payload_intact() {
        let h = SideChannelHeader {
            kind: SideChannelKind::ClosurePull,
            build_id: 7,
            paths: vec!["/nix/store/zzz-out".into()],
        };
        let mut wire: Vec<u8> = h.encode_line();
        wire.extend_from_slice(b"BINARY-PAYLOAD-FOLLOWS");

        let mut reader = BufReader::new(&wire[..]);
        let parsed = read_header(&mut reader).await.expect("header parses");
        assert_eq!(parsed, h);

        // Reader must be positioned at the first byte AFTER the
        // header newline — i.e. the start of the binary payload.
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest, b"BINARY-PAYLOAD-FOLLOWS");
    }

    #[tokio::test]
    async fn read_header_handles_chunk_boundary_inside_header() {
        // Simulate a transport that delivers bytes in arbitrary
        // chunks — the byte-by-byte read loop must work regardless.
        let h = SideChannelHeader {
            kind: SideChannelKind::ClosurePush,
            build_id: 99,
            paths: vec!["/nix/store/aaa".into()],
        };
        let mut wire: Vec<u8> = h.encode_line();
        wire.extend_from_slice(b"PAYLOAD");

        // tokio::io::BufReader with a tiny capacity simulates the
        // worst-case tiny-chunks behaviour.
        let mut reader = BufReader::with_capacity(1, &wire[..]);
        let parsed = read_header(&mut reader).await.expect("header parses");
        assert_eq!(parsed, h);
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest, b"PAYLOAD");
    }

    #[tokio::test]
    async fn read_header_caps_at_max_bytes() {
        // A peer that opens a side channel and sends ridiculous
        // amounts of data with no newline must not OOM the agent.
        let mut wire = vec![b'x'; MAX_HEADER_BYTES + 1];
        wire.push(b'\n');
        let mut reader = BufReader::new(&wire[..]);
        let err = read_header(&mut reader).await.unwrap_err();
        assert!(
            matches!(err, SideChannelError::HeaderTooLong { .. }),
            "expected HeaderTooLong, got {err:?}",
        );
    }

    #[tokio::test]
    async fn read_header_unexpected_eof_before_newline() {
        let wire = b"{\"kind\":\"closure_push\",\"build_id\":1,\"paths\":[]"; // no newline
        let mut reader = BufReader::new(&wire[..]);
        let err = read_header(&mut reader).await.unwrap_err();
        assert!(
            matches!(err, SideChannelError::UnexpectedEof),
            "expected UnexpectedEof, got {err:?}",
        );
    }

    #[tokio::test]
    async fn write_then_read_round_trips_through_pipe() {
        let h = SideChannelHeader {
            kind: SideChannelKind::ClosurePush,
            build_id: 13,
            paths: vec!["/nix/store/foo".into()],
        };
        let (rx, mut tx) = tokio::io::duplex(64 * 1024);
        write_header(&mut tx, &h).await.unwrap();
        // After write, drop the writer so the reader sees EOF on
        // remaining bytes — but only after we've read the header.
        let mut reader = BufReader::new(rx);
        let parsed = read_header(&mut reader).await.unwrap();
        assert_eq!(parsed, h);
    }

    // ---------- dispatch_inbound tests ----------
    //
    // These drive the full agent-side responder through
    // `tokio::io::duplex`: the test spawns the dispatcher on one
    // end and a fake "daemon" on the other that writes a header
    // and binary payload (push) or reads them (pull). Same shape
    // the russh wiring will take, just without russh.
    //
    // The fake nix-store binary follows the same convention as
    // `closure_xfer::tests`: a tiny shell script handling
    // `--import` (cat stdin → sink) and `--export <paths>` (record
    // argv + emit canned bytes from a fixture file).

    use crate::protocol::BuildOutcomeStatus;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use tempfile::tempdir;

    fn fake_nix_store(path: &Path, sink: &Path, argv: &Path, payload: &Path) {
        // Atomic-rename install. Under parallel cargo-test fork
        // pressure, sibling threads' forks can briefly inherit a
        // writable fd to this script before the parent thread closes
        // it; the next exec attempt then sees ETXTBSY. Writing to
        // `.tmp` + chmod + rename means the final path never had a
        // writable fd opened on it.
        //
        // Modes the fake handles:
        //  - `--import`            : cat stdin into `sink`, exit 0
        //  - `--export <paths>`    : record argv into `argv`, emit `payload`
        //  - `--query --requisites <paths>` : emit each requested path
        //    plus a synthetic `<path>-runtime-dep` line per path, so
        //    callers that pre-expand the closure (the agent's
        //    ClosurePull arm) can be observed adding the synthetic
        //    deps to the subsequent `--export` argv.
        let tmp = path.with_extension("tmp");
        {
            let mut f = std::fs::File::create(&tmp).unwrap();
            writeln!(
                f,
                r#"#!/bin/sh
if [ "$1" = "--query" ] && [ "$2" = "--requisites" ]; then
  shift 2
  for a in "$@"; do
    printf '%s\n' "$a"
    printf '%s-runtime-dep\n' "$a"
  done
  exit 0
fi
case "$1" in
  --import)
    cat > "{sink}"
    exit 0
    ;;
  --export)
    : > "{argv}"
    for a in "$@"; do printf '%s\n' "$a" >> "{argv}"; done
    cat "{payload}"
    exit 0
    ;;
esac
exit 99
"#,
                sink = sink.display(),
                argv = argv.display(),
                payload = payload.display(),
            )
            .unwrap();
            f.sync_all().unwrap();
        }
        let mut perm = std::fs::metadata(&tmp).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&tmp, perm).unwrap();
        std::fs::rename(&tmp, path).unwrap();
    }

    #[tokio::test]
    async fn dispatch_inbound_handles_closure_push_end_to_end() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("nix-store");
        let sink = dir.path().join("imp-sink.bin");
        let argv = dir.path().join("imp-argv.txt");
        let payload = dir.path().join("imp-payload.bin");
        std::fs::write(&payload, b"unused").unwrap();
        fake_nix_store(&bin, &sink, &argv, &payload);

        // Daemon-side fixture: a header + binary payload that the
        // agent's dispatcher must consume, parse, and feed into
        // `nix-store --import`.
        let header = SideChannelHeader {
            kind: SideChannelKind::ClosurePush,
            build_id: 17,
            paths: vec!["/nix/store/aaa.drv".into(), "/nix/store/bbb-dep".into()],
        };
        let nar_payload: Vec<u8> = (0u8..=255).chain(std::iter::once(b'X')).collect();

        let (mut daemon_tx_to_agent, mut agent_rx) = {
            let (rx, tx) = tokio::io::duplex(8 * 1024);
            (tx, rx)
        };
        // Agent → daemon direction: must be sized for the agent's
        // reply trailer JSON line. 4 KiB is more than enough.
        let (mut agent_writer, mut daemon_reader) = tokio::io::duplex(4 * 1024);

        // Daemon-side: write header + payload, then drop the
        // writer so the agent sees EOF and `import` finishes.
        let nar_clone = nar_payload.clone();
        let daemon_task = tokio::spawn(async move {
            write_header(&mut daemon_tx_to_agent, &header)
                .await
                .unwrap();
            daemon_tx_to_agent.write_all(&nar_clone).await.unwrap();
            daemon_tx_to_agent.shutdown().await.unwrap();
            // Read the agent's trailer so we can assert on it.
            let mut buf = Vec::new();
            daemon_reader.read_to_end(&mut buf).await.unwrap();
            buf
        });

        let outcome = dispatch_inbound(&bin, &mut agent_rx, &mut agent_writer)
            .await
            .expect("dispatch should succeed for closure_push");
        // Drop the agent writer so daemon's read_to_end completes.
        drop(agent_writer);
        let trailer_bytes = daemon_task.await.unwrap();

        match outcome {
            DispatchOutcome::ClosurePushed {
                build_id,
                paths,
                import,
            } => {
                assert_eq!(build_id, 17);
                assert_eq!(paths, vec!["/nix/store/aaa.drv", "/nix/store/bbb-dep"]);
                assert_eq!(import.status, BuildOutcomeStatus::Success);
                assert_eq!(import.bytes_transferred, nar_payload.len() as u64);
            }
            other => panic!("expected ClosurePushed, got {other:?}"),
        }
        assert_eq!(
            std::fs::read(&sink).unwrap(),
            nar_payload,
            "every byte of the payload must reach `nix-store --import` stdin",
        );
        // The agent must send a single newline-terminated JSON
        // ClosurePushReply with ok=true.
        let trailer = std::str::from_utf8(&trailer_bytes).unwrap();
        let line = trailer
            .lines()
            .next()
            .expect("agent must emit a reply trailer");
        let reply: ClosurePushReply = serde_json::from_str(line).unwrap();
        assert!(reply.ok, "successful import must surface as ok=true");
        assert_eq!(reply.bytes_received, nar_payload.len() as u64);
        assert_eq!(reply.exit_code, Some(0));
    }

    /// Failure path: agent's `nix-store --import` exits non-zero.
    /// The reply trailer must carry `ok=false` and the captured
    /// stderr so the daemon side can surface it instead of just
    /// "broken pipe".
    #[tokio::test]
    async fn dispatch_inbound_closure_push_failure_emits_stderr_in_reply() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("nix-store");
        // Fake nix-store that fails on --import with a recognisable
        // stderr line — mirrors the kind of message a real
        // nix-store would emit on a hash mismatch.
        let body = "#!/bin/sh\n\
            if [ \"$1\" = \"--import\" ]; then\n  \
              cat >/dev/null\n  \
              echo \"error: hash mismatch importing path \
'/nix/store/x'\" >&2\n  \
              exit 1\n\
            fi\n\
            exit 99\n";
        let tmp = bin.with_extension("tmp");
        {
            let mut f = std::fs::File::create(&tmp).unwrap();
            f.write_all(body.as_bytes()).unwrap();
            f.sync_all().unwrap();
        }
        let mut perm = std::fs::metadata(&tmp).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&tmp, perm).unwrap();
        std::fs::rename(&tmp, &bin).unwrap();

        let header = SideChannelHeader {
            kind: SideChannelKind::ClosurePush,
            build_id: 99,
            paths: vec!["/nix/store/x".into()],
        };
        let payload: Vec<u8> = vec![1, 2, 3, 4];
        let (mut daemon_tx_to_agent, mut agent_rx) = {
            let (rx, tx) = tokio::io::duplex(4096);
            (tx, rx)
        };
        let (mut agent_writer, mut daemon_reader) = tokio::io::duplex(4096);

        let payload_clone = payload.clone();
        let daemon_task = tokio::spawn(async move {
            write_header(&mut daemon_tx_to_agent, &header)
                .await
                .unwrap();
            daemon_tx_to_agent.write_all(&payload_clone).await.unwrap();
            daemon_tx_to_agent.shutdown().await.unwrap();
            let mut buf = Vec::new();
            daemon_reader.read_to_end(&mut buf).await.unwrap();
            buf
        });

        let outcome = dispatch_inbound(&bin, &mut agent_rx, &mut agent_writer)
            .await
            .expect("dispatch_inbound itself returns Ok even on import failure");
        drop(agent_writer);
        let trailer = daemon_task.await.unwrap();

        match outcome {
            DispatchOutcome::ClosurePushed { import, .. } => {
                assert_eq!(import.status, BuildOutcomeStatus::Failure);
                assert_eq!(import.exit_code, Some(1));
            }
            other => panic!("expected ClosurePushed, got {other:?}"),
        }

        let line = std::str::from_utf8(&trailer)
            .unwrap()
            .lines()
            .next()
            .unwrap();
        let reply: ClosurePushReply = serde_json::from_str(line).unwrap();
        assert!(!reply.ok, "import failure must surface as ok=false");
        assert_eq!(reply.exit_code, Some(1));
        assert!(
            reply.stderr.contains("hash mismatch"),
            "agent's stderr must round-trip in the reply: {:?}",
            reply.stderr,
        );
    }

    #[tokio::test]
    async fn dispatch_inbound_handles_closure_pull_end_to_end() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("nix-store");
        let sink = dir.path().join("imp-sink.bin");
        let argv = dir.path().join("exp-argv.txt");
        let payload = dir.path().join("exp-payload.bin");
        let canned: Vec<u8> = (0u8..200).cycle().take(50_000).collect();
        std::fs::write(&payload, &canned).unwrap();
        fake_nix_store(&bin, &sink, &argv, &payload);

        // Daemon-side fixture: write a Pull header asking for two
        // output paths, then read back what the agent sends.
        let header = SideChannelHeader {
            kind: SideChannelKind::ClosurePull,
            build_id: 23,
            paths: vec!["/nix/store/zzz-out".into(), "/nix/store/yyy-out-dev".into()],
        };

        let (rx_daemon_to_agent, mut tx_daemon_to_agent) = tokio::io::duplex(64 * 1024);
        let (mut rx_agent_to_daemon, tx_agent_to_daemon) = tokio::io::duplex(64 * 1024);
        let mut agent_reader = BufReader::new(rx_daemon_to_agent);
        let mut agent_writer = tx_agent_to_daemon;

        let header_clone = header.clone();
        let daemon_task = tokio::spawn(async move {
            write_header(&mut tx_daemon_to_agent, &header_clone)
                .await
                .unwrap();
            tx_daemon_to_agent.shutdown().await.unwrap();

            // Read everything the agent emits.
            let mut buf = Vec::new();
            rx_agent_to_daemon.read_to_end(&mut buf).await.unwrap();
            buf
        });

        let outcome = dispatch_inbound(&bin, &mut agent_reader, &mut agent_writer)
            .await
            .expect("dispatch should succeed for closure_pull");
        // Drop the agent writer so the daemon's read_to_end can finish.
        drop(agent_writer);
        let received = daemon_task.await.unwrap();

        match outcome {
            DispatchOutcome::ClosurePulled {
                build_id,
                paths,
                export,
            } => {
                assert_eq!(build_id, 23);
                assert_eq!(paths, header.paths);
                assert_eq!(export.status, BuildOutcomeStatus::Success);
                assert_eq!(export.bytes_transferred, canned.len() as u64);
            }
            other => panic!("expected ClosurePulled, got {other:?}"),
        }
        assert_eq!(
            received, canned,
            "agent's `nix-store --export` stdout must reach the daemon-side reader byte-for-byte",
        );

        // Argv shape: the dispatcher must hand `--export` the FULL
        // closure of the requested output paths, not just the paths
        // themselves. Our fake `nix-store --query --requisites`
        // synthesises a `<path>-runtime-dep` entry per path, so the
        // expanded argv interleaves each header path with its
        // pseudo-dep — proving the dispatcher pre-expanded before
        // exporting.
        let argv_lines = std::fs::read_to_string(&argv).unwrap();
        let lines: Vec<&str> = argv_lines.lines().collect();
        assert_eq!(
            lines,
            vec![
                "--export",
                "/nix/store/zzz-out",
                "/nix/store/zzz-out-runtime-dep",
                "/nix/store/yyy-out-dev",
                "/nix/store/yyy-out-dev-runtime-dep",
            ],
            "ClosurePull must expand to the full runtime closure \
             before exporting; otherwise the daemon's `nix-store \
             --import` rejects outputs whose runtime deps came from \
             a binary cache during the build (see the \
             `OVMF-202602-fd is not valid` regression).",
        );
    }

    #[tokio::test]
    async fn dispatch_inbound_surfaces_header_parse_error_before_spawning() {
        // A garbage header must fail fast — no subprocess spawn,
        // no nix-store invocation. We point the dispatcher at a
        // path that doesn't exist; if it tried to spawn we'd get a
        // Spawn error, but we expect a Header error instead.
        let dir = tempdir().unwrap();
        let bin = dir.path().join("does-not-exist");

        let mut wire: Vec<u8> = b"this is not json\n".to_vec();
        wire.extend_from_slice(b"binary-payload");
        let mut reader = BufReader::new(&wire[..]);
        let mut writer = Vec::new();

        let err = dispatch_inbound(&bin, &mut reader, &mut writer)
            .await
            .expect_err("malformed header must error");
        assert!(
            matches!(err, DispatchError::Header(_)),
            "expected Header error, got {err:?}",
        );
    }
}
