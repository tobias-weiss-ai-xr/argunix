//! Unix-socket control server (M8).
//!
//! Listens on `socket_path`, accepts JSON-lines requests from
//! `medusactl`, dispatches to the right handler, sends a JSON-lines
//! response, closes the connection. Single-shot per connection;
//! adding streaming responses would be a future extension.
//!
//! Each accepted connection is handled in its own tokio task so a
//! slow `reload` (which goes off-host to a forge for `ensure_webhooks`)
//! doesn't block subsequent `status` queries.

use anyhow::Context;
use medusa_control::{Request, Response};
use medusa_web::{AppState, ConfigSnapshot};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// Everything the control server needs to handle requests. Lives as
/// long as the daemon and is cloned into per-connection tasks.
#[derive(Clone)]
pub struct ControlContext {
    pub socket_path: PathBuf,
    pub app_state: AppState,
    pub store: medusa_store::SqlxStore,
    pub log_dir: PathBuf,
    pub gc_root_dir: PathBuf,
    pub config_path: PathBuf,
    pub skip_secret_check: bool,
}

pub fn spawn(ctx: ControlContext) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = run(ctx).await {
            tracing::error!(error = %e, "control server exited with error");
        }
    })
}

async fn run(ctx: ControlContext) -> anyhow::Result<()> {
    // RuntimeDirectory= creates the parent dir; if a stale socket
    // file is there from a prior crashed run, remove it. (NixOS's
    // RuntimeDirectoryPreserve= defaults to "no", so this should be
    // a no-op in production.)
    if ctx.socket_path.exists() {
        if let Err(e) = tokio::fs::remove_file(&ctx.socket_path).await {
            tracing::warn!(
                error = %e,
                path = %ctx.socket_path.display(),
                "couldn't remove stale control socket"
            );
        }
    }
    let listener = UnixListener::bind(&ctx.socket_path)
        .with_context(|| format!("binding {}", ctx.socket_path.display()))?;
    tracing::info!(
        path = %ctx.socket_path.display(),
        "control socket listening",
    );
    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "accept on control socket failed; retrying");
                continue;
            }
        };
        let ctx = ctx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, ctx).await {
                tracing::warn!(error = %e, "control connection failed");
            }
        });
    }
}

async fn handle_connection(stream: UnixStream, ctx: ControlContext) -> anyhow::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Ok(());
    }
    let response = match serde_json::from_str::<Request>(line.trim_end_matches('\n')) {
        Ok(req) => dispatch(req, &ctx).await,
        Err(e) => Response::error(format!("malformed request: {e}")),
    };
    let mut buf = serde_json::to_vec(&response)?;
    buf.push(b'\n');
    write_half.write_all(&buf).await?;
    write_half.shutdown().await?;
    Ok(())
}

async fn dispatch(req: Request, ctx: &ControlContext) -> Response {
    match req {
        Request::Reload { config_path } => {
            let path = config_path.unwrap_or_else(|| ctx.config_path.clone());
            match handle_reload(path, ctx).await {
                Ok(details) => Response::ok_with(details),
                Err(e) => Response::error(format!("{e:#}")),
            }
        }
        Request::Status => Response::ok_with(handle_status(ctx).await),
    }
}

async fn handle_reload(
    config_path: PathBuf,
    ctx: &ControlContext,
) -> anyhow::Result<serde_json::Value> {
    tracing::info!(path = %config_path.display(), "reload requested");

    // Q77: validate first, then atomically swap. Any error before the
    // swap leaves the running daemon untouched.
    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Reloading]);

    let new_config = medusa_config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;
    if !ctx.skip_secret_check {
        new_config
            .validate_secrets_exist()
            .context("validating secret files")?;
    }
    let new_providers = medusa_web::build_providers(&new_config)
        .await
        .context("building forge providers")?;

    let n_forges = new_providers.len();
    let n_repos = new_config.repos.len();

    let new_snap = Arc::new(ConfigSnapshot {
        config: Arc::new(new_config),
        providers: Arc::new(new_providers),
    });
    ctx.app_state.current.store(new_snap.clone());
    tracing::info!(forges = n_forges, repos = n_repos, "config swapped");

    // Run the same post-load housekeeping the startup path runs:
    // prune orphaned repos that no longer appear, then auto-install
    // webhooks for any new repos. Both are idempotent.
    super::prune_orphan_state(&new_snap.config, &ctx.store, &ctx.log_dir, &ctx.gc_root_dir).await;
    medusa_web::ensure_webhooks(&new_snap.config, &new_snap.providers, &ctx.store).await;

    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]);
    Ok(serde_json::json!({
        "forges": n_forges,
        "repos": n_repos,
    }))
}

async fn handle_status(ctx: &ControlContext) -> serde_json::Value {
    let snap = ctx.app_state.current.load_full();
    let uptime = ctx.app_state.started_at.elapsed().as_secs();
    serde_json::json!({
        "uptime_seconds": uptime,
        "forges": snap.providers.len(),
        "repos": snap.config.repos.len(),
        "external_url": snap.config.external_url,
        "paused_forges": ctx.app_state.pauses.snapshot(),
    })
}
