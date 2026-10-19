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
use php_unit::{CacheSlot, MapPtrOffset, OpArray};

use super::published::SEALED;
use super::{FrameRef, Request, SafepointIdx, Word};
use crate::api::values::ClassId;
use crate::arena::Arena;

/// A hold on the epoch a request started in (design §5.14): its arena,
/// whose published classes, held units and their words stand while any
/// hold does, whatever restart replaced it since. `Send` and `Sync`, so
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

    /// The row of a held op array.
    fn func(&self, f: FuncRef) -> Option<&super::published::Func> {
        match f.0 {
            MapPtrOffset::Engine(offset) => self.0.func(offset),
            MapPtrOffset::Request(_) => None,
        }
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
        let unit = unit.published();
        let files = crate::unit_cache::Files::new(1);
        files.funcs.publish(&unit);
        let arena = Arena::of_engine(Arc::from([]), 0, files);
        (Epoch(Arc::new(arena)), FuncRef(unit.main.runtime_cache))
    }
}

/// The epoch `req` started in, held.
#[must_use]
pub fn epoch(req: &Request) -> Epoch {
    Epoch(Arc::clone(req.arena()))
}

/// One op array as the tier names it (`jit-design.md` §3.1): by the
/// map-pointer offset its unit's publication gave it, which no other op
/// array of the engine shares, where its unit is held; by the request's
/// own offset where it was compiled for the request alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FuncRef(MapPtrOffset);

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
    let held = req
        .served_op_array(frame)
        .and_then(|code| match code.runtime_cache {
            MapPtrOffset::Engine(offset) => req.arena().func(offset),
            MapPtrOffset::Request(_) => None,
        });
    held.map_or(&SEALED, |func| func.word(at))
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
