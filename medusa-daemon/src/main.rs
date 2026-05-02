use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
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
}

#[derive(Args, Debug)]
struct RunArgs {
    /// Path to the medusa YAML config.
    #[arg(long, value_name = "PATH")]
    config: PathBuf,

    /// Skip checking that every secret file referenced by the config exists.
    /// Useful for `medusa run --skip-secret-check` validation in CI.
    #[arg(long)]
    skip_secret_check: bool,
}

#[derive(Args, Debug)]
struct EvalArgs {
    /// Path to a local checkout containing a `flake.nix`.
    #[arg(long, value_name = "PATH")]
    src: PathBuf,

    /// Comma-separated systems to evaluate fragments under.
    /// Defaults to the host's local system.
    #[arg(long, value_delimiter = ',', value_name = "SYSTEM[,SYSTEM]")]
    systems: Option<Vec<String>>,

    /// Wall-clock seconds per fragment for `nix-eval-jobs`.
    #[arg(long, default_value_t = 600, value_name = "SECONDS")]
    timeout_seconds: u64,
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
    }
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

    let pool = medusa_store::open_at(std::path::Path::new("./db.sqlite"))
        .await
        .context("opening sqlite database at ./db.sqlite")?;
    let store = medusa_store::SqlxStore::new(pool);

    let n = <medusa_store::SqlxStore as medusa_store::JobStore>::mark_running_interrupted(&store)
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
    tracing::info!(
        src = %request.source_path.display(),
        ?systems,
        "starting offline evaluation",
    );

    let jobs = medusa_eval::evaluate(&request)
        .await
        .context("running nix-eval-jobs")?;
    tracing::info!(count = jobs.len(), "evaluation produced jobs");

    let serialised = serde_json::to_string_pretty(&jobs).context("serialising job list to JSON")?;
    println!("{serialised}");
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
