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

    let mut child = Command::new("nix-eval-jobs")
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
