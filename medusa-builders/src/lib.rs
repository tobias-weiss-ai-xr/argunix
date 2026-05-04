//! Dynamic builder pool for medusa (M13 / `design/builders.md`).
//!
//! Builders dial medusa over SSH (russh server) and authenticate either
//! with a shared enrollment token (first connect) or a per-builder
//! public key (TOFU after first connect; persisted in `builders` sqlite).
//!
//! This crate currently exposes only the auth layer — channel and
//! capability protocols land in subsequent PRs.

mod auth;
mod host_key;
mod server;

pub use auth::AuthState;
pub use host_key::{HostKey, HostKeyError, load_or_generate};
pub use server::{BuilderServer, ServerConfig, ServerError};
