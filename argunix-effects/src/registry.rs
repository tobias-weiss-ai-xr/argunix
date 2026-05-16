//! The `registry-push` effect: push a built `dockerTools` image to an
//! *external* docker registry the operator already trusts and manages.
//!
//! argunix also ships its own read-only registry (`argunix-registry`)
//! — but most teams already run a registry they trust and won't
//! migrate. Pushing the image out to *their* registry instead means
//! argunix is additive infrastructure, not a replacement, and argunix
//! carries no image-lifecycle responsibility.
//!
//! Mechanics: `skopeo copy <transport>:<archive> docker://<ref>`, once
//! per tag. The transport follows the job's `meta.image-format` — a
//! `docker` job is a `docker-archive:` tarball, an `oci` job is an
//! `oci-archive:` layout — so the effect never has to sniff the
//! archive. The image is tagged with the branch name (when the eval
//! ran on a `refs/heads/*` ref), with `latest` when that branch is the
//! repo's default branch, and always with an immutable `sha-<short>`
//! tag. Registry credentials come from a file holding a
//! single `user:password` line, read at push time — never at config
//! time, and never logged.
//!
//! Multi-arch: an `oci-archive` may carry a manifest list, so its push
//! adds `--multi-arch all` to copy every platform rather than only the
//! runner's. A `docker-archive` cannot carry a list — `docker save`
//! has no multi-arch — so a `docker` job is always single-platform.
//! When several `docker` jobs build the same attribute they still race
//! on the branch tag (last write wins); assembling one cross-system
//! index is future work, tracked against the same `Effect` seam.

use crate::{Effect, EffectOutcome, ImageFormat, OutputContext, Severity, image_segment};
use async_trait::async_trait;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

/// Per-tag `skopeo copy` timeout. Layer uploads dominate; a registry
/// that cannot ingest a `dockerTools` image's layers in five minutes
/// is effectively down.
const PER_TAG_TIMEOUT: Duration = Duration::from_secs(300);

/// A configured external docker registry argunix should push images
/// to. Built by the worker from `argunix_config::Registry` plus the
/// repo it applies to.
#[derive(Debug, Clone)]
pub struct RegistryPush {
    /// Configured target name (the key under `registries:` in the
    /// YAML). Surfaced verbatim in `effect_runs.target`.
    pub target: String,
    /// Registry host[:port], no scheme — `ghcr.io`, `127.0.0.1:5000`.
    pub registry_url: String,
    /// Namespace / project the image lands under on the registry. May
    /// contain a `{slug}` placeholder, resolved per-build to the repo's
    /// slug by [`RegistryPush::resolve_namespace`] — that is what lets
    /// one catalog entry serve many repos.
    pub namespace: String,
    /// Path to a file containing one `user:password` line. `None` for
    /// an anonymous-push registry. Read at push time.
    pub auth_path: Option<PathBuf>,
    /// Skip TLS verification on push. Needed for a plain-HTTP registry
    /// (a local `registry:2`, an internal mirror without TLS).
    pub insecure: bool,
}

impl RegistryPush {
    /// Resolve the configured `namespace` against the repo being built:
    /// every `{slug}` occurrence is replaced with the repo's slug (the
    /// forge-side `owner/repo` path). A namespace with no placeholder —
    /// a literal `myorg` — comes back unchanged.
    ///
    /// `{slug}` is what lets a single `registries:` catalog entry serve
    /// many repos. On GitLab the registry path *is* the project path,
    /// and the argunix repo slug already equals it, so `{slug}` pushes
    /// each repo under its own project; on ghcr it yields the
    /// conventional `<owner>/<repo>/<image>` layout.
    fn resolve_namespace(&self, repo_slug: &str) -> String {
        self.namespace.replace("{slug}", repo_slug)
    }

    /// `docker://<url>/<namespace>/<image>` — the dest minus the tag.
    /// `namespace` is the already-`{slug}`-resolved value.
    fn dest_base(&self, namespace: &str, image: &str) -> String {
        format!(
            "docker://{}/{}/{}",
            self.registry_url.trim_end_matches('/'),
            namespace.trim_matches('/'),
            image,
        )
    }
}

#[async_trait]
impl Effect for RegistryPush {
    fn kind(&self) -> &'static str {
        "registry-push"
    }

    fn target(&self) -> &str {
        &self.target
    }

    fn severity(&self) -> Severity {
        // A registry that rejects a push is a degraded distribution
        // path, not a broken build — the artifact is already realised
        // and (typically) in a cache. Same policy as binary-cache push.
        Severity::Advisory
    }

    async fn run(&self, ctx: &OutputContext<'_>) -> EffectOutcome {
        let Some(fmt) = ctx.image_format else {
            return EffectOutcome::skipped("not a container image");
        };
        let Some(archive) = ctx.primary_output() else {
            return EffectOutcome::failure("build produced no output path to push");
        };

        let image = image_segment(ctx.attr_path);
        let namespace = self.resolve_namespace(ctx.repo_slug);
        let base = self.dest_base(&namespace, &image);

        let tags = push_tags(ctx);

        // Credentials are read once, here — not at config load — and
        // never logged. A read failure fails the effect rather than
        // silently pushing anonymously.
        let creds = match &self.auth_path {
            Some(path) => match tokio::fs::read_to_string(path).await {
                Ok(s) => Some(s.trim().to_string()),
                Err(e) => {
                    return EffectOutcome::failure(format!(
                        "reading registry credentials {}: {e}",
                        path.display(),
                    ));
                }
            },
            None => None,
        };

        let mut pushed: Vec<String> = Vec::new();
        for tag in &tags {
            let dest = format!("{base}:{tag}");
            if let Err(detail) = self
                .skopeo_copy(fmt, archive, &dest, creds.as_deref())
                .await
            {
                return EffectOutcome::failure(format!(
                    "pushing {} to {}: {detail}",
                    redact_dest(&dest),
                    self.target,
                ));
            }
            pushed.push(redact_dest(&dest));
        }

        EffectOutcome::success(format!("pushed {} to {}", pushed.join(", "), self.target,))
    }
}

impl RegistryPush {
    /// Run one `skopeo copy <transport>:<archive> <dest>`. The
    /// transport (`docker-archive:` / `oci-archive:`) is chosen from
    /// `fmt`; an OCI archive additionally gets `--multi-arch all` so a
    /// manifest list is copied whole rather than just the runner's
    /// platform. `Ok` on a zero exit, `Err(detail)` carries the failure
    /// reason (timeout, spawn error, or skopeo's own stderr).
    async fn skopeo_copy(
        &self,
        fmt: ImageFormat,
        archive: &str,
        dest: &str,
        creds: Option<&str>,
    ) -> Result<(), String> {
        let mut cmd = Command::new("skopeo");
        // `--insecure-policy` skips containers/image trust-policy
        // evaluation — argunix is copying its own freshly-built nix
        // output, there is no upstream signer to check. Mirrors
        // `argunix-registry::convert`.
        cmd.arg("--insecure-policy")
            .arg("copy")
            .arg(format!("{}:{archive}", fmt.skopeo_transport()))
            .arg(dest);
        // An OCI archive may be a manifest list — copy every platform.
        // Harmless on a single-arch OCI archive; a docker-archive can't
        // carry a list at all, so the flag stays OCI-only.
        if fmt == ImageFormat::Oci {
            cmd.arg("--multi-arch").arg("all");
        }
        if self.insecure {
            cmd.arg("--dest-tls-verify=false");
        }
        if let Some(creds) = creds {
            cmd.arg("--dest-creds").arg(creds);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| format!("spawning skopeo: {e}"))?;
        let mut stderr = child.stderr.take().expect("stderr piped");
        let collect = async {
            use tokio::io::AsyncReadExt;
            let mut buf = Vec::new();
            stderr.read_to_end(&mut buf).await?;
            let status = child.wait().await?;
            Ok::<_, std::io::Error>((status, buf))
        };

        let (status, stderr_buf) = match timeout(PER_TAG_TIMEOUT, collect).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(format!("waiting for skopeo: {e}")),
            Err(_) => {
                return Err(format!(
                    "skopeo copy timed out after {}s",
                    PER_TAG_TIMEOUT.as_secs(),
                ));
            }
        };
        if status.success() {
            return Ok(());
        }
        Err(format!(
            "skopeo exited {:?}: {}",
            status.code(),
            String::from_utf8_lossy(&stderr_buf).trim(),
        ))
    }
}

/// The tag set one push covers, in push order:
///
/// * the **branch** name, when the build ran on a `refs/heads/*` ref
///   — a mutable, human-facing tag;
/// * **`latest`**, when that branch is the repo's *default* branch, so
///   `latest` always names the newest mainline image and a PR or
///   feature-branch build never moves it;
/// * an immutable **`sha-<short>`** tag, always.
///
/// Deduplicated, so a branch literally named `latest` or `sha-<short>`
/// does not push the same ref twice.
fn push_tags(ctx: &OutputContext<'_>) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    if let Some(branch) = ctx.branch() {
        tags.push(sanitize_tag(branch));
    }
    if ctx.is_default_branch() {
        let latest = "latest".to_string();
        if !tags.contains(&latest) {
            tags.push(latest);
        }
    }
    let sha_tag = format!("sha-{}", ctx.short_sha());
    if !tags.contains(&sha_tag) {
        tags.push(sha_tag);
    }
    tags
}

/// Coerce an arbitrary branch name into a docker tag. Docker tags
/// allow `[A-Za-z0-9_.-]` and cap at 128 chars; `/` in particular
/// (release branches like `release/1.2`) is illegal, so map every
/// disallowed byte to `-`.
fn sanitize_tag(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    out.truncate(128);
    if out.is_empty() {
        out.push_str("latest");
    }
    out
}

/// `docker://host/ns/img:tag` → `host/ns/img:tag` for log / detail
/// display. The `docker://` scheme is noise and a dest never carries
/// credentials (those go through `--dest-creds`), so this is purely
/// cosmetic.
fn redact_dest(dest: &str) -> String {
    dest.strip_prefix("docker://").unwrap_or(dest).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push() -> RegistryPush {
        RegistryPush {
            target: "ghcr".into(),
            registry_url: "ghcr.io".into(),
            namespace: "myorg".into(),
            auth_path: None,
            insecure: false,
        }
    }

    #[test]
    fn dest_base_joins_cleanly() {
        assert_eq!(
            push().dest_base("myorg", "my-image"),
            "docker://ghcr.io/myorg/my-image",
        );
    }

    #[test]
    fn dest_base_tolerates_stray_slashes() {
        let p = RegistryPush {
            registry_url: "ghcr.io/".into(),
            ..push()
        };
        assert_eq!(p.dest_base("/myorg/", "img"), "docker://ghcr.io/myorg/img");
    }

    #[test]
    fn resolve_namespace_substitutes_slug() {
        let p = RegistryPush {
            namespace: "{slug}".into(),
            ..push()
        };
        assert_eq!(
            p.resolve_namespace("oci-community/images/example"),
            "oci-community/images/example",
        );
    }

    #[test]
    fn resolve_namespace_passes_literal_through() {
        // No placeholder — the configured namespace is used verbatim,
        // ignoring the repo slug.
        assert_eq!(push().resolve_namespace("some/repo"), "myorg");
    }

    #[test]
    fn sanitize_tag_replaces_slashes() {
        assert_eq!(sanitize_tag("release/1.2"), "release-1.2");
        assert_eq!(sanitize_tag("main"), "main");
        assert_eq!(sanitize_tag("feature/x@y"), "feature-x-y");
    }

    #[test]
    fn redact_dest_strips_scheme() {
        assert_eq!(redact_dest("docker://ghcr.io/o/i:main"), "ghcr.io/o/i:main",);
    }

    #[tokio::test]
    async fn skips_non_image_jobs() {
        let ctx = OutputContext {
            forge: "gh",
            repo_slug: "o/r",
            attr_path: "packages.x86_64-linux.hello",
            system: "x86_64-linux",
            git_ref: "refs/heads/main",
            default_branch: None,
            sha: "0123456789abcdef",
            image_format: None,
            output_paths: &["/nix/store/x".to_string()],
        };
        let outcome = push().run(&ctx).await;
        assert_eq!(outcome.status, crate::EffectStatus::Skipped);
    }

    #[tokio::test]
    async fn fails_when_no_output_path() {
        let ctx = OutputContext {
            forge: "gh",
            repo_slug: "o/r",
            attr_path: "packages.x86_64-linux.img",
            system: "x86_64-linux",
            git_ref: "refs/heads/main",
            default_branch: None,
            sha: "0123456789abcdef",
            image_format: Some(ImageFormat::Docker),
            output_paths: &[],
        };
        let outcome = push().run(&ctx).await;
        assert_eq!(outcome.status, crate::EffectStatus::Failure);
        assert!(outcome.detail.contains("no output"));
    }

    #[tokio::test]
    async fn missing_credentials_file_fails_before_skopeo() {
        let ctx = OutputContext {
            forge: "gh",
            repo_slug: "o/r",
            attr_path: "packages.x86_64-linux.img",
            system: "x86_64-linux",
            git_ref: "refs/heads/main",
            default_branch: None,
            sha: "0123456789abcdef",
            image_format: Some(ImageFormat::Docker),
            output_paths: &["/nix/store/does-not-matter".to_string()],
        };
        let p = RegistryPush {
            auth_path: Some(PathBuf::from("/nonexistent/argunix-creds-zzz")),
            ..push()
        };
        let outcome = p.run(&ctx).await;
        assert_eq!(outcome.status, crate::EffectStatus::Failure);
        assert!(
            outcome.detail.contains("credentials"),
            "got: {}",
            outcome.detail,
        );
    }

    #[tokio::test]
    async fn oci_job_passes_the_image_gate() {
        // An `oci` job is an image like a `docker` one: it must clear
        // the `image_format` gate and fail only on the missing output,
        // not be skipped as a non-image.
        let ctx = OutputContext {
            forge: "gh",
            repo_slug: "o/r",
            attr_path: "packages.x86_64-linux.img",
            system: "x86_64-linux",
            git_ref: "refs/heads/main",
            default_branch: None,
            sha: "0123456789abcdef",
            image_format: Some(ImageFormat::Oci),
            output_paths: &[],
        };
        let outcome = push().run(&ctx).await;
        assert_eq!(outcome.status, crate::EffectStatus::Failure);
        assert!(outcome.detail.contains("no output"));
    }

    fn tag_ctx<'a>(git_ref: &'a str, default_branch: Option<&'a str>) -> OutputContext<'a> {
        OutputContext {
            forge: "gh",
            repo_slug: "o/r",
            attr_path: "packages.x86_64-linux.img",
            system: "x86_64-linux",
            git_ref,
            default_branch,
            sha: "0123456789abcdef",
            image_format: Some(ImageFormat::Docker),
            output_paths: &[],
        }
    }

    #[test]
    fn push_tags_default_branch_gets_latest() {
        let tags = push_tags(&tag_ctx("refs/heads/main", Some("main")));
        assert_eq!(tags, vec!["main", "latest", "sha-0123456789ab"]);
    }

    #[test]
    fn push_tags_bare_daemon_push_ref_gets_latest() {
        // The daemon stores a push eval's git_ref as the bare branch
        // name — this is the real production shape, and it must tag
        // the branch + `latest` just like the `refs/heads/` form.
        let tags = push_tags(&tag_ctx("main", Some("main")));
        assert_eq!(tags, vec!["main", "latest", "sha-0123456789ab"]);
    }

    #[test]
    fn push_tags_feature_branch_has_no_latest() {
        let tags = push_tags(&tag_ctx("refs/heads/feature", Some("main")));
        assert_eq!(tags, vec!["feature", "sha-0123456789ab"]);
    }

    #[test]
    fn push_tags_pr_ref_is_sha_only() {
        let tags = push_tags(&tag_ctx("refs/pull/7/head", Some("main")));
        assert_eq!(tags, vec!["sha-0123456789ab"]);
    }

    #[test]
    fn push_tags_no_latest_when_default_branch_unknown() {
        let tags = push_tags(&tag_ctx("refs/heads/main", None));
        assert_eq!(tags, vec!["main", "sha-0123456789ab"]);
    }

    #[test]
    fn push_tags_dedups_branch_named_latest() {
        // A branch literally named `latest` that is also the default
        // branch must not enqueue `latest` twice.
        let tags = push_tags(&tag_ctx("refs/heads/latest", Some("latest")));
        assert_eq!(tags, vec!["latest", "sha-0123456789ab"]);
    }
}
