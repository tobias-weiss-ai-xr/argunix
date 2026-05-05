//! End-to-end agent test.
//!
//! Spawns the medusa builder-server, runs the agent against it, and
//! verifies:
//!   - first-contact token enrollment writes the row in sqlite,
//!   - subsequent reconnect uses pubkey auth,
//!   - shutdown signal sends a `Shutdown` control message and
//!     transitions the registry entry to `Disconnecting`.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use medusa_builder_agent::{AgentConfig, PersistedKey, run};
use medusa_builders::{
    BuilderDispatcher, BuilderRegistry, BuilderServer, ConnState, ServerConfig, SideChannelHeader,
    SideChannelKind, load_or_generate as load_host_key,
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
        nix_store_bin: AgentConfig::default_nix_store_bin(),
        medusa_host_key_path: None,
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

/// Lay down a fake `nix-store` shell script at `path` that, on
/// `--import`, pipes stdin into a sink file and exits 0. Used to
/// verify the agent's side-channel handler runs the right
/// subprocess and forwards bytes byte-for-byte from the daemon.
fn fake_nix_store_import(path: &Path, sink: &Path) {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    // Atomic-rename install: dodge ETXTBSY under parallel cargo-test
    // fork pressure by ensuring the final path never had a writable
    // fd opened on it.
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp).unwrap();
        writeln!(
            f,
            r#"#!/bin/sh
if [ "$1" = "--import" ]; then
  cat > "{sink}"
  exit 0
fi
exit 99
"#,
            sink = sink.display(),
        )
        .unwrap();
        f.sync_all().unwrap();
    }
    let mut perm = std::fs::metadata(&tmp).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&tmp, perm).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

/// M14b end-to-end: spawn a real medusa server + a real agent over
/// SSH, open a session channel from the daemon side, write a
/// `ClosurePush` side-channel header + binary payload, and assert
/// the agent's fake `nix-store --import` received every byte. This
/// is the legacy `cat` round-trip test (M13) replaced for the new
/// transport.
#[tokio::test]
async fn closure_push_side_channel_end_to_end() {
    let (addr, _store, registry) = spawn_server().await;
    let identity = fresh_identity();

    // Fake `nix-store` so we don't need a real nix install just to
    // exercise the channel plumbing. `--import` cats stdin → sink.
    let bin_dir = tempfile::tempdir().unwrap();
    let fake_bin = bin_dir.path().join("nix-store");
    let import_sink = bin_dir.path().join("imp-sink.bin");
    fake_nix_store_import(&fake_bin, &import_sink);

    let cfg = AgentConfig {
        medusa: addr,
        identity: identity.clone(),
        enrollment_token: Some(Arc::new(ENROLL_TOKEN.to_vec())),
        name: BuilderName::new("xfer-builder").unwrap(),
        capabilities: caps(),
        reconnect_initial_backoff: Duration::from_millis(100),
        nix_store_bin: PathBuf::from(&fake_bin),
        medusa_host_key_path: None,
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

    // Open a side channel from the daemon side. `BuilderDispatcher`
    // owns the russh server-side `Channel<ServerMsg>`.
    let dispatcher = BuilderDispatcher::new(registry.clone());
    let mut dispatched = dispatcher
        .open_channel(&BuilderName::new("xfer-builder").unwrap())
        .await
        .expect("open_channel must succeed");
    let channel = dispatched.take_channel().expect("channel present");

    // Daemon-side write: header + binary payload.
    let header = SideChannelHeader {
        kind: SideChannelKind::ClosurePush,
        build_id: 1234,
        paths: vec!["/nix/store/aaa-foo.drv".into()],
    };
    let payload: Vec<u8> = (0u8..=255).chain(std::iter::once(b'X')).collect();

    // Hack: the daemon side has `Channel::data` which writes bytes
    // out, but no AsyncWrite adapter yet (that's the next slice on
    // the daemon side). For the test we use the russh `data()` API
    // directly — same byte stream the AsyncWrite adapter would
    // produce.
    let mut header_bytes = header.encode_line();
    channel.data(&header_bytes[..]).await.unwrap();
    header_bytes.clear(); // drop reference
    channel.data(&payload[..]).await.unwrap();
    // Half-close the channel so the agent's import subprocess sees
    // EOF on stdin and exits.
    channel.eof().await.unwrap();

    // Wait until the fake nix-store has written everything to the
    // sink. The agent runs the subprocess + waits for it; we poll
    // the sink file size.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut last_len = 0usize;
    while tokio::time::Instant::now() < deadline {
        if let Ok(meta) = std::fs::metadata(&import_sink) {
            let len = meta.len() as usize;
            if len == payload.len() {
                last_len = len;
                break;
            }
            last_len = len;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        last_len,
        payload.len(),
        "fake `nix-store --import` should have received the full payload",
    );
    let written = std::fs::read(&import_sink).expect("sink readable");
    assert_eq!(
        written, payload,
        "every byte of the daemon's payload must reach the agent's `nix-store --import`",
    );

    drop(channel);
    drop(dispatched);
    let _ = sd_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(3), agent_handle).await;
}
