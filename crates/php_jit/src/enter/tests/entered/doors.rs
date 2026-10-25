//! An exit's entry slot under a publish on another thread (#3231, #3229's
//! ruling, `jit-design.md` §4.4): a root whose entry guard always fails
//! leaves through exit 0 on every turn of one thread, while a second
//! thread compiles the side trace grown from that exit and publishes it,
//! which writes the exit's slot with release. Every turn's answer is the
//! root's exit, the frame stood at its opline, until the root's code reads
//! the slot set with acquire; from that turn on it is the side's end, the
//! side entered by the root's own jump with no crossing between. The
//! request's row of the side is laid before the side exists, so the slot
//! alone decides.

use std::sync::atomic::{AtomicU32, Ordering};

use php_engine::tier::{
    self, Extension, ExtensionBuilder, Flow, FrameRef, FuncRef, Request, Site, Tier,
};
use php_unit::OplineIdx;

use crate::enter::{Answered, At, Exited, Left, Readiness};
use crate::record::{Start, Stop};
use crate::table::{self, TraceId, TraceName};
use crate::testing::{self, Scratch, function_of, stdout};
use crate::types::{
    Check, End, Entry, ExitId, ExitSpec, Frame, FrameInfo, Grows, Guard, GuardAt, Landing, Place,
    TraceIr,
};

/// What the turns saw: how many answered by the root's exit and how many
/// by the side's end, and the first answer that was neither, or that came
/// back to the root once the side had answered.
#[derive(Debug, Default)]
struct Turns {
    before: u32,
    after: u32,
    wrong: Option<String>,
    raced: bool,
}

/// The turns the root runs before the other thread compiles its side, and
/// the turns it runs once the side answers.
const BEFORE: u32 = 2_000;
const AFTER: u32 = 2_000;
/// The turns past which the side not answering fails the test.
const LIMIT: u32 = 2_000_000;

/// The opline the root's exit lands at, the one its end lands at, and the
/// one the side's end lands at.
const ROOT_EXIT: u32 = 1;
const ROOT_END: u32 = 2;
const SIDE_END: u32 = 3;

/// An exit of the frame the trace enters on that lands at `opline`.
fn exit_at(opline: u32, grows: Grows) -> ExitSpec {
    ExitSpec {
        frame: Frame::ROOT,
        opline: Some(OplineIdx(opline)),
        landing: Landing::PLAIN,
        stores: Vec::new(),
        known: Vec::new(),
        grows,
        link: None,
        layout: false,
    }
}

/// A trace of no ops over `f`'s frame, started as `start`, with `guards`
/// and `exits`, ending through its last exit.
fn trace(f: FuncRef, start: Start, guards: Vec<Guard>, exits: Vec<ExitSpec>) -> TraceIr {
    let end = ExitId(u16::try_from(exits.len().saturating_sub(1)).unwrap_or(0));
    TraceIr {
        entry: Entry {
            start,
            root: f,
            stop: Stop::Return,
        },
        frames: vec![FrameInfo {
            func: f,
            place: Place::Root,
            cvs: 1,
            temps: 0,
            literals: 0,
        }],
        values: Vec::new(),
        ops: Vec::new(),
        guards,
        exits,
        unwinds: Vec::new(),
        map_ptrs: Vec::new(),
        end: End::Leave(end),
        body: None,
    }
}

/// The name a trace of the test is published under.
fn named(at: &[u8]) -> TraceName {
    TraceName {
        at: at.into(),
        line: 1,
    }
}

/// What a turn's answer was: the root's exit, or the side's end.
fn turned(
    answered: &Answered<'_>,
    (root, side): (TraceId, TraceId),
    at: Option<OplineIdx>,
) -> Result<bool, String> {
    match (answered.by.id, answered.left, at) {
        (
            by,
            Left::Exit(Exited {
                exit: ExitId(0), ..
            }),
            Some(OplineIdx(ROOT_EXIT)),
        ) if by == root => Ok(false),
        (
            by,
            Left::Exit(Exited {
                exit: ExitId(0), ..
            }),
            Some(OplineIdx(SIDE_END)),
        ) if by == side => Ok(true),
        _ => Err(format!(
            "{:?} by {:?} standing at {at:?}",
            answered.left, answered.by.id
        )),
    }
}

/// The root entered on `frame` turn after turn while another thread
/// compiles and publishes its side.
#[allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]
fn raced(req: &mut Request, frame: FrameRef) -> Result<Turns, String> {
    let shared = req
        .engine::<crate::JitEngine>()
        .ok_or("no engine state")?
        .shared
        .clone();
    let backend = shared.backend.as_ref().ok_or("no backend")?;
    let f = tier::func(req, frame).ok_or("no op array for f")?;
    let epoch = table::holding(req, &shared.policy, &shared.perf);
    let table = table::of_epoch(&epoch).ok_or("no table")?;
    // The root: its entry's stack check never holds, so it leaves through
    // exit 0, which grows side traces, on every turn.
    let always = Guard {
        at: GuardAt::Entry,
        check: Check::Room { bytes: u32::MAX },
        exit: ExitId(0),
    };
    let root = trace(
        f,
        Start::Enter,
        vec![always],
        vec![
            exit_at(ROOT_EXIT, Grows::Side),
            exit_at(ROOT_END, Grows::Never),
        ],
    );
    let root = backend
        .compile(&root, named(b"root"), None)
        .map_err(|abort| format!("{abort:?}"))?;
    let root = table
        .publish(root, Start::Enter)
        .map_err(|full| format!("{full:?}"))?;
    let side_id = table.next().ok_or("no number for the side")?;
    // The side's row, ready, as the crossing lays one for a trace with no
    // calls in place: from here only the slot keeps the jump closed.
    let state = tier::state::<crate::JitRequest>(req).ok_or("no request state")?;
    state
        .headers
        .rows(|| table.segments())
        .lay(side_id, std::ptr::null(), Readiness::Settled);
    let grown = Start::Side {
        parent: root,
        exit: ExitId(0),
    };
    let side = trace(f, grown, Vec::new(), vec![exit_at(SIDE_END, Grows::Never)]);
    let turns = AtomicU32::new(0);
    std::thread::scope(|scope| {
        let publishing = scope.spawn(|| {
            while turns.load(Ordering::Relaxed) < BEFORE {
                std::hint::spin_loop();
            }
            let compiled = backend
                .compile(&side, named(b"side"), None)
                .map_err(|abort| format!("{abort:?}"))?;
            table
                .publish(compiled, grown)
                .map_err(|full| format!("{full:?}"))
        });
        let outcome = (|| {
            let mut seen = Turns::default();
            let root_trace = table.get(root).ok_or("the root is not in its table")?;
            for turn in 0..LIMIT {
                let answered = super::crossed(req, frame, (&epoch, root_trace), At::First)
                    .map_err(|fatal| format!("{fatal:?}"))?;
                let at = tier::frame(req, frame).map(|view| view.opline());
                match turned(&answered, (root, side_id), at) {
                    Ok(false) if seen.after == 0 => seen.before += 1,
                    Ok(true) => seen.after += 1,
                    Ok(false) => {
                        seen.wrong = Some(format!("turn {turn}: the root's exit after the side's"));
                    }
                    Err(wrong) => seen.wrong = Some(format!("turn {turn}: {wrong}")),
                }
                turns.fetch_add(1, Ordering::Relaxed);
                if seen.wrong.is_some() || seen.after >= AFTER {
                    break;
                }
            }
            Ok::<Turns, String>(seen)
        })();
        // The publisher waits on the turns alone, so it ends however the
        // turns did.
        turns.store(BEFORE, Ordering::Relaxed);
        let published = publishing
            .join()
            .map_err(|_| "the publishing thread panicked".to_owned())??;
        let mut seen = outcome?;
        if published != side_id {
            return Err(format!("the side numbered {published:?}, not {side_id:?}"));
        }
        seen.raced = true;
        Ok(seen)
    })
}

/// The tier that races `f`'s first activation and leaves every other to
/// the interpreter.
struct Racing;

impl Tier for Racing {
    type State = crate::JitRequest;

    fn lay(started: &tier::Started<'_>) -> Option<crate::JitRequest> {
        crate::JitRequest::laid(*started)
    }

    fn safepoint(req: &mut Request, frame: FrameRef, site: Site, _: tier::Held<'_>) -> Flow {
        if site != Site::Entry
            || function_of(req, frame).as_deref() != Some(b"f".as_slice())
            || req.state::<Turns>().raced
        {
            return Flow::Continue;
        }
        let Some(stands) = tier::frame(req, frame).map(|view| view.opline()) else {
            return Flow::Continue;
        };
        let turns = raced(req, frame).unwrap_or_else(|wrong| Turns {
            wrong: Some(wrong),
            raced: true,
            ..Turns::default()
        });
        *req.state::<Turns>() = turns;
        tier::stand(req, frame, stands);
        Flow::Continue
    }

    fn extension() -> Option<Extension> {
        let mut ini = crate::policy::ini();
        ini.extend(testing::declared(&testing::OPCACHE));
        Some(
            ExtensionBuilder::default()
                .ini(ini)
                .engine_state(crate::JitEngine::new)
                .rinit(crate::JitEngine::request_started)
                .request_state::<Turns>()
                .build("jit"),
        )
    }
}

/// The root answers its exit on every turn before the side is published
/// and the side's end on every turn after its slot is read set, never
/// anything else and never the root's exit again; `f` then runs as the
/// interpreter runs it.
#[test]
fn a_side_published_on_another_thread_is_entered_through_its_slot() {
    let engine =
        testing::engine::<Racing>(&[("jit.mode", "force"), ("opcache.jit_buffer_size", "64K")]);
    let (served, turns) = Scratch::new().serve_kept::<Turns>(
        &engine,
        "function f($x) { $a = 1; $b = 2; $c = 3; return $x + $a + $b + $c; }
        echo f(4), \"\\n\";",
    );
    assert_eq!(stdout(&served), "10\n", "{:?}", served.fatal);
    let turns = turns.unwrap_or_default();
    assert!(turns.raced, "f was never raced");
    assert_eq!(turns.wrong, None);
    assert!(turns.before >= BEFORE, "{turns:?}");
    assert!(turns.after >= AFTER, "{turns:?}");
}
