use crate::auth::AuthState;
use crate::host_key::HostKey;
use crate::protocol::{ControlMessage, LineFramer};
use crate::registry::{
    BuildLifecycle, BuilderRegistry, ConnState, ConnectedBuilder, RusshSession, TryForward,
};
use argunix_domain::{BuilderCapabilities, BuilderName, BuilderPubkey};
use argunix_store::{BuilderStore, NewBuilder, SqlxStore};
use base64::Engine as _;
use russh::keys::PrivateKey;
use russh::keys::ssh_key;
use russh::server::{Auth, Handle as SessionHandle, Handler, Msg, Server, Session};
use russh::{Channel, ChannelId, Disconnect, MethodKind, MethodSet};
use socket2::{SockRef, TcpKeepalive};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;
use tokio::task::AbortHandle;

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("running russh server on {addr}: {source}")]
    Run {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("converting host key: {0}")]
    HostKey(String),
    #[error("store: {0}")]
    Store(#[from] argunix_store::StoreError),
}

/// Inputs to spin up the embedded SSH server. One struct so `run` has a
/// stable signature even when we add knobs (idle timeout, max channels,
/// allow-list, …) later.
pub struct ServerConfig {
    pub listen: SocketAddr,
    pub host_key: HostKey,
    /// Bytes of the shared enrollment token. Compared verbatim against the
    /// password-method credential. Constant-time compare via `constant_time_eq`
    /// below; the API takes the bytes so the caller can reload from the
    /// credentials file on a hot-reload without re-deriving anything.
    pub enrollment_token: Arc<Vec<u8>>,
    pub store: Arc<SqlxStore>,
    /// Runtime view of which builders are currently connected. PR #7's
    /// dispatcher reads it; argunixctl reads it.
    pub registry: Arc<BuilderRegistry>,
}

/// Marker name kept for `pub use` consumers; the actual entry point is
/// the free function [`run`].
pub struct BuilderServer;

impl BuilderServer {
    /// Run the listener until cancelled. Constructs an `russh::server::Config`
    /// with the supplied host key and the argunix auth method set
    /// (password OR publickey).
    pub async fn run(cfg: ServerConfig) -> Result<(), ServerError> {
        let key =
            signing_key_to_russh(&cfg.host_key).map_err(|e| ServerError::HostKey(e.to_string()))?;

        let methods: MethodSet = (&[MethodKind::Password, MethodKind::PublicKey][..]).into();
        let russh_cfg = russh::server::Config {
            keys: vec![key],
            methods,
            auth_rejection_time: std::time::Duration::from_secs(1),
            inactivity_timeout: Some(std::time::Duration::from_secs(120)),
            // App-layer keepalive: probe each builder every 30s and
            // tear the session down after 3 unanswered probes (~90s to
            // detect a half-open connection). Symmetric with the
            // agent's client-side keepalive — without it a builder
            // whose connection silently dies (NAT mapping expiry, host
            // vanished) lingers as a zombie registry entry, and any
            // side channels opened on it stay wedged until the kernel's
            // TCP retransmit budget runs out, minutes later.
            keepalive_interval: Some(std::time::Duration::from_secs(30)),
            keepalive_max: 3,
            nodelay: true,
            // Enlarge the per-channel flow-control window (russh's 2 MiB
            // default is too small for the bidirectional nix-daemon
            // tunnel and deadlocks a large closure push). This governs
            // the builder→coordinator (response/progress) direction.
            // See `channel_io::BUILDER_SESSION_WINDOW_SIZE`.
            window_size: crate::channel_io::BUILDER_SESSION_WINDOW_SIZE,
            maximum_packet_size: crate::channel_io::BUILDER_SESSION_MAX_PACKET,
            ..Default::default()
        };
        let russh_cfg = Arc::new(russh_cfg);

        let mut server = ServerInner {
            store: cfg.store,
            enrollment_token: cfg.enrollment_token,
            registry: cfg.registry,
        };
        let listen = cfg.listen;
        // Accept connections ourselves rather than via `run_on_address`
        // so we can set SO_KEEPALIVE on each accepted socket. The russh
        // keepalive above already detects half-open connections at the
        // SSH layer; TCP keepalive is the same belt-and-suspenders the
        // agent runs, and trips faster when the peer is fully
        // unreachable.
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .map_err(|source| ServerError::Run {
                addr: listen,
                source,
            })?;
        loop {
            let (socket, peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(source) => {
                    return Err(ServerError::Run {
                        addr: listen,
                        source,
                    });
                }
            };
            let keepalive = TcpKeepalive::new()
                .with_time(std::time::Duration::from_secs(30))
                .with_interval(std::time::Duration::from_secs(10))
                .with_retries(3);
            if let Err(e) = SockRef::from(&socket).set_tcp_keepalive(&keepalive) {
                tracing::warn!(error = %e, %peer, "set_tcp_keepalive() on builder socket failed");
            }
            let handler = server.new_client(Some(peer));
            // Capture the slot before the handler is moved into the
            // session task; we publish the task's AbortHandle into it
            // once spawned so the registry (watchdog / takeover) can
            // force this connection's socket closed even when the
            // session loop is wedged on a blocked outbound flush.
            let abort_slot = handler.abort_slot.clone();
            let russh_cfg = russh_cfg.clone();
            let join = tokio::spawn(async move {
                match russh::server::run_stream(russh_cfg, socket, handler).await {
                    Ok(session) => {
                        if let Err(e) = session.await {
                            tracing::debug!(error = %e, %peer, "builder connection closed with error");
                        }
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, %peer, "builder connection setup failed");
                    }
                }
            });
            let _ = abort_slot.set(join.abort_handle());
        }
    }
}

#[derive(Clone)]
struct ServerInner {
    store: Arc<SqlxStore>,
    enrollment_token: Arc<Vec<u8>>,
    registry: Arc<BuilderRegistry>,
}

impl Server for ServerInner {
    type Handler = ConnectionHandler;
    fn new_client(&mut self, _peer: Option<SocketAddr>) -> ConnectionHandler {
        let connection_id = self.registry.next_connection_id();
        ConnectionHandler {
            store: self.store.clone(),
            enrollment_token: self.enrollment_token.clone(),
            registry: self.registry.clone(),
            connection_id,
            state: Arc::new(Mutex::new(AuthState::Unauthenticated)),
            offered_pubkey: Arc::new(Mutex::new(None)),
            framers: Arc::new(Mutex::new(HashMap::new())),
            control_channel: Arc::new(Mutex::new(None)),
            registered_name: Arc::new(std::sync::Mutex::new(None)),
            abort_slot: Arc::new(OnceLock::new()),
            log_drops: Arc::new(std::sync::Mutex::new(HashSet::new())),
        }
    }
}

/// One connection's worth of handler state. russh owns the handler;
/// it's not cloned per callback, but the mutable bits live behind
/// `Arc<Mutex<_>>` for symmetry with the registry's std::sync::Mutex.
pub(crate) struct ConnectionHandler {
    store: Arc<SqlxStore>,
    enrollment_token: Arc<Vec<u8>>,
    registry: Arc<BuilderRegistry>,
    /// Distinguishes this connection from any other for the same
    /// builder name; used by `BuilderRegistry::remove_if_matches` so
    /// a stale handler dropping after a takeover can't yank the
    /// successor's registration.
    connection_id: u64,
    state: Arc<Mutex<AuthState>>,
    /// Captured during pubkey-offered even when the key isn't (yet) in
    /// the `builders` table — needed so `FreshEnrollment` can carry the
    /// key the client used during the SSH handshake. Without this, when
    /// the token-auth path succeeds we would have no pubkey to persist.
    offered_pubkey: Arc<Mutex<Option<BuilderPubkey>>>,
    /// Per-channel line-framing buffers. SSH delivers bytes as a stream
    /// with arbitrary chunk boundaries, so we accumulate per-channel
    /// until we see `\n` and only then parse the JSON.
    framers: Arc<Mutex<HashMap<ChannelId, LineFramer>>>,
    /// The first session channel the agent opens is treated as the
    /// control channel.
    control_channel: Arc<Mutex<Option<ChannelId>>>,
    /// Name we registered under, if `hello` succeeded. `Drop` reads
    /// this synchronously (so it can't be a tokio Mutex).
    registered_name: Arc<std::sync::Mutex<Option<BuilderName>>>,
    /// The session task's `AbortHandle`, published by
    /// [`BuilderServer::run`] right after it spawns the task. Stored in
    /// the [`ConnectedBuilder`] on `hello` so the registry can abort a
    /// wedged session (watchdog eviction / takeover). A `OnceLock`
    /// because the handle only exists after the spawn, which is after
    /// `new_client` builds this handler.
    abort_slot: Arc<OnceLock<AbortHandle>>,
    /// Build ids that have had at least one `BuildLogChunk` dropped
    /// because the worker's lifecycle channel was full. The session
    /// read loop never blocks on worker back-pressure (that coupling
    /// reaped healthy builders under load); instead it drops the chunk
    /// and records the build here so the terminal `BuildFinished` can
    /// mark the stored log truncated. Entries are removed when the
    /// `BuildFinished` is forwarded.
    log_drops: Arc<std::sync::Mutex<HashSet<i64>>>,
}

impl ConnectionHandler {
    /// PR #5 reads this from the control-channel handler to decide
    /// whether an incoming `hello` is a fresh enrollment (writes a new
    /// `builders` row) or a refresh (overwrites capabilities on the
    /// existing row).
    #[allow(dead_code)]
    pub(crate) async fn auth_state(&self) -> AuthState {
        self.state.lock().await.clone()
    }
}

impl Drop for ConnectionHandler {
    fn drop(&mut self) {
        // Synchronous cleanup: take the registered name (if any) and
        // remove it from the registry only if it still matches THIS
        // connection's id. A takeover that already replaced this
        // connection bumped the row's `connection_id`, so the
        // remove_if_matches predicate evaluates false and the new
        // registration is preserved.
        let name = self.registered_name.lock().unwrap().take();
        if let Some(n) = name {
            self.registry.remove_if_matches(&n, self.connection_id);
        }
    }
}

impl Handler for ConnectionHandler {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        // Constant-time comparison so timing doesn't leak token length /
        // matching prefix bytes.
        let presented = password.as_bytes();
        let expected: &[u8] = self.enrollment_token.as_ref();
        if !constant_time_eq(presented, expected) {
            tracing::warn!(user = %user, "builder auth_password rejected: token mismatch");
            return Ok(Auth::reject());
        }
        let pubkey = self.offered_pubkey.lock().await.clone();
        let new_state = match pubkey {
            Some(pk) => AuthState::FreshEnrollment { pubkey: pk },
            None => {
                tracing::warn!(
                    user = %user,
                    "builder auth_password accepted but no pubkey offered; \
                     fresh enrollment will be deferred until the agent \
                     re-presents its key"
                );
                AuthState::Unauthenticated
            }
        };
        *self.state.lock().await = new_state;
        tracing::info!(user = %user, "builder auth_password accepted");
        Ok(Auth::Accept)
    }

    async fn auth_publickey_offered(
        &mut self,
        _user: &str,
        public_key: &ssh_key::PublicKey,
    ) -> Result<Auth, Self::Error> {
        // Capture the offered key regardless of whether it ends up
        // matching a builders row. If pubkey-auth fails, the agent will
        // retry with the enrollment token (password method); we still
        // need the key bytes then to write the TOFU record.
        if let Some(pk) = ed25519_pubkey_bytes(public_key) {
            *self.offered_pubkey.lock().await = Some(pk);
        }
        // The offer phase is non-binding — the agent must follow up
        // with a signed `auth_publickey` to actually authenticate.
        Ok(Auth::Accept)
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        public_key: &ssh_key::PublicKey,
    ) -> Result<Auth, Self::Error> {
        let Some(raw) = ed25519_pubkey_bytes(public_key) else {
            tracing::warn!(user = %user, "builder auth_publickey rejected: non-ed25519 key");
            return Ok(Auth::reject());
        };
        *self.offered_pubkey.lock().await = Some(raw.clone());
        match self.store.find_active_by_pubkey(&raw).await {
            Ok(Some(record)) => {
                tracing::info!(
                    builder = %record.name,
                    user = %user,
                    "builder auth_publickey accepted",
                );
                *self.state.lock().await = AuthState::Established(record);
                Ok(Auth::Accept)
            }
            Ok(None) => {
                tracing::warn!(
                    user = %user,
                    "builder auth_publickey rejected: no active row matches",
                );
                Ok(Auth::reject())
            }
            Err(e) => {
                tracing::error!(error = %e, "store error during pubkey lookup");
                Ok(Auth::reject())
            }
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        let id = channel.id();
        let mut framers = self.framers.lock().await;
        framers.entry(id).or_insert_with(LineFramer::new);
        let mut control = self.control_channel.lock().await;
        if control.is_none() {
            *control = Some(id);
        }
        Ok(true)
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let is_control = {
            let control = self.control_channel.lock().await;
            *control == Some(channel)
        };
        if !is_control {
            // PR #6 will route non-control channels to build sessions.
            return Ok(());
        }
        let messages = {
            let mut framers = self.framers.lock().await;
            framers
                .entry(channel)
                .or_insert_with(LineFramer::new)
                .extend(data)
        };
        for parsed in messages {
            match parsed {
                Err(e) => {
                    tracing::warn!(error = %e, "control channel: dropping malformed line");
                }
                Ok(msg) => self.handle_control(channel, msg, session).await?,
            }
        }
        Ok(())
    }
}

impl ConnectionHandler {
    /// Forward a build lifecycle event onto the registry's
    /// per-(builder, build_id) mpsc, if a worker has registered one.
    /// Silently no-ops if the event arrives for an unknown build —
    /// most likely the worker already gave up (cancel / disconnect)
    /// and unregistered, or a misbehaving agent is sending build_ids
    /// it never received a `Build` for.
    /// Non-blocking forward of a non-terminal lifecycle event
    /// (`Started` / `LogChunk`) to the worker. **Never awaits** — this
    /// runs in russh's single session read loop, where a blocking send
    /// stalls every channel on the connection (heartbeats included) and
    /// gets the builder reaped under load. Returns `true` if the chunk
    /// was dropped because the worker's channel was full, so the caller
    /// can remember to mark the stored log truncated.
    fn forward_nonblocking(&self, build_id: i64, event: BuildLifecycle) -> bool {
        let name = self.registered_name.lock().unwrap().clone();
        let Some(name) = name else {
            tracing::warn!(build_id, "build-lifecycle message before hello; dropping",);
            return false;
        };
        match self
            .registry
            .try_forward_build_event(&name, build_id, event)
        {
            TryForward::Delivered => false,
            TryForward::NoReceiver => {
                tracing::debug!(
                    builder = %name,
                    build_id,
                    "build-lifecycle message for unregistered build; dropping",
                );
                false
            }
            TryForward::Full(_) => true,
        }
    }

    /// Deliver the terminal `BuildFinished` event reliably without
    /// blocking the session read loop. Fast-path `try_send`; if the
    /// worker is behind, hand the event to a detached task that does the
    /// bounded-blocking send. Nothing follows `Finished` for a build, so
    /// off-loading it cannot reorder events.
    fn forward_finished(&self, build_id: i64, event: BuildLifecycle) {
        let name = self.registered_name.lock().unwrap().clone();
        let Some(name) = name else {
            tracing::warn!(build_id, "build-finished before hello; dropping",);
            return;
        };
        match self
            .registry
            .try_forward_build_event(&name, build_id, event)
        {
            TryForward::Delivered => {}
            TryForward::NoReceiver => {
                tracing::debug!(
                    builder = %name,
                    build_id,
                    "build-finished for unregistered build; dropping",
                );
            }
            TryForward::Full(event) => {
                let registry = self.registry.clone();
                tokio::spawn(async move {
                    registry.forward_build_event(&name, build_id, event).await;
                });
            }
        }
    }

    async fn handle_control(
        &self,
        channel: ChannelId,
        msg: ControlMessage,
        session: &mut Session,
    ) -> Result<(), russh::Error> {
        // Any inbound control frame proves the agent's session loop is
        // alive — refresh the liveness clock regardless of message type,
        // and before any `.await` below can stall the read loop. The
        // heartbeat is merely the frame that guarantees a minimum
        // cadence; a builder busy streaming lifecycle events must not be
        // reaped for skipping beats.
        if let Some(name) = self.registered_name.lock().unwrap().clone() {
            self.registry.touch_activity(&name);
        }
        match msg {
            ControlMessage::Hello {
                name,
                systems,
                native_system,
                features,
                max_jobs,
                nix_version,
            } => {
                let caps = BuilderCapabilities {
                    systems,
                    native_system,
                    features,
                    max_jobs,
                    nix_version,
                };
                let pre_state = self.state.lock().await.clone();
                let outcome = match pre_state {
                    AuthState::FreshEnrollment { pubkey } => {
                        // First contact via token: write a new
                        // builders row with the captured pubkey + the
                        // self-described name and capabilities.
                        let now = chrono::Utc::now();
                        match self
                            .store
                            .upsert(
                                NewBuilder {
                                    name: name.clone(),
                                    pubkey,
                                    capabilities: caps,
                                },
                                now,
                            )
                            .await
                        {
                            Ok(id) => match self.store.get(id).await {
                                Ok(Some(record)) => Some(record),
                                Ok(None) => {
                                    tracing::error!(
                                        "freshly enrolled builder row id={} disappeared",
                                        id.get()
                                    );
                                    None
                                }
                                Err(e) => {
                                    tracing::error!(error = %e, "fetching freshly enrolled builder");
                                    None
                                }
                            },
                            Err(e) => {
                                // A `BuilderNameConflict` here means a
                                // token-authenticated client tried to
                                // enroll under a name already bound to a
                                // different key (or a revoked one) — a
                                // name-hijack / revocation-bypass attempt.
                                // Reject the enrollment. See bugs.md
                                // SEC-4 / SEC-8.
                                tracing::warn!(
                                    error = %e,
                                    builder = %name,
                                    "rejecting fresh enrollment (name bound to another key or revoked)",
                                );
                                None
                            }
                        }
                    }
                    AuthState::Established(record) => {
                        // Reconnect: ignore any rename attempt the
                        // agent's hostname change might be implying.
                        // Operators rename via argunixctl, not via the
                        // wire protocol.
                        if record.name.as_str() != name.as_str() {
                            tracing::warn!(
                                row_name = %record.name,
                                hello_name = %name,
                                "builder hello.name differs from sqlite row; using row name",
                            );
                        }
                        // Refresh capabilities. Upsert is keyed on
                        // name (== record.name); pubkey already
                        // matched during auth so we re-bind it
                        // verbatim.
                        let now = chrono::Utc::now();
                        match self
                            .store
                            .upsert(
                                NewBuilder {
                                    name: record.name.clone(),
                                    pubkey: record.pubkey.clone(),
                                    capabilities: caps,
                                },
                                now,
                            )
                            .await
                        {
                            Ok(id) => match self.store.get(id).await {
                                Ok(Some(refreshed)) => Some(refreshed),
                                _ => Some(record),
                            },
                            Err(e) => {
                                tracing::error!(error = %e, "capabilities refresh failed");
                                Some(record)
                            }
                        }
                    }
                    AuthState::Unauthenticated => {
                        tracing::warn!(
                            "control channel: hello received before successful auth; closing",
                        );
                        let _ = session.close(channel);
                        return Ok(());
                    }
                };

                let Some(record) = outcome else {
                    let _ = session.close(channel);
                    return Ok(());
                };

                let welcome = ControlMessage::Welcome {
                    builder_id: record.id.get().to_string(),
                };
                let bytes: bytes::Bytes = welcome.encode_line().into();
                if let Err(e) = session.data(channel, bytes) {
                    tracing::warn!(error = ?e, "writing welcome failed");
                }
                tracing::info!(
                    builder = %record.name,
                    builder_id = record.id.get(),
                    "builder hello accepted",
                );

                // Register in the runtime view. A takeover under the
                // same name returns the prior session so we can fire a
                // Kick + disconnect off-task.
                let session_handle = Arc::new(session.handle());
                let connected = ConnectedBuilder {
                    builder_id: record.id,
                    capabilities: record.capabilities.clone(),
                    state: ConnState::Active,
                    connected_since: chrono::Utc::now(),
                    connection_id: self.connection_id,
                    session: Some(RusshSession {
                        handle: session_handle.clone(),
                        control_channel: channel,
                    }),
                    last_activity: std::time::Instant::now(),
                    abort: self.abort_slot.get().cloned(),
                };
                let displaced = self.registry.register(record.name.clone(), connected);
                *self.registered_name.lock().unwrap() = Some(record.name.clone());
                if let Some(prev) = displaced {
                    let name = prev.name.clone();
                    let prev_session = prev.session;
                    let prev_abort = prev.abort;
                    tokio::spawn(async move {
                        kick_displaced(name, prev_session, prev_abort).await;
                    });
                }

                *self.state.lock().await = AuthState::Established(record);
            }
            ControlMessage::Heartbeat { ts, stats } => {
                let state = self.state.lock().await.clone();
                let AuthState::Established(record) = state else {
                    tracing::warn!("control channel: heartbeat before hello; ignoring",);
                    return Ok(());
                };
                let now = chrono::Utc::now();
                // Liveness was already refreshed at the top of
                // handle_control (before any `.await`); the heartbeat
                // handler only carries the stats payload and the DB
                // last-seen bookkeeping.
                if let Some(stats) = stats {
                    self.registry.push_stats(&record.name, now, stats);
                }
                if let Err(e) = self.store.mark_seen(record.id, now).await {
                    tracing::warn!(error = %e, "mark_seen failed");
                }
                tracing::trace!(
                    builder = %record.name,
                    ts,
                    has_stats = stats.is_some(),
                    "heartbeat",
                );
            }
            ControlMessage::Shutdown { reason, drain } => {
                let state = self.state.lock().await.clone();
                if let AuthState::Established(record) = state {
                    tracing::info!(
                        builder = %record.name,
                        reason = %reason,
                        drain,
                        "builder shutting down",
                    );
                    // Stop the dispatcher from sending new build
                    // channels to this builder. The actual close
                    // happens shortly after on the agent side; we
                    // could keep accepting in-flight builds in the
                    // meantime, but PR #6 has none — interrupted
                    // recovery (PR #8) will requeue them anyway.
                    self.registry.mark_disconnecting(&record.name);
                } else {
                    tracing::info!(
                        reason = %reason,
                        drain,
                        "unauthenticated agent reports shutdown; ignoring",
                    );
                }
            }
            ControlMessage::Welcome { .. }
            | ControlMessage::Kick { .. }
            | ControlMessage::Build { .. }
            | ControlMessage::Abort { .. } => {
                // These are server→builder messages; receiving them
                // from the builder is a protocol violation.
                tracing::warn!(
                    "control channel: server-only message received from builder; ignoring",
                );
            }
            ControlMessage::BuildStarted { build_id, pid } => {
                // Non-terminal; best-effort. This is the first event, so
                // the worker channel is empty — a drop here would only
                // lose the pid, never liveness.
                self.forward_nonblocking(build_id, BuildLifecycle::Started { pid });
            }
            ControlMessage::BuildLogChunk {
                build_id,
                bytes_b64,
            } => match base64::engine::general_purpose::STANDARD.decode(&bytes_b64) {
                Ok(bytes) => {
                    if self.forward_nonblocking(build_id, BuildLifecycle::LogChunk { bytes }) {
                        // Worker is behind. Drop the chunk rather than
                        // stall the read loop (which would starve
                        // heartbeats and get this builder evicted), and
                        // remember to mark the log truncated at Finished.
                        self.log_drops.lock().unwrap().insert(build_id);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        build_id,
                        "control channel: BuildLogChunk base64 invalid; dropping",
                    );
                }
            },
            ControlMessage::BuildFinished {
                build_id,
                status,
                exit_code,
                output_paths,
                log_truncated,
            } => {
                let dropped = self.log_drops.lock().unwrap().remove(&build_id);
                self.forward_finished(
                    build_id,
                    BuildLifecycle::Finished {
                        status,
                        exit_code,
                        output_paths,
                        log_truncated: log_truncated || dropped,
                    },
                );
            }
        }
        Ok(())
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

/// Extract the raw 32 bytes of an ed25519 ssh public key. Returns None
/// for any other algorithm.
fn ed25519_pubkey_bytes(public_key: &ssh_key::PublicKey) -> Option<BuilderPubkey> {
    let raw = public_key.key_data().ed25519().map(|k| k.0)?;
    BuilderPubkey::from_bytes(&raw).ok()
}

fn signing_key_to_russh(host_key: &HostKey) -> Result<PrivateKey, russh::keys::Error> {
    let seed = host_key.signing_key().to_bytes();
    let kp = ssh_key::private::Ed25519Keypair::from_seed(&seed);
    let kpd = ssh_key::private::KeypairData::Ed25519(kp);
    PrivateKey::new(kpd, "argunix-builders").map_err(Into::into)
}

/// Send a Kick on the displaced builder's control channel and
/// disconnect its session, then abort its session task as a backstop.
///
/// The graceful `kick` + `disconnect` is best-effort: it routes through
/// the old session's run loop, which may be wedged on a blocked
/// outbound flush (the classic slept-laptop-mid-transfer case is
/// exactly why this connection got displaced). The `abort` is the
/// reliable lever — it drops the session task, closing its socket so
/// any side-channel transfer still bridged onto the old connection
/// errors out instead of hanging on the kernel's TCP retransmit budget.
async fn kick_displaced(
    name: BuilderName,
    session: Option<RusshSession>,
    abort: Option<AbortHandle>,
) {
    if let Some(session) = session {
        let kick = ControlMessage::Kick {
            reason: "another connection registered under the same name".into(),
        };
        let bytes: bytes::Bytes = kick.encode_line().into();
        // Bound the graceful path so a wedged loop can't delay the
        // abort backstop below indefinitely.
        let graceful = async {
            let _ = session.handle.data(session.control_channel, bytes).await;
            let _ = session
                .handle
                .disconnect(
                    Disconnect::ByApplication,
                    "displaced by a new connection".into(),
                    "en".into(),
                )
                .await;
        };
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), graceful).await;
    }
    if let Some(abort) = abort {
        abort.abort();
    }
    tracing::info!(builder = %name, "kicked displaced connection");
}

// Silence unused-import diagnostic in builds where SessionHandle is
// only referenced via Arc<...> in the registry struct.
#[allow(dead_code)]
type _SessionHandleAlias = SessionHandle;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_only_when_lengths_and_bytes_agree() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"abc", b""));
    }
}
