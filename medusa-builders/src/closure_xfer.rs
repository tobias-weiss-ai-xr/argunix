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

use crate::channel_io::with_channel_io;
use crate::protocol::BuildOutcomeStatus;
use crate::side_channel::{
    ClosurePushReply, SideChannelError, SideChannelHeader, SideChannelKind, write_header,
};
use russh::Channel;
use russh::server::Msg as ServerMsg;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Command;

/// `Command::spawn` wrapper that retries `ETXTBSY` ("Text file busy")
/// for up to 200 ms before giving up. ETXTBSY happens transiently when
/// a sibling thread's `fork()` inherits a writable fd to a recently-
/// written executable; the kernel closes the inherited fd as soon as
/// that child `exec`s (FD_CLOEXEC) but a few-millisecond window
/// remains. Without this retry, parallel `cargo test` runs across
/// subprocess-heavy tests flake intermittently.
fn spawn_retrying_etxtbsy(cmd: &mut Command) -> std::io::Result<tokio::process::Child> {
    // 26 == ETXTBSY on Linux; stable across kernels. Avoids pulling
    // in `libc` for a single constant. (Stable since 1.83 there is
    // also `ErrorKind::ExecutableFileBusy`, but we keep the raw-os
    // form for the older toolchain on the build server.)
    const ETXTBSY: i32 = 26;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
    loop {
        match cmd.spawn() {
            Ok(child) => return Ok(child),
            Err(e) if e.raw_os_error() == Some(ETXTBSY) && std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

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
    #[error("writing side-channel header: {0}")]
    Header(#[from] SideChannelError),
    #[error("running `nix-store --query --requisites`: {0}")]
    QueryRequisites(#[source] std::io::Error),
    #[error("`nix-store --query --requisites` exited {code:?}: {stderr}")]
    QueryRequisitesFailed { code: Option<i32>, stderr: String },
    /// Agent's `nix-store --import` returned a non-success outcome.
    /// Daemon-side surfaces this when the side-channel reply trailer
    /// from the agent says `ok: false`, OR when a daemon-side IO
    /// error coincides with such a reply (the agent's stderr is the
    /// more diagnostic part). Carries the agent's stderr so an
    /// operator can see the actual nix error in the daemon log /
    /// build log without grepping the agent's journal.
    #[error(
        "agent `nix-store --import` failed: exit_code={exit_code:?}, \
         bytes_received={bytes_received}; agent stderr:\n{stderr}"
    )]
    AgentImportFailed {
        exit_code: Option<i32>,
        bytes_received: u64,
        stderr: String,
    },
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
    let mut cmd = Command::new(nix_store_bin);
    cmd.arg("--import")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = spawn_retrying_etxtbsy(&mut cmd).map_err(|source| ClosureXferError::Spawn {
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
    let mut cmd = Command::new(nix_store_bin);
    cmd.arg("--export")
        .args(paths)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = spawn_retrying_etxtbsy(&mut cmd).map_err(|source| ClosureXferError::Spawn {
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

/// Compute the closure (transitive `--requisites`) of `paths` by
/// shelling out to `<nix_store_bin> --query --requisites <paths...>`.
///
/// Two callers, one helper:
///
/// - **Daemon, pre-push:** expands the *drv* path to every input drv
///   and source the agent needs to be able to run `--realise`.
/// - **Agent, pre-pull:** expands the build *output* paths to their
///   full runtime closure. Critical: `nix-store --export` ships only
///   the listed paths, not their references. Without expanding here,
///   any output that picks up a runtime dep via substitution during
///   the build (e.g. an OVMF / glibc / busybox path the build pulled
///   from cache.nixos.org) would arrive on the daemon side as an
///   unimportable orphan. See the regression that surfaced as
///   `error: path '/nix/store/...-OVMF-202602-fd' is not valid`.
pub async fn query_requisites(
    nix_store_bin: &Path,
    paths: &[String],
) -> Result<Vec<String>, ClosureXferError> {
    let mut cmd = Command::new(nix_store_bin);
    cmd.arg("--query").arg("--requisites");
    for p in paths {
        cmd.arg(p);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = spawn_retrying_etxtbsy(&mut cmd).map_err(ClosureXferError::QueryRequisites)?;
    let output = child
        .wait_with_output()
        .await
        .map_err(ClosureXferError::QueryRequisites)?;
    if !output.status.success() {
        return Err(ClosureXferError::QueryRequisitesFailed {
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// Daemon-side: open the supplied russh channel, write a `ClosurePush`
/// header and then stream `<nix_store_bin> --export <paths>` bytes
/// onto the channel. The agent's [`crate::dispatch_inbound`] decodes
/// the header and pipes the rest into its own `nix-store --import`.
///
/// Takes ownership of `channel` and closes it cleanly on return.
pub async fn push_closure_over_channel(
    channel: Channel<ServerMsg>,
    build_id: i64,
    paths: Vec<String>,
    nix_store_bin: &Path,
) -> Result<ClosureXferOutcome, ClosureXferError> {
    let header = SideChannelHeader {
        kind: SideChannelKind::ClosurePush,
        build_id,
        paths: paths.clone(),
    };
    let nix_store_bin = nix_store_bin.to_path_buf();
    let outcome = with_channel_io(channel, None, |io| async move {
        let (mut reader, mut writer) = tokio::io::split(io);
        write_header(&mut writer, &header).await?;
        // Capture the export result without propagating yet — we
        // want to read the agent's reply trailer even if our own
        // write half broke (BrokenPipe on the duplex usually means
        // the agent's import exited and closed the channel; the
        // agent's reply tells us *why*).
        let export_result = export_closure(&nix_store_bin, &paths, &mut writer).await;
        // Drop our writer so the channel pump signals EOF on the
        // remote side (agent's `nix-store --import` exits on EOF).
        drop(writer);

        // Read the agent's reply trailer. Best-effort with a
        // generous timeout — a healthy agent replies promptly; a
        // dead one is bounded by the timeout.
        let mut reply_buf = Vec::new();
        let _ =
            tokio::time::timeout(Duration::from_secs(60), reader.read_to_end(&mut reply_buf)).await;
        let reply = first_json_line::<ClosurePushReply>(&reply_buf);

        match (export_result, reply) {
            (Ok(_o), Some(r)) if !r.ok => Err(ClosureXferError::AgentImportFailed {
                exit_code: r.exit_code,
                bytes_received: r.bytes_received,
                stderr: r.stderr,
            }),
            (Ok(o), _) => Ok(o),
            (Err(daemon_err), Some(r)) if !r.ok => {
                // Both sides errored. The agent's stderr is more
                // diagnostic than the daemon's IO error; surface it
                // and append the daemon-side detail.
                Err(ClosureXferError::AgentImportFailed {
                    exit_code: r.exit_code,
                    bytes_received: r.bytes_received,
                    stderr: format!("{}\n[daemon-side IO error: {}]", r.stderr, daemon_err),
                })
            }
            (Err(e), _) => Err(e),
        }
    })
    .await;
    outcome
}

/// Parse the first newline-terminated JSON object out of `buf`,
/// silently returning None on any error. Used to be tolerant of
/// agent versions that emit no trailer or mangled trailers.
fn first_json_line<T: serde::de::DeserializeOwned>(buf: &[u8]) -> Option<T> {
    let line = buf.split(|&b| b == b'\n').next()?;
    if line.is_empty() {
        return None;
    }
    serde_json::from_slice(line).ok()
}

/// Daemon-side: open the supplied russh channel, write a `ClosurePull`
/// header asking the agent to export `paths`, and pipe the agent's
/// stdout (the NAR archive) into a local `<nix_store_bin> --import`
/// subprocess. Used to materialise a builder's output paths into the
/// daemon's local store after a successful build.
pub async fn pull_closure_over_channel(
    channel: Channel<ServerMsg>,
    build_id: i64,
    paths: Vec<String>,
    nix_store_bin: &Path,
) -> Result<ClosureXferOutcome, ClosureXferError> {
    let header = SideChannelHeader {
        kind: SideChannelKind::ClosurePull,
        build_id,
        paths: paths.clone(),
    };
    let nix_store_bin = nix_store_bin.to_path_buf();
    let outcome = with_channel_io(channel, None, |io| async move {
        let (mut reader, mut writer) = tokio::io::split(io);
        write_header(&mut writer, &header).await?;
        // Half-close our write side so the agent doesn't wait for
        // more bytes after seeing the header. (russh's `Channel::eof`
        // would do this on the channel; here we just stop writing —
        // the channel stays open for the agent's stdout to flow back.)
        let _ = writer.flush().await;
        drop(writer);
        // Stream the agent's `--export` stdout straight into our
        // local `nix-store --import`.
        let outcome = import_closure(&nix_store_bin, &mut reader).await?;
        Ok::<ClosureXferOutcome, ClosureXferError>(outcome)
    })
    .await;
    outcome
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
    /// Atomically install an executable script at `path`. Writes to a
    /// sibling `.tmp` path (chmod'd while still under that name) and
    /// renames into place — so the final path never had a writable
    /// fd opened on it. Without this, a sibling thread's fork() can
    /// briefly inherit our writable fd; the child then exec's *its*
    /// own script and Linux returns ETXTBSY because the inherited fd
    /// (still pointing at our path) is now seen as in-use.
    fn install_script_atomic(path: &Path, body: &str) {
        let tmp = path.with_extension("tmp");
        {
            let mut f = std::fs::File::create(&tmp).unwrap();
            f.write_all(body.as_bytes()).unwrap();
            f.sync_all().unwrap();
        }
        let mut perm = std::fs::metadata(&tmp).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&tmp, perm).unwrap();
        std::fs::rename(&tmp, path).unwrap();
    }

    fn fake_nix_store(path: &Path, sink_path: &Path, argv_path: &Path, payload_path: &Path) {
        let body = format!(
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
        );
        install_script_atomic(path, &body);
    }

    fn fake_nix_store_failing(path: &Path, exit_code: i32, stderr_msg: &str) {
        let body = format!(
            "#!/bin/sh\nprintf '%s' \"{msg}\" >&2\nexit {code}\n",
            msg = stderr_msg,
            code = exit_code,
        );
        install_script_atomic(path, &body);
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
