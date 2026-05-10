//! Drive a [`ScheduleStrategy`] until it has nothing left to dispatch.
//!
//! [`drive`] is the generic dispatch loop the worker will eventually
//! call instead of its hand-rolled `JoinSet` selection at
//! `worker.rs:394`. It's separated out here so it can be unit-tested
//! against a stub spawner without booting the whole eval pipeline.
//!
//! ## Loop shape
//!
//! ```text
//! loop {
//!     while let Some(d) = strategy.dispatch() {
//!         spawn(d) → future of (token, status) joins the set
//!     }
//!     if no in-flight and no pending: done
//!     await one of:
//!         - join_next from the set       → strategy.complete(token, status)
//!         - cancel.cancelled()           → drain (no abort)
//! }
//! ```
//!
//! Cancel semantics mirror `worker.rs:444-457`: we stop spawning new
//! work but **don't** call `JoinSet::abort_all` — in-flight tasks are
//! expected to observe the cancel token themselves and tear down their
//! remote builds gracefully. Aborting would drop the futures before
//! they could send `Abort` over the side-channel, leaving zombie
//! builds running on the builders.
//!
//! ## Cascade-skip side effects
//!
//! When the strategy reports `cascaded_skips` on a `complete` call, we
//! invoke the `on_skip` callback once per skipped Job. The caller
//! handles DB row + forge check updates; the driver doesn't reach into
//! either.

use argunix_domain::JobStatus;
use argunix_sched::{CascadedSkip, DispatchToken, Dispatched, ScheduleStrategy};
use argunix_web::CancelToken;
use std::future::Future;
use tokio::task::JoinSet;

/// Run the dispatch loop until `strategy` is fully drained or `cancel`
/// fires.
///
/// `spawn` takes one [`Dispatched`] and returns a future that resolves
/// to its terminal [`JobStatus`]. The driver injects the `(token,
/// status)` pair back into the strategy via `complete` when the future
/// resolves.
///
/// `on_skip` is called once per [`CascadedSkip`] surfaced by
/// `strategy.complete`. Callers typically use it to mark the affected
/// Job rows as `Failure` and post a synthetic forge check.
///
/// On cancel, the driver stops dispatching new work but waits for
/// every in-flight build to resolve before returning, so caller-side
/// state stays consistent.
// Not yet wired into worker.rs's dispatch loop — that wiring is the
// next milestone. The driver's tests below exercise it via the trait
// directly so the integration point doesn't have to wait.
#[allow(dead_code)]
pub async fn drive<F, Fut>(
    strategy: &mut dyn ScheduleStrategy,
    spawn: F,
    cancel: &CancelToken,
    mut on_skip: impl FnMut(CascadedSkip),
) where
    F: Fn(Dispatched) -> Fut,
    Fut: Future<Output = JobStatus> + Send + 'static,
{
    // The set holds (DispatchToken, JobStatus) results. We carry the
    // token through the future so the join_next branch knows which
    // Step finished without us bookkeeping a parallel map.
    let mut set: JoinSet<(DispatchToken, JobStatus)> = JoinSet::new();

    loop {
        // Spawn while strategy has work and we aren't cancelling.
        if !cancel.is_cancelled() {
            while let Some(d) = strategy.dispatch() {
                let token = d.token;
                let fut = spawn(d);
                set.spawn(async move {
                    let status = fut.await;
                    (token, status)
                });
            }
        }

        // Termination: in-flight set is empty AND either the strategy
        // has nothing pending OR we're cancelled (no new work will be
        // dispatched, so anything still pending stays pending forever
        // — that's caller-side state for them to GC).
        if set.is_empty() && (strategy.pending_count() == 0 || cancel.is_cancelled()) {
            break;
        }

        // Wait for the next in-flight build to finish. Once cancel has
        // fired we drop the cancel arm of the select — `cancelled()`
        // resolves immediately while the flag is set, which would
        // turn this select into a busy-loop and starve `join_next` of
        // its turn to wake.
        let outcome = if cancel.is_cancelled() {
            set.join_next().await
        } else {
            tokio::select! {
                biased;
                joined = set.join_next() => joined,
                _ = cancel.cancelled() => {
                    // Re-enter the loop: the cancel-aware spawn block at
                    // the top will skip starting new work, and we'll
                    // fall through to the `is_cancelled()` branch above
                    // to drain in-flight without re-entering this select.
                    continue;
                }
            }
        };

        let Some(result) = outcome else {
            // join_next returned None → set is empty. Either we're
            // truly done or strategy still has pending work. Loop
            // again so the dispatch block at the top picks it up.
            continue;
        };

        let (token, status) = match result {
            Ok(pair) => pair,
            Err(join_err) => {
                // A spawn task panicked or was aborted. We can't recover
                // its terminal status; surface as Failure to the strategy
                // so dependents cascade-skip rather than hang. The
                // panicked token is unrecoverable from the JoinError; we
                // synthesize a Failure here, but cleaning up the strategy
                // entry depends on the strategy seeing a `complete` for
                // that token. Without it, the strategy's pending_count
                // never decrements. Since we can't extract the token from
                // the JoinError, log loudly and skip — this is a
                // programmer-error path (build futures shouldn't panic).
                tracing::error!(
                    error = %join_err,
                    "dispatched build task panicked; strategy may stall",
                );
                continue;
            }
        };

        let effects = strategy.complete(token, status);
        for skip in effects.cascaded_skips {
            on_skip(skip);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use argunix_domain::{DerivationInfo, EvalId, JobId, RepoId};
    use argunix_sched::{DagStrategy, FlatStrategy, ScheduleItem};
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Mutex;

    fn jid(n: i64) -> JobId {
        JobId::new(n)
    }
    fn rid(n: i64) -> RepoId {
        RepoId::new(n)
    }

    fn drv(path: &str, inputs: &[&str]) -> DerivationInfo {
        DerivationInfo {
            drv_path: path.into(),
            system: Some("x86_64-linux".into()),
            required_features: Vec::new(),
            input_drvs: inputs.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Spawner that returns canned statuses keyed by drv path. Records
    /// the order of dispatch in `seen` so tests can assert on
    /// dependency ordering. `delay_ms` lets us simulate concurrent
    /// builds finishing out-of-order.
    #[derive(Clone, Default)]
    struct StubSpawner {
        statuses: Arc<Mutex<HashMap<String, JobStatus>>>,
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl StubSpawner {
        fn new(statuses: HashMap<&str, JobStatus>) -> Self {
            Self {
                statuses: Arc::new(Mutex::new(
                    statuses
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v))
                        .collect(),
                )),
                seen: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn dispatch_order(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }

        fn spawn_fn(
            self,
        ) -> impl Fn(Dispatched) -> std::pin::Pin<Box<dyn Future<Output = JobStatus> + Send>>
        {
            move |d: Dispatched| {
                let statuses = self.statuses.clone();
                let seen = self.seen.clone();
                Box::pin(async move {
                    seen.lock().unwrap().push(d.drv_path.clone());
                    statuses
                        .lock()
                        .unwrap()
                        .get(&d.drv_path)
                        .copied()
                        .unwrap_or(JobStatus::Success)
                })
            }
        }
    }

    fn item_with_closure(
        repo: RepoId,
        eval: EvalId,
        job: JobId,
        head: DerivationInfo,
        closure: Vec<DerivationInfo>,
    ) -> ScheduleItem {
        ScheduleItem {
            repo_id: repo,
            eval_id: eval,
            job_id: job,
            head_drv: head,
            closure,
        }
    }

    #[tokio::test]
    async fn flat_strategy_drains_through_driver() {
        let mut s = FlatStrategy::new(None);
        for n in 0..5 {
            s.enqueue(item_with_closure(
                rid(1),
                EvalId::new(0),
                jid(n),
                drv(&format!("/nix/store/{n:02}-x.drv"), &[]),
                Vec::new(),
            ));
        }

        let spawner = StubSpawner::new(HashMap::new());
        let cancel = CancelToken::new();
        let mut skips: Vec<CascadedSkip> = Vec::new();
        let spawn_fn = spawner.clone().spawn_fn();
        drive(&mut s, spawn_fn, &cancel, |s| skips.push(s)).await;

        assert_eq!(spawner.dispatch_order().len(), 5);
        assert_eq!(s.pending_count(), 0);
        assert_eq!(s.in_flight_count(), 0);
        assert!(skips.is_empty());
    }

    #[tokio::test]
    async fn dag_linear_chain_dispatches_in_dependency_order() {
        // A → B → C. Strategy gates dispatch; the driver should call
        // spawn() in order A, B, C even though all three are enqueued
        // up front.
        let mut s = DagStrategy::new(None);
        let a_drv = drv("/nix/store/aaaa-a.drv", &[]);
        let b_drv = drv("/nix/store/bbbb-b.drv", &["/nix/store/aaaa-a.drv"]);
        let c_drv = drv("/nix/store/cccc-c.drv", &["/nix/store/bbbb-b.drv"]);
        s.enqueue(item_with_closure(
            rid(1),
            EvalId::new(1),
            jid(1),
            a_drv.clone(),
            vec![],
        ));
        s.enqueue(item_with_closure(
            rid(1),
            EvalId::new(1),
            jid(2),
            b_drv.clone(),
            vec![a_drv.clone()],
        ));
        s.enqueue(item_with_closure(
            rid(1),
            EvalId::new(1),
            jid(3),
            c_drv,
            vec![a_drv, b_drv],
        ));

        let spawner = StubSpawner::new(HashMap::new());
        let cancel = CancelToken::new();
        let spawn_fn = spawner.clone().spawn_fn();
        drive(&mut s, spawn_fn, &cancel, |_| {}).await;

        let order = spawner.dispatch_order();
        assert_eq!(order.len(), 3);
        assert_eq!(order[0], "/nix/store/aaaa-a.drv");
        assert_eq!(order[1], "/nix/store/bbbb-b.drv");
        assert_eq!(order[2], "/nix/store/cccc-c.drv");
    }

    #[tokio::test]
    async fn cascade_skip_callback_fires_on_dep_failure() {
        // A → B, A → C. A fails. Driver should fire on_skip twice
        // (once for B, once for C) and never call spawn for them.
        let mut s = DagStrategy::new(None);
        let a = drv("/nix/store/aaaa-a.drv", &[]);
        let b = drv("/nix/store/bbbb-b.drv", &["/nix/store/aaaa-a.drv"]);
        let c = drv("/nix/store/cccc-c.drv", &["/nix/store/aaaa-a.drv"]);
        s.enqueue(item_with_closure(
            rid(1),
            EvalId::new(7),
            jid(1),
            a.clone(),
            vec![],
        ));
        s.enqueue(item_with_closure(
            rid(1),
            EvalId::new(7),
            jid(2),
            b,
            vec![a.clone()],
        ));
        s.enqueue(item_with_closure(
            rid(1),
            EvalId::new(7),
            jid(3),
            c,
            vec![a],
        ));

        let mut statuses = HashMap::new();
        statuses.insert("/nix/store/aaaa-a.drv", JobStatus::Failure);
        let spawner = StubSpawner::new(statuses);
        let cancel = CancelToken::new();
        let mut skipped: Vec<CascadedSkip> = Vec::new();
        let spawn_fn = spawner.clone().spawn_fn();
        drive(&mut s, spawn_fn, &cancel, |sk| skipped.push(sk)).await;

        // A was the only thing dispatched.
        assert_eq!(spawner.dispatch_order(), vec!["/nix/store/aaaa-a.drv"]);
        // B and C surface as cascade-skips.
        let mut skipped_jobs: Vec<JobId> = skipped.iter().map(|s| s.job_id).collect();
        skipped_jobs.sort_by_key(|j| j.get());
        assert_eq!(skipped_jobs, vec![jid(2), jid(3)]);
        for sk in &skipped {
            assert_eq!(sk.reason_drv, "/nix/store/aaaa-a.drv");
            assert_eq!(sk.eval_id, EvalId::new(7));
        }
    }

    #[tokio::test]
    async fn shared_internal_step_dispatched_once_via_driver() {
        // The motivating case end-to-end: top-level X and Y both
        // depend on internal Z. Driver must call spawn for Z exactly
        // once (head_job = None), then for X and Y after Z succeeds.
        let mut s = DagStrategy::new(None);
        let z = drv("/nix/store/zzzz-z.drv", &[]);
        let x = drv("/nix/store/xxxx-x.drv", &["/nix/store/zzzz-z.drv"]);
        let y = drv("/nix/store/yyyy-y.drv", &["/nix/store/zzzz-z.drv"]);
        s.enqueue(item_with_closure(
            rid(1),
            EvalId::new(1),
            jid(1),
            x,
            vec![z.clone()],
        ));
        s.enqueue(item_with_closure(
            rid(1),
            EvalId::new(1),
            jid(2),
            y,
            vec![z],
        ));

        let spawner = StubSpawner::new(HashMap::new());
        let cancel = CancelToken::new();
        let spawn_fn = spawner.clone().spawn_fn();
        drive(&mut s, spawn_fn, &cancel, |_| {}).await;

        let order = spawner.dispatch_order();
        // Z dispatches first; X and Y follow in some order.
        assert_eq!(order[0], "/nix/store/zzzz-z.drv");
        assert_eq!(order.len(), 3, "Z, X, Y exactly — Z is not duplicated");
        assert!(order[1..].contains(&"/nix/store/xxxx-x.drv".to_string()));
        assert!(order[1..].contains(&"/nix/store/yyyy-y.drv".to_string()));
    }

    #[tokio::test]
    async fn cancel_drains_in_flight_without_aborting() {
        // Concurrency cap of 1 so the strategy hands out one job at a
        // time. The first build's spawn fires cancel; the driver must
        // then refuse to dispatch any of the remaining 9 jobs and
        // return as soon as that first task resolves. Without the cap,
        // FlatStrategy(None) dispatches all 10 in one synchronous burst
        // before any task can run — cancel-blocks-new-dispatch is then
        // unobservable.
        let mut s = FlatStrategy::new(Some(1));
        for n in 0..10 {
            s.enqueue(item_with_closure(
                rid(1),
                EvalId::new(0),
                jid(n),
                drv(&format!("/nix/store/{n:02}-x.drv"), &[]),
                Vec::new(),
            ));
        }

        let cancel = CancelToken::new();
        let cancel_clone = cancel.clone();
        let spawner = StubSpawner::new(HashMap::new());
        let seen = spawner.seen.clone();

        // Spawn with a hook that cancels right after the first dispatch
        // is observed, then yields long enough for the driver to see the
        // cancel before spawning more work.
        let spawn_fn = {
            let seen_inner = seen.clone();
            let statuses = spawner.statuses.clone();
            move |d: Dispatched| -> std::pin::Pin<Box<dyn Future<Output = JobStatus> + Send>> {
                let cancel = cancel_clone.clone();
                let seen = seen_inner.clone();
                let statuses = statuses.clone();
                Box::pin(async move {
                    let first = {
                        let mut g = seen.lock().unwrap();
                        let first = g.is_empty();
                        g.push(d.drv_path.clone());
                        first
                    };
                    if first {
                        cancel.cancel();
                        // Yield a few times so the driver definitely
                        // observes the cancel before this build resolves.
                        for _ in 0..5 {
                            tokio::task::yield_now().await;
                        }
                    }
                    statuses
                        .lock()
                        .unwrap()
                        .get(&d.drv_path)
                        .copied()
                        .unwrap_or(JobStatus::Success)
                })
            }
        };

        drive(&mut s, spawn_fn, &cancel, |_| {}).await;

        let order = spawner.dispatch_order();
        // Exactly one build should have happened — the one that fired
        // the cancel. The rest must remain pending.
        assert_eq!(
            order.len(),
            1,
            "expected exactly one dispatch before cancel; got {order:?}"
        );
        assert_eq!(s.in_flight_count(), 0, "in-flight drained");
        assert!(s.pending_count() > 0, "remaining work stayed pending");
    }

    #[tokio::test]
    async fn concurrency_cap_through_driver() {
        // Cap=2 → driver dispatches up to 2 concurrent builds; the
        // remaining work waits.
        let mut s = FlatStrategy::new(Some(2));
        for n in 0..5 {
            s.enqueue(item_with_closure(
                rid(1),
                EvalId::new(0),
                jid(n),
                drv(&format!("/nix/store/{n:02}-x.drv"), &[]),
                Vec::new(),
            ));
        }
        let spawner = StubSpawner::new(HashMap::new());
        let cancel = CancelToken::new();
        let spawn_fn = spawner.clone().spawn_fn();
        drive(&mut s, spawn_fn, &cancel, |_| {}).await;

        // All 5 eventually dispatch in some order (cap doesn't limit
        // total throughput, only in-flight).
        assert_eq!(spawner.dispatch_order().len(), 5);
        assert_eq!(s.pending_count(), 0);
        assert_eq!(s.in_flight_count(), 0);
    }
}
