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

#[derive(Debug, Clone)]
pub struct EvalRequest {
    /// Path to a local checkout containing a `flake.nix`.
    pub source_path: PathBuf,
    /// Systems to evaluate fragments under, e.g. `["x86_64-linux"]`.
    pub systems: Vec<String>,
    /// Top-level flake outputs to walk per system. Defaults to
    /// [`crate::DEFAULT_FLAKE_OUTPUTS`] when empty.
    pub outputs: Vec<String>,
    /// Wall-clock cap on each `nix-eval-jobs` subprocess.
    pub timeout: Duration,
}

impl EvalRequest {
    pub fn for_local_flake(source_path: PathBuf, systems: Vec<String>) -> Self {
        Self {
            source_path,
            systems,
            outputs: crate::DEFAULT_FLAKE_OUTPUTS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
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

    let default_outputs: Vec<String>;
    let outputs: &[String] = if request.outputs.is_empty() {
        default_outputs = crate::DEFAULT_FLAKE_OUTPUTS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        &default_outputs
    } else {
        &request.outputs
    };

    let mut jobs = Vec::new();
    for system in &request.systems {
        for output in outputs {
            let fragment = format!("{output}.{system}");
            let mut produced = run_one(&request.source_path, &fragment, request.timeout).await?;
            jobs.append(&mut produced);
        }
    }
    Ok(jobs)
}

async fn run_one(
    source: &Path,
    fragment: &str,
    wall_clock: Duration,
) -> Result<Vec<JobSpec>, EvalError> {
    let flake_uri = format!("{}#{fragment}", source.display());
    tracing::debug!(fragment, %flake_uri, "spawning nix-eval-jobs");

    // `--extra-experimental-features` is additive, so we don't override
    // anything an operator already has in `nix.conf`. We just guarantee
    // that the two features `--flake` URIs depend on are on for the
    // spawned subprocess — otherwise medusa breaks on any host whose
    // system-wide nix.conf hasn't been opted in.
    let mut child = Command::new("nix-eval-jobs")
        .args(["--extra-experimental-features", "nix-command flakes"])
        .arg("--flake")
        .arg(&flake_uri)
        .arg("--meta")
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
        // Heuristic: nix-eval-jobs returns non-zero with empty stderr in
        // some "no such output" cases (e.g. devShells.<system> not
        // provided). Treat that as "no jobs" rather than a hard failure.
        if stderr_buf.trim().is_empty() {
            tracing::debug!(
                fragment,
                "no output from nix-eval-jobs; treating as no jobs"
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    /// Stand up a fake `nix-eval-jobs` script in `dir/bin/` that records
    /// its argv to `dir/argv.txt` (one arg per line, NUL-terminated would
    /// be safer but newlines are fine for the flag we're checking) and
    /// then prints an empty (zero-jobs) JSON-lines stream so the runner
    /// returns Ok([]). Returns the directory; the caller must keep it
    /// alive for the duration of the test.
    fn fake_nix_eval_jobs_recording_argv(dir: &Path) {
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let script = bin.join("nix-eval-jobs");
        let mut f = std::fs::File::create(&script).unwrap();
        // Write argv ($0 plus all args) one per line into argv.txt next
        // to the script.
        writeln!(
            f,
            r#"#!/bin/sh
out="$(dirname "$0")/../argv.txt"
: > "$out"
for a in "$@"; do printf '%s\n' "$a" >> "$out"; done
exit 0
"#
        )
        .unwrap();
        let mut perm = std::fs::metadata(&script).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&script, perm).unwrap();
    }

    #[tokio::test]
    async fn passes_extra_experimental_features_to_nix_eval_jobs() {
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
            outputs: vec!["packages".into()],
            timeout: Duration::from_secs(10),
        };
        let result = evaluate(&request).await;

        // Restore PATH before any assertion can panic.
        unsafe { std::env::set_var("PATH", &original_path) };

        result.expect("evaluate() should succeed against the fake");

        let argv =
            std::fs::read_to_string(bin_root.path().join("argv.txt")).expect("fake recorded argv");
        let lines: Vec<&str> = argv.lines().collect();

        // Expect contiguous "--extra-experimental-features" "nix-command flakes"
        // somewhere in the args, before --flake.
        let flag_idx = lines
            .iter()
            .position(|a| *a == "--extra-experimental-features")
            .expect("--extra-experimental-features missing from argv");
        assert_eq!(
            lines.get(flag_idx + 1).copied(),
            Some("nix-command flakes"),
            "expected experimental-features value `nix-command flakes`, got argv {lines:?}",
        );
        let flake_idx = lines
            .iter()
            .position(|a| *a == "--flake")
            .expect("--flake missing");
        assert!(flag_idx < flake_idx, "feature flag must precede --flake");
    }
}
