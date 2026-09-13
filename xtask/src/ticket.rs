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
//! mostly say.
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
//! every track's (CONTEXT.md, Frontier), so a close running on a track
//! refuses a successor without that track's label, names the `gh issue
//! edit <n> --add-label track:<name>` that adds it, and asks the tracker
//! for nothing. Nothing originates the label otherwise: the skills carry
//! the closing ticket's when it has one, so a ticket filed off an
//! unlabelled one landed in both frontiers, and the close is the session's
//! last act and the only instrument that already reads the tracker. A
//! close on `main` asks nothing of a successor's labels, since a ticket
//! filed from the main checkout is every track's by design. The edge is
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
//! What the seam does not cover, written down rather than pretended: the
//! rule is judged by `permit`, whose tools are `Edit`, `Write` and `Bash`,
//! so a GitHub MCP server attached later would close a ticket unjudged.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::manifest::{Manifest, Rule};
use crate::reading::{Reading, Status};
use crate::steer::STEERABLE;
use crate::track::{MAIN, Ticket, Track};
use crate::track_next::{READY, TRACK_LABEL};

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

/// What `xtask ticket` does: read the criteria, or read them and close
/// under a verdict, naming what the work opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    Read,
    Close(Verdict, Successors),
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

    /// The reason a withdrawal carries, none for a close of work done.
    fn withdrawn(&self) -> Option<&str> {
        match self {
            Verdict::Done => None,
            Verdict::Withdrawn(reason) => Some(reason),
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
    fn tickets(&self) -> &[Ticket] {
        match self {
            Successors::Tickets(tickets) => tickets,
            Successors::Nothing => &[],
        }
    }

    /// Each successor exists at the tracker, is open, and on a track
    /// carries that track's label; the first that is not is the refusal.
    fn held(&self, gh: &Gh, root: &Path, track: &str) -> Result<()> {
        if let Successors::Tickets(tickets) = self
            && tickets.is_empty()
        {
            bail!("opens no ticket and asserts nothing; `--opens <n>` or `--opens-nothing`");
        }
        let label = (track != MAIN).then(|| format!("{TRACK_LABEL}{track}"));
        for &successor in self.tickets() {
            let issue = gh
                .view(root, successor)
                .with_context(|| format!("opens {successor}"))?;
            if issue.state == State::Closed {
                bail!("opens {successor}, which is closed; a successor is an open ticket");
            }
            if let Some(label) = &label
                && !issue.labels.iter().any(|carried| &carried.name == label)
            {
                bail!(
                    "opens {successor}, which carries no `{label}` label; a ticket filed on the {track} track is that track's, and one with no `{TRACK_LABEL}` label is every track's: `gh issue edit {} --add-label {label}`",
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

    /// Every unticked box by name, numbered so a comma in one is not a gap
    /// between two.
    fn unticked_named(&self) -> String {
        named(&self.unticked)
    }

    /// Every ticked box by name, the same way, for the withdrawal one
    /// refuses.
    fn ticked_named(&self) -> String {
        named(&self.ticked)
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

impl Denied {
    /// Whether this row says the label is wrong: it does while no
    /// instrument may write the path, and a path `xtask steer` writes is
    /// an act an agent takes, so the row is a note and not a refusal.
    fn mislabelled(&self) -> bool {
        self.sanctioned.is_none()
    }
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
        // The path is the thing asked for while the clause is about it.
        let asked = paths(clause)
            .into_iter()
            .filter(|(at, _)| writing || subject(clause, *at));
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
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub state: State,
    pub url: String,
    /// The labels the tracker carries on it, which a close reads to hold a
    /// successor to its track (#141).
    #[serde(default)]
    pub labels: Vec<Label>,
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

    /// `gh issue view <n> --json …`, one issue, its labels among them,
    /// since a close holds a successor to the track it was filed on.
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
                "number,title,body,state,url,labels,blocking",
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
fn half_done(criteria: &Criteria) -> String {
    format!(
        "a withdrawal is of a ticket with no box ticked, and {} {}: {}; work half done is not withdrawn, it is finished or its boxes are unticked by a person",
        criteria.ticked.len(),
        if criteria.ticked.len() == 1 {
            "is"
        } else {
            "are"
        },
        criteria.ticked_named()
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
/// `track` is the track the close runs on, the name [`Track::of`] answers,
/// which a successor's `track:` label is held to; the command line reads
/// it once and hands it down, the way `track next` takes the track it
/// answers for.
pub fn run(
    root: &Path,
    manifest: &Manifest,
    gh: &Gh,
    act: Act,
    ticket: Ticket,
    session_id: Option<&str>,
    track: &str,
) -> Result<Reading> {
    let issue = gh.view(root, ticket)?;
    let criteria = Criteria::parse(&issue.body);
    let rows = denied(manifest, &issue.labels, &criteria);
    // The rows that say the label is wrong, and the rows an instrument
    // writes, which are a note and not a refusal.
    let (denied, steered): (Vec<&Denied>, Vec<&Denied>) =
        rows.iter().partition(|row| row.mislabelled());
    let mut reading =
        Reading::stamped_over(INSTRUMENT, &scope(ticket), root, Path::new(STAMPED_OVER))?;
    reading.count("criteria", criteria.count() as u64)?;
    reading.count("ticked", criteria.ticked.len() as u64)?;
    reading.count("unticked", criteria.unticked.len() as u64)?;
    reading.count("denied", denied.len() as u64)?;
    reading.count("steerable", steered.len() as u64)?;
    reading.rule = denied
        .first()
        .map_or_else(String::new, |first| first.rule.clone());
    if let Act::Close(verdict, successors) = &act {
        reading.count("opens", successors.tickets().len() as u64)?;
        reading.count("withdrawn", u64::from(verdict.withdrawn().is_some()))?;
    }
    session_id
        .unwrap_or_default()
        .clone_into(&mut reading.session);
    let names = criteria.unticked_named();
    let boxes = format!(
        "{} of {} criteria ticked",
        criteria.ticked.len(),
        criteria.count()
    );
    // What a close a condition refuses left at the tracker, which is
    // nothing, said once for every condition that refuses one.
    let nothing = match act {
        Act::Read => "",
        Act::Close(..) => "; not closed, nothing changed at the tracker",
    };
    let (status, message) = match act {
        // A withdrawal is held to the boxes the other way round: one
        // ticked is work half done, and half-done work is not withdrawn.
        Act::Close(Verdict::Withdrawn(_), _) if !criteria.ticked.is_empty() => (
            Status::Fail,
            format!("{ticket}: {boxes}; {}{nothing}", half_done(&criteria)),
        ),
        Act::Close(Verdict::Withdrawn(_), _) if issue.state == State::Closed => (
            Status::Fail,
            format!("{ticket}: {boxes}; closed already, nothing changed at the tracker"),
        ),
        Act::Close(Verdict::Withdrawn(reason), successors) => {
            let comment = format!(
                "Withdrawn by `xtask ticket close` at {}: {reason}; {boxes}",
                reading.commit
            );
            match close(gh, root, track, &issue, &reading, &successors, &comment) {
                Ok((opened, closed)) => (
                    Status::Pass,
                    format!("{ticket}: {boxes}; withdrawn: {reason}; {opened}; {closed}"),
                ),
                Err(text) => (Status::Fail, format!("{ticket}: {boxes}; {text}")),
            }
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
        Act::Close(Verdict::Done, _) if issue.state == State::Closed => (
            Status::Fail,
            format!("{ticket}: {boxes}; closed already, nothing changed at the tracker"),
        ),
        Act::Close(Verdict::Done, successors) => {
            let comment = format!(
                "Closed by `xtask ticket close` at {}: {boxes}",
                reading.commit
            );
            match close(gh, root, track, &issue, &reading, &successors, &comment) {
                Ok((opened, closed)) => (
                    Status::Pass,
                    format!("{ticket}: {boxes}; {opened}; {closed}"),
                ),
                Err(text) => (Status::Fail, format!("{ticket}: {boxes}; {text}")),
            }
        }
    };
    reading.status = status;
    // A criterion an instrument writes is said beside whatever verdict
    // stands, since it is what the heuristic matched and not a refusal.
    reading.message = message + &steerable(&steered);
    Ok(reading)
}

/// The act at the tracker, the same for a close of work done and a
/// withdrawal: every successor held, the edges the tracker lacks set, and
/// the issue closed with `comment` and what it opened appended. What comes
/// back is what the reading says of the successors and of the close, or
/// the text of the refusal, which says what stands at the tracker.
fn close(
    gh: &Gh,
    root: &Path,
    track: &str,
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
        .held(gh, root, track)
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
}
