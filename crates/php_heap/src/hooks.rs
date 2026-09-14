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
use crate::Zval;
use crate::object::Obj;
use crate::resource::{Res, ResourceType};

/// What ends a request's PHP code short of a normal return (design
/// §5.9): `E_ERROR` and its kin, or the memory limit. Carried as the
/// `Err` of the loop and propagated with `?` through nested serving;
/// Zend's `zend_bailout` without its `longjmp`. `exit` is not one: php 8
/// unwinds it as a throwable, and so does the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fatal {
    /// A fatal error, with the message the request renders.
    Error(String),
    /// The memory limit, with the bytes that were asked for.
    OutOfMemory(usize),
}

/// The five things the heap needs done above it (`zend_objects_store_del`,
/// `get_gc`, the `zend_mm` limit callback), and the heap itself, handed
/// back for each step of an operation that can reach them.
pub trait HeapHooks {
    /// The heap the operation is on, asked for again after every call
    /// up; the request's own, and the recorder's under Miri.
    fn heap(&mut self) -> &mut Heap;
    /// The object's `__destruct`, served before its storage is freed.
    /// The object is handed as php hands `dtor_obj` its pointer, still
    /// in the store and held at one owner.
    fn destruct(&mut self, obj: Obj);
    /// The object's native payload, released through its handler table:
    /// what the payload names of the heap, given back before the payload
    /// is dropped. The object's bucket is already invalid, as it is when
    /// php calls `free_obj`, so it is handed and not looked up.
    fn free_native(&mut self, obj: Obj);
    /// The values the object owns, for the cycle collector's traversal.
    fn gc_children(&mut self, obj: Obj, visit: &mut dyn FnMut(Zval));
    /// A resource's per-type destructor, `list_dtor_ex`: served once,
    /// by `zend_list_close` or by the release that reaches zero, with
    /// the resource already closed and the type it had handed beside
    /// it, as php hands the destructor a copy of the struct; the
    /// payload is still readable and is dropped after this returns.
    fn free_resource(&mut self, res: Res, kind: ResourceType);
    /// The memory limit was hit asking for `bytes` more.
    fn out_of_memory(&mut self, bytes: usize) -> Fatal;
}
