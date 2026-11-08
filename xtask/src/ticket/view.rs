//! `xtask ticket view <n> [--since <time>]`: a ticket as a builder reads
//! it, the body and the comments it may act on, and a count of what was
//! left out (#4263; Round 3, Q16 (c)). It finishes Round 2, Q13's comment
//! rule, since a reader that drops outsiders' comments is that filter:
//! `gh issue view --comments` prints everyone's.
//!
//! Which comments are printed, the rule stated once, in [`Kept::of`]:
//!
//! * A comment whose author is the repository's `OWNER`, a `MEMBER` or a
//!   `COLLABORATOR`, as the tracker's `authorAssociation` says, is a
//!   maintainer's, a person's or an agent's: agents comment under a
//!   person's account (113 of 299 builders did on their own ticket), so
//!   the tracker cannot tell the two apart and neither does this.
//! * A comment by `github-actions` is the harness's own CI writing
//!   (`scripts/ci/red.sh`), printed, except its "Still red at" lines,
//!   one per red run of a ticket already filed, which are counted.
//! * Anyone else's is an outsider's, withheld and counted by its
//!   association, and so is a comment a maintainer hid.
//! * With `--since`, a comment written before that instant is counted
//!   and not printed.
//!
//! It prints rather than refuses, and writes no reading: it measures
//! nothing. `ticket ready` prints the same way the comments newer than the
//! branch's first commit, what was said on the ticket after its work began
//! ([`newer_than_the_work`]).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{Gh, Issue};
use crate::reading::epoch_seconds;
use crate::track::{Ticket, Track};

/// The associations whose comments are a maintainer's.
const MAINTAINERS: [&str; 3] = ["OWNER", "MEMBER", "COLLABORATOR"];
/// The login the harness's CI comments under.
const CI: &str = "github-actions";
/// How CI's comment on a ticket whose red run repeats opens.
const STILL_RED: &str = "Still red at ";

/// One issue with its comments, as `gh issue view --json …,comments`
/// answers it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Thread {
    #[serde(flatten)]
    pub issue: Issue,
    #[serde(default)]
    pub comments: Vec<Comment>,
}

/// One comment as the tracker spells it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Comment {
    pub author: Author,
    #[serde(rename = "authorAssociation")]
    pub association: String,
    pub body: String,
    /// When it was written, RFC 3339 in UTC, as `2026-11-03T13:15:14Z`.
    #[serde(rename = "createdAt")]
    pub created_at: String,
    /// Whether a maintainer hid it.
    #[serde(default, rename = "isMinimized")]
    pub hidden: bool,
}

/// A comment's author, by login.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Author {
    pub login: String,
}

/// What becomes of one comment.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Kept {
    Printed,
    Earlier,
    Hidden,
    StillRed,
    Outsider(String),
}

impl Kept {
    /// The rule, in the module's order: an instant before `since` first,
    /// then a hidden comment, CI's repeated red, and an outsider's.
    fn of(comment: &Comment, since: Option<i64>) -> Result<Kept> {
        if let Some(since) = since
            && instant(&comment.created_at)? < since
        {
            return Ok(Kept::Earlier);
        }
        let ci = comment.author.login == CI;
        Ok(if comment.hidden {
            Kept::Hidden
        } else if ci && comment.body.starts_with(STILL_RED) {
            Kept::StillRed
        } else if ci || MAINTAINERS.contains(&comment.association.as_str()) {
            Kept::Printed
        } else {
            Kept::Outsider(comment.association.clone())
        })
    }
}

/// Seconds since 1970 of `text`, an RFC 3339 instant or a date, which
/// `--since` takes and the tracker writes.
pub fn instant(text: &str) -> Result<i64> {
    let at = text
        .trim()
        .parse()
        .with_context(|| format!("{text:?} is no time: give one as the tracker writes it, `2026-11-03T13:15:14Z`, or a date"))?;
    epoch_seconds(&at)
}

/// The comments of `thread` this module prints, each under a line naming
/// its author, association and time, and the line saying what it withheld:
/// what `view` and `ready` print below the ticket.
fn comments(thread: &Thread, since: Option<i64>, after: &str) -> Result<String> {
    let mut shown: Vec<String> = Vec::new();
    let mut withheld: BTreeMap<String, usize> = BTreeMap::new();
    let mut outsiders: BTreeMap<String, usize> = BTreeMap::new();
    let mut printed = 0;
    for comment in &thread.comments {
        let why = match Kept::of(comment, since)? {
            Kept::Printed => {
                printed += 1;
                shown.push(format!(
                    "\n--- {} ({}) at {}\n{}\n",
                    comment.author.login,
                    comment.association,
                    comment.created_at,
                    comment.body.trim_end()
                ));
                continue;
            }
            Kept::Earlier => format!("written before {after}"),
            Kept::Hidden => "hidden by a maintainer".to_owned(),
            Kept::StillRed => "CI's \"Still red\" lines".to_owned(),
            Kept::Outsider(association) => {
                *outsiders.entry(association).or_default() += 1;
                continue;
            }
        };
        *withheld.entry(why).or_default() += 1;
    }
    let mut said: Vec<String> = withheld
        .iter()
        .map(|(why, n)| format!("{n} {why}"))
        .collect();
    if !outsiders.is_empty() {
        let by: Vec<String> = outsiders.iter().map(|(a, n)| format!("{a} {n}")).collect();
        let n: usize = outsiders.values().sum();
        said.push(format!("{n} by outsiders ({})", by.join(", ")));
    }
    let total = thread.comments.len();
    let held = total - printed;
    shown.push(if held == 0 {
        format!("\n{printed} of {total} comment(s) printed; withheld nothing\n")
    } else {
        format!(
            "\n{printed} of {total} comment(s) printed; withheld {held}: {}\n",
            said.join(", ")
        )
    });
    Ok(shown.concat())
}

/// `xtask ticket view`: the ticket's title, state and body, then the
/// comments [`Kept::of`] prints, and what it withheld, counted. With
/// `since`, the comments before it are among the withheld.
pub fn view(root: &Path, gh: &Gh, ticket: Ticket, since: Option<&str>) -> Result<String> {
    let thread = gh.thread(root, ticket)?;
    let at = since.map(instant).transpose()?;
    let issue = &thread.issue;
    Ok(format!(
        "{ticket} {} ({}) {}\n\n{}\n{}",
        issue.title,
        issue.state.as_str(),
        issue.url,
        issue.body.trim_end(),
        comments(&thread, at, since.unwrap_or_default())?
    ))
}

/// What `ticket ready` prints of `ticket`: the comments newer than the
/// first commit of `HEAD`'s branch since its base, said after the work
/// began and so perhaps unread, by the same rule as `view`; the base
/// itself where the branch has no commit yet.
pub fn newer_than_the_work(root: &Path, gh: &Gh, ticket: Ticket) -> Result<String> {
    let base = Track::of(root, None)?.base;
    let range = format!("{base}..HEAD");
    let first = crate::git::git(root, &["rev-list", "--reverse", &range])?;
    let first = first.lines().next().unwrap_or(&base).to_owned();
    let time = crate::git::git(root, &["log", "-1", "--format=%ct", &first])?;
    let since: i64 = time
        .trim()
        .parse()
        .with_context(|| format!("{first}'s commit time {time:?} is no number"))?;
    let thread = gh.thread(root, ticket)?;
    let after = format!("{}'s first commit, {}", ticket, crate::track::short(&first));
    Ok(format!(
        "{ticket}: the comments since {after}:\n{}",
        comments(&thread, Some(since), &after)?
    ))
}

#[cfg(test)]
mod tests {
    use super::{Author, Comment, Kept};

    fn comment(login: &str, association: &str, body: &str) -> Comment {
        Comment {
            author: Author {
                login: login.to_owned(),
            },
            association: association.to_owned(),
            body: body.to_owned(),
            created_at: "2026-11-03T13:15:14Z".to_owned(),
            hidden: false,
        }
    }

    /// The rule: a maintainer's printed, CI's printed but its repeated red
    /// counted, an outsider's withheld by association, a hidden one
    /// withheld, and one before `--since` withheld before anything else.
    #[test]
    fn a_maintainers_comment_is_printed_and_an_outsiders_is_withheld() {
        let of = |c: &Comment, since| Kept::of(c, since).unwrap();
        for association in ["OWNER", "MEMBER", "COLLABORATOR"] {
            assert_eq!(of(&comment("m", association, "x"), None), Kept::Printed);
        }
        assert_eq!(
            of(&comment("github-actions", "NONE", "Green again: x"), None),
            Kept::Printed
        );
        assert_eq!(
            of(
                &comment("github-actions", "NONE", "Still red at abc: x"),
                None
            ),
            Kept::StillRed
        );
        assert_eq!(
            of(&comment("someone", "NONE", "Still red at abc: x"), None),
            Kept::Outsider("NONE".to_owned())
        );
        assert_eq!(
            of(&comment("someone", "CONTRIBUTOR", "x"), None),
            Kept::Outsider("CONTRIBUTOR".to_owned())
        );
        let hidden = Comment {
            hidden: true,
            ..comment("m", "OWNER", "x")
        };
        assert_eq!(of(&hidden, None), Kept::Hidden);
        let since = super::instant("2026-11-04").unwrap();
        assert_eq!(of(&comment("m", "OWNER", "x"), Some(since)), Kept::Earlier);
        assert!(super::instant("yesterday").is_err());
    }
}
