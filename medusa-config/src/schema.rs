use crate::SecretFile;
use medusa_domain::{ForgeKind, Slug};
use serde::Deserialize;
use std::collections::BTreeMap;

/// Top-level medusa configuration.
///
/// Runtime model: a flat list of repos each carrying a `forge` string
/// pointing at an entry in `forges`. The on-disk YAML uses a richer
/// shape (repos nested inside their forge entry) — see [`WireConfig`]
/// and [`Config`]'s `try_from` — but every callsite still sees the
/// flat shape it was originally written against.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, try_from = "WireConfig")]
pub struct Config {
    pub external_url: String,
    pub listen: String,
    pub control_socket: std::path::PathBuf,
    pub dry_run: bool,
    pub schedule: Schedule,
    pub retention: Retention,
    pub eval: EvalDefaults,
    pub forges: BTreeMap<String, ForgeConfig>,
    pub binary_caches: Vec<BinaryCache>,
    pub repos: Vec<Repo>,
    /// Dynamic builder pool listener (M13). Absent ⇒ medusa falls back to
    /// the host's existing `nix.buildMachines`. Present ⇒ medusa runs an
    /// embedded SSH server that accepts incoming builder enrollments and
    /// reverse-tunnel registrations on `listen`.
    pub builder_enrollment: Option<BuilderEnrollment>,
}

/// Top-level YAML block that turns on the dynamic builder pool.
///
/// One block, set once, never edited per-builder — see `design/builders.md`.
/// Operators rotate the token by replacing the file on disk and triggering
/// `medusactl reload`; existing builders keep their pubkey-based connections,
/// only fresh enrollments need the new token.
#[derive(Debug, Clone)]
pub struct BuilderEnrollment {
    /// File containing the shared enrollment token. Builders dialing in for
    /// the first time present this token via SSH password auth; subsequent
    /// connects use pubkey auth against the `builders` sqlite row written
    /// at first contact (TOFU).
    pub token_path: SecretFile,
    /// `host:port` for the embedded russh server. Default `0.0.0.0:2222`.
    /// Distinct from the webhook `listen` port — the protocol is SSH.
    pub listen: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireBuilderEnrollment {
    token_path: SecretFile,
    #[serde(default = "default_builder_listen")]
    listen: String,
}

fn default_builder_listen() -> String {
    "0.0.0.0:2222".to_string()
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

/// Runtime model of a configured forge. Built from [`WireForgeConfig`]
/// during deserialization; not serde-derived itself because the YAML
/// shape has an extra `repos` map that doesn't belong on the runtime
/// type.
#[derive(Debug, Clone)]
pub struct ForgeConfig {
    pub kind: ForgeKind,
    pub api_url: String,
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

/// Runtime model of a configured repo. Built from [`WireRepo`] +
/// the parent forge name during deserialization.
#[derive(Debug, Clone)]
pub struct Repo {
    pub slug: Slug,
    /// Name of an entry in [`Config::forges`]. Populated from the
    /// parent map key when reading the YAML — never typed by hand.
    pub forge: String,
    pub watched_branches: Vec<String>,
    pub build_prs: bool,
    pub pr_allowlist: Vec<String>,
    pub clone: CloneConfig,
    pub eval: EvalOverrides,
    pub collapsed_check_threshold: Option<u32>,
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

// ============================================================
// Wire shape — what the YAML actually looks like.
// ============================================================
//
// Repos live nested under their forge:
//
//   forges:
//     gh:
//       kind: github
//       api_url: https://api.github.com
//       token_path: /run/credentials/medusa.service/gh-token
//       repos:
//         myorg/myrepo:
//           watched_branches: [main]
//
// Two upsides over the previous flat-list shape:
// - Repo→forge association is structural, not string-referential.
//   The `UnknownForge` validation case becomes impossible.
// - Same slug across forges is naturally distinguished by parent map
//   key, no special handling needed in the YAML.
//
// At deserialization time we flatten back to `Config { forges, repos }`
// so every callsite reading `config.repos` keeps working.

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireConfig {
    external_url: String,
    #[serde(default = "default_listen")]
    listen: String,
    #[serde(default = "default_control_socket")]
    control_socket: std::path::PathBuf,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    schedule: Schedule,
    #[serde(default)]
    retention: Retention,
    #[serde(default)]
    eval: EvalDefaults,
    forges: BTreeMap<String, WireForgeConfig>,
    #[serde(default)]
    binary_caches: Vec<BinaryCache>,
    #[serde(default)]
    builder_enrollment: Option<WireBuilderEnrollment>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireForgeConfig {
    kind: ForgeKind,
    api_url: String,
    token_path: Option<SecretFile>,
    app_id: Option<u64>,
    app_private_key_path: Option<SecretFile>,
    #[serde(default)]
    repos: BTreeMap<String, WireRepo>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRepo {
    #[serde(default = "default_watched_branches")]
    watched_branches: Vec<String>,
    #[serde(default = "default_build_prs")]
    build_prs: bool,
    #[serde(default)]
    pr_allowlist: Vec<String>,
    #[serde(default)]
    clone: CloneConfig,
    #[serde(default)]
    eval: EvalOverrides,
    collapsed_check_threshold: Option<u32>,
    #[serde(default = "default_weight")]
    weight: u32,
}

impl TryFrom<WireConfig> for Config {
    type Error = String;

    fn try_from(wire: WireConfig) -> Result<Self, Self::Error> {
        let mut forges = BTreeMap::new();
        let mut repos = Vec::new();

        for (forge_name, wire_forge) in wire.forges {
            for (slug_str, wire_repo) in wire_forge.repos {
                let slug = Slug::new(slug_str.clone()).map_err(|e| {
                    format!("invalid slug `{slug_str}` under forge `{forge_name}`: {e}",)
                })?;
                repos.push(Repo {
                    slug,
                    forge: forge_name.clone(),
                    watched_branches: wire_repo.watched_branches,
                    build_prs: wire_repo.build_prs,
                    pr_allowlist: wire_repo.pr_allowlist,
                    clone: wire_repo.clone,
                    eval: wire_repo.eval,
                    collapsed_check_threshold: wire_repo.collapsed_check_threshold,
                    weight: wire_repo.weight,
                });
            }
            forges.insert(
                forge_name,
                ForgeConfig {
                    kind: wire_forge.kind,
                    api_url: wire_forge.api_url,
                    token_path: wire_forge.token_path,
                    app_id: wire_forge.app_id,
                    app_private_key_path: wire_forge.app_private_key_path,
                },
            );
        }

        Ok(Config {
            external_url: wire.external_url,
            listen: wire.listen,
            control_socket: wire.control_socket,
            dry_run: wire.dry_run,
            schedule: wire.schedule,
            retention: wire.retention,
            eval: wire.eval,
            forges,
            binary_caches: wire.binary_caches,
            repos,
            builder_enrollment: wire.builder_enrollment.map(|w| BuilderEnrollment {
                token_path: w.token_path,
                listen: w.listen,
            }),
        })
    }
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
      myorg/myrepo: {}
"#
        .to_string()
    }

    #[test]
    fn defaults_for_minimal() {
        let c = parse(&minimal_yaml());
        assert_eq!(c.listen, "127.0.0.1:8080");
        assert_eq!(c.schedule.collapsed_check_threshold, 100);
        assert!(!c.dry_run);
        assert_eq!(c.repos.len(), 1);
        assert_eq!(c.repos[0].forge, "github-myorg");
        assert_eq!(c.repos[0].slug.as_str(), "myorg/myrepo");
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
      myorg/myrepo:
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
      myorg/marketing/marketing-project-1: {}
"#;
        let c = parse(s);
        assert_eq!(
            c.repos[0].slug.as_str(),
            "myorg/marketing/marketing-project-1"
        );
        assert_eq!(c.repos[0].forge, "gl");
    }

    #[test]
    fn same_slug_on_two_forges_disambiguated_by_parent() {
        // Same `tfc/pprintpp` slug present on both `gh` and `cb`. The
        // new shape disambiguates by the parent map key — both entries
        // arrive in `Config.repos` with their respective `forge`
        // fields populated.
        let s = r#"
external_url: https://medusa.example.com
forges:
  gh:
    kind: github
    api_url: https://api.github.com
    token_path: /tmp/tok
    repos:
      tfc/pprintpp: {}
  cb:
    kind: forgejo
    api_url: https://codeberg.org/api/v1
    token_path: /tmp/tok2
    repos:
      tfc/pprintpp: {}
"#;
        let c = parse(s);
        assert_eq!(c.repos.len(), 2);
        let forges: Vec<&str> = c.repos.iter().map(|r| r.forge.as_str()).collect();
        assert!(forges.contains(&"gh"));
        assert!(forges.contains(&"cb"));
        // Both with the same slug.
        assert!(c.repos.iter().all(|r| r.slug.as_str() == "tfc/pprintpp"));
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
      invalid-no-slash: {}
"#;
        let err = serde_yaml::from_str::<Config>(s).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("slug"));
    }
}
