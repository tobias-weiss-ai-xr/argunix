//! Read-only Docker Registry HTTP API V2.
//!
//! Routes (axum 0.8 path syntax):
//!
//! - `GET  /v2/`                                  — version probe (200 `{}`)
//! - `GET  /v2/{*name}/manifests/{reference}`     — manifest by tag or digest
//! - `HEAD /v2/{*name}/manifests/{reference}`     — same headers, no body
//! - `GET  /v2/{*name}/blobs/{digest}`            — blob by digest
//! - `HEAD /v2/{*name}/blobs/{digest}`            — same headers, no body
//! - `GET  /v2/{*name}/tags/list`                 — tags for one image
//!
//! Names may contain slashes (`codeberg/tfc/argunix/my-image`), so the
//! `name` capture is a wildcard that consumes everything up to the
//! literal `/manifests/`, `/blobs/`, or `/tags/` segment. Axum routes
//! these by suffix automatically.
//!
//! Multi-arch:
//!
//! When a tag-based manifest GET arrives, the handler asks the store
//! for every per-system row matching `(image_name, git_ref)` and
//! assembles an OCI image index referencing each child manifest by
//! digest. The index is hashed and cached as a blob in the pool so
//! subsequent requests get the same digest. Single-system images
//! still go through index assembly — the index has one entry, but
//! clients see a stable shape and can use `--platform` deterministically.

use crate::state::RegistryState;
use argunix_store::{DockerImageRecord, DockerImageStore, RepoStore, SqlxStore};
use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxPath, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::sync::Arc;

const OCI_INDEX_TYPE: &str = "application/vnd.oci.image.index.v1+json";
const OCI_MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

/// Wired into argunix-web's router. State is the registry state dir +
/// a handle to the SqlxStore for SQLite lookups.
#[derive(Clone)]
pub struct RegistryApi {
    pub state: Arc<RegistryState>,
    pub store: SqlxStore,
}

pub fn router(api: RegistryApi) -> Router {
    Router::new()
        .route("/v2/", get(version_probe))
        .route("/v2", get(version_probe))
        .route("/v2/{*tail}", get(dispatch_get).head(dispatch_head))
        .with_state(api)
}

async fn version_probe() -> impl IntoResponse {
    let mut headers = HeaderMap::new();
    headers.insert(
        "Docker-Distribution-Api-Version",
        HeaderValue::from_static("registry/2.0"),
    );
    (StatusCode::OK, headers, "{}")
}

/// Decompose `<name>/manifests/<ref>`, `<name>/blobs/<digest>`, or
/// `<name>/tags/list` and dispatch.
fn split_tail(tail: &str) -> Option<(&str, Endpoint<'_>)> {
    if let Some(idx) = tail.rfind("/manifests/") {
        let (name, rest) = tail.split_at(idx);
        let reference = &rest["/manifests/".len()..];
        return Some((name, Endpoint::Manifest(reference)));
    }
    if let Some(idx) = tail.rfind("/blobs/") {
        let (name, rest) = tail.split_at(idx);
        let digest = &rest["/blobs/".len()..];
        return Some((name, Endpoint::Blob(digest)));
    }
    if let Some(stripped) = tail.strip_suffix("/tags/list") {
        return Some((stripped, Endpoint::TagsList));
    }
    None
}

enum Endpoint<'a> {
    Manifest(&'a str),
    Blob(&'a str),
    TagsList,
}

async fn dispatch_get(State(api): State<RegistryApi>, AxPath(tail): AxPath<String>) -> Response {
    match split_tail(&tail) {
        Some((name, Endpoint::Manifest(reference))) => {
            handle_manifest(&api, name, reference, true).await
        }
        Some((name, Endpoint::Blob(digest))) => handle_blob(&api, name, digest, true).await,
        Some((name, Endpoint::TagsList)) => handle_tags(&api, name).await,
        _ => not_found("UNSUPPORTED", "endpoint not implemented"),
    }
}

async fn dispatch_head(State(api): State<RegistryApi>, AxPath(tail): AxPath<String>) -> Response {
    match split_tail(&tail) {
        Some((name, Endpoint::Manifest(reference))) => {
            handle_manifest(&api, name, reference, false).await
        }
        Some((name, Endpoint::Blob(digest))) => handle_blob(&api, name, digest, false).await,
        _ => not_found("UNSUPPORTED", "endpoint not implemented"),
    }
}

/// Resolve `image_name` + `reference` to a manifest. Reference is one
/// of:
///   * `sha256:<hex>` — exact manifest digest (per-arch child).
///   * `sha-<short>`  — short sha prefix; assemble index across systems.
///   * any other tag  — branch name; assemble index across systems for
///                       `refs/heads/<tag>`. Falls back to default
///                       branch when tag is `"latest"`.
async fn handle_manifest(api: &RegistryApi, name: &str, reference: &str, body: bool) -> Response {
    if let Some(digest_resp) = manifest_by_digest(api, name, reference, body).await {
        return digest_resp;
    }

    let rows = match resolve_tag(api, name, reference).await {
        Ok(rows) => rows,
        Err(resp) => return resp,
    };
    if rows.is_empty() {
        return not_found("MANIFEST_UNKNOWN", "no build matches that tag");
    }

    let (index_bytes, index_digest) = match assemble_index(api, &rows).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(OCI_INDEX_TYPE),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(index_bytes.len()));
    headers.insert(
        "Docker-Content-Digest",
        HeaderValue::from_str(&index_digest).expect("ascii digest"),
    );
    if body {
        (StatusCode::OK, headers, index_bytes).into_response()
    } else {
        (StatusCode::OK, headers, Body::empty()).into_response()
    }
}

/// `reference == "sha256:<hex>"` shortcut: serve any manifest-shaped
/// blob from the pool, regardless of whether it's a per-build
/// manifest (written by `convert`) or an assembled image index
/// (written by `assemble_index`). Returns `None` if `reference` is
/// not a digest, leaving tag resolution to the caller.
///
/// Why the blob pool and not `docker_images`: docker pull always
/// re-fetches the manifest it just received, by the digest we
/// announced via `Docker-Content-Digest`. For an image index that
/// digest is the index's own — and indexes have no `docker_images`
/// row, only a blob-pool entry. Per-build manifests are also
/// content-addressed in the same pool, so one lookup path covers
/// both shapes.
async fn manifest_by_digest(
    api: &RegistryApi,
    _name: &str,
    reference: &str,
    body: bool,
) -> Option<Response> {
    let Some(hex) = reference.strip_prefix("sha256:") else {
        return None;
    };
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(not_found("MANIFEST_UNKNOWN", "malformed digest"));
    }
    let path = api.state.blob_path(hex);
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Some(not_found(
                "MANIFEST_UNKNOWN",
                "no manifest with that digest",
            ));
        }
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "registry: blob read");
            return Some(internal_error());
        }
    };

    // The right Content-Type depends on the body shape: an image
    // index, a per-arch image manifest, or a docker v2 manifest.
    // Each one carries its own `mediaType` field at the top level;
    // honour it so clients that strict-match Accept get a payload
    // they understand.
    let media_type = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| {
            v.get("mediaType")
                .and_then(|s| s.as_str())
                .map(String::from)
        })
        .unwrap_or_else(|| OCI_MANIFEST_TYPE.to_string());

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&media_type)
            .unwrap_or_else(|_| HeaderValue::from_static(OCI_MANIFEST_TYPE)),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(bytes.len()));
    headers.insert(
        "Docker-Content-Digest",
        HeaderValue::from_str(reference).expect("ascii digest"),
    );
    if body {
        Some((StatusCode::OK, headers, bytes).into_response())
    } else {
        Some((StatusCode::OK, headers, Body::empty()).into_response())
    }
}

/// Resolve a tag to one row per system.
async fn resolve_tag(
    api: &RegistryApi,
    name: &str,
    reference: &str,
) -> Result<Vec<DockerImageRecord>, Response> {
    if let Some(short) = reference.strip_prefix("sha-") {
        return <SqlxStore as DockerImageStore>::rows_by_sha_prefix(&api.store, name, short)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "registry: rows_by_sha_prefix failed");
                internal_error()
            });
    }

    let git_ref = if reference == "latest" {
        match resolve_default_branch(api, name).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                return Err(not_found(
                    "MANIFEST_UNKNOWN",
                    "default branch not yet known for this repo",
                ));
            }
            Err(resp) => return Err(resp),
        }
    } else {
        format!("refs/heads/{reference}")
    };

    <SqlxStore as DockerImageStore>::latest_per_system_for_branch(&api.store, name, &git_ref)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "registry: latest_per_system_for_branch failed");
            internal_error()
        })
}

/// `image_name` is `<forge>/<owner>/<repo>/<attr>`. Look up
/// `(<forge>, <owner>/<repo>)` in the repos table to find the
/// default branch.
async fn resolve_default_branch(
    api: &RegistryApi,
    image_name: &str,
) -> Result<Option<String>, Response> {
    let mut parts = image_name.splitn(2, '/');
    let forge = parts.next().unwrap_or_default();
    let rest = parts.next().unwrap_or_default();
    // <owner>/<repo>/<attr> — split off the trailing /attr.
    let Some(idx) = rest.rfind('/') else {
        return Ok(None);
    };
    let slug_str = &rest[..idx];
    let slug = match argunix_domain::Slug::new(slug_str) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    let repo = match <SqlxStore as RepoStore>::find(&api.store, forge, &slug).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "registry: repos lookup failed");
            return Err(internal_error());
        }
    };
    Ok(repo
        .and_then(|r| r.default_branch)
        .map(|b| format!("refs/heads/{b}")))
}

#[derive(Serialize)]
struct OciIndex {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    #[serde(rename = "mediaType")]
    media_type: &'static str,
    manifests: Vec<OciIndexEntry>,
}

#[derive(Serialize)]
struct OciIndexEntry {
    #[serde(rename = "mediaType")]
    media_type: &'static str,
    digest: String,
    size: u64,
    platform: OciPlatform,
}

#[derive(Serialize)]
struct OciPlatform {
    architecture: &'static str,
    os: &'static str,
}

fn nix_system_to_platform(system: &str) -> Option<OciPlatform> {
    match system {
        "x86_64-linux" => Some(OciPlatform {
            architecture: "amd64",
            os: "linux",
        }),
        "aarch64-linux" => Some(OciPlatform {
            architecture: "arm64",
            os: "linux",
        }),
        "x86_64-darwin" => Some(OciPlatform {
            architecture: "amd64",
            os: "darwin",
        }),
        "aarch64-darwin" => Some(OciPlatform {
            architecture: "arm64",
            os: "darwin",
        }),
        _ => None,
    }
}

/// Assemble + cache an OCI image index across the per-system rows.
/// The index bytes are also written into the blob pool so that a
/// follow-up `GET /v2/<name>/manifests/sha256:<index-digest>` request
/// resolves through the same path.
async fn assemble_index(
    api: &RegistryApi,
    rows: &[DockerImageRecord],
) -> Result<(Vec<u8>, String), Response> {
    let mut entries = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(platform) = nix_system_to_platform(&row.system) else {
            // Skip systems we don't know how to map. Better: serve
            // them under a non-standard arch tag, but skip is fine
            // for the prototype.
            continue;
        };
        let size = match tokio::fs::metadata(&row.manifest_path).await {
            Ok(m) => m.len(),
            Err(e) => {
                tracing::warn!(path = %row.manifest_path, error = %e, "registry: manifest stat");
                return Err(internal_error());
            }
        };
        entries.push(OciIndexEntry {
            media_type: OCI_MANIFEST_TYPE,
            digest: row.manifest_digest.clone(),
            size,
            platform,
        });
    }
    let index = OciIndex {
        schema_version: 2,
        media_type: OCI_INDEX_TYPE,
        manifests: entries,
    };
    let bytes = serde_json::to_vec(&index).map_err(|e| {
        tracing::warn!(error = %e, "registry: index serialize");
        internal_error()
    })?;

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let hex = hex::encode(hasher.finalize());
    let digest = format!("sha256:{hex}");

    // Cache the index bytes under its digest so /manifests/sha256:<x>
    // can serve via the blob pool.
    let cache_path = api.state.blob_path(&hex);
    if !cache_path.exists() {
        if let Err(e) = tokio::fs::write(&cache_path, &bytes).await {
            // Non-fatal: serve the bytes anyway, just log.
            tracing::warn!(path = %cache_path.display(), error = %e, "registry: index cache write");
        }
    }
    Ok((bytes, digest))
}

async fn handle_blob(api: &RegistryApi, _name: &str, digest: &str, body: bool) -> Response {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return not_found("BLOB_UNKNOWN", "only sha256 digests are served");
    };
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) || hex.len() != 64 {
        return not_found("BLOB_UNKNOWN", "malformed digest");
    }
    let path = api.state.blob_path(hex);
    let meta = match tokio::fs::metadata(&path).await {
        Ok(m) => m,
        Err(_) => return not_found("BLOB_UNKNOWN", "no such blob"),
    };

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(meta.len()));
    headers.insert(
        "Docker-Content-Digest",
        HeaderValue::from_str(digest).expect("ascii digest"),
    );

    if !body {
        return (StatusCode::OK, headers, Body::empty()).into_response();
    }

    // Read the whole blob into memory. Fine for the prototype — layers
    // from `dockerTools` are small. Streaming via `tokio-util::io::
    // ReaderStream` is a follow-up once we know the typical sizes.
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "registry: blob read");
            return internal_error();
        }
    };
    (StatusCode::OK, headers, bytes).into_response()
}

async fn handle_tags(_api: &RegistryApi, name: &str) -> Response {
    // Without a "rows for image_name" query, keep this minimal: only
    // surface the digest of the most recent manifest seen, leaving a
    // proper enumeration to a follow-up. Returning an empty list is
    // spec-legal and avoids an extra index.
    #[derive(Serialize)]
    struct Tags<'a> {
        name: &'a str,
        tags: Vec<String>,
    }
    let body = serde_json::to_vec(&Tags {
        name,
        tags: Vec::new(),
    })
    .unwrap_or_default();
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    errors: [ErrorDetail<'a>; 1],
}
#[derive(Serialize)]
struct ErrorDetail<'a> {
    code: &'a str,
    message: &'a str,
}

fn not_found(code: &str, message: &str) -> Response {
    let body = serde_json::to_vec(&ErrorEnvelope {
        errors: [ErrorDetail { code, message }],
    })
    .unwrap_or_default();
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

fn internal_error() -> Response {
    let body = serde_json::to_vec(&ErrorEnvelope {
        errors: [ErrorDetail {
            code: "INTERNAL",
            message: "internal error; check daemon logs",
        }],
    })
    .unwrap_or_default();
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}
