use argunix_domain::{EvalId, JobId, RepoId};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum GcRootError {
    #[error("spawning `nix-store --add-root`: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("creating gc-root directory `{dir}`: {error}")]
    CreateDir { dir: PathBuf, error: std::io::Error },
    #[error("`nix-store --add-root {root}` exited with status {status:?}\nstderr:\n{stderr}")]
    NonZero {
        root: PathBuf,
        status: Option<i32>,
        stderr: String,
    },
    #[error("waiting for `nix-store --add-root`: {0}")]
    Io(#[source] std::io::Error),
}

/// Construct a per-job GC-root path under `base_dir`. The schema is
/// `<base>/<repo>/<eval>/<job>`, matching the layout in `design/plan.md`
/// (M3 / Q47).
pub fn gc_root_path(base_dir: &Path, repo: RepoId, eval: EvalId, job: JobId) -> PathBuf {
    base_dir
        .join(repo.get().to_string())
        .join(eval.get().to_string())
        .join(job.get().to_string())
}

/// Add an *indirect* GC root at `root_path` pointing to `output_path`. Only
/// successful builds get roots (Q48); failed builds leave the log only.
///
/// The directory containing `root_path` is created first. We delegate the
/// actual symlink to `nix-store --add-root --indirect <output>` rather than
/// manage the symlink ourselves so nix's gc respects it correctly.
pub async fn add_gc_root(root_path: &Path, output_path: &str) -> Result<(), GcRootError> {
    if let Some(parent) = root_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| GcRootError::CreateDir {
                dir: parent.to_path_buf(),
                error: e,
            })?;
    }

    let output = Command::new("nix-store")
        .arg("--add-root")
        .arg(root_path)
        .arg("--indirect")
        .arg("--realise")
        .arg(output_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(GcRootError::Spawn)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(GcRootError::NonZero {
            root: root_path.to_path_buf(),
            status: output.status.code(),
            stderr,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gc_root_path_layout() {
        let p = gc_root_path(
            Path::new("/nix/var/nix/gcroots/per-user/argunix"),
            RepoId::new(7),
            EvalId::new(42),
            JobId::new(123),
        );
        assert_eq!(
            p,
            PathBuf::from("/nix/var/nix/gcroots/per-user/argunix/7/42/123"),
        );
    }
}
