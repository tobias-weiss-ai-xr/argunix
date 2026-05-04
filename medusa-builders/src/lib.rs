//! Dynamic builder pool for medusa (M13 / `design/builders.md`).
//!
//! Builders dial medusa over SSH (russh server) and authenticate either
//! with a shared enrollment token (first connect) or a per-builder
//! public key (TOFU after first connect; persisted in `builders` sqlite).
//!
//! This crate currently exposes only the auth layer — channel and
//! capability protocols land in subsequent PRs.

mod auth;
mod dispatcher;
mod host_key;
mod protocol;
mod registry;
mod server;
mod socket_server;
mod systems;

pub use auth::AuthState;
pub use dispatcher::{BuilderDispatcher, DispatchError, DispatchedBuild};
pub use host_key::{HostKey, HostKeyError, load_or_generate};
pub use protocol::{ControlMessage, LineFramer, ProtocolError};
pub use registry::{
    BuilderRegistry, BuilderSnapshot, ConnState, ConnectedBuilder, DisplacedConnection,
    RusshSession,
};
pub use server::{BuilderServer, ServerConfig, ServerError};
pub use socket_server::{SocketError, SocketGuard, SocketServer};
pub use systems::SystemsResolver;
