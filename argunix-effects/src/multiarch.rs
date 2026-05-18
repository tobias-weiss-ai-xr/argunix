//! Multi-arch OCI image assembly — the *fan-in*.
//!
//! `design/multi-arch.md` is the full context. When a flake exposes a
//! `docker` image once per architecture (`packages.x86_64-linux.hello`,
//! `packages.aarch64-linux.hello`), argunix builds each as its own job,
//! then stitches the per-arch results into one multi-arch OCI index on
//! the external registry. This module is the pure subprocess half: push
//! each arch slice with `skopeo`, then `oras manifest index create` the
//! index. The daemon decides which jobs form a group and calls in here
//! once they have all built.

use crate::sbom::run_capture;
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;

/// Per-arch `skopeo copy` timeout — layer uploads dominate.
const COPY_TIMEOUT: Duration = Duration::from_secs(600);
/// `oras manifest index create` timeout — metadata only, quick.
const INDEX_TIMEOUT: Duration = Duration::from_secs(120);

/// Map a Nix system tuple to the OCI `architecture` an image-index
/// entry carries. `None` for a system with no OCI equivalent — that
/// slice is skipped rather than mis-tagged.
pub fn oci_arch(system: &str) -> Option<&'static str> {
    match system {
        "x86_64-linux" | "x86_64-darwin" => Some("amd64"),
        "aarch64-linux" | "aarch64-darwin" => Some("arm64"),
        "armv7l-linux" => Some("arm"),
        "i686-linux" => Some("386"),
        "riscv64-linux" => Some("riscv64"),
        _ => None,
    }
}

/// The tag set a multi-arch index is published under — the branch,
/// `latest` on the repo's default branch, and the immutable
/// `sha-<short>`. The fan-in counterpart of `registry::push_tags`,
/// computed from raw eval fields since the fan-in spans several jobs
/// and has no single `OutputContext`.
pub fn image_tags(git_ref: &str, short_sha: &str, default_branch: Option<&str>) -> Vec<String> {
    let branch = branch_of(git_ref);
    let mut tags: Vec<String> = Vec::new();
    if let Some(b) = branch {
        tags.push(sanitize_tag(b));
        if default_branch == Some(b) {
            tags.push("latest".to_string());
        }
    }
    let sha_tag = format!("sha-{short_sha}");
    if !tags.contains(&sha_tag) {
        tags.push(sha_tag);
    }
    tags
}

/// Branch name a `git_ref` denotes, or `None` for a PR / non-branch
/// ref. Mirrors `OutputContext::branch`.
fn branch_of(git_ref: &str) -> Option<&str> {
    if let Some(b) = git_ref.strip_prefix("refs/heads/") {
        return Some(b);
    }
    if git_ref.is_empty() || git_ref.starts_with("refs/") {
        return None;
    }
    Some(git_ref)
}

/// Coerce a branch name into a valid docker tag (`[A-Za-z0-9_.-]`,
/// ≤128 chars). Same rule as `registry::sanitize_tag`.
fn sanitize_tag(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    out.truncate(128);
    if out.is_empty() {
        out.push_str("latest");
    }
    out
}

/// One architecture's built `docker` image archive, going into the
/// multi-arch index.
#[derive(Debug, Clone)]
pub struct ArchSlice {
    /// Nix system tuple, e.g. `aarch64-linux`.
    pub system: String,
    /// The `docker-archive` store path the build produced.
    pub archive: String,
    /// CycloneDX SBOM bytes for this arch's image. When present, the
    /// fan-in attaches it as a referrer of this arch's manifest
    /// digest, so `oras discover <ref>@<digest>` finds a per-platform
    /// SBOM. `None` ⇒ no per-arch SBOM (generation failed upstream).
    pub sbom: Option<Vec<u8>>,
}

/// A registry to assemble a multi-arch index on — the same coordinates
/// a `RegistryPush` carries.
#[derive(Debug, Clone)]
pub struct MultiArchTarget {
    /// Configured registry name (the `registries:` key).
    pub target: String,
    /// Registry host[:port], no scheme.
    pub registry_url: String,
    /// Namespace; may contain a `{slug}` placeholder.
    pub namespace: String,
    /// File holding one `user:password` line. `None` ⇒ anonymous.
    pub auth_path: Option<PathBuf>,
    /// Plain-HTTP registry.
    pub insecure: bool,
}

impl MultiArchTarget {
    /// `docker://<url>/<namespace>/<image>` — the dest minus the tag,
    /// with `{slug}` resolved against `repo_slug`.
    fn base(&self, repo_slug: &str, image: &str) -> String {
        format!(
            "{}/{}/{}",
            self.registry_url.trim_end_matches('/'),
            self.namespace
                .replace("{slug}", repo_slug)
                .trim_matches('/'),
            image,
        )
    }

    /// Push every arch slice as an OCI manifest, then create a
    /// multi-arch OCI index under each tag. Returns a one-line summary
    /// on success.
    pub async fn assemble(
        &self,
        repo_slug: &str,
        image: &str,
        short_sha: &str,
        slices: &[ArchSlice],
        tags: &[String],
    ) -> Result<String, String> {
        let base = self.base(repo_slug, image);
        let creds = match &self.auth_path {
            Some(path) => match tokio::fs::read_to_string(path).await {
                Ok(s) => Some(s.trim().to_string()),
                Err(e) => {
                    return Err(format!(
                        "reading registry credentials {}: {e}",
                        path.display(),
                    ));
                }
            },
            None => None,
        };

        // Step 1: push each arch slice under an immutable, arch-tagged
        // reference, capturing the OCI manifest digest skopeo produced,
        // then attach that arch's SBOM as a referrer of the digest.
        let mut entries: Vec<(String, String)> = Vec::new();
        let mut sbom_count = 0usize;
        for slice in slices {
            let Some(arch) = oci_arch(&slice.system) else {
                tracing::warn!(system = %slice.system, "multi-arch: unmappable system; slice skipped");
                continue;
            };
            let arch_tag = format!("sha-{short_sha}-{arch}");
            let digest = self
                .push_slice(&base, &slice.archive, &arch_tag, creds.as_deref())
                .await?;

            // Per-arch SBOM: attach to the manifest digest, not a tag,
            // so it stays bound to exactly this platform's image. A
            // failed attach is logged, not fatal — the index still
            // assembles. (Mirrors `SbomAttach`'s `Advisory`-on-failure
            // stance: a missing SBOM is not a broken image.)
            if let Some(sbom) = &slice.sbom {
                let hint = format!("argunix-sbom-{image}-{short_sha}-{arch}");
                let aref = format!("{base}@{digest}");
                match crate::sbom::attach_cyclonedx_referrer(
                    &aref,
                    sbom,
                    &hint,
                    self.insecure,
                    creds.as_deref(),
                )
                .await
                {
                    Ok(()) => sbom_count += 1,
                    Err(e) => tracing::warn!(
                        system = %slice.system,
                        error = %e,
                        "multi-arch: per-arch SBOM attach failed",
                    ),
                }
            }
            entries.push((arch.to_string(), digest));
        }
        if entries.is_empty() {
            return Err("no arch slices could be pushed".into());
        }

        // Step 2: assemble the index under every public tag.
        for tag in tags {
            self.create_index(&base, tag, &entries, creds.as_deref())
                .await?;
        }

        let arches: Vec<&str> = entries.iter().map(|(a, _)| a.as_str()).collect();
        let sbom_note = if sbom_count > 0 {
            format!(
                ", {sbom_count} per-arch SBOM{}",
                if sbom_count == 1 { "" } else { "s" },
            )
        } else {
            String::new()
        };
        Ok(format!(
            "assembled {}-arch index ({}{sbom_note}) → {base}:{}",
            entries.len(),
            arches.join("+"),
            tags.join(", "),
        ))
    }

    /// `skopeo copy --format oci docker-archive:<slice> docker://…` —
    /// converts the docker archive to an OCI manifest on the registry,
    /// returning the digest skopeo wrote to `--digestfile`.
    async fn push_slice(
        &self,
        base: &str,
        archive: &str,
        arch_tag: &str,
        creds: Option<&str>,
    ) -> Result<String, String> {
        let digestfile = std::env::temp_dir().join(format!("argunix-ma-{arch_tag}.digest"));
        let mut cmd = Command::new("skopeo");
        cmd.arg("--insecure-policy")
            .arg("copy")
            .arg("--format")
            .arg("oci")
            .arg("--digestfile")
            .arg(&digestfile);
        if self.insecure {
            cmd.arg("--dest-tls-verify=false");
        }
        if let Some(creds) = creds {
            cmd.arg("--dest-creds").arg(creds);
        }
        cmd.arg(format!("docker-archive:{archive}"))
            .arg(format!("docker://{base}:{arch_tag}"));

        run_capture(cmd, "skopeo copy", COPY_TIMEOUT).await?;
        let digest = tokio::fs::read_to_string(&digestfile)
            .await
            .map_err(|e| format!("reading skopeo digestfile: {e}"))?
            .trim()
            .to_string();
        let _ = tokio::fs::remove_file(&digestfile).await;
        if digest.is_empty() {
            return Err("skopeo produced an empty digest".into());
        }
        Ok(digest)
    }

    /// `oras manifest index create <base>:<tag> <base>@<digest>…` —
    /// the multi-arch OCI index referencing the per-arch manifests.
    async fn create_index(
        &self,
        base: &str,
        tag: &str,
        entries: &[(String, String)],
        creds: Option<&str>,
    ) -> Result<(), String> {
        let mut cmd = Command::new("oras");
        cmd.arg("manifest").arg("index").arg("create");
        // `oras` may touch `$HOME` — point it at a writable dir.
        cmd.env("HOME", std::env::temp_dir());
        if self.insecure {
            cmd.arg("--plain-http");
        }
        if let Some(creds) = creds {
            let Some((user, pass)) = creds.split_once(':') else {
                return Err("credentials file is not in `user:password` form".into());
            };
            cmd.arg("--username").arg(user).arg("--password").arg(pass);
        }
        cmd.arg(format!("{base}:{tag}"));
        for (_arch, digest) in entries {
            cmd.arg(format!("{base}@{digest}"));
        }
        run_capture(cmd, "oras manifest index create", INDEX_TIMEOUT)
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oci_arch_maps_known_systems() {
        assert_eq!(oci_arch("x86_64-linux"), Some("amd64"));
        assert_eq!(oci_arch("aarch64-linux"), Some("arm64"));
        assert_eq!(oci_arch("sparc-linux"), None);
    }

    #[test]
    fn image_tags_default_branch_gets_latest() {
        let tags = image_tags("refs/heads/main", "0123456789ab", Some("main"));
        assert_eq!(tags, vec!["main", "latest", "sha-0123456789ab"]);
    }

    #[test]
    fn image_tags_feature_branch_has_no_latest() {
        let tags = image_tags("refs/heads/feature", "0123456789ab", Some("main"));
        assert_eq!(tags, vec!["feature", "sha-0123456789ab"]);
    }

    #[test]
    fn image_tags_bare_ref_and_pr_ref() {
        // The daemon stores a push eval's ref as a bare branch name.
        assert_eq!(
            image_tags("main", "0123456789ab", Some("main")),
            vec!["main", "latest", "sha-0123456789ab"],
        );
        // A PR ref is not a branch — only the sha tag.
        assert_eq!(
            image_tags("refs/pull/7/head", "0123456789ab", Some("main")),
            vec!["sha-0123456789ab"],
        );
    }

    #[test]
    fn base_resolves_slug_placeholder() {
        let t = MultiArchTarget {
            target: "ghcr".into(),
            registry_url: "ghcr.io".into(),
            namespace: "{slug}".into(),
            auth_path: None,
            insecure: false,
        };
        assert_eq!(t.base("org/repo", "hello"), "ghcr.io/org/repo/hello");
    }
}
