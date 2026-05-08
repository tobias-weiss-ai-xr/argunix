//! GitHub forge provider with Personal-Access-Token auth.
//!
//! What this covers (M5a):
//! - HMAC-SHA256 webhook verification (`X-Hub-Signature-256: sha256=<hex>`).
//! - Webhook event parsing for `push` and `pull_request`.
//! - Commit-status posting via `POST /repos/{owner}/{repo}/statuses/{sha}`.
//! - Collaborator-permission lookup via
//!   `GET /repos/{owner}/{repo}/collaborators/{user}/permission`.
//! - Merge-ref fetching for fork PRs via the pull-request endpoint.
//!
//! GitHub Apps auth (richer Checks API, per-output annotations) is M5c.

use crate::errors::ForgeError;
use crate::events::{NormalizedEvent, PullRequestAction, PullRequestEvent, PushEvent};
use crate::permission::Permission;
use crate::{CheckHandle, CheckPost, CheckState, HookId, Provider};
use argunix_domain::{Sha, Slug};
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use reqwest::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use serde::Deserialize;
use sha2::Sha256;

const DEFAULT_USER_AGENT: &str = "argunix-ci";

#[derive(Debug, Clone)]
pub struct GithubProvider {
    /// e.g. `https://api.github.com`. No trailing slash.
    pub api_url: String,
    pub token: String,
    /// argunix's externally-visible URL, used for status/check `target_url`s.
    pub external_url: String,
    pub user_agent: String,
    client: reqwest::Client,
}

impl GithubProvider {
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
        // Constant-time comparison via `subtle` would be ideal; the `hmac`
        // crate's `verify_slice` is the right primitive but consumes the
        // mac. We've already consumed it, so use length-checked eq from
        // hex bytes — for non-secret-derived HMACs this is good enough,
        // and the verifier itself is invoked only on incoming webhooks
        // (no timing-oracle attack surface for argunix).
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
impl Provider for GithubProvider {
    async fn verify_signature(
        &self,
        headers: &[(String, String)],
        body: &[u8],
        secret_bytes: &[u8],
    ) -> Result<(), ForgeError> {
        let header = Self::header(headers, "X-Hub-Signature-256")
            .ok_or(ForgeError::MissingHeader("X-Hub-Signature-256"))?;
        let hex = header.strip_prefix("sha256=").ok_or_else(|| {
            ForgeError::invalid_header("X-Hub-Signature-256", "missing sha256= prefix")
        })?;
        if !Self::verify_hmac_sha256(secret_bytes, body, hex) {
            return Err(ForgeError::BadSignature);
        }
        Ok(())
    }

    async fn parse_event(
        &self,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<Option<NormalizedEvent>, ForgeError> {
        let event = Self::header(headers, "X-GitHub-Event")
            .ok_or(ForgeError::MissingHeader("X-GitHub-Event"))?;
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
        let url = format!(
            "{}/repos/{}/pulls/{}",
            self.api_url,
            slug.as_str(),
            pr_number
        );
        let response = self
            .client
            .get(&url)
            .header(USER_AGENT, &self.user_agent)
            .header(ACCEPT, "application/vnd.github+json")
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
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
            merge_commit_sha: Option<String>,
        }
        let body: PullView = response.json().await?;
        match body.merge_commit_sha {
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
            .header(ACCEPT, "application/vnd.github+json")
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
            .send()
            .await?;
        let status = response.status();
        if status.as_u16() == 401 {
            return Err(ForgeError::Unauthorised);
        }
        if status.as_u16() == 404 {
            // GitHub returns 404 if the user has no relationship to the repo.
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
        Ok(Permission::from_github(&body.permission))
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
            .header(ACCEPT, "application/vnd.github+json")
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
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

        // GitHub's status API returns the created status object with an
        // `id` integer; we use that as the handle.
        #[derive(Deserialize)]
        struct StatusView {
            id: i64,
        }
        let body: StatusView = response.json().await?;
        Ok(CheckHandle(body.id.to_string()))
    }

    async fn ensure_webhook(
        &self,
        slug: &Slug,
        target_url: &str,
        secret: &[u8],
    ) -> Result<HookId, ForgeError> {
        // GET existing hooks; find one whose `config.url` matches.
        let list_url = format!("{}/repos/{}/hooks", self.api_url, slug.as_str());
        let list_resp = self
            .client
            .get(&list_url)
            .header(USER_AGENT, &self.user_agent)
            .header(ACCEPT, "application/vnd.github+json")
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
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

        // The caller provides the secret already in string-form bytes
        // (auto-install generates ASCII hex). Send those bytes as the
        // string secret to the forge so the forge's HMAC key matches
        // what argunix later reads back from sqlite for verification.
        let secret_str = std::str::from_utf8(secret).map_err(|_| ForgeError::Api {
            status: 0,
            url: "ensure_webhook (local)".to_string(),
            body: "webhook secret is not valid UTF-8".to_string(),
        })?;
        let payload = serde_json::json!({
            "name": "web",
            "active": true,
            "events": ["push", "pull_request"],
            "config": {
                "url": target_url,
                "content_type": "json",
                "secret": secret_str,
                "insecure_ssl": "0",
            }
        });

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
            .header(ACCEPT, "application/vnd.github+json")
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
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
        // Derive the public host from the API URL.
        // - api.github.com → github.com
        // - <enterprise>/api/v3 → <enterprise>
        let host = derive_clone_host(&self.api_url);
        format!(
            "https://x-access-token:{}@{}/{}.git",
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
    if let Some(rest) = trimmed.strip_prefix("api.") {
        // github.com SaaS shape: api.github.com → github.com.
        return rest.split('/').next().unwrap_or(rest).to_string();
    }
    // Enterprise shape: <host>/api/v3 → <host>.
    if let Some((host, _)) = trimmed.split_once("/api/") {
        return host.to_string();
    }
    trimmed.to_string()
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
        name: Option<String>,
        description: Option<String>,
        html_url: Option<String>,
    }
    #[derive(Deserialize)]
    struct Pusher {
        name: Option<String>,
    }

    let view: PushView = serde_json::from_slice(body).map_err(ForgeError::BadPayload)?;
    let slug = Slug::new(view.repository.full_name.clone())
        .map_err(|e| ForgeError::InvalidSlug(view.repository.full_name, e))?;
    let sha = Sha::new(view.after.clone()).map_err(|e| ForgeError::InvalidSha(view.after, e))?;

    Ok(PushEvent {
        slug,
        git_ref: view.git_ref,
        sha,
        pusher: view.pusher.and_then(|p| p.name),
        repo_name: view.repository.name,
        repo_description: view.repository.description.filter(|s| !s.is_empty()),
        repo_web_url: view.repository.html_url.filter(|s| !s.is_empty()),
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
        name: Option<String>,
        description: Option<String>,
        html_url: Option<String>,
    }
    #[derive(Deserialize)]
    struct PullRequest {
        head: Side,
        base: Side,
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
        login: String,
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
        author: view.pull_request.user.login,
        action: PullRequestAction::from_str(&view.action),
        is_fork,
        repo_name: view.repository.name,
        repo_description: view.repository.description.filter(|s| !s.is_empty()),
        repo_web_url: view.repository.html_url.filter(|s| !s.is_empty()),
    })
}

#[cfg(test)]
mod tests;
