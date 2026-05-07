use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

#[derive(Debug, Clone)]
pub struct CacheRef {
    /// The cache URL as accepted by `nix path-info --store <url>`.
    pub url: String,
    /// Whether to consult this cache for substitution. Caches with
    /// `substitute = false` are skipped during the cache-check phase.
    pub substitute: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheCheckResult {
    /// At least one configured cache (with substitute=true) has the path.
    Hit { cache_url: String },
    /// No cache has the path; we'll need to build.
    Miss,
}

#[derive(Debug, thiserror::Error)]
pub enum CacheCheckError {
    #[error("spawning `nix path-info`: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("`nix path-info --store {cache}` timed out after {seconds}s")]
    Timeout { cache: String, seconds: u64 },
    #[error("waiting for `nix path-info`: {0}")]
    Io(#[source] std::io::Error),
}

/// Probe each substitute-enabled cache for `output_path`. The first hit
/// wins. A non-zero exit code from `nix path-info` is treated as
/// "not present" — that's how nix-cli reports cache misses.
pub async fn check_cache(
    output_path: &str,
    caches: &[CacheRef],
    per_call_timeout: Duration,
) -> Result<CacheCheckResult, CacheCheckError> {
    for cache in caches {
        if !cache.substitute {
            continue;
        }
        let mut child = Command::new("nix")
            .arg("path-info")
            .arg("--store")
            .arg(&cache.url)
            .arg(output_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(CacheCheckError::Spawn)?;

        let status = match timeout(per_call_timeout, child.wait()).await {
            Ok(s) => s.map_err(CacheCheckError::Io)?,
            Err(_) => {
                let _ = child.start_kill();
                return Err(CacheCheckError::Timeout {
                    cache: cache.url.clone(),
                    seconds: per_call_timeout.as_secs(),
                });
            }
        };

        if status.success() {
            return Ok(CacheCheckResult::Hit {
                cache_url: cache.url.clone(),
            });
        }
    }
    Ok(CacheCheckResult::Miss)
}
