//! Coordinator-host stats ring for the `/hosts` page.
//!
//! Mirror of [`argunix_builders::BuilderRegistry`]'s per-builder stats
//! window, but for argunix's *own* host. A background tokio task ticks
//! the [`StatsSampler`] on the same 5s cadence builders use and pushes
//! samples here; the `/api/host/stats` handler snapshots the ring so
//! the `/hosts` page can render the same cpu / load / mem sparklines
//! it already draws for builders.

use argunix_builders::{StatsSample, StatsSampler};
use chrono::Utc;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;

/// Same depth the per-builder ring uses
/// (`registry::STATS_RING_CAPACITY`). ~5 minutes of history at 5s cadence.
const HOST_RING_CAPACITY: usize = 60;

/// Sampling cadence — match the agent's heartbeat period so the
/// coordinator and builder cards on `/hosts` advance in lockstep.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// In-memory ring of recent host stats samples. Cheap to clone (just an
/// `Arc`), safe to share across the daemon's worker, web router, and
/// the sampler task.
#[derive(Clone, Default)]
pub struct HostStatsRing {
    inner: Arc<Mutex<VecDeque<StatsSample>>>,
}

impl HostStatsRing {
    pub fn new() -> Self {
        Self::default()
    }

    fn push(&self, sample: StatsSample) {
        let mut ring = self.inner.lock().unwrap();
        if ring.len() == HOST_RING_CAPACITY {
            ring.pop_front();
        }
        ring.push_back(sample);
    }

    /// Snapshot the ring oldest-first for the JSON endpoint.
    pub fn snapshot(&self) -> Vec<StatsSample> {
        self.inner.lock().unwrap().iter().copied().collect()
    }
}

/// Spawn a tokio task that samples `/proc` every 5s and pushes into
/// `ring`. Returns the join handle so the caller can hold on to it for
/// the daemon's lifetime (drop = abort = sampler stops).
pub fn spawn_sampler(ring: HostStatsRing) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut sampler = StatsSampler::new();
        let mut tick = tokio::time::interval(SAMPLE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if let Some(stats) = sampler.sample() {
                ring.push(StatsSample {
                    ts: Utc::now(),
                    stats,
                });
            }
        }
    })
}
