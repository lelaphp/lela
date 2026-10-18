//! The target (#1184): the tests the pinned oracle meets under the
//! runner, the set parity is stated over (design §1, as #1161 ruled it).
//! A run under the runner's control, `--engine` the pinned php, keeps the
//! oracle's verdict on every test of the subset as a run is kept for
//! `--against`, under the pin rather than a tree: the oracle's digest and
//! the corpus's tag, so it is taken again only when the pin moves, and
//! every engine's run until then is held to it. A test is in the target
//! where the oracle meets it or is expected to fail it; one the oracle
//! skips or misses, a row of the registry accepting the miss or not, is
//! outside, named and never counted against the engine. An engine's run of
//! the subset is then read as met-of-target: the target's tests whose
//! verdict under the engine is the oracle's, or whose id a registry row
//! holds whatever the engine did, a skip included (#1161). A test the
//! kept target holds no verdict for at all is neither: the target was kept
//! over a corpus the subset no longer is, as #1598 grew `ext/intl` by
//! `ext/intl/uchar/tests` under the same pin, and the reading counts it
//! apart and says how to keep the target again (#1644).
//!
//! **Why a test is outside.** The control keeps what each test the oracle
//! missed keeps and the reason each it skipped was given, so a reading held
//! to the target names each test outside it with why the oracle left it
//! out: its skip's reason, or its miss and the registry row that holds it
//! where one does ([`LeftOut`]). Each list is named while it is no longer
//! than [`NAMED`]: the tests outside the target, while there are no more
//! than that many, and the met ones, while the target is no larger. Past
//! it the reading counts the tests outside by kind and points to the kept
//! target, which holds each with its reason, and to the engine's kept run,
//! which holds what the engine came to on each (#2033). A target kept
//! before its run kept the skips' reasons does not stand, so `--unkept`
//! takes it again.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::against::{RUNS, Run};
use super::miss::Signature;
use super::{CORPUS, Judgement, Subset, redirect};
use crate::divergence::Registry;
use crate::oracle::Pin;
use crate::reading::{Reading, listed, plural};

/// What names the pin a target is kept under: the corpus's tag and the
/// oracle's digest.
pub(crate) fn key(pin: &Pin) -> String {
    format!("pin-{}-{}", pin.source.tag, pin.oracle.md5)
}

/// Where a workspace keeps every target under the pin, relative to its
/// root: beside the kept runs, so `worktree retire` carries them with
/// those, and CI's cache keeps the directory between runs (#1733).
pub(crate) fn kept_under(pin: &Pin) -> PathBuf {
    Path::new(RUNS).join(key(pin))
}

/// Where a workspace keeps the target of `scope` under the pin, relative
/// to its root: what a reading points to for the tests it does not name.
/// It stands in the workspace that took it, or in the main one once
/// `worktree retire` has carried it there.
pub(crate) fn kept_at(pin: &Pin, scope: &str) -> PathBuf {
    kept_under(pin).join(format!("{scope}.json"))
}

/// Where the workspace at `root` keeps the target of `scope` under the
/// pin.
fn path(root: &Path, pin: &Pin, scope: &str) -> PathBuf {
    root.join(kept_at(pin, scope))
}

/// The most tests a reading names in one list; past it, it counts them and
/// points to the kept target (#2033).
pub(crate) const NAMED: usize = 10;

/// What a reading held to a target points to for the tests it does not
/// name, each relative to a workspace's root.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Kept<'a> {
    /// The kept target ([`kept_at`]).
    pub target: &'a Path,
    /// The engine's run, where this run is kept.
    pub run: Option<&'a Path>,
}

/// Why the oracle left a test out of the target, as the kept target and
/// the registry say (#2033).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LeftOut {
    /// The oracle skipped it, for the reason its `SKIPIF` or the runner
    /// gave; none where the target was kept before its run kept the skips'
    /// reasons.
    Skipped(Option<String>),
    /// The oracle missed it: the cause its miss keeps, where the target
    /// keeps one, and the accepted registry row that holds it, where one
    /// does.
    Missed {
        cause: Option<Signature>,
        row: Option<Held>,
    },
    /// The served lane left it out whatever the oracle did: its `--ENV--`
    /// sets a variable no HTTP field carries ([`Judgement::Unsent`]).
    Unsent,
}

/// The accepted registry row that holds a test the oracle missed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Held {
    /// The row whose own id is the test's.
    Own,
    /// A row of another id that covers the test, by that id.
    Covered(String),
}

impl fmt::Display for LeftOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LeftOut::Skipped(Some(reason)) if reason.is_empty() => {
                f.write_str("skipped by the oracle, giving no reason")
            }
            LeftOut::Skipped(Some(reason)) => write!(f, "skipped by the oracle {reason:?}"),
            LeftOut::Skipped(None) => f.write_str("skipped by the oracle, its reason not kept"),
            LeftOut::Unsent => {
                f.write_str("left out of the lane, its --ENV-- setting what no HTTP field carries")
            }
            LeftOut::Missed { cause, row } => {
                f.write_str("missed by the oracle")?;
                if let Some(cause) = cause {
                    write!(f, ", {cause}")?;
                }
                match row {
                    Some(Held::Own) => f.write_str(" under its own registry row"),
                    Some(Held::Covered(row)) => write!(f, " under the registry row {row}"),
                    None => Ok(()),
                }
            }
        }
    }
}

/// Where one test stands against the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Standing {
    /// The oracle skipped or missed it: outside the target, whatever the
    /// engine's verdict of the moment, and why.
    Outside(LeftOut),
    /// The target holds no verdict for it: kept over another corpus than
    /// the one the subset holds now, so it predates the test.
    Predated,
    /// In the target, and the engine's verdict is the oracle's or a
    /// registry row holds the id.
    Met,
    /// In the target, and the engine's verdict is not the oracle's.
    Missed,
}

/// One subset's target: the oracle's verdicts under the runner's control,
/// kept as a run is.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Target(Run);

impl Target {
    /// The oracle's verdicts of a control run, kept.
    pub(crate) fn of(run: Run) -> Target {
        Target(run)
    }

    /// The target of `scope` under the pin, from the first of `roots` that
    /// keeps one; none where no control run of the subset has been kept
    /// under this pin.
    pub(crate) fn kept(roots: &[PathBuf], pin: &Pin, scope: &str) -> Result<Option<Target>> {
        for root in roots {
            let file = path(root, pin, scope);
            if file.is_file() {
                let text = std::fs::read_to_string(&file)
                    .with_context(|| format!("reading {}", file.display()))?;
                let run = serde_json::from_str(&text)
                    .with_context(|| format!("{} is not a kept target", file.display()))?;
                return Ok(Some(Target(run)));
            }
        }
        Ok(None)
    }

    /// The target kept for `scope` under the pin in the workspace at
    /// `root`, replaced whole as a reading is.
    pub(crate) fn keep(&self, root: &Path, pin: &Pin, scope: &str) -> Result<()> {
        let text = serde_json::to_string_pretty(&self.0).context("rendering the target")?;
        crate::reading::replace(&path(root, pin, scope), text.as_bytes())
    }

    /// Whether the test is in the target: the oracle passed it, warned or
    /// not, or failed it as its `--XFAIL--` asks.
    pub(crate) fn holds(&self, id: &str) -> bool {
        matches!(
            self.0.verdicts.get(id),
            Some(oracle) if oracle.passed() || *oracle == Judgement::XFail
        )
    }

    /// Whether the target stands for `subset` as it is now: it holds a
    /// verdict for each of `tests`, the subset's files, so it predates none
    /// of them (#1644), a test that redirects to a set by a verdict kept
    /// through it for each test of the set ([`redirect::set_kept`]), and
    /// it keeps the reason for each skip ([`keeps_every_reason`]). What
    /// `phpt --unkept` asks before the control runs a subset again (#1830).
    ///
    /// [`keeps_every_reason`]: Target::keeps_every_reason
    pub(crate) fn stands_for(&self, subset: &Subset, tests: &[PathBuf]) -> bool {
        let verdicts = &self.0.verdicts;
        self.keeps_every_reason()
            && tests.iter().all(|test| {
                let id = subset.id(test);
                verdicts.contains_key(&id) || redirect::set_kept(subset, &id, verdicts.keys())
            })
    }

    /// Whether the target keeps the reason for each test the oracle
    /// skipped: one kept before its run kept them does not, and does not
    /// stand ([`stands_for`](Target::stands_for)), so `--unkept` takes it
    /// again and the readings held to it name each skip's reason (#2033).
    pub(crate) fn keeps_every_reason(&self) -> bool {
        let Run {
            verdicts, skips, ..
        } = &self.0;
        verdicts
            .iter()
            .all(|(id, verdict)| *verdict != Judgement::Skip || skips.contains_key(id))
    }

    /// Why the oracle left the test out of the target, `oracle` being its
    /// verdict there: a skip with the reason it was given, or a miss with
    /// what it keeps and the accepted registry row that holds it.
    fn left_out(&self, registry: &Registry, id: &str, oracle: Judgement) -> LeftOut {
        match oracle {
            Judgement::Skip => LeftOut::Skipped(self.0.skips.get(id).cloned()),
            _ => LeftOut::Missed {
                cause: self.0.misses.get(id).map(|miss| miss.signature.clone()),
                row: registry.accepted(id).map(|row| {
                    if row.id.as_str() == id {
                        Held::Own
                    } else {
                        Held::Covered(row.id.to_string())
                    }
                }),
            },
        }
    }

    /// Where the test stands, the engine having come to `engine` on it.
    /// A test the engine came to `Diverged` on is met, since a registry
    /// row accepted its difference, under its own id or, in the served
    /// subset, a `makala:` one (#2644); one the served lane left out is
    /// outside, whatever the oracle did.
    pub(crate) fn standing(&self, registry: &Registry, id: &str, engine: Judgement) -> Standing {
        if engine == Judgement::Unsent {
            return Standing::Outside(LeftOut::Unsent);
        }
        match self.0.verdicts.get(id) {
            Some(&oracle) if self.holds(id) => {
                let accepted = engine == Judgement::Diverged || registry.accepted(id).is_some();
                // A pass and a warned pass met the same expectation, told
                // apart by the test's `--XFAIL--` alone.
                if engine == oracle || (engine.passed() && oracle.passed()) || accepted {
                    Standing::Met
                } else {
                    Standing::Missed
                }
            }
            Some(&oracle) => Standing::Outside(self.left_out(registry, id, oracle)),
            None => Standing::Predated,
        }
    }
}

/// One engine's run of a subset read against its target.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Measure {
    pub target: u64,
    /// The target's tests the engine met, by divergence key.
    pub met: Vec<String>,
    /// The subset's tests outside the target, by divergence key, each with
    /// why the oracle left it out (#2033).
    pub outside: Vec<(String, LeftOut)>,
    /// The subset's tests the kept target holds no verdict for.
    pub predated: u64,
    /// The target's tests the engine missed, by divergence key.
    pub missed: Vec<String>,
    /// Of `missed`, those the engine skipped (#1834).
    pub missed_skipped: u64,
}

impl Measure {
    /// The key of a reading's `numbers` that counts the target's tests.
    pub(crate) const TARGET: &str = "target";
    /// The key that counts the target's tests the engine met.
    pub(crate) const MET: &str = "met";
    /// The key that counts the subset's tests outside the target.
    pub(crate) const OUTSIDE: &str = "outside";
    /// The key that counts the subset's tests the kept target predates,
    /// written only where there are any (#1644).
    pub(crate) const PREDATED: &str = "predated";
    /// The key that counts the target's misses the engine skipped, written
    /// on every reading held to a target from #1834 on, so a reading
    /// without it is one from before the count was kept.
    pub(crate) const MISSED_SKIPPED: &str = "missed_skipped";

    /// The measure written into the reading's numbers, under the keys
    /// above.
    pub(crate) fn write(&self, reading: &mut Reading) -> Result<()> {
        reading.count(Self::TARGET, self.target)?;
        reading.count(Self::MET, u64::try_from(self.met.len())?)?;
        reading.count(Self::OUTSIDE, u64::try_from(self.outside.len())?)?;
        reading.count(Self::MISSED_SKIPPED, self.missed_skipped)?;
        if self.predated > 0 {
            reading.count(Self::PREDATED, self.predated)?;
        }
        Ok(())
    }

    /// Every verdict of the run, by divergence key, placed against `target`.
    pub(crate) fn of<'a>(
        target: &Target,
        registry: &Registry,
        verdicts: impl IntoIterator<Item = (&'a str, Judgement)>,
    ) -> Measure {
        let mut measure = Measure::default();
        for (id, engine) in verdicts {
            match target.standing(registry, id, engine) {
                Standing::Outside(left) => measure.outside.push((id.to_owned(), left)),
                Standing::Predated => measure.predated += 1,
                Standing::Met => {
                    measure.target += 1;
                    measure.met.push(id.to_owned());
                }
                Standing::Missed => {
                    measure.target += 1;
                    measure.missed.push(id.to_owned());
                    if engine == Judgement::Skip {
                        measure.missed_skipped += 1;
                    }
                }
            }
        }
        measure
    }

    /// The reading's words for it: met of target, the met tests named
    /// while the target is no larger than [`NAMED`]; the tests outside it
    /// ([`outside_words`](Measure::outside_words)); the target's misses;
    /// `kept`, where a list is left unnamed; and, where the target predates
    /// some of the subset's tests or keeps no reason for its skips, that,
    /// with `keeps`, the command that keeps the target again.
    pub(crate) fn words(&self, kept: Kept<'_>, keeps: &str) -> String {
        let target_named = usize::try_from(self.target).unwrap_or(usize::MAX) <= NAMED;
        let met = if self.met.is_empty() || !target_named {
            String::new()
        } else {
            format!(" ({})", in_corpus(&self.met).join(", "))
        };
        let missed = if self.missed.is_empty() {
            String::new()
        } else {
            format!(
                "; the target's misses: {}",
                listed(&in_corpus(&self.missed))
            )
        };
        let pointed_to = if target_named && self.outside.len() <= NAMED {
            String::new()
        } else {
            let run = match kept.run {
                Some(run) if !target_named => format!(
                    ", and the engine's verdict on each test at {}",
                    run.display()
                ),
                _ => String::new(),
            };
            format!(
                "; the target and the oracle's reason for each test outside it are kept at {}{run}",
                kept.target.display()
            )
        };
        let mut stale = Vec::new();
        if self.predated > 0 {
            let n = self.predated;
            stale.push(format!(
                "{n} test{} the target predates, kept over another corpus",
                plural(usize::try_from(n).unwrap_or(usize::MAX))
            ));
        }
        if (self.outside.iter()).any(|(_, left)| *left == LeftOut::Skipped(None)) {
            stale.push("the target was kept before a run kept the reason for each skip".to_owned());
        }
        let keep_again = if stale.is_empty() {
            String::new()
        } else {
            format!("; {}; {keeps}", stale.join("; "))
        };
        format!(
            "{} of {} target tests met{met}, {}{missed}{pointed_to}{keep_again}",
            self.met.len(),
            self.target,
            self.outside_words()
        )
    }

    /// The tests outside the target in the reading's words: how many, then
    /// each named under why the oracle left it out, the reasons in the
    /// order their first test comes, or past [`NAMED`] how many for each
    /// kind of reason.
    fn outside_words(&self) -> String {
        let n = self.outside.len();
        if n == 0 {
            return "0 outside".to_owned();
        }
        if n > NAMED {
            let skipped = (self.outside.iter())
                .filter(|(_, left)| matches!(left, LeftOut::Skipped(_)))
                .count();
            let held = (self.outside.iter())
                .filter(|(_, left)| matches!(left, LeftOut::Missed { row: Some(_), .. }))
                .count();
            let unsent = (self.outside.iter())
                .filter(|(_, left)| *left == LeftOut::Unsent)
                .count();
            let mut kinds = Vec::new();
            if skipped > 0 {
                kinds.push(format!("{skipped} skipped by the oracle"));
            }
            if n > skipped + unsent {
                kinds.push(format!("{} missed by it", n - skipped - unsent));
            }
            if held > 0 {
                kinds.push(format!("{held} of those under a registry row"));
            }
            if unsent > 0 {
                kinds.push(format!("{unsent} left out of the lane"));
            }
            return format!("{n} outside ({})", kinds.join(", "));
        }
        let mut groups: Vec<(&LeftOut, Vec<&str>)> = Vec::new();
        for (id, left) in &self.outside {
            let name = named(id);
            match groups.iter_mut().find(|(said, _)| *said == left) {
                Some((_, names)) => names.push(name),
                None => groups.push((left, vec![name])),
            }
        }
        let groups: Vec<String> = groups
            .iter()
            .map(|(left, names)| format!("{left}: {}", names.join(", ")))
            .collect();
        format!("{n} outside ({})", groups.join("; "))
    }
}

/// A divergence key as the reading names the test: its path in the corpus,
/// which a `makala:` row's id carries after its prefix.
pub(super) fn named(id: &str) -> &str {
    (id.strip_prefix("phpt:"))
        .and_then(|rest| rest.strip_prefix(CORPUS))
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(id)
}

/// Each of `ids` as the reading names it.
fn in_corpus(ids: &[String]) -> Vec<&str> {
    ids.iter().map(|id| named(id)).collect()
}

#[cfg(test)]
mod tests {
    // The target from inside, over the fixture at `xtask/fixtures/phpt/met-of-target/`,
    // which plants two tests the oracle misses. Nothing here spawns php, so
    // the negative controls run on every machine.

    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::phpt::against::diff;
    use crate::scratch::Tree;

    fn fixture(name: &str) -> Run {
        let file = crate::package_dir()
            .join("fixtures/phpt/met-of-target")
            .join(name);
        serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap()
    }

    fn id(name: &str) -> String {
        format!("phpt:php-src/ext/fixture/tests/{name}")
    }

    /// The registry the fixture reads under: a row over the test the engine
    /// skipped, and one over the oracle's own miss.
    fn registry() -> Registry {
        let row = |name: &str| {
            format!(
                "[[divergence]]\nid = \"{}\"\nkind = \"phpt\"\nreason = \"a person's reason\"\nstatus = \"accepted\"\nsince = \"2026-09-30\"\n",
                id(name)
            )
        };
        Registry::parse(&format!("{}\n{}", row("row.phpt"), row("oracle-row.phpt"))).unwrap()
    }

    /// Where the tests' reading points to what it does not name.
    fn kept() -> Kept<'static> {
        Kept {
            target: Path::new("kept.json"),
            run: None,
        }
    }

    fn measured() -> Measure {
        let engine = fixture("engine.json");
        let target = Target::of(fixture("control.json"));
        Measure::of(
            &target,
            &registry(),
            engine.verdicts.iter().map(|(id, j)| (id.as_str(), *j)),
        )
    }

    /// The negative control of #1184: the oracle's two planted misses, one
    /// under a registry row, and its skip are outside the target whatever
    /// the engine did, and a reading that counted either miss in `target`
    /// fails here.
    #[test]
    fn a_test_the_oracle_misses_is_outside_and_never_in_the_target() {
        let target = Target::of(fixture("control.json"));
        for miss in ["oracle-miss.phpt", "oracle-row.phpt", "oracle-skip.phpt"] {
            assert!(!target.holds(&id(miss)), "{miss} is in the target");
            for engine in [Judgement::Pass, Judgement::Fail, Judgement::Diverged] {
                let standing = target.standing(&registry(), &id(miss), engine);
                assert!(
                    matches!(standing, Standing::Outside(_)),
                    "{miss} under {engine:?}: {standing:?}"
                );
            }
        }
        let measure = measured();
        assert_eq!(measure.outside.len(), 3, "{measure:#?}");
        assert_eq!(measure.target, 4, "{measure:#?}");
    }

    /// An engine's warned pass meets the oracle's pass, as a target kept
    /// before #2922 holds every warned test, and misses its expected
    /// failure.
    #[test]
    fn a_warned_pass_meets_a_pass_and_misses_an_expected_failure() {
        let target = Target::of(fixture("control.json"));
        let standing = |test| target.standing(&registry(), &id(test), Judgement::Warn);
        assert_eq!(standing("kept.phpt"), Standing::Met);
        assert_eq!(standing("xfail.phpt"), Standing::Missed);
    }

    /// A test the engine came to `Diverged` on is met, though no `phpt:`
    /// row holds it, since a `makala:` row accepted it in the served lane;
    /// and one the served lane left out is outside the target whatever the
    /// oracle did, named with why (#2644).
    #[test]
    fn a_diverged_test_is_met_and_one_the_served_lane_left_out_is_outside() {
        let target = Target::of(fixture("control.json"));
        let test = id("kept.phpt");
        let standing = target.standing(&Registry::default(), &test, Judgement::Diverged);
        assert_eq!(standing, Standing::Met);
        let standing = target.standing(&Registry::default(), &test, Judgement::Unsent);
        assert_eq!(standing, Standing::Outside(LeftOut::Unsent));
        let measure = Measure::of(
            &target,
            &Registry::default(),
            [(test.as_str(), Judgement::Unsent)],
        );
        assert!(
            measure.words(kept(), "").contains(
                "left out of the lane, its --ENV-- setting what no HTTP field carries: ext/fixture/tests/kept.phpt"
            ),
            "{measure:?}"
        );
    }

    /// Met-of-target: the engine's verdict equal to the oracle's, an expected
    /// failure included, or a row over the id whatever the engine did, its
    /// skip included (#1161); the one miss named.
    #[test]
    fn the_target_is_met_where_the_engine_answers_as_the_oracle_or_a_row_holds_it() {
        let measure = measured();
        assert_eq!(measure.met, ["kept.phpt", "row.phpt", "xfail.phpt"].map(id));
        assert_eq!(measure.missed, [id("missed.phpt")]);
    }

    /// Each test outside the target is named with why the oracle left it
    /// out (#2033): its skip's reason, or what its miss keeps and the
    /// registry row that holds it where one does; and with the subset this
    /// small, the target's met tests are named too, so the reading alone
    /// says which tests the subset is held to.
    #[test]
    fn a_test_outside_the_target_is_named_with_the_oracles_reason() {
        let measure = measured();
        assert_eq!(
            measure.outside,
            [
                (
                    id("oracle-miss.phpt"),
                    LeftOut::Missed {
                        cause: Some(Signature::PastLimit),
                        row: None
                    }
                ),
                (
                    id("oracle-row.phpt"),
                    LeftOut::Missed {
                        cause: None,
                        row: Some(Held::Own)
                    }
                ),
                (
                    id("oracle-skip.phpt"),
                    LeftOut::Skipped(Some("For Linux only".to_owned()))
                ),
            ]
        );
        assert_eq!(
            measure.words(kept(), "unread"),
            "3 of 4 target tests met (ext/fixture/tests/kept.phpt, ext/fixture/tests/row.phpt, \
             ext/fixture/tests/xfail.phpt), 3 outside (missed by the oracle, ran past the \
             recipe's limit: ext/fixture/tests/oracle-miss.phpt; missed by the oracle under its \
             own registry row: ext/fixture/tests/oracle-row.phpt; skipped by the oracle \"For \
             Linux only\": ext/fixture/tests/oracle-skip.phpt); the target's misses: \
             ext/fixture/tests/missed.phpt"
        );
    }

    /// Tests outside the target that share a reason are named once under
    /// it, a row covering another id is named by its id, and a target kept
    /// before a run kept its skips' reasons says so, with the command that
    /// keeps it again, and does not stand (#2033).
    #[test]
    fn outside_tests_are_grouped_by_reason_and_a_reasonless_target_says_so() {
        let mut control = fixture("control.json");
        for name in ["gh10370_1.phpt", "gh10370_2.phpt"] {
            control.verdicts.insert(id(name), Judgement::Skip);
            control.skips.insert(id(name), "For Linux only".to_owned());
        }
        control
            .verdicts
            .insert(id("covered.phpt"), Judgement::Diverged);
        let registry = Registry::parse(&format!(
            "[[divergence]]\nid = \"{}\"\nkind = \"phpt\"\nreason = \"a person's reason\"\nstatus = \"accepted\"\nsince = \"2026-10-05\"\ncovers = [\"{}\"]\n",
            id("oracle-row.phpt"),
            id("covered.phpt")
        ))
        .unwrap();
        let ids = [
            "covered.phpt",
            "gh10370_1.phpt",
            "gh10370_2.phpt",
            "oracle-skip.phpt",
        ]
        .map(id);
        let target = Target::of(control.clone());
        assert!(target.keeps_every_reason());
        let measure = Measure::of(
            &target,
            &registry,
            ids.iter().map(|id| (id.as_str(), Judgement::Pass)),
        );
        assert_eq!(
            measure.words(kept(), "keeps"),
            "0 of 0 target tests met, 4 outside (missed by the oracle under the registry row \
             phpt:php-src/ext/fixture/tests/oracle-row.phpt: ext/fixture/tests/covered.phpt; \
             skipped by the oracle \"For Linux only\": ext/fixture/tests/gh10370_1.phpt, \
             ext/fixture/tests/gh10370_2.phpt, ext/fixture/tests/oracle-skip.phpt)"
        );

        control.skips.clear();
        let target = Target::of(control);
        assert!(!target.keeps_every_reason());
        let measure = Measure::of(
            &target,
            &registry,
            ids.iter().map(|id| (id.as_str(), Judgement::Pass)),
        );
        assert!(
            measure.words(kept(), "keeps").ends_with(
                "skipped by the oracle, its reason not kept: ext/fixture/tests/gh10370_1.phpt, \
                 ext/fixture/tests/gh10370_2.phpt, ext/fixture/tests/oracle-skip.phpt); the \
                 target was kept before a run kept the reason for each skip; keeps"
            ),
            "{measure:#?}"
        );
    }

    /// Past [`NAMED`] tests, the reading counts the outside tests by why
    /// and points to the kept target, which names each with its reason,
    /// rather than growing the message by every name; the met tests of a
    /// target that large are left to it too.
    #[test]
    fn a_large_subset_points_to_the_kept_target() {
        let mut control = fixture("control.json");
        let mut ids = Vec::new();
        for n in 0..=NAMED {
            let name = id(&format!("skip-{n}.phpt"));
            control.verdicts.insert(name.clone(), Judgement::Skip);
            control
                .skips
                .insert(name.clone(), "no reason here".to_owned());
            ids.push(name);
        }
        for n in 0..=NAMED {
            let name = id(&format!("pass-{n}.phpt"));
            control.verdicts.insert(name.clone(), Judgement::Pass);
            ids.push(name);
        }
        ids.push(id("oracle-miss.phpt"));
        let measure = Measure::of(
            &Target::of(control),
            &registry(),
            ids.iter().map(|id| (id.as_str(), Judgement::Pass)),
        );
        assert_eq!(
            measure.words(
                Kept {
                    target: Path::new("target/harness/phpt-runs/pin/ext/fixture.json"),
                    run: Some(Path::new("target/harness/phpt-runs/tree/ext/fixture.json")),
                },
                "keeps"
            ),
            "11 of 11 target tests met, 12 outside (11 skipped by the oracle, 1 missed by it); \
             the target and the oracle's reason for each test outside it are kept at \
             target/harness/phpt-runs/pin/ext/fixture.json, and the engine's verdict on each test at \
             target/harness/phpt-runs/tree/ext/fixture.json"
        );
    }

    /// The case from the merge check at e01b1f66: a test the oracle misses,
    /// `stream_server_reneg_limit` there, read `diverged` at the base under its
    /// row and a fail past the recipe's limit at the head. Without a target
    /// `--against` calls it a regression; with the target kept it is none of
    /// held, regressed or fixed, while a target test that regresses still
    /// does.
    #[test]
    fn a_test_outside_the_target_never_reads_as_regressed() {
        let base = fixture("engine.json");
        let mut head = base.clone();
        head.verdicts.insert(id("oracle-row.phpt"), Judgement::Fail);
        let unheld = diff(&base, &head, None).unwrap();
        assert_eq!(unheld.regressed, [(id("oracle-row.phpt"), Judgement::Fail)]);

        let target = Target::of(fixture("control.json"));
        let against = diff(&base, &head, Some(&target)).unwrap();
        assert!(against.regressed.is_empty(), "{against:#?}");
        // Of the target, kept.phpt alone was met at the base: xfail.phpt's
        // expected failure and row.phpt's skip met nothing there.
        assert_eq!(against.held, 1, "{against:#?}");

        head.verdicts.insert(id("kept.phpt"), Judgement::Fail);
        let against = diff(&base, &head, Some(&target)).unwrap();
        assert_eq!(against.regressed, [(id("kept.phpt"), Judgement::Fail)]);
    }

    /// A target is kept under the pin and found by it: another tag or another
    /// digest finds none.
    #[test]
    fn a_target_is_kept_and_found_under_the_pin() {
        let dir = Tree::new("target-kept").unwrap();
        let mut pin =
            Pin::parse(&fs::read_to_string(crate::package_dir().join("../oracle.toml")).unwrap())
                .unwrap();
        let target = Target::of(fixture("control.json"));
        target.keep(dir.path(), &pin, "ext/fixture").unwrap();
        let roots = [dir.path().to_path_buf()];
        assert_eq!(
            Target::kept(&roots, &pin, "ext/fixture").unwrap(),
            Some(target)
        );
        assert_eq!(Target::kept(&roots, &pin, "ext/other").unwrap(), None);
        pin.oracle.md5 = "0".repeat(32);
        assert_eq!(Target::kept(&roots, &pin, "ext/fixture").unwrap(), None);
    }
}
