//! Per-job build dispatcher.
//!
//! The unit of work is a single derivation, identified by its `.drv` path.
//! Each one runs `nix-store --realise` (cache-skip for already-cached
//! outputs is handled upstream by `nix-eval-jobs --check-cache-status`
//! via system-wide substituters, not in argunix's own config), captures
//! the build log to a zstd-compressed file, and on success registers a
//! GC root under `/nix/var/nix/gcroots/argunix/<repo>/<eval>/<job>` —
//! see [docs/concepts/gc-roots.md] — and pushes the closure to every
//! configured `binary_caches` entry.
//!
//! Scope:
//! - synchronous build per job (the daemon's offline `argunix build`
//!   subcommand iterates jobs sequentially);
//! - log capture: stream nix-store stderr to memory (capped at the
//!   configured size), write a single zstd-compressed file at the end;
//! - GC root: post-success `nix-store --add-root <root> --indirect <output>`;
//! - post-success cache publish via `nix copy --to <store-uri>`.

mod gc_root;
mod log_capture;
mod push;
mod runner;

pub use gc_root::{add_gc_root, gc_root_path};
pub use log_capture::{LogCaptureLimit, write_zstd_log};
pub use push::{PushCache, PushError, push_to_caches};
pub use runner::{BuildError, BuildOutcome, BuildRequest, BuildStatus, run_build};
