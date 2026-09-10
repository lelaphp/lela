//! The tally behind the behaviour harness's lanes (harness-design.md §4.2,
//! §4.3, §7): what `phpt` and `witness` hold in common once each has said
//! what one item is and how it is judged. A tally is the verdicts of one
//! run of a corpus through the [`Runner`], and what follows from them: the
//! print loop, the counts, the runner ladder and the reading's status. A
//! lane is a scheduled run of a corpus, the per-commit subsets or the
//! nightly whole (harness §7), and never this module; a lane contributes
//! its [`Judgement`], a per-item judge and the words of its message, and
//! nothing else. The compile-identity lane, when it comes, is the third
//! caller.
//!
//! **The runner ladder.** What ran decides what a green tally means. A
//! [`Runner::Control`] is the pinned oracle standing where the engine
//! belongs: its reading is a skip whatever the items say, a control of the
//! lane and never a reading of the engine. A [`Runner::Absent`] is a skip
//! that says why. The engine alone is pass or fail.
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
//! run is counted, never a refusal (#65).

use std::fmt;
use std::path::Path;

use anyhow::Result;

use crate::oracle::Runner;
use crate::reading::{Reading, Status, listed};

/// What one item of a lane came to. A lane's enum carries the variants
/// upstream gives it; the tally reads three things of any of them.
pub trait Judgement: Copy + Eq + fmt::Debug {
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
pub struct Judged<J> {
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
}

impl<J> Judged<J> {
    pub fn new(id: &str, name: &str, judgement: J, note: impl Into<String>) -> Judged<J> {
        Judged {
            id: id.to_owned(),
            name: name.to_owned(),
            place: String::new(),
            judgement,
            note: note.into(),
        }
    }
}

/// What one run of a corpus came to: the verdicts, and the counts that
/// read off them.
#[derive(Debug, Clone)]
pub struct Tally<J> {
    pub judged: Vec<Judged<J>>,
    /// The items found, which is more than `judged` when nothing ran.
    pub found: usize,
}

impl<J> Default for Tally<J> {
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
            let judged = judge(item)?;
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
            tally.judged.push(judged);
        }
        Ok(tally)
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

    /// The items that met what they were held to.
    pub fn met(&self) -> u64 {
        self.judged.iter().filter(|j| j.judgement.met()).count() as u64
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
            Runner::Absent(why) => Outcome::Absent(why),
            Runner::Control(binary) => Outcome::Control(binary),
            Runner::Engine(_) if !self.against().is_empty() => Outcome::Failed,
            Runner::Engine(_) if self.met() == 0 => Outcome::NothingRan,
            Runner::Engine(_) => Outcome::Passed,
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
        reading.count("run", self.judged.len() as u64)?;
        reading.status = outcome.status();
        reading.message = message;
        Ok(reading)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Went {
        Pass,
        Diverged,
        Fail,
        Skip,
    }

    impl Judgement for Went {
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
        Runner::Engine(PathBuf::from("target/debug/php"))
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
        // them: the same rung, with the count of what did not run on it
        // (#65). Coverage is the lane's and was judged before this.
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
}
