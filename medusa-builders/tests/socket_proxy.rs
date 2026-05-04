//! End-to-end test for the per-builder Unix socket proxy.
//!
//! Pipeline under test:
//! `UnixStream client` → `SocketServer accept` → `BuilderDispatcher::open_channel` →
//! `russh server::Handle::channel_open_session` → stub-agent's
//! `server_channel_open_session` → echo loop.
//!
//! Verifies bytes round-trip both directions and that closing the
//! Unix client cleanly tears down the SSH channel.

use std::sync::Arc;
use std::time::Duration;

use medusa_builders::{
    BuilderDispatcher, BuilderRegistry, BuilderServer, ConnState, ControlMessage, ServerConfig,
    SocketServer, load_or_generate,
};
use medusa_domain::{BuilderName, BuilderPubkey};
use medusa_store::{SqlxStore, open_in_memory};
use russh::ChannelMsg;
use russh::client::{self, Handler, Msg as ClientMsg, Session as ClientSession};
use russh::keys::ssh_key::PrivateKey;
use russh::keys::{Algorithm, PrivateKeyWithHashAlg};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

const ENROLL_TOKEN: &[u8] = b"test-enrollment-token";

#[derive(Clone)]
struct StubAgent {
    on_inbound_channel: tokio::sync::mpsc::UnboundedSender<russh::Channel<ClientMsg>>,
}

impl Handler for StubAgent {
    type Error = russh::Error;
    async fn check_server_key(
        &mut self,
        _: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
    async fn server_channel_open_session(
        &mut self,
        channel: russh::Channel<ClientMsg>,
        _: &mut ClientSession,
    ) -> Result<(), Self::Error> {
        let _ = self.on_inbound_channel.send(channel);
        Ok(())
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

fn fresh_client_key() -> (PrivateKey, BuilderPubkey) {
    let k = PrivateKey::random(&mut rand10::rng(), Algorithm::Ed25519).unwrap();
    let pk = BuilderPubkey::from_bytes(&k.public_key().key_data().ed25519().unwrap().0).unwrap();
    (k, pk)
}

async fn enroll_stub_agent(
    addr: std::net::SocketAddr,
    name: &str,
) -> (
    client::Handle<StubAgent>,
    tokio::sync::mpsc::UnboundedReceiver<russh::Channel<ClientMsg>>,
) {
    let (key, _) = fresh_client_key();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let cfg = Arc::new(client::Config::default());
    let mut session = client::connect(
        cfg,
        addr,
        StubAgent {
            on_inbound_channel: tx,
        },
    )
    .await
    .unwrap();
    let _ = session
        .authenticate_publickey(
            "medusa-builder",
            PrivateKeyWithHashAlg::new(Arc::new(key), None),
        )
        .await
        .unwrap();
    let r = session
        .authenticate_password("medusa-builder", std::str::from_utf8(ENROLL_TOKEN).unwrap())
        .await
        .unwrap();
    assert!(r.success());

    let mut ch = session.channel_open_session().await.unwrap();
    let hello = ControlMessage::Hello {
        name: BuilderName::new(name).unwrap(),
        systems: vec!["x86_64-linux".into()],
        features: vec![],
        max_jobs: 4,
        nix_version: "2.18.1".into(),
    };
    ch.data(&hello.encode_line()[..]).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut buf = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("welcome never arrived");
        }
        match tokio::time::timeout(remaining, ch.wait()).await {
            Ok(Some(ChannelMsg::Data { data })) => {
                buf.extend_from_slice(&data);
                if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line = std::str::from_utf8(&buf[..pos]).unwrap();
                    let m: ControlMessage = serde_json::from_str(line).unwrap();
                    assert!(matches!(m, ControlMessage::Welcome { .. }));
                    break;
                }
            }
            _ => continue,
        }
    }
    std::mem::forget(ch);
    (session, rx)
}

async fn wait_active(reg: &BuilderRegistry, name: &str) {
    let bn = BuilderName::new(name).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if let Some(s) = reg.snapshot(&bn) {
            if s.state == ConnState::Active {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("builder {name} never became Active");
}

/// Spawn a task that echoes every Data message it receives back on the
/// same channel. Returns a JoinHandle for the test to await on.
fn spawn_echo_for(
    mut on_inbound: tokio::sync::mpsc::UnboundedReceiver<russh::Channel<ClientMsg>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(mut channel) = on_inbound.recv().await {
            tokio::spawn(async move {
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                while tokio::time::Instant::now() < deadline {
                    match tokio::time::timeout(Duration::from_millis(200), channel.wait()).await {
                        Ok(Some(ChannelMsg::Data { data })) => {
                            let _ = channel.data(&data[..]).await;
                        }
                        Ok(Some(ChannelMsg::Eof)) | Ok(Some(ChannelMsg::Close)) | Ok(None) => break,
                        _ => continue,
                    }
                }
                let _ = channel.close().await;
            });
        }
    })
}

#[tokio::test]
async fn unix_socket_proxies_bytes_to_builder_channel() {
    let (addr, _store, registry) = spawn_server().await;
    let (_session, on_open) = enroll_stub_agent(addr, "echo-builder").await;
    wait_active(&registry, "echo-builder").await;
    let _echo = spawn_echo_for(on_open);

    let dispatcher = Arc::new(BuilderDispatcher::new(registry.clone()));
    let dir = tempfile::tempdir().unwrap();
    let socket_server = SocketServer::new(dir.path().to_path_buf(), dispatcher);
    let guard = socket_server
        .listen_for(BuilderName::new("echo-builder").unwrap())
        .await
        .expect("listen_for must succeed");
    let socket_path = guard.path().to_path_buf();
    assert!(
        socket_path.exists(),
        "socket file must exist after listen_for"
    );

    // Connect to the per-builder socket as a Unix client.
    let mut client = UnixStream::connect(&socket_path).await.unwrap();

    // Round-trip 1: client → server → channel → echo → channel → server → client.
    let payload_a = b"alpha-bytes-on-the-wire";
    client.write_all(payload_a).await.unwrap();
    client.flush().await.unwrap();
    let mut got = vec![0u8; payload_a.len()];
    tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut got))
        .await
        .expect("echoed bytes must come back through the proxy")
        .unwrap();
    assert_eq!(&got, payload_a);

    // Round-trip 2: a chunkier payload to exercise multi-read paths.
    let mut big = vec![0u8; 64 * 1024];
    for (i, b) in big.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    client.write_all(&big).await.unwrap();
    client.flush().await.unwrap();
    let mut got_big = vec![0u8; big.len()];
    tokio::time::timeout(Duration::from_secs(10), client.read_exact(&mut got_big))
        .await
        .expect("64K echo round-trip")
        .unwrap();
    assert_eq!(got_big, big);

    // While at least one client is connected, in_flight is 1.
    let snap = registry
        .snapshot(&BuilderName::new("echo-builder").unwrap())
        .unwrap();
    assert_eq!(snap.in_flight, 1, "active proxy reflects as 1 in_flight");

    drop(client);
    // Allow a moment for the proxy task to observe the EOF + drop the
    // DispatchedBuild guard.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let snap = registry
        .snapshot(&BuilderName::new("echo-builder").unwrap())
        .unwrap();
    assert_eq!(snap.in_flight, 0, "client drop releases the slot");

    guard.close().await;
    assert!(
        !socket_path.exists(),
        "SocketGuard::close must remove the socket file",
    );
}

#[tokio::test]
async fn listen_for_unknown_builder_still_binds_but_open_channel_errors() {
    // The socket lifecycle is decoupled from the registry: SocketServer
    // creates the listener regardless. Connecting to a socket whose
    // builder isn't registered (e.g. it dropped between listener bind
    // and a client connecting) yields a NotRegistered error from the
    // dispatcher; the proxy task surfaces this by closing the Unix
    // client. The Unix client just sees an immediate EOF with no bytes.
    let (_addr, _store, registry) = spawn_server().await;
    let dispatcher = Arc::new(BuilderDispatcher::new(registry));
    let dir = tempfile::tempdir().unwrap();
    let socket_server = SocketServer::new(dir.path().to_path_buf(), dispatcher);
    let guard = socket_server
        .listen_for(BuilderName::new("nobody").unwrap())
        .await
        .unwrap();
    let mut client = UnixStream::connect(guard.path()).await.unwrap();
    let mut buf = [0u8; 16];
    // Send a request; we expect either an immediate EOF or no data
    // followed by close — proxy refused to forward because the
    // builder isn't there.
    let _ = client.write_all(b"hello").await;
    let n = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf))
        .await
        .expect("connection must close promptly when builder isn't registered")
        .unwrap_or(0);
    assert_eq!(n, 0, "no bytes are echoed when the builder isn't there");
    guard.close().await;
}
