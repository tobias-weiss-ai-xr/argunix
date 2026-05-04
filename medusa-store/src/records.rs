use chrono::{DateTime, Utc};
use medusa_domain::{
    AttrPath, BuilderCapabilities, BuilderId, BuilderName, BuilderPubkey, EvalId, EvalStatus,
    JobId, JobStatus, RepoId, Sha, Slug,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRecord {
    pub id: RepoId,
    pub forge: String,
    pub slug: Slug,
}

/// Fields supplied when creating a new evaluation row.
#[derive(Debug, Clone)]
pub struct NewEvaluation {
    pub repo_id: RepoId,
    pub trigger: String,
    pub git_ref: String,
    pub sha: Sha,
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
/// successful `hello`; `revoked_at` is set by `medusactl builders revoke`
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
