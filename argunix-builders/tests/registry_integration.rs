//! End-to-end tests for runtime registry semantics:
//! - Successful `hello` registers the builder as Active.
//! - `shutdown` flips the entry to Disconnecting.
//! - Connection drop removes the entry.
//! - A second connection under the same builder name displaces the
//!   first; the first sees its session disconnected.

use std::sync::Arc;
use std::time::Duration;

use argunix_builders::{
    BuilderRegistry, BuilderServer, ConnState, ControlMessage, ServerConfig, load_or_generate,
};
use argunix_domain::{BuilderCapabilities, BuilderName, BuilderPubkey};
use argunix_store::{BuilderStore, NewBuilder, SqlxStore, open_in_memory};
use chrono::Utc;
use russh::ChannelMsg;
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

async fn spawn_server() -> (std::net::SocketAddr, Arc<SqlxStore>, Arc<BuilderRegistry>) {
    let pool = open_in_memory().await.unwrap();
    let store = Arc::new(SqlxStore::new(pool));
    let dir = tempfile::tempdir().unwrap();
    let host_key = load_or_generate(&dir.path().join("host_key")).unwrap();
    let registry = BuilderRegistry::new();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let cfg = ServerConfig {
        listen: addr,
        host_key,
        enrollment_token: Arc::new(ENROLL_TOKEN.to_vec()),
        store: store.clone(),
        registry: registry.clone(),
    };
    tokio::spawn(async move {
        let _ = BuilderServer::run(cfg).await;
    });
    for _ in 0..40 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    std::mem::forget(dir);
    (addr, store, registry)
}

async fn connect(addr: std::net::SocketAddr) -> client::Handle<AcceptAnyHostKey> {
    let cfg = Arc::new(client::Config::default());
    client::connect(cfg, addr, AcceptAnyHostKey).await.unwrap()
}

fn fresh_client_key() -> (PrivateKey, BuilderPubkey) {
    let k = PrivateKey::random(&mut rand10::rng(), Algorithm::Ed25519).unwrap();
    let pk = BuilderPubkey::from_bytes(&k.public_key().key_data().ed25519().unwrap().0).unwrap();
    (k, pk)
}

fn caps(systems: &[&str], features: &[&str], max_jobs: u32) -> BuilderCapabilities {
    BuilderCapabilities {
        systems: systems.iter().map(|s| s.to_string()).collect(),
        features: features.iter().map(|s| s.to_string()).collect(),
        max_jobs,
        nix_version: "test".into(),
    }
}

async fn await_welcome(channel: &mut russh::Channel<russh::client::Msg>) -> Option<ControlMessage> {
    let mut buf: Vec<u8> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, channel.wait()).await {
            Err(_) => return None,
            Ok(None) => return None,
            Ok(Some(ChannelMsg::Data { data })) => {
                buf.extend_from_slice(&data);
                if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line = &buf[..pos];
                    if let Ok(s) = std::str::from_utf8(line) {
                        if let Ok(msg) = serde_json::from_str::<ControlMessage>(s) {
                            return Some(msg);
                        }
                    }
                    return None;
                }
            }
            Ok(Some(_)) => continue,
        }
    }
}

/// Wait until `pred(snap)` returns true for the named builder, or the
/// deadline passes. Avoids racing the test against the server's
/// post-callback bookkeeping.
async fn wait_for<F>(reg: &BuilderRegistry, name: &str, mut pred: F) -> bool
where
    F: FnMut(&argunix_builders::BuilderSnapshot) -> bool,
{
    let bn = BuilderName::new(name).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(s) = reg.snapshot(&bn) {
            if pred(&s) {
                return true;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_absent(reg: &BuilderRegistry, name: &str) -> bool {
    let bn = BuilderName::new(name).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if reg.snapshot(&bn).is_none() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn hello_registers_builder_as_active() {
    let (addr, store, registry) = spawn_server().await;
    let (client_key, client_pubkey) = fresh_client_key();
    let _ = <SqlxStore as BuilderStore>::upsert(
        &store,
        NewBuilder {
            name: BuilderName::new("alice").unwrap(),
            pubkey: client_pubkey,
            capabilities: caps(&["x86_64-linux"], &[], 1),
        },
        Utc::now(),
    )
    .await
    .unwrap();

    let mut session = connect(addr).await;
    let _ = session
        .authenticate_publickey(
            "argunix-builder",
            PrivateKeyWithHashAlg::new(Arc::new(client_key), None),
        )
        .await
        .unwrap();
    let mut channel = session.channel_open_session().await.unwrap();
    let hello = ControlMessage::Hello {
        name: BuilderName::new("alice").unwrap(),
        systems: vec!["x86_64-linux".into()],
        features: vec![],
        max_jobs: 1,
        nix_version: "2.18.1".into(),
    };
    channel.data(&hello.encode_line()[..]).await.unwrap();
    let _ = await_welcome(&mut channel).await.unwrap();

    assert!(
        wait_for(&registry, "alice", |s| s.state == ConnState::Active).await,
        "registry must show alice as Active after hello",
    );
}

#[tokio::test]
async fn shutdown_message_flips_state_to_disconnecting() {
    let (addr, store, registry) = spawn_server().await;
    let (client_key, client_pubkey) = fresh_client_key();
    let _ = <SqlxStore as BuilderStore>::upsert(
        &store,
        NewBuilder {
            name: BuilderName::new("bob").unwrap(),
            pubkey: client_pubkey,
            capabilities: caps(&["x86_64-linux"], &[], 1),
        },
        Utc::now(),
    )
    .await
    .unwrap();
    let mut session = connect(addr).await;
    let _ = session
        .authenticate_publickey(
            "argunix-builder",
            PrivateKeyWithHashAlg::new(Arc::new(client_key), None),
        )
        .await
        .unwrap();
    let mut channel = session.channel_open_session().await.unwrap();
    let hello = ControlMessage::Hello {
        name: BuilderName::new("bob").unwrap(),
        systems: vec!["x86_64-linux".into()],
        features: vec![],
        max_jobs: 1,
        nix_version: "2.18.1".into(),
    };
    channel.data(&hello.encode_line()[..]).await.unwrap();
    let _ = await_welcome(&mut channel).await.unwrap();
    assert!(wait_for(&registry, "bob", |s| s.state == ConnState::Active).await);

    let bye = ControlMessage::Shutdown {
        reason: "going to bed".into(),
        drain: false,
    };
    channel.data(&bye.encode_line()[..]).await.unwrap();
    assert!(
        wait_for(&registry, "bob", |s| s.state == ConnState::Disconnecting).await,
        "shutdown must flip state to Disconnecting",
    );
}

#[tokio::test]
async fn connection_drop_removes_entry() {
    let (addr, store, registry) = spawn_server().await;
    let (client_key, client_pubkey) = fresh_client_key();
    let _ = <SqlxStore as BuilderStore>::upsert(
        &store,
        NewBuilder {
            name: BuilderName::new("ephemeral").unwrap(),
            pubkey: client_pubkey,
            capabilities: caps(&["x86_64-linux"], &[], 1),
        },
        Utc::now(),
    )
    .await
    .unwrap();
    {
        let mut session = connect(addr).await;
        let _ = session
            .authenticate_publickey(
                "argunix-builder",
                PrivateKeyWithHashAlg::new(Arc::new(client_key), None),
            )
            .await
            .unwrap();
        let mut channel = session.channel_open_session().await.unwrap();
        let hello = ControlMessage::Hello {
            name: BuilderName::new("ephemeral").unwrap(),
            systems: vec!["x86_64-linux".into()],
            features: vec![],
            max_jobs: 1,
            nix_version: "2.18.1".into(),
        };
        channel.data(&hello.encode_line()[..]).await.unwrap();
        let _ = await_welcome(&mut channel).await.unwrap();
        assert!(wait_for(&registry, "ephemeral", |s| s.state == ConnState::Active).await);

        // Disconnect the client cleanly; russh's drop on session ends
        // the connection.
        let _ = session
            .disconnect(russh::Disconnect::ByApplication, "test done", "en")
            .await;
    }

    assert!(
        wait_for_absent(&registry, "ephemeral").await,
        "connection drop must remove the registry entry",
    );
}

#[tokio::test]
async fn duplicate_name_displaces_old_connection() {
    let (addr, store, registry) = spawn_server().await;

    // Two distinct keys: argunix upserts the row by name on hello.
    let (key_a, pubkey_a) = fresh_client_key();
    let (key_b, pubkey_b) = fresh_client_key();
    let _ = <SqlxStore as BuilderStore>::upsert(
        &store,
        NewBuilder {
            name: BuilderName::new("dup").unwrap(),
            pubkey: pubkey_a,
            capabilities: caps(&["x86_64-linux"], &[], 1),
        },
        Utc::now(),
    )
    .await
    .unwrap();

    // First connection.
    let mut session_a = connect(addr).await;
    let _ = session_a
        .authenticate_publickey(
            "argunix-builder",
            PrivateKeyWithHashAlg::new(Arc::new(key_a), None),
        )
        .await
        .unwrap();
    let mut channel_a = session_a.channel_open_session().await.unwrap();
    let hello = ControlMessage::Hello {
        name: BuilderName::new("dup").unwrap(),
        systems: vec!["x86_64-linux".into()],
        features: vec![],
        max_jobs: 1,
        nix_version: "2.18.1".into(),
    };
    channel_a.data(&hello.encode_line()[..]).await.unwrap();
    let _ = await_welcome(&mut channel_a).await.unwrap();

    let snap_a = registry
        .snapshot(&BuilderName::new("dup").unwrap())
        .unwrap();

    // Second connection: same name, different pubkey. Auth via token
    // (so the new pubkey is captured for FreshEnrollment); on hello,
    // argunix upserts the row with pubkey_b and the registry entry is
    // displaced.
    let mut session_b = connect(addr).await;
    let _ = session_b
        .authenticate_publickey(
            "argunix-builder",
            PrivateKeyWithHashAlg::new(Arc::new(key_b), None),
        )
        .await
        .unwrap();
    let _ = session_b
        .authenticate_password(
            "argunix-builder",
            std::str::from_utf8(ENROLL_TOKEN).unwrap(),
        )
        .await
        .unwrap();
    let mut channel_b = session_b.channel_open_session().await.unwrap();
    channel_b.data(&hello.encode_line()[..]).await.unwrap();
    let _ = await_welcome(&mut channel_b).await.unwrap();

    // Wait until the registry's connected_since changes — that's how
    // we know the takeover landed.
    assert!(
        wait_for(&registry, "dup", |s| s.connected_since
            > snap_a.connected_since)
        .await,
        "registry should reflect the new connection's connected_since",
    );

    // Sqlite row got overwritten with pubkey_b. The first connection's
    // session was sent a Kick + disconnected; verify we can no longer
    // exchange data on it (russh's pump returns an error or closes).
    let row = <SqlxStore as BuilderStore>::find_by_name(&store, "dup")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.pubkey, pubkey_b);

    // Try to send another heartbeat on channel_a; it should fail
    // because the server disconnected the displaced session.
    let hb = ControlMessage::Heartbeat { ts: 1, load: None };
    let send = channel_a.data(&hb.encode_line()[..]).await;
    let recv = tokio::time::timeout(Duration::from_secs(2), channel_a.wait()).await;
    assert!(
        send.is_err() || matches!(recv, Ok(None) | Ok(Some(ChannelMsg::Close))),
        "displaced channel must be closed",
    );
}
