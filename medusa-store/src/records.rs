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
