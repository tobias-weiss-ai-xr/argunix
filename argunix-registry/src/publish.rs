//! Entrypoint called by the worker after `BuildStatus::Success` for a
//! docker-image job. Mirrors the failure policy of `binary_caches`
//! pushing: errors are logged, never failing the job.

use crate::convert::{self, ConvertError};
use crate::state::RegistryState;
use argunix_domain::{EvalId, JobId, RepoId, Sha};
use argunix_store::{DockerImageStore, NewDockerImage, SqlxStore, StoreError};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("converting docker archive: {0}")]
    Convert(#[from] ConvertError),
    #[error("recording docker image: {0}")]
    Store(#[from] StoreError),
    #[error("missing build output path")]
    NoOutput,
}

/// Inputs to one publish call. Sourced from the JobSpec + the
/// build outcome the worker has just observed.
#[derive(Clone)]
pub struct PublishRequest<'a> {
    pub state: &'a RegistryState,
    pub store: &'a SqlxStore,
    pub repo_id: RepoId,
    pub eval_id: EvalId,
    pub job_id: JobId,
    /// Forge slug, e.g. `"github"`. Goes into the image name's first
    /// path segment.
    pub forge: &'a str,
    /// Repo slug, e.g. `"tfc/argunix"`. Goes into the image name's
    /// second + third segments.
    pub repo_slug: &'a str,
    /// Last component of the JobSpec's attr_path, lowercased.
    /// e.g. `my-image` from `packages.x86_64-linux.my-image`.
    pub attr_leaf: &'a str,
    pub system: &'a str,
    pub git_ref: &'a str,
    pub sha: &'a Sha,
    /// Build's primary output path — the docker-archive tarball on disk.
    pub output_path: Option<&'a str>,
}

/// Convert + persist. Best-effort: callers should log the error and
/// keep the job in `Success`.
pub async fn publish(req: PublishRequest<'_>) -> Result<(), PublishError> {
    let output = req.output_path.ok_or(PublishError::NoOutput)?;
    let archive = Path::new(output);

    let converted =
        convert::convert(req.state, archive, req.repo_id.get(), req.job_id.get()).await?;

    let image_name = format!("{}/{}/{}", req.forge, req.repo_slug, req.attr_leaf);

    <SqlxStore as DockerImageStore>::create(
        req.store,
        NewDockerImage {
            repo_id: req.repo_id,
            eval_id: req.eval_id,
            job_id: req.job_id,
            image_name,
            system: req.system.to_string(),
            git_ref: req.git_ref.to_string(),
            sha: req.sha.clone(),
            manifest_digest: converted.manifest_digest,
            manifest_path: converted.manifest_path.to_string_lossy().into_owned(),
        },
    )
    .await?;

    Ok(())
}

/// Strip a `packages.<system>.` (or `dockerImages.<system>.`) prefix
/// from `attr_path` and return the trailing leaf, lowercased and with
/// any remaining dots replaced by `-` so it fits a docker image name
/// path segment.
pub fn attr_leaf(attr_path: &str) -> String {
    let trimmed = attr_path.splitn(3, '.').nth(2).unwrap_or(attr_path);
    trimmed.to_ascii_lowercase().replace('.', "-")
}

#[cfg(test)]
mod tests {
    use super::attr_leaf;

    #[test]
    fn strips_packages_prefix() {
        assert_eq!(attr_leaf("packages.x86_64-linux.my-image"), "my-image");
    }

    #[test]
    fn handles_nested_attr() {
        assert_eq!(
            attr_leaf("packages.x86_64-linux.suite.thing"),
            "suite-thing"
        );
    }

    #[test]
    fn falls_back_to_full_path_when_too_short() {
        assert_eq!(attr_leaf("just-a-name"), "just-a-name");
    }
}
