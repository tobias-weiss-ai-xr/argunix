//! Pluggable scheduling strategies.
//!
//! [`ScheduleStrategy`] is the dispatch interface; [`FlatStrategy`] is
//! the weighted-fair-queueing implementation that's been here from
//! day one. Future strategies (e.g. a DAG-gated one for intra-eval
//! dependency-aware dispatch) plug in through the same trait without
//! touching the daemon.
//!
//! See `flat.rs` for the WFQ algorithm details and `wfq.rs` for the
//! reusable WFQ core that any strategy can wrap.

mod flat;
mod wfq;

#[cfg(test)]
mod tests;

pub use flat::FlatStrategy;
pub use wfq::DEFAULT_WEIGHT;

use argunix_domain::{EvalId, JobId, JobStatus, RepoId};

/// Returned from [`ScheduleStrategy::dispatch`] when a job was selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dispatched {
    pub job_id: JobId,
    pub repo_id: RepoId,
}

/// One unit of work handed to a strategy. Strategies cherry-pick the
/// fields they care about; ones they ignore are noise to them.
///
/// `FlatStrategy` reads only `repo_id` and `job_id`. A future
/// dependency-aware strategy will additionally read `drv_path`,
/// `system`, `required_features`, and `input_drvs` to build a Step
/// graph and gate dispatch on dependency completion.
#[derive(Debug, Clone)]
pub struct ScheduleItem {
    pub repo_id: RepoId,
    pub eval_id: EvalId,
    pub job_id: JobId,
    pub drv_path: Option<String>,
    pub system: Option<String>,
    pub required_features: Vec<String>,
    /// Direct input derivations of `drv_path`. `None` means "unknown" /
    /// "not computed" — strategies that gate on deps must treat that as
    /// "no deps to wait for", so populating the trait without populating
    /// the field stays behaviourally identical to the flat scheduler.
    pub input_drvs: Option<Vec<String>>,
}

/// Strategy selector for [`build`]. Surfaces in config as
/// `[scheduler] kind = "flat"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SchedulerKind {
    Flat,
}

impl Default for SchedulerKind {
    fn default() -> Self {
        SchedulerKind::Flat
    }
}

/// Pluggable scheduling interface. Implementations decide what to
/// dispatch next from the items they've been given; the daemon
/// drives every strategy through this trait without branching on type.
///
/// `complete` requires a terminal [`JobStatus`]; passing a non-terminal
/// status is a programming error (debug-asserted in [`FlatStrategy`]).
pub trait ScheduleStrategy: Send {
    fn set_weight(&mut self, repo_id: RepoId, weight: u32);
    fn enqueue(&mut self, item: ScheduleItem);
    fn dispatch(&mut self) -> Option<Dispatched>;
    fn complete(&mut self, job_id: JobId, status: JobStatus) -> Option<RepoId>;

    fn pending_count(&self) -> usize;
    fn in_flight_count(&self) -> usize;
    fn pending_for(&self, repo_id: RepoId) -> usize;

    /// No-op for strategies that don't care about wall clock. Kept on
    /// the trait so callers can drive every strategy with a periodic
    /// tick without branching on type.
    fn tick(&mut self, _dt: std::time::Duration) {}
}

/// Construct a strategy from config. Daemon owns the box behind a
/// `Mutex` for service-mode use.
pub fn build(kind: SchedulerKind, concurrency_cap: Option<usize>) -> Box<dyn ScheduleStrategy> {
    match kind {
        SchedulerKind::Flat => Box::new(FlatStrategy::new(concurrency_cap)),
    }
}
