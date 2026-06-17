//! In-memory map of currently-connected builders.
//!
//! Backs both the dispatcher (PR #7 — "give me a builder that can build
//! `<system>` and isn't at `max_jobs`") and `argunixctl builders`. Lifetime
//! is bound to the underlying SSH connection: insertion happens when the
//! agent's `hello` is accepted, removal when the connection drops.
//!
//! Persistent state — pubkey, capabilities snapshot — lives in the
//! `builders` sqlite table (see `argunix-store::BuilderStore`). The
//! registry is the *runtime* view, not a cache of sqlite.

use crate::protocol::{BuildOutcomeStatus, BuilderStats};
use argunix_domain::{BuilderCapabilities, BuilderId, BuilderName};
use chrono::{DateTime, Utc};
use russh::ChannelId;
use russh::server::Handle as SessionHandle;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

/// One stats sample with the wall-clock time at which the coordinator
/// received it. Surfaced to the web UI as the JSON shape under
/// `GET /api/builders/{name}/stats`.
#[derive(Debug, Clone, Copy)]
pub struct StatsSample {
    pub ts: DateTime<Utc>,
    pub stats: BuilderStats,
}

/// Per-builder ring of recent stats samples. ~5 minutes at 5s cadence.
/// Stored in-memory only — restarts wipe the window, which is fine for
/// the "is this thing alive" UX (we don't keep history past now).
const STATS_RING_CAPACITY: usize = 60;

/// Whether the dispatcher should consider a connection for new work.
///
/// `Disconnecting` is set on receipt of a `shutdown` message from the
/// agent (graceful stop) so we stop sending new build channels even
/// though the SSH connection is still up. The connection is removed
/// outright when the SSH session ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Active,
    Disconnecting,
}

/// SSH-side details for one connection. Kept separate from
/// [`ConnectedBuilder`] so unit tests can build registry entries
/// without standing up a real russh session.
#[derive(Clone)]
pub struct RusshSession {
    pub handle: Arc<SessionHandle>,
    pub control_channel: ChannelId,
}

/// One connected builder. Owned by [`BuilderRegistry`].
pub struct ConnectedBuilder {
    pub builder_id: BuilderId,
    pub capabilities: BuilderCapabilities,
    pub state: ConnState,
    pub connected_since: DateTime<Utc>,
    /// Unique-per-connection value. A stale handler's Drop must not
    /// remove the row that a takeover just inserted; we compare this
    /// id at remove time.
    pub connection_id: u64,
    /// `Some` in production (set by the SSH server's `data` callback
    /// after a successful `hello`); `None` in unit tests that don't
    /// drive a real connection. PR #7 will require `Some` to open
    /// build channels.
    pub session: Option<RusshSession>,
    /// Monotonic timestamp of the last sign of life from this builder
    /// (its `hello`, then refreshed on every `heartbeat`). Read by the
    /// liveness watchdog to detect a builder that went silent —
    /// independent of russh/TCP keepalive, which can be starved when
    /// the peer freezes mid-transfer and our outbound flush blocks.
    pub last_heartbeat: Instant,
    /// `Some` in production: the abort handle for the tokio task
    /// running this connection's russh session loop. The watchdog and
    /// the takeover path call `.abort()` on it to force the underlying
    /// socket closed, so any in-flight side-channel transfer
    /// (`nix copy` push/pull) wedged on a dead channel errors out
    /// promptly instead of waiting on the kernel's TCP retransmit
    /// budget. `None` in unit tests without a real connection.
    pub abort: Option<AbortHandle>,
}

/// A snapshot of one builder's runtime state — what `argunixctl builders`
/// and the dispatcher consume. Decoupled from `ConnectedBuilder` so
/// callers don't carry a borrow on the registry's internal mutex.
#[derive(Debug, Clone)]
pub struct BuilderSnapshot {
    pub name: BuilderName,
    pub builder_id: BuilderId,
    pub capabilities: BuilderCapabilities,
    pub state: ConnState,
    pub connected_since: DateTime<Utc>,
    pub in_flight: u32,
    /// Identifies this *connection*, distinct from `builder_id` (the
    /// persistent row) and `name`. A builder that drops and reconnects
    /// keeps its name and row id but gets a fresh `connection_id`. The
    /// dispatch loop excludes transport-failed builders by this, not by
    /// name, so a reconnected builder is eligible for retry again.
    pub connection_id: u64,
}

/// Which transport/build phase a `(builder, build_id)` pair is
/// in right now. Set by the worker as it walks through
/// `dispatch_pool_build`; cleared on every exit (success, failure,
/// cancel, timeout) via [`PhaseGuard`] in the daemon. Surfaced to the
/// status page so operators see whether a builder is staging inputs
/// (`Push`), running the build (`Build`), or fetching outputs (`Pull`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildPhase {
    Push,
    Build,
    Pull,
}

impl BuildPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            BuildPhase::Push => "push",
            BuildPhase::Build => "build",
            BuildPhase::Pull => "pull",
        }
    }
}

#[derive(Default)]
pub struct BuilderRegistry {
    inner: Mutex<HashMap<BuilderName, ConnectedBuilder>>,
    /// Per-builder running count of dispatched build channels. PR #7
    /// will increment/decrement this; PR #6 just exposes it.
    in_flight: Mutex<HashMap<BuilderName, u32>>,
    /// Monotonic counter for `connection_id`. Wraps after 2^64 connects,
    /// which is fine.
    next_conn_id: AtomicU64,
    /// Per-(builder, build_id) lifecycle event channels. The
    /// worker registers a sender via `register_in_flight_build` before
    /// emitting a `Build` control message; the connection handler
    /// looks up `(name, build_id)` on every `BuildStarted /
    /// BuildLogChunk / BuildFinished` it receives and forwards the
    /// event to the matching mpsc. Keyed by `(BuilderName, i64)` so a
    /// reused build_id across builders (unlikely with sqlite-allocated
    /// JobIds, but cheap to defend) doesn't cross-fire.
    in_flight_builds: Mutex<HashMap<(BuilderName, i64), mpsc::Sender<BuildLifecycle>>>,
    /// Per-(builder, build_id) live phase. Worker writes via
    /// [`Self::set_phase`] and clears via [`Self::clear_phase`] (or a
    /// `PhaseGuard` so exit paths can't forget). Read by the status
    /// page to annotate running-job rows.
    phases: Mutex<HashMap<(BuilderName, i64), BuildPhase>>,
    /// Per-builder ring of the last [`STATS_RING_CAPACITY`] heartbeat
    /// stats samples. Written by the SSH server's heartbeat handler;
    /// read by the web layer to render live sparklines. Entries for a
    /// builder live until the builder reconnects (we wipe on register
    /// so a stale window from a previous incarnation doesn't show up).
    stats: Mutex<HashMap<BuilderName, VecDeque<StatsSample>>>,
}

/// A single event in a build's lifecycle. The daemon's worker
/// task drains a `mpsc::Receiver<BuildLifecycle>` returned by
/// [`BuilderRegistry::register_in_flight_build`] (registered before
/// the `Build` control message is sent) until it sees `Finished` or
/// the channel closes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildLifecycle {
    Started {
        pid: Option<u32>,
    },
    LogChunk {
        bytes: Vec<u8>,
    },
    Finished {
        status: BuildOutcomeStatus,
        exit_code: Option<i32>,
        output_paths: Vec<String>,
        log_truncated: bool,
    },
}

/// Outcome of a non-blocking [`BuilderRegistry::try_forward_build_event`].
#[derive(Debug)]
pub enum TryForward {
    /// Event was queued to the worker's lifecycle channel.
    Delivered,
    /// No worker is registered for this `(builder, build_id)` — the
    /// worker already gave up (cancel / disconnect) and unregistered,
    /// or the agent sent a bogus `build_id`.
    NoReceiver,
    /// The worker's channel is full. The event is returned so the
    /// caller can drop it (log chunks) or re-deliver it out-of-band
    /// (the terminal `Finished`).
    Full(BuildLifecycle),
}

/// What a takeover surfaces about the connection it just displaced. The
/// caller — running on a tokio task — uses these to send a `kick`
/// message and disconnect the old SSH session.
pub struct DisplacedConnection {
    pub name: BuilderName,
    pub session: Option<RusshSession>,
    /// Abort handle of the displaced connection's session task. The
    /// caller aborts it after sending `kick` so a wedged old session
    /// (e.g. a slept laptop that just reconnected under the same name)
    /// can't keep a side-channel transfer alive on its half-open
    /// socket.
    pub abort: Option<AbortHandle>,
}

impl BuilderRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Mint a fresh `connection_id` for a new ConnectedBuilder.
    pub fn next_connection_id(&self) -> u64 {
        self.next_conn_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Insert `conn` under `name`. If a prior connection was registered
    /// under the same name, evict it and return a [`DisplacedConnection`]
    /// so the caller can send a `kick` over the old session.
    pub fn register(
        &self,
        name: BuilderName,
        conn: ConnectedBuilder,
    ) -> Option<DisplacedConnection> {
        let mut map = self.inner.lock().unwrap();
        let prior = map.insert(name.clone(), conn);
        // Drop any stats from a previous incarnation so the UI doesn't
        // show a window that ends before this connection started.
        self.stats.lock().unwrap().remove(&name);
        // If we displaced a prior connection, its in-flight builds are
        // gone with the agent (a reconnecting builder has no memory of
        // them). Drop the lifecycle senders so workers waiting on
        // `recv()` see the channel close and exit their drain loop with
        // "builder disconnected mid-build" instead of wedging until the
        // per-job wall-clock timeout fires. The prior handler's `Drop`
        // can't do this from `remove_if_matches` because the new
        // connection's id no longer matches.
        if prior.is_some() {
            let drained = self.drain_in_flight_for(&name);
            if !drained.is_empty() {
                tracing::warn!(
                    builder = %name,
                    build_ids = ?drained,
                    "displaced builder had in-flight builds; closed lifecycle channels so workers fail them",
                );
            }
        }
        prior.map(|p| DisplacedConnection {
            name,
            session: p.session,
            abort: p.abort,
        })
    }

    /// Refresh the in-memory liveness timestamp for `name`. Called from
    /// the SSH server's heartbeat handler. No-op if the builder isn't
    /// registered (a heartbeat racing a removal).
    pub fn touch_heartbeat(&self, name: &BuilderName) {
        if let Some(c) = self.inner.lock().unwrap().get_mut(name) {
            c.last_heartbeat = Instant::now();
        }
    }

    /// Names of registered builders whose last heartbeat is older than
    /// `max_silence`. The watchdog evicts these — see [`Self::evict_dead`].
    pub fn stale_builders(&self, max_silence: Duration) -> Vec<BuilderName> {
        let now = Instant::now();
        self.inner
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, c)| now.duration_since(c.last_heartbeat) > max_silence)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Forcibly evict a builder the watchdog has declared dead.
    ///
    /// Removes the registry entry (so the dispatcher stops considering
    /// it), drains its in-flight build lifecycle senders + phases (so a
    /// worker blocked on `lifecycle.recv()` wakes and retries), and
    /// aborts its session task (so its socket closes and any in-flight
    /// side-channel `nix copy` transfer wedged on the dead channel
    /// errors out instead of hanging on the kernel's TCP retransmit
    /// budget). Returns `true` if a builder was actually evicted.
    pub fn evict_dead(&self, name: &BuilderName) -> bool {
        let abort = {
            let mut map = self.inner.lock().unwrap();
            match map.remove(name) {
                Some(c) => {
                    self.in_flight.lock().unwrap().remove(name);
                    self.stats.lock().unwrap().remove(name);
                    c.abort
                }
                None => return false,
            }
        };
        let drained = self.drain_in_flight_for(name);
        if let Some(a) = abort {
            a.abort();
        }
        tracing::warn!(
            builder = %name,
            build_ids = ?drained,
            "builder went silent past the liveness threshold; evicted and \
             aborted its session so in-flight builds fail over to another builder",
        );
        true
    }

    /// Drop all per-build lifecycle senders and live-phase entries for
    /// `name`. Workers blocked on the corresponding `mpsc::Receiver`
    /// observe the channel close on their next poll. Returns the
    /// build_ids whose senders were dropped (for logging).
    fn drain_in_flight_for(&self, name: &BuilderName) -> Vec<i64> {
        let mut drained = Vec::new();
        {
            let mut builds = self.in_flight_builds.lock().unwrap();
            // Collect keys first to avoid mutating while iterating.
            let keys: Vec<(BuilderName, i64)> =
                builds.keys().filter(|(n, _)| n == name).cloned().collect();
            for k in keys {
                drained.push(k.1);
                builds.remove(&k);
            }
        }
        {
            let mut phases = self.phases.lock().unwrap();
            let keys: Vec<(BuilderName, i64)> =
                phases.keys().filter(|(n, _)| n == name).cloned().collect();
            for k in keys {
                phases.remove(&k);
            }
        }
        drained
    }

    pub fn mark_disconnecting(&self, name: &BuilderName) {
        if let Some(c) = self.inner.lock().unwrap().get_mut(name) {
            c.state = ConnState::Disconnecting;
        }
    }

    /// Remove only if the registered entry's `connection_id` matches.
    /// Called from `ConnectionHandler::drop`; without the id check, a
    /// stale handler dropping *after* a takeover would tear down the
    /// fresh registration that displaced it.
    pub fn remove_if_matches(&self, name: &BuilderName, connection_id: u64) {
        let mut map = self.inner.lock().unwrap();
        let matches = map
            .get(name)
            .map(|c| c.connection_id == connection_id)
            .unwrap_or(false);
        if matches {
            map.remove(name);
            self.in_flight.lock().unwrap().remove(name);
            self.stats.lock().unwrap().remove(name);
            // Release inner-mutex before we touch in_flight_builds /
            // phases — the drain helper takes those locks itself, and
            // we don't want to hold three at once.
            drop(map);
            let drained = self.drain_in_flight_for(name);
            if !drained.is_empty() {
                tracing::warn!(
                    builder = %name,
                    build_ids = ?drained,
                    "builder disconnected with in-flight builds; closed lifecycle channels so workers fail them",
                );
            }
        }
    }

    pub fn snapshot(&self, name: &BuilderName) -> Option<BuilderSnapshot> {
        let map = self.inner.lock().unwrap();
        let c = map.get(name)?;
        let in_flight = self
            .in_flight
            .lock()
            .unwrap()
            .get(name)
            .copied()
            .unwrap_or(0);
        Some(BuilderSnapshot {
            name: name.clone(),
            builder_id: c.builder_id,
            capabilities: c.capabilities.clone(),
            state: c.state,
            connected_since: c.connected_since,
            in_flight,
            connection_id: c.connection_id,
        })
    }

    /// All currently-registered builders (Active + Disconnecting), name-ordered.
    pub fn list(&self) -> Vec<BuilderSnapshot> {
        let map = self.inner.lock().unwrap();
        let in_flight = self.in_flight.lock().unwrap();
        let mut out: Vec<BuilderSnapshot> = map
            .iter()
            .map(|(name, c)| BuilderSnapshot {
                name: name.clone(),
                builder_id: c.builder_id,
                capabilities: c.capabilities.clone(),
                state: c.state,
                connected_since: c.connected_since,
                in_flight: in_flight.get(name).copied().unwrap_or(0),
                connection_id: c.connection_id,
            })
            .collect();
        out.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        out
    }

    /// Active, non-excluded builders that can run `system`, after
    /// applying the **native tier**. This is the shared candidate set
    /// behind both [`Self::eligible`] and [`Self::any_matching_builder`].
    ///
    /// Native preference is absolute and capacity-blind: if *any*
    /// connected builder runs `system` natively (its
    /// `capabilities.native_system`), the emulated ones (which list
    /// `system` only via `extra-platforms`/binfmt) are dropped from the
    /// candidate set entirely — regardless of how loaded the native
    /// builders are. Routing both functions through this is what makes a
    /// job *wait* for a busy native builder rather than spill onto
    /// emulation: `eligible` comes back empty (no native slot) while
    /// `any_matching_builder` stays true (a native builder exists), which
    /// the worker reads as a capacity wait.
    ///
    /// Only when no native builder for `system` is connected at all do
    /// emulated builders become candidates. So if the lone native
    /// builder disconnects mid-wait, the next call admits emulation and
    /// the job proceeds there.
    fn system_candidates(&self, system: &str, exclude: &HashSet<u64>) -> Vec<BuilderSnapshot> {
        let supporting: Vec<BuilderSnapshot> = self
            .list()
            .into_iter()
            .filter(|b| b.state == ConnState::Active)
            .filter(|b| !exclude.contains(&b.connection_id))
            .filter(|b| b.capabilities.systems.iter().any(|s| s == system))
            .collect();
        let native_exists = supporting
            .iter()
            .any(|b| b.capabilities.native_system == system);
        if native_exists {
            supporting
                .into_iter()
                .filter(|b| b.capabilities.native_system == system)
                .collect()
        } else {
            supporting
        }
    }

    /// Builders that match `(system, features)`, are `Active`, aren't at
    /// `max_jobs`, aren't in `exclude`, and pass the native-tier gate
    /// (see [`Self::system_candidates`]). Sorted by `in_flight` ascending
    /// so callers can pick the least-loaded eligible builder.
    pub fn eligible(
        &self,
        system: &str,
        features: &[String],
        exclude: &HashSet<u64>,
    ) -> Vec<BuilderSnapshot> {
        let mut found: Vec<BuilderSnapshot> = self
            .system_candidates(system, exclude)
            .into_iter()
            .filter(|b| features.iter().all(|f| b.capabilities.features.contains(f)))
            .filter(|b| b.in_flight < b.capabilities.max_jobs)
            .collect();
        found.sort_by_key(|b| b.in_flight);
        found
    }

    /// Like [`Self::eligible`] but ignores the `max_jobs` filter — used
    /// by the dispatcher to tell "no matching builder exists" (interrupt
    /// the job) from "matching builders exist but are at capacity"
    /// (wait for a slot). Capacity contention is normal queueing, not
    /// a failure; "no match at all" is a real config / pool problem.
    ///
    /// Applies the same native-tier gate as [`Self::eligible`], so a job
    /// for a `system` with a connected-but-busy native builder reports
    /// `true` here (→ wait) even though `eligible` is empty.
    pub fn any_matching_builder(
        &self,
        system: &str,
        features: &[String],
        exclude: &HashSet<u64>,
    ) -> bool {
        self.system_candidates(system, exclude)
            .into_iter()
            .any(|b| features.iter().all(|f| b.capabilities.features.contains(f)))
    }

    /// Look up the SSH-side details for a registered builder. Used by
    /// PR #7 to open new build channels into the chosen builder.
    pub fn session(&self, name: &BuilderName) -> Option<RusshSession> {
        self.inner
            .lock()
            .unwrap()
            .get(name)
            .and_then(|c| c.session.clone())
    }

    /// Bump the per-builder in-flight build count. Called by the build
    /// worker once per dispatched derivation: increment before
    /// the build starts, decrement when it returns. This is the
    /// authoritative "how busy is this builder" gauge that drives both
    /// `eligible()`'s capacity check and the status page's per-builder
    /// counter.
    ///
    /// The channel layer (`BuilderDispatcher`) does **not** call these:
    /// counting channels conflates connection-pool size with build load
    /// and over-reports. The worker owns the counter so the gauge
    /// reflects "derivations currently realising on this builder".
    pub fn inc_in_flight(&self, name: &BuilderName) {
        *self
            .in_flight
            .lock()
            .unwrap()
            .entry(name.clone())
            .or_insert(0) += 1;
    }

    pub fn dec_in_flight(&self, name: &BuilderName) {
        let mut map = self.in_flight.lock().unwrap();
        if let Some(v) = map.get_mut(name) {
            *v = v.saturating_sub(1);
        }
    }

    /// Register a `(builder, build_id)` so subsequent
    /// `BuildStarted / BuildLogChunk / BuildFinished` events arriving
    /// on that builder's control channel get forwarded to the returned
    /// `Receiver`. The caller is expected to:
    ///
    ///  1. call this before sending the `Build` message,
    ///  2. drain the receiver until `Finished` (or the channel closes
    ///     because the connection dropped),
    ///  3. call [`Self::unregister_in_flight_build`] on completion or
    ///     cancellation so the map doesn't leak.
    ///
    /// Capacity sized for an LLVM-class internal-json firehose: a
    /// single 16 KiB `BuildLogChunk` carries dozens of events, and a
    /// busy compile easily pushes hundreds of events per second. The
    /// 64-slot version dropped on the slightest scheduler hiccup,
    /// taking out `actStart`/`actStop`/`resProgress` along with log
    /// lines and freezing the nom tree at whatever was building when
    /// the drops began. 4096 entries × ~16 KiB = ~64 MiB worst-case
    /// per build — bounded and acceptable, while leaving multi-second
    /// headroom before pressure even matters. The forward path still
    /// uses `try_send` (so a hard-stuck consumer cannot grow agent
    /// memory unboundedly via SSH back-pressure), but drops now
    /// require the consumer to be genuinely wedged, not just slow.
    pub fn register_in_flight_build(
        &self,
        name: BuilderName,
        build_id: i64,
    ) -> mpsc::Receiver<BuildLifecycle> {
        let (tx, rx) = mpsc::channel(4096);
        self.in_flight_builds
            .lock()
            .unwrap()
            .insert((name, build_id), tx);
        rx
    }

    /// Remove the in-flight entry. Idempotent — safe to call from
    /// both the worker's success path and a cancel path.
    pub fn unregister_in_flight_build(&self, name: &BuilderName, build_id: i64) {
        self.in_flight_builds
            .lock()
            .unwrap()
            .remove(&(name.clone(), build_id));
    }

    /// Forward a lifecycle event from the connection handler to the
    /// matching worker. Returns `false` if no entry was registered for
    /// `(name, build_id)` (most likely the worker already gave up and
    /// unregistered, or the agent is sending bogus build_ids).
    ///
    /// Blocking send: when the worker's lifecycle channel is full this
    /// awaits a slot rather than dropping the event. With the agent's
    /// outbound queue also bounded, the resulting back-pressure walks
    /// all the way back to `nix-store --realise`'s stderr writer on
    /// the builder, briefly stalling the build instead of corrupting
    /// the live view or the stored log. Memory at every hop is
    /// bounded by the channel capacities; no event is ever lost.
    ///
    /// `try_send`-first fast-path so the common (non-contended) case
    /// is non-blocking and the `.await` is reached only under
    /// pressure. The sender is cloned out from under the std `Mutex`
    /// before awaiting — never hold a sync mutex across `.await`.
    pub async fn forward_build_event(
        &self,
        name: &BuilderName,
        build_id: i64,
        event: BuildLifecycle,
    ) -> bool {
        let sender = {
            let map = self.in_flight_builds.lock().unwrap();
            match map.get(&(name.clone(), build_id)) {
                Some(tx) => tx.clone(),
                None => return false,
            }
        };
        match sender.try_send(event) {
            Ok(()) => true,
            Err(tokio::sync::mpsc::error::TrySendError::Full(event)) => {
                // Slow consumer: wait for a slot. Back-pressure propagates
                // through the russh receive task into the agent.
                sender.send(event).await.is_ok()
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// Non-blocking forward. Unlike [`Self::forward_build_event`] this
    /// **never awaits** — it is called from the SSH server's single
    /// session read loop (`Handler::data`), where a blocking send on a
    /// full worker channel would stall *every* channel on the
    /// connection, including the heartbeats the liveness watchdog reads.
    /// That coupling is what reaped healthy builders under sudden load.
    ///
    /// On a full channel the event is handed back in [`TryForward::Full`]
    /// so the caller decides: log chunks get dropped (and the build's
    /// stored log marked truncated), while the terminal `Finished` event
    /// is re-delivered out-of-band on a detached task so it is never
    /// lost. See `server::ConnectionHandler::handle_control`.
    pub fn try_forward_build_event(
        &self,
        name: &BuilderName,
        build_id: i64,
        event: BuildLifecycle,
    ) -> TryForward {
        let sender = {
            let map = self.in_flight_builds.lock().unwrap();
            match map.get(&(name.clone(), build_id)) {
                Some(tx) => tx.clone(),
                None => return TryForward::NoReceiver,
            }
        };
        match sender.try_send(event) {
            Ok(()) => TryForward::Delivered,
            Err(tokio::sync::mpsc::error::TrySendError::Full(event)) => TryForward::Full(event),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => TryForward::NoReceiver,
        }
    }

    /// Set the current build phase for `(name, build_id)`. Called by
    /// the daemon's worker as it advances through push → build → pull;
    /// idempotent (re-setting the same phase is a no-op).
    pub fn set_phase(&self, name: &BuilderName, build_id: i64, phase: BuildPhase) {
        self.phases
            .lock()
            .unwrap()
            .insert((name.clone(), build_id), phase);
    }

    /// Drop the live phase entry. Safe to call from any exit path.
    pub fn clear_phase(&self, name: &BuilderName, build_id: i64) {
        self.phases
            .lock()
            .unwrap()
            .remove(&(name.clone(), build_id));
    }

    /// Snapshot of all live phase entries, keyed by builder *name* (as
    /// String) so the UI doesn't need to round-trip through
    /// `BuilderName::new` on the parsed `String` it already holds.
    /// Read once per status-page render and looked up per running row.
    pub fn phase_snapshot(&self) -> HashMap<(String, i64), BuildPhase> {
        self.phases
            .lock()
            .unwrap()
            .iter()
            .map(|((n, id), p)| ((n.as_str().to_string(), *id), *p))
            .collect()
    }

    /// Append a stats sample to the per-builder ring, evicting the
    /// oldest if at capacity. Called from the SSH server's heartbeat
    /// handler.
    pub fn push_stats(&self, name: &BuilderName, ts: DateTime<Utc>, stats: BuilderStats) {
        let mut map = self.stats.lock().unwrap();
        let ring = map.entry(name.clone()).or_default();
        if ring.len() == STATS_RING_CAPACITY {
            ring.pop_front();
        }
        ring.push_back(StatsSample { ts, stats });
    }

    /// Snapshot the per-builder stats ring in chronological order
    /// (oldest first). Returns an empty vec if the builder isn't
    /// connected or has not sent a heartbeat with stats yet.
    pub fn stats_snapshot(&self, name: &BuilderName) -> Vec<StatsSample> {
        self.stats
            .lock()
            .unwrap()
            .get(name)
            .map(|ring| ring.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Looks up the builder name currently dispatching `build_id`, if
    /// any. Used by the job page to know whose stats ring to attach
    /// to a running job. O(phases-map size) — fine for human-scale
    /// numbers of concurrent builds.
    pub fn builder_for_build(&self, build_id: i64) -> Option<BuilderName> {
        self.phases
            .lock()
            .unwrap()
            .keys()
            .find(|(_, id)| *id == build_id)
            .map(|(name, _)| name.clone())
    }
}

/// Builder heartbeat cadence (the agent sends one every 30s; see
/// `design/builders.md`). The watchdog threshold is a multiple of this.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// How long a registered builder may go without a heartbeat before the
/// watchdog declares it dead. Three missed beats (~95s), matching the
/// russh keepalive intent — but enforced by an independent timer that
/// a wedged session loop cannot starve.
pub const LIVENESS_MAX_SILENCE: Duration = Duration::from_secs(95);

/// How often the watchdog scans the registry for silent builders.
pub const WATCHDOG_SCAN_INTERVAL: Duration = Duration::from_secs(15);

/// Spawn the liveness watchdog: every [`WATCHDOG_SCAN_INTERVAL`], evict
/// any builder that hasn't sent a heartbeat within
/// [`LIVENESS_MAX_SILENCE`]. This is the backstop that catches a
/// builder whose connection silently froze (a slept laptop, a NAT
/// mapping that expired mid-transfer) — cases where russh's own
/// keepalive is starved because the session loop is parked on a
/// blocked outbound flush. Eviction frees in-flight builds for retry
/// (see [`BuilderRegistry::evict_dead`]).
pub fn spawn_liveness_watchdog(registry: Arc<BuilderRegistry>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(WATCHDOG_SCAN_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            for name in registry.stale_builders(LIVENESS_MAX_SILENCE) {
                registry.evict_dead(&name);
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use argunix_domain::BuilderId;

    fn caps(systems: &[&str], features: &[&str], max_jobs: u32) -> BuilderCapabilities {
        // Mirror the agent: the first entry is the native `system`, the
        // rest are emulated `extra-platforms`. Tiering tests rely on this.
        BuilderCapabilities {
            systems: systems.iter().map(|s| s.to_string()).collect(),
            native_system: systems.first().map(|s| s.to_string()).unwrap_or_default(),
            features: features.iter().map(|s| s.to_string()).collect(),
            max_jobs,
            nix_version: "test".into(),
        }
    }

    fn conn(reg: &BuilderRegistry, builder_id: i64, caps: BuilderCapabilities) -> ConnectedBuilder {
        ConnectedBuilder {
            builder_id: BuilderId::new(builder_id),
            capabilities: caps,
            state: ConnState::Active,
            connected_since: Utc::now(),
            connection_id: reg.next_connection_id(),
            session: None,
            last_heartbeat: Instant::now(),
            abort: None,
        }
    }

    #[test]
    fn register_and_snapshot_round_trip() {
        let reg = BuilderRegistry::new();
        let name = BuilderName::new("a").unwrap();
        let displaced = reg.register(name.clone(), conn(&reg, 1, caps(&["x86_64-linux"], &[], 2)));
        assert!(displaced.is_none());
        let snap = reg.snapshot(&name).unwrap();
        assert_eq!(snap.builder_id.get(), 1);
        assert_eq!(snap.state, ConnState::Active);
        assert_eq!(snap.in_flight, 0);
    }

    #[test]
    fn duplicate_name_displaces_prior_and_returns_handle() {
        let reg = BuilderRegistry::new();
        let name = BuilderName::new("dup").unwrap();
        let _ = reg.register(name.clone(), conn(&reg, 1, caps(&["x86_64-linux"], &[], 1)));
        let prior = reg.register(name.clone(), conn(&reg, 2, caps(&["x86_64-linux"], &[], 1)));
        let prior = prior.expect("second register must displace the first");
        assert_eq!(prior.name.as_str(), "dup");
        // Registry now points at the *second* connection.
        let snap = reg.snapshot(&name).unwrap();
        assert_eq!(snap.builder_id.get(), 2);
    }

    #[test]
    fn remove_if_matches_only_removes_matching_connection_id() {
        let reg = BuilderRegistry::new();
        let name = BuilderName::new("a").unwrap();
        let first = conn(&reg, 1, caps(&["x86_64-linux"], &[], 1));
        let first_id = first.connection_id;
        let _ = reg.register(name.clone(), first);
        // Takeover.
        let second = conn(&reg, 2, caps(&["x86_64-linux"], &[], 1));
        let _ = reg.register(name.clone(), second);

        // The first connection's drop runs late; with the conn_id check
        // it must NOT evict the second registration.
        reg.remove_if_matches(&name, first_id);
        let snap = reg
            .snapshot(&name)
            .expect("second registration still present");
        assert_eq!(snap.builder_id.get(), 2);
    }

    #[tokio::test]
    async fn disconnect_drops_in_flight_lifecycle_senders() {
        // The wedge bug: a worker waiting on the lifecycle receiver
        // must observe channel-close (recv() -> None) when the builder
        // disconnects, instead of blocking until its wall-clock timeout.
        let reg = BuilderRegistry::new();
        let name = BuilderName::new("eu").unwrap();
        let first = conn(&reg, 1, caps(&["x86_64-linux"], &["kvm"], 4));
        let conn_id = first.connection_id;
        let _ = reg.register(name.clone(), first);

        let mut rx_a = reg.register_in_flight_build(name.clone(), 100);
        let mut rx_b = reg.register_in_flight_build(name.clone(), 101);
        reg.set_phase(&name, 100, BuildPhase::Build);
        reg.set_phase(&name, 101, BuildPhase::Pull);

        // Simulate plain disconnect via the same code path
        // `ConnectionHandler::drop` uses.
        reg.remove_if_matches(&name, conn_id);

        // Both receivers must now close.
        assert!(
            rx_a.recv().await.is_none(),
            "receiver A should observe close"
        );
        assert!(
            rx_b.recv().await.is_none(),
            "receiver B should observe close"
        );
        // Phases are wiped so the UI doesn't keep showing a phase for
        // a dead build.
        assert!(reg.phase_snapshot().is_empty());
    }

    #[tokio::test]
    async fn takeover_drops_prior_in_flight_lifecycle_senders() {
        // Reconnect path: the displaced agent's in-flight builds are
        // gone with it. The new agent will dispatch fresh builds; the
        // old senders MUST be dropped so the old workers wake up and
        // fail their jobs, freeing capacity.
        let reg = BuilderRegistry::new();
        let name = BuilderName::new("eu").unwrap();
        let _ = reg.register(name.clone(), conn(&reg, 1, caps(&["x86_64-linux"], &[], 1)));
        let mut rx_old = reg.register_in_flight_build(name.clone(), 100);
        reg.set_phase(&name, 100, BuildPhase::Build);

        // Reconnect — second `register` displaces the first.
        let _ = reg.register(name.clone(), conn(&reg, 1, caps(&["x86_64-linux"], &[], 1)));

        assert!(
            rx_old.recv().await.is_none(),
            "prior incarnation's lifecycle receiver should observe close on takeover",
        );
        assert!(reg.phase_snapshot().is_empty());
    }

    #[tokio::test]
    async fn disconnect_only_drains_the_named_builder() {
        // Other builders' in-flight lifecycle channels must keep working.
        let reg = BuilderRegistry::new();
        let dead = BuilderName::new("dead").unwrap();
        let alive = BuilderName::new("alive").unwrap();
        let dead_conn = conn(&reg, 1, caps(&["x86_64-linux"], &[], 1));
        let dead_conn_id = dead_conn.connection_id;
        let _ = reg.register(dead.clone(), dead_conn);
        let _ = reg.register(
            alive.clone(),
            conn(&reg, 2, caps(&["x86_64-linux"], &[], 1)),
        );
        let mut rx_dead = reg.register_in_flight_build(dead.clone(), 100);
        let tx_alive_present_before = reg
            .forward_build_event(&alive, 200, BuildLifecycle::Started { pid: Some(1) })
            .await;
        // No registered receiver for build 200 yet.
        assert!(!tx_alive_present_before);
        let _rx_alive = reg.register_in_flight_build(alive.clone(), 200);

        reg.remove_if_matches(&dead, dead_conn_id);

        assert!(rx_dead.recv().await.is_none());
        // The alive builder's sender must still be live: forward_build_event
        // succeeds (returns true) because the receiver is registered.
        assert!(
            reg.forward_build_event(&alive, 200, BuildLifecycle::Started { pid: Some(2) })
                .await
        );
    }

    #[test]
    fn stale_builders_reports_only_silent_ones() {
        let reg = BuilderRegistry::new();
        let fresh = BuilderName::new("fresh").unwrap();
        let silent = BuilderName::new("silent").unwrap();
        let _ = reg.register(
            fresh.clone(),
            conn(&reg, 1, caps(&["x86_64-linux"], &[], 1)),
        );
        let mut old = conn(&reg, 2, caps(&["x86_64-linux"], &[], 1));
        // Backdate the silent builder's last heartbeat well past the
        // threshold.
        old.last_heartbeat = Instant::now() - Duration::from_secs(600);
        let _ = reg.register(silent.clone(), old);

        let stale = reg.stale_builders(LIVENESS_MAX_SILENCE);
        assert_eq!(stale, vec![silent.clone()]);

        // A heartbeat refresh clears staleness.
        reg.touch_heartbeat(&silent);
        assert!(reg.stale_builders(LIVENESS_MAX_SILENCE).is_empty());
    }

    #[tokio::test]
    async fn evict_dead_drains_in_flight_and_removes_entry() {
        let reg = BuilderRegistry::new();
        let name = BuilderName::new("zzz").unwrap();
        let _ = reg.register(name.clone(), conn(&reg, 1, caps(&["x86_64-linux"], &[], 4)));
        let mut rx = reg.register_in_flight_build(name.clone(), 7);
        reg.set_phase(&name, 7, BuildPhase::Pull);

        assert!(reg.evict_dead(&name), "evict should report a hit");
        // Worker blocked on the lifecycle receiver wakes via channel close.
        assert!(rx.recv().await.is_none(), "lifecycle channel must close");
        // Entry, phase, and in-flight accounting all gone.
        assert!(reg.snapshot(&name).is_none());
        assert!(reg.phase_snapshot().is_empty());
        // Idempotent: a second evict is a no-op.
        assert!(!reg.evict_dead(&name));
    }

    #[test]
    fn mark_disconnecting_flips_state() {
        let reg = BuilderRegistry::new();
        let name = BuilderName::new("a").unwrap();
        let _ = reg.register(name.clone(), conn(&reg, 1, caps(&["x86_64-linux"], &[], 1)));
        reg.mark_disconnecting(&name);
        assert_eq!(reg.snapshot(&name).unwrap().state, ConnState::Disconnecting);
    }

    #[test]
    fn eligible_filters_by_system() {
        let reg = BuilderRegistry::new();
        let _ = reg.register(
            BuilderName::new("linux").unwrap(),
            conn(&reg, 1, caps(&["x86_64-linux"], &[], 1)),
        );
        let _ = reg.register(
            BuilderName::new("darwin").unwrap(),
            conn(&reg, 2, caps(&["aarch64-darwin"], &[], 1)),
        );
        let lst = reg.eligible("x86_64-linux", &[], &HashSet::new());
        assert_eq!(lst.len(), 1);
        assert_eq!(lst[0].name.as_str(), "linux");
    }

    #[test]
    fn eligible_excludes_disconnecting() {
        let reg = BuilderRegistry::new();
        let name = BuilderName::new("a").unwrap();
        let _ = reg.register(name.clone(), conn(&reg, 1, caps(&["x86_64-linux"], &[], 1)));
        reg.mark_disconnecting(&name);
        let lst = reg.eligible("x86_64-linux", &[], &HashSet::new());
        assert!(lst.is_empty());
    }

    #[test]
    fn eligible_respects_exclude_set() {
        let reg = BuilderRegistry::new();
        let a = conn(&reg, 1, caps(&["x86_64-linux"], &[], 1));
        let a_conn = a.connection_id;
        let _ = reg.register(BuilderName::new("a").unwrap(), a);
        let _ = reg.register(
            BuilderName::new("b").unwrap(),
            conn(&reg, 2, caps(&["x86_64-linux"], &[], 1)),
        );
        let mut excl = HashSet::new();
        excl.insert(a_conn);
        let lst = reg.eligible("x86_64-linux", &[], &excl);
        assert_eq!(lst.len(), 1);
        assert_eq!(lst[0].name.as_str(), "b");
    }

    #[test]
    fn reconnected_builder_is_eligible_despite_prior_connection_excluded() {
        // The transport-failure retry path excludes by connection_id. A
        // builder that drops and reconnects under the same name gets a
        // fresh connection_id, so it must become eligible again even
        // while its prior connection is still in the exclude set — this
        // is what lets a sole builder's brief reconnect be retried
        // instead of leaving the job stuck.
        let reg = BuilderRegistry::new();
        let name = BuilderName::new("solo").unwrap();
        let first = conn(&reg, 1, caps(&["x86_64-linux"], &[], 1));
        let first_conn = first.connection_id;
        let _ = reg.register(name.clone(), first);

        let mut excluded = HashSet::new();
        excluded.insert(first_conn);
        assert!(
            reg.eligible("x86_64-linux", &[], &excluded).is_empty(),
            "the failed connection must be excluded",
        );

        // Same builder reconnects: fresh connection_id displaces the old.
        let second = conn(&reg, 1, caps(&["x86_64-linux"], &[], 1));
        let second_conn = second.connection_id;
        assert_ne!(first_conn, second_conn, "reconnect must mint a new id");
        let _ = reg.register(name.clone(), second);

        let lst = reg.eligible("x86_64-linux", &[], &excluded);
        assert_eq!(lst.len(), 1, "the reconnected builder must be eligible");
        assert_eq!(lst[0].connection_id, second_conn);
    }

    #[test]
    fn eligible_requires_all_features() {
        let reg = BuilderRegistry::new();
        let _ = reg.register(
            BuilderName::new("plain").unwrap(),
            conn(&reg, 1, caps(&["x86_64-linux"], &[], 1)),
        );
        let _ = reg.register(
            BuilderName::new("kvm").unwrap(),
            conn(&reg, 2, caps(&["x86_64-linux"], &["kvm"], 1)),
        );
        let lst = reg.eligible("x86_64-linux", &["kvm".to_string()], &HashSet::new());
        assert_eq!(lst.len(), 1);
        assert_eq!(lst[0].name.as_str(), "kvm");
    }

    #[test]
    fn eligible_skips_at_capacity() {
        let reg = BuilderRegistry::new();
        let name = BuilderName::new("busy").unwrap();
        let _ = reg.register(name.clone(), conn(&reg, 1, caps(&["x86_64-linux"], &[], 1)));
        reg.inc_in_flight(&name);
        let lst = reg.eligible("x86_64-linux", &[], &HashSet::new());
        assert!(
            lst.is_empty(),
            "builder at max_jobs must not appear in eligible()",
        );
    }

    #[test]
    fn any_matching_builder_ignores_capacity() {
        // The pre-flight uses `any_matching_builder` to ask "does a
        // capable builder *exist*", separately from "is one free now".
        // A capable builder at max_jobs must still count as a match —
        // otherwise the pre-flight fails a job fast (e.g. a nixos-test
        // needing kvm) the instant every single-slot builder is busy,
        // even though they all advertise the required features. The job
        // should instead queue on the dispatch loop's capacity wait.
        let reg = BuilderRegistry::new();
        let name = BuilderName::new("kvm").unwrap();
        let _ = reg.register(
            name.clone(),
            conn(&reg, 1, caps(&["x86_64-linux"], &["kvm"], 1)),
        );
        reg.inc_in_flight(&name); // now at capacity

        let req = vec!["kvm".to_string()];
        assert!(
            reg.eligible("x86_64-linux", &req, &HashSet::new())
                .is_empty(),
            "at-capacity builder must not be eligible() (no free slot)",
        );
        assert!(
            reg.any_matching_builder("x86_64-linux", &req, &HashSet::new()),
            "at-capacity builder must still count as a capability match",
        );
    }

    #[test]
    fn any_matching_builder_false_when_feature_absent() {
        let reg = BuilderRegistry::new();
        let _ = reg.register(
            BuilderName::new("plain").unwrap(),
            conn(&reg, 1, caps(&["x86_64-linux"], &[], 1)),
        );
        assert!(
            !reg.any_matching_builder("x86_64-linux", &["kvm".to_string()], &HashSet::new()),
            "no builder advertises kvm — this is the genuine fail-fast case",
        );
    }

    #[test]
    fn eligible_orders_by_least_loaded() {
        let reg = BuilderRegistry::new();
        let big = BuilderName::new("big").unwrap();
        let small = BuilderName::new("small").unwrap();
        let _ = reg.register(big.clone(), conn(&reg, 1, caps(&["x86_64-linux"], &[], 4)));
        let _ = reg.register(
            small.clone(),
            conn(&reg, 2, caps(&["x86_64-linux"], &[], 4)),
        );
        reg.inc_in_flight(&big);
        reg.inc_in_flight(&big);
        let lst = reg.eligible("x86_64-linux", &[], &HashSet::new());
        assert_eq!(lst.len(), 2);
        assert_eq!(lst[0].name.as_str(), "small"); // 0 in flight beats 2
        assert_eq!(lst[1].name.as_str(), "big");
    }

    // ---------- native-tier preference (binfmt emulation) ----------

    #[test]
    fn eligible_prefers_native_over_emulated() {
        // One native aarch64 builder + one x86 builder offering aarch64
        // via binfmt. An aarch64 job must only see the native one.
        let reg = BuilderRegistry::new();
        let native = BuilderName::new("arm").unwrap();
        let emu = BuilderName::new("x86-binfmt").unwrap();
        let _ = reg.register(
            native.clone(),
            conn(&reg, 1, caps(&["aarch64-linux"], &[], 4)),
        );
        let _ = reg.register(
            emu.clone(),
            conn(&reg, 2, caps(&["x86_64-linux", "aarch64-linux"], &[], 4)),
        );
        let lst = reg.eligible("aarch64-linux", &[], &HashSet::new());
        assert_eq!(lst.len(), 1);
        assert_eq!(lst[0].name.as_str(), "arm");
    }

    #[test]
    fn busy_native_holds_back_emulated_for_capacity_wait() {
        // The native builder is saturated. The job must NOT spill to the
        // emulated x86 builder: `eligible` is empty (no native slot) but
        // `any_matching_builder` stays true (native exists) → the worker
        // treats this as a capacity wait, not a no-match interrupt.
        let reg = BuilderRegistry::new();
        let native = BuilderName::new("arm").unwrap();
        let emu = BuilderName::new("x86-binfmt").unwrap();
        let _ = reg.register(
            native.clone(),
            conn(&reg, 1, caps(&["aarch64-linux"], &[], 1)),
        );
        let _ = reg.register(
            emu.clone(),
            conn(&reg, 2, caps(&["x86_64-linux", "aarch64-linux"], &[], 4)),
        );
        reg.inc_in_flight(&native); // now at max_jobs

        assert!(
            reg.eligible("aarch64-linux", &[], &HashSet::new())
                .is_empty(),
            "no native slot ⇒ nothing eligible (must not fall to emulation)",
        );
        assert!(
            reg.any_matching_builder("aarch64-linux", &[], &HashSet::new()),
            "native builder still exists ⇒ wait for a slot, don't interrupt",
        );
    }

    #[test]
    fn emulated_used_when_no_native_present() {
        // No native aarch64 builder connected at all — emulation is the
        // only option, so the x86 binfmt builder becomes eligible. This
        // is also the native-disconnects-mid-wait fallback.
        let reg = BuilderRegistry::new();
        let emu = BuilderName::new("x86-binfmt").unwrap();
        let _ = reg.register(
            emu.clone(),
            conn(&reg, 1, caps(&["x86_64-linux", "aarch64-linux"], &[], 4)),
        );
        let lst = reg.eligible("aarch64-linux", &[], &HashSet::new());
        assert_eq!(lst.len(), 1);
        assert_eq!(lst[0].name.as_str(), "x86-binfmt");
        assert!(reg.any_matching_builder("aarch64-linux", &[], &HashSet::new()));
    }

    #[test]
    fn excluded_native_unblocks_emulated() {
        // If the only native builder is excluded (e.g. transport failure
        // this dispatch), the tier opens up to emulation rather than
        // wedging the job — `exclude` is applied before the native check.
        let reg = BuilderRegistry::new();
        let native = BuilderName::new("arm").unwrap();
        let emu = BuilderName::new("x86-binfmt").unwrap();
        let native_conn = conn(&reg, 1, caps(&["aarch64-linux"], &[], 4));
        let native_conn_id = native_conn.connection_id;
        let _ = reg.register(native.clone(), native_conn);
        let _ = reg.register(
            emu.clone(),
            conn(&reg, 2, caps(&["x86_64-linux", "aarch64-linux"], &[], 4)),
        );
        let mut excluded = HashSet::new();
        excluded.insert(native_conn_id);
        let lst = reg.eligible("aarch64-linux", &[], &excluded);
        assert_eq!(lst.len(), 1);
        assert_eq!(lst[0].name.as_str(), "x86-binfmt");
    }

    #[test]
    fn native_x86_unaffected_by_aarch64_tiering() {
        // x86 jobs still see every x86-native builder; the aarch64 tier
        // logic doesn't perturb the common case.
        let reg = BuilderRegistry::new();
        let a = BuilderName::new("a").unwrap();
        let b = BuilderName::new("b").unwrap();
        let _ = reg.register(
            a.clone(),
            conn(&reg, 1, caps(&["x86_64-linux", "aarch64-linux"], &[], 4)),
        );
        let _ = reg.register(b.clone(), conn(&reg, 2, caps(&["x86_64-linux"], &[], 4)));
        let lst = reg.eligible("x86_64-linux", &[], &HashSet::new());
        assert_eq!(lst.len(), 2);
    }

    // ---------- in-flight build routing ----------

    #[tokio::test]
    async fn forward_build_event_delivers_to_registered_receiver() {
        let reg = BuilderRegistry::new();
        let name = BuilderName::new("a").unwrap();
        let mut rx = reg.register_in_flight_build(name.clone(), 7);

        let delivered = reg
            .forward_build_event(&name, 7, BuildLifecycle::Started { pid: Some(123) })
            .await;
        assert!(delivered);
        let ev = rx.recv().await.expect("event must arrive");
        assert_eq!(ev, BuildLifecycle::Started { pid: Some(123) });
    }

    #[tokio::test]
    async fn forward_build_event_returns_false_for_unknown_build() {
        let reg = BuilderRegistry::new();
        let name = BuilderName::new("a").unwrap();
        // Nothing registered.
        let delivered = reg
            .forward_build_event(
                &name,
                999,
                BuildLifecycle::Finished {
                    status: BuildOutcomeStatus::Success,
                    exit_code: Some(0),
                    output_paths: vec![],
                    log_truncated: false,
                },
            )
            .await;
        assert!(!delivered);
    }

    #[tokio::test]
    async fn try_forward_delivers_and_reports_no_receiver() {
        let reg = BuilderRegistry::new();
        let name = BuilderName::new("a").unwrap();
        let mut rx = reg.register_in_flight_build(name.clone(), 7);

        assert!(matches!(
            reg.try_forward_build_event(&name, 7, BuildLifecycle::Started { pid: Some(1) }),
            TryForward::Delivered
        ));
        assert_eq!(
            rx.recv().await.unwrap(),
            BuildLifecycle::Started { pid: Some(1) }
        );

        // No entry for this build id.
        assert!(matches!(
            reg.try_forward_build_event(&name, 999, BuildLifecycle::Started { pid: None }),
            TryForward::NoReceiver
        ));
    }

    #[tokio::test]
    async fn try_forward_returns_full_and_hands_event_back_when_channel_saturated() {
        let reg = BuilderRegistry::new();
        let name = BuilderName::new("a").unwrap();
        // Hold the receiver but never drain it, so the channel fills.
        let _rx = reg.register_in_flight_build(name.clone(), 1);
        // Fill to capacity (register_in_flight_build uses mpsc::channel(4096)).
        for _ in 0..4096 {
            assert!(matches!(
                reg.try_forward_build_event(&name, 1, BuildLifecycle::LogChunk { bytes: vec![0] }),
                TryForward::Delivered
            ));
        }
        // The next send must report Full and return the event intact so
        // the caller can drop it (log chunk) or re-deliver it (Finished)
        // — never blocking the session read loop.
        match reg.try_forward_build_event(&name, 1, BuildLifecycle::LogChunk { bytes: vec![9] }) {
            TryForward::Full(BuildLifecycle::LogChunk { bytes }) => assert_eq!(bytes, vec![9]),
            other => panic!("expected Full(LogChunk), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unregister_drops_receiver_so_worker_observes_channel_close() {
        let reg = BuilderRegistry::new();
        let name = BuilderName::new("a").unwrap();
        let mut rx = reg.register_in_flight_build(name.clone(), 1);
        reg.unregister_in_flight_build(&name, 1);
        // Sender dropped; the receiver must observe channel close.
        assert!(
            rx.recv().await.is_none(),
            "receiver must close after unregister"
        );
    }

    #[tokio::test]
    async fn registry_keys_per_builder_so_same_build_id_does_not_cross_fire() {
        // build_id should normally be unique daemon-wide (sqlite JobId),
        // but cross-builder isolation is cheap and worth pinning down so
        // a future change to id allocation can't accidentally turn into
        // a routing bug.
        let reg = BuilderRegistry::new();
        let alpha = BuilderName::new("alpha").unwrap();
        let beta = BuilderName::new("beta").unwrap();
        let mut rx_alpha = reg.register_in_flight_build(alpha.clone(), 42);
        let mut rx_beta = reg.register_in_flight_build(beta.clone(), 42);

        reg.forward_build_event(&alpha, 42, BuildLifecycle::Started { pid: Some(1) })
            .await;
        let on_alpha = tokio::time::timeout(std::time::Duration::from_millis(100), rx_alpha.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(on_alpha, BuildLifecycle::Started { pid: Some(1) });

        // Beta's receiver must not have observed alpha's event.
        let on_beta =
            tokio::time::timeout(std::time::Duration::from_millis(50), rx_beta.recv()).await;
        assert!(
            on_beta.is_err(),
            "beta receiver must not have received alpha's event"
        );
    }
}
