//! `medusa-pipe`: tiny `nix-store --builders` `ssh-command` shim.
//!
//! medusa-build invokes `nix-store --realise --builders 'ssh-ng://x@local?ssh-command=medusa-pipe <name>' ...`.
//! Nix forks `medusa-pipe <name>` and gives it stdin/stdout connected to its
//! own `nix-store --serve` worker. medusa-pipe connects to
//! `/run/medusa/builders/<name>.sock` and pipes those bytes through; the
//! medusa daemon on the other end of the socket opens a fresh SSH build
//! channel into the named builder and proxies the bytes onward to the
//! agent's `nix-store --serve --write`.
//!
//! The binary intentionally does nothing else — no logging beyond a fatal
//! error message, no protocol awareness. Bytes in, bytes out.

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Parser, Debug)]
#[command(
    name = "medusa-pipe",
    about = "Forward stdin/stdout to /run/medusa/builders/<name>.sock"
)]
struct Cli {
    /// Override the directory containing per-builder sockets. Defaults
    /// to /run/medusa/builders. Useful for tests and for non-NixOS hosts
    /// where the daemon's RuntimeDirectory is elsewhere. Must come
    /// before the positional `name` since trailing positional args are
    /// captured into `_ignored` (see below).
    #[arg(long, default_value = "/run/medusa/builders")]
    socket_dir: PathBuf,

    /// Builder name. Must match a registered (Active) builder on the
    /// medusa daemon side; otherwise the daemon refuses the connection
    /// and this process exits non-zero.
    name: String,

    /// nix's `ssh-ng://` store (which is what dispatches to us)
    /// unconditionally appends `--stdio` to the remote-program argv,
    /// and may append `--store <uri>` and other flags in the future.
    /// We accept-and-discard everything past `name` so future nix
    /// versions don't break dispatch.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
    _ignored: Vec<String>,
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        anyhow::bail!("builder name must not be empty");
    }
    if name.len() > 64 {
        anyhow::bail!("builder name too long");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        anyhow::bail!(
            "builder name contains characters that wouldn't be valid in /run/medusa/builders/<name>.sock"
        );
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    validate_name(&cli.name).with_context(|| format!("invalid builder name `{}`", cli.name))?;

    let path = cli.socket_dir.join(format!("{}.sock", cli.name));
    let socket = tokio::net::UnixStream::connect(&path)
        .await
        .with_context(|| format!("connecting to {}", path.display()))?;

    let (mut sock_read, mut sock_write) = socket.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    // Two unbuffered byte pumps. Either side closing ends the run; we
    // surface the first error.
    let stdin_to_sock = async move {
        let mut buf = [0u8; 8 * 1024];
        loop {
            let n = stdin.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            sock_write.write_all(&buf[..n]).await?;
            sock_write.flush().await?;
        }
        // Half-close so the server-side knows we're done sending.
        let _ = sock_write.shutdown().await;
        Ok::<(), std::io::Error>(())
    };
    let sock_to_stdout = async move {
        let mut buf = [0u8; 8 * 1024];
        loop {
            let n = sock_read.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            stdout.write_all(&buf[..n]).await?;
            stdout.flush().await?;
        }
        Ok::<(), std::io::Error>(())
    };

    tokio::select! {
        r = stdin_to_sock => r.context("piping stdin to socket")?,
        r = sock_to_stdout => r.context("piping socket to stdout")?,
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_name_accepts_hostnames() {
        validate_name("bobs-mini").unwrap();
        validate_name("alices_thinkpad.local").unwrap();
    }

    #[test]
    fn validate_name_rejects_empty() {
        assert!(validate_name("").is_err());
    }

    #[test]
    fn validate_name_rejects_slash() {
        assert!(validate_name("a/b").is_err());
    }

    #[test]
    fn validate_name_rejects_too_long() {
        let s: String = std::iter::repeat('a').take(65).collect();
        assert!(validate_name(&s).is_err());
    }
}
