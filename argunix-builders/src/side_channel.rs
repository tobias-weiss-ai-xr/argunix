//! Side-channel framing.
//!
//! A side channel is a fresh russh session channel opened per
//! closure transfer (one in each direction per build). The channel
//! starts with a single newline-terminated JSON header describing
//! what the payload is, followed by raw bytes until channel-close.
//!
//! Currently the protocol carries a single binary kind: the agent
//! spawns `nix-daemon --stdio` and tunnels its stdin/stdout
//! through the channel; the daemon side wires this into a local
//! Unix socket so `nix copy --from/--to unix:///path` can drive it
//! as a normal nix-daemon endpoint. This streams per-file with
//! bounded memory, instead of the legacy `--export | --import`
//! path that OOM'd on multi-GB single-NAR image outputs because
//! `nix-store --import` buffered each NAR for hash verification
//! before extracting.
//!
//! The header still exists so a future protocol change can be
//! introduced behind a new kind without breaking older agents on
//! the wire.

use crate::closure_xfer::ClosureXferError;
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Header that prefaces every side channel. Always exactly one
/// `\n`-terminated JSON line, written before any binary payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SideChannelHeader {
    pub kind: SideChannelKind,
    /// Correlates with the `Build` control message that triggered
    /// this transfer. Receivers log it for trace correlation. May
    /// be 0 for transfers not tied to a specific build (e.g. a
    /// pre-build push initiated outside the eval pipeline).
    pub build_id: i64,
    /// Reserved for future protocol kinds; the current
    /// `NixDaemonStdio` kind ignores this. Kept on the wire so
    /// future variants can carry a path list without a header
    /// schema bump.
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideChannelKind {
    /// Bidirectional. Agent spawns `nix-daemon --stdio` and tunnels
    /// its stdin/stdout through this channel; the daemon side
    /// connects this to a local Unix-domain socket so
    /// `nix copy --from/--to unix:///path` can use it as a normal
    /// nix-daemon endpoint. The daemon protocol streams per-file
    /// with bounded memory, fixing the multi-GB per-NAR OOMs that
    /// broke our previous `--export | --import` path on image-
    /// style outputs.
    NixDaemonStdio,
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

/// Cap on the side-channel header size. The header is a tiny JSON
/// line — no path list — so the cap is purely a defense against a
/// hostile / runaway peer that opens a channel and never sends a
/// newline.
pub const MAX_HEADER_BYTES: usize = 16 * 1024 * 1024;

impl SideChannelHeader {
    /// Encode as a single JSON line with trailing `\n`.
    pub fn encode_line(&self) -> Vec<u8> {
        let mut buf = serde_json::to_vec(self).expect("SideChannelHeader always serialises");
        buf.push(b'\n');
        buf
    }
}

/// Read the header line from `reader`, byte-by-byte until a `\n`
/// arrives. Caps at [`MAX_HEADER_BYTES`] to defend against a peer
/// that opens a channel and never sends a newline.
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

/// Outcome of a side-channel dispatch.
#[derive(Debug)]
pub enum DispatchOutcome {
    /// The agent forwarded daemon-protocol bytes between the
    /// channel and the system `nix-daemon` socket and the channel
    /// was driven to completion (one or both directions closed).
    /// Counts are useful for throughput diagnostics.
    NixDaemonTunneled {
        build_id: i64,
        bytes_to_daemon: u64,
        bytes_from_daemon: u64,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("reading side-channel header: {0}")]
    Header(#[from] SideChannelError),
    #[error("forwarding daemon-protocol bytes: {0}")]
    Xfer(#[from] ClosureXferError),
    #[error("connecting to nix-daemon socket at `{path}`: {source}")]
    DaemonSocket {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Agent-side responder: read the side-channel header, branch on
/// `kind`. Currently only `NixDaemonStdio`: open a connection to
/// the system `nix-daemon` socket and bidirectionally pipe bytes
/// between the channel and the socket.
///
/// **Why connect to the socket instead of spawning
/// `nix-daemon --stdio`:** `nix-daemon --stdio` invoked by an
/// unprivileged user runs an in-process daemon that doesn't have
/// access to the system store DB at `/nix/var/nix/db/db.sqlite`,
/// so every `queryValidPaths` returns false and `nix copy --from`
/// fails with `path '...' does not exist` even though the path is
/// right there. The system daemon socket is world-connectable, the
/// kernel authenticates via `SO_PEERCRED`, and the agent's user is
/// in `trusted-users` (per the NixOS module) — so connecting
/// directly gives full daemon-protocol access without the
/// permission landmine.
///
/// `nix_daemon_socket` is taken explicitly so unit tests can point
/// at a custom Unix socket (e.g. an echo server bound to a temp
/// path) rather than the system socket.
pub async fn dispatch_inbound<R, W>(
    nix_daemon_socket: &Path,
    reader: &mut R,
    writer: &mut W,
) -> Result<DispatchOutcome, DispatchError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let header = read_header(reader).await?;
    match header.kind {
        SideChannelKind::NixDaemonStdio => {
            let socket = tokio::net::UnixStream::connect(nix_daemon_socket)
                .await
                .map_err(|source| DispatchError::DaemonSocket {
                    path: nix_daemon_socket.to_path_buf(),
                    source,
                })?;
            let (sock_reader, sock_writer) = socket.into_split();

            let to_daemon = async move {
                let mut sock_writer = sock_writer;
                let r = tokio::io::copy(reader, &mut sock_writer).await;
                let _ = sock_writer.shutdown().await;
                r
            };
            let from_daemon = async move {
                let mut sock_reader = sock_reader;
                tokio::io::copy(&mut sock_reader, writer).await
            };
            let (to_result, from_result) = tokio::join!(to_daemon, from_daemon);

            Ok(DispatchOutcome::NixDaemonTunneled {
                build_id: header.build_id,
                bytes_to_daemon: to_result.unwrap_or(0),
                bytes_from_daemon: from_result.unwrap_or(0),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use tokio::io::BufReader;

    #[test]
    fn header_round_trip_via_json() {
        let h = SideChannelHeader {
            kind: SideChannelKind::NixDaemonStdio,
            build_id: 42,
            paths: vec![],
        };
        let line = h.encode_line();
        assert!(line.ends_with(b"\n"), "header line must terminate with \\n");
        let s = std::str::from_utf8(&line[..line.len() - 1]).unwrap();
        let back: SideChannelHeader = serde_json::from_str(s).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn header_kind_is_snake_case_on_wire() {
        let h = SideChannelHeader {
            kind: SideChannelKind::NixDaemonStdio,
            build_id: 1,
            paths: vec![],
        };
        let s = String::from_utf8(h.encode_line()).unwrap();
        assert!(
            s.contains("\"kind\":\"nix_daemon_stdio\""),
            "unexpected kind on wire: {s}",
        );
    }

    #[tokio::test]
    async fn read_header_parses_line_then_leaves_payload_intact() {
        let h = SideChannelHeader {
            kind: SideChannelKind::NixDaemonStdio,
            build_id: 7,
            paths: vec![],
        };
        let mut wire: Vec<u8> = h.encode_line();
        wire.extend_from_slice(b"BINARY-PAYLOAD-FOLLOWS");

        let mut reader = BufReader::new(&wire[..]);
        let parsed = read_header(&mut reader).await.expect("header parses");
        assert_eq!(parsed, h);

        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest, b"BINARY-PAYLOAD-FOLLOWS");
    }

    #[tokio::test]
    async fn read_header_caps_at_max_bytes() {
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
        let wire = b"{\"kind\":\"nix_daemon_stdio\",\"build_id\":1"; // no newline
        let mut reader = BufReader::new(&wire[..]);
        let err = read_header(&mut reader).await.unwrap_err();
        assert!(
            matches!(err, SideChannelError::UnexpectedEof),
            "expected UnexpectedEof, got {err:?}",
        );
    }

    /// End-to-end: dispatch_inbound connects to the configured
    /// Unix socket and bidirectionally pipes bytes. Stand up an
    /// echo server bound to a temp socket — every byte written
    /// must come back through the channel.
    #[tokio::test]
    async fn dispatch_inbound_tunnels_bytes_through_socket() {
        let dir = tempdir().unwrap();
        let socket_path = dir.path().join("fake-daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        // Echo server: accept one connection, copy stdin → stdout.
        let echo_task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (mut r, mut w) = stream.split();
            let _ = tokio::io::copy(&mut r, &mut w).await;
        });

        let header = SideChannelHeader {
            kind: SideChannelKind::NixDaemonStdio,
            build_id: 11,
            paths: vec![],
        };
        let payload: Vec<u8> = (0u8..=255).chain(0..200).collect();

        let (rx_d_to_a, mut tx_d_to_a) = tokio::io::duplex(64 * 1024);
        let (mut rx_a_to_d, tx_a_to_d) = tokio::io::duplex(64 * 1024);
        let mut agent_reader = BufReader::new(rx_d_to_a);
        let mut agent_writer = tx_a_to_d;

        let payload_clone = payload.clone();
        let daemon_task = tokio::spawn(async move {
            write_header(&mut tx_d_to_a, &header).await.unwrap();
            tx_d_to_a.write_all(&payload_clone).await.unwrap();
            tx_d_to_a.shutdown().await.unwrap();
            let mut buf = Vec::new();
            rx_a_to_d.read_to_end(&mut buf).await.unwrap();
            buf
        });

        let outcome = dispatch_inbound(&socket_path, &mut agent_reader, &mut agent_writer)
            .await
            .expect("dispatch should succeed");
        drop(agent_writer);
        let received = daemon_task.await.unwrap();
        let _ = echo_task.await;

        match outcome {
            DispatchOutcome::NixDaemonTunneled {
                build_id,
                bytes_to_daemon,
                bytes_from_daemon,
            } => {
                assert_eq!(build_id, 11);
                assert_eq!(bytes_to_daemon, payload.len() as u64);
                assert_eq!(bytes_from_daemon, payload.len() as u64);
            }
        }
        assert_eq!(
            received, payload,
            "every byte must round-trip through the tunneled socket",
        );
    }

    #[tokio::test]
    async fn dispatch_inbound_surfaces_header_parse_error_before_spawning() {
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
