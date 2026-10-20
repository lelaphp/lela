//! The census: every test a run missed, grouped by the signature its miss
//! kept (#1185), and among them the tests `php_compile` refused, grouped
//! by the construct it refused them for (#617).
//!
//! A test that fails, borks or is [`Judgement::Unread`] keeps a
//! [`Miss`] on its verdict, decoded once where the adapter reached it
//! (the `miss` module), so the census is a fold over what the run already
//! kept and nothing is run twice: `--census` is one more rendering of the
//! same lane. Every miss stands in exactly one group, the group of its
//! signature, and [`Census::accounts_for`] holds the census to that.
//!
//! **Why it is worth a rendering.** Which cause holds the most tests is
//! the sharpest thing the corpus can say about what to build next. Until
//! the unread census it was counted by eye, one subset at a time, into the
//! body of each ticket that named a construct (#580, #581, #582, #583);
//! until the signatures were kept, the rest was re-run and read off a
//! printed run by `.claude/skills/specs/first-failures.awk`.
//!
//! The unread construct is grouped by equality and never normalised. The
//! compiler's refusals are a closed set of sentences it writes itself, so
//! two tests stopped by one construct carry one string; a sentence that
//! carried a name the program chose would group only with itself, which
//! is the honest reading of it and not a bucket to be widened by
//! guesswork. The other signatures are masked where the miss was decoded,
//! and grouped here by value.

use std::collections::BTreeMap;

use super::miss::{Miss, Signature};
use super::{Judgement, Tally};
use crate::tally::Judgement as _;

/// At most this many tests are named under an unread construct; the rest
/// are counted. The house's rendering of a long list
/// (`reading::Log::unreadable_note`).
const NAMED: usize = 5;

/// One signature and the tests that missed with it, as [`Census::rows`]
/// orders them: each test by its path in the corpus.
pub(crate) struct Row {
    pub signature: Signature,
    pub tests: Vec<String>,
}

impl Row {
    /// The tests named, at most [`NAMED`] of them and the rest counted.
    fn named(&self) -> String {
        let shown: Vec<&str> = self.tests.iter().take(NAMED).map(String::as_str).collect();
        let rest = self.tests.len().saturating_sub(shown.len());
        let shown = shown.join(", ");
        if rest == 0 {
            shown
        } else {
            format!("{shown} and {rest} more")
        }
    }
}

/// The misses of one run, or of a lane's subsets folded together, by
/// signature.
#[derive(Default)]
pub(crate) struct Census {
    by_signature: BTreeMap<Signature, Vec<String>>,
}

/// Every miss a subset's run kept, by the test's divergence key.
pub(crate) fn misses(tally: &Tally) -> impl Iterator<Item = (&str, &Miss)> {
    tally
        .judged
        .iter()
        .filter_map(|judged| Some((judged.id.as_str(), judged.miss.as_ref()?)))
}

/// Every verdict of a subset's run, as [`Census::accounts_for`] reads it.
pub(crate) fn verdicts(tally: &Tally) -> impl Iterator<Item = (&str, Judgement, Option<&Miss>)> {
    tally
        .judged
        .iter()
        .map(|judged| (judged.id.as_str(), judged.judgement, judged.miss.as_ref()))
}

impl Census {
    /// Every miss added under its signature, each test by its divergence
    /// key.
    pub(crate) fn add<'a>(&mut self, misses: impl IntoIterator<Item = (&'a str, &'a Miss)>) {
        for (id, miss) in misses {
            self.by_signature
                .entry(miss.signature.clone())
                .or_default()
                .push(super::target::named(id).to_owned());
        }
    }

    /// Whether every test that missed kept a miss, stands in the group of
    /// its signature, and the census groups nothing else: the words of
    /// the first that does not. `verdicts` is every test of the run, by
    /// its divergence key, with its judgement and what it kept.
    pub(crate) fn accounts_for<'a>(
        &self,
        verdicts: impl IntoIterator<Item = (&'a str, Judgement, Option<&'a Miss>)>,
    ) -> Result<(), String> {
        let mut counted = 0;
        for (id, judgement, miss) in verdicts {
            if !judgement.misses() {
                continue;
            }
            counted += 1;
            let test = super::target::named(id);
            let Some(miss) = miss else {
                return Err(format!(
                    "{test} is a {} and kept no miss, so no group of the census holds it",
                    judgement.as_str()
                ));
            };
            let grouped = self
                .by_signature
                .get(&miss.signature)
                .is_some_and(|tests| tests.iter().any(|t| t == test));
            if !grouped {
                return Err(format!(
                    "{test} missed with `{}` and stands in no group of the census",
                    miss.signature
                ));
            }
        }
        if self.missed() != counted {
            return Err(format!(
                "the census groups {} tests and the run missed {counted}",
                self.missed()
            ));
        }
        Ok(())
    }

    /// The tests counted, over every signature.
    pub(crate) fn missed(&self) -> usize {
        self.by_signature.values().map(Vec::len).sum()
    }

    /// The tests the compiler refused, over every construct.
    pub(crate) fn unread(&self) -> usize {
        self.unread_rows().iter().map(|row| row.tests.len()).sum()
    }

    /// The groups, the signature holding the most tests first. A tie is
    /// broken by the signature's own order, so the order is total and one
    /// run's census prints the same twice.
    pub(crate) fn rows(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = self
            .by_signature
            .iter()
            .map(|(signature, tests)| Row {
                signature: signature.clone(),
                tests: tests.clone(),
            })
            .collect();
        rows.sort_by(|a, b| {
            b.tests
                .len()
                .cmp(&a.tests.len())
                .then_with(|| a.signature.cmp(&b.signature))
        });
        rows
    }

    /// The groups of the tests the compiler refused, in [`Census::rows`]'
    /// order: the unread census.
    pub(crate) fn unread_rows(&self) -> Vec<Row> {
        self.rows()
            .into_iter()
            .filter(|row| matches!(row.signature, Signature::Unread(_)))
            .collect()
    }

    /// The census under `scope`: the unread constructs, then every miss
    /// by signature with every test of its group, each the widest first.
    /// An empty one says so rather than printing a heading over nothing:
    /// a subset with no such test is the answer, not the absence of one.
    pub(crate) fn print(&self, scope: &str) {
        let unread = self.unread_rows();
        if unread.is_empty() {
            println!("phpt census {scope}: no test was refused by the compiler");
        } else {
            println!(
                "phpt census {scope}: {} unread over {} construct{}",
                self.unread(),
                unread.len(),
                if unread.len() == 1 { "" } else { "s" }
            );
            for row in &unread {
                println!(
                    "  {:>5}  {}: {}",
                    row.tests.len(),
                    row.signature,
                    row.named()
                );
            }
        }
        let rows = self.rows();
        if rows.is_empty() {
            println!("phpt census {scope}: no test missed");
            return;
        }
        println!(
            "phpt census {scope}: {} missed over {} signature{}",
            self.missed(),
            rows.len(),
            if rows.len() == 1 { "" } else { "s" }
        );
        for row in &rows {
            println!(
                "  {:>5}  {}: {}",
                row.tests.len(),
                row.signature,
                row.tests.join(", ")
            );
        }
    }
}
