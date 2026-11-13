//! What the compiled tier reads of the op arrays and classes a request's
//! epoch holds (`jit-design.md` §3.2, §4.1 *The owned view*, §5.3;
//! design §5.14 *Identities*, §6.3): the safepoint word a frame's op
//! array carries, the [`FuncRef`] that names the op array, the
//! [`Epoch`] a request holds, and [`UnitView`], the owned view a trace
//! is built from on a thread with no request.
//!
//! An op array of a unit the epoch's cache holds is named by the
//! map-pointer offset the engine gave it as the unit was published,
//! which is its own and stands as long as the epoch
//! ([`super::published`]); one compiled for its request alone is named
//! by the request's offset, which no epoch holds a row at. So a view
//! accepts the one and refuses the other by the offset's kind, and holds
//! nothing but the epoch: whatever it accepted, the epoch keeps.

use std::sync::Arc;

use php_heap::{AnyState, EngineState, ZvalType};
use php_link::LinkedClass;
use php_unit::decl::{Bare, FunctionDecl, HookKind, Name, Parameter, Type, Visibility};
use php_unit::{CacheSlot, MapPtrOffset, OpArray, Opcode, OperandKind, Opline, OplineIdx};

use super::facts::{ClassFacts, Linked, OwnClass, RecordClass};
use super::published::SEALED;
use super::{FrameRef, Request, SafepointIdx, Word};
use crate::api::values::ClassId;
use crate::arena::Arena;

/// A hold on the epoch a request started in (design §5.14): its arena,
/// whose published classes, held units and their words, and the
/// engine's interned strings they name, stand while any hold does,
/// whatever restart replaced it since and whether or not the engine
/// still stands. `Send` and `Sync`, so
/// a claim and a view can leave the request's thread.
#[derive(Clone)]
pub struct Epoch(Arc<Arena>);

impl std::fmt::Debug for Epoch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Epoch").field(&Arc::as_ptr(&self.0)).finish()
    }
}

impl Epoch {
    /// The word of safepoint `at` of the op array `f` names: zero at
    /// publication and the tier's after it; the sealed word for an op
    /// array this epoch holds no row for, a request's, and for a number
    /// past its last safepoint.
    #[must_use]
    pub fn word(&self, f: impl Into<FuncRef>, at: SafepointIdx) -> &Word {
        self.func(f).map_or(&SEALED, |func| func.word(at))
    }

    /// The landing word of opline `at` of the op array `f` names
    /// (`jit-design.md` §2.2): the word of the returns that land there,
    /// which php's `zend_jit_trace_setup_ret_counter` arms on the opline
    /// past a call; sealed until a tier armed it
    /// ([`Epoch::lay_landing`]), and for an op array this epoch holds no
    /// row for.
    #[must_use]
    pub fn landing(&self, f: impl Into<FuncRef>, at: OplineIdx) -> &Word {
        self.func(f).map_or(&SEALED, |func| func.landing(at))
    }

    /// [`Epoch::landing`] where the op array's landing words are laid
    /// down, each of which stands where it is for as long as the epoch,
    /// so a tier may keep its address from one return to the next. None
    /// where they are not, when the landing reads sealed until a tier lays
    /// them, and for an op array this epoch holds no row for.
    #[must_use]
    pub fn laid_landing(&self, f: impl Into<FuncRef>, at: OplineIdx) -> Option<&Word> {
        self.func(f)?.laid_landing(at)
    }

    /// The landing word of opline `at` of the op array `f` names, for a
    /// tier to arm, the op array's landing words laid down sealed the
    /// first time; none for an op array this epoch holds no row for, and
    /// past its last opline.
    #[must_use]
    pub fn lay_landing(&self, f: impl Into<FuncRef>, at: OplineIdx) -> Option<&Word> {
        self.0.lay_landing(EngineFuncRef::of(f.into())?.0, at)
    }

    /// Where compiled code finds the landing words of the op arrays this
    /// epoch holds, the ones it published and the preload's
    /// (`jit-design.md` §4.9): what a trace's escape reads, laid down
    /// where it was not.
    #[must_use]
    pub fn landing_index(&self) -> super::LandingIndex {
        self.0.landing_index()
    }

    /// The op array `f` names, held by this epoch for as long as the
    /// borrow: what compiled code that recorded `f` in this epoch hands a
    /// step ([`super::step_held`]), so the step reads the op array the
    /// trace holds and no frame's function. None for an op array this
    /// epoch holds no row for, a request's.
    #[must_use]
    pub fn held(&self, f: impl Into<FuncRef>) -> Option<Held<'_>> {
        self.func(f)?.op_array().map(Held)
    }

    /// The row of a held op array, none for a request's.
    fn func(&self, f: impl Into<FuncRef>) -> Option<&super::published::Func> {
        self.row(EngineFuncRef::of(f.into())?)
    }

    /// The row of the op array the engine numbered `f`, where this epoch
    /// holds one.
    fn row(&self, f: EngineFuncRef) -> Option<&super::published::Func> {
        self.0.func(f.0)
    }

    /// The tier's value for this epoch where the tier made one
    /// ([`epoch_state`]) and it is a `T`: what a holder that outlives
    /// its request reads, as an exit's claim reads its trace's word
    /// (`jit-design.md` §4.5, §4.7).
    #[must_use]
    pub fn tier_state<T: std::any::Any>(&self) -> Option<&T> {
        self.0.tier.get()?.downcast_ref()
    }

    /// Whether `req` started in this epoch.
    fn of(&self, req: &Request) -> bool {
        Arc::ptr_eq(&self.0, req.arena())
    }

    /// An epoch of its own over `unit`, held by its cache as an engine
    /// holds a file, with the [`FuncRef`] of its main op array: a fixture
    /// for a tier's tests, which build no engine.
    #[cfg(any(test, feature = "test"))]
    #[must_use]
    pub fn fixture(mut unit: php_unit::Unit) -> (Epoch, FuncRef) {
        let numbers = std::sync::atomic::AtomicU32::new(0);
        crate::engine::assign_map_ptrs(&mut unit, &numbers);
        let unit = unit.published(None);
        let files = crate::unit_cache::Files::new(1);
        files.funcs.publish(&unit, crate::tier::Laid::COLD);
        let arena = Arena::of_engine(&[], 0, files, crate::arena::Interned::default());
        (Epoch(Arc::new(arena)), FuncRef(unit.main.runtime_cache))
    }
}

/// The epoch `req` started in, held.
#[must_use]
pub fn epoch(req: &Request) -> Epoch {
    Epoch(Arc::clone(req.arena()))
}

/// The tier's value for the epoch `req` started in, `make`'s the first
/// time any request of the epoch asks, so it is the epoch's and goes
/// with it: the trace table, which a restart's epoch starts without
/// (`jit-design.md` §4.5). A borrow of the request's own hold, which
/// takes nothing; none where the epoch holds a value of another type.
///
/// Every request of the epoch reads it, so it is engine state (ADR-0005,
/// #4492): a value holding a request's, a pointer into that request's
/// heap, is refused,
///
/// ```compile_fail,E0277
/// # fn main() {}
/// include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../xtask/fixtures/api-check/engine-state/epoch.rs"));
/// ```
pub fn epoch_state<T: EngineState + std::any::Any + Send + Sync>(
    req: &Request,
    make: impl FnOnce() -> T,
) -> Option<&T> {
    let held = req.arena().tier.get_or_init(|| AnyState::new(make()));
    held.downcast_ref()
}

/// One op array as the tier names it (`jit-design.md` §3.1): by the
/// map-pointer offset its unit's publication gave it, which no other op
/// array of the engine shares, where its unit is held; by the request's
/// own offset where it was compiled for the request alone.
///
/// So it may be a request's, and is no engine state (design §5.14,
/// *Identities*; #4631): engine state names an op array by the
/// [`EngineFuncRef`] made from one, and a `FuncRef` asked to stand there
/// is refused,
///
/// ```compile_fail,E0277
/// fn engine_state<T: php_engine::api::abi::EngineState>() {}
/// engine_state::<php_engine::tier::EngineFuncRef>();
/// engine_state::<php_engine::tier::FuncRef>();
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FuncRef(MapPtrOffset);

/// An op array the engine numbered, by the map-pointer offset its unit's
/// publication gave it (design §5.14, *Identities*; #4631): the name an
/// op array has in engine state, the epoch's tier state among it.
///
/// It may stand there, and derives `EngineState`, because the engine
/// never reuses one of its map-pointer offsets: `map_ptr_last` only adds,
/// so the number names one op array for as long as the engine stands,
/// whichever epoch holds its row. A request's offset, which numbers
/// another op array in every other request, has none: the one way to
/// make one is [`EngineFuncRef::of`], which refuses it, and where a
/// [`FuncRef`] is asked for, this is one, by `From`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, php_macros::EngineState)]
pub struct EngineFuncRef(u32);

impl EngineFuncRef {
    /// The op array `f` names, where the engine numbered it; none for one
    /// compiled for its request alone. The one place an `EngineFuncRef`
    /// is made.
    #[must_use]
    pub const fn of(f: FuncRef) -> Option<EngineFuncRef> {
        match f.0 {
            MapPtrOffset::Engine(offset) => Some(EngineFuncRef(offset)),
            MapPtrOffset::Request(_) => None,
        }
    }

    /// The engine's map-pointer offset that numbers the op array: what
    /// compiled code, the kernels and a recorded trace's text form name
    /// it by, and where the epoch keeps its row.
    #[must_use]
    pub const fn offset(self) -> u32 {
        self.0
    }
}

impl From<EngineFuncRef> for FuncRef {
    fn from(f: EngineFuncRef) -> FuncRef {
        FuncRef(MapPtrOffset::Engine(f.0))
    }
}

impl FuncRef {
    /// The map-pointer offset that names the op array, the engine's or
    /// the request's: where the request's map table holds its runtime
    /// cache, which [`laid`](super::laid) reads. A recorded trace's text form writes
    /// an [`EngineFuncRef`]'s offset (`jit-design.md` §4.1).
    #[must_use]
    pub fn offset(self) -> MapPtrOffset {
        self.0
    }

    /// The op array `offset` names, read back from a recorded trace's
    /// text form; whether an epoch holds a row there is
    /// [`Epoch::word`]'s and [`UnitView::hold`]'s to say.
    #[must_use]
    pub const fn at(offset: MapPtrOffset) -> FuncRef {
        FuncRef(offset)
    }
}

/// The op array a holder holds for a frame (`jit-design.md` §2.2, #3144):
/// the one the loop has in hand at a safepoint, or the one an epoch holds
/// for a trace that recorded it ([`Epoch::held`]). Only those hand one
/// out, so the borrow is the hold: the op array stands for as long as the
/// borrow. Not the op array itself, so what a tier reads of it is what
/// these methods say, its offset, its words and its safepoints, as php's
/// `zend_jit_trace_counter_helper` reads `ZEND_OP_TRACE_INFO(opline,
/// offset)` from the op array the interpreter has in hand.
#[derive(Clone, Copy)]
pub struct Held<'a>(&'a OpArray);

impl std::fmt::Debug for Held<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Held").field(&self.0.runtime_cache).finish()
    }
}

impl<'a> Held<'a> {
    /// The op array the loop holds in hand.
    pub(crate) fn of(code: &'a OpArray) -> Held<'a> {
        Held(code)
    }

    /// The op array itself: what the engine's own loop runs, and what a
    /// tier reads of the code of a frame it goes on in ([`served`]).
    #[must_use]
    pub fn code(self) -> &'a OpArray {
        self.0
    }

    /// The [`FuncRef`] that names the op array, its `runtime_cache`
    /// offset.
    #[must_use]
    pub fn func(self) -> FuncRef {
        FuncRef(self.0.runtime_cache)
    }

    /// The word of safepoint `at`, what the tier reads at a site
    /// (`jit-design.md` §3.2), read at the op array's offset in the epoch
    /// `req` started in: the sealed word for an op array no epoch holds,
    /// as `eval`'s and an uncached include's, and past the op array's last
    /// safepoint.
    #[must_use]
    pub fn word(self, req: &Request, at: SafepointIdx) -> &Word {
        row(req, self.0.runtime_cache).map_or(&SEALED, |func| func.word(at))
    }

    /// The opline safepoint `at` stands at, the back-edge's header or the
    /// `RETURN` its number names, where a tier that serves the site's
    /// opline itself stands the frame first: a `RETURN`'s inline path
    /// raises its site before it stands its frame there. None past the
    /// last safepoint.
    #[must_use]
    pub fn safepoint_at(self, at: SafepointIdx) -> Option<OplineIdx> {
        self.0.safepoints.get(usize::try_from(at.0).ok()?).copied()
    }
}

/// The row of the epoch `req` started in at `offset`, none for a
/// request's offset.
fn row(req: &Request, offset: MapPtrOffset) -> Option<&super::published::Func> {
    req.arena().func(EngineFuncRef::of(FuncRef(offset))?.0)
}

/// The op array an activated user frame runs, [`func`]'s answer, read
/// where it can be off its `EX(run_time_cache)` as php's compiled code
/// reads `EX(func)`, with no lookup of its function: what a kernel names
/// the frame it leaves the interpreter in by. Looked up as [`func`] looks
/// it up where the frame reads a cache that is no op array's own, a
/// closure's or a trait method's copy's, and where it serves a request's
/// op array; none for a frame that is no activated user frame, a call
/// being built or a native's.
#[must_use]
pub fn running_func(req: &Request, frame: FrameRef) -> Option<FuncRef> {
    let header = req.vm().at(frame)?;
    if !header.info.user() {
        return None;
    }
    let offset = header.runtime_cache().offset();
    if own_row(req, header).is_some() {
        return Some(FuncRef(offset));
    }
    func(req, frame)
}

/// The cache an activated user frame reads, `EX(run_time_cache)`, named
/// as the op array whose own cache it is would be: the op array it runs
/// where the cache is that op array's own, which a caller that holds that
/// op array's row knows; a closure's or a trait method's copy's
/// otherwise, which no op array's [`FuncRef`] names ([`running_func`]
/// tells the two apart). None for a frame that is no activated user
/// frame.
#[must_use]
pub fn frame_cache(req: &Request, frame: FrameRef) -> Option<FuncRef> {
    let header = req.vm().at(frame)?;
    header
        .info
        .user()
        .then(|| FuncRef(header.runtime_cache().offset()))
}

/// The row of the op array whose own cache `header`'s frame reads, an
/// activated user frame's, where the epoch holds one at that offset: none
/// for a cache that is no op array's own (a closure's, a trait method's
/// copy's), for a request's, and for any other frame.
#[inline]
fn own_row<'r>(
    req: &'r Request,
    header: &crate::exec::Header,
) -> Option<&'r super::published::Func> {
    match header.runtime_cache().offset() {
        MapPtrOffset::Engine(offset) if header.info.user() => req.arena().func(offset),
        _ => None,
    }
}

/// The op array `frame` runs and the [`FuncRef`] of the cache it reads,
/// where it reads its op array's own cache and every frame that runs the
/// op array runs it in one scope ([`super::published::Func::fixes_scope`]):
/// so it is no closure's and no trait method's copy's, and a slot of its
/// cache answers each frame alike.
///
/// The recorder admits only these op arrays, and compiled code's guard
/// on a method's pair rests on that refusal (`jit-design.md` §4.3): it
/// tests the slot's tag and that the class past it is the object's
/// (`php_jit`'s `cache_guard`), where [`Request::cached_callee`] also
/// tests that the frame reads the slot back, by its function or its
/// scope, and a frame of an op array whose scope is fixed reads back
/// every pair its op array's frames wrote. `cached_callee` never tests
/// the fake scope (`EG(fake_scope)`, [`Request::in_fake_scope`]), and a
/// resumed generator or fiber may stand under one, as php's does, so the
/// guard asks nothing of it either.
fn scoped_code(req: &Request, frame: FrameRef) -> Option<(EngineFuncRef, &OpArray)> {
    let header = req.vm().at(frame)?;
    let func = own_row(req, header).filter(|func| func.fixes_scope())?;
    let f = EngineFuncRef::of(FuncRef(header.runtime_cache().offset()))?;
    Some((f, func.op_array()?))
}

/// The op array `frame` serves; none for a frame that serves none.
#[must_use]
pub fn func(req: &Request, frame: FrameRef) -> Option<FuncRef> {
    Some(FuncRef(req.served_op_array(frame)?.runtime_cache))
}

/// The row of the op array `frame` serves, where an epoch holds it: none
/// for a request's op array and a frame that serves none. An activated
/// frame that reads its op array's own cache names its row by its
/// `EX(run_time_cache)` ([`own_row`]), with no lookup of its function;
/// any other frame is looked up.
fn held(req: &Request, frame: FrameRef) -> Option<&super::published::Func> {
    let header = req.vm().at(frame)?;
    own_row(req, header).or_else(|| row(req, req.served_op_array(frame)?.runtime_cache))
}

/// The oplines the opline at `at` of `code` may jump to, past falling
/// through to the next one, as `zend_dump_op` prints them and
/// `zend_build_cfg` draws its edges: each target of a `SWITCH_LONG`,
/// `SWITCH_STRING` or `MATCH`'s table, its `CONST` `op2`, in the table's
/// order, then the one field `zend_vm_opcodes_flags` shapes `JMP_ADDR`
/// ([`php_unit::Opline::jump_target`]): `op1` for a `JMP` or a
/// `FAST_CALL`, `op2` for a conditional jump, a `FE_RESET_*` or a `CATCH`
/// that is not its try's last, and `extended_value` for a `FE_FETCH_*`
/// or a table's default. None for an opline that jumps nowhere, and past
/// the last.
pub fn jump_targets(code: &OpArray, at: OplineIdx) -> impl Iterator<Item = OplineIdx> + use<'_> {
    let op = code.oplines.get(at.0 as usize);
    let table = op
        .filter(|op| {
            matches!(
                op.opcode,
                Opcode::SwitchLong | Opcode::SwitchString | Opcode::Match
            ) && op.op2_type == OperandKind::Const
        })
        .and_then(|op| code.literals.get(op.op2 as usize))
        .and_then(|zval| zval.as_arr());
    let cases = table.into_iter().flat_map(|table| {
        table.iter().filter_map(|(_, target)| {
            target
                .as_long()
                .and_then(|at| u32::try_from(at).ok())
                .map(OplineIdx)
        })
    });
    cases.chain(op.and_then(Opline::jump_target))
}

/// The op array `frame` serves, as the epoch `req` started in holds it,
/// for as long as the request: what a tier reads where the interpreter
/// goes on in a frame whose op array the loop does not hand it, a
/// return's caller (`jit-design.md` §4.9). None for a request's op array
/// and a frame that serves none.
#[must_use]
pub fn served(req: &Request, frame: FrameRef) -> Option<Held<'_>> {
    held(req, frame)?.op_array().map(Held)
}

/// The landing word of opline `at` of the op array `frame` serves, what
/// the tier reads where a return lands there ([`Epoch::landing`]): the
/// sealed word until a tier armed it, and for an op array no epoch holds;
/// with that op array ([`served`]), read off the one row, which the tier
/// reads past a landing no trace armed (`jit-design.md` §4.9).
#[must_use]
pub fn landing(req: &Request, frame: FrameRef, at: OplineIdx) -> (&Word, Option<Held<'_>>) {
    held(req, frame).map_or((&SEALED, None), |func| {
        (func.landing(at), func.op_array().map(Held))
    })
}

/// The landing word of the opline `frame` stands at, in the op array it
/// serves: what a tier's inlined test of a return reads in the caller it
/// popped to (`jit-design.md` §2.2), its position and its op array's row
/// and nothing else, the row read off the frame's
/// `EX(run_time_cache)`, and looked up only where that is no
/// op array's own cache, a closure's or a trait method's copy's. Sealed
/// where the frame stands nowhere, as [`landing`] reads it. One call, so
/// each `RETURN*` arm the test is inlined in keeps to the call and the
/// compare.
#[must_use]
#[inline(never)]
pub fn landing_stood(req: &Request, frame: FrameRef) -> &Word {
    let Some(header) = req.vm().at(frame) else {
        return &SEALED;
    };
    let at = OplineIdx(header.opline);
    match own_row(req, header) {
        Some(func) => func.landing(at),
        None => landing_looked_up(req, frame, at),
    }
}

/// [`landing_stood`] for a frame that reads no op array's own cache: its
/// op array looked up. Out of line, so the test in the `RETURN*` arms
/// keeps to what it reads there.
#[cold]
#[inline(never)]
fn landing_looked_up(req: &Request, frame: FrameRef, at: OplineIdx) -> &Word {
    landing(req, frame, at).0
}

/// Where a request keeps one slot of a held op array's `run_time_cache`:
/// the engine's map-pointer offset every request lays that cache at, and
/// the slot in it, which compiled code reads past the request's map
/// base (`jit-design.md` §4.2's `map_ptrs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, php_macros::EngineState)]
pub struct MapPtrRaw {
    pub offset: u32,
    pub slot: u32,
}

/// Where one slot of a held op array's runtime cache stands in one
/// request ([`touch`]): the engine's slot itself, which compiled code
/// reads in place past [`PLACE`](super::PLACE), standing there for as long
/// as the request runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachedSlot(std::ptr::NonNull<u8>);

impl CachedSlot {
    /// The slot's address, its provenance exposed, as compiled code is
    /// handed it.
    #[must_use]
    pub fn address(self) -> usize {
        self.0.as_ptr().expose_provenance()
    }
}

/// The touch (`jit-design.md` §4.6, #2957's note): where slot `raw` of
/// the runtime cache of the op array `epoch` holds at its offset stands
/// in `req`, where the request has laid that cache down. php lays a
/// function's cache at the first `INIT_*` that pushes its call in the
/// request (`init_func_run_time_cache`, `Z/Zend/zend_vm_def.h:4072`),
/// and so does the engine's `push_call`, so a
/// trace entered before its inlined callee's first call finds none, and
/// the touch lays nothing down: the slot is answered none, its opline is
/// its handler's until the call's `INIT_*` lays the cache, and a later
/// entry touches it again. A function's cache stands where it was laid
/// for as long as the request is served, so compiled code that was handed
/// the slot reads it for as long as it may. None also for a file's own
/// code, whose every activation lays a fresh cache, for an op array the
/// epoch holds no row for, and for a slot past its cache.
#[must_use]
pub fn touch(req: &Request, epoch: &Epoch, raw: MapPtrRaw) -> Option<CachedSlot> {
    let code = touched_code(epoch, raw)?;
    let at = crate::exec::CacheAt::of(code.runtime_cache);
    let slot = req.touch_cache(at, CacheSlot(raw.slot))?;
    Some(CachedSlot(slot.cast()))
}

/// Whether [`touch`] can ever answer slot `raw` in a request: not for a
/// file's own code, an op array the epoch holds no row for, or a slot past
/// its cache, which no call lays down.
#[must_use]
pub fn touchable(epoch: &Epoch, raw: MapPtrRaw) -> bool {
    touched_code(epoch, raw)
        .is_some_and(|code| crate::exec::CacheSize::of(code).holds(CacheSlot(raw.slot)))
}

/// The op array of a function's whose cache slot `raw` names, which the
/// touch reads.
fn touched_code(epoch: &Epoch, raw: MapPtrRaw) -> Option<&OpArray> {
    (epoch.0.func(raw.offset))
        .filter(|func| !func.is_main())
        .and_then(|func| func.op_array())
}

/// Where a property opline over a constant name finds the property on an
/// object of one published class, from one op array
/// ([`UnitView::hold_place`]): the declared slot, `OBJ_PROP_NUM`, and
/// whether the property carries a type, whose write, compound write and
/// step php's code generator serves through `zend_jit_assign_to_typed_prop`
/// and its kin and this tier through the handler, a `readonly` one and one
/// of a set visibility among them.
///
/// For a request-scoped class, whose id and slots are the request's, the
/// place is the one the recording's request found, by which the trace
/// decides how it serves the opline, and compiled code reads the place
/// of the request it runs in from that request's cache slot
/// ([`PropertySlot::cached`]). The class is named as the recording names
/// it ([`RecordClass`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PropertySlot {
    func: EngineFuncRef,
    at: OplineIdx,
    class: RecordClass,
    slot: u32,
    typed: bool,
    admits: u16,
    class_of: PlaceClass,
}

/// How compiled code finds a property's place on its object's class: by
/// a guard on a class every request of the epoch shares, whose place is
/// the one the view took in every request, or from the runtime-cache slot
/// of the request it runs in, for a request-scoped class. One of the two
/// is always the case, so a place served in place is either guarded on
/// its class or read where the class is the request's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceClass {
    /// A published class's place, under a guard on the class.
    Shared(EpochClassId),
    /// A request-scoped class's place, read from this cache slot.
    Cached(MapPtrRaw),
}

impl PropertySlot {
    /// How compiled code finds the place: under a guard on a shared class,
    /// or from the request's cache slot ([`PlaceClass`]).
    #[must_use]
    pub fn class_of(self) -> PlaceClass {
        self.class_of
    }

    /// The runtime-cache slot compiled code reads the place from, in the
    /// request it runs in, for a request-scoped class, as php's code reads
    /// `CACHED_PTR(cache_slot)` for a class that is not immutable
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:14341-14370`); none for a
    /// published class, whose place is this one in every request.
    #[must_use]
    pub fn cached(self) -> Option<MapPtrRaw> {
        match self.class_of {
            PlaceClass::Cached(raw) => Some(raw),
            PlaceClass::Shared(_) => None,
        }
    }

    /// The declared slot, by its number among the object's slots.
    #[must_use]
    pub fn slot(self) -> u32 {
        self.slot
    }

    /// Whether the property carries a type.
    #[must_use]
    pub fn typed(self) -> bool {
        self.typed
    }

    /// Whether a write of a scalar of `kind` lands in the slot as it
    /// stands, with nothing to verify or convert, as the handler's inline
    /// path writes it: any scalar of an untyped slot, one its type's mask
    /// holds of a typed one, none of a `readonly` one or one of a set
    /// visibility the scope lacks.
    #[must_use]
    pub fn admits(self, kind: ZvalType) -> bool {
        (self.admits >> (kind as u16 & 15)) & 1 != 0
    }
}

/// The class and the callee a method's call over constant names resolved
/// together, from one op array ([`UnitView::hold_callee`]): the pair the
/// handler wrote into the opline's runtime-cache slot,
/// `CACHE_POLYMORPHIC_PTR(opline->result.num, ce, fbc)`, a published
/// method's or a request-scoped class's own, which compiled code holds
/// the slot of the request it runs in
/// to before it pushes the call in place, as php's code reads
/// `CACHED_PTR(opline->result.num)` and the callee past it
/// (`zend_jit_init_method_call`, `zend_jit_init_static_method_call`,
/// `Z/ext/opcache/jit/zend_jit_ir.c:9025-9056`, `:9255-9265`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CalleeSlot {
    func: EngineFuncRef,
    at: OplineIdx,
    class: RecordClass,
    function: Option<EpochFunction>,
    instance: bool,
    cached: MapPtrRaw,
}

impl CalleeSlot {
    /// The class the pair was resolved on: the receiver's, or the one an
    /// `INIT_STATIC_METHOD_CALL` names, as the recording names it
    /// ([`RecordClass`]).
    #[must_use]
    pub fn class(self) -> RecordClass {
        self.class
    }

    /// The callee's number as the slot holds it, a published method's,
    /// the same in every request of the epoch
    /// ([`PlaceLayout::pair_function`](super::PlaceLayout::pair_function));
    /// none for a request-scoped class's own method, numbered by each
    /// request for itself, which compiled code holds the slot to by the
    /// callee's record ([`PlaceLayout::pair_record`](super::PlaceLayout::pair_record)).
    #[must_use]
    pub fn function(self) -> Option<EpochFunction> {
        self.function
    }

    /// Whether the callee takes `$this`, php's `ZEND_ACC_STATIC` clear.
    #[must_use]
    pub fn instance(self) -> bool {
        self.instance
    }

    /// The runtime-cache slot compiled code reads the pair from.
    #[must_use]
    pub fn cached(self) -> MapPtrRaw {
        self.cached
    }
}

pub use crate::arena::{EpochClassId, EpochFunction};

/// What a trace may read of the op arrays, literals and published
/// classes it touched (`jit-design.md` §4.1, §5.3): built by the recorder
/// from the request, it holds the request's epoch once and the ids it
/// accepted, and nothing else, so it is `Send` and outlives the request
/// that built it for as long as it stands. What it reads, it reads
/// through the epoch, which keeps every held unit and published class
/// until it is dropped.
///
/// It names a class as a recording does ([`RecordClass`]), never by a
/// request's number, and holds no declaration: what a trace's build reads
/// of one it reads through [`UnitView::class_facts`], copied by one
/// projection that keeps no value (#4653). So no reader of a view on the
/// worker reaches a value of the request's, and none is handed a
/// declaration,
///
/// ```compile_fail,E0599
/// fn declaration(
///     view: &php_engine::tier::UnitView,
///     class: php_engine::tier::RecordClass,
/// ) -> Option<&php_unit::decl::ClassDecl> {
///     view.class_decl(class)
/// }
/// ```
#[derive(Debug)]
pub struct UnitView {
    epoch: Epoch,
    funcs: Vec<EngineFuncRef>,
    /// The shared classes the view holds ([`UnitView::hold_class`]).
    classes: Vec<EpochClassId>,
    /// The plain calls the recording took ([`UnitView::hold_call`]), each
    /// beside its called scope as the recording names it, by which a
    /// trace's build finds the call of a receiver it sampled.
    calls: Vec<(Option<RecordClass>, super::PlainCall)>,
    /// The property places the recording took ([`UnitView::hold_place`]).
    places: Vec<PropertySlot>,
    /// The pairs of a method's call the recording took
    /// ([`UnitView::hold_callee`]).
    callees: Vec<CalleeSlot>,
    /// Whether an observer of calls was declared ([`UnitView::observed`]).
    observed: bool,
    /// What the natives of the frameless calls the recording served
    /// declare they return, by their row of the engine's frameless table
    /// ([`UnitView::hold_frameless`]).
    frameless: Vec<(u32, Option<Type>)>,
    /// `zend_flf_functions` by name, the engine's frameless table, taken
    /// when the view first holds an op array that calls through it.
    frameless_names: Vec<Name>,
    /// The classes php's analysis of each held op array binds by name
    /// ([`UnitView::class_named`]), bound as the view accepts it.
    named: Vec<(EngineFuncRef, Box<[u8]>, RecordClass)>,
    /// The engine's own classes the declared types of what the view
    /// holds name ([`UnitView::engine_class`]).
    engine_named: Vec<(Box<[u8]>, EpochClassId)>,
    /// The class each held op array is declared in ([`UnitView::scope`]).
    scopes: Vec<(EngineFuncRef, RecordClass)>,
    /// Each class bound for the analysis with its parent
    /// ([`UnitView::parent`]).
    related: Vec<(RecordClass, Option<RecordClass>)>,
    /// What the analysis reads of the declarations of the shared classes
    /// the view holds ([`UnitView::class_facts`]): each of the engine's
    /// own it binds, whose declaration the epoch does not keep, and each
    /// row of the epoch's, projected as an own class's is.
    shared_facts: Vec<(EpochClassId, ClassFacts)>,
    /// The request's own classes the view met, by [`OwnClass`]: each
    /// one's name, and, where the analysis binds it, its linked record and
    /// what the analysis reads of its declaration, as the request linked
    /// it, which the epoch does not hold ([`UnitView::record_class`]).
    own: Vec<OwnEntry>,
    /// The op arrays whose analysis binds one of `own`
    /// ([`UnitView::binds_request_class`]).
    request_bound: Vec<EngineFuncRef>,
    /// The functions the calls of each held op array reach, bound as the
    /// view accepts it ([`UnitView::called`]).
    called: Vec<(EngineFuncRef, Site, CalledFunction)>,
}

/// One of the request's own classes a view met, at its [`OwnClass`]: its
/// name, by which the view finds it again at record time, since one
/// request declares a name for one class alone; and, where
/// the analysis binds it, its linked record, its classes named as the
/// view names them, and what the analysis reads of its declaration.
#[derive(Debug)]
struct OwnEntry {
    name: Name,
    bound: Option<(LinkedClass<RecordClass>, ClassFacts)>,
}

/// What a view bound for php's analysis of one op array
/// ([`UnitView::bindings`]): the class the op array is declared in, the
/// classes its literals name, the engine's own classes declared types
/// name, and the functions its calls reach, each by its identity and
/// each callee's body by its kind ([`CalleeBody`]). php rebuilds the
/// analysis at every trace's build against the tables as they stand then
/// (`zend_jit_trace_build_ssa`); two builds that bind the same find the
/// same in them, so an analysis kept under one binding set is the one
/// php would build at the next build that binds it (ADR-0015, as amended
/// on #3571; #3579). The epoch keeps an analysis under these alone, so
/// each class is named as every request of the epoch names it
/// ([`EpochClassId`]), and no op array is named at all.
#[derive(Debug, Clone, PartialEq, Eq, php_macros::EngineState)]
pub struct Bindings {
    scope: Option<EpochClassId>,
    named: Vec<(Box<[u8]>, EpochClassId)>,
    engine_named: Vec<(Box<[u8]>, EpochClassId)>,
    called: Vec<(Site<EpochClassId>, CalledFunction<EpochClassId, CalleeBody>)>,
}

/// What the body of a function a call reaches is to the op array the
/// call is in, as [`Bindings`] names it (#4628): none, a native's; the
/// op array itself, a recursive call's; or another user function's. The
/// key names the kind and no body, so no [`FuncRef`] and no map-pointer
/// number, a request's above all, stands in it by type.
///
/// The kind is enough, and this is why. The key's one reader of a
/// callee's body is the tier's call analysis (`Bound::callee`), which
/// reads it as one of these three. On the third it reads what the other
/// function's analysis, earlier in the same build, found it returns, and
/// marks the analysis as one that read returns, which the epoch keeps
/// under no key. So an analysis the epoch keeps read no other body's
/// identity, and two builds whose bindings differ only in which other
/// user function a call reaches read the same analysis. The key still
/// tells a call bound from one left unbound, and a native from a user
/// function, as the ruling on #3589 keeps `called` for.
///
/// A change that reads a kept callee's body for anything else, or keeps
/// an analysis that read what another function returns, must name the
/// body in the key again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, php_macros::EngineState)]
pub enum CalleeBody {
    /// A native's: there is no op array.
    Native,
    /// The op array the call is in.
    Itself,
    /// Another user function's op array.
    Another,
}

/// Where [`UnitView::called`] looks a callee up, owned, a method's class
/// named as a `C`.
#[derive(Debug, Clone, PartialEq, Eq, php_macros::EngineState)]
enum Site<C = RecordClass> {
    Function(Box<[u8]>),
    Method(C, Box<[u8]>),
    Frameless(u32),
    ParentHook(Box<[u8]>, HookKind),
}

impl Site {
    /// The site as [`UnitView::called`] is asked for it.
    fn as_call_site(&self) -> CallSite<'_> {
        match self {
            Site::Function(name) => CallSite::Function(name),
            Site::Method(class, name) => CallSite::Method(*class, name),
            Site::Frameless(index) => CallSite::Frameless(*index),
            Site::ParentHook(name, kind) => CallSite::ParentHook(name, *kind),
        }
    }

    /// The site with its class named as every request of the epoch names
    /// it; none for a class of the request's own.
    fn shared(&self) -> Option<Site<EpochClassId>> {
        Some(match self {
            Site::Function(name) => Site::Function(name.clone()),
            Site::Method(class, name) => Site::Method(class.shared()?, name.clone()),
            Site::Frameless(index) => Site::Frameless(*index),
            Site::ParentHook(name, kind) => Site::ParentHook(name.clone(), *kind),
        })
    }
}

/// What php's analysis of an op array asks [`UnitView::called`] for: a
/// function by its lower-cased name, a method of a class by its
/// lower-cased name, the native of a row of the engine's frameless
/// table, or the hook of a kind the parent of the op array's class holds
/// for a property by its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallSite<'a> {
    Function(&'a [u8]),
    Method(RecordClass, &'a [u8]),
    Frameless(u32),
    ParentHook(&'a [u8], HookKind),
}

/// A function a call of a held op array reaches, as php's call analysis
/// reads it (`zend_optimizer_get_called_func`, `zend_get_func_info`):
/// its name, its declared return, its body, and a method's class and
/// flags. As the view answers it, a method's class is named as the
/// recording names it ([`RecordClass`]) and the body is what a user
/// function's call serves ([`CalledBody`]); among [`Bindings`], the class
/// is named as every request of the epoch names it, a `C`, and the body
/// by its kind, a `B` ([`CalleeBody`]).
#[derive(Debug, Clone, PartialEq, Eq, php_macros::EngineState)]
pub struct CalledFunction<C = RecordClass, B = CalledBody> {
    /// `common.function_name`, as declared.
    pub name: Name,
    pub return_type: Option<Type>,
    pub returns_by_ref: bool,
    pub generator: bool,
    /// What a user function's call serves, a native's none; its kind
    /// among [`Bindings`].
    pub body: B,
    /// A method's class and flags; none for a function.
    pub method: Option<CalledMethod<C>>,
}

/// The body of a function a call reaches, as a view holds it: none, a
/// native's; an op array the engine numbered, by its [`EngineFuncRef`];
/// or an op array of the request's own, a method an `eval`'d parent
/// declares, which no number names past its request, as a plain call's
/// is named by none (`Serves::Request`; design §5.14, *Identities*).
#[derive(Debug, Clone, Copy, PartialEq, Eq, php_macros::EngineState)]
pub enum CalledBody {
    /// A native's: there is no op array.
    Native,
    /// An op array the engine numbered.
    Engine(EngineFuncRef),
    /// An op array of the request's own.
    Request,
}

impl CalledFunction {
    /// What this function's body is to the op array `f` names, whose call
    /// reaches it: the one place the kind is decided, for the key and for
    /// the analysis that reads it.
    #[must_use]
    pub fn body_from(&self, f: EngineFuncRef) -> CalleeBody {
        match self.body {
            CalledBody::Native => CalleeBody::Native,
            CalledBody::Engine(g) if g == f => CalleeBody::Itself,
            CalledBody::Engine(_) | CalledBody::Request => CalleeBody::Another,
        }
    }

    /// The function as [`Bindings`] names it for a call of the op array
    /// `f` names: its method's class as every request of the epoch names
    /// it, none for a class of the request's own, and its body by its
    /// kind.
    fn shared(&self, f: EngineFuncRef) -> Option<CalledFunction<EpochClassId, CalleeBody>> {
        let method = match self.method {
            Some(m) => Some(CalledMethod {
                scope: m.scope.shared()?,
                visibility: m.visibility,
                is_final: m.is_final,
                is_abstract: m.is_abstract,
                tentative_return: m.tentative_return,
            }),
            None => None,
        };
        Some(CalledFunction {
            name: self.name.clone(),
            return_type: self.return_type.clone(),
            returns_by_ref: self.returns_by_ref,
            generator: self.generator,
            body: self.body_from(f),
            method,
        })
    }
}

/// A method's `common.scope` and the `fn_flags` php's call analysis
/// reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, php_macros::EngineState)]
#[allow(clippy::struct_excessive_bools)]
pub struct CalledMethod<C = RecordClass> {
    /// The class that declares it.
    pub scope: C,
    pub visibility: Visibility,
    pub is_final: bool,
    pub is_abstract: bool,
    /// The tentative bit of a native's declared return type.
    pub tentative_return: bool,
}

/// What [`UnitView::declaration`] reads of an op array: the declaration
/// it is the body of, none for a file's own code, and the name of the
/// class a method or a property hook is declared in.
#[derive(Debug, Clone, Copy)]
pub struct Declaration<'a> {
    pub function: Option<&'a FunctionDecl>,
    pub scope: Option<&'a Name>,
}

/// What [`CalledFunction`] holds of `func`, a function the request links.
fn called(
    req: &Request,
    func: crate::api::call::Function,
    method: Option<CalledMethod>,
) -> Option<CalledFunction> {
    let decl = req.symbols().function_decl(func)?;
    let body = match req.user_function(func) {
        Some(user) => {
            let code = crate::exec::body_of(&user.unit, user.body)?;
            EngineFuncRef::of(FuncRef(code.runtime_cache))
                .map_or(CalledBody::Request, CalledBody::Engine)
        }
        None => CalledBody::Native,
    };
    Some(CalledFunction {
        name: decl.name.clone(),
        return_type: decl.return_type.clone(),
        returns_by_ref: decl.returns_by_ref,
        generator: decl.is_generator,
        body,
        method,
    })
}

/// A view and an epoch hold leave the request's thread with a claim and
/// a compile job (`jit-design.md` §4.7, §4.8): held here, where they are
/// defined, so a field that is not `Send` fails the build.
const _: () = {
    const fn sendable<T: Send>() {}
    const fn shared<T: Send + Sync>() {}
    sendable::<UnitView>();
    shared::<Epoch>();
};

impl UnitView {
    /// An empty view holding `req`'s epoch.
    #[must_use]
    pub fn new(req: &Request) -> UnitView {
        UnitView {
            epoch: epoch(req),
            funcs: Vec::new(),
            classes: Vec::new(),
            calls: Vec::new(),
            places: Vec::new(),
            callees: Vec::new(),
            observed: crate::observer::observing(req),
            frameless: Vec::new(),
            frameless_names: Vec::new(),
            named: Vec::new(),
            engine_named: Vec::new(),
            scopes: Vec::new(),
            related: Vec::new(),
            shared_facts: Vec::new(),
            own: Vec::new(),
            request_bound: Vec::new(),
            called: Vec::new(),
        }
    }

    /// Accept the op array `f` names where the view's epoch holds its
    /// unit, holding nothing, with the [`EngineFuncRef`] that names it in
    /// what the view holds; none for a request-scoped one, which a trace
    /// may not read past the request, and for a request of another epoch.
    pub fn hold(&mut self, req: &Request, f: impl Into<FuncRef>) -> Option<EngineFuncRef> {
        let f = EngineFuncRef::of(f.into())?;
        if !self.epoch.of(req) || self.epoch.row(f).is_none() {
            return None;
        }
        if !self.funcs.contains(&f) {
            self.funcs.push(f);
            let frameless = (self.epoch.row(f).and_then(|func| func.op_array()))
                .is_some_and(|body| body.oplines.iter().any(|op| op.opcode.is_frameless_icall()));
            if frameless && self.frameless_names.is_empty() {
                self.frameless_names = req.shared.frameless_names();
            }
            self.bind_names(req, f);
        }
        Some(f)
    }

    /// Bind what php's analysis of the op array `f` names may look up
    /// when a trace's build analyses it (`zend_jit_trace_build_ssa`, its
    /// `script` null): each string constant that names a class the
    /// request's table holds, the engine's own or one declared in the op array's
    /// file (`zend_optimizer_get_class_entry`, `zend_optimizer_ignore_class`);
    /// the class it is declared in; and each of the engine's classes a
    /// declared type names, which bind whatever the file
    /// (`zend_convert_type`). Each bound class is held with its parents,
    /// so the analysis reads them on a thread with no request. A class
    /// only the request holds binds as it stands in the request, as
    /// php's `op_array->scope` and `EG(class_table)` name the request's, and
    /// marks the op array's analysis as the request's
    /// ([`UnitView::binds_request_class`]).
    fn bind_names(&mut self, req: &Request, f: EngineFuncRef) {
        let epoch = self.epoch.clone();
        let Some(func) = epoch.row(f) else {
            return;
        };
        let Some(body) = func.op_array() else {
            return;
        };
        let (decl, scope) = func.declaration().unwrap_or_default();
        let lookup = |name: &[u8]| {
            (!name.is_empty() && !name.starts_with(b"\\"))
                .then(|| req.class_named(Bare::resolved(name)))?
        };
        // Each class bound beside the request's number for it, which the
        // calls' lookup asks the request by while the view is built.
        let mut bound: Vec<(ClassId, RecordClass)> = Vec::new();
        let scope = scope.and_then(|name| lookup(&name.0));
        if let Some(scope) = scope
            && let Some(class) = self.relate(req, scope)
        {
            self.scopes.push((f, class));
            self.mark_own(f, class);
            bound.push((scope, class));
        }
        for name in body
            .literals
            .iter()
            .filter_map(|literal| literal.str_bytes())
        {
            let Some(found) = lookup(name) else {
                continue;
            };
            let own = req.native_class_at(found).is_some()
                || req
                    .symbols()
                    .class_decl(found)
                    .is_some_and(|d| d.file.0 == body.filename.0);
            if own && let Some(class) = self.relate(req, found) {
                self.named
                    .push((f, name.to_ascii_lowercase().into_boxed_slice(), class));
                self.mark_own(f, class);
                bound.push((found, class));
            }
        }
        let mut types = Vec::new();
        if let Some(decl) = decl {
            types.extend(decl.parameters.iter().filter_map(|p| p.ty.as_ref()));
            types.extend(decl.return_type.as_ref());
        }
        self.bind_engine_types(req, &types);
        for name in [&b"closure"[..], b"traversable"] {
            self.bind_engine_type(req, name);
        }
        let scope = scope.filter(|_| self.scope(f).is_some());
        self.bind_calls(req, (f, body), &bound, scope);
    }

    /// Bind the functions the calls of `body`, the op array `f` names,
    /// reach, as php's call analysis finds them when it builds the
    /// analysis ([`UnitView::called`]): an `INIT_FCALL`'s function, a
    /// native or one declared in the same file
    /// (`zend_optimizer_ignore_function`); the method a constant name
    /// names of the op array's class or of a class it names, `bound`,
    /// each beside the request's number for it; and a frameless call's
    /// native. `scope` is the request's number for the op array's class,
    /// where the view holds it.
    fn bind_calls(
        &mut self,
        req: &Request,
        (f, body): (EngineFuncRef, &OpArray),
        bound: &[(ClassId, RecordClass)],
        scope: Option<ClassId>,
    ) {
        let literal = |n: u32| body.literals.get(n as usize).and_then(|z| z.str_bytes());
        let mut sites = Vec::new();
        for op in &body.oplines {
            match op.opcode {
                Opcode::InitFcall => {
                    sites.extend(literal(op.op2).map(|n| (None, Site::Function(n.into()))));
                }
                Opcode::InitMethodCall | Opcode::InitStaticMethodCall
                    if op.op2_type == OperandKind::Const
                        && (op.opcode == Opcode::InitStaticMethodCall
                            || op.op1_type == OperandKind::Unused) =>
                {
                    // php finds the method of an `INIT_METHOD_CALL` on
                    // `$this` alone (`zend_optimizer_get_called_func`).
                    let Some(name) = literal(op.op2 + 1) else {
                        continue;
                    };
                    sites.extend(
                        bound
                            .iter()
                            .map(|&c| (Some(c.0), Site::Method(c.1, name.into()))),
                    );
                }
                Opcode::InitParentPropertyHookCall => {
                    let kind = if op.op2 == 0 {
                        HookKind::Get
                    } else {
                        HookKind::Set
                    };
                    sites.extend(literal(op.op1).map(|n| (None, Site::ParentHook(n.into(), kind))));
                }
                _ if op.opcode.is_frameless_icall() => {
                    sites.push((None, Site::Frameless(op.extended_value)));
                }
                _ => {}
            }
        }
        for (class, site) in sites {
            if self
                .called
                .iter()
                .any(|(func, held, _)| *func == f && *held == site)
            {
                continue;
            }
            let found = match (&site, class) {
                (Site::Function(name), _) => {
                    req.function_named(Bare::resolved(name)).and_then(|func| {
                        let user = req.user_function(func);
                        let elsewhere = user.is_some_and(|user| {
                            crate::exec::body_of(&user.unit, user.body)
                                .is_none_or(|code| code.filename != body.filename)
                        });
                        (!elsewhere).then(|| called(req, func, None)).flatten()
                    })
                }
                (Site::Method(..), None) => None,
                (Site::Method(_, name), Some(class)) => {
                    req.symbols().method_named(class, name).and_then(|func| {
                        let decl = req.method_decl(func)?;
                        let method = CalledMethod {
                            scope: self.record_class(req, req.method_class(func)?),
                            visibility: decl.visibility,
                            is_final: decl.is_final,
                            is_abstract: decl.is_abstract,
                            tentative_return: decl.tentative_return,
                        };
                        called(req, func, Some(method))
                    })
                }
                (Site::Frameless(index), _) => req
                    .frameless_entry(*index)
                    .and_then(|(func, _)| called(req, func, None)),
                // `zend_get_property_info` on the parent of the op array's
                // class, a property it may read that is not private, and
                // its hook of that kind.
                (Site::ParentHook(name, kind), _) => {
                    let parent = scope.and_then(|c| req.symbols().class_parent(c));
                    parent.and_then(|parent| {
                        let (_, property) = req.property_info_of(parent, name)?;
                        if property.visibility == Visibility::Private {
                            return None;
                        }
                        let hooked = req.row_hooked(parent, name)?;
                        let func = match kind {
                            HookKind::Get => hooked.get,
                            HookKind::Set => hooked.set,
                        }?;
                        called(req, func, None)
                    })
                }
            };
            if let Some(found) = found {
                self.called.push((f, site, found));
            }
        }
    }

    /// Bind the engine's own classes `types` name ([`UnitView::bind_names`]).
    fn bind_engine_types(&mut self, req: &Request, types: &[&Type]) {
        let mut names = Vec::new();
        let mut walk: Vec<&Type> = types.to_vec();
        while let Some(ty) = walk.pop() {
            match ty {
                Type::Named(name) => names.push(&name.0),
                Type::Nullable(inner) => walk.push(inner),
                Type::Union(members) | Type::Intersection(members) => walk.extend(members),
                _ => {}
            }
        }
        for name in names {
            self.bind_engine_type(req, name);
        }
    }

    /// Bind the engine's own class `name` ([`UnitView::bind_names`]).
    fn bind_engine_type(&mut self, req: &Request, name: &[u8]) {
        let lcname = name.to_ascii_lowercase();
        if self.engine_named.iter().any(|(held, _)| **held == *lcname) || name.starts_with(b"\\") {
            return;
        }
        if let Some(class) = req.class_named(Bare::resolved(name))
            && req.native_class_at(class).is_some()
            && let Some(RecordClass::Shared(class)) = self.relate(req, class)
        {
            self.engine_named.push((lcname.into_boxed_slice(), class));
        }
    }

    /// Hold class `c` for the analysis with its parents, what the
    /// analysis reads of the engine's declaration of it where it is one of
    /// the engine's own, and the engine's classes its properties' and
    /// constants' types name, with the class as the recording names it;
    /// none where the request has not linked it ([`UnitView::hold_class`],
    /// [`UnitView::bind_own`]).
    fn relate(&mut self, req: &Request, c: ClassId) -> Option<RecordClass> {
        let class = self.record_class(req, c);
        if self.related.iter().any(|(held, _)| *held == class) {
            return Some(class);
        }
        let held = match class {
            RecordClass::Shared(_) => self.hold_class(req, c),
            RecordClass::Own(own) => self.bind_own(req, c, own),
        };
        if !held {
            return None;
        }
        let at = self.related.len();
        self.related.push((class, None));
        let Some(decl) = req.symbols().class_decl(c) else {
            return Some(class);
        };
        if let RecordClass::Shared(shared) = class
            && req.native_class_at(c).is_some()
            && !self.shared_facts.iter().any(|(held, _)| *held == shared)
        {
            self.shared_facts.push((shared, ClassFacts::of(decl)));
        }
        let types: Vec<&Type> = (decl.properties.iter().filter_map(|p| p.ty.as_ref()))
            .chain(decl.constants.iter().filter_map(|c| c.ty.as_ref()))
            .collect();
        self.bind_engine_types(req, &types);
        if let Some(parent) = req.symbols().class_parent(c)
            && let Some(parent) = self.relate(req, parent)
            && let Some((_, held)) = self.related.get_mut(at)
        {
            *held = Some(parent);
        }
        Some(class)
    }

    /// Bind the request's own class `c`, which the view names `own`, for
    /// the analysis as the request linked it: its linked record, its
    /// classes named as the view names them, and what the analysis reads
    /// of its declaration ([`ClassFacts::of`]), copied, since no other
    /// request reads them; `false` for a request of another epoch and a
    /// class the request has not linked.
    fn bind_own(&mut self, req: &Request, c: ClassId, own: OwnClass) -> bool {
        if !self.epoch.of(req) {
            return false;
        }
        if self
            .own
            .get(own.position())
            .is_some_and(|entry| entry.bound.is_some())
        {
            return true;
        }
        let (Some(linked), Some(decl)) = (req.linked_class(c), req.symbols().class_decl(c)) else {
            return false;
        };
        let Some(linked) = linked.try_map(|class| Some(self.record_class(req, class))) else {
            return false;
        };
        let facts = ClassFacts::of(decl);
        match self.own.get_mut(own.position()) {
            Some(entry) => {
                entry.bound = Some((linked, facts));
                true
            }
            None => false,
        }
    }

    /// Mark the analysis of `f` as the request's where `class`, a class
    /// it binds, is the request's own.
    fn mark_own(&mut self, f: EngineFuncRef, class: RecordClass) {
        if matches!(class, RecordClass::Own(_)) && !self.request_bound.contains(&f) {
            self.request_bound.push(f);
        }
    }

    /// Whether the analysis of the op array `f` names binds a class only
    /// the request holds, its declaring class or one it names: what it
    /// learns is this request's, so it is built anew at each trace's
    /// build, as php builds every one, and kept for no other.
    #[must_use]
    pub fn binds_request_class(&self, f: EngineFuncRef) -> bool {
        self.request_bound.contains(&f)
    }

    /// What the view bound for php's analysis of the op array `f` names,
    /// the key the epoch keeps its analysis under: an analysis built
    /// under one [`Bindings`] is read only where a later build's view
    /// binds the same, each callee's body named by what it is to `f`'s
    /// op array ([`CalledFunction::body_from`]). None where the analysis
    /// binds a class only the request holds
    /// ([`UnitView::binds_request_class`]), or names one the view minted
    /// no [`EpochClassId`] for, which the epoch keeps no analysis under.
    #[must_use]
    pub fn bindings(&self, f: EngineFuncRef) -> Option<Bindings> {
        if self.binds_request_class(f) {
            return None;
        }
        let shared = |name: &[u8], class: RecordClass| Some((name.into(), class.shared()?));
        let mut engine_named: Vec<(Box<[u8]>, EpochClassId)> = (self.engine_named.iter())
            .map(|(name, class)| (name.clone(), *class))
            .collect();
        // Each name is bound once, so the names alone order them.
        engine_named.sort_unstable_by(|(a, _), (b, _): &(Box<[u8]>, _)| a.cmp(b));
        Some(Bindings {
            scope: match self.scope(f) {
                Some(class) => Some(class.shared()?),
                None => None,
            },
            named: (self.named.iter())
                .filter(|(func, ..)| *func == f)
                .map(|(_, name, class)| shared(name, *class))
                .collect::<Option<_>>()?,
            engine_named,
            called: (self.called.iter())
                .filter(|(func, ..)| *func == f)
                .map(|(_, site, called)| Some((site.shared()?, called.shared(f)?)))
                .collect::<Option<_>>()?,
        })
    }

    /// The function the call `site` of the op array `f` names reaches, as
    /// the view bound it when it accepted the op array; none where php's
    /// call analysis finds none.
    #[must_use]
    pub fn called(&self, f: EngineFuncRef, site: CallSite<'_>) -> Option<&CalledFunction> {
        (self.called.iter())
            .find(|(func, held, _)| *func == f && held.as_call_site() == site)
            .map(|(.., called)| called)
    }

    /// `zend_flf_functions` by name, as the dump of an op array the view
    /// holds names the native a `FRAMELESS_ICALL_*` calls; empty until
    /// [`UnitView::hold`] accepts an op array with such a call.
    #[must_use]
    pub fn frameless_names(&self) -> &[Name] {
        &self.frameless_names
    }

    /// Accept class `c` where every request shares it, the engine's own
    /// or one the epoch published (`ClassId::is_published`), holding
    /// nothing; `false` for a request-local one, which the trace reads
    /// through the request's cache slot instead.
    pub fn hold_class(&mut self, req: &Request, c: ClassId) -> bool {
        self.held_class(req, c).is_some()
    }

    /// [`UnitView::hold_class`], with the identity the arena minted for
    /// `c`. A row of the epoch's has what the analysis reads of its
    /// declaration projected once, as an own class's is
    /// ([`ClassFacts::of`]), for [`UnitView::class_facts`].
    fn held_class(&mut self, req: &Request, c: ClassId) -> Option<EpochClassId> {
        let shared = self.shared(req, c)?;
        if !self.classes.contains(&shared) {
            self.classes.push(shared);
            if let Some(decl) = self.epoch.0.class(c).and_then(|row| row.decl())
                && !self.shared_facts.iter().any(|(held, _)| *held == shared)
            {
                self.shared_facts.push((shared, ClassFacts::of(decl)));
            }
        }
        Some(shared)
    }

    /// The identity every request of the view's epoch names class `c` by,
    /// the check [`UnitView::hold_class`] makes: where `req` started in
    /// the view's epoch, the epoch's arena's one check
    /// ([`Arena::shared_class`]), which mints the identity where `c` is
    /// one of the engine's own or a row the epoch holds. None for a
    /// request-local class and a request of another epoch; the view mints
    /// none of its own.
    pub(super) fn shared(&self, req: &Request, c: ClassId) -> Option<EpochClassId> {
        if !self.epoch.of(req) {
            return None;
        }
        self.epoch.0.shared_class(c)
    }

    /// The identity every request of the view's epoch calls `function`
    /// by, beside [`UnitView::shared`]'s for a class: where `req` started
    /// in the view's epoch, the epoch's arena's one check of a function
    /// ([`Arena::shared_function`]), which mints the identity where the
    /// function is a native or a native method, or a user function or
    /// method whose row the epoch holds. None for a request's own; the
    /// view mints none of its own.
    pub(super) fn shared_function(
        &self,
        req: &Request,
        function: crate::api::call::Function,
    ) -> Option<EpochFunction> {
        if !self.epoch.of(req) {
            return None;
        }
        self.epoch.0.shared_function(function)
    }

    /// Class `c`, the recording request's number for it, as a recording
    /// names it (design §5.14, *Identities*; #4653): by the identity
    /// every request of the view's epoch names it by where the arena's one
    /// test finds it shared ([`UnitView::shared`]), and otherwise by the
    /// index the view gives a class of the request's own, the one it gave
    /// the class before where it met it already, found by its name, which
    /// the request declares for one class alone. Total, so a
    /// recording names every object's class it samples, and holding
    /// nothing of the request: an own class's entry holds its name until
    /// the analysis binds it ([`UnitView::bind_own`]).
    pub fn record_class(&mut self, req: &Request, c: ClassId) -> RecordClass {
        if let Some(shared) = self.shared(req, c) {
            return RecordClass::Shared(shared);
        }
        let name = req.symbols().class_decl(c).map(|decl| &decl.name);
        let met = name.and_then(|name| {
            (self.own.iter()).position(|own| own.name.0.eq_ignore_ascii_case(&name.0))
        });
        if let Some(at) = met {
            return RecordClass::Own(OwnClass::of(at));
        }
        // A class with no declaration is met as a new one each time,
        // which names no two classes as one.
        let at = self.own.len();
        let name = name.cloned().unwrap_or_default();
        self.own.push(OwnEntry { name, bound: None });
        RecordClass::Own(OwnClass::of(at))
    }

    /// The identity every request of the view's epoch names the class
    /// numbered `number` by, as a recorded trace's text form writes a
    /// shared class (`jit-design.md` §4.1, `TraceRecord::parse`): the
    /// epoch's arena's one test ([`Arena::shared_class`]), which mints it
    /// where the number names one of the engine's own or a row the epoch
    /// holds; none for any other number, a request's own class's among
    /// them, so a text read back through the view names no class its
    /// epoch does not share.
    #[must_use]
    pub fn shared_numbered(&self, number: u32) -> Option<EpochClassId> {
        self.epoch.0.shared_class(ClassId(number))
    }

    /// Take the place the property opline at `at` of the op array `frame`
    /// serves names on an object of `class`, a declared slot, as the
    /// engine's runtime cache holds it after a pass of the handler
    /// ([`Request::cached_property`]), so a trace that recorded the opline
    /// reads and writes the slot in place under a guard on the object's
    /// class, as php's code generator does for a property its
    /// `zend_get_known_property_info` finds on an immutable class
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:14313-14340`). Taken only where
    /// every frame that runs the op array finds the place one way: the
    /// frame reads its op array's own cache, so it is no closure's and no
    /// trait method's copy's, and the op array runs in one scope
    /// ([`super::published::Func::fixes_scope`]), which is what the
    /// placement asks of the frame. A published class's id names one
    /// linked, immutable class in every request of the epoch, whose slots
    /// and their types are that record's; a request-scoped one, a class a
    /// request linked over one of its own (`zend_lazy_class_load`'s copy),
    /// every enum and an evaluated class, has an id and slots of the
    /// request's, so its place is read where compiled code runs from that
    /// request's slot ([`PropertySlot::cached`]), as php's code reads it
    /// for a class that is not immutable (`:14341-14370`). `false`, taking
    /// nothing, for any other, and where the cache names no declared slot.
    pub fn hold_place(
        &mut self,
        req: &Request,
        frame: FrameRef,
        at: OplineIdx,
        class: ClassId,
    ) -> bool {
        let Some((f, code)) = scoped_code(req, frame) else {
            return false;
        };
        let Some(op) = code.oplines.get(at.0 as usize) else {
            return false;
        };
        // `CACHE_ADDR(opline->extended_value & ~ZEND_FETCH_OBJ_FLAGS)`, an
        // `ASSIGN_OBJ_OP`'s its `OP_DATA`'s: a `_W` fetch carries php's two
        // fetch flags beside the slot, and the rest none.
        let flags = crate::handlers::FETCH_REF | crate::handlers::FETCH_DIM_WRITE;
        let slot = match op.opcode {
            Opcode::FetchObjW => CacheSlot(op.extended_value & !flags),
            Opcode::FetchObjR
            | Opcode::AssignObj
            | Opcode::PreIncObj
            | Opcode::PreDecObj
            | Opcode::PostIncObj
            | Opcode::PostDecObj => CacheSlot(op.extended_value),
            Opcode::AssignObjOp => match code.oplines.get(at.0 as usize + 1) {
                Some(data) => CacheSlot(data.extended_value),
                None => return false,
            },
            _ => return false,
        };
        if op.op2_type != OperandKind::Const || self.hold(req, f).is_none() {
            return false;
        }
        // A request-scoped class's place is read from the cache slot of
        // the request compiled code runs in.
        let class_of = if class.is_published() {
            match self.held_class(req, class) {
                Some(shared) => PlaceClass::Shared(shared),
                None => return false,
            }
        } else {
            match self.cache_slot(f, slot) {
                Some(raw) => PlaceClass::Cached(raw),
                None => return false,
            }
        };
        let Some(crate::exec::PropertyOffset::Declared { slot, ty }) =
            req.cached_property(frame, slot, class)
        else {
            return false;
        };
        // The scalars a write lands as they stand, by the slot type's own
        // test ([`crate::exec::SlotType::admits`]): none of a guarded
        // slot, `readonly` or of a set visibility the scope lacks.
        let scalars = [
            php_heap::Zval::null(),
            php_heap::Zval::bool(false),
            php_heap::Zval::bool(true),
            php_heap::Zval::long(0),
            php_heap::Zval::double(0.0),
        ];
        let admits = scalars
            .into_iter()
            .filter(|z| ty.admits(*z))
            .fold(0u16, |kinds, z| kinds | 1 << (z.kind() as u16));
        let place = PropertySlot {
            func: f,
            at,
            class: self.record_class(req, class),
            slot,
            typed: ty.typed(),
            admits,
            class_of,
        };
        if !self.places.contains(&place) {
            self.places.push(place);
        }
        true
    }

    /// Take the class and the callee the `INIT_METHOD_CALL` over a
    /// constant name, or the `INIT_STATIC_METHOD_CALL` over a constant
    /// name and a class a constant, `self` or `parent` names, at `at` of the op array `frame` serves wrote into
    /// its runtime-cache slot, as the handler wrote it on this pass
    /// ([`Request::cached_callee`]), so a trace that recorded the call
    /// pushes it in place where the slot of the request it runs in holds
    /// the same pair. Taken only where every frame that runs the op array
    /// reads the pair one way, as [`UnitView::hold_place`] takes a place;
    /// `false`, taking nothing, for any other, and where the slot holds no
    /// pair.
    pub fn hold_callee(&mut self, req: &Request, frame: FrameRef, at: OplineIdx) -> bool {
        let Some((f, code)) = scoped_code(req, frame) else {
            return false;
        };
        let Some(op) = code.oplines.get(at.0 as usize) else {
            return false;
        };
        let named = match op.opcode {
            Opcode::InitMethodCall => op.op2_type == OperandKind::Const,
            Opcode::InitStaticMethodCall => {
                op.op2_type == OperandKind::Const && crate::exec::class_fixed(op.op1_type, op.op1)
            }
            _ => false,
        };
        let slot = CacheSlot(op.out);
        if !named || self.hold(req, f).is_none() {
            return false;
        }
        let Some(cached) = self.cache_slot(f, slot) else {
            return false;
        };
        let crate::exec::Cached::ClassAndCallee {
            class,
            function,
            instance,
            ..
        } = req.cached_in_scope(frame, slot)
        else {
            return false;
        };
        let pair = CalleeSlot {
            func: f,
            at,
            class: self.record_class(req, class),
            function: self.shared_function(req, function),
            instance,
            cached,
        };
        self.callees.retain(|held| (held.func, held.at) != (f, at));
        self.callees.push(pair);
        true
    }

    /// The pair the call at `at` of the op array `f` resolved; none
    /// unless [`UnitView::hold_callee`] took it.
    #[must_use]
    pub fn callee_at(&self, f: EngineFuncRef, at: OplineIdx) -> Option<CalleeSlot> {
        (self.callees.iter())
            .find(|pair| pair.func == f && pair.at == at)
            .copied()
    }

    /// The runtime-cache slot compiled code reads what the fetch `op` of
    /// the op array `f` fetches from, as php's code reads
    /// `CACHED_PTR(opline->extended_value)`: a `FETCH_CONSTANT`'s, and
    /// a `FETCH_STATIC_PROP_R`'s over a name and a class php's trace
    /// compiles it over (`Z/ext/opcache/jit/zend_jit_trace.c:6146-6152`) in
    /// an op array every frame runs in one scope, whose slot answers each
    /// frame alike ([`super::published::Func::fixes_scope`]); none for any
    /// other opline and for a slot past its cache.
    #[must_use]
    pub fn fetched_slot(&self, f: EngineFuncRef, op: &Opline) -> Option<MapPtrRaw> {
        match op.opcode {
            Opcode::FetchConstant => self.cache_slot(f, CacheSlot(op.extended_value)),
            Opcode::FetchStaticPropR
                if op.op1_type == OperandKind::Const
                    && crate::exec::class_fixed(op.op2_type, op.op2)
                    && self
                        .epoch
                        .row(f)
                        .is_some_and(super::published::Func::fixes_scope) =>
            {
                self.cache_slot(f, crate::handlers::fetched::static_slot(op))
            }
            _ => None,
        }
    }

    /// The runtime-cache slot compiled code reads the callee of the
    /// function's call by its name `op` from, as php's code reads
    /// `CACHED_PTR(opline->result.num)` (`zend_jit_init_fcall`,
    /// `Z/ext/opcache/jit/zend_jit_ir.c:8860-8880`); none for any other
    /// opline and for a slot past its cache.
    #[must_use]
    pub fn named_slot(&self, f: EngineFuncRef, op: &Opline) -> Option<MapPtrRaw> {
        matches!(
            op.opcode,
            Opcode::InitFcallByName | Opcode::InitNsFcallByName
        )
        .then(|| self.cache_slot(f, CacheSlot(op.out)))
        .flatten()
    }

    /// The declared slot the property opline at `at` of the op array `f`
    /// names on an object of `class`; none unless
    /// [`UnitView::hold_place`] took it.
    #[must_use]
    pub fn place(
        &self,
        f: EngineFuncRef,
        at: OplineIdx,
        class: RecordClass,
    ) -> Option<PropertySlot> {
        (self.places.iter())
            .find(|place| place.func == f && place.at == at && place.class == class)
            .copied()
    }

    /// Whether an observer of calls was declared for the engine, which
    /// compiled code tells of every call and return its trace makes, so
    /// that it leaves no frame in place (design §4.3 seam 9).
    #[must_use]
    pub fn observed(&self) -> bool {
        self.observed
    }

    /// Take the call being built at `call` where it is a plain call
    /// ([`super::plain_call`]) to an op array the view holds, or to a
    /// request-scoped class's own method whose op array is the request's,
    /// which the view never holds and the call names by its class's name
    /// and its own (#3561, #3570), so a trace that recorded it writes it
    /// in place; `false`, taking nothing, for any other.
    pub fn hold_call(&mut self, req: &Request, call: FrameRef) -> bool {
        let Some((scope, plain)) = super::layout::scoped_call(self, req, call) else {
            return false;
        };
        let held = match plain.callee() {
            Some(callee) => self.hold(req, callee).is_some(),
            None => plain.is_native() || plain.own_class().is_some(),
        };
        if !held {
            return false;
        }
        let call = (scope.map(|scope| self.record_class(req, scope)), plain);
        if !self.calls.contains(&call) {
            self.calls.push(call);
        }
        true
    }

    /// Take what the native of row `index` of the engine's frameless
    /// table, the one a `FRAMELESS_ICALL_*` whose `extended_value` is
    /// `index` calls (`ZEND_FLF_FUNC`), declares it returns: none where it
    /// declares nothing or returns by reference. php's trace types what
    /// such a call writes by it, through the call map it builds for every
    /// op array a trace runs through, a file's own code among them
    /// (`zend_jit_trace_build_ssa`, `Z/ext/opcache/jit/zend_jit_trace.c:555-557`;
    /// `zend_get_func_info`, `Z/Zend/Optimizer/zend_func_info.c:132-150`),
    /// as it types a `DO_ICALL`'s by the function its record names.
    pub fn hold_frameless(&mut self, req: &Request, index: u32) {
        if self.frameless.iter().any(|(held, _)| *held == index) {
            return;
        }
        let returns = req
            .frameless_entry(index)
            .and_then(|(function, _)| req.symbols().function_decl(function))
            .filter(|decl| !decl.returns_by_ref)
            .and_then(|decl| decl.return_type.clone());
        self.frameless.push((index, returns));
    }

    /// What the native of row `index` of the engine's frameless table
    /// declares it returns; none unless [`UnitView::hold_frameless`] took
    /// it, where it declares nothing, and where it returns by reference.
    #[must_use]
    pub fn frameless_returns(&self, index: u32) -> Option<&Type> {
        (self.frameless.iter())
            .find(|(held, _)| *held == index)
            .and_then(|(_, returns)| returns.as_ref())
    }

    /// The plain calls to the op array `f` names with `count` arguments
    /// [`UnitView::hold_call`] took: a function's, and a method's of each
    /// called scope ([`super::PlainCall::is_method`]).
    pub fn calls_of(
        &self,
        f: EngineFuncRef,
        count: u32,
    ) -> impl Iterator<Item = &super::PlainCall> {
        (self.calls.iter())
            .map(|(_, call)| call)
            .filter(move |call| {
                call.callee() == Some(f) && call.count() == count && !call.is_closure()
            })
    }

    /// The plain call to the op array `f` names with `count` arguments
    /// [`UnitView::hold_call`] took of a method over a receiver of
    /// `class`, its called scope as the recording names it; none unless
    /// it took one.
    #[must_use]
    pub fn method_call(
        &self,
        f: EngineFuncRef,
        count: u32,
        class: RecordClass,
    ) -> Option<&super::PlainCall> {
        (self.calls.iter())
            .find(|(scope, call)| {
                *scope == Some(class)
                    && call.callee() == Some(f)
                    && call.count() == count
                    && !call.is_closure()
            })
            .map(|(_, call)| call)
    }

    /// The plain call of a closure over the op array `f` names, with
    /// `count` arguments, [`UnitView::hold_call`] took.
    #[must_use]
    pub fn closure_call(&self, f: EngineFuncRef, count: u32) -> Option<&super::PlainCall> {
        (self.calls.iter())
            .map(|(_, call)| call)
            .find(|call| call.callee() == Some(f) && call.count() == count && call.is_closure())
    }

    /// The plain call to the native named `name`, lowered, with `count`
    /// arguments; none unless [`UnitView::hold_call`] took one.
    #[must_use]
    pub fn native_call(&self, name: &[u8], count: u32) -> Option<&super::PlainCall> {
        (self.calls.iter())
            .map(|(_, call)| call)
            .find(|call| call.is_native() && call.name() == name && call.count() == count)
    }

    /// The plain call [`UnitView::hold_call`] took to the method `name`,
    /// lowered, of the request-scoped class `scope`, with `count`
    /// arguments, whose op array is the request's own; none unless it
    /// took one.
    #[must_use]
    pub fn request_call(
        &self,
        scope: RecordClass,
        name: &[u8],
        count: u32,
    ) -> Option<&super::PlainCall> {
        (self.calls.iter())
            .find(|(held, call)| {
                call.is_request()
                    && *held == Some(scope)
                    && call.name() == name
                    && call.count() == count
            })
            .map(|(_, call)| call)
    }

    /// The op array `f` names; none unless [`UnitView::hold`] accepted it.
    #[must_use]
    pub fn op_array(&self, f: EngineFuncRef) -> Option<&OpArray> {
        if !self.funcs.contains(&f) {
            return None;
        }
        self.epoch.row(f)?.op_array()
    }

    /// Whether the op array `f` names is its file's own code, whose
    /// variables a symbol table reaches, php's `!op_array->function_name`
    /// (`zend_jit_var_may_alias`, `Z/ext/opcache/jit/zend_jit_trace.c:363`);
    /// none unless [`UnitView::hold`] accepted it.
    #[must_use]
    pub fn is_main(&self, f: EngineFuncRef) -> Option<bool> {
        if !self.funcs.contains(&f) {
            return None;
        }
        Some(self.epoch.row(f)?.is_main())
    }

    /// Whether the op array `f` names is a property hook's body, php's
    /// `op_array.prop_info`, which php's recorder stops at a call into
    /// (`ZEND_JIT_TRACE_STOP_PROP_HOOK_CALL`); none unless
    /// [`UnitView::hold`] accepted it.
    #[must_use]
    pub fn is_hook(&self, f: EngineFuncRef) -> Option<bool> {
        if !self.funcs.contains(&f) {
            return None;
        }
        Some(self.epoch.row(f)?.is_hook())
    }

    /// Whether a frame that runs the op array `f` names may have a
    /// receiver, `EX(This)`, which its leave gives up: a method's, a
    /// closure's, a hook's and a file's own code's, which an include
    /// from a method runs with its includer's; never a function's. None
    /// unless [`UnitView::hold`] accepted it.
    #[must_use]
    pub fn receives(&self, f: EngineFuncRef) -> Option<bool> {
        if !self.funcs.contains(&f) {
            return None;
        }
        Some(self.epoch.row(f)?.receives())
    }

    /// The parameters the declaration of the op array `f` names lists,
    /// php's `arg_info`, with their types and whether each is passed by
    /// reference; none unless [`UnitView::hold`] accepted it, and an
    /// empty list for a file's own code.
    #[must_use]
    pub fn parameters(&self, f: EngineFuncRef) -> Option<&[Parameter]> {
        if !self.funcs.contains(&f) {
            return None;
        }
        self.epoch.row(f)?.parameters()
    }

    /// The declaration the op array `f` names is the body of and the class
    /// it is declared in, php's `function_name` and `scope`, which
    /// php's analysis of the op array reads; none unless [`UnitView::hold`]
    /// accepted it.
    #[must_use]
    pub fn declaration(&self, f: EngineFuncRef) -> Option<Declaration<'_>> {
        if !self.funcs.contains(&f) {
            return None;
        }
        let (function, scope) = self.epoch.row(f)?.declaration()?;
        Some(Declaration { function, scope })
    }

    /// The class `lcname`, in lower case, binds for php's analysis of the
    /// op array `f` names: the engine's own, or one declared in the op
    /// array's file, as the request's table held it when the view
    /// accepted the op array (`zend_optimizer_get_class_entry`, its
    /// `script` null); none for any other.
    #[must_use]
    pub fn class_named(&self, f: EngineFuncRef, lcname: &[u8]) -> Option<RecordClass> {
        (self.named.iter())
            .find(|(func, name, _)| *func == f && **name == *lcname)
            .map(|(_, _, class)| *class)
    }

    /// The engine's own class `lcname`, in lower case, where a declared
    /// type of what the view holds names it, or it is `Closure` or
    /// `Traversable`, which php's analysis names itself.
    #[must_use]
    pub fn engine_class(&self, lcname: &[u8]) -> Option<EpochClassId> {
        (self.engine_named.iter())
            .find(|(name, _)| **name == *lcname)
            .map(|(_, class)| *class)
    }

    /// The class the op array `f` names is declared in, php's
    /// `op_array->scope`, where the view holds it.
    #[must_use]
    pub fn scope(&self, f: EngineFuncRef) -> Option<RecordClass> {
        (self.scopes.iter())
            .find(|(func, _)| *func == f)
            .map(|(_, class)| *class)
    }

    /// `ce->parent` of a class bound for the analysis.
    #[must_use]
    pub fn parent(&self, c: RecordClass) -> Option<RecordClass> {
        (self.related.iter())
            .find(|(class, _)| *class == c)
            .and_then(|(_, parent)| *parent)
    }

    /// What the analysis reads of the declaration of class `c`
    /// ([`ClassFacts`]): a request's own where the analysis binds it, and
    /// a shared one where [`UnitView::hold_class`] accepted it, none for
    /// one of the engine's own the analysis did not bind.
    #[must_use]
    pub fn class_facts(&self, c: RecordClass) -> Option<&ClassFacts> {
        match c {
            RecordClass::Own(own) => {
                let (_, facts) = self.own.get(own.position())?.bound.as_ref()?;
                Some(facts)
            }
            RecordClass::Shared(shared) => (self.shared_facts.iter())
                .find(|(held, _)| *held == shared)
                .map(|(_, facts)| facts),
        }
    }

    /// The linked record of class `c`, its classes named as the view
    /// names them ([`Linked`]): a request's own where the analysis binds
    /// it, and a shared one where [`UnitView::hold_class`] accepted it.
    #[must_use]
    pub fn class(&self, c: RecordClass) -> Option<Linked<'_>> {
        match c {
            RecordClass::Own(own) => {
                let (linked, _) = self.own.get(own.position())?.bound.as_ref()?;
                Some(Linked::own(c, linked))
            }
            RecordClass::Shared(shared) => {
                if !self.classes.contains(&shared) {
                    return None;
                }
                if let Some(record) = self.epoch.0.native(shared) {
                    return Some(Linked::native(shared, record));
                }
                let record = self.epoch.0.class(ClassId::from(shared))?.record()?;
                Some(Linked::published(shared, record, &self.epoch.0))
            }
        }
    }

    /// Where slot `slot` of the `run_time_cache` of the op array `f` names
    /// stands; none unless [`UnitView::hold`] accepted it, and for a slot
    /// past its cache.
    #[must_use]
    pub fn cache_slot(&self, f: EngineFuncRef, slot: CacheSlot) -> Option<MapPtrRaw> {
        let code = self.op_array(f)?;
        (slot.0 < code.cache_size).then_some(MapPtrRaw {
            offset: f.offset(),
            slot: slot.0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{CallSite, CalledBody, EngineFuncRef, UnitView};
    use crate::test::Files;

    /// Whether `entry` binds the call `site` of the op array `f` names.
    fn binds(
        entry: &(EngineFuncRef, super::Site, super::CalledFunction),
        f: EngineFuncRef,
        site: CallSite<'_>,
    ) -> bool {
        entry.0 == f && entry.1.as_call_site() == site
    }

    /// The key `view` binds for `f` once its call `site` reaches `body`.
    fn reaching(
        view: &mut UnitView,
        f: EngineFuncRef,
        site: CallSite<'_>,
        body: CalledBody,
    ) -> Option<super::Bindings> {
        (view.called.iter_mut())
            .filter(|entry| binds(entry, f, site))
            .for_each(|(.., called)| called.body = body);
        view.bindings(f)
    }

    /// The key a view binds for `f`'s analysis names the body of the
    /// function its call of `g` reaches by its kind (#4628): bound to
    /// another user op array than `g`'s, the file's own code or a
    /// request's own op array, the key is the one it binds for `g`; bound
    /// to a native, to `f` itself, or to nothing, the key is another.
    #[test]
    fn the_key_names_a_callees_body_by_its_kind() {
        let files = Files::new(
            "unit-view-key",
            &[(
                "inc.php",
                "<?php function g() { return 1; }\n\
                 function f() { g(); return hold_caller(); }\n\
                 hold_caller(); f();",
            )],
        );
        let mut views = crate::tests::unit_view::kept(files.path());
        let (mut view, f) = views.pop().expect("f's view");
        let (_, main) = views.pop().expect("the view of the file's own code");
        let f = EngineFuncRef::of(f).expect("f's op array, the engine's");
        let main = EngineFuncRef::of(main).expect("the file's own code, the engine's");
        let site = CallSite::Function(b"g");
        let g = view.called(f, site).map(|called| called.body);
        assert!(
            matches!(g, Some(CalledBody::Engine(g)) if g != f && g != main),
            "f's call reaches g's op array: {g:?}"
        );
        let key = view.bindings(f);
        assert!(key.is_some(), "f's analysis is kept under a key");
        for other in [CalledBody::Engine(main), CalledBody::Request] {
            assert_eq!(
                reaching(&mut view, f, site, other),
                key,
                "another user op array than g's: {other:?}"
            );
        }
        for (body, kind) in [
            (CalledBody::Native, "a native"),
            (CalledBody::Engine(f), "f itself"),
        ] {
            assert_ne!(reaching(&mut view, f, site, body), key, "{kind}");
        }
        view.called.retain(|entry| !binds(entry, f, site));
        assert_ne!(view.bindings(f), key, "the call left unbound");
    }
}
