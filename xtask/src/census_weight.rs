//! What a ticket is worth to the frontier, read off the unread census:
//! the number of refused tests the constructs its title names
//! would let through.
//!
//! `xtask phpt --census` carries a count per construct on each `phpt`
//! reading's `groups`. This reads them back from the store and weighs a
//! ticket by them, so `track next` can hand out the widest work rather
//! than the oldest.
//!
//! **The matching is deliberately coarse and deliberately conservative.**
//! A census key is a sentence `php_compile` wrote about itself, `a
//! `global` statement this compiler cannot read yet`; a ticket title is a
//! sentence a planner wrote, `php_compile, php_engine: the global
//! statement, BIND_GLOBAL binding a CV to the request's symbol table`.
//! Nothing joins the two but the words they share. So the boilerplate
//! tail and the words that carry no subject are dropped from the key, and
//! a ticket takes the group only when **every** word left stands in its
//! title. A group that matches nothing weighs nothing, and a ticket that
//! matches nothing sorts where it sorts today, by number: a weight this
//! cannot find is the floor it was already standing on, while a weight it
//! finds wrongly would send a track after the wrong work, which is the
//! failure worth avoiding.
//!
//! Two tickets may take one group, as two whose titles both name `eval`
//! take the `eval` group. That is honest: either of them lets those tests
//! through, and the census cannot say which. It counts for both.

use std::collections::{BTreeMap, HashSet};

use anyhow::Result;
use toml::value::Datetime;

use crate::reading::{Number, Store};

/// The words a census key carries that name no subject, dropped before a
/// title is held to it: the refusal's own boilerplate and the grammar
/// around the construct.
const EMPTY: &[&str] = &[
    "this", "compiler", "cannot", "read", "yet", "a", "an", "the", "of", "whose", "what", "is",
    "no", "to", "in", "at", "and", "or", "over", "sent", "has", "not", "with", "for", "its", "by",
    "that", "it", "one", "this",
];

/// The counts per construct of the store's `phpt` readings, and when the
/// newest of them was taken.
#[derive(Debug, Default)]
pub(crate) struct Weights {
    by_construct: BTreeMap<String, u64>,
    at: Option<Datetime>,
}

/// One group a ticket took, as `--why` prints it.
pub(crate) struct Why {
    pub construct: String,
    pub tests: u64,
}

impl Weights {
    /// The census as the store holds it: every `phpt` reading's `groups`
    /// summed by construct, since a construct refuses tests in more than
    /// one subset and the frontier is one list over all of them.
    pub(crate) fn of_store(store: &Store) -> Result<Weights> {
        let mut weights = Weights::default();
        for reading in store.readings()?.readings {
            if reading.instrument != "phpt" {
                continue;
            }
            for (construct, number) in &reading.groups {
                let n = match number {
                    Number::Int(n) => u64::try_from(*n).unwrap_or(0),
                    Number::Float(_) => 0,
                };
                *weights.by_construct.entry(construct.clone()).or_default() += n;
            }
            if !reading.groups.is_empty() && weights.at.as_ref().is_none_or(|at| at < &reading.at) {
                weights.at = Some(reading.at);
            }
        }
        Ok(weights)
    }

    /// Whether the store holds a census at all. Without one the frontier
    /// is ordered as it always was and says so.
    pub(crate) fn is_empty(&self) -> bool {
        self.by_construct.is_empty()
    }

    /// When the census this weighs by was taken, so a stale ranking is
    /// visible rather than silent.
    pub(crate) fn at(&self) -> Option<&Datetime> {
        self.at.as_ref()
    }

    /// What `title` is worth, and the groups behind it, widest first.
    pub(crate) fn weigh(&self, title: &str) -> (u64, Vec<Why>) {
        let words = words_of(title);
        let mut why: Vec<Why> = self
            .by_construct
            .iter()
            .filter(|(construct, _)| names(construct, &words))
            .map(|(construct, tests)| Why {
                construct: construct.clone(),
                tests: *tests,
            })
            .collect();
        why.sort_by(|a, b| {
            b.tests
                .cmp(&a.tests)
                .then_with(|| a.construct.cmp(&b.construct))
        });
        (why.iter().map(|one| one.tests).sum(), why)
    }
}

/// Whether the title's words name the construct: every word of the key
/// that carries a subject stands among them. A key that is all
/// boilerplate names nothing, since every title would take it.
///
/// A word php spells with punctuation, such as the class constant
/// fetch, the increment or the nullsafe arrow, is held as a part of a
/// word rather than a whole one, because a title writes it attached to
/// the name it acts on: the census names the operator alone where the
/// ticket writes `Foo::class`, and they are the same construct. A word of
/// letters alone is held whole, so `print` does not take a title that
/// says `sprintf`.
fn names(construct: &str, title: &HashSet<String>) -> bool {
    let mut carried = words_of(construct)
        .into_iter()
        .filter(|word| !EMPTY.contains(&word.as_str()))
        .peekable();
    carried.peek().is_some()
        && carried.all(|word| {
            if word.chars().all(char::is_alphanumeric) {
                title.contains(&word)
            } else {
                title.iter().any(|said| said.contains(&word))
            }
        })
}

/// A sentence as the words this compares: lower case, the backticks and
/// the prose punctuation gone, and the spellings php writes kept whole,
/// so `::class`, `++` and `?->` are each one word and not none.
fn words_of(sentence: &str) -> HashSet<String> {
    sentence
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '_' | ':' | '+' | '-' | '>' | '?') {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// A census of the given rows, for the tests of this module and of the
/// frontier that orders by it.
#[cfg(test)]
pub(crate) fn weighing_for_tests(rows: &[(&str, u64)]) -> Weights {
    let mut weights = Weights::default();
    for (construct, tests) in rows {
        weights.by_construct.insert((*construct).to_owned(), *tests);
    }
    weights
}

#[cfg(test)]
mod tests {
    use super::weighing_for_tests as weighing;

    /// The real rows of the lane at d2bc7f7 against the real titles of the
    /// tickets open for them: each takes its own group and no other's.
    #[test]
    fn a_title_takes_the_group_whose_every_carrying_word_it_names() {
        let weights = weighing(&[
            (
                "a class reference of the scope this compiler cannot read yet",
                206,
            ),
            ("a `print` this compiler cannot read yet", 172),
            ("a `global` statement this compiler cannot read yet", 75),
            ("a `::class` this compiler cannot read yet", 61),
        ]);

        let (weight, why) = weights.weigh(
            "php_compile, php_engine: static as a class reference, zend_compile_class_ref's ZEND_FETCH_CLASS_STATIC over the frame's called scope",
        );
        assert_eq!(weight, 206);
        assert_eq!(why.len(), 1);

        let (weight, _) = weights.weigh(
            "php_compile, php_engine: the global statement, BIND_GLOBAL binding a CV to the request's symbol table",
        );
        assert_eq!(weight, 75);

        // `::class` survives the punctuation, which is the whole of that key.
        let (weight, _) =
            weights.weigh("php_compile: Foo::class, the class name zend_compile_class_const folds");
        assert_eq!(weight, 61);
    }

    /// One group taken by two tickets is counted for both: either of them
    /// lets those tests through and the census cannot say which.
    #[test]
    fn two_tickets_that_name_one_construct_each_take_its_whole_weight() {
        let weights = weighing(&[("an `eval` this compiler cannot read yet", 40)]);
        let first = weights.weigh(
            "php_compile: eval as INCLUDE_OR_EVAL and print as ECHO, not a call to a function of that name",
        );
        let second = weights
            .weigh("php_compile, php_engine: eval is the fifth arm of INCLUDE_OR_EVAL, a request-scoped unit on a code frame");
        assert_eq!(first.0, 40);
        assert_eq!(second.0, 40);
    }

    /// A title that names none of the census's constructs weighs nothing,
    /// which is the floor: it sorts by number, as every ticket did before
    /// the census existed. A key must carry a word of its own to be taken
    /// at all, so a title does not collect a group by its boilerplate.
    #[test]
    fn a_title_that_names_no_construct_weighs_nothing() {
        let weights = weighing(&[
            ("a `print` this compiler cannot read yet", 172),
            ("this compiler cannot read yet", 9),
        ]);
        let (weight, why) = weights
            .weigh("xtask witness: the linked adapter serves the snippet as Standard input code");
        assert_eq!(weight, 0);
        assert!(why.is_empty());

        // The all-boilerplate key is taken by nothing, including a title
        // that happens to repeat the words.
        let (weight, _) = weights.weigh("php_compile: this compiler cannot read yet");
        assert_eq!(weight, 0);
    }

    /// A partial naming is not a naming: `print` alone does not take the
    /// group that asks for a print inside a string.
    #[test]
    fn every_carrying_word_is_required_and_not_merely_one() {
        let weights = weighing(&[(
            "a `print` inside a string this compiler cannot read yet",
            12,
        )]);
        assert_eq!(weights.weigh("php_compile: print as ECHO").0, 0);
        assert_eq!(
            weights
                .weigh("php_compile: a print inside a string, the encaps list")
                .0,
            12
        );
    }

    /// The weight sums the groups a title takes, and `weigh` orders them
    /// widest first so `--why` reads as the census does.
    #[test]
    fn a_title_that_names_two_constructs_sums_them_widest_first() {
        let weights = weighing(&[
            ("a `print` this compiler cannot read yet", 172),
            ("an `eval` this compiler cannot read yet", 40),
        ]);
        let (weight, why) = weights.weigh(
            "php_compile: eval as INCLUDE_OR_EVAL and print as ECHO, not a call to a function of that name",
        );
        assert_eq!(weight, 212);
        assert_eq!(why.len(), 2);
        assert_eq!(why[0].tests, 172);
        assert_eq!(why[1].tests, 40);
    }
}
