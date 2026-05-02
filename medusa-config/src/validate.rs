use crate::SecretFile;
use crate::schema::{Config, ForgeAuth, ForgeAuthShapeError};
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("repo `{repo}` references unknown forge `{forge}`")]
    UnknownForge { repo: String, forge: String },
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
}

impl Config {
    /// Cross-reference checks that don't touch the filesystem.
    pub fn validate_references(&self) -> Result<(), ValidationError> {
        for (name, forge) in &self.forges {
            forge.auth().map_err(|e| ValidationError::ForgeAuth {
                forge: name.clone(),
                error: e,
            })?;
        }
        for repo in &self.repos {
            if !self.forges.contains_key(&repo.forge) {
                return Err(ValidationError::UnknownForge {
                    repo: repo.slug.as_str().to_string(),
                    forge: repo.forge.clone(),
                });
            }
            if matches!(repo.clone.method, crate::CloneMethod::Ssh)
                && repo.clone.ssh_key_path.is_none()
            {
                return Err(ValidationError::SshWithoutKey {
                    repo: repo.slug.as_str().to_string(),
                });
            }
        }
        Ok(())
    }

    /// Verify that every secret file is present and readable. Used at daemon
    /// startup; not in tests, since the secrets typically don't exist there.
    pub fn validate_secrets_exist(&self) -> Result<(), ValidationError> {
        for (name, forge) in &self.forges {
            check_readable(&forge.webhook_secret_path)?;
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
    webhook_secret_path: /tmp/wh
    token_path: /tmp/tok
repos:
  - slug: a/b
    forge: fg
"#,
        );
        c.validate_references().unwrap();
    }

    #[test]
    fn unknown_forge_rejected() {
        let c = parse(
            r#"
external_url: https://m.example.com
forges:
  fg:
    kind: forgejo
    api_url: https://forge.example.com/api/v1
    webhook_secret_path: /tmp/wh
    token_path: /tmp/tok
repos:
  - slug: a/b
    forge: nonexistent
"#,
        );
        let err = c.validate_references().unwrap_err();
        assert!(matches!(err, crate::ValidationError::UnknownForge { .. }));
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
    webhook_secret_path: /tmp/wh
    token_path: /tmp/tok
repos:
  - slug: a/b
    forge: fg
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
    webhook_secret_path: /tmp/medusa-definitely-not-a-real-path-zzz
    token_path: /tmp/medusa-definitely-not-a-real-path-zzz
"#,
        );
        let err = c.validate_secrets_exist().unwrap_err();
        assert!(matches!(
            err,
            crate::ValidationError::SecretUnreadable { .. }
        ));
    }

    #[test]
    fn validate_secrets_exist_passes_when_files_present() {
        let dir = tempfile::tempdir().unwrap();
        let wh = dir.path().join("wh");
        let tok = dir.path().join("tok");
        std::fs::write(&wh, "x").unwrap();
        std::fs::write(&tok, "x").unwrap();
        let yaml = format!(
            r#"
external_url: https://m.example.com
forges:
  fg:
    kind: forgejo
    api_url: https://forge.example.com/api/v1
    webhook_secret_path: {wh}
    token_path: {tok}
"#,
            wh = wh.display(),
            tok = tok.display(),
        );
        let c: Config = serde_yaml::from_str(&yaml).unwrap();
        c.validate_secrets_exist().unwrap();
    }
}
