//! Closure-transfer subprocess helpers (M14b).
//!
//! Companion to [`crate::side_channel`]: the wire format lives in
//! `side_channel`, the actual `nix-store --export` / `--import`
//! subprocess work lives here. Both daemon and agent use both
//! helpers — the daemon exports drv closures and imports outputs;
//! the agent imports drv closures and exports outputs — so a
//! single home keeps the symmetry visible.
//!
//! Neither helper knows about russh: they take an `AsyncRead` /
//! `AsyncWrite` so unit tests can drive them through
//! `tokio::io::duplex` against fake `nix-store` shell scripts. The
//! caller is responsible for having read or written the
//! [`crate::side_channel::SideChannelHeader`] first.

use crate::protocol::BuildOutcomeStatus;
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Command;

/// Outcome of a closure-import or closure-export subprocess run.
#[derive(Debug)]
pub struct ClosureXferOutcome {
    pub status: BuildOutcomeStatus,
    pub exit_code: Option<i32>,
    /// Captured stderr — small for both `--import` and `--export`,
    /// just error messages on failure. Forward to the daemon as a
    /// `BuildLogChunk` for diagnostics when the import path runs
    /// agent-side, or log directly when the daemon runs it.
    pub stderr: Vec<u8>,
    /// Bytes that crossed the pipe in this transfer's primary
    /// direction (subprocess stdin for import, subprocess stdout
    /// for export). Diagnostic only.
    pub bytes_transferred: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ClosureXferError {
    #[error("spawning `{bin} {op}`: {source}")]
    Spawn {
        bin: String,
        op: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("piping bytes to `nix-store --import` stdin: {0}")]
    StdinPipe(#[source] std::io::Error),
    #[error("piping `nix-store --export` stdout: {0}")]
    StdoutPipe(#[source] std::io::Error),
    #[error("reading nix-store stderr: {0}")]
    StderrRead(#[source] std::io::Error),
    #[error("waiting for nix-store: {0}")]
    Wait(#[source] std::io::Error),
}

/// Run `<nix_store_bin> --import` and pipe `reader` into its stdin
/// until `reader` reports EOF, then await the subprocess and report
/// its exit. Captures stderr in memory for diagnostics.
///
/// Used by:
/// - the agent on a `closure_push` side channel (drv closures from daemon),
/// - the daemon on a `closure_pull` side channel (output paths from agent).
///
/// `kill_on_drop(true)` so a panic / cancel reaps the child cleanly.
/// `nix_store_bin` is taken explicitly (not via `PATH`) so unit
/// tests can inject a fake binary without mutating process-global
/// PATH — the existing `medusa-build::runner` tests pay a
/// `PATH_LOCK` Mutex tax for that and we'd like to avoid it.
pub async fn import_closure<R>(
    nix_store_bin: &Path,
    reader: &mut R,
) -> Result<ClosureXferOutcome, ClosureXferError>
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
        .map_err(|source| ClosureXferError::Spawn {
            bin: nix_store_bin.display().to_string(),
            op: "--import",
            source,
        })?;

    let mut stdin = child.stdin.take().expect("stdin piped");
    let mut stderr = child.stderr.take().expect("stderr piped");

    let pipe_in = async {
        let mut buf = vec![0u8; 64 * 1024];
        let mut total: u64 = 0;
        loop {
            let n = reader
                .read(&mut buf)
                .await
                .map_err(ClosureXferError::StdinPipe)?;
            if n == 0 {
                break;
            }
            stdin
                .write_all(&buf[..n])
                .await
                .map_err(ClosureXferError::StdinPipe)?;
            total += n as u64;
        }
        // Half-close stdin so nix-store --import sees EOF and exits.
        stdin
            .shutdown()
            .await
            .map_err(ClosureXferError::StdinPipe)?;
        drop(stdin);
        Ok::<u64, ClosureXferError>(total)
    };
    let collect_stderr = async {
        let mut buf = Vec::new();
        stderr
            .read_to_end(&mut buf)
            .await
            .map_err(ClosureXferError::StderrRead)?;
        Ok::<Vec<u8>, ClosureXferError>(buf)
    };
    let (bytes_transferred, stderr_buf) = tokio::try_join!(pipe_in, collect_stderr)?;

    let status = child.wait().await.map_err(ClosureXferError::Wait)?;
    Ok(ClosureXferOutcome {
        status: if status.success() {
            BuildOutcomeStatus::Success
        } else {
            BuildOutcomeStatus::Failure
        },
        exit_code: status.code(),
        stderr: stderr_buf,
        bytes_transferred,
    })
}

/// Run `<nix_store_bin> --export <paths...>` and pipe its stdout
/// (the NAR archive of the closure) onto `writer`, then await the
/// subprocess. The `--export` form takes only the paths the caller
/// names; if the closure of those paths is required, the caller
/// must list them — typically via `nix-store --query --requisites`
/// first. (Caller's responsibility because the daemon already
/// computes the requisites set when scheduling.)
///
/// Used by:
/// - the daemon on a `closure_push` side channel (drv closures to agent),
/// - the agent on a `closure_pull` side channel (output paths to daemon).
pub async fn export_closure<W>(
    nix_store_bin: &Path,
    paths: &[String],
    writer: &mut W,
) -> Result<ClosureXferOutcome, ClosureXferError>
where
    W: AsyncWrite + Unpin,
{
    let mut child = Command::new(nix_store_bin)
        .arg("--export")
        .args(paths)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| ClosureXferError::Spawn {
            bin: nix_store_bin.display().to_string(),
            op: "--export",
            source,
        })?;

    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");

    let pipe_out = async {
        let mut buf = vec![0u8; 64 * 1024];
        let mut total: u64 = 0;
        loop {
            let n = stdout
                .read(&mut buf)
                .await
                .map_err(ClosureXferError::StdoutPipe)?;
            if n == 0 {
                break;
            }
            writer
                .write_all(&buf[..n])
                .await
                .map_err(ClosureXferError::StdoutPipe)?;
            total += n as u64;
        }
        writer.flush().await.map_err(ClosureXferError::StdoutPipe)?;
        Ok::<u64, ClosureXferError>(total)
    };
    let collect_stderr = async {
        let mut buf = Vec::new();
        stderr
            .read_to_end(&mut buf)
            .await
            .map_err(ClosureXferError::StderrRead)?;
        Ok::<Vec<u8>, ClosureXferError>(buf)
    };
    let (bytes_transferred, stderr_buf) = tokio::try_join!(pipe_out, collect_stderr)?;

    let status = child.wait().await.map_err(ClosureXferError::Wait)?;
    Ok(ClosureXferOutcome {
        status: if status.success() {
            BuildOutcomeStatus::Success
        } else {
            BuildOutcomeStatus::Failure
        },
        exit_code: status.code(),
        stderr: stderr_buf,
        bytes_transferred,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;
    use tokio::io::BufReader;

    /// Lay down a fake `nix-store` that handles `--import` (cat
    /// stdin to sink) and `--export <paths...>` (record argv +
    /// emit canned bytes from a fixture file).
    fn fake_nix_store(path: &Path, sink_path: &Path, argv_path: &Path, payload_path: &Path) {
        let mut f = std::fs::File::create(path).unwrap();
        writeln!(
            f,
            r#"#!/bin/sh
case "$1" in
  --import)
    cat > "{sink}"
    exit 0
    ;;
  --export)
    : > "{argv}"
    for a in "$@"; do printf '%s\n' "$a" >> "{argv}"; done
    cat "{payload}"
    exit 0
    ;;
esac
exit 99
"#,
            sink = sink_path.display(),
            argv = argv_path.display(),
            payload = payload_path.display(),
        )
        .unwrap();
        f.sync_all().unwrap();
        let mut perm = std::fs::metadata(path).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(path, perm).unwrap();
    }

    fn fake_nix_store_failing(path: &Path, exit_code: i32, stderr_msg: &str) {
        let mut f = std::fs::File::create(path).unwrap();
        writeln!(
            f,
            r#"#!/bin/sh
printf '%s' "{msg}" >&2
exit {code}
"#,
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
    async fn import_pipes_bytes_to_subprocess_stdin_byte_for_byte() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("nix-store");
        let sink = dir.path().join("sink.bin");
        let argv = dir.path().join("argv.txt");
        let payload = dir.path().join("payload.bin");
        std::fs::write(&payload, b"unused-for-import-test").unwrap();
        fake_nix_store(&bin, &sink, &argv, &payload);

        let bytes: Vec<u8> = (0u8..=255).chain(std::iter::once(b'X')).collect();
        let mut reader = BufReader::new(&bytes[..]);
        let outcome = import_closure(&bin, &mut reader).await.unwrap();
        assert_eq!(outcome.status, BuildOutcomeStatus::Success);
        assert_eq!(outcome.bytes_transferred, bytes.len() as u64);
        assert_eq!(std::fs::read(&sink).unwrap(), bytes);
    }

    #[tokio::test]
    async fn import_nonzero_exit_captures_stderr() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("nix-store");
        fake_nix_store_failing(&bin, 1, "error: corrupted NAR");
        let mut reader = BufReader::new(&b"junk"[..]);
        let outcome = import_closure(&bin, &mut reader).await.unwrap();
        assert_eq!(outcome.status, BuildOutcomeStatus::Failure);
        assert!(
            String::from_utf8_lossy(&outcome.stderr).contains("corrupted NAR"),
            "stderr captured for log forwarding",
        );
    }

    #[tokio::test]
    async fn import_missing_binary_typed_error() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("does-not-exist");
        let mut reader = BufReader::new(&b"x"[..]);
        let err = import_closure(&bin, &mut reader).await.unwrap_err();
        assert!(matches!(err, ClosureXferError::Spawn { .. }));
    }

    #[tokio::test]
    async fn export_writes_subprocess_stdout_to_writer_byte_for_byte() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("nix-store");
        let sink = dir.path().join("sink.bin");
        let argv = dir.path().join("argv.txt");
        let payload_path = dir.path().join("payload.bin");
        // Cover NUL + high-bit + boundary bytes again, as the
        // export channel must be opaque-binary clean.
        let payload: Vec<u8> = (0u8..=255).collect();
        std::fs::write(&payload_path, &payload).unwrap();
        fake_nix_store(&bin, &sink, &argv, &payload_path);

        let mut out: Vec<u8> = Vec::new();
        let paths = vec![
            "/nix/store/aaa-foo".to_string(),
            "/nix/store/bbb-bar".to_string(),
        ];
        let outcome = export_closure(&bin, &paths, &mut out).await.unwrap();
        assert_eq!(outcome.status, BuildOutcomeStatus::Success);
        assert_eq!(outcome.bytes_transferred, payload.len() as u64);
        assert_eq!(out, payload, "all bytes from --export stdout reach writer");

        // Argv shape: `--export` followed by the paths, in order.
        let argv_lines = std::fs::read_to_string(&argv).unwrap();
        let lines: Vec<&str> = argv_lines.lines().collect();
        assert_eq!(
            lines,
            vec!["--export", "/nix/store/aaa-foo", "/nix/store/bbb-bar"],
            "subprocess argv must list paths in caller order"
        );
    }

    #[tokio::test]
    async fn export_nonzero_exit_captures_stderr() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("nix-store");
        fake_nix_store_failing(&bin, 2, "error: path not in store");
        let mut out: Vec<u8> = Vec::new();
        let outcome = export_closure(&bin, &["/nix/store/missing".to_string()], &mut out)
            .await
            .unwrap();
        assert_eq!(outcome.status, BuildOutcomeStatus::Failure);
        assert_eq!(outcome.exit_code, Some(2));
        assert!(String::from_utf8_lossy(&outcome.stderr).contains("path not in store"),);
    }

    #[tokio::test]
    async fn export_with_empty_paths_still_runs_and_succeeds() {
        // A degenerate but legal case — `nix-store --export` with
        // no paths emits an empty stream and exits 0. Our fake
        // mirrors that.
        let dir = tempdir().unwrap();
        let bin = dir.path().join("nix-store");
        let sink = dir.path().join("sink.bin");
        let argv = dir.path().join("argv.txt");
        let payload_path = dir.path().join("payload.bin");
        std::fs::write(&payload_path, b"").unwrap();
        fake_nix_store(&bin, &sink, &argv, &payload_path);

        let mut out: Vec<u8> = Vec::new();
        let outcome = export_closure(&bin, &[], &mut out).await.unwrap();
        assert_eq!(outcome.status, BuildOutcomeStatus::Success);
        assert_eq!(outcome.bytes_transferred, 0);
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn export_then_import_round_trips_through_a_pipe() {
        // End-to-end through `tokio::io::duplex`: export side
        // produces canned bytes; import side consumes them. This is
        // the shape the russh wiring will eventually take, just
        // with a real channel instead of duplex.
        let dir = tempdir().unwrap();

        let exporter_bin = dir.path().join("nix-store-exporter");
        let exporter_sink = dir.path().join("exp-sink.bin");
        let exporter_argv = dir.path().join("exp-argv.txt");
        let payload_path = dir.path().join("payload.bin");
        let payload: Vec<u8> = (0u8..200).cycle().take(200_000).collect();
        std::fs::write(&payload_path, &payload).unwrap();
        fake_nix_store(&exporter_bin, &exporter_sink, &exporter_argv, &payload_path);

        let importer_bin = dir.path().join("nix-store-importer");
        let importer_sink = dir.path().join("imp-sink.bin");
        let importer_argv = dir.path().join("imp-argv.txt");
        let importer_payload = dir.path().join("imp-payload.bin");
        std::fs::write(&importer_payload, b"").unwrap();
        fake_nix_store(
            &importer_bin,
            &importer_sink,
            &importer_argv,
            &importer_payload,
        );

        let (rx, mut tx) = tokio::io::duplex(4096);
        let mut rx = BufReader::new(rx);

        // Run both halves concurrently — exactly mirrors the
        // daemon-side encoder + agent-side decoder pair.
        let exporter = tokio::spawn(async move {
            export_closure(&exporter_bin, &["/nix/store/aaa".to_string()], &mut tx).await
        });
        let importer = tokio::spawn(async move { import_closure(&importer_bin, &mut rx).await });

        let exp = exporter.await.unwrap().unwrap();
        let imp = importer.await.unwrap().unwrap();
        assert_eq!(exp.status, BuildOutcomeStatus::Success);
        assert_eq!(imp.status, BuildOutcomeStatus::Success);
        assert_eq!(imp.bytes_transferred, payload.len() as u64);
        assert_eq!(
            std::fs::read(&importer_sink).unwrap(),
            payload,
            "every byte must round-trip exporter→duplex→importer",
        );
    }
}
