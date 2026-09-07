//! The harness for the third engine (`harness-design.md` §4): the seam
//! manifest and the oracle pin loaded by one reader, the reading and its
//! store, `permit`, the judge at the tool call, and `status`, the one
//! renderer.
#![forbid(unsafe_code)]

pub mod cli;
pub mod git;
pub mod manifest;
pub mod permit;
pub mod reading;
pub mod status;

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
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
        Command::Permit { json, bench, runs } => {
            if bench {
                permit::run_bench(&root, runs)
            } else {
                permit::run(&root, json.as_deref())
            }
        }
    }
}
