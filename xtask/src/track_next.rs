//! The ticket a track works next (harness-design.md §5, A track's
//! plan; CONTEXT.md, Track, Frontier, Ticket): `xtask track next` answers
//! it from the tracker's open tickets and the repository's branch
//! records, as text or as one JSON object, and writes no reading. The
//! frontier is the open tickets labelled `ready-for-agent` that no open
//! ticket blocks, that are the track's, and that no branch of the
//! repository records; the next is the widest of them by the census, the
//! number breaking a tie. An empty frontier is stated, "none", and
//! is not an error, since a track with nothing to work has not failed.
//!
//! A track is the tickets that carry its `track:<name>` label, and a
//! ticket with no `track:` label is `main`'s alone (ADR-0009), so every
//! ticket is some track's and two runs on different tracks never compete
//! for an unlabelled one; [`tracks_of`] is the one statement of that.
//! A ticket closes when its work is on `main`, so a
//! blocker whose branch waits on its merge is open and blocks, and a
//! ticket is cut only from a `main` that holds what it waited on; what
//! blocks is [`crate::ticket::BlockedBy::open`] alone. "No branch yet" is the
//! repository's own record, `branch.<name>.ticket`, read once per answer
//! as [`Records`].
//! The tracker is reached through [`Gh::open`], the ticket module's one
//! list, and through nothing else here. The same list answers what the
//! track waits on: its open tickets labelled `for-a-person`, which
//! `track work` prints when the frontier stays empty.

use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

use crate::census_weight::Weights;
use crate::reading::Store;
use crate::ticket::{Gh, Open};
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
/// session's skill from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Next {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub labels: Vec<String>,
    /// The refused tests the census says this ticket's constructs would
    /// let through; zero where the census weighs it at nothing and
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
    let found = widest(&open, track, &weights, &Records::read(root));
    Ok(found.cloned().map(|ticket| {
        let mut next = Next::from(ticket);
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
/// answering what the branches of the clone record: labelled
/// `ready-for-agent`, blocked by no open ticket, the track's, and
/// recorded by no branch. A function of the tracker's answer, those
/// records and the census the store holds.
///
/// Widest is the census's count of the refused tests the ticket's
/// constructs would let through, and the tie is the number, the
/// tracker's order of filing. A frontier the census weighs at nothing
/// — because no census has been run, or because none of these tickets
/// names a construct it counted — is every weight zero and so every tie,
/// which is the order this had before the census existed.
fn widest<'a>(
    open: &'a [Open],
    track: &str,
    weights: &Weights,
    records: &Records,
) -> Option<&'a Open> {
    open.iter()
        .filter(|ticket| ticket.label_names().any(|name| name == READY))
        .filter(|ticket| ticket.blocked_by.open().next().is_none())
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
    use super::{Weights, is_tracks, widest};
    use crate::ticket::{BlockedBy, Blocker, Label, Open, State};
    use crate::track::{Records, Ticket};

    /// The records of branches `t<n>` for each of `on`.
    fn records(on: &[u64]) -> Records {
        Records::of(on.iter().map(|&n| (format!("t{n}"), Ticket::new(n))))
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
    /// it.
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
        let none = records(&[]);
        let found = widest(&open, "main", &weights, &none).expect("a frontier");
        assert_eq!(found.number, 3, "the widest, not the oldest");

        // With the widest already on a branch, the next widest follows.
        let found = widest(&open, "main", &weights, &records(&[3])).expect("a frontier");
        assert_eq!(found.number, 4);

        // Weighed at nothing, the order is the filing order it always was.
        let found = widest(&open, "main", &Weights::default(), &none).expect("a frontier");
        assert_eq!(found.number, 1);
    }

    /// Only an open blocker blocks: #9 waits on #1, open,
    /// and not on #2, closed on `t2`, whether or not `main` holds `t2`
    /// yet, since a ticket closes only once its work is on `main`; with
    /// #1 closed too, #9 is the frontier.
    #[test]
    fn only_an_open_blocker_blocks() {
        let mut ticket = ready(9, "ticket 9");
        ticket.blocked_by = BlockedBy {
            nodes: [(1, State::Open), (2, State::Closed)]
                .into_iter()
                .map(|(number, state)| Blocker { number, state })
                .collect(),
        };
        assert_eq!(ticket.blocked_by.open().collect::<Vec<_>>(), [1]);
        let none = Weights::default();
        let open = [ticket];
        assert_eq!(widest(&open, "main", &none, &records(&[2])), None);

        let [mut ticket] = open;
        ticket.blocked_by.nodes[0].state = State::Closed;
        let open = [ticket];
        let found = widest(&open, "main", &none, &records(&[2])).map(|t| t.number);
        assert_eq!(found, Some(9));
    }

    /// No `track:` label is `main`'s alone (ADR-0009); one names
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
