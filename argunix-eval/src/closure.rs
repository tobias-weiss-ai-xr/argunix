//! Walk the dependency closure of one or more head derivations via
//! `nix derivation show --recursive`. The result is a deduplicated map
//! `drv_path → DerivationInfo` covering the union of every head's
//! transitive inputs — exactly the input shape `argunix-sched`'s
//! `DagStrategy` consumes.
//!
//! # Schema
//!
//! `nix derivation show --recursive` produces JSON of the form:
//!
//! ```json
//! {
//!   "version": 4,
//!   "derivations": {
//!     "<basename>.drv": {
//!       "system": "x86_64-linux",
//!       "inputs": { "drvs": { "<input-basename>.drv": {...} }, "srcs": [] },
//!       "env": { "requiredSystemFeatures": "kvm uid-range", ... },
//!       ...
//!     },
//!     ...
//!   }
//! }
//! ```
//!
//! Keys (both at top level and inside `inputs.drvs`) are *basenames*,
//! not full paths. We prepend the store dir, detected from the head's
//! own full path, so the result's keys match what nix-eval-jobs and
//! the rest of the pipeline emit.
//!
//! # Schema versioning
//!
//! Schema `version` 4 is what current Nix emits. Earlier versions
//! aren't supported here; older Nix is expected to be upgraded.

use argunix_domain::DerivationInfo;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

const DEFAULT_STORE_DIR: &str = "/nix/store";
/// Schema versions emitted by `nix derivation show --recursive` we know
/// how to parse.
const SUPPORTED_VERSIONS: &[u64] = &[3, 4];

#[derive(Debug, thiserror::Error)]
pub enum ClosureError {
    #[error("spawning nix derivation show: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("reading nix derivation show output: {0}")]
    Io(#[from] std::io::Error),
    #[error("nix derivation show exited with status {status:?}\nstderr:\n{stderr}")]
    NonZero { status: Option<i32>, stderr: String },
    #[error("nix derivation show timed out after {seconds}s")]
    Timeout { seconds: u64 },
    #[error("parsing nix derivation show output: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("unsupported nix derivation show schema version {version} (supported: {supported:?})")]
    UnsupportedVersion {
        version: u64,
        supported: &'static [u64],
    },
}

/// Result of walking one or more head derivations' transitive closures.
/// `derivations` is keyed by full drv path (with the store dir prefix);
/// every drv from every head's closure (including the heads themselves)
/// appears exactly once.
#[derive(Debug, Clone)]
pub struct ClosureWalk {
    /// Head drv paths echoed back, in the order the caller passed them.
    /// Useful to map per-Job ScheduleItems back to their entry in
    /// `derivations` without re-querying nix.
    pub heads: Vec<String>,
    pub derivations: HashMap<String, DerivationInfo>,
}

impl ClosureWalk {
    /// Compute the per-head transitive closure (head + every drv
    /// reachable via `input_drvs` that exists in `derivations`). Drvs
    /// not in `derivations` are external (substituters) and excluded —
    /// matching `DagStrategy`'s behaviour of only gating on in-graph
    /// deps.
    ///
    /// Returned vector excludes the head itself; callers should pass it
    /// to `ScheduleItem.closure` and the head separately as
    /// `head_drv`.
    pub fn closure_for(&self, head_drv: &str) -> Vec<DerivationInfo> {
        use std::collections::HashSet;
        let mut out: Vec<DerivationInfo> = Vec::new();
        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(head_drv.to_string());
        let mut queue: Vec<String> = self
            .derivations
            .get(head_drv)
            .map(|d| d.input_drvs.clone())
            .unwrap_or_default();
        while let Some(drv) = queue.pop() {
            if !visited.insert(drv.clone()) {
                continue;
            }
            // External deps (not in derivations map) are silently
            // dropped — DagStrategy doesn't gate on them.
            if let Some(info) = self.derivations.get(&drv) {
                queue.extend(info.input_drvs.iter().cloned());
                out.push(info.clone());
            }
        }
        out
    }
}

/// Spawn `nix derivation show --recursive <heads>` and parse the
/// result. `heads` are full drv paths (`/nix/store/...-foo.drv`); they
/// must all live under the same store dir.
pub async fn walk_closures(
    heads: &[&str],
    wall_clock: Duration,
) -> Result<ClosureWalk, ClosureError> {
    if heads.is_empty() {
        return Ok(ClosureWalk {
            heads: Vec::new(),
            derivations: HashMap::new(),
        });
    }

    tracing::debug!(head_count = heads.len(), "spawning nix derivation show");

    let mut cmd = Command::new("nix");
    cmd.args([
        "--extra-experimental-features",
        "nix-command",
        "derivation",
        "show",
        "--recursive",
    ]);
    for h in heads {
        cmd.arg(h);
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(ClosureError::Spawn)?;

    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");

    let collect = async {
        let mut so = String::new();
        let mut se = String::new();
        let so_fut = stdout.read_to_string(&mut so);
        let se_fut = stderr.read_to_string(&mut se);
        tokio::try_join!(so_fut, se_fut)?;
        Ok::<(String, String), std::io::Error>((so, se))
    };

    let (stdout_buf, stderr_buf) = match timeout(wall_clock, collect).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(ClosureError::Io(e)),
        Err(_) => {
            let _ = child.start_kill();
            return Err(ClosureError::Timeout {
                seconds: wall_clock.as_secs(),
            });
        }
    };

    let status = match timeout(Duration::from_secs(5), child.wait()).await {
        Ok(s) => s?,
        Err(_) => {
            let _ = child.start_kill();
            return Err(ClosureError::Timeout {
                seconds: wall_clock.as_secs(),
            });
        }
    };
    if !status.success() {
        return Err(ClosureError::NonZero {
            status: status.code(),
            stderr: stderr_buf,
        });
    }

    parse(&stdout_buf, heads)
}

/// Parse-only entry point, exposed for unit testing without spawning
/// `nix`. `heads` is just echoed back into the result.
pub fn parse(json: &str, heads: &[&str]) -> Result<ClosureWalk, ClosureError> {
    let store_dir = detect_store_dir(heads);
    let doc: ShowDoc = serde_json::from_str(json)?;
    if let Some(v) = doc.version {
        if !SUPPORTED_VERSIONS.contains(&v) {
            return Err(ClosureError::UnsupportedVersion {
                version: v,
                supported: SUPPORTED_VERSIONS,
            });
        }
    }
    let mut derivations = HashMap::with_capacity(doc.derivations.len());
    for (basename, raw) in doc.derivations {
        let drv_path = format!("{store_dir}/{basename}");
        let input_drvs: Vec<String> = raw
            .inputs
            .drvs
            .into_keys()
            .map(|k| format!("{store_dir}/{k}"))
            .collect();
        let required_features = raw
            .env
            .required_system_features
            .map(|s| {
                s.split_whitespace()
                    .map(String::from)
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default();
        derivations.insert(
            drv_path.clone(),
            DerivationInfo {
                drv_path,
                system: Some(raw.system),
                required_features,
                input_drvs,
            },
        );
    }
    Ok(ClosureWalk {
        heads: heads.iter().map(|s| s.to_string()).collect(),
        derivations,
    })
}

/// Return the directory portion of the first head, e.g. `/nix/store`
/// for `/nix/store/aaaa-foo.drv`. Falls back to `/nix/store` if the
/// caller passed something unexpected (no slash). All heads are
/// assumed to share the same store dir; we don't cross-check.
fn detect_store_dir(heads: &[&str]) -> String {
    heads
        .first()
        .and_then(|h| Path::new(h).parent())
        .and_then(|p| p.to_str())
        .map(String::from)
        .unwrap_or_else(|| DEFAULT_STORE_DIR.to_string())
}

#[derive(Deserialize)]
struct ShowDoc {
    #[serde(default)]
    version: Option<u64>,
    derivations: HashMap<String, RawDrv>,
}

#[derive(Deserialize)]
struct RawDrv {
    system: String,
    inputs: RawInputs,
    #[serde(default)]
    env: RawEnv,
}

#[derive(Deserialize)]
struct RawInputs {
    #[serde(default)]
    drvs: HashMap<String, serde_json::Value>,
}

#[derive(Deserialize, Default)]
struct RawEnv {
    #[serde(rename = "requiredSystemFeatures")]
    required_system_features: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drv_show_v4(body: &str) -> String {
        format!(r#"{{"version":4,"derivations":{body}}}"#)
    }

    #[test]
    fn parses_minimal_single_drv() {
        let json = drv_show_v4(
            r#"{
                "aaaa-hello.drv": {
                    "system": "x86_64-linux",
                    "inputs": { "drvs": {}, "srcs": [] },
                    "env": {}
                }
            }"#,
        );
        let walk = parse(&json, &["/nix/store/aaaa-hello.drv"]).unwrap();
        assert_eq!(walk.derivations.len(), 1);
        let d = walk.derivations.get("/nix/store/aaaa-hello.drv").unwrap();
        assert_eq!(d.drv_path, "/nix/store/aaaa-hello.drv");
        assert_eq!(d.system.as_deref(), Some("x86_64-linux"));
        assert!(d.required_features.is_empty());
        assert!(d.input_drvs.is_empty());
    }

    #[test]
    fn input_drvs_are_prefixed_with_store_dir() {
        let json = drv_show_v4(
            r#"{
                "bbbb-b.drv": {
                    "system": "x86_64-linux",
                    "inputs": {
                        "drvs": {
                            "aaaa-a.drv": {"outputs":["out"],"dynamicOutputs":{}}
                        },
                        "srcs": []
                    },
                    "env": {}
                },
                "aaaa-a.drv": {
                    "system": "x86_64-linux",
                    "inputs": { "drvs": {}, "srcs": [] },
                    "env": {}
                }
            }"#,
        );
        let walk = parse(&json, &["/nix/store/bbbb-b.drv"]).unwrap();
        let b = walk.derivations.get("/nix/store/bbbb-b.drv").unwrap();
        assert_eq!(b.input_drvs, vec!["/nix/store/aaaa-a.drv".to_string()]);
    }

    #[test]
    fn required_features_are_split_on_whitespace() {
        let json = drv_show_v4(
            r#"{
                "aaaa.drv": {
                    "system": "x86_64-linux",
                    "inputs": { "drvs": {}, "srcs": [] },
                    "env": { "requiredSystemFeatures": "kvm  uid-range\tcuda" }
                }
            }"#,
        );
        let walk = parse(&json, &["/nix/store/aaaa.drv"]).unwrap();
        let d = walk.derivations.get("/nix/store/aaaa.drv").unwrap();
        assert_eq!(
            d.required_features,
            vec![
                "kvm".to_string(),
                "uid-range".to_string(),
                "cuda".to_string()
            ],
        );
    }

    #[test]
    fn missing_required_features_field_yields_empty_vec() {
        let json = drv_show_v4(
            r#"{
                "aaaa.drv": {
                    "system": "x86_64-linux",
                    "inputs": { "drvs": {}, "srcs": [] },
                    "env": {}
                }
            }"#,
        );
        let walk = parse(&json, &["/nix/store/aaaa.drv"]).unwrap();
        let d = walk.derivations.get("/nix/store/aaaa.drv").unwrap();
        assert!(d.required_features.is_empty());
    }

    #[test]
    fn unknown_schema_version_is_rejected() {
        let json = r#"{"version":99,"derivations":{}}"#;
        let err = parse(json, &["/nix/store/x.drv"]).unwrap_err();
        match err {
            ClosureError::UnsupportedVersion { version, .. } => assert_eq!(version, 99),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn store_dir_detected_from_first_head() {
        // Custom store dir, e.g. a nix chroot store.
        let json = r#"{"version":4,"derivations":{
                "aaaa.drv": {
                    "system": "x86_64-linux",
                    "inputs": { "drvs": {}, "srcs": [] },
                    "env": {}
                }
            }}"#;
        let walk = parse(json, &["/var/lib/medusa/store/aaaa.drv"]).unwrap();
        assert!(
            walk.derivations
                .contains_key("/var/lib/medusa/store/aaaa.drv"),
            "key was: {:?}",
            walk.derivations.keys().collect::<Vec<_>>(),
        );
    }

    #[test]
    fn closure_for_returns_only_in_graph_inputs_excluding_head() {
        // hello → glibc; both are in the map.
        // hello also lists `bash.drv` as input; bash is NOT in the map
        // (substitute-only) and must be excluded from closure_for.
        let json = drv_show_v4(
            r#"{
                "hello.drv": {
                    "system": "x86_64-linux",
                    "inputs": {
                        "drvs": {
                            "glibc.drv": {"outputs":["out"],"dynamicOutputs":{}},
                            "bash.drv": {"outputs":["out"],"dynamicOutputs":{}}
                        },
                        "srcs": []
                    },
                    "env": {}
                },
                "glibc.drv": {
                    "system": "x86_64-linux",
                    "inputs": { "drvs": {}, "srcs": [] },
                    "env": {}
                }
            }"#,
        );
        let walk = parse(&json, &["/nix/store/hello.drv"]).unwrap();
        let closure = walk.closure_for("/nix/store/hello.drv");
        assert_eq!(closure.len(), 1, "only glibc; bash is external");
        assert_eq!(closure[0].drv_path, "/nix/store/glibc.drv");
    }

    #[test]
    fn closure_for_is_transitive() {
        // hello → glibc → linux (deep chain).
        let json = drv_show_v4(
            r#"{
                "hello.drv": {
                    "system": "x86_64-linux",
                    "inputs": { "drvs": {"glibc.drv":{"outputs":["out"],"dynamicOutputs":{}}}, "srcs": [] },
                    "env": {}
                },
                "glibc.drv": {
                    "system": "x86_64-linux",
                    "inputs": { "drvs": {"linux.drv":{"outputs":["out"],"dynamicOutputs":{}}}, "srcs": [] },
                    "env": {}
                },
                "linux.drv": {
                    "system": "x86_64-linux",
                    "inputs": { "drvs": {}, "srcs": [] },
                    "env": {}
                }
            }"#,
        );
        let walk = parse(&json, &["/nix/store/hello.drv"]).unwrap();
        let closure = walk.closure_for("/nix/store/hello.drv");
        let paths: std::collections::HashSet<&str> =
            closure.iter().map(|d| d.drv_path.as_str()).collect();
        assert!(paths.contains("/nix/store/glibc.drv"));
        assert!(paths.contains("/nix/store/linux.drv"));
        assert!(!paths.contains("/nix/store/hello.drv"), "head is excluded");
    }

    #[test]
    fn empty_heads_returns_empty_walk() {
        // Async helper not invoked here; just check `parse` handles
        // an empty derivations map.
        let walk = parse(r#"{"version":4,"derivations":{}}"#, &[]).unwrap();
        assert!(walk.derivations.is_empty());
        assert!(walk.heads.is_empty());
    }
}
