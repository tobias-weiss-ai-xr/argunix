//! Forgejo (and Gitea-compatible) forge provider with token auth.
//!
//! Forgejo / Gitea expose a Gitea-shaped API; the webhook payload and
//! status-posting shape both closely mirror GitHub's. The notable
//! differences:
//! - Webhook signature header is `X-Gitea-Signature: <hex>` (raw, no
//!   `sha256=` prefix).
//! - Event header is `X-Gitea-Event` (`push`, `pull_request`, `ping`).
//! - The status API uses the same `/repos/{owner}/{repo}/statuses/{sha}`
//!   path but the response body shape is slightly different (no
//!   numeric `id`; we fall back to the URL).
//! - `api_url` is operator-configured per Q86 — there's no SaaS host.
//!
//! Code consciously written from scratch (not derived from any
//! GPL/EUPL'd reference). Shape that matches GitHub's is convergent —
//! both forges Gitea-shaped APIs from the same lineage.

use crate::errors::ForgeError;
use crate::events::{NormalizedEvent, PullRequestAction, PullRequestEvent, PushEvent};
use crate::permission::Permission;
use crate::{CheckHandle, CheckPost, CheckState, HookId, Provider};
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use medusa_domain::{Sha, Slug};
use reqwest::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use serde::Deserialize;
use sha2::Sha256;

const DEFAULT_USER_AGENT: &str = "medusa-ci";

#[derive(Debug, Clone)]
pub struct ForgejoProvider {
    /// e.g. `https://forgejo.example.com/api/v1`. No trailing slash.
    pub api_url: String,
    pub token: String,
    /// medusa's externally-visible URL for `target_url`s.
    pub external_url: String,
    pub user_agent: String,
    client: reqwest::Client,
}

impl ForgejoProvider {
    pub fn new(api_url: String, token: String, external_url: String) -> Self {
        Self::with_client(api_url, token, external_url, reqwest::Client::new())
    }

    pub fn with_client(
        api_url: String,
        token: String,
        external_url: String,
        client: reqwest::Client,
    ) -> Self {
        Self {
            api_url: api_url.trim_end_matches('/').to_string(),
            token,
            external_url: external_url.trim_end_matches('/').to_string(),
            user_agent: DEFAULT_USER_AGENT.to_string(),
            client,
        }
    }

    fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub(crate) fn verify_hmac_sha256(secret: &[u8], body: &[u8], signature_hex: &str) -> bool {
        type HmacSha256 = Hmac<Sha256>;
        let Ok(mut mac) = HmacSha256::new_from_slice(secret) else {
            return false;
        };
        mac.update(body);
        let computed = mac.finalize().into_bytes();
        let Ok(provided) = hex::decode(signature_hex) else {
            return false;
        };
        computed.as_slice() == provided.as_slice()
    }

    fn check_state_str(state: CheckState) -> &'static str {
        match state {
            CheckState::Pending => "pending",
            CheckState::Success => "success",
            CheckState::Failure => "failure",
            CheckState::Error => "error",
        }
    }
}

#[async_trait]
impl Provider for ForgejoProvider {
    async fn verify_signature(
        &self,
        headers: &[(String, String)],
        body: &[u8],
        secret_bytes: &[u8],
    ) -> Result<(), ForgeError> {
        // Forgejo sends raw hex (no `sha256=` prefix) under
        // `X-Gitea-Signature`. Some deployments also set
        // `X-Forgejo-Signature` — accept either.
        let header = Self::header(headers, "X-Gitea-Signature")
            .or_else(|| Self::header(headers, "X-Forgejo-Signature"))
            .ok_or(ForgeError::MissingHeader("X-Gitea-Signature"))?;
        if !Self::verify_hmac_sha256(secret_bytes, body, header) {
            return Err(ForgeError::BadSignature);
        }
        Ok(())
    }

    async fn parse_event(
        &self,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<Option<NormalizedEvent>, ForgeError> {
        let event = Self::header(headers, "X-Gitea-Event")
            .or_else(|| Self::header(headers, "X-Forgejo-Event"))
            .ok_or(ForgeError::MissingHeader("X-Gitea-Event"))?;
        match event {
            "push" => parse_push(body).map(|e| Some(NormalizedEvent::Push(e))),
            "pull_request" => {
                parse_pull_request(body).map(|e| Some(NormalizedEvent::PullRequest(e)))
            }
            "ping" => Ok(None),
            _ => Ok(None),
        }
    }

    async fn fetch_merge_ref(
        &self,
        slug: &Slug,
        pr_number: u64,
    ) -> Result<Option<Sha>, ForgeError> {
        // Forgejo doesn't pre-compute a "prospective merge SHA" the
        // way GitHub does, so this just returns the PR's `merge_base`
        // when present. Callers fall back to the head SHA per Q66.
        let url = format!(
            "{}/repos/{}/pulls/{}",
            self.api_url,
            slug.as_str(),
            pr_number,
        );
        let response = self
            .client
            .get(&url)
            .header(USER_AGENT, &self.user_agent)
            .header(ACCEPT, "application/json")
            .header(AUTHORIZATION, format!("token {}", self.token))
            .send()
            .await?;
        let status = response.status();
        if status.as_u16() == 401 {
            return Err(ForgeError::Unauthorised);
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ForgeError::Api {
                status: status.as_u16(),
                url,
                body,
            });
        }

        #[derive(Deserialize)]
        struct PullView {
            merge_base: Option<String>,
        }
        let body: PullView = response.json().await?;
        match body.merge_base {
            Some(s) => Sha::new(s.clone())
                .map(Some)
                .map_err(|e| ForgeError::InvalidSha(s, e)),
            None => Ok(None),
        }
    }

    async fn query_user_permission(
        &self,
        slug: &Slug,
        user: &str,
    ) -> Result<Permission, ForgeError> {
        // Gitea/Forgejo: same path as GitHub. Returns:
        // { "permission": "admin"|"write"|"read"|"none", "user": {...} }
        let url = format!(
            "{}/repos/{}/collaborators/{}/permission",
            self.api_url,
            slug.as_str(),
            user,
        );
        let response = self
            .client
            .get(&url)
            .header(USER_AGENT, &self.user_agent)
            .header(ACCEPT, "application/json")
            .header(AUTHORIZATION, format!("token {}", self.token))
            .send()
            .await?;
        let status = response.status();
        if status.as_u16() == 401 {
            return Err(ForgeError::Unauthorised);
        }
        if status.as_u16() == 404 {
            return Ok(Permission::None);
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ForgeError::Api {
                status: status.as_u16(),
                url,
                body,
            });
        }

        #[derive(Deserialize)]
        struct PermView {
            permission: String,
        }
        let body: PermView = response.json().await?;
        Ok(Permission::from_gitea(&body.permission))
    }

    async fn post_check(&self, post: CheckPost) -> Result<CheckHandle, ForgeError> {
        let url = format!(
            "{}/repos/{}/statuses/{}",
            self.api_url,
            post.slug.as_str(),
            post.sha.as_str(),
        );
        let mut payload = serde_json::Map::new();
        payload.insert(
            "state".into(),
            serde_json::Value::String(Self::check_state_str(post.state).to_string()),
        );
        payload.insert(
            "context".into(),
            serde_json::Value::String(post.context.clone()),
        );
        if let Some(d) = post.description {
            payload.insert("description".into(), serde_json::Value::String(d));
        }
        if let Some(t) = post.target_url {
            payload.insert("target_url".into(), serde_json::Value::String(t));
        }

        let response = self
            .client
            .post(&url)
            .header(USER_AGENT, &self.user_agent)
            .header(ACCEPT, "application/json")
            .header(AUTHORIZATION, format!("token {}", self.token))
            .json(&serde_json::Value::Object(payload))
            .send()
            .await?;
        let status = response.status();
        if status.as_u16() == 401 {
            return Err(ForgeError::Unauthorised);
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ForgeError::Api {
                status: status.as_u16(),
                url,
                body,
            });
        }

        // Forgejo's create-status response has an `id` integer; treat
        // it the same way GitHub does. Body parse failures fall back
        // to the URL so the caller still gets *something*.
        #[derive(Deserialize)]
        struct StatusView {
            id: Option<i64>,
        }
        let handle = match response.json::<StatusView>().await {
            Ok(v) => v.id.map(|i| i.to_string()).unwrap_or_else(|| url.clone()),
            Err(_) => url.clone(),
        };
        Ok(CheckHandle(handle))
    }

    async fn ensure_webhook(
        &self,
        slug: &Slug,
        target_url: &str,
        secret: &[u8],
    ) -> Result<HookId, ForgeError> {
        // Gitea/Forgejo: shape mirrors GitHub's REST surface.
        let list_url = format!("{}/repos/{}/hooks", self.api_url, slug.as_str());
        let list_resp = self
            .client
            .get(&list_url)
            .header(USER_AGENT, &self.user_agent)
            .header(ACCEPT, "application/json")
            .header(AUTHORIZATION, format!("token {}", self.token))
            .send()
            .await?;
        let status = list_resp.status();
        if status.as_u16() == 401 {
            return Err(ForgeError::Unauthorised);
        }
        if !status.is_success() {
            let body = list_resp.text().await.unwrap_or_default();
            return Err(ForgeError::Api {
                status: status.as_u16(),
                url: list_url,
                body,
            });
        }
        #[derive(Deserialize)]
        struct HookView {
            id: i64,
            config: HookConfig,
        }
        #[derive(Deserialize)]
        struct HookConfig {
            url: Option<String>,
        }
        let hooks: Vec<HookView> = list_resp.json().await?;
        let existing = hooks
            .into_iter()
            .find(|h| h.config.url.as_deref() == Some(target_url));

        // Send the secret bytes as a string (auto-install ensures
        // they're ASCII). The forge stores it as text and HMACs with
        // those text bytes — same key medusa uses to verify later.
        let secret_str = std::str::from_utf8(secret).map_err(|_| ForgeError::Api {
            status: 0,
            url: "ensure_webhook (local)".to_string(),
            body: "webhook secret is not valid UTF-8".to_string(),
        })?;
        let payload = serde_json::json!({
            "type": "gitea",
            "active": true,
            "events": ["push", "pull_request"],
            "config": {
                "url": target_url,
                "content_type": "json",
                "secret": secret_str,
            }
        });

        // Known Forgejo limitation: PATCHing an existing hook does NOT
        // update `config.secret` reliably (validated against Codeberg's
        // current Forgejo). The other fields (events, url,
        // content_type) update fine. If you regenerate the secret in
        // medusa's sqlite, you must also delete the hook in the
        // Forgejo UI so the next auto-install pass POSTs a fresh one
        // with the new secret. (TODO: detect "hook_id stored but
        // sqlite was just wiped" via a new `Provider::ensure_webhook`
        // arg taking the prior hook_id, and force delete+POST.)
        let (method, url) = match &existing {
            Some(h) => (
                reqwest::Method::PATCH,
                format!("{}/repos/{}/hooks/{}", self.api_url, slug.as_str(), h.id),
            ),
            None => (reqwest::Method::POST, list_url.clone()),
        };
        let resp = self
            .client
            .request(method, &url)
            .header(USER_AGENT, &self.user_agent)
            .header(ACCEPT, "application/json")
            .header(AUTHORIZATION, format!("token {}", self.token))
            .json(&payload)
            .send()
            .await?;
        let status = resp.status();
        if status.as_u16() == 401 {
            return Err(ForgeError::Unauthorised);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ForgeError::Api {
                status: status.as_u16(),
                url,
                body,
            });
        }
        let view: HookView = resp.json().await?;
        Ok(HookId(view.id.to_string()))
    }

    fn clone_url(&self, slug: &Slug) -> String {
        // Forgejo: derive the host by stripping the `/api/v1` suffix
        // (the standard path) from `api_url`.
        let host = derive_clone_host(&self.api_url);
        format!(
            "https://medusa:{}@{}/{}.git",
            self.token,
            host,
            slug.as_str(),
        )
    }
}

fn derive_clone_host(api_url: &str) -> String {
    let trimmed = api_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    if let Some((host, _)) = trimmed.split_once("/api/") {
        host.to_string()
    } else {
        trimmed.to_string()
    }
}

fn parse_push(body: &[u8]) -> Result<PushEvent, ForgeError> {
    #[derive(Deserialize)]
    struct PushView {
        #[serde(rename = "ref")]
        git_ref: String,
        after: String,
        repository: Repository,
        pusher: Option<Pusher>,
    }
    #[derive(Deserialize)]
    struct Repository {
        full_name: String,
    }
    #[derive(Deserialize)]
    struct Pusher {
        // Gitea/Forgejo use `username`; some Gitea versions use `login`.
        username: Option<String>,
        login: Option<String>,
    }

    let view: PushView = serde_json::from_slice(body).map_err(ForgeError::BadPayload)?;
    let slug = Slug::new(view.repository.full_name.clone())
        .map_err(|e| ForgeError::InvalidSlug(view.repository.full_name, e))?;
    let sha = Sha::new(view.after.clone()).map_err(|e| ForgeError::InvalidSha(view.after, e))?;

    Ok(PushEvent {
        slug,
        git_ref: view.git_ref,
        sha,
        pusher: view.pusher.and_then(|p| p.username.or(p.login)),
    })
}

fn parse_pull_request(body: &[u8]) -> Result<PullRequestEvent, ForgeError> {
    #[derive(Deserialize)]
    struct PrView {
        action: String,
        number: u64,
        repository: Repository,
        pull_request: PullRequest,
    }
    #[derive(Deserialize)]
    struct Repository {
        full_name: String,
    }
    #[derive(Deserialize)]
    struct PullRequest {
        head: Side,
        base: Side,
        // Gitea/Forgejo PR objects expose the author under `user`.
        user: User,
    }
    #[derive(Deserialize)]
    struct Side {
        sha: String,
        #[serde(rename = "ref")]
        git_ref: String,
        repo: Option<SideRepo>,
    }
    #[derive(Deserialize)]
    struct SideRepo {
        full_name: String,
    }
    #[derive(Deserialize)]
    struct User {
        // `login` on github, `username` or `login` on gitea/forgejo
        // depending on version. Try both.
        username: Option<String>,
        login: Option<String>,
    }

    let view: PrView = serde_json::from_slice(body).map_err(ForgeError::BadPayload)?;
    let slug = Slug::new(view.repository.full_name.clone())
        .map_err(|e| ForgeError::InvalidSlug(view.repository.full_name.clone(), e))?;
    let head_sha = Sha::new(view.pull_request.head.sha.clone())
        .map_err(|e| ForgeError::InvalidSha(view.pull_request.head.sha, e))?;

    let head_full = view
        .pull_request
        .head
        .repo
        .as_ref()
        .map(|r| r.full_name.clone());
    let is_fork = head_full
        .as_deref()
        .map(|h| h != view.repository.full_name)
        .unwrap_or(false);

    Ok(PullRequestEvent {
        slug,
        pr_number: view.number,
        head_sha,
        head_ref: view.pull_request.head.git_ref,
        base_ref: view.pull_request.base.git_ref,
        author: view
            .pull_request
            .user
            .username
            .or(view.pull_request.user.login)
            .unwrap_or_default(),
        action: PullRequestAction::from_str(&view.action),
        is_fork,
    })
}

#[cfg(test)]
mod tests;
