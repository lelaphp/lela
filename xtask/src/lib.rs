//! The harness for the third engine (`harness-design.md` §4): the seam
//! manifest and the oracle pin loaded by one reader, the oracle module that
//! owns every comparison and the divergence registry, the reading and its
//! store, the instruments, `permit`, the judge at the tool call, `done`,
//! the definition of done, `vocab`, the glossary as a sensor, `phpt`, the
//! subset runner, and `status`, the one renderer.
#![forbid(unsafe_code)]

pub mod api_check;
pub mod budget;
pub mod cli;
pub mod clippy;
pub mod divergence;
pub mod done;
pub mod generated;
pub mod git;
pub mod glossary;
pub mod lines;
pub mod lock;
pub mod manifest;
pub mod new_ext;
pub mod oracle;
pub mod permit;
pub mod phpt;
pub mod reading;
pub mod sources;
pub mod status;
pub mod unsafe_audit;
pub mod vocab;

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
        Command::Permit {
            json,
            above_the_command_hook,
            bench,
            runs,
            diff,
        } => {
            if bench {
                permit::run_bench(&root, runs)
            } else if diff {
                permit::run_diff(&root, &manifest)
            } else {
                let adapter = if above_the_command_hook {
                    permit::Adapter::AboveTheCommandHook
                } else {
                    permit::Adapter::CommandHook
                };
                permit::run(&root, json.as_deref(), adapter);
                Ok(())
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
        Command::DivergenceReason => {
            let registry = Registry::load(&root)?;
            let mut reading = divergence::reason(&registry, &manifest);
            reading.stamp(&Stamp::take(&root, Path::new(divergence::FILE))?);
            store.write(&reading)?;
            if reading.status == Status::Fail {
                bail!("{}", reading.message);
            }
            println!("{}", reading.message);
            Ok(())
        }
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
        Command::Vocab { crates } => {
            if vocab::run(&root, &manifest, &store, &crates)? {
                Ok(())
            } else {
                bail!("vocab: an avoided word is in use; the findings are above")
            }
        }
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
