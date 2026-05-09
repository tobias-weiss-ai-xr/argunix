use crate::jobspec::{JobSpec, parse_lines};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    #[error("source path `{0}` does not exist")]
    MissingSource(PathBuf),
    #[error("source path `{0}` is not a flake (no flake.nix); non-flake mode not yet implemented")]
    NotAFlake(PathBuf),
    #[error("spawning nix-eval-jobs: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("reading nix-eval-jobs output: {0}")]
    Io(#[source] std::io::Error),
    #[error("nix-eval-jobs ({fragment}) exited with status {status:?}\nstderr:\n{stderr}")]
    Subprocess {
        fragment: String,
        status: Option<i32>,
        stderr: String,
    },
    #[error("nix-eval-jobs ({fragment}) timed out after {seconds}s")]
    Timeout { fragment: String, seconds: u64 },
    #[error("parsing nix-eval-jobs output for {fragment}: {source}")]
    Parse {
        fragment: String,
        #[source]
        source: crate::jobspec::ParseError,
    },
}

/// How a flake output is walked by `nix-eval-jobs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FragmentKind {
    /// Walk `<output>.<system>` once per system in the request.
    /// Standard for `packages`, `checks`, `devShells`.
    PerSystem,
    /// Walk `<output>` once with no per-system fan-out and apply
    /// `fn_expr` to each leaf to extract the buildable derivation.
    /// The system is read off the resulting derivation rather than
    /// imposed by the caller — that's how `nixosConfigurations.<name>`
    /// (system-less attribute path, system determined by the module
    /// itself) ends up with a correct `system` field downstream.
    Apply { fn_expr: String },
}

/// One flake output to walk during an evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlakeOutput {
    /// Top-level attribute name, e.g. `packages` or `nixosConfigurations`.
    pub name: String,
    pub kind: FragmentKind,
}

#[derive(Debug, Clone)]
pub struct EvalRequest {
    /// Path to a local checkout containing a `flake.nix`.
    pub source_path: PathBuf,
    /// Systems to evaluate `PerSystem` fragments under, e.g.
    /// `["x86_64-linux"]`. Ignored for `Apply` outputs — those run
    /// once regardless of this list.
    pub systems: Vec<String>,
    /// Top-level flake outputs to walk. Defaults to
    /// [`crate::default_flake_outputs`] when empty.
    pub outputs: Vec<FlakeOutput>,
    /// Wall-clock cap on each `nix-eval-jobs` subprocess.
    pub timeout: Duration,
}

impl EvalRequest {
    pub fn for_local_flake(source_path: PathBuf, systems: Vec<String>) -> Self {
        Self {
            source_path,
            systems,
            outputs: crate::default_flake_outputs(),
            timeout: Duration::from_secs(600),
        }
    }
}

/// Evaluate the source repo under each requested fragment and return the
/// union of jobs.
///
/// Fragments that resolve to a missing flake output (e.g. the flake doesn't
/// expose `checks.aarch64-linux`) just contribute zero jobs — that's normal.
/// We treat a non-zero exit code from nix-eval-jobs as an error on a
/// best-effort basis: empty stderr is interpreted as "missing output" rather
/// than failure, since nix-eval-jobs itself doesn't distinguish them
/// reliably.
pub async fn evaluate(request: &EvalRequest) -> Result<Vec<JobSpec>, EvalError> {
    if !request.source_path.exists() {
        return Err(EvalError::MissingSource(request.source_path.clone()));
    }
    if !request.source_path.join("flake.nix").exists() {
        return Err(EvalError::NotAFlake(request.source_path.clone()));
    }

    let default_outputs: Vec<FlakeOutput>;
    let outputs: &[FlakeOutput] = if request.outputs.is_empty() {
        default_outputs = crate::default_flake_outputs();
        &default_outputs
    } else {
        &request.outputs
    };

    let mut jobs = Vec::new();
    for output in outputs {
        match &output.kind {
            FragmentKind::PerSystem => {
                for system in &request.systems {
                    let fragment = format!("{}.{}", output.name, system);
                    let mut produced =
                        run_one(&request.source_path, &fragment, None, request.timeout).await?;
                    jobs.append(&mut produced);
                }
            }
            FragmentKind::Apply { fn_expr } => {
                let mut produced = run_one(
                    &request.source_path,
                    &output.name,
                    Some(fn_expr.as_str()),
                    request.timeout,
                )
                .await?;
                jobs.append(&mut produced);
            }
        }
    }
    Ok(jobs)
}

async fn run_one(
    source: &Path,
    fragment: &str,
    apply: Option<&str>,
    wall_clock: Duration,
) -> Result<Vec<JobSpec>, EvalError> {
    let flake_uri = format!("{}#{fragment}", source.display());
    tracing::debug!(fragment, %flake_uri, ?apply, "spawning nix-eval-jobs");

    // `--extra-experimental-features` is additive, so we don't override
    // anything an operator already has in `nix.conf`. We just guarantee
    // that the two features `--flake` URIs depend on are on for the
    // spawned subprocess — otherwise argunix breaks on any host whose
    // system-wide nix.conf hasn't been opted in.
    let mut cmd = Command::new("nix-eval-jobs");
    cmd.args(["--extra-experimental-features", "nix-command flakes"])
        .arg("--flake")
        .arg(&flake_uri)
        .arg("--meta");
    if let Some(expr) = apply {
        // `--apply <fn>` runs after each leaf attr is resolved and
        // before nix-eval-jobs decides whether the attr is a
        // derivation. For `nixosConfigurations.<name>` we apply
        // `(x: x.config.system.build.toplevel)` so the system's
        // toplevel derivation gets emitted instead of the
        // configuration attrset itself.
        cmd.arg("--apply").arg(expr);
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(EvalError::Spawn)?;

    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");

    let collect = async {
        let stdout_fut = async {
            let mut buf = String::new();
            stdout.read_to_string(&mut buf).await?;
            Ok::<String, std::io::Error>(buf)
        };
        let stderr_fut = async {
            let mut buf = String::new();
            stderr.read_to_string(&mut buf).await?;
            Ok::<String, std::io::Error>(buf)
        };
        tokio::try_join!(stdout_fut, stderr_fut)
    };

    let (stdout_buf, stderr_buf) = match timeout(wall_clock, collect).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(EvalError::Io(e)),
        Err(_) => {
            let _ = child.start_kill();
            return Err(EvalError::Timeout {
                fragment: fragment.to_string(),
                seconds: wall_clock.as_secs(),
            });
        }
    };

    let status = match timeout(Duration::from_secs(5), child.wait()).await {
        Ok(s) => s.map_err(EvalError::Io)?,
        Err(_) => {
            let _ = child.start_kill();
            return Err(EvalError::Timeout {
                fragment: fragment.to_string(),
                seconds: wall_clock.as_secs(),
            });
        }
    };

    if !status.success() {
        if stderr_indicates_missing_fragment(fragment, &stderr_buf) {
            tracing::debug!(
                fragment,
                "flake does not provide fragment; treating as no jobs"
            );
            return Ok(Vec::new());
        }
        return Err(EvalError::Subprocess {
            fragment: fragment.to_string(),
            status: status.code(),
            stderr: stderr_buf,
        });
    }

    parse_lines(fragment, &stdout_buf).map_err(|e| EvalError::Parse {
        fragment: fragment.to_string(),
        source: e,
    })
}

/// Decide whether a non-zero exit from `nix-eval-jobs` should be treated
/// as "the flake simply doesn't provide this fragment" (zero jobs) or as
/// a real failure.
///
/// Walking the default outputs is a probe — most flakes don't expose
/// every one of `packages` / `checks` / `devShells` for every system,
/// and a missing fragment is normal. We recognise it from two signals:
///
/// - empty stderr (some older nix-eval-jobs paths emit nothing)
/// - a `does not provide attribute '<fragment>'` line, possibly mixed
///   with unrelated warnings like `warning: unknown setting 'allowed-users'`
///   that nix prints ahead of the actual error.
fn stderr_indicates_missing_fragment(fragment: &str, stderr: &str) -> bool {
    if stderr.trim().is_empty() {
        return true;
    }
    let marker = format!("does not provide attribute '{fragment}'");
    stderr.contains(&marker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    /// Stand up a fake `nix-eval-jobs` script in `dir/bin/` that
    /// appends its argv to `dir/argv.txt` (one arg per line, with a
    /// `===` delimiter line between invocations so the test can
    /// distinguish per-fragment calls) and then prints an empty
    /// (zero-jobs) JSON-lines stream so the runner returns Ok([]).
    /// Returns the directory; the caller must keep it alive for the
    /// duration of the test.
    fn fake_nix_eval_jobs_recording_argv(dir: &Path) {
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let script = bin.join("nix-eval-jobs");
        let mut f = std::fs::File::create(&script).unwrap();
        writeln!(
            f,
            r#"#!/bin/sh
out="$(dirname "$0")/../argv.txt"
for a in "$@"; do printf '%s\n' "$a" >> "$out"; done
printf '===\n' >> "$out"
exit 0
"#
        )
        .unwrap();
        let mut perm = std::fs::metadata(&script).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&script, perm).unwrap();
    }

    /// Split an argv.txt body into one Vec<&str> per `===`-delimited
    /// invocation.
    fn split_invocations(argv: &str) -> Vec<Vec<&str>> {
        let mut out = Vec::new();
        let mut current = Vec::new();
        for line in argv.lines() {
            if line == "===" {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            } else {
                current.push(line);
            }
        }
        if !current.is_empty() {
            out.push(current);
        }
        out
    }

    #[tokio::test]
    async fn nix_eval_jobs_argv_per_fragment_kind() {
        let bin_root = tempdir().unwrap();
        fake_nix_eval_jobs_recording_argv(bin_root.path());

        // Need a flake.nix to satisfy the source-existence check, even
        // though our fake binary ignores everything.
        let src = tempdir().unwrap();
        std::fs::write(src.path().join("flake.nix"), b"# stub\n").unwrap();

        let original_path = std::env::var_os("PATH").unwrap_or_default();
        let mut new_path = std::ffi::OsString::from(bin_root.path().join("bin"));
        new_path.push(":");
        new_path.push(&original_path);
        // SAFETY: tests are single-threaded enough for the duration of the
        // spawn; PATH is restored before the test exits.
        // SAFETY: This test relies on no other test mutating PATH concurrently;
        // cargo runs tests in parallel by default, so this could be flaky if
        // we add more PATH-mutating tests. Right now there is exactly one.
        unsafe { std::env::set_var("PATH", &new_path) };

        let request = EvalRequest {
            source_path: src.path().to_path_buf(),
            systems: vec!["x86_64-linux".into()],
            outputs: vec![
                FlakeOutput {
                    name: "packages".into(),
                    kind: FragmentKind::PerSystem,
                },
                FlakeOutput {
                    name: "nixosConfigurations".into(),
                    kind: FragmentKind::Apply {
                        fn_expr: "x: x.config.system.build.toplevel".into(),
                    },
                },
            ],
            timeout: Duration::from_secs(10),
        };
        let result = evaluate(&request).await;

        // Restore PATH before any assertion can panic.
        unsafe { std::env::set_var("PATH", &original_path) };

        result.expect("evaluate() should succeed against the fake");

        let argv =
            std::fs::read_to_string(bin_root.path().join("argv.txt")).expect("fake recorded argv");
        let calls = split_invocations(&argv);
        assert_eq!(
            calls.len(),
            2,
            "expected 2 nix-eval-jobs invocations (1 per-system + 1 apply), got {calls:?}",
        );

        // First call: PerSystem `packages.x86_64-linux`. No --apply.
        let pkgs = &calls[0];
        let flag_idx = pkgs
            .iter()
            .position(|a| *a == "--extra-experimental-features")
            .expect("--extra-experimental-features missing from per-system argv");
        assert_eq!(
            pkgs.get(flag_idx + 1).copied(),
            Some("nix-command flakes"),
            "expected experimental-features value `nix-command flakes` in per-system call, got {pkgs:?}",
        );
        let flake_idx = pkgs
            .iter()
            .position(|a| *a == "--flake")
            .expect("--flake missing from per-system argv");
        assert!(flag_idx < flake_idx, "feature flag must precede --flake");
        assert!(
            pkgs.get(flake_idx + 1)
                .map(|u| u.ends_with("#packages.x86_64-linux"))
                .unwrap_or(false),
            "per-system flake URI should end with #packages.x86_64-linux, got {pkgs:?}",
        );
        assert!(
            !pkgs.iter().any(|a| *a == "--apply"),
            "PerSystem call must NOT pass --apply; got {pkgs:?}",
        );

        // Second call: Apply on `nixosConfigurations`. --apply with the
        // toplevel-extracting function; flake URI without per-system suffix.
        let nixos = &calls[1];
        let apply_idx = nixos
            .iter()
            .position(|a| *a == "--apply")
            .expect("--apply missing from nixosConfigurations argv");
        assert_eq!(
            nixos.get(apply_idx + 1).copied(),
            Some("x: x.config.system.build.toplevel"),
            "expected toplevel-extracting fn after --apply, got {nixos:?}",
        );
        let nixos_flake_idx = nixos
            .iter()
            .position(|a| *a == "--flake")
            .expect("--flake missing from nixosConfigurations argv");
        assert!(
            nixos
                .get(nixos_flake_idx + 1)
                .map(|u| u.ends_with("#nixosConfigurations"))
                .unwrap_or(false),
            "Apply flake URI should end with #nixosConfigurations (no system suffix), got {nixos:?}",
        );
    }

    #[test]
    fn missing_fragment_with_warnings_is_not_a_failure() {
        // Real stderr observed against a flake that doesn't expose
        // devShells: the "does not provide attribute" error sits below
        // unrelated `unknown setting` warnings nix prints first.
        let stderr = "\
warning: unknown setting 'allowed-users'
warning: unknown setting 'trusted-users'
error: flake 'git+file:///var/lib/argunix/work/6?shallow=1' does not provide attribute 'devShells.x86_64-linux'
error: worker error: error: flake 'git+file:///var/lib/argunix/work/6?shallow=1' does not provide attribute 'devShells.x86_64-linux'
";
        assert!(stderr_indicates_missing_fragment(
            "devShells.x86_64-linux",
            stderr
        ));
    }

    #[test]
    fn empty_stderr_is_treated_as_missing_fragment() {
        assert!(stderr_indicates_missing_fragment("checks.x86_64-linux", ""));
        assert!(stderr_indicates_missing_fragment(
            "checks.x86_64-linux",
            "   \n  \t\n"
        ));
    }

    #[test]
    fn real_eval_error_is_not_swallowed() {
        // A genuine evaluation failure (syntax error, undefined var)
        // must still surface as a hard failure even though stderr is
        // non-empty — i.e. the classifier must NOT treat any non-empty
        // stderr as "missing fragment".
        let stderr = "\
error: undefined variable 'foo'
       at /nix/store/.../flake.nix:42:5
";
        assert!(!stderr_indicates_missing_fragment(
            "packages.x86_64-linux",
            stderr
        ));
    }

    #[test]
    fn missing_attribute_for_a_different_fragment_is_not_swallowed() {
        // Defensive: if nix reports a missing attribute that isn't the
        // fragment we asked for, that's a real bug in the flake's eval
        // — don't classify it as "this fragment is just absent".
        let stderr = "\
error: flake '...' does not provide attribute 'packages.x86_64-linux.tool'
";
        assert!(!stderr_indicates_missing_fragment(
            "packages.x86_64-linux",
            stderr
        ));
    }
}
