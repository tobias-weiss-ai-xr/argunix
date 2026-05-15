use argunix_domain::{
    AttrPath, BuilderCapabilities, BuilderId, BuilderName, BuilderPubkey, EvalId, EvalStatus,
    JobId, JobStatus, RepoId, Sha, Slug,
};
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;

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
    /// Forge-reported default branch (`main`, `master`, …). Populated
    /// from `repository.default_branch` (GitHub / Forgejo) or
    /// `project.default_branch` (GitLab) on every webhook payload —
    /// these forges always include it. `None` until the first matching
    /// webhook lands; the badge endpoint falls back to "any branch"
    /// then.
    pub default_branch: Option<String>,
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
    /// `meta.mainProgram` from `nix-eval-jobs --meta`. The executable's
    /// basename inside `<output>/bin/`. `None` when the derivation didn't
    /// declare one; the synthetic-flake endpoint then omits the
    /// `apps.<system>.<attr>` entry for that job.
    pub main_program: Option<String>,
    /// Output-name → store-path map as reported by nix-eval-jobs, e.g.
    /// `{"out": "/nix/store/zzz-foo", "dev": "/nix/store/yyy-foo-dev"}`.
    /// Persisted so the synthetic-flake endpoint can pick the right
    /// output without a second eval pass. Empty for rows that didn't
    /// produce outputs (eval errors).
    pub outputs: BTreeMap<String, String>,
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
    /// job has never been dispatched (still queued). The dispatcher reads
    /// this on re-queue to set anti-affinity.
    pub builder_id: Option<BuilderId>,
    /// How many times this job has been interrupted by transport drop /
    /// graceful builder shutdown. Capped by `MAX_INTERRUPTIONS`; on
    /// exceedance the job flips to `Failure` with `failure_reason` set.
    pub interrupt_count: u32,
    /// Set when a job fails for a non-build-process reason (currently only
    /// "exceeded interruption retry limit"). NULL for build-level failures.
    pub failure_reason: Option<String>,
    /// Per-phase accounting for pool-dispatched builds. All `None`
    /// for jobs that were never dispatched or built locally.
    pub phase_metrics: JobPhaseMetrics,
    /// `meta.mainProgram` captured at eval time. See [`NewJob::main_program`].
    pub main_program: Option<String>,
    /// Full output-name → store-path map captured at eval time. See
    /// [`NewJob::outputs`]. Empty on rows that pre-date the
    /// `outputs_json` column or that errored before nix-eval-jobs
    /// reported outputs.
    pub outputs: BTreeMap<String, String>,
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
    /// Wall-clock of the post-success `nix copy --to <binary_cache>`
    /// publish, summed across every configured cache. Written by a
    /// detached background task that fires after `finish` — so the
    /// value lands on the row a few seconds to several minutes after
    /// `finished_at`. `None` for jobs with no `binary_caches`
    /// configured, for failures (no push attempted), and for any job
    /// where the daemon was shut down mid-push.
    pub cache_push_ms: Option<u64>,
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

/// Fields supplied when registering a converted docker image. Inserted
/// after a successful build of an attribute flagged with
/// `meta.docker-image == true` and a successful skopeo conversion of
/// the docker-archive tarball into the registry blob pool.
#[derive(Debug, Clone)]
pub struct NewDockerImage {
    pub repo_id: RepoId,
    pub eval_id: EvalId,
    pub job_id: JobId,
    /// Forge-prefixed image name without the host (`<forge>/<owner>/<repo>/<attr>`).
    pub image_name: String,
    /// Nix system tuple (`x86_64-linux`, `aarch64-linux`).
    pub system: String,
    pub git_ref: String,
    pub sha: Sha,
    /// `sha256:<hex>` of the converted manifest.json bytes.
    pub manifest_digest: String,
    /// Absolute path to the on-disk manifest.json in the registry pool.
    pub manifest_path: String,
}

/// One row of `docker_images`. Returned by registry lookups so the
/// HTTP layer can serve manifests / blobs from disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerImageRecord {
    pub repo_id: RepoId,
    pub eval_id: EvalId,
    pub job_id: JobId,
    pub image_name: String,
    pub system: String,
    pub git_ref: String,
    pub sha: Sha,
    pub manifest_digest: String,
    pub manifest_path: String,
    pub created_at: DateTime<Utc>,
}
