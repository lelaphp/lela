//! The frontier until it empties, one ticket at a time from start to
//! retire without a person (#77, #78; harness-design.md §4.4, §4.5, §5):
//! `xtask track work`. From inside a track's clone it
//! catches the track up with main first, [`track_sync::sync`] (#80), so
//! the harness the session runs under is no older than main's and the
//! worktree is laid down off a track that has main; then it
//! asks [`track_next::next`] for the ticket, lays the ticket's worktree
//! down with the branch `<track>/t<n>` recording it, as `worktree add
//! --ticket` does, and starts a headless Claude Code session in the
//! worktree on the model the clone records, with a prompt that names
//! the ticket and runs the skill its label chooses (#81): `/research
//! <n>` for a ticket labelled `frontier:research`, `/grill-design <n>`
//! for `frontier:decision`, and `/build <n>` for any other, as
//! [`Skill::of`] says; a ticket carrying both `frontier:` labels names
//! no one skill and is refused before any worktree is laid down, named.
//! The skill's line is the one thing the label changes: under it every
//! prompt names the exit condition this run reads back (#115), that the
//! session ends by running `cargo xtask ticket close <n>` and, when
//! something outside the ticket blocks that close, files the blocker as
//! a ticket and names it in its reply, so the contract travels with the
//! job rather than resting on a skill file elsewhere in the filesystem.
//! The hooks that already exist
//! police the session: `permit` judges each call and the Stop hook holds
//! the turn until `done` is green and the ticket's boxes are ticked, so
//! nothing here judges the work. When the session ends with the ticket
//! closed at the tracker, the track's branch is fast-forwarded to the
//! worktree's tip, pushed to `origin`, and the worktree retired, which
//! appends its readings into the track's store. When the session ends
//! with the ticket open, the worktree is left standing for a person, the
//! ticket's record kept with its branch, nothing retired, and the line
//! says so; the console shows the worktree.
//!
//! A worktree left standing is not left standing for good (#116). Its
//! branch records its ticket, so [`track_next::next`] names that ticket
//! no more, and finished work whose ticket a person closed afterwards
//! would rot beside a frontier that keeps growing. So a run begins by
//! sweeping the track's standing worktrees, [`sweep`]: a worktree whose
//! ticket has since closed and whose branch is ahead of the track is
//! fast-forwarded, pushed and retired, down the one path a close takes,
//! [`merge_back`]; one whose ticket is still open, or whose branch is
//! not ahead, is left standing and named, and the run goes on. Each is
//! one line, [`Swept::render`], and a sweep with nothing to merge back
//! prints none of them and moves no ref, so a run over a track with no
//! standing worktree reads as it did before.
//!
//! The sweep is the run's first act, before the catch-up with main and
//! so before the frontier is consulted, because a stranded branch
//! descends from the track as it stood when the branch was laid down:
//! catch the track up first and the merge main brings makes every such
//! branch no fast-forward, and every sweep would answer "not ahead"
//! forever. It is the run's act and not the step's, so a worktree this
//! run leaves standing is not swept again by the step after it; a ticket
//! closed while the run is going is the next run's to sweep. Two
//! stranded branches off one tip are not both fast-forwards: the first
//! moves the track and the second stands, named, since the track moves
//! by fast-forward alone and rebasing a person's branch is a person's.
//! A clone whose track is `main` sweeps nothing, since
//! every branch on no track of its own would answer to that name and a
//! worktree a person laid down is not this run's to retire.
//!
//! Without `--one` the step repeats while `track next` names a ticket
//! (#78): a ticket the session left open, or closed with the branch not
//! ahead of the track, is the session's failure and does not stop the
//! run, since its branch records it and the frontier names it no more,
//! and the run ends at "none" with one line per ticket and, once any was
//! worked, a closing line naming each with its outcome. A failure of the
//! harness itself stops the run and is named: a sync that refuses, a
//! tracker that does not answer, a build that does not stand, a session
//! program that does not start, a push refused, a retire that will not;
//! what was worked before it is named the same way in the error. `--one` runs the step once and
//! exits 1 when the ticket is left to a person.
//!
//! A track that records `track.frontier=auto` fills its own frontier
//! (#85): when `next` answers "none", a session is started on the prompt
//! `/frontier` in the clone itself, not a worktree, on the same model,
//! permission mode and effort as a ticket's, and `next` is asked again
//! once it ends. A frontier still empty then ends the run, `--one` or
//! not, with the line listing the track's open `for-a-person` tickets,
//! what the track waits on. Without the record the run ends at "none"
//! as before; a record that is not `auto` refuses before any session.
//! What bounds the run is the planner filing nothing: a planner that
//! keeps filing tickets whose sessions leave them standing plans again
//! at each "none", and a person reading the lines is what stops it.
//!
//! What the session is started with (#75): `claude -p --model
//! <track.model.session>`, the subagents' model handed to the child
//! through the setting Claude Code reads for it, the environment
//! variable [`SUBAGENT_VARIABLE`], and a permission mode and an effort
//! that are constants here, [`PERMISSION_MODE`] and [`EFFORT`], which
//! the clone's `track.permission-mode` and `track.effort` override only
//! when set; no track writes them today. The program itself is
//! `track.session.program` where the clone records one, so a test's
//! stand-in runs where `claude` would, and `claude` on `PATH` otherwise.
//! A clone that records no model refuses before any worktree is laid
//! down, and so does a repository with no `origin`, since the branch is
//! pushed to it.
//!
//! The track's branch moves only by fast-forward: a tip that is not
//! ahead of the track, the track's own commit included, refuses, with
//! the worktree standing. No trust line is printed and no grant waited
//! on: `computed` covers a worktree by its clone's grant (#74).
//!
//! What a run leaves behind is not stdout alone (#117). Every step is
//! held by [`hold`]: its verdict, [`Landed`], joins the run's [`Tally`]
//! and its own reading is written to the clone's store, scoped to the
//! ticket as a `ticket` reading is, a pass on a ticket landed and a fail
//! on a worktree standing, with the commit its branch is at and the
//! reason in the message. A sweep's step is held the same way, since
//! what it does to a worktree is what a step does to one. The run itself
//! writes one more, [`record_run`], scoped to [`RUN`]: how many tickets
//! it worked, how many landed and how it ended, written whatever the run
//! came to, so a run the harness stopped leaves the record of what it
//! worked rather than a terminal to reconstruct it from. `xtask status
//! --section track` renders the last of them. The worktree's own
//! readings still come in by the retire, as they did; these are the
//! run's, and their status is never the run's exit, which says whether
//! the harness stood and not whether a session landed its ticket. What
//! the session spent joins the step's reading (#210): the plugin records
//! a `usage` reading once a turn, under the ticket, and `hold` puts the
//! last one's `usd`, `tokens` and `turn` beside the commit, the run's
//! reading summing the `usd`; a ticket whose session recorded none, a
//! stand-in in a test, carries none and renders blank.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use anyhow::{Context, Result, bail};

use crate::git::git;
use crate::reading::{Reading, Status, Store};
use crate::tally::{Judged, Judgement, Outcome, Tally};
use crate::ticket::{self, Gh, STAMPED_OVER, State};
use crate::track::{self, Model, Ticket, Track, fast_forward, not_ahead, rev, short};
use crate::track_next::{self, Next};
use crate::track_sync;
use crate::usage::{Spent, Total};
use crate::worktree::{self, say};

/// The prefix of every line printed, so a person reads whose line it is.
const WORK: &str = "track work";
/// The instrument's name, the `instrument` field of every reading a run
/// writes (#117).
pub(crate) const INSTRUMENT: &str = "track-work";
/// The scope of the run's own reading; every other is a ticket's number,
/// the scope a `ticket` reading is written under.
pub(crate) const RUN: &str = "run";
/// How the closing reading of a run the harness stopped opens.
const STOPPED: &str = "stopped by the harness after";
/// How the closing reading of a `--one` run opens.
const ONE_STEP: &str = "the one step worked";

/// The permission mode a session runs under unless the clone records
/// `track.permission-mode`: `permit` judges every call against the
/// manifest through the hooks, and `auto` decides the rest without
/// asking, so nothing is left for a prompt no one answers and nothing
/// runs with the permission checks switched off.
pub(crate) const PERMISSION_MODE: &str = "auto";
/// The effort a session runs at unless the clone records `track.effort`.
pub(crate) const EFFORT: &str = "high";
/// The environment variable Claude Code reads the subagents' model from.
pub(crate) const SUBAGENT_VARIABLE: &str = "CLAUDE_CODE_SUBAGENT_MODEL";
/// The program a session is, where the clone records none.
const PROGRAM: &str = "claude";
/// The prompt of the session that fills the frontier (#85).
const FRONTIER_PROMPT: &str = "/frontier";
/// The clone's records that override the constants and the program.
const PROGRAM_KEY: &str = "track.session.program";
const MODE_KEY: &str = "track.permission-mode";
const EFFORT_KEY: &str = "track.effort";
/// The prefix of the label that says which side of the frontier a
/// ticket is on; the rest of the label is the side.
const FRONTIER_LABEL: &str = "frontier:";
const RESEARCH: &str = "research";
const DECISION: &str = "decision";

/// The skill a session starts on, chosen by the ticket's `frontier:`
/// label: a research ticket is read, a decision ticket is grilled, and
/// any other is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skill {
    /// `/build <n>`: a ticket with no `frontier:` label.
    Build,
    /// `/research <n>`: a ticket labelled `frontier:research`.
    Research,
    /// `/grill-design <n>`: a ticket labelled `frontier:decision`.
    GrillDesign,
}

impl Skill {
    /// The skill `ticket`'s labels choose, from the labels as the tracker
    /// spells them; both `frontier:` labels at once is a refusal that
    /// names the ticket and the labels, and a `frontier:` label naming
    /// neither side is one too, since a side no skill answers is not
    /// built by default.
    pub fn of<'a>(ticket: Ticket, labels: impl IntoIterator<Item = &'a str>) -> Result<Skill> {
        let sides = labels
            .into_iter()
            .filter_map(|label| label.strip_prefix(FRONTIER_LABEL))
            .collect::<Vec<_>>();
        match sides.as_slice() {
            [] => Ok(Skill::Build),
            [side] if *side == RESEARCH => Ok(Skill::Research),
            [side] if *side == DECISION => Ok(Skill::GrillDesign),
            [side] => bail!(
                "{ticket} is labelled {FRONTIER_LABEL}{side}, a side of the frontier no skill answers; `{FRONTIER_LABEL}{RESEARCH}` and `{FRONTIER_LABEL}{DECISION}` are the two, and nothing is laid down"
            ),
            many => bail!(
                "{ticket} is labelled {}, and a ticket is on one side of the frontier; a person takes one label off, and nothing is laid down",
                many.iter()
                    .map(|side| format!("{FRONTIER_LABEL}{side}"))
                    .collect::<Vec<_>>()
                    .join(" and ")
            ),
        }
    }

    /// The skill's name as the prompt invokes it.
    pub fn name(self) -> &'static str {
        match self {
            Skill::Build => "build",
            Skill::Research => "research",
            Skill::GrillDesign => "grill-design",
        }
    }
}

/// One headless session as it is started: the program, the model per
/// role, the permission mode and the effort.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    program: PathBuf,
    model: Model,
    permission_mode: String,
    effort: String,
}

impl Session {
    /// The session the clone at `root` starts on `model`: the constants,
    /// each overridden by its record where one is set.
    pub fn of(root: &Path, model: Model) -> Session {
        Session {
            program: track::config(root, PROGRAM_KEY)
                .map_or_else(|| PathBuf::from(PROGRAM), PathBuf::from),
            model,
            permission_mode: track::config(root, MODE_KEY)
                .unwrap_or_else(|| PERMISSION_MODE.to_owned()),
            effort: track::config(root, EFFORT_KEY).unwrap_or_else(|| EFFORT.to_owned()),
        }
    }

    /// The prompt: its first line names the ticket and runs the skill on
    /// it, `/<skill> <n>`, and the rest names the exit condition this run
    /// reads back, the same for every skill, since the one signal is the
    /// ticket closed at the tracker and a session that ends with it open
    /// leaves the worktree standing however finished its work is.
    pub fn prompt(skill: Skill, ticket: Ticket) -> String {
        let n = ticket.number();
        format!(
            "/{} {n}\n\n\
             The session ends by running `cargo xtask ticket close {n}`. That close is the one \
             signal this run reads back: a ticket still open at the tracker leaves the worktree \
             standing for a person, however finished the work is. The close holds the ticket to \
             its acceptance criteria, so tick each box the work meets first, and it takes \
             `--opens <m>` for every ticket this work filed, or `--opens-nothing`.\n\n\
             When something outside this ticket blocks the close, file that blocker with `gh \
             issue create`, name its number in your reply, and leave the ticket open.\n",
            skill.name()
        )
    }

    /// The arguments after the program, in the order they are passed,
    /// ending in `prompt`: a ticket's, [`Session::prompt`], or the
    /// frontier's, [`FRONTIER_PROMPT`].
    pub fn arguments(&self, prompt: &str) -> Vec<String> {
        vec![
            "-p".to_owned(),
            "--model".to_owned(),
            self.model.session.clone(),
            "--permission-mode".to_owned(),
            self.permission_mode.clone(),
            "--effort".to_owned(),
            self.effort.clone(),
            prompt.to_owned(),
        ]
    }

    /// The line saying what is started where: the program and its
    /// arguments, the directory and the subagents' model.
    fn announce(&self, dir: &Path, prompt: &str) {
        say(
            WORK,
            &format!(
                "session: {} {} in {}, subagents on {}",
                self.program.display(),
                self.arguments(prompt).join(" "),
                dir.display(),
                self.model.subagent()
            ),
        );
    }

    /// The session run in `dir` on `prompt`, its streams the caller's,
    /// its exit when it ends, which is said and not judged: whether the
    /// ticket closed at the tracker is the one signal, since the hooks
    /// judged the session's turns; a program that does not start is an
    /// error.
    fn run(&self, dir: &Path, prompt: &str) -> Result<ExitStatus> {
        Command::new(&self.program)
            .args(self.arguments(prompt))
            .env(SUBAGENT_VARIABLE, self.model.subagent())
            .current_dir(dir)
            .stdin(Stdio::null())
            .status()
            .with_context(|| {
                format!(
                    "the session did not start: {} could not be run",
                    self.program.display()
                )
            })
    }
}

/// What one step did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Worked {
    /// The frontier of the track is empty; nothing was laid down.
    Nothing { track: String },
    /// The frontier is still empty after a `/frontier` session (#85);
    /// nothing was laid down, and `waits_on` is the track's open
    /// `for-a-person` tickets.
    StillEmpty { track: String, waits_on: Vec<Next> },
    /// The ticket closed: the track's branch is at `commit`, pushed, and
    /// the worktree on `branch` retired.
    Closed {
        ticket: Ticket,
        branch: String,
        commit: String,
        track: String,
    },
    /// The session ended and the ticket is the session's to finish, `why`:
    /// open, or closed with the branch not ahead of the track. The
    /// worktree at `dir` stands for a person with the branch at `commit`,
    /// nothing retired.
    Standing {
        ticket: Ticket,
        commit: String,
        dir: PathBuf,
        why: String,
    },
}

impl Worked {
    /// The one line: the prefix and what the outcome says, and under a
    /// frontier still empty the tickets the track waits on.
    pub fn render(&self) -> String {
        let head = format!("{WORK}: {}\n", self.said());
        match self {
            Worked::StillEmpty { waits_on, .. } => std::iter::once(head)
                .chain(
                    waits_on
                        .iter()
                        .map(|ticket| format!("  #{} {}\n", ticket.number, ticket.title)),
                )
                .collect(),
            _ => head,
        }
    }

    /// What the line says after the prefix: the ticket, the commit and
    /// the outcome, in one sentence, which is also the message of the
    /// step's reading (#117), so the terminal and the store say the same
    /// thing in the same words.
    fn said(&self) -> String {
        match self {
            Worked::Nothing { track } => {
                format!("none: the frontier of {track} is empty; nothing laid down")
            }
            Worked::StillEmpty { track, waits_on } => {
                let waits = match waits_on.len() {
                    0 => "no for-a-person ticket".to_owned(),
                    1 => "1 for-a-person ticket:".to_owned(),
                    n => format!("{n} for-a-person tickets:"),
                };
                format!(
                    "none: the frontier of {track} is still empty after the {FRONTIER_PROMPT} session; nothing laid down; the track waits on {waits}"
                )
            }
            Worked::Closed {
                ticket,
                branch,
                commit,
                track,
            } => format!(
                "{ticket} closed at {}; {track} fast-forwarded to it and pushed to origin, {branch} retired",
                short(commit)
            ),
            Worked::Standing {
                ticket,
                commit,
                dir,
                why,
            } => format!(
                "{ticket} {why}, the branch at {}; the worktree stands at {} for a person, nothing retired",
                short(commit),
                dir.display()
            ),
        }
    }

    /// The step as `hold` reads it: its verdict, which the tally holds and
    /// the step's reading is written from, and where its worktree stands
    /// when it does; an empty frontier worked no ticket and has none.
    fn step(&self) -> Option<Step> {
        match self {
            Worked::Nothing { .. } | Worked::StillEmpty { .. } => None,
            Worked::Closed { ticket, commit, .. } => Some(Step {
                judged: verdict(*ticket, commit, Landed::Closed, self.said()),
                standing_at: None,
            }),
            Worked::Standing {
                ticket,
                commit,
                dir,
                ..
            } => Some(Step {
                judged: verdict(*ticket, commit, Landed::Standing, self.said()),
                standing_at: Some(dir.clone()),
            }),
        }
    }
}

/// One step as [`hold`] reads it: the verdict, and the worktree that
/// stands when the ticket was left standing, whose own store still holds
/// what its session spent (#210); a retired worktree's readings are in
/// the track's log already.
struct Step {
    judged: Judged<Landed>,
    standing_at: Option<PathBuf>,
}

/// What one step of a run came to, the two outcomes a ticket has: the
/// judgement the run's [`Tally`] holds and the step's reading takes its
/// status from (#117).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Landed {
    /// The ticket closed and its branch is on the track.
    Closed,
    /// The worktree stands for a person, and the ticket is the session's
    /// to have finished.
    Standing,
}

impl Judgement for Landed {
    type Miss = std::convert::Infallible;
    const PASS: Landed = Landed::Closed;

    fn as_str(self) -> &'static str {
        match self {
            Landed::Closed => "closed",
            Landed::Standing => "standing",
        }
    }

    /// A worktree standing is the session's failure, and the run's
    /// reading says so; what it is not is the run's exit.
    fn counts_against(self) -> bool {
        self == Landed::Standing
    }

    fn met(self) -> bool {
        self == Landed::Closed
    }
}

/// One step's verdict: the ticket's number as the scope its reading is
/// written under and the ticket as the closing line names it, the commit
/// its branch is at where the verdict places the item, and `said`, which
/// is the line's words and the reading's message.
fn verdict(ticket: Ticket, commit: &str, landed: Landed, said: String) -> Judged<Landed> {
    let mut judged = Judged::new(&ticket::scope(ticket), &ticket.to_string(), landed, said);
    short(commit).clone_into(&mut judged.place);
    judged
}

/// What the sweep did to one standing worktree of the track (#116): the
/// same three facts a [`Worked`] carries, the ticket, the commit and the
/// reason, said the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Swept {
    /// The ticket closed since its session ended and the branch was ahead:
    /// the track is at `commit`, pushed, and the worktree on `branch`
    /// retired.
    MergedBack {
        ticket: Ticket,
        branch: String,
        commit: String,
        track: String,
    },
    /// The worktree at `dir` stands on, `why`: its ticket is still open,
    /// or it closed with the branch not ahead of the track.
    Stands {
        ticket: Ticket,
        commit: String,
        dir: PathBuf,
        why: String,
    },
}

impl Swept {
    /// The one line: the ticket, the commit and the outcome.
    pub fn render(&self) -> String {
        format!("{WORK}: {}\n", self.said())
    }

    /// What the line says after the prefix, and the message of the step's
    /// reading (#117), as a [`Worked`]'s is.
    fn said(&self) -> String {
        match self {
            Swept::MergedBack {
                ticket,
                branch,
                commit,
                track,
            } => format!(
                "swept {ticket}, closed since its session at {}; {track} fast-forwarded to it and pushed to origin, {branch} retired",
                short(commit)
            ),
            Swept::Stands {
                ticket,
                commit,
                dir,
                why,
            } => format!(
                "{ticket} {why}, the branch at {}; the worktree stands at {} for a person, nothing retired",
                short(commit),
                dir.display()
            ),
        }
    }

    /// The step: what the sweep did to one worktree is what a step does
    /// to one, so it is judged and read back the same way.
    fn step(&self) -> Step {
        match self {
            Swept::MergedBack { ticket, commit, .. } => Step {
                judged: verdict(*ticket, commit, Landed::Closed, self.said()),
                standing_at: None,
            },
            Swept::Stands {
                ticket,
                commit,
                dir,
                ..
            } => Step {
                judged: verdict(*ticket, commit, Landed::Standing, self.said()),
                standing_at: Some(dir.clone()),
            },
        }
    }
}

/// The closing line of a run, and the message of its reading: `how` it
/// ended, how many tickets it worked and each with its outcome.
fn outcomes(tally: &Tally<Landed>, how: &str) -> String {
    let briefs = tally
        .judged
        .iter()
        .map(|judged| format!("{} {}", judged.name, judged.judgement.as_str()))
        .collect::<Vec<_>>();
    match briefs.len() {
        0 => format!("{how} no ticket"),
        1 => format!("{how} 1 ticket: {}", briefs[0]),
        n => format!("{how} {n} tickets: {}", briefs.join(", ")),
    }
}

/// One step held (#117): its verdict kept for the run's closing reading,
/// and its own reading written under the ticket's number, a pass on a
/// ticket landed on the track and a fail on a worktree left standing,
/// carrying the commit its branch is at, the reason, and what the
/// session spent (#210): the `usd`, `tokens` and `turn` of the last
/// `usage` reading under the ticket's scope, read from the standing
/// worktree's store where one stands and from the track's log otherwise,
/// which is where a retire put it; a ticket with none carries none. The
/// reading is stamped over the workspace, which is what a ticket's is
/// stamped over, since a step has no tree of its own. The status written
/// is never the run's exit: a ticket left standing is the session's
/// failure and does not stop the run. A step that worked no ticket has no
/// verdict and writes nothing.
fn hold(
    root: &Path,
    store: &Store,
    tally: &mut Tally<Landed>,
    total: &mut Total,
    step: Option<Step>,
) -> Result<()> {
    let Some(Step {
        judged,
        standing_at,
    }) = step
    else {
        return Ok(());
    };
    let mut reading = Reading::stamped_over(INSTRUMENT, &judged.id, root, Path::new(STAMPED_OVER))?;
    reading.status = match judged.judgement {
        Landed::Closed => Status::Pass,
        Landed::Standing => Status::Fail,
    };
    judged.place.clone_into(&mut reading.commit);
    reading.message.clone_from(&judged.note);
    let own = standing_at.as_deref().map(Store::of_workspace);
    let stores: Vec<&Store> = own.iter().chain(std::iter::once(store)).collect();
    let spent = Spent::last(&stores, &judged.id)?;
    if let Some(spent) = spent {
        spent.carry(&mut reading);
    }
    total.add(spent);
    store.record(std::slice::from_ref(&reading))?;
    tally.found += 1;
    tally.judged.push(judged);
    Ok(())
}

/// The run's own reading, scoped to [`RUN`]: how many tickets it worked,
/// how many landed and the `usd` its sessions spent together in its
/// numbers, `how` it ended and each ticket's outcome in its message. A
/// run that landed nothing is a skip and not a pass, the tally's rung for
/// a run that measured nothing; a run that left a worktree standing
/// fails. Written whatever the run came to, so a run the harness stopped
/// leaves the record of what it worked.
fn record_run(
    root: &Path,
    store: &Store,
    tally: &Tally<Landed>,
    total: Total,
    how: &str,
) -> Result<()> {
    let standing = tally.against().len() as u64;
    let outcome = match (standing, tally.met()) {
        (0, 0) => Outcome::NothingRan,
        (0, _) => Outcome::Passed,
        _ => Outcome::Failed,
    };
    let mut reading = tally.reading(
        Reading::stamped_over(INSTRUMENT, RUN, root, Path::new(STAMPED_OVER))?,
        &outcome,
        outcomes(tally, how),
    )?;
    reading.count("landed", tally.met())?;
    reading.count("standing", standing)?;
    total.carry(&mut reading);
    store.record(std::slice::from_ref(&reading))?;
    Ok(())
}

/// One ticket, start to retire: the track caught up with main, its next
/// ticket, its worktree, the session, and on a close the fast-forward,
/// the push and the retire. A session that ends with the ticket open, or
/// closed with the branch not ahead of the track, is an outcome naming
/// the worktree that stands; a failure of the harness is the error.
pub fn work(root: &Path, gh: &Gh) -> Result<Worked> {
    let track = Track::of(root, None)?;
    let Some(model) = track.model.clone() else {
        bail!(
            "the clone records no model, so no session is started and nothing is laid down; `git config {} <name>` records one",
            "track.model.session"
        );
    };
    let session = Session::of(root, model);
    let fills_own = track::fills_own_frontier(root).context(
        "the frontier record is not read, so no session is started and nothing is laid down",
    )?;
    git(root, &["remote", "get-url", "origin"], None)
        .context("the repository has no origin, and the track's branch is pushed to it; nothing is laid down")?;
    let synced = track_sync::sync(root)
        .context("the track was not caught up with main, so nothing is laid down")?;
    print!("{}", synced.render());
    // Read again: the sync moved the track, and its base with it.
    let track = Track::of(root, None)?;
    let next = match track_next::next(root, gh, &track.name)? {
        Some(next) => next,
        None if fills_own => match fill(root, gh, &session, &track.name)? {
            Some(next) => next,
            None => {
                return Ok(Worked::StillEmpty {
                    waits_on: track_next::for_a_person(root, gh, &track.name)?,
                    track: track.name,
                });
            }
        },
        None => return Ok(Worked::Nothing { track: track.name }),
    };
    let ticket = Ticket::new(next.number);
    let skill = Skill::of(ticket, next.labels.iter().map(String::as_str))?;
    let branch = format!("{}/t{}", track.name, next.number);
    say(WORK, &format!("{ticket} {}: on {branch}", next.title));

    let dir = worktree::add(root, &branch, Some(ticket))?;
    let prompt = Session::prompt(skill, ticket);
    session.announce(&dir, &prompt);
    let status = session.run(&dir, &prompt)?;
    say(WORK, &format!("the session ended: {status}"));
    let tip = rev(root, &format!("refs/heads/{branch}"))?;
    let issue = gh.view(root, ticket)?;
    if issue.state != State::Closed {
        return Ok(Worked::Standing {
            ticket,
            commit: tip,
            dir,
            why: format!("is open after the session ({status})"),
        });
    }
    match merge_back(root, &track, ticket, &branch, &tip, &dir)? {
        Some(why) => Ok(Worked::Standing {
            ticket,
            commit: tip,
            dir,
            why: format!("closed, but the branch is not ahead of the track: {why}"),
        }),
        None => Ok(Worked::Closed {
            ticket,
            branch,
            commit: tip,
            track: track.name,
        }),
    }
}

/// The branch of a closed ticket merged back into its track: the
/// fast-forward, the push to `origin` and the retire, in that order, or
/// why `tip` is not ahead of the track, which merges nothing back and
/// leaves the worktree at `dir` standing. The one path a close takes,
/// whether the session that did the work has just ended or ended a run
/// ago and [`sweep`] found it (#116); a fast-forward or a retire that
/// refuses is a failure of the harness, named against the worktree.
fn merge_back(
    root: &Path,
    track: &Track,
    ticket: Ticket,
    branch: &str,
    tip: &str,
    dir: &Path,
) -> Result<Option<String>> {
    if let Some(why) = not_ahead(root, track, tip)? {
        return Ok(Some(why));
    }
    fast_forward(WORK, root, track, tip).with_context(|| {
        format!(
            "{ticket} closed, but the track did not move to {}; the worktree stands at {}, nothing retired",
            short(tip),
            dir.display()
        )
    })?;
    worktree::retire(root, branch).with_context(|| {
        format!(
            "{ticket} closed and {} pushed at {}, but the worktree stands at {}; retire it by hand once it is clean",
            track.name,
            short(tip),
            dir.display()
        )
    })?;
    Ok(None)
}

/// The track's standing worktrees swept (#116): each one [`standing`]
/// names, oldest ticket first, its ticket read from the tracker, and the
/// branch of a closed one merged back by [`merge_back`]. The track is
/// read again at each worktree, since a merge back moves it, which is
/// also why the second of two branches off one tip stands: it no longer
/// descends from the track the first moved. An empty answer is a sweep
/// that moved no ref and has nothing to say.
pub fn sweep(root: &Path, gh: &Gh) -> Result<Vec<Swept>> {
    let mut swept = Vec::new();
    for (dir, branch, ticket) in standing(root, &Track::of(root, None)?)? {
        let track = Track::of(root, None)?;
        let tip = rev(root, &format!("refs/heads/{branch}"))?;
        let issue = gh.view(root, ticket)?;
        if issue.state != State::Closed {
            swept.push(Swept::Stands {
                ticket,
                commit: tip,
                dir,
                why: "is still open at the tracker".to_owned(),
            });
            continue;
        }
        swept.push(
            match merge_back(root, &track, ticket, &branch, &tip, &dir)? {
                Some(why) => Swept::Stands {
                    ticket,
                    commit: tip,
                    dir,
                    why: format!("closed, but the branch is not ahead of the track: {why}"),
                },
                None => Swept::MergedBack {
                    ticket,
                    branch,
                    commit: tip,
                    track: track.name,
                },
            },
        );
    }
    Ok(swept)
}

/// The worktrees a sweep looks at: every linked worktree of the clone,
/// the main one left out, that is on a branch of `track` recording a
/// ticket, by ticket number, which is the tracker's order of filing. A
/// branch that records none was laid down for no ticket and is a
/// person's, and so is every worktree of a clone whose track is
/// `main`, where a branch on no track of its own answers
/// to the name and none of them is this run's. A worktree whose own
/// track cannot be said is left alone rather than swept blind.
fn standing(root: &Path, track: &Track) -> Result<Vec<(PathBuf, String, Ticket)>> {
    if track.name == track::MAIN {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    for (dir, on) in track::worktrees(&track.store)? {
        let Some(branch) = on else { continue };
        if dir == track.store {
            continue;
        }
        let Ok(its) = Track::of(root, Some(&branch)) else {
            continue;
        };
        if its.name != track.name {
            continue;
        }
        if let Some(ticket) = its.ticket {
            found.push((dir, branch, ticket));
        }
    }
    found.sort_by_key(|(_, _, ticket)| ticket.number());
    Ok(found)
}

/// The frontier of `track` filled by a `/frontier` session in the clone
/// (#85), and the ticket `next` names once it ends, `None` for a
/// frontier still empty; the session's exit is said, not judged, as a
/// ticket's is.
fn fill(root: &Path, gh: &Gh, session: &Session, track: &str) -> Result<Option<Next>> {
    say(WORK, &format!("the frontier of {track} is empty"));
    session.announce(root, FRONTIER_PROMPT);
    let status = session.run(root, FRONTIER_PROMPT)?;
    say(
        WORK,
        &format!("the {FRONTIER_PROMPT} session ended: {status}"),
    );
    track_next::next(root, gh, track)
}

/// How a run ended: the words its closing reading opens with, and the
/// refusal a `--one` step whose ticket was left to a person ends on,
/// which is the exit and not a failure of the harness.
struct Ended {
    how: String,
    refused: Option<String>,
}

/// `xtask track work`: the tracker on `PATH`, the frontier worked until
/// it empties and the closing line printed, or with `one` a single step whose
/// ticket left open is the exit 1. Every step's reading and the run's own
/// are written whatever the run came to (#117), and the run's error, where
/// it has one, is the error it ends on.
pub fn command(root: &Path, one: bool) -> Result<()> {
    let store = Store::of_workspace(root);
    let mut tally = Tally::default();
    let mut total = Total::default();
    let ended = run(root, &store, &mut tally, &mut total, one);
    let how = match &ended {
        Ok(ended) => ended.how.clone(),
        Err(_) => STOPPED.to_owned(),
    };
    let written = record_run(root, &store, &tally, total, &how);
    let ended = ended?;
    written?;
    match ended.refused {
        Some(line) => bail!("{line}"),
        None => Ok(()),
    }
}

/// The run itself: the sweep, then one step or the frontier until it
/// empties, each held by [`hold`] as it is reached, so a run the harness
/// stops keeps the steps it took.
fn run(
    root: &Path,
    store: &Store,
    tally: &mut Tally<Landed>,
    total: &mut Total,
    one: bool,
) -> Result<Ended> {
    let gh = Gh::on_path();
    for outcome in sweep(root, &gh)
        .context("the track's standing worktrees were not swept, so nothing is laid down")?
    {
        print!("{}", outcome.render());
        hold(root, store, tally, total, Some(outcome.step()))?;
    }
    if one {
        let outcome = work(root, &gh)?;
        hold(root, store, tally, total, outcome.step())?;
        let refused = if let Worked::Standing { .. } = outcome {
            Some(outcome.render().trim_end().to_owned())
        } else {
            print!("{}", outcome.render());
            None
        };
        return Ok(Ended {
            how: ONE_STEP.to_owned(),
            refused,
        });
    }
    loop {
        let outcome =
            work(root, &gh).with_context(|| format!("{WORK}: {}", outcomes(tally, STOPPED)))?;
        print!("{}", outcome.render());
        let empty = match &outcome {
            Worked::Nothing { track } | Worked::StillEmpty { track, .. } => {
                Some(format!("the frontier of {track} is empty after"))
            }
            _ => None,
        };
        hold(root, store, tally, total, outcome.step())?;
        if let Some(how) = empty {
            if !tally.judged.is_empty() {
                say(WORK, &outcomes(tally, &how));
            }
            return Ok(Ended { how, refused: None });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No `frontier:` label builds; one names its side; two is a refusal
    /// naming both, and a side no skill answers is one naming it.
    #[test]
    fn the_frontier_label_chooses_the_skill() {
        let ticket = Ticket::new(20);
        assert_eq!(
            Skill::of(ticket, ["ready-for-agent", "track:main"]).unwrap(),
            Skill::Build
        );
        assert_eq!(
            Skill::of(ticket, ["ready-for-agent", "frontier:research"]).unwrap(),
            Skill::Research
        );
        assert_eq!(
            Skill::of(ticket, ["frontier:decision"]).unwrap(),
            Skill::GrillDesign
        );
        let both = Skill::of(ticket, ["frontier:research", "frontier:decision"]).unwrap_err();
        assert!(
            both.to_string()
                .contains("#20 is labelled frontier:research and frontier:decision"),
            "{both}"
        );
        let other = Skill::of(ticket, ["frontier:build"]).unwrap_err();
        assert!(
            other
                .to_string()
                .contains("frontier:build, a side of the frontier no skill answers"),
            "{other}"
        );
    }

    /// Every skill's prompt opens on its own line, `/<skill> <n>`, and
    /// every one of them names the close that ends the session and the
    /// filed ticket that stands in for it when it cannot.
    #[test]
    fn the_prompt_names_the_skill_the_ticket_and_the_close() {
        let ticket = Ticket::new(20);
        for (skill, line) in [
            (Skill::Build, "/build 20"),
            (Skill::Research, "/research 20"),
            (Skill::GrillDesign, "/grill-design 20"),
        ] {
            let prompt = Session::prompt(skill, ticket);
            assert_eq!(prompt.lines().next(), Some(line), "{prompt}");
            assert!(
                prompt.contains("cargo xtask ticket close 20"),
                "{skill:?} names no close: {prompt}"
            );
            assert!(
                prompt.contains("--opens-nothing"),
                "{skill:?} names neither successor: {prompt}"
            );
            assert!(
                prompt.contains("gh issue create"),
                "{skill:?} says nothing of a blocker: {prompt}"
            );
        }
    }
}
