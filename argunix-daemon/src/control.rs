//! Unix-socket control server.
//!
//! Listens on `socket_path`, accepts JSON-lines requests from
//! `argunixctl`, dispatches to the right handler, sends a JSON-lines
//! response, closes the connection. Single-shot per connection;
//! adding streaming responses would be a future extension.
//!
//! Each accepted connection is handled in its own tokio task so a
//! slow `reload` (which goes off-host to a forge for `ensure_webhooks`)
//! doesn't block subsequent `status` queries.

use anyhow::Context;
use argunix_builders::BuilderRegistry;
use argunix_control::{BuilderInfo, Request, Response};
use argunix_store::BuilderStore;
use argunix_web::{AppState, ConfigSnapshot};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// Everything the control server needs to handle requests. Lives as
/// long as the daemon and is cloned into per-connection tasks.
#[derive(Clone)]
pub struct ControlContext {
    pub socket_path: PathBuf,
    pub app_state: AppState,
    pub store: argunix_store::SqlxStore,
    pub log_dir: PathBuf,
    pub gc_root_dir: PathBuf,
    pub config_path: PathBuf,
    pub skip_secret_check: bool,
    /// Runtime view of currently-connected builders. Empty when
    /// `builder_enrollment` isn't configured. Held as Arc so the
    /// BuilderServer task can write into the same registry.
    pub builder_registry: Arc<BuilderRegistry>,
    /// Path to local `nix-store` (gc-root registration post-pull).
    pub nix_store_bin: PathBuf,
    /// Path to local `nix` binary (drives `nix copy --from/--to`
    /// for closure transfer).
    pub nix_bin: PathBuf,
    /// Wall-clock cap for a single test-dispatched build.
    pub build_timeout: std::time::Duration,
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
    // Restrict the socket to its owner (the service user) + root. The
    // NixOS module runs under UMask=0027 (→ 0750, group-accessible), and
    // a non-module deployment with a looser umask could expose a group-
    // or world-writable control socket to an unprivileged local user who
    // could then `Reload`/`TestDispatchDrv`. 0700 makes filesystem perms
    // the access control. See bugs.md SEC-9.
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    if let Err(e) =
        std::fs::set_permissions(&ctx.socket_path, std::fs::Permissions::from_mode(0o700))
    {
        tracing::warn!(error = %e, "could not tighten control socket permissions to 0700");
    }
    // The socket's owner uid is our own; only that uid (or root) may
    // connect. Defense-in-depth on top of the 0700 mode via SO_PEERCRED.
    let allowed_uid: Option<u32> = std::fs::metadata(&ctx.socket_path).map(|m| m.uid()).ok();
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
        // Verify the peer's uid. Filesystem perms already gate this, but
        // a SO_PEERCRED check is cheap and catches a mis-permissioned
        // socket. See bugs.md SEC-9.
        if let Some(owner) = allowed_uid {
            match stream.peer_cred() {
                Ok(cred) if cred.uid() == owner || cred.uid() == 0 => {}
                Ok(cred) => {
                    tracing::warn!(
                        peer_uid = cred.uid(),
                        "rejecting control connection from unauthorized uid",
                    );
                    continue;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not read control peer credentials; rejecting");
                    continue;
                }
            }
        }
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
    // Bound the request: a client streaming one endless line would grow
    // `line` without limit and OOM the daemon. Requests are small JSON
    // objects, so cap the whole read at 64 KiB. See bugs.md COR-12.
    const MAX_REQUEST_BYTES: u64 = 64 * 1024;
    let mut reader = BufReader::new(read_half.take(MAX_REQUEST_BYTES));
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
            // The caller-supplied path is honoured because the NixOS
            // reload flow needs it: after `nixos-rebuild switch` the new
            // unit's `ExecReload` passes the new generation's
            // `/nix/store` config path to the daemon still running the
            // old one. This is safe only because the control socket is
            // locked to its owner uid + root (0700 + SO_PEERCRED, see the
            // listener) — an untrusted local user cannot reach this path
            // to point the daemon at a malicious config. See bugs.md SEC-9.
            let path = config_path.unwrap_or_else(|| ctx.config_path.clone());
            match handle_reload(path, ctx).await {
                Ok(details) => Response::ok_with(details),
                Err(e) => Response::error(format!("{e:#}")),
            }
        }
        Request::Status => Response::ok_with(handle_status(ctx).await),
        Request::BuildersList => match handle_builders_list(ctx).await {
            Ok(v) => Response::ok_with(v),
            Err(e) => Response::error(format!("{e:#}")),
        },
        Request::BuildersRevoke { name } => match handle_builders_revoke(&name, ctx).await {
            Ok(v) => Response::ok_with(v),
            Err(e) => Response::error(format!("{e:#}")),
        },
        Request::BuildersRemove { name } => match handle_builders_remove(&name, ctx).await {
            Ok(v) => Response::ok_with(v),
            Err(e) => Response::error(format!("{e:#}")),
        },
        Request::BuildersRename { old, new } => {
            match handle_builders_rename(&old, &new, ctx).await {
                Ok(v) => Response::ok_with(v),
                Err(e) => Response::error(format!("{e:#}")),
            }
        }
        Request::TestDispatchDrv { drv_path, builder } => {
            match handle_test_dispatch_drv(&drv_path, &builder, ctx).await {
                Ok(v) => Response::ok_with(v),
                Err(e) => Response::error(format!("{e:#}")),
            }
        }
    }
}

/// VM test driver: dispatch one drv to a named builder via the
/// dynamic pool. Bypasses the worker's eval pipeline so a NixOS test
/// can exercise the transport without standing up a fake forge.
async fn handle_test_dispatch_drv(
    drv_path: &str,
    builder: &str,
    ctx: &ControlContext,
) -> anyhow::Result<serde_json::Value> {
    use argunix_domain::BuilderName;
    let name = BuilderName::new(builder)
        .map_err(|e| anyhow::anyhow!("invalid builder name `{builder}`: {e}"))?;
    if ctx.builder_registry.snapshot(&name).is_none() {
        anyhow::bail!("builder `{builder}` is not currently connected");
    }
    // Synthetic build_id: epoch nanos so two concurrent test dispatches
    // (unlikely but cheap to defend) don't collide.
    let build_id: i64 = chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or_else(|| chrono::Utc::now().timestamp_micros());
    let test_dir = ctx.gc_root_dir.join("test-dispatch");
    let _ = tokio::fs::create_dir_all(&test_dir).await;
    let log_path = test_dir.join(format!("{build_id}.log.zst"));
    let gc_root = test_dir.join(format!("{build_id}"));

    let spec = crate::worker::PoolDispatchSpec {
        registry: ctx.builder_registry.clone(),
        builder_name: &name,
        build_id,
        drv_path,
        gc_root: &gc_root,
        log_path: &log_path,
        log_limit: argunix_build::LogCaptureLimit::default(),
        build_timeout: ctx.build_timeout,
        nix_store_bin: &ctx.nix_store_bin,
        nix_bin: &ctx.nix_bin,
        live_logs: None,
        // Single-shot operator verb; runs outside the worker's
        // build-concurrency budget.
        transfer_slot: None,
    };
    match crate::worker::dispatch_pool_build(spec, None).await? {
        crate::worker::PoolDispatchResult::Outcome {
            outcome: o,
            phase_metrics,
        } => Ok(serde_json::json!({
            "status": match o.status {
                argunix_build::BuildStatus::Success => "success",
                argunix_build::BuildStatus::Failure => "failure",
            },
            "output_paths": o.output_paths,
            "log_path": o.log_path.to_string_lossy(),
            "log_truncated": o.log_truncated,
            "phase_metrics": {
                "push_bytes": phase_metrics.push_bytes,
                "push_ms": phase_metrics.push_ms,
                "build_ms": phase_metrics.build_ms,
                "pull_bytes": phase_metrics.pull_bytes,
                "pull_ms": phase_metrics.pull_ms,
            },
        })),
        crate::worker::PoolDispatchResult::TransportFailure { phase_metrics } => {
            Ok(serde_json::json!({
                "status": "transport_failure",
                "phase_metrics": {
                    "push_bytes": phase_metrics.push_bytes,
                    "push_ms": phase_metrics.push_ms,
                    "build_ms": phase_metrics.build_ms,
                    "pull_bytes": phase_metrics.pull_bytes,
                    "pull_ms": phase_metrics.pull_ms,
                },
            }))
        }
        crate::worker::PoolDispatchResult::Cancelled => {
            anyhow::bail!("test dispatch cancelled (unexpected — no cancel token)");
        }
    }
}

async fn handle_reload(
    config_path: PathBuf,
    ctx: &ControlContext,
) -> anyhow::Result<serde_json::Value> {
    tracing::info!(path = %config_path.display(), "reload requested");

    // Validate first, then atomically swap. Any error before the
    // swap leaves the running daemon untouched.
    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Reloading]);

    let new_config = argunix_config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;
    if !ctx.skip_secret_check {
        new_config
            .validate_secrets_exist()
            .context("validating secret files")?;
    }
    let new_providers = argunix_web::build_providers(&new_config)
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
    argunix_web::ensure_webhooks(&new_snap.config, &new_snap.providers, &ctx.store).await;

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

async fn handle_builders_list(ctx: &ControlContext) -> anyhow::Result<serde_json::Value> {
    let rows = ctx
        .store
        .list_all()
        .await
        .context("listing builders from sqlite")?;
    let mut out: Vec<BuilderInfo> = Vec::with_capacity(rows.len());
    for row in rows {
        let snap = ctx.builder_registry.snapshot(&row.name);
        let connected = snap.is_some() && row.revoked_at.is_none();
        let in_flight = snap.as_ref().map(|s| s.in_flight).unwrap_or(0);
        out.push(BuilderInfo {
            id: row.id.get(),
            name: row.name.as_str().to_string(),
            systems: row.capabilities.systems,
            features: row.capabilities.features,
            max_jobs: row.capabilities.max_jobs,
            nix_version: row.capabilities.nix_version,
            enrolled_at: row.enrolled_at.to_rfc3339(),
            last_seen: row.last_seen.to_rfc3339(),
            revoked_at: row.revoked_at.map(|t| t.to_rfc3339()),
            connected,
            in_flight,
        });
    }
    Ok(serde_json::to_value(&out)?)
}

async fn handle_builders_revoke(
    name: &str,
    ctx: &ControlContext,
) -> anyhow::Result<serde_json::Value> {
    let now = chrono::Utc::now();
    let revoked = ctx
        .store
        .revoke(name, now)
        .await
        .context("revoking builder in sqlite")?;
    if !revoked {
        anyhow::bail!("no such builder: {name}");
    }
    // If there's a live SSH session, kick it now so the builder
    // doesn't keep heartbeating after revocation. The Drop on the
    // ConnectionHandler will remove the registry row when the SSH
    // session tears down.
    let kicked = match argunix_domain::BuilderName::new(name) {
        Ok(builder_name) => match ctx.builder_registry.session(&builder_name) {
            Some(session) => {
                let kick = argunix_builders::ControlMessage::Kick {
                    reason: "revoked by operator".into(),
                };
                let bytes: bytes::Bytes = kick.encode_line().into();
                let _ = session.handle.data(session.control_channel, bytes).await;
                let _ = session
                    .handle
                    .disconnect(
                        russh::Disconnect::ByApplication,
                        "revoked by operator".into(),
                        "en".into(),
                    )
                    .await;
                true
            }
            None => false,
        },
        Err(_) => false,
    };
    tracing::info!(
        builder = %name,
        kicked,
        "builder revoked",
    );
    Ok(serde_json::json!({
        "name": name,
        "kicked": kicked,
    }))
}

async fn handle_builders_remove(
    name: &str,
    ctx: &ControlContext,
) -> anyhow::Result<serde_json::Value> {
    // Kick a live session first (same as revoke), then delete the row so
    // the name is free for a different key to enroll under.
    let kicked = match argunix_domain::BuilderName::new(name) {
        Ok(builder_name) => match ctx.builder_registry.session(&builder_name) {
            Some(session) => {
                let kick = argunix_builders::ControlMessage::Kick {
                    reason: "removed by operator".into(),
                };
                let bytes: bytes::Bytes = kick.encode_line().into();
                let _ = session.handle.data(session.control_channel, bytes).await;
                let _ = session
                    .handle
                    .disconnect(
                        russh::Disconnect::ByApplication,
                        "removed by operator".into(),
                        "en".into(),
                    )
                    .await;
                true
            }
            None => false,
        },
        Err(_) => false,
    };
    let removed = ctx
        .store
        .remove(name)
        .await
        .context("removing builder from sqlite")?;
    if !removed {
        anyhow::bail!("no such builder: {name}");
    }
    tracing::info!(builder = %name, kicked, "builder removed");
    Ok(serde_json::json!({
        "name": name,
        "kicked": kicked,
    }))
}

async fn handle_builders_rename(
    old: &str,
    new: &str,
    ctx: &ControlContext,
) -> anyhow::Result<serde_json::Value> {
    // Validate the target name shape eagerly so a typo doesn't land
    // an unconstrained value in sqlite (BuilderName parsing happens
    // at row read time, but we want to catch this at write time).
    let _ = argunix_domain::BuilderName::new(new)
        .with_context(|| format!("invalid new builder name `{new}`"))?;
    let renamed = ctx
        .store
        .rename(old, new)
        .await
        .context("renaming builder in sqlite")?;
    if !renamed {
        anyhow::bail!("rename failed: `{old}` doesn't exist or `{new}` already does");
    }
    tracing::info!(old, new, "builder renamed");
    // Live registry entries keyed on old name aren't auto-renamed —
    // the next reconnect's hello will pick up the new sqlite row's
    // name. Operators can `argunixctl builders` after rename to see
    // the row under its new name.
    Ok(serde_json::json!({ "old": old, "new": new }))
}
