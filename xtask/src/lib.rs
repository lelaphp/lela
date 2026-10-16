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
//! nothing beside it (#55): [`run`], the binary's body; each instrument's
//! run function with the arguments it takes and the value it returns; the
//! data modules, [`manifest`], [`oracle`] with the pin, [`divergence`]
//! with the registry, [`reading`], and [`carried`], which of the manifest's
//! `feature` rows a build carries; and [`scratch`], the scratch tree
//! both sides of the harness's own tests make one of. Everything else is
//! crate-private, and a test that needs an internal is a unit test beside
//! it.
#![forbid(unsafe_code)]

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
pub mod divergence;
pub mod done;
pub mod generated;
mod git;
mod glossary;
/// What a build of the harness judges by, digested: the seam manifest and
/// `xtask`'s own sources (harness-design.md §4.4, #16). Shared with
/// `build.rs`, which includes the file rather than importing it.
mod inputs;
pub mod key;
mod layout;
mod lines;
mod lock;
mod makala;
pub mod manifest;
pub mod miri;
pub mod new_ext;
pub mod new_sapi;
pub mod oracle;
pub mod permit;
pub mod phpt;
pub mod port_map;
pub mod reading;
mod reply;
mod rust;
pub mod scratch;
mod sources;
pub mod status;
pub mod steer;
mod sysv;
mod tally;
pub mod test;
pub mod ticket;
pub mod track;
pub mod track_chains;
pub mod track_next;
pub mod track_work;
pub mod unsafe_audit;
pub mod usage;
pub mod vocab;
pub mod witness;
pub mod worktree;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::Parser;

use cli::{Cli, Command, TicketCommand, TrackCommand, UsageCommand, WorktreeCommand};
use divergence::Registry;
use manifest::Manifest;
use reading::{Outcome, Reading, Store};
use sources::Standing;

/// The workspace this binary runs in: the nearest directory holding a
/// `Cargo.toml` with a `[workspace]` table, from the working directory up,
/// else from `CARGO_MANIFEST_DIR` as cargo sets it for the process, else
/// the one it was built in. Cargo reuses one compiled binary for every
/// checkout sharing a target directory whose sources are no newer than it,
/// so a path baked in at compile time can name another checkout (#2719).
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

/// `xtask/` as cargo names it for the process running, a test's among
/// them, else the one in the workspace it runs in: what a test reads its
/// fixtures under, never a path baked in at compile time (#2719).
pub fn package_dir() -> &'static Path {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        std::env::var_os("CARGO_MANIFEST_DIR")
            .map_or_else(|| built_root().join("xtask"), PathBuf::from)
    })
}

/// The binary's whole body: the command line run, an error printed as
/// `xtask: <err>`, and the exit. A worktree laid down but not ready is
/// neither done nor undone: 3, so a caller can tell it from a refusal
/// that changed nothing, which is 1.
pub fn run() -> ExitCode {
    let ran = command_line();
    oracle::run_ended();
    match ran {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask: {err:#}");
            if err.downcast_ref::<worktree::NotReady>().is_some() {
                ExitCode::from(3)
            } else {
                ExitCode::from(1)
            }
        }
    }
}

/// The command line, dispatched: one arm per instrument.
fn command_line() -> Result<()> {
    let cli = Cli::parse();
    // Absolute where the line is read: a program runs in its test's own
    // directory, where a relative root names nothing (#942).
    let root = match cli.root {
        Some(root) => std::path::absolute(&root)
            .with_context(|| format!("resolving --root {}", root.display()))?,
        None => built_root(),
    };
    // The judge at the tool call reads the manifest itself: a manifest it
    // cannot read is a refusal it records before it exits, and a manifest
    // loaded out here would take that exit before permit saw the call
    // (#16, harness-design.md §4.4).
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
            done::run_hook(&root, &manifest, &Registry::load(&root)?, &store);
            Ok(())
        }
        Command::Done { base, not_run, .. } => {
            let registry = Registry::load(&root)?;
            let base = base.as_deref();
            done::run(&root, &manifest, &registry, &store, base, &not_run)
        }
        Command::Bench(ask) => bench_command(&root, &store, &ask),
        Command::Budget { staged } => {
            record(&store, &budget::measure(&root, &manifest, &store, staged)?)
        }
        Command::Clippy { crates, staged } => {
            clippy::command(&root, &manifest, &store, &crates, staged)
        }
        Command::Test { crates, staged } => {
            test::command(&root, &manifest, &store, &crates, staged)
        }
        Command::Miri { crates } => miri::command(&root, &manifest, &store, &crates),
        Command::Phpt(ask) => phpt_command(&root, &manifest, &store, ask),
        Command::PortMap { tree } => port_map::command(&root, &manifest, &store, tree.as_deref()),
        Command::CompileIdentity(ask) => identity_command(&root, &manifest, &store, ask),
        Command::Crosscheck { ext, staged } => {
            crosscheck_command(&root, &manifest, &store, ext, staged)
        }
        ask @ (Command::FpmCompare { .. } | Command::Soak { .. }) => {
            record(&store, &[makala::run(&root, &manifest, ask)?])
        }
        Command::UnsafeAudit { staged } => record(
            &store,
            &unsafe_audit::measure(&root, &manifest, &store, staged)?,
        ),
        Command::Generate => generate_command(&root, &manifest),
        Command::ApiCheck { write, staged } => {
            api_check::run(&manifest, &root, &store, write, staged)
        }
        Command::DivergenceReason => divergence_command(&root, &manifest, &store),
        Command::NewExt { name } => {
            said(&new_ext::run(&root, &manifest, &name)?);
            Ok(())
        }
        Command::NewSapi { name, binary } => {
            said(&new_sapi::run(&root, &manifest, &name, binary.as_deref())?);
            Ok(())
        }
        Command::Witness(ask) => witness::command(&root, &manifest, &store, ask),
        Command::Vocab { crates, staged } => {
            vocab_command(&root, &manifest, &store, crates, staged)
        }
        Command::Ticket { command } => ticket_command(&root, &manifest, &store, command),
        Command::Usage {
            command: UsageCommand::Record { session, turn },
        } => usage::record(&root, &store, &session, turn),
        Command::Track { .. } => bail!("track: answered above"),
        Command::Regions { .. } => bail!("regions: answered above"),
        Command::Worktree { .. } => bail!("worktree: answered above"),
        Command::Steer { .. } => bail!("steer: answered above"),
    }
}

/// What `new-ext` or `new-sapi` laid down, a file a line, and its notes.
fn said(laid: &new_ext::Laid) {
    for path in &laid.written {
        println!("wrote {path}");
    }
    for note in &laid.notes {
        println!("{note}");
    }
}

/// `steer`: one proposal judged against the harness's own file and
/// applied where it only adds, as one reading.
fn steer(root: &Path, store: &Store, file: &str, from: &Path, dry: bool) -> Result<()> {
    record(store, &[steer::run(root, file, from, dry)?])
}

/// What every instrument ends with: each reading printed as one line and
/// the set recorded, whose outcome is the exit.
pub(crate) fn record(store: &Store, readings: &[Reading]) -> Result<()> {
    recorded(store, readings)?.exit()
}

/// `xtask vocab`: the crates named, or with `--staged` every crate less
/// those whose reading stands for the commit (#1143).
fn vocab_command(
    root: &Path,
    manifest: &Manifest,
    store: &Store,
    crates: Vec<String>,
    staged: bool,
) -> Result<()> {
    let crates = if staged {
        let all: Vec<String> = manifest.crates.iter().map(|c| c.name.clone()).collect();
        let standing = Standing::of_every_crate(true, root, manifest, store, vocab::INSTRUMENT)?;
        let left = standing.unmeasured(&all);
        if left.is_empty() {
            return Ok(());
        }
        left
    } else {
        crates
    };
    match vocab::run(root, manifest, store, &crates)? {
        Outcome::Met => Ok(()),
        Outcome::Unmet { .. } => bail!("vocab: an avoided word is in use; the findings are above"),
    }
}

/// [`record`] without the exit: every reading printed and written, and
/// what they owe, for an instrument whose exit answers something else.
fn recorded(store: &Store, readings: &[Reading]) -> Result<Outcome> {
    for reading in readings {
        println!(
            "{} {} {}: {}",
            reading.status.as_str(),
            reading.instrument,
            reading.scope,
            reading.message
        );
    }
    store.record(readings)
}

/// `xtask ticket read`, `close` and `parent`: the ticket named or the
/// branch's, read through the `gh` on `PATH`, one reading recorded; a
/// close names what it opened before anything is fetched, and the
/// branch's track, read once here, says which label its successors carry.
/// `parent` measures a sheet against the cap and reads no criteria, so it
/// answers above the two that do.
fn ticket_command(
    root: &Path,
    manifest: &Manifest,
    store: &Store,
    command: TicketCommand,
) -> Result<()> {
    if let TicketCommand::Parent { number, session_id } = command {
        let sheet = ticket::ticket_of(root, Some(number))?;
        let reading = ticket::parent(root, &ticket::Gh::on_path(), sheet, session_id.as_deref())?;
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
        } => {
            let ticket = ticket::ticket_of(root, number)?;
            let successors = ticket::Successors::named(ticket, &opens, opens_nothing)?;
            (ticket::Act::Ready(successors), ticket, session_id)
        }
        TicketCommand::Parent { .. } | TicketCommand::Merged { .. } => {
            bail!("ticket parent and merged: answered above")
        }
    };
    let reading = ticket::run(
        root,
        manifest,
        &ticket::Gh::on_path(),
        act,
        ticket,
        session_id.as_deref(),
    )?;
    record(store, std::slice::from_ref(&reading))
}

/// `xtask phpt`: the subsets asked for, each run and each written.
fn phpt_command(root: &Path, manifest: &Manifest, store: &Store, ask: cli::Phpt) -> Result<()> {
    let registry = Registry::load(root)?;
    if let (Some(subset), false) = (&ask.subset, ask.upstream.is_empty()) {
        return phpt::upstream::compare(manifest, &registry, subset, &ask.upstream, ask.jobs);
    }
    let since = ask.base;
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
        return phpt::base::take(root, manifest, &registry, &ask, since);
    }
    let readings = phpt::run(root, manifest, &registry, &ask)?;
    let outcome = recorded(store, &readings)?;
    // `--against` asks whether a test met at the commit fails here, and
    // the exit is that answer; the subsets' own readings are written as
    // ever and hold what they always held (#664).
    if ask.against.is_some() {
        return phpt::against::answer(&readings);
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
/// runner is refused before anything is measured.
fn bench_command(root: &Path, store: &Store, ask: &cli::Bench) -> Result<()> {
    bench::refuse_a_hosted_runner(std::env::var(bench::RUNNER_ENVIRONMENT).ok().as_deref())?;
    bench::run(
        root,
        store,
        ask.bar.as_deref(),
        ask.from.as_deref(),
        ask.reps,
        (ask.label, ask.note.as_deref().unwrap_or_default()),
        ask.log.as_deref(),
    )
}

/// `xtask compile-identity`: the corpus asked for, dumped twice and
/// written; or one file, whose verdict is the exit and which records
/// nothing.
fn identity_command(
    root: &Path,
    manifest: &Manifest,
    store: &Store,
    ask: cli::Identity,
) -> Result<()> {
    let registry = Registry::load(root)?;
    let ask = compile_identity::Ask {
        file: ask.file,
        corpus: ask.corpus,
        engine: ask.engine,
        regime: ask.regime,
    };
    record(
        store,
        &compile_identity::run(root, manifest, &registry, &ask)?,
    )
}

/// `xtask crosscheck`: the extensions asked for, each rendered by the
/// oracle and held against the engine's rendering, each written.
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
        // no reading stands for (#1159): what `done` holds to its row,
        // and an engine crate's change reaches every one (#1312).
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
    record(
        store,
        &crosscheck::run(root, manifest, &registry, store, &ask)?,
    )
}

/// `xtask divergence-reason`: the registry against the rule whose `check`
/// names it. A function of the shape `phpt` and `status` have, so that the
/// dispatch above stays a list of one arm per instrument: it is at clippy's
/// hundred-line ceiling, and `witness` is the arm that tipped it.
fn divergence_command(root: &Path, manifest: &Manifest, store: &Store) -> Result<()> {
    let registry = Registry::load(root)?;
    let reading = divergence::reason(&registry, manifest, root)?;
    match store.record(std::slice::from_ref(&reading))? {
        Outcome::Met => {
            println!("{}", reading.message);
            Ok(())
        }
        Outcome::Unmet { .. } => bail!("{}", reading.message),
    }
}

/// The commands that measure nothing and read no manifest, answered
/// before `Manifest::load`: the track, so a worktree whose manifest is
/// unreadable can still say its branch, base and ticket; the two halves of a
/// worktree, whose manifest is the worktree's, once built there, and is
/// gone once it is retired; and the regions, which pre-commit renders
/// whatever the manifest says.
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
        Command::Worktree {
            command: WorktreeCommand::Add { branch, ticket },
        } => Some(worktree::add(root, branch, ticket.map(track::Ticket::new)).map(|_| ())),
        Command::Worktree {
            command: WorktreeCommand::Retire { branch },
        } => Some(worktree::retire(root, branch)),
        _ => None,
    }
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
        week,
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
                week,
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
