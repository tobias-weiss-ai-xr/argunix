//! Build-channel dispatcher: end-to-end test that exercises both ends.
//!
//! Stands up the medusa SSH server (russh server), authenticates a stub
//! agent (russh client) that captures inbound `server_channel_open_session`
//! callbacks, then drives the dispatcher to open a fresh build channel
//! into the stub agent and round-trips a few hundred bytes.

use std::sync::Arc;
use std::time::Duration;

use medusa_builders::{
    BuilderDispatcher, BuilderRegistry, BuilderServer, ConnState, ControlMessage, ServerConfig,
    load_or_generate,
};
use medusa_domain::{BuilderName, BuilderPubkey};
use medusa_store::{SqlxStore, open_in_memory};
use russh::ChannelMsg;
use russh::client::{self, Handler, Msg as ClientMsg, Session as ClientSession};
use russh::keys::ssh_key::PrivateKey;
use russh::keys::{Algorithm, PrivateKeyWithHashAlg};

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
        // Hand the channel off to the test for echo handling. Errors
        // ignored: receiver may have dropped if the test already
        // tore down.
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

fn fresh_client_key() -> (PrivateKey, BuilderPubkey) {
    let k = PrivateKey::random(&mut rand10::rng(), Algorithm::Ed25519).unwrap();
    let pk = BuilderPubkey::from_bytes(&k.public_key().key_data().ed25519().unwrap().0).unwrap();
    (k, pk)
}

/// Connect a stub agent, authenticate via token, send hello, await
/// welcome. Returns the client handle plus a receiver that yields any
/// inbound channel-opens (each one a build dispatch from medusa).
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
    assert!(r.success(), "token auth must succeed");

    let mut ch = session.channel_open_session().await.unwrap();
    let hello = ControlMessage::Hello {
        name: BuilderName::new(name).unwrap(),
        systems: vec!["x86_64-linux".into()],
        features: vec![],
        max_jobs: 4,
        nix_version: "2.18.1".into(),
    };
    ch.data(&hello.encode_line()[..]).await.unwrap();

    // Drain control until welcome arrives. We don't need to keep the
    // control channel — the test only cares about build channels, but
    // we can't drop the session handle.
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

    // Keep the control channel alive for the duration of the test
    // by leaking the handle; russh tears it down when dropped.
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

#[tokio::test]
async fn dispatch_opens_channel_to_eligible_builder() {
    let (addr, _store, registry) = spawn_server().await;
    let (_session, mut on_open) = enroll_stub_agent(addr, "echo-builder").await;
    wait_active(&registry, "echo-builder").await;

    let dispatcher = BuilderDispatcher::new(registry.clone());
    let mut dispatched = dispatcher
        .dispatch("x86_64-linux", &[], &Default::default())
        .await
        .expect("dispatch must succeed");

    // The agent should have observed the inbound channel open.
    let mut agent_channel = tokio::time::timeout(Duration::from_secs(2), on_open.recv())
        .await
        .expect("inbound channel-open arrives at the agent")
        .expect("on_open channel is open");

    // Set up an echo on the agent side.
    let echo_task = tokio::spawn(async move {
        let mut total: Vec<u8> = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), agent_channel.wait()).await {
                Ok(Some(ChannelMsg::Data { data })) => {
                    total.extend_from_slice(&data);
                    let _ = agent_channel.data(&data[..]).await;
                }
                Ok(Some(ChannelMsg::Close)) | Ok(None) => break,
                _ => continue,
            }
        }
        total
    });

    // Server-side: send some bytes; the agent should echo them back.
    let mut medusa_channel = dispatched.take_channel().expect("channel present");
    let payload = b"hello-from-medusa\n";
    medusa_channel.data(&payload[..]).await.unwrap();

    // Read the echo back (best-effort with a deadline).
    let mut got = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while got.len() < payload.len() && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), medusa_channel.wait()).await {
            Ok(Some(ChannelMsg::Data { data })) => got.extend_from_slice(&data),
            Ok(Some(ChannelMsg::Close)) | Ok(None) => break,
            _ => continue,
        }
    }
    assert_eq!(&got, payload, "echoed bytes should round-trip");

    // Verify in_flight ticked up.
    let snap = registry
        .snapshot(&BuilderName::new("echo-builder").unwrap())
        .unwrap();
    assert_eq!(snap.in_flight, 1, "in_flight reflects the dispatched build");

    drop(medusa_channel);
    drop(dispatched); // dec_in_flight on drop
    let _ = echo_task.await;

    // After drop, in_flight returns to 0.
    let snap = registry
        .snapshot(&BuilderName::new("echo-builder").unwrap())
        .unwrap();
    assert_eq!(snap.in_flight, 0, "drop releases capacity");
}

#[tokio::test]
async fn dispatch_returns_no_eligible_builder_when_registry_empty() {
    let (_addr, _store, registry) = spawn_server().await;
    let dispatcher = BuilderDispatcher::new(registry.clone());
    let result = dispatcher
        .dispatch("x86_64-linux", &[], &Default::default())
        .await;
    let Err(err) = result else {
        panic!("dispatch must fail when no builders are registered");
    };
    assert!(
        matches!(
            err,
            medusa_builders::DispatchError::NoEligibleBuilder { .. }
        ),
        "no builders means NoEligibleBuilder, not AllOpensFailed",
    );
}
