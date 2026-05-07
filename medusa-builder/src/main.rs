//! medusa-builder agent entry point.
//!
//! Loads / generates the persistent identity, discovers capabilities
//! once at startup (re-discovered on each reconnect cycle), and runs
//! the dial-and-serve loop. SIGTERM triggers a clean disconnect via
//! a `shutdown` control message.

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use medusa_builder_agent::{AgentConfig, discover_capabilities, load_or_generate, run};
use medusa_domain::BuilderName;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::lookup_host;
use tokio::signal::unix::{SignalKind, signal};
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
    let name_str = cli
        .name
        .or_else(|| hostname::get_hostname())
        .unwrap_or_else(|| "medusa-builder".into());
    let name = BuilderName::new(&name_str)
        .map_err(|e| anyhow!("invalid builder name `{name_str}`: {e}"))?;

    // Resolve `host:port` once at startup. The agent reconnects against
    // this fixed address; if DNS changes, the unit needs a restart.
    let host_port = format!("{}:{}", cli.medusa_host, cli.medusa_port);
    let medusa_addr = lookup_host(&host_port)
        .await
        .with_context(|| format!("resolving medusa address `{host_port}`"))?
        .next()
        .ok_or_else(|| anyhow!("no addresses resolved for `{host_port}`"))?;

    // Read the enrollment token if a path was provided. Once medusa's
    // TOFU row is in place, the operator removes the file (or the
    // `enrollment_token_path` line) and pubkey auth takes over.
    let enrollment_token = match cli.enrollment_token_path.as_ref() {
        Some(path) => {
            let bytes = tokio::fs::read(path)
                .await
                .with_context(|| format!("reading enrollment token at {}", path.display()))?;
            Some(Arc::new(strip_trailing_newlines(bytes)))
        }
        None => None,
    };

    let cfg = AgentConfig {
        medusa: medusa_addr,
        identity,
        enrollment_token,
        name,
        capabilities: caps.inner,
        reconnect_initial_backoff: AgentConfig::default_backoff(),
        nix_store_bin: AgentConfig::default_nix_store_bin(),
        nix_daemon_socket: AgentConfig::default_nix_daemon_socket(),
        // Pin medusa's SSH host key under the agent's state dir so a
        // future server-key swap is caught at TOFU rather than
        // silently accepted.
        medusa_host_key_path: Some(cli.state_dir.join("medusa-host-key.pub")),
    };

    tracing::info!(
        name = %name_str,
        medusa = %host_port,
        medusa_resolved = %medusa_addr,
        "agent starting",
    );

    let shutdown = wait_for_shutdown();
    run(cfg, shutdown).await.context("agent loop")?;
    Ok(())
}

/// Wait for SIGTERM or SIGINT; resolves on either.
async fn wait_for_shutdown() {
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "could not install SIGTERM handler");
            std::future::pending::<()>().await;
            return;
        }
    };
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "could not install SIGINT handler");
            std::future::pending::<()>().await;
            return;
        }
    };
    tokio::select! {
        _ = sigterm.recv() => tracing::info!("received SIGTERM, shutting down"),
        _ = sigint.recv()  => tracing::info!("received SIGINT, shutting down"),
    }
}

/// Trim trailing newlines/whitespace from a token file. Operators
/// typically `echo "secret" > /run/credentials/...` which appends a
/// newline; medusa's server side compares the exact bytes.
fn strip_trailing_newlines(mut v: Vec<u8>) -> Vec<u8> {
    while matches!(v.last(), Some(b'\n') | Some(b'\r')) {
        v.pop();
    }
    v
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
