//! medusa-builder agent (M13b).
//!
//! The client-side counterpart to medusa-builders' SSH server: dials
//! medusa, authenticates (pubkey first, fall back to enrollment
//! token), opens a control channel, sends a `hello` carrying the
//! builder's self-discovered capabilities, then heartbeats. When
//! medusa initiates an inbound build channel, the agent spawns
//! `nix-store --serve --write` and pipes the channel's stdio into
//! the subprocess.
//!
//! This crate is split into a `medusa_builder_agent` library + a
//! `medusa-builder` binary. The library is what tests exercise.

mod agent;
mod capabilities;
mod identity;

pub use agent::{AgentConfig, AgentError, run};
pub use capabilities::{Capabilities, CapabilitiesError, discover_capabilities};
pub use identity::{IdentityError, PersistedKey, load_or_generate};
