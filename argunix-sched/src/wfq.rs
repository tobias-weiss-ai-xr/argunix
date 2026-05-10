//! Weighted-fair-queueing core. Used by [`crate::FlatStrategy`] today;
//! future strategies can wrap this to compose WFQ fairness with extra
//! scheduling concerns (e.g. dependency gating).
//!
//! Goals:
//! - per-repo `weight` (default 1) gives a target dispatch ratio,
//! - no repo is starved as long as it has pending work,
//! - in-flight count is capped by a configurable global limit,
//! - data structures are cheap to rebuild on hot-reload.
//!
//! ## Algorithm
//!
//! Each repo carries a monotonic `virtual_time: f64`. The repo with the
//! lowest virtual_time and pending work wins each dispatch; ties are
//! broken by the oldest pending entry's enqueue sequence (stable FIFO
//! tiebreak). After dispatch, the winner's virtual_time advances by
//! `1.0 / weight`, so heavier-weighted repos move forward more slowly per
//! job and therefore dispatch more often.
//!
//! When a repo transitions from empty to non-empty, its virtual_time is
//! pulled forward to at least `system_virtual_time` (the minimum
//! virtual_time among repos with pending work). This keeps a long-idle
//! repo from accumulating an unbounded "free-ride" advantage and stops
//! it from monopolising the scheduler when it next becomes active.
//!
//! The plan's text describes a credit-and-tick formulation; we ship
//! virtual-time WFQ instead because the credit form does not actually
//! enforce weight ratios under steady-state dispatch (the heavy-weighted
//! repo always has more credit and never lets the lighter repo win after
//! the cap saturates). Both formulations meet the same observable spec:
//! dispatches respect weight, no repo is starved.

use crate::Dispatched;
use argunix_domain::{JobId, RepoId};
use std::collections::{HashMap, VecDeque};

/// Default per-repo weight (`repos[].weight` defaults to 1).
pub const DEFAULT_WEIGHT: u32 = 1;

#[derive(Debug)]
struct RepoState {
    weight: u32,
    virtual_time: f64,
    /// Each entry carries its enqueue seq so the head's seq is the
    /// repo's "earliest pending" without any auxiliary bookkeeping.
    pending: VecDeque<(u64, JobId)>,
}

impl RepoState {
    fn new(weight: u32) -> Self {
        Self {
            weight: weight.max(1),
            virtual_time: 0.0,
            pending: VecDeque::new(),
        }
    }

    fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    fn earliest_seq(&self) -> Option<u64> {
        self.pending.front().map(|(seq, _)| *seq)
    }
}

#[derive(Debug)]
pub(crate) struct WfqCore {
    repos: HashMap<RepoId, RepoState>,
    in_flight: HashMap<JobId, RepoId>,
    concurrency_cap: Option<usize>,
    next_seq: u64,
}

impl WfqCore {
    pub(crate) fn new(concurrency_cap: Option<usize>) -> Self {
        Self {
            repos: HashMap::new(),
            in_flight: HashMap::new(),
            concurrency_cap,
            next_seq: 0,
        }
    }

    /// Register or update a repo's weight. Updates retain current
    /// virtual_time. New repos start at `0.0`; that's pulled forward to
    /// `system_virtual_time` when they enqueue their first job.
    pub(crate) fn set_weight(&mut self, repo_id: RepoId, weight: u32) {
        let state = self
            .repos
            .entry(repo_id)
            .or_insert_with(|| RepoState::new(weight));
        state.weight = weight.max(1);
    }

    /// Add a job to its repo's pending queue. Repos auto-register with
    /// [`DEFAULT_WEIGHT`] if not yet known. If the repo was empty *and*
    /// at least one other repo currently has pending work, its
    /// virtual_time is pulled forward to `system_virtual_time` so the
    /// just-arrived repo doesn't free-ride on idle history.
    pub(crate) fn enqueue(&mut self, repo_id: RepoId, job_id: JobId) {
        let snap_to = self.system_virtual_time();
        let seq = self.next_seq;
        self.next_seq += 1;

        let state = self
            .repos
            .entry(repo_id)
            .or_insert_with(|| RepoState::new(DEFAULT_WEIGHT));
        if !state.has_pending() {
            if let Some(min_vt) = snap_to {
                if state.virtual_time < min_vt {
                    state.virtual_time = min_vt;
                }
            }
        }
        state.pending.push_back((seq, job_id));
    }

    /// Pick the next job to dispatch. Selection rules:
    /// 1. Skip if the in-flight cap is full.
    /// 2. Among repos with pending work, pick the one with the lowest
    ///    virtual_time. Ties → smallest earliest-pending seq.
    /// 3. Advance the winner's virtual_time by `1 / weight`.
    pub(crate) fn dispatch(&mut self) -> Option<Dispatched> {
        if let Some(cap) = self.concurrency_cap {
            if self.in_flight.len() >= cap {
                return None;
            }
        }

        let mut best: Option<(RepoId, f64, u64)> = None;
        for (&repo_id, state) in &self.repos {
            if !state.has_pending() {
                continue;
            }
            let earliest = state.earliest_seq().expect("non-empty pending");
            let candidate = (repo_id, state.virtual_time, earliest);
            best = Some(match best {
                None => candidate,
                Some(prev) => {
                    if candidate.1 < prev.1 || (candidate.1 == prev.1 && candidate.2 < prev.2) {
                        candidate
                    } else {
                        prev
                    }
                }
            });
        }

        let (repo_id, _, _) = best?;
        let state = self.repos.get_mut(&repo_id).expect("found repo above");
        let (_seq, job_id) = state.pending.pop_front().expect("repo had pending");
        state.virtual_time += 1.0 / state.weight as f64;
        self.in_flight.insert(job_id, repo_id);
        Some(Dispatched { job_id, repo_id })
    }

    /// Mark a job as finished; frees an in-flight slot.
    /// Returns the repo the job belonged to, if it was tracked.
    pub(crate) fn complete(&mut self, job_id: JobId) -> Option<RepoId> {
        self.in_flight.remove(&job_id)
    }

    pub(crate) fn pending_count(&self) -> usize {
        self.repos.values().map(|s| s.pending.len()).sum()
    }

    pub(crate) fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    pub(crate) fn pending_for(&self, repo_id: RepoId) -> usize {
        self.repos
            .get(&repo_id)
            .map(|s| s.pending.len())
            .unwrap_or(0)
    }

    pub(crate) fn virtual_time_for(&self, repo_id: RepoId) -> Option<f64> {
        self.repos.get(&repo_id).map(|s| s.virtual_time)
    }

    fn system_virtual_time(&self) -> Option<f64> {
        self.repos
            .values()
            .filter(|s| s.has_pending())
            .map(|s| s.virtual_time)
            .reduce(f64::min)
    }
}
