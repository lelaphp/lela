//! The ticket a track works next (#73; harness-design.md §4.5 item 7;
//! CONTEXT.md, Track, Ticket): `xtask track next` answers it from the
//! tracker's open tickets and the clone's branch records, as text or as
//! one JSON object, and writes no reading. The frontier is the open
//! tickets labelled `ready-for-agent` that no open ticket blocks, that
//! carry no `track:<name>` label naming another track, and that no branch
//! of this clone records; the next is the oldest of them, by number,
//! which is the tracker's order of filing. An empty frontier is stated,
//! "none", and is not an error, since a track with nothing to work has
//! not failed.
//!
//! A `track:<name>` label says only that track works the ticket: harness
//! tickets carry `track:main`, and a ticket with no `track:` label is
//! every track's. Both tracks work the same tickets independently, so
//! "no branch yet" is the clone's own record, `branch.<name>.ticket`, the
//! one [`track::branch_of_ticket`] reads, and never a look across tracks.
//! The tracker is reached through [`Gh::open`], the ticket module's one
//! list, and through nothing else here. The same list answers what the
//! track waits on (#85): its open tickets labelled `for-a-person`, which
//! `track work` prints when the frontier stays empty.

use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

use crate::ticket::{Gh, Open};
use crate::track::{self, Ticket, Track};

/// The label a ticket an agent may take carries. The frontier reads it
/// here and `ticket` holds such a ticket's criteria to what an agent may
/// write; one statement, two readers.
pub(crate) const READY: &str = "ready-for-agent";
/// The label a ticket that is a person's alone carries (harness §11).
const PERSON: &str = "for-a-person";
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
}

impl Next {
    /// One line: the ticket as it is cited and its title, or "none" with
    /// the track whose frontier is empty.
    pub fn render(found: Option<&Next>, track: &str) -> String {
        match found {
            Some(next) => format!("#{} {}\n", next.number, next.title),
            None => format!("none: the frontier of {track} is empty\n"),
        }
    }

    /// The same as one object: `ticket`, `title` and `url` are null for
    /// an empty frontier.
    pub fn to_json(found: Option<&Next>, track: &str) -> Value {
        json!({
            "track": track,
            "ticket": found.map(|next| next.number),
            "title": found.map(|next| next.title.as_str()),
            "url": found.map(|next| next.url.as_str()),
        })
    }
}

/// The ticket `track` works next in the clone at `root`, `None` for an
/// empty frontier.
pub fn next(root: &Path, gh: &Gh, track: &str) -> Result<Option<Next>> {
    let open = gh.open(root)?;
    Ok(frontier(&open, track, |ticket| {
        track::branch_of_ticket(root, ticket).is_some()
    })
    .cloned()
    .map(Next::from))
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
        }
    }
}

/// The oldest of the open tickets that pass the four filters, `recorded`
/// answering whether a branch of the clone records one; a function of
/// the tracker's answer and that alone.
fn frontier<'a>(
    open: &'a [Open],
    track: &str,
    recorded: impl Fn(Ticket) -> bool,
) -> Option<&'a Open> {
    open.iter()
        .filter(|ticket| ticket.label_names().any(|name| name == READY))
        .filter(|ticket| !ticket.blocked())
        .filter(|ticket| is_tracks(ticket, track))
        .filter(|ticket| !recorded(Ticket::new(ticket.number)))
        .min_by_key(|ticket| ticket.number)
}

/// Whether `track` works the ticket: it carries no `track:` label, or one
/// of the labels it carries names this track.
fn is_tracks(ticket: &Open, track: &str) -> bool {
    let mut named = ticket
        .label_names()
        .filter_map(|name| name.strip_prefix(TRACK_LABEL))
        .peekable();
    named.peek().is_none() || named.any(|name| name == track)
}

/// `xtask track next`: the track named, or the branch's, and the answer
/// printed; no reading.
pub fn command(root: &Path, track: Option<&str>, json: bool) -> Result<()> {
    let name = match track {
        Some(name) => name.to_owned(),
        None => Track::of(root, None)?.name,
    };
    let found = next(root, &Gh::on_path(), &name)?;
    if json {
        println!("{}", Next::to_json(found.as_ref(), &name));
    } else {
        print!("{}", Next::render(found.as_ref(), &name));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_tracks;
    use crate::ticket::{BlockedBy, Label, Open};

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

    /// No `track:` label is every track's; one names its track alone; two
    /// name two.
    #[test]
    fn a_track_label_names_the_one_track_working_the_ticket() {
        assert!(is_tracks(&labelled(&["ready-for-agent"]), "main"));
        assert!(is_tracks(&labelled(&["ready-for-agent"]), "t-x"));
        assert!(is_tracks(&labelled(&["track:main"]), "main"));
        assert!(!is_tracks(&labelled(&["track:main"]), "t-x"));
        assert!(is_tracks(&labelled(&["track:main", "track:t-x"]), "t-x"));
        assert!(!is_tracks(&labelled(&["track:main", "track:t-x"]), "t-y"));
    }
}
