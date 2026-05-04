//! Spawn the medusa-pipe binary, point it at a temp Unix socket, and
//! verify bytes round-trip in both directions.

use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::process::Command;

fn medusa_pipe_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_medusa-pipe"))
}

#[tokio::test]
async fn round_trip_bidirectional_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("bobs-mini.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();

    // Spawn medusa-pipe with stdin/stdout piped.
    let mut child = Command::new(medusa_pipe_bin())
        .arg("bobs-mini")
        .arg("--socket-dir")
        .arg(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut child_stdin = child.stdin.take().unwrap();
    let mut child_stdout = child.stdout.take().unwrap();

    // Wait for medusa-pipe to connect.
    let (mut sock, _addr) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("medusa-pipe must connect to the socket")
        .unwrap();

    // 1. Send bytes via medusa-pipe's stdin → expect them on the socket.
    let payload_a = b"hello-from-stdin\n";
    tokio::io::AsyncWriteExt::write_all(&mut child_stdin, payload_a)
        .await
        .unwrap();
    AsyncWriteExt::flush(&mut child_stdin).await.unwrap();
    let mut got_on_socket = vec![0u8; payload_a.len()];
    tokio::time::timeout(Duration::from_secs(5), sock.read_exact(&mut got_on_socket))
        .await
        .expect("bytes sent into stdin must appear on the socket")
        .unwrap();
    assert_eq!(&got_on_socket, payload_a);

    // 2. Send bytes on the socket → expect them on medusa-pipe's stdout.
    let payload_b = b"echo-from-server\n";
    sock.write_all(payload_b).await.unwrap();
    sock.flush().await.unwrap();
    let mut got_on_stdout = vec![0u8; payload_b.len()];
    tokio::time::timeout(
        Duration::from_secs(5),
        child_stdout.read_exact(&mut got_on_stdout),
    )
    .await
    .expect("bytes sent on the socket must appear on stdout")
    .unwrap();
    assert_eq!(&got_on_stdout, payload_b);

    // Half-close the socket from the server side; medusa-pipe should
    // also half-close its stdin pipe and exit cleanly when stdin EOFs.
    drop(sock);
    drop(child_stdin);
    let exit = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("medusa-pipe should exit shortly after the socket closes")
        .unwrap();
    assert!(
        exit.success(),
        "medusa-pipe must exit 0 on clean teardown, got {exit:?}",
    );
}

#[tokio::test]
async fn fails_when_socket_does_not_exist() {
    let dir = tempfile::tempdir().unwrap();
    // No listener — the connect should fail and the process exits non-zero.
    let exit = Command::new(medusa_pipe_bin())
        .arg("absent")
        .arg("--socket-dir")
        .arg(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status()
        .await
        .unwrap();
    assert!(!exit.success(), "missing socket → non-zero exit");
}

// Silence unused-import lint when only one test references std::io::Write.
#[allow(dead_code)]
fn _force_use(_: &mut dyn Write) {}
