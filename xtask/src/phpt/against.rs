//! `xtask phpt --against <commit>`: a subset's run held test by test to
//! the stored run at a commit, the merge base as a rule (#663, #664). A
//! test that met its expectation there and fails here is a regression,
//! and the comparison's reading fails while any is, naming them.
//!
//! **What a run is, and what keys it.** Every run in which the engine
//! judged every test a subset holds, and it holds one, is kept as its
//! verdicts, one per test, at
//! `target/harness/phpt-runs/<tree>/<scope>.json`, in the reading store,
//! so CI's upload of the store carries them and a run CI took can serve
//! as a base (harness §4.2, #725). The key is the tree the
//! engine's binary is built from, the tracked files under the paths its
//! dep-info names ([`BuiltFrom::keyed`], #3109), and not the tree the
//! subset's reading is stamped with: that one is the crate's own, which
//! does not move when `php_engine` does, while the verdicts do. A commit
//! that writes no source of the binary, `seams.toml`, `ext/clippy.toml`,
//! a crate it does not link, leaves the key where it was, and every run
//! kept under it stands. The paths are kept beside the runs ([`KEYED`]),
//! and a run is looked for under the tree over each set of them kept, so
//! the key is taken from a commit's objects as from the working tree: the
//! run a worktree at the merge base took, or one an earlier commit with
//! the same binary took, is found by the commit alone from any worktree of
//! the repository ([`roots`]), and a run over sources that are no commit's
//! is found by none.
//!
//! **What a miss keeps.** Beside its verdict, each test that missed keeps
//! its first differing lines and its signature under `misses`, by the same
//! key (#1185), so a run says why a test missed without being run again.
//! A run kept before the field was is read with none. Each test that
//! skipped keeps the reason it was given under `skips`, so a target says
//! why the oracle left a test out of it (#2033).
//!
//! **What gives no verdict.** A comparison is made or it is a skip that
//! says why, and never a pass: no run stored at the commit, a run at the
//! head that measured no engine (no binary, one the tree has moved past,
//! the oracle standing in for it, a pinned php not on this machine), the
//! engine's sources written during the run, the two runs over other
//! corpora or other tests, or no test met at the commit, which leaves
//! nothing to hold. A test past the recipe's limit is its own fail where
//! the adapter judges it so (#691), and a regression like any other.
//!
//! **What the target leaves out.** Where the subset's target is kept
//! (the `target` module, #1184), a test outside it, one the oracle itself
//! skips or misses, is neither held nor a regression nor a fix: its
//! verdict under the engine may flip between a row's `diverged` and a fail
//! past the recipe's limit from one run to the next, as
//! `ext/openssl/tests/stream_server_reneg_limit.phpt` did, and there is no
//! php behaviour on this machine to hold it to.
//!
//! **What the exit answers.** With `--against` the exit is the
//! comparisons', 1 while any is not a pass, so a caller that merges on the
//! exit merges on a pass alone; the subset's own reading is written
//! beside it as before and holds what it always held.
//!
//! **What a base with no run leaves.** The subset runs whatever the base
//! keeps, and only the comparison reads `skip`, its message carrying the
//! subset's own counts: a merge held against a commit nobody ran still
//! measures the head, and `done` finds every touched subset's reading
//! fresh (#1435). The skip names how to fetch the runs CI's dashboard
//! keeps on `main` ([`fetch`], #2362) before how to take one.
//!
//! **What `done` reads of it.** The kept runs and nothing else: condition 1
//! holds a touched crate's subset to no regression against the merge base
//! by diffing the run kept at the base's engine tree with the one kept at
//! this tree's itself ([`regressed`]), and never reads the `@against`
//! reading (harness §4.5, #725).

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::miss::Miss;
use super::target::{Target, named};
use super::{INSTRUMENT, Judgement, Lane, Scope, Tally};
use crate::git;
use crate::key::Snapshot;
use crate::manifest::Manifest;
use crate::oracle::built_from::BuiltFrom;
use crate::oracle::{Pin, Runner};
use crate::reading::{Reading, Status, listed};
use crate::tally::Judgement as _;
use crate::track;

/// The engine's sources as the run found them, by each file's stat data:
/// every file the index tracks, a superset of what any binary's run is
/// kept under ([`BuiltFrom::keyed`]), and the index. What
/// [`Runs::after`] asks after each subset before it asks git anything, as
/// git's index takes a file whose stat data is the one it recorded as
/// unchanged, so a run of many subsets over sources nobody writes asks
/// git nothing after any of them (#2972).
struct Stood {
    /// Each tracked file, then the index.
    files: Vec<PathBuf>,
    /// Each file's stat data, `None` for one gone, as of the last time the
    /// sources were found where the run began.
    stat: RefCell<Vec<Option<Stat>>>,
}

/// What a write to a file moves: its times, its length and, for a file
/// replaced by a rename, its inode. The change time is the kernel's, which
/// no tool that restores a modification time sets back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stat {
    modified: (i64, i64),
    changed: (i64, i64),
    len: u64,
    inode: u64,
}

impl Stood {
    /// The files at `root` and their stat data now: one question to git,
    /// over the engine's sources alone, and one for where the index is,
    /// whose stat is taken before the listing read from it, so an entry
    /// added after the listing moves it.
    fn of(root: &Path) -> Result<Stood> {
        let index = root.join(git::rev_parse(root, &["--git-path", "index"])?);
        let indexed = Stood::stat(std::slice::from_ref(&index));
        let args = ["ls-files", "-z", "--cached"];
        let mut files: Vec<PathBuf> = (git::listed(root, &args)?.into_iter())
            .map(|path| root.join(path))
            .collect();
        // A conflicted path is listed once per stage.
        files.dedup();
        let mut stat = Stood::stat(&files);
        files.push(index);
        stat.extend(indexed);
        Ok(Stood {
            files,
            stat: RefCell::new(stat),
        })
    }

    fn stat(files: &[PathBuf]) -> Vec<Option<Stat>> {
        let stat = |file: &PathBuf| {
            let meta = fs::symlink_metadata(file).ok()?;
            Some(Stat {
                modified: (meta.mtime(), meta.mtime_nsec()),
                changed: (meta.ctime(), meta.ctime_nsec()),
                len: meta.len(),
                inode: meta.ino(),
            })
        };
        files.iter().map(stat).collect()
    }
}

/// Where a workspace keeps its runs, relative to its root: in the reading
/// store, [`crate::reading::DIR`] (harness §4.2, #725).
pub(crate) const RUNS: &str = "target/harness/phpt-runs";

/// One subset's run as it is kept: every test's verdict by its divergence
/// key, and what names the run without keying it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Run {
    /// The commit `HEAD` was at when the run was taken.
    pub commit: String,
    /// The pinned corpus, its tag and commit.
    pub corpus: String,
    /// The engine that ran: the binary's path and md5, or the linked one.
    pub engine: String,
    /// The [`Pin::digest`] of the pin the run was taken under: under
    /// another, the run is stale, as a reading of [`UNDER_RECIPE`] is
    /// (#2725). A run kept before the field was reads empty, which no
    /// digest is.
    ///
    /// [`UNDER_RECIPE`]: crate::reading::UNDER_RECIPE
    #[serde(default)]
    pub recipe: String,
    pub verdicts: BTreeMap<String, Judgement>,
    /// What each test that missed keeps, by the same key: every fail,
    /// bork and unread verdict has one.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub misses: BTreeMap<String, Miss>,
    /// Why each test that skipped skipped, by the same key: the reason its
    /// `SKIPIF` or the runner gave, so the target names it beside each
    /// test the oracle left out (#2033). A run kept before the field was
    /// is read with none.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub skips: BTreeMap<String, String>,
}

impl Run {
    /// Whether the run holds a test, as a run of the engine must to be
    /// kept or read (#3143). A run of none is no evidence about the tree:
    /// `xtask`'s own tests took one of `ext/ctype` over a scratch corpus
    /// into the store of the checkout they ran in, and it stood as the
    /// base's run, the 49 tests of the head's not the same as its none, and
    /// `--base` would not replace it. So one is never kept, and one kept
    /// before is read as no run at all.
    fn holds_a_test(&self) -> bool {
        !self.verdicts.is_empty()
    }
}

/// What a head's run comes to against the stored one.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Diff {
    /// The tests met at the commit that are not met here, with their
    /// verdict here.
    pub regressed: Vec<(String, Judgement)>,
    /// The tests met at the commit.
    pub held: u64,
    /// The tests that counted against the reading at the commit and are
    /// met here.
    pub fixed: u64,
}

/// The runs one `phpt` invocation keeps and is held against: the tree it
/// keys its own by, what names them, and the commit asked for.
pub(crate) struct Runs {
    root: PathBuf,
    /// The tree before any subset ran: what the run's readings are
    /// stamped with and what [`Runs::tree`] keys its own by.
    snapshot: Snapshot,
    /// The workspaces a base's run and a target are looked for in,
    /// [`roots`].
    roots: Vec<PathBuf>,
    /// The engine's sources by their stat data, which [`Runs::moved`]
    /// asks before git.
    stood: Stood,
    commit: String,
    corpus: String,
    /// The pin's digest, which every run this one keeps carries.
    recipe: String,
    base: Option<Base>,
}

/// The commit a run is held against: its short id and its tracked files,
/// which its runs are kept under.
struct Base {
    commit: String,
    snapshot: Arc<Snapshot>,
}

impl Runs {
    /// The tree before any subset ran, which every reading of the run is
    /// stamped with.
    pub(crate) fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// The runs of the workspace at `root`, held against `against` when
    /// one is named; a commit the repository does not have is refused.
    pub(crate) fn new(root: &Path, manifest: &Manifest, against: Option<&str>) -> Result<Runs> {
        let base = against.map(|named| Base::at(root, named)).transpose()?;
        let source = &manifest.pin.source;
        // Stat before the snapshot: a file written between the two is one
        // whose stat has moved by the first subset's end, and is asked of
        // git there.
        let stood = Stood::of(root)?;
        let snapshot = Snapshot::disk(root)?;
        Ok(Runs {
            root: root.to_path_buf(),
            roots: roots(root),
            stood,
            snapshot,
            commit: git::head_commit(root)?,
            corpus: format!("{} {}", source.tag, source.commit),
            recipe: manifest.pin.digest().to_owned(),
            base,
        })
    }

    /// The target of the subset `scope` names after its run, and whether
    /// this run kept it: where the runner's control judged every test,
    /// outside a tier lane, the oracle's verdicts kept under the pin
    /// (#1184); otherwise the one kept under the pin in the first of
    /// [`roots`] that keeps one, if any does.
    pub(crate) fn target_of(
        &self,
        pin: &Pin,
        scope: &str,
        (tally, runner): (&Tally, &Runner),
        lane: Option<Lane>,
    ) -> Result<(Option<Target>, bool)> {
        if let (Runner::Control(oracle), None) = (runner, lane) {
            let engine = format!("{} md5 {}", oracle.display(), pin.oracle.md5);
            if let Some(run) = self.kept(tally, engine) {
                let target = Target::of(run);
                target.keep(&self.root, pin, scope)?;
                return Ok((Some(target), true));
            }
        }
        Ok((self.kept_target(pin, scope)?, false))
    }

    /// The target kept for `scope` under the pin, from the first of
    /// [`roots`] that keeps one: what an engine's run of it is held to.
    pub(crate) fn kept_target(&self, pin: &Pin, scope: &str) -> Result<Option<Target>> {
        Target::kept(&self.roots, pin, scope)
    }

    /// After a subset ran: its run kept, where the engine judged every
    /// test and the sources stood still, and with `--against` the
    /// comparison's reading, over the target where one is kept.
    pub(crate) fn after(
        &self,
        manifest: &Manifest,
        held_as: &Scope,
        (tally, runner): (&Tally, &Runner),
        subset: &Reading,
        target: Option<&Target>,
    ) -> Result<Option<Reading>> {
        let scope = &held_as.to_string();
        let run = self.run_of(tally, runner);
        let (paths, key) = self.keyed(runner)?;
        let moved = self.moved(&paths, &key)?;
        if let (Some(run), false) = (&run, moved) {
            remember(&self.root, &paths)?;
            keep(&path(&self.root, &key, scope), run)?;
        }
        let Some(base) = &self.base else {
            return Ok(None);
        };
        // A base with no run kept is a comparison that gives no verdict and
        // never a subset left unrun: the subset's own reading stands beside
        // it, and the skip says so with its counts (#1435).
        let diff = match (base.run(&self.roots, scope, &self.recipe)?, run) {
            (Err(why), _) => Err(format!(
                "{why}; the subset ran, and its own reading stands beside this one: {}",
                subset.message
            )),
            (Ok(_), _) if moved => {
                Err("the engine's sources were written while the subset ran".to_owned())
            }
            (Ok(_), None) => Err(format!("this run measured no engine: {}", subset.message)),
            (Ok(was), Some(run)) => diff(&was, &run, target),
        };
        let scope = held_as.held_against().to_string();
        let reading = Reading::for_scope(INSTRUMENT, &scope, manifest, &self.snapshot)?;
        compared(reading.under(&manifest.pin), &base.commit, diff).map(Some)
    }

    /// The paths a run on `runner` is kept under, what the binary is built
    /// from ([`BuiltFrom::keyed`]), and the key of the tree the run began
    /// at over them.
    fn keyed(&self, runner: &Runner) -> Result<(Vec<PathBuf>, String)> {
        let built = runner.binary().map_or(BuiltFrom::Unlisted, BuiltFrom::of);
        let paths = built.keyed(&self.root);
        let key = self.snapshot.tracked_over(&paths)?;
        Ok((paths, key))
    }

    /// Whether the engine's sources were written since the run began: not
    /// where no file's stat data moved; where one did, the tree's tracked
    /// files under `paths` held to `key`, the one the run keeps its own by,
    /// and, where they stand still, the stat data taken before that kept,
    /// so a file touched and not written is asked of git once.
    fn moved(&self, paths: &[PathBuf], key: &str) -> Result<bool> {
        let now = Stood::stat(&self.stood.files);
        if now == *self.stood.stat.borrow() {
            return Ok(false);
        }
        let moved = Snapshot::disk(&self.root)?.tracked_over(paths)? != key;
        if !moved {
            self.stood.stat.replace(now);
        }
        Ok(moved)
    }

    /// The run a tally is where the engine judged every test the subset
    /// holds, and none where anything else ran, anything was left or the
    /// subset holds no test ([`Run::holds_a_test`]).
    fn run_of(&self, tally: &Tally, runner: &Runner) -> Option<Run> {
        let engine = match runner {
            Runner::Engine(_) => runner.stamp()?,
            Runner::Linked => "the linked engine".to_owned(),
            _ => return None,
        };
        self.kept(tally, engine).filter(Run::holds_a_test)
    }

    /// Where the engine's run of `scope` is kept after it, relative to the
    /// workspace's root, where it will be: [`Runs::run_of`] is one. What a
    /// reading held to a target points to for the met tests it does not
    /// name (#2033); `after` keeps it there unless the sources moved while
    /// it ran.
    pub(crate) fn run_kept_at(
        &self,
        scope: &Scope,
        (tally, runner): (&Tally, &Runner),
    ) -> Option<PathBuf> {
        self.run_of(tally, runner)?;
        let (_, key) = self.keyed(runner).ok()?;
        Some(path(Path::new(""), &key, &scope.to_string()))
    }

    /// The run a tally is where it judged every test the subset holds,
    /// named by `engine`.
    fn kept(&self, tally: &Tally, engine: String) -> Option<Run> {
        let mut run = Run {
            commit: self.commit.clone(),
            corpus: self.corpus.clone(),
            engine,
            recipe: self.recipe.clone(),
            verdicts: BTreeMap::new(),
            misses: BTreeMap::new(),
            skips: BTreeMap::new(),
        };
        for judged in whole(tally).then_some(&tally.judged)? {
            let id = &judged.id;
            run.verdicts.insert(id.clone(), judged.judgement);
            if let Some(miss) = &judged.miss {
                run.misses.insert(id.clone(), miss.clone());
            }
            if judged.judgement == Judgement::Skip {
                run.skips.insert(id.clone(), judged.note.clone());
            }
        }
        Some(run)
    }
}

impl Base {
    /// The commit `named` names in the repository at `root`, with the
    /// files it tracks; a name that is no commit is refused.
    fn at(root: &Path, named: &str) -> Result<Base> {
        let full = track::rev(root, named).with_context(|| format!("--against {named}"))?;
        Ok(Base {
            commit: track::short(&full).to_owned(),
            snapshot: Snapshot::commit(root, &full)?,
        })
    }

    /// The run of `scope` stored at the commit under the pin whose digest
    /// is `recipe`, from the first of `roots` that keeps one, or why there
    /// is none, naming each root looked in or the run under another pin.
    fn run(&self, roots: &[PathBuf], scope: &str, recipe: &str) -> Result<Result<Run, String>> {
        let commit = &self.commit;
        let take = super::base::call(commit, scope);
        match kept(roots, &self.snapshot, scope, recipe)? {
            Kept::Fresh(run) => return Ok(Ok(run)),
            Kept::Stale => {
                return Ok(Err(format!(
                    "the run of {scope} kept at {commit} is stale; {STALE}; take one with `{take}`"
                )));
            }
            Kept::None => {}
        }
        let looked = looked(roots);
        let fetch = roots.first().map_or_else(String::new, |root| {
            format!(
                "fetch the runs CI's dashboard keeps with `{}`, or ",
                fetch(root)
            )
        });
        Ok(Err(format!(
            "no run of {scope} is kept at {commit}'s engine in {looked}; {fetch}take one with `{take}`, which builds the engine at {commit} in a worktree of its own and keeps the run in the main worktree"
        )))
    }
}

/// The name the dashboard's oracle job uploads [`RUNS`] under: every run
/// its runner keeps, each under its engine's tree (#2362).
pub(crate) const UPLOAD: &str = "phpt-runs";

/// The line that lays the runs CI kept into the workspace at `root`,
/// where [`roots`] looks first (#2362): the latest [`UPLOAD`] downloaded
/// apart, since `gh` writes over no file, and every run and every set of
/// sources one was kept over ([`KEYED`]) copied in where the workspace
/// keeps none, as [`carry`] copies a worktree's.
pub(crate) fn fetch(root: &Path) -> String {
    format!(
        "d=$(mktemp -d) && gh run download -n {UPLOAD} -D \"$d\" && rsync -a --ignore-existing \"$d/\" '{}/'",
        root.join(RUNS).display()
    )
}

/// The run of `scope` kept at `commit`'s engine tree under `pin` in a
/// workspace of the repository at `root` ([`roots`]), or why there is
/// none: what `--against` holds a subset to, and what `phpt --base` keeps.
pub(crate) fn kept_at(
    root: &Path,
    pin: &Pin,
    commit: &str,
    scope: &str,
) -> Result<Result<Run, String>> {
    Base::at(root, commit)?.run(&roots(root), scope, pin.digest())
}

/// Why a kept run is stale, the words `done` gives a stale reading
/// under [`crate::reading::UNDER_RECIPE`].
const STALE: &str = "oracle.toml changed since it was taken";

/// Where a kept run or target is looked for from the workspace at `root`:
/// its checkouts ([`track::checkouts`]), or `root` alone where git cannot
/// list them. The one rule for where a run lives: in the workspace
/// that took it ([`Runs::after`]), keyed by the tree its engine is built
/// from ([`keys`]), so a run taken in a worktree at a base, as the skip of
/// [`Base::run`] asks, is found by the commit from every other worktree
/// while that one stands (#1869). The main worktree outlives the others,
/// so `worktree retire` carries a branch worktree's runs into it
/// ([`carry`]) before removing it.
fn roots(root: &Path) -> Vec<PathBuf> {
    track::checkouts(root).unwrap_or_else(|_| fs::canonicalize(root).into_iter().collect())
}

/// The workspaces at `roots`, as a run not kept in any names them.
fn looked(roots: &[PathBuf]) -> String {
    let looked: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
    looked.join(", ")
}

fn path(root: &Path, tree: &str, scope: &str) -> PathBuf {
    root.join(RUNS).join(tree).join(format!("{scope}.json"))
}

/// Where a workspace keeps each set of paths a run was kept under
/// ([`BuiltFrom::keyed`]), under [`RUNS`]: one file per set, named by its
/// id, beside the runs, so `worktree retire` carries it with them
/// and CI's upload holds it (#3109).
const KEYED: &str = "built-from";

/// Keep `paths`, the set a run at `root` is kept under, among the sets
/// [`keys`] looks under, where it is not kept already and is not the one
/// [`keys`] looks under with none kept.
fn remember(root: &Path, paths: &[PathBuf]) -> Result<()> {
    if paths == BuiltFrom::Unlisted.keyed(root) {
        return Ok(());
    }
    let text = serde_json::to_string_pretty(paths).context("rendering a set of sources")?;
    let file =
        (root.join(RUNS).join(KEYED)).join(format!("{}.json", git::blob_id(text.as_bytes())));
    if file.is_file() {
        return Ok(());
    }
    crate::reading::replace(&file, text.as_bytes())
}

/// Every key a run at `snapshot`'s engine may be kept under: its tracked
/// files over each set of paths a run was kept under in any of `roots`
/// ([`remember`]), and over [`BuiltFrom::Unlisted`]'s, which needs none
/// kept. Two trees with one key over a set build one binary from it, since
/// the set holds every manifest the build resolved its packages from, so a
/// run found under any is the engine's at this tree.
fn keys(snapshot: &Snapshot, roots: &[PathBuf]) -> Result<Vec<String>> {
    let mut sets = BTreeSet::from([BuiltFrom::Unlisted.keyed(snapshot.root())]);
    for root in roots {
        let Ok(files) = crate::entries(&root.join(RUNS).join(KEYED)) else {
            continue;
        };
        for file in files
            .iter()
            .filter(|file| file.extension().is_some_and(|e| e == "json"))
        {
            let set: Vec<PathBuf> = serde_json::from_str(&crate::read(file)?)
                .with_context(|| format!("{} is not a set of paths", file.display()))?;
            sets.insert(set);
        }
    }
    sets.iter().map(|set| snapshot.tracked_over(set)).collect()
}

/// What the workspaces at `roots` keep of `scope` at `snapshot`'s engine.
enum Kept {
    /// The newest run under any of its [`keys`] taken under the pin.
    Fresh(Run),
    /// Runs under other pins alone.
    Stale,
    None,
}

/// The run of `scope` at `snapshot`'s engine, under any of its [`keys`]
/// in any of `roots`, the newest of those taken under the pin whose digest
/// is `recipe`; a run of no test is none ([`Run::holds_a_test`]).
fn kept(roots: &[PathBuf], snapshot: &Snapshot, scope: &str, recipe: &str) -> Result<Kept> {
    let mut found: Option<(Option<SystemTime>, Run)> = None;
    let mut stale = false;
    for key in keys(snapshot, roots)? {
        for root in roots {
            let file = path(root, &key, scope);
            let Some(run) = read(&file)?.filter(Run::holds_a_test) else {
                continue;
            };
            if run.recipe != recipe {
                stale = true;
                continue;
            }
            let at = written(&file);
            if found.as_ref().is_none_or(|(newest, _)| at > *newest) {
                found = Some((at, run));
            }
        }
    }
    Ok(match found {
        Some((_, run)) => Kept::Fresh(run),
        None if stale => Kept::Stale,
        None => Kept::None,
    })
}

/// `run` kept at `file`, a subset's or a target's, replaced whole as a
/// reading's file is, so a comparison reading it from another worktree
/// never finds it cut short.
pub(super) fn keep(file: &Path, run: &Run) -> Result<()> {
    let text = serde_json::to_string_pretty(run).context("rendering the run")?;
    crate::reading::replace(file, text.as_bytes())
}

/// The run kept at `file`, if one is: a subset's or a target's.
pub(super) fn read(file: &Path) -> Result<Option<Run>> {
    let Some(text) = crate::read_if(file)? else {
        return Ok(None);
    };
    let run =
        serde_json::from_str(&text).with_context(|| format!("{} is not a run", file.display()))?;
    Ok(Some(run))
}

/// Where the workspace `snapshot` stands in keeps the run of `scope` taken
/// on a binary with no dep-info of its own ([`BuiltFrom::Unlisted`]), one
/// of the files [`regressed`] reads as this tree's run.
pub fn kept_run(snapshot: &Snapshot, scope: &str) -> Result<PathBuf> {
    let tree = snapshot.tracked_over(&BuiltFrom::Unlisted.keyed(snapshot.root()))?;
    Ok(path(snapshot.root(), &tree, scope))
}

/// What [`regressed`] finds of one subset against the base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Against {
    /// Every test met at the base is met here, or none was met there.
    Held,
    /// No run is kept at this tree's engine: the subset is unrun here.
    Unrun(String),
    /// What is unmet with a run here: no run at the base, two runs that do
    /// not compare, or the tests that regressed.
    Unmet(String),
}

/// Condition 1's `phpt` (harness §4.5, #725): every test met in the run of
/// `scope` kept at `base`'s engine tree is met in the one kept at
/// `snapshot`'s, a skip meeting none ([`diff`]), over the target kept under
/// `pin` where there is one; a run kept under another pin is stale
/// (#2725).
pub(crate) fn regressed(
    snapshot: &Snapshot,
    base: &str,
    pin: &Pin,
    scope: &str,
) -> Result<Against> {
    let roots = roots(snapshot.root());
    let run = match kept(&roots, snapshot, scope, pin.digest())? {
        Kept::Fresh(run) => run,
        Kept::Stale => {
            return Ok(Against::Unrun(format!(
                "the run of {scope} kept at this tree's engine is stale; {STALE}"
            )));
        }
        Kept::None => {
            return Ok(Against::Unrun(format!(
                "no run of {scope} is kept at this tree's engine (in {}); take one with `cargo xtask phpt --subset {scope}`",
                looked(&roots)
            )));
        }
    };
    let base = Base::at(snapshot.root(), base)?;
    let was = match base.run(&roots, scope, pin.digest())? {
        Ok(was) => was,
        Err(why) => return Ok(Against::Unmet(why)),
    };
    let target = Target::kept(&roots, pin, scope)?;
    let diff = match diff(&was, &run, target.as_ref()) {
        Ok(diff) if diff.regressed.is_empty() => return Ok(Against::Held),
        Ok(diff) => diff,
        Err(why) => {
            return Ok(Against::Unmet(format!(
                "{scope}: no verdict against {}; {why}",
                base.commit
            )));
        }
    };
    let names: Vec<&str> = diff.regressed.iter().map(|(id, _)| named(id)).collect();
    Ok(Against::Unmet(format!(
        "{scope}: {} of {} tests met at {} are not met here: {}",
        names.len(),
        diff.held,
        base.commit,
        listed(&names)
    )))
}

/// Whether a tally judged every test its subset holds, which a run or a
/// target must to be kept.
fn whole(tally: &Tally) -> bool {
    tally.judged.len() == tally.found
}

/// What [`carry`] did with one worktree's kept runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Carried {
    /// The runs copied in, which the workspace kept none of.
    pub carried: usize,
    /// The runs the workspace kept already, left as they stood.
    pub kept: usize,
}

/// Every run the workspace at `from` keeps, copied into the one at `into`
/// where that one keeps no run of the same scope under the same engine
/// tree; one it keeps already is left standing unless the carried one
/// [`supersedes`] it. A run is keyed by the tree the engine's binary is
/// built from, so a run taken anywhere at that tree under one pin is the
/// same run, and the one standing is as good as the one carried. What
/// `worktree retire` keeps of a worktree before removing it, so a run
/// taken there is still found, from the main worktree among [`roots`],
/// once the worktree is gone (#1268).
pub(crate) fn carry(from: &Path, into: &Path) -> Result<Carried> {
    let runs = from.join(RUNS);
    let mut files = Vec::new();
    crate::tally::files_under(&runs, &mut files)?;
    files.retain(|file| file.extension().is_some_and(|e| e == "json"));
    files.sort();
    let mut carried = Carried::default();
    for file in files {
        let Ok(under) = file.strip_prefix(&runs) else {
            continue;
        };
        let to = into.join(RUNS).join(under);
        if to.exists() && !supersedes(&file, &to) {
            carried.kept += 1;
            continue;
        }
        if let Some(dir) = to.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        fs::copy(&file, &to)
            .with_context(|| format!("copying {} to {}", file.display(), to.display()))?;
        carried.carried += 1;
    }
    Ok(carried)
}

/// Whether the run kept at `file` takes the place of the one standing at
/// `to`: taken after it and under another pin, so the one standing is
/// stale once the carried one is fresh (#2725), as a run `phpt --base`
/// takes again under a moved pin is. A run of no test is read as no run
/// (#3143): one holding a test replaces it, and it replaces none that does.
fn supersedes(file: &Path, to: &Path) -> bool {
    let (carried, standing) = (read(file).ok().flatten(), read(to).ok().flatten());
    let tested = |run: &Option<Run>| run.as_ref().is_some_and(Run::holds_a_test);
    if tested(&carried) != tested(&standing) {
        return tested(&carried);
    }
    let recipe = |run: Option<Run>| run.map(|run| run.recipe);
    recipe(carried) != recipe(standing) && written(file) > written(to)
}

/// When the file at `path` was last written, where that can be read.
fn written(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// The head's run against the stored one, or why the two do not compare;
/// a test outside `target`, where one is kept, is none of held, regressed
/// or fixed.
pub(crate) fn diff(was: &Run, run: &Run, target: Option<&Target>) -> Result<Diff, String> {
    if was.corpus != run.corpus {
        return Err(format!(
            "the stored run is over the corpus {} and this one over {}",
            was.corpus, run.corpus
        ));
    }
    if !was.verdicts.keys().eq(run.verdicts.keys()) {
        return Err(format!(
            "the stored run holds {} tests and this one {}, and they are not the same tests",
            was.verdicts.len(),
            run.verdicts.len()
        ));
    }
    let mut diff = Diff::default();
    for ((id, then), now) in was.verdicts.iter().zip(run.verdicts.values()) {
        if target.is_some_and(|target| !target.holds(id)) {
            continue;
        }
        if then.met() {
            diff.held += 1;
            // The compiler refusing a test it read there is a loss as a
            // failure is, and so is a skip: neither meets a test (#725).
            if now.counts_against() || matches!(now, Judgement::Unread | Judgement::Skip) {
                diff.regressed.push((id.clone(), *now));
            }
        } else if then.counts_against() && now.met() {
            diff.fixed += 1;
        }
    }
    Ok(diff)
}

/// The words, the counts and the status of one comparison: fail while a
/// test met at `commit` is not met here, a skip that says why where no
/// verdict is given, and a pass otherwise.
pub(crate) fn compared(
    mut reading: Reading,
    commit: &str,
    diff: Result<Diff, String>,
) -> Result<Reading> {
    let diff = match diff {
        Ok(diff) if diff.held > 0 => diff,
        diff => {
            let none = format!("no test met its expectation at {commit}, so nothing is held");
            reading.status = Status::Skip;
            let why = diff.err().unwrap_or(none);
            reading.message = format!("{}: no verdict against {commit}; {why}", reading.scope);
            return Ok(reading);
        }
    };
    let names: Vec<&str> = diff
        .regressed
        .iter()
        .map(|(id, now)| {
            println!("regressed {id}: met at {commit}, {} here", now.as_str());
            named(id)
        })
        .collect();
    reading.count("held", diff.held)?;
    reading.count("regressed", names.len() as u64)?;
    reading.count("fixed", diff.fixed)?;
    let (status, lost) = if names.is_empty() {
        (Status::Pass, "none fails here".to_owned())
    } else {
        (
            Status::Fail,
            format!("{} fail here: {}", names.len(), listed(&names)),
        )
    };
    reading.status = status;
    reading.message = format!(
        "{}: of {} tests met at {commit}, {lost}; {} newly met",
        reading.scope, diff.held, diff.fixed
    );
    Ok(reading)
}

/// The exit `--against` owes: 1 while any comparison is not a pass,
/// whatever the subsets' own readings say.
pub(crate) fn answer(readings: &[Reading]) -> Result<()> {
    let asked: Vec<&Reading> = readings
        .iter()
        .filter(|r| r.scope.parse::<Scope>().is_ok_and(|scope| scope.against))
        .collect();
    let short: Vec<&str> = asked
        .iter()
        .filter(|r| r.status != Status::Pass)
        .map(|r| r.scope.as_str())
        .collect();
    if short.is_empty() {
        return Ok(());
    }
    bail!(
        "{} of {} comparisons are no pass: {}",
        short.len(),
        asked.len(),
        listed(&short)
    )
}

#[cfg(test)]
mod tests;
