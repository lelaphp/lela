//! One phpt reading as a row of `status --section phpt --format jsonl
//! --every` (harness-design.md §4.6, #1745): the facts of that reading
//! alone, under a shape `v` versions. Which reading stands for a subset,
//! which subsets overlap, which a crate owns and which parity counts all
//! depend on the manifest at the ref a reader views, so they stay the
//! reader's; a row says what one reading measured and nothing about the
//! others.

use serde::Serialize;

use super::target::Measure;
use super::{Comparison, Judgement, Lane, Scope, numbers};
use crate::reading::Reading;
use crate::status::PerReading;
use crate::status::every::{Head, count};
use crate::tally;

/// The version of [`Row`]'s shape. It changes when the shape does, a
/// field added, removed, renamed or read differently, and never for a
/// new reading; a reader refuses a `v` it does not know.
pub(crate) const V: u32 = 1;

/// One phpt reading, in the shape the dashboard and any other reader of
/// the corpus takes it in. The field order is the line's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Row {
    /// [`V`], and where the reading was taken.
    #[serde(flatten)]
    head: Head,
    /// The subset the scope names; null for a scope outside the grammar
    /// [`Scope`] reads, which a reader takes for no subset's.
    subset: Option<String>,
    /// What the run was; null where the subset is.
    role: Option<Role>,
    /// The compiled tier's lane the run was under; null for none, and
    /// where the subset is.
    lane: Option<Lane>,
    /// Whether anything ran: false for a binary absent or stale, a crate
    /// not yet created, a subset refused before it ran, and a comparison,
    /// which runs nothing of its own.
    measured: bool,
    /// The tests the subset holds.
    tests: u64,
    /// The corpus's files the subset holds; its tests where a reading
    /// from before #1589 counted none.
    files: u64,
    /// Passed.
    p: u64,
    /// Diverged: missed, under an accepted registry row.
    d: u64,
    /// Failed or borked: what fails the reading.
    f: u64,
    /// Unread: refused by the compiler before any test code ran.
    u: u64,
    /// The rest of `tests`: skipped, expected failures, and whatever the
    /// reading did not account for; `p + d + f + u + s` is `tests`.
    s: u64,
    /// The tests that met their expectation, passed or diverged.
    met: u64,
    /// Met-of-target, where the reading was held to one (#1184); null
    /// where it was not.
    target: Option<Target>,
}

/// What a run was beside the subset's own: the engine's run, the
/// runner's control with the pinned php in the engine's place, or a run's
/// comparison against a kept commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Role {
    /// The engine's run of the subset.
    Engine,
    /// The runner's control: the pinned php run in the engine's place.
    Control,
    /// A run's comparison against the run kept at a commit (#664).
    Against,
}

/// The reading held to its subset's target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct Target {
    /// The tests in the target.
    pub(crate) n: u64,
    /// The target's tests the engine met.
    pub(crate) met: u64,
    /// The subset's tests outside the target: the oracle skipped or
    /// missed them.
    pub(crate) outside: u64,
    /// The subset's tests the kept target holds no verdict for, kept apart
    /// from `outside` (#1644); 0 where the reading counts none.
    pub(crate) predated: u64,
}

impl Row {
    /// The subset whose own run the reading is: the engine's, beside no
    /// control, lane or comparison; none for every other reading.
    pub(crate) fn run_of(&self) -> Option<&str> {
        match (self.role, self.lane) {
            (Some(Role::Engine), None) => self.subset.as_deref(),
            _ => None,
        }
    }

    /// Met-of-target, where the reading was held to one.
    pub(crate) fn target(&self) -> Option<Target> {
        self.target
    }

    /// Whether anything ran.
    pub(crate) fn measured(&self) -> bool {
        self.measured
    }

    /// The tests the subset holds.
    pub(crate) fn tests(&self) -> u64 {
        self.tests
    }
}

impl PerReading for Row {
    const INSTRUMENT: &'static str = super::INSTRUMENT;

    /// The facts of one reading: the rules, as a pure function of it.
    fn of(reading: &Reading) -> Row {
        let counted = |key: &str| count(reading, key).unwrap_or(0);
        let sum = |which: fn(Judgement) -> bool| -> u64 {
            Judgement::ALL
                .into_iter()
                .filter(|j| which(*j))
                .map(|j| counted(j.key()))
                .sum()
        };
        let scope: Option<Scope> = reading.scope.parse().ok();
        let role = scope.as_ref().map(|scope| match scope.compares() {
            None => Role::Engine,
            Some((_, Comparison::Control)) => Role::Control,
            Some((_, Comparison::Against)) => Role::Against,
        });
        let tests = counted(numbers::TESTS);
        let (p, d, u) = (
            counted(Judgement::Pass.key()),
            counted(Judgement::Diverged.key()),
            counted(Judgement::Unread.key()),
        );
        let f = sum(<Judgement as tally::Judgement>::counts_against);
        let nothing_ran = counted(numbers::ENGINE_ABSENT) > 0 || counted(numbers::ENGINE_STALE) > 0;
        let judged = counted(tally::RUN) > 0 || sum(|_| true) > 0;
        Row {
            head: Head::of(reading, V),
            subset: scope.as_ref().map(|scope| scope.subset.clone()),
            role,
            lane: scope.as_ref().and_then(|scope| scope.lane),
            measured: judged && !nothing_ran,
            tests,
            files: count(reading, numbers::FILES).unwrap_or(tests),
            p,
            d,
            f,
            u,
            s: tests.saturating_sub(p + d + f + u),
            met: sum(<Judgement as tally::Judgement>::met),
            target: count(reading, Measure::TARGET).map(|n| Target {
                n,
                met: counted(Measure::MET),
                outside: counted(Measure::OUTSIDE),
                predated: counted(Measure::PREDATED),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Lane, Role, Row, Target};
    use crate::reading::{Number, Reading};
    use crate::status::PerReading;

    /// A phpt reading of `scope` with the counts named.
    fn reading(scope: &str, counts: &[(&str, i64)]) -> Reading {
        let mut reading = Reading::new(crate::phpt::INSTRUMENT, scope);
        reading.numbers = counts
            .iter()
            .map(|(key, n)| ((*key).to_owned(), Number::Int(*n)))
            .collect();
        reading.commit = "a0f92cf".to_owned();
        reading
    }

    /// A reading's counts, by key.
    type Counts<'a> = &'a [(&'a str, i64)];

    /// The keyset of the first readings the log keeps (2026-09-17): no
    /// `unread`, `engine_stale`, `files` or target.
    const FIRST: &[(&str, i64)] = &[
        ("borked", 1),
        ("diverged", 2),
        ("engine_absent", 0),
        ("failed", 3),
        ("passed", 40),
        ("run", 55),
        ("skipped", 4),
        ("tests", 55),
        ("xfailed", 0),
    ];

    /// One fixture row per keyset the log has held, in the order they
    /// arrived: each renders the five outcomes summing to its tests, met
    /// as passed and diverged, and `files` its tests until it counted
    /// files of its own (#1589).
    #[test]
    fn every_historical_keyset_renders_its_outcomes() {
        let stale_and_unread = [FIRST, &[("engine_stale", 0), ("unread", 5)]].concat();
        let targeted = [
            stale_and_unread.as_slice(),
            &[("target", 44), ("met", 40), ("outside", 6)],
        ]
        .concat();
        let filed = [stale_and_unread.as_slice(), &[("files", 48)]].concat();
        let filed_targeted = [targeted.as_slice(), &[("files", 48)]].concat();
        let predated = [filed_targeted.as_slice(), &[("predated", 2)]].concat();
        let cases: [(&str, Counts, u64, u64); 6] = [
            ("first", FIRST, 0, 55),
            ("stale and unread", &stale_and_unread, 5, 55),
            ("held to a target", &targeted, 5, 55),
            ("files", &filed, 5, 48),
            ("files and a target", &filed_targeted, 5, 48),
            ("predated", &predated, 5, 48),
        ];
        for (name, counts, unread, files) in cases {
            let row = Row::of(&reading("ext/standard", counts));
            assert!(row.measured, "{name}: {row:#?}");
            assert_eq!(
                (row.p, row.d, row.f, row.u, row.met, row.files),
                (40, 2, 4, unread, 42, files),
                "{name}"
            );
            assert_eq!(row.p + row.d + row.f + row.u + row.s, row.tests, "{name}");
            assert_eq!(row.subset.as_deref(), Some("ext/standard"), "{name}");
            assert_eq!(row.role, Some(Role::Engine), "{name}");
        }
    }

    /// A binary absent or stale ran nothing, and the row says so whatever
    /// else the reading counts; a subset run whose every test skipped did
    /// run (#475, #132).
    #[test]
    fn an_absent_or_stale_engine_measured_nothing() {
        let quiet = [("run", 0), ("tests", 50), ("engine_stale", 0)];
        for (name, flag) in [("absent", "engine_absent"), ("stale", "engine_stale")] {
            let counts = [&quiet[..], &[(flag, 1)]].concat();
            let row = Row::of(&reading("ext/standard", &counts));
            assert!(!row.measured, "{name}: {row:#?}");
            assert_eq!(
                row.s, 50,
                "{name}: every test of an unrun subset is the rest"
            );
        }
        let skipped = [
            ("run", 50),
            ("skipped", 50),
            ("tests", 50),
            ("engine_absent", 0),
            ("engine_stale", 0),
        ];
        assert!(Row::of(&reading("ext/standard", &skipped)).measured);
        assert!(!Row::of(&reading("ext/standard", &[])).measured);
    }

    /// A reading held to a target that predates some of the subset's
    /// tests keeps those apart from the ones outside it (#1644).
    #[test]
    fn a_predated_row_keeps_outside_and_predated_apart() {
        let counts = [
            ("tests", 50),
            ("run", 50),
            ("passed", 40),
            ("target", 44),
            ("met", 40),
            ("outside", 4),
            ("predated", 2),
        ];
        let row = Row::of(&reading("ext/intl", &counts));
        assert_eq!(
            row.target,
            Some(Target {
                n: 44,
                met: 40,
                outside: 4,
                predated: 2
            })
        );
    }

    /// A reading never held to a target says so with a null, never a
    /// target of nothing; one held with nothing predated counts none.
    #[test]
    fn a_row_without_a_target_carries_null() {
        let row = Row::of(&reading("ext/standard", FIRST));
        assert_eq!(row.target, None);
        let line = serde_json::to_value(&row).unwrap();
        assert_eq!(line["target"], serde_json::Value::Null);
        assert_eq!(line["v"], 1);

        let held = [FIRST, &[("target", 44), ("met", 40), ("outside", 6)]].concat();
        let row = Row::of(&reading("ext/standard", &held));
        assert_eq!(row.target.map(|t| t.predated), Some(0));
    }

    /// The scope read through the grammar: a control, a lane and a
    /// comparison each say what the run was, and a scope outside the
    /// grammar is no subset's rather than a subset of its own spelling.
    #[test]
    fn the_scope_is_read_through_its_grammar() {
        let cases = [
            ("ext/pcre", Some("ext/pcre"), Some(Role::Engine), None),
            (
                "ext/pcre@control",
                Some("ext/pcre"),
                Some(Role::Control),
                None,
            ),
            (
                "ext/pcre@against",
                Some("ext/pcre"),
                Some(Role::Against),
                None,
            ),
            (
                "ext/pcre@force",
                Some("ext/pcre"),
                Some(Role::Engine),
                Some(Lane::Force),
            ),
            ("X@control@against", Some("X"), Some(Role::Against), None),
            ("ext/pcre@jit", None, None, None),
        ];
        for (scope, subset, role, lane) in cases {
            let row = Row::of(&reading(scope, &[]));
            assert_eq!(
                (row.subset.as_deref(), row.role, row.lane),
                (subset, role, lane),
                "{scope}"
            );
        }
        let row = Row::of(&reading("ext/pcre@control@exit-fuzz", &[]));
        let line = serde_json::to_value(&row).unwrap();
        assert_eq!(
            (&line["role"], &line["lane"], &line["commit"]),
            (&json!("control"), &json!("exit-fuzz"), &json!("a0f92cf"))
        );
    }
}
