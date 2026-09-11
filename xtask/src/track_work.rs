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
//! the ticket and runs `/implement <n>`. The hooks that already exist
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
//! on: `computed` covers a worktree by its clone's grant (#74). A
//! command, not an instrument: the readings are the worktree's own,
//! appended by the retire, and none is written here.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use anyhow::{Context, Result, bail};

use crate::git::git;
use crate::ticket::{Gh, State};
use crate::track::{self, Model, Ticket, Track, fast_forward, not_ahead, rev, short};
use crate::track_next;
use crate::track_sync;
use crate::worktree::{self, say};

/// The prefix of every line printed, so a person reads whose line it is.
const WORK: &str = "track work";

/// The permission mode a session runs under unless the clone records
/// `track.permission-mode`: every call is judged by `permit` through the
/// hooks, so nothing is left for a prompt no one answers.
pub(crate) const PERMISSION_MODE: &str = "bypassPermissions";
/// The effort a session runs at unless the clone records `track.effort`.
pub(crate) const EFFORT: &str = "high";
/// The environment variable Claude Code reads the subagents' model from.
pub(crate) const SUBAGENT_VARIABLE: &str = "CLAUDE_CODE_SUBAGENT_MODEL";
/// The program a session is, where the clone records none.
const PROGRAM: &str = "claude";
/// The clone's records that override the constants and the program.
const PROGRAM_KEY: &str = "track.session.program";
const MODE_KEY: &str = "track.permission-mode";
const EFFORT_KEY: &str = "track.effort";

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

    /// The prompt: it names the ticket and runs `/implement <n>`.
    pub fn prompt(ticket: Ticket) -> String {
        format!("/implement {}", ticket.number())
    }

    /// The arguments after the program, in the order they are passed.
    pub fn arguments(&self, ticket: Ticket) -> Vec<String> {
        vec![
            "-p".to_owned(),
            "--model".to_owned(),
            self.model.session.clone(),
            "--permission-mode".to_owned(),
            self.permission_mode.clone(),
            "--effort".to_owned(),
            self.effort.clone(),
            Session::prompt(ticket),
        ]
    }

    /// The session run in `dir` for `ticket`, its streams the caller's,
    /// its exit when it ends, which is said and not judged: whether the
    /// ticket closed at the tracker is the one signal, since the hooks
    /// judged the session's turns; a program that does not start is an
    /// error.
    fn run(&self, dir: &Path, ticket: Ticket) -> Result<ExitStatus> {
        Command::new(&self.program)
            .args(self.arguments(ticket))
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
    /// The one line: the ticket, the commit and the outcome.
    pub fn render(&self) -> String {
        match self {
            Worked::Nothing { track } => {
                format!("{WORK}: none: the frontier of {track} is empty; nothing laid down\n")
            }
            Worked::Closed {
                ticket,
                branch,
                commit,
                track,
            } => format!(
                "{WORK}: {ticket} closed at {}; {track} fast-forwarded to it and pushed to origin, {branch} retired\n",
                short(commit)
            ),
            Worked::Standing {
                ticket,
                commit,
                dir,
                why,
            } => format!(
                "{WORK}: {ticket} {why}, the branch at {}; the worktree stands at {} for a person, nothing retired\n",
                short(commit),
                dir.display()
            ),
        }
    }

    /// The ticket and its outcome in one word, as the closing line lists
    /// them; an empty frontier worked no ticket and has none.
    fn brief(&self) -> Option<String> {
        match self {
            Worked::Nothing { .. } => None,
            Worked::Closed { ticket, .. } => Some(format!("{ticket} closed")),
            Worked::Standing { ticket, .. } => Some(format!("{ticket} standing")),
        }
    }
}

/// The closing line of a run: `how` it ended, how many tickets it worked
/// and each with its outcome.
fn outcomes(worked: &[Worked], how: &str) -> String {
    let briefs = worked.iter().filter_map(Worked::brief).collect::<Vec<_>>();
    match briefs.len() {
        0 => format!("{how} no ticket"),
        1 => format!("{how} 1 ticket: {}", briefs[0]),
        n => format!("{how} {n} tickets: {}", briefs.join(", ")),
    }
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
            "the clone records no model, so no session is started and nothing is laid down; `xtask track new` records one, or `git config {} <name>`",
            "track.model.session"
        );
    };
    let session = Session::of(root, model);
    git(root, &["remote", "get-url", "origin"], None)
        .context("the repository has no origin, and the track's branch is pushed to it; nothing is laid down")?;
    let synced = track_sync::sync(root)
        .context("the track was not caught up with main, so nothing is laid down")?;
    print!("{}", synced.render());
    // Read again: the sync moved the track, and its base with it.
    let track = Track::of(root, None)?;
    let Some(next) = track_next::next(root, gh, &track.name)? else {
        return Ok(Worked::Nothing { track: track.name });
    };
    let ticket = Ticket::new(next.number);
    let branch = format!("{}/t{}", track.name, next.number);
    say(WORK, &format!("{ticket} {}: on {branch}", next.title));

    let dir = worktree::add(root, &branch, Some(ticket))?;
    say(
        WORK,
        &format!(
            "session: {} {} in {}, subagents on {}",
            session.program.display(),
            session.arguments(ticket).join(" "),
            dir.display(),
            session.model.subagent()
        ),
    );
    let status = session.run(&dir, ticket)?;
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
    if let Some(why) = not_ahead(root, &track, &tip)? {
        return Ok(Worked::Standing {
            ticket,
            commit: tip,
            dir,
            why: format!("closed, but the branch is not ahead of the track: {why}"),
        });
    }
    fast_forward(WORK, root, &track, &tip).with_context(|| {
        format!(
            "{ticket} closed, but the track did not move to {}; the worktree stands at {}, nothing retired",
            short(&tip),
            dir.display()
        )
    })?;
    worktree::retire(root, &branch).with_context(|| {
        format!(
            "{ticket} closed and {} pushed at {}, but the worktree stands at {}; retire it by hand once it is clean",
            track.name,
            short(&tip),
            dir.display()
        )
    })?;
    Ok(Worked::Closed {
        ticket,
        branch,
        commit: tip,
        track: track.name,
    })
}

/// `xtask track work`: the tracker on `PATH`, the frontier worked until
/// it empties and the closing line printed, or with `one` a single step whose
/// ticket left open is the exit 1.
pub fn command(root: &Path, one: bool) -> Result<()> {
    let gh = Gh::on_path();
    if one {
        let outcome = work(root, &gh)?;
        if let Worked::Standing { .. } = outcome {
            bail!("{}", outcome.render().trim_end());
        }
        print!("{}", outcome.render());
        return Ok(());
    }
    let mut worked = Vec::new();
    loop {
        let outcome = work(root, &gh).with_context(|| {
            format!(
                "{WORK}: {}",
                outcomes(&worked, "stopped by the harness after")
            )
        })?;
        print!("{}", outcome.render());
        if let Worked::Nothing { track } = &outcome {
            if !worked.is_empty() {
                say(
                    WORK,
                    &outcomes(&worked, &format!("the frontier of {track} is empty after")),
                );
            }
            return Ok(());
        }
        worked.push(outcome);
    }
}
