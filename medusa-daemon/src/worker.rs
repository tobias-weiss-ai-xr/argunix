//! Background evaluator/builder.
//!
//! The webhook handler creates an `Evaluation` row with status=Queued and
//! sends its `EvalId` to the worker via an mpsc channel. The worker:
//!
//! 1. Looks up the eval and its repo,
//! 2. Resolves the forge provider and constructs a clone URL,
//! 3. Shells out to `git` to clone the repo at the recorded SHA into a
//!    temp work dir,
//! 4. Runs the eval pipeline (medusa-eval),
//! 5. Persists each discovered job and runs the build pipeline (medusa-build),
//! 6. Updates the evaluation's terminal status.
//!
//! What's deferred to M5c3/d:
//! - cancel-on-new-push,
//! - PR permission/allowlist enforcement,
//! - merge-ref retry for fork PRs,
//! - per-job vs collapsed check threshold (M5c3 ships per-job for any size).

use anyhow::{Context, anyhow};
use chrono::Utc;
use medusa_config::Config;
use medusa_domain::{EvalId, EvalStatus, JobId, JobStatus, RepoId, Sha, Slug};
use medusa_forge::{CheckPost, CheckState, Provider};
use medusa_store::{EvalStore, JobStore, RepoStore, SqlxStore};
use medusa_web::{eval_target_url, job_target_url};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{Instrument, info_span};

/// State the worker needs to process evaluations end-to-end.
#[derive(Clone)]
pub struct WorkerContext {
    pub config: Arc<Config>,
    pub providers: Arc<HashMap<String, Arc<dyn Provider>>>,
    pub store: SqlxStore,
    pub work_dir: PathBuf,
    pub log_dir: PathBuf,
    pub gc_root_dir: PathBuf,
    pub eval_timeout: Duration,
    pub build_timeout: Duration,
    pub clone_timeout: Duration,
    pub systems: Vec<String>,
}

/// Spawn the worker on the current tokio runtime. Returns a `JoinHandle`
/// that resolves when the channel is closed and the last in-flight
/// evaluation finishes.
pub fn spawn(
    ctx: WorkerContext,
    mut rx: mpsc::UnboundedReceiver<EvalId>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(eval_id) = rx.recv().await {
            let span = info_span!("evaluation", eval_id = eval_id.get());
            if let Err(e) = process(&ctx, eval_id).instrument(span.clone()).await {
                let _enter = span.enter();
                tracing::error!(error = %e, "evaluation failed in worker");
                let _ = <SqlxStore as EvalStore>::finish(
                    &ctx.store,
                    eval_id,
                    EvalStatus::EvaluationFailed,
                    Utc::now(),
                )
                .await;
            }
        }
        tracing::info!("worker channel closed; exiting");
    })
}

async fn process(ctx: &WorkerContext, eval_id: EvalId) -> anyhow::Result<()> {
    tracing::info!("worker picked up evaluation");

    let eval = <SqlxStore as EvalStore>::get(&ctx.store, eval_id)
        .await?
        .ok_or_else(|| anyhow!("evaluation row {} disappeared", eval_id.get()))?;
    let repo = <SqlxStore as RepoStore>::get(&ctx.store, eval.repo_id)
        .await?
        .ok_or_else(|| anyhow!("repo row {} disappeared", eval.repo_id.get()))?;
    let provider = ctx
        .providers
        .get(&repo.forge)
        .ok_or_else(|| anyhow!("no provider for forge `{}`", repo.forge))?;

    <SqlxStore as EvalStore>::set_status(&ctx.store, eval_id, EvalStatus::Evaluating).await?;

    let work_dir = ctx.work_dir.join(eval_id.get().to_string());
    if work_dir.exists() {
        tokio::fs::remove_dir_all(&work_dir)
            .await
            .with_context(|| format!("clearing stale workdir {}", work_dir.display()))?;
    }

    let clone_url = provider.clone_url(&repo.slug);
    clone_repo(&clone_url, &eval.sha, &work_dir, ctx.clone_timeout)
        .await
        .with_context(|| format!("cloning {} at {}", repo.slug, eval.sha))?;

    let request = medusa_eval::EvalRequest {
        source_path: work_dir.clone(),
        systems: ctx.systems.clone(),
        outputs: medusa_eval::DEFAULT_FLAKE_OUTPUTS
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        timeout: ctx.eval_timeout,
    };
    let jobs = match medusa_eval::evaluate(&request).await {
        Ok(jobs) => jobs,
        Err(e) => {
            <SqlxStore as EvalStore>::finish(
                &ctx.store,
                eval_id,
                EvalStatus::EvaluationFailed,
                Utc::now(),
            )
            .await?;
            // Q52: surface eval-time failure as a single failed forge check.
            post_overall_check(
                ctx,
                provider,
                &repo.forge,
                &repo.slug,
                &eval.sha,
                eval_id,
                CheckState::Failure,
                "evaluation failed",
            );
            return Err(anyhow::Error::from(e).context("evaluation failed"));
        }
    };
    <SqlxStore as EvalStore>::set_status(&ctx.store, eval_id, EvalStatus::Building).await?;
    tracing::info!(count = jobs.len(), "evaluation finished");

    let caches: Vec<medusa_build::CacheRef> = ctx
        .config
        .binary_caches
        .iter()
        .map(|c| medusa_build::CacheRef {
            url: c.url.clone(),
            substitute: c.substitute,
        })
        .collect();

    let mut tally = JobTally::default();
    for spec in jobs {
        let job_id = persist_job(&ctx.store, eval_id, &spec).await?;
        let outcome = build_one(ctx, repo.id, eval_id, job_id, &spec, &caches).await;
        let final_status = match outcome {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, attr = %spec.attr_path, "build pipeline error");
                JobStatus::Failure
            }
        };
        tally.record(final_status);
        post_per_job_check(
            ctx,
            provider,
            &repo.forge,
            &repo.slug,
            &eval.sha,
            eval_id,
            &spec.attr_path.as_str().to_string(),
            final_status,
        );
    }

    <SqlxStore as EvalStore>::finish(&ctx.store, eval_id, EvalStatus::Done, Utc::now()).await?;
    tracing::info!(
        success = tally.success,
        cached = tally.cached,
        failure = tally.failure,
        "evaluation finished",
    );

    let overall_state = if tally.failure > 0 {
        CheckState::Failure
    } else {
        CheckState::Success
    };
    let description = format!(
        "{} ok, {} cached, {} failed",
        tally.success, tally.cached, tally.failure,
    );
    post_overall_check(
        ctx,
        provider,
        &repo.forge,
        &repo.slug,
        &eval.sha,
        eval_id,
        overall_state,
        &description,
    );

    if let Err(e) = tokio::fs::remove_dir_all(&work_dir).await {
        tracing::warn!(error = %e, dir = %work_dir.display(), "failed to clean workdir");
    }
    Ok(())
}

#[derive(Default)]
struct JobTally {
    success: usize,
    cached: usize,
    failure: usize,
}

impl JobTally {
    fn record(&mut self, status: JobStatus) {
        match status {
            JobStatus::Success => self.success += 1,
            JobStatus::Cached => self.cached += 1,
            JobStatus::Failure => self.failure += 1,
            _ => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn post_per_job_check(
    ctx: &WorkerContext,
    provider: &Arc<dyn Provider>,
    forge: &str,
    slug: &Slug,
    sha: &Sha,
    eval_id: EvalId,
    attr_path: &str,
    status: JobStatus,
) {
    let state = match status {
        JobStatus::Success | JobStatus::Cached => CheckState::Success,
        JobStatus::Failure => CheckState::Failure,
        JobStatus::Cancelled | JobStatus::Interrupted => CheckState::Error,
        JobStatus::SkippedNoBuilder => return,
        JobStatus::Queued | JobStatus::Running => return,
    };
    let target = job_target_url(&ctx.config.external_url, forge, slug, eval_id, attr_path);
    let post = CheckPost {
        slug: slug.clone(),
        sha: sha.clone(),
        context: format!("medusa: {attr_path}"),
        state,
        description: Some(match status {
            JobStatus::Cached => "cache hit".to_string(),
            JobStatus::Success => "build ok".to_string(),
            JobStatus::Failure => "build failed".to_string(),
            _ => "build error".to_string(),
        }),
        target_url: Some(target),
    };
    spawn_post_check(provider.clone(), post);
}

#[allow(clippy::too_many_arguments)]
fn post_overall_check(
    ctx: &WorkerContext,
    provider: &Arc<dyn Provider>,
    forge: &str,
    slug: &Slug,
    sha: &Sha,
    eval_id: EvalId,
    state: CheckState,
    description: &str,
) {
    let target = eval_target_url(&ctx.config.external_url, forge, slug, eval_id);
    let post = CheckPost {
        slug: slug.clone(),
        sha: sha.clone(),
        context: "medusa: evaluation".to_string(),
        state,
        description: Some(description.to_string()),
        target_url: Some(target),
    };
    spawn_post_check(provider.clone(), post);
}

fn spawn_post_check(provider: Arc<dyn Provider>, post: CheckPost) {
    tokio::spawn(async move {
        if let Err(e) = provider.post_check(post).await {
            tracing::warn!(error = %e, "forge post_check failed");
        }
    });
}

async fn persist_job(
    store: &SqlxStore,
    eval_id: EvalId,
    spec: &medusa_eval::JobSpec,
) -> anyhow::Result<JobId> {
    let job_id = <SqlxStore as JobStore>::create(
        store,
        medusa_store::NewJob {
            eval_id,
            attr_path: spec.attr_path.clone(),
            drv_path: spec.drv_path.clone(),
            system: spec.system.clone().unwrap_or_else(|| "unknown".to_string()),
        },
    )
    .await?;
    if spec.error.is_some() {
        <SqlxStore as JobStore>::finish(store, job_id, JobStatus::Failure, Utc::now(), None, None)
            .await?;
    }
    Ok(job_id)
}

#[allow(clippy::too_many_arguments)]
async fn build_one(
    ctx: &WorkerContext,
    repo_id: RepoId,
    eval_id: EvalId,
    job_id: JobId,
    spec: &medusa_eval::JobSpec,
    caches: &[medusa_build::CacheRef],
) -> anyhow::Result<JobStatus> {
    if spec.error.is_some() {
        return Ok(JobStatus::Failure);
    }
    let Some(drv_path) = spec.drv_path.clone() else {
        return Ok(JobStatus::Failure);
    };

    if let Some(output) = spec.primary_output() {
        match medusa_build::check_cache(output, caches, Duration::from_secs(30)).await {
            Ok(medusa_build::CacheCheckResult::Hit { cache_url }) => {
                tracing::info!(job_id = job_id.get(), cache = %cache_url, "cache hit");
                <SqlxStore as JobStore>::finish(
                    &ctx.store,
                    job_id,
                    JobStatus::Cached,
                    Utc::now(),
                    None,
                    Some(output),
                )
                .await?;
                return Ok(JobStatus::Cached);
            }
            Ok(medusa_build::CacheCheckResult::Miss) => {}
            Err(e) => tracing::warn!(error = %e, "cache check failed; falling through to build"),
        }
    }

    <SqlxStore as JobStore>::start(&ctx.store, job_id, Utc::now()).await?;

    let log_path = ctx
        .log_dir
        .join(repo_id.get().to_string())
        .join(eval_id.get().to_string())
        .join(format!("{}.log.zst", job_id.get()));
    let request = medusa_build::BuildRequest {
        drv_path: drv_path.clone(),
        log_path: log_path.clone(),
        timeout: ctx.build_timeout,
        log_limit: medusa_build::LogCaptureLimit::default(),
    };
    let outcome = medusa_build::run_build(&request).await?;
    let log_path_str = log_path.to_string_lossy().into_owned();

    match outcome.status {
        medusa_build::BuildStatus::Success => {
            let primary = outcome
                .output_paths
                .first()
                .cloned()
                .or_else(|| spec.primary_output().map(String::from));
            if let Some(out) = primary.as_deref() {
                let root = medusa_build::gc_root_path(&ctx.gc_root_dir, repo_id, eval_id, job_id);
                if let Err(e) = medusa_build::add_gc_root(&root, out).await {
                    tracing::warn!(error = %e, "gc-root creation failed");
                }
            }
            <SqlxStore as JobStore>::finish(
                &ctx.store,
                job_id,
                JobStatus::Success,
                Utc::now(),
                Some(&log_path_str),
                primary.as_deref(),
            )
            .await?;
            Ok(JobStatus::Success)
        }
        medusa_build::BuildStatus::Failure => {
            <SqlxStore as JobStore>::finish(
                &ctx.store,
                job_id,
                JobStatus::Failure,
                Utc::now(),
                Some(&log_path_str),
                None,
            )
            .await?;
            Ok(JobStatus::Failure)
        }
    }
}

async fn clone_repo(
    url: &str,
    sha: &medusa_domain::Sha,
    dst: &Path,
    timeout: Duration,
) -> anyhow::Result<()> {
    if let Some(parent) = dst.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }

    run_git(
        timeout,
        &["clone", "--filter=blob:none", url, &dst.to_string_lossy()],
    )
    .await?;
    run_git_in(
        timeout,
        dst,
        &["fetch", "--depth=1", "origin", sha.as_str()],
    )
    .await?;
    run_git_in(timeout, dst, &["checkout", sha.as_str()]).await?;
    Ok(())
}

async fn run_git(timeout: Duration, args: &[&str]) -> anyhow::Result<()> {
    run_git_with_optional_cwd(timeout, None, args).await
}

async fn run_git_in(timeout: Duration, cwd: &Path, args: &[&str]) -> anyhow::Result<()> {
    run_git_with_optional_cwd(timeout, Some(cwd), args).await
}

async fn run_git_with_optional_cwd(
    timeout: Duration,
    cwd: Option<&Path>,
    args: &[&str],
) -> anyhow::Result<()> {
    let mut cmd = Command::new("git");
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().context("spawning git")?;
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(o) => o.context("waiting for git")?,
        Err(_) => return Err(anyhow!("git timed out after {}s", timeout.as_secs())),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "git {:?} failed with status {:?}: {}",
            args,
            output.status.code(),
            stderr.trim(),
        ));
    }
    Ok(())
}
