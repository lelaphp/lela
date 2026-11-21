//! The tally behind the behaviour harness's lanes (harness-design.md §4.2,
//! §4.3, §7): what `phpt`, `witness` and `compile-identity` hold in common
//! once each has said what one item is and how it is judged. A tally is the verdicts of one
//! run of a corpus through the [`Runner`], and what follows from them: the
//! print loop, the counts, the runner ladder and the reading's status. A
//! lane is a scheduled run of a corpus, the per-commit subsets or the
//! nightly whole (harness §7), and never this module; a lane contributes
//! its [`Judgement`], what it keeps of an item that missed, a per-item
//! judge and the words of its message, and nothing else.
//!
//! **The runner ladder.** What ran decides what a green tally means. A
//! [`Runner::Control`] is the pinned oracle standing where the engine
//! belongs: its reading is a skip whatever the items say, a control of the
//! lane and never a reading of the engine. A [`Runner::Absent`] is a skip
//! that says why, and so is a [`Runner::Stale`], the binary the tree has
//! moved past, whose reading would otherwise be the engine of another tree
//! stamped with this one. The engine alone is pass or fail, and it is the engine
//! whether the binary served the program in a process of its own or the
//! linked engine served it in this one ([`Runner::Linked`]): the two are
//! the adapters of one seam over one executor (design §8).
//!
//! **Nothing ran is not green.** Under the engine, a tally where nothing
//! met its expectation is a skip and not a pass: an empty subset, or one
//! every test skipped out of, and a green reading there would say the
//! engine was measured when it was not. The ladder holds this once, here,
//! and [`Outcome::NothingRan`] is the rung. A lane whose empty tally is a
//! measurement of its own, the witness lane over a crate with no witness
//! and nothing that needs one, has a rung of its own ahead of the ladder,
//! [`Tally::complete`]: green there means the pinned oracle was here to
//! have run what there was none of, and without it the rung is
//! [`Outcome::OracleAbsent`], a skip that says why, since a green line
//! without the oracle is a claim. The same rung holds a tally whose items
//! were found and none run because the oracle was not here to run them:
//! coverage is the lane's and is judged first, and a snippet that did not
//! run is counted, never a refusal.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::num::NonZeroUsize;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, PoisonError, mpsc};
use std::thread;

use anyhow::{Result, anyhow};

use crate::oracle::Runner;
use crate::reading::{Reading, Status, listed};

/// The key of a lane's reading that counts the items judged.
pub(crate) const RUN: &str = "run";

/// What one item of a lane came to. A lane's enum carries the variants
/// upstream gives it; the tally reads three things of any of them.
pub trait Judgement: Copy + Eq + fmt::Debug {
    /// What the lane keeps of an item that missed, beside its note: the
    /// phpt lane's first differing lines and signature. A lane
    /// that keeps nothing more than the note names `Infallible`.
    type Miss: Clone + Eq + fmt::Debug + Send;
    /// The one judgement the print loop leaves out.
    const PASS: Self;
    fn as_str(self) -> &'static str;
    /// Whether the reading fails on it.
    fn counts_against(self) -> bool;
    /// Whether the item met what it was held to: it passed, or it differs
    /// and an accepted registry row says so.
    fn met(self) -> bool;
}

/// One item's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judged<J: Judgement> {
    /// The divergence key, whatever the judgement.
    pub id: String,
    /// What the reading names the item by when it counts against.
    pub name: String,
    /// What the printed line adds after the id when the id alone does not
    /// place the item: empty for a corpus path, ` at ext/pcre/src/lib.rs:31,
    /// pub fn preg_match` for a witness.
    pub place: String,
    pub judgement: J,
    /// Why: the skip's reason, the borkage, the accepted row's reason, or
    /// the difference.
    pub note: String,
    /// What the lane keeps of the miss, decoded where the verdict was
    /// made; none where the item did not miss or the lane keeps nothing.
    pub miss: Option<J::Miss>,
}

impl<J: Judgement> Judged<J> {
    pub fn new(id: &str, name: &str, judgement: J, note: impl Into<String>) -> Judged<J> {
        Judged {
            id: id.to_owned(),
            name: name.to_owned(),
            place: String::new(),
            judgement,
            note: note.into(),
            miss: None,
        }
    }

    /// The verdict with what the lane keeps of its miss.
    pub fn keeping(mut self, miss: J::Miss) -> Judged<J> {
        self.miss = Some(miss);
        self
    }
}

/// What one run of a corpus came to: the verdicts, and the counts that
/// read off them.
#[derive(Debug, Clone)]
pub struct Tally<J: Judgement> {
    pub judged: Vec<Judged<J>>,
    /// The items found, which is more than `judged` when nothing ran.
    pub found: usize,
}

impl<J: Judgement> Default for Tally<J> {
    fn default() -> Tally<J> {
        Tally {
            judged: Vec::new(),
            found: 0,
        }
    }
}

/// Where a run stands on the ladder, and so what status its reading has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome<'a> {
    /// Something counts against the reading.
    Failed,
    /// Nothing ran, and why.
    Absent(&'a str),
    /// The pinned oracle ran as the engine: a control of the lane.
    Control(&'a Path),
    /// The engine ran and nothing met its expectation.
    NothingRan,
    /// Nothing ran, and the pinned oracle was not here to have run it,
    /// whether nothing was found or what was found could not run: the
    /// green a complete tally would read is a claim without the oracle,
    /// and this is the skip that says why.
    OracleAbsent(&'a str),
    /// The engine ran and every item met its expectation.
    Passed,
}

impl Outcome<'_> {
    pub fn status(&self) -> Status {
        match self {
            Outcome::Failed => Status::Fail,
            Outcome::Absent(_)
            | Outcome::Control(_)
            | Outcome::NothingRan
            | Outcome::OracleAbsent(_) => Status::Skip,
            Outcome::Passed => Status::Pass,
        }
    }
}

impl<J: Judgement> Tally<J> {
    /// The items a run found and judged none of, since nothing could run
    /// them.
    pub fn found(found: usize) -> Tally<J> {
        Tally {
            judged: Vec::new(),
            found,
        }
    }

    /// Every item through the judge, each verdict printed as it is reached
    /// unless it passed. A failure prints its difference: that is what a
    /// person reads to decide whether it is the engine's or the corpus's.
    /// Every other judgement is one line, its reason. The line is keyed by
    /// the divergence id, which is what a person writes a registry row
    /// against when the difference is one to accept (harness §4.3).
    pub fn judge<I>(
        items: &[I],
        mut judge: impl FnMut(&I) -> Result<Judged<J>>,
    ) -> Result<Tally<J>> {
        let mut tally = Tally::found(items.len());
        for item in items {
            tally.record(judge(item)?);
        }
        Ok(tally)
    }

    /// [`Tally::judge_apart`] on the seat's share of the machine, for a
    /// lane whose items share nothing: each witness runs in a scratch tree
    /// of its own, and most of its wall is the oracle's process starting,
    /// which a thread spends waiting.
    pub fn judge_beside<I: Sync>(
        items: &[I],
        judge: impl Fn(&I) -> Result<Judged<J>> + Sync,
    ) -> Result<Tally<J>>
    where
        J: Send,
    {
        let apart = vec![Apart::default(); items.len()];
        Tally::judge_apart(items, crate::seat::jobs(), NonZeroUsize::MIN, &apart, judge)
    }

    /// [`Tally::judge`] on [`pool`]'s threads, each item held apart from
    /// the items `apart[n]` says it may not run beside and in batches of
    /// `batch`. The verdicts are printed and kept in the items' order all
    /// the same, so the run reads as the one-at-a-time run does, and a
    /// pool of one is that run.
    pub fn judge_apart<I: Sync>(
        items: &[I],
        threads: NonZeroUsize,
        batch: NonZeroUsize,
        apart: &[Apart],
        judge: impl Fn(&I) -> Result<Judged<J>> + Sync,
    ) -> Result<Tally<J>>
    where
        J: Send,
    {
        let mut tally = Tally::found(items.len());
        pool(items, threads, batch, apart, judge, |judged| {
            tally.record(judged);
        })?;
        Ok(tally)
    }

    /// One verdict kept, and printed unless it passed.
    fn record(&mut self, judged: Judged<J>) {
        if judged.judgement != J::PASS {
            let note = if judged.judgement.counts_against() {
                judged.note.clone()
            } else {
                judged.note.lines().next().unwrap_or_default().to_owned()
            };
            println!(
                "{} {}{}: {note}",
                judged.judgement.as_str(),
                judged.id,
                judged.place
            );
        }
        self.judged.push(judged);
    }

    pub fn count(&self, judgement: J) -> u64 {
        self.judged
            .iter()
            .filter(|j| j.judgement == judgement)
            .count() as u64
    }

    /// The items found and not run: what the reading counts when the
    /// oracle was not here to run them.
    pub fn not_run(&self) -> u64 {
        self.found.saturating_sub(self.judged.len()) as u64
    }

    /// Whether the run judged every item it found: the one rule a subset's
    /// run is kept by and a corpus run is whole by.
    pub fn whole(&self) -> bool {
        self.judged.len() == self.found
    }

    /// The items that met what they were held to.
    pub fn met(&self) -> u64 {
        self.judged.iter().filter(|j| j.judgement.met()).count() as u64
    }

    /// What met of what was found, to one decimal: the burndown's number,
    /// read off one reading. Nothing found is nothing met.
    pub fn percent(&self) -> f64 {
        if self.found == 0 {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)]
        let ratio = (self.met() as f64 / self.found as f64) * 1000.0;
        ratio.round() / 10.0
    }

    /// The items that fail the reading, by name.
    pub fn against(&self) -> Vec<&str> {
        self.judged
            .iter()
            .filter(|j| j.judgement.counts_against())
            .map(|j| j.name.as_str())
            .collect()
    }

    /// The names against, as a message's tail: `: a, b, c`, and nothing
    /// when none is.
    pub fn missed(&self) -> String {
        let against = self.against();
        if against.is_empty() {
            String::new()
        } else {
            format!(": {}", listed(&against))
        }
    }

    /// The ladder's rung for what ran. A lane with a rung of its own ahead
    /// of the ladder, a directory the pinned tree has not or a kernel
    /// without a witness, decides that before asking.
    pub fn outcome<'a>(&self, runner: &'a Runner) -> Outcome<'a> {
        match runner {
            // A binary the tree has moved past ran nothing, as an absent
            // one did, and the reading's numbers say which of the two it
            // was: `done` excuses the missing binary and not this.
            Runner::Absent(why) | Runner::Stale(why) => Outcome::Absent(why),
            Runner::Control(binary) => Outcome::Control(binary),
            Runner::Engine(..) | Runner::Linked if !self.against().is_empty() => Outcome::Failed,
            Runner::Engine(..) | Runner::Linked if self.met() == 0 => Outcome::NothingRan,
            Runner::Engine(..) | Runner::Linked => Outcome::Passed,
        }
    }

    /// The rung ahead of the ladder for a lane whose tally with nothing
    /// run is a measurement of its own: a crate with no witness and no
    /// kernel or native that needs one is complete, whatever stands in
    /// for the engine. Green means the oracle agreed, so the pass stands
    /// only while the pinned oracle is here to have run what there was
    /// none of; `absent` is why it is not, and then the rung is a skip
    /// that says so, whether nothing was found or what was found did not
    /// run for want of the oracle (`not_run` counts those). A tally that
    /// ran anything ran it under the oracle, and the word is the ladder's.
    pub fn complete<'a>(&self, absent: Option<&'a str>) -> Outcome<'a> {
        match absent {
            Some(why) if self.judged.is_empty() => Outcome::OracleAbsent(why),
            _ => Outcome::Passed,
        }
    }

    /// The reading the lane constructed, stamped over its scope, with
    /// `run` in its numbers, the rung's status, and the lane's words. The
    /// lane adds its own counts after.
    pub fn reading(
        &self,
        mut reading: Reading,
        outcome: &Outcome<'_>,
        message: String,
    ) -> Result<Reading> {
        reading.count(RUN, self.judged.len() as u64)?;
        reading.status = outcome.status();
        reading.message = message;
        Ok(reading)
    }
}

/// What one item may not run beside in its run: upstream's conflict keys
/// (run-tests.php:1341-1372). No two items holding one key run at once,
/// and an item alone runs with nothing of its run beside it; an item
/// holding no key runs beside anything but an item alone. Two runs share
/// no tree to be held apart in: a phpt run stands in a copy of the pinned
/// tree of its own, and a witness in a scratch tree of its own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Apart {
    pub within: Within,
    pub keys: Vec<String>,
}

/// How an item stands among the items of its own run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Within {
    /// In a batch of its neighbours, beside any item its keys allow.
    #[default]
    Batch,
    /// Upstream's `all`: in no batch, and nothing of its run beside it.
    Alone,
}

/// `each` over every item on the seat's share of the machine, the
/// answers in the items' order: for a lane whose items share nothing and
/// spend their wall waiting on processes, as `crosscheck`'s renderings do.
pub(crate) fn each_beside<T: Sync, R: Send>(
    items: &[T],
    each: impl Fn(&T) -> R + Sync,
) -> Result<Vec<R>> {
    let mut answers = Vec::with_capacity(items.len());
    let apart = vec![Apart::default(); items.len()];
    let (each, batch) = (|item: &T| Ok(each(item)), NonZeroUsize::MIN);
    let jobs = crate::seat::jobs();
    pool(items, jobs, batch, &apart, each, |r| answers.push(r))?;
    Ok(answers)
}

/// `each` over every item on a pool of `threads`, each item held apart
/// from the items `apart[n]` says it may not run beside, as [`Queue::take`]
/// holds them. The items are cut into batches of `batch` neighbours in
/// their order, and a batch's items are taken one after another, as one
/// of upstream's workers runs the batch it is handed
/// (run-tests.php:1742-1744, 1279-1325); the batches run beside each
/// other, and a batch of one holds nothing. An item alone is in no batch
/// and is not counted in one. `on` is handed each answer on the calling
/// thread in the items' order, each once every one before it is in. An
/// item whose `each` fails or panics ends the run at its place: its error
/// is the answer, and nothing more starts.
pub(crate) fn pool<I: Sync, R: Send>(
    items: &[I],
    threads: NonZeroUsize,
    batch: NonZeroUsize,
    apart: &[Apart],
    each: impl Fn(&I) -> Result<R> + Sync,
    mut on: impl FnMut(R),
) -> Result<()> {
    let queue = Mutex::new(Queue::new(apart, batch));
    let freed = Condvar::new();
    let (sent, received) = mpsc::channel();
    thread::scope(|scope| {
        for _ in 0..threads.get().min(items.len()) {
            let (sent, queue, freed, each) = (sent.clone(), &queue, &freed, &each);
            scope.spawn(move || {
                let lock = || crate::locked(queue);
                loop {
                    let mut waiting = lock();
                    let at = loop {
                        if let Some(at) = waiting.take(apart) {
                            break at;
                        }
                        if waiting.waiting.is_empty() {
                            return;
                        }
                        waiting = freed.wait(waiting).unwrap_or_else(PoisonError::into_inner);
                    };
                    drop(waiting);
                    // An item that panics gives its keys back all the
                    // same, so no thread waits on them for ever.
                    let answer = panic::catch_unwind(AssertUnwindSafe(|| each(&items[at])))
                        .unwrap_or_else(|_| Err(anyhow!("item {at} panicked")));
                    lock().done(at, &apart[at]);
                    freed.notify_all();
                    // A send that fails is the receiver gone, an answer
                    // before this one having ended the run: nothing more
                    // starts.
                    if sent.send((at, answer)).is_err() {
                        lock().waiting.clear();
                        freed.notify_all();
                        return;
                    }
                }
            });
        }
        drop(sent);
        let mut next = 0;
        let mut early = BTreeMap::new();
        for (at, answer) in received {
            early.insert(at, answer);
            while let Some(answer) = early.remove(&next) {
                on(answer?);
                next += 1;
            }
        }
        Ok(())
    })
}

/// The items not yet started and what the running ones hold: where the
/// threads of [`pool`] take their next item from.
#[derive(Debug, Default)]
struct Queue {
    /// The items not yet started, in their order.
    waiting: Vec<usize>,
    /// The batch of each item, by its place; none for an item alone.
    batch: Vec<Option<usize>>,
    /// The keys the running items hold.
    held: BTreeSet<String>,
    /// The batches an item of which is running.
    busy: BTreeSet<usize>,
    running: usize,
    /// Whether the item running is one that runs alone.
    alone: bool,
}

impl Queue {
    /// Every item waiting, each item not alone in the batch its place
    /// among those items falls in.
    fn new(apart: &[Apart], batch: NonZeroUsize) -> Queue {
        let mut placed = 0;
        let batch = apart
            .iter()
            .map(|apart| match apart.within {
                Within::Alone => None,
                Within::Batch => {
                    placed += 1;
                    Some((placed - 1) / batch)
                }
            })
            .collect();
        Queue {
            waiting: (0..apart.len()).collect(),
            batch,
            ..Queue::default()
        }
    }

    /// The next item that may start now: the first waiting whose batch
    /// has no item running and whose keys neither a running item nor an
    /// earlier waiting item holds, so a batch's items and the items
    /// sharing a key start in their order, which upstream keeps for them
    /// (run-tests.php:1374-1380). An item a key holds back lets the rest
    /// of its batch go on without it, as upstream sets it aside out of
    /// the batch it was taking (run-tests.php:1541-1546). An item alone
    /// starts when it is first in line and nothing runs, and nothing
    /// after it starts first: the items before it are done before it and
    /// the ones after it wait for it, as a run of one would have it.
    /// Upstream runs those last instead, which no verdict can tell from
    /// this.
    fn take(&mut self, apart: &[Apart]) -> Option<usize> {
        if self.alone {
            return None;
        }
        let mut passed: Vec<&str> = Vec::new();
        for (n, &at) in self.waiting.iter().enumerate() {
            let batch = self.batch[at];
            let keys = &apart[at].keys;
            match apart[at].within {
                Within::Alone if n == 0 && self.running == 0 => self.alone = true,
                Within::Alone => return None,
                Within::Batch
                    if batch.is_some_and(|batch| self.busy.contains(&batch))
                        || keys.iter().any(|key| {
                            self.held.contains(key) || passed.contains(&key.as_str())
                        }) =>
                {
                    passed.extend(keys.iter().map(String::as_str));
                    continue;
                }
                Within::Batch => self.held.extend(keys.iter().cloned()),
            }
            self.busy.extend(batch);
            self.running += 1;
            return Some(self.waiting.remove(n));
        }
        None
    }

    /// The item at `at` done: what it held given back, its batch's among
    /// it.
    fn done(&mut self, at: usize, apart: &Apart) {
        self.running -= 1;
        if let Some(batch) = self.batch[at] {
            self.busy.remove(&batch);
        }
        match apart.within {
            Within::Alone => self.alone = false,
            Within::Batch => {
                for key in &apart.keys {
                    self.held.remove(key);
                }
            }
        }
    }
}

/// Every file under `dir`, recursively, in no order: what a lane walks a
/// corpus with, whether the corpus is the pinned tree's subsets or a
/// directory of files. A directory that is not there has none, since
/// whether the corpus is where it should be is the lane's sentence and
/// not this walk's; one that cannot be listed is the error.
pub fn files_under(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for path in crate::entries(dir)? {
        if path.is_dir() {
            files_under(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

/// Whether a file `pick` takes stands anywhere under `dir`, walked as
/// [`files_under`] walks it and read up to the first one found.
pub(crate) fn any_file_under(dir: &Path, pick: fn(&Path) -> bool) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if path.is_dir() {
            any_file_under(&path, pick)
        } else {
            pick(&path)
        }
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Instant;

    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Went {
        Pass,
        Diverged,
        Fail,
        Skip,
    }

    impl Judgement for Went {
        type Miss = std::convert::Infallible;
        const PASS: Went = Went::Pass;

        fn as_str(self) -> &'static str {
            match self {
                Went::Pass => "pass",
                Went::Diverged => "diverged",
                Went::Fail => "fail",
                Went::Skip => "skip",
            }
        }

        fn counts_against(self) -> bool {
            self == Went::Fail
        }

        fn met(self) -> bool {
            matches!(self, Went::Pass | Went::Diverged)
        }
    }

    fn tally(went: &[Went]) -> Tally<Went> {
        let judged = went
            .iter()
            .enumerate()
            .map(|(i, went)| Judged::new(&format!("id{i}"), &format!("item{i}"), *went, ""))
            .collect::<Vec<_>>();
        Tally {
            found: judged.len(),
            judged,
        }
    }

    fn engine() -> Runner {
        Runner::Engine(PathBuf::from("target/debug/php"), None)
    }

    #[test]
    fn nothing_ran_is_not_green() {
        // Every item skipped out, or none was found: the engine was not
        // measured, and the rung is a skip, never a pass.
        assert_eq!(
            tally(&[Went::Skip, Went::Skip]).outcome(&engine()),
            Outcome::NothingRan
        );
        assert_eq!(
            Tally::<Went>::found(3).outcome(&engine()),
            Outcome::NothingRan
        );
        assert_eq!(
            Tally::<Went>::default().outcome(&engine()),
            Outcome::NothingRan
        );
        assert_eq!(Outcome::NothingRan.status(), Status::Skip);
        // One item met, by passing or by an accepted difference, and the
        // engine was measured.
        assert_eq!(
            tally(&[Went::Skip, Went::Pass]).outcome(&engine()),
            Outcome::Passed
        );
        assert_eq!(tally(&[Went::Diverged]).outcome(&engine()), Outcome::Passed);
    }

    #[test]
    fn the_ladder_lets_the_engine_alone_pass_or_fail() {
        let control = Runner::Control(PathBuf::from("php"));
        let absent = Runner::Absent("not built".to_owned());
        let failed = tally(&[Went::Pass, Went::Fail]);
        assert_eq!(failed.outcome(&engine()), Outcome::Failed);
        assert_eq!(failed.outcome(&control), Outcome::Control(Path::new("php")));
        assert_eq!(failed.outcome(&absent), Outcome::Absent("not built"));
        assert_eq!(Outcome::Failed.status(), Status::Fail);
        assert_eq!(Outcome::Control(Path::new("php")).status(), Status::Skip);
        assert_eq!(Outcome::Absent("not built").status(), Status::Skip);
        assert_eq!(Outcome::Passed.status(), Status::Pass);
    }

    #[test]
    fn a_complete_tally_with_nothing_run_is_green_only_with_the_oracle() {
        // Nothing to run and the pinned oracle here to have run it: the
        // measurement is complete, whatever runner stands in for the
        // engine. Without the oracle the same green would be a claim, and
        // the rung is a skip that says why.
        let empty = Tally::<Went>::default();
        assert_eq!(empty.complete(None), Outcome::Passed);
        assert_eq!(
            empty.complete(Some("not at /pinned/php")),
            Outcome::OracleAbsent("not at /pinned/php")
        );
        assert_eq!(
            Outcome::OracleAbsent("not at /pinned/php").status(),
            Status::Skip
        );
        // The rung is the empty tally's alone: one that ran anything was
        // run by the oracle and stands on the ladder.
        assert_eq!(
            tally(&[Went::Pass]).complete(Some("not at /pinned/php")),
            Outcome::Passed
        );
        // Items found and none run, since the oracle was not here to run
        // them: the same rung, with the count of what did not run on it.
        // Coverage is the lane's and was judged before this.
        assert_eq!(
            Tally::<Went>::found(3).complete(Some("not at /pinned/php")),
            Outcome::OracleAbsent("not at /pinned/php")
        );
        assert_eq!(Tally::<Went>::found(3).not_run(), 3);
        assert_eq!(tally(&[Went::Pass]).not_run(), 0);
    }

    #[test]
    fn the_counts_and_the_names_against_read_off_the_judgements() {
        let mixed = tally(&[
            Went::Pass,
            Went::Fail,
            Went::Diverged,
            Went::Fail,
            Went::Skip,
        ]);
        assert_eq!(mixed.count(Went::Fail), 2);
        assert_eq!(mixed.met(), 2);
        assert_eq!(mixed.against(), ["item1", "item3"]);
        assert_eq!(mixed.missed(), ": item1, item3");
        assert_eq!(tally(&[Went::Pass]).missed(), "");
    }

    #[test]
    fn the_judge_loop_keeps_every_verdict_in_order_and_counts_what_it_found() {
        let items = [Went::Fail, Went::Pass, Went::Skip];
        let tally =
            Tally::judge(&items, |went| Ok(Judged::new("id", "item", *went, "why"))).unwrap();
        assert_eq!(tally.found, 3);
        let went: Vec<Went> = tally.judged.iter().map(|j| j.judgement).collect();
        assert_eq!(went, items);
        let reading = tally
            .reading(
                Reading::new("tally", "ext/fixture"),
                &tally.outcome(&engine()),
                "words".to_owned(),
            )
            .unwrap();
        assert_eq!(reading.status, Status::Fail);
        assert_eq!(reading.message, "words");
        assert_eq!(reading.numbers["run"], crate::reading::Number::Int(3));
    }

    /// The verdicts come back in the items' order however the threads
    /// finish them: here the earlier items take longest.
    #[test]
    fn beside_keeps_the_items_order_whichever_thread_finishes_first() {
        let items: Vec<(usize, Went)> = (0..64)
            .map(|i| (i, [Went::Fail, Went::Pass, Went::Skip][i % 3]))
            .collect();
        let tally = Tally::judge_beside(&items, |(i, went)| {
            std::thread::sleep(std::time::Duration::from_millis(64 - *i as u64));
            Ok(Judged::new(&i.to_string(), "item", *went, "why"))
        })
        .unwrap();
        assert_eq!(tally.found, 64);
        let ids: Vec<String> = tally.judged.iter().map(|j| j.id.clone()).collect();
        let want: Vec<String> = (0..64).map(|i| i.to_string()).collect();
        assert_eq!(ids, want);
    }

    /// Items judged on a pool of `threads` in batches of `batch`, each
    /// holding what `apart` gives it: what ran at once, as spans between
    /// two instants, one per item in the items' order, and the order items
    /// started.
    fn spans(
        apart: &[Apart],
        threads: usize,
        batch: usize,
        took: impl Fn(usize) -> u64 + Sync,
    ) -> (Vec<(Instant, Instant)>, Vec<usize>) {
        let at = Instant::now;
        let (spans, started) = (
            Mutex::new(vec![(at(), at()); apart.len()]),
            Mutex::new(Vec::new()),
        );
        let items: Vec<usize> = (0..apart.len()).collect();
        let tally = Tally::judge_apart(
            &items,
            NonZeroUsize::new(threads).unwrap(),
            NonZeroUsize::new(batch).unwrap(),
            apart,
            |n| {
                started.lock().unwrap().push(*n);
                let start = at();
                thread::sleep(std::time::Duration::from_millis(took(*n)));
                spans.lock().unwrap()[*n] = (start, at());
                Ok(Judged::new(&n.to_string(), "item", Went::Pass, ""))
            },
        )
        .unwrap();
        let ids: Vec<String> = tally.judged.iter().map(|j| j.id.clone()).collect();
        let want: Vec<String> = items.iter().map(ToString::to_string).collect();
        assert_eq!(ids, want, "the verdicts are in the items' order");
        (spans.into_inner().unwrap(), started.into_inner().unwrap())
    }

    fn overlap(a: (Instant, Instant), b: (Instant, Instant)) -> bool {
        a.0 < b.1 && b.0 < a.1
    }

    fn keys(keys: &[&str]) -> Apart {
        Apart {
            keys: keys.iter().map(|k| (*k).to_owned()).collect(),
            ..Apart::default()
        }
    }

    fn alone() -> Apart {
        Apart {
            within: Within::Alone,
            ..Apart::default()
        }
    }

    /// Two items naming one key never run at once, and start in their
    /// order; items naming none run beside them, which is the control
    /// that shows the pool runs anything at once at all.
    #[test]
    fn items_sharing_a_key_never_run_at_once_and_keep_their_order() {
        let apart = [
            keys(&["disk"]),
            keys(&[]),
            keys(&["disk", "net"]),
            keys(&[]),
            keys(&["net"]),
        ];
        let (spans, started) = spans(&apart, 8, 1, |_| 60);
        assert!(!overlap(spans[0], spans[2]), "{spans:?}");
        assert!(!overlap(spans[2], spans[4]), "{spans:?}");
        assert!(
            spans[0].1 <= spans[2].0 && spans[2].1 <= spans[4].0,
            "{spans:?}"
        );
        assert!(overlap(spans[1], spans[3]), "the control: {spans:?}");
        assert!(overlap(spans[0], spans[1]), "the control: {spans:?}");
        let order = |n| started.iter().position(|s| *s == n).unwrap();
        assert!(order(0) < order(2) && order(2) < order(4), "{started:?}");
    }

    /// An item alone runs with nothing beside it: everything before it is
    /// done when it starts, and nothing after it starts until it is done.
    #[test]
    fn an_item_alone_runs_with_nothing_beside_it() {
        let apart = [keys(&[]), keys(&[]), alone(), keys(&[]), keys(&[])];
        let (spans, _) = spans(&apart, 8, 1, |_| 40);
        for (n, span) in spans.iter().enumerate().filter(|(n, _)| *n != 2) {
            assert!(!overlap(spans[2], *span), "{n}: {spans:?}");
        }
        assert!(overlap(spans[0], spans[1]), "the control: {spans:?}");
        assert!(overlap(spans[3], spans[4]), "the control: {spans:?}");
    }

    /// A pool of one is the one-at-a-time run: each item starts when the
    /// one before it is done, in the items' order.
    #[test]
    fn a_pool_of_one_runs_the_items_one_at_a_time_in_order() {
        let apart = [keys(&["a"]), keys(&[]), alone(), keys(&[])];
        let (spans, started) = spans(&apart, 1, 1, |_| 5);
        assert_eq!(started, [0, 1, 2, 3]);
        assert!(spans.windows(2).all(|w| w[0].1 <= w[1].0), "{spans:?}");
    }

    /// The items of one batch run one after another and in their order,
    /// as one of upstream's workers runs the batch it is handed; the
    /// batches run beside each other, which is the control that shows the
    /// pool runs anything at once at all.
    #[test]
    fn a_batchs_items_run_one_after_another_and_the_batches_beside_each_other() {
        let apart = vec![keys(&[]); 6];
        let (spans, started) = spans(&apart, 8, 3, |_| 40);
        for batch in [&spans[0..3], &spans[3..6]] {
            assert!(batch.windows(2).all(|w| w[0].1 <= w[1].0), "{spans:?}");
        }
        let order = |n| started.iter().position(|s| *s == n).unwrap();
        assert!(order(0) < order(1) && order(1) < order(2), "{started:?}");
        assert!(overlap(spans[0], spans[3]), "the control: {spans:?}");
    }

    /// An item alone is in no batch and is not counted in one, as upstream
    /// takes it out of the list it cuts its batches from
    /// (run-tests.php:1365-1370).
    #[test]
    fn an_item_alone_is_in_no_batch_and_the_batches_count_past_it() {
        let apart = [
            keys(&[]),
            keys(&[]),
            alone(),
            keys(&[]),
            keys(&[]),
            keys(&[]),
        ];
        let queue = Queue::new(&apart, NonZeroUsize::new(2).unwrap());
        assert_eq!(
            queue.batch,
            [Some(0), Some(0), None, Some(1), Some(1), Some(2)]
        );
    }

    /// An item a key holds back lets the rest of its batch go on without
    /// it, and runs once both the key and its batch are free.
    #[test]
    fn an_item_a_key_holds_back_lets_its_batch_go_on() {
        // Batch 0 is items 0 and 1, batch 1 is items 2 and 3; items 0 and
        // 2 share a key.
        let apart = [keys(&["k"]), keys(&[]), keys(&["k"]), keys(&[])];
        let mut queue = Queue::new(&apart, NonZeroUsize::new(2).unwrap());
        assert_eq!(queue.take(&apart), Some(0));
        assert_eq!(queue.take(&apart), Some(3), "2 held back, 3 goes on");
        assert_eq!(queue.take(&apart), None, "1 waits for its batch");
        queue.done(0, &apart[0]);
        assert_eq!(queue.take(&apart), Some(1));
        assert_eq!(queue.take(&apart), None, "2 waits for its batch");
        queue.done(3, &apart[3]);
        assert_eq!(queue.take(&apart), Some(2));
    }

    /// An item that takes long holds up only the items it conflicts with:
    /// the rest are done before it is, while the one sharing its key waits.
    #[test]
    fn a_long_item_holds_up_only_what_it_conflicts_with() {
        let apart = [
            keys(&["slow"]),
            keys(&[]),
            keys(&[]),
            keys(&["slow"]),
            keys(&[]),
        ];
        let (spans, _) = spans(&apart, 2, 1, |n| if n == 0 { 400 } else { 20 });
        for n in [1, 2, 4] {
            assert!(spans[n].1 < spans[0].1, "{n}: {spans:?}");
        }
        assert!(spans[3].0 >= spans[0].1, "{spans:?}");
    }

    /// An item the judge cannot judge ends the run with its error, as the
    /// one-at-a-time loop's `?` does, whatever came after it.
    #[test]
    fn beside_ends_on_the_first_error_in_the_items_order() {
        let items: Vec<usize> = (0..32).collect();
        let err = Tally::judge_beside(&items, |i| match i {
            5 | 20 => anyhow::bail!("item {i}"),
            _ => Ok(Judged::new("id", "item", Went::Pass, "")),
        })
        .map(|_| ())
        .unwrap_err();
        assert_eq!(err.to_string(), "item 5");
    }
}
