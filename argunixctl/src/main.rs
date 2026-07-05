//! Thin client for the argunix daemon's unix-socket control protocol.
//!
//! Wire format and request/response types live in `argunix-control`;
//! this is just argument parsing + a one-shot socket round-trip.

use argunix_control::{BuilderInfo, Request, Response};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser, Debug)]
#[command(version, about = "argunix CI control utility")]
struct Cli {
    /// Path to the daemon's unix-domain control socket.
    #[arg(long, value_name = "PATH", default_value = "/run/argunix/control.sock")]
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
    /// Manage the dynamic builder pool.
    #[command(subcommand)]
    Builders(BuildersCommand),
    /// Test-only: dispatch a local drv path to the named builder over
    /// the side-channel transport. Used by the NixOS test that
    /// exercises the dynamic-pool path without standing up a fake
    /// forge. Not part of the operator surface.
    #[command(name = "test-dispatch-drv", hide = true)]
    TestDispatchDrv {
        #[arg(long, value_name = "BUILDER")]
        builder: String,
        /// Path to the .drv to realise on the builder.
        drv: String,
    },
}

#[derive(Subcommand, Debug)]
enum BuildersCommand {
    /// List every known builder (registered + revoked) with current
    /// connection status, capabilities, and in-flight build count.
    List {
        /// Print the raw JSON response instead of the formatted table.
        #[arg(long)]
        json: bool,
    },
    /// Revoke a builder. Sets `revoked_at` in sqlite; if currently
    /// connected, sends a kick + disconnects the SSH session. The name
    /// stays bound to its key — a token-authenticated reconnect can no
    /// longer un-revoke it. Use `remove` to free the name for a new key.
    Revoke { name: String },
    /// Remove a builder entirely. Deletes the sqlite row (and kicks it if
    /// connected), freeing the name so a *different* key can enroll under
    /// it. This is the escape hatch for decommissioning a builder or
    /// replacing its key.
    Remove { name: String },
    /// Rename `old` to `new`. Fails if `old` doesn't exist or `new`
    /// already does.
    Rename { old: String, new: String },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    // The `builders list` subcommand renders a table by default; every
    // other subcommand uses the generic ok/error printer. Branch
    // before the request dispatch so the table path can format its
    // typed response without going through serde_json::Value twice.
    let json_for_list = match &cli.command {
        Command::Builders(BuildersCommand::List { json }) => Some(*json),
        _ => None,
    };

    let req = match cli.command {
        Command::Reload { config } => Request::Reload {
            config_path: config,
        },
        Command::Status => Request::Status,
        Command::Builders(BuildersCommand::List { .. }) => Request::BuildersList,
        Command::Builders(BuildersCommand::Revoke { name }) => Request::BuildersRevoke { name },
        Command::Builders(BuildersCommand::Remove { name }) => Request::BuildersRemove { name },
        Command::Builders(BuildersCommand::Rename { old, new }) => {
            Request::BuildersRename { old, new }
        }
        Command::TestDispatchDrv { builder, drv } => Request::TestDispatchDrv {
            drv_path: drv,
            builder,
        },
    };

    match argunix_control::send(&cli.socket, &req).await {
        Ok(Response::Ok { details }) => match (json_for_list, details) {
            (Some(false), Some(d)) => match serde_json::from_value::<Vec<BuilderInfo>>(d.clone()) {
                Ok(builders) => {
                    print_builders_table(&builders);
                    ExitCode::SUCCESS
                }
                Err(_) => {
                    print_pretty(&d);
                    ExitCode::SUCCESS
                }
            },
            (_, Some(d)) => {
                print_pretty(&d);
                ExitCode::SUCCESS
            }
            (_, None) => {
                println!("ok");
                ExitCode::SUCCESS
            }
        },
        Ok(Response::Error { message }) => {
            eprintln!("argunixctl: error: {message}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("argunixctl: {e}");
            ExitCode::from(1)
        }
    }
}

fn print_pretty(value: &serde_json::Value) {
    match serde_json::to_string_pretty(value) {
        Ok(s) => println!("{s}"),
        Err(_) => println!("{value}"),
    }
}

fn print_builders_table(builders: &[BuilderInfo]) {
    if builders.is_empty() {
        println!("(no builders enrolled)");
        return;
    }
    // Compute column widths so revoked rows with short names don't
    // collapse into the systems column.
    let name_w = builders
        .iter()
        .map(|b| b.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    println!(
        "{:<name_w$}  {:>6}  {:<10}  {:>5}  systems",
        "NAME",
        "STATUS",
        "LAST-SEEN",
        "JOBS",
        name_w = name_w,
    );
    for b in builders {
        let status = if b.revoked_at.is_some() {
            "revoked"
        } else if b.connected {
            "active"
        } else {
            "offline"
        };
        let last_seen = b.last_seen.split('T').next().unwrap_or("?");
        println!(
            "{:<name_w$}  {:>6}  {:<10}  {:>5}  {}",
            b.name,
            status,
            last_seen,
            b.in_flight,
            b.systems.join(","),
            name_w = name_w,
        );
    }
}
