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
//! **The engine's kernels** are `lower::Kernel`'s (§5.2), among them
//! `step`, the opline served whole through `tier::step`; `push_call`, an
//! inline `INIT_*` served the same way where the callee its cache slot
//! holds is the one recorded, and refused before anything is pushed
//! where it is not; `release`, php's `zval_ptr_dtor`; and `free_cv`,
//! php's `zend_jit_free_cv` at a leave. Each that reads its frame stands
//! it at the opline it serves first, php's `SAVE_OPLINE()`, so what it
//! raises is reported there. Each line of the list names the engine item
//! it serves through, which `api-check` writes into `tier.lock`'s kernels
//! section where the face does not list it already (§5.3).
//!
//! **What `enter` does not do yet.** A call `push_call` began is on the
//! engine's stack, pushed by the engine, where php's compiled code keeps
//! it in a register until an exit's `RESTORE_CALL` links it, so
//! a landing's `restores_call` restores nothing.
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

use php_engine::tier::{
    self, CachedSlot, Epoch, Fatal, FrameRef, FramelessAt, FuncRef, Held, Request, SafepointIdx,
    Stepped, TraceNum, Word,
};
use php_heap::{Arr, HeaderImage, Key, Scalar, Zval, ZvalRef, ZvalType};
use php_unit::{MapPtrOffset, OperandKind, OplineIdx};

use crate::lower::{self, OperandKinds, Stood, engine_offset};
use crate::table::{Exit, Trace};
use crate::types::ExitId;

/// What a request is to the crossing: the interrupt compiled code tests at a
/// loop header, and whether a kernel that returned left the request
/// unwinding.
pub(crate) trait Crossed {
    /// The request's interrupt, which the engine's timer and signal
    /// delivery raise (design §5.8).
    fn interrupt(&self) -> &AtomicBool;

    /// Whether the request unwinds: a throwable or an exit stands
    /// pending, or a fatal is under way, which leaves every frame as an
    /// exit does.
    fn unwinding(&self) -> bool;
}

impl Crossed for Request {
    fn interrupt(&self) -> &AtomicBool {
        self.limits().interrupt().raised()
    }

    fn unwinding(&self) -> bool {
        self.diagnostics().raised().is_some() || self.diagnostics().bailing_out()
    }
}

/// The first argument of every trace and every kernel wrapper
/// (`jit-design.md` §4.6): a local of the `enter` that made it, its
/// address held by compiled code for exactly that call. Compiled code
/// reads `req` at offset zero, `interrupt` at [`lower::INTERRUPT_AT`],
/// `at` at [`lower::ENTRY_AT`], `stood` at [`lower::STOOD_AT`], `stack` at
/// [`lower::STACK_AT`], `images` at [`lower::IMAGES_AT`], `root` at
/// [`lower::ROOT_AT`], `ready` at [`lower::READY_AT`], `rows` at
/// [`lower::ROWS_AT`] and `trace` at [`lower::TRACE_AT`], and nothing
/// else; a jump to another trace writes the entry fields, `at` to
/// `ready` and `trace`, as `enter` writes them (`lower`'s *The entry*);
/// `pending` has no C layout
/// and is the wrappers' alone. It borrows the request mutably for `'a`,
/// so the request outlives it and nothing else touches the request
/// while compiled code may.
#[repr(C)]
pub(crate) struct ExitCtx<'a, R = Request> {
    req: *mut R,
    interrupt: *const AtomicBool,
    /// Where the trace is entered.
    at: At,
    /// Where the frame stands as the trace enters, as a kernel reports
    /// it, laid down for a trace whose code reads it there
    /// ([`Trace::entry_stood`]); null and zero otherwise.
    stood: Stood,
    /// The request's VM stack, which code that pushes and leaves a plain
    /// call in place moves the words of (`tier::stack`); null for a
    /// stand-in request.
    stack: *mut u8,
    /// The request's addresses the trace's code reads, the headers of the
    /// calls it pushes in place and the cache slots it reads ([`Images`]).
    images: *const usize,
    /// The place on the stack of the frame the trace is entered on, and,
    /// where it escapes, of the frame it escaped into.
    root: u32,
    /// Not zero where the trace pushes and makes its calls in place in
    /// this request: each one's header is the request's, and the caches of
    /// the callees it enters are laid down ([`Trace::laid`]).
    ready: u32,
    /// The request's rows ([`Rows`]), by which a jump finds the row of the
    /// trace it enters: [`NO_ROWS`] for a request that keeps none.
    rows: *const usize,
    /// The number of the trace entered: the one `enter` calls, and then
    /// whichever a jump entered, which answers.
    trace: u32,
    pending: Option<Pending>,
    /// The epoch the trace was taken from, which holds every op array it
    /// recorded: what a kernel serves those op arrays out of
    /// ([`tier::step_held`]), none for a stand-in request.
    holding: Option<Holding<'a>>,
    lent: PhantomData<&'a mut R>,
}

// The two offsets compiled code reads, held to the ones `lower` emits.
const _: () = assert!(offset_of!(ExitCtx<'static>, req) == 0);
const _: () = assert!(
    lower::INTERRUPT_AT >= 0
        && offset_of!(ExitCtx<'static>, interrupt) == lower::INTERRUPT_AT.unsigned_abs() as usize
);
const _: () = assert!(
    lower::ENTRY_AT >= 0
        && offset_of!(ExitCtx<'static>, at) == lower::ENTRY_AT.unsigned_abs() as usize
);
const _: () = assert!(
    lower::STOOD_AT >= 0
        && offset_of!(ExitCtx<'static>, stood) == lower::STOOD_AT.unsigned_abs() as usize
);
const _: () = assert!(
    lower::STACK_AT >= 0
        && offset_of!(ExitCtx<'static>, stack) == lower::STACK_AT.unsigned_abs() as usize
);
const _: () = assert!(
    lower::IMAGES_AT >= 0
        && offset_of!(ExitCtx<'static>, images) == lower::IMAGES_AT.unsigned_abs() as usize
);
const _: () = assert!(
    lower::ROOT_AT >= 0
        && offset_of!(ExitCtx<'static>, root) == lower::ROOT_AT.unsigned_abs() as usize
);
const _: () = assert!(
    lower::READY_AT >= 0
        && offset_of!(ExitCtx<'static>, ready) == lower::READY_AT.unsigned_abs() as usize
);
const _: () = assert!(
    lower::ROWS_AT >= 0
        && offset_of!(ExitCtx<'static>, rows) == lower::ROWS_AT.unsigned_abs() as usize
);
const _: () = assert!(
    lower::TRACE_AT >= 0
        && offset_of!(ExitCtx<'static>, trace) == lower::TRACE_AT.unsigned_abs() as usize
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
            at: At::First,
            stood: Stood::NOWHERE,
            stack: ptr::null_mut(),
            images: ptr::null(),
            root: 0,
            ready: 0,
            rows: NO_ROWS.as_ptr(),
            trace: 0,
            pending: None,
            holding: None,
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

// Compiled code reads a throw's status by `lower`'s number for it.
const _: () = assert!(Status::Exception as u32 == lower::THREW);

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
    kernel: impl FnOnce(&mut R, Option<&Holding<'_>>) -> Result<T, Fatal>,
) -> Status {
    // SAFETY: the caller hands the live context `enter` made, borrowed by
    // nothing else for the length of this call.
    let cx = unsafe { &mut *cx };
    // SAFETY: `req` came from the `&'a mut R` the `ExitCtx` borrows for its
    // whole life, and compiled code touches the request only through
    // wrappers, one at a time on its stack.
    let req = unsafe { &mut *cx.req };
    let holding = cx.holding.as_ref();
    match catch_unwind(AssertUnwindSafe(|| kernel(req, holding))) {
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
            if unsafe { &*cx.req }.unwinding() {
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

/// Where a kernel serves: the room of its frame, the opline, and the
/// hold on the epoch the trace that called it holds its op arrays in,
/// none for a stand-in request.
#[derive(Debug, Clone, Copy)]
pub(crate) struct On<'h, 'a> {
    pub(crate) room: Room,
    pub(crate) at: OplineIdx,
    pub(crate) holding: Option<&'h Holding<'a>>,
}

/// The epoch an `enter` took its trace from, and the op array its
/// kernels served last, by the engine offset that numbers it: compiled
/// code steps the oplines of one op array one after another, so a kernel finds it
/// again with one compare and no row of the epoch read.
#[derive(Debug)]
pub(crate) struct Holding<'a> {
    epoch: &'a Epoch,
    last: std::cell::Cell<Option<(u32, Held<'a>)>>,
}

impl<'a> Holding<'a> {
    /// The hold on `epoch`, having served nothing yet.
    fn of(epoch: &'a Epoch) -> Holding<'a> {
        Holding {
            epoch,
            last: std::cell::Cell::new(None),
        }
    }

    /// Whether the op array the engine numbers `func` is the one served
    /// last, whose row the epoch holds at that offset.
    fn served(&self, func: u32) -> bool {
        self.last.get().is_some_and(|(last, _)| last == func)
    }

    /// The op array the engine numbers `func`, as the epoch holds it.
    fn held(&self, func: u32) -> Option<Held<'a>> {
        if let Some((last, held)) = self.last.get()
            && last == func
        {
            return Some(held);
        }
        let held = self.epoch.held(FuncRef::at(MapPtrOffset::Engine(func)))?;
        self.last.set(Some((func, held)));
        Some(held)
    }
}

/// What the request lends a crossing of its own state for as long as the
/// crossing serves (`jit-design.md` §4.6, #3437): the tier's state, and
/// through it the engine's `Shared`; the request's hold on the epoch it
/// started in, which its traces are taken from and their kernels serve
/// out of, taken once, at the request's first visit; and the epoch's
/// trace table, kept from the first visit that finds it made. So an entry
/// takes no hold and looks up no table of its own, as php's enters a
/// trace through the handler its op array holds.
#[derive(Clone, Copy)]
pub(crate) struct Lent<'l> {
    pub(crate) state: &'l crate::JitRequest,
    pub(crate) epoch: &'l Epoch,
    pub(crate) table: Option<&'l crate::table::Table>,
    /// Made by [`lent`] alone, so `epoch` is the one `state` holds.
    _made: (),
}

impl<'l> Lent<'l> {
    /// The engine's state, which the request holds for as long as it serves.
    pub(crate) fn shared(self) -> &'l crate::Shared {
        &self.state.shared
    }

    /// The words the request kept for opline `at` of the op array `func`
    /// names ([`Landings::landed`]), of the epoch the state holds.
    #[inline]
    pub(crate) fn landed(self, func: FuncRef, at: OplineIdx) -> Option<Landed<'l>> {
        // SAFETY: the state's `Landings` is kept through `Lent::keep`
        // alone, which hands it `self.epoch`, and `self.epoch` is the
        // epoch the state holds, laid once (`lent`, which alone makes a
        // `Lent`): every word kept there is of this epoch, which the state
        // holds for at least `'l`.
        unsafe { self.state.landings.landed(self.epoch, func, at) }
    }

    /// Keep the words a return or a leave reads where it goes on at
    /// opline `at` of the op array `func` names ([`Landings::keep`]), of
    /// the epoch the state holds.
    pub(crate) fn keep(
        self,
        func: FuncRef,
        at: OplineIdx,
        header: Option<SafepointIdx>,
    ) -> Landed<'l> {
        self.state.landings.keep(self.epoch, func, at, header)
    }
}

/// How many oplines a request keeps the words of ([`Landings`]): a
/// recursion that returns past each of two calls in turn, as fib's and
/// `make_tree`'s in `binary_trees` do, finds both, with room for a caller's.
const LANDINGS: usize = 4;

/// The sealed word, which a landing reads where its op array's landing
/// words are not laid down.
static SEALED: Word = Word::sealed();

/// Where the request went on last at an opline past a return or a
/// trace's leave (`dispatch::goes_on`), as php's interpreter finds the
/// handler an opline holds in the opline itself with no search: for each
/// of the last [`LANDINGS`] oplines, of an op array the epoch holds, the
/// address of its landing word, where its op array's are laid down, and
/// that of the word of the loop rooted there, where one is; so a return
/// that lands there again reads its word with no row of the epoch read.
/// Kept in the request's tier state and read and written through a lend
/// of it alone ([`Lent::landed`], [`Lent::keep`]), so every address is of
/// a word of the one epoch the state holds.
#[derive(Debug, Default)]
pub(crate) struct Landings {
    kept: [std::cell::Cell<Kept>; LANDINGS],
    next: std::cell::Cell<usize>,
}

/// One opline [`Landings`] keeps; its addresses zero where it keeps
/// none, and `func` none in a place that keeps nothing.
#[derive(Debug, Default, Clone, Copy)]
struct Kept {
    func: Option<FuncRef>,
    at: OplineIdx,
    landing: usize,
    header: usize,
}

/// The words a return or a leave reads where it goes on at an opline
/// ([`Landings::landed`]): its landing word, sealed until a trace armed
/// it, and the word of the loop rooted there, where one is.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Landed<'l> {
    pub(crate) landing: &'l Word,
    pub(crate) header: Option<&'l Word>,
}

impl Landings {
    /// The words kept for opline `at` of the op array `func` names; none
    /// where none are kept. A landing word its op array had not laid down
    /// when it was kept is read again, since a trace may have armed it
    /// since, and kept where it is laid now.
    ///
    /// # Safety
    ///
    /// `epoch` is the one every word kept here was kept of, by
    /// [`Landings::keep`].
    #[inline]
    unsafe fn landed<'e>(
        &self,
        epoch: &'e Epoch,
        func: FuncRef,
        at: OplineIdx,
    ) -> Option<Landed<'e>> {
        let (slot, kept) = (self.kept.iter())
            .map(|slot| (slot, slot.get()))
            .find(|(_, kept)| kept.func == Some(func) && kept.at == at)?;
        let landing = match kept.landing {
            0 => laid_since(epoch, slot, kept),
            // SAFETY: kept by `keep` or `laid_since`, of `epoch`, as this
            // function's contract says.
            landing => unsafe { kept_word(epoch, landing) },
        };
        // SAFETY: kept by `keep`, of `epoch`, as this function's contract
        // says.
        let header = (kept.header != 0).then(|| unsafe { kept_word(epoch, kept.header) });
        Some(Landed { landing, header })
    }

    /// Keep the words a return or a leave reads where it goes on at
    /// opline `at` of the op array `func` names, of `epoch`, the one every
    /// word kept here is of: its landing word, and the word of the loop
    /// rooted there, `header`; in place of the opline kept longest.
    fn keep<'e>(
        &self,
        epoch: &'e Epoch,
        func: FuncRef,
        at: OplineIdx,
        header: Option<SafepointIdx>,
    ) -> Landed<'e> {
        let landing = epoch.laid_landing(func, at);
        let header = header.map(|header| epoch.word(func, header));
        let next = self.next.get();
        if let Some(slot) = self.kept.get(next) {
            slot.set(Kept {
                func: Some(func),
                at,
                landing: landing.map_or(0, address),
                header: header.map_or(0, address),
            });
        }
        self.next.set((next + 1) % LANDINGS);
        Landed {
            landing: landing.unwrap_or(&SEALED),
            header,
        }
    }
}

/// The landing word `kept` keeps none of, its op array's landing words
/// not laid down when it was kept: the word laid since, kept in `slot`
/// from now on, or the sealed word where none is laid yet.
#[cold]
#[inline(never)]
fn laid_since<'e>(epoch: &'e Epoch, slot: &std::cell::Cell<Kept>, kept: Kept) -> &'e Word {
    let Some(func) = kept.func else {
        return &SEALED;
    };
    epoch.laid_landing(func, kept.at).map_or(&SEALED, |word| {
        slot.set(Kept {
            landing: address(word),
            ..kept
        });
        word
    })
}

/// The word at `address`, which [`Landings`] kept, for as long as
/// `epoch` is borrowed.
///
/// # Safety
///
/// `address` is that of a word of `epoch`'s that [`Landings`] kept: a
/// safepoint's word, or a landing word its op array laid down, each of
/// which the epoch's rows keep where it was laid, written only through
/// its atomics, until the epoch drops (`tier::published`).
unsafe fn kept_word(_epoch: &Epoch, address: usize) -> &Word {
    let word = ptr::with_exposed_provenance::<Word>(address);
    // SAFETY: the function's contract: the word stands for as long as
    // `epoch`, which the borrow keeps.
    unsafe { &*word }
}

/// The address of `word`, its provenance exposed, as [`Landings`] keeps it.
fn address(word: &Word) -> usize {
    ptr::from_ref(word).expose_provenance()
}

/// `serve` given the request and what it lends of itself ([`Lent`]): the
/// tier's state looked up once, its hold on the epoch taken the first
/// time, and the epoch's table kept the first time it is made; none for
/// a request the tier laid no state on, a stand-in's.
#[inline]
pub(crate) fn lent<T>(
    req: &mut Request,
    serve: impl FnOnce(&mut Request, Lent<'_>) -> T,
) -> Option<T> {
    let state = tier::state::<crate::JitRequest>(req)?;
    state.epoch.get_or_init(|| tier::epoch(req));
    let lending = tier::lending(req);
    let found = ptr::from_ref(state);
    // SAFETY: the request lays its tier's state once, as it is made, in a
    // box of its own, and drops it only as the request ends; `serve`
    // borrows the request for less than that. Nothing replaces, moves
    // out of or borrows the box mutably meanwhile, and php_engine's types
    // hold that outside `api::request`, where the request is made: the
    // box is `TierState`'s private field, with no `&mut` method, and the
    // request's field holding it is private to `api::request`, which
    // hands it out only shared (`Request::tier_state`, `tier::state`).
    // So the state stands where `tier::state` found it, written only
    // through its cells, for as long as `serve` serves: what the box
    // holds is not the request's own memory, which `serve`'s borrow
    // covers. What the types leave open is `api::request` itself and
    // every file below it, and a swap of two whole requests. Two debug
    // assertions hold those: `lending` fails as a state dropped or
    // replaced while it stands drops (`tier::Lending`), and the one past
    // `serve` catches a state swapped away that stands swapped when
    // `serve` returns; a swap undone before then, or a read of the state
    // during `serve` after one, they do not.
    let state = unsafe { &*found };
    let epoch = state.epoch.get()?;
    let table = match state.table.get() {
        Some(table) => Some(&**table),
        None => crate::table::shared_of_epoch(epoch)
            .map(|table| &**state.table.get_or_init(|| std::sync::Arc::clone(table))),
    };
    let served = serve(
        req,
        Lent {
            state,
            epoch,
            table,
            _made: (),
        },
    );
    // The state stands where it was lent: not replaced or moved while
    // the crossing borrowed it.
    debug_assert!(tier::state::<crate::JitRequest>(req).is_some_and(|now| ptr::eq(now, found)));
    drop(lending);
    Some(served)
}

/// One `extern "C"` wrapper per listed kernel (`jit-design.md` §4.6,
/// §5.1), in a `mod` the invocation names, over the request type it
/// names. A line `name(a: A) -> T = tier::item;` writes
/// `name(cx, frame, opline, a, out: *mut T) -> Status`, which calls the
/// `name` beside the invocation as `name(req, On { room, at, holding },
/// a)`, the room, the opline and the `ExitCtx`'s hold on the epoch
/// ([`On`]); a line with no `-> T` has no out pointer, and
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
            unsafe { $crate::enter::crossed(cx, out, |req, holding| super::$name(req, $crate::enter::On { room, at, holding }, $($arg),*)) }
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
            unsafe { $crate::enter::crossed(cx, std::ptr::null_mut::<()>(), |req, holding| super::$name(req, $crate::enter::On { room, at, holding }, $($arg),*)) }
        }
    };
}

kernels! {
    Request => mod wrapped {
        Step => step(func: u32) -> Stood = tier::step_held;
        Handle => handle(func: u32) = tier::dispatch_held;
        PushCall => push_call(func: u32, callee: u32) -> Stood = tier::step_held;
        Call => call(func: u32, callee: u32) -> Stood = tier::call_held;
        Release => release(word: u64, type_info: u32) = tier::release;
        FreeCv => free_cv(word: u64, type_info: u32) = tier::free_cv;
        Identical => identical(a: u64, a_info: u32, b: u64, b_info: u32) -> u64;
        RopeInit => rope_init(word: u64, type_info: u32) -> u64 = tier::rope_open;
        RopeAdd => rope_add(rope: u64, word: u64, type_info: u32) = tier::rope_push;
        RopeEnd => rope_end(rope: u64, word: u64, type_info: u32) -> Zval = tier::rope_end;
        AssignConcat => assign_concat(place: u64, word: u64, type_info: u32) = tier::assign_concat;
        Concat => concat(a: u64, a_info: u32, b: u64, b_info: u32, kinds: OperandKinds) -> Zval = tier::concat;
        Find => find(array: u64, key: u64, key_info: u32, constant: u32) -> Zval;
        Lookup => lookup(array: u64, key: u64, key_info: u32, constant: u32) -> Zval = tier::lookup_element;
        Leave => leave(word: u64, type_info: u32) -> u64 = tier::return_entry;
        Frameless => frameless(place: u32, at: FramelessAt, kinds: OperandKinds, a: u64, a_info: u32, b: u64, b_info: u32, c: u64, c_info: u32) -> Zval = tier::frameless_call;
        FreeThis => free_this() = tier::free_this;
        MakeRef => make_ref(place: u64) -> u64 = tier::make_ref;
        CallNative => call_native(call: u32) -> Zval = tier::call_native;
        PopCall => pop_call() = tier::pop_call;
        Interrupt => interrupt() = tier::interrupted;
    }
}

/// The `step` kernel (`jit-design.md` §5.2): the opline at `at` of the op
/// array the engine numbers `func`, which the trace recorded and its epoch
/// holds, served whole over that op array ([`tier::step_held`]), and
/// where the interpreter stands after it: nowhere ([`Stood::NOWHERE`])
/// where the opline left the serving's entry frame and the serving is
/// over, as php's `zend_jit_leave_func_helper` and a `RETURN`'s handler
/// answer the halt for a `ZEND_CALL_TOP` frame, which compiled code
/// past an opline that leaves its frame reads (`Emit::halts`). No opline
/// that ends a serving leaves a throwable pending as it does, its value
/// handed over with no destructor called; one that did would be answered
/// as a throw ([`answered`]) and unwound from a frame already returned,
/// so it is refused as the internal error it is.
fn step(req: &mut Request, on: On<'_, '_>, func: u32) -> Result<Stood, Fatal> {
    let frame = standing(req, on.room, on.at)?;
    let (held, epoch) = held(on, func)?;
    match tier::step_held(req, frame, held, epoch)? {
        Stepped::Done if req.unwinding() => Err(internal(
            "a step that ended the serving a trace runs in left it unwinding",
        )),
        Stepped::Done => Ok(Stood::NOWHERE),
        stepped => stood(req, stepped, (frame, func), on.holding),
    }
}

/// The `handle` kernel: the opline at `at` of the op array the engine
/// numbers `func`, one whose handler goes on in its frame or throws,
/// served by its handler alone ([`tier::dispatch_held`]), as php's code
/// calls such an opline's handler directly; where the interpreter
/// stands after it is left to compiled code, which knows it, and a throw
/// is the request's pending throwable, which the wrapper answers as one.
fn handle(req: &mut Request, on: On<'_, '_>, func: u32) -> Result<(), Fatal> {
    let frame = standing(req, on.room, on.at)?;
    let (held, _) = held(on, func)?;
    tier::dispatch_held(req, frame, held)
}

/// The `frameless` kernel: the native at `at` of the engine's
/// frameless table, the `FRAMELESS_ICALL_*` at `on.at` of the frame at
/// `place`, served over the three operand zvals compiled code read, each
/// of the kind `kinds` names for it ([`OperandKinds`]), with no
/// opline read ([`tier::frameless_call`]), as php's code calls the
/// native's frameless handler; where the interpreter stands after it is
/// compiled code's, and a throw is the request's pending throwable.
#[allow(
    clippy::too_many_arguments,
    reason = "php's frameless handler takes three operand zvals, each two words across the crossing"
)]
fn frameless(
    req: &mut Request,
    on: On<'_, '_>,
    place: u32,
    at: FramelessAt,
    kinds: OperandKinds,
    a: u64,
    a_info: u32,
    b: u64,
    b_info: u32,
    c: u64,
    c_info: u32,
) -> Result<Zval, Fatal> {
    let frame = at_place(req, on.room, place);
    let [a_kind, b_kind, c_kind] = operand_kinds(kinds)?;
    let operands = [
        (zval(a, a_info), a_kind),
        (zval(b, b_info), b_kind),
        (zval(c, c_info), c_kind),
    ];
    tier::frameless_call(req, frame, at, operands)
}

/// The `call_native` kernel: the native the call at `call` on the stack
/// is to, the call on top and made the one being served by compiled code,
/// served with no opline read ([`tier::call_native`]), as php's code calls
/// the native's handler with the call; where the interpreter stands
/// after it is compiled code's, and a throw is the request's pending
/// throwable.
fn call_native(req: &mut Request, _: On<'_, '_>, call: u32) -> Result<Zval, Fatal> {
    tier::call_native(req, FrameRef(call))
}

/// The `pop_call` kernel: the call on top popped and its arguments given
/// up ([`tier::pop_call`]), a destructor's throw left pending.
#[allow(
    clippy::unnecessary_wraps,
    reason = "a kernel answers as `kernels!` calls it, and this one never fails"
)]
fn pop_call(req: &mut Request, _: On<'_, '_>) -> Result<(), Fatal> {
    tier::pop_call(req);
    Ok(())
}

/// The `interrupt` kernel: the interrupt check after a native returned,
/// php's `zend_fcall_interrupt` ([`tier::interrupted`]); a hook's throw is
/// the request's pending throwable.
fn interrupt(req: &mut Request, _: On<'_, '_>) -> Result<(), Fatal> {
    tier::interrupted(req).map(drop)
}

/// The kinds `kinds` carries across, in operand order.
fn operand_kinds(kinds: OperandKinds) -> Result<[OperandKind; 3], Fatal> {
    kinds
        .kinds()
        .ok_or_else(|| internal("an operand of no operand kind"))
}

/// The `leave` kernel: the value the `RETURN` at `at`, which a trace ends
/// at, hands over, in a frame served from outside the loop, set as the
/// serving's return value where the frame is the serving's entry
/// ([`tier::return_entry`]), as the handler's `RETURN` of such a frame sets
/// it and ends the serving, answered one; zero, with nothing done, for any
/// other frame, which the trace's end exit leaves to the interpreter. php's
/// `zend_jit_leave_func` returns `ZEND_VM_RETURN` to `execute_ex` for such
/// a frame, `ZEND_CALL_TOP`.
fn leave(req: &mut Request, on: On<'_, '_>, word: u64, type_info: u32) -> Result<u64, Fatal> {
    let frame = standing(req, on.room, on.at)?;
    Ok(u64::from(tier::return_entry(
        req,
        frame,
        zval(word, type_info),
    )))
}

/// The `push_call` kernel (`jit-design.md` §4.3): the `INIT_*` at `at`
/// served as its handler serves it where its cache slot holds the op
/// array the engine numbers `callee`, which its inline path pushes with
/// no lookup, as php's trace guards `CACHED_PTR` against the function it
/// recorded; where it holds another, or none yet, nothing is pushed and
/// the call stood is null, so compiled code leaves for the interpreter to
/// serve the opline whole.
fn push_call(req: &mut Request, on: On<'_, '_>, func: u32, callee: u32) -> Result<Stood, Fatal> {
    let frame = standing(req, on.room, on.at)?;
    let (held, epoch) = held(on, func)?;
    if tier::primed(req, frame, held, on.at).and_then(engine_offset) != Some(callee) {
        let refused = where_stood(req, frame, Some(func), on.holding)?;
        return Ok(Stood {
            call: ptr::null_mut(),
            ..refused
        });
    }
    let stepped = tier::step_held(req, frame, held, epoch)?;
    stood(req, stepped, (frame, func), on.holding)
}

/// The `call` kernel (`jit-design.md` §5.2): the `DO_*CALL` at `at` of
/// the op array the engine numbers `func`, a call to the op array it
/// numbers `callee`, which the trace recorded and its `INIT_*` pushed,
/// made as the handler's inline path makes it and the callee activated,
/// with no dispatch ([`tier::call_held`]); where the interpreter stands
/// after it, in the callee, named by the op array it runs, which a
/// `DO_FCALL` of a callee another call varies holds to the one recorded.
fn call(req: &mut Request, on: On<'_, '_>, func: u32, callee: u32) -> Result<Stood, Fatal> {
    let frame = standing(req, on.room, on.at)?;
    let (entered, _) = held(on, callee)?;
    let (held, epoch) = held(on, func)?;
    let stepped = tier::call_held(req, frame, (held, entered), epoch)?;
    stood(req, stepped, (frame, func), on.holding)
}

/// The op array the engine numbers `func`, as the epoch of the trace
/// that recorded it holds it, and that epoch.
fn held<'a>(on: On<'_, 'a>, func: u32) -> Result<(Held<'a>, &'a Epoch), Fatal> {
    let holding = on
        .holding
        .ok_or_else(|| internal("a kernel was called with no epoch to serve from"))?;
    let held = holding
        .held(func)
        .ok_or_else(|| internal("a trace stepped an op array its epoch does not hold"))?;
    Ok((held, holding.epoch))
}

/// The `release` kernel: php's `zval_ptr_dtor` of the counted value
/// compiled code gives up, a destructor's throw left pending.
fn release(req: &mut Request, on: On<'_, '_>, word: u64, type_info: u32) -> Result<(), Fatal> {
    standing(req, on.room, on.at)?;
    tier::release(req, zval(word, type_info));
    Ok(())
}

/// The `free_cv` kernel: php's `zend_jit_free_cv` of a variable of the
/// frame a `RETURN` leaves in place, that frame off the chain first
/// ([`tier::free_cv`]), a destructor's throw left pending.
fn free_cv(req: &mut Request, on: On<'_, '_>, word: u64, type_info: u32) -> Result<(), Fatal> {
    let frame = standing(req, on.room, on.at)?;
    tier::free_cv(req, frame, zval(word, type_info));
    Ok(())
}

/// The `free_this` kernel: the receiver of the method's frame a
/// `RETURN` leaves in place, after its variables, where its owner or its
/// pin is the last, that frame off the chain first
/// ([`tier::free_this`]), a destructor's throw left pending.
fn free_this(req: &mut Request, on: On<'_, '_>) -> Result<(), Fatal> {
    let frame = standing(req, on.room, on.at)?;
    tier::free_this(req, frame);
    Ok(())
}

/// The `identical` kernel: php's `zend_is_identical` of two zvals of a
/// scalar kind or a string, one or zero, as compiled code compares two
/// strings' bytes; it reads nothing of the request and stands no frame,
/// as php's call of it saves no opline.
#[allow(
    clippy::unnecessary_wraps,
    reason = "a kernel answers as `kernels!` calls it, and this one never fails"
)]
fn identical(
    _: &mut Request,
    _: On<'_, '_>,
    a: u64,
    a_info: u32,
    b: u64,
    b_info: u32,
) -> Result<u64, Fatal> {
    let (a, b) = (zval(a, a_info), zval(b, b_info));
    let same = Scalar::of(&a)
        .zip(Scalar::of(&b))
        .is_some_and(|(a, b)| php_heap::identical(a, b));
    Ok(u64::from(same))
}

/// The `rope_init` kernel: `ROPE_INIT` of a string compiled code hands
/// with its owner ([`tier::rope_open`]), the rope's position, which its
/// temporary holds.
fn rope_init(req: &mut Request, _: On<'_, '_>, word: u64, type_info: u32) -> Result<u64, Fatal> {
    let rope = tier::rope_open(req, zval(word, type_info))?;
    u64::try_from(rope).map_err(|_| internal("a rope at a negative position"))
}

/// The `rope_add` kernel: `ROPE_ADD` of a string compiled code hands
/// with its owner onto the rope at `rope` ([`tier::rope_push`]).
fn rope_add(
    req: &mut Request,
    _: On<'_, '_>,
    rope: u64,
    word: u64,
    type_info: u32,
) -> Result<(), Fatal> {
    let rope = i64::try_from(rope).map_err(|_| internal("a rope past its positions"))?;
    tier::rope_push(req, rope, zval(word, type_info))
}

/// The `rope_end` kernel: `ROPE_END` of a string compiled code hands with
/// its owner, the rope at `rope` joined ([`tier::rope_end`]), the joined
/// string's zval, or `UNDEF` where the join refused and left its
/// throwable pending, which the wrapper answers as one. Compiled code
/// stood the frame at the opline first, php's `SAVE_OPLINE()`.
fn rope_end(
    req: &mut Request,
    _: On<'_, '_>,
    rope: u64,
    word: u64,
    type_info: u32,
) -> Result<Zval, Fatal> {
    let rope = i64::try_from(rope).map_err(|_| internal("a rope past its positions"))?;
    Ok(tier::rope_end(req, rope, zval(word, type_info))?.unwrap_or(Zval::UNDEF))
}

/// The `assign_concat` kernel: `.=` of a string compiled code hands with
/// no owner onto the zval at `place`, a variable of the frame or an
/// array's element, which holds a string ([`tier::assign_concat`]), the
/// frame stood at the opline, php's `SAVE_OPLINE()`, where the join's
/// refusal is raised.
fn assign_concat(
    req: &mut Request,
    on: On<'_, '_>,
    place: u64,
    word: u64,
    type_info: u32,
) -> Result<(), Fatal> {
    standing(req, on.room, on.at)?;
    let place = zval(place, u32::from(ZvalType::Indirect as u8))
        .as_indirect()
        .ok_or_else(|| internal("compiled code's .= onto no place"))?;
    tier::assign_concat(req, place, zval(word, type_info));
    Ok(())
}

/// The `concat` kernel: `CONCAT` or `FAST_CONCAT` of two strings
/// ([`tier::concat`]), `kinds` the two sides' `IS_*` ([`OperandKinds`]),
/// the left one handed with its owner where it
/// is a temporary; the joined string's zval, or `UNDEF` where the join
/// refused and left its throwable pending, which the wrapper answers as
/// one. Compiled code stood the frame at the opline first.
fn concat(
    req: &mut Request,
    _: On<'_, '_>,
    a: u64,
    a_info: u32,
    b: u64,
    b_info: u32,
    kinds: OperandKinds,
) -> Result<Zval, Fatal> {
    let [a_kind, b_kind, _] = operand_kinds(kinds)?;
    let left = (zval(a, a_info), a_kind);
    let right = (zval(b, b_info), b_kind);
    Ok(tier::concat(req, left, right)?.unwrap_or(Zval::UNDEF))
}

/// The array at `array` and the key the zval `key`, `key_info` names in
/// it, read as the handler's inline path reads a dimension
/// ([`tier::dim_key`]).
fn keyed_element(array: u64, (key, key_info): (u64, u32), constant: u32) -> Option<(Arr, Key)> {
    let array = zval(array, u32::from(ZvalType::Array as u8)).as_arr()?;
    Some((array, tier::dim_key(zval(key, key_info), constant != 0)?))
}

/// What a lookup kernel answers for the element at `entry`: its place,
/// an `IS_INDIRECT`, where it holds a value or a reference; `UNDEF` where
/// it is not there, is unset or is a symbol table's `IS_INDIRECT`, which
/// compiled code reads as a key the array has not got: a read leaves for
/// the interpreter, a write is its handler's, and a test answers no.
fn placed(entry: Option<ZvalRef>) -> Zval {
    match entry {
        Some(entry) if !matches!(entry.read().kind(), ZvalType::Undef | ZvalType::Indirect) => {
            Zval::indirect(entry)
        }
        _ => Zval::UNDEF,
    }
}

/// The `find` kernel: the element the key names in the array at `array`,
/// for a read, `isset()`, a compound write or a fetch for `_RW`, `zend_hash_find` over a
/// hash or a packed table, as php's code calls `zend_hash_find_known_hash`,
/// `zend_jit_symtable_find` and `_zend_hash_index_find`
/// (`Z/ext/opcache/jit/zend_jit_ir.c:11917-12511`); it reads nothing of
/// the request and stands no frame, as php's call of it saves no opline.
#[allow(
    clippy::unnecessary_wraps,
    reason = "a kernel answers as `kernels!` calls it, and this one never fails"
)]
fn find(
    _: &mut Request,
    _: On<'_, '_>,
    array: u64,
    key: u64,
    key_info: u32,
    constant: u32,
) -> Result<Zval, Fatal> {
    let found = keyed_element(array, (key, key_info), constant)
        .and_then(|(array, key)| array.value_ref(key));
    Ok(placed(found))
}

/// The `lookup` kernel: the element the key names in the array at
/// `array`, which compiled code holds the one owner of, found or laid
/// down holding `null` ([`tier::lookup_element`]), as php's code calls
/// `zend_hash_lookup` and `zend_hash_index_lookup`; `UNDEF` where nothing
/// was laid down, the memory limit's fatal, which the request ends in,
/// answered as its unwinding.
#[allow(
    clippy::unnecessary_wraps,
    reason = "a kernel answers as `kernels!` calls it, and this one never fails"
)]
fn lookup(
    req: &mut Request,
    _: On<'_, '_>,
    array: u64,
    key: u64,
    key_info: u32,
    constant: u32,
) -> Result<Zval, Fatal> {
    let laid = keyed_element(array, (key, key_info), constant)
        .and_then(|(array, key)| tier::lookup_element(req, array, key));
    Ok(placed(laid))
}

/// The `make_ref` kernel: the reference a `SEND_REF` makes of the
/// variable at `place` ([`tier::make_ref`]), as php's `zend_jit_send_ref`
/// makes it, written into the place; one where it made it, zero where
/// the memory limit refused it, the request ended in its fatal, which
/// the wrapper answers as the request's unwinding, or a fallible scope's
/// refusal, whose send the handler then serves.
fn make_ref(req: &mut Request, on: On<'_, '_>, place: u64) -> Result<u64, Fatal> {
    standing(req, on.room, on.at)?;
    let place = zval(place, u32::from(ZvalType::Indirect as u8))
        .as_indirect()
        .ok_or_else(|| internal("compiled code's SEND_REF of no place"))?;
    Ok(u64::from(tier::make_ref(req, place)))
}

/// The frame at `place` on the stack, which compiled code holds for the
/// frame whose room it handed a kernel, so no frame is looked for by its
/// room ([`standing`]); a debug build holds the two to one frame.
fn at_place(req: &Request, room: Room, place: u32) -> FrameRef {
    let frame = FrameRef(place);
    debug_assert_eq!(
        NonNull::new(room.0).and_then(|room| tier::at_room(req, room)),
        Some(frame),
        "compiled code's place of a frame is the place of the frame at its room"
    );
    frame
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

/// Where the interpreter stands after a step of `stepped`'s opline in
/// `frame`, which serves the op array the engine numbers `func`: the
/// frame it goes on in, whatever the step pushed or popped. A step that
/// ended the serving is the `step` kernel's to answer ([`step`]); the
/// `push_call` and `call` kernels serve no opline that leaves its frame,
/// so for them it is an internal error.
fn stood(
    req: &mut Request,
    stepped: Stepped,
    (frame, func): (FrameRef, u32),
    holding: Option<&Holding<'_>>,
) -> Result<Stood, Fatal> {
    match stepped {
        Stepped::Same(at) => where_stood(req, at, (at == frame).then_some(func), holding),
        Stepped::Pushed(at) | Stepped::Popped(at) | Stepped::Moved(at) | Stepped::Threw(at) => {
            where_stood(req, at, None, holding)
        }
        Stepped::Done => Err(internal(
            "a kernel's step ended the serving a trace runs in",
        )),
    }
}

/// The [`Stood`] of `frame`: its room, the room of the call under way in
/// it, `EX(call)`, the engine offset of its op array, `func` where the
/// caller knows it and read off the frame where not
/// ([`tier::running_func`]), none where the request alone holds it, and
/// its opline. A trait's method copied into its class reads a cache of
/// its own, which is no op array's, so its frame is named by the op array
/// it runs, the one its trace recorded, and its recursive return roots
/// and escapes as php's does (the exit numbers are #1220's):
///
/// ```php
/// trait T { function fib($n) { if ($n < 2) { return $n; } return $this->fib($n - 1) + $this->fib($n - 2); } }
/// class C { use T; }
/// echo (new C)->fib(12), "\n";
/// ```
/// ```expect regime=jit-inline process
/// 144
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %s:3
/// ---- TRACE 1 abort (deep recursion)
///
/// ---- TRACE 1 start (enter) C::fib() %s:2
/// ---- TRACE 1 stop (recursive call)
/// ---- TRACE 1 compiled
///
///      TRACE 1 exit %d C::fib() %s:2
/// ---- TRACE 2 start (side trace 1/%d) C::fib() %s:2
/// ---- TRACE 2 stop (return)
/// ---- TRACE 2 compiled
///
/// ---- TRACE 3 start (return) C::fib() %s:2
/// ---- TRACE 3 stop (recursive return)
/// ---- TRACE 3 compiled
///
///      TRACE 3 exit %d C::fib() %s:2
/// ---- TRACE 4 start (side trace 3/%d) C::fib() %s:2
/// ---- TRACE 4 stop (link to 1)
/// ---- TRACE 4 compiled
///
///      TRACE 1 exit %d C::fib() %s:2
/// ---- TRACE 5 start (side trace 1/%d) C::fib() %s:2
/// ---- TRACE 5 stop (return)
/// ---- TRACE 5 compiled
///
/// ```
fn where_stood(
    req: &mut Request,
    frame: FrameRef,
    func: Option<u32>,
    holding: Option<&Holding<'_>>,
) -> Result<Stood, Fatal> {
    let call = match tier::building_call(req).filter(|call| *call != frame) {
        Some(call) => tier::room(req, call).map_or(ptr::null_mut(), NonNull::as_ptr),
        None => ptr::null_mut(),
    };
    let room =
        tier::room(req, frame).ok_or_else(|| internal("a step went on in a frame with no room"))?;
    // A cache the hold served the op array of last is that op array's own.
    let func = match (func, tier::frame_cache(req, frame).and_then(engine_offset)) {
        (Some(func), _) => func,
        (None, Some(cache)) if holding.is_some_and(|holding| holding.served(cache)) => cache,
        (None, _) => tier::running_func(req, frame)
            .and_then(engine_offset)
            .unwrap_or(u32::MAX),
    };
    Ok(Stood {
        frame: room.as_ptr(),
        call,
        func,
        opline: tier::frame(req, frame).map_or(0, |view| view.opline().0),
        room: tier::stack_room(req),
    })
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

/// How a trace gave the interpreter back its request (`jit-design.md`
/// §4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Left {
    /// Through an exit, landed.
    Exit(Exited),
    /// Through an unwind, which takes no exit: a throwable a kernel left
    /// pending, which the loop unwinds from where it was raised, in the
    /// frame that stands.
    Unwound(FrameRef),
    /// Through php's interrupt stub, which takes no exit: a loop's
    /// interrupt test found the interrupt raised, and the frame that
    /// stands is stood at the loop's header, where the stub goes on once
    /// it handled the interrupt.
    Interrupted(FrameRef),
    /// Through php's escape stub, which takes no exit: a return past the
    /// trace's lowest frame left the frame that stands at an opline the
    /// trace does not go on at, whose handler the interpreter runs.
    Escaped(FrameRef),
    /// Through the `RETURN` of the serving's entry frame, which takes no
    /// exit: the serving is over and its return value set
    /// ([`lower::RETURNED`]), as php's `ZEND_VM_RETURN` ends `execute_ex`.
    Returned,
    /// Entered at its loop's body, the trace found the frame holding
    /// other than it knows there: nothing ran and the frame stands where
    /// it stood, so `dispatch` goes on to the loop's header and enters the
    /// trace at its first op there, whose guards take php's exits.
    Declined,
}

/// Where a trace is entered: at its first op, or at its loop's body
/// (`types::BodyEntry`), as compiled code reads it in the `ExitCtx`, a
/// `u32` that is not zero at the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum At {
    First = 0,
    Body = 1,
}

/// Where a trace left the interpreter through an exit: the exit, and the
/// frame the loop goes on in, landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Exited {
    pub(crate) exit: ExitId,
    pub(crate) frame: FrameRef,
}

/// What compiled code is entered as (`jit-design.md` §4.3): the trace's
/// prologue, called with the `ExitCtx`, the room of the frame it enters
/// on and the address of its body, which it calls on the tail convention,
/// answering the exit it left through, or another of `lower`'s answers.
type Entry = unsafe extern "C" fn(*mut ExitCtx<'_>, *mut u8, *const u8) -> u32;

/// What a call of [`enter`] answered: how the request was given back, and
/// the trace that gave it back, the one entered or one it jumped to,
/// whose exit it is.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Answered<'t> {
    pub(crate) left: Left,
    pub(crate) by: &'t Trace,
}

/// `trace` called on `frame` (`jit-design.md` §4.6), out of `epoch`, the
/// epoch whose table holds it and whose rows hold every op array it
/// recorded, which its kernels serve from, with the request's
/// trace number, php's `EG(jit_trace_num)`, the trace's for the length of
/// the call and the one before it again after. The trace's row is laid
/// down in the request's rows ([`Rows`]) before it is called, so a jump
/// from another trace finds it ready, out of `state`, the tier's state
/// the request lends ([`lent`]), none for a stand-in's; an entry after
/// the row settled reads it alone ([`Rows::settled`]). A panic a wrapper caught
/// is resumed and a fatal returned, read before the exit; an unwind hands
/// back the frame that stands, and an exit is landed ([`land`]), of the
/// trace that answered, which is `trace` or one it jumped to, named in
/// the `ExitCtx`. Reads no word and follows no link: what happens next
/// is `dispatch`'s.
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
pub(crate) fn enter<'t>(
    req: &mut Request,
    state: Option<&crate::JitRequest>,
    frame: FrameRef,
    (epoch, trace): (&'t Epoch, &'t Trace),
    at: At,
) -> Result<Answered<'t>, Fatal> {
    let room = tier::room(req, frame)
        .ok_or_else(|| internal("a trace entered on a frame with no room"))?;
    // SAFETY: the table published bytes `lower` compiled for this ABI at
    // this address, final and executable for as long as the trace, which
    // the borrow of the epoch's table keeps.
    let entry = unsafe { std::mem::transmute::<*const u8, Entry>(trace.entry().as_ptr()) };
    // Where the frame stands as a kernel would report it, which the
    // trace's code reads as the last kernel's: its IP guard, and the call
    // its frame began before it.
    let stood = if trace.entry_stood {
        // A frame whose cache is the own cache of the op array the trace
        // opens in runs that op array, named with no row of the epoch read.
        let own = (trace.opens)
            .and_then(|(func, _)| engine_offset(func))
            .filter(|func| tier::frame_cache(req, frame).and_then(engine_offset) == Some(*func));
        Some(where_stood(req, frame, own, None)?)
    } else {
        None
    };
    // The stack the trace's calls in place are made on, their headers in
    // this request, and whether they are made in place, laid down in the
    // trace's row for the jumps to it.
    let stack = tier::stack(req).as_ptr();
    let (images, rows) = Images::of(req, state, (epoch, trace));
    let ready = images.ready();
    let before = tier::trace_num(req);
    tier::set_trace_num(req, TraceNum::from_raw(trace.id.get()));
    let mut cx = ExitCtx::new(&mut *req);
    cx.at = at;
    (cx.stack, cx.images) = (stack, images.addresses());
    (cx.root, cx.ready) = (frame.0, u32::from(ready));
    (cx.rows, cx.trace) = (rows, trace.id.get());
    if let Some(stood) = stood {
        cx.stood = stood;
    }
    cx.holding = Some(Holding::of(epoch));
    // SAFETY: `cx` lives on this frame for the whole call and nothing but
    // the code and its wrappers touches the request meanwhile; `room` is
    // the room of a frame on the request's stack, and the body is the
    // trace's, as the prologue asks.
    let answer = unsafe { entry(&raw mut cx, room.as_ptr(), trace.body().as_ptr()) };
    let (pending, by, escaped) = (cx.take_pending(), cx.trace, cx.root);
    tier::set_trace_num(req, before);
    match pending {
        Some(Pending::Panic(payload)) => resume_unwind(payload),
        Some(Pending::Bailout(fatal)) => return Err(fatal),
        None => {}
    }
    let by = if by == trace.id.get() {
        trace
    } else {
        crate::table::TraceId::new(by)
            .and_then(|id| crate::table::of_epoch(epoch)?.get(id))
            .ok_or_else(|| internal("a trace answered as one its table does not hold"))?
    };
    let left = match Answer::of(answer, by.exits.len()) {
        Answer::Exit(exit) => {
            let frame = land(req, &by.exits[usize::from(exit.0)])?;
            Left::Exit(Exited { exit, frame })
        }
        Answer::Unwound if req.unwinding() => {
            let frame =
                tier::current(req).ok_or_else(|| internal("a trace left no frame standing"))?;
            Left::Unwound(frame)
        }
        Answer::Unwound => return Err(internal("a trace unwound with no throwable pending")),
        Answer::Declined if at == At::Body && ptr::eq(by, trace) => Left::Declined,
        Answer::Declined => {
            return Err(internal("a trace declined an entry at its first op"));
        }
        Answer::Interrupted => {
            let header = by
                .stub
                .ok_or_else(|| internal("a trace took an interrupt stub it does not have"))?;
            let frame =
                tier::current(req).ok_or_else(|| internal("a trace left no frame standing"))?;
            tier::stand(req, frame, header);
            Left::Interrupted(frame)
        }
        // The frame the escape returned into, which its code hands back.
        Answer::Escaped => {
            let frame = FrameRef(escaped);
            debug_assert_eq!(
                tier::current(req),
                Some(frame),
                "an escape hands back the innermost frame"
            );
            Left::Escaped(frame)
        }
        Answer::Returned => Left::Returned,
        Answer::Abandoned => {
            return Err(internal(
                "a trace left through no exit of its own and kept nothing",
            ));
        }
    };
    Ok(Answered { left, by })
}

/// What compiled code reads for a cache slot the touch found none for: a
/// tag that names no place, which the guard refuses before it reads past
/// it.
static NO_PLACE: u32 = tier::PLACE.unprimed();

/// The address compiled code reads for a slot the touch has not answered,
/// [`NO_PLACE`]'s.
fn no_place() -> usize {
    ptr::from_ref(&NO_PLACE).expose_provenance()
}

/// Whether slot `raw`, whose address the request keeps as `at`, is one a
/// later entry touches again: the touch has not answered it, and a call
/// can lay its cache, where a file's own code's or one past its cache
/// never is ([`tier::touchable`]).
fn untouched(epoch: &Epoch, raw: tier::MapPtrRaw, at: usize) -> bool {
    at == no_place() && tier::touchable(epoch, raw)
}

/// The touch (`jit-design.md` §4.6, #2957's note; [`tier::touch`]): the
/// address of each runtime-cache slot `trace`'s code reads in `req`, one
/// a slot of `Trace.map_ptrs`, where the request laid its cache down, and
/// [`NO_PLACE`]'s where it has not yet, since php lays a callee's cache at
/// its call's `INIT_*` and no sooner: the opline is its handler's until
/// then, and a later entry touches the slot again ([`Entered::untouched`]).
///
/// So a request that enters a trace an earlier request published, before
/// the trace's inlined method was called in it, finds that method's cache
/// not laid down, as php's does: a closure a reflector makes over the
/// method there takes a cache of its own (`zend_create_closure_ex`), where
/// one made once the call laid the cache shares it. Two requests of one
/// built-in server, each of the program the snippet writes:
///
/// ```php
/// $d = sys_get_temp_dir() . '/touch' . getmypid(); @mkdir($d);
/// file_put_contents("$d/r.php", '<?php eval(\'class Base { public $x = 1; }\'); class C extends Base { function m() { return $this->x; } }
/// function size($o) { $m = memory_get_usage(); $c = (new ReflectionMethod("C", "m"))->getClosure($o); $grew = memory_get_usage() - $m; unset($c); return $grew; }
/// function fcc($o) { $m = memory_get_usage(); $c = $o->m(...); $grew = memory_get_usage() - $m; unset($c); return $grew; }
/// function k($o, $n) { $t = 0; $w = $f = 0; for ($i = 0; $i < $n; $i++) { if ($i == 0) { $w = size($o); $f = fcc($o); } $t += $o->m(); } return $t . " " . ($w > size($o) ? "own" : "shared") . " " . ($f > fcc($o) ? "own" : "shared"); }
/// echo k(new C, 20), "\n";');
/// $s = stream_socket_server('tcp://127.0.0.1:0'); $port = parse_url(stream_socket_get_name($s, false), PHP_URL_PORT); fclose($s);
/// $ini = ['opcache.enable=1', 'opcache.enable_cli=1', 'opcache.jit=tracing', 'opcache.jit_buffer_size=64M', 'opcache.jit_hot_loop=1', 'opcache.jit_hot_func=1', 'opcache.jit_hot_side_exit=1', 'opcache.file_update_protection=0'];
/// if (($mode = ini_get('jit.mode')) !== false) { $ini[] = "jit.mode=$mode"; }
/// $cmd = [PHP_BINARY, '-n']; foreach ($ini as $i) { $cmd[] = '-d'; $cmd[] = $i; } array_push($cmd, '-S', "127.0.0.1:$port", '-t', $d);
/// $p = proc_open($cmd, [1 => ['file', '/dev/null', 'w'], 2 => ['file', '/dev/null', 'w']], $pipes);
/// for ($n = 0; $n < 250 && !@fsockopen('127.0.0.1', $port); $n++) { usleep(20000); }
/// for ($r = 0; $r < 2; $r++) { echo file_get_contents("http://127.0.0.1:$port/r.php"); }
/// proc_terminate($p); proc_close($p); unlink("$d/r.php"); rmdir($d);
/// ```
/// ```expect regime=jit-force process
/// 20 own shared
/// 20 own shared
/// ```
///
/// ```php
/// $d = sys_get_temp_dir() . '/touch' . getmypid(); @mkdir($d);
/// file_put_contents("$d/r.php", '<?php eval(\'class Base { public $x = 1; }\'); class C extends Base { function m() { return $this->x; } }
/// function size($o) { $m = memory_get_usage(); $c = (new ReflectionMethod("C", "m"))->getClosure($o); $grew = memory_get_usage() - $m; unset($c); return $grew; }
/// function fcc($o) { $m = memory_get_usage(); $c = $o->m(...); $grew = memory_get_usage() - $m; unset($c); return $grew; }
/// function k($o, $n) { $t = 0; $w = $f = 0; for ($i = 0; $i < $n; $i++) { if ($i == 0) { $w = size($o); $f = fcc($o); } $t += $o->m(); } return $t . " " . ($w > size($o) ? "own" : "shared") . " " . ($f > fcc($o) ? "own" : "shared"); }
/// echo k(new C, 20), "\n";');
/// $s = stream_socket_server('tcp://127.0.0.1:0'); $port = parse_url(stream_socket_get_name($s, false), PHP_URL_PORT); fclose($s);
/// $ini = ['opcache.enable=1', 'opcache.enable_cli=1', 'opcache.jit=tracing', 'opcache.jit_buffer_size=64M', 'opcache.jit_hot_loop=1', 'opcache.jit_hot_func=1', 'opcache.jit_hot_side_exit=1', 'opcache.file_update_protection=0'];
/// if (($mode = ini_get('jit.mode')) !== false) { $ini[] = "jit.mode=$mode"; }
/// $cmd = [PHP_BINARY, '-n']; foreach ($ini as $i) { $cmd[] = '-d'; $cmd[] = $i; } array_push($cmd, '-S', "127.0.0.1:$port", '-t', $d);
/// $p = proc_open($cmd, [1 => ['file', '/dev/null', 'w'], 2 => ['file', '/dev/null', 'w']], $pipes);
/// for ($n = 0; $n < 250 && !@fsockopen('127.0.0.1', $port); $n++) { usleep(20000); }
/// for ($r = 0; $r < 2; $r++) { echo file_get_contents("http://127.0.0.1:$port/r.php"); }
/// proc_terminate($p); proc_close($p); unlink("$d/r.php"); rmdir($d);
/// ```
/// ```expect regime=jit-exit-fuzz process
/// 20 own shared
/// 20 own shared
/// ```
fn touched(req: &Request, (epoch, trace): (&Epoch, &Trace)) -> impl Iterator<Item = usize> {
    (trace.map_ptrs.iter()).map(move |raw| touch_one(req, epoch, *raw))
}

/// [`touched`] of the one slot `raw`.
fn touch_one(req: &Request, epoch: &Epoch, raw: tier::MapPtrRaw) -> usize {
    tier::touch(req, epoch, raw).map_or_else(no_place, CachedSlot::address)
}

/// The request's addresses a trace's code reads: the headers of the calls
/// it pushes in place, one address a call in its order
/// (`lower::Compiled::calls`), each the header `push_call` lays down for
/// the call in this request ([`tier::call_image`]), since the function's
/// number and its record are the request's; then the runtime-cache slots it
/// reads, one a slot of `Trace.map_ptrs` ([`touched`]); and whether its
/// code makes its calls in place: every header laid out and the cache of
/// every callee it enters laid down ([`Trace::callee_caches`]). A header a
/// call's function is not the request's yet for reads zero, and the calls
/// are served through the kernels until it is. A request keeps what it
/// laid out for every trace it entered ([`Headers`]); the stand-in tiers
/// of the tests, which keep no state of the tier's, lay them out for the
/// one entry.
enum Images {
    /// None to read: the trace pushes nothing in place and reads no slot.
    None,
    /// The request's, kept in its state for as long as it runs.
    Kept { at: *const usize, laid: Readiness },
    /// Laid out for this entry alone: the headers, held while the code
    /// reads them, and the addresses.
    Own {
        #[expect(dead_code, reason = "read by the code alone")]
        images: Box<[HeaderImage]>,
        addresses: Box<[usize]>,
        laid: bool,
    },
}

impl Images {
    /// What `trace`'s code reads in `req`, the touched cache slots beside
    /// the headers ([`touched`]), and the request's
    /// rows with the trace's laid down as they say, which a jump to the
    /// trace reads ([`Rows`]); a request that keeps no state of the tier's
    /// keeps no rows, and its rows are [`NO_ROWS`].
    fn of(
        req: &Request,
        state: Option<&crate::JitRequest>,
        (epoch, trace): (&Epoch, &Trace),
    ) -> (Images, *const usize) {
        if let Some(state) = state {
            let rows = (state.headers).rows(|| {
                crate::table::of_epoch(epoch).map_or(0, crate::table::TraceTable::segments)
            });
            if let Some(at) = rows.settled(trace.id) {
                let laid = Readiness::Settled;
                return (Images::Kept { at, laid }, rows.base());
            }
            let images = if trace.calls.is_empty() && trace.map_ptrs.is_empty() {
                Images::None
            } else {
                (state.headers.entered(req, (epoch, trace))).map_or_else(
                    || Images::own(req, (epoch, trace)),
                    |(at, laid)| Images::Kept { at, laid },
                )
            };
            rows.lay(trace.id, images.addresses(), images.row());
            return (images, rows.base());
        }
        if trace.calls.is_empty() && trace.map_ptrs.is_empty() {
            return (Images::None, NO_ROWS.as_ptr());
        }
        (Images::own(req, (epoch, trace)), NO_ROWS.as_ptr())
    }

    /// What `trace`'s code reads laid out for one entry, by a request that
    /// keeps no state of the tier's.
    fn own(req: &Request, (epoch, trace): (&Epoch, &Trace)) -> Images {
        let images: Vec<Option<HeaderImage>> = (trace.calls.iter())
            .map(|call| tier::call_image(req, call))
            .collect();
        let missing = images.iter().any(Option::is_none);
        let addresses = (images.iter())
            .map(|image| image.as_ref().map_or(0, HeaderImage::address))
            .chain(touched(req, (epoch, trace)))
            .collect();
        Images::Own {
            images: images.into_iter().flatten().collect(),
            addresses,
            laid: !missing && callees_laid(req, trace),
        }
    }

    /// Whether the trace's code makes its calls in place.
    fn ready(&self) -> bool {
        match self {
            Images::None => true,
            Images::Kept { laid, .. } => laid.ready(),
            Images::Own { laid, .. } => *laid,
        }
    }

    /// What the trace's row in the request's [`Rows`] says of these: a jump
    /// enters a trace only through the request's own addresses for it,
    /// never those made for one entry alone, which it finds unready.
    fn row(&self) -> Readiness {
        match self {
            Images::None => Readiness::Settled,
            Images::Kept { laid, .. } => *laid,
            Images::Own { .. } => Readiness::Unready,
        }
    }

    /// Where compiled code reads the addresses, null where it reads none.
    fn addresses(&self) -> *const usize {
        match self {
            Images::Kept { at, .. } => *at,
            Images::Own { addresses, .. } => addresses.as_ptr(),
            Images::None => ptr::null(),
        }
    }
}

/// Whether the cache of every op array `trace`'s calls in place enter is
/// laid down in `req` ([`Trace::callee_caches`]).
fn callees_laid(req: &Request, trace: &Trace) -> bool {
    trace
        .callee_caches
        .iter()
        .all(|callee| tier::laid(req, *callee))
}

/// What a request keeps of the headers its traces push calls in place
/// with ([`Images`]): each call's header, by its callee and argument
/// count, laid out once, and per trace it entered, by the trace's number,
/// the addresses of its calls' headers and whether the caches of the
/// callees it enters were found laid down, which once laid stay laid for
/// the request. A header's bytes and an address list stand where they
/// were laid out for as long as the request runs, which is as long as the
/// code that reads them can. An entry reads its trace's row by its
/// number, with no hash taken; compiled code reads the [`Rows`].
#[derive(Debug, Default)]
pub(crate) struct Headers {
    kept: std::cell::RefCell<HeadersKept>,
    rows: std::cell::OnceCell<Rows>,
}

/// A call's header by the op array a published user function's call
/// serves, the name of the class a request-scoped class's own method's is
/// found under, the call's own name, the argument count, and a method's
/// called scope, which its header names: one image per call
/// [`tier::PlainCall`]'s equality tells apart.
type CallKey = (
    Option<FuncRef>,
    Box<[u8]>,
    Box<[u8]>,
    u32,
    Option<tier::ClassId>,
);

#[derive(Debug, Default)]
struct HeadersKept {
    calls: std::collections::HashMap<CallKey, HeaderImage>,
    traces: Vec<Option<Entered>>,
}

/// What a request keeps of one trace it entered ([`Headers`]): the
/// addresses its code reads, whether a call's header is missing yet,
/// whether its calls are made in place, and whether a cache slot it
/// reads was not laid down yet, which each entry after touches again.
#[derive(Debug)]
struct Entered {
    addresses: Box<[usize]>,
    missing: bool,
    laid: bool,
    untouched: bool,
}

impl Entered {
    /// Where its addresses are, and how far they are laid ([`Readiness`]).
    fn found(&self) -> (*const usize, Readiness) {
        let readiness = match (self.laid, self.untouched) {
            (false, _) => Readiness::Unready,
            (true, true) => Readiness::Ready,
            (true, false) => Readiness::Settled,
        };
        (self.addresses.as_ptr(), readiness)
    }
}

/// How far what a trace's code reads in a request is laid: its calls
/// not all made in place yet, which the kernels serve until they are;
/// made in place, a cache slot left that each entry after touches again;
/// or settled, every call made in place and every slot answered, so no
/// entry after lays out or touches anything again ([`Rows::settled`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Readiness {
    Unready,
    Ready,
    Settled,
}

impl Readiness {
    /// Whether the trace's code makes its calls in place.
    fn ready(self) -> bool {
        self != Readiness::Unready
    }
}

impl Headers {
    /// The addresses `trace`'s code reads in `req` ([`Images`]), laid out
    /// the first time the request enters it, the cache slots among them
    /// as the touch answers them ([`touched`]), and whether its calls are
    /// made in place; a header missing before is laid out once its
    /// function is the request's, and a slot whose cache was not laid
    /// down is touched again, in place; and how far that is laid
    /// ([`Readiness`]). None for a number no row holds.
    fn entered(
        &self,
        req: &Request,
        (epoch, trace): (&Epoch, &Trace),
    ) -> Option<(*const usize, Readiness)> {
        let mut kept = self.kept.borrow_mut();
        let row = usize::try_from(trace.id.get()).ok()?;
        let HeadersKept { calls, traces } = &mut *kept;
        if let Some(Some(entered)) = traces.get_mut(row) {
            if !entered.laid {
                if entered.missing {
                    entered.missing = false;
                    for (call, at) in trace.calls.iter().zip(entered.addresses.iter_mut()) {
                        if *at == 0 {
                            *at = header_of(calls, req, call).unwrap_or(0);
                            entered.missing |= *at == 0;
                        }
                    }
                }
                entered.laid = !entered.missing && callees_laid(req, trace);
            }
            if entered.untouched {
                let slots = entered
                    .addresses
                    .get_mut(trace.calls.len()..)
                    .unwrap_or_default();
                entered.untouched = false;
                for (raw, at) in trace.map_ptrs.iter().zip(slots) {
                    if *at == no_place() {
                        *at = touch_one(req, epoch, *raw);
                        entered.untouched |= untouched(epoch, *raw, *at);
                    }
                }
            }
            return Some(entered.found());
        }
        let mut addresses = Vec::with_capacity(trace.calls.len() + trace.map_ptrs.len());
        for call in &trace.calls {
            addresses.push(header_of(calls, req, call).unwrap_or(0));
        }
        let missing = addresses.contains(&0);
        let headers = addresses.len();
        addresses.extend(touched(req, (epoch, trace)));
        let retouch = (addresses.get(headers..).unwrap_or_default().iter())
            .zip(&trace.map_ptrs)
            .any(|(at, raw)| untouched(epoch, *raw, *at));
        let entered = Entered {
            addresses: addresses.into_boxed_slice(),
            missing,
            laid: !missing && callees_laid(req, trace),
            untouched: retouch,
        };
        let found = entered.found();
        if traces.len() <= row {
            traces.resize_with(row + 1, || None);
        }
        traces[row] = Some(entered);
        Some(found)
    }

    /// The request's rows, laid down the first time a trace is entered,
    /// as many segments as `segments` counts: as many as the trace table
    /// of the request's epoch has.
    fn rows(&self, segments: impl FnOnce() -> usize) -> &Rows {
        self.rows.get_or_init(|| Rows::new(segments()))
    }
}

/// The address of the request's header for `call`, laid out once per
/// callee and argument count; none while the call's function is not the
/// request's.
fn header_of(
    calls: &mut std::collections::HashMap<CallKey, HeaderImage>,
    req: &Request,
    call: &tier::PlainCall,
) -> Option<usize> {
    let key = (
        call.callee(),
        Box::<[u8]>::from(call.own_class().unwrap_or_default()),
        Box::<[u8]>::from(call.name()),
        call.count(),
        // The scope the image writes: a request-scoped class's is none.
        call.scope().filter(|class| tier::class_published(*class)),
    );
    if let Some(image) = calls.get(&key) {
        return Some(image.address());
    }
    let image = tier::call_image(req, call)?;
    let address = image.address();
    calls.insert(key, image);
    Some(address)
}

/// A request's rows (`jit-design.md` §4.6, `lower::ROWS_AT`): per trace
/// the crossing entered in the request, by the trace's number, where its
/// calls' headers are in the request and whether they are ready, which a
/// jump from another trace reads before it enters the trace and finds
/// unready for a trace the crossing has not entered. The first table
/// holds one address per segment of `lower::ROW_SEGMENT` rows, zero until
/// a trace of that segment is entered, and the table and every segment
/// stand where they were laid for as long as the request runs, written
/// only through cells, so the code that reads them reads them whole.
#[derive(Debug)]
struct Rows {
    segments: Box<[std::cell::OnceCell<Box<[Row]>>]>,
    firsts: Box<[std::cell::Cell<usize>]>,
}

/// One trace's row in a request's [`Rows`], as compiled code reads it,
/// its first two words; the third, whether the row is settled, is the
/// crossing's alone ([`Rows::settled`]).
#[repr(C)]
#[derive(Debug, Default)]
struct Row {
    images: std::cell::Cell<usize>,
    ready: std::cell::Cell<u32>,
    settled: std::cell::Cell<bool>,
}

const _: () = assert!(
    size_of::<Row>() == 16
        && lower::ROW_BYTES == 16
        && offset_of!(Row, images) == lower::ROW_IMAGES.unsigned_abs() as usize
        && offset_of!(Row, ready) == lower::ROW_READY.unsigned_abs() as usize
        && size_of::<std::cell::Cell<usize>>() == size_of::<usize>()
);

/// The rows of a request that keeps none: every segment absent, for any
/// trace a table numbers, so a jump finds no row ready.
static NO_ROWS: [usize; (crate::table::TraceId::LIMIT as usize).div_ceil(lower::ROW_SEGMENT)] =
    [0; (crate::table::TraceId::LIMIT as usize).div_ceil(lower::ROW_SEGMENT)];

impl Rows {
    /// `segments` segments, none laid down.
    fn new(segments: usize) -> Rows {
        Rows {
            segments: (0..segments).map(|_| std::cell::OnceCell::new()).collect(),
            firsts: (0..segments).map(|_| std::cell::Cell::new(0)).collect(),
        }
    }

    /// The row of the trace `id` names: where the addresses its code reads
    /// are, `images`, and how far they are laid, `readiness`, which says
    /// whether its calls are ready and whether no entry after lays the row
    /// again; nothing for a number past the segments.
    fn lay(&self, id: crate::table::TraceId, images: *const usize, readiness: Readiness) {
        let (segment, at) = id.slot();
        let (Some(cell), Some(first)) = (self.segments.get(segment), self.firsts.get(segment))
        else {
            return;
        };
        let rows = cell.get_or_init(|| (0..lower::ROW_SEGMENT).map(|_| Row::default()).collect());
        first.set(rows.as_ptr().expose_provenance());
        if let Some(row) = rows.get(at) {
            row.images.set(images.expose_provenance());
            row.ready.set(u32::from(readiness.ready()));
            row.settled.set(readiness == Readiness::Settled);
        }
    }

    /// Where the addresses the code of the trace `id` names reads are, in
    /// a row an entry laid settled ([`Rows::lay`]): what each entry after
    /// it reads and nothing else, as php's enters a trace through the
    /// handler its op array holds with nothing looked up. None for a row
    /// not laid or not settled yet.
    #[inline]
    fn settled(&self, id: crate::table::TraceId) -> Option<*const usize> {
        let (segment, at) = id.slot();
        let row = self.segments.get(segment)?.get()?.get(at)?;
        row.settled
            .get()
            .then(|| ptr::with_exposed_provenance(row.images.get()))
    }

    /// The first table's address, which compiled code reads past.
    fn base(&self) -> *const usize {
        self.firsts.as_ptr().cast()
    }
}

/// What compiled code answered `enter`, decoded once: an exit of the
/// trace's, [`lower::UNWOUND`], [`lower::INTERRUPTED`], [`lower::DECLINED`],
/// [`lower::ESCAPED`], [`lower::RETURNED`], or anything else, [`lower::ABANDONED`]
/// with nothing kept among it, which no trace answers but by a defect.
enum Answer {
    Exit(ExitId),
    Unwound,
    Interrupted,
    Declined,
    Escaped,
    Returned,
    Abandoned,
}

impl Answer {
    /// The word compiled code answered, for a trace of `exits` exits.
    fn of(word: u32, exits: usize) -> Answer {
        match u16::try_from(word) {
            Ok(id) if usize::from(id) < exits => Answer::Exit(ExitId(id)),
            _ if word == lower::UNWOUND => Answer::Unwound,
            _ if word == lower::INTERRUPTED => Answer::Interrupted,
            _ if word == lower::DECLINED => Answer::Declined,
            _ if word == lower::ESCAPED => Answer::Escaped,
            _ if word == lower::RETURNED => Answer::Returned,
            _ => Answer::Abandoned,
        }
    }
}

/// The landing (the glossary's Landing): the frame the loop goes on in,
/// read from the stack, stood at the exit's opline, and the temporaries
/// the opline before it read freed for `FreeTemps`. A throwable a kernel
/// left pending stands where it was raised, at the opline its frame was
/// stood at, which is where the loop unwinds from, so nothing is landed.
fn land(req: &mut Request, exit: &Exit) -> Result<FrameRef, Fatal> {
    let frame = tier::current(req).ok_or_else(|| internal("a trace left no frame standing"))?;
    if req.unwinding() {
        return Ok(frame);
    }
    if let Some(at) = exit.opline {
        tier::stand(req, frame, at);
    }
    let frees = exit.landing.frees;
    if frees.any() {
        let read = exit
            .opline
            .and_then(|at| at.0.checked_sub(1))
            .ok_or_else(|| internal("a landing frees what no opline before its own read"))?;
        tier::free_operands(req, frame, OplineIdx(read), frees.op1, frees.op2);
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

    fn unwinding(&self) -> bool {
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
fn tally(t: &mut Tally, _: On<'_, '_>, n: u64) -> Result<u64, Fatal> {
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

    use super::{ExitCtx, Fatal, On, Pending, Room, Status, Tally, answered, tallied};

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
        let on = On {
            room: Room(frame),
            at: OplineIdx(opline),
            holding: None,
        };
        let answer = super::tally(req, on, n);
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
                let kernel: fn(&mut Tally, On<'_, '_>, u64) -> Result<u64, Fatal> =
                    black_box(super::tally);
                let on = On {
                    room: Room(std::ptr::null_mut()),
                    at: OplineIdx(0),
                    holding: None,
                };
                for i in 0..n {
                    last = kernel(&mut tally, on, i)?;
                }
            }
        }
        Ok(last)
    }
}

#[cfg(test)]
pub(crate) mod tests;
