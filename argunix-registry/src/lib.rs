//! Read-only Docker Registry V2 surface for argunix-built docker images.
//!
//! When `meta.docker-image == true` is set on a derivation that
//! `dockerTools.{buildImage,buildLayeredImage}` produces, argunix:
//!
//! 1. Builds it via the normal pipeline (its output is a docker-archive
//!    tarball in the nix store).
//! 2. After `BuildStatus::Success`, calls [`publish::publish`] which
//!    shells out to `skopeo copy docker-archive:<store-path> dir:<tmp>`,
//!    moves the per-layer/config blobs into a content-addressed pool,
//!    and inserts a row into `docker_images`.
//! 3. Serves the result under `/v2/<forge>/<owner>/<repo>/<attr>` via
//!    [`api::router`]. Multi-arch is assembled at request time as an
//!    OCI image index across every per-system row of the same
//!    `(image_name, git_ref)` tuple.

pub mod api;
pub mod convert;
pub mod publish;
pub mod state;

pub use api::router;
pub use publish::{PublishError, publish};
pub use state::RegistryState;
