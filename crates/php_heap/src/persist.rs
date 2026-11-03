//! A published unit's one block (design §6.3): where the unit's tables
//! and the bytes of its names are laid at their length, one after
//! another, rather than one allocation each, as opcache lays a cached file.
//!
//! php sizes a cached file before it copies it: `zend_accel_script_persist_calc`
//! (`Z/ext/opcache/zend_persist_calc.c:632`) adds up every table the
//! file holds, each rounded to `ZEND_MM_ALIGNMENT` by `ADD_SIZE`, and
//! `zend_accel_script_persist` (`Z/ext/opcache/zend_persist.c:1473`)
//! copies each into the memory that size reserved, bumping `ZCG(mem)`
//! past it. A [`PersistSize`] is the first pass and a [`Persist`] the
//! second: the block is one allocation of the size the first counted.
//! A table is laid in the block's area for its alignment, eight, sixteen
//! or thirty-two, each table rounded to it, the widest area first, so
//! every area starts aligned; a name's bytes are laid from the block's
//! back. So neither pass depends on the order the other walked in. A
//! table the size did not count, or one whose element needs more than
//! thirty-two, is copied into an allocation of its own at its length,
//! which costs the unit its rounding and nothing else.
//!
//! The block is shared, as opcache's memory is: each [`Table`] laid in
//! it, and each [`Bytes`] laid in it, holds one share, and the block
//! goes back to the allocator with the last. A unit's tables are read
//! by every request that serves it, so a share is counted atomically.
//! An element laid here is `Copy`, so nothing laid here is dropped: the
//! block is freed whole, as opcache's `zend_accel_discard_script` frees
//! a cached file's memory without visiting what is in it.

use std::alloc::Layout;
use std::marker::PhantomData;
use std::mem::{align_of, size_of};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicUsize, Ordering, fence};

use crate::Bytes;

/// The alignments a table is laid at, one area of the block each, the
/// widest first: an opline's, sixteen, and php's `ZEND_MM_ALIGNMENT`,
/// which every narrower element is laid at. The widest is the block's
/// own alignment.
const AREAS: [usize; 3] = [32, 16, 8];

/// The block's alignment, its widest area's.
const ALIGN: usize = AREAS[0];

/// The area a table of `T` is laid in, the narrowest that aligns it.
fn area_of<T>() -> Option<usize> {
    AREAS.iter().rposition(|&align| align_of::<T>() <= align)
}

/// The block's header, before what is laid in it.
#[repr(C)]
struct Header {
    /// How many tables, byte strings and [`Persist`]s name the block.
    shares: AtomicUsize,
    /// The block's size, the header's among it.
    size: usize,
}

/// Where a block's first table is laid.
const FIRST: usize = size_of::<Header>().next_multiple_of(ALIGN);

/// The top bit of a [`Table`]'s `cap`: set where the table is laid in a
/// block, the rest then the byte offset of its first element from the
/// block's start. No vector's capacity reaches it but a zero-sized
/// element's, which is never laid.
const LAID: usize = 1 << (usize::BITS - 1);

/// One share more of the block at `base`.
pub(crate) fn retain(base: NonNull<u8>) {
    // SAFETY: the block holds its header at its start while a share is
    // held, and the caller holds one.
    let shares = unsafe { &base.cast::<Header>().as_ref().shares };
    if shares.fetch_add(1, Ordering::Relaxed) > isize::MAX as usize {
        // As `Arc` refuses a count it cannot hold.
        std::process::abort();
    }
}

/// One share of the block at `base` given back, and the block with the
/// last.
pub(crate) fn release(base: NonNull<u8>) {
    let header = base.cast::<Header>();
    // SAFETY: the block holds its header at its start while a share is
    // held, and the caller holds one.
    let shares = unsafe { &header.as_ref().shares };
    if shares.fetch_sub(1, Ordering::Release) != 1 {
        return;
    }
    fence(Ordering::Acquire);
    // SAFETY: the last share is the caller's, so nothing else reads or
    // writes the header.
    let size = unsafe { header.as_ref().size };
    let Ok(layout) = Layout::from_size_align(size, ALIGN) else {
        return;
    };
    // SAFETY: the block was allocated with this layout ([`Persist::new`])
    // and no share of it is left.
    unsafe { std::alloc::dealloc(base.as_ptr(), layout) };
}

/// The first pass, `zend_accel_script_persist_calc`'s: the size of the
/// block a unit's tables and names are laid in, each area's tables
/// rounded to its alignment as `ADD_SIZE` rounds every table to eight.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PersistSize {
    /// The bytes of each area's tables.
    areas: [usize; AREAS.len()],
    /// The bytes of the names.
    names: usize,
}

impl PersistSize {
    /// A table of `len` elements counted, where [`Persist::table`] would
    /// lay it in the block.
    pub fn table<T: Copy>(&mut self, len: usize) {
        if let Some((area, bytes)) = laid_size::<T>(len)
            && let Some(sum) = self.areas.get_mut(area)
        {
            *sum = sum.saturating_add(bytes);
        }
    }

    /// `len` bytes of a name counted, unrounded.
    pub fn bytes(&mut self, len: usize) {
        self.names = self.names.saturating_add(len);
    }

    /// What the passes counted, in bytes, the header's left out.
    #[must_use]
    pub fn get(self) -> usize {
        (self.areas.iter()).fold(self.names, |sum, &area| sum.saturating_add(area))
    }
}

/// The area a table of `len` elements is laid in and the bytes it takes
/// there, rounded to the area's alignment; none where it is not laid.
fn laid_size<T>(len: usize) -> Option<(usize, usize)> {
    if len == 0 || size_of::<T>() == 0 {
        return None;
    }
    let area = area_of::<T>()?;
    let align = *AREAS.get(area)?;
    Some((
        area,
        len.checked_mul(size_of::<T>())?
            .checked_next_multiple_of(align)?,
    ))
}

/// The second pass, `zend_accel_script_persist`'s: one block of the size
/// the first counted, and the tables and names copied into it, tables
/// from its front and names from its back. It holds one share of the
/// block until it is dropped.
#[derive(Debug)]
pub struct Persist {
    /// The block's start; none where nothing was counted or the
    /// allocator refused it, and then every table is a copy of its own.
    base: Option<NonNull<u8>>,
    /// The offset of the next table of each area.
    fronts: [usize; AREAS.len()],
    /// The offset past each area.
    ends: [usize; AREAS.len()],
    /// The offset past the last name laid, the block's size at first.
    back: usize,
    /// Where the names' room begins, past the last area.
    names: usize,
    /// How many tables and names were copied apart, not laid.
    apart: usize,
}

impl Persist {
    /// A block of the size `size` counted.
    #[must_use]
    pub fn new(size: PersistSize) -> Persist {
        let mut fronts = [FIRST; AREAS.len()];
        let mut ends = [FIRST; AREAS.len()];
        let mut at = FIRST;
        for ((front, end), &bytes) in fronts.iter_mut().zip(&mut ends).zip(&size.areas) {
            *front = at;
            at = at.saturating_add(bytes);
            *end = at;
        }
        let base = (size.get() > 0)
            .then(|| at.checked_add(size.names))
            .flatten()
            .and_then(|total| Some((Layout::from_size_align(total, ALIGN).ok()?, total)))
            .and_then(|(layout, total)| {
                // SAFETY: the layout's size is at least the header's,
                // never zero.
                let base = NonNull::new(unsafe { std::alloc::alloc(layout) })?;
                let header = Header {
                    shares: AtomicUsize::new(1),
                    size: total,
                };
                // SAFETY: the block is fresh, aligned for the header and
                // at least as large as it.
                unsafe { base.cast::<Header>().write(header) };
                Some((base, total))
            });
        Persist {
            base: base.map(|(base, _)| base),
            fronts,
            ends,
            back: base.map_or(0, |(_, total)| total),
            names: at,
            apart: 0,
        }
    }

    /// `items`, laid in the block where the first pass counted room for
    /// them, else copied into a table of their own at their length.
    pub fn table<T: Copy>(&mut self, items: &[T]) -> Table<T> {
        let Some(at) = self.room::<T>(items.len()) else {
            if !items.is_empty() {
                self.apart += 1;
            }
            return Table::from(items.to_vec());
        };
        let to = at.0.cast::<T>();
        // SAFETY: the room is `items.len()` elements of the block, aligned
        // for `T`, which nothing else names, and `items` is not in it.
        unsafe { to.copy_from_nonoverlapping(NonNull::from(items).cast(), items.len()) };
        retain(at.1);
        Table {
            at: to,
            len: items.len(),
            cap: LAID | at.2,
            owns: PhantomData,
        }
    }

    /// Room in the block for `len` elements of `T` in its area: where
    /// they go, the block and their offset in it.
    fn room<T>(&mut self, len: usize) -> Option<(NonNull<u8>, NonNull<u8>, usize)> {
        let base = self.base?;
        let (area, size) = laid_size::<T>(len)?;
        let front = self.fronts.get_mut(area)?;
        let offset = *front;
        let end = offset.checked_add(size)?;
        if end > *self.ends.get(area)? {
            return None;
        }
        *front = end;
        // SAFETY: `offset` is within the block, `end` not past its end.
        Some((unsafe { base.add(offset) }, base, offset))
    }

    /// `bytes`, laid in the block from its back where the first pass
    /// counted room for them, else copied into a box of their own.
    pub fn bytes(&mut self, bytes: &[u8]) -> Bytes {
        let Some(base) = self.base else {
            return self.apart_bytes(bytes);
        };
        let Some(offset) = self.back.checked_sub(bytes.len()) else {
            return self.apart_bytes(bytes);
        };
        if bytes.is_empty() || offset < self.names || !Bytes::layable(offset, bytes.len()) {
            return self.apart_bytes(bytes);
        }
        self.back = offset;
        // SAFETY: `offset` is within the block, `bytes.len()` past it not
        // past its end.
        let to = unsafe { base.add(offset) };
        // SAFETY: the room is `bytes.len()` bytes of the block that
        // nothing else names, and `bytes` is not in it.
        unsafe { to.copy_from_nonoverlapping(NonNull::from(bytes).cast(), bytes.len()) };
        retain(base);
        Bytes::laid(to, bytes.len(), offset)
    }

    fn apart_bytes(&mut self, bytes: &[u8]) -> Bytes {
        if !bytes.is_empty() {
            self.apart += 1;
        }
        Bytes::from(bytes)
    }

    /// How many tables and names were copied into allocations of their
    /// own rather than laid in the block: none where the first pass
    /// counted what the second laid.
    #[must_use]
    pub fn apart(&self) -> usize {
        self.apart
    }

    /// How many bytes of the block nothing was laid in.
    #[must_use]
    pub fn unused(&self) -> usize {
        let areas = (self.ends.iter().zip(&self.fronts)).map(|(end, front)| end - front);
        areas.sum::<usize>() + self.back.saturating_sub(self.names)
    }
}

impl Drop for Persist {
    /// The pass's share back: the block stays while a table or a name
    /// laid in it stands.
    fn drop(&mut self) {
        if let Some(base) = self.base {
            release(base);
        }
    }
}

/// A table at its length: a vector of its own while it is written, as
/// the compiler writes one, or laid in a published unit's block by
/// [`Persist::table`] and read through a share of it. Either reads as
/// the slice it holds, and either is a vector's size. Written once laid,
/// it is copied out of the block first, so a unit's block is never
/// written once laid.
pub struct Table<T> {
    at: NonNull<T>,
    len: usize,
    /// A vector's capacity, or [`LAID`] and the offset in the block.
    cap: usize,
    owns: PhantomData<T>,
}

// SAFETY: a table owns its elements as a vector does, or shares them,
// read-only, with the other tables of its block; either way handing it
// to another thread hands `T`s over and lets both threads read them.
unsafe impl<T: Send + Sync> Send for Table<T> {}
// SAFETY: a shared table is read, or cloned, which shares or copies `T`s.
unsafe impl<T: Send + Sync> Sync for Table<T> {}

impl<T> Table<T> {
    /// An empty table, which allocates nothing.
    #[must_use]
    pub const fn new() -> Table<T> {
        Table {
            at: NonNull::dangling(),
            len: 0,
            cap: 0,
            owns: PhantomData,
        }
    }

    /// Whether the table is laid in a block.
    #[must_use]
    pub fn is_laid(&self) -> bool {
        size_of::<T>() != 0 && self.cap & LAID != 0
    }

    /// The block the table is laid in.
    fn base(&self) -> Option<NonNull<u8>> {
        if !self.is_laid() {
            return None;
        }
        let offset = self.cap & !LAID;
        // The block's start, `offset` bytes before the first element.
        let at = self.at.as_ptr().cast::<u8>().wrapping_sub(offset);
        NonNull::new(at)
    }

    /// What the table holds room for: its length where it is laid.
    #[must_use]
    pub fn capacity(&self) -> usize {
        if self.is_laid() { self.len } else { self.cap }
    }
}

impl<T: Clone> Table<T> {
    /// The table as a vector to write, the table's own: a laid table is
    /// copied out of its block first. The table is the vector again when
    /// the guard goes.
    pub fn vec(&mut self) -> TableVec<'_, T> {
        let vec = Vec::from(std::mem::take(self));
        TableVec { table: self, vec }
    }

    /// The table's first `len` elements, the rest dropped; a laid table
    /// stays laid.
    pub fn truncate(&mut self, len: usize) {
        if self.is_laid() {
            self.len = self.len.min(len);
            return;
        }
        self.vec().truncate(len);
    }

    /// Every element dropped: a laid table's share of its block given
    /// back, a vector's room kept, as a vector keeps it.
    pub fn clear(&mut self) {
        if self.is_laid() {
            *self = Table::new();
        } else {
            self.vec().clear();
        }
    }

    /// `value` after the last element.
    pub fn push(&mut self, value: T) {
        self.vec().push(value);
    }

    /// The last element, taken out.
    pub fn pop(&mut self) -> Option<T> {
        self.vec().pop()
    }

    /// `value` at `at`, what was there and after it moved one on.
    pub fn insert(&mut self, at: usize, value: T) {
        self.vec().insert(at, value);
    }

    /// The element at `at` taken out, what was after it moved one back.
    pub fn remove(&mut self, at: usize) -> T {
        self.vec().remove(at)
    }

    /// Room for `more` elements past the length.
    pub fn reserve(&mut self, more: usize) {
        self.vec().reserve(more);
    }

    /// `items` after the last element.
    pub fn extend_from_slice(&mut self, items: &[T]) {
        self.vec().extend_from_slice(items);
    }

    /// The elements `keep` answers true for, in order.
    pub fn retain(&mut self, keep: impl FnMut(&T) -> bool) {
        self.vec().retain(keep);
    }

    /// The room past the length given back.
    pub fn shrink_to_fit(&mut self) {
        if !self.is_laid() {
            self.vec().shrink_to_fit();
        }
    }
}

impl<T> Default for Table<T> {
    fn default() -> Table<T> {
        Table::new()
    }
}

impl<T> Drop for Table<T> {
    /// A laid table's share of its block back, or the vector dropped.
    fn drop(&mut self) {
        if let Some(base) = self.base() {
            release(base);
            return;
        }
        // SAFETY: an unlaid table's parts are a vector's, which this
        // table is the one owner of.
        drop(unsafe { Vec::from_raw_parts(self.at.as_ptr(), self.len, self.cap) });
    }
}

impl<T> From<Vec<T>> for Table<T> {
    fn from(vec: Vec<T>) -> Table<T> {
        let mut vec = std::mem::ManuallyDrop::new(vec);
        Table {
            at: NonNull::new(vec.as_mut_ptr()).unwrap_or(NonNull::dangling()),
            len: vec.len(),
            cap: vec.capacity(),
            owns: PhantomData,
        }
    }
}

impl<T: Clone> From<Table<T>> for Vec<T> {
    /// The table's vector, handed back whole, or a copy of a laid one.
    fn from(table: Table<T>) -> Vec<T> {
        if table.is_laid() {
            return table.to_vec();
        }
        let table = std::mem::ManuallyDrop::new(table);
        // SAFETY: an unlaid table's parts are a vector's, and the table
        // is forgotten, so the vector has one owner.
        unsafe { Vec::from_raw_parts(table.at.as_ptr(), table.len, table.cap) }
    }
}

impl<T> std::ops::Deref for Table<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        // SAFETY: `at` holds `len` initialised elements, the vector's or
        // the block's, the block kept by this table's share.
        unsafe { std::slice::from_raw_parts(self.at.as_ptr(), self.len) }
    }
}

impl<T: Clone> std::ops::DerefMut for Table<T> {
    /// The elements to change in place, a laid table's copied out of its
    /// block first.
    fn deref_mut(&mut self) -> &mut [T] {
        if self.is_laid() {
            let owned = Vec::from(std::mem::take(self));
            *self = Table::from(owned);
        }
        // SAFETY: an unlaid table's `at` holds `len` initialised elements
        // of its vector, which this table owns, borrowed mutably here.
        unsafe { std::slice::from_raw_parts_mut(self.at.as_ptr(), self.len) }
    }
}

impl<T: Clone> Clone for Table<T> {
    /// One share more of a laid table's block; a copy of an unlaid one.
    fn clone(&self) -> Table<T> {
        let Some(base) = self.base() else {
            return Table::from(self.to_vec());
        };
        retain(base);
        Table {
            at: self.at,
            len: self.len,
            cap: self.cap,
            owns: PhantomData,
        }
    }
}

impl<T> FromIterator<T> for Table<T> {
    fn from_iter<I: IntoIterator<Item = T>>(items: I) -> Table<T> {
        Table::from(items.into_iter().collect::<Vec<T>>())
    }
}

impl<T: Clone> Extend<T> for Table<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, items: I) {
        self.vec().extend(items);
    }
}

impl<'a, T: Copy + 'a> Extend<&'a T> for Table<T> {
    fn extend<I: IntoIterator<Item = &'a T>>(&mut self, items: I) {
        self.vec().extend(items);
    }
}

impl<'a, T> IntoIterator for &'a Table<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> std::slice::Iter<'a, T> {
        self.iter()
    }
}

impl<'a, T: Clone> IntoIterator for &'a mut Table<T> {
    type Item = &'a mut T;
    type IntoIter = std::slice::IterMut<'a, T>;

    fn into_iter(self) -> std::slice::IterMut<'a, T> {
        self.iter_mut()
    }
}

impl<T: Clone> IntoIterator for Table<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> std::vec::IntoIter<T> {
        Vec::from(self).into_iter()
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for Table<T> {
    /// As the slice prints, laid or not.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        <[T] as std::fmt::Debug>::fmt(self, f)
    }
}

impl<T: PartialEq> PartialEq for Table<T> {
    fn eq(&self, other: &Table<T>) -> bool {
        **self == **other
    }
}

impl<T: Eq> Eq for Table<T> {}

impl<T: std::hash::Hash> std::hash::Hash for Table<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (**self).hash(state);
    }
}

/// A [`Table`] written as the vector it is, by [`Table::vec`]; the table
/// takes the vector back when this goes.
pub struct TableVec<'a, T> {
    table: &'a mut Table<T>,
    vec: Vec<T>,
}

impl<T> std::ops::Deref for TableVec<'_, T> {
    type Target = Vec<T>;

    fn deref(&self) -> &Vec<T> {
        &self.vec
    }
}

impl<T> std::ops::DerefMut for TableVec<'_, T> {
    fn deref_mut(&mut self) -> &mut Vec<T> {
        &mut self.vec
    }
}

impl<T> Drop for TableVec<'_, T> {
    fn drop(&mut self) {
        *self.table = Table::from(std::mem::take(&mut self.vec));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn laid<T: Copy>(tables: &[&[T]], names: &[&[u8]]) -> (Vec<Table<T>>, Vec<Bytes>, usize) {
        let mut size = PersistSize::default();
        for t in tables {
            size.table::<T>(t.len());
        }
        for n in names {
            size.bytes(n.len());
        }
        let mut persist = Persist::new(size);
        let tables = tables.iter().map(|t| persist.table(t)).collect();
        let names = names.iter().map(|n| persist.bytes(n)).collect();
        assert_eq!(
            persist.unused(),
            0,
            "the second pass filled what the first counted"
        );
        (tables, names, persist.apart())
    }

    /// Two passes lay every table and name in one block, each read as
    /// what it was copied from, none apart; a clone of a laid one shares
    /// the block; and the block outlives the pass, every table and name
    /// laid in it read after the pass has gone, and goes with the last
    /// of them.
    #[test]
    fn a_units_tables_and_names_are_laid_in_one_block_its_size() {
        let (tables, names, apart) =
            laid::<u32>(&[&[1, 2, 3], &[4], &[]], &[b"$this", b"/srv/a.php"]);
        assert_eq!(apart, 0, "the first pass counted what the second laid");
        assert!(tables[0].is_laid() && tables[1].is_laid());
        assert!(!tables[2].is_laid(), "an empty table takes no room");
        assert_eq!(&*tables[0], &[1, 2, 3]);
        assert_eq!(&*tables[1], &[4]);
        let (first, second) = (tables[0].as_ptr().addr(), tables[1].as_ptr().addr());
        assert_eq!(
            second - first,
            16,
            "the next table at the next multiple of eight"
        );
        assert_eq!(&*names[0], b"$this");
        assert_eq!(&*names[1], b"/srv/a.php");
        let base = tables[0].base().map(NonNull::addr);
        assert_eq!(tables[1].base().map(NonNull::addr), base, "one block");
        let shared = tables[0].clone();
        assert_eq!(
            shared.as_ptr(),
            tables[0].as_ptr(),
            "a clone shares the block"
        );
        let name = names[1].clone();
        assert_eq!(name.as_ptr(), names[1].as_ptr());
        drop(tables);
        drop(names);
        assert_eq!(&*shared, &[1, 2, 3], "the block stays while a share does");
        assert_eq!(&*name, b"/srv/a.php");
    }

    /// A table is laid in its alignment's area of the one block, the
    /// widest first, whatever order the passes walk in.
    #[test]
    fn each_table_is_laid_aligned_in_its_areas_room() {
        #[derive(Clone, Copy, Debug, PartialEq)]
        #[repr(C, align(32))]
        struct Wide(u8);
        let mut size = PersistSize::default();
        size.table::<u8>(3);
        size.table::<Wide>(2);
        size.table::<u128>(1);
        size.bytes(5);
        let mut persist = Persist::new(size);
        let narrow = persist.table(&[1u8, 2, 3]);
        let name = persist.bytes(b"label");
        let wide = persist.table(&[Wide(1), Wide(2)]);
        let middle = persist.table(&[7u128]);
        assert_eq!((persist.apart(), persist.unused()), (0, 0));
        assert_eq!(wide.as_ptr().addr() % 32, 0);
        assert_eq!(middle.as_ptr().addr() % align_of::<u128>(), 0);
        let base = wide.base().map(NonNull::addr);
        assert_eq!(narrow.base().map(NonNull::addr), base);
        assert!(wide.as_ptr().addr() < middle.as_ptr().addr());
        assert!(middle.as_ptr().addr() < narrow.as_ptr().addr());
        assert!(narrow.as_ptr().addr() < name.as_ptr().addr());
        assert_eq!(
            (&*wide, &*middle, &*narrow),
            (&[Wide(1), Wide(2)][..], &[7][..], &[1, 2, 3][..])
        );
    }

    /// A table written once laid is copied out of its block, which is
    /// never written, and the other shares read what was laid; a table
    /// the first pass did not count is a copy of its own at its length.
    #[test]
    fn a_laid_table_written_is_copied_out_and_an_uncounted_one_apart() {
        let mut size = PersistSize::default();
        size.table::<u64>(2);
        let mut persist = Persist::new(size);
        let mut table = persist.table(&[7u64, 8]);
        let other = table.clone();
        let extra = persist.table(&[9u64]);
        assert_eq!(persist.apart(), 1, "the first pass counted two elements");
        assert!(!extra.is_laid());
        assert_eq!((extra.capacity(), &*extra), (1, &[9][..]));
        drop(persist);
        table.push(10);
        assert!(!table.is_laid());
        assert_eq!(&*table, &[7, 8, 10]);
        assert_eq!(&*other, &[7, 8]);
        let mut changed = other.clone();
        changed[0] = 1;
        assert_eq!((&*changed, &*other), (&[1, 8][..], &[7, 8][..]));
        let mut shorter = other.clone();
        shorter.truncate(1);
        assert!(shorter.is_laid(), "a laid table cut short stays laid");
        assert_eq!(&*shorter, &[7]);
        shorter.clear();
        assert!(shorter.is_empty() && !shorter.is_laid());
    }

    /// An unlaid table is the vector it was made from, written through
    /// its guard and handed back whole; and a table is a vector's size.
    #[test]
    fn an_unlaid_table_is_its_vector() {
        let mut table = Table::from(Vec::with_capacity(4));
        table.push(1u8);
        table.extend([2, 3]);
        table.vec().insert(0, 0);
        assert_eq!((&*table, table.capacity()), (&[0, 1, 2, 3][..], 4));
        assert_eq!(table.remove(0), 0);
        table.retain(|&b| b != 2);
        let at = table.as_ptr();
        let vec = Vec::from(table);
        assert_eq!((vec.as_ptr(), &vec[..]), (at, &[1, 3][..]));
        let collected: Table<String> = ["a".to_owned()].into_iter().collect();
        assert_eq!(collected.clone(), collected);
        assert_eq!(size_of::<Table<u64>>(), size_of::<Vec<u64>>());
        assert_eq!(Table::<()>::new().capacity(), 0);
    }

    /// A unit's tables are read on every thread that serves it: the
    /// last share given back on another thread frees the block once.
    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "a test starts its own threads (seams.toml rule fork.threads)"
    )]
    fn a_block_shared_across_threads_goes_with_its_last_share() {
        let (tables, names, _) = laid::<u16>(&[&[1, 2]], &[b"name"]);
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let (table, name) = (tables[0].clone(), names[0].clone());
                std::thread::spawn(move || (table[1], name.len()))
            })
            .collect();
        drop((tables, names));
        for handle in handles {
            assert_eq!(handle.join().ok(), Some((2, 4)));
        }
    }
}
