//! Resources (design §5.5): php's `zend_resource`
//! (`Z/Zend/zend_types.h:586-591`) and the list it stands in,
//! `EG(regular_list)`, copied from `Z/Zend/zend_list.c` at the tag. A
//! resource is thirty-two bytes out of the request's heap, the
//! refcounted header, the id `get_resource_id()` reads, php's `handle`,
//! the type as the engine numbers it, `-1` once closed, and `ptr`, the
//! native payload; `ZVAL_NEW_RES` (`Z/Zend/zend_types.h:1185`) `emalloc`s
//! exactly that, and `memory_get_usage()` moves by it.
//!
//! A [`Res`] is the pointer php's `zend_resource *` is, `Copy`, no
//! `Drop`, its owners counted by [`crate::Heap::addref`] and
//! [`crate::Heap::release`]. The list holds no owner: php's
//! `zend_list_insert` stores the resource with the one owner it hands
//! the caller, and the release that reaches zero is `zend_list_free`,
//! which deletes the entry, whose destructor `list_entry_destructor`
//! serves the per-type destructor through [`HeapHooks::free_resource`]
//! while the resource stands open and gives the bytes back either way.
//! `zend_list_close` is [`crate::Heap::resource_close`]: the destructor
//! served once and the resource left in the list, closed, until its
//! owners let go, which is what `fclose($f); var_dump($f)` shows.
//!
//! **The payload.** php's `ptr` is whatever the extension `emalloc`ed;
//! here it is one typed slot, as an object's is (design §5.4), a second
//! allocation of the request's heap carrying its table before it, so
//! the header stays php's thirty-two bytes. The drop is derived from
//! the type; what the payload names of the heap is given back by the
//! engine's destructor before the drop runs.
//!
//! **The list's own storage.** php's `EG(regular_list)` is a packed
//! `zend_hash` whose bucket array is the request's memory; the list here
//! is on the system allocator and counts nothing, as the object store's
//! table did before it moved onto the heap. What the list holds is
//! php's: the ids from one, never reused within a request
//! (`zend_hash_next_free_element`), and the overflow fatal at
//! `ZEND_LONG_MAX`.

use std::any::Any;
use std::cell::Cell;
use std::ptr::NonNull;

use crate::gc::{self, Refcounted};
use crate::hooks::{Fatal, HeapHooks};
use crate::payload::{Payload, payload_of};
use crate::{Heap, ZvalType};

/// A resource type as the engine numbers it: php's `int type`, the
/// index of its destructor in `list_destructors`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResourceType(pub u32);

/// php's `type` once `zend_list_close` ran: closed.
const CLOSED: i32 = -1;

/// `zend_resource`: thirty-two bytes, php's own order.
#[repr(C)]
pub(crate) struct ResHeader {
    /// `zend_refcounted_h gc`, `GC_RESOURCE`: not collectable, as php
    /// marks it.
    gc: Refcounted,
    /// `handle`: the list's index, `get_resource_id()`.
    id: i64,
    /// `type`: the engine's number, or `-1` once closed.
    kind: Cell<i32>,
    _pad: u32,
    /// `ptr`: the payload allocation, none for a resource of no payload
    /// and none once closed.
    ptr: Cell<Option<NonNull<PayloadBox>>>,
}

/// What sits at `ptr`: the payload's table, then the payload itself at
/// its own alignment.
#[repr(C)]
struct PayloadBox {
    table: &'static Payload,
}

/// One resource: php's `zend_resource *`. Naming one does not keep it
/// alive, exactly as php's pointer does not; the engine's kernels take
/// and give up owners through [`crate::Heap::addref`] and
/// [`crate::Heap::release`].
#[derive(Clone, Copy)]
pub struct Res(NonNull<ResHeader>);

// SAFETY: a `Res` is the pointer php's `zend_resource *` is: a request's
// resource moves with the request that owns it, whole, to the thread
// that serves it (design §5.14), and its payload is `Send` by the bound
// on `Heap::resource_with`. The refcount `Cell` is what keeps it off
// `Sync`.
unsafe impl Send for Res {}

impl Res {
    fn header(&self) -> &ResHeader {
        // SAFETY: a `Res` names a resource `init` laid down, live while
        // its owner holds it, which is php's `zend_resource *` contract.
        unsafe { self.0.as_ref() }
    }

    /// Where the header sits, which is where the allocation starts, for
    /// the heap that frees it.
    pub(crate) const fn address(self) -> NonNull<u8> {
        self.0.cast()
    }

    /// The payload word of a zval naming this resource.
    pub(crate) fn payload(self) -> u64 {
        self.0.as_ptr().expose_provenance() as u64
    }

    /// The resource a zval's payload word names.
    ///
    /// # Safety
    ///
    /// The word came from [`Res::payload`] for a resource still live.
    pub(crate) unsafe fn from_payload(payload: u64) -> Option<Res> {
        let at: *mut ResHeader =
            std::ptr::with_exposed_provenance_mut(usize::try_from(payload).unwrap_or(0));
        NonNull::new(at).map(Res)
    }

    /// `ZVAL_NEW_RES` over a fresh allocation: one owner.
    ///
    /// # Safety
    ///
    /// `at` is writable for `size_of::<ResHeader>()` bytes and holds
    /// nothing else.
    unsafe fn init(
        at: NonNull<u8>,
        id: i64,
        kind: ResourceType,
        ptr: Option<NonNull<PayloadBox>>,
    ) -> Res {
        let header: NonNull<ResHeader> = at.cast();
        // SAFETY: the caller's allocation is the header and nothing else
        // holds it.
        unsafe {
            header.as_ptr().write(ResHeader {
                gc: Refcounted::new(ZvalType::Resource, gc::NOT_COLLECTABLE),
                id,
                kind: Cell::new(i32::try_from(kind.0).unwrap_or(i32::MAX)),
                _pad: 0,
                ptr: Cell::new(ptr),
            });
        }
        Res(header)
    }

    /// `Z_RES_HANDLE`: the list's index, `get_resource_id()`.
    #[must_use]
    pub fn id(&self) -> i64 {
        self.header().id
    }

    /// `Z_RES_TYPE`: the type as the engine numbered it, and `None` once
    /// closed, which is php's `type < 0`.
    #[must_use]
    pub fn resource_type(&self) -> Option<ResourceType> {
        u32::try_from(self.header().kind.get())
            .ok()
            .map(ResourceType)
    }

    /// Whether `zend_list_close` ran: `resource (closed)` in every
    /// type name php prints.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.header().kind.get() < 0
    }

    /// `GC_REFCOUNT`.
    #[must_use]
    pub fn refcount(&self) -> u32 {
        self.header().gc.refcount()
    }

    /// `GC_ADDREF`.
    pub fn addref(&self) {
        self.header().gc.addref();
    }

    /// `GC_DELREF`, and what is left.
    pub(crate) fn delref(self) -> u32 {
        self.header().gc.delref()
    }

    /// Whether two names name one resource, php's pointer equality.
    #[must_use]
    pub fn is_same(&self, other: Res) -> bool {
        self.0 == other.0
    }

    /// The payload's table and where the payload sits after it, when
    /// the resource carries one.
    fn payload_at(self) -> Option<(&'static Payload, NonNull<u8>)> {
        let at = self.header().ptr.get()?;
        // SAFETY: `ptr` was written by `resource_with` from a live
        // allocation, and cleared before that allocation went back.
        let table = unsafe { at.as_ref() }.table;
        Some((table, payload_addr(at, table)))
    }

    /// The payload as `T`, when the resource carries one of that type.
    fn native_ptr<T: Any + Send>(self) -> Option<NonNull<T>> {
        let (table, at) = self.payload_at()?;
        table.is::<T>().then(|| at.cast())
    }
}

impl std::fmt::Debug for Res {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "resource(#{} of type {})",
            self.id(),
            self.header().kind.get()
        )
    }
}

/// The payload's offset inside its allocation: after the table, at the
/// payload's own alignment.
fn payload_offset(table: &Payload) -> usize {
    size_of::<PayloadBox>().next_multiple_of(table.layout.align())
}

/// Where the payload sits in the allocation at `at`.
fn payload_addr(at: NonNull<PayloadBox>, table: &Payload) -> NonNull<u8> {
    // SAFETY: the allocation was sized by `Heap::resource_with` to hold
    // the payload at this offset.
    unsafe { at.cast::<u8>().add(payload_offset(table)) }
}

/// `EG(regular_list)`: the resources of one request by id, the next id
/// never given, and no owner held.
#[derive(Default)]
pub(crate) struct RegularList {
    entries: Vec<Option<Res>>,
    /// `nNextFreeElement`: the id the next insert takes, from one.
    next: i64,
}

impl RegularList {
    /// `zend_hash_index_find(&EG(regular_list), handle)`.
    pub(crate) fn get(&self, id: i64) -> Option<Res> {
        let at = usize::try_from(id.checked_sub(1)?).ok()?;
        self.entries.get(at).copied().flatten()
    }

    /// `zend_list_insert`'s id: `zend_hash_next_free_element`, one for
    /// the first, and php's fatal at `ZEND_LONG_MAX`.
    fn next_id(&self) -> Result<i64, Fatal> {
        let index = self.next;
        if index == 0 {
            return Ok(1);
        }
        if index == i64::MAX {
            return Err(Fatal::Error(String::from("Resource ID space overflow")));
        }
        Ok(index)
    }

    /// `zend_hash_index_add_new(&EG(regular_list), handle, &zv)`.
    fn add(&mut self, id: i64, r: Res) {
        let at = usize::try_from(id - 1).unwrap_or(0);
        if self.entries.len() <= at {
            self.entries.resize(at + 1, None);
        }
        if let Some(cell) = self.entries.get_mut(at) {
            *cell = Some(r);
        }
        self.next = id.saturating_add(1);
    }

    /// `zend_hash_index_del(&EG(regular_list), handle)` without the
    /// entry's destructor, which the caller serves.
    fn del(&mut self, id: i64) {
        if let Some(cell) = usize::try_from(id - 1)
            .ok()
            .and_then(|at| self.entries.get_mut(at))
        {
            *cell = None;
        }
    }

    /// How many entries name a live resource.
    pub(crate) fn live(&self) -> usize {
        self.entries.iter().flatten().count()
    }

    /// `nNumUsed`: one past the last id given.
    fn used(&self) -> usize {
        self.entries.len()
    }
}

impl Drop for RegularList {
    /// `zend_destroy_rsrc_list` after the loop is gone: every
    /// payload still on a resource is dropped, so a payload's own
    /// resources go with the request; the bytes are the heap's and go
    /// with its chunks. Declared before `mm` on the heap, so this runs
    /// first.
    fn drop(&mut self) {
        for r in self.entries.drain(..).flatten() {
            r.drop_payload();
        }
    }
}

impl std::fmt::Debug for RegularList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RegularList({} live, next {})", self.live(), self.next)
    }
}

impl Res {
    /// Drop the payload once, and never again: `ptr` is taken off the
    /// header first, so the sweep and the destructor cannot both drop
    /// it. The allocation's bytes are the caller's to give back.
    fn drop_payload(self) -> Option<NonNull<PayloadBox>> {
        let at = self.header().ptr.take()?;
        // SAFETY: `ptr` was written by `resource_with` from a live
        // allocation, and this is the one take of it.
        let table = unsafe { at.as_ref() }.table;
        // SAFETY: the payload was written by `resource_with` at this
        // address, has not been dropped, since `ptr` still named it, and
        // nothing reads it after this.
        unsafe { (table.drop)(payload_addr(at, table)) };
        Some(at)
    }
}

impl Heap {
    /// `zend_register_resource(NULL, type)` (`Z/Zend/zend_list.c:41`):
    /// a resource of `kind` with no payload, one owner, in the list
    /// under the id [`Res::id`] reads.
    pub fn resource(&mut self, kind: ResourceType) -> Result<Res, Fatal> {
        self.resource_alloc(kind, None)
    }

    /// The same resource carrying `payload` as its `ptr` (design §5.5):
    /// what an extension `emalloc`s and hands `zend_register_resource`,
    /// counted with the resource. `T` is `Send` because the request is;
    /// what it names of the heap is given back through
    /// [`HeapHooks::free_resource`] before it is dropped.
    pub fn resource_with<T: Any + Send>(
        &mut self,
        kind: ResourceType,
        payload: T,
    ) -> Result<Res, Fatal> {
        let table = payload_of::<T>();
        if table.layout.align() > 8 {
            return Err(Fatal::Error(format!(
                "InternalError: a native payload aligned to {} cannot sit on the request heap",
                table.layout.align()
            )));
        }
        let size = payload_offset(table) + table.layout.size();
        let at = self.mm.alloc(size)?.cast::<PayloadBox>();
        // SAFETY: the allocation holds the table at its start and nothing
        // else holds it.
        unsafe { at.as_ptr().write(PayloadBox { table }) };
        // SAFETY: the allocation was sized to hold the payload at this
        // offset, which nothing has written.
        unsafe { payload_addr(at, table).cast::<T>().as_ptr().write(payload) };
        match self.resource_alloc(kind, Some(at)) {
            Ok(r) => Ok(r),
            Err(fatal) => {
                // SAFETY: the payload was written just above and nothing
                // names it.
                unsafe { (table.drop)(payload_addr(at, table)) };
                // SAFETY: the allocation came from this heap and its
                // payload is dropped.
                unsafe { self.mm.free(at.cast()) };
                Err(fatal)
            }
        }
    }

    /// `zend_list_insert` (`Z/Zend/zend_list.c:24`): the id, the
    /// thirty-two bytes and the entry.
    fn resource_alloc(
        &mut self,
        kind: ResourceType,
        ptr: Option<NonNull<PayloadBox>>,
    ) -> Result<Res, Fatal> {
        let id = self.resources.next_id()?;
        let at = self.mm.alloc(size_of::<ResHeader>())?;
        // SAFETY: the allocation is exactly the header's size and nothing
        // else holds it.
        let r = unsafe { Res::init(at, id, kind, ptr) };
        self.resources.add(id, r);
        Ok(r)
    }

    /// The resource under `id`, while its entry stands: what
    /// `zend_fetch_resource` starts from.
    #[must_use]
    pub fn resource_at(&self, id: i64) -> Option<Res> {
        self.resources.get(id)
    }

    /// How many resources the list holds, closed ones included.
    #[must_use]
    pub fn resources(&self) -> usize {
        self.resources.live()
    }

    /// The payload as `T`, `None` for a resource of another type, of
    /// none, or closed. Borrowed for as long as the heap is, which is
    /// what keeps a `Copy` name from handing out two.
    #[must_use]
    pub fn resource_native<T: Any + Send>(&self, r: Res) -> Option<&T> {
        let at = r.native_ptr::<T>()?;
        // SAFETY: the payload was written by `resource_with` as a `T` and
        // is live while the heap is borrowed.
        Some(unsafe { at.as_ref() })
    }

    /// The payload as `T`, mutably; the heap is held for as long as the
    /// borrow is, so no second name reaches it meanwhile.
    pub fn resource_native_mut<T: Any + Send>(&mut self, r: Res) -> Option<&mut T> {
        let mut at = r.native_ptr::<T>()?;
        // SAFETY: as `resource_native`, and the heap is held mutably.
        Some(unsafe { at.as_mut() })
    }

    /// `zend_list_close` (`Z/Zend/zend_list.c:74`): a resource nothing
    /// owns is freed; one still owned and open is closed, its
    /// destructor served once, and stays in the list for its owners.
    pub fn resource_close(hooks: &mut dyn HeapHooks, r: Res) {
        if r.refcount() == 0 {
            Heap::resource_free(hooks, r);
        } else if !r.is_closed() {
            Heap::resource_dtor(hooks, r);
        }
    }

    /// `zend_resource_dtor` (`Z/Zend/zend_list.c:59`): the type set to
    /// closed before the destructor runs, so a destructor that closes
    /// its own resource again finds it closed, and the destructor
    /// handed the type it had, as php hands it a copy of the struct;
    /// then the payload dropped and its bytes given back.
    fn resource_dtor(hooks: &mut dyn HeapHooks, r: Res) {
        let Some(kind) = r.resource_type() else {
            return;
        };
        r.header().kind.set(CLOSED);
        hooks.free_resource(r, kind);
        if let Some(at) = r.drop_payload() {
            // SAFETY: the allocation came from `resource_with` and its
            // payload is dropped, so nothing names it.
            unsafe { hooks.heap().mm.free(at.cast()) };
        }
    }

    /// `zend_list_free` (`Z/Zend/zend_list.c:68`) and the entry's
    /// destructor, `list_entry_destructor` (`Z/Zend/zend_list.c:174`):
    /// the entry taken off the list, the per-type destructor served
    /// while the resource stands open, and the bytes given back.
    pub(crate) fn resource_free(hooks: &mut dyn HeapHooks, r: Res) {
        hooks.heap().resources.del(r.id());
        Heap::resource_dtor(hooks, r);
        // SAFETY: the resource came from `resource_alloc`, its entry is
        // gone and its payload dropped, so nothing names it.
        unsafe { hooks.heap().mm.free(r.address()) };
    }

    /// `zend_close_rsrc_list` (`Z/Zend/zend_list.c:215`), the shutdown
    /// walk: every open resource closed from the top down, so a
    /// resource is closed before the ones it was built on, and the walk
    /// restarted from the new top when a destructor added one.
    pub fn close_resources(hooks: &mut dyn HeapHooks) {
        let mut num = hooks.heap().resources.used();
        let mut i = num;
        while i > 0 {
            i -= 1;
            let id = i64::try_from(i).unwrap_or(i64::MAX).saturating_add(1);
            if let Some(r) = hooks.heap().resources.get(id)
                && !r.is_closed()
            {
                Heap::resource_dtor(hooks, r);
                let used = hooks.heap().resources.used();
                if used != num {
                    num = used;
                    i = used;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Zval;
    use crate::object::Obj;

    use crate::serial::alone;

    /// The heap's tests pass this where a request would serve a
    /// destructor: the heap itself, and what would have been served,
    /// with the type each resource had when its destructor ran.
    struct Recorder {
        heap: Heap,
        freed: Vec<(i64, u32, bool)>,
    }

    impl HeapHooks for Recorder {
        fn heap(&mut self) -> &mut Heap {
            &mut self.heap
        }
        fn destruct(&mut self, _: Obj) {}
        fn free_native(&mut self, _: Obj) {}
        fn gc_children(&mut self, _: Obj, _: &mut dyn FnMut(Zval)) {}
        fn free_resource(&mut self, res: Res, kind: ResourceType) {
            self.freed.push((res.id(), kind.0, res.is_closed()));
        }
        fn out_of_memory(&mut self, bytes: usize) -> Fatal {
            Fatal::OutOfMemory(bytes)
        }
    }

    /// A payload whose drop is observed.
    struct Observed(std::rc::Rc<Cell<u32>>);

    impl Drop for Observed {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    // A test payload is dropped on the test's thread; the bound is the
    // engine's for a request that moves.
    // SAFETY: the test never moves the heap off its thread.
    unsafe impl Send for Observed {}

    /// A resource is php's thirty-two bytes, its id from one and never
    /// reused, a close serves the destructor once with the type
    /// it had and leaves the entry standing, closed, and the last
    /// release gives the bytes back and the entry with them.
    #[test]
    fn a_resource_is_thirty_two_bytes_closed_once_and_freed_at_the_last_release() {
        let _alone = alone();
        assert_eq!(size_of::<ResHeader>(), 32);
        let mut rec = Recorder {
            heap: Heap::new(),
            freed: Vec::new(),
        };
        let start = rec.heap.memory_usage(false);
        let drops = std::rc::Rc::new(Cell::new(0));
        let Ok(first) = rec.heap.resource(ResourceType(3)) else {
            return;
        };
        let Ok(second) = rec
            .heap
            .resource_with(ResourceType(4), Observed(std::rc::Rc::clone(&drops)))
        else {
            return;
        };
        assert_eq!(first.id(), 1);
        assert_eq!(second.id(), 2);
        assert_eq!(rec.heap.memory_usage(false) - start, 32 + 32 + 16);
        assert_eq!(rec.heap.resources(), 2);
        assert!(rec.heap.resource_native::<Observed>(second).is_some());
        assert!(rec.heap.resource_native::<u8>(second).is_none());
        assert!(rec.heap.resource_native::<Observed>(first).is_none());
        let z = Zval::resource(second);
        assert!(z.is_refcounted());
        assert_eq!(z.as_resource().map(|r| r.id()), Some(2));
        rec.heap.addref(z);
        assert_eq!(second.refcount(), 2);
        // Closed: the destructor once, handed type 4 with the resource
        // already closed, the payload dropped, the entry standing.
        Heap::resource_close(&mut rec, second);
        Heap::resource_close(&mut rec, second);
        assert_eq!(rec.freed, vec![(2, 4, true)]);
        assert_eq!(drops.get(), 1);
        assert!(second.is_closed());
        assert_eq!(second.resource_type(), None);
        assert!(rec.heap.resource_at(2).is_some());
        assert_eq!(rec.heap.memory_usage(false) - start, 32 + 32);
        // The last release: no second destructor, the entry gone.
        Heap::release(&mut rec, z);
        Heap::release(&mut rec, z);
        assert_eq!(rec.freed.len(), 1);
        assert!(rec.heap.resource_at(2).is_none());
        assert_eq!(rec.heap.memory_usage(false) - start, 32);
        // An open resource released serves its destructor on the way
        // out, and the next id is never one given before.
        Heap::release(&mut rec, Zval::resource(first));
        assert_eq!(rec.freed, vec![(2, 4, true), (1, 3, true)]);
        assert_eq!(rec.heap.memory_usage(false), start);
        let Ok(third) = rec.heap.resource(ResourceType(3)) else {
            return;
        };
        assert_eq!(third.id(), 3);
        // The shutdown walk closes what is open, from the top down, and
        // the sweep drops a payload nothing closed.
        let Ok(fourth) = rec
            .heap
            .resource_with(ResourceType(5), Observed(std::rc::Rc::clone(&drops)))
        else {
            return;
        };
        Heap::close_resources(&mut rec);
        assert_eq!(rec.freed[2..], [(4, 5, true), (3, 3, true)]);
        assert!(third.is_closed() && fourth.is_closed());
        assert_eq!(drops.get(), 2);
        let Ok(_) = rec
            .heap
            .resource_with(ResourceType(5), Observed(std::rc::Rc::clone(&drops)))
        else {
            return;
        };
        drop(rec);
        assert_eq!(drops.get(), 3, "the sweep dropped the last payload");
    }
}
