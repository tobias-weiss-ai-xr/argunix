use serde::{Deserialize, Serialize};
use std::fmt;

/// An attribute path within a Nix expression, dot-separated as printed by
/// `nix-eval-jobs`, e.g. `packages.x86_64-linux.foo`.
///
/// We treat it as an opaque string for now; structure-aware parsing (handling
/// quoted segments like `packages."x86_64-linux"."weird.name"`) lands when we
/// actually need it.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AttrPath(String);

impl AttrPath {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AttrPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for AttrPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("AttrPath").field(&self.0).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let p = AttrPath::new("packages.x86_64-linux.foo");
        assert_eq!(p.as_str(), "packages.x86_64-linux.foo");
    }
}
