//! The runtime caches a request reaches (design §5.14): php's
//! `ZEND_MAP_PTR(op_array->run_time_cache)`, which is either an offset
//! into the request's map table or a cache of the request's own on the
//! heap (`ZEND_MAP_PTR_IS_OFFSET`). An op array of a unit the engine
//! published takes an offset of the engine's count as it is published
//! (`ZEND_MAP_PTR_NEW`), and every request that serves it finds the
//! cache at that entry of its map table. php gives an op array of a
//! unit compiled for one request alone no offset but a cache allocated
//! with the op array (`ZEND_ACC_HEAP_RT_CACHE`,
//! `Z/Zend/zend_execute.c:4516`), which the request's second table is
//! the home of ([`MapPtrOffset::Request`]).

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
/// numbers it hands the op arrays of the units it compiled for itself.
#[derive(Debug, Default)]
pub(crate) struct RuntimeCaches {
    engine: MapTable<RuntimeCache>,
    request: MapTable<RuntimeCache>,
}

impl RuntimeCaches {
    /// `ZEND_MAP_PTR_GET`: the cache at `at`, none where the request has
    /// not reached it.
    pub(crate) fn get(&self, at: CacheAt) -> Option<&RuntimeCache> {
        match at {
            CacheAt::Engine(ptr) => self.engine.get(ptr),
            CacheAt::Request(ptr) => self.request.get(ptr),
        }
    }

    /// The cache at `at`, an empty one laid in where the request had
    /// none.
    pub(crate) fn map(&mut self, at: CacheAt) -> &mut RuntimeCache {
        match at {
            CacheAt::Engine(ptr) => self.engine.map(ptr),
            CacheAt::Request(ptr) => self.request.map(ptr),
        }
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
