use serde::{Deserialize, Serialize};

/// User permission on a repository. Forge-agnostic; per-forge providers
/// translate their native permission strings to this enum.
///
/// `can_trigger_ci()` is the live half of the PR allowlist gate:
/// committers/maintainers are fine; strangers should not be able to
/// trigger random PRs. See [docs/concepts/allowlist.md].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    None,
    Read,
    Triage,
    Write,
    Maintain,
    Admin,
}

impl Permission {
    pub fn can_trigger_ci(self) -> bool {
        matches!(
            self,
            Permission::Write | Permission::Maintain | Permission::Admin
        )
    }
}

impl Permission {
    /// Map a github `permission` string (`admin`, `maintain`, `write`,
    /// `triage`, `read`, `none`) to our enum.
    pub(crate) fn from_github(s: &str) -> Self {
        match s {
            "admin" => Permission::Admin,
            "maintain" => Permission::Maintain,
            "write" => Permission::Write,
            "triage" => Permission::Triage,
            "read" => Permission::Read,
            _ => Permission::None,
        }
    }

    /// Map a Gitea/Forgejo `permission` string (`admin`, `write`,
    /// `read`, `none`) to our enum. Gitea has a coarser ladder than
    /// GitHub — no separate triage/maintain.
    pub(crate) fn from_gitea(s: &str) -> Self {
        match s {
            "admin" | "owner" => Permission::Admin,
            "write" => Permission::Write,
            "read" => Permission::Read,
            _ => Permission::None,
        }
    }

    /// Map a GitLab `access_level` integer to our enum.
    /// GitLab levels: 10 Guest, 20 Reporter, 30 Developer,
    /// 40 Maintainer, 50 Owner.
    pub(crate) fn from_gitlab_access_level(level: u32) -> Self {
        match level {
            50 => Permission::Admin,
            40 => Permission::Maintain,
            30 => Permission::Write,
            20 => Permission::Triage, // Reporter — read + report issues, no push
            10 => Permission::Read,
            _ => Permission::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ci_trigger_predicate() {
        assert!(Permission::Admin.can_trigger_ci());
        assert!(Permission::Maintain.can_trigger_ci());
        assert!(Permission::Write.can_trigger_ci());
        assert!(!Permission::Triage.can_trigger_ci());
        assert!(!Permission::Read.can_trigger_ci());
        assert!(!Permission::None.can_trigger_ci());
    }

    #[test]
    fn from_github_strings() {
        assert_eq!(Permission::from_github("admin"), Permission::Admin);
        assert_eq!(Permission::from_github("write"), Permission::Write);
        assert_eq!(Permission::from_github("read"), Permission::Read);
        assert_eq!(Permission::from_github("nonsense"), Permission::None);
    }
}
