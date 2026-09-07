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
    /// Hold the seams: regenerate and diff each lock, check crate edges,
    /// `use` paths and the bundle order; one reading per seam and per crate.
    ApiCheck {
        /// Rewrite each lock file from the sources instead of failing on a diff.
        #[arg(long)]
        write: bool,
    },
    /// Render one section from the manifest and the store, or with
    /// `--session` the map plus the local facts; computes nothing else.
    Status {
        #[arg(long, required_unless_present = "session", conflicts_with = "session")]
        section: Option<Section>,
        #[arg(long, default_value = "md")]
        format: Format,
        /// Print AGENTS.md and .claude/harness.local.md in one block, for
        /// the `SessionStart` hook; the local file is laid down when missing.
        #[arg(long)]
        session: bool,
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
    /// The definition of done: print the conditions unmet for the changes
    /// since the base and exit 1 while any is; the Stop hook, pre-push and
    /// CI read the same list.
    Done {
        /// Measure the changes since the merge base with this ref; default `main`.
        #[arg(long, value_name = "REF", conflicts_with = "hook")]
        base: Option<String>,
        /// Read the Stop hook's stdin: exit 0 when `stop_hook_active`,
        /// otherwise print the block JSON with the list while anything is unmet.
        #[arg(long)]
        hook: bool,
    },
    /// Hold every crate to its line ceiling and the workspace to [budget];
    /// refuse committed generated code and a stale generated file.
    Budget,
    /// Allow `unsafe` only in the homes the manifest's unsafe.homes rule names.
    UnsafeAudit,
    /// Write the files the manifest's `generated` rules render (ext/clippy.toml).
    Generate,
    /// Hold the divergence registry to the manifest rule whose `check` names
    /// this instrument: an accepted row has a reason a person wrote.
    DivergenceReason,
    /// Lay one extension crate down at ext/<name> in the template topology:
    /// the facade as its one dependency, `extension()`, state on the value,
    /// a test through the capture sink, a witness, ext/clippy.toml
    /// rewritten and the row in ext/AGENTS.md; refuses an existing one.
    NewExt {
        /// The extension's name as php-src spells it, with a `[[crate]]` row.
        name: String,
    },
    /// The glossary as a sensor: `_Avoid_` words in public item names and
    /// rustdoc prose, never in a lock; one reading per crate.
    Vocab {
        /// Only these crates, by manifest name; default every crate that exists.
        #[arg(long = "crate", value_name = "NAME")]
        crates: Vec<String>,
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
    /// The divergence registry, with counts by kind and status.
    Divergence,
    /// The phpt readings, one row per subset.
    Phpt,
    /// The witness readings, one row per crate.
    Witness,
    /// The mutants readings, one row per crate.
    Mutants,
    /// Each rule permit judges: its last finding, denial or note, and how
    /// many denials the log holds.
    Permits,
    /// The log and the status of the working tree; volatile.
    Git,
    /// The bundle's registration order against the crate graph.
    Bundle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Md,
    Json,
}
