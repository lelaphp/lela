//! The key a reading is stamped with and held to (#1318): the hash of the
//! scope's sources and, where the scope is a workspace crate, of every
//! workspace crate in its dependency closure, as cargo's fingerprint
//! takes it. A crate stands only while its own tree and the trees it is
//! built from are the ones it was measured over, so a change to
//! `php_engine` marks the reading of every crate above it stale, and a
//! change to one leaf extension marks that extension and the crates that
//! depend on it, and no other.
//!
//! A key is a lookup on a [`Snapshot`]: each file's blob id, read once
//! from the working tree, the index or a commit, and asked for the key of
//! every scope the run stamps or holds (#2163). The stamp, `done`,
//! pre-commit's standing readings, `witness --unmeasured` and `permit`'s
//! `unless` all ask one. From the working tree, a file whose copy git
//! knows to be the index's takes the index's id, and only the rest are
//! read and hashed, so a run that keys eighty crates built from
//! `php_engine` reads the engine's files once, if at all, where each key
//! once read them again. The closure is read from `cargo metadata` once
//! per process and root ([`Graph::of`]), and only when a crate's key is
//! asked for, so a stamp over one file spawns no cargo.
//!
//! The closure is the crate, each workspace crate it depends on directly
//! as a normal, build or dev dependency, and, from those, each reached
//! through normal and build dependencies alone: a dependency's dev
//! dependencies are not part of what the crate is built from. A build
//! dependency is taken whole, since what it runs can generate code. The
//! edges are the ones the manifests declare (`--no-deps`), an optional
//! dependency among them whether a feature turns it on or not: the graph
//! is read without resolving a version or a feature, so it needs no
//! registry and writes no `Cargo.lock`, and a key that counts one crate
//! too many re-measures a crate where one too few would let a reading
//! stand over code it never ran. The hash is one listing over the union
//! of the closure's files, so a crate with no workspace dependency keeps
//! the key its own tree gave it.
//!
//! A crate's key also covers what decides outside every crate how rustc
//! compiles it ([`BUILT_UNDER`], one list): the toolchain file, whole; of
//! `.cargo/config.toml` its rustflags, whose `[target.'cfg(all())']`
//! passes LLVM's `--cgp-max-address-users-to-scan` to every profile
//! (design §10 Profiles, #3372), its profiles, and the `[env]` variables
//! rustc and every C build read for every crate ([`REKEYED`]: the
//! deployment target every object and link is made for, #3356); and of
//! the workspace's `Cargo.toml` its profiles, which the release and the
//! lanes' builds are made in. A reading taken under another toolchain,
//! other rustflags, another profile or another deployment target measured
//! another build, so it is stale, as one taken before a crate of the
//! closure changed is. Any other `[env]` line, such as a library's
//! directory, moves no crate's key, since the crate it reaches is the one
//! whose `build.rs` reads it, and neither does the rest of the manifest,
//! whose members and dependencies reach a key through the crates' own.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::git::{self, Index, Stamp};
use crate::layout::normalize;
use crate::reading::joined;

/// Each file of the workspace and its blob id, read once, and the key of
/// any scope a lookup on it ([`Snapshot::key`]). Taken from one of three
/// contents:
///
/// - [`Snapshot::disk`], the working tree, tracked and untracked files
///   alike: what an instrument stamps and what `done` holds a reading to.
///   Taken before the instrument measures, so a file changed while it
///   runs leaves the reading stale rather than fresh over what it never
///   measured; an instrument that writes sources and then stamps them
///   takes another after it writes.
/// - [`Snapshot::index`], what the index holds: what a commit takes,
///   pre-commit's question (#1143).
/// - [`Snapshot::commit`], the tracked files as the commit holds them:
///   what `permit`'s `unless` reads as evidence a commit was measured
///   (#1448), and what a phpt binary was built from.
///
/// Every key is the one the tree hash over the files on disk gave before
/// #2163, byte for byte: the blob id of each file under the scope and its
/// closure, then the blob id of the sorted `<id> <path>` listing.
#[derive(Debug)]
pub struct Snapshot {
    root: PathBuf,
    /// `HEAD`'s short id when the snapshot was taken, the stamp's commit.
    commit: String,
    /// Each file the content holds and its blob id, sorted by path.
    files: Vec<(String, String)>,
    /// Each tracked file and its id as the content holds it, `gone` for
    /// one missing from the working tree, sorted by path: the listing a
    /// build's sources are named by ([`Snapshot::tracked_over`]).
    tracked: Vec<(String, String)>,
    /// The paths the snapshot was read under, `None` for the whole
    /// content: a key asked of a path outside them is refused, since the
    /// files it would hash were never read ([`Snapshot::disk_over`]).
    covers: Option<Vec<PathBuf>>,
    /// Each key asked of the snapshot, by path: a run that stamps one
    /// scope twice, as `done` stamps the workspace for its verdict and
    /// holds its ticket's reading to it, hashes the listing once.
    keys: Mutex<BTreeMap<PathBuf, String>>,
    /// Each key over tracked files asked of it, by its paths: `done` asks
    /// one per set of sources a phpt run was kept over, for every touched
    /// crate ([`Snapshot::tracked_over`]).
    tracked_keys: Mutex<BTreeMap<Vec<PathBuf>, String>>,
    /// Where the text of a file of [`BUILT_UNDER`] is read from.
    source: Source,
    /// The listing entries of what decides in each file of
    /// [`BUILT_UNDER`] read for its part, read once
    /// ([`Snapshot::built_under`]).
    built_under: OnceLock<Vec<(String, String)>>,
}

/// Where a snapshot's files are: on disk, or in git's objects, as an
/// index or a commit holds them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Disk,
    Git,
}

/// The commits read in this process, by root and id: a commit's files
/// never change, so `permit`'s look through a file's history lists each
/// commit once however many keys it asks of it.
static COMMITS: OnceLock<Mutex<Commits>> = OnceLock::new();

/// The commits read, by the root they were read in and their id.
type Commits = BTreeMap<(PathBuf, String), Arc<Snapshot>>;

impl Snapshot {
    /// A snapshot of the whole content at `root`: `files` and `tracked`
    /// as the fields say, taken at `commit`, no key asked of it yet.
    fn new(
        (root, source): (&Path, Source),
        commit: String,
        files: Vec<(String, String)>,
        tracked: Vec<(String, String)>,
    ) -> Snapshot {
        Snapshot {
            root: root.to_path_buf(),
            commit,
            files,
            tracked,
            covers: None,
            keys: Mutex::default(),
            tracked_keys: Mutex::default(),
            source,
            built_under: OnceLock::new(),
        }
    }

    /// The working tree now under `path` and, for a crate, its closure
    /// alone: what one stamp outside an instrument's run reads, a
    /// `permit` call's over the file it touched above all, where asking
    /// git about every file of the tree would cost the call more than its
    /// judgement does. One question to git, the files under the path
    /// with each tagged tracked or not, and each file read and hashed:
    /// over a few files that costs less than the three questions more the
    /// index's ids would. Its keys are [`Snapshot::disk`]'s for every path
    /// it covers, and it refuses any other.
    pub(crate) fn disk_over(root: &Path, path: &Path) -> Result<Snapshot> {
        let covers = closure(root, path)?;
        let mut files = BTreeMap::new();
        let mut tracked = BTreeMap::new();
        for entry in git::listed_under(root, &["-t"], &covers)? {
            // `<tag> <path>`: `?` for an untracked file, a letter for one
            // the index holds.
            let (tag, path) = entry
                .split_once(' ')
                .with_context(|| format!("git ls-files -t wrote {entry:?}"))?;
            let file = root.join(path);
            let id = file.is_file().then(|| blob(&file)).transpose()?;
            if tag != "?" {
                tracked.insert(
                    path.to_owned(),
                    id.clone().unwrap_or_else(|| "gone".to_owned()),
                );
            }
            if let Some(id) = id {
                files.insert(path.to_owned(), id);
            }
        }
        let (files, tracked) = (files.into_iter().collect(), tracked.into_iter().collect());
        Ok(Snapshot {
            covers: Some(covers),
            ..Snapshot::new(
                (root, Source::Disk),
                git::head_commit(root)?,
                files,
                tracked,
            )
        })
    }

    /// The working tree now. Four questions to git, each over the whole
    /// tree: the index's entries, the tracked files whose copy differs
    /// from the index's, the line endings of each, and the untracked
    /// files it does not ignore. A file the index holds at stage 0 whose
    /// copy git does not list as modified takes the index's id; git
    /// compares content where the stat data cannot settle it (a racily
    /// clean entry). Every other file is read and hashed: a modified,
    /// untracked or conflicted one, and one whose copy carries line
    /// endings the index's has not, which `core.autocrlf=input` leaves
    /// unlisted (#2163; the main checkout carried two such files under
    /// `research/` when this was written, a fresh checkout none).
    pub fn disk(root: &Path) -> Result<Snapshot> {
        let commit = git::head_commit(root)?;
        let mut staged = BTreeMap::new();
        let mut tracked = BTreeSet::new();
        for (path, id) in git::stage(root)? {
            if let Some(id) = id {
                staged.insert(path.clone(), id);
            }
            tracked.insert(path);
        }
        let mut differs =
            BTreeSet::from_iter(git::listed(root, &["ls-files", "-z", "--modified"])?);
        for (head, path) in git::entries(root, &["ls-files", "-z", "--eol"])? {
            if head.contains("w/crlf") || head.contains("w/mixed") {
                differs.insert(path);
            }
        }
        let untracked = BTreeSet::from_iter(git::untracked(root)?);
        // A file's id, or `None` for one that is not a file on disk.
        let id = |path: &str| -> Option<Result<String>> {
            match staged.get(path) {
                Some(id) if !differs.contains(path) => Some(Ok(id.clone())),
                _ => {
                    let file = root.join(path);
                    file.is_file().then(|| blob(&file))
                }
            }
        };
        let mut files = Vec::new();
        let mut on_disk = Vec::new();
        for path in &tracked {
            let held = match id(path) {
                Some(found) => {
                    let found = found?;
                    files.push((path.clone(), found.clone()));
                    found
                }
                None => "gone".to_owned(),
            };
            on_disk.push((path.clone(), held));
        }
        for path in untracked.difference(&tracked) {
            if let Some(found) = id(path) {
                files.push((path.clone(), found?));
            }
        }
        files.sort_unstable();
        Ok(Snapshot::new((root, Source::Disk), commit, files, on_disk))
    }

    /// What `index` holds, as a commit would take it: the keys the scopes
    /// would have on disk were the working tree the index, with no
    /// untracked file beside it.
    pub(crate) fn index(root: &Path, index: &Index) -> Result<Snapshot> {
        let files = index.entries().to_vec();
        let commit = git::head_commit(root)?;
        Ok(Snapshot::new(
            (root, Source::Git),
            commit,
            files.clone(),
            files,
        ))
    }

    /// The tracked files as `commit` holds them, read once per process
    /// and commit.
    pub fn commit(root: &Path, commit: &str) -> Result<Arc<Snapshot>> {
        let read = COMMITS.get_or_init(|| Mutex::new(BTreeMap::new()));
        memo(read, (root.to_path_buf(), commit.to_owned()), || {
            let mut files = git::blobs(root, commit, &[])?;
            files.sort_unstable();
            let short = commit.chars().take(7).collect();
            Ok(Arc::new(Snapshot::new(
                (root, Source::Git),
                short,
                files.clone(),
                files,
            )))
        })
    }

    /// The workspace root the snapshot was taken in.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// The key of the sources under `path`, relative to the root: one
    /// hash over the files of the path and, when the path is a workspace
    /// crate's directory, of the directories of its dependency closure
    /// ([`Graph::paths`]), and of what decides in each file rustc is built
    /// under ([`BUILT_UNDER`]). The graph is read only for a
    /// directory with a manifest, the one kind of path it can widen. A
    /// path outside what a [`Snapshot::disk_over`] was read under is
    /// refused.
    pub fn key(&self, path: &Path) -> Result<String> {
        memo(&self.keys, path.to_path_buf(), || {
            let paths = closure(&self.root, path)?;
            self.covering(&paths)?;
            let mut listing = under(&self.files, &paths);
            if crate_dir(&self.root, path) {
                listing.extend(self.built_under()?.iter().cloned());
                listing.sort_unstable();
            }
            Ok(git::listing_id(&listing))
        })
    }

    /// The listing entries a crate's key takes beside its closure's files
    /// for each file of [`BUILT_UNDER`] read for its part: its path and
    /// the blob id of what of it decides ([`BuiltUnder::part_of`]), none
    /// where the file is absent or names nothing that does. Read from
    /// disk for a snapshot of the working tree and from git's object
    /// otherwise.
    fn built_under(&self) -> Result<&[(String, String)]> {
        if let Some(kept) = self.built_under.get() {
            return Ok(kept);
        }
        let mut entries = Vec::new();
        for file in BUILT_UNDER
            .iter()
            .filter(|f| !toolchain().any(|t| t == f.path))
        {
            let text = match self.source {
                Source::Disk => {
                    let at = self.root.join(file.path);
                    at.is_file()
                        .then(|| fs::read_to_string(&at))
                        .transpose()
                        .with_context(|| format!("reading {}", at.display()))?
                }
                Source::Git => {
                    let found = self
                        .files
                        .binary_search_by(|(p, _)| p.as_str().cmp(file.path));
                    found
                        .ok()
                        .map(|at| git::git(&self.root, &["cat-file", "blob", &self.files[at].1]))
                        .transpose()?
                }
            };
            if let Some(part) = text.as_deref().and_then(|t| file.part_of(t)) {
                entries.push((file.path.to_owned(), git::blob_id(part.as_bytes())));
            }
        }
        Ok(self.built_under.get_or_init(|| entries))
    }

    /// The stamp of the sources under `path`: its key, and the commit
    /// the snapshot was taken at.
    pub(crate) fn stamp(&self, path: &Path) -> Result<Stamp> {
        Ok(Stamp {
            key: self.key(path)?,
            commit: self.commit.clone(),
        })
    }

    /// The hash over every file under `paths`, with no closure added.
    #[cfg(test)]
    pub(crate) fn over(&self, paths: &[PathBuf]) -> Result<String> {
        self.covering(paths)?;
        Ok(git::listing_id(&under(&self.files, paths)))
    }

    /// The hash over the tracked files under `paths` alone, a file gone
    /// from the working tree listed as gone: a checkout of a commit with
    /// nothing under `paths` written since hashes to the commit's own
    /// (`phpt --against`, which names a build by its sources). Tracked
    /// files alone, since an untracked one the build reads is reached by
    /// an edit to a tracked one, and an untracked `.DS_Store` would
    /// otherwise leave a clean checkout matching no commit.
    pub(crate) fn tracked_over(&self, paths: &[PathBuf]) -> Result<String> {
        memo(&self.tracked_keys, paths.to_vec(), || {
            self.covering(paths)?;
            Ok(git::listing_id(&under(&self.tracked, paths)))
        })
    }

    /// Refuse `paths` where the snapshot was read under others: the files
    /// it would hash were never read, and a key over what is missing is
    /// no key of the tree.
    fn covering(&self, paths: &[PathBuf]) -> Result<()> {
        let Some(covers) = &self.covers else {
            return Ok(());
        };
        let outside: Vec<String> = paths
            .iter()
            .filter(|p| {
                !covers
                    .iter()
                    .any(|c| whole(c) || normalize(p).starts_with(normalize(c)))
            })
            .map(|p| p.display().to_string())
            .collect();
        if !outside.is_empty() {
            bail!(
                "a snapshot read under {} has no key over {}",
                joined(covers.iter().map(|c| c.display().to_string()), ", "),
                outside.join(", ")
            );
        }
        Ok(())
    }
}

/// What of a file of [`BUILT_UNDER`] decides how rustc compiles a crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Part {
    /// The whole file: the toolchain.
    Whole,
    /// Of the cargo configuration, `[build] rustflags`, each `[target.<t>]
    /// rustflags`, `[profile]` and each `[env]` variable of [`REKEYED`].
    Config,
    /// Of the workspace's manifest, `[profile]`.
    Manifest,
}

/// One file outside every crate that decides how rustc compiles each one,
/// and what of it does.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BuiltUnder {
    pub(crate) path: &'static str,
    pub(crate) part: Part,
}

/// The files outside every crate that decide how rustc compiles each one,
/// each under every crate's key for its [`Part`] (design §10 Profiles,
/// #3372): the toolchain, in either spelling rustup reads, the cargo
/// configuration and the workspace's manifest.
pub(crate) const BUILT_UNDER: [BuiltUnder; 4] = [
    BuiltUnder::of("rust-toolchain.toml", Part::Whole),
    BuiltUnder::of("rust-toolchain", Part::Whole),
    BuiltUnder::of(".cargo/config.toml", Part::Config),
    BuiltUnder::of("Cargo.toml", Part::Manifest),
];

/// The cargo configuration's `[env]` variables that decide how every
/// crate is compiled, so a crate's key moves with each: rustc reads
/// `MACOSX_DEPLOYMENT_TARGET` for every object and link, and cc for every
/// C build, and `api-check` holds it to the pinned php's (#3356).
pub(crate) const REKEYED: [&str; 1] = [DEPLOYMENT_TARGET];

/// The variable that names the macOS every object and link is made for.
pub(crate) const DEPLOYMENT_TARGET: &str = "MACOSX_DEPLOYMENT_TARGET";

/// The file of [`BUILT_UNDER`] at `path`, relative to the root, if it is
/// one.
pub(crate) fn built_under(path: &str) -> Option<BuiltUnder> {
    BUILT_UNDER.into_iter().find(|f| f.path == path)
}

/// The files of [`BUILT_UNDER`] a crate's key takes whole, the toolchain's
/// two spellings, by their paths.
pub(crate) fn toolchain() -> impl Iterator<Item = &'static str> {
    (BUILT_UNDER.into_iter()).filter_map(|f| matches!(f.part, Part::Whole).then_some(f.path))
}

/// The files of [`BUILT_UNDER`] among `paths` whose part differs between
/// the commit `base` and the working tree at `root`, in `paths`' order:
/// what moved every crate's key since, which `done` names and asks the
/// `fold` reading for. A manifest change to a member or a dependency, or
/// an `[alias]`, moves no part and is not among them.
pub(crate) fn moved<'p>(root: &Path, base: &str, paths: &[&'p str]) -> Result<Vec<&'p str>> {
    let mut moved = Vec::new();
    for path in paths {
        let Some(file) = built_under(path) else {
            continue;
        };
        let part = |text: Option<String>| text.as_deref().and_then(|t| file.part_of(t));
        let now = fs::read_to_string(root.join(path)).ok();
        if part(git::show(root, base, path)?) != part(now) {
            moved.push(*path);
        }
    }
    Ok(moved)
}

impl BuiltUnder {
    const fn of(path: &'static str, part: Part) -> BuiltUnder {
        BuiltUnder { path, part }
    }

    /// What of `text`, the file's, decides, in one canonical rendering;
    /// none where it names nothing that does. A text that does not parse
    /// is taken whole, since what it says cannot be told apart.
    pub(crate) fn part_of(self, text: &str) -> Option<String> {
        match self.part {
            Part::Whole => return Some(text.to_owned()),
            Part::Config | Part::Manifest => {}
        }
        let Ok(table) = text.parse::<toml::Table>() else {
            return Some(text.trim_end().to_owned());
        };
        let mut kept = toml::Table::new();
        let mut keep = |key: String, value: Option<&toml::Value>| {
            if let Some(value) = value {
                kept.insert(key, value.clone());
            }
        };
        keep("profile".to_owned(), table.get("profile"));
        match self.part {
            Part::Config => {
                keep(
                    "build.rustflags".to_owned(),
                    table.get("build").and_then(|b| b.get("rustflags")),
                );
                let targets = table.get("target").and_then(toml::Value::as_table);
                for (name, target) in targets.into_iter().flatten() {
                    keep(format!("target.{name}.rustflags"), target.get("rustflags"));
                }
                for variable in REKEYED {
                    let env = table.get("env").and_then(|e| e.get(variable));
                    keep(format!("env.{variable}"), env);
                }
            }
            Part::Manifest | Part::Whole => {}
        }
        (!kept.is_empty()).then(|| kept.to_string())
    }
}

/// Whether `path` is a workspace crate's directory, one with a manifest.
fn crate_dir(root: &Path, path: &Path) -> bool {
    !whole(path) && root.join(path).join("Cargo.toml").is_file()
}

/// The paths the key of `path` is taken over: the path alone, or a
/// workspace crate's directory, its closure's and the toolchain file, the
/// graph read only for a directory with a manifest, the one kind of path
/// it can widen.
fn closure(root: &Path, path: &Path) -> Result<Vec<PathBuf>> {
    if crate_dir(root, path) {
        let mut paths = Graph::of(root)?.paths(path);
        paths.extend(toolchain().map(PathBuf::from));
        Ok(paths)
    } else {
        Ok(vec![path.to_path_buf()])
    }
}

/// Whether `path` names the whole tree.
fn whole(path: &Path) -> bool {
    path.components().all(|c| matches!(c, Component::CurDir))
}

/// The entries of `listing` under any of `scopes`, as git's pathspec takes
/// a directory or a file: the path itself or anything below it.
fn under(listing: &[(String, String)], scopes: &[PathBuf]) -> Vec<(String, String)> {
    if scopes.iter().any(|s| whole(s)) {
        return listing.to_vec();
    }
    let scopes: Vec<PathBuf> = scopes.iter().map(|s| normalize(s)).collect();
    listing
        .iter()
        .filter(|(path, _)| scopes.iter().any(|s| Path::new(path).starts_with(s)))
        .cloned()
        .collect()
}

/// Each workspace crate's directory and the directories of its
/// dependency closure, relative to the workspace root.
#[derive(Debug, Default)]
pub(crate) struct Graph {
    closures: BTreeMap<PathBuf, BTreeSet<PathBuf>>,
}

/// `cargo metadata --format-version 1 --no-deps` over the workspace whose
/// manifest is `root`'s, asked at `root` so its toolchain file holds.
pub(crate) fn cargo_metadata(root: &Path) -> Result<String> {
    let manifest = root.join("Cargo.toml");
    let out = Command::new("cargo")
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--offline",
        ])
        .arg("--manifest-path")
        .arg(&manifest)
        .current_dir(root)
        .output();
    crate::printed(out, &format!("cargo metadata over {}", manifest.display()))
}

/// `make`'s value at `key`, kept in `cache` the first time and read from it
/// after: what each read here that runs once per process keeps.
fn memo<K: Ord, V: Clone>(
    cache: &Mutex<BTreeMap<K, V>>,
    key: K,
    make: impl FnOnce() -> Result<V>,
) -> Result<V> {
    if let Some(kept) = cache.lock().ok().and_then(|held| held.get(&key).cloned()) {
        return Ok(kept);
    }
    let value = make()?;
    if let Ok(mut held) = cache.lock() {
        held.insert(key, value.clone());
    }
    Ok(value)
}

/// The graphs read in this process, by root: `cargo metadata` runs once
/// per root and not once per crate.
static READ: OnceLock<Mutex<BTreeMap<PathBuf, Arc<Graph>>>> = OnceLock::new();

impl Graph {
    /// The graph of the workspace at `root`, read once per process. A
    /// root with no `Cargo.toml` is no workspace and has an empty graph,
    /// so every path's closure is the path alone.
    pub(crate) fn of(root: &Path) -> Result<Arc<Graph>> {
        let read = READ.get_or_init(|| Mutex::new(BTreeMap::new()));
        memo(read, root.to_path_buf(), || {
            Ok(Arc::new(Graph::read(root)?))
        })
    }

    fn read(root: &Path) -> Result<Graph> {
        if !root.join("Cargo.toml").is_file() {
            return Ok(Graph::default());
        }
        Graph::from_metadata(&cargo_metadata(root)?)
    }

    /// The graph from what `cargo metadata --format-version 1 --no-deps`
    /// writes: its workspace members and the path dependencies among them.
    pub(crate) fn from_metadata(text: &str) -> Result<Graph> {
        let metadata: Metadata =
            serde_json::from_str(text).context("cargo metadata wrote no metadata")?;
        let root = Path::new(&metadata.workspace_root);
        // Each member by the absolute directory a dependency's `path` names.
        let mut dirs: BTreeMap<&Path, PathBuf> = BTreeMap::new();
        let members = metadata
            .packages
            .iter()
            .filter(|p| metadata.workspace_members.contains(&p.id));
        for package in members.clone() {
            let at = Path::new(&package.manifest_path)
                .parent()
                .with_context(|| format!("{} has no directory", package.manifest_path))?;
            let dir = at.strip_prefix(root).with_context(|| {
                format!(
                    "{} is outside the workspace root {}",
                    at.display(),
                    root.display()
                )
            })?;
            dirs.insert(at, dir.to_path_buf());
        }
        // Each member's edges to other members: every kind, and the kinds a
        // dependency passes on, normal and build.
        let mut every: BTreeMap<&Path, Vec<&Path>> = BTreeMap::new();
        let mut built: BTreeMap<&Path, Vec<&Path>> = BTreeMap::new();
        for package in members {
            let Some(at) = Path::new(&package.manifest_path).parent() else {
                continue;
            };
            for dep in &package.dependencies {
                let Some(to) = dep.path.as_deref().map(Path::new) else {
                    continue;
                };
                if !dirs.contains_key(to) {
                    continue;
                }
                every.entry(at).or_default().push(to);
                if dep.kind.as_deref() != Some("dev") {
                    built.entry(at).or_default().push(to);
                }
            }
        }
        let mut closures = BTreeMap::new();
        for (at, dir) in &dirs {
            let mut reached: BTreeSet<&Path> = BTreeSet::from([*at]);
            let mut queue: Vec<&Path> = every.get(at).cloned().unwrap_or_default();
            while let Some(next) = queue.pop() {
                if reached.insert(next) {
                    queue.extend(built.get(next).into_iter().flatten());
                }
            }
            let paths = reached
                .iter()
                .filter_map(|r| dirs.get(r))
                .cloned()
                .collect();
            closures.insert(dir.clone(), paths);
        }
        Ok(Graph { closures })
    }

    /// Whether this process has read the graph of the workspace at
    /// `root`: what a test holds a key over one file to.
    #[cfg(test)]
    pub(crate) fn was_read(root: &Path) -> bool {
        READ.get()
            .and_then(|read| read.lock().ok().map(|held| held.contains_key(root)))
            .unwrap_or(false)
    }

    /// The paths the key of `path` is taken over, sorted: the path alone,
    /// or a workspace crate's directory and its closure's.
    pub(crate) fn paths(&self, path: &Path) -> Vec<PathBuf> {
        self.closures
            .get(path)
            .map_or_else(|| vec![path.to_path_buf()], |c| c.iter().cloned().collect())
    }
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    workspace_members: Vec<String>,
    workspace_root: String,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    manifest_path: String,
    #[serde(default)]
    dependencies: Vec<Dependency>,
}

/// One dependency as the package's manifest declares it: a workspace
/// crate is one with a `path`.
#[derive(Deserialize)]
struct Dependency {
    /// `None` for a normal dependency, `"dev"` or `"build"`.
    kind: Option<String>,
    path: Option<String>,
}

/// The fixture of #1318, `xtask/fixtures/closure`, committed once as a
/// scratch repository of its own: `ext/reflection` and `ext/ctype` built
/// from `php_ext`, which is built from `php_engine`.
#[cfg(test)]
pub(crate) fn fixture(name: &str) -> crate::scratch::Tree {
    let tree = crate::scratch::Tree::new(&format!("closure-{name}")).unwrap();
    let fixture = crate::built_root().join("xtask/fixtures/closure");
    crate::scratch::repository(&fixture, &tree, &["."]).unwrap();
    tree
}

/// The blob id git gives the file's bytes.
fn blob(file: &Path) -> Result<String> {
    let bytes = fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    Ok(git::blob_id(&bytes))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{Graph, Snapshot};
    use crate::built_root;
    use crate::git::{self, tracked_hash, tree_hash};
    use crate::scratch::committing;

    /// The scopes a snapshot is held to the tree hash over: the whole
    /// tree, a crate's closure, a crate, a directory and one file.
    fn scopes(root: &Path, file: &str) -> Vec<Vec<PathBuf>> {
        vec![
            vec![PathBuf::from(".")],
            super::closure(root, Path::new("ext/reflection")).unwrap(),
            vec![PathBuf::from("crates/php_engine")],
            vec![PathBuf::from("ext")],
            vec![PathBuf::from(file)],
        ]
    }

    /// Every key a snapshot of the working tree gives is the one the tree
    /// hash over the files on disk gives, byte for byte (#2163), and its
    /// tracked keys are the tracked hash's, on disk and at the commit:
    /// over a fixture with an untracked file, a modified tracked file, a
    /// deleted tracked file, a file staged and then modified again, and a
    /// file whose copy has CRLF line endings under `core.autocrlf=input`,
    /// which git lists as unmodified while its index holds LF.
    #[test]
    fn a_snapshots_keys_are_the_tree_hash_of_the_files_on_disk() {
        let tree = super::fixture("snapshot");
        let root: &Path = &tree;
        let write = |path: &str, text: &str| fs::write(root.join(path), text).unwrap();
        git::git(root, &["config", "core.autocrlf", "input"]).unwrap();
        write("ext/ctype/crlf.txt", "one\r\ntwo\r\n");
        write("crates/php_ext/gone.txt", "deleted after its commit\n");
        committing(
            root,
            &["add", "--", "ext/ctype/crlf.txt", "crates/php_ext/gone.txt"],
        );
        committing(root, &["commit", "-q", "-m", "crlf"]);
        let eol = git::git(root, &["ls-files", "--eol", "ext/ctype/crlf.txt"]).unwrap();
        assert!(eol.contains("i/lf") && eol.contains("w/crlf"), "{eol}");
        write("ext/reflection/src/untracked.rs", "fn untracked() {}\n");
        write("crates/php_engine/src/lib.rs", "// modified\n");
        fs::remove_file(root.join("crates/php_ext/gone.txt")).unwrap();
        write("ext/reflection/src/lib.rs", "// staged\n");
        committing(root, &["add", "--", "ext/reflection/src/lib.rs"]);
        write("ext/reflection/src/lib.rs", "// staged, then modified\n");
        let head = git::git(root, &["rev-parse", "HEAD"]).unwrap();
        let head = head.trim();
        let now = Snapshot::disk(root).unwrap();
        let then = Snapshot::commit(root, head).unwrap();
        for paths in scopes(root, "ext/ctype/crlf.txt") {
            let named: Vec<&str> = paths.iter().filter_map(|p| p.to_str()).collect();
            assert_eq!(
                now.over(&paths).unwrap(),
                tree_hash(root, &paths).unwrap(),
                "{paths:?}"
            );
            assert_eq!(
                now.tracked_over(&paths).unwrap(),
                tracked_hash(root, &named, None).unwrap(),
                "{paths:?}"
            );
            assert_eq!(
                then.tracked_over(&paths).unwrap(),
                tracked_hash(root, &named, Some(head)).unwrap(),
                "{paths:?}"
            );
        }
        assert_eq!(
            now.key(Path::new("ext/reflection")).unwrap(),
            tree_hash(
                root,
                &super::closure(root, Path::new("ext/reflection")).unwrap()
            )
            .unwrap()
        );
        // Read under one path, a snapshot keys it as the whole tree's
        // does, and refuses a path it never read.
        for path in [
            "ext/reflection",
            "ext/ctype/crlf.txt",
            "crates/php_ext",
            "ext",
        ] {
            let path = Path::new(path);
            let over = Snapshot::disk_over(root, path).unwrap();
            assert_eq!(over.key(path).unwrap(), now.key(path).unwrap(), "{path:?}");
        }
        let one = Snapshot::disk_over(root, Path::new("ext/ctype")).unwrap();
        assert!(one.key(Path::new("ext/reflection")).is_err());
        assert!(one.key(Path::new(".")).is_err());
    }

    /// The same over this workspace, whatever its checkout holds: the
    /// whole tree's key, and an extension's over its closure. The
    /// line-ending case is the fixture's above, since a fresh checkout
    /// writes no CRLF copy for `core.autocrlf=input` to leave unlisted.
    #[test]
    fn a_snapshot_of_the_workspace_keys_it_as_the_tree_hash_does() {
        let root = built_root();
        let now = Snapshot::disk(&root).unwrap();
        assert_eq!(
            now.key(Path::new(".")).unwrap(),
            tree_hash(&root, &[PathBuf::from(".")]).unwrap()
        );
        // A crate's: its closure's files and what decides in the
        // configuration and the manifest, each by its blob id.
        let json = super::closure(&root, Path::new("ext/json")).unwrap();
        let mut listing: Vec<(String, String)> = git::files_under(&root, &json)
            .unwrap()
            .into_iter()
            .map(|path| {
                let id = git::blob_id(&fs::read(root.join(&path)).unwrap());
                (path, id)
            })
            .collect();
        for file in super::BUILT_UNDER
            .iter()
            .filter(|f| !super::toolchain().any(|t| t == f.path))
        {
            let text = fs::read_to_string(root.join(file.path)).unwrap();
            let part = file.part_of(&text).unwrap();
            listing.push((file.path.into(), git::blob_id(part.as_bytes())));
        }
        listing.sort_unstable();
        assert_eq!(
            now.key(Path::new("ext/json")).unwrap(),
            git::listing_id(&listing)
        );
    }

    /// A crate's key moves with the toolchain, with the codegen tables of
    /// the cargo configuration, whose rustflags every profile passes
    /// (#3372), with its deployment target (#3356) and with the manifest's
    /// profiles, and not with another `[env]` line, the rest of the
    /// manifest or a file rustc never reads; the
    /// index's key agrees with the working tree's, read from git's object.
    #[test]
    fn a_crates_key_moves_with_what_rustc_is_built_under() {
        let tree = super::fixture("built-under");
        let ctype = Path::new("ext/ctype");
        let key = |tree: &Path| Snapshot::disk(tree).unwrap().key(ctype).unwrap();
        let before = key(&tree);
        let write = |file: &str, text: &str| {
            let path = tree.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
        };
        let manifest = fs::read_to_string(tree.join("Cargo.toml")).unwrap();
        write("README.md", "read by no rustc\n");
        write(".cargo/config.toml", "[env]\nA = \"1\"\n");
        write(
            "Cargo.toml",
            &format!("{manifest}\n[workspace.package]\nversion = \"1.0.0\"\n"),
        );
        assert_eq!(key(&tree), before);
        let mut seen = vec![before];
        let profiled = format!("{manifest}\n[profile.release]\ncodegen-units = 16\n");
        for (file, text) in [
            (
                ".cargo/config.toml",
                "[env]\nA = \"2\"\n[target.'cfg(all())']\nrustflags = [\"-C\", \"x\"]\n",
            ),
            (
                ".cargo/config.toml",
                "[target.'cfg(all())']\nrustflags = [\"-C\", \"y\"]\n",
            ),
            (
                ".cargo/config.toml",
                "[build]\nrustflags = [\"-C\", \"y\"]\n",
            ),
            (".cargo/config.toml", "[profile.release]\nlto = true\n"),
            (
                ".cargo/config.toml",
                "[profile.release]\nlto = true\n[env]\nMACOSX_DEPLOYMENT_TARGET = \"11.0\"\n",
            ),
            ("Cargo.toml", profiled.as_str()),
            ("rust-toolchain.toml", "[toolchain]\n"),
            ("rust-toolchain", "stable\n"),
        ] {
            write(file, text);
            let now = key(&tree);
            assert!(
                !seen.contains(&now),
                "{file}: {text} left ext/ctype's key where it was"
            );
            assert_eq!(
                Snapshot::disk_over(&tree, ctype)
                    .unwrap()
                    .key(ctype)
                    .unwrap(),
                now,
                "{text}"
            );
            seen.push(now);
        }
        write(
            ".cargo/config.toml",
            "[env]\nB = \"3\"\nMACOSX_DEPLOYMENT_TARGET = \"11.0\"\n[profile.release]\nlto = true\n",
        );
        let now = key(&tree);
        assert_eq!(&now, seen.last().unwrap());
        committing(&tree, &["add", "--", "."]);
        let index = Snapshot::index(&tree, &git::Index::read(&tree).unwrap()).unwrap();
        assert_eq!(index.key(ctype).unwrap(), now);
    }

    /// A key over one file reads no crate graph: no `cargo metadata` for
    /// a stamp over `seams.toml`. A crate's key reads it.
    #[test]
    fn a_key_over_a_file_spawns_no_cargo() {
        let tree = super::fixture("no-graph");
        let now = Snapshot::disk(&tree).unwrap();
        now.key(Path::new("Cargo.toml")).unwrap();
        now.key(Path::new("ext")).unwrap();
        assert!(!Graph::was_read(&tree));
        now.key(Path::new("ext/ctype")).unwrap();
        assert!(Graph::was_read(&tree));
    }

    /// A snapshot is the tree when it was taken: a file written after it
    /// leaves its keys where they were, so a reading stamped with the one
    /// taken before an instrument measured is stale once the tree moves,
    /// and the next snapshot's key moves with the tree.
    #[test]
    fn a_snapshot_keeps_the_tree_it_was_taken_over() {
        let tree = super::fixture("before");
        let before = Snapshot::disk(&tree).unwrap();
        let ctype = Path::new("ext/ctype");
        let key = before.key(ctype).unwrap();
        fs::write(
            tree.join("ext/ctype/src/lib.rs"),
            "// written while measuring\n",
        )
        .unwrap();
        assert_eq!(before.key(ctype).unwrap(), key);
        assert_ne!(Snapshot::disk(&tree).unwrap().key(ctype).unwrap(), key);
    }

    fn closure(graph: &Graph, dir: &str) -> Vec<String> {
        graph
            .paths(Path::new(dir))
            .iter()
            .map(|p| p.display().to_string())
            .collect()
    }

    /// The workspace's own graph: an extension's closure reaches
    /// `php_engine` through `php_ext`, `php_engine`'s reaches no
    /// extension, and a leaf extension is in no other leaf's closure.
    #[cfg(tracked)]
    #[test]
    fn the_workspace_closure_follows_cargos_graph() {
        let graph = Graph::of(&built_root()).unwrap();
        let reflection = closure(&graph, "ext/reflection");
        assert!(
            reflection.contains(&"crates/php_engine".to_owned()),
            "{reflection:?}"
        );
        assert!(reflection.contains(&"crates/php_ext".to_owned()));
        assert!(!reflection.contains(&"ext/ctype".to_owned()));
        let engine = closure(&graph, "crates/php_engine");
        assert!(engine.iter().all(|p| !p.starts_with("ext/")), "{engine:?}");
        assert!(closure(&graph, "crates/php_bundle").contains(&"ext/ctype".to_owned()));
        assert_eq!(closure(&graph, "docs"), ["docs"]);
        assert_eq!(closure(&graph, "."), ["."]);
    }

    /// The fixture's graph: an extension's closure reaches `php_engine`
    /// through `php_ext`, and neither extension is in the other's.
    #[test]
    fn the_fixtures_closure_reaches_through_php_ext() {
        let tree = super::fixture("graph");
        let graph = Graph::of(&tree).unwrap();
        assert_eq!(
            closure(&graph, "ext/reflection"),
            ["crates/php_engine", "crates/php_ext", "ext/reflection"]
        );
        assert_eq!(closure(&graph, "crates/php_engine"), ["crates/php_engine"]);
    }

    /// A dev dependency is followed from the crate itself and not from a
    /// crate it reaches: `b` dev-depends on `c`, so `c` is in `b`'s
    /// closure and not in `a`'s, which depends on `b`.
    #[test]
    fn a_dependencys_dev_dependencies_are_not_followed() {
        let member = |name: &str| format!("path+file:///w/{name}#0.0.0");
        let edge = |to: &str, kind: &str| format!(r#"{{"kind":{kind},"path":"/w/{to}"}}"#);
        let package = |name: &str, deps: &[String]| {
            format!(
                r#"{{"id":"{}","manifest_path":"/w/{name}/Cargo.toml","dependencies":[{}]}}"#,
                member(name),
                deps.join(",")
            )
        };
        let text = format!(
            r#"{{"packages":[{},{},{}],"workspace_members":["{}","{}","{}"],"workspace_root":"/w"}}"#,
            package(
                "a",
                &[edge("b", "null"), r#"{"kind":null,"path":null}"#.to_owned()]
            ),
            package("b", &[edge("c", r#""dev""#)]),
            package("c", &[edge("elsewhere", "null")]),
            member("a"),
            member("b"),
            member("c"),
        );
        let graph = Graph::from_metadata(&text).unwrap();
        assert_eq!(closure(&graph, "a"), ["a", "b"]);
        assert_eq!(closure(&graph, "b"), ["b", "c"]);
        assert_eq!(graph.paths(Path::new("c")), [PathBuf::from("c")]);
    }
}
