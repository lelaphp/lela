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
//! once nothing holds the unit, as its last holder gives it up, so an
//! `eval` in a loop grows neither the engine's count nor the request's
//! table.

use php_heap::{MapPtr, MapTable};
use php_unit::{MapPtrOffset, OpArray, PublishedUnit, Unit, WeakUnit};

use super::RuntimeCache;

/// `EX(run_time_cache)` typed, the engine's side of what an op array
/// carries ([`MapPtrOffset`]): an entry of the request's map table at
/// the offset the engine gave the op array, or an entry of the caches
/// the request holds for the op arrays of the units compiled for it
/// alone, at the number it gave the op array.
///
/// One half-word, so a frame's header holds it in four bytes: the entry's
/// number, with [`CacheAt::REQUEST`] set for the request's own table, as
/// [`crate::api::values::ClassId::LOCAL`] marks a class of the request's.
/// Both numbers count op arrays, the engine's those it published and the
/// request's those of its own units standing at once, far below the bit.
/// Two are one where they name the same entry of the same table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CacheAt(u32);

/// Which table a [`CacheAt`] names an entry of, and the entry.
enum Table {
    Engine(MapPtr<RuntimeCache>),
    Request(MapPtr<RuntimeCache>),
}

impl CacheAt {
    /// The bit that marks an entry of the request's own caches.
    const REQUEST: u32 = 1 << 31;

    /// The cache an op array's `runtime_cache` names, the one place the
    /// unit's number becomes the engine's pointer.
    pub(crate) fn of(offset: MapPtrOffset) -> CacheAt {
        match offset {
            MapPtrOffset::Engine(at) => CacheAt::engine(at),
            MapPtrOffset::Request(at) => CacheAt::request(at),
        }
    }

    fn engine(at: u32) -> CacheAt {
        CacheAt(at & !CacheAt::REQUEST)
    }

    fn request(at: u32) -> CacheAt {
        CacheAt(at | CacheAt::REQUEST)
    }

    /// The table and the entry, decoded.
    fn table(self) -> Table {
        let at = MapPtr::new(self.0 & !CacheAt::REQUEST);
        if self.0 & CacheAt::REQUEST == 0 {
            Table::Engine(at)
        } else {
            Table::Request(at)
        }
    }
}

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
    held: Vec<(WeakUnit, Vec<u32>)>,
    /// The vectors of numbers units nothing holds any longer stood in,
    /// emptied, for the next units to take.
    spare: Vec<Vec<u32>>,
}

impl RuntimeCaches {
    /// `ZEND_MAP_PTR_GET`: the cache at `at`, none where the request has
    /// not reached it.
    #[inline]
    pub(crate) fn get(&self, at: CacheAt) -> Option<&RuntimeCache> {
        match at.table() {
            Table::Engine(ptr) => self.engine.get(ptr),
            Table::Request(ptr) => self.request.get(ptr),
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
        match at.table() {
            Table::Engine(ptr) => self.engine.map(ptr),
            Table::Request(ptr) => self.request.map(ptr),
        }
    }

    /// A cache of the request's own for a function that is no op
    /// array's own: a trait's method copied into a class the request
    /// declared (`function_add_ref`'s fresh map pointer), whose cache the
    /// arena holds once its first call lays it down, or a closure in
    /// another scope than its function's cache was laid down for
    /// (`zend_create_closure_ex`'s `ZEND_ACC_HEAP_RT_CACHE`), whose
    /// `room` is php's `emalloc` of the cache, held until the closure
    /// gives it back ([`RuntimeCaches::give_back`]). A number of the
    /// request's, one nothing holds any longer first.
    pub(crate) fn own_cache(&mut self, room: Option<php_heap::Block>) -> CacheAt {
        let at = CacheAt::request(self.next_number());
        self.map(at).room = room;
        at
    }

    /// The cache at `at`, which was the request's own
    /// ([`RuntimeCaches::own_cache`]), given back: emptied, its room
    /// back to the request's heap, as php's `efree` of a closure's
    /// `ZEND_ACC_HEAP_RT_CACHE` cache frees it, and its number handed
    /// out again. A pointer of the engine's is never given back.
    pub(crate) fn give_back(&mut self, heap: &mut php_heap::Heap, at: CacheAt) {
        if let Table::Request(ptr) = at.table() {
            let cache = std::mem::take(self.request.map(ptr));
            if let Some(room) = cache.room {
                heap.free_block(room);
            }
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

    /// `unit`, whose last share its holder is giving up: the numbers
    /// its op arrays held handed out again, their caches emptied, as php
    /// frees a heap cache with its op array, so the next unit takes them
    /// with no scan. A unit another share holds, or one the request did
    /// not take, keeps what it holds.
    pub(crate) fn release(&mut self, unit: &PublishedUnit) {
        let Some(at) = self.held.iter().rposition(|(held, _)| {
            std::ptr::eq(held.as_ptr(), PublishedUnit::as_ptr(unit)) && held.strong_count() == 1
        }) else {
            return;
        };
        let (_, mut numbers) = self.held.swap_remove(at);
        for &number in &numbers {
            *self.request.map(MapPtr::new(number)) = RuntimeCache::default();
        }
        self.free.append(&mut numbers);
        self.spare.push(numbers);
    }

    /// A unit compiled for this request alone, shared from here on, and
    /// published in `room`, a unit the request gave up, where there is
    /// one: each of its op arrays given a number of the request's, one
    /// handed back first. A unit's last holder hands its numbers back as
    /// it gives the unit up ([`RuntimeCaches::release`]); the scan here
    /// finds those of a unit whose last share went another way, a
    /// function or a closure it declared dropped with it, and empties
    /// their caches as php frees a heap cache with its op array.
    pub(crate) fn take(
        &mut self,
        mut unit: Unit,
        room: Option<php_unit::SpareUnit>,
    ) -> PublishedUnit {
        let (free, request, spare) = (&mut self.free, &mut self.request, &mut self.spare);
        // Asked first, as the scan finds nothing where every unit gone
        // was given back by its last holder, which is the rule.
        if self.held.iter().any(|(unit, _)| unit.strong_count() == 0) {
            self.held.retain_mut(|(unit, numbers)| {
                if unit.strong_count() > 0 {
                    return true;
                }
                for &number in numbers.iter() {
                    *request.map(MapPtr::new(number)) = RuntimeCache::default();
                }
                free.append(numbers);
                spare.push(std::mem::take(numbers));
                false
            });
        }
        let mut numbers = self.spare.pop().unwrap_or_default();
        for code in op_arrays_mut(&mut unit) {
            let number = self.next_number();
            code.runtime_cache = MapPtrOffset::Request(number);
            numbers.push(number);
        }
        let unit = unit.published(room);
        self.held.push((PublishedUnit::downgrade(&unit), numbers));
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
