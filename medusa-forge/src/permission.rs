use serde::{Deserialize, Serialize};

/// User permission on a repository. Forge-agnostic; per-forge providers
/// translate their native permission strings to this enum.
///
/// `can_trigger_ci()` is the predicate Q3 hinges on: "committers/maintainers
/// are fine, strangers should not be able to trigger random PRs".
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
