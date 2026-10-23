//! The open tickets of a track as chains (harness-design.md §5, A track's
//! plan): `xtask track chains` prints every open ticket that is this
//! track's under the ticket that blocks it, so a session reads the whole
//! frontier and what waits behind it in one call instead of walking
//! `blockedBy` ticket by ticket. A ticket blocks as `track next` judges
//! it, [`crate::ticket::BlockedBy::open`]: while it is open (#2326). A chain starts at a ticket no open ticket
//! shown blocks and runs down through what it unblocks; a ticket with two
//! blockers is written out under the first by number and named under the
//! others. Tickets that block nothing and wait on nothing follow the
//! chains, on their own. Every line carries the ticket's standing as
//! `track next` judges it: `ready` for `ready-for-agent`, `a person's` for
//! `for-a-person`, the branch of this clone that records it, and the
//! tickets it waits on, this track's or not. Reads the tracker through
//! [`Gh::open`], the ticket module's one list, and the branch records
//! once, and writes no reading.
//!
//! `--crate <name>` keeps the tickets whose title names the crate before
//! its colon, the shape the frontier skill files them in (`php_engine,
//! ext/standard: ...`); `--crate none` keeps the ones naming no crate, the
//! manifest rows, design revisions and decisions.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::manifest::{Layout, Manifest};
use crate::ticket::{Gh, Open};
use crate::track::{Records, Ticket};
use crate::track_next::{PERSON, READY, is_tracks};

/// The `--crate` value that keeps the tickets naming no crate.
const NO_CRATE: &str = "none";

/// One open ticket of the track: its standing and its edges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Row {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub labels: Vec<String>,
    /// The branch of this clone that records the ticket, when one does.
    pub branch: Option<String>,
    /// The open tickets blocking it, shown here or not.
    pub waits_on: Vec<u64>,
    /// The tickets shown here that it blocks.
    pub blocks: Vec<u64>,
}

impl Row {
    /// The standing in brackets: the label's word, the branch, what it
    /// waits on; `[ready, waits on #12, #14 and 3 more]`.
    fn standing(&self) -> String {
        let word = if self.labels.iter().any(|l| l == PERSON) {
            "a person's"
        } else if self.labels.iter().any(|l| l == READY) {
            "ready"
        } else {
            "no label"
        };
        let mut parts = vec![word.to_owned()];
        if let Some(branch) = &self.branch {
            parts.push(format!("on {branch}"));
        }
        if !self.waits_on.is_empty() {
            parts.push(format!("waits on {}", cited(&self.waits_on)));
        }
        parts.join(", ")
    }
}

/// Numbers as they are cited, three at most: `#12, #14 and 3 more`.
fn cited(numbers: &[u64]) -> String {
    let shown: Vec<String> = numbers.iter().take(3).map(|n| format!("#{n}")).collect();
    match numbers.len() {
        0..=3 => shown.join(", "),
        more => format!("{} and {} more", shown.join(", "), more - 3),
    }
}

/// The chains of one track: its rows by number, each knowing what it
/// waits on and what it blocks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Chains {
    pub track: String,
    pub rows: Vec<Row>,
}

impl Chains {
    /// The rows of `track` among the open tickets, `krate` narrowing them
    /// to the ones naming it, a crate being one `layout` lays down,
    /// `records` answering what the branches of the clone record; a
    /// function of the tracker's answer and those.
    pub(crate) fn of(
        open: Vec<Open>,
        track: &str,
        krate: Option<(&str, &Layout)>,
        records: &Records,
    ) -> Chains {
        let mut kept: Vec<Open> = open
            .into_iter()
            .filter(|ticket| is_tracks(ticket, track))
            .filter(|ticket| krate.is_none_or(|(name, layout)| names(&ticket.title, name, layout)))
            .collect();
        kept.sort_by_key(|ticket| ticket.number);
        let shown: BTreeSet<u64> = kept.iter().map(|ticket| ticket.number).collect();
        let mut blocks: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        for ticket in &kept {
            for blocker in ticket.blocked_by.open().filter(|n| shown.contains(n)) {
                blocks.entry(blocker).or_default().push(ticket.number);
            }
        }
        let rows = kept
            .into_iter()
            .map(|ticket| Row {
                branch: records
                    .branch(Ticket::new(ticket.number))
                    .map(str::to_owned),
                waits_on: ticket.blocked_by.open().collect(),
                blocks: blocks.remove(&ticket.number).unwrap_or_default(),
                labels: ticket.label_names().map(str::to_owned).collect(),
                number: ticket.number,
                title: ticket.title,
                url: ticket.url,
            })
            .collect();
        Chains {
            track: track.to_owned(),
            rows,
        }
    }

    /// The text: one line of counts, the chains, then the tickets on their
    /// own; a ticket reached twice is named where it was written out.
    pub fn render(&self) -> String {
        let by: BTreeMap<u64, &Row> = self.rows.iter().map(|row| (row.number, row)).collect();
        let waiting = self
            .rows
            .iter()
            .filter(|row| !row.waits_on.is_empty())
            .count();
        let person = self
            .rows
            .iter()
            .filter(|row| row.labels.iter().any(|l| l == PERSON))
            .count();
        let mut out = format!(
            "{}: {} open, {} no ticket blocks, {waiting} waiting on a blocker, {person} a person's\n",
            self.track,
            self.rows.len(),
            self.rows.len() - waiting,
        );
        let roots = self
            .rows
            .iter()
            .filter(|row| row.waits_on.iter().all(|n| !by.contains_key(n)));
        let (linked, alone): (Vec<&Row>, Vec<&Row>) = roots.partition(|row| !row.blocks.is_empty());
        let mut seen = BTreeSet::new();
        let mut under = BTreeMap::new();
        for root in linked {
            out.push('\n');
            write_chain(&mut out, root, 0, None, &by, &mut seen, &mut under);
        }
        if !alone.is_empty() {
            writeln!(
                out,
                "\n{} on their own: no open ticket shown blocks them and they block nothing shown",
                alone.len()
            )
            .ok();
            for row in alone {
                writeln!(out, "#{} {} [{}]", row.number, row.title, row.standing()).ok();
            }
        }
        let cycle: Vec<&Row> = self
            .rows
            .iter()
            .filter(|row| !seen.contains(&row.number) && !row.blocks.is_empty())
            .collect();
        if !cycle.is_empty() {
            writeln!(out, "\n{} in a cycle, each waiting on another", cycle.len()).ok();
            for row in cycle {
                writeln!(out, "#{} {} [{}]", row.number, row.title, row.standing()).ok();
            }
        }
        out
    }
}

/// One ticket and, indented under it, what it unblocks; `under` remembers
/// where each was written out so a second reaching names it.
fn write_chain(
    out: &mut String,
    row: &Row,
    depth: usize,
    parent: Option<u64>,
    by: &BTreeMap<u64, &Row>,
    seen: &mut BTreeSet<u64>,
    under: &mut BTreeMap<u64, u64>,
) {
    let indent = "  ".repeat(depth);
    if !seen.insert(row.number) {
        let first = under
            .get(&row.number)
            .map_or_else(String::new, |n| format!(" under #{n}"));
        writeln!(out, "{indent}#{}, written out above{first}", row.number).ok();
        return;
    }
    if let Some(parent) = parent {
        under.insert(row.number, parent);
    }
    writeln!(
        out,
        "{indent}#{} {} [{}]",
        row.number,
        row.title,
        row.standing()
    )
    .ok();
    for number in &row.blocks {
        if let Some(child) = by.get(number) {
            write_chain(out, child, depth + 1, Some(row.number), by, seen, under);
        }
    }
}

/// Whether the title names `krate` before its colon, the crates being
/// `php`, `xtask`, `php_*` and those under a directory of `layout`;
/// [`NO_CRATE`] matches a title naming none.
fn names(title: &str, krate: &str, layout: &Layout) -> bool {
    let named: Vec<&str> = title
        .split_once(':')
        .map(|(head, _)| {
            head.split(',')
                .filter_map(|part| part.split_whitespace().next())
                .filter(|word| is_crate(word, layout))
                .collect()
        })
        .unwrap_or_default();
    if krate == NO_CRATE {
        named.is_empty()
    } else {
        named.contains(&krate)
    }
}

/// Whether a word of a title's head is a workspace crate's name: the
/// harness, a `php_*` crate, or one under a directory `layout` lays crates
/// down in (`ext/pcre`, `sapi/cli`); `php`, the binary's crate before
/// ADR-0008, names the binary still.
fn is_crate(word: &str, layout: &Layout) -> bool {
    word == "php"
        || word == crate::layout::HARNESS
        || word.starts_with("php_")
        || word
            .split_once('/')
            .is_some_and(|(dir, _)| layout.dirs().any(|d| d == dir))
}

/// `xtask track chains`: the track named and its chains printed as text
/// or as one JSON object; no reading.
pub fn command(root: &Path, track: &str, krate: Option<&str>, json: bool) -> Result<()> {
    let layout = Manifest::load(root)?.layout();
    let open = Gh::on_path().open(root)?;
    let krate = krate.map(|name| (name, &layout));
    let chains = Chains::of(open, track, krate, &Records::read(root));
    if json {
        println!("{}", serde_json::to_string_pretty(&chains)?);
    } else {
        print!("{}", chains.render());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Chains, names};
    use crate::ticket::{BlockedBy, Blocker, Label, Open, State};
    use crate::track::{Records, Ticket};

    fn ticket(number: u64, title: &str, labels: &[&str], blocked_by: &[(u64, State)]) -> Open {
        Open {
            number,
            title: title.to_owned(),
            url: format!("https://example.test/{number}"),
            labels: labels
                .iter()
                .map(|name| Label {
                    name: (*name).to_owned(),
                })
                .collect(),
            blocked_by: BlockedBy {
                nodes: blocked_by
                    .iter()
                    .map(|(number, state)| Blocker {
                        number: *number,
                        state: *state,
                    })
                    .collect(),
            },
        }
    }

    /// A chain runs from the blocker down; a ticket with two blockers is
    /// written out under the first and named under the second; a closed
    /// blocker is no wait, whether or not `main` holds the branch
    /// recording it (#2326); a ticket with no edge stands on its own; a blocker
    /// labelled another track is a wait but not a row.
    #[test]
    fn chains_are_written_from_the_blocker_down() {
        let open = vec![
            ticket(1, "php_engine: a", &["ready-for-agent"], &[]),
            ticket(
                2,
                "php_compile: b",
                &["ready-for-agent"],
                &[(1, State::Open), (9, State::Closed)],
            ),
            ticket(
                3,
                "seams.toml: c",
                &["for-a-person"],
                &[(1, State::Open), (2, State::Open)],
            ),
            ticket(4, "xtask: d", &[], &[(7, State::Open)]),
            ticket(5, "ext/json: e", &["ready-for-agent"], &[]),
            ticket(
                6,
                "ext/pcre: g",
                &["ready-for-agent"],
                &[(8, State::Closed)],
            ),
            ticket(7, "php_heap: f", &["ready-for-agent", "track:other"], &[]),
        ];
        let recorded = vec![
            ("t5".to_owned(), Ticket::new(5)),
            ("t8".to_owned(), Ticket::new(8)),
            ("t9".to_owned(), Ticket::new(9)),
        ];
        let records = Records::of(recorded);
        let chains = Chains::of(open, "main", None, &records);
        assert_eq!(
            chains.render(),
            "main: 6 open, 3 no ticket blocks, 3 waiting on a blocker, 1 a person's\n\
             \n\
             #1 php_engine: a [ready]\n\
             \x20 #2 php_compile: b [ready, waits on #1]\n\
             \x20   #3 seams.toml: c [a person's, waits on #1, #2]\n\
             \x20 #3, written out above under #2\n\
             \n\
             3 on their own: no open ticket shown blocks them and they block nothing shown\n\
             #4 xtask: d [no label, waits on #7]\n\
             #5 ext/json: e [ready, on t5]\n\
             #6 ext/pcre: g [ready]\n"
        );
    }

    /// `--crate` keeps the tickets naming the crate before the colon, and
    /// `none` the ones naming no crate.
    #[test]
    fn a_crate_is_named_before_the_colon() {
        let rows = ["php_engine", "ext/standard"].map(crate::manifest::Crate::named);
        let layout = &super::Layout::of(&rows);
        let names = |title, krate| names(title, krate, layout);
        assert!(names("php_engine, ext/standard: a native", "ext/standard"));
        assert!(names("php_engine api: a lock line", "php_engine"));
        assert!(!names("php_engine: a native", "php_compile"));
        assert!(names("seams.toml: a row", "none"));
        assert!(names("a title with no colon", "none"));
        assert!(!names("php_engine: a native", "none"));
        assert!(names("docs/adr: a reversal", "none"));
        assert!(names("sapi/cli: a host", "none"));
    }
}
