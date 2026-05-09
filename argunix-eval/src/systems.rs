//! Detection of available builder systems.
//!
//! v1 only knows about the local system. Anything beyond the local
//! system must be supplied explicitly by the caller (e.g. via a
//! `--systems` CLI flag in tests). `/etc/nix/machines` parsing for
//! filtering is a future extension.

/// Best-effort detection of the local nix `<arch>-<os>` system tuple.
///
/// We use the same constants the nix daemon uses, e.g. `x86_64-linux`,
/// `aarch64-linux`. This is just `std::env::consts::ARCH` + `OS` mapped
/// through nix's naming convention.
pub fn detect_local_systems() -> Vec<String> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        // nix uses i686 / armv7l / etc; cover the common ones lazily as
        // they show up.
        other => other,
    };
    let os = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        other => other,
    };
    vec![format!("{arch}-{os}")]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_includes_a_recognisable_system() {
        let systems = detect_local_systems();
        assert!(!systems.is_empty());
        assert!(systems[0].contains('-'));
    }
}
