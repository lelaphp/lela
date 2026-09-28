//! What a crate's sources are, for the instruments that read them
//! (`budget`, `unsafe-audit`): the `.rs` files under the crate's path that
//! git does not ignore, less its tests, benches, examples and fixtures;
//! and which crates a staged change touches, and which readings already
//! stand for what is staged, for the instruments pre-commit runs over
//! them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::git;
use crate::manifest::{Crate, Manifest};
use crate::reading::{Reading, Status, Store, listed};

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

/// The readings that stand for a commit (#1143): of one instrument, each
/// that passes and was taken over exactly what the index holds of its
/// scope, by scope, so pre-commit measures what none covers and writes no
/// reading for the rest. A reading taken over a working tree that differs
/// from the index there is over content the commit does not take, and
/// stands for nothing. Nothing stands while a staged file lies outside
/// every crate: the manifest, the workspace's `Cargo.toml` and lockfile
/// and the glossary are read by the instruments and stamped into no
/// reading, so a commit that changes one is measured whole, as before.
#[derive(Debug, Default)]
pub(crate) struct Standing(BTreeMap<String, Reading>);

impl Standing {
    /// Of `scopes`, the readings of `instrument` that stand for what is
    /// staged, each named on stdout as the reading the run relies on.
    pub(crate) fn for_commit(
        root: &Path,
        manifest: &Manifest,
        store: &Store,
        instrument: &str,
        scopes: &[String],
    ) -> Result<Standing> {
        let staged = git::staged(root)?;
        if let Some(outside) = staged
            .iter()
            .find(|c| manifest.crates_of(&[&c.path]).is_empty())
        {
            println!(
                "{instrument}: {} is staged outside every crate, so no reading stands for the commit",
                outside.path
            );
            return Ok(Standing::default());
        }
        let index = git::Index::read(root)?;
        let mut standing = BTreeMap::new();
        for scope in scopes {
            let Some(path) = manifest.scope_path(scope) else {
                continue;
            };
            let Some(reading) = store.read(instrument, scope)? else {
                continue;
            };
            if reading.status == Status::Pass && reading.fresh_for(&index.hash(&path)) {
                standing.insert(scope.clone(), reading);
            }
        }
        let standing = Standing(standing);
        if let Some(line) = standing.said(instrument) {
            println!("{line}");
        }
        Ok(standing)
    }

    /// The one line a run prints for what it relies on: the reading when
    /// one stands, the scopes and the oldest reading when several do.
    fn said(&self, instrument: &str) -> Option<String> {
        let oldest = self.0.values().min_by_key(|r| r.at.to_string())?;
        let taken = format!("{} at {}", oldest.at, oldest.commit);
        Some(match self.0.len() {
            1 => format!(
                "{instrument}: {} stands on its pass of {taken}, taken over what is staged; not run again",
                oldest.scope
            ),
            n => {
                let names: Vec<&str> = self.0.keys().map(String::as_str).collect();
                format!(
                    "{instrument}: {n} scopes stand on passes taken over what is staged, the oldest of {taken}; not run again: {}",
                    listed(&names)
                )
            }
        })
    }

    /// [`Standing::for_commit`] over every crate that exists, for an
    /// instrument that measures them all; none unless `staged` asks.
    pub(crate) fn of_every_crate(
        staged: bool,
        root: &Path,
        manifest: &Manifest,
        store: &Store,
        instrument: &str,
    ) -> Result<Standing> {
        if !staged {
            return Ok(Standing::default());
        }
        let names: Vec<String> = manifest
            .crates
            .iter()
            .filter(|c| exists(root, c))
            .map(|c| c.name.clone())
            .collect();
        Standing::for_commit(root, manifest, store, instrument, &names)
    }

    /// The standing reading over `scope`, if one stands.
    pub(crate) fn get(&self, scope: &str) -> Option<&Reading> {
        self.0.get(scope)
    }

    /// Of `names`, the ones no standing reading covers: what the run
    /// still measures.
    pub(crate) fn unmeasured(&self, names: &[String]) -> Vec<String> {
        names
            .iter()
            .filter(|n| !self.0.contains_key(n.as_str()))
            .cloned()
            .collect()
    }
}

/// The crates `instrument` measures for a commit: the ones with a staged
/// change ([`staged_crates`]) less those a reading stands for. Empty, and
/// said, when no crate has a staged change.
pub(crate) fn for_commit(
    root: &Path,
    manifest: &Manifest,
    store: &Store,
    instrument: &str,
) -> Result<Vec<String>> {
    let touched = staged_crates(root, manifest)?;
    if touched.is_empty() {
        println!("{instrument}: no crate has a staged change");
        return Ok(touched);
    }
    Ok(Standing::for_commit(root, manifest, store, instrument, &touched)?.unmeasured(&touched))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{Standing, is_binding};
    use crate::built_root;
    use crate::git::git;
    use crate::manifest::Manifest;
    use crate::reading::{Reading, Status, Store};
    use crate::scratch::Tree;

    /// A repository with `xtask` committed as one file and a manifest to
    /// map it, and a store beside it: what pre-commit stands on.
    fn repository(name: &str) -> (Tree, Manifest, Store) {
        let tree = Tree::new(&format!("standing-{name}")).unwrap();
        fs::create_dir_all(tree.join("xtask/src")).unwrap();
        fs::write(tree.join("xtask/Cargo.toml"), "[package]\n").unwrap();
        fs::write(tree.join("xtask/src/lib.rs"), "a\n").unwrap();
        git(&tree, &["init", "-q", "-b", "main"], None).unwrap();
        git(&tree, &["add", "--", "xtask"], None).unwrap();
        let commit = ["-c", "user.name=x", "-c", "user.email=x@x", "-c"];
        let rest = ["commit.gpgsign=false", "-c", "core.hooksPath=/var/empty"];
        let args = [&commit[..], &rest[..], &["commit", "-q", "-m", "scratch"]].concat();
        git(&tree, &args, None).unwrap();
        let store = Store::at(tree.join("target/harness"));
        (tree, Manifest::load(&built_root()).unwrap(), store)
    }

    /// Write `text` into the crate, and stage it when `stage` says.
    fn edit(tree: &Tree, text: &str, stage: bool) {
        fs::write(tree.join("xtask/src/lib.rs"), text).unwrap();
        if stage {
            git(tree, &["add", "--", "xtask"], None).unwrap();
        }
    }

    /// A passing `clippy` reading over the crate as it is on disk now.
    fn pass(tree: &Tree, manifest: &Manifest, store: &Store) {
        let krate = manifest.crate_named("xtask").unwrap();
        let mut reading = Reading::for_crate("clippy", krate, tree).unwrap();
        reading.status = Status::Pass;
        store.record(&[reading]).unwrap();
    }

    fn stands(tree: &Tree, manifest: &Manifest, store: &Store) -> bool {
        let scopes = ["xtask".to_owned()];
        let standing = Standing::for_commit(tree, manifest, store, "clippy", &scopes).unwrap();
        standing.get("xtask").is_some()
    }

    #[test]
    fn a_passing_reading_over_what_is_staged_stands() {
        let (tree, manifest, store) = repository("staged");
        edit(&tree, "b\n", true);
        pass(&tree, &manifest, &store);
        assert!(stands(&tree, &manifest, &store));
    }

    #[test]
    fn a_reading_over_an_unstaged_change_does_not_stand() {
        let (tree, manifest, store) = repository("unstaged");
        edit(&tree, "b\n", true);
        edit(&tree, "c\n", false);
        pass(&tree, &manifest, &store);
        assert!(!stands(&tree, &manifest, &store));
    }

    #[test]
    fn a_change_staged_after_the_reading_is_measured_again() {
        let (tree, manifest, store) = repository("restaged");
        edit(&tree, "b\n", true);
        pass(&tree, &manifest, &store);
        edit(&tree, "c\n", true);
        assert!(!stands(&tree, &manifest, &store));
    }

    #[test]
    fn a_staged_file_outside_every_crate_leaves_nothing_standing() {
        let (tree, manifest, store) = repository("outside");
        edit(&tree, "b\n", true);
        pass(&tree, &manifest, &store);
        fs::write(tree.join("seams.toml"), "").unwrap();
        git(&tree, &["add", "--", "seams.toml"], None).unwrap();
        assert!(!stands(&tree, &manifest, &store));
    }

    #[test]
    fn bindings_are_under_an_extension_ffi_directory() {
        assert!(is_binding("ext/pcre/src/ffi/mod.rs"));
        assert!(is_binding("ext/pcre/src/ffi/deep/sys.rs"));
        assert!(!is_binding("ext/pcre/src/lib.rs"));
        assert!(!is_binding("crates/php_heap/src/ffi/x.rs"));
    }
}
