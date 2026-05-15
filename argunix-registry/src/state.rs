//! On-disk layout for the registry blob pool.
//!
//! ```text
//! <root>/
//!   blobs/
//!     <hex-sha256>            # content-addressed image blobs (layers, configs, manifests)
//!   manifests/
//!     <repo-id>/<job-id>/manifest.json   # per-build manifest pinned by absolute path
//!   tmp/
//!     <unique>/               # scratch dir for in-flight `skopeo copy`
//! ```
//!
//! The blob pool is a flat content-addressed directory: every layer,
//! image config, and per-build manifest is stored exactly once, named
//! by its sha256 hex digest. The `manifests/` subtree records each
//! build's manifest under a stable absolute path so the SQLite row's
//! `manifest_path` is enough to serve it without re-resolving via the
//! blob pool.

use std::path::{Path, PathBuf};

/// Where the registry stores its converted blobs and manifests.
#[derive(Debug, Clone)]
pub struct RegistryState {
    root: PathBuf,
}

impl RegistryState {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn blob_dir(&self) -> PathBuf {
        self.root.join("blobs")
    }

    pub fn manifest_dir(&self) -> PathBuf {
        self.root.join("manifests")
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// Path to a blob by its hex sha256 (without the `sha256:` prefix).
    pub fn blob_path(&self, hex: &str) -> PathBuf {
        self.blob_dir().join(hex)
    }

    /// Per-(repo, job) manifest path. Unique even across rebuilds of
    /// the same attribute on the same sha (job ids never collide).
    pub fn manifest_path(&self, repo_id: i64, job_id: i64) -> PathBuf {
        self.manifest_dir()
            .join(repo_id.to_string())
            .join(job_id.to_string())
            .join("manifest.json")
    }

    /// Create blobs/ manifests/ tmp/ if they don't exist. Idempotent;
    /// safe to call on every daemon start.
    pub async fn ensure_dirs(&self) -> std::io::Result<()> {
        for d in [self.blob_dir(), self.manifest_dir(), self.tmp_dir()] {
            tokio::fs::create_dir_all(&d).await?;
        }
        Ok(())
    }
}
