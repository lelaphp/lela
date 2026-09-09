//! The harness for the third engine (`harness-design.md` §4): the seam
//! manifest and the oracle pin loaded by one reader, the oracle module that
//! owns every comparison and the divergence registry, the reading and its
//! store, the instruments, `permit`, the judge at the tool call, `done`,
//! the definition of done, `track`, the branch's track and its base,
//! `vocab`, the glossary as a sensor, `phpt`, the
//! subset runner, `witness`, the rustdoc witnesses, and `status`, the one
//! renderer.
#![forbid(unsafe_code)]

pub mod api_check;
pub mod budget;
pub mod built;
pub mod cli;
pub mod clippy;
pub mod divergence;
pub mod done;
pub mod generated;
pub mod git;
pub mod glossary;
/// What a build of the harness judges by, digested: the seam manifest and
/// `xtask`'s own sources (harness-design.md §4.4, #16). Shared with
/// `build.rs`, which includes the file rather than importing it.
pub mod inputs;
pub mod lines;
pub mod lock;
pub mod manifest;
pub mod new_ext;
pub mod oracle;
pub mod permit;
pub mod phpt;
pub mod reading;
pub mod rust;
pub mod sources;
pub mod status;
pub mod track;
pub mod unsafe_audit;
pub mod vocab;
pub mod witness;

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::Parser;

use cli::{Cli, Command};
use divergence::Registry;
use git::Stamp;
use manifest::Manifest;
use reading::{Reading, Status, Store};

/// The workspace this binary was built in: the parent of `xtask/`.
pub fn built_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// Run the command line; the binary's whole body.
pub fn run() -> Result<()> {
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
    // The track measures nothing and reads no manifest, so a worktree
    // whose manifest is unreadable can still say which track it is on.
    if let Command::Track { branch, json } = &cli.command {
        return track_command(&root, branch.as_deref(), *json);
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
            bench, runs, diff, ..
        } => {
            if bench {
                permit::run_bench(&root, runs)
            } else if diff {
                permit::run_diff(&root, &manifest)
            } else {
                bail!("permit: the judge at the tool call is run above")
            }
        }
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
        Command::Witness { crates, accept } => {
            witness::command(&root, &manifest, &store, crates, accept)
        }
        Command::Vocab { crates } => {
            if vocab::run(&root, &manifest, &store, &crates)? {
                Ok(())
            } else {
                bail!("vocab: an avoided word is in use; the findings are above")
            }
        }
        Command::Track { .. } => bail!("track: answered above"),
    }
}

/// What every instrument ends with: each reading written to the store and
/// printed as one line; an error when any failed, so the exit status says so.
pub fn record(store: &Store, readings: &[Reading]) -> Result<()> {
    let mut failed = 0;
    for reading in readings {
        store.write(reading)?;
        println!(
            "{} {} {}: {}",
            reading.status.as_str(),
            reading.instrument,
            reading.scope,
            reading.message
        );
        if reading.status == Status::Fail {
            failed += 1;
        }
    }
    if failed > 0 {
        bail!("{failed} of {} readings failed", readings.len());
    }
    Ok(())
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
/// names it. A helper of the shape `phpt` and `status` have, so that the
/// dispatch above stays a list of one arm per instrument: it is at clippy's
/// hundred-line ceiling, and `witness` is the arm that tipped it.
fn divergence_command(root: &Path, manifest: &Manifest, store: &Store) -> Result<()> {
    let registry = Registry::load(root)?;
    let mut reading = divergence::reason(&registry, manifest);
    reading.stamp(&Stamp::take(root, Path::new(divergence::FILE))?);
    store.write(&reading)?;
    if reading.status == Status::Fail {
        bail!("{}", reading.message);
    }
    println!("{}", reading.message);
    Ok(())
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
