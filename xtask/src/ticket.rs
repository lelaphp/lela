//! `xtask ticket`, the instrument that owns what a ticket is (#28;
//! CONTEXT.md, Ticket): `read <n>` fetches the issue, reads the boxes under
//! its `## Acceptance criteria` heading and writes a reading of instrument
//! `ticket` over the ticket's number, the criteria counted, the ticked, and
//! the unticked named in the message; `close <n>` reads the same way and
//! then closes the issue at the tracker, refusing while any box is unticked
//! and changing nothing there. A ticket is closed by the instrument that
//! read its criteria and by nothing else: the rule the ticket proposes
//! denies `gh issue close` and `gh issue reopen` to an agent, the shape
//! `git.no-verify` has, so the checkbox grammar lives here once and not in
//! a `permit` regex, the Stop hook and CI. With no number, the ticket is
//! the branch's, the record `xtask worktree add --ticket` wrote and
//! [`Track::of`] reads (#30); a branch that records none is refused by
//! name, and no number is guessed from the branch.
//!
//! The reading holds the ticket to its boxes: `pass` when every criterion
//! is ticked, `fail` while one is not, and `fail` for a ticket with no
//! criteria at all, since a ticket with no box cannot be shown done.
//!
//! Ticked boxes are not enough (#29): work that found a thing it did not
//! do has three homes, a ticket of its own, a divergence row, or a rule
//! proposed in a commit message, and a reply is not one of them. So a close
//! names what it opened, as an argument and not a sentence after it:
//! `--opens <n>` for each ticket filed, repeatable, or `--opens-nothing`,
//! and a close that names neither is refused before the tracker is asked
//! anything ([`Successors::named`]). Each successor is fetched and must be
//! open; one that is closed, missing or unreadable refuses the close with
//! nothing changed. Whose the successor is is not held, `for-a-person`
//! as good as `ready-for-agent`: the bar is that it was filed. The edge is
//! the tracker's own blocking relation, `gh issue edit <n> --add-blocking`,
//! set before the close, so it is what `gh issue view` prints as
//! `blocking` and `blocked-by` and not a heading appended to a body. The
//! reading counts the successors as `opens` and names them in the message;
//! `--opens-nothing` is an assertion, written there with the session that
//! made it, which is what a person holds someone to later.
//!
//! A close that went through records what closed it: the commit and
//! tree it was stamped at, the way every reading is, and the session the
//! call named, the way a `permit` reading carries the hook's `session_id`
//! (harness-design.md §4.2); the tracker gets the same in the closing
//! comment. A close the tracker refused is a failed reading carrying `gh`'s
//! text.
//!
//! The tracker is `gh` and nothing else: no trait, no second adapter, until
//! a second tracker exists to make a seam real. Only the fetch and the
//! close touch it, through [`Gh`], one program the command line takes
//! from `PATH` and a test names by path: a shell file that answers `issue
//! view` from a recorded transcript and writes down what `issue close`
//! asked, so nothing under `cargo test` reaches the network. The
//! judgement is [`Criteria::parse`], a function of the body alone.
//!
//! What the seam does not cover, written down rather than pretended: the
//! rule is judged by `permit`, whose tools are `Edit`, `Write` and `Bash`,
//! so a GitHub MCP server attached later would close a ticket unjudged.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::reading::{Reading, Status};
use crate::track::{Ticket, Track};

/// The instrument name every reading of ticket carries.
pub(crate) const INSTRUMENT: &str = "ticket";
/// The path every reading of ticket is stamped over: the workspace, since
/// a ticket has no tree of its own, so `done` holds the reading fresh
/// while the workspace's tree still matches (#31; the rule is stated in
/// `done`'s rustdoc). One statement, read by the writer and the reader.
pub(crate) const STAMPED_OVER: &str = ".";
/// The heading whose boxes are the acceptance criteria, matched without
/// regard to case at level two.
const HEADING: &str = "acceptance criteria";

/// What `xtask ticket` does: read the criteria, or read them and close,
/// naming what the work opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    Read,
    Close(Successors),
}

/// What a close names as opened: the tickets filed for what the work
/// found and did not do, or the assertion that there was nothing to file.
/// One of the two, always (#29).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Successors {
    Tickets(Vec<Ticket>),
    Nothing,
}

impl Successors {
    /// From the flags of `close`: `--opens <n>`, repeatable, or
    /// `--opens-nothing`. Refuses a call that gives neither and one that
    /// gives both, naming the two, and a ticket that opens itself; a
    /// number named twice counts once.
    pub fn named(closing: Ticket, opens: &[u64], nothing: bool) -> Result<Successors> {
        match (opens.is_empty(), nothing) {
            (true, false) => bail!(
                "`xtask ticket close` names what the work opened: `--opens <n>` for each ticket filed, repeatable, or `--opens-nothing`; without one of the two nothing closes"
            ),
            (false, true) => bail!(
                "`--opens {}` and `--opens-nothing` contradict each other; give one of the two",
                opens
                    .iter()
                    .map(u64::to_string)
                    .collect::<Vec<_>>()
                    .join(" --opens ")
            ),
            (true, true) => Ok(Successors::Nothing),
            (false, false) => {
                let mut tickets: Vec<Ticket> = Vec::new();
                for &number in opens {
                    let ticket = Ticket::new(number);
                    if ticket == closing {
                        bail!(
                            "{closing} cannot open itself; name the ticket filed for what was found"
                        );
                    }
                    if !tickets.contains(&ticket) {
                        tickets.push(ticket);
                    }
                }
                Ok(Successors::Tickets(tickets))
            }
        }
    }

    /// The tickets named, none for the assertion.
    fn tickets(&self) -> &[Ticket] {
        match self {
            Successors::Tickets(tickets) => tickets,
            Successors::Nothing => &[],
        }
    }

    /// Each successor exists at the tracker and is open; the first that is
    /// not is the refusal.
    fn held_open(&self, gh: &Gh, root: &Path) -> Result<()> {
        if let Successors::Tickets(tickets) = self
            && tickets.is_empty()
        {
            bail!("opens no ticket and asserts nothing; `--opens <n>` or `--opens-nothing`");
        }
        for &successor in self.tickets() {
            let issue = gh
                .view(root, successor)
                .with_context(|| format!("opens {successor}"))?;
            if issue.state == State::Closed {
                bail!("opens {successor}, which is closed; a successor is an open ticket");
            }
        }
        Ok(())
    }

    /// What the message and the closing comment say: the successors by
    /// number, or the assertion with the session that made it.
    fn describe(&self, session_id: Option<&str>) -> String {
        match self {
            Successors::Tickets(tickets) => format!(
                "opens {}",
                tickets
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Successors::Nothing => match session_id {
                Some(id) => format!("opens nothing, asserted in session {id}"),
                None => "opens nothing, asserted with no session named".to_owned(),
            },
        }
    }
}

/// The acceptance criteria of one ticket body: every box under the
/// `## Acceptance criteria` heading, up to the next heading of level one
/// or two, a box being a list item at any indentation that opens with
/// `[ ]`, `[x]` or `[X]`. A box elsewhere in the body, and a box inside a
/// fenced block, is not a criterion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Criteria {
    /// Whether the body has the heading at all.
    pub found: bool,
    pub ticked: Vec<String>,
    pub unticked: Vec<String>,
}

impl Criteria {
    /// The grammar, over the body alone.
    pub fn parse(body: &str) -> Criteria {
        let mut criteria = Criteria::default();
        let mut inside = false;
        let mut fenced = false;
        for line in body.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                fenced = !fenced;
                continue;
            }
            if fenced {
                continue;
            }
            if let Some(title) = heading(line) {
                inside = title.eq_ignore_ascii_case(HEADING);
                criteria.found |= inside;
                continue;
            }
            if !inside {
                continue;
            }
            if let Some((ticked, text)) = checkbox(trimmed) {
                if ticked {
                    criteria.ticked.push(text.to_owned());
                } else {
                    criteria.unticked.push(text.to_owned());
                }
            }
        }
        criteria
    }

    /// The boxes, ticked and unticked together.
    pub fn count(&self) -> usize {
        self.ticked.len() + self.unticked.len()
    }

    /// Every criterion is ticked and there is at least one.
    pub fn met(&self) -> bool {
        self.count() > 0 && self.unticked.is_empty()
    }
}

/// The title of a level one or two heading at the margin, indented by
/// three spaces at most as `CommonMark` has it, `None` for any other line:
/// a `## ` deeper in is a list item's text, not a section.
fn heading(line: &str) -> Option<&str> {
    let indent = line.len() - line.trim_start().len();
    if indent > 3 {
        return None;
    }
    let title = line
        .trim()
        .strip_prefix("## ")
        .or_else(|| line.strip_prefix("# "))?;
    Some(title.trim().trim_end_matches('#').trim())
}

/// A list item that opens with a box: whether it is ticked, and its text.
fn checkbox(line: &str) -> Option<(bool, &str)> {
    let item = ["- ", "* ", "+ "]
        .iter()
        .find_map(|marker| line.strip_prefix(marker))?
        .trim_start();
    let (ticked, rest) = if let Some(rest) = item.strip_prefix("[ ]") {
        (false, rest)
    } else if let Some(rest) = item
        .strip_prefix("[x]")
        .or_else(|| item.strip_prefix("[X]"))
    {
        (true, rest)
    } else {
        return None;
    };
    // GitHub's grammar: the box is followed by whitespace or by nothing.
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some((ticked, rest.trim()))
}

/// One issue as the tracker answers it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub state: State,
    pub url: String,
    /// The tickets this one blocks, nested as `blockedBy` is; an edge the
    /// tracker has is not set again, since it refuses a second one.
    #[serde(default)]
    pub blocking: BlockedBy,
}

/// One open issue as `gh issue list` answers it: what the frontier is
/// judged from, the labels and the blocking relation carried as the
/// tracker spells them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Open {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub labels: Vec<Label>,
    #[serde(rename = "blockedBy")]
    pub blocked_by: BlockedBy,
}

impl Open {
    /// The ticket's labels by name.
    pub fn label_names(&self) -> impl Iterator<Item = &str> {
        self.labels.iter().map(|label| label.name.as_str())
    }

    /// Whether an open ticket blocks this one; a closed blocker does not.
    pub fn blocked(&self) -> bool {
        self.blocked_by
            .nodes
            .iter()
            .any(|blocker| blocker.state == State::Open)
    }
}

/// One label, by name.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Label {
    pub name: String,
}

/// The tickets blocking one, as the tracker nests them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct BlockedBy {
    pub nodes: Vec<Blocker>,
}

/// One ticket in a blocking relation: its number and whether it is open.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Blocker {
    pub number: u64,
    pub state: State,
}

/// An issue's state as `gh` spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum State {
    Open,
    Closed,
}

impl State {
    /// The word in lowercase, for a message.
    fn as_str(self) -> &'static str {
        match self {
            State::Open => "open",
            State::Closed => "closed",
        }
    }
}

/// The tracker: one `gh` program, run in the workspace so it resolves the
/// repository from the checkout's remote. The only two calls the
/// instrument makes are here. The command line runs [`Gh::on_path`]; a
/// test runs [`Gh::at`] over a shell file that answers `issue view` from
/// a recorded transcript and writes what `issue close` asked, so the
/// judgement is held without the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gh {
    program: PathBuf,
}

impl Gh {
    /// The `gh` on `PATH`.
    pub fn on_path() -> Gh {
        Gh::at(Path::new("gh"))
    }

    /// A `gh` at `program`, a test's recorded one.
    pub fn at(program: &Path) -> Gh {
        Gh {
            program: program.to_path_buf(),
        }
    }

    /// `gh issue view <n> --json …`, one issue.
    pub fn view(&self, root: &Path, ticket: Ticket) -> Result<Issue> {
        let number = ticket.number().to_string();
        let text = gh(
            &self.program,
            root,
            &[
                "issue",
                "view",
                &number,
                "--json",
                "number,title,body,state,url,blocking",
            ],
        )
        .with_context(|| format!("{ticket} could not be read from the tracker"))?;
        serde_json::from_str(&text)
            .with_context(|| format!("{ticket}: gh's answer is not an issue"))
    }

    /// `gh issue list --state open --json …`: every open issue with its
    /// labels and what blocks it, the one call `track next` makes (#73).
    /// Every open issue, not the labelled ones alone, so the frontier's
    /// filters are judged here, where a test can hold each of them.
    pub fn open(&self, root: &Path) -> Result<Vec<Open>> {
        let text = gh(
            &self.program,
            root,
            &[
                "issue",
                "list",
                "--state",
                "open",
                "--limit",
                "1000",
                "--json",
                "number,title,url,labels,blockedBy",
            ],
        )
        .context("the open tickets could not be listed from the tracker")?;
        serde_json::from_str(&text).context("gh's answer is not a list of issues")
    }

    /// `gh issue edit <n> --add-blocking <a,b>`: the edge from the ticket
    /// closing to each it opened, the tracker's own blocking relation.
    /// Nothing is asked for no successors.
    pub fn blocking(&self, root: &Path, ticket: Ticket, successors: &[Ticket]) -> Result<()> {
        if successors.is_empty() {
            return Ok(());
        }
        let number = ticket.number().to_string();
        let opened = successors
            .iter()
            .map(|t| t.number().to_string())
            .collect::<Vec<_>>()
            .join(",");
        gh(
            &self.program,
            root,
            &["issue", "edit", &number, "--add-blocking", &opened],
        )
        .with_context(|| format!("{ticket}'s edge to what it opened was not set"))?;
        Ok(())
    }

    /// `gh issue close <n> --comment <text>`: the one act.
    pub fn close(&self, root: &Path, ticket: Ticket, comment: &str) -> Result<()> {
        let number = ticket.number().to_string();
        gh(
            &self.program,
            root,
            &["issue", "close", &number, "--comment", comment],
        )
        .with_context(|| format!("{ticket} was not closed"))?;
        Ok(())
    }
}

/// Run `program` in `root` and return its stdout; a non-zero exit is an
/// error carrying its stderr.
fn gh(program: &Path, root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("running gh")?;
    if !out.status.success() {
        bail!(
            "gh {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    String::from_utf8(out.stdout).context("gh wrote something that is not UTF-8")
}

/// The scope a ticket's reading is written under: its number, the key
/// `done` reads it back by.
pub(crate) fn scope(ticket: Ticket) -> String {
    ticket.number().to_string()
}

/// The ticket named, or the branch's; a branch that records none is a
/// refusal that names the branch.
pub fn ticket_of(root: &Path, number: Option<u64>) -> Result<Ticket> {
    if let Some(number) = number {
        return Ok(Ticket::new(number));
    }
    let track = Track::of(root, None)?;
    match (track.ticket, track.branch) {
        (Some(ticket), _) => Ok(ticket),
        (None, Some(branch)) => bail!(
            "{branch} records no ticket; name one (`xtask ticket {{read,close}} <n>`), or lay the branch down with `xtask worktree add --ticket`"
        ),
        (None, None) => bail!("HEAD is detached and records no ticket; name one"),
    }
}

/// `xtask ticket read` and `close`: the issue fetched through `gh`, the
/// criteria read, the reading of instrument `ticket` over the ticket's
/// number, stamped over the workspace as `done`'s is, and for a close with
/// every box ticked and every successor open the edge and then the act at
/// the tracker before the reading is written. The caller records the
/// reading; its status is the exit.
pub fn run(
    root: &Path,
    gh: &Gh,
    act: Act,
    ticket: Ticket,
    session_id: Option<&str>,
) -> Result<Reading> {
    let issue = gh.view(root, ticket)?;
    let criteria = Criteria::parse(&issue.body);
    let mut reading =
        Reading::stamped_over(INSTRUMENT, &scope(ticket), root, Path::new(STAMPED_OVER))?;
    reading.count("criteria", criteria.count() as u64)?;
    reading.count("ticked", criteria.ticked.len() as u64)?;
    reading.count("unticked", criteria.unticked.len() as u64)?;
    if let Act::Close(successors) = &act {
        reading.count("opens", successors.tickets().len() as u64)?;
    }
    session_id
        .unwrap_or_default()
        .clone_into(&mut reading.session);
    // Every unticked box by name, numbered so a comma in one is not a gap
    // between two.
    let names = criteria
        .unticked
        .iter()
        .enumerate()
        .map(|(i, text)| format!("({}) {text}", i + 1))
        .collect::<Vec<_>>()
        .join("; ");
    let boxes = format!(
        "{} of {} criteria ticked",
        criteria.ticked.len(),
        criteria.count()
    );
    let (status, message) = match act {
        _ if !criteria.found => (
            Status::Fail,
            format!("{ticket} has no `## Acceptance criteria` heading; nothing to hold it to"),
        ),
        _ if criteria.count() == 0 => (
            Status::Fail,
            format!("{ticket} has no box under `## Acceptance criteria`; nothing to hold it to"),
        ),
        _ if !criteria.met() => (
            Status::Fail,
            format!(
                "{ticket}: {boxes}; unticked: {names}{}",
                match act {
                    Act::Read => "",
                    Act::Close(_) => "; not closed, nothing changed at the tracker",
                }
            ),
        ),
        Act::Read => (
            Status::Pass,
            format!("{ticket}: {boxes}; {}", issue.state.as_str()),
        ),
        Act::Close(_) if issue.state == State::Closed => (
            Status::Fail,
            format!("{ticket}: {boxes}; closed already, nothing changed at the tracker"),
        ),
        Act::Close(successors) => {
            let opened = successors.describe(session_id);
            let comment = format!(
                "Closed by `xtask ticket close` at {}: {boxes}; {opened}.",
                reading.commit
            );
            // The edges the tracker has already stand; only the rest are set,
            // so a close retried after one that failed past this point lands.
            let missing = successors
                .tickets()
                .iter()
                .copied()
                .filter(|t| !issue.blocking.nodes.iter().any(|n| n.number == t.number()))
                .collect::<Vec<_>>();
            let closed = successors
                .held_open(gh, root)
                .and_then(|()| gh.blocking(root, ticket, &missing))
                .map_err(|err| format!("{err:#}; not closed, nothing changed at the tracker"))
                .and_then(|()| {
                    // The edge is set by now; a close the tracker refuses
                    // leaves it standing, and the message says so.
                    gh.close(root, ticket, &comment).map_err(|err| {
                        let edge = match successors {
                            Successors::Tickets(_) => "; the edge to what it opened stands",
                            Successors::Nothing => "",
                        };
                        format!("{opened}{edge}; {err:#}")
                    })
                });
            match closed {
                Ok(()) => (
                    Status::Pass,
                    format!(
                        "{ticket}: {boxes}; {opened}; closed by xtask ticket close at {}{}",
                        reading.commit,
                        session_id
                            .map(|id| format!(" in session {id}"))
                            .unwrap_or_default()
                    ),
                ),
                Err(text) => (Status::Fail, format!("{ticket}: {boxes}; {text}")),
            }
        }
    };
    reading.status = status;
    reading.message = message;
    Ok(reading)
}

#[cfg(test)]
mod tests {
    use super::{Criteria, Successors};
    use crate::track::Ticket;

    /// The grammar of the flags (#29): one of the two, not neither, not
    /// both, not the ticket itself, each successor once.
    #[test]
    fn a_close_names_its_successors_or_asserts_none_and_never_neither() {
        let closing = Ticket::new(29);
        let neither = Successors::named(closing, &[], false)
            .unwrap_err()
            .to_string();
        assert!(
            neither.contains("--opens <n>") && neither.contains("--opens-nothing"),
            "{neither}"
        );
        let both = Successors::named(closing, &[31], true)
            .unwrap_err()
            .to_string();
        assert!(
            both.contains("--opens 31") && both.contains("--opens-nothing"),
            "{both}"
        );
        let itself = Successors::named(closing, &[31, 29], false)
            .unwrap_err()
            .to_string();
        assert!(itself.contains("#29 cannot open itself"), "{itself}");
        assert_eq!(
            Successors::named(closing, &[], true).unwrap(),
            Successors::Nothing
        );
        assert_eq!(
            Successors::named(closing, &[31, 32, 31], false).unwrap(),
            Successors::Tickets(vec![Ticket::new(31), Ticket::new(32)])
        );
        assert_eq!(
            Successors::Tickets(vec![Ticket::new(31), Ticket::new(32)]).describe(Some("s")),
            "opens #31, #32"
        );
        assert_eq!(
            Successors::Nothing.describe(Some("s1")),
            "opens nothing, asserted in session s1"
        );
        assert_eq!(
            Successors::Nothing.describe(None),
            "opens nothing, asserted with no session named"
        );
    }

    /// The grammar over a body with ticked, unticked, indented and
    /// non-criteria boxes (#28).
    #[test]
    fn the_boxes_under_the_heading_are_the_criteria_and_no_others() {
        let body = "## What to build\n\n- [ ] a box in the wrong section\n\n## Acceptance criteria\n\n- [x] ticked\n- [ ] unticked\n  - [X] indented and ticked\n    * [ ] indented twice, unticked\n- not a box\n```\n- [ ] a box in a fence\n```\n- [ ]no space, not a box\n\n### A sub-heading stays inside\n\n+ [ ] under the sub-heading\n\n## Blocked by\n\n- [ ] a box after the section\n";
        let criteria = Criteria::parse(body);
        assert!(criteria.found);
        assert_eq!(criteria.ticked, ["ticked", "indented and ticked"]);
        assert_eq!(
            criteria.unticked,
            [
                "unticked",
                "indented twice, unticked",
                "under the sub-heading"
            ]
        );
        assert_eq!(criteria.count(), 5);
        assert!(!criteria.met());
    }

    #[test]
    fn the_heading_is_matched_without_regard_to_case_and_a_body_without_it_says_so() {
        let met = Criteria::parse("## ACCEPTANCE CRITERIA\n- [x] one\n- [X] two\n");
        assert!(met.found && met.met() && met.count() == 2);
        let none = Criteria::parse("## Acceptance\n- [ ] one\n");
        assert!(!none.found && none.count() == 0 && !none.met());
        let empty = Criteria::parse("## Acceptance criteria\n\nnone yet\n");
        assert!(empty.found && empty.count() == 0 && !empty.met());
    }
}
