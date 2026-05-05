//! Build-channel dispatcher.
//!
//! The piece that — given `(system, features, exclude_set)` — picks a
//! connected builder and opens a fresh SSH session channel into it, on
//! which medusa will speak `nix-store --serve --write`.
//!
//! **Note on `in_flight`** (M14). The dispatcher used to inc/dec
//! `BuilderRegistry::in_flight` around each opened channel, which made
//! `in_flight` a count of *open SSH channels* rather than running
//! builds. nix's ssh-ng store opens multiple channels per realise call
//! (substitution probes, path queries, the actual build), so the count
//! routinely overstated the real load — the status page showed e.g. 9
//! "in flight" while only 1 derivation was building. The counter is
//! now owned by the build worker and incremented exactly once per
//! dispatched derivation; the channel layer (this file +
//! `socket_server`) leaves it alone.

use crate::registry::BuilderRegistry;
use medusa_domain::BuilderName;
use russh::Channel;
use russh::server::Msg;
use std::collections::HashSet;
use std::sync::Arc;

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
    /// `in_flight` accounting is the worker's responsibility (M14):
    /// the worker increments before calling here and decrements when
    /// the build finishes. This function does not touch the counter.
    pub async fn dispatch(
        &self,
        system: &str,
        features: &[String],
        exclude: &HashSet<BuilderName>,
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
    /// Used by the socket-server proxy: when the build worker has
    /// already picked the builder, the proxy forwards `medusa-pipe`
    /// bytes onto a fresh channel without an `eligible` walk.
    ///
    /// Does not touch `in_flight` — see the file-level note. The
    /// worker is responsible for the counter; one realise call may
    /// open multiple channels for substitution / path-query / build,
    /// and counting them all would over-report load.
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
}

/// Owner of an opened build channel against a registered builder.
/// In M13 this also held an `in_flight` slot via Drop; M14 moved that
/// accounting to the worker, so this struct is now just a typed
/// channel-plus-name wrapper.
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
