//! The ticket a track works next (#73; harness-design.md §5, A track's
//! plan; CONTEXT.md, Track, Frontier, Ticket): `xtask track next` answers
//! it from the tracker's open tickets and the repository's branch
//! records, as text or as one JSON object, and writes no reading. The
//! frontier is the open tickets labelled `ready-for-agent` that no ticket
//! blocks while it is open or while the branch that records it is not yet
//! in `main`, that are the track's, and that no branch of the repository
//! records; the next is the widest of them by the census, the number
//! breaking a tie (#618). An empty frontier is stated, "none", and is not
//! an error, since a track with nothing to work has not failed.
//!
//! A track is the tickets that carry its `track:<name>` label, and a
//! ticket with no `track:` label is `main`'s alone (ADR-0009), so every
//! ticket is some track's and two runs on different tracks never compete
//! for an unlabelled one; [`tracks_of`] is the one statement of that.
//! A closed blocker whose branch `main` does not contain yet still blocks
//! (#1843, #1850), so a ticket is cut only from a `main` that holds what
//! it waited on, even when the same run closed its blocker;
//! [`waits_on`] is the one statement of that. "No branch yet" and "not
//! yet in `main`" are the repository's own records, `branch.<name>.ticket`
//! and git's reachability, read once per answer as [`Records`].
//! The tracker is reached through [`Gh::open`], the ticket module's one
//! list, and through nothing else here. The same list answers what the
//! track waits on (#85): its open tickets labelled `for-a-person`, which
//! `track work` prints when the frontier stays empty.

use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

use crate::census_weight::Weights;
use crate::reading::Store;
use crate::ticket::{Gh, Open, State};
use crate::track::{MAIN, Records, Ticket};
use toml::value::Datetime;

/// The label a ticket an agent may take carries. The frontier reads it
/// here, `ticket` holds such a ticket's criteria to what an agent may
/// write and `track chains` names it on a line; one statement, three
/// readers.
pub(crate) const READY: &str = "ready-for-agent";
/// The label a ticket that is a person's alone carries (harness §11).
pub(crate) const PERSON: &str = "for-a-person";
/// The prefix of the label that names the one track working a ticket.
/// The frontier reads it here and `ticket` holds a close's successors to
/// it; one statement, two readers.
pub(crate) const TRACK_LABEL: &str = "track:";

/// The ticket a track works next: its number, title and url, and its
/// labels as the tracker spells them, which `track work` reads the
/// session's skill from (#81).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Next {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub labels: Vec<String>,
    /// The refused tests the census says this ticket's constructs would
    /// let through (#618); zero where the census weighs it at nothing and
    /// where the store holds no census at all.
    pub weight: u64,
}

impl Next {
    /// One line: the ticket as it is cited and its title, or "none" with
    /// the track whose frontier is empty. The weight is not on this line:
    /// it is what `--why` asks for, and every caller of this one reads a
    /// ticket number out of it.
    pub fn render(found: Option<&Next>, track: &str) -> String {
        match found {
            Some(next) => format!("#{} {}\n", next.number, next.title),
            None => format!("none: the frontier of {track} is empty\n"),
        }
    }

    /// Why this ticket and not another: the weight, the census rows
    /// behind it and when that census was taken, so a ranking made on a
    /// stale reading is seen to be one. A frontier the census weighs at
    /// nothing says that instead, since the order is then the filing
    /// order it always was.
    pub(crate) fn why(found: Option<&Next>, weights: &Weights, track: &str) -> String {
        let Some(next) = found else {
            return format!("none: the frontier of {track} is empty\n");
        };
        let mut said = vec![format!("#{} {}", next.number, next.title)];
        if weights.is_empty() {
            said.push(
                "  the store holds no unread census, so the frontier is ordered by number, the tracker's order of filing; run `xtask phpt --census`".to_owned(),
            );
            return said.join("\n") + "\n";
        }
        let (weight, why) = weights.weigh(&next.title);
        said.push(match weights.at() {
            Some(at) => format!("  weight {weight}, off the census of {at}"),
            None => format!("  weight {weight}"),
        });
        if why.is_empty() {
            said.push(
                "  no construct of the census stands in this title, so it was taken by number"
                    .to_owned(),
            );
        }
        said.extend(
            why.iter()
                .map(|one| format!("  {:>5}  {}", one.tests, one.construct)),
        );
        said.join("\n") + "\n"
    }

    /// The same as one object: `ticket`, `title` and `url` are null for
    /// an empty frontier.
    pub fn to_json(found: Option<&Next>, track: &str, census_at: Option<&Datetime>) -> Value {
        json!({
            "track": track,
            "ticket": found.map(|next| next.number),
            "title": found.map(|next| next.title.as_str()),
            "url": found.map(|next| next.url.as_str()),
            "weight": found.map(|next| next.weight),
            "census_at": census_at.map(ToString::to_string),
        })
    }
}

/// The ticket `track` works next in the clone at `root`, `None` for an
/// empty frontier.
pub fn next(root: &Path, gh: &Gh, track: &str) -> Result<Option<Next>> {
    let open = gh.open(root)?;
    let weights = Weights::of_store(&Store::of_workspace(root))?;
    let records = Records::read(root)?;
    Ok(frontier(&open, track, &weights, &records).map(|found| {
        let mut next = Next::from(found.clone());
        next.weight = weights.weigh(&next.title).0;
        next
    }))
}

/// The open tickets `track` waits on a person for: those labelled
/// `for-a-person` that are not another track's, by number.
pub fn for_a_person(root: &Path, gh: &Gh, track: &str) -> Result<Vec<Next>> {
    let mut open = gh.open(root)?;
    open.retain(|ticket| ticket.label_names().any(|name| name == PERSON));
    open.retain(|ticket| is_tracks(ticket, track));
    open.sort_by_key(|ticket| ticket.number);
    Ok(open.into_iter().map(Next::from).collect())
}

impl From<Open> for Next {
    fn from(open: Open) -> Next {
        let labels = open.label_names().map(str::to_owned).collect();
        Next {
            number: open.number,
            title: open.title,
            url: open.url,
            labels,
            weight: 0,
        }
    }
}

/// The widest of the open tickets that pass the four filters, `records`
/// answering what the branches of the clone record; a function of the
/// tracker's answer, those records and the census the store holds.
///
/// Widest is the census's count of the refused tests the ticket's
/// constructs would let through (#617, #618), and the tie is the number,
/// the tracker's order of filing. A frontier the census weighs at nothing
/// — because no census has been run, or because none of these tickets
/// names a construct it counted — is every weight zero and so every tie,
/// which is the order this had before the census existed.
fn frontier<'a>(
    open: &'a [Open],
    track: &str,
    weights: &Weights,
    records: &Records,
) -> Option<&'a Open> {
    open.iter()
        .filter(|ticket| ticket.label_names().any(|name| name == READY))
        .filter(|ticket| waits_on(ticket, records).next().is_none())
        .filter(|ticket| is_tracks(ticket, track))
        .filter(|ticket| records.branch(Ticket::new(ticket.number)).is_none())
        .min_by_key(|ticket| {
            let weight = weights.weigh(&ticket.title).0;
            (std::cmp::Reverse(weight), ticket.number)
        })
}

/// Whether `track` works the ticket: it is one of the ticket's
/// [`tracks_of`]. `track chains` and [`for_a_person`] keep the same
/// tickets.
pub(crate) fn is_tracks(ticket: &Open, track: &str) -> bool {
    tracks_of(ticket.label_names()).contains(&track)
}

/// The tracks a ticket's labels put it on: the name of each `track:`
/// label it carries, and [`MAIN`] alone where it carries none (ADR-0009),
/// so every ticket is some track's. The frontier, the chains, what a
/// track waits on and a close's successors all read the rule here.
pub(crate) fn tracks_of<'a>(labels: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    let named: Vec<&str> = labels
        .into_iter()
        .filter_map(|name| name.strip_prefix(TRACK_LABEL))
        .collect();
    if named.is_empty() { vec![MAIN] } else { named }
}

/// The tickets that still block `ticket`, by number: each blocker open at
/// the tracker, and each closed one recorded by a branch `main` does not
/// contain yet (#1843, #1850), since its work is not in the `main` the
/// ticket would be cut from until /specs merges that branch. The
/// frontier and the chains read the rule here.
pub(crate) fn waits_on<'a>(
    ticket: &'a Open,
    records: &'a Records,
) -> impl Iterator<Item = u64> + 'a {
    ticket
        .blocked_by
        .nodes
        .iter()
        .filter(|blocker| {
            blocker.state == State::Open || records.unmerged(Ticket::new(blocker.number))
        })
        .map(|blocker| blocker.number)
}

/// `xtask track next`: the track named and the answer printed; no
/// reading.
pub fn command(root: &Path, track: &str, json: bool, why: bool) -> Result<()> {
    let found = next(root, &Gh::on_path(), track)?;
    let weights = Weights::of_store(&Store::of_workspace(root))?;
    if json {
        println!("{}", Next::to_json(found.as_ref(), track, weights.at()));
    } else if why {
        print!("{}", Next::why(found.as_ref(), &weights, track));
    } else {
        print!("{}", Next::render(found.as_ref(), track));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{Weights, frontier, is_tracks, waits_on};
    use crate::ticket::{BlockedBy, Blocker, Label, Open, State};
    use crate::track::{Records, Ticket};

    /// The records of branches `t<n>` for each of `on`, those of
    /// `unmerged` not in `main`.
    fn records(on: &[u64], unmerged: &[&str]) -> Records {
        let recorded = on
            .iter()
            .map(|&n| (format!("t{n}"), Ticket::new(n)))
            .collect();
        Records::of(recorded, &unmerged.iter().copied().collect::<BTreeSet<_>>())
    }

    /// An open, unblocked, ready ticket with no `track:` label, `main`'s,
    /// by number and title: what the frontier orders.
    fn ready(number: u64, title: &str) -> Open {
        Open {
            number,
            title: title.to_owned(),
            url: String::new(),
            labels: vec![Label {
                name: super::READY.to_owned(),
            }],
            blocked_by: BlockedBy::default(),
        }
    }

    fn weighing(rows: &[(&str, u64)]) -> Weights {
        crate::census_weight::weighing_for_tests(rows)
    }

    fn labelled(labels: &[&str]) -> Open {
        Open {
            number: 1,
            title: String::new(),
            url: String::new(),
            labels: labels
                .iter()
                .map(|name| Label {
                    name: (*name).to_owned(),
                })
                .collect(),
            blocked_by: BlockedBy::default(),
        }
    }

    /// The frontier is ordered by what the census weighs each ticket at,
    /// and the number breaks the tie: #3 is filed last and worked first
    /// because it names the construct holding the most refused tests,
    /// while #1 and #2 weigh nothing and keep their filing order behind
    /// it (#618).
    #[test]
    fn the_frontier_is_the_widest_ticket_and_the_number_breaks_the_tie() {
        let weights = weighing(&[
            ("a `global` statement this compiler cannot read yet", 75),
            ("a `print` this compiler cannot read yet", 172),
        ]);
        let open = vec![
            ready(1, "xtask witness: the linked adapter serves the snippet"),
            ready(
                2,
                "php_engine: a reference carries the typed properties bound to it",
            ),
            ready(
                3,
                "php_compile: print as ECHO, not a call to a function of that name",
            ),
            ready(
                4,
                "php_compile: the global statement, BIND_GLOBAL binding a CV",
            ),
        ];
        let none = records(&[], &[]);
        let found = frontier(&open, "main", &weights, &none).expect("a frontier");
        assert_eq!(found.number, 3, "the widest, not the oldest");

        // With the widest already on a branch, the next widest follows.
        let found = frontier(&open, "main", &weights, &records(&[3], &[])).expect("a frontier");
        assert_eq!(found.number, 4);

        // Weighed at nothing, the order is the filing order it always was.
        let found = frontier(&open, "main", &Weights::default(), &none).expect("a frontier");
        assert_eq!(found.number, 1);
    }

    /// A blocker blocks while it is open, or while the branch recording
    /// it is not in `main` (#1850): of #1 to #4, open, closed on `t2`
    /// not merged, closed on `t3` merged and closed with no branch, the
    /// first two wait.
    #[test]
    fn a_closed_blocker_waits_while_its_branch_is_not_in_main() {
        let mut ticket = ready(9, "ticket 9");
        ticket.blocked_by = BlockedBy {
            nodes: [
                (1, State::Open),
                (2, State::Closed),
                (3, State::Closed),
                (4, State::Closed),
            ]
            .into_iter()
            .map(|(number, state)| Blocker { number, state })
            .collect(),
        };
        let held = records(&[2, 3], &["t2"]);
        assert_eq!(waits_on(&ticket, &held).collect::<Vec<_>>(), [1, 2]);
    }

    /// No `track:` label is `main`'s alone (ADR-0009, #1836); one names
    /// its track alone; two name two.
    #[test]
    fn a_track_label_names_the_one_track_working_the_ticket() {
        assert!(is_tracks(&labelled(&["ready-for-agent"]), "main"));
        assert!(!is_tracks(&labelled(&["ready-for-agent"]), "parity-hosts"));
        assert!(is_tracks(&labelled(&["track:main"]), "main"));
        assert!(!is_tracks(&labelled(&["track:main"]), "t-x"));
        assert!(is_tracks(&labelled(&["track:main", "track:t-x"]), "t-x"));
        assert!(!is_tracks(&labelled(&["track:main", "track:t-x"]), "t-y"));
    }
}
