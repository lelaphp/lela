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
//!
//! Either table's entry names a cache; its slots stand in one slab of
//! the request's ([`Slab`]), and a frame reads them off the base its
//! activation resolved the entry to ([`CacheBase`]), never through the
//! entry again, as php's frame reads `EX(run_time_cache)` and never the
//! map pointer.

use php_heap::{MapPtr, MapTable};
use php_unit::{MapPtrOffset, OpArray, PublishedUnit, Unit, WeakUnit};

use super::{CacheSlot, Cached, RuntimeCache, SLOT_BYTES};

/// `EX(run_time_cache)` resolved: where a laid cache's first slot stands
/// in the request's [`Slab`], which an activation reads off the cache it
/// lays or finds laid, once, as `i_init_func_execute_data` reads
/// `RUN_TIME_CACHE(op_array)` once into `EX(run_time_cache)`
/// (`Z/Zend/zend_execute.c:4446`), so that `CACHED_PTR` is one read off
/// it (`Z/Zend/zend_execute.h:527`, [`RuntimeCaches::slot`]).
///
/// One half-word: the chunk in the top [`CHUNK_BITS`] and the slot's
/// place in it below. [`CacheBase::NONE`] names the last chunk, which
/// the slab never carves, so a frame with no cache laid reads nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CacheBase(u32);

/// The bits of a [`CacheBase`] that name its chunk.
const CHUNK_BITS: u32 = 5;
/// The bits that place a slot in its chunk.
const PLACE_BITS: u32 = u32::BITS - CHUNK_BITS;
/// The chunks a slab holds, the last never carved.
const CHUNKS: usize = 1 << CHUNK_BITS;
/// The slots the first chunk holds; each after holds twice the one
/// before, up to what [`PLACE_BITS`] can place.
const FIRST_CHUNK: usize = 1024;

impl CacheBase {
    /// No cache: the last place of the chunk no cache is carved from.
    pub(crate) const NONE: CacheBase = CacheBase(u32::MAX);

    fn at(chunk: usize, place: usize) -> Option<CacheBase> {
        // The last chunk is `NONE`'s.
        let chunk = u32::try_from(chunk).ok().filter(|_| chunk + 1 < CHUNKS)?;
        let place = u32::try_from(place).ok().filter(|&p| p < 1 << PLACE_BITS)?;
        Some(CacheBase(chunk << PLACE_BITS | place))
    }

    /// The chunk, below [`CHUNKS`] by construction.
    #[inline(always)]
    fn chunk(self) -> usize {
        (self.0 >> PLACE_BITS) as usize
    }

    #[inline(always)]
    fn place(self) -> usize {
        (self.0 & ((1 << PLACE_BITS) - 1)) as usize
    }
}

impl Default for CacheBase {
    fn default() -> CacheBase {
        CacheBase::NONE
    }
}

/// Where one cache's slots were carved in the slab, and how many: what
/// its entry keeps across a lay, so a cache laid again, a file's on each
/// activation or a number handed out again, lays over the same slots.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Place {
    base: CacheBase,
    len: u32,
}

/// The slots of every cache one request lays down, carved one cache
/// after the other from chunks that never move, as php carves each
/// function's cache from `CG(arena)`'s blocks
/// (`init_func_run_time_cache_i`, `Z/Zend/zend_execute.c:4455`). A chunk
/// is filled only within the room it was made with, so a slot stands
/// where it was carved for as long as the request runs, which compiled
/// code that reads a slot in place relies on (`tier::touch`). The chunks
/// stand in the slab itself, so a read finds its chunk with no bounds
/// test, and the slot with one.
#[derive(Debug)]
struct Slab {
    chunks: [Vec<Cached>; CHUNKS],
    /// The chunk caches are carved from now.
    open: usize,
}

impl Default for Slab {
    fn default() -> Slab {
        Slab {
            chunks: std::array::from_fn(|_| Vec::new()),
            open: 0,
        }
    }
}

impl Slab {
    /// `len` slots carved, all `Unprimed`, from the open chunk, or from a
    /// new one where it has not the room; none past what the slab can
    /// place, which no request's caches reach.
    fn carve(&mut self, len: usize) -> Option<CacheBase> {
        loop {
            let open = self.open;
            // The last chunk is [`CacheBase::NONE`]'s, never carved.
            let chunk = self.chunks.get_mut(open).filter(|_| open + 1 < CHUNKS)?;
            if chunk.capacity() == 0 {
                // Twice the chunk before, up to what a base can place.
                let doubled = open.min((PLACE_BITS - FIRST_CHUNK.trailing_zeros()) as usize);
                let room = (FIRST_CHUNK << doubled).max(len);
                *chunk = Vec::with_capacity(room);
            }
            if chunk.capacity() - chunk.len() >= len {
                let base = CacheBase::at(open, chunk.len())?;
                // Within the room the chunk was made with, so it does
                // not move.
                chunk.resize(chunk.len() + len, Cached::Unprimed);
                return Some(base);
            }
            self.open += 1;
        }
    }

    /// The slots at `place`, every one `Unprimed` again.
    fn clear(&mut self, place: Place) {
        let at = place.base.place();
        if let Some(slots) = (self.chunks.get_mut(place.base.chunk()))
            .and_then(|chunk| chunk.get_mut(at..at + place.len as usize))
        {
            slots.fill(Cached::Unprimed);
        }
    }

    #[inline(always)]
    fn slot(&self, base: CacheBase, slot: CacheSlot) -> Option<&Cached> {
        // The chunk is a half-word's top bits, below `CHUNKS`, so the
        // index is never out of the array and is not tested.
        let chunk = &self.chunks[base.chunk() % CHUNKS];
        chunk.get(base.place() + (slot.0 / SLOT_BYTES) as usize)
    }

    fn slot_mut(&mut self, base: CacheBase, slot: CacheSlot) -> Option<&mut Cached> {
        let chunk = &mut self.chunks[base.chunk() % CHUNKS];
        chunk.get_mut(base.place() + (slot.0 / SLOT_BYTES) as usize)
    }
}

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

    /// The `runtime_cache` offset this names an entry at, [`CacheAt::of`]
    /// read back: the engine's for an entry of the engine's table, the
    /// request's for one of its own.
    pub(crate) fn offset(self) -> MapPtrOffset {
        let at = self.0 & !CacheAt::REQUEST;
        if self.0 & CacheAt::REQUEST == 0 {
            MapPtrOffset::Engine(at)
        } else {
            MapPtrOffset::Request(at)
        }
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
    /// The slots of every cache either table's entries laid down.
    slab: Slab,
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
    /// The numbers [`RuntimeCaches::number`] gave the unit about to be
    /// published, until [`RuntimeCaches::hold`] holds them beside it.
    numbering: Vec<u32>,
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
        self.get(at).is_some_and(|cache| cache.laid)
    }

    /// `RUN_TIME_CACHE(op_array)` as an activation reads it into
    /// `EX(run_time_cache)`: where the cache at `at` stands in the slab,
    /// none where it was not laid down.
    #[inline]
    pub(crate) fn base(&self, at: CacheAt) -> Option<CacheBase> {
        let cache = self.get(at).filter(|cache| cache.laid)?;
        Some(cache.place.map_or(CacheBase::NONE, |place| place.base))
    }

    /// `CACHED_PTR(num)` off `EX(run_time_cache)`: the slot `slot`, a
    /// byte offset as the opline carries it, of the cache `base` names,
    /// read in place. A slot past the slab's, or a frame with no cache,
    /// reads none, which a kernel takes as `Unprimed`; the compiler
    /// numbers no slot past its cache's own `cache_size`
    /// ([`super::CacheSize::holds`]).
    #[inline(always)]
    pub(crate) fn slot(&self, base: CacheBase, slot: CacheSlot) -> Option<&Cached> {
        self.slab.slot(base, slot)
    }

    /// `CACHE_PTR(num, ptr)` off `EX(run_time_cache)`: `value` into the
    /// slot [`RuntimeCaches::slot`] reads. A frame with no cache takes
    /// nothing.
    pub(crate) fn set_slot(&mut self, base: CacheBase, slot: CacheSlot, value: Cached) {
        if let Some(cell) = self.slab.slot_mut(base, slot) {
            *cell = value;
        }
    }

    /// Where slot `slot` of the cache at `at` stands, which compiled code
    /// reads in place (`tier::touch`): none for a cache not laid down and
    /// a slot past it. A function's cache stands where it was carved for
    /// as long as the request runs.
    pub(crate) fn place_of(
        &self,
        at: CacheAt,
        slot: CacheSlot,
    ) -> Option<std::ptr::NonNull<Cached>> {
        let place = self.get(at).filter(|cache| cache.laid)?.place?;
        if slot.0 / SLOT_BYTES >= place.len {
            return None;
        }
        self.slab
            .slot(place.base, slot)
            .map(std::ptr::NonNull::from)
    }

    /// `zend_arena_alloc(&CG(arena), cache_size)` and the `memset` after
    /// it: the cache at `at` laid down with `cache_size / sizeof(void *)`
    /// slots, all `Unprimed`, and its observer slot unresolved, over the
    /// slots it was carved before where they are room enough; where it
    /// stands. A cache past what the slab can place is laid with no
    /// slot, which reads `Unprimed` every pass.
    pub(crate) fn lay_down(&mut self, at: CacheAt, cache_size: u32) -> CacheBase {
        let len = cache_size / SLOT_BYTES;
        let cache = match at.table() {
            Table::Engine(ptr) => self.engine.map(ptr),
            Table::Request(ptr) => self.request.map(ptr),
        };
        let place = match cache.place {
            Some(place) if place.len >= len => {
                self.slab.clear(place);
                Some(place)
            }
            _ => self
                .slab
                .carve(len as usize)
                .map(|base| Place { base, len }),
        };
        cache.lay(place);
        place.map_or(CacheBase::NONE, |place| place.base)
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
            if let Some(room) = self.request.map(ptr).empty() {
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

    /// Whether `unit` is one the request compiled for itself and took
    /// ([`RuntimeCaches::take`]): an `eval`'s, a host's source or a file
    /// no cache keeps.
    pub(crate) fn took(&self, unit: &PublishedUnit) -> bool {
        self.held
            .iter()
            .any(|(held, _)| std::ptr::eq(held.as_ptr(), PublishedUnit::as_ptr(unit)))
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
        let (_, numbers) = self.held.swap_remove(at);
        self.hand_back(numbers);
    }

    /// The numbers of a unit that is gone handed out again, their caches
    /// emptied, as php frees a heap cache with its op array. In line in
    /// its two callers: an `eval` in a loop hands one number back each
    /// time (#3051).
    #[inline(always)]
    fn hand_back(&mut self, mut numbers: Vec<u32>) {
        // One push a number: an `eval`'s unit has one or a few, which a
        // push writes in place where `append` copies through `memcpy`.
        for &number in &numbers {
            self.request.map(MapPtr::new(number)).empty();
            self.free.push(number);
        }
        numbers.clear();
        self.spare.push(numbers);
    }

    /// A unit compiled for this request alone, about to be shared, where
    /// its caller publishes it: each of its op arrays given a number of
    /// the request's, one handed back first, which [`RuntimeCaches::hold`]
    /// holds beside it once it is published. A unit's last holder hands
    /// its numbers back as it gives the unit up
    /// ([`RuntimeCaches::release`]); the scan here finds those of a unit
    /// whose last share went another way, a function or a closure it
    /// declared dropped with it, and empties their caches as php frees a
    /// heap cache with its op array.
    pub(crate) fn number(&mut self, unit: &mut Unit) {
        // The scan finds nothing where every unit gone was given back by
        // its last holder, which is the rule.
        while let Some(at) = self
            .held
            .iter()
            .position(|(unit, _)| unit.strong_count() == 0)
        {
            let (_, numbers) = self.held.swap_remove(at);
            self.hand_back(numbers);
        }
        let mut numbers = self.spare.pop().unwrap_or_default();
        for code in op_arrays_mut(unit) {
            let number = self.next_number();
            code.runtime_cache = MapPtrOffset::Request(number);
            numbers.push(number);
        }
        self.numbering = numbers;
    }

    /// `unit`, numbered ([`RuntimeCaches::number`]) and published since,
    /// held with its numbers.
    pub(crate) fn hold(&mut self, unit: &PublishedUnit) {
        let numbers = std::mem::take(&mut self.numbering);
        self.held.push((PublishedUnit::downgrade(unit), numbers));
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
