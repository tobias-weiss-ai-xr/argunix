use argunix_domain::{
    AttrPath, BuilderCapabilities, BuilderId, BuilderName, BuilderPubkey, EvalId, EvalStatus,
    JobId, JobStatus, RepoId, Sha, Slug,
};
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRecord {
    pub id: RepoId,
    pub forge: String,
    pub slug: Slug,
    /// Forge-supplied display name. Populated from each webhook payload.
    /// `None` until the first matching webhook lands.
    pub name: Option<String>,
    /// Forge-supplied description. Same population rules as `name`.
    pub description: Option<String>,
    /// Forge-supplied project web URL. `repository.html_url` on
    /// GitHub / Forgejo, `repository.web_url` on GitLab. `None` until
    /// the first matching webhook lands; UI falls back to a URL
    /// constructed from the YAML config until then.
    pub web_url: Option<String>,
}

/// Fields supplied when creating a new evaluation row.
#[derive(Debug, Clone)]
pub struct NewEvaluation {
    pub repo_id: RepoId,
    pub trigger: String,
    pub git_ref: String,
    pub sha: Sha,
    /// PR number when `trigger == "pull_request"`. `None` for branch
    /// pushes. Populated from the webhook payload so the UI can link
    /// directly to the PR on the forge.
    pub pr_number: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalRecord {
    pub id: EvalId,
    pub repo_id: RepoId,
    pub trigger: String,
    pub git_ref: String,
    pub sha: Sha,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub status: EvalStatus,
    /// PR number for pull-request evals. See [`NewEvaluation::pr_number`].
    pub pr_number: Option<u32>,
    /// Stamp set when the eval transitioned `Evaluating → Building`.
    /// Splits the eval's total wall-clock into eval time
    /// (`building_started_at - started_at`) and build time
    /// (`finished_at - building_started_at`). `None` for rows that
    /// never reached `Building` (eval-failed, cancelled mid-eval) or
    /// that finished before this column existed.
    pub building_started_at: Option<DateTime<Utc>>,
    /// Worker-captured error detail for `EvaluationFailed` rows —
    /// typically the chained anyhow `{:#}` message
    /// (`cloning <repo>: timed out`, multi-line nix-eval-jobs
    /// stderr, etc.). `None` on all other terminal states and on
    /// rows that pre-date this column.
    pub failure_reason: Option<String>,
}

/// Fields supplied when creating a new job row.
#[derive(Debug, Clone)]
pub struct NewJob {
    pub eval_id: EvalId,
    pub attr_path: AttrPath,
    pub drv_path: Option<String>,
    pub system: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRecord {
    pub id: JobId,
    pub eval_id: EvalId,
    pub attr_path: AttrPath,
    pub drv_path: Option<String>,
    pub system: String,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub status: JobStatus,
    pub log_path: Option<String>,
    pub output_path: Option<String>,
    /// The most recent builder this job was dispatched to. None means the
    /// job has never been dispatched (still queued) or pre-dates M13. The
    /// dispatcher reads this on re-queue to set anti-affinity.
    pub builder_id: Option<BuilderId>,
    /// How many times this job has been interrupted by transport drop /
    /// graceful builder shutdown. Capped by `MAX_INTERRUPTIONS`; on
    /// exceedance the job flips to `Failure` with `failure_reason` set.
    pub interrupt_count: u32,
    /// Set when a job fails for a non-build-process reason (currently only
    /// "exceeded interruption retry limit"). NULL for build-level failures.
    pub failure_reason: Option<String>,
    /// Per-phase accounting for pool-dispatched builds (M16). All `None`
    /// for jobs that were never dispatched, built locally, or finished
    /// before this column-set existed.
    pub phase_metrics: JobPhaseMetrics,
}

/// Bytes through our russh tunnel + wall-clock per pool-dispatch phase.
/// Stored on `jobs` once the build reaches a terminal state; rendered
/// on the job detail page so operators can answer "where did the wall
/// clock / bandwidth go on this build."
///
/// `bytes` measure transport-level bytes through the proxy in each
/// direction (daemon-protocol framing included), not the on-disk size
/// of the closure. They'll be a few percent above what `du -b $output`
/// reports — that's the right number for "what did our network
/// actually carry."
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobPhaseMetrics {
    /// Bytes daemon → builder during the drv-closure push.
    pub push_bytes: Option<u64>,
    /// Wall-clock of the entire `nix copy --to` invocation.
    pub push_ms: Option<u64>,
    /// Wall-clock between agent's `BuildStarted` and `BuildFinished`.
    pub build_ms: Option<u64>,
    /// Bytes builder → daemon during the output-closure pull.
    pub pull_bytes: Option<u64>,
    /// Wall-clock of the entire `nix copy --from` invocation.
    pub pull_ms: Option<u64>,
}

/// A job row joined with its evaluation and repo. Used by the status
/// page to render "what is the cluster doing right now?" without the
/// caller having to do N+1 lookups against the eval/repo tables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobWithContext {
    pub job: JobRecord,
    pub forge: String,
    pub slug: Slug,
    pub git_ref: String,
    pub short_sha: String,
}

/// An evaluation row joined with its repo. Status page uses this to
/// show currently-evaluating + eval-queued rows without the caller
/// doing N+1 lookups against `repos`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalWithRepo {
    pub eval: EvalRecord,
    pub forge: String,
    pub slug: Slug,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeStatusRecord {
    pub eval_id: EvalId,
    pub kind: String,
    pub handle: Option<String>,
    pub last_posted_at: DateTime<Utc>,
}

/// Fields supplied at first-connect enrollment.
#[derive(Debug, Clone)]
pub struct NewBuilder {
    pub name: BuilderName,
    pub pubkey: BuilderPubkey,
    pub capabilities: BuilderCapabilities,
}

/// One row of the `builders` table. `last_seen` is updated on every
/// successful `hello`; `revoked_at` is set by `argunixctl builders revoke`
/// and (when present) makes pubkey-auth lookups skip the row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuilderRecord {
    pub id: BuilderId,
    pub name: BuilderName,
    pub pubkey: BuilderPubkey,
    pub capabilities: BuilderCapabilities,
    pub enrolled_at: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}
