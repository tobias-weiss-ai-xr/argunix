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
use argunix_domain::{EvalId, EvalStatus, ImageFormat, JobId, JobStatus, RepoId, Sha, Slug};
use argunix_effects::{Effect, EffectStatus, OutputContext, Severity};
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

/// RAII backstop that guarantees a job marked `Running` cannot linger
/// in that state if `build_one` unwinds before writing a terminal
/// status — a `?` error out of dispatch, or the whole build-phase
/// task being dropped on cancellation. On drop it spawns a task that
/// calls [`JobStore::interrupt_if_running`]; the conditional
/// `WHERE status = 'running'` makes that a no-op once a real verdict
/// (`finish`) has landed, so the guard never needs disarming. Process
/// death is the one case it cannot cover — the boot-time
/// `mark_running_interrupted` pass handles that. Together they keep
/// the read-only UI's "building right now" honest across a restart.
struct RunningJobGuard {
    store: argunix_store::SqlxStore,
    job_id: JobId,
}

impl RunningJobGuard {
    fn arm(store: argunix_store::SqlxStore, job_id: JobId) -> Self {
        Self { store, job_id }
    }
}

impl Drop for RunningJobGuard {
    fn drop(&mut self) {
        let store = self.store.clone();
        let job_id = self.job_id;
        tokio::spawn(async move {
            match <argunix_store::SqlxStore as JobStore>::interrupt_if_running(&store, job_id).await
            {
                Ok(true) => tracing::warn!(
                    job_id = job_id.get(),
                    "build_one exited without a verdict; job flipped running -> interrupted",
                ),
                Ok(false) => {}
                Err(e) => tracing::warn!(
                    error = %e,
                    job_id = job_id.get(),
                    "failed to interrupt an abandoned running job",
                ),
            }
        });
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
    /// How long `build_one` waits for an eligible pool builder before
    /// giving up on a job and marking it `Interrupted`. See
    /// [`argunix_config::schema::Schedule::builder_wait_seconds`].
    pub builder_wait: Duration,
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
    /// Where converted docker-image blobs and per-build manifests
    /// live. The HTTP `/v2/...` registry surface reads from the same
    /// path, so daemon and web server must agree.
    pub registry_state: Arc<argunix_registry::RegistryState>,
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
        let clone_creds = provider.clone_credentials();
        let clone_fut = clone_repo(
            &clone_url,
            &eval.sha,
            &work_dir,
            ctx.clone_timeout,
            clone_creds.as_ref(),
        );
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

    let push_caches: Vec<argunix_build::PushCache> = snap
        .config
        .binary_caches
        .iter()
        .map(|c| argunix_build::PushCache {
            url: c.push_url.clone(),
            signing_key_path: c.signing_key_path.path().to_path_buf(),
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

    // Post-build registry-push effects for this repo, resolved from
    // the `registries` catalog. Built once per eval and cloned into
    // each per-job build task, mirroring `push_caches`.
    let registry_effects: Vec<Arc<dyn Effect>> = repo_cfg
        .map(|r| crate::effects::registry_push_effects(&snap.config, r))
        .unwrap_or_default();

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
            push_caches,
            registry_effects,
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
    push_caches: Vec<argunix_build::PushCache>,
    registry_effects: Vec<Arc<dyn Effect>>,
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
    // Image jobs sharing a logical name across systems — their per-job
    // `registry-push` is suppressed; the post-build multi-arch fan-in
    // (or, for an `oci` clash, an errored effect) handles them instead.
    // See `design/multi-arch.md`.
    let suppressed_push_ids = crate::multiarch::suppressed_push_job_ids(&specs_by_id);
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
        // True iff the spawn pass below ran out of dispatchable work
        // (`dispatch()` returned None) *while still holding a build
        // permit* — as opposed to stopping because permits were
        // exhausted. Drives the spin-proof wait further down.
        let mut strategy_drained_with_permit = false;
        // Spawn while we have permits and the strategy has Ready Steps.
        if !cancel.is_cancelled() {
            loop {
                let permit = match global_sem.clone().try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => break, // No global permits free.
                };
                let Some(d) = strategy.dispatch() else {
                    drop(permit);
                    strategy_drained_with_permit = true;
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
                let is_multiarch_member = suppressed_push_ids.contains(&job_id);
                let ctx_c = ctx.clone();
                let cancel_c = cancel.clone();
                let push_caches_c = push_caches.clone();
                let registry_effects_c = registry_effects.clone();
                let repo_c = repo.clone();
                let eval_c = eval.clone();
                let span = info_span!(
                    "job",
                    job_id = job_id.get(),
                    attr = %spec.attr_path,
                );
                set.spawn(async move {
                    let _permit = permit; // released on drop
                    // Run `build_one` in a nested task so a panic inside
                    // it surfaces here as a `JoinError` we convert to an
                    // `Err` outcome, rather than propagating out and
                    // destroying *this* task — which would lose `token`
                    // and orphan the panicked job's DAG dependents
                    // (they'd never cascade-skip). See bugs.md COR-6.
                    let spec_for_build = spec.clone();
                    let inner = tokio::spawn(
                        async move {
                            build_one(
                                &ctx_c,
                                &repo_c,
                                &eval_c,
                                job_id,
                                &spec_for_build,
                                &push_caches_c,
                                &registry_effects_c,
                                collapsed_mode,
                                is_multiarch_member,
                                &cancel_c,
                            )
                            .await
                        }
                        .instrument(span),
                    );
                    let res = match inner.await {
                        Ok(r) => r,
                        Err(join_err) => Err(anyhow!("build task panicked: {join_err}")),
                    };
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

        // Spin guard. With nothing in flight, falling into the
        // `select!` below would poll `set.join_next()` on an empty
        // `JoinSet` — which resolves to `Ready(None)` *immediately* —
        // turning this loop into a 100%-CPU busy-spin that starves the
        // async runtime and wedges the whole daemon.
        //
        // The checks above already proved the strategy still has
        // dispatchable work and we are not cancelled, so the spawn
        // pass added nothing for exactly one of two reasons:
        //
        //  * `strategy_drained_with_permit` — `dispatch()` yielded
        //    nothing although a build permit was free. The strategy is
        //    wedged: a drained strategy must report
        //    `pending_count() == 0`, so this is a scheduler bug. Fail
        //    this eval's build phase loudly rather than pin a core
        //    forever.
        //  * otherwise — the global build semaphore is fully held by
        //    other evals. Block on a permit (raced with cancel) so the
        //    loop sleeps instead of spinning, then retry.
        if set.is_empty() {
            if strategy_drained_with_permit {
                tracing::error!(
                    eval_id = eval_id.get(),
                    pending = strategy.pending_count(),
                    "build scheduler wedged — dispatchable work remains but \
                     dispatch() yielded nothing; aborting this eval's build phase",
                );
                break 'outer;
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {}
                _ = global_sem.acquire() => {}
            }
            continue 'outer;
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
                // Panics in `build_one` are now caught by the nested task
                // (see the spawn site) and surface as an `Err` outcome
                // with the token preserved, so this arm is only reached
                // if the *outer* task itself is aborted (e.g. runtime
                // teardown). We can't recover the token here, but the
                // strategy is dropped at end-of-function anyway; record a
                // failure so the tally stays meaningful.
                tracing::error!(error = %join_err, "build wrapper task failed to join");
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

    // Cross-system multi-arch fan-in: per-arch `docker` image jobs of
    // one logical name get stitched into a multi-arch OCI index.
    run_multiarch_fan_in(&ctx, &repo, &eval, eval_id, &specs_by_id).await;

    // An eval whose jobs could not be built (no matching builder, or a
    // builder that never returned within the retry budget) must not read
    // as green. `failure` maps to a red Failure; interruptions that were
    // never resolved map to Error so the commit does not show Success for
    // work that never ran. See bugs.md COR-1.
    let overall_state = if tally.failure > 0 {
        CheckState::Failure
    } else if tally.interrupted > 0 {
        CheckState::Error
    } else {
        CheckState::Success
    };
    let mut description = format!(
        "{} ok, {} cached, {} failed",
        tally.success, tally.cached, tally.failure,
    );
    if tally.interrupted > 0 {
        description.push_str(&format!(", {} interrupted", tally.interrupted));
    }
    if tally.cancelled > 0 {
        description.push_str(&format!(", {} cancelled", tally.cancelled));
    }
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

    // Backstop the fire-and-forget per-job posts: re-assert every job's
    // terminal state from the DB once, now that the build phase is done.
    // A dropped terminal post would otherwise leave a built job stuck
    // showing "queued" on the forge.
    if !collapsed_mode {
        reconcile_forge_checks(&ctx, &provider, &repo, &eval, eval_id).await;
    }

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
/// `BuilderRegistry::eligible(system, required_features, exclude)` and
/// takes the first entry — which `eligible()` already sorts
/// least-loaded-first. Returns `None` when:
///
/// - the spec has no `system` (we can't filter and shouldn't guess);
/// - no connected builder advertises the system *and* every required
///   feature *and* has free `max_jobs` capacity right now;
/// - every such builder is in `exclude` — `build_one` populates this
///   with builders that already hit a transport failure for this job,
///   so a flapping builder isn't retried in a tight loop.
///
/// In the None case the caller falls through to a local
/// `nix-store --realise` (no `--builders`), which honours the host's
/// `nix.buildMachines` if any. The pre-flight earlier in `build_one`
/// has already failed-fast for the unsatisfiable-features subset.
fn pick_builder_for_spec(
    registry: &argunix_builders::BuilderRegistry,
    spec: &argunix_eval::JobSpec,
    exclude: &std::collections::HashSet<u64>,
) -> Option<argunix_builders::BuilderSnapshot> {
    let system = spec.system.as_deref()?;
    let eligible = registry.eligible(system, &spec.required_system_features, exclude);
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
    interrupted: usize,
    cancelled: usize,
}

impl JobTally {
    fn record(&mut self, status: JobStatus) {
        match status {
            JobStatus::Success => self.success += 1,
            JobStatus::Cached => self.cached += 1,
            JobStatus::Failure => self.failure += 1,
            JobStatus::Interrupted => self.interrupted += 1,
            JobStatus::Cancelled => self.cancelled += 1,
            JobStatus::SkippedNoBuilder | JobStatus::Queued | JobStatus::Running => {}
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

/// Re-assert every job's terminal forge check from the database, once,
/// after the build phase finishes. Per-job posts are fire-and-forget;
/// if a terminal one is dropped (a network blip, a 429 burst) the job
/// is left showing "queued"/"pending" on the forge forever even though
/// it built fine. This sweep reads the authoritative status from the DB
/// and re-posts it: the forge swallows the no-op transitions for jobs
/// already correct (GitLab 400s them — see `gitlab.rs`; GitHub/Forgejo
/// accept them idempotently), and flips the stragglers to their true
/// state. Only the build/job checks are reconciled — effect outcomes no
/// longer post forge checks. Caller skips this in collapsed mode, where
/// per-job checks don't exist.
async fn reconcile_forge_checks(
    ctx: &WorkerContext,
    provider: &Arc<dyn Provider>,
    repo: &argunix_store::RepoRecord,
    eval: &argunix_store::EvalRecord,
    eval_id: EvalId,
) {
    let rows = match <SqlxStore as JobStore>::list_by_eval(&ctx.store, eval_id).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                error = %e,
                eval_id = eval_id.get(),
                "reconcile: list_by_eval failed; skipping forge-check sweep",
            );
            return;
        }
    };
    for row in rows {
        // Only terminal jobs have a settled state to assert. A job still
        // Queued/Running here (shouldn't happen post-build) has nothing
        // to reconcile to. `post_per_job_check` itself no-ops on the
        // non-terminal and SkippedNoBuilder states.
        if !row.status.is_terminal() {
            continue;
        }
        // Eval-time errors never produced a drv_path (nix-eval-jobs
        // errored before one existed) — reuse that to label the check
        // the same way the live completion path does.
        let had_eval_error = row.drv_path.is_none();
        post_per_job_check(
            ctx,
            provider,
            &repo.forge,
            &repo.slug,
            &eval.sha,
            eval_id,
            row.attr_path.as_str(),
            row.status,
            had_eval_error,
        );
    }
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

/// A forge-post error worth retrying: a network blip, or a server-side
/// status the forge might recover from (429 rate-limit, any 5xx). A 4xx
/// other than 429 is a request the forge will keep rejecting (and the
/// one benign 4xx — GitLab's "Cannot transition" 400 — is already
/// swallowed inside `post_check`, returning `Ok`), so retrying it just
/// wastes calls. `Unauthorised` is handled separately (it pauses the
/// forge), so it never reaches here.
fn is_transient_forge_error(e: &ForgeError) -> bool {
    match e {
        ForgeError::Http(_) => true,
        ForgeError::Api { status, .. } => *status == 429 || *status >= 500,
        _ => false,
    }
}

/// Skip post_check entirely if the forge is paused; mark the forge
/// healthy on a successful post and pause it on 401. See
/// [docs/concepts/forge-pause.md].
///
/// Transient failures (network, 429, 5xx) are retried with exponential
/// backoff before giving up. A status post is otherwise pure
/// fire-and-forget, so a single dropped *terminal* post leaves a job
/// stuck showing "queued" on the forge even though it built fine. Retry
/// shrinks that window; the end-of-eval `reconcile_forge_checks` sweep
/// is the backstop when every retry here is exhausted.
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
        const MAX_ATTEMPTS: u32 = 4;
        let mut backoff = Duration::from_millis(500);
        for attempt in 1..=MAX_ATTEMPTS {
            match provider.post_check(post.clone()).await {
                Ok(_) => {
                    pauses.mark_healthy(&forge_name);
                    return;
                }
                Err(ForgeError::Unauthorised) => {
                    pauses.pause(&forge_name, "401 from post_check");
                    return;
                }
                Err(e) if is_transient_forge_error(&e) && attempt < MAX_ATTEMPTS => {
                    tracing::debug!(
                        forge = %forge_name,
                        error = %e,
                        attempt,
                        "forge post_check transient error; retrying after backoff",
                    );
                    tokio::time::sleep(backoff).await;
                    backoff *= 2;
                }
                Err(e) => {
                    tracing::warn!(forge = %forge_name, error = %e, "forge post_check failed");
                    return;
                }
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
/// Jobs whose full spec was persisted (`spec_json`, see bugs.md COR-4)
/// rehydrate verbatim — preserving `image_format`, `meta`, `outputs`, and
/// `required_system_features`, so resumed image jobs still push to the
/// registry, attach SBOMs, and participate in the multi-arch fan-in.
///
/// Rows written before the `spec_json` column existed fall back to a
/// lossy reconstruction that loses `outputs` / `required_system_features`
/// (cache-skip and feature pre-flight are then missed — acceptable on
/// crash recovery: a cache-miss just re-builds, a feature mismatch
/// surfaces as a normal build failure).
async fn load_jobs_for_resume(
    store: &SqlxStore,
    eval_id: EvalId,
) -> anyhow::Result<Vec<(argunix_eval::JobSpec, JobId)>> {
    use argunix_domain::AttrPath;
    // Primary path: rehydrate the full spec from the persisted JSON.
    let specs = <SqlxStore as JobStore>::resume_specs_for_eval(store, eval_id)
        .await
        .with_context(|| format!("loading job specs for resumed eval {}", eval_id.get()))?;
    let mut by_id: std::collections::HashMap<JobId, argunix_eval::JobSpec> =
        std::collections::HashMap::with_capacity(specs.len());
    for (id, json) in specs {
        match serde_json::from_str::<argunix_eval::JobSpec>(&json) {
            Ok(spec) => {
                by_id.insert(id, spec);
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    job_id = id.get(),
                    "persisted job spec failed to deserialize; falling back to lossy reconstruction",
                );
            }
        }
    }

    let rows = <SqlxStore as JobStore>::list_by_eval(store, eval_id)
        .await
        .with_context(|| format!("loading jobs for resumed eval {}", eval_id.get()))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        if row.status != JobStatus::Queued {
            continue;
        }
        let spec = by_id
            .remove(&row.id)
            .unwrap_or_else(|| argunix_eval::JobSpec {
                attr_path: AttrPath::new(row.attr_path.as_str().to_string()),
                drv_path: row.drv_path.clone(),
                system: Some(row.system.clone()),
                error: None,
                outputs: std::collections::BTreeMap::new(),
                meta: serde_json::Value::Null,
                is_cached: false,
                required_system_features: Vec::new(),
                image_format: None,
            });
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
    // `meta.mainProgram` (when nix-eval-jobs's `--meta` includes it) lets
    // the synthetic-flake endpoint emit a working `apps.<sys>.<attr>` for
    // `nix run`. Missing for most derivations — that's fine, those
    // attrs just won't show up under `apps` in the synthetic flake.
    let main_program = spec
        .meta
        .get("mainProgram")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let job_id = <SqlxStore as JobStore>::create(
        store,
        argunix_store::NewJob {
            eval_id,
            attr_path: spec.attr_path.clone(),
            drv_path: spec.drv_path.clone(),
            system,
            main_program,
            outputs: spec.outputs.clone(),
        },
    )
    .await?;
    // Persist the whole spec as JSON so crash-resume can rehydrate it
    // verbatim (image_format, meta, required_system_features) rather than
    // lossily reconstructing it from columns. Best-effort: a failure here
    // only degrades resume to the lossy path for this one job. COR-4.
    match serde_json::to_string(spec) {
        Ok(json) => {
            if let Err(e) = <SqlxStore as JobStore>::set_spec_json(store, job_id, &json).await {
                tracing::warn!(error = %e, attr = %spec.attr_path, "failed to persist job spec_json");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, attr = %spec.attr_path, "failed to serialize job spec for resume")
        }
    }
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
#[allow(clippy::too_many_arguments)]
async fn build_one(
    ctx: &WorkerContext,
    repo: &argunix_store::RepoRecord,
    eval: &argunix_store::EvalRecord,
    job_id: JobId,
    spec: &argunix_eval::JobSpec,
    push_caches: &[argunix_build::PushCache],
    registry_effects: &[Arc<dyn Effect>],
    collapsed_mode: bool,
    is_multiarch_member: bool,
    cancel: &argunix_web::CancelToken,
) -> anyhow::Result<JobStatus> {
    let repo_id = repo.id;
    let eval_id = eval.id;
    if spec.error.is_some() {
        return Ok(JobStatus::Failure);
    }
    let Some(drv_path) = spec.drv_path.clone() else {
        return Ok(JobStatus::Failure);
    };

    // `is_cached` is set by `nix-eval-jobs --check-cache-status` when
    // the output is already valid locally or fetchable from a
    // configured system-wide substituter. Short-circuit before any
    // builder dispatch — argunix no longer keeps its own pre-build
    // cache probe; `nix.settings.substituters` on the host is the
    // single source of truth.
    if spec.is_cached {
        if let Some(output) = spec.primary_output() {
            tracing::info!(job_id = job_id.get(), output = %output, "local store hit");
            let output = output.to_string();
            <SqlxStore as JobStore>::finish(
                &ctx.store,
                job_id,
                JobStatus::Cached,
                Utc::now(),
                None,
                Some(&output),
                &JobPhaseMetrics::default(),
            )
            .await?;
            // Post-build effects run for cache hits too — a cached
            // image still needs to reach the external registry.
            spawn_post_build_effects(
                ctx,
                repo,
                eval,
                spec,
                job_id,
                push_caches,
                registry_effects,
                collapsed_mode,
                is_multiarch_member,
                vec![output],
            );
            return Ok(JobStatus::Cached);
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
    //
    // This tests *capability*, not capacity: `any_matching_builder`
    // ignores `max_jobs`, so a build whose only obstacle is that every
    // capable builder is momentarily busy is NOT failed here — it falls
    // through to the dispatch loop, which queues on the capacity-wait
    // path. Using the capacity-sensitive `eligible` here would fail
    // such a job fast the instant all (often single-slot) builders are
    // occupied, even though they advertise every required feature.
    if !spec.required_system_features.is_empty() {
        if let Some(system) = spec.system.as_deref() {
            let has_capable_builder = ctx.builder_registry.any_matching_builder(
                system,
                &spec.required_system_features,
                &std::collections::HashSet::new(),
            );
            if !has_capable_builder {
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

    // Backstop: the only way a job becomes `running` from here is via
    // `JobStore::dispatch` inside the loop below. If `dispatch_pool_build`
    // then unwinds before a terminal `finish` — a `?` error, the build
    // phase task dropped on cancellation — this guard's conditional
    // `interrupt_if_running` flips the row out of `running` so the UI's
    // "building now" never carries a phantom entry. The guard is safe
    // to arm pre-`dispatch`: its UPDATE is gated on `status = 'running'`
    // and is a no-op on queued / terminal rows.
    let _running_guard = RunningJobGuard::arm(ctx.store.clone(), job_id);

    // Pre-create the gcroot parent dir so `nix-store --add-root` (run
    // by the agent on the picked builder) can drop the symlink
    // atomically with the build.
    let gc_root = argunix_build::gc_root_path(&ctx.gc_root_dir, repo_id, eval_id, job_id);
    if let Some(parent) = gc_root.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            tracing::warn!(error = %e, dir = %parent.display(), "failed to create gcroot parent dir; build will run without a gcroot");
        }
    }

    // Dispatch loop. `pick_builder_for_spec` returns the least-loaded
    // eligible builder not in `excluded`; `eligible()` already filters
    // by (system, requiredSystemFeatures, max_jobs cap). On a transport
    // failure — the builder disconnected before a verdict — we add its
    // *connection_id* to `excluded` and pick the next one. Excluding by
    // connection (not name) is deliberate: a builder that drops and
    // reconnects comes back with a fresh connection_id, so it is
    // eligible for retry again — without this, a sole builder for a
    // system that briefly reconnected stayed excluded for the rest of
    // the dispatch and the job was needlessly `Interrupted`. A genuine
    // verdict ends the loop. When no eligible builder remains, the job
    // is marked `Interrupted` and `build_one` returns: the coordinator
    // has no
    // local build path, so any execution must go through the pool. If
    // the operator wants the coordinator host to also build, they
    // enrol `argunix-builder` on it as a loopback builder — it then
    // appears in the pool like any other and is picked here normally.
    //
    // Transport-exhaustion: each transport-failed builder is excluded
    // after at most one attempt, so the loop terminates once the
    // matching set is exhausted via that path. The `None` branch
    // distinguishes two cases: matching builders are at `max_jobs`
    // (normal queueing — wait), versus no matching builder exists at
    // all (interrupt, with an opt-in enrolment grace via
    // `schedule.builder_wait_seconds`).
    //
    // Cancellation: `dispatch_build_via_pool` handles cancel internally
    // (it sends `Abort` and drains `BuildFinished{Killed}`); the wait
    // loops in the `None` branch also honour the cancel token. See
    // [docs/concepts/cancel-on-push.md].
    // Connection ids of builders that transport-failed this dispatch.
    let mut excluded: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut builder_wait_used = false;
    let (outcome, phase_metrics) = loop {
        match pick_builder_for_spec(&ctx.builder_registry, spec, &excluded) {
            Some(b) => {
                // Reserve the slot before recording dispatch so a
                // concurrent worker sees the up-to-date in_flight
                // number. `_slot` drops at the end of the iteration —
                // including the retry `continue`.
                let _slot = BuilderSlot::reserve(ctx.builder_registry.clone(), b.name.clone());
                // Surfaces the chosen builder in the read-only UI's
                // running table and keeps per-builder running counts
                // grouped from the DB honest.
                <SqlxStore as JobStore>::dispatch(&ctx.store, job_id, b.builder_id, Utc::now())
                    .await?;
                tracing::info!(
                    eval_id = eval_id.get(),
                    drv = %drv_path,
                    log = %log_path.display(),
                    pinned_builder = %b.name,
                    "dispatching build",
                );
                // Dispatch via the dynamic builder pool through side
                // channels. The helper drives push-closure → Build
                // → drain lifecycle → pull-closure → register-gcroot
                // entirely; the `cancel` token is honoured inside.
                match dispatch_build_via_pool(
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
                {
                    PoolAttempt::Verdict(outcome, phase_metrics) => {
                        break (outcome, phase_metrics);
                    }
                    PoolAttempt::TransportFailure => {
                        tracing::warn!(
                            job_id = job_id.get(),
                            builder = %b.name,
                            connection_id = b.connection_id,
                            "pool dispatch hit a transport failure; excluding this \
                             connection and retrying (a reconnect gets a fresh \
                             connection_id and is eligible again)",
                        );
                        excluded.insert(b.connection_id);
                        continue;
                    }
                }
            }
            None => {
                // No eligible pool builder remains — either none ever
                // matched (wrong system / required-feature combination
                // the pool can't satisfy right now) or every one that
                // did has been excluded by a transport failure during
                // this dispatch. The coordinator does not build
                // locally: there is no second code path that could
                // satisfy this.
                //
                // Before giving up, optionally wait briefly for an
                // eligible builder to (re)enrol. This bridges the
                // canonical race where the daemon's resume pass
                // re-dispatches a job microseconds after startup, but
                // the (loopback or remote) agent has not yet
                // reconnected. Opt-in via
                // `schedule.builder_wait_seconds`; default 0 keeps
                // existing behaviour and the sandbox tests fast.
                let spec_system = spec.system.as_deref().unwrap_or("");
                let has_match = ctx.builder_registry.any_matching_builder(
                    spec_system,
                    &spec.required_system_features,
                    &excluded,
                );
                if has_match {
                    // Capacity wait: matching builders exist but every
                    // one is at `max_jobs`. This is normal queueing,
                    // not a failure — sit on the cancel/poll loop until
                    // a slot opens. If the last matching builder
                    // disconnects mid-wait we fall through to the
                    // no-match path on the next iteration.
                    tracing::debug!(
                        job_id = job_id.get(),
                        spec_system = ?spec.system,
                        "all matching builders at capacity; waiting for a slot",
                    );
                    loop {
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => {
                                tracing::info!(
                                    job_id = job_id.get(),
                                    "cancelled while waiting for builder capacity",
                                );
                                <SqlxStore as JobStore>::finish(
                                    &ctx.store, job_id, JobStatus::Cancelled,
                                    Utc::now(), None, None, &JobPhaseMetrics::default(),
                                ).await?;
                                return Ok(JobStatus::Cancelled);
                            }
                            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                        }
                        if !ctx
                            .builder_registry
                            .eligible(spec_system, &spec.required_system_features, &excluded)
                            .is_empty()
                        {
                            break;
                        }
                        if !ctx.builder_registry.any_matching_builder(
                            spec_system,
                            &spec.required_system_features,
                            &excluded,
                        ) {
                            break;
                        }
                    }
                    continue;
                }
                // No matching builder at all. Optionally wait briefly
                // for one to enrol — opt-in via builder_wait_seconds,
                // useful for the post-restart reconnect race.
                if !builder_wait_used && !ctx.builder_wait.is_zero() {
                    builder_wait_used = true;
                    let deadline = tokio::time::Instant::now() + ctx.builder_wait;
                    tracing::info!(
                        job_id = job_id.get(),
                        spec_system = ?spec.system,
                        wait_secs = ctx.builder_wait.as_secs(),
                        "no matching builder yet; waiting for one to enrol",
                    );
                    let arrived = loop {
                        if ctx.builder_registry.any_matching_builder(
                            spec_system,
                            &spec.required_system_features,
                            &excluded,
                        ) {
                            break true;
                        }
                        if tokio::time::Instant::now() >= deadline {
                            break false;
                        }
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => {
                                tracing::info!(
                                    job_id = job_id.get(),
                                    "cancelled while waiting for a matching builder",
                                );
                                <SqlxStore as JobStore>::finish(
                                    &ctx.store, job_id, JobStatus::Cancelled,
                                    Utc::now(), None, None, &JobPhaseMetrics::default(),
                                ).await?;
                                return Ok(JobStatus::Cancelled);
                            }
                            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                        }
                    };
                    if arrived {
                        continue;
                    }
                }
                tracing::warn!(
                    job_id = job_id.get(),
                    tried = excluded.len(),
                    spec_system = ?spec.system,
                    "no matching pool builder; marking job interrupted for retry \
                     (enrol a builder on this host as a loopback if the coordinator \
                     should also build)",
                );
                <SqlxStore as JobStore>::finish(
                    &ctx.store,
                    job_id,
                    JobStatus::Interrupted,
                    Utc::now(),
                    None,
                    None,
                    &JobPhaseMetrics::default(),
                )
                .await?;
                return Ok(JobStatus::Interrupted);
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
        argunix_build::BuildStatus::Success => {
            // gcroot was already registered by `nix-store --realise --add-root`
            // above (atomic with the build) — nothing else to do here.
            let primary = outcome
                .output_paths
                .first()
                .cloned()
                .or_else(|| spec.primary_output().map(String::from));

            // Mark the job done *before* the cache push so the UI flips
            // green as soon as the build artifact is back on the
            // coordinator. A 388 MB nix-shell to S3 can take minutes
            // and would otherwise leave the job rendering as "still
            // building" with a `total` that silently includes the
            // upload. Publish is best-effort anyway (failures don't
            // fail the job), so detaching it from the job lifecycle
            // costs us nothing semantically — a flaky cache still logs
            // a warning, just without holding the UI hostage.
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

            // Post-build effects (binary-cache push + external
            // registry push), detached so a slow push doesn't hold the
            // build loop's concurrency slot. Each attempt is recorded
            // in `effect_runs`.
            spawn_post_build_effects(
                ctx,
                repo,
                eval,
                spec,
                job_id,
                push_caches,
                registry_effects,
                collapsed_mode,
                is_multiarch_member,
                outcome.output_paths.clone(),
            );

            // Internal embedded registry (argunix's own read-only
            // `/v2` surface) — independent of, and complementary to,
            // the external registry push above. Awaited inline since
            // it only writes to the local blob pool.
            //
            // Only `docker` images are ingested here: the embedded
            // registry's converter is single-manifest, so an `oci`
            // (potentially multi-arch) image is distributed solely via
            // the `registry-push` effect — see `argunix-effects`.
            match spec.image_format {
                Some(ImageFormat::Docker) => {
                    try_publish_docker_image(
                        &ctx.store,
                        &ctx.registry_state,
                        repo_id,
                        eval_id,
                        job_id,
                        spec,
                        primary.as_deref(),
                    )
                    .await;
                }
                Some(ImageFormat::Oci) => {
                    tracing::info!(
                        job_id = job_id.get(),
                        "oci image: embedded registry publish skipped \
                         (oci images are distributed via the registry-push effect)",
                    );
                }
                None => {}
            }

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

/// Spawn the detached post-build effects task: binary-cache push +
/// external registry push, each recorded in `effect_runs`. Detached so
/// a slow push (minutes, for a multi-GB closure or a layered image)
/// doesn't hold the build loop's concurrency slot. Called from both
/// the fresh-`Success` and the `Cached` paths — a cached image still
/// needs to reach the registry.
#[allow(clippy::too_many_arguments)]
fn spawn_post_build_effects(
    ctx: &WorkerContext,
    repo: &argunix_store::RepoRecord,
    eval: &argunix_store::EvalRecord,
    spec: &argunix_eval::JobSpec,
    job_id: JobId,
    push_caches: &[argunix_build::PushCache],
    registry_effects: &[Arc<dyn Effect>],
    collapsed_mode: bool,
    is_multiarch_member: bool,
    output_paths: Vec<String>,
) {
    // A container image always has post-build work even with no caches
    // / registries configured: its size and SBOM are recorded
    // regardless (both `docker` and `oci` archives carry a closure).
    let is_image = spec.image_format.is_some();
    if output_paths.is_empty()
        || (push_caches.is_empty() && registry_effects.is_empty() && !is_image)
    {
        return;
    }
    let store = ctx.store.clone();
    let caches: Vec<argunix_build::PushCache> = push_caches.to_vec();
    // A job that is one arch slice of a multi-arch group must run
    // neither its own `registry-push` (it would race the shared tags)
    // nor its own `sbom-attach` (that would bind a per-arch SBOM to the
    // index tag). The post-build fan-in pushes the assembled index and
    // attaches a per-arch SBOM to each per-arch manifest digest.
    let reg_effects: Vec<Arc<dyn Effect>> = registry_effects
        .iter()
        .filter(|e| !(is_multiarch_member && matches!(e.kind(), "registry-push" | "sbom-attach")))
        .cloned()
        .collect();
    let forge = repo.forge.clone();
    let slug = repo.slug.clone();
    let sha = eval.sha.clone();
    let eval_id = eval.id;
    let default_branch = repo.default_branch.clone();
    let git_ref = eval.git_ref.clone();
    let attr_path = spec.attr_path.as_str().to_string();
    let system = spec.system.clone().unwrap_or_else(|| "unknown".to_string());
    let image_format = spec.image_format;
    let sbom_roots = argunix_effects::sbom::runtime_roots(&spec.meta);
    // Resolved up front so the spawned task doesn't borrow the config
    // snapshot. `Reported` effects post their own forge check through
    // this provider.
    let snap = ctx.current.load();
    let provider = snap.providers.get(&repo.forge).cloned();
    let external_url = snap.config.external_url.clone();
    drop(snap);
    let pauses = ctx.pauses.clone();
    tokio::spawn(
        async move {
            // Record image size + persist the CycloneDX SBOM for any
            // container image, before any push — independent of effect
            // config.
            if image_format.is_some() {
                crate::effects::record_image_artifacts(
                    &store,
                    job_id,
                    &attr_path,
                    &output_paths,
                    &sbom_roots,
                )
                .await;
            }
            if !caches.is_empty() {
                crate::effects::cache_push_and_record(
                    &store,
                    job_id,
                    &output_paths,
                    &caches,
                    Duration::from_secs(300),
                )
                .await;
            }
            if !reg_effects.is_empty() {
                let octx = OutputContext {
                    forge: &forge,
                    repo_slug: slug.as_str(),
                    attr_path: &attr_path,
                    system: &system,
                    git_ref: &git_ref,
                    default_branch: default_branch.as_deref(),
                    sha: sha.as_str(),
                    image_format,
                    output_paths: &output_paths,
                    sbom_runtime_roots: &sbom_roots,
                };
                let reports =
                    crate::effects::run_effects(&store, job_id, &reg_effects, &octx).await;
                // Only `Severity::Reported` effects post a forge check.
                // No effect uses that today — registry-push and
                // sbom-attach are `Advisory`, so their outcome lives in
                // `effect_runs`/the argunix UI and never reaches the
                // forge (a degraded push is not a property of the repo's
                // commit). This block is the seam for a future effect
                // whose result *is* meant to gate the commit (e.g. a
                // deploy). Suppressed in collapsed-check mode, like
                // per-job checks.
                if !collapsed_mode {
                    if let Some(provider) = &provider {
                        for r in &reports {
                            if r.severity != Severity::Reported {
                                continue;
                            }
                            let state = match r.status {
                                EffectStatus::Success => CheckState::Success,
                                EffectStatus::Failure => CheckState::Failure,
                                // A skipped effect (e.g. a non-image
                                // job) gets no forge check at all.
                                EffectStatus::Skipped => continue,
                            };
                            let post = CheckPost {
                                slug: slug.clone(),
                                sha: sha.clone(),
                                context: format!(
                                    "argunix: {} · {}",
                                    effect_check_label(r.kind),
                                    r.target,
                                ),
                                state,
                                description: Some(summarise_for_check(&r.detail, 140)),
                                target_url: Some(job_target_url(
                                    &external_url,
                                    &forge,
                                    &slug,
                                    eval_id,
                                    &attr_path,
                                )),
                            };
                            spawn_post_check(provider.clone(), post, forge.clone(), pauses.clone());
                        }
                    }
                }
            }
        }
        .in_current_span(),
    );
}

/// Short label for an effect's forge-check context: `registry-push` →
/// `registry`, `sbom-attach` → `sbom`; any other kind passes through.
fn effect_check_label(kind: &str) -> &str {
    match kind {
        "registry-push" => "registry",
        "sbom-attach" => "sbom",
        other => other,
    }
}

/// Cross-system multi-arch fan-in. After the build phase, stitch the
/// per-arch `docker` image jobs of one logical name into a multi-arch
/// OCI index (the work lives in `multiarch::run_fan_in`). Each outcome
/// is recorded as a `registry-index` row in `effect_runs` (visible in
/// the argunix UI) — it is *not* posted as a forge check. Distribution
/// is best-effort: a failed index push is not a property of the repo's
/// commit and must not redden an otherwise-green build. See
/// `design/multi-arch.md`.
async fn run_multiarch_fan_in(
    ctx: &WorkerContext,
    repo: &argunix_store::RepoRecord,
    eval: &argunix_store::EvalRecord,
    eval_id: EvalId,
    specs_by_id: &std::collections::HashMap<JobId, argunix_eval::JobSpec>,
) {
    let config = ctx.current.load().config.clone();
    // Outcomes are recorded inside `run_fan_in` as `effect_runs` rows;
    // we deliberately don't surface them on the forge.
    crate::multiarch::run_fan_in(
        &ctx.store,
        eval_id,
        specs_by_id,
        &config,
        &repo.forge,
        repo.slug.as_str(),
        repo.default_branch.as_deref(),
        &eval.git_ref,
        eval.sha.as_str(),
    )
    .await;
}

/// Best-effort docker registry publish. Any error is logged at `warn`
/// — the build is already a success, so registry-publish failure must
/// not flip the job's terminal status. Mirrors `binary_caches` push
/// failure policy.
async fn try_publish_docker_image(
    store: &SqlxStore,
    state: &Arc<argunix_registry::RegistryState>,
    repo_id: RepoId,
    eval_id: EvalId,
    job_id: JobId,
    spec: &argunix_eval::JobSpec,
    output_path: Option<&str>,
) {
    let repo = match <SqlxStore as argunix_store::RepoStore>::get(store, repo_id).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            tracing::warn!(repo_id = repo_id.get(), "registry: repo row missing");
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "registry: repo lookup failed");
            return;
        }
    };
    let eval = match <SqlxStore as argunix_store::EvalStore>::get(store, eval_id).await {
        Ok(Some(e)) => e,
        Ok(None) => {
            tracing::warn!(eval_id = eval_id.get(), "registry: eval row missing");
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "registry: eval lookup failed");
            return;
        }
    };
    let attr_leaf = argunix_registry::publish::attr_leaf(spec.attr_path.as_str());
    let system = spec.system.as_deref().unwrap_or("unknown");
    let req = argunix_registry::publish::PublishRequest {
        state,
        store,
        repo_id,
        eval_id,
        job_id,
        forge: &repo.forge,
        repo_slug: repo.slug.as_str(),
        attr_leaf: &attr_leaf,
        system,
        git_ref: &eval.git_ref,
        sha: &eval.sha,
        output_path,
    };
    if let Err(e) = argunix_registry::publish(req).await {
        tracing::warn!(
            job_id = job_id.get(),
            attr = %spec.attr_path,
            error = %e,
            "docker registry publish failed; job stays success",
        );
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

/// Outcome of one pool-dispatch attempt against a single builder.
///
/// - `Outcome` — the builder produced a genuine verdict (the
///   derivation built, or it failed to build, or it timed out). This
///   is terminal: `build_one` records it as-is.
/// - `Cancelled` — the caller's cancel token fired. The worker's
///   `build_one` translates this into a `JobStatus::Cancelled` row
///   update; the test dispatch path never sees it (no cancel token).
/// - `TransportFailure` — the builder connection broke *before* a
///   verdict was reached (push-closure failed, the `Build` message
///   couldn't be sent, or the lifecycle channel closed mid-build).
///   The derivation never actually failed to build — `build_one`
///   should re-dispatch it to another eligible builder rather than
///   recording `JobStatus::Failure`.
pub enum PoolDispatchResult {
    Outcome {
        outcome: argunix_build::BuildOutcome,
        phase_metrics: JobPhaseMetrics,
    },
    Cancelled,
    TransportFailure {
        phase_metrics: JobPhaseMetrics,
    },
}

/// What one `dispatch_build_via_pool` attempt yielded. `build_one`
/// loops on `TransportFailure`, re-dispatching to another builder.
enum PoolAttempt {
    /// The builder reached a genuine verdict (success / failure /
    /// timeout). Terminal — recorded as-is.
    Verdict(argunix_build::BuildOutcome, JobPhaseMetrics),
    /// The builder connection broke before a verdict. The derivation
    /// never actually failed to build; try another builder.
    TransportFailure,
}

/// Worker-side wrapper around [`dispatch_pool_build`]. Translates
/// `PoolDispatchResult::Cancelled` into a `JobStore` row update + the
/// `JobStatus::Cancelled` return, and maps `Outcome` / `TransportFailure`
/// onto [`PoolAttempt`] for `build_one`'s retry loop.
async fn dispatch_build_via_pool(
    ctx: &WorkerContext,
    builder_name: &argunix_domain::BuilderName,
    job_id: JobId,
    drv_path: &str,
    gc_root: &Path,
    log_path: &Path,
    log_limit: argunix_build::LogCaptureLimit,
    cancel: &argunix_web::CancelToken,
) -> anyhow::Result<PoolAttempt> {
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
        } => Ok(PoolAttempt::Verdict(outcome, phase_metrics)),
        PoolDispatchResult::TransportFailure { .. } => Ok(PoolAttempt::TransportFailure),
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

/// Render one parsed build event into the stored log buffer,
/// honouring the raw-size cap. `log_truncated` is set once the cap is
/// reached; rendering overshoots the cap by at most one line, which is
/// fine for a 100 MB cap.
fn append_log_event(
    log_buf: &mut Vec<u8>,
    ev: &argunix_nom::NomEvent,
    cap: usize,
    log_truncated: &mut bool,
) {
    let Some(line) = argunix_nom::render_storage_line(ev) else {
        return;
    };
    if log_buf.len() >= cap {
        *log_truncated = true;
        return;
    }
    log_buf.extend_from_slice(line.as_bytes());
    log_buf.push(b'\n');
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
    // Parses the agent's raw `internal-json` stderr into structured
    // build events — one instance per build, fed each log chunk.
    let mut nom = argunix_nom::NomParser::new();

    // A pre-verdict transport failure (push-closure / dispatch). The
    // derivation never reached the builder's `nix-store --realise`, so
    // this is a `TransportFailure`, not a build `Failure` — `build_one`
    // re-dispatches to another builder.
    let early_failure = |log_buf: Vec<u8>, log_path: PathBuf, phase_metrics: JobPhaseMetrics| async move {
        argunix_build::write_zstd_log(&log_path, log_buf).await?;
        Ok::<_, anyhow::Error>(PoolDispatchResult::TransportFailure { phase_metrics })
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
    let push_metrics = match nix_copy_phase(
        &registry,
        &dispatcher,
        builder_name,
        NixCopyDirection::To,
        &[drv_path.to_string()],
        nix_bin,
        build_id,
        cancel,
    )
    .await
    {
        CopyPhaseOutcome::Done(Ok(m)) => Some(m),
        CopyPhaseOutcome::Done(Err(e)) => {
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
        CopyPhaseOutcome::BuilderGone => {
            // Builder evicted/displaced mid-push: the derivation never
            // reached its `nix-store --realise`. Treat as a transport
            // failure so `build_one` re-dispatches elsewhere.
            tracing::warn!(
                builder = %builder_name,
                build_id,
                drv = drv_path,
                "builder left the pool during drv-closure push; failing over",
            );
            log_buf.extend_from_slice(
                format!(
                    "argunix: builder `{builder_name}` disconnected while pushing the \
                     drv closure; retrying on another builder.\n"
                )
                .as_bytes(),
            );
            phase_metrics.push_ms = Some(push_started_at.elapsed().as_millis() as u64);
            phase_metrics.push_bytes = Some(0);
            return early_failure(log_buf, log_path.to_path_buf(), phase_metrics).await;
        }
        CopyPhaseOutcome::Cancelled => {
            tracing::info!(
                builder = %builder_name,
                build_id,
                "cancel during drv-closure push",
            );
            log_buf.extend_from_slice(b"\nargunix: cancelled while pushing drv closure.\n");
            argunix_build::write_zstd_log(log_path, log_buf).await?;
            return Ok(PoolDispatchResult::Cancelled);
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
    //    move on.
    let grace = Duration::from_secs(60);
    let timeout_deadline = tokio::time::Instant::now() + build_timeout + grace;
    let mut output_paths: Vec<String> = Vec::new();
    // Set in every loop-exit path (Finished / channel-closed / timeout)
    // before the loop breaks; read after the loop.
    let mut final_status: BuildOutcomeStatus;
    let mut exit_code: Option<i32> = None;
    let mut aborted = false;
    let mut timed_out = false;
    // Set when the builder connection broke before a genuine verdict
    // (lifecycle channel closed mid-build, or output-closure pull
    // failed). Turns the result into a `TransportFailure` so the job
    // is retried on another builder instead of recorded as a failure.
    let mut transport_failed = false;
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
                        // Parse the raw internal-json chunk into events:
                        // stream them to the live tap and render
                        // per-derivation-prefixed text into the stored
                        // log.
                        for ev in nom.feed(&bytes) {
                            if let Some(ref tap) = live_log {
                                tap.push(ev.clone());
                            }
                            append_log_event(&mut log_buf, &ev, cap, &mut log_truncated);
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
                        transport_failed = true;
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
    // Flush any partial final line the parser buffered.
    for ev in nom.finish() {
        if let Some(ref tap) = live_log {
            tap.push(ev.clone());
        }
        append_log_event(&mut log_buf, &ev, cap, &mut log_truncated);
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
        match nix_copy_phase(
            &registry,
            &dispatcher,
            builder_name,
            NixCopyDirection::From,
            &output_paths,
            nix_bin,
            build_id,
            cancel,
        )
        .await
        {
            CopyPhaseOutcome::Done(Ok(m)) => {
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
            CopyPhaseOutcome::Done(Err(e)) => {
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
                // The derivation built fine on the builder; only the
                // output transfer broke. Retry on another builder
                // rather than reporting a spurious build failure.
                transport_failed = true;
            }
            CopyPhaseOutcome::BuilderGone => {
                tracing::warn!(
                    build_id,
                    builder = %builder_name,
                    "builder left the pool while pulling output closure; failing over",
                );
                log_buf.extend_from_slice(
                    format!(
                        "\nargunix: builder `{builder_name}` disconnected while pulling the \
                         output closure; retrying on another builder.\n"
                    )
                    .as_bytes(),
                );
                phase_metrics.pull_ms = Some(pull_started_at.elapsed().as_millis() as u64);
                phase_metrics.pull_bytes = Some(0);
                final_status = BuildOutcomeStatus::Failure;
                // Outputs built but never reached us — a transport
                // failure. Retried elsewhere; the cache-skip check on
                // retry collapses to a hit if the dead builder had
                // already pushed to a binary cache.
                transport_failed = true;
            }
            CopyPhaseOutcome::Cancelled => {
                tracing::info!(
                    build_id,
                    builder = %builder_name,
                    "cancel while pulling output closure",
                );
                log_buf.extend_from_slice(b"\nargunix: cancelled while pulling output closure.\n");
                if log_truncated {
                    log_buf.extend_from_slice(b"\n--- log truncated by argunix ---\n");
                }
                argunix_build::write_zstd_log(log_path, log_buf).await?;
                return Ok(PoolDispatchResult::Cancelled);
            }
        }
    }

    if log_truncated {
        log_buf.extend_from_slice(b"\n--- log truncated by argunix ---\n");
    }
    argunix_build::write_zstd_log(log_path, log_buf).await?;

    // A mid-build disconnect or a failed output pull is a transport
    // failure, not a build verdict — surface it so `build_one` retries
    // on another builder. The log written above is kept so the last
    // attempt's diagnostics survive if every builder fails.
    if transport_failed {
        return Ok(PoolDispatchResult::TransportFailure { phase_metrics });
    }

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

/// Outcome of a push/pull closure-transfer phase.
enum CopyPhaseOutcome {
    /// `nix copy` ran to completion (success or a real error).
    Done(Result<argunix_builders::NixCopyMetrics, argunix_builders::ClosureXferError>),
    /// The operator cancelled (cancel-on-push) while the transfer ran.
    Cancelled,
    /// The chosen builder left the registry while the transfer ran —
    /// the watchdog evicted it, or it was displaced by a reconnect.
    /// The transfer is abandoned (its `nix copy` subprocess is killed
    /// via `kill_on_drop`) and the job fails over to another builder.
    BuilderGone,
}

/// Run one closure-transfer phase (`nix copy --to` push or
/// `nix copy --from` pull), racing it against the cancel token *and*
/// the builder's continued presence in the registry.
///
/// The build phase already races `lifecycle.recv()` against cancel and
/// a wall clock, so a vanished builder during the build is caught by
/// the registry draining the lifecycle channel. The transfer phases had
/// no such guard: a builder that froze mid-`nix copy` (a slept laptop)
/// left the worker blocked in `cmd.output().await` indefinitely — the
/// underlying russh channel only errors once the kernel's TCP
/// retransmit budget is exhausted (many minutes), and not at all if the
/// peer later resumes. Polling registry presence lets the liveness
/// watchdog's eviction unblock us within its scan interval; dropping the
/// `nix_copy_over_pool` future kills the `nix copy` child (`kill_on_drop`)
/// and aborts its proxy.
async fn nix_copy_phase(
    registry: &Arc<argunix_builders::BuilderRegistry>,
    dispatcher: &BuilderDispatcher,
    builder_name: &argunix_domain::BuilderName,
    direction: NixCopyDirection,
    paths: &[String],
    nix_bin: &Path,
    build_id: i64,
    cancel: Option<&argunix_web::CancelToken>,
) -> CopyPhaseOutcome {
    let copy = nix_copy_over_pool(
        dispatcher,
        builder_name,
        direction,
        paths,
        nix_bin,
        build_id,
    );
    tokio::pin!(copy);
    let gone = async {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if registry.snapshot(builder_name).is_none() {
                return;
            }
        }
    };
    tokio::pin!(gone);
    tokio::select! {
        biased;
        _ = wait_cancelled(cancel) => CopyPhaseOutcome::Cancelled,
        _ = &mut gone => CopyPhaseOutcome::BuilderGone,
        r = &mut copy => CopyPhaseOutcome::Done(r),
    }
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

/// A git credential helper that answers `get` with the username and
/// token taken from the environment. The secret is therefore never in
/// argv (visible in `/proc/<pid>/cmdline`), in the clone URL, or echoed
/// by git's own stderr — closing the token-disclosure paths of SEC-1.
const GIT_CRED_HELPER: &str = "!f() { test \"$1\" = get && \
     printf 'username=%s\\npassword=%s\\n' \"$ARGUNIX_GIT_USER\" \"$ARGUNIX_GIT_TOKEN\"; }; f";

async fn clone_repo(
    url: &str,
    sha: &argunix_domain::Sha,
    dst: &Path,
    timeout: Duration,
    creds: Option<&argunix_forge::GitCredentials>,
) -> anyhow::Result<()> {
    if let Some(parent) = dst.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }

    run_git(
        timeout,
        creds,
        &["clone", "--filter=blob:none", url, &dst.to_string_lossy()],
    )
    .await?;
    run_git_in(
        timeout,
        dst,
        creds,
        &["fetch", "--depth=1", "origin", sha.as_str()],
    )
    .await?;
    run_git_in(timeout, dst, creds, &["checkout", sha.as_str()]).await?;
    Ok(())
}

async fn run_git(
    timeout: Duration,
    creds: Option<&argunix_forge::GitCredentials>,
    args: &[&str],
) -> anyhow::Result<()> {
    run_git_with_optional_cwd(timeout, None, creds, args).await
}

async fn run_git_in(
    timeout: Duration,
    cwd: &Path,
    creds: Option<&argunix_forge::GitCredentials>,
    args: &[&str],
) -> anyhow::Result<()> {
    run_git_with_optional_cwd(timeout, Some(cwd), creds, args).await
}

async fn run_git_with_optional_cwd(
    timeout: Duration,
    cwd: Option<&Path>,
    creds: Option<&argunix_forge::GitCredentials>,
    args: &[&str],
) -> anyhow::Result<()> {
    let mut cmd = Command::new("git");
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    // Inject HTTPS auth via a credential helper that reads the token from
    // the environment, so the token never touches argv or the URL. The
    // leading empty `credential.helper=` clears any inherited system/global
    // helpers. `GIT_TERMINAL_PROMPT=0` makes auth failures fail fast
    // instead of blocking on a prompt.
    if let Some(c) = creds {
        cmd.arg("-c")
            .arg("credential.helper=")
            .arg("-c")
            .arg(format!("credential.helper={GIT_CRED_HELPER}"))
            .env("ARGUNIX_GIT_USER", &c.username)
            .env("ARGUNIX_GIT_TOKEN", &c.token)
            .env("GIT_TERMINAL_PROMPT", "0");
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
            native_system: systems.first().map(|s| s.to_string()).unwrap_or_default(),
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
                last_heartbeat: std::time::Instant::now(),
                abort: None,
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
            image_format: None,
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
        assert!(
            pick_builder_for_spec(&reg, &spec(Some("x86_64-linux"), &[]), &Default::default())
                .is_none()
        );
    }

    #[test]
    fn pick_builder_returns_none_when_no_system_match() {
        let reg = BuilderRegistry::new();
        register(&reg, "darwin", 1, caps(&["aarch64-darwin"], &[], 4));
        assert!(
            pick_builder_for_spec(&reg, &spec(Some("x86_64-linux"), &[]), &Default::default())
                .is_none()
        );
    }

    #[test]
    fn pick_builder_picks_least_loaded_eligible() {
        let reg = BuilderRegistry::new();
        register(&reg, "busy", 1, caps(&["x86_64-linux"], &[], 4));
        register(&reg, "idle", 2, caps(&["x86_64-linux"], &[], 4));
        // Saturate "busy" with one slot taken.
        let _slot = BuilderSlot::reserve(reg.clone(), BuilderName::new("busy").unwrap());

        let chosen =
            pick_builder_for_spec(&reg, &spec(Some("x86_64-linux"), &[]), &Default::default())
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

        let chosen = pick_builder_for_spec(
            &reg,
            &spec(Some("x86_64-linux"), &["kvm"]),
            &Default::default(),
        )
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
            pick_builder_for_spec(&reg, &spec(Some("x86_64-linux"), &[]), &Default::default())
                .is_none(),
            "saturated builder must not be chosen — caller falls back to multi-builder \
             arg so nix's own scheduler can retry against the full pool",
        );
    }

    #[test]
    fn pick_builder_skips_excluded_builders() {
        let reg = BuilderRegistry::new();
        register(&reg, "alpha", 1, caps(&["x86_64-linux"], &[], 4));
        register(&reg, "beta", 2, caps(&["x86_64-linux"], &[], 4));
        let alpha_conn = reg
            .snapshot(&BuilderName::new("alpha").unwrap())
            .unwrap()
            .connection_id;
        let beta_conn = reg
            .snapshot(&BuilderName::new("beta").unwrap())
            .unwrap()
            .connection_id;
        let mut excluded = std::collections::HashSet::new();
        excluded.insert(alpha_conn);
        let chosen = pick_builder_for_spec(&reg, &spec(Some("x86_64-linux"), &[]), &excluded)
            .expect("beta is still eligible");
        assert_eq!(
            chosen.name.as_str(),
            "beta",
            "an excluded builder must not be chosen for a retry",
        );
        // Excluding every eligible builder yields None — `build_one`
        // then falls back to a local build.
        excluded.insert(beta_conn);
        assert!(
            pick_builder_for_spec(&reg, &spec(Some("x86_64-linux"), &[]), &excluded).is_none(),
            "all eligible builders excluded → caller falls back to local",
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
