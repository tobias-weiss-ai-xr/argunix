//! Unix-socket control protocol shared by the daemon and `medusactl`.
//!
//! Wire format: JSON-lines. One request per line, one response per
//! line. Both sides write a `\n`-terminated JSON object and then
//! flush. This is intentionally text-based so an operator can poke
//! the socket with `socat`/`nc` for ad-hoc debugging.
//!
//! Per Q76 the framing is "tiny RPC … probably bincode + length-
//! prefix; could be JSON-lines for debuggability — leaning JSON-
//! lines". We picked the latter.
//!
//! The protocol is request/response with no pipelining: client sends
//! one request, server sends one response, connection closes. Adding
//! streaming responses (e.g. `tail-log`) is a future extension —
//! we'd switch to a sequence of newline-delimited frames terminated
//! by `{"status":"end"}`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Inbound request from `medusactl`. Tagged on `command` so adding
/// new operations doesn't break wire compatibility with old clients.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "command", rename_all = "kebab-case")]
pub enum Request {
    /// Re-read the YAML config from disk and atomically swap into the
    /// running daemon. `config_path` is optional — when omitted, the
    /// daemon re-reads from the path it was started with. Operators
    /// running on NixOS where each rebuild produces a fresh path in
    /// `/nix/store/...` will normally pass it explicitly so the
    /// reload picks up the new generation's config.
    Reload {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config_path: Option<PathBuf>,
    },
    /// Snapshot of daemon health: uptime, configured forge/repo
    /// counts, paused forges, queue depth.
    Status,
}

/// Server's reply. `status` is `"ok"` on success or `"error"` on
/// failure; `details` carries operation-specific data on success,
/// `message` carries the human-readable explanation on failure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum Response {
    Ok {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
    },
    Error {
        message: String,
    },
}

impl Response {
    pub fn ok() -> Self {
        Self::Ok { details: None }
    }
    pub fn ok_with(details: serde_json::Value) -> Self {
        Self::Ok {
            details: Some(details),
        }
    }
    pub fn error(message: impl Into<String>) -> Self {
        Self::Error {
            message: message.into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("connecting to control socket `{path}`: {error}")]
    Connect {
        path: PathBuf,
        #[source]
        error: std::io::Error,
    },
    #[error("writing request: {0}")]
    Write(#[source] std::io::Error),
    #[error("reading response: {0}")]
    Read(#[source] std::io::Error),
    #[error("server closed the connection without responding")]
    EmptyResponse,
    #[error("decoding response: {0}")]
    Decode(#[source] serde_json::Error),
    #[error("encoding request: {0}")]
    Encode(#[source] serde_json::Error),
}

/// Thin async client. One round-trip per call; opens a fresh
/// connection each time so `medusactl` invocations don't have to
/// share state.
pub async fn send(socket: &std::path::Path, req: &Request) -> Result<Response, ClientError> {
    let stream = UnixStream::connect(socket)
        .await
        .map_err(|e| ClientError::Connect {
            path: socket.to_path_buf(),
            error: e,
        })?;
    let (read_half, mut write_half) = stream.into_split();

    let mut line = serde_json::to_vec(req).map_err(ClientError::Encode)?;
    line.push(b'\n');
    write_half
        .write_all(&line)
        .await
        .map_err(ClientError::Write)?;
    write_half.shutdown().await.map_err(ClientError::Write)?;

    let mut reader = BufReader::new(read_half);
    let mut buf = String::new();
    let n = reader
        .read_line(&mut buf)
        .await
        .map_err(ClientError::Read)?;
    if n == 0 {
        return Err(ClientError::EmptyResponse);
    }
    serde_json::from_str(buf.trim_end_matches('\n')).map_err(ClientError::Decode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_serializes_kebab_case_tag() {
        let r = Request::Reload {
            config_path: Some(PathBuf::from("/etc/medusa.yaml")),
        };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(
            s,
            r#"{"command":"reload","config_path":"/etc/medusa.yaml"}"#
        );
    }

    #[test]
    fn reload_without_path_omits_field() {
        let r = Request::Reload { config_path: None };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, r#"{"command":"reload"}"#);
        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn status_request_round_trips() {
        let r = Request::Status;
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, r#"{"command":"status"}"#);
        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(back, Request::Status);
    }

    #[test]
    fn ok_response_serializes_without_details_when_none() {
        let s = serde_json::to_string(&Response::ok()).unwrap();
        assert_eq!(s, r#"{"status":"ok"}"#);
    }

    #[test]
    fn ok_response_with_details_round_trips() {
        let resp = Response::ok_with(serde_json::json!({"forges": 3}));
        let s = serde_json::to_string(&resp).unwrap();
        assert_eq!(s, r#"{"status":"ok","details":{"forges":3}}"#);
        let back: Response = serde_json::from_str(&s).unwrap();
        assert_eq!(back, resp);
    }

    #[test]
    fn error_response_round_trips() {
        let r = Response::error("config invalid");
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, r#"{"status":"error","message":"config invalid"}"#);
        let back: Response = serde_json::from_str(&s).unwrap();
        assert_eq!(back, r);
    }
}
