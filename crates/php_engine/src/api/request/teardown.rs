//! A debug build's end of a request (design §5.1, §5.10): php's shutdown
//! as a `ZEND_DEBUG` build serves it, which has no fast shutdown
//! (`Z/Zend/zend_execute_API.c:437-447`), and the request leak sensor that
//! stands where its `zend_mm_shutdown` reports what is left
//! (`zend_mm_check_leaks`). Two steps of it: the collection
//! `zend_shutdown_executor_values` runs before the object storage is
//! freed, and the sensor after the request's last release, which frees
//! every block of the request's lifetime by name and then requires the
//! heap to hold nothing.
//!
//! Both are silent after a fatal, as php's are after
//! `CG(unclean_shutdown)`: the frames a `zend_bailout` left were never
//! unwound, and what they held is the heap's to discard. A release build
//! compiles none of this and keeps php's fast teardown, the heap's
//! chunks given back whole.

use super::Request;

impl Request {
    /// What `zend_shutdown_executor_values` releases by name in a debug
    /// build after the static variables, and a release build leaves to
    /// the heap's mass release (`Z/Zend/zend_execute_API.c:286-358`): the
    /// request's constants, then each class's static members, evaluated
    /// constants, enum cases and default properties, any of which may
    /// hold an object, before the final collection and the object
    /// storage. Their destructors were served, so this frees and serves
    /// nothing.
    pub(crate) fn free_executor_values(&mut self) {
        self.free_constants();
        self.free_static_members();
        self.free_class_data();
        self.free_enum_cases();
    }

    /// The collection a debug build of php serves in
    /// `zend_shutdown_executor_values`, after the symbol table, the
    /// static variables and the handlers are released and before the
    /// object storage is freed (`Z/Zend/zend_execute_API.c:420-424`), so
    /// a cycle the program left is collected rather than reported. Not
    /// after a fatal. php calls it through `gc_collect_cycles`, the
    /// pointer an extension may stand around; this is the heap's own
    /// collection beneath every link, so what an extension observes of a
    /// request is a release build's, the oracle's, whatever the build.
    pub(crate) fn shutdown_collect(&mut self) {
        if !self.unclean_shutdown() {
            let _ = self.gc_collect_own();
        }
    }

    /// The request leak sensor (design §5.1): what `shutdown_executor` frees
    /// by name in a debug build, the VM stack's pages
    /// (`zend_vm_stack_destroy`), `EG(symtable_cache)` and
    /// `EG(included_files)`; the extensions' request states dropped; then
    /// the heap's own tables and its walk ([`php_heap::Heap::sense_leaks`]),
    /// which records a block still held as the sensors record a defect,
    /// for [`Request::end_in_sensed`] to raise as the `InternalError` it
    /// ends the request in. Silent after a fatal, and where
    /// `report_memleaks` was off as the request's shutdown began, as
    /// php's `php_request_shutdown` reads it (`Z/main/main.c:1962`).
    pub(crate) fn sense_leaks(&mut self, report_memleaks: bool) {
        if self.unclean_shutdown() || !report_memleaks {
            return;
        }
        let mut stack = std::mem::take(&mut self.vm.stack);
        let dropped = stack.close(&mut self.heap);
        self.vm.stack = stack;
        self.drop_frames(dropped);
        self.free_rooms();
        self.free_hash_tables();
        self.drop_request_states();
        let shared = &self.shared;
        self.heap.sense_leaks(&|origin| {
            shared
                .extensions
                .get(usize::from(origin.number()))
                .map_or_else(
                    || format!("the extension numbered {}", origin.number()),
                    |extension| extension.name.clone(),
                )
        });
    }
}
