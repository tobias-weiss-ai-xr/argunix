//! Background evaluator/builder.
//!
//! The webhook handler creates an `Evaluation` row with status=Queued and
//! sends its `EvalId` to the worker via an mpsc channel. The worker:
//!
//! 1. Looks up the eval and its repo,
//! 2. Resolves the forge provider and constructs a clone URL,
//! 3. Shells out to `git` to clone the repo at the recorded SHA into a
//!    temp work dir,
//! 4. Runs the eval pipeline (argunix-eval),
//! 5. Persists each discovered job and runs the build pipeline (argunix-build),
//! 6. Updates the evaluation's terminal status.
//!
//! PR permission/allowlist and watched-branches gating happen earlier in
//! the pipeline — see `argunix_web::policy` — so by the time the worker
//! picks an evaluation up, it's already been authorised.

use anyhow::{Context, anyhow};
use arc_swap::ArcSwap;
use argunix_builders::{
    BuildLifecycle, BuildOutcomeStatus, BuildPhase, BuilderDispatcher, NixCopyDirection,
    nix_copy_over_pool,
};
use argunix_domain::{EvalId, EvalStatus, JobId, JobStatus, RepoId, Sha, Slug};
use argunix_forge::{CheckPost, CheckState, ForgeError, Provider};
use argunix_sched::ScheduleStrategy;
use argunix_store::{EvalStore, JobPhaseMetrics, JobStore, RepoStore, SqlxStore};
use argunix_web::{CancelRegistry, ConfigSnapshot, PauseRegistry, eval_target_url, job_target_url};
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{Instrument, info_span};

/// RAII guard that reserves an `in_flight` slot on a specific builder
/// for the lifetime of one dispatched derivation. Increments
/// on construction; decrements on drop (including when the build
/// future is dropped due to cancellation).
struct BuilderSlot {
    registry: Arc<argunix_builders::BuilderRegistry>,
    name: argunix_domain::BuilderName,
}

impl BuilderSlot {
    fn reserve(
        registry: Arc<argunix_builders::BuilderRegistry>,
        name: argunix_domain::BuilderName,
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

/// RAII guard that owns the live-phase entry for one
/// `(builder, build_id)` pair. Each `set` overwrites the registry's
/// phase map; `drop` clears it, so every exit path of
/// `dispatch_pool_build` (including `?` early returns and panics)
/// removes the entry. Backs the status page's "this builder is
/// pushing / building / pulling right now" overlay.
struct PhaseGuard {
    registry: Arc<argunix_builders::BuilderRegistry>,
    name: argunix_domain::BuilderName,
    build_id: i64,
}

impl PhaseGuard {
    fn new(
        registry: Arc<argunix_builders::BuilderRegistry>,
        name: argunix_domain::BuilderName,
        build_id: i64,
    ) -> Self {
        Self {
            registry,
            name,
            build_id,
        }
    }
    fn set(&self, phase: BuildPhase) {
        self.registry.set_phase(&self.name, self.build_id, phase);
    }
}

impl Drop for PhaseGuard {
    fn drop(&mut self) {
        self.registry.clear_phase(&self.name, self.build_id);
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
    /// match, dispatch goes through the side channels (push closure
    /// → `Build` control message → drain lifecycle → pull outputs).
    /// On no match, the worker falls back to a local `nix-store
    /// --realise` (which itself may use the host's `nix.buildMachines`).
    pub builder_registry: Arc<argunix_builders::BuilderRegistry>,
    /// Per-running-build broadcast taps for the SSE log endpoint.
    /// Pool dispatch opens an entry on `BuildStarted`, pushes each
    /// chunk it gets from the agent, and closes on `BuildFinished`.
    pub live_logs: Arc<argunix_web::LiveLogRegistry>,
    /// Path to the local `nix-store` binary. Used post-pull to
    /// register gc-roots (`nix-store --add-root --indirect`); the
    /// closure transfer itself uses `nix copy` instead.
    pub nix_store_bin: PathBuf,
    /// Path to the local `nix` binary. Used to drive the closure
    /// transfer (`nix copy --from/--to unix:///proxy.sock`). The
    /// proxy is a per-build Unix-domain socket that forwards to the
    /// builder's `nix-daemon --stdio` over our russh side channel.
    /// Tests inject a fake; in production `"nix"` on PATH.
    pub nix_bin: PathBuf,
    /// Maximum number of derivations to build in parallel across the
    /// whole *daemon*. Per-builder concurrency is additionally
    /// gated by each builder's advertised `max_jobs`. Set in
    /// `main.rs`; clamped to ≥1 at use. The actual gating is done
    /// by `global_build_sem` (this scalar is the configured cap,
    /// kept around for future diagnostics / control queries).
    #[allow(dead_code)]
    pub build_concurrency: usize,
    /// Single semaphore the build phase acquires permits from before
    /// spawning a per-derivation task. Lifted out of the per-eval loop
    /// so the cap is global: two concurrent evals share one pool of
    /// `build_concurrency` permits instead of each getting their own.
    /// Without this, the user-visible "global" cap is actually
    /// `build_concurrency × concurrent_evals`, which would overcommit
    /// the daemon-side resources (`nix copy` proxy sockets, log
    /// streamers, file descriptors).
    pub global_build_sem: Arc<tokio::sync::Semaphore>,
    /// Counter of detached build-phase tasks `process` spawned. The
    /// daemon's shutdown sequence awaits this so SIGTERM doesn't
    /// drop in-flight `nix copy` and `nix-store --realise` mid-stream.
    /// Without this tracker, the eval worker drains immediately after
    /// my spawn-and-return refactor — but the actual build tasks
    /// would die when the runtime tears down.
    pub build_tasks: Arc<BuildTaskTracker>,
    /// Cross-eval dispatch scheduler. Today every eval drains its
    /// own JoinSet locally inside `process`, so this field is
    /// constructed but not yet read — wiring it through the dispatch
    /// loop is the next milestone (M14). The strategy is shared
    /// across the eval worker and (eventually) a global dispatcher
    /// task; both lock briefly to call sync methods. `std::sync::Mutex`
    /// matches the pattern used elsewhere in this crate
    /// (see `dispatch_driver`'s test module) and avoids dragging
    /// `tokio::sync::Mutex`'s `await` requirement into call sites
    /// that don't otherwise need to be async.
    #[allow(dead_code)]
    pub scheduler: Arc<std::sync::Mutex<Box<dyn argunix_sched::ScheduleStrategy>>>,
}

/// Counter + Notify the daemon uses to wait for all detached build
/// phase tasks before exiting. Same shape as `CancelToken`'s
/// `flag + notify` pair: cheap, no allocation per spawn, no Mutex
/// held across await.
#[derive(Default)]
pub struct BuildTaskTracker {
    in_flight: std::sync::atomic::AtomicUsize,
    notify: tokio::sync::Notify,
}

impl BuildTaskTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawn `fut` on the current tokio runtime and register it with
    /// the tracker. The spawned task decrements the counter and wakes
    /// any waiter on its return path, so a panic before `await`
    /// completion would still leave the counter accurate (Drop on the
    /// helper guard).
    pub fn spawn<F>(self: &Arc<Self>, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.in_flight
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let tracker = self.clone();
        tokio::spawn(async move {
            // RAII guard so a panic still decrements + notifies.
            struct Decrement(Arc<BuildTaskTracker>);
            impl Drop for Decrement {
                fn drop(&mut self) {
                    self.0
                        .in_flight
                        .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    self.0.notify.notify_waiters();
                }
            }
            let _g = Decrement(tracker);
            fut.await;
        });
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Resolve once the in-flight count drops to zero. The notify-then-
    /// recheck dance handles the race where every task finishes
    /// between our load and our `notified` registration.
    pub async fn wait_idle(&self) {
        loop {
            if self.in_flight() == 0 {
                return;
            }
            let notified = self.notify.notified();
            tokio::pin!(notified);
            if self.in_flight() == 0 {
                return;
            }
            notified.await;
        }
    }
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
                let chained = format!("{e:#}");
                tracing::error!(error = %chained, "evaluation failed in worker");
                // Same chained string lands on the eval row so the UI
                // can show *why* the eval failed without operators
                // having to grep daemon logs. The inner failure handlers
                // (clone, nix-eval-jobs) may already have written a
                // more specific reason via `fail_with_reason`; this is
                // the safety net for unexpected errors that propagated
                // out of `process` without a reason being recorded.
                let _ = <SqlxStore as EvalStore>::fail_with_reason(
                    &ctx.store,
                    eval_id,
                    &chained,
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

    // Register a cancel token *before* checking the DB row, so a
    // cancel that arrives after this point but before we start work is
    // captured. If the DB already says Cancelled (cancel arrived before
    // the worker picked it up), bail without doing anything. See
    // [docs/concepts/cancel-on-push.md] for the broader cancellation
    // model.
    let cancel = ctx.cancellations.register(eval_id);
    let cancel_guard = CancelGuard {
        registry: ctx.cancellations.clone(),
        eval_id,
    };

    // Snapshot the swappable bundle for this evaluation. A reload that
    // lands while we're mid-eval will swap the daemon's pointer but
    // this snapshot remains valid — we finish on the config we
    // started with, so the eval doesn't see provider/repo changes
    // mid-flight.
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
    // Clone the Arc out so the spawned build task can take ownership;
    // the eval task no longer needs to borrow from `snap` past return.
    let provider = snap
        .providers
        .get(&repo.forge)
        .ok_or_else(|| anyhow!("no provider for forge `{}`", repo.forge))?
        .clone();

    // Crash-recovery: an eval already in `Building` was mid-build
    // when the previous daemon died. Jobs are persisted; skip
    // clone + nix-eval-jobs + persist and pick up the build phase
    // with whatever the DB still holds. `main.rs` requeued any
    // `Interrupted` jobs back to `Queued` before redispatching, so
    // every still-pending job is in `Queued` when we get here.
    let is_resume = eval.status == EvalStatus::Building;
    let work_dir = ctx.work_dir.join(eval_id.get().to_string());

    let persisted: Vec<(argunix_eval::JobSpec, JobId)> = if is_resume {
        tracing::info!("resuming building evaluation; skipping clone/eval phase");
        load_jobs_for_resume(&ctx.store, eval_id).await?
    } else {
        <SqlxStore as EvalStore>::start(&ctx.store, eval_id, Utc::now(), EvalStatus::Evaluating)
            .await?;

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
                tracing::info!("evaluation cancelled during clone");
                <SqlxStore as EvalStore>::finish(
                    &ctx.store, eval_id, EvalStatus::Cancelled, Utc::now()
                ).await?;
                return Ok(());
            }
        };

        if cancel.is_cancelled() {
            tracing::info!("evaluation cancelled before eval phase");
            <SqlxStore as EvalStore>::finish(
                &ctx.store,
                eval_id,
                EvalStatus::Cancelled,
                Utc::now(),
            )
            .await?;
            return Ok(());
        }

        // Place nix-eval-jobs' indirect GC roots inside the per-eval
        // work_dir. work_dir is removed at the end of `run_build_phase`
        // (after every job in this eval has reached a terminal state), so
        // the eval-time drv roots naturally release at that point —
        // successful jobs already hold their own output gcroots by then
        // (which pin the drv via nix's default `gc-keep-derivations`),
        // failed/cancelled jobs have nothing to keep. Without these roots
        // the system nix-gc can reclaim a queued job's drv before the
        // worker's `nix copy --to` runs, which then fails with "no
        // substituter that can build it" — the drv is CI-internal and
        // can't be re-fetched from any cache.
        let eval_drv_roots = work_dir.join(".eval-drvs");
        let request = argunix_eval::EvalRequest {
            source_path: work_dir.clone(),
            systems: ctx.systems.clone(),
            outputs: argunix_eval::default_flake_outputs(),
            timeout: ctx.eval_timeout,
            gc_roots_dir: Some(eval_drv_roots),
        };
        let jobs = tokio::select! {
            biased;
            res = argunix_eval::evaluate(&request) => res,
            _ = cancel.cancelled() => {
                tracing::info!("evaluation cancelled during nix-eval-jobs");
                <SqlxStore as EvalStore>::finish(
                    &ctx.store, eval_id, EvalStatus::Cancelled, Utc::now()
                ).await?;
                return Ok(());
            }
        };
        let jobs = match jobs {
            Ok(jobs) => jobs,
            Err(e) => {
                // Surface eval-time failure as a single failed forge
                // check. Github's status `description` field is capped at
                // 140 chars, so we truncate the (often multi-line)
                // nix-eval-jobs error before posting. The eval row's
                // status + `failure_reason` are written by the worker's
                // outer error trap (see `spawn_worker`) using the full
                // chained error, so the UI gets the unsummarised text.
                let detail = summarise_for_check(&e.to_string(), 130);
                post_overall_check(
                    ctx,
                    &provider,
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
        <SqlxStore as EvalStore>::mark_building(&ctx.store, eval_id, Utc::now()).await?;
        tracing::info!(count = jobs.len(), "evaluation finished");

        // Persist every job spec to the DB *before* starting the build
        // loop. Without this, the read-only UI's job table grows row by
        // row as the worker iterates, and a user looking at an in-flight
        // evaluation can't tell whether the rows currently shown are the
        // final list or whether more are still to come. With upfront
        // persistence, the table reflects the final shape as soon as the
        // eval transitions to `Building`, and the eval's status field is
        // the single source of truth for "is anything still pending?".
        let mut persisted: Vec<(argunix_eval::JobSpec, JobId)> = Vec::with_capacity(jobs.len());
        for spec in jobs {
            let job_id = persist_job(&ctx.store, &ctx.log_dir, repo.id, eval_id, &spec).await?;
            persisted.push((spec, job_id));
        }
        persisted
    };

    let caches: Vec<argunix_build::CacheRef> = snap
        .config
        .binary_caches
        .iter()
        .map(|c| argunix_build::CacheRef {
            url: c.url.clone(),
            substitute: c.substitute,
        })
        .collect();

    // Above the threshold we collapse per-job checks into a single
    // rolling `argunix: evaluation` status whose description is
    // updated as jobs finish. PAT path (commit statuses) caps the
    // description at 140 chars — no markdown bullets — so the full
    // job list lives in argunix's UI, reachable via the status's
    // target_url. Richer markdown summaries become possible once the
    // GitHub-App Checks API path is wired up. See
    // [docs/concepts/collapsed-checks.md].
    let repo_cfg = snap
        .config
        .repos
        .iter()
        .find(|r| r.forge == repo.forge && r.slug == repo.slug);
    let threshold = repo_cfg
        .and_then(|r| r.collapsed_check_threshold)
        .unwrap_or(snap.config.schedule.collapsed_check_threshold);
    let total = persisted.len();
    let collapsed_mode = total as u32 > threshold;
    if collapsed_mode {
        tracing::info!(
            jobs = total,
            threshold,
            "collapsed check mode active; per-job statuses suppressed",
        );
    }

    // Walk the dependency closures of every top-level Job's drv so the
    // DagStrategy in `run_build_phase` can gate Job B on Job A when B
    // transitively depends on A. This is the answer to the original
    // duplicate-work question: if A and B are both top-level and B
    // needs A's output, the gating ensures A finishes before B's
    // builder is asked to realise B (and either substitutes A from
    // the post-build cache or — when both land on the same builder —
    // reuses A's local store entry).
    //
    // Walk failures degrade to no-gating: an empty `ClosureWalk`
    // means every Step's `head_drv.input_drvs` is empty, so the
    // strategy treats every Job as immediately Ready and we get the
    // pre-refactor behaviour. We log loudly but don't fail the eval —
    // the underlying nix issue (nix-command not enabled, drv path
    // ungettable, …) shouldn't kill the build phase.
    let head_drv_paths: Vec<&str> = persisted
        .iter()
        .filter_map(|(s, _)| s.drv_path.as_deref())
        .collect();
    let walk = match argunix_eval::walk_closures(&head_drv_paths, ctx.eval_timeout).await {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(
                error = %e,
                head_count = head_drv_paths.len(),
                "closure walk failed; build phase falls back to no-gating",
            );
            argunix_eval::ClosureWalk {
                heads: head_drv_paths.iter().map(|s| s.to_string()).collect(),
                derivations: std::collections::HashMap::new(),
            }
        }
    };
    tracing::info!(
        top_level_jobs = persisted.len(),
        derivations_in_walk = walk.derivations.len(),
        "closure walk done",
    );

    // Replace the initial "evaluating…" overall check (posted at
    // webhook time) with a "building N jobs" pending update.
    // Without this, the GitHub /
    // GitLab / Forgejo UI shows "evaluating…" the entire time builds
    // are running, which is misleading once eval is actually done.
    // In collapsed_mode the rolling tally updates further refine
    // this; here we just ensure there's at least one transition.
    post_overall_check(
        ctx,
        &provider,
        &repo.forge,
        &repo.slug,
        &eval.sha,
        eval_id,
        CheckState::Pending,
        &format!("building {total} jobs"),
    );

    // Post pending per-job checks upfront so the user sees the full
    // matrix of `argunix: <attr>` rows on the commit page immediately,
    // each in the "queued" state, rather than rows blinking into
    // existence one by one as builds finish. Skipped in collapsed
    // mode where per-job checks are entirely suppressed. Also skipped
    // on resume — the previous instance already posted them, and the
    // jobs left to run are a subset of the original set, so re-posting
    // "pending" for the already-finished ones would be a regression on
    // the forge UI.
    if !collapsed_mode && !is_resume {
        for (spec, _) in &persisted {
            post_per_job_check_pending(
                ctx,
                &provider,
                &repo.forge,
                &repo.slug,
                &eval.sha,
                eval_id,
                spec.attr_path.as_str(),
            );
        }
    }

    // Build phase runs as a background task. The eval task (this
    // function's caller) returns immediately and pulls the next
    // EvalId off the channel — so eval N+1's clone+eval starts
    // concurrently with eval N's still-running builds, which is the
    // user-visible point of this whole refactor.
    //
    // Cancel ownership transfers into the spawned task via
    // `cancel_guard`: that's how cancel-on-push can still find this
    // eval's CancelToken in the registry mid-build, and how the
    // registry entry gets dropped exactly when the build phase
    // terminates (Drop on the guard, regardless of error path).
    //
    // Errors inside the build phase used to bubble up via process()'s
    // Result, where the outer `worker::spawn` trap recorded them in
    // `evaluations.failure_reason`. With detached spawning that
    // pathway is gone, so we replicate the trap inline before letting
    // the task end.
    let ctx_owned = ctx.clone();
    let store_owned = ctx.store.clone();
    ctx.build_tasks.spawn(async move {
        if let Err(e) = run_build_phase(
            ctx_owned,
            eval_id,
            repo,
            eval,
            provider,
            persisted,
            walk,
            total,
            collapsed_mode,
            caches,
            cancel,
            work_dir,
            cancel_guard,
        )
        .await
        {
            let chained = format!("{e:#}");
            tracing::error!(error = %chained, "build phase failed");
            let _ = <SqlxStore as EvalStore>::fail_with_reason(
                &store_owned,
                eval_id,
                &chained,
                Utc::now(),
            )
            .await;
        }
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_build_phase(
    ctx: WorkerContext,
    eval_id: EvalId,
    repo: argunix_store::RepoRecord,
    eval: argunix_store::EvalRecord,
    provider: Arc<dyn Provider>,
    persisted: Vec<(argunix_eval::JobSpec, JobId)>,
    walk: argunix_eval::ClosureWalk,
    total: usize,
    collapsed_mode: bool,
    caches: Vec<argunix_build::CacheRef>,
    cancel: argunix_web::CancelToken,
    work_dir: PathBuf,
    // Owns the cancel-token deregister responsibility. Drops at the
    // end of this function, *after* the dispatch loop terminates and
    // the final overall check is posted, so cancel-on-push can find
    // the eval throughout its build phase.
    cancel_guard: CancelGuard,
) -> anyhow::Result<()> {
    let _cancel_guard = cancel_guard;

    let mut tally = JobTally::default();
    let summary_debounce = std::time::Duration::from_secs(2);
    let mut last_summary_post: Option<std::time::Instant> = None;

    // Build a per-eval DagStrategy gated on top-level→top-level deps.
    // For each top-level Job's drv, we look at its transitive closure
    // (from `walk`) and pick out the OTHER top-level Jobs in this same
    // eval that appear in it. Those become the Step's `input_drvs`.
    // Internal Steps (drvs in the closure that aren't top-level Jobs)
    // are treated as external — DagStrategy ignores them and the
    // builder substitutes them as today. Effect: B waits until A
    // succeeds before its dispatch fires, so two builders can't
    // independently rebuild A in parallel for B.
    //
    // Strategy `cap = None`: the global semaphore (`ctx.global_build_sem`)
    // is the canonical cap, shared across evals. The strategy itself
    // doesn't enforce a cap.
    let head_paths: std::collections::HashSet<String> = persisted
        .iter()
        .filter_map(|(s, _)| s.drv_path.clone())
        .collect();
    let specs_by_id: std::collections::HashMap<JobId, argunix_eval::JobSpec> =
        persisted.iter().map(|(s, j)| (*j, s.clone())).collect();
    let mut strategy = argunix_sched::DagStrategy::new(None);
    strategy.set_weight(repo.id, 1);
    // Jobs without a drv_path (eval-error jobs) were already finalised
    // in persist_job; we still need to record their tally + post their
    // check so the eval's overall summary is correct. We process them
    // upfront, before the dispatch loop, so the rolling collapsed-mode
    // summary already reflects the eval-time failures.
    for (spec, _job_id) in &persisted {
        if spec.drv_path.is_some() {
            continue;
        }
        tally.record(JobStatus::Failure);
        if !collapsed_mode {
            post_per_job_check(
                &ctx,
                &provider,
                &repo.forge,
                &repo.slug,
                &eval.sha,
                eval_id,
                &spec.attr_path.as_str().to_string(),
                JobStatus::Failure,
                spec.error.is_some(),
            );
        }
    }
    for (spec, job_id) in &persisted {
        let Some(drv_path) = spec.drv_path.clone() else {
            continue;
        };
        let toplevel_deps: Vec<String> = walk
            .closure_for(&drv_path)
            .into_iter()
            .filter(|d| head_paths.contains(&d.drv_path))
            .map(|d| d.drv_path)
            .collect();
        strategy.enqueue(argunix_sched::ScheduleItem {
            repo_id: repo.id,
            eval_id,
            job_id: *job_id,
            head_drv: argunix_domain::DerivationInfo {
                drv_path,
                system: spec.system.clone(),
                required_features: spec.required_system_features.clone(),
                input_drvs: toplevel_deps,
            },
            closure: Vec::new(),
        });
    }
    drop(persisted); // ownership now lives in `specs_by_id` + `strategy`.

    // Parallelise the build loop. Up to `ctx.build_concurrency`
    // derivations build in parallel *across the whole daemon* (see
    // `WorkerContext::global_build_sem`); per-builder capacity is
    // gated separately by each builder's advertised `max_jobs` (read
    // inside `pick_builder_for_spec` via `BuilderRegistry::eligible`).
    // When the pool is saturated, `pick_builder_for_spec` returns None
    // and `build_one` falls back to a multi-builder `--builders`
    // snapshot, so over-cap dispatches don't deadlock — they go
    // through nix's own scheduler instead.
    let global_sem = ctx.global_build_sem.clone();
    type BuildResult = (
        JobId,
        argunix_eval::JobSpec,
        anyhow::Result<JobStatus>,
        argunix_sched::DispatchToken,
    );
    let mut set: tokio::task::JoinSet<BuildResult> = tokio::task::JoinSet::new();

    'outer: loop {
        // Spawn while we have permits and the strategy has Ready Steps.
        if !cancel.is_cancelled() {
            loop {
                let permit = match global_sem.clone().try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => break, // No global permits free.
                };
                let Some(d) = strategy.dispatch() else {
                    drop(permit);
                    break;
                };
                let job_id = d
                    .head_job
                    .expect("DagStrategy with empty closure dispatches only head Steps");
                let spec = specs_by_id
                    .get(&job_id)
                    .expect("spec present for every enqueued job_id")
                    .clone();
                let token = d.token;
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
                    (job_id, spec, res, token)
                });
            }
        }

        // Termination: in-flight set empty AND strategy fully drained.
        if set.is_empty() && strategy.pending_count() == 0 {
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
                    "evaluation cancelled mid-build-loop; awaiting graceful shutdown of in-flight builds",
                );
                // Drop the strategy's still-pending Steps for this
                // eval. The returned skips are jobs that were waiting
                // on a dep that won't fire now; the daemon writes
                // their DB rows + posts forge checks via the same
                // cascade-skip path used for failure-driven cascades.
                let skips = strategy.cancel_eval(eval_id);
                for skip in skips {
                    if let Some(skipped_spec) = specs_by_id.get(&skip.job_id) {
                        handle_cascade_skip(
                            &ctx,
                            &provider,
                            &repo,
                            &eval,
                            skipped_spec,
                            skip,
                            &mut tally,
                            collapsed_mode,
                        )
                        .await;
                    }
                }
                // Drain naturally — each in-flight build observes
                // the cancel token through its own future and
                // returns `JobStatus::Cancelled` after Abort + drain.
                // The per-build wall-clock timeout bounds how long
                // a wedged agent can keep us here.
                while let Some(joined) = set.join_next().await {
                    if let Ok((_, _, _, token)) = joined {
                        let _ = strategy.complete(token, JobStatus::Cancelled);
                    }
                }
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
        let (primary_job_id, spec, outcome, token) = match joined {
            Ok(t) => t,
            Err(join_err) => {
                // A spawned task panicked or was cancelled; treat as
                // a pipeline error so the overall eval still finishes
                // with a meaningful tally. We can't reach the strategy
                // to call `complete` for the lost token (we don't have
                // it), but the strategy's pending_count was already
                // decremented when dispatch handed it out, so the
                // termination check still works — it just leaves a
                // dangling in_flight slot until the strategy is
                // dropped at end-of-function.
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
        // Tell the strategy how this Step ended. Effects:
        //   - cascaded_skips: top-level Jobs that became unbuildable
        //     because a dep terminated as failure / cancelled. We
        //     finalise each one here since they never receive a
        //     Dispatched.
        //   - alias_completions: top-level Jobs that share the same
        //     `head_drv.drv_path` as the just-completed primary
        //     (Nix re-exports — `pkgs.foo` / `pkgs.bar` aliasing the
        //     same drv). The build only ran once; we mirror the
        //     primary's terminal status into every alias's DB row +
        //     forge check.
        let effects = strategy.complete(token, final_status);
        for skip in effects.cascaded_skips {
            if let Some(skipped_spec) = specs_by_id.get(&skip.job_id) {
                handle_cascade_skip(
                    &ctx,
                    &provider,
                    &repo,
                    &eval,
                    skipped_spec,
                    skip,
                    &mut tally,
                    collapsed_mode,
                )
                .await;
            }
        }
        if !effects.alias_completions.is_empty() {
            // Read the primary's row once (after build_one wrote it)
            // to recover log_path + output_path so aliases can share
            // them: every alias's DB row points at the same log file
            // and output, the UI's log link works under either attr,
            // and we don't waste disk by duplicating the log.
            let primary_row = match <SqlxStore as JobStore>::get(&ctx.store, primary_job_id).await {
                Ok(row) => row,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        primary = primary_job_id.get(),
                        "failed to fetch primary row for alias mirroring",
                    );
                    None
                }
            };
            for alias in effects.alias_completions {
                if let Some(alias_spec) = specs_by_id.get(&alias.job_id) {
                    handle_alias_completion(
                        &ctx,
                        &provider,
                        &repo,
                        &eval,
                        alias_spec,
                        alias,
                        final_status,
                        primary_row.as_ref(),
                        &mut tally,
                        collapsed_mode,
                    )
                    .await;
                }
            }
        }
        tally.record(final_status);
        if collapsed_mode {
            // Debounce. Only post a summary update if 2s elapsed
            // since the last one. The unconditional final post after
            // the loop will catch any tail tally that the debounce
            // dropped. See [docs/concepts/collapsed-checks.md].
            let elapsed_ok = match last_summary_post {
                None => true,
                Some(t) => t.elapsed() >= summary_debounce,
            };
            if elapsed_ok {
                let desc = collapsed_progress(&tally, total);
                post_overall_check(
                    &ctx,
                    &provider,
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
                &ctx,
                &provider,
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
        &ctx,
        &provider,
        &repo.forge,
        &repo.slug,
        &eval.sha,
        eval_id,
        overall_state,
        &description,
    );

    // On crash-resume the workdir may not exist (the previous instance
    // already removed it, or never created it). The existence guard
    // keeps the cleanup quiet in that case.
    if work_dir.exists() {
        if let Err(e) = tokio::fs::remove_dir_all(&work_dir).await {
            tracing::warn!(error = %e, dir = %work_dir.display(), "failed to clean workdir");
        }
    }
    Ok(())
}

/// Handle a top-level Job that became unbuildable because a Step in
/// its closure terminated as failure / cancelled (DagStrategy emits
/// these via `CompletionEffects::cascaded_skips`). The skipped Job
/// never receives a `Dispatched`, so the dispatch loop won't write
/// its DB row or post its forge check via the normal
/// completion path — this helper does it instead.
#[allow(clippy::too_many_arguments)]
async fn handle_cascade_skip(
    ctx: &WorkerContext,
    provider: &Arc<dyn Provider>,
    repo: &argunix_store::RepoRecord,
    eval: &argunix_store::EvalRecord,
    spec: &argunix_eval::JobSpec,
    skip: argunix_sched::CascadedSkip,
    tally: &mut JobTally,
    collapsed_mode: bool,
) {
    // Synthetic log so the UI's log viewer has something to show. The
    // operator clicking through to a Skipped job sees *why*: which
    // upstream drv took it down. Mirror the eval-error log path
    // convention (see `persist_job`) so the UI's
    // `log_path` lookup hits the same way.
    let log_path = ctx
        .log_dir
        .join(skip.repo_id.get().to_string())
        .join(skip.eval_id.get().to_string())
        .join(format!("{}.log.zst", skip.job_id.get()));
    let body = format!(
        "argunix: skipped because a dependency failed.\n\
         attribute: {}\n\
         dependency drv: {}\n",
        spec.attr_path.as_str(),
        skip.reason_drv,
    );
    if let Err(e) = argunix_build::write_zstd_log(&log_path, body.into_bytes()).await {
        tracing::warn!(
            error = %e,
            attr = %spec.attr_path,
            "failed to write cascade-skip log",
        );
    }
    let log_path_str = log_path.to_string_lossy().into_owned();
    if let Err(e) = <SqlxStore as JobStore>::finish(
        &ctx.store,
        skip.job_id,
        JobStatus::Failure,
        Utc::now(),
        Some(&log_path_str),
        None,
        &JobPhaseMetrics::default(),
    )
    .await
    {
        tracing::error!(
            error = %e,
            job_id = skip.job_id.get(),
            "failed to finalise cascade-skipped job in DB",
        );
    }
    tally.record(JobStatus::Failure);
    if !collapsed_mode {
        post_per_job_check(
            ctx,
            provider,
            &repo.forge,
            &repo.slug,
            &eval.sha,
            skip.eval_id,
            &spec.attr_path.as_str().to_string(),
            JobStatus::Failure,
            true, // is_eval_error: surface as a skip-style description in the forge UI
        );
    }
}

/// Mirror the primary's terminal status into an aliased Job — same
/// `head_drv.drv_path` re-exported under another attribute path, so
/// the build only ran once but every aliased Job needs its own DB
/// row finalised + forge check posted.
///
/// `primary_row` is the just-finalised row for the dispatched head
/// Job (read by the caller from `JobStore::get`). When present, the
/// alias inherits its `log_path` and `output_path` so the UI's log
/// viewer works under either attribute name. When `None` (a DB-read
/// hiccup), the alias is finalised with the status alone.
#[allow(clippy::too_many_arguments)]
async fn handle_alias_completion(
    ctx: &WorkerContext,
    provider: &Arc<dyn Provider>,
    repo: &argunix_store::RepoRecord,
    eval: &argunix_store::EvalRecord,
    alias_spec: &argunix_eval::JobSpec,
    alias: argunix_sched::AliasCompletion,
    primary_status: JobStatus,
    primary_row: Option<&argunix_store::JobRecord>,
    tally: &mut JobTally,
    collapsed_mode: bool,
) {
    let log_path = primary_row.and_then(|r| r.log_path.as_deref());
    let output_path = primary_row.and_then(|r| r.output_path.as_deref());
    if let Err(e) = <SqlxStore as JobStore>::finish(
        &ctx.store,
        alias.job_id,
        primary_status,
        Utc::now(),
        log_path,
        output_path,
        &JobPhaseMetrics::default(),
    )
    .await
    {
        tracing::error!(
            error = %e,
            job_id = alias.job_id.get(),
            "failed to finalise aliased job in DB",
        );
    }
    tally.record(primary_status);
    if !collapsed_mode {
        post_per_job_check(
            ctx,
            provider,
            &repo.forge,
            &repo.slug,
            &eval.sha,
            alias.eval_id,
            &alias_spec.attr_path.as_str().to_string(),
            primary_status,
            alias_spec.error.is_some(),
        );
    }
}

/// In-progress description for the rolling collapsed check. GitHub
/// commit-status descriptions are capped at 140 chars; this is
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

/// Pick the builder this derivation should run on. Walks
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
    registry: &argunix_builders::BuilderRegistry,
    spec: &argunix_eval::JobSpec,
) -> Option<argunix_builders::BuilderSnapshot> {
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
    registry: &argunix_builders::BuilderRegistry,
) -> String {
    let mut out = String::new();
    out.push_str("argunix pre-flight: no connected builder satisfies this derivation's\n");
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
        context: format!("argunix: {attr_path}"),
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
        context: format!("argunix: {attr_path}"),
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
        context: "argunix: evaluation".to_string(),
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

/// Skip post_check entirely if the forge is paused; mark the forge
/// healthy on a successful post and pause it on 401. Other errors leave
/// the registry alone — those are transient and don't indicate a broken
/// credential. See [docs/concepts/forge-pause.md].
fn spawn_post_check(
    provider: Arc<dyn Provider>,
    post: CheckPost,
    forge_name: String,
    pauses: Arc<PauseRegistry>,
) {
    if pauses.is_paused(&forge_name) {
        tracing::info!(
            forge = %forge_name,
            "skipping forge post_check: forge paused",
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

/// Reconstruct the worker's `Vec<(JobSpec, JobId)>` from the DB rows
/// for an evaluation that was already `Building` on the previous
/// daemon instance. Only jobs still in `Queued` are returned — any
/// terminal job (success/failure/cached/cancelled) from before the
/// crash is left alone. `Interrupted` jobs are expected to have been
/// flipped back to `Queued` by `main.rs`'s resume pass before this is
/// reached.
///
/// The reconstructed `JobSpec` loses the original `outputs` map and
/// `required_system_features` — those weren't persisted to the jobs
/// table. The consequence: the resumed build will miss the cache-skip
/// shortcut (no `primary_output` to query a binary cache for) and
/// won't see required-feature pre-flight. Both are acceptable on
/// crash recovery: cache-miss costs a re-build (correct result, just
/// slower), and a feature mismatch surfaces as a normal nix build
/// failure rather than a fast-fail.
async fn load_jobs_for_resume(
    store: &SqlxStore,
    eval_id: EvalId,
) -> anyhow::Result<Vec<(argunix_eval::JobSpec, JobId)>> {
    use argunix_domain::AttrPath;
    let rows = <SqlxStore as JobStore>::list_by_eval(store, eval_id)
        .await
        .with_context(|| format!("loading jobs for resumed eval {}", eval_id.get()))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        if row.status != JobStatus::Queued {
            continue;
        }
        let spec = argunix_eval::JobSpec {
            attr_path: AttrPath::new(row.attr_path.as_str().to_string()),
            drv_path: row.drv_path.clone(),
            system: Some(row.system.clone()),
            error: None,
            outputs: std::collections::BTreeMap::new(),
            meta: serde_json::Value::Null,
            is_cached: false,
            required_system_features: Vec::new(),
        };
        out.push((spec, row.id));
    }
    Ok(out)
}

async fn persist_job(
    store: &SqlxStore,
    log_dir: &Path,
    repo_id: RepoId,
    eval_id: EvalId,
    spec: &argunix_eval::JobSpec,
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
        argunix_store::NewJob {
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
        if let Err(e) = argunix_build::write_zstd_log(&log_path, body.into_bytes()).await {
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
            &JobPhaseMetrics::default(),
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
        "argunix: this attribute failed at evaluation time, before any build started.\n\
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
    spec: &argunix_eval::JobSpec,
    caches: &[argunix_build::CacheRef],
    cancel: &argunix_web::CancelToken,
) -> anyhow::Result<JobStatus> {
    if spec.error.is_some() {
        return Ok(JobStatus::Failure);
    }
    let Some(drv_path) = spec.drv_path.clone() else {
        return Ok(JobStatus::Failure);
    };

    // `is_cached` is set by `nix-eval-jobs --check-cache-status` when
    // the derivation's outputs are already valid locally or fetchable
    // from a configured substituter. This catches the case the HTTP
    // `check_cache` probe below misses: an output the coordinator built
    // in a previous eval (e.g. a PR that has now been merged into main
    // re-evaluating to the same drv). Short-circuit before any builder
    // dispatch — the outputs are right there in /nix/store.
    if spec.is_cached {
        if let Some(output) = spec.primary_output() {
            tracing::info!(job_id = job_id.get(), output = %output, "local store hit");
            <SqlxStore as JobStore>::finish(
                &ctx.store,
                job_id,
                JobStatus::Cached,
                Utc::now(),
                None,
                Some(output),
                &JobPhaseMetrics::default(),
            )
            .await?;
            return Ok(JobStatus::Cached);
        }
    }

    if let Some(output) = spec.primary_output() {
        match argunix_build::check_cache(output, caches, Duration::from_secs(30)).await {
            Ok(argunix_build::CacheCheckResult::Hit { cache_url }) => {
                tracing::info!(job_id = job_id.get(), cache = %cache_url, "cache hit");
                <SqlxStore as JobStore>::finish(
                    &ctx.store,
                    job_id,
                    JobStatus::Cached,
                    Utc::now(),
                    None,
                    Some(output),
                    &JobPhaseMetrics::default(),
                )
                .await?;
                return Ok(JobStatus::Cached);
            }
            Ok(argunix_build::CacheCheckResult::Miss) => {}
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
    // only when the build finally gives up.
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
                if let Err(e) = argunix_build::write_zstd_log(&log_path, log.into_bytes()).await {
                    tracing::warn!(error = %e, "failed to write fail-fast log");
                }
                <SqlxStore as JobStore>::finish(
                    &ctx.store,
                    job_id,
                    JobStatus::Failure,
                    Utc::now(),
                    Some(&log_path.to_string_lossy()),
                    None,
                    &JobPhaseMetrics::default(),
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

    // Pick a specific builder for this derivation and pin nix to
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
        // table and keeps per-builder running counts grouped from the
        // DB honest.
        <SqlxStore as JobStore>::dispatch(&ctx.store, job_id, b.builder_id, Utc::now()).await?;
    }

    // Pre-create the gcroot parent dir so `nix-store --add-root` can drop
    // the symlink atomically with the build (otherwise it ENOENTs and the
    // realise call fails before any building happens).
    let gc_root = argunix_build::gc_root_path(&ctx.gc_root_dir, repo_id, eval_id, job_id);
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
    // Race the build against the eval's cancel signal. `biased;`
    // polls the build first — if it just resolved with success we
    // honour that even if cancel arrived in the same event-loop tick
    // (cancellation is cooperative; a green build is not retroactively
    // failed). On cancel-wins we drop the build future; for the local
    // fallback path, `Command::kill_on_drop(true)` reaps the child;
    // for the remote (pool) path, `dispatch_build_via_pool` sends an
    // `Abort` control message and drains the resulting
    // `BuildFinished{Killed}` itself. See [docs/concepts/cancel-on-push.md].
    let (outcome, phase_metrics) = match &chosen {
        Some(b) => {
            // Dispatch via the dynamic builder pool through side
            // channels. The helper drives push-closure → Build
            // → drain lifecycle → pull-closure → register-gcroot
            // entirely; the `cancel` token is honoured inside.
            dispatch_build_via_pool(
                ctx,
                &b.name,
                job_id,
                &drv_path,
                &gc_root,
                &log_path,
                argunix_build::LogCaptureLimit::default(),
                cancel,
            )
            .await?
        }
        None => {
            // No matching connected builder: fall back to a local
            // `nix-store --realise` (no `--builders`). The host's
            // `nix.buildMachines`, if any, is honoured natively; if
            // not, the build runs locally. Local builds have no
            // remote-transport phases, so we record empty metrics.
            let request = argunix_build::BuildRequest {
                drv_path: drv_path.clone(),
                log_path: log_path.clone(),
                timeout: ctx.build_timeout,
                log_limit: argunix_build::LogCaptureLimit::default(),
                gc_root: Some(gc_root.clone()),
            };
            let local_outcome = tokio::select! {
                biased;
                res = argunix_build::run_build(&request) => res?,
                _ = cancel.cancelled() => {
                    tracing::info!(
                        job_id = job_id.get(),
                        attr = %spec.attr_path,
                        "build cancelled by new push; killing nix-store",
                    );
                    <SqlxStore as JobStore>::finish(
                        &ctx.store, job_id, JobStatus::Cancelled, Utc::now(), None, None,
                        &JobPhaseMetrics::default(),
                    ).await?;
                    return Ok(JobStatus::Cancelled);
                }
            };
            (local_outcome, JobPhaseMetrics::default())
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
        argunix_build::BuildStatus::Success => {
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
                &phase_metrics,
            )
            .await?;
            Ok(JobStatus::Success)
        }
        argunix_build::BuildStatus::Failure => {
            <SqlxStore as JobStore>::finish(
                &ctx.store,
                job_id,
                JobStatus::Failure,
                Utc::now(),
                Some(&log_path_str),
                None,
                &phase_metrics,
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
    pub registry: Arc<argunix_builders::BuilderRegistry>,
    pub builder_name: &'a argunix_domain::BuilderName,
    pub build_id: i64,
    pub drv_path: &'a str,
    pub gc_root: &'a Path,
    pub log_path: &'a Path,
    pub log_limit: argunix_build::LogCaptureLimit,
    pub build_timeout: Duration,
    pub nix_store_bin: &'a Path,
    pub nix_bin: &'a Path,
    /// Optional broadcast tap for the SSE log endpoint. Worker passes
    /// the global registry; the test dispatch path can pass `None`.
    pub live_logs: Option<Arc<argunix_web::LiveLogRegistry>>,
}

/// Distinguishes "build cancelled by caller" from "build finished
/// (success or failure) with this BuildOutcome". The worker's
/// `build_one` translates `Cancelled` into a `JobStatus::Cancelled`
/// row update and a `JobStatus::Cancelled` return value; the test
/// dispatch path never sees `Cancelled` (no cancel token is wired in).
pub enum PoolDispatchResult {
    Outcome {
        outcome: argunix_build::BuildOutcome,
        phase_metrics: JobPhaseMetrics,
    },
    Cancelled,
}

/// Worker-side wrapper that translates `PoolDispatchResult::Cancelled`
/// into a `JobStore` row update + the `JobStatus::Cancelled` return.
async fn dispatch_build_via_pool(
    ctx: &WorkerContext,
    builder_name: &argunix_domain::BuilderName,
    job_id: JobId,
    drv_path: &str,
    gc_root: &Path,
    log_path: &Path,
    log_limit: argunix_build::LogCaptureLimit,
    cancel: &argunix_web::CancelToken,
) -> anyhow::Result<(argunix_build::BuildOutcome, JobPhaseMetrics)> {
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
        nix_bin: &ctx.nix_bin,
        live_logs: Some(ctx.live_logs.clone()),
    };
    match dispatch_pool_build(spec, Some(cancel)).await? {
        PoolDispatchResult::Outcome {
            outcome,
            phase_metrics,
        } => Ok((outcome, phase_metrics)),
        PoolDispatchResult::Cancelled => {
            <SqlxStore as JobStore>::finish(
                &ctx.store,
                job_id,
                JobStatus::Cancelled,
                Utc::now(),
                Some(&log_path.to_string_lossy()),
                None,
                &JobPhaseMetrics::default(),
            )
            .await?;
            Err(anyhow!("build cancelled"))
        }
    }
}

/// Daemon-side build orchestration for the dynamic builder pool.
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
    cancel: Option<&argunix_web::CancelToken>,
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
        nix_bin,
        live_logs,
    } = spec;
    let dispatcher = BuilderDispatcher::new(registry.clone());
    let cap = log_limit.max_raw_bytes;
    // Open the broadcast tap up-front so a fast UI can subscribe before
    // the first chunk arrives. The guard's `Drop` closes the registry
    // entry on every exit path so push-failure and dispatch-failure
    // returns don't leak entries.
    let live_log = live_logs.as_ref().map(|r| r.open(build_id));
    struct LiveLogGuard {
        registry: Option<Arc<argunix_web::LiveLogRegistry>>,
        build_id: i64,
    }
    impl Drop for LiveLogGuard {
        fn drop(&mut self) {
            if let Some(r) = &self.registry {
                r.close(self.build_id);
            }
        }
    }
    let _live_log_guard = LiveLogGuard {
        registry: live_logs.clone(),
        build_id,
    };

    let mut log_buf: Vec<u8> = Vec::new();
    let mut log_truncated = false;

    let early_failure = |log_buf: Vec<u8>, log_path: PathBuf, phase_metrics: JobPhaseMetrics| async move {
        argunix_build::write_zstd_log(&log_path, log_buf).await?;
        Ok::<_, anyhow::Error>(PoolDispatchResult::Outcome {
            outcome: argunix_build::BuildOutcome {
                status: argunix_build::BuildStatus::Failure,
                exit_code: None,
                output_paths: Vec::new(),
                log_path,
                log_truncated: false,
            },
            phase_metrics,
        })
    };

    // 1+2. Push the drv (and its closure) to the builder via
    //      `nix copy --to`. The closure is expanded automatically by
    //      `nix copy`, valid-paths are probed natively by the daemon
    //      protocol, and bytes stream per-file with bounded memory.
    //      Replaces the previous query-requisites + valid-paths
    //      probe + chunked-export dance.
    let phase_guard = PhaseGuard::new(registry.clone(), builder_name.clone(), build_id);
    phase_guard.set(BuildPhase::Push);
    let mut phase_metrics = JobPhaseMetrics::default();
    let push_started_at = std::time::Instant::now();
    let push_metrics = match nix_copy_over_pool(
        &dispatcher,
        builder_name,
        NixCopyDirection::To,
        &[drv_path.to_string()],
        nix_bin,
        build_id,
    )
    .await
    {
        Ok(m) => Some(m),
        Err(e) => {
            tracing::warn!(
                error = %e,
                builder = %builder_name,
                build_id,
                drv = drv_path,
                "nix copy --to (push drv closure) failed",
            );
            log_buf.extend_from_slice(
                format!(
                    "argunix: pushing drv closure to `{builder}` failed:\n\
                     {e}\n\
                     argunix: drv={drv_path}\n",
                    builder = builder_name,
                )
                .as_bytes(),
            );
            // Even on failure, record what we did push (= 0 or partial)
            // and how long we tried for. Useful when looking at
            // operations that flap between success and failure.
            phase_metrics.push_ms = Some(push_started_at.elapsed().as_millis() as u64);
            phase_metrics.push_bytes = Some(0);
            return early_failure(log_buf, log_path.to_path_buf(), phase_metrics).await;
        }
    };
    if let Some(m) = push_metrics {
        phase_metrics.push_bytes = Some(m.bytes_to_builder);
        phase_metrics.push_ms = Some(m.elapsed.as_millis() as u64);
    }
    phase_guard.set(BuildPhase::Build);

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
            log_buf.extend_from_slice(format!("argunix: dispatch_build failed: {e}\n").as_bytes());
            return early_failure(log_buf, log_path.to_path_buf(), phase_metrics).await;
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
    //    around `argunix_build::run_build`.
    let grace = Duration::from_secs(60);
    let timeout_deadline = tokio::time::Instant::now() + build_timeout + grace;
    let mut output_paths: Vec<String> = Vec::new();
    // Set in every loop-exit path (Finished / channel-closed / timeout)
    // before the loop breaks; read after the loop.
    let mut final_status: BuildOutcomeStatus;
    let mut exit_code: Option<i32> = None;
    let mut aborted = false;
    let mut timed_out = false;
    // Wall-clock between agent's `BuildStarted` and `BuildFinished`.
    // None until BuildStarted arrives.
    let mut build_started_at: Option<std::time::Instant> = None;
    loop {
        tokio::select! {
            biased;
            ev = lifecycle.recv() => {
                match ev {
                    Some(BuildLifecycle::Started { pid }) => {
                        build_started_at = Some(std::time::Instant::now());
                        tracing::info!(job_id = build_id, ?pid, builder = %builder_name, "agent started build");
                    }
                    Some(BuildLifecycle::LogChunk { bytes }) => {
                        if let Some(ref tap) = live_log {
                            tap.push(&bytes);
                        }
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
                        if let Some(started) = build_started_at {
                            phase_metrics.build_ms =
                                Some(started.elapsed().as_millis() as u64);
                        }
                        break;
                    }
                    None => {
                        tracing::warn!(job_id = build_id, builder = %builder_name, "lifecycle channel closed before BuildFinished");
                        log_buf.extend_from_slice(b"\nargunix: builder disconnected mid-build.\n");
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
                        "\nargunix: build timed out after {}s; sending Abort to builder.\n",
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
                    b"argunix: agent unresponsive after Abort; synthesising Killed outcome.\n",
                );
                final_status = BuildOutcomeStatus::Killed;
                break;
            }
        }
    }
    registry.unregister_in_flight_build(builder_name, build_id);
    drop(live_log);

    if aborted || (final_status == BuildOutcomeStatus::Killed && !timed_out) {
        // Operator-initiated cancel (or agent-emitted Killed before
        // we asked) → surface as JobStatus::Cancelled via the
        // worker's wrapper.
        if log_truncated {
            log_buf.extend_from_slice(b"\n--- log truncated by argunix ---\n");
        }
        argunix_build::write_zstd_log(log_path, log_buf).await?;
        return Ok(PoolDispatchResult::Cancelled);
    }
    if timed_out {
        // Wall-clock timeout: not a user cancel, surface as Failure
        // so the eval rolls up correctly and the job row gets the
        // right terminal status.
        if log_truncated {
            log_buf.extend_from_slice(b"\n--- log truncated by argunix ---\n");
        }
        argunix_build::write_zstd_log(log_path, log_buf).await?;
        return Ok(PoolDispatchResult::Outcome {
            outcome: argunix_build::BuildOutcome {
                status: argunix_build::BuildStatus::Failure,
                exit_code,
                output_paths: Vec::new(),
                log_path: log_path.to_path_buf(),
                log_truncated,
            },
            phase_metrics,
        });
    }

    // 5. On success, pull the output closure into the local store
    //    via `nix copy --from`. Closure expansion, valid-path
    //    deduplication, and per-file streaming are all handled by
    //    the daemon protocol — no chunking, no topo sort, no
    //    explicit memory throttle needed.
    if final_status == BuildOutcomeStatus::Success && !output_paths.is_empty() {
        phase_guard.set(BuildPhase::Pull);
        let pull_started_at = std::time::Instant::now();
        match nix_copy_over_pool(
            &dispatcher,
            builder_name,
            NixCopyDirection::From,
            &output_paths,
            nix_bin,
            build_id,
        )
        .await
        {
            Ok(m) => {
                phase_metrics.pull_bytes = Some(m.bytes_from_builder);
                phase_metrics.pull_ms = Some(m.elapsed.as_millis() as u64);
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
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    build_id,
                    builder = %builder_name,
                    "nix copy --from (pull output closure) failed",
                );
                log_buf.extend_from_slice(b"\nargunix: pulling output closure failed:\n");
                log_buf.extend_from_slice(format!("{e}\n").as_bytes());
                phase_metrics.pull_ms = Some(pull_started_at.elapsed().as_millis() as u64);
                phase_metrics.pull_bytes = Some(0);
                final_status = BuildOutcomeStatus::Failure;
            }
        }
    }

    if log_truncated {
        log_buf.extend_from_slice(b"\n--- log truncated by argunix ---\n");
    }
    argunix_build::write_zstd_log(log_path, log_buf).await?;

    Ok(PoolDispatchResult::Outcome {
        outcome: argunix_build::BuildOutcome {
            status: match final_status {
                BuildOutcomeStatus::Success => argunix_build::BuildStatus::Success,
                _ => argunix_build::BuildStatus::Failure,
            },
            exit_code,
            output_paths,
            log_path: log_path.to_path_buf(),
            log_truncated,
        },
        phase_metrics,
    })
}

/// Wait on an optional cancel token. Returns immediately when fired;
/// stays pending forever when `cancel` is `None`.
async fn wait_cancelled(cancel: Option<&argunix_web::CancelToken>) {
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
    sha: &argunix_domain::Sha,
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
/// its many error paths. Arc-based so the same guard can be moved into
/// a spawned task — the build phase (which runs detached from the
/// eval task) is responsible for deregistering its own cancel token
/// when it terminates, so cancel-on-push can still find the eval
/// mid-build.
struct CancelGuard {
    registry: Arc<CancelRegistry>,
    eval_id: EvalId,
}

impl Drop for CancelGuard {
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
    use argunix_builders::{BuilderRegistry, ConnState, ConnectedBuilder};
    use argunix_domain::{AttrPath, BuilderCapabilities, BuilderId, BuilderName, JobStatus};

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

    fn spec(system: Option<&str>, required: &[&str]) -> argunix_eval::JobSpec {
        argunix_eval::JobSpec {
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
