//! Dial medusa, authenticate, hello, heartbeat, accept build channels.
//!
//! Runs forever (with reconnect-and-backoff) until the supplied
//! `shutdown` future fires. On clean shutdown, sends a `shutdown`
//! control message before closing the session so medusa logs the
//! event at INFO and immediately marks the registry entry
//! `Disconnecting`.

use crate::identity::PersistedKey;
use medusa_builders::ControlMessage;
use medusa_domain::{BuilderCapabilities, BuilderName};
use russh::ChannelMsg;
use russh::client::{self, Handle, Handler, Msg as ClientMsg, Session as ClientSession};
use russh::keys::ssh_key::{self, PrivateKey};
use russh::keys::{Algorithm, PrivateKeyWithHashAlg};
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("converting identity to russh key: {0}")]
    IdentityKey(String),
    #[error("connecting to medusa at {addr}: {source}")]
    Connect {
        addr: SocketAddr,
        #[source]
        source: russh::Error,
    },
    #[error("ssh: {0}")]
    Ssh(#[from] russh::Error),
    #[error("auth refused: pubkey rejected and no enrollment token configured")]
    PubkeyRejectedNoToken,
    #[error("auth refused: pubkey AND token both rejected")]
    AllAuthRejected,
}

/// Inputs to the agent loop. The caller assembles these from CLI /
/// config; tests construct them directly.
#[derive(Clone)]
pub struct AgentConfig {
    pub medusa: SocketAddr,
    pub identity: PersistedKey,
    /// `None` after first successful enrollment; the caller wipes
    /// the field once the daemon's TOFU row is in place.
    pub enrollment_token: Option<Arc<Vec<u8>>>,
    pub name: BuilderName,
    pub capabilities: BuilderCapabilities,
    /// Initial reconnect backoff. Doubles up to 30s on repeated failures;
    /// resets to this value on every successful hello.
    pub reconnect_initial_backoff: Duration,
    /// Command to spawn for each inbound build channel. Defaults to
    /// `["nix-daemon", "--stdio"]`. medusa dispatches via
    /// `--builders ssh-ng://…`, and the `ssh-ng` scheme expects the
    /// remote end to speak the nix daemon protocol over stdio — not
    /// the legacy `nix-store --serve` protocol used by `ssh://`.
    /// Tests substitute a stub like `["cat"]` so the byte-pump can be
    /// exercised without a real nix store.
    pub nix_serve_command: Arc<Vec<String>>,
    /// Path under which the agent pins medusa's SSH host key (TOFU).
    /// First successful connect writes the presented pubkey here in
    /// OpenSSH single-line format; subsequent connects refuse if the
    /// presented key does not match.
    ///
    /// `None` disables TOFU entirely — used by the integration tests
    /// where the daemon's host key is regenerated per VM. Production
    /// deployments should always set this.
    pub medusa_host_key_path: Option<PathBuf>,
}

impl AgentConfig {
    pub fn default_backoff() -> Duration {
        Duration::from_secs(2)
    }
    pub fn default_nix_serve_command() -> Arc<Vec<String>> {
        Arc::new(vec!["nix-daemon".into(), "--stdio".into()])
    }
}

/// The agent's main loop. Returns `Ok(())` on clean shutdown.
/// Reconnects with exponential backoff on transport / auth failures
/// (auth failure that's clearly fatal — no token configured — bails
/// out instead of looping forever).
pub async fn run<S: Future<Output = ()>>(cfg: AgentConfig, shutdown: S) -> Result<(), AgentError> {
    tokio::pin!(shutdown);
    let mut backoff = cfg.reconnect_initial_backoff;
    loop {
        // The connect-and-serve cycle returns either:
        //   - Ok(true)  — clean shutdown signalled by the caller
        //   - Ok(false) — disconnected; the caller wants a reconnect
        //   - Err(_)    — fatal error (bad token + bad pubkey)
        let cycle = serve_one_connection(&cfg, &mut shutdown);
        match cycle.await {
            Ok(true) => {
                tracing::info!("agent shutdown complete");
                return Ok(());
            }
            Ok(false) => {
                tracing::info!(
                    backoff_ms = backoff.as_millis() as u64,
                    "agent reconnecting after disconnect",
                );
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = &mut shutdown => return Ok(()),
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
            Err(AgentError::PubkeyRejectedNoToken) => {
                // Fatal — the only reason a builder would be in this
                // state is operator revocation without re-issuing a
                // token. Nothing to retry.
                return Err(AgentError::PubkeyRejectedNoToken);
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    backoff_ms = backoff.as_millis() as u64,
                    "agent connection cycle errored; backing off",
                );
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = &mut shutdown => return Ok(()),
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

async fn serve_one_connection(
    cfg: &AgentConfig,
    shutdown: &mut std::pin::Pin<&mut impl Future<Output = ()>>,
) -> Result<bool, AgentError> {
    // Build the russh PrivateKey from our 32-byte seed (same shape
    // as the server-side host key conversion).
    let key = identity_to_russh_private_key(&cfg.identity)
        .map_err(|e| AgentError::IdentityKey(e.to_string()))?;
    let key = Arc::new(key);

    let client_cfg = Arc::new(client::Config::default());
    let handler = AgentClient {
        nix_serve_cmd: cfg.nix_serve_command.clone(),
        medusa_host_key_path: cfg.medusa_host_key_path.clone(),
    };
    let mut session = client::connect(client_cfg, cfg.medusa, handler)
        .await
        .map_err(|source| AgentError::Connect {
            addr: cfg.medusa,
            source,
        })?;

    // Try pubkey first. On the daemon's first contact the row
    // doesn't exist yet → pubkey auth fails → fall back to token.
    // After successful enrollment the daemon's TOFU row matches our
    // pubkey on every subsequent connect.
    let user = "medusa-builder";
    let pubkey_attempt = session
        .authenticate_publickey(user, PrivateKeyWithHashAlg::new(key, None))
        .await
        .map_err(AgentError::Ssh)?;
    if !pubkey_attempt.success() {
        match &cfg.enrollment_token {
            None => return Err(AgentError::PubkeyRejectedNoToken),
            Some(tok) => {
                let token_str = std::str::from_utf8(tok.as_ref()).unwrap_or("");
                let pw = session
                    .authenticate_password(user, token_str)
                    .await
                    .map_err(AgentError::Ssh)?;
                if !pw.success() {
                    return Err(AgentError::AllAuthRejected);
                }
                tracing::info!("enrolled with token; subsequent connects will use pubkey");
            }
        }
    } else {
        tracing::info!("authenticated via pubkey");
    }

    // Open the control channel and send hello.
    let mut channel = session.channel_open_session().await?;
    let hello = ControlMessage::Hello {
        name: cfg.name.clone(),
        systems: cfg.capabilities.systems.clone(),
        features: cfg.capabilities.features.clone(),
        max_jobs: cfg.capabilities.max_jobs,
        nix_version: cfg.capabilities.nix_version.clone(),
    };
    channel.data(&hello.encode_line()[..]).await?;

    // Drain control until welcome arrives. Drop trailing data into
    // the heartbeat loop's framer.
    let welcome_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut welcome_seen = false;
    let mut framer = medusa_builders::LineFramer::new();
    while tokio::time::Instant::now() < welcome_deadline {
        let remaining = welcome_deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, channel.wait()).await {
            Ok(Some(ChannelMsg::Data { data })) => {
                for parsed in framer.extend(&data) {
                    if let Ok(ControlMessage::Welcome { builder_id }) = parsed {
                        tracing::info!(builder_id = %builder_id, "registered with medusa");
                        welcome_seen = true;
                        break;
                    }
                }
                if welcome_seen {
                    break;
                }
            }
            Ok(Some(ChannelMsg::Close)) | Ok(None) | Err(_) => break,
            _ => continue,
        }
    }
    if !welcome_seen {
        // medusa closed the channel without welcoming us. Force a
        // reconnect (the next attempt may fare differently).
        return Ok(false);
    }

    // Heartbeat + shutdown loop.
    let mut hb_interval = tokio::time::interval(Duration::from_secs(30));
    hb_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Skip the immediate first tick.
    hb_interval.tick().await;

    loop {
        tokio::select! {
            _ = &mut *shutdown => {
                let bye = ControlMessage::Shutdown {
                    reason: "agent stopping".into(),
                    drain: false,
                };
                let _ = channel.data(&bye.encode_line()[..]).await;
                let _ = channel.close().await;
                let _ = session.disconnect(
                    russh::Disconnect::ByApplication,
                    "agent shutdown",
                    "en",
                ).await;
                return Ok(true);
            }
            _ = hb_interval.tick() => {
                let hb = ControlMessage::Heartbeat {
                    ts: chrono::Utc::now().timestamp(),
                    load: None,
                };
                if channel.data(&hb.encode_line()[..]).await.is_err() {
                    return Ok(false);
                }
            }
            ev = channel.wait() => match ev {
                Some(ChannelMsg::Data { data }) => {
                    for parsed in framer.extend(&data) {
                        match parsed {
                            Ok(ControlMessage::Kick { reason }) => {
                                tracing::warn!(reason = %reason, "kicked by medusa; reconnect after backoff");
                                return Ok(false);
                            }
                            Ok(other) => {
                                tracing::warn!(?other, "ignoring unexpected server-to-builder message");
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "control line dropped");
                            }
                        }
                    }
                }
                Some(ChannelMsg::Close) | None => return Ok(false),
                Some(_) => continue,
            }
        }
    }
}

/// russh client Handler. Dispatches inbound build channels (medusa-
/// initiated) to a `nix-daemon --stdio` subprocess (or whatever
/// `nix_serve_command` was set to) and pumps bytes between the
/// channel and the subprocess's stdio.
#[derive(Clone)]
struct AgentClient {
    nix_serve_cmd: Arc<Vec<String>>,
    medusa_host_key_path: Option<PathBuf>,
}

impl Handler for AgentClient {
    type Error = russh::Error;
    async fn check_server_key(
        &mut self,
        presented: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        // TOFU on medusa's SSH host key. First successful connect
        // pins the key under `medusa_host_key_path`; later connects
        // refuse on mismatch. Operator recovery: delete the pinned
        // file, then restart the agent.
        let Some(path) = self.medusa_host_key_path.as_deref() else {
            // Tests: TOFU disabled, accept any key.
            return Ok(true);
        };
        match check_or_pin_host_key(path, presented).await {
            Ok(()) => Ok(true),
            Err(e) => {
                tracing::error!(
                    error = %e,
                    pinned = %path.display(),
                    "medusa host key TOFU check failed; refusing connection",
                );
                Ok(false)
            }
        }
    }
    async fn server_channel_open_session(
        &mut self,
        channel: russh::Channel<ClientMsg>,
        _session: &mut ClientSession,
    ) -> Result<(), Self::Error> {
        let cmd = self.nix_serve_cmd.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_build_channel(channel, cmd).await {
                tracing::warn!(error = %e, "build-channel proxy ended with error");
            }
        });
        Ok(())
    }
}

/// Spawn the configured nix subprocess (`nix-daemon --stdio` by
/// default) and bidirectionally pump bytes between the SSH build
/// channel and the subprocess's stdin/stdout. medusa-side never
/// parses the wire bytes — they're the standard nix daemon protocol
/// that medusa's own `nix-store --realise --builders ssh-ng://…`
/// worker speaks.
async fn serve_build_channel(
    mut channel: russh::Channel<ClientMsg>,
    cmd: Arc<Vec<String>>,
) -> Result<(), std::io::Error> {
    if cmd.is_empty() {
        return Err(std::io::Error::other("nix_serve_command is empty"));
    }
    let started_at = std::time::Instant::now();
    let channel_id: u32 = channel.id().into();
    let mut child = tokio::process::Command::new(&cmd[0])
        .args(cmd[1..].iter())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let pid = child.id();
    tracing::info!(
        channel = channel_id,
        pid = pid,
        cmd = ?cmd.as_slice(),
        "build channel opened; spawned nix subprocess",
    );
    let mut stdin = child.stdin.take().expect("stdin piped");
    let mut stdout = child.stdout.take().expect("stdout piped");

    let mut buf = vec![0u8; 32 * 1024];
    let mut stdin_open = true;
    loop {
        tokio::select! {
            // SSH channel → subprocess stdin
            ev = channel.wait() => match ev {
                Some(ChannelMsg::Data { data }) => {
                    if !stdin_open {
                        continue;
                    }
                    if stdin.write_all(&data).await.is_err() {
                        stdin_open = false;
                        continue;
                    }
                    if stdin.flush().await.is_err() {
                        stdin_open = false;
                    }
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                    let _ = stdin.shutdown().await;
                    break;
                }
                Some(_) => continue,
            },
            // subprocess stdout → SSH channel
            r = stdout.read(&mut buf) => match r {
                Ok(0) => break,
                Ok(n) => {
                    if channel.data(&buf[..n]).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }
    // After break: drain any remaining stdout, then half-close + close.
    while let Ok(Ok(n)) =
        tokio::time::timeout(Duration::from_millis(500), stdout.read(&mut buf)).await
    {
        if n == 0 {
            break;
        }
        if channel.data(&buf[..n]).await.is_err() {
            break;
        }
    }
    let _ = channel.eof().await;
    let _ = channel.close().await;
    let exit = child.wait().await;
    tracing::info!(
        channel = channel_id,
        pid = pid,
        elapsed_ms = started_at.elapsed().as_millis() as u64,
        exit = ?exit.as_ref().ok().and_then(|s| s.code()),
        "build channel closed; nix subprocess exited",
    );
    Ok(())
}

// Suppress unused-import warning on `Handle` when only Handler is in use.
#[allow(dead_code)]
type _HandleAlias<T> = Handle<T>;

#[derive(Debug, thiserror::Error)]
enum HostKeyTofuError {
    #[error("serialising presented host pubkey: {0}")]
    Serialize(#[source] ssh_key::Error),
    #[error("reading pinned host pubkey at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("writing pinned host pubkey at {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "medusa host pubkey changed — pinned `{pinned}` but server presented `{presented}`. \
         If this is intentional, delete the pinned file and restart the agent."
    )]
    Mismatch { pinned: String, presented: String },
}

/// TOFU on the medusa host key. Reads the pinned key (if present) and
/// compares against `presented`. If absent, writes the presented key
/// in OpenSSH single-line format so the next connect compares against it.
async fn check_or_pin_host_key(
    path: &Path,
    presented: &ssh_key::PublicKey,
) -> Result<(), HostKeyTofuError> {
    let presented_line = presented
        .to_openssh()
        .map_err(HostKeyTofuError::Serialize)?;
    let presented_line = presented_line.trim().to_string();

    match tokio::fs::read_to_string(path).await {
        Ok(contents) => {
            let pinned = contents.trim().to_string();
            if pinned == presented_line {
                Ok(())
            } else {
                Err(HostKeyTofuError::Mismatch {
                    pinned,
                    presented: presented_line,
                })
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // First-contact: pin atomically via tmp+rename so a crash
            // mid-write doesn't leave a half-pinned key on disk.
            if let Some(parent) = path.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            let tmp = path.with_extension("pub.tmp");
            tokio::fs::write(&tmp, format!("{presented_line}\n"))
                .await
                .map_err(|source| HostKeyTofuError::Write {
                    path: tmp.clone(),
                    source,
                })?;
            tokio::fs::rename(&tmp, path)
                .await
                .map_err(|source| HostKeyTofuError::Write {
                    path: path.to_path_buf(),
                    source,
                })?;
            tracing::info!(
                pinned = %path.display(),
                "pinned medusa host key on first contact (TOFU)",
            );
            Ok(())
        }
        Err(e) => Err(HostKeyTofuError::Read {
            path: path.to_path_buf(),
            source: e,
        }),
    }
}

fn identity_to_russh_private_key(
    identity: &PersistedKey,
) -> Result<PrivateKey, russh::keys::Error> {
    let _ = Algorithm::Ed25519; // keep import live across feature toggles
    let seed = identity.signing_key().to_bytes();
    let kp = ssh_key::private::Ed25519Keypair::from_seed(&seed);
    let kpd = ssh_key::private::KeypairData::Ed25519(kp);
    PrivateKey::new(kpd, "medusa-builder").map_err(Into::into)
}

#[cfg(test)]
mod host_key_tofu_tests {
    use super::*;

    fn pubkey() -> ssh_key::PublicKey {
        // Minimal ed25519 key derived from a deterministic seed; only
        // the bytes matter — no real authentication happens here.
        let kp = ssh_key::private::Ed25519Keypair::from_seed(&[7u8; 32]);
        let kpd = ssh_key::private::KeypairData::Ed25519(kp);
        let priv_key = ssh_key::PrivateKey::new(kpd, "test").unwrap();
        priv_key.public_key().clone()
    }

    fn other_pubkey() -> ssh_key::PublicKey {
        let kp = ssh_key::private::Ed25519Keypair::from_seed(&[42u8; 32]);
        let kpd = ssh_key::private::KeypairData::Ed25519(kp);
        let priv_key = ssh_key::PrivateKey::new(kpd, "test").unwrap();
        priv_key.public_key().clone()
    }

    #[tokio::test]
    async fn first_contact_pins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("medusa-host-key.pub");
        let key = pubkey();

        check_or_pin_host_key(&path, &key).await.unwrap();
        assert!(path.exists(), "first contact must pin the key file");
        let pinned = tokio::fs::read_to_string(&path).await.unwrap();
        let expected = key.to_openssh().unwrap();
        assert_eq!(pinned.trim(), expected.trim());
    }

    #[tokio::test]
    async fn matching_key_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("medusa-host-key.pub");
        let key = pubkey();

        check_or_pin_host_key(&path, &key).await.unwrap();
        // Second call with the same key must succeed.
        check_or_pin_host_key(&path, &key).await.unwrap();
    }

    #[tokio::test]
    async fn mismatched_key_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("medusa-host-key.pub");

        check_or_pin_host_key(&path, &pubkey()).await.unwrap();
        let err = check_or_pin_host_key(&path, &other_pubkey())
            .await
            .unwrap_err();
        assert!(
            matches!(err, HostKeyTofuError::Mismatch { .. }),
            "expected Mismatch, got {err:?}",
        );
    }
}
