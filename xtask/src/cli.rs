//! The command line. `xtask --help` is the list of instruments
//! (harness-design.md §5).

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "xtask",
    about = "The harness for the third engine",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// The workspace root; defaults to the workspace this binary was built in.
    #[arg(long, global = true, value_name = "DIR")]
    pub root: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Render one section from the manifest and the store; computes nothing else.
    Status {
        #[arg(long)]
        section: Section,
        #[arg(long, default_value = "md")]
        format: Format,
    },
    /// Judge one intended tool call from the `PreToolUse` hook's stdin; the
    /// decision is the JSON on stdout and the exit is 0 always.
    Permit {
        /// The call as a JSON argument instead of stdin (the function-hook adapter).
        #[arg(long, value_name = "JSON", conflicts_with = "bench")]
        json: Option<String>,
        /// Measure p99 latency at the tool call against the 100 ms bar.
        #[arg(long)]
        bench: bool,
        /// How many calls `--bench` times.
        #[arg(long, default_value_t = 100, requires = "bench")]
        runs: usize,
    },
}

/// The sections of harness-design.md §4.6 that have a renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Section {
    /// The manifest's rules with ADR numbers and failure text.
    Rules,
    /// Each lock's width against `api_items`.
    Locks,
    /// Each crate against its ceiling.
    Budget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Md,
    Json,
}
