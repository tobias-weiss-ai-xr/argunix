use crate::state::AppState;
use argunix_domain::{EvalId, ForgeKind, Slug};
use argunix_forge::{
    CheckPost, CheckState, NormalizedEvent, Provider, PullRequestEvent, PushEvent,
};
use argunix_store::{EvalStore, RepoStore};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use bytes::Bytes;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum WebhookError {
    #[error("unknown forge kind in URL path: `{0}`")]
    UnknownForgeKind(String),
    #[error("malformed JSON body: {0}")]
    BadJson(#[source] serde_json::Error),
    #[error("payload missing `repository.full_name`; ignored as non-repo event")]
    NoRepository,
    #[error("invalid slug `{slug}` in payload: {source}")]
    InvalidSlug {
        slug: String,
        #[source]
        source: argunix_domain::SlugError,
    },
    #[error("repo `{0}` is not configured in argunix")]
    RepoNotConfigured(String),
    #[error("no provider built for forge `{0}` (this is a daemon bug)")]
    NoProvider(String),
    #[error(
        "no webhook secret stored for `{forge}/{slug}` — auto-install hasn't \
         completed for this repo yet"
    )]
    WebhookNotProvisioned { forge: String, slug: String },
    #[error(transparent)]
    Forge(#[from] argunix_forge::ForgeError),
    #[error(transparent)]
    Store(#[from] argunix_store::StoreError),
}

impl WebhookError {
    pub fn status_code(&self) -> StatusCode {
        match self {
            WebhookError::UnknownForgeKind(_) => StatusCode::NOT_FOUND,
            WebhookError::BadJson(_) => StatusCode::BAD_REQUEST,
            WebhookError::NoRepository => StatusCode::ACCEPTED,
            WebhookError::InvalidSlug { .. } => StatusCode::BAD_REQUEST,
            WebhookError::RepoNotConfigured(_) => StatusCode::NOT_FOUND,
            WebhookError::NoProvider(_) => StatusCode::INTERNAL_SERVER_ERROR,
            WebhookError::WebhookNotProvisioned { .. } => StatusCode::SERVICE_UNAVAILABLE,
            WebhookError::Forge(argunix_forge::ForgeError::BadSignature) => {
                StatusCode::UNAUTHORIZED
            }
            WebhookError::Forge(argunix_forge::ForgeError::MissingHeader(_))
            | WebhookError::Forge(argunix_forge::ForgeError::InvalidHeader { .. })
            | WebhookError::Forge(argunix_forge::ForgeError::BadPayload(_))
            | WebhookError::Forge(argunix_forge::ForgeError::InvalidSlug(_, _))
            | WebhookError::Forge(argunix_forge::ForgeError::InvalidSha(_, _)) => {
                StatusCode::BAD_REQUEST
            }
            WebhookError::Forge(_) => StatusCode::INTERNAL_SERVER_ERROR,
            WebhookError::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for WebhookError {
    fn into_response(self) -> axum::response::Response {
        // Logging happens in `handle()` where forge+slug context is
        // available; this impl just turns the error into a response.
        let status = self.status_code();
        let body = self.to_string();
        (status, body).into_response()
    }
}

/// Context captured during `handle()` so rejection log lines can name
/// which forge / repo the failed event was for. Populated as soon as
/// each piece of information is identified; remaining fields stay
/// `None` if we never got that far.
#[derive(Default)]
struct WebhookCtx {
    /// URL path segment — `github`, `gitlab`, `forgejo`. Always set.
    forge_kind: String,
    /// Slug parsed from the payload preview, if we got past the parse step.
    slug: Option<Slug>,
    /// Configured forge name from `forges.<key>`, if we got as far as
    /// matching the repo. Distinct from `forge_kind`: an operator may
    /// have multiple GitHub forges configured under different keys
    /// (e.g. `github-myorg`, `github-other`).
    forge_name: Option<String>,
}

/// `POST /webhook/{forge_kind}` where `forge_kind` is `github`,
/// `gitlab`, or `forgejo`.
pub async fn handle(
    Path(forge_kind): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let mut ctx = WebhookCtx {
        forge_kind: forge_kind.clone(),
        ..Default::default()
    };
    match handle_inner(forge_kind, state, headers, body, &mut ctx).await {
        Ok(status) => status.into_response(),
        Err(err) => log_and_respond(&ctx, err),
    }
}

fn log_and_respond(ctx: &WebhookCtx, err: WebhookError) -> axum::response::Response {
    let status = err.status_code();
    let slug_str = ctx.slug.as_ref().map(|s| s.as_str().to_string());
    let body = err.to_string();
    if status.is_server_error() {
        tracing::error!(
            forge_kind = %ctx.forge_kind,
            forge_name = ?ctx.forge_name,
            slug = ?slug_str,
            error = %body,
            "webhook error",
        );
    } else if status.is_client_error() {
        tracing::warn!(
            forge_kind = %ctx.forge_kind,
            forge_name = ?ctx.forge_name,
            slug = ?slug_str,
            status = status.as_u16(),
            error = %body,
            "webhook rejected",
        );
    }
    (status, body).into_response()
}

async fn handle_inner(
    forge_kind: String,
    state: AppState,
    headers: HeaderMap,
    body: Bytes,
    ctx: &mut WebhookCtx,
) -> Result<StatusCode, WebhookError> {
    let expected_kind = match forge_kind.as_str() {
        "github" => ForgeKind::Github,
        "gitlab" => ForgeKind::Gitlab,
        "forgejo" => ForgeKind::Forgejo,
        _ => return Err(WebhookError::UnknownForgeKind(forge_kind)),
    };

    // Snapshot the swappable bundle once at the top — every subsequent
    // read uses this exact `(config, providers)` pair, so a concurrent
    // `argunixctl reload` can't pull the rug out mid-handler.
    let snap = state.current.load_full();

    // Untrusted preview parse: just enough to find which configured repo
    // and forge this is for. HMAC-verified parse follows.
    let preview: PayloadPreview = serde_json::from_slice(&body).map_err(WebhookError::BadJson)?;
    // GitLab includes a top-level `repository` object that lacks
    // `full_name` — present from before GitLab unified namespaces and
    // kept around for compatibility. We accept either source.
    let repo_full_name = preview
        .repository
        .and_then(|r| r.full_name)
        .or(preview.project.and_then(|p| p.path_with_namespace))
        .ok_or(WebhookError::NoRepository)?;

    let slug = Slug::new(repo_full_name.clone()).map_err(|e| WebhookError::InvalidSlug {
        slug: repo_full_name.clone(),
        source: e,
    })?;
    ctx.slug = Some(slug.clone());
    // The same slug can appear under multiple forges (e.g. you might
    // have `tfc/pprintpp` on both GitHub and a self-hosted Forgejo —
    // argunix keys repos by `(forge_name, slug)`). Filter by both the
    // slug AND the URL's forge kind so the right repo is picked.
    let repo_cfg = snap
        .config
        .repos
        .iter()
        .find(|r| {
            r.slug == slug
                && snap
                    .config
                    .forges
                    .get(&r.forge)
                    .map(|f| f.kind == expected_kind)
                    .unwrap_or(false)
        })
        .ok_or_else(|| WebhookError::RepoNotConfigured(repo_full_name.clone()))?;
    ctx.forge_name = Some(repo_cfg.forge.clone());

    let provider = snap
        .providers
        .get(&repo_cfg.forge)
        .ok_or_else(|| WebhookError::NoProvider(repo_cfg.forge.clone()))?
        .clone();

    // Webhook secret is argunix-managed: generated at startup, stored
    // in sqlite, pushed to the forge by the auto-install pass. If
    // we don't have it yet the auto-install hasn't completed (or
    // failed) — reject this event with 404 since we can't verify it.
    let secret = state
        .store
        .get_webhook_secret(&repo_cfg.forge, &slug)
        .await?
        .ok_or_else(|| WebhookError::WebhookNotProvisioned {
            forge: repo_cfg.forge.clone(),
            slug: slug.as_str().to_string(),
        })?;

    let header_pairs = headers_to_pairs(&headers);

    provider
        .verify_signature(&header_pairs, &body, &secret)
        .await?;

    let Some(event) = provider.parse_event(&header_pairs, &body).await? else {
        // dropped events (ping, unknown action) → 202 with no DB write
        return Ok(StatusCode::ACCEPTED);
    };

    match crate::policy::evaluate(&provider, repo_cfg, &event, &repo_cfg.forge, &state.pauses).await
    {
        crate::policy::Decision::Build => {
            persist(&state, &repo_cfg.forge, &provider, event).await?;
        }
        decision => {
            tracing::info!(
                slug = %slug,
                decision = ?decision,
                "webhook event dropped by policy",
            );
        }
    }
    Ok(StatusCode::ACCEPTED)
}

async fn persist(
    state: &AppState,
    forge_name: &str,
    provider: &Arc<dyn Provider>,
    event: NormalizedEvent,
) -> Result<(), WebhookError> {
    let (slug, git_ref, sha, trigger) = match &event {
        NormalizedEvent::Push(PushEvent {
            slug, git_ref, sha, ..
        }) => (
            slug.clone(),
            git_ref.clone(),
            sha.clone(),
            "push".to_string(),
        ),
        NormalizedEvent::PullRequest(PullRequestEvent {
            slug,
            pr_number,
            head_sha,
            head_ref,
            ..
        }) => (
            slug.clone(),
            format!("refs/pull/{pr_number}/head:{head_ref}"),
            head_sha.clone(),
            "pull_request".to_string(),
        ),
    };

    let repo_id = state.store.upsert(forge_name, &slug).await?;

    // Q99: drop duplicate `(repo_id, sha)` events within the configured
    // window. GitHub sends both a `push` and a `pull_request.synchronize`
    // for the same SHA on every PR push; without this, every PR would
    // produce two parallel evaluations and two sets of forge checks.
    if !state.coalesce.admit(repo_id, sha.clone()) {
        tracing::info!(
            repo_id = repo_id.get(),
            slug = %slug,
            sha = %sha,
            "dropping duplicate webhook event within coalesce window",
        );
        return Ok(());
    }

    // Q39: cancel any in-flight evaluations for the same branch with a
    // different SHA. We only fire here if the new SHA is different
    // (matching SHAs were already filtered by the coalesce check, but
    // a new push to the same branch with a different SHA arrives here).
    let key = crate::cancel::branch_key(&git_ref);
    let active = state.store.list_active_by_branch_key(repo_id, key).await?;
    for prev in active.iter().filter(|e| e.sha != sha) {
        tracing::info!(
            repo_id = repo_id.get(),
            branch = key,
            superseded_eval = prev.id.get(),
            superseded_sha = %prev.sha,
            new_sha = %sha,
            "cancelling in-flight evaluation superseded by new push (Q39)",
        );
        // DB-level cancel: covers the case where the worker hasn't
        // picked up this eval yet (cancel arrives before the mpsc
        // dispatch reaches it).
        let _ = <argunix_store::SqlxStore as EvalStore>::finish(
            &state.store,
            prev.id,
            argunix_domain::EvalStatus::Cancelled,
            chrono::Utc::now(),
        )
        .await;
        // In-memory cancel: signal any worker currently mid-evaluation
        // to bail out / kill its `nix-store --realise`.
        state.cancellations.cancel(prev.id);
    }

    let eval_id = <argunix_store::SqlxStore as EvalStore>::create(
        &state.store,
        argunix_store::NewEvaluation {
            repo_id,
            trigger,
            git_ref: git_ref.clone(),
            sha: sha.clone(),
        },
    )
    .await?;
    tracing::info!(
        repo_id = repo_id.get(),
        eval_id = eval_id.get(),
        slug = %slug,
        sha = %sha,
        "evaluation queued",
    );

    // Q51: post a `argunix: evaluation` pending check immediately so the
    // PR shows argunix received the event. We spawn this so a slow forge
    // doesn't slow the webhook ack — the worker still proceeds even if
    // this fails.
    let post = CheckPost {
        slug: slug.clone(),
        sha: sha.clone(),
        context: "argunix: evaluation".to_string(),
        state: CheckState::Pending,
        description: Some("evaluating…".to_string()),
        target_url: Some(eval_target_url(
            &state.current.load().config.external_url,
            forge_name,
            &slug,
            eval_id,
        )),
    };
    spawn_post_check(
        provider.clone(),
        post,
        forge_name.to_string(),
        state.pauses.clone(),
    );

    // Best-effort: hand off to the worker. If the channel has been closed
    // (daemon is shutting down), the eval is still in the DB and a future
    // restart's startup scan will pick it up.
    let _ = state.work_dispatcher.send(eval_id);
    Ok(())
}

pub fn eval_target_url(external_url: &str, forge: &str, slug: &Slug, eval_id: EvalId) -> String {
    let base = external_url.trim_end_matches('/');
    format!(
        "{base}/r/{forge}/{slug}/eval/{eval}",
        slug = slug.as_str(),
        eval = eval_id.get()
    )
}

pub fn job_target_url(
    external_url: &str,
    forge: &str,
    slug: &Slug,
    eval_id: EvalId,
    attr_path: &str,
) -> String {
    let base = external_url.trim_end_matches('/');
    format!(
        "{base}/r/{forge}/{slug}/eval/{eval}/job/{attr}",
        slug = slug.as_str(),
        eval = eval_id.get(),
        attr = attr_path,
    )
}

fn spawn_post_check(
    provider: Arc<dyn Provider>,
    post: CheckPost,
    forge_name: String,
    pauses: Arc<crate::pause::PauseRegistry>,
) {
    if pauses.is_paused(&forge_name) {
        tracing::info!(
            forge = %forge_name,
            "skipping forge post_check: forge paused (Q82)",
        );
        return;
    }
    tokio::spawn(async move {
        match provider.post_check(post).await {
            Ok(_) => pauses.mark_healthy(&forge_name),
            Err(argunix_forge::ForgeError::Unauthorised) => {
                pauses.pause(&forge_name, "401 from post_check");
            }
            Err(e) => {
                tracing::warn!(forge = %forge_name, error = %e, "forge post_check failed");
            }
        }
    });
}

fn headers_to_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|s| (k.as_str().to_string(), s.to_string()))
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct PayloadPreview {
    /// GitHub / Forgejo: `repository.full_name`.
    repository: Option<RepositoryPreview>,
    /// GitLab: `project.path_with_namespace`.
    project: Option<ProjectPreview>,
}

#[derive(Debug, Deserialize)]
struct RepositoryPreview {
    /// GitHub & Forgejo include this; GitLab doesn't (the field is
    /// absent from its `repository` sub-object — GitLab carries the
    /// canonical project identity in `project.path_with_namespace`).
    full_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProjectPreview {
    path_with_namespace: Option<String>,
}

#[cfg(test)]
mod preview_tests {
    //! Regression tests for the untrusted-payload preview parser. The
    //! only purpose of this parse is to extract the slug — anything
    //! else (signature, full event shape) is verified by the provider
    //! after this point.
    use super::*;

    fn extract(body: &[u8]) -> Option<String> {
        let p: PayloadPreview = serde_json::from_slice(body).ok()?;
        p.repository
            .and_then(|r| r.full_name)
            .or(p.project.and_then(|p| p.path_with_namespace))
    }

    #[test]
    fn github_payload_uses_repository_full_name() {
        let body = serde_json::json!({
            "ref": "refs/heads/main",
            "repository": { "full_name": "alice/repo" }
        })
        .to_string();
        assert_eq!(extract(body.as_bytes()), Some("alice/repo".to_string()));
    }

    #[test]
    fn gitlab_payload_falls_through_to_project_path() {
        // GitLab's actual webhook shape has a top-level `repository`
        // object with NO `full_name`, plus the canonical identity at
        // `project.path_with_namespace`.
        let body = serde_json::json!({
            "ref": "refs/heads/main",
            "repository": {
                "name": "pprintpp",
                "url": "git@gitlab.com:jonge/pprintpp.git",
                "git_http_url": "https://gitlab.com/jonge/pprintpp.git",
                "visibility_level": 20
            },
            "project": {
                "id": 1234,
                "path_with_namespace": "jonge/pprintpp"
            }
        })
        .to_string();
        assert_eq!(extract(body.as_bytes()), Some("jonge/pprintpp".to_string()));
    }

    #[test]
    fn payload_without_either_returns_none() {
        let body = b"{}";
        assert_eq!(extract(body), None);
    }
}
