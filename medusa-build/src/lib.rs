//! Per-job build dispatcher.
//!
//! The unit of work is a single derivation, identified by its `.drv` path.
//! For each one we either skip the build (the output is already in a
//! configured binary cache, Q59) or run `nix-store --realise` and capture the
//! build log to a zstd-compressed file. Successful builds get a GC root
//! under `/nix/var/nix/gcroots/per-user/medusa/<repo>/<eval>/<job>` (Q47/Q48).
//!
//! v1 scope (M3):
//! - synchronous build per job (the daemon's offline `medusa build`
//!   subcommand iterates jobs sequentially);
//! - cache-skip via `nix path-info --store <cache> <output-path>`;
//! - log capture: stream nix-store stderr to memory (capped at the
//!   configured size), write a single zstd-compressed file at the end;
//! - GC root: post-success `nix-store --add-root <root> --indirect <output>`.
//!
//! Cache push (Q17) is async-background and lands with the scheduler in M4.

mod cache;
mod gc_root;
mod log_capture;
mod runner;

pub use cache::{CacheCheckResult, CacheRef, check_cache};
pub use gc_root::{add_gc_root, gc_root_path};
pub use log_capture::{LogCaptureLimit, write_zstd_log};
pub use runner::{BuildError, BuildOutcome, BuildRequest, BuildStatus, run_build};
