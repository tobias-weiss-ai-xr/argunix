//! Sandboxed wrapper around `nix-eval-jobs`.
//!
//! v1 scope: flake mode only, walks `packages.<system>`, `checks.<system>`,
//! and `devShells.<system>` for each system in the request. Non-flake mode,
//! `nixosConfigurations`, `hydraJobs`, `homeConfigurations` and
//! `darwinConfigurations` are deferred to a follow-up.
//!
//! Each fragment is evaluated by spawning `nix-eval-jobs` once and parsing
//! its JSON-lines output. Network access is allowed by default (matches the
//! M2 plan and is required for IFD); a wall-clock timeout from the request
//! caps each subprocess. Memory limits are enforced by systemd in production
//! (M9); we only apply timeouts here.

mod jobspec;
mod runner;
mod systems;

pub use jobspec::{JobSpec, ParseError, RawJob, parse_lines};
pub use runner::{EvalError, EvalRequest, evaluate};
pub use systems::detect_local_systems;

/// Top-level flake outputs medusa walks per system. See M2 in `design/plan.md`.
pub const DEFAULT_FLAKE_OUTPUTS: &[&str] = &["packages", "checks", "devShells"];
