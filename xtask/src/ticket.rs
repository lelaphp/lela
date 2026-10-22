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
//! A ticket an agent may take is held to its label beside its boxes
//! (#153): `ready-for-agent` says an agent works it, and a criterion
//! asking for a write into a path the manifest denies an agent
//! contradicts that label before the work starts, the way #111's first
//! criterion, "`oracle.toml` carries a `[regime.dump]`", did. The paths
//! are the manifest's own, every rule carrying `deny = true` over `Edit`
//! or `Write` read through [`Rule::covers`], so a rule added to
//! `seams.toml` is held here without an edit; the reading fails naming
//! the path and the rule, and the ticket is a person's to relabel
//! (CONTEXT.md, Ticket; harness-design.md §11). A ticket carrying
//! `for-a-person` is not read this way at all: its label already says
//! whose the act is.
//!
//! What `permit` denies is the `Edit` and the `Write`, and for three of
//! those paths that is no longer the whole of it: `xtask steer`
//! ([`crate::steer::STEERABLE`]) is the sanctioned writer of `seams.toml`,
//! `oracle.toml` and the divergence registry, and an agent may run it for
//! an addition a person has settled. #152 is the measurement: the
//! maintainer settled the `[regime.dump]` block on the ticket, labelled it
//! `ready-for-agent`, and an agent landed it, so a criterion whose object
//! is `oracle.toml` stood beside a label that was right. A criterion
//! naming a steerable path is therefore not a mislabel and does not fail
//! the reading; the message names it and the instrument that writes it,
//! since a heuristic that says what it matched is worth more than one
//! that stays silent. What still fails is a criterion naming a denied
//! path no instrument writes, `.claude/**` and the two harness-plugin
//! manifests, where an agent has no way through at all. Whether a
//! steerable criterion asks for an addition or for a change to what
//! already binds is `steer`'s own judgement at the write, and is not read
//! from prose here.
//!
//! Which criterion asks for the write is a heuristic, and one that says
//! what it matched rather than one that stays silent, the shape
//! `ticket.reply-unfiled` has: a false positive costs one hold and the
//! recovery is a person's word on the label. A criterion is read to its
//! main clause, what stands before its first comma, colon or
//! subordinating word, since
//! a path named after one is a path the clause describes something else
//! by: "a ticket whose criterion names `oracle.toml`" asks for no write
//! into the pin. Inside that clause the path is the thing asked for when
//! the clause is about it — it opens the clause, as #111's "`oracle.toml`
//! carries a `[regime.dump]`" does — or when a verb of writing stands in
//! the clause, "a `[regime.dump]` added to `oracle.toml`". A clause that
//! names the file as where something is read stays clean, "the ceiling in
//! `seams.toml`", which is what the open tickets this was measured over
//! mostly say. A clause that lands its text somewhere other than the path
//! asks for no write either (#1672): a verb of drafting, proposing or
//! quoting with a comment, the ticket or the commit message as its place,
//! #1516's "the `.claude/settings.json` `PostToolUse` entry is drafted in a
//! comment on the person's ticket", asks for a comment an agent posts. A
//! verb of writing in the same clause still asks for the write. Nor does
//! a clause whose subject stays as it is (#2391): #630's
//! "`.claude/harness.local.md` still renders this machine's live numbers"
//! asks that nothing regress, where "`oracle.toml` carries" asks for new
//! content.
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
//! as good as `ready-for-agent`: the bar is that it was filed. Which
//! track's it is, is held (#141): a ticket carrying no `track:` label is
//! `main`'s (ADR-0009, CONTEXT.md, Track), so a close of a ticket whose
//! own `track:` label names another track refuses a successor without
//! that label, names the `gh issue edit <n> --add-label track:<name>`
//! that adds it, and asks the tracker for nothing. The skills carry the
//! closing ticket's label when it has one, and the close is the session's
//! last act and the only instrument that already reads the tracker, so it
//! is where a successor that would fall to `main` is caught. A close of
//! one of `main`'s tickets asks nothing of a successor's labels, since
//! work on `main` may file for any track. The edge is
//! the tracker's own blocking relation, `gh issue edit <n> --add-blocking`,
//! set before the close, so it is what `gh issue view` prints as
//! `blocking` and `blocked-by` and not a heading appended to a body. The
//! reading counts the successors as `opens` and names them in the message;
//! `--opens-nothing` is an assertion, written there with the session that
//! made it, which is what a person holds someone to later.
//!
//! A ticket can also describe nothing that is in the tree (#185): #167
//! was filed from a worktree behind the track, measured a defect that
//! #135 had already answered on the tip, and had boxes no work could
//! tick, one of them a person's decision on a question that was no
//! longer open. Such a ticket is withdrawn rather than done, and the
//! verdict is the same instrument's: `close <n> --withdrawn <reason>`,
//! where the reason is the measurement that says there is nothing here,
//! what was run and where, and a bare or empty flag is refused before the
//! tracker is asked anything ([`Verdict::given`]). A withdrawal is held
//! to the boxes the other way round: it is refused while any box is
//! ticked, naming the ticked, since work half done is finished or has its
//! box unticked by a person and is not withdrawn; a ticket with no box at
//! all, which cannot be shown done, is withdrawn like any other, and a
//! ticket carrying `for-a-person` is too, since the label says whose the
//! work is and there is none. The reading counts `withdrawn`, 1 or 0 on
//! every close, so `status --section ticket` tells the two closes apart
//! from the store alone, and carries the reason in its message; the
//! closing comment opens with `Withdrawn by` and carries it too, which is
//! where a person reads it. A withdrawal names what it opened as a close
//! of work does, `--opens` or `--opens-nothing`, since the measurement
//! may have found the real defect elsewhere, as #167's found #169.
//!
//! Whose act a withdrawal is, the question #185 asked inside itself: an
//! agent's, guarded by the measurement. The guard is that the reason is
//! required and recorded with the session, that one ticked box refuses
//! it, and that the act leaves a comment a person reads; the recovery
//! from a false withdrawal is `gh issue reopen`, which an agent is denied
//! by `ticket.close` and a person is not. A ticket filed against a stale
//! branch is handed to a session by `track next` like any other, and that
//! session's own branch is then the one the withdrawal comes from, so a
//! verdict reserved for a person would stall the track on every such
//! ticket. #167 is the instance: the maintainer said on it that it should
//! close as answered by #135 and filed #185 `ready-for-agent` with the
//! close as a criterion.
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
//! A chart sheet holds at most 100 sub-issues, GitHub's cap on the parent
//! of a sub-issue relation, and a session learnt that from the refusal
//! `gh issue create --parent <n>` gave after its ticket was filed: four
//! sessions of one run left a ticket no chart indexes (#448). `parent <n>`
//! is the same fact before the call, measured where the criteria are: the
//! sheet's sub-issue count as the tracker answers it, `subIssuesSummary`,
//! against [`CAP`], as a reading of instrument `ticket` under the scope
//! [`parent_scope`] and never the sheet's own number, which `read` writes
//! the sheet's criteria under. It passes while there is room, saying how
//! much, and fails at the cap and for a sheet that is closed, naming what
//! the tracker would refuse and what the frontier says instead: a chart
//! that fills is continued by a sheet at the same row, never widened, and
//! the filled one is then closed with a comment naming the sheet that
//! continues it, which is a session's own act since `ticket.close` does
//! not judge the close of an issue labelled `frontier:chart` (CONTEXT.md,
//! Chart; harness-design.md ledger 53).
//!
//! What the seam does not cover, written down rather than pretended: the
//! rule is judged by `permit`, whose tools are `Edit`, `Write` and `Bash`,
//! so a GitHub MCP server attached later would close a ticket unjudged.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::key::Snapshot;
use crate::manifest::{Manifest, Rule};
use crate::reading::{Reading, Status, millis};
use crate::steer::STEERABLE;
use crate::track::{MAIN, Ticket, Track};
use crate::track_next::{READY, TRACK_LABEL, tracks_of};

/// The instrument name every reading of ticket carries.
pub(crate) const INSTRUMENT: &str = "ticket";
/// The path every reading of ticket is stamped over: the workspace, since
/// a ticket has no tree of its own, so `done` holds the reading fresh
/// while the workspace's tree still matches (#31; the rule is stated in
/// `done`'s rustdoc). One statement, read by the writer and the reader.
pub(crate) const STAMPED_OVER: &str = ".";

/// A reading of `instrument` over `scope`, stamped over the workspace,
/// [`STAMPED_OVER`], as `snapshot` holds it.
pub(crate) fn stamped_over(instrument: &str, scope: &str, snapshot: &Snapshot) -> Result<Reading> {
    Reading::stamped_over(instrument, scope, snapshot, Path::new(STAMPED_OVER))
}

/// What a ready record says of a branch whose work opened nothing.
const NOTHING: &str = "nothing";

/// The configuration key of `ticket`'s ready record on `branch` (#2311),
/// one per ticket, so a branch carrying a bundle keeps every ticket it
/// readied (#2358): written by `ticket ready` beside
/// `branch.<name>.ticket`, in the configuration every worktree of the
/// repository shares, read at the merge, and dropped with the branch.
fn ready_key(branch: &str, ticket: Ticket) -> String {
    format!("branch.{branch}.{}", ready_name(ticket))
}

/// The variable name of `ticket`'s ready record in its branch's section,
/// the one spelling [`ready_key`] writes and [`record_of`] seeks.
fn ready_name(ticket: Ticket) -> String {
    format!("ready-{}", ticket.number())
}

/// The successors `ticket`'s ready record on `branch` names, `None` where
/// it records none: what `track work` reads a session's end by (#2311).
pub fn ready_of(root: &Path, branch: &str, ticket: Ticket) -> Result<Option<Successors>> {
    let key = ready_key(branch, ticket);
    let Ok(text) = crate::git::git(root, &["config", "--get", &key], None) else {
        return Ok(None);
    };
    Successors::of_record(&text).map(Some)
}

/// The successors the ready record of `ticket` names on whichever branch
/// wrote it, the last in the configuration's order where two branches did,
/// so a retry on a later branch wins, `None` where no branch has one:
/// what the merge closes the ticket with, since a bundle's tickets are
/// readied on a branch that records only the first of them (#2358).
fn record_of(root: &Path, ticket: Ticket) -> Result<Option<Successors>> {
    let pattern = format!(r"^branch\..*\.{}$", ready_name(ticket));
    let Ok(found) = crate::git::git(root, &["config", "--get-regexp", &pattern], None) else {
        return Ok(None);
    };
    found
        .lines()
        .last()
        .and_then(|line| line.split_once(' '))
        .map(|(_, text)| Successors::of_record(text))
        .transpose()
}

/// The heading whose boxes are the acceptance criteria, matched without
/// regard to case at level two.
const HEADING: &str = "acceptance criteria";
/// How many sub-issues GitHub holds under one parent, the number the
/// tracker refuses the next against. Not a bound this harness chose, so
/// `seams.toml` carries nothing about it.
pub const CAP: u64 = 100;

/// What `xtask ticket` does: read the criteria; read them and record the
/// branch ready to merge, naming what the work opened (#2311); or read
/// them and close under a verdict, naming what the work opened, at a
/// commit that stands where [`Reach`] says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    Read,
    Ready(Successors),
    Close(Verdict, Successors, Reach),
}

/// Where the commit a close is made at stands against `main` (#2249). A
/// ticket closed on a commit `main` does not contain says work is in
/// `main` that is not, which hides it from the frontier and from every
/// reader of the tracker, as #2163 to #2168 were closed on a stack that
/// had not merged; so a close of work done is refused off `main`, and a
/// withdrawal, which says nothing is in the tree, is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// `main` contains the commit.
    Landed,
    /// `main` does not contain the commit, on the branch named.
    Unmerged { commit: String, branch: String },
}

impl Reach {
    /// Where `root`'s `HEAD` stands against its `main`, its own or
    /// `origin`'s: contained when the merge base of the two is `HEAD`.
    pub fn of(root: &Path) -> Result<Reach> {
        let head = crate::git::rev_parse(root, &["HEAD"])?;
        if crate::track::merge_base(root, &head, MAIN)? == head {
            return Ok(Reach::Landed);
        }
        let branch = crate::git::rev_parse(root, &["--abbrev-ref", "HEAD"])?;
        Ok(Reach::Unmerged {
            commit: crate::track::short(&head).to_owned(),
            branch,
        })
    }
}

/// What a close says of the ticket: done, every box ticked, or withdrawn,
/// nothing in the tree to build, with the measurement that says so (#185).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Done,
    Withdrawn(String),
}

impl Verdict {
    /// From the flag of `close`: `--withdrawn <reason>` is the withdrawal,
    /// its absence the close of work done. A reason that is empty, or
    /// whitespace, is refused: the withdrawal is the measurement, and a
    /// bare flag measures nothing.
    pub fn given(withdrawn: Option<&str>) -> Result<Verdict> {
        match withdrawn {
            None => Ok(Verdict::Done),
            Some(reason) if reason.trim().is_empty() => bail!(
                "`--withdrawn` carries the measurement that says there is nothing here, what was run and where; an empty reason withdraws nothing"
            ),
            Some(reason) => Ok(Verdict::Withdrawn(reason.trim().to_owned())),
        }
    }
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
    /// The ready record's text: the successors' numbers, a space between,
    /// or `nothing`.
    fn record(&self) -> String {
        match self {
            Successors::Nothing => String::from(NOTHING),
            Successors::Tickets(tickets) => tickets
                .iter()
                .map(|t| t.number().to_string())
                .collect::<Vec<_>>()
                .join(" "),
        }
    }

    /// The successors a ready record names; a record that names neither
    /// numbers nor `nothing` is an error, never a close that opens nothing.
    fn of_record(text: &str) -> Result<Successors> {
        let text = text.trim();
        if text == NOTHING {
            return Ok(Successors::Nothing);
        }
        let tickets = text
            .split_whitespace()
            .map(|n| n.parse().map(Ticket::new))
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| {
                format!("the ready record {text:?} names neither tickets nor {NOTHING}")
            })?;
        if tickets.is_empty() {
            bail!("the ready record is empty; it names the tickets the work opened or {NOTHING}");
        }
        Ok(Successors::Tickets(tickets))
    }

    fn tickets(&self) -> &[Ticket] {
        match self {
            Successors::Tickets(tickets) => tickets,
            Successors::Nothing => &[],
        }
    }

    /// Each successor exists at the tracker, is open, and shares one of
    /// the tracks of `closed`, the ticket that opens it, where those are
    /// not `main`'s; the first that is not is the refusal.
    fn held(&self, gh: &Gh, root: &Path, closed: &Issue) -> Result<()> {
        let tracks = &tracks_of(closed.labels.iter().map(|l| l.name.as_str()))[..];
        if let Successors::Tickets(tickets) = self
            && tickets.is_empty()
        {
            bail!("opens no ticket and asserts nothing; `--opens <n>` or `--opens-nothing`");
        }
        // A close of one of `main`'s tickets asks nothing of a successor's
        // labels; any other is held to share one of the closed ticket's
        // tracks, and the edit named adds the first.
        let first = match tracks {
            [first, ..] if !tracks.contains(&MAIN) => Some(*first),
            _ => None,
        };
        for &successor in self.tickets() {
            let issue = gh
                .view(root, successor)
                .with_context(|| format!("opens {successor}"))?;
            if issue.state == State::Closed {
                bail!("opens {successor}, which is closed; a successor is an open ticket");
            }
            let Some(first) = first else { continue };
            let its = tracks_of(issue.labels.iter().map(|l| l.name.as_str()));
            if !its.iter().any(|track| tracks.contains(track)) {
                let wanted = tracks
                    .iter()
                    .map(|track| format!("`{TRACK_LABEL}{track}`"))
                    .collect::<Vec<_>>()
                    .join(" or ");
                bail!(
                    "opens {successor}, which carries no {wanted} label; what a {} ticket opens is that track's, and a ticket with no `{TRACK_LABEL}` label is {MAIN}'s: `gh issue edit {} --add-label {TRACK_LABEL}{first}`",
                    tracks.join(" and "),
                    successor.number()
                );
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
        let mut fenced: Option<Fence> = None;
        for line in body.lines() {
            let trimmed = line.trim();
            match fenced {
                Some(open) => {
                    if open.closed_by(line) {
                        fenced = None;
                    }
                    continue;
                }
                None => {
                    if let Some(open) = Fence::opened_by(line) {
                        fenced = Some(open);
                        continue;
                    }
                }
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

/// Boxes by name, numbered so a comma in one is not a gap between two.
fn named(boxes: &[String]) -> String {
    boxes
        .iter()
        .enumerate()
        .map(|(i, text)| format!("({}) {text}", i + 1))
        .collect::<Vec<_>>()
        .join("; ")
}

/// The opening line of a fenced block as `CommonMark` reads one: a run of
/// three or more backticks or tildes, the indentation before it, and for
/// backticks an info string with no backtick in it. Only a line of the
/// same character, at least as long, indented at most three spaces past
/// the opening and with nothing but whitespace after closes it, so a
/// quoted file's own fence line inside the block is the block's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fence {
    mark: char,
    run: usize,
    indent: usize,
}

impl Fence {
    /// The indentation in columns, a tab to the next multiple of four, and
    /// the rest of the line after it.
    fn split(line: &str) -> (usize, &str) {
        let mut columns = 0;
        for (at, c) in line.char_indices() {
            match c {
                ' ' => columns += 1,
                '\t' => columns += 4 - columns % 4,
                _ => return (columns, &line[at..]),
            }
        }
        (columns, "")
    }

    /// The run of `mark` that opens `rest`, in characters.
    fn run(rest: &str, mark: char) -> usize {
        rest.chars().take_while(|&c| c == mark).count()
    }

    /// The fence `line` opens, `None` for any other line. The opening may
    /// sit at any indentation, a fence in a list item being indented with
    /// the item; its close is measured from there.
    fn opened_by(line: &str) -> Option<Fence> {
        let (indent, rest) = Fence::split(line);
        let mark = rest.chars().next().filter(|c| matches!(c, '`' | '~'))?;
        let run = Fence::run(rest, mark);
        if run < 3 {
            return None;
        }
        if mark == '`' && rest[run..].contains('`') {
            return None;
        }
        Some(Fence { mark, run, indent })
    }

    /// Whether `line` closes this fence.
    fn closed_by(self, line: &str) -> bool {
        let (indent, rest) = Fence::split(line);
        if indent > self.indent + 3 {
            return false;
        }
        let run = Fence::run(rest, self.mark);
        run >= self.run && rest[run..].trim().is_empty()
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

/// One acceptance criterion asking for a write into a path a rule denies
/// the agent: the criterion as the body writes it, the path its main
/// clause names, the rule of the manifest whose scope covers that path,
/// and the instrument sanctioned to write that file where one stands
/// (#153).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denied {
    pub criterion: String,
    pub path: String,
    pub rule: String,
    pub sanctioned: Option<String>,
}

/// The criteria of a ticket carrying `ready-for-agent` that ask for a
/// write into a denied path: one row per criterion, at the first path of
/// its main clause a rule covers, carrying the `xtask steer` that writes
/// it where the path is one of [`STEERABLE`]. A ticket without the label
/// is not read, and neither is a path a rule exempts.
pub fn denied(manifest: &Manifest, labels: &[Label], criteria: &Criteria) -> Vec<Denied> {
    if !labels.iter().any(|label| label.name == READY) {
        return Vec::new();
    }
    // The rules that deny an agent the write: `deny = true` over `Edit`
    // or `Write`, the predicate `permit` refuses such a call by.
    let rules: Vec<&Rule> = manifest
        .rules
        .iter()
        .filter(|rule| rule.predicate.deny == Some(true))
        .filter(|rule| rule.tool.iter().any(|t| t == "Edit" || t == "Write"))
        .collect();
    let mut found = Vec::new();
    // Ticked or not: what a criterion asks for it asks for either way.
    for criterion in criteria.ticked.iter().chain(&criteria.unticked) {
        let clause = object(criterion);
        let writing = words(clause).any(|word| WRITING.contains(&word.as_str()));
        // A clause that lands its text elsewhere, drafted in a comment or
        // proposed on the ticket, asks for no write into the path it is
        // about (#1672).
        if !writing && elsewhere(clause) {
            continue;
        }
        // The path is the thing asked for while the clause is about it,
        // and not where the clause says it stays as it is (#2391).
        let asked = paths(clause).into_iter().filter(|(at, named)| {
            writing || (subject(clause, *at) && !kept(&clause[at + named.len()..]))
        });
        for (_, named) in asked {
            let path = Path::new(named);
            if let Some(rule) = rules
                .iter()
                .find(|rule| rule.covers(path) && !rule.exempts(path))
            {
                found.push(Denied {
                    criterion: criterion.clone(),
                    path: named.to_owned(),
                    rule: rule.id.clone(),
                    sanctioned: STEERABLE
                        .contains(&named)
                        .then(|| format!("cargo xtask steer {named} --from <proposal>")),
                });
                break;
            }
        }
    }
    found
}

/// The words a clause about something other than the criterion's object
/// opens with; what follows one describes a thing named.
const SUBORDINATING: [&str; 12] = [
    "that", "which", "whose", "when", "where", "while", "if", "so", "because", "unless", "until",
    "rather",
];

/// A criterion's main clause: its text up to the first comma, colon,
/// semicolon or subordinating word, which is what the criterion is about.
fn object(criterion: &str) -> &str {
    let mut at = 0;
    for word in criterion.split_inclusive(char::is_whitespace) {
        if let Some(offset) = word.find([',', ':', ';']) {
            return criterion[..at + offset].trim_end();
        }
        let bare = word.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        if SUBORDINATING.contains(&bare.to_ascii_lowercase().as_str()) {
            return criterion[..at].trim_end();
        }
        at += word.len();
    }
    criterion.trim_end()
}

/// The verbs that say the file changes rather than that it is read: a
/// clause carrying one asks for the write wherever the path stands in it.
const WRITING: [&str; 15] = [
    "add", "adds", "added", "gain", "gains", "gained", "write", "writes", "written", "edit",
    "edits", "edited", "bump", "bumps", "bumped",
];

/// The verbs that say a text is set down somewhere for a reader rather
/// than into the file it is about: drafted, proposed, quoted.
const LANDING: [&str; 15] = [
    "draft",
    "drafts",
    "drafted",
    "propose",
    "proposes",
    "proposed",
    "quote",
    "quotes",
    "quoted",
    "post",
    "posts",
    "posted",
    "suggest",
    "suggests",
    "suggested",
];

/// The places such a text lands in that are not a file of the checkout:
/// a comment, the ticket, the commit message.
const PLACES: [&str; 7] = [
    "comment", "comments", "ticket", "issue", "message", "thread", "reply",
];

/// Whether a clause lands its text somewhere other than the path it names
/// (#1672): a verb of [`LANDING`] stands in it, and `in` or `on` opens a
/// phrase one of [`PLACES`] stands in within the five words after it, as #1516's
/// "the `.claude/settings.json` `PostToolUse` entry is drafted in a comment on
/// the person's ticket" does. A verb of writing in the same clause wins,
/// and the caller reads it first.
fn elsewhere(clause: &str) -> bool {
    let words: Vec<String> = words(clause).collect();
    let landing = words.iter().any(|word| LANDING.contains(&word.as_str()));
    let placed = words.iter().enumerate().any(|(i, word)| {
        (word == "in" || word == "on")
            && words
                .iter()
                .skip(i + 1)
                .take(5)
                .any(|next| PLACES.contains(&next.as_str()))
    });
    landing && placed
}

/// The words that say a file stays as it is: a criterion asking that
/// nothing regress, which asks for no write.
const STAYING: [&str; 9] = [
    "still",
    "keeps",
    "keep",
    "stays",
    "stay",
    "remains",
    "remain",
    "unchanged",
    "untouched",
];

/// The words that say a thing kept is changed all the same, so the
/// clause asks for the write (#2396): "keeps its ceiling raised to
/// 40000". `set` is one of them only before `to`, since "the set of
/// rules" changes nothing.
const CHANGING: [&str; 4] = ["raised", "lowered", "changed", "moved"];

/// Whether what follows a clause's subject says the subject stays as it
/// is (#2391): one of [`STAYING`] among its first two words, as #630's
/// "`.claude/harness.local.md` still renders this machine's live numbers"
/// or "`seams.toml` is unchanged" says, and no word of [`CHANGING`] after
/// it (#2396). A possessive subject, "`seams.toml`'s ceiling stays", is
/// read past the noun it possesses. A word of staying further on is
/// about something else the clause names.
fn kept(after: &str) -> bool {
    let after = after.trim_start_matches(['`', '"']);
    let after = match after
        .strip_prefix("'s")
        .or_else(|| after.strip_prefix("\u{2019}s"))
    {
        Some(owned) => owned
            .trim_start()
            .split_once(char::is_whitespace)
            .map_or("", |(_noun, rest)| rest),
        None => after,
    };
    let words: Vec<String> = words(after).collect();
    let Some(at) = words
        .iter()
        .take(2)
        .position(|word| STAYING.contains(&word.as_str()))
    else {
        return false;
    };
    let rest = &words[at + 1..];
    !rest.iter().enumerate().any(|(i, word)| {
        CHANGING.contains(&word.as_str())
            || (word == "set" && rest.get(i + 1).is_some_and(|next| next == "to"))
    })
}

/// Whether a path is a path character: what a token naming one is made of.
fn of_a_path(c: char) -> bool {
    c.is_ascii_alphanumeric() || "._-/*".contains(c)
}

/// The words of a clause, lowercased, the punctuation and the backticks
/// around each left off.
fn words(clause: &str) -> impl Iterator<Item = String> {
    clause
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
}

/// Whether the path at `at` is the clause's subject: nothing but an
/// article stands before it, so the criterion is about that file.
fn subject(clause: &str, at: usize) -> bool {
    let before: Vec<String> = words(&clause[..at]).collect();
    before.is_empty() || (before.len() == 1 && ["the", "a", "an"].contains(&before[0].as_str()))
}

/// The paths a clause names, each with where it stands: every run of path
/// characters carrying a `.` or a `/` beside a letter or a digit, with the
/// backticks, quotes and sentence ends around it left off.
fn paths(clause: &str) -> Vec<(usize, &str)> {
    let mut found = Vec::new();
    let mut at = 0;
    for piece in clause.split_inclusive(|c: char| !of_a_path(c)) {
        let token = piece
            .trim_end_matches(|c: char| !of_a_path(c))
            .trim_end_matches('.');
        if (token.contains('.') || token.contains('/'))
            && token.chars().any(|c| c.is_ascii_alphanumeric())
        {
            found.push((at, token));
        }
        at += piece.len();
    }
    found
}

/// One issue as the tracker answers it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub state: State,
    pub url: String,
    /// The labels the tracker carries on it, which a close reads its own
    /// track from and holds a successor to (#141).
    #[serde(default)]
    pub labels: Vec<Label>,
    /// The tickets this one blocks, nested as `blockedBy` is; an edge the
    /// tracker has is not set again, since it refuses a second one.
    #[serde(default)]
    pub blocking: BlockedBy,
    /// The tickets blocking this one, which a merge naming it with no
    /// ready record reads as a partial landing while one is open (#2394).
    #[serde(default, rename = "blockedBy")]
    pub blocked_by: BlockedBy,
    /// How many sub-issues stand under it, which a chart sheet is held to
    /// [`CAP`] by (#448); zero for an issue the tracker answered before
    /// the field was asked for.
    #[serde(default, rename = "subIssuesSummary")]
    pub sub_issues: SubIssues,
}

/// What the tracker counts under one parent: the sub-issues it holds, as
/// `gh issue view --json subIssuesSummary` answers it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct SubIssues {
    pub total: u64,
}

/// One open issue as `gh issue list` answers it: what the frontier is
/// judged from, the labels and the blocking relation carried as the
/// tracker spells them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
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
}

/// One label, by name.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Label {
    pub name: String,
}

/// The tickets blocking one, as the tracker nests them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct BlockedBy {
    pub nodes: Vec<Blocker>,
}

impl BlockedBy {
    /// The blockers still open, by number: what blocks a ticket, since a
    /// ticket closes when its work is on `main` (#2270, #2326). The
    /// frontier, the chains and a merge's partial landing (#2394) read
    /// the rule here.
    pub(crate) fn open(&self) -> impl Iterator<Item = u64> + '_ {
        self.nodes
            .iter()
            .filter(|blocker| blocker.state == State::Open)
            .map(|blocker| blocker.number)
    }
}

/// One ticket in a blocking relation: its number and whether it is open.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Blocker {
    pub number: u64,
    pub state: State,
}

/// An issue's state as `gh` spells it, and `MERGED`, which `gh issue
/// view` answers of a number that is a merged pull request: a merge made
/// through GitHub names its pull request as `#n` (#3242), and no ticket
/// is ever merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum State {
    Open,
    Closed,
    Merged,
}

impl State {
    /// The word in lowercase, for a message.
    fn as_str(self) -> &'static str {
        match self {
            State::Open => "open",
            State::Closed => "closed",
            State::Merged => "merged",
        }
    }
}

/// The tracker: one `gh` program, run in the workspace so it resolves the
/// repository from the checkout's remote. The only two calls the
/// instrument makes are here. The command line runs [`Gh::on_path`]; a
/// test runs [`Gh::at`] over a shell file that answers `issue view` from
/// a recorded transcript and writes what `issue close` asked, so the
/// judgement is held without the network. Every call is timed, by its
/// verb, for the reading of [`merged`] that names what the tracker cost
/// (#2813).
#[derive(Debug)]
pub struct Gh {
    program: PathBuf,
    spent: RefCell<BTreeMap<String, Spent>>,
}

/// The calls of one verb `gh` was asked, and the time they took, each
/// retry of a read among them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Spent {
    calls: u64,
    took: Duration,
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
            spent: RefCell::default(),
        }
    }

    /// What every verb asked so far cost, and their sum.
    fn spent(&self) -> (BTreeMap<String, Spent>, Spent) {
        let by_verb = self.spent.borrow().clone();
        let all = by_verb.values().fold(Spent::default(), |sum, s| Spent {
            calls: sum.calls + s.calls,
            took: sum.took + s.took,
        });
        (by_verb, all)
    }

    /// `gh issue view <n> --json …`, one issue, its labels among them,
    /// since a close holds a successor to the closed ticket's track. A
    /// merged pull request is refused: it is no ticket.
    pub fn view(&self, root: &Path, ticket: Ticket) -> Result<Issue> {
        self.answer(root, ticket)?
            .with_context(|| format!("{ticket} is a pull request, not a ticket"))
    }

    /// What `gh issue view <n>` answers: the issue, or none where `#n` is
    /// a merged pull request, which `gh` answers as well (#3242).
    fn answer(&self, root: &Path, ticket: Ticket) -> Result<Option<Issue>> {
        let number = ticket.number().to_string();
        let text = self
            .read(
                root,
                &[
                    "issue",
                    "view",
                    &number,
                    "--json",
                    "number,title,body,state,url,labels,blocking,blockedBy,subIssuesSummary",
                ],
            )
            .with_context(|| format!("{ticket} could not be read from the tracker"))?;
        let issue: Issue = serde_json::from_str(&text)
            .with_context(|| format!("{ticket}: gh's answer is not an issue"))?;
        Ok((issue.state != State::Merged).then_some(issue))
    }

    /// `gh issue list --state open --json …`: every open issue with its
    /// labels and what blocks it, the one call `track next` makes (#73).
    /// Every open issue, not the labelled ones alone, so the frontier's
    /// filters are judged here, where a test can hold each of them.
    pub fn open(&self, root: &Path) -> Result<Vec<Open>> {
        let text = self
            .read(
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
        self.call(root, &["issue", "edit", &number, "--add-blocking", &opened])
            .with_context(|| format!("{ticket}'s edge to what it opened was not set"))?;
        Ok(())
    }

    /// `gh issue close <n> --comment <text>`: the one act.
    pub fn close(&self, root: &Path, ticket: Ticket, comment: &str) -> Result<()> {
        let number = ticket.number().to_string();
        self.call(root, &["issue", "close", &number, "--comment", comment])
            .with_context(|| format!("{ticket} was not closed"))?;
        Ok(())
    }

    /// [`Gh::call`] for a call that only reads, asked up to [`READS`]
    /// times with a longer pause before each retry, but for the answer
    /// that there is no such issue.
    fn read(&self, root: &Path, args: &[&str]) -> Result<String> {
        let mut asked = 1;
        loop {
            match self.call(root, args) {
                Err(err) if asked < READS && !no_issue(&err) => {
                    std::thread::sleep(Duration::from_millis(500) * asked);
                    asked += 1;
                }
                answer => return answer,
            }
        }
    }

    /// Run the program in `root` and return its stdout, the call timed
    /// under its verb, `args[1]`; a non-zero exit is an error carrying its
    /// stderr.
    fn call(&self, root: &Path, args: &[&str]) -> Result<String> {
        let started = Instant::now();
        let out = Command::new(&self.program)
            .args(args)
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();
        let mut spent = self.spent.borrow_mut();
        let verb = spent
            .entry(args.get(1).copied().unwrap_or("").to_owned())
            .or_default();
        verb.calls += 1;
        verb.took += started.elapsed();
        crate::printed(out, &format!("gh {}", args.join(" ")))
    }
}

/// How often a read of the tracker is asked before its failure stands.
/// Under load the machine runs out of ports for a moment, and `gh` fails
/// with "can't assign requested address" a call later would not (#2804);
/// a read changes nothing at the tracker, so asking it again is safe,
/// where a close asked twice would comment twice.
const READS: u32 = 3;

/// What `gh` says of a number that is no issue, a failure no retry mends.
const NO_ISSUE: &str = "Could not resolve to an issue";

/// Whether `err` is the tracker's answer that the number is no issue.
fn no_issue(err: &anyhow::Error) -> bool {
    format!("{err:#}").contains(NO_ISSUE)
}

/// Every criterion of `rows` as the reading cites it: the criterion, the
/// path its main clause names and the rule whose scope covers it.
fn asked(rows: &[&Denied]) -> String {
    rows.iter()
        .map(|row| {
            format!(
                "{:?} asks for {}, which {} denies an agent",
                row.criterion, row.path, row.rule
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// What a reading says of a ticket an agent may take whose criteria ask
/// for a write no instrument makes: every criterion that does, the path
/// and the rule, and what a person does with the ticket; `nothing` is what
/// a close this refuses left at the tracker.
fn mislabel(ticket: Ticket, rows: &[&Denied], nothing: &str) -> String {
    format!(
        "{ticket} carries `{READY}` and {} of its criteria ask{} for a write an agent may not make: {}; an edit there is a person's alone (harness §11), so a person relabels the ticket, `gh issue edit {} --add-label for-a-person --remove-label {READY}`, or writes the criterion as the act an agent takes{nothing}",
        rows.len(),
        if rows.len() == 1 { "s" } else { "" },
        asked(rows),
        ticket.number()
    )
}

/// What a reading says of a withdrawal refused by a ticked box (#185):
/// the boxes ticked by name, and what work half done is instead.
/// What a close of work done at a commit `main` does not contain is
/// refused with: the commit, its branch and the two ways on (#2249).
fn unlanded(commit: &str, branch: &str) -> String {
    format!(
        "{commit} on {branch} is not in main: merge it first, or close with --withdrawn where nothing of the work is in the tree"
    )
}

fn half_done(criteria: &Criteria) -> String {
    let n = criteria.ticked.len();
    let (is, named) = (if n == 1 { "is" } else { "are" }, named(&criteria.ticked));
    format!(
        "a withdrawal is of a ticket with no box ticked, and {n} {is}: {named}; work half done is not withdrawn, it is finished or its boxes are unticked by a person"
    )
}

/// The note a reading carries for a criterion naming a path `permit`
/// denies the agent but an instrument writes: what it matched and the
/// call that makes the write, said beside whatever verdict stands, since
/// the label is not wrong and the reading does not fail for it.
fn steerable(rows: &[&Denied]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let calls = rows
        .iter()
        .filter_map(|row| row.sanctioned.as_deref())
        .map(|call| format!("`{call}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "; {}; the write is an agent's through {calls}, which applies an addition a person has settled and refuses a change to what already binds",
        asked(rows)
    )
}

/// The scope a ticket's reading is written under: its number, the key
/// `done` reads it back by.
pub(crate) fn scope(ticket: Ticket) -> String {
    ticket.number().to_string()
}

/// The scope one sheet's cap reading is written under, `parent/<n>`:
/// beside the ticket readings and never over one, since the reading under
/// the sheet's own number is what `read` wrote of its criteria.
pub fn parent_scope(sheet: Ticket) -> String {
    format!("parent/{}", sheet.number())
}

/// `xtask ticket parent <n>`: how many sub-issues one chart sheet holds
/// and whether the tracker will take another, as one reading, so a
/// session knows before it files and not from the refusal after (#448).
/// Pass while there is room, naming how much; fail at [`CAP`], and fail
/// for a sheet that is closed, which indexes nothing filed under it. The
/// tracker is read and nothing there is asked to change.
pub fn parent(root: &Path, gh: &Gh, sheet: Ticket, session_id: Option<&str>) -> Result<Reading> {
    let issue = gh.view(root, sheet)?;
    let held = issue.sub_issues.total;
    let spare = CAP.saturating_sub(held);
    let mut reading = stamped(&Snapshot::disk(root)?, &parent_scope(sheet), session_id)?;
    reading.count("subissues", held)?;
    reading.count("cap", CAP)?;
    reading.count("room", spare)?;
    let holds = format!("{sheet} holds {held} of GitHub's cap of {CAP} sub-issues");
    let (status, message) = if issue.state == State::Closed {
        (
            Status::Fail,
            format!(
                "{holds} and is closed; a ticket filed under a closed sheet is indexed by no open chart, so file it against the sheet that continues this one"
            ),
        )
    } else if spare == 0 {
        (
            Status::Fail,
            format!(
                "{holds}: `gh issue create --parent {}` is refused with `Parent cannot have more than {CAP} sub-issues (addSubIssue)`; a chart that fills is continued by a sheet at the same row and never widened, and the filled one is closed with a comment naming the sheet that continues it (CONTEXT.md, Chart)",
                sheet.number()
            ),
        )
    } else {
        (Status::Pass, format!("{holds}, room for {spare} more"))
    };
    reading.status = status;
    reading.message = message;
    Ok(reading)
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
/// every box ticked and every successor held the edge and then the act at
/// the tracker before the reading is written. The caller records the
/// reading; its status is the exit.
///
/// A close holds each successor to the closed ticket's own track, read
/// from its `track:` labels ([`tracks_of`]).
pub fn run(
    root: &Path,
    manifest: &Manifest,
    gh: &Gh,
    act: Act,
    ticket: Ticket,
    session_id: Option<&str>,
) -> Result<Reading> {
    let (issue, snapshot) = (gh.view(root, ticket)?, Snapshot::disk(root)?);
    judged(root, manifest, gh, act, &issue, &snapshot, session_id)
}

/// [`run`] over an issue already read, stamped over `snapshot`: [`merged`]
/// reads each ticket once and the workspace once for all of them (#2813).
fn judged(
    root: &Path,
    manifest: &Manifest,
    gh: &Gh,
    act: Act,
    issue: &Issue,
    snapshot: &Snapshot,
    session_id: Option<&str>,
) -> Result<Reading> {
    let ticket = Ticket::new(issue.number);
    let criteria = Criteria::parse(&issue.body);
    let rows = denied(manifest, &issue.labels, &criteria);
    // The rows that say the label is wrong, a path no instrument may
    // write, and the rows an instrument writes: a path `xtask steer`
    // writes is an act an agent takes, so the row is a note and not a
    // refusal.
    let (denied, steered): (Vec<&Denied>, Vec<&Denied>) =
        rows.iter().partition(|row| row.sanctioned.is_none());
    let mut reading = stamped(snapshot, &scope(ticket), session_id)?;
    counted(&mut reading, &criteria, &denied, steered.len(), &act)?;
    let names = named(&criteria.unticked);
    let boxes = format!(
        "{} of {} criteria ticked",
        criteria.ticked.len(),
        criteria.count()
    );
    // What a close a condition refuses left at the tracker, which is
    // nothing, said once for every condition that refuses one.
    let nothing = match act {
        Act::Read => "",
        Act::Ready(_) => "; not ready, nothing recorded",
        Act::Close(..) => "; not closed, nothing changed at the tracker",
    };
    // A close at the tracker, which says what it opened and closed after
    // `said`, or why it did not.
    let closing = |successors: &Successors, comment: &str, said: &str| match close(
        gh, root, issue, &reading, successors, comment,
    ) {
        Ok((opened, closed)) => (
            Status::Pass,
            format!("{ticket}: {boxes}; {said}{opened}; {closed}"),
        ),
        Err(text) => (Status::Fail, format!("{ticket}: {boxes}; {text}")),
    };
    let (status, message) = match act {
        // A withdrawal is held to the boxes the other way round: one
        // ticked is work half done, and half-done work is not withdrawn.
        Act::Close(Verdict::Withdrawn(_), ..) if !criteria.ticked.is_empty() => (
            Status::Fail,
            format!("{ticket}: {boxes}; {}{nothing}", half_done(&criteria)),
        ),
        Act::Close(Verdict::Withdrawn(_), ..) if issue.state == State::Closed => (
            Status::Fail,
            format!("{ticket}: {boxes}; closed already, nothing changed at the tracker"),
        ),
        Act::Close(Verdict::Withdrawn(reason), successors, _) => {
            let comment = format!(
                "Withdrawn by `xtask ticket close` at {}: {reason}; {boxes}",
                reading.commit
            );
            closing(&successors, &comment, &format!("withdrawn: {reason}; "))
        }
        _ if !criteria.found => (
            Status::Fail,
            format!("{ticket} has no `## Acceptance criteria` heading; nothing to hold it to"),
        ),
        _ if criteria.count() == 0 => (
            Status::Fail,
            format!("{ticket} has no box under `## Acceptance criteria`; nothing to hold it to"),
        ),
        _ if !denied.is_empty() => (Status::Fail, mislabel(ticket, &denied, nothing)),
        _ if !criteria.met() => (
            Status::Fail,
            format!("{ticket}: {boxes}; unticked: {names}{nothing}"),
        ),
        Act::Read => (
            Status::Pass,
            format!("{ticket}: {boxes}; {}", issue.state.as_str()),
        ),
        Act::Ready(_) if issue.state == State::Closed => (
            Status::Fail,
            format!("{ticket}: {boxes}; closed already, nothing recorded"),
        ),
        Act::Ready(successors) => match ready(gh, root, issue, &reading, &successors) {
            Ok(said) => (Status::Pass, format!("{ticket}: {boxes}; {said}")),
            Err(text) => (Status::Fail, format!("{ticket}: {boxes}; {text}")),
        },
        Act::Close(Verdict::Done, ..) if issue.state == State::Closed => (
            Status::Fail,
            format!("{ticket}: {boxes}; closed already, nothing changed at the tracker"),
        ),
        // Every box ticked says the work is done, and a commit `main`
        // does not contain says it is not there yet (#2249).
        Act::Close(Verdict::Done, _, Reach::Unmerged { commit, branch }) => (
            Status::Fail,
            format!("{ticket}: {boxes}; {}{nothing}", unlanded(&commit, &branch)),
        ),
        Act::Close(Verdict::Done, successors, Reach::Landed) => {
            let comment = format!(
                "Closed by `xtask ticket close` at {}: {boxes}",
                reading.commit
            );
            closing(&successors, &comment, "")
        }
    };
    reading.status = status;
    // A criterion an instrument writes is said beside whatever verdict
    // stands, since it is what the heuristic matched and not a refusal.
    reading.message = message + &steerable(&steered);
    Ok(reading)
}

/// What a ticket reading counts: the boxes, the rows that say the label
/// is wrong and those an instrument writes, and for a ready record or a
/// close the successors and whether it is a withdrawal; its rule is the
/// first denial's.
fn counted(
    reading: &mut Reading,
    criteria: &Criteria,
    denied: &[&Denied],
    steered: usize,
    act: &Act,
) -> Result<()> {
    reading.count("criteria", criteria.count() as u64)?;
    reading.count("ticked", criteria.ticked.len() as u64)?;
    reading.count("unticked", criteria.unticked.len() as u64)?;
    reading.count("denied", denied.len() as u64)?;
    reading.count("steerable", steered as u64)?;
    reading.rule = denied
        .first()
        .map_or_else(String::new, |first| first.rule.clone());
    match act {
        Act::Close(verdict, successors, _) => {
            reading.count(OPENS, successors.tickets().len() as u64)?;
            reading.count(
                WITHDRAWN,
                u64::from(matches!(verdict, Verdict::Withdrawn(_))),
            )?;
        }
        Act::Ready(successors) => {
            reading.count(OPENS, successors.tickets().len() as u64)?;
        }
        Act::Read => {}
    }
    Ok(())
}

/// The successors a ready record or a close names, counted.
const OPENS: &str = "opens";
/// Whether a close was a withdrawal, 1 or 0: counted on a close alone,
/// which is how a close's reading is told from a ready record's.
const WITHDRAWN: &str = "withdrawn";

/// A close as its reading says it, read back from the store: what it
/// opened and whether it withdrew the ticket (#1157).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Closed {
    pub(crate) opens: u64,
    pub(crate) withdrawn: bool,
}

impl Closed {
    /// The close `reading` records; none for a reading of a read, a ready
    /// record or a merge that left a ticket open, and for a close refused.
    pub(crate) fn of(reading: &Reading) -> Option<Closed> {
        let count = |key| u64::try_from(reading.int(key)?).ok();
        (reading.instrument == INSTRUMENT && reading.status == Status::Pass).then_some(())?;
        Some(Closed {
            withdrawn: count(WITHDRAWN)? > 0,
            opens: count(OPENS)?,
        })
    }
}

/// The tickets the merge commits `since..HEAD` name, each closed as it
/// lands (#2270, #2311): the hook on a merge commit, and on a fast-forward
/// of `main`, calls this. An open ticket a subject names as `#n` is
/// closed through [`run`] with the successors its ready record names,
/// whichever branch wrote it (#2358). Those with no ready record are read
/// after every close, so a blocker the same merge closes holds nothing,
/// and each is left open as [`left_open`] says. A ticket closed already,
/// and a number the tracker answers is no issue, is passed over; a number
/// it answers as a merged pull request is [`pulled`], skipped with a
/// reading that says so (#3242). One the
/// tracker cannot read, or whose close it refuses, is [`unclosed`] and the
/// run goes on, so one failure of the network neither passes a ticket
/// over nor loses the closes made before it (#2804). Off `main` nothing is closed: the fast-forward that brings
/// the merge there calls this again.
///
/// On `main` the last reading is the run's own, under [`MERGED`]: what
/// each phase took, each ticket's close and every `gh` call by its verb,
/// and the run whole, so a merge's post-commit time is read from the
/// store and not guessed (#2813). The whole is the time inside this
/// call: cargo's start of the binary before it, a no-op build of a fraction
/// of a second where pre-commit has built it, and the store's write of
/// the readings after it are outside.
pub fn merged(root: &Path, manifest: &Manifest, gh: &Gh, since: &str) -> Result<Vec<Reading>> {
    let started = Instant::now();
    if Reach::of(root)? != Reach::Landed {
        return Ok(Vec::new());
    }
    // The commit `since` names now, for a message read after `HEAD` moves.
    let parent = crate::git::rev_parse(root, &["--short", since])?;
    let again = format!("`xtask ticket merged --since {parent}` on main again");
    let mut readings = Vec::new();
    let mut unready = Vec::new();
    let mut pulls = Vec::new();
    let named = named_by_merges(root, &format!("{since}..HEAD"))?;
    let hashing = Instant::now();
    let snapshot = Snapshot::disk(root)?;
    let hashed = hashing.elapsed();
    let mut closes = Vec::new();
    for &ticket in &named {
        let began = Instant::now();
        let outcome = gh.answer(root, ticket).and_then(|answer| {
            let Some(issue) = answer else {
                pulls.push(ticket);
                return Ok(None);
            };
            if issue.state == State::Closed {
                return Ok(None);
            }
            let Some(successors) = record_of(root, ticket)? else {
                unready.push(ticket);
                return Ok(None);
            };
            let act = Act::Close(Verdict::Done, successors, Reach::Landed);
            judged(root, manifest, gh, act, &issue, &snapshot, None).map(Some)
        });
        match outcome {
            Ok(Some(reading)) if reading.status != Status::Pass => {
                readings.push(unclosed(reading, &again)?);
            }
            Ok(Some(reading)) => {
                closes.push(format!("closed {ticket} in {}", secs(began.elapsed())));
                readings.push(reading);
            }
            Ok(None) => {}
            Err(err) if no_issue(&err) => {}
            Err(err) => readings.push(failed(&snapshot, ticket, &err, &again)?),
        }
    }
    for &pull in &pulls {
        readings.push(pulled(&snapshot, pull)?);
    }
    for ticket in unready {
        match gh.view(root, ticket) {
            Ok(issue) if issue.state == State::Closed => {}
            Ok(issue) => readings.push(left_open(&snapshot, &issue, &again)?),
            Err(err) if no_issue(&err) => {}
            Err(err) => readings.push(failed(&snapshot, ticket, &err, &again)?),
        }
    }
    let mut own = stamped(&snapshot, MERGED, None)?;
    let made = closes.len();
    let (by_verb, all) = gh.spent();
    let took = started.elapsed();
    own.status = Status::Pass;
    let tickets = named.len().saturating_sub(pulls.len());
    own.count("named", tickets as u64)?;
    own.count("pulls", pulls.len() as u64)?;
    own.count("closed", made as u64)?;
    own.count("gh_calls", all.calls)?;
    own.count("gh_ms", millis(all.took))?;
    own.count("workspace_ms", millis(hashed))?;
    own.count("ms", millis(took))?;
    // Each phase: the closes in the order made, the tracker by verb, and
    // the read of the workspace every reading is stamped over.
    let mut phases = closes;
    phases.extend(
        by_verb
            .iter()
            .map(|(verb, s)| format!("gh {verb} {} time(s) in {}", s.calls, secs(s.took))),
    );
    phases.push(format!("the workspace read in {}", secs(hashed)));
    let skipped = pulls.iter().map(Ticket::to_string).collect::<Vec<_>>();
    let skipped = if skipped.is_empty() {
        String::new()
    } else {
        format!(
            ", and skipped {}: pull request(s), not tickets",
            skipped.join(", ")
        )
    };
    own.message = format!(
        "the merges since {} name {} ticket(s), {} closed here{skipped}; {}; {} in all",
        parent.trim(),
        tickets,
        made,
        phases.join("; "),
        secs(took)
    );
    readings.push(own);
    Ok(readings)
}

/// The scope of the reading [`merged`] writes of its own run on `main`.
pub const MERGED: &str = "merged";

/// A span in seconds to a tenth, for a message.
fn secs(span: Duration) -> String {
    format!("{:.1} s", span.as_secs_f64())
}

/// A reading of the instrument over `scope`, stamped over the workspace
/// as every reading of the instrument is, taken in the session named.
fn stamped(snapshot: &Snapshot, scope: &str, session_id: Option<&str>) -> Result<Reading> {
    let mut reading = stamped_over(INSTRUMENT, scope, snapshot)?;
    session_id
        .unwrap_or_default()
        .clone_into(&mut reading.session);
    Ok(reading)
}

/// The reading of a ticket [`merged`] could not even read or close for
/// `err`, as [`unclosed`] says it.
fn failed(
    snapshot: &Snapshot,
    ticket: Ticket,
    err: &anyhow::Error,
    again: &str,
) -> Result<Reading> {
    let mut reading = stamped(snapshot, &scope(ticket), None)?;
    reading.message = format!("{err:#}");
    unclosed(reading, again)
}

/// A ticket a merge names that it left open: failed, counted [`UNCLOSED`]
/// so `done` names it ([`stranded`]) until a close replaces the reading,
/// and saying how to close it.
fn unclosed(mut reading: Reading, again: &str) -> Result<Reading> {
    reading.status = Status::Fail;
    reading.count(UNCLOSED, 1)?;
    reading.message = format!(
        "{}; the merge did not close it: close it with `xtask ticket close {}` naming what it opened, or, once what stopped it is settled, run {again}",
        reading.message, reading.scope
    );
    Ok(reading)
}

/// The reading of a number a merge names that `gh` answers as a merged
/// pull request (#3242): skipped, since a merge made through GitHub names
/// its pull request and not a ticket, and the ticket the pull request
/// closes is GitHub's to close. It replaces a reading that left the
/// number [`unclosed`], so `done` names it no longer.
fn pulled(snapshot: &Snapshot, pull: Ticket) -> Result<Reading> {
    let mut reading = stamped(snapshot, &scope(pull), None)?;
    reading.status = Status::Skip;
    reading.message =
        format!("{pull} is a pull request a merge into main names, not a ticket; skipped");
    Ok(reading)
}

/// The reading of a ticket a merge names with no ready record, which it
/// leaves open. It is a partial landing (#2394) while a box stands
/// unticked and an open ticket still blocks it ([`BlockedBy::open`]):
/// the build skill leaves a ticket
/// whose last box waits on another open and wires that one on, so the
/// reading skips the close, saying so and what holds it. Otherwise it is
/// [`unclosed`], and where the merge lands part of it the message says
/// how to wire the blocker on first.
fn left_open(snapshot: &Snapshot, issue: &Issue, again: &str) -> Result<Reading> {
    let ticket = Ticket::new(issue.number);
    let mut left = stamped(snapshot, &scope(ticket), None)?;
    let blockers: Vec<String> = issue
        .blocked_by
        .open()
        .map(|number| Ticket::new(number).to_string())
        .collect();
    if blockers.is_empty() || Criteria::parse(&issue.body).unticked.is_empty() {
        left.message = format!(
            "{ticket} is named by a merge into main and no branch has a ready record of it; where the merge lands part of it and a box waits on another ticket, wire that one on with `gh issue edit <blocker> --add-blocking {}` first",
            ticket.number()
        );
        return unclosed(left, again);
    }
    left.status = Status::Skip;
    left.count(PARTIAL, 1)?;
    left.message = format!(
        "{ticket} is named by a merge into main with no ready record, a box unticked, and waits on {}; the merge lands part of it, left open",
        blockers.join(", ")
    );
    Ok(left)
}

/// The number a reading of [`merged`] counts for a ticket the merge named
/// and left open, which [`stranded`] reads: for want of a ready record,
/// or because the tracker could not be read or refused the close.
const UNCLOSED: &str = "unclosed";

/// The number a reading of [`merged`] counts for a ticket the merge
/// landed part of: named, with no ready record, and blocked by an open
/// ticket (#2394).
const PARTIAL: &str = "partial";

/// The tickets the subjects of the merge commits of `range` name as `#n`,
/// each once, in the order named.
fn named_by_merges(root: &Path, range: &str) -> Result<Vec<Ticket>> {
    let subjects = crate::git::git(root, &["log", "--merges", "--format=%s", range], None)?;
    let mut named: Vec<Ticket> = Vec::new();
    for word in subjects.split(|c: char| !(c == '#' || c.is_ascii_digit())) {
        if let Some(Ok(number)) = word.strip_prefix('#').map(str::parse) {
            let ticket = Ticket::new(number);
            if !named.contains(&ticket) {
                named.push(ticket);
            }
        }
    }
    Ok(named)
}

/// What `done` names of the merges since `base` (#2358): each ticket a
/// merge commit's subject names whose last `ticket` reading says
/// [`merged`] left it [`unclosed`], until a close replaces it. Where `base` is `HEAD`, as on `main`, the merges are
/// `HEAD`'s own, so the session that made the merge reads what it
/// stranded, and a branch laid on that tip with no commit of its own
/// reads it the same; a branch reads the merges it carries, `main`'s
/// being behind its base. The store is read, never the tracker.
pub fn stranded(root: &Path, store: &crate::reading::Store, base: &str) -> Result<Vec<String>> {
    let head = crate::git::rev_parse(root, &["HEAD"])?;
    let range = if head != base {
        format!("{base}..HEAD")
    } else if crate::git::rev_parse(root, &["--verify", "--quiet", "HEAD^1"]).is_ok() {
        String::from("HEAD^1..HEAD")
    } else {
        // A root commit has no first parent, and so no merge to read.
        return Ok(Vec::new());
    };
    let mut unmet = Vec::new();
    for ticket in named_by_merges(root, &range)? {
        if let Some(reading) = store.read(INSTRUMENT, &scope(ticket))?
            && reading.numbers.contains_key(UNCLOSED)
        {
            unmet.push(format!("ticket: {}", reading.message));
        }
    }
    Ok(unmet)
}

/// The branch ready to merge (#2311): every successor held as a close
/// holds it, and the record written on the branch `HEAD` is on, which the
/// merge reads to close the ticket. Nothing at the tracker changes.
fn ready(
    gh: &Gh,
    root: &Path,
    issue: &Issue,
    reading: &Reading,
    successors: &Successors,
) -> std::result::Result<String, String> {
    let session_id = (!reading.session.is_empty()).then_some(reading.session.as_str());
    let opened = successors.describe(session_id);
    successors
        .held(gh, root, issue)
        .map_err(|err| format!("{err:#}; not ready, nothing recorded"))?;
    let branch = crate::track::head_branch(root)
        .ok_or("HEAD is detached, and a ready record is a branch's; not ready, nothing recorded")?;
    let key = ready_key(&branch, Ticket::new(issue.number));
    crate::git::git(root, &["config", &key, &successors.record()], None)
        .map_err(|err| format!("{key} could not be written: {err:#}; not ready"))?;
    Ok(format!(
        "{opened}; ready to merge at {}, recorded as {key}; the merge into main closes it",
        reading.commit
    ))
}

/// The act at the tracker, the same for a close of work done and a
/// withdrawal: every successor held, the edges the tracker lacks set, and
/// the issue closed with `comment` and what it opened appended. What comes
/// back is what the reading says of the successors and of the close, or
/// the text of the refusal, which says what stands at the tracker.
fn close(
    gh: &Gh,
    root: &Path,
    issue: &Issue,
    reading: &Reading,
    successors: &Successors,
    comment: &str,
) -> std::result::Result<(String, String), String> {
    let ticket = Ticket::new(issue.number);
    let session_id = (!reading.session.is_empty()).then_some(reading.session.as_str());
    let opened = successors.describe(session_id);
    let comment = format!("{comment}; {opened}.");
    // The edges the tracker has already stand; only the rest are set,
    // so a close retried after one that failed past this point lands.
    let missing = successors
        .tickets()
        .iter()
        .copied()
        .filter(|t| !issue.blocking.nodes.iter().any(|n| n.number == t.number()))
        .collect::<Vec<_>>();
    successors
        .held(gh, root, issue)
        .and_then(|()| gh.blocking(root, ticket, &missing))
        .map_err(|err| format!("{err:#}; not closed, nothing changed at the tracker"))?;
    // The edge is set by now; a close the tracker refuses leaves it
    // standing, and the message says so.
    gh.close(root, ticket, &comment).map_err(|err| {
        let edge = match successors {
            Successors::Tickets(_) => "; the edge to what it opened stands",
            Successors::Nothing => "",
        };
        format!("{opened}{edge}; {err:#}")
    })?;
    let closed = format!(
        "closed by xtask ticket close at {}{}",
        reading.commit,
        session_id
            .map(|id| format!(" in session {id}"))
            .unwrap_or_default()
    );
    Ok((opened, closed))
}

#[cfg(test)]
mod tests {
    use super::{Criteria, Denied, Label, Successors, Verdict, denied};
    use crate::manifest::{Manifest, OneOrMany, Rule};
    use crate::track::Ticket;

    /// The manifest such a ticket is held to: one rule denying `Edit` and
    /// `Write` over the pin, the manifest and the harness's own tree, with
    /// the skills under it exempt, so both keys are read here.
    fn manifest() -> Manifest {
        let mut manifest = Manifest::named("xtask");
        let mut rule = Rule::named("harness.self", "oracle.toml");
        rule.scope = OneOrMany::Many(vec![
            "oracle.toml".to_owned(),
            "seams.toml".to_owned(),
            ".claude/**".to_owned(),
        ]);
        rule.tool = OneOrMany::Many(vec!["Edit".to_owned(), "Write".to_owned()]);
        rule.exempt = vec![".claude/skills/**".to_owned()];
        manifest.rules.push(rule);
        manifest
    }

    /// One criterion of a ticket carrying `label`, as the judgement reads
    /// it.
    fn read(label: &str, criterion: &str) -> Vec<Denied> {
        let labels = [Label {
            name: label.to_owned(),
        }];
        let criteria = Criteria::parse(&format!("## Acceptance criteria\n\n- [ ] {criterion}\n"));
        denied(&manifest(), &labels, &criteria)
    }

    /// What a criterion asks for is its main clause's: the path the clause
    /// opens with, or the path a verb of writing stands beside (#153).
    #[test]
    fn a_criterion_whose_object_is_a_denied_path_asks_for_the_write_and_one_naming_it_does_not() {
        let subject = read(
            "ready-for-agent",
            "`oracle.toml` carries a `[regime.dump]` with the ini above",
        );
        assert_eq!(subject.len(), 1, "{subject:?}");
        assert_eq!(subject[0].path, "oracle.toml");
        assert_eq!(subject[0].rule, "harness.self");
        assert_eq!(
            read(
                "ready-for-agent",
                "A `[regime.dump]` block is added to `oracle.toml`"
            )
            .len(),
            1,
            "a verb of writing names the path it writes"
        );
        // The path `permit` denies but `xtask steer` writes, so the row is
        // a note and not a refusal, and a path no instrument writes is the
        // refusal (#152).
        assert!(subject[0].sanctioned.is_some(), "{subject:?}");
        let hook = read("ready-for-agent", "`.claude/settings.json` gains a hook");
        assert_eq!(hook.len(), 1, "{hook:?}");
        assert_eq!(hook[0].sanctioned, None, "{hook:?}");
        // The clause the path stands in is about the fixture and the
        // ticket, not about the pin.
        assert!(
            read(
                "ready-for-agent",
                "One fixture under `xtask/fixtures/` that must fail: a ticket whose criterion names `oracle.toml`"
            )
            .is_empty()
        );
        // The file is where a number is read, not a file that changes.
        assert!(
            read(
                "ready-for-agent",
                "It totals what the copy would spend against the crate's 26000-line ceiling in `seams.toml`, and names what the design drops"
            )
            .is_empty()
        );
    }

    /// A criterion whose path is the subject of a text drafted in a comment
    /// asks for a comment and not for a write (#1672), and one that writes
    /// the path stays a write whatever else it names.
    #[test]
    fn a_criterion_drafting_a_denied_path_in_a_comment_asks_for_no_write() {
        assert!(
            read(
                "ready-for-agent",
                "The `.claude/settings.json` PostToolUse entry is drafted in a comment on the person's ticket"
            )
            .is_empty()
        );
        assert!(
            read(
                "ready-for-agent",
                "`oracle.toml`'s `[regime.dump]` block is proposed in the commit message"
            )
            .is_empty()
        );
        // The fixture's write stands, and a verb of writing beside a draft
        // is still the write.
        assert_eq!(
            read(
                "ready-for-agent",
                "`.claude/settings.json` gains the hook the lane runs under"
            )
            .len(),
            1
        );
        assert_eq!(
            read(
                "ready-for-agent",
                "`.claude/settings.json` gains the hook drafted in a comment on the ticket"
            )
            .len(),
            1
        );
        // A place without a verb that sets the text down there is not one.
        assert_eq!(
            read(
                "ready-for-agent",
                "`.claude/settings.json` carries the hook named in the ticket"
            )
            .len(),
            1
        );
    }

    /// A subject said to stay as it is asks for no write (#2391), and a
    /// word of staying past the two after the subject is about something
    /// else the clause names, so the write stands. A possessive subject
    /// is read past the noun it possesses, and a word of change after
    /// the staying word is the write it says (#2396).
    #[test]
    fn a_criterion_whose_denied_subject_stays_as_it_is_asks_for_no_write() {
        for kept in [
            "`.claude/harness.local.md` still renders this machine's live numbers",
            "`seams.toml` keeps its ceilings",
            "`oracle.toml` stays as it is",
            "`seams.toml` is unchanged",
            "`seams.toml`'s ceiling stays",
            "`seams.toml`\u{2019}s ceiling remains unchanged",
            "`seams.toml` keeps the set of rules it has",
        ] {
            assert!(read("ready-for-agent", kept).is_empty(), "{kept}");
        }
        for written in [
            "`oracle.toml` carries a `[regime.dump]` the lane still reads",
            "`.claude/settings.json` still gains a hook",
            "`seams.toml` keeps its ceiling raised to 40000",
            "`seams.toml` keeps its ceiling set to 40000",
            "`seams.toml`'s ceiling stays lowered",
        ] {
            assert_eq!(read("ready-for-agent", written).len(), 1, "{written}");
        }
    }

    /// The label says whose the act is, and the rule's `exempt` says which
    /// paths under its scope it does not deny.
    #[test]
    fn a_for_a_person_ticket_is_not_read_this_way_and_an_exempt_path_is_not_denied() {
        assert!(read("for-a-person", "`oracle.toml` carries a `[regime.dump]`").is_empty());
        assert!(read("ready-for-agent", "`.claude/settings.json` gains a hook").len() == 1);
        assert!(
            read(
                "ready-for-agent",
                "`.claude/skills/build/SKILL.md` gains the sentence"
            )
            .is_empty()
        );
    }

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

    /// #1314's body as it was first filed (#1320): a diff quoted in a
    /// fenced block, the diff carrying the quoted file's own fence line
    /// four spaces in. That line is the block's text, not its close, so the
    /// heading below the block is read; an outer `~~~` reads the same.
    #[test]
    fn a_fence_line_quoted_inside_a_fenced_block_does_not_close_it() {
        let filed = BODY_1314;
        for body in [
            filed.to_owned(),
            filed
                .replace(
                    "```diff
", "~~~diff
",
                )
                .replace(
                    "
```

## Acceptance",
                    "
~~~

## Acceptance",
                ),
        ] {
            let criteria = Criteria::parse(&body);
            assert!(criteria.found, "{body}");
            assert_eq!(criteria.count(), 1, "{body}");
            assert!(criteria.met(), "{body}");
        }
    }

    /// The close as `CommonMark` has it (#1320): the same character, a run
    /// at least the opening's, indented at most three spaces past the
    /// opening, nothing but whitespace after; a fence never closed runs to
    /// the end of the body.
    #[test]
    fn a_fence_closes_only_on_its_own_character_and_length() {
        let after = "
## Acceptance criteria
- [ ] one
";
        let read = |block: &str| Criteria::parse(&format!("{block}{after}")).found;
        // Closed: the heading after it is read.
        assert!(read(
            "```
x
```
"
        ));
        assert!(read(
            "````
x
`````
"
        ));
        assert!(read(
            "~~~
x
   ~~~  
"
        ));
        assert!(read(
            "```rust
x
```
"
        ));
        assert!(read(
            "  - item
    ```
    x
    ```
"
        ));
        // Still open: the heading is the block's text.
        assert!(!read(
            "```
~~~
"
        ));
        assert!(!read(
            "~~~
```
"
        ));
        assert!(!read(
            "````
```
"
        ));
        assert!(!read(
            "```
    ```
"
        ));
        assert!(!read(
            "```
``` not a close
"
        ));
        // Not an opening: an inline span and a backtick in the info string.
        assert!(read(
            "```x```
"
        ));
        assert!(read(
            "``not a fence
"
        ));
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

    /// A withdrawal is its reason (#185): the flag absent is the close of
    /// work done, the flag with a reason is the withdrawal with it trimmed,
    /// and the flag with nothing or with whitespace withdraws nothing.
    #[test]
    fn a_withdrawal_carries_its_measurement_and_a_bare_flag_is_refused() {
        assert_eq!(Verdict::given(None).unwrap(), Verdict::Done);
        assert_eq!(
            Verdict::given(Some("  measured on 7d6f1e8: nothing here \n")).unwrap(),
            Verdict::Withdrawn("measured on 7d6f1e8: nothing here".to_owned())
        );
        for bare in ["", "   ", "\n"] {
            let err = Verdict::given(Some(bare)).unwrap_err().to_string();
            assert!(err.contains("an empty reason withdraws nothing"), "{err}");
        }
    }

    /// #1314's body as filed, before the diff was dropped (efc13721).
    const BODY_1314: &str = r"#1312 adds `cargo xtask witness --unmeasured`, which measures only the crates with no passing witness reading over their tree now. The merge check in `.claude/skills/specs/SKILL.md` §6 still runs `cargo xtask witness` over every crate, right after the merge commit's hook measured the staged ones over the same tree, and says nothing about not running while a merge commit's hook runs, which is where a session's load-flaky witnesses and tests came from. The file is under harness.self, so the edit is a person's.

The proposed patch (`patch -p0 < SKILL-1312.patch` from the checkout, or by hand):

```diff
--- .claude/skills/specs/SKILL.md	2026-09-30 02:35:26
+++ .claude/skills/specs/SKILL.md	2026-09-30 14:36:45
@@ -78,14 +78,14 @@
 
 ## 6. The merge check
 
-After the last merge of a batch, before any push:
+After the last merge of a batch, before any push, and never while a merge commit's hook is running: the check and the hook share the target directory and the cores, and a hook starved of either is what fails witnesses and tests under load (#1312).
 
 1. `cargo xtask port-map` when `crates/php_ext` changed.
-2. `cargo xtask witness` over every crate. The hook runs only staged crates' witnesses, which is how #681 broke ext/reflection unseen until #733.
+2. `cargo xtask witness --unmeasured`: every crate without a passing reading over its tree now. The hook runs only staged crates' witnesses, which is how #681 broke ext/reflection unseen until #733; the ones it measured over the tree that landed stand and are not run again (#1312).
 3. On a fresh release build, the lane and `Zend/tests`, each held test by test against the merge base, the tip before the batch, whose runs the cut's first step kept (#664). The witness and the four runs read the tree and not each other, and each run works in its own copy of the pinned tree (#1183), so they run at once; the lane is the longest:
    ```
    cargo build --release -p php_sapi_cli -p php_sapi_cgi -p xtask
-   cargo xtask witness > $S/witness-<head>.out 2>&1 &
+   cargo xtask witness --unmeasured > $S/witness-<head>.out 2>&1 &
    cargo xtask phpt --census --against <base> > $S/lane-<head>.out &
    cargo xtask phpt --subset Zend/tests --against <base> > $S/zend-<head>.out &
    cargo xtask phpt --subset tests --against <base> > $S/tests-<head>.out &
```

## Acceptance criteria

- [x] A person lands the merge check's step 2 as `cargo xtask witness --unmeasured`, in the prose and in the fenced block, and the sentence that the check never runs while a merge commit's hook is running, or rules otherwise here.

";
}
