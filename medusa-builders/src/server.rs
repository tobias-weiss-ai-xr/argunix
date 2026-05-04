use crate::auth::AuthState;
use crate::host_key::HostKey;
use medusa_domain::BuilderPubkey;
use medusa_store::{BuilderStore, SqlxStore};
use russh::keys::PrivateKey;
use russh::keys::ssh_key;
use russh::server::{Auth, Handler, Msg, Server, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("running russh server on {addr}: {source}")]
    Run {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("converting host key: {0}")]
    HostKey(String),
    #[error("store: {0}")]
    Store(#[from] medusa_store::StoreError),
}

/// Inputs to spin up the embedded SSH server. One struct so `run` has a
/// stable signature even when we add knobs (idle timeout, max channels,
/// allow-list, …) later.
pub struct ServerConfig {
    pub listen: SocketAddr,
    pub host_key: HostKey,
    /// Bytes of the shared enrollment token. Compared verbatim against the
    /// password-method credential. Constant-time compare via `constant_time_eq`
    /// below; the API takes the bytes so the caller can reload from the
    /// credentials file on a hot-reload without re-deriving anything.
    pub enrollment_token: Arc<Vec<u8>>,
    pub store: Arc<SqlxStore>,
}

/// Marker name kept for `pub use` consumers; the actual entry point is
/// the free function [`run`].
pub struct BuilderServer;

impl BuilderServer {
    /// Run the listener until cancelled. Constructs an `russh::server::Config`
    /// with the supplied host key and the medusa auth method set
    /// (password OR publickey).
    pub async fn run(cfg: ServerConfig) -> Result<(), ServerError> {
        let key =
            signing_key_to_russh(&cfg.host_key).map_err(|e| ServerError::HostKey(e.to_string()))?;

        let methods: MethodSet = (&[MethodKind::Password, MethodKind::PublicKey][..]).into();
        let russh_cfg = russh::server::Config {
            keys: vec![key],
            methods,
            auth_rejection_time: std::time::Duration::from_secs(1),
            inactivity_timeout: Some(std::time::Duration::from_secs(120)),
            ..Default::default()
        };
        let russh_cfg = Arc::new(russh_cfg);

        let mut server = ServerInner {
            store: cfg.store,
            enrollment_token: cfg.enrollment_token,
        };
        let listen = cfg.listen;
        server
            .run_on_address(russh_cfg, listen)
            .await
            .map_err(|source| ServerError::Run {
                addr: listen,
                source,
            })?;
        Ok(())
    }
}

#[derive(Clone)]
struct ServerInner {
    store: Arc<SqlxStore>,
    enrollment_token: Arc<Vec<u8>>,
}

impl Server for ServerInner {
    type Handler = ConnectionHandler;
    fn new_client(&mut self, _peer: Option<SocketAddr>) -> ConnectionHandler {
        ConnectionHandler {
            store: self.store.clone(),
            enrollment_token: self.enrollment_token.clone(),
            state: Arc::new(Mutex::new(AuthState::Unauthenticated)),
            offered_pubkey: Arc::new(Mutex::new(None)),
        }
    }
}

/// One connection's worth of handler state. russh clones this per
/// auth/channel callback, so the mutable bits live behind `Arc<Mutex<_>>`.
#[derive(Clone)]
pub(crate) struct ConnectionHandler {
    store: Arc<SqlxStore>,
    enrollment_token: Arc<Vec<u8>>,
    state: Arc<Mutex<AuthState>>,
    /// Captured during pubkey-offered even when the key isn't (yet) in
    /// the `builders` table — needed so `FreshEnrollment` can carry the
    /// key the client used during the SSH handshake. Without this, when
    /// the token-auth path succeeds we would have no pubkey to persist.
    offered_pubkey: Arc<Mutex<Option<BuilderPubkey>>>,
}

impl ConnectionHandler {
    /// PR #5 reads this from the control-channel handler to decide
    /// whether an incoming `hello` is a fresh enrollment (writes a new
    /// `builders` row) or a refresh (overwrites capabilities on the
    /// existing row).
    #[allow(dead_code)]
    pub(crate) async fn auth_state(&self) -> AuthState {
        self.state.lock().await.clone()
    }
}

impl Handler for ConnectionHandler {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        // Constant-time comparison so timing doesn't leak token length /
        // matching prefix bytes.
        let presented = password.as_bytes();
        let expected: &[u8] = self.enrollment_token.as_ref();
        if !constant_time_eq(presented, expected) {
            tracing::warn!(user = %user, "builder auth_password rejected: token mismatch");
            return Ok(Auth::reject());
        }
        let pubkey = self.offered_pubkey.lock().await.clone();
        let new_state = match pubkey {
            Some(pk) => AuthState::FreshEnrollment { pubkey: pk },
            None => {
                tracing::warn!(
                    user = %user,
                    "builder auth_password accepted but no pubkey offered; \
                     fresh enrollment will be deferred until the agent \
                     re-presents its key"
                );
                AuthState::Unauthenticated
            }
        };
        *self.state.lock().await = new_state;
        tracing::info!(user = %user, "builder auth_password accepted");
        Ok(Auth::Accept)
    }

    async fn auth_publickey_offered(
        &mut self,
        _user: &str,
        public_key: &ssh_key::PublicKey,
    ) -> Result<Auth, Self::Error> {
        // Capture the offered key regardless of whether it ends up
        // matching a builders row. If pubkey-auth fails, the agent will
        // retry with the enrollment token (password method); we still
        // need the key bytes then to write the TOFU record.
        if let Some(pk) = ed25519_pubkey_bytes(public_key) {
            *self.offered_pubkey.lock().await = Some(pk);
        }
        // The offer phase is non-binding — the agent must follow up
        // with a signed `auth_publickey` to actually authenticate.
        Ok(Auth::Accept)
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        public_key: &ssh_key::PublicKey,
    ) -> Result<Auth, Self::Error> {
        let Some(raw) = ed25519_pubkey_bytes(public_key) else {
            tracing::warn!(user = %user, "builder auth_publickey rejected: non-ed25519 key");
            return Ok(Auth::reject());
        };
        *self.offered_pubkey.lock().await = Some(raw.clone());
        match self.store.find_active_by_pubkey(&raw).await {
            Ok(Some(record)) => {
                tracing::info!(
                    builder = %record.name,
                    user = %user,
                    "builder auth_publickey accepted",
                );
                *self.state.lock().await = AuthState::Established(record);
                Ok(Auth::Accept)
            }
            Ok(None) => {
                tracing::warn!(
                    user = %user,
                    "builder auth_publickey rejected: no active row matches",
                );
                Ok(Auth::reject())
            }
            Err(e) => {
                tracing::error!(error = %e, "store error during pubkey lookup");
                Ok(Auth::reject())
            }
        }
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        // PR #5 wires control + build channels. For now accept the open
        // request so handshake-level smoke tests can verify auth.
        Ok(true)
    }

    async fn data(
        &mut self,
        _channel: ChannelId,
        _data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Discarded until PR #5 implements the control channel.
        Ok(())
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

/// Extract the raw 32 bytes of an ed25519 ssh public key. Returns None
/// for any other algorithm.
fn ed25519_pubkey_bytes(public_key: &ssh_key::PublicKey) -> Option<BuilderPubkey> {
    let raw = public_key.key_data().ed25519().map(|k| k.0)?;
    BuilderPubkey::from_bytes(&raw).ok()
}

fn signing_key_to_russh(host_key: &HostKey) -> Result<PrivateKey, russh::keys::Error> {
    let seed = host_key.signing_key().to_bytes();
    let kp = ssh_key::private::Ed25519Keypair::from_seed(&seed);
    let kpd = ssh_key::private::KeypairData::Ed25519(kp);
    PrivateKey::new(kpd, "medusa-builders").map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_only_when_lengths_and_bytes_agree() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"abc", b""));
    }
}
