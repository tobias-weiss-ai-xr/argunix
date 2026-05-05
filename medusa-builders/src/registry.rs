//! In-memory map of currently-connected builders (M13 / `design/builders.md`).
//!
//! Backs both the dispatcher (PR #7 — "give me a builder that can build
//! `<system>` and isn't at `max_jobs`") and `medusactl builders`. Lifetime
//! is bound to the underlying SSH connection: insertion happens when the
//! agent's `hello` is accepted, removal when the connection drops.
//!
//! Persistent state — pubkey, capabilities snapshot — lives in the
//! `builders` sqlite table (see `medusa-store::BuilderStore`). The
//! registry is the *runtime* view, not a cache of sqlite.

use chrono::{DateTime, Utc};
use medusa_domain::{BuilderCapabilities, BuilderId, BuilderName};
use russh::ChannelId;
use russh::server::Handle as SessionHandle;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

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

/// A snapshot of one builder's runtime state — what `medusactl builders`
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

#[derive(Default)]
pub struct BuilderRegistry {
    inner: Mutex<HashMap<BuilderName, ConnectedBuilder>>,
    /// Per-builder running count of dispatched build channels. PR #7
    /// will increment/decrement this; PR #6 just exposes it.
    in_flight: Mutex<HashMap<BuilderName, u32>>,
    /// Monotonic counter for `connection_id`. Wraps after 2^64 connects,
    /// which is fine.
    next_conn_id: AtomicU64,
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
        prior.map(|p| DisplacedConnection {
            name,
            session: p.session,
        })
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
    /// worker (M14) once per dispatched derivation: increment before
    /// the build starts, decrement when it returns. This is the
    /// authoritative "how busy is this builder" gauge that drives both
    /// `eligible()`'s capacity check and the status page's per-builder
    /// counter.
    ///
    /// The channel layer (`BuilderDispatcher`, `socket_server`) does
    /// **not** call these. nix's ssh-ng store opens several channels
    /// per realise call (substitute probes, path queries, builds);
    /// counting channels conflates connection-pool size with build
    /// load and over-reports.
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use medusa_domain::BuilderId;

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
}
