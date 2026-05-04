//! Verifies that wiring `socket_server: Some(_)` into `ServerConfig`
//! makes the BuilderServer auto-create a per-builder Unix socket on
//! every Hello, and tear it down on disconnect.

use std::sync::Arc;
use std::time::Duration;

use medusa_builders::{
    BuilderDispatcher, BuilderRegistry, BuilderServer, ConnState, ControlMessage, ServerConfig,
    SocketServer, load_or_generate,
};
use medusa_domain::{BuilderName, BuilderPubkey};
use medusa_store::{SqlxStore, open_in_memory};
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

async fn spawn_server() -> (
    std::net::SocketAddr,
    Arc<SqlxStore>,
    Arc<BuilderRegistry>,
    std::path::PathBuf,
) {
    let pool = open_in_memory().await.unwrap();
    let store = Arc::new(SqlxStore::new(pool));
    let dir = tempfile::tempdir().unwrap();
    let host_key = load_or_generate(&dir.path().join("host_key")).unwrap();
    let registry = BuilderRegistry::new();
    let socket_dir = dir.path().join("builders");

    let dispatcher = Arc::new(BuilderDispatcher::new(registry.clone()));
    let socket_server = Arc::new(SocketServer::new(socket_dir.clone(), dispatcher));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let cfg = ServerConfig {
        listen: addr,
        host_key,
        enrollment_token: Arc::new(ENROLL_TOKEN.to_vec()),
        store: store.clone(),
        registry: registry.clone(),
        socket_server: Some(socket_server),
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
    (addr, store, registry, socket_dir)
}

fn fresh_client_key() -> (PrivateKey, BuilderPubkey) {
    let k = PrivateKey::random(&mut rand10::rng(), Algorithm::Ed25519).unwrap();
    let pk = BuilderPubkey::from_bytes(&k.public_key().key_data().ed25519().unwrap().0).unwrap();
    (k, pk)
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
            Ok(Some(ChannelMsg::Data { data })) => {
                buf.extend_from_slice(&data);
                if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line = std::str::from_utf8(&buf[..pos]).ok()?;
                    return serde_json::from_str(line).ok();
                }
            }
            _ => return None,
        }
    }
}

async fn enroll_and_hello(
    addr: std::net::SocketAddr,
    name: &str,
) -> client::Handle<AcceptAnyHostKey> {
    let (key, _) = fresh_client_key();
    let cfg = Arc::new(client::Config::default());
    let mut session = client::connect(cfg, addr, AcceptAnyHostKey).await.unwrap();
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
    let mut channel = session.channel_open_session().await.unwrap();
    let hello = ControlMessage::Hello {
        name: BuilderName::new(name).unwrap(),
        systems: vec!["x86_64-linux".into()],
        features: vec![],
        max_jobs: 1,
        nix_version: "2.18.1".into(),
    };
    channel.data(&hello.encode_line()[..]).await.unwrap();
    let _ = await_welcome(&mut channel).await.unwrap();
    std::mem::forget(channel);
    session
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

async fn wait_for_socket(socket_dir: &std::path::Path, name: &str, present: bool) -> bool {
    let path = socket_dir.join(format!("{name}.sock"));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if path.exists() == present {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

#[tokio::test]
async fn hello_creates_per_builder_socket_disconnect_removes_it() {
    let (addr, _store, registry, socket_dir) = spawn_server().await;
    let session = enroll_and_hello(addr, "auto-managed").await;
    wait_active(&registry, "auto-managed").await;

    assert!(
        wait_for_socket(&socket_dir, "auto-managed", true).await,
        "BuilderServer with socket_server=Some must auto-create the per-builder socket on hello",
    );

    // Disconnect the agent; the ConnectionHandler's Drop should
    // tear down the SocketGuard which removes the file synchronously.
    let _ = session
        .disconnect(russh::Disconnect::ByApplication, "test done", "en")
        .await;
    drop(session);

    assert!(
        wait_for_socket(&socket_dir, "auto-managed", false).await,
        "connection drop must remove the per-builder socket",
    );
}
