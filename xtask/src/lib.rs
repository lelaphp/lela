//! The harness for the third engine (`harness-design.md` §4): the seam
//! manifest and the oracle pin loaded by one reader, the oracle module that
//! owns every comparison and the divergence registry, the reading and its
//! store, the instruments, `permit`, the judge at the hook's call, `done`,
//! the definition of done, `track`, the branch's track and its base,
//! `vocab`, the glossary as a sensor, `ticket`, the closer, `phpt`, the
//! subset runner, `witness`, the rustdoc witnesses, `compile_identity`,
//! op-array identity against php's own dump, `crosscheck`, each
//! extension's rendering against php's own `--re`, and `status`, the one
//! renderer.
//!
//! What the crate exposes is what a caller of an instrument needs and
//! nothing beside it: [`run`], the binary's body; each instrument's
//! run function with the arguments it takes and the value it returns; the
//! data modules, [`manifest`], [`oracle`] with the pin, [`divergence`]
//! with the registry, [`reading`], and [`carried`], which of the manifest's
//! `feature` rows a build carries; and [`scratch`], the scratch tree
//! both sides of the harness's own tests make one of. Everything else is
//! crate-private, and a test that needs an internal is a unit test beside
//! it.
#![deny(unsafe_code)]

// The unit tests' own name for this crate, so the topics of `tests/common`
// they share with the integration tests name its items as those do.
#[cfg(test)]
extern crate self as xtask;
#[cfg(test)]
#[path = "../tests/common"]
mod common {
    pub mod phpt_tree;
    pub mod registry;
}

pub mod api_check;
mod batch;
pub mod bench;
pub mod budget;
mod built;
pub mod carried;
mod census_weight;
mod cfg;
pub mod cli;
pub mod clippy;
pub mod compile_identity;
pub mod computed;
pub mod crosscheck;
pub mod dispatch_census;
pub mod divergence;
pub mod done;
pub mod exit;
mod fold;
mod fuzz;
pub mod generated;
mod git;
mod glossary;
pub mod hostile;
mod hosts;
/// What a build of the harness judges by, digested: the seam manifest and
/// `xtask`'s own sources (harness-design.md §4.4). Shared with
/// `build.rs`, which includes the file rather than importing it.
mod inputs;
pub mod key;
mod layout;
mod licence;
mod lines;
mod lock;
mod makala;
pub mod manifest;
pub mod miri;
pub mod new_ext;
pub mod new_sapi;
pub mod oracle;
pub mod owner;
pub mod permit;
pub mod phpt;
pub mod port_map;
pub mod queue;
pub mod reading;
pub mod review;
mod rust;
pub mod scratch;
mod seat;
mod server;
pub mod sha256;
mod sources;
pub mod status;
pub mod steer;
mod sysv;
mod tally;
pub mod test;
pub mod ticket;
pub mod trace_dump;
pub mod track;
pub mod track_chains;
pub mod track_next;
pub mod track_work;
mod turn;
pub mod unsafe_audit;
pub mod usage;
pub mod vocab;
pub mod witness;
mod workloads;
pub mod worktree;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};

use cli::{
    Cli, Command, OracleCommand, ReviewCommand, TicketCommand, TrackCommand, UsageCommand,
    WorktreeCommand,
};
use divergence::Registry;
use manifest::Manifest;
use owner::SessionId;
use reading::{Outcome, Reading, Store};
use sources::Standing;

/// The workspace this binary runs in: the nearest directory holding a
/// `Cargo.toml` with a `[workspace]` table, from the working directory up,
/// else from `CARGO_MANIFEST_DIR` as cargo sets it for the process, else
/// the one it was built in. Cargo reuses one compiled binary for every
/// checkout sharing a target directory whose sources are no newer than it,
/// so a path baked in at compile time can name another checkout.
/// The working directory comes first, as it does for cargo finding a
/// workspace, since every process a cargo-run one starts inherits its
/// `CARGO_MANIFEST_DIR`, a session `track work` spawns and its hooks
/// among them; so a run from inside a fixture workspace measures that
/// fixture, as `cargo` run there would.
pub(crate) fn built_root() -> PathBuf {
    let workspace = |from: PathBuf| {
        from.ancestors()
            .find(|dir| {
                let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap_or_default();
                manifest.lines().any(|line| line.trim() == "[workspace]")
            })
            .map(Path::to_path_buf)
    };
    std::env::current_dir()
        .ok()
        .and_then(workspace)
        .or_else(|| std::env::var_os("CARGO_MANIFEST_DIR").and_then(|dir| workspace(dir.into())))
        .or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .map(Path::to_path_buf)
        })
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The text of the file at `path`, an error naming it where it cannot be
/// read: every instrument's read of a file it needs.
pub(crate) fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

/// What a command named `what` printed on its first stream, where it ran
/// and exited 0; the error carries its status and its second stream.
pub(crate) fn printed(out: std::io::Result<std::process::Output>, what: &str) -> Result<String> {
    let out = out.with_context(|| format!("running {what}"))?;
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("{what} exited {}: {}", out.status, said.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The TOML file at `path` as a `T`, an error naming it where it cannot be
/// read or is not one.
pub(crate) fn read_toml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    toml::from_str(&read(path)?).with_context(|| format!("{}", path.display()))
}

/// The text of the file at `path`, `None` where there is no such file, an
/// error naming it where it cannot be read.
pub(crate) fn read_if(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// The paths of the entries of the directory `dir`, none where there is
/// no such directory, an error naming it where it cannot be listed.
pub(crate) fn entries(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("listing {}", dir.display())),
    };
    let listing = || format!("listing {}", dir.display());
    entries
        .map(|entry| Ok(entry.with_context(listing)?.path()))
        .collect()
}

/// The file at `path` removed, where there is one, an error naming it
/// where it cannot be.
pub(crate) fn remove(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
            Err(err).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// The directory `dir` made, with every one above it that is missing, an
/// error naming it where it cannot be.
pub(crate) fn make_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))
}

/// The file at `path` opened to be locked, made where it is not there yet
/// and left as it stands where it is.
pub(crate) fn lock_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::File::options();
    options.read(true).write(true).create(true).truncate(false);
    options.open(path)
}

/// The value under `lock`, a poisoned one included: a thread that panicked
/// holding it leaves what it gathered as it was.
pub(crate) fn locked<T>(lock: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The directory the file at `path` goes in, made as [`make_dir`] makes
/// one, so the file can be written there.
pub(crate) fn make_parent(path: &Path) -> Result<()> {
    path.parent().map_or(Ok(()), make_dir)
}

/// `contents` written to the file at `path`, an error naming it where it
/// cannot be: every instrument's write of a file it lays down.
pub(crate) fn write(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

/// `xtask/` as cargo names it for the process running, a test's among
/// them, else the one in the workspace it runs in: what a test reads its
/// fixtures under, never a path baked in at compile time.
pub fn package_dir() -> &'static Path {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        std::env::var_os("CARGO_MANIFEST_DIR")
            .map_or_else(|| built_root().join("xtask"), PathBuf::from)
    })
}

/// The binary's whole body: the command line run, an error printed as
/// `xtask: <err>`, and the exit, which `exit::status` reads off the error.
pub fn run() -> ExitCode {
    let ran = command_line();
    oracle::run_ended();
    match ran {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask: {err:#}");
            ExitCode::from(exit::status(&err))
        }
    }
}

/// The command line, dispatched: one arm per instrument.
fn command_line() -> Result<()> {
    let cli = Cli::read();
    // Absolute where the line is read: a program runs in its test's own
    // directory, where a relative root names nothing.
    let root = match cli.root {
        Some(root) => std::path::absolute(&root)
            .with_context(|| format!("resolving --root {}", root.display()))?,
        None => built_root(),
    };
    // The judge at the tool call reads the manifest itself: a manifest it
    // cannot read is a refusal it records before it exits, and a manifest
    // loaded out here would take that exit before permit saw the call
    // (harness-design.md §4.4).
    if let Command::Permit {
        json,
        above_the_command_hook,
        after,
        bench: false,
        diff: false,
        ..
    } = &cli.command
    {
        return match (*after, *above_the_command_hook) {
            (true, _) => permit::after(&root, json.as_deref()),
            (false, true) => {
                permit::run(&root, json.as_deref(), permit::Adapter::AboveTheCommandHook)
            }
            (false, false) => permit::run(&root, json.as_deref(), permit::Adapter::CommandHook),
        };
    }
    if let Some(answered) = before_the_manifest(&root, &cli.command) {
        return answered;
    }
    let manifest = Manifest::load(&root)?;
    let store = Store::of_workspace(&root);
    let _seat = seat::of_command(&cli.command);
    match cli.command {
        ask @ Command::Status { .. } => status_command(&manifest, &store, &root, ask),
        // The judge itself returned above; what is left of `permit` are the
        // two callers that read the manifest as a whole.
        Command::Permit {
            bench: true,
            runs,
            bar,
            ..
        } => permit::run_bench(&root, &manifest, &store, runs, bar),
        Command::Permit { diff: true, .. } => permit::run_diff(&root, &manifest),
        Command::Permit { .. } => bail!("permit: the judge at the tool call is run above"),
        Command::Done { hook: true, .. } => {
            // A build apart from the tree exits 1 and judges no turn, so
            // the hook's next candidate does, as permit's walk.
            built::judges(&root, &root, &built_root())?;
            done::run_hook(&root, &manifest, &Registry::load(&root)?, &store);
            Ok(())
        }
        asked @ Command::Done { .. } => done::command(&root, &manifest, &store, asked),
        Command::Bench(ask) => bench_command(&root, &store, &ask),
        ask @ (Command::Budget { .. } | Command::UnsafeAudit { .. } | Command::ApiCheck { .. }) => {
            reading_the_tree(&root, &manifest, &store, &ask)
        }
        // The crates named, or with `--staged` the ones with a staged change.
        Command::Clippy { staged: true, .. } => {
            record(&store, &clippy::run_staged(&root, &manifest, &store)?)
        }
        Command::Clippy { crates, .. } => record(&store, &clippy::run(&root, &manifest, &crates)?),
        Command::Test { staged: true, .. } => {
            record(&store, &test::run_staged(&root, &manifest, &store)?)
        }
        Command::Test { crates, target, .. } => {
            record(&store, &test::run_on(&root, &manifest, &crates, target)?)
        }
        Command::Miri { crates } => record(&store, &miri::run(&root, &manifest, &crates)?),
        Command::Phpt(ask) => phpt_command(&root, &manifest, &store, ask),
        Command::PortMap { tree } => port_map::command(&root, &manifest, &store, tree.as_deref()),
        Command::CompileIdentity(ask) => compile_identity::command(&root, &manifest, &store, &ask),
        Command::TraceDump(ask) => trace_dump::command(&root, &manifest, &store, &ask),
        Command::Hostile => hostile::command(&root, &manifest, &store),
        Command::Fuzz(ask) => fuzz::command(&root, &manifest, &store, &ask),
        Command::Fold(ask) => fold::command(&root, &manifest, &store, &ask),
        Command::DispatchCensus(ask) => dispatch_census::command(&root, &manifest, &store, &ask),
        Command::Crosscheck { ext, staged } => {
            crosscheck_command(&root, &manifest, &store, ext, staged)
        }
        ask @ (Command::FpmCompare { .. } | Command::Soak { .. }) => {
            record(&store, &[makala::run(&root, &manifest, ask)?])
        }
        Command::Generate => generate_command(&root, &manifest),
        Command::DivergenceReason => divergence_command(&root, &manifest, &store),
        Command::NewExt { name } => said(new_ext::run(&root, &manifest, &name)),
        Command::NewSapi { name, binary } => {
            said(new_sapi::run(&root, &manifest, &name, binary.as_deref()))
        }
        Command::Witness(ask) => witness::command(&root, &manifest, &store, ask),
        Command::Vocab(ask) => vocab_command(&root, &manifest, &store, ask),
        Command::Ticket { command } => ticket_command(&root, &manifest, &store, command),
        Command::Usage {
            command: UsageCommand::Record { session, turn },
        } => usage::record(&root, &store, &session, turn),
        Command::Track { .. } => bail!("track: answered above"),
        Command::Regions { .. } => bail!("regions: answered above"),
        Command::Worktree { .. } => bail!("worktree: answered above"),
        Command::Steer { .. } => bail!("steer: answered above"),
        Command::Oracle { .. } => bail!("oracle: answered above"),
        Command::Hosts { .. } => bail!("hosts: answered above"),
        Command::Review { .. } => bail!("review: answered above"),
    }
}

/// What `new-ext` or `new-sapi` laid down, a file a line, and its notes.
fn said(laid: Result<new_ext::Laid>) -> Result<()> {
    let laid = laid?;
    for path in &laid.written {
        println!("wrote {path}");
    }
    for note in &laid.notes {
        println!("{note}");
    }
    Ok(())
}

/// `oracle propose`: the diff on stdout and the notes on stderr, and
/// nothing written, the pin read whatever its defects.
fn oracle_propose(root: &Path, keg: Option<&Path>, source: Option<&Path>) -> Result<()> {
    let proposal = oracle::propose(root, keg, source)?;
    for note in &proposal.notes {
        eprintln!("{note}");
    }
    print!("{}", proposal.diff);
    Ok(())
}

/// `steer`: one proposal judged against the harness's own file and
/// applied where it only adds, as one reading.
fn steer(root: &Path, store: &Store, file: &str, from: &Path, dry: bool) -> Result<()> {
    let manifest = steer::binding(root)?;
    record(store, &[steer::run(root, &manifest, file, from, dry)?])
}

/// What every instrument ends with: each reading printed as one line and
/// the set recorded, whose outcome is the exit.
pub(crate) fn record(store: &Store, readings: &[Reading]) -> Result<()> {
    recorded(store, readings)?.exit()
}

/// `xtask vocab`: the crates named, or with `--staged` every crate less
/// those whose reading stands for the commit, or with `--message`
/// the commit message alone.
fn vocab_command(root: &Path, manifest: &Manifest, store: &Store, ask: cli::Vocab) -> Result<()> {
    let cli::Vocab {
        crates,
        staged,
        message,
    } = ask;
    if let Some(file) = message {
        // A note, never a refusal: the reading is recorded as a
        // measurement, whose exit is not its status.
        let reading = vocab::message(root, manifest, &file)?;
        println!("{reading}");
        return store.record_measurement(std::slice::from_ref(&reading));
    }
    over_index(root, manifest, staged, |root, manifest| {
        let crates = if staged {
            let all: Vec<String> = manifest.crates.iter().map(|c| c.name.clone()).collect();
            let standing =
                Standing::of_every_crate(true, root, manifest, store, vocab::INSTRUMENT)?;
            let left = standing.unmeasured(&all);
            if left.is_empty() {
                return Ok(());
            }
            left
        } else {
            crates
        };
        vocab::run(root, manifest, store, &crates)?
            .exit_saying(|| "vocab: an avoided word is in use; the findings are above".to_owned())
    })
}

/// `budget`, `unsafe-audit` and `api-check`, three of the four pre-commit
/// runs over the tree before anything is built, `vocab` the fourth: each
/// over the index under `--staged` ([`over_index`]).
fn reading_the_tree(root: &Path, manifest: &Manifest, store: &Store, ask: &Command) -> Result<()> {
    match *ask {
        Command::Budget { staged } => {
            let measure =
                |root: &Path, manifest: &Manifest| budget::measure(root, manifest, store, staged);
            record(store, &over_index(root, manifest, staged, measure)?)
        }
        Command::UnsafeAudit { staged } => {
            let measure = |root: &Path, manifest: &Manifest| {
                unsafe_audit::measure(root, manifest, store, staged)
            };
            record(store, &over_index(root, manifest, staged, measure)?)
        }
        Command::ApiCheck { write, staged } => {
            over_index(root, manifest, staged, |root, manifest| {
                api_check::run(manifest, root, store, write, staged)
            })
        }
        _ => bail!("only budget, unsafe-audit and api-check read the tree here"),
    }
}

/// `measure` over the workspace, or with `staged` over what the index
/// holds: where the working tree holds what the commit does not
/// take, over the index laid down apart ([`sources::Laid`]) and the
/// manifest it carries, so the sources an instrument reads and the closure
/// it keys a crate's reading over are the commit's. The readings are
/// recorded in the workspace's store all the same.
fn over_index<T>(
    root: &Path,
    manifest: &Manifest,
    staged: bool,
    measure: impl FnOnce(&Path, &Manifest) -> Result<T>,
) -> Result<T> {
    let laid = if staged {
        sources::Laid::of(root)?
    } else {
        None
    };
    match &laid {
        Some(laid) => {
            println!(
                "the working tree holds what the commit does not take, so the index is laid down at {} and measured there",
                laid.root().display()
            );
            measure(laid.root(), &Manifest::load(laid.root())?)
        }
        None => measure(root, manifest),
    }
}

/// [`record`] without the exit: every reading printed and written, and
/// what they owe, for an instrument whose exit answers something else.
fn recorded(store: &Store, readings: &[Reading]) -> Result<Outcome> {
    for reading in readings {
        println!("{reading}");
    }
    store.record(readings)
}

/// `xtask ticket read`, `close` and `parent`: the ticket named or the
/// branch's, read through the `gh` on `PATH`, one reading recorded; a
/// close names what it opened before anything is fetched, and the
/// branch's track, read once here, says which label its successors carry.
/// `parent` measures a sheet against the cap and reads no criteria, so it
/// answers above the two that do. Each is taken in the session
/// [`ticket::dispatched`] resolves from what the call named.
fn ticket_command(
    root: &Path,
    manifest: &Manifest,
    store: &Store,
    command: TicketCommand,
) -> Result<()> {
    if let TicketCommand::Parent { number, session_id } = command {
        let sheet = ticket::ticket_of(root, Some(number))?;
        let session = ticket::dispatched(session_id.as_deref());
        let session = session.as_ref().map(SessionId::as_str);
        let reading = ticket::parent(root, &ticket::Gh::on_path(), sheet, session)?;
        return record(store, std::slice::from_ref(&reading));
    }
    if let TicketCommand::Merged { since } = command {
        let readings = ticket::merged(root, manifest, &ticket::Gh::on_path(), &since)?;
        return record(store, &readings);
    }
    let (act, ticket, session_id) = match command {
        TicketCommand::Read { number, session_id } => (
            ticket::Act::Read,
            ticket::ticket_of(root, number)?,
            session_id,
        ),
        TicketCommand::Close {
            number,
            session_id,
            opens,
            opens_nothing,
            withdrawn,
        } => {
            let ticket = ticket::ticket_of(root, number)?;
            let verdict = ticket::Verdict::given(withdrawn.as_deref())?;
            let successors = ticket::Successors::named(ticket, &opens, opens_nothing)?;
            (
                ticket::Act::Close(verdict, successors, ticket::Reach::of(root)?),
                ticket,
                session_id,
            )
        }
        TicketCommand::Ready {
            number,
            session_id,
            opens,
            opens_nothing,
            found,
            found_nothing,
        } => {
            let ticket = ticket::ticket_of(root, number)?;
            let successors = ticket::Successors::named(ticket, &opens, opens_nothing)?;
            let found = ticket::found::Found::named(found.as_deref(), found_nothing)?;
            (ticket::Act::Ready(successors, found), ticket, session_id)
        }
        TicketCommand::Parent { .. }
        | TicketCommand::Merged { .. }
        | TicketCommand::Brief { .. }
        | TicketCommand::View { .. } => {
            bail!("ticket parent, merged, brief and view: answered above")
        }
    };
    let asks_ready = matches!(act, ticket::Act::Ready(..));
    let gh = ticket::Gh::on_path();
    let session = ticket::dispatched(session_id.as_deref());
    let session = session.as_ref().map(SessionId::as_str);
    let reading = ticket::run(root, manifest, &gh, act, ticket, session)?;
    // What was said on the ticket since its work began, printed and never
    // a refusal (Round 3, Q16 (c)): a builder that comments on its own
    // ticket is as common as one that does not.
    if asks_ready {
        match ticket::view::newer_than_the_work(root, &gh, ticket) {
            Ok(said) => print!("{said}"),
            Err(err) => println!("{ticket}: the comments could not be read: {err:#}"),
        }
    }
    record(store, std::slice::from_ref(&reading))
}

/// `xtask phpt`: the subsets asked for, each run and each written.
fn phpt_command(root: &Path, manifest: &Manifest, store: &Store, ask: cli::Phpt) -> Result<()> {
    let registry = Registry::load(root)?;
    if let (Some(subset), false) = (&ask.subset, ask.upstream.is_empty()) {
        return phpt::upstream::compare(manifest, &registry, subset, &ask.upstream, ask.jobs);
    }
    let (since, per_commit) = (ask.baseline.base, ask.baseline.per_commit);
    let ask = phpt::Ask {
        subset: ask.subset,
        full: ask.full,
        lane: ask.jit,
        engine: ask.binary.engine(),
        census: ask.census,
        against: ask.against,
        jobs: ask.jobs,
        locks: None,
        unkept: ask.unkept,
    };
    if let Some(since) = &since {
        return phpt::base::take(root, manifest, &registry, &ask, since, per_commit);
    }
    let ran = phpt::run(root, manifest, &registry, &ask)?;
    let outcome = recorded(store, &ran.readings)?;
    // A subset that was to run and did not leaves the run short, which no
    // judgement of the rest can make whole: that is the exit, written
    // after the readings that ran.
    tally::whole(phpt::INSTRUMENT, &ran.untaken)?;
    // `--against` asks whether a test met at the commit fails here, and
    // the exit is that answer; the subsets' own readings are written as
    // ever and hold what they always held.
    if ask.against.is_some() {
        return phpt::against::answer(&ran.readings);
    }
    outcome.exit()
}

/// `xtask generate`: the files the manifest's `generated` rules render,
/// each written and each named. A function of the shape `bench` and
/// `divergence-reason` have, so the dispatch stays a list of one arm per
/// instrument as its ceiling tightens.
fn generate_command(root: &Path, manifest: &Manifest) -> Result<()> {
    for path in generated::write_all(root, manifest)? {
        println!("wrote {}", path.display());
    }
    Ok(())
}

/// `xtask bench`: the bars asked for, each measured under the protocol and
/// each written; the exit is 0 whatever a reading says. A GitHub-hosted
/// runner asked as anything but a hosted machine is refused before
/// anything is measured.
fn bench_command(root: &Path, store: &Store, ask: &cli::Bench) -> Result<()> {
    let environment = std::env::var(bench::RUNNER_ENVIRONMENT).ok();
    bench::refuse_a_hosted_runner(environment.as_deref(), ask.machine)?;
    bench::run(
        root,
        store,
        ask.bar.as_deref(),
        ask.from.as_deref(),
        (ask.reps, ask.machine),
        (ask.label, ask.note.as_deref().unwrap_or_default()),
        ask.log.as_deref(),
    )
}

/// `xtask crosscheck`: the extensions asked for, each rendered by the
/// oracle and held against the engine's rendering, each written, and the
/// exit the tally's rule gives.
fn crosscheck_command(
    root: &Path,
    manifest: &Manifest,
    store: &Store,
    ext: Vec<String>,
    staged: bool,
) -> Result<()> {
    let registry = Registry::load(root)?;
    let ext = if staged {
        // The carried extensions whose rendering reads a staged file and
        // no reading stands for: what `done` holds to its row,
        // and an engine crate's change reaches every one.
        let carried = crosscheck::extensions(
            manifest,
            &carried::Carried::binary(root)?,
            &crosscheck::Ask::default(),
        );
        let touched = crosscheck::staged(root, manifest, store, &carried)?;
        if touched.is_empty() {
            println!(
                "crosscheck: no extension the binary carries renders again for what is staged"
            );
            return Ok(());
        }
        touched
    } else {
        ext
    };
    let ask = crosscheck::Ask {
        ext,
        ..crosscheck::Ask::default()
    };
    crosscheck::run(root, manifest, &registry, store, &ask)?.record(store, crosscheck::INSTRUMENT)
}

/// `xtask divergence-reason`: the registry against the rule whose `check`
/// names it. A function of the shape `phpt` and `status` have, so that the
/// dispatch above stays a list of one arm per instrument: it is at clippy's
/// hundred-line ceiling, and `witness` is the arm that tipped it.
fn divergence_command(root: &Path, manifest: &Manifest, store: &Store) -> Result<()> {
    let registry = Registry::load(root)?;
    let reading = divergence::reason(&registry, manifest, root)?;
    store
        .record(std::slice::from_ref(&reading))?
        .exit_saying(|| reading.message.clone())?;
    println!("{}", reading.message);
    Ok(())
}

/// The commands that measure nothing and read no manifest, answered
/// before `Manifest::load`: the track, so a worktree whose manifest is
/// unreadable can still say its branch, base and ticket; the two halves of a
/// worktree, whose manifest is the worktree's, once built there, and is
/// gone once it is retired; the regions, which pre-commit renders
/// whatever the manifest says; and the hosts, which a workspace builds by
/// its `Cargo.toml` alone, in the seat the command takes, or which
/// `--list` prints from the host table, building nothing and seatless.
fn before_the_manifest(root: &Path, command: &Command) -> Option<Result<()>> {
    match command {
        Command::Track {
            command: Some(TrackCommand::Next { track, json, why }),
            ..
        } => Some(track_next::command(root, track, *json, *why)),
        Command::Track {
            command: Some(TrackCommand::Chains { track, krate, json }),
            ..
        } => Some(track_chains::command(root, track, krate.as_deref(), *json)),
        Command::Track {
            command: Some(TrackCommand::Work { one, run }),
            ..
        } => Some(track_work::command(root, run, *one)),
        Command::Track { branch, json, .. } => Some(track_command(root, branch.as_deref(), *json)),
        Command::Regions { staged, files } => Some(computed::command(root, files, *staged)),
        Command::Review { command } => Some(match command {
            ReviewCommand::Record { file } => review::record(root, file),
            ReviewCommand::Resolve { scope, act } => review::resolve(root, scope, act),
            ReviewCommand::Cover { merge } => review::cover(root, merge.as_deref()),
            ReviewCommand::Class { head } => review::class(root, head),
        }),
        Command::Steer {
            file,
            from,
            dry_run,
        } => Some(steer(
            root,
            &Store::of_workspace(root),
            file,
            from,
            *dry_run,
        )),
        Command::Oracle {
            command: OracleCommand::Propose { keg, source },
        } => Some(oracle_propose(root, keg.as_deref(), source.as_deref())),
        Command::Hosts { profile, list } => Some(hosts::answer(root, *profile, *list)),
        Command::Worktree {
            command: WorktreeCommand::Add { branch, ticket },
        } => Some(
            worktree::add(
                root,
                branch,
                ticket.map(track::Ticket::new),
                SessionId::current().as_ref(),
            )
            .map(|_| ()),
        ),
        Command::Worktree {
            command: WorktreeCommand::Retire { branch },
        } => Some(worktree::retire(root, branch)),
        Command::Worktree {
            command: WorktreeCommand::Own { session, release },
        } => Some(
            session
                .as_deref()
                .map(|id| SessionId::new(id).context("--session names no session"))
                .transpose()
                .and_then(|named| {
                    worktree::own(root, SessionId::current().as_ref(), named, *release)
                }),
        ),
        Command::Worktree {
            command: WorktreeCommand::Whose,
        } => Some(worktree::whose(root, SessionId::current().as_ref())),
        Command::Worktree {
            command: WorktreeCommand::Queue,
        } => Some(queue::claim(root, SessionId::current().as_ref()).map(|_| ())),
        Command::Worktree {
            command: WorktreeCommand::Land,
        } => Some(queue::land(root).inspect(|()| review::after_land(root))),
        Command::Worktree {
            command: WorktreeCommand::Judge { base },
        } => Some(queue::judge::run(root, base)),
        Command::Ticket {
            command:
                TicketCommand::Brief {
                    number,
                    scratch,
                    owns,
                    spec,
                },
        } => Some(brief_command(
            root,
            *number,
            scratch.as_deref(),
            owns,
            spec.as_deref(),
        )),
        Command::Ticket {
            command: TicketCommand::View { number, since },
        } => Some(ticket::ticket_of(root, *number).and_then(|ticket| {
            let shown = ticket::view::view(root, &ticket::Gh::on_path(), ticket, since.as_deref())?;
            print!("{shown}");
            Ok(())
        })),
        _ => None,
    }
}

/// `xtask ticket brief`: the brief printed whole, by this build only
/// where it is the tree's; writes no reading.
fn brief_command(
    root: &Path,
    number: u64,
    scratch: Option<&Path>,
    owns: &[String],
    spec: Option<&str>,
) -> Result<()> {
    let ask = ticket::brief::Ask {
        scratch: scratch.map(Path::to_path_buf),
        owns: owns.to_vec(),
        spec: spec.map(str::to_owned),
    };
    let gh = ticket::Gh::on_path();
    let brief = ticket::brief::brief(root, &built_root(), &gh, track::Ticket::new(number), &ask)?;
    print!("{brief}");
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
/// facts for the session-start hook, whose stdin head names the session;
/// with `--every`, one row per reading of the section, from the store's
/// log or the one `--log` hands it (harness-design.md §4.6).
fn status_command(manifest: &Manifest, store: &Store, root: &Path, ask: Command) -> Result<()> {
    let Command::Status {
        section,
        format,
        session_id,
        uncut,
        tracked,
        every,
        log,
        session: _,
    } = ask
    else {
        bail!("status: handed another command");
    };
    let text = match section {
        Some(section) if every => status::every::render(store, section, format, log.as_deref())?,
        Some(section) => {
            let registry = Registry::load(root)?;
            let against = if tracked {
                status::Against::Tree
            } else {
                status::Against::Checkout(store)
            };
            status::render(
                manifest,
                &registry,
                against,
                root,
                section,
                format,
                session_id.as_deref(),
                uncut,
            )?
        }
        None => {
            let id = session_id.or_else(status::session_of_stdin);
            status::session(manifest, store, root, id.as_deref(), format)?
        }
    };
    std::io::stdout().write_all(text.as_bytes())?;
    Ok(())
}
