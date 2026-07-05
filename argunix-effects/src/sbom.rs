//! The `sbom-attach` effect: generate a CycloneDX SBOM for an OCI
//! image and attach it to the pushed image as an OCI *referrer*.
//!
//! `design/sbom.md` is the full design context. The short version:
//! because argunix ran the build it holds the exact runtime closure of
//! the image's contents — there is no scanning and no guessing, the
//! way Syft / Trivy / Grype must. This module *transcribes* that
//! closure into a CycloneDX document.
//!
//! ## What the SBOM is *of*
//!
//! The **runtime** contents of the image — what is shipped in the
//! running container — never the build-time closure (which drags in
//! `tar`, the layered-image builder, …).
//!
//! By default argunix reads this straight out of the built image. A
//! `dockerTools` OCI layer blob is a tarball of `/nix/store/<path>`
//! directory trees, and dockerTools ships the *full* runtime closure
//! of the image's contents — so enumerating the `/nix/store` entries
//! across every layer yields the exact set of store paths the image
//! ships. No flake cooperation, no scanner heuristics, and a multi-arch
//! image index just means more layer blobs: every platform's paths are
//! unioned into one SBOM.
//!
//! A flake may instead *declare* the contents via
//! `meta.sbom-runtime-roots` — a list of store paths whose
//! `nix-store --query --requisites` closure becomes the SBOM. When
//! present this *overrides* the layer scan; it is the graph-pure path
//! for callers that want it (the declared roots must be in the local
//! store). See `design/sbom.md`.
//!
//! ## Placement
//!
//! The SBOM is **not** injected into the image — that would change the
//! image digest. It is pushed as a *separate* artifact whose manifest
//! `subject` points at the image, via `oras attach`. The image is
//! byte-identical to what nix built; the SBOM is discoverable with
//! `oras discover` / `cosign download sbom`.
//!
//! Scope is `oci` images only (see `design/sbom.md`); a `docker` job is
//! skipped. Generation is pure (a function of the nix store graph);
//! attachment is the impure, authenticated part. [`SbomAttach`] does
//! both in one [`Effect::run`] so the call site stays effect-shaped.

use crate::{Effect, EffectOutcome, OutputContext, Severity, image_segment};
use async_trait::async_trait;
use flate2::read::GzDecoder;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

/// Standard CycloneDX JSON media type — what `oras attach` records as
/// the referrer's `artifactType` and what devguard / scanners filter
/// on when they discover referrers.
pub const CYCLONEDX_MEDIA_TYPE: &str = "application/vnd.cyclonedx+json";

/// Timeout for the `nix-store --query --requisites` closure walk.
const GENERATE_TIMEOUT: Duration = Duration::from_secs(120);
/// Timeout for one `oras attach`. Uploads one small JSON blob plus a
/// manifest — generous, but a hung registry must not wedge the effect.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(300);

/// Extract `meta.sbom-runtime-roots` from a job's `meta` JSON: the
/// store paths the flake declares as the image's runtime contents.
/// A missing attribute, a non-array, or non-string entries yield an
/// empty list — the effect then skips rather than failing.
pub fn runtime_roots(meta: &Value) -> Vec<String> {
    meta.get("sbom-runtime-roots")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Generate a CycloneDX SBOM for an OCI image and attach it to the
/// already-pushed image on `target` as an OCI referrer. Built by the
/// worker from the same `argunix_config::Registry` entry as the paired
/// [`RegistryPush`](crate::RegistryPush), and run *after* it.
#[derive(Debug, Clone)]
pub struct SbomAttach {
    /// Configured registry name (the key under `registries:`).
    /// Surfaced verbatim in `effect_runs.target`.
    pub target: String,
    /// Registry host[:port], no scheme — same value `RegistryPush` uses.
    pub registry_url: String,
    /// Namespace the image lands under; may contain `{slug}`, resolved
    /// per-build to the repo slug.
    pub namespace: String,
    /// File holding one `user:password` line. `None` ⇒ anonymous.
    pub auth_path: Option<PathBuf>,
    /// Plain-HTTP registry — adds `--plain-http` to `oras`.
    pub insecure: bool,
}

#[async_trait]
impl Effect for SbomAttach {
    fn kind(&self) -> &'static str {
        "sbom-attach"
    }

    fn target(&self) -> &str {
        &self.target
    }

    fn severity(&self) -> Severity {
        // Recorded in `effect_runs` only, no forge check — same policy
        // as `registry-push`. A failed SBOM attach is not a property of
        // the repo's commit and must not redden its forge status.
        Severity::Advisory
    }

    async fn run(&self, ctx: &OutputContext<'_>) -> EffectOutcome {
        // Any container image — `docker` or `oci`. The CycloneDX
        // document is transcribed from the image's `/nix/store`
        // closure, which both archive formats carry. (For a multi-arch
        // `docker` group this per-job effect is suppressed: the fan-in
        // attaches a per-arch SBOM to each per-arch manifest digest —
        // see `design/multi-arch.md`.)
        if ctx.image_format.is_none() {
            return EffectOutcome::skipped("not a container image");
        }
        let image = image_segment(ctx.attr_path);
        let namespace = self.namespace.replace("{slug}", ctx.repo_slug);
        // Attach to the immutable `sha-<short>` tag — always pushed by
        // `registry-push`, and stable, so the SBOM's `subject` digest
        // does not move under it.
        let tag = format!("sha-{}", ctx.short_sha());
        let reference = format!(
            "{}/{}/{}:{}",
            self.registry_url.trim_end_matches('/'),
            namespace.trim_matches('/'),
            image,
            tag,
        );

        // Generation — pure (see `generate_sbom`).
        let (sbom, n_components) =
            match generate_sbom(ctx.attr_path, ctx.output_paths, ctx.sbom_runtime_roots).await {
                Ok(v) => v,
                Err(e) => return EffectOutcome::failure(e),
            };

        // Credentials read here, never at config time, never logged.
        let creds = match &self.auth_path {
            Some(path) => match tokio::fs::read_to_string(path).await {
                Ok(s) => Some(s.trim().to_string()),
                Err(e) => {
                    return EffectOutcome::failure(format!(
                        "reading registry credentials {}: {e}",
                        path.display(),
                    ));
                }
            },
            None => None,
        };

        let hint = format!("argunix-sbom-{image}-{}", ctx.short_sha());
        match attach_cyclonedx_referrer(&reference, &sbom, &hint, self.insecure, creds.as_deref())
            .await
        {
            Ok(()) => EffectOutcome::success(format!(
                "attached SBOM ({n_components} components) to {reference}",
            )),
            Err(detail) => {
                EffectOutcome::failure(format!("attaching SBOM to {reference}: {detail}",))
            }
        }
    }
}

/// Stage `sbom` to a temp file and `oras attach` it as a CycloneDX
/// referrer of `reference` — the artifact's manifest `subject` is the
/// target manifest. On a registry without the OCI 1.1 referrers API,
/// `oras` transparently falls back to the referrers tag schema.
///
/// Shared by the single-arch [`SbomAttach`] effect and the multi-arch
/// fan-in ([`crate::multiarch`]), which attaches a per-arch SBOM to
/// each per-arch manifest digest. `name_hint` becomes the staged
/// filename — and so the artifact's title annotation — so pass
/// something unique per attach to keep concurrent attaches from
/// colliding on the temp path.
pub(crate) async fn attach_cyclonedx_referrer(
    reference: &str,
    sbom: &[u8],
    name_hint: &str,
    insecure: bool,
    creds: Option<&str>,
) -> Result<(), String> {
    let staged = std::env::temp_dir().join(format!("{name_hint}.cdx.json"));
    if let Err(e) = tokio::fs::write(&staged, sbom).await {
        return Err(format!("writing SBOM to {}: {e}", staged.display()));
    }
    let result = run_oras_attach(&staged, reference, insecure, creds).await;
    let _ = tokio::fs::remove_file(&staged).await;
    result
}

/// `oras attach --artifact-type <cyclonedx> <image-ref> <file>`.
async fn run_oras_attach(
    file: &Path,
    reference: &str,
    insecure: bool,
    creds: Option<&str>,
) -> Result<(), String> {
    // `oras` records the file argument verbatim as the artifact's
    // title annotation and rejects absolute paths. Run it from the
    // file's directory and pass the bare name, so the title is a
    // clean filename (and `oras pull` writes it back sensibly).
    let dir = file.parent().unwrap_or_else(|| Path::new("."));
    let name = file
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| "staged SBOM path has no file name".to_string())?;

    let mut cmd = Command::new("oras");
    cmd.current_dir(dir)
        .arg("attach")
        .arg("--artifact-type")
        .arg(CYCLONEDX_MEDIA_TYPE);
    // `oras` may touch `$HOME` (docker-style config / cache). The
    // daemon runs as a system user whose home may be unwritable —
    // point it at the temp dir so an anonymous push never fails on
    // a read-only home.
    cmd.env("HOME", std::env::temp_dir());
    if insecure {
        cmd.arg("--plain-http");
    }
    if let Some(creds) = creds {
        let Some((user, pass)) = creds.split_once(':') else {
            return Err("credentials file is not in `user:password` form".into());
        };
        cmd.arg("--username").arg(user).arg("--password").arg(pass);
    }
    cmd.arg(reference)
        .arg(format!("{name}:{CYCLONEDX_MEDIA_TYPE}"));
    run_capture(cmd, "oras attach", ATTACH_TIMEOUT)
        .await
        .map(|_| ())
}

/// Generate the CycloneDX SBOM for an OCI image job: resolve the
/// runtime store paths (from the image layers, or `meta`-declared
/// roots) and serialize a CycloneDX document. Returns
/// `(json_bytes, component_count)`.
///
/// Pure — a function of the Nix store graph, and deterministic (no
/// timestamp). The daemon calls this both to attach the SBOM as a
/// registry referrer ([`SbomAttach`]) and to persist it to the store;
/// because it is deterministic, both callers get byte-identical
/// documents. Takes the raw inputs rather than an [`OutputContext`] so
/// the daemon can call it without building one. `Err` carries a
/// human-readable reason.
pub async fn generate_sbom(
    attr_path: &str,
    output_paths: &[String],
    sbom_runtime_roots: &[String],
) -> Result<(Vec<u8>, usize), String> {
    let image = image_segment(attr_path);
    let store_paths = if sbom_runtime_roots.is_empty() {
        let archive = output_paths
            .first()
            .ok_or_else(|| "build produced no image archive to read".to_string())?;
        store_paths_from_image(archive)
            .await
            .map_err(|e| format!("reading image contents: {e}"))?
    } else {
        closure_from_roots(sbom_runtime_roots)
            .await
            .map_err(|e| format!("resolving sbom roots: {e}"))?
    };
    if store_paths.is_empty() {
        return Err("no /nix/store paths found for the image".to_string());
    }
    let count = store_paths.len();
    let doc = build_cyclonedx(&image, &store_paths);
    let bytes = serde_json::to_vec_pretty(&doc).map_err(|e| format!("serializing SBOM: {e}"))?;
    Ok((bytes, count))
}

/// Resolve declared `meta.sbom-runtime-roots` to the full runtime
/// closure via `nix-store --query --requisites`. The graph-pure path —
/// used only when the flake opts in; the roots must be valid in the
/// local store. Returns the closure as sorted `/nix/store/...` paths.
async fn closure_from_roots(roots: &[String]) -> Result<Vec<String>, String> {
    // The roots come straight from the repo's `meta.sbom-runtime-roots`,
    // so they are untrusted. `nix-store`'s argument parser is
    // order-independent: a root beginning with `-` would be read as an
    // option, not a path, injecting flags into a coordinator-side nix
    // invocation. Reject anything that is not a plain absolute store
    // path. See bugs.md SEC-2.
    for root in roots {
        if !root.starts_with("/nix/store/") {
            return Err(format!(
                "sbom-runtime-roots entry is not a /nix/store path: {root:?}"
            ));
        }
    }
    let mut cmd = Command::new("nix-store");
    cmd.arg("--query").arg("--requisites");
    for root in roots {
        cmd.arg(root);
    }
    let stdout = run_capture(cmd, "nix-store --query --requisites", GENERATE_TIMEOUT).await?;

    let mut paths: Vec<String> = String::from_utf8_lossy(&stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Read the store paths the image actually ships out of its OCI layer
/// blobs — the default contents source, needing no flake cooperation.
///
/// The blocking tar/decompression walk runs on a blocking thread.
async fn store_paths_from_image(archive: &str) -> Result<Vec<String>, String> {
    let archive = archive.to_string();
    tokio::task::spawn_blocking(move || scan_oci_archive(Path::new(&archive)))
        .await
        .map_err(|e| format!("sbom scan task panicked: {e}"))?
}

/// How a blob in the OCI archive is packed. A blob that matches none of
/// these is not a layer (a JSON manifest / config) and is skipped.
enum Blob {
    Gzip,
    Zstd,
    Tar,
}

/// Bytes sniffed from the head of each blob — enough to reach the
/// `ustar` magic a POSIX tar carries at offset 257.
const SNIFF_LEN: usize = 512;

/// Classify a blob from its leading bytes: gzip / zstd magic, or the
/// `ustar` magic of an uncompressed tar. `None` ⇒ not a layer.
fn classify(head: &[u8]) -> Option<Blob> {
    if head.len() >= 2 && head[0] == 0x1f && head[1] == 0x8b {
        return Some(Blob::Gzip);
    }
    if head.len() >= 4 && head[..4] == [0x28, 0xb5, 0x2f, 0xfd] {
        return Some(Blob::Zstd);
    }
    if head.len() >= 262 && &head[257..262] == b"ustar" {
        return Some(Blob::Tar);
    }
    None
}

/// Scan an OCI image archive and union the `/nix/store` directory names
/// across all of its layer blobs.
///
/// The archive itself may be a plain, gzip- or zstd-compressed tar (the
/// `oci-image-*.tar.gz` shape nix produces), and inside it each layer
/// blob may again be compressed — both levels are sniffed and
/// decompressed. JSON manifest / config blobs do not classify as a tar
/// and are skipped; a multi-arch index simply has more layer blobs, so
/// every platform's paths land in one set.
fn scan_oci_archive(archive: &Path) -> Result<Vec<String>, String> {
    let file =
        std::fs::File::open(archive).map_err(|e| format!("opening {}: {e}", archive.display()))?;
    let outer = open_maybe_tar(file)
        .map_err(|e| format!("reading image archive: {e}"))?
        .ok_or("image archive is not a tar (plain, gzip or zstd)")?;

    let mut names: BTreeSet<String> = BTreeSet::new();
    scan_layout_tar(outer, &mut names)?;
    Ok(names
        .into_iter()
        .map(|n| format!("/nix/store/{n}"))
        .collect())
}

/// Iterate one OCI-layout tar; every blob is tried as a layer.
fn scan_layout_tar<R: Read>(reader: R, names: &mut BTreeSet<String>) -> Result<(), String> {
    let mut layout = tar::Archive::new(reader);
    let entries = layout
        .entries()
        .map_err(|e| format!("reading image archive entries: {e}"))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("reading image archive entry: {e}"))?;
        if entry.header().entry_type().is_dir() {
            continue;
        }
        // A blob that does not sniff as a tar is a JSON manifest /
        // config — skip it.
        if let Some(layer) =
            open_maybe_tar(entry).map_err(|e| format!("reading layer blob: {e}"))?
        {
            collect_layer(layer, names)?;
        }
    }
    Ok(())
}

/// Sniff the head of `reader`; if it is a plain, gzip- or
/// zstd-compressed tar, return a reader yielding the decompressed tar
/// stream. `None` when the bytes are not a tar at all. The sniffed
/// prefix is chained back in front so no bytes are lost.
fn open_maybe_tar<'a, R: Read + 'a>(mut reader: R) -> std::io::Result<Option<Box<dyn Read + 'a>>> {
    let mut head = [0u8; SNIFF_LEN];
    let n = read_fill(&mut reader, &mut head)?;
    let Some(kind) = classify(&head[..n]) else {
        return Ok(None);
    };
    let chained = Cursor::new(head[..n].to_vec()).chain(reader);
    Ok(Some(match kind {
        Blob::Gzip => Box::new(GzDecoder::new(chained)),
        Blob::Zstd => Box::new(zstd::stream::read::Decoder::new(chained)?),
        Blob::Tar => Box::new(chained),
    }))
}

/// Read into `buf` until it is full or the reader is exhausted,
/// returning the number of bytes actually read. `Read::read` may return
/// short, so a single call cannot be trusted to fill the sniff buffer.
fn read_fill<R: Read>(reader: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// Read one layer tar and insert every `/nix/store/<name>` directory
/// name it carries into `names`.
fn collect_layer<R: Read>(reader: R, names: &mut BTreeSet<String>) -> Result<(), String> {
    let mut layer = tar::Archive::new(reader);
    let entries = layer.entries().map_err(|e| format!("reading layer: {e}"))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("reading layer entry: {e}"))?;
        let Ok(path) = entry.path() else {
            continue;
        };
        if let Some(name) = nix_store_name(&path) {
            names.insert(name);
        }
    }
    Ok(())
}

/// `[./]nix/store/<name>/…` → `Some("<name>")`. Any other path, and the
/// store-optimiser `.links` directory (no `-`, unlike a real store-path
/// name), yield `None`.
fn nix_store_name(path: &Path) -> Option<String> {
    let mut comps = path.components().filter_map(|c| match c {
        std::path::Component::Normal(s) => s.to_str(),
        _ => None,
    });
    if comps.next()? != "nix" || comps.next()? != "store" {
        return None;
    }
    let name = comps.next()?;
    name.contains('-').then(|| name.to_string())
}

/// Assemble the CycloneDX 1.5 JSON document. One `component` per store
/// path in the closure; `metadata.component` is the image itself.
///
/// There is deliberately no `metadata.timestamp` — the SBOM is then a
/// pure function of the closure, so the SBOM of a content-addressed
/// store path is itself reproducible.
fn build_cyclonedx(image_name: &str, paths: &[String]) -> Value {
    let components: Vec<Value> = paths
        .iter()
        .map(|path| {
            let base = store_path_basename(path);
            let (name, version) = parse_name_version(base);
            let purl = match &version {
                Some(v) => format!("pkg:nix/{name}@{v}"),
                None => format!("pkg:nix/{name}"),
            };
            let mut component = json!({
                "type": "library",
                "bom-ref": base,
                "name": name,
                "purl": purl,
                "properties": [
                    { "name": "nix:store_path", "value": path },
                ],
            });
            if let Some(v) = version {
                component["version"] = json!(v);
            }
            component
        })
        .collect();

    json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "metadata": {
            "tools": [
                { "vendor": "applicative.systems", "name": "argunix" },
            ],
            "component": {
                "type": "container",
                "bom-ref": image_name,
                "name": image_name,
            },
        },
        "components": components,
    })
}

/// `/nix/store/<hash>-<rest>` → `<hash>-<rest>` (the last path
/// segment). Used verbatim as a component's unique `bom-ref`.
fn store_path_basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Split a store-path basename into `(name, version)`.
///
/// A basename is `<hash>-<name>[-<version>][-<suffix>…]`. The leading
/// 32-char hash is dropped (everything up to the first `-`); the
/// remainder is split at the first dash-segment that starts with a
/// digit — that segment and everything after it is the version.
/// A basename with no digit-led segment has no version.
fn parse_name_version(basename: &str) -> (String, Option<String>) {
    // Drop the hash: everything up to and including the first `-`.
    let rest = basename.split_once('-').map(|(_, r)| r).unwrap_or(basename);
    let segments: Vec<&str> = rest.split('-').collect();
    let version_at = segments
        .iter()
        .position(|seg| seg.chars().next().is_some_and(|c| c.is_ascii_digit()));
    match version_at {
        // A name is required; a basename that is *only* a version
        // (`version_at == 0`) keeps the whole string as the name.
        Some(idx) if idx > 0 => (segments[..idx].join("-"), Some(segments[idx..].join("-"))),
        _ => (rest.to_string(), None),
    }
}

/// Spawn `cmd`, wait with a timeout, return stdout on a zero exit.
/// `Err` carries a human reason (spawn error, timeout, or the trimmed
/// stderr). `kill_on_drop` plus the timeout guarantees no orphan.
/// Shared with `multiarch` — both shell out to the same image tools.
pub(crate) async fn run_capture(
    mut cmd: Command,
    what: &str,
    limit: Duration,
) -> Result<Vec<u8>, String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().map_err(|e| format!("spawning {what}: {e}"))?;

    let output = match timeout(limit, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(format!("waiting for {what}: {e}")),
        Err(_) => {
            return Err(format!("{what} timed out after {}s", limit.as_secs()));
        }
    };
    if output.status.success() {
        return Ok(output.stdout);
    }
    Err(format!(
        "{what} exited {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).trim(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ImageFormat;

    #[test]
    fn runtime_roots_reads_string_array() {
        let meta = json!({ "sbom-runtime-roots": ["/nix/store/a", "/nix/store/b"] });
        assert_eq!(runtime_roots(&meta), vec!["/nix/store/a", "/nix/store/b"]);
    }

    #[test]
    fn runtime_roots_missing_or_wrong_shape_is_empty() {
        assert!(runtime_roots(&json!({})).is_empty());
        assert!(runtime_roots(&json!({ "sbom-runtime-roots": "not-an-array" })).is_empty());
        // Non-string entries are dropped, not fatal.
        assert_eq!(
            runtime_roots(&json!({ "sbom-runtime-roots": ["/nix/store/a", 7] })),
            vec!["/nix/store/a"],
        );
    }

    #[test]
    fn parse_name_version_splits_at_first_digit_segment() {
        assert_eq!(
            parse_name_version("abc123-busybox-1.36.1"),
            ("busybox".into(), Some("1.36.1".into())),
        );
        assert_eq!(
            parse_name_version("abc123-bash-interactive-5.2p37"),
            ("bash-interactive".into(), Some("5.2p37".into())),
        );
        // Multi-segment version (`<ver>-<patch-count>`) stays whole.
        assert_eq!(
            parse_name_version("abc123-glibc-2.39-31"),
            ("glibc".into(), Some("2.39-31".into())),
        );
    }

    #[test]
    fn parse_name_version_no_version() {
        assert_eq!(parse_name_version("abc123-source"), ("source".into(), None));
    }

    #[test]
    fn store_path_basename_takes_last_segment() {
        assert_eq!(
            store_path_basename("/nix/store/abc123-busybox-1.36.1"),
            "abc123-busybox-1.36.1",
        );
    }

    #[test]
    fn build_cyclonedx_shape() {
        let doc = build_cyclonedx(
            "my-image",
            &["/nix/store/abc123-busybox-1.36.1".to_string()],
        );
        assert_eq!(doc["bomFormat"], "CycloneDX");
        assert_eq!(doc["specVersion"], "1.5");
        assert_eq!(doc["metadata"]["component"]["name"], "my-image");
        let comp = &doc["components"][0];
        assert_eq!(comp["name"], "busybox");
        assert_eq!(comp["version"], "1.36.1");
        assert_eq!(comp["purl"], "pkg:nix/busybox@1.36.1");
        assert_eq!(
            comp["properties"][0]["value"],
            "/nix/store/abc123-busybox-1.36.1",
        );
    }

    fn oci_ctx<'a>(roots: &'a [String], image_format: Option<ImageFormat>) -> OutputContext<'a> {
        OutputContext {
            forge: "gh",
            repo_slug: "o/r",
            attr_path: "packages.x86_64-linux.img",
            system: "x86_64-linux",
            git_ref: "refs/heads/main",
            default_branch: Some("main"),
            sha: "0123456789abcdef0123456789abcdef01234567",
            image_format,
            output_paths: &[],
            sbom_runtime_roots: roots,
        }
    }

    fn attach() -> SbomAttach {
        SbomAttach {
            target: "local".into(),
            registry_url: "127.0.0.1:5000".into(),
            namespace: "myorg".into(),
            auth_path: None,
            insecure: true,
        }
    }

    #[tokio::test]
    async fn skips_non_image_jobs() {
        // A job with no image format is not a container image — the
        // SBOM effect has nothing to attach.
        let roots = vec!["/nix/store/x".to_string()];
        let ctx = oci_ctx(&roots, None);
        let outcome = attach().run(&ctx).await;
        assert_eq!(outcome.status, crate::EffectStatus::Skipped);
    }

    #[tokio::test]
    async fn docker_and_oci_images_are_both_in_scope() {
        // Both archive formats carry a `/nix/store` closure, so both
        // are SBOM'd. With neither declared roots nor a build output to
        // scan, generation *fails* rather than skipping — proving the
        // effect ran instead of bailing on the format.
        for fmt in [ImageFormat::Docker, ImageFormat::Oci] {
            let ctx = oci_ctx(&[], Some(fmt));
            let outcome = attach().run(&ctx).await;
            assert_eq!(outcome.status, crate::EffectStatus::Failure);
            assert!(
                outcome.detail.contains("no image archive"),
                "got: {}",
                outcome.detail,
            );
        }
    }

    #[test]
    fn classify_detects_layer_packings() {
        assert!(matches!(classify(&[0x1f, 0x8b, 0, 0]), Some(Blob::Gzip)));
        assert!(matches!(
            classify(&[0x28, 0xb5, 0x2f, 0xfd]),
            Some(Blob::Zstd),
        ));
        let mut tar_head = vec![0u8; SNIFF_LEN];
        tar_head[257..262].copy_from_slice(b"ustar");
        assert!(matches!(classify(&tar_head), Some(Blob::Tar)));
        // A JSON manifest / config blob is not a layer.
        assert!(classify(br#"{"schemaVersion":2}"#).is_none());
    }

    #[test]
    fn nix_store_name_extracts_the_store_path() {
        assert_eq!(
            nix_store_name(Path::new("nix/store/abc-foo-1.0/bin/foo")),
            Some("abc-foo-1.0".to_string()),
        );
        // A leading `./` (how tar often records paths) is tolerated.
        assert_eq!(
            nix_store_name(Path::new("./nix/store/abc-bar")),
            Some("abc-bar".to_string()),
        );
        // …as is an absolute path — nix image layers record these.
        assert_eq!(
            nix_store_name(Path::new("/nix/store/abc-baz-2.0/lib/x.so")),
            Some("abc-baz-2.0".to_string()),
        );
        // Not under /nix/store, and the store-optimiser `.links` dir.
        assert_eq!(nix_store_name(Path::new("etc/passwd")), None);
        assert_eq!(nix_store_name(Path::new("nix/store/.links")), None);
    }

    #[test]
    fn collect_layer_unions_nix_store_dirs() {
        // A plain tar carrying two store paths plus an unrelated file.
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            for path in [
                "nix/store/aaa-foo-1.0/bin/foo",
                "nix/store/bbb-libc-2.39/lib/libc.so",
                "etc/hostname",
            ] {
                let data = b"x";
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_entry_type(tar::EntryType::Regular);
                header.set_cksum();
                builder.append_data(&mut header, path, &data[..]).unwrap();
            }
            builder.finish().unwrap();
        }
        let mut names = BTreeSet::new();
        collect_layer(Cursor::new(tar_bytes), &mut names).unwrap();
        assert_eq!(
            names.into_iter().collect::<Vec<_>>(),
            vec!["aaa-foo-1.0".to_string(), "bbb-libc-2.39".to_string()],
        );
    }

    #[test]
    fn scan_layout_tar_reads_compressed_layers_and_skips_json() {
        use std::io::Write;

        // A gzip-compressed layer carrying one store path, recorded
        // with an absolute `/nix/store/...` path as nix images do.
        let layer_gz = {
            let mut layer = Vec::new();
            {
                let mut builder = tar::Builder::new(&mut layer);
                let data = b"x";
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_entry_type(tar::EntryType::Regular);
                header.set_cksum();
                builder
                    .append_data(&mut header, "nix/store/zzz-pkg-1.0/bin/pkg", &data[..])
                    .unwrap();
                builder.finish().unwrap();
            }
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            enc.write_all(&layer).unwrap();
            enc.finish().unwrap()
        };

        // An OCI-layout tar holding that layer blob plus a JSON blob —
        // the JSON manifest must be skipped, the layer scanned.
        let mut layout = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut layout);
            for (path, body) in [
                ("blobs/sha256/aaa", layer_gz.as_slice()),
                ("index.json", br#"{"schemaVersion":2}"#.as_slice()),
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_entry_type(tar::EntryType::Regular);
                header.set_cksum();
                builder.append_data(&mut header, path, body).unwrap();
            }
            builder.finish().unwrap();
        }

        let mut names = BTreeSet::new();
        scan_layout_tar(Cursor::new(layout), &mut names).unwrap();
        assert_eq!(
            names.into_iter().collect::<Vec<_>>(),
            vec!["zzz-pkg-1.0".to_string()],
        );
    }
}
