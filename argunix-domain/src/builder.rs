use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BuilderNameError {
    #[error("builder name must not be empty")]
    Empty,
    #[error("builder name must be ≤ 64 chars")]
    TooLong,
    #[error("builder name must contain only ASCII letters, digits, '-', '_', '.'")]
    InvalidChars,
}

/// Operator-visible identifier for a builder. Defaults to the builder's
/// `networking.hostName` but the operator can override on the builder side.
/// Constrained so it works as a filename component (`/run/argunix/builders/<name>.sock`),
/// a column in `argunixctl builders` output, and a URL-safe segment.
#[derive(Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct BuilderName(String);

impl BuilderName {
    pub fn new(s: impl Into<String>) -> Result<Self, BuilderNameError> {
        let s = s.into();
        if s.is_empty() {
            return Err(BuilderNameError::Empty);
        }
        if s.len() > 64 {
            return Err(BuilderNameError::TooLong);
        }
        if !s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            return Err(BuilderNameError::InvalidChars);
        }
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BuilderName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for BuilderName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("BuilderName").field(&self.0).finish()
    }
}

impl<'de> Deserialize<'de> for BuilderName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        BuilderName::new(s).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BuilderPubkeyError {
    #[error("ed25519 pubkey must be exactly 32 bytes, got {0}")]
    WrongLength(usize),
}

/// Raw ed25519 public key, exactly 32 bytes. SSH wraps this in its own
/// framing on the wire; we strip to the raw key for storage and comparison.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct BuilderPubkey([u8; 32]);

impl BuilderPubkey {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, BuilderPubkeyError> {
        if bytes.len() != 32 {
            return Err(BuilderPubkeyError::WrongLength(bytes.len()));
        }
        let mut a = [0u8; 32];
        a.copy_from_slice(bytes);
        Ok(Self(a))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for BuilderPubkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // First 8 bytes hex is enough to distinguish at a glance without
        // dumping the full key into logs.
        let prefix: String = self.0.iter().take(8).map(|b| format!("{b:02x}")).collect();
        write!(f, "BuilderPubkey({prefix}…)")
    }
}

/// What a builder can do, as reported by its own `nix show-config` on every
/// reconnect. Source-of-truth lives on the builder; argunix caches the latest
/// snapshot in the `builders` sqlite row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuilderCapabilities {
    /// Every `<system>` the builder can realise: its native `system`
    /// plus any `extra-platforms` (e.g. binfmt-emulated targets). The
    /// native one is always also present here; see [`Self::native_system`].
    pub systems: Vec<String>,
    /// The builder's own `system` from `nix show-config` — the platform
    /// it runs *natively* rather than under emulation. Used by the
    /// scheduler to prefer native builders absolutely: an emulated
    /// builder is only considered for a `<system>` when no native
    /// builder for it is connected. Empty string means "unknown"
    /// (e.g. a pre-native-system agent); such a builder is treated as
    /// non-native for every system.
    #[serde(default)]
    pub native_system: String,
    pub features: Vec<String>,
    pub max_jobs: u32,
    pub nix_version: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_name_accepts_hostname_shapes() {
        BuilderName::new("bobs-mini").unwrap();
        BuilderName::new("alices_thinkpad.local").unwrap();
        BuilderName::new("mac01").unwrap();
    }

    #[test]
    fn builder_name_rejects_empty() {
        assert_eq!(BuilderName::new(""), Err(BuilderNameError::Empty));
    }

    #[test]
    fn builder_name_rejects_too_long() {
        let s: String = std::iter::repeat('a').take(65).collect();
        assert_eq!(BuilderName::new(s), Err(BuilderNameError::TooLong));
    }

    #[test]
    fn builder_name_rejects_slash() {
        // we route via /run/argunix/builders/<name>.sock — slashes break that.
        assert_eq!(BuilderName::new("a/b"), Err(BuilderNameError::InvalidChars));
    }

    #[test]
    fn builder_name_rejects_whitespace() {
        assert_eq!(BuilderName::new("a b"), Err(BuilderNameError::InvalidChars));
    }

    #[test]
    fn pubkey_round_trip() {
        let raw = [7u8; 32];
        let k = BuilderPubkey::from_bytes(&raw).unwrap();
        assert_eq!(k.as_bytes(), &raw);
    }

    #[test]
    fn pubkey_rejects_short() {
        assert_eq!(
            BuilderPubkey::from_bytes(&[0u8; 31]),
            Err(BuilderPubkeyError::WrongLength(31))
        );
    }

    #[test]
    fn pubkey_debug_truncates() {
        let k = BuilderPubkey::from_bytes(&[0xab; 32]).unwrap();
        let s = format!("{k:?}");
        assert!(s.contains("abababab"));
        assert!(s.contains('…'));
    }
}
