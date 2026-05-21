mod control;
mod dispatch_driver;
mod effects;
mod gc;
mod multiarch;
mod worker;

use anyhow::{Context, anyhow};
use argunix_domain::{EvalId, EvalStatus, ImageFormat, JobId, JobStatus, RepoId, Sha, Slug};
use argunix_store::{EvalStore, JobPhaseMetrics, JobStore, RepoStore};
use chrono::Utc;
use clap::{Args, Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(version, about = "argunix CI daemon")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the argunix daemon (service-mode skeleton).
    Run(RunArgs),
    /// Evaluate a local flake and print discovered jobs as JSON.
    Eval(EvalArgs),
    /// Evaluate and build a local flake end-to-end (single-shot pipeline).
    Build(BuildArgs),
    /// Run as an HTTP daemon: accept webhooks, queue evaluations.
    Serve(ServeArgs),
}

#[derive(Args, Debug)]
struct ServeArgs {
    /// Path to the argunix YAML config.
    #[arg(long, value_name = "PATH")]
    config: PathBuf,
    /// Skip checking that every secret file referenced by the config exists.
    #[arg(long)]
    skip_secret_check: bool,
    /// Override the listen address from the config.
    #[arg(long, value_name = "HOST:PORT")]
    listen: Option<String>,
    /// Override the work directory used for clones (default: ./work).
    #[arg(long, value_name = "PATH")]
    work_dir: Option<PathBuf>,
    /// Override the log directory (default: ./logs).
    #[arg(long, value_name = "PATH")]
    log_dir: Option<PathBuf>,
    /// Override the GC root base directory.
    #[arg(long, value_name = "PATH")]
    gc_root_dir: Option<PathBuf>,
    /// Override systems to evaluate (default: host's local system).
    #[arg(long, value_delimiter = ',', value_name = "SYSTEM[,SYSTEM]")]
    systems: Option<Vec<String>>,
    /// Path to the unix-domain control socket (`argunixctl` connects
    /// here). Default: `/run/argunix/control.sock`.
    #[arg(long, value_name = "PATH")]
    control_socket: Option<PathBuf>,
    /// Path to the local `nix-store` binary. Used post-pull to
    /// register gc-roots (`nix-store --add-root --indirect`).
    /// Default resolves on PATH; the NixOS module pins it.
    #[arg(long, value_name = "PATH", default_value = "nix-store")]
    nix_store_bin: PathBuf,
    /// Path to the local `nix` binary. Used to drive closure
    /// transfer via `nix copy --from/--to unix:///proxy.sock`,
    /// where the proxy tunnels the daemon protocol through our
    /// russh side channel to the builder's `nix-daemon --stdio`.
    /// Default resolves on PATH; the NixOS module pins it.
    #[arg(long, value_name = "PATH", default_value = "nix")]
    nix_bin: PathBuf,
}

#[derive(Args, Debug)]
struct RunArgs {
    #[arg(long, value_name = "PATH")]
    config: PathBuf,
    #[arg(long)]
    skip_secret_check: bool,
}

#[derive(Args, Debug)]
struct EvalArgs {
    #[arg(long, value_name = "PATH")]
    src: PathBuf,
    #[arg(long, value_delimiter = ',', value_name = "SYSTEM[,SYSTEM]")]
    systems: Option<Vec<String>>,
    #[arg(long, default_value_t = 600, value_name = "SECONDS")]
    timeout_seconds: u64,
}

#[derive(Args, Debug)]
struct BuildArgs {
    /// Path to the argunix YAML config (used for binary cache list).
    #[arg(long, value_name = "PATH")]
    config: PathBuf,
    /// Skip checking that every secret file referenced by the config exists.
    #[arg(long)]
    skip_secret_check: bool,
    /// Path to a local checkout containing a `flake.nix`.
    #[arg(long, value_name = "PATH")]
    src: PathBuf,
    /// Repo slug, e.g. `myorg/myrepo`. Used as the persistent repo identity.
    #[arg(long, value_name = "SLUG")]
    slug: String,
    /// Forge name (matches the `forges:` key in the YAML config).
    #[arg(long, value_name = "FORGE")]
    forge: String,
    /// Git ref recorded for the evaluation, e.g. `main`. Stored as
    /// the short branch name (no `refs/heads/` prefix) — webhook
    /// ingestion normalizes the same way.
    #[arg(long, value_name = "REF", default_value = "HEAD")]
    git_ref: String,
    /// 40-hex-char SHA recorded for the evaluation. If omitted, a synthetic
    /// zero SHA is recorded (single-shot mode skips the clone step).
    #[arg(
        long,
        value_name = "SHA",
        default_value = "0000000000000000000000000000000000000000"
    )]
    sha: String,
    /// Trigger label recorded on the evaluation row, e.g. `manual`, `push`.
    #[arg(long, value_name = "TRIGGER", default_value = "manual")]
    trigger: String,
    /// Comma-separated systems to evaluate. Defaults to the host's local system.
    #[arg(long, value_delimiter = ',', value_name = "SYSTEM[,SYSTEM]")]
    systems: Option<Vec<String>>,
    /// Wall-clock seconds for each `nix-eval-jobs` subprocess.
    #[arg(long, default_value_t = 600, value_name = "SECONDS")]
    eval_timeout_seconds: u64,
    /// Override the GC root base directory (for tests).
    #[arg(long, value_name = "PATH")]
    gc_root_dir: Option<PathBuf>,
    /// Override the log directory (for tests). Defaults to `./logs`.
    #[arg(long, value_name = "PATH")]
    log_dir: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;
    runtime.block_on(dispatch(cli))
}

async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Run(args) => run(args).await,
        Command::Eval(args) => eval(args).await,
        Command::Build(args) => build(args).await,
        Command::Serve(args) => serve(args).await,
    }
}

async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let config = argunix_config::load(&args.config)
        .with_context(|| format!("loading config from {}", args.config.display()))?;
    if !args.skip_secret_check {
        config
            .validate_secrets_exist()
            .context("validating secret files")?;
    }
    let providers = argunix_web::build_providers(&config)
        .await
        .context("building forge providers")?;
    tracing::info!(
        forges = providers.len(),
        repos = config.repos.len(),
        "providers initialised"
    );

    let pool = argunix_store::open_at(Path::new("./db.sqlite"))
        .await
        .context("opening sqlite database at ./db.sqlite")?;
    let store = argunix_store::SqlxStore::new(pool);

    let n = <argunix_store::SqlxStore as JobStore>::mark_running_interrupted(&store)
        .await
        .context("recovering interrupted jobs")?;
    if n > 0 {
        tracing::info!(count = n, "marked previously-running jobs as interrupted");
    }

    let work_dir = args
        .work_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("./work"));
    let log_dir = args
        .log_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("./logs"));
    let gc_root_dir = args
        .gc_root_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("/nix/var/nix/gcroots/per-user/argunix"));
    let systems = args
        .systems
        .clone()
        .unwrap_or_else(argunix_eval::detect_local_systems);

    let pauses = std::sync::Arc::new(argunix_web::PauseRegistry::new());
    let cancellations = std::sync::Arc::new(argunix_web::CancelRegistry::new());

    // Atomic-swappable bundle. Both AppStateInner and WorkerContext
    // hold the same Arc<ArcSwap<_>>; `argunixctl reload` constructs a
    // new ConfigSnapshot and stores into it, in-flight handlers and
    // evals keep the snapshot they captured at start.
    let snapshot = Arc::new(argunix_web::ConfigSnapshot {
        config: Arc::new(config),
        providers: Arc::new(providers),
    });
    let current = Arc::new(arc_swap::ArcSwap::from(snapshot));

    // Config-driven cleanup: at every startup, prune any repo (and
    // its evaluations / jobs / logs / GC roots) that no longer
    // appears in `config.repos`. This catches orphans left behind
    // when an operator renames a forge entry or removes a repo from
    // the YAML.
    prune_orphan_state(&current.load_full().config, &store, &log_dir, &gc_root_dir).await;

    // Auto-install / refresh webhooks at every startup. Best-effort:
    // a forge being unreachable doesn't block daemon startup.
    {
        let snap = current.load_full();
        argunix_web::ensure_webhooks(&snap.config, &snap.providers, &store).await;
    }

    // Shared registry of currently-connected builders. Created
    // before the worker so it can compose `--builders` per dispatch;
    // BuilderServer (when `builder_enrollment` is configured) writes
    // into the same Arc; argunixctl reads it via the control socket.
    let builder_registry = argunix_builders::BuilderRegistry::new();
    let live_logs = argunix_web::LiveLogRegistry::new();

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    // Closure transfer is `nix copy` over a tunneled
    // `nix-daemon --stdio`, which streams per-file with bounded
    // memory. `build_concurrency` is the global cap on parallel
    // in-flight builds. Operators tune this via the YAML
    // `schedule.build_concurrency` key (default 4).
    let build_concurrency: usize = current.load().config.schedule.build_concurrency as usize;
    // Per-build wall-clock timeout, from the YAML
    // `schedule.build_timeout_seconds` key (default 5h). Read once at
    // startup, like `build_concurrency`.
    let build_timeout =
        Duration::from_secs(current.load().config.schedule.build_timeout_seconds as u64);
    // How long the dispatcher waits for an eligible builder before
    // giving up and marking a job `Interrupted`. Default 0 (immediate)
    // — operators set a modest value (e.g. 30s) when a builder
    // restarts in lockstep with the coordinator and needs a few
    // seconds to re-enrol after the resume pass dispatches the eval.
    let builder_wait =
        Duration::from_secs(current.load().config.schedule.builder_wait_seconds as u64);
    // Single global build cap shared across all in-flight evals. With
    // build dispatch now spawned per-eval (see `worker::process`),
    // this prevents two concurrent evals from each getting their own
    // pool of `build_concurrency` permits.
    let global_build_sem =
        std::sync::Arc::new(tokio::sync::Semaphore::new(build_concurrency.max(1)));
    // Tracks the detached build-phase tasks `process` spawns so the
    // shutdown sequence can wait for them rather than letting the
    // runtime tear them down mid-`nix copy`.
    let build_tasks = std::sync::Arc::new(worker::BuildTaskTracker::new());
    // Cross-eval dispatch scheduler. Defaults to flat WFQ per
    // `SchedulerKind::default()`; the build_concurrency value is the
    // scheduler's in-flight cap (later, when the dispatcher reads from
    // this strategy, it replaces the per-eval JoinSet semaphore in
    // worker.rs). Constructing it here means a future config knob
    // (`[scheduler] kind = "dag"`) drops in without touching the
    // worker.
    let scheduler = std::sync::Arc::new(std::sync::Mutex::new(argunix_sched::build(
        argunix_sched::SchedulerKind::default(),
        Some(build_concurrency),
    )));
    // Registry blob/manifest pool. Fixed location for the prototype:
    // `./registry-state` next to the sqlite db. The web router and the
    // worker share the same Arc.
    let registry_state = std::sync::Arc::new(argunix_registry::RegistryState::new(
        std::path::PathBuf::from("./registry-state"),
    ));
    if let Err(e) = registry_state.ensure_dirs().await {
        tracing::warn!(error = %e, "failed to create registry state dirs at startup");
    }
    let worker_ctx = worker::WorkerContext {
        current: current.clone(),
        store: store.clone(),
        work_dir,
        log_dir: log_dir.clone(),
        gc_root_dir: gc_root_dir.clone(),
        eval_timeout: Duration::from_secs(600),
        build_timeout,
        builder_wait,
        clone_timeout: Duration::from_secs(300),
        systems,
        pauses: pauses.clone(),
        cancellations: cancellations.clone(),
        builder_registry: builder_registry.clone(),
        live_logs: live_logs.clone(),
        nix_store_bin: args.nix_store_bin.clone(),
        nix_bin: args.nix_bin.clone(),
        build_concurrency,
        global_build_sem: global_build_sem.clone(),
        build_tasks: build_tasks.clone(),
        scheduler: scheduler.clone(),
        registry_state: registry_state.clone(),
    };
    let worker_handle = worker::spawn(worker_ctx, rx);

    // Redrive `Queued` evaluations the previous instance never
    // started processing.
    match <argunix_store::SqlxStore as EvalStore>::list_queued_ids(&store).await {
        Ok(ids) if !ids.is_empty() => {
            tracing::info!(
                count = ids.len(),
                "redriving queued evaluations from prior run"
            );
            for id in ids {
                if let Err(e) = tx.send(id) {
                    tracing::warn!(error = %e, "failed to enqueue eval for redrive");
                }
            }
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "could not list queued evaluations at startup"),
    }

    // Resume `Building` evaluations that were mid-build when the
    // previous daemon instance died. Their jobs are already persisted
    // — the just-completed `mark_running_interrupted` pass flipped any
    // jobs that were `Running` to `Interrupted`. We requeue those
    // `Interrupted` jobs back to `Queued` and hand the eval to the
    // worker, which detects the already-`Building` state and skips
    // the clone/eval/persist phase.
    match <argunix_store::SqlxStore as EvalStore>::list_building_ids(&store).await {
        Ok(ids) if !ids.is_empty() => {
            tracing::info!(
                count = ids.len(),
                "resuming building evaluations from prior run"
            );
            for id in ids {
                match <argunix_store::SqlxStore as JobStore>::requeue_interrupted_for_eval(
                    &store, id,
                )
                .await
                {
                    Ok(n) => {
                        if n > 0 {
                            tracing::info!(
                                eval_id = id.get(),
                                requeued = n,
                                "requeued interrupted jobs for resumed eval"
                            );
                        }
                    }
                    Err(e) => tracing::warn!(
                        error = %e,
                        eval_id = id.get(),
                        "failed to requeue interrupted jobs",
                    ),
                }
                if let Err(e) = tx.send(id) {
                    tracing::warn!(error = %e, "failed to enqueue eval for resume");
                }
            }
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "could not list building evaluations at startup"),
    }

    let listen = args
        .listen
        .clone()
        .unwrap_or_else(|| current.load().config.listen.clone());
    let coalesce_seconds: u64 = current
        .load()
        .config
        .schedule
        .webhook_coalesce_seconds
        .into();
    let coalesce = std::sync::Arc::new(argunix_web::CoalescePool::new(
        std::time::Duration::from_secs(coalesce_seconds),
    ));
    let host_stats = argunix_web::HostStatsRing::new();
    // Background sampler — ticks /proc every 5s and pushes into
    // `host_stats`, the same ring `/api/host/stats` reads from. Aborted
    // when its handle drops (at daemon shutdown).
    let _host_stats_handle = argunix_web::spawn_host_sampler(host_stats.clone());

    // Probe `nix --version` and `nix-eval-jobs --version` once so the
    // /hosts page coordinator card can show the toolchain. Detection
    // never fails — missing/unparsable binaries leave "unknown".
    // `nix-eval-jobs` is resolved via PATH because that's how
    // `argunix-eval` invokes it; the `--nix-bin` flag pins `nix`
    // itself for the worker but not the eval helper.
    let nix_bin_str = args.nix_bin.to_string_lossy().to_string();
    let coordinator_versions = std::sync::Arc::new(
        argunix_web::detect_coordinator_versions(&nix_bin_str, "nix-eval-jobs").await,
    );

    let inner = argunix_web::AppStateInner {
        current: current.clone(),
        store: store.clone(),
        work_dispatcher: tx,
        coalesce,
        pauses,
        cancellations,
        builder_registry: builder_registry.clone(),
        live_logs,
        host_stats,
        started_at: std::time::Instant::now(),
        coordinator_versions,
    };
    let app_state = std::sync::Arc::new(inner);
    let registry_router = argunix_registry::router(argunix_registry::api::RegistryApi {
        state: registry_state.clone(),
        store: store.clone(),
    });
    let router = argunix_web::router(app_state.clone()).merge(registry_router);

    // Spawn the control-socket server. Bound to the path from CLI
    // args (default `/run/argunix/control.sock`). Runs as a background
    // task; survives reloads, gets aborted at daemon shutdown so it
    // releases its `AppState` clone (which holds an `mpsc::Sender`
    // to the worker — without dropping that, the worker's `rx.recv()`
    // never returns `None` and the drain hangs).
    let control_path = args
        .control_socket
        .clone()
        .unwrap_or_else(|| PathBuf::from("/run/argunix/control.sock"));
    let builder_server_handle = spawn_builder_server_if_configured(
        &current.load_full().config,
        &store,
        builder_registry.clone(),
    )
    .await
    .context("starting builder enrollment server")?;
    // Liveness watchdog: evict builders that go silent past the
    // heartbeat threshold, freeing their in-flight jobs to retry
    // elsewhere. Only meaningful alongside the enrollment server, and it
    // is the backstop for the case russh/TCP keepalive can't catch — a
    // builder frozen mid-transfer (slept laptop) where our outbound
    // flush blocks and starves russh's own keepalive timer.
    let builder_watchdog_handle = builder_server_handle
        .is_some()
        .then(|| argunix_builders::spawn_liveness_watchdog(builder_registry.clone()));
    // Retention GC. Background ticker; aborted at shutdown
    // alongside the control + builder tasks. No-op on a config with
    // no `retention.max_age_days` and no `retention.max_size_gb`.
    let gc_handle = gc::spawn(gc::GcContext {
        current: current.clone(),
        store: store.clone(),
        log_dir: log_dir.clone(),
        gc_root_dir: gc_root_dir.clone(),
    });

    let control_handle = control::spawn(control::ControlContext {
        socket_path: control_path,
        app_state: app_state.clone(),
        store,
        log_dir,
        gc_root_dir,
        config_path: args.config.clone(),
        skip_secret_check: args.skip_secret_check,
        builder_registry,
        nix_store_bin: args.nix_store_bin.clone(),
        nix_bin: args.nix_bin.clone(),
        build_timeout,
    });

    // Tell systemd we're ready (so `Type=notify-reload` can sequence
    // ExecReload after startup completes).
    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]);

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    let local = listener.local_addr().context("reading local addr")?;
    println!("listening on {local}");
    tracing::info!(%local, "argunix http server ready");

    // Run axum on a separate task so the shutdown sequence below can
    // bound its drain time. Past versions used
    // `serve(...).with_graceful_shutdown(...).await` directly, which
    // hangs forever when an HTTP keep-alive (e.g. a reverse proxy
    // upstream) is still parked on the connection.
    let (graceful_tx, graceful_rx) = tokio::sync::oneshot::channel::<()>();
    let serve_fut = axum::serve(listener, router).with_graceful_shutdown(async move {
        let _ = graceful_rx.await;
    });
    // axum's `WithGracefulShutdown` isn't `IntoFuture`-aware until
    // it's awaited; wrap in an `async` block to give tokio::spawn an
    // actual `Future`.
    let serve_handle = tokio::spawn(async move { serve_fut.await });

    // Wait for the actual signal — the shutdown_signal future used to
    // be wired *into* axum's graceful_shutdown, but we now want to
    // sequence the rest of the shutdown explicitly.
    shutdown_signal().await;
    tracing::info!("shutdown signal received; draining");

    // Tell axum to start its drain (returns Err only if the receiver
    // already dropped, which we don't care about).
    let _ = graceful_tx.send(());

    // Cap the HTTP drain. `with_graceful_shutdown` waits for every
    // open connection to close — long-poll/SSE clients and
    // misbehaving reverse-proxy keep-alives can park there forever.
    // 10 s is plenty for a real in-flight POST handler to finish.
    match tokio::time::timeout(Duration::from_secs(10), serve_handle).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(e))) => tracing::warn!(error = %e, "axum serve returned error during drain"),
        Ok(Err(e)) => tracing::warn!(error = %e, "axum serve task join error"),
        Err(_) => tracing::warn!("axum drain exceeded 10s; aborting in-flight HTTP connections"),
    }

    // Release every clone of the worker's mpsc Sender so its
    // `rx.recv()` returns None and the worker drains in-flight
    // evaluations before exiting:
    //   1. axum dropped its router-state clone when its serve future
    //      resolved (or was aborted).
    //   2. The control task holds another clone via `ControlContext`;
    //      abort it (it's a hot accept-loop with no other clean exit).
    //   3. Drop the local `app_state` Arc held by main itself.
    control_handle.abort();
    let _ = control_handle.await;
    gc_handle.abort();
    let _ = gc_handle.await;
    drop(app_state);

    // Stop the builder enrollment server cleanly when configured.
    // Without aborting it, connected agents see a TCP RST when the
    // runtime drops; aborting first lets russh send the SSH
    // disconnect message, which is the politer wire shape and stops
    // the agent from logging a spurious error on graceful operator
    // restarts.
    if let Some(h) = builder_server_handle {
        h.abort();
        let _ = h.await;
    }
    if let Some(h) = builder_watchdog_handle {
        h.abort();
        let _ = h.await;
    }

    // Bounded drain — systemd's `TimeoutStopSec` would SIGKILL us
    // eventually, but that races with the unit's restart counter and
    // can leave half-finished log entries. 30 s gives an in-flight
    // `nix-store --realise` a fair chance to wrap up; longer than
    // that and the operator wanted a hard restart anyway.
    //
    // Two phases:
    //   1. The eval worker itself drains (its rx side closes when the
    //      last sender Arc drops above). With my spawn-and-return
    //      refactor in `worker::process`, this is fast — the worker
    //      doesn't await build phases anymore.
    //   2. The detached build-phase tasks (registered with
    //      `build_tasks`) drain. This is where the real wall-clock
    //      goes, since these are the `nix copy` / `nix-store --realise`
    //      pipelines.
    let drain_deadline = Duration::from_secs(30);
    let drain_start = std::time::Instant::now();
    match tokio::time::timeout(drain_deadline, worker_handle).await {
        Ok(_) => tracing::info!("eval worker drained"),
        Err(_) => tracing::warn!("eval worker did not drain within 30s"),
    }
    let remaining = drain_deadline.saturating_sub(drain_start.elapsed());
    let in_flight = build_tasks.in_flight();
    if in_flight > 0 {
        tracing::info!(
            in_flight,
            remaining_secs = remaining.as_secs(),
            "waiting for in-flight build phases to finish",
        );
    }
    match tokio::time::timeout(remaining, build_tasks.wait_idle()).await {
        Ok(()) => tracing::info!("graceful shutdown complete"),
        Err(_) => tracing::warn!(
            in_flight = build_tasks.in_flight(),
            "build phases did not drain within 30s; exiting and letting systemd reap any in-flight build"
        ),
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let term = async {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            if let Ok(mut s) = signal(SignalKind::terminate()) {
                s.recv().await;
            } else {
                std::future::pending::<()>().await;
            }
        }
        #[cfg(not(unix))]
        std::future::pending::<()>().await;
    };
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    tracing::info!("shutdown signal received; draining");
}

async fn run(args: RunArgs) -> anyhow::Result<()> {
    let config = argunix_config::load(&args.config)
        .with_context(|| format!("loading config from {}", args.config.display()))?;
    if !args.skip_secret_check {
        config
            .validate_secrets_exist()
            .context("validating secret files")?;
    }
    tracing::info!(
        repos = config.repos.len(),
        forges = config.forges.len(),
        binary_caches = config.binary_caches.len(),
        "config loaded",
    );

    let pool = argunix_store::open_at(Path::new("./db.sqlite"))
        .await
        .context("opening sqlite database at ./db.sqlite")?;
    let store = argunix_store::SqlxStore::new(pool);

    let n = <argunix_store::SqlxStore as JobStore>::mark_running_interrupted(&store)
        .await
        .context("recovering interrupted jobs")?;
    if n > 0 {
        tracing::info!(count = n, "marked previously-running jobs as interrupted");
    }

    println!("ready");
    Ok(())
}

async fn eval(args: EvalArgs) -> anyhow::Result<()> {
    let systems = args
        .systems
        .unwrap_or_else(argunix_eval::detect_local_systems);
    let request = argunix_eval::EvalRequest {
        source_path: args
            .src
            .canonicalize()
            .with_context(|| format!("resolving --src path {}", args.src.display()))?,
        systems: systems.clone(),
        outputs: argunix_eval::default_flake_outputs(),
        timeout: Duration::from_secs(args.timeout_seconds),
        // Offline `argunix eval` just prints jobs as JSON — no
        // subsequent push/build, so we don't need to pin the drvs.
        gc_roots_dir: None,
    };
    tracing::info!(src = %request.source_path.display(), ?systems, "starting offline evaluation");
    let jobs = argunix_eval::evaluate(&request)
        .await
        .context("running nix-eval-jobs")?;
    tracing::info!(count = jobs.len(), "evaluation produced jobs");
    let serialised = serde_json::to_string_pretty(&jobs).context("serialising job list to JSON")?;
    println!("{serialised}");
    Ok(())
}

async fn build(args: BuildArgs) -> anyhow::Result<()> {
    let config = argunix_config::load(&args.config)
        .with_context(|| format!("loading config from {}", args.config.display()))?;
    if !args.skip_secret_check {
        config
            .validate_secrets_exist()
            .context("validating secret files")?;
    }

    let slug = Slug::new(args.slug.clone()).map_err(|e| anyhow!("invalid --slug: {e}"))?;
    let sha = Sha::new(args.sha.clone()).map_err(|e| anyhow!("invalid --sha: {e}"))?;

    if !config.forges.contains_key(&args.forge) {
        return Err(anyhow!(
            "--forge `{}` is not configured (known: {})",
            args.forge,
            config.forges.keys().cloned().collect::<Vec<_>>().join(", "),
        ));
    }

    let pool = argunix_store::open_at(Path::new("./db.sqlite"))
        .await
        .context("opening sqlite database")?;
    let store = argunix_store::SqlxStore::new(pool);

    let repo_id = <argunix_store::SqlxStore as RepoStore>::upsert(&store, &args.forge, &slug)
        .await
        .context("upserting repo")?;

    let eval_id = <argunix_store::SqlxStore as EvalStore>::create(
        &store,
        argunix_store::NewEvaluation {
            repo_id,
            trigger: args.trigger.clone(),
            git_ref: args.git_ref.clone(),
            sha,
            pr_number: None,
        },
    )
    .await
    .context("creating evaluation row")?;
    <argunix_store::SqlxStore as EvalStore>::set_status(&store, eval_id, EvalStatus::Evaluating)
        .await?;
    tracing::info!(
        repo_id = repo_id.get(),
        eval_id = eval_id.get(),
        "evaluation started"
    );

    let systems = args
        .systems
        .unwrap_or_else(argunix_eval::detect_local_systems);
    let eval_request = argunix_eval::EvalRequest {
        source_path: args
            .src
            .canonicalize()
            .with_context(|| format!("resolving --src path {}", args.src.display()))?,
        systems: systems.clone(),
        outputs: argunix_eval::default_flake_outputs(),
        timeout: Duration::from_secs(args.eval_timeout_seconds),
        // Offline `argunix build` runs eval + build back-to-back in the
        // same process; the drvs land in the local store and are
        // realised immediately, so the GC race the worker hits doesn't
        // apply here.
        gc_roots_dir: None,
    };
    let jobs = match argunix_eval::evaluate(&eval_request).await {
        Ok(j) => j,
        Err(e) => {
            // Stamp the failure reason onto the eval row so the UI
            // can show *why* the eval failed rather than just the
            // bare status. The CLI path has no outer error trap
            // (unlike the worker loop), so we capture it here.
            // Convert through anyhow so `{:#}` walks the source chain
            // — `Display` on the bare `EvalError` would only show the
            // outermost message.
            let err = anyhow::Error::from(e).context("evaluation failed");
            let chained = format!("{err:#}");
            <argunix_store::SqlxStore as EvalStore>::fail_with_reason(
                &store,
                eval_id,
                &chained,
                Utc::now(),
            )
            .await?;
            return Err(err);
        }
    };
    <argunix_store::SqlxStore as EvalStore>::mark_building(&store, eval_id, Utc::now()).await?;
    tracing::info!(count = jobs.len(), "evaluation finished");

    let push_caches: Vec<argunix_build::PushCache> = config
        .binary_caches
        .iter()
        .map(|c| argunix_build::PushCache {
            url: c.push_url.clone(),
            signing_key_path: c.signing_key_path.path().to_path_buf(),
        })
        .collect();

    // Post-build registry-push effects for this repo, resolved from
    // the `registries` catalog via the repo's effective
    // `push_to_registries` binding.
    let registry_effects = config
        .repos
        .iter()
        .find(|r| r.forge == args.forge && r.slug == slug)
        .map(|r| effects::registry_push_effects(&config, r))
        .unwrap_or_default();

    let log_base = args
        .log_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("./logs"));
    let gc_root_base = args
        .gc_root_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("/nix/var/nix/gcroots/per-user/argunix"));

    let build_timeout = Duration::from_secs(config.schedule.build_timeout_seconds as u64);
    let push_timeout = Duration::from_secs(300);

    // Single-shot mode shares the registry-state convention with the
    // long-running daemon: `./registry-state` next to the sqlite db.
    let registry_state = std::sync::Arc::new(argunix_registry::RegistryState::new(
        std::path::PathBuf::from("./registry-state"),
    ));
    if let Err(e) = registry_state.ensure_dirs().await {
        tracing::warn!(error = %e, "failed to create registry state dirs");
    }

    // Persist every job up front so the multi-arch grouping sees all
    // of the eval's job ids before the build phase.
    let mut persisted: Vec<(argunix_eval::JobSpec, JobId)> = Vec::new();
    for spec in jobs {
        let job_id = persist_job(&store, eval_id, &spec).await?;
        persisted.push((spec, job_id));
    }
    let specs_by_id: std::collections::HashMap<JobId, argunix_eval::JobSpec> =
        persisted.iter().map(|(s, j)| (*j, s.clone())).collect();
    let suppressed = crate::multiarch::suppressed_push_job_ids(&specs_by_id);

    let mut summary = Summary::default();
    for (spec, job_id) in &persisted {
        let outcome = build_one_job(
            &store,
            &registry_state,
            repo_id,
            eval_id,
            *job_id,
            spec,
            &push_caches,
            &registry_effects,
            suppressed.contains(job_id),
            &args.git_ref,
            &args.sha,
            push_timeout,
            build_timeout,
            &log_base,
            &gc_root_base,
        )
        .await;
        match outcome {
            Ok(s) => summary.add(s),
            Err(e) => {
                tracing::error!(error = %e, attr = %spec.attr_path, "job pipeline error");
                summary.errors += 1;
            }
        }
    }

    <argunix_store::SqlxStore as EvalStore>::finish(&store, eval_id, EvalStatus::Done, Utc::now())
        .await?;

    // Cross-system multi-arch fan-in. Records `effect_runs` rows; no
    // forge checks are posted for it (neither here nor in the daemon).
    crate::multiarch::run_fan_in(
        &store,
        eval_id,
        &specs_by_id,
        &config,
        &args.forge,
        &args.slug,
        None,
        &args.git_ref,
        &args.sha,
    )
    .await;
    println!(
        "eval={eval_id} cached={c} success={s} failure={f} skipped={k} errors={e}",
        eval_id = eval_id.get(),
        c = summary.cached,
        s = summary.success,
        f = summary.failure,
        k = summary.skipped,
        e = summary.errors,
    );
    Ok(())
}

async fn persist_job(
    store: &argunix_store::SqlxStore,
    eval_id: EvalId,
    spec: &argunix_eval::JobSpec,
) -> anyhow::Result<JobId> {
    // Same capture as in worker::persist_job — kept in sync so jobs
    // landed via either path show up in the synthetic-flake endpoint.
    let main_program = spec
        .meta
        .get("mainProgram")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let job_id = <argunix_store::SqlxStore as JobStore>::create(
        store,
        argunix_store::NewJob {
            eval_id,
            attr_path: spec.attr_path.clone(),
            drv_path: spec.drv_path.clone(),
            system: spec.system.clone().unwrap_or_else(|| "unknown".to_string()),
            main_program,
            outputs: spec.outputs.clone(),
        },
    )
    .await
    .context("creating job row")?;
    if spec.error.is_some() {
        // Eval errors land as terminal failures with no build attempted.
        <argunix_store::SqlxStore as JobStore>::finish(
            store,
            job_id,
            JobStatus::Failure,
            Utc::now(),
            None,
            None,
            &JobPhaseMetrics::default(),
        )
        .await?;
    }
    Ok(job_id)
}

#[allow(clippy::too_many_arguments)]
async fn build_one_job(
    store: &argunix_store::SqlxStore,
    registry_state: &Arc<argunix_registry::RegistryState>,
    repo_id: RepoId,
    eval_id: EvalId,
    job_id: JobId,
    spec: &argunix_eval::JobSpec,
    push_caches: &[argunix_build::PushCache],
    registry_effects: &[Arc<dyn argunix_effects::Effect>],
    is_multiarch_member: bool,
    git_ref: &str,
    sha: &str,
    push_timeout: Duration,
    build_timeout: Duration,
    log_base: &Path,
    gc_root_base: &Path,
) -> anyhow::Result<JobStatus> {
    if spec.error.is_some() {
        return Ok(JobStatus::Failure);
    }

    let Some(drv_path) = spec.drv_path.clone() else {
        // Jobs without a drv path can't be built — most likely an eval-time
        // error already recorded above.
        return Ok(JobStatus::Failure);
    };

    // `is_cached` is set by `nix-eval-jobs --check-cache-status` when
    // the output is already in the local store or fetchable from a
    // configured system-wide substituter. Short-circuit before any
    // build runs — mirrors the worker's behaviour. Argunix doesn't
    // keep its own cache probe any more; system-wide nix.settings is
    // the single source of truth for "is this path cached".
    if spec.is_cached {
        if let Some(output) = spec.primary_output() {
            tracing::info!(job_id = job_id.get(), output = %output, "local store hit");
            let output = output.to_string();
            <argunix_store::SqlxStore as JobStore>::finish(
                store,
                job_id,
                JobStatus::Cached,
                Utc::now(),
                None,
                Some(&output),
                &JobPhaseMetrics::default(),
            )
            .await?;
            // Post-build effects run for cache hits too: a cached
            // output is valid locally, but the external registry /
            // binary cache may not have it yet. The output closure is
            // already realised, so the effects have everything they
            // need.
            let outputs = [output];
            if !push_caches.is_empty() {
                let _ = effects::run_cache_push_effects(
                    store,
                    job_id,
                    &outputs,
                    push_caches,
                    push_timeout,
                )
                .await;
            }
            if !registry_effects.is_empty() {
                run_registry_effects_cli(
                    store,
                    job_id,
                    spec,
                    repo_id,
                    git_ref,
                    sha,
                    &outputs,
                    registry_effects,
                    is_multiarch_member,
                )
                .await;
            }
            if spec.image_format.is_some() {
                effects::record_image_artifacts(
                    store,
                    job_id,
                    spec.attr_path.as_str(),
                    &outputs,
                    &argunix_effects::sbom::runtime_roots(&spec.meta),
                )
                .await;
            }
            return Ok(JobStatus::Cached);
        }
    }

    <argunix_store::SqlxStore as JobStore>::start(store, job_id, Utc::now()).await?;

    let log_path = log_base
        .join(repo_id.get().to_string())
        .join(eval_id.get().to_string())
        .join(format!("{}.log.zst", job_id.get()));
    let gc_root = argunix_build::gc_root_path(gc_root_base, repo_id, eval_id, job_id);
    if let Some(parent) = gc_root.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            tracing::warn!(error = %e, dir = %parent.display(), "failed to create gcroot parent dir");
        }
    }
    let request = argunix_build::BuildRequest {
        drv_path: drv_path.clone(),
        log_path: log_path.clone(),
        timeout: build_timeout,
        log_limit: argunix_build::LogCaptureLimit::default(),
        gc_root: Some(gc_root),
        // The single-shot `argunix build` CLI runs locally — no
        // dynamic pool involvement. Falls through to the host's
        // `nix.buildMachines` like before.
    };
    let outcome = argunix_build::run_build(&request)
        .await
        .with_context(|| format!("building {drv_path}"))?;

    let log_path_str = log_path.to_string_lossy().into_owned();
    match outcome.status {
        argunix_build::BuildStatus::Success => {
            // gcroot was registered atomically by `nix-store --realise --add-root`.
            let primary_output = outcome
                .output_paths
                .first()
                .cloned()
                .or_else(|| spec.primary_output().map(String::from));

            // Binary-cache push — a post-build effect, recorded one
            // `effect_runs` row per cache. Best-effort: a flaky cache
            // is logged, the job stays a local success.
            if !push_caches.is_empty() && !outcome.output_paths.is_empty() {
                let _ = effects::run_cache_push_effects(
                    store,
                    job_id,
                    &outcome.output_paths,
                    push_caches,
                    push_timeout,
                )
                .await;
            }

            <argunix_store::SqlxStore as JobStore>::finish(
                store,
                job_id,
                JobStatus::Success,
                Utc::now(),
                Some(&log_path_str),
                primary_output.as_deref(),
                &JobPhaseMetrics::default(),
            )
            .await?;

            // Post-build registry-push effects: push the built image
            // out to every external registry the repo binds. Recorded
            // in `effect_runs`; best-effort like the cache push.
            if !registry_effects.is_empty() {
                run_registry_effects_cli(
                    store,
                    job_id,
                    spec,
                    repo_id,
                    git_ref,
                    sha,
                    &outcome.output_paths,
                    registry_effects,
                    is_multiarch_member,
                )
                .await;
            }
            if spec.image_format.is_some() {
                effects::record_image_artifacts(
                    store,
                    job_id,
                    spec.attr_path.as_str(),
                    &outcome.output_paths,
                    &argunix_effects::sbom::runtime_roots(&spec.meta),
                )
                .await;
            }

            // Internal embedded registry (argunix's own /v2 surface) —
            // independent of, and complementary to, the external push.
            // Only `docker` images are ingested here; an `oci` image
            // (potentially multi-arch) goes out via the registry-push
            // effect only — the embedded converter is single-manifest.
            match spec.image_format {
                Some(ImageFormat::Docker) => {
                    try_publish_docker_image_cli(
                        store,
                        registry_state,
                        repo_id,
                        eval_id,
                        job_id,
                        spec,
                        primary_output.as_deref(),
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
            <argunix_store::SqlxStore as JobStore>::finish(
                store,
                job_id,
                JobStatus::Failure,
                Utc::now(),
                Some(&log_path_str),
                None,
                &JobPhaseMetrics::default(),
            )
            .await?;
            Ok(JobStatus::Failure)
        }
    }
}

/// Build an [`OutputContext`] for `job_id`'s output and run every
/// registry-push effect against it, recording `effect_runs` rows.
/// Single-shot CLI counterpart of `worker::run_registry_effects`.
#[allow(clippy::too_many_arguments)]
async fn run_registry_effects_cli(
    store: &argunix_store::SqlxStore,
    job_id: JobId,
    spec: &argunix_eval::JobSpec,
    repo_id: RepoId,
    git_ref: &str,
    sha: &str,
    output_paths: &[String],
    registry_effects: &[Arc<dyn argunix_effects::Effect>],
    is_multiarch_member: bool,
) {
    let repo = match <argunix_store::SqlxStore as RepoStore>::get(store, repo_id).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            tracing::warn!(repo_id = repo_id.get(), "effects: repo row missing");
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "effects: repo lookup failed");
            return;
        }
    };
    let sbom_roots = argunix_effects::sbom::runtime_roots(&spec.meta);
    let ctx = argunix_effects::OutputContext {
        forge: &repo.forge,
        repo_slug: repo.slug.as_str(),
        attr_path: spec.attr_path.as_str(),
        system: spec.system.as_deref().unwrap_or("unknown"),
        git_ref,
        default_branch: repo.default_branch.as_deref(),
        sha,
        image_format: spec.image_format,
        output_paths,
        sbom_runtime_roots: &sbom_roots,
    };
    // A job that is one arch slice of a multi-arch group must run
    // neither its own `registry-push` nor its own `sbom-attach` — the
    // post-build fan-in pushes the assembled index and attaches a
    // per-arch SBOM to each per-arch manifest digest.
    let reg_effects: Vec<Arc<dyn argunix_effects::Effect>> = registry_effects
        .iter()
        .filter(|e| !(is_multiarch_member && matches!(e.kind(), "registry-push" | "sbom-attach")))
        .cloned()
        .collect();
    effects::run_effects(store, job_id, &reg_effects, &ctx).await;
}

/// Single-shot CLI variant of the worker's docker-image publish. Same
/// best-effort policy: any failure is logged at `warn` and the job
/// stays `Success`.
async fn try_publish_docker_image_cli(
    store: &argunix_store::SqlxStore,
    state: &Arc<argunix_registry::RegistryState>,
    repo_id: RepoId,
    eval_id: EvalId,
    job_id: JobId,
    spec: &argunix_eval::JobSpec,
    output_path: Option<&str>,
) {
    let repo = match <argunix_store::SqlxStore as RepoStore>::get(store, repo_id).await {
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
    let eval = match <argunix_store::SqlxStore as EvalStore>::get(store, eval_id).await {
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

#[derive(Default)]
struct Summary {
    success: usize,
    failure: usize,
    cached: usize,
    skipped: usize,
    errors: usize,
}

impl Summary {
    fn add(&mut self, status: JobStatus) {
        match status {
            JobStatus::Success => self.success += 1,
            JobStatus::Failure => self.failure += 1,
            JobStatus::Cached => self.cached += 1,
            JobStatus::SkippedNoBuilder => self.skipped += 1,
            _ => {}
        }
    }
}

/// Drop any repo no longer present in `config.repos`, plus its
/// evaluations / jobs / queue rows / forge_status rows / build logs /
/// GC roots. Best-effort filesystem cleanup: failures log a warning
/// but don't block startup. Runs in the same task as `serve()` before
/// the worker is spawned, so nothing is in-flight against the rows
/// we're about to delete.
pub(crate) async fn prune_orphan_state(
    config: &argunix_config::Config,
    store: &argunix_store::SqlxStore,
    log_dir: &Path,
    gc_root_dir: &Path,
) {
    let keep: Vec<(String, argunix_domain::Slug)> = config
        .repos
        .iter()
        .map(|r| (r.forge.clone(), r.slug.clone()))
        .collect();
    let pruned = match <argunix_store::SqlxStore as argunix_store::RepoStore>::prune_repos_not_in(
        store, &keep,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "config-driven prune failed; orphan rows may remain");
            return;
        }
    };
    if pruned.is_empty() {
        return;
    }
    for repo in &pruned {
        let id = repo.id.get().to_string();
        let logs_path = log_dir.join(&id);
        let gc_path = gc_root_dir.join(&id);
        if let Err(e) = tokio::fs::remove_dir_all(&logs_path).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(error = %e, dir = %logs_path.display(), "failed to remove orphan log dir");
            }
        }
        if let Err(e) = tokio::fs::remove_dir_all(&gc_path).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(error = %e, dir = %gc_path.display(), "failed to remove orphan gcroot dir");
            }
        }
        tracing::info!(
            forge = %repo.forge,
            slug = %repo.slug,
            repo_id = repo.id.get(),
            "pruned orphan repo no longer in config",
        );
    }
    tracing::info!(count = pruned.len(), "config-driven prune pass complete");
}

/// If `config.builder_enrollment` is set, load/generate the embedded
/// SSH host key under `./builder-host-key`, read the enrollment token,
/// and spawn the builder-pool SSH server on the configured listen
/// address. No-op when `builder_enrollment` is absent — operators not
/// using the dynamic pool keep paying nothing for it.
async fn spawn_builder_server_if_configured(
    config: &argunix_config::Config,
    store: &argunix_store::SqlxStore,
    registry: Arc<argunix_builders::BuilderRegistry>,
) -> anyhow::Result<Option<tokio::task::JoinHandle<()>>> {
    let Some(enroll) = config.builder_enrollment.as_ref() else {
        return Ok(None);
    };

    let listen: std::net::SocketAddr = enroll
        .listen
        .parse()
        .with_context(|| format!("parsing builder_enrollment.listen `{}`", enroll.listen))?;

    let host_key_path = PathBuf::from("./builder-host-key");
    let host_key = argunix_builders::load_or_generate(&host_key_path).with_context(|| {
        format!(
            "loading/generating builder host key at {}",
            host_key_path.display()
        )
    })?;

    let token_bytes = tokio::fs::read(enroll.token_path.path())
        .await
        .with_context(|| {
            format!(
                "reading builder enrollment token at {}",
                enroll.token_path.path().display()
            )
        })?;
    let token = Arc::new(strip_trailing_newlines(token_bytes));

    let server_cfg = argunix_builders::ServerConfig {
        listen,
        host_key,
        enrollment_token: token,
        store: Arc::new(store.clone()),
        registry,
    };

    let handle = tokio::spawn(async move {
        if let Err(e) = argunix_builders::BuilderServer::run(server_cfg).await {
            tracing::error!(error = %e, "builder enrollment server exited");
        }
    });

    tracing::info!(%listen, "builder enrollment server listening");
    Ok(Some(handle))
}

fn strip_trailing_newlines(mut v: Vec<u8>) -> Vec<u8> {
    while matches!(v.last(), Some(b'\n') | Some(b'\r')) {
        v.pop();
    }
    v
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("argunix=info,warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .json()
        .init();
}
