//! The harness for the third engine (`harness-design.md` §4): the seam
//! manifest and the oracle pin loaded by one reader, the oracle module that
//! owns every comparison and the divergence registry, the reading and its
//! store, the instruments, `permit`, the judge at the hook's call, `done`,
//! the definition of done, `track`, the branch's track and its base,
//! `vocab`, the glossary as a sensor, `ticket`, the closer, `phpt`, the
//! subset runner, `witness`, the rustdoc witnesses, and `status`, the one
//! renderer.
//!
//! What the crate exposes is what a caller of an instrument needs and
//! nothing beside it (#55): [`run`], the binary's body; each instrument's
//! run function with the arguments it takes and the value it returns; the
//! data modules, [`manifest`], [`oracle`] with the pin, [`divergence`]
//! with the registry, and [`reading`]. Everything else is crate-private,
//! and a test that needs an internal is a unit test beside it.
#![forbid(unsafe_code)]

pub mod api_check;
pub mod budget;
mod built;
pub mod cli;
pub mod clippy;
pub mod computed;
pub mod divergence;
pub mod done;
pub mod generated;
mod git;
mod glossary;
/// What a build of the harness judges by, digested: the seam manifest and
/// `xtask`'s own sources (harness-design.md §4.4, #16). Shared with
/// `build.rs`, which includes the file rather than importing it.
mod inputs;
mod lines;
mod lock;
pub mod manifest;
pub mod new_ext;
pub mod oracle;
pub mod permit;
pub mod phpt;
pub mod reading;
mod rust;
mod sources;
pub mod status;
mod tally;
pub mod ticket;
pub mod track;
pub mod unsafe_audit;
pub mod vocab;
pub mod witness;
pub mod worktree;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::Parser;

use cli::{Cli, Command, TicketCommand, WorktreeCommand};
use divergence::Registry;
use manifest::Manifest;
use reading::{Outcome, Reading, Store};

/// The workspace this binary was built in: the parent of `xtask/`.
pub(crate) fn built_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// The binary's whole body: the command line run, an error printed as
/// `xtask: <err>`, and the exit. A worktree laid down but not ready is
/// neither done nor undone: 3, so a caller can tell it from a refusal
/// that changed nothing, which is 1.
pub fn run() -> ExitCode {
    match command_line() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask: {err:#}");
            if err.downcast_ref::<worktree::NotReady>().is_some() {
                ExitCode::from(3)
            } else {
                ExitCode::from(1)
            }
        }
    }
}

/// The command line, dispatched: one arm per instrument.
fn command_line() -> Result<()> {
    let cli = Cli::parse();
    let root = cli.root.unwrap_or_else(built_root);
    // The judge at the tool call reads the manifest itself: a manifest it
    // cannot read is a refusal it records before it exits, and a manifest
    // loaded out here would take that exit before permit saw the call
    // (#16, harness-design.md §4.4).
    if let Command::Permit {
        json,
        above_the_command_hook,
        bench: false,
        diff: false,
        ..
    } = &cli.command
    {
        let adapter = if *above_the_command_hook {
            permit::Adapter::AboveTheCommandHook
        } else {
            permit::Adapter::CommandHook
        };
        return permit::run(&root, json.as_deref(), adapter);
    }
    if let Some(answered) = before_the_manifest(&root, &cli.command) {
        return answered;
    }
    let manifest = Manifest::load(&root)?;
    let store = Store::of_workspace(&root);
    match cli.command {
        Command::Status {
            section,
            format,
            week,
            session_id,
            session: _,
        } => status_command(&manifest, &store, &root, section, format, week, session_id),
        // The judge itself returned above; what is left of `permit` are the
        // two callers that read the manifest as a whole.
        Command::Permit {
            bench: true,
            runs,
            bar,
            ..
        } => permit::run_bench(&root, &manifest, &store, runs, bar),
        Command::Permit { diff: true, .. } => permit::run_diff(&root, &manifest),
        Command::Permit { .. } => bail!("permit: the judge at the tool call is run above"),
        Command::Done { base, hook } => {
            let registry = Registry::load(&root)?;
            if hook {
                done::run_hook(&root, &manifest, &registry, &store);
                Ok(())
            } else {
                done::run(&root, &manifest, &registry, &store, base.as_deref())
            }
        }
        Command::Budget => record(&store, &budget::run(&root, &manifest)?),
        Command::Clippy { crates, staged } => {
            let readings = if staged {
                clippy::run_staged(&root, &manifest)?
            } else {
                clippy::run(&root, &manifest, &crates)?
            };
            record(&store, &readings)
        }
        Command::Phpt {
            subset,
            full,
            jit,
            engine,
        } => phpt_command(&root, &manifest, &store, subset, full, jit, engine),
        Command::UnsafeAudit => record(&store, &unsafe_audit::run(&root, &manifest)?),
        Command::Generate => {
            for path in generated::write_all(&root, &manifest)? {
                println!("wrote {}", path.display());
            }
            Ok(())
        }
        Command::ApiCheck { write } => api_check::run(&manifest, &root, &store, write),
        Command::DivergenceReason => divergence_command(&root, &manifest, &store),
        Command::NewExt { name } => {
            let laid = new_ext::run(&root, &manifest, &name)?;
            for path in &laid.written {
                println!("wrote {path}");
            }
            for note in &laid.notes {
                println!("{note}");
            }
            Ok(())
        }
        Command::Witness {
            crates,
            accept,
            staged,
        } => witness::command(&root, &manifest, &store, crates, accept, staged),
        Command::Vocab { crates } => match vocab::run(&root, &manifest, &store, &crates)? {
            Outcome::Met => Ok(()),
            Outcome::Unmet { .. } => {
                bail!("vocab: an avoided word is in use; the findings are above")
            }
        },
        Command::Ticket { command } => ticket_command(&root, &store, command),
        Command::Track { .. } => bail!("track: answered above"),
        Command::Regions { .. } => bail!("regions: answered above"),
        Command::Worktree { .. } => bail!("worktree: answered above"),
    }
}

/// What every instrument ends with: each reading printed as one line and
/// the set recorded, whose outcome is the exit.
pub(crate) fn record(store: &Store, readings: &[Reading]) -> Result<()> {
    for reading in readings {
        println!(
            "{} {} {}: {}",
            reading.status.as_str(),
            reading.instrument,
            reading.scope,
            reading.message
        );
    }
    store.record(readings)?.exit()
}

/// `xtask ticket read` and `close`: the ticket named or the branch's, read
/// through the `gh` on `PATH`, one reading recorded.
fn ticket_command(root: &Path, store: &Store, command: TicketCommand) -> Result<()> {
    let (act, number, session_id) = match command {
        TicketCommand::Read { number, session_id } => (ticket::Act::Read, number, session_id),
        TicketCommand::Close { number, session_id } => (ticket::Act::Close, number, session_id),
    };
    let ticket = ticket::ticket_of(root, number)?;
    let reading = ticket::run(
        root,
        &ticket::Gh::on_path(),
        act,
        ticket,
        session_id.as_deref(),
    )?;
    record(store, std::slice::from_ref(&reading))
}

/// `xtask phpt`: the subsets asked for, each run and each written.
fn phpt_command(
    root: &Path,
    manifest: &Manifest,
    store: &Store,
    subset: Option<String>,
    full: bool,
    jit: Option<phpt::Lane>,
    engine: Option<PathBuf>,
) -> Result<()> {
    let registry = Registry::load(root)?;
    let ask = phpt::Ask {
        subset,
        full,
        lane: jit,
        engine,
    };
    record(store, &phpt::run(root, manifest, &registry, &ask)?)
}

/// `xtask divergence-reason`: the registry against the rule whose `check`
/// names it. A function of the shape `phpt` and `status` have, so that the
/// dispatch above stays a list of one arm per instrument: it is at clippy's
/// hundred-line ceiling, and `witness` is the arm that tipped it.
fn divergence_command(root: &Path, manifest: &Manifest, store: &Store) -> Result<()> {
    let registry = Registry::load(root)?;
    let reading = divergence::reason(&registry, manifest, root)?;
    match store.record(std::slice::from_ref(&reading))? {
        Outcome::Met => {
            println!("{}", reading.message);
            Ok(())
        }
        Outcome::Unmet { .. } => bail!("{}", reading.message),
    }
}

/// The commands that measure nothing and read no manifest, answered
/// before `Manifest::load`: the track, so a worktree whose manifest is
/// unreadable can still say which track it is on; the two halves of a
/// worktree, whose manifest is the worktree's, once built there, and is
/// gone once it is retired; and the regions, which pre-commit renders
/// whatever the manifest says.
fn before_the_manifest(root: &Path, command: &Command) -> Option<Result<()>> {
    match command {
        Command::Track { branch, json } => Some(track_command(root, branch.as_deref(), *json)),
        Command::Regions { files } => Some(computed::command(root, files)),
        Command::Worktree {
            command: WorktreeCommand::Add { branch, ticket },
        } => Some(worktree::add(root, branch, ticket.map(track::Ticket::new)).map(|_| ())),
        Command::Worktree {
            command: WorktreeCommand::Retire { branch },
        } => Some(worktree::retire(root, branch)),
        _ => None,
    }
}

/// `xtask track`: the facts as text, or as one object; no reading.
fn track_command(root: &Path, branch: Option<&str>, json: bool) -> Result<()> {
    let track = track::Track::of(root, branch)?;
    if json {
        println!("{}", track.to_json());
    } else {
        print!("{}", track.render());
    }
    Ok(())
}

/// `xtask status`: one section, or with no section the map and the local
/// facts for the session-start hook, whose stdin head names the session.
fn status_command(
    manifest: &Manifest,
    store: &Store,
    root: &Path,
    section: Option<cli::Section>,
    format: cli::Format,
    week: Option<usize>,
    session_id: Option<String>,
) -> Result<()> {
    let text = if let Some(section) = section {
        let registry = Registry::load(root)?;
        status::render(
            manifest,
            &registry,
            store,
            root,
            section,
            format,
            week,
            session_id.as_deref(),
        )?
    } else {
        let id = session_id.or_else(status::session_of_stdin);
        status::session(manifest, store, root, id.as_deref())?
    };
    std::io::stdout().write_all(text.as_bytes())?;
    Ok(())
}
