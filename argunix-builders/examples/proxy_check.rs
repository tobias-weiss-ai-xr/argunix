//! Manual probe: bind a Unix socket, forward each accept to
//! /nix/var/nix/daemon-socket/socket using the same tokio + half-close
//! pattern as our russh bridge. Run, then point `nix path-info --store
//! unix:///tmp/argunix-proxy-check.sock <path>` at it.

use tokio::io::AsyncWriteExt;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let proxy = "/tmp/argunix-proxy-check.sock";
    let target = "/nix/var/nix/daemon-socket/socket";
    let _ = std::fs::remove_file(proxy);
    let listener = tokio::net::UnixListener::bind(proxy)?;
    println!("listening at {proxy}, target {target}");
    loop {
        let (sock, _) = listener.accept().await?;
        tokio::spawn(async move {
            let upstream = match tokio::net::UnixStream::connect(target).await {
                Ok(u) => u,
                Err(e) => {
                    eprintln!("upstream connect: {e}");
                    return;
                }
            };
            let (sock_reader, sock_writer) = sock.into_split();
            let (up_reader, up_writer) = upstream.into_split();
            let to_up = async move {
                let mut sock_reader = sock_reader;
                let mut up_writer = up_writer;
                let _ = tokio::io::copy(&mut sock_reader, &mut up_writer).await;
                let _ = up_writer.shutdown().await;
            };
            let from_up = async move {
                let mut up_reader = up_reader;
                let mut sock_writer = sock_writer;
                let _ = tokio::io::copy(&mut up_reader, &mut sock_writer).await;
                let _ = sock_writer.shutdown().await;
            };
            tokio::join!(to_up, from_up);
        });
    }
}
