//! `russh::Channel` ↔ `AsyncRead`+`AsyncWrite` adapter (M14b).
//!
//! Bridges the message-oriented russh channel API (`channel.wait()`
//! returning `ChannelMsg::Data { data: CryptoVec }` events) to a
//! byte-stream `AsyncRead` + `AsyncWrite` interface, so transport-
//! agnostic helpers like [`crate::dispatch_inbound`] can drive it
//! without knowing about russh.
//!
//! Why a single function (not a `split_channel` returning two
//! halves): russh's `Channel` is owned and methods take `&mut self`,
//! so we can't easily split it across two threads without locking.
//! Instead, [`with_channel_io`] runs a pump task that owns the
//! channel and ferries bytes through a `tokio::io::duplex` pipe —
//! the user supplies a closure that receives the `AsyncRead`/Write
//! end and returns when its work is done. The pump shuts down when
//! the closure resolves or the channel reports EOF/close.

use russh::client::Msg as ClientMsg;
use russh::server::Msg as ServerMsg;
use russh::{Channel, ChannelId, ChannelMsg};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::oneshot;

/// Internal pump-loop that owns the russh channel and ferries bytes
/// in both directions through `pump_side`. Generic over the channel
/// "side" type so the same loop drives both the agent-side
/// `Channel<ClientMsg>` (server-pushed) and the daemon-side
/// `Channel<ServerMsg>` (client-opened).
///
/// `close_rx`, when supplied, is an out-of-band signal that the
/// channel has been closed by the peer. Required on the agent side:
/// for *server-pushed* channels, russh delivers `CHANNEL_EOF` and
/// `CHANNEL_CLOSE` only via the `Handler::channel_eof/close`
/// callbacks — never through `Channel::wait()` — so without this
/// signal the pump would spin forever after a clean close. Optional
/// on the daemon side, where `wait()` does see Eof/Close.
async fn run_pump<S>(
    mut channel: Channel<S>,
    mut pump_side: DuplexStream,
    close_rx: Option<oneshot::Receiver<()>>,
) -> Channel<S>
where
    S: ChannelSide,
{
    let mut buf = vec![0u8; 16 * 1024];
    let mut close_rx = close_rx;
    // Once the user side stops writing (its writer dropped — we see
    // Ok(0) on `pump_side.read`), we send `channel.eof()` to signal
    // the peer and disable the read arm. We must NOT break the whole
    // loop here: if the peer is still working (e.g. an agent draining
    // `nix-store --import` after we've sent EOF), it may still emit
    // Data events that the user side wants to see, and we need to
    // wait for the peer's own Eof/Close before tearing down — which
    // is what makes "user closure returned" mean "remote handling
    // is also done" daemon-side.
    let mut user_writer_done = false;
    loop {
        tokio::select! {
            // Channel → pump_side (user reads via `read()` end).
            ev = channel.wait() => match ev {
                Some(ChannelMsg::Data { data }) => {
                    if pump_side.write_all(&data).await.is_err() {
                        break;
                    }
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                    let _ = pump_side.shutdown().await;
                    break;
                }
                Some(_) => continue,
            },
            // pump_side → channel (user writes via `write()` end).
            // After the user's writer is dropped (Ok(0) once), we
            // disable this arm via `user_writer_done`; the loop
            // continues so peer→user events are still pumped.
            r = pump_side.read(&mut buf), if !user_writer_done => match r {
                Ok(0) => {
                    let _ = channel.eof().await;
                    user_writer_done = true;
                }
                Ok(n) => {
                    if channel.data(&buf[..n]).await.is_err() {
                        break;
                    }
                }
                Err(_) => {
                    user_writer_done = true;
                }
            },
            // Agent-side: out-of-band EOF/Close from Handler.
            // `wait_close` sets the slot to None after firing so
            // the branch can never re-fire.
            //
            // Russh delivers `CHANNEL_EOF` via a separate callback
            // path from the channel mpsc, so `channel.wait()` may
            // still have queued `Data` events that haven't been
            // pumped yet. Drain them with a short polling timeout
            // before signalling EOF to the user — otherwise the
            // closing peer's last bytes get dropped and the
            // user-side subprocess (e.g. `nix-store --import`) sees
            // a truncated stream.
            _ = wait_close(&mut close_rx) => {
                loop {
                    match tokio::time::timeout(
                        std::time::Duration::from_millis(50),
                        channel.wait(),
                    ).await {
                        Ok(Some(ChannelMsg::Data { data })) => {
                            if pump_side.write_all(&data).await.is_err() {
                                break;
                            }
                        }
                        _ => break,
                    }
                }
                let _ = pump_side.shutdown().await;
                break;
            }
        }
    }
    channel
}

async fn wait_close(slot: &mut Option<oneshot::Receiver<()>>) {
    match slot {
        Some(rx) => {
            let _ = rx.await;
            *slot = None;
        }
        None => std::future::pending::<()>().await,
    }
}

/// Trait abstracting the two russh channel-side type tags
/// (`russh::client::Msg` for the agent, `russh::server::Msg` for
/// the daemon). Lets [`with_channel_io`] be generic so the same
/// adapter serves both sides without duplication. The supertrait
/// bound `From<(ChannelId, ChannelMsg)> + Send + Sync + 'static` is
/// what russh's `Channel<S>` already requires for its `data` /
/// `eof` / `close` / `wait` methods.
pub trait ChannelSide: From<(ChannelId, ChannelMsg)> + Send + Sync + 'static {}

impl ChannelSide for ClientMsg {}
impl ChannelSide for ServerMsg {}

/// Run `f` with an `AsyncRead`+`AsyncWrite` view onto `channel`.
/// Returns whatever `f` returns; the channel is closed cleanly when
/// `f` resolves (or earlier if the channel goes away).
///
/// The pump task and `f` run concurrently via `tokio::join!`, so
/// `f` can both read from and write to the channel without
/// deadlocking even when the russh channel uses internal flow
/// control.
///
/// `close_rx` is required on the agent side (server-pushed
/// channels — russh hides EOF/Close from `wait()` and delivers them
/// only through `Handler::channel_eof/close`). Pass `None` on the
/// daemon side where `wait()` sees them natively.
pub async fn with_channel_io<S, F, Fut, T>(
    channel: Channel<S>,
    close_rx: Option<oneshot::Receiver<()>>,
    f: F,
) -> T
where
    S: ChannelSide,
    F: FnOnce(DuplexStream) -> Fut,
    Fut: std::future::Future<Output = T> + Send,
{
    // 64K duplex buffer — sized to absorb a typical russh channel
    // window without backpressuring the pump on every Data event.
    let (user_side, pump_side) = tokio::io::duplex(64 * 1024);
    let user_fut = f(user_side);
    let (channel, result) = tokio::join!(run_pump(channel, pump_side, close_rx), user_fut);
    // After f resolves, ensure the russh channel is fully closed.
    // Best-effort — if the peer already closed it, this is a no-op.
    let _ = channel.eof().await;
    let _ = channel.close().await;
    result
}
