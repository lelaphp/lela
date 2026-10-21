//! What a binary of the engine is built from (#3109): the paths cargo's
//! dep-info beside it names, `<binary>.d`, which is the list cargo decides
//! a relink by. Two questions read it, and nothing else answers either:
//!
//! - whether the binary is the tree's ([`BuiltFrom::newest`]): behind the
//!   tree only where a path the build read, or the manifest of a package
//!   it links, was written after it, so a write cargo relinks for stales
//!   it and one to `ext/clippy.toml`, a README or a test does not;
//! - what a phpt run is kept under ([`BuiltFrom::keyed`]): every package
//!   the binary links, whole, every host's, and the workspace's manifest,
//!   lock and toolchain, so a commit that changes no source of the binary
//!   keeps every run, and the key is taken from a commit's objects as from
//!   the working tree.
//!
//! A binary with no dep-info of its own, none beside it or one naming
//! another binary (a target directory copied from another checkout), is
//! keyed over every crate directory, as every binary was before #3109,
//! and held to every file under them.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::hosts::{HOSTS, MAKALA};
use crate::layout::{CRATE_DIRS, normalize};

/// The workspace's own files every build of a package reads.
const WORKSPACE: [&str; 4] = [
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    ".cargo/config.toml",
];

/// What a binary with no dep-info is keyed over beside the crate
/// directories: the key every run had before #3109, so a run kept then is
/// found by it still.
const UNLISTED: [&str; 3] = ["Cargo.toml", "Cargo.lock", "ops.toml"];

/// What a binary is built from: the paths its dep-info names, or every
/// source under the crate directories where it has none of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BuiltFrom {
    /// The paths the dep-info names after the binary, as cargo wrote them.
    Listed(Vec<PathBuf>),
    Unlisted,
}

impl BuiltFrom {
    /// What the binary at `binary` is built from, read off `<binary>.d`.
    pub(crate) fn of(binary: &Path) -> BuiltFrom {
        let mut info = binary.as_os_str().to_owned();
        info.push(".d");
        let Ok(text) = fs::read_to_string(&info) else {
            return BuiltFrom::Unlisted;
        };
        let mut words = words(&text);
        let names = |target: &str| {
            let target = Path::new(target.strip_suffix(':')?);
            Some(fs::canonicalize(target).ok()? == fs::canonicalize(binary).ok()?)
        };
        if words.next().as_deref().and_then(names) != Some(true) {
            return BuiltFrom::Unlisted;
        }
        BuiltFrom::Listed(words.map(PathBuf::from).collect())
    }

    /// The paths, relative to `root`, a phpt run on the binary is kept
    /// under: each package of the workspace the dep-info names a path in,
    /// whole, each host's, each other file of the workspace it names (a
    /// file a `build.rs` reads, `ops.toml`), and [`WORKSPACE`]. Where it
    /// names no package under `root`, every crate directory, as
    /// [`BuiltFrom::Unlisted`] is.
    pub(crate) fn keyed(&self, root: &Path) -> Vec<PathBuf> {
        let (packages, hosts) = (self.packages(root), HOSTS.map(|h| h.krate));
        // The served subset's host serves it beside them (`phpt::http`).
        let hosts = hosts.into_iter().chain([MAKALA.krate]).collect::<Vec<_>>();
        let every = UNLISTED.iter().chain(CRATE_DIRS.iter().map(|d| &d.dir));
        if packages.is_empty() {
            return every.map(PathBuf::from).collect();
        }
        let workspace = WORKSPACE.iter().chain(&hosts).map(PathBuf::from);
        let keyed: BTreeSet<PathBuf> = packages.into_iter().chain(workspace).collect();
        keyed.into_iter().collect()
    }

    /// Each path under `root` the dep-info names as its package's
    /// directory, or as itself outside one; none under `target`, which
    /// holds what a `build.rs` generated from the others.
    fn packages(&self, root: &Path) -> BTreeSet<PathBuf> {
        let BuiltFrom::Listed(paths) = self else {
            return BTreeSet::new();
        };
        let canonical = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let within = |path: &Path| {
            let path = path
                .strip_prefix(root)
                .or_else(|_| path.strip_prefix(&canonical));
            Some(normalize(path.ok()?))
        };
        let package = |path: PathBuf| match path.components().next() {
            Some(first) if is_package(first.as_ref()) => {
                Some(Path::new(&first).join(path.components().nth(1)?))
            }
            _ => Some(path),
        };
        let named = paths.iter().filter_map(|path| within(path));
        let named = named.filter(|path| !path.starts_with("target"));
        let found: BTreeSet<PathBuf> = named.filter_map(package).collect();
        let linked = found.iter().any(|path| is_package(path));
        if linked { found } else { BTreeSet::new() }
    }

    /// The newest path the binary of the host `host` is built from under
    /// `root`, and when it was written: each path the dep-info names, a
    /// directory by every file under it, as cargo reads a `build.rs`'s
    /// `rerun-if-changed`, each linked package's `Cargo.toml` and the
    /// toolchain, whose change cargo rebuilds every crate for; with no
    /// dep-info, every `.rs` and `.toml` under the crate directories that
    /// is no test, bench, example or fixture, and no `clippy.toml`, but
    /// another host's, since writing `sapi/fpm` relinks no `php`. `None`
    /// where none is there, and then the binary is behind nothing.
    pub(crate) fn newest(&self, root: &Path, host: &Path) -> Option<(PathBuf, SystemTime)> {
        let listed = match self {
            BuiltFrom::Listed(paths) => paths.clone(),
            BuiltFrom::Unlisted => CRATE_DIRS.iter().map(|d| root.join(d.dir)).collect(),
        };
        let mut found = Vec::new();
        for path in listed {
            if path.is_dir() {
                crate::tally::files_under(&path, &mut found).unwrap_or_default();
            } else {
                found.push(path);
            }
        }
        let packages = self.packages(root).into_iter().filter(|p| is_package(p));
        found.extend(packages.map(|package| root.join(package).join("Cargo.toml")));
        found.push(root.join("rust-toolchain.toml"));
        if *self == BuiltFrom::Unlisted {
            found.retain(|path| is_engine_source(path.strip_prefix(root).unwrap_or(path)));
        }
        let other_host = |path: &Path| {
            let relative = path.strip_prefix(root).unwrap_or(path);
            relative.starts_with("sapi") && !relative.starts_with(host)
        };
        (found.into_iter().filter(|path| !other_host(path)))
            .filter_map(|path| Some((path.clone(), path.metadata().ok()?.modified().ok()?)))
            .max_by_key(|(_, written)| *written)
    }
}

/// Whether one file, named relative to the root, is code a binary with no
/// dep-info may be built from: a `.rs` or a `.toml` a `build.rs` reads,
/// and no test, bench, example or fixture, which no binary links, nor the
/// generated `clippy.toml`, which no build reads.
fn is_engine_source(relative: &Path) -> bool {
    let kind = relative.extension().and_then(|kind| kind.to_str());
    let skipped = ["tests", "tests.rs", "benches", "examples", "fixtures"];
    matches!(kind, Some("rs" | "toml"))
        && !relative.ends_with("clippy.toml")
        && !(relative.components()).any(|part| skipped.iter().any(|s| part.as_os_str() == *s))
}

/// Whether `path`, relative to the root, is under a crate directory: a
/// package's directory or a path in one.
fn is_package(path: &Path) -> bool {
    CRATE_DIRS.iter().any(|d| path.starts_with(d.dir))
}

/// The words of a dep-info line, each path with its `\ ` read as a space.
fn words(text: &str) -> std::vec::IntoIter<String> {
    let mut words: Vec<String> = Vec::new();
    for piece in text.split_whitespace() {
        match words.last_mut() {
            Some(word) if word.ends_with('\\') => {
                word.pop();
                word.push(' ');
                word.push_str(piece);
            }
            _ => words.push(piece.to_owned()),
        }
    }
    words.into_iter()
}

#[cfg(test)]
mod tests;
