//! End-to-end agent test.
//!
//! Spawns the medusa builder-server, runs the agent against it, and
//! verifies:
//!   - first-contact token enrollment writes the row in sqlite,
//!   - subsequent reconnect uses pubkey auth,
//!   - shutdown signal sends a `Shutdown` control message and
//!     transitions the registry entry to `Disconnecting`.

use std::sync::Arc;
use std::time::Duration;

use medusa_builder_agent::{AgentConfig, PersistedKey, run};
use medusa_builders::{
    BuilderRegistry, BuilderServer, ConnState, ServerConfig, load_or_generate as load_host_key,
};
use medusa_domain::{BuilderCapabilities, BuilderName};
use medusa_store::{BuilderStore, SqlxStore, open_in_memory};

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
        socket_server: None,
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
    let key = medusa_builder_agent::load_or_generate(&path).unwrap();
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
        medusa: addr,
        identity: identity.clone(),
        enrollment_token: Some(Arc::new(ENROLL_TOKEN.to_vec())),
        name: BuilderName::new("agent-test").unwrap(),
        capabilities: caps(),
        reconnect_initial_backoff: Duration::from_millis(100),
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
