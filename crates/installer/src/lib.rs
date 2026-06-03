#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
//! Installer helpers. Pure version-string derivation (unit-tested);
//! the actual MSI/bundle build lives in `xtask package` + `wix/`.

/// Convert a Cargo SemVer string ("0.0.0", "1.2.3", "1.2.3-rc.1") into a
/// WiX/MSI 4-part numeric version "MAJOR.MINOR.PATCH.0". MSI ignores
/// pre-release tags and requires numeric fields, so they are stripped.
pub fn installer_version(cargo_version: &str) -> String {
    let core = cargo_version
        .split(['-', '+'])
        .next()
        .unwrap_or(cargo_version);
    let mut parts: Vec<&str> = core.split('.').collect();
    while parts.len() < 3 {
        parts.push("0");
    }
    let n = |s: &str| {
        s.chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
    };
    let major = n(parts.first().copied().unwrap_or("0"));
    let minor = n(parts.get(1).copied().unwrap_or("0"));
    let patch = n(parts.get(2).copied().unwrap_or("0"));
    let d = |s: String| if s.is_empty() { "0".to_string() } else { s };
    format!("{}.{}.{}.0", d(major), d(minor), d(patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_semver() {
        assert_eq!(installer_version("1.2.3"), "1.2.3.0");
    }

    #[test]
    fn zero_version() {
        assert_eq!(installer_version("0.0.0"), "0.0.0.0");
    }

    #[test]
    fn strips_prerelease_and_build() {
        assert_eq!(installer_version("1.4.0-rc.1"), "1.4.0.0");
        assert_eq!(installer_version("2.0.0+win"), "2.0.0.0");
    }

    #[test]
    fn pads_short_version() {
        assert_eq!(installer_version("1"), "1.0.0.0");
        assert_eq!(installer_version("1.5"), "1.5.0.0");
    }

    #[test]
    fn non_numeric_field_falls_back_to_zero() {
        assert_eq!(installer_version("x.y.z"), "0.0.0.0");
    }
}
