//! Per-builder Unix socket bridge.
//!
//! For every Active builder, medusa exposes a Unix socket at
//! `<socket_dir>/<name>.sock`. nix invokes `medusa-pipe <name>` (via
//! the `ssh-command=` knob in `--builders`); medusa-pipe connects to
//! that socket; this module's accept loop opens a fresh SSH build
//! channel into the named builder and bidirectionally proxies bytes
//! between the Unix client and the SSH channel.
//!
//! On the agent side, the channel's stdio is piped to
//! `nix-store --serve --write` (M13b). On medusa's side, nix's worker
//! sees the channel as a normal `nix-store --serve` peer — medusa
//! never parses or modifies any of the wire bytes.

use crate::dispatcher::{BuilderDispatcher, DispatchError};
use medusa_domain::BuilderName;
use russh::ChannelMsg;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

#[derive(Debug, thiserror::Error)]
pub enum SocketError {
    #[error("creating socket dir `{path}`: {source}")]
    Mkdir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("binding unix socket `{path}`: {source}")]
    Bind {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Per-builder Unix socket lifecycle. Construct one per medusa daemon
/// (with a shared `socket_dir`); call `listen_for(name)` each time a
/// builder transitions to Active.
pub struct SocketServer {
    pub socket_dir: PathBuf,
    pub dispatcher: Arc<BuilderDispatcher>,
}

impl SocketServer {
    pub fn new(socket_dir: PathBuf, dispatcher: Arc<BuilderDispatcher>) -> Self {
        Self {
            socket_dir,
            dispatcher,
        }
    }

    /// Bind `<socket_dir>/<name>.sock`, spawn an accept loop, return
    /// a [`SocketGuard`] whose drop removes the file. The accept task
    /// keeps running until the listener is dropped (which happens
    /// when the SocketGuard is dropped).
    pub async fn listen_for(&self, name: BuilderName) -> Result<SocketGuard, SocketError> {
        // Best-effort mkdir. Caller is responsible for permissions.
        if !self.socket_dir.exists() {
            tokio::fs::create_dir_all(&self.socket_dir)
                .await
                .map_err(|source| SocketError::Mkdir {
                    path: self.socket_dir.clone(),
                    source,
                })?;
        }
        let path = self.socket_dir.join(format!("{}.sock", name.as_str()));
        // If a stale socket file is sitting there from a previous
        // process / crash, remove it before binding.
        let _ = tokio::fs::remove_file(&path).await;
        let listener = UnixListener::bind(&path).map_err(|source| SocketError::Bind {
            path: path.clone(),
            source,
        })?;

        let dispatcher = self.dispatcher.clone();
        let name_for_task = name.clone();
        let task = tokio::spawn(async move {
            accept_loop(listener, dispatcher, name_for_task).await;
        });

        Ok(SocketGuard {
            path,
            task: Some(task),
        })
    }
}

/// RAII handle holding a builder's Unix socket open. Drop removes the
/// socket file and aborts the accept loop. Call `close()` to do this
/// asynchronously and await the loop's shutdown for clean teardown.
pub struct SocketGuard {
    path: PathBuf,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl SocketGuard {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stop accepting and remove the socket file. Awaits the accept
    /// loop's shutdown so the file is gone by the time this returns.
    pub async fn close(mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
            let _ = t.await;
        }
        let _ = tokio::fs::remove_file(&self.path).await;
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
        // Best-effort sync removal. tokio::fs would need an awaiter;
        // std::fs is fine here because we're already dropping.
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn accept_loop(
    listener: UnixListener,
    dispatcher: Arc<BuilderDispatcher>,
    name: BuilderName,
) {
    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(builder = %name, error = %e, "unix accept failed; backing off");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        let dispatcher = dispatcher.clone();
        let name = name.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_connection(stream, dispatcher, &name).await {
                tracing::warn!(builder = %name, error = %e, "build-channel proxy ended with error");
            }
        });
    }
}

async fn serve_connection(
    stream: UnixStream,
    dispatcher: Arc<BuilderDispatcher>,
    name: &BuilderName,
) -> Result<(), DispatchError> {
    let mut dispatched = dispatcher.open_channel(name).await?;
    let mut channel = dispatched
        .take_channel()
        .ok_or_else(|| DispatchError::NoSession {
            name: name.as_str().to_string(),
        })?;
    let channel_id: u32 = channel.id().into();
    let started_at = std::time::Instant::now();
    tracing::info!(
        builder = %name,
        channel = channel_id,
        "build proxy started",
    );

    let (mut sock_read, mut sock_write) = stream.into_split();

    // One task, both directions, single `select!`. The russh
    // Channel can't be split into independent read/write halves the
    // way a TCP stream can — `wait()` consumes events from a
    // shared receiver — so we keep both directions in one place.
    let mut sock_buf = [0u8; 32 * 1024];
    let exit_reason = loop {
        tokio::select! {
            // Unix → SSH channel
            r = sock_read.read(&mut sock_buf) => match r {
                Ok(0) => {
                    let _ = channel.eof().await;
                    break "unix-eof";
                }
                Ok(n) => {
                    tracing::trace!(builder = %name, channel = channel_id, n, "unix→ssh");
                    if channel.data(&sock_buf[..n]).await.is_err() {
                        break "ssh-write-failed";
                    }
                }
                Err(e) => {
                    tracing::debug!(builder = %name, error = %e, "unix read ended");
                    break "unix-read-error";
                }
            },
            // SSH channel → Unix
            ev = channel.wait() => match ev {
                Some(ChannelMsg::Data { data }) => {
                    tracing::trace!(builder = %name, channel = channel_id, n = data.len(), "ssh→unix");
                    if sock_write.write_all(&data).await.is_err() {
                        break "unix-write-failed";
                    }
                    if sock_write.flush().await.is_err() {
                        break "unix-flush-failed";
                    }
                }
                Some(ChannelMsg::Eof) => break "channel-eof",
                Some(ChannelMsg::Close) => break "channel-close",
                None => break "channel-end-of-stream",
                Some(other) => {
                    tracing::debug!(
                        builder = %name,
                        channel = channel_id,
                        msg = ?std::mem::discriminant(&other),
                        "build proxy: non-data ChannelMsg ignored",
                    );
                    continue;
                }
            }
        }
    };
    let _ = channel.close().await;
    let _ = sock_write.shutdown().await;
    tracing::info!(
        builder = %name,
        channel = channel_id,
        elapsed_ms = started_at.elapsed().as_millis() as u64,
        reason = exit_reason,
        "build proxy finished",
    );
    Ok(())
}
