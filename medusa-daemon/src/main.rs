use anyhow::Context;
use clap::Parser;
use std::path::PathBuf;

/// medusa CI daemon (M1 skeleton).
#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// Path to the medusa YAML config.
    #[arg(long, value_name = "PATH")]
    config: PathBuf,

    /// Skip checking that every secret file referenced by the config exists.
    /// Useful for `medusa --check`-style validation in CI.
    #[arg(long)]
    skip_secret_check: bool,
}

fn main() -> anyhow::Result<()> {
    init_tracing();
    let args = Args::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;
    runtime.block_on(run(args))
}

async fn run(args: Args) -> anyhow::Result<()> {
    // Step 2 of the boot sequence (Q106): load + validate config.
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

    // Step 1 of the boot sequence: open db and run migrations. The order is
    // reversed in M1 only because we want config errors to surface before
    // touching the filesystem; a future restructure can move db open back
    // ahead of config load when we have a proper init pipeline.
    let pool = medusa_store::open_at(std::path::Path::new("./db.sqlite"))
        .await
        .context("opening sqlite database at ./db.sqlite")?;
    let store = medusa_store::SqlxStore::new(pool);

    // Step 5 of the boot sequence: any rows still marked `running` are
    // leftovers from a crashed previous instance. Mark them interrupted so
    // the (future) scheduler can re-queue them.
    let n = <medusa_store::SqlxStore as medusa_store::JobStore>::mark_running_interrupted(&store)
        .await
        .context("recovering interrupted jobs")?;
    if n > 0 {
        tracing::info!(count = n, "marked previously-running jobs as interrupted");
    }

    println!("ready");
    Ok(())
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
