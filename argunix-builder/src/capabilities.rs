//! Self-discovery of nix-side capabilities.
//!
//! Capabilities live exclusively on the builder side and are reported
//! to argunix on every `hello`, so they cannot drift between what the
//! daemon believes and what the builder actually supports. The daemon
//! caches the latest snapshot in `builders.systems` /
//! `builders.features` / `builders.max_jobs` / `builders.nix_version`
//! but treats the live channel as truth.
//!
//! Source of truth here is `nix show-config --json`, which the test
//! suite stubs via a simple JSON literal.

use argunix_domain::BuilderCapabilities;
use serde::Deserialize;
use std::process::Stdio;

#[derive(Debug, thiserror::Error)]
pub enum CapabilitiesError {
    #[error("spawning `nix show-config --json`: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("`nix show-config --json` exited with status {status:?}\nstderr:\n{stderr}")]
    NonZero { status: i32, stderr: String },
    #[error("waiting for `nix show-config --json`: {0}")]
    Wait(#[source] std::io::Error),
    #[error("parsing nix show-config JSON: {0}")]
    Parse(#[from] serde_json::Error),
}

/// Wrapper around `BuilderCapabilities` that also remembers which
/// nix binary produced the figures. We expose only the wrapped type
/// to callers; the binary path is for tests.
#[derive(Debug, Clone)]
pub struct Capabilities {
    pub inner: BuilderCapabilities,
}

/// Run `nix show-config --json` and translate the relevant fields
/// into `BuilderCapabilities`. The agent calls this once per
/// `hello`, so a config change on the builder is picked up at the
/// next reconnect / heartbeat cycle.
pub async fn discover_capabilities(nix_bin: &str) -> Result<Capabilities, CapabilitiesError> {
    // `nix show-config` lives under the `nix-command` experimental
    // feature. Some NixOS installs don't enable it system-wide, so we
    // turn it on per-invocation rather than depending on operator
    // config. (`config show` is the non-deprecated spelling, but
    // routes through the same gate; pinning the alias keeps stderr
    // quiet on both old and new nix.)
    let output = tokio::process::Command::new(nix_bin)
        .arg("--extra-experimental-features")
        .arg("nix-command")
        .arg("show-config")
        .arg("--json")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(CapabilitiesError::Spawn)?;
    if !output.status.success() {
        return Err(CapabilitiesError::NonZero {
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        });
    }
    let mut caps = parse_show_config(&output.stdout)?;
    // Nix 2.24+ dropped `nix-version` from `config show --json`.
    // Fall back to parsing `nix --version` so the hosts page doesn't
    // render "nix unknown".
    if caps.inner.nix_version == "unknown" {
        if let Some(v) = nix_version_via_cli(nix_bin).await {
            caps.inner.nix_version = v;
        }
    }
    Ok(caps)
}

/// Run `nix --version` and extract the semver-ish token from the
/// output. Returns `None` on any failure — the caller keeps the
/// "unknown" placeholder rather than blowing up capability discovery
/// over a cosmetic field.
async fn nix_version_via_cli(nix_bin: &str) -> Option<String> {
    let output = tokio::process::Command::new(nix_bin)
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
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_nix_version(&stdout)
}

/// Pull the version token out of `nix --version` output.
/// Examples seen in the wild:
///   `nix (Nix) 2.18.1`
///   `nix (Lix, like Nix) 2.91.0`
///   `nix (Snix) 0.1.0`
/// We grab the last whitespace-separated token on the first line.
pub fn parse_nix_version(output: &str) -> Option<String> {
    let first = output.lines().next()?.trim();
    let token = first.split_whitespace().last()?;
    // Sanity-check: must start with a digit. Anything else means we
    // misread the line — better to keep "unknown" than to surface
    // garbage.
    if !token.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(token.to_string())
}

/// Public-for-tests: parses the JSON shape `nix show-config --json`
/// produces. Tolerant of extra fields and missing optional ones.
pub fn parse_show_config(json: &[u8]) -> Result<Capabilities, CapabilitiesError> {
    // `nix show-config --json` shape (abridged):
    // {
    //   "system":          {"value": "x86_64-linux", ...},
    //   "extra-platforms": {"value": ["i686-linux"], ...},
    //   "system-features": {"value": ["kvm", "big-parallel", ...], ...},
    //   "max-jobs":        {"value": 4, ...},
    //   ...
    // }
    #[derive(Deserialize)]
    struct StringField {
        value: String,
    }
    #[derive(Deserialize)]
    struct VecField {
        #[serde(default)]
        value: Vec<String>,
    }
    #[derive(Deserialize)]
    struct IntField {
        value: serde_json::Value,
    }
    #[derive(Deserialize)]
    struct ShowConfig {
        system: StringField,
        #[serde(rename = "extra-platforms", default)]
        extra_platforms: Option<VecField>,
        #[serde(rename = "system-features", default)]
        system_features: Option<VecField>,
        #[serde(rename = "max-jobs", default)]
        max_jobs: Option<IntField>,
        #[serde(rename = "nix-version", default)]
        nix_version: Option<StringField>,
    }
    let cfg: ShowConfig = serde_json::from_slice(json)?;
    let mut systems = vec![cfg.system.value];
    if let Some(extra) = cfg.extra_platforms {
        systems.extend(extra.value);
    }
    let features = cfg.system_features.map(|v| v.value).unwrap_or_default();
    let max_jobs = cfg
        .max_jobs
        .and_then(|f| match f.value {
            serde_json::Value::Number(n) => n.as_u64(),
            serde_json::Value::String(s) => s.parse::<u64>().ok(),
            _ => None,
        })
        .unwrap_or(1)
        .min(u32::MAX as u64) as u32;
    let nix_version = cfg
        .nix_version
        .map(|f| f.value)
        .unwrap_or_else(|| "unknown".into());
    Ok(Capabilities {
        inner: BuilderCapabilities {
            systems,
            features,
            max_jobs: max_jobs.max(1),
            nix_version,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_show_config() {
        let json = br#"{
            "system": {"value": "x86_64-linux"}
        }"#;
        let caps = parse_show_config(json).unwrap();
        assert_eq!(caps.inner.systems, vec!["x86_64-linux".to_string()]);
        assert!(caps.inner.features.is_empty());
        assert_eq!(caps.inner.max_jobs, 1);
        assert_eq!(caps.inner.nix_version, "unknown");
    }

    #[test]
    fn parses_full_show_config() {
        let json = br#"{
            "system": {"value": "x86_64-linux"},
            "extra-platforms": {"value": ["i686-linux"]},
            "system-features": {"value": ["kvm", "big-parallel"]},
            "max-jobs": {"value": 4},
            "nix-version": {"value": "2.18.1"}
        }"#;
        let caps = parse_show_config(json).unwrap();
        assert_eq!(caps.inner.systems, vec!["x86_64-linux", "i686-linux"]);
        assert_eq!(caps.inner.features, vec!["kvm", "big-parallel"]);
        assert_eq!(caps.inner.max_jobs, 4);
        assert_eq!(caps.inner.nix_version, "2.18.1");
    }

    #[test]
    fn max_jobs_zero_clamps_to_one() {
        let json = br#"{
            "system": {"value": "x86_64-linux"},
            "max-jobs": {"value": 0}
        }"#;
        let caps = parse_show_config(json).unwrap();
        // BuilderCapabilities.max_jobs is u32; an agent reporting 0
        // would mean "do nothing" — meaningless. Clamp to 1 so
        // argunix always sees a usable builder.
        assert_eq!(caps.inner.max_jobs, 1);
    }

    #[test]
    fn max_jobs_string_form_accepted() {
        // Some nix builds output max-jobs as a string. Be lenient.
        let json = br#"{
            "system": {"value": "x86_64-linux"},
            "max-jobs": {"value": "8"}
        }"#;
        let caps = parse_show_config(json).unwrap();
        assert_eq!(caps.inner.max_jobs, 8);
    }

    #[test]
    fn parses_nix_version_classic() {
        assert_eq!(
            parse_nix_version("nix (Nix) 2.18.1\n"),
            Some("2.18.1".into())
        );
    }

    #[test]
    fn parses_nix_version_lix() {
        // Lix masquerades as nix; we want its actual version token.
        assert_eq!(
            parse_nix_version("nix (Lix, like Nix) 2.91.0\n"),
            Some("2.91.0".into()),
        );
    }

    #[test]
    fn parses_nix_version_snix() {
        assert_eq!(
            parse_nix_version("nix (Snix) 0.1.0\n"),
            Some("0.1.0".into())
        );
    }

    #[test]
    fn parses_nix_version_rejects_non_numeric() {
        // Defensive: if `nix --version` ever changes shape, we'd
        // rather render "unknown" than something nonsensical.
        assert_eq!(parse_nix_version("usage: nix [options]"), None);
        assert_eq!(parse_nix_version(""), None);
    }

    #[test]
    fn unknown_top_level_keys_are_ignored() {
        let json = br#"{
            "system": {"value": "x86_64-linux"},
            "unknown-knob": {"value": "ignored"}
        }"#;
        let caps = parse_show_config(json).unwrap();
        assert_eq!(caps.inner.systems, vec!["x86_64-linux".to_string()]);
    }
}
