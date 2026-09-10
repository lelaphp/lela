//! Seam 1, the upward seam (design §4.3): how the heap runs a destructor
//! without knowing the VM. Every heap operation that can free an object
//! or a resource takes `&mut dyn HeapHooks`; the request implements it by
//! serving `__destruct`, dispatching to the object's handler table and
//! turning an exhausted memory limit into a [`Fatal`]. Under Miri the
//! heap's tests pass a recorder that logs what would have been served.

use crate::ObjectId;
use crate::Zval;

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
/// `get_gc`, the `zend_mm` limit callback).
pub trait HeapHooks {
    /// The object's `__destruct`, served before its storage is freed.
    fn destruct(&mut self, obj: ObjectId);
    /// The object's native payload, released through its handler table.
    fn free_native(&mut self, obj: ObjectId);
    /// The values the object owns, for the cycle collector's traversal.
    fn gc_children(&mut self, obj: ObjectId, visit: &mut dyn FnMut(Zval));
    /// A resource's per-type destructor.
    fn free_resource(&mut self, res: u32);
    /// The memory limit was hit asking for `bytes` more.
    fn out_of_memory(&mut self, bytes: usize) -> Fatal;
}
