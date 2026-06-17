//! Build-channel dispatcher.
//!
//! The piece that — given `(system, features, exclude_set)` — picks a
//! connected builder and opens a fresh SSH session channel into it. The
//! channel is used for one of the side-channel directions
//! (`ClosurePush` from daemon, `ClosurePull` from agent) or for sending
//! a `Build` control message; see [`crate::side_channel`].
//!
//! **Note on `in_flight`.** The dispatcher does not touch
//! `BuilderRegistry::in_flight`; the worker owns that counter and
//! increments exactly once per dispatched derivation so the status
//! page reflects running *builds* rather than open channels.

use crate::protocol::ControlMessage;
use crate::registry::{BuildLifecycle, BuilderRegistry};
use argunix_domain::BuilderName;
use russh::Channel;
use russh::server::Msg;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("no eligible builder for system `{system}`")]
    NoEligibleBuilder { system: String },
    #[error("opening channel to builder `{name}` failed; no fallback available")]
    AllOpensFailed { name: String },
    #[error("builder `{name}` is not currently registered")]
    NotRegistered { name: String },
    #[error("builder `{name}` has no SSH session (test fixture without a real connection)")]
    NoSession { name: String },
    #[error("opening channel to builder `{name}`: {source}")]
    OpenFailed {
        name: String,
        #[source]
        source: russh::Error,
    },
}

#[derive(Clone)]
pub struct BuilderDispatcher {
    registry: Arc<BuilderRegistry>,
}

impl BuilderDispatcher {
    pub fn new(registry: Arc<BuilderRegistry>) -> Self {
        Self { registry }
    }

    /// Pick the least-loaded eligible builder and open a fresh SSH
    /// session channel into it. Returns a [`DispatchedBuild`] holding
    /// the channel.
    ///
    /// On `channel_open_session` failure (builder dropped between
    /// eligibility check and open, or rejected the open), we walk to
    /// the next eligible candidate. Only when every candidate has been
    /// tried do we return `Err`.
    ///
    /// `in_flight` accounting is the worker's responsibility:
    /// the worker increments before calling here and decrements when
    /// the build finishes. This function does not touch the counter.
    pub async fn dispatch(
        &self,
        system: &str,
        features: &[String],
        exclude: &HashSet<u64>,
    ) -> Result<DispatchedBuild, DispatchError> {
        let eligible = self.registry.eligible(system, features, exclude);
        if eligible.is_empty() {
            return Err(DispatchError::NoEligibleBuilder {
                system: system.to_string(),
            });
        }
        let mut last_failed: Option<String> = None;
        for snap in eligible {
            let Some(session) = self.registry.session(&snap.name) else {
                // Raced; the entry was removed between `eligible` and
                // here. Try the next candidate.
                continue;
            };
            match session.handle.channel_open_session().await {
                Ok(channel) => {
                    tracing::debug!(builder = %snap.name, "build channel opened");
                    return Ok(DispatchedBuild {
                        name: snap.name,
                        channel: Some(channel),
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        builder = %snap.name,
                        error = %e,
                        "channel_open_session failed; trying next candidate",
                    );
                    last_failed = Some(snap.name.as_str().to_string());
                    continue;
                }
            }
        }
        Err(match last_failed {
            Some(n) => DispatchError::AllOpensFailed { name: n },
            None => DispatchError::NoEligibleBuilder {
                system: system.to_string(),
            },
        })
    }

    /// Open a fresh SSH session channel into a *specific* builder.
    /// The daemon-side worker uses this to open a `ClosurePush`
    /// channel (to ship the drv closure) and a `ClosurePull` channel
    /// (to fetch the built outputs); see [`crate::side_channel`] for
    /// the framing.
    ///
    /// Does not touch `in_flight` — see the file-level note.
    pub async fn open_channel(&self, name: &BuilderName) -> Result<DispatchedBuild, DispatchError> {
        let session = self
            .registry
            .session(name)
            .ok_or_else(|| DispatchError::NotRegistered {
                name: name.as_str().to_string(),
            })?;
        match session.handle.channel_open_session().await {
            Ok(channel) => Ok(DispatchedBuild {
                name: name.clone(),
                channel: Some(channel),
            }),
            Err(e) => Err(DispatchError::OpenFailed {
                name: name.as_str().to_string(),
                source: e,
            }),
        }
    }

    /// Register a build in the registry's in-flight map and send
    /// a `Build` control message on the named builder's control
    /// channel. Returns the lifecycle receiver. The worker drains it
    /// (BuildStarted → BuildLogChunk* → BuildFinished) and is
    /// responsible for calling [`BuilderRegistry::unregister_in_flight_build`]
    /// (or [`Self::abort_build`]) on completion / cancellation.
    ///
    /// Registration happens *before* the wire write so a fast agent
    /// can never produce a BuildStarted that arrives at the
    /// connection handler before the worker's mpsc is in place.
    pub async fn dispatch_build(
        &self,
        name: &BuilderName,
        build_id: i64,
        drv_path: String,
        gc_root: Option<String>,
        timeout_secs: u64,
        max_log_bytes: u64,
    ) -> Result<mpsc::Receiver<BuildLifecycle>, DispatchError> {
        let session = self
            .registry
            .session(name)
            .ok_or_else(|| DispatchError::NotRegistered {
                name: name.as_str().to_string(),
            })?;

        let rx = self
            .registry
            .register_in_flight_build(name.clone(), build_id);

        let msg = ControlMessage::Build {
            build_id,
            drv_path,
            gc_root,
            timeout_secs,
            max_log_bytes,
        };
        let bytes: bytes::Bytes = msg.encode_line().into();
        if session
            .handle
            .data(session.control_channel, bytes)
            .await
            .is_err()
        {
            // Couldn't write the message — undo the registration so
            // the registry doesn't leak a sender that never gets
            // unregistered by a worker that won't run.
            self.registry.unregister_in_flight_build(name, build_id);
            return Err(DispatchError::OpenFailed {
                name: name.as_str().to_string(),
                source: russh::Error::SendError,
            });
        }
        Ok(rx)
    }

    /// Send an `Abort` control message on the named builder's
    /// control channel and unregister the in-flight entry. The
    /// worker should still drain its lifecycle receiver until
    /// `Finished{Killed}` arrives so `BuildSlot` accounting closes.
    /// Idempotent: returns Ok even if the build was already
    /// unregistered (e.g. a race with the worker's own success path).
    pub async fn abort_build(
        &self,
        name: &BuilderName,
        build_id: i64,
    ) -> Result<(), DispatchError> {
        let session = self
            .registry
            .session(name)
            .ok_or_else(|| DispatchError::NotRegistered {
                name: name.as_str().to_string(),
            })?;
        let bytes: bytes::Bytes = ControlMessage::Abort { build_id }.encode_line().into();
        // Best-effort send; if the channel is already torn down,
        // unregistering will let the worker observe a closed mpsc.
        let _ = session.handle.data(session.control_channel, bytes).await;
        Ok(())
    }
}

/// Owner of an opened build channel against a registered builder.
/// `in_flight` accounting is the worker's responsibility (it
/// increments once per dispatched derivation), so this struct is
/// just a typed channel-plus-name wrapper.
pub struct DispatchedBuild {
    pub name: BuilderName,
    channel: Option<Channel<Msg>>,
}

impl DispatchedBuild {
    /// Take the SSH channel out of the wrapper. The caller is
    /// responsible for closing it; nothing else happens on drop.
    pub fn take_channel(&mut self) -> Option<Channel<Msg>> {
        self.channel.take()
    }
}
