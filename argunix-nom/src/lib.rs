//! `argunix-nom` — a parser for nix's `internal-json` build-log format.
//!
//! When argunix runs a build with `nix … --log-format internal-json`,
//! nix emits structured `@nix {…}` activity events on stderr instead
//! of plain text. [`NomParser`] turns that byte stream into
//! [`NomEvent`]s, which argunix uses two ways:
//!
//!   * [`render_storage_line`] renders them back to flat text — each
//!     build line prefixed with the derivation it came from — for the
//!     stored `.log.zst`.
//!   * the daemon streams them to the web UI for a colored,
//!     per-derivation live log and a `nix-output-monitor`-style view
//!     of what is currently building.
//!
//! The parser is deliberately defensive: `internal-json` is a
//! semi-internal nix format, so anything unrecognised — an unknown
//! action code, malformed JSON, a non-`@nix` line — degrades to
//! [`NomEvent::Raw`] rather than failing. It never errors and never
//! panics.

mod event;
mod parser;
mod render;

pub use event::{ActivityKind, NomEvent};
pub use parser::NomParser;
pub use render::render_storage_line;
