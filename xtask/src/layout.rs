//! Where the workspace's crates live (design §4.2, ADR-0004, ADR-0008):
//! the one statement of the directories a crate is laid down in, what a
//! crate in each is named in the manifest, and what cargo calls its
//! package. `crates/<name>` is `<name>` and its package is itself;
//! `ext/<name>` is `ext/<name>`, package `php_ext_<name>`; `sapi/<name>` is
//! `sapi/<name>`, package `php_sapi_<name>`; the harness is `xtask`, a
//! crate of its own at the root. Every instrument that turns a name into a
//! path or a package, or a path or a package into a name, asks here.

use std::path::{Path, PathBuf};

/// One directory crates are laid down in, one crate per subdirectory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CrateDir {
    /// The directory, relative to the root.
    pub(crate) dir: &'static str,
    /// Whether a crate's manifest name carries the directory before it,
    /// `ext/pcre`, or is the bare name, `php_heap`.
    prefixed: bool,
    /// What a crate's package name carries before the bare name.
    package: &'static str,
}

/// The directories crates are laid down in, the unprefixed one first.
pub(crate) const CRATE_DIRS: [CrateDir; 3] = [
    CrateDir {
        dir: "crates",
        prefixed: false,
        package: "",
    },
    CrateDir {
        dir: "ext",
        prefixed: true,
        package: "php_ext_",
    },
    CrateDir {
        dir: "sapi",
        prefixed: true,
        package: "php_sapi_",
    },
];

/// The harness, a crate of its own at the root.
pub(crate) const HARNESS: &str = "xtask";

impl CrateDir {
    /// The manifest name of the crate `bare` in this directory.
    pub(crate) fn name(self, bare: &str) -> String {
        if self.prefixed {
            format!("{}/{bare}", self.dir)
        } else {
            bare.to_owned()
        }
    }
}

/// The directory a manifest name's crate is in, with its bare name.
fn split(name: &str) -> Option<(CrateDir, &str)> {
    if name == HARNESS {
        return None;
    }
    for dir in CRATE_DIRS.iter().filter(|d| d.prefixed) {
        if let Some(bare) = name
            .strip_prefix(dir.dir)
            .and_then(|rest| rest.strip_prefix('/'))
        {
            return Some((*dir, bare));
        }
    }
    Some((CRATE_DIRS[0], name))
}

/// Where the crate named `name` is, relative to the root.
pub(crate) fn path(name: &str) -> PathBuf {
    match split(name) {
        Some((dir, bare)) => Path::new(dir.dir).join(bare),
        None => PathBuf::from(name),
    }
}

/// The cargo package of the crate named `name`.
pub(crate) fn package(name: &str) -> String {
    match split(name) {
        Some((dir, bare)) => format!("{}{bare}", dir.package),
        None => name.to_owned(),
    }
}

/// The manifest name of the crate at `rel`, relative to the root: the
/// inverse of [`path`]. `None` for a path that is no crate's directory.
pub(crate) fn name_of_path(rel: &Path) -> Option<String> {
    let parts: Vec<&str> = rel.iter().filter_map(|c| c.to_str()).collect();
    match parts.as_slice() {
        [HARNESS] => Some(HARNESS.to_owned()),
        [dir, bare] => CRATE_DIRS
            .iter()
            .find(|d| d.dir == *dir)
            .map(|d| d.name(bare)),
        _ => None,
    }
}

/// The manifest name of the crate whose package is `package`, for the
/// packages of this workspace: the inverse of [`package`], where every
/// other `php_*` package and `xtask` are themselves. `None` for a package
/// that is no workspace crate.
pub(crate) fn name_of_package(package: &str) -> Option<String> {
    for dir in CRATE_DIRS.iter().filter(|d| d.prefixed) {
        if let Some(bare) = package.strip_prefix(dir.package) {
            return Some(dir.name(bare));
        }
    }
    (package.starts_with("php_") || package == HARNESS).then(|| package.to_owned())
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

    use super::{name_of_package, name_of_path, package, path};

    #[test]
    fn a_name_is_a_path_and_a_package_and_back() {
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
            assert_eq!(name_of_path(Path::new(at)).as_deref(), Some(name), "{at}");
            assert_eq!(name_of_package(cargo).as_deref(), Some(name), "{cargo}");
        }
    }

    #[test]
    fn what_is_no_crate_is_named_nothing() {
        assert_eq!(name_of_path(Path::new("sapi")), None);
        assert_eq!(name_of_path(Path::new("sapi/cli/src")), None);
        assert_eq!(name_of_path(Path::new("docs/adr")), None);
        assert_eq!(name_of_package("serde"), None);
    }
}
