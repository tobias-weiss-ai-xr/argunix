use std::path::{Path, PathBuf};

/// Per-build log size cap (Q50). Bytes counted at the *raw* stage; the
/// on-disk file is zstd-compressed and may be smaller.
#[derive(Debug, Clone, Copy)]
pub struct LogCaptureLimit {
    pub max_raw_bytes: usize,
}

impl Default for LogCaptureLimit {
    fn default() -> Self {
        // 100 MB raw, the v1 default per `design/questions-answers.md` Q50.
        Self {
            max_raw_bytes: 100 * 1024 * 1024,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LogWriteError {
    #[error("creating log directory `{dir}`: {error}")]
    CreateDir { dir: PathBuf, error: std::io::Error },
    #[error("zstd-encoding log: {0}")]
    Encode(#[source] std::io::Error),
    #[error("writing log file `{path}`: {error}")]
    Write {
        path: PathBuf,
        error: std::io::Error,
    },
    #[error("encode task panicked: {0}")]
    Join(#[from] tokio::task::JoinError),
}

/// Serialise `raw` (already capped by the caller) as a zstd-compressed
/// file at `path`, creating parent directories as needed.
///
/// We do the zstd encode on a blocking task to keep the runtime free of
/// CPU work. v1 doesn't stream — we buffer the entire log in memory at
/// the cap (default 100 MB) and write once.
pub async fn write_zstd_log(path: &Path, raw: Vec<u8>) -> Result<(), LogWriteError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| LogWriteError::CreateDir {
                dir: parent.to_path_buf(),
                error: e,
            })?;
    }

    let encoded = tokio::task::spawn_blocking(move || zstd::encode_all(raw.as_slice(), 3)).await?;
    let encoded = encoded.map_err(LogWriteError::Encode)?;

    tokio::fs::write(path, encoded)
        .await
        .map_err(|e| LogWriteError::Write {
            path: path.to_path_buf(),
            error: e,
        })?;
    Ok(())
}

/// Append a "log truncated" marker to `buf` to make truncation visible to
/// readers without forcing them to compare byte counts.
pub(crate) fn mark_truncated(buf: &mut Vec<u8>) {
    buf.extend_from_slice(b"\n--- log truncated by argunix ---\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trips_through_zstd() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("a/b/log.zst");
        let payload = b"hello, log\n".to_vec();
        write_zstd_log(&log, payload.clone()).await.unwrap();
        let on_disk = std::fs::read(&log).unwrap();
        let decoded = zstd::decode_all(on_disk.as_slice()).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn truncation_marker_is_visible_text() {
        let mut buf = b"some build output".to_vec();
        mark_truncated(&mut buf);
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("--- log truncated by argunix ---"));
    }
}
