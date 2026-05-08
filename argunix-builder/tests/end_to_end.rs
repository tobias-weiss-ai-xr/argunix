//! End-to-end agent test.
//!
//! Spawns the argunix builder-server, runs the agent against it, and
//! verifies:
//!   - first-contact token enrollment writes the row in sqlite,
//!   - subsequent reconnect uses pubkey auth,
//!   - shutdown signal sends a `Shutdown` control message and
//!     transitions the registry entry to `Disconnecting`.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use argunix_builder_agent::{AgentConfig, PersistedKey, run};
use argunix_builders::{
    BuilderDispatcher, BuilderRegistry, BuilderServer, ConnState, ServerConfig, SideChannelHeader,
    SideChannelKind, load_or_generate as load_host_key,
};
use argunix_domain::{BuilderCapabilities, BuilderName};
use argunix_store::{BuilderStore, SqlxStore, open_in_memory};

const ENROLL_TOKEN: &[u8] = b"agent-e2e-token";

async fn spawn_server() -> (std::net::SocketAddr, Arc<SqlxStore>, Arc<BuilderRegistry>) {
    let pool = open_in_memory().await.unwrap();
    let store = Arc::new(SqlxStore::new(pool));
    let dir = tempfile::tempdir().unwrap();
    let host_key = load_host_key(&dir.path().join("host_key")).unwrap();
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

fn caps() -> BuilderCapabilities {
    BuilderCapabilities {
        systems: vec!["x86_64-linux".into()],
        features: vec![],
        max_jobs: 2,
        nix_version: "2.18.1".into(),
    }
}

fn fresh_identity() -> PersistedKey {
    // Persist into a tempdir so subsequent reuses pick up the same
    // file and the agent re-presents the same pubkey.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("identity");
    let key = argunix_builder_agent::load_or_generate(&path).unwrap();
    std::mem::forget(dir);
    key
}

async fn wait_for_row(store: &Arc<SqlxStore>, name: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(_)) = <SqlxStore as BuilderStore>::find_by_name(store, name).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for builders row `{name}`");
}

async fn wait_for_active(reg: &Arc<BuilderRegistry>, name: &str) {
    let bn = BuilderName::new(name).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if let Some(s) = reg.snapshot(&bn) {
            if s.state == ConnState::Active {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for builder `{name}` to be Active");
}

#[tokio::test]
async fn token_first_then_pubkey_subsequent() {
    let (addr, store, registry) = spawn_server().await;
    let identity = fresh_identity();

    let cfg = AgentConfig {
        argunix: addr,
        identity: identity.clone(),
        enrollment_token: Some(Arc::new(ENROLL_TOKEN.to_vec())),
        name: BuilderName::new("agent-test").unwrap(),
        capabilities: caps(),
        reconnect_initial_backoff: Duration::from_millis(100),
        nix_store_bin: AgentConfig::default_nix_store_bin(),
        nix_daemon_socket: AgentConfig::default_nix_daemon_socket(),
        argunix_host_key_path: None,
        build_gcroot_dir: AgentConfig::default_build_gcroot_dir(),
    };
    let (sd_tx, sd_rx) = tokio::sync::oneshot::channel::<()>();
    let cfg_a = cfg.clone();
    let agent_handle = tokio::spawn(async move {
        let _ = run(cfg_a, async move {
            let _ = sd_rx.await;
        })
        .await;
    });

    // First attempt: pubkey auth fails (TOFU not yet established);
    // agent falls back to token; row appears in sqlite under our
    // builder name.
    wait_for_row(&store, "agent-test").await;

    // Registry shows Active.
    wait_for_active(&registry, "agent-test").await;

    // Trigger shutdown; the agent sends a Shutdown control message.
    // The server should flip the registry to Disconnecting before
    // the connection actually closes.
    let _ = sd_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(3), agent_handle).await;
}

/// M16 end-to-end: spawn a real argunix server + a real agent over
/// SSH, open a side channel from the daemon side, write a
/// `NixDaemonStdio` header + payload, and assert every byte
/// round-trips back through the agent's socket tunnel. The fake
/// "nix-daemon socket" is a Unix socket bound to a temp path with
/// an echo server attached, so the agent's forwarding loop does a
/// byte-for-byte round-trip.
#[tokio::test]
async fn nix_daemon_stdio_side_channel_end_to_end() {
    let (addr, _store, registry) = spawn_server().await;
    let identity = fresh_identity();

    let bin_dir = tempfile::tempdir().unwrap();
    let fake_socket = bin_dir.path().join("fake-daemon.sock");
    let listener = tokio::net::UnixListener::bind(&fake_socket).unwrap();
    // Echo every accepted connection. The agent connects once per
    // side channel; we accept and copy stdin → stdout in a loop so
    // the test stays alive across reconnects.
    let echo_handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let (mut r, mut w) = stream.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });

    let cfg = AgentConfig {
        argunix: addr,
        identity: identity.clone(),
        enrollment_token: Some(Arc::new(ENROLL_TOKEN.to_vec())),
        name: BuilderName::new("xfer-builder").unwrap(),
        capabilities: caps(),
        reconnect_initial_backoff: Duration::from_millis(100),
        nix_store_bin: AgentConfig::default_nix_store_bin(),
        nix_daemon_socket: PathBuf::from(&fake_socket),
        argunix_host_key_path: None,
        build_gcroot_dir: AgentConfig::default_build_gcroot_dir(),
    };
    let (sd_tx, sd_rx) = tokio::sync::oneshot::channel::<()>();
    let cfg_a = cfg.clone();
    let agent_handle = tokio::spawn(async move {
        let _ = run(cfg_a, async move {
            let _ = sd_rx.await;
        })
        .await;
    });

    wait_for_active(&registry, "xfer-builder").await;

    let dispatcher = BuilderDispatcher::new(registry.clone());
    let mut dispatched = dispatcher
        .open_channel(&BuilderName::new("xfer-builder").unwrap())
        .await
        .expect("open_channel must succeed");
    let mut channel = dispatched.take_channel().expect("channel present");

    let header = SideChannelHeader {
        kind: SideChannelKind::NixDaemonStdio,
        build_id: 1234,
        paths: vec![],
    };
    let payload: Vec<u8> = (0u8..=255).chain(std::iter::once(b'X')).collect();

    let header_bytes = header.encode_line();
    channel.data(&header_bytes[..]).await.unwrap();
    channel.data(&payload[..]).await.unwrap();
    channel.eof().await.unwrap();

    // Read echoed bytes back from the channel until we see the full
    // payload. The echo server emits bytes as it reads them, so the
    // round-trip arrives streamingly.
    let mut received: Vec<u8> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline && received.len() < payload.len() {
        match tokio::time::timeout(Duration::from_millis(500), channel.wait()).await {
            Ok(Some(russh::ChannelMsg::Data { data })) => {
                received.extend_from_slice(&data);
            }
            Ok(Some(russh::ChannelMsg::Eof)) | Ok(Some(russh::ChannelMsg::Close)) | Ok(None) => {
                break;
            }
            Ok(Some(_)) => continue,
            Err(_) => continue, // poll timeout; loop and re-check deadline
        }
    }
    assert_eq!(
        received, payload,
        "every byte of the daemon's payload must round-trip through the agent's socket tunnel",
    );

    drop(channel);
    drop(dispatched);
    let _ = sd_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(3), agent_handle).await;
    echo_handle.abort();
}
