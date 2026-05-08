use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForgeKind {
    Github,
    Gitlab,
    Forgejo,
}

impl ForgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ForgeKind::Github => "github",
            ForgeKind::Gitlab => "gitlab",
            ForgeKind::Forgejo => "forgejo",
        }
    }

    /// Web URL for pull request / merge request `pr_number` on
    /// `repo_web_url` (the project page URL — what the forge gives us
    /// in the `repository.html_url` / `repository.web_url` payload
    /// field).
    pub fn pr_url(self, repo_web_url: &str, pr_number: u32) -> String {
        let base = repo_web_url.trim_end_matches('/');
        match self {
            ForgeKind::Github => format!("{base}/pull/{pr_number}"),
            ForgeKind::Forgejo => format!("{base}/pulls/{pr_number}"),
            ForgeKind::Gitlab => format!("{base}/-/merge_requests/{pr_number}"),
        }
    }

    /// Web URL for branch `branch` on `repo_web_url`. Caller passes
    /// the short branch name (no `refs/heads/`), matching how we
    /// store `git_ref` post-normalization.
    pub fn branch_url(self, repo_web_url: &str, branch: &str) -> String {
        let base = repo_web_url.trim_end_matches('/');
        match self {
            ForgeKind::Github | ForgeKind::Forgejo => format!("{base}/tree/{branch}"),
            ForgeKind::Gitlab => format!("{base}/-/tree/{branch}"),
        }
    }

    /// Web URL for commit `sha` on `repo_web_url`.
    pub fn commit_url(self, repo_web_url: &str, sha: &str) -> String {
        let base = repo_web_url.trim_end_matches('/');
        match self {
            ForgeKind::Github | ForgeKind::Forgejo => format!("{base}/commit/{sha}"),
            ForgeKind::Gitlab => format!("{base}/-/commit/{sha}"),
        }
    }
}

impl fmt::Display for ForgeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercase_serde_names() {
        let k: ForgeKind = serde_json::from_str("\"github\"").unwrap();
        assert_eq!(k, ForgeKind::Github);
        let k: ForgeKind = serde_json::from_str("\"gitlab\"").unwrap();
        assert_eq!(k, ForgeKind::Gitlab);
        let k: ForgeKind = serde_json::from_str("\"forgejo\"").unwrap();
        assert_eq!(k, ForgeKind::Forgejo);
    }

    #[test]
    fn unknown_rejected() {
        assert!(serde_json::from_str::<ForgeKind>("\"gerrit\"").is_err());
    }

    #[test]
    fn url_builders_per_kind() {
        let gh = ForgeKind::Github;
        assert_eq!(
            gh.pr_url("https://github.com/me/repo", 7),
            "https://github.com/me/repo/pull/7"
        );
        assert_eq!(
            gh.branch_url("https://github.com/me/repo/", "main"),
            "https://github.com/me/repo/tree/main"
        );
        assert_eq!(
            gh.commit_url("https://github.com/me/repo", "abc123"),
            "https://github.com/me/repo/commit/abc123"
        );

        let gl = ForgeKind::Gitlab;
        assert_eq!(
            gl.pr_url("https://gitlab.com/me/repo", 7),
            "https://gitlab.com/me/repo/-/merge_requests/7"
        );
        assert_eq!(
            gl.branch_url("https://gitlab.com/me/repo", "main"),
            "https://gitlab.com/me/repo/-/tree/main"
        );
        assert_eq!(
            gl.commit_url("https://gitlab.com/me/repo", "abc123"),
            "https://gitlab.com/me/repo/-/commit/abc123"
        );

        let fj = ForgeKind::Forgejo;
        assert_eq!(
            fj.pr_url("https://codeberg.org/me/repo", 7),
            "https://codeberg.org/me/repo/pulls/7"
        );
        assert_eq!(
            fj.branch_url("https://codeberg.org/me/repo", "main"),
            "https://codeberg.org/me/repo/tree/main"
        );
        assert_eq!(
            fj.commit_url("https://codeberg.org/me/repo", "abc123"),
            "https://codeberg.org/me/repo/commit/abc123"
        );
    }
}
