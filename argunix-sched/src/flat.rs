//! Flat WFQ-only strategy. Reads each [`ScheduleItem`]'s top-level
//! Job + head derivation; ignores the closure. Every dispatch is a
//! head Step, so [`Dispatched::head_job`] is always `Some`. Closure
//! dedup and dependency gating do not happen — that's [`crate::DagStrategy`]'s
//! job. The two strategies share the same WFQ core for cross-repo
//! fairness.

use crate::wfq::WfqCore;
use crate::{
    CascadedSkip, CompletionEffects, DerivationInfo, DispatchToken, Dispatched, ScheduleItem,
    ScheduleStrategy,
};
use argunix_domain::{EvalId, JobId, JobStatus, RepoId};
use std::collections::HashMap;

/// Per-Job state FlatStrategy needs to reconstruct a [`Dispatched`]
/// when WFQ pops the JobId. Lives here (not in WfqCore) because WFQ
/// is intentionally generic over the tag and doesn't know about
/// schedule-item-shaped fields.
#[derive(Debug, Clone)]
struct FlatPending {
    repo_id: RepoId,
    eval_id: EvalId,
    head_drv: DerivationInfo,
}

#[derive(Debug)]
pub struct FlatStrategy {
    wfq: WfqCore<JobId>,
    pending: HashMap<JobId, FlatPending>,
    next_token: u64,
    /// Token → JobId so `complete(token, …)` can resolve back to the
    /// WFQ tag. FlatStrategy never dispatches non-head steps, so each
    /// live token corresponds to exactly one JobId.
    in_flight_token_to_job: HashMap<DispatchToken, JobId>,
}

impl FlatStrategy {
    pub fn new(concurrency_cap: Option<usize>) -> Self {
        Self {
            wfq: WfqCore::new(concurrency_cap),
            pending: HashMap::new(),
            next_token: 0,
            in_flight_token_to_job: HashMap::new(),
        }
    }

    /// WFQ-internal observability: per-repo virtual time. Only meaningful
    /// for the WFQ algorithm; not on the trait because other strategies
    /// (e.g. dependency-gated ones) won't have a virtual time per repo.
    pub fn virtual_time_for(&self, repo_id: RepoId) -> Option<f64> {
        self.wfq.virtual_time_for(repo_id)
    }

    fn mint_token(&mut self) -> DispatchToken {
        let t = DispatchToken(self.next_token);
        self.next_token += 1;
        t
    }
}

impl Default for FlatStrategy {
    fn default() -> Self {
        Self::new(None)
    }
}

impl ScheduleStrategy for FlatStrategy {
    fn set_weight(&mut self, repo_id: RepoId, weight: u32) {
        self.wfq.set_weight(repo_id, weight);
    }

    fn enqueue(&mut self, item: ScheduleItem) {
        // Closure is ignored: flat strategy treats every Job as
        // independent and lets nix's builder-side substituter handle
        // shared internal derivations.
        self.pending.insert(
            item.job_id,
            FlatPending {
                repo_id: item.repo_id,
                eval_id: item.eval_id,
                head_drv: item.head_drv,
            },
        );
        self.wfq.enqueue(item.repo_id, item.job_id);
    }

    fn dispatch(&mut self) -> Option<Dispatched> {
        let popped = self.wfq.dispatch()?;
        // Clone now to release the borrow on `self.pending` before we
        // touch `self.next_token` / `self.in_flight_token_to_job`.
        let pend = self
            .pending
            .get(&popped.tag)
            .expect("WFQ popped a JobId we have no pending entry for")
            .clone();
        let token = self.mint_token();
        self.in_flight_token_to_job.insert(token, popped.tag);
        Some(Dispatched {
            token,
            repo_id: popped.repo_id,
            eval_id: pend.eval_id,
            drv_path: pend.head_drv.drv_path,
            system: pend.head_drv.system,
            required_features: pend.head_drv.required_features,
            head_job: Some(popped.tag),
        })
    }

    fn complete(&mut self, token: DispatchToken, status: JobStatus) -> CompletionEffects {
        debug_assert!(
            status.is_terminal(),
            "complete called with non-terminal status: {status:?}",
        );
        let Some(job_id) = self.in_flight_token_to_job.remove(&token) else {
            return CompletionEffects::default();
        };
        self.pending.remove(&job_id);
        CompletionEffects {
            repo_id: self.wfq.complete(job_id),
            cascaded_skips: Vec::new(),
        }
    }

    fn cancel_eval(&mut self, eval_id: EvalId) -> Vec<CascadedSkip> {
        // FlatStrategy holds no Step graph, so cancelling an eval
        // reduces to: remove its pending entries from WFQ, drop the
        // metadata, return one skip per dropped Job. In-flight Jobs
        // are left alone — the daemon's per-eval CancelToken signals
        // them; their results arrive via complete() in due course.
        let removed = self
            .wfq
            .cancel_pending(|tag| matches!(self.pending.get(tag), Some(p) if p.eval_id == eval_id));
        let reason = format!("eval {} cancelled", eval_id.get());
        removed
            .into_iter()
            .filter_map(|jid| {
                let p = self.pending.remove(&jid)?;
                Some(CascadedSkip {
                    job_id: jid,
                    eval_id: p.eval_id,
                    repo_id: p.repo_id,
                    reason_drv: reason.clone(),
                })
            })
            .collect()
    }

    fn pending_count(&self) -> usize {
        self.wfq.pending_count()
    }

    fn in_flight_count(&self) -> usize {
        self.wfq.in_flight_count()
    }

    fn pending_for(&self, repo_id: RepoId) -> usize {
        self.wfq.pending_for(repo_id)
    }
}
