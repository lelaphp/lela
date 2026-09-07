//! The harness for the third engine (`harness-design.md` §4): the seam
//! manifest and the oracle pin loaded by one reader, the oracle module that
//! owns every comparison and the divergence registry, the reading and its
//! store, the instruments, `permit`, the judge at the tool call, `vocab`,
//! the glossary as a sensor, and `status`, the one renderer.
#![forbid(unsafe_code)]

pub mod api_check;
pub mod budget;
pub mod cli;
pub mod divergence;
pub mod generated;
pub mod git;
pub mod glossary;
pub mod lines;
pub mod lock;
pub mod manifest;
pub mod oracle;
pub mod permit;
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
            session: _,
        } => {
            let text = if let Some(section) = section {
                let registry = Registry::load(&root)?;
                status::render(&manifest, &registry, &store, &root, section, format)?
            } else {
                status::session(&root)?
            };
            std::io::stdout().write_all(text.as_bytes())?;
            Ok(())
        }
        Command::Permit { json, bench, runs } => {
            if bench {
                permit::run_bench(&root, runs)
            } else {
                permit::run(&root, json.as_deref());
                Ok(())
            }
        }
        Command::Budget => record(&store, &budget::run(&root, &manifest)?),
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
