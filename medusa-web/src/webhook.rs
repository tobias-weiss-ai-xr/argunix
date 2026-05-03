use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use bytes::Bytes;
use medusa_domain::{EvalId, ForgeKind, Slug};
use medusa_forge::{CheckPost, CheckState, NormalizedEvent, Provider, PullRequestEvent, PushEvent};
use medusa_store::{EvalStore, RepoStore};
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
        source: medusa_domain::SlugError,
    },
    #[error("repo `{0}` is not configured in medusa")]
    RepoNotConfigured(String),
    #[error(
        "repo `{slug}` is configured under forge kind `{configured}` but webhook hit /{requested}"
    )]
    KindMismatch {
        slug: String,
        configured: ForgeKind,
        requested: String,
    },
    #[error("no provider built for forge `{0}` (this is a daemon bug)")]
    NoProvider(String),
    #[error("reading webhook secret: {0}")]
    SecretRead(#[source] std::io::Error),
    #[error(transparent)]
    Forge(#[from] medusa_forge::ForgeError),
    #[error(transparent)]
    Store(#[from] medusa_store::StoreError),
}

impl IntoResponse for WebhookError {
    fn into_response(self) -> axum::response::Response {
        let status = match &self {
            WebhookError::UnknownForgeKind(_) => StatusCode::NOT_FOUND,
            WebhookError::BadJson(_) => StatusCode::BAD_REQUEST,
            WebhookError::NoRepository => StatusCode::ACCEPTED,
            WebhookError::InvalidSlug { .. } => StatusCode::BAD_REQUEST,
            WebhookError::RepoNotConfigured(_) => StatusCode::NOT_FOUND,
            WebhookError::KindMismatch { .. } => StatusCode::BAD_REQUEST,
            WebhookError::NoProvider(_) => StatusCode::INTERNAL_SERVER_ERROR,
            WebhookError::SecretRead(_) => StatusCode::INTERNAL_SERVER_ERROR,
            WebhookError::Forge(medusa_forge::ForgeError::BadSignature) => StatusCode::UNAUTHORIZED,
            WebhookError::Forge(medusa_forge::ForgeError::MissingHeader(_))
            | WebhookError::Forge(medusa_forge::ForgeError::InvalidHeader { .. })
            | WebhookError::Forge(medusa_forge::ForgeError::BadPayload(_))
            | WebhookError::Forge(medusa_forge::ForgeError::InvalidSlug(_, _))
            | WebhookError::Forge(medusa_forge::ForgeError::InvalidSha(_, _)) => {
                StatusCode::BAD_REQUEST
            }
            WebhookError::Forge(_) => StatusCode::INTERNAL_SERVER_ERROR,
            WebhookError::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = self.to_string();
        // Log at the level the response code suggests so noisy bots don't
        // pollute the logs at warn+.
        if status.is_server_error() {
            tracing::error!(error = %body, "webhook error");
        } else if status.is_client_error() {
            tracing::warn!(status = status.as_u16(), error = %body, "webhook rejected");
        }
        (status, body).into_response()
    }
}

/// `POST /webhook/{forge_kind}`. Path segment must currently be `github`;
/// `gitlab` and `forgejo` land in M7.
pub async fn handle(
    Path(forge_kind): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, WebhookError> {
    if forge_kind != "github" {
        return Err(WebhookError::UnknownForgeKind(forge_kind));
    }

    // Untrusted preview parse: just enough to find which configured repo
    // and forge this is for. HMAC-verified parse follows.
    let preview: PayloadPreview = serde_json::from_slice(&body).map_err(WebhookError::BadJson)?;
    let Some(repo_full_name) = preview.repository.map(|r| r.full_name) else {
        return Err(WebhookError::NoRepository);
    };

    let slug = Slug::new(repo_full_name.clone()).map_err(|e| WebhookError::InvalidSlug {
        slug: repo_full_name.clone(),
        source: e,
    })?;
    let repo_cfg = state
        .config
        .repos
        .iter()
        .find(|r| r.slug == slug)
        .ok_or_else(|| WebhookError::RepoNotConfigured(repo_full_name.clone()))?;

    let forge_cfg = state.config.forges.get(&repo_cfg.forge).ok_or_else(|| {
        // We checked this at config validation time, but be defensive.
        WebhookError::NoProvider(repo_cfg.forge.clone())
    })?;
    if forge_cfg.kind != ForgeKind::Github {
        return Err(WebhookError::KindMismatch {
            slug: slug.as_str().to_string(),
            configured: forge_cfg.kind,
            requested: forge_kind.clone(),
        });
    }

    let provider = state
        .providers
        .get(&repo_cfg.forge)
        .ok_or_else(|| WebhookError::NoProvider(repo_cfg.forge.clone()))?
        .clone();

    let secret = tokio::fs::read(forge_cfg.webhook_secret_path.path())
        .await
        .map_err(WebhookError::SecretRead)?;

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
    let active = state
        .store
        .list_active_by_branch_key(repo_id, key)
        .await?;
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
        let _ = <medusa_store::SqlxStore as EvalStore>::finish(
            &state.store,
            prev.id,
            medusa_domain::EvalStatus::Cancelled,
            chrono::Utc::now(),
        )
        .await;
        // In-memory cancel: signal any worker currently mid-evaluation
        // to bail out / kill its `nix-store --realise`.
        state.cancellations.cancel(prev.id);
    }

    let eval_id = <medusa_store::SqlxStore as EvalStore>::create(
        &state.store,
        medusa_store::NewEvaluation {
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

    // Q51: post a `medusa: evaluation` pending check immediately so the
    // PR shows medusa received the event. We spawn this so a slow forge
    // doesn't slow the webhook ack — the worker still proceeds even if
    // this fails.
    let post = CheckPost {
        slug: slug.clone(),
        sha: sha.clone(),
        context: "medusa: evaluation".to_string(),
        state: CheckState::Pending,
        description: Some("evaluating…".to_string()),
        target_url: Some(eval_target_url(
            &state.config.external_url,
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
            Err(medusa_forge::ForgeError::Unauthorised) => {
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
    repository: Option<RepositoryPreview>,
}

#[derive(Debug, Deserialize)]
struct RepositoryPreview {
    full_name: String,
}
