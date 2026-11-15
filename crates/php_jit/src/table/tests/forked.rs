//! A table across the engine's fork (`jit-design.md` §4.4): a trace
//! published before the fork runs in both processes, and what each
//! publishes after it, at the same address of the same chunk, is what
//! that process runs and never what the other wrote. The fork is the one
//! `pcntl_fork` asks, the request's, `Limits::fork`, asked from a tier's
//! safepoint with that request alone inside the engine, so the test runs
//! in a process of its own, this binary started again for it alone. A
//! child whose executable view may be gone ends at the fork, with
//! opcache's fatal line and status, which its parent reads.

use std::io::{Read, Write};
use std::sync::{Arc, Weak};

use php_engine::test::in_a_process_apart;
use php_engine::tier::{
    self, Extension, ExtensionBuilder, Flow, FrameRef, Inherited, Request, Site, Tier,
};

use super::super::code::{Unadoptable, callable, returning, waited};
use super::super::{Outputs, Perf, Table, TraceId, TraceTable};
use super::{compiled, exit, trace};
use crate::record::Start;
use crate::table;
use crate::testing;

/// What the request's half of the test came to, kept for the test.
#[derive(Debug, Default)]
struct Came(Option<Result<String, String>>);

/// The tier that forks at a request's first activation, its epoch's
/// table holding one trace.
struct Forking;

impl Tier for Forking {
    type State = ();

    fn safepoint(req: &mut Request, _: FrameRef, site: Site, _: tier::Held<'_>) -> Flow {
        if site == Site::Entry && req.state::<Came>().0.is_none() {
            let came = forked(req);
            req.state::<Came>().0 = Some(came);
        }
        Flow::Continue
    }

    fn extension() -> Option<Extension> {
        Some(
            ExtensionBuilder::default()
                .request_state::<Came>()
                .build("forking"),
        )
    }
}

/// The epoch's table made and 7 published as its trace 1, the request's
/// fork asked, and each process's half done: the child's ends its process,
/// the parent's answers what both came to.
fn forked(req: &mut Request) -> Result<String, String> {
    let policy = testing::policy(&[("opcache.jit_buffer_size", "64K")]);
    let epoch = table::holding(req, &policy, &Outputs::none());
    let table = table::of_epoch(&epoch).ok_or("the epoch made no table")?;
    if publish(table, 7)? != trace(1) || ran(table, trace(1)) != Some(7) {
        return Err("the first trace did not run".to_owned());
    }
    let (mut from_child, mut to_parent) = std::io::pipe().map_err(|e| e.to_string())?;
    let (mut from_parent, mut to_child) = std::io::pipe().map_err(|e| e.to_string())?;
    // The side is told by the process's id, `Forked` being no item of
    // the tier's face.
    let asker = std::process::id();
    let _side = req.limits_mut().fork().map_err(|e| e.to_string())?;
    if std::process::id() == asker {
        drop((from_parent, to_parent));
        parent(table, &mut from_child, &mut to_child)
    } else {
        drop((from_child, to_child));
        let answered = child(table, &mut from_parent, &mut to_parent);
        std::process::exit(i32::from(answered.is_err()))
    }
}

/// What the trace `id` of `table` returns, entered as compiled code's
/// caller enters it; none where the table holds no code for it.
fn ran(table: &Table, id: TraceId) -> Option<u32> {
    let entry = table.get(id)?.code.entry();
    Some(callable(entry)())
}

/// The body returning `value` published into `table` as the next trace.
fn publish(table: &Table, value: u16) -> Result<TraceId, String> {
    (table.publish(compiled(&returning(value), vec![]), Start::Loop))
        .map_err(|refused| format!("the body returning {value}: {refused:?}"))
}

/// The address the trace `id` of `table` is entered at, as a word.
fn address(table: &Table, id: TraceId) -> Result<u64, String> {
    let entry = table.get(id).ok_or("no code for the trace")?.code.entry();
    u64::try_from(entry.as_ptr().addr()).map_err(|e| e.to_string())
}

fn read_word(from: &mut impl Read) -> Result<u64, String> {
    let mut word = [0; 8];
    from.read_exact(&mut word).map_err(|e| e.to_string())?;
    Ok(u64::from_ne_bytes(word))
}

fn write_word(to: &mut impl Write, word: u64) -> Result<(), String> {
    to.write_all(&word.to_ne_bytes()).map_err(|e| e.to_string())
}

/// The child's half, which answers over `to_parent`: it runs the trace
/// published before the fork, publishes 30 as trace 2 and says where it
/// placed it; once the parent has published 20 and 21 as its own traces 2
/// and 3, it runs its trace 2, publishes 31 as trace 3 and runs it, and
/// answers the three values, where it placed trace 3, and whether the
/// file behind its arena holds the seals the arena's first file did.
fn child(
    table: &Table,
    from_parent: &mut impl Read,
    to_parent: &mut impl Write,
) -> Result<(), String> {
    let before = ran(table, trace(1)).unwrap_or(0);
    let second = publish(table, 30)?;
    write_word(to_parent, address(table, second)?)?;
    read_word(from_parent)?;
    let own = ran(table, second).unwrap_or(0);
    let third = publish(table, 31)?;
    let last = ran(table, third).unwrap_or(0);
    let placed = address(table, third)?;
    // Elsewhere the arena maps no file: macOS aarch64's `MAP_JIT` mapping is
    // copied by the fork, and macOS x86-64's two views are anonymous, laid
    // again over one copy by the child's adopt.
    #[cfg(not(target_os = "linux"))]
    let sealed = table.writer().arena.is_some();
    #[cfg(target_os = "linux")]
    let sealed = table.writer().arena.as_ref().is_some_and(|arena| {
        use rustix::fs::SealFlags;
        let held = SealFlags::SHRINK | SealFlags::GROW | SealFlags::SEAL;
        arena.pages().seals().contains(held)
    });
    for word in [before, own, last, u32::from(sealed)] {
        write_word(to_parent, u64::from(word))?;
    }
    write_word(to_parent, placed)?;
    Ok(())
}

/// The parent's half: it waits for the child's trace 2, publishes 20 as
/// its own trace 2, at the address the child placed its own, and 21 as
/// trace 3, lets the child go on, and reads the child's answers, among
/// them where the child placed its trace 3, which must be where the
/// parent placed its own; it then runs its three traces, the last after
/// the child published 31 at that address.
fn parent(
    table: &Table,
    from_child: &mut impl Read,
    to_child: &mut impl Write,
) -> Result<String, String> {
    let placed = read_word(from_child)?;
    let second = publish(table, 20)?;
    let at = address(table, second)?;
    if at != placed {
        return Err(format!(
            "the child placed {placed:#x} and the parent {at:#x}"
        ));
    }
    let third = publish(table, 21)?;
    write_word(to_child, 1)?;
    let mut answers = [0; 4];
    for answer in &mut answers {
        *answer = read_word(from_child)?;
    }
    let [before, own, last, sealed] = answers;
    let (placed, at) = (read_word(from_child)?, address(table, third)?);
    if at != placed {
        return Err(format!(
            "the child placed trace 3 at {placed:#x} and the parent at {at:#x}"
        ));
    }
    let ours = [trace(1), second, third].map(|id| ran(table, id).unwrap_or(0));
    Ok(format!(
        "child {before} {own} {last} sealed {sealed}, parent {} {} {}",
        ours[0], ours[1], ours[2]
    ))
}

/// A trace published before the request's fork runs in both processes,
/// and what each publishes after it at the same address of its one chunk
/// is what it runs: the child's trace 2, published first, still returns
/// 30 once the parent published 20 at its address, and the parent's
/// trace 3 still returns 21 once the child published 31 at its address.
/// Where the child shares its parent's pages, as it did while both views
/// were one shared `memfd` across the fork, the later write is what both
/// enter. The child's arena maps a file sealed as the first was.
#[test]
fn a_forks_child_runs_its_parents_code_and_neither_runs_what_the_other_publishes_after() {
    // The test's name as the harness takes it, the crate's name cut off.
    let module = module_path!().split_once("::").map_or("", |(_, rest)| rest);
    let name = format!(
        "{module}::a_forks_child_runs_its_parents_code_and_neither_runs_what_the_other_publishes_after"
    );
    in_a_process_apart(&name, "child 7 30 31 sealed 1, parent 7 20 21", || {
        let engine = testing::engine::<Forking>(&[]);
        let scratch = testing::Scratch::new();
        let (_, came) = scratch.serve_kept::<Came>(&engine, "function f() {} f();");
        came.and_then(|came| came.0)
            .ok_or_else(|| "the request kept nothing".to_owned())?
    });
}

/// The tier that forks at a request's first activation with a table over
/// [`Unadoptable`] entered among what the fork holds, so the child's
/// first mapping is refused where its executable view may be gone.
struct Losing;

impl Tier for Losing {
    type State = ();

    fn safepoint(req: &mut Request, _: FrameRef, site: Site, _: tier::Held<'_>) -> Flow {
        if site == Site::Entry && req.state::<Came>().0.is_none() {
            let came = lost(req);
            req.state::<Came>().0 = Some(came);
        }
        Flow::Continue
    }

    fn extension() -> Option<Extension> {
        Some(
            ExtensionBuilder::default()
                .request_state::<Came>()
                .build("losing"),
        )
    }
}

/// A table over [`Unadoptable`] holding one trace, entered among what
/// the fork holds, and the request's fork asked: the child ends in the
/// fork's child half and never returns here; the parent answers how it
/// ended, and that it still holds its trace.
fn lost(req: &mut Request) -> Result<String, String> {
    let policy = testing::policy(&[("opcache.jit_buffer_size", "64K")]);
    let table = Arc::new(TraceTable::<Unadoptable>::with_perf(&policy, Perf::none()));
    let before = (table.publish(compiled(&[0; 8], vec![exit(None)]), Start::Loop))
        .map_err(|refused| format!("the trace before the fork: {refused:?}"))?;
    let entry: Weak<dyn Inherited> = Arc::downgrade(&table) as Weak<TraceTable<Unadoptable>>;
    tier::inherit(entry);
    let asker = std::process::id();
    let _side = req.limits_mut().fork().map_err(|e| e.to_string())?;
    if std::process::id() != asker {
        // A child that went on past its hold says so by its status.
        std::process::exit(0);
    }
    let ended = waited().map_err(|e| e.to_string())?;
    match ended {
        Ok(254) if table.get(before).is_some() => Ok("the child exited 254".to_owned()),
        Ok(status) => Err(format!("the child exited {status}")),
        Err(signal) => Err(format!("the child ended on signal {signal}")),
    }
}

/// A fork's child whose first mapping was refused, where Linux may have
/// taken its executable view away, writes opcache's fatal line and exits
/// with its status, 254 (`exit(-2)`), before it returns into any frame
/// (`jit-design.md` §4.4, ruled 2026-11-11): the process apart passes only
/// where its parent read that status, and printed holds the child's line.
#[test]
fn a_forks_child_whose_code_may_be_gone_ends_with_opcaches_fatal_line_and_status() {
    let module = module_path!().split_once("::").map_or("", |(_, rest)| rest);
    let name = format!(
        "{module}::a_forks_child_whose_code_may_be_gone_ends_with_opcaches_fatal_line_and_status"
    );
    let line = "Fatal Error Could not keep JIT: the code buffer was lost at fork (Cannot allocate memory)\n";
    in_a_process_apart(&name, line, || {
        let engine = testing::engine::<Losing>(&[]);
        let scratch = testing::Scratch::new();
        let (_, came) = scratch.serve_kept::<Came>(&engine, "function f() {} f();");
        came.and_then(|came| came.0)
            .ok_or_else(|| "the request kept nothing".to_owned())?
    });
}
