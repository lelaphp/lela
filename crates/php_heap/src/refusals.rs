//! Every place the request's heap can refuse an allocation, driven to the
//! refusal (#3199, out of #3162's reading of CVE-2026-109618). php leaves
//! an allocation the memory limit refuses by a jump, and its request end
//! then reads memory the refused path had published and not finished.
//! Here the refusal is an `Err` passed up, so the question is the one the
//! jump leaves behind in a different form: whether a path that meets the
//! limit leaves what it touched as something the request's end, the
//! shutdown functions and the destructors after it can still read and
//! write. Under Miri (`cargo xtask miri`) a read of memory such a path
//! left uninitialised, or gave back, or a write past what it holds, is
//! undefined behaviour Miri names; natively the tests hold the
//! fatal's text, every value read back through its own lookups, and the
//! debug build's freed-use sensors.
//!
//! Two kinds of refusal point, two tests:
//!
//! - **The allocator's own arms of the limit**, each reached through the
//!   limit itself, at a heap whose one chunk is full
//!   ([`each_arm_of_the_allocator_the_limit_reaches_refuses_and_leaves_the_block_it_had`]).
//!   Three arms the limit cannot reach are the system's alone, and are
//!   not here: `grow` and `alloc_huge` given no mapping (`Out of memory`),
//!   and a huge block's list node, asked of its bin after the mapping with
//!   at least a chunk of the limit still free.
//! - **Every entry point above it**, refused at each of its asks in turn
//!   ([`POINTS`]): a heap at its limit refuses whichever ask finds no room,
//!   which a test cannot stage for the `n`th ask of an operation, so the
//!   allocator is armed to refuse that ask as the limit refuses it
//!   (`Mm::refuse_after`). What the operation touched is then read and
//!   written as a shutdown function would, the operation served again
//!   whole, and everything released. The cycle collector's root buffer
//!   and the interned strings are the system allocator's, which no
//!   request's limit holds, and are not here.

use std::ffi::c_void;
use std::ptr::NonNull;

use crate::serial::alone;
use crate::{
    Arr, Block, Fatal, FatalReason, Frame, GcTable, Heap, HeapHooks, Key, Obj, Res, ResourceType,
    VmStack, Zval, ZvalType, efree, emalloc, erealloc,
};

/// One page of a chunk, the smallest `run` the allocator hands out.
const PAGE: usize = 4096;

/// A request as the heap sees one: its heap, and what the test holds of
/// it, each kind of value in the place a request keeps it.
struct Request {
    heap: Heap,
    /// One owner each.
    held: Vec<Zval>,
    blocks: Vec<Block>,
    stack: VmStack<u32>,
    /// Frames off the stack, as a generator keeps its own.
    frames: Vec<Frame<u32>>,
    /// Iterators, as a `foreach` by reference keeps its own.
    iterators: Vec<u32>,
}

impl Request {
    fn new() -> Request {
        Request {
            heap: Heap::new(),
            held: Vec::new(),
            blocks: Vec::new(),
            stack: VmStack::new(),
            frames: Vec::new(),
            iterators: Vec::new(),
        }
    }

    /// The array the test holds at `at`.
    fn arr(&self, at: usize) -> Arr {
        self.held[at]
            .as_arr()
            .unwrap_or_else(|| panic!("held[{at}] is not an array"))
    }

    /// The object the test holds at `at`.
    fn obj(&self, at: usize) -> Obj {
        self.held[at]
            .as_obj()
            .unwrap_or_else(|| panic!("held[{at}] is not an object"))
    }

    /// What a shutdown function does with what a refused operation
    /// touched: every array the test holds alone written at every row its
    /// table says it has room for, and one past, and read back; then
    /// every value read through its own lookups ([`walk`]).
    fn use_after(&mut self, name: &str) {
        for z in self.held.clone() {
            let Some(a) = z.as_arr() else {
                continue;
            };
            if a.refcount() != 1 || a.is_immutable() {
                continue;
            }
            let room = i64::from(a.table_size());
            for i in 0..=room {
                Heap::array_update(self, a, Key::Long(i), Zval::long(i))
                    .unwrap_or_else(|f| panic!("{name}: row {i} of {room}: {f:?}"));
            }
            for i in 0..=room {
                assert_eq!(
                    a.get(Key::Long(i)).and_then(Zval::as_long),
                    Some(i),
                    "{name}: row {i} of {room} reads back"
                );
            }
        }
        for z in &self.held {
            walk(&self.heap, *z, name);
        }
    }

    /// The request's end: what it holds read once more and released, the
    /// stack closed, and the debug build's sensors asked what they found.
    fn end(mut self, name: &str) {
        for z in &self.held {
            walk(&self.heap, *z, name);
        }
        for idx in std::mem::take(&mut self.iterators) {
            self.heap.iterator_del(idx);
        }
        for z in std::mem::take(&mut self.held).into_iter().rev() {
            Heap::release(&mut self, z);
        }
        for block in std::mem::take(&mut self.blocks) {
            self.heap.free_block(block);
        }
        for frame in std::mem::take(&mut self.frames) {
            frame.free(&mut self.heap);
        }
        let _headers = self.stack.close(&mut self.heap);
        self.heap.gc_mem_caches();
        assert_eq!(self.heap.take_sensed(), None, "{name}: the sensors");
    }
}

impl HeapHooks for Request {
    fn heap(&mut self) -> &mut Heap {
        &mut self.heap
    }
    fn destruct(&mut self, _: Obj) {}
    fn free_native(&mut self, _: Obj) {}
    fn gc_children(&mut self, _: Obj, _: &mut dyn FnMut(Zval)) -> GcTable {
        GcTable::Properties
    }
    fn free_resource(&mut self, _: Res, _: ResourceType) {}
    fn out_of_memory(&mut self, bytes: usize) -> Fatal {
        Fatal::out_of_memory(bytes)
    }
}

/// `z` read through and through: a string's ends; an array's room held
/// by its data, and its every row found again through its own lookup
/// and counted against its length; an object's slots and table; a
/// reference's value.
fn walk(heap: &Heap, z: Zval, name: &str) {
    match z.kind() {
        ZvalType::String => {
            if let Some(s) = z.as_str() {
                let bytes = s.bytes();
                std::hint::black_box((bytes.first(), bytes.last()));
            }
        }
        ZvalType::Array => {
            let Some(a) = z.as_arr() else { return };
            assert!(a.holds_its_room(heap), "{name}: the room its size says");
            let mut rows = 0;
            for (key, value) in &a {
                assert_eq!(a.get(key), Some(value), "{name}: {key:?} found again");
                walk(heap, value, name);
                rows += 1;
            }
            assert_eq!(
                rows,
                a.len(),
                "{name}: the rows walked are the rows counted"
            );
        }
        ZvalType::Object => {
            let Some(o) = z.as_obj() else { return };
            for i in 0..o.slots() {
                walk(heap, o.slot(i), name);
            }
            if let Some(table) = o.properties() {
                walk(heap, Zval::array(table), name);
            }
        }
        ZvalType::Reference => {
            if let Some(r) = z.as_reference() {
                walk(heap, r.val(), name);
            }
        }
        _ => {}
    }
}

/// Whether `refused` is the limit's fatal.
fn is_the_limit<T>(refused: &Result<T, Fatal>) -> bool {
    matches!(
        refused.as_ref().map_err(Fatal::reason),
        Err(FatalReason::Error(text)) if text.starts_with("Allowed memory size of ")
    )
}

/// One entry point of the heap that asks the allocator, refused at each
/// of its asks in turn.
struct Point {
    /// Where the refusal lands, as a failure names it.
    name: &'static str,
    /// What the operation needs, laid down before the limit is armed.
    lay: fn(&mut Request) -> Result<(), Fatal>,
    /// The operation: refused at one of its asks, served again whole once
    /// what it touched has been used, and once more unarmed to count its
    /// asks.
    op: fn(&mut Request) -> Result<(), Fatal>,
}

/// A point that lays nothing down.
#[expect(
    clippy::unnecessary_wraps,
    reason = "a `Point`'s `lay`, whose type every point shares"
)]
fn nothing(_: &mut Request) -> Result<(), Fatal> {
    Ok(())
}

/// A string of `len` bytes, held.
fn hold_string(r: &mut Request, bytes: &[u8]) -> Result<(), Fatal> {
    let s = r.heap.string(bytes)?;
    r.held.push(Zval::string(s));
    Ok(())
}

/// A packed list of eight longs, its room full, held.
fn full_list(r: &mut Request) -> Result<(), Fatal> {
    let a = r.heap.array_packed(8)?;
    r.held.push(Zval::array(a));
    for i in 0..8 {
        r.heap.array_append(a, Zval::long(i))?;
    }
    Ok(())
}

/// A packed list of four strings, held twice, so a write separates it.
fn shared_list(r: &mut Request) -> Result<(), Fatal> {
    let a = r.heap.array_packed(8)?;
    r.held.push(Zval::array(a));
    for i in 0..4 {
        let s = r.heap.string(format!("element {i}").as_bytes())?;
        r.heap.array_append(a, Zval::string(s))?;
    }
    a.addref();
    r.held.push(Zval::array(a));
    Ok(())
}

/// A hash of eight string keys, its room full, held.
fn full_hash(r: &mut Request) -> Result<(), Fatal> {
    let a = r.heap.array_mixed(8)?;
    r.held.push(Zval::array(a));
    for i in 0..8 {
        let key = r.heap.string(format!("key {i}").as_bytes())?;
        Heap::array_update(r, a, Key::Str(key), Zval::long(8 - i))?;
        r.heap.release_string(key);
    }
    Ok(())
}

/// A hash of eight string keys, held twice, so a write separates it.
fn shared_hash(r: &mut Request) -> Result<(), Fatal> {
    full_hash(r)?;
    let a = r.arr(0);
    a.addref();
    r.held.push(Zval::array(a));
    Ok(())
}

/// An object of two slots, a string and a long, held.
fn object(r: &mut Request) -> Result<(), Fatal> {
    let o = r.heap.object(1, 2, 0)?;
    r.held.push(Zval::object(o));
    let s = r.heap.string(b"slot")?;
    Heap::object_set_slot(r, o, 0, Zval::string(s));
    Heap::object_set_slot(r, o, 1, Zval::long(1));
    Ok(())
}

/// [`object`] with a dynamic table of two rows.
fn object_with_table(r: &mut Request) -> Result<(), Fatal> {
    object(r)?;
    let o = r.obj(0);
    let table = r.heap.object_properties(o)?;
    for name in [&b"dynamic"[..], b"other"] {
        let key = r.heap.string(name)?;
        let value = r.heap.string(b"value")?;
        Heap::array_update(r, table, Key::Str(key), Zval::string(value))?;
        r.heap.release_string(key);
    }
    Ok(())
}

/// Eight resources, the room of the request's list full, held.
fn eight_resources(r: &mut Request) -> Result<(), Fatal> {
    for _ in 0..8 {
        let res = r.heap.resource(ResourceType(1))?;
        r.held.push(Zval::resource(res));
    }
    Ok(())
}

/// The value `entry` holds, as a long, for a sort.
fn long_of(entry: &crate::SortEntry) -> i64 {
    entry.val.as_long().unwrap_or(0)
}

/// Every entry point of the heap that asks the allocator, by the file
/// that asks: strings (`lib.rs`, `string.rs`), blocks, arrays and the
/// sort, objects and their store, tables copied, references, resources
/// and their list, the compiler's arena, the VM stack, iterators, and C
/// under a lend.
const POINTS: &[Point] = &[
    Point {
        name: "a string",
        lay: nothing,
        op: |r| hold_string(r, &[b's'; 40]),
    },
    Point {
        name: "a huge string",
        lay: nothing,
        op: |r| {
            let s = r.heap.string_with(3_000_000, |bytes| bytes.fill(b'h'))?;
            r.held.push(Zval::string(s));
            Ok(())
        },
    },
    Point {
        name: "a fresh string grown into a run",
        lay: nothing,
        op: |r| {
            let fresh = r.heap.string_fresh(16)?;
            let fresh = r.heap.string_extend(fresh, 5000)?;
            r.held.push(Zval::string(fresh.finish()));
            Ok(())
        },
    },
    Point {
        name: "a fresh string cut to a lower bin",
        lay: nothing,
        op: |r| {
            let fresh = r.heap.string_fresh(3000)?;
            let fresh = r.heap.string_truncate(fresh, 8)?;
            r.held.push(Zval::string(fresh.finish()));
            Ok(())
        },
    },
    Point {
        name: "an append to a string of one owner",
        lay: |r| hold_string(r, &[b'a'; 100]),
        op: |r| {
            let s = r.held[0].as_str().unwrap_or_else(|| panic!("a string"));
            let tail = r.heap.string(&[b't'; 3000])?;
            r.held.push(Zval::string(tail));
            r.held[0] = Zval::string(r.heap.string_append(s, tail)?);
            Ok(())
        },
    },
    Point {
        name: "an append to a shared string",
        lay: |r| {
            hold_string(r, &[b'a'; 100])?;
            r.held[0].addref();
            r.held.push(r.held[0]);
            Ok(())
        },
        op: |r| {
            let s = r.held[0].as_str().unwrap_or_else(|| panic!("a string"));
            let tail = r.heap.string(b"tail")?;
            r.held.push(Zval::string(tail));
            r.held[0] = Zval::string(r.heap.string_append(s, tail)?);
            Ok(())
        },
    },
    Point {
        name: "a block",
        lay: nothing,
        op: |r| {
            let block = r.heap.block(64)?;
            r.blocks.push(block);
            Ok(())
        },
    },
    Point {
        name: "a block resized into a run",
        lay: nothing,
        op: |r| {
            let block = r.heap.block(64)?;
            let block = r.heap.block_resize(block, 6000)?;
            r.blocks.push(block);
            Ok(())
        },
    },
    Point {
        name: "an empty array",
        lay: nothing,
        op: |r| {
            let a = r.heap.array()?;
            r.held.push(Zval::array(a));
            Ok(())
        },
    },
    Point {
        name: "a list laid down",
        lay: nothing,
        op: |r| {
            let a = r.heap.array_packed(8)?;
            r.held.push(Zval::array(a));
            Ok(())
        },
    },
    Point {
        name: "a hash laid down",
        lay: nothing,
        op: |r| {
            let a = r.heap.array_mixed(8)?;
            r.held.push(Zval::array(a));
            Ok(())
        },
    },
    Point {
        name: "an append past a list's room",
        lay: full_list,
        op: |r| {
            let a = r.arr(0);
            r.heap.array_append(a, Zval::long(8)).map(drop)
        },
    },
    Point {
        name: "an index far past a full list's room",
        lay: full_list,
        op: |r| {
            let a = r.arr(0);
            Heap::array_update(r, a, Key::Long(100), Zval::long(100))
        },
    },
    Point {
        name: "a string key written to a list",
        lay: |r| {
            full_list(r)?;
            hold_string(r, b"key")
        },
        op: |r| {
            let (a, key) = (r.arr(0), r.held[1].as_str());
            let key = key.unwrap_or_else(|| panic!("a string"));
            Heap::array_update(r, a, Key::Str(key), Zval::long(1))
        },
    },
    Point {
        name: "a key past a full hash's room",
        lay: |r| {
            full_hash(r)?;
            hold_string(r, b"ninth")
        },
        op: |r| {
            let (a, key) = (r.arr(0), r.held[1].as_str());
            let key = key.unwrap_or_else(|| panic!("a string"));
            Heap::array_update(r, a, Key::Str(key), Zval::long(9))
        },
    },
    Point {
        name: "a key made after a full hash's room",
        lay: full_hash,
        op: |r| {
            let a = r.arr(0);
            Heap::array_str_update(r, a, b"made after the room", Zval::long(9))
        },
    },
    Point {
        name: "a key added new past a full hash's room",
        lay: |r| {
            full_hash(r)?;
            hold_string(r, b"new")
        },
        op: |r| {
            let (a, key) = (r.arr(0), r.held[1].as_str());
            let key = key.unwrap_or_else(|| panic!("a string"));
            r.heap.array_add_new(a, Key::Str(key), Zval::long(9))
        },
    },
    Point {
        name: "a lookup that adds past a list's room",
        lay: full_list,
        op: |r| {
            let a = r.arr(0);
            r.heap.array_lookup(a, Key::Long(8)).map(drop)
        },
    },
    Point {
        name: "a next entry past a list's room",
        lay: full_list,
        op: |r| {
            let a = r.arr(0);
            r.heap.array_next_entry(a).map(drop)
        },
    },
    Point {
        name: "a list extended",
        lay: full_list,
        op: |r| {
            let a = r.arr(0);
            r.heap.array_extend(a, 100)
        },
    },
    Point {
        name: "a hash extended",
        lay: full_hash,
        op: |r| {
            let a = r.arr(0);
            r.heap.array_extend(a, 100)
        },
    },
    Point {
        name: "a list copied",
        lay: shared_list,
        op: |r| {
            let copy = r.heap.array_dup(r.arr(0))?;
            r.held.push(Zval::array(copy));
            Ok(())
        },
    },
    Point {
        name: "a hash copied",
        lay: shared_hash,
        op: |r| {
            let copy = r.heap.array_dup(r.arr(0))?;
            r.held.push(Zval::array(copy));
            Ok(())
        },
    },
    Point {
        name: "a shared list separated",
        lay: shared_list,
        op: |r| {
            let a = r.arr(1);
            if a.refcount() > 1 {
                r.held[1] = Zval::array(r.heap.separate(a)?);
            }
            Ok(())
        },
    },
    Point {
        name: "a list sorted",
        lay: full_list,
        op: |r| {
            let a = r.arr(0);
            Heap::array_sort(r, a, false, &mut |_, x, y| long_of(y).cmp(&long_of(x)))
        },
    },
    Point {
        name: "a hash sorted and renumbered",
        lay: full_hash,
        op: |r| {
            let a = r.arr(0);
            Heap::array_sort(r, a, true, &mut |_, x, y| long_of(x).cmp(&long_of(y)))
        },
    },
    Point {
        name: "an iterator moved onto a shared table",
        lay: |r| {
            full_list(r)?;
            shared_list(r)?;
            let idx = r.heap.iterator_add(r.arr(0), 0);
            r.iterators.push(idx);
            Ok(())
        },
        op: |r| {
            let (idx, b) = (r.iterators[0], r.arr(2));
            let (_, separated) = r.heap.iterator_pos_ex(idx, b)?;
            if !separated.is_same(b) {
                r.held[2] = Zval::array(separated);
            }
            Ok(())
        },
    },
    Point {
        name: "a symbol table made a property table",
        lay: full_list,
        op: |r| {
            let a = r.arr(0);
            let table = Heap::symtable_to_proptable(r, a)?;
            r.held.push(Zval::array(table));
            Ok(())
        },
    },
    Point {
        name: "an object",
        lay: nothing,
        op: object,
    },
    Point {
        name: "an object past the store's room",
        lay: |r| {
            // The store's 1024 buckets, the first never handed out.
            for _ in 1..1024 {
                let o = r.heap.object(1, 0, 0)?;
                r.held.push(Zval::object(o));
            }
            Ok(())
        },
        op: |r| {
            let o = r.heap.object(1, 0, 0)?;
            r.held.push(Zval::object(o));
            Ok(())
        },
    },
    Point {
        name: "an object's table laid down",
        lay: object,
        op: |r| r.heap.object_properties(r.obj(0)).map(drop),
    },
    Point {
        name: "an object's property table laid down",
        lay: object,
        op: |r| r.heap.object_property_table(r.obj(0)).map(drop),
    },
    Point {
        name: "an object's table extended",
        lay: object_with_table,
        op: |r| r.heap.object_properties_extend(r.obj(0), 20).map(drop),
    },
    Point {
        name: "an object cloned",
        lay: object_with_table,
        op: |r| {
            if let Some(clone) = r.heap.object_clone(r.obj(0), 0)? {
                r.held.push(Zval::object(clone));
            }
            Ok(())
        },
    },
    Point {
        name: "an object's shared table copied",
        lay: |r| {
            object(r)?;
            let table = r.heap.object_properties(r.obj(0))?;
            table.addref();
            r.held.push(Zval::array(table));
            hold_string(r, b"first")?;
            hold_string(r, b"second")
        },
        op: |r| {
            let o = r.obj(0);
            if let Some(table) = o.properties()
                && table.refcount() == 1
            {
                table.addref();
                r.held.push(Zval::array(table));
            }
            let names: Vec<_> = r.held.iter().skip(2).filter_map(|z| z.as_str()).collect();
            r.heap.object_properties_copy(o, &names).map(drop)
        },
    },
    Point {
        name: "a reference",
        lay: nothing,
        op: |r| {
            let cell = r.heap.reference(Zval::long(1))?;
            r.held.push(Zval::reference(cell));
            Ok(())
        },
    },
    Point {
        name: "a reference's shared array separated",
        lay: |r| {
            shared_list(r)?;
            let a = r.held.pop().unwrap_or_else(|| panic!("the second owner"));
            let cell = r.heap.reference(a)?;
            r.held.push(Zval::reference(cell));
            Ok(())
        },
        op: |r| {
            let cell = r.held[1].as_reference();
            let cell = cell.unwrap_or_else(|| panic!("a reference"));
            r.heap.reference_separate(cell).map(drop)
        },
    },
    Point {
        name: "a resource",
        lay: nothing,
        op: |r| {
            let res = r.heap.resource(ResourceType(1))?;
            r.held.push(Zval::resource(res));
            Ok(())
        },
    },
    Point {
        name: "a resource past the list's room",
        lay: eight_resources,
        op: |r| {
            let res = r.heap.resource(ResourceType(1))?;
            r.held.push(Zval::resource(res));
            Ok(())
        },
    },
    Point {
        name: "a resource with a payload",
        lay: nothing,
        op: |r| {
            let res = r
                .heap
                .resource_with(ResourceType(1), String::from("payload"))?;
            r.held.push(Zval::resource(res));
            Ok(())
        },
    },
    Point {
        name: "the compiler's arena past its block",
        lay: nothing,
        op: |r| r.heap.carve(70_000),
    },
    Point {
        name: "the VM stack's first page",
        lay: nothing,
        op: |r| r.stack.open(&mut r.heap),
    },
    Point {
        name: "a frame past the VM stack's page",
        lay: |r| r.stack.open(&mut r.heap),
        op: |r| r.stack.push_frame(&mut r.heap, 20_000, 0, 1).map(drop),
    },
    Point {
        name: "the top frame grown past its page",
        lay: |r| r.stack.push_frame(&mut r.heap, 16_000, 0, 1).map(drop),
        op: |r| {
            let top = r.stack.current();
            r.stack.grow_top(&mut r.heap, top, 1000)
        },
    },
    Point {
        name: "a frame moved to the heap",
        lay: |r| {
            r.stack.push_frame(&mut r.heap, 4, 2, 1)?;
            if let Some((_, frame)) = r.stack.top_mut() {
                frame.slots_mut().fill(Zval::long(4));
            }
            Ok(())
        },
        op: |r| {
            if let Some(frame) = r.stack.move_to_heap(&mut r.heap)? {
                r.frames.push(frame);
            }
            Ok(())
        },
    },
    Point {
        name: "a frame moved to the heap and restored",
        lay: |r| r.stack.open(&mut r.heap),
        op: |r| {
            r.stack.push_frame(&mut r.heap, 4, 0, 1)?;
            if let Some(frame) = r.stack.move_to_heap(&mut r.heap)? {
                r.stack.restore_frame(&mut r.heap, frame)?;
            }
            Ok(())
        },
    },
    Point {
        name: "C's emalloc under a lend",
        lay: nothing,
        op: |r| {
            r.heap.lend(|| {
                let at = emalloc(100).cast::<u8>();
                // SAFETY: a block of 100 bytes `emalloc` answered, the
                // heap's or, refused, the system's.
                unsafe { at.write_bytes(7, 100) };
                // SAFETY: the block, freed once under the lend it was
                // made in.
                unsafe { efree(at.cast()) };
            })
        },
    },
    Point {
        name: "C's erealloc under a lend",
        lay: nothing,
        op: |r| {
            let kept = r.heap.lend(|| {
                let at = emalloc(100).cast::<u8>();
                // SAFETY: a block of 100 bytes `emalloc` answered.
                unsafe { at.write_bytes(7, 100) };
                // SAFETY: a live block of these entries.
                let moved = unsafe { erealloc(at.cast(), 6000) }.cast::<u8>();
                // SAFETY: the first 100 bytes moved with it.
                let kept = unsafe { std::slice::from_raw_parts(moved, 100) }
                    .iter()
                    .all(|b| *b == 7);
                // SAFETY: the block, freed once under the lend.
                unsafe { efree(moved.cast::<c_void>()) };
                kept
            })?;
            assert!(kept, "the bytes moved with the block");
            Ok(())
        },
    },
];

/// Each entry point of [`POINTS`] refused at its first ask, then its
/// second, and so on until it is served whole: the refusal is the
/// limit's fatal, what the operation touched still reads and writes as
/// it should once the limit has been rendered, the operation is served
/// whole after it, and the request ends with nothing sensed.
#[test]
fn every_ask_of_every_entry_point_refused_in_turn_leaves_what_it_touched_whole() {
    let _alone = alone();
    let mut refused = 0;
    for point in POINTS {
        let mut asks = 0;
        loop {
            let mut r = Request::new();
            (point.lay)(&mut r).unwrap_or_else(|f| panic!("{}: laid down: {f:?}", point.name));
            r.heap.mm.refuse_after(asks);
            let served = (point.op)(&mut r);
            let fired = r.heap.mm.disarm();
            if !fired {
                assert!(served.is_ok(), "{}: unrefused: {served:?}", point.name);
                r.end(point.name);
                break;
            }
            assert!(
                is_the_limit(&served),
                "{}: ask {asks} refused, and the operation answered {served:?}",
                point.name
            );
            r.heap.overflow_rendered();
            r.use_after(point.name);
            (point.op)(&mut r)
                .unwrap_or_else(|f| panic!("{}: served again after ask {asks}: {f:?}", point.name));
            r.end(point.name);
            asks += 1;
            refused += 1;
        }
        assert!(asks > 0, "{}: the operation asked for nothing", point.name);
    }
    assert!(refused >= POINTS.len());
}

/// A heap whose one chunk is full: the limit at the bytes the chunk
/// holds, every free page taken in a `run` of one, and the fatal that
/// met rendered. The next ask that needs a page meets the limit; what
/// the caller allocated before stands, and its bins keep their free
/// slots.
fn brim(heap: &mut Heap) -> Vec<NonNull<u8>> {
    assert!(heap.set_memory_limit(heap.memory_usage(true)));
    let mut pages = Vec::new();
    while let Ok(at) = heap.mm.alloc(PAGE) {
        pages.push(at);
    }
    heap.overflow_rendered();
    pages
}

/// `len` bytes of `heap` written with `byte`.
fn written(heap: &mut Heap, len: usize, byte: u8) -> NonNull<u8> {
    let at = heap
        .mm
        .alloc(len)
        .unwrap_or_else(|f| panic!("{len} bytes: {f:?}"));
    // SAFETY: a fresh block of `len` bytes.
    unsafe { at.as_ptr().write_bytes(byte, len) };
    at
}

/// Whether the first `len` bytes at `at` read `byte`: every byte of a
/// block a bin or a `run` holds, and every 4096th of a huge one, which
/// is one per page it was copied in, so Miri reads three megabytes in a
/// few hundred reads.
fn reads(at: NonNull<u8>, len: usize, byte: u8) -> bool {
    // SAFETY: a live block of at least `len` bytes, written by
    // [`written`].
    let bytes = unsafe { std::slice::from_raw_parts(at.as_ptr(), len) };
    let step = if len > crate::mm::CHUNK_SIZE / 2 {
        PAGE
    } else {
        1
    };
    bytes
        .iter()
        .step_by(step)
        .chain(bytes.last())
        .all(|b| *b == byte)
}

/// The allocator's arms of the limit, each reached through the limit at
/// a full heap: a bin with no free slot, a `run` of pages, a huge block;
/// a reallocation cut to a lower bin, grown past its bin, grown from a
/// bin to a `run`, grown past the pages after its `run`, a huge block
/// grown past the limit by the difference and one cut to a `run`. Each
/// refusal is the limit's fatal with the limit lifted until it is
/// rendered, the block a reallocation was asked of keeps its bytes and
/// is freed after, and the heap's count reads where it stood.
#[test]
fn each_arm_of_the_allocator_the_limit_reaches_refuses_and_leaves_the_block_it_had() {
    let _alone = alone();
    let fresh = [(100, "a bin"), (5000, "a run"), (3_000_000, "a huge block")];
    for (len, arm) in fresh {
        let mut heap = Heap::new();
        let pages = brim(&mut heap);
        let before = heap.memory_usage(false);
        let refused = heap.mm.alloc(len);
        assert!(is_the_limit(&refused), "{arm}: {refused:?}");
        assert!(heap.in_memory_limit_error(), "{arm}: the limit lifted");
        assert_eq!(
            heap.memory_usage(false),
            before,
            "{arm}: the count taken back"
        );
        heap.overflow_rendered();
        for page in pages {
            // SAFETY: a page `brim` took, freed once.
            unsafe { heap.mm.free(page) };
        }
    }
    let moved = [
        (3000, 100, "cut to a lower bin"),
        (100, 2000, "grown past its bin"),
        (100, 5000, "grown from a bin to a run"),
        (5000, 9000, "grown past the pages after its run"),
        (3_000_000, 5000, "a huge block cut to a run"),
    ];
    for (from, to, arm) in moved {
        let mut heap = Heap::new();
        let at = written(&mut heap, from, 0x5a);
        let pages = brim(&mut heap);
        let before = heap.memory_usage(false);
        // SAFETY: a live block of this heap whose `from` bytes are written.
        let refused = unsafe { heap.mm.realloc(at, from, to) };
        assert!(is_the_limit(&refused), "{arm}: {refused:?}");
        assert_eq!(heap.memory_usage(false), before, "{arm}: the count unmoved");
        assert!(reads(at, from, 0x5a), "{arm}: the block keeps its bytes");
        heap.overflow_rendered();
        // SAFETY: the block the refusal left standing, and the pages, each
        // freed once.
        unsafe { heap.mm.free(at) };
        for page in pages {
            // SAFETY: as above.
            unsafe { heap.mm.free(page) };
        }
    }
}

/// `zend_hash_iterator_pos_ex` over a shared table whose copy the limit
/// refuses: the iterator still stands on the table it stood on, counted
/// there, so the table's destruction poisons it and the next move of it
/// reads no freed table.
#[test]
fn an_iterator_whose_copy_the_limit_refuses_stays_counted_where_it_stood() {
    let _alone = alone();
    let mut r = Request::new();
    full_list(&mut r).unwrap_or_else(|f| panic!("{f:?}"));
    shared_list(&mut r).unwrap_or_else(|f| panic!("{f:?}"));
    let (a, b) = (r.arr(0), r.arr(1));
    let idx = r.heap.iterator_add(a, 0);
    r.heap.mm.refuse_after(0);
    let refused = r.heap.iterator_pos_ex(idx, b);
    assert!(r.heap.mm.disarm() && is_the_limit(&refused), "{refused:?}");
    r.heap.overflow_rendered();
    assert!(a.has_iterators(), "the iterator still counts on its table");
    let first = r.held.remove(0);
    Heap::release(&mut r, first);
    let (_, separated) = r
        .heap
        .iterator_pos_ex(idx, b)
        .unwrap_or_else(|f| panic!("{f:?}"));
    r.held[0] = Zval::array(separated);
    r.iterators.push(idx);
    r.end("the iterator");
}
