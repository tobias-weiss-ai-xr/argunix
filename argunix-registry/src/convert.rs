//! `skopeo copy docker-archive:<store-path> dir:<scratch>` plus the
//! follow-up move of per-blob files into the content-addressed pool.
//!
//! `skopeo` writes the OCI/distribution layout to a directory:
//!
//! ```text
//! <scratch>/
//!   manifest.json     # the v2 manifest (or OCI), references blobs by sha256:<hex>
//!   version           # "Directory Transport Version: 1.1\n"
//!   <hex>             # one file per layer/config blob, named by hex sha256
//! ```
//!
//! After `skopeo copy` we:
//!
//! 1. Hash the manifest bytes — that's the `Docker-Content-Digest`
//!    served back to clients on manifest GETs/HEADs.
//! 2. Move every `<hex>` blob from the scratch dir into the shared
//!    blob pool, using `rename` (atomic when same filesystem). A
//!    duplicate blob is a no-op — content-addressed, two builds of
//!    the same layer share storage.
//! 3. Move the manifest into its per-build path under `manifests/`,
//!    AND store a copy in the blob pool keyed by its digest so that
//!    `GET /v2/<name>/manifests/sha256:<hex>` requests can be served
//!    by digest lookup against the same pool.

use crate::state::RegistryState;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum ConvertError {
    #[error("spawning skopeo: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("skopeo copy exited {status:?}\nstderr:\n{stderr}")]
    Skopeo { status: Option<i32>, stderr: String },
    #[error("filesystem error in {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("scratch dir produced no manifest.json")]
    MissingManifest,
}

/// What the conversion produced.
#[derive(Debug, Clone)]
pub struct Converted {
    /// `sha256:<hex>` of the manifest bytes.
    pub manifest_digest: String,
    /// Absolute path to the per-build manifest under `manifests/`.
    pub manifest_path: PathBuf,
}

/// Run `skopeo copy docker-archive:<archive> dir:<scratch>`, then move
/// the produced blobs into `state`'s blob pool and the manifest into
/// `state`'s per-build manifest path. Returns the manifest digest +
/// final on-disk path so the caller can persist them.
///
/// The `archive` is typically a nix store path produced by
/// `dockerTools.{buildImage,buildLayeredImage}` — the tarball is at
/// the path itself. `repo_id` and `job_id` together name the manifest's
/// home under `manifests/<repo_id>/<job_id>/manifest.json`.
pub async fn convert(
    state: &RegistryState,
    archive: &Path,
    repo_id: i64,
    job_id: i64,
) -> Result<Converted, ConvertError> {
    state.ensure_dirs().await.map_err(|e| ConvertError::Io {
        path: state.root().to_path_buf(),
        source: e,
    })?;

    // Use a job-scoped scratch dir so concurrent conversions don't collide.
    // Removed at the end (best effort).
    let scratch = state.tmp_dir().join(format!("convert-{repo_id}-{job_id}"));
    if scratch.exists() {
        tokio::fs::remove_dir_all(&scratch)
            .await
            .map_err(|e| ConvertError::Io {
                path: scratch.clone(),
                source: e,
            })?;
    }
    tokio::fs::create_dir_all(&scratch)
        .await
        .map_err(|e| ConvertError::Io {
            path: scratch.clone(),
            source: e,
        })?;

    let src = format!("docker-archive:{}", archive.display());
    let dst = format!("dir:{}", scratch.display());
    // `--insecure-policy` skips containers/image's trust policy
    // evaluation. We're copying argunix's own freshly-built nix
    // store output — there is no upstream signer whose key we
    // could check against, so the policy lookup would only either
    // reject everything (no policy.json on the host) or fall
    // through to insecureAcceptAnything anyway. Skipping the
    // lookup avoids requiring a /etc/containers/policy.json on
    // every deployment.
    let out = Command::new("skopeo")
        .args(["--insecure-policy", "copy", &src, &dst])
        .output()
        .await
        .map_err(ConvertError::Spawn)?;
    if !out.status.success() {
        return Err(ConvertError::Skopeo {
            status: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }

    let manifest_src = scratch.join("manifest.json");
    let manifest_bytes = tokio::fs::read(&manifest_src).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ConvertError::MissingManifest
        } else {
            ConvertError::Io {
                path: manifest_src.clone(),
                source: e,
            }
        }
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&manifest_bytes);
    let manifest_hex = hex::encode(hasher.finalize());
    let manifest_digest = format!("sha256:{manifest_hex}");

    // Move every non-manifest, non-version file into the blob pool. They
    // are already named by hex digest by skopeo's `dir:` transport.
    let mut entries = tokio::fs::read_dir(&scratch)
        .await
        .map_err(|e| ConvertError::Io {
            path: scratch.clone(),
            source: e,
        })?;
    while let Some(entry) = entries.next_entry().await.map_err(|e| ConvertError::Io {
        path: scratch.clone(),
        source: e,
    })? {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str == "manifest.json" || name_str == "version" {
            continue;
        }
        let dst = state.blob_path(&name_str);
        move_or_replace(&entry.path(), &dst).await?;
    }

    // Also pin the manifest in the blob pool by its digest, so digest
    // lookups (`/v2/<name>/manifests/sha256:<hex>`) can resolve via
    // the blob pool without a manifest_path indirection.
    let blob_manifest = state.blob_path(&manifest_hex);
    if !blob_manifest.exists() {
        tokio::fs::write(&blob_manifest, &manifest_bytes)
            .await
            .map_err(|e| ConvertError::Io {
                path: blob_manifest.clone(),
                source: e,
            })?;
    }

    // And stash the per-build manifest under manifests/<repo>/<job>/.
    let manifest_path = state.manifest_path(repo_id, job_id);
    if let Some(parent) = manifest_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| ConvertError::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
    }
    tokio::fs::write(&manifest_path, &manifest_bytes)
        .await
        .map_err(|e| ConvertError::Io {
            path: manifest_path.clone(),
            source: e,
        })?;

    let _ = tokio::fs::remove_dir_all(&scratch).await;

    Ok(Converted {
        manifest_digest,
        manifest_path,
    })
}

/// Move src→dst, falling back to copy+remove when rename fails (cross-fs).
/// If dst already exists (same content addressed name), src is just removed.
async fn move_or_replace(src: &Path, dst: &Path) -> Result<(), ConvertError> {
    if dst.exists() {
        let _ = tokio::fs::remove_file(src).await;
        return Ok(());
    }
    match tokio::fs::rename(src, dst).await {
        Ok(()) => Ok(()),
        Err(_) => {
            tokio::fs::copy(src, dst)
                .await
                .map_err(|e| ConvertError::Io {
                    path: dst.to_path_buf(),
                    source: e,
                })?;
            tokio::fs::remove_file(src)
                .await
                .map_err(|e| ConvertError::Io {
                    path: src.to_path_buf(),
                    source: e,
                })?;
            Ok(())
        }
    }
}
