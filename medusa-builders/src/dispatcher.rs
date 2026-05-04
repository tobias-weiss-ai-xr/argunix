//! Build-channel dispatcher.
//!
//! The piece that — given `(system, features, exclude_set)` — picks a
//! connected builder and opens a fresh SSH session channel into it, on
//! which medusa will speak `nix-store --serve --write`. PR #8 will
//! wire this into the build worker; PR #7 (this) is the standalone
//! mechanism + tests.

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

    /// Pick the least-loaded eligible builder, open a fresh SSH session
    /// channel into it, and return a [`DispatchedBuild`] guard. The
    /// guard decrements the builder's `in_flight` count on drop, so
    /// callers don't have to remember to release capacity.
    ///
    /// On `channel_open_session` failure (builder dropped between
    /// eligibility check and open, or rejected the open), we walk to
    /// the next eligible candidate. Only when every candidate has been
    /// tried do we return `Err`.
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
            // Reserve capacity before awaiting the open. A concurrent
            // dispatch on the same builder would otherwise see stale
            // in_flight counts and over-subscribe. If the open fails
            // we decrement back.
            self.registry.inc_in_flight(&snap.name);
            match session.handle.channel_open_session().await {
                Ok(channel) => {
                    tracing::debug!(builder = %snap.name, "build channel opened");
                    return Ok(DispatchedBuild {
                        registry: self.registry.clone(),
                        name: snap.name,
                        channel: Some(channel),
                    });
                }
                Err(e) => {
                    self.registry.dec_in_flight(&snap.name);
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
    /// Used by the socket-server proxy: nix's `--builders` arg lists
    /// every connected builder, nix picks one, then invokes
    /// `medusa-pipe <name>` for the chosen one — at which point we
    /// already know the builder, no `eligible` walk needed.
    pub async fn open_channel(&self, name: &BuilderName) -> Result<DispatchedBuild, DispatchError> {
        let session = self
            .registry
            .session(name)
            .ok_or_else(|| DispatchError::NotRegistered {
                name: name.as_str().to_string(),
            })?;
        // Reserve before await so concurrent dispatches see the
        // pending count.
        self.registry.inc_in_flight(name);
        match session.handle.channel_open_session().await {
            Ok(channel) => Ok(DispatchedBuild {
                registry: self.registry.clone(),
                name: name.clone(),
                channel: Some(channel),
            }),
            Err(e) => {
                self.registry.dec_in_flight(name);
                Err(DispatchError::OpenFailed {
                    name: name.as_str().to_string(),
                    source: e,
                })
            }
        }
    }
}

/// RAII handle for a build channel held against a registered builder.
/// Drop releases the builder's in-flight capacity.
pub struct DispatchedBuild {
    registry: Arc<BuilderRegistry>,
    pub name: BuilderName,
    channel: Option<Channel<Msg>>,
}

impl DispatchedBuild {
    /// Take the SSH channel out of the guard. The guard still
    /// decrements in_flight on drop; the channel is the caller's
    /// concern thereafter.
    pub fn take_channel(&mut self) -> Option<Channel<Msg>> {
        self.channel.take()
    }
}

impl Drop for DispatchedBuild {
    fn drop(&mut self) {
        self.registry.dec_in_flight(&self.name);
    }
}
