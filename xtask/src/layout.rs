//! Where the workspace's crates live (design §4.2, ADR-0004, ADR-0008):
//! what a crate's manifest name says of its directory and its package.
//! A row named `<dir>/<name>` is a crate laid down at `<dir>/<name>`,
//! package `php_<dir>_<name>`, so `ext/pcre` is `php_ext_pcre`; a bare
//! `<name>` is at `crates/<name>` and its package is itself; the harness
//! is `xtask`, a crate of its own at the root. Which directories crates
//! are laid down in is the manifest's `[[crate]]` rows' and stated nowhere
//! else ([`Layout`], #3187's ruling, #3207). Every instrument that turns a
//! name into a path or a package, or a path or a package into a name,
//! asks here.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::manifest::Crate;

/// The harness, a crate of its own at the root.
pub(crate) const HARNESS: &str = "xtask";

/// The directory a crate whose row names it bare is laid down in.
const BARE: &str = "crates";

/// The directories the manifest's crates are laid down in, derived from
/// its `[[crate]]` rows: `crates`, where a bare name lives, then each
/// directory a row names before its `/`, sorted (#3207).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    dirs: Vec<String>,
}

impl Layout {
    /// The layout `crates`, the manifest's rows, lay their crates down in.
    pub fn of(crates: &[Crate]) -> Layout {
        let named: BTreeSet<&str> = crates
            .iter()
            .filter_map(|krate| Some(split(&krate.name)?.0))
            .collect();
        let dirs = std::iter::once(BARE).chain(named);
        Layout {
            dirs: dirs.map(str::to_owned).collect(),
        }
    }

    /// Each directory crates are laid down in, `crates` first.
    pub(crate) fn dirs(&self) -> impl Iterator<Item = &str> {
        self.dirs.iter().map(String::as_str)
    }

    /// The manifest name of the crate at `rel`, relative to the root: the
    /// inverse of [`path`]. `None` for a path that is no crate's directory.
    pub(crate) fn name_of_path(&self, rel: &Path) -> Option<String> {
        let parts: Vec<&str> = rel.iter().filter_map(|c| c.to_str()).collect();
        match parts.as_slice() {
            [HARNESS] => Some(HARNESS.to_owned()),
            [BARE, bare] => Some((*bare).to_owned()),
            [dir, bare] => self
                .dirs()
                .any(|d| d == *dir)
                .then(|| format!("{dir}/{bare}")),
            _ => None,
        }
    }

    /// The manifest name of the crate whose package is `package`, for the
    /// packages of this workspace: the inverse of [`package`], where every
    /// other `php_*` package and `xtask` are themselves. `None` for a
    /// package that is no workspace crate.
    pub(crate) fn name_of_package(&self, package: &str) -> Option<String> {
        let prefixed = self.dirs().skip(1).find_map(|dir| {
            let bare = package.strip_prefix("php_")?.strip_prefix(dir)?;
            Some(format!("{dir}/{}", bare.strip_prefix('_')?))
        });
        prefixed.or_else(|| {
            (package.starts_with("php_") || package == HARNESS).then(|| package.to_owned())
        })
    }
}

/// The directory a prefixed manifest name names and its bare name;
/// `None` for a bare one.
fn split(name: &str) -> Option<(&str, &str)> {
    name.split_once('/')
}

/// Where the crate named `name` is, relative to the root.
pub(crate) fn path(name: &str) -> PathBuf {
    match split(name) {
        Some(_) => PathBuf::from(name),
        None if name == HARNESS => PathBuf::from(name),
        None => Path::new(BARE).join(name),
    }
}

/// The cargo package of the crate named `name`.
pub(crate) fn package(name: &str) -> String {
    match split(name) {
        Some((dir, bare)) => format!("php_{dir}_{bare}"),
        None => name.to_owned(),
    }
}

/// A path with its `.` and `..` components folded, without touching the
/// filesystem: how a pathspec, a manifest's `path` and a dep-info's
/// `crates/php_engine/../../ops.toml` name a file.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            // A `..` past the start is kept, as `../x` names no `x` here.
            std::path::Component::ParentDir => {
                if !out.pop() && !out.has_root() {
                    out.push(component);
                }
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Layout, package, path};
    use crate::manifest::Crate;

    fn layout() -> Layout {
        let rows = ["php_heap", "php_sapi", "ext/pcre", "sapi/cli", "xtask"];
        Layout::of(&rows.map(Crate::named))
    }

    #[test]
    fn a_name_is_a_path_and_a_package_and_back() {
        let layout = layout();
        for (name, at, cargo) in [
            ("php_heap", "crates/php_heap", "php_heap"),
            ("php_sapi", "crates/php_sapi", "php_sapi"),
            ("ext/pcre", "ext/pcre", "php_ext_pcre"),
            ("sapi/cli", "sapi/cli", "php_sapi_cli"),
            ("sapi/cgi", "sapi/cgi", "php_sapi_cgi"),
            ("xtask", "xtask", "xtask"),
        ] {
            assert_eq!(path(name), Path::new(at), "{name}");
            assert_eq!(package(name), cargo, "{name}");
            let named = layout.name_of_path(Path::new(at));
            assert_eq!(named.as_deref(), Some(name), "{at}");
            let named = layout.name_of_package(cargo);
            assert_eq!(named.as_deref(), Some(name), "{cargo}");
        }
    }

    /// The directories are the rows' own, so one no row names is no
    /// crate's.
    #[test]
    fn the_directories_are_the_rows_and_what_is_no_crate_is_named_nothing() {
        let layout = layout();
        assert_eq!(layout.dirs().collect::<Vec<_>>(), ["crates", "ext", "sapi"]);
        assert_eq!(layout.name_of_path(Path::new("sapi")), None);
        assert_eq!(layout.name_of_path(Path::new("sapi/cli/src")), None);
        assert_eq!(layout.name_of_path(Path::new("docs/adr")), None);
        assert_eq!(layout.name_of_package("serde"), None);
        let bare = Layout::of(&[Crate::named("php_heap")]);
        assert_eq!(bare.name_of_path(Path::new("ext/pcre")), None);
        assert_eq!(
            bare.name_of_package("php_ext_pcre").as_deref(),
            Some("php_ext_pcre")
        );
    }
}
