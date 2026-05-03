//! Per-evaluation cancellation tokens (Q39 / Q104 / Q105).
//!
//! Layout:
//! - Each in-flight evaluation has a [`CancelToken`]: a cheap-to-clone
//!   handle wrapping an `AtomicBool` plus a tokio `Notify`. Setters
//!   (cancel) and observers (`is_cancelled`, `cancelled` future) are
//!   independent so the worker can both poll between phases AND race
//!   the running subprocess against an async signal in the same loop.
//! - The [`CancelRegistry`] is a process-global map from `EvalId` to
//!   token. The webhook handler looks up the previous in-flight eval's
//!   token to fire the cancellation; the worker registers the token on
//!   eval pickup and removes it on terminal status.
//!
//! Q105: cancellation is *cooperative*. The worker checks the token at
//! safe points (between phases, between jobs) and the build subprocess
//! is interrupted via `select!`, but a build that just succeeded keeps
//! its success — we honour the result we have rather than racing to
//! kill a process that's already exited.

use medusa_domain::EvalId;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// Cheap-to-clone signal that one specific evaluation should stop.
#[derive(Clone, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark cancelled and wake every waiter on `cancelled()`. Idempotent.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Future that resolves when `cancel()` is called. Safe to await
    /// inside a `tokio::select!` to interrupt a long-running operation.
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        // Race the notification against a re-check of the flag, so a
        // cancel that landed between the early check above and adding
        // the listener still wakes us up.
        let notified = self.notify.notified();
        tokio::pin!(notified);
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

#[derive(Default)]
pub struct CancelRegistry {
    inner: Mutex<HashMap<EvalId, CancelToken>>,
}

impl CancelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Worker-side: install a token for `eval_id`. Returns the token
    /// the caller should consult between phases. Idempotent — re-
    /// registering the same eval replaces the old token.
    pub fn register(&self, eval_id: EvalId) -> CancelToken {
        let token = CancelToken::new();
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(eval_id, token.clone());
        token
    }

    /// Worker-side: drop the token for `eval_id`. Call on terminal
    /// status (success, failure, cancelled, eval-failed).
    pub fn deregister(&self, eval_id: EvalId) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&eval_id);
    }

    /// Webhook-side: signal `eval_id`'s in-flight worker (if any) to
    /// stop. No-op if the eval isn't registered (it might be already
    /// finished, or may not have been picked up yet — the row is also
    /// marked Cancelled in the DB so the worker checks-and-bails on
    /// pickup either way).
    pub fn cancel(&self, eval_id: EvalId) {
        if let Some(t) = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&eval_id)
        {
            t.cancel();
        }
    }
}

/// Strip the trailing `:<branch_name>` we add to PR git_refs to make
/// them unique-but-readable. The result is the *branch key* we use to
/// identify "the same logical branch across pushes" — i.e. what we
/// match on for cancel-on-new-push.
///
/// Examples:
///   `refs/heads/main`              → `refs/heads/main`
///   `refs/pull/42/head:feature-x`  → `refs/pull/42/head`
///   `refs/tags/v1.0`               → `refs/tags/v1.0`  (won't match anything; tag pushes are dropped earlier anyway)
pub fn branch_key(git_ref: &str) -> &str {
    git_ref.split_once(':').map_or(git_ref, |(k, _)| k)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn fresh_token_not_cancelled() {
        let t = CancelToken::new();
        assert!(!t.is_cancelled());
    }

    #[test]
    fn cancel_flips_flag() {
        let t = CancelToken::new();
        t.cancel();
        assert!(t.is_cancelled());
    }

    #[test]
    fn cancel_is_idempotent() {
        let t = CancelToken::new();
        t.cancel();
        t.cancel();
        assert!(t.is_cancelled());
    }

    #[tokio::test]
    async fn cancelled_future_resolves_after_cancel() {
        let t = CancelToken::new();
        let t2 = t.clone();
        let h = tokio::spawn(async move { t2.cancelled().await });
        // Give the spawn a moment to register its waiter.
        tokio::time::sleep(Duration::from_millis(10)).await;
        t.cancel();
        h.await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_future_resolves_immediately_if_already_cancelled() {
        let t = CancelToken::new();
        t.cancel();
        // Should return immediately without polling Notify.
        tokio::time::timeout(Duration::from_millis(50), t.cancelled())
            .await
            .expect("cancelled() should resolve immediately when already cancelled");
    }

    #[test]
    fn registry_round_trip() {
        let r = CancelRegistry::new();
        let id = EvalId::new(7);
        let token = r.register(id);
        assert!(!token.is_cancelled());
        r.cancel(id);
        assert!(token.is_cancelled());
        r.deregister(id);
        // After deregister, cancel is a no-op (no token under that id).
        let id2 = EvalId::new(8);
        r.cancel(id2); // shouldn't panic
    }

    #[test]
    fn registry_cancel_only_targets_specified_eval() {
        let r = CancelRegistry::new();
        let a = r.register(EvalId::new(1));
        let b = r.register(EvalId::new(2));
        r.cancel(EvalId::new(1));
        assert!(a.is_cancelled());
        assert!(!b.is_cancelled());
    }

    #[test]
    fn branch_key_strips_pr_branch_suffix() {
        assert_eq!(branch_key("refs/heads/main"), "refs/heads/main");
        assert_eq!(branch_key("refs/pull/42/head:feature-x"), "refs/pull/42/head");
        assert_eq!(branch_key("refs/tags/v1.0"), "refs/tags/v1.0");
        assert_eq!(branch_key(""), "");
    }
}
