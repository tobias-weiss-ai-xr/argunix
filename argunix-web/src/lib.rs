//! HTTP surface: webhook ingestion and (later) the read-only UI.
//!
//! v1 (M5b) ships:
//! - `POST /webhook/github` — verify HMAC, parse event, persist evaluation,
//!   return 202.
//! - `GET /healthz` — always 200, returns "ok".
//!
//! Read-only UI, JSON content negotiation, badges, and `/metrics` come
//! later (M6).

mod auto_install;
mod cancel;
mod coalesce;
mod live_log;
mod pause;
mod policy;
mod state;
mod ui;
mod webhook;

pub use auto_install::ensure_all as ensure_webhooks;
pub use cancel::{CancelRegistry, CancelToken, branch_key};
pub use coalesce::CoalescePool;
pub use live_log::{LiveLog, LiveLogRegistry};
pub use pause::PauseRegistry;

pub use policy::{Decision as PolicyDecision, evaluate as evaluate_policy};
pub use state::{AppState, AppStateInner, BuildProvidersError, ConfigSnapshot, build_providers};
pub use webhook::{eval_target_url, job_target_url};

use axum::Router;
use axum::routing::{get, post};
use std::sync::Arc;
use tower_http::services::ServeDir;

pub fn router(state: AppState) -> Router {
    // ServeDir takes the path at construction time, not per-request, so we
    // resolve `static_dir` from the initial config snapshot. Reload swaps
    // forge providers but not asset paths.
    let static_dir = state.current.load_full().config.web.static_dir.clone();

    Router::new()
        .route("/", get(ui::status))
        .route("/_/status", get(ui::status_fragment))
        .route("/repos", get(ui::index))
        .route("/healthz", get(healthz))
        .route("/r/{forge}/{*tail}", get(ui::dispatch_repo))
        .route("/api/builders/{name}/stats", get(ui::builder_stats))
        .route("/api/jobs/{job_id}/log/stream", get(ui::job_log_stream))
        .route("/webhook/{forge_kind}", post(webhook::handle))
        .nest_service("/static", ServeDir::new(static_dir))
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

/// Convenience for callers that already have an `AppStateInner` and want
/// to wrap it in the `Arc` the router expects.
pub fn router_from_inner(inner: AppStateInner) -> Router {
    router(Arc::new(inner))
}
