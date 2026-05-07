use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SlugError {
    #[error("slug must not be empty")]
    Empty,
    #[error("slug must contain at least one '/' (org/repo or org/sub/.../repo)")]
    MissingSeparator,
    #[error("slug must not start or end with '/' or contain '//'")]
    InvalidSlashes,
    #[error("slug must not contain whitespace or control characters")]
    InvalidChars,
}

/// A repository slug. Supports nested paths (gitlab subgroups), e.g.
/// `myorg/marketing/marketing-project-1`.
#[derive(Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Slug(String);

impl Slug {
    pub fn new(s: impl Into<String>) -> Result<Self, SlugError> {
        let s = s.into();
        if s.is_empty() {
            return Err(SlugError::Empty);
        }
        if s.starts_with('/') || s.ends_with('/') || s.contains("//") {
            return Err(SlugError::InvalidSlashes);
        }
        if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(SlugError::InvalidChars);
        }
        if !s.contains('/') {
            return Err(SlugError::MissingSeparator);
        }
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The path segments. For `myorg/sub/repo` this returns `["myorg", "sub", "repo"]`.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }
}

impl fmt::Display for Slug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Slug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Slug").field(&self.0).finish()
    }
}

impl<'de> Deserialize<'de> for Slug {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Slug::new(s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_org_repo() {
        assert_eq!(Slug::new("myorg/myrepo").unwrap().as_str(), "myorg/myrepo");
    }

    #[test]
    fn accepts_subgroup() {
        let s = Slug::new("myorg/marketing/marketing-project-1").unwrap();
        assert_eq!(
            s.segments().collect::<Vec<_>>(),
            vec!["myorg", "marketing", "marketing-project-1"]
        );
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(Slug::new(""), Err(SlugError::Empty));
    }

    #[test]
    fn rejects_no_slash() {
        assert_eq!(Slug::new("flat"), Err(SlugError::MissingSeparator));
    }

    #[test]
    fn rejects_leading_slash() {
        assert_eq!(Slug::new("/a/b"), Err(SlugError::InvalidSlashes));
    }

    #[test]
    fn rejects_trailing_slash() {
        assert_eq!(Slug::new("a/b/"), Err(SlugError::InvalidSlashes));
    }

    #[test]
    fn rejects_double_slash() {
        assert_eq!(Slug::new("a//b"), Err(SlugError::InvalidSlashes));
    }

    #[test]
    fn rejects_whitespace() {
        assert_eq!(Slug::new("a b/c"), Err(SlugError::InvalidChars));
    }
}
