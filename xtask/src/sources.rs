//! What a crate's sources are, for the instruments that read them
//! (`budget`, `unsafe-audit`): the `.rs` files under the crate's path that
//! git does not ignore, less its tests, benches, examples and fixtures;
//! and which crates a staged change touches, and which readings already
//! stand for what is staged, for the instruments pre-commit runs over
//! them; and the index laid down as a tree of its own, for those that
//! read the sources to read what the commit takes ([`Laid`]).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::git;
use crate::key;
use crate::layout::HARNESS;
use crate::lock;
use crate::manifest::{AvoidFrom, Crate, Manifest};
use crate::reading::{Reading, Status, Store, listed};
use crate::scratch::Tree;

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
/// scope and a colon; `status` folds the rows that say it into one.
pub(crate) const NOT_YET_CREATED: &str = "not yet created";

/// Whether the crate has been created: its directory holds a `Cargo.toml`.
pub fn exists(root: &Path, krate: &Crate) -> bool {
    root.join(krate.path()).join("Cargo.toml").is_file()
}

/// Every crate directory on disk (each directory of the manifest's
/// [`Layout`](crate::layout::Layout) and `xtask`, by the presence of a
/// `Cargo.toml`) that has no `[[crate]]` row, as its manifest name
/// (`ext/<name>`, `sapi/<name>` or `<name>`).
pub fn undeclared(root: &Path, manifest: &Manifest) -> Vec<String> {
    let mut out = Vec::new();
    let layout = manifest.layout();
    for dir in layout.dirs() {
        let mut paths = crate::entries(&root.join(dir)).unwrap_or_default();
        paths.sort();
        for path in paths {
            if !path.join("Cargo.toml").is_file() {
                continue;
            }
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let Some(name) = layout.name_of_path(rel) else {
                continue;
            };
            if manifest.crate_named(&name).is_none() {
                out.push(name);
            }
        }
    }
    if root.join(HARNESS).join("Cargo.toml").is_file() && manifest.crate_named(HARNESS).is_none() {
        out.push(HARNESS.to_owned());
    }
    out
}

/// The crates with a staged change, by manifest name: pre-commit's touched
/// crates (harness-design.md §6), what `clippy --staged` and `witness
/// --staged` run over, a crate whose tests read a staged file among them
/// ([`Manifest::touched_by`]).
pub(crate) fn staged_crates(root: &Path, manifest: &Manifest) -> Result<Vec<String>> {
    let staged = git::staged(root)?;
    let paths: Vec<&str> = staged.iter().map(|c| c.path.as_str()).collect();
    let moved = key::parts_moved(root, ("HEAD", key::Now::Index), &paths)?;
    Ok(manifest
        .touched_by(root, &paths, &moved)
        .into_iter()
        .map(|c| c.name.clone())
        .collect())
}

/// The readings that stand for a commit: of one instrument, each
/// that passes and was taken over exactly what the index holds of its
/// scope and of its dependency closure ([`key::Snapshot::key`]), by scope, so pre-commit measures what none covers and writes no
/// reading for the rest. The one asked is the reading that stands over
/// that content, [`Store::over`]'s: the scope's file, or the index's
/// entry for the key, which is where a retired worktree's pass is landed.
/// A reading taken over a working tree that differs
/// from the index there is over content the commit does not take, and
/// stands for nothing. A staged file outside every crate is stamped into
/// no reading, so each voids the readings it can change, as [`Reach`]
/// sorts them, and what they void is kept beside the readings
/// ([`Outside`]), for an instrument that measures what nothing stands for
/// rather than what is staged under a crate.
#[derive(Debug, Default)]
pub(crate) struct Standing {
    readings: BTreeMap<String, Reading>,
    outside: Outside,
}

/// What the files staged outside every crate void, as [`Reach`] sorts
/// them. A file only the harness reads voids xtask's reading alone, which
/// [`Standing`] drops itself, so it is not among these.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum Outside {
    /// No reading but, at most, xtask's.
    #[default]
    Nothing,
    /// Every reading, for this file.
    Every(String),
    /// The readings of the crates whose closure the staged lock moves
    /// ([`lock_moved`]).
    Lock,
}

/// What a staged file outside every crate can change of a reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// Nothing an instrument reads: a Markdown file other than a
    /// glossary, such as a region file pre-commit renders.
    Nothing,
    /// Every reading: a file every build or every instrument reads, the
    /// workspace's `Cargo.toml`, `.cargo/`, the toolchain and lint
    /// configuration, the opcode spec two build scripts read, the
    /// manifest, the oracle pin and the divergence registry; and a
    /// glossary, to `vocab`.
    Every,
    /// The readings of the crates whose dependency closure the lock
    /// pins otherwise than it did where the reading was taken.
    Lock,
    /// xtask's reading alone, since its tests read the tree (a census
    /// under `data/`, a hook under `scripts/`, a workflow).
    Harness,
}

/// The files outside every crate that every reading depends on.
const READ_BY_EVERY: [&str; 12] = [
    "Cargo.toml",
    "rust-toolchain.toml",
    "rust-toolchain",
    "clippy.toml",
    ".clippy.toml",
    "rustfmt.toml",
    ".rustfmt.toml",
    "ext/clippy.toml",
    "ops.toml",
    crate::manifest::FILE,
    crate::divergence::FILE,
    "oracle.toml",
];

impl Reach {
    fn of(path: &str, instrument: &str, glossaries: &[&str]) -> Reach {
        if READ_BY_EVERY.contains(&path) || path.starts_with(".cargo/") {
            Reach::Every
        } else if path == lock::FILE {
            Reach::Lock
        } else if glossaries.contains(&path) {
            if instrument == crate::vocab::INSTRUMENT {
                Reach::Every
            } else {
                Reach::Harness
            }
        } else if Path::new(path).extension().is_some_and(|e| e == "md") {
            Reach::Nothing
        } else {
            Reach::Harness
        }
    }
}

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
        let over: Vec<(String, PathBuf)> = scopes
            .iter()
            .filter_map(|scope| manifest.scope_path(scope).map(|path| (scope.clone(), path)))
            .collect();
        Standing::for_commit_over(root, manifest, store, instrument, &over)
    }

    /// [`Standing::for_commit`] over scopes each named with the path its
    /// reading is stamped over, for an instrument whose scope is no name
    /// [`Manifest::scope_path`] maps, such as crosscheck's `ext/<name>/ri`:
    /// the key is still [`key::Snapshot::key`] over that path.
    pub(crate) fn for_commit_over(
        root: &Path,
        manifest: &Manifest,
        store: &Store,
        instrument: &str,
        scopes: &[(String, PathBuf)],
    ) -> Result<Standing> {
        let glossaries: Vec<&str> = manifest
            .rules
            .iter()
            .filter_map(|r| r.predicate.avoid_from.as_ref().map(AvoidFrom::file))
            .collect();
        let mut outside = Outside::Nothing;
        let mut harness = None;
        for change in git::staged(root)? {
            if !manifest.crates_of(&[change.path.as_str()]).is_empty() {
                continue;
            }
            match Reach::of(&change.path, instrument, &glossaries) {
                Reach::Nothing => {}
                Reach::Every => {
                    println!(
                        "{instrument}: {} is staged outside every crate, so no reading stands for the commit",
                        change.path
                    );
                    return Ok(Standing {
                        readings: BTreeMap::new(),
                        outside: Outside::Every(change.path),
                    });
                }
                Reach::Lock => outside = Outside::Lock,
                Reach::Harness => harness = harness.or(Some(change.path)),
            }
        }
        let index = key::Snapshot::index(root, &git::Index::read(root)?)?;
        let mut standing = BTreeMap::new();
        for (scope, path) in scopes {
            let over = store.over(instrument, scope, &index.key(path)?, manifest.pin_digest())?;
            if let Some(reading) = over.fresh().filter(|r| r.status == Status::Pass) {
                standing.insert(scope.clone(), reading);
            }
        }
        if let Some(path) = harness
            && standing.remove("xtask").is_some()
        {
            println!(
                "{instrument}: {path} is staged outside every crate and xtask's tests read the tree, so xtask's reading does not stand"
            );
        }
        if outside == Outside::Lock {
            let taken = standing.iter().filter_map(|(scope, reading)| {
                let path = scopes.iter().find(|(s, _)| s == scope)?.1.as_path();
                Some((scope.as_str(), path, reading.commit.as_str()))
            });
            let moved = lock_moved(root, manifest, taken)?;
            if !moved.is_empty() {
                println!(
                    "{instrument}: {} is staged and pins a dependency otherwise than where the reading was taken, so no reading stands for {}",
                    lock::FILE,
                    listed(&moved)
                );
            }
            standing.retain(|scope, _| !moved.contains(scope));
        }
        let standing = Standing {
            readings: standing,
            outside,
        };
        if let Some(line) = standing.said(instrument) {
            println!("{line}");
        }
        Ok(standing)
    }

    /// The one line a run prints for what it relies on: the reading when
    /// one stands, the scopes and the oldest reading when several do.
    fn said(&self, instrument: &str) -> Option<String> {
        let oldest = self.readings.values().min_by_key(|r| r.at.to_string())?;
        let taken = format!("{} at {}", oldest.at, crate::track::short(&oldest.commit));
        Some(match self.readings.len() {
            1 => format!(
                "{instrument}: {} stands on its pass of {taken}, taken over what is staged; not run again",
                oldest.scope
            ),
            n => {
                let names: Vec<&str> = self.readings.keys().map(String::as_str).collect();
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
        self.readings.get(scope)
    }

    /// What the files staged outside every crate void.
    pub(crate) fn outside(&self) -> &Outside {
        &self.outside
    }

    /// Of `names`, the ones no standing reading covers: what the run
    /// still measures.
    pub(crate) fn unmeasured(&self, names: &[String]) -> Vec<String> {
        names
            .iter()
            .filter(|n| !self.readings.contains_key(n.as_str()))
            .cloned()
            .collect()
    }
}

/// Of `taken`, each a scope, the path its reading is keyed over and the
/// commit it was taken at, the scopes whose crate's dependency closure the
/// staged lock pins otherwise than the lock at that commit: the
/// standing readings' commits, or `HEAD` for what the commit itself moves.
/// A scope at no commit git can show, or over no crate the lock
/// names, is counted moved. A reading's own staged lock, when the commit
/// it was taken for staged one, is not kept; the lock at its commit stands
/// for it.
pub(crate) fn lock_moved<'a>(
    root: &Path,
    manifest: &Manifest,
    taken: impl IntoIterator<Item = (&'a str, &'a Path, &'a str)>,
) -> Result<Vec<String>> {
    let staged = git::staged_content(root, lock::FILE)?;
    let mut at: BTreeMap<&str, Option<String>> = BTreeMap::new();
    let mut moved = Vec::new();
    for (scope, path, commit) in taken {
        let then = at.entry(commit).or_insert_with(|| {
            (!commit.is_empty())
                .then(|| git::show(root, commit, lock::FILE).ok().flatten())
                .flatten()
        });
        let same = manifest.crate_of(path).is_some_and(|krate| {
            let package = krate.package();
            let now = staged.as_deref().and_then(|s| lock::closure(s, &package));
            now.is_some() && then.as_deref().and_then(|t| lock::closure(t, &package)) == now
        });
        if !same {
            moved.push(scope.to_owned());
        }
    }
    Ok(moved)
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

/// The index laid down as a repository of its own: every file it
/// holds at stage 0 checked out, its own index the same entries, its
/// objects borrowed from the workspace's and its `HEAD` the workspace's.
/// `vocab`, `budget`, `unsafe-audit` and `api-check` under `--staged` read
/// the sources of a crate no reading stands for, and the closure each
/// crate is keyed over comes from `cargo metadata` over the manifests at
/// the root; rooted here, both are what the commit takes, where at the
/// workspace an unstaged edit was read, and an unstaged dependency in a
/// `Cargo.toml` moved the key of a crate the commit leaves as it was. The
/// readings are stamped here, over the index's content, which is what
/// [`Standing`] asks of them at the next commit. Git is asked here without
/// the hook's variables ([`git::apart`]), and the tree goes when the value
/// does.
pub(crate) struct Laid {
    tree: Tree,
}

impl Laid {
    /// The index at `root` laid down, or none where the working tree is
    /// what it holds already: no tracked file differs from its staged copy,
    /// no untracked file stands beside them, and no entry is marked
    /// `skip-worktree` or `assume-unchanged`, which `--modified` passes
    /// over however its working copy differs; `ls-files -v` tags every
    /// other entry `H`.
    pub(crate) fn of(root: &Path) -> Result<Option<Laid>> {
        let differs = git::listed(root, &["ls-files", "-z", "--modified"])?;
        let marked = (git::listed(root, &["ls-files", "-z", "-v"])?.iter())
            .any(|entry| !entry.starts_with("H "));
        if differs.is_empty() && !marked && git::untracked(root)?.is_empty() {
            return Ok(None);
        }
        Laid::lay(root).map(Some)
    }

    fn lay(root: &Path) -> Result<Laid> {
        let head = git::rev_parse(root, &["--verify", "HEAD"])?;
        let objects = root.join(git::rev_parse(root, &["--git-path", "objects"])?);
        // `<mode> <id> <stage>\t<path>`, as `update-index --index-info`
        // reads them back: a conflicted path has no stage 0 and a
        // submodule no file, so neither is laid.
        let mut entries = String::new();
        for entry in git::listed(root, &["ls-files", "--stage", "-z"])? {
            let head = entry.split('\t').next().unwrap_or_default();
            if head.ends_with(" 0") && !head.starts_with("160000 ") {
                entries.push_str(&entry);
                entries.push('\0');
            }
        }
        let laid = Laid {
            tree: Tree::new("staged")?,
        };
        let at = laid.root();
        git::apart(at);
        git::git(at, &["init", "-q", "--template="])?;
        let info = at.join(".git/objects/info");
        fs::create_dir_all(&info).with_context(|| format!("making {}", info.display()))?;
        fs::write(info.join("alternates"), format!("{}\n", objects.display()))
            .context("naming the workspace's objects")?;
        fs::write(at.join(".git/HEAD"), format!("{head}\n")).context("writing HEAD")?;
        git::fed(at, &["update-index", "-z", "--index-info"], &entries)?;
        git::git(at, &["checkout-index", "--all", "--force", "--index"])?;
        Ok(laid)
    }

    /// The root the index is laid at.
    pub(crate) fn root(&self) -> &Path {
        self.tree.path()
    }
}

impl Drop for Laid {
    fn drop(&mut self) {
        git::rejoined(self.tree.path());
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{Laid, Standing, is_binding};
    use crate::built_root;
    use crate::git::git;
    use crate::manifest::Manifest;
    use crate::reading::{Reading, Status, Store};
    use crate::scratch::{Tree, committing};

    /// A repository with `xtask` committed as one file and a manifest to
    /// map it, and a store beside it: what pre-commit stands on.
    fn repository(name: &str) -> (Tree, Manifest, Store) {
        let tree = Tree::new(&format!("standing-{name}")).unwrap();
        fs::create_dir_all(tree.join("xtask/src")).unwrap();
        fs::write(tree.join("xtask/Cargo.toml"), "[package]\n").unwrap();
        fs::write(tree.join("xtask/src/lib.rs"), "a\n").unwrap();
        git(&tree, &["init", "-q", "-b", "main"]).unwrap();
        git(&tree, &["add", "--", "xtask"]).unwrap();
        committing(&tree, &["commit", "-q", "-m", "scratch"]);
        let store = Store::at(tree.join("target/harness"));
        (tree, Manifest::load(&built_root()).unwrap(), store)
    }

    /// Write `text` into the crate, and stage it when `stage` says.
    fn edit(tree: &Tree, text: &str, stage: bool) {
        fs::write(tree.join("xtask/src/lib.rs"), text).unwrap();
        if stage {
            git(tree, &["add", "--", "xtask"]).unwrap();
        }
    }

    /// A passing `clippy` reading over the crate as it is on disk now.
    fn pass(tree: &Tree, manifest: &Manifest, store: &Store) {
        let krate = manifest.crate_named("xtask").unwrap();
        let mut reading =
            Reading::for_crate("clippy", krate, &crate::key::Snapshot::disk(tree).unwrap())
                .unwrap();
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
    fn a_staged_markdown_file_no_instrument_reads_leaves_the_reading_standing() {
        let (tree, manifest, store) = repository("markdown");
        edit(&tree, "b\n", true);
        pass(&tree, &manifest, &store);
        fs::write(tree.join("AGENTS.md"), "").unwrap();
        git(&tree, &["add", "--", "AGENTS.md"]).unwrap();
        assert!(stands(&tree, &manifest, &store));
    }

    #[test]
    fn a_staged_file_outside_every_crate_leaves_nothing_standing() {
        let (tree, manifest, store) = repository("outside");
        edit(&tree, "b\n", true);
        pass(&tree, &manifest, &store);
        fs::write(tree.join("seams.toml"), "").unwrap();
        git(&tree, &["add", "--", "seams.toml"]).unwrap();
        assert!(!stands(&tree, &manifest, &store));
    }

    /// A passing `clippy` reading over `php_heap` as it is on disk, the
    /// crate laid beside `xtask` and committed first.
    #[cfg(tracked)]
    fn heap_passes(tree: &Tree, manifest: &Manifest, store: &Store) {
        fs::create_dir_all(tree.join("crates/php_heap/src")).unwrap();
        fs::write(tree.join("crates/php_heap/Cargo.toml"), "[package]\n").unwrap();
        fs::write(tree.join("crates/php_heap/src/lib.rs"), "h\n").unwrap();
        committing(tree, &["add", "--", "crates"]);
        committing(tree, &["commit", "-q", "-m", "heap"]);
        let krate = manifest.crate_named("php_heap").unwrap();
        let mut reading =
            Reading::for_crate("clippy", krate, &crate::key::Snapshot::disk(tree).unwrap())
                .unwrap();
        reading.status = Status::Pass;
        store.record(&[reading]).unwrap();
    }

    #[cfg(tracked)]
    fn standing_over(tree: &Tree, manifest: &Manifest, store: &Store) -> Vec<String> {
        let scopes = ["xtask".to_owned(), "php_heap".to_owned()];
        let standing = Standing::for_commit(tree, manifest, store, "clippy", &scopes).unwrap();
        scopes
            .into_iter()
            .filter(|s| standing.get(s).is_some())
            .collect()
    }

    /// A staged census is read by xtask's tests and by no build: xtask is
    /// measured again and every other crate's pass stands.
    #[cfg(tracked)]
    #[test]
    fn a_staged_file_only_the_harness_reads_voids_xtasks_reading_alone() {
        let (tree, manifest, store) = repository("census");
        edit(&tree, "b\n", true);
        pass(&tree, &manifest, &store);
        heap_passes(&tree, &manifest, &store);
        fs::create_dir_all(tree.join("data")).unwrap();
        fs::write(tree.join("data/port-map.v0.toml"), "").unwrap();
        git(&tree, &["add", "--", "data"]).unwrap();
        assert_eq!(standing_over(&tree, &manifest, &store), ["php_heap"]);
    }

    /// A staged lock voids the reading of a crate whose closure it moves,
    /// and leaves one whose closure it pins as before standing.
    #[cfg(tracked)]
    #[test]
    fn a_staged_lock_voids_only_the_crates_whose_closure_it_moves() {
        let (tree, manifest, store) = repository("lock");
        let lock = "version = 4\n\n\
            [[package]]\nname = \"php_heap\"\nversion = \"0.0.0\"\n\n\
            [[package]]\nname = \"serde\"\nversion = \"1.0.0\"\nsource = \"registry+x\"\nchecksum = \"aa\"\n\n\
            [[package]]\nname = \"xtask\"\nversion = \"0.0.0\"\ndependencies = [\n \"serde\",\n]\n";
        fs::write(tree.join("Cargo.lock"), lock).unwrap();
        committing(&tree, &["add", "--", "Cargo.lock"]);
        committing(&tree, &["commit", "-q", "-m", "lock"]);
        pass(&tree, &manifest, &store);
        heap_passes(&tree, &manifest, &store);
        assert_eq!(
            standing_over(&tree, &manifest, &store),
            ["xtask", "php_heap"]
        );
        let bumped = lock.replace("checksum = \"aa\"", "checksum = \"bb\"");
        fs::write(tree.join("Cargo.lock"), bumped).unwrap();
        git(&tree, &["add", "--", "Cargo.lock"]).unwrap();
        assert_eq!(standing_over(&tree, &manifest, &store), ["php_heap"]);
    }

    /// A branch's worktree measures `xtask` and passes, is retired, and
    /// main moves on without touching `xtask`: at the merge, what the
    /// index holds of `xtask` is what the worktree measured, so its pass,
    /// which the retire appended into main's log and nowhere else, stands
    /// and `test` is not run again. Before the retire the pass is
    /// in the worktree's store alone and nothing stands.
    #[test]
    fn a_retired_worktrees_pass_stands_at_the_merge_of_its_branch() {
        let (tree, manifest, store) = repository("retired");
        fs::write(tree.join(".gitignore"), "target/\n").unwrap();
        committing(&tree, &["add", "--", ".gitignore"]);
        committing(&tree, &["commit", "-q", "-m", "ignored"]);
        let beside = tree.join("worktrees/t-x");
        let at = beside.to_str().unwrap();
        committing(&tree, &["worktree", "add", "-q", "-b", "t-x", at]);
        fs::write(beside.join("xtask/src/lib.rs"), "b\n").unwrap();
        committing(&beside, &["commit", "-q", "-am", "the branch's change"]);
        let krate = manifest.crate_named("xtask").unwrap();
        let mut reading =
            Reading::for_crate("test", krate, &crate::key::Snapshot::disk(&beside).unwrap())
                .unwrap();
        reading.status = Status::Pass;
        Store::of_workspace(&beside).record(&[reading]).unwrap();

        fs::write(tree.join("notes.md"), "main moves on\n").unwrap();
        committing(&tree, &["add", "--", "notes.md"]);
        committing(&tree, &["commit", "-q", "-m", "main's own change"]);
        let merge = ["merge", "-q", "--no-ff", "--no-commit", "t-x"];
        committing(&tree, &merge);
        let scopes = ["xtask".to_owned()];
        let unretired = Standing::for_commit(&tree, &manifest, &store, "test", &scopes).unwrap();
        assert!(unretired.get("xtask").is_none());
        committing(&tree, &["merge", "--abort"]);

        crate::worktree::retire(&tree, "t-x").unwrap();
        committing(&tree, &merge);
        assert!(
            store.read("test", "xtask").unwrap().is_none(),
            "no file came"
        );
        let standing = Standing::for_commit(&tree, &manifest, &store, "test", &scopes).unwrap();
        let said = standing.said("test").unwrap();
        assert!(
            said.starts_with("test: xtask stands on its pass of "),
            "{said}"
        );
        let measured = super::for_commit(&tree, &manifest, &store, "test").unwrap();
        assert!(measured.is_empty(), "cargo test runs over {measured:?}");
    }

    /// Pre-commit's standing readings are keyed on each crate's closure
    /// (`xtask/fixtures/closure`): a staged change to `ext/ctype`
    /// alone leaves every other crate's pass standing, and one to
    /// `php_engine` leaves standing no crate built from it.
    #[cfg(tracked)]
    #[test]
    fn a_staged_change_voids_the_readings_of_the_crates_built_from_it() {
        let tree = crate::key::fixture("standing");
        let manifest = Manifest::load(&built_root()).unwrap();
        let store = Store::at(tree.join("target/harness"));
        let scopes: Vec<String> = ["php_engine", "php_ext", "ext/reflection", "ext/ctype"]
            .map(str::to_owned)
            .to_vec();
        for name in &scopes {
            let krate = manifest.crate_named(name).unwrap();
            let mut reading =
                Reading::for_crate("clippy", krate, &crate::key::Snapshot::disk(&tree).unwrap())
                    .unwrap();
            reading.status = Status::Pass;
            store.record(&[reading]).unwrap();
        }
        let standing = || {
            let standing =
                Standing::for_commit(&tree, &manifest, &store, "clippy", &scopes).unwrap();
            scopes
                .iter()
                .filter(|s| standing.get(s).is_some())
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(standing(), scopes);

        fs::write(
            tree.join("ext/ctype/src/lib.rs"),
            "pub fn ctype_digit() {}\n",
        )
        .unwrap();
        git(&tree, &["add", "--", "ext/ctype"]).unwrap();
        assert_eq!(standing(), ["php_engine", "php_ext", "ext/reflection"]);

        fs::write(
            tree.join("crates/php_engine/src/lib.rs"),
            "pub fn execute() { }\n",
        )
        .unwrap();
        git(&tree, &["add", "--", "crates/php_engine"]).unwrap();
        assert!(standing().is_empty(), "{:?}", standing());
    }

    /// The index laid down holds the staged copy of a file and not the
    /// working one, nor a file git does not track, and keys a crate as
    /// the index does; a working tree that is the index lays nothing.
    #[test]
    fn the_index_laid_down_holds_what_the_commit_takes() {
        let (tree, _, _) = repository("laid");
        assert!(Laid::of(&tree).unwrap().is_none(), "nothing differs");
        edit(&tree, "b\n", true);
        edit(&tree, "c\n", false);
        fs::write(tree.join("xtask/src/new.rs"), "d\n").unwrap();
        let laid = Laid::of(&tree).unwrap().unwrap();
        let at = laid.root();
        assert_eq!(
            fs::read_to_string(at.join("xtask/src/lib.rs")).unwrap(),
            "b\n"
        );
        assert!(!at.join("xtask/src/new.rs").exists());
        let index = crate::key::Snapshot::index(&tree, &crate::git::Index::read(&tree).unwrap());
        let laid_key = crate::key::Snapshot::disk(at)
            .unwrap()
            .key("xtask".as_ref());
        assert_eq!(
            laid_key.unwrap(),
            index.unwrap().key("xtask".as_ref()).unwrap()
        );
        let gone = at.to_path_buf();
        drop(laid);
        assert!(!gone.exists());
    }

    /// A file marked `skip-worktree` or `assume-unchanged` whose working
    /// copy differs from its staged one is hidden from `ls-files
    /// --modified`; the index is laid down all the same, and holds the
    /// staged copy.
    #[test]
    fn a_marked_file_that_differs_lays_the_index_down() {
        for mark in ["--skip-worktree", "--assume-unchanged"] {
            let (tree, _, _) = repository("marked");
            git(&tree, &["update-index", mark, "--", "xtask/src/lib.rs"]).unwrap();
            fs::write(tree.join("xtask/src/lib.rs"), "b\n").unwrap();
            let laid = Laid::of(&tree).unwrap();
            let laid = laid.unwrap_or_else(|| panic!("{mark}: nothing laid"));
            assert_eq!(
                fs::read_to_string(laid.root().join("xtask/src/lib.rs")).unwrap(),
                "a\n",
                "{mark}"
            );
        }
    }

    #[test]
    fn bindings_are_under_an_extension_ffi_directory() {
        assert!(is_binding("ext/pcre/src/ffi/mod.rs"));
        assert!(is_binding("ext/pcre/src/ffi/deep/sys.rs"));
        assert!(!is_binding("ext/pcre/src/lib.rs"));
        assert!(!is_binding("crates/php_heap/src/ffi/x.rs"));
    }
}
