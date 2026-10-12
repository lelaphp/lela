//! The runtime caches a request reaches (design §5.14): php's
//! `ZEND_MAP_PTR(op_array->run_time_cache)`, which is either an offset
//! into the request's map table or a cache of the request's own on the
//! heap (`ZEND_MAP_PTR_IS_OFFSET`). An op array of a unit the engine
//! published takes an offset of the engine's count as it is published
//! (`ZEND_MAP_PTR_NEW`), and every request that serves it finds the
//! cache at that entry of its map table. An op array of a unit compiled
//! for one request alone, from source the host handed over, an `eval`'s
//! string or a file no cache keeps, takes no offset: php gives it a
//! cache allocated with the op array and freed with it
//! (`ZEND_ACC_HEAP_RT_CACHE`, `Z/Zend/zend_execute.c:4516`,
//! `Z/Zend/zend_opcode.c:559`), so `CG(map_ptr_last)` moves only for
//! what opcache persists. Here the request hands such an op array a
//! number of its own when it takes the unit, holds its cache in a
//! second table beside the map table, and hands the number out again
//! once nothing holds the unit, so an `eval` in a loop grows neither
//! the engine's count nor the request's table.

use std::sync::{Arc, Weak};

use php_heap::{MapPtr, MapTable};
use php_unit::{MapPtrOffset, OpArray, Unit};

use super::RuntimeCache;

/// `EX(run_time_cache)` typed, the engine's side of what an op array
/// carries ([`MapPtrOffset`]): an entry of the request's map table at
/// the offset the engine gave the op array, or an entry of the caches
/// the request holds for the op arrays of the units compiled for it
/// alone, at the number it gave the op array.
#[derive(Debug, Clone, Copy)]
pub(crate) enum CacheAt {
    Engine(MapPtr<RuntimeCache>),
    Request(MapPtr<RuntimeCache>),
}

impl CacheAt {
    /// The cache an op array's `runtime_cache` names, the one place the
    /// unit's number becomes the engine's pointer.
    pub(crate) fn of(offset: MapPtrOffset) -> CacheAt {
        match offset {
            MapPtrOffset::Engine(at) => CacheAt::Engine(MapPtr::new(at)),
            MapPtrOffset::Request(at) => CacheAt::Request(MapPtr::new(at)),
        }
    }
}

/// Two pointers are one where they name the same entry of the same
/// table.
impl PartialEq for CacheAt {
    fn eq(&self, other: &CacheAt) -> bool {
        match (self, other) {
            (CacheAt::Engine(one), CacheAt::Engine(two))
            | (CacheAt::Request(one), CacheAt::Request(two)) => one.offset() == two.offset(),
            _ => false,
        }
    }
}

impl Eq for CacheAt {}

impl Default for CacheAt {
    fn default() -> CacheAt {
        CacheAt::of(MapPtrOffset::default())
    }
}

/// Every runtime cache one request reaches: the map table, indexed by
/// the engine's offsets, and the request's own caches, indexed by the
/// numbers it hands the op arrays of the units it compiled for itself,
/// with what it needs to hand a number out again.
#[derive(Debug, Default)]
pub(crate) struct RuntimeCaches {
    engine: MapTable<RuntimeCache>,
    request: MapTable<RuntimeCache>,
    /// How many numbers the request has handed out, the next new one.
    last: u32,
    /// The numbers of units nothing holds any longer, handed out first.
    free: Vec<u32>,
    /// Each request-only unit the request took, and the numbers its op
    /// arrays hold.
    held: Vec<(Weak<Unit>, Vec<u32>)>,
}

impl RuntimeCaches {
    /// `ZEND_MAP_PTR_GET`: the cache at `at`, none where the request has
    /// not reached it.
    #[inline]
    pub(crate) fn get(&self, at: CacheAt) -> Option<&RuntimeCache> {
        match at {
            CacheAt::Engine(ptr) => self.engine.get(ptr),
            CacheAt::Request(ptr) => self.request.get(ptr),
        }
    }

    /// `RUN_TIME_CACHE(op_array)` not `NULL`: whether the cache at `at`
    /// was laid down ([`super::lay_runtime_cache`]), which an activation
    /// finds it is after the function's first.
    #[inline]
    pub(crate) fn laid(&self, at: CacheAt) -> bool {
        self.get(at).is_some_and(|cache| cache.slots.is_some())
    }

    /// The cache at `at`, an empty one laid in where the request had
    /// none.
    pub(crate) fn map(&mut self, at: CacheAt) -> &mut RuntimeCache {
        match at {
            CacheAt::Engine(ptr) => self.engine.map(ptr),
            CacheAt::Request(ptr) => self.request.map(ptr),
        }
    }

    /// A cache of the request's own for a function that is no op
    /// array's own, a trait's method copied into a class the request
    /// declared (`function_add_ref`'s fresh map pointer): a number of
    /// the request's, one nothing holds any longer first, held for the
    /// rest of the request.
    pub(crate) fn own_cache(&mut self) -> CacheAt {
        CacheAt::Request(MapPtr::new(self.next_number()))
    }

    /// The cache at `at`, which was the request's own
    /// ([`RuntimeCaches::own_cache`]), given back: emptied, as php's
    /// `efree` of a closure's `ZEND_ACC_HEAP_RT_CACHE` cache frees it,
    /// and its number handed out again. A pointer of the engine's is
    /// never given back.
    pub(crate) fn give_back(&mut self, at: CacheAt) {
        if let CacheAt::Request(ptr) = at {
            *self.request.map(ptr) = RuntimeCache::default();
            self.free.push(ptr.offset());
        }
    }

    /// The next number of the request's: one a unit nothing holds any
    /// longer gave back first, else a new one.
    fn next_number(&mut self) -> u32 {
        self.free.pop().unwrap_or_else(|| {
            let next = self.last;
            self.last = self.last.saturating_add(1);
            next
        })
    }

    /// A unit compiled for this request alone, shared from here on: each
    /// of its op arrays given a number of the request's, a number of a
    /// unit nothing holds any longer first, whose cache was emptied as
    /// the unit was found over, as php frees a heap cache with its op
    /// array.
    pub(crate) fn take(&mut self, mut unit: Unit) -> Arc<Unit> {
        let (free, request) = (&mut self.free, &mut self.request);
        self.held.retain_mut(|(unit, numbers)| {
            if unit.strong_count() > 0 {
                return true;
            }
            for &number in numbers.iter() {
                *request.map(MapPtr::new(number)) = RuntimeCache::default();
            }
            free.append(numbers);
            false
        });
        let mut numbers = Vec::new();
        for code in op_arrays_mut(&mut unit) {
            let number = self.next_number();
            code.runtime_cache = MapPtrOffset::Request(number);
            numbers.push(number);
        }
        let unit = unit.published();
        self.held.push((Arc::downgrade(&unit), numbers));
        unit
    }
}

/// Every op array of `unit`: its main one, its functions', the ones no
/// name binds, its methods' and its property hooks'.
pub(crate) fn op_arrays_mut(unit: &mut Unit) -> impl Iterator<Item = &mut OpArray> {
    std::iter::once(&mut unit.main)
        .chain(unit.functions.iter_mut().map(|(_, code)| code))
        .chain(unit.dynamic.iter_mut().map(|(_, code)| code))
        .chain(unit.methods.iter_mut().map(|method| &mut method.body))
        .chain(unit.hooks.iter_mut().map(|hook| &mut hook.body))
}
