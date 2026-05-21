//! Multi-arch image grouping — the daemon half of the cross-system
//! fan-in.
//!
//! `design/multi-arch.md`. When a flake exposes a `docker` image once
//! per architecture, those per-system jobs are stitched — after they
//! have all built — into one multi-arch OCI index on the registry.
//! This module classifies an eval's image jobs into groups and
//! resolves the registry targets; `worker::run_multiarch_fan_in` runs
//! the assembly itself.

use std::collections::{BTreeMap, HashMap, HashSet};

use argunix_config::Config;
use argunix_domain::{EvalId, ImageFormat, JobId, JobStatus};
use argunix_effects::{ArchSlice, MultiArchTarget, image_segment};
use argunix_eval::JobSpec;
use argunix_store::{EffectRunStore, JobRecord, JobStore, SqlxStore};
use chrono::Utc;

/// Job ids whose per-job `registry-push` must be suppressed: every
/// image job that shares its logical name with another image job in
/// the same eval. Either it forms a multi-arch group (the fan-in
/// pushes the index) or an `oci` clash (the fan-in refuses) — either
/// way the per-job push must not run and race the shared tags.
///
/// Computed from the job *specs* alone, before the build phase.
pub fn suppressed_push_job_ids(specs: &HashMap<JobId, JobSpec>) -> HashSet<JobId> {
    let mut by_name: BTreeMap<String, Vec<JobId>> = BTreeMap::new();
    for (job_id, spec) in specs {
        if spec.image_format.is_some() {
            by_name
                .entry(image_segment(spec.attr_path.as_str()))
                .or_default()
                .push(*job_id);
        }
    }
    by_name
        .into_values()
        .filter(|ids| ids.len() >= 2)
        .flatten()
        .collect()
}

/// A successfully-built arch member, before the fan-in generates its
/// SBOM. `classify` is synchronous; per-arch SBOM generation is async,
/// so [`run_fan_in`] turns each `PendingSlice` into an [`ArchSlice`].
pub struct PendingSlice {
    /// The per-arch job that produced this slice. The fan-in records a
    /// `registry-index` effect_run against it, so the job's own page
    /// shows the assembly it was part of.
    pub job_id: JobId,
    /// Nix system tuple, e.g. `aarch64-linux`.
    pub system: String,
    /// The `docker-archive` store path the build produced.
    pub archive: String,
    /// Attr path — the SBOM document's component name derives from it.
    pub attr_path: String,
    /// `meta.sbom-runtime-roots`, if the flake declared them — else the
    /// SBOM is transcribed from the image's layers.
    pub sbom_runtime_roots: Vec<String>,
}

/// One classified image group of an eval.
pub enum ImageGroup {
    /// ≥2 per-arch `docker` jobs of one logical name → assemble + push
    /// a multi-arch OCI index.
    MultiArch {
        /// Logical image name (the registry image segment).
        name: String,
        /// Per-arch slices whose member job built successfully — each
        /// carries its own job id (the fan-in records a per-slice
        /// `registry-index` effect_run).
        slices: Vec<PendingSlice>,
        /// Systems whose member job did *not* build — named in the
        /// outcome detail when the index is assembled partial.
        missing: Vec<String>,
    },
    /// ≥2 image jobs of one logical name with an `oci` among them —
    /// ambiguous (is each a slice, or a complete image?); refuse.
    Clash {
        name: String,
        anchor: JobId,
        detail: String,
    },
}

/// Group the eval's image jobs by logical name and classify each group
/// with ≥2 members. Lone images (one job) yield nothing — the per-job
/// `registry-push` already handled them.
pub fn classify(specs: &HashMap<JobId, JobSpec>, records: &[JobRecord]) -> Vec<ImageGroup> {
    let rec: BTreeMap<JobId, &JobRecord> = records.iter().map(|r| (r.id, r)).collect();

    let mut by_name: BTreeMap<String, Vec<(JobId, &JobSpec)>> = BTreeMap::new();
    for (job_id, spec) in specs {
        if spec.image_format.is_some() {
            by_name
                .entry(image_segment(spec.attr_path.as_str()))
                .or_default()
                .push((*job_id, spec));
        }
    }

    let mut out = Vec::new();
    for (name, members) in by_name {
        if members.len() < 2 {
            continue; // a lone image — nothing to fan in.
        }
        let oci = members
            .iter()
            .filter(|(_, s)| s.image_format == Some(ImageFormat::Oci))
            .count();

        if oci >= 1 {
            // Two or more same-name image jobs with any `oci` among
            // them — a complete `oci` image exposed across systems, or
            // `oci` mixed with `docker` slices. Either way ambiguous.
            let anchor = members.iter().map(|(j, _)| *j).min().expect("non-empty");
            out.push(ImageGroup::Clash {
                name,
                anchor,
                detail: format!(
                    "{} image jobs share this name across systems, {oci} of them `oci` — \
                     a complete multi-arch `oci` image needs a single system output, or \
                     expose per-arch `docker` outputs so argunix assembles the index",
                    members.len(),
                ),
            });
            continue;
        }

        // All `docker`, ≥2 systems — a multi-arch group.
        let mut slices = Vec::new();
        let mut missing = Vec::new();
        for (job_id, spec) in &members {
            let system = spec.system.clone().unwrap_or_else(|| "unknown".to_string());
            match rec.get(job_id) {
                Some(r)
                    if matches!(r.status, JobStatus::Success | JobStatus::Cached)
                        && r.output_path.is_some() =>
                {
                    slices.push(PendingSlice {
                        job_id: *job_id,
                        system,
                        archive: r.output_path.clone().expect("checked"),
                        attr_path: spec.attr_path.as_str().to_string(),
                        sbom_runtime_roots: argunix_effects::sbom::runtime_roots(&spec.meta),
                    });
                }
                _ => missing.push(system),
            }
        }
        out.push(ImageGroup::MultiArch {
            name,
            slices,
            missing,
        });
    }
    out
}

/// Resolve the `MultiArchTarget`s a repo's images should be assembled
/// onto — its effective `push_to_registries` against the global
/// `registries` catalog. Mirrors `effects::registry_push_effects`.
pub fn multiarch_targets(
    config: &Config,
    repo_forge: &str,
    repo_slug: &str,
) -> Vec<MultiArchTarget> {
    let Some(repo) = config
        .repos
        .iter()
        .find(|r| r.forge == repo_forge && r.slug.as_str() == repo_slug)
    else {
        return Vec::new();
    };
    repo.push_to_registries
        .iter()
        .filter_map(|name| {
            config.registries.get(name).map(|reg| MultiArchTarget {
                target: name.clone(),
                registry_url: reg.url.clone(),
                namespace: reg.namespace.clone(),
                auth_path: reg.auth_path.as_ref().map(|p| p.path().to_path_buf()),
                insecure: reg.insecure,
            })
        })
        .collect()
}

/// The cross-system multi-arch fan-in. Classifies the eval's image
/// jobs, and for each multi-arch `docker` group assembles + pushes a
/// multi-arch OCI index on every bound registry; for an `oci` clash
/// records an errored effect. Each outcome is recorded as a
/// `registry-index` row in `effect_runs` (visible in the argunix UI).
/// Distribution is best-effort and intentionally not surfaced as a
/// forge check — a failed index push is not a property of the repo's
/// commit. Shared by both build paths.
pub async fn run_fan_in(
    store: &SqlxStore,
    eval_id: EvalId,
    specs_by_id: &HashMap<JobId, JobSpec>,
    config: &Config,
    repo_forge: &str,
    repo_slug: &str,
    default_branch: Option<&str>,
    git_ref: &str,
    sha: &str,
) {
    let records = match <SqlxStore as JobStore>::list_by_eval(store, eval_id).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "multi-arch: list_by_eval failed; fan-in skipped");
            return;
        }
    };
    let groups = classify(specs_by_id, &records);
    if groups.is_empty() {
        return;
    }
    let targets = multiarch_targets(config, repo_forge, repo_slug);
    let short_sha = sha.get(..12).unwrap_or(sha);
    let tags = argunix_effects::multiarch::image_tags(git_ref, short_sha, default_branch);

    for group in groups {
        match group {
            ImageGroup::MultiArch {
                name,
                slices,
                missing,
            } => {
                if slices.is_empty() {
                    tracing::warn!(image = %name, "multi-arch: no arch built; nothing to assemble");
                    continue;
                }
                // Generate each arch's SBOM once, ahead of the
                // per-registry assembly loop — the bytes are
                // deterministic and registry-independent. The fan-in
                // attaches each to its per-arch manifest digest; the
                // per-job `record_image_artifacts` stores the same
                // document in the DB. A generation failure is logged
                // and that arch's index entry simply carries no SBOM.
                let mut arch_slices: Vec<ArchSlice> = Vec::with_capacity(slices.len());
                for ps in &slices {
                    let sbom = match argunix_effects::sbom::generate_sbom(
                        &ps.attr_path,
                        std::slice::from_ref(&ps.archive),
                        &ps.sbom_runtime_roots,
                    )
                    .await
                    {
                        Ok((bytes, n)) => {
                            tracing::info!(
                                image = %name,
                                system = %ps.system,
                                components = n,
                                "multi-arch: per-arch SBOM generated",
                            );
                            Some(bytes)
                        }
                        Err(e) => {
                            tracing::warn!(
                                image = %name,
                                system = %ps.system,
                                error = %e,
                                "multi-arch: per-arch SBOM generation failed",
                            );
                            None
                        }
                    };
                    arch_slices.push(ArchSlice {
                        system: ps.system.clone(),
                        archive: ps.archive.clone(),
                        sbom,
                    });
                }
                for target in &targets {
                    // One `registry-index` effect_run per slice job, so
                    // every per-arch job page shows the assembly it was
                    // part of — not just the lowest-id member.
                    let mut runs = Vec::with_capacity(slices.len());
                    for ps in &slices {
                        runs.push(
                            <SqlxStore as EffectRunStore>::create_effect_run(
                                store,
                                ps.job_id,
                                "registry-index",
                                &target.target,
                                Utc::now(),
                            )
                            .await
                            .ok(),
                        );
                    }
                    let result = target
                        .assemble(repo_slug, &name, short_sha, &arch_slices, &tags)
                        .await;
                    let (status, detail) = match &result {
                        Ok(summary) => {
                            let detail = if missing.is_empty() {
                                summary.clone()
                            } else {
                                format!("{summary} (missing arch: {})", missing.join(", "))
                            };
                            ("success", detail)
                        }
                        Err(e) => ("failure", e.clone()),
                    };
                    for run_id in runs.into_iter().flatten() {
                        if let Err(e) = <SqlxStore as EffectRunStore>::finish_effect_run(
                            store,
                            run_id,
                            status,
                            Some(&detail),
                            Utc::now(),
                        )
                        .await
                        {
                            tracing::warn!(error = %e, "multi-arch: effect_runs finish failed");
                        }
                    }
                    tracing::info!(
                        image = %name,
                        registry = %target.target,
                        detail = %detail,
                        "multi-arch fan-in",
                    );
                }
            }
            ImageGroup::Clash {
                name,
                anchor,
                detail,
            } => {
                let run_id = <SqlxStore as EffectRunStore>::create_effect_run(
                    store,
                    anchor,
                    "registry-index",
                    &name,
                    Utc::now(),
                )
                .await
                .ok();
                if let Some(id) = run_id {
                    let _ = <SqlxStore as EffectRunStore>::finish_effect_run(
                        store,
                        id,
                        "failure",
                        Some(&detail),
                        Utc::now(),
                    )
                    .await;
                }
                tracing::warn!(image = %name, detail = %detail, "multi-arch: oci clash");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use argunix_domain::AttrPath;
    use argunix_store::JobPhaseMetrics;

    /// A `JobSpec` built through the real `nix-eval-jobs` line parser
    /// — avoids hand-constructing every field. `leaf` is the bare attr
    /// name; `parse_lines` prepends the `packages.<system>` prefix, so
    /// `attr_path` ends up `packages.<system>.<leaf>`, exactly as in a
    /// real eval.
    fn spec(leaf: &str, system: &str, format: Option<&str>) -> JobSpec {
        let meta = match format {
            Some(f) => format!(r#","meta":{{"image-format":"{f}"}}"#),
            None => String::new(),
        };
        let line = format!(
            r#"{{"attr":"{leaf}","drvPath":"/nix/store/x-{system}.drv","system":"{system}"{meta}}}"#
        );
        argunix_eval::parse_lines(&format!("packages.{system}"), &line)
            .expect("valid job line")
            .pop()
            .expect("one job")
    }

    fn specs(jobs: &[(i64, &str, &str, Option<&str>)]) -> HashMap<JobId, JobSpec> {
        jobs.iter()
            .map(|(id, leaf, system, format)| (JobId::new(*id), spec(leaf, system, *format)))
            .collect()
    }

    fn record(id: i64, status: JobStatus, output: Option<&str>) -> JobRecord {
        JobRecord {
            id: JobId::new(id),
            eval_id: EvalId::new(1),
            attr_path: AttrPath::new("x"),
            drv_path: None,
            system: "x86_64-linux".to_string(),
            started_at: None,
            finished_at: None,
            status,
            log_path: None,
            output_path: output.map(String::from),
            builder_id: None,
            interrupt_count: 0,
            failure_reason: None,
            phase_metrics: JobPhaseMetrics::default(),
            main_program: None,
            outputs: Default::default(),
            image_size_bytes: None,
        }
    }

    #[test]
    fn two_docker_systems_form_a_multiarch_group() {
        let s = specs(&[
            (1, "app", "x86_64-linux", Some("docker")),
            (2, "app", "aarch64-linux", Some("docker")),
        ]);
        let recs = vec![
            record(1, JobStatus::Success, Some("/nix/store/a-amd64.tar")),
            record(2, JobStatus::Success, Some("/nix/store/a-arm64.tar")),
        ];
        let groups = classify(&s, &recs);
        assert_eq!(groups.len(), 1);
        match &groups[0] {
            ImageGroup::MultiArch {
                name,
                slices,
                missing,
            } => {
                assert_eq!(name, "app");
                assert_eq!(slices.len(), 2);
                // each slice carries the job id of its per-arch build
                let ids: Vec<_> = slices.iter().map(|s| s.job_id).collect();
                assert!(ids.contains(&JobId::new(1)) && ids.contains(&JobId::new(2)));
                assert!(missing.is_empty());
            }
            ImageGroup::Clash { .. } => panic!("expected a multi-arch group, got a clash"),
        }
    }

    #[test]
    fn a_failed_arch_is_recorded_as_missing() {
        let s = specs(&[
            (1, "app", "x86_64-linux", Some("docker")),
            (2, "app", "aarch64-linux", Some("docker")),
        ]);
        let recs = vec![
            record(1, JobStatus::Success, Some("/nix/store/a-amd64.tar")),
            record(2, JobStatus::Failure, None),
        ];
        let groups = classify(&s, &recs);
        match &groups[0] {
            ImageGroup::MultiArch {
                slices, missing, ..
            } => {
                assert_eq!(slices.len(), 1);
                assert_eq!(missing, &["aarch64-linux"]);
            }
            ImageGroup::Clash { .. } => panic!("expected a partial multi-arch group"),
        }
    }

    #[test]
    fn two_oci_systems_are_a_clash() {
        let s = specs(&[
            (1, "app", "x86_64-linux", Some("oci")),
            (2, "app", "aarch64-linux", Some("oci")),
        ]);
        let recs = vec![
            record(1, JobStatus::Success, Some("/nix/store/a.tar")),
            record(2, JobStatus::Success, Some("/nix/store/b.tar")),
        ];
        let groups = classify(&s, &recs);
        assert_eq!(groups.len(), 1);
        assert!(matches!(groups[0], ImageGroup::Clash { .. }));
    }

    #[test]
    fn a_lone_image_is_not_a_group() {
        let s = specs(&[(1, "app", "x86_64-linux", Some("oci"))]);
        let recs = vec![record(1, JobStatus::Success, Some("/nix/store/a.tar"))];
        assert!(classify(&s, &recs).is_empty());
    }

    #[test]
    fn suppressed_ids_cover_every_grouped_image_job() {
        let s = specs(&[
            (1, "app", "x86_64-linux", Some("docker")),
            (2, "app", "aarch64-linux", Some("docker")),
            // a lone image and a non-image job — neither suppressed.
            (3, "solo", "x86_64-linux", Some("docker")),
            (4, "lib", "x86_64-linux", None),
        ]);
        let suppressed = suppressed_push_job_ids(&s);
        assert!(suppressed.contains(&JobId::new(1)));
        assert!(suppressed.contains(&JobId::new(2)));
        assert!(!suppressed.contains(&JobId::new(3)));
        assert!(!suppressed.contains(&JobId::new(4)));
    }
}
