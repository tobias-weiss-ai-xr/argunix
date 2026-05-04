//! YAML configuration for medusa.
//!
//! See `design/questions-answers.md` Q83 for the schema and the rules for
//! secret paths (env-var substitution, never-inline). Unknown keys are
//! rejected to catch typos early.

mod path;
mod schema;
mod secret;
mod validate;

pub use path::{ResolveError, resolve_path};
pub use schema::{
    BinaryCache, BuilderEnrollment, CloneConfig, CloneMethod, Config, EvalDefaults, EvalOverrides,
    ForgeAuth, ForgeAuthShapeError, ForgeConfig, Repo, Retention, Schedule, WebConfig,
};
pub use secret::SecretFile;
pub use validate::ValidationError;

use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("reading {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing YAML: {0}")]
    Parse(#[from] serde_yaml::Error),
    #[error(transparent)]
    Validate(#[from] ValidationError),
}

/// Read, parse, and validate (excluding existence of secret files) the YAML
/// at `path`. Use [`Config::validate_secrets_exist`] separately when you want
/// the daemon's full pre-flight check.
pub fn load(path: &Path) -> Result<Config, LoadError> {
    let body = std::fs::read_to_string(path).map_err(|e| LoadError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let config: Config = serde_yaml::from_str(&body)?;
    config.validate_references()?;
    Ok(config)
}
