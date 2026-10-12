//! `xtask phpt --against <commit>`: a subset's run held test by test to
//! the stored run at a commit, the merge base as a rule (#663, #664). A
//! test that met its expectation there and fails here is a regression,
//! and the comparison's reading fails while any is, naming them.
//!
//! **What a run is, and what keys it.** Every run in which the engine
//! judged every test a subset holds is kept as its verdicts, one per
//! test, at `target/harness/phpt-runs/<tree>/<scope>.json`, in the
//! reading store, so CI's upload of the store carries them and a run CI
//! took can serve as a base (harness §4.2, #725). The key is the tree the
//! engine's binary is built from, [`built_from`], and not the tree the
//! subset's reading is stamped with: that one is the crate's own, which
//! does not move when `php_engine` does, while the verdicts do. The same
//! key is taken from a commit's objects, so the run a worktree at the
//! merge base took is found by the commit alone from any worktree of the
//! repository ([`roots`]), and a run over sources that are no commit's is
//! found by none.
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

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::miss::Miss;
use super::target::Target;
use super::{CORPUS, INSTRUMENT, Judgement, Lane, Scope, Tally};
use crate::git;
use crate::key::Snapshot;
use crate::manifest::Manifest;
use crate::oracle::{Pin, Runner};
use crate::reading::{Reading, Status, listed};
use crate::tally::Judgement as _;
use crate::track;

/// What the engine's binary is built from: the crates, extensions and
/// hosts it links ([`crate::layout::CRATE_DIRS`]), the workspace's lock,
/// and `ops.toml`, which two `build.rs` read.
fn built_from() -> Vec<PathBuf> {
    let files = ["Cargo.toml", "Cargo.lock", "ops.toml"];
    let dirs = crate::layout::CRATE_DIRS.iter().map(|d| d.dir);
    files.into_iter().chain(dirs).map(PathBuf::from).collect()
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
    tree: String,
    commit: String,
    corpus: String,
    base: Option<Base>,
}

/// The commit a run is held against: its short id and the key its runs
/// stand under.
struct Base {
    commit: String,
    tree: String,
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
        let snapshot = Snapshot::disk(root)?;
        Ok(Runs {
            root: root.to_path_buf(),
            roots: roots(root),
            tree: snapshot.tracked_over(&built_from())?,
            snapshot,
            commit: git::head_commit(root)?,
            corpus: format!("{} {}", source.tag, source.commit),
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
        let moved = Snapshot::disk(&self.root)?.tracked_over(&built_from())? != self.tree;
        if let (Some(run), false) = (&run, moved) {
            let file = path(&self.root, &self.tree, scope);
            let text = serde_json::to_string_pretty(run).context("rendering the run")?;
            // Replaced whole, as a reading's file is, so a comparison
            // reading it from another worktree never finds it cut short.
            crate::reading::replace(&file, text.as_bytes())?;
        }
        let Some(base) = &self.base else {
            return Ok(None);
        };
        // A base with no run kept is a comparison that gives no verdict and
        // never a subset left unrun: the subset's own reading stands beside
        // it, and the skip says so with its counts (#1435).
        let diff = match (base.run(&self.roots, scope)?, run) {
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
        held(manifest, &self.snapshot, held_as, &base.commit, diff).map(Some)
    }

    /// The run a tally is where the engine judged every test the subset
    /// holds, and none where anything else ran or anything was left.
    fn run_of(&self, tally: &Tally, runner: &Runner) -> Option<Run> {
        let engine = match runner {
            Runner::Engine(_) => runner.stamp()?,
            Runner::Linked => "the linked engine".to_owned(),
            _ => return None,
        };
        self.kept(tally, engine)
    }

    /// Where the engine's run of `scope` is kept after it, relative to the
    /// workspace's root, where it will be: the engine judged every test the
    /// subset holds. What a reading held to a target points to for the met
    /// tests it does not name (#2033); `after` keeps it there unless the
    /// sources moved while it ran.
    pub(crate) fn run_kept_at(
        &self,
        scope: &Scope,
        (tally, runner): (&Tally, &Runner),
    ) -> Option<PathBuf> {
        let engine = matches!(runner, Runner::Engine(_) | Runner::Linked);
        (engine && whole(tally)).then(|| {
            Path::new(RUNS)
                .join(&self.tree)
                .join(format!("{scope}.json"))
        })
    }

    /// The run a tally is where it judged every test the subset holds,
    /// named by `engine`.
    fn kept(&self, tally: &Tally, engine: String) -> Option<Run> {
        whole(tally).then(|| Run {
            commit: self.commit.clone(),
            corpus: self.corpus.clone(),
            engine,
            verdicts: tally
                .judged
                .iter()
                .map(|j| (j.id.clone(), j.judgement))
                .collect(),
            misses: tally
                .judged
                .iter()
                .filter_map(|j| Some((j.id.clone(), j.miss.clone()?)))
                .collect(),
            skips: tally
                .judged
                .iter()
                .filter(|j| j.judgement == Judgement::Skip)
                .map(|j| (j.id.clone(), j.note.clone()))
                .collect(),
        })
    }
}

impl Base {
    /// The commit `named` names in the repository at `root`, keyed by the
    /// tree its engine is built from; a name that is no commit is refused.
    fn at(root: &Path, named: &str) -> Result<Base> {
        let full = track::rev(root, named).with_context(|| format!("--against {named}"))?;
        Ok(Base {
            commit: track::short(&full).to_owned(),
            tree: Snapshot::commit(root, &full)?.tracked_over(&built_from())?,
        })
    }

    /// The run of `scope` stored at the commit, from the first of `roots`
    /// that keeps one, or why there is none, naming each root looked in.
    fn run(&self, roots: &[PathBuf], scope: &str) -> Result<Result<Run, String>> {
        if let Some(run) = read(roots, &self.tree, scope)? {
            return Ok(Ok(run));
        }
        let looked: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
        let looked = looked.join(", ");
        let Base { commit, tree } = self;
        let fetch = roots.first().map_or_else(String::new, |root| {
            format!(
                "fetch the runs CI's dashboard keeps with `{}`, or ",
                fetch(root, tree)
            )
        });
        Ok(Err(format!(
            "no run of {scope} is kept at {commit} (the engine's tree {tree}) in {looked}; {fetch}take one with `{}`, which builds the engine at {commit} in a worktree of its own and keeps the run in the main worktree",
            super::base::call(commit, scope)
        )))
    }
}

/// The name the dashboard's oracle job uploads [`RUNS`] under: every run
/// its runner keeps, each under its engine's tree (#2362).
pub(crate) const UPLOAD: &str = "phpt-runs";

/// The line that lays the runs CI kept under the engine's tree `tree` into
/// the workspace at `root`, where [`roots`] looks first (#2362): the
/// latest [`UPLOAD`] downloaded apart, since `gh` writes over no file,
/// and that tree's runs copied in where the workspace keeps none, as
/// [`carry`] copies a worktree's.
pub(crate) fn fetch(root: &Path, tree: &str) -> String {
    format!(
        "d=$(mktemp -d) && gh run download -n {UPLOAD} -D \"$d\" && rsync -a --ignore-existing \"$d/{tree}\" '{}/'",
        root.join(RUNS).display()
    )
}

/// The run of `scope` kept at `commit`'s engine tree in a workspace of
/// the repository at `root` ([`roots`]), or why there is none: what
/// `--against` holds a subset to, and what `phpt --base` keeps.
pub(crate) fn kept_at(root: &Path, commit: &str, scope: &str) -> Result<Result<Run, String>> {
    Base::at(root, commit)?.run(&roots(root), scope)
}

/// Where a kept run or target is looked for from the workspace at `root`:
/// `root`, then every worktree of its repository that stands on disk, as
/// `git worktree list` names them, the main one first; each once, by its
/// canonical path. The one rule for where a run lives: in the workspace
/// that took it ([`Runs::after`]), keyed by the tree its engine is built
/// from, so a run taken in a worktree at a base, as the skip of
/// [`Base::run`] asks, is found by the commit from every other worktree
/// while that one stands (#1869). The main worktree outlives the others,
/// so `worktree retire` carries a branch worktree's runs into it
/// ([`carry`]) before removing it.
fn roots(root: &Path) -> Vec<PathBuf> {
    let listed = track::worktrees(root).unwrap_or_default();
    let mut roots: Vec<PathBuf> = Vec::new();
    let standing = std::iter::once(root.to_path_buf())
        .chain(listed.into_iter().map(|(tree, _)| tree))
        .filter_map(|tree| fs::canonicalize(tree).ok());
    for tree in standing {
        if !roots.contains(&tree) {
            roots.push(tree);
        }
    }
    roots
}

fn path(root: &Path, tree: &str, scope: &str) -> PathBuf {
    root.join(RUNS).join(tree).join(format!("{scope}.json"))
}

/// The run of `scope` kept at the engine tree `tree`, from the first of
/// `roots` that keeps one.
fn read(roots: &[PathBuf], tree: &str, scope: &str) -> Result<Option<Run>> {
    for root in roots {
        let file = path(root, tree, scope);
        if file.is_file() {
            let text =
                fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
            let run = serde_json::from_str(&text)
                .with_context(|| format!("{} is not a run", file.display()))?;
            return Ok(Some(run));
        }
    }
    Ok(None)
}

/// Where the workspace `snapshot` stands in keeps the run of `scope` taken
/// at its engine tree, the file [`regressed`] reads as this tree's run.
pub fn kept_run(snapshot: &Snapshot, scope: &str) -> Result<PathBuf> {
    let tree = snapshot.tracked_over(&built_from())?;
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
/// `pin` where there is one.
pub(crate) fn regressed(
    snapshot: &Snapshot,
    base: &str,
    pin: Option<&Pin>,
    scope: &str,
) -> Result<Against> {
    let roots = roots(snapshot.root());
    let tree = snapshot.tracked_over(&built_from())?;
    let Some(run) = read(&roots, &tree, scope)? else {
        return Ok(Against::Unrun(format!(
            "no run of {scope} is kept at this tree's engine ({tree}); take one with `cargo xtask phpt --subset {scope}`"
        )));
    };
    let base = Base::at(snapshot.root(), base)?;
    let was = match base.run(&roots, scope)? {
        Ok(was) => was,
        Err(_) if run.verdicts.is_empty() => return Ok(Against::Held),
        Err(why) => return Ok(Against::Unmet(why)),
    };
    let target = pin
        .map(|pin| Target::kept(&roots, pin, scope))
        .transpose()?;
    let diff = match diff(&was, &run, target.flatten().as_ref()) {
        Ok(diff) if diff.regressed.is_empty() => return Ok(Against::Held),
        Ok(diff) => diff,
        Err(why) => {
            return Ok(Against::Unmet(format!(
                "{scope}: no verdict against {}; {why}",
                base.commit
            )));
        }
    };
    let prefix = format!("phpt:{CORPUS}/");
    let names: Vec<&str> = diff
        .regressed
        .iter()
        .map(|(id, _)| id.strip_prefix(&prefix).unwrap_or(id))
        .collect();
    Ok(Against::Unmet(format!(
        "{scope}: {} of {} tests met at {} are not met here: {}",
        names.len(),
        diff.held,
        base.commit,
        listed(&names)
    )))
}

/// Whether a tally judged every test its subset holds, which a run must
/// to be kept. A subset that holds none in the pinned tree, as an
/// extension with no tests of its own does (`ext/lexbor`), is kept as a
/// run of none, which holds nothing against any base (#725).
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
/// tree; one it keeps already is left standing. A run is keyed by the tree
/// the engine's binary is built from, so a run taken anywhere at that tree
/// is the same run, and the one standing is as good as the one carried. What
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
        if to.exists() {
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

/// The comparison's reading over `scope`, at the scope the subset's
/// reading has, [`Scope::held_against`].
fn held(
    manifest: &Manifest,
    snapshot: &Snapshot,
    scope: &Scope,
    commit: &str,
    diff: Result<Diff, String>,
) -> Result<Reading> {
    let scope = scope.held_against().to_string();
    let reading = Reading::for_scope(INSTRUMENT, &scope, manifest, snapshot)?.under(&manifest.pin);
    compared(reading, commit, diff)
}

/// The words, the counts and the status of one comparison: fail while a
/// test met at `commit` is not met here, a skip that says why where no
/// verdict is given, and a pass otherwise.
pub(crate) fn compared(
    mut reading: Reading,
    commit: &str,
    diff: Result<Diff, String>,
) -> Result<Reading> {
    let scope = reading.scope.clone();
    let diff = match diff {
        Ok(diff) if diff.held == 0 => Err(format!(
            "no test met its expectation at {commit}, so nothing is held"
        )),
        diff => diff,
    };
    let diff = match diff {
        Ok(diff) => diff,
        Err(why) => {
            reading.status = Status::Skip;
            reading.message = format!("{scope}: no verdict against {commit}; {why}");
            return Ok(reading);
        }
    };
    let prefix = format!("phpt:{CORPUS}/");
    let names: Vec<&str> = diff
        .regressed
        .iter()
        .map(|(id, now)| {
            println!("regressed {id}: met at {commit}, {} here", now.as_str());
            id.strip_prefix(&prefix).unwrap_or(id)
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
        "{scope}: of {} tests met at {commit}, {lost}; {} newly met",
        diff.held, diff.fixed
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
