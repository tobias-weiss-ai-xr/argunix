//! Daemon-side helper that drives `nix copy` against a builder
//! through a Unix-socket proxy backed by our russh side-channel.
//!
//! Flow per call:
//! 1. Bind a private Unix-domain socket in a temp dir.
//! 2. Spawn an accept loop that, on each accept, opens a
//!    [`SideChannelKind::NixDaemonStdio`] side channel to the named
//!    builder and bidirectionally pipes bytes between the socket
//!    and the channel. The channel side has the agent running
//!    `nix-daemon --stdio`, so the socket effectively *is* the
//!    builder's nix-daemon endpoint as far as the daemon-side `nix`
//!    binary is concerned.
//! 3. Shell out to `nix copy --from|--to unix:///<sock> <paths>`.
//!    `nix copy` opens the socket, speaks the daemon protocol,
//!    and copies the listed paths along with their closures
//!    automatically — no `--requisites` expansion or topo sort
//!    needed. The daemon protocol streams per-file with bounded
//!    memory, fixing the OOMs from the legacy `--export | --import`
//!    path on multi-GB single-NAR outputs.
//! 4. Tear down the proxy + remove the socket.

use crate::channel_io::with_channel_io;
use crate::dispatcher::BuilderDispatcher;
use crate::side_channel::{SideChannelError, SideChannelHeader, SideChannelKind, write_header};
use argunix_domain::BuilderName;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

/// Per-call byte-count + wall-clock for one `nix_copy_over_pool`
/// invocation. Returned to the worker so it can persist per-job
/// transport metrics without instrumenting the full nix-copy
/// stderr.
#[derive(Debug, Clone, Copy, Default)]
pub struct NixCopyMetrics {
    /// Bytes daemon-side `nix copy` sent into the proxy socket
    /// (i.e. daemon → builder over our russh tunnel). Includes
    /// daemon-protocol framing.
    pub bytes_to_builder: u64,
    /// Bytes proxy received from the builder (builder → daemon).
    pub bytes_from_builder: u64,
    /// Wall-clock from `nix copy` spawn to the subprocess exiting,
    /// proxy teardown included.
    pub elapsed: Duration,
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
    #[error("writing side-channel header: {0}")]
    Header(#[from] SideChannelError),
    #[error("creating temporary directory for proxy socket: {0}")]
    Tempdir(#[source] std::io::Error),
    #[error("binding Unix socket at `{path}`: {source}")]
    BindSocket {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("opening side channel to builder `{name}`: {error}")]
    OpenChannel { name: String, error: String },
    #[error("`nix copy` exited {code:?} (direction={direction:?}): {stderr}")]
    NixCopyFailed {
        direction: &'static str,
        code: Option<i32>,
        stderr: String,
    },
    #[error("running `nix copy`: {0}")]
    NixCopySpawn(#[source] std::io::Error),
}

/// Direction of a `nix copy` invocation.
#[derive(Debug, Clone, Copy)]
pub enum NixCopyDirection {
    /// `nix copy --from <builder> <paths>` — pulls paths from the
    /// builder's nix store into the local store. Used after a build
    /// to materialise outputs.
    From,
    /// `nix copy --to <builder> <paths>` — pushes paths from the
    /// local store into the builder's store. Used before a build to
    /// stage drv inputs.
    To,
}

impl NixCopyDirection {
    fn flag(self) -> &'static str {
        match self {
            NixCopyDirection::From => "--from",
            NixCopyDirection::To => "--to",
        }
    }
    fn label(self) -> &'static str {
        match self {
            NixCopyDirection::From => "from",
            NixCopyDirection::To => "to",
        }
    }
}

/// Run `nix copy --{from|to} ssh-ng://localhost?remote-program=...
/// <paths>` against a pool builder. A private Unix socket forwards
/// connections to a fresh `NixDaemonStdio` side channel; the agent
/// on the other end pipes those bytes to its system `nix-daemon`
/// socket.
///
/// **Why `ssh-ng://localhost` and not `unix:///<sock>`** — the
/// `unix://` URI scheme in Nix is `UDSRemoteStore`, which inherits
/// `LocalFSStore` and overrides `getFSAccessor` to read store
/// objects directly from the local filesystem (`local-fs-store.cc`
/// `LocalFSStore::getFSAccessor`). That's correct when the daemon
/// at the other end of the unix socket serves the *same* /nix/store
/// the client sees — but in our case the source store lives on a
/// different VM, so the FS check fails with `path '...' does not
/// exist` even though the daemon protocol would happily serve the
/// path. `SSHStore` (the `ssh-ng://` URI) only inherits
/// `RemoteStore`, so `getFSAccessor` goes through `RemoteFSAccessor`
/// which fetches via the daemon protocol. The `localhost` authority
/// engages `fakeSSH = true` in `SSHMaster`, which skips the actual
/// `ssh` subprocess and execs `remote-program` directly. We point
/// `remote-program` at a tiny shell wrapper that runs
/// `socat - UNIX-CONNECT:<sock>` against our proxy.
///
/// `--no-check-sigs` is always passed: we trust the agent over our
/// authenticated SSH session, and build outputs aren't auto-signed.
pub async fn nix_copy_over_pool(
    dispatcher: &BuilderDispatcher,
    builder_name: &BuilderName,
    direction: NixCopyDirection,
    paths: &[String],
    nix_bin: &Path,
    build_id: i64,
) -> Result<NixCopyMetrics, ClosureXferError> {
    if paths.is_empty() {
        return Ok(NixCopyMetrics::default());
    }
    let started_at = Instant::now();
    // Shared atomics: each accepted bridge increments these as bytes
    // flow. Returning the sum lets the worker persist per-job
    // transport metrics. "to_builder" / "from_builder" semantics are
    // direction-independent here — they always describe the daemon
    // → builder / builder → daemon directions through our tunnel,
    // regardless of whether `nix copy` is pushing or pulling.
    let bytes_to_builder = Arc::new(AtomicU64::new(0));
    let bytes_from_builder = Arc::new(AtomicU64::new(0));
    let sock_dir = tempfile::tempdir().map_err(ClosureXferError::Tempdir)?;
    let sock_path = sock_dir.path().join("nix-daemon.sock");
    let wrapper_path = sock_dir.path().join("tunnel.sh");
    let listener = tokio::net::UnixListener::bind(&sock_path).map_err(|source| {
        ClosureXferError::BindSocket {
            path: sock_path.clone(),
            source,
        }
    })?;

    // Drop the wrapper script. `$1` is our socket path; any further
    // args (nix appends `--stdio` and possibly `--store …`) are
    // ignored. Shell exec replaces the script process with socat,
    // so `wait()` reaps the actual byte forwarder.
    {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;
        let mut f = std::fs::File::create(&wrapper_path).map_err(ClosureXferError::Tempdir)?;
        f.write_all(b"#!/bin/sh\nexec socat - \"UNIX-CONNECT:$1\"\n")
            .map_err(ClosureXferError::Tempdir)?;
        f.sync_all().map_err(ClosureXferError::Tempdir)?;
        let mut perm = std::fs::metadata(&wrapper_path)
            .map_err(ClosureXferError::Tempdir)?
            .permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&wrapper_path, perm).map_err(ClosureXferError::Tempdir)?;
    }

    // Spawn the proxy. Each accepted connection opens its own
    // side channel — `nix copy` typically uses a single connection
    // per invocation but we don't constrain that.
    let dispatcher = Arc::new(dispatcher.clone());
    let builder = builder_name.clone();
    let bytes_to = bytes_to_builder.clone();
    let bytes_from = bytes_from_builder.clone();
    // Abort the accept loop on every exit path — including when this
    // whole future is dropped mid-transfer (the worker races us against
    // cancel / builder-gone). Without this, a dropped `nix_copy_over_pool`
    // would leak its accept-loop task. Pairs with `cmd.kill_on_drop`
    // below so a dropped transfer leaves nothing running.
    struct AbortOnDrop(tokio::task::JoinHandle<()>);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let proxy = AbortOnDrop(tokio::spawn(async move {
        loop {
            let (sock, _) = match listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(error = %e, "proxy listener accept failed; stopping");
                    break;
                }
            };
            let dispatcher = dispatcher.clone();
            let builder = builder.clone();
            let bytes_to = bytes_to.clone();
            let bytes_from = bytes_from.clone();
            tokio::spawn(async move {
                if let Err(e) = bridge_unix_to_channel(
                    sock,
                    &dispatcher,
                    &builder,
                    build_id,
                    bytes_to,
                    bytes_from,
                )
                .await
                {
                    tracing::warn!(
                        error = %e,
                        builder = %builder,
                        build_id,
                        "nix-daemon-stdio bridge failed",
                    );
                }
            });
        }
    }));

    // remote-program is a `Setting<Strings>` (whitespace-split), so
    // we pass two tokens: the wrapper path and the socket path.
    let store_uri = format!(
        "ssh-ng://localhost?remote-program={}%20{}",
        wrapper_path.display(),
        sock_path.display(),
    );
    let mut cmd = Command::new(nix_bin);
    cmd.arg("--extra-experimental-features")
        .arg("nix-command")
        .arg("copy")
        .arg("--no-check-sigs")
        .arg(direction.flag())
        .arg(&store_uri)
        .args(paths)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = cmd.output().await.map_err(ClosureXferError::NixCopySpawn)?;

    drop(proxy); // AbortOnDrop: stops the accept loop
    drop(sock_dir); // removes the wrapper + temp socket file

    if !output.status.success() {
        return Err(ClosureXferError::NixCopyFailed {
            direction: direction.label(),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(NixCopyMetrics {
        bytes_to_builder: bytes_to_builder.load(Ordering::Relaxed),
        bytes_from_builder: bytes_from_builder.load(Ordering::Relaxed),
        elapsed: started_at.elapsed(),
    })
}

/// Counting variant of `tokio::io::copy`: forwards reader → writer
/// like the standard helper but also adds each chunk's length to a
/// shared atomic so the parent can report total bytes seen.
async fn copy_counted<R, W>(
    reader: &mut R,
    writer: &mut W,
    counter: &AtomicU64,
) -> std::io::Result<u64>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; 8 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            writer.flush().await?;
            return Ok(total);
        }
        writer.write_all(&buf[..n]).await?;
        total += n as u64;
        counter.fetch_add(n as u64, Ordering::Relaxed);
    }
}

/// Bridge a single accepted Unix-socket connection to a fresh
/// `NixDaemonStdio` side channel. Writes the header, then bidir-
/// ectionally pipes bytes between the socket and the channel,
/// summing the per-direction byte counts into the shared atomics
/// the parent passes in for transport-metrics accounting.
async fn bridge_unix_to_channel(
    sock: tokio::net::UnixStream,
    dispatcher: &BuilderDispatcher,
    builder_name: &BuilderName,
    build_id: i64,
    bytes_to_builder: Arc<AtomicU64>,
    bytes_from_builder: Arc<AtomicU64>,
) -> Result<(), ClosureXferError> {
    let chan = dispatcher
        .open_channel(builder_name)
        .await
        .map_err(|e| ClosureXferError::OpenChannel {
            name: builder_name.as_str().to_string(),
            error: format!("{e}"),
        })?
        .take_channel()
        .expect("dispatcher returned channel");

    let _ = with_channel_io(chan, None, move |io| async move {
        let (chan_reader, chan_writer) = tokio::io::split(io);

        // Write the protocol header so the agent forwards to its
        // system nix-daemon socket.
        let header = SideChannelHeader {
            kind: SideChannelKind::NixDaemonStdio,
            build_id,
            paths: vec![],
        };
        let mut chan_writer = chan_writer;
        if let Err(e) = write_header(&mut chan_writer, &header).await {
            tracing::warn!(error = %e, "writing side-channel header failed");
            return;
        }

        let (sock_reader, sock_writer) = sock.into_split();
        // Each direction takes ownership of its sock half + channel
        // half so we can explicitly half-close once that direction
        // EOFs. Without this, when `nix copy` disconnects (sock
        // reader EOFs) we'd never tell the agent "no more
        // requests", the agent's tunneled `nix-daemon` stays
        // blocked on read, and `from_chan` never completes — the
        // whole bridge wedges.
        let to_chan = async move {
            let mut sock_reader = sock_reader;
            let mut chan_writer = chan_writer;
            let r = copy_counted(&mut sock_reader, &mut chan_writer, &bytes_to_builder).await;
            let _ = chan_writer.shutdown().await;
            drop(chan_writer);
            r
        };
        let from_chan = async move {
            let mut chan_reader = chan_reader;
            let mut sock_writer = sock_writer;
            let r = copy_counted(&mut chan_reader, &mut sock_writer, &bytes_from_builder).await;
            let _ = sock_writer.shutdown().await;
            r
        };
        let _ = tokio::join!(to_chan, from_chan);
    })
    .await;
    Ok(())
}
