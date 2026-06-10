//! Dial argunix, authenticate, hello, heartbeat, accept build channels.
//!
//! Runs forever (with reconnect-and-backoff) until the supplied
//! `shutdown` future fires. On clean shutdown, sends a `shutdown`
//! control message before closing the session so argunix logs the
//! event at INFO and immediately marks the registry entry
//! `Disconnecting`.

use crate::identity::PersistedKey;
use argunix_builders::{
    BuildOutcomeStatus, ControlMessage, SideChannelDispatchOutcome, dispatch_inbound,
    with_channel_io,
};
use argunix_domain::{BuilderCapabilities, BuilderName};
use base64::Engine as _;
use russh::ChannelMsg;
use russh::client::{self, Handle, Handler, Msg as ClientMsg, Session as ClientSession};
use russh::keys::ssh_key::{self, PrivateKey};
use russh::keys::{Algorithm, PrivateKeyWithHashAlg};
use socket2::{SockRef, TcpKeepalive};
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("converting identity to russh key: {0}")]
    IdentityKey(String),
    #[error("connecting to argunix at {addr}: {source}")]
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
    pub argunix: SocketAddr,
    pub identity: PersistedKey,
    /// `None` after first successful enrollment; the caller wipes
    /// the field once the daemon's TOFU row is in place.
    pub enrollment_token: Option<Arc<Vec<u8>>>,
    pub name: BuilderName,
    pub capabilities: BuilderCapabilities,
    /// Initial reconnect backoff. Doubles up to 30s on repeated failures;
    /// resets to this value on every successful hello.
    pub reconnect_initial_backoff: Duration,
    /// Path to the local `nix-store` binary. Used by `nix-store
    /// --realise` during build execution. Tests substitute a fake
    /// shell-script binary; production deployments leave it at the
    /// default `"nix-store"` resolved on the agent's `PATH`.
    pub nix_store_bin: PathBuf,
    /// Path to the system `nix-daemon` socket. Each inbound
    /// `NixDaemonStdio` side channel forwards its bytes
    /// bidirectionally to a connection on this socket — the daemon
    /// side then drives the resulting daemon-protocol stream with
    /// `nix copy --from/--to unix:///proxy.sock`. Defaults to the
    /// NixOS standard `/nix/var/nix/daemon-socket/socket`; tests
    /// point at a temp-path echo socket.
    pub nix_daemon_socket: PathBuf,
    /// Path under which the agent pins argunix's SSH host key (TOFU).
    /// First successful connect writes the presented pubkey here in
    /// OpenSSH single-line format; subsequent connects refuse if the
    /// presented key does not match.
    ///
    /// `None` disables TOFU entirely — used by the integration tests
    /// where the daemon's host key is regenerated per VM. Production
    /// deployments should always set this.
    pub argunix_host_key_path: Option<PathBuf>,
    /// Directory under which the agent stores throwaway gcroots
    /// for in-flight builds when the daemon does not supply its
    /// own gcroot path. Without this, `nix-store --realise` emits
    /// `warning: you did not specify '--add-root'…` on every build,
    /// which ends up in the captured build log. The link is removed
    /// once the build reaches a terminal state.
    pub build_gcroot_dir: PathBuf,
}

impl AgentConfig {
    pub fn default_backoff() -> Duration {
        Duration::from_secs(2)
    }
    pub fn default_nix_store_bin() -> PathBuf {
        PathBuf::from("nix-store")
    }
    pub fn default_nix_daemon_socket() -> PathBuf {
        PathBuf::from("/nix/var/nix/daemon-socket/socket")
    }
    pub fn default_build_gcroot_dir() -> PathBuf {
        PathBuf::from("/var/lib/argunix-builder/build-gcroots")
    }
}

/// How long the agent tolerates *zero* connection progress — no
/// heartbeat sent, no control frame received, no log chunk written —
/// before it concludes the control session is wedged and tears it down
/// to reconnect. The failure this guards against does not surface as an
/// error: a stalled outbound flush against a back-pressured or silently
/// half-open coordinator just *blocks forever* (TCP raises nothing for a
/// slow-but-alive peer, and russh's own keepalive is starved on the same
/// wedged task). Set equal to the coordinator's
/// [`argunix_builders::LIVENESS_MAX_SILENCE`]: past that point the
/// coordinator has already evicted us, so clinging to the dead socket is
/// pointless — reconnecting re-enrols us and refills the pool.
const SELF_LIVENESS_TIMEOUT: Duration = argunix_builders::LIVENESS_MAX_SILENCE;

/// Upper bound on a single outbound `channel.data()` write. A write that
/// neither completes nor errors within this window is wedged on a full
/// send window the peer will never drain; time out and reconnect rather
/// than park the session loop indefinitely. This is the load-bearing
/// fix — the incident where builders silently vanished from the pool was
/// a write that blocked here forever without ever erroring.
const WRITE_STALL_TIMEOUT: Duration = argunix_builders::LIVENESS_MAX_SILENCE;

/// How often the self-liveness watchdog re-checks the progress clock.
const PROGRESS_SCAN_INTERVAL: Duration = Duration::from_secs(5);

/// Monotonic "last progress" clock shared by the session loop and the
/// self-liveness watchdog. Any byte that moves on the control channel —
/// a heartbeat we managed to send, a control frame we received, or a
/// build lifecycle/log chunk we wrote — bumps it. The watchdog
/// reconnects when nothing has moved for [`SELF_LIVENESS_TIMEOUT`]. On a
/// healthy link the 5s heartbeat keeps bumping it (heartbeats interleave
/// even during a large side-channel transfer); only a genuinely wedged
/// session, where even the heartbeat write blocks, lets it go stale.
struct ProgressClock {
    base: tokio::time::Instant,
    last_ms: AtomicU64,
}

impl ProgressClock {
    fn new() -> Self {
        Self {
            base: tokio::time::Instant::now(),
            last_ms: AtomicU64::new(0),
        }
    }

    /// Record that the connection just made progress.
    fn bump(&self) {
        self.last_ms
            .store(self.base.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    /// Time since the last [`Self::bump`].
    fn idle(&self) -> Duration {
        let now = self.base.elapsed().as_millis() as u64;
        Duration::from_millis(now.saturating_sub(self.last_ms.load(Ordering::Relaxed)))
    }
}

/// Resolves once `clock` has shown no progress for `timeout`. Driven as a
/// *sibling* of the session loop in an outer `select!`, so its timer is
/// still serviced while the session loop is parked inside a wedged
/// `channel.data().await` — the one situation neither a write error nor
/// russh's own keepalive can surface.
async fn liveness_watchdog(clock: Arc<ProgressClock>, timeout: Duration, scan: Duration) {
    let mut tick = tokio::time::interval(scan);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if clock.idle() >= timeout {
            return;
        }
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

    // SSH-layer keepalive: russh sends a global request every 30s when
    // nothing has been received and tears the session down after
    // `keepalive_max` (3) unanswered ones — so a wedged peer is
    // surfaced as a transport error within ~2 minutes regardless of
    // what the kernel TCP stack thinks.
    let client_cfg = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        // Enlarge the per-channel flow-control window (russh's 2 MiB
        // default deadlocks the bidirectional nix-daemon tunnel on a
        // large closure push). This governs the coordinator→builder
        // (request/NAR) direction. See
        // `argunix_builders::BUILDER_SESSION_WINDOW_SIZE`.
        window_size: argunix_builders::BUILDER_SESSION_WINDOW_SIZE,
        maximum_packet_size: argunix_builders::BUILDER_SESSION_MAX_PACKET,
        ..client::Config::default()
    });
    let handler = AgentClient {
        nix_daemon_socket: Arc::new(cfg.nix_daemon_socket.clone()),
        argunix_host_key_path: cfg.argunix_host_key_path.clone(),
        close_signals: Arc::new(StdMutex::new(HashMap::new())),
    };

    // Dead-peer detection. The agent writes a heartbeat every 5s, so
    // the connection is *never idle* — which means SO_KEEPALIVE never
    // fires (its timer only runs on an idle socket). A coordinator
    // that vanishes (process restart, host gone) leaves the socket in
    // TCP retransmit, governed by `tcp_retries2` (~15min) before a
    // write finally errors — far too long to notice a restart.
    //
    // TCP_USER_TIMEOUT is the real fix: it caps how long *sent* data
    // may sit unacknowledged before the kernel forcibly closes the
    // connection, overriding `tcp_retries2`. With a 5s heartbeat and a
    // 30s cap, a dead coordinator is detected within ~30s — the next
    // heartbeat write errors and the agent reconnects. SO_KEEPALIVE is
    // kept for the genuinely-idle case; TCP_USER_TIMEOUT also bounds
    // its probe window.
    let tcp = tokio::net::TcpStream::connect(cfg.argunix)
        .await
        .map_err(|e| AgentError::Connect {
            addr: cfg.argunix,
            source: russh::Error::IO(e),
        })?;
    let sock = SockRef::from(&tcp);
    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(30))
        .with_interval(Duration::from_secs(10))
        .with_retries(3);
    if let Err(e) = sock.set_tcp_keepalive(&keepalive) {
        tracing::warn!(error = %e, "set_tcp_keepalive() failed; idle dead-peer detection may be slow");
    }
    if let Err(e) = sock.set_tcp_user_timeout(Some(Duration::from_secs(30))) {
        tracing::warn!(
            error = %e,
            "set_tcp_user_timeout() failed; a dead coordinator may take ~15min to detect",
        );
    }
    drop(sock);
    let mut session = client::connect_stream(client_cfg, tcp, handler)
        .await
        .map_err(|source| AgentError::Connect {
            addr: cfg.argunix,
            source,
        })?;

    // Try pubkey first. On the daemon's first contact the row
    // doesn't exist yet → pubkey auth fails → fall back to token.
    // After successful enrollment the daemon's TOFU row matches our
    // pubkey on every subsequent connect.
    let user = "argunix-builder";
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
        native_system: cfg.capabilities.native_system.clone(),
        features: cfg.capabilities.features.clone(),
        max_jobs: cfg.capabilities.max_jobs,
        nix_version: cfg.capabilities.nix_version.clone(),
    };
    channel.data(&hello.encode_line()[..]).await?;

    // Drain control until welcome arrives. Drop trailing data into
    // the heartbeat loop's framer.
    let welcome_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut welcome_seen = false;
    let mut framer = argunix_builders::LineFramer::new();
    while tokio::time::Instant::now() < welcome_deadline {
        let remaining = welcome_deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, channel.wait()).await {
            Ok(Some(ChannelMsg::Data { data })) => {
                for parsed in framer.extend(&data) {
                    if let Ok(ControlMessage::Welcome { builder_id }) = parsed {
                        tracing::info!(builder_id = %builder_id, "registered with argunix");
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
        // argunix closed the channel without welcoming us. Force a
        // reconnect (the next attempt may fare differently).
        return Ok(false);
    }

    // Heartbeat + shutdown loop. 5s cadence so the web UI's stats
    // sparkline feels live; payload is ~50 bytes so the bandwidth
    // cost over a 30s baseline is negligible.
    let mut hb_interval = tokio::time::interval(Duration::from_secs(5));
    hb_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Skip the immediate first tick.
    hb_interval.tick().await;
    let mut stats_sampler = argunix_builders::StatsSampler::new();

    // Outbound control queue. Build tasks send `BuildStarted /
    // BuildLogChunk / BuildFinished` here; the main loop drains the
    // queue and writes to the SSH channel (which only one task can hold
    // a `&mut` to at a time).
    //
    // Bounded on purpose. Pairs with the coordinator-side lifecycle
    // channel (also bounded) to give end-to-end back-pressure with no
    // silent log loss: if the coordinator is slow to drain its end,
    // the russh send window stalls, the main loop's `channel.data()`
    // blocks, this queue fills, and `pump_stderr_as_chunks` blocks on
    // send → the `nix-store --realise` stderr pipe fills → the build
    // briefly pauses. Memory at both ends is bounded by the queue
    // capacities; nothing grows without limit, nothing is dropped.
    //
    // Heartbeats bypass this queue (the main loop writes them
    // directly to the SSH channel), so a saturated log queue cannot
    // starve the heartbeats the coordinator uses to detect dead
    // builders.
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(4096);
    // In-flight build map. `Abort` looks up the oneshot for the
    // matching `build_id` and signals the build task to SIGKILL its
    // `nix-store --realise` child.
    let in_flight: Arc<StdMutex<HashMap<i64, oneshot::Sender<()>>>> =
        Arc::new(StdMutex::new(HashMap::new()));

    // Progress clock for the self-liveness watchdog (below). The session
    // loop runs as the inner future of an outer `select!` against the
    // watchdog; the watchdog is a sibling future, so it is still polled
    // even while this loop is parked inside a wedged `channel.data()`.
    let progress = Arc::new(ProgressClock::new());
    let session_loop = async {
        'session: loop {
            tokio::select! {
                _ = &mut *shutdown => {
                    // Best-effort goodbye, bounded by a short timeout. Over
                    // a half-open connection an un-timeouted
                    // `channel.data().await` blocks until russh's keepalive
                    // tears the session down (~90s), which makes
                    // `systemctl stop`/`restart` of the agent unit hang
                    // until systemd escalates to SIGKILL. We're exiting
                    // regardless — a dropped `Shutdown` message just means
                    // argunix reaps us via its own keepalive instead of
                    // seeing a clean drain.
                    let bye = ControlMessage::Shutdown {
                        reason: "agent stopping".into(),
                        drain: false,
                    };
                    let goodbye = async {
                        let _ = channel.data(&bye.encode_line()[..]).await;
                        let _ = channel.close().await;
                        let _ = session.disconnect(
                            russh::Disconnect::ByApplication,
                            "agent shutdown",
                            "en",
                        ).await;
                    };
                    if tokio::time::timeout(Duration::from_secs(3), goodbye)
                        .await
                        .is_err()
                    {
                        tracing::warn!(
                            "graceful shutdown timed out (connection likely half-open); exiting anyway",
                        );
                    }
                    break 'session Ok(true);
                }
                _ = hb_interval.tick() => {
                    let hb = ControlMessage::Heartbeat {
                        ts: chrono::Utc::now().timestamp(),
                        stats: stats_sampler.sample(),
                    };
                    // Bounded write: a heartbeat that neither completes nor
                    // errors within the window means the session is wedged
                    // (full send window against a peer that never drains).
                    // Treat it as a disconnect and reconnect.
                    match tokio::time::timeout(
                        WRITE_STALL_TIMEOUT,
                        channel.data(&hb.encode_line()[..]),
                    )
                    .await
                    {
                        Ok(Ok(())) => progress.bump(),
                        Ok(Err(_)) => break 'session Ok(false),
                        Err(_) => {
                            tracing::warn!(
                                "heartbeat write stalled past the liveness timeout; reconnecting",
                            );
                            break 'session Ok(false);
                        }
                    }
                }
                // Outbound from build tasks → SSH channel.
                Some(bytes) = out_rx.recv() => {
                    match tokio::time::timeout(
                        WRITE_STALL_TIMEOUT,
                        channel.data(&bytes[..]),
                    )
                    .await
                    {
                        Ok(Ok(())) => progress.bump(),
                        Ok(Err(_)) => break 'session Ok(false),
                        Err(_) => {
                            tracing::warn!(
                                "build log/lifecycle write stalled past the liveness timeout; reconnecting",
                            );
                            break 'session Ok(false);
                        }
                    }
                }
                ev = channel.wait() => match ev {
                    Some(ChannelMsg::Data { data }) => {
                        progress.bump();
                        for parsed in framer.extend(&data) {
                            match parsed {
                                Ok(ControlMessage::Kick { reason }) => {
                                    tracing::warn!(reason = %reason, "kicked by argunix; reconnect after backoff");
                                    break 'session Ok(false);
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
                                    // If the daemon supplied a gcroot path it
                                    // owns the lifetime; otherwise we plant a
                                    // throwaway under our state dir solely to
                                    // silence `nix-store`'s "you did not
                                    // specify '--add-root'…" warning. Either
                                    // way we hand the agent path to handle_build
                                    // and `agent_owns_gcroot` tells it whether
                                    // to clean up after the build.
                                    let (effective_gcroot, agent_owns_gcroot) = match gc_root {
                                        Some(p) => (Some(p), false),
                                        None => (
                                            Some(
                                                cfg.build_gcroot_dir
                                                    .join(build_id.to_string())
                                                    .to_string_lossy()
                                                    .into_owned(),
                                            ),
                                            true,
                                        ),
                                    };
                                    tokio::spawn(async move {
                                        handle_build(
                                            build_id,
                                            drv_path,
                                            effective_gcroot,
                                            agent_owns_gcroot,
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
                    Some(ChannelMsg::Close) | None => break 'session Ok(false),
                    Some(_) => continue,
                }
            }
        }
    };

    // Run the session loop against the self-liveness watchdog. The
    // watchdog wins only if the loop made no progress for
    // `SELF_LIVENESS_TIMEOUT` — i.e. it is wedged on a flush that will
    // never complete. Returning `Ok(false)` drops `session`/`channel`
    // when this function returns, closing the dead socket, and `run`
    // reconnects with backoff; the fresh `Hello` displaces our stale
    // registry entry on the coordinator.
    let outcome = tokio::select! {
        o = session_loop => o,
        _ = liveness_watchdog(
            progress.clone(),
            SELF_LIVENESS_TIMEOUT,
            PROGRESS_SCAN_INTERVAL,
        ) => {
            tracing::warn!(
                idle_ms = progress.idle().as_millis() as u64,
                timeout_ms = SELF_LIVENESS_TIMEOUT.as_millis() as u64,
                "self-liveness watchdog fired: no connection progress within \
                 the timeout; tearing down the wedged session to reconnect",
            );
            Ok(false)
        }
    };

    // The control connection is going away — either we're shutting
    // down (Ok(true)) or we're about to reconnect (Ok(false)). Either
    // way the coordinator can no longer receive results for these
    // build_ids: after a reconnect the agent sends a fresh `hello` and
    // the coordinator re-dispatches via its own Q79 re-queue, with no
    // memory of the old per-connection build_id mapping. So a build we
    // leave running here is orphaned work that nothing will ever
    // collect. Kill every in-flight `nix-store --realise` so a
    // stopped/restarted coordinator actually quiesces this builder
    // instead of leaving zombie builds churning. `handle_build`
    // observes the fired oneshot, SIGKILLs its child, and exits.
    abort_all_in_flight(&in_flight);

    outcome
}

/// Fire the abort oneshot for every build still tracked in `in_flight`,
/// draining the map. Each send wakes `handle_build`'s `abort_rx` arm,
/// which SIGKILLs the `nix-store --realise` child and reports `Killed`.
/// Called when the control connection is torn down so the agent doesn't
/// leave orphaned builds running across a reconnect.
fn abort_all_in_flight(in_flight: &Arc<StdMutex<HashMap<i64, oneshot::Sender<()>>>>) {
    let pending: Vec<(i64, oneshot::Sender<()>)> = {
        let mut map = in_flight.lock().unwrap_or_else(|e| e.into_inner());
        map.drain().collect()
    };
    if pending.is_empty() {
        return;
    }
    let build_ids: Vec<i64> = pending.iter().map(|(id, _)| *id).collect();
    tracing::warn!(
        ?build_ids,
        "control connection closed with in-flight builds; killing local nix-store --realise so the coordinator quiesces this builder",
    );
    for (_, abort_tx) in pending {
        // Err only if the build task already finished and dropped its
        // receiver between our drain and now — nothing to cancel.
        let _ = abort_tx.send(());
    }
}

/// Agent-side handler for one `Build` dispatch. Spawns
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
/// `agent_owns_gcroot` is true iff `gc_root` was synthesised by the
/// agent (the daemon passed `None`). We then remove the link once the
/// build reaches a terminal state — daemon-supplied gcroots are left
/// alone for the daemon to manage.
async fn handle_build(
    build_id: i64,
    drv_path: String,
    gc_root: Option<String>,
    agent_owns_gcroot: bool,
    timeout_secs: u64,
    max_log_bytes: u64,
    nix_store_bin: &Path,
    out_tx: mpsc::Sender<Vec<u8>>,
    abort_rx: oneshot::Receiver<()>,
) {
    if agent_owns_gcroot {
        if let Some(ref g) = gc_root {
            if let Some(parent) = std::path::Path::new(g).parent() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    tracing::warn!(
                        build_id,
                        error = %e,
                        path = %parent.display(),
                        "failed to create agent-owned gcroot dir; \
                         nix-store will warn about --add-root",
                    );
                }
            }
        }
    }
    let mut cmd = Command::new(nix_store_bin);
    cmd.arg("--realise");
    // If a substituter download fails mid-stream (e.g. S3-style cache
    // returning HTTP/2 stream errors or 504 on a range-resume), build
    // from source instead of failing the job. Without this nix bails
    // with "no substituter that can build it" and points the user at
    // `--fallback` anyway.
    cmd.arg("--fallback");
    // Structured `@nix {…}` build events on stderr — the coordinator
    // parses them (`argunix-nom`) into per-derivation log lines. The
    // chunk transport is byte-transparent; stdout is unaffected.
    cmd.arg("--log-format").arg("internal-json");
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
            let _ = out_tx
                .send(
                    ControlMessage::BuildFinished {
                        build_id,
                        status: BuildOutcomeStatus::SpawnFailed,
                        exit_code: None,
                        output_paths: Vec::new(),
                        log_truncated: false,
                    }
                    .encode_line(),
                )
                .await;
            return;
        }
    };
    let pid = child.id();
    let _ = out_tx
        .send(ControlMessage::BuildStarted { build_id, pid }.encode_line())
        .await;

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

    // On the abort path we MUST NOT await the stdout/stderr pump tasks.
    // `child.start_kill()` sends SIGKILL to the immediate child only;
    // any grandchildren the child forked (a shell's `sleep`, a build
    // wrapper's nested process, …) inherit and keep the pipe
    // write-ends, so the tasks' `read()` calls would block on EOF
    // until those grandchildren exit naturally. We saw this hang for
    // 30+ seconds under cargo-tests sandbox load. Discarding the
    // task results is fine: log chunks already streamed up to the
    // moment of abort are in `out_tx`'s backlog, and output_paths
    // are empty on non-success below regardless. Aborting the tasks
    // drops the read-ends of the pipes; the orphaned grandchildren's
    // writes go to a closed pipe (no impact on the daemon, and the
    // grandchildren reap themselves when they finish).
    let (stderr_bytes_sent, log_truncated, mut output_paths) = if aborted {
        stderr_task.abort();
        stdout_task.abort();
        (0u64, false, Vec::<String>::new())
    } else {
        let (sent, trunc) = stderr_task.await.unwrap_or((0, false));
        (sent, trunc, stdout_task.await.unwrap_or_default())
    };

    // When the agent owns the gcroot, `nix-store --realise --add-root
    // <path>` prints the symlink path on stdout, not the underlying
    // `/nix/store/<hash>-<name>`. The daemon will `nix copy --from`
    // *the agent's nix store* and needs the real store path, not a
    // path that only exists on the agent's filesystem. Resolve every
    // symlink we emitted before reporting.
    if agent_owns_gcroot {
        for p in output_paths.iter_mut() {
            match tokio::fs::read_link(&p).await {
                Ok(target) => *p = target.to_string_lossy().into_owned(),
                Err(e) => tracing::warn!(
                    build_id,
                    error = %e,
                    path = %p,
                    "could not resolve agent-owned gcroot symlink; \
                     reporting raw path (daemon will likely fail to pull)",
                ),
            }
        }
    }

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

    // Recovery: `nix-store --realise` succeeded but produced no
    // captured stderr. The realise was satisfied by substitution, or
    // this client was a silent waiter on another in-progress build of
    // the same drv (nix-daemon streams the live log only to the
    // client that *triggered* the build). Either way the coordinator
    // would otherwise record an empty stored log. Recover the drv's
    // archived log from the local `nix-daemon` log archive — these
    // frames flow through the same chunk pipeline as live stderr.
    if !aborted && status == BuildOutcomeStatus::Success && stderr_bytes_sent == 0 {
        tracing::debug!(
            build_id,
            drv = %drv_path,
            "build finished with empty stderr; trying nix-store --read-log fallback",
        );
        try_send_archived_log(&nix_store_bin, build_id, &drv_path, &out_tx).await;
    }

    tracing::info!(
        build_id,
        ?status,
        ?exit_code,
        output_paths_count = output_paths.len(),
        log_truncated,
        "build finished",
    );
    let _ = out_tx
        .send(
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
        )
        .await;

    // Best-effort cleanup of the throwaway gcroot symlink(s). For a
    // multi-output drv `nix-store --realise --add-root <root>` may
    // emit `<root>` plus `<root>-<output>` siblings, so we sweep the
    // parent directory by build-id prefix. Failures are non-fatal —
    // the daemon already has its own gcroot; these links only existed
    // to silence the `nix-store --add-root` warning.
    if agent_owns_gcroot {
        if let Some(ref g) = gc_root {
            let path = std::path::Path::new(g);
            if let (Some(parent), Some(file_name)) =
                (path.parent(), path.file_name().and_then(|n| n.to_str()))
            {
                if let Ok(mut rd) = tokio::fs::read_dir(parent).await {
                    while let Ok(Some(entry)) = rd.next_entry().await {
                        let name = entry.file_name();
                        let name_s = name.to_string_lossy();
                        if name_s == file_name || name_s.starts_with(&format!("{file_name}-")) {
                            let _ = tokio::fs::remove_file(entry.path()).await;
                        }
                    }
                }
            }
        }
    }
}

/// `Command::spawn` wrapper that retries `ETXTBSY` ("Text file busy")
/// for up to ~200 ms. Mirrors the helper of the same name in
/// `argunix-builders::closure_xfer`. Without it, fresh-script tests
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
/// Returns `(bytes_sent, truncated)` — `bytes_sent == 0` on a clean
/// success means the realise was satisfied entirely by substitution
/// (or the client was a silent waiter on another in-progress build),
/// so the caller can fall back to `nix-store --read-log` for the
/// drv's archived log instead of recording a blank stored log.
async fn pump_stderr_as_chunks<R>(
    build_id: i64,
    mut stderr: R,
    max_log_bytes: u64,
    out_tx: mpsc::Sender<Vec<u8>>,
) -> (u64, bool)
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
            // Blocking send: when the queue is full we back-pressure
            // into this read loop, which lets the `nix-store --realise`
            // stderr pipe fill up and stalls the build briefly rather
            // than dropping log frames. The receiver going away (e.g.
            // the SSH session tore down) returns `Err` and we exit.
            if out_tx.send(frame).await.is_err() {
                break;
            }
            sent += take as u64;
        }
    }
    (sent, truncated)
}

/// Run `nix-store --read-log <drv>` and stream its stdout to the
/// coordinator as one or more `BuildLogChunk` frames. Used as a
/// recovery path when `nix-store --realise` succeeded but produced no
/// stderr — substitution, or this client was a silent waiter on
/// another in-progress build of the same drv. Best-effort: any
/// failure (binary missing, drv not in the archive, non-zero exit)
/// returns silently and the coordinator ends up with whatever
/// chunks it has (possibly none).
async fn try_send_archived_log(
    nix_store_bin: &std::path::Path,
    build_id: i64,
    drv_path: &str,
    out_tx: &mpsc::Sender<Vec<u8>>,
) {
    let output = match tokio::process::Command::new(nix_store_bin)
        .arg("--read-log")
        .arg(drv_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
    {
        Ok(o) => o,
        Err(_) => return,
    };
    if !output.status.success() || output.stdout.is_empty() {
        return;
    }
    // Chunk to match the live stream's 16 KiB framing.
    for chunk in output.stdout.chunks(16 * 1024) {
        let bytes_b64 = base64::engine::general_purpose::STANDARD.encode(chunk);
        let frame = ControlMessage::BuildLogChunk {
            build_id,
            bytes_b64,
        }
        .encode_line();
        if out_tx.send(frame).await.is_err() {
            return;
        }
    }
}

/// russh client Handler. Every inbound session channel is a
/// **side channel** — the daemon writes a JSON header, then the
/// channel tunnels stdin/stdout of `nix-daemon --stdio` so the
/// daemon side can drive `nix copy --from/--to unix:///proxy.sock`
/// against it.
///
/// `close_signals` works around a russh quirk: on a *server-pushed*
/// session channel (where argunix initiates the channel into the
/// agent), `CHANNEL_EOF` and `CHANNEL_CLOSE` are delivered only via
/// the `Handler::channel_eof` / `Handler::channel_close` callbacks,
/// not through the `Channel`'s mpsc that `Channel::wait()` reads.
/// We forward those into the channel-IO adapter so the side-channel
/// dispatcher sees clean EOF on `nix-store --import`'s stdin.
#[derive(Clone)]
struct AgentClient {
    nix_daemon_socket: Arc<PathBuf>,
    argunix_host_key_path: Option<PathBuf>,
    close_signals: Arc<StdMutex<HashMap<russh::ChannelId, oneshot::Sender<()>>>>,
}

impl Handler for AgentClient {
    type Error = russh::Error;
    async fn check_server_key(
        &mut self,
        presented: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        // TOFU on argunix's SSH host key. First successful connect
        // pins the key under `argunix_host_key_path`; later connects
        // refuse on mismatch. Operator recovery: delete the pinned
        // file, then restart the agent.
        let Some(path) = self.argunix_host_key_path.as_deref() else {
            // Tests: TOFU disabled, accept any key.
            return Ok(true);
        };
        match check_or_pin_host_key(path, presented).await {
            Ok(()) => Ok(true),
            Err(e) => {
                tracing::error!(
                    error = %e,
                    pinned = %path.display(),
                    "argunix host key TOFU check failed; refusing connection",
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
        let nix_daemon_socket = self.nix_daemon_socket.clone();
        tokio::spawn(async move {
            serve_side_channel(channel, &nix_daemon_socket, close_rx).await;
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

/// Agent-side side-channel handler. Wraps the russh channel via
/// [`with_channel_io`] and hands the duplex stream to
/// [`dispatch_inbound`], which reads the header and forwards bytes
/// to/from the system `nix-daemon` socket for the lifetime of the
/// channel.
async fn serve_side_channel(
    channel: russh::Channel<ClientMsg>,
    nix_daemon_socket: &Path,
    close_rx: oneshot::Receiver<()>,
) {
    let started_at = std::time::Instant::now();
    let channel_id: u32 = channel.id().into();
    let nix_daemon_socket = nix_daemon_socket.to_path_buf();
    let outcome = with_channel_io(channel, Some(close_rx), |io| async move {
        let (mut reader, mut writer) = tokio::io::split(io);
        dispatch_inbound(&nix_daemon_socket, &mut reader, &mut writer).await
    })
    .await;
    let elapsed_ms = started_at.elapsed().as_millis() as u64;
    match outcome {
        Ok(SideChannelDispatchOutcome::NixDaemonTunneled {
            build_id,
            bytes_to_daemon,
            bytes_from_daemon,
        }) => {
            tracing::info!(
                channel = channel_id,
                elapsed_ms,
                kind = "nix_daemon_stdio",
                build_id,
                bytes_to_daemon,
                bytes_from_daemon,
                "side channel finished",
            );
        }
        Err(e) => {
            tracing::warn!(
                channel = channel_id,
                elapsed_ms,
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
        "argunix host pubkey changed — pinned `{pinned}` but server presented `{presented}`. \
         If this is intentional, delete the pinned file and restart the agent."
    )]
    Mismatch { pinned: String, presented: String },
}

/// TOFU on the argunix host key. Reads the pinned key (if present) and
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
                "pinned argunix host key on first contact (TOFU)",
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
    PrivateKey::new(kpd, "argunix-builder").map_err(Into::into)
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
        let path = dir.path().join("argunix-host-key.pub");
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
        let path = dir.path().join("argunix-host-key.pub");
        let key = pubkey();

        check_or_pin_host_key(&path, &key).await.unwrap();
        // Second call with the same key must succeed.
        check_or_pin_host_key(&path, &key).await.unwrap();
    }

    #[tokio::test]
    async fn mismatched_key_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("argunix-host-key.pub");

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
    use argunix_builders::{BuildOutcomeStatus, ControlMessage, LineFramer};
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    /// Drain `out_rx` until a `BuildFinished` frame arrives. Returns
    /// the `(BuildStarted, BuildLogChunk*, BuildFinished)` triple.
    async fn collect_messages(mut out_rx: mpsc::Receiver<Vec<u8>>) -> Vec<ControlMessage> {
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

        let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>(4096);
        let (_abort_tx, abort_rx) = oneshot::channel::<()>();

        handle_build(
            42,
            "/nix/store/aaa-deriv.drv".to_string(),
            None,
            false,
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

        let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>(4096);
        let (_abort_tx, abort_rx) = oneshot::channel::<()>();
        handle_build(
            1,
            "/nix/store/x.drv".into(),
            None,
            false,
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
        let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>(4096);
        let (_abort_tx, abort_rx) = oneshot::channel::<()>();
        handle_build(
            9,
            "/nix/store/x.drv".into(),
            None,
            false,
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

        let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(4096);
        let (abort_tx, abort_rx) = oneshot::channel::<()>();

        let bin_owned = bin.clone();
        let handle = tokio::spawn(async move {
            handle_build(
                77,
                "/nix/store/x.drv".into(),
                None,
                false,
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
    async fn abort_all_in_flight_fires_every_pending_build_and_drains_map() {
        // Mirrors what serve_one_connection does when the control
        // connection drops: every tracked build's abort oneshot must
        // fire, and the map must be emptied so a subsequent reconnect
        // starts clean.
        let in_flight: Arc<StdMutex<HashMap<i64, oneshot::Sender<()>>>> =
            Arc::new(StdMutex::new(HashMap::new()));
        let (tx_a, rx_a) = oneshot::channel::<()>();
        let (tx_b, rx_b) = oneshot::channel::<()>();
        {
            let mut map = in_flight.lock().unwrap();
            map.insert(1, tx_a);
            map.insert(2, tx_b);
        }

        abort_all_in_flight(&in_flight);

        assert!(rx_a.await.is_ok(), "build 1 must receive the abort signal");
        assert!(rx_b.await.is_ok(), "build 2 must receive the abort signal");
        assert!(
            in_flight.lock().unwrap().is_empty(),
            "map must be drained after aborting all in-flight builds",
        );

        // Idempotent: a second call on the now-empty map is a no-op.
        abort_all_in_flight(&in_flight);
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
        let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>(4096);
        let (_abort_tx, abort_rx) = oneshot::channel::<()>();
        handle_build(
            5,
            "/nix/store/x.drv".into(),
            None,
            false,
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

#[cfg(test)]
mod watchdog_tests {
    //! The self-liveness watchdog: it must fire when the connection
    //! stops making progress, and stay quiet while progress continues.
    //! Uses short real-time windows (the workspace tokio build does not
    //! enable `test-util`, so paused-clock tests are unavailable).
    use super::*;

    #[tokio::test]
    async fn fires_after_no_progress() {
        let clock = Arc::new(ProgressClock::new());
        clock.bump();
        // The watchdog must resolve once `idle` exceeds the 150ms window.
        let fired = tokio::time::timeout(
            Duration::from_secs(2),
            liveness_watchdog(clock, Duration::from_millis(150), Duration::from_millis(20)),
        )
        .await;
        assert!(
            fired.is_ok(),
            "watchdog should fire after the no-progress window elapses",
        );
    }

    #[tokio::test]
    async fn held_off_by_ongoing_progress() {
        let clock = Arc::new(ProgressClock::new());
        clock.bump();
        let wd = liveness_watchdog(
            clock.clone(),
            Duration::from_millis(150),
            Duration::from_millis(20),
        );
        tokio::pin!(wd);
        // Bump every 50ms (well within the 150ms window) for ~500ms; the
        // watchdog must never fire while progress keeps coming.
        for _ in 0..10 {
            tokio::select! {
                biased;
                _ = &mut wd => panic!("watchdog fired despite ongoing progress"),
                _ = tokio::time::sleep(Duration::from_millis(50)) => clock.bump(),
            }
        }
    }

    #[tokio::test]
    async fn idle_reports_elapsed_since_bump() {
        let clock = ProgressClock::new();
        clock.bump();
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            clock.idle() >= Duration::from_millis(50),
            "idle must reflect time since the last bump, got {:?}",
            clock.idle(),
        );
        clock.bump();
        assert!(
            clock.idle() < Duration::from_millis(40),
            "a fresh bump must reset idle, got {:?}",
            clock.idle(),
        );
    }
}
