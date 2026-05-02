use std::path::PathBuf;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ResolveError {
    #[error("empty environment variable name in path expansion")]
    EmptyVarName,
    #[error("unterminated `${{...}}` in path")]
    UnterminatedBrace,
    #[error("environment variable `{0}` is not set")]
    UndefinedVar(String),
}

/// Substitute `$NAME` and `${NAME}` segments in `s` using the process
/// environment, returning the resulting path.
///
/// We deliberately do not try to be a shell — only `$VAR` and `${VAR}`
/// are expanded; tildes, command substitution, etc. are passed through.
pub fn resolve_path(s: &str) -> Result<PathBuf, ResolveError> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let braced = chars.peek() == Some(&'{');
        if braced {
            chars.next();
        }
        let mut name = String::new();
        loop {
            match chars.peek().copied() {
                None => {
                    if braced {
                        return Err(ResolveError::UnterminatedBrace);
                    }
                    break;
                }
                Some('}') if braced => {
                    chars.next();
                    break;
                }
                Some(c) if !braced && !is_var_char(c) => break,
                Some(c) => {
                    name.push(c);
                    chars.next();
                }
            }
        }
        if name.is_empty() {
            return Err(ResolveError::EmptyVarName);
        }
        let value = std::env::var(&name).map_err(|_| ResolveError::UndefinedVar(name))?;
        out.push_str(&value);
    }
    Ok(PathBuf::from(out))
}

fn is_var_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_env<T>(key: &str, value: &str, f: impl FnOnce() -> T) -> T {
        let prev = std::env::var(key).ok();
        // SAFETY: tests run single-threaded by setting CARGO_TEST_THREADS=1, and
        // we restore the previous value below. We only set keys unique to a test.
        unsafe {
            std::env::set_var(key, value);
        }
        let result = f();
        unsafe {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        result
    }

    #[test]
    fn passes_through_plain() {
        assert_eq!(resolve_path("/etc/foo").unwrap(), PathBuf::from("/etc/foo"));
    }

    #[test]
    fn expands_dollar_var() {
        with_env(
            "MEDUSA_TEST_DOLLAR",
            "/run/credentials/medusa.service",
            || {
                assert_eq!(
                    resolve_path("$MEDUSA_TEST_DOLLAR/token").unwrap(),
                    PathBuf::from("/run/credentials/medusa.service/token"),
                );
            },
        );
    }

    #[test]
    fn expands_braced_var() {
        with_env("MEDUSA_TEST_BRACED", "abc", || {
            assert_eq!(
                resolve_path("/x/${MEDUSA_TEST_BRACED}/y").unwrap(),
                PathBuf::from("/x/abc/y"),
            );
        });
    }

    #[test]
    fn errors_on_undefined_var() {
        // pick a name unlikely to be set
        assert!(matches!(
            resolve_path("/a/$MEDUSA_DEFINITELY_NOT_SET_xyzzy/b"),
            Err(ResolveError::UndefinedVar(_))
        ));
    }

    #[test]
    fn errors_on_empty_var_name() {
        assert_eq!(resolve_path("a$/b"), Err(ResolveError::EmptyVarName));
    }

    #[test]
    fn errors_on_unterminated_brace() {
        assert_eq!(
            resolve_path("a/${UNCLOSED"),
            Err(ResolveError::UnterminatedBrace),
        );
    }
}
