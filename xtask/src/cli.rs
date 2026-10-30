//! The command line. `xtask --help` is the list of instruments
//! (harness-design.md §5).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::phpt::Lane;

#[derive(Debug, Parser)]
#[command(
    name = "xtask",
    about = "The harness for the third engine",
    disable_help_subcommand = true
)]
pub(crate) struct Cli {
    /// The workspace root; defaults to the workspace this binary runs in.
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
        #[arg(long, conflicts_with = "staged")]
        write: bool,
        /// For pre-commit: leave out each crate whose reading passes and
        /// was taken over what is staged, naming the reading (#1143).
        #[arg(long)]
        staged: bool,
    },
    /// Render one section from the manifest, registry, store, repository
    /// and pinned tree; computes nothing else.
    ///
    /// What it reads is harness-design.md §4.6's: the manifest, the
    /// divergence registry and the store, and what the repository and the
    /// pinned tree hold on this machine (the `git`, `oracle` and `parity`
    /// sections), only ever reading them: never the tracker or the network,
    /// never a build or a run. With `--session` it prints the map and the
    /// local facts in place of a section.
    Status {
        #[arg(long, required_unless_present = "session", conflicts_with = "session")]
        section: Option<Section>,
        #[arg(long, default_value = "md")]
        format: Format,
        /// "This session" of the `permits` section: the session whose
        /// denials are counted apart. `--session` takes it from the hook's
        /// stdin head instead.
        #[arg(long = "session-id", value_name = "ID")]
        session_id: Option<String>,
        /// Every row. Without it, what the local facts render: the
        /// divergence section is the counts and the `open` rows alone, their
        /// reasons cut to the message width, `budget`, `phpt`, `witness`
        /// and `mutants` render the rows a session acts on and count the
        /// passing rest in one line, `permits` names a rule's newest
        /// sessions and counts the rest, and `rules` leaves out the text a
        /// rule's failure prints, as the map carries it. With it, every
        /// divergence row with its reason whole, as `docs/divergence.md`
        /// shows the registry, every row of the other four, every session of
        /// `permits` and every rule's text.
        #[arg(long)]
        uncut: bool,
        /// Render against the tree alone, as a tracked region carries the
        /// section: no reading is read and a table drops its `reading`
        /// column, so the bytes are the same on any checkout (#2377); a
        /// section whose rows are this checkout's alone is refused
        /// (harness-design.md §5, revision 6).
        #[arg(long, requires = "section", conflicts_with_all = ["session_id", "every"])]
        tracked: bool,
        /// Every reading of a section that keeps one per scope, in the
        /// log's order, rather than the one that stands, as
        /// `--format jsonl`: one line per reading carrying `v`, the version
        /// of its shape, which a reader refuses when it does not know it.
        /// Built for phpt (harness-design.md §4.6).
        #[arg(long, requires = "section", conflicts_with_all = ["session_id", "uncut"])]
        every: bool,
        /// The log `--every` renders in place of the store's: another
        /// store's log file or the published history, whose lean rows
        /// `{i,s,t,a,c,n}` read too; `-` is the standard input. A line that
        /// is no reading is counted on stderr and skipped.
        #[arg(long, value_name = "FILE", requires = "every")]
        log: Option<PathBuf>,
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
        /// The `PostToolUse` hook's call: hold the `Bash` call on stdin to
        /// what was taken before it, put back each of `harness.self`'s files
        /// it moved, other than what `steer` wrote, and deny it there.
        #[arg(long, conflicts_with_all = ["bench", "diff", "above_the_command_hook"])]
        after: bool,
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
    /// The definition of done: the conditions unmet since the base, and
    /// exit 1 while any is; the Stop hook, pre-push and CI read the list.
    Done {
        /// Measure the changes since the merge base with this ref;
        /// default `main`, or `origin`'s where the checkout has none.
        #[arg(long, value_name = "REF", conflicts_with = "hook")]
        base: Option<String>,
        /// Read the Stop hook's stdin: exit 0 when `stop_hook_active`,
        /// otherwise print the block JSON with the list while anything is unmet.
        #[arg(long)]
        hook: bool,
        /// An instrument this run did not take: a missing reading of it is
        /// a note that it did not run, never unmet and never met. CI names
        /// the oracle job's two while that job is skipped (#2477).
        /// An `[[owed]]` row names it, or the run is refused.
        #[arg(long = "not-run", value_name = "INSTRUMENT", conflicts_with = "hook")]
        not_run: Vec<String>,
        /// Who asks, which the reading carries (#3619); CI where
        /// `GITHUB_ACTIONS` is set, a hand otherwise. The hook's is the
        /// event it came in on.
        #[arg(long, value_enum, conflicts_with = "hook")]
        caller: Option<crate::done::Caller>,
    },
    /// Say the branch, the commit it was taken off main at, the store
    /// and the branch's ticket; as text or as one object. Writes no
    /// reading.
    ///
    /// A track is a `track:<name>` label on the tracker, `main` for a
    /// ticket with none, and has no branch (ADR-0009), so none is
    /// printed. The base is the merge base with `main`, `origin`'s where
    /// the checkout has none; the store is the main checkout's.
    ///
    /// `next --track <name>` names the ticket the track works next, the
    /// widest open `ready-for-agent` ticket that is unblocked, the
    /// track's and on no branch; `chains` prints every open ticket of the
    /// track under the one that blocks it. `work --model <name>` takes
    /// those tickets from start to retire without a person.
    #[command(args_conflicts_with_subcommands = true)]
    Track {
        /// A branch other than `HEAD`'s, made or not yet: what the add
        /// command asks before it cuts the branch from `main`.
        #[arg(long, value_name = "NAME")]
        branch: Option<String>,
        /// Print the same facts as one JSON object.
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        command: Option<TrackCommand>,
    },
    /// A ticket's worktree: `add` lays one down at <checkout>-worktrees/<branch>,
    /// `retire` appends its log into the store and removes it. Writes no
    /// reading.
    ///
    /// `add` lays the worktree down ready to be worked in, the branch cut
    /// from `main` with the ticket it is for recorded on it, `xtask` built
    /// there, the local file laid and rendered under the grant over the
    /// worktrees' directory (#74, §11); refuses a branch that exists and
    /// a directory that does. `retire` appends the worktree's log into the
    /// main checkout's store, removes the worktree and prints the
    /// branch-delete line as a person's; refuses uncommitted work.
    Worktree {
        #[command(subcommand)]
        command: WorktreeCommand,
    },
    /// A ticket, one GitHub issue: `read <n>` holds it to its acceptance
    /// criteria, `close <n>` closes it naming what it opened, `parent <n>`
    /// says whether a chart sheet is at the cap.
    ///
    /// `read <n>` writes the reading of the boxes under the ticket's
    /// `## Acceptance criteria` heading, and `parent <n>` the reading of
    /// one chart sheet's sub-issues against GitHub's cap of 100, before a
    /// ticket is filed against it.
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
    /// What a session spent per turn: `record` reads the plugin's
    /// `SessionUsage` on stdin and writes one `usage` reading under the
    /// branch's ticket, or the session id
    ///
    /// The object is what `$.session.usage()` answered, and the plugin's
    /// `turn.complete` pipes it here once a turn, so the last reading
    /// under a scope is the session's final figure; `track work` joins it
    /// to the ticket's own reading. The scope is the ticket recorded on
    /// the branch where there is one and the session id otherwise, so an
    /// interactive session on `main` still shows up. Usage is measured,
    /// never judged: the reading passes whatever the numbers say.
    Usage {
        #[command(subcommand)]
        command: UsageCommand,
    },
    /// Render the named files' regions with `computed run` and say what it
    /// did, one line per file, for pre-commit to re-stage. Writes no reading.
    ///
    /// Each line is `written` or `unchanged`. Refuses a broken marker, a
    /// missing `computed` and an untrusted checkout, whose `computed trust`
    /// line is printed as a person's.
    Regions {
        /// For pre-commit: refuse, before rendering, a region the commit
        /// would carry, one it would write or one of a file the commit
        /// stages, that reads a path whose working copy is not what the
        /// commit stages, since its render carries that change (#2236).
        #[arg(long)]
        staged: bool,
        /// The files, relative to the root.
        #[arg(required = true, value_name = "FILE")]
        files: Vec<String>,
    },
    /// Measure one bar under the protocol: paired reps with the A/A
    /// control and the load average, one reading per bar, exit 0 always.
    ///
    /// Without an argument, every bar the harness declares.
    Bench(Bench),
    /// Hold every crate to its line ceiling and the workspace to [budget];
    /// refuse committed generated code, a stale generated file and a
    /// `build.rs` that bakes its crate's directory in.
    Budget {
        /// For pre-commit: leave out each crate whose reading passes and
        /// was taken over what is staged, naming the reading (#1143).
        #[arg(long)]
        staged: bool,
    },
    /// `cargo clippy` with every warning denied, on every target of each
    /// crate and in release where it names `debug_assertions`, as the
    /// readings `done` reads; one per crate and profile.
    Clippy {
        /// Only these crates, by manifest name; default every crate that exists.
        #[arg(long = "crate", value_name = "NAME", conflicts_with = "staged")]
        crates: Vec<String>,
        /// Only the crates with a staged change (pre-commit's touched crates).
        #[arg(long)]
        staged: bool,
    },
    /// `cargo test` on each crate, as the reading `done` reads; one
    /// reading per crate, with what passed, failed, was ignored and was
    /// filtered out in its numbers.
    Test {
        /// Only these crates, by manifest name; default every crate that exists.
        #[arg(long = "crate", value_name = "NAME", conflicts_with = "staged")]
        crates: Vec<String>,
        /// Only the crates with a staged change (pre-commit's touched crates).
        #[arg(long)]
        staged: bool,
    },
    /// `cargo miri test` on `php_heap`, or on the crates named, as the
    /// lane design §9 runs per commit; one reading per crate, with what
    /// ran, failed and was ignored in its numbers.
    Miri {
        /// Only these crates, by manifest name; default the crates the
        /// manifest's `[[owed]]` row of `miri` names, `php_heap`.
        #[arg(long = "crate", value_name = "NAME")]
        crates: Vec<String>,
    },
    /// Run the pinned `.phpt` corpus through the engine and hold each test
    /// to its `--EXPECT*--` section; one reading per subset, scoped to the
    /// crate whose subset it is.
    ///
    /// Without `--subset`, the per-commit lane the manifest composes.
    Phpt(Phpt),
    /// Join the Mahout census to `api.lock`: each item `same`, `renamed`
    /// or `gone`, the rows not yet taken as the number; one reading, at the
    /// lock's crate.
    ///
    /// Without `--tree`, the committed census `data/port-map.v0.toml`.
    PortMap {
        /// Regenerate the census against the inherited Mahout tree, which
        /// lives outside this repository, and join that instead; a tree
        /// the census cannot read is a skip of the regeneration and never
        /// a pass.
        #[arg(long, value_name = "PATH")]
        tree: Option<PathBuf>,
    },
    /// Compile a corpus under the oracle's dump recipe and under the
    /// engine, and hold every file to byte-identity; one reading per
    /// corpus, scoped to the corpus.
    ///
    /// Without an argument, the per-commit corpus of design §6.1: every
    /// file of `Zend/tests`, `tests` and `sapi/cli` of the pinned php-src.
    CompileIdentity(crate::compile_identity::Ask),
    /// Hold each default extension's `--re` and `--ri` rendering to the
    /// oracle's bytes, kept in the store; one reading per extension per flag.
    ///
    /// The oracle's bytes are kept at
    /// `target/harness/crosscheck/ext/<name>/<digest>.re` and `.ri`; the
    /// `--re` reading is scoped `ext/<name>`, the `--ri` one
    /// `ext/<name>/ri`, and each names its invocation. The engine's side
    /// is `echo new ReflectionExtension('<name>');` and `(new
    /// ReflectionExtension('<name>'))->info();` through the linked engine,
    /// so an extension whose declarations or MINFO table are not the
    /// oracle's fails naming the first differing line, and one the engine
    /// cannot render fails in the engine's own words.
    Crosscheck {
        /// One extension by its php name, repeatable; default every
        /// extension the binary under test carries.
        #[arg(long = "ext", value_name = "NAME", conflicts_with = "staged")]
        ext: Vec<String>,
        /// Before a commit: only the extensions the binary carries whose
        /// rendering reads a staged file, an engine crate's reaching
        /// every one, less those whose two readings pass over what is
        /// staged, naming the readings (#1312, #1159). Pre-commit ran it
        /// until revision 140 and runs it no more.
        #[arg(long)]
        staged: bool,
    },
    /// Hold the engine's `TRACE` lines under `jit.mode=force` to the
    /// oracle's under `[regime.jit-inline]` on the seven kernels, kept in
    /// the store; one reading per kernel.
    ///
    /// Both sides' `---- TRACE` lines are kept at
    /// `target/harness/trace-dump/<kernel>/<digest>.oracle` and `.engine`,
    /// held through the registry under `trace:dump/<kernel>`; the engine's
    /// side is `php` built with the tier's feature.
    TraceDump(crate::trace_dump::Ask),
    /// Hold the release `serve_frame` to LLVM's fold of a request field's
    /// address into each access; one reading, at `sapi/cli`.
    ///
    /// Each instantiation is disassembled, and one that reaches a field
    /// through an address held apart from the request's register fails.
    /// The fold is what `.cargo/config.toml`'s LLVM
    /// `--cgp-max-address-users-to-scan=1000` buys every profile (design
    /// §10 Profiles, #3372), and the sensor is how a toolchain that keeps
    /// the option's name and changes its meaning is found: `done` asks for
    /// a fresh passing reading on a diff that changes the toolchain file or
    /// `.cargo/config.toml`. A site is an `add xN, <request>, #C` whose
    /// sum the next instruction naming `xN` loads or stores through at
    /// an offset `[<request>, #C + offset]` could encode, the shape #3345
    /// found; an `aarch64` Mac alone, a skip elsewhere.
    Fold(crate::fold::Ask),
    /// Hold makala to the pinned php-fpm: each request of the HTTP-served
    /// subset served by both, the status, header lines and body compared;
    /// one reading, at makala's crate.
    ///
    /// php-fpm is handed each request under the pin's `[fastcgi]` mapping,
    /// as nginx in front of it would hand it on (#2349); a machine without
    /// the pinned php-fpm and php-src measures nothing, stated as a skip.
    FpmCompare {
        /// One test of the subset by its path in the pinned tree,
        /// repeatable; default every one.
        #[arg(long = "test", value_name = "PATH")]
        tests: Vec<String>,
    },
    /// Many clients against one makala for a fixed time, each response held
    /// to the request served alone; one reading, at makala's crate.
    ///
    /// The reading counts the requests served, mismatches, leak findings
    /// and memory growth. A bare call is the 30-second smoke; `--nightly`
    /// is the nightly lane's ten minutes, #2336's ruling, which runs in CI
    /// only (#2632).
    Soak {
        /// The nightly lane's length, ten minutes, in place of the smoke's
        /// 30 seconds.
        #[arg(long)]
        nightly: bool,
        /// How long the clients send, in seconds, over either length.
        #[arg(long)]
        seconds: Option<u64>,
        /// How many clients send at once.
        #[arg(long, default_value_t = crate::makala::soak::CLIENTS)]
        clients: usize,
    },
    /// Allow `unsafe` only in the homes the manifest's unsafe.homes rule names.
    UnsafeAudit {
        /// For pre-commit: leave out each crate whose reading passes and
        /// was taken over what is staged, naming the reading (#1143).
        #[arg(long)]
        staged: bool,
    },
    /// Write the files the manifest's `generated` rules render (ext/clippy.toml).
    Generate,
    /// Apply a proposal to one of the harness's own files, and only where it
    /// adds: one changed value writes nothing.
    ///
    /// The files are the ones `permit` denies the agent, and the judgement is
    /// that every value the tracked file carries stands unchanged in the
    /// proposal, at the path and in the order it stood in; one changed
    /// ceiling, one widened edge, one dropped rule, one dropped key, one row
    /// moved past another and nothing is written. A key a table or an
    /// array-of-tables row does not carry is an addition, and every addition
    /// is named in the reading. What a new row should say is a person's,
    /// settled on a ticket; this is how a settled row reaches the file.
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
    /// Lay one extension crate down at ext/<name> from the template, with
    /// its `php_bundle` line, ext/clippy.toml and ext/AGENTS.md row; refuses
    /// an existing one.
    ///
    /// The template: the facade as its one dependency, `extension()`, state
    /// on the value, a test through the capture sink and a witness.
    NewExt {
        /// The extension's name as php-src spells it, with a `[[crate]]` row.
        name: String,
    },
    /// Lay one host crate down at sapi/<name> as the CLI and CGI are laid,
    /// with its binary and test; refuses an existing one or one without its
    /// `[[crate]]` row (ADR-0008).
    ///
    /// The template: the facade and the bundle as its two dependencies,
    /// `main`, a host that serves the file its line names until its own
    /// replaces it, the build date's `build.rs` and a test of the binary.
    NewSapi {
        /// The host's name as php-src spells its directory, with a `[[crate]]` row.
        name: String,
        /// The binary's name; `php-<name>` where none is given.
        #[arg(long = "bin", value_name = "NAME")]
        binary: Option<String>,
    },
    /// Run every `php` fence of a crate's rustdoc under the oracle's recipe
    /// and hold it to its `expect` fence; one reading per crate.
    ///
    /// Each snippet runs as `witness.php`; the kernels and natives without
    /// a witness are counted.
    Witness(Witness),
    /// The glossary as a sensor: `_Avoid_` words in public item names and
    /// rustdoc prose, never in a lock; one reading per crate.
    Vocab {
        /// Only these crates, by manifest name; default every crate that exists.
        #[arg(long = "crate", value_name = "NAME", conflicts_with = "staged")]
        crates: Vec<String>,
        /// For pre-commit: leave out each crate whose reading passes and
        /// was taken over what is staged, naming the reading (#1143).
        #[arg(long)]
        staged: bool,
    },
}

/// What one `bench` run is asked for, flattened onto the command so the
/// dispatch has one line for the instrument as it has for every other.
#[derive(Debug, Args)]
pub(crate) struct Bench {
    /// Only this bar, by name; default every declared one.
    #[arg(long, value_name = "NAME")]
    pub bar: Option<String>,
    /// A bar read from a TOML file instead of the declared ones: the
    /// negative control under xtask/fixtures/bench is run this way.
    #[arg(long, value_name = "PATH", conflicts_with = "bar")]
    pub from: Option<PathBuf>,
    /// Measured reps per arm, after the protocol's discarded warm-up.
    #[arg(long, default_value_t = crate::bench::REPS, value_name = "N")]
    pub reps: usize,
    /// The standing the numbers are offered with (protocol §8).
    #[arg(long, value_enum, default_value = "unlabelled")]
    pub label: crate::bench::Label,
    /// The written reservation a provisional or baseline-of-record label
    /// owes.
    #[arg(long, value_name = "TEXT")]
    pub note: Option<String>,
    /// The log a bar held to a tolerance reads its earlier nights and its
    /// baseline from, in place of the store's: the published history the
    /// nightly lane fetches, since its store starts empty (#2659).
    #[arg(long, value_name = "FILE")]
    pub log: Option<PathBuf>,
}

/// What one `witness` run is asked for, flattened onto the command so the
/// dispatch has one line for the instrument as it has for every other.
#[derive(Debug, Args)]
pub(crate) struct Witness {
    /// Only these crates, by manifest name; default every crate that exists.
    #[arg(long = "crate", value_name = "NAME", conflicts_with = "staged")]
    pub crates: Vec<String>,
    /// Only the crates with a staged change (pre-commit's touched crates).
    #[arg(long)]
    pub staged: bool,
    /// Only the crates with no passing reading over their tree now, as
    /// `done` reads fresh: the merge check's, after pre-commit measured
    /// the staged crates (#1312).
    #[arg(long, conflicts_with_all = ["crates", "staged"])]
    pub unmeasured: bool,
    #[command(flatten)]
    pub accepting: Accepting,
    /// Only the fences in rustdoc under this source file, or the lines
    /// of a host crate's `tests/lines.toml` named here, from the
    /// workspace root or any tail of it; prints and writes no reading.
    #[arg(
        long,
        value_name = "PATH",
        requires = "crates",
        conflicts_with = "item"
    )]
    pub file: Option<PathBuf>,
    /// Only the fences on this item, by the name its declaration
    /// writes (`preg_quote`, a method's `add`), or a host's line by its
    /// `name`; prints and writes no reading.
    #[arg(long, value_name = "NAME", requires = "crates")]
    pub item: Option<String>,
    /// Serve the run in this process and append what it judged to
    /// this file, for the process watching it to write: how
    /// `witness` starts itself, never typed.
    #[arg(long, value_name = "PATH", hide = true)]
    pub journal: Option<PathBuf>,
}

/// What a `witness` run writes onto the fences (`witness::Accept`).
#[derive(Debug, Args)]
pub(crate) struct Accepting {
    /// Write the oracle's status, or the signal its run ended on, onto
    /// each `expect` fence line that does not already pin it; the only
    /// thing that writes an `exit` or a `signal`.
    #[arg(long)]
    pub accept: bool,
    /// With `--accept`, also write the oracle's two streams, normalised,
    /// into each fence marked `capture`, refusing a line that differs
    /// between two runs from two scratch directories (#801).
    #[arg(long, requires = "accept")]
    pub streams: bool,
    /// With `--accept`, also write each plain fence the oracle does not
    /// meet from the oracle's two runs, normalised, keeping each authored
    /// line whose wildcards the oracle's line still matches, and refusing
    /// a line the two runs differ on and a fence a `trace:` row covers
    /// (#3455).
    #[arg(long, requires = "accept", conflicts_with = "streams")]
    pub bodies: bool,
}

/// What one `phpt` run is asked for, flattened onto the command so the
/// dispatch has one line for the instrument as it has for every other.
#[derive(Debug, Args)]
pub(crate) struct Phpt {
    /// The subset: a crate by name (`php_engine`, `pcre`, `ext/pcre`),
    /// whose subsets its `[[crate]]` row owns, or a directory of the
    /// pinned php-src tree (`Zend/tests`). Without it, the per-commit
    /// lane: every subset the manifest's rows own.
    #[arg(long, value_name = "NAME", conflicts_with = "full")]
    pub subset: Option<String>,
    /// Every subset of the corpus: the nightly lane, which CI runs.
    #[arg(long)]
    pub full: bool,
    /// The compiled tier's lane: the engine runs under `jit.mode`.
    #[arg(long, value_name = "MODE")]
    pub jit: Option<Lane>,
    #[command(flatten)]
    pub binary: PhptEngine,
    /// The census beside the run: every missed test grouped by the
    /// signature its miss kept, and the tests the compiler refused grouped
    /// by the construct it refused them for, carried on each reading; each
    /// the widest first. One more rendering of the same lane; nothing runs
    /// twice and no count changes.
    #[arg(long)]
    pub census: bool,
    /// Hold each subset, test by test, to the run kept at this commit, the
    /// merge base as a rule: one more reading per subset, failing while a
    /// test met there is not met here, and the exit is those readings'.
    #[arg(long, value_name = "COMMIT")]
    pub against: Option<String>,
    #[command(flatten)]
    pub baseline: PhptBase,
    /// How many tests run at once: the machine's cores unless named, and
    /// one test at a time at 1. Tests that share a `--CONFLICTS--` key
    /// never run at once whatever the number.
    #[arg(long, value_name = "N")]
    pub jobs: Option<std::num::NonZeroUsize>,
    /// Under the runner's control, `--control` or `--engine` a pinned php:
    /// run only the subsets whose kept target does not stand, none kept
    /// under the pin or one that predates a test the subset has now, and
    /// say so of the rest.
    #[arg(long)]
    pub unkept: bool,
    /// php-src's own `run-tests.php` beside this runner, both under the
    /// pinned oracle, over these `.phpt` files of the subset only, each
    /// verdict printed beside the other; a test judged apart is the exit,
    /// and nothing is recorded. A directory, a glob or no file is refused
    /// (#1973).
    #[arg(
        long,
        value_name = "FILE",
        num_args = 1..,
        requires = "subset",
        conflicts_with_all = ["full", "jit", "engine", "control", "census", "against", "base", "unkept"]
    )]
    pub upstream: Vec<PathBuf>,
}

/// The selection of runs to recover at a published base.
#[derive(Debug, Args)]
pub(crate) struct PhptBase {
    /// Take the run `--against` holds each subset to at the merge base
    /// with this ref, `main` unless named, as `done --base` takes it: the
    /// subset named or every touched one the lane runs, run in a worktree
    /// laid at the base with its hosts built, the runs kept in the main
    /// worktree and the worktree removed (#2364).
    #[arg(
        long,
        value_name = "REF",
        num_args = 0..=1,
        default_missing_value = "main",
        conflicts_with_all = ["full", "jit", "engine", "control", "against", "unkept"]
    )]
    pub base: Option<String>,
    /// With `--base`, take every subset of the per-commit lane, including
    /// untouched crates. This does not select the nightly corpus.
    #[arg(long, requires = "base", conflicts_with_all = ["subset", "full"])]
    pub per_commit: bool,
}

/// The binary under test one `phpt` run asks for, at most one of the two.
#[derive(Debug, Args)]
pub(crate) struct PhptEngine {
    /// The binary the process adapter runs; without it, the engine's own
    /// where cargo puts it.
    #[arg(long, value_name = "PATH")]
    pub engine: Option<PathBuf>,
    /// The runner's control: each subset under its own oracle's pinned
    /// php, the main one or the variant its row names, as the binary
    /// under test, which keeps the subset's target under that pin.
    #[arg(long, conflicts_with_all = ["engine", "jit"])]
    pub control: bool,
}

impl PhptEngine {
    pub(crate) fn engine(self) -> crate::phpt::Engine {
        match (self.engine, self.control) {
            (Some(path), _) => crate::phpt::Engine::Named(path),
            (None, true) => crate::phpt::Engine::Control,
            (None, false) => crate::phpt::Engine::Built,
        }
    }
}

/// What is done to a track rather than asked of it.
#[derive(Debug, Subcommand)]
pub(crate) enum TrackCommand {
    /// The ticket the track works next: the widest open `ready-for-agent`
    /// ticket by the census that no ticket blocks, that carries no
    /// `track:<name>` label naming another track, and that no branch of
    /// the repository records; "none" when the frontier is empty. Reads the tracker
    /// through the ticket module and writes no reading.
    Next {
        /// The track, by its `track:` label.
        #[arg(long, value_name = "NAME", default_value = crate::track::MAIN)]
        track: String,
        /// Print the answer as one JSON object.
        #[arg(long)]
        json: bool,
        /// Say why this ticket and not another: its weight off the unread
        /// census, the rows behind it and when that census was taken.
        #[arg(long, conflicts_with = "json")]
        why: bool,
    },
    /// Every open ticket of the track under the ticket that blocks it,
    /// one call in place of a walk over `blockedBy`: a chain starts at a
    /// ticket no open ticket shown blocks and runs down through what it
    /// unblocks, a ticket with two blockers is written out under the
    /// first and named under the others, and the tickets with no edge
    /// follow on their own. Each line carries the ticket's standing as
    /// `next` judges it: `ready` or `a person's`, the branch recording
    /// it, the tickets it waits on. Reads the
    /// tracker through the ticket module and writes no reading.
    Chains {
        /// The track, by its `track:` label.
        #[arg(long, value_name = "NAME", default_value = crate::track::MAIN)]
        track: String,
        /// Keep the tickets whose title names this crate before its colon
        /// (`php_engine, ext/standard: ...`); `none` keeps the ones naming
        /// no crate, the manifest rows, design revisions and decisions.
        #[arg(long = "crate", value_name = "NAME")]
        krate: Option<String>,
        /// Print the rows as one JSON object, each with what it waits on
        /// and what it blocks.
        #[arg(long)]
        json: bool,
    },
    /// The frontier until it empties, without a person, from main's
    /// checkout: for each ticket `next` names on `--track`, its worktree
    /// laid down on `t<n>` off `main`, a headless session in it on
    /// `--model`, and when the session ends with the ticket closed, the
    /// worktree retired and the branch left for /specs to merge; `main`
    /// is never moved or pushed. A session that ends with the ticket
    /// open leaves the worktree standing for a person and the run goes
    /// on; a failure of the harness itself stops it, named. With `--fill`, "none" from
    /// `next` starts a `/frontier` session in the checkout and the run
    /// goes on with what it filed; a frontier still empty after it ends
    /// the run listing the track's open `for-a-person` tickets.
    /// Prints one line per ticket naming the ticket, the
    /// commit and the outcome, then a closing line once any was worked.
    /// Writes one reading per step, scoped to the ticket, and one for the
    /// run; `xtask status --section track` renders the last run.
    Work {
        /// One step alone, a `/frontier` session before it under `--fill`
        /// when the frontier is empty: the line printed, and exit 1 with
        /// the reason when the session leaves the ticket to a person.
        #[arg(long)]
        one: bool,
        #[command(flatten)]
        run: crate::track_work::Run,
    },
}

/// The two halves of a ticket's worktree.
#[derive(Debug, Subcommand)]
pub(crate) enum WorktreeCommand {
    /// Lay the worktree for a branch down, ready to be worked in, with
    /// the ticket its work is for recorded on the branch.
    Add {
        /// The branch, not yet made; it is cut from `main`.
        branch: String,
        /// The issue the branch's work is for, recorded on the branch and
        /// read by `xtask track`; one branch per ticket. Without it the
        /// branch records none, which `xtask track` says.
        #[arg(long, value_name = "N")]
        ticket: Option<u64>,
    },
    /// Append the worktree's log into the main checkout's store, then
    /// remove the worktree; the branch is kept.
    Retire {
        /// The branch a worktree is on.
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
    /// Read as `read` does, then record the ticket ready to merge on the
    /// branch with what the work opened, `branch.<name>.ready-<n>`, one per
    /// ticket (#2358), which the merge into `main` closes the ticket by
    /// (#2270, #2311); refuse while a box is unticked,
    /// while a successor is closed or missing, and without `--opens` or
    /// `--opens-nothing`. Nothing at the tracker changes.
    Ready {
        /// The issue number; without it, the branch's recorded ticket.
        number: Option<u64>,
        /// The session the reading carries, the way a `permit` reading
        /// carries the hook's `session_id`; empty when none is named.
        #[arg(long = "session-id", value_name = "ID")]
        session_id: Option<String>,
        /// A ticket filed for what this work found and did not do; once
        /// per ticket. Each must exist and be open.
        #[arg(long = "opens", value_name = "N")]
        opens: Vec<u64>,
        /// The assertion that the work found nothing to file.
        #[arg(long = "opens-nothing")]
        opens_nothing: bool,
    },
    /// Close each open ticket the merge commits since `--since` name as
    /// `#n`, with the successors its ready record names; one with no ready
    /// record fails, named, and `done` names it. What the hooks on a merge into `main`
    /// run (#2311); off `main` it closes nothing. On `main` it writes one
    /// reading more, `ticket merged`, naming each phase with its time: each
    /// close, the `gh` calls by verb, the read of the workspace and the run
    /// whole (#2813).
    Merged {
        /// The commit `main` stood at before the merge.
        #[arg(long, value_name = "REV")]
        since: String,
    },
    /// Read as `read` does, then set the blocking edge to each ticket the
    /// work opened and close the issue at the tracker with a comment naming
    /// the commit and the successors; refuse while a box is unticked, while
    /// a successor is closed or missing, while `main` does not contain the
    /// commit (#2249), and without `--opens` or `--opens-nothing`. With
    /// `--withdrawn <reason>` the ticket is closed as withdrawn rather than
    /// done, refused while any box is ticked.
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
        /// The ticket is withdrawn rather than done: it describes nothing
        /// in the tree, and the reason is the measurement that says so,
        /// what was run and where, which the reading and the closing
        /// comment carry. Refused while any box is ticked, since work half
        /// done is not a withdrawal.
        #[arg(long = "withdrawn", value_name = "REASON")]
        withdrawn: Option<String>,
    },
    /// Say how many sub-issues one chart sheet holds and whether the
    /// tracker will take another, before a ticket is filed against it;
    /// exit 1 at GitHub's cap of 100 and for a sheet that is closed.
    ///
    /// The count is `gh issue view --json subIssuesSummary`, written as a
    /// reading of instrument `ticket` under the scope `parent/<n>`.
    /// Nothing at the tracker is asked to change.
    Parent {
        /// The sheet's issue number.
        number: u64,
        /// The session the reading carries, the way a `permit` reading
        /// carries the hook's `session_id`; empty when none is named.
        #[arg(long = "session-id", value_name = "ID")]
        session_id: Option<String>,
    },
}

/// Record what one turn spent.
#[derive(Debug, Subcommand)]
pub(crate) enum UsageCommand {
    /// Read the `SessionUsage` object on stdin and write the turn's
    /// reading under the branch's ticket, or under the session id.
    Record {
        /// The session's id, the reading's scope on a branch that records
        /// no ticket and its `session` on every reading.
        #[arg(long, value_name = "ID")]
        session: String,
        /// The turn's number in the session, counted by the plugin.
        #[arg(long, value_name = "N")]
        turn: u64,
    },
}

/// The sections of harness-design.md §4.6 that have a renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Section {
    /// The manifest's rules, each with its ADR number, the predicate that
    /// judges it and its reading; the text its failure prints is under
    /// `--uncut` and in JSON (#1772).
    Rules,
    /// Each lock's width, as the tree's lock lists it, against
    /// `api_items`, with its api-check reading; `--every` renders every
    /// api-check reading, a crate's edges among them.
    #[value(alias = "api-check")]
    Locks,
    /// Each crate against its ceiling.
    Budget,
    /// The divergence registry, with counts by kind and status.
    Divergence,
    /// The phpt readings, one row per subset.
    Phpt,
    /// The witness readings, one row per crate.
    Witness,
    /// The test readings, one row per crate.
    Test,
    /// The mutants readings, one row per crate.
    Mutants,
    /// The bench readings, one row per bar.
    Bench,
    /// The port-map reading: the census joined to `api.lock`, with the
    /// rows the port has not taken.
    PortMap,
    /// The crosscheck readings, one row per extension.
    Crosscheck,
    /// The ticket readings, one row per ticket: its criteria, what closed
    /// it and the session.
    Ticket,
    /// The last 7 days of `ticket` readings in the main worktree's store
    /// and this checkout's: the closes, the successors they opened, the
    /// withdrawals among them and the successors opened per close, in
    /// one line (#1157).
    Tickets,
    /// The last run of `xtask track work`: one row per ticket it worked,
    /// with the outcome, what its session spent and the commit its branch
    /// is at, and the run's own row last with the usd total.
    Track,
    /// The usage readings, the last per scope: the session, the turn it
    /// reached, its tokens and its usd.
    Usage,
    /// Each rule permit judges, the manifest's and then the three permit
    /// owns, which no manifest row names: `computed.region-body`,
    /// `permit.stale-build` and `permit.unjudged-call`. Each row has its
    /// last finding, denial or note, the denials the log holds per session
    /// and in total, and beside `denials` the `handed on` column: the calls
    /// a stale build handed to the candidate that rebuilds, which judged
    /// them, so a walk is never counted as a refusal (#118, #125). Then the
    /// log per instrument, with the candidates for silent (the CI section
    /// of §7).
    Permits,
    /// The log and the status of the working tree; volatile.
    Git,
    /// The bundle's registration order against the crate graph.
    Bundle,
    /// The manifest's `[[crate]]` rows counted by the directory each is
    /// laid down in, and how many are behind a feature; under `--uncut`
    /// and in JSON every row, with whether it is created (#2250).
    Crates,
    /// The manifest's `[[edge]]` rows: each edge, how its row qualifies
    /// it and its ADRs (#2250).
    Edges,
    /// Each item lock's lines counted by the module that defines them,
    /// the widest first: the groups of the seam's api-check reading, which
    /// `locks` totals (#1795).
    Modules,
    /// The pin as this machine has it: the tree's variable and the
    /// promise's, each by name and whether it is set, the pin's path, tag
    /// and commit (#66), which of the two places the tree was found at,
    /// and the pinned binary. CI reads the names
    /// here and spells them nowhere (#36).
    Oracle,
    /// The corpus CI checks out at the pin: the tag, the commit, the
    /// `.phpt` files the pin counts, the sparse patterns the subsets need
    /// and the cache key they make, where the targets under the pin are
    /// kept, and the hosts the lane runs a test through. CI reads them
    /// here and writes no directory of the corpus and no host (#1713,
    /// #1733, #1908).
    Corpus,
    /// Met-of-target per suite and in total, from the phpt reading that
    /// stands for each suite parity counts, and the suites outside parity
    /// with the ruling that put them there or as not built (design §1,
    /// §12; #1761).
    Parity,
    /// The fields of `oracle::Invocation` past the binary and the
    /// program, read from the type's source: each one's type, whether a
    /// `.phpt` section adds it or none gives it, and the first sentence
    /// of its rustdoc; harness-design.md §4.3's region (#1319).
    Invocation,
    /// The conditions of done, each the first rustdoc paragraph of a
    /// variant of `done::Condition`, read from its source and numbered;
    /// harness-design.md §4.5's region (#2309).
    Conditions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Md,
    Json,
    /// One JSON line per reading, each carrying `v`, the version of its
    /// shape; `--every`'s alone.
    Jsonl,
}
