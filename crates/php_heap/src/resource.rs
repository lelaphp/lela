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
//! **The list's own storage.** php's `EG(regular_list)` is a `zend_hash`
//! inited at eight (`zend_init_rsrc_list`, `Z/Zend/zend_list.c:204-208`)
//! whose data is the request's memory from the first `zend_list_insert`
//! on, packed, `HT_PACKED_SIZE_EX(8, HT_MIN_MASK)` bytes and doubling
//! where `zend_hash_packed_grow` doubles; the list here is one [`Arr`]
//! keyed by id, so those bytes are the array's own data allocation and
//! `memory_get_usage()` moves by them where php's does. The `HashTable`
//! itself is a field of `zend_executor_globals` and not `emalloc`ed, so
//! the header is [`crate::array::table_header`]'s on the system
//! allocator, charged as php charges it, which is not at all. What the
//! list holds is php's: the ids from one, never reused within a request
//! (`zend_hash_next_free_element`), and the overflow fatal at
//! `ZEND_LONG_MAX`.

use std::any::Any;
use std::cell::Cell;
use std::ptr::NonNull;

use crate::array::{
    Allocate, Arr, Key, System, free_table_header, index_add_new, index_del, table_header,
};
use crate::gc::{self, Counted, Refcounted};
use crate::hooks::{Fatal, HeapHooks};
use crate::mm::Mm;
use crate::payload::{Payload, payload_of};
use crate::{Heap, Zval, ZvalType};

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
    /// Which allocator `ptr` came from, in the word php's struct pads.
    home: Home,
    /// Where it stands with the list and its free, in the same word.
    listing: Cell<Listing>,
    /// The rest of that word, written zero: no byte of the header's
    /// second and third words is padding, so the debug build's freed-use
    /// guard reads them all initialised as it looks for the freed-block
    /// mark there ([`crate::mm::freed::is_freed`], #2414).
    spare: [u8; 2],
    /// `ptr`: the payload allocation, none for a resource of no payload
    /// and none once closed.
    ptr: Cell<Option<NonNull<PayloadBox>>>,
}

// The header at the block's start, where `Zval::counted` reads it for
// every counted kind without asking which.
const _: () = assert!(std::mem::offset_of!(ResHeader, gc) == 0);

/// Where a resource stands with `EG(regular_list)` and with its free,
/// which is what the release that reaches zero reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Listing {
    /// In the list: the free deletes its entry, whose destructor is
    /// php's only `efree` of a resource.
    Listed,
    /// Taken off the list by its owners ([`Heap::resource_unlist`]): its
    /// thirty-two bytes are counted off `memory_get_usage()` as php's
    /// `efree` there counts them and kept, retired, for the payload the
    /// owners still name, and the free gives them back.
    Unlisted,
    /// The free is under way: a destructor that takes an owner of its own
    /// resource and gives it up again reaches the free a second time and
    /// finds nothing to do.
    Destroying,
    /// Destroyed: read only while a pin stood at the free, whose bytes,
    /// retired, wait for the pin.
    Destroyed,
}

/// Where a resource's payload was allocated: the request's heap, which
/// `memory_get_usage()` counts and the request's end takes back whole,
/// or the system allocator, for what php `pemalloc`s persistent (a
/// persistent stream's `php_stream`), which no request counts and whose
/// drop gives it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Home {
    Request,
    Persistent,
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

    /// `zend_refcounted_h gc`: the count and the flags.
    pub(crate) fn gc(self) -> Counted {
        // SAFETY: the block starts with its header, and stands while
        // its owner holds the handle, as `header` reads it.
        unsafe { Counted::new(self.address()) }
    }

    /// Where the header sits, which is where the allocation starts, for
    /// the heap that frees it.
    pub(crate) const fn address(self) -> NonNull<u8> {
        self.0.cast()
    }

    /// The payload word of a zval naming this resource.
    pub(crate) const fn payload(self) -> crate::zval::Word {
        crate::zval::Word::pointer(self.0)
    }

    /// The resource a zval's payload word names.
    ///
    /// # Safety
    ///
    /// The word came from [`Res::payload`] for a resource still live.
    pub(crate) unsafe fn from_payload(payload: crate::zval::Word) -> Option<Res> {
        // SAFETY: the word holds the pointer `payload` stored, by the
        // contract.
        unsafe { payload.as_pointer::<ResHeader>() }.map(Res)
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
        home: Home,
    ) -> Res {
        let header: NonNull<ResHeader> = at.cast();
        // SAFETY: the caller's allocation is the header and nothing else
        // holds it.
        unsafe {
            header.as_ptr().write(ResHeader {
                gc: Refcounted::new(ZvalType::Resource, gc::NOT_COLLECTABLE),
                id,
                kind: Cell::new(i32::try_from(kind.0).unwrap_or(i32::MAX)),
                home,
                listing: Cell::new(Listing::Listed),
                spare: [0; 2],
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
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn addref(&self) {
        self.gc().addref();
    }

    /// Whether two names name one resource, php's pointer equality.
    #[must_use]
    pub fn is_same(&self, other: Res) -> bool {
        self.0 == other.0
    }

    /// Whether its last owner went while a pin stood, so the bytes the
    /// pin keeps are retired and go back with it.
    pub(crate) fn is_destroyed(self) -> bool {
        self.header().listing.get() == Listing::Destroyed
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

/// `zend_init_rsrc_list`'s size (`Z/Zend/zend_list.c:204-208`):
/// `zend_hash_init(&EG(regular_list), 8, NULL, list_entry_destructor, 0)`.
const LIST_SIZE: u32 = 8;

/// `EG(regular_list)`: the resources of one request by id, one [`Arr`]
/// whose value at `handle` is the resource's zval and which holds no
/// owner, the next id never given.
///
/// The header is laid down by the first insert on the system allocator,
/// as php's is a field of `zend_executor_globals` and never the
/// request's memory; the data is the request's, 136 bytes for the
/// packed table of eight `zend_list_insert`'s first
/// `zend_hash_index_add_new` lays down, in php's 160-byte bin, and the
/// doubling of `zend_hash_packed_grow` on the id that is the table's
/// size. What `memory_get_usage()` moves by is the resource's thirty-two
/// bytes and its payload, and the list's own data where php's grows.
///
/// Read off the oracle at php's own resource ids, `php -n` at 8.5.9 over
/// nine `fopen('php://memory', 'r')` into `$a[]`, printing
/// `get_resource_id` and the step `memory_get_usage()` took: the CLI's
/// `STDIN`, `STDOUT` and `STDERR` stand in the list before the first
/// line and the first open makes `FG(default_context)` before its
/// stream, so the first open is id 5, and id 8, the
/// fourth open, costs 456 where its neighbours cost 296, which is the
/// packed table's data going from the 160-byte bin to the 320-byte one;
/// the ninth open, id 13, costs the same 160 more again, that time for
/// `$a`'s own table. The test below reads the same 160 and 320 over
/// this list. It is not a witness fence because the engine has no
/// `fopen` yet to serve it.
///
pub(crate) struct RegularList {
    /// The `HashTable`, laid down by the first insert; `None` is a
    /// request that has made no resource.
    arr: Option<Arr>,
}

impl RegularList {
    /// A list holding nothing: what `zend_init_rsrc_list` leaves behind
    /// at request start, `nNextFreeElement` at zero.
    pub(crate) const fn new() -> RegularList {
        RegularList { arr: None }
    }

    /// `zend_hash_index_find(&EG(regular_list), handle)`.
    pub(crate) fn get(&self, id: i64) -> Option<Res> {
        self.arr?.get(Key::Long(id))?.as_resource()
    }

    /// `zend_list_insert`'s id (`Z/Zend/zend_list.c:32-44`):
    /// `zend_hash_next_free_element`, one for the first, and php's fatal
    /// at `ZEND_LONG_MAX`.
    fn next_id(&self) -> Result<i64, Fatal> {
        let index = self.arr.map_or(0, |arr| arr.next_free());
        if index == 0 {
            return Ok(1);
        }
        if index == i64::MAX {
            return Err(Fatal::error("Resource ID space overflow"));
        }
        Ok(index)
    }

    /// `zend_hash_index_add_new(&EG(regular_list), handle, &zv)`, with
    /// the room the add takes out of `mm`: the packed table on the
    /// first insert, the doubled one on the id past its end.
    fn add(&mut self, mm: &mut Mm, id: i64, r: Res) -> Result<(), Fatal> {
        let arr = if let Some(arr) = self.arr {
            arr
        } else {
            let arr = table_header(LIST_SIZE)?;
            arr.set_next_free(0);
            *self.arr.insert(arr)
        };
        index_add_new(mm, arr, id, Zval::resource(r))
    }

    /// `zend_hash_index_del(&EG(regular_list), handle)` without the
    /// entry's destructor, which the caller serves; whether an entry
    /// stood there, php's `SUCCESS`.
    fn del(&mut self, id: i64) -> bool {
        self.arr.and_then(|arr| index_del(arr, id)).is_some()
    }

    /// How many entries name a live resource, `zend_hash_num_elements`.
    pub(crate) fn live(&self) -> usize {
        self.arr.as_ref().map_or(0, Arr::len)
    }

    /// `nNumUsed`: one past the last position written.
    fn used(&self) -> u32 {
        self.arr.map_or(0, Arr::num_used)
    }

    /// `ZEND_HASH_ELEMENT(ht, i)`: the resource at position `i`, if one
    /// stands there.
    fn at(&self, i: u32) -> Option<Res> {
        self.arr?.entry_at(i)?.1.as_resource()
    }
}

impl RegularList {
    /// `zend_destroy_rsrc_list` (`Z/Zend/zend_list.c:217`) in a debug
    /// build's teardown before the request leak sensor: every resource the
    /// list still holds freed by `list_entry_destructor`, its payload
    /// dropped and given back, then the list's data, then its header.
    /// The resources were closed before, by `zend_close_rsrc_list`, so no
    /// per-type destructor is left to serve.
    #[cfg(debug_assertions)]
    pub(crate) fn destroy(&mut self, mm: &mut Mm) {
        let Some(arr) = self.arr.take() else {
            return;
        };
        for (_, z) in &arr {
            let Some(r) = z.as_resource() else { continue };
            if let Some(payload) = r.drop_payload() {
                // SAFETY: the payload was laid down out of `mm` and is
                // dropped, so nothing names it.
                unsafe { mm.free(payload.cast()) };
            }
            // SAFETY: the list was the resource's last name php frees it
            // through, and its thirty-two bytes came from `mm`.
            unsafe { mm.free(r.address()) };
        }
        crate::array::free_table_data(mm, arr);
        free_table_header(arr);
    }
}

impl Drop for RegularList {
    /// `zend_destroy_rsrc_list` after the loop is gone: every
    /// payload still on a resource is dropped, so a payload's own
    /// resources go with the request; the bytes are the heap's and go
    /// with its chunks, the list's data with them. Declared before `mm`
    /// on the heap, so this runs first, while the data it walks is
    /// still mapped; the header alone is this list's to free.
    fn drop(&mut self) {
        let Some(arr) = self.arr.take() else {
            return;
        };
        for (_, z) in &arr {
            if let Some(r) = z.as_resource() {
                r.drop_payload();
            }
        }
        free_table_header(arr);
    }
}

impl std::fmt::Debug for RegularList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let next = self.arr.map_or(0, |arr| arr.next_free());
        write!(f, "RegularList({} live, next {next})", self.live())
    }
}

impl Res {
    /// Drop the payload once, and never again: `ptr` is taken off the
    /// header first, so the sweep and the destructor cannot both drop
    /// it. A payload of the request's heap leaves its bytes to the
    /// caller to give back; a persistent one's go back here, `pefree`.
    fn drop_payload(self) -> Option<NonNull<PayloadBox>> {
        let at = self.header().ptr.take()?;
        // SAFETY: `ptr` was written by `resource_with` from a live
        // allocation, and this is the one take of it.
        let table = unsafe { at.as_ref() }.table;
        // SAFETY: the payload was written by `resource_with` at this
        // address, has not been dropped, since `ptr` still named it, and
        // nothing reads it after this.
        unsafe { (table.drop)(payload_addr(at, table)) };
        match self.header().home {
            Home::Request => Some(at),
            Home::Persistent => {
                // SAFETY: the allocation came from `System` in
                // `resource_persistent` and its payload is dropped.
                unsafe { System.free(at.cast()) };
                None
            }
        }
    }
}

impl Heap {
    /// `zend_register_resource(NULL, type)` (`Z/Zend/zend_list.c:41`):
    /// a resource of `kind` with no payload, one owner, in the list
    /// under the id [`Res::id`] reads.
    pub fn resource(&mut self, kind: ResourceType) -> Result<Res, Fatal> {
        self.resource_alloc(kind, None, Home::Request)
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
        let size = payload_offset(table) + table.layout.size();
        self.resource_sized(kind, payload, size)
    }

    /// [`Heap::resource_with`] with the payload laid inside an
    /// allocation of `size` bytes, the struct php `emalloc`s for the
    /// resource (`sizeof(php_stream)` for a stream), so the request is
    /// charged php's size whatever the Rust payload's is; a payload that
    /// does not fit it, behind the table's word, is an internal error.
    pub fn resource_sized<T: Any + Send>(
        &mut self,
        kind: ResourceType,
        payload: T,
        size: usize,
    ) -> Result<Res, Fatal> {
        self.resource_in(kind, payload, size, Home::Request)
    }

    /// [`Heap::resource_sized`] for what php `pemalloc`s persistent: the
    /// thirty-two bytes of the resource on the request's heap, in its
    /// list, and the payload's `size` on the system allocator, which no
    /// request counts, given back when the payload is dropped.
    pub fn resource_persistent<T: Any + Send>(
        &mut self,
        kind: ResourceType,
        payload: T,
        size: usize,
    ) -> Result<Res, Fatal> {
        self.resource_in(kind, payload, size, Home::Persistent)
    }

    fn resource_in<T: Any + Send>(
        &mut self,
        kind: ResourceType,
        payload: T,
        size: usize,
        home: Home,
    ) -> Result<Res, Fatal> {
        let table = payload_of::<T>();
        if table.layout.align() > 8 {
            return Err(Fatal::error(format!(
                "InternalError: a native payload aligned to {} cannot sit on the request heap",
                table.layout.align()
            )));
        }
        if payload_offset(table) + table.layout.size() > size {
            return Err(Fatal::error(format!(
                "InternalError: a native payload of {} bytes does not fit the {size} php allocates",
                table.layout.size()
            )));
        }
        let at = match home {
            Home::Request => self.mm.alloc(size)?,
            Home::Persistent => System.alloc(size)?,
        }
        .cast::<PayloadBox>();
        // SAFETY: the allocation holds the table at its start and nothing
        // else holds it.
        unsafe { at.as_ptr().write(PayloadBox { table }) };
        // SAFETY: the allocation was sized to hold the payload at this
        // offset, which nothing has written.
        unsafe { payload_addr(at, table).cast::<T>().as_ptr().write(payload) };
        match self.resource_alloc(kind, Some(at), home) {
            Ok(r) => Ok(r),
            Err(fatal) => {
                // SAFETY: the payload was written just above and nothing
                // names it.
                unsafe { (table.drop)(payload_addr(at, table)) };
                match home {
                    // SAFETY: the allocation came from this heap and its
                    // payload is dropped.
                    Home::Request => unsafe { self.mm.free(at.cast()) },
                    // SAFETY: the allocation came from `System` and its
                    // payload is dropped.
                    Home::Persistent => unsafe { System.free(at.cast()) },
                }
                Err(fatal)
            }
        }
    }

    /// `zend_list_insert` (`Z/Zend/zend_list.c:32`): the id, the
    /// thirty-two bytes and the entry, in php's order, so the list's
    /// data is laid down or doubled after the resource it makes room
    /// for. An entry the list has no room for gives the thirty-two
    /// bytes back; php fatals there and gives back nothing.
    fn resource_alloc(
        &mut self,
        kind: ResourceType,
        ptr: Option<NonNull<PayloadBox>>,
        home: Home,
    ) -> Result<Res, Fatal> {
        let id = self.resources.next_id()?;
        let at = self.mm.alloc(size_of::<ResHeader>())?;
        // SAFETY: the allocation is exactly the header's size and nothing
        // else holds it.
        let r = unsafe { Res::init(at, id, kind, ptr, home) };
        if let Err(fatal) = self.resources.add(&mut self.mm, id, r) {
            // SAFETY: the header was laid down just above and nothing
            // names it.
            unsafe { self.mm.free(at) };
            return Err(fatal);
        }
        Ok(r)
    }

    /// The resource under `id`, while its entry stands: what
    /// `zend_fetch_resource` starts from.
    #[must_use]
    pub fn resource_at(&self, id: i64) -> Option<Res> {
        self.resources.get(id)
    }

    /// `zend_hash_index_del(&EG(regular_list), handle)` with the list's
    /// destructor set aside, as mysqlnd's `mysqlnd_fixup_regular_list`
    /// does to the stream it opens (`Z/ext/mysqlnd/mysqlnd_vio.c:115-126`):
    /// the entry deleted and the resource left standing, its payload with
    /// it, its owners' alone. The list no longer finds or counts it, the
    /// shutdown walk does not close it, and its id is spent; the release
    /// that reaches zero serves its destructor and frees it as it would a
    /// listed one. php `efree`s the thirty-two bytes there and keeps the
    /// `php_stream`, so `memory_get_usage()` falls by them here; the bytes
    /// themselves stay, retired, as the payload's home, and go back at
    /// the last release without being counted off a second time.
    pub fn resource_unlist(&mut self, r: Res) {
        if r.header().listing.get() == Listing::Listed && self.resources.del(r.id()) {
            r.header().listing.set(Listing::Unlisted);
            self.mm.retire(r.address());
        }
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
    ///
    /// The entry is the owner of the bytes, as the hash's destructor is
    /// php's only `efree` of a resource: a destructor that takes an owner
    /// of its own resource and gives it up again, as a user filter's
    /// `$this->stream` does while the stream's free flushes it, reaches
    /// here a second time, finds no entry, and frees nothing, where php's
    /// `zend_hash_index_del` finds none either.
    ///
    /// A stream whose last owner goes while a user filter is on its write
    /// chain: the free flushes the chain, the filter's call puts the
    /// stream in `$this->stream` and takes it out again, and the stream
    /// is freed once, so the resources made after it stand.
    ///
    /// ```php
    /// class F extends php_user_filter {
    ///     function filter($in, $out, &$consumed, $closing): int {
    ///         echo $closing ? "closing\n" : "filter\n";
    ///         return PSFS_ERR_FATAL;
    ///     }
    /// }
    /// stream_filter_register('f', 'F');
    /// for ($i = 0; $i < 3; $i++) {
    ///     $m = fopen('php://memory', 'w+');
    ///     stream_filter_append($m, 'f', STREAM_FILTER_WRITE);
    ///     $m = null;
    ///     $a[] = fopen('php://memory', 'r');
    /// }
    /// var_dump(count($a), get_resource_type($a[0]));
    /// ```
    /// ```expect
    /// closing
    /// closing
    /// closing
    /// int(3)
    /// string(6) "stream"
    /// ```
    pub(crate) fn resource_free(hooks: &mut dyn HeapHooks, r: Res) {
        match r.header().listing.replace(Listing::Destroying) {
            Listing::Listed => {
                hooks.heap().resources.del(r.id());
                Heap::resource_dtor(hooks, r);
            }
            Listing::Unlisted => Heap::resource_dtor(hooks, r),
            // The free under way, reached again from its destructor.
            Listing::Destroying => return,
            // Destroyed under a pin, and let go again: the retired bytes
            // go back once no pin stands.
            Listing::Destroyed => {}
        }
        let pinned = r.gc().pinned();
        r.header().listing.set(Listing::Destroyed);
        // SAFETY: the resource came from `resource_alloc`, its entry is
        // gone and its payload dropped, so only a pin names it; an
        // unlisted one's bytes were retired at the unlist, and
        // `give_back` gives retired bytes back without counting them off
        // again.
        unsafe { hooks.heap().give_back(r.address(), pinned) };
    }

    /// `zend_close_rsrc_list` (`Z/Zend/zend_list.c:217`), the shutdown
    /// walk: every open resource closed from the top position down, so
    /// a resource is closed before the ones it was built on, the
    /// element read again each step since a destructor may move the
    /// data, and the walk restarted from the new top when a destructor
    /// added one.
    pub fn close_resources(hooks: &mut dyn HeapHooks) {
        let mut num = hooks.heap().resources.used();
        let mut i = num;
        while i > 0 {
            i -= 1;
            if let Some(r) = hooks.heap().resources.at(i)
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
        fn gc_children(&mut self, _: Obj, _: &mut dyn FnMut(Zval)) -> crate::GcTable {
            crate::GcTable::Properties
        }
        fn free_resource(&mut self, res: Res, kind: ResourceType) {
            self.freed.push((res.id(), kind.0, res.is_closed()));
        }
        fn out_of_memory(&mut self, bytes: usize) -> Fatal {
            Fatal::out_of_memory(bytes)
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

    /// The bin php's allocator serves the list's first data allocation
    /// from: `HT_PACKED_SIZE_EX(8, HT_MIN_MASK)`, eight zvals and two
    /// hash slots, 136 bytes, in the 160-byte bin. The first resource of
    /// a request lays it down and nothing gives it back before the
    /// request ends.
    const LIST: usize = 160;

    /// A resource is php's thirty-two bytes, its id from one and never
    /// reused, a close serves the destructor once with the type
    /// it had and leaves the entry standing, closed, and the last
    /// release gives the bytes back and the entry with them; the list's
    /// data, laid down by the first, stays.
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
        let first = rec.heap.resource(ResourceType(3)).expect("a resource");
        let second = rec
            .heap
            .resource_with(ResourceType(4), Observed(std::rc::Rc::clone(&drops)))
            .expect("a resource");
        assert_eq!(first.id(), 1);
        assert_eq!(second.id(), 2);
        assert_eq!(rec.heap.memory_usage(false) - start, LIST + 32 + 32 + 16);
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
        assert_eq!(rec.heap.memory_usage(false) - start, LIST + 32 + 32);
        // The last release: no second destructor, the entry gone.
        Heap::release(&mut rec, z);
        Heap::release(&mut rec, z);
        assert_eq!(rec.freed.len(), 1);
        assert!(rec.heap.resource_at(2).is_none());
        assert_eq!(rec.heap.memory_usage(false) - start, LIST + 32);
        // An open resource released serves its destructor on the way
        // out, and the next id is never one given before.
        Heap::release(&mut rec, Zval::resource(first));
        assert_eq!(rec.freed, vec![(2, 4, true), (1, 3, true)]);
        assert_eq!(rec.heap.memory_usage(false), start + LIST);
        let third = rec.heap.resource(ResourceType(3)).expect("a resource");
        assert_eq!(third.id(), 3);
        // The shutdown walk closes what is open, from the top down, and
        // the sweep drops a payload nothing closed.
        let fourth = rec
            .heap
            .resource_with(ResourceType(5), Observed(std::rc::Rc::clone(&drops)))
            .expect("a resource");
        Heap::close_resources(&mut rec);
        assert_eq!(rec.freed[2..], [(4, 5, true), (3, 3, true)]);
        assert!(third.is_closed() && fourth.is_closed());
        assert_eq!(drops.get(), 2);
        let _left_for_the_sweep = rec
            .heap
            .resource_with(ResourceType(5), Observed(std::rc::Rc::clone(&drops)))
            .expect("a resource");
        drop(rec);
        assert_eq!(drops.get(), 3, "the sweep dropped the last payload");
    }

    /// A resource its owners took off the list: the list neither finds
    /// nor counts it, the shutdown walk leaves it open, its id stays
    /// spent, its thirty-two bytes leave `memory_get_usage()` at the
    /// unlist, as `mysqlnd_fixup_regular_list`'s `efree` takes them, and
    /// its last release serves the destructor once and gives back its
    /// payload's bytes and never the header's a second time.
    #[test]
    fn an_unlisted_resource_is_its_owners_alone_and_freed_at_their_last_release() {
        let _alone = alone();
        let mut rec = Recorder {
            heap: Heap::new(),
            freed: Vec::new(),
        };
        let start = rec.heap.memory_usage(false);
        let drops = std::rc::Rc::new(Cell::new(0));
        let kept = rec
            .heap
            .resource_with(ResourceType(4), Observed(std::rc::Rc::clone(&drops)))
            .expect("a resource");
        assert_eq!(rec.heap.memory_usage(false) - start, LIST + 32 + 16);
        rec.heap.resource_unlist(kept);
        rec.heap.resource_unlist(kept);
        assert_eq!(rec.heap.memory_usage(false) - start, LIST + 16);
        assert_eq!(rec.heap.resources(), 0);
        assert!(rec.heap.resource_at(1).is_none());
        Heap::close_resources(&mut rec);
        assert!(!kept.is_closed() && rec.freed.is_empty());
        let next = rec.heap.resource(ResourceType(3)).expect("a resource");
        assert_eq!(next.id(), 2, "the unlisted id stays spent");
        assert_eq!(rec.heap.memory_usage(false) - start, LIST + 32 + 16);
        let z = Zval::resource(kept);
        rec.heap.addref(z);
        // A pin over it let go while it lives keeps its bytes.
        rec.heap.pin_block(z);
        rec.heap.unpin_block(z);
        assert_eq!(
            rec.heap.resource_native::<Observed>(kept).map(|_| ()),
            Some(())
        );
        Heap::release(&mut rec, z);
        assert!(rec.freed.is_empty(), "an owner still holds it");
        Heap::release(&mut rec, z);
        assert_eq!(rec.freed, vec![(1, 4, true)]);
        assert_eq!(drops.get(), 1);
        assert_eq!(rec.heap.memory_usage(false) - start, LIST + 32);
        // Closed first, then let go: the destructor is not served again.
        let closed = rec.heap.resource(ResourceType(5)).expect("a resource");
        rec.heap.resource_unlist(closed);
        Heap::resource_close(&mut rec, closed);
        Heap::release(&mut rec, Zval::resource(closed));
        assert_eq!(rec.freed, vec![(1, 4, true), (3, 5, true)]);
        assert_eq!(rec.heap.memory_usage(false) - start, LIST + 32);
        // Let go under a pin: destroyed at the release, its bytes kept for
        // the pin and given back with it, counted off once.
        let pinned = rec.heap.resource(ResourceType(6)).expect("a resource");
        rec.heap.resource_unlist(pinned);
        let z = Zval::resource(pinned);
        rec.heap.pin_block(z);
        Heap::release(&mut rec, z);
        assert_eq!(rec.freed[2..], [(4, 6, true)]);
        assert!(rec.heap.mm.is_retired(pinned.address()));
        assert_eq!(rec.heap.memory_usage(false) - start, LIST + 32);
        rec.heap.unpin_block(z);
        assert!(!rec.heap.mm.is_retired(pinned.address()));
        assert_eq!(rec.heap.memory_usage(false) - start, LIST + 32);
    }

    /// `EG(regular_list)`'s storage is the request's, sized as
    /// `zend_hash_init(..., 8, ...)` sizes a packed table and doubled
    /// where `zend_hash_packed_grow` doubles it, on the id that is the
    /// table's size; the 56-byte header is a field of php's
    /// `zend_executor_globals` and is charged as php charges it, which
    /// is not at all, so the first resource moves `memory_get_usage()`
    /// by its own thirty-two bytes and the list's 160 and nothing more.
    #[test]
    fn the_list_s_storage_is_php_s_packed_table_and_doubles_at_the_eighth_id() {
        let _alone = alone();
        let mut heap = Heap::new();
        let start = heap.memory_usage(false);
        let mut ids = Vec::new();
        for i in 1..=17 {
            let r = heap.resource(ResourceType(1)).expect("a resource");
            ids.push(r.id());
            let list = match i {
                1..=7 => LIST,
                // `HT_PACKED_SIZE_EX(16, HT_MIN_MASK)`, 264, in the
                // 320-byte bin: the 160 given back, the 320 taken.
                8..=15 => 320,
                // `HT_PACKED_SIZE_EX(32, HT_MIN_MASK)`, 520, in the
                // 640-byte bin.
                _ => 640,
            };
            assert_eq!(
                heap.memory_usage(false) - start,
                list + 32 * i,
                "after resource {i}"
            );
        }
        assert_eq!(ids, (1..=17).collect::<Vec<i64>>());
        assert_eq!(heap.resources(), 17);
        assert_eq!(heap.resource_at(8).map(|r| r.id()), Some(8));
        assert!(heap.resource_at(18).is_none());
        assert_eq!(heap.resources.used(), 18, "position zero is a hole");
    }
}
