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

use php_link::LinkedClass;
use php_unit::decl::Parameter;
use php_unit::{CacheSlot, MapPtrOffset, OpArray, OplineIdx};

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

/// The op array `frame` serves; none for a frame that serves none.
#[must_use]
pub fn func(req: &Request, frame: FrameRef) -> Option<FuncRef> {
    Some(FuncRef(req.served_op_array(frame)?.runtime_cache))
}

/// The word of safepoint `at` of the op array `frame` serves, what the
/// tier reads at a site (`jit-design.md` §3.2): the sealed word for an op
/// array no epoch holds, as `eval`'s and an uncached include's, for a
/// frame that serves none, and past the op array's last safepoint.
#[must_use]
pub fn word(req: &Request, frame: FrameRef, at: SafepointIdx) -> &Word {
    held(req, frame).map_or(&SEALED, |func| func.word(at))
}

/// The row of the op array `frame` serves, where an epoch holds it: none
/// for a request's op array and a frame that serves none.
fn held(req: &Request, frame: FrameRef) -> Option<&super::published::Func> {
    match req.served_op_array(frame)?.runtime_cache {
        MapPtrOffset::Engine(offset) => req.arena().func(offset),
        MapPtrOffset::Request(_) => None,
    }
}

/// The landing word of opline `at` of the op array `frame` serves, what
/// the tier reads where a return lands there ([`Epoch::landing`]): the
/// sealed word until a tier armed it, and for an op array no epoch holds.
#[must_use]
pub fn landing(req: &Request, frame: FrameRef, at: OplineIdx) -> &Word {
    held(req, frame).map_or(&SEALED, |func| func.landing(at))
}

/// The opline safepoint `at` of the op array `frame` serves stands at,
/// the back-edge's header or the `RETURN` its number names, where a
/// tier that serves the site's opline itself stands the frame first: a
/// `RETURN`'s inline path raises its site before it stands its frame
/// there. None for a frame that serves no op array and past its last
/// safepoint.
#[must_use]
pub fn safepoint_at(req: &Request, frame: FrameRef, at: SafepointIdx) -> Option<OplineIdx> {
    let code = req.served_op_array(frame)?;
    code.safepoints.get(usize::try_from(at.0).ok()?).copied()
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
