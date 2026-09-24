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
use crate::gc::{self, GC_BLACK, GC_GREY, GC_PURPLE, GC_WHITE, Refcounted};
use crate::hooks::HeapHooks;
use crate::object::Obj;
use crate::reference::Ref;
use crate::{Heap, Zval};

/// `GC_INVALID`: index zero is never a root.
const INVALID: u32 = 0;
/// `GC_FIRST_ROOT`.
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

    fn gc(&self) -> &Refcounted {
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

/// One entry of `GC_G(buf)`: php's tagged pointer, its low two bits
/// the entry's kind (`GC_ROOT`, `GC_UNUSED`, `GC_GARBAGE`,
/// `GC_DTOR_GARBAGE`) and, for an unused one, the next unused index in
/// the bits above them.
#[derive(Clone, Copy, Debug)]
enum Entry {
    Root(Node),
    Unused(u32),
    Garbage(Node),
    DtorGarbage(Obj),
}

/// `zend_gc_globals`, request-scoped: the root buffer and the numbers
/// `gc_status()` reports.
#[derive(Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "php's `zend_gc_globals` keeps these four flags as `bool`s, and `gc_status()` reports three of them"
)]
pub(crate) struct Collector {
    /// `buf`, the entries below `first_unused`; the ones above it are
    /// php's allocated and unwritten, and are not laid down here.
    buf: Vec<Entry>,
    /// Whether `gc_enable` laid the buffer down (php's `buf != NULL`).
    allocated: bool,
    enabled: bool,
    active: bool,
    protected: bool,
    full: bool,
    unused: u32,
    first_unused: u32,
    threshold: u32,
    buf_size: u32,
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
    /// `threshold`.
    pub threshold: u32,
    /// `buffer_size`.
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
            buf: Vec::new(),
            allocated: false,
            enabled: false,
            active: false,
            protected: true,
            full: false,
            unused: INVALID,
            first_unused: INVALID,
            threshold: INVALID,
            buf_size: INVALID,
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
        if self.allocated {
            self.active = false;
            self.protected = false;
            self.full = false;
            self.unused = INVALID;
            self.first_unused = FIRST_ROOT;
            self.buf.clear();
            self.buf.push(Entry::Unused(INVALID));
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
        if enable && !old && !self.allocated {
            self.allocated = true;
            self.buf_size = DEFAULT_BUF_SIZE;
            self.threshold = THRESHOLD_DEFAULT;
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
            threshold: self.threshold,
            buffer_size: self.buf_size,
            roots: self.num_roots,
            application_time: self.activated_at.elapsed(),
            collector_time: self.collecting,
            destructor_time: self.destructing,
            free_time: self.freeing,
        }
    }

    fn entry(&self, idx: u32) -> Entry {
        self.buf
            .get(idx as usize)
            .copied()
            .unwrap_or(Entry::Unused(INVALID))
    }

    fn set(&mut self, idx: u32, entry: Entry) {
        if let Some(at) = self.buf.get_mut(idx as usize) {
            *at = entry;
        }
    }

    /// `gc_fetch_unused`: the head of the unused list, taken off it.
    fn fetch_unused(&mut self) -> u32 {
        let idx = self.unused;
        self.unused = match self.entry(idx) {
            Entry::Unused(next) => next,
            _ => INVALID,
        };
        idx
    }

    /// `gc_link_unused`: the entry onto the unused list.
    fn link_unused(&mut self, idx: u32) {
        self.set(idx, Entry::Unused(self.unused));
        self.unused = idx;
    }

    /// `gc_fetch_next_unused`: the first index never written.
    fn fetch_next_unused(&mut self) -> u32 {
        let idx = self.first_unused;
        self.first_unused += 1;
        self.buf.push(Entry::Unused(INVALID));
        idx
    }

    /// `gc_remove_from_roots`.
    fn remove_from_roots(&mut self, idx: u32) {
        self.link_unused(idx);
        self.num_roots = self.num_roots.saturating_sub(1);
    }

    /// A free index for a root, a garbage entry or an extra root, the
    /// buffer grown past its size where it must be (`gc_add_garbage`'s
    /// and `gc_extra_root`'s ladder); none where it cannot grow.
    fn fetch_any(&mut self) -> Option<u32> {
        if self.unused != INVALID {
            return Some(self.fetch_unused());
        }
        if self.first_unused != self.buf_size {
            return Some(self.fetch_next_unused());
        }
        self.grow_root_buffer();
        if self.first_unused == self.buf_size {
            return None;
        }
        Some(self.fetch_next_unused())
    }

    /// `gc_grow_root_buffer`. php warns `GC buffer overflow (GC
    /// disabled)` when the buffer is at its largest; a buffer of 2^30
    /// entries is sixteen gigabytes here, so the flags are set and the
    /// warning, which no request reaches, is not raised.
    fn grow_root_buffer(&mut self) {
        if self.buf_size >= MAX_BUF_SIZE && !self.full {
            self.active = true;
            self.protected = true;
            self.full = true;
            return;
        }
        let new_size = if self.buf_size < BUF_GROW_STEP {
            self.buf_size.saturating_mul(2)
        } else {
            self.buf_size.saturating_add(BUF_GROW_STEP)
        };
        self.buf_size = new_size.min(MAX_BUF_SIZE);
    }

    /// `gc_adjust_threshold`: a collection that freed little, or left the
    /// buffer at its threshold, raises the threshold a step, growing the
    /// buffer to hold it; one that freed enough lowers it a step, never
    /// below the default.
    fn adjust_threshold(&mut self, count: i64) {
        if count < THRESHOLD_TRIGGER || self.num_roots >= self.threshold {
            if self.threshold < THRESHOLD_MAX {
                let new_threshold = (self.threshold + THRESHOLD_STEP).min(THRESHOLD_MAX);
                if new_threshold > self.buf_size {
                    self.grow_root_buffer();
                }
                if new_threshold <= self.buf_size {
                    self.threshold = new_threshold;
                }
            }
        } else if self.threshold > THRESHOLD_DEFAULT {
            self.threshold = self
                .threshold
                .saturating_sub(THRESHOLD_STEP)
                .max(THRESHOLD_DEFAULT);
        }
    }

    /// Write `node` at `idx` as a root and put its compressed index and
    /// `color` in its header, counting it.
    fn place(&mut self, idx: u32, node: Node, color: u32) {
        self.set(idx, Entry::Root(node));
        node.gc().set_info(compress(idx) | color);
        self.num_roots += 1;
    }

    /// `gc_decompress`: the entry holding `node`, whose header keeps
    /// its index modulo 2^19 past the uncompressed range.
    fn decompress(&self, node: Node, mut idx: u32) -> Option<u32> {
        loop {
            if idx >= self.first_unused {
                return None;
            }
            if let Entry::Root(n) | Entry::Garbage(n) = self.entry(idx)
                && n.is_same(node)
            {
                return Some(idx);
            }
            if let Entry::DtorGarbage(o) = self.entry(idx)
                && Node::Obj(o).is_same(node)
            {
                return Some(idx);
            }
            idx += MAX_UNCOMPRESSED;
        }
    }

    /// `GC_REMOVE_FROM_BUFFER` and `gc_remove_from_buffer`: a value
    /// that carries the collector's bits has them cleared, and the entry
    /// its address names is put back on the unused list. What `zend_array_destroy` and
    /// `zend_objects_store_del` do before the value's memory goes, so the
    /// buffer never names freed memory.
    pub(crate) fn remove_from_buffer(&mut self, node: Node) {
        let header = node.gc();
        if header.info() == 0 {
            return;
        }
        let idx = header.address();
        header.set_info(0);
        let idx = if self.first_unused >= MAX_UNCOMPRESSED {
            self.decompress(node, idx)
        } else {
            Some(idx)
        };
        if let Some(idx) = idx
            && idx != INVALID
        {
            self.remove_from_roots(idx);
        }
    }

    /// `gc_compact`, the two-finger compaction: the roots moved down
    /// into the unused entries below them, each header's address
    /// rewritten, so the roots are the entries from the first on.
    fn compact(&mut self) {
        if self.num_roots + FIRST_ROOT == self.first_unused {
            return;
        }
        if self.num_roots != 0 {
            let mut free = FIRST_ROOT;
            let mut scan = self.first_unused - 1;
            let end = self.num_roots;
            while free < scan {
                while !matches!(self.entry(free), Entry::Unused(_)) {
                    free += 1;
                }
                while matches!(self.entry(scan), Entry::Unused(_)) {
                    scan -= 1;
                }
                if scan > free {
                    let moved = self.entry(scan);
                    self.set(free, moved);
                    if let Some(node) = entry_node(moved) {
                        let color = node.gc().color();
                        node.gc().set_info(compress(free) | color);
                    }
                    free += 1;
                    scan -= 1;
                    if scan <= end {
                        break;
                    }
                }
            }
        }
        self.unused = INVALID;
        self.first_unused = self.num_roots + FIRST_ROOT;
        self.buf.truncate(self.first_unused as usize);
    }

    /// `gc_add_garbage`: a white value that was not a root, added as
    /// garbage for the free phase, black.
    fn add_garbage(&mut self, node: Node) {
        if let Some(idx) = self.fetch_any() {
            self.set(idx, Entry::Garbage(node));
            node.gc().set_info(compress(idx) | GC_BLACK);
            self.num_roots += 1;
        }
    }
}

/// The value an entry names, whatever its kind.
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

/// `GC_HAS_DESTRUCTORS`: a garbage object whose destructor is still to
/// be served was found.
const HAS_DESTRUCTORS: u32 = 1;

/// The children php's walk reads off one value, `get_gc` for an object:
/// the zvals in `kids`, and the table php's handler hands back beside
/// them, whose own entries follow in `kids` from the index returned.
/// For an object with a dynamic table that is the table, walked after
/// what the handler names, its declared slots first, as php's
/// `rebuild_object_properties` puts the slots' `IS_INDIRECT` rows first;
/// for one without, the handler's zvals and then the slots, php's
/// `properties_table`.
fn children(hooks: &mut dyn HeapHooks, node: Node, kids: &mut Vec<Zval>) -> (Option<Arr>, usize) {
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
        Node::Obj(o) => {
            hooks.gc_children(o, &mut |z| kids.push(z));
            let table = o.properties();
            let split = if table.is_some() { kids.len() } else { 0 };
            for i in 0..o.slots() {
                kids.push(o.slot(i));
            }
            if let Some(table) = table {
                table.gc_values(kids);
            }
            (table.filter(|t| !t.is_immutable()), split)
        }
    }
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
    if g.color() == GC_GREY {
        return false;
    }
    g.set_color(GC_GREY);
    true
}

/// `gc_scan_black`'s step: the owner given back, and black where it was
/// not.
fn blacken(child: Node) -> bool {
    let g = child.gc();
    g.addref();
    if g.color() == GC_BLACK {
        return false;
    }
    g.set_color(GC_BLACK);
    true
}

/// `gc_scan`'s step: a grey child whitened.
fn whiten(child: Node) -> bool {
    let g = child.gc();
    if g.color() != GC_GREY {
        return false;
    }
    g.set_color(GC_WHITE);
    true
}

/// `gc_collect_white`'s step: the owner given back, and a white child
/// blackened.
fn collect(child: Node) -> bool {
    let g = child.gc();
    g.addref();
    if g.color() != GC_WHITE {
        return false;
    }
    g.set_color(GC_BLACK);
    true
}

/// Whether the walk may read the object's members: not once `free_obj`
/// ran over them (`IS_OBJ_FREE_CALLED`).
fn walkable(o: Obj) -> bool {
    !o.has_flag(gc::OBJ_FREE_CALLED)
}

impl Heap {
    /// `gc_check_possible_root` (`Z/Zend/zend_gc.h:114`): what a release
    /// that left owners does with the value. A reference is looked
    /// through to the value it holds; an array or an object not in the
    /// buffer yet and not marked uncollectable is a possible root.
    pub(crate) fn gc_check_possible_root(hooks: &mut dyn HeapHooks, z: Zval) {
        let node = match Node::of(z) {
            Some(Node::Ref(r)) => match Node::of(r.val()) {
                Some(Node::Ref(_)) | None => return,
                Some(held) => held,
            },
            Some(node) => node,
            None => return,
        };
        if node.gc().may_leak() {
            Heap::gc_possible_root(hooks, node);
        }
    }

    /// `gc_possible_root` (`Z/Zend/zend_gc.c:738`): the value in the
    /// buffer, purple, in an unused entry or the next one under the
    /// threshold; at the threshold, `gc_possible_root_when_full`.
    fn gc_possible_root(hooks: &mut dyn HeapHooks, node: Node) {
        let gc = &mut hooks.heap().gc;
        if gc.protected {
            return;
        }
        if gc.unused != INVALID {
            let idx = gc.fetch_unused();
            gc.place(idx, node, GC_PURPLE);
        } else if gc.first_unused < gc.threshold {
            let idx = gc.fetch_next_unused();
            gc.place(idx, node, GC_PURPLE);
        } else {
            Heap::gc_possible_root_when_full(hooks, node);
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
            let count = Heap::gc_collect_cycles(hooks);
            hooks.heap().gc.adjust_threshold(count);
            if node.gc().delref() == 0 {
                match node {
                    Node::Arr(a) => Heap::array_destroy(hooks, a),
                    Node::Obj(o) => Heap::store_del(hooks, o),
                    Node::Ref(_) => {}
                }
                return;
            }
            if node.gc().info() != 0 {
                return;
            }
        }
        let gc = &mut hooks.heap().gc;
        let idx = if gc.unused != INVALID {
            gc.fetch_unused()
        } else if gc.first_unused != gc.buf_size {
            gc.fetch_next_unused()
        } else {
            gc.grow_root_buffer();
            if gc.first_unused == gc.buf_size {
                return;
            }
            gc.fetch_next_unused()
        };
        gc.place(idx, node, GC_PURPLE);
    }

    /// `gc_mark_grey`: every value reachable from `root` one owner fewer
    /// for each owner inside the walk, and grey.
    fn gc_mark_grey(hooks: &mut dyn HeapHooks, root: Node, kids: &mut Vec<Zval>) {
        let mut stack = Vec::new();
        let mut node = root;
        loop {
            let next = match node {
                Node::Obj(o) if walkable(o) => match children(hooks, node, kids) {
                    (Some(table), split) => {
                        let g = table.gc();
                        g.delref();
                        if g.color() == GC_GREY {
                            walk(&kids[..split], &mut stack, grey)
                        } else {
                            g.set_color(GC_GREY);
                            push_all(&kids[..split], &mut stack, grey);
                            walk(&kids[split..], &mut stack, grey)
                        }
                    }
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
                Node::Obj(o) if walkable(o) => match children(hooks, node, kids) {
                    (Some(table), split) => {
                        let g = table.gc();
                        g.addref();
                        if g.color() == GC_BLACK {
                            walk(&kids[..split], &mut stack, blacken)
                        } else {
                            g.set_color(GC_BLACK);
                            push_all(&kids[..split], &mut stack, blacken);
                            walk(&kids[split..], &mut stack, blacken)
                        }
                    }
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
            let next = if g.color() != GC_WHITE {
                None
            } else if g.refcount() > 0 {
                g.set_color(GC_BLACK);
                Heap::gc_scan_black(hooks, node, kids);
                None
            } else {
                match node {
                    Node::Obj(o) if walkable(o) => match children(hooks, node, kids) {
                        (Some(table), split) => {
                            let g = table.gc();
                            if g.color() == GC_GREY {
                                g.set_color(GC_WHITE);
                                stack.push(Node::Arr(table));
                                push_all(&kids[..split], &mut stack, whiten);
                                walk(&kids[split..], &mut stack, whiten)
                            } else {
                                walk(&kids[..split], &mut stack, whiten)
                            }
                        }
                        (None, _) => walk(kids, &mut stack, whiten),
                    },
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
        flags: &mut u32,
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
                    if o.gc().info() == 0 {
                        hooks.heap().gc.add_garbage(node);
                    }
                    if !o.has_flag(gc::OBJ_DESTRUCTOR_CALLED) && hooks.has_destructor(o) {
                        *flags |= HAS_DESTRUCTORS;
                    }
                    match children(hooks, node, kids) {
                        (Some(table), split) => {
                            let g = table.gc();
                            g.addref();
                            if g.color() == GC_WHITE {
                                g.set_color(GC_BLACK);
                                push_all(&kids[..split], &mut stack, collect);
                                walk(&kids[split..], &mut stack, collect)
                            } else {
                                walk(&kids[..split], &mut stack, collect)
                            }
                        }
                        (None, _) => walk(kids, &mut stack, collect),
                    }
                }
                Node::Obj(_) => None,
                Node::Arr(_) => {
                    if node.gc().info() == 0 {
                        hooks.heap().gc.add_garbage(node);
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
            } else if node.gc().address() != 0 && node.gc().color() == GC_BLACK {
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
                    Node::Obj(o) if walkable(o) => match children(hooks, node, kids) {
                        (Some(table), split) => {
                            push_all(&kids[..split], &mut stack, |_| true);
                            let g = table.gc();
                            if g.address() != 0 && g.color() == GC_BLACK {
                                hooks.heap().gc.remove_from_buffer(Node::Arr(table));
                            }
                            walk(&kids[split..], &mut stack, |_| true)
                        }
                        (None, _) => walk(kids, &mut stack, |_| true),
                    },
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
        while idx < hooks.heap().gc.first_unused {
            if let Entry::Root(node) = hooks.heap().gc.entry(idx)
                && node.gc().color() == GC_PURPLE
            {
                node.gc().set_color(GC_GREY);
                Heap::gc_mark_grey(hooks, node, kids);
            }
            idx += 1;
        }
    }

    /// `gc_scan_roots`: every grey root whitened and scanned, and then
    /// the extra roots the scan added.
    fn gc_scan_roots(hooks: &mut dyn HeapHooks, kids: &mut Vec<Zval>) {
        let mut idx = FIRST_ROOT;
        while idx < hooks.heap().gc.first_unused {
            if let Entry::Root(node) = hooks.heap().gc.entry(idx)
                && node.gc().color() == GC_GREY
            {
                node.gc().set_color(GC_WHITE);
                Heap::gc_scan(hooks, node, kids);
            }
            idx += 1;
        }
    }

    /// `gc_collect_roots`: the live roots out of the buffer, the rest
    /// compacted to its front and each made garbage, a white one walked
    /// for the garbage it reaches.
    fn gc_collect_roots(hooks: &mut dyn HeapHooks, flags: &mut u32, kids: &mut Vec<Zval>) -> i64 {
        let gc = &mut hooks.heap().gc;
        for idx in FIRST_ROOT..gc.first_unused {
            if let Entry::Root(node) = gc.entry(idx)
                && node.gc().color() == GC_BLACK
            {
                node.gc().set_info(0);
                gc.remove_from_roots(idx);
            }
        }
        gc.compact();
        let end = gc.first_unused;
        let mut count = 0;
        for idx in FIRST_ROOT..end {
            let gc = &mut hooks.heap().gc;
            let Entry::Root(node) = gc.entry(idx) else {
                continue;
            };
            gc.set(idx, Entry::Garbage(node));
            if node.gc().color() == GC_WHITE {
                node.gc().set_color(GC_BLACK);
                count += Heap::gc_collect_white(hooks, node, flags, kids);
            }
        }
        count
    }

    /// `gc_call_destructors`: the destructor of every object in the
    /// entries from `from` to `end` that is to have one served, each
    /// entry made a root again for the next collection, and an object whose
    /// destructor another destructor served already passed over. The
    /// buffer may grow meanwhile, so each entry is read afresh.
    fn gc_call_destructors(hooks: &mut dyn HeapHooks, from: u32, end: u32) {
        let mut idx = from;
        while idx != end {
            if let Entry::DtorGarbage(o) = hooks.heap().gc.entry(idx) {
                hooks.heap().gc.set(idx, Entry::Root(Node::Obj(o)));
                if !o.has_flag(gc::OBJ_DESTRUCTOR_CALLED) {
                    o.add_flag(gc::OBJ_DESTRUCTOR_CALLED);
                    o.addref();
                    hooks.destruct(o);
                    o.delref();
                }
            }
            idx += 1;
        }
    }

    /// The free phase of `zend_gc_collect_cycles`: every garbage object
    /// out of the store, turned to `IS_NULL` and its members freed once,
    /// every garbage table turned to `IS_NULL` and its contents
    /// destroyed; then, in a second pass, what is still garbage off the
    /// buffer and its bytes back to the heap.
    fn gc_free_garbage(hooks: &mut dyn HeapHooks, end: u32) {
        for idx in FIRST_ROOT..end {
            let Entry::Garbage(node) = hooks.heap().gc.entry(idx) else {
                continue;
            };
            match node {
                Node::Obj(o) => {
                    let handle = o.id().0;
                    hooks.heap().store.invalidate(handle);
                    o.gc().set_kind_null();
                    if !o.has_flag(gc::OBJ_FREE_CALLED) {
                        o.add_flag(gc::OBJ_FREE_CALLED);
                        o.addref();
                        Heap::object_free(hooks, o);
                        o.delref();
                    }
                    hooks.heap().store.add_to_free_list(handle);
                }
                Node::Arr(a) => {
                    a.gc().set_kind_null();
                    Heap::array_free_contents(hooks, a);
                }
                Node::Ref(_) => {}
            }
        }
        let heap = hooks.heap();
        for idx in FIRST_ROOT..end {
            let Entry::Garbage(node) = heap.gc.entry(idx) else {
                continue;
            };
            heap.gc.link_unused(idx);
            heap.gc.num_roots = heap.gc.num_roots.saturating_sub(1);
            let at = match node {
                Node::Obj(o) => o.address(),
                Node::Arr(a) => a.address(),
                Node::Ref(_) => continue,
            };
            // SAFETY: the value was garbage, freed of its contents above,
            // out of the store and off the buffer, so nothing names it.
            unsafe { heap.mm.free(at) };
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
            let mut flags = 0;
            let mut count = Heap::gc_collect_roots(hooks, &mut flags, &mut kids);
            if hooks.heap().gc.num_roots == 0 {
                hooks.heap().gc.active = false;
                break;
            }
            let end = hooks.heap().gc.first_unused;
            if flags & HAS_DESTRUCTORS != 0 {
                should_rerun = true;
                for idx in FIRST_ROOT..end {
                    if let Entry::Garbage(Node::Obj(o)) = hooks.heap().gc.entry(idx)
                        && !o.has_flag(gc::OBJ_DESTRUCTOR_CALLED)
                    {
                        if hooks.has_destructor(o) {
                            hooks.heap().gc.set(idx, Entry::DtorGarbage(o));
                            o.gc().set_color(GC_PURPLE);
                        } else {
                            o.add_flag(gc::OBJ_DESTRUCTOR_CALLED);
                        }
                    }
                }
                for idx in FIRST_ROOT..end {
                    if let Entry::DtorGarbage(o) = hooks.heap().gc.entry(idx) {
                        count -= Heap::gc_remove_nested_data(hooks, Node::Obj(o), &mut kids);
                    }
                }
                let dtor_start = Instant::now();
                if !hooks.gc_destructors_in_fiber(end) {
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
        hooks.heap().gc.collecting += start.elapsed();
        total
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
    }

    impl Recorder {
        fn new() -> Recorder {
            Recorder {
                heap: Heap::new(),
                served: Vec::new(),
                keep: None,
                kept: Vec::new(),
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
        fn gc_children(&mut self, _: Obj, _: &mut dyn FnMut(Zval)) {}
        fn free_resource(&mut self, _: crate::Res, _: crate::ResourceType) {}
        fn out_of_memory(&mut self, bytes: usize) -> Fatal {
            Fatal::OutOfMemory(bytes)
        }
    }

    /// Two objects, each in the other's slot: `$a->b = $b; $b->a = $a;`.
    fn pair(r: &mut Recorder) -> Option<(Obj, Obj)> {
        let a = r.heap.object(1, 1).ok()?;
        let b = r.heap.object(1, 1).ok()?;
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
}
