use crate::SecretFile;
use argunix_domain::{ForgeKind, Slug};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Top-level argunix configuration.
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
    pub web: WebConfig,
    pub forges: BTreeMap<String, ForgeConfig>,
    pub binary_caches: Vec<BinaryCache>,
    pub repos: Vec<Repo>,
    /// Dynamic builder pool listener (M13). Absent ⇒ argunix falls back to
    /// the host's existing `nix.buildMachines`. Present ⇒ argunix runs an
    /// embedded SSH server that accepts incoming builder enrollments and
    /// reverse-tunnel registrations on `listen`.
    pub builder_enrollment: Option<BuilderEnrollment>,
}

/// Top-level YAML block that turns on the dynamic builder pool.
///
/// One block, set once, never edited per-builder — see `design/builders.md`.
/// Operators rotate the token by replacing the file on disk and triggering
/// `argunixctl reload`; existing builders keep their pubkey-based connections,
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

/// Paths used by the read-only HTTP UI.
///
/// `static_dir` is the directory served at `/static/` — Tailwind-compiled
/// CSS, images, fonts, etc. Read at daemon startup; reload swaps the
/// rest of the config but not this path (`ServeDir` is wired at
/// router-construction time).
///
/// HTML templates are baked into the binary by Askama at compile time,
/// so there is intentionally no `template_dir` knob.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebConfig {
    #[serde(default = "default_static_dir")]
    pub static_dir: PathBuf,
}

fn default_static_dir() -> PathBuf {
    PathBuf::from("static")
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            static_dir: default_static_dir(),
        }
    }
}

fn default_listen() -> String {
    "127.0.0.1:8080".to_string()
}

fn default_control_socket() -> std::path::PathBuf {
    std::path::PathBuf::from("/run/argunix/control.sock")
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
    /// coalescing argunix would run the same eval twice. Q99.
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

/// Retention rules. Defaults: keep everything forever (Q11) and tick
/// hourly (Q25 / M10). `interval_minutes` and `max_size_gb` are global
/// only — sizing across repos is the budget operators actually care
/// about, and one ticker fits all. Per-repo override is a separate
/// [`RepoRetention`] carried on each [`Repo`].
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Retention {
    pub max_age_days: Option<u32>,
    pub max_size_gb: Option<u64>,
    #[serde(default = "default_retention_interval_minutes")]
    pub interval_minutes: u32,
}

fn default_retention_interval_minutes() -> u32 {
    60
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            max_age_days: None,
            max_size_gb: None,
            interval_minutes: default_retention_interval_minutes(),
        }
    }
}

/// Per-repo retention override. Only `max_age_days` is overridable;
/// the size budget and tick interval are global.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoRetention {
    pub max_age_days: Option<u32>,
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
///
/// The user provides the *web* URL — the URL they paste from a
/// browser — and we derive the API URL from it per [`ForgeKind`]
/// using the version we pin against. github.com is the only forge
/// where the API lives on a different host (`api.github.com`); GHES,
/// GitLab, and Forgejo all expose their API at a stable suffix on
/// the same host (`/api/v3`, `/api/v4`, `/api/v1`).
#[derive(Debug, Clone)]
pub struct ForgeConfig {
    pub kind: ForgeKind,
    pub web_url: String,
    pub token_path: Option<SecretFile>,
    pub app_id: Option<u64>,
    pub app_private_key_path: Option<SecretFile>,
}

impl ForgeConfig {
    /// Construct the API base URL for this forge from `web_url` +
    /// `kind`. Trailing slashes on `web_url` are tolerated.
    pub fn api_url(&self) -> String {
        let web = self.web_url.trim_end_matches('/');
        match self.kind {
            // github.com SaaS: API on a separate hostname.
            ForgeKind::Github if web == "https://github.com" => {
                "https://api.github.com".to_string()
            }
            ForgeKind::Github if web == "http://github.com" => "http://api.github.com".to_string(),
            // GitHub Enterprise: API on the same host under /api/v3.
            ForgeKind::Github => format!("{web}/api/v3"),
            // GitLab pins to /api/v4.
            ForgeKind::Gitlab => format!("{web}/api/v4"),
            // Forgejo / Gitea pin to /api/v1.
            ForgeKind::Forgejo => format!("{web}/api/v1"),
        }
    }
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
    pub retention: RepoRetention,
}

impl Repo {
    /// Effective `max_age_days` for this repo: the per-repo override
    /// when set, falling back to the global `Retention.max_age_days`.
    /// `None` from both means "no age cap for this repo".
    pub fn effective_max_age_days(&self, global: &Retention) -> Option<u32> {
        self.retention.max_age_days.or(global.max_age_days)
    }
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
//       web_url: https://github.com
//       token_path: /run/credentials/argunix.service/gh-token
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
    #[serde(default)]
    web: WebConfig,
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
    /// User-facing web URL of the forge instance — the URL pasted
    /// from a browser (`https://github.com`, `https://gitlab.example.com`,
    /// `https://codeberg.org`). Used directly for forge org-level
    /// links in the UI; the API URL is derived per `kind` (see
    /// [`ForgeConfig::api_url`]).
    web_url: String,
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
    #[serde(default)]
    retention: RepoRetention,
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
                    retention: wire_repo.retention,
                });
            }
            forges.insert(
                forge_name,
                ForgeConfig {
                    kind: wire_forge.kind,
                    web_url: wire_forge.web_url,
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
            web: wire.web,
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
external_url: https://argunix.example.com

forges:
  github-myorg:
    kind: github
    web_url: https://github.com
    token_path: /tmp/argunix-test/tok
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
    fn retention_defaults_keep_forever_with_hourly_tick() {
        let c = parse(&minimal_yaml());
        assert!(c.retention.max_age_days.is_none());
        assert!(c.retention.max_size_gb.is_none());
        assert_eq!(c.retention.interval_minutes, 60);
        assert!(c.repos[0].retention.max_age_days.is_none());
        assert_eq!(c.repos[0].effective_max_age_days(&c.retention), None);
    }

    #[test]
    fn parses_retention_with_per_repo_override() {
        let s = r#"
external_url: https://argunix.example.com
retention:
  max_age_days: 30
  max_size_gb: 50
  interval_minutes: 15
forges:
  gh:
    kind: github
    web_url: https://github.com
    token_path: /tmp/tok
    repos:
      slow/burner: {}
      fast/churn:
        retention:
          max_age_days: 3
"#;
        let c = parse(s);
        assert_eq!(c.retention.max_age_days, Some(30));
        assert_eq!(c.retention.max_size_gb, Some(50));
        assert_eq!(c.retention.interval_minutes, 15);

        let by_slug: std::collections::BTreeMap<_, _> =
            c.repos.iter().map(|r| (r.slug.as_str(), r)).collect();
        // No override → falls back to global 30.
        assert_eq!(
            by_slug["slow/burner"].effective_max_age_days(&c.retention),
            Some(30)
        );
        // Override wins → 3.
        assert_eq!(
            by_slug["fast/churn"].effective_max_age_days(&c.retention),
            Some(3)
        );
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
external_url: https://argunix.example.com
forges:
  github-myorg:
    kind: github
    web_url: https://github.com
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
external_url: https://argunix.example.com
forges:
  gl:
    kind: gitlab
    web_url: https://gitlab.example.com
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
external_url: https://argunix.example.com
forges:
  gh:
    kind: github
    web_url: https://github.com
    token_path: /tmp/tok
    repos:
      tfc/pprintpp: {}
  cb:
    kind: forgejo
    web_url: https://codeberg.org
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
external_url: https://argunix.example.com
forges:
  github-myorg:
    kind: github
    web_url: https://github.com
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
external_url: https://argunix.example.com
forges:
  github-myorg:
    kind: github
    web_url: https://github.com
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
external_url: https://argunix.example.com
forges:
  github-myorg:
    kind: github
    web_url: https://github.com
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
external_url: https://argunix.example.com
forges:
  github-myorg:
    kind: github
    web_url: https://github.com
    token_path: /tmp/tok
    repos:
      invalid-no-slash: {}
"#;
        let err = serde_yaml::from_str::<Config>(s).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("slug"));
    }

    fn forge(kind: ForgeKind, web_url: &str) -> ForgeConfig {
        ForgeConfig {
            kind,
            web_url: web_url.to_string(),
            token_path: None,
            app_id: None,
            app_private_key_path: None,
        }
    }

    #[test]
    fn api_url_github_com_uses_api_subdomain() {
        assert_eq!(
            forge(ForgeKind::Github, "https://github.com").api_url(),
            "https://api.github.com",
        );
        // Trailing slash tolerated.
        assert_eq!(
            forge(ForgeKind::Github, "https://github.com/").api_url(),
            "https://api.github.com",
        );
    }

    #[test]
    fn api_url_github_enterprise_appends_v3() {
        assert_eq!(
            forge(ForgeKind::Github, "https://ghe.example.com").api_url(),
            "https://ghe.example.com/api/v3",
        );
    }

    #[test]
    fn api_url_gitlab_appends_v4() {
        assert_eq!(
            forge(ForgeKind::Gitlab, "https://gitlab.example.com").api_url(),
            "https://gitlab.example.com/api/v4",
        );
        assert_eq!(
            forge(ForgeKind::Gitlab, "https://gitlab.com").api_url(),
            "https://gitlab.com/api/v4",
        );
    }

    #[test]
    fn api_url_forgejo_appends_v1() {
        assert_eq!(
            forge(ForgeKind::Forgejo, "https://codeberg.org").api_url(),
            "https://codeberg.org/api/v1",
        );
    }
}
