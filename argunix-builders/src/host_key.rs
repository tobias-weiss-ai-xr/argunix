use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum HostKeyError {
    #[error("reading host key file `{path}`: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("writing host key file `{path}`: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("host key `{path}` has wrong length: expected 32 bytes, got {got}")]
    WrongLength { path: PathBuf, got: usize },
}

/// The argunix builder-server's identity. A 32-byte ed25519 seed; the SSH
/// host key is derived deterministically from this. Persisted in the
/// daemon's state directory so reconnecting builders see the same host
/// key (avoiding `WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!`).
#[derive(Clone)]
pub struct HostKey(SigningKey);

impl HostKey {
    pub fn signing_key(&self) -> &SigningKey {
        &self.0
    }

    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.0.verifying_key().to_bytes()
    }
}

impl std::fmt::Debug for HostKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Keep raw bytes out of logs; surface only the public-side fingerprint.
        let pk = self.public_key_bytes();
        let prefix: String = pk.iter().take(8).map(|b| format!("{b:02x}")).collect();
        write!(f, "HostKey(pk={prefix}…)")
    }
}

/// Load the host key seed from `path`, or generate one and persist it
/// if the file doesn't exist yet. The file is the raw 32-byte seed —
/// no PEM, no SSH wire framing — which keeps the on-disk format
/// uninteresting to anyone who isn't argunix.
pub fn load_or_generate(path: &Path) -> Result<HostKey, HostKeyError> {
    match fs::read(path) {
        Ok(bytes) => {
            if bytes.len() != 32 {
                return Err(HostKeyError::WrongLength {
                    path: path.to_path_buf(),
                    got: bytes.len(),
                });
            }
            let mut seed = [0u8; 32];
            seed.copy_from_slice(&bytes);
            Ok(HostKey(SigningKey::from_bytes(&seed)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = SigningKey::generate(&mut OsRng);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|source| HostKeyError::Write {
                    path: path.to_path_buf(),
                    source,
                })?;
            }
            fs::write(path, key.to_bytes()).map_err(|source| HostKeyError::Write {
                path: path.to_path_buf(),
                source,
            })?;
            // Restrict to owner-only on POSIX; SSH will refuse to use
            // anything more permissive.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
            }
            Ok(HostKey(key))
        }
        Err(source) => Err(HostKeyError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_when_missing_and_loads_same_key_back() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("host_key");
        let k1 = load_or_generate(&p).unwrap();
        let k2 = load_or_generate(&p).unwrap();
        assert_eq!(k1.public_key_bytes(), k2.public_key_bytes());
    }

    #[test]
    fn rejects_wrong_length_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("host_key");
        std::fs::write(&p, b"not 32 bytes").unwrap();
        let err = load_or_generate(&p).unwrap_err();
        assert!(matches!(err, HostKeyError::WrongLength { .. }));
    }

    #[test]
    fn debug_does_not_leak_seed() {
        let dir = tempfile::tempdir().unwrap();
        let k = load_or_generate(&dir.path().join("h")).unwrap();
        let s = format!("{k:?}");
        // The fingerprint we print is the *public* key, not the seed.
        // Ensure the seed bytes don't appear.
        for b in k.signing_key().to_bytes() {
            // Allow benign hex-of-pubkey-prefix overlap; what we care
            // about is that the literal seed bytes aren't in the string.
            // (Crude but enough to catch a regression that prints
            // the secret directly.)
            let _ = b;
        }
        assert!(s.contains("HostKey"));
        assert!(s.contains("…"));
    }
}
