use crate::SecretFile;
use medusa_domain::{ForgeKind, Slug};
use serde::Deserialize;
use std::collections::BTreeMap;

/// Top-level medusa configuration. See `design/questions-answers.md` Q83.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub external_url: String,
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_control_socket")]
    pub control_socket: std::path::PathBuf,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub schedule: Schedule,
    #[serde(default)]
    pub retention: Retention,
    #[serde(default)]
    pub eval: EvalDefaults,
    pub forges: BTreeMap<String, ForgeConfig>,
    #[serde(default)]
    pub binary_caches: Vec<BinaryCache>,
    #[serde(default)]
    pub repos: Vec<Repo>,
}

fn default_listen() -> String {
    "127.0.0.1:8080".to_string()
}

fn default_control_socket() -> std::path::PathBuf {
    std::path::PathBuf::from("/run/medusa/control.sock")
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    /// Number of jobs above which we collapse per-job forge checks into a
    /// single check with a markdown summary. See Q19.
    #[serde(default = "default_collapsed_threshold")]
    pub collapsed_check_threshold: u32,
    /// Window during which duplicate `(repo_id, sha)` webhook events are
    /// dropped. GitHub fires a `push` and a `pull_request.synchronize`
    /// for the same SHA milliseconds apart on every PR push; without
    /// coalescing medusa would run the same eval twice. Q99.
    #[serde(default = "default_webhook_coalesce_seconds")]
    pub webhook_coalesce_seconds: u32,
}

fn default_collapsed_threshold() -> u32 {
    100
}

fn default_webhook_coalesce_seconds() -> u32 {
    5
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            collapsed_check_threshold: default_collapsed_threshold(),
            webhook_coalesce_seconds: default_webhook_coalesce_seconds(),
        }
    }
}

/// Retention rules. Defaults: keep everything forever (Q11).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Retention {
    pub max_age_days: Option<u32>,
    pub max_size_gb: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalDefaults {
    #[serde(default = "default_memory_max")]
    pub memory_max: String,
    #[serde(default = "default_eval_timeout_seconds")]
    pub timeout_seconds: u32,
    #[serde(default = "default_non_flake_max_depth")]
    pub non_flake_max_depth: u32,
    #[serde(default = "default_allow_network")]
    pub allow_network: bool,
}

fn default_memory_max() -> String {
    "4G".to_string()
}
fn default_eval_timeout_seconds() -> u32 {
    600
}
fn default_non_flake_max_depth() -> u32 {
    5
}
fn default_allow_network() -> bool {
    true
}

impl Default for EvalDefaults {
    fn default() -> Self {
        Self {
            memory_max: default_memory_max(),
            timeout_seconds: default_eval_timeout_seconds(),
            non_flake_max_depth: default_non_flake_max_depth(),
            allow_network: default_allow_network(),
        }
    }
}

/// Per-repo eval-section overrides. Every field is optional; absent means
/// "use the global default".
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalOverrides {
    pub memory_max: Option<String>,
    pub timeout_seconds: Option<u32>,
    pub non_flake_max_depth: Option<u32>,
    pub allow_network: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgeConfig {
    pub kind: ForgeKind,
    pub api_url: String,

    // Auth: either set `token_path` (PAT-style), or both `app_id` and
    // `app_private_key_path` (GitHub-App-style). [`ForgeConfig::auth`]
    // turns these into a [`ForgeAuth`] and rejects mixed/empty shapes.
    // Kept as optional flat fields because serde's `flatten` does not
    // compose with `deny_unknown_fields`.
    //
    // No `webhook_secret_path`: medusa generates and owns the webhook
    // secret per repo, stored in sqlite. The auto-install pass at
    // startup pushes it to the forge alongside the hook itself.
    pub token_path: Option<SecretFile>,
    pub app_id: Option<u64>,
    pub app_private_key_path: Option<SecretFile>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ForgeAuthShapeError {
    #[error("forge has no auth configured: set token_path, or app_id + app_private_key_path")]
    Missing,
    #[error(
        "forge has both PAT and App auth configured: set either token_path, \
         or both app_id and app_private_key_path, not both"
    )]
    Mixed,
    #[error("App auth requires both app_id and app_private_key_path")]
    PartialApp,
}

#[derive(Debug, Clone)]
pub enum ForgeAuth {
    Token {
        token_path: SecretFile,
    },
    App {
        app_id: u64,
        app_private_key_path: SecretFile,
    },
}

impl ForgeConfig {
    pub fn auth(&self) -> Result<ForgeAuth, ForgeAuthShapeError> {
        match (
            self.token_path.clone(),
            self.app_id,
            self.app_private_key_path.clone(),
        ) {
            (Some(token_path), None, None) => Ok(ForgeAuth::Token { token_path }),
            (None, Some(app_id), Some(app_private_key_path)) => Ok(ForgeAuth::App {
                app_id,
                app_private_key_path,
            }),
            (None, None, None) => Err(ForgeAuthShapeError::Missing),
            (None, Some(_), None) | (None, None, Some(_)) => Err(ForgeAuthShapeError::PartialApp),
            _ => Err(ForgeAuthShapeError::Mixed),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BinaryCache {
    pub url: String,
    pub signing_key_path: SecretFile,
    #[serde(default)]
    pub push: bool,
    #[serde(default)]
    pub substitute: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Repo {
    pub slug: Slug,
    /// Name of an entry in [`Config::forges`].
    pub forge: String,
    #[serde(default = "default_watched_branches")]
    pub watched_branches: Vec<String>,
    #[serde(default = "default_build_prs")]
    pub build_prs: bool,
    #[serde(default)]
    pub pr_allowlist: Vec<String>,
    #[serde(default)]
    pub clone: CloneConfig,
    #[serde(default)]
    pub eval: EvalOverrides,
    pub collapsed_check_threshold: Option<u32>,
    #[serde(default = "default_weight")]
    pub weight: u32,
}

fn default_watched_branches() -> Vec<String> {
    vec!["main".to_string()]
}

fn default_build_prs() -> bool {
    true
}

fn default_weight() -> u32 {
    1
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloneConfig {
    #[serde(default)]
    pub method: CloneMethod,
    pub ssh_key_path: Option<SecretFile>,
    #[serde(default)]
    pub persistent: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CloneMethod {
    #[default]
    Https,
    Ssh,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Config {
        serde_yaml::from_str(s).expect("parse failed")
    }

    fn minimal_yaml() -> String {
        r#"
external_url: https://medusa.example.com

forges:
  github-myorg:
    kind: github
    api_url: https://api.github.com
    token_path: /tmp/medusa-test/tok

repos:
  - slug: myorg/myrepo
    forge: github-myorg
"#
        .to_string()
    }

    #[test]
    fn defaults_for_minimal() {
        let c = parse(&minimal_yaml());
        assert_eq!(c.listen, "127.0.0.1:8080");
        assert_eq!(c.schedule.collapsed_check_threshold, 100);
        assert!(!c.dry_run);
        assert_eq!(c.repos[0].watched_branches, vec!["main"]);
        assert!(c.repos[0].build_prs);
        assert_eq!(c.repos[0].weight, 1);
        assert_eq!(c.repos[0].clone.method, CloneMethod::Https);
        assert!(matches!(
            c.forges["github-myorg"].auth().unwrap(),
            ForgeAuth::Token { .. }
        ));
    }

    #[test]
    fn rejects_unknown_top_level_key() {
        let mut s = minimal_yaml();
        s.push_str("\nflavour_of_the_day: vanilla\n");
        let err = serde_yaml::from_str::<Config>(&s).unwrap_err();
        assert!(err.to_string().contains("flavour_of_the_day"));
    }

    #[test]
    fn rejects_unknown_repo_key() {
        let s = r#"
external_url: https://medusa.example.com
forges:
  github-myorg:
    kind: github
    api_url: https://api.github.com
    token_path: /tmp/tok
repos:
  - slug: myorg/myrepo
    forge: github-myorg
    extra_field: oops
"#;
        let err = serde_yaml::from_str::<Config>(s).unwrap_err();
        assert!(err.to_string().contains("extra_field"));
    }

    #[test]
    fn parses_subgroup_slug() {
        let s = r#"
external_url: https://medusa.example.com
forges:
  gl:
    kind: gitlab
    api_url: https://gitlab.example.com/api/v4
    token_path: /tmp/tok
repos:
  - slug: myorg/marketing/marketing-project-1
    forge: gl
"#;
        let c = parse(s);
        assert_eq!(
            c.repos[0].slug.as_str(),
            "myorg/marketing/marketing-project-1"
        );
    }

    #[test]
    fn parses_app_auth() {
        let s = r#"
external_url: https://medusa.example.com
forges:
  github-myorg:
    kind: github
    api_url: https://api.github.com
    app_id: 12345
    app_private_key_path: /tmp/key.pem
"#;
        let c = parse(s);
        let auth = c.forges["github-myorg"].auth().unwrap();
        let ForgeAuth::App { app_id, .. } = auth else {
            panic!("expected app auth, got {auth:?}");
        };
        assert_eq!(app_id, 12345);
    }

    #[test]
    fn forge_with_no_auth_rejected() {
        let s = r#"
external_url: https://medusa.example.com
forges:
  github-myorg:
    kind: github
    api_url: https://api.github.com
"#;
        let c = parse(s);
        assert_eq!(
            c.forges["github-myorg"].auth().unwrap_err(),
            ForgeAuthShapeError::Missing
        );
    }

    #[test]
    fn forge_with_mixed_auth_rejected() {
        let s = r#"
external_url: https://medusa.example.com
forges:
  github-myorg:
    kind: github
    api_url: https://api.github.com
    token_path: /tmp/tok
    app_id: 12345
    app_private_key_path: /tmp/key.pem
"#;
        let c = parse(s);
        assert_eq!(
            c.forges["github-myorg"].auth().unwrap_err(),
            ForgeAuthShapeError::Mixed
        );
    }

    #[test]
    fn rejects_invalid_slug() {
        let s = r#"
external_url: https://medusa.example.com
forges:
  github-myorg:
    kind: github
    api_url: https://api.github.com
    token_path: /tmp/tok
repos:
  - slug: invalid-no-slash
    forge: github-myorg
"#;
        let err = serde_yaml::from_str::<Config>(s).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("slug"));
    }
}
