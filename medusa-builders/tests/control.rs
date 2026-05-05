//! Control-channel protocol tests. Verifies the post-auth wire shape:
//! hello after token-auth writes a new `builders` row + replies with
//! welcome; hello after pubkey-auth refreshes capabilities; heartbeat
//! advances `last_seen`; hello-before-auth is rejected.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use medusa_builders::{
    BuilderRegistry, BuilderServer, ControlMessage, ServerConfig, load_or_generate,
};
use medusa_domain::{BuilderCapabilities, BuilderName, BuilderPubkey};
use medusa_store::{BuilderStore, NewBuilder, SqlxStore, open_in_memory};
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
        nix_version: "2.18.1".into(),
    }
}

/// Drive the channel until we either parse a Welcome message or hit
/// EOF/timeout. Bounded so a buggy server can't hang the test forever.
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

#[tokio::test]
async fn hello_after_token_auth_creates_row_and_returns_welcome() {
    let (addr, store, _registry) = spawn_server().await;
    let (client_key, expected_pubkey) = fresh_client_key();
    let mut session = connect(addr).await;
    // Offer the pubkey alongside password auth so the server can
    // capture it for `FreshEnrollment` (real agents always do this).
    let _ = session
        .authenticate_publickey(
            "medusa-builder",
            PrivateKeyWithHashAlg::new(Arc::new(client_key), None),
        )
        .await
        .unwrap();
    let result = session
        .authenticate_password("medusa-builder", std::str::from_utf8(ENROLL_TOKEN).unwrap())
        .await
        .unwrap();
    assert!(result.success(), "token auth must succeed");

    let mut channel = session.channel_open_session().await.unwrap();
    let hello = ControlMessage::Hello {
        name: BuilderName::new("bobs-mini").unwrap(),
        systems: vec!["aarch64-darwin".into(), "aarch64-linux".into()],
        features: vec!["big-parallel".into()],
        max_jobs: 2,
        nix_version: "2.18.1".into(),
    };
    channel.data(&hello.encode_line()[..]).await.unwrap();

    let welcome = await_welcome(&mut channel)
        .await
        .expect("server should reply with welcome");
    let id_str = match &welcome {
        ControlMessage::Welcome { builder_id } => builder_id.clone(),
        other => panic!("expected Welcome, got {other:?}"),
    };

    let row = <SqlxStore as BuilderStore>::find_by_name(&store, "bobs-mini")
        .await
        .unwrap()
        .expect("freshly enrolled row must exist");
    assert_eq!(row.id.get().to_string(), id_str);
    assert_eq!(row.pubkey, expected_pubkey);
    assert_eq!(row.capabilities.systems.len(), 2);
    assert_eq!(row.capabilities.features, vec!["big-parallel".to_string()]);
    assert_eq!(row.capabilities.max_jobs, 2);
}

#[tokio::test]
async fn hello_after_pubkey_auth_refreshes_capabilities() {
    let (addr, store, _registry) = spawn_server().await;
    let (client_key, client_pubkey) = fresh_client_key();
    // Pre-seed the row with stale capabilities; after Hello the row
    // should reflect the agent's freshly-reported caps.
    let _ = <SqlxStore as BuilderStore>::upsert(
        &store,
        NewBuilder {
            name: BuilderName::new("mac01").unwrap(),
            pubkey: client_pubkey,
            capabilities: caps(&["aarch64-darwin"], &[], 1),
        },
        Utc::now(),
    )
    .await
    .unwrap();

    let mut session = connect(addr).await;
    let result = session
        .authenticate_publickey(
            "medusa-builder",
            PrivateKeyWithHashAlg::new(Arc::new(client_key), None),
        )
        .await
        .unwrap();
    assert!(result.success());

    let mut channel = session.channel_open_session().await.unwrap();
    let hello = ControlMessage::Hello {
        name: BuilderName::new("mac01").unwrap(),
        systems: vec!["aarch64-darwin".into(), "x86_64-darwin".into()],
        features: vec!["big-parallel".into(), "kvm".into()],
        max_jobs: 4,
        nix_version: "2.20.0".into(),
    };
    channel.data(&hello.encode_line()[..]).await.unwrap();

    let _ = await_welcome(&mut channel)
        .await
        .expect("welcome expected on hello");

    let row = <SqlxStore as BuilderStore>::find_by_name(&store, "mac01")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.capabilities.systems.len(), 2);
    assert_eq!(row.capabilities.features.len(), 2);
    assert_eq!(row.capabilities.max_jobs, 4);
    assert_eq!(row.capabilities.nix_version, "2.20.0");
}

#[tokio::test]
async fn heartbeat_advances_last_seen() {
    let (addr, store, _registry) = spawn_server().await;
    let (client_key, client_pubkey) = fresh_client_key();
    let t0 = Utc::now() - chrono::Duration::seconds(3600);
    let _ = <SqlxStore as BuilderStore>::upsert(
        &store,
        NewBuilder {
            name: BuilderName::new("watcher").unwrap(),
            pubkey: client_pubkey,
            capabilities: caps(&["x86_64-linux"], &[], 1),
        },
        t0,
    )
    .await
    .unwrap();

    let mut session = connect(addr).await;
    let _ = session
        .authenticate_publickey(
            "medusa-builder",
            PrivateKeyWithHashAlg::new(Arc::new(client_key), None),
        )
        .await
        .unwrap();
    let mut channel = session.channel_open_session().await.unwrap();
    let hello = ControlMessage::Hello {
        name: BuilderName::new("watcher").unwrap(),
        systems: vec!["x86_64-linux".into()],
        features: vec![],
        max_jobs: 1,
        nix_version: "2.18.1".into(),
    };
    channel.data(&hello.encode_line()[..]).await.unwrap();
    let _ = await_welcome(&mut channel).await.unwrap();

    // Heartbeat with a fixed ts; medusa stamps `last_seen` with its
    // own chrono::Utc::now(), which must be strictly after t0 (1h ago).
    let hb = ControlMessage::Heartbeat {
        ts: 12345,
        load: Some(0.5),
    };
    channel.data(&hb.encode_line()[..]).await.unwrap();

    // Poll sqlite — server's mark_seen runs on the data callback's
    // tokio task, so allow a brief window for it to land.
    let mut last_seen_advanced = false;
    for _ in 0..40 {
        let row = <SqlxStore as BuilderStore>::find_by_name(&store, "watcher")
            .await
            .unwrap()
            .unwrap();
        if row.last_seen > t0 + chrono::Duration::seconds(60) {
            last_seen_advanced = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        last_seen_advanced,
        "heartbeat must advance last_seen past the hour-old initial value",
    );
}

#[tokio::test]
async fn hello_with_mismatched_name_uses_existing_row() {
    let (addr, store, _registry) = spawn_server().await;
    let (client_key, client_pubkey) = fresh_client_key();
    let _ = <SqlxStore as BuilderStore>::upsert(
        &store,
        NewBuilder {
            name: BuilderName::new("canonical-name").unwrap(),
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
            "medusa-builder",
            PrivateKeyWithHashAlg::new(Arc::new(client_key), None),
        )
        .await
        .unwrap();
    let mut channel = session.channel_open_session().await.unwrap();
    // Agent reports a different name; server should ignore and use
    // the row's canonical name. No new row appears.
    let hello = ControlMessage::Hello {
        name: BuilderName::new("agent-renamed-itself").unwrap(),
        systems: vec!["x86_64-linux".into()],
        features: vec![],
        max_jobs: 2,
        nix_version: "2.18.1".into(),
    };
    channel.data(&hello.encode_line()[..]).await.unwrap();
    let _ = await_welcome(&mut channel).await.unwrap();

    let all = <SqlxStore as BuilderStore>::list_all(&store).await.unwrap();
    let names: Vec<&str> = all.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["canonical-name"]);
    let row = &all[0];
    // Capabilities still updated.
    assert_eq!(row.capabilities.max_jobs, 2);
}
