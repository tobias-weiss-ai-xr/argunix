//! Pure domain types for argunix.

mod attr_path;
mod builder;
mod derivation;
mod forge_kind;
mod ids;
mod image_format;
mod sha;
mod slug;
mod status;

pub use attr_path::AttrPath;
pub use builder::{
    BuilderCapabilities, BuilderName, BuilderNameError, BuilderPubkey, BuilderPubkeyError,
};
pub use derivation::DerivationInfo;
pub use forge_kind::ForgeKind;
pub use ids::{BuilderId, EvalId, JobId, RepoId};
pub use image_format::{ImageFormat, ImageFormatError};
pub use sha::{Sha, ShaError};
pub use slug::{Slug, SlugError};
pub use status::{EvalStatus, JobStatus, ParseStatusError};
