//! The hash iterators (design §5.3): php's `EG(ht_iterators)`, the list
//! of positions a `foreach` by reference and a `foreach` over an object
//! register against the table they walk, so that a body which deletes,
//! appends or rehashes the table leaves the walk where php's leaves it.
//! Copied from `Z/Zend/zend_hash.c:521-745` at the tag: the list and its
//! `next_copy` rings, `zend_hash_iterator_add`, `_pos`, `_pos_ex` and
//! `_del`, and the three moves a write makes, `zend_hash_iterators_update`
//! in a rehash that compacts, `zend_hash_iterators_clamp_max` where a
//! delete trims `nNumUsed`, and `zend_hash_iterators_remove` where the
//! table is destroyed. A table carries how many iterators name it in the
//! third byte of its flags, php's `u.v.nIteratorsCount`, so a table no
//! walk names costs a write one test and nothing else.
//!
//! php keeps sixteen iterators inline in `EG` before it allocates
//! (`ht_iterators_slots`); this list is a vector the request owns beside
//! its heap, which `memory_get_usage()` does not read, and no program
//! nests sixteen walks to tell the two apart.

// A slot's index is php's `uint32_t`, and the list never holds more
// iterators than a request can nest walks; the narrowing casts are php's.
#![allow(clippy::cast_possible_truncation)]

use crate::Heap;
use crate::array::Arr;
use crate::hooks::Fatal;

/// What an iterator names: nothing, a table destroyed under it
/// (`HT_POISONED_PTR`), or a live table.
#[derive(Clone, Copy)]
enum Target {
    Free,
    Poisoned,
    Table(Arr),
}

impl Target {
    fn is(self, a: Arr) -> bool {
        matches!(self, Target::Table(t) if t.is_same(a))
    }
}

/// `HashTableIterator` (`Z/Zend/zend_types.h:570`): the table, the
/// position in it, and the next iterator of the ring of copies
/// `zend_array_dup` made of this one, itself when there are none.
///
/// `lead` is the one word php's struct has not: the declared property
/// slots an object's walk has passed. php's properties table carries an
/// object's declared slots as its first buckets, `IS_INDIRECT` to the
/// slots, and walks them with the same position; this heap keeps them
/// out of the table (design §5.4), so the table an object walk
/// registers against is the dynamic one and the walk's place among the
/// slots before it is kept here. No write of the table moves it, since
/// php's writes never compact the `IS_INDIRECT` buckets either: an
/// `unset` of a declared property leaves its bucket and makes the slot
/// `UNDEF`.
#[derive(Clone, Copy)]
struct HtIter {
    ht: Target,
    pos: u32,
    next_copy: u32,
    lead: u32,
}

/// `EG(ht_iterators)` and `EG(ht_iterators_used)`: every slot laid down
/// so far and the length of the prefix any iterator stands in.
#[derive(Debug, Default)]
pub(crate) struct Iterators {
    slots: Vec<HtIter>,
    used: u32,
}

impl std::fmt::Debug for HtIter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HtIter({}, {}, {})", self.pos, self.next_copy, self.lead)
    }
}

/// `HT_ITERATORS_COUNT` is the third byte of the flags word.
const COUNT_SHIFT: u32 = 16;
/// `HT_ITERATORS_OVERFLOW`: the count at 255 is no longer kept.
const OVERFLOW: u32 = 0xff;

impl Arr {
    /// `HT_ITERATORS_COUNT(ht)`.
    pub(crate) fn iterators_count(self) -> u32 {
        (self.flags() >> COUNT_SHIFT) & 0xff
    }

    /// `HT_HAS_ITERATORS(ht)`: a `foreach` is registered at the table,
    /// which is what `zlo_is_iterating` refuses a reset over.
    #[must_use]
    pub fn has_iterators(self) -> bool {
        self.iterators_count() != 0
    }

    fn set_iterators_count(self, count: u32) {
        let flags = self.flags() & !(0xff << COUNT_SHIFT);
        self.set_flags(flags | (count << COUNT_SHIFT));
    }

    /// `HT_INC_ITERATORS_COUNT` under `!HT_ITERATORS_OVERFLOW`. An
    /// immutable table is shared by every thread and takes no count: php
    /// separates one before any walk registers against it, and so does
    /// the engine, so the guard only keeps a published unit's bytes
    /// unwritten.
    fn inc_iterators(self) {
        let count = self.iterators_count();
        if count != OVERFLOW && !self.is_immutable() {
            self.set_iterators_count(count + 1);
        }
    }

    /// `HT_DEC_ITERATORS_COUNT` under `!HT_ITERATORS_OVERFLOW`.
    fn dec_iterators(self) {
        let count = self.iterators_count();
        if count != OVERFLOW && count != 0 && !self.is_immutable() {
            self.set_iterators_count(count - 1);
        }
    }
}

impl Iterators {
    fn get(&self, idx: u32) -> Option<&HtIter> {
        self.slots.get(idx as usize)
    }

    fn get_mut(&mut self, idx: u32) -> Option<&mut HtIter> {
        self.slots.get_mut(idx as usize)
    }

    /// The iterators in the used prefix, as the walks over
    /// `EG(ht_iterators_used)` read them.
    fn in_use(&mut self) -> impl Iterator<Item = &mut HtIter> {
        let used = self.used as usize;
        self.slots.iter_mut().take(used)
    }

    /// `zend_hash_iterator_add` (`Z/Zend/zend_hash.c:536`): the first free
    /// slot, or eight more laid down, given `a` and `pos`, the table's
    /// count one more.
    pub(crate) fn add(&mut self, a: Arr, pos: u32) -> u32 {
        a.inc_iterators();
        let free = self
            .slots
            .iter()
            .position(|iter| matches!(iter.ht, Target::Free));
        let idx = if let Some(idx) = free {
            idx as u32
        } else {
            let idx = self.slots.len() as u32;
            let blank = HtIter {
                ht: Target::Free,
                pos: 0,
                next_copy: 0,
                lead: 0,
            };
            self.slots.extend((0..8).map(|i| HtIter {
                next_copy: idx + i,
                ..blank
            }));
            idx
        };
        if let Some(iter) = self.get_mut(idx) {
            *iter = HtIter {
                ht: Target::Table(a),
                pos,
                next_copy: idx,
                lead: 0,
            };
        }
        if idx + 1 > self.used {
            self.used = idx + 1;
        }
        idx
    }

    /// `zend_hash_remove_iterator_copies` (`Z/Zend/zend_hash.c:521`):
    /// every copy on `idx`'s ring deleted, and the ring closed on `idx`.
    fn remove_copies(&mut self, idx: u32) {
        let Some(mut next) = self.get(idx).map(|iter| iter.next_copy) else {
            return;
        };
        while next != idx {
            let cur = next;
            let Some(iter) = self.get_mut(cur) else {
                break;
            };
            next = iter.next_copy;
            // `cur_iter->next_copy = cur_idx`, so the delete does not
            // walk the ring again.
            iter.next_copy = cur;
            self.del(cur);
        }
        if let Some(iter) = self.get_mut(idx) {
            iter.next_copy = idx;
        }
    }

    /// `zend_hash_iterator_find_copy_pos` (`Z/Zend/zend_hash.c:576`): the
    /// copy on `idx`'s ring that walks `a`, its table and position
    /// taken over by `idx` and the ring dropped; `false`, with the ring
    /// dropped all the same, where no copy walks `a`.
    fn find_copy_pos(&mut self, idx: u32, a: Arr) -> bool {
        let Some(iter) = self.get(idx).copied() else {
            return false;
        };
        let mut next = iter.next_copy;
        if next == idx {
            return false;
        }
        while next != idx {
            let Some(copy) = self.get(next).copied() else {
                break;
            };
            if copy.ht.is(a) {
                if let Target::Table(old) = iter.ht {
                    old.dec_iterators();
                }
                a.inc_iterators();
                if let Some(iter) = self.get_mut(idx) {
                    iter.ht = copy.ht;
                    iter.pos = copy.pos;
                    iter.lead = copy.lead;
                }
                self.remove_copies(idx);
                return true;
            }
            next = copy.next_copy;
        }
        self.remove_copies(idx);
        false
    }

    /// `zend_hash_iterator_pos` (`Z/Zend/zend_hash.c:607`): the position
    /// iterator `idx` stands at in `a`. An iterator that named another
    /// table, one the loop's subject was replaced by, is moved onto `a`,
    /// at the copy's position where `zend_array_dup` made `a` from the
    /// table it walked, and at `a`'s internal pointer otherwise, which
    /// for an object's table is its place among the declared slots
    /// (`HtIter::lead`) as well as in its own buckets.
    pub(crate) fn pos(&mut self, idx: u32, a: Arr) -> u32 {
        let Some(iter) = self.get(idx).copied() else {
            return a.num_used();
        };
        if !iter.ht.is(a) && !self.find_copy_pos(idx, a) {
            if let Target::Table(old) = iter.ht {
                old.dec_iterators();
            }
            a.inc_iterators();
            if let Some(iter) = self.get_mut(idx) {
                iter.ht = Target::Table(a);
                iter.pos = a.position();
                iter.lead = a.lead();
            }
        }
        self.get(idx).map_or(a.num_used(), |iter| iter.pos)
    }

    /// `EG(ht_iterators)[idx].pos = pos`.
    pub(crate) fn set_pos(&mut self, idx: u32, pos: u32) {
        if let Some(iter) = self.get_mut(idx) {
            iter.pos = pos;
        }
    }

    /// `zend_hash_iterator_del` (`Z/Zend/zend_hash.c:655`): the slot
    /// free, the table's count one fewer, the ring of copies deleted, and
    /// the used prefix trimmed past the free slots at its end.
    pub(crate) fn del(&mut self, idx: u32) {
        let Some(iter) = self.get(idx).copied() else {
            return;
        };
        if let Target::Table(a) = iter.ht {
            a.dec_iterators();
        }
        if let Some(slot) = self.get_mut(idx) {
            slot.ht = Target::Free;
        }
        if iter.next_copy != idx {
            self.remove_copies(idx);
        }
        if idx + 1 == self.used {
            let mut at = idx;
            while at > 0
                && self
                    .get(at - 1)
                    .is_some_and(|i| matches!(i.ht, Target::Free))
            {
                at -= 1;
            }
            self.used = at;
        }
    }

    /// `_zend_hash_iterators_remove` (`Z/Zend/zend_hash.c:680`): every
    /// iterator on `a`, which is being destroyed, poisoned.
    pub(crate) fn remove(&mut self, a: Arr) {
        if !a.has_iterators() {
            return;
        }
        for iter in self.in_use() {
            if iter.ht.is(a) {
                iter.ht = Target::Poisoned;
            }
        }
    }

    /// `zend_hash_iterators_lower_pos` (`Z/Zend/zend_hash.c:700`): the
    /// lowest position at or after `start` an iterator on `a` stands at,
    /// `nNumUsed` where none does.
    pub(crate) fn lower_pos(&mut self, a: Arr, start: u32) -> u32 {
        let mut res = a.num_used();
        for iter in self.in_use() {
            if iter.ht.is(a) && iter.pos >= start && iter.pos < res {
                res = iter.pos;
            }
        }
        res
    }

    /// `zend_hash_iterators_update` (`Z/Zend/zend_hash.h:366`): every
    /// iterator on `a` at `from` moved to `to`.
    pub(crate) fn update(&mut self, a: Arr, from: u32, to: u32) {
        if !a.has_iterators() {
            return;
        }
        for iter in self.in_use() {
            if iter.ht.is(a) && iter.pos == from {
                iter.pos = to;
            }
        }
    }

    /// Every iterator on `a` at `to`, which is what the rehash of an
    /// emptied table writes (`Z/Zend/zend_hash.c:1347`).
    pub(crate) fn reset(&mut self, a: Arr) {
        if !a.has_iterators() {
            return;
        }
        for iter in self.in_use() {
            if iter.ht.is(a) {
                iter.pos = 0;
            }
        }
    }

    /// `zend_hash_iterators_clamp_max` (`Z/Zend/zend_hash.c:1443`): no
    /// iterator on `a` past `max`.
    pub(crate) fn clamp_max(&mut self, a: Arr, max: u32) {
        if !a.has_iterators() {
            return;
        }
        for iter in self.in_use() {
            if iter.ht.is(a) {
                iter.pos = iter.pos.min(max);
            }
        }
    }

    /// `zend_array_dup_ht_iterators` (`Z/Zend/zend_hash.c:2353`): an
    /// iterator on `target` beside each one on `source`, at its
    /// position, linked into its ring, so a walk whose table is copied
    /// on write finds the copy it should go on in.
    pub(crate) fn dup(&mut self, source: Arr, target: Arr) {
        if !source.has_iterators() {
            return;
        }
        let end = self.used;
        for at in 0..end {
            let Some(iter) = self.get(at).copied() else {
                continue;
            };
            if !iter.ht.is(source) {
                continue;
            }
            let copy = self.add(target, iter.pos);
            if let Some(slot) = self.get_mut(copy) {
                slot.next_copy = iter.next_copy;
                slot.lead = iter.lead;
            }
            if let Some(slot) = self.get_mut(at) {
                slot.next_copy = copy;
            }
        }
    }
}

impl Heap {
    /// `zend_hash_iterator_add` (`Z/Zend/zend_hash.c:536`): an iterator
    /// over `a` at `pos`, its index the `Z_FE_ITER` word of the loop's
    /// variable.
    pub fn iterator_add(&mut self, a: Arr, pos: u32) -> u32 {
        self.iterators.add(a, pos)
    }

    /// `zend_hash_iterator_pos` (`Z/Zend/zend_hash.c:607`): where
    /// iterator `idx` stands in `a`, the iterator moved onto `a` where
    /// it named another table.
    pub fn iterator_pos(&mut self, idx: u32, a: Arr) -> u32 {
        self.iterators.pos(idx, a)
    }

    /// `zend_hash_iterator_pos_ex` (`Z/Zend/zend_hash.c:626`), what
    /// `FE_FETCH_RW` reads over an array: [`Heap::iterator_pos`], but an
    /// iterator that named another table separates `a` first where it is
    /// shared, `SEPARATE_ARRAY`, before it registers on it. The array
    /// handed back is what the loop's reference must hold from here, `a`
    /// itself or its copy of one owner with `a` one owner fewer.
    pub fn iterator_pos_ex(&mut self, idx: u32, a: Arr) -> Result<(u32, Arr), Fatal> {
        let Some(iter) = self.iterators.get(idx).copied() else {
            return Ok((a.num_used(), a));
        };
        if iter.ht.is(a) || self.iterators.find_copy_pos(idx, a) {
            return Ok((self.iterators.get(idx).map_or(0, |i| i.pos), a));
        }
        // php counts the iterator off its old table before it copies
        // (`Z/Zend/zend_hash.c:633-643`) and leaves by a jump where the
        // copy fails; here the copy comes first, so a copy the limit
        // refuses leaves the iterator counted on the table it stands on,
        // and that table's destruction poisons it.
        let mut held = a;
        if a.refcount() > 1 || a.is_immutable() {
            held = self.array_dup(a)?;
            let _ = a.delref();
        }
        if let Target::Table(old) = iter.ht {
            old.dec_iterators();
        }
        held.inc_iterators();
        let pos = held.position();
        if let Some(iter) = self.iterators.get_mut(idx) {
            iter.ht = Target::Table(held);
            iter.pos = pos;
            iter.lead = held.lead();
        }
        Ok((pos, held))
    }

    /// Whether iterator `idx` still names `a`: the table it was
    /// registered at or moved onto stands, neither destroyed under it
    /// (`HT_POISONED_PTR`) nor left for another. The test reads the
    /// iterator alone, never `a`, so it is sound where `a` was freed.
    #[must_use]
    pub fn iterator_on(&self, idx: u32, a: Arr) -> bool {
        self.iterators.get(idx).is_some_and(|iter| iter.ht.is(a))
    }

    /// `EG(ht_iterators)[idx].pos = pos`: where the fetch leaves the
    /// walk.
    pub fn iterator_set_pos(&mut self, idx: u32, pos: u32) {
        self.iterators.set_pos(idx, pos);
    }

    /// The declared property slots an object's walk has passed before
    /// the dynamic table the iterator stands in (`HtIter::lead`).
    #[must_use]
    pub fn iterator_lead(&self, idx: u32) -> u32 {
        self.iterators.get(idx).map_or(0, |iter| iter.lead)
    }

    /// Where an object's walk leaves its place among the declared slots.
    pub fn iterator_set_lead(&mut self, idx: u32, lead: u32) {
        if let Some(iter) = self.iterators.get_mut(idx) {
            iter.lead = lead;
        }
    }

    /// `zend_hash_iterator_del` (`Z/Zend/zend_hash.c:655`): the iterator
    /// given up, where the loop's `FE_FREE` or the unwind frees its
    /// variable.
    pub fn iterator_del(&mut self, idx: u32) {
        self.iterators.del(idx);
    }

    /// `zend_hash_iterators_lower_pos` (`Z/Zend/zend_hash.c:700`): the
    /// lowest position at or after `start` an iterator on `a` stands at,
    /// `nNumUsed` where none does. What a native that rebuilds a table
    /// walks its buckets against, as `php_splice` does.
    pub fn iterators_lower_pos(&mut self, a: Arr, start: u32) -> u32 {
        self.iterators.lower_pos(a, start)
    }

    /// `zend_hash_iterators_update` (`Z/Zend/zend_hash.h:366`): every
    /// iterator on `a` at `from` moved to `to`.
    pub fn iterators_update(&mut self, a: Arr, from: u32, to: u32) {
        self.iterators.update(a, from, to);
    }

    /// `zend_hash_iterators_advance` (`Z/Zend/zend_hash.c:730`): every
    /// iterator on `a` `step` positions on, what `array_unshift` makes
    /// of the elements it puts before them.
    pub fn iterators_advance(&mut self, a: Arr, step: u32) {
        if !a.has_iterators() {
            return;
        }
        for iter in self.iterators.in_use() {
            if iter.ht.is(a) {
                iter.pos = iter.pos.wrapping_add(step);
            }
        }
    }

    /// The swap `array_unshift` and `php_splice` make of a table they
    /// rebuilt, `HT_SET_ITERATORS_COUNT(&new, HT_ITERATORS_COUNT(old))`
    /// and `HT_SET_ITERATORS_COUNT(old, 0)`: php writes the new table's
    /// data into the old one's header, so every iterator on `from` stands
    /// on `to` from here at the position it has.
    pub fn iterators_carry(&mut self, from: Arr, to: Arr) {
        if !from.has_iterators() || from.is_same(to) {
            return;
        }
        for iter in self.iterators.in_use() {
            if iter.ht.is(from) {
                iter.ht = Target::Table(to);
            }
        }
        if !to.is_immutable() {
            to.set_iterators_count(from.iterators_count());
        }
        from.set_iterators_count(0);
    }
}
