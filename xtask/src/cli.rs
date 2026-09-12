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
    /// `--session` the map and the local facts; computes nothing else.
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
        /// Print AGENTS.md and .claude/harness.local.md for the function
        /// hook's `prompt.context`: as one text, or with `--format json` as
        /// the two blocks `map` and `facts`, each under the engine's cap on
        /// its own; the local file is laid down when missing.
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
    /// Say which track the branch is on, the commit `done` measures since,
    /// the track's store root, the branch's ticket and the track's model;
    /// as text or as one object. Writes no reading.
    ///
    /// The track is the longest existing branch that is a proper path
    /// prefix of the branch's name, `main` for a branch on none; for a
    /// detached `HEAD` it is the clone's record, which a track's own clone
    /// carries, and `main` where there is none. The model is the clone's
    /// record too, one per role, and a clone with none says so.
    ///
    /// `new <name> --model <name>` lays a track down: `origin/<name>` at
    /// `main`, a clone beside the repository detached at it and recording
    /// its track and its model, `xtask` built there, the console's row,
    /// the `computed trust` line printed as a person's.
    /// `next` names the ticket the track works next, the oldest open
    /// `ready-for-agent` ticket that is unblocked, this track's and on no
    /// branch of this clone. `sync` catches the track up with main.
    /// `work` takes that ticket from start to retire without a person,
    /// syncing before each.
    #[command(args_conflicts_with_subcommands = true)]
    Track {
        /// A branch other than `HEAD`'s, made or not yet: what the add
        /// command asks before it takes the branch off its track.
        #[arg(long, value_name = "NAME")]
        branch: Option<String>,
        /// Print the same facts as one JSON object.
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        command: Option<TrackCommand>,
    },
    /// A track's worktrees: `add` lays one down under .claude/worktrees/<branch>,
    /// `retire` appends its log into the track's store and removes it.
    /// Writes no reading.
    ///
    /// `add` lays the worktree down ready to be worked in, the branch off
    /// its track with the ticket it is for recorded on it, `xtask` built
    /// there, the local file laid and rendered under the clone's grant,
    /// which covers the worktree and asks for none of its own (#74, §11);
    /// refuses a branch that exists and a directory that does. `retire` appends the worktree's log into its track's
    /// store, removes the worktree and prints the branch-delete line as a
    /// person's; refuses uncommitted work.
    Worktree {
        #[command(subcommand)]
        command: WorktreeCommand,
    },
    /// A ticket, one GitHub issue: `read <n>` holds it to its acceptance
    /// criteria and writes the reading; `close <n>` does the same and then
    /// closes it, naming what the work opened.
    ///
    /// `read <n>` fetches the issue through `gh`, holds it to the boxes
    /// under its `## Acceptance criteria` heading and writes a reading of
    /// instrument `ticket`, the unticked named. `close <n>` names what the
    /// work opened (`--opens <n>`, repeatable, or `--opens-nothing`)
    /// through the tracker's blocking relation, refusing while a box is
    /// unticked, while a successor is not open, and while neither flag is
    /// given, changing nothing at the tracker. No number is the branch's
    /// ticket, as `xtask track` reads it. The one way an agent closes a
    /// ticket.
    Ticket {
        #[command(subcommand)]
        command: TicketCommand,
    },
    /// Render the named files' regions with `computed run` and say what it
    /// did, one line per file, for pre-commit to re-stage. Writes no reading.
    ///
    /// Each line is `written` or `unchanged`. Refuses a broken marker, a
    /// missing `computed` and an untrusted checkout, whose `computed trust`
    /// line is printed as a person's.
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
    /// to its `--EXPECT*--` section; one reading per subset, scoped to the
    /// crate whose subset it is.
    ///
    /// Without `--subset`, the per-commit lane the manifest composes.
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
    /// Apply a proposal to one of the harness's own files, the ones `permit`
    /// denies the agent, and only where it adds: every table, key, value and
    /// array-of-tables row the tracked file carries must stand unchanged in
    /// the proposal. One changed ceiling, one widened edge, one dropped rule
    /// and nothing is written.
    ///
    /// What a new row should say is a person's, settled on a ticket; this is
    /// how a settled row reaches the file.
    Steer {
        /// The file to write, relative to the workspace root: seams.toml,
        /// oracle.toml or docs/divergence.toml.
        file: String,
        /// The proposed whole file, judged against the tracked one.
        #[arg(long, value_name = "PATH")]
        from: PathBuf,
        /// Judge and write the reading, but leave the file as it is.
        #[arg(long)]
        dry_run: bool,
    },
    /// Hold the divergence registry to the manifest rule whose `check` names
    /// this instrument: an accepted row has a reason a person wrote.
    DivergenceReason,
    /// Lay one extension crate down at ext/<name> in the template topology,
    /// its line in `php_bundle`, ext/clippy.toml and its row in ext/AGENTS.md;
    /// refuses an existing one.
    ///
    /// The template: the facade as its one dependency, `extension()`, state
    /// on the value, a test through the capture sink and a witness.
    NewExt {
        /// The extension's name as php-src spells it, with a `[[crate]]` row.
        name: String,
    },
    /// Extract every `php` fence and its `expect` fence from a crate's
    /// rustdoc, run each under the oracle's recipe and hold it to what the
    /// fence pins. One reading per crate.
    ///
    /// Each snippet runs as `witness.php`; the kernels and natives without
    /// a witness are counted.
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

/// What is done to a track rather than asked of it.
#[derive(Debug, Subcommand)]
pub(crate) enum TrackCommand {
    /// Lay a track down: push `main` to `origin/<name>`, clone origin
    /// beside the repository detached at it, record the track and its
    /// model on the clone, build `xtask` there, add the console's row and
    /// print the trust line; refuses a branch that exists, on the clone
    /// or on origin, and a directory that does.
    New {
        /// The track's name, a branch not yet made, with no slash: it is
        /// the path prefix of its branches' names.
        name: String,
        /// The model the track's sessions run on, recorded on the clone
        /// as `track.model.session` and read by `xtask track`.
        #[arg(long, value_name = "NAME")]
        model: String,
        /// The model the sessions' subagents run on, recorded as
        /// `track.model.subagent`; the session's when not given.
        #[arg(long, value_name = "NAME")]
        subagent_model: Option<String>,
    },
    /// The ticket the track works next: the oldest open `ready-for-agent`
    /// ticket that no open ticket blocks, that carries no `track:<name>`
    /// label naming another track, and that no branch of this clone
    /// records; "none" when the frontier is empty. Reads the tracker
    /// through the ticket module and writes no reading.
    Next {
        /// The track, by name; default the branch's, `main` for a branch
        /// on none and for a detached `HEAD`, which a track's own clone is.
        #[arg(long, value_name = "NAME")]
        track: Option<String>,
        /// Print the answer as one JSON object.
        #[arg(long)]
        json: bool,
    },
    /// The track caught up with main: from inside the track's clone,
    /// `origin/main` merged into the track's branch, the branch pushed
    /// to origin, the clone re-detached at the new tip and `xtask` built
    /// there, so the track never runs a harness older than main's; one
    /// line says what moved. A merge that conflicts refuses, names the
    /// conflict, and changes neither the branch nor the clone. Writes
    /// no reading.
    Sync,
    /// The frontier until it empties, without a person: for each ticket
    /// `next` names, the track caught up with main first, its worktree
    /// laid down on `<track>/t<n>`, a headless session in it on the model
    /// the clone records, and when the session ends with the ticket
    /// closed, the track's branch fast-forwarded to the worktree's tip,
    /// pushed to origin and the worktree retired. A session that ends
    /// with the ticket open, or closed with the branch not ahead of the
    /// track, leaves the worktree standing for a person and the run goes
    /// on; a failure of the harness itself, a sync that refuses included,
    /// stops it, named. With `track.frontier=auto` recorded on the clone,
    /// "none" from `next` starts a `/frontier` session in the clone and
    /// the run goes on with what it filed; a frontier still empty after
    /// it ends the run listing the track's open `for-a-person` tickets.
    /// Prints one line per ticket naming the ticket, the
    /// commit and the outcome, then a closing line once any was worked.
    /// Writes one reading per step, scoped to the ticket, and one for the
    /// run; `xtask status --section track` renders the last run.
    Work {
        /// One step alone, a `/frontier` session before it when the clone
        /// records `track.frontier=auto` and the frontier is empty: the
        /// line printed, and exit 1 with the reason when the session
        /// leaves the ticket to a person.
        #[arg(long)]
        one: bool,
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
    /// Read as `read` does, then set the blocking edge to each ticket the
    /// work opened and close the issue at the tracker with a comment naming
    /// the commit and the successors; refuse while a box is unticked, while
    /// a successor is closed or missing, and without `--opens` or
    /// `--opens-nothing`.
    Close {
        /// The issue number; without it, the branch's recorded ticket.
        number: Option<u64>,
        /// The session the reading carries, the way a `permit` reading
        /// carries the hook's `session_id`; empty when none is named.
        #[arg(long = "session-id", value_name = "ID")]
        session_id: Option<String>,
        /// A ticket filed for what this work found and did not do; once
        /// per ticket. Each must exist and be open; whose it is, an agent's
        /// or a person's, is not held: the bar is that it was filed.
        #[arg(long = "opens", value_name = "N")]
        opens: Vec<u64>,
        /// The assertion that the work found nothing to file, recorded in
        /// the reading with the session that made it.
        #[arg(long = "opens-nothing")]
        opens_nothing: bool,
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
    /// The last run of `xtask track work`: one row per ticket it worked,
    /// with the outcome and the commit its branch is at, and the run's
    /// own row last.
    Track,
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
    /// The pin as this machine has it: the tree's variable and the
    /// promise's, each by name and whether it is set, the pin's path, and
    /// which of the two places the tree was found at. CI reads the names
    /// here and spells them nowhere (#36).
    Oracle,
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
