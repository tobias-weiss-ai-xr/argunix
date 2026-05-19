//! In-memory tap on a build's structured log stream so the web UI can
//! SSE-tail a running build. The worker parses the agent's raw
//! `internal-json` chunks into [`NomEvent`]s (`argunix-nom`) and pushes
//! them here; each SSE subscriber atomically snapshots the buffered
//! prefix and subscribes to a tokio broadcast for everything after.
//!
//! Strictly in-memory — no persistence. Coordinator restart wipes
//! every running tail, which is intentional: post-build the static
//! log endpoint serves the zstd-compressed final log from disk.

use argunix_nom::NomEvent;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

const BROADCAST_CAPACITY: usize = 256;

/// Cap on the replayed prefix a late-joining subscriber receives. A
/// runaway build does not grow this without bound; live subscribers
/// still see every event via the broadcast — only the catch-up buffer
/// stops at the cap, with one sentinel event marking the gap.
const MAX_BUFFERED_EVENTS: usize = 50_000;

/// Per-build state. Buffer + broadcaster are coupled under one mutex
/// so a subscriber's snapshot-then-subscribe is atomic relative to
/// pushes — no event can land in the gap and be missed or replayed.
pub struct LiveLog {
    inner: Mutex<Inner>,
}

struct Inner {
    buf: Vec<NomEvent>,
    truncated: bool,
    tx: broadcast::Sender<NomEvent>,
}

impl LiveLog {
    pub fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Arc::new(Self {
            inner: Mutex::new(Inner {
                buf: Vec::new(),
                truncated: false,
                tx,
            }),
        })
    }

    /// Append an event and fan it out to live subscribers. `send`
    /// errors are ignored — they only mean no subscribers, which is
    /// the common case (most builds finish without anyone watching).
    pub fn push(&self, event: NomEvent) {
        let mut g = self.inner.lock().unwrap();
        if g.buf.len() < MAX_BUFFERED_EVENTS {
            g.buf.push(event.clone());
        } else if !g.truncated {
            g.truncated = true;
            g.buf.push(NomEvent::Raw {
                text: "argunix: live log buffer truncated — open the full log".to_string(),
            });
        }
        let _ = g.tx.send(event);
    }

    /// Take a snapshot of the buffered prefix and a subscription to
    /// future events. The two are produced under one lock so the
    /// caller cannot miss an event (or see it twice) in the gap
    /// between snapshot and subscribe.
    pub fn subscribe(&self) -> (Vec<NomEvent>, broadcast::Receiver<NomEvent>) {
        let g = self.inner.lock().unwrap();
        (g.buf.clone(), g.tx.subscribe())
    }
}

/// Coordinator-wide registry of live build logs. Worker calls
/// [`Self::open`] on `BuildStarted`, [`Self::push`] on each event,
/// [`Self::close`] on `BuildFinished` (or any exit path). Web SSE
/// handlers call [`Self::get`] to subscribe.
#[derive(Default)]
pub struct LiveLogRegistry {
    entries: Mutex<HashMap<i64, Arc<LiveLog>>>,
}

impl LiveLogRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Create the per-build entry. Idempotent — re-opening returns
    /// the existing entry rather than wiping in-flight subscribers.
    pub fn open(&self, build_id: i64) -> Arc<LiveLog> {
        let mut map = self.entries.lock().unwrap();
        map.entry(build_id).or_insert_with(LiveLog::new).clone()
    }

    /// Look up the entry for `build_id`, or `None` if no build with
    /// that id is currently being streamed (already finished, or
    /// never started here).
    pub fn get(&self, build_id: i64) -> Option<Arc<LiveLog>> {
        self.entries.lock().unwrap().get(&build_id).cloned()
    }

    /// Drop the entry. Existing subscribers see their broadcast
    /// receiver close (because the sender — held only inside `Inner`
    /// — is dropped along with the last `Arc<LiveLog>`).
    pub fn close(&self, build_id: i64) {
        self.entries.lock().unwrap().remove(&build_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(text: &str) -> NomEvent {
        NomEvent::Raw {
            text: text.to_string(),
        }
    }

    #[tokio::test]
    async fn snapshot_and_subscribe_are_atomic() {
        let log = LiveLog::new();
        log.push(raw("hello"));
        let (snap, mut rx) = log.subscribe();
        assert_eq!(snap, [raw("hello")]);

        log.push(raw("world"));
        let event = rx.recv().await.unwrap();
        assert_eq!(event, raw("world"));
    }

    #[tokio::test]
    async fn close_drops_subscribers() {
        let reg = LiveLogRegistry::new();
        let log = reg.open(7);
        let (_snap, mut rx) = log.subscribe();
        drop(log);
        reg.close(7);
        // Sender lives only inside Inner; once the Arc drops, the
        // receiver observes Closed.
        assert!(rx.recv().await.is_err());
    }

    #[tokio::test]
    async fn open_is_idempotent() {
        let reg = LiveLogRegistry::new();
        let a = reg.open(1);
        a.push(raw("first"));
        let b = reg.open(1);
        b.push(raw("second"));
        let (snap, _rx) = a.subscribe();
        assert_eq!(snap, [raw("first"), raw("second")]);
    }
}
