//! Thin client for the medusa daemon's unix-socket control protocol.
//!
//! Wire format and request/response types live in `medusa-control`;
//! this is just argument parsing + a one-shot socket round-trip.

use clap::{Parser, Subcommand};
use medusa_control::{Request, Response};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser, Debug)]
#[command(version, about = "medusa CI control utility")]
struct Cli {
    /// Path to the daemon's unix-domain control socket.
    #[arg(long, value_name = "PATH", default_value = "/run/medusa/control.sock")]
    socket: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Trigger an atomic config reload (used by systemd `ExecReload=`).
    Reload {
        /// Path to the YAML config to reload from. When omitted the
        /// daemon re-reads from the path it was started with.
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
    },
    /// Print the daemon's current state (uptime, repos, paused forges).
    Status,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let req = match cli.command {
        Command::Reload { config } => Request::Reload {
            config_path: config,
        },
        Command::Status => Request::Status,
    };
    match medusa_control::send(&cli.socket, &req).await {
        Ok(Response::Ok { details }) => {
            if let Some(d) = details {
                match serde_json::to_string_pretty(&d) {
                    Ok(s) => println!("{s}"),
                    Err(_) => println!("{d}"),
                }
            } else {
                println!("ok");
            }
            ExitCode::SUCCESS
        }
        Ok(Response::Error { message }) => {
            eprintln!("medusactl: error: {message}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("medusactl: {e}");
            ExitCode::from(1)
        }
    }
}
