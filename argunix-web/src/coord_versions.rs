//! Coordinator-side version detection for `nix` and `nix-eval-jobs`.
//!
//! The /hosts page renders these alongside the coordinator card so an
//! operator can see at a glance which toolchain the daemon will use to
//! drive evaluations. Detection runs once at daemon startup — these
//! binaries don't change while the daemon is up — and the result is
//! stashed in `AppState`. The dev binary hard-codes a fixture so
//! template rendering doesn't depend on the host having these tools
//! installed.
//!
//! Two strategies, tried in order:
//!  1. `<bin> --version` and parse the output.
//!  2. Resolve the binary on PATH, canonicalise to follow the symlink
//!     into `/nix/store`, and extract `<version>` from the
//!     `<hash>-<pname>-<version>` directory component.
//!
//! `nix-eval-jobs` has no `--version` flag, so it falls through to (2)
//! every time. `nix` typically resolves at (1). Anything we can't pin
//! down stays "unknown" so the card always renders something legible.

use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Resolved versions of the two coordinator-side tools the /hosts
/// page surfaces. Both fields default to "unknown" when detection
/// fails (binary missing, --version output unrecognised, …).
#[derive(Debug, Clone)]
pub struct CoordinatorVersions {
    pub nix_version: String,
    pub nix_eval_jobs_version: String,
}

impl CoordinatorVersions {
    /// Placeholder used by test/dev fixtures and by the daemon when
    /// detection is skipped. Both fields read "unknown" — the
    /// template renders that verbatim, signalling to the operator
    /// that detection didn't run.
    pub fn unknown() -> Self {
        Self {
            nix_version: "unknown".into(),
            nix_eval_jobs_version: "unknown".into(),
        }
    }
}

/// Detect both versions in parallel, each via the two-step strategy
/// (CLI `--version` first, nix-store path second). Each binary is
/// named at the call site so the daemon can pin them via NixOS-module
/// paths (matching the same `--nix-bin` / `--nix-store-bin` plumbing
/// the worker uses). Probes never fail: a missing binary just leaves
/// that field as "unknown".
pub async fn detect(nix_bin: &str, nix_eval_jobs_bin: &str) -> CoordinatorVersions {
    let (nix, nej) = tokio::join!(
        detect_one(nix_bin, parse_nix_version),
        detect_one(nix_eval_jobs_bin, parse_nix_eval_jobs_version),
    );
    CoordinatorVersions {
        nix_version: nix.unwrap_or_else(|| "unknown".into()),
        nix_eval_jobs_version: nej.unwrap_or_else(|| "unknown".into()),
    }
}

/// Try `<bin> --version` first; if that fails (non-zero exit, missing
/// binary, or unparsable output), fall back to extracting the version
/// from the binary's nix-store path. `nix-eval-jobs` has no version
/// flag, so it always lands in the fallback.
async fn detect_one(bin: &str, parse: fn(&str) -> Option<String>) -> Option<String> {
    if let Some(out) = run_version(bin).await {
        if let Some(v) = parse(&out) {
            return Some(v);
        }
    }
    version_from_bin_path(bin)
}

async fn run_version(bin: &str) -> Option<String> {
    let output = tokio::process::Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Resolve `bin` to an absolute, symlink-followed path and pull the
/// `<version>` out of the `/nix/store/<hash>-<pname>-<version>/...`
/// component. Returns None on a non-store path (e.g. `/usr/bin/...`
/// on a non-NixOS dev box) — caller renders "unknown" then.
fn version_from_bin_path(bin: &str) -> Option<String> {
    let real = resolve_bin(bin)?;
    version_from_store_path(&real)
}

/// Find `bin` on disk: absolute / relative paths are canonicalised
/// directly; bare names are looked up against `$PATH`. Symlinks are
/// followed so we end up at the actual nix-store path.
fn resolve_bin(bin: &str) -> Option<PathBuf> {
    let path = Path::new(bin);
    if path.is_absolute() || bin.contains('/') {
        return std::fs::canonicalize(path).ok();
    }
    let env_path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&env_path) {
        let cand = dir.join(bin);
        if cand.is_file() {
            return std::fs::canonicalize(cand).ok();
        }
    }
    None
}

/// Pull the version out of `/nix/store/<hash>-<pname>-<version>/...`.
/// We strip the leading `<hash>-` from the store-entry directory and
/// then walk left-to-right for the first segment that starts with a
/// digit — everything from there to the end of the directory name is
/// the version. This handles `2.34.1` as well as multi-segment
/// versions like `0.1.0-rc1` while tolerating multi-word pnames such
/// as `nix-eval-jobs`.
pub fn version_from_store_path(path: &Path) -> Option<String> {
    let mut comps = path.components();
    while let Some(c) = comps.next() {
        if c.as_os_str() == "store" {
            break;
        }
    }
    let entry = comps.next()?.as_os_str().to_str()?;
    let after_hash = entry.split_once('-')?.1;
    let segments: Vec<&str> = after_hash.split('-').collect();
    let first_ver = segments
        .iter()
        .position(|s| s.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
    Some(segments[first_ver..].join("-"))
}

/// Pull the version token out of `nix --version` output. Mirrors the
/// helper in `argunix-builder/src/capabilities.rs`; duplicated rather
/// than imported so argunix-web doesn't have to depend on the agent
/// crate just for a 6-line parser.
pub fn parse_nix_version(output: &str) -> Option<String> {
    let first = output.lines().next()?.trim();
    let token = first.split_whitespace().last()?;
    if !token.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(token.to_string())
}

/// `nix-eval-jobs --version` outputs `nix-eval-jobs <version>` (with
/// a trailing newline) on every release we've seen. Same defensive
/// stance as `parse_nix_version`: we want a digit-led token or
/// nothing.
pub fn parse_nix_eval_jobs_version(output: &str) -> Option<String> {
    let first = output.lines().next()?.trim();
    let token = first.split_whitespace().last()?;
    if !token.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(token.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nix_classic() {
        assert_eq!(
            parse_nix_version("nix (Nix) 2.18.1\n"),
            Some("2.18.1".into())
        );
    }

    #[test]
    fn parses_nix_lix() {
        assert_eq!(
            parse_nix_version("nix (Lix, like Nix) 2.91.0"),
            Some("2.91.0".into()),
        );
    }

    #[test]
    fn parses_nix_eval_jobs() {
        assert_eq!(
            parse_nix_eval_jobs_version("nix-eval-jobs 2.24.0\n"),
            Some("2.24.0".into()),
        );
    }

    #[test]
    fn rejects_non_numeric_token() {
        assert_eq!(parse_nix_version("usage: nix [options]"), None);
        assert_eq!(parse_nix_eval_jobs_version(""), None);
    }

    #[test]
    fn version_from_store_path_extracts_simple_semver() {
        let p = Path::new(
            "/nix/store/vbd5db5lx59kvq6q80qsbrx9jy82qax2-nix-eval-jobs-2.34.1/bin/nix-eval-jobs",
        );
        assert_eq!(version_from_store_path(p), Some("2.34.1".into()));
    }

    #[test]
    fn version_from_store_path_extracts_pre_release_suffix() {
        // Nix store entries for pre-release versions look like
        // `<hash>-<pname>-0.1.0-rc1` — we want the full tail starting
        // at the first digit-led segment.
        let p = Path::new("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-foo-0.1.0-rc1/bin/foo");
        assert_eq!(version_from_store_path(p), Some("0.1.0-rc1".into()));
    }

    #[test]
    fn version_from_store_path_handles_single_word_pname() {
        // `nix` itself: `<hash>-nix-2.18.1`.
        let p = Path::new("/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-nix-2.18.1/bin/nix");
        assert_eq!(version_from_store_path(p), Some("2.18.1".into()));
    }

    #[test]
    fn version_from_store_path_returns_none_for_non_store_path() {
        // Dev box where the binary is shipped via apt, not nix.
        let p = Path::new("/usr/bin/nix-eval-jobs");
        assert_eq!(version_from_store_path(p), None);
    }

    #[test]
    fn version_from_store_path_returns_none_when_no_digit_segment() {
        // Pathological: a store entry whose pname has no version. Not
        // observed in practice, but we'd rather render "unknown" than
        // surface a chunk of the package name.
        let p = Path::new("/nix/store/aaaa-just-a-name/bin/x");
        assert_eq!(version_from_store_path(p), None);
    }

    /// Live smoke against whatever `nix` / `nix-eval-jobs` are on the
    /// host. `#[ignore]` so CI doesn't need either tool installed —
    /// run with `cargo test -p argunix-web -- --ignored
    /// coord_versions::tests::live_detect`.
    #[tokio::test]
    #[ignore]
    async fn live_detect() {
        let v = detect("nix", "nix-eval-jobs").await;
        println!("nix: {}", v.nix_version);
        println!("nix-eval-jobs: {}", v.nix_eval_jobs_version);
        assert_ne!(v.nix_version, "unknown", "nix version should resolve");
        assert_ne!(
            v.nix_eval_jobs_version, "unknown",
            "nix-eval-jobs version should resolve via store-path fallback",
        );
    }
}
