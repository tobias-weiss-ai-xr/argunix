//! Sandboxed wrapper around `nix-eval-jobs`.
//!
//! Flake mode only. By default we walk:
//!
//! - `packages.<system>`, `checks.<system>`, `devShells.<system>` for
//!   each requested system (per-system fan-out).
//! - `nixosConfigurations.<name>` and `homeConfigurations.<name>` once
//!   each, with `--apply` extracting the buildable derivation (the
//!   toplevel system / the activation package). The system is taken
//!   from the resulting derivation, so per-job system filtering
//!   downstream still works without our caller knowing the target
//!   architecture up front.
//!
//! Non-flake mode, `hydraJobs`, and `darwinConfigurations` are not
//! covered. Each fragment is evaluated by spawning `nix-eval-jobs`
//! once and parsing its JSON-lines output. Network access is allowed
//! by default (required for IFD); a wall-clock timeout from the
//! request caps each subprocess. Memory limits are enforced by
//! systemd in production; we only apply timeouts here.

mod jobspec;
mod runner;
mod systems;

pub use jobspec::{JobSpec, ParseError, RawJob, parse_lines};
pub use runner::{EvalError, EvalRequest, FlakeOutput, FragmentKind, evaluate};
pub use systems::detect_local_systems;

/// The flake outputs argunix walks by default. Three per-system
/// fan-outs (`packages`, `checks`, `devShells`) and two `--apply`
/// outputs (`nixosConfigurations`, `homeConfigurations`).
pub fn default_flake_outputs() -> Vec<FlakeOutput> {
    vec![
        FlakeOutput {
            name: "packages".into(),
            kind: FragmentKind::PerSystem,
        },
        FlakeOutput {
            name: "checks".into(),
            kind: FragmentKind::PerSystem,
        },
        FlakeOutput {
            name: "devShells".into(),
            kind: FragmentKind::PerSystem,
        },
        FlakeOutput {
            name: "nixosConfigurations".into(),
            kind: FragmentKind::Apply {
                fn_expr: "x: x.config.system.build.toplevel".into(),
            },
        },
        FlakeOutput {
            name: "homeConfigurations".into(),
            kind: FragmentKind::Apply {
                fn_expr: "x: x.activationPackage".into(),
            },
        },
    ]
}
