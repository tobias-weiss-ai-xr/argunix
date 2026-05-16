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
//! v1 ships exactly one effect: [`registry::RegistryPush`].

pub mod registry;

pub use registry::RegistryPush;

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
    /// Full commit sha.
    pub sha: &'a str,
    /// True when the JobSpec carried `meta.docker-image == true`.
    /// Effects that only make sense for images (registry push) skip
    /// when this is false.
    pub is_docker_image: bool,
    /// Realised output store paths. The first entry is the primary
    /// output — for a `dockerTools` image that is the docker-archive
    /// tarball.
    pub output_paths: &'a [String],
}

impl OutputContext<'_> {
    /// Branch name when `git_ref` is a `refs/heads/<branch>` ref;
    /// `None` for PR refs, tags, or anything else. Used as a docker
    /// image tag.
    pub fn branch(&self) -> Option<&str> {
        self.git_ref.strip_prefix("refs/heads/")
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

/// How seriously a failing effect should be taken.
///
/// Declared per effect *kind*, never hardcoded globally — a flaky
/// cache and a failed production deploy are not the same event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Failure is logged and recorded in `effect_runs`, but the job
    /// stays green. Correct for opportunistic pushes (cache, registry):
    /// the build itself succeeded, only distribution degraded.
    Advisory,
    /// Failure must surface on its own status line. Reserved for
    /// effects like deploy where "did it actually happen" is the
    /// whole point; no effect uses it yet, but [`Effect::severity`]
    /// is the seam so a future deploy effect doesn't have to retrofit
    /// the call site.
    #[allow(dead_code)]
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
            sha: "0123456789abcdef0123456789abcdef01234567",
            is_docker_image: true,
            output_paths: &[],
        };
        assert_eq!(ctx.branch(), Some("main"));
        assert_eq!(ctx.short_sha(), "0123456789ab");
    }

    #[test]
    fn branch_none_for_pr_ref() {
        let ctx = OutputContext {
            forge: "gh",
            repo_slug: "o/r",
            attr_path: "x",
            system: "x86_64-linux",
            git_ref: "refs/pull/7/head",
            sha: "abc",
            is_docker_image: true,
            output_paths: &[],
        };
        assert_eq!(ctx.branch(), None);
        // short_sha falls back when the sha is shorter than 12.
        assert_eq!(ctx.short_sha(), "abc");
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
