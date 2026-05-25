use crate::log_capture::{LogCaptureLimit, LogWriteError, mark_truncated, write_zstd_log};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("spawning `nix-store --realise`: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("`nix-store --realise {drv}` timed out after {seconds}s")]
    Timeout { drv: String, seconds: u64 },
    #[error("waiting for `nix-store --realise`: {0}")]
    Io(#[source] std::io::Error),
    #[error("writing build log: {0}")]
    Log(#[from] LogWriteError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildStatus {
    Success,
    Failure,
}

#[derive(Debug, Clone)]
pub struct BuildOutcome {
    pub status: BuildStatus,
    pub exit_code: Option<i32>,
    /// Output paths printed by `nix-store --realise` on success, in the
    /// order nix-store reported them. Empty on failure.
    pub output_paths: Vec<String>,
    /// Where the log file was written (always — both successful and failed
    /// builds get a log saved).
    pub log_path: PathBuf,
    /// Whether the log was truncated to fit within the size cap.
    pub log_truncated: bool,
}

#[derive(Debug, Clone)]
pub struct BuildRequest {
    pub drv_path: String,
    pub log_path: PathBuf,
    pub timeout: Duration,
    pub log_limit: LogCaptureLimit,
    /// Optional GC-root path. When set, passed as `--add-root <path>` to
    /// `nix-store --realise` so the build output is registered as a root
    /// atomically with the build, and nix-store stays quiet (without it,
    /// nix-store warns "you did not specify '--add-root'…" on every run).
    /// The caller is responsible for ensuring the parent directory exists.
    pub gc_root: Option<PathBuf>,
}

/// Spawn `nix-store --realise <drv>`, capture stdout (output paths) and
/// stderr (build log), and return a [`BuildOutcome`] with status and on-disk
/// log path. The caller is responsible for translating success → `Success`
/// or `Cached` and for adding the GC root.
pub async fn run_build(request: &BuildRequest) -> Result<BuildOutcome, BuildError> {
    tracing::debug!(drv = %request.drv_path, "spawning nix-store --realise");

    let mut cmd = Command::new("nix-store");
    cmd.arg("--realise");
    // If a substituter download fails mid-stream (e.g. S3-style cache
    // returning HTTP/2 stream errors or 504 on a range-resume), build
    // from source rather than failing the job — nix itself points users
    // at `--fallback` in that error.
    cmd.arg("--fallback");
    // `internal-json` makes nix emit structured `@nix {…}` activity
    // events on stderr — `argunix-nom` parses them into per-derivation
    // log lines. Output paths still go to stdout, unaffected.
    cmd.arg("--log-format").arg("internal-json");
    if let Some(root) = &request.gc_root {
        cmd.arg("--add-root").arg(root);
    }
    let mut child = cmd
        .arg(&request.drv_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(BuildError::Spawn)?;

    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");
    let cap = request.log_limit.max_raw_bytes;

    let collect = async {
        let stdout_fut = async {
            let mut buf = Vec::new();
            stdout.read_to_end(&mut buf).await?;
            Ok::<Vec<u8>, std::io::Error>(buf)
        };
        let stderr_fut = async { collect_capped(&mut stderr, cap).await };
        tokio::try_join!(stdout_fut, stderr_fut)
    };

    let (stdout_buf, (stderr_buf, log_truncated)) = match timeout(request.timeout, collect).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(BuildError::Io(e)),
        Err(_) => {
            let _ = child.start_kill();
            return Err(BuildError::Timeout {
                drv: request.drv_path.clone(),
                seconds: request.timeout.as_secs(),
            });
        }
    };

    let status = match timeout(Duration::from_secs(5), child.wait()).await {
        Ok(s) => s.map_err(BuildError::Io)?,
        Err(_) => {
            let _ = child.start_kill();
            return Err(BuildError::Timeout {
                drv: request.drv_path.clone(),
                seconds: request.timeout.as_secs(),
            });
        }
    };

    // If `nix-store --realise` produced no stderr but succeeded, the
    // output was substituted (or already present) and no build ran on
    // this host. Try the local log archive — `nix-store --read-log` —
    // first, in case argunix or someone else on this machine built the
    // drv earlier. If that's also empty, write an honest placeholder
    // saying we didn't build it, rather than leaving a blank file
    // that makes the UI link dead-end.
    let log_bytes = if status.success() && stderr_buf.iter().all(u8::is_ascii_whitespace) {
        read_archived_drv_log(&request.drv_path)
            .await
            .unwrap_or_else(|| no_log_placeholder(&request.drv_path).into_bytes())
    } else {
        render_internal_json(&stderr_buf)
    };
    write_zstd_log(&request.log_path, log_bytes).await?;

    let outputs = parse_realise_stdout(&stdout_buf);

    Ok(BuildOutcome {
        status: if status.success() {
            BuildStatus::Success
        } else {
            BuildStatus::Failure
        },
        exit_code: status.code(),
        output_paths: outputs,
        log_path: request.log_path.clone(),
        log_truncated,
    })
}

/// Render a captured `nix-store --realise --log-format internal-json`
/// stderr buffer into the flat, per-derivation-prefixed text argunix
/// stores. Plain (non-`internal-json`) input passes through unchanged
/// via the parser's `Raw` fallback, so an old builder or a fake one
/// in a test still produces a readable log.
fn render_internal_json(raw: &[u8]) -> Vec<u8> {
    let mut parser = argunix_nom::NomParser::new();
    let mut events = parser.feed(raw);
    events.extend(parser.finish());
    let mut out = Vec::new();
    for ev in &events {
        if let Some(line) = argunix_nom::render_storage_line(ev) {
            out.extend_from_slice(line.as_bytes());
            out.push(b'\n');
        }
    }
    out
}

/// `nix-store --read-log <drv>` — local `/nix/var/log/nix/drvs/...` only.
/// Best-effort: any failure (binary missing, drv not in the archive,
/// non-zero exit) returns None and the caller writes a placeholder.
async fn read_archived_drv_log(drv_path: &str) -> Option<Vec<u8>> {
    let output = Command::new("nix-store")
        .arg("--read-log")
        .arg(drv_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    if output.status.success() && !output.stdout.is_empty() {
        Some(output.stdout)
    } else {
        None
    }
}

/// "We didn't build this" placeholder. Written into the `.log.zst` when
/// the realise succeeded with no captured stderr and the local log
/// archive has nothing for this drv either — meaning the output came
/// from a substituter and never built on this host. We deliberately
/// don't query the substituter for an upstream log: that's network
/// traffic on every cache hit and the upstream log isn't *our* build.
fn no_log_placeholder(drv_path: &str) -> String {
    format!(
        "(no local build log)\n\n\
         The derivation `{drv_path}` was not built by this host — its\n\
         output was substituted from a binary cache (or was already\n\
         present in the local store from an earlier run). argunix\n\
         records the build logs of its own builds; logs from other\n\
         hosts (e.g. cache.nixos.org's Hydra) are not fetched.\n",
    )
}

async fn collect_capped<R: AsyncReadExt + Unpin>(
    stream: &mut R,
    cap_bytes: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::with_capacity(cap_bytes.min(64 * 1024));
    let mut chunk = [0u8; 16 * 1024];
    let mut truncated = false;
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        if buf.len() + n <= cap_bytes {
            buf.extend_from_slice(&chunk[..n]);
            continue;
        }
        let remaining = cap_bytes.saturating_sub(buf.len());
        buf.extend_from_slice(&chunk[..remaining]);
        truncated = true;
        // Drain the remainder so the child process can finish writing
        // without blocking on a full pipe buffer; we discard the contents.
        let mut sink = tokio::io::sink();
        tokio::io::copy(stream, &mut sink).await?;
        break;
    }
    if truncated {
        mark_truncated(&mut buf);
    }
    Ok((buf, truncated))
}

fn parse_realise_stdout(stdout: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialise PATH-mutating tests in this module. cargo test runs in
    /// parallel by default; without this, the two `does_not_pass_…` /
    /// `passes_add_root_…` tests race on the global PATH env var.
    static PATH_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn parses_realise_output_paths() {
        let stdout = b"/nix/store/zzz-hello\n/nix/store/yyy-hello-dev\n\n";
        let parsed = parse_realise_stdout(stdout);
        assert_eq!(
            parsed,
            vec!["/nix/store/zzz-hello", "/nix/store/yyy-hello-dev"]
        );
    }

    #[test]
    fn parse_realise_handles_empty_stdout() {
        assert!(parse_realise_stdout(b"").is_empty());
    }

    /// Regression test for an earlier bug where argunix-build invoked
    /// `nix-store --realise -L <drv>`. `-L` is a `nix` (new-CLI) flag and
    /// the legacy `nix-store` rejects it, so every realise call exited
    /// with an argv error before doing any building. This test stands up
    /// a fake `nix-store` that records its argv to a file, runs
    /// run_build(), and asserts that `-L` never appears.
    #[tokio::test]
    async fn does_not_pass_minus_l_to_nix_store() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use tempfile::tempdir;

        let bin_root = tempdir().unwrap();
        let bin = bin_root.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let argv_log = bin_root.path().join("argv.txt");
        let script = bin.join("nix-store");
        // Write + close + chmod, in that order. Linux returns ETXTBSY
        // if you try to exec a file that still has a writable fd open.
        {
            let mut f = std::fs::File::create(&script).unwrap();
            writeln!(
                f,
                r#"#!/bin/sh
# `--read-log` is what run_build's empty-stderr fallback calls; we
# want it to return non-zero so the fallback writes the placeholder
# instead and the assertion can still find the original --realise
# argv (which would otherwise be overwritten by the second call).
if [ "$1" = "--read-log" ]; then exit 1; fi
out="{}"
: > "$out"
for a in "$@"; do printf '%s\n' "$a" >> "$out"; done
echo /nix/store/zzz-fake
exit 0
"#,
                argv_log.display(),
            )
            .unwrap();
            f.sync_all().unwrap();
        }
        let mut perm = std::fs::metadata(&script).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&script, perm).unwrap();

        let log_dir = tempdir().unwrap();
        let log_path = log_dir.path().join("build.log.zst");

        let _guard = PATH_LOCK.lock().unwrap();
        let original_path = std::env::var_os("PATH").unwrap_or_default();
        let mut new_path = std::ffi::OsString::from(&bin);
        new_path.push(":");
        new_path.push(&original_path);
        unsafe { std::env::set_var("PATH", &new_path) };

        let request = BuildRequest {
            drv_path: "/nix/store/aaaa-fake.drv".to_string(),
            log_path: log_path.clone(),
            timeout: Duration::from_secs(10),
            log_limit: LogCaptureLimit::default(),
            gc_root: None,
        };
        let outcome = run_build(&request).await;

        unsafe { std::env::set_var("PATH", &original_path) };

        let outcome = outcome.expect("run_build should succeed");
        assert_eq!(outcome.status, BuildStatus::Success);

        let argv = std::fs::read_to_string(&argv_log).expect("fake recorded argv");
        let lines: Vec<&str> = argv.lines().collect();
        assert!(
            lines.iter().any(|a| *a == "--realise"),
            "argv missing --realise: {lines:?}",
        );
        assert!(
            !lines
                .iter()
                .any(|a| *a == "-L" || *a == "--print-build-logs"),
            "argv contains a `nix build`-only flag that nix-store rejects: {lines:?}",
        );
    }

    /// When the caller asks for a gc-root, run_build must forward
    /// `--add-root <path>` to nix-store *before* the drv path. Without
    /// that, nix-store warns "you did not specify '--add-root'" on every
    /// invocation and the build output is briefly unprotected.
    #[tokio::test]
    async fn passes_add_root_when_gc_root_set() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use tempfile::tempdir;

        let bin_root = tempdir().unwrap();
        let bin = bin_root.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let argv_log = bin_root.path().join("argv.txt");
        let script = bin.join("nix-store");
        {
            let mut f = std::fs::File::create(&script).unwrap();
            writeln!(
                f,
                r#"#!/bin/sh
# `--read-log` is what run_build's empty-stderr fallback calls; we
# want it to return non-zero so the fallback writes the placeholder
# instead and the assertion can still find the original --realise
# argv (which would otherwise be overwritten by the second call).
if [ "$1" = "--read-log" ]; then exit 1; fi
out="{}"
: > "$out"
for a in "$@"; do printf '%s\n' "$a" >> "$out"; done
echo /nix/store/zzz-fake
exit 0
"#,
                argv_log.display(),
            )
            .unwrap();
            f.sync_all().unwrap();
        }
        let mut perm = std::fs::metadata(&script).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&script, perm).unwrap();

        let log_dir = tempdir().unwrap();
        let log_path = log_dir.path().join("build.log.zst");
        let gc_root = log_dir.path().join("gcroot-symlink");

        let _guard = PATH_LOCK.lock().unwrap();
        let original_path = std::env::var_os("PATH").unwrap_or_default();
        let mut new_path = std::ffi::OsString::from(&bin);
        new_path.push(":");
        new_path.push(&original_path);
        unsafe { std::env::set_var("PATH", &new_path) };

        let request = BuildRequest {
            drv_path: "/nix/store/aaaa-fake.drv".to_string(),
            log_path: log_path.clone(),
            timeout: Duration::from_secs(10),
            log_limit: LogCaptureLimit::default(),
            gc_root: Some(gc_root.clone()),
        };
        let outcome = run_build(&request).await;

        unsafe { std::env::set_var("PATH", &original_path) };

        outcome.expect("run_build should succeed");

        let argv = std::fs::read_to_string(&argv_log).expect("fake recorded argv");
        let lines: Vec<&str> = argv.lines().collect();

        let add_root_idx = lines
            .iter()
            .position(|a| *a == "--add-root")
            .expect("--add-root missing from argv");
        assert_eq!(
            lines.get(add_root_idx + 1).map(|s| s.as_ref()),
            Some(gc_root.to_string_lossy().as_ref()),
            "--add-root must be followed by the gc-root path; got argv {lines:?}",
        );
        let drv_idx = lines
            .iter()
            .position(|a| *a == "/nix/store/aaaa-fake.drv")
            .expect("drv path missing");
        assert!(
            add_root_idx < drv_idx,
            "--add-root must precede the drv path"
        );
    }

    /// When `nix-store --realise` succeeds with empty stderr (the output
    /// was already in the local store and no build ran), run_build must
    /// fall back to `nix-store --read-log <drv>` so the captured log
    /// contains the *original* build's output instead of being blank.
    #[tokio::test]
    async fn empty_stderr_falls_back_to_read_log() {
        use std::io::{Read, Write};
        use std::os::unix::fs::PermissionsExt;
        use tempfile::tempdir;

        let bin_root = tempdir().unwrap();
        let bin = bin_root.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let script = bin.join("nix-store");
        // Fake nix-store: --realise prints the store path on stdout and
        // exits silently (no stderr); --read-log prints a canned log
        // body that the test will look for in the captured .log.zst.
        {
            let mut f = std::fs::File::create(&script).unwrap();
            writeln!(
                f,
                r#"#!/bin/sh
case "$1" in
  --realise)
    echo /nix/store/zzz-fake
    exit 0
    ;;
  --read-log)
    echo "[archived] hello world"
    echo "[archived] line two"
    exit 0
    ;;
esac
exit 99
"#,
            )
            .unwrap();
            f.sync_all().unwrap();
        }
        let mut perm = std::fs::metadata(&script).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&script, perm).unwrap();

        let log_dir = tempdir().unwrap();
        let log_path = log_dir.path().join("build.log.zst");

        let _guard = PATH_LOCK.lock().unwrap();
        let original_path = std::env::var_os("PATH").unwrap_or_default();
        let mut new_path = std::ffi::OsString::from(&bin);
        new_path.push(":");
        new_path.push(&original_path);
        unsafe { std::env::set_var("PATH", &new_path) };

        let request = BuildRequest {
            drv_path: "/nix/store/aaaa-fake.drv".to_string(),
            log_path: log_path.clone(),
            timeout: Duration::from_secs(10),
            log_limit: LogCaptureLimit::default(),
            gc_root: None,
        };
        let outcome = run_build(&request).await;

        unsafe { std::env::set_var("PATH", &original_path) };

        let outcome = outcome.expect("run_build should succeed");
        assert_eq!(outcome.status, BuildStatus::Success);

        // Read the .log.zst back and confirm it contains the read-log content.
        let f = std::fs::File::open(&log_path).expect("log file written");
        let mut decoder = zstd::stream::Decoder::new(f).unwrap();
        let mut decoded = String::new();
        decoder.read_to_string(&mut decoded).unwrap();
        assert!(
            decoded.contains("[archived] hello world"),
            "expected archived log content in .log.zst, got: {decoded:?}",
        );
    }

    /// Substituted derivation, no local archived log either: the on-disk
    /// `.log.zst` must contain the explanatory placeholder, not be blank.
    #[tokio::test]
    async fn empty_stderr_with_no_local_log_writes_placeholder() {
        use std::io::{Read, Write};
        use std::os::unix::fs::PermissionsExt;
        use tempfile::tempdir;

        let bin_root = tempdir().unwrap();
        let bin = bin_root.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // nix-store: --realise succeeds with empty stderr; --read-log
        // exits non-zero (no archived log).
        {
            let script = bin.join("nix-store");
            let mut f = std::fs::File::create(&script).unwrap();
            writeln!(
                f,
                r#"#!/bin/sh
case "$1" in
  --realise) echo /nix/store/zzz-fake; exit 0 ;;
  --read-log) exit 1 ;;
esac
exit 99
"#,
            )
            .unwrap();
            f.sync_all().unwrap();
            let mut perm = std::fs::metadata(&script).unwrap().permissions();
            perm.set_mode(0o755);
            std::fs::set_permissions(&script, perm).unwrap();
        }

        let log_dir = tempdir().unwrap();
        let log_path = log_dir.path().join("build.log.zst");

        let _guard = PATH_LOCK.lock().unwrap();
        let original_path = std::env::var_os("PATH").unwrap_or_default();
        let mut new_path = std::ffi::OsString::from(&bin);
        new_path.push(":");
        new_path.push(&original_path);
        unsafe { std::env::set_var("PATH", &new_path) };

        let request = BuildRequest {
            drv_path: "/nix/store/aaaa-fake.drv".to_string(),
            log_path: log_path.clone(),
            timeout: Duration::from_secs(10),
            log_limit: LogCaptureLimit::default(),
            gc_root: None,
        };
        let outcome = run_build(&request).await;

        unsafe { std::env::set_var("PATH", &original_path) };

        let outcome = outcome.expect("run_build should succeed");
        assert_eq!(outcome.status, BuildStatus::Success);

        let f = std::fs::File::open(&log_path).expect("log file written");
        let mut decoder = zstd::stream::Decoder::new(f).unwrap();
        let mut decoded = String::new();
        decoder.read_to_string(&mut decoded).unwrap();
        assert!(
            decoded.contains("(no local build log)"),
            "expected placeholder in .log.zst, got: {decoded:?}",
        );
        assert!(
            decoded.contains("/nix/store/aaaa-fake.drv"),
            "placeholder should mention the drv path, got: {decoded:?}",
        );
    }
}
