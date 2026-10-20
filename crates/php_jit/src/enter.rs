//! The crossing (`jit-design.md` §4.6): [`enter`], which calls a
//! published trace on a frame and lands the exit it left through; the
//! [`ExitCtx`] compiled code hands every kernel wrapper; the [`Status`] a
//! wrapper answers with; and [`kernels!`], which expands one wrapper per
//! listed kernel and, for the engine's list, the `Symbols` table `lower`
//! calls them through.
//!
//! **A wrapper** rebuilds the request from the `ExitCtx`, calls the
//! kernel inside `catch_unwind(AssertUnwindSafe(..))`, writes the
//! kernel's value through the out pointer, and maps what came back to a
//! `Status`: a fatal or a panic payload is kept in the `ExitCtx` for
//! `enter` to return or resume, and a throwable the kernel left pending
//! is answered as one. Wrappers are `extern "C"` and not `"C-unwind"`: no
//! panic reaches compiled code (`research/llvm-research.r1.md` B.4).
//!
//! **The engine's kernels** are `lower::Kernel`'s four (§5.2): `step`,
//! the opline served whole through `tier::step`; `push_call`, an inline
//! `INIT_*` served the same way where the callee its cache slot holds is
//! the one recorded, and refused before anything is pushed where it is
//! not; `release`, php's `zval_ptr_dtor`; and `copy`, php's
//! `ZVAL_COPY_DEREF`. Each that reads its frame stands it at the opline
//! it serves first, php's `SAVE_OPLINE()`, so what it raises is reported
//! there. Each line of the list names the engine item it serves through,
//! which `api-check` writes into `tier.lock`'s kernels section where the
//! face does not list it already (§5.3).
//!
//! **What `enter` does not do yet.** It touches no map pointer on a
//! request's first entry: `lower` lowers no read of a runtime-cache slot
//! past the request's map base, so there is nothing to lay down until it
//! does. A call `push_call` began is on the engine's stack, pushed by
//! the engine, where php's compiled code keeps it in a register until an exit's
//! `RESTORE_CALL` links it, so `Landing::RestoreCall` restores nothing.
//!
//! `ExitCtx` is generic over the request it carries, so that the bench
//! bar and the wrappers' own tests cross it over a stand-in with no
//! engine (`jit-design.md` revision 20).
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::any::Any;
use std::marker::PhantomData;
use std::mem::offset_of;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::ptr::{self, NonNull};
use std::sync::atomic::AtomicBool;

use php_engine::tier::{self, Fatal, FrameRef, FuncRef, Request, Stepped, TraceNum};
use php_heap::Zval;
use php_unit::{MapPtrOffset, OplineIdx};

use crate::lower::{self, Stood};
use crate::table::{Exit, Trace};
use crate::types::{ExitId, Landing};

/// What a request is to the crossing: the interrupt compiled code tests at a
/// loop header, and whether a kernel that returned left a throwable
/// pending.
pub(crate) trait Crossed {
    /// The request's interrupt, which the engine's timer and signal
    /// delivery raise (design §5.8).
    fn interrupt(&self) -> &AtomicBool;

    /// Whether a throwable or an exit stands pending, or a fatal is
    /// unwinding the request, which leaves every frame as an exit does.
    fn thrown(&self) -> bool;
}

impl Crossed for Request {
    fn interrupt(&self) -> &AtomicBool {
        self.limits().interrupt().raised()
    }

    fn thrown(&self) -> bool {
        self.diagnostics().raised().is_some() || self.diagnostics().bailing_out()
    }
}

/// The first argument of every trace and every kernel wrapper
/// (`jit-design.md` §4.6): a local of the `enter` that made it, its
/// address held by compiled code for exactly that call. Compiled code
/// reads `req` at offset zero and `interrupt` at
/// [`lower::INTERRUPT_AT`], and nothing else; `pending` has no C layout
/// and is the wrappers' alone. It borrows the request mutably for `'a`,
/// so the request outlives it and nothing else touches the request
/// while compiled code may.
#[repr(C)]
pub(crate) struct ExitCtx<'a, R = Request> {
    req: *mut R,
    interrupt: *const AtomicBool,
    pending: Option<Pending>,
    lent: PhantomData<&'a mut R>,
}

// The two offsets compiled code reads, held to the ones `lower` emits.
const _: () = assert!(offset_of!(ExitCtx<'static>, req) == 0);
const _: () = assert!(
    lower::INTERRUPT_AT >= 0
        && offset_of!(ExitCtx<'static>, interrupt) == lower::INTERRUPT_AT.unsigned_abs() as usize
);

impl<'a, R: Crossed> ExitCtx<'a, R> {
    /// The `ExitCtx` over `req`, which it borrows for as long as it lives,
    /// with the request's interrupt taken once, so compiled code needs no
    /// layout of the request.
    pub(crate) fn new(req: &'a mut R) -> ExitCtx<'a, R> {
        let interrupt = ptr::from_ref(req.interrupt());
        ExitCtx {
            req: ptr::from_mut(req),
            interrupt,
            pending: None,
            lent: PhantomData,
        }
    }
}

impl<R> ExitCtx<'_, R> {
    /// What a wrapper kept for `enter`, taken: a fatal to return, or a
    /// panic payload to resume.
    pub(crate) fn take_pending(&mut self) -> Option<Pending> {
        self.pending.take()
    }
}

/// What a wrapper caught and compiled code cannot carry.
pub(crate) enum Pending {
    /// The kernel raised a fatal, an `exit` or the memory limit.
    Bailout(Fatal),
    /// The kernel panicked; `enter` resumes the unwind past the code.
    Panic(Box<dyn Any + Send>),
}

/// What a wrapper answers compiled code, which branches on it: on
/// `Continue` the kernel's value is in the out slot, and on any other the
/// trace leaves through the opline's exit, or answers
/// [`lower::ABANDONED`] where it names none, and `enter` reads the
/// `ExitCtx`. `Exception` is a throwable the kernel left pending on the
/// request, its value written all the same.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Continue = 0,
    Exception = 1,
    Bailout = 2,
    Panic = 3,
}

/// What a kernel answers through its wrapper's out pointer, written as
/// compiled code reads it.
pub(crate) trait Out: Sized {
    /// `self` written to `out`.
    ///
    /// # Safety
    ///
    /// `out` is valid for one write of a `Self`; for `()`, anything.
    unsafe fn put(self, out: *mut Self);
}

impl Out for () {
    /// A kernel that answers nothing has no out pointer.
    unsafe fn put(self, _: *mut ()) {}
}

impl Out for u64 {
    unsafe fn put(self, out: *mut u64) {
        // SAFETY: the caller hands a slot valid for one write.
        unsafe { out.write(self) };
    }
}

impl Out for Stood {
    unsafe fn put(self, out: *mut Stood) {
        // SAFETY: the caller hands compiled code's `Stood`, valid for one
        // write.
        unsafe { out.write(self) };
    }
}

impl Out for Zval {
    /// `ZVAL_COPY_VALUE`: the value and its type info, and never the
    /// slot's `u2` word, which is the holder's (`ZvalRef::write`).
    unsafe fn put(self, out: *mut Zval) {
        // SAFETY: `out` is a frame slot, sixteen aligned bytes, and the
        // first twelve of a zval are its value and its type info on the
        // little-endian targets compiled code runs on (`Zval::TYPE_AT`).
        unsafe {
            ptr::copy_nonoverlapping(
                ptr::from_ref(&self).cast::<u8>(),
                out.cast::<u8>(),
                VALUE_AND_TYPE,
            );
        };
    }
}

/// The bytes of a zval `ZVAL_COPY_VALUE` writes: its value word and its
/// 32-bit type info.
const VALUE_AND_TYPE: usize = Zval::TYPE_AT + size_of::<u32>();
const _: () =
    assert!(Zval::TYPE_AT == 8 && size_of::<Zval>() == 16 && cfg!(target_endian = "little"));

/// One kernel call across the crossing: the request rebuilt from `cx`,
/// the kernel called inside `catch_unwind`, its value written to `out`, and
/// what came back mapped to a [`Status`]. Inlined into each wrapper, so a
/// wrapper is one frame around its kernel.
///
/// # Safety
///
/// `cx` is one [`ExitCtx::new`] made, live and not otherwise
/// borrowed for the call, and `out` is as [`Out::put`] asks.
#[inline(always)] // jit-design.md §4.6: the wrapper is the one frame between compiled code and the kernel
#[allow(clippy::inline_always)]
pub(crate) unsafe fn crossed<R: Crossed, T: Out>(
    cx: *mut ExitCtx<'_, R>,
    out: *mut T,
    kernel: impl FnOnce(&mut R) -> Result<T, Fatal>,
) -> Status {
    // SAFETY: the caller hands the live context `enter` made, borrowed by
    // nothing else for the length of this call.
    let cx = unsafe { &mut *cx };
    // SAFETY: `req` came from the `&'a mut R` the `ExitCtx` borrows for its
    // whole life, and compiled code touches the request only through
    // wrappers, one at a time on its stack.
    let req = unsafe { &mut *cx.req };
    match catch_unwind(AssertUnwindSafe(|| kernel(req))) {
        // SAFETY: the caller hands an out slot as `put` asks.
        Ok(answer) => unsafe { answered(cx, out, answer) },
        Err(payload) => {
            cx.pending = Some(Pending::Panic(payload));
            Status::Panic
        }
    }
}

/// What a kernel that returned answered, mapped to a [`Status`]: its
/// value written to `out`, or its fatal kept in `cx` for `enter`.
///
/// # Safety
///
/// `out` is as [`Out::put`] asks, and the kernel's borrow of the request
/// `cx` carries has ended, so it is read here as no one else holds it.
#[inline(always)] // part of `crossed`, the one frame around the kernel
#[allow(clippy::inline_always)]
unsafe fn answered<R: Crossed, T: Out>(
    cx: &mut ExitCtx<'_, R>,
    out: *mut T,
    answer: Result<T, Fatal>,
) -> Status {
    match answer {
        Ok(value) => {
            // SAFETY: the caller hands an out slot as `put` asks.
            unsafe { value.put(out) };
            // SAFETY: as in `crossed`, `req` is the request the `ExitCtx`
            // borrows, and the kernel's borrow of it has ended.
            if unsafe { &*cx.req }.thrown() {
                Status::Exception
            } else {
                Status::Continue
            }
        }
        Err(fatal) => {
            cx.pending = Some(Pending::Bailout(fatal));
            Status::Bailout
        }
    }
}

/// The room of the frame a kernel serves an opline of, php's
/// `execute_data`, as compiled code hands it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Room(pub(crate) *mut u8);

/// One `extern "C"` wrapper per listed kernel (`jit-design.md` §4.6,
/// §5.1), in a `mod` the invocation names, over the request type it
/// names. A line `name(a: A) -> T = tier::item;` writes
/// `name(cx, frame, opline, a, out: *mut T) -> Status`, which calls the
/// `name` beside the invocation as `name(req, Room(frame),
/// OplineIdx(opline), a)`; a line with no `-> T` has no out pointer, and
/// `= tier::item` names the engine item the kernel serves through, which
/// the wrappers' `mod` imports, so the item's removal fails this build,
/// and which `api-check` reads for `tier.lock`. The engine's list names
/// the `lower::Kernel` of each line, `Step => step(..)`, and gets
/// `symbols()`, one address per kernel, its match over `Kernel` holding
/// the list to every kernel `lower` calls.
macro_rules! kernels {
    ($request:ty => mod $module:ident { $( $kernel:ident => $name:ident ( $($arg:ident : $ty:ty),* ) $(-> $out:ty)? $(= $($seg:ident)::+)? ; )* }) => {
        kernels!(@module $request => $module { $( $name ( $($arg : $ty),* ) $(-> $out)? $(= $($seg)::+)? ; )* } {
            /// The kernel symbol table `lower` calls the wrappers through.
            pub(crate) fn symbols() -> $crate::lower::Symbols {
                $crate::lower::Symbols::new(|kernel| match kernel {
                    $( $crate::lower::Kernel::$kernel => ($name as *const ()).addr(), )*
                })
            }
        });
    };
    ($request:ty => mod $module:ident { $( $name:ident ( $($arg:ident : $ty:ty),* ) $(-> $out:ty)? $(= $($seg:ident)::+)? ; )* }) => {
        kernels!(@module $request => $module { $( $name ( $($arg : $ty),* ) $(-> $out)? $(= $($seg)::+)? ; )* } {});
    };
    (@module $request:ty => $module:ident { $( $name:ident ( $($arg:ident : $ty:ty),* ) $(-> $out:ty)? $(= $($seg:ident)::+)? ; )* } { $($symbols:tt)* }) => {
        pub(crate) mod $module {
            #[allow(unused_imports, reason = "the types a list names resolve beside the invocation, and a list may name none")]
            use super::*;
            $(
                $( #[allow(unused_imports, reason = "the engine item the kernel serves through, held to exist")] use php_engine::$($seg)::+ as _; )?
                kernels!(@wrapper $request, $name ( $($arg : $ty),* ) $(-> $out)?);
            )*
            $($symbols)*
        }
    };
    (@wrapper $request:ty, $name:ident ( $($arg:ident : $ty:ty),* ) -> $out:ty) => {
        /// The wrapper compiled code calls for the kernel of this name.
        ///
        /// # Safety
        ///
        /// As [`crossed`](crate::enter::crossed): `cx` is live and
        /// unborrowed, and `out` is valid for one write.
        pub(crate) unsafe extern "C" fn $name(
            cx: *mut $crate::enter::ExitCtx<'_, $request>,
            frame: *mut u8,
            opline: u32,
            $($arg: $ty,)*
            out: *mut $out,
        ) -> $crate::enter::Status {
            let (room, at) = ($crate::enter::Room(frame), php_unit::OplineIdx(opline));
            // SAFETY: this wrapper's own contract, handed on whole.
            unsafe { $crate::enter::crossed(cx, out, |req| super::$name(req, room, at, $($arg),*)) }
        }
    };
    (@wrapper $request:ty, $name:ident ( $($arg:ident : $ty:ty),* )) => {
        /// The wrapper compiled code calls for the kernel of this name.
        ///
        /// # Safety
        ///
        /// As [`crossed`](crate::enter::crossed): `cx` is live and
        /// unborrowed.
        pub(crate) unsafe extern "C" fn $name(
            cx: *mut $crate::enter::ExitCtx<'_, $request>,
            frame: *mut u8,
            opline: u32,
            $($arg: $ty,)*
        ) -> $crate::enter::Status {
            let (room, at) = ($crate::enter::Room(frame), php_unit::OplineIdx(opline));
            // SAFETY: this wrapper's own contract; a kernel that answers
            // nothing has no out pointer.
            unsafe { $crate::enter::crossed(cx, std::ptr::null_mut::<()>(), |req| super::$name(req, room, at, $($arg),*)) }
        }
    };
}

kernels! {
    Request => mod wrapped {
        Step => step() -> Stood = tier::step;
        PushCall => push_call(callee: u32) -> Stood = tier::step;
        Release => release(word: u64, type_info: u32) = tier::release;
        Copy => copy(word: u64, type_info: u32) -> Zval;
    }
}

/// The `step` kernel (`jit-design.md` §5.2): the opline at `at` served
/// whole, and where the interpreter stands after it.
fn step(req: &mut Request, room: Room, at: OplineIdx) -> Result<Stood, Fatal> {
    let frame = standing(req, room, at)?;
    let stepped = tier::step(req, frame)?;
    stood(req, stepped)
}

/// The `push_call` kernel (`jit-design.md` §4.3): the `INIT_*` at `at`
/// served as its handler serves it where its cache slot holds the op
/// array the engine numbers `callee`, which its inline path pushes with
/// no lookup, as php's trace guards `CACHED_PTR` against the function it
/// recorded; where it holds another, or none yet, nothing is pushed and
/// the call stood is null, so compiled code leaves for the interpreter to
/// serve the opline whole.
fn push_call(req: &mut Request, room: Room, at: OplineIdx, callee: u32) -> Result<Stood, Fatal> {
    let frame = standing(req, room, at)?;
    if tier::primed(req, frame, at).and_then(engine_offset) != Some(callee) {
        let refused = where_stood(req, frame)?;
        return Ok(Stood {
            call: ptr::null_mut(),
            ..refused
        });
    }
    let stepped = tier::step(req, frame)?;
    stood(req, stepped)
}

/// The `release` kernel: php's `zval_ptr_dtor` of the counted value
/// compiled code gives up, a destructor's throw left pending.
fn release(
    req: &mut Request,
    room: Room,
    at: OplineIdx,
    word: u64,
    type_info: u32,
) -> Result<(), Fatal> {
    standing(req, room, at)?;
    tier::release(req, zval(word, type_info));
    Ok(())
}

/// The `copy` kernel: php's `ZVAL_COPY_DEREF`, the value a reference
/// holds in place of the reference, with one owner more.
#[allow(
    clippy::unnecessary_wraps,
    reason = "a kernel answers a Result; this one never fails"
)]
fn copy(_: &mut Request, _: Room, _: OplineIdx, word: u64, type_info: u32) -> Result<Zval, Fatal> {
    let z = zval(word, type_info);
    let z = z.as_reference().map_or(z, |reference| reference.val());
    z.addref();
    Ok(z)
}

/// The frame whose room compiled code handed a kernel, stood at the
/// opline the kernel serves: php's `SAVE_OPLINE()`, so what it raises is
/// reported at that opline and a step serves it.
fn standing(req: &mut Request, room: Room, at: OplineIdx) -> Result<FrameRef, Fatal> {
    let frame = NonNull::new(room.0)
        .and_then(|room| tier::at_room(req, room))
        .ok_or_else(|| internal("a kernel was handed a room no frame on the stack has"))?;
    tier::stand(req, frame, at);
    Ok(frame)
}

/// Where the interpreter stands after a step: the frame it goes on in,
/// whatever the step pushed or popped.
fn stood(req: &mut Request, stepped: Stepped) -> Result<Stood, Fatal> {
    match stepped {
        Stepped::Same(frame)
        | Stepped::Pushed(frame)
        | Stepped::Popped(frame)
        | Stepped::Moved(frame)
        | Stepped::Threw(frame) => where_stood(req, frame),
        Stepped::Done => Err(internal(
            "a kernel's step ended the serving a trace runs in",
        )),
    }
}

/// The [`Stood`] of `frame`: its room, the room of the call under way in
/// it, `EX(call)`, the engine offset of its op array, none where the
/// request alone holds it, and its opline.
fn where_stood(req: &mut Request, frame: FrameRef) -> Result<Stood, Fatal> {
    let call = match tier::building(req).filter(|building| building.call != frame) {
        Some(building) => tier::room(req, building.call).map_or(ptr::null_mut(), NonNull::as_ptr),
        None => ptr::null_mut(),
    };
    let room =
        tier::room(req, frame).ok_or_else(|| internal("a step went on in a frame with no room"))?;
    Ok(Stood {
        frame: room.as_ptr(),
        call,
        func: tier::func(req, frame)
            .and_then(engine_offset)
            .unwrap_or(u32::MAX),
        opline: tier::frame(req, frame).map_or(0, |view| view.opline().0),
    })
}

/// The engine offset that numbers the op array `f` names, which compiled
/// code compares a callee with; none for a request's own.
fn engine_offset(f: FuncRef) -> Option<u32> {
    match f.offset() {
        MapPtrOffset::Engine(n) => Some(n),
        MapPtrOffset::Request(_) => None,
    }
}

/// The zval compiled code hands a kernel as its two words, its value, a
/// pointer for a counted type, and php's `Z_TYPE_INFO`, with a `u2` of
/// zero.
fn zval(word: u64, type_info: u32) -> Zval {
    /// A zval's two words as compiled code holds them.
    #[repr(C)]
    struct Words {
        value: *mut u8,
        info: u64,
    }
    const _: () = assert!(
        size_of::<Words>() == size_of::<Zval>() && offset_of!(Words, info) == Zval::TYPE_AT
    );
    let words = Words {
        value: ptr::with_exposed_provenance_mut(usize::try_from(word).unwrap_or(usize::MAX)),
        info: u64::from(type_info),
    };
    // SAFETY: `Zval` is `repr(C)`, a word that is an integer or a pointer
    // and then the info word, sixteen bytes `Words` lays out the same,
    // and any bits are a zval's; compiled code read these from a slot the
    // interpreter wrote, so the pointer is the one a counted type names.
    unsafe { std::mem::transmute::<Words, Zval>(words) }
}

/// Where a trace left the interpreter (`jit-design.md` §4.6): the exit it
/// left through, and the frame the loop goes on in, landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Exited {
    pub(crate) exit: ExitId,
    pub(crate) frame: FrameRef,
}

/// What compiled code is entered as (`jit-design.md` §4.3): the
/// `ExitCtx` and the room of the frame the trace was recorded in, and the
/// exit it left through, or [`lower::ABANDONED`].
type Entry = unsafe extern "C" fn(*mut ExitCtx<'_>, *mut u8) -> u32;

/// `trace` called on `frame` (`jit-design.md` §4.6), with the request's
/// trace number, php's `EG(jit_trace_num)`, the trace's for the length of
/// the call and the one before it again after. A panic a wrapper caught
/// is resumed and a fatal returned, read before the exit; otherwise the
/// exit is landed ([`land`]). Reads no word and follows no link: what
/// happens next is `dispatch`'s.
///
/// The crossing's invariant is per fiber stack (§4.6): a fiber's compiled
/// loop suspends from inside the kernel that serves `Fiber::suspend`, its
/// machine frame and `ExitCtx` waiting on the fiber's stack, while
/// `$main`'s compiled loop resumes it from inside the kernel that serves
/// `resume`, each loop going on where it stood:
///
/// ```php
/// $f = new Fiber(function () { $s = 0; for ($i = 0; $i < 300; $i++) { $s += Fiber::suspend($i); } return $s; });
/// $v = $f->start(); $t = 0;
/// for ($j = 0; $j < 300; $j++) { $t += $v; $v = $f->resume($j); }
/// echo $t, " ", $f->getReturn(), "\n";
/// ```
/// ```expect exit=0 regime=jit-force process
/// 44850 44850
/// ```
pub(crate) fn enter(req: &mut Request, frame: FrameRef, trace: &Trace) -> Result<Exited, Fatal> {
    let room = tier::room(req, frame)
        .ok_or_else(|| internal("a trace entered on a frame with no room"))?;
    // SAFETY: the table published bytes `lower` compiled for this ABI at
    // this address, final and executable for as long as the trace, which
    // the borrow of the epoch's table keeps.
    let entry = unsafe { std::mem::transmute::<*const u8, Entry>(trace.entry().as_ptr()) };
    let before = tier::trace_num(req);
    tier::set_trace_num(req, TraceNum::from_raw(trace.id.get()));
    let mut cx = ExitCtx::new(&mut *req);
    // SAFETY: `cx` lives on this frame for the whole call and nothing but
    // the code and its wrappers touches the request meanwhile; `room` is
    // the room of a frame on the request's stack.
    let answer = unsafe { entry(&raw mut cx, room.as_ptr()) };
    let pending = cx.take_pending();
    tier::set_trace_num(req, before);
    match pending {
        Some(Pending::Panic(payload)) => resume_unwind(payload),
        Some(Pending::Bailout(fatal)) => return Err(fatal),
        None => {}
    }
    let id = u16::try_from(answer)
        .ok()
        .filter(|id| usize::from(*id) < trace.exits.len())
        .ok_or_else(|| internal("a trace left through no exit of its own and kept nothing"))?;
    let frame = land(req, &trace.exits[usize::from(id)])?;
    Ok(Exited {
        exit: ExitId(id),
        frame,
    })
}

/// The landing (the glossary's Landing): the frame the loop goes on in,
/// read from the stack, stood at the exit's opline, and the temporaries
/// the opline before it read freed for `FreeTemps`. A throwable a kernel
/// left pending stands where it was raised, at the opline its frame was
/// stood at, which is where the loop unwinds from, so nothing is landed.
fn land(req: &mut Request, exit: &Exit) -> Result<FrameRef, Fatal> {
    let frame = tier::current(req).ok_or_else(|| internal("a trace left no frame standing"))?;
    if req.thrown() {
        return Ok(frame);
    }
    if let Some(at) = exit.opline {
        tier::stand(req, frame, at);
    }
    match exit.landing {
        Landing::Plain | Landing::RestoreCall => {}
        Landing::FreeTemps { op1, op2 } => {
            let read = exit
                .opline
                .and_then(|at| at.0.checked_sub(1))
                .ok_or_else(|| internal("a landing frees what no opline before its own read"))?;
            tier::free_operands(req, frame, OplineIdx(read), op1, op2);
        }
    }
    Ok(frame)
}

/// The crossing's `InternalError`.
fn internal(why: &str) -> Fatal {
    Fatal::error(format!("InternalError: {why}"))
}

/// The stand-in request of the bench and the tests: one word a kernel
/// writes, so a call has an effect neither arm can fold away, and
/// whether a kernel left a throwable pending.
#[cfg(any(test, feature = "bench"))]
#[derive(Debug, Default)]
pub(crate) struct Tally {
    word: u64,
    thrown: bool,
}

#[cfg(any(test, feature = "bench"))]
impl Crossed for Tally {
    fn interrupt(&self) -> &AtomicBool {
        static NEVER: AtomicBool = AtomicBool::new(false);
        &NEVER
    }

    fn thrown(&self) -> bool {
        self.thrown
    }
}

/// The test kernel: one step of a linear congruential generator over the
/// tally, kept out of line so that each arm calls it. It raises a fatal
/// at the last `n`, which no loop of calls reaches, so that it can fail and unwind
/// as a kernel does and the optimiser keeps the wrapper's whole shape: a
/// kernel it could prove never fails nor unwinds would lose both the
/// mapping and the `catch_unwind` the bar measures.
#[cfg(any(test, feature = "bench"))]
#[inline(never)]
fn tally(t: &mut Tally, _: Room, _: OplineIdx, n: u64) -> Result<u64, Fatal> {
    if n == u64::MAX {
        return Err(Fatal::error("the tally's last step"));
    }
    t.word = t
        .word
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(n);
    Ok(t.word)
}

#[cfg(any(test, feature = "bench"))]
kernels! { Tally => mod tallied { tally(n: u64) -> u64; } }

/// The bench bar's program (#2742): the same loop of kernel calls, through
/// the wrapper or straight to the kernel, each through a function pointer
/// the optimiser cannot see through, as compiled code calls a wrapper by
/// its address.
#[cfg(feature = "bench")]
pub mod bench {
    use std::hint::black_box;
    use std::panic::resume_unwind;

    use php_unit::OplineIdx;

    use super::{ExitCtx, Fatal, Pending, Room, Status, Tally, answered, tallied};

    /// Which way the loop's calls go.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Through {
        /// Each call through the kernel's `extern "C"` wrapper.
        Wrapper,
        /// Each call through [`tally_without_unwind_guard`]: the wrapper
        /// with no `catch_unwind`, which no production wrapper is (#2879).
        WrapperBare,
        /// Each call to the kernel itself.
        Direct,
    }

    impl Through {
        /// The way an arm's argument names: `wrapper`, `wrapper-bare` or
        /// `direct`.
        #[must_use]
        pub fn named(name: &str) -> Option<Through> {
            match name {
                "wrapper" => Some(Through::Wrapper),
                "wrapper-bare" => Some(Through::WrapperBare),
                "direct" => Some(Through::Direct),
                _ => None,
            }
        }
    }

    /// The wrapper's address as compiled code holds it.
    type Wrapper =
        unsafe extern "C" fn(*mut ExitCtx<'_, Tally>, *mut u8, u32, u64, *mut u64) -> Status;

    /// The bench's arm `wrapper-bare` and nothing else: `tallied::tally`'s
    /// shape, the request rebuilt from `cx`, the kernel's value written
    /// through `out` and mapped to a `Status`, with no `catch_unwind`
    /// around the kernel, so `wrapper` against it is what the guard costs
    /// on the path that does not unwind, and it against `direct` what the
    /// frame costs (#2879). A kernel's panic here aborts the process, as
    /// one through an `extern "C"` function does, so a wrapper compiled
    /// code calls is never this (`jit-design.md` §4.6); the test kernel
    /// raises none.
    ///
    /// # Safety
    ///
    /// As [`crossed`](super::crossed): `cx` is live and unborrowed, and
    /// `out` is valid for one write.
    unsafe extern "C" fn tally_without_unwind_guard(
        cx: *mut ExitCtx<'_, Tally>,
        frame: *mut u8,
        opline: u32,
        n: u64,
        out: *mut u64,
    ) -> Status {
        // SAFETY: the caller hands the live context, borrowed by nothing
        // else for the length of this call.
        let cx = unsafe { &mut *cx };
        // SAFETY: `req` came from the `&mut Tally` the `ExitCtx` borrows.
        let req = unsafe { &mut *cx.req };
        let answer = super::tally(req, Room(frame), OplineIdx(opline), n);
        // SAFETY: the caller hands an out slot valid for one write.
        unsafe { answered(cx, out, answer) }
    }

    /// `n` calls of the test kernel `through` one way, and the tally they
    /// leave, which is the same whichever way they went. A fatal is
    /// returned and a panic resumed, as `enter` answers them, though the
    /// test kernel raises neither.
    pub fn calls(through: Through, n: u64) -> Result<u64, Fatal> {
        let mut tally = Tally::default();
        let mut last = 0;
        match through {
            Through::Wrapper | Through::WrapperBare => {
                let wrapper: Wrapper = black_box(if through == Through::Wrapper {
                    tallied::tally
                } else {
                    tally_without_unwind_guard
                });
                let mut cx = ExitCtx::new(&mut tally);
                for i in 0..n {
                    // SAFETY: `cx` is this frame's live context, borrowed by
                    // nothing else, and `last` takes one write.
                    let status =
                        unsafe { wrapper(&raw mut cx, std::ptr::null_mut(), 0, i, &raw mut last) };
                    // As compiled code does: the answer alone on the way
                    // through, the `ExitCtx` read only when a call stopped.
                    if status != Status::Continue {
                        return match cx.take_pending() {
                            Some(Pending::Panic(payload)) => resume_unwind(payload),
                            Some(Pending::Bailout(fatal)) => Err(fatal),
                            None => Err(Fatal::error(
                                "InternalError: a kernel wrapper stopped and kept nothing",
                            )),
                        };
                    }
                }
            }
            Through::Direct => {
                let kernel: fn(&mut Tally, Room, OplineIdx, u64) -> Result<u64, Fatal> =
                    black_box(super::tally);
                for i in 0..n {
                    last = kernel(&mut tally, Room(std::ptr::null_mut()), OplineIdx(0), i)?;
                }
            }
        }
        Ok(last)
    }
}

#[cfg(test)]
pub(crate) mod tests;
