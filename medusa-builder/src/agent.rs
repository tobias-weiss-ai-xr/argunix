//! Dial medusa, authenticate, hello, heartbeat, accept build channels.
//!
//! Runs forever (with reconnect-and-backoff) until the supplied
//! `shutdown` future fires. On clean shutdown, sends a `shutdown`
//! control message before closing the session so medusa logs the
//! event at INFO and immediately marks the registry entry
//! `Disconnecting`.

use crate::identity::PersistedKey;
use medusa_builders::ControlMessage;
use medusa_domain::{BuilderCapabilities, BuilderName};
use russh::ChannelMsg;
use russh::client::{self, Handle, Handler, Msg as ClientMsg, Session as ClientSession};
use russh::keys::ssh_key::{self, PrivateKey};
use russh::keys::{Algorithm, PrivateKeyWithHashAlg};
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("converting identity to russh key: {0}")]
    IdentityKey(String),
    #[error("connecting to medusa at {addr}: {source}")]
    Connect {
        addr: SocketAddr,
        #[source]
        source: russh::Error,
    },
    #[error("ssh: {0}")]
    Ssh(#[from] russh::Error),
    #[error("auth refused: pubkey rejected and no enrollment token configured")]
    PubkeyRejectedNoToken,
    #[error("auth refused: pubkey AND token both rejected")]
    AllAuthRejected,
}

/// Inputs to the agent loop. The caller assembles these from CLI /
/// config; tests construct them directly.
#[derive(Clone)]
pub struct AgentConfig {
    pub medusa: SocketAddr,
    pub identity: PersistedKey,
    /// `None` after first successful enrollment; the caller wipes
    /// the field once the daemon's TOFU row is in place.
    pub enrollment_token: Option<Arc<Vec<u8>>>,
    pub name: BuilderName,
    pub capabilities: BuilderCapabilities,
    /// Initial reconnect backoff. Doubles up to 30s on repeated failures;
    /// resets to this value on every successful hello.
    pub reconnect_initial_backoff: Duration,
}

impl AgentConfig {
    pub fn default_backoff() -> Duration {
        Duration::from_secs(2)
    }
}

/// The agent's main loop. Returns `Ok(())` on clean shutdown.
/// Reconnects with exponential backoff on transport / auth failures
/// (auth failure that's clearly fatal — no token configured — bails
/// out instead of looping forever).
pub async fn run<S: Future<Output = ()>>(cfg: AgentConfig, shutdown: S) -> Result<(), AgentError> {
    tokio::pin!(shutdown);
    let mut backoff = cfg.reconnect_initial_backoff;
    loop {
        // The connect-and-serve cycle returns either:
        //   - Ok(true)  — clean shutdown signalled by the caller
        //   - Ok(false) — disconnected; the caller wants a reconnect
        //   - Err(_)    — fatal error (bad token + bad pubkey)
        let cycle = serve_one_connection(&cfg, &mut shutdown);
        match cycle.await {
            Ok(true) => {
                tracing::info!("agent shutdown complete");
                return Ok(());
            }
            Ok(false) => {
                tracing::info!(
                    backoff_ms = backoff.as_millis() as u64,
                    "agent reconnecting after disconnect",
                );
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = &mut shutdown => return Ok(()),
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
            Err(AgentError::PubkeyRejectedNoToken) => {
                // Fatal — the only reason a builder would be in this
                // state is operator revocation without re-issuing a
                // token. Nothing to retry.
                return Err(AgentError::PubkeyRejectedNoToken);
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    backoff_ms = backoff.as_millis() as u64,
                    "agent connection cycle errored; backing off",
                );
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = &mut shutdown => return Ok(()),
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

async fn serve_one_connection(
    cfg: &AgentConfig,
    shutdown: &mut std::pin::Pin<&mut impl Future<Output = ()>>,
) -> Result<bool, AgentError> {
    // Build the russh PrivateKey from our 32-byte seed (same shape
    // as the server-side host key conversion).
    let key = identity_to_russh_private_key(&cfg.identity)
        .map_err(|e| AgentError::IdentityKey(e.to_string()))?;
    let key = Arc::new(key);

    let client_cfg = Arc::new(client::Config::default());
    let mut session = client::connect(client_cfg, cfg.medusa, AgentClient)
        .await
        .map_err(|source| AgentError::Connect {
            addr: cfg.medusa,
            source,
        })?;

    // Try pubkey first. On the daemon's first contact the row
    // doesn't exist yet → pubkey auth fails → fall back to token.
    // After successful enrollment the daemon's TOFU row matches our
    // pubkey on every subsequent connect.
    let user = "medusa-builder";
    let pubkey_attempt = session
        .authenticate_publickey(user, PrivateKeyWithHashAlg::new(key, None))
        .await
        .map_err(AgentError::Ssh)?;
    if !pubkey_attempt.success() {
        match &cfg.enrollment_token {
            None => return Err(AgentError::PubkeyRejectedNoToken),
            Some(tok) => {
                let token_str = std::str::from_utf8(tok.as_ref()).unwrap_or("");
                let pw = session
                    .authenticate_password(user, token_str)
                    .await
                    .map_err(AgentError::Ssh)?;
                if !pw.success() {
                    return Err(AgentError::AllAuthRejected);
                }
                tracing::info!("enrolled with token; subsequent connects will use pubkey");
            }
        }
    } else {
        tracing::info!("authenticated via pubkey");
    }

    // Open the control channel and send hello.
    let mut channel = session.channel_open_session().await?;
    let hello = ControlMessage::Hello {
        name: cfg.name.clone(),
        systems: cfg.capabilities.systems.clone(),
        features: cfg.capabilities.features.clone(),
        max_jobs: cfg.capabilities.max_jobs,
        nix_version: cfg.capabilities.nix_version.clone(),
    };
    channel.data(&hello.encode_line()[..]).await?;

    // Drain control until welcome arrives. Drop trailing data into
    // the heartbeat loop's framer.
    let welcome_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut welcome_seen = false;
    let mut framer = medusa_builders::LineFramer::new();
    while tokio::time::Instant::now() < welcome_deadline {
        let remaining = welcome_deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, channel.wait()).await {
            Ok(Some(ChannelMsg::Data { data })) => {
                for parsed in framer.extend(&data) {
                    if let Ok(ControlMessage::Welcome { builder_id }) = parsed {
                        tracing::info!(builder_id = %builder_id, "registered with medusa");
                        welcome_seen = true;
                        break;
                    }
                }
                if welcome_seen {
                    break;
                }
            }
            Ok(Some(ChannelMsg::Close)) | Ok(None) | Err(_) => break,
            _ => continue,
        }
    }
    if !welcome_seen {
        // medusa closed the channel without welcoming us. Force a
        // reconnect (the next attempt may fare differently).
        return Ok(false);
    }

    // Heartbeat + shutdown loop.
    let mut hb_interval = tokio::time::interval(Duration::from_secs(30));
    hb_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Skip the immediate first tick.
    hb_interval.tick().await;

    loop {
        tokio::select! {
            _ = &mut *shutdown => {
                let bye = ControlMessage::Shutdown {
                    reason: "agent stopping".into(),
                    drain: false,
                };
                let _ = channel.data(&bye.encode_line()[..]).await;
                let _ = channel.close().await;
                let _ = session.disconnect(
                    russh::Disconnect::ByApplication,
                    "agent shutdown",
                    "en",
                ).await;
                return Ok(true);
            }
            _ = hb_interval.tick() => {
                let hb = ControlMessage::Heartbeat {
                    ts: chrono::Utc::now().timestamp(),
                    load: None,
                };
                if channel.data(&hb.encode_line()[..]).await.is_err() {
                    return Ok(false);
                }
            }
            ev = channel.wait() => match ev {
                Some(ChannelMsg::Data { data }) => {
                    for parsed in framer.extend(&data) {
                        match parsed {
                            Ok(ControlMessage::Kick { reason }) => {
                                tracing::warn!(reason = %reason, "kicked by medusa; reconnect after backoff");
                                return Ok(false);
                            }
                            Ok(other) => {
                                tracing::warn!(?other, "ignoring unexpected server-to-builder message");
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "control line dropped");
                            }
                        }
                    }
                }
                Some(ChannelMsg::Close) | None => return Ok(false),
                Some(_) => continue,
            }
        }
    }
}

/// russh client Handler. PR slice for build-channel handling lands
/// in the next iteration; for now the default `server_channel_open_session`
/// just drops the channel, which is correct for the heartbeat-only
/// lifecycle of this slice.
#[derive(Clone)]
struct AgentClient;

impl Handler for AgentClient {
    type Error = russh::Error;
    async fn check_server_key(&mut self, _: &ssh_key::PublicKey) -> Result<bool, Self::Error> {
        // M13b: TOFU on the medusa host key. The agent stores the
        // first-seen host pubkey in <state-dir>/medusa-host-key and
        // refuses to connect if it ever changes — same trust shape
        // as the daemon's view of the agent. Slice for that lands
        // alongside the build-channel handling; today we accept any
        // host key so the test harness works.
        Ok(true)
    }
    async fn server_channel_open_session(
        &mut self,
        _channel: russh::Channel<ClientMsg>,
        _session: &mut ClientSession,
    ) -> Result<(), Self::Error> {
        // Build-channel handling slice. For now drop the channel —
        // medusa will see the channel close and treat the dispatch
        // as a transport failure (re-queue with anti-affinity, capped
        // at 3, per PR #2). Tests of the heartbeat-only path don't
        // exercise this path.
        Ok(())
    }
}

// Suppress unused-import warning on `Handle` when only Handler is in use.
#[allow(dead_code)]
type _HandleAlias<T> = Handle<T>;

fn identity_to_russh_private_key(
    identity: &PersistedKey,
) -> Result<PrivateKey, russh::keys::Error> {
    let _ = Algorithm::Ed25519; // keep import live across feature toggles
    let seed = identity.signing_key().to_bytes();
    let kp = ssh_key::private::Ed25519Keypair::from_seed(&seed);
    let kpd = ssh_key::private::KeypairData::Ed25519(kp);
    PrivateKey::new(kpd, "medusa-builder").map_err(Into::into)
}
