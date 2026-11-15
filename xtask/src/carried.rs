//! Which of the manifest's `feature` rows a build carries
//! (harness-design.md §7, revision 63).
//!
//! A row's `feature` says its extension is behind a feature of
//! `php_bundle`, outside php's default `./configure`; it does not say which
//! builds turn that feature on. Each building crate says so itself, in the
//! `features` of its `php_bundle` dependency, most often by naming a set
//! the bundle defines, `host`, or in its own `default` as
//! `php_bundle/<feature>`, as each host turns the tier's `jit` on so that
//! `--no-default-features` builds it without, and this module
//! reads the declaration there and the set's members in the bundle and
//! nowhere else,
//! so no list in the harness is a second copy of either. A row with no
//! `feature` is the bundle's default set, which every build carries.
//!
//! Two builds are read. The binary under test, [`BINARY`]'s crate, is the
//! one the per-commit phpt lane runs: the lane runs the subsets of every
//! row it carries, and of every row naming an oracle variant on the engine
//! built with the row's feature, `done` asks a touched crate of
//! either for a `phpt` reading, and `crosscheck` renders every extension
//! it carries (§4.5 condition 1). `php-linked`, the engine the lanes serve a snippet
//! through, is the other: [`BINARY`]'s crate with the bundle's `linked` set
//! on top (revision 84). `crosscheck` serves an extension through
//! it where it carries the extension and through the binary where it does
//! not.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result};
use toml::Value;

use crate::manifest::Crate;

/// The crate that builds the binary under test, relative to the
/// workspace root: `sapi/cli`, php's CLI host, whose binary is `php`
/// (ADR-0008). The one place the harness names it.
pub const BINARY: &str = "sapi/cli";

/// The package every build turns its extension features on through.
pub(crate) const BUNDLE: &str = "php_bundle";

/// The bundle's feature `php-linked` is built with on top of [`BINARY`]'s.
const LINKED: &str = "linked";

/// The `php_bundle` features one build turns on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Carried {
    features: BTreeSet<String>,
}

impl Carried {
    /// What the binary under test carries, read from [`BINARY`]'s
    /// `Cargo.toml` under `root`, each set it names read as its members
    /// ([`members`]). A workspace that has not created the
    /// crate yet builds no binary to carry a feature, so it carries the
    /// default set alone; a `Cargo.toml` that declares no `php_bundle`
    /// dependency is refused, since that is a binary this module no longer
    /// knows how to read.
    pub fn binary(root: &Path) -> Result<Carried> {
        Carried::binary_over(root, &bundle_features(root)?)
    }

    /// What `php-linked` carries, the engine the witness and crosscheck
    /// lanes serve a snippet through (harness §8, revision 84): it is
    /// [`BINARY`]'s crate built with the bundle's `linked` set, so what
    /// that crate declares and every member of the set, read from
    /// `php_bundle`'s own `Cargo.toml` under `root`.
    pub(crate) fn linked(root: &Path) -> Result<Carried> {
        let features = bundle_features(root)?;
        let mut carried = Carried::binary_over(root, &features)?;
        carried.features.extend(members(&features, [LINKED]));
        Ok(carried)
    }

    /// [`Carried::binary`] over the bundle's `features`, read once.
    fn binary_over(root: &Path, features: &toml::Table) -> Result<Carried> {
        let path = root.join(BINARY).join("Cargo.toml");
        let Some(text) = crate::read_if(&path)? else {
            return Ok(Carried::default());
        };
        let declared = Carried::declared(&text).with_context(|| format!("{}", path.display()))?;
        Ok(Carried {
            features: members(features, declared.features.iter().map(String::as_str)),
        })
    }

    /// The features a crate's `Cargo.toml` turns on in its `php_bundle`
    /// dependency, and those its `default` names as `php_bundle/<feature>`;
    /// a feature of its own it does not turn on by default is not
    /// read, as `linked` is [`Carried::linked`]'s.
    pub(crate) fn declared(text: &str) -> Result<Carried> {
        let table: toml::Table = toml::from_str(text).context("not TOML")?;
        let bundle = table
            .get("dependencies")
            .and_then(|deps| deps.get(BUNDLE))
            .with_context(|| format!("no `{BUNDLE}` dependency"))?;
        let named = |on: Option<&Value>| -> Vec<String> {
            let on = on.and_then(Value::as_array).into_iter().flatten();
            on.filter_map(Value::as_str).map(str::to_owned).collect()
        };
        let mut features: BTreeSet<String> = named(bundle.get("features")).into_iter().collect();
        let defaults = named(table.get("features").and_then(|own| own.get("default")));
        features.extend(defaults.iter().filter_map(|entry| {
            let feature = entry.strip_prefix(BUNDLE)?.strip_prefix('/')?;
            Some(feature.to_owned())
        }));
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

/// The bundle's `[features]` under `root`, empty where the crate is not
/// created yet.
fn bundle_features(root: &Path) -> Result<toml::Table> {
    let path = root.join("crates").join(BUNDLE).join("Cargo.toml");
    let Some(text) = crate::read_if(&path)? else {
        return Ok(toml::Table::new());
    };
    let mut table: toml::Table =
        toml::from_str(&text).with_context(|| format!("{}", path.display()))?;
    Ok(match table.remove("features") {
        Some(Value::Table(features)) => features,
        _ => toml::Table::new(),
    })
}

/// The features a build turning `names` on carries: a feature whose
/// entries are all others of the bundle's names a set, as `host`, `linked`
/// and `default` do, and stands for its members, each read the same way;
/// any other feature, an empty one among them, is itself. An extension's
/// feature switches on a dependency of its own, so what it requires
/// beside it is not read here, as a row's `feature` names the extension
/// alone.
fn members<'a>(
    features: &toml::Table,
    names: impl IntoIterator<Item = &'a str>,
) -> BTreeSet<String> {
    fn read(features: &toml::Table, name: &str, into: &mut BTreeSet<String>) {
        let set: Option<Vec<&str>> = features
            .get(name)
            .and_then(Value::as_array)
            .map(|on| on.iter().filter_map(Value::as_str).collect())
            .filter(|on: &Vec<&str>| {
                !on.is_empty() && on.iter().all(|entry| features.contains_key(*entry))
            });
        match set {
            Some(on) => on
                .into_iter()
                .for_each(|member| read(features, member, into)),
            None => {
                into.insert(name.to_owned());
            }
        }
    }
    let mut carried = BTreeSet::new();
    for name in names {
        read(features, name, &mut carried);
    }
    carried
}

#[cfg(test)]
#[cfg(tracked)]
mod tests {
    use super::{BINARY, Carried};
    use crate::manifest::{Crate, Manifest};
    use std::path::Path;

    fn workspace() -> &'static Path {
        crate::package_dir().parent().unwrap()
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

    /// A feature of the bundle the crate's `default` turns on is carried,
    /// and one only another of its own features turns on is not.
    #[test]
    fn a_build_carries_what_its_default_features_turn_on_in_the_bundle() {
        let carried = Carried::declared(
            "[features]\ndefault = [\"php_bundle/jit\"]\nlinked = [\"php_bundle/zend_test\"]\n\n\
             [dependencies]\nphp_bundle = { path = \"x\", features = [\"bz2\"] }\n",
        )
        .unwrap();
        assert_eq!(carried, Carried::turning_on(&["bz2", "jit"]));
        assert!(carried.carries(&Crate::named("php_jit").with_feature("jit")));
        assert!(!carried.carries(&Crate::named("ext/zend_test").with_feature("zend_test")));
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

    /// A set the binary names is read as its members, a set inside it the
    /// same way, and an extension's feature as itself whatever it requires:
    /// the binary turning `host` on carries `pdo_mysql` and not `mysqlnd`,
    /// `php-linked` what `linked` adds to it, and an empty feature is itself.
    #[test]
    fn a_set_the_build_names_is_read_as_its_members() {
        let root = crate::scratch::Tree::new("carried-sets").unwrap();
        for dir in [BINARY, "crates/php_bundle"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        root.write(
            "crates/php_bundle/Cargo.toml",
            "[features]\nhost = [\"core\", \"bz2\"]\ncore = [\"pdo_mysql\"]\n\
             linked = [\"host\", \"zend_test\"]\npdo_mysql = [\"dep:p\", \"mysqlnd\"]\n\
             mysqlnd = [\"dep:m\"]\nbz2 = [\"dep:b\"]\nzend_test = [\"dep:z\"]\njit = []\n",
        )
        .unwrap();
        root.write(
            format!("{BINARY}/Cargo.toml"),
            "[dependencies]\nphp_bundle = { path = \"x\", features = [\"host\", \"jit\"] }\n",
        )
        .unwrap();
        assert_eq!(
            Carried::binary(&root).unwrap(),
            Carried::turning_on(&["pdo_mysql", "bz2", "jit"])
        );
        assert_eq!(
            Carried::linked(&root).unwrap(),
            Carried::turning_on(&["pdo_mysql", "bz2", "jit", "zend_test"])
        );
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
