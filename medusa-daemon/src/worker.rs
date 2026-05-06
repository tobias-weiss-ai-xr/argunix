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
//! - merge-ref retry for fork PRs,
//! - per-job vs collapsed check threshold (M5c3 ships per-job for any size).
//!
//! PR permission/allowlist and watched-branches gating happen earlier in
//! the pipeline — see `medusa_web::policy` — so by the time the worker
//! picks an evaluation up, it's already been authorised.

use anyhow::{Context, anyhow};
use arc_swap::ArcSwap;
use chrono::Utc;
use medusa_builders::{
    BuildLifecycle, BuildOutcomeStatus, BuilderDispatcher, ClosureXferError,
    pull_closure_over_channel, push_closure_over_channel, query_invalid_over_channel,
    query_requisites,
};
use medusa_domain::{EvalId, EvalStatus, JobId, JobStatus, RepoId, Sha, Slug};
use medusa_forge::{CheckPost, CheckState, ForgeError, Provider};
use medusa_store::{EvalStore, JobStore, RepoStore, SqlxStore};
use medusa_web::{CancelRegistry, ConfigSnapshot, PauseRegistry, eval_target_url, job_target_url};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{Instrument, info_span};

/// RAII guard that reserves an `in_flight` slot on a specific builder
/// for the lifetime of one dispatched derivation (M14). Increments
/// on construction; decrements on drop (including when the build
/// future is dropped due to cancellation).
struct BuilderSlot {
    registry: Arc<medusa_builders::BuilderRegistry>,
    name: medusa_domain::BuilderName,
}

impl BuilderSlot {
    fn reserve(
        registry: Arc<medusa_builders::BuilderRegistry>,
        name: medusa_domain::BuilderName,
    ) -> Self {
        registry.inc_in_flight(&name);
        Self { registry, name }
    }
}

impl Drop for BuilderSlot {
    fn drop(&mut self) {
        self.registry.dec_in_flight(&self.name);
    }
}

/// State the worker needs to process evaluations end-to-end.
#[derive(Clone)]
pub struct WorkerContext {
    /// Atomically-swappable bundle of config + providers. The worker
    /// snapshots this once at the top of every evaluation so a
    /// reload mid-eval never produces inconsistent state.
    pub current: Arc<ArcSwap<ConfigSnapshot>>,
    pub store: SqlxStore,
    pub work_dir: PathBuf,
    pub log_dir: PathBuf,
    pub gc_root_dir: PathBuf,
    pub eval_timeout: Duration,
    pub build_timeout: Duration,
    pub clone_timeout: Duration,
    pub systems: Vec<String>,
    pub pauses: Arc<PauseRegistry>,
    pub cancellations: Arc<CancelRegistry>,
    /// Shared registry of currently-connected builders. The worker
    /// picks one per derivation via `pick_builder_for_spec`; on a
    /// match, dispatch goes through M14b side channels (push closure
    /// → `Build` control message → drain lifecycle → pull outputs).
    /// On no match, the worker falls back to a local `nix-store
    /// --realise` (which itself may use the host's `nix.buildMachines`).
    pub builder_registry: Arc<medusa_builders::BuilderRegistry>,
    /// Path to the local `nix-store` binary. Used by the M14b
    /// side-channel transport to compute drv closures
    /// (`nix-store --query --requisites`), export drv closures to
    /// the agent, and import the agent's output closure back into
    /// the daemon's local store. Tests inject a fake binary; in
    /// production, `PathBuf::from("nix-store")` resolves on PATH.
    pub nix_store_bin: PathBuf,
    /// Maximum number of derivations to build in parallel across the
    /// whole evaluation (M14). Per-builder concurrency is additionally
    /// gated by each builder's advertised `max_jobs`. Defaults to 16
    /// in `main.rs`; clamped to ≥1 at use.
    pub build_concurrency: usize,
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
                // `{:#}` is anyhow's "alternate" Display, which walks the
                // entire context chain on a single line — we lose the
                // backtrace but get the cause. Without this you only see
                // the topmost `.context("…")` and the actual root failure
                // (a nix-eval-jobs stderr, a git error, …) is invisible.
                tracing::error!(error = %format!("{e:#}"), "evaluation failed in worker");
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

    // Q39: register a cancel token *before* checking the DB row, so a
    // cancel that arrives after this point but before we start work is
    // captured. If the DB already says Cancelled (cancel arrived before
    // the worker picked it up), bail without doing anything.
    let cancel = ctx.cancellations.register(eval_id);
    let _guard = CancelGuard {
        registry: &ctx.cancellations,
        eval_id,
    };

    // Snapshot the swappable bundle for this evaluation. A reload that
    // lands while we're mid-eval will swap the daemon's pointer but
    // this snapshot remains valid — we finish on the config we
    // started with (Q22 / Q70).
    let snap = ctx.current.load_full();

    let eval = <SqlxStore as EvalStore>::get(&ctx.store, eval_id)
        .await?
        .ok_or_else(|| anyhow!("evaluation row {} disappeared", eval_id.get()))?;

    if eval.status == EvalStatus::Cancelled || cancel.is_cancelled() {
        tracing::info!("evaluation cancelled before worker pickup; skipping",);
        return Ok(());
    }

    let repo = <SqlxStore as RepoStore>::get(&ctx.store, eval.repo_id)
        .await?
        .ok_or_else(|| anyhow!("repo row {} disappeared", eval.repo_id.get()))?;
    let provider = snap
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
    let clone_fut = clone_repo(&clone_url, &eval.sha, &work_dir, ctx.clone_timeout);
    tokio::select! {
        biased;
        r = clone_fut => r.with_context(|| format!("cloning {} at {}", repo.slug, eval.sha))?,
        _ = cancel.cancelled() => {
            tracing::info!("evaluation cancelled during clone (Q39)");
            <SqlxStore as EvalStore>::finish(
                &ctx.store, eval_id, EvalStatus::Cancelled, Utc::now()
            ).await?;
            return Ok(());
        }
    };

    if cancel.is_cancelled() {
        tracing::info!("evaluation cancelled before eval phase (Q39)");
        <SqlxStore as EvalStore>::finish(&ctx.store, eval_id, EvalStatus::Cancelled, Utc::now())
            .await?;
        return Ok(());
    }

    let request = medusa_eval::EvalRequest {
        source_path: work_dir.clone(),
        systems: ctx.systems.clone(),
        outputs: medusa_eval::DEFAULT_FLAKE_OUTPUTS
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        timeout: ctx.eval_timeout,
    };
    let jobs = tokio::select! {
        biased;
        res = medusa_eval::evaluate(&request) => res,
        _ = cancel.cancelled() => {
            tracing::info!("evaluation cancelled during nix-eval-jobs (Q39)");
            <SqlxStore as EvalStore>::finish(
                &ctx.store, eval_id, EvalStatus::Cancelled, Utc::now()
            ).await?;
            return Ok(());
        }
    };
    let jobs = match jobs {
        Ok(jobs) => jobs,
        Err(e) => {
            <SqlxStore as EvalStore>::finish(
                &ctx.store,
                eval_id,
                EvalStatus::EvaluationFailed,
                Utc::now(),
            )
            .await?;
            // Q52: surface eval-time failure as a single failed forge
            // check. Github's status `description` field is capped at 140
            // chars, so we truncate the (often multi-line) nix-eval-jobs
            // error before posting; the full chain still goes to the
            // daemon log via the worker's outer error trap.
            let detail = summarise_for_check(&e.to_string(), 130);
            post_overall_check(
                ctx,
                provider,
                &repo.forge,
                &repo.slug,
                &eval.sha,
                eval_id,
                CheckState::Failure,
                &format!("evaluation failed: {detail}"),
            );
            return Err(anyhow::Error::from(e).context("evaluation failed"));
        }
    };
    <SqlxStore as EvalStore>::set_status(&ctx.store, eval_id, EvalStatus::Building).await?;
    tracing::info!(count = jobs.len(), "evaluation finished");

    let caches: Vec<medusa_build::CacheRef> = snap
        .config
        .binary_caches
        .iter()
        .map(|c| medusa_build::CacheRef {
            url: c.url.clone(),
            substitute: c.substitute,
        })
        .collect();

    // Q5/Q19: above the threshold we collapse per-job checks into a
    // single rolling `medusa: evaluation` status whose description is
    // updated as jobs finish. PAT path (commit statuses) caps the
    // description at 140 chars — no markdown bullets — so the full
    // job list lives in medusa's UI, reachable via the status's
    // target_url. Markdown summaries land with GitHub-App / Checks
    // API in M5c.
    let repo_cfg = snap
        .config
        .repos
        .iter()
        .find(|r| r.forge == repo.forge && r.slug == repo.slug);
    let threshold = repo_cfg
        .and_then(|r| r.collapsed_check_threshold)
        .unwrap_or(snap.config.schedule.collapsed_check_threshold);
    let total = jobs.len();
    let collapsed_mode = total as u32 > threshold;
    if collapsed_mode {
        tracing::info!(
            jobs = total,
            threshold,
            "collapsed check mode active; per-job statuses suppressed (Q19)",
        );
    }

    // Persist every job spec to the DB *before* starting the build
    // loop. Without this, the read-only UI's job table grows row by
    // row as the worker iterates, and a user looking at an in-flight
    // evaluation can't tell whether the rows currently shown are the
    // final list or whether more are still to come. With upfront
    // persistence, the table reflects the final shape as soon as the
    // eval transitions to `Building`, and the eval's status field is
    // the single source of truth for "is anything still pending?".
    let mut persisted: Vec<(medusa_eval::JobSpec, JobId)> = Vec::with_capacity(jobs.len());
    for spec in jobs {
        let job_id = persist_job(&ctx.store, &ctx.log_dir, repo.id, eval_id, &spec).await?;
        persisted.push((spec, job_id));
    }

    // Replace the initial Q51 "evaluating…" overall check with a
    // "building N jobs" pending update. Without this, the GitHub /
    // GitLab / Forgejo UI shows "evaluating…" the entire time builds
    // are running, which is misleading once eval is actually done.
    // In collapsed_mode the rolling tally updates further refine
    // this; here we just ensure there's at least one transition.
    post_overall_check(
        ctx,
        provider,
        &repo.forge,
        &repo.slug,
        &eval.sha,
        eval_id,
        CheckState::Pending,
        &format!("building {total} jobs"),
    );

    // Post pending per-job checks upfront so the user sees the full
    // matrix of `medusa: <attr>` rows on the commit page immediately,
    // each in the "queued" state, rather than rows blinking into
    // existence one by one as builds finish. Skipped in collapsed
    // mode where per-job checks are entirely suppressed.
    if !collapsed_mode {
        for (spec, _) in &persisted {
            post_per_job_check_pending(
                ctx,
                provider,
                &repo.forge,
                &repo.slug,
                &eval.sha,
                eval_id,
                spec.attr_path.as_str(),
            );
        }
    }

    let mut tally = JobTally::default();
    let summary_debounce = std::time::Duration::from_secs(2);
    let mut last_summary_post: Option<std::time::Instant> = None;

    // M14: parallelise the per-eval build loop. Up to
    // `ctx.build_concurrency` derivations build in parallel; per-builder
    // capacity is gated separately by each builder's `max_jobs` (read
    // inside `pick_builder_for_spec` via `BuilderRegistry::eligible`).
    // When the pool is saturated, `pick_builder_for_spec` returns None
    // and `build_one` falls back to a multi-builder `--builders`
    // snapshot, so over-cap dispatches don't deadlock — they go
    // through nix's own scheduler instead.
    let global_sem = Arc::new(tokio::sync::Semaphore::new(ctx.build_concurrency.max(1)));
    let mut set: tokio::task::JoinSet<(JobId, medusa_eval::JobSpec, anyhow::Result<JobStatus>)> =
        tokio::task::JoinSet::new();
    let mut work_iter = persisted.into_iter();
    let mut work_drained = false;

    'outer: loop {
        // Spawn while we have permits and still have jobs to dispatch.
        if !work_drained && !cancel.is_cancelled() {
            loop {
                let permit = match global_sem.clone().try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => break, // No global permits free.
                };
                let Some((spec, job_id)) = work_iter.next() else {
                    drop(permit);
                    work_drained = true;
                    break;
                };
                let ctx_c = ctx.clone();
                let cancel_c = cancel.clone();
                let caches_c = caches.clone();
                let repo_id = repo.id;
                let span = info_span!(
                    "job",
                    job_id = job_id.get(),
                    attr = %spec.attr_path,
                );
                set.spawn(async move {
                    let _permit = permit; // released on drop
                    let res = build_one(
                        &ctx_c, repo_id, eval_id, job_id, &spec, &caches_c, &cancel_c,
                    )
                    .instrument(span)
                    .await;
                    (job_id, spec, res)
                });
            }
        }

        // Termination: nothing in flight and nothing left to dispatch.
        if work_drained && set.is_empty() {
            break 'outer;
        }
        // Cancel arrived but nothing in flight either — fall through
        // to the cancelled-finish below.
        if cancel.is_cancelled() && set.is_empty() {
            break 'outer;
        }

        // Wait for either a build to finish or a cancel signal. On
        // cancel, do NOT `abort_all` the in-flight builds: each
        // spawned task holds a clone of the same `cancel` token,
        // and `dispatch_pool_build` has its own cancel arm that
        // sends `Abort` to the builder and drains the resulting
        // `BuildFinished{Killed}`. `JoinSet::abort_all` would
        // forcibly drop those futures before they ever delivered
        // `Abort`, leaving the agent's `nix-store --realise`
        // running to completion on the builder host (observed
        // symptom: jobset shows Cancelled in the UI but builders
        // keep building). The local fallback path's
        // `Command::kill_on_drop(true)` is also reachable through
        // the same cancel token (its own select! arm at line
        // ~1045), so a graceful drain is correct for both branches.
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                tracing::info!(
                    in_flight = set.len(),
                    remaining_done = tally.success + tally.cached + tally.failure,
                    "evaluation cancelled mid-build-loop (Q39); awaiting graceful shutdown of in-flight builds",
                );
                // Drain naturally — each in-flight build observes
                // the cancel token through its own future and
                // returns `JobStatus::Cancelled` after Abort + drain.
                // The per-build wall-clock timeout bounds how long
                // a wedged agent can keep us here.
                while set.join_next().await.is_some() {}
                <SqlxStore as EvalStore>::finish(
                    &ctx.store,
                    eval_id,
                    EvalStatus::Cancelled,
                    Utc::now(),
                )
                .await?;
                return Ok(());
            }
            r = set.join_next() => r,
        };
        let Some(joined) = next else {
            continue;
        };
        let (_job_id, spec, outcome) = match joined {
            Ok(t) => t,
            Err(join_err) => {
                // A spawned task panicked or was cancelled; treat as
                // a pipeline error so the overall eval still finishes
                // with a meaningful tally.
                tracing::error!(error = %join_err, "build task panicked");
                tally.record(JobStatus::Failure);
                continue;
            }
        };
        let final_status = match outcome {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, attr = %spec.attr_path, "build pipeline error");
                JobStatus::Failure
            }
        };
        tally.record(final_status);
        if collapsed_mode {
            // Q69: debounce. Only post a summary update if 2s elapsed
            // since the last one. The unconditional final post after
            // the loop will catch any tail tally that the debounce
            // dropped.
            let elapsed_ok = match last_summary_post {
                None => true,
                Some(t) => t.elapsed() >= summary_debounce,
            };
            if elapsed_ok {
                let desc = collapsed_progress(&tally, total);
                post_overall_check(
                    ctx,
                    provider,
                    &repo.forge,
                    &repo.slug,
                    &eval.sha,
                    eval_id,
                    CheckState::Pending,
                    &desc,
                );
                last_summary_post = Some(std::time::Instant::now());
            }
        } else {
            post_per_job_check(
                ctx,
                provider,
                &repo.forge,
                &repo.slug,
                &eval.sha,
                eval_id,
                &spec.attr_path.as_str().to_string(),
                final_status,
                spec.error.is_some(),
            );
        }
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

/// Q19/Q102: in-progress description for the rolling collapsed check.
/// GitHub commit-status descriptions are capped at 140 chars; this is
/// generously inside that.
fn collapsed_progress(tally: &JobTally, total: usize) -> String {
    let done = tally.success + tally.cached + tally.failure;
    format!(
        "{done}/{total} done — {ok} ok, {cached} cached, {failed} failed",
        done = done,
        total = total,
        ok = tally.success,
        cached = tally.cached,
        failed = tally.failure,
    )
}

/// Pull a forge-check-friendly one-liner out of a (potentially multi-line)
/// error message: take the first non-empty line, strip whitespace, and
/// hard-cap at `max_chars` characters with an ellipsis if needed.
fn summarise_for_check(err: &str, max_chars: usize) -> String {
    let first = err
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if first.chars().count() <= max_chars {
        first.to_string()
    } else {
        let mut out: String = first.chars().take(max_chars - 1).collect();
        out.push('…');
        out
    }
}

/// Pick the builder this derivation should run on (M14). Walks
/// `BuilderRegistry::eligible(system, required_features, exclude={})`
/// and takes the first entry — which `eligible()` already sorts
/// least-loaded-first. Returns `None` when:
///
/// - the spec has no `system` (we can't filter and shouldn't guess);
/// - no connected builder advertises the system *and* every required
///   feature *and* has free `max_jobs` capacity right now.
///
/// In the second case the caller falls through to a local
/// `nix-store --realise` (no `--builders`), which honours the host's
/// `nix.buildMachines` if any. The pre-flight earlier in `build_one`
/// has already failed-fast for the unsatisfiable-features subset.
fn pick_builder_for_spec(
    registry: &medusa_builders::BuilderRegistry,
    spec: &medusa_eval::JobSpec,
) -> Option<medusa_builders::BuilderSnapshot> {
    let system = spec.system.as_deref()?;
    let eligible = registry.eligible(
        system,
        &spec.required_system_features,
        &std::collections::HashSet::new(),
    );
    eligible.into_iter().next()
}

/// Build the synthetic stderr written into the job's log file when
/// the pre-flight rejects the build. Mirrors the shape of nix's own
/// "Failed to find a machine for remote build!" message so an
/// operator scanning the UI doesn't have to learn a second format.
fn synthesize_no_eligible_builder_log(
    attr: &str,
    drv: &str,
    system: &str,
    required: &[String],
    registry: &medusa_builders::BuilderRegistry,
) -> String {
    let mut out = String::new();
    out.push_str("medusa pre-flight: no connected builder satisfies this derivation's\n");
    out.push_str("requiredSystemFeatures. Failing fast instead of waiting for nix's\n");
    out.push_str("remote-build scheduler to give up.\n\n");
    out.push_str(&format!("attribute: {attr}\n"));
    out.push_str(&format!("derivation: {drv}\n"));
    out.push_str(&format!(
        "required (system, features): ({system}, {required:?})\n\n"
    ));
    let snapshots = registry.list();
    if snapshots.is_empty() {
        out.push_str("connected builders: (none)\n");
    } else {
        out.push_str("connected builders:\n");
        for b in snapshots {
            out.push_str(&format!(
                "  - {name}: systems={systems:?} features={features:?} max_jobs={max} state={state:?}\n",
                name = b.name,
                systems = b.capabilities.systems,
                features = b.capabilities.features,
                max = b.capabilities.max_jobs,
                state = b.state,
            ));
        }
    }
    out.push_str("\nFix one of:\n");
    out.push_str("  1. Add the missing feature(s) to a builder's nix.settings.system-features.\n");
    out.push_str("  2. Add a builder host that natively satisfies the feature.\n");
    out.push_str("  3. Drop the requirement from the derivation if it isn't actually needed.\n");
    out
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

/// Post the initial Pending check for a per-job context, fired upfront
/// so the user sees "queued" on the commit page immediately rather
/// than rows appearing one by one as builds finish.
#[allow(clippy::too_many_arguments)]
fn post_per_job_check_pending(
    ctx: &WorkerContext,
    provider: &Arc<dyn Provider>,
    forge: &str,
    slug: &Slug,
    sha: &Sha,
    eval_id: EvalId,
    attr_path: &str,
) {
    let target = job_target_url(
        &ctx.current.load().config.external_url,
        forge,
        slug,
        eval_id,
        attr_path,
    );
    let post = CheckPost {
        slug: slug.clone(),
        sha: sha.clone(),
        context: format!("medusa: {attr_path}"),
        state: CheckState::Pending,
        description: Some("queued".to_string()),
        target_url: Some(target),
    };
    spawn_post_check(
        provider.clone(),
        post,
        forge.to_string(),
        ctx.pauses.clone(),
    );
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
    had_eval_error: bool,
) {
    let state = match status {
        JobStatus::Success | JobStatus::Cached => CheckState::Success,
        JobStatus::Failure => CheckState::Failure,
        JobStatus::Cancelled | JobStatus::Interrupted => CheckState::Error,
        JobStatus::SkippedNoBuilder => return,
        JobStatus::Queued | JobStatus::Running => return,
    };
    let target = job_target_url(
        &ctx.current.load().config.external_url,
        forge,
        slug,
        eval_id,
        attr_path,
    );
    let post = CheckPost {
        slug: slug.clone(),
        sha: sha.clone(),
        context: format!("medusa: {attr_path}"),
        state,
        description: Some(match (status, had_eval_error) {
            (JobStatus::Cached, _) => "cache hit".to_string(),
            (JobStatus::Success, _) => "build ok".to_string(),
            // Distinguish eval-time failures from build-time failures
            // — the same JobStatus::Failure covers both, and operators
            // looking at the forge UI need to know which.
            (JobStatus::Failure, true) => "evaluation failed".to_string(),
            (JobStatus::Failure, false) => "build failed".to_string(),
            _ => "build error".to_string(),
        }),
        target_url: Some(target),
    };
    spawn_post_check(
        provider.clone(),
        post,
        forge.to_string(),
        ctx.pauses.clone(),
    );
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
    let target = eval_target_url(
        &ctx.current.load().config.external_url,
        forge,
        slug,
        eval_id,
    );
    let post = CheckPost {
        slug: slug.clone(),
        sha: sha.clone(),
        context: "medusa: evaluation".to_string(),
        state,
        description: Some(description.to_string()),
        target_url: Some(target),
    };
    spawn_post_check(
        provider.clone(),
        post,
        forge.to_string(),
        ctx.pauses.clone(),
    );
}

/// Q82: skip post_check entirely if the forge is paused; mark the forge
/// healthy on a successful post and pause it on 401. Other errors leave
/// the registry alone — those are transient and don't indicate a broken
/// credential.
fn spawn_post_check(
    provider: Arc<dyn Provider>,
    post: CheckPost,
    forge_name: String,
    pauses: Arc<PauseRegistry>,
) {
    if pauses.is_paused(&forge_name) {
        tracing::info!(
            forge = %forge_name,
            "skipping forge post_check: forge paused (Q82)",
        );
        return;
    }
    tokio::spawn(async move {
        match provider.post_check(post).await {
            Ok(_) => pauses.mark_healthy(&forge_name),
            Err(ForgeError::Unauthorised) => {
                pauses.pause(&forge_name, "401 from post_check");
            }
            Err(e) => {
                tracing::warn!(forge = %forge_name, error = %e, "forge post_check failed");
            }
        }
    });
}

async fn persist_job(
    store: &SqlxStore,
    log_dir: &Path,
    repo_id: RepoId,
    eval_id: EvalId,
    spec: &medusa_eval::JobSpec,
) -> anyhow::Result<JobId> {
    // For an eval-time error, nix-eval-jobs typically doesn't include
    // the `system` field — it errored before reaching that point. We
    // fall back to parsing it out of the attr path (`packages.<system>
    // .<rest>`) so the UI doesn't show "unknown".
    let system = spec
        .system
        .clone()
        .or_else(|| system_from_attr_path(spec.attr_path.as_str()))
        .unwrap_or_else(|| "unknown".to_string());
    let job_id = <SqlxStore as JobStore>::create(
        store,
        medusa_store::NewJob {
            eval_id,
            attr_path: spec.attr_path.clone(),
            drv_path: spec.drv_path.clone(),
            system,
        },
    )
    .await?;
    if let Some(error) = spec.error.as_deref() {
        // Write the eval error to the standard log path so the UI's
        // log viewer surfaces it. Without this, the job appears as
        // "failure" with no clickable detail and the operator has to
        // grep daemon logs for the underlying nix error.
        let log_path = log_dir
            .join(repo_id.get().to_string())
            .join(eval_id.get().to_string())
            .join(format!("{}.log.zst", job_id.get()));
        let body = format_eval_error_log(spec.attr_path.as_str(), error);
        if let Err(e) = medusa_build::write_zstd_log(&log_path, body.into_bytes()).await {
            tracing::warn!(
                error = %e,
                attr = %spec.attr_path,
                "failed to write eval-error log",
            );
        }
        let log_path_str = log_path.to_string_lossy().into_owned();
        <SqlxStore as JobStore>::finish(
            store,
            job_id,
            JobStatus::Failure,
            Utc::now(),
            Some(&log_path_str),
            None,
        )
        .await?;
    }
    Ok(job_id)
}

/// Parse `<output>.<system>.<rest>` (e.g. `packages.x86_64-linux.image-v1`)
/// and return the `system` segment. Returns `None` when the path
/// doesn't have the canonical 3+-segment shape.
fn system_from_attr_path(attr_path: &str) -> Option<String> {
    let mut parts = attr_path.splitn(3, '.');
    let _output = parts.next()?;
    let system = parts.next()?;
    // Sanity: require there's a third segment (the actual leaf attr)
    // so we don't misinterpret a 2-segment path like `formatter.x86_64-linux`.
    parts.next()?;
    Some(system.to_string())
}

/// Format the eval error for the build-log file. Mirrors the local
/// fail-fast log shape from `synthesize_no_eligible_builder_log`: a
/// short prologue plus the underlying nix message verbatim, so the
/// UI's log viewer renders something operator-actionable.
fn format_eval_error_log(attr_path: &str, error: &str) -> String {
    format!(
        "medusa: this attribute failed at evaluation time, before any build started.\n\
         attribute: {attr_path}\n\
         \n\
         nix-eval-jobs error:\n\
         {error}\n",
    )
}

#[allow(clippy::too_many_arguments)]
async fn build_one(
    ctx: &WorkerContext,
    repo_id: RepoId,
    eval_id: EvalId,
    job_id: JobId,
    spec: &medusa_eval::JobSpec,
    caches: &[medusa_build::CacheRef],
    cancel: &medusa_web::CancelToken,
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

    let log_path = ctx
        .log_dir
        .join(repo_id.get().to_string())
        .join(eval_id.get().to_string())
        .join(format!("{}.log.zst", job_id.get()));

    // Pre-flight: if the derivation declares `requiredSystemFeatures`
    // and no connected builder advertises every feature on the
    // matching system, fail fast. Without this, nix's remote-build
    // scheduler silently retries internally for many minutes before
    // printing "Failed to find a machine" — visible to the operator
    // only when the build finally gives up. See `bugs.md`.
    if !spec.required_system_features.is_empty() {
        if let Some(system) = spec.system.as_deref() {
            let eligible = ctx.builder_registry.eligible(
                system,
                &spec.required_system_features,
                &std::collections::HashSet::new(),
            );
            if eligible.is_empty() {
                let log = synthesize_no_eligible_builder_log(
                    &spec.attr_path.as_str().to_string(),
                    &drv_path,
                    system,
                    &spec.required_system_features,
                    &ctx.builder_registry,
                );
                if let Err(e) = medusa_build::write_zstd_log(&log_path, log.into_bytes()).await {
                    tracing::warn!(error = %e, "failed to write fail-fast log");
                }
                <SqlxStore as JobStore>::finish(
                    &ctx.store,
                    job_id,
                    JobStatus::Failure,
                    Utc::now(),
                    Some(&log_path.to_string_lossy()),
                    None,
                )
                .await?;
                tracing::warn!(
                    drv = %drv_path,
                    system,
                    required = ?spec.required_system_features,
                    "no eligible builder advertises required features; failing job fast",
                );
                return Ok(JobStatus::Failure);
            }
        }
    }

    // M14: pick a specific builder for this derivation and pin nix to
    // it. `eligible()` already returns least-loaded-first, filtered by
    // (system, requiredSystemFeatures, max_jobs cap). We reserve the
    // slot before recording dispatch + starting the build so a
    // concurrent worker task sees the up-to-date in_flight number.
    //
    // If `eligible` returns nothing — either no connected builders at
    // all, or none match this derivation's system/features — we fall
    // through to the legacy multi-builder `--builders` arg (or to the
    // host's `nix.buildMachines` if that's also empty). Pre-flight
    // for required-feature jobs already short-circuits the
    // "connected pool exists but nothing matches" case above.
    let chosen = pick_builder_for_spec(&ctx.builder_registry, spec);
    let _slot = chosen
        .as_ref()
        .map(|b| BuilderSlot::reserve(ctx.builder_registry.clone(), b.name.clone()));

    <SqlxStore as JobStore>::start(&ctx.store, job_id, Utc::now()).await?;
    if let Some(b) = &chosen {
        // Surfaces the chosen builder in the read-only UI's running
        // table and lets future M14 work (per-builder running counts
        // grouped from the DB) be honest.
        <SqlxStore as JobStore>::dispatch(&ctx.store, job_id, b.builder_id, Utc::now()).await?;
    }

    // Pre-create the gcroot parent dir so `nix-store --add-root` can drop
    // the symlink atomically with the build (otherwise it ENOENTs and the
    // realise call fails before any building happens).
    let gc_root = medusa_build::gc_root_path(&ctx.gc_root_dir, repo_id, eval_id, job_id);
    if let Some(parent) = gc_root.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            tracing::warn!(error = %e, dir = %parent.display(), "failed to create gcroot parent dir; build will run without a gcroot");
        }
    }
    tracing::info!(
        eval_id = eval_id.get(),
        drv = %drv_path,
        log = %log_path.display(),
        pinned_builder = chosen.as_ref().map(|b| b.name.as_str()),
        "dispatching build",
    );
    // Q39 / Q104 / Q105: race the build against the eval's cancel
    // signal. `biased;` polls the build first — if it just resolved
    // with success we honour that even if cancel arrived in the same
    // event-loop tick (Q105). On cancel-wins we drop the build future;
    // for the local fallback path, `Command::kill_on_drop(true)` reaps
    // the child; for the remote (pool) path, `dispatch_build_via_pool`
    // sends an `Abort` control message and drains the resulting
    // `BuildFinished{Killed}` itself.
    let outcome = match &chosen {
        Some(b) => {
            // M14b: dispatch via the dynamic builder pool through
            // side channels. The helper drives push-closure → Build
            // → drain lifecycle → pull-closure → register-gcroot
            // entirely; the `cancel` token is honoured inside.
            dispatch_build_via_pool(
                ctx,
                &b.name,
                job_id,
                &drv_path,
                &gc_root,
                &log_path,
                medusa_build::LogCaptureLimit::default(),
                cancel,
            )
            .await?
        }
        None => {
            // No matching connected builder: fall back to a local
            // `nix-store --realise` (no `--builders`). The host's
            // `nix.buildMachines`, if any, is honoured natively; if
            // not, the build runs locally.
            let request = medusa_build::BuildRequest {
                drv_path: drv_path.clone(),
                log_path: log_path.clone(),
                timeout: ctx.build_timeout,
                log_limit: medusa_build::LogCaptureLimit::default(),
                gc_root: Some(gc_root.clone()),
            };
            tokio::select! {
                biased;
                res = medusa_build::run_build(&request) => res?,
                _ = cancel.cancelled() => {
                    tracing::info!(
                        job_id = job_id.get(),
                        attr = %spec.attr_path,
                        "build cancelled by new push (Q39); killing nix-store",
                    );
                    <SqlxStore as JobStore>::finish(
                        &ctx.store, job_id, JobStatus::Cancelled, Utc::now(), None, None
                    ).await?;
                    return Ok(JobStatus::Cancelled);
                }
            }
        }
    };
    let log_path_str = log_path.to_string_lossy().into_owned();
    tracing::info!(
        eval_id = eval_id.get(),
        drv = %drv_path,
        status = ?outcome.status,
        "build finished",
    );

    match outcome.status {
        medusa_build::BuildStatus::Success => {
            // gcroot was already registered by `nix-store --realise --add-root`
            // above (atomic with the build) — nothing else to do here.
            let primary = outcome
                .output_paths
                .first()
                .cloned()
                .or_else(|| spec.primary_output().map(String::from));
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

/// Bundle of the small bits `dispatch_pool_build` reads. Lifted out
/// of `WorkerContext` so the same orchestration can also be invoked
/// from the control socket's `TestDispatchDrv` handler without
/// constructing a full WorkerContext.
pub struct PoolDispatchSpec<'a> {
    pub registry: Arc<medusa_builders::BuilderRegistry>,
    pub builder_name: &'a medusa_domain::BuilderName,
    pub build_id: i64,
    pub drv_path: &'a str,
    pub gc_root: &'a Path,
    pub log_path: &'a Path,
    pub log_limit: medusa_build::LogCaptureLimit,
    pub build_timeout: Duration,
    pub nix_store_bin: &'a Path,
}

/// Distinguishes "build cancelled by caller" from "build finished
/// (success or failure) with this BuildOutcome". The worker's
/// `build_one` translates `Cancelled` into a `JobStatus::Cancelled`
/// row update and a `JobStatus::Cancelled` return value; the test
/// dispatch path never sees `Cancelled` (no cancel token is wired in).
pub enum PoolDispatchResult {
    Outcome(medusa_build::BuildOutcome),
    Cancelled,
}

/// Worker-side wrapper that translates `PoolDispatchResult::Cancelled`
/// into a `JobStore` row update + the `JobStatus::Cancelled` return.
async fn dispatch_build_via_pool(
    ctx: &WorkerContext,
    builder_name: &medusa_domain::BuilderName,
    job_id: JobId,
    drv_path: &str,
    gc_root: &Path,
    log_path: &Path,
    log_limit: medusa_build::LogCaptureLimit,
    cancel: &medusa_web::CancelToken,
) -> anyhow::Result<medusa_build::BuildOutcome> {
    let spec = PoolDispatchSpec {
        registry: ctx.builder_registry.clone(),
        builder_name,
        build_id: job_id.get(),
        drv_path,
        gc_root,
        log_path,
        log_limit,
        build_timeout: ctx.build_timeout,
        nix_store_bin: &ctx.nix_store_bin,
    };
    match dispatch_pool_build(spec, Some(cancel)).await? {
        PoolDispatchResult::Outcome(o) => Ok(o),
        PoolDispatchResult::Cancelled => {
            <SqlxStore as JobStore>::finish(
                &ctx.store,
                job_id,
                JobStatus::Cancelled,
                Utc::now(),
                Some(&log_path.to_string_lossy()),
                None,
            )
            .await?;
            Err(anyhow!("build cancelled"))
        }
    }
}

/// M14b daemon-side build orchestration.
///
/// Drives one derivation through the pool: push the drv's input
/// closure to the chosen builder over a `ClosurePush` side channel,
/// send a `Build` control message, drain the resulting
/// `BuildStarted` / `BuildLogChunk*` / `BuildFinished` lifecycle
/// (raced against an optional `cancel` token), pull the output
/// closure over a `ClosurePull` side channel, and register the
/// gcroot for the daemon-side copy of the first output.
///
/// Returns a [`PoolDispatchResult`]: either `Outcome(BuildOutcome)`
/// for the normal (success / failure) cases or `Cancelled` if the
/// caller's `cancel` token fired. The Cancelled-row update on
/// `JobStore` is the worker's responsibility — see the
/// [`dispatch_build_via_pool`] thin wrapper above.
pub async fn dispatch_pool_build(
    spec: PoolDispatchSpec<'_>,
    cancel: Option<&medusa_web::CancelToken>,
) -> anyhow::Result<PoolDispatchResult> {
    let PoolDispatchSpec {
        registry,
        builder_name,
        build_id,
        drv_path,
        gc_root,
        log_path,
        log_limit,
        build_timeout,
        nix_store_bin,
    } = spec;
    let dispatcher = BuilderDispatcher::new(registry.clone());
    let cap = log_limit.max_raw_bytes;

    let mut log_buf: Vec<u8> = Vec::new();
    let mut log_truncated = false;

    let early_failure = |log_buf: Vec<u8>, log_path: PathBuf| async move {
        medusa_build::write_zstd_log(&log_path, log_buf).await?;
        Ok::<_, anyhow::Error>(PoolDispatchResult::Outcome(medusa_build::BuildOutcome {
            status: medusa_build::BuildStatus::Failure,
            exit_code: None,
            output_paths: Vec::new(),
            log_path,
            log_truncated: false,
        }))
    };

    // 1. Compute the drv's input closure on the daemon side.
    //    `nix-store --query --requisites <drv>` returns the drv plus
    //    every input drv it transitively depends on, plus any source
    //    paths referenced by builtins. That's exactly what the agent
    //    needs to be able to run `--realise` on the drv.
    let closure = match query_requisites(nix_store_bin, &[drv_path.to_string()]).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(job_id = %build_id, error = %e, "could not compute drv closure");
            log_buf.extend_from_slice(
                format!("medusa: could not compute drv closure: {e}\n").as_bytes(),
            );
            return early_failure(log_buf, log_path.to_path_buf()).await;
        }
    };

    // 2a. Probe the builder for which closure paths it already has,
    //     so we can ship only the missing subset. Opens a side
    //     channel, sends `QueryValidPaths`, reads the reply, and
    //     filters. On *any* probe error we fall back to pushing the
    //     full closure — the optimization must never break a build,
    //     and it lets us interoperate with older agents that don't
    //     know the new header kind.
    let full_closure_count = closure.len();
    let to_push: Vec<String> = match dispatcher.open_channel(builder_name).await {
        Ok(mut d) => {
            let probe_chan = d.take_channel().expect("dispatcher returned channel");
            match query_invalid_over_channel(probe_chan, build_id, closure.clone()).await {
                Ok(invalid) => {
                    tracing::debug!(
                        builder = %builder_name,
                        build_id,
                        queried = full_closure_count,
                        invalid = invalid.len(),
                        "valid-paths probe succeeded",
                    );
                    invalid
                }
                Err(e) => {
                    tracing::warn!(
                        builder = %builder_name,
                        build_id,
                        error = %e,
                        "valid-paths probe failed; falling back to full push",
                    );
                    closure
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                builder = %builder_name,
                build_id,
                error = %e,
                "could not open probe channel; falling back to full push",
            );
            closure
        }
    };

    // 2b. If the builder already has every path, skip the push
    //     entirely — `nix-store --export` with no paths errors out,
    //     and there's nothing to ship anyway.
    if to_push.is_empty() {
        tracing::info!(
            builder = %builder_name,
            build_id,
            queried = full_closure_count,
            "builder has full closure; skipping ClosurePush",
        );
    } else {
        // 2c. Push the (filtered) closure to the builder.
        let push_chan = match dispatcher.open_channel(builder_name).await {
            Ok(mut d) => d.take_channel(),
            Err(e) => {
                tracing::warn!(builder = %builder_name, error = %e, "open ClosurePush channel failed");
                log_buf.extend_from_slice(
                    format!("medusa: could not open builder channel: {e}\n").as_bytes(),
                );
                return early_failure(log_buf, log_path.to_path_buf()).await;
            }
        };
        let push_chan = push_chan.expect("dispatcher returned channel");
        let closure_path_count = to_push.len();
        if let Err(e) = push_closure_over_channel(push_chan, build_id, to_push, nix_store_bin).await
        {
            tracing::warn!(
                error = %e,
                builder = %builder_name,
                build_id,
                drv = drv_path,
                closure_paths = closure_path_count,
                "push_closure_over_channel failed",
            );
            // Surface the failure in the build log. `e`'s Display already
            // includes the agent's stderr when the failure was an import
            // rejection (see `ClosureXferError::AgentImportFailed`); for
            // pure transport errors (BrokenPipe, channel teardown) we
            // additionally hint where to look on the agent side.
            log_buf.extend_from_slice(
                format!(
                    "medusa: pushing drv closure to `{builder}` failed:\n\
                     {e}\n\
                     medusa: drv={drv_path}, closure_paths={closure_path_count}\n\
                     medusa: if no agent stderr is shown above, the channel was\n\
                     medusa: torn down before the agent could reply — check the\n\
                     medusa: builder's `journalctl -u medusa-builder.service` for\n\
                     medusa: a `side channel ended with error` line near build_id={build_id}.\n",
                    builder = builder_name,
                )
                .as_bytes(),
            );
            return early_failure(log_buf, log_path.to_path_buf()).await;
        }
    }

    // 3. Send Build over the control channel; subscribe to lifecycle.
    let mut lifecycle = match dispatcher
        .dispatch_build(
            builder_name,
            build_id,
            drv_path.to_string(),
            // gcroot is daemon-side — we register it after the pull
            // closure lands locally. Asking the agent to add a gcroot
            // on the builder host would protect the agent's copy, not
            // ours.
            None,
            build_timeout.as_secs(),
            cap as u64,
        )
        .await
    {
        Ok(rx) => rx,
        Err(e) => {
            tracing::warn!(error = %e, "dispatch_build failed");
            log_buf.extend_from_slice(format!("medusa: dispatch_build failed: {e}\n").as_bytes());
            return early_failure(log_buf, log_path.to_path_buf()).await;
        }
    };

    // 4. Drain lifecycle, racing against cancel + a daemon-side wall
    //    clock. The agent passes `--option build-timeout` to nix-store,
    //    which only kills the *builder script*, not a wedged
    //    `--realise` pipeline (substitution loops, hung writes to the
    //    nix-daemon socket, agents that emit `BuildStarted` and then
    //    silently hang). Without an outer timer, `dispatch_pool_build`
    //    would wait on `lifecycle.recv()` forever. We give the build
    //    `build_timeout + grace` (grace = 60s for the agent's SIGKILL
    //    + final stderr drain), then synthesize a Killed outcome and
    //    move on. Mirrors the local fallback's `tokio::time::timeout`
    //    around `medusa_build::run_build`.
    let grace = Duration::from_secs(60);
    let timeout_deadline = tokio::time::Instant::now() + build_timeout + grace;
    let mut output_paths: Vec<String> = Vec::new();
    let mut final_status = BuildOutcomeStatus::Failure;
    let mut exit_code: Option<i32> = None;
    let mut aborted = false;
    let mut timed_out = false;
    loop {
        tokio::select! {
            biased;
            ev = lifecycle.recv() => {
                match ev {
                    Some(BuildLifecycle::Started { pid }) => {
                        tracing::info!(job_id = build_id, ?pid, builder = %builder_name, "agent started build");
                    }
                    Some(BuildLifecycle::LogChunk { bytes }) => {
                        if log_buf.len() < cap {
                            let remaining = cap - log_buf.len();
                            if bytes.len() <= remaining {
                                log_buf.extend_from_slice(&bytes);
                            } else {
                                log_buf.extend_from_slice(&bytes[..remaining]);
                                log_truncated = true;
                            }
                        } else {
                            log_truncated = true;
                        }
                    }
                    Some(BuildLifecycle::Finished {
                        status,
                        exit_code: code,
                        output_paths: outs,
                        log_truncated: t,
                    }) => {
                        final_status = status;
                        exit_code = code;
                        output_paths = outs;
                        log_truncated = log_truncated || t;
                        break;
                    }
                    None => {
                        tracing::warn!(job_id = build_id, builder = %builder_name, "lifecycle channel closed before BuildFinished");
                        log_buf.extend_from_slice(b"\nmedusa: builder disconnected mid-build.\n");
                        final_status = BuildOutcomeStatus::Failure;
                        break;
                    }
                }
            }
            _ = wait_cancelled(cancel), if !aborted => {
                aborted = true;
                tracing::info!(job_id = build_id, builder = %builder_name, "cancel: sending Abort to builder");
                let _ = dispatcher.abort_build(builder_name, build_id).await;
                // Continue draining for the BuildFinished{Killed} that
                // the agent will emit after SIGKILLing nix-store.
            }
            _ = tokio::time::sleep_until(timeout_deadline), if !aborted && !timed_out => {
                tracing::warn!(
                    job_id = build_id,
                    builder = %builder_name,
                    timeout_secs = build_timeout.as_secs(),
                    "build wall-clock timeout exceeded; sending Abort and giving the agent {}s to drain",
                    grace.as_secs(),
                );
                log_buf.extend_from_slice(
                    format!(
                        "\nmedusa: build timed out after {}s; sending Abort to builder.\n",
                        build_timeout.as_secs(),
                    ).as_bytes(),
                );
                timed_out = true;
                let _ = dispatcher.abort_build(builder_name, build_id).await;
                // After Abort, give the agent up to `grace` for its
                // BuildFinished{Killed} (and any final log chunks) to
                // arrive — *don't* spin re-firing the timer. If the
                // agent is unresponsive, the second timer fires and we
                // synthesize a Killed outcome.
                tokio::time::sleep(grace).await;
                tracing::warn!(
                    job_id = build_id,
                    builder = %builder_name,
                    "agent did not emit BuildFinished within grace window; synthesising Killed",
                );
                log_buf.extend_from_slice(
                    b"medusa: agent unresponsive after Abort; synthesising Killed outcome.\n",
                );
                final_status = BuildOutcomeStatus::Killed;
                break;
            }
        }
    }
    registry.unregister_in_flight_build(builder_name, build_id);

    if aborted || (final_status == BuildOutcomeStatus::Killed && !timed_out) {
        // Operator-initiated cancel (or agent-emitted Killed before
        // we asked) → surface as JobStatus::Cancelled via the
        // worker's wrapper.
        if log_truncated {
            log_buf.extend_from_slice(b"\n--- log truncated by medusa ---\n");
        }
        medusa_build::write_zstd_log(log_path, log_buf).await?;
        return Ok(PoolDispatchResult::Cancelled);
    }
    if timed_out {
        // Wall-clock timeout: not a user cancel, surface as Failure
        // so the eval rolls up correctly and the job row gets the
        // right terminal status.
        if log_truncated {
            log_buf.extend_from_slice(b"\n--- log truncated by medusa ---\n");
        }
        medusa_build::write_zstd_log(log_path, log_buf).await?;
        return Ok(PoolDispatchResult::Outcome(medusa_build::BuildOutcome {
            status: medusa_build::BuildStatus::Failure,
            exit_code,
            output_paths: Vec::new(),
            log_path: log_path.to_path_buf(),
            log_truncated,
        }));
    }

    // 5. On success, pull the output closure into the local store.
    if final_status == BuildOutcomeStatus::Success && !output_paths.is_empty() {
        let pull_chan = match dispatcher.open_channel(builder_name).await {
            Ok(mut d) => d.take_channel(),
            Err(e) => {
                tracing::warn!(error = %e, "open ClosurePull channel failed");
                log_buf.extend_from_slice(
                    format!("\nmedusa: could not open pull channel: {e}\n").as_bytes(),
                );
                final_status = BuildOutcomeStatus::Failure;
                None
            }
        };
        if let Some(chan) = pull_chan {
            match pull_closure_over_channel(chan, build_id, output_paths.clone(), nix_store_bin)
                .await
            {
                Ok(o) if o.status == BuildOutcomeStatus::Success => {
                    // 6. Register gcroot on the first output (matches
                    //    the local path's atomic --add-root semantics
                    //    as closely as we can post-hoc).
                    if let Some(first) = output_paths.first() {
                        if let Err(e) = add_indirect_gcroot(nix_store_bin, gc_root, first).await {
                            tracing::warn!(
                                error = %e,
                                gc_root = %gc_root.display(),
                                output = %first,
                                "registering gcroot failed; output may be GC'd",
                            );
                        }
                    }
                }
                Ok(o) => {
                    tracing::warn!(
                        status = ?o.status,
                        stderr = %String::from_utf8_lossy(&o.stderr),
                        "pulling output closure failed",
                    );
                    log_buf.extend_from_slice(b"\nmedusa: pulling output closure failed:\n");
                    log_buf.extend_from_slice(&o.stderr);
                    log_buf.push(b'\n');
                    final_status = BuildOutcomeStatus::Failure;
                }
                Err(ClosureXferError::Spawn { .. }) | Err(_) => {
                    log_buf.extend_from_slice(b"\nmedusa: pulling output closure errored.\n");
                    final_status = BuildOutcomeStatus::Failure;
                }
            }
        }
    }

    if log_truncated {
        log_buf.extend_from_slice(b"\n--- log truncated by medusa ---\n");
    }
    medusa_build::write_zstd_log(log_path, log_buf).await?;

    Ok(PoolDispatchResult::Outcome(medusa_build::BuildOutcome {
        status: match final_status {
            BuildOutcomeStatus::Success => medusa_build::BuildStatus::Success,
            _ => medusa_build::BuildStatus::Failure,
        },
        exit_code,
        output_paths,
        log_path: log_path.to_path_buf(),
        log_truncated,
    }))
}

/// Wait on an optional cancel token. Returns immediately when fired;
/// stays pending forever when `cancel` is `None`.
async fn wait_cancelled(cancel: Option<&medusa_web::CancelToken>) {
    match cancel {
        Some(c) => c.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}

/// Run `<nix_store_bin> --add-root <gc_root> --indirect <output>`
/// after pulling the output closure. Best-effort: nix-store will print
/// to stderr on failure but the build is otherwise complete.
async fn add_indirect_gcroot(
    nix_store_bin: &Path,
    gc_root: &Path,
    output: &str,
) -> anyhow::Result<()> {
    let out = Command::new(nix_store_bin)
        .arg("--add-root")
        .arg(gc_root)
        .arg("--indirect")
        .arg("--realise")
        .arg(output)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if !out.status.success() {
        return Err(anyhow!(
            "nix-store --add-root --indirect --realise {} exited {:?}: {}",
            output,
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim(),
        ));
    }
    Ok(())
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

/// RAII handle that deregisters the cancellation token on `Drop` so the
/// registry doesn't leak entries when `process()` returns from any of
/// its many error paths.
struct CancelGuard<'a> {
    registry: &'a CancelRegistry,
    eval_id: EvalId,
}

impl Drop for CancelGuard<'_> {
    fn drop(&mut self) {
        self.registry.deregister(self.eval_id);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BuilderSlot, JobTally, collapsed_progress, format_eval_error_log, pick_builder_for_spec,
        summarise_for_check, synthesize_no_eligible_builder_log, system_from_attr_path,
    };
    use medusa_builders::{BuilderRegistry, ConnState, ConnectedBuilder};
    use medusa_domain::{AttrPath, BuilderCapabilities, BuilderId, BuilderName, JobStatus};

    fn caps(systems: &[&str], features: &[&str], max_jobs: u32) -> BuilderCapabilities {
        BuilderCapabilities {
            systems: systems.iter().map(|s| s.to_string()).collect(),
            features: features.iter().map(|s| s.to_string()).collect(),
            max_jobs,
            nix_version: "test".into(),
        }
    }

    fn register(reg: &BuilderRegistry, name: &str, builder_id: i64, c: BuilderCapabilities) {
        let _ = reg.register(
            BuilderName::new(name).unwrap(),
            ConnectedBuilder {
                builder_id: BuilderId::new(builder_id),
                capabilities: c,
                state: ConnState::Active,
                connected_since: chrono::Utc::now(),
                connection_id: reg.next_connection_id(),
                session: None,
            },
        );
    }

    fn spec(system: Option<&str>, required: &[&str]) -> medusa_eval::JobSpec {
        medusa_eval::JobSpec {
            attr_path: AttrPath::new("packages.x86_64-linux.foo"),
            drv_path: Some("/nix/store/xxx-foo.drv".into()),
            system: system.map(str::to_string),
            error: None,
            outputs: Default::default(),
            meta: serde_json::Value::Null,
            is_cached: false,
            required_system_features: required.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn builder_slot_increments_and_decrements_in_flight() {
        let reg = BuilderRegistry::new();
        let name = BuilderName::new("solo").unwrap();
        register(&reg, "solo", 1, caps(&["x86_64-linux"], &[], 4));
        assert_eq!(reg.snapshot(&name).unwrap().in_flight, 0);

        let slot = BuilderSlot::reserve(reg.clone(), name.clone());
        assert_eq!(
            reg.snapshot(&name).unwrap().in_flight,
            1,
            "reserve must increment the per-builder in-flight count",
        );
        drop(slot);
        assert_eq!(
            reg.snapshot(&name).unwrap().in_flight,
            0,
            "drop must release the slot — even on cancellation paths",
        );
    }

    #[test]
    fn pick_builder_returns_none_when_pool_empty() {
        let reg = BuilderRegistry::new();
        assert!(pick_builder_for_spec(&reg, &spec(Some("x86_64-linux"), &[])).is_none());
    }

    #[test]
    fn pick_builder_returns_none_when_no_system_match() {
        let reg = BuilderRegistry::new();
        register(&reg, "darwin", 1, caps(&["aarch64-darwin"], &[], 4));
        assert!(pick_builder_for_spec(&reg, &spec(Some("x86_64-linux"), &[])).is_none());
    }

    #[test]
    fn pick_builder_picks_least_loaded_eligible() {
        let reg = BuilderRegistry::new();
        register(&reg, "busy", 1, caps(&["x86_64-linux"], &[], 4));
        register(&reg, "idle", 2, caps(&["x86_64-linux"], &[], 4));
        // Saturate "busy" with one slot taken.
        let _slot = BuilderSlot::reserve(reg.clone(), BuilderName::new("busy").unwrap());

        let chosen = pick_builder_for_spec(&reg, &spec(Some("x86_64-linux"), &[]))
            .expect("eligible builder should be returned");
        assert_eq!(
            chosen.name.as_str(),
            "idle",
            "must prefer the builder with fewer in-flight slots",
        );
    }

    #[test]
    fn pick_builder_filters_by_required_features() {
        let reg = BuilderRegistry::new();
        register(&reg, "plain", 1, caps(&["x86_64-linux"], &[], 4));
        register(&reg, "kvm", 2, caps(&["x86_64-linux"], &["kvm"], 4));

        let chosen = pick_builder_for_spec(&reg, &spec(Some("x86_64-linux"), &["kvm"]))
            .expect("kvm-capable builder exists");
        assert_eq!(
            chosen.name.as_str(),
            "kvm",
            "must filter out builders missing required features",
        );
    }

    #[test]
    fn pick_builder_returns_none_when_at_max_jobs() {
        let reg = BuilderRegistry::new();
        register(&reg, "tiny", 1, caps(&["x86_64-linux"], &[], 1));
        let _slot = BuilderSlot::reserve(reg.clone(), BuilderName::new("tiny").unwrap());

        assert!(
            pick_builder_for_spec(&reg, &spec(Some("x86_64-linux"), &[])).is_none(),
            "saturated builder must not be chosen — caller falls back to multi-builder \
             arg so nix's own scheduler can retry against the full pool",
        );
    }

    #[test]
    fn no_eligible_builder_log_lists_drv_attr_features_and_builders() {
        let reg = BuilderRegistry::new();
        let log = synthesize_no_eligible_builder_log(
            "packages.x86_64-linux.test-cuda-amd",
            "/nix/store/xxx-test-cuda-amd.drv",
            "x86_64-linux",
            &["cuda".into(), "uid-range".into()],
            &reg,
        );
        assert!(log.contains("packages.x86_64-linux.test-cuda-amd"));
        assert!(log.contains("/nix/store/xxx-test-cuda-amd.drv"));
        assert!(log.contains("x86_64-linux"));
        assert!(log.contains("cuda"));
        assert!(log.contains("uid-range"));
        assert!(
            log.contains("connected builders: (none)"),
            "empty registry must surface in the log",
        );
        assert!(
            log.contains("Fix one of"),
            "operator-actionable hint must be included",
        );
    }

    #[test]
    fn collapsed_progress_zero_done() {
        let t = JobTally::default();
        let s = collapsed_progress(&t, 200);
        assert_eq!(s, "0/200 done — 0 ok, 0 cached, 0 failed");
    }

    #[test]
    fn collapsed_progress_mixed() {
        let mut t = JobTally::default();
        for _ in 0..10 {
            t.record(JobStatus::Success);
        }
        for _ in 0..3 {
            t.record(JobStatus::Cached);
        }
        for _ in 0..2 {
            t.record(JobStatus::Failure);
        }
        let s = collapsed_progress(&t, 200);
        assert_eq!(s, "15/200 done — 10 ok, 3 cached, 2 failed");
    }

    #[test]
    fn collapsed_progress_fits_github_140char_cap() {
        let mut t = JobTally::default();
        // Worst case: every counter at 5 digits, total at 5 digits.
        for _ in 0..99_999 {
            t.record(JobStatus::Success);
        }
        let s = collapsed_progress(&t, 99_999);
        assert!(
            s.len() <= 140,
            "collapsed_progress exceeded GitHub's 140-char description cap: {} chars",
            s.len()
        );
    }

    #[test]
    fn summary_picks_first_nonempty_line() {
        let err = "\n\n  evaluation failed  \nbecause of reasons\nwith more context\n";
        assert_eq!(summarise_for_check(err, 80), "evaluation failed");
    }

    #[test]
    fn summary_truncates_with_ellipsis() {
        let line = "a".repeat(200);
        let s = summarise_for_check(&line, 50);
        assert_eq!(s.chars().count(), 50);
        assert!(s.ends_with('…'));
    }

    #[test]
    fn summary_handles_empty_input() {
        assert_eq!(summarise_for_check("", 20), "");
        assert_eq!(summarise_for_check("\n\n\n", 20), "");
    }

    #[test]
    fn system_from_attr_path_extracts_canonical_three_segment() {
        assert_eq!(
            system_from_attr_path("packages.x86_64-linux.image-v1"),
            Some("x86_64-linux".to_string()),
        );
        assert_eq!(
            system_from_attr_path("checks.aarch64-darwin.foo"),
            Some("aarch64-darwin".to_string()),
        );
        // Nested attrs (e.g. nixosConfigurations.<name>.config.system.build.toplevel)
        // — second segment is the system regardless of further nesting.
        assert_eq!(
            system_from_attr_path("packages.x86_64-linux.foo.bar.baz"),
            Some("x86_64-linux".to_string()),
        );
    }

    #[test]
    fn system_from_attr_path_returns_none_for_two_segment_paths() {
        // `formatter.x86_64-linux` is a flake output without a leaf
        // attr beneath the system; treat as unknown rather than
        // misclaiming x86_64-linux as the system.
        assert_eq!(system_from_attr_path("formatter.x86_64-linux"), None);
        assert_eq!(system_from_attr_path("packages"), None);
        assert_eq!(system_from_attr_path(""), None);
    }

    #[test]
    fn eval_error_log_includes_attr_and_underlying_message() {
        let body = format_eval_error_log(
            "packages.x86_64-linux.image-v1",
            "error: duplicate derivation output 'scripts'",
        );
        assert!(body.contains("packages.x86_64-linux.image-v1"));
        assert!(body.contains("duplicate derivation output"));
        assert!(
            body.contains("evaluation time"),
            "log must distinguish eval-time from build-time failure: {body}",
        );
    }
}
