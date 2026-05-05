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
//! This module owns the **codec only** — encoding, decoding, and
//! the IO helpers to read/write a header from/to an `AsyncRead` /
//! `AsyncWrite`. The actual `nix-store --import` / `--export` work
//! lives on the agent (`medusa-builder`) and the daemon
//! (`medusa-build`) respectively, since each side only needs one
//! direction's subprocess plumbing.

use serde::{Deserialize, Serialize};
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

/// Generously above any realistic header size — `paths` may have a
/// few hundred entries for a fat closure but each is ~80 chars; 64K
/// is enough headroom while still tripping fast on garbage / a
/// misbehaving peer.
pub const MAX_HEADER_BYTES: usize = 64 * 1024;

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
}
