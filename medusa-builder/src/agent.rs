//! Dial medusa, authenticate, hello, heartbeat, accept build channels.
//!
//! Runs forever (with reconnect-and-backoff) until the supplied
//! `shutdown` future fires. On clean shutdown, sends a `shutdown`
//! control message before closing the session so medusa logs the
//! event at INFO and immediately marks the registry entry
//! `Disconnecting`.

use crate::identity::PersistedKey;
use base64::Engine as _;
use medusa_builders::{BuildOutcomeStatus, ControlMessage, dispatch_inbound, with_channel_io};
use medusa_domain::{BuilderCapabilities, BuilderName};
use russh::ChannelMsg;
use russh::client::{self, Handle, Handler, Msg as ClientMsg, Session as ClientSession};
use russh::keys::ssh_key::{self, PrivateKey};
use russh::keys::{Algorithm, PrivateKeyWithHashAlg};
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

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
    /// Path to the local `nix-store` binary (M14b). Each inbound
    /// side channel either runs `<nix_store_bin> --import` (closure
    /// push from daemon) or `<nix_store_bin> --export <paths>`
    /// (closure pull for outputs). Tests substitute a fake
    /// shell-script binary; production deployments leave it at the
    /// default `"nix-store"` resolved on the agent's `PATH`.
    pub nix_store_bin: PathBuf,
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
    pub fn default_nix_store_bin() -> PathBuf {
        PathBuf::from("nix-store")
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
        nix_store_bin: Arc::new(cfg.nix_store_bin.clone()),
        medusa_host_key_path: cfg.medusa_host_key_path.clone(),
        close_signals: Arc::new(StdMutex::new(HashMap::new())),
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

    // M14b: outbound control queue. Build tasks send `BuildStarted /
    // BuildLogChunk / BuildFinished` here; the main loop drains the
    // queue and writes to the SSH channel (which only one task can hold
    // a `&mut` to at a time).
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    // M14b: in-flight build map. `Abort` looks up the oneshot for the
    // matching `build_id` and signals the build task to SIGKILL its
    // `nix-store --realise` child.
    let in_flight: Arc<StdMutex<HashMap<i64, oneshot::Sender<()>>>> =
        Arc::new(StdMutex::new(HashMap::new()));

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
            // Outbound from build tasks → SSH channel.
            Some(bytes) = out_rx.recv() => {
                if channel.data(&bytes[..]).await.is_err() {
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
                            Ok(ControlMessage::Build {
                                build_id,
                                drv_path,
                                gc_root,
                                timeout_secs,
                                max_log_bytes,
                            }) => {
                                let (abort_tx, abort_rx) = oneshot::channel::<()>();
                                in_flight
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .insert(build_id, abort_tx);
                                let nix_store_bin = cfg.nix_store_bin.clone();
                                let out_tx = out_tx.clone();
                                let in_flight_for_task = in_flight.clone();
                                tokio::spawn(async move {
                                    handle_build(
                                        build_id,
                                        drv_path,
                                        gc_root,
                                        timeout_secs,
                                        max_log_bytes,
                                        &nix_store_bin,
                                        out_tx,
                                        abort_rx,
                                    ).await;
                                    in_flight_for_task
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .remove(&build_id);
                                });
                            }
                            Ok(ControlMessage::Abort { build_id }) => {
                                let tx = in_flight
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .remove(&build_id);
                                if let Some(tx) = tx {
                                    let _ = tx.send(());
                                } else {
                                    tracing::debug!(build_id, "abort for unknown build_id; ignoring");
                                }
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

/// M14b agent-side handler for one `Build` dispatch. Spawns
/// `nix-store --realise <drv> [--add-root <gc_root>] [--option build-timeout <s>]`
/// (no `--builders` — the agent is the builder), pumps stderr into
/// `BuildLogChunk` frames (base64, capped at `max_log_bytes`), captures
/// stdout as the realised output paths, and sends `BuildStarted` /
/// `BuildFinished` over `out_tx`. On receipt of a signal on `abort_rx`,
/// SIGKILLs the child and reports `BuildOutcomeStatus::Killed`.
///
/// Errors that prevent the subprocess from running are surfaced as
/// `BuildFinished{status: SpawnFailed}` so the daemon side never hangs
/// waiting for a terminal message.
async fn handle_build(
    build_id: i64,
    drv_path: String,
    gc_root: Option<String>,
    timeout_secs: u64,
    max_log_bytes: u64,
    nix_store_bin: &Path,
    out_tx: mpsc::UnboundedSender<Vec<u8>>,
    abort_rx: oneshot::Receiver<()>,
) {
    let mut cmd = Command::new(nix_store_bin);
    cmd.arg("--realise");
    if let Some(ref g) = gc_root {
        cmd.arg("--add-root").arg(g);
    }
    if timeout_secs > 0 {
        // `nix-store` doesn't take `--timeout`; that's a `nix build`
        // flag. Use the equivalent settings-style override so the
        // option lands without an "unknown flag" parse error.
        cmd.arg("--option")
            .arg("build-timeout")
            .arg(timeout_secs.to_string());
    }
    cmd.arg(&drv_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = match spawn_retrying_etxtbsy(&mut cmd) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(
                build_id,
                error = %e,
                "spawning nix-store --realise failed",
            );
            let _ = out_tx.send(
                ControlMessage::BuildFinished {
                    build_id,
                    status: BuildOutcomeStatus::SpawnFailed,
                    exit_code: None,
                    output_paths: Vec::new(),
                    log_truncated: false,
                }
                .encode_line(),
            );
            return;
        }
    };
    let pid = child.id();
    let _ = out_tx.send(ControlMessage::BuildStarted { build_id, pid }.encode_line());

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    // Stderr → BuildLogChunk frames, capped at max_log_bytes raw bytes.
    let stderr_tx = out_tx.clone();
    let stderr_task = tokio::spawn(async move {
        pump_stderr_as_chunks(build_id, stderr, max_log_bytes, stderr_tx).await
    });

    // Stdout → output path list. nix-store --realise prints one path
    // per line; we collect after EOF.
    let stdout_task = tokio::spawn(async move {
        let mut buf = String::new();
        let mut s = stdout;
        let _ = s.read_to_string(&mut buf).await;
        buf.lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
    });

    // Race child wait against an explicit Abort signal.
    let mut aborted = false;
    let exit_status = tokio::select! {
        s = child.wait() => s,
        _ = abort_rx => {
            aborted = true;
            let _ = child.start_kill();
            child.wait().await
        }
    };

    let log_truncated = stderr_task.await.unwrap_or(false);
    let output_paths = stdout_task.await.unwrap_or_default();

    let (status, exit_code) = match exit_status {
        Ok(s) => {
            if aborted {
                (BuildOutcomeStatus::Killed, s.code())
            } else if s.success() {
                (BuildOutcomeStatus::Success, s.code())
            } else {
                (BuildOutcomeStatus::Failure, s.code())
            }
        }
        Err(e) => {
            tracing::warn!(build_id, error = %e, "child wait failed");
            (BuildOutcomeStatus::Failure, None)
        }
    };

    tracing::info!(
        build_id,
        ?status,
        ?exit_code,
        output_paths_count = output_paths.len(),
        log_truncated,
        "build finished",
    );
    let _ = out_tx.send(
        ControlMessage::BuildFinished {
            build_id,
            status,
            exit_code,
            // Empty output list on non-success: the daemon shouldn't
            // try to pull a closure that wasn't built.
            output_paths: if status == BuildOutcomeStatus::Success {
                output_paths
            } else {
                Vec::new()
            },
            log_truncated,
        }
        .encode_line(),
    );
}

/// `Command::spawn` wrapper that retries `ETXTBSY` ("Text file busy")
/// for up to ~200 ms. Mirrors the helper of the same name in
/// `medusa-builders::closure_xfer`. Without it, fresh-script tests
/// flake under parallel cargo-test fork pressure: a sibling thread's
/// `fork()` briefly inherits a writable fd to a recently-written
/// executable; FD_CLOEXEC eventually closes it on `exec`, but the
/// few-millisecond window is long enough for a sibling exec to fail.
fn spawn_retrying_etxtbsy(cmd: &mut Command) -> std::io::Result<tokio::process::Child> {
    const ETXTBSY: i32 = 26;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
    loop {
        match cmd.spawn() {
            Ok(child) => return Ok(child),
            Err(e) if e.raw_os_error() == Some(ETXTBSY) && std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Read from `stderr` until EOF and emit `BuildLogChunk` frames for
/// every chunk read. Stops emitting (but keeps draining the pipe so the
/// child doesn't block) once `max_log_bytes` raw bytes have been sent;
/// the caller surfaces this in `BuildFinished{log_truncated: true}`.
async fn pump_stderr_as_chunks<R>(
    build_id: i64,
    mut stderr: R,
    max_log_bytes: u64,
    out_tx: mpsc::UnboundedSender<Vec<u8>>,
) -> bool
where
    R: AsyncReadExt + Unpin,
{
    let mut buf = [0u8; 16 * 1024];
    let mut sent: u64 = 0;
    let mut truncated = false;
    loop {
        let n = match stderr.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        if truncated {
            // We've already capped; keep draining so the child's
            // pipe doesn't fill up, but discard contents.
            continue;
        }
        let take = if sent + n as u64 > max_log_bytes {
            truncated = true;
            max_log_bytes.saturating_sub(sent) as usize
        } else {
            n
        };
        if take > 0 {
            let bytes_b64 = base64::engine::general_purpose::STANDARD.encode(&buf[..take]);
            let frame = ControlMessage::BuildLogChunk {
                build_id,
                bytes_b64,
            }
            .encode_line();
            if out_tx.send(frame).is_err() {
                // Receiver gone — control channel torn down. Bail.
                break;
            }
            sent += take as u64;
        }
    }
    truncated
}

/// russh client Handler. M14b: every inbound session channel is a
/// **side channel** — the daemon writes a JSON header then either
/// pushes a closure (`nix-store --import` runs here) or pulls one
/// (`nix-store --export` runs here). The byte-for-byte pump used
/// for the legacy `nix-store --serve --write` flow is gone.
///
/// `close_signals` works around a russh quirk: on a *server-pushed*
/// session channel (where medusa initiates the channel into the
/// agent), `CHANNEL_EOF` and `CHANNEL_CLOSE` are delivered only via
/// the `Handler::channel_eof` / `Handler::channel_close` callbacks,
/// not through the `Channel`'s mpsc that `Channel::wait()` reads.
/// We forward those into the channel-IO adapter so the side-channel
/// dispatcher sees clean EOF on `nix-store --import`'s stdin.
#[derive(Clone)]
struct AgentClient {
    nix_store_bin: Arc<PathBuf>,
    medusa_host_key_path: Option<PathBuf>,
    close_signals: Arc<StdMutex<HashMap<russh::ChannelId, oneshot::Sender<()>>>>,
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
        let (close_tx, close_rx) = oneshot::channel::<()>();
        self.close_signals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(channel.id(), close_tx);
        let nix_store_bin = self.nix_store_bin.clone();
        tokio::spawn(async move {
            serve_side_channel(channel, &nix_store_bin, close_rx).await;
        });
        Ok(())
    }

    // CHANNEL_EOF / CHANNEL_CLOSE arrive here, not through the
    // Channel's mpsc; forward the *first* of either to the pump
    // (whichever fires first; the second is a no-op because the
    // map entry was already consumed).
    async fn channel_eof(
        &mut self,
        channel: russh::ChannelId,
        _session: &mut ClientSession,
    ) -> Result<(), Self::Error> {
        signal_close(&self.close_signals, channel);
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: russh::ChannelId,
        _session: &mut ClientSession,
    ) -> Result<(), Self::Error> {
        signal_close(&self.close_signals, channel);
        Ok(())
    }
}

fn signal_close(
    signals: &Arc<StdMutex<HashMap<russh::ChannelId, oneshot::Sender<()>>>>,
    channel: russh::ChannelId,
) {
    let tx = signals
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&channel);
    if let Some(tx) = tx {
        let _ = tx.send(());
    }
}

/// M14b agent-side channel handler. Wraps the russh channel via
/// [`with_channel_io`] (which ferries bytes through a duplex pipe
/// to give us `AsyncRead`+`AsyncWrite`) and hands the duplex stream
/// to [`dispatch_inbound`]. The dispatcher reads the side-channel
/// header, then either runs `nix-store --import` (closure push from
/// daemon) or `nix-store --export` (closure pull for outputs)
/// against the channel.
async fn serve_side_channel(
    channel: russh::Channel<ClientMsg>,
    nix_store_bin: &Path,
    close_rx: oneshot::Receiver<()>,
) {
    let started_at = std::time::Instant::now();
    let channel_id: u32 = channel.id().into();
    let nix_store_bin = nix_store_bin.to_path_buf();
    let outcome = with_channel_io(channel, Some(close_rx), |io| async move {
        // Split into independent read/write halves so
        // `dispatch_inbound` can read the header + payload and
        // optionally write export bytes back simultaneously.
        let (mut reader, mut writer) = tokio::io::split(io);
        dispatch_inbound(&nix_store_bin, &mut reader, &mut writer).await
    })
    .await;
    match outcome {
        Ok(o) => {
            tracing::info!(
                channel = channel_id,
                elapsed_ms = started_at.elapsed().as_millis() as u64,
                outcome = ?o,
                "side channel finished",
            );
        }
        Err(e) => {
            tracing::warn!(
                channel = channel_id,
                elapsed_ms = started_at.elapsed().as_millis() as u64,
                error = %e,
                "side channel ended with error",
            );
        }
    }
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

#[cfg(test)]
mod build_handler_tests {
    //! Unit-level coverage for `handle_build` against a fake `nix-store`
    //! shell script. Drives the agent-side state machine without
    //! standing up a real SSH session: the test is the daemon side; it
    //! reads frames off the outbound mpsc and asserts on their contents.
    use super::*;
    use medusa_builders::{BuildOutcomeStatus, ControlMessage, LineFramer};
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    /// Drain `out_rx` until a `BuildFinished` frame arrives. Returns
    /// the `(BuildStarted, BuildLogChunk*, BuildFinished)` triple.
    async fn collect_messages(mut out_rx: mpsc::UnboundedReceiver<Vec<u8>>) -> Vec<ControlMessage> {
        let mut framer = LineFramer::new();
        let mut messages = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), out_rx.recv()).await {
                Ok(Some(bytes)) => {
                    for parsed in framer.extend(&bytes) {
                        if let Ok(m) = parsed {
                            let is_finished = matches!(m, ControlMessage::BuildFinished { .. });
                            messages.push(m);
                            if is_finished {
                                return messages;
                            }
                        }
                    }
                }
                Ok(None) => return messages,
                Err(_) => continue,
            }
        }
        messages
    }

    fn write_fake_nix_store(path: &Path, body: &str) {
        // Atomic-rename install to avoid ETXTBSY under parallel
        // cargo-test fork pressure: write+chmod a `.tmp`, then
        // rename, so the final path never had a writable fd opened.
        let tmp = path.with_extension("tmp");
        {
            let mut f = std::fs::File::create(&tmp).unwrap();
            f.write_all(b"#!/bin/sh\n").unwrap();
            f.write_all(body.as_bytes()).unwrap();
            f.sync_all().unwrap();
        }
        let mut perm = std::fs::metadata(&tmp).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&tmp, perm).unwrap();
        std::fs::rename(&tmp, path).unwrap();
    }

    #[tokio::test]
    async fn successful_build_emits_started_chunks_and_finished_with_outputs() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("nix-store");
        // Fake nix-store: print three lines on stderr (build log) and
        // two paths on stdout (output paths), then exit 0.
        write_fake_nix_store(
            &bin,
            r#"
echo "building drv" >&2
echo "running phase" >&2
echo "done" >&2
echo "/nix/store/zzz-out"
echo "/nix/store/yyy-out-dev"
exit 0
"#,
        );

        let (out_tx, out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (_abort_tx, abort_rx) = oneshot::channel::<()>();

        handle_build(
            42,
            "/nix/store/aaa-deriv.drv".to_string(),
            None,
            0,
            64 * 1024,
            &bin,
            out_tx,
            abort_rx,
        )
        .await;

        let messages = collect_messages(out_rx).await;
        assert!(matches!(
            messages.first(),
            Some(ControlMessage::BuildStarted { build_id: 42, .. })
        ));
        let mut log_bytes = Vec::new();
        let mut finished = None;
        for m in &messages {
            match m {
                ControlMessage::BuildLogChunk {
                    build_id,
                    bytes_b64,
                } => {
                    assert_eq!(*build_id, 42);
                    let raw = base64::engine::general_purpose::STANDARD
                        .decode(bytes_b64)
                        .unwrap();
                    log_bytes.extend_from_slice(&raw);
                }
                ControlMessage::BuildFinished { .. } => finished = Some(m.clone()),
                _ => {}
            }
        }
        let log_str = String::from_utf8_lossy(&log_bytes);
        assert!(log_str.contains("building drv"));
        assert!(log_str.contains("running phase"));
        assert!(log_str.contains("done"));
        let ControlMessage::BuildFinished {
            status,
            output_paths,
            log_truncated,
            ..
        } = finished.expect("BuildFinished must be sent")
        else {
            unreachable!()
        };
        assert_eq!(status, BuildOutcomeStatus::Success);
        assert_eq!(
            output_paths,
            vec!["/nix/store/zzz-out", "/nix/store/yyy-out-dev"]
        );
        assert!(!log_truncated);
    }

    #[tokio::test]
    async fn nonzero_exit_reports_failure_with_no_output_paths() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("nix-store");
        write_fake_nix_store(
            &bin,
            r#"
echo "build went wrong" >&2
echo "/nix/store/should-not-leak"
exit 7
"#,
        );

        let (out_tx, out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (_abort_tx, abort_rx) = oneshot::channel::<()>();
        handle_build(
            1,
            "/nix/store/x.drv".into(),
            None,
            0,
            64 * 1024,
            &bin,
            out_tx,
            abort_rx,
        )
        .await;
        let messages = collect_messages(out_rx).await;
        let last = messages.last().expect("at least BuildFinished");
        let ControlMessage::BuildFinished {
            status,
            exit_code,
            output_paths,
            ..
        } = last
        else {
            panic!("expected BuildFinished");
        };
        assert_eq!(*status, BuildOutcomeStatus::Failure);
        assert_eq!(*exit_code, Some(7));
        assert!(
            output_paths.is_empty(),
            "non-success must not surface stdout-derived paths",
        );
    }

    #[tokio::test]
    async fn missing_nix_store_binary_emits_spawn_failed() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("does-not-exist");
        let (out_tx, out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (_abort_tx, abort_rx) = oneshot::channel::<()>();
        handle_build(
            9,
            "/nix/store/x.drv".into(),
            None,
            0,
            64 * 1024,
            &bin,
            out_tx,
            abort_rx,
        )
        .await;
        let messages = collect_messages(out_rx).await;
        // Only BuildFinished{SpawnFailed} expected — no BuildStarted.
        assert_eq!(messages.len(), 1, "exactly one frame on spawn failure");
        let ControlMessage::BuildFinished { status, .. } = &messages[0] else {
            panic!("expected BuildFinished");
        };
        assert_eq!(*status, BuildOutcomeStatus::SpawnFailed);
    }

    #[tokio::test]
    async fn abort_kills_running_subprocess_and_reports_killed() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("nix-store");
        // Long-running fake: sleeps so the test has a window to abort.
        write_fake_nix_store(
            &bin,
            r#"
echo "starting long build" >&2
sleep 30
echo "/nix/store/never"
exit 0
"#,
        );

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (abort_tx, abort_rx) = oneshot::channel::<()>();

        let bin_owned = bin.clone();
        let handle = tokio::spawn(async move {
            handle_build(
                77,
                "/nix/store/x.drv".into(),
                None,
                0,
                64 * 1024,
                &bin_owned,
                out_tx,
                abort_rx,
            )
            .await;
        });

        // Wait for BuildStarted before signalling abort so we know the
        // subprocess actually exists.
        let mut framer = LineFramer::new();
        let started_seen = async {
            loop {
                let bytes = out_rx.recv().await.unwrap();
                for m in framer.extend(&bytes) {
                    if let Ok(ControlMessage::BuildStarted { build_id: 77, .. }) = m {
                        return;
                    }
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(5), started_seen)
            .await
            .expect("BuildStarted must arrive promptly");

        abort_tx.send(()).expect("abort_rx still alive");

        // Drain remaining frames; expect BuildFinished{Killed} fast.
        let collect = async {
            loop {
                let bytes = match out_rx.recv().await {
                    Some(b) => b,
                    None => return None,
                };
                for m in framer.extend(&bytes) {
                    if let Ok(ControlMessage::BuildFinished { status, .. }) = m {
                        return Some(status);
                    }
                }
            }
        };
        let status = tokio::time::timeout(Duration::from_secs(5), collect)
            .await
            .expect("BuildFinished must follow Abort within seconds")
            .expect("stream did not end before BuildFinished");
        assert_eq!(
            status,
            BuildOutcomeStatus::Killed,
            "abort path must surface as Killed, not Failure",
        );
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn log_chunks_capped_at_max_log_bytes_with_truncation_flag() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("nix-store");
        // Emit far more bytes than the cap so truncation kicks in.
        write_fake_nix_store(
            &bin,
            r#"
# Each iteration writes ~100 bytes of stderr.
i=0
while [ $i -lt 100 ]; do
  printf 'log line %03d: %s\n' "$i" "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" >&2
  i=$((i + 1))
done
echo /nix/store/zzz
exit 0
"#,
        );

        let cap: u64 = 512;
        let (out_tx, out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (_abort_tx, abort_rx) = oneshot::channel::<()>();
        handle_build(
            5,
            "/nix/store/x.drv".into(),
            None,
            0,
            cap,
            &bin,
            out_tx,
            abort_rx,
        )
        .await;
        let messages = collect_messages(out_rx).await;
        let mut total_log_bytes = 0u64;
        let mut log_truncated = None;
        for m in &messages {
            match m {
                ControlMessage::BuildLogChunk { bytes_b64, .. } => {
                    let raw = base64::engine::general_purpose::STANDARD
                        .decode(bytes_b64)
                        .unwrap();
                    total_log_bytes += raw.len() as u64;
                }
                ControlMessage::BuildFinished {
                    log_truncated: t, ..
                } => log_truncated = Some(*t),
                _ => {}
            }
        }
        assert!(
            total_log_bytes <= cap,
            "log bytes must be capped: got {total_log_bytes}, cap {cap}",
        );
        assert_eq!(log_truncated, Some(true), "truncation flag must be set");
    }
}
