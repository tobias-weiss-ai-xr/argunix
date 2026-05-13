use crate::records::{
    BuilderRecord, EvalRecord, EvalWithRepo, ForgeStatusRecord, JobPhaseMetrics, JobRecord,
    JobWithContext, NewBuilder, NewEvaluation, NewJob, RepoRecord,
};
use argunix_domain::{
    BuilderId, BuilderPubkey, EvalId, EvalStatus, JobId, JobStatus, RepoId, Slug,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("status `{0}` from database is not a recognised value")]
    InvalidStatus(String),
    #[error("invalid sha in row id={id}: {error}")]
    InvalidSha {
        id: i64,
        #[source]
        error: argunix_domain::ShaError,
    },
    #[error("invalid slug in row id={id}: {error}")]
    InvalidSlug {
        id: i64,
        #[source]
        error: argunix_domain::SlugError,
    },
    #[error("invalid builder name in row id={id}: {error}")]
    InvalidBuilderName {
        id: i64,
        #[source]
        error: argunix_domain::BuilderNameError,
    },
    #[error("invalid builder pubkey in row id={id}: {error}")]
    InvalidBuilderPubkey {
        id: i64,
        #[source]
        error: argunix_domain::BuilderPubkeyError,
    },
    #[error("invalid builder capabilities JSON in row id={id}: {error}")]
    InvalidBuilderCapabilities {
        id: i64,
        #[source]
        error: serde_json::Error,
    },
}

#[async_trait]
pub trait RepoStore: Send + Sync {
    async fn upsert(&self, forge: &str, slug: &Slug) -> Result<RepoId, StoreError>;
    async fn get(&self, id: RepoId) -> Result<Option<RepoRecord>, StoreError>;
    async fn find(&self, forge: &str, slug: &Slug) -> Result<Option<RepoRecord>, StoreError>;
    async fn list(&self) -> Result<Vec<RepoRecord>, StoreError>;
    /// Read the argunix-managed webhook secret for `(forge, slug)`. None
    /// if the auto-install pass hasn't generated/stored one yet.
    async fn get_webhook_secret(
        &self,
        forge: &str,
        slug: &Slug,
    ) -> Result<Option<Vec<u8>>, StoreError>;
    /// Persist the generated `secret` and the forge-side `hook_id`
    /// returned by `Provider::ensure_webhook`. The hook id is opaque
    /// (string form) — GitHub returns an integer, GitLab an integer,
    /// Forgejo an integer; we store as text to keep the schema
    /// agnostic. Idempotent: overwrites any prior values.
    async fn set_webhook(
        &self,
        repo_id: RepoId,
        secret: &[u8],
        hook_id: &str,
    ) -> Result<(), StoreError>;
    /// Update the forge-supplied display fields for `repo_id`. All
    /// fields are optional — pass `None` to clear, `Some(_)` to
    /// overwrite. Called from each webhook handler so the UI's
    /// `/repos` and per-repo pages stay current with whatever the
    /// forge surfaces; on push and PR events all three forges include
    /// these on the `repository` / `project` payload object
    /// (`html_url` for GitHub / Forgejo, `web_url` for GitLab).
    /// `default_branch` is consumed by the README badge endpoint so
    /// `/badge/<forge>/<slug>.svg` reflects the default branch's
    /// status rather than "any-branch latest".
    async fn set_metadata(
        &self,
        repo_id: RepoId,
        name: Option<&str>,
        description: Option<&str>,
        web_url: Option<&str>,
        default_branch: Option<&str>,
    ) -> Result<(), StoreError>;
    /// Delete every repo (and its cascaded evaluations / jobs / queue
    /// rows / forge_status rows) whose `(forge, slug)` does not appear
    /// in `keep`. Returns the deleted repo records so the caller can
    /// also clean up their on-disk logs and GC roots. Runs in a single
    /// transaction.
    async fn prune_repos_not_in(
        &self,
        keep: &[(String, Slug)],
    ) -> Result<Vec<RepoRecord>, StoreError>;
}

#[async_trait]
pub trait EvalStore: Send + Sync {
    async fn create(&self, new: NewEvaluation) -> Result<EvalId, StoreError>;
    async fn get(&self, id: EvalId) -> Result<Option<EvalRecord>, StoreError>;
    async fn set_status(&self, id: EvalId, status: EvalStatus) -> Result<(), StoreError>;
    /// Mark this evaluation as started: writes `started_at` and flips
    /// status. Idempotent in the sense that calling it twice will
    /// overwrite `started_at` — callers (worker on first pickup) should
    /// only call it once per eval.
    async fn start(
        &self,
        id: EvalId,
        started_at: DateTime<Utc>,
        status: EvalStatus,
    ) -> Result<(), StoreError>;
    async fn finish(
        &self,
        id: EvalId,
        status: EvalStatus,
        finished_at: DateTime<Utc>,
    ) -> Result<(), StoreError>;
    /// Mark the eval as `EvaluationFailed` and record the worker's
    /// captured error string in `failure_reason`. The UI surfaces
    /// this on the per-eval page so operators can see *why* an eval
    /// failed (clone timeout, nix-eval-jobs stderr, …) without
    /// digging through daemon logs. Use this instead of plain
    /// `finish(EvaluationFailed)` when a reason is available.
    async fn fail_with_reason(
        &self,
        id: EvalId,
        reason: &str,
        finished_at: DateTime<Utc>,
    ) -> Result<(), StoreError>;
    /// Mark the `Evaluating → Building` transition: flips status and
    /// stamps `building_started_at`. Drives the eval/build wall-clock
    /// split on the per-eval UI page. Use this instead of `set_status`
    /// for the entry into `Building` so the timestamp is always
    /// recorded atomically with the status change.
    async fn mark_building(
        &self,
        id: EvalId,
        building_started_at: DateTime<Utc>,
    ) -> Result<(), StoreError>;
    /// Most-recent evaluations for `repo_id`, newest first, capped at `limit`.
    /// Used by the read-only UI's repo page.
    async fn list_by_repo(
        &self,
        repo_id: RepoId,
        limit: u32,
    ) -> Result<Vec<EvalRecord>, StoreError>;
    /// All non-terminal evaluations (queued / evaluating / building) for
    /// `repo_id` whose `git_ref` *starts with* `branch_key_prefix`. Used by
    /// cancel-on-new-push: a fresh push on a branch finds all
    /// in-flight evals for that branch and cancels them.
    /// See [docs/concepts/cancel-on-push.md].
    async fn list_active_by_branch_key(
        &self,
        repo_id: RepoId,
        branch_key_prefix: &str,
    ) -> Result<Vec<EvalRecord>, StoreError>;
    /// IDs of every evaluation in the `Queued` state — i.e. webhook
    /// landed, row created, but the worker hasn't picked it up yet.
    /// Used at daemon startup to redrive evals the previous instance
    /// died before processing.
    async fn list_queued_ids(&self) -> Result<Vec<EvalId>, StoreError>;

    /// IDs of evaluations that were mid-build when the previous daemon
    /// instance died — i.e. `status = 'building'`. Their jobs are
    /// already persisted, so the worker can skip the clone / eval /
    /// persist phases and pick up the build loop from where it
    /// stopped. Companion to `JobStore::requeue_interrupted_for_eval`
    /// which flips this eval's `Interrupted` jobs back to `Queued` so
    /// the resumed worker actually retries them.
    async fn list_building_ids(&self) -> Result<Vec<EvalId>, StoreError>;
    /// Up to `limit` evaluations whose row sits in `status`, joined
    /// with their repo for UI display. Ordering: oldest started first
    /// (so a stale `Evaluating` row from a crashed worker floats to
    /// the top of the status page) — we ORDER BY id ASC since
    /// `started_at` is `NULL` for `Queued` rows. Used by the status
    /// page to render "evaluating right now" + "eval queue depth"
    /// without N+1'ing through `repos`.
    async fn list_by_status(
        &self,
        status: EvalStatus,
        limit: u32,
    ) -> Result<Vec<EvalWithRepo>, StoreError>;

    /// Retention pickers. Both filter to *terminal* statuses
    /// (`evaluation_failed`, `done`, `cancelled`) — in-flight evals are
    /// never returned. Both also require `finished_at IS NOT NULL` so a
    /// terminal row that somehow lost its timestamp can't be selected.
    ///
    /// Age-based picker: every terminal eval for `repo_id` whose
    /// `finished_at` is at or before `cutoff`. Used by the per-repo
    /// max-age sweep.
    async fn list_terminal_evals_older_than(
        &self,
        repo_id: RepoId,
        cutoff: DateTime<Utc>,
    ) -> Result<Vec<EvalRecord>, StoreError>;

    /// Size-based picker: terminal evals across all repos, oldest
    /// `finished_at` first, capped at `limit`. The GC walks this list
    /// deleting one eval at a time, re-measuring on-disk size after
    /// each, until under the budget.
    async fn list_terminal_evals_oldest_first(
        &self,
        limit: u32,
    ) -> Result<Vec<EvalRecord>, StoreError>;

    /// Cascade-delete one evaluation: removes its `queue` rows,
    /// `forge_status` rows, `jobs` rows, and the `evaluations` row
    /// itself in a single transaction. Mirrors the cascade shape used
    /// by `RepoStore::prune_repos_not_in`. The caller is responsible
    /// for cleaning up on-disk state (logs, GC roots) keyed on
    /// `<repo_id>/<eval_id>/`. No-op if the row doesn't exist.
    async fn delete_eval_cascade(&self, eval_id: EvalId) -> Result<(), StoreError>;
}

#[async_trait]
pub trait JobStore: Send + Sync {
    async fn create(&self, new: NewJob) -> Result<JobId, StoreError>;
    async fn get(&self, id: JobId) -> Result<Option<JobRecord>, StoreError>;
    async fn list_by_eval(&self, eval_id: EvalId) -> Result<Vec<JobRecord>, StoreError>;
    /// Every job currently in `Running` state across the cluster, joined
    /// with its evaluation and repo so the status page can render it
    /// without N+1 lookups. Oldest-started first (so "longest in flight"
    /// floats to the top).
    async fn list_running(&self) -> Result<Vec<JobWithContext>, StoreError>;
    /// Up to `limit` jobs in `Queued` state, oldest first. Same join shape
    /// as `list_running`. Jobs whose eval is in a terminal state are
    /// filtered out — they're queue rows the dispatcher hasn't reaped yet
    /// (Cancelled evals leave queued job rows behind) and surfacing them
    /// would lie about real upcoming work.
    async fn list_queued(&self, limit: u32) -> Result<Vec<JobWithContext>, StoreError>;
    async fn set_status(&self, id: JobId, status: JobStatus) -> Result<(), StoreError>;
    /// Mark a job as `Running` and stamp `started_at`.
    async fn start(&self, id: JobId, started_at: DateTime<Utc>) -> Result<(), StoreError>;
    /// Mark a job terminal: set `status`, `finished_at`, and (where present)
    /// the on-disk paths produced by the build pipeline. `metrics` carries
    /// per-phase byte counts and durations for pool-dispatched builds;
    /// pass [`JobPhaseMetrics::default`] (all `None`) for local builds or
    /// failures with no transport activity to record.
    async fn finish(
        &self,
        id: JobId,
        status: JobStatus,
        finished_at: DateTime<Utc>,
        log_path: Option<&str>,
        output_path: Option<&str>,
        metrics: &JobPhaseMetrics,
    ) -> Result<(), StoreError>;
    /// Used at boot: mark every still-`running` job as `interrupted`.
    /// Returns the number of rows updated. Does NOT touch `interrupt_count`
    /// — boot-time interruption is argunix's fault, not the builder's, and
    /// shouldn't push the job toward the per-job retry cap.
    async fn mark_running_interrupted(&self) -> Result<u64, StoreError>;

    /// Crash-recovery companion to `mark_running_interrupted`. Flips
    /// every `Interrupted` job belonging to `eval_id` back to `Queued`
    /// and clears its `started_at` so a resumed worker can dispatch it
    /// from scratch. Returns the number of rows updated. Like
    /// `mark_running_interrupted`, does NOT touch `interrupt_count`
    /// — the prior interruption was the daemon dying, not the job
    /// itself misbehaving.
    async fn requeue_interrupted_for_eval(&self, eval_id: EvalId) -> Result<u64, StoreError>;

    /// Record dispatch of `id` to `builder_id` and stamp `started_at`.
    /// Sets status to `Running` and writes the builder for anti-affinity
    /// tracking on any later re-queue.
    async fn dispatch(
        &self,
        id: JobId,
        builder_id: BuilderId,
        started_at: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    /// Transport-failure recovery for the dynamic builder pool. Under
    /// a single transaction: increment `interrupt_count`; if the new
    /// count is ≤ `MAX_INTERRUPTIONS`, flip status to `Interrupted`;
    /// otherwise flip to `Failure`, stamp `finished_at`, and write
    /// `failure_reason="exceeded interruption retry limit"`. Returns
    /// the outcome and (when re-queueing) the prior builder so the
    /// caller can build an anti-affinity exclude-set for the next
    /// dispatch.
    async fn record_interruption(
        &self,
        id: JobId,
        now: DateTime<Utc>,
    ) -> Result<InterruptOutcome, StoreError>;
}

/// Maximum number of transport interruptions before a job flips from
/// `Interrupted` to `Failure`. Counter lives on `jobs.interrupt_count`.
pub const MAX_INTERRUPTIONS: u32 = 3;

/// What `JobStore::record_interruption` decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterruptOutcome {
    /// Re-queue with anti-affinity excluding `prior_builder`.
    ReQueued {
        new_count: u32,
        prior_builder: Option<BuilderId>,
    },
    /// Cap exceeded; the job is now in `Failure` and should not be re-queued.
    FailedExceeded { prior_builder: Option<BuilderId> },
}

#[async_trait]
pub trait BuilderStore: Send + Sync {
    /// First-connect enrollment, or capabilities refresh on reconnect.
    /// Idempotent on `name`: existing rows have their pubkey, capabilities,
    /// and `last_seen` overwritten and `revoked_at` cleared. Returns the
    /// row's id so the caller can register the in-memory connection.
    async fn upsert(&self, new: NewBuilder, now: DateTime<Utc>) -> Result<BuilderId, StoreError>;

    async fn get(&self, id: BuilderId) -> Result<Option<BuilderRecord>, StoreError>;

    async fn find_by_name(&self, name: &str) -> Result<Option<BuilderRecord>, StoreError>;

    /// Pubkey-auth lookup. Skips revoked rows so a revoked builder is forced
    /// back through the enrollment-token path. None means "no active row
    /// matches this pubkey" — auth fails and the agent retries with the
    /// enrollment token.
    async fn find_active_by_pubkey(
        &self,
        pubkey: &BuilderPubkey,
    ) -> Result<Option<BuilderRecord>, StoreError>;

    /// Touch `last_seen`. Called on every successful heartbeat.
    async fn mark_seen(&self, id: BuilderId, now: DateTime<Utc>) -> Result<(), StoreError>;

    /// `argunixctl builders revoke <name>`. Sets `revoked_at`; subsequent
    /// pubkey-auth attempts will fail until the builder re-enrolls with a
    /// fresh token. Returns false if the name is unknown.
    async fn revoke(&self, name: &str, now: DateTime<Utc>) -> Result<bool, StoreError>;

    /// `argunixctl builders rename`. Returns false if `old` doesn't exist or
    /// `new` already exists.
    async fn rename(&self, old: &str, new: &str) -> Result<bool, StoreError>;

    /// All builders, oldest enrollment first. Includes revoked rows so
    /// `argunixctl builders` can show them.
    async fn list_all(&self) -> Result<Vec<BuilderRecord>, StoreError>;
}

#[async_trait]
pub trait ForgeStatusStore: Send + Sync {
    async fn upsert(
        &self,
        eval_id: EvalId,
        kind: &str,
        handle: Option<&str>,
    ) -> Result<(), StoreError>;
    async fn get(
        &self,
        eval_id: EvalId,
        kind: &str,
    ) -> Result<Option<ForgeStatusRecord>, StoreError>;
}
