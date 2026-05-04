use crate::records::{
    BuilderRecord, EvalRecord, ForgeStatusRecord, JobRecord, NewBuilder, NewEvaluation, NewJob,
    RepoRecord,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use medusa_domain::{BuilderId, BuilderPubkey, EvalId, EvalStatus, JobId, JobStatus, RepoId, Slug};

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
        error: medusa_domain::ShaError,
    },
    #[error("invalid slug in row id={id}: {error}")]
    InvalidSlug {
        id: i64,
        #[source]
        error: medusa_domain::SlugError,
    },
    #[error("invalid builder name in row id={id}: {error}")]
    InvalidBuilderName {
        id: i64,
        #[source]
        error: medusa_domain::BuilderNameError,
    },
    #[error("invalid builder pubkey in row id={id}: {error}")]
    InvalidBuilderPubkey {
        id: i64,
        #[source]
        error: medusa_domain::BuilderPubkeyError,
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
    /// Read the medusa-managed webhook secret for `(forge, slug)`. None
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
    async fn finish(
        &self,
        id: EvalId,
        status: EvalStatus,
        finished_at: DateTime<Utc>,
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
    /// cancel-on-new-push (Q39): a fresh push on a branch finds all
    /// in-flight evals for that branch and cancels them.
    async fn list_active_by_branch_key(
        &self,
        repo_id: RepoId,
        branch_key_prefix: &str,
    ) -> Result<Vec<EvalRecord>, StoreError>;
    /// IDs of every evaluation whose status isn't terminal. Used at
    /// daemon startup to redrive evaluations the previous instance was
    /// in the middle of when it shut down (or crashed). Order: oldest
    /// first, so the worker drains FIFO.
    async fn list_non_terminal_ids(&self) -> Result<Vec<EvalId>, StoreError>;
}

#[async_trait]
pub trait JobStore: Send + Sync {
    async fn create(&self, new: NewJob) -> Result<JobId, StoreError>;
    async fn get(&self, id: JobId) -> Result<Option<JobRecord>, StoreError>;
    async fn list_by_eval(&self, eval_id: EvalId) -> Result<Vec<JobRecord>, StoreError>;
    async fn set_status(&self, id: JobId, status: JobStatus) -> Result<(), StoreError>;
    /// Mark a job as `Running` and stamp `started_at`.
    async fn start(&self, id: JobId, started_at: DateTime<Utc>) -> Result<(), StoreError>;
    /// Mark a job terminal: set `status`, `finished_at`, and (where present)
    /// the on-disk paths produced by the build pipeline.
    async fn finish(
        &self,
        id: JobId,
        status: JobStatus,
        finished_at: DateTime<Utc>,
        log_path: Option<&str>,
        output_path: Option<&str>,
    ) -> Result<(), StoreError>;
    /// Used at boot (Q79): mark every still-`running` job as `interrupted`.
    /// Returns the number of rows updated. Does NOT touch `interrupt_count`
    /// — boot-time interruption is medusa's fault, not the builder's, and
    /// shouldn't push the job toward the per-job retry cap.
    async fn mark_running_interrupted(&self) -> Result<u64, StoreError>;

    /// Record dispatch of `id` to `builder_id` and stamp `started_at`.
    /// Sets status to `Running` and writes the builder for anti-affinity
    /// tracking on any later re-queue.
    async fn dispatch(
        &self,
        id: JobId,
        builder_id: BuilderId,
        started_at: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    /// Transport-failure recovery (M13 / design/builders.md Q109). Under a
    /// single transaction: increment `interrupt_count`; if the new count is
    /// ≤ `MAX_INTERRUPTIONS`, flip status to `Interrupted`; otherwise flip
    /// to `Failure`, stamp `finished_at`, and write
    /// `failure_reason="exceeded interruption retry limit"`. Returns the
    /// outcome and (when re-queueing) the prior builder so the caller can
    /// build an anti-affinity exclude-set for the next dispatch.
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
    /// enrollment token (see design/builders.md).
    async fn find_active_by_pubkey(
        &self,
        pubkey: &BuilderPubkey,
    ) -> Result<Option<BuilderRecord>, StoreError>;

    /// Touch `last_seen`. Called on every successful heartbeat.
    async fn mark_seen(&self, id: BuilderId, now: DateTime<Utc>) -> Result<(), StoreError>;

    /// `medusactl builders revoke <name>`. Sets `revoked_at`; subsequent
    /// pubkey-auth attempts will fail until the builder re-enrolls with a
    /// fresh token. Returns false if the name is unknown.
    async fn revoke(&self, name: &str, now: DateTime<Utc>) -> Result<bool, StoreError>;

    /// `medusactl builders rename`. Returns false if `old` doesn't exist or
    /// `new` already exists.
    async fn rename(&self, old: &str, new: &str) -> Result<bool, StoreError>;

    /// All builders, oldest enrollment first. Includes revoked rows so
    /// `medusactl builders` can show them.
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
