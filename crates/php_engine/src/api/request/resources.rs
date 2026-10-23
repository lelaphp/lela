//! The resources area of a request: php's resource type table,
//! `zend_register_list_destructors_ex` and what reads it, and the
//! request's resources made, read, closed and listed. The table is
//! [`Resources`], whose field no file but this one reads.

use php_heap::{Heap, Res, ResourceType};

use crate::api::raising::Thrown;
use crate::api::values::Resource;

use super::{Hooks, Memory, Phase, Request};

/// A resource's per-type destructor, php's `list_dtor_ex`: served once,
/// by [`Resource::close`] or by the release that reaches zero, with the
/// resource already closed and its payload still readable. What it
/// throws is left pending where the release stood, as a destructor's
/// `EG(exception)` is, which `zend_test`'s throwing resource is there
/// to test.
pub type ResourceDtor = fn(&mut Request, Resource) -> Result<(), Thrown>;

/// The resource types this request knows, php's `list_destructors`
/// (`zend_register_list_destructors_ex`): a name and a destructor per
/// type, numbered in registration order. The list itself,
/// `EG(regular_list)`, is the heap's, reached through
/// [`Memory::resource`].
#[derive(Default)]
pub struct Resources {
    types: Vec<(Vec<u8>, Option<ResourceDtor>)>,
}

impl Resources {
    /// `zend_register_list_destructors_ex`: the type's name, which
    /// `get_resource_type()` prints, and its destructor, which seam 1
    /// serves when the resource closes. The number is the type's.
    pub fn register_type(&mut self, name: &[u8], dtor: Option<ResourceDtor>) -> ResourceType {
        let kind = ResourceType(u32::try_from(self.types.len()).unwrap_or(u32::MAX));
        self.types.push((name.to_vec(), dtor));
        kind
    }

    /// The type's destructor, if it registered one.
    pub(super) fn dtor(&self, kind: ResourceType) -> Option<ResourceDtor> {
        self.types.get(kind.0 as usize).and_then(|(_, dtor)| *dtor)
    }
}

impl Request {
    /// `EG(regular_list)` in id order, every resource whose entry stands,
    /// closed or not: what `get_resources()` walks.
    pub(crate) fn resource_list(&self) -> Vec<Resource> {
        let live = self.heap.resources();
        let mut found = Vec::with_capacity(live);
        let mut id = 1;
        while found.len() < live && id < i64::MAX {
            if let Some(resource) = self.memory().resource(id) {
                found.push(resource);
            }
            id += 1;
        }
        found
    }

    /// `zend_fetch_list_dtor_id`: the resource type registered under
    /// `name`, as `get_resources()` is given it.
    pub(crate) fn resource_type_named(&self, name: &[u8]) -> Option<ResourceType> {
        (self.resources.types.iter())
            .position(|(registered, _)| registered == name)
            .and_then(|at| u32::try_from(at).ok())
            .map(ResourceType)
    }

    /// `zend_register_resource`, what [`Resource::new`] is: the
    /// thirty-two bytes and the payload out of this request's heap, one
    /// owner, in the list under the next id, and the memory limit
    /// or php's `Resource ID space overflow` as the fatal that ends the
    /// request.
    pub(crate) fn new_resource<T: std::any::Any + Send>(
        &mut self,
        kind: ResourceType,
        payload: T,
    ) -> Result<Resource, Thrown> {
        let allocated = self.heap.resource_with(kind, payload);
        self.fatal_of(allocated).map(Resource::of)
    }

    /// The type's name as registered, `zend_rsrc_list_get_rsrc_type`.
    pub(crate) fn resource_type_name(&self, kind: ResourceType) -> Option<&[u8]> {
        let (name, _) = self.resources.types.get(kind.0 as usize)?;
        Some(name)
    }

    /// `zend_list_close`, what [`Resource::close`] is.
    pub(crate) fn close_resource(&mut self, r: Res) {
        Heap::resource_close(&mut Hooks(self), r);
    }

    /// The native payload of `r` as `T`, borrowed for as long as the
    /// request is.
    pub(crate) fn resource_native<T: std::any::Any + Send>(&self, r: Res) -> Option<&T> {
        self.heap.resource_native(r)
    }

    /// The native payload of `r` as `T`, mutably.
    pub(crate) fn resource_native_mut<T: std::any::Any + Send>(
        &mut self,
        r: Res,
    ) -> Option<&mut T> {
        self.heap.resource_native_mut(r)
    }

    /// `zend_close_rsrc_list`, the first act of
    /// `zend_shutdown_executor_values` (design §5.10): every open
    /// resource closed from the top down, its destructor served, before
    /// the object storage is freed. Each destructor is a `zend_try` of its
    /// own: a fatal a user-space stream's `stream_close` raises is caught
    /// after it, and the walk goes on from the top, where a resource the
    /// destructor opened now stands (`Z/Zend/zend_list.c:215-247`).
    ///
    /// ```php
    /// class W { public $context; static $n = 0;
    ///     function stream_open($p, $m, $o, &$op) { return true; }
    ///     function stream_close() { echo "close ", ++self::$n, "\n"; if (self::$n == 1) undefined(); } }
    /// stream_wrapper_register('ww', 'W');
    /// $a = fopen('ww://a', 'r'); $b = fopen('ww://b', 'r');
    /// undefined_too();
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught Error: Call to undefined function undefined_too() in %s:7
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 7
    /// close 1
    ///
    /// Fatal error: Uncaught Error: Call to undefined function undefined() in %s:4
    /// Stack trace:
    /// #0 [internal function]: W->stream_close()
    /// #1 {main}
    ///   thrown in %s on line 4
    /// close 2
    /// ```
    pub(crate) fn close_resources(&mut self) {
        self.enter_phase(Phase::ResourceShutdown);
        Heap::close_resources(&mut Hooks(self));
    }
}

impl Memory<&Request> {
    /// `zend_hash_index_find(&EG(regular_list), id)`: the resource with
    /// `id`, while its entry stands, closed or not.
    #[must_use]
    pub fn resource(self, id: i64) -> Option<Resource> {
        self.0.heap.resource_at(id).map(Resource::of)
    }
}

impl<'r> Memory<&'r mut Request> {
    /// The request's resource types, for an extension registering one.
    pub fn resources(self) -> &'r mut Resources {
        &mut self.0.resources
    }
}
