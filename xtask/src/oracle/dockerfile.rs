//! The apache image's `PHP_VERSION` held to the pin (#3533):
//! `dist/Dockerfile` states the version php's image sets as the default of
//! its `ARG PHP_VERSION`, a second statement of `oracle.toml`'s, so a pin
//! moved without it fails here, at the commit that moves it, rather than
//! at a release's image build, which checks it against the built `php`.

/// The Dockerfile at `path` names, as the default of its one
/// `ARG PHP_VERSION`, the pinned `version`; otherwise what it names, or
/// that it names none or several.
fn holds(path: &std::path::Path, version: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let named: Vec<&str> = (text.lines())
        .filter_map(|line| line.trim().strip_prefix("ARG PHP_VERSION="))
        .collect();
    match named[..] {
        [one] if one == version => Ok(()),
        [one] => Err(format!(
            "{}'s ARG PHP_VERSION is {one}, and oracle.toml pins php {version}: the image's version moves with the oracle (#3533)",
            path.display()
        )),
        _ => Err(format!(
            "{} names {} `ARG PHP_VERSION=` lines, where the apache stage names one, the pinned php's (#3533)",
            path.display(),
            named.len()
        )),
    }
}

mod tests {
    use super::holds;

    /// The fixture's Dockerfile names the version the preview image had
    /// drifted to, 8.5.9, which the hold holds to a pin of that version
    /// and fails against any other, naming both. No version here is the
    /// pin's, which no source file states.
    #[test]
    fn a_dockerfile_naming_another_version_fails_naming_both() {
        let fixture = crate::package_dir().join("fixtures/oracle/dockerfile-drift/Dockerfile");
        let why = holds(&fixture, "9.0.0").unwrap_err();
        assert!(
            why.contains("ARG PHP_VERSION is 8.5.9, and oracle.toml pins php 9.0.0"),
            "{why}"
        );
        assert_eq!(holds(&fixture, "8.5.9"), Ok(()));
    }

    /// `dist/Dockerfile`'s apache stage names the version `oracle.toml`
    /// pins.
    #[cfg(tracked)]
    #[test]
    fn the_apache_image_s_php_version_is_the_pinned_php_s() {
        let root = crate::package_dir().parent().unwrap().to_path_buf();
        let manifest = crate::manifest::Manifest::load(&root).unwrap();
        let version = &manifest.pin().unwrap().oracle.version;
        assert_eq!(holds(&root.join("dist/Dockerfile"), version), Ok(()));
    }
}
