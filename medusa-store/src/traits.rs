use crate::records::{EvalRecord, ForgeStatusRecord, JobRecord, NewEvaluation, NewJob, RepoRecord};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use medusa_domain::{EvalId, EvalStatus, JobId, JobStatus, RepoId, Slug};

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
}

#[async_trait]
pub trait RepoStore: Send + Sync {
    async fn upsert(&self, forge: &str, slug: &Slug) -> Result<RepoId, StoreError>;
    async fn get(&self, id: RepoId) -> Result<Option<RepoRecord>, StoreError>;
    async fn find(&self, forge: &str, slug: &Slug) -> Result<Option<RepoRecord>, StoreError>;
    async fn list(&self) -> Result<Vec<RepoRecord>, StoreError>;
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
    /// Returns the number of rows updated.
    async fn mark_running_interrupted(&self) -> Result<u64, StoreError>;
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
