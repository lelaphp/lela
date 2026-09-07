//! The harness for the third engine (`harness-design.md` §4): the seam
//! manifest and the oracle pin loaded by one reader, the reading and its
//! store, `status`, the one renderer, and `vocab`, the glossary as a sensor.
#![forbid(unsafe_code)]

pub mod cli;
pub mod git;
pub mod glossary;
pub mod manifest;
pub mod reading;
pub mod status;
pub mod vocab;

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::Parser;

use cli::{Cli, Command};
use manifest::Manifest;
use reading::Store;

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
    match cli.command {
        Command::Status { section, format } => {
            let manifest = Manifest::load(&root)?;
            let store = Store::of_workspace(&root);
            let text = status::render(&manifest, &store, section, format)?;
            std::io::stdout().write_all(text.as_bytes())?;
            Ok(())
        }
        Command::Vocab { crates } => {
            let manifest = Manifest::load(&root)?;
            let store = Store::of_workspace(&root);
            if vocab::run(&root, &manifest, &store, &crates)? {
                Ok(())
            } else {
                bail!("vocab: an avoided word is in use; the findings are above")
            }
        }
    }
}
