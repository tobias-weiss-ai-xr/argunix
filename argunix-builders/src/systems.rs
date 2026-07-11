//! Resolved list of `<system>` tuples available for evaluation.
//!
//! argunix-eval needs to know which `<system>` attributes to walk in
//! flake/non-flake mode. The set is the *union* of:
//!
//!   1. the argunix host's own system (always present, even when no
//!      remote builders are connected),
//!   2. every Active builder's advertised systems.
//!
//! Disconnecting / disconnected builders are excluded — there's no
//! point evaluating attributes whose only candidate builder has just
//! signalled it's leaving (the dispatch would race the disconnect).

use crate::registry::{BuilderRegistry, ConnState};
use std::collections::BTreeSet;
use std::sync::Arc;

/// Reads the registry at call time; deduplicates against `local`.
///
/// Held by the worker context so each evaluation observes a fresh
/// snapshot. Cheap to clone (just an Arc bump).
#[derive(Clone)]
pub struct SystemsResolver {
    local: Vec<String>,
    registry: Arc<BuilderRegistry>,
}

impl SystemsResolver {
    pub fn new(local: Vec<String>, registry: Arc<BuilderRegistry>) -> Self {
        Self { local, registry }
    }

    /// Returns local ∪ ⋃ {Active builder.systems}, sorted deterministically
    /// so log lines and tests aren't order-flaky.
    pub fn current(&self) -> Vec<String> {
        let mut out: BTreeSet<String> = self.local.iter().cloned().collect();
        for b in self.registry.list() {
            if b.state == ConnState::Active {
                out.extend(b.capabilities.systems.iter().cloned());
            }
        }
        out.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ConnectedBuilder;
    use argunix_domain::{BuilderCapabilities, BuilderId, BuilderName};
    use chrono::Utc;

    fn caps(systems: &[&str]) -> BuilderCapabilities {
        BuilderCapabilities {
            systems: systems.iter().map(|s| s.to_string()).collect(),
            native_system: systems.first().map(|s| s.to_string()).unwrap_or_default(),
            features: vec![],
            max_jobs: 1,
            nix_version: "test".into(),
        }
    }

    fn conn(reg: &BuilderRegistry, id: i64, systems: &[&str]) -> ConnectedBuilder {
        ConnectedBuilder {
            builder_id: BuilderId::new(id),
            capabilities: caps(systems),
            state: ConnState::Active,
            connected_since: Utc::now(),
            connection_id: reg.next_connection_id(),
            session: None,
            last_activity: std::time::Instant::now(),
            abort: None,
        }
    }

    #[test]
    fn empty_registry_returns_local_only() {
        let reg = BuilderRegistry::new();
        let r = SystemsResolver::new(vec!["x86_64-linux".into()], reg);
        assert_eq!(r.current(), vec!["x86_64-linux".to_string()]);
    }

    #[test]
    fn unions_active_builders_systems_with_local() {
        let reg = BuilderRegistry::new();
        let _ = reg.register(
            BuilderName::new("mac").unwrap(),
            conn(&reg, 1, &["aarch64-darwin"]),
        );
        let _ = reg.register(
            BuilderName::new("arm").unwrap(),
            conn(&reg, 2, &["aarch64-linux"]),
        );
        let r = SystemsResolver::new(vec!["x86_64-linux".into()], reg);
        let got = r.current();
        // Sorted: aarch64-darwin, aarch64-linux, x86_64-linux.
        assert_eq!(
            got,
            vec![
                "aarch64-darwin".to_string(),
                "aarch64-linux".to_string(),
                "x86_64-linux".to_string(),
            ]
        );
    }

    #[test]
    fn deduplicates_overlapping_systems() {
        let reg = BuilderRegistry::new();
        let _ = reg.register(
            BuilderName::new("a").unwrap(),
            conn(&reg, 1, &["x86_64-linux", "i686-linux"]),
        );
        let _ = reg.register(
            BuilderName::new("b").unwrap(),
            conn(&reg, 2, &["x86_64-linux"]),
        );
        let r = SystemsResolver::new(vec!["x86_64-linux".into()], reg);
        let got = r.current();
        assert_eq!(
            got,
            vec!["i686-linux".to_string(), "x86_64-linux".to_string()]
        );
    }

    #[test]
    fn disconnecting_builders_excluded() {
        let reg = BuilderRegistry::new();
        let mac = BuilderName::new("mac").unwrap();
        let _ = reg.register(mac.clone(), conn(&reg, 1, &["aarch64-darwin"]));
        reg.mark_disconnecting(&mac);
        let r = SystemsResolver::new(vec!["x86_64-linux".into()], reg);
        // mac is Disconnecting → its system isn't included.
        assert_eq!(r.current(), vec!["x86_64-linux".to_string()]);
    }

    #[test]
    fn current_is_a_fresh_snapshot_each_call() {
        let reg = BuilderRegistry::new();
        let r = SystemsResolver::new(vec!["x86_64-linux".into()], reg.clone());
        assert_eq!(r.current().len(), 1);
        let _ = reg.register(
            BuilderName::new("mac").unwrap(),
            conn(&reg, 1, &["aarch64-darwin"]),
        );
        // Same resolver, registry mutated underneath; new call sees it.
        assert_eq!(r.current().len(), 2);
    }
}
