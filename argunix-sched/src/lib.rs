//! Pluggable scheduling strategies.
//!
//! [`ScheduleStrategy`] is the dispatch interface. Implementations
//! today:
//!
//! - [`FlatStrategy`] — weighted-fair-queueing across repos at the
//!   top-level-Job granularity. The dispatch behaviour medusa shipped
//!   with; closure information on a [`ScheduleItem`] is ignored.
//! - [`DagStrategy`] — same WFQ fairness, plus an in-memory Step graph
//!   that dedups derivations across a Job's transitive closure and
//!   gates dispatch on dependency completion. Solves the
//!   "two top-level Jobs share an internal `glibc.drv` and both
//!   builders rebuild it independently" problem at the source.
//!
//! The trait surface is wide enough that an internal Step (a drv that
//! isn't a top-level attr) can be dispatched too: [`Dispatched`] carries
//! `head_job: Option<JobId>`, which is `Some` for top-level Jobs and
//! `None` for internal Steps. The daemon-side dispatch handler
//! branches on that to decide whether to update a DB row + post a forge
//! check (`Some`) or just realize the drv with no DB side-effect
//! (`None`).

mod dag;
mod flat;
mod wfq;

#[cfg(test)]
mod tests;

pub use dag::DagStrategy;
pub use flat::FlatStrategy;
pub use wfq::DEFAULT_WEIGHT;

use argunix_domain::{EvalId, JobId, JobStatus, RepoId};

/// Strategy-assigned identifier for one in-flight dispatch. Opaque to
/// callers — they hand it back to [`ScheduleStrategy::complete`] when
/// the build terminates and the strategy looks up its own state.
///
/// Tokens are unique per strategy instance and never reused for the
/// lifetime of that instance. They are *not* unique across strategy
/// instances or daemon restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DispatchToken(pub u64);

/// Information about one derivation, sufficient to dispatch a build of
/// it on a remote builder.
///
/// `input_drvs` is the *direct* input derivations of `drv_path`. The
/// strategy uses it to build a Step graph; for [`DagStrategy`] the
/// caller is expected to provide the full transitive closure (head
/// drv + every reachable input) on the [`ScheduleItem`] so the graph
/// is complete. Drvs whose `input_drvs` reference paths not present in
/// the supplied closure are treated as external (already substituted
/// or fetched at build time) and not gated on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivationInfo {
    pub drv_path: String,
    pub system: Option<String>,
    pub required_features: Vec<String>,
    pub input_drvs: Vec<String>,
}

/// One unit of work handed to a strategy. Strategies cherry-pick the
/// fields they care about; ones they ignore are noise to them.
///
/// [`FlatStrategy`] reads `repo_id`, `job_id`, and `head_drv`'s
/// `drv_path` / `system` / `required_features` for materialising
/// [`Dispatched`]; it ignores `closure`. [`DagStrategy`] additionally
/// uses `closure` to build the dependency graph.
#[derive(Debug, Clone)]
pub struct ScheduleItem {
    pub repo_id: RepoId,
    pub eval_id: EvalId,
    pub job_id: JobId,
    /// The top-level derivation backing this Job. `head_drv.drv_path`
    /// is the dispatch target for the Job itself.
    pub head_drv: DerivationInfo,
    /// Transitive input derivations of `head_drv`, excluding the head
    /// itself. Order is irrelevant; the strategy dedups by `drv_path`
    /// across this and prior items. May be empty (e.g. a Job whose
    /// derivation has no inputs in the eval).
    pub closure: Vec<DerivationInfo>,
}

/// Returned from [`ScheduleStrategy::dispatch`] when something is ready
/// to build. May represent either a top-level Job (head Step) or an
/// internal Step in some Job's closure.
#[derive(Debug, Clone)]
pub struct Dispatched {
    pub token: DispatchToken,
    pub repo_id: RepoId,
    pub eval_id: EvalId,
    pub drv_path: String,
    pub system: Option<String>,
    pub required_features: Vec<String>,
    /// `Some(job_id)` when this dispatch realises a top-level Job's
    /// `head_drv` — the daemon updates the Job row + posts a forge
    /// check on completion. `None` when this is an internal Step
    /// dispatched for the side effect of producing its output (other
    /// Steps depend on it); no DB row exists for these.
    pub head_job: Option<JobId>,
}

/// Side-effects of a [`ScheduleStrategy::complete`] call that the
/// daemon needs to act on without waiting for further dispatches.
///
/// `cascaded_skips` lists head Jobs that became unbuildable because a
/// Step in their closure failed. The daemon should mark those Jobs as
/// [`JobStatus::Failure`] (with a synthetic log explaining which dep
/// failed) and post a failed forge check — they will *not* show up in
/// any subsequent [`ScheduleStrategy::dispatch`] call.
#[derive(Debug, Default, Clone)]
pub struct CompletionEffects {
    /// Repo the just-completed dispatch belonged to, if it was tracked.
    /// Mirrors the historical return value of `complete`.
    pub repo_id: Option<RepoId>,
    pub cascaded_skips: Vec<CascadedSkip>,
}

/// One head Job that became unbuildable because a Step in its closure
/// terminated as `Failure` / `Cancelled` / `SkippedNoBuilder`.
#[derive(Debug, Clone)]
pub struct CascadedSkip {
    pub job_id: JobId,
    pub eval_id: EvalId,
    pub repo_id: RepoId,
    /// drvPath of the Step whose failure propagated up to this Job.
    /// Used by the daemon to render an actionable synthetic log
    /// ("skipped: dependency `<drv>` failed").
    pub reason_drv: String,
}

/// Strategy selector for [`build`]. Surfaces in config as
/// `[scheduler] kind = "flat" | "dag"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SchedulerKind {
    #[default]
    Flat,
    Dag,
}

/// Pluggable scheduling interface. Implementations decide what to
/// dispatch next from the items they've been given; the daemon
/// drives every strategy through this trait without branching on type.
///
/// `complete` requires a terminal [`JobStatus`]; passing a non-terminal
/// status is a programming error (debug-asserted in implementations).
pub trait ScheduleStrategy: Send {
    fn set_weight(&mut self, repo_id: RepoId, weight: u32);
    fn enqueue(&mut self, item: ScheduleItem);
    fn dispatch(&mut self) -> Option<Dispatched>;
    fn complete(&mut self, token: DispatchToken, status: JobStatus) -> CompletionEffects;

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
        SchedulerKind::Dag => Box::new(DagStrategy::new(concurrency_cap)),
    }
}
