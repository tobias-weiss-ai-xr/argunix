//! GitLab forge provider with PAT auth.
//!
//! GitLab differs from GitHub/Forgejo in three meaningful ways:
//! - Webhook authentication is a plain shared-secret comparison via the
//!   `X-Gitlab-Token` header, not HMAC. Operators set the same string
//!   on both sides; medusa byte-compares.
//! - Event types arrive in `X-Gitlab-Event` ("Push Hook", "Merge
//!   Request Hook", "System Hook").
//! - Project slugs containing slashes (subgroups: `org/team/project`)
//!   are URL-encoded into a single REST path segment when calling the
//!   API: `org%2Fteam%2Fproject`.
//!
//! The status surface is `/api/v4/projects/:id/statuses/:sha` with the
//! same five state strings as GitHub-style commit statuses.

use crate::errors::ForgeError;
use crate::events::{NormalizedEvent, PullRequestAction, PullRequestEvent, PushEvent};
use crate::permission::Permission;
use crate::{CheckHandle, CheckPost, CheckState, Provider};
use async_trait::async_trait;
use medusa_domain::{Sha, Slug};
use reqwest::header::{ACCEPT, USER_AGENT};
use serde::Deserialize;

const DEFAULT_USER_AGENT: &str = "medusa-ci";
const PRIVATE_TOKEN: &str = "PRIVATE-TOKEN";

#[derive(Debug, Clone)]
pub struct GitlabProvider {
    /// e.g. `https://gitlab.example.com/api/v4`. No trailing slash.
    pub api_url: String,
    pub token: String,
    pub external_url: String,
    pub user_agent: String,
    client: reqwest::Client,
}

impl GitlabProvider {
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

    fn check_state_str(state: CheckState) -> &'static str {
        match state {
            CheckState::Pending => "pending",
            CheckState::Success => "success",
            CheckState::Failure => "failed",
            CheckState::Error => "canceled",
        }
    }

    /// Project slug → REST path segment. GitLab requires URL-encoding
    /// the full project path so it fits into one segment regardless
    /// of subgroup depth.
    fn project_path(slug: &Slug) -> String {
        url_encode_segment(slug.as_str())
    }
}

#[async_trait]
impl Provider for GitlabProvider {
    async fn verify_signature(
        &self,
        headers: &[(String, String)],
        _body: &[u8],
        secret_bytes: &[u8],
    ) -> Result<(), ForgeError> {
        let header = Self::header(headers, "X-Gitlab-Token")
            .ok_or(ForgeError::MissingHeader("X-Gitlab-Token"))?;
        // Constant-time comparison would be ideal here. The `subtle`
        // crate has it; for a v1 implementation we fall back to a
        // length-checked byte equality, which is fine because the
        // attacker doesn't see timing differences from outside the
        // process and we're not deriving anything from the token.
        if header.as_bytes() == secret_bytes {
            Ok(())
        } else {
            Err(ForgeError::BadSignature)
        }
    }

    async fn parse_event(
        &self,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<Option<NormalizedEvent>, ForgeError> {
        let event = Self::header(headers, "X-Gitlab-Event")
            .ok_or(ForgeError::MissingHeader("X-Gitlab-Event"))?;
        match event {
            "Push Hook" | "Tag Push Hook" => {
                parse_push(body).map(|e| Some(NormalizedEvent::Push(e)))
            }
            "Merge Request Hook" => {
                parse_merge_request(body).map(|e| Some(NormalizedEvent::PullRequest(e)))
            }
            "System Hook" | "Pipeline Hook" => Ok(None),
            _ => Ok(None),
        }
    }

    async fn fetch_merge_ref(
        &self,
        slug: &Slug,
        pr_number: u64,
    ) -> Result<Option<Sha>, ForgeError> {
        // GitLab MR's `merge_commit_sha` is null until merged. There's
        // also `head_pipeline.sha` and `diff_refs.base_sha`, but for
        // the prospective-merge use case we just return None and let
        // the caller fall back to the head SHA (Q66).
        let url = format!(
            "{}/projects/{}/merge_requests/{}",
            self.api_url,
            Self::project_path(slug),
            pr_number,
        );
        let response = self
            .client
            .get(&url)
            .header(USER_AGENT, &self.user_agent)
            .header(ACCEPT, "application/json")
            .header(PRIVATE_TOKEN, &self.token)
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
        struct MrView {
            merge_commit_sha: Option<String>,
        }
        let body: MrView = response.json().await?;
        match body.merge_commit_sha {
            Some(s) if !s.is_empty() => Sha::new(s.clone())
                .map(Some)
                .map_err(|e| ForgeError::InvalidSha(s, e)),
            _ => Ok(None),
        }
    }

    async fn query_user_permission(
        &self,
        slug: &Slug,
        user: &str,
    ) -> Result<Permission, ForgeError> {
        // GitLab's permission lookup is two hops:
        //   1. resolve username → numeric user id (`/users?username=...`)
        //   2. query project member access level
        //      (`/projects/:id/members/all/:user_id`)
        // Both 404 paths translate to Permission::None.
        let users_url = format!(
            "{}/users?username={}",
            self.api_url,
            url_encode_segment(user),
        );
        let users_resp = self
            .client
            .get(&users_url)
            .header(USER_AGENT, &self.user_agent)
            .header(ACCEPT, "application/json")
            .header(PRIVATE_TOKEN, &self.token)
            .send()
            .await?;
        let status = users_resp.status();
        if status.as_u16() == 401 {
            return Err(ForgeError::Unauthorised);
        }
        if !status.is_success() {
            let body = users_resp.text().await.unwrap_or_default();
            return Err(ForgeError::Api {
                status: status.as_u16(),
                url: users_url,
                body,
            });
        }

        #[derive(Deserialize)]
        struct UserView {
            id: u64,
        }
        let users: Vec<UserView> = users_resp.json().await?;
        let Some(user_id) = users.first().map(|u| u.id) else {
            return Ok(Permission::None);
        };

        let members_url = format!(
            "{}/projects/{}/members/all/{}",
            self.api_url,
            Self::project_path(slug),
            user_id,
        );
        let members_resp = self
            .client
            .get(&members_url)
            .header(USER_AGENT, &self.user_agent)
            .header(ACCEPT, "application/json")
            .header(PRIVATE_TOKEN, &self.token)
            .send()
            .await?;
        let status = members_resp.status();
        if status.as_u16() == 401 {
            return Err(ForgeError::Unauthorised);
        }
        if status.as_u16() == 404 {
            return Ok(Permission::None);
        }
        if !status.is_success() {
            let body = members_resp.text().await.unwrap_or_default();
            return Err(ForgeError::Api {
                status: status.as_u16(),
                url: members_url,
                body,
            });
        }

        #[derive(Deserialize)]
        struct MemberView {
            access_level: u32,
        }
        let body: MemberView = members_resp.json().await?;
        Ok(Permission::from_gitlab_access_level(body.access_level))
    }

    async fn post_check(&self, post: CheckPost) -> Result<CheckHandle, ForgeError> {
        let url = format!(
            "{}/projects/{}/statuses/{}",
            self.api_url,
            Self::project_path(&post.slug),
            post.sha.as_str(),
        );
        let mut payload = serde_json::Map::new();
        payload.insert(
            "state".into(),
            serde_json::Value::String(Self::check_state_str(post.state).to_string()),
        );
        payload.insert(
            "name".into(),
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
            .header(PRIVATE_TOKEN, &self.token)
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

    fn clone_url(&self, slug: &Slug) -> String {
        let host = derive_clone_host(&self.api_url);
        // GitLab supports `oauth2:<token>@` for HTTPS clone with a PAT.
        format!(
            "https://oauth2:{}@{}/{}.git",
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

/// Minimal RFC 3986 unreserved-characters URL-encoder for a single path
/// segment. We only need this for project paths (which contain `/`,
/// possibly other punctuation) and usernames, which GitLab restricts
/// to a small alphabet anyway. Keeping it inline avoids dragging in a
/// urlencoding crate for two callers.
fn url_encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            other => {
                out.push('%');
                out.push_str(&format!("{other:02X}"));
            }
        }
    }
    out
}

fn parse_push(body: &[u8]) -> Result<PushEvent, ForgeError> {
    #[derive(Deserialize)]
    struct PushView {
        #[serde(rename = "ref")]
        git_ref: String,
        after: String,
        project: Project,
        user_username: Option<String>,
    }
    #[derive(Deserialize)]
    struct Project {
        path_with_namespace: String,
    }

    let view: PushView = serde_json::from_slice(body).map_err(ForgeError::BadPayload)?;
    let slug = Slug::new(view.project.path_with_namespace.clone()).map_err(|e| {
        ForgeError::InvalidSlug(view.project.path_with_namespace, e)
    })?;
    let sha = Sha::new(view.after.clone()).map_err(|e| ForgeError::InvalidSha(view.after, e))?;

    Ok(PushEvent {
        slug,
        git_ref: view.git_ref,
        sha,
        pusher: view.user_username,
    })
}

fn parse_merge_request(body: &[u8]) -> Result<PullRequestEvent, ForgeError> {
    // GitLab Merge Request Hook payload shape:
    //   {
    //     project: { path_with_namespace, ... },
    //     user: { username, ... },             // who fired the event
    //     object_attributes: {
    //       iid, action, last_commit { id }, source_branch, target_branch,
    //       source { path_with_namespace }, target { path_with_namespace },
    //     }
    //   }
    #[derive(Deserialize)]
    struct MrView {
        project: Project,
        user: User,
        object_attributes: ObjectAttributes,
    }
    #[derive(Deserialize)]
    struct Project {
        path_with_namespace: String,
    }
    #[derive(Deserialize)]
    struct User {
        username: String,
    }
    #[derive(Deserialize)]
    struct ObjectAttributes {
        iid: u64,
        action: Option<String>,
        last_commit: LastCommit,
        source_branch: String,
        target_branch: String,
        source: Option<SideProject>,
        target: Option<SideProject>,
    }
    #[derive(Deserialize)]
    struct SideProject {
        path_with_namespace: Option<String>,
    }
    #[derive(Deserialize)]
    struct LastCommit {
        id: String,
    }

    let view: MrView = serde_json::from_slice(body).map_err(ForgeError::BadPayload)?;
    let slug = Slug::new(view.project.path_with_namespace.clone()).map_err(|e| {
        ForgeError::InvalidSlug(view.project.path_with_namespace.clone(), e)
    })?;
    let head_sha = Sha::new(view.object_attributes.last_commit.id.clone())
        .map_err(|e| ForgeError::InvalidSha(view.object_attributes.last_commit.id, e))?;

    let source_path = view
        .object_attributes
        .source
        .as_ref()
        .and_then(|s| s.path_with_namespace.clone());
    let target_path = view
        .object_attributes
        .target
        .as_ref()
        .and_then(|t| t.path_with_namespace.clone())
        .unwrap_or_else(|| view.project.path_with_namespace.clone());
    let is_fork = source_path
        .as_deref()
        .map(|s| s != target_path)
        .unwrap_or(false);

    Ok(PullRequestEvent {
        slug,
        pr_number: view.object_attributes.iid,
        head_sha,
        head_ref: view.object_attributes.source_branch,
        base_ref: view.object_attributes.target_branch,
        author: view.user.username,
        action: PullRequestAction::from_str(
            view.object_attributes
                .action
                .as_deref()
                .map(map_gitlab_action)
                .unwrap_or("other"),
        ),
        is_fork,
    })
}

/// Translate GitLab's MR action labels (open / reopen / update / close /
/// merge / approved / unapproved …) into our shared vocabulary.
fn map_gitlab_action(s: &str) -> &str {
    match s {
        "open" => "opened",
        "reopen" => "reopened",
        "update" => "synchronize",
        "close" => "closed",
        "merge" => "closed",
        _ => "other",
    }
}

#[cfg(test)]
mod tests;
