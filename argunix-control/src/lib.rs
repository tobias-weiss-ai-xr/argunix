//! Unix-socket control protocol shared by the daemon and `argunixctl`.
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

/// Inbound request from `argunixctl`. Tagged on `command` so adding
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
    /// List every known builder (registered + revoked) for `argunixctl
    /// builders`. Response `details` is an array of builder snapshot
    /// objects (see [`BuilderInfo`]).
    BuildersList,
    /// Revoke a builder by name. Sets `revoked_at` in sqlite and, if
    /// the builder is currently connected, sends a `kick` message and
    /// disconnects the SSH session. Subsequent reconnects with the
    /// existing pubkey fail; the agent has to re-enroll with a fresh
    /// enrollment token.
    BuildersRevoke { name: String },
    /// Rename `old → new`. Fails if `old` doesn't exist or if `new`
    /// already exists.
    BuildersRename { old: String, new: String },
    /// Test-only (M14b VM test driver): take an existing local drv
    /// path, push its closure to the named builder over a side
    /// channel, send a `Build` control message, drain the lifecycle,
    /// pull the output closure back, and register a transient
    /// gcroot. Returns the realised output paths on success.
    ///
    /// Not intended for operator use — the read-only worker pipeline
    /// is the supported path. Used by the NixOS test that exercises
    /// the dynamic-pool transport without standing up a fake forge.
    TestDispatchDrv { drv_path: String, builder: String },
}

/// One row of the `argunixctl builders` output.
///
/// Mirrors `argunix-store::BuilderRecord` plus the runtime-only
/// `connected` / `in_flight` fields the registry adds. Lives here
/// rather than in `argunix-store` so `argunixctl` doesn't have to
/// pull in sqlx.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BuilderInfo {
    pub id: i64,
    pub name: String,
    pub systems: Vec<String>,
    pub features: Vec<String>,
    pub max_jobs: u32,
    pub nix_version: String,
    pub enrolled_at: String,
    pub last_seen: String,
    /// `Some(rfc3339)` if the operator revoked it; `None` for active rows.
    pub revoked_at: Option<String>,
    /// True if the registry currently holds an SSH session for this
    /// builder. False for revoked rows or builders that are simply
    /// not connected right now.
    pub connected: bool,
    /// Builds in progress on this builder. Always `0` for revoked /
    /// disconnected rows.
    pub in_flight: u32,
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
/// connection each time so `argunixctl` invocations don't have to
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
            config_path: Some(PathBuf::from("/etc/argunix.yaml")),
        };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(
            s,
            r#"{"command":"reload","config_path":"/etc/argunix.yaml"}"#
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

    #[test]
    fn builders_list_request_round_trips() {
        let r = Request::BuildersList;
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, r#"{"command":"builders-list"}"#);
        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn builders_revoke_request_round_trips() {
        let r = Request::BuildersRevoke {
            name: "bobs-mini".into(),
        };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, r#"{"command":"builders-revoke","name":"bobs-mini"}"#);
        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn builders_rename_request_round_trips() {
        let r = Request::BuildersRename {
            old: "old".into(),
            new: "new".into(),
        };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(
            s,
            r#"{"command":"builders-rename","old":"old","new":"new"}"#
        );
        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn builder_info_round_trips() {
        let info = BuilderInfo {
            id: 7,
            name: "bobs-mini".into(),
            systems: vec!["aarch64-darwin".into()],
            features: vec!["big-parallel".into()],
            max_jobs: 2,
            nix_version: "2.18.1".into(),
            enrolled_at: "2026-05-04T10:00:00Z".into(),
            last_seen: "2026-05-04T10:30:00Z".into(),
            revoked_at: None,
            connected: true,
            in_flight: 1,
        };
        let s = serde_json::to_string(&info).unwrap();
        let back: BuilderInfo = serde_json::from_str(&s).unwrap();
        assert_eq!(back, info);
    }
}
