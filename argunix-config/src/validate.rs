use crate::SecretFile;
use crate::schema::{Config, ForgeAuth, ForgeAuthShapeError};
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("repo `{repo}` uses clone.method=ssh but has no clone.ssh_key_path")]
    SshWithoutKey { repo: String },
    #[error("forge `{forge}`: {error}")]
    ForgeAuth {
        forge: String,
        #[source]
        error: ForgeAuthShapeError,
    },
    #[error("secret file `{path}` is not readable: {error}")]
    SecretUnreadable { path: PathBuf, error: String },
    #[error("builder_enrollment.listen `{listen}` is not a valid socket address: {error}")]
    BuilderListenInvalid { listen: String, error: String },
}

impl Config {
    /// Cross-reference checks that don't touch the filesystem.
    ///
    /// The "repo references unknown forge" case that used to live here
    /// is now structurally impossible: repos are nested under their
    /// forge in the YAML, so the runtime model can't have a repo whose
    /// `forge` field doesn't appear in `forges`.
    pub fn validate_references(&self) -> Result<(), ValidationError> {
        for (name, forge) in &self.forges {
            forge.auth().map_err(|e| ValidationError::ForgeAuth {
                forge: name.clone(),
                error: e,
            })?;
        }
        for repo in &self.repos {
            if matches!(repo.clone.method, crate::CloneMethod::Ssh)
                && repo.clone.ssh_key_path.is_none()
            {
                return Err(ValidationError::SshWithoutKey {
                    repo: repo.slug.as_str().to_string(),
                });
            }
        }
        if let Some(b) = &self.builder_enrollment {
            // Parsing eagerly here means a typo like `0.0.0.0::2222` is
            // caught at config load, not at the russh bind() call deep
            // in the daemon's startup sequence.
            b.listen.parse::<std::net::SocketAddr>().map_err(|e| {
                ValidationError::BuilderListenInvalid {
                    listen: b.listen.clone(),
                    error: e.to_string(),
                }
            })?;
        }
        Ok(())
    }

    /// Verify that every secret file is present and readable. Used at daemon
    /// startup; not in tests, since the secrets typically don't exist there.
    pub fn validate_secrets_exist(&self) -> Result<(), ValidationError> {
        for (name, forge) in &self.forges {
            let auth = forge.auth().map_err(|e| ValidationError::ForgeAuth {
                forge: name.clone(),
                error: e,
            })?;
            match auth {
                ForgeAuth::Token { token_path } => check_readable(&token_path)?,
                ForgeAuth::App {
                    app_private_key_path,
                    ..
                } => check_readable(&app_private_key_path)?,
            }
        }
        for cache in &self.binary_caches {
            check_readable(&cache.signing_key_path)?;
        }
        for repo in &self.repos {
            if let Some(key) = &repo.clone.ssh_key_path {
                check_readable(key)?;
            }
        }
        if let Some(b) = &self.builder_enrollment {
            check_readable(&b.token_path)?;
        }
        Ok(())
    }
}

fn check_readable(secret: &SecretFile) -> Result<(), ValidationError> {
    let path = secret.path();
    std::fs::metadata(path).map_err(|e| ValidationError::SecretUnreadable {
        path: path.to_path_buf(),
        error: e.to_string(),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::Config;

    fn parse(s: &str) -> Config {
        serde_yaml::from_str(s).expect("parse failed")
    }

    #[test]
    fn references_ok() {
        let c = parse(
            r#"
external_url: https://m.example.com
forges:
  fg:
    kind: forgejo
    api_url: https://forge.example.com/api/v1
    token_path: /tmp/tok
    repos:
      a/b: {}
"#,
        );
        c.validate_references().unwrap();
    }

    #[test]
    fn ssh_without_key_rejected() {
        let c = parse(
            r#"
external_url: https://m.example.com
forges:
  fg:
    kind: forgejo
    api_url: https://forge.example.com/api/v1
    token_path: /tmp/tok
    repos:
      a/b:
        clone:
          method: ssh
"#,
        );
        let err = c.validate_references().unwrap_err();
        assert!(matches!(err, crate::ValidationError::SshWithoutKey { .. }));
    }

    #[test]
    fn validate_secrets_exist_finds_missing_file() {
        let c = parse(
            r#"
external_url: https://m.example.com
forges:
  fg:
    kind: forgejo
    api_url: https://forge.example.com/api/v1
    token_path: /tmp/argunix-definitely-not-a-real-path-zzz
"#,
        );
        let err = c.validate_secrets_exist().unwrap_err();
        assert!(matches!(
            err,
            crate::ValidationError::SecretUnreadable { .. }
        ));
    }

    #[test]
    fn builder_enrollment_absent_by_default() {
        let c = parse(
            r#"
external_url: https://m.example.com
forges:
  fg:
    kind: forgejo
    api_url: https://forge.example.com/api/v1
    token_path: /tmp/tok
"#,
        );
        assert!(c.builder_enrollment.is_none());
    }

    #[test]
    fn builder_enrollment_default_listen() {
        let c = parse(
            r#"
external_url: https://m.example.com
builder_enrollment:
  token_path: /tmp/argunix-builder-token
forges:
  fg:
    kind: forgejo
    api_url: https://forge.example.com/api/v1
    token_path: /tmp/tok
"#,
        );
        let b = c.builder_enrollment.as_ref().unwrap();
        assert_eq!(b.listen, "0.0.0.0:2222");
    }

    #[test]
    fn builder_enrollment_listen_override() {
        let c = parse(
            r#"
external_url: https://m.example.com
builder_enrollment:
  token_path: /tmp/argunix-builder-token
  listen: 127.0.0.1:9999
forges:
  fg:
    kind: forgejo
    api_url: https://forge.example.com/api/v1
    token_path: /tmp/tok
"#,
        );
        let b = c.builder_enrollment.as_ref().unwrap();
        assert_eq!(b.listen, "127.0.0.1:9999");
    }

    #[test]
    fn builder_enrollment_invalid_listen_rejected() {
        let c = parse(
            r#"
external_url: https://m.example.com
builder_enrollment:
  token_path: /tmp/argunix-builder-token
  listen: not-a-socket-addr
forges:
  fg:
    kind: forgejo
    api_url: https://forge.example.com/api/v1
    token_path: /tmp/tok
"#,
        );
        let err = c.validate_references().unwrap_err();
        assert!(matches!(
            err,
            crate::ValidationError::BuilderListenInvalid { .. }
        ));
    }

    #[test]
    fn builder_enrollment_missing_token_path_rejected_at_parse_time() {
        let s = r#"
external_url: https://m.example.com
builder_enrollment: {}
forges:
  fg:
    kind: forgejo
    api_url: https://forge.example.com/api/v1
    token_path: /tmp/tok
"#;
        let err = serde_yaml::from_str::<Config>(s).unwrap_err();
        assert!(err.to_string().contains("token_path"));
    }

    #[test]
    fn builder_enrollment_unreadable_token_caught_by_secrets_validation() {
        let dir = tempfile::tempdir().unwrap();
        let tok = dir.path().join("forge-tok");
        std::fs::write(&tok, "x").unwrap();
        let yaml = format!(
            r#"
external_url: https://m.example.com
builder_enrollment:
  token_path: /tmp/argunix-builder-token-does-not-exist-zzz
forges:
  fg:
    kind: forgejo
    api_url: https://forge.example.com/api/v1
    token_path: {tok}
"#,
            tok = tok.display(),
        );
        let c: Config = serde_yaml::from_str(&yaml).unwrap();
        let err = c.validate_secrets_exist().unwrap_err();
        assert!(matches!(
            err,
            crate::ValidationError::SecretUnreadable { .. }
        ));
    }

    #[test]
    fn validate_secrets_exist_passes_when_files_present() {
        let dir = tempfile::tempdir().unwrap();
        let tok = dir.path().join("tok");
        std::fs::write(&tok, "x").unwrap();
        let yaml = format!(
            r#"
external_url: https://m.example.com
forges:
  fg:
    kind: forgejo
    api_url: https://forge.example.com/api/v1
    token_path: {tok}
"#,
            tok = tok.display(),
        );
        let c: Config = serde_yaml::from_str(&yaml).unwrap();
        c.validate_secrets_exist().unwrap();
    }
}
