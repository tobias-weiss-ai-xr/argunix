//! Dynamic builder pool for medusa (M13 / `design/builders.md`).
//!
//! Builders dial medusa over SSH (russh server) and authenticate either
//! with a shared enrollment token (first connect) or a per-builder
//! public key (TOFU after first connect; persisted in `builders` sqlite).

mod auth;
mod channel_io;
mod closure_xfer;
mod dispatcher;
mod host_key;
mod protocol;
mod registry;
mod server;
mod side_channel;
mod systems;

pub use auth::AuthState;
pub use channel_io::{ChannelSide, with_channel_io};
pub use closure_xfer::{ClosureXferError, NixCopyDirection, nix_copy_over_pool};
pub use dispatcher::{BuilderDispatcher, DispatchError, DispatchedBuild};
pub use host_key::{HostKey, HostKeyError, load_or_generate};
pub use protocol::{BuildOutcomeStatus, ControlMessage, LineFramer, ProtocolError};
pub use registry::{
    BuildLifecycle, BuilderRegistry, BuilderSnapshot, ConnState, ConnectedBuilder,
    DisplacedConnection, RusshSession,
};
pub use server::{BuilderServer, ServerConfig, ServerError};
pub use side_channel::{
    DispatchError as SideChannelDispatchError, DispatchOutcome as SideChannelDispatchOutcome,
    MAX_HEADER_BYTES, SideChannelError, SideChannelHeader, SideChannelKind, dispatch_inbound,
    read_header, write_header,
};
pub use systems::SystemsResolver;
