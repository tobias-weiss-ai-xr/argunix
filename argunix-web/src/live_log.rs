//! In-memory tap on a build's stderr stream so the web UI can SSE-tail
//! a running build. The worker pushes raw chunks here as it receives
//! them from the agent's `BuildLogChunk` frames; each SSE subscriber
//! atomically snapshots the buffered prefix and subscribes to a
//! tokio broadcast for everything that arrives after.
//!
//! Strictly in-memory — no persistence. Coordinator restart wipes
//! every running tail, which is intentional: post-build the static
//! log endpoint serves the zstd-compressed final log from disk.

use bytes::Bytes;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

const BROADCAST_CAPACITY: usize = 256;

/// Per-build state. Buffer + broadcaster are coupled under one mutex
/// so a subscriber's snapshot-then-subscribe is atomic relative to
/// pushes — no chunk can land in the gap and be missed or replayed.
pub struct LiveLog {
    inner: Mutex<Inner>,
}

struct Inner {
    buf: Vec<u8>,
    tx: broadcast::Sender<Bytes>,
}

impl LiveLog {
    pub fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Arc::new(Self {
            inner: Mutex::new(Inner {
                buf: Vec::new(),
                tx,
            }),
        })
    }

    /// Append a chunk and fan it out to live subscribers. `send`
    /// errors are ignored — they only mean no subscribers, which is
    /// the common case (most builds finish without anyone watching).
    pub fn push(&self, bytes: &[u8]) {
        let mut g = self.inner.lock().unwrap();
        g.buf.extend_from_slice(bytes);
        let _ = g.tx.send(Bytes::copy_from_slice(bytes));
    }

    /// Take a snapshot of the buffered prefix and a subscription to
    /// future chunks. The two are produced under one lock so the
    /// caller cannot miss bytes (or see them twice) in the gap
    /// between snapshot and subscribe.
    pub fn subscribe(&self) -> (Vec<u8>, broadcast::Receiver<Bytes>) {
        let g = self.inner.lock().unwrap();
        (g.buf.clone(), g.tx.subscribe())
    }
}

/// Coordinator-wide registry of live build logs. Worker calls
/// [`Self::open`] on `BuildStarted`, [`Self::push`] on each chunk,
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

    #[tokio::test]
    async fn snapshot_and_subscribe_are_atomic() {
        let log = LiveLog::new();
        log.push(b"hello ");
        let (snap, mut rx) = log.subscribe();
        assert_eq!(snap, b"hello ");

        log.push(b"world");
        let chunk = rx.recv().await.unwrap();
        assert_eq!(&chunk[..], b"world");
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
        a.push(b"first ");
        let b = reg.open(1);
        b.push(b"second");
        let (snap, _rx) = a.subscribe();
        assert_eq!(snap, b"first second");
    }
}
