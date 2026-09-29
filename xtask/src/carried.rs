//! Which of the manifest's `feature` rows a build carries
//! (harness-design.md §7, revision 63; #1252).
//!
//! A row's `feature` says its extension is behind a feature of
//! `php_bundle`, outside php's default `./configure`; it does not say which
//! builds turn that feature on. Each building crate says so itself, in the
//! `features` of its `php_bundle` dependency, and this module reads the
//! declaration there and nowhere else, so no list in the harness is a
//! second copy of it. A row with no `feature` is the bundle's default set,
//! which every build carries.
//!
//! Two builds are read. The binary under test, [`BINARY`]'s crate, is the
//! one the per-commit phpt lane runs: the lane runs the subsets of every
//! row it carries, `done` asks a touched crate it carries for a `phpt`
//! reading, and `crosscheck` renders every extension it carries (§4.5
//! condition 1). The engine linked into this harness is the other,
//! declared by `xtask/Cargo.toml` as this build was compiled against it:
//! `crosscheck` serves an extension through it where it carries the
//! extension and through the binary where it does not, which is `pcntl`
//! (revision 61).

use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result};
use toml::Value;

use crate::manifest::Crate;

/// The crate that builds the binary under test, relative to the
/// workspace root: `sapi/cli`, php's CLI host, whose binary is `php`
/// (ADR-0008). The one place the harness names it.
pub const BINARY: &str = "sapi/cli";

/// The package every build turns its extension features on through.
const BUNDLE: &str = "php_bundle";

/// The `php_bundle` features one build turns on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Carried {
    features: BTreeSet<String>,
}

impl Carried {
    /// What the binary under test carries, read from [`BINARY`]'s
    /// `Cargo.toml` under `root`. A workspace that has not created the
    /// crate yet builds no binary to carry a feature, so it carries the
    /// default set alone; a `Cargo.toml` that declares no `php_bundle`
    /// dependency is refused, since that is a binary this module no longer
    /// knows how to read.
    pub fn binary(root: &Path) -> Result<Carried> {
        let path = root.join(BINARY).join("Cargo.toml");
        match fs::read_to_string(&path) {
            Ok(text) => Carried::declared(&text).with_context(|| format!("{}", path.display())),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Carried::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// What the engine linked into this harness carries: `xtask`'s own
    /// `Cargo.toml` as this build was compiled against it.
    pub(crate) fn linked() -> Result<Carried> {
        Carried::declared(include_str!("../Cargo.toml")).context("xtask/Cargo.toml")
    }

    /// The features a crate's `Cargo.toml` turns on in its `php_bundle`
    /// dependency.
    pub(crate) fn declared(text: &str) -> Result<Carried> {
        let table: toml::Table = toml::from_str(text).context("not TOML")?;
        let bundle = table
            .get("dependencies")
            .and_then(|deps| deps.get(BUNDLE))
            .with_context(|| format!("no `{BUNDLE}` dependency"))?;
        let features = bundle
            .get("features")
            .and_then(Value::as_array)
            .map(|on| {
                on.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Ok(Carried { features })
    }

    /// A build turning these features on, as a test states one.
    #[cfg(test)]
    pub(crate) fn turning_on(features: &[&str]) -> Carried {
        Carried {
            features: features.iter().map(|f| (*f).to_owned()).collect(),
        }
    }

    /// Whether the build carries the row's crate: a row with no `feature`
    /// always, one with a `feature` where the build turns it on.
    pub fn carries(&self, krate: &Crate) -> bool {
        krate
            .feature
            .as_ref()
            .is_none_or(|feature| self.features.contains(feature))
    }
}

#[cfg(test)]
mod tests {
    use super::{BINARY, Carried};
    use crate::manifest::{Crate, Manifest};
    use std::path::Path;

    fn workspace() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
    }

    #[test]
    fn a_build_carries_the_default_set_and_the_features_its_bundle_dependency_turns_on() {
        let carried = Carried::declared(
            "[dependencies]\nphp_bundle = { path = \"../php_bundle\", features = [\"openssl\", \"bz2\"] }\n",
        )
        .unwrap();
        assert!(carried.carries(&Crate::named("ext/ctype")));
        assert!(carried.carries(&Crate::named("ext/openssl").with_feature("openssl")));
        assert!(carried.carries(&Crate::named("ext/bz2").with_feature("bz2")));
        assert!(!carried.carries(&Crate::named("ext/curl").with_feature("curl")));
    }

    #[test]
    fn a_declaration_without_the_bundle_is_refused_and_an_absent_crate_carries_the_default_set() {
        let refused = Carried::declared("[dependencies]\nanyhow = \"1\"\n").unwrap_err();
        assert!(format!("{refused:#}").contains("php_bundle"), "{refused:#}");
        let bare = Carried::declared("[dependencies]\nphp_bundle = { path = \"x\" }\n").unwrap();
        assert_eq!(bare, Carried::default());
        let root = crate::scratch::Tree::new("carried-absent").unwrap();
        assert_eq!(Carried::binary(&root).unwrap(), Carried::default());
    }

    /// The binary's crate is where [`BINARY`] says, and every feature its
    /// declaration turns on is a manifest row's, so a crate moved or a
    /// feature spelled otherwise than its row is this test red rather than
    /// a lane that quietly measures less.
    #[test]
    fn the_binary_crate_is_where_the_constant_says_and_each_feature_it_turns_on_is_a_rows() {
        let root = workspace();
        assert!(
            root.join(BINARY).join("Cargo.toml").is_file(),
            "{BINARY} has no Cargo.toml"
        );
        let carried = Carried::binary(root).unwrap();
        assert!(!carried.features.is_empty(), "{carried:?}");
        let manifest = Manifest::load(root).unwrap();
        for feature in &carried.features {
            assert!(
                manifest
                    .crates
                    .iter()
                    .any(|krate| krate.feature.as_ref() == Some(feature)),
                "{BINARY} turns on {feature}, which no [[crate]] row names"
            );
        }
    }
}
