//! The engine's half of a frame (design §5.6): what `php_heap`'s VM
//! stack does not hold, because it belongs with the engine's `Func` and
//! not with the memory model — the op array being served, the opline the
//! loop is at, and where the caller wants the value the frame returns.
//! Beside them, the calls `INIT_FCALL` builds and `DO_ICALL` serves,
//! which are php's frames pushed on the VM stack before their arguments
//! are known.
//!
//! The slots are the heap's: a CV or a temporary is a zval of
//! `php_heap::Frame`, read and written through the request's `slot`,
//! `take_slot` and `set_slot` as a [`Value`] naming what the zval names
//! with no owner taken (design §5.1), and the engine takes and gives up
//! owners here where php's handlers do, with `ZVAL_COPY`,
//! `ZVAL_COPY_VALUE` and `FREE_OP` as [`take_operand`], [`operand`] and
//! [`free_operand`].

use std::cell::Cell;
use std::sync::Arc;

use php_heap::{FrameRef, MapPtr, MapTable, Zval};
use php_unit::decl::HookKind;
use php_unit::{CacheSlot, ConstIdx, Level, OpArray, OperandKind, Opline, Unit};

use crate::api::call::Function;
use crate::api::raising::{Thrown, warning};
use crate::api::request::Request;
use crate::api::values::{ClassId, PhpObject, PhpString, TypeTag, Value};

/// Which op array of a unit a frame serves: the main one, the body of
/// one of the unit's functions, the body of a function no name bound
/// while the file compiled and a `DECLARE_FUNCTION` bound where it
/// stands ([`Unit::dynamic`]), the body of one of its classes'
/// methods, or of one of their properties' hooks ([`Unit::hooks`]),
/// each by index, as `EX(func)` names one `zend_op_array`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Body {
    Main,
    Function(u32),
    Dynamic(u32),
    Method {
        class: u32,
        method: u32,
    },
    Hook {
        class: u32,
        property: u32,
        kind: HookKind,
    },
}

/// Which construct an `INCLUDE_OR_EVAL` is, php's `ZEND_INCLUDE`,
/// `ZEND_INCLUDE_ONCE`, `ZEND_REQUIRE`, `ZEND_REQUIRE_ONCE` and
/// `ZEND_EVAL` (`Z/Zend/zend_compile.h:1075-1079`), one bit each in the
/// opline's `extended_value`: what `zend_include_or_eval` switches on,
/// what `php_verror` names a diagnostic raised at the opline by, and
/// what `zend_fetch_debug_backtrace` names the frame it pushed by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IncludeKind {
    Eval,
    Include,
    IncludeOnce,
    Require,
    RequireOnce,
}

impl IncludeKind {
    /// The construct an `extended_value` names; none for a value the
    /// compiler never writes.
    pub(crate) fn of(extended_value: u32) -> Option<IncludeKind> {
        match extended_value {
            1 => Some(IncludeKind::Eval),
            2 => Some(IncludeKind::Include),
            4 => Some(IncludeKind::IncludeOnce),
            8 => Some(IncludeKind::Require),
            16 => Some(IncludeKind::RequireOnce),
            _ => None,
        }
    }

    /// The keyword, as php's diagnostics and traces write it.
    pub(crate) fn name(self) -> &'static str {
        match self {
            IncludeKind::Eval => "eval",
            IncludeKind::Include => "include",
            IncludeKind::IncludeOnce => "include_once",
            IncludeKind::Require => "require",
            IncludeKind::RequireOnce => "require_once",
        }
    }

    /// Whether the construct is `include_once` or `require_once`, the
    /// two that read the included-files table before they open a file.
    pub(crate) fn once(self) -> bool {
        matches!(self, IncludeKind::IncludeOnce | IncludeKind::RequireOnce)
    }

    /// Whether a file that cannot be opened is php's `Error`, the two
    /// `require` forms, or the warning and `false`, the two `include`
    /// forms.
    pub(crate) fn required(self) -> bool {
        matches!(self, IncludeKind::Require | IncludeKind::RequireOnce)
    }
}

/// One symbol table of the request, by its place among them: the first
/// is `EG(symbol_table)`, the main op array's and every code frame's it
/// includes; each after it is one `zend_rebuild_symbol_table` built for
/// a function frame that included a file, freed when that frame pops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TableId(pub(crate) u32);

impl TableId {
    /// `EG(symbol_table)`.
    pub(crate) const GLOBAL: TableId = TableId(0);
}

/// The unit a frame serves and which of its op arrays, held by a share
/// of the unit so the oplines can be read while the request is borrowed.
pub(crate) struct Serving {
    pub(crate) unit: Arc<Unit>,
    pub(crate) body: Body,
}

impl Serving {
    /// The op array itself; none for a body the unit has not, which is
    /// this engine's error and never a program's.
    pub(crate) fn code(&self) -> Option<&OpArray> {
        body_of(&self.unit, self.body)
    }
}

/// `EX(prev_execute_data)` as far as a diagnostic reads it: the frame
/// that was php's `EG(current_execute_data)` when this one was pushed,
/// where that is user code, which php's `zend_missing_arg_error` and
/// `zend_verify_arg_error` test with `ZEND_USER_CODE(ptr->func->common.type)`
/// and `ZEND_ARG_USES_STRICT_TYPES()` reads the regime of; and whether
/// an opline of it pushed the call or the call was served from outside
/// the loop, which a backtrace's fake include row reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Caller {
    /// An opline of that frame pushed the call: a `DO_*CALL`, an include.
    Opline(FrameRef),
    /// Served from outside the loop, `zend_call_function`, while the
    /// loop stood in that frame with no native above it: an error
    /// handler, an autoloader or a magic method a handler reached.
    Loop(FrameRef),
    /// Served from outside the loop with no user code current: by a
    /// native, by `serve` itself, or a generator resumed.
    Outside,
}

impl Caller {
    /// The user frame php's `prev_execute_data` is, if it is one.
    pub(crate) fn frame(self) -> Option<FrameRef> {
        match self {
            Caller::Opline(frame) | Caller::Loop(frame) => Some(frame),
            Caller::Outside => None,
        }
    }
}

/// `EX(This)` as an object, and whether the call holds an owner of it,
/// which php says with `ZEND_CALL_RELEASE_THIS`. php sets that bit, and
/// takes the owner, where the call's receiver came from a value the
/// caller may give up while the call runs (`INIT_METHOD_CALL` over a
/// variable or a temporary, `new`, a dynamic or user call over an array
/// callable, a generator's frame) and leaves it clear where the caller
/// keeps the object: `zend_call_function`, which every call a native
/// makes goes through, and a method called on the calling frame's own
/// `$this`. So the destructor of an object whose last outside owner
/// lets it go while one of its methods runs from a native is served
/// there, inside the method.
///
/// A lent receiver is a name with no owner, so it pins the object
/// ([`Receiver::lent`], [`php_heap::Obj::pin`]): php frees the
/// object under the running frame when its last owner lets it go, and
/// here its destructor and its members' release are served at that
/// moment while its bytes and its id stand until the last pin is
/// let go ([`Request::give_up_receiver`]); and a frame that leaves the
/// running chain, a generator's or a suspended fiber's, owns what it was
/// lent.
#[derive(Clone, Copy)]
pub(crate) enum Receiver {
    /// `ZEND_CALL_RELEASE_THIS`: one owner the call holds, which the
    /// frame it becomes gives up when it is popped and the collector
    /// counts as one of the object's.
    Owned(PhpObject),
    /// No `ZEND_CALL_RELEASE_THIS`: the object whoever made the call
    /// keeps, named and not owned.
    Lent(PhpObject),
}

impl Receiver {
    /// The receiver of a call php makes without `ZEND_CALL_RELEASE_THIS`
    /// (`zend_call_function`, `$this->m()`, a forwarding `parent::m()`):
    /// lent, the object named with no owner and pinned
    /// ([`php_heap::Obj::pin`]) until the call or the frame it becomes
    /// gives the name up ([`Request::give_up_receiver`]). An autoloader
    /// that unregisters itself gives up the object's last owner outside
    /// the call, and php frees the object there, inside the method: its
    /// destructor is served and its members are released at that
    /// moment, while the method goes on over the object it still names,
    /// whose bytes stand until the pin is let go:
    ///
    /// ```php
    /// class A { function __invoke($c) { spl_autoload_unregister($this); echo "unregistered\n"; $this->after(); }
    ///     function after() { echo "after\n"; }
    ///     function __destruct() { echo "A gone\n"; } }
    /// spl_autoload_register(new A);
    /// var_dump(class_exists('X'));
    /// ```
    /// ```expect
    /// A gone
    /// unregistered
    /// after
    /// bool(false)
    /// ```
    pub(crate) fn lent(object: PhpObject) -> Receiver {
        object.heap().pin();
        Receiver::Lent(object)
    }

    /// The object, owned or not.
    pub(crate) fn object(self) -> PhpObject {
        match self {
            Receiver::Owned(object) | Receiver::Lent(object) => object,
        }
    }

    /// The object where the call owns it, which is what a pop gives up
    /// and what `zend_unfinished_execution_gc` walks.
    pub(crate) fn owned(self) -> Option<PhpObject> {
        match self {
            Receiver::Owned(object) => Some(object),
            Receiver::Lent(_) => None,
        }
    }
}

impl CallState {
    /// `Z_OBJ(call->This)`: the receiver, a view.
    pub(crate) fn this(&self) -> Option<PhpObject> {
        self.receiver.map(Receiver::object)
    }
}

impl FrameState {
    /// `Z_OBJ(EX(This))`: the receiver, a view.
    pub(crate) fn this(&self) -> Option<PhpObject> {
        self.receiver.map(Receiver::object)
    }
}

/// What php's `INCLUDE_OR_EVAL` keeps on the request's heap for the op
/// array a code frame serves, freed with the frame however it ends
/// (`zend_leave_helper`'s `ZEND_CALL_NESTED_CODE` arm, `destroy_op_array`
/// and `efree_size`): the op array itself, which `zend_accel_load_script`
/// copies out of the cache and `zend_compile` makes, and the runtime
/// cache `i_init_code_execute_data` lays down for it, since a main op
/// array's is `ZEND_ACC_HEAP_RT_CACHE`. The engine's op array and cache
/// are the unit's and the map table's; these are php's bytes.
pub(crate) struct HeapOpArray {
    pub(crate) copy: crate::api::values::Emalloc,
    pub(crate) run_time_cache: Option<crate::api::values::Emalloc>,
}

/// `sizeof(zend_op_array)` as `emalloc` bins it, the 256-byte bin.
pub(crate) const OP_ARRAY_SIZE: usize = 256;

/// `EX(extra_named_params)`: the extra named arguments, name and value
/// in the order given, beside the table php keeps them in on the
/// request's heap, `zend_new_array(0)`'s 56 bytes and the bucket and
/// hash data `zend_hash_real_init_mixed` lays at eight rows and
/// `zend_hash_do_resize` doubles, so a frame that carries them, and a
/// throw it meets, peaks at php's sizes. The rows read as the `Vec`
/// they are; [`ExtraNamed::lay`] grows the table to hold them.
#[derive(Default)]
pub(crate) struct ExtraNamed {
    rows: Vec<(PhpString, Value)>,
    /// The `HashTable` and its data at `nTableSize` rows.
    table: Option<(php_heap::Block, php_heap::Block, usize)>,
}

impl ExtraNamed {
    /// These rows with a table laid for them, `zend_handle_named_arg`
    /// over each in turn.
    pub(crate) fn laid(
        req: &mut Request,
        rows: Vec<(PhpString, Value)>,
    ) -> Result<ExtraNamed, Thrown> {
        let mut named = ExtraNamed { rows, table: None };
        named.lay(req)?;
        Ok(named)
    }

    /// The table grown to hold the rows: `zend_new_array(0)` and
    /// `zend_hash_real_init_mixed`'s `HT_SIZE_EX(8, -16)` for the first,
    /// and the next doubling allocated before the old one is freed, as
    /// `zend_hash_do_resize` copies, for a row past `nTableSize`. A
    /// `Bucket` is 32 bytes and each row two hash slots of 4.
    pub(crate) fn lay(&mut self, req: &mut Request) -> Result<(), Thrown> {
        if self.rows.is_empty() {
            return Ok(());
        }
        let (head, mut data, mut size) = if let Some(table) = self.table.take() {
            table
        } else {
            let head = req.heap_mut().block(56);
            let head = req.fatal_of(head)?;
            let data = req.heap_mut().block(8 * 40);
            (head, req.fatal_of(data)?, 8)
        };
        while self.rows.len() > size {
            size <<= 1;
            let grown = req.heap_mut().block(size * 40);
            let grown = req.fatal_of(grown)?;
            req.heap_mut()
                .free_block(std::mem::replace(&mut data, grown));
        }
        self.table = Some((head, data, size));
        Ok(())
    }

    /// The table alone taken off, the rows left: what `RECV_VARIADIC`
    /// hands an untyped variadic with no positional extras as its
    /// array, `ZVAL_ARR(params, EX(extra_named_params))`, which the
    /// engine's collected array stands for.
    pub(crate) fn take_table(&mut self) -> ExtraNamed {
        ExtraNamed {
            rows: Vec::new(),
            table: self.table.take(),
        }
    }

    /// `zend_array_destroy` of the table: its data, then the 56 bytes;
    /// the rows handed back for their owners to be given up.
    pub(crate) fn free(self, req: &mut Request) -> Vec<(PhpString, Value)> {
        if let Some((head, data, _)) = self.table {
            req.heap_mut().free_block(data);
            req.heap_mut().free_block(head);
        }
        self.rows
    }
}

impl std::ops::Deref for ExtraNamed {
    type Target = Vec<(PhpString, Value)>;
    fn deref(&self) -> &Self::Target {
        &self.rows
    }
}

impl std::ops::DerefMut for ExtraNamed {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.rows
    }
}

/// One frame's engine state.
pub(crate) struct FrameState {
    /// The unit being served, shared with every other request serving it
    /// and never copied (design §5.14).
    pub(crate) unit: Arc<Unit>,
    /// Which of its op arrays: the main one for the frame `serve` pushes,
    /// a function's or a method's for one a `DO_UCALL` or a `DO_FCALL*`
    /// pushed (design §5.6).
    pub(crate) body: Body,
    /// The linked function the frame serves, `EX(func)`; none for the
    /// main op array, which is no function.
    pub(crate) function: Option<Function>,
    /// Where in it the loop stands.
    pub(crate) opline: u32,
    /// The caller's slot for what this frame returns, when the call that
    /// pushed it wants the value.
    pub(crate) returns_to: Option<(FrameRef, u32)>,
    /// `EX(prev_execute_data)` as far as a diagnostic reads it
    /// ([`Caller`]).
    pub(crate) caller: Caller,
    /// `EX(This)` as an object: the receiver of a method call, with
    /// whether the frame owns it until it is popped
    /// (`ZEND_CALL_RELEASE_THIS`); none for the main op array, a function
    /// or a method declared `static`.
    pub(crate) receiver: Option<Receiver>,
    /// The called scope, `static`; none for the main op array or a
    /// function.
    pub(crate) called_scope: Option<ClassId>,
    /// `EX(func)->op_array.scope` where the frame serves no function of
    /// its own: the including frame's scope, which `INCLUDE_OR_EVAL`
    /// writes onto the op array it pushes (`new_op_array->scope =
    /// EX(func)->op_array.scope`), so `self::` and a private property
    /// inside an included file read as the includer's. A function's or
    /// a method's scope is its declaration's and this is `None`.
    pub(crate) scope: Option<ClassId>,
    /// `EX(symbol_table)` under `ZEND_CALL_HAS_SYMBOL_TABLE`: the table
    /// the frame's CVs are attached to, which the main op array's frame
    /// and every code frame have and a function frame gets when it
    /// includes a file (`zend_rebuild_symbol_table`); `None` for a
    /// function frame nothing has asked one of.
    pub(crate) symbol_table: Option<TableId>,
    /// The `Closure` object whose call pushed the frame, one owner the
    /// frame holds until it is popped (`ZEND_CALL_CLOSURE`,
    /// `zend_vm_stack_free_call_frame`'s `OBJ_RELEASE`): what php
    /// reaches through `EX(func)->op_array.static_variables_ptr`, since
    /// the op array a closure serves is the closure's own copy. The
    /// `BIND_STATIC` prologue of the body reads its bound variables off
    /// this and nothing else does; none for every other frame.
    pub(crate) closure: Option<PhpObject>,
    /// `ZEND_CALL_NUM_ARGS`: how many arguments the call that pushed the
    /// frame placed, which the `RECV*` prologue reads.
    pub(crate) num_args: u32,
    /// The arguments past the declared parameters, which php moves past
    /// the temporaries (`zend_copy_extra_args`) for `func_get_args` to
    /// read; owned by the frame and given up with it.
    pub(crate) extra_args: Vec<Value>,
    /// `ZEND_CALL_HAS_EXTRA_NAMED_PARAMS` and `EX(extra_named_params)`:
    /// the named arguments the function declares no parameter for,
    /// which `zend_handle_named_arg` collects for a variadic callee, in
    /// the order they were given; `RECV_VARIADIC` adds them to its
    /// array by name after the extra arguments, and a backtrace prints
    /// them `name: value`. Each name and value one owner of the frame's,
    /// given up with it (`zend_free_extra_named_params`).
    pub(crate) extra_named: ExtraNamed,
    /// How many calls stood on the call stack when the frame was pushed,
    /// which is where the frame sits among them: php's one
    /// `prev_execute_data` chain is two stacks here, and this is what
    /// interleaves them again for a backtrace. A `DO_UCALL` takes its
    /// call off the stack before it pushes the frame, so a frame an
    /// opline entered counts only the calls standing around its caller;
    /// a frame a native re-entered the loop on counts the native that
    /// entered it too, and so stands above it.
    pub(crate) calls_below: u32,
    /// The frame is being popped: `zend_leave_helper` has written
    /// `EG(current_execute_data) = EX(prev_execute_data)` ahead of
    /// `i_free_compiled_variables`, so a destructor what it gives up
    /// serves is called from the caller's activation, and this one is
    /// on no chain a backtrace reads.
    pub(crate) leaving: bool,
    /// The `Generator` whose frame this is, php's `EX(return_value)`
    /// under `ZEND_CALL_GENERATOR` (`zend_get_running_generator`): set
    /// by `GENERATOR_CREATE` when it moves the frame into the object,
    /// read by `YIELD`, `YIELD_FROM` and `GENERATOR_RETURN`. A view with
    /// no owner, since the object owns the frame and not the other way
    /// round; none for every other frame.
    pub(crate) generator: Option<PhpObject>,
    /// php's fast-call temporaries, one per `finally` block of the op
    /// array a `FAST_CALL` or an unwind has entered. php keeps two
    /// fields in the one zval at `EX_VAR(opline->result.var)`,
    /// `Z_OPLINE_NUM_P` and `Z_OBJ_P`, which no zval of this engine
    /// holds together, so the cell stands here and the temporary itself
    /// is left `UNDEF`. Nothing but `FAST_CALL`, `FAST_RET`,
    /// `DISCARD_EXCEPTION` and the unwinder reads it.
    fast_calls: Vec<FastCall>,
    /// The op array's bytes an `INCLUDE_OR_EVAL` frame holds on the
    /// request's heap ([`HeapOpArray`]); none for every other frame.
    pub(crate) op_array: Option<HeapOpArray>,
    /// `EX(run_time_cache)`: where the op array's runtime cache stands in
    /// the request's map table (design §5.6), read off the op array as
    /// `RUN_TIME_CACHE(op_array)` reads it when the frame is activated.
    /// The frame holds the pointer and never the cache: the cache is the
    /// request's, laid down on the op array's first activation
    /// ([`init_runtime_cache`]) and kept until request end, so a
    /// function called twice reads on its second call what its first
    /// primed.
    runtime_cache: MapPtr<RuntimeCache>,
    /// The frame's identity, given the first time an extension keeps it
    /// ([`crate::api::request::FrameId`]) and never given again in the
    /// request; none until then. It moves with the state, so a
    /// generator's frame keeps it across its resumptions as php's
    /// `execute_data` is one, and a frame pushed later where this one
    /// stood has none or another.
    pub(crate) identity: Cell<Option<crate::api::request::Identity>>,
}

/// One op array's runtime cache for one request, what
/// `ZEND_MAP_PTR(op_array->run_time_cache)` reaches: `NULL`, which is
/// what a fresh map table holds, until the op array's first activation
/// lays it down, and from then one slot per `sizeof(void *)` of
/// `cache_size`, every one [`Cached::Unprimed`] when laid down
/// (`memset(run_time_cache, 0, cache_size)`) and kept until request
/// end. The engine's slot is an enum where php's is a word, so the
/// storage is the engine's and not the heap's; the table it stands in
/// is the heap's. Beside the slots stands the op array's observer slot
/// (design §4.3 seam 9), php's `ZEND_OBSERVER_DATA` at the
/// `zend_observer_fcall_op_array_extension` offset of the same cache,
/// unresolved when the cache is laid down.
#[derive(Debug, Default)]
pub(crate) struct RuntimeCache {
    slots: Option<Box<[Cached]>>,
    observer: crate::observer::Slot,
}

impl RuntimeCache {
    /// `zend_arena_alloc(&CG(arena), cache_size)` and the `memset` after
    /// it: `cache_size / sizeof(void *)` slots, all `Unprimed`, and the
    /// observer slot unresolved, over whatever stood here.
    fn lay_down(&mut self, cache_size: u32) {
        let slots = usize::try_from(cache_size / SLOT_BYTES).unwrap_or(0);
        self.slots = Some(vec![Cached::Unprimed; slots].into_boxed_slice());
        self.observer = crate::observer::Slot::Unresolved;
    }

    /// `CACHED_PTR(num)`: what the slot at `slot`, a byte offset as the
    /// opline carries it, holds. A slot past `cache_size`, or a cache not
    /// laid down, reads `Unprimed`, so a kernel over it looks the table
    /// up every pass, which is the reading a program that was never
    /// given the slot would have had; php's would be a read past the
    /// allocation.
    fn get(&self, slot: CacheSlot) -> Cached {
        self.slots
            .as_ref()
            .and_then(|slots| slots.get((slot.0 / SLOT_BYTES) as usize))
            .copied()
            .unwrap_or_default()
    }

    /// `CACHE_PTR(num, ptr)`: `value` into the slot at `slot`, where
    /// every pass after reads it. A slot past `cache_size` takes nothing.
    fn set(&mut self, slot: CacheSlot, value: Cached) {
        if let Some(cell) = self
            .slots
            .as_mut()
            .and_then(|slots| slots.get_mut((slot.0 / SLOT_BYTES) as usize))
        {
            *cell = value;
        }
    }
}

/// `ZEND_MAP_PTR(op_array->run_time_cache)` typed: where the op array's
/// runtime cache stands in a request's map table, the offset the engine
/// assigned the op array at publication ([`OpArray::runtime_cache`]).
pub(crate) fn runtime_cache_of(code: &OpArray) -> MapPtr<RuntimeCache> {
    MapPtr::new(code.runtime_cache)
}

/// `ZEND_OBSERVER_DATA(function)` reached through the function rather
/// than a frame, as `zend_observer_add_begin_handler` and its kin reach
/// it: the observer slot of `code`'s runtime cache in the request's map
/// table, none where the cache was never laid down, php's
/// `RUN_TIME_CACHE` still `NULL`.
pub(crate) fn observer_slot_of<'map>(
    map: &'map mut MapTable,
    code: &OpArray,
) -> Option<&'map mut crate::observer::Slot> {
    let ptr = runtime_cache_of(code);
    map.get(ptr)?.slots.as_ref()?;
    Some(&mut map.map(ptr).observer)
}

/// The runtime cache laid down for an activation of `body`, before its
/// frame runs. For a function's or a method's body it is
/// `zend_init_func_run_time_cache` (`Z/Zend/zend_execute.c:4497`): laid
/// down on the first activation in the request and found laid down by
/// every one after, so a function called twice reads on its second
/// call the slots its first primed. For the main op array it is what
/// `i_init_code_execute_data` does (`Z/Zend/zend_execute.c:4516`): a
/// fresh cache on every activation, since php frees a main op array's
/// with the op array (`ZEND_ACC_HEAP_RT_CACHE`) and a file included
/// twice is compiled or fetched twice. One storage holds both, the
/// request's map table; only the activation differs.
///
/// What a second call reads is the first call's resolution, so a name
/// the first call resolved past the namespace stays resolved that way
/// once the namespace binds the name; a cache the frame owned would
/// look the name up again and find the new binding, where php does not:
///
/// ```php
/// namespace Foo;
/// function f() { return strlen("abc"); }
/// echo f(), "\n";
/// if (true) { function strlen($s) { return 99; } }
/// echo f(), "\n";
/// ```
/// ```expect
/// 3
/// 3
/// ```
///
/// A closure rebound to another scope, and a trait's method in each
/// class that uses it, reads the cache its op array has, where php
/// gives each a cache of its own (`zend_create_closure`,
/// `Z/Zend/zend_closures.c:794`; `zend_add_trait_method`,
/// `Z/Zend/zend_inheritance.c:2369`) because its
/// slots hold what the scope decides. A function, a class and a
/// constant resolve the same in every scope; a method's callee does
/// not, so [`Cached::ClassAndCallee`] carries the scope it was resolved
/// from and answers no other. The day a slot here holds a property
/// offset, a rebound closure takes its own cache there.
pub(crate) fn init_runtime_cache(map: &mut MapTable, code: &OpArray, body: Body) {
    let cache = map.map(runtime_cache_of(code));
    if body == Body::Main || cache.slots.is_none() {
        cache.lay_down(code.cache_size);
    }
}

/// One `FAST_CALL`'s temporary while the `finally` block it entered
/// runs (`Z/Zend/zend_vm_def.h:8647`): the two fields php keeps in one
/// zval there, held apart.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FastCall {
    /// Which temporary of the frame, the `FAST_CALL`'s `result`.
    var: u32,
    /// `Z_OPLINE_NUM_P`: the `FAST_CALL` the block is to return past,
    /// and none where an unwind entered it, php's `(uint32_t)-1`.
    pub(crate) from: Option<u32>,
    /// `Z_OBJ_P`: what an unwind delayed while the block runs, a
    /// throwable with one owner held here or the graceful exit, until a
    /// `FAST_RET` makes it pending again, a `DISCARD_EXCEPTION` gives it
    /// up, or the unwinder chains it.
    pub(crate) delayed: Option<crate::api::request::Raised>,
}

/// One slot of the runtime cache, the `void *` at `CACHE_ADDR(num)`,
/// named by what the opline that owns the slot writes into it:
/// `CACHE_PTR(opline->result.num, fbc)` from the `INIT_*` handlers,
/// `CACHE_PTR(..., ce)` from the handlers that resolve a class off a
/// constant pair, and `zend_jmp_fl_result` from `JMP_FRAMELESS`, whose
/// three values are `ZEND_JMP_FL_UNPRIMED = 0`, `ZEND_JMP_FL_MISS = 1`
/// and `ZEND_JMP_FL_HIT = 2` (`Z/Zend/zend_frameless_function.h:126`).
/// Zero is what the fresh cache holds, so the first pass of any of them
/// finds `Unprimed` and looks the table up once.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Cached {
    /// Nothing written yet: the table is looked up on this pass.
    #[default]
    Unprimed,
    /// The callee an `INIT_FCALL`, `INIT_FCALL_BY_NAME` or
    /// `INIT_NS_FCALL_BY_NAME` resolved on its first pass.
    Function(Function),
    /// The class a `CATCH`, `NEW`, `INSTANCEOF` or
    /// `FETCH_CLASS_CONSTANT` resolved off its constant pair on its
    /// first pass, `CACHE_PTR(..., ce)`. A class the linker has placed
    /// stays in the table for the life of the request, so a slot this
    /// primed answers every pass after.
    Class(ClassId),
    /// The class and the callee an `INIT_STATIC_METHOD_CALL` over two
    /// constants, or an `INIT_METHOD_CALL` over a constant name,
    /// resolved together, `CACHE_POLYMORPHIC_PTR(opline->result.num,
    /// ce, fbc)`: php writes the class into the slot the opline names
    /// and the callee into the one after it, and reads the callee back
    /// only where the class of this pass, the one named or the
    /// receiver's, is the one written (`Z/Zend/zend_vm_def.h:3670`,
    /// `:3778`). One variant holds both, so the pair is written and
    /// read as one, and the second slot the compiler reserves stands as
    /// php's dump has it.
    ///
    /// `scope` is the calling scope the callee was resolved from, which
    /// the access rule reads, and the private method of the scope that
    /// stands in for another class's (`zend_std_get_method`,
    /// `Z/Zend/zend_object_handlers.c:1939`). php keys on no scope
    /// because its cache is one per scope: a trait's method is copied
    /// into each class that uses it with a cache of its own
    /// (`zend_add_trait_method`, `Z/Zend/zend_inheritance.c:2369`), and
    /// a closure bound to another scope is given a fresh one
    /// (`zend_create_closure`, `Z/Zend/zend_closures.c:794`). This
    /// engine's cache is one per op array, so the pair is read back only
    /// in the scope that wrote it, and two scopes calling through one
    /// opline take the slot in turn where php's keep one each. What the
    /// slot holds is what was asked and what answered, never how the
    /// answer was found.
    ClassAndCallee {
        class: ClassId,
        scope: Option<ClassId>,
        function: Function,
    },
    /// `ZEND_JMP_FL_MISS`: the table has the name in the namespace, so
    /// the jump is not taken and the call by name after it runs.
    Miss,
    /// `ZEND_JMP_FL_HIT`: the table has not the name in the namespace,
    /// so the jump lands on the frameless call over the global one.
    Hit,
}

/// `sizeof(void *)`: what `zend_alloc_cache_slot` grows `cache_size` by
/// for one slot, so a slot's number is its byte offset over this.
const SLOT_BYTES: u32 = 8;

/// One call from its `INIT_FCALL` until the `DO_*CALL` that serves it
/// returns: what php pushes on the VM stack as a call frame, and keeps
/// there while the callee runs. A `FRAMELESS_ICALL_*` lays one down too,
/// over its operands and already serving, for the span of the native:
/// php pushes no frame for it, but resolves the function off the opline
/// wherever a frame would have been read, in `zend_active_function` for
/// a raise's words and in `zend_fetch_debug_backtrace` for the frame a
/// trace prints, and this is that resolution held in one place.
pub(crate) struct CallState {
    /// The callee, resolved at the `INIT_FCALL`.
    pub(crate) function: Function,
    /// The callee's name as the `INIT_*` constant spelled it, for what a
    /// raise at the `INIT` says; none for a frameless call, which no
    /// constant names and no raise reads.
    pub(crate) name: Option<PhpString>,
    /// The arguments the `SEND_*` oplines have placed, read off this
    /// frame by the callee and by a backtrace taken while it runs, as
    /// `ZEND_CALL_ARG` reads them off php's; nothing copies them.
    pub(crate) arguments: Vec<Value>,
    /// How many the `INIT_FCALL` said there are, which bounds them.
    pub(crate) count: usize,
    /// `ZEND_CALL_HAS_THIS`: the receiver of a method call, php's
    /// `EX(This)`, with whether the call owns it
    /// (`ZEND_CALL_RELEASE_THIS`); none for a function or a method
    /// called on the class.
    pub(crate) receiver: Option<Receiver>,
    /// The called scope, `static`: the class a method was called on,
    /// which is the receiver's for an instance call and the one named
    /// for a call on the class; none for a function.
    pub(crate) called_scope: Option<ClassId>,
    /// `ZEND_CALL_CLOSURE`: the `Closure` object an
    /// `INIT_DYNAMIC_CALL` or a `Closure::__invoke` found the callee
    /// on, one owner the call holds, which the frame it becomes takes
    /// over; none for every other call.
    pub(crate) closure: Option<PhpObject>,
    /// `ZEND_CALL_HAS_EXTRA_NAMED_PARAMS` and `call->extra_named_params`:
    /// the named arguments the callee declares no parameter for, placed
    /// by `zend_handle_named_arg` for a variadic callee, at a call site's
    /// `SEND_*` and in `zend_call_function`'s walk alike. A native reads
    /// them off the call as `Z_PARAM_VARIADIC_WITH_NAMED` reads them, or
    /// refuses them as `Z_PARAM_VARIADIC` refuses them; a user
    /// function's frame takes them over. Each name and value one owner
    /// the call holds, given up with it.
    pub(crate) extra_named: ExtraNamed,
    /// Where the call stands.
    pub(crate) stage: CallStage,
    /// `ZEND_CALL_SEND_ARG_BY_REF`: what the last `CHECK_FUNC_ARG` read
    /// off the callee for the argument after it, whether its parameter
    /// takes it by reference, which the `_FUNC_ARG` fetch and the
    /// `SEND_FUNC_ARG` after it serve as a write or as a read by.
    pub(crate) send_arg_by_ref: bool,
    /// Whether the call is a `FRAMELESS_ICALL_*`'s, whose arguments are
    /// the opline's operands: a coercion of one is kept in `coerced` and
    /// not written over the argument, as `Z_FLF_PARAM_STR` converts in a
    /// temporary of the body's and a trace still reads the operand.
    pub(crate) frameless: bool,
    /// `USED_RET()` (`Z/Zend/zend_API.h`): whether the caller reads what
    /// the callee returns.
    pub(crate) return_value: ReturnValue,
    /// `ZEND_CALL_DYNAMIC`: the callee was named as the program served,
    /// by `INIT_DYNAMIC_CALL` or `INIT_USER_CALL`, which a native that
    /// reads its caller's frame refuses (`zend_forbid_dynamic_call`).
    pub(crate) dynamic: bool,
    /// php's `str_tmp`: what a coercion made for a frameless call, freed
    /// with the call as `Z_FLF_PARAM_FREE_STR` frees it at the body's end.
    pub(crate) coerced: Vec<Value>,
    /// How far down its native's chain of interceptions the call is
    /// served: zero for the entry the function table holds, the last
    /// interception's, and one more for each
    /// [`crate::api::call::passthrough`] to the entry that one replaced.
    pub(crate) beneath: usize,
}

/// `USED_RET()`: whether a call's caller reads what it returns. It is
/// `Unused` only where a `DO_*CALL` whose `result` operand is `UNUSED`
/// serves the call; a call `zend_call_function` makes is `Used`, as php's
/// test finds the frame before the callee's internal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReturnValue {
    Used,
    Unused,
}

/// Where a call stands: its `SEND_*` oplines placing arguments after the
/// `INIT_FCALL`, or its `DO_*CALL` serving it. A backtrace sees the
/// second and not the first, as `zend_fetch_debug_backtrace` walks
/// `EG(current_execute_data)`, a chain a frame `INIT_FCALL` has pushed
/// and nothing has activated is not on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallStage {
    Building,
    Serving,
}

/// Every frame's engine state, and the calls being built.
#[derive(Default)]
pub(crate) struct Vm {
    frames: Vec<FrameState>,
    calls: Vec<CallState>,
    /// php's `execute_fake` placeholders that outlive the generator frame
    /// they stood under, while what the frame held is released.
    placeholders: Vec<Placeholder>,
    /// The sites standing, `zend_get_attribute_object`'s dummy frames.
    sites: Vec<Site>,
    /// How many times a kernel has looked a name up among the linked
    /// functions, the linked classes or a receiver's methods, which is
    /// what a primed cache slot spares: the tests count it across two
    /// passes of one opline.
    #[cfg(test)]
    pub(crate) lookups: u32,
}

/// `zend_hash_find_known_hash(EG(function_table), name)` from a kernel
/// whose opline owns a cache slot: the lookup a primed slot spares, so
/// every one is made here and the tests count them.
pub(crate) fn find_function(req: &mut Request, name: &[u8]) -> Option<Function> {
    #[cfg(test)]
    {
        req.vm_mut().lookups = req.vm_mut().lookups.saturating_add(1);
    }
    req.function_named(name)
}

/// `zend_lookup_class_ex(name, NULL, ZEND_FETCH_CLASS_NO_AUTOLOAD)`
/// from a kernel whose opline owns a cache slot: the linked classes'
/// half of [`find_function`], made here and nowhere else so the tests
/// count what a primed slot spares. The lowered name is what the lookup
/// is keyed by, so it is the half of the constant pair a caller hands
/// over.
pub(crate) fn find_class(req: &mut Request, lowered: &[u8]) -> Option<ClassId> {
    #[cfg(test)]
    {
        req.vm_mut().lookups = req.vm_mut().lookups.saturating_add(1);
    }
    req.class_named(lowered)
}

/// php's `execute_fake` placeholder standing where a generator's frame
/// stood, the frame gone from the stack while `zend_generator_close`
/// releases what it held, since `EG(current_execute_data)` is moved back
/// to the placeholder and not past it: a backtrace a destructor takes
/// still reads the generators the resumption came through
/// ([`crate::generator::placeholder_frames`]). `frame` and `calls` are
/// where on the stack the generator's frame stood.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Placeholder {
    pub(crate) frame: usize,
    pub(crate) calls: usize,
    pub(crate) generator: PhpObject,
}

/// `zend_get_attribute_object`'s dummy call frame
/// (`Z/Zend/zend_attributes.c:340-370`): user code at a file and a line
/// of its own, the attribute's, which serves nothing and shows no trace
/// row (`ZEND_ACC_CALL_VIA_TRAMPOLINE`), so what is called from it is
/// called from there. It is the executing site while nothing stands
/// above it, the site the row above it names, and the caller whose
/// `strict_types` an argument above it is checked under. `frame` and
/// `calls` are where it stands among the frames and the calls, as a
/// [`Placeholder`]'s are.
#[derive(Debug, Clone)]
pub(crate) struct Site {
    pub(crate) frame: usize,
    pub(crate) calls: usize,
    pub(crate) file: Box<[u8]>,
    pub(crate) line: u32,
    /// `ZEND_ACC_STRICT_TYPES` on the dummy function, the attribute's
    /// `ZEND_ATTRIBUTE_STRICT_TYPES`.
    pub(crate) strict: bool,
}

impl Vm {
    /// The sites standing, the innermost last.
    pub(crate) fn sites(&self) -> &[Site] {
        &self.sites
    }

    /// The innermost site where no frame stands above it: what php's
    /// `EG(current_execute_data)` walk meets first as user code.
    pub(crate) fn innermost_site(&self) -> Option<&Site> {
        self.sites
            .last()
            .filter(|site| site.frame >= self.frames.len())
    }

    /// A site at `file` and `line` above everything standing, until
    /// [`Vm::pop_site`].
    pub(crate) fn push_site(&mut self, file: &[u8], line: u32, strict: bool) {
        self.sites.push(Site {
            frame: self.frames.len(),
            calls: self.calls.len(),
            file: file.into(),
            line,
            strict,
        });
    }

    pub(crate) fn pop_site(&mut self) {
        self.sites.pop();
    }

    /// The placeholders standing, the innermost last.
    pub(crate) fn placeholders(&self) -> &[Placeholder] {
        &self.placeholders
    }

    /// A placeholder where `generator`'s frame stood, the frame just
    /// taken off the stack, until [`Vm::pop_placeholder`].
    pub(crate) fn push_placeholder(&mut self, generator: PhpObject) {
        self.placeholders.push(Placeholder {
            frame: self.frames.len(),
            calls: self.calls.len(),
            generator,
        });
    }

    pub(crate) fn pop_placeholder(&mut self) {
        self.placeholders.pop();
    }

    /// Push one frame's state; its place is the `FrameRef` the VM stack
    /// gave the frame, and the two are pushed and popped together. The
    /// slots are the stack's, every one `IS_UNDEF` until an opline
    /// writes it (`i_init_func_execute_data`), which is what a read of
    /// a CV warns of.
    pub(crate) fn push(&mut self, unit: Arc<Unit>) {
        self.push_body(unit, Body::Main, None, None, None, 0, None, Caller::Outside);
    }

    /// Push the state of a frame serving `body` of `unit`: the function
    /// it is, its receiver and called scope, how many arguments the
    /// call placed, where the caller wants what it returns, and the
    /// frame whose opline pushed the call
    /// (`zend_vm_stack_push_call_frame` with `i_init_func_execute_data`'s
    /// fields filled).
    #[allow(clippy::too_many_arguments)] // the frame header's fields, each one fact
    pub(crate) fn push_body(
        &mut self,
        unit: Arc<Unit>,
        body: Body,
        function: Option<Function>,
        receiver: Option<Receiver>,
        called_scope: Option<ClassId>,
        num_args: u32,
        returns_to: Option<(FrameRef, u32)>,
        caller: Caller,
    ) {
        let runtime_cache = body_of(&unit, body).map_or(MapPtr::new(0), runtime_cache_of);
        let calls_below = u32::try_from(self.calls.len()).unwrap_or(u32::MAX);
        self.frames.push(FrameState {
            unit,
            body,
            function,
            opline: 0,
            returns_to,
            caller,
            receiver,
            called_scope,
            scope: None,
            symbol_table: None,
            closure: None,
            num_args,
            extra_args: Vec::new(),
            extra_named: ExtraNamed::default(),
            calls_below,
            leaving: false,
            generator: None,
            fast_calls: Vec::new(),
            op_array: None,
            runtime_cache,
            identity: Cell::new(None),
        });
    }

    /// A frame's state that was off the stack back on top of it, the
    /// half of a generator's resumption that is the engine's: the calls
    /// its body was building when it yielded go back on the call stack
    /// above the ones standing, and the frame counts those as below it
    /// (`zend_generator_restore_call_stack`).
    pub(crate) fn resume(&mut self, mut state: FrameState, calls: Vec<CallState>) {
        state.calls_below = u32::try_from(self.calls.len()).unwrap_or(u32::MAX);
        self.calls.extend(calls);
        self.frames.push(state);
    }

    /// The innermost frame's state off the stack with the calls it was
    /// building, which stand above its `calls_below`: what
    /// `zend_generator_freeze_call_stack` takes with a generator that
    /// yields in the middle of a call's arguments.
    pub(crate) fn suspend(&mut self) -> Option<(FrameState, Vec<CallState>)> {
        let state = self.frames.pop()?;
        let below = usize::try_from(state.calls_below)
            .unwrap_or(usize::MAX)
            .min(self.calls.len());
        let calls = self.calls.split_off(below);
        Some((state, calls))
    }

    /// Every frame and call of this execution lent its receiver made its
    /// owner: the objects the caller is to take one owner of for each,
    /// and let one pin of each go.
    pub(crate) fn own_lent(&mut self) -> Vec<PhpObject> {
        let mut taken = Vec::new();
        let receivers = self
            .frames
            .iter_mut()
            .map(|state| &mut state.receiver)
            .chain(self.calls.iter_mut().map(|call| &mut call.receiver));
        for receiver in receivers {
            if let Some(Receiver::Lent(held)) = *receiver {
                *receiver = Some(Receiver::Owned(held));
                taken.push(held);
            }
        }
        taken
    }

    /// The frame's state off the stack; its slots are the heap frame's,
    /// released by the request before it pops both, and its receiver and
    /// extra arguments are handed back for the request to release.
    pub(crate) fn pop(&mut self) -> Option<FrameState> {
        self.frames.pop()
    }

    /// The innermost frame's state, the one the loop is executing.
    pub(crate) fn innermost(&self) -> Option<&FrameState> {
        self.frames.last()
    }

    /// Every frame's state, the entry frame first.
    pub(crate) fn frames(&self) -> &[FrameState] {
        &self.frames
    }

    /// `Z_OBJ(EX(This))`: the receiver of `frame`, a view with no owner
    /// taken; none where the frame has none.
    pub(crate) fn this(&self, frame: FrameRef) -> Option<PhpObject> {
        self.at(frame).and_then(FrameState::this)
    }

    pub(crate) fn at(&self, frame: FrameRef) -> Option<&FrameState> {
        self.frames.get(frame.0 as usize)
    }

    pub(crate) fn at_mut(&mut self, frame: FrameRef) -> Option<&mut FrameState> {
        self.frames.get_mut(frame.0 as usize)
    }

    /// The unit and the op array being served in `frame`, the unit
    /// shared rather than copied, so the loop can read its oplines while
    /// the request is borrowed.
    pub(crate) fn serving(&self, frame: FrameRef) -> Option<Serving> {
        self.at(frame).map(|state| Serving {
            unit: Arc::clone(&state.unit),
            body: state.body,
        })
    }

    /// Where `frame` is to write what it returns, and nothing when the
    /// call that pushed it wants no value.
    pub(crate) fn returns_to(&self, frame: FrameRef) -> Option<(FrameRef, u32)> {
        self.at(frame).and_then(|state| state.returns_to)
    }

    /// Where the loop stands in `frame`.
    pub(crate) fn opline(&self, frame: FrameRef) -> u32 {
        self.at(frame).map_or(0, |state| state.opline)
    }

    /// `CACHED_PTR(num)`: what the runtime cache of the op array `frame`
    /// serves holds at `slot`, read off the request's map table
    /// (`EX(run_time_cache)`, [`RuntimeCache::get`]).
    pub(crate) fn cached(&self, map: &MapTable, frame: FrameRef, slot: CacheSlot) -> Cached {
        self.at(frame)
            .and_then(|state| map.get(state.runtime_cache))
            .map_or(Cached::Unprimed, |cache| cache.get(slot))
    }

    /// `CACHE_PTR(num, ptr)`: `value` into the runtime cache of the op
    /// array `frame` serves at `slot`, in the request's map table
    /// ([`RuntimeCache::set`]).
    pub(crate) fn prime(
        &self,
        map: &mut MapTable,
        frame: FrameRef,
        slot: CacheSlot,
        value: Cached,
    ) {
        if let Some(state) = self.at(frame) {
            map.map(state.runtime_cache).set(slot, value);
        }
    }

    /// `ZEND_OBSERVER_DATA(EX(func))`: the observer slot of the op array
    /// `frame` serves, in the request's map table; unresolved for a frame
    /// that stands nowhere.
    pub(crate) fn observer_slot(&self, map: &MapTable, frame: FrameRef) -> crate::observer::Slot {
        self.observer_slot_held(map, frame)
            .cloned()
            .unwrap_or_default()
    }

    /// [`Vm::observer_slot`] borrowed where it stands, for the walks that
    /// read one handler of it at a time.
    pub(crate) fn observer_slot_held<'map>(
        &self,
        map: &'map MapTable,
        frame: FrameRef,
    ) -> Option<&'map crate::observer::Slot> {
        self.at(frame)
            .and_then(|state| map.get(state.runtime_cache))
            .map(|cache| &cache.observer)
    }

    /// The observer slot of the op array `frame` serves, resolved to
    /// `slot` (`zend_observer_fcall_install`).
    pub(crate) fn set_observer_slot(
        &self,
        map: &mut MapTable,
        frame: FrameRef,
        slot: crate::observer::Slot,
    ) {
        if let Some(state) = self.at(frame) {
            map.map(state.runtime_cache).observer = slot;
        }
    }

    /// `ZEND_VM_SET_OPCODE`: the frame stands at `at`, which is where a
    /// jump leaves it.
    pub(crate) fn jump(&mut self, frame: FrameRef, at: u32) {
        if let Some(state) = self.at_mut(frame) {
            state.opline = at;
        }
    }

    /// `ZEND_VM_NEXT_OPCODE`: on to the opline after this one.
    pub(crate) fn advance(&mut self, frame: FrameRef) {
        if let Some(state) = self.at_mut(frame) {
            state.opline = state.opline.saturating_add(1);
        }
    }

    pub(crate) fn push_call(&mut self, call: CallState) {
        self.calls.push(call);
    }

    /// The call the `SEND_*` oplines are placing arguments into.
    pub(crate) fn building_call(&mut self) -> Option<&mut CallState> {
        self.calls.last_mut()
    }

    /// The callee of the call being built, which a `SEND_*_EX` reads the
    /// parameter's by-reference bit off (`ARG_MUST_BE_SENT_BY_REF`).
    pub(crate) fn building_call_function(&self) -> Option<Function> {
        self.calls.last().map(|call| call.function)
    }

    /// The call being served, for a coercion that writes an argument
    /// back into the frame as php's `Z_PARAM_*` do.
    pub(crate) fn serving_call_mut(&mut self) -> Option<&mut CallState> {
        self.calls
            .last_mut()
            .filter(|call| call.stage == CallStage::Serving)
    }

    /// `DO_*CALL` making the call it serves the current frame,
    /// `EG(current_execute_data) = call`: the callee, and the frame left
    /// on the stack for the callee to read its arguments off, with
    /// whether the opline's `result` operand is read. Nothing under a
    /// `DO_*CALL` is none.
    pub(crate) fn activate_call(&mut self, return_value: ReturnValue) -> Option<Function> {
        let call = self.calls.last_mut()?;
        call.stage = CallStage::Serving;
        call.return_value = return_value;
        Some(call.function)
    }

    /// `zend_vm_stack_free_call_frame`: the served call off the stack,
    /// its arguments and its name handed back for the request to release
    /// (`zend_vm_stack_free_args`).
    pub(crate) fn pop_call(&mut self) -> Option<CallState> {
        self.calls.pop()
    }

    /// The call being served, php's `execute_data` inside a native: the
    /// one the innermost `DO_*CALL` activated, and none while the loop is
    /// between calls or a call is still being built.
    /// How many calls are on the stack, being made or served: what tells
    /// a native served from inside an operation of the frame apart from
    /// the operation itself.
    pub(crate) fn call_depth(&self) -> usize {
        self.calls.len()
    }

    pub(crate) fn serving_call(&self) -> Option<&CallState> {
        self.calls
            .last()
            .filter(|call| call.stage == CallStage::Serving)
    }

    /// The fast-call cell of `frame`'s temporary `var`, made where the
    /// frame has none yet: php's zval is the temporary itself and is
    /// always there, so a first read makes the cell php would have read.
    pub(crate) fn fast_call(&mut self, frame: FrameRef, var: u32) -> Option<&mut FastCall> {
        let state = self.at_mut(frame)?;
        if let Some(at) = state.fast_calls.iter().position(|cell| cell.var == var) {
            return state.fast_calls.get_mut(at);
        }
        state.fast_calls.push(FastCall {
            var,
            from: None,
            delayed: None,
        });
        state.fast_calls.last_mut()
    }

    /// Every call on the stack, outermost first, each one's place among
    /// them what a frame's `calls_below` is counted against: what a
    /// backtrace interleaves with the frames to walk php's one chain.
    pub(crate) fn calls(&self) -> &[CallState] {
        &self.calls
    }
}

/// The main op array of `unit`, what the frame `serve` pushes serves.
pub(crate) fn code_of(unit: &Unit) -> &OpArray {
    &unit.main
}

/// The op array `body` names in `unit`; none for an index the unit has
/// not.
pub(crate) fn body_of(unit: &Unit, body: Body) -> Option<&OpArray> {
    match body {
        Body::Main => Some(&unit.main),
        Body::Function(at) => unit.functions.get(at as usize).map(|(_, code)| code),
        Body::Dynamic(at) => unit.dynamic.get(at as usize).map(|(_, code)| code),
        Body::Method { class, method } => unit
            .methods
            .iter()
            .find(|m| m.class == class && m.method == method)
            .map(|m| &m.body),
        Body::Hook {
            class,
            property,
            kind,
        } => unit
            .hooks
            .iter()
            .find(|h| h.class == class && h.property == property && h.kind == kind)
            .map(|h| &h.body),
    }
}

/// `ZEND_CALL_USES_STRICT_TYPES(call)` (`Z/Zend/zend_compile.h:716`):
/// whether the op array `state` serves was compiled under
/// `declare(strict_types=1)`, which the compiler sets on every op array
/// of a file that declared it ([`OpArray::strict_types`]). False for a
/// frame whose body the unit has not, which is this engine's error and
/// never a program's.
pub(crate) fn strict_types_of(state: &FrameState) -> bool {
    body_of(&state.unit, state.body).is_some_and(|code| code.strict_types)
}

/// The line of the opline `state` stands at, `EX(opline)->lineno`; zero
/// for a frame past its op array.
pub(crate) fn line_of(state: &FrameState) -> u32 {
    body_of(&state.unit, state.body)
        .and_then(|code| opline_at(code, state.opline))
        .map_or(0, |op| op.lineno)
}

/// The slot a `TMP`/`VAR` operand names: temporaries stand past the last
/// CV on the frame, as `EX_NUM_TO_VAR` places them past the last CV.
pub(crate) fn temporary_slot(code: &OpArray, number: u32) -> u32 {
    code.num_cvs.saturating_add(number)
}

/// One operand's value (seam 6, design §4.3): a constant from the op
/// array's table, a CV or a temporary from the frame. An `UNUSED`
/// operand is `&EX(This)` where the opcode reads one
/// (`GET_OP1_OBJ_ZVAL_PTR_UNDEF`), the frame's receiver as a view, and
/// reads as `null` on a frame that has none, which is what a handler
/// that reads one there would have been doing wrong.
///
/// A CV nothing has written is php's `BP_VAR_R` read of an undefined
/// variable, `zval_undefined_cv` (`Z/Zend/zend_execute.c:242`): the
/// warning naming the variable, and `null` in its place. php makes the
/// CV no less undefined for having been read, so a second read warns
/// again.
///
/// ```php
/// echo $a; $a = 1; echo $a, $a = 2, $a;
/// echo $b, $b;
/// ```
/// ```expect
/// Warning: Undefined variable $a in %s on line 2
/// 122
/// Warning: Undefined variable $b in %s on line 3
///
/// Warning: Undefined variable $b in %s on line 3
/// ```
pub(crate) fn operand(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    kind: OperandKind,
    field: u32,
) -> Value {
    match kind {
        OperandKind::Const => constant(code, ConstIdx(field)),
        OperandKind::Cv => {
            let value = req.slot(frame, field);
            if value.tag() != TypeTag::Undef {
                return value;
            }
            let name = code
                .vars
                .get(field as usize)
                .map(|name| String::from_utf8_lossy(&name.0).into_owned())
                .unwrap_or_default();
            warning(req, &format!("Undefined variable ${name}"));
            Value::null()
        }
        OperandKind::Tmp | OperandKind::Var => req.slot(frame, temporary_slot(code, field)),
        OperandKind::Unused => req
            .vm()
            .this(frame)
            .map_or_else(Value::null, |this| this.as_value()),
    }
}

/// `GET_OP1_ZVAL_PTR_UNDEF`: [`operand`] without the warning, for a
/// handler that reads an undefined CV as the `UNDEF` it is and decides
/// for itself, as `SWITCH_LONG` falls through to the chain and `MATCH`
/// warns on its own.
pub(crate) fn operand_undef(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    kind: OperandKind,
    field: u32,
) -> Value {
    match kind {
        OperandKind::Cv => req.slot(frame, field),
        _ => operand(req, frame, code, kind, field),
    }
}

/// `GET_OP1_OBJ_ZVAL_PTR_PTR_UNDEF(BP_VAR_W)`: the container of a
/// property write, as it stands and with no diagnostic, since the write
/// site raises its own `Error` over what it finds there: a CV read
/// undefined or not (`BP_VAR_W` warns of nothing), a `VAR` read through
/// the `IS_INDIRECT` a `FETCH_*_W` left in it (`_get_zval_ptr_ptr_var`,
/// `Z_INDIRECT`) and as it stands otherwise, which is what `NEW` and
/// `FETCH_THIS` leave. An `UNUSED` operand is `$this`, the frame's
/// receiver, which the compiler writes only inside an instance method's
/// body; a `CONST` or a `TMP` is no container php compiles a write over.
pub(crate) fn container(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    kind: OperandKind,
    field: u32,
    opcode: &str,
) -> Result<Value, php_heap::Fatal> {
    match kind {
        OperandKind::Cv => Ok(req.slot(frame, field)),
        OperandKind::Var => {
            let slot = temporary_slot(code, field);
            let Some(at) = req.slot_ptr(frame, slot) else {
                return Ok(Value::undef());
            };
            let z = at.read();
            Ok(value_of(z.as_indirect().map_or(z, php_heap::ZvalRef::read)))
        }
        OperandKind::Unused => req.vm().this(frame).map_or_else(
            || {
                Err(php_heap::Fatal::Error(format!(
                    "InternalError: {opcode} over $this on a frame with no receiver"
                )))
            },
            |this| Ok(this.as_value()),
        ),
        OperandKind::Const | OperandKind::Tmp => Err(php_heap::Fatal::Error(format!(
            "InternalError: {opcode} over a constant or a temporary, which php compiles no write over"
        ))),
    }
}

/// The operand's value as the handler's own: a `CV` or a `CONST` with
/// one more owner taken (`ZVAL_COPY`, `Z_TRY_ADDREF`), a `TMP` or a `VAR`
/// moved out of its slot, which is left `UNDEF` (`ZVAL_COPY_VALUE` and
/// no `FREE_OP`). This is what `ASSIGN`, `SEND_*` and `RETURN` read,
/// each of them a handler that keeps the value past the opline, and
/// each of them `ZVAL_COPY_DEREF` over a CV: a reference the CV holds
/// is followed and the value it holds is what is copied, never the
/// cell (`zend_copy_to_variable`, `Z/Zend/zend_execute.h:142-145`). A
/// `VAR` holding a reference, what `ASSIGN_REF` leaves under
/// `RETURN_VALUE_USED`, is followed the same way (lines 156-161): the
/// value the cell holds is the handler's with one more owner, and the
/// `VAR`'s owner of the cell is given up, which frees the cell when it
/// was the last.
pub(crate) fn take_operand(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    kind: OperandKind,
    field: u32,
) -> Value {
    match kind {
        OperandKind::Tmp | OperandKind::Var => {
            let value = req.take_slot(frame, temporary_slot(code, field));
            match value.as_php_ref() {
                Some(cell) => {
                    let held = cell.get();
                    req.addref(&held);
                    req.release(value);
                    held
                }
                None => value,
            }
        }
        OperandKind::Const | OperandKind::Cv | OperandKind::Unused => {
            let value = operand(req, frame, code, kind, field).deref();
            req.addref(&value);
            value
        }
    }
}

/// `FREE_OP`: a `TMP` or a `VAR` operand read by [`operand`] and done
/// with, its slot's owner given up as `zval_ptr_dtor_nogc` gives it up,
/// never a possible root; a `CV` and a `CONST` are nobody's to free here
/// and nothing happens.
pub(crate) fn free_operand(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    kind: OperandKind,
    field: u32,
) {
    if matches!(kind, OperandKind::Tmp | OperandKind::Var) {
        let value = req.take_slot(frame, temporary_slot(code, field));
        req.release_nogc(value);
    }
}

/// A property's name as the opline carries it in `op2`, php's `name` and
/// `tmp_name` pair: the constant string the compiler wrote, or the value
/// a `CV` or a temporary holds (`$o->$p`, `$o->{expr}`), read and
/// converted once where the handler asks for it, which is where php's
/// `zval_try_get_tmp_string(GET_OP2_ZVAL_PTR(BP_VAR_R), &tmp_name)`
/// stands, and given up with the operand when the handler is done
/// ([`Member::release`], `zend_tmp_string_release` and `FREE_OP2`).
///
/// The read warns of a variable nothing wrote and names the empty
/// property; the conversion is `zval_try_get_string`'s, so an array is
/// `"Array"` after its warning and an object without `__toString` is the
/// `Error` in place of the fetch.
///
/// ```php
/// class C { public $a = 1; function __toString(): string { return 'a'; } }
/// $o = new C;
/// echo $o->$undefined ?? 'n', "\n";
/// echo $o->{new C}, "\n";
/// try { echo $o->{new stdClass}; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $n = null;
/// echo $n->$undefined, "|\n";
/// $x = 'b';
/// try { $n->$x = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $k = [1];
/// echo $o->$k ?? 'arr', "\n";
/// $t = true;
/// $o->$t = 2;
/// var_dump(isset($n->$x), isset($o->{1}), $o->$t);
/// ```
/// ```expect
/// Warning: Undefined variable $undefined in %s on line 4
/// n
/// 1
/// Object of class stdClass could not be converted to string
///
/// Warning: Undefined variable $undefined in %s on line 8
///
/// Warning: Attempt to read property "" on null in %s on line 8
/// |
/// Attempt to assign property "b" on null
///
/// Warning: Array to string conversion in %s on line 12
/// arr
///
/// Deprecated: Creation of dynamic property C::$1 is deprecated in %s on line 14
/// bool(false)
/// bool(true)
/// int(2)
/// ```
#[derive(Default)]
pub(crate) struct Member {
    read: Option<Value>,
    /// Whether the handler holds an owner of what was read: a
    /// constant string is the op array's, and anything else is the
    /// string the conversion answered with one owner.
    owned: bool,
}

impl Member {
    /// The name, read and converted on the first ask and the same value
    /// after it: the constant itself, or a string the handler holds one
    /// owner of until [`Member::release`].
    pub(crate) fn name(
        &mut self,
        req: &mut Request,
        frame: FrameRef,
        code: &OpArray,
        op: &Opline,
    ) -> Result<Value, Thrown> {
        if let Some(read) = self.read {
            return Ok(read);
        }
        let value = operand(req, frame, code, op.op2_type, op.op2).deref();
        let name = if op.op2_type == OperandKind::Const && value.as_str_bytes().is_some() {
            value
        } else {
            self.owned = true;
            Value::string(crate::ops::to_string(req, &value)?)
        };
        self.read = Some(name);
        Ok(name)
    }

    /// The name's bytes, as [`Member::name`] reads them.
    pub(crate) fn bytes(
        &mut self,
        req: &mut Request,
        frame: FrameRef,
        code: &OpArray,
        op: &Opline,
    ) -> Result<Vec<u8>, Thrown> {
        let name = self.name(req, frame, code, op)?;
        Ok(name.as_str_bytes().unwrap_or_default().to_vec())
    }

    /// `if (OP2_TYPE == IS_CV && Z_TYPE_P(EX_VAR(opline->op2.var)) ==
    /// IS_UNDEF) ZVAL_UNDEFINED_OP2()`: the one thing a `BP_VAR_IS` fetch
    /// over what is no object asks of its name, which it never converts.
    pub(crate) fn touch(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) {
        if op.op2_type == OperandKind::Cv {
            operand(req, frame, code, op.op2_type, op.op2);
        }
    }

    /// `zend_tmp_string_release(tmp_name)` and `FREE_OP2()`: the string
    /// the conversion made given up, and the operand's temporary with it.
    pub(crate) fn release(self, req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) {
        if let Some(read) = self.read.filter(|_| self.owned) {
            req.release(read);
        }
        free_operand(req, frame, code, op.op2_type, op.op2);
    }
}

/// A method's name as `INIT_METHOD_CALL` and `INIT_STATIC_METHOD_CALL`
/// carry it in `op2`: the constant pair the compiler wrote, the name as
/// written and lowered, or the string a `CV` or a temporary holds, which
/// the handler takes an owner of, lowers itself as `get_method` does
/// with no key, and gives up with [`MethodName::release`].
pub(crate) struct MethodName {
    pub(crate) written: Value,
    pub(crate) lowered: Vec<u8>,
    owned: bool,
}

impl MethodName {
    /// The name `op2` carries. A method the program names is
    /// `GET_OP2_ZVAL_PTR_UNDEF(BP_VAR_R)`: the string the operand holds,
    /// through a reference, and php's `Error` `Method name must be a
    /// string` for anything else, after the warning for a variable
    /// nothing wrote; either way the operand is freed here (`FREE_OP2`),
    /// since nothing after reads it.
    pub(crate) fn of(
        req: &mut Request,
        frame: FrameRef,
        code: &OpArray,
        op: &Opline,
    ) -> Result<MethodName, Thrown> {
        if op.op2_type == OperandKind::Const {
            let lowered = constant(code, ConstIdx(op.op2.saturating_add(1)));
            return Ok(MethodName {
                written: constant(code, ConstIdx(op.op2)),
                lowered: lowered.as_str_bytes().unwrap_or_default().to_vec(),
                owned: false,
            });
        }
        let value = operand_undef(req, frame, code, op.op2_type, op.op2);
        let held = value.deref();
        let name = if let Some(bytes) = held.as_str_bytes() {
            let lowered = bytes.to_ascii_lowercase();
            req.addref(&held);
            Ok(MethodName {
                written: held,
                lowered,
                owned: true,
            })
        } else {
            if value.tag() == TypeTag::Undef {
                operand(req, frame, code, op.op2_type, op.op2);
            }
            Err(crate::api::raising::throw_error(
                req,
                "Error",
                "Method name must be a string",
            ))
        };
        free_operand(req, frame, code, op.op2_type, op.op2);
        name
    }

    /// The owner of a name the program computed given up.
    pub(crate) fn release(self, req: &mut Request) {
        if self.owned {
            req.release(self.written);
        }
    }
}

/// `FREE_VAR_PTR_AND_EXTRACT_RESULT_IF_NECESSARY`
/// (`Z/Zend/zend_execute.c:163`): a `FETCH_DIM_W`, `FETCH_DIM_RW`,
/// `FETCH_OBJ_W` or `FETCH_OBJ_RW` over a `VAR` gives the container up
/// after it wrote the place it found into its own `VAR`. A container the
/// `VAR` holds the last owner of, a call's answer, takes the place with
/// it when it goes, so the `IS_INDIRECT` naming the place is first
/// replaced by what the place holds, one owner taken (`ZVAL_COPY`), and
/// the link after it writes into that copy. A `VAR` holding a CV's
/// `IS_INDIRECT` owns nothing and frees nothing.
///
/// So a trace's `args` written through is the copy's, and never the
/// frame's parameter (bug #79108), and a call's object or array written
/// through lives until the write is done:
///
/// ```php
/// function test(string $val) {
///     $a = &$val;
///     hackingHere();
///     echo $val, "\n";
/// }
/// function hackingHere() {
///     debug_backtrace()[1]['args'][0] = 'Modified';
/// }
/// test('Original');
/// class C { public $p = [0]; function __destruct() { echo "d\n"; } }
/// function g() { return new C; }
/// g()->p[0] = 1;
/// function f() { return [[1], [2]]; }
/// f()[1][0] = 5;
/// $c = new C;
/// function h($c) { return $c; }
/// h($c)->p[0] = 7;
/// var_dump($c->p);
/// echo "end\n";
/// ```
/// ```expect
/// Original
/// d
/// array(1) {
///   [0]=>
///   int(7)
/// }
/// end
/// d
/// ```
pub(crate) fn free_container_var(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) {
    if op.op1_type != OperandKind::Var {
        return;
    }
    let container = req.take_slot(frame, temporary_slot(code, op.op1));
    if container.to_zval().is_refcounted() && container.refcount() == 1 {
        let out = temporary_slot(code, op.out);
        if let Some(var) = req.slot_ptr(frame, out)
            && let Some(place) = var.read().as_indirect()
        {
            let held = value_of(place.read());
            req.addref(&held);
            var.write(held.to_zval());
        }
    }
    req.release_nogc(container);
}

/// `zend_array_dup(op_array->static_variables)`: a request's copy of
/// the op array's `static` table on its heap (design §5.13), each name
/// at the value the unit starts it at and a closure's `use` variable
/// `null` until its `BIND_LEXICAL` fills it, each at the position the
/// compiler recorded; none where the op array binds nothing, php's null
/// `static_variables`. The table's one owner is the caller's.
///
/// ```php
/// function t() { $k = 1; return function () use ($k) {}; }
/// $keep = []; $m = memory_get_usage();
/// for ($i = 0; $i < 100; $i++) $keep[] = t();
/// echo (memory_get_usage() - $m) / 100, "\n";
/// ```
/// ```expect
/// 786.16
/// ```
pub(crate) fn starting_statics(
    req: &mut Request,
    code: &OpArray,
) -> Result<Option<crate::api::values::PhpArray>, Thrown> {
    if code.bound_vars.is_empty() {
        return Ok(None);
    }
    let mut table = req.array()?;
    for var in &code.bound_vars {
        let value = if var.lexical {
            Value::null()
        } else {
            value_of(var.value)
        };
        req.addref(&value);
        let set = req
            .interned(&var.name.0)
            .and_then(|name| table.set(req, crate::api::values::Key::Str(name), value));
        if let Err(thrown) = set {
            req.release(Value::array(table));
            return Err(thrown);
        }
    }
    Ok(Some(table))
}

/// The values of a `static` table in position order, views; the
/// starting ones where the table was never laid down.
pub(crate) fn statics_values(
    code: &OpArray,
    table: Option<crate::api::values::PhpArray>,
) -> Vec<Value> {
    match table {
        Some(table) => (0..code.bound_vars.len())
            .map(|at| {
                u32::try_from(at)
                    .ok()
                    .and_then(|at| table.at_position(at))
                    .map_or_else(Value::undef, |(value, _)| value)
            })
            .collect(),
        None => code
            .bound_vars
            .iter()
            .map(|var| {
                if var.lexical {
                    Value::undef()
                } else {
                    value_of(var.value)
                }
            })
            .collect(),
    }
}

/// The op array's constant at `at`, as a value.
pub(crate) fn constant(code: &OpArray, at: ConstIdx) -> Value {
    code.literals
        .get(at.0 as usize)
        .copied()
        .map_or_else(Value::null, value_of)
}

/// `RT_CONSTANT(opline, opline->op2)` as `ZEND_RECV_INIT` reads it: the
/// value with one more owner, and, where it is the `IS_CONSTANT_AST` the
/// compiler left for a default naming a constant (`zend_compile_params`
/// under `ZEND_COMPILE_NO_PERSISTENT_CONSTANT_SUBSTITUTION`), the
/// constant expression it indexes, evaluated in the scope being executed,
/// which is the function's own (`zval_update_constant_with_ctx`,
/// `EX(func)->op_array.scope`) both where its `RECV_INIT` reads it and
/// where `zend_handle_undef_args` fills a skipped parameter under the
/// callee's frame. The second word says which of the two it was, since
/// php holds only what it evaluated to the parameter's type. php keeps
/// an evaluated value that is not refcounted in the opline's runtime
/// cache; nothing a program can see turns on that, and it is not kept
/// here.
pub(crate) fn default_value(
    req: &mut Request,
    code: &OpArray,
    at: ConstIdx,
) -> Result<(Value, bool), Thrown> {
    let unevaluated = code
        .literals
        .get(at.0 as usize)
        .and_then(|constant| constant.as_const_expr())
        .and_then(|at| code.const_exprs.get(at as usize));
    if let Some(expr) = unevaluated {
        let scope = req.executed_scope();
        return Ok((
            crate::api::call::eval_const_expr_made(req, expr, scope)?,
            true,
        ));
    }
    let value = constant(code, at);
    req.addref(&value);
    Ok((value, false))
}

/// A zval as the value vocabulary holds it, no owner taken: the string
/// or the array is named and not copied, as php's `ZVAL_COPY_VALUE`
/// names it.
pub(crate) fn value_of(zval: Zval) -> Value {
    Value::of_zval(zval)
}

/// The opline at `at` of `code`, if there is one.
pub(crate) fn opline_at(code: &OpArray, at: u32) -> Option<&Opline> {
    code.oplines.get(at as usize)
}

/// `i_init_func_execute_data`'s skip (`Z/Zend/zend_execute.c:4426-4437`)
/// and `zend_copy_extra_args`' (`:4363-4369`): where a frame just pushed
/// stands before the observer's begin, as php's `ZEND_DO_UCALL` lays the
/// frame down before `ZEND_OBSERVER_FCALL_BEGIN`, so a begin handler reads
/// the opline php's reads. A function that declares no type skips the
/// `RECV*` of each argument passed, `opline += num_args`, and of every
/// declared parameter where more were passed, `opline += first_extra_arg`,
/// so a parameter the call left out still reaches its `RECV` and its
/// `ArgumentCountError`, and a variadic its `RECV_VARIADIC`; never past
/// the entry safepoint. One that declares a type keeps every one of
/// them, since `ZEND_ACC_HAS_TYPE_HINTS` makes each a check: `None`.
pub(crate) fn skipped_recv(passed: u32, parameters: u32, typed: bool, entry: u32) -> Option<u32> {
    (!typed).then(|| passed.min(parameters).min(entry))
}

/// `i_init_func_execute_data`'s parameter prologue over a frame just
/// pushed (`Z/Zend/zend_execute.c:4426-4437`): the `RECV*` oplines from
/// where the push left the frame, past the ones php skips
/// ([`skipped_recv`]), up to the entry safepoint, each placing a
/// parameter and each running the type check the declaration carries.
/// The design's entry safepoint stands after the last of them either
/// way (§5.6, the accepted `trace:entry-counter-typed-recv` row), which
/// is why this runs before the loop is asked for the frame's activation.
/// php dispatches each through its VM, so an extension observing `RECV`
/// is told of each one served here (design §7.1).
///
/// `Some` is the frame a `RECV` raised on, whose throwable is pending;
/// the caller decides what a raise before the activation means to it.
/// Every call a user function is reached by crosses this: the one an
/// opline pushed ([`enter`]) and the one a native served
/// ([`Request::serve_user`]), so a `ReflectionMethod::invoke` verifies
/// what `DO_UCALL` verifies, and the coercion a scalar parameter
/// carries outside `strict_types` is served there too:
///
/// ```php
/// function f(int $a, float $b, string $c) { var_dump($a, $b, $c); }
/// (new ReflectionFunction("f"))->invoke("5", "2.5", 7);
/// ```
/// ```expect
/// int(5)
/// float(2.5)
/// string(1) "7"
/// ```
pub(crate) fn parameter_prologue(
    req: &mut Request,
    callee: FrameRef,
    body: &OpArray,
) -> Result<Option<FrameRef>, php_heap::Fatal> {
    let entry = body.safepoints.first().map_or(0, |at| at.0);
    while req.vm().opline(callee) < entry {
        let Some(prologue) = opline_at(body, req.vm().opline(callee)) else {
            break;
        };
        crate::observer::tell_before(req, callee, prologue.opcode);
        let stepped = match prologue.opcode {
            php_unit::Opcode::Recv => {
                crate::handlers::recv::<crate::tier::Nop>(req, callee, body, prologue)?
            }
            php_unit::Opcode::RecvInit => {
                crate::handlers::recv_init::<crate::tier::Nop>(req, callee, body, prologue)?
            }
            php_unit::Opcode::RecvVariadic => {
                crate::handlers::recv_variadic::<crate::tier::Nop>(req, callee, body, prologue)?
            }
            other => {
                return Err(php_heap::Fatal::Error(format!(
                    "InternalError: {} stands before the entry safepoint, and this engine serves no handler for it yet",
                    other.name()
                )));
            }
        };
        if let crate::tier::Stepped::Threw(at) = stepped {
            return Ok(Some(at));
        }
    }
    Ok(None)
}

/// `E_FATAL_ERRORS` (`Z/Zend/zend_errors.h:47`): the six levels a `@`
/// does not silence, and what `E_HAS_ONLY_FATAL_ERRORS` tests a mask
/// against.
pub(crate) const E_FATAL_ERRORS: i64 = Level::Error.bit()
    | Level::CoreError.bit()
    | Level::CompileError.bit()
    | Level::UserError.bit()
    | Level::RecoverableError.bit()
    | Level::Parse.bit();

/// `E_HAS_ONLY_FATAL_ERRORS(mask)` (`Z/Zend/zend_errors.h:49`).
pub(crate) fn has_only_fatal_errors(mask: i64) -> bool {
    mask & !E_FATAL_ERRORS == 0
}

/// The restore `END_SILENCE` and the unwinder share: the word put back
/// to what a `BEGIN_SILENCE` saved, where the word still holds only
/// fatal levels and the saved one held more.
pub(crate) fn restore_silenced(req: &mut Request, saved: &Value) {
    let saved = saved.as_int().unwrap_or(0);
    if has_only_fatal_errors(req.error_reporting()) && !has_only_fatal_errors(saved) {
        req.set_error_reporting(saved);
    }
}

/// The value a `FAST_CALL` emitted by a `return` was carrying past the
/// `finally` block in its `op2`, given up where the return never
/// happened.
pub(crate) fn free_carried_value(req: &mut Request, frame: FrameRef, code: &OpArray, from: u32) {
    let Some(op) = opline_at(code, from) else {
        return;
    };
    if !matches!(op.op2_type, OperandKind::Tmp | OperandKind::Var) {
        return;
    }
    let slot = temporary_slot(code, op.op2);
    let garbage = req.take_slot(frame, slot);
    req.release(garbage);
}

/// `cleanup_live_vars` (`Z/Zend/zend_execute.c:4893`): every live range
/// the unwind leaves, given up. A range is left when it is open at
/// `op_num` and the opline being landed on is at or past its end, but
/// for the half of a loop variable's range an early return split off,
/// whose loop the landing is still inside; `catch_op` of zero is php's
/// "landing nowhere", the walk out of the frame, where every open range
/// is left.
///
/// ```php
/// class C { function ok() { throw new Exception; } }
/// foreach ([new C, new C] as $c) {
///     try { if ($c->ok()) { return; } } catch (Exception $e) { echo "caught\n"; }
/// }
/// echo "done\n";
/// ```
/// ```expect
/// caught
/// caught
/// done
/// ```
///
/// php frees each kind by what it holds; every kind here is one value in
/// one temporary, freed the same way, but `ZEND_LIVE_SILENCE`, which
/// holds the `error_reporting` a `BEGIN_SILENCE` saved: the word is put
/// back as `END_SILENCE` would have put it ([`restore_silenced`]), so a
/// throw under a `@` does not leave the request silenced.
///
/// `ZEND_LIVE_NEW` holds the object a `NEW` made; its constructor threw,
/// so the object is marked as `zend_object_store_ctor_failed` marks it
/// and its destructor is never served, the operand beside it freed first:
///
/// ```php
/// class Foo { function __construct() { echo "Foo\n"; } function __destruct() { echo "~Foo\n"; } }
/// class Bar { function __construct() { echo "Bar\n"; throw new Exception; } function __destruct() { echo "~Bar\n"; } }
/// try { new Foo() + new Bar(); } catch (Exception $e) { echo "caught\n"; }
/// try { $b = new Bar; } catch (Exception $e) { echo "caught\n"; }
/// var_dump(isset($b));
/// ```
/// ```expect
/// Foo
/// Bar
/// ~Foo
/// caught
/// Bar
/// caught
/// bool(false)
/// ```
pub(crate) fn cleanup_live_vars(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op_num: u32,
    catch_op: u32,
) {
    for range in &code.live_ranges {
        if range.start.0 > op_num {
            break;
        }
        if op_num >= range.end.0 {
            continue;
        }
        if catch_op != 0 && (catch_op < range.end.0 || stays_in_split_loop(code, range, catch_op)) {
            continue;
        }
        let slot = temporary_slot(code, range.var);
        if range.kind == php_unit::LiveKind::Loop
            && let Some(held) = req.slot_ptr(frame, slot).map(php_heap::ZvalRef::read)
        {
            crate::handlers::ForeachVar::iterator_del(req, held);
        }
        let garbage = req.take_slot(frame, slot);
        release_live(req, range.kind, garbage);
    }
}

/// What one live range held, given up as its kind says, for the
/// unwinder and for a generator closed before it finished alike: the
/// `error_reporting` a `BEGIN_SILENCE` saved put back, the object whose
/// constructor never ran marked as `zend_object_store_ctor_failed`
/// marks it, as though its destructor had been served, so releasing it
/// serves none; and every kind released. A loop variable's iterator is
/// the caller's to delete first, while the slot still holds it.
///
/// A generator destroyed while it waits inside the arguments of a `new`
/// gives the object up with no destructor served, as an unwind would:
///
/// ```php
/// class D { function __destruct() { echo "D::__destruct\n"; } }
/// function g() { new D(yield); }
/// $g = g(); $g->current(); unset($g); echo "done\n";
/// ```
/// ```expect
/// done
/// ```
pub(crate) fn release_live(req: &mut Request, kind: php_unit::LiveKind, garbage: Value) {
    match kind {
        php_unit::LiveKind::Silence => restore_silenced(req, &garbage),
        php_unit::LiveKind::New => {
            if let Some(object) = garbage.as_object() {
                object.heap().set_destructor_called(true);
            }
        }
        php_unit::LiveKind::TmpVar | php_unit::LiveKind::Loop | php_unit::LiveKind::Rope => {}
    }
    req.release(garbage);
}

/// `ZEND_FREE_ON_RETURN` (`Z/Zend/zend_compile.h:1104`): the bit on the
/// `FREE` or `FE_FREE` a `return`, a `break` or a `continue` emits for a
/// loop variable it leaves behind.
const FREE_ON_RETURN: u32 = 1 << 0;

/// The distance from a `FREE` or `FE_FREE` flagged `ZEND_FREE_ON_RETURN`
/// to the loop's own last free, which `zend_calc_live_ranges` wrote into
/// its `op2`; none for any other opline.
pub(crate) fn loop_end_distance(op: &Opline) -> Option<u32> {
    (matches!(op.opcode, php_unit::Opcode::Free | php_unit::Opcode::FeFree)
        && op.extended_value & FREE_ON_RETURN != 0)
        .then_some(op.op2)
}

/// `cleanup_live_vars`'s "split range for loop vars": a range that ends
/// at an early return's free of its loop variable is only the half
/// before the return, and a landing inside the loop's rest still needs
/// the variable, so it is kept.
fn stays_in_split_loop(code: &OpArray, range: &php_unit::LiveRange, catch_op: u32) -> bool {
    opline_at(code, range.end.0)
        .and_then(loop_end_distance)
        .is_some_and(|to_end| catch_op < range.end.0.saturating_add(to_end))
}

/// `zend_unfinished_execution_gc_ex` (`Z/Zend/zend_execute.c:4658`):
/// what one frame that is not running holds, named for the collector.
/// The receiver and the closure its call released, the CVs, the extra
/// arguments and the extra named ones, the calls it was building
/// ([`unfinished_calls_gc`]), the temporaries and loop variables live
/// where it stands, and what its symbol table holds beside the CVs
/// attached to it. Every frame the collector reads is walked here, a
/// suspended generator's and each frame of a suspended fiber alike.
pub(crate) fn unfinished_execution_gc(
    req: &Request,
    slots: &php_heap::Frame,
    state: &FrameState,
    calls: &[CallState],
    visit: &mut dyn FnMut(Value),
) {
    if let Some(this) = state.receiver.and_then(Receiver::owned) {
        visit(this.as_value());
    }
    if let Some(closure) = state.closure {
        visit(closure.as_value());
    }
    let Some(code) = body_of(&state.unit, state.body) else {
        return;
    };
    let slot = |at: u32| Value::of_zval(slots.slot(php_heap::Slot(at)));
    for cv in 0..code.num_cvs {
        visit(slot(cv));
    }
    for extra in &state.extra_args {
        visit(*extra);
    }
    for (_, value) in state.extra_named.iter() {
        visit(*value);
    }
    unfinished_calls_gc(calls, visit);
    if state.opline != 0 {
        for range in &code.live_ranges {
            if range.start.0 > state.opline {
                break;
            }
            if state.opline < range.end.0
                && matches!(
                    range.kind,
                    php_unit::LiveKind::TmpVar | php_unit::LiveKind::Loop
                )
            {
                visit(slot(temporary_slot(code, range.var)));
            }
        }
    }
    if let Some(table) = state.symbol_table {
        req.gc_symbol_table(table, visit);
    }
}

/// `zend_unfinished_calls_gc`: the calls a frame was building,
/// innermost first, each with the arguments it has placed, its
/// receiver, its extra named arguments and its closure.
pub(crate) fn unfinished_calls_gc(calls: &[CallState], visit: &mut dyn FnMut(Value)) {
    for call in calls.iter().rev() {
        for argument in &call.arguments {
            visit(*argument);
        }
        if let Some(this) = call.receiver.and_then(Receiver::owned) {
            visit(this.as_value());
        }
        for (_, value) in call.extra_named.iter() {
            visit(*value);
        }
        if let Some(closure) = call.closure {
            visit(closure.as_value());
        }
    }
}

/// `cleanup_unfinished_calls` (`Z/Zend/zend_execute.c`): the calls the
/// frame was still building or serving when the throwable was raised,
/// given up innermost first. A frame records how many calls stood below
/// it, which is where its own begin.
pub(crate) fn cleanup_unfinished_calls(req: &mut Request, frame: FrameRef) {
    let below = req.vm().at(frame).map_or(0, |state| state.calls_below);
    while u32::try_from(req.vm().calls().len()).unwrap_or(0) > below {
        req.pop_call();
    }
}

/// `ZEND_LAST_CATCH` (`Z/Zend/zend_compile.h`): the bit beside the cache
/// slot on the `extended_value` of the last `CATCH` of a `try`, the one
/// that rethrows rather than jumping to a clause after it.
pub(crate) const LAST_CATCH: u32 = 1 << 0;

/// `CACHED_PTR(slot)`, else `zend_lookup_class_ex` under
/// `ZEND_FETCH_CLASS_NO_AUTOLOAD` over the lowered half of the constant
/// pair at `at` and `CACHE_PTR(slot, ce)` over what it found: the
/// silent resolution `ZEND_CATCH` and `ZEND_INSTANCEOF` make, and the
/// lookup half of the throwing one, `class_operand`. A name nothing
/// declares primes nothing — php's `CATCH` writes the null it found and
/// reads it back as unprimed, and the other three throw before
/// `CACHE_PTR` — so a class declared after a pass is found by the next.
///
/// No slot is `init_static_method_call`'s: php primes that one with
/// the class alone only where `op2` is no constant, and with the class
/// and the callee together where it is (`CACHE_POLYMORPHIC_PTR`), so it
/// reads and writes [`Cached::ClassAndCallee`] off the slot itself.
pub(crate) fn cached_class(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    at: u32,
    slot: Option<CacheSlot>,
) -> Option<crate::api::values::ClassId> {
    if let Some(Cached::Class(class)) = slot.map(|slot| req.cached(frame, slot)) {
        return Some(class);
    }
    let lowered = constant(code, ConstIdx(at.saturating_add(1)));
    let found = lowered
        .as_str_bytes()
        .and_then(|lowered| find_class(req, lowered));
    if let (Some(slot), Some(class)) = (slot, found) {
        req.prime(frame, slot, Cached::Class(class));
    }
    found
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use php_heap::{FrameRef, MapTable};
    use php_unit::{CacheSlot, OpArray, Unit};

    use super::{Body, Cached, code_of, init_runtime_cache, runtime_cache_of};
    use crate::api::call::Function;
    use crate::api::request::Request;
    use crate::engine::{Engine, RequestDescription};

    /// A unit whose main op array has no oplines and `cache_size` bytes
    /// of runtime cache.
    fn with_cache(cache_size: u32) -> Arc<Unit> {
        Arc::new(Unit {
            main: OpArray {
                cache_size,
                ..OpArray::default()
            },
            ..Unit::default()
        })
    }

    /// The main op array's activation lays its cache down with
    /// `cache_size / sizeof(void *)` slots, all zero
    /// (`memset(run_time_cache, 0, cache_size)`): a slot inside keeps
    /// what is primed into it, a slot past the end takes nothing and
    /// reads `Unprimed`, and the next activation lays a fresh one down
    /// as `zend_execute` allocates one each time it enters the op array,
    /// so a frame pushed after the first is popped reads every slot
    /// fresh.
    #[test]
    fn a_main_op_array_s_activation_lays_cache_size_bytes_of_slots_down_fresh() {
        let engine = Engine::cli_builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let unit = with_cache(24);
        let frame = req.pushed(Arc::clone(&unit));
        for at in [0, 8, 16, 24] {
            assert_eq!(req.cached(frame, CacheSlot(at)), Cached::Unprimed);
        }
        req.prime(frame, CacheSlot(16), Cached::Function(Function(3)));
        req.prime(frame, CacheSlot(24), Cached::Hit);
        assert_eq!(
            req.cached(frame, CacheSlot(16)),
            Cached::Function(Function(3)),
            "the third slot is inside 24 bytes"
        );
        assert_eq!(
            req.cached(frame, CacheSlot(24)),
            Cached::Unprimed,
            "the fourth is past them and takes nothing"
        );
        req.pop_frame();

        let frame = req.pushed(unit);
        assert_eq!(
            req.cached(frame, CacheSlot(16)),
            Cached::Unprimed,
            "a frame pushed after is fresh"
        );
        req.pop_frame();
    }

    /// A function body's cache is laid down on its first activation
    /// and found by every one after (`zend_init_func_run_time_cache`):
    /// what one activation primes, the next reads; and the cache is
    /// reached through the request's map table, so a frame not on the
    /// stack has none to read.
    #[test]
    fn a_function_body_s_cache_is_laid_down_once_and_kept_for_the_request() {
        let engine = Engine::cli_builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let unit = with_cache(8);
        let mut map = MapTable::new();
        let code = code_of(&unit);
        for _ in 0..2 {
            init_runtime_cache(&mut map, code, Body::Function(0));
        }
        let ptr = runtime_cache_of(code);
        let laid = map.get(ptr).and_then(|cache| cache.slots.as_ref());
        assert_eq!(
            laid.map(|slots| slots.len()),
            Some(1),
            "one slot of eight bytes"
        );
        map.map(ptr).set(CacheSlot(0), Cached::Miss);
        init_runtime_cache(&mut map, code, Body::Function(0));
        assert_eq!(
            map.get(ptr).map(|cache| cache.get(CacheSlot(0))),
            Some(Cached::Miss),
            "the second activation finds the first's slot"
        );
        init_runtime_cache(&mut map, code, Body::Main);
        assert_eq!(
            map.get(ptr).map(|cache| cache.get(CacheSlot(0))),
            Some(Cached::Unprimed),
            "the main op array's activation lays a fresh one down"
        );

        assert_eq!(
            req.cached(FrameRef(7), CacheSlot(0)),
            Cached::Unprimed,
            "a frame not on the stack reads nothing"
        );
        req.prime(FrameRef(7), CacheSlot(0), Cached::Hit);
        assert_eq!(req.cached(FrameRef(7), CacheSlot(0)), Cached::Unprimed);
    }
}
