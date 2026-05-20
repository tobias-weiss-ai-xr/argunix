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
use tokio::sync::mpsc;

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

/// What a takeover surfaces about the connection it just displaced. The
/// caller — running on a tokio task — uses these to send a `kick`
/// message and disconnect the old SSH session.
pub struct DisplacedConnection {
    pub name: BuilderName,
    pub session: Option<RusshSession>,
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
        })
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
            })
            .collect();
        out.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        out
    }

    /// Builders that match `(system, features)`, are `Active`, aren't at
    /// `max_jobs`, and aren't in `exclude`. Sorted by `in_flight` ascending
    /// so callers can pick the least-loaded eligible builder.
    pub fn eligible(
        &self,
        system: &str,
        features: &[String],
        exclude: &HashSet<BuilderName>,
    ) -> Vec<BuilderSnapshot> {
        let mut found: Vec<BuilderSnapshot> = self
            .list()
            .into_iter()
            .filter(|b| b.state == ConnState::Active)
            .filter(|b| !exclude.contains(&b.name))
            .filter(|b| b.capabilities.systems.iter().any(|s| s == system))
            .filter(|b| features.iter().all(|f| b.capabilities.features.contains(f)))
            .filter(|b| b.in_flight < b.capabilities.max_jobs)
            .collect();
        found.sort_by_key(|b| b.in_flight);
        found
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

#[cfg(test)]
mod tests {
    use super::*;
    use argunix_domain::BuilderId;

    fn caps(systems: &[&str], features: &[&str], max_jobs: u32) -> BuilderCapabilities {
        BuilderCapabilities {
            systems: systems.iter().map(|s| s.to_string()).collect(),
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
        let _ = reg.register(
            BuilderName::new("a").unwrap(),
            conn(&reg, 1, caps(&["x86_64-linux"], &[], 1)),
        );
        let _ = reg.register(
            BuilderName::new("b").unwrap(),
            conn(&reg, 2, caps(&["x86_64-linux"], &[], 1)),
        );
        let mut excl = HashSet::new();
        excl.insert(BuilderName::new("a").unwrap());
        let lst = reg.eligible("x86_64-linux", &[], &excl);
        assert_eq!(lst.len(), 1);
        assert_eq!(lst[0].name.as_str(), "b");
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
