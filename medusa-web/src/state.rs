use crate::coalesce::CoalescePool;
use crate::pause::PauseRegistry;
use medusa_config::{Config, ForgeAuth, ForgeConfig};
use medusa_domain::{EvalId, ForgeKind};
use medusa_forge::{ForgejoProvider, GithubProvider, GitlabProvider, Provider};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

/// Daemon-side state shared by every request handler. Wrapped in `Arc`
/// for `axum`'s `State` extractor (which requires `Clone`).
pub type AppState = Arc<AppStateInner>;

pub struct AppStateInner {
    pub config: Arc<Config>,
    pub providers: HashMap<String, Arc<dyn Provider>>,
    pub store: medusa_store::SqlxStore,
    /// Channel to the background worker. After the webhook handler
    /// persists an evaluation row, it sends the new id here so the
    /// worker can pick it up immediately rather than polling.
    pub work_dispatcher: UnboundedSender<EvalId>,
    /// Drops duplicate `(repo_id, sha)` events within a short window
    /// (Q99). Configured from `Schedule::webhook_coalesce_seconds`.
    pub coalesce: Arc<CoalescePool>,
    /// Tracks which forges are currently paused due to 401s (Q82).
    pub pauses: Arc<PauseRegistry>,
    /// Per-eval cancellation tokens for cancel-on-new-push (Q39).
    pub cancellations: Arc<crate::cancel::CancelRegistry>,
}

#[derive(Debug, thiserror::Error)]
pub enum BuildProvidersError {
    #[error("forge `{forge}` has invalid auth: {error}")]
    Auth {
        forge: String,
        #[source]
        error: medusa_config::ForgeAuthShapeError,
    },
    #[error("forge `{forge}` uses kind `{kind}` which is not yet supported in v1")]
    UnsupportedKind { forge: String, kind: ForgeKind },
    #[error("forge `{forge}` uses GitHub-App auth, which is not yet supported (PAT only for now)")]
    AppAuthUnsupported { forge: String },
    #[error("forge `{forge}` token file `{path}` is not readable: {source}")]
    TokenRead {
        forge: String,
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Build a provider per forge defined in `config`. Reads token files from
/// disk synchronously (this only runs at daemon startup).
pub async fn build_providers(
    config: &Config,
) -> Result<HashMap<String, Arc<dyn Provider>>, BuildProvidersError> {
    let mut providers = HashMap::new();
    for (name, forge_cfg) in &config.forges {
        let provider = build_one(name, forge_cfg, &config.external_url).await?;
        providers.insert(name.clone(), provider);
    }
    Ok(providers)
}

async fn build_one(
    name: &str,
    forge_cfg: &ForgeConfig,
    external_url: &str,
) -> Result<Arc<dyn Provider>, BuildProvidersError> {
    let token = read_token(name, forge_cfg).await?;
    match forge_cfg.kind {
        ForgeKind::Github => Ok(Arc::new(GithubProvider::new(
            forge_cfg.api_url.clone(),
            token,
            external_url.to_string(),
        )) as Arc<dyn Provider>),
        ForgeKind::Gitlab => Ok(Arc::new(GitlabProvider::new(
            forge_cfg.api_url.clone(),
            token,
            external_url.to_string(),
        )) as Arc<dyn Provider>),
        ForgeKind::Forgejo => Ok(Arc::new(ForgejoProvider::new(
            forge_cfg.api_url.clone(),
            token,
            external_url.to_string(),
        )) as Arc<dyn Provider>),
    }
}

/// Read the token file referenced by `forge_cfg.auth().token_path`.
/// Returns `BuildProvidersError::AppAuthUnsupported` for app-style
/// configs (M5c work) — those land later when we add Checks API.
async fn read_token(name: &str, forge_cfg: &ForgeConfig) -> Result<String, BuildProvidersError> {
    let auth = forge_cfg.auth().map_err(|e| BuildProvidersError::Auth {
        forge: name.to_string(),
        error: e,
    })?;
    match auth {
        ForgeAuth::Token { token_path } => {
            let raw = tokio::fs::read_to_string(token_path.path())
                .await
                .map_err(|e| BuildProvidersError::TokenRead {
                    forge: name.to_string(),
                    path: token_path.path().to_path_buf(),
                    source: e,
                })?;
            Ok(raw.trim().to_string())
        }
        ForgeAuth::App { .. } => Err(BuildProvidersError::AppAuthUnsupported {
            forge: name.to_string(),
        }),
    }
}
