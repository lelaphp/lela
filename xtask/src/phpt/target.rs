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

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::against::{RUNS, Run};
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

/// Where the workspace at `root` keeps the target of `scope` under the
/// pin.
fn path(root: &Path, pin: &Pin, scope: &str) -> PathBuf {
    root.join(kept_under(pin)).join(format!("{scope}.json"))
}

/// Where one test stands against the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Standing {
    /// The oracle skipped or missed it: outside the target, whatever the
    /// engine's verdict of the moment.
    Outside,
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

    /// Whether the test is in the target: the oracle met it or failed it
    /// as its `--XFAIL--` asks.
    pub(crate) fn holds(&self, id: &str) -> bool {
        matches!(
            self.0.verdicts.get(id),
            Some(Judgement::Pass | Judgement::XFail)
        )
    }

    /// Whether the target stands for `subset` as it is now: it holds a
    /// verdict for each of `tests`, the subset's files, so it predates none
    /// of them (#1644), a test that redirects to a set by a verdict kept
    /// through it for each test of the set ([`redirect::set_kept`]). What `phpt --unkept` asks before the control runs a
    /// subset again (#1830).
    pub(crate) fn stands_for(&self, subset: &Subset, tests: &[PathBuf]) -> bool {
        let verdicts = &self.0.verdicts;
        tests.iter().all(|test| {
            let id = subset.id(test);
            verdicts.contains_key(&id) || redirect::set_kept(subset, &id, verdicts.keys())
        })
    }

    /// Where the test stands, the engine having come to `engine` on it.
    pub(crate) fn standing(&self, registry: &Registry, id: &str, engine: Judgement) -> Standing {
        match self.0.verdicts.get(id) {
            Some(&oracle) if self.holds(id) => {
                if engine == oracle || registry.accepted(id).is_some() {
                    Standing::Met
                } else {
                    Standing::Missed
                }
            }
            Some(_) => Standing::Outside,
            None => Standing::Predated,
        }
    }
}

/// One engine's run of a subset read against its target.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Measure {
    pub target: u64,
    pub met: u64,
    pub outside: u64,
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
        reading.count(Self::MET, self.met)?;
        reading.count(Self::OUTSIDE, self.outside)?;
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
                Standing::Outside => measure.outside += 1,
                Standing::Predated => measure.predated += 1,
                Standing::Met => {
                    measure.target += 1;
                    measure.met += 1;
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

    /// The reading's words for it: met of target, the misses named, and
    /// where the target predates some of the subset's tests, how many and
    /// `keeps`, the command that keeps the target again.
    pub(crate) fn words(&self, keeps: &str) -> String {
        let prefix = format!("phpt:{CORPUS}/");
        let names: Vec<&str> = self
            .missed
            .iter()
            .map(|id| id.strip_prefix(&prefix).unwrap_or(id))
            .collect();
        let missed = if names.is_empty() {
            String::new()
        } else {
            format!("; the target's misses: {}", listed(&names))
        };
        let predated = match self.predated {
            0 => String::new(),
            n => format!(
                "; {n} test{} the target predates, kept over another corpus; {keeps}",
                plural(usize::try_from(n).unwrap_or(usize::MAX))
            ),
        };
        format!(
            "{} of {} target tests met, {} outside{missed}{predated}",
            self.met, self.target, self.outside
        )
    }
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
        let file = Path::new(env!("CARGO_MANIFEST_DIR"))
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
                assert_eq!(
                    target.standing(&registry(), &id(miss), engine),
                    Standing::Outside,
                    "{miss} under {engine:?}"
                );
            }
        }
        let measure = measured();
        assert_eq!(measure.outside, 3, "{measure:#?}");
        assert_eq!(measure.target, 4, "{measure:#?}");
    }

    /// Met-of-target: the engine's verdict equal to the oracle's, an expected
    /// failure included, or a row over the id whatever the engine did, its
    /// skip included (#1161); the one miss named.
    #[test]
    fn the_target_is_met_where_the_engine_answers_as_the_oracle_or_a_row_holds_it() {
        let measure = measured();
        assert_eq!(measure.met, 3, "{measure:#?}");
        assert_eq!(measure.missed, [id("missed.phpt")]);
        assert_eq!(
            measure.words("unread"),
            "3 of 4 target tests met, 3 outside; the target's misses: ext/fixture/tests/missed.phpt"
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
        let mut pin = Pin::parse(
            &fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../oracle.toml"))
                .unwrap(),
        )
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
