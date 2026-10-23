//! The frontier until it empties, one ticket at a time from start to
//! retire without a person (#77, #78; harness-design.md §4.4, §4.5, §5):
//! `xtask track work`. From main's checkout it asks
//! [`track_next::next`] for the ticket of the track `--track` names, a
//! `track:<name>` label and `main` by default (ADR-0009), lays the
//! ticket's worktree down on the branch `t<n>`, cut from `main` and
//! recording the ticket, as `worktree add --ticket` does, and starts a
//! headless Claude Code session in the worktree on the model `--model`
//! names, with a prompt that names the ticket and runs the skill its
//! label chooses (#81): `/research <n>` for a ticket labelled
//! `frontier:research`, `/grill-design <n>` for `frontier:decision`, and
//! `/build <n>` for any other, as [`Skill::of`] says; a ticket carrying
//! both `frontier:` labels names no one skill and is refused before any
//! worktree is laid down, named.
//! The skill's line is the one thing the label changes: under it every
//! prompt names the exit condition this run reads back (#115), that the
//! session ends by running `cargo xtask ticket ready <n>` (#2311) and,
//! when something outside the ticket blocks that record, files the
//! blocker as a ticket and names it in its reply, so the contract travels
//! with the job rather than resting on a skill file elsewhere in the
//! filesystem.
//! The hooks that already exist police the session: `permit` judges each
//! call and the Stop hook holds the turn until `done` is green and the
//! ticket's boxes are ticked, so nothing here judges the work. When the
//! session ends with the branch's ready record written, the worktree is
//! retired, which appends its readings into the checkout's store, and the
//! branch `t<n>` is left with its records for the /specs coordinator to
//! merge through its merge check (#1749, ADR-0009), whose merge commit
//! closes the ticket (#2270, #2311): the loop never moves `main` and
//! pushes nothing, and a ticket the ready one blocks stays blocked until
//! the merge closes it. The ready record is the one signal, beside a
//! ticket withdrawn at the tracker with nothing committed, which is
//! retired the same way. When the session ends with neither, the
//! worktree is left standing for a person, the ticket's record kept with its branch, nothing retired, and
//! the line says so; the console shows the worktree. A worktree left
//! standing is not the loop's again, whatever becomes of its ticket: a
//! run lays down, and retires, only the worktree its own session ran in,
//! and a standing one is retired by /specs or a person with `xtask
//! worktree retire` once its branch is merged or waits to be (#1755).
//!
//! Without `--one` the step repeats while `track next` names a ticket
//! (#78): a ticket the session left open is the session's failure and
//! does not stop the run, since its branch records it and the frontier
//! names it no more, and the run ends at "none" with one line per ticket
//! and, once any was worked, a closing line naming each with its outcome.
//! A failure of the harness itself stops the run and is named: a tracker
//! that does not answer, a build that does not stand, a session program
//! that does not start, a retire that will not; what was worked before it
//! is named the same way in the error. `--one` runs the step once and
//! exits 1 when the ticket is left to a person.
//!
//! With `--fill` the run fills its own frontier (#85): when the frontier
//! is empty, a session is started on the prompt
//! `/frontier` in the checkout itself, not a worktree, on the same model,
//! permission mode and effort as a ticket's, and `next` is asked again
//! once it ends. A frontier still empty then ends the run, `--one` or
//! not, with the line listing the track's open `for-a-person` tickets,
//! what the track waits on. Without `--fill` the run ends at "none". What
//! bounds the run is the planner filing nothing: a planner that keeps
//! filing tickets whose sessions leave them standing plans again at each
//! "none", and a person reading the lines is what stops it.
//!
//! What the session is started with, every part a flag of the run and
//! none a record of the checkout ([`Session`]): `claude -p --model
//! <model>` from `PATH`, the subagents' model, `--subagent-model` or the
//! session's, handed to the child through the setting Claude Code reads
//! for it, the environment variable [`SUBAGENT_VARIABLE`], and the
//! permission mode and the effort, whose defaults are [`PERMISSION_MODE`]
//! and [`EFFORT`]. No trust line is waited on: `computed` covers a
//! worktree by the grant over the worktrees' directory (#74).
//!
//! What a run leaves behind is not stdout alone (#117). Every step is
//! held by [`hold`]: its verdict, [`Landed`], joins the run's [`Tally`]
//! and its own reading is written to the checkout's store, scoped to the
//! ticket as a `ticket` reading is, a pass on a ticket landed and a fail
//! on a worktree standing, with the commit its branch is at and the
//! reason in the message. The run itself
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

use crate::key::Snapshot;
use crate::reading::{Reading, Status, Store, joined};
use crate::tally::{Judged, Judgement, Outcome, Tally};
use crate::ticket::{self, Gh, State};
use crate::track::{Ticket, rev, short};
use crate::track_next::{self, Next};
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

/// The permission mode a session runs under unless `--permission-mode`
/// names another: `permit` judges every call against the manifest
/// through the hooks, and `auto` decides the rest without asking, so
/// nothing is left for a prompt no one answers and nothing runs with the
/// permission checks switched off.
pub(crate) const PERMISSION_MODE: &str = "auto";
/// The effort a session runs at unless `--effort` names another.
pub(crate) const EFFORT: &str = "high";
/// The environment variable Claude Code reads the subagents' model from.
pub(crate) const SUBAGENT_VARIABLE: &str = "CLAUDE_CODE_SUBAGENT_MODEL";
/// The program a session is, found on `PATH`.
const PROGRAM: &str = "claude";
/// The prompt of the session that fills the frontier (#85).
const FRONTIER_PROMPT: &str = "/frontier";
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
                joined(
                    many.iter().map(|side| format!("{FRONTIER_LABEL}{side}")),
                    " and "
                )
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

/// One headless session as it is started, every part a flag of `track
/// work` (ADR-0009): the model per role, the permission mode and the
/// effort. The program is [`PROGRAM`] on `PATH`.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct Session {
    /// The model the sessions run on.
    #[arg(long, value_name = "NAME")]
    pub model: String,
    /// The model the sessions' subagents run on; the session's when not
    /// given.
    #[arg(long, value_name = "NAME")]
    pub subagent_model: Option<String>,
    /// The sessions' permission mode.
    #[arg(long, value_name = "MODE", default_value = PERMISSION_MODE)]
    pub permission_mode: String,
    /// The sessions' effort.
    #[arg(long, value_name = "LEVEL", default_value = EFFORT)]
    pub effort: String,
}

impl Session {
    /// The prompt: its first line names the ticket and runs the skill on
    /// it, `/<skill> <n>`, and the rest names the exit condition this run
    /// reads back, the same for every skill, since the one signal is the
    /// branch's ready record, or the ticket withdrawn at the tracker, and a
    /// session that ends with neither leaves the worktree standing however
    /// finished its work is (#2311).
    pub fn prompt(skill: Skill, ticket: Ticket) -> String {
        let n = ticket.number();
        format!(
            "/{} {n}\n\n\
             The session ends by running `cargo xtask ticket ready {n}`. That record is the one \
             signal this run reads back: a branch with no ready record leaves the worktree \
             standing for a person, however finished the work is, and the merge into main \
             closes the ticket (#2270). The record holds the ticket to its acceptance criteria, \
             so tick each box the work meets first, and it takes `--opens <m>` for every ticket \
             this work filed, or `--opens-nothing`. A ticket that describes nothing in the tree \
             is withdrawn instead, `cargo xtask ticket close {n} --withdrawn <reason>`.\n\n\
             When something outside this ticket blocks the record, file that blocker with `gh \
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
            self.model.clone(),
            "--permission-mode".to_owned(),
            self.permission_mode.clone(),
            "--effort".to_owned(),
            self.effort.clone(),
            prompt.to_owned(),
        ]
    }

    /// The subagents' model: the one named, or the session's.
    fn subagent(&self) -> &str {
        self.subagent_model.as_deref().unwrap_or(&self.model)
    }

    /// The line saying what is started where: the program and its
    /// arguments, the directory and the subagents' model.
    fn announce(&self, dir: &Path, prompt: &str) {
        say!(
            WORK,
            "session: {PROGRAM} {} in {}, subagents on {}",
            self.arguments(prompt).join(" "),
            dir.display(),
            self.subagent()
        );
    }

    /// The session run in `dir` on `prompt`, its streams the caller's,
    /// its exit when it ends, which is said and not judged: whether the
    /// ticket closed at the tracker is the one signal, since the hooks
    /// judged the session's turns; a program that does not start is an
    /// error.
    fn run(&self, dir: &Path, prompt: &str) -> Result<ExitStatus> {
        Command::new(PROGRAM)
            .args(self.arguments(prompt))
            .env(SUBAGENT_VARIABLE, self.subagent())
            .current_dir(dir)
            .stdin(Stdio::null())
            .status()
            .with_context(|| format!("the session did not start: {PROGRAM} could not be run"))
    }
}

/// What a run works and how, every part a flag of `track work`
/// (ADR-0009): the track whose frontier it works, the session each
/// ticket is started on, and whether an empty frontier starts a
/// `/frontier` session.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct Run {
    /// The track whose frontier is worked, by its `track:` label.
    #[arg(long, value_name = "NAME", default_value = crate::track::MAIN)]
    pub track: String,
    #[command(flatten)]
    pub session: Session,
    /// Start a `/frontier` session when the frontier is empty (#85).
    #[arg(long)]
    pub fill: bool,
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
    /// The ticket ready to merge, or withdrawn: the worktree on `branch`
    /// retired, and `branch`, at `commit`, left for /specs to merge
    /// (#1749), whose merge closes a ready ticket (#2311).
    Closed {
        ticket: Ticket,
        branch: String,
        commit: String,
        ready: bool,
    },
    /// The session ended with the ticket open, `why`: the ticket is the
    /// session's to finish. The worktree at `dir` stands for a person with
    /// the branch at `commit`, nothing retired.
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
        let under: Vec<String> = match self {
            Worked::StillEmpty { waits_on, .. } => waits_on
                .iter()
                .map(|ticket| format!("  #{} {}\n", ticket.number, ticket.title))
                .collect(),
            _ => Vec::new(),
        };
        std::iter::once(head).chain(under).collect()
    }

    /// How a run that ends on this step ended, the words its closing
    /// line and reading open with, where the step is an empty frontier.
    fn ended(&self) -> Option<String> {
        match self {
            Worked::Nothing { track } | Worked::StillEmpty { track, .. } => {
                Some(format!("the frontier of {track} is empty after"))
            }
            Worked::Closed { .. } | Worked::Standing { .. } => None,
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
                ready,
            } => {
                let (at, closes) = if *ready {
                    ("ready", ", and the merge closes it")
                } else {
                    ("closed", "")
                };
                format!(
                    "{ticket} {at} at {}; {branch} is left for /specs to merge, its worktree retired{closes}",
                    short(commit)
                )
            }
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
/// the checkout's log already.
struct Step {
    judged: Judged<Landed>,
    standing_at: Option<PathBuf>,
}

/// What one step of a run came to, the two outcomes a ticket has: the
/// judgement the run's [`Tally`] holds and the step's reading takes its
/// status from (#117).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Landed {
    /// The ticket closed, its worktree retired and its branch left for
    /// /specs.
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
/// ticket landed and a fail on a worktree left standing,
/// carrying the commit its branch is at, the reason, and what the
/// session spent (#210): the `usd`, `tokens` and `turn` of the last
/// `usage` reading under the ticket's scope, read from the standing
/// worktree's store where one stands and from the checkout's log otherwise,
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
    // After the session landed what it landed: the stamp is over the tree
    // it left.
    let mut reading = stamped(root, &judged.id)?;
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

/// A reading of the instrument over `scope`, stamped over the tree at
/// `root` as it stands now.
fn stamped(root: &Path, scope: &str) -> Result<Reading> {
    ticket::stamped_over(INSTRUMENT, scope, &Snapshot::disk(root)?)
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
    let mut reading = tally.reading(stamped(root, RUN)?, &outcome, outcomes(tally, how))?;
    reading.count("landed", tally.met())?;
    reading.count("standing", standing)?;
    total.carry(&mut reading);
    store.record(std::slice::from_ref(&reading))?;
    Ok(())
}

/// One ticket, start to retire: the track's next ticket, its worktree on
/// `t<n>` off `main`, the session, and on a close the retire, the branch
/// left for /specs. A session that ends with the ticket open is an
/// outcome naming the worktree that stands; a failure of the harness is
/// the error.
pub fn work(root: &Path, gh: &Gh, run: &Run) -> Result<Worked> {
    let track = run.track.clone();
    let next = match track_next::next(root, gh, &track)? {
        Some(next) => next,
        None if run.fill => match fill(root, gh, &run.session, &track)? {
            Some(next) => next,
            None => {
                return Ok(Worked::StillEmpty {
                    waits_on: track_next::for_a_person(root, gh, &track)?,
                    track,
                });
            }
        },
        None => return Ok(Worked::Nothing { track }),
    };
    let ticket = Ticket::new(next.number);
    let skill = Skill::of(ticket, next.labels.iter().map(String::as_str))?;
    let branch = format!("t{}", next.number);
    say!(WORK, "{ticket} {}: on {branch}", next.title);

    let dir = worktree::add(root, &branch, Some(ticket))?;
    let prompt = Session::prompt(skill, ticket);
    run.session.announce(&dir, &prompt);
    let status = run.session.run(&dir, &prompt)?;
    say!(WORK, "the session ended: {status}");
    let tip = rev(root, &format!("refs/heads/{branch}"))?;
    let issue = gh.view(root, ticket)?;
    let ready = ticket::ready_of(root, &branch, ticket)?.is_some();
    if issue.state != State::Closed && !ready {
        return Ok(Worked::Standing {
            ticket,
            commit: tip,
            dir,
            why: format!("is open with no ready record after the session ({status})"),
        });
    }
    worktree::retire(root, &branch).with_context(|| {
        format!(
            "{ticket} closed at {}, but its worktree stands at {}; retire it by hand once it is clean",
            short(&tip),
            dir.display()
        )
    })?;
    Ok(Worked::Closed {
        ticket,
        branch,
        commit: tip,
        ready,
    })
}

/// The frontier of `track` filled by a `/frontier` session in the
/// checkout (#85), and the frontier as it is read once the session ends;
/// the session's exit is said, not judged, as a ticket's is.
fn fill(root: &Path, gh: &Gh, session: &Session, track: &str) -> Result<Option<Next>> {
    say!(WORK, "the frontier of {track} is empty");
    session.announce(root, FRONTIER_PROMPT);
    let status = session.run(root, FRONTIER_PROMPT)?;
    say!(WORK, "the {FRONTIER_PROMPT} session ended: {status}");
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
pub fn command(root: &Path, run: &Run, one: bool) -> Result<()> {
    let store = Store::of_workspace(root);
    let mut tally = Tally::default();
    let mut total = Total::default();
    let ended = steps(root, run, &store, &mut tally, &mut total, one);
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

/// The run itself: one step or the frontier until it empties, each held
/// by [`hold`] as it is reached, so a run the harness stops keeps the
/// steps it took.
fn steps(
    root: &Path,
    run: &Run,
    store: &Store,
    tally: &mut Tally<Landed>,
    total: &mut Total,
    one: bool,
) -> Result<Ended> {
    let gh = Gh::on_path();
    if one {
        let outcome = work(root, &gh, run)?;
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
        let outcome = work(root, &gh, run)
            .with_context(|| format!("{WORK}: {}", outcomes(tally, STOPPED)))?;
        print!("{}", outcome.render());
        let empty = outcome.ended();
        hold(root, store, tally, total, outcome.step())?;
        if let Some(how) = empty {
            if !tally.judged.is_empty() {
                say!(WORK, "{}", outcomes(tally, &how));
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
