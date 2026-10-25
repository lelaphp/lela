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

use php_heap::ZvalType;
use php_link::LinkedClass;
use php_unit::decl::Parameter;
use php_unit::{CacheSlot, MapPtrOffset, OpArray, Opcode, OperandKind, Opline, OplineIdx};

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
    pub fn word(&self, f: FuncRef, at: SafepointIdx) -> &Word {
        self.func(f).map_or(&SEALED, |func| func.word(at))
    }

    /// The landing word of opline `at` of the op array `f` names
    /// (`jit-design.md` §2.2): the word of the returns that land there,
    /// which php's `zend_jit_trace_setup_ret_counter` arms on the opline
    /// past a call; sealed until a tier armed it
    /// ([`Epoch::lay_landing`]), and for an op array this epoch holds no
    /// row for.
    #[must_use]
    pub fn landing(&self, f: FuncRef, at: OplineIdx) -> &Word {
        self.func(f).map_or(&SEALED, |func| func.landing(at))
    }

    /// The landing word of opline `at` of the op array `f` names, for a
    /// tier to arm, the op array's landing words laid down sealed the
    /// first time; none for an op array this epoch holds no row for, and
    /// past its last opline.
    #[must_use]
    pub fn lay_landing(&self, f: FuncRef, at: OplineIdx) -> Option<&Word> {
        self.func(f)?.lay_landing(at)
    }

    /// The op array `f` names, held by this epoch for as long as the
    /// borrow: what compiled code that recorded `f` in this epoch hands a
    /// step ([`super::step_held`]), so the step reads the op array the
    /// trace holds and no frame's function. None for an op array this
    /// epoch holds no row for, a request's.
    #[must_use]
    pub fn held(&self, f: FuncRef) -> Option<Held<'_>> {
        self.func(f)?.op_array().map(Held)
    }

    /// The row of a held op array.
    fn func(&self, f: FuncRef) -> Option<&super::published::Func> {
        match f.0 {
            MapPtrOffset::Engine(offset) => self.0.func(offset),
            MapPtrOffset::Request(_) => None,
        }
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
        files.funcs.publish(&unit);
        let arena = Arena::of_engine(Arc::from([]), 0, files, crate::arena::Interned::default());
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
pub fn epoch_state<T: std::any::Any + Send + Sync>(
    req: &Request,
    make: impl FnOnce() -> T,
) -> Option<&T> {
    let held = req.arena().tier.get_or_init(|| Box::new(make()));
    held.downcast_ref()
}

/// One op array as the tier names it (`jit-design.md` §3.1): by the
/// map-pointer offset its unit's publication gave it, which no other op
/// array of the engine shares, where its unit is held; by the request's
/// own offset where it was compiled for the request alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FuncRef(MapPtrOffset);

impl FuncRef {
    /// The map-pointer offset that names the op array, as a recorded
    /// trace's text form writes it (`jit-design.md` §4.1).
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
    match offset {
        MapPtrOffset::Engine(offset) => req.arena().func(offset),
        MapPtrOffset::Request(_) => None,
    }
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
fn own_row<'r>(
    req: &'r Request,
    header: &crate::exec::Header,
) -> Option<&'r super::published::Func> {
    match header.runtime_cache().offset() {
        MapPtrOffset::Engine(offset) if header.info.user() => req.arena().func(offset),
        _ => None,
    }
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
/// sealed word until a tier armed it, and for an op array no epoch holds.
#[must_use]
pub fn landing(req: &Request, frame: FrameRef, at: OplineIdx) -> &Word {
    held(req, frame).map_or(&SEALED, |func| func.landing(at))
}

/// Where a request keeps one slot of a held op array's `run_time_cache`:
/// the engine's map-pointer offset every request lays that cache at, and
/// the slot in it, which compiled code reads past the request's map
/// base (`jit-design.md` §4.2's `map_ptrs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    (epoch.func(FuncRef(MapPtrOffset::Engine(raw.offset))))
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
/// ([`PropertySlot::cached`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PropertySlot {
    func: FuncRef,
    at: OplineIdx,
    class: ClassId,
    slot: u32,
    typed: bool,
    admits: u16,
    cached: Option<MapPtrRaw>,
}

impl PropertySlot {
    /// The runtime-cache slot compiled code reads the place from, in the
    /// request it runs in, for a request-scoped class, as php's code reads
    /// `CACHED_PTR(cache_slot)` for a class that is not immutable
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:14341-14370`); none for a
    /// published class, whose place is this one in every request.
    #[must_use]
    pub fn cached(self) -> Option<MapPtrRaw> {
        self.cached
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

/// What a trace may read of the op arrays, literals and published
/// classes it touched (`jit-design.md` §4.1, §5.3): built by the recorder
/// from the request, it holds the request's epoch once and the ids it
/// accepted, and nothing else, so it is `Send` and outlives the request
/// that built it for as long as it stands. What it reads, it reads
/// through the epoch, which keeps every held unit and published class
/// until it is dropped.
#[derive(Debug)]
pub struct UnitView {
    epoch: Epoch,
    funcs: Vec<FuncRef>,
    classes: Vec<ClassId>,
    /// The plain calls the recording took ([`UnitView::hold_call`]).
    calls: Vec<super::PlainCall>,
    /// The property places the recording took ([`UnitView::hold_place`]).
    places: Vec<PropertySlot>,
    /// Whether an observer of calls was declared ([`UnitView::observed`]).
    observed: bool,
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
            observed: crate::observer::observing(req),
        }
    }

    /// Accept the op array `f` names where the view's epoch holds its
    /// unit, holding nothing; `false` for a request-scoped one, which a
    /// trace may not read past the request, and for a request of another
    /// epoch.
    pub fn hold(&mut self, req: &Request, f: FuncRef) -> bool {
        if !self.epoch.of(req) || self.epoch.func(f).is_none() {
            return false;
        }
        if !self.funcs.contains(&f) {
            self.funcs.push(f);
        }
        true
    }

    /// Accept class `c` where every request shares it, the engine's own
    /// or one the epoch published (`ClassId::is_published`), holding
    /// nothing; `false` for a request-local one, which the trace reads
    /// through the request's cache slot instead.
    pub fn hold_class(&mut self, req: &Request, c: ClassId) -> bool {
        if !self.epoch.of(req) || !c.is_published() || self.epoch.0.linked_class(c).is_none() {
            return false;
        }
        if !self.classes.contains(&c) {
            self.classes.push(c);
        }
        true
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
        let Some(header) = req.vm().at(frame) else {
            return false;
        };
        let Some(func) = own_row(req, header).filter(|func| func.fixes_scope()) else {
            return false;
        };
        let f = FuncRef(header.runtime_cache().offset());
        let Some(op) = func
            .op_array()
            .and_then(|code| code.oplines.get(at.0 as usize))
        else {
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
            Opcode::AssignObjOp => match func
                .op_array()
                .and_then(|code| code.oplines.get(at.0 as usize + 1))
            {
                Some(data) => CacheSlot(data.extended_value),
                None => return false,
            },
            _ => return false,
        };
        if op.op2_type != OperandKind::Const || !self.hold(req, f) {
            return false;
        }
        // A request-scoped class's place is read from the cache slot of
        // the request compiled code runs in.
        let cached = if class.is_published() {
            if !self.hold_class(req, class) {
                return false;
            }
            None
        } else {
            match self.cache_slot(f, slot) {
                Some(raw) => Some(raw),
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
            class,
            slot,
            typed: ty.typed(),
            admits,
            cached,
        };
        if !self.places.contains(&place) {
            self.places.push(place);
        }
        true
    }

    /// The declared slot the property opline at `at` of the op array `f`
    /// names on an object of `class`; none unless
    /// [`UnitView::hold_place`] took it.
    #[must_use]
    pub fn place(&self, f: FuncRef, at: OplineIdx, class: ClassId) -> Option<PropertySlot> {
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
    /// ([`super::plain_call`]) to an op array the view holds, so a trace
    /// that recorded it writes it in place; `false`, taking nothing, for
    /// any other.
    pub fn hold_call(&mut self, req: &Request, call: FrameRef) -> bool {
        let Some(plain) = super::plain_call(req, call) else {
            return false;
        };
        if let Some(callee) = plain.callee()
            && !self.hold(req, callee)
        {
            return false;
        }
        if !self.calls.contains(&plain) {
            self.calls.push(plain);
        }
        true
    }

    /// The plain call to the op array `f` names with `count` arguments;
    /// none unless [`UnitView::hold_call`] took one.
    #[must_use]
    pub fn call(&self, f: FuncRef, count: u32) -> Option<&super::PlainCall> {
        self.calls
            .iter()
            .find(|call| call.callee() == Some(f) && call.count() == count)
    }

    /// The plain call to the native named `name`, lowered, with `count`
    /// arguments; none unless [`UnitView::hold_call`] took one.
    #[must_use]
    pub fn native_call(&self, name: &[u8], count: u32) -> Option<&super::PlainCall> {
        (self.calls.iter())
            .find(|call| call.callee().is_none() && call.name() == name && call.count() == count)
    }

    /// The op array `f` names; none unless [`UnitView::hold`] accepted it.
    #[must_use]
    pub fn op_array(&self, f: FuncRef) -> Option<&OpArray> {
        if !self.funcs.contains(&f) {
            return None;
        }
        self.epoch.func(f)?.op_array()
    }

    /// Whether the op array `f` names is its file's own code, whose
    /// variables a symbol table reaches, php's `!op_array->function_name`
    /// (`zend_jit_var_may_alias`, `Z/ext/opcache/jit/zend_jit_trace.c:363`);
    /// none unless [`UnitView::hold`] accepted it.
    #[must_use]
    pub fn is_main(&self, f: FuncRef) -> Option<bool> {
        if !self.funcs.contains(&f) {
            return None;
        }
        Some(self.epoch.func(f)?.is_main())
    }

    /// The parameters the declaration of the op array `f` names lists,
    /// php's `arg_info`, with their types and whether each is passed by
    /// reference; none unless [`UnitView::hold`] accepted it, and an
    /// empty list for a file's own code.
    #[must_use]
    pub fn parameters(&self, f: FuncRef) -> Option<&[Parameter]> {
        if !self.funcs.contains(&f) {
            return None;
        }
        self.epoch.func(f)?.parameters()
    }

    /// The linked record of class `c`; none unless
    /// [`UnitView::hold_class`] accepted it.
    #[must_use]
    pub fn class(&self, c: ClassId) -> Option<&LinkedClass<ClassId>> {
        if !self.classes.contains(&c) {
            return None;
        }
        self.epoch.0.linked_class(c)
    }

    /// Where slot `slot` of the `run_time_cache` of the op array `f` names
    /// stands; none unless [`UnitView::hold`] accepted it, and for a slot
    /// past its cache.
    #[must_use]
    pub fn cache_slot(&self, f: FuncRef, slot: CacheSlot) -> Option<MapPtrRaw> {
        let code = self.op_array(f)?;
        let MapPtrOffset::Engine(offset) = code.runtime_cache else {
            return None;
        };
        (slot.0 < code.cache_size).then_some(MapPtrRaw {
            offset,
            slot: slot.0,
        })
    }
}
