//! Agent-side handler for `SideChannelKind::ClosurePush` (M14b).
//!
//! When the daemon opens a side channel and writes a
//! `closure_push` header, the agent pipes everything that follows
//! into `nix-store --import` on the local host. That subprocess
//! reads a `nix-store --export` byte stream from stdin and
//! materialises the contained store paths into the local nix
//! store. The drv (and its transitive input closure) is then
//! present and a subsequent `Build` message can ask for it to be
//! realised.
//!
//! This module is the agent's nix-side glue. It does **not** know
//! about russh — it works with any `AsyncRead` so unit tests can
//! drive it through `tokio::io::duplex` against a fake nix-store.
//! The russh wiring (read header, dispatch on kind, hand the
//! channel to this function) lands in a follow-up slice when we
//! finalise the side-channel-vs-legacy dispatch on the agent.

use medusa_builders::BuildOutcomeStatus;
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

#[derive(Debug)]
pub struct ImportOutcome {
    pub status: BuildOutcomeStatus,
    pub exit_code: Option<i32>,
    /// Captured stderr from `nix-store --import` (small — the import
    /// path doesn't normally chatter, just emits errors).
    pub stderr: Vec<u8>,
    /// Bytes read from `reader` and forwarded to the subprocess
    /// stdin. Diagnostic only.
    pub bytes_imported: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("spawning `{bin} --import`: {source}")]
    Spawn {
        bin: String,
        #[source]
        source: std::io::Error,
    },
    #[error("piping closure into `nix-store --import` stdin: {0}")]
    StdinPipe(#[source] std::io::Error),
    #[error("reading `nix-store --import` stderr: {0}")]
    StderrRead(#[source] std::io::Error),
    #[error("waiting for `nix-store --import`: {0}")]
    Wait(#[source] std::io::Error),
}

/// Run `<nix_store_bin> --import` and pipe `reader` into its stdin
/// until `reader` reports EOF, then await the subprocess and report
/// its exit. Captures stderr in memory so the caller can forward it
/// to the daemon as a `BuildLogChunk` for diagnostics.
///
/// The subprocess is spawned with `kill_on_drop(true)` so a panic /
/// cancel on the agent side reaps the child cleanly.
///
/// `nix_store_bin` is taken explicitly (rather than relying on
/// `PATH`) so unit tests can hand in a fake binary without mutating
/// process-global PATH — the existing `medusa-build::runner` tests
/// pay a `PATH_LOCK` Mutex tax for that and we'd like to avoid it.
pub async fn import_closure<R>(
    nix_store_bin: &Path,
    reader: &mut R,
) -> Result<ImportOutcome, ImportError>
where
    R: AsyncRead + Unpin,
{
    let mut child = Command::new(nix_store_bin)
        .arg("--import")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| ImportError::Spawn {
            bin: nix_store_bin.display().to_string(),
            source,
        })?;

    let mut stdin = child.stdin.take().expect("stdin piped");
    let mut stderr = child.stderr.take().expect("stderr piped");

    // Pipe reader → subprocess stdin and stderr → in-memory buffer
    // concurrently; either ends when its source EOFs / errors.
    let pipe_in = async {
        let mut buf = vec![0u8; 64 * 1024];
        let mut total: u64 = 0;
        loop {
            let n = reader
                .read(&mut buf)
                .await
                .map_err(ImportError::StdinPipe)?;
            if n == 0 {
                break;
            }
            stdin
                .write_all(&buf[..n])
                .await
                .map_err(ImportError::StdinPipe)?;
            total += n as u64;
        }
        // Half-close stdin so nix-store --import sees EOF and exits.
        stdin.shutdown().await.map_err(ImportError::StdinPipe)?;
        drop(stdin);
        Ok::<u64, ImportError>(total)
    };
    let collect_stderr = async {
        let mut buf = Vec::new();
        stderr
            .read_to_end(&mut buf)
            .await
            .map_err(ImportError::StderrRead)?;
        Ok::<Vec<u8>, ImportError>(buf)
    };
    let (bytes_imported, stderr_buf) = tokio::try_join!(pipe_in, collect_stderr)?;

    let status = child.wait().await.map_err(ImportError::Wait)?;
    Ok(ImportOutcome {
        status: if status.success() {
            BuildOutcomeStatus::Success
        } else {
            BuildOutcomeStatus::Failure
        },
        exit_code: status.code(),
        stderr: stderr_buf,
        bytes_imported,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;
    use tokio::io::BufReader;

    /// Lay down a fake `nix-store` shell script at `path` that, on
    /// `--import`, copies stdin to a sink file and exits with the
    /// given status. Lets us assert exact bytes piped through.
    fn fake_nix_store(path: &Path, sink_path: &Path, exit_code: i32, stderr_msg: &str) {
        let mut f = std::fs::File::create(path).unwrap();
        writeln!(
            f,
            r#"#!/bin/sh
if [ "$1" = "--import" ]; then
  cat > "{sink}"
  printf '%s' "{msg}" >&2
  exit {code}
fi
exit 99
"#,
            sink = sink_path.display(),
            msg = stderr_msg,
            code = exit_code,
        )
        .unwrap();
        f.sync_all().unwrap();
        let mut perm = std::fs::metadata(path).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(path, perm).unwrap();
    }

    #[tokio::test]
    async fn pipes_stdin_to_nix_store_import_byte_for_byte() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("nix-store");
        let sink = dir.path().join("sink.bin");
        fake_nix_store(&bin, &sink, 0, "");

        // Synthetic payload: 257 bytes including a NUL and high-bit
        // bytes, to confirm we don't accidentally treat the stream
        // as text.
        let payload: Vec<u8> = (0u8..=255).chain(std::iter::once(b'X')).collect();
        let mut reader = BufReader::new(&payload[..]);
        let outcome = import_closure(&bin, &mut reader)
            .await
            .expect("import succeeds");
        assert_eq!(outcome.status, BuildOutcomeStatus::Success);
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.bytes_imported, payload.len() as u64);

        let written = std::fs::read(&sink).unwrap();
        assert_eq!(
            written, payload,
            "every byte must reach nix-store --import stdin unchanged",
        );
    }

    #[tokio::test]
    async fn nonzero_exit_surfaces_as_failure_with_stderr_captured() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("nix-store");
        let sink = dir.path().join("sink.bin");
        fake_nix_store(&bin, &sink, 1, "error: corrupted NAR");

        let mut reader = BufReader::new(&b"junk"[..]);
        let outcome = import_closure(&bin, &mut reader)
            .await
            .expect("import call itself completes");
        assert_eq!(outcome.status, BuildOutcomeStatus::Failure);
        assert_eq!(outcome.exit_code, Some(1));
        assert!(
            String::from_utf8_lossy(&outcome.stderr).contains("corrupted NAR"),
            "stderr must be captured for log forwarding; got {:?}",
            String::from_utf8_lossy(&outcome.stderr),
        );
    }

    #[tokio::test]
    async fn missing_binary_returns_spawn_error_not_panic() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("does-not-exist");
        let mut reader = BufReader::new(&b"x"[..]);
        let err = import_closure(&bin, &mut reader)
            .await
            .expect_err("missing binary must fail");
        assert!(
            matches!(err, ImportError::Spawn { .. }),
            "expected Spawn error, got {err:?}",
        );
    }

    #[tokio::test]
    async fn empty_payload_still_runs_import_and_exits_cleanly() {
        // A defensive case: if the daemon opens a side channel and
        // closes it without sending payload bytes, our handler
        // should still spawn `nix-store --import`, hand it EOF, and
        // report whatever it exits with. The fake script `cat`s
        // stdin to sink (yielding empty file) and exits 0.
        let dir = tempdir().unwrap();
        let bin = dir.path().join("nix-store");
        let sink = dir.path().join("sink.bin");
        fake_nix_store(&bin, &sink, 0, "");

        let empty: &[u8] = &[];
        let mut reader = BufReader::new(empty);
        let outcome = import_closure(&bin, &mut reader)
            .await
            .expect("import completes on empty input");
        assert_eq!(outcome.bytes_imported, 0);
        assert_eq!(outcome.status, BuildOutcomeStatus::Success);
        assert_eq!(std::fs::read(&sink).unwrap(), Vec::<u8>::new());
    }
}
