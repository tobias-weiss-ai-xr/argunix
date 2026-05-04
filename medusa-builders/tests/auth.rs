//! End-to-end auth-state-machine tests. We spin the medusa builder
//! server on an ephemeral port and drive it with a real russh client.
//! Verifies the four auth outcomes from `design/builders.md`:
//!
//! 1. Correct enrollment token (password) → accept (FreshEnrollment).
//! 2. Wrong enrollment token → reject.
//! 3. Pubkey matching an active `builders` row → accept (Established).
//! 4. Pubkey matching a *revoked* row → reject (forces re-enrollment).
//!
//! PR #5 will add a fifth case ("unknown pubkey, then fall back to
//! token") once the agent's reconnect protocol is wired up.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use medusa_builders::{BuilderServer, ServerConfig, load_or_generate};
use medusa_domain::{BuilderCapabilities, BuilderName, BuilderPubkey};
use medusa_store::{BuilderStore, NewBuilder, SqlxStore, open_in_memory};
use russh::client::{self, Handler};
use russh::keys::ssh_key::PrivateKey;
use russh::keys::{Algorithm, PrivateKeyWithHashAlg};

const ENROLL_TOKEN: &[u8] = b"test-enrollment-token";

struct AcceptAnyHostKey;
impl Handler for AcceptAnyHostKey {
    type Error = russh::Error;
    async fn check_server_key(
        &mut self,
        _: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

async fn spawn_server() -> (std::net::SocketAddr, Arc<SqlxStore>) {
    let pool = open_in_memory().await.unwrap();
    let store = Arc::new(SqlxStore::new(pool));
    let dir = tempfile::tempdir().unwrap();
    let host_key_path = dir.path().join("host_key");
    let host_key = load_or_generate(&host_key_path).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let cfg = ServerConfig {
        listen: addr,
        host_key,
        enrollment_token: Arc::new(ENROLL_TOKEN.to_vec()),
        store: store.clone(),
    };
    tokio::spawn(async move {
        // Best-effort: the test will tear the runtime down when it's done.
        let _ = BuilderServer::run(cfg).await;
    });

    // Race-tolerant wait for the listener inside russh to come up.
    for _ in 0..40 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // Keep tempdir alive for the duration of the test by leaking it; the
    // host_key file's bytes are loaded into memory at spawn time so the
    // file vanishing afterwards is harmless, but leaking keeps things tidy.
    std::mem::forget(dir);
    (addr, store)
}

async fn make_client(addr: std::net::SocketAddr) -> client::Handle<AcceptAnyHostKey> {
    let cfg = Arc::new(client::Config::default());
    client::connect(cfg, addr, AcceptAnyHostKey).await.unwrap()
}

fn fresh_client_key() -> (PrivateKey, BuilderPubkey) {
    let key = PrivateKey::random(&mut rand10::rng(), Algorithm::Ed25519).unwrap();
    let pk = BuilderPubkey::from_bytes(&key.public_key().key_data().ed25519().unwrap().0).unwrap();
    (key, pk)
}

fn caps() -> BuilderCapabilities {
    BuilderCapabilities {
        systems: vec!["x86_64-linux".into()],
        features: vec![],
        max_jobs: 1,
        nix_version: "test".into(),
    }
}

#[tokio::test]
async fn token_auth_accepts_correct_token() {
    let (addr, _store) = spawn_server().await;
    let mut client = make_client(addr).await;
    let result = client
        .authenticate_password("medusa-builder", std::str::from_utf8(ENROLL_TOKEN).unwrap())
        .await
        .unwrap();
    assert!(
        result.success(),
        "correct enrollment token should authenticate",
    );
}

#[tokio::test]
async fn token_auth_rejects_wrong_token() {
    let (addr, _store) = spawn_server().await;
    let mut client = make_client(addr).await;
    let result = client
        .authenticate_password("medusa-builder", "bogus-token")
        .await
        .unwrap();
    assert!(!result.success(), "wrong enrollment token must be rejected",);
}

#[tokio::test]
async fn pubkey_auth_accepts_known_active_row() {
    let (addr, store) = spawn_server().await;
    let (client_key, client_pubkey) = fresh_client_key();

    // Pre-seed an active builders row matching the client's pubkey —
    // the daemon would have written this when the builder first
    // enrolled with a token (PR #5 wires that path; here we assume it).
    let _ = <SqlxStore as BuilderStore>::upsert(
        &store,
        NewBuilder {
            name: BuilderName::new("preexisting").unwrap(),
            pubkey: client_pubkey,
            capabilities: caps(),
        },
        Utc::now(),
    )
    .await
    .unwrap();

    let mut client = make_client(addr).await;
    let result = client
        .authenticate_publickey(
            "medusa-builder",
            PrivateKeyWithHashAlg::new(Arc::new(client_key), None),
        )
        .await
        .unwrap();
    assert!(
        result.success(),
        "pubkey matching an active row should authenticate",
    );
}

#[tokio::test]
async fn pubkey_auth_rejects_unknown_key() {
    let (addr, _store) = spawn_server().await;
    let (client_key, _) = fresh_client_key();
    let mut client = make_client(addr).await;
    let result = client
        .authenticate_publickey(
            "medusa-builder",
            PrivateKeyWithHashAlg::new(Arc::new(client_key), None),
        )
        .await
        .unwrap();
    assert!(
        !result.success(),
        "pubkey not matching any builders row must be rejected",
    );
}

#[tokio::test]
async fn pubkey_auth_rejects_revoked_row() {
    let (addr, store) = spawn_server().await;
    let (client_key, client_pubkey) = fresh_client_key();

    // Seed an active row, then revoke it. Pubkey auth must skip revoked
    // rows (this is the path that forces a builder to re-enroll with a
    // fresh token after `medusactl builders revoke`).
    let _ = <SqlxStore as BuilderStore>::upsert(
        &store,
        NewBuilder {
            name: BuilderName::new("victim").unwrap(),
            pubkey: client_pubkey,
            capabilities: caps(),
        },
        Utc::now(),
    )
    .await
    .unwrap();
    let _ = <SqlxStore as BuilderStore>::revoke(&store, "victim", Utc::now())
        .await
        .unwrap();

    let mut client = make_client(addr).await;
    let result = client
        .authenticate_publickey(
            "medusa-builder",
            PrivateKeyWithHashAlg::new(Arc::new(client_key), None),
        )
        .await
        .unwrap();
    assert!(
        !result.success(),
        "revoked row must not authenticate via pubkey",
    );
}
