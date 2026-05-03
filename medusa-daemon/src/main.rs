mod worker;

use anyhow::{Context, anyhow};
use chrono::Utc;
use clap::{Args, Parser, Subcommand};
use medusa_domain::{EvalId, EvalStatus, JobId, JobStatus, RepoId, Sha, Slug};
use medusa_store::{EvalStore, JobStore, RepoStore};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(version, about = "medusa CI daemon")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the medusa daemon (M1 service-mode skeleton).
    Run(RunArgs),
    /// Evaluate a local flake and print discovered jobs as JSON.
    Eval(EvalArgs),
    /// Evaluate and build a local flake end-to-end (M3 single-shot pipeline).
    Build(BuildArgs),
    /// Run as an HTTP daemon: accept webhooks, queue evaluations.
    Serve(ServeArgs),
}

#[derive(Args, Debug)]
struct ServeArgs {
    /// Path to the medusa YAML config.
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
    /// Path to the medusa YAML config (used for binary cache list).
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
    /// Git ref recorded for the evaluation, e.g. `refs/heads/main`.
    /// Defaults to a placeholder so callers without git context still work.
    #[arg(long, value_name = "REF", default_value = "refs/heads/HEAD")]
    git_ref: String,
    /// 40-hex-char SHA recorded for the evaluation. If omitted, a synthetic
    /// zero SHA is recorded (real cloning lands in M5).
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
    /// Wall-clock seconds for each `nix-store --realise`.
    #[arg(long, default_value_t = 7200, value_name = "SECONDS")]
    build_timeout_seconds: u64,
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
    let config = medusa_config::load(&args.config)
        .with_context(|| format!("loading config from {}", args.config.display()))?;
    if !args.skip_secret_check {
        config
            .validate_secrets_exist()
            .context("validating secret files")?;
    }
    let providers = medusa_web::build_providers(&config)
        .await
        .context("building forge providers")?;
    tracing::info!(
        forges = providers.len(),
        repos = config.repos.len(),
        "providers initialised"
    );

    let pool = medusa_store::open_at(Path::new("./db.sqlite"))
        .await
        .context("opening sqlite database at ./db.sqlite")?;
    let store = medusa_store::SqlxStore::new(pool);

    let n = <medusa_store::SqlxStore as JobStore>::mark_running_interrupted(&store)
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
        .unwrap_or_else(|| PathBuf::from("/nix/var/nix/gcroots/per-user/medusa"));
    let systems = args
        .systems
        .clone()
        .unwrap_or_else(medusa_eval::detect_local_systems);

    let providers_arc = Arc::new(providers);
    let config_arc = Arc::new(config);
    let pauses = std::sync::Arc::new(medusa_web::PauseRegistry::new());
    let cancellations = std::sync::Arc::new(medusa_web::CancelRegistry::new());

    // Config-driven cleanup: at every startup, prune any repo (and
    // its evaluations / jobs / logs / GC roots) that no longer
    // appears in `config.repos`. This catches orphans left behind
    // when an operator renames a forge entry or removes a repo from
    // the YAML.
    prune_orphan_state(&config_arc, &store, &log_dir, &gc_root_dir).await;

    // Auto-install / refresh webhooks at every startup. Best-effort:
    // a forge being unreachable doesn't block daemon startup.
    medusa_web::ensure_webhooks(&config_arc, &providers_arc, &store).await;

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let worker_ctx = worker::WorkerContext {
        config: config_arc.clone(),
        providers: providers_arc.clone(),
        store: store.clone(),
        work_dir,
        log_dir,
        gc_root_dir,
        eval_timeout: Duration::from_secs(600),
        build_timeout: Duration::from_secs(7200),
        clone_timeout: Duration::from_secs(300),
        systems,
        pauses: pauses.clone(),
        cancellations: cancellations.clone(),
    };
    let worker_handle = worker::spawn(worker_ctx, rx);

    let listen = args
        .listen
        .clone()
        .unwrap_or_else(|| config_arc.listen.clone());
    let coalesce = std::sync::Arc::new(medusa_web::CoalescePool::new(
        std::time::Duration::from_secs(config_arc.schedule.webhook_coalesce_seconds.into()),
    ));
    let inner = medusa_web::AppStateInner {
        config: config_arc,
        providers: (*providers_arc).clone(),
        store,
        work_dispatcher: tx,
        coalesce,
        pauses,
        cancellations,
    };
    let router = medusa_web::router_from_inner(inner);

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    let local = listener.local_addr().context("reading local addr")?;
    println!("listening on {local}");
    tracing::info!(%local, "medusa http server ready");

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("axum serve")?;

    // axum has finished serving; the only remaining `Sender` for the
    // worker channel lives inside the dropped `AppStateInner`, so the
    // worker will see the channel close and exit. Await it to drain any
    // in-flight evaluation.
    let _ = worker_handle.await;
    tracing::info!("graceful shutdown complete");
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
    let config = medusa_config::load(&args.config)
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

    let pool = medusa_store::open_at(Path::new("./db.sqlite"))
        .await
        .context("opening sqlite database at ./db.sqlite")?;
    let store = medusa_store::SqlxStore::new(pool);

    let n = <medusa_store::SqlxStore as JobStore>::mark_running_interrupted(&store)
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
        .unwrap_or_else(medusa_eval::detect_local_systems);
    let request = medusa_eval::EvalRequest {
        source_path: args
            .src
            .canonicalize()
            .with_context(|| format!("resolving --src path {}", args.src.display()))?,
        systems: systems.clone(),
        outputs: medusa_eval::DEFAULT_FLAKE_OUTPUTS
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        timeout: Duration::from_secs(args.timeout_seconds),
    };
    tracing::info!(src = %request.source_path.display(), ?systems, "starting offline evaluation");
    let jobs = medusa_eval::evaluate(&request)
        .await
        .context("running nix-eval-jobs")?;
    tracing::info!(count = jobs.len(), "evaluation produced jobs");
    let serialised = serde_json::to_string_pretty(&jobs).context("serialising job list to JSON")?;
    println!("{serialised}");
    Ok(())
}

async fn build(args: BuildArgs) -> anyhow::Result<()> {
    let config = medusa_config::load(&args.config)
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

    let pool = medusa_store::open_at(Path::new("./db.sqlite"))
        .await
        .context("opening sqlite database")?;
    let store = medusa_store::SqlxStore::new(pool);

    let repo_id = <medusa_store::SqlxStore as RepoStore>::upsert(&store, &args.forge, &slug)
        .await
        .context("upserting repo")?;

    let eval_id = <medusa_store::SqlxStore as EvalStore>::create(
        &store,
        medusa_store::NewEvaluation {
            repo_id,
            trigger: args.trigger.clone(),
            git_ref: args.git_ref.clone(),
            sha,
        },
    )
    .await
    .context("creating evaluation row")?;
    <medusa_store::SqlxStore as EvalStore>::set_status(&store, eval_id, EvalStatus::Evaluating)
        .await?;
    tracing::info!(
        repo_id = repo_id.get(),
        eval_id = eval_id.get(),
        "evaluation started"
    );

    let systems = args
        .systems
        .unwrap_or_else(medusa_eval::detect_local_systems);
    let eval_request = medusa_eval::EvalRequest {
        source_path: args
            .src
            .canonicalize()
            .with_context(|| format!("resolving --src path {}", args.src.display()))?,
        systems: systems.clone(),
        outputs: medusa_eval::DEFAULT_FLAKE_OUTPUTS
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        timeout: Duration::from_secs(args.eval_timeout_seconds),
    };
    let jobs = match medusa_eval::evaluate(&eval_request).await {
        Ok(j) => j,
        Err(e) => {
            <medusa_store::SqlxStore as EvalStore>::finish(
                &store,
                eval_id,
                EvalStatus::EvaluationFailed,
                Utc::now(),
            )
            .await?;
            return Err(anyhow::Error::from(e).context("evaluation failed"));
        }
    };
    <medusa_store::SqlxStore as EvalStore>::set_status(&store, eval_id, EvalStatus::Building)
        .await?;
    tracing::info!(count = jobs.len(), "evaluation finished");

    let caches: Vec<medusa_build::CacheRef> = config
        .binary_caches
        .iter()
        .map(|c| medusa_build::CacheRef {
            url: c.url.clone(),
            substitute: c.substitute,
        })
        .collect();

    let log_base = args
        .log_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("./logs"));
    let gc_root_base = args
        .gc_root_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("/nix/var/nix/gcroots/per-user/medusa"));

    let build_timeout = Duration::from_secs(args.build_timeout_seconds);
    let cache_timeout = Duration::from_secs(30);

    let mut summary = Summary::default();
    for spec in jobs {
        let job_id = persist_job(&store, eval_id, &spec).await?;
        let outcome = build_one_job(
            &store,
            repo_id,
            eval_id,
            job_id,
            &spec,
            &caches,
            cache_timeout,
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

    <medusa_store::SqlxStore as EvalStore>::finish(&store, eval_id, EvalStatus::Done, Utc::now())
        .await?;
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
    store: &medusa_store::SqlxStore,
    eval_id: EvalId,
    spec: &medusa_eval::JobSpec,
) -> anyhow::Result<JobId> {
    let job_id = <medusa_store::SqlxStore as JobStore>::create(
        store,
        medusa_store::NewJob {
            eval_id,
            attr_path: spec.attr_path.clone(),
            drv_path: spec.drv_path.clone(),
            system: spec.system.clone().unwrap_or_else(|| "unknown".to_string()),
        },
    )
    .await
    .context("creating job row")?;
    if spec.error.is_some() {
        // Eval errors land as terminal failures with no build attempted.
        <medusa_store::SqlxStore as JobStore>::finish(
            store,
            job_id,
            JobStatus::Failure,
            Utc::now(),
            None,
            None,
        )
        .await?;
    }
    Ok(job_id)
}

#[allow(clippy::too_many_arguments)]
async fn build_one_job(
    store: &medusa_store::SqlxStore,
    repo_id: RepoId,
    eval_id: EvalId,
    job_id: JobId,
    spec: &medusa_eval::JobSpec,
    caches: &[medusa_build::CacheRef],
    cache_timeout: Duration,
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

    if let Some(output) = spec.primary_output() {
        match medusa_build::check_cache(output, caches, cache_timeout).await {
            Ok(medusa_build::CacheCheckResult::Hit { cache_url }) => {
                tracing::info!(
                    job_id = job_id.get(),
                    cache = %cache_url,
                    "cache hit; marking job cached without building",
                );
                <medusa_store::SqlxStore as JobStore>::finish(
                    store,
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
            Err(e) => {
                tracing::warn!(error = %e, "cache check failed; falling through to build");
            }
        }
    }

    <medusa_store::SqlxStore as JobStore>::start(store, job_id, Utc::now()).await?;

    let log_path = log_base
        .join(repo_id.get().to_string())
        .join(eval_id.get().to_string())
        .join(format!("{}.log.zst", job_id.get()));
    let gc_root = medusa_build::gc_root_path(gc_root_base, repo_id, eval_id, job_id);
    if let Some(parent) = gc_root.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            tracing::warn!(error = %e, dir = %parent.display(), "failed to create gcroot parent dir");
        }
    }
    let request = medusa_build::BuildRequest {
        drv_path: drv_path.clone(),
        log_path: log_path.clone(),
        timeout: build_timeout,
        log_limit: medusa_build::LogCaptureLimit::default(),
        gc_root: Some(gc_root),
    };
    let outcome = medusa_build::run_build(&request)
        .await
        .with_context(|| format!("building {drv_path}"))?;

    let log_path_str = log_path.to_string_lossy().into_owned();
    match outcome.status {
        medusa_build::BuildStatus::Success => {
            // gcroot was registered atomically by `nix-store --realise --add-root`.
            let primary_output = outcome
                .output_paths
                .first()
                .cloned()
                .or_else(|| spec.primary_output().map(String::from));

            <medusa_store::SqlxStore as JobStore>::finish(
                store,
                job_id,
                JobStatus::Success,
                Utc::now(),
                Some(&log_path_str),
                primary_output.as_deref(),
            )
            .await?;
            Ok(JobStatus::Success)
        }
        medusa_build::BuildStatus::Failure => {
            <medusa_store::SqlxStore as JobStore>::finish(
                store,
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
async fn prune_orphan_state(
    config: &medusa_config::Config,
    store: &medusa_store::SqlxStore,
    log_dir: &Path,
    gc_root_dir: &Path,
) {
    let keep: Vec<(String, medusa_domain::Slug)> = config
        .repos
        .iter()
        .map(|r| (r.forge.clone(), r.slug.clone()))
        .collect();
    let pruned = match <medusa_store::SqlxStore as medusa_store::RepoStore>::prune_repos_not_in(
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

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("medusa=info,warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .json()
        .init();
}
