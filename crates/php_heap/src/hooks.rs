//! Seam 1, the upward seam (design §4.3): how the heap runs a destructor
//! without knowing the VM. Every heap operation that can free an object
//! or a resource takes `&mut dyn HeapHooks`; the request implements it by
//! serving `__destruct`, dispatching to the object's handler table and
//! turning an exhausted memory limit into a [`Fatal`]. Under Miri the
//! heap's tests pass a recorder that logs what would have been served.
//!
//! **The heap is reached through the hooks.** A destructor is PHP code,
//! and PHP code allocates, so while it is served the heap must be the
//! request's again and not the release's; and the request owns the heap,
//! so it cannot lend the heap to a release and be the hooks beside it,
//! which is the two `&mut` Rust refuses. So an operation that can free
//! takes the hooks alone and asks [`HeapHooks::heap`] for the heap, once
//! per step and again after every call up, exactly as php re-reads
//! `EG(objects_store)` after `dtor_obj`, because the destructor may have
//! grown it. The request implements the hooks over itself, and the heap
//! it hands back is its own field.

use crate::Heap;
use crate::array::Arr;
use crate::object::Obj;
use crate::reference::Ref;
use crate::resource::{Res, ResourceType};
use crate::{Zval, ZvalRef};

/// What ends a request's PHP code short of a normal return (design
/// §5.9): `E_ERROR` and its kin, or the memory limit. Carried as the
/// `Err` of the loop and propagated with `?` through nested serving;
/// Zend's `zend_bailout` without its `longjmp`. `exit` is not one: php 8
/// unwinds it as a throwable, and so does the engine.
///
/// One word, its [`FatalReason`] boxed: a fatal is the cold path of every
/// call that can raise one, so a `Result<T, Fatal>` costs a pointer beside
/// `T`, and a handler's answer to the loop, where it goes on and the fatal
/// that ends it instead, is two registers rather than a slot in memory
/// (design §5.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fatal(Box<FatalReason>);

/// Why a request ends in a [`Fatal`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FatalReason {
    /// A fatal error, with the message the request renders.
    Error(String),
    /// The memory limit, with the bytes that were asked for.
    OutOfMemory(usize),
    /// php 8.5.12's hard limit (`zend_mm_hard_limit_error`,
    /// `Z/Zend/zend_alloc.c:162`): an allocation past it ends the
    /// process where it stands, `_exit(137)`, with no shutdown of any
    /// kind, and nothing but one line on the standard error, which names
    /// the hard limit and the bytes that were asked for.
    HardLimit {
        /// `heap->hard_limit`.
        limit: usize,
        /// The bytes past what the chunks held.
        asked: usize,
    },
    /// A fallible allocation the soft limit refused, php's `NULL` from
    /// `_femalloc` with the delayed OOM left pending: no end of the
    /// request, but the `Err` that carries the refusal out of the
    /// allocation to the fallible scope that asked
    /// ([`crate::Heap::set_fallible`]), which reads it as php's
    /// `RETURN_OOM`.
    Refused,
}

impl Fatal {
    /// A fatal error, with the message the request renders.
    #[cold]
    #[must_use]
    pub fn error(message: impl Into<String>) -> Fatal {
        Fatal(Box::new(FatalReason::Error(message.into())))
    }

    /// The memory limit, with the bytes that were asked for.
    #[cold]
    #[must_use]
    pub fn out_of_memory(bytes: usize) -> Fatal {
        Fatal(Box::new(FatalReason::OutOfMemory(bytes)))
    }

    /// The hard limit met asking for `asked` bytes
    /// ([`FatalReason::HardLimit`]).
    #[cold]
    #[must_use]
    pub fn hard_limit(limit: usize, asked: usize) -> Fatal {
        Fatal(Box::new(FatalReason::HardLimit { limit, asked }))
    }

    /// A fallible allocation refused ([`FatalReason::Refused`]).
    #[cold]
    #[must_use]
    pub(crate) fn refused() -> Fatal {
        Fatal(Box::new(FatalReason::Refused))
    }

    /// Why the request ends.
    #[must_use]
    pub fn reason(&self) -> &FatalReason {
        &self.0
    }

    /// Why the request ends, taken out.
    #[must_use]
    pub fn into_reason(self) -> FatalReason {
        *self.0
    }
}

/// The table php's `get_gc` hands back beside the values it names, the
/// `HashTable *` the collector's walks colour and walk after them
/// (`Z/Zend/zend_gc.c:1112`), which takes the place of the object's own
/// properties.
#[derive(Debug)]
pub enum GcTable {
    /// `zend_std_get_properties(obj)`, or php's `NULL` for an object
    /// with no table: the object's declared slots and dynamic table,
    /// which the heap walks itself after the values the handler named.
    Properties,
    /// A table of the heap's the payload owns, as `zend_closure_get_gc`
    /// hands back the closure's static variables: coloured as php
    /// colours it, so it is walked once in each phase, and never counted
    /// or freed among the garbage, since the object frees it.
    Arr(Arr),
    /// A table that is no value of the heap's, the symbol table a frame
    /// rebuilt, which `zend_fiber_object_gc` and `zend_generator_get_gc`
    /// hand back: its rows in order, walked as a table reached for the
    /// first time, which the one object holding it is in each phase.
    Rows(Vec<Zval>),
}

/// The five things the heap needs done above it (`zend_objects_store_del`,
/// `get_gc`, the `zend_mm` limit callback), and the heap itself, handed
/// back for each step of an operation that can reach them.
pub trait HeapHooks {
    /// The heap the operation is on, asked for again after every call
    /// up; the request's own, and the recorder's under Miri.
    fn heap(&mut self) -> &mut Heap;
    /// php's `gc_collect_cycles` pointer (`Z/Zend/zend_gc.c:254`), which
    /// a full root buffer calls (`:702`): the count the collection freed where
    /// something stands over the heap's own collection, `None` where
    /// [`Heap::gc_collect_cycles`], php's `zend_gc_collect_cycles`, is
    /// the pointer, as it is for a heap whose host overrides nothing.
    fn gc_collect_cycles(&mut self) -> Option<i64> {
        None
    }
    /// The object's `__destruct`, served before its storage is freed.
    /// The object is handed as php hands `dtor_obj` its pointer, still
    /// in the store and held at one owner.
    fn destruct(&mut self, obj: Obj);
    /// The object's native payload, released through its handler table,
    /// php's `free_obj` of an internal class: what the payload names of
    /// the heap, given back before the payload is dropped, with the
    /// members still standing; they are freed after this returns, unless
    /// the handler freed them first through [`Heap::object_std_dtor`]
    /// where php's calls `zend_object_std_dtor`. The object's bucket is
    /// already invalid, as it is when php calls `free_obj`, so it is
    /// handed and not looked up.
    fn free_native(&mut self, obj: Obj);
    /// php's `get_gc`, for the cycle collector's walk: the values the
    /// object's payload owns, handed to `visit` as php's handler fills
    /// `*table`, and the table it hands back beside them.
    fn gc_children(&mut self, obj: Obj, visit: &mut dyn FnMut(Zval)) -> GcTable;
    /// A resource's per-type destructor, `list_dtor_ex`: served once,
    /// by `zend_list_close` or by the release that reaches zero, with
    /// the resource already closed and the type it had handed beside
    /// it, as php hands the destructor a copy of the struct; the
    /// payload is still readable and is dropped after this returns.
    fn free_resource(&mut self, res: Res, kind: ResourceType);
    /// The memory limit was hit asking for `bytes` more.
    fn out_of_memory(&mut self, bytes: usize) -> Fatal;
    /// `gc_grow_root_buffer`'s `zend_error(E_WARNING, "GC buffer
    /// overflow (GC disabled)\n")` (`Z/Zend/zend_gc.c:641`): the root
    /// buffer is at `GC_MAX_BUF_SIZE` and cannot grow, so the collector
    /// has turned itself off. Told once, after the collector is marked
    /// full, so a root an error handler adds meanwhile finds it off
    /// rather than raising the warning again; a heap with nowhere to raise
    /// a warning has a body of its own to do nothing.
    fn gc_overflow(&mut self) {}
    /// `zend_free_source_list` (`Z/Zend/zend_types.h:1093`): the
    /// typed-property sources the engine hung on a reference by
    /// [`Ref::set_sources`], given up as the cell is destroyed and
    /// before the value it holds is released. A heap that hands no
    /// reference to a typed property never sees this, so it has a body
    /// of its own to do nothing.
    fn free_reference(&mut self, r: Ref) {
        let _ = r;
    }
    /// `ZEND_REF_DEL_TYPE_SOURCE` in `zend_object_std_dtor`
    /// (`Z/Zend/zend_objects.c:62`): a slot of an object being freed
    /// held a cell the engine typed, which the slot's property types no
    /// longer once the slot gives the cell up. A heap that types no
    /// reference has nothing to do.
    fn slot_source_gone(&mut self, obj: Obj, slot: u32, r: Ref) {
        let _ = (obj, slot, r);
    }
    /// Whether the object has a destructor to serve, php's
    /// `obj->handlers->dtor_obj != zend_objects_destroy_object ||
    /// obj->ce->destructor`, which the cycle collector asks of its
    /// garbage before it decides to serve destructors first (design
    /// §5.12). A heap that cannot tell says yes, and is served
    /// [`HeapHooks::destruct`] for every object, which does nothing
    /// where there is nothing to serve.
    fn has_destructor(&mut self, obj: Obj) -> bool {
        let _ = obj;
        true
    }
    /// The cycle collector's destructor phase served inside a fiber:
    /// php runs a collection's destructors in a fiber of its own when a
    /// fiber is running, `gc_call_destructors_in_fiber`, so that a
    /// destructor that suspends suspends that fiber and not the one the
    /// collection interrupted. `false` is the phase served in place by
    /// the heap, the entries below `end` walked for their destructors;
    /// a heap with no fibers always answers so.
    fn gc_destructors_in_fiber(&mut self, from: u32, end: u32) -> bool {
        let _ = (from, end);
        false
    }
    /// The temporaries live where every frame of the running execution
    /// stands, php's `ZEND_LIVE_TMPVAR` and `ZEND_LIVE_LOOP` ranges open
    /// at each frame's opline, innermost frame first: what
    /// `zend_gc_remove_root_tmpvars` takes out of the root buffer before
    /// a collection and `zend_gc_check_root_tmpvars` puts back after it,
    /// since a temporary is freed without a root and one the producing
    /// code rooted would otherwise be lost. A heap with no frames has
    /// none.
    fn gc_live_temporaries(&mut self, _visit: &mut dyn FnMut(Zval)) {}
    /// A `WeakMap`'s entries for the cycle collector, each key beside
    /// its entry in place (`zend_weakmap_get_key_entry_gc`), and whether
    /// `map` is one: php tests `get_gc == zend_weakmap_get_gc`, since an
    /// entry is owned by the map and kept alive by its key and the walk
    /// treats it as reached through both. A heap that hands out no weak
    /// map has none.
    fn gc_weakmap_entries(&mut self, map: Obj, _visit: &mut dyn FnMut(Obj, ZvalRef)) -> bool {
        let _ = map;
        false
    }
    /// Every `WeakMap` entry keyed by `key`, in place, beside the map
    /// that holds it (`zend_weakmap_get_object_key_entry_gc`): what the
    /// collector walks from an object carrying
    /// `IS_OBJ_WEAKLY_REFERENCED`. A `WeakReference` names no entry, and
    /// a heap that hands out no weak map has none.
    fn gc_weakly_keyed(&mut self, key: Obj, _visit: &mut dyn FnMut(ZvalRef, Obj)) {
        let _ = key;
    }
    /// `zend_weakrefs_notify` (`Z/Zend/zend_weakrefs.c:212`): the object
    /// is being freed, its destructor served and its members not yet
    /// released, and every `WeakReference` naming it is cleared and
    /// every `WeakMap` entry keyed by it removed, the entry's value
    /// released there. Called only for an object carrying
    /// `IS_OBJ_WEAKLY_REFERENCED`, which a heap that hands out no weak
    /// reference never sets, so it has a body of its own to do nothing.
    fn weakrefs_notify(&mut self, obj: Obj) {
        let _ = obj;
    }
    /// `zend_lazy_object_del_info` from `zend_object_std_dtor`
    /// (`Z/Zend/zend_objects.c:85`): the object is being freed while
    /// lazy, after the weak references were told and before its members
    /// go, and what the engine keeps aside for it, the initializer or
    /// the instance, is given up. Called only for an object whose
    /// [`crate::Laziness`] is not `Eager`, which a heap that makes no
    /// lazy object never sees, so it has a body of its own to do
    /// nothing.
    fn forget_lazy(&mut self, obj: Obj) {
        let _ = obj;
    }
}
