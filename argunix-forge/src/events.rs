use argunix_domain::{Sha, Slug};

/// A webhook event in argunix-shaped form.
#[derive(Debug, Clone)]
pub enum NormalizedEvent {
    Push(PushEvent),
    PullRequest(PullRequestEvent),
}

#[derive(Debug, Clone)]
pub struct PushEvent {
    pub slug: Slug,
    /// `refs/heads/main`, `refs/tags/v1.0`, …
    pub git_ref: String,
    pub sha: Sha,
    /// Login name of the user who pushed (for permission checks). Some
    /// events carry the author rather than pusher; we pick whichever the
    /// forge gives us via `pusher.name` / `head_commit.author.login`.
    pub pusher: Option<String>,
    /// Forge-supplied display name (`repository.name` on GitHub/Forgejo,
    /// `project.name` on GitLab). `None` if the field is absent.
    pub repo_name: Option<String>,
    /// Forge-supplied description (`repository.description` /
    /// `project.description`). `None` if absent or empty.
    pub repo_description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PullRequestEvent {
    pub slug: Slug,
    pub pr_number: u64,
    pub head_sha: Sha,
    pub head_ref: String,
    pub base_ref: String,
    /// Login of the user who opened or pushed to the PR (used for the
    /// author-permission check).
    pub author: String,
    pub action: PullRequestAction,
    /// True iff the PR's head repo differs from the base repo (a fork PR).
    pub is_fork: bool,
    /// Forge-supplied repo display name. See [`PushEvent::repo_name`].
    pub repo_name: Option<String>,
    /// Forge-supplied repo description. See [`PushEvent::repo_description`].
    pub repo_description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestAction {
    Opened,
    Reopened,
    Synchronize,
    Closed,
    Edited,
    /// Anything we don't translate. Callers usually drop these.
    Other,
}

impl PullRequestAction {
    pub(crate) fn from_str(s: &str) -> Self {
        match s {
            "opened" => Self::Opened,
            "reopened" => Self::Reopened,
            "synchronize" => Self::Synchronize,
            "closed" => Self::Closed,
            "edited" => Self::Edited,
            _ => Self::Other,
        }
    }

    /// Should this action trigger an evaluation?
    pub fn should_evaluate(self) -> bool {
        matches!(self, Self::Opened | Self::Reopened | Self::Synchronize)
    }
}
