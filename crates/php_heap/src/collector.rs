//! The cycle collector (design §5.12): `Zend/zend_gc.c` at the tag,
//! Bacon and Rajan's synchronous collector as php runs it. A value whose
//! refcount a release takes to a number above zero may be the last
//! outside owner of a cycle, so the release puts it in the root buffer
//! (`gc_possible_root`) coloured purple; when the buffer reaches its
//! threshold, or `gc_collect_cycles()` asks, the collector subtracts the
//! owners the values in the buffer give each other (mark grey), gives
//! back what an owner outside still holds (scan, scan black), and what
//! is left with no owner is garbage (collect white): its destructors
//! served first, the collection repeated once when any were, then every member
//! freed.
//!
//! **What php's header gives the collector, this header gives it.** The
//! colour and the buffer index are the top twenty-two bits of the
//! refcounted word, `GC_INFO`, exactly where `zend_refcounted_h` keeps
//! them (design revision 21), so a value is in the buffer when its
//! address is not zero and `gc_status()`'s `roots` counts the same
//! values php's does. The buffer itself is php's `pemalloc`, outside the
//! request's accounting, and is the request's alone: one request's
//! collection never touches another's heap.
//!
//! **Where the walk leaves the heap.** An array's values and a
//! reference's value are the heap's to walk. An object's are its
//! declared slots and its dynamic table, which the heap walks, and what
//! its handler table's `get_gc` names beside them, which the heap asks
//! for through [`HeapHooks::gc_children`]; the destructors are served
//! through [`HeapHooks::destruct`], and whether an object has one to
//! serve is [`HeapHooks::has_destructor`]. Nothing else of the engine is
//! reached.
//!
//! The walks are php's depth-first walks with an explicit stack: of the
//! children a value names, the first one the phase newly colours is the
//! value walked next (php's `goto tail_call`) and the rest are pushed in
//! order, which makes the order the garbage is found in, and so the
//! order its destructors are served in and its ids go back on the free
//! list, php's.

use std::time::{Duration, Instant};

use crate::array::Arr;
use crate::gc::{self, Colour, Counted};
use crate::hooks::{GcTable, HeapHooks};
use crate::object::Obj;
use crate::reference::Ref;
use crate::zval::WeakMarks;
use crate::{Heap, Zval, ZvalRef, ZvalType};

/// `GC_FIRST_ROOT`: the first index of the root buffer, zero being
/// the address a value in no entry reads.
const FIRST_ROOT: u32 = 1;
/// `GC_DEFAULT_BUF_SIZE`: the entries `gc_enable` lays down.
const DEFAULT_BUF_SIZE: u32 = 16 * 1024;
/// `GC_BUF_GROW_STEP`.
const BUF_GROW_STEP: u32 = 128 * 1024;
/// `GC_MAX_UNCOMPRESSED`: past this index the twenty bits of the
/// address hold the index modulo 2^19 with the 2^19 bit set.
const MAX_UNCOMPRESSED: u32 = 512 * 1024;
/// `GC_MAX_BUF_SIZE`.
const MAX_BUF_SIZE: u32 = 0x4000_0000;
/// `GC_THRESHOLD_DEFAULT`: ten thousand roots past the first index.
const THRESHOLD_DEFAULT: u32 = 10_000 + FIRST_ROOT;
/// `GC_THRESHOLD_STEP`.
const THRESHOLD_STEP: u32 = 10_000;
/// `GC_THRESHOLD_MAX`.
const THRESHOLD_MAX: u32 = 1_000_000_000;
/// `GC_THRESHOLD_TRIGGER`: a collection that frees fewer than this many values
/// raises the threshold.
const THRESHOLD_TRIGGER: i64 = 100;

/// One value the collector walks: a table, an object, or a reference
/// the walk passes through. php's `zend_refcounted *` with its type
/// read off the header.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Node {
    Arr(Arr),
    Obj(Obj),
    Ref(Ref),
}

impl Node {
    /// `Z_COLLECTABLE_P(zv)` and `Z_COUNTED_P(zv)`: the value a zval
    /// names when it is one a cycle can pass through.
    fn of(z: Zval) -> Option<Node> {
        if !z.is_collectable() {
            return None;
        }
        if let Some(a) = z.as_arr() {
            Some(Node::Arr(a))
        } else if let Some(o) = z.as_obj() {
            Some(Node::Obj(o))
        } else {
            z.as_reference().map(Node::Ref)
        }
    }

    fn gc(self) -> Counted {
        match self {
            Node::Arr(a) => a.gc(),
            Node::Obj(o) => o.gc(),
            Node::Ref(r) => r.gc(),
        }
    }

    fn is_same(self, other: Node) -> bool {
        match (self, other) {
            (Node::Arr(a), Node::Arr(b)) => a.is_same(b),
            (Node::Obj(a), Node::Obj(b)) => a.is_same(b),
            (Node::Ref(a), Node::Ref(b)) => a.is_same(b),
            _ => false,
        }
    }

    fn is_reference(self) -> bool {
        matches!(self, Node::Ref(_))
    }
}

/// One entry of `GC_G(buf)`: php's tagged pointer decoded, its low two
/// bits the entry's kind (`GC_ROOT`, `GC_UNUSED`, `GC_GARBAGE`,
/// `GC_DTOR_GARBAGE`) and, for an unused one, the next unused index in
/// the bits above them, none at the end of the list.
#[derive(Clone, Copy, Debug)]
enum Entry {
    Root(Node),
    Unused(Option<u32>),
    Garbage(Node),
    DtorGarbage(Obj),
}

/// The root buffer as `gc_enable` lays it down: php's `buf` with its
/// `unused` list, `first_unused`, `gc_threshold` and `buf_size`. The
/// entries below `first_unused` are held from index one on; the ones
/// above it are php's allocated and unwritten, and are not laid down.
#[derive(Debug)]
struct Buffer {
    /// The entries from `GC_FIRST_ROOT` to `first_unused`, index `i`
    /// at `entries[i - 1]`.
    entries: Vec<Entry>,
    /// The head of the unused list.
    unused: Option<u32>,
    threshold: u32,
    size: u32,
}

impl Buffer {
    /// `gc_enable`'s buffer: the default size and threshold, empty.
    fn new() -> Buffer {
        Buffer {
            entries: Vec::new(),
            unused: None,
            threshold: THRESHOLD_DEFAULT,
            size: DEFAULT_BUF_SIZE,
        }
    }

    /// `first_unused`: the first index never written.
    fn first_unused(&self) -> u32 {
        FIRST_ROOT + u32::try_from(self.entries.len()).unwrap_or(u32::MAX - FIRST_ROOT)
    }

    fn entry(&self, idx: u32) -> Option<Entry> {
        let at = idx.checked_sub(FIRST_ROOT)? as usize;
        self.entries.get(at).copied()
    }

    fn set(&mut self, idx: u32, entry: Entry) {
        if let Some(at) = idx
            .checked_sub(FIRST_ROOT)
            .and_then(|at| self.entries.get_mut(at as usize))
        {
            *at = entry;
        }
    }

    /// `gc_fetch_unused`: the head of the unused list, taken off it.
    fn fetch_unused(&mut self) -> Option<u32> {
        let idx = self.unused?;
        self.unused = match self.entry(idx) {
            Some(Entry::Unused(next)) => next,
            _ => None,
        };
        Some(idx)
    }

    /// `gc_link_unused`: the entry onto the unused list.
    fn link_unused(&mut self, idx: u32) {
        self.set(idx, Entry::Unused(self.unused));
        self.unused = Some(idx);
    }

    /// `gc_fetch_next_unused`: the first index never written.
    fn fetch_next_unused(&mut self) -> u32 {
        let idx = self.first_unused();
        self.entries.push(Entry::Unused(None));
        idx
    }

    /// `gc_decompress`: the entry holding `node`, whose header keeps
    /// its index modulo 2^19 past the uncompressed range.
    fn decompress(&self, node: Node, mut idx: u32) -> Option<u32> {
        loop {
            if entry_node(self.entry(idx)?).is_some_and(|n| n.is_same(node)) {
                return Some(idx);
            }
            idx += MAX_UNCOMPRESSED;
        }
    }
}

/// `zend_gc_globals`, request-scoped: the root buffer and the numbers
/// `gc_status()` reports.
#[derive(Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "php's `zend_gc_globals` keeps these four flags as `bool`s, and `gc_status()` reports three of them"
)]
pub(crate) struct Collector {
    /// `buf`, none until the first `gc_enable` lays it down.
    buf: Option<Buffer>,
    enabled: bool,
    active: bool,
    protected: bool,
    full: bool,
    num_roots: u32,
    runs: u32,
    collected: u32,
    activated_at: Instant,
    collecting: Duration,
    destructing: Duration,
    freeing: Duration,
}

/// What `gc_status()` reads, `zend_gc_status`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GcStatus {
    /// `running`: a collection is under way.
    pub running: bool,
    /// `protected`: no value may be added to the buffer.
    pub protected: bool,
    /// `full`: the buffer reached its largest size and the collector
    /// turned itself off.
    pub full: bool,
    /// `runs`.
    pub runs: u32,
    /// `collected`: the values freed, strings and resources not among
    /// them (php 8.5).
    pub collected: u32,
    /// `threshold`, zero while there is no buffer.
    pub threshold: u32,
    /// `buffer_size`, zero while there is no buffer.
    pub buffer_size: u32,
    /// `roots`.
    pub roots: u32,
    /// `application_time`, since the request began.
    pub application_time: Duration,
    /// `collector_time`.
    pub collector_time: Duration,
    /// `destructor_time`.
    pub destructor_time: Duration,
    /// `free_time`.
    pub free_time: Duration,
}

impl Collector {
    /// `gc_globals_ctor_ex`: off, protected, no buffer, as a process is
    /// before the `zend.enable_gc` entry is read.
    pub(crate) fn new() -> Collector {
        Collector {
            buf: None,
            enabled: false,
            active: false,
            protected: true,
            full: false,
            num_roots: 0,
            runs: 0,
            collected: 0,
            activated_at: Instant::now(),
            collecting: Duration::ZERO,
            destructing: Duration::ZERO,
            freeing: Duration::ZERO,
        }
    }

    /// `gc_reset`: the counts back to zero over a buffer that stands,
    /// and the clock restarted whatever there is.
    fn reset(&mut self) {
        if let Some(buf) = &mut self.buf {
            buf.entries.clear();
            buf.unused = None;
            self.active = false;
            self.protected = false;
            self.full = false;
            self.num_roots = 0;
            self.runs = 0;
            self.collected = 0;
            self.collecting = Duration::ZERO;
            self.destructing = Duration::ZERO;
            self.freeing = Duration::ZERO;
        }
        self.activated_at = Instant::now();
    }

    /// `gc_enable`: the old setting; the first enable lays the buffer
    /// down at its default size and threshold and resets the counts.
    pub(crate) fn enable(&mut self, enable: bool) -> bool {
        let old = self.enabled;
        self.enabled = enable;
        if enable && !old && self.buf.is_none() {
            self.buf = Some(Buffer::new());
            self.reset();
        }
        old
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// `gc_protect`: the old setting.
    pub(crate) fn protect(&mut self, protect: bool) -> bool {
        std::mem::replace(&mut self.protected, protect)
    }

    /// `zend_gc_get_status`.
    pub(crate) fn status(&self) -> GcStatus {
        GcStatus {
            running: self.active,
            protected: self.protected,
            full: self.full,
            runs: self.runs,
            collected: self.collected,
            threshold: self.buf.as_ref().map_or(0, |buf| buf.threshold),
            buffer_size: self.buf.as_ref().map_or(0, |buf| buf.size),
            roots: self.num_roots,
            application_time: self.activated_at.elapsed(),
            collector_time: self.collecting,
            destructor_time: self.destructing,
            free_time: self.freeing,
        }
    }

    /// `first_unused`, `GC_FIRST_ROOT` with no buffer.
    fn first_unused(&self) -> u32 {
        self.buf.as_ref().map_or(FIRST_ROOT, Buffer::first_unused)
    }

    fn entry(&self, idx: u32) -> Option<Entry> {
        self.buf.as_ref()?.entry(idx)
    }

    fn set(&mut self, idx: u32, entry: Entry) {
        if let Some(buf) = &mut self.buf {
            buf.set(idx, entry);
        }
    }

    /// `gc_remove_from_roots`.
    fn remove_from_roots(&mut self, idx: u32) {
        if let Some(buf) = &mut self.buf {
            buf.link_unused(idx);
        }
        self.num_roots = self.num_roots.saturating_sub(1);
    }

    /// A free index for a root php adds under its threshold
    /// (`gc_possible_root`'s ladder): an unused entry, or the next one
    /// while it is under the threshold; none at the threshold, which is
    /// `gc_possible_root_when_full`'s.
    fn fetch_under_threshold(&mut self) -> Option<u32> {
        let buf = self.buf.as_mut()?;
        if let Some(idx) = buf.fetch_unused() {
            return Some(idx);
        }
        (buf.first_unused() < buf.threshold).then(|| buf.fetch_next_unused())
    }

    /// A free index for a root past the threshold, a garbage entry or an
    /// extra root, the buffer grown past its size where it must be
    /// (`gc_add_garbage`'s and `gc_extra_root`'s ladder); none where it
    /// cannot grow.
    fn fetch_any(&mut self) -> Option<u32> {
        let buf = self.buf.as_mut()?;
        if let Some(idx) = buf.fetch_unused() {
            return Some(idx);
        }
        if buf.first_unused() == buf.size {
            self.grow_root_buffer();
        }
        let buf = self.buf.as_mut()?;
        (buf.first_unused() != buf.size).then(|| buf.fetch_next_unused())
    }

    /// `gc_grow_root_buffer`: at its largest the buffer grows no more and
    /// the collector turns itself off, marked full, which
    /// [`Heap::gc_growing`] tells as php's warning.
    fn grow_root_buffer(&mut self) {
        let Some(buf) = &mut self.buf else { return };
        if buf.size >= MAX_BUF_SIZE && !self.full {
            self.active = true;
            self.protected = true;
            self.full = true;
            return;
        }
        let new_size = if buf.size < BUF_GROW_STEP {
            buf.size.saturating_mul(2)
        } else {
            buf.size.saturating_add(BUF_GROW_STEP)
        };
        buf.size = new_size.min(MAX_BUF_SIZE);
    }

    /// `gc_adjust_threshold`: a collection that freed little, or left the
    /// buffer at its threshold, raises the threshold a step, growing the
    /// buffer to hold it; one that freed enough lowers it a step, never
    /// below the default.
    fn adjust_threshold(&mut self, count: i64) {
        let num_roots = self.num_roots;
        let Some(buf) = &self.buf else { return };
        let threshold = buf.threshold;
        if count < THRESHOLD_TRIGGER || num_roots >= threshold {
            if threshold < THRESHOLD_MAX {
                let new_threshold = (threshold + THRESHOLD_STEP).min(THRESHOLD_MAX);
                if new_threshold > buf.size {
                    self.grow_root_buffer();
                }
                if let Some(buf) = &mut self.buf
                    && new_threshold <= buf.size
                {
                    buf.threshold = new_threshold;
                }
            }
        } else if threshold > THRESHOLD_DEFAULT
            && let Some(buf) = &mut self.buf
        {
            buf.threshold = threshold
                .saturating_sub(THRESHOLD_STEP)
                .max(THRESHOLD_DEFAULT);
        }
    }

    /// Write `node` at `idx` as a root and put its compressed index and
    /// `colour` in its header, counting it.
    fn place(&mut self, idx: u32, node: Node, colour: Colour) {
        self.set(idx, Entry::Root(node));
        node.gc().set_place(Some(compress(idx)), colour);
        self.num_roots += 1;
    }

    /// `GC_REMOVE_FROM_BUFFER` and `gc_remove_from_buffer`: a value
    /// that carries the collector's bits has them cleared, and the entry
    /// its address names is put back on the unused list. What
    /// `zend_array_destroy` and `zend_objects_store_del` do before the
    /// value's memory goes, so the buffer never names freed memory.
    pub(crate) fn remove_from_buffer(&mut self, node: Node) {
        let header = node.gc();
        if header.is_unplaced() {
            return;
        }
        let address = header.address();
        header.set_place(None, Colour::Black);
        let Some(address) = address else { return };
        let idx = if self.first_unused() >= MAX_UNCOMPRESSED {
            self.buf
                .as_ref()
                .and_then(|buf| buf.decompress(node, address))
        } else {
            Some(address)
        };
        if let Some(idx) = idx {
            self.remove_from_roots(idx);
        }
    }

    /// `gc_compact`, the two-finger compaction: the roots moved down
    /// into the unused entries below them, each header's address
    /// rewritten, so the roots are the entries from the first on.
    fn compact(&mut self) {
        let num_roots = self.num_roots;
        let Some(buf) = &mut self.buf else { return };
        if num_roots + FIRST_ROOT == buf.first_unused() {
            return;
        }
        if num_roots != 0 {
            let is_unused = |entry: Option<Entry>| matches!(entry, Some(Entry::Unused(_)));
            let mut free = FIRST_ROOT;
            let mut scan = buf.first_unused() - 1;
            let end = num_roots;
            while free < scan {
                while !is_unused(buf.entry(free)) {
                    free += 1;
                }
                while is_unused(buf.entry(scan)) {
                    scan -= 1;
                }
                if scan > free {
                    if let Some(moved) = buf.entry(scan) {
                        buf.set(free, moved);
                        if let Some(node) = entry_node(moved) {
                            let colour = node.gc().colour();
                            node.gc().set_place(Some(compress(free)), colour);
                        }
                    }
                    free += 1;
                    scan -= 1;
                    if scan <= end {
                        break;
                    }
                }
            }
        }
        buf.unused = None;
        buf.entries.truncate(num_roots as usize);
    }

    /// `gc_extra_root`: a value the weak-map walk must still scan,
    /// added during a collection with its colour kept.
    fn extra_root(&mut self, node: Node) {
        if let Some(idx) = self.fetch_any() {
            let colour = node.gc().colour();
            self.place(idx, node, colour);
        }
    }

    /// `gc_add_garbage`: a white value that was not a root, added as
    /// garbage for the free phase, black.
    fn add_garbage(&mut self, node: Node) {
        if let Some(idx) = self.fetch_any() {
            self.set(idx, Entry::Garbage(node));
            node.gc().set_place(Some(compress(idx)), Colour::Black);
            self.num_roots += 1;
        }
    }
}

/// The value an entry names, whatever its kind; none for an unused one.
fn entry_node(entry: Entry) -> Option<Node> {
    match entry {
        Entry::Root(n) | Entry::Garbage(n) => Some(n),
        Entry::DtorGarbage(o) => Some(Node::Obj(o)),
        Entry::Unused(_) => None,
    }
}

/// `gc_compress`: an index past 2^19 as its remainder with the 2^19
/// bit set, since the header keeps twenty bits of it.
fn compress(idx: u32) -> u32 {
    if idx < MAX_UNCOMPRESSED {
        return idx;
    }
    (idx % MAX_UNCOMPRESSED) | MAX_UNCOMPRESSED
}

/// `GC_G(collected) += count`: php's `int` count added to its
/// `uint32_t`, which wraps as C's conversion does.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "php's `uint32_t += int`, whose wrap is the behaviour being copied"
)]
fn as_count(count: i64) -> u32 {
    count as u32
}

/// The table php's walk reaches through an object after the values its
/// handler named, `get_gc`'s returned `ht`: one of the heap's, which
/// each phase colours, or the rows of one that is no heap value, which
/// each phase reaches once, through the one object holding it.
#[derive(Clone, Copy)]
enum Table {
    Arr(Arr),
    Rows,
}

impl Table {
    /// Whether the phase whose step on a child is `step` walks the
    /// table's rows: php's colour test on `ht`, and always for rows.
    fn newly(self, step: impl FnOnce(Node) -> bool) -> bool {
        match self {
            Table::Arr(a) => step(Node::Arr(a)),
            Table::Rows => true,
        }
    }
}

/// The children php's walk reads off one value, `get_gc` for an object:
/// the zvals in `kids`, and the table php's handler hands back beside
/// them, whose own entries follow in `kids` from the index returned.
/// For an object of the properties' handlers with a dynamic table that
/// is the table, walked after what the handler names, its declared slots
/// first, as php's `rebuild_object_properties` puts the slots'
/// `IS_INDIRECT` rows first; for one without, the handler's zvals and
/// then the slots, php's `properties_table`. A table the handler hands
/// back takes the properties' place, as php's does.
fn children(hooks: &mut dyn HeapHooks, node: Node, kids: &mut Vec<Zval>) -> (Option<Table>, usize) {
    kids.clear();
    match node {
        Node::Arr(a) => {
            a.gc_values(kids);
            (None, 0)
        }
        Node::Ref(r) => {
            kids.push(r.val());
            (None, 0)
        }
        Node::Obj(o) => match hooks.gc_children(o, &mut |z| kids.push(z)) {
            GcTable::Properties => {
                let table = o.properties();
                let split = if table.is_some() { kids.len() } else { 0 };
                for i in 0..o.slots() {
                    kids.push(o.slot(i));
                }
                if let Some(table) = table {
                    table.gc_values(kids);
                }
                (owned(table), split)
            }
            GcTable::Arr(table) => {
                let split = kids.len();
                table.gc_values(kids);
                (owned(Some(table)), split)
            }
            GcTable::Rows(rows) => {
                let split = kids.len();
                kids.extend(rows);
                (Some(Table::Rows), split)
            }
        },
    }
}

/// A table of the heap's as the walk colours it: an immutable one has no
/// header to colour and holds nothing a cycle passes through, so its
/// rows are walked as the handler's values are.
fn owned(table: Option<Arr>) -> Option<Table> {
    table.filter(|t| !t.is_immutable()).map(Table::Arr)
}

/// php's `handle_zvals` loop over `list` for one phase: `step` is what
/// the phase does to each collectable child and whether it newly
/// coloured it; the first child it newly coloured is the one walked
/// next, and each after it is pushed.
fn walk(list: &[Zval], stack: &mut Vec<Node>, mut step: impl FnMut(Node) -> bool) -> Option<Node> {
    let mut first = None;
    for z in list {
        if let Some(child) = Node::of(*z)
            && step(child)
        {
            if first.is_none() {
                first = Some(child);
            } else {
                stack.push(child);
            }
        }
    }
    first
}

/// The loop php runs over a handler's table before it walks the table
/// the handler returned: every child `step` newly coloured is pushed.
fn push_all(list: &[Zval], stack: &mut Vec<Node>, mut step: impl FnMut(Node) -> bool) {
    for z in list {
        if let Some(child) = Node::of(*z)
            && step(child)
        {
            stack.push(child);
        }
    }
}

/// `gc_mark_grey`'s step: one owner fewer, and grey where it was not.
fn grey(child: Node) -> bool {
    let g = child.gc();
    g.delref();
    if g.colour() == Colour::Grey {
        return false;
    }
    g.set_colour(Colour::Grey);
    true
}

/// `gc_scan_black`'s step: the owner given back, and black where it was
/// not.
fn blacken(child: Node) -> bool {
    let g = child.gc();
    g.addref();
    if g.colour() == Colour::Black {
        return false;
    }
    g.set_colour(Colour::Black);
    true
}

/// `gc_scan`'s step: a grey child whitened.
fn whiten(child: Node) -> bool {
    let g = child.gc();
    if g.colour() != Colour::Grey {
        return false;
    }
    g.set_colour(Colour::White);
    true
}

/// `gc_collect_white`'s step: the owner given back, and a white child
/// blackened.
fn collect(child: Node) -> bool {
    let g = child.gc();
    g.addref();
    if g.colour() != Colour::White {
        return false;
    }
    g.set_colour(Colour::Black);
    true
}

/// `zend_weakmap_get_object_key_entry_gc`: the `WeakMap` entries keyed
/// by `o`, each beside its map; none for an object no weak map keys.
fn weakly_keyed(hooks: &mut dyn HeapHooks, o: Obj) -> Vec<(ZvalRef, Obj)> {
    let mut entries = Vec::new();
    if o.is_weakly_referenced() {
        hooks.gc_weakly_keyed(o, &mut |entry, map| entries.push((entry, map)));
    }
    entries
}

/// `zend_weakmap_get_key_entry_gc`: a `WeakMap`'s keys beside its
/// entries, none for an object that is no `WeakMap`.
fn weakmap_entries(hooks: &mut dyn HeapHooks, o: Obj) -> Option<Vec<(Obj, ZvalRef)>> {
    let mut entries = Vec::new();
    hooks
        .gc_weakmap_entries(o, &mut |key, entry| entries.push((key, entry)))
        .then_some(entries)
}

/// The value `gc_check_possible_root` offers the buffer for `z`: an
/// array or an object, or the one a reference holds, not in the buffer
/// yet and not marked uncollectable (`GC_MAY_LEAK`); none for anything
/// else.
fn possible_root(z: Zval) -> Option<Node> {
    let node = match Node::of(z)? {
        Node::Ref(r) => match Node::of(r.val())? {
            Node::Ref(_) => return None,
            held => held,
        },
        node => node,
    };
    node.gc().may_leak().then_some(node)
}

/// Whether the walk may read the object's members: not once `free_obj`
/// ran over them (`IS_OBJ_FREE_CALLED`).
fn walkable(o: Obj) -> bool {
    !o.has_flag(gc::OBJ_FREE_CALLED)
}

impl Heap {
    /// `f` over the collector, and php's `GC buffer overflow` warning
    /// where it is what filled the root buffer: every path that can grow
    /// the buffer runs through here, so the warning is raised once, as
    /// `gc_grow_root_buffer` raises it the first time it finds the buffer
    /// at `GC_MAX_BUF_SIZE` ([`HeapHooks::gc_overflow`]).
    fn gc_growing<R>(hooks: &mut dyn HeapHooks, f: impl FnOnce(&mut Collector) -> R) -> R {
        let full = hooks.heap().gc.full;
        let out = f(&mut hooks.heap().gc);
        if !full && hooks.heap().gc.full {
            hooks.gc_overflow();
        }
        out
    }

    /// `gc_check_possible_root` (`Z/Zend/zend_gc.h:114`): what a release
    /// that left owners does with the value. A reference is looked
    /// through to the value it holds; an array or an object not in the
    /// buffer yet and not marked uncollectable is a possible root.
    pub fn gc_check_possible_root(hooks: &mut dyn HeapHooks, z: Zval) {
        if let Some(node) = possible_root(z) {
            Heap::gc_possible_root(hooks, node);
        }
    }

    /// One owner of `z` given up where that serves no PHP code:
    /// `GC_DELREF` with another owner standing, and
    /// `gc_check_possible_root` taking nothing or placing the value in the
    /// root buffer under its threshold ([`Heap::gc_check_possible_root`]).
    /// Whether it was given up. Where it was not, the last owner, which
    /// `rc_dtor_func` destroys, or a possible root with the buffer at its
    /// threshold, where `gc_possible_root_when_full` collects, nothing is
    /// touched and the owner is the caller's to give up through
    /// [`Heap::release`]. What a frame's leave gives up an array or an
    /// object its variables own with, as `i_free_compiled_variables`
    /// gives them up with no destructor served and nothing collected.
    #[must_use]
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn release_kept(&mut self, z: Zval) -> bool {
        let Some(header) = z.counted().filter(|header| header.refcount() > 1) else {
            return false;
        };
        let gc = &mut self.gc;
        match possible_root(z) {
            Some(node) if !gc.protected && gc.buf.is_some() => {
                let Some(idx) = gc.fetch_under_threshold() else {
                    return false;
                };
                let _owners_left = Heap::give_up(header);
                gc.place(idx, node, Colour::Purple);
            }
            _ => {
                let _owners_left = Heap::give_up(header);
            }
        }
        true
    }

    /// `gc_possible_root` (`Z/Zend/zend_gc.c:738`): the value in the
    /// buffer, purple, in an unused entry or the next one under the
    /// threshold; at the threshold, `gc_possible_root_when_full`.
    fn gc_possible_root(hooks: &mut dyn HeapHooks, node: Node) {
        let gc = &mut hooks.heap().gc;
        if gc.protected || gc.buf.is_none() {
            return;
        }
        match gc.fetch_under_threshold() {
            Some(idx) => gc.place(idx, node, Colour::Purple),
            None => Heap::gc_possible_root_when_full(hooks, node),
        }
    }

    /// `gc_possible_root_when_full` (`Z/Zend/zend_gc.c:692`): a collection, the
    /// value held across it, the threshold adjusted by what the collection
    /// freed; then the value freed where the collection left it no owner, left
    /// where the collection put it in the buffer, and added otherwise, past the
    /// threshold and growing the buffer where it must.
    fn gc_possible_root_when_full(hooks: &mut dyn HeapHooks, node: Node) {
        let gc = &hooks.heap().gc;
        if gc.enabled && !gc.active {
            node.gc().addref();
            let count = match hooks.gc_collect_cycles() {
                Some(count) => count,
                None => Heap::gc_collect_cycles(hooks),
            };
            Heap::gc_growing(hooks, |gc| gc.adjust_threshold(count));
            if node.gc().delref() == 0 {
                match node {
                    Node::Arr(a) => Heap::array_destroy(hooks, a),
                    Node::Obj(o) => Heap::store_del(hooks, o),
                    Node::Ref(_) => {}
                }
                return;
            }
            if !node.gc().is_unplaced() {
                return;
            }
        }
        Heap::gc_growing(hooks, |gc| {
            if let Some(idx) = gc.fetch_any() {
                gc.place(idx, node, Colour::Purple);
            }
        });
    }

    /// `gc_mark_grey`'s two weak-map passes over an object: every entry
    /// keyed by it marked as reached from its key, one owner taken off
    /// it unless the map's walk took one already, and greyed; and, for
    /// a `WeakMap`, every entry marked as reached from the map, one owner
    /// taken off unless its key's walk took one, and greyed, which is
    /// all of the map's walk. Whether the object was a `WeakMap`.
    fn gc_grey_weak(hooks: &mut dyn HeapHooks, o: Obj, stack: &mut Vec<Node>) -> bool {
        for (entry, _) in weakly_keyed(hooks, o) {
            let z = entry.read();
            let Some(child) = Node::of(z) else { continue };
            let marks = z.weak_marks();
            entry.write(z.with_weak_marks(WeakMarks {
                from_key: true,
                ..marks
            }));
            if !marks.from_map {
                child.gc().delref();
            }
            if child.gc().colour() != Colour::Grey {
                child.gc().set_colour(Colour::Grey);
                stack.push(child);
            }
        }
        let Some(entries) = weakmap_entries(hooks, o) else {
            return false;
        };
        for (_, entry) in entries {
            let z = entry.read();
            let Some(child) = Node::of(z) else { continue };
            let marks = z.weak_marks();
            entry.write(z.with_weak_marks(WeakMarks {
                from_map: true,
                ..marks
            }));
            if !marks.from_key {
                child.gc().delref();
            }
            if child.gc().colour() != Colour::Grey {
                child.gc().set_colour(Colour::Grey);
                stack.push(child);
            }
        }
        true
    }

    /// `gc_scan_black`'s two weak-map passes: an entry keyed by the
    /// object loses its key mark, and is blackened here when its map is
    /// black and did not reach it, while a grey map not in the buffer is
    /// added as an extra root so the scan reaches it; a `WeakMap`'s
    /// entries the same with key and map exchanged, which is all of the
    /// map's walk. Whether the object was a `WeakMap`.
    fn gc_black_weak(hooks: &mut dyn HeapHooks, o: Obj, stack: &mut Vec<Node>) -> bool {
        for (entry, map) in weakly_keyed(hooks, o) {
            let z = entry.read();
            let Some(child) = Node::of(z) else { continue };
            let marks = z.weak_marks();
            entry.write(z.with_weak_marks(WeakMarks {
                from_key: false,
                ..marks
            }));
            let map = Node::Obj(map);
            if map.gc().colour() == Colour::Grey {
                if map.gc().address().is_none() {
                    Heap::gc_growing(hooks, |gc| gc.extra_root(map));
                }
            } else if !marks.from_map && blacken(child) {
                stack.push(child);
            }
        }
        let Some(entries) = weakmap_entries(hooks, o) else {
            return false;
        };
        for (key, entry) in entries {
            let z = entry.read();
            let Some(child) = Node::of(z) else { continue };
            let marks = z.weak_marks();
            entry.write(z.with_weak_marks(WeakMarks {
                from_map: false,
                ..marks
            }));
            let key = Node::Obj(key);
            if key.gc().colour() == Colour::Grey {
                if key.gc().address().is_none() {
                    Heap::gc_growing(hooks, |gc| gc.extra_root(key));
                }
            } else if !marks.from_key && blacken(child) {
                stack.push(child);
            }
        }
        true
    }

    /// `gc_scan`'s weak-map pass (`zend_weakmap_get_object_entry_gc`):
    /// every grey entry keyed by the object whitened.
    fn gc_whiten_weakly_keyed(hooks: &mut dyn HeapHooks, o: Obj, stack: &mut Vec<Node>) {
        for (entry, _) in weakly_keyed(hooks, o) {
            if let Some(child) = Node::of(entry.read())
                && whiten(child)
            {
                stack.push(child);
            }
        }
    }

    /// `gc_collect_white`'s two weak-map passes: an entry keyed by the
    /// object and reached from it, and a `WeakMap`'s entry reached from
    /// the map, has both marks cleared, its owner given back and, white,
    /// blackened; the map's pass is all of the map's walk. Whether the
    /// object was a `WeakMap`.
    fn gc_collect_weak(hooks: &mut dyn HeapHooks, o: Obj, stack: &mut Vec<Node>) -> bool {
        let clear = |entry: ZvalRef, reached: fn(WeakMarks) -> bool, stack: &mut Vec<Node>| {
            let z = entry.read();
            if let Some(child) = Node::of(z)
                && reached(z.weak_marks())
            {
                entry.write(z.with_weak_marks(WeakMarks::default()));
                if collect(child) {
                    stack.push(child);
                }
            }
        };
        for (entry, _) in weakly_keyed(hooks, o) {
            clear(entry, |marks| marks.from_key, stack);
        }
        let Some(entries) = weakmap_entries(hooks, o) else {
            return false;
        };
        for (_, entry) in entries {
            clear(entry, |marks| marks.from_map, stack);
        }
        true
    }

    /// `gc_mark_grey`: every value reachable from `root` one owner fewer
    /// for each owner inside the walk, and grey.
    fn gc_mark_grey(hooks: &mut dyn HeapHooks, root: Node, kids: &mut Vec<Zval>) {
        let mut stack = Vec::new();
        let mut node = root;
        loop {
            let next = match node {
                Node::Obj(o) if walkable(o) && Heap::gc_grey_weak(hooks, o, &mut stack) => None,
                Node::Obj(o) if walkable(o) => match children(hooks, node, kids) {
                    (Some(table), split) if table.newly(grey) => {
                        push_all(&kids[..split], &mut stack, grey);
                        walk(&kids[split..], &mut stack, grey)
                    }
                    (Some(_), split) => walk(&kids[..split], &mut stack, grey),
                    (None, _) => walk(kids, &mut stack, grey),
                },
                Node::Obj(_) => None,
                Node::Arr(_) | Node::Ref(_) => {
                    children(hooks, node, kids);
                    walk(kids, &mut stack, grey)
                }
            };
            match next.or_else(|| stack.pop()) {
                Some(n) => node = n,
                None => break,
            }
        }
    }

    /// `gc_scan_black`: every value reachable from `root`, which the
    /// caller blackened, given back the owners the grey walk took and
    /// blackened: live.
    fn gc_scan_black(hooks: &mut dyn HeapHooks, root: Node, kids: &mut Vec<Zval>) {
        let mut stack = Vec::new();
        let mut node = root;
        loop {
            let next = match node {
                Node::Obj(o) if walkable(o) && Heap::gc_black_weak(hooks, o, &mut stack) => None,
                Node::Obj(o) if walkable(o) => match children(hooks, node, kids) {
                    (Some(table), split) if table.newly(blacken) => {
                        push_all(&kids[..split], &mut stack, blacken);
                        walk(&kids[split..], &mut stack, blacken)
                    }
                    (Some(_), split) => walk(&kids[..split], &mut stack, blacken),
                    (None, _) => walk(kids, &mut stack, blacken),
                },
                Node::Obj(_) => None,
                Node::Arr(_) | Node::Ref(_) => {
                    children(hooks, node, kids);
                    walk(kids, &mut stack, blacken)
                }
            };
            match next.or_else(|| stack.pop()) {
                Some(n) => node = n,
                None => break,
            }
        }
    }

    /// `gc_scan`: over the white values reachable from `root`, one that
    /// an owner outside the walk still holds is blackened with all it
    /// reaches, and the grey ones the rest reach are whitened.
    fn gc_scan(hooks: &mut dyn HeapHooks, root: Node, kids: &mut Vec<Zval>) {
        let mut stack = Vec::new();
        let mut node = root;
        loop {
            let g = node.gc();
            // A pin is no owner: a pinned value nothing else reaches is
            // garbage as php's is, and its pin keeps only its bytes
            // ([`Heap::give_back`]).
            let next = if g.colour() != Colour::White {
                None
            } else if g.refcount() > 0 {
                g.set_colour(Colour::Black);
                Heap::gc_scan_black(hooks, node, kids);
                None
            } else {
                match node {
                    Node::Obj(o) if walkable(o) => {
                        Heap::gc_whiten_weakly_keyed(hooks, o, &mut stack);
                        match children(hooks, node, kids) {
                            (Some(table), split) if table.newly(whiten) => {
                                if let Table::Arr(a) = table {
                                    stack.push(Node::Arr(a));
                                }
                                push_all(&kids[..split], &mut stack, whiten);
                                walk(&kids[split..], &mut stack, whiten)
                            }
                            (Some(_), split) => walk(&kids[..split], &mut stack, whiten),
                            (None, _) => walk(kids, &mut stack, whiten),
                        }
                    }
                    Node::Obj(_) => None,
                    Node::Arr(_) | Node::Ref(_) => {
                        children(hooks, node, kids);
                        walk(kids, &mut stack, whiten)
                    }
                }
            };
            match next.or_else(|| stack.pop()) {
                Some(n) => node = n,
                None => break,
            }
        }
    }

    /// `gc_collect_white`: every white value reachable from `root`, which
    /// the caller blackened, given back its owners, blackened and added
    /// to the buffer as garbage where it is not in it already; the count
    /// of what was found, references not among it, and `flags` told
    /// when a destructor is still to be served.
    fn gc_collect_white(
        hooks: &mut dyn HeapHooks,
        root: Node,
        has_destructors: &mut bool,
        kids: &mut Vec<Zval>,
    ) -> i64 {
        let mut count = 0;
        let mut stack = Vec::new();
        let mut node = root;
        loop {
            if !node.is_reference() {
                count += 1;
            }
            let next = match node {
                Node::Obj(o) if walkable(o) => {
                    if o.gc().is_unplaced() {
                        Heap::gc_growing(hooks, |gc| gc.add_garbage(node));
                    }
                    if !o.has_flag(gc::OBJ_DESTRUCTOR_CALLED) && hooks.has_destructor(o) {
                        *has_destructors = true;
                    }
                    if Heap::gc_collect_weak(hooks, o, &mut stack) {
                        None
                    } else {
                        match children(hooks, node, kids) {
                            (Some(table), split) if table.newly(collect) => {
                                push_all(&kids[..split], &mut stack, collect);
                                walk(&kids[split..], &mut stack, collect)
                            }
                            (Some(_), split) => walk(&kids[..split], &mut stack, collect),
                            (None, _) => walk(kids, &mut stack, collect),
                        }
                    }
                }
                Node::Obj(_) => None,
                Node::Arr(_) => {
                    if node.gc().is_unplaced() {
                        Heap::gc_growing(hooks, |gc| gc.add_garbage(node));
                    }
                    children(hooks, node, kids);
                    walk(kids, &mut stack, collect)
                }
                Node::Ref(_) => {
                    children(hooks, node, kids);
                    walk(kids, &mut stack, collect)
                }
            };
            match next.or_else(|| stack.pop()) {
                Some(n) => node = n,
                None => break,
            }
        }
        count
    }

    /// `gc_remove_nested_data_from_buffer`: what an object whose
    /// destructor is to be served reaches is taken out of the garbage,
    /// since the destructor may keep it; the count of what was taken, the
    /// object itself among it.
    fn gc_remove_nested_data(hooks: &mut dyn HeapHooks, root: Node, kids: &mut Vec<Zval>) -> i64 {
        let mut count = 0;
        let mut stack = Vec::new();
        let mut node = root;
        let mut first = true;
        loop {
            let traverse = if first {
                first = false;
                count += 1;
                true
            } else if node.gc().address().is_some() && node.gc().colour() == Colour::Black {
                hooks.heap().gc.remove_from_buffer(node);
                count += 1;
                true
            } else if let Node::Ref(r) = node {
                if let Some(held) = Node::of(r.val()) {
                    node = held;
                    continue;
                }
                false
            } else {
                false
            };
            let next = if traverse {
                match node {
                    Node::Obj(o) if walkable(o) => {
                        for (entry, _) in weakly_keyed(hooks, o) {
                            if let Some(child) = Node::of(entry.read()) {
                                stack.push(child);
                            }
                        }
                        match children(hooks, node, kids) {
                            (Some(table), split) => {
                                push_all(&kids[..split], &mut stack, |_| true);
                                if let Table::Arr(table) = table {
                                    let g = table.gc();
                                    if g.address().is_some() && g.colour() == Colour::Black {
                                        hooks.heap().gc.remove_from_buffer(Node::Arr(table));
                                    }
                                }
                                walk(&kids[split..], &mut stack, |_| true)
                            }
                            (None, _) => walk(kids, &mut stack, |_| true),
                        }
                    }
                    Node::Arr(_) => {
                        children(hooks, node, kids);
                        walk(kids, &mut stack, |_| true)
                    }
                    Node::Obj(_) | Node::Ref(_) => None,
                }
            } else {
                None
            };
            match next.or_else(|| stack.pop()) {
                Some(n) => node = n,
                None => break,
            }
        }
        count
    }

    /// `gc_mark_roots`: the buffer compacted, then every purple root
    /// greyed and walked.
    fn gc_mark_roots(hooks: &mut dyn HeapHooks, kids: &mut Vec<Zval>) {
        hooks.heap().gc.compact();
        let mut idx = FIRST_ROOT;
        while idx < hooks.heap().gc.first_unused() {
            if let Some(Entry::Root(node)) = hooks.heap().gc.entry(idx)
                && node.gc().colour() == Colour::Purple
            {
                node.gc().set_colour(Colour::Grey);
                Heap::gc_mark_grey(hooks, node, kids);
            }
            idx += 1;
        }
    }

    /// `gc_scan_roots`: every grey root whitened and scanned, and then
    /// the extra roots the scan added.
    fn gc_scan_roots(hooks: &mut dyn HeapHooks, kids: &mut Vec<Zval>) {
        let mut idx = FIRST_ROOT;
        while idx < hooks.heap().gc.first_unused() {
            if let Some(Entry::Root(node)) = hooks.heap().gc.entry(idx)
                && node.gc().colour() == Colour::Grey
            {
                node.gc().set_colour(Colour::White);
                Heap::gc_scan(hooks, node, kids);
            }
            idx += 1;
        }
    }

    /// `gc_collect_roots`: the live roots out of the buffer, the rest
    /// compacted to its front and each made garbage, a white one walked
    /// for the garbage it reaches.
    fn gc_collect_roots(
        hooks: &mut dyn HeapHooks,
        has_destructors: &mut bool,
        kids: &mut Vec<Zval>,
    ) -> i64 {
        let gc = &mut hooks.heap().gc;
        for idx in FIRST_ROOT..gc.first_unused() {
            if let Some(Entry::Root(node)) = gc.entry(idx)
                && node.gc().colour() == Colour::Black
            {
                node.gc().set_place(None, Colour::Black);
                gc.remove_from_roots(idx);
            }
        }
        gc.compact();
        let end = gc.first_unused();
        let mut count = 0;
        for idx in FIRST_ROOT..end {
            let gc = &mut hooks.heap().gc;
            let Some(Entry::Root(node)) = gc.entry(idx) else {
                continue;
            };
            gc.set(idx, Entry::Garbage(node));
            if node.gc().colour() == Colour::White {
                node.gc().set_colour(Colour::Black);
                count += Heap::gc_collect_white(hooks, node, has_destructors, kids);
            }
        }
        count
    }

    /// `gc_call_destructors`: the destructor of every object in the
    /// entries from `from` to `end` that is to have one served
    /// ([`Heap::gc_call_destructor`]). The buffer may grow meanwhile, so
    /// each entry is read afresh.
    fn gc_call_destructors(hooks: &mut dyn HeapHooks, from: u32, end: u32) {
        for idx in from..end {
            Heap::gc_call_destructor(hooks, idx);
        }
    }

    /// One entry of `gc_call_destructors` (`Z/Zend/zend_gc.c:1872`): the
    /// entry at `idx`, where it is garbage whose destructor is to be
    /// served, made a root again for the next collection, and the
    /// object's destructor served unless another destructor served it
    /// already. The object served, for a walk that is not the heap's own:
    /// the engine's, from the fiber [`HeapHooks::gc_destructors_in_fiber`]
    /// serves the phase in, which hands an object whose destructor
    /// suspended that fiber back to [`Heap::gc_check_possible_root`] once
    /// the destructor returns.
    pub fn gc_call_destructor(hooks: &mut dyn HeapHooks, idx: u32) -> Option<Obj> {
        let Some(Entry::DtorGarbage(o)) = hooks.heap().gc.entry(idx) else {
            return None;
        };
        hooks.heap().gc.set(idx, Entry::Root(Node::Obj(o)));
        if o.has_flag(gc::OBJ_DESTRUCTOR_CALLED) {
            return None;
        }
        o.add_flag(gc::OBJ_DESTRUCTOR_CALLED);
        o.addref();
        hooks.destruct(o);
        o.delref();
        Some(o)
    }

    /// The free phase of `zend_gc_collect_cycles`: every garbage object
    /// out of the store, turned to `IS_NULL` and its members freed once,
    /// every garbage table turned to `IS_NULL` and its contents
    /// destroyed; then, in a second pass, what is still garbage off the
    /// buffer and its bytes back to the heap, but for a value a pin
    /// stands over, whose bytes alone wait for the last pin
    /// ([`Heap::give_back`]).
    fn gc_free_garbage(hooks: &mut dyn HeapHooks, end: u32) {
        for idx in FIRST_ROOT..end {
            let Some(Entry::Garbage(node)) = hooks.heap().gc.entry(idx) else {
                continue;
            };
            match node {
                Node::Obj(o) => {
                    let handle = o.id().0;
                    hooks.heap().store.invalidate(handle);
                    o.gc().set_kind(ZvalType::Null);
                    if !o.has_flag(gc::OBJ_FREE_CALLED) {
                        o.add_flag(gc::OBJ_FREE_CALLED);
                        o.addref();
                        Heap::object_free(hooks, o);
                        o.delref();
                    }
                    hooks.heap().store.add_to_free_list(handle);
                }
                Node::Arr(a) => {
                    a.gc().set_kind(ZvalType::Null);
                    Heap::array_free_contents(hooks, a);
                }
                Node::Ref(_) => {}
            }
        }
        let heap = hooks.heap();
        for idx in FIRST_ROOT..end {
            let Some(Entry::Garbage(node)) = heap.gc.entry(idx) else {
                continue;
            };
            heap.gc.remove_from_roots(idx);
            // Where a pin keeps the bytes, the value stands destroyed and
            // of its own type, so what the pin names reads it so and an
            // owner taken of it since gives it up as any value; the
            // counts its garbage held of it went with that garbage.
            let (at, pinned) = match node {
                Node::Obj(o) => {
                    let pinned = o.pins() > 0;
                    if pinned {
                        o.gc().set_kind(ZvalType::Object);
                    }
                    (o.address(), pinned)
                }
                Node::Arr(a) => {
                    let pinned = a.gc().pinned();
                    if pinned {
                        a.emptied();
                    }
                    (a.address(), pinned)
                }
                Node::Ref(_) => continue,
            };
            if pinned {
                node.gc().set_refcount(0);
            }
            // SAFETY: the value was garbage, freed of its contents above,
            // out of the store and off the buffer, so only a pin names
            // it.
            unsafe { heap.give_back(at, pinned) };
        }
    }

    /// `zend_gc_collect_cycles` (`Z/Zend/zend_gc.c:1995`), what
    /// `gc_collect_cycles()` calls and a full buffer runs: nothing while
    /// a collection is under way; otherwise the roots marked, scanned and
    /// collected, the destructors of the garbage served first where it
    /// has any, and the collection repeated once after them, since a destructor
    /// may have given the garbage an owner no count shows; then the
    /// garbage freed. The number of values freed, as php 8.5 counts them.
    pub fn gc_collect_cycles(hooks: &mut dyn HeapHooks) -> i64 {
        let start = Instant::now();
        let mut total = 0i64;
        let mut should_rerun = false;
        let mut did_rerun = false;
        let mut kids = Vec::new();
        if hooks.heap().gc.num_roots != 0 && !hooks.heap().gc.active {
            Heap::gc_remove_root_tmpvars(hooks);
        }
        loop {
            if hooks.heap().gc.num_roots == 0 {
                hooks.heap().gc.compact();
                if should_rerun && !did_rerun {
                    did_rerun = true;
                    continue;
                }
                break;
            }
            if hooks.heap().gc.active {
                hooks.heap().gc.collecting += start.elapsed();
                return 0;
            }
            {
                let gc = &mut hooks.heap().gc;
                gc.runs += 1;
                gc.active = true;
            }
            Heap::gc_mark_roots(hooks, &mut kids);
            Heap::gc_scan_roots(hooks, &mut kids);
            let mut has_destructors = false;
            let mut count = Heap::gc_collect_roots(hooks, &mut has_destructors, &mut kids);
            if hooks.heap().gc.num_roots == 0 {
                hooks.heap().gc.active = false;
                break;
            }
            let end = hooks.heap().gc.first_unused();
            if has_destructors {
                should_rerun = true;
                for idx in FIRST_ROOT..end {
                    if let Some(Entry::Garbage(Node::Obj(o))) = hooks.heap().gc.entry(idx)
                        && !o.has_flag(gc::OBJ_DESTRUCTOR_CALLED)
                    {
                        if hooks.has_destructor(o) {
                            hooks.heap().gc.set(idx, Entry::DtorGarbage(o));
                            o.gc().set_colour(Colour::Purple);
                        } else {
                            o.add_flag(gc::OBJ_DESTRUCTOR_CALLED);
                        }
                    }
                }
                for idx in FIRST_ROOT..end {
                    if let Some(Entry::DtorGarbage(o)) = hooks.heap().gc.entry(idx) {
                        count -= Heap::gc_remove_nested_data(hooks, Node::Obj(o), &mut kids);
                    }
                }
                let dtor_start = Instant::now();
                if !hooks.gc_destructors_in_fiber(FIRST_ROOT, end) {
                    Heap::gc_call_destructors(hooks, FIRST_ROOT, end);
                }
                hooks.heap().gc.destructing += dtor_start.elapsed();
                if hooks.heap().gc.protected {
                    hooks.heap().gc.collecting += start.elapsed();
                    return 0;
                }
            }
            let free_start = Instant::now();
            Heap::gc_free_garbage(hooks, end);
            let gc = &mut hooks.heap().gc;
            gc.freeing += free_start.elapsed();
            gc.collected = gc.collected.wrapping_add(as_count(count));
            total += count;
            gc.active = false;
            gc.compact();
            if should_rerun && !did_rerun {
                did_rerun = true;
                continue;
            }
            break;
        }
        // No collection while the temporaries are rooted again, before
        // the threshold is adjusted, which php guards against unbounded
        // recursion.
        hooks.heap().gc.active = true;
        Heap::gc_check_root_tmpvars(hooks);
        hooks.heap().gc.active = false;
        hooks.heap().gc.collecting += start.elapsed();
        total
    }

    /// `zend_gc_remove_root_tmpvars`: every live temporary out of the
    /// root buffer, so the collection counts it as the outside owner it
    /// is.
    fn gc_remove_root_tmpvars(hooks: &mut dyn HeapHooks) {
        let mut live = Vec::new();
        hooks.gc_live_temporaries(&mut |z| live.push(z));
        let gc = &mut hooks.heap().gc;
        for z in live {
            if let Some(node) = Node::of(z) {
                gc.remove_from_buffer(node);
            }
        }
    }

    /// `zend_gc_check_root_tmpvars`: every live temporary a possible
    /// root again.
    fn gc_check_root_tmpvars(hooks: &mut dyn HeapHooks) {
        let mut live = Vec::new();
        hooks.gc_live_temporaries(&mut |z| live.push(z));
        for z in live {
            Heap::gc_check_possible_root(hooks, z);
        }
    }

    /// `gc_enable`: whether the collector was on. The first enable lays
    /// the root buffer down, which a request started with
    /// `zend.enable_gc` off has none of.
    pub fn gc_enable(&mut self, enable: bool) -> bool {
        self.gc.enable(enable)
    }

    /// `gc_enabled`.
    #[must_use]
    pub fn gc_enabled(&self) -> bool {
        self.gc.enabled()
    }

    /// `gc_protect`: whether additions to the buffer were refused; php
    /// refuses them from a fatal on.
    pub fn gc_protect(&mut self, protect: bool) -> bool {
        self.gc.protect(protect)
    }

    /// `zend_gc_get_status`, what `gc_status()` reads.
    #[must_use]
    pub fn gc_status(&self) -> GcStatus {
        self.gc.status()
    }

    /// `zend_mm_gc`, what `gc_mem_caches()` calls: the wholly free bin
    /// `runs` given back to their chunks, and the bytes that was.
    pub fn gc_mem_caches(&mut self) -> usize {
        self.mm.gc()
    }

    /// The collector as the request's start leaves it: php's process
    /// state before `zend.enable_gc` is read, then that entry's
    /// `OnUpdateGCEnabled` over it, so a request started with the entry
    /// off has no buffer, refuses every root and reports a size and a
    /// threshold of zero until `gc_enable()`.
    pub fn gc_startup(&mut self, enabled: bool) {
        self.gc = Collector::new();
        self.gc.enable(enabled);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::hooks::Fatal;
    use crate::serial::alone;

    /// The heap and a log of the destructors served, the object store's
    /// ids in the order the collector served them.
    struct Recorder {
        heap: Heap,
        served: Vec<u32>,
        /// An object the destructor gives an owner to, php's
        /// `$GLOBALS['x'] = $this` in `__destruct`.
        keep: Option<u32>,
        kept: Vec<Obj>,
        /// How many times the overflow warning was raised.
        overflows: u32,
    }

    impl Recorder {
        fn new() -> Recorder {
            Recorder {
                heap: Heap::new(),
                served: Vec::new(),
                keep: None,
                kept: Vec::new(),
                overflows: 0,
            }
        }
    }

    impl HeapHooks for Recorder {
        fn heap(&mut self) -> &mut Heap {
            &mut self.heap
        }
        fn destruct(&mut self, obj: Obj) {
            self.served.push(obj.id().0);
            if self.keep == Some(obj.id().0) {
                obj.addref();
                self.kept.push(obj);
            }
        }
        fn free_native(&mut self, _: Obj) {}
        fn gc_children(&mut self, _: Obj, _: &mut dyn FnMut(Zval)) -> GcTable {
            GcTable::Properties
        }
        fn free_resource(&mut self, _: crate::Res, _: crate::ResourceType) {}
        fn out_of_memory(&mut self, bytes: usize) -> Fatal {
            Fatal::out_of_memory(bytes)
        }
        fn gc_overflow(&mut self) {
            self.overflows += 1;
        }
    }

    /// `gc_grow_root_buffer` at `GC_MAX_BUF_SIZE`: the collector turns
    /// itself off, running, protected and full, and php's `GC buffer
    /// overflow (GC disabled)` warning is raised the first time and not
    /// again; a buffer under the largest grows without one. 2^30 entries
    /// are made by no test, so the size is set to the largest directly.
    #[test]
    fn a_root_buffer_at_its_largest_warns_once_and_turns_the_collector_off() {
        let _alone = alone();
        let mut r = Recorder::new();
        r.heap.gc.enable(true);
        Heap::gc_growing(&mut r, Collector::grow_root_buffer);
        assert_eq!(r.heap.gc_status().buffer_size, DEFAULT_BUF_SIZE * 2);
        assert_eq!(r.overflows, 0);
        if let Some(buf) = &mut r.heap.gc.buf {
            buf.size = MAX_BUF_SIZE;
        }
        Heap::gc_growing(&mut r, Collector::grow_root_buffer);
        let status = r.heap.gc_status();
        assert_eq!(r.overflows, 1);
        assert!(status.running && status.protected && status.full);
        assert_eq!(status.buffer_size, MAX_BUF_SIZE);
        Heap::gc_growing(&mut r, Collector::grow_root_buffer);
        assert_eq!(r.overflows, 1);
        assert_eq!(r.heap.gc_status().buffer_size, MAX_BUF_SIZE);
    }

    /// Two objects, each in the other's slot: `$a->b = $b; $b->a = $a;`.
    fn pair(r: &mut Recorder) -> Option<(Obj, Obj)> {
        let a = r.heap.object(1, 1, 0).ok()?;
        let b = r.heap.object(1, 1, 0).ok()?;
        Heap::object_set_slot(r, a, 0, Zval::object(b));
        b.addref();
        Heap::object_set_slot(r, b, 0, Zval::object(a));
        a.addref();
        Some((a, b))
    }

    /// A two-object cycle whose outside owners are gone is two roots
    /// until a collection, and nothing after it: both freed, both destructors
    /// served first in the order the walk found them, the collection repeated
    /// once for them, the ids back on the free list.
    #[test]
    fn a_two_object_cycle_is_collected_after_its_destructors() {
        let _alone = alone();
        let mut r = Recorder::new();
        let start = r.heap.memory_usage(false);
        let Some((a, b)) = pair(&mut r) else { return };
        Heap::release(&mut r, Zval::object(a));
        Heap::release(&mut r, Zval::object(b));
        assert_eq!(r.heap.gc_status().roots, 2);
        assert_eq!(r.heap.objects(), 2);
        let collected = Heap::gc_collect_cycles(&mut r);
        assert_eq!(r.served, vec![1, 2]);
        assert_eq!(collected, 2);
        let status = r.heap.gc_status();
        assert_eq!((status.runs, status.collected, status.roots), (2, 2, 0));
        assert_eq!(r.heap.objects(), 0);
        assert_eq!(r.heap.memory_usage(false), start);
    }

    /// An object a destructor gives an owner to is not freed, and
    /// neither is what it holds: the collection after the destructors finds
    /// the object live, and a destructor is served once.
    #[test]
    fn an_object_its_destructor_resurrects_is_kept() {
        let _alone = alone();
        let mut r = Recorder::new();
        let Some((a, b)) = pair(&mut r) else { return };
        r.keep = Some(1);
        Heap::release(&mut r, Zval::object(a));
        Heap::release(&mut r, Zval::object(b));
        let collected = Heap::gc_collect_cycles(&mut r);
        assert_eq!(collected, 0);
        assert_eq!(r.served, vec![1, 2]);
        assert_eq!(r.heap.objects(), 2);
        let kept = r.kept.pop();
        assert!(kept.is_some_and(|k| k.is_same(a)));
        // The last owner gone, the cycle is garbage again, and no
        // destructor is served twice.
        Heap::release(&mut r, Zval::object(a));
        assert_eq!(Heap::gc_collect_cycles(&mut r), 2);
        assert_eq!(r.served, vec![1, 2]);
        assert_eq!(r.heap.objects(), 0);
    }

    /// `$a = []; $a[] =& $a; unset($a);`: one root, the table, and one
    /// value freed, the reference the walk passes through not counted.
    #[test]
    fn a_table_that_holds_itself_by_reference_is_one_value() {
        let _alone = alone();
        let mut r = Recorder::new();
        let start = r.heap.memory_usage(false);
        let Ok(a) = r.heap.array() else { return };
        let Ok(cell) = r.heap.reference(Zval::array(a)) else {
            return;
        };
        cell.addref();
        assert_eq!(r.heap.array_append(a, Zval::reference(cell)), Ok(true));
        Heap::release(&mut r, Zval::reference(cell));
        assert_eq!(r.heap.gc_status().roots, 1);
        assert_eq!(Heap::gc_collect_cycles(&mut r), 1);
        let status = r.heap.gc_status();
        assert_eq!((status.runs, status.collected, status.roots), (1, 1, 0));
        assert_eq!(r.heap.memory_usage(false), start);
    }

    /// A root still held from outside is taken out of the buffer by the
    /// collection and nothing is freed.
    #[test]
    fn a_live_root_leaves_the_buffer_and_nothing_is_freed() {
        let _alone = alone();
        let mut r = Recorder::new();
        let Some((a, b)) = pair(&mut r) else { return };
        Heap::release(&mut r, Zval::object(b));
        assert_eq!(r.heap.gc_status().roots, 1);
        assert_eq!(Heap::gc_collect_cycles(&mut r), 0);
        assert_eq!(r.heap.gc_status().roots, 0);
        assert_eq!(r.heap.objects(), 2);
        assert!(r.served.is_empty());
        assert_eq!(a.refcount(), 2);
        assert_eq!(b.refcount(), 1);
    }

    /// An owner given up where another is held, php's `GC_DELREF` and
    /// `gc_check_possible_root`, leaves the value a possible root and
    /// never destroys it; asked of the last owner, it gives nothing up.
    #[test]
    fn a_held_release_is_a_possible_root_and_never_the_last() {
        let _alone = alone();
        let mut r = Recorder::new();
        let Ok(o) = r.heap.object(1, 0, 0) else {
            return;
        };
        let z = Zval::object(o);
        o.addref();
        assert!(Heap::release_held(&mut r, z));
        assert_eq!(o.refcount(), 1);
        assert_eq!(r.heap.gc_status().roots, 1, "a possible root");
        assert!(!Heap::release_held(&mut r, z), "the last owner is kept");
        assert_eq!(o.refcount(), 1);
        assert!(r.served.is_empty(), "nothing is destroyed");
        Heap::release(&mut r, z);
        assert_eq!(r.served, vec![1]);
    }

    /// A pinned block whose last owner goes is destroyed then, as php
    /// destroys it, and its bytes are counted off then: an object's
    /// destructor is served and a table's rows released, and only the
    /// bytes wait for the pins. A second pin over a pinned table is
    /// counted, so the bytes wait for whichever of the two is let go
    /// last, the first taken let go first here, as a fiber lets go a
    /// pin it took after the one let go outside it (#2291).
    #[test]
    fn a_pinned_block_is_destroyed_at_its_last_release_and_only_its_bytes_wait() {
        let _alone = alone();
        let mut r = Recorder::new();
        let start = r.heap.memory_usage(false);
        let (Ok(o), Ok(a), Ok(s)) = (
            r.heap.object(1, 0, 0),
            r.heap.array(),
            r.heap.string(b"held"),
        ) else {
            return;
        };
        assert_eq!(r.heap.array_append(a, Zval::string(s)), Ok(true));
        let (object, table) = (Zval::object(o), Zval::array(a));
        o.addref();
        r.heap.pin_block(object);
        r.heap.pin_block(table);
        r.heap.pin_block(table);
        assert_eq!(o.refcount(), 2, "the pin is no count");
        Heap::release(&mut r, object);
        Heap::release(&mut r, object);
        Heap::release(&mut r, table);
        assert_eq!(r.served, vec![1], "the destructor is served at the release");
        assert_eq!(r.heap.objects(), 0);
        assert_eq!(a.len(), 0, "the table stands empty");
        assert_eq!(
            r.heap.memory_usage(false),
            start,
            "and its memory is counted off"
        );
        assert!(r.heap.mm.is_retired(a.address()));
        r.heap.unpin_block(object);
        r.heap.unpin_block(table);
        assert!(
            !r.heap.mm.is_retired(o.address()),
            "the bytes go back with the pin"
        );
        assert!(
            r.heap.mm.is_retired(a.address()),
            "the second pin keeps the table's"
        );
        r.heap.unpin_block(table);
        assert!(!r.heap.mm.is_retired(a.address()));
        assert_eq!(r.heap.memory_usage(false), start, "counted once");
    }

    /// A pinned table an owner is taken of after its destruction, by
    /// what the pin let name it, is an empty table, not freed when the
    /// pin goes and freed at its next last release as any table is, with
    /// nothing counted twice.
    #[test]
    fn a_pinned_table_owned_again_is_an_empty_table_given_up_as_any() {
        let _alone = alone();
        let mut r = Recorder::new();
        let start = r.heap.memory_usage(false);
        let (Ok(a), Ok(s)) = (r.heap.array(), r.heap.string(b"held")) else {
            return;
        };
        assert_eq!(r.heap.array_append(a, Zval::string(s)), Ok(true));
        let table = Zval::array(a);
        r.heap.pin_block(table);
        Heap::release(&mut r, table);
        assert_eq!(r.heap.memory_usage(false), start);
        r.heap.addref(table);
        r.heap.unpin_block(table);
        assert!(
            r.heap.mm.is_retired(a.address()),
            "owned again, nothing is freed"
        );
        assert_eq!(a.refcount(), 1);
        assert_eq!(r.heap.array_append(a, Zval::long(1)), Ok(true));
        Heap::release(&mut r, table);
        assert!(!r.heap.mm.is_retired(a.address()));
        assert_eq!(r.heap.memory_usage(false), start);
    }

    /// A pinned value in a cycle nothing else holds is garbage, as php's
    /// is: the collection serves the destructors and frees the members,
    /// and the pin keeps only the bytes.
    #[test]
    fn a_pinned_cycle_is_collected_and_the_pin_keeps_its_bytes() {
        let _alone = alone();
        let mut r = Recorder::new();
        let start = r.heap.memory_usage(false);
        let Some((a, b)) = pair(&mut r) else { return };
        r.heap.pin_block(Zval::object(a));
        Heap::release(&mut r, Zval::object(a));
        Heap::release(&mut r, Zval::object(b));
        assert_eq!(Heap::gc_collect_cycles(&mut r), 2);
        assert_eq!(r.served, vec![1, 2]);
        assert_eq!(r.heap.objects(), 0);
        assert_eq!(r.heap.memory_usage(false), start);
        assert!(r.heap.mm.is_retired(a.address()));
        r.heap.unpin_block(Zval::object(a));
        assert!(!r.heap.mm.is_retired(a.address()));
        assert_eq!(r.heap.memory_usage(false), start);
    }
}
