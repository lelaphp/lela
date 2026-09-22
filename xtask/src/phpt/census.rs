//! The unread census: the tests `php_compile` refused, grouped by the
//! construct it refused them for (#617).
//!
//! A test is [`Judgement::Unread`] when everything the engine wrote was
//! one `Parse error` line ending in the compiler's own `cannot read yet`
//! sentence, before any test code ran ([`super::adapter::unread`]). That
//! sentence is captured per test and carried as the verdict's note, so the
//! census is a fold over verdicts the run already reached and nothing is
//! run twice: `--census` is one more rendering of the same lane.
//!
//! **Why it is worth a rendering.** Unread stops a test before anything
//! else can: the corpus at a44c6b3 reads 1,432 of 7,856 run refused this
//! way, over 46 constructs. Which construct holds the most tests
//! is therefore the sharpest thing the corpus can say about what to build
//! next, and until this it was counted by eye, one subset at a time, into
//! the body of each ticket that named a construct (#580, #581, #582, #583).
//!
//! The note is grouped by equality and never normalised. The compiler's
//! refusals are a closed set of sentences it writes itself, so two tests
//! stopped by one construct carry one string; a sentence that carried a
//! name the program chose would group only with itself, which is the
//! honest reading of it and not a bucket to be widened by guesswork.

use std::collections::BTreeMap;

use super::{Judgement, Tally};

/// At most this many tests are named under a group; the rest are counted.
/// The house's rendering of a long list (`reading::Log::unreadable_note`).
const NAMED: usize = 5;

/// One construct and the tests it stopped, as [`Census::rows`] orders them.
pub(crate) struct Row {
    pub construct: String,
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

/// The unread verdicts of one run, or of a lane's subsets folded together.
#[derive(Default)]
pub(crate) struct Census {
    by_construct: BTreeMap<String, Vec<String>>,
}

impl Census {
    /// Every unread verdict of `tally` added under its construct. A
    /// verdict of any other judgement is not the compiler's refusal and is
    /// not counted here, whatever its note says.
    pub(crate) fn add(&mut self, tally: &Tally) {
        for judged in &tally.judged {
            if judged.judgement == Judgement::Unread {
                self.by_construct
                    .entry(judged.note.clone())
                    .or_default()
                    .push(judged.name.clone());
            }
        }
    }

    /// The tests counted, over every construct.
    pub(crate) fn unread(&self) -> usize {
        self.by_construct.values().map(Vec::len).sum()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.by_construct.is_empty()
    }

    /// The groups, the construct holding the most tests first. A tie is
    /// broken by the construct's own text, so the order is total and one
    /// run's census prints the same twice.
    pub(crate) fn rows(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = self
            .by_construct
            .iter()
            .map(|(construct, tests)| Row {
                construct: construct.clone(),
                tests: tests.clone(),
            })
            .collect();
        rows.sort_by(|a, b| {
            b.tests
                .len()
                .cmp(&a.tests.len())
                .then_with(|| a.construct.cmp(&b.construct))
        });
        rows
    }

    /// The census under `scope`, the widest construct first. An empty
    /// census says so rather than printing a heading over nothing: a
    /// subset with no unread test is the answer, not the absence of one.
    pub(crate) fn print(&self, scope: &str) {
        if self.is_empty() {
            println!("phpt census {scope}: no test was refused by the compiler");
            return;
        }
        let rows = self.rows();
        println!(
            "phpt census {scope}: {} unread over {} construct{}",
            self.unread(),
            rows.len(),
            if rows.len() == 1 { "" } else { "s" }
        );
        for row in &rows {
            println!(
                "  {:>5}  {}: {}",
                row.tests.len(),
                row.construct,
                row.named()
            );
        }
    }
}
