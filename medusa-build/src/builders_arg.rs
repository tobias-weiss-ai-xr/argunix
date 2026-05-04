//! `--builders` argument construction for `nix-store --realise`.
//!
//! When the dynamic builder pool is in use, we pass nix one line per
//! currently-Active builder. nix's parser then matches a job's
//! `<system>` against each entry's systems list and picks one (using
//! its own scheduling — speed factor, max-jobs, etc).
//!
//! Per-line format (same shape as `nix.buildMachines`):
//!
//! ```text
//! <URI> <SYSTEMS> <SSH-KEY> <MAX-JOBS> <SPEED-FACTOR> <FEATURES> <MANDATORY-FEATURES>
//! ```
//!
//! medusa uses:
//!
//! - `URI = ssh-ng://x@local?ssh-command=<medusa-pipe-path>%20<name>` —
//!   `medusa-pipe` becomes nix's transport, connecting to the
//!   per-builder Unix socket which the daemon proxies onto the chosen
//!   agent's SSH session.
//! - `SSH-KEY = -` — none, the proxy doesn't need one.
//! - `SPEED-FACTOR = 1` — uniform until we have a reason to differentiate.
//! - `MANDATORY-FEATURES` empty.
//!
//! Lines are joined with `\n`; nix's argument parser treats the whole
//! string as a multi-line buildMachines block.

use medusa_builders::{BuilderRegistry, ConnState};

/// Build the `--builders` argument string. Returns `None` when no
/// Active builders are registered, in which case the caller should
/// fall through to the host's existing `nix.buildMachines` (the
/// additive-not-replacing semantics from `design/builders.md`).
///
/// `medusa_pipe_path` is the absolute path to the `medusa-pipe`
/// binary; we URL-encode it as a single token so spaces don't break
/// nix's parser. `socket_dir` defaults to `/run/medusa/builders` on
/// the agent end (medusa-pipe's --socket-dir flag); we don't pass it
/// here because nix treats unknown query params as opaque.
pub fn compose_builders_arg(registry: &BuilderRegistry, medusa_pipe_path: &str) -> Option<String> {
    let active: Vec<_> = registry
        .list()
        .into_iter()
        .filter(|b| b.state == ConnState::Active)
        .collect();
    if active.is_empty() {
        return None;
    }
    let mut lines = Vec::with_capacity(active.len());
    for b in active {
        lines.push(format_one(&b.name, &b.capabilities, medusa_pipe_path));
    }
    Some(lines.join("\n"))
}

fn format_one(
    name: &medusa_domain::BuilderName,
    caps: &medusa_domain::BuilderCapabilities,
    medusa_pipe_path: &str,
) -> String {
    let url_encoded_pipe = url_encode_token(medusa_pipe_path);
    let uri = format!(
        "ssh-ng://x@local?ssh-command={pipe}%20{name}",
        pipe = url_encoded_pipe,
        name = name,
    );
    let systems = if caps.systems.is_empty() {
        "-".to_string()
    } else {
        caps.systems.join(",")
    };
    let features = if caps.features.is_empty() {
        "-".to_string()
    } else {
        caps.features.join(",")
    };
    format!(
        "{uri} {systems} - {max_jobs} 1 {features} -",
        max_jobs = caps.max_jobs,
    )
}

/// Minimal percent-encoding for the medusa-pipe path so nix parses
/// the URI correctly when the path contains spaces / special chars.
/// We're not URL-encoding for the network — just escaping shell-ish
/// metacharacters that would confuse nix's machines parser.
fn url_encode_token(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use medusa_builders::ConnectedBuilder;
    use medusa_domain::{BuilderCapabilities, BuilderId, BuilderName};
    use std::sync::Arc;

    fn caps(systems: &[&str], features: &[&str], max_jobs: u32) -> BuilderCapabilities {
        BuilderCapabilities {
            systems: systems.iter().map(|s| s.to_string()).collect(),
            features: features.iter().map(|s| s.to_string()).collect(),
            max_jobs,
            nix_version: "test".into(),
        }
    }

    fn conn(reg: &BuilderRegistry, id: i64, c: BuilderCapabilities) -> ConnectedBuilder {
        ConnectedBuilder {
            builder_id: BuilderId::new(id),
            capabilities: c,
            state: ConnState::Active,
            connected_since: Utc::now(),
            connection_id: reg.next_connection_id(),
            session: None,
        }
    }

    #[test]
    fn empty_registry_yields_none() {
        let reg: Arc<BuilderRegistry> = BuilderRegistry::new();
        assert!(compose_builders_arg(&reg, "/usr/bin/medusa-pipe").is_none());
    }

    #[test]
    fn single_builder_renders_expected_line() {
        let reg = BuilderRegistry::new();
        let _ = reg.register(
            BuilderName::new("bobs-mini").unwrap(),
            conn(&reg, 1, caps(&["aarch64-darwin"], &["big-parallel"], 2)),
        );
        let got = compose_builders_arg(&reg, "/usr/bin/medusa-pipe").unwrap();
        assert_eq!(
            got,
            "ssh-ng://x@local?ssh-command=/usr/bin/medusa-pipe%20bobs-mini \
             aarch64-darwin - 2 1 big-parallel -"
                .replace("             ", "")
        );
    }

    #[test]
    fn multi_builder_one_line_per_entry() {
        let reg = BuilderRegistry::new();
        let _ = reg.register(
            BuilderName::new("a").unwrap(),
            conn(&reg, 1, caps(&["x86_64-linux"], &[], 4)),
        );
        let _ = reg.register(
            BuilderName::new("b").unwrap(),
            conn(&reg, 2, caps(&["aarch64-linux"], &[], 2)),
        );
        let got = compose_builders_arg(&reg, "/usr/bin/medusa-pipe").unwrap();
        let lines: Vec<&str> = got.split('\n').collect();
        assert_eq!(lines.len(), 2, "one line per Active builder");
    }

    #[test]
    fn disconnecting_builders_excluded() {
        let reg = BuilderRegistry::new();
        let name = BuilderName::new("dying").unwrap();
        let _ = reg.register(name.clone(), conn(&reg, 1, caps(&["x86_64-linux"], &[], 1)));
        reg.mark_disconnecting(&name);
        // No Active builders → None.
        assert!(compose_builders_arg(&reg, "/usr/bin/medusa-pipe").is_none());
    }

    #[test]
    fn empty_features_render_as_dash() {
        let reg = BuilderRegistry::new();
        let _ = reg.register(
            BuilderName::new("plain").unwrap(),
            conn(&reg, 1, caps(&["x86_64-linux"], &[], 1)),
        );
        let got = compose_builders_arg(&reg, "/usr/bin/medusa-pipe").unwrap();
        // `features` slot must be `-`, not empty.
        assert!(got.contains(" - 1 1 - -"), "got: {got}");
    }

    #[test]
    fn multiple_systems_comma_joined() {
        let reg = BuilderRegistry::new();
        let _ = reg.register(
            BuilderName::new("m").unwrap(),
            conn(&reg, 1, caps(&["aarch64-darwin", "x86_64-darwin"], &[], 2)),
        );
        let got = compose_builders_arg(&reg, "/usr/bin/medusa-pipe").unwrap();
        assert!(got.contains("aarch64-darwin,x86_64-darwin"));
    }

    #[test]
    fn pipe_path_with_spaces_is_url_encoded() {
        let reg = BuilderRegistry::new();
        let _ = reg.register(
            BuilderName::new("a").unwrap(),
            conn(&reg, 1, caps(&["x86_64-linux"], &[], 1)),
        );
        let got = compose_builders_arg(&reg, "/path with space/medusa-pipe").unwrap();
        // Spaces inside the path get %20-encoded so nix's
        // whitespace-separated parser doesn't split mid-token.
        // Path slashes are kept readable.
        assert!(
            got.contains("/path%20with%20space/medusa-pipe%20a"),
            "spaces encoded but slashes preserved; got: {got}",
        );
    }
}
