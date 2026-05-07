//! Pre-eval gating for incoming webhook events.
//!
//! Sits between `Provider::parse_event` and the persist/queue step in the
//! webhook handler. Two responsibilities:
//!
//! - **Push events:** drop pushes whose ref isn't in the repo's
//!   `watched_branches`. Q84 promises glob support; v1 does exact-match
//!   against `refs/heads/<name>` and leaves globs for a follow-up.
//! - **PR events:** enforce the `build_prs` flag, then the Q3/Q31 gate:
//!   query the author's permission live; on success, allow if they have
//!   write or above; on either denial or forge failure, fall back to the
//!   per-repo `pr_allowlist`. Forge failures are logged at warn level
//!   (per Q31) but never reject a build that the allowlist would accept.

use crate::pause::PauseRegistry;
use argunix_config::Repo;
use argunix_forge::{ForgeError, NormalizedEvent, Provider, PullRequestEvent, PushEvent};
use std::sync::Arc;

/// Verdict for a single webhook event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Build,
    DropPushUnwatchedBranch { git_ref: String },
    DropPrsDisabled,
    DropPrUntrustedAuthor { author: String },
    DropPrIgnoredAction,
}

/// Apply the policy. The caller has already filtered the event to one
/// belonging to `repo`.
///
/// `forge_name` is the configured key under `forges:` and is used as the
/// PauseRegistry key — when `query_user_permission` returns 401 we mark
/// the forge paused; on success we mark it healthy. Q82.
pub async fn evaluate(
    provider: &Arc<dyn Provider>,
    repo: &Repo,
    event: &NormalizedEvent,
    forge_name: &str,
    pauses: &PauseRegistry,
) -> Decision {
    match event {
        NormalizedEvent::Push(push) => evaluate_push(repo, push),
        NormalizedEvent::PullRequest(pr) => {
            evaluate_pr(provider, repo, pr, forge_name, pauses).await
        }
    }
}

fn evaluate_push(repo: &Repo, push: &PushEvent) -> Decision {
    if branch_matches(&push.git_ref, &repo.watched_branches) {
        Decision::Build
    } else {
        Decision::DropPushUnwatchedBranch {
            git_ref: push.git_ref.clone(),
        }
    }
}

async fn evaluate_pr(
    provider: &Arc<dyn Provider>,
    repo: &Repo,
    pr: &PullRequestEvent,
    forge_name: &str,
    pauses: &PauseRegistry,
) -> Decision {
    if !pr.action.should_evaluate() {
        return Decision::DropPrIgnoredAction;
    }
    if !repo.build_prs {
        return Decision::DropPrsDisabled;
    }

    match provider.query_user_permission(&pr.slug, &pr.author).await {
        Ok(perm) if perm.can_trigger_ci() => {
            // The call landed without auth issues — clear any prior pause.
            pauses.mark_healthy(forge_name);
            Decision::Build
        }
        Ok(perm) => {
            pauses.mark_healthy(forge_name);
            if author_in_allowlist(&pr.author, &repo.pr_allowlist) {
                Decision::Build
            } else {
                tracing::info!(
                    author = %pr.author,
                    permission = ?perm,
                    pr = pr.pr_number,
                    "PR author lacks CI permission and is not in pr_allowlist; dropping",
                );
                Decision::DropPrUntrustedAuthor {
                    author: pr.author.clone(),
                }
            }
        }
        Err(e) => {
            // Q82: 401 means the token is broken — pause this forge.
            // Other errors (network blips, 5xx) don't pause, just fall
            // through to the existing Q31 allowlist-fallback behaviour.
            if matches!(e, ForgeError::Unauthorised) {
                pauses.pause(forge_name, "401 from query_user_permission");
            }
            // Q31: forge query failed — fall back to allowlist only.
            if author_in_allowlist(&pr.author, &repo.pr_allowlist) {
                tracing::warn!(
                    error = %e,
                    author = %pr.author,
                    pr = pr.pr_number,
                    "permission query failed; allowing via pr_allowlist fallback",
                );
                Decision::Build
            } else {
                tracing::warn!(
                    error = %e,
                    author = %pr.author,
                    pr = pr.pr_number,
                    "permission query failed and author not in pr_allowlist; dropping",
                );
                Decision::DropPrUntrustedAuthor {
                    author: pr.author.clone(),
                }
            }
        }
    }
}

fn author_in_allowlist(author: &str, allowlist: &[String]) -> bool {
    allowlist.iter().any(|a| a == author)
}

/// True if `git_ref` matches any of `branches`. Accepts either bare branch
/// names (`main`) or fully-qualified refs (`refs/heads/main`); the configured
/// list is canonicalised to a leading `refs/heads/` so both forms work.
/// Tag pushes (`refs/tags/...`) never match.
fn branch_matches(git_ref: &str, branches: &[String]) -> bool {
    if !git_ref.starts_with("refs/heads/") {
        return false;
    }
    branches.iter().any(|b| {
        let canonical = if b.starts_with("refs/") {
            b.clone()
        } else {
            format!("refs/heads/{b}")
        };
        canonical == git_ref
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use argunix_config::{CloneConfig, EvalOverrides, Repo};
    use argunix_domain::{Sha, Slug};
    use argunix_forge::{
        CheckHandle, CheckPost, ForgeError, NormalizedEvent, Permission, Provider,
        PullRequestAction, PullRequestEvent, PushEvent,
    };
    use async_trait::async_trait;
    use std::sync::Arc;

    fn repo_with(build_prs: bool, allowlist: Vec<&str>, watched: Vec<&str>) -> Repo {
        Repo {
            slug: Slug::new("myorg/myrepo").unwrap(),
            forge: "github-myorg".into(),
            watched_branches: watched.into_iter().map(String::from).collect(),
            build_prs,
            pr_allowlist: allowlist.into_iter().map(String::from).collect(),
            clone: CloneConfig::default(),
            eval: EvalOverrides::default(),
            collapsed_check_threshold: None,
            weight: 1,
        }
    }

    fn push(git_ref: &str) -> NormalizedEvent {
        NormalizedEvent::Push(PushEvent {
            slug: Slug::new("myorg/myrepo").unwrap(),
            git_ref: git_ref.into(),
            sha: Sha::new("0123456789abcdef0123456789abcdef01234567").unwrap(),
            pusher: Some("alice".into()),
            repo_name: None,
            repo_description: None,
        })
    }

    fn pr(action: PullRequestAction, author: &str) -> NormalizedEvent {
        NormalizedEvent::PullRequest(PullRequestEvent {
            slug: Slug::new("myorg/myrepo").unwrap(),
            pr_number: 7,
            head_sha: Sha::new("1111111111111111111111111111111111111111").unwrap(),
            head_ref: "feature".into(),
            base_ref: "main".into(),
            author: author.into(),
            action,
            is_fork: true,
            repo_name: None,
            repo_description: None,
        })
    }

    /// Test double for `Provider`. Only `query_user_permission` is meaningful;
    /// the rest panic so a misuse shows up loudly.
    struct FakeProvider {
        result: Result<Permission, ForgeErrorKind>,
    }

    #[derive(Clone)]
    enum ForgeErrorKind {
        Unauthorised,
    }

    #[async_trait]
    impl Provider for FakeProvider {
        async fn verify_signature(
            &self,
            _: &[(String, String)],
            _: &[u8],
            _: &[u8],
        ) -> Result<(), ForgeError> {
            unreachable!("policy never verifies signatures");
        }
        async fn parse_event(
            &self,
            _: &[(String, String)],
            _: &[u8],
        ) -> Result<Option<NormalizedEvent>, ForgeError> {
            unreachable!("policy never parses events");
        }
        async fn fetch_merge_ref(&self, _: &Slug, _: u64) -> Result<Option<Sha>, ForgeError> {
            unreachable!("policy never fetches merge refs");
        }
        async fn query_user_permission(&self, _: &Slug, _: &str) -> Result<Permission, ForgeError> {
            match &self.result {
                Ok(p) => Ok(*p),
                Err(ForgeErrorKind::Unauthorised) => Err(ForgeError::Unauthorised),
            }
        }
        async fn post_check(&self, _: CheckPost) -> Result<CheckHandle, ForgeError> {
            unreachable!("policy never posts checks");
        }
        async fn ensure_webhook(
            &self,
            _: &Slug,
            _: &str,
            _: &[u8],
        ) -> Result<argunix_forge::HookId, ForgeError> {
            unreachable!("policy never installs webhooks");
        }
        fn clone_url(&self, _: &Slug) -> String {
            unreachable!("policy never builds clone URLs");
        }
    }

    fn provider_returning(perm: Permission) -> Arc<dyn Provider> {
        Arc::new(FakeProvider { result: Ok(perm) }) as Arc<dyn Provider>
    }

    fn provider_unauthorised() -> Arc<dyn Provider> {
        Arc::new(FakeProvider {
            result: Err(ForgeErrorKind::Unauthorised),
        }) as Arc<dyn Provider>
    }

    fn fresh_pauses() -> PauseRegistry {
        PauseRegistry::new()
    }

    #[tokio::test]
    async fn push_to_watched_branch_builds() {
        let repo = repo_with(true, vec![], vec!["main"]);
        let prov = provider_returning(Permission::Read);
        let pauses = fresh_pauses();
        assert_eq!(
            evaluate(&prov, &repo, &push("refs/heads/main"), "gh", &pauses).await,
            Decision::Build,
        );
    }

    #[tokio::test]
    async fn push_to_unwatched_branch_dropped() {
        let repo = repo_with(true, vec![], vec!["main"]);
        let prov = provider_returning(Permission::Read);
        let pauses = fresh_pauses();
        let d = evaluate(&prov, &repo, &push("refs/heads/feature"), "gh", &pauses).await;
        assert!(matches!(d, Decision::DropPushUnwatchedBranch { .. }));
    }

    #[tokio::test]
    async fn push_with_fully_qualified_config_entry() {
        let repo = repo_with(true, vec![], vec!["refs/heads/release"]);
        let prov = provider_returning(Permission::Read);
        let pauses = fresh_pauses();
        assert_eq!(
            evaluate(&prov, &repo, &push("refs/heads/release"), "gh", &pauses).await,
            Decision::Build,
        );
    }

    #[tokio::test]
    async fn tag_push_never_matches_branch_list() {
        let repo = repo_with(true, vec![], vec!["main", "v1"]);
        let prov = provider_returning(Permission::Read);
        let pauses = fresh_pauses();
        let d = evaluate(&prov, &repo, &push("refs/tags/v1"), "gh", &pauses).await;
        assert!(matches!(d, Decision::DropPushUnwatchedBranch { .. }));
    }

    #[tokio::test]
    async fn pr_with_writer_author_builds() {
        let repo = repo_with(true, vec![], vec!["main"]);
        let prov = provider_returning(Permission::Write);
        let pauses = fresh_pauses();
        assert_eq!(
            evaluate(
                &prov,
                &repo,
                &pr(PullRequestAction::Opened, "alice"),
                "gh",
                &pauses
            )
            .await,
            Decision::Build,
        );
    }

    #[tokio::test]
    async fn pr_with_stranger_author_dropped() {
        let repo = repo_with(true, vec![], vec!["main"]);
        let prov = provider_returning(Permission::None);
        let pauses = fresh_pauses();
        let d = evaluate(
            &prov,
            &repo,
            &pr(PullRequestAction::Opened, "stranger"),
            "gh",
            &pauses,
        )
        .await;
        assert!(matches!(d, Decision::DropPrUntrustedAuthor { .. }));
    }

    #[tokio::test]
    async fn pr_with_stranger_author_in_allowlist_builds() {
        let repo = repo_with(true, vec!["stranger"], vec!["main"]);
        let prov = provider_returning(Permission::None);
        let pauses = fresh_pauses();
        assert_eq!(
            evaluate(
                &prov,
                &repo,
                &pr(PullRequestAction::Opened, "stranger"),
                "gh",
                &pauses,
            )
            .await,
            Decision::Build,
        );
    }

    #[tokio::test]
    async fn pr_when_build_prs_disabled_dropped() {
        let repo = repo_with(false, vec!["alice"], vec!["main"]);
        let prov = provider_returning(Permission::Admin);
        let pauses = fresh_pauses();
        assert_eq!(
            evaluate(
                &prov,
                &repo,
                &pr(PullRequestAction::Opened, "alice"),
                "gh",
                &pauses
            )
            .await,
            Decision::DropPrsDisabled,
        );
    }

    #[tokio::test]
    async fn pr_with_ignored_action_dropped() {
        let repo = repo_with(true, vec![], vec!["main"]);
        let prov = provider_returning(Permission::Admin);
        let pauses = fresh_pauses();
        let d = evaluate(
            &prov,
            &repo,
            &pr(PullRequestAction::Closed, "alice"),
            "gh",
            &pauses,
        )
        .await;
        assert_eq!(d, Decision::DropPrIgnoredAction);
    }

    #[tokio::test]
    async fn pr_forge_failure_falls_back_to_allowlist_allow() {
        let repo = repo_with(true, vec!["alice"], vec!["main"]);
        let prov = provider_unauthorised();
        let pauses = fresh_pauses();
        assert_eq!(
            evaluate(
                &prov,
                &repo,
                &pr(PullRequestAction::Opened, "alice"),
                "gh",
                &pauses
            )
            .await,
            Decision::Build,
        );
    }

    #[tokio::test]
    async fn pr_forge_failure_without_allowlist_drops() {
        let repo = repo_with(true, vec![], vec!["main"]);
        let prov = provider_unauthorised();
        let pauses = fresh_pauses();
        let d = evaluate(
            &prov,
            &repo,
            &pr(PullRequestAction::Opened, "alice"),
            "gh",
            &pauses,
        )
        .await;
        assert!(matches!(d, Decision::DropPrUntrustedAuthor { .. }));
    }

    /// Q82: a 401 from query_user_permission marks the forge paused.
    #[tokio::test]
    async fn pr_unauthorised_pauses_forge() {
        let repo = repo_with(true, vec![], vec!["main"]);
        let prov = provider_unauthorised();
        let pauses = fresh_pauses();
        let _ = evaluate(
            &prov,
            &repo,
            &pr(PullRequestAction::Opened, "alice"),
            "gh",
            &pauses,
        )
        .await;
        assert!(pauses.is_paused("gh"));
    }

    /// Q82: a successful permission lookup clears any prior pause —
    /// this is how argunix auto-recovers after a token rotation.
    #[tokio::test]
    async fn pr_successful_query_unpauses_forge() {
        let repo = repo_with(true, vec![], vec!["main"]);
        let pauses = fresh_pauses();
        pauses.pause("gh", "401 from earlier webhook");
        assert!(pauses.is_paused("gh"));

        let prov = provider_returning(Permission::Write);
        let _ = evaluate(
            &prov,
            &repo,
            &pr(PullRequestAction::Opened, "alice"),
            "gh",
            &pauses,
        )
        .await;
        assert!(!pauses.is_paused("gh"));
    }
}
