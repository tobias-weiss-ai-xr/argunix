//! Post-build *effects*: authenticated, impure things argunix does with a
//! build's outputs *after* the derivation has been realised.
//!
//! A binary cache push is deeply nix-native (`nix copy`, signing keys,
//! substituter semantics) and stays in `argunix-build`. Everything else
//! in this family — pushing a docker image to an external registry,
//! deploying a NixOS system over SSH, uploading an SBOM — shares one
//! shape: *job succeeded → take its outputs → do an authenticated thing
//! to an external system*. None of it can happen inside the nix
//! sandbox; the nix ecosystem calls these "effects" and so do we.
//!
//! The design is **generic in the lifecycle, specialised in the
//! action**. The plumbing — selecting which effects apply, running
//! them, recording an `effect_runs` row per attempt, declaring a
//! failure severity — is shared and lives at the call site (the worker
//! / the `argunix build` CLI). Each *action* is a hand-written typed
//! implementation of [`Effect`]. There is intentionally no
//! "run an arbitrary command" effect: that would re-import the
//! untyped-YAML, opaque-debugging world argunix exists to replace.
//! Adding a new effect later (deploy, SBOM upload) is a new `impl
//! Effect` — the trait is the extension point, so the door stays open
//! without a generic config escape hatch.
//!
//! Effects shipped here: [`registry::RegistryPush`] pushes a built
//! image to an external registry; [`sbom::SbomAttach`] generates a
//! CycloneDX SBOM for an OCI image and attaches it to that image as an
//! OCI referrer. See `design/sbom.md`.

pub mod multiarch;
pub mod registry;
pub mod sbom;

pub use multiarch::{ArchSlice, MultiArchTarget};
pub use registry::RegistryPush;
pub use sbom::SbomAttach;

pub use argunix_domain::ImageFormat;

use async_trait::async_trait;

/// Everything an [`Effect`] needs to know about the build output it is
/// acting on. Borrowed for the duration of one [`Effect::run`] call —
/// the worker owns the backing strings.
#[derive(Debug, Clone, Copy)]
pub struct OutputContext<'a> {
    /// Configured forge name, e.g. `"github-myorg"`. First path
    /// segment of a registry image name.
    pub forge: &'a str,
    /// Repo slug, e.g. `"tfc/argunix"`.
    pub repo_slug: &'a str,
    /// Full argunix attr path, e.g. `packages.x86_64-linux.my-image`.
    pub attr_path: &'a str,
    /// Nix system tuple the output was built for.
    pub system: &'a str,
    /// Git ref the eval ran against, e.g. `refs/heads/main` or a PR
    /// ref. Effects derive a human tag from it via [`Self::branch`].
    pub git_ref: &'a str,
    /// The repo's default branch (`main` / `master`), as reported by
    /// the forge on webhook payloads. `None` when argunix has not yet
    /// seen a webhook for the repo. Drives the `latest` tag — only a
    /// build *on* the default branch moves it.
    pub default_branch: Option<&'a str>,
    /// Full commit sha.
    pub sha: &'a str,
    /// `Some` when the JobSpec declared `meta.image-format`, carrying
    /// the archive format. Effects that only make sense for images
    /// (registry push) skip when this is `None`.
    pub image_format: Option<ImageFormat>,
    /// Realised output store paths. The first entry is the primary
    /// output — for a `dockerTools` image that is the docker-archive
    /// tarball.
    pub output_paths: &'a [String],
    /// Store paths the flake declared via `meta.sbom-runtime-roots` —
    /// the runtime contents of an OCI image, the roots whose closure
    /// [`sbom::SbomAttach`] transcribes into an SBOM. Empty for any job
    /// that did not declare the attribute.
    pub sbom_runtime_roots: &'a [String],
}

impl OutputContext<'_> {
    /// Branch name this build ran on, or `None` for a pull-request /
    /// non-branch build. Used as a docker image tag.
    ///
    /// `git_ref` reaches an effect in one of three shapes; this
    /// flattens all of them:
    ///
    /// * `<branch>` — a bare branch name: how the daemon stores a
    ///   *push* eval's ref (`argunix-web::webhook` drops the
    ///   `refs/heads/` prefix on ingest);
    /// * `refs/heads/<branch>` — the raw form the `argunix build` CLI
    ///   passes straight through `--git-ref`;
    /// * `refs/pull/<n>/head:<branch>` — the synthetic form a daemon
    ///   *pull-request* eval carries. Not a branch build → `None`.
    pub fn branch(&self) -> Option<&str> {
        if let Some(b) = self.git_ref.strip_prefix("refs/heads/") {
            return Some(b);
        }
        // A PR ref (or any other `refs/*` shape, or an empty ref) is
        // not a branch; anything else is a bare branch name.
        if self.git_ref.is_empty() || self.git_ref.starts_with("refs/") {
            return None;
        }
        Some(self.git_ref)
    }

    /// True when this build ran *on* the repo's default branch — the
    /// condition under which `registry-push` also moves the `latest`
    /// tag. False when the ref is not a `refs/heads/*` branch or the
    /// default branch is not known.
    pub fn is_default_branch(&self) -> bool {
        matches!(
            (self.branch(), self.default_branch),
            (Some(b), Some(d)) if b == d,
        )
    }

    /// Short (12-hex) form of the commit sha, used as an immutable
    /// `sha-<short>` docker tag. Falls back to the whole sha when it
    /// is somehow shorter than 12 chars.
    pub fn short_sha(&self) -> &str {
        self.sha.get(..12).unwrap_or(self.sha)
    }

    /// Primary output store path — the first realised path. `None`
    /// when the build recorded no outputs.
    pub fn primary_output(&self) -> Option<&str> {
        self.output_paths.first().map(String::as_str)
    }
}

/// How an effect's outcome is surfaced. Declared per effect *kind*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// The outcome lives only in `effect_runs` and the daemon log — no
    /// forge check of its own. A failure never fails the job. The seam
    /// for a future purely-internal effect; no effect uses it today.
    #[allow(dead_code)]
    Advisory,
    /// The effect posts its **own** forge check — success or failure —
    /// so a contributor sees it in their PR / commit checks alongside
    /// the build. A failure still does *not* fail the job: a degraded
    /// push is not a broken build; only the effect's own check goes red.
    Reported,
}

/// Terminal state of one [`Effect::run`] call. Maps directly onto the
/// `effect_runs.status` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectStatus {
    /// The effect did its work.
    Success,
    /// The effect tried and failed. `detail` carries the reason.
    Failure,
    /// The effect did not apply to this output (e.g. a registry push
    /// against a non-image job) and deliberately did nothing.
    Skipped,
}

impl EffectStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            EffectStatus::Success => "success",
            EffectStatus::Failure => "failure",
            EffectStatus::Skipped => "skipped",
        }
    }
}

/// What one [`Effect::run`] produced. The caller turns this into an
/// `effect_runs` row and a log line.
#[derive(Debug, Clone)]
pub struct EffectOutcome {
    pub status: EffectStatus,
    /// One-line human summary on success / skip, or the error on
    /// failure. Stored in `effect_runs.detail`.
    pub detail: String,
}

impl EffectOutcome {
    pub fn success(detail: impl Into<String>) -> Self {
        Self {
            status: EffectStatus::Success,
            detail: detail.into(),
        }
    }
    pub fn failure(detail: impl Into<String>) -> Self {
        Self {
            status: EffectStatus::Failure,
            detail: detail.into(),
        }
    }
    pub fn skipped(detail: impl Into<String>) -> Self {
        Self {
            status: EffectStatus::Skipped,
            detail: detail.into(),
        }
    }
}

/// One post-build action. Implementors are hand-written and typed —
/// see the module docs for why there is no generic command effect.
#[async_trait]
pub trait Effect: Send + Sync {
    /// Stable kind tag, written verbatim to `effect_runs.kind`.
    /// e.g. `"registry-push"`.
    fn kind(&self) -> &'static str;

    /// Name of the configured target this effect acts on — a registry
    /// name, a deploy host. Written to `effect_runs.target` so the UI
    /// can say *which* of several targets failed.
    fn target(&self) -> &str;

    /// Failure severity for this kind. See [`Severity`].
    fn severity(&self) -> Severity;

    /// Run the effect against `ctx`. Must not panic and must not
    /// return `Err` — every outcome, including failure, is an
    /// [`EffectOutcome`] so the caller always gets an `effect_runs`
    /// row. Implementors capture subprocess stderr into
    /// `EffectOutcome::detail`.
    async fn run(&self, ctx: &OutputContext<'_>) -> EffectOutcome;
}

/// Derive a docker-image-name path segment from an argunix attr path.
///
/// Strips a leading `packages.<system>.` (or `dockerImages.<system>.`)
/// prefix and lowercases the remainder, replacing interior dots with
/// `-` so a nested attr still fits one image-name segment.
pub fn image_segment(attr_path: &str) -> String {
    let leaf = attr_path.splitn(3, '.').nth(2).unwrap_or(attr_path);
    leaf.to_ascii_lowercase().replace('.', "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_strips_heads_prefix() {
        let ctx = OutputContext {
            forge: "gh",
            repo_slug: "o/r",
            attr_path: "packages.x86_64-linux.img",
            system: "x86_64-linux",
            git_ref: "refs/heads/main",
            default_branch: Some("main"),
            sha: "0123456789abcdef0123456789abcdef01234567",
            image_format: Some(ImageFormat::Docker),
            output_paths: &[],
            sbom_runtime_roots: &[],
        };
        assert_eq!(ctx.branch(), Some("main"));
        assert_eq!(ctx.short_sha(), "0123456789ab");
        // Built on the default branch — `latest` applies.
        assert!(ctx.is_default_branch());
    }

    #[test]
    fn branch_none_for_pr_ref() {
        let ctx = OutputContext {
            forge: "gh",
            repo_slug: "o/r",
            attr_path: "x",
            system: "x86_64-linux",
            git_ref: "refs/pull/7/head",
            default_branch: Some("main"),
            sha: "abc",
            image_format: Some(ImageFormat::Docker),
            output_paths: &[],
            sbom_runtime_roots: &[],
        };
        assert_eq!(ctx.branch(), None);
        // short_sha falls back when the sha is shorter than 12.
        assert_eq!(ctx.short_sha(), "abc");
        // A PR ref is never the default branch.
        assert!(!ctx.is_default_branch());
    }

    #[test]
    fn is_default_branch_false_for_other_branch() {
        let ctx = OutputContext {
            forge: "gh",
            repo_slug: "o/r",
            attr_path: "x",
            system: "x86_64-linux",
            git_ref: "refs/heads/feature",
            default_branch: Some("main"),
            sha: "abc",
            image_format: Some(ImageFormat::Docker),
            output_paths: &[],
            sbom_runtime_roots: &[],
        };
        assert_eq!(ctx.branch(), Some("feature"));
        assert!(!ctx.is_default_branch());
    }

    #[test]
    fn is_default_branch_false_when_default_unknown() {
        let ctx = OutputContext {
            forge: "gh",
            repo_slug: "o/r",
            attr_path: "x",
            system: "x86_64-linux",
            git_ref: "refs/heads/main",
            default_branch: None,
            sha: "abc",
            image_format: Some(ImageFormat::Docker),
            output_paths: &[],
            sbom_runtime_roots: &[],
        };
        assert!(!ctx.is_default_branch());
    }

    #[test]
    fn branch_handles_bare_daemon_push_ref() {
        // The daemon stores a push eval's git_ref as the bare branch
        // name — the common production shape. This must resolve to the
        // branch (and, against a matching default, be the default).
        let ctx = OutputContext {
            forge: "gh",
            repo_slug: "o/r",
            attr_path: "x",
            system: "x86_64-linux",
            git_ref: "main",
            default_branch: Some("main"),
            sha: "abc",
            image_format: Some(ImageFormat::Docker),
            output_paths: &[],
            sbom_runtime_roots: &[],
        };
        assert_eq!(ctx.branch(), Some("main"));
        assert!(ctx.is_default_branch());
    }

    #[test]
    fn branch_none_for_daemon_pr_ref() {
        // A daemon PR eval carries the synthetic
        // `refs/pull/<n>/head:<branch>` form — not a branch build.
        let ctx = OutputContext {
            forge: "gh",
            repo_slug: "o/r",
            attr_path: "x",
            system: "x86_64-linux",
            git_ref: "refs/pull/7/head:feature-x",
            default_branch: Some("main"),
            sha: "abc",
            image_format: Some(ImageFormat::Docker),
            output_paths: &[],
            sbom_runtime_roots: &[],
        };
        assert_eq!(ctx.branch(), None);
        assert!(!ctx.is_default_branch());
    }

    #[test]
    fn image_segment_strips_prefix_and_lowercases() {
        assert_eq!(image_segment("packages.x86_64-linux.My-Image"), "my-image");
        assert_eq!(
            image_segment("packages.x86_64-linux.suite.Thing"),
            "suite-thing"
        );
        assert_eq!(image_segment("bare"), "bare");
    }
}
