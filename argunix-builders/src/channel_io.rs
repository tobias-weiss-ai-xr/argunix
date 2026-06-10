//! `russh::Channel` ↔ `AsyncRead`+`AsyncWrite` adapter.
//!
//! Bridges the message-oriented russh channel API (`channel.wait()`
//! returning `ChannelMsg::Data { data: CryptoVec }` events) to a
//! byte-stream `AsyncRead` + `AsyncWrite` interface, so transport-
//! agnostic helpers like [`crate::dispatch_inbound`] can drive it
//! without knowing about russh.
//!
//! The two directions are pumped by **separate tasks** joined at the
//! parent level. Multiplexing them via `tokio::select!` with `.await`
//! calls inside the arms deadlocks under back-pressure: when one
//! direction is awaiting (e.g. `pump_writer.write_all` blocked
//! because the user's reader is slow), the other arm is not polled,
//! so the response stream that would unblock the first direction
//! cannot drain. The classic nix-daemon-protocol pattern (request →
//! response) hits this every time the response queue fills.

use russh::client::Msg as ClientMsg;
use russh::server::Msg as ServerMsg;
use russh::{Channel, ChannelId, ChannelMsg, ChannelReadHalf, ChannelWriteHalf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::oneshot;

/// russh per-channel flow-control window for builder sessions.
///
/// russh's 2 MiB default is too small for the bidirectional nix-daemon
/// protocol tunneled over a side channel. During a large
/// `nix copy --to` push the request fills the coordinator→builder
/// window while the daemon's interleaved progress output fills the
/// builder→coordinator window; because each endpoint (`nix copy` and
/// `nix-daemon`) can be blocked *writing* while the other is also
/// blocked *writing*, the exchange deadlocks. Observed in production:
/// a push froze at ~3.4 MB with both directions stalled (TCP fully
/// acked, zero send-queue) until the liveness watchdog evicted the
/// builder.
///
/// Sizing the window well above the daemon's total in-flight progress
/// (a few MB even for a many-thousand-derivation closure) guarantees
/// the daemon never blocks writing progress, so it keeps draining the
/// request and the deadlock cycle cannot form. The window is a
/// flow-control ceiling, not a preallocation, so the memory cost is
/// bounded by actual in-flight bytes. Applied to BOTH the coordinator's
/// `server::Config` and the agent's `client::Config` so both the
/// request and response directions are covered.
pub const BUILDER_SESSION_WINDOW_SIZE: u32 = 32 * 1024 * 1024;

/// Maximum SSH packet size for builder sessions. Raised from russh's
/// 32 KiB default to cut per-packet overhead on the bulk closure
/// transfer. Must stay ≤ [`BUILDER_SESSION_WINDOW_SIZE`].
pub const BUILDER_SESSION_MAX_PACKET: u32 = 256 * 1024;

/// Inbound pump: russh channel → `pump_writer` (which the user reads
/// via the other end of the duplex). Runs until the peer signals
/// EOF/close either inline (via `channel.wait()` returning Eof/Close)
/// or out-of-band (via `close_rx`, used on agent-side server-pushed
/// channels — russh delivers their EOF/Close only through the
/// Handler callback path).
async fn run_pump_inbound(
    mut read_half: ChannelReadHalf,
    mut pump_writer: WriteHalf<DuplexStream>,
    mut close_rx: Option<oneshot::Receiver<()>>,
) {
    loop {
        tokio::select! {
            ev = read_half.wait() => match ev {
                Some(ChannelMsg::Data { data }) => {
                    if pump_writer.write_all(&data).await.is_err() {
                        return;
                    }
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                    let _ = pump_writer.shutdown().await;
                    return;
                }
                Some(_) => continue,
            },
            _ = wait_close(&mut close_rx) => {
                // Russh delivers `CHANNEL_EOF` via a separate callback
                // path from the channel mpsc, so the receiver may
                // still have queued `Data` events that haven't been
                // pumped yet. Drain them with a short polling timeout
                // before signalling EOF to the user — otherwise the
                // closing peer's last bytes get dropped and the
                // user-side subprocess (e.g. `nix-daemon --stdio`)
                // sees a truncated stream.
                loop {
                    match tokio::time::timeout(
                        Duration::from_millis(50),
                        read_half.wait(),
                    ).await {
                        Ok(Some(ChannelMsg::Data { data })) => {
                            if pump_writer.write_all(&data).await.is_err() {
                                return;
                            }
                        }
                        _ => break,
                    }
                }
                let _ = pump_writer.shutdown().await;
                return;
            }
        }
    }
}

/// Outbound pump: `pump_reader` (which the user writes via the other
/// end of the duplex) → russh channel `data` frames. Runs until the
/// user drops its writer (`pump_reader.read` returns 0), at which
/// point we send `CHANNEL_EOF` so the peer knows we're done writing.
/// `close()` is the caller's responsibility — see [`with_channel_io`].
async fn run_pump_outbound<S>(
    mut pump_reader: ReadHalf<DuplexStream>,
    write_half: Arc<ChannelWriteHalf<S>>,
) where
    S: ChannelSide,
{
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match pump_reader.read(&mut buf).await {
            Ok(0) => {
                let _ = write_half.eof().await;
                return;
            }
            Ok(n) => {
                if write_half.data(&buf[..n]).await.is_err() {
                    return;
                }
            }
            Err(_) => return,
        }
    }
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
/// The inbound pump, outbound pump, and `f` run concurrently via
/// `tokio::join!`, each as an independent future on the same task.
/// This is what allows `f` to do bidirectional I/O without dead-
/// locking on back-pressure: a stall in one direction can no longer
/// starve the other.
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
    let (pump_reader, pump_writer) = tokio::io::split(pump_side);
    let (read_half, write_half) = channel.split();
    let write_half = Arc::new(write_half);

    let user_fut = f(user_side);
    let inbound = run_pump_inbound(read_half, pump_writer, close_rx);
    let outbound = run_pump_outbound::<S>(pump_reader, write_half.clone());

    let (_, _, result) = tokio::join!(inbound, outbound, user_fut);

    // After f resolves, ensure the russh channel is fully closed.
    // Best-effort — if the peer already closed it, this is a no-op.
    let _ = write_half.eof().await;
    let _ = write_half.close().await;
    result
}
