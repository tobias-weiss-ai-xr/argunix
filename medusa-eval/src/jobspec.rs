use medusa_domain::AttrPath;
use serde::{Deserialize, Serialize};

/// One JSON-lines record from `nix-eval-jobs` stdout. Mirror of the schema
/// the tool emits — kept loose because nix-eval-jobs has historically added
/// optional fields between releases.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawJob {
    /// The attribute name within the requested fragment, dot-joined for
    /// nested attrsets. We *prefix* this with the fragment we asked for to
    /// build the full medusa attr path.
    pub attr: String,
    pub drv_path: Option<String>,
    pub system: Option<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub meta: serde_json::Value,
    #[serde(default)]
    pub is_cached: bool,
}

/// A medusa-side job spec produced by combining a [`RawJob`] with the
/// fragment prefix it was discovered under.
#[derive(Debug, Clone, Serialize)]
pub struct JobSpec {
    pub attr_path: AttrPath,
    pub drv_path: Option<String>,
    pub system: Option<String>,
    pub error: Option<String>,
    pub meta: serde_json::Value,
    pub is_cached: bool,
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
        out.push(JobSpec {
            attr_path: AttrPath::new(if raw.attr.is_empty() {
                prefix.to_string()
            } else {
                format!("{prefix}.{}", raw.attr)
            }),
            drv_path: raw.drv_path,
            system: raw.system,
            error: raw.error,
            meta: raw.meta,
            is_cached: raw.is_cached,
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
}
