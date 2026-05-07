//! On-disk persistence of the builder's ed25519 identity key.
//!
//! Mirrors the daemon-side `argunix-builders::host_key` shape: 32 raw
//! bytes (the seed), chmod 0600, regenerated on first boot. The agent
//! presents this key for SSH publickey auth on every reconnect; on
//! first contact, the daemon's TOFU path captures it via
//! `auth_publickey_offered` and writes it to the `builders` row.

use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("reading identity key file `{path}`: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("writing identity key file `{path}`: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("identity key `{path}` has wrong length: expected 32 bytes, got {got}")]
    WrongLength { path: PathBuf, got: usize },
}

/// The builder's persistent identity. ed25519 32-byte seed.
#[derive(Clone)]
pub struct PersistedKey(SigningKey);

impl PersistedKey {
    pub fn signing_key(&self) -> &SigningKey {
        &self.0
    }
    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.0.verifying_key().to_bytes()
    }
}

impl std::fmt::Debug for PersistedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pk = self.public_key_bytes();
        let prefix: String = pk.iter().take(8).map(|b| format!("{b:02x}")).collect();
        write!(f, "PersistedKey(pk={prefix}…)")
    }
}

/// Load the identity from `path`, generating + persisting one if the
/// file doesn't exist. The on-disk shape is the raw 32-byte seed —
/// no PEM, no SSH wire framing — matching the daemon's host-key
/// persistence so a single helper can be borrowed by both crates if
/// we ever consolidate.
pub fn load_or_generate(path: &Path) -> Result<PersistedKey, IdentityError> {
    match fs::read(path) {
        Ok(bytes) => {
            if bytes.len() != 32 {
                return Err(IdentityError::WrongLength {
                    path: path.to_path_buf(),
                    got: bytes.len(),
                });
            }
            let mut seed = [0u8; 32];
            seed.copy_from_slice(&bytes);
            Ok(PersistedKey(SigningKey::from_bytes(&seed)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = SigningKey::generate(&mut OsRng);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|source| IdentityError::Write {
                    path: path.to_path_buf(),
                    source,
                })?;
            }
            fs::write(path, key.to_bytes()).map_err(|source| IdentityError::Write {
                path: path.to_path_buf(),
                source,
            })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
            }
            Ok(PersistedKey(key))
        }
        Err(source) => Err(IdentityError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_through_persisted_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("id");
        let k1 = load_or_generate(&path).unwrap();
        let k2 = load_or_generate(&path).unwrap();
        assert_eq!(k1.public_key_bytes(), k2.public_key_bytes());
    }

    #[test]
    fn rejects_wrong_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("id");
        std::fs::write(&path, b"too short").unwrap();
        assert!(matches!(
            load_or_generate(&path).unwrap_err(),
            IdentityError::WrongLength { .. }
        ));
    }

    #[test]
    fn debug_keeps_seed_out_of_logs() {
        let dir = tempfile::tempdir().unwrap();
        let k = load_or_generate(&dir.path().join("id")).unwrap();
        let s = format!("{k:?}");
        assert!(s.contains("PersistedKey"));
        assert!(s.contains("…"));
    }
}
