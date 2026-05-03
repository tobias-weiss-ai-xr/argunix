//! HTTP surface: webhook ingestion and (later) the read-only UI.
//!
//! v1 (M5b) ships:
//! - `POST /webhook/github` — verify HMAC, parse event, persist evaluation,
//!   return 202.
//! - `GET /healthz` — always 200, returns "ok".
//!
//! Read-only UI, JSON content negotiation, badges, and `/metrics` come
//! later (M6).

mod coalesce;
mod pause;
mod policy;
mod state;
mod ui;
mod webhook;

pub use coalesce::CoalescePool;
pub use pause::PauseRegistry;

pub use policy::{Decision as PolicyDecision, evaluate as evaluate_policy};
pub use state::{AppState, AppStateInner, BuildProvidersError, build_providers};
pub use webhook::{eval_target_url, job_target_url};

use axum::Router;
use axum::routing::{get, post};
use std::sync::Arc;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(ui::index))
        .route("/healthz", get(healthz))
        .route("/r/{forge}/{*tail}", get(ui::dispatch_repo))
        .route("/webhook/{forge_kind}", post(webhook::handle))
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
