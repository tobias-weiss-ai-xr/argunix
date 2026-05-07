//! Webhook coalescing (Q99).
//!
//! GitHub (and every other forge we've looked at) fires multiple webhook
//! events for the same `(repo, sha)` within milliseconds — a `push` plus
//! a `pull_request.synchronize`, or two `push` events on a force-push
//! that lands at the same SHA. Without coalescing, argunix would queue
//! one evaluation per event and the GitHub commit page ends up with two
//! sets of identical checks racing each other.
//!
//! The pool admits the *first* event for each `(repo_id, sha)` and
//! drops every subsequent one within `window`. Cleanup is opportunistic
//! — every `admit()` call sweeps expired entries. The map is small
//! (one entry per active SHA per repo) so this is cheap.
//!
//! In-memory only: a daemon restart clears the table. Duplicates that
//! cross a restart will eval twice. We've decided that's acceptable in
//! v1 — it's the rare case and the cost is one extra evaluation, not a
//! correctness issue.

use argunix_domain::{RepoId, Sha};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct CoalescePool {
    inner: Mutex<HashMap<(RepoId, Sha), Instant>>,
    window: Duration,
}

impl CoalescePool {
    pub fn new(window: Duration) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            window,
        }
    }

    /// Try to admit `(repo_id, sha)` as a fresh event. Returns `true` if
    /// the caller should proceed (event is new or its window has
    /// expired); `false` if it's a duplicate within the window.
    pub fn admit(&self, repo_id: RepoId, sha: Sha) -> bool {
        self.admit_at(repo_id, sha, Instant::now())
    }

    fn admit_at(&self, repo_id: RepoId, sha: Sha, now: Instant) -> bool {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // Opportunistic cleanup: any entry whose deadline is in the past
        // can go. Cheap because the map is small and called on every
        // webhook (the slow path).
        map.retain(|_, deadline| *deadline > now);
        let key = (repo_id, sha);
        if map.contains_key(&key) {
            return false;
        }
        map.insert(key, now + self.window);
        true
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha_for(suffix: char) -> Sha {
        // 40-hex-char SHA: 39 zeros + the suffix nibble.
        let mut s = String::with_capacity(40);
        s.extend(std::iter::repeat('0').take(39));
        s.push(suffix);
        Sha::new(s).unwrap()
    }

    #[test]
    fn first_event_admitted_duplicate_dropped() {
        let pool = CoalescePool::new(Duration::from_secs(5));
        let t0 = Instant::now();
        let r = RepoId::new(1);
        let s = sha_for('a');
        assert!(pool.admit_at(r, s.clone(), t0));
        assert!(!pool.admit_at(r, s.clone(), t0 + Duration::from_secs(1)));
        // Still within window — still dropped.
        assert!(!pool.admit_at(r, s, t0 + Duration::from_secs(4)));
    }

    #[test]
    fn admits_again_after_window() {
        let pool = CoalescePool::new(Duration::from_secs(5));
        let t0 = Instant::now();
        let r = RepoId::new(1);
        let s = sha_for('a');
        assert!(pool.admit_at(r, s.clone(), t0));
        // 5s + 1ms after first admit: deadline (t0+5s) is now in the past,
        // so the next admit should succeed and start a new window.
        assert!(pool.admit_at(r, s, t0 + Duration::from_secs(5) + Duration::from_millis(1)));
    }

    #[test]
    fn different_keys_dont_interfere() {
        let pool = CoalescePool::new(Duration::from_secs(5));
        let t0 = Instant::now();
        assert!(pool.admit_at(RepoId::new(1), sha_for('a'), t0));
        assert!(pool.admit_at(RepoId::new(1), sha_for('b'), t0)); // same repo, different sha
        assert!(pool.admit_at(RepoId::new(2), sha_for('a'), t0)); // same sha, different repo
    }

    #[test]
    fn opportunistic_cleanup_shrinks_map() {
        let pool = CoalescePool::new(Duration::from_secs(5));
        let t0 = Instant::now();
        for c in ['a', 'b', 'c', 'd'] {
            assert!(pool.admit_at(RepoId::new(1), sha_for(c), t0));
        }
        assert_eq!(pool.len(), 4);
        // Past every deadline; the next admit should clean them out.
        let later = t0 + Duration::from_secs(10);
        assert!(pool.admit_at(RepoId::new(1), sha_for('e'), later));
        assert_eq!(pool.len(), 1);
    }
}
