use argunix_domain::{AttrPath, ImageFormat};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One JSON-lines record from `nix-eval-jobs` stdout. Mirror of the schema
/// the tool emits — kept loose because nix-eval-jobs has historically added
/// optional fields between releases.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawJob {
    /// The attribute name within the requested fragment, dot-joined for
    /// nested attrsets. We *prefix* this with the fragment we asked for to
    /// build the full argunix attr path.
    pub attr: String,
    pub drv_path: Option<String>,
    pub system: Option<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub outputs: BTreeMap<String, String>,
    #[serde(default)]
    pub meta: serde_json::Value,
    #[serde(default)]
    pub is_cached: bool,
    /// Mirrors `requiredSystemFeatures` from the .drv. nix-eval-jobs
    /// emits this per-attr when the derivation declares it. Used by
    /// the worker's pre-flight to fail-fast when no connected builder
    /// can satisfy the features (otherwise nix's remote-build
    /// scheduler retries internally for a long time before printing
    /// "Failed to find a machine").
    #[serde(default)]
    pub required_system_features: Vec<String>,
}

/// A argunix-side job spec produced by combining a [`RawJob`] with the
/// fragment prefix it was discovered under.
#[derive(Debug, Clone, Serialize)]
pub struct JobSpec {
    pub attr_path: AttrPath,
    pub drv_path: Option<String>,
    pub system: Option<String>,
    pub error: Option<String>,
    /// Output-name → store path map, e.g. `{"out": "/nix/store/zzz-foo"}`.
    /// Pre-computed by `nix-eval-jobs` so we can do cache-skip without
    /// shelling out to `nix-store --query --outputs` separately.
    pub outputs: BTreeMap<String, String>,
    pub meta: serde_json::Value,
    pub is_cached: bool,
    /// `requiredSystemFeatures` from the .drv (e.g. `["cuda"]`,
    /// `["uid-range"]`). Empty when the derivation didn't declare any.
    pub required_system_features: Vec<String>,
    /// `Some` when the derivation declared `meta.image-format`, marking
    /// the build output as a container image argunix should publish
    /// after a successful build; the variant records the archive
    /// format. `None` for an ordinary package.
    pub image_format: Option<ImageFormat>,
}

impl JobSpec {
    /// The primary output path (`outputs["out"]` if present, else the first
    /// output, else `None`). Used by cache-skip and log-naming.
    pub fn primary_output(&self) -> Option<&str> {
        self.outputs
            .get("out")
            .or_else(|| self.outputs.values().next())
            .map(String::as_str)
    }
}

/// Read the `meta.image-format` marker. A missing attribute means an
/// ordinary package (`None`); an attribute carrying an unrecognised
/// value is logged and treated as `None` rather than failing the whole
/// evaluation — one bad `meta` field shouldn't sink every other job in
/// the same flake.
fn meta_image_format(attr: &str, meta: &serde_json::Value) -> Option<ImageFormat> {
    let raw = meta.get("image-format")?;
    let Some(s) = raw.as_str() else {
        tracing::warn!(attr, ?raw, "meta.image-format is not a string; ignoring");
        return None;
    };
    match s.parse() {
        Ok(fmt) => Some(fmt),
        Err(e) => {
            tracing::warn!(attr, %e, "ignoring unrecognised meta.image-format value");
            None
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("malformed JSON line {line}: {source}")]
    Json {
        line: usize,
        #[source]
        source: serde_json::Error,
    },
}

/// Parse the JSON-lines body produced by `nix-eval-jobs`, prepending
/// `prefix` (e.g. `packages.x86_64-linux`) to each record's `attr` to build
/// the full attribute path.
pub fn parse_lines(prefix: &str, body: &str) -> Result<Vec<JobSpec>, ParseError> {
    let mut out = Vec::new();
    for (idx, line) in body.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let raw: RawJob = serde_json::from_str(trimmed).map_err(|e| ParseError::Json {
            line: idx + 1,
            source: e,
        })?;
        let image_format = meta_image_format(&raw.attr, &raw.meta);
        out.push(JobSpec {
            attr_path: AttrPath::new(if raw.attr.is_empty() {
                prefix.to_string()
            } else {
                format!("{prefix}.{}", raw.attr)
            }),
            drv_path: raw.drv_path,
            system: raw.system,
            error: raw.error,
            outputs: raw.outputs,
            meta: raw.meta,
            is_cached: raw.is_cached,
            required_system_features: raw.required_system_features,
            image_format,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_success_line() {
        let body =
            r#"{"attr":"hello","drvPath":"/nix/store/xxx-hello.drv","system":"x86_64-linux"}"#;
        let jobs = parse_lines("packages.x86_64-linux", body).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].attr_path.as_str(), "packages.x86_64-linux.hello");
        assert_eq!(
            jobs[0].drv_path.as_deref(),
            Some("/nix/store/xxx-hello.drv")
        );
        assert_eq!(jobs[0].system.as_deref(), Some("x86_64-linux"));
        assert!(jobs[0].error.is_none());
    }

    #[test]
    fn parses_error_line() {
        let body = r#"{"attr":"broken","error":"evaluation aborted","system":"x86_64-linux"}"#;
        let jobs = parse_lines("checks.x86_64-linux", body).unwrap();
        assert_eq!(jobs[0].attr_path.as_str(), "checks.x86_64-linux.broken");
        assert!(jobs[0].drv_path.is_none());
        assert_eq!(jobs[0].error.as_deref(), Some("evaluation aborted"));
    }

    #[test]
    fn parses_multiple_lines_with_blanks() {
        let body = "\n\
            {\"attr\":\"a\",\"drvPath\":\"/nix/store/a.drv\",\"system\":\"x86_64-linux\"}\n\
            \n\
            {\"attr\":\"b\",\"drvPath\":\"/nix/store/b.drv\",\"system\":\"x86_64-linux\"}\n";
        let jobs = parse_lines("packages.x86_64-linux", body).unwrap();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].attr_path.as_str(), "packages.x86_64-linux.a");
        assert_eq!(jobs[1].attr_path.as_str(), "packages.x86_64-linux.b");
    }

    #[test]
    fn handles_nested_attr() {
        // nix-eval-jobs emits dot-joined attr names for nested attrsets when
        // we point it at a parent attrset.
        let body = r#"{"attr":"suite.test1","drvPath":"/nix/store/x.drv","system":"x86_64-linux"}"#;
        let jobs = parse_lines("checks.x86_64-linux", body).unwrap();
        assert_eq!(
            jobs[0].attr_path.as_str(),
            "checks.x86_64-linux.suite.test1"
        );
    }

    #[test]
    fn invalid_json_reports_line_number() {
        let body = "{\"attr\":\"ok\",\"drvPath\":\"/nix/store/a.drv\"}\nthis-is-not-json\n";
        let err = parse_lines("packages.x86_64-linux", body).unwrap_err();
        match err {
            ParseError::Json { line, .. } => assert_eq!(line, 2),
        }
    }

    #[test]
    fn keeps_meta_passthrough() {
        let body = r#"{"attr":"hello","drvPath":"/nix/store/x.drv","system":"x86_64-linux","meta":{"description":"hi","platforms":["x86_64-linux"]}}"#;
        let jobs = parse_lines("packages.x86_64-linux", body).unwrap();
        assert_eq!(jobs[0].meta["description"], "hi");
    }

    #[test]
    fn parses_outputs_map() {
        let body = r#"{"attr":"hello","drvPath":"/nix/store/x.drv","system":"x86_64-linux","outputs":{"out":"/nix/store/zzz-hello","dev":"/nix/store/yyy-hello-dev"}}"#;
        let jobs = parse_lines("packages.x86_64-linux", body).unwrap();
        assert_eq!(jobs[0].outputs.len(), 2);
        assert_eq!(jobs[0].primary_output(), Some("/nix/store/zzz-hello"));
    }

    #[test]
    fn primary_output_falls_back_to_first_when_no_out() {
        let body = r#"{"attr":"x","drvPath":"/nix/store/x.drv","system":"x86_64-linux","outputs":{"lib":"/nix/store/aaa-x-lib"}}"#;
        let jobs = parse_lines("packages.x86_64-linux", body).unwrap();
        assert_eq!(jobs[0].primary_output(), Some("/nix/store/aaa-x-lib"));
    }

    #[test]
    fn parses_required_system_features() {
        let body = r#"{"attr":"x","drvPath":"/nix/store/x.drv","system":"x86_64-linux","requiredSystemFeatures":["cuda","uid-range"]}"#;
        let jobs = parse_lines("packages.x86_64-linux", body).unwrap();
        assert_eq!(
            jobs[0].required_system_features,
            vec!["cuda".to_string(), "uid-range".to_string()],
        );
    }

    #[test]
    fn missing_required_system_features_is_empty_vec() {
        let body = r#"{"attr":"x","drvPath":"/nix/store/x.drv","system":"x86_64-linux"}"#;
        let jobs = parse_lines("packages.x86_64-linux", body).unwrap();
        assert!(jobs[0].required_system_features.is_empty());
    }

    #[test]
    fn detects_oci_image_format_meta() {
        let body = r#"{"attr":"img","drvPath":"/nix/store/x.drv","system":"x86_64-linux","meta":{"image-format":"oci"}}"#;
        let jobs = parse_lines("packages.x86_64-linux", body).unwrap();
        assert_eq!(jobs[0].image_format, Some(ImageFormat::Oci));
    }

    #[test]
    fn detects_docker_image_format_meta() {
        let body = r#"{"attr":"img","drvPath":"/nix/store/x.drv","system":"x86_64-linux","meta":{"image-format":"docker"}}"#;
        let jobs = parse_lines("packages.x86_64-linux", body).unwrap();
        assert_eq!(jobs[0].image_format, Some(ImageFormat::Docker));
    }

    #[test]
    fn missing_image_format_meta_is_none() {
        let body = r#"{"attr":"x","drvPath":"/nix/store/x.drv","system":"x86_64-linux","meta":{"description":"hi"}}"#;
        let jobs = parse_lines("packages.x86_64-linux", body).unwrap();
        assert!(jobs[0].image_format.is_none());
    }

    #[test]
    fn unrecognised_image_format_meta_is_none() {
        // A typo'd value is ignored, not fatal — the job is simply
        // treated as a non-image build.
        let body = r#"{"attr":"x","drvPath":"/nix/store/x.drv","system":"x86_64-linux","meta":{"image-format":"tarball"}}"#;
        let jobs = parse_lines("packages.x86_64-linux", body).unwrap();
        assert!(jobs[0].image_format.is_none());
    }

    #[test]
    fn primary_output_none_when_outputs_empty() {
        let body = r#"{"attr":"x","drvPath":"/nix/store/x.drv","system":"x86_64-linux"}"#;
        let jobs = parse_lines("packages.x86_64-linux", body).unwrap();
        assert!(jobs[0].primary_output().is_none());
    }
}
