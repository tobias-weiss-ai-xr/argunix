//! Forge abstraction.
//!
//! The [`Provider`] trait is the single seam between medusa's CI logic and
//! whichever forge (github, gitlab, forgejo) is hosting a repo. v1 covers
//! the operations needed end-to-end:
//!
//! - verify an incoming webhook signature,
//! - parse a webhook body into a forge-agnostic [`NormalizedEvent`],
//! - look up the prospective merge ref for a fork PR,
//! - check whether a user has CI-trigger permission on a repo,
//! - post commit statuses / checks back to the forge.
//!
//! M5a ships the trait + the github implementation with PAT auth; gitlab,
//! forgejo, and github-app auth land later.

pub mod github;

mod errors;
mod events;
mod permission;

pub use errors::ForgeError;
pub use events::{NormalizedEvent, PullRequestAction, PullRequestEvent, PushEvent};
pub use github::GithubProvider;
pub use permission::Permission;

use async_trait::async_trait;
use medusa_domain::{Sha, Slug};

/// A forge-side identifier for a posted check / status. Returned by
/// `post_*_check` so we can update or replace it later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckHandle(pub String);

/// State of an evaluation we're reporting. Forge providers map these to
/// the underlying check / status terms (github statuses use `pending`,
/// `success`, `failure`, `error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    Pending,
    Success,
    Failure,
    Error,
}

/// Inputs needed to post a single forge status. The provider chooses
/// which API to call (statuses vs checks) based on its configured auth
/// kind.
#[derive(Debug, Clone)]
pub struct CheckPost {
    pub slug: Slug,
    pub sha: Sha,
    /// Short context name shown in the forge UI, e.g. `medusa: hello`.
    pub context: String,
    pub state: CheckState,
    /// Human-readable one-liner shown alongside the state.
    pub description: Option<String>,
    /// Click-through URL to medusa's UI (Q101).
    pub target_url: Option<String>,
}

#[async_trait]
pub trait Provider: Send + Sync {
    /// Verify the webhook signature on an incoming request. `secret_bytes`
    /// is the raw secret (already loaded from disk by the caller).
    async fn verify_signature(
        &self,
        headers: &[(String, String)],
        body: &[u8],
        secret_bytes: &[u8],
    ) -> Result<(), ForgeError>;

    /// Translate a webhook into a medusa-shaped event, or `Ok(None)` for
    /// events we deliberately ignore (pings, branch deletions, etc).
    async fn parse_event(
        &self,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<Option<NormalizedEvent>, ForgeError>;

    /// Fetch the prospective merge SHA for fork PR `pr_number` against
    /// `slug`'s default branch. Returns `Ok(None)` if the forge has not
    /// yet computed it.
    async fn fetch_merge_ref(&self, slug: &Slug, pr_number: u64)
    -> Result<Option<Sha>, ForgeError>;

    /// Resolve `user`'s permission level on `slug`.
    async fn query_user_permission(
        &self,
        slug: &Slug,
        user: &str,
    ) -> Result<Permission, ForgeError>;

    /// Post a check / commit status. Returns the forge-side handle.
    async fn post_check(&self, post: CheckPost) -> Result<CheckHandle, ForgeError>;

    /// Build a clone URL for `slug` that includes whatever auth this
    /// provider uses. v1 covers HTTPS-with-token; SSH and per-repo
    /// `clone.method` overrides are deferred to M7.
    fn clone_url(&self, slug: &Slug) -> String;
}
