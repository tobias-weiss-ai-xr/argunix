use serde::{Deserialize, Serialize};

/// Information about one derivation, sufficient to dispatch a build of
/// it on a remote builder and to participate in dependency-aware
/// scheduling.
///
/// `input_drvs` is the set of *direct* input derivations of `drv_path`
/// (full paths, e.g. `/nix/store/aaaa-foo.drv`). Strategies that gate
/// on dependencies use this to build a graph; the closure walker
/// (`argunix-eval`) populates it from `nix derivation show --recursive`.
///
/// Pure data type with no behaviour: lives in `argunix-domain` because
/// both `argunix-eval` (producer) and `argunix-sched` (consumer)
/// depend on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivationInfo {
    pub drv_path: String,
    pub system: Option<String>,
    pub required_features: Vec<String>,
    pub input_drvs: Vec<String>,
}
