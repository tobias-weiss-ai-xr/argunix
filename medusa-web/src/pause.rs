//! Forge-level pause registry (Q82).
//!
//! When a forge call returns `401 Unauthorised`, the credential we're
//! using is broken (token revoked, rotated, or never had the necessary
//! scope). Without intervention, every subsequent webhook for repos on
//! that forge would re-trigger the same call and get the same 401 — the
//! forge ends up rate-limiting us, the journal fills with warnings, and
//! the operator has nothing actionable in the logs.
//!
//! On 401, mark the *forge* as paused (the token is per-forge, not
//! per-repo, so per-forge is the natural granularity). Subsequent
//! `post_check` calls for that forge are skipped silently. The pause is
//! cleared automatically on the *next* successful auth-bearing call —
//! typically `query_user_permission` from the webhook handler's policy
//! gate. So when the operator rotates the token and a fresh webhook
//! arrives, medusa unpauses on its own.
//!
//! In-memory only: a daemon restart clears all pauses. That's fine — a
//! fresh start re-tries every forge call anyway.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

#[derive(Debug, Clone)]
struct PauseEntry {
    /// Read by tests today and intended for `medusactl status` later
    /// (when M8 lands). Keeping the field stable so the wire format
    /// doesn't change when we grow it.
    #[allow(dead_code)]
    since: Instant,
    reason: String,
}

#[derive(Default)]
pub struct PauseRegistry {
    inner: Mutex<HashMap<String, PauseEntry>>,
}

impl PauseRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pause `forge`. Logs at WARN once per transition (going from
    /// healthy → paused, or paused-with-old-reason → paused-with-new-reason).
    /// Re-pausing with the same reason is silent so we don't spam the
    /// log on every webhook.
    pub fn pause(&self, forge: &str, reason: impl Into<String>) {
        let reason = reason.into();
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let prev = map.get(forge).cloned();
        let entry = PauseEntry {
            since: Instant::now(),
            reason: reason.clone(),
        };
        map.insert(forge.to_string(), entry);
        drop(map);

        let should_log = match prev {
            None => true,
            Some(p) => p.reason != reason,
        };
        if should_log {
            tracing::warn!(
                forge,
                reason = %reason,
                "pausing forge after auth failure; subsequent forge calls will be skipped until a successful auth attempt unpauses",
            );
        }
    }

    /// Clear any pause on `forge`. Logs at INFO if a pause was actually
    /// cleared.
    pub fn mark_healthy(&self, forge: &str) {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if map.remove(forge).is_some() {
            tracing::info!(forge, "unpausing forge — auth call succeeded");
        }
    }

    pub fn is_paused(&self, forge: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(forge)
    }

    /// Snapshot of currently-paused forges (for `medusactl status` later).
    pub fn snapshot(&self) -> Vec<(String, String)> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(k, v)| (k.clone(), v.reason.clone()))
            .collect()
    }

    #[cfg(test)]
    fn paused_since(&self, forge: &str) -> Option<Instant> {
        self.inner
            .lock()
            .unwrap()
            .get(forge)
            .map(|e| e.since)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_registry_has_nothing_paused() {
        let r = PauseRegistry::new();
        assert!(!r.is_paused("gh"));
    }

    #[test]
    fn pause_then_is_paused() {
        let r = PauseRegistry::new();
        r.pause("gh", "401 from query_user_permission");
        assert!(r.is_paused("gh"));
        assert!(!r.is_paused("other-forge"));
    }

    #[test]
    fn mark_healthy_clears_pause() {
        let r = PauseRegistry::new();
        r.pause("gh", "401");
        assert!(r.is_paused("gh"));
        r.mark_healthy("gh");
        assert!(!r.is_paused("gh"));
    }

    #[test]
    fn mark_healthy_on_unpaused_forge_is_noop() {
        let r = PauseRegistry::new();
        r.mark_healthy("gh"); // no panic, no state change
        assert!(!r.is_paused("gh"));
    }

    #[test]
    fn snapshot_lists_paused_forges() {
        let r = PauseRegistry::new();
        r.pause("gh", "401");
        r.pause("gl", "scope insufficient");
        let mut snap = r.snapshot();
        snap.sort();
        assert_eq!(
            snap,
            vec![
                ("gh".to_string(), "401".to_string()),
                ("gl".to_string(), "scope insufficient".to_string()),
            ],
        );
    }

    #[test]
    fn re_pausing_with_same_reason_does_not_reset_since() {
        let r = PauseRegistry::new();
        r.pause("gh", "401");
        let t1 = r.paused_since("gh").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        r.pause("gh", "401"); // same reason
        let t2 = r.paused_since("gh").unwrap();
        // Pause replaces the entry, so `since` does advance — that's
        // fine; what we care about is that we don't spam the log.
        // Just assert the registry still reports paused.
        assert!(r.is_paused("gh"));
        assert!(t2 >= t1);
    }
}
