//! The safety net under #3230: a known callee's push, call and leave
//! written in place leave the request's stack, byte for byte, as the
//! engine's own push, call and leave do (`jit-design.md` §5.3). `f` calls
//! `g(1, 2)` four times. On the first, the test steps `f`'s `INIT_FCALL`,
//! its sends and its `DO_UCALL` and `g`'s `RETURN` through the
//! interpreter's handlers, `push_call`, `enter_call` and the leave
//! kernel's `leave_plainly`, and reads the stack after each; on the next
//! three, code `lower` compiles from a trace of the same oplines makes
//! each in place, and the stack is read at the same points. The frames
//! stand at the same addresses in every call, so the reads are held to be
//! equal whole: the rooms' links and headers, the slots' values and type
//! words, the frame records and the stack's top, end, frame count and
//! innermost room.

use php_engine::tier::{
    self, Extension, ExtensionBuilder, FRAME, Flow, FrameRef, FuncRef, PlainCall, Request, Site,
    Tier,
};
use php_unit::{MapPtrOffset, Opcode, OperandKind, OplineIdx};

use crate::enter::On;
use crate::lower::{Backend, Compiled, Kernel, Stood, Symbols};
use crate::record::{Began, Callee, Start, Stop};
use crate::table::{self, Outputs, TraceName};
use crate::testing::{self, Scratch, function_of, stdout};
use crate::types::{
    Constant, End, Entry, ExitId, ExitSpec, Form, Frame, FrameInfo, Grows, Imm, Landing, Line, Op,
    Operand, Place, TraceIr, Unwind,
};

/// What a read of the stack holds: the room of the frame read, its link,
/// header and slots, each slot's value and type word; that frame's
/// record; the stack's top, end, frame count and innermost room, each as
/// an offset from the room read where it is an address.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Read {
    room: Vec<u8>,
    slots: Vec<[u8; 12]>,
    record: (usize, usize, u8),
    stack: [isize; 4],
}

/// The words a read takes, as `FRAME` names them.
const fn at(word: u32) -> usize {
    word as usize
}

/// The `n` bytes at `from`.
fn bytes(from: *const u8, n: usize) -> Vec<u8> {
    // SAFETY: every caller reads a room, a slot or a word of a frame on
    // the request's stack, or the stack itself, which stand for the read.
    unsafe { std::slice::from_raw_parts(from, n) }.to_vec()
}

/// The word at `at` bytes into `base`.
fn word(base: *const u8, at: usize) -> usize {
    let mut word = [0; size_of::<usize>()];
    word.copy_from_slice(&bytes(base.wrapping_add(at), size_of::<usize>()));
    usize::from_ne_bytes(word)
}

/// The stack as it stands, read at `frame`'s room.
fn read(req: &mut Request, frame: FrameRef) -> Option<Read> {
    let room = tier::room(req, frame)?.as_ptr().cast_const();
    let stack = tier::stack(req).as_ptr().cast_const();
    let frames = word(stack, at(FRAME.frames()));
    let record =
        std::ptr::with_exposed_provenance::<u8>(frames + at(FRAME.record()) * frame.0 as usize);
    let len = word(record, at(FRAME.record_len()));
    let first = word(record, at(FRAME.record_at()));
    let tag = bytes(record.wrapping_add(at(FRAME.record_placed())), 1)[0];
    let slots = (0..len)
        .map(|slot| {
            let mut read = [0; 12];
            read.copy_from_slice(&bytes(room.wrapping_add(at(FRAME.room()) + 16 * slot), 12));
            read
        })
        .collect();
    let from = |address: usize| address.wrapping_sub(room.addr()).cast_signed();
    Some(Read {
        room: bytes(room, at(FRAME.room())),
        slots,
        record: (first.wrapping_sub(room.addr()), len, tag),
        stack: [
            from(word(stack, at(FRAME.top()))),
            from(word(stack, at(FRAME.end()))),
            word(stack, at(FRAME.frames_len())).cast_signed(),
            from(word(stack, at(FRAME.innermost()))),
        ],
    })
}

/// The reads of one way of making the call: after the push, read at the
/// call; after the call, read at the callee; after the leave, read at the
/// caller.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Reads {
    pushed: Option<Read>,
    made: Option<Read>,
    left: Option<Read>,
}

/// What the test keeps across the request.
#[derive(Debug, Default)]
struct Parity {
    calls: u32,
    plain: Option<PlainCall>,
    by_kernel: Reads,
    in_place: Reads,
}

/// The first opline of `frame`'s op array of `opcode`, and the slot its
/// `result` operand names past the variables, where it names one.
fn named(req: &Request, frame: FrameRef, opcode: Opcode) -> Option<(u32, Option<u32>)> {
    let f = func(req, frame)?;
    let mut view = tier::UnitView::new(req);
    view.hold(req, f).then_some(())?;
    let code = view.op_array(f)?;
    let (at, op) = (code.oplines.iter().enumerate()).find(|(_, op)| op.opcode == opcode)?;
    let out = (op.out_type != OperandKind::Unused).then_some(op.out);
    Some((u32::try_from(at).ok()?, out))
}

/// The engine offset of the op array `frame` serves.
fn func(req: &Request, frame: FrameRef) -> Option<FuncRef> {
    tier::func(req, frame).filter(|f| matches!(f.offset(), MapPtrOffset::Engine(_)))
}

/// An inline line of `frame` at `opline`.
fn line(frame: Frame, opline: u32, opcode: Opcode, op1: Operand, imm: Imm) -> Line {
    Line {
        frame,
        opline: OplineIdx(opline),
        opcode,
        op1,
        op2: Operand::Unused,
        data: Operand::Unused,
        out: None,
        op1_def: None,
        op2_def: None,
        fused: None,
        imm,
        form: Form::Inline,
        map_ptr: None,
        exit: None,
        unwind: None,
    }
}

/// The trace of `ops` over `frames`, ending through its one exit, which
/// lands `frame` at `opline`.
fn trace(frames: Vec<FrameInfo>, ops: Vec<Op>, (frame, opline): (Frame, u32)) -> TraceIr {
    let root = frames
        .first()
        .map_or(FuncRef::at(MapPtrOffset::Engine(0)), |f| f.func);
    TraceIr {
        entry: Entry {
            start: Start::Enter,
            root,
            stop: Stop::Return,
        },
        frames,
        values: Vec::new(),
        ops,
        guards: Vec::new(),
        exits: vec![ExitSpec {
            frame,
            opline: Some(OplineIdx(opline)),
            landing: Landing::Plain,
            stores: Vec::new(),
            known: Vec::new(),
            grows: Grows::Never,
            link: None,
            layout: false,
        }],
        unwinds: vec![Unwind { stores: Vec::new() }],
        map_ptrs: Vec::new(),
        end: End::Leave(ExitId(0)),
        body: None,
    }
}

/// A kernel that refuses: what serves an opline whole, or a call, stands
/// in for the code in place only where the code fell back to it, which
/// none of these traces does, so a fallback fails the test.
fn refused(_: &mut Request, _: On<'_, '_>, _: u32) -> Result<Stood, tier::Fatal> {
    Err(tier::Fatal::error(
        "a kernel served what the code makes in place",
    ))
}

/// [`refused`] for the kernels that take a callee.
fn refused_call(_: &mut Request, _: On<'_, '_>, _: u32, _: u32) -> Result<Stood, tier::Fatal> {
    Err(tier::Fatal::error(
        "a kernel served a call the code makes in place",
    ))
}

kernels! { Request => mod refusing { refused(func: u32) -> Stood; refused_call(func: u32, callee: u32) -> Stood; } }

/// `ir` compiled for the host, the kernels that serve an opline or a call
/// whole refusing.
fn compiled(ir: &TraceIr) -> Option<Compiled> {
    let symbols = Symbols::new(|kernel| match kernel {
        Kernel::Step => (refusing::refused as *const ()).addr(),
        Kernel::PushCall | Kernel::Call => (refusing::refused_call as *const ()).addr(),
        Kernel::Release => (super::super::super::wrapped::release as *const ()).addr(),
        Kernel::Copy => (super::super::super::wrapped::copy as *const ()).addr(),
    });
    let backend = Backend::new(&testing::policy(&[]), symbols).ok()?;
    backend
        .compile(
            ir,
            TraceName {
                at: b"parity".to_vec().into_boxed_slice(),
                line: 1,
            },
            None,
        )
        .ok()
}

/// `ir` entered on `frame`, through its one exit.
fn entered(req: &mut Request, frame: FrameRef, ir: &TraceIr) -> Option<FrameRef> {
    let body = compiled(ir)?;
    let policy = testing::policy(&[("opcache.jit_buffer_size", "64K")]);
    let epoch = table::holding(req, &policy, &Outputs::none());
    let table = table::of_epoch(&epoch)?;
    let id = table.publish(body, Start::Enter).ok()?;
    let trace = table.get(id)?;
    match super::super::super::enter(req, frame, (&epoch, trace), super::super::super::At::First)
        .ok()?
        .left
    {
        super::super::super::Left::Exit(exited) => Some(exited.frame),
        _ => None,
    }
}

/// The first call, made by the interpreter's handlers one step at a time,
/// read after the push, the call and the leave.
fn by_kernel(req: &mut Request, f: FrameRef) -> Option<Reads> {
    let (init, _) = named(req, f, Opcode::InitFcall)?;
    let (call, _) = named(req, f, Opcode::DoUcall)?;
    tier::stand(req, f, OplineIdx(init));
    tier::step(req, f).ok()?;
    let pushed_at = tier::building_call(req)?;
    req.state::<Parity>().plain = tier::plain_call(req, pushed_at);
    let pushed = read(req, pushed_at);
    while tier::frame(req, f)?.opline().0 < call {
        tier::step(req, f).ok()?;
    }
    let tier::Stepped::Pushed(g) = tier::step(req, f).ok()? else {
        return None;
    };
    let made = read(req, g);
    let (ret, _) = named(req, g, Opcode::Return)?;
    while tier::frame(req, g)?.opline().0 < ret {
        tier::step(req, g).ok()?;
    }
    tier::step(req, g).ok()?;
    let left = read(req, f);
    Some(Reads { pushed, made, left })
}

/// The frames of `f` and of `g` as a trace of `f`'s call holds them.
fn frames(f: FuncRef, g: FuncRef) -> Vec<FrameInfo> {
    vec![
        FrameInfo {
            func: f,
            place: Place::Root,
            cvs: 1,
            temps: 2,
        },
        FrameInfo {
            func: g,
            place: Place::Callee(Frame::ROOT),
            cvs: 2,
            temps: 0,
        },
    ]
}

/// `f`'s push in place, the trace leaving past it.
fn push_in_place(req: &mut Request, f: FrameRef) -> Option<Read> {
    let plain = req.state::<Parity>().plain.clone()?;
    let (init, _) = named(req, f, Opcode::InitFcall)?;
    let (callee, me) = (plain.callee(), func(req, f)?);
    let ops = vec![
        Op::Line(line(
            Frame::ROOT,
            init,
            Opcode::InitFcall,
            Operand::Unused,
            Imm::None,
        )),
        Op::InitCall {
            frame: Frame::ROOT,
            callee: Callee::User(callee),
            began: Began::Here,
            plain: Some(plain),
        },
    ];
    let ir = trace(frames(me, callee), ops, (Frame::ROOT, init + 1));
    entered(req, f, &ir)?;
    let call = tier::building_call(req)?;
    read(req, call)
}

/// `f`'s push, sends and call made in place, the trace leaving in `g` at
/// its entry.
fn make_in_place(req: &mut Request, f: FrameRef) -> Option<Read> {
    let plain = req.state::<Parity>().plain.clone()?;
    let entry = plain.activation()?.opline();
    let (init, _) = named(req, f, Opcode::InitFcall)?;
    let (call, returns) = named(req, f, Opcode::DoUcall)?;
    let (callee, me) = (plain.callee(), func(req, f)?);
    let send = |at: u32, n: u32| {
        Op::Line(line(
            Frame::ROOT,
            at,
            Opcode::SendVal,
            Operand::Constant(Constant::Long(i64::from(n))),
            Imm::Arg(n),
        ))
    };
    let mut made = line(
        Frame::ROOT,
        call,
        Opcode::DoUcall,
        Operand::Unused,
        Imm::Make { returns },
    );
    made.unwind = Some(crate::types::UnwindId(0));
    let ops = vec![
        Op::Line(line(
            Frame::ROOT,
            init,
            Opcode::InitFcall,
            Operand::Unused,
            Imm::None,
        )),
        Op::InitCall {
            frame: Frame::ROOT,
            callee: Callee::User(callee),
            began: Began::Here,
            plain: Some(plain),
        },
        send(init + 1, 1),
        send(init + 2, 2),
        Op::Line(made),
        Op::Enter {
            frame: Frame(1),
            stores: Vec::new(),
        },
    ];
    let ir = trace(frames(me, callee), ops, (Frame(1), entry));
    let g = entered(req, f, &ir)?;
    read(req, g)
}

/// `g`'s return of `7` left in place, the trace leaving in `f` past its
/// call.
fn leave_in_place(req: &mut Request, g: FrameRef) -> Option<(FrameRef, Read)> {
    let (ret, _) = named(req, g, Opcode::Return)?;
    let f = tier::below(req, g)?;
    let (call, _) = named(req, f, Opcode::DoUcall)?;
    let (me, caller) = (func(req, g)?, func(req, f)?);
    let ops = vec![
        Op::Line(line(
            Frame::ROOT,
            ret,
            Opcode::Return,
            Operand::Constant(Constant::Long(7)),
            Imm::Leave { owners: true },
        )),
        Op::Back {
            frame: Frame(1),
            out: None,
            escapes: Some((caller, OplineIdx(call + 1))),
        },
    ];
    let frames = vec![
        FrameInfo {
            func: me,
            place: Place::Root,
            cvs: 2,
            temps: 0,
        },
        FrameInfo {
            func: caller,
            place: Place::Caller(Frame::ROOT),
            cvs: 1,
            temps: 2,
        },
    ];
    let ir = trace(frames, ops, (Frame(1), call + 1));
    let back = entered(req, g, &ir)?;
    Some((back, read(req, back)?))
}

/// The tier that makes `f`'s call one way or the other.
struct Paired;

impl Tier for Paired {
    type State = ();

    fn safepoint(req: &mut Request, frame: FrameRef, site: Site, _: tier::Held<'_>) -> Flow {
        if site != Site::Entry {
            return Flow::Continue;
        }
        let Some(function) = function_of(req, frame) else {
            return Flow::Continue;
        };
        let calls = req.state::<Parity>().calls;
        match (function.as_slice(), calls) {
            (b"f", 0) => {
                req.state::<Parity>().calls += 1;
                let Some(reads) = by_kernel(req, frame) else {
                    return Flow::Bail(tier::Fatal::error("the call by the handlers"));
                };
                req.state::<Parity>().by_kernel = reads;
                Flow::Resume(frame)
            }
            (b"f", 1) => {
                req.state::<Parity>().calls += 1;
                let pushed = push_in_place(req, frame);
                req.state::<Parity>().in_place.pushed = pushed;
                Flow::Resume(frame)
            }
            (b"f", 2) => {
                req.state::<Parity>().calls += 1;
                let made = make_in_place(req, frame);
                req.state::<Parity>().in_place.made = made;
                match tier::current(req) {
                    Some(at) => Flow::Resume(at),
                    None => Flow::Bail(tier::Fatal::error("no frame after the call")),
                }
            }
            (b"f", 3) => {
                req.state::<Parity>().calls += 1;
                Flow::Continue
            }
            (b"g", 4) => {
                req.state::<Parity>().calls += 1;
                let Some((back, left)) = leave_in_place(req, frame) else {
                    return Flow::Bail(tier::Fatal::error("the leave in place"));
                };
                req.state::<Parity>().in_place.left = Some(left);
                Flow::Resume(back)
            }
            _ => Flow::Continue,
        }
    }

    fn extension() -> Option<Extension> {
        Some(
            ExtensionBuilder::default()
                .request_state::<Parity>()
                .build("parity"),
        )
    }
}

/// The push, the call and the leave written in place leave the stack as
/// `push_call`, `enter_call` and `leave_plainly` leave it, and the
/// program writes what the interpreter writes.
#[test]
fn a_call_made_in_place_leaves_the_stack_as_the_engines_own() {
    let engine = testing::engine::<Paired>(&[]);
    let (served, parity) = Scratch::new().serve_kept::<Parity>(
        &engine,
        "function g($a, $b) { return 7; }
        function f() { $r = g(1, 2); echo $r, \"\\n\"; }
        f(); f(); f(); f();",
    );
    assert_eq!(stdout(&served), "7\n7\n7\n7\n", "{:?}", served.fatal);
    let parity = parity.unwrap_or_default();
    assert_eq!(parity.calls, 5);
    let Reads { pushed, made, left } = parity.by_kernel;
    assert!(pushed.is_some() && made.is_some() && left.is_some());
    assert_eq!(parity.in_place.pushed, pushed, "the push");
    assert_eq!(parity.in_place.made, made, "the call");
    assert_eq!(parity.in_place.left, left, "the leave");
}
