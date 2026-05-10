//! Flat WFQ-only strategy. Reads `repo_id` and `job_id` from each
//! [`ScheduleItem`] and ignores the rest. This is the dispatch
//! behaviour medusa has had since the scheduler crate was created;
//! the trait + new fields exist so future strategies can layer
//! dependency gating on top without disturbing this one.

use crate::wfq::WfqCore;
use crate::{Dispatched, ScheduleItem, ScheduleStrategy};
use argunix_domain::{JobId, JobStatus, RepoId};

#[derive(Debug)]
pub struct FlatStrategy {
    wfq: WfqCore,
}

impl FlatStrategy {
    pub fn new(concurrency_cap: Option<usize>) -> Self {
        Self {
            wfq: WfqCore::new(concurrency_cap),
        }
    }

    /// WFQ-internal observability: per-repo virtual time. Only meaningful
    /// for the WFQ algorithm; not on the trait because other strategies
    /// (e.g. dependency-gated ones) won't have a virtual time per repo.
    pub fn virtual_time_for(&self, repo_id: RepoId) -> Option<f64> {
        self.wfq.virtual_time_for(repo_id)
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
        self.wfq.enqueue(item.repo_id, item.job_id);
    }

    fn dispatch(&mut self) -> Option<Dispatched> {
        self.wfq.dispatch()
    }

    fn complete(&mut self, job_id: JobId, status: JobStatus) -> Option<RepoId> {
        debug_assert!(
            status.is_terminal(),
            "complete called with non-terminal status: {status:?}",
        );
        self.wfq.complete(job_id)
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
