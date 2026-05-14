use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

/// A binary cache argunix should push successful build outputs to.
///
/// `url` is the nix store URI accepted by `nix copy --to` —
/// `s3://bucket?region=…`, `file:///var/cache/argunix`, `ssh://…`,
/// etc. `signing_key_path` is the nix-format secret key whose path is
/// handed to `nix copy` via the `secret-key` query parameter so the
/// daemon signs the narinfo on upload (a cache that serves unsigned
/// narinfo can't be substituted by clients unless they opt in to
/// `require-sigs=false`, which defeats the purpose).
#[derive(Debug, Clone)]
pub struct PushCache {
    pub url: String,
    pub signing_key_path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum PushError {
    #[error("spawning `nix copy --to {url}`: {source}")]
    Spawn {
        url: String,
        #[source]
        source: std::io::Error,
    },
    #[error("`nix copy --to {url}` timed out after {seconds}s")]
    Timeout { url: String, seconds: u64 },
    #[error("waiting for `nix copy --to {url}`: {source}")]
    Io {
        url: String,
        #[source]
        source: std::io::Error,
    },
    #[error("`nix copy --to {url}` exited {code:?}: {stderr}")]
    Exit {
        url: String,
        code: Option<i32>,
        stderr: String,
    },
}

/// Push `output_paths` and their closure to every configured cache.
///
/// Per-cache failures are returned as a `Vec<PushError>` — callers
/// typically log the failures but keep the job's `Success` status so a
/// flaky cache doesn't poison the build pipeline. An empty `caches`
/// slice or empty `output_paths` is a no-op.
pub async fn push_to_caches(
    output_paths: &[String],
    caches: &[PushCache],
    per_cache_timeout: Duration,
) -> Vec<PushError> {
    let mut errors = Vec::new();
    if output_paths.is_empty() || caches.is_empty() {
        return errors;
    }
    for cache in caches {
        if let Err(e) = push_one(
            &cache.url,
            &cache.signing_key_path,
            output_paths,
            per_cache_timeout,
        )
        .await
        {
            errors.push(e);
        }
    }
    errors
}

async fn push_one(
    url: &str,
    signing_key_path: &Path,
    output_paths: &[String],
    per_cache_timeout: Duration,
) -> Result<(), PushError> {
    let store_uri = with_secret_key(url, signing_key_path);
    let mut cmd = Command::new("nix");
    // `--extra-experimental-features nix-command` is additive — we
    // guarantee the feature is on for this subprocess without
    // overriding anything an operator already has in `nix.conf`,
    // mirroring the eval runner. Without it, `nix copy` aborts with
    // "experimental Nix feature 'nix-command' is disabled" on hosts
    // whose system-wide nix.conf hasn't opted in.
    cmd.args(["--extra-experimental-features", "nix-command"])
        .arg("copy")
        .arg("--to")
        .arg(&store_uri)
        .args(output_paths)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|source| PushError::Spawn {
        url: url.to_string(),
        source,
    })?;

    let mut stderr = child.stderr.take().expect("stderr piped");
    let collect = async {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        stderr.read_to_end(&mut buf).await?;
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((status, buf))
    };

    let (status, stderr_buf) = match timeout(per_cache_timeout, collect).await {
        Ok(Ok(v)) => v,
        Ok(Err(source)) => {
            return Err(PushError::Io {
                url: url.to_string(),
                source,
            });
        }
        Err(_) => {
            return Err(PushError::Timeout {
                url: url.to_string(),
                seconds: per_cache_timeout.as_secs(),
            });
        }
    };

    if status.success() {
        return Ok(());
    }
    Err(PushError::Exit {
        url: url.to_string(),
        code: status.code(),
        stderr: String::from_utf8_lossy(&stderr_buf).to_string(),
    })
}

/// Append `secret-key=<path>` to the store URI's query string.
///
/// `nix copy --to "file:///var/cache?secret-key=/etc/key.sec"` is how
/// nix learns which key to sign uploads with; without this, the cache
/// holds unsigned narinfo and clients reject the substitution unless
/// they explicitly disable `require-sigs`.
fn with_secret_key(url: &str, signing_key_path: &Path) -> String {
    let key = signing_key_path.display();
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}secret-key={key}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn append_secret_key_to_bare_url() {
        let s = with_secret_key("file:///var/cache", &PathBuf::from("/etc/key.sec"));
        assert_eq!(s, "file:///var/cache?secret-key=/etc/key.sec");
    }

    #[test]
    fn append_secret_key_when_query_already_present() {
        let s = with_secret_key(
            "s3://bucket?region=us-east-1",
            &PathBuf::from("/etc/key.sec"),
        );
        assert_eq!(s, "s3://bucket?region=us-east-1&secret-key=/etc/key.sec");
    }

    #[tokio::test]
    async fn no_caches_no_errors() {
        let outputs = vec!["/nix/store/abc".to_string()];
        let caches: Vec<PushCache> = Vec::new();
        let errs = push_to_caches(&outputs, &caches, Duration::from_secs(1)).await;
        assert!(errs.is_empty());
    }

    #[tokio::test]
    async fn no_outputs_no_errors() {
        let outputs: Vec<String> = Vec::new();
        let caches = vec![PushCache {
            url: "file:///tmp/x".to_string(),
            signing_key_path: PathBuf::from("/tmp/key"),
        }];
        let errs = push_to_caches(&outputs, &caches, Duration::from_secs(1)).await;
        assert!(errs.is_empty());
    }
}
