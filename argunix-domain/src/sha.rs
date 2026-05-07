use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ShaError {
    #[error("git sha must be exactly 40 hex characters")]
    Invalid,
}

#[derive(Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Sha(String);

impl Sha {
    pub fn new(s: impl Into<String>) -> Result<Self, ShaError> {
        let s = s.into();
        if s.len() != 40 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(ShaError::Invalid);
        }
        Ok(Self(s.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn short(&self) -> &str {
        &self.0[..7]
    }
}

impl fmt::Display for Sha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Sha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Sha").field(&self.0).finish()
    }
}

impl<'de> Deserialize<'de> for Sha {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Sha::new(s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_40_hex() {
        let s = "0123456789abcdef0123456789ABCDEF01234567";
        let sha = Sha::new(s).unwrap();
        assert_eq!(sha.as_str(), s.to_ascii_lowercase());
    }

    #[test]
    fn rejects_short() {
        assert!(Sha::new("abc").is_err());
    }

    #[test]
    fn rejects_non_hex() {
        let s = "g123456789abcdef0123456789abcdef01234567";
        assert!(Sha::new(s).is_err());
    }

    #[test]
    fn short_is_seven_chars() {
        let s = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(Sha::new(s).unwrap().short(), "0123456");
    }
}
