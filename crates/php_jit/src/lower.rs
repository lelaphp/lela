//! What becomes of a [`TraceIr`] (`jit-design.md` §4.3): the seam is the
//! value, and two modules read it. [`dump`] renders it as text, which
//! `types` is held to before code is made; [`Backend`] compiles it to
//! final bytes over Cranelift, a [`Compiled`], which `table` publishes.
//! Nothing but `lower` names a Cranelift type, so a Cranelift bump is a
//! change here alone.
//!
//! **What compiled code is.** A trace is one function, its body,
//! `fn(cx: *mut ExitCtx, frame: *mut u8) -> u32` on Cranelift's tail
//! convention, entered with the crossing's `ExitCtx` and the room of the
//! frame it runs in, php's `execute_data`, and answering the [`ExitId`]
//! it left through. Its bytes begin with the prologue,
//! `extern "C" fn(cx, frame, body) -> u32` on the host's convention,
//! which `enter` calls and which calls the body: php's trace prologue,
//! which a jump from another trace passes over
//! (`Z/ext/opcache/jit/zend_jit_ir.c:17418-17432`). A body hands on to
//! another trace by a tail call, which gives its own machine frame up
//! where php's jump keeps the one frame every trace shares: a link jumps
//! to the trace it links to, and an exit whose side trace is published
//! jumps to it through the exit's [entry slot](EntrySlots), each laying
//! down the entry the trace it enters reads.
//!
//! **The entry** (`jit-design.md` §4.6). What a body reads as it enters
//! is the room it was handed and the `ExitCtx`'s entry fields: where it
//! is entered ([`ENTRY_AT`]), where the frame stands as a kernel would
//! say ([`STOOD_AT`]), the frame's place on the stack
//! ([`ROOT_AT`]), its calls' headers in this request and whether they
//! are ready ([`IMAGES_AT`], [`READY_AT`]), and its own number
//! ([`TRACE_AT`]), by which `enter` reads the exit it answers. `enter`
//! lays them down as it calls the prologue, and a jump lays them down as
//! the exit it leaves through would have left the frame, so a trace is
//! entered alike whichever way it is reached.
//! It runs on the interpreter's frames: a slot is a load and a store at
//! `room + ROOM_BYTES + 16 * var` with the zval layout `php_heap` defines,
//! so an exit leaves every frame as the interpreter reads it once it has
//! stored the scalars it kept in registers with their type words. What
//! compiled code does to the frame chain, and to anything the engine
//! keeps behind its frames, is a call of a kernel through the one
//! emitter; [`Kernel`] lists them and [`Symbols`] holds their addresses.
//! Every kernel answers the crossing's status word, zero to go on, and on
//! anything else compiled code leaves through the opline's exit where an
//! inline form has one, or through its unwind, which stores the
//! registers and answers [`UNWOUND`], taking no exit, or, where the
//! opline names neither, answers [`ABANDONED`], for `enter` to read what
//! the kernel kept.
//!
//! **The interrupt test** (`jit-design.md` §4.3). A trace that loops loads
//! the interrupt pointer at offset [`INTERRUPT_AT`] of its `ExitCtx` once
//! as it enters, and at its header loads the byte it points to and leaves
//! through the header's interrupt exit when it is not zero: one load and
//! one branch per iteration, php's cost (`rfc-jit.md:118`).
//!
//! [`ExitId`]: crate::types::ExitId

mod clif;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the goldens' text; no host prints a trace's IR yet"
    )
)]
pub(crate) mod dump;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, AtomicU8, AtomicU64, Ordering};

use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::SourceLoc;
use cranelift_codegen::isa::{self, OwnedTargetIsa};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::{Context, Final, MachSrcLoc};
use php_engine::tier::{FuncRef, MapPtrRaw, PlainCall};
use php_unit::OplineIdx;

use crate::policy::{Mode, Policy};
use crate::record::{Began, Stop};
use crate::table::TraceName;
use crate::types::{Abort, Check, End, ExitId, ExitSpec, GuardAt, Interrupt, Op, TraceIr};

/// A trace in final bytes (`jit-design.md` §4.3), what the `Backend` makes
/// and `table::TraceTable::publish` copies into the code arena once:
/// the bytes, where they are entered, the alignment their first byte
/// needs, one [`ExitSpec`] per exit in [`ExitId`](crate::types::ExitId)
/// order, the runtime-cache slots the code reads, where each recorded
/// opline's code begins, and what a profiler names the trace by.
#[derive(Debug, Clone)]
pub(crate) struct Compiled {
    pub(crate) bytes: Vec<u8>,
    /// The offset in `bytes` the trace is entered at: its prologue,
    /// which `enter` calls on the host's convention.
    pub(crate) entry_offset: usize,
    /// The offset in `bytes` of its body, on the tail convention, which
    /// the prologue calls and a jump from another trace enters.
    pub(crate) body_offset: usize,
    /// A power of two.
    pub(crate) align: usize,
    pub(crate) exits: Box<[ExitSpec]>,
    /// The exit a trace that does not loop ends through, none for a loop:
    /// php's return to the interpreter at a trace's end
    /// (`zend_jit_trace_return`), which no side trace grows from.
    pub(crate) leaves: Option<ExitId>,
    /// Where a loop whose interrupt test takes php's stub stands its frame
    /// once it answers [`INTERRUPTED`]: its header's opline. None for a
    /// loop whose test takes an exit and for a trace that does not loop.
    pub(crate) stub: Option<OplineIdx>,
    /// The op array and opline the trace's first recorded opline stands
    /// at, where a frame entering it stands.
    pub(crate) opens: Option<(FuncRef, OplineIdx)>,
    /// Whether its code reads where the frame stands as it enters, which
    /// `enter` hands it at [`STOOD_AT`] as a kernel reports it: for php's
    /// IP guard of a side trace whose parent's exit named no opline
    /// (`record::Landed::Stood`), and for a call its frame began before
    /// the trace, php's fake init, whose room it reads there
    /// ([`stood_at_entry`]).
    pub(crate) entry_stood: bool,
    /// Where a loop's trace is entered at its body too: the opline its
    /// back edge lands on (`types::BodyEntry`).
    pub(crate) body: Option<OplineIdx>,
    /// The slots of the requests' runtime caches the code reads, which
    /// `enter` lays down on a request's first entry.
    pub(crate) map_ptrs: Box<[MapPtrRaw]>,
    pub(crate) marks: Box<[Mark]>,
    /// php's `zend_jit_trace_name` but the number, which the table
    /// assigns: the job's, named where the trace's `TRACE start` line is.
    pub(crate) name: TraceName,
    /// Under `jit.mode=exit-fuzz`, the byte per guard the code marks as
    /// the guard fails once, once per trace and so per process; the code
    /// holds their address, so they live as long as it. Empty otherwise.
    pub(crate) fuzzed: Fuzzed,
    /// One entry slot per exit, which the code reads where the exit may
    /// jump to its side trace; the code holds their address, so they live
    /// as long as it.
    pub(crate) slots: EntrySlots,
    /// Its landing slots, which the code reads where it escapes.
    pub(crate) landings: LandingSlots,
    /// The plain calls the code pushes in place, in the order it reads
    /// their headers in, the request's, at [`IMAGES_AT`].
    pub(crate) calls: Box<[PlainCall]>,
    /// The op arrays a call the code makes in place enters whose caches
    /// `enter` asks are laid down in the request, as php's
    /// `zend_jit_init_func_run_time_cache_helper` lays one down at the
    /// push (`Z/ext/opcache/jit/zend_jit_ir.c:8840-8850`): every callee
    /// but one of a recursive call, which runs in a frame of its own op
    /// array and so finds its cache laid.
    pub(crate) callee_caches: Box<[FuncRef]>,
}

/// The bytes compiled code reads and writes under `jit.mode=exit-fuzz`
/// (`jit-design.md` §4.6, §7): one per guard, zero until the guard has
/// failed once, at an address the code holds as a constant, so a clone
/// shares them as it shares the bytes that name them.
///
/// A loop whose guards each fail once, the interrupt test's among them
/// and its jump's direction not (#3055's ruling), lands in the
/// interpreter at each and prints what php prints:
///
/// ```php
/// $a = [3, 1, 4, 1, 5]; $s = 0;
/// for ($i = 0; $i < 100; $i++) { $s += $a[$i % 5] * $i; }
/// echo $s, "\n";
/// ```
/// ```expect exit=0 regime=jit-exit-fuzz process
/// 13940
/// ```
#[derive(Debug, Clone, Default)]
pub(crate) struct Fuzzed(Arc<[AtomicU8]>);

impl Fuzzed {
    /// `n` bytes, none failed yet.
    fn new(n: usize) -> Fuzzed {
        Fuzzed((0..n).map(|_| AtomicU8::new(0)).collect())
    }

    /// The bytes as the lowering hands them out.
    fn fuzz(&self) -> clif::Fuzz {
        clif::Fuzz {
            base: self.0.as_ptr().addr(),
            len: self.0.len(),
        }
    }
}

/// How a trace is entered by a jump from another (`jit-design.md` §4.6):
/// the address of its body and its number, which the jump writes at
/// [`TRACE_AT`] and finds the request's row of the trace by. The table
/// lays one down in each trace it publishes, which never moves while the
/// table stands, and an exit's [entry slot](EntrySlots) names it.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct Entrance {
    pub(crate) body: usize,
    pub(crate) trace: u32,
}

/// Where compiled code reads an [`Entrance`]'s body and number.
pub(crate) const ENTRANCE_BODY: i32 = 0;
pub(crate) const ENTRANCE_TRACE: i32 = 8;
const _: () = assert!(
    std::mem::offset_of!(Entrance, body) == ENTRANCE_BODY.unsigned_abs() as usize
        && std::mem::offset_of!(Entrance, trace) == ENTRANCE_TRACE.unsigned_abs() as usize
);

/// One entry slot per exit of a trace (#3229's ruling, `jit-design.md`
/// §4.4, §4.6): null until the side trace grown from the exit is
/// published, then the side's [`Entrance`], written once, with release,
/// by the publish that placed it. The exit's code loads its slot with
/// acquire and jumps to the side where it is set, and answers its
/// [`ExitId`](crate::types::ExitId) as before where it is not; no
/// published byte is written again, which is what php's patch of the
/// exit's jump (`zend_jit_link_side_trace`,
/// `Z/ext/opcache/jit/zend_jit_ir.c:17413`) would have done.
///
/// A slot never names code that outlives the code that reads it: a side
/// is published only into the table its parent stands in
/// ([`crate::table::TraceTable::publish`]), the parent's trace holds its
/// slots and the side's trace its `Entrance`, and the table drops every
/// trace together, with its epoch, once no request, job or claim holds
/// it, so a trace running in a request whose epoch holds the table runs
/// beside every trace a slot of it names.
#[derive(Debug, Clone, Default)]
pub(crate) struct EntrySlots(Arc<[AtomicPtr<Entrance>]>);

impl EntrySlots {
    /// `n` slots, none set.
    pub(crate) fn new(n: usize) -> EntrySlots {
        EntrySlots(
            (0..n)
                .map(|_| AtomicPtr::new(std::ptr::null_mut()))
                .collect(),
        )
    }

    /// The address of the first slot, which the code reads past.
    fn base(&self) -> usize {
        self.0.as_ptr().addr()
    }

    /// `side` written to the slot of `exit`, once: false where the slot
    /// is set already or the trace has no such exit, which writes
    /// nothing.
    pub(crate) fn open(&self, exit: ExitId, side: &Entrance) -> bool {
        let entrance = std::ptr::from_ref(side).cast_mut();
        self.0.get(usize::from(exit.0)).is_some_and(|slot| {
            slot.compare_exchange(
                std::ptr::null_mut(),
                entrance,
                Ordering::Release,
                Ordering::Relaxed,
            )
            .is_ok()
        })
    }

    /// The address the slot of `exit` holds, null where it is unset.
    #[cfg(test)]
    pub(crate) fn opened(&self, exit: ExitId) -> Option<usize> {
        let slot = self.0.get(usize::from(exit.0))?;
        Some(slot.load(Ordering::Acquire).addr())
    }
}

/// A trace's landing slots (`jit-design.md` §4.9): where a trace escapes,
/// a return that leaves its lowest frame at an opline it does not go on at,
/// php's `jit_stub_trace_escape`, which goes on into the handler of that
/// opline, the trace compiled there where one is
/// (`Z/ext/opcache/jit/zend_jit_ir.c:11188-11250`), its code compares where
/// the frame stands, its op array and opline, with each slot and jumps to
/// the trace the slot names where one holds it, and answers
/// [`ESCAPED`] as before where none does. A slot is written once, its
/// place before its trace with release, by the crossing the first time it
/// enters a trace compiled at a landing the trace escaped to
/// ([`LandingSlots::remember`]); a landing's word that reads compiled
/// reads so for as long as the table stands, so the slot names what the
/// interpreter would have entered there. [`LANDINGS`] of them, the
/// landings first entered; an escape to another is answered as before.
#[derive(Debug, Clone, Default)]
pub(crate) struct LandingSlots(Arc<[LandingSlot]>);

/// One landing slot: the trace entered there, and where, as
/// [`LandingSlots::key`] writes it, zero while the slot is free.
#[repr(C)]
#[derive(Debug, Default)]
pub(crate) struct LandingSlot {
    entrance: AtomicPtr<Entrance>,
    key: AtomicU64,
}

/// How many landing slots a trace has.
pub(crate) const LANDINGS: usize = 2;

/// Where compiled code reads a landing slot's fields, and its stride.
pub(crate) const LANDING_ENTRANCE: i32 = 0;
pub(crate) const LANDING_KEY: i32 = 8;
pub(crate) const LANDING_BYTES: usize = 16;
const _: () = assert!(
    std::mem::offset_of!(LandingSlot, entrance) == LANDING_ENTRANCE.unsigned_abs() as usize
        && std::mem::offset_of!(LandingSlot, key) == LANDING_KEY.unsigned_abs() as usize
        && size_of::<LandingSlot>() == LANDING_BYTES
);

/// The bit every written key carries, so no key is zero, a free slot's.
pub(crate) const LANDING_TAG: u64 = 1 << 63;

impl LandingSlots {
    /// [`LANDINGS`] slots, none written.
    pub(crate) fn new() -> LandingSlots {
        LandingSlots((0..LANDINGS).map(|_| LandingSlot::default()).collect())
    }

    /// The address of the first slot, which the code reads past.
    fn base(&self) -> usize {
        self.0.as_ptr().addr()
    }

    /// The key of the landing at `opline` of the op array the engine
    /// numbers `func`: the two words a `Stood` holds them in, read as one,
    /// with [`LANDING_TAG`]. An op array's oplines are numbered below
    /// `2^31`.
    pub(crate) fn key(func: u32, opline: u32) -> u64 {
        u64::from(func) | u64::from(opline) << 32 | LANDING_TAG
    }

    /// `landed` remembered as the trace compiled at the landing `key`
    /// names, in the first free slot, once: nothing where a slot holds the
    /// landing already or none is free.
    pub(crate) fn remember(&self, key: u64, landed: &Entrance) {
        if self
            .0
            .iter()
            .any(|slot| slot.key.load(Ordering::Relaxed) == key)
        {
            return;
        }
        let claimed = self.0.iter().find(|slot| {
            slot.key
                .compare_exchange(0, key, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        });
        if let Some(slot) = claimed {
            slot.entrance
                .store(std::ptr::from_ref(landed).cast_mut(), Ordering::Release);
        }
    }

    /// The entrance the slot holding `key` names, none where no slot does.
    #[cfg(test)]
    pub(crate) fn landed(&self, key: u64) -> Option<usize> {
        let slot = self
            .0
            .iter()
            .find(|slot| slot.key.load(Ordering::Relaxed) == key)?;
        Some(slot.entrance.load(Ordering::Acquire).addr())
    }
}

/// The trace a link jumps to (`jit-design.md` §4.3), published before the
/// trace that links to it is compiled, so the jump is in that trace's
/// bytes before they are published, as php's `zend_jit_trace_link_to_root`
/// writes it (`Z/ext/opcache/jit/zend_jit_ir.c:17418-17432`): its
/// [`Entrance`], and whether the jump tests the interrupt first, which
/// php's does where a side trace links to its own root and the root loops
/// (`Z/ext/opcache/jit/zend_jit_trace.c:7285-7303`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Linked {
    pub(crate) body: usize,
    pub(crate) trace: u32,
    pub(crate) interrupt: bool,
    /// The op array, by the engine's number, and the opline the trace
    /// opens at, where a link that names no opline leaves the frame
    /// standing.
    pub(crate) opens: Option<(u32, u32)>,
}

/// Where one recorded opline's code begins: the op array, named with
/// its function since a trace inlines calls, the opline, and the offset
/// in the trace's bytes. What a profiler and a disassembly read; it costs
/// nothing in the code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Mark {
    pub(crate) func: FuncRef,
    pub(crate) opline: OplineIdx,
    pub(crate) offset: u32,
}

/// Why a host compiles no trace (`jit-design.md` §4.3): Cranelift has no
/// ISA for it, or none this build carries, or it is not a 64-bit
/// host. The tier then runs with `Mode::Off`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Unsupported(pub(crate) &'static str);

/// What compiled code answers where a kernel answered other than zero
/// and the opline that called it names no exit: past every
/// [`ExitId`](crate::types::ExitId), which is sixteen bits. `enter` reads
/// what the kernel kept in the `ExitCtx`, a fatal or a panic to resume.
pub(crate) const ABANDONED: u32 = u32::MAX;

/// What compiled code answers where a kernel threw, bailed out or
/// panicked and the trace stored what it keeps in registers, taking no
/// exit (`types::Unwind`): past every [`ExitId`](crate::types::ExitId),
/// and not [`ABANDONED`]. `enter` reads what the kernel kept, and a
/// throwable pending on the request is unwound from where it was raised.
pub(crate) const UNWOUND: u32 = u32::MAX - 1;

/// What compiled code answers where a loop's interrupt test takes php's
/// interrupt stub, `jit_stub_interrupt_handler`, and no exit
/// (`types::Interrupt::Stub`): past every
/// [`ExitId`](crate::types::ExitId), and neither [`ABANDONED`] nor
/// [`UNWOUND`]. Nothing is stored; `enter` stands the frame at the loop's
/// header, [`Compiled::stub`], for `dispatch` to make the loop's
/// interrupt check and enter the trace again there.
pub(crate) const INTERRUPTED: u32 = u32::MAX - 2;

/// What compiled code answers where a loop's trace entered at its body
/// (`types::BodyEntry`) finds the frame holding other than it knows:
/// past every [`ExitId`](crate::types::ExitId), and none of
/// [`ABANDONED`], [`UNWOUND`] and [`INTERRUPTED`]. Nothing ran and
/// nothing is stored.
pub(crate) const DECLINED: u32 = u32::MAX - 3;

/// What compiled code answers where a return past the trace's lowest
/// frame finds the interpreter standing elsewhere than the trace goes on
/// at (`types::Op::Back`'s `escapes`): php's `jit_stub_trace_escape`,
/// which no exit point numbers, so no exit line is written and no word is
/// bumped. Past every [`ExitId`](crate::types::ExitId), and none of
/// [`ABANDONED`], [`UNWOUND`], [`INTERRUPTED`] and [`DECLINED`]. Nothing is
/// stored, since the trace knows nothing of the frame it returned into;
/// `enter` hands back the frame that stands, at the opline the return
/// left it, whose handler `dispatch` runs as php's interpreter does.
pub(crate) const ESCAPED: u32 = u32::MAX - 4;

/// Where compiled code reads, in the `ExitCtx`, where it is entered
/// (`enter::At`), a `u32` that is not zero at a loop's body
/// (`types::BodyEntry`): the field `enter` lays down past the interrupt
/// pointer.
pub(crate) const ENTRY_AT: i32 = 16;

/// Where compiled code reads the interrupt pointer in the `ExitCtx`
/// (`jit-design.md` §4.6): the request at offset zero, the pointer to its
/// interrupt byte at offset eight, the field `enter` lays down with the
/// first trace it enters (#2957).
pub(crate) const INTERRUPT_AT: i32 = 8;

/// Where compiled code reads, in the `ExitCtx`, where the frame stands
/// as the trace enters, a [`Stood`] as a kernel writes one, which `enter`
/// lays down past where it is entered for a trace whose code reads it
/// there ([`Compiled::entry_stood`]).
pub(crate) const STOOD_AT: i32 = 24;

/// Where compiled code reads, in the `ExitCtx`, the address of the
/// request's VM stack (`php_engine::tier::stack`), which it pushes and
/// pops a plain call's frame on in place: past the `Stood`.
pub(crate) const STACK_AT: i32 = 56;

/// Where compiled code reads, in the `ExitCtx`, the address of the
/// request's headers for the calls it pushes in place, one address a call
/// in [`Compiled::calls`]' order, each `php_engine::tier::FRAME`'s
/// `header_bytes` long (`php_engine::tier::call_image`).
pub(crate) const IMAGES_AT: i32 = 64;

/// Where compiled code reads, in the `ExitCtx`, the place on the stack of
/// the frame the trace is entered on, a `u32`: the caller a call it makes
/// in place from that frame names.
pub(crate) const ROOT_AT: i32 = 72;

/// Where compiled code reads, in the `ExitCtx`, whether it pushes and
/// makes its calls in place in this request, a `u32` that is zero where a
/// call's header is not the request's yet, its function not declared, or
/// the cache of a callee it enters ([`Compiled::laid`]) is not laid down,
/// so that it serves them through the kernels instead.
pub(crate) const READY_AT: i32 = 76;

/// Where compiled code reads, in the `ExitCtx`, the address of the
/// request's rows (`enter::Rows`): one address per segment of
/// [`ROW_SEGMENT`] rows, zero for a segment no trace of it was entered
/// in, and in a row, at [`ROW_IMAGES`], the address of the trace's calls'
/// headers in the request and, at [`ROW_READY`], a `u32` that is not
/// zero once the crossing has entered the trace in the request with its
/// calls ready. A jump enters a trace only where its row is ready.
pub(crate) const ROWS_AT: i32 = 80;

/// Where compiled code writes, in the `ExitCtx`, the number of the trace
/// it enters by a jump, which `enter` reads the exit it answers against:
/// `enter` writes the number of the trace it calls.
pub(crate) const TRACE_AT: i32 = 88;

/// The rows a segment of a request's rows holds, as many as a segment of
/// the trace table holds slots, so a trace's number names its row.
pub(crate) const ROW_SEGMENT: usize = crate::table::SEGMENT;
const _: () = assert!(ROW_SEGMENT.is_power_of_two());

/// A row's bytes, and where in it its fields are (`enter::Row`).
pub(crate) const ROW_BYTES: i64 = 16;
pub(crate) const ROW_IMAGES: i32 = 0;
pub(crate) const ROW_READY: i32 = 8;

/// Whether `ir`'s code reads where the frame stands as it enters: its
/// first op holds it, php's IP guard; a call its frame began before
/// the trace is under way before any kernel stood the frame, whose room
/// is the stood call's; or it holds the stack's room, php's stack check,
/// at its entry or at a call it pushes.
fn stood_at_entry(ir: &TraceIr) -> bool {
    let ip_guarded = (ir.guards.iter())
        .any(|guard| guard.at == GuardAt::Entry && matches!(guard.check, Check::Stands { .. }));
    let begun_before = (ir.ops.iter())
        .take_while(|op| !matches!(op, Op::Line(_)))
        .any(|op| {
            matches!(
                op,
                Op::InitCall {
                    began: Began::Before,
                    ..
                }
            )
        });
    let room_held = (ir.guards.iter()).any(|guard| matches!(guard.check, Check::Room { .. }));
    ip_guarded || begun_before || room_held
}

/// The plain calls `ir` pushes in place, each once, which the compiled
/// code's copies of their headers are read from, and the op arrays they
/// enter but the frame's own, whose caches `enter` asks are laid
/// ([`Compiled::callee_caches`]).
fn in_place(ir: &TraceIr) -> (Box<[PlainCall]>, Box<[FuncRef]>) {
    let mut calls: Vec<PlainCall> = Vec::new();
    let mut caches: Vec<FuncRef> = Vec::new();
    for op in &ir.ops {
        let Op::InitCall {
            frame,
            plain: Some(plain),
            ..
        } = op
        else {
            continue;
        };
        if !calls.contains(plain) {
            calls.push(plain.clone());
        }
        let own = ir.frames.get(usize::from(frame.0)).map(|info| info.func);
        if own != Some(plain.callee()) && !caches.contains(&plain.callee()) {
            caches.push(plain.callee());
        }
    }
    (calls.into_boxed_slice(), caches.into_boxed_slice())
}

/// Where the trace's first recorded opline stands.
fn opens_at(ir: &TraceIr) -> Option<(FuncRef, OplineIdx)> {
    ir.ops.iter().find_map(|op| match op {
        Op::Line(line) => {
            let frame = ir.frames.get(usize::from(line.frame.0))?;
            Some((frame.func, line.opline))
        }
        _ => None,
    })
}

/// The opline a loop whose interrupt test takes php's stub stands its
/// frame at, its header's.
fn stub_at(ir: &TraceIr) -> Option<OplineIdx> {
    let End::Loop(header) = ir.end else {
        return None;
    };
    match header.interrupt {
        Interrupt::Stub(at) => Some(at),
        Interrupt::Exit(_) => None,
    }
}

/// A kernel compiled code calls (`jit-design.md` §4.6, §5.2), by the
/// wrapper `enter`'s `kernels!` list makes of it. Every wrapper is
/// `extern "C"` on the host's convention, takes the `ExitCtx`, the room
/// of the frame the opline runs in and the opline's index first, and
/// answers the crossing's status word; where it writes a value, the
/// pointer it writes through is its last argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kernel {
    /// `step(cx, frame, opline: u32, func: u32, stood: *mut Stood)`: the
    /// opline served whole through `php_engine::tier::step_held` (§5.2)
    /// over the op array the engine numbers `func`, the one the trace
    /// recorded, and where the interpreter stands after it written to
    /// `stood`.
    Step,
    /// `push_call(cx, frame, opline: u32, func: u32, callee: u32, stood:
    /// *mut Stood)`: an inline `INIT_*` of the op array the engine numbers
    /// `func`, served as its handler serves it where
    /// its callee resolves to the op array the engine numbers `callee`;
    /// where it resolves to another, nothing is pushed and `stood.call`
    /// is null.
    PushCall,
    /// `call(cx, frame, opline: u32, func: u32, callee: u32, stood: *mut
    /// Stood)`: a `DO_*CALL` of the op array the engine numbers `func`,
    /// to the callee the trace recorded, which it numbers `callee`, made
    /// through `php_engine::tier::call_held` with no dispatch, and where
    /// the interpreter stands after it, in the callee, written to `stood`.
    Call,
    /// `release(cx, frame, opline: u32, word: u64, type_info: u32)`:
    /// php's `zval_ptr_dtor` of the counted value the zval names, a
    /// destructor's throw answered as one.
    Release,
    /// `copy(cx, frame, opline: u32, word: u64, type_info: u32, out: *mut
    /// Zval)`: php's `ZVAL_COPY_DEREF` of the value the zval names into
    /// `out`.
    Copy,
}

impl Kernel {
    /// Every kernel, in the order [`Symbols`] keeps them.
    pub(crate) const ALL: [Kernel; 5] = [
        Kernel::Step,
        Kernel::PushCall,
        Kernel::Call,
        Kernel::Release,
        Kernel::Copy,
    ];

    /// The kernel's place in [`Kernel::ALL`].
    const fn index(self) -> usize {
        self as usize
    }
}

/// The kernel symbol table (`jit-design.md` §4.3): the address of each
/// kernel's wrapper, which compiled code calls as a 64-bit constant, so
/// its bytes are final. `enter`'s `kernels!` list makes the one the
/// engine's `Backend` is built with (#2957).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Symbols([usize; Kernel::ALL.len()]);

impl Symbols {
    /// The table of the addresses `address` names, one per kernel.
    pub(crate) fn new(address: impl Fn(Kernel) -> usize) -> Symbols {
        Symbols(Kernel::ALL.map(address))
    }

    /// The address of `kernel`'s wrapper.
    pub(crate) fn address(&self, kernel: Kernel) -> usize {
        self.0[kernel.index()]
    }
}

/// Where the interpreter stands after a kernel served an opline whole
/// ([`Kernel::Step`], [`Kernel::PushCall`]): the room of the frame that
/// runs, php's `EX`; the room of the call under way in it, `EX(call)`,
/// null where none is; the op array it serves, by the engine offset that
/// numbers it (`FuncRef`); the opline it goes on at; and the bytes the
/// stack's top page has left for frames, php's `EG(vm_stack_end) -
/// EG(vm_stack_top)`. A local of compiled code the kernel writes, by
/// which compiled code follows the frame chain: the call it enters, the
/// return it lands from, the call its sends write, the caller php's IP
/// guard holds, the way a stepped jump went, and the room php's stack
/// check holds against the frames the trace pushes.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Stood {
    pub(crate) frame: *mut u8,
    pub(crate) call: *mut u8,
    pub(crate) func: u32,
    pub(crate) opline: u32,
    pub(crate) room: usize,
}

impl Stood {
    /// Where nothing stood: a null frame and call, and no room.
    pub(crate) const NOWHERE: Stood = Stood {
        frame: std::ptr::null_mut(),
        call: std::ptr::null_mut(),
        func: 0,
        opline: 0,
        room: 0,
    };
}

/// The host's `Backend` (`jit-design.md` §4.3): its ISA with the flags the
/// [`Policy`] asks for, the kernel table compiled code calls, and the
/// prologue every trace's bytes begin with. It holds no state of one
/// compile, so one serves every compile of the engine.
///
/// **The prologue** is php's trace prologue
/// (`zend_jit_trace_prologue_size`, `Z/ext/opcache/jit/zend_jit_ir.c:3252`):
/// one host-convention function, `extern "C" fn(cx, frame, body) -> u32`,
/// that calls the body at `body` on the tail convention and answers what
/// it answers, so `enter` crosses into any trace through it and a trace
/// that jumps to another passes over it, as php's link jumps past the
/// prologue to the code after it. It is compiled once, with the
/// `Backend`, and copied ahead of each body.
pub(crate) struct Backend {
    isa: OwnedTargetIsa,
    symbols: Symbols,
    /// Whether every guard fails once, `jit.mode=exit-fuzz`.
    fuzz: bool,
    /// The prologue's bytes and the alignment they need.
    prologue: (Box<[u8]>, usize),
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backend")
            .field("isa", &self.isa.triple().to_string())
            .finish_non_exhaustive()
    }
}

// The worker compiles on its own thread with the engine's one backend
// (`jit-design.md` §4.3, §4.8), which the design left unverified.
const _: () = {
    const fn shared<T: Send + Sync>() {}
    shared::<Backend>();
};

/// How a host's ISA is found: `cranelift-native`'s lookup on a real host.
type Host = fn() -> Result<isa::Builder, &'static str>;

/// A function Cranelift compiled: its bytes, the alignment their first
/// needs, and where each op's code is.
struct Machine {
    bytes: Vec<u8>,
    align: usize,
    srclocs: Vec<MachSrcLoc<Final>>,
}

impl Backend {
    /// The host's `Backend`, its ISA found through `cranelift-native`, with the
    /// flags `policy` asks for, calling the kernels `symbols` names; or
    /// `Unsupported` where the host has none, which runs the tier with
    /// `Mode::Off` (`jit-design.md` §4).
    pub(crate) fn new(policy: &Policy, symbols: Symbols) -> Result<Backend, Unsupported> {
        Backend::on(cranelift_native::builder, policy, symbols)
    }

    /// The `Backend` of the ISA `host` finds.
    fn on(host: Host, policy: &Policy, symbols: Symbols) -> Result<Backend, Unsupported> {
        let builder = host().map_err(Unsupported)?;
        let isa = builder
            .finish(flags()?)
            .map_err(|_| Unsupported("the host's ISA refused the tier's flags"))?;
        if isa.pointer_bits() != 64 {
            return Err(Unsupported("a 32-bit host"));
        }
        let prologue = machine(&*isa, clif::prologue(isa.frontend_config()))
            .map_err(|_| Unsupported("the host's ISA refused the trace prologue"))?;
        Ok(Backend {
            isa,
            symbols,
            fuzz: policy.mode() == Mode::ExitFuzz,
            prologue: (prologue.bytes.into_boxed_slice(), prologue.align),
        })
    }

    /// `ir` in final bytes, named `name`: the prologue, then the body
    /// [`clif::lower`] builds, compiled for the host, with a [`Mark`] per
    /// recorded opline, its link a jump to `linked` where it links to a
    /// trace with code. Bytes that would need relocating are php's
    /// `COMPILER_ERROR`, as is any refusal of Cranelift's: kernel
    /// addresses, the entry slots' and a link's are constants, so a trace's
    /// bytes work wherever the arena places them.
    pub(crate) fn compile(
        &self,
        ir: &TraceIr,
        name: TraceName,
        linked: Option<Linked>,
    ) -> Result<Compiled, Abort> {
        let config = self.isa.frontend_config();
        let (calls, callee_caches) = in_place(ir);
        let slots = EntrySlots::new(ir.exits.len());
        let landings = LandingSlots::new();
        let joins = clif::Joins {
            slots: slots.base(),
            landings: landings.base(),
            linked,
        };
        // Under exit-fuzz a first pass counts the guards, one byte each.
        let fuzzed = if self.fuzz {
            let (_, sites) = clif::lower(
                ir,
                config,
                (&self.symbols, &calls),
                (Some(clif::Fuzz::COUNTING), joins),
            )?;
            Fuzzed::new(sites)
        } else {
            Fuzzed::default()
        };
        let fuzz = self.fuzz.then(|| fuzzed.fuzz());
        let (func, _) = clif::lower(ir, config, (&self.symbols, &calls), (fuzz, joins))?;
        let body = machine(&*self.isa, func)?;
        let (prologue, prologue_align) = (&self.prologue.0, self.prologue.1);
        let body_offset = prologue.len().next_multiple_of(body.align);
        let mut bytes = Vec::with_capacity(body_offset + body.bytes.len());
        bytes.extend_from_slice(prologue);
        bytes.resize(body_offset, 0);
        bytes.extend_from_slice(&body.bytes);
        let marks = marks(ir, &body.srclocs, (body_offset, bytes.len()));
        Ok(Compiled {
            bytes,
            entry_offset: 0,
            body_offset,
            align: prologue_align.max(body.align),
            exits: ir.exits.clone().into_boxed_slice(),
            leaves: match ir.end {
                End::Leave(exit) | End::Return(exit) => Some(exit),
                End::Loop(_) => None,
            },
            stub: stub_at(ir),
            opens: opens_at(ir),
            entry_stood: stood_at_entry(ir),
            body: ir.body.as_ref().map(|body| body.opline),
            map_ptrs: ir.map_ptrs.clone().into_boxed_slice(),
            marks,
            name,
            fuzzed,
            slots,
            landings,
            callee_caches,
            calls,
        })
    }

    /// The body `ir` lowers to, before Cranelift's passes: the CLIF the
    /// goldens hold, its entry slots at a made-up address so the text is
    /// one on every machine.
    #[cfg(test)]
    fn clif(
        &self,
        ir: &TraceIr,
        linked: Option<Linked>,
    ) -> Result<cranelift_codegen::ir::Function, Abort> {
        let (calls, _) = in_place(ir);
        let joins = clif::Joins {
            slots: 0x6000_0000,
            landings: 0x6100_0000,
            linked,
        };
        clif::lower(
            ir,
            self.isa.frontend_config(),
            (&self.symbols, &calls),
            (None, joins),
        )
        .map(|(func, _)| func)
    }
}

/// `func` compiled for `isa`: none where Cranelift refuses it or its
/// bytes would need relocating, php's `COMPILER_ERROR`.
fn machine(
    isa: &dyn isa::TargetIsa,
    func: cranelift_codegen::ir::Function,
) -> Result<Machine, Abort> {
    let mut ctx = Context::for_function(func);
    let code = ctx
        .compile(isa, &mut ControlPlane::default())
        .map_err(|_| refused("Cranelift refused the trace's function"))?;
    if !code.buffer.relocs().is_empty() {
        return Err(refused("the trace's code needs relocating"));
    }
    let align = usize::try_from(code.buffer.alignment.max(1))
        .map_err(|_| refused("an alignment past the host's"))?;
    Ok(Machine {
        bytes: code.code_buffer().to_vec(),
        align,
        srclocs: code.buffer.get_srclocs_sorted().to_vec(),
    })
}

/// The ISA's flags (`jit-design.md` §4.3): optimised for speed, the
/// verifier where assertions are, and frame pointers, which a body's tail
/// call is made over and a profiler walks where `opcache.jit_debug` asks
/// for `PERF` or `PERF_DUMP`.
fn flags() -> Result<settings::Flags, Unsupported> {
    let mut flags = settings::builder();
    let mut set = |name: &str, value: &str| {
        flags
            .set(name, value)
            .map_err(|_| Unsupported("Cranelift lacks a flag the tier sets"))
    };
    set("opt_level", "speed")?;
    set(
        "enable_verifier",
        if cfg!(debug_assertions) {
            "true"
        } else {
            "false"
        },
    )?;
    // A body hands on by a tail call, which Cranelift makes over the frame
    // pointer on x86-64; macOS on aarch64 keeps one in every frame anyway.
    set("preserve_frame_pointers", "true")?;
    Ok(settings::Flags::new(flags))
}

/// php's `COMPILER_ERROR`, for a trace Cranelift would not compile.
fn refused(why: &'static str) -> Abort {
    Abort {
        stop: Stop::CompilerError,
        why,
    }
}

/// Where each recorded opline's code begins in a trace's bytes, its body
/// at `body` of `len`: the lowest offset of the code its op left, and
/// where Cranelift folded an op's code away, where the next op's begins.
fn marks(ir: &TraceIr, srclocs: &[MachSrcLoc<Final>], (body, len): (usize, usize)) -> Box<[Mark]> {
    let end = u32::try_from(len).unwrap_or(u32::MAX);
    let body = u32::try_from(body).unwrap_or(u32::MAX);
    let mut starts = vec![None::<u32>; ir.ops.len()];
    for loc in srclocs {
        if loc.loc.is_default() {
            continue;
        }
        if let Some(start) = starts.get_mut(loc.loc.bits() as usize) {
            let at = loc.start.saturating_add(body);
            *start = Some(start.map_or(at, |s| s.min(at)));
        }
    }
    let mut next = end;
    for start in starts.iter_mut().rev() {
        next = *start.get_or_insert(next);
    }
    ir.ops
        .iter()
        .zip(starts)
        .filter_map(|(op, start)| match op {
            Op::Line(line) => Some(Mark {
                func: ir.frames.get(usize::from(line.frame.0))?.func,
                opline: line.opline,
                offset: start.unwrap_or(end),
            }),
            _ => None,
        })
        .collect()
}

/// The source location compiled code carries for the op at `at`, which
/// [`marks`] reads back.
fn located(at: usize) -> SourceLoc {
    SourceLoc::new(u32::try_from(at).unwrap_or(u32::MAX - 1))
}
