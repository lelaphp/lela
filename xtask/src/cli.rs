//! The command line. `xtask --help` is the list of instruments
//! (harness-design.md §5).

use std::path::PathBuf;

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};

use crate::phpt::Lane;

#[derive(Debug, Parser)]
#[command(
    name = "xtask",
    about = "The harness for the third engine",
    disable_help_subcommand = true
)]
pub(crate) struct Cli {
    /// The workspace root; defaults to the workspace this binary was built in.
    #[arg(long, global = true, value_name = "DIR")]
    pub root: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Hold the seams: regenerate and diff each lock, hold crate edges,
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
        /// One row of the `week` section: the Nth bar of the manifest's
        /// `[[week]]` table, counted from 1.
        #[arg(long, value_name = "N", requires = "section")]
        week: Option<usize>,
        /// "This session" of the `permits` section: the session whose
        /// denials are counted apart. `--session` takes it from the hook's
        /// stdin head instead.
        #[arg(long = "session-id", value_name = "ID")]
        session_id: Option<String>,
        /// Print AGENTS.md and .claude/harness.local.md in one block, for
        /// the `SessionStart` hook; the local file is laid down when missing.
        #[arg(long)]
        session: bool,
    },
    /// Judge one intended call from the `PreToolUse` hook's stdin; the
    /// decision is the JSON on stdout and the exit is 0 always.
    Permit {
        /// The call as a JSON argument instead of stdin.
        #[arg(long, value_name = "JSON", conflicts_with_all = ["bench", "diff"])]
        json: Option<String>,
        /// The function-hook adapter's call: a denial ends the call here and
        /// is recorded; an allow goes on to the command hook beneath, which
        /// records the notes, so a note is one reading and not two.
        #[arg(long, conflicts_with_all = ["bench", "diff"])]
        above_the_command_hook: bool,
        /// Judge the staged diff, each file as the `Write` of its staged
        /// content (pre-commit); exit 1 while any is denied.
        #[arg(long, conflicts_with = "bench")]
        diff: bool,
        /// Measure p99 latency at the call against the bar: a
        /// measurement, which owes no exit, so the reading says pass or
        /// fail and the exit is 0 whatever it says.
        #[arg(long)]
        bench: bool,
        /// How many calls `--bench` times.
        #[arg(long, default_value_t = 100, requires = "bench")]
        runs: usize,
        /// The p99 bar `--bench` holds the reading to, in milliseconds;
        /// harness-design.md §4.4's by default.
        #[arg(long, default_value_t = crate::permit::BAR_MS, value_name = "MS", requires = "bench")]
        bar: f64,
    },
    /// The definition of done: print the conditions unmet for the changes
    /// since the base and exit 1 while any is; the Stop hook, pre-push and
    /// CI read the same list.
    Done {
        /// Measure the changes since the merge base with this ref;
        /// default the branch's track, and `main` for a branch on none.
        #[arg(long, value_name = "REF", conflicts_with = "hook")]
        base: Option<String>,
        /// Read the Stop hook's stdin: exit 0 when `stop_hook_active`,
        /// otherwise print the block JSON with the list while anything is unmet.
        #[arg(long)]
        hook: bool,
    },
    /// Say which track this worktree's branch is on (the longest existing
    /// branch that is a proper path prefix of its name, `main` for a branch
    /// on none), the commit `done` measures since, the root that holds
    /// the track's store, and the ticket the branch records; as text, or
    /// as one object. Writes no reading.
    Track {
        /// A branch other than `HEAD`'s, made or not yet: what the add
        /// command asks before it takes the branch off its track.
        #[arg(long, value_name = "NAME")]
        branch: Option<String>,
        /// Print the same facts as one JSON object.
        #[arg(long)]
        json: bool,
    },
    /// A track's worktrees: `add` lays one down under .claude/worktrees/<branch>
    /// ready to be worked in, the branch off its track with the ticket it
    /// is for recorded on it, `xtask` built there,
    /// the local file laid and rendered, the `computed trust` line printed
    /// as a person's; refuses a branch that exists and a directory that
    /// does. `retire` appends the worktree's log into its track's store,
    /// removes the worktree and prints the branch-delete line as a
    /// person's; refuses uncommitted work. Writes no reading.
    Worktree {
        #[command(subcommand)]
        command: WorktreeCommand,
    },
    /// A ticket, one GitHub issue: `read <n>` fetches it through `gh`,
    /// holds it to the boxes under its `## Acceptance criteria` heading
    /// and writes a reading of instrument `ticket`, the unticked named;
    /// `close <n>` does the same and then closes it, refusing while a box
    /// is unticked and changing nothing at the tracker. No number is the
    /// branch's ticket, as `xtask track` reads it. The one way an agent
    /// closes a ticket.
    Ticket {
        #[command(subcommand)]
        command: TicketCommand,
    },
    /// Render the named files' regions with `computed run` and say what it
    /// did, one line per file, `written` or `unchanged`, for pre-commit to
    /// re-stage; refuses a broken marker, a missing `computed` and an
    /// untrusted checkout, whose `computed trust` line is printed as a
    /// person's. Writes no reading.
    Regions {
        /// The files, relative to the root.
        #[arg(required = true, value_name = "FILE")]
        files: Vec<String>,
    },
    /// Hold every crate to its line ceiling and the workspace to [budget];
    /// refuse committed generated code and a stale generated file.
    Budget,
    /// `cargo clippy` with every warning denied, on every target of each
    /// crate, as the reading `done` reads; one reading per crate.
    Clippy {
        /// Only these crates, by manifest name; default every crate that exists.
        #[arg(long = "crate", value_name = "NAME", conflicts_with = "staged")]
        crates: Vec<String>,
        /// Only the crates with a staged change (pre-commit's touched crates).
        #[arg(long)]
        staged: bool,
    },
    /// Run the pinned `.phpt` corpus through the engine and hold each test
    /// to its `--EXPECT*--` section: the per-commit lane the manifest
    /// composes, or the subset named; one reading per subset, scoped to
    /// the crate whose subset it is.
    Phpt {
        /// The subset: a crate by name (`php_engine`, `pcre`, `ext/pcre`),
        /// whose subsets its `[[crate]]` row owns, or a directory of the
        /// pinned php-src tree (`Zend/tests`). Without it, the per-commit
        /// lane: every subset the manifest's rows own.
        #[arg(long, value_name = "NAME", conflicts_with = "full")]
        subset: Option<String>,
        /// Every subset of the corpus: the nightly lane, which CI runs.
        #[arg(long)]
        full: bool,
        /// The compiled tier's lane: the engine runs under `jit.mode`.
        #[arg(long, value_name = "MODE")]
        jit: Option<Lane>,
        /// The binary the process adapter runs; without it, the engine's
        /// own where cargo puts it.
        #[arg(long, value_name = "PATH")]
        engine: Option<PathBuf>,
    },
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
    /// Extract every `php` fence and its `expect` fence from a crate's
    /// rustdoc, run each snippet under the oracle's recipe as `witness.php`
    /// and hold it to what the fence pins; count the kernels and natives
    /// without one. One reading per crate.
    Witness {
        /// Only these crates, by manifest name; default every crate that exists.
        #[arg(long = "crate", value_name = "NAME", conflicts_with = "staged")]
        crates: Vec<String>,
        /// Only the crates with a staged change (pre-commit's touched crates).
        #[arg(long)]
        staged: bool,
        /// Write the oracle's status onto each `expect` fence line that
        /// does not already pin it; the only thing that writes an `exit`.
        #[arg(long)]
        accept: bool,
    },
    /// The glossary as a sensor: `_Avoid_` words in public item names and
    /// rustdoc prose, never in a lock; one reading per crate.
    Vocab {
        /// Only these crates, by manifest name; default every crate that exists.
        #[arg(long = "crate", value_name = "NAME")]
        crates: Vec<String>,
    },
}

/// The two halves of a track's worktree.
#[derive(Debug, Subcommand)]
pub(crate) enum WorktreeCommand {
    /// Lay the worktree for a branch down, ready to be worked in, with
    /// the ticket its work is for recorded on the branch.
    Add {
        /// The branch, not yet made; it names its own track.
        branch: String,
        /// The issue the branch's work is for, recorded on the branch and
        /// read by `xtask track`; one branch per ticket. Without it the
        /// branch records none, which `xtask track` says.
        #[arg(long, value_name = "N")]
        ticket: Option<u64>,
    },
    /// Append the worktree's log into its track's store, then remove the
    /// worktree; the branch is kept.
    Retire {
        /// The branch a worktree is on; it names its own track.
        branch: String,
    },
}

/// Read the criteria, or read them and close.
#[derive(Debug, Subcommand)]
pub(crate) enum TicketCommand {
    /// Fetch the issue, hold it to its acceptance criteria, write the
    /// reading; exit 1 while a box is unticked.
    Read {
        /// The issue number; without it, the branch's recorded ticket.
        number: Option<u64>,
        /// The session the reading carries, the way a `permit` reading
        /// carries the hook's `session_id`; empty when none is named.
        #[arg(long = "session-id", value_name = "ID")]
        session_id: Option<String>,
    },
    /// Read as `read` does, then close the issue at the tracker with a
    /// comment naming the commit; refuse while a box is unticked.
    Close {
        /// The issue number; without it, the branch's recorded ticket.
        number: Option<u64>,
        /// The session the reading carries, the way a `permit` reading
        /// carries the hook's `session_id`; empty when none is named.
        #[arg(long = "session-id", value_name = "ID")]
        session_id: Option<String>,
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
    /// The ticket readings, one row per ticket: its criteria, what closed
    /// it and the session.
    Ticket,
    /// Each rule permit judges: its last finding, denial or note, and the
    /// denials the log holds per session and in total; then the log per
    /// instrument, with the candidates for silent (the CI section of §7).
    Permits,
    /// The weekly exit bar table: each `[[week]]` of the manifest with
    /// pass, fail or "no reading" from the readings it names.
    Week,
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

/// The subcommands `xtask` has, by name: what a reading's `instrument`
/// and a `[[week]]` read may name. `xtask --help` is the same list.
pub(crate) fn instruments() -> Vec<String> {
    Cli::command()
        .get_subcommands()
        .map(|c| c.get_name().to_owned())
        .collect()
}
