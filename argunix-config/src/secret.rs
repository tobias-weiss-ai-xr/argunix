use serde::Deserialize;
use std::fmt;
use std::path::{Path, PathBuf};

/// A path to a file containing a secret (token, webhook secret, signing key,
/// SSH private key, …). The path is resolved through [`crate::resolve_path`]
/// at deserialization time so `$CREDENTIALS_DIRECTORY/foo` works unmodified.
///
/// `Debug` prints the *path*, never the contents — and the contents are not
/// read at deserialization time. Read them only when you actually need them.
#[derive(Clone)]
pub struct SecretFile(PathBuf);

impl SecretFile {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl fmt::Debug for SecretFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SecretFile").field(&self.0).finish()
    }
}

impl<'de> Deserialize<'de> for SecretFile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let resolved = crate::resolve_path(&s).map_err(serde::de::Error::custom)?;
        Ok(SecretFile(resolved))
    }
}
