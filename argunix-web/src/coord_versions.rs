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
//! Output parsing matches the formats observed in the wild:
//!   `nix (Nix) 2.18.1`
//!   `nix (Lix, like Nix) 2.91.0`
//!   `nix-eval-jobs 2.24.0`
//!
//! Anything we can't parse falls back to "unknown" so the card always
//! renders something legible — startup never fails over a cosmetic
//! field.

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

/// Run both `--version` probes in parallel. Each binary is named at
/// the call site so the daemon can pin them via NixOS-module-supplied
/// paths (matching the same `--nix-bin` / `--nix-store-bin` plumbing
/// the worker uses). Probes never fail: a missing binary just leaves
/// that field as "unknown".
pub async fn detect(nix_bin: &str, nix_eval_jobs_bin: &str) -> CoordinatorVersions {
    let (nix, nej) = tokio::join!(run_version(nix_bin), run_version(nix_eval_jobs_bin),);
    CoordinatorVersions {
        nix_version: nix
            .as_deref()
            .and_then(parse_nix_version)
            .unwrap_or_else(|| "unknown".into()),
        nix_eval_jobs_version: nej
            .as_deref()
            .and_then(parse_nix_eval_jobs_version)
            .unwrap_or_else(|| "unknown".into()),
    }
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
}
