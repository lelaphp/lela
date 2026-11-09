//! `ticket ready --found <file>` and `--found-nothing`: the found work a
//! session met, each item with the outcome its verifier gave it, held to
//! the tickets the ready record names (Round 2, Q15 to Q20 (a); CONTEXT.md,
//! Found work and Verifying).
//!
//! The file is the handback's `## Found work` section with each item's
//! outcome written first, one item per line; the section alone where the
//! file holds that heading, and the whole file where it does not, so a
//! handback passes as it stands. An item is a line, a list marker before it
//! or not, that opens with one of the six outcomes, its backticks optional:
//!
//! * `absorb`: the ticket cannot pass without it, so it was the ticket's
//!   own work and not found work.
//! * `build`: a fresh builder builds it on the same ticket, as a box there.
//! * `file`: filed as a ticket of its own, `file #<n>: <the item>`.
//! * `person`: filed `for-a-person`, `person #<n>: <the item>`.
//! * `refuted`: the claim does not hold at the head the verifier read.
//! * `noted`: true, with nothing to do, and kept on the reading.
//!
//! The number after the outcome is the ticket the item was filed as: `file`
//! and `person` carry one, the other four none, so the one slot means one
//! thing. A ticket the text cites is prose. A section of `None.` alone is
//! found work of no item, as `--found-nothing` is. A line that opens with
//! no outcome, a `file` or `person` item that names no ticket and one of
//! the other four that names one are refused before the tracker is asked
//! anything ([`Found::named`]), and so is a file that holds no item.
//!
//! `ready` refuses a file naming a filed ticket that `--opens` does not
//! (`Found::unopened`), and the reading counts the items by outcome; what
//! the reading cannot hold to anything, as `--opens-nothing` cannot, is
//! that the section names every item the session met.

use std::path::Path;

use anyhow::{Context, Result, bail};

use super::Successors;
use crate::reading::{Reading, joined};
use crate::track::Ticket;

/// The heading of the handback's section, matched without regard to case
/// at level one or two.
const HEADING: &str = "found work";
/// The line a section with no item holds.
const NONE: &str = "None.";
/// The count of every item, whatever its outcome.
const FOUND: &str = "found";

/// What one item of found work ended as, its verifier's word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Absorb,
    Build,
    File,
    Person,
    Refuted,
    Noted,
}

impl Outcome {
    /// Every outcome, in the order a reading's message names them.
    const ALL: [Outcome; 6] = [
        Outcome::Absorb,
        Outcome::Build,
        Outcome::File,
        Outcome::Person,
        Outcome::Refuted,
        Outcome::Noted,
    ];

    /// The word the file and the reading's count spell it with.
    fn word(self) -> &'static str {
        match self {
            Outcome::Absorb => "absorb",
            Outcome::Build => "build",
            Outcome::File => "file",
            Outcome::Person => "person",
            Outcome::Refuted => "refuted",
            Outcome::Noted => "noted",
        }
    }

    /// Whether an item of this outcome was filed as a ticket, and so names
    /// it after the outcome.
    fn files(self) -> bool {
        matches!(self, Outcome::File | Outcome::Person)
    }

    fn of_word(word: &str) -> Option<Outcome> {
        Outcome::ALL.into_iter().find(|o| o.word() == word)
    }
}

/// One item of found work: its outcome, the ticket it was filed as where
/// the outcome files one, and its text with the verifier's citation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    outcome: Outcome,
    filed: Option<Ticket>,
    text: String,
}

/// What `ticket ready` names as found: the items of a file, or the
/// assertion that there were none. One of the two, always.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    Items(Vec<Item>),
    Nothing,
}

impl Found {
    /// From the flags of `ready`: `--found <file>`, read and parsed, or
    /// `--found-nothing`. Refuses a call that gives neither, one that gives
    /// both, a file that cannot be read and one [`Found::parse`] refuses.
    pub fn named(file: Option<&Path>, nothing: bool) -> Result<Found> {
        match (file, nothing) {
            (None, false) => bail!(
                "`xtask ticket ready` names what the work found: `--found <file>`, the `## Found work` section with each item's outcome, or `--found-nothing`; without one of the two nothing is recorded"
            ),
            (Some(file), true) => bail!(
                "`--found {}` and `--found-nothing` contradict each other; give one of the two",
                file.display()
            ),
            (None, true) => Ok(Found::Nothing),
            (Some(file), false) => {
                let text = std::fs::read_to_string(file)
                    .with_context(|| format!("--found {}", file.display()))?;
                Found::parse(&text).with_context(|| format!("--found {}", file.display()))
            }
        }
    }

    /// The items `text` holds, as the module says; `Nothing` for a section
    /// of `None.` alone.
    fn parse(text: &str) -> Result<Found> {
        let section = section(text);
        let lines = section
            .iter()
            .map(|&(at, line)| (at, line.trim()))
            .filter(|(_, line)| !line.is_empty())
            .collect::<Vec<_>>();
        if let [(_, line)] = lines[..]
            && line == NONE
        {
            return Ok(Found::Nothing);
        }
        if lines.is_empty() {
            bail!(
                "holds no item; a section with none is `{NONE}`, and `--found-nothing` says the same"
            );
        }
        lines
            .into_iter()
            .map(|(at, line)| item(line).with_context(|| format!("line {at}: {line:?}")))
            .collect::<Result<Vec<_>>>()
            .map(Found::Items)
    }

    fn items(&self) -> &[Item] {
        match self {
            Found::Items(items) => items,
            Found::Nothing => &[],
        }
    }

    /// The tickets the items name as filed that `successors` do not: what
    /// `ready` refuses while any is left.
    pub(super) fn unopened(&self, successors: &Successors) -> Vec<Ticket> {
        let mut unopened: Vec<Ticket> = Vec::new();
        for ticket in self.items().iter().filter_map(|item| item.filed) {
            if !successors.tickets().contains(&ticket) && !unopened.contains(&ticket) {
                unopened.push(ticket);
            }
        }
        unopened
    }

    /// The counts a ready reading carries: every item as `found`, and each
    /// outcome under its word, a zero where none ended so.
    pub(super) fn count(&self, reading: &mut Reading) -> Result<()> {
        reading.count(FOUND, self.items().len() as u64)?;
        for outcome in Outcome::ALL {
            reading.count(outcome.word(), self.of(outcome).count() as u64)?;
        }
        Ok(())
    }

    fn of(&self, outcome: Outcome) -> impl Iterator<Item = &Item> {
        self.items()
            .iter()
            .filter(move |item| item.outcome == outcome)
    }

    /// What the reading's message says: the items by outcome and each
    /// `noted` item whole, since the reading is where it is kept, or the
    /// assertion with the session that made it.
    pub(super) fn describe(&self, session_id: Option<&str>) -> String {
        let items = self.items();
        if items.is_empty() {
            return match session_id {
                Some(id) => format!("found nothing, asserted in session {id}"),
                None => "found nothing, asserted with no session named".to_owned(),
            };
        }
        let by = Outcome::ALL
            .into_iter()
            .map(|outcome| (outcome, self.of(outcome).count()))
            .filter(|&(_, n)| n > 0)
            .map(|(outcome, n)| format!("{n} {}", outcome.word()));
        let said = format!("found {}: {}", items.len(), joined(by, ", "));
        let noted = joined(self.of(Outcome::Noted).map(|item| item.text.as_str()), "; ");
        if noted.is_empty() {
            said
        } else {
            format!("{said}; noted: {noted}")
        }
    }
}

/// The lines of the `## Found work` section with their numbers in the
/// file, up to the next heading of level one or two; every line where the
/// file holds no such heading.
fn section(text: &str) -> Vec<(usize, &str)> {
    let numbered = text.lines().enumerate().map(|(i, line)| (i + 1, line));
    let opens = |line: &str| super::heading(line).is_some_and(|t| t.eq_ignore_ascii_case(HEADING));
    if !text.lines().any(opens) {
        return numbered.collect();
    }
    numbered
        .skip_while(|&(_, line)| !opens(line))
        .skip(1)
        .take_while(|&(_, line)| super::heading(line).is_none())
        .collect()
}

/// One item from its line, held to the grammar the module states.
fn item(line: &str) -> Result<Item> {
    let line = ["- ", "* ", "+ "]
        .iter()
        .find_map(|marker| line.strip_prefix(marker))
        .unwrap_or(line)
        .trim_start();
    let end = line
        .find(|c: char| c.is_whitespace() || c == ':')
        .unwrap_or(line.len());
    let (word, rest) = line.split_at(end);
    let words = joined(Outcome::ALL.iter().map(|o| format!("`{}`", o.word())), ", ");
    let Some(outcome) = Outcome::of_word(word.trim_matches('`')) else {
        bail!("opens with no outcome; an item opens with one of {words}");
    };
    let rest = rest.trim_start();
    let (filed, rest) = match rest.strip_prefix('#') {
        Some(after) => {
            let digits = after
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(after.len());
            let number = after[..digits].parse().with_context(|| {
                format!("`#` after `{}` opens no ticket number", outcome.word())
            })?;
            (Some(Ticket::new(number)), &after[digits..])
        }
        None => (None, rest),
    };
    match (outcome.files(), filed) {
        (true, None) => bail!(
            "a `{0}` item names the ticket it was filed as after its outcome, `{0} #<n>: <the item>`",
            outcome.word()
        ),
        (false, Some(ticket)) => bail!(
            "a `{}` item files nothing, so no ticket follows its outcome; {ticket} cited is the item's text, after the colon",
            outcome.word()
        ),
        _ => {}
    }
    let text = rest
        .trim_start()
        .trim_start_matches([':', '-', '\u{2014}'])
        .trim();
    Ok(Item {
        outcome,
        filed,
        text: text.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::{Found, Item, Outcome};
    use crate::ticket::Successors;
    use crate::track::Ticket;

    /// Each outcome is read, a filed ticket after `file` and `person`, the
    /// list marker and the backticks optional; the section is read alone
    /// where the handback holds its heading.
    #[test]
    fn a_handback_s_section_reads_as_its_items_with_their_outcomes() {
        let handback = "Done.\n\n## Found work\n\n- `file` #41: the dtor runs twice, see php_heap/src/gc.rs:12\n* person #42: a ceiling\nnoted: the log is long\n\n## After\n\n- refuted: never read\n";
        let Found::Items(items) = Found::parse(handback).unwrap() else {
            panic!("items");
        };
        assert_eq!(
            items,
            vec![
                Item {
                    outcome: Outcome::File,
                    filed: Some(Ticket::new(41)),
                    text: "the dtor runs twice, see php_heap/src/gc.rs:12".to_owned(),
                },
                Item {
                    outcome: Outcome::Person,
                    filed: Some(Ticket::new(42)),
                    text: "a ceiling".to_owned(),
                },
                Item {
                    outcome: Outcome::Noted,
                    filed: None,
                    text: "the log is long".to_owned(),
                },
            ]
        );
        assert_eq!(
            Found::parse("## Found work\n\nNone.\n").unwrap(),
            Found::Nothing
        );
    }

    /// A line with no outcome, a filed outcome with no ticket, an unfiled
    /// one with a ticket and an empty section are refused, naming the line.
    #[test]
    fn a_line_out_of_the_grammar_is_refused_naming_it() {
        for (text, said) in [
            (
                "- the dtor runs twice",
                "line 1: \"- the dtor runs twice\": opens with no outcome",
            ),
            (
                "absorb: a\nfile: the dtor",
                "line 2: \"file: the dtor\": a `file` item names the ticket",
            ),
            ("noted #7: a", "a `noted` item files nothing"),
            ("## Found work\n\n", "holds no item"),
        ] {
            let err = format!("{:#}", Found::parse(text).unwrap_err());
            assert!(err.contains(said), "{text:?}: {err}");
        }
    }

    /// The tickets filed and not named by `--opens` are what `ready`
    /// refuses: none where every one is named, and every one for
    /// `--opens-nothing`.
    #[test]
    fn a_filed_ticket_the_successors_do_not_name_is_unopened() {
        let found = Found::parse("file #41: a\nperson #42: b\nbuild: c\n").unwrap();
        let opens = |n: &[u64]| Successors::Tickets(n.iter().copied().map(Ticket::new).collect());
        assert!(found.unopened(&opens(&[41, 42, 43])).is_empty());
        assert_eq!(found.unopened(&opens(&[41])), vec![Ticket::new(42)]);
        assert_eq!(
            found.unopened(&Successors::Nothing),
            vec![Ticket::new(41), Ticket::new(42)]
        );
    }
}
