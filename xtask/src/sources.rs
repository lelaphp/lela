//! What a crate's sources are, for the instruments that read them
//! (`budget`, `unsafe-audit`): the `.rs` files under the crate's path that
//! git does not ignore, less its tests, benches, examples and fixtures;
//! and which crates a staged change touches, for the instruments
//! pre-commit runs over them (`clippy`, `witness`).

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::git;
use crate::manifest::{Crate, Manifest};

/// Where crates live under the root (design §4.2): `crates/*`, `ext/*`, `xtask`.
pub const CRATE_DIRS: [&str; 3] = ["crates", "ext", "xtask"];

/// Directories under a crate whose files are not its code.
const NOT_CODE: [&str; 4] = ["tests", "benches", "examples", "fixtures"];

/// The Rust sources of a crate, relative to the root, sorted.
pub fn rust(root: &Path, krate: &Crate) -> Result<Vec<String>> {
    let base = krate.path();
    Ok(git::files(root, &base)?
        .into_iter()
        .filter(|p| Path::new(p).extension().is_some_and(|e| e == "rs"))
        .filter(|p| {
            let rel = Path::new(p).strip_prefix(&base).unwrap_or(Path::new(p));
            let top_level_not_code = rel
                .components()
                .next()
                .is_some_and(|first| NOT_CODE.iter().any(|d| first.as_os_str() == *d));
            // A `#[cfg(test)] mod tests;` lives in `tests.rs` or `tests/`.
            let test_module = rel
                .components()
                .any(|c| c.as_os_str() == "tests" || c.as_os_str() == "tests.rs");
            !top_level_not_code && !test_module
        })
        .collect())
}

/// Whether a source is one of an extension's bindings, `ext/*/src/ffi/**`,
/// pooled under `[budget] bindings` rather than counted against the crate.
pub fn is_binding(path: &str) -> bool {
    let mut parts = path.split('/');
    parts.next() == Some("ext")
        && parts.next().is_some()
        && parts.next() == Some("src")
        && parts.next() == Some("ffi")
        && parts.next().is_some()
}

/// What a reading says of a crate that has no `Cargo.toml` yet, after the
/// scope and a colon; `status` folds the rows that say it into one (#64).
pub(crate) const NOT_YET_CREATED: &str = "not yet created";

/// Whether the crate has been created: its directory holds a `Cargo.toml`.
pub fn exists(root: &Path, krate: &Crate) -> bool {
    root.join(krate.path()).join("Cargo.toml").is_file()
}

/// Every crate directory on disk (`crates/*`, `ext/*`, `xtask`, by the
/// presence of a `Cargo.toml`) that has no `[[crate]]` row, as its
/// manifest name (`ext/<name>` or `<name>`).
pub fn undeclared(root: &Path, manifest: &Manifest) -> Vec<String> {
    let mut out = Vec::new();
    for dir in CRATE_DIRS.iter().filter(|d| **d != "xtask") {
        let prefix = if *dir == "ext" { "ext/" } else { "" };
        let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
            continue;
        };
        let mut names: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        names.sort();
        for path in names {
            if !path.join("Cargo.toml").is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let name = format!("{prefix}{name}");
            if manifest.crate_named(&name).is_none() {
                out.push(name);
            }
        }
    }
    if root.join("xtask/Cargo.toml").is_file() && manifest.crate_named("xtask").is_none() {
        out.push("xtask".to_owned());
    }
    out
}

/// The crates with a staged change, by manifest name: pre-commit's touched
/// crates (harness-design.md §6), what `clippy --staged` and `witness
/// --staged` run over.
pub(crate) fn staged_crates(root: &Path, manifest: &Manifest) -> Result<Vec<String>> {
    let staged = git::staged(root)?;
    let paths: Vec<&str> = staged.iter().map(|c| c.path.as_str()).collect();
    Ok(manifest
        .crates_of(&paths)
        .into_iter()
        .map(|c| c.name.clone())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::is_binding;

    #[test]
    fn bindings_are_under_an_extension_ffi_directory() {
        assert!(is_binding("ext/pcre/src/ffi/mod.rs"));
        assert!(is_binding("ext/pcre/src/ffi/deep/sys.rs"));
        assert!(!is_binding("ext/pcre/src/lib.rs"));
        assert!(!is_binding("crates/php_heap/src/ffi/x.rs"));
    }
}
