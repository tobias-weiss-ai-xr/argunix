//! medusa-builder agent entry point.
//!
//! Loads / generates the persistent identity, discovers capabilities
//! once at startup (re-discovered on each reconnect cycle), and runs
//! the dial-and-serve loop. SIGTERM triggers a clean disconnect via
//! a `shutdown` control message.

use anyhow::{Context, Result};
use clap::Parser;
use medusa_builder_agent::{discover_capabilities, load_or_generate};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "medusa-builder",
    about = "Dial medusa, register as a builder, serve nix-store builds"
)]
struct Cli {
    /// Hostname or IP of the medusa daemon.
    #[arg(long)]
    medusa_host: String,
    /// Builder-enrollment SSH port on medusa (default 2222).
    #[arg(long, default_value_t = 2222)]
    medusa_port: u16,
    /// Path to the file containing the shared enrollment token.
    /// Used only on first connect or after `medusactl builders revoke`.
    /// Pubkey-auth uses the persistent identity in `state-dir`.
    #[arg(long)]
    enrollment_token_path: Option<PathBuf>,
    /// Builder name reported in the `hello` message. Defaults to the
    /// machine's hostname.
    #[arg(long)]
    name: Option<String>,
    /// Persistent state directory. The identity key lives at
    /// `<state-dir>/identity.ed25519`.
    #[arg(long, default_value = "/var/lib/medusa-builder")]
    state_dir: PathBuf,
    /// `nix` binary to invoke for `show-config`. Defaults to whatever
    /// `nix` resolves to on the agent's PATH.
    #[arg(long, default_value = "nix")]
    nix_bin: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .try_init();
    let cli = Cli::parse();

    let identity_path = cli.state_dir.join("identity.ed25519");
    let identity = load_or_generate(&identity_path)
        .with_context(|| format!("loading identity at {}", identity_path.display()))?;
    tracing::info!(
        identity = ?identity,
        path = %identity_path.display(),
        "identity loaded",
    );

    let caps = discover_capabilities(&cli.nix_bin).await.with_context(|| {
        format!(
            "discovering capabilities via `{} show-config --json`",
            cli.nix_bin
        )
    })?;
    tracing::info!(
        systems = ?caps.inner.systems,
        features = ?caps.inner.features,
        max_jobs = caps.inner.max_jobs,
        nix_version = %caps.inner.nix_version,
        "capabilities discovered",
    );

    // Resolve builder name.
    let name = cli
        .name
        .or_else(|| hostname::get_hostname())
        .unwrap_or_else(|| "medusa-builder".into());

    // The actual dial-and-serve loop is the next slice of M13b. For
    // now we surface the inputs we'd feed it and exit cleanly so the
    // binary builds and `medusa-builder --help` works as a smoke
    // check on the bin target.
    tracing::info!(
        name = %name,
        medusa = %format!("{}:{}", cli.medusa_host, cli.medusa_port),
        enrollment_token = ?cli.enrollment_token_path,
        "agent inputs gathered (dial loop pending in M13b slice 2)",
    );
    Ok(())
}

/// Minimal hostname resolver — the `hostname` crate is overkill for
/// what we need (a single nix syscall), and pulling in another dep
/// for something readable from /etc/hostname or `uname` is overkill.
mod hostname {
    pub fn get_hostname() -> Option<String> {
        // Prefer /etc/hostname (NixOS, systemd), fall back to gethostname.
        if let Ok(s) = std::fs::read_to_string("/etc/hostname") {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        // gethostname via libc — but we don't want to pull in libc just
        // for this. The HOSTNAME env var is set by most shells.
        std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty())
    }
}
